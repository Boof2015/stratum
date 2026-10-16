"""Bounded client-side output conversion, separate from Stratum's native clock.

One converter per track, kept alive across network segments. A fixed output
clock is never restarted at a natural handoff. This is a proof adapter, not
Astra's DSP implementation or a bit-perfect playback mode.
"""
from fractions import Fraction
from math import gcd
import os
from pathlib import Path
import selectors
import subprocess
import time

import navidrome as nav

RATES = (44100, 48000, 96000)
FRAME_BYTES = 8


def mapping(infos, ranges, output_rate):
    nav.require(output_rate in RATES and len(infos) == len(ranges), "invalid output clock")
    elapsed, result, previous = Fraction(0), [], 0
    for info, part in zip(infos, ranges):
        rate, origin, frames = info['sample_rate'], part['origin'], part['frames']
        nav.require(rate in RATES and 0 <= origin and frames > 0 and origin + frames <= info['frames'],
                    "invalid native presentation range")
        elapsed += Fraction(frames, rate)
        boundary = (elapsed.numerator * output_rate) // elapsed.denominator
        # Keep conversion phase anchored at source frame zero, with real filter
        # context. Source seeks round down by less than one output frame; output
        # durations use cumulative rational time, not rounding each segment.
        stride = rate // gcd(rate, output_rate)
        begin = max(0, origin - 120 * rate // 1000)
        begin -= begin % stride
        end = min(info['frames'], origin + frames + 120 * rate // 1000)
        count = boundary - previous
        skip = (origin - begin) * output_rate // rate
        nav.require(count > 0 and count <= ((end - begin) * output_rate + rate - 1) // rate - skip,
                    "conversion cannot cover output interval")
        result.append({**part, 'sample_rate': rate, 'input_origin': begin,
                       'input_end': end, 'output_start': previous,
                       'output_frames': count, 'discard_output_start': skip})
        previous = boundary
    return result


def resample_filter(rate):
    return f'aresample={rate}:resampler=swr:exact_rational=1:async=0:dither_method=none'


class Converter:
    """Streaming pipe with bounded I/O and explicit EOF drain, off audio thread.

    send(start, pcm) admits output PCM into the existing sink. The converter
    never pads, flushes during starvation, or resets at a quality change.
    """
    def __init__(self, part, output_rate, send, directory, cancel=None):
        self.part, self.rate, self.send = part, output_rate, send
        self.cancel = cancel
        self.input_rate = part['sample_rate']
        self.expected_input = part['input_end'] - part['input_origin']
        self.input_frames = self.raw_output_frames = self.output_frames = 0
        self.peak_pending_output_frames = 0
        self.partial = bytearray()
        self.finished = self.closed = False
        self.process = None
        self.log = None
        self.selector = selectors.DefaultSelector()
        self.deadline = time.monotonic() + 90
        self.first_output_at = None
        if self.input_rate != output_rate:
            self.log = (Path(directory) / 'resampler.log').open('wb')
            try:
                self.process = subprocess.Popen(nav.FFMPEG + [
                    '-f', 's32le', '-ar', str(self.input_rate), '-ac', '2',
                    '-probesize', '32', '-analyzeduration', '0', '-blocksize', '8192', '-i', 'pipe:0',
                    '-af', resample_filter(output_rate), '-c:a', 'pcm_s32le',
                    '-f', 's32le', '-flush_packets', '1', 'pipe:1'],
                    stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=self.log, bufsize=0)
                os.set_blocking(self.process.stdin.fileno(), False)
                os.set_blocking(self.process.stdout.fileno(), False)
                self.selector.register(self.process.stdout, selectors.EVENT_READ)
            except BaseException:
                self.close()
                raise

    def _check(self):
        if self.cancel is not None and self.cancel.is_set():
            raise nav.Cancelled("superseded output conversion")

    def _emit(self, data):
        self._check()
        self.partial.extend(data)
        size = len(self.partial) // FRAME_BYTES * FRAME_BYTES
        if not size:
            return
        block = bytes(self.partial[:size])
        del self.partial[:size]
        start = self.raw_output_frames
        self.raw_output_frames += size // FRAME_BYTES
        maximum = (self.expected_input * self.rate + self.input_rate - 1) // self.input_rate
        nav.require(self.raw_output_frames <= maximum, 'resampler exceeded output bound')
        lo = max(start, self.part['discard_output_start'])
        hi = min(self.raw_output_frames, self.part['discard_output_start'] + self.part['output_frames'])
        # Native helper packet size stays bounded even if several pipe reads
        # arrive at once. No source-frame count is used as an output-frame count.
        quantum = 2 * self.rate
        while lo < hi:
            self._check()
            take = min(quantum, hi - lo)
            payload = block[(lo - start) * FRAME_BYTES:(lo - start + take) * FRAME_BYTES]
            self.send(self.part['output_start'] + self.output_frames, payload)
            if self.first_output_at is None:
                self.first_output_at = time.monotonic()
            self.output_frames += take
            lo += take

    def _pump(self, payload, target, final=False):
        process = self.process
        local_deadline = min(self.deadline, time.monotonic() + 8)
        if payload:
            self.selector.register(process.stdin, selectors.EVENT_WRITE)
        eof = False
        while payload or (not eof if final else self.raw_output_frames < target):
            self._check()
            nav.require(time.monotonic() < local_deadline, 'resampler pipe timed out')
            for key, _ in self.selector.select(0.05):
                if key.fileobj is process.stdin:
                    try:
                        count = os.write(process.stdin.fileno(), payload[:65536])
                        nav.require(count > 0, 'resampler input closed')
                        payload = payload[count:]
                    except BlockingIOError:
                        continue
                    if not payload:
                        self.selector.unregister(process.stdin)
                else:
                    try:
                        chunk = os.read(process.stdout.fileno(), 65536)
                    except BlockingIOError:
                        continue
                    if chunk:
                        self._emit(chunk)
                    else:
                        eof = True
                        self.selector.unregister(process.stdout)
                        nav.require(final and not payload, 'resampler ended before native EOF')
            if not eof and process.poll() is not None and not self.selector.get_map():
                raise RuntimeError('resampler stopped without output')
        return eof

    def append(self, source_start, pcm):
        self._check()
        nav.require(not self.finished and not self.closed and len(pcm) % FRAME_BYTES == 0 and len(pcm) > 0,
                    'invalid converter input')
        frames = len(pcm) // FRAME_BYTES
        nav.require(source_start == self.part['input_origin'] + self.input_frames
                    and self.input_frames + frames <= self.expected_input,
                    'missing, repeated or out-of-range native frames')
        self.input_frames += frames
        if self.process is None:
            self._emit(pcm)
        else:
            # Allow demux/encoder batching plus the resampler's short filter
            # delay. This is waiting for output, never manufactured silence.
            expected = self.input_frames * self.rate // self.input_rate
            batch_margin = (8192 * self.rate + self.input_rate - 1) // self.input_rate + 8192
            self._pump(memoryview(pcm), max(0, expected - batch_margin))
            self.peak_pending_output_frames = max(self.peak_pending_output_frames, expected - self.raw_output_frames)

    def finish(self):
        self._check()
        nav.require(not self.finished and not self.closed and self.input_frames == self.expected_input,
                    'converter finished before native EOF/context')
        if self.process is not None:
            self.process.stdin.close()
            self._pump(memoryview(b''), 0, final=True)
            nav.require(self.process.wait(timeout=3) == 0, 'resampler failed')
        nav.require(not self.partial and self.output_frames == self.part['output_frames'],
                    'resampler produced a short output interval')
        self.finished = True
        return {'input_frames': self.input_frames, 'converted_frames': self.raw_output_frames,
                'output_frames': self.output_frames, 'first_output_at': self.first_output_at,
                'conversion_active': self.input_rate != self.rate,
                'peak_pending_output_frames': self.peak_pending_output_frames,
                'discard_output_start': self.part['discard_output_start'],
                'discard_output_end': self.raw_output_frames - self.part['discard_output_start'] - self.output_frames}

    def close(self):
        # Called by the owning worker, never concurrently with append/finish.
        # Other threads cancel via the event; the output gate handles late sends.
        if self.closed:
            return
        self.closed = True
        if self.process is not None:
            if self.process.poll() is None:
                if self.cancel is not None and self.cancel.is_set():
                    self.process.kill()  # Obsolete state must not drain into a blocked output pipe.
                else:
                    self.process.terminate()
                try:
                    self.process.wait(timeout=2)
                except subprocess.TimeoutExpired:
                    self.process.kill()
                    self.process.wait(timeout=2)
            self.process.stdin.close()
            self.process.stdout.close()
        self.selector.close()
        if self.log is not None:
            self.log.close()

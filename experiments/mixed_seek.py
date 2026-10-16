#!/usr/bin/env python3
"""Seek/skip cancellation through the fixed-output converter, with forced races.

Silent file output by default. Disposable Navidrome only; no Astra integration.
Quality changes are scripted to exercise ownership, not to evaluate adaptation.
"""
import argparse
import base64
import hashlib
import http.client
import json
from pathlib import Path
import tempfile
import threading
import time
import urllib.parse

import handoff
import live
import navidrome as nav
from mixed_handoff import convert_reference, make_plans
from output_clock import Converter, mapping, resample_filter
from seek import Client, SeekServer, Ticket, validate_envelope
from seek_output import FileTimeline, NativeTimeline


class MixedClient(Client):
    def __init__(self, server, sink, output, infos, output_rate):
        super().__init__(server, sink, output, infos)
        self.output_rate = output_rate
        rate = infos[0]["sample_rate"]
        self.policy = live.Policy(infos[0]["bytes"] / (infos[0]["frames"] / rate) * (2 * rate + 1) / rate,
                                  segment_seconds=(2 * rate + 1) / rate)
        self.held_input = threading.Event()
        self.input_rejected = 0
        self.converters, self.conversions, self.handles = {}, {}, {}

    def switch(self, track, origin, frames, reason):
        requested = time.monotonic()
        part = mapping([self.infos[track]], [{'origin': origin, 'frames': frames}], self.output_rate)[0]
        with self.lock:
            old = self.generation
            self.generation += 1
            generation = self.generation
            if old >= 0:
                self.tickets[old].cancel()
                self.sink.switch(generation, part['output_frames'])
            self.plans[generation] = {**part, 'generation': generation, 'track': track,
                'requested': requested, 'flushed': time.monotonic(), 'segments': []}
            self.tickets[generation] = Ticket()
            self.events.append({'generation': generation, 'reason': reason, 'track': track,
                                'origin': origin, 'requested': requested, 'output_ready': time.monotonic()})
        if old >= 0:
            self.server.cancel(old)
        thread = threading.Thread(target=self.produce, args=(generation,), daemon=True)
        self.workers.append(thread)
        thread.start()
        print(f'generation {generation}: {reason} → track {track + 1} at '
              f'{origin / part["sample_rate"]:.3f}s; {part["sample_rate"]} → {self.output_rate} Hz', flush=True)

    def admit_native(self, generation, segment, observation):
        # Do not hold this lock during conversion: an old pipe may be blocked.
        # Its worker owns its converter; cancellation and a second output gate
        # cover a switch immediately after this input-admission check.
        with self.lock:
            if generation != self.generation:
                self.input_rejected += 1
                return False
            self.plans[generation]['segments'].append(segment)
            if segment['frames'] == 2 * self.plans[generation]['sample_rate'] + 1:
                self.policy.observe(*observation)
            return True

    def admit_output(self, generation, start, pcm):
        with self.lock:
            if generation != self.generation:
                self.stale_rejected += 1
                raise nav.Cancelled('late converted output rejected')
            nav.require(self.sink.append(generation, start, pcm), 'current output rejected')
            self.handles[generation].write(pcm)

    def send(self, generation, start, pcm):
        played = self.sink.snapshot(generation)['started']
        if generation == 1 and played is not None and time.monotonic() - played >= 0.5 and not self.held.is_set():
            # Hold a REAL converted block after the converter's cancellation
            # check. Cancellation cannot undo a callback already in progress.
            self.held.set()
            nav.require(self.release.wait(12), 'converted-completion barrier timed out')
        self.admit_output(generation, start, pcm)

    def produce(self, generation):
        part, ticket = self.plans[generation], self.tickets[generation]
        folder = self.output / f'generation-{generation}'
        converter = None
        try:
            with (folder / 'output.s32le').open('wb') as handle:
                self.handles[generation] = handle
                converter = Converter(part, self.output_rate,
                    lambda start, pcm: self.send(generation, start, pcm), folder, cancel=ticket.cancelled)
                self.converters[generation] = converter
                for segment in make_plans([self.infos[part['track']]], [part]):
                    while self.sink.snapshot(generation)['buffered'] >= 8:
                        ticket.check()
                        ticket.cancelled.wait(0.02)
                    ticket.check()
                    # First run stays lossless until a real download is
                    # interrupted. Each resumed run changes quality after its
                    # small context block, keeping the same converter alive.
                    quality = 'lossless' if generation == 0 or segment['source_start'] < part['origin'] else 'opus-64'
                    pcm, record, observation = self.fetch_native(generation, segment, quality)
                    if generation == 2 and not self.held_input.is_set():
                        self.held_input.set()
                        nav.require(self.release.wait(12), 'native-completion barrier timed out')
                    if not self.admit_native(generation, record, observation):
                        raise nav.Cancelled('late native input rejected')
                    converter.append(segment['source_start'], pcm)
                self.conversions[generation] = converter.finish()
        except BaseException as error:
            if not ticket.cancelled.is_set():
                self.error = error
        finally:
            if converter is not None:
                converter.close()
                self.conversions.setdefault(generation, {
                    'cancelled': ticket.cancelled.is_set(), 'input_frames': converter.input_frames,
                    'output_frames': converter.output_frames, 'finished': converter.finished})
                self.conversions[generation]['closed'] = converter.closed
                self.conversions[generation]['process_stopped'] = converter.process is None or converter.process.poll() is not None

    def fetch_native(self, generation, segment, quality):
        plan, ticket = self.plans[generation], self.tickets[generation]
        start, frames, rate = segment['source_start'], segment['frames'], plan['sample_rate']
        ticket.check()
        job = {'generation': generation, 'source_start': start, 'frames': frames, 'quality': quality,
               'stage': 'connecting', 'began': time.monotonic(), 'received_bytes': 0}
        self.jobs.append(job)
        connection = http.client.HTTPConnection('127.0.0.1', self.server.port, timeout=5)
        try:
            connection.connect()
            ticket.attach(connection.sock)
            connection.request('GET', self.server.route + '?' + urllib.parse.urlencode({
                'generation': generation, 'track': plan['track'], 'start': start, 'frames': frames, 'quality': quality}))
            with connection.getresponse() as response:
                nav.require(response.status == 200, 'segment request failed')
                size = int(response.headers['Content-Length'])
                nav.require(0 < size <= live.MAX_PAYLOAD, 'invalid response size')
                meta = validate_envelope(json.loads(base64.b64decode(response.headers['X-Stratum-Test'])),
                    generation, plan['track'], start, quality, frames=frames, rate=rate)
                body_started = time.monotonic()
                job.update(stage='download', body_started=body_started)
                body = bytearray()
                while len(body) < size:
                    ticket.check()
                    chunk = response.read(min(4096, size - len(body)))
                    nav.require(bool(chunk), 'truncated segment')
                    body.extend(chunk)
                    job['received_bytes'] = len(body)
                received = time.monotonic()
            ticket.check()
            nav.require(hashlib.sha256(body).hexdigest() == meta['payload_sha256'], 'corrupt payload')
            folder = self.output / f'generation-{generation}'
            encoded = folder / (f'segment-{start}.' + ('flac' if quality == 'lossless' else 'opus'))
            encoded.write_bytes(body)
            job['stage'] = 'decode'
            conversion = ['-af', resample_filter(rate)] if rate != 48000 and quality != 'lossless' else []
            pcm = nav.run(nav.FFMPEG + ['-i', str(encoded), '-map', '0:a:0'] + conversion +
                          ['-f', 's32le', '-c:a', 'pcm_s32le', 'pipe:1'], cancel=ticket.cancelled)
            nav.require(len(pcm) == meta['decoded_frames'] * 8, 'decoded duration mismatch')
            begin = meta['discard_start_frames'] * 8
            pcm = pcm[begin:begin + frames * 8]
            decoded = time.monotonic()
            name = f'segment-{start}.s32le'
            (folder / name).write_bytes(pcm)
            job['stage'] = 'decoded'
            return pcm, {**segment, 'track': 0, 'quality': quality, 'pcm': name}, (
                size, received - body_started, body_started - job['began'] + decoded - received)
        except BaseException:
            job['cancelled'] = ticket.cancelled.is_set()
            raise
        finally:
            connection.close()
            job['finished'] = time.monotonic()

    def script(self):
        a, b = (info['sample_rate'] for info in self.infos)
        self.switch(0, 80 * a + 37, 32 * a + 7, 'start')
        self.wait(lambda: any(j['generation'] == 0 and j['stage'] == 'download'
            and 'finished' not in j and j['received_bytes'] >= 8192 and self.sink.started is not None
            and time.monotonic() - self.sink.started >= 5.1 for j in self.jobs), 'no slow download to interrupt')
        self.switch(0, 40 * a + 71, 32 * a + 7, 'seek backward during download')
        self.wait(self.held.is_set, 'no converted block to race')
        self.switch(0, 60 * a + 113, 32 * a + 7, 'seek while old converter holds output')
        self.wait(self.held_input.is_set, 'no decoded native block to race')
        self.switch(0, 120 * a + 151, 32 * a + 7, 'rapid newer seek wins')
        self.release.set()
        self.wait(lambda: self.stale_rejected == 1 and self.input_rejected == 1, 'late audio was not rejected', timeout=5)
        if isinstance(self.sink, NativeTimeline):
            self.sink.inject_stale(1)
        self.wait(lambda: self.sink.snapshot(3)['started'] is not None
                  and time.monotonic() - self.sink.snapshot(3)['started'] >= 1, 'latest seek never resumed')
        self.wait(lambda: any(j['generation'] == 3 and 'finished' not in j for j in self.jobs), 'no work pending at skip')
        self.switch(1, 20 * b + 197, 8 * b + 9, 'skip to a different native rate')
        self.sink.finish()
        self.wait(lambda: 4 in self.conversions, 'final converter did not drain')
        if self.error:
            raise self.error


def verify_generation(folder, plan, snapshot, source, output_rate, checker):
    actual = (folder / 'output.s32le').read_bytes()
    nav.require(len(actual) == snapshot['accepted'] * 8, 'output admission count mismatch')
    nav.require(snapshot.get('silence_frames', 0) == 0, 'resumed playback starved')
    delivery = {'seek': True, 'output_rate': output_rate, 'frames': snapshot['accepted'],
        'rendered_media_frames': snapshot['consumed'],
        'tracks': [{**plan, 'source': str(source), 'output_pcm': 'output.s32le'}],
        'segments': plan['segments'], 'underruns': snapshot.get('underruns', [])}
    path = folder / 'delivery.json'
    path.write_text(json.dumps(delivery, indent=2) + '\n')
    checked = json.loads(nav.run([str(checker), str(path)]))
    nav.require(checked['exact_frames'] and checked['rendered_matches'], 'capture/accounting mismatch')
    if plan['segments']:
        native = checked['tracks'][0]
        nav.require(native['lossless_exact'] and handoff.alignment_passes(native), 'native source verification failed')
    if actual:
        native_path = folder / 'reference-input.s32le'
        with native_path.open('wb') as stream:
            for segment in plan['segments']:
                stream.write((folder / segment['pcm']).read_bytes())
        reference = convert_reference(native_path, plan['sample_rate'], output_rate,
            plan['discard_output_start'], len(actual) // 8, raw=True)
        nav.require(actual == reference, 'converted prefix differs from continuous reference')
        if all(s['quality'] == 'lossless' for s in plan['segments']):
            original = convert_reference(source, plan['sample_rate'], output_rate,
                plan['origin'] * output_rate // plan['sample_rate'], len(actual) // 8)
            nav.require(actual == original, 'seek conversion lost original source phase')
    return {**checked, 'continuous_output_matches': True, 'source': str(source)}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    for name in ('navidrome', 'stratum', 'checker', 'source', 'skip-source', 'out'):
        parser.add_argument('--' + name, type=Path, required=True)
    parser.add_argument('--output-rate', type=int, choices=(44100, 48000, 96000), required=True)
    parser.add_argument('--audio-helper', type=Path)
    parser.add_argument('--audio-device')
    parser.add_argument('--audio-gain', type=float, default=0.25)
    args = parser.parse_args()
    nav.require(bool(args.audio_helper) == bool(args.audio_device) and 0 <= args.audio_gain <= 1, 'invalid output options')
    for name in ('navidrome', 'stratum', 'checker', 'source', 'skip_source', 'audio_helper'):
        if getattr(args, name):
            setattr(args, name, getattr(args, name).resolve(strict=True))
    sources = [args.source, args.skip_source]
    infos = [handoff.inspect_source(s, args.stratum) for s in sources]
    nav.require(infos[0]['sha256'] != infos[1]['sha256'], 'use different tracks')
    nav.require(infos[0]['frames'] > 153 * infos[0]['sample_rate']
                and infos[1]['frames'] > 29 * infos[1]['sample_rate'], 'sources too short')
    args.out.mkdir(parents=True, exist_ok=False)
    args.out = args.out.resolve()
    total = mapping([infos[0]], [{'origin': 80 * infos[0]['sample_rate'] + 37,
        'frames': 32 * infos[0]['sample_rate'] + 7}], args.output_rate)[0]['output_frames']
    with tempfile.TemporaryDirectory(prefix='stratum-mixed-seek-nav-') as temporary:
        with nav.disposable_library(args.navidrome, sources, Path(temporary)) as (account, songs, _):
            sink = (NativeTimeline(args.out, total, args.audio_helper, args.audio_device, args.audio_gain, rate=args.output_rate)
                    if args.audio_helper else FileTimeline(args.out, total, rate=args.output_rate))
            client = None
            try:
                with SeekServer(account, songs, infos, args.stratum, args.out, sink).serve() as server:
                    client = MixedClient(server, sink, args.out, infos, args.output_rate)
                    client.script()
            finally:
                if client is not None:
                    client.stop()
                else:
                    sink.stop()
    checks, generations = [], []
    for generation, plan in client.plans.items():
        snapshot = sink.snapshot(generation)
        checks.append(verify_generation(args.out / f'generation-{generation}', plan, snapshot,
                                       sources[plan['track']], args.output_rate, args.checker))
        device = snapshot.get('device_report')
        if device:
            nav.require(device['timeline_discontinuities'] == 0, 'native timeline discontinuity')
            if generation == 4:
                live.validate_device_timing(device, snapshot['consumed'] / args.output_rate)
        generations.append({**plan, **snapshot, 'resume_seconds': None if snapshot['started'] is None
                            else snapshot['started'] - plan['requested']})
    nav.require(sink.snapshot(2)['accepted'] == sink.snapshot(2)['consumed'] == 0, 'superseded startup produced audio')
    nav.require(sink.snapshot(4)['consumed'] == client.plans[4]['output_frames'], 'skip did not finish')
    nav.require(not any('error' in r for r in server.records), 'server request failed')
    nav.require(any(r.get('cancelled') and r['generation'] == 0 for r in server.records), 'download was not cancelled')
    nav.require(all(c['closed'] and c['process_stopped'] for c in client.conversions.values()), 'converter remained')
    result = {'experiment': 'stratum-mixed-seek-poc-1', 'output_rate': args.output_rate,
        'source_info': infos, 'generations': generations, 'events': client.events, 'client_jobs': client.jobs,
        'server_requests': server.records, 'audio_checks': checks, 'conversions': client.conversions,
        'stale_native_completions_rejected': client.input_rejected, 'stale_converted_completions_rejected': client.stale_rejected,
        'native_stale_packets_rejected': sink.rejected if isinstance(sink, NativeTimeline) else None,
        'source_sessions': {g: s.summary() for g, s in server.sessions.items()}, 'server_stopped': True,
        'scope': 'Intentional seek pauses; fixed processed output; scripted quality changes; no Astra integration.'}
    (args.out / 'report.json').write_text(json.dumps(result, indent=2) + '\n')
    print(f'Mixed-rate seek/skip checks passed: {args.out / "report.json"}', flush=True)


if __name__ == '__main__':
    main()

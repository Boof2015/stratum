"""Adapter for the bounded macOS AudioQueue helper. Output is opt-in by device UID."""
import json
import os
import selectors
import struct
import subprocess
import threading
import time

import navidrome as nav


class DevicePlayback:
    def __init__(self, output, helper, device, total, gain, rate=nav.RATE):
        nav.require(rate in (44100, 48000), "unsupported device client clock")
        self.rate = rate
        self.output, self.helper, self.device = output, helper, device
        self.total, self.gain = total, gain
        self.condition = threading.Condition()
        self.submitted = self.accepted = self.consumed = 0
        self.started = None
        self.ready, self.done, self.cancel = threading.Event(), threading.Event(), threading.Event()
        self.error = self.process = None
        self.underruns = []
        self.silence_frames = 0
        self.max_wake_late_ms = None  # Native callback, not the file sink's sleep loop.
        self.device_report = None

    def buffered(self):
        with self.condition:
            # Excludes the helper's three 20 ms AudioQueue buffers. Conservative
            # by at most 60 ms relative to data waiting in the native PCM ring.
            return max(0, self.accepted - self.consumed) / self.rate

    def append(self, start, pcm):
        nav.require(self.ready.wait(12), "audio helper did not open")
        if self.error:
            raise self.error
        frames, remainder = divmod(len(pcm), nav.CHANNELS * 4)
        nav.require(not remainder and frames > 0 and start == self.submitted and start + frames <= self.total,
                    "non-contiguous hardware delivery")
        process = self.process
        nav.require(process is not None and process.poll() is None, "audio helper stopped")
        payload = memoryview(struct.pack("<QQ", start, frames) + pcm)
        deadline = time.monotonic() + 5
        with selectors.DefaultSelector() as selector:
            selector.register(process.stdin, selectors.EVENT_WRITE)
            while payload:
                nav.require(not self.cancel.is_set() and time.monotonic() < deadline, "audio pipe write cancelled/timed out")
                if not selector.select(timeout=0.1):
                    continue
                try:
                    written = os.write(process.stdin.fileno(), payload)
                    nav.require(written > 0, "audio helper pipe closed")
                    payload = payload[written:]
                except BlockingIOError:
                    continue
        self.submitted += frames
        with self.condition:
            acknowledged = self.condition.wait_for(
                lambda: self.accepted >= self.submitted or self.error is not None or self.done.is_set(), timeout=5)
            if self.error:
                raise self.error
            nav.require(acknowledged and self.accepted >= self.submitted, "audio helper did not accept PCM")

    def stop(self):
        self.cancel.set()
        process = self.process
        if process is not None and process.poll() is None:
            process.terminate()
            try:
                process.wait(timeout=2)
            except subprocess.TimeoutExpired:
                process.kill()
                process.wait(timeout=2)

    def play(self):
        watchdog = None
        try:
            with (self.output / "device-stderr.log").open("wb") as log:
                self.process = subprocess.Popen([str(self.helper), "--device", self.device,
                    "--out", str(self.output), "--total", str(self.total), "--gain", str(self.gain)]
                    + (["--rate", str(self.rate)] if self.rate != nav.RATE else []),
                    stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=log, bufsize=0)
                os.set_blocking(self.process.stdin.fileno(), False)
                watchdog = threading.Timer(90, self.stop)
                watchdog.daemon = True
                watchdog.start()
                if self.cancel.is_set():
                    self.stop()
                finished = False
                for line in self.process.stdout:
                    nav.require(len(line) < 16384, "oversized audio status")
                    status = json.loads(line)
                    event = status["event"]
                    if event == "opened":
                        nav.require(status["device"]["uid"] == self.device, "audio helper opened another output")
                        self.ready.set()
                    elif event == "started":
                        self.started = time.monotonic()
                        print("Device playback started.", flush=True)
                    elif event == "status":
                        with self.condition:
                            accepted, consumed = status["accepted"], status["consumed"]
                            nav.require(self.accepted <= accepted <= self.total and self.consumed <= consumed <= accepted,
                                        "invalid audio queue counters")
                            self.accepted, self.consumed = accepted, consumed
                            self.condition.notify_all()
                    elif event == "finished":
                        finished = True
                    else:
                        raise RuntimeError("unknown audio helper event")
                code = self.process.wait(timeout=3)
                if code or not finished:
                    detail = (self.output / "device-stderr.log").read_text(errors="replace")[-2000:]
                    raise RuntimeError("audio output failed: " + detail.strip())
                report = json.loads((self.output / "device.json").read_text())
                nav.require(report["consumed_frames"] == self.total and report["device"]["uid"] == self.device
                            and report["client_sample_rate"] == self.rate,
                            "hardware output ended on the wrong track/device")
                self.device_report = report
                self.consumed = report["consumed_frames"]
                self.underruns, self.silence_frames = report["underruns"], report["silence_frames"]
                self.elapsed = report["elapsed_seconds"]
        except BaseException as error:
            self.error = error
        finally:
            if watchdog is not None:
                watchdog.cancel()
            self.stop()
            if self.process is not None:
                self.process.stdin.close()
                self.process.stdout.close()
            self.ready.set()
            self.done.set()
            with self.condition:
                self.condition.notify_all()

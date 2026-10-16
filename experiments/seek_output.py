"""Generation-aware sinks for seek.py. One native process, bounded reset count."""
import json
import os
import selectors
import struct
import subprocess
import threading
import time

import live
import navidrome as nav


class FileTimeline:
    def __init__(self, output, total, rate=live.RATE):
        self.output, self.players, self.threads = output, {}, {}
        nav.require(rate in (44100, 48000, 96000), "unsupported output clock")
        self.rate = rate
        self.archived = {}
        self.generation, self.started = -1, None
        self.switch(0, total)

    def switch(self, generation, total):
        if self.generation in self.players:
            old = self.players[self.generation]
            old.cancel.set()
            old.ready.set()
            self.threads[self.generation].join(timeout=3)
            nav.require(not self.threads[self.generation].is_alive(), "old consumer did not stop")
        self.generation = generation
        folder = self.output / f"generation-{generation}"
        folder.mkdir()
        (folder / "rendered.s32le").touch()
        player = live.Playback(folder, total=total,
                               start_buffer=3 * (2 * self.rate + 1) if generation == 0 else 2 * self.rate + 1, rate=self.rate)
        self.players[generation] = player
        thread = threading.Thread(target=player.play, daemon=True)
        self.threads[generation] = thread
        thread.start()

    def append(self, generation, start, pcm):
        if generation != self.generation:
            return False
        self.players[generation].append(start, pcm)
        return True

    def snapshot(self, generation):
        if generation in self.archived:
            return dict(self.archived[generation])
        p = self.players[generation]
        if self.started is None and p.started is not None:
            self.started = p.started
        with p.lock:
            accepted, consumed, buffered = p.consumed + p.frames, p.consumed, p.frames / self.rate
        return {"accepted": accepted, "consumed": consumed, "started": p.started,
                "underruns": p.underruns, "silence_frames": p.silence_frames,
                "buffered": buffered, "done": p.done.is_set()}

    def retire(self, generation):
        # Call after its producer has joined. Retain counters, not abandoned PCM.
        if generation in self.archived:
            return
        player = self.players[generation]
        nav.require(player.done.is_set(), "cannot retire an active consumer")
        self.threads[generation].join(timeout=3)
        nav.require(not self.threads[generation].is_alive(), "consumer thread remained")
        self.archived[generation] = self.snapshot(generation)
        with player.lock:
            player.queue.clear()
            player.frames = 0
        del self.players[generation]
        del self.threads[generation]

    def finish(self):
        p = self.players[self.generation]
        nav.require(p.done.wait(45), "file sink did not finish")
        if p.error:
            raise p.error

    def stop(self):
        for p in self.players.values():
            p.cancel.set()
            p.ready.set()
        for thread in self.threads.values():
            thread.join(timeout=3)
            nav.require(not thread.is_alive(), "file sink thread remains")


class NativeTimeline:
    def __init__(self, output, total, helper, device, gain, rate=live.RATE):
        self.output, self.device = output, device
        nav.require(rate in (44100, 48000, 96000), "unsupported output clock")
        self.rate = rate
        self.condition = threading.Condition()
        self.write_lock = threading.Lock()
        self.generation, self.started = 0, None
        self.states = {0: {"accepted": 0, "consumed": 0, "started": None, "done": False}}
        self.opened, self.rejected = set(), []
        self.error = None
        self.done = threading.Event()
        self.log = (output / "device-stderr.log").open("wb")
        self.process = subprocess.Popen([str(helper), "--device", device, "--out", str(output),
            "--total", str(total), "--gain", str(gain), "--rate", str(rate), "--interactive"],
            stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=self.log, bufsize=0)
        os.set_blocking(self.process.stdin.fileno(), False)
        self.thread = threading.Thread(target=self.read, daemon=True)
        self.thread.start()
        self.watchdog = threading.Timer(90, self.terminate)
        self.watchdog.daemon = True
        self.watchdog.start()
        try:
            self.wait(lambda: 0 in self.opened)
        except BaseException:
            self.stop()
            raise

    def wait(self, predicate):
        with self.condition:
            ok = self.condition.wait_for(lambda: predicate() or self.error or self.done.is_set(), timeout=8)
            if self.error:
                raise self.error
            nav.require(ok and predicate(), "native output acknowledgement timed out")

    def write(self, data):
        data = memoryview(data)
        deadline = time.monotonic() + 4
        with selectors.DefaultSelector() as selector:
            selector.register(self.process.stdin, selectors.EVENT_WRITE)
            while data:
                nav.require(self.process.poll() is None and time.monotonic() < deadline, "native pipe stopped/timed out")
                if not selector.select(timeout=0.05):
                    continue
                try:
                    sent = os.write(self.process.stdin.fileno(), data)
                    nav.require(sent > 0, "native pipe closed")
                    data = data[sent:]
                except BlockingIOError:
                    continue

    def switch(self, generation, total):
        with self.write_lock:
            nav.require(generation > self.generation, "generation did not advance")
            self.generation = generation
            self.states[generation] = {"accepted": 0, "consumed": 0, "started": None, "done": False}
            self.write(struct.pack("<QQQ", generation, 2**64 - 1, total))
            self.wait(lambda: generation in self.opened)

    def append(self, generation, start, pcm):
        with self.write_lock:
            if generation != self.generation:
                return False
            count, remainder = divmod(len(pcm), live.FRAME_BYTES)
            nav.require(not remainder and 0 < count <= 2 * self.rate + 1, "invalid PCM packet")
            self.write(struct.pack("<QQQ", generation, start, count) + pcm)
            self.wait(lambda: self.states[generation]["accepted"] >= start + count)
            return True

    def inject_stale(self, generation):
        # Deliberate native-boundary race probe, independent of the client gate.
        with self.write_lock:
            self.write(struct.pack("<QQQ", generation, 0, 1) + bytes(live.FRAME_BYTES))
            self.wait(lambda: generation in self.rejected)

    def snapshot(self, generation):
        with self.condition:
            state = dict(self.states[generation])
        state["buffered"] = (state["accepted"] - state["consumed"]) / self.rate
        return state

    def read(self):
        try:
            finished = False
            for line in self.process.stdout:
                nav.require(len(line) < 16384, "oversized native status")
                msg = json.loads(line)
                kind, generation = msg["event"], msg["generation"]
                with self.condition:
                    if kind == "rejected":
                        self.rejected.append(generation)
                    else:
                        state = self.states[generation]
                        if kind == "opened":
                            nav.require(msg["device"]["uid"] == self.device, "wrong output device")
                            self.opened.add(generation)
                        elif kind == "started":
                            state["started"] = time.monotonic()
                            if self.started is None:
                                self.started = state["started"]
                        elif kind == "status":
                            nav.require(state["consumed"] <= msg["consumed"] <= msg["accepted"]
                                        and state["accepted"] <= msg["accepted"], "invalid native counters")
                            state.update(accepted=msg["accepted"], consumed=msg["consumed"])
                        elif kind in ("flushed", "finished"):
                            report = json.loads((self.output / f"generation-{generation}" / "device.json").read_text())
                            nav.require(report["client_sample_rate"] == self.rate, "native output clock changed")
                            state.update(accepted=report["accepted_frames"], consumed=report["consumed_frames"],
                                         underruns=report["underruns"], silence_frames=report["silence_frames"],
                                         device_report=report, done=True)
                            finished = kind == "finished"
                        else:
                            raise RuntimeError("unknown native event")
                    self.condition.notify_all()
            nav.require(self.process.wait(timeout=3) == 0 and finished,
                        "native helper failed: " + (self.output / "device-stderr.log").read_text(errors="replace")[-2000:])
        except BaseException as error:
            self.error = error
            self.terminate()
        finally:
            self.done.set()
            with self.condition:
                self.condition.notify_all()

    def terminate(self):
        if self.process.poll() is None:
            self.process.terminate()
            try:
                self.process.wait(timeout=2)
            except subprocess.TimeoutExpired:
                self.process.kill()
                self.process.wait(timeout=2)

    def finish(self):
        nav.require(self.done.wait(45), "native output did not finish")
        if self.error:
            raise self.error

    def stop(self):
        self.watchdog.cancel()
        self.terminate()
        self.thread.join(timeout=3)
        nav.require(not self.thread.is_alive(), "native reader did not stop")
        self.process.stdin.close()
        self.process.stdout.close()
        self.log.close()

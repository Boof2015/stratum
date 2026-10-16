#!/usr/bin/env python3
"""Bounded real-time delivery experiment; file sink unless audio is explicitly selected.

A disposable Navidrome feeds the Rust encoder on demand. A loopback segment
endpoint meters delivery in real time. The client consumes PCM on a 48 kHz clock,
recording silence on starvation instead of concealing missed deadlines. An
optional macOS helper lets an explicitly selected output device drive consumption.
"""
import argparse
import base64
from collections import deque
import contextlib
import hashlib
import http.server
import json
from pathlib import Path
import secrets
import socket
import tempfile
import threading
import time
import urllib.parse
import urllib.request

import navidrome as nav
from source_session import SourceSession

RATE = nav.RATE
FRAME_BYTES = nav.CHANNELS * 4
SEGMENT = 96001
TOTAL = 16 * SEGMENT
ORIGIN = 80 * RATE + 37
QUANTUM = 960
START_BUFFER = 3 * SEGMENT
TARGET_BUFFER = 4 * SEGMENT
MAX_PAYLOAD = 2 * 1024 * 1024
QUALITIES = ("opus-64", "opus-128", "lossless")


class RetryLower(Exception):
    pass


class Policy:
    """Experimental policy: quick measured downshift, cautious measured recovery.

    Has no access to the traffic profile. Include source/encode/decode overhead
    in the budget; compressed bytes alone are not the full preparation cost.
    """
    def __init__(self, lossless_bytes):
        self.lossless_bytes = lossless_bytes
        self.capacity = None
        self.overhead = 0.8
        self.quality = "lossless"
        self.good = 0

    def size(self, quality):
        return {"lossless": self.lossless_bytes,
                "opus-128": 38000, "opus-64": 20000}[quality]

    def observe(self, size, seconds, overhead):
        measured = size / max(seconds, 0.001)
        # Fall immediately; require evidence from three completed requests to rise.
        self.capacity = measured if self.capacity is None else min(measured, self.capacity * 0.5 + measured * 0.5)
        self.overhead = max(0.15, self.overhead * 0.7 + overhead * 0.3)
        self.good += 1

    def choose(self, buffer_seconds):
        if self.capacity is None:
            return self.quality
        budget = min(SEGMENT / RATE * 0.85, max(0.3, buffer_seconds - 1))
        eligible = [q for q in QUALITIES if self.size(q) / (self.capacity * 0.8) + self.overhead < budget]
        best = eligible[-1] if eligible else "opus-64"
        if QUALITIES.index(best) < QUALITIES.index(self.quality):
            self.quality, self.good = best, 0
        elif best != self.quality and self.good >= 3 and buffer_seconds >= 5:
            self.quality, self.good = best, 0
        return self.quality

    def endangered(self, received, size, seconds, buffer_seconds):
        if self.quality == "opus-64" or received < 16384 or seconds < 0.35:
            return False
        measured = received / seconds
        remaining = (size - received) / measured + self.overhead
        if remaining <= max(0.5, buffer_seconds - 1):
            return False
        self.capacity, self.quality, self.good = measured, "opus-64", 0
        return True


class Playback:
    """Real wall-clock PCM consumer. A late producer cannot move its deadline.

    rendered.s32le includes every starvation quantum. Per-segment PCM files are
    also checked afterward against the source, separately from delivery timing.
    """
    def __init__(self, output, total=TOTAL, start_buffer=START_BUFFER):
        self.output = output
        self.total = total
        self.start_buffer = min(total, start_buffer)
        self.lock = threading.Lock()
        self.queue = deque()
        self.frames = 0
        self.consumed = 0
        self.started = None
        self.ready = threading.Event()
        self.done = threading.Event()
        self.cancel = threading.Event()
        self.error = None
        self.underruns = []
        self.silence_frames = 0
        self.max_wake_late_ms = 0

    def buffered(self):
        with self.lock:
            return self.frames / RATE

    def append(self, start, pcm):
        with self.lock:
            nav.require(start == self.consumed + self.frames, "non-contiguous delivery")
            nav.require(len(pcm) % FRAME_BYTES == 0, "partial PCM frame")
            self.queue.append((memoryview(pcm), time.monotonic()))
            self.frames += len(pcm) // FRAME_BYTES
            if self.frames >= self.start_buffer:
                self.ready.set()

    def consume(self, count, deadline=float("inf")):
        with self.lock:
            count = min(count, self.total - self.consumed)
            if self.frames < count:
                return None, count
            # A late wakeup must not make data that missed the scheduled audio
            # deadline look on-time. Check availability at the intended tick.
            available = 0
            for chunk, ready_at in self.queue:
                if ready_at > deadline:
                    break
                available += len(chunk) // FRAME_BYTES
                if available >= count:
                    break
            if available < count:
                return None, count
            parts, remaining = [], count * FRAME_BYTES
            while remaining:
                head, ready_at = self.queue.popleft()
                take = min(remaining, len(head))
                parts.append(head[:take])
                if take < len(head):
                    self.queue.appendleft((head[take:], ready_at))
                remaining -= take
            self.frames -= count
            self.consumed += count
            return b"".join(parts), count

    def play(self):
        try:
            nav.require(self.ready.wait(25), "startup buffer deadline exceeded")
            nav.require(not self.cancel.is_set(), "playback cancelled")
            self.started = time.monotonic()
            tick, starving = 0, False
            with (self.output / "rendered.s32le").open("wb") as sink:
                while self.consumed < self.total and not self.cancel.is_set():
                    deadline = self.started + tick * QUANTUM / RATE
                    self.cancel.wait(max(0, deadline - time.monotonic()))
                    if self.cancel.is_set():
                        break
                    now = time.monotonic()
                    nav.require(now - self.started < 65, "playback lifetime exceeded")
                    self.max_wake_late_ms = max(self.max_wake_late_ms, (now - deadline) * 1000)
                    pcm, count = self.consume(QUANTUM, deadline)
                    if pcm is None:
                        if not starving:
                            self.underruns.append({"at_media_frame": self.consumed,
                                                  "at_seconds": now - self.started,
                                                  "silence_frames": 0})
                        starving = True
                        self.underruns[-1]["silence_frames"] += count
                        self.silence_frames += count
                        pcm = bytes(count * FRAME_BYTES)
                    else:
                        starving = False
                    sink.write(pcm)
                    tick += 1
                # Wait out the final quantum instead of ending a tick early.
                end = self.started + (self.total + self.silence_frames) / RATE
                self.cancel.wait(max(0, end - time.monotonic()))
                nav.require(not self.cancel.is_set(), "playback cancelled")
            self.elapsed = time.monotonic() - self.started
        except BaseException as error:
            self.error = error
        finally:
            self.done.set()


class Link:
    """Aggregate body-rate shaper on the Sidecar→client hop, not Navidrome→Sidecar.

    Fixed 60 ms response delay; fast → 192 kbit/s → fast after playback starts.
    No saved burst credit. Rate changes apply to requests already in flight.
    """
    def __init__(self, playback, impaired):
        self.playback, self.impaired = playback, impaired
        self.lock = threading.Lock()
        self.bytes = 0

    def rate(self, now):
        elapsed = 0 if self.playback.started is None else now - self.playback.started
        return 24000 if self.impaired and 5 <= elapsed < 17 else 1250000

    def write(self, handler, data):
        with self.lock:
            remaining = len(data)
            previous = time.monotonic()
            while remaining > 0:
                delay = min(0.02, remaining / self.rate(previous))
                time.sleep(delay)
                now = time.monotonic()
                remaining -= (now - previous) * self.rate(previous)
                previous = now
            handler.wfile.write(data)
            self.bytes += len(data)


@contextlib.contextmanager
def endpoint(navidrome, song, binary, output, playback, impaired, source_session=None):
    route = "/" + secrets.token_urlsafe(24) + "/segment"
    records, lock = [], threading.Lock()
    link = Link(playback, impaired)

    class Handler(http.server.BaseHTTPRequestHandler):
        def log_message(self, *_):
            pass

        def do_GET(self):
            record = None
            try:
                self.connection.settimeout(5)
                self.connection.setsockopt(socket.SOL_SOCKET, socket.SO_SNDBUF, 4096)
                url = urllib.parse.urlsplit(self.path)
                if url.path != route:
                    self.send_error(404)
                    return
                params = urllib.parse.parse_qs(url.query, strict_parsing=True)
                start, quality = int(params["start"][0]), params["quality"][0]
                nav.require(0 <= start < TOTAL and start % SEGMENT == 0, "invalid interval")
                nav.require(quality in QUALITIES and set(params) == {"start", "quality"}, "invalid request")
                with lock:
                    nav.require(len(records) < 64, "request budget exceeded")
                    record = {"start_frame": start, "quality": quality,
                              "began": time.monotonic(), "sent_bytes": 0}
                    records.append(record)
                    work = output / ("request-%03d" % len(records))
                    work.mkdir()
                # Each request reauthorizes through Navidrome. Source access is
                # interval-bounded; nothing prebuilds later audio for the client.
                source_start = ORIGIN + start - nav.CONTEXT
                source = work / "source.flac"
                record["acquisition"] = nav.acquire(navidrome, song["id"], source_start,
                                                     SEGMENT + 2 * nav.CONTEXT, source,
                                                     session=source_session)
                segment = json.loads(nav.run([str(binary), "segment", "--source", str(source),
                    "--start-frame", str(nav.CONTEXT), "--frames", str(SEGMENT),
                    "--quality", quality, "--cache", str(work / "cache")]))
                payload = Path(segment["path"]).read_bytes()
                metadata = segment["metadata"]
                nav.require(len(payload) <= MAX_PAYLOAD, "oversized segment")
                # Test envelope, not a proposed public Stratum wire specification.
                envelope = {"start": start, "frames": SEGMENT, "metadata": metadata}
                record["prepared"] = time.monotonic()
                record["payload_bytes"] = len(payload)
                time.sleep(0.06)
                self.send_response(200)
                self.send_header("Content-Length", str(len(payload)))
                self.send_header("X-Stratum-Test", base64.b64encode(json.dumps(envelope).encode()).decode())
                self.send_header("Connection", "close")
                self.end_headers()
                for offset in range(0, len(payload), 4096):
                    chunk = payload[offset:offset + 4096]
                    link.write(self, chunk)
                    record["sent_bytes"] += len(chunk)
            except (BrokenPipeError, ConnectionResetError):
                if record is not None:
                    record["client_cancelled"] = True
            except Exception as error:
                if record is not None:
                    record["error"] = type(error).__name__
                self.close_connection = True
            finally:
                if record is not None:
                    record["finished"] = time.monotonic()

    server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    server.daemon_threads = True
    worker = threading.Thread(target=server.serve_forever, daemon=True)
    worker.start()
    try:
        yield f"http://127.0.0.1:{server.server_port}{route}", records
    finally:
        server.shutdown()
        server.server_close()
        worker.join(timeout=2)
        deadline = time.monotonic() + 35
        while any("finished" not in r for r in records) and time.monotonic() < deadline:
            time.sleep(0.05)
        nav.require(all("finished" in r for r in records), "segment workers did not finish")


def receive(url, start, quality, policy, playback, adaptive, output):
    began = time.monotonic()
    request = urllib.request.Request(url + "?" + urllib.parse.urlencode({"start": start, "quality": quality}))
    with nav.HTTP.open(request, timeout=10) as response:
        size = int(response.headers["Content-Length"])
        nav.require(0 < size <= MAX_PAYLOAD, "invalid payload size")
        envelope = json.loads(base64.b64decode(response.headers["X-Stratum-Test"]))
        meta = envelope["metadata"]
        nav.require(envelope["start"] == start and envelope["frames"] == SEGMENT,
                    "response interval mismatch")
        nav.require(meta["quality"] == quality and meta["sample_rate"] == RATE and meta["channels"] == nav.CHANNELS,
                    "response format mismatch")
        nav.require(meta["presentation"]["frames"] == SEGMENT and
                    meta["discard_start_frames"] + SEGMENT + meta["discard_end_frames"] == meta["decoded_frames"],
                    "invalid trims")
        body_started = time.monotonic()
        body = bytearray()
        while len(body) < size:
            chunk = response.read(min(4096, size - len(body)))
            nav.require(chunk, "truncated segment")
            body.extend(chunk)
            elapsed = time.monotonic() - body_started
            if adaptive and playback.started is not None and policy.endangered(len(body), size, elapsed, playback.buffered()):
                raise RetryLower()
        received = time.monotonic()
    nav.require(hashlib.sha256(body).hexdigest() == meta["payload_sha256"], "corrupt segment")
    path = output / (f"segment-{start}.{ 'flac' if quality == 'lossless' else 'opus'}")
    path.write_bytes(body)
    pcm = nav.decode(path)
    nav.require(len(pcm) == meta["decoded_frames"] * FRAME_BYTES, "decoded duration mismatch")
    trim = meta["discard_start_frames"] * FRAME_BYTES
    pcm = pcm[trim:trim + SEGMENT * FRAME_BYTES]
    decoded = time.monotonic()
    pcm_path = output / f"segment-{start}.s32le"
    pcm_path.write_bytes(pcm)
    policy.observe(size, received - body_started, body_started - began + decoded - received)
    playback.append(start, pcm)
    return {"start": start, "frames": SEGMENT, "quality": quality,
            "pcm": pcm_path.name, "began": began, "ready": decoded,
            "payload_bytes": size, "transfer_seconds": received - body_started,
            "buffer_after_seconds": playback.buffered()}


def scenario(name, navidrome, song, binary, output, source_cache="session", audio=None, source_seconds=160):
    output.mkdir()
    if audio is None:
        playback = Playback(output)
    else:
        from device_output import DevicePlayback
        playback = DevicePlayback(output, audio["helper"], audio["device"], TOTAL, audio["gain"])
    player = threading.Thread(target=playback.play, daemon=True)
    policy = Policy(song["size"] / source_seconds * (SEGMENT / RATE))
    session = SourceSession(navidrome, song["id"]) if source_cache == "session" else None
    adaptive = name != "forced-lossless"
    started = time.monotonic()
    segments, retries = [], []
    player.start()
    try:
        with endpoint(navidrome, song, binary, output, playback, name != "steady", session) as (url, server_records):
            for start in range(0, TOTAL, SEGMENT):
                while playback.buffered() >= TARGET_BUFFER / RATE:
                    nav.require(not playback.done.wait(0.02), "consumer stopped early")
                quality = policy.choose(playback.buffered()) if adaptive and playback.started is not None else "lossless"
                try:
                    segment = receive(url, start, quality, policy, playback, adaptive, output)
                except RetryLower:
                    retries.append({"start": start, "from": quality, "to": "opus-64",
                                    "at": time.monotonic(), "buffer_seconds": playback.buffered()})
                    segment = receive(url, start, "opus-64", policy, playback, adaptive, output)
                segments.append(segment)
                if len(segments) == 1 or segments[-1]["quality"] != segments[-2]["quality"]:
                    print(f"{name}: at media {start / RATE:.1f}s → {segment['quality']}; "
                          f"buffer {segment['buffer_after_seconds']:.2f}s", flush=True)
            nav.require(playback.done.wait(65), "consumer failed to finish")
            if playback.error:
                raise playback.error
        playback_start = playback.started
        for record in server_records:
            for key in ("began", "prepared", "finished"):
                if key in record:
                    record[key] -= playback_start
        for segment in segments:
            for key in ("began", "ready"):
                segment[key] -= playback_start
        for retry in retries:
            retry["at"] -= playback_start
        upstream_bytes = sum(r["acquisition"]["body_bytes_received"] for r in server_records)
        if session is not None:
            session.close()
            source_metrics = session.summary()
            nav.require(source_metrics["upstream_body_bytes"] == upstream_bytes, "source traffic counted twice")
            nav.require(source_metrics["validation_requests"] == len(server_records), "cached request skipped validation")
            nav.require(source_metrics["peak_cached_bytes"] <= source_metrics["cache_limit_bytes"], "source cache limit exceeded")
        else:
            source_metrics = {"upstream_body_bytes": upstream_bytes}
        result = {"scenario": name, "origin": ORIGIN, "frames": TOTAL,
                  "source_cache": {"mode": source_cache, **source_metrics},
                  "startup_seconds": playback_start - started, "elapsed_seconds": playback.elapsed,
                  "underruns": playback.underruns, "silence_frames": playback.silence_frames,
                  "max_wake_late_ms": playback.max_wake_late_ms,
                  "segments": segments, "retries": retries, "server_requests": server_records,
                  "acquired_after_playback_started": sum(r["began"] > 0 for r in server_records),
                  "received_after_playback_started": sum(s["ready"] > 0 for s in segments)}
        if audio is not None:
            result["device_output"] = playback.device_report
        (output / "delivery.json").write_text(json.dumps(result, indent=2) + "\n")
        print(f"{name}: {len(playback.underruns)} underruns, "
              f"{playback.silence_frames / RATE:.3f}s silence, {len(retries)} cancelled downloads; "
              f"{upstream_bytes:,} upstream bytes ({source_cache})", flush=True)
        return result
    finally:
        playback.cancel.set()
        playback.ready.set()
        if audio is not None:
            playback.stop()
        player.join(timeout=3)
        nav.require(not player.is_alive(), "consumer did not stop")
        if session is not None:
            session.close()


def validate_source(info, size, synthetic):
    nav.require(info["sample_rate"] == RATE and info["channels"] == 2 and info["bits_per_sample"] in (16, 24),
                "live output requires native 48 kHz stereo 16/24-bit lossless audio")
    nav.require(ORIGIN + TOTAL + nav.CONTEXT <= info["frames"] <= 600 * RATE and size <= nav.MAX_BYTES,
                "source must cover the 1:20–1:52 excerpt plus context, and fit within 10 minutes / 96 MiB")
    if synthetic:
        nav.require(info["frames"] == 160 * RATE + 317 and info["bits_per_sample"] == 24,
                    "use the synthetic Navidrome fixture, or --source for music")


def validate_device_timing(device, duration):
    nav.require(device["timeline_discontinuities"] == 0, "device timeline changed during playback")
    # AudioQueue time is time since queue start, not a count of content frames.
    # Stop(false) drains the device before IsRunning becomes false, so the final
    # clock can extend beyond the content, particularly with Bluetooth output.
    # Check it against the same run's wall clock and separately require it to
    # cover the content. Exact submitted frames are checked by live_check.
    clock = device["queue_sample_time"] / RATE
    elapsed = device["elapsed_seconds"]
    nav.require(duration - 0.06 <= elapsed <= duration + 3, "device drain duration out of bounds")
    nav.require(clock >= duration - 0.06 and abs(clock - elapsed) < 0.25,
                "device queue clock did not cover output or disagreed with elapsed time")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    for name in ("navidrome", "stratum", "checker", "out"):
        parser.add_argument("--" + name, required=True, type=Path)
    source_args = parser.add_mutually_exclusive_group(required=True)
    source_args.add_argument("--fixture", type=Path, help="Original deterministic synthetic fixture")
    source_args.add_argument("--source", type=Path, help="Local 48 kHz stereo FLAC/WAV music for listening")
    parser.add_argument("--scenario", choices=("all", "steady", "adaptive", "forced-lossless"), default="all")
    parser.add_argument("--source-cache", choices=("session", "per-segment"), default="session",
                        help="Reuse source reads across segments, or reproduce the original baseline")
    parser.add_argument("--audio-helper", type=Path, help="Built macOS AudioQueue helper (explicit opt-in)")
    parser.add_argument("--audio-device", help="Exact output UID from the helper's --list")
    parser.add_argument("--audio-gain", type=float, default=0.25, help="Test-only gain, 0..1; default -12 dB")
    args = parser.parse_args()
    nav.require(bool(args.audio_helper) == bool(args.audio_device), "provide both audio helper and output UID")
    nav.require(0 <= args.audio_gain <= 1, "audio gain must be between zero and one")
    audio = None
    if args.audio_helper:
        nav.require(args.scenario != "all", "choose one scenario for an audible test")
        audio = {"helper": args.audio_helper.resolve(strict=True), "device": args.audio_device, "gain": args.audio_gain}
    for name in ("navidrome", "stratum", "checker"):
        setattr(args, name, getattr(args, name).resolve(strict=True))
    source = (args.source or args.fixture).resolve(strict=True)
    info = json.loads(nav.run([str(args.stratum), "inspect", "--source", str(source)]))
    validate_source(info, source.stat().st_size, synthetic=args.fixture is not None)
    source_seconds = info["frames"] / RATE if args.source else 160
    args.out.mkdir(parents=True, exist_ok=False)
    args.out = args.out.resolve()
    names = ("steady", "adaptive", "forced-lossless") if args.scenario == "all" else (args.scenario,)
    reports = []
    for name in names:
        # A separate disposable server for each bounded run; no persistent account.
        with tempfile.TemporaryDirectory(prefix="stratum-live-nav-") as state:
            with nav.disposable(args.navidrome, source, Path(state)) as (server, song, _):
                reports.append(scenario(name, server, song, args.stratum, args.out / name,
                                        args.source_cache, audio, source_seconds))
        report = reports[-1]
        report["server_stopped"] = True
        # Full local source decode is an after-the-run oracle, never client input.
        checks = json.loads(nav.run([str(args.checker), str(source), str(args.out / name / "delivery.json")]))
        report["audio_checks"] = checks
        raw = args.out / name / "rendered.s32le"
        nav.run(nav.FFMPEG + ["-f", "s32le", "-ar", str(RATE), "-ac", "2", "-i", str(raw),
                             "-c:a", "pcm_s24le", str(args.out / name / "listening.wav")])
    result = {"experiment": "stratum-live-delivery-poc-1", "scenarios": reports,
              "source_kind": "music" if args.source else "synthetic", "source_info": info,
              "profile": {"fast_bits_per_second": 10000000, "slow_bits_per_second": 192000,
                          "slow_from_playback_seconds": 5, "recovery_seconds": 17,
                          "response_delay_ms": 60},
              "scope": ("Real HTTP and native AudioQueue output; pre-gain PCM capture, not an acoustic recording or bit-perfect/Bluetooth-fidelity test."
                        if audio else "Real HTTP and wall-clock PCM consumption, file sink; no audio device or Astra integration.")}
    (args.out / "report.json").write_text(json.dumps(result, indent=2) + "\n")
    for report in reports:
        checks = report["audio_checks"]
        nav.require(checks["exact_frames"] and checks["lossless_exact"] and checks["zero_lag"] and checks["inconclusive"] == 0,
                    "audio verification failed")
        nav.require(report["acquired_after_playback_started"] > 0 and report["received_after_playback_started"] > 0,
                    "test was accidentally fully buffered")
        if report["scenario"] == "forced-lossless":
            nav.require(report["silence_frames"] > 0, "negative control did not produce a stall")
        else:
            nav.require(report["silence_frames"] == 0, "playback starved")
        duration = (TOTAL + report["silence_frames"]) / RATE
        nav.require(checks["rendered_matches"] and abs(report["elapsed_seconds"] - duration) < (3 if audio else 0.1),
                    "consumer output/clock mismatch")
        if audio:
            validate_device_timing(report["device_output"], duration)
        nav.require(not any("error" in r for r in report["server_requests"]), "server request failed")
        if report["scenario"] == "adaptive":
            nav.require(report["retries"] and any(s["quality"] == "opus-64" for s in report["segments"])
                        and report["segments"][-1]["quality"] == "lossless", "downshift/recovery not exercised")
    print(f"Checks passed. Report: {args.out / 'report.json'}", flush=True)


if __name__ == "__main__":
    main()

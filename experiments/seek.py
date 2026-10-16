#!/usr/bin/env python3
"""Bounded seek/skip races over real Navidrome HTTP; optional explicit audio output.

Five playback generations, one final 8-second excerpt. Deliberate seeks are
interruptions; this measures stale-audio exclusion and exact resume positions,
not gaplessness at a user-requested jump. Test wire format, not a public protocol.
"""
import argparse
import base64
import contextlib
import hashlib
import http.client
import http.server
import json
from pathlib import Path
import secrets
import socket
import tempfile
import threading
import time
import urllib.parse

import live
from handoff import acquisition_interval, validate_envelope as validate_native_envelope
import navidrome as nav
from seek_output import FileTimeline, NativeTimeline
from source_session import SourceSession

RATE, SEGMENT, FRAME_BYTES = live.RATE, live.SEGMENT, live.FRAME_BYTES


class Ticket:
    """Only this request's socket/processes are cancelled; late completion is possible."""
    def __init__(self):
        self.cancelled = threading.Event()
        self.lock, self.socket = threading.Lock(), None

    def attach(self, sock):
        with self.lock:
            self.socket = sock
            if self.cancelled.is_set():
                self.shutdown()
                raise nav.Cancelled("superseded before connect completed")

    def shutdown(self):
        if self.socket is not None:
            with contextlib.suppress(OSError):
                self.socket.shutdown(socket.SHUT_RDWR)

    def cancel(self):
        self.cancelled.set()
        with self.lock:
            self.shutdown()

    def check(self):
        if self.cancelled.is_set():
            raise nav.Cancelled("superseded request")


class SeekServer:
    def __init__(self, account, songs, infos, binary, output, clock, max_generations=5, max_requests=80):
        nav.require(1 <= max_generations <= 64 and 1 <= max_requests <= 2000, "invalid proof limits")
        self.max_generations, self.max_requests = max_generations, max_requests
        self.account, self.songs, self.infos = account, songs, infos
        self.binary, self.output, self.clock = binary, output, clock
        self.lock, self.rate_lock = threading.Lock(), threading.Lock()
        self.records, self.sessions, self.cancelled = [], {}, {}
        self.route = "/" + secrets.token_urlsafe(24)

    def session_key(self, generation, track):
        return generation

    def before_request(self, handler):
        return True

    def flag(self, generation):
        with self.lock:
            return self.cancelled.setdefault(generation, threading.Event())

    def rate(self):
        elapsed = 0 if self.clock.started is None else time.monotonic() - self.clock.started
        return 24000 if 5 <= elapsed < 17 else 1250000

    def write(self, handler, data, cancel):
        with self.rate_lock:
            remaining, previous = len(data), time.monotonic()
            while remaining > 0:
                if cancel.wait(min(0.02, remaining / self.rate())):
                    raise nav.Cancelled("server download cancelled")
                now = time.monotonic()
                remaining -= (now - previous) * self.rate()
                previous = now
            handler.wfile.write(data)

    @contextlib.contextmanager
    def serve(self):
        owner = self
        class Handler(http.server.BaseHTTPRequestHandler):
            def log_message(self, *_):
                pass

            def do_DELETE(self):
                try:
                    url = urllib.parse.urlsplit(self.path)
                    params = urllib.parse.parse_qs(url.query, strict_parsing=True)
                    gen = int(params["generation"][0])
                    nav.require(url.path == owner.route and set(params) == {"generation"} and 0 <= gen < owner.max_generations,
                                "invalid cancellation")
                    owner.flag(gen).set()
                    self.send_response(204)
                    self.send_header("Connection", "close")
                    self.end_headers()
                except Exception:
                    self.send_error(400)

            def do_GET(self):
                record = None
                try:
                    self.connection.settimeout(5)
                    self.connection.setsockopt(socket.SOL_SOCKET, socket.SO_SNDBUF, 4096)
                    url = urllib.parse.urlsplit(self.path)
                    params = urllib.parse.parse_qs(url.query, strict_parsing=True)
                    nav.require(url.path == owner.route and set(params) in ({"generation", "track", "start", "quality"},
                                    {"generation", "track", "start", "frames", "quality"})
                                and all(len(v) == 1 for v in params.values()),
                                "invalid request")
                    gen, track, start = (int(params[k][0]) for k in ("generation", "track", "start"))
                    quality = params["quality"][0]
                    nav.require(0 <= gen < owner.max_generations and 0 <= track < len(owner.infos) and quality in live.QUALITIES, "invalid identity")
                    rate = owner.infos[track]["sample_rate"]
                    frames = int(params.get("frames", [str(SEGMENT)])[0])
                    nav.require(0 < frames <= 2 * rate + 1 and 0 <= start
                                and start + frames <= owner.infos[track]["frames"], "invalid source interval")
                    begin, count, trim = acquisition_interval(start, frames, owner.infos[track]["frames"], rate)
                    cancel = owner.flag(gen)
                    if not owner.before_request(self):
                        return
                    with owner.lock:
                        if cancel.is_set():
                            raise nav.Cancelled("generation already retired")
                        nav.require(len(owner.records) < owner.max_requests, "request budget exceeded")
                        key = owner.session_key(gen, track)
                        if key not in owner.sessions:
                            owner.sessions[key] = SourceSession(owner.account, owner.songs[track]["id"])
                        session = owner.sessions[key]
                        record = {"generation": gen, "track": track, "source_start": start, "quality": quality,
                                  "frames": frames, "sample_rate": rate, "began": time.monotonic(), "sent_bytes": 0}
                        owner.records.append(record)
                        self.proof_record = record
                        work = owner.output / f"request-{len(owner.records):03d}"
                        work.mkdir()
                    if cancel.is_set():
                        raise nav.Cancelled("cancelled before acquisition")
                    source = work / "source.flac"
                    record["acquisition"] = nav.acquire(owner.account, owner.songs[track]["id"], begin,
                        count, source, session=session, cancel=cancel, rate=rate)
                    segment = json.loads(nav.run([str(owner.binary), "segment", "--source", str(source),
                        "--start-frame", str(trim), "--frames", str(frames), "--quality", quality,
                        "--cache", str(work / "cache")], cancel=cancel))
                    payload = Path(segment["path"]).read_bytes()
                    nav.require(len(payload) <= live.MAX_PAYLOAD, "oversized segment")
                    record["prepared"] = time.monotonic()
                    record["payload_bytes"] = len(payload)
                    if cancel.wait(0.06):
                        raise nav.Cancelled("cancelled before headers")
                    envelope = {"generation": gen, "track": track, "source_start": start,
                                "frames": frames, "metadata": segment["metadata"]}
                    self.send_response(200)
                    self.send_header("Content-Length", str(len(payload)))
                    self.send_header("X-Stratum-Test", base64.b64encode(json.dumps(envelope).encode()).decode())
                    self.send_header("Connection", "close")
                    self.end_headers()
                    for pos in range(0, len(payload), 4096):
                        chunk = payload[pos:pos + 4096]
                        owner.write(self, chunk, cancel)
                        record["sent_bytes"] += len(chunk)
                except (nav.Cancelled, BrokenPipeError, ConnectionResetError):
                    if record is not None:
                        record["cancelled"] = True
                    self.close_connection = True
                except Exception as error:
                    if record is not None:
                        record["error"] = str(error)
                    self.close_connection = True
                finally:
                    if record is not None:
                        record["finished"] = time.monotonic()

        server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        server.daemon_threads = True
        thread = threading.Thread(target=server.serve_forever, daemon=True)
        thread.start()
        self.port = server.server_port
        try:
            yield self
        finally:
            for flag in self.cancelled.values():
                flag.set()
            server.shutdown()
            server.server_close()
            thread.join(timeout=2)
            deadline = time.monotonic() + 35
            while any("finished" not in r for r in self.records) and time.monotonic() < deadline:
                time.sleep(0.02)
            nav.require(all("finished" in r for r in self.records), "server work remained")
            for session in self.sessions.values():
                session.close()

    def cancel(self, generation):
        connection = http.client.HTTPConnection("127.0.0.1", self.port, timeout=3)
        try:
            connection.request("DELETE", self.route + "?generation=" + str(generation))
            with connection.getresponse() as response:
                nav.require(response.status == 204, "server refused cancellation")
        finally:
            connection.close()


def validate_envelope(envelope, generation, track, source_start, quality, frames=SEGMENT, rate=RATE):
    nav.require((envelope["generation"], envelope["track"], envelope["source_start"], envelope["frames"])
                == (generation, track, source_start, frames), "response belongs to another playback position")
    return validate_native_envelope(envelope, {"generation": generation, "track": track,
        "source_start": source_start, "frames": frames}, quality, rate)


class Client:
    def __init__(self, server, sink, output, infos):
        self.server, self.sink, self.output, self.infos = server, sink, output, infos
        self.lock = threading.RLock()
        self.generation = -1
        self.plans, self.tickets, self.workers, self.jobs, self.events = {}, {}, [], [], []
        self.error = None
        self.held, self.release = threading.Event(), threading.Event()
        self.stale_rejected = 0
        self.policy = live.Policy(infos[0]["bytes"] / (infos[0]["frames"] / RATE) * SEGMENT / RATE)

    def switch(self, track, origin, total, reason):
        requested = time.monotonic()
        with self.lock:
            old = self.generation
            self.generation += 1
            generation = self.generation
            if old >= 0:
                self.tickets[old].cancel()
                self.sink.switch(generation, total)
            self.plans[generation] = {"generation": generation, "track": track, "origin": origin, "total": total,
                                      "requested": requested, "flushed": time.monotonic(), "segments": []}
            self.tickets[generation] = Ticket()
            self.events.append({"generation": generation, "reason": reason, "track": track, "origin": origin,
                                "requested": requested, "output_ready": time.monotonic()})
        if old >= 0:
            self.server.cancel(old)
        thread = threading.Thread(target=self.produce, args=(generation,), daemon=True)
        self.workers.append(thread)
        thread.start()
        print(f"generation {generation}: {reason} → track {track + 1} at {origin / RATE:.3f}s", flush=True)

    def deliver(self, generation, offset, pcm, segment, observation):
        # Generation check, policy update and append are atomic with switch().
        # Cancellation alone cannot provide this guarantee for completed work.
        with self.lock:
            if generation != self.generation:
                self.stale_rejected += 1
                return False
            if not self.sink.append(generation, offset, pcm):
                raise RuntimeError("current generation rejected by sink")
            self.policy.observe(*observation)
            self.plans[generation]["segments"].append(segment)
            return True

    def produce(self, generation):
        plan, ticket = self.plans[generation], self.tickets[generation]
        try:
            for offset in range(0, plan["total"], SEGMENT):
                while self.sink.snapshot(generation)["buffered"] >= live.TARGET_BUFFER / RATE:
                    ticket.check()
                    ticket.cancelled.wait(0.02)
                ticket.check()
                with self.lock:
                    if generation != self.generation:
                        raise nav.Cancelled("stale producer")
                    buffer = self.sink.snapshot(generation)["buffered"]
                    quality = self.policy.choose(buffer) if generation or self.sink.started is not None else "lossless"
                try:
                    self.fetch(generation, offset, quality)
                except live.RetryLower:
                    self.fetch(generation, offset, "opus-64")
        except (nav.Cancelled, OSError, http.client.HTTPException) as error:
            if not ticket.cancelled.is_set():
                self.error = error
        except BaseException as error:
            if not ticket.cancelled.is_set():
                self.error = error

    def fetch(self, generation, offset, quality):
        plan, ticket = self.plans[generation], self.tickets[generation]
        ticket.check()
        source_start = plan["origin"] + offset
        job = {"generation": generation, "offset": offset, "quality": quality, "stage": "connecting",
               "began": time.monotonic(), "received_bytes": 0}
        self.jobs.append(job)
        connection = http.client.HTTPConnection("127.0.0.1", self.server.port, timeout=5)
        try:
            connection.connect()
            ticket.attach(connection.sock)
            connection.request("GET", self.server.route + "?" + urllib.parse.urlencode({"generation": generation,
                "track": plan["track"], "start": source_start, "quality": quality}))
            with connection.getresponse() as response:
                nav.require(response.status == 200, "segment request failed")
                size = int(response.headers["Content-Length"])
                nav.require(0 < size <= live.MAX_PAYLOAD, "invalid response size")
                meta = validate_envelope(json.loads(base64.b64decode(response.headers["X-Stratum-Test"])),
                                         generation, plan["track"], source_start, quality)
                body_started = time.monotonic()
                job.update(stage="download", body_started=body_started)
                body = bytearray()
                while len(body) < size:
                    ticket.check()
                    chunk = response.read(min(4096, size - len(body)))
                    nav.require(bool(chunk), "truncated segment")
                    body.extend(chunk)
                    job["received_bytes"] = len(body)
                    with self.lock:
                        ticket.check()
                        if self.sink.started is not None and self.policy.endangered(len(body), size,
                                time.monotonic() - body_started, self.sink.snapshot(generation)["buffered"]):
                            job["retry_lower"] = True
                            raise live.RetryLower()
                received = time.monotonic()
            ticket.check()
            nav.require(hashlib.sha256(body).hexdigest() == meta["payload_sha256"], "corrupt payload")
            folder = self.output / f"generation-{generation}"
            encoded = folder / (f"segment-{offset}." + ("flac" if quality == "lossless" else "opus"))
            encoded.write_bytes(body)
            job["stage"] = "decode"
            pcm = nav.run(nav.FFMPEG + ["-i", str(encoded), "-map", "0:a:0", "-f", "s32le",
                                      "-c:a", "pcm_s32le", "pipe:1"], cancel=ticket.cancelled)
            nav.require(len(pcm) == meta["decoded_frames"] * FRAME_BYTES, "decoded frame count mismatch")
            begin = meta["discard_start_frames"] * FRAME_BYTES
            pcm = pcm[begin:begin + SEGMENT * FRAME_BYTES]
            decoded = time.monotonic()
            pcm_name = f"segment-{offset}.s32le"
            (folder / pcm_name).write_bytes(pcm)
            segment = {"start": offset, "frames": SEGMENT, "quality": quality, "pcm": pcm_name}
            # Force one already-completed old result to arrive AFTER newer seeks.
            # This deliberately bypasses cancellation, as a real completion race can.
            played = self.sink.snapshot(generation)["started"]
            if generation == 1 and played is not None and decoded - played >= 0.5 and not self.held.is_set():
                job["stage"] = "held-completion"
                self.held.set()
                nav.require(self.release.wait(8), "late-completion test barrier timed out")
            accepted = self.deliver(generation, offset, pcm, segment,
                                   (size, received - body_started, body_started - job["began"] + decoded - received))
            job["stage"] = "accepted" if accepted else "stale-rejected"
        except BaseException:
            job["cancelled"] = ticket.cancelled.is_set()
            raise
        finally:
            connection.close()
            job["finished"] = time.monotonic()

    def wait(self, predicate, message, timeout=25):
        deadline = time.monotonic() + timeout
        while not predicate():
            if self.error:
                raise self.error
            nav.require(time.monotonic() < deadline, message)
            self.sink.snapshot(self.generation)
            time.sleep(0.01)

    def script(self):
        self.switch(0, 80 * RATE + 37, live.TOTAL, "start")
        self.wait(lambda: any(j["generation"] == 0 and j["stage"] == "download" and j["quality"] == "lossless"
            and "finished" not in j and j["received_bytes"] >= 8192 and self.sink.started is not None
            and time.monotonic() - self.sink.started >= 5.1 for j in self.jobs), "no slow download to interrupt")
        self.switch(0, 40 * RATE + 71, live.TOTAL, "seek backward during download")
        self.wait(self.held.is_set, "no completed quality-change result to race")
        self.switch(0, 60 * RATE + 113, live.TOTAL, "seek during quality change")
        self.wait(lambda: any(r["generation"] == 2 for r in self.server.records), "intermediate seek never requested", timeout=5)
        self.switch(0, 120 * RATE + 151, live.TOTAL, "rapid newer seek supersedes the previous seek")
        self.release.set()
        self.wait(lambda: self.stale_rejected == 1, "late result was not rejected", timeout=5)
        if isinstance(self.sink, NativeTimeline):
            self.sink.inject_stale(1)
        self.wait(lambda: self.sink.snapshot(3)["started"] is not None
                  and time.monotonic() - self.sink.snapshot(3)["started"] >= 1, "latest seek never resumed")
        self.wait(lambda: any(j["generation"] == 3 and "finished" not in j for j in self.jobs), "no pending request at skip")
        self.switch(1, 20 * RATE + 197, 4 * SEGMENT, "skip to second track during pending work")
        self.sink.finish()
        if self.error:
            raise self.error

    def stop(self):
        for ticket in self.tickets.values():
            ticket.cancel()
        self.release.set()
        for thread in self.workers:
            thread.join(timeout=10)
            nav.require(not thread.is_alive(), "client worker remained")
        self.sink.stop()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    for name in ("navidrome", "stratum", "checker", "source", "skip-source", "out"):
        parser.add_argument("--" + name, type=Path, required=True)
    parser.add_argument("--audio-helper", type=Path)
    parser.add_argument("--audio-device")
    parser.add_argument("--audio-gain", type=float, default=0.25)
    args = parser.parse_args()
    nav.require(bool(args.audio_helper) == bool(args.audio_device) and 0 <= args.audio_gain <= 1, "invalid audio options")
    for name in ("navidrome", "stratum", "checker", "source", "skip_source", "audio_helper"):
        if getattr(args, name):
            setattr(args, name, getattr(args, name).resolve(strict=True))
    sources = [args.source, args.skip_source]
    infos = [json.loads(nav.run([str(args.stratum), "inspect", "--source", str(p)])) for p in sources]
    for info in infos:
        live.validate_source(info, info["bytes"], synthetic=False)
    nav.require(infos[0]["frames"] >= 120 * RATE + 151 + live.TOTAL + nav.CONTEXT, "first source too short for late seek")
    nav.require(infos[0]["sha256"] != infos[1]["sha256"], "skip test requires two different sources")
    args.out.mkdir(parents=True, exist_ok=False)
    args.out = args.out.resolve()
    with tempfile.TemporaryDirectory(prefix="stratum-seek-nav-") as temporary:
        with nav.disposable_library(args.navidrome, sources, Path(temporary)) as (account, songs, _):
            sink = (NativeTimeline(args.out, live.TOTAL, args.audio_helper, args.audio_device, args.audio_gain)
                    if args.audio_helper else FileTimeline(args.out, live.TOTAL))
            client = None
            try:
                with SeekServer(account, songs, infos, args.stratum, args.out, sink).serve() as server:
                    client = Client(server, sink, args.out, infos)
                    client.script()
            finally:
                if client is not None:
                    client.stop()
                else:
                    sink.stop()
    checks, generations = [], []
    for generation, plan in client.plans.items():
        snapshot = sink.snapshot(generation)
        folder = args.out / f"generation-{generation}"
        delivery = {"origin": plan["origin"], "frames": sum(s["frames"] for s in plan["segments"]),
                    "rendered_media_frames": snapshot["consumed"], "segments": plan["segments"],
                    "underruns": snapshot.get("underruns", [])}
        nav.require(delivery["frames"] == snapshot["accepted"], "sink and admission counts disagree")
        (folder / "delivery.json").write_text(json.dumps(delivery, indent=2) + "\n")
        if delivery["frames"]:
            checked = json.loads(nav.run([str(args.checker), str(sources[plan["track"]]), str(folder / "delivery.json")]))
            nav.require(all(checked[k] for k in ("exact_frames", "lossless_exact", "rendered_matches", "zero_lag"))
                        and checked["inconclusive"] == 0, "seek audio verification failed")
        else:
            nav.require(not (folder / "rendered.s32le").read_bytes(), "superseded startup produced audio")
            checked = {"no_audio": True}
        nav.require(snapshot.get("silence_frames", 0) == 0, "resumed playback starved")
        device = snapshot.get("device_report")
        if device:
            nav.require(device["timeline_discontinuities"] == 0, "device timeline changed")
            if generation == client.generation:
                live.validate_device_timing(device, snapshot["consumed"] / RATE)
        checks.append(checked)
        generations.append({**plan, **snapshot, "resume_seconds": None if snapshot["started"] is None
                            else snapshot["started"] - plan["requested"]})
    nav.require(client.stale_rejected == 1 and sink.snapshot(2)["consumed"] == 0, "latest seek did not win")
    nav.require(sink.snapshot(4)["consumed"] == 4 * SEGMENT, "skip did not finish second track")
    nav.require(not any("error" in r for r in server.records), "server request failed")
    nav.require(any(r.get("cancelled") and r["generation"] == 0 for r in server.records), "download was not cancelled")
    result = {"experiment": "stratum-seek-poc-1", "generations": generations, "events": client.events,
              "client_jobs": client.jobs, "server_requests": server.records, "audio_checks": checks,
              "stale_completions_rejected": client.stale_rejected, "server_stopped": True,
              "source_sessions": {generation: session.summary() for generation, session in server.sessions.items()},
              "native_stale_packets_rejected": sink.rejected if isinstance(sink, NativeTimeline) else None,
              "scope": "Intentional seek pauses; pre-gain native submissions or wall-clock file capture. No Astra integration."}
    (args.out / "report.json").write_text(json.dumps(result, indent=2) + "\n")
    print(f"Seek/skip checks passed. Report: {args.out / 'report.json'}", flush=True)


if __name__ == "__main__":
    main()

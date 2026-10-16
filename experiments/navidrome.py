#!/usr/bin/env python3
"""Disposable, loopback-only source acquisition experiment. Standard library only.

Starts a fresh Navidrome, imports a synthetic fixture, checks late HTTP seeking,
then exercises the existing Rust audio engine with that acquired interval.
Never connects to an existing server or reads Astra's credentials/settings.
"""
import argparse
from collections import OrderedDict
import contextlib
import hashlib
import http.server
import json
import os
from pathlib import Path
import re
import secrets
import shutil
import signal
import socket
import subprocess
import tempfile
import threading
import time
import urllib.error
import urllib.parse
import urllib.request

RATE = 48000
CHANNELS = 2
CONTEXT = 5760
TIMEOUT = 30
MAX_BYTES = 96 * 1024 * 1024
BLOCK_BYTES = 256 * 1024
MAX_CACHED_BLOCKS = 32
FFMPEG = ["ffmpeg", "-hide_banner", "-loglevel", "error", "-nostdin", "-y",
          "-threads", "1", "-filter_threads", "1"]


class NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, req, fp, code, msg, headers, newurl):
        return None


HTTP = urllib.request.build_opener(urllib.request.ProxyHandler({}), NoRedirect())


def require(condition, message):
    if not condition:
        raise RuntimeError(message)


class Cancelled(RuntimeError):
    pass


def run(command, timeout=TIMEOUT, cancel=None):
    if cancel is None:
        result = subprocess.run(command, capture_output=True, timeout=timeout)
    else:
        with subprocess.Popen(command, stdout=subprocess.PIPE, stderr=subprocess.PIPE, start_new_session=True) as process:
            deadline = time.monotonic() + timeout
            try:
                while True:
                    if cancel.is_set():
                        raise Cancelled("request cancelled")
                    if time.monotonic() >= deadline:
                        raise subprocess.TimeoutExpired(command, timeout)
                    try:
                        stdout, stderr = process.communicate(timeout=min(0.05, deadline - time.monotonic()))
                        break
                    except subprocess.TimeoutExpired:
                        continue
                if cancel.is_set():
                    raise Cancelled("request cancelled")
                result = subprocess.CompletedProcess(command, process.returncode, stdout, stderr)
            finally:
                stop_group(process)
    if result.returncode:
        # Commands never contain Navidrome credentials. Still avoid printing URLs.
        raise RuntimeError(f"{Path(command[0]).name} failed: "
                           + result.stderr.decode(errors="replace")[-4000:])
    return result.stdout


def json_request(url, body=None):
    data = None if body is None else json.dumps(body).encode()
    request = urllib.request.Request(url, data=data,
                                    headers={"Content-Type": "application/json"})
    try:
        with HTTP.open(request, timeout=5) as response:
            payload = response.read(1024 * 1024 + 1)
            if len(payload) > 1024 * 1024:
                raise RuntimeError("metadata response exceeded its limit")
            return json.loads(payload)
    except urllib.error.HTTPError as error:
        raise RuntimeError(f"Navidrome metadata request failed (HTTP {error.code})") from None


class Navidrome:
    def __init__(self, base, username, password):
        self.base = base
        salt = secrets.token_hex(12)
        self.auth = {"u": username, "s": salt,
                     "t": hashlib.md5((password + salt).encode()).hexdigest(),
                     "v": "1.16.1", "c": "stratum-poc", "f": "json"}

    def url(self, endpoint, **params):
        return self.base + "/rest/" + endpoint + ".view?" + urllib.parse.urlencode({**self.auth, **params})

    def request(self, endpoint, **params):
        body = json_request(self.url(endpoint, **params))["subsonic-response"]
        if body.get("status") != "ok":
            raise RuntimeError("Navidrome refused request (Subsonic "
                               + str(body.get("error", {}).get("code", "unknown")) + ")")
        return body


def stop_group(process):
    if process.poll() is None:
        with contextlib.suppress(ProcessLookupError):
            os.killpg(process.pid, signal.SIGTERM)
        try:
            process.wait(timeout=4)
        except subprocess.TimeoutExpired:
            with contextlib.suppress(ProcessLookupError):
                os.killpg(process.pid, signal.SIGKILL)
            process.wait(timeout=3)


@contextlib.contextmanager
def disposable(navidrome_binary, fixture, state):
    with disposable_library(navidrome_binary, [fixture], state) as (nav, songs, bad_auth):
        yield nav, songs[0], bad_auth


def identify_test_songs(nav, fixtures, songs):
    """Fixture setup only: Subsonic paths may be virtual tag-based paths.

    Match size plus a bounded original-byte prefix; reject ambiguous matches.
    These setup reads are separate from measured playback acquisition traffic.
    """
    signatures = []
    for source in fixtures:
        with source.open("rb") as file:
            signatures.append((source.stat().st_size, file.read(65536)))
    ordered = [None] * len(fixtures)
    for song in songs:
        size = int(song["size"])
        length = min(size, 65536)
        request = urllib.request.Request(nav.url("stream", id=song["id"], format="raw", maxBitRate=0),
                                         headers={"Range": f"bytes=0-{length - 1}", "Accept-Encoding": "identity"})
        with HTTP.open(request, timeout=5) as response:
            require(response.status == 206 and response.headers.get("Content-Range") == f"bytes 0-{length - 1}/{size}",
                    "test track identity range rejected")
            prefix = response.read(length + 1)
        require(len(prefix) == length, "test track identity prefix truncated")
        matches = [index for index, signature in enumerate(signatures) if signature == (size, prefix)]
        require(len(matches) == 1 and ordered[matches[0]] is None, "test track mapping ambiguous")
        ordered[matches[0]] = song
    require(all(ordered), "test track mapping incomplete")
    return ordered


@contextlib.contextmanager
def disposable_library(navidrome_binary, fixtures, state, lifetime=180):
    require(1 <= len(fixtures) <= 3, "test library must contain one to three tracks")
    require(30 <= lifetime <= 900, "invalid disposable server lifetime")
    music = state / "music"
    music.mkdir()
    names = [("Stratum fixture" if len(fixtures) == 1 else f"Stratum fixture {index}") + fixture.suffix.lower()
             for index, fixture in enumerate(fixtures)]
    for index, fixture in enumerate(fixtures):
        shutil.copy2(fixture, music / names[index])
    with socket.socket() as reservation:
        reservation.bind(("127.0.0.1", 0))
        port = reservation.getsockname()[1]
    env = {k: v for k, v in os.environ.items() if not k.startswith("ND_")}
    env.update({"ND_ADDRESS": "127.0.0.1", "ND_PORT": str(port),
                "ND_MUSICFOLDER": str(music), "ND_DATAFOLDER": str(state / "data"),
                "ND_CACHEFOLDER": str(state / "cache"), "ND_LOGLEVEL": "warn",
                "ND_ENABLEEXTERNALSERVICES": "false", "ND_ENABLEINSIGHTSCOLLECTOR": "false",
                "ND_ENABLEGRAVATAR": "false", "ND_ENABLEARTWORKPRECACHE": "false",
                "ND_FFMPEGPATH": shutil.which("ffmpeg"), "ND_SCANNER_SCHEDULE": "0"})
    with (state / "navidrome.log").open("wb") as log:
        process = subprocess.Popen([str(navidrome_binary)], cwd=state, env=env,
                                   stdin=subprocess.DEVNULL, stdout=log, stderr=log,
                                   start_new_session=True)
        # Hard lifetime bound independent of the test's happy path.
        watchdog = threading.Timer(lifetime, stop_group, args=(process,))
        watchdog.daemon = True
        watchdog.start()
        try:
            base = f"http://127.0.0.1:{port}"
            deadline = time.monotonic() + 20
            while True:
                if process.poll() is not None:
                    raise RuntimeError("disposable Navidrome exited during startup: "
                                       + (state / "navidrome.log").read_text(errors="replace")[-4000:])
                try:
                    with HTTP.open(base + "/ping", timeout=0.5) as response:
                        if response.status == 200:
                            break
                except (OSError, urllib.error.URLError):
                    pass
                if time.monotonic() > deadline:
                    raise RuntimeError("disposable Navidrome did not start in 20 seconds")
                time.sleep(0.1)
            username, password = "stratum-test", secrets.token_urlsafe(24)
            json_request(base + "/auth/createAdmin", {"username": username, "password": password})
            nav = Navidrome(base, username, password)
            nav.request("startScan")
            deadline = time.monotonic() + 30
            while True:
                songs = nav.request("getRandomSongs", size=10).get("randomSongs", {}).get("song", [])
                if len(songs) == len(fixtures):
                    break
                if time.monotonic() > deadline:
                    raise RuntimeError("Navidrome did not scan the test track in 30 seconds")
                time.sleep(0.2)
            if len(fixtures) > 1:
                songs = identify_test_songs(nav, fixtures, songs)
            yield nav, songs, Navidrome(base, username, "deliberately-wrong-password")
        finally:
            watchdog.cancel()
            stop_group(process)


class RangeSource:
    """Bounded original-byte acquisition; temporary, per-user/per-track cache.

    The decoder may see an open-ended file, but each upstream request is bounded.
    Reject ignored ranges, changing validators, wrong ranges, and truncated data.
    """
    def __init__(self, url, size):
        self.url = url
        self.size = size
        self.blocks = OrderedDict()
        self.records = []
        self.hits = 0
        self.validator = None
        self.lock = threading.Lock()
        self.invalid = False
        self.peak_cached_bytes = 0

    def invalidate(self):
        with self.lock:
            self.invalid = True
            self.blocks.clear()

    def _read(self, start, end, purpose="block"):
        require(not self.invalid, "source session is invalid")
        headers = {"Range": f"bytes={start}-{end}", "Accept-Encoding": "identity"}
        if self.validator:
            headers["If-Range"] = self.validator
        try:
            with HTTP.open(urllib.request.Request(self.url, headers=headers), timeout=5) as response:
                expected_range = f"bytes {start}-{end}/{self.size}"
                require(response.status == 206 and response.headers.get("Content-Range") == expected_range,
                        "upstream did not honor the exact byte range")
                require(response.headers.get("Content-Encoding", "identity") == "identity",
                        "encoded upstream body cannot be byte-addressed")
                etag = response.headers.get("ETag")
                validator = etag if etag and not etag.startswith("W/") else response.headers.get("Last-Modified")
                require(validator and (self.validator is None or validator == self.validator),
                        "upstream source validator absent or changed")
                self.validator = validator
                data = response.read(end - start + 2)
                self.records.append({"range": headers["Range"], "status": response.status,
                                     "content_range": response.headers.get("Content-Range"),
                                     "body_bytes_received": len(data), "complete": True,
                                     "purpose": purpose})
                require(len(data) == end - start + 1, "upstream returned an incomplete byte range")
                require(sum(r["body_bytes_received"] for r in self.records) <= MAX_BYTES,
                        "upstream transfer limit exceeded")
                return data
        except Exception:
            # A warm entry must never remain usable after a source/auth failure.
            self.invalid = True
            self.blocks.clear()
            raise

    def revalidate(self):
        """A real authenticated stream read, even if all decoder bytes are cached."""
        with self.lock:
            self._read(0, 0, purpose="validation")

    def block(self, index):
        with self.lock:
            require(not self.invalid, "source session is invalid")
            require(index >= 0, "negative source block")
            if index in self.blocks:
                self.blocks.move_to_end(index)
                self.hits += 1
                return self.blocks[index]
            start = index * BLOCK_BYTES
            end = min(start + BLOCK_BYTES, self.size) - 1
            if start > end:
                raise RuntimeError("block outside source")
            data = self._read(start, end)
            self.blocks[index] = data
            while len(self.blocks) > MAX_CACHED_BLOCKS:
                self.blocks.popitem(last=False)
            self.peak_cached_bytes = max(self.peak_cached_bytes, sum(map(len, self.blocks.values())))
            return data


@contextlib.contextmanager
def metered_source(nav, song_id, bounded=True, session=None):
    if session is None:
        with _metered_source(nav, song_id, bounded) as result:
            yield result
        return
    require(bounded, "source sessions require bounded ranges")
    # The lease covers decoder shutdown as well as authorization, so an old
    # gateway cannot continue using a cache after another acquisition revalidates.
    with session.lease(nav, song_id) as source:
        with _metered_source(nav, song_id, bounded, source) as (url, metrics):
            yield url, metrics
        metrics["upstream_requests"] = source.records[session.record_start:]
        metrics["block_cache_hits"] = source.hits - session.hit_start
        require(not any("error" in r for r in metrics["decoder_requests"]),
                "upstream acquisition had a transport error")


@contextlib.contextmanager
def _metered_source(nav, song_id, bounded=True, block_source=None):
    # Real identity/access check; the stream also uses this user's credentials.
    if block_source is None:
        song = nav.request("getSong", id=song_id)["song"]
        upstream_url = nav.url("stream", id=song_id, format="raw", maxBitRate=0)
        block_source = RangeSource(upstream_url, int(song["size"])) if bounded else None
    else:
        upstream_url = block_source.url
    route = "/" + secrets.token_urlsafe(24) + "/source.flac"
    records = []
    lock = threading.Lock()
    metrics = {"upstream_requests": block_source.records if bounded else records,
               "decoder_requests": records, "block_cache_hits": 0}

    class Handler(http.server.BaseHTTPRequestHandler):
        protocol_version = "HTTP/1.1"

        def log_message(self, *_):
            pass

        def do_GET(self):
            if self.path != route:
                self.send_error(404)
                return
            record = {"range": self.headers.get("Range"), "status": None,
                      "content_range": None, "body_bytes_received": 0,
                      "body_bytes_forwarded": 0, "complete": False}
            with lock:
                if len(records) >= 100:
                    self.send_error(429)
                    return
                records.append(record)
            if block_source is not None:
                self.serve_blocks(record)
                return
            headers = {"Accept-Encoding": "identity"}
            if record["range"]:
                headers["Range"] = record["range"]
            try:
                with HTTP.open(urllib.request.Request(upstream_url, headers=headers), timeout=5) as response:
                    record["status"] = response.status
                    record["content_range"] = response.headers.get("Content-Range")
                    if response.status not in (200, 206) or "json" in response.headers.get("Content-Type", "") or "xml" in response.headers.get("Content-Type", ""):
                        self.send_error(502, "Upstream did not return audio")
                        return
                    self.send_response(response.status)
                    for name in ["Content-Type", "Content-Length", "Content-Range", "Accept-Ranges"]:
                        if name in response.headers:
                            self.send_header(name, response.headers[name])
                    self.send_header("Connection", "close")
                    self.end_headers()
                    while True:
                        chunk = response.read(64 * 1024)
                        if not chunk:
                            break
                        with lock:
                            record["body_bytes_received"] += len(chunk)
                            if sum(r["body_bytes_received"] for r in records) > MAX_BYTES:
                                record["error"] = "transfer limit exceeded"
                                break
                        self.wfile.write(chunk)
                        record["body_bytes_forwarded"] += len(chunk)
            except (BrokenPipeError, ConnectionResetError):
                record["client_closed_early"] = True
            except Exception as error:
                # Do not save URLs or exceptions that may contain auth parameters.
                record["error"] = type(error).__name__
                self.close_connection = True
            finally:
                record["complete"] = True

        def serve_blocks(self, record):
            try:
                # Stop a fast loopback sender from fetching megabytes while the
                # decoder probes/seeks/closes its current input connection.
                self.connection.setsockopt(socket.SOL_SOCKET, socket.SO_SNDBUF, 16 * 1024)
                self.connection.settimeout(5)
                requested = record["range"] or f"bytes=0-{block_source.size - 1}"
                match = re.fullmatch(r"bytes=(\d+)-(\d*)", requested)
                if not match:
                    self.send_error(416)
                    return
                start = int(match[1])
                end = min(int(match[2]) if match[2] else block_source.size - 1, block_source.size - 1)
                if start > end or start >= block_source.size:
                    self.send_error(416)
                    return
                # Fetch/validate the first block before advertising success.
                block_source.block(start // BLOCK_BYTES)
                status = 206 if record["range"] else 200
                self.send_response(status)
                self.send_header("Content-Type", "audio/flac")
                self.send_header("Content-Length", str(end - start + 1))
                self.send_header("Accept-Ranges", "bytes")
                if status == 206:
                    self.send_header("Content-Range", f"bytes {start}-{end}/{block_source.size}")
                self.send_header("Connection", "close")
                self.end_headers()
                record["status"] = status
                while start <= end:
                    block = block_source.block(start // BLOCK_BYTES)
                    offset = start % BLOCK_BYTES
                    length = min(16 * 1024, len(block) - offset, end - start + 1)
                    self.wfile.write(block[offset:offset + length])
                    record["body_bytes_forwarded"] += length
                    start += length
            except (BrokenPipeError, ConnectionResetError):
                record["client_closed_early"] = True
            except Exception as error:
                record["error"] = str(error) if isinstance(error, RuntimeError) else type(error).__name__
                self.close_connection = True
            finally:
                record["complete"] = True

    server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    server.daemon_threads = True
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    try:
        yield f"http://127.0.0.1:{server.server_port}{route}", metrics
    finally:
        server.shutdown()
        server.server_close()
        thread.join(timeout=2)
        deadline = time.monotonic() + 6
        while any(not r["complete"] for r in records) and time.monotonic() < deadline:
            time.sleep(0.02)
        if any(not r["complete"] for r in records):
            raise RuntimeError("metered source requests did not finish")
        if bounded:
            metrics["block_cache_hits"] = block_source.hits


def acquire(nav, song_id, start, frames, output, bounded=True, session=None, cancel=None, rate=RATE):
    """Fetch an original-source interval over authenticated Navidrome HTTP.

    The temporary gateway keeps upstream credentials out of FFmpeg arguments and
    meters body bytes. Bounded mode caches original byte blocks for this acquisition,
    or across acquisitions when given an authorized private source session.
    It does not rewrite audio or implement production auth.
    """
    require(rate in (44100, 48000, 96000), "unsupported acquisition clock")
    seek_seconds, skip = divmod(start, rate)
    began = time.monotonic()
    with metered_source(nav, song_id, bounded=bounded, session=session) as (url, metrics):
        run(FFMPEG + ["-protocol_whitelist", "file,http,tcp", "-ss", str(seek_seconds),
            "-i", url, "-map", "0:a:0", "-vn", "-map_metadata", "-1", "-af",
            f"atrim=start_sample={skip}:end_sample={skip + frames},asetpts=PTS-STARTPTS",
            "-c:a", "flac", "-frame_size", "4096", "-sample_fmt", "s32", "-bits_per_raw_sample", "24", str(output)], cancel=cancel)
        acquisition_ms = (time.monotonic() - began) * 1000
    if any("error" in r for r in metrics["decoder_requests"]):
        raise RuntimeError("upstream acquisition had a transport error")
    return {"start_frame": start, "frames": frames, "acquisition_ms": acquisition_ms,
            "mode": "session" if session is not None else "bounded" if bounded else "passthrough",
            "body_bytes_received": sum(r["body_bytes_received"] for r in metrics["upstream_requests"]),
            **metrics}


def decode(path, rate=None):
    conversion = [] if rate is None else ["-af", f"aresample={rate}:resampler=swr:exact_rational=1:async=0:dither_method=none"]
    return run(FFMPEG + ["-i", str(path), "-map", "0:a:0"] + conversion + ["-f", "s32le", "-c:a", "pcm_s32le", "pipe:1"])


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--navidrome", required=True, type=Path)
    parser.add_argument("--stratum", required=True, type=Path)
    parser.add_argument("--fixture", required=True, type=Path)
    parser.add_argument("--out", required=True, type=Path)
    args = parser.parse_args()
    args.navidrome = args.navidrome.resolve(strict=True)
    args.stratum = args.stratum.resolve(strict=True)
    args.fixture = args.fixture.resolve(strict=True)
    # A fresh directory makes cold-read/cache measurements unambiguous.
    args.out.mkdir(parents=True, exist_ok=False)
    args.out = args.out.resolve()
    info = json.loads(run([str(args.stratum), "inspect", "--source", str(args.fixture)]))
    if info["sample_rate"] != RATE or info["channels"] != CHANNELS or info["bits_per_sample"] != 24 or info["frames"] != 160 * RATE + 317 or args.fixture.stat().st_size > MAX_BYTES:
        raise RuntimeError("use the 160-second stereo 24-bit synthetic fixture example")
    reference = decode(args.fixture)
    require(len(reference) == info["frames"] * CHANNELS * 4, "reference frame count mismatch")
    version = run([str(args.navidrome), "--version"]).decode().strip()
    report = {"navidrome_version": version, "source_bytes": args.fixture.stat().st_size,
              "source_frames": info["frames"], "cases": [],
              "scope": "Loopback source-acquisition proof, not a production adapter or live player. Body bytes are measured at the gateway, not Navidrome disk I/O."}
    with tempfile.TemporaryDirectory(prefix="stratum-nav-instance-") as state:
        with disposable(args.navidrome, args.fixture, Path(state)) as (nav, song, bad_auth):
            print("Disposable Navidrome ready; synthetic track scanned.", flush=True)
            baseline = acquire(nav, song["id"], 150 * RATE + 37 - CONTEXT,
                               2 * RATE + 1 + 2 * CONTEXT, args.out / "baseline.flac", bounded=False)
            baseline_start = baseline["start_frame"] * CHANNELS * 4
            baseline_end = baseline_start + baseline["frames"] * CHANNELS * 4
            require(decode(args.out / "baseline.flac") == reference[baseline_start:baseline_end], "baseline source PCM mismatch")
            baseline["pcm_exact"] = True
            report["unbounded_baseline"] = baseline
            for label, start, frames in [("start", 0, 2 * RATE + 1),
                                         ("late", 150 * RATE + 37 - CONTEXT, 2 * RATE + 1 + 2 * CONTEXT)]:
                path = args.out / (label + ".flac")
                measured = acquire(nav, song["id"], start, frames, path)
                pcm = decode(path)
                expected = reference[start * CHANNELS * 4:(start + frames) * CHANNELS * 4]
                measured["label"] = label
                measured["pcm_exact"] = pcm == expected
                measured["source_fraction_received"] = measured["body_bytes_received"] / report["source_bytes"]
                if not measured["pcm_exact"]:
                    raise RuntimeError(f"{label} source interval does not match the original PCM")
                report["cases"].append(measured)
                print(f"{label}: exact PCM; received {measured['body_bytes_received']:,} / {report['source_bytes']:,} source bytes", flush=True)
            try:
                acquire(bad_auth, song["id"], 0, RATE, args.out / "must-not-exist.flac")
                raise AssertionError("invalid credentials were accepted")
            except RuntimeError as error:
                if "Subsonic 40" not in str(error):
                    raise
                report["wrong_password_rejected"] = True
            # Same acquired late context, then the real Rust segment encoder.
            segment = json.loads(run([str(args.stratum), "segment", "--source", str(args.out / "late.flac"),
                "--start-frame", str(CONTEXT), "--frames", str(2 * RATE + 1), "--quality", "opus-128",
                "--cache", str(args.out / "segment-cache")]))
            metadata = segment["metadata"]
            pcm = decode(segment["path"])
            a = metadata["discard_start_frames"] * CHANNELS * 4
            b = a + metadata["presentation"]["frames"] * CHANNELS * 4
            require(len(pcm) == metadata["decoded_frames"] * CHANNELS * 4, "late Opus decoded length mismatch")
            report["late_opus_segment"] = segment
            report["late_opus_presentation_frames"] = len(pcm[a:b]) // (CHANNELS * 4)
            # Compare against the same request from the full LOCAL reference.
            # With identical source PCM/context and encoder settings, the decoded
            # lossy result should be identical too; no lag correction is allowed.
            local = json.loads(run([str(args.stratum), "segment", "--source", str(args.fixture),
                "--start-frame", str(150 * RATE + 37), "--frames", str(2 * RATE + 1),
                "--quality", "opus-128", "--cache", str(args.out / "local-reference-cache")]))
            local_pcm = decode(local["path"])
            local_a = local["metadata"]["discard_start_frames"] * CHANNELS * 4
            require(pcm[a:b] == local_pcm[local_a:local_a + b - a], "late Opus differs from the local reference encode")
            report["late_opus_matches_local_reference"] = True
            # A listening excerpt whose source also arrives through Navidrome.
            excerpt_start, excerpt_frames = 144 * RATE, 12 * RATE + 337
            excerpt = args.out / "navidrome-excerpt.flac"
            report["listening_acquisition"] = acquire(nav, song["id"], excerpt_start, excerpt_frames, excerpt)
            require(decode(excerpt) == reference[excerpt_start * CHANNELS * 4:(excerpt_start + excerpt_frames) * CHANNELS * 4], "listening excerpt source PCM mismatch")
            run([str(args.stratum), "demo", "--source", str(excerpt), "--out", str(args.out / "listening")])
            listening = json.loads((args.out / "listening/report.json").read_text())
            require(listening["exact_frame_count"] and listening["lossless_sections_exact"], "listening output lost source frames")
            require(listening["alignment_windows_at_zero_lag"] and not listening["alignment_inconclusive_windows"], "listening output alignment check failed")
            report["listening_checks"] = {k: listening[k] for k in ["exact_frame_count", "lossless_sections_exact", "alignment_windows_at_zero_lag", "alignment_evaluable_windows", "alignment_inconclusive_windows"]}
    report["disposable_server_stopped"] = True
    (args.out / "report.json").write_text(json.dumps(report, indent=2) + "\n")
    print("Navidrome stopped; report and listening files: " + str(args.out), flush=True)


if __name__ == "__main__":
    main()

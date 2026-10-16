#!/usr/bin/env python3
"""Natural track handoff proof: two Navidrome files, one continuous PCM consumer.

By default a source is split into two disposable tracks with a known continuous
join. --next-source instead tests the first file's real EOF into the second's
beginning. Only an explicit audio helper/device enables physical output. No seek
or queue reset occurs at the track boundary. This is not an Astra integration.
"""
import argparse
import base64
import contextlib
from dataclasses import dataclass
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

import live
import navidrome as nav
from source_session import SourceSession

RATE, FRAME_BYTES = live.RATE, live.FRAME_BYTES
SEGMENT, TOTAL = live.SEGMENT, live.TOTAL
BOUNDARY = 6 * SEGMENT + 1  # One-frame EOF segment; also inside a device quantum.


@dataclass(frozen=True)
class Clock:
    rate: int = RATE

    def __post_init__(self):
        nav.require(self.rate in (44100, 48000), "unsupported source clock")

    @property
    def segment(self):
        return 2 * self.rate + 1

    @property
    def total(self):
        return 16 * self.segment

    @property
    def boundary(self):
        return 6 * self.segment + 1


SCENARIOS = ("lossless", "mixed", "reverse", "adaptive", "late-next")


def intervals(lengths, origins=None, rate=RATE):
    segment = Clock(rate).segment
    origins = [0] * len(lengths) if origins is None else origins
    nav.require(len(origins) == len(lengths) and all(origin >= 0 for origin in origins), "invalid source origins")
    result, position = [], 0
    for track, length in enumerate(lengths):
        nav.require(length >= 16, "source too short")
        for start in range(0, length, segment):
            frames = min(segment, length - start)
            result.append({"track": track, "source_start": origins[track] + start,
                           "start": position, "frames": frames})
            position += frames
    return result


def playback_ranges(infos, actual_pair):
    clock = Clock(infos[0].get("sample_rate", RATE))
    nav.require(all(info.get("sample_rate", RATE) == clock.rate for info in infos),
                "mixed source rates need a separate output policy; no silent conversion")
    if not actual_pair:
        nav.require([info["frames"] for info in infos] == [clock.boundary, clock.total - clock.boundary], "invalid split fixture")
        return [{"origin": 0, "frames": info["frames"]} for info in infos]
    nav.require(infos[0]["frames"] >= clock.boundary and infos[1]["frames"] >= clock.total - clock.boundary,
                "album tracks too short for excerpt")
    return [{"origin": infos[0]["frames"] - clock.boundary, "frames": clock.boundary},
            {"origin": 0, "frames": clock.total - clock.boundary}]


def acquisition_interval(start, frames, total, rate=RATE):
    context = 120 * rate // 1000
    nav.require(0 <= start < total and 0 < frames <= total - start, "invalid source interval")
    begin, end = max(0, start - context), min(total, start + frames + context)
    if rate == 44100:
        begin -= begin % 147
    return begin, end - begin, start - begin


def validate_envelope(envelope, interval, quality, rate=RATE):
    nav.require(all(envelope[key] == value for key, value in interval.items()), "response track/interval mismatch")
    meta = envelope["metadata"]
    nav.require(meta["quality"] == quality and meta["sample_rate"] == rate and meta["channels"] == 2,
                "response format mismatch")
    nav.require(meta["presentation"]["frames"] == interval["frames"] and
                meta["discard_start_frames"] >= 0 and meta["discard_end_frames"] >= 0 and
                meta["discard_start_frames"] + interval["frames"] + meta["discard_end_frames"] == meta["decoded_frames"],
                "invalid segment trims")
    if rate != RATE:
        codec_rate = rate if quality == "lossless" else 48000
        native_frames = meta["encoded_interval"]["frames"]
        codec_frames = (native_frames * codec_rate + rate - 1) // rate
        maximum_native = (codec_frames * rate + codec_rate - 1) // codec_rate
        nav.require(meta["codec_sample_rate"] == codec_rate and meta["codec_decoded_frames"] == codec_frames
                    and native_frames <= meta["decoded_frames"] <= maximum_native
                    and meta["resample_tail_frames"] == meta["decoded_frames"] - native_frames,
                    "invalid native/codec clock mapping")
    return meta


@contextlib.contextmanager
def endpoint(account, songs, infos, binary, output, playback, plans, impaired):
    route = "/" + secrets.token_urlsafe(24) + "/segment"
    records, lock = [], threading.Lock()
    link = live.Link(playback, impaired)
    sessions = [SourceSession(account, song["id"]) for song in songs]

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
                nav.require(set(params) == {"index", "quality"} and all(len(v) == 1 for v in params.values()),
                            "invalid request")
                index, quality = int(params["index"][0]), params["quality"][0]
                nav.require(0 <= index < len(plans) and quality in live.QUALITIES, "invalid request interval")
                plan = plans[index]
                with lock:
                    nav.require(len(records) < 64, "request budget exceeded")
                    record = {**plan, "index": index, "quality": quality,
                              "began": time.monotonic(), "sent_bytes": 0}
                    records.append(record)
                    work = output / ("request-%03d" % len(records))
                    work.mkdir()
                track = plan["track"]
                begin, count, trim = acquisition_interval(plan["source_start"], plan["frames"], infos[track]["frames"], infos[track]["sample_rate"])
                source = work / "source.flac"
                record["acquisition"] = nav.acquire(account, songs[track]["id"], begin, count, source,
                                                     session=sessions[track], rate=infos[track]["sample_rate"])
                segment = json.loads(nav.run([str(binary), "segment", "--source", str(source),
                    "--start-frame", str(trim), "--frames", str(plan["frames"]), "--quality", quality,
                    "--cache", str(work / "cache")]))
                payload = Path(segment["path"]).read_bytes()
                nav.require(0 < len(payload) <= live.MAX_PAYLOAD, "oversized segment")
                record.update(prepared=time.monotonic(), payload_bytes=len(payload))
                envelope = {**plan, "metadata": segment["metadata"]}
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
    worker = threading.Thread(target=server.serve_forever, daemon=True)
    worker.start()
    try:
        yield f"http://127.0.0.1:{server.server_port}{route}", records, sessions
    finally:
        server.shutdown()
        server.server_close()
        worker.join(timeout=2)
        deadline = time.monotonic() + 35
        while any("finished" not in record for record in records) and time.monotonic() < deadline:
            time.sleep(0.02)
        nav.require(all("finished" in record for record in records), "segment workers remained")
        for session in sessions:
            session.close()


def receive(url, index, plan, quality, policy, playback, adaptive, output, rate=RATE):
    began = time.monotonic()
    request = urllib.request.Request(url + "?" + urllib.parse.urlencode({"index": index, "quality": quality}))
    with nav.HTTP.open(request, timeout=10) as response:
        size = int(response.headers["Content-Length"])
        nav.require(0 < size <= live.MAX_PAYLOAD, "invalid payload size")
        meta = validate_envelope(json.loads(base64.b64decode(response.headers["X-Stratum-Test"])), plan, quality, rate)
        body_started, body = time.monotonic(), bytearray()
        while len(body) < size:
            chunk = response.read(min(4096, size - len(body)))
            nav.require(chunk, "truncated segment")
            body.extend(chunk)
            if adaptive and playback.started is not None and policy.endangered(
                    len(body), size, time.monotonic() - body_started, playback.buffered()):
                raise live.RetryLower()
        received = time.monotonic()
    nav.require(hashlib.sha256(body).hexdigest() == meta["payload_sha256"], "corrupt segment")
    encoded = output / (f"segment-{index}." + ("flac" if quality == "lossless" else "opus"))
    encoded.write_bytes(body)
    pcm = nav.decode(encoded, rate=rate if rate != 48000 and quality != "lossless" else None)
    nav.require(len(pcm) == meta["decoded_frames"] * FRAME_BYTES, "decoded duration mismatch")
    trim = meta["discard_start_frames"] * FRAME_BYTES
    pcm = pcm[trim:trim + plan["frames"] * FRAME_BYTES]
    decoded = time.monotonic()
    pcm_path = output / f"segment-{index}.s32le"
    pcm_path.write_bytes(pcm)
    # Tiny EOF segments have disproportionate overhead/headers. They should not
    # convince the controller that full-size segments have become cheap.
    if plan["frames"] == Clock(rate).segment:
        policy.observe(size, received - body_started, body_started - began + decoded - received)
    playback.append(plan["start"], pcm)
    return {**plan, "quality": quality, "pcm": pcm_path.name,
            "began": began, "ready": decoded, "payload_bytes": size,
            "buffer_after_seconds": playback.buffered(), "metadata": meta}


def wait_until(playback, predicate, timeout=25):
    deadline = time.monotonic() + timeout
    while not predicate():
        if playback.error:
            raise playback.error
        nav.require(time.monotonic() < deadline and not playback.done.wait(0.01), "consumer stopped/timed out")


def run_case(name, account, songs, infos, sources, args, output):
    output.mkdir()
    clock = Clock(infos[0]["sample_rate"])
    rate, segment_frames, total, boundary = clock.rate, clock.segment, clock.total, clock.boundary
    ranges = playback_ranges(infos, bool(args.next_source))
    plans = intervals([part["frames"] for part in ranges], [part["origin"] for part in ranges], rate)
    if args.audio_helper:
        from device_output import DevicePlayback
        playback = DevicePlayback(output, args.audio_helper, args.audio_device, total, args.audio_gain, rate=rate)
    else:
        playback = live.Playback(output, total=total, start_buffer=3 * segment_frames, rate=rate)
    policy = live.Policy(infos[0]["bytes"] / (infos[0]["frames"] / rate) * segment_frames / rate, segment_seconds=segment_frames / rate)
    player = threading.Thread(target=playback.play, daemon=True)
    segments, retries = [], []
    began = time.monotonic()
    player.start()
    try:
        with endpoint(account, songs, infos, args.stratum, output, playback, plans,
                      name in ("mixed", "adaptive", "late-next")) as (url, records, sessions):
            for index, plan in enumerate(plans):
                wait_until(playback, lambda: playback.buffered() < 4 * segment_frames / rate)
                if plan["track"] == 1 and plan["source_start"] == 0:
                    if name in ("mixed", "adaptive"):
                        # Ensure preparation really begins on the impaired link;
                        # never prebuild the next track before playback starts.
                        wait_until(playback, lambda: playback.started is not None and
                                   time.monotonic() - playback.started >= 5.25)
                    elif name == "late-next":
                        # Negative control: deliberately miss the handoff deadline.
                        wait_until(playback, lambda: playback.started is not None and
                                   time.monotonic() - playback.started >= boundary / rate + 0.5)
                if name == "adaptive":
                    policy.lossless_bytes = infos[plan["track"]]["bytes"] / (infos[plan["track"]]["frames"] / rate) * segment_frames / rate
                    quality = policy.choose(playback.buffered()) if playback.started is not None else "lossless"
                elif name == "reverse":
                    quality = "opus-64" if plan["track"] == 0 else "lossless"
                else:
                    quality = "opus-64" if name in ("mixed", "late-next") and plan["track"] == 1 else "lossless"
                try:
                    segment = receive(url, index, plan, quality, policy, playback, name == "adaptive", output, rate)
                except live.RetryLower:
                    retries.append({**plan, "from": quality, "at": time.monotonic()})
                    segment = receive(url, index, plan, "opus-64", policy, playback, True, output, rate)
                segments.append(segment)
                if len(segments) == 1 or plan["source_start"] == 0 or segments[-2]["quality"] != segment["quality"]:
                    print(f"{name}: track {plan['track'] + 1}, timeline {plan['start'] / rate:.3f}s → {segment['quality']}", flush=True)
            nav.require(playback.done.wait(65), "consumer failed to finish")
            if playback.error:
                raise playback.error
    finally:
        playback.cancel.set()
        playback.ready.set()
        if args.audio_helper:
            playback.stop()
        player.join(timeout=5)
        nav.require(not player.is_alive(), "consumer remained")
    nav.require(not any("error" in record for record in records), "segment request failed")
    first_next = next(segment for segment in segments if segment["track"] == 1)
    boundary_time = playback.started + boundary / rate
    delivery = {"tracks": [{"source": str(source), **part}
                            for source, part in zip(sources, ranges)],
                "sample_rate": rate, "frames": total, "segments": segments, "underruns": playback.underruns}
    (output / "delivery.json").write_text(json.dumps(delivery, indent=2) + "\n")
    checks = json.loads(nav.run([str(args.checker), str(output / "delivery.json")]))
    nav.require(all(checks[key] for key in ("exact_frames", "lossless_exact", "rendered_matches", "zero_lag"))
                and checks["inconclusive"] == 0, "handoff audio verification failed")
    if name == "late-next":
        nav.require(playback.silence_frames > 0 and first_next["ready"] > boundary_time,
                    "negative control did not miss its deadline")
    else:
        nav.require(playback.silence_frames == 0 and first_next["ready"] < boundary_time,
                    "natural handoff starved")
    if name in ("mixed", "adaptive"):
        nav.require(5 <= first_next["began"] - playback.started < 17, "next track not prepared during slow link")
    if args.audio_helper:
        nav.require(playback.device_report["timeline_discontinuities"] == 0, "device timeline changed")
        live.validate_device_timing(playback.device_report, (total + playback.silence_frames) / rate)
    else:
        nav.require(abs(playback.elapsed - (total + playback.silence_frames) / rate) < 0.1,
                    "file consumer clock mismatch")
    for track, session in enumerate(sessions):
        metrics = session.summary()
        requests = [record for record in records if record["track"] == track]
        nav.require(metrics["validation_requests"] == len(requests), "cached request skipped validation")
        nav.require(metrics["upstream_body_bytes"] == sum(record["acquisition"]["body_bytes_received"] for record in requests),
                    "source traffic counted twice")
        nav.require(metrics["peak_cached_bytes"] <= metrics["cache_limit_bytes"], "source cache limit exceeded")
    result = {"sample_rate": rate, "scenario": name, "boundary_frame": boundary, "frames": total,
              "startup_seconds": playback.started - began, "elapsed_seconds": playback.elapsed,
              "next_track_request_at_seconds": first_next["began"] - playback.started,
              "next_track_ready_margin_seconds": boundary_time - first_next["ready"],
              "silence_frames": playback.silence_frames, "underruns": playback.underruns,
              "checks": checks, "segments": segments, "retries": retries, "server_requests": records,
              "source_sessions": [session.summary() for session in sessions],
              "device": getattr(playback, "device_report", None)}
    (output / "report.json").write_text(json.dumps(result, indent=2) + "\n")
    # Preserve actual starvation in the optional listening file too.
    nav.run(nav.FFMPEG + ["-f", "s32le", "-ar", str(rate), "-ac", "2", "-i", str(output / "rendered.s32le"),
                         "-c:a", "pcm_s24le", str(output / "listening.wav")])
    print(f"{name}: checks passed; {playback.silence_frames / rate:.3f}s starvation; "
          f"next track ready {result['next_track_ready_margin_seconds']:.3f}s before boundary", flush=True)
    return result


def inspect_source(source, binary):
    info = json.loads(nav.run([str(binary), "inspect", "--source", str(source)]))
    nav.require(info["sample_rate"] in (44100, 48000) and info["channels"] == 2 and info["bits_per_sample"] in (16, 24),
                "native 44.1/48 kHz stereo lossless input required")
    nav.require(info["bytes"] <= 96 * 1024 * 1024 and info["frames"] <= 600 * info["sample_rate"], "source outside proof bounds")
    return info


def prepare_sources(source, origin, binary, output):
    info = inspect_source(source, binary)
    clock = Clock(info["sample_rate"])
    if origin is None:
        origin = 80 * clock.rate + 37
    nav.require(origin >= 0 and origin + clock.total <= info["frames"], "excerpt outside source")
    sources, infos = [], []
    for index, (offset, frames) in enumerate(((0, clock.boundary), (clock.boundary, clock.total - clock.boundary))):
        target = output / f"track-{index + 1}.flac"
        nav.run(nav.FFMPEG + ["-i", str(source), "-map", "0:a:0", "-vn", "-map_metadata", "-1", "-af",
            f"atrim=start_sample={origin + offset}:end_sample={origin + offset + frames},asetpts=PTS-STARTPTS",
            "-c:a", "flac", "-sample_fmt", "s32", "-bits_per_raw_sample", str(info["bits_per_sample"]),
            "-metadata", f"title=Stratum handoff {index + 1}", str(target)])
        track = json.loads(nav.run([str(binary), "inspect", "--source", str(target)]))
        nav.require(track["frames"] == frames, "fixture split changed duration")
        sources.append(target)
        infos.append(track)
    reference = nav.decode(source)[origin * FRAME_BYTES:(origin + clock.total) * FRAME_BYTES]
    nav.require(b"".join(nav.decode(path) for path in sources) == reference, "split changed source PCM")
    (output / "reference.s32le").write_bytes(reference)
    return sources, infos


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    for name in ("navidrome", "stratum", "checker", "source", "out"):
        parser.add_argument("--" + name, type=Path, required=True)
    parser.add_argument("--next-source", type=Path, help="use two original tracks: actual EOF into frame zero")
    parser.add_argument("--origin", type=int, help="excerpt origin for split-source mode only")
    parser.add_argument("--scenario", choices=(*SCENARIOS, "all"), default="all")
    parser.add_argument("--audio-helper", type=Path)
    parser.add_argument("--audio-device")
    parser.add_argument("--audio-gain", type=float, default=0.25)
    args = parser.parse_args()
    nav.require(bool(args.audio_helper) == bool(args.audio_device) and 0 <= args.audio_gain <= 1, "invalid audio options")
    nav.require(not args.audio_helper or args.scenario != "all", "choose one scenario for native output")
    nav.require(not args.next_source or args.origin is None, "--origin does not apply to actual track endings")
    for name in ("navidrome", "stratum", "checker", "source", "next_source", "audio_helper"):
        if getattr(args, name):
            setattr(args, name, getattr(args, name).resolve(strict=True))
    args.out.mkdir(parents=True, exist_ok=False)
    args.out = args.out.resolve()
    if args.next_source:
        sources = [args.source, args.next_source]
        infos = [inspect_source(source, args.stratum) for source in sources]
        nav.require(infos[0]["sha256"] != infos[1]["sha256"], "actual handoff requires two different sources")
        playback_ranges(infos, True)
    else:
        sources, infos = prepare_sources(args.source, args.origin,
                                        args.stratum, args.out)
    results = []
    for name in SCENARIOS if args.scenario == "all" else (args.scenario,):
        with tempfile.TemporaryDirectory(prefix="stratum-handoff-nav-") as temporary:
            with nav.disposable_library(args.navidrome, sources, Path(temporary)) as (account, songs, _):
                results.append(run_case(name, account, songs, infos, sources, args, args.out / name))
    (args.out / "report.json").write_text(json.dumps({"experiment": "stratum-handoff-poc-1", "cases": results,
        "source_mode": "actual-pair" if args.next_source else "split-source", "source_info": infos,
        "server_stopped": True, "scope": "Disposable Navidrome; real EOF into frame zero; one output timeline; no Astra integration."}, indent=2) + "\n")
    print(f"Handoff proof passed. Report: {args.out / 'report.json'}", flush=True)


if __name__ == "__main__":
    main()

# Reusing Navidrome source reads

The first [live-delivery proof](live-delivery-proof.md) stayed continuous through a
client bandwidth drop, but its adaptive run read 39.18 MB from Navidrome for a
32-second excerpt of a 35.44 MB file. Each segment started a new source byte cache,
so FFmpeg's probing, seeking and overlapping codec context repeatedly fetched the
same original bytes. A quality retry fetched them again.

This experiment keeps the same audio engine, playback policy and traffic schedule,
and adds one private source cache per account, track and playback session. It is
still Python experiment infrastructure feeding the Rust engine, not a production
Sidecar cache or a finalized authentication design.

## Behavior

`SourceSession` owns the existing 256 KiB range reader and 8 MiB LRU. Decoders and
encoders still run for each interval; cached original byte blocks survive between
them. Different quality requests can reuse the same source bytes. No full-track
download, cross-user sharing, persistent disk cache or new representation is added.

Before every acquisition, including an entirely cached interval:

1. Check that the session belongs to this exact account handle and track.
2. Ask Navidrome for the track again with that account; reject lost access or a
   changed size/stream identity.
3. Read one actual stream byte over authenticated HTTP, bypassing the byte cache.
   Require HTTP 206, the exact range/size, identity encoding and the pinned source
   validator. Send `If-Range` after the first check.
4. Only then allow the decoder to read cached or newly fetched blocks.

A strong ETag is preferred; otherwise the reader uses Last-Modified. A changed
validator, invalid range, truncated response, rejected account or failed connection
closes the session and clears its bytes. It never silently serves the warm cache
after validation fails. A fresh session is required afterward. Leases serialize
acquisitions and include decoder/gateway shutdown, so another request cannot reuse
the old authorization window while the preceding decoder is still reading.

The cache limit stays 8 MiB. Eviction only removes source blocks; a later authorized
request can fetch them again. Closing playback releases the cached bytes. Request
metrics are deltas, so the aggregate report does not count reused blocks twice.

## Reproduce the comparison

Use the same fixture, dependencies and build commands as the live proof. Run these
sequentially to avoid competing benchmark traffic/CPU. Each takes about two minutes:

```sh
python3 -B experiments/live.py \
  --navidrome /path/to/navidrome \
  --stratum target/release/stratum-poc \
  --checker target/release/examples/live_check \
  --fixture artifacts/navidrome-source.flac \
  --out artifacts/source-session/shared --source-cache session

python3 -B experiments/live.py \
  --navidrome /path/to/navidrome \
  --stratum target/release/stratum-poc \
  --checker target/release/examples/live_check \
  --fixture artifacts/navidrome-source.flac \
  --out artifacts/source-session/baseline --source-cache per-segment
```

Both run steady, adaptive drop/recovery, and forced-lossless drop/recovery cases.
The impairment remains 10 Mbit/s → 192 kbit/s at playback second 5 → 10 Mbit/s at
second 17, with a 60 ms response delay. Source acquisition stays on local loopback.
All source caches start empty. Navidrome/OS file caches may be warm; these are not
cold-disk or WAN benchmarks. `report.json` includes a `source_cache` summary in
each case, plus the existing audio/timing checks and listening files.

## Recorded comparison — 2026-10-16

Same 32.0003-second excerpt, Navidrome 0.64.2, FFmpeg 9.0.1, macOS arm64. These
are fresh sequential runs of both modes, not a comparison against old timings:

| Case | Per-segment upstream bytes | Session upstream bytes | Reduction | Starvation, both modes |
| --- | ---: | ---: | ---: | ---: |
| Steady | 37,293,248 | 11,325,660 | 69.6% | 0 |
| Adaptive drop/recovery | 39,181,708 | 11,325,661 | 71.1% | 0 |
| Forced lossless drop/recovery | 37,031,104 | 11,325,660 | 69.4% | 3.46 s |

Session-cache peaks were **8,179,916 bytes**, below the 8,388,608-byte limit. Its
44 data-block reads covered 43 distinct ranges; one block was fetched again after
eviction. The adaptive run's extra validation byte comes from its cancelled
lossless request and retry. All 17 acquisitions reauthorized and validated; both
non-adaptive cases did so for all 16 acquisitions.

Automatic changed lossless → Opus64 → Opus128 → lossless at the same approximate
media times (12/24/30 seconds). Every case in both modes passed all 96 alignment
windows, exact frame accounting, exact lossless PCM and rendered-output checks.
Startup remained about 3.9 seconds; this change targets traffic reuse, not startup.
All temporary servers stopped, and closed sessions cleared their byte caches.

The real-server guard probe also passed: exact cold/warm PCM, one-byte warm
validation, rejection after credentials became invalid, and rejection after a
scan published a changed Last-Modified. Before that scan the header stayed the
same; this limitation is recorded, not treated as a passing mutation detector.

Validation: 26 Python regression tests and 9 Rust tests, formatting and Clippy.
Artifacts are ignored under `artifacts/source-session/` (`shared`, `baseline`,
`guards`). Code and documentation remain for the user to commit.

## Cached-access and revision checks

This separate short probe runs another real disposable Navidrome:

```sh
python3 -B experiments/session_probe.py \
  --navidrome /path/to/navidrome \
  --fixture artifacts/navidrome-source.flac \
  --out artifacts/source-session/guards
```

It fetches the same interval twice and compares both against a separate source
decode. The second acquisition must receive exactly one upstream body byte: the
fresh stream validation. It then makes the session's credentials invalid and
checks that cached audio is rejected before any output is created. A new session
is warmed and the disposable fixture copy's modification time is advanced by two
seconds. The probe records the validator before scanning, triggers a scan, then
requires reuse to fail with the cache cleared once Navidrome reports a new revision.
The original fixture and real
servers/accounts are untouched. This tests a rejected sign-in and changed server
validator; it does not exercise Navidrome's library ACL administration.

Network-free regression tests additionally cover metadata succeeding while the
stream denies access, same-size replacement, size changes, another account/track,
unreachable upstream, ignored ranges, explicit close, LRU eviction and concurrent
leases. They keep the existing bounded-range and playback policy tests.

## Limits and next work

**A real-server finding:** Navidrome 0.64.2 derives the original stream's
Last-Modified from the database's `MediaFile.UpdatedAt`, not a fresh file stat.
Our first mutation probe failed because changing the fixture's mtime left the
reported revision unchanged before a scan. The revised probe retains this
observation and separately verifies rejection after the reported revision changes.
See Navidrome's [ModTime and Serve implementation](https://github.com/navidrome/navidrome/blob/v0.64.2/core/stream/media_streamer.go#L148).

Therefore a changed underlying file can remain invisible until Navidrome scans it;
reusing bytes does **not** guarantee a consistent source if the file is edited in
that interval. Last-Modified also has second-level precision. Same-size edits
within that precision, incorrect/stale validators and malicious servers are not detected reliably.
This pins the revision Navidrome reports; it does not invent a cryptographic track
revision. Stronger identity semantics remain part of production source design.

Access is checked for each acquisition, not pushed continuously during already
authorized delivery. Revocation after a check and client-retained/offline audio are
separate concerns. The session cannot be reused after a credential/connection
identity change. There is no multi-user cache or public session API here.

The loopback endpoints still use temporary random routes for test isolation.
Trusted discovery, pairing, real client authentication, scoped-user integration,
slow Navidrome connections, physical audio devices and Astra integration remain
unfinished. No new public protocol or user-facing policy follows from this proof.

The cache saves repeated source traffic; it does not eliminate probing, decoder
startup, FLAC framing overhead or all read-ahead. CPU, actual disk I/O and total
wire traffic are not measured by the application body-byte counters.

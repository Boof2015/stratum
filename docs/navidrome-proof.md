# Disposable Navidrome source-acquisition proof

The first local proof established interval alignment. This experiment asks whether
Navidrome can supply those intervals without requiring a complete source download,
and whether the resulting audio still matches the local reference exactly.

It runs a **real Navidrome process**, not a fake API. Python orchestrates the
temporary server and measures upstream HTTP reads; the existing Rust engine still
encodes the final Stratum segments. No production backend interface is finalized.

## Reproduce

Requires Python 3.9+, Rust, FFmpeg with libopus, and a Navidrome executable. Tested
on macOS arm64 with [Navidrome 0.64.2](https://github.com/navidrome/navidrome/releases/tag/v0.64.2)
(`10114574`), FFmpeg 9.0.1 and Rust 1.98.1.

The official `navidrome_0.64.2_darwin_arm64.tar.gz` used for the first run was
verified against its release SHA-256 before execution:

```text
8b0a7798001453719ad50c12f7025a6044546c4a3568efe51feb296681f29956
```

Download the appropriate official archive for your platform separately; the script
never installs or downloads software. The orchestration currently requires POSIX
process groups (macOS/Linux), not Windows.

```sh
cargo build --release --bins --examples
mkdir -p artifacts
cargo run --release --example fixture -- artifacts/navidrome-source.flac
python3 -B experiments/navidrome.py \
  --navidrome /path/to/navidrome \
  --stratum target/release/stratum-poc \
  --fixture artifacts/navidrome-source.flac \
  --out artifacts/navidrome-run
```

Generate the fixture once. It contains 7,680,317 stereo frames of 24-bit/48 kHz
audio (160 seconds plus 317 frames). Reuse it for later runs with a new `--out`
directory. The fixture is deliberately too large for a two-second request to hide
an accidental full download.

The test creates a fresh music folder/database under the system temporary
directory, copies only this fixture, and creates an ephemeral account. Existing
`ND_*` environment settings are removed from the server's environment. External
services and insights collection are disabled. Nothing is added to a real library.

Both HTTP listeners use `127.0.0.1` and temporary ports. Subprocess requests have
timeouts, and Navidrome has an independent 180-second lifetime watchdog. Normal and
exception exits stop its process group and remove its temporary instance data.
There is no browser, Electron process, OS service installation or automatic playback.

## Acquisition path

```text
FFmpeg → loopback measurement gateway → Navidrome /rest/stream
                                      format=raw, maxBitRate=0
         ↓
sample-exact lossless interval → Rust segment engine → Opus / listening proof
```

The gateway checks `getSong`, then requests the original stream using the same
temporary user's Subsonic token/salt. Those credentials stay in the Python process;
FFmpeg sees only a temporary loopback URL. Redirects and environment HTTP proxies
are disabled in the upstream client. No authentication material is written into
the report.

The experiment compares two acquisition strategies:

- **Pass-through baseline:** forwards FFmpeg's byte-range requests as received.
  Open-ended requests allow the fast server/socket pipeline to send audio the
  decoder will never consume, including overlapping reads while it seeks.
- **Bounded reads:** translates those requests into at most 256 KiB upstream ranges
  and reuses recently fetched original byte blocks. An 8 MiB LRU bounds this temporary
  per-user/per-track cache. A small downstream send buffer limits how far acquisition
  can run ahead of the decoder. Bytes are not transcoded or altered in the gateway.

Bounded mode requires HTTP 206 with the exact requested `Content-Range`, no content
encoding, the expected byte count and a consistent strong ETag or Last-Modified
validator. Ignored ranges, truncated data and changed validators fail. It does not
silently download the whole file as a fallback. Last-Modified has limited time
resolution; this is not yet a general source-version guarantee for changing files.

The byte cache exists only for one acquisition session. It is not a persistent
server cache, a multi-user cache, or a proof of authorization after revocation.

## First results, 2026-10-16

The source file was **35,442,892 bytes**. Each bounded case started with an empty
gateway cache. Navidrome and the OS may already have had the file in memory;
these are cold *adapter-cache* measurements, not cold disk benchmarks.

| Request | Body bytes received from Navidrome | Local acquisition time |
| --- | ---: | ---: |
| Late interval, pass-through baseline | 14,713,251 | 53.88 ms |
| First 96,001 frames, bounded | 786,432 | 34.51 ms |
| Late interval including codec context, bounded | 1,888,460 | 43.81 ms |
| 12-second listening excerpt near the end, bounded | 3,985,612 | 51.78 ms |

The late presentation starts **37 frames after 2:30**, lasts 96,001 frames, and
acquires 120 ms of context on each side. Its bounded request received about **5.3%**
of the complete source, versus about 41.5% in the pass-through baseline. Read-ahead
and cancellation timing vary; earlier pass-through runs received 15–16 MB.

Verified in the real-server run:

- Both acquisition strategies returned source PCM exactly matching the corresponding
  interval of a separate local decode, including the odd frame boundary.
- The Opus segment made from the acquired late interval decoded identically to the
  equivalent Opus segment made directly from the local full source.
- The listening excerpt, obtained through Navidrome, kept the exact output frame
  count and lossless PCM. All 38 alignment windows were evaluable with zero lag.
- A wrong password was rejected before audio acquisition.
- The disposable Navidrome was stopped after the test.

`report.json` retains requested ranges, actual response ranges, byte counts and
the checks. `listening/reference.wav` and `listening/switched.wav` provide a listening
comparison; switches occur near 6, 8 and 10 seconds of the excerpt. They have no
crossfade or inserted silence.

Seven network-free Python regression tests also cover block reuse/eviction,
partial final blocks, ignored/wrong ranges, truncated/oversized data, changing
validators, weak ETags and rejection before opening the stream.

```sh
python3 -B -m unittest discover -s experiments -p 'test_*.py'
```

## Limits and next work

The reported byte count is the application body read by the gateway, including
its read-ahead. It does not measure Navidrome disk reads, HTTP headers, TCP overhead
or retransmissions. The timing includes acquisition and lossless interval encoding,
but excludes gateway teardown, reference comparisons and the later Opus encode.

The full local reference decode is a test oracle; it is not part of remote
acquisition. The experiment stages a fetched lossless interval before passing it to
the existing Rust file-based engine. It does **not** yet connect a progressive
Navidrome source directly to a live Stratum player. The listening proof similarly
acquires its excerpt first and assembles the output offline.

The measured result is for a seekable FLAC served by this Navidrome release on
loopback. Test more formats, source rates, realistic RTT/bandwidth, interrupted
requests and servers/proxies that do not honor ranges before choosing production
fallbacks. Navidrome `timeOffset` transcoding was not used or evaluated here.

Discovery, pairing, persistent cache authorization, scoped-user/revocation tests,
live buffer deadlines and the Rust backend boundary remain future work.

References: [OpenSubsonic stream parameters](https://opensubsonic.netlify.app/docs/endpoints/stream/),
[Navidrome 0.64.2 stream handler](https://github.com/navidrome/navidrome/blob/v0.64.2/server/subsonic/stream.go).

# Real-time delivery proof

This is the third experiment, following local sample alignment and bounded
Navidrome acquisition. It asks whether a client can start consuming audio before
later intervals have been acquired, then change representation when the connection
slows without missing its audio deadlines.

The server is a disposable Navidrome 0.64.2 with the existing synthetic 24-bit,
48 kHz stereo fixture. Python orchestrates a loopback endpoint and client; the
existing Rust engine encodes each requested interval. This is an experiment, not
an implementation commitment for Sidecar's server stack or public wire format.

## Reproduce

Requirements and fixture generation are in [the acquisition proof](navidrome-proof.md).
Reuse that fixture and use a fresh output directory:

```sh
cargo build --release --bins --examples
python3 -B experiments/live.py \
  --navidrome /path/to/navidrome \
  --stratum target/release/stratum-poc \
  --checker target/release/examples/live_check \
  --fixture artifacts/navidrome-source.flac \
  --out artifacts/live-run
```

By default it runs three cases, taking about two minutes in total. Use
`--scenario steady`, `--scenario adaptive` or `--scenario forced-lossless` to run
one. All listeners bind loopback. Each case gets a separate temporary server,
synthetic library and account, with the acquisition harness's 180-second server
watchdog. Requests, subprocesses, playback and worker shutdown are bounded. No
browser, Electron process, audio device or existing server is opened.

The current default is `--source-cache session`, reusing source byte blocks within
the case. Use `--source-cache per-segment` for the original implementation measured
below. The bandwidth schedule, playback policy and audio checks are unchanged.
See [source-session results](source-session-proof.md) for the comparison.

## What actually runs live

```text
Navidrome original HTTP ranges → interval acquisition → Rust FLAC/Opus encoder
                                                            ↓
client queue ← decode and trim ← throttled HTTP segment response
      ↓
48 kHz consumption clock → recorded PCM (including any starvation silence)
```

The test plays sixteen 96,001-frame intervals, just over 32 seconds total, starting
37 frames after 1:20 in the original fixture. Boundaries deliberately miss Opus
packet boundaries. Every segment request acquires its source interval from
Navidrome and encodes on demand. No later segment is prepared in advance of its
request. Acquisition still stages a small lossless interval before invoking the
file-based engine; it does not download the whole track first.

The client starts after buffering three segments (~6 seconds) and aims for four
(~8 seconds), with at most one further segment in flight. A separate consumer
wakes every 20 ms. Data has to have been ready by the *scheduled* deadline, even if
that thread wakes late. When data is missing, the clock continues and the sink
records silence; the source position waits until it can resume. Missed deadlines
are never repaired by stitching an apparently gapless file afterward.

This is real wall-clock consumption into a file, **not an audio-device callback**.
Wake lateness is recorded separately. After the run, a separate Rust checker
decodes the original fixture as an oracle. It checks interval order, frame counts,
exact lossless PCM, zero-lag alignment windows, and that the actual rendered output
equals the delivered audio plus exactly the recorded starvation silence. The
client never uses the oracle to play or choose quality.

## The network test and policy

The throttled hop is **Sidecar to client**. Navidrome to Sidecar stays on fast
loopback, modelling a Sidecar installed near Navidrome. The body-rate shaper uses
real HTTP sockets, a small send buffer, aggregate pacing and no saved burst credit:

- 10 Mbit/s before playback and for the first 5 seconds.
- 192 kbit/s from playback time 5 to 17 seconds.
- 10 Mbit/s thereafter.
- A fixed 60 ms response delay, separate from acquisition and encoding.

Changes affect a request already in flight. These are application body rates and
a response delay, not a simulation of TCP congestion, packet loss or full RTT.
The steady control stays at 10 Mbit/s throughout. Forced lossless sees exactly the
same rate schedule as adaptive but cannot downshift or cancel for quality.

The client cannot inspect the link schedule. It uses observed body throughput,
preparation/decode overhead and buffered audio. If the remaining transfer threatens
the deadline, it discards the partial payload and retries the **same interval** as
64 kbit/s Opus. It never queues both versions, advances past a failed interval,
changes quality inside a decoded segment, or conceals a join with a crossfade.

Completed requests update a throughput estimate. Upward changes require three
successful observations and at least five seconds of buffer, with transfer and
preparation headroom. This is a small experimental policy, not Astra's production
automatic-quality policy. Its constants and startup buffer are not product decisions.

## Reading the output

- `report.json`: all cases, traffic schedule, cancellations, buffer measurements,
  audio checks, source acquisition records and server cleanup status.
- `<case>/delivery.json`: raw delivery measurements; timestamps are relative to
  playback start, so negative values mean startup buffering.
- `<case>/listening.wav`: what the timed consumer produced. Adaptive and steady
  should be continuous; forced lossless deliberately includes its actual stall.
- `<case>/rendered.s32le`: the same output before WAV conversion.
- Per-request source intervals/encoded payloads and per-segment PCM remain under
  the output directory for inspection. Use the ignored `artifacts/` directory.

The all-case command fails if adaptive/steady stall, forced lossless does not
stall, adaptive never exercises cancellation/downshift/recovery, everything was
acquired before playback, audio alignment fails, or rendered output/timing disagree.

## Recorded result — 2026-10-16

Navidrome 0.64.2, FFmpeg 9.0.1, macOS arm64. Each run consumed 1,536,016
source frames (32.0003 seconds), with 13 of its 16 segments received after playback
began. The source acquisition itself also continued after playback started.

| Case | Startup | Starvation silence | Result |
| --- | ---: | ---: | --- |
| Steady, automatic | 3.96 s | 0 | Stayed lossless |
| Drop/recovery, automatic | 3.93 s | 0 | Lossless → 64 → 128 → lossless |
| Same drop/recovery, forced lossless | 3.94 s | 3.48 s | Two recorded underruns |

Automatic cancelled the in-flight lossless interval with about 3.04 seconds still
buffered. The audible representation boundaries in `adaptive/listening.wav` are
near 12, 24 and 30 seconds. The controller requested those changes earlier, while
the existing buffer was still playing. Every case passed all 96 alignment windows,
exact frame accounting, exact lossless PCM and rendered-output verification.
Maximum consumer wake lateness was about 8–13 ms and was included in the report;
the readiness check uses scheduled deadlines, not those late wake times.

The adaptive case's successfully received payloads totalled 3.76 MB, plus an
abandoned lossless response (the server wrote another 0.35 MB). Forced lossless
received 7.72 MB. These are audio body bytes, not total wire traffic.

**Upstream efficiency is still weak:** the adaptive case read 39.18 MB from
Navidrome for the 32-second excerpt, versus a 35.44 MB full source file. Forced
lossless read 37.03 MB. Independent per-segment caches repeat FLAC probing and
overlapping range reads. The test proves deadline feasibility with a fast local
Navidrome hop, not efficient slow-source delivery. Reusing an authorized,
revision-consistent source session/cache is a concrete next server-side experiment.

Nine Rust tests and fifteen network-free Python tests pass, as do formatting and
Clippy with warnings denied. All disposable servers stopped. Generated artifacts
are ignored; the test code and this method are intended to be committed by the user.

## Limits

This does not test a physical DAC, Astra's native audio queue, real music, other
source rates/formats, album boundaries, concurrent users or arbitrary network
outages. Below the minimum representation's sustainable rate, buffering remains
necessary; there is no claim that adaptation prevents every stall.

The original per-segment mode gives each acquisition its own small byte cache;
the session mode now reuses those blocks. Both still start fresh decoder/encoder
processes for intervals. Source validators have limited precision, especially
Last-Modified; this is not a complete guarantee against all in-place file changes.
Navidrome 0.64.2 also derives that header from scanned database metadata, so a
file change can remain invisible until its next scan; the source-session document
records the real-server probe that exposed this limitation.

Every request rechecks the disposable account through Navidrome, but the loopback
endpoint's random route is only test isolation. It does not implement production
client authentication, shared multi-user caches, discovery or trusted pairing.
Cached-access rejection and observed revision changes have separate tests in the
source-session experiment. Lossless FLAC preserves decoded source PCM, not original file bytes.

Before production, exercise actual audio output, diverse source formats/rates,
slow source acquisition, jitter/disconnects, seeking/skipping, source revisions,
resource limits and authorization. Keep discovery/auth design separate from this
audio feasibility result.

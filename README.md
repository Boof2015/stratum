# Stratum

An experimental audio transport for requesting precise intervals at different
qualities on a shared playback timeline. The intended first server implementation,
Astra Sidecar, will work alongside Navidrome; Astra will be a client.

This repository contains **audio and delivery proofs of concept**, not a deployed
server or an Astra integration. They generate independently decodable FLAC/Opus
segments, trim them onto one sample timeline, acquire source intervals from a
disposable Navidrome, and test delivery against a real-time consumption clock.

## Run the proof

Requires Rust 1.88 or newer and `ffmpeg` / `ffprobe` on PATH. FFmpeg must include
the `libopus` encoder. The initial run was checked on macOS with Rust 1.98.1 and
FFmpeg 9.0.1; other platforms/builds have not yet been exercised.

```sh
cargo run --release -- demo --out artifacts/demo
```

This creates a deterministic 48 kHz stereo FLAC fixture and requests alternating
lossless / Opus 64 kbps / Opus 128 kbps / lossless intervals. Segment boundaries
deliberately fall between Opus packet boundaries. Nothing plays automatically.

- `artifacts/demo/reference.wav`: the source PCM for comparison.
- `artifacts/demo/switched.wav`: the joined output, with no crossfade or inserted silence.
- `artifacts/demo/report.json`: intervals, encoded sizes, cold/warm preparation times,
  decoder pre-skip, alignment estimates, and waveform steps at joins.
- `artifacts/demo/cache/`: encoded intervals and their metadata.

The generated example enters Opus near 6 seconds, changes bitrate near 8 seconds,
and returns to lossless near 10 seconds. The exact boundaries are in the report.
Listen to both files; frame accounting cannot establish that every lossy transition
is inaudible.

Try your own **44.1/48 kHz, mono/stereo, 16/24-bit FLAC or PCM WAV**, from 6 seconds to
3 minutes long:

```sh
cargo run --release -- demo --source /path/to/music.flac --out artifacts/music
```

Other rates, layouts and lossy source formats are explicitly rejected. Lossless keeps the source
sample rate and PCM. For 44.1 kHz sources, Opus explicitly converts to 48 kHz
and back before native-frame trimming; see [the native-rate proof](docs/native-rate-proof.md).
There is no claim that re-encoded lossless delivery preserves the original bytes.
Re-running a demo overwrites its generated listening files and report; use a
dedicated output directory. Encoded cache entries can be reused.

## Request one interval

```sh
cargo run --release -- inspect --source /path/to/music.flac
cargo run --release -- segment --source /path/to/music.flac \
  --start-frame 7200037 --frames 96001 --quality opus-128 --cache artifacts/cache
```

The second command requests just over two seconds starting 37 frames after 2:30.
The source must be long enough. JSON on stdout describes the encoded file, its
presentation interval and the trimming required after decoding. Qualities are
`lossless`, `opus-64` and `opus-128`. One frame is one sample per channel.

The CLI is an experimental adapter to the engine. There is no HTTP listener,
discovery endpoint or finalized wire protocol. See [the interval contract](docs/audio-proof.md).

## Disposable Navidrome proof

The next experiment runs a real Navidrome against a temporary synthetic library.
It measures late seeking over authenticated original-audio HTTP requests, compares
every fetched PCM sample with the local reference, then feeds the acquired audio
through the Rust segment engine. See [setup, results and limits](docs/navidrome-proof.md).

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

Use a fresh output directory for each run; the fixture can be reused. Python 3.9+
and a Navidrome executable are required. The script does not download executables,
connect to an existing server, or read Astra's sign-in. Both temporary HTTP
listeners bind to loopback, and Navidrome is stopped after the test.

## What this establishes

- Integer frame addressing and independently decodable representations.
- Codec pre-skip handled once, followed by explicit context trimming.
- Exact lossless reconstruction of the requested source PCM.
- Independent reference comparisons for representation alignment, including a
  late seek, odd frame boundaries and a one-frame final request.
- Cache reuse, source/quality/interval separation, and repair of corrupted entries.
- Real-time delivery during a controlled bandwidth drop, including abandoning a
  slow lossless request and retrying the same interval as Opus before it is needed.

The original demo assembles output **offline**. The separate live experiment below
uses real HTTP transfers and wall-clock PCM consumption, writing its output to a
file. Its optional macOS output helper also exercises a selected audio device.
These experiments do not establish behavior across arbitrary WAN conditions,
inaudibility on all music, or gapless playback across arbitrary source formats.
The natural-handoff experiment below covers two separate same-format tracks.

The `lossless` representation is re-encoded FLAC with the same decoded PCM for
supported inputs. It is not unchanged source bytes. Original-byte delivery, rates beyond 44.1/48 kHz, and lossy originals remain separate work.

## Live delivery proof

Reuse the synthetic fixture from the Navidrome experiment above, then run:

```sh
cargo build --release --bins --examples
python3 -B experiments/live.py \
  --navidrome /path/to/navidrome \
  --stratum target/release/stratum-poc \
  --checker target/release/examples/live_check \
  --fixture artifacts/navidrome-source.flac \
  --out artifacts/live-run
```

This takes about two minutes. Three disposable-server runs compare a steady link,
automatic quality through a bandwidth drop/recovery, and forced lossless through
the same drop. Nothing plays through your speakers automatically. Each scenario
produces `listening.wav`; real starvation is retained as silence in that file.
See [method, results and limits](docs/live-delivery-proof.md).

The live test now reuses an 8 MiB source cache across intervals within one account
and track session. Every interval still checks Navidrome access and stream revision
before using cached bytes. Add `--source-cache per-segment` to reproduce the earlier
independent-cache baseline. See [the source-session experiment](docs/source-session-proof.md)
for traffic comparisons and cached-access rejection checks.

### Listen through an audio device (macOS)

The optional native helper uses an AudioQueue callback to consume the same live
stream. Select an exact output from `--list`; the helper leaves the system's
default output, volume and sample rate settings alone. The test uses its own
reduced gain and stops after one excerpt. See [build and listening commands](docs/device-output-proof.md).
Normal runs and automated unit tests still produce no sound.

### Seeking and skipping

`experiments/seek.py` extends the live proof with backward/rapid seeks and a skip
between two tracks. It cancels obsolete HTTP/decoder work, flushes the output,
rejects intentionally late completions, and checks exact resume positions. Its
default sink is a file; optional macOS playback uses one native process across
all resets. See [the sequence, commands and limits](docs/seek-proof.md).

### Natural track endings

`experiments/handoff.py` crosses a real track ending in disposable Navidrome on one
continuous timeline, including a one-frame EOF segment, different qualities at
the boundary and next-track preparation under a slow connection. A deliberately
late next track is the negative control. An independent source/PCM checker verifies
the join and catches missing, repeated or silenced samples. Optional native output
keeps the same queue running across the boundary. See [commands and limits](docs/handoff-proof.md).
Add `--next-source` to use two actual album files, playing the first file's real
ending into frame zero of the second instead of splitting one continuous source.

### Native 44.1 kHz sources

The core and natural-handoff harness accept 44.1 kHz sources, keeping requests,
lossless PCM and the client output on that native clock. Opus has an explicit
48 kHz codec clock and rate conversion. The file handoff proofs cover
both quality directions and automatic adaptation at 44.1 kHz; native output
also passes a lossless-to-Opus album transition. Both tracks must
have the same rate; mixed-rate track handoffs remain separate work. The older
`live.py`, `seek.py` and standalone `navidrome.py` scenarios remain 48 kHz.
See [contract, measurements and limits](docs/native-rate-proof.md).

## Checks

```sh
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test -- --test-threads=1
python3 -B -m unittest discover -s experiments -p 'test_*.py'
```

Integration tests run actual FFmpeg encodes and decodes, including a synthetic
154-second source. They fail if FFmpeg is missing. Each subprocess has a timeout,
direct arguments and output bounds; no shell, GUI or playback process is launched.

`src/timeline.rs` owns interval arithmetic; `src/media.rs` owns source validation,
encoding and cache metadata; `src/proof.rs` is the disposable fixture/reference
client. Discovery and Navidrome credentials do not belong in the audio engine.

## Next experiments

1. Listen to music at the joins and investigate any audible switching artifacts.
2. Build on the disposable Navidrome acquisition result: a production source adapter
   still needs user-scoped authorization, source revision handling and integration
   with the engine. The Python experiment currently feeds acquired intervals into
   the local Rust engine; it is not a shipped Sidecar service.
3. Extend the timing proof beyond 44.1/48 kHz and to lossy originals before defining a
   production timeline/representation contract.
4. Extend the device, seek/skip and natural-handoff experiments to Astra's pipeline,
   harder network conditions, queue edits and source-cache reuse across seeks. Discovery, pairing, per-user authorization
   and production cache eviction must be designed before deployment.

The current cache is an unbounded experiment directory, not a server cache. No
existing accounts, servers, music libraries, or Astra settings are modified by the tests.

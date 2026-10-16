# Stratum

An experimental audio transport for requesting precise intervals at different
qualities on a shared playback timeline. The intended first server implementation,
Astra Sidecar, will work alongside Navidrome; Astra will be a client.

This repository currently contains the **audio proof of concept**, not a deployed
server or an Astra integration. It generates independently decodable FLAC/Opus
segments, trims them onto one sample timeline, and produces a switched listening
sample with measurements.

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

Try your own **48 kHz, mono/stereo, 16/24-bit FLAC or PCM WAV**, from 6 seconds to
3 minutes long:

```sh
cargo run --release -- demo --source /path/to/music.flac --out artifacts/music
```

Other rates, layouts and lossy source formats are explicitly rejected. There is
no silent resampling or claim that a transcoded original is byte-preserving.
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

## What this establishes

- Integer frame addressing and independently decodable representations.
- Codec pre-skip handled once, followed by explicit context trimming.
- Exact lossless reconstruction of the requested source PCM.
- Independent reference comparisons for representation alignment, including a
  late seek, odd frame boundaries and a one-frame final request.
- Cache reuse, source/quality/interval separation, and repair of corrupted entries.

The proof assembles output **offline**. It does not yet establish real-time
buffer behavior, efficient Navidrome reads, inaudibility on all music, adaptation
under a changing connection, or gapless playback between different tracks.

The `lossless` representation is re-encoded FLAC with the same decoded PCM for
supported inputs. It is not unchanged source bytes. Original delivery, other
sample rates, and lossy originals remain separate work.

## Checks

```sh
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test -- --test-threads=1
```

Integration tests run actual FFmpeg encodes and decodes, including a synthetic
154-second source. They fail if FFmpeg is missing. Each subprocess has a timeout,
direct arguments and output bounds; no shell, GUI or playback process is launched.

`src/timeline.rs` owns interval arithmetic; `src/media.rs` owns source validation,
encoding and cache metadata; `src/proof.rs` is the disposable fixture/reference
client. Discovery and Navidrome credentials do not belong in the audio engine.

## Next experiments

1. Listen to music at the joins and investigate any audible switching artifacts.
2. Run the same interval requests through an authenticated Navidrome source adapter,
   measuring upstream bytes read and cold late-seek cost. Source reads are currently
   local, and opening a source hashes the entire file.
3. Extend the timing proof to 44.1/96 kHz and lossy originals before defining a
   production timeline/representation contract.
4. Add a bounded real-time client/server experiment. Discovery, pairing, per-user
   authorization, cache eviction and cancellation must be designed before deployment.

The current cache is an unbounded experiment directory, not a server cache. No
accounts, live servers, music libraries, or Astra settings are modified by the tests.

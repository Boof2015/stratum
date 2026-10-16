# Natural track handoff experiment

`experiments/handoff.py` tests natural EOF between **two separate Navidrome
tracks through one continuous PCM output**. It supports both a controlled split
fixture and two actual album files. Unlike the seek proof, it never
flushes, stops or recreates the audio queue at the boundary. No Astra code changes.

## What is exercised

In its default mode the harness takes a 32.000333-second excerpt of a supported source and splits it
losslessly into two independent FLAC files, then imports those files into a fresh,
disposable Navidrome. Their concatenation is verified against the source before
testing. The local full decode is only fixture/oracle work: playback still gets
every interval through authenticated Navidrome HTTP, on demand.

The first file ends at frame **576,007**, about 12 seconds into the excerpt. Its
last request contains **one frame**, which tests the tiny FLAC block workaround
and Opus end trimming. The second file starts at its own frame zero. Both the
track boundary and final output fall inside the sink's 960-frame quantum; they
must not be rounded, padded or truncated to fit it.

The server clamps the 120 ms encoder context to **each file's actual boundaries**.
It cannot borrow samples from the other file, even though the fixture originally
came from a continuous recording. The client validates track identity, per-track
position, global position, quality and frame count before admission. One global
frame counter feeds the existing file sink or noninteractive native helper.

The client prepares the next track while the current track is playing, with the
existing approximately 8-second buffer target. Automatic mode carries measured
network state across the track boundary and updates the lossless size estimate
for the new source. A tiny EOF segment does not update full-segment throughput/
preparation estimates: its headers and preparation cost are disproportionate.
This is a two-track scheduling proof, not a production queue or prefetch API.

With `--next-source`, the disposable server imports copies of the two **original
files**, without clipping or re-encoding them first. The client requests the final
576,007 frames of the first, reaches its actual EOF, then requests the first
960,009 frames of the second, beginning at frame zero. Context stays within each
original file. The independent checker uses those original files too, enforces
the first file's real EOF and the second's real beginning, and stops at the end
of the bounded second-track excerpt.

| Scenario | Track 1 → track 2 | Link / purpose |
| --- | --- | --- |
| `lossless` | FLAC → FLAC | Steady; exact PCM join |
| `mixed` | FLAC → Opus 64 | Slow-link preparation, different quality at the join |
| `reverse` | Opus 64 → FLAC | Reverse quality direction and one-frame lossy EOF |
| `adaptive` | Controller selects quality | Same bandwidth drop/recovery as the earlier live proof |
| `late-next` | FLAC → Opus 64 | Negative control: deliberately withhold the next request beyond EOF |

The impaired link is 10 Mbit/s, drops to 192 kbit/s at playback second 5, and
recovers at second 17, with a 60 ms response delay. For `mixed` and `adaptive`,
next-track preparation starts no earlier than second 5.25, to ensure it happens
during the slow period. The controller cannot see this schedule. This is actual
loopback HTTP body pacing, not WAN packet loss/congestion simulation.

## Reproduce without sound

Use the source fixture and Navidrome binary from the earlier experiments:

```sh
cargo build --release --bins --examples
python3 -B experiments/handoff.py \
  --navidrome /path/to/navidrome \
  --stratum target/release/stratum-poc \
  --checker target/release/examples/handoff_check \
  --source artifacts/navidrome-source.flac \
  --out artifacts/handoff-file --scenario all
```

Choose a fresh output directory. Five sequential runs take about three minutes.
Each case uses a fresh disposable library/account. No existing servers, accounts,
settings or source files are changed. Both HTTP listeners bind loopback; the
existing subprocess timeouts and Navidrome watchdog remain in effect. Each track
has its own private 8 MiB source cache and revalidates access before each interval.

Each case writes `delivery.json`, `report.json`, the received segments,
`rendered.s32le`, and `listening.wav`. The latter two **retain real starvation**.
The top-level directory also holds the split files and exact excerpt reference.
Source acquisition traffic excludes the setup reads used to match test tracks.

Native output is opt-in. Build and select a device as described in
[the device-output proof](device-output-proof.md), then add:

```sh
  --audio-helper artifacts/audio-output \
  --audio-device BuiltInSpeakerDevice --audio-gain 0
```

Choose one `--scenario` for native output; `all` is rejected. Gain zero exercises
the device callback without audible output. A nonzero gain plays sound. For
listening, use real music with `--source`, not the synthetic measurement signal.
The default excerpt starts at `80 * source_rate + 37`; `--origin` accepts another frame
position. Inputs can be native 44.1/48 kHz stereo 16/24-bit lossless, at most 10 minutes
and 96 MiB, with enough frames for the excerpt. This creates an artificial track
split in music, useful for hearing continuity independently of how an album was
mastered. It is not a test of every kind of real album boundary.

To test an actual album transition instead, supply both files:

```sh
python3 -B experiments/handoff.py \
  --navidrome /path/to/navidrome \
  --stratum target/release/stratum-poc \
  --checker target/release/examples/handoff_check \
  --source '/path/to/01. First song.flac' \
  --next-source '/path/to/02. Second song.flac' \
  --out artifacts/handoff-album --scenario mixed
```

Add the explicit audio options above for physical output. `--origin` is rejected
with `--next-source` because this mode always tests the actual file ending. Both
files must meet the supported-format limits and have the same sample rate; their
bit depths may differ. At 44.1 kHz, segments contain 88,201 frames, the boundary
is frame 529,207, and the full output has 1,411,216 frames. The earlier numbers
and recorded results below describe the original 48 kHz experiment. See the
[44.1 kHz follow-up](native-rate-proof.md) for its separate results.

## Independent checks

`examples/handoff_check.rs` decodes the two original files independently. It
requires exact ordered coverage of each declared interval and verifies:

- Lossless segments match every source sample, including the one-frame EOF.
- Lossy segments align with their correct source at beginning/middle/end windows.
- The full capture equals the submitted media plus **only** reported starvation,
  including the exact join and final short output buffer.
- The first file reaches its exact EOF and the second begins at zero; no old-track
  frame may substitute for the next file. In split-fixture mode both finish fully.

An individual one-frame lossy segment is too short for correlation. The report
explicitly counts it under `short_lossy_frames_without_lag_estimate`; its frame
count, identity, trim arithmetic and capture admission are checked, but it has no
independent lag estimate. Do not interpret 96 passing windows as measuring that
single sample's lossy fidelity.

The negative control intentionally pauses. The sink inserts silence without
advancing source position, then resumes with every original frame retained. Since
it consumes whole quanta, starvation can begin a few frames before the track
boundary when there is not enough PCM to fill the next quantum. A successful
negative-control run is **not** reported as gapless.

To check the oracle itself, run this against a successful all-lossless case:

```sh
python3 -B experiments/handoff_oracle_probe.py \
  --checker target/release/examples/handoff_check \
  --delivery artifacts/handoff-file/lossless/delivery.json
```

It verifies the baseline, then rejects seven corrupted copies: silence at the join,
a missing last frame, a repeated last frame, a sample corrupted in both segment
and capture, wrong track identity, stopping before the first file's real EOF,
and starting after the second file's frame zero. Originals are never edited. The fourth
case prevents a comparison against the submitted segments alone from falsely
establishing source accuracy.

## Recorded results

2026-10-16, Navidrome 0.64.2, native 48 kHz stereo 24-bit sources:

| Run | Starvation | Next track ready before boundary |
| --- | --- | --- |
| File, lossless → lossless | None | 6.020 s |
| File, lossless → Opus 64, slow link | None | 5.223 s |
| File, Opus 64 → lossless | None | 6.002 s |
| File, automatic, slow link | None | 3.786 s |
| File, deliberately late next track | 2.060 s, expected | 2.041 s **after** boundary |
| Bluetooth, muted, lossless → Opus 64 | None | 5.241 s |
| Bluetooth, BALALAIKA, lossless → Opus 64 | None | 5.249 s |
| Bluetooth, real album pair, muted, lossless → Opus 64 | None | 5.219 s |
| Bluetooth, real album pair, listening, lossless → Opus 64 | None | 5.224 s |

Every run completed exactly 1,536,016 media frames, passed 96 alignment windows
and the exact lossless/capture comparisons. The reverse-quality file run had one
lossy frame too short for correlation, explicitly reported as described above.
Automatic mode began the second track at Opus 64 and recovered to Opus 128 near
timeline second 28. Bluetooth ran at 44.1 kHz via CoreAudio's downstream conversion
from the 48 kHz client stream; no device timeline discontinuities were reported.

The real album pair was **Safe Room → What Are The Odds (feat. Kiyon)** from
*Patchwork*. Both files are 48 kHz, changing from 24-bit to 16-bit. The first
interval began at original frame 9,699,422, reached its actual EOF at 10,275,429,
then continued from the second file's frame zero. Both album runs passed the same
96-window/frame/lossless/capture checks, with no device timeline discontinuities.
The listener confirmed **“Clean transition.”** The earlier split-BALALAIKA test
also sounded smooth; it was a controlled file-boundary test, not two album songs.

The seven corruption controls also passed. Repository checks: 45 Python tests,
9 Rust tests, format and Clippy. Existing native helper and Rust audio engine
source are unchanged. Raw captures/reports stay ignored under
`artifacts/handoff-proof/`. These margins describe controlled runs, not general
network latency or gapless guarantees for arbitrary connections.

## Limits

Native captures are pre-gain PCM submissions, not recordings of physical or
Bluetooth output. Audible continuity still needs listening. Lossy boundary
waveforms can differ despite correct timing; no crossfade or silence removal is
used to conceal them. Lossless means exact source PCM, not original encoded bytes.

The original recorded runs use two native 48 kHz stereo lossless sources on one
configured device; the linked follow-up adds same-rate 44.1 kHz sources.
Neither covers mixed sample rates between tracks, lossy original codecs, long albums, queue edits while preparing the
next track, output-device changes, production authentication/discovery, or Astra
integration. The earlier source-revision/Last-Modified limitations still apply.

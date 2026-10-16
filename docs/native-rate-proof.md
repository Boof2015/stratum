# Native 44.1 kHz proof

This extends the audio engine and natural-handoff experiment to native 44.1 kHz
lossless sources. A 44.1 kHz request stays on that source's frame clock, lossless
FLAC retains its rate and exact PCM, and the client output also runs at 44.1 kHz.
This differs from the previous Bluetooth runs, which supplied 48 kHz client PCM
and let CoreAudio convert it to a 44.1 kHz device.

At this stage, the supported input set was 44.1/48 kHz, mono/stereo, 16/24-bit FLAC or PCM WAV.
The handoff/device harness requires stereo and two tracks at the **same rate**.
There is no Astra integration or production Sidecar service in this change.

## Two clocks, one presentation timeline

[Revision 2 of the interval contract](audio-proof.md) distinguishes the native
source rate from the payload's codec rate. FLAC uses the native rate. Opus uses
48 kHz; the client explicitly converts its decoded PCM back to 44.1 kHz before
trimming and admitting exactly the requested native frames.

Both conversions use FFmpeg's `swr` resampler with `exact_rational=1`, `async=0`
and `dither_method=none`. There is no time stretching, crossfade, silence padding,
lag correction or lossless-path resampling. These tested settings are part of
this experimental decoder contract, not a claim that arbitrary resamplers will
return the same counts or samples.

The encoder's real-source context begins on a multiple of 147 native frames,
which corresponds to 160 Opus frames. This anchors its phase to the track's frame
zero, including after late seeks. The Navidrome acquisition interval follows the
same alignment, so starting a temporary acquired file at its own zero does not
move the global conversion phase. Context remains inside that track's boundaries.

Two edge cases mattered:

- **Conversion tail:** ceiling both rate ratios predicts up to one extra native
  frame. FFmpeg actually flushes either N or N+1 depending on the interval. A
  request starting at native frame 6,615,037 exposed that assuming N+1 is wrong.
  The engine measures the count, accepts only the bounded zero/one extra frame,
  records it in `resample_tail_frames`, and includes it in the client's end trim.
  A short result fails. Rounding never accumulates on the presentation timeline.
- **Tiny FLAC input frame:** sample-accurate trimming can leave a three-frame
  first input frame even when the requested interval is long. The default FLAC
  encoder initialization rejected it as an invalid block size. Explicitly setting
  its block size to 4,096 fixes initialization without changing source samples
  or adding padding. Tiny requested tails still include and discard real context.

A whole source can be as short as 16 frames for lossless. Rate-converted Opus
requires at least 32 real context frames; smaller sources fail explicitly. Tests
cover the 16/31/32-frame boundary, both Opus qualities and exact lossless tails.
A one-frame request from an ordinary song works because real context is available.

## Reproduce

The default fixture remains 48 kHz. Supply its optional rate to generate native
44.1 kHz test data, then run the existing handoff harness:

```sh
cargo build --release --bins --examples
mkdir -p artifacts
cargo run --release --example fixture -- artifacts/source-441.flac 44100
python3 -B experiments/handoff.py \
  --navidrome /path/to/navidrome \
  --stratum target/release/stratum-poc \
  --checker target/release/examples/handoff_check \
  --source artifacts/source-441.flac \
  --out artifacts/handoff-441 --scenario all
```

Choose a fresh output directory. These file-sink runs play no sound. For an
actual album pair, use `--source` and `--next-source` with two supported files
at the same rate. Native output remains explicit; build/select a device using
[the device instructions](device-output-proof.md), choose one scenario and add
`--audio-helper`, `--audio-device`, and `--audio-gain 0` for a muted test. A nonzero
gain plays audio. The handoff harness passes the source rate to the helper;
its standalone `--rate` option defaults to 48 kHz for older callers.

At 44.1 kHz, segments have 88,201 frames, the first track ends at timeline frame
529,207, and the whole test presents 1,411,216 frames (32.000363 seconds). The
one-frame EOF request is retained. The consumer's 20 ms quantum is 882 frames,
so both the boundary and the final tail must cross partial quanta correctly.

## Recorded results

2026-10-16, macOS, FFmpeg 9.0.1, Navidrome 0.64.2. Each case streams through a
fresh disposable Navidrome using authenticated original-audio acquisition and
the existing per-account, per-track source cache. The network schedule is the
same real HTTP pacing used by the [handoff proof](handoff-proof.md).

| Run, all at 44.1 kHz | Starvation | Next track ready before boundary |
| --- | --- | --- |
| File, lossless → lossless | None | 6.011 s |
| File, lossless → Opus 64, slow link | None | 5.188 s |
| File, Opus 64 → lossless | None | 6.030 s |
| File, automatic, slow link | None | 3.780 s |
| File, deliberately late next track | 2.060 s, expected | 2.053 s **after** boundary |
| Bluetooth, album pair, muted, lossless → Opus 64 | None | 5.226 s |
| Bluetooth, album pair, listening, lossless → Opus 64 | None | 5.206 s |

All seven runs presented exactly 1,411,216 media frames and passed 96 evaluable
alignment windows, exact lossless/source comparisons and capture accounting.
The negative control retains its real 90,846 silence frames and every media
frame; it is not reported as gapless. The reverse-quality run has one lossy
frame too short for a correlation estimate, explicitly reported by the checker.
The automatic run began the second track at Opus 64 and reached Opus 128 near
second 26; it did not return to lossless within this excerpt.

The album pair was *Patchwork*: Safe Room → What Are The Odds (feat. Kiyon).
The user authorized temporary 44.1 kHz conversions of the previously supplied
48 kHz originals. These copies are the native 44.1 kHz sources for this test;
lossless comparisons are against those copies, not the 48 kHz originals. No
original music file was changed. The first copy is 24-bit / 9,440,551 frames,
the second 16-bit / 10,487,783 frames. Playback starts at the first's frame
8,911,344, reaches its actual EOF, then starts the second at frame zero.

Both Bluetooth runs used a 44.1 kHz client clock and a reported 44.1 kHz device
rate, with no queue timeline discontinuities. The audible run used queue gain
0.25, and the listener confirmed **“Clean transition.”** Captures are pre-gain
PCM submissions, not physical or Bluetooth output recordings; this is not a
bit-perfect Bluetooth claim.

Additional checks:

- **13 Rust tests:** native-rate switching, exact lossless PCM, zero-lag late
  seeks and starts around the 147-frame phase boundary, one-frame tails, WAV
  minimum lengths, corrupted clock metadata and existing 48 kHz regressions.
- **48 Python tests:** native scheduling/clock conversion bounds, wrong-rate
  responses, mixed-source-rate rejection and existing delivery/cancellation tests.
- **Independent oracle:** the 44.1 kHz baseline passes; all seven corrupted joins
  fail, including jointly corrupting both a segment and the captured submission.
- **Native helper:** strict C++ warnings-as-errors build; self-tests at both
  rates; muted 48 kHz legacy packets, fully queued reset, superseded startup,
  stale generation rejection and exact new-generation PCM all pass.
- A final 48 kHz Navidrome mixed-quality handoff regression passes all 96
  windows and exact frame/lossless/capture checks, with no starvation and the
  next track ready 5.253 seconds early.
- Format, Clippy and release build pass. The final explicit encoder-resampler
  settings were added after the file/muted runs; all 103 prior request segments
  were re-encoded and produced identical decoded samples and timeline metadata.
  The audible album run used the final settings. Ogg stream identifiers can
  change container hashes between otherwise equivalent encodes.

Ignored reports, captures and temporary music copies live under
`artifacts/native-rate-proof/`. No real server, account, Astra setting or system
output/volume/sample-rate setting was changed.

## Limits and next work

The older `live.py`, `seek.py` and standalone `navidrome.py` scenario CLIs remain
48 kHz. Shared helpers are rate-aware where the handoff harness needs them.
Exact 44.1 kHz seek positions are covered in the core and late album acquisition,
but the full interactive cancellation/rapid-seek sequence has not been run at
44.1 kHz. Mixed 44.1 ↔ 48 kHz track transitions are explicitly rejected: they
need a deliberate output-clock policy. 96 kHz, lossy originals, long-running
playback, output switching and queue edits remain separate experiments.

The earlier source-revision/Last-Modified limitation still applies. Production
scoped authorization, discovery/pairing, cache eviction and Astra integration
are not implemented by these proofs.

Codec timing reference: [Ogg Opus PCM positions and trimming](https://www.rfc-editor.org/rfc/rfc7845.html#section-4).
Conversion settings: [FFmpeg resampler documentation](https://ffmpeg.org/ffmpeg-resampler.html).

## Subsequent 96 kHz extension

The later [high-rate proof](high-rate-proof.md) adds 96 kHz sources, even-frame
resampler-phase anchoring and native buffer allocation by seconds. The 44.1 kHz
results above are unchanged historical runs; current support and the new
measurement-window reasoning are documented there.

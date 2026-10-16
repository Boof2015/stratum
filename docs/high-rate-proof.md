# Native 96 kHz proof

This extends the [44.1 kHz proof](native-rate-proof.md) to native 96 kHz lossless
sources. Requests, presentation, lossless payloads and the client PCM queue stay
at 96 kHz. Opus explicitly converts through its 48 kHz codec clock and returns
to the native clock before presentation trimming. No original files or Astra
code are changed; this remains a bounded proof, not a deployed Sidecar service.

## Timing and preservation

At 96 kHz, two native frames correspond to one Opus frame. The real-source
context begins on an **even** native frame, including the Navidrome acquisition
interval, so late seeks and odd request boundaries keep the conversion phase
anchored to the track's frame zero. Requests themselves are never rounded.

An odd encoded interval can produce one extra native frame after conversion.
The existing revision-2 metadata records the codec count, native decoded count,
and measured zero/one-frame tail explicitly. The client removes that tail along
with context and presents exactly the requested count. The earlier 44.1/48 kHz
encoding behavior and cache contract remain unchanged; 96 kHz is an extension
of the accepted source rates, not a reinterpretation of existing entries.

Lossless retains exact source PCM, native rate and bit depth. It is re-encoded
FLAC, not the original encoded bytes. The native 96 kHz synthetic fixture includes
a 30,017 Hz component, so a silent 48 kHz conversion cannot pass its exact lossless
comparison. This fixture is for silent measurement only. Opus is a reduced-quality
representation and does not preserve that high-frequency content; converting its
output back to 96 kHz preserves the timeline, not the original information.

The same explicit FFmpeg `swr` settings are used in both conversion directions:
`exact_rational=1`, `async=0`, and `dither_method=none`. No crossfade, time stretching,
silence padding or post-hoc lag correction is applied. At 96 kHz, rate-converted
Opus requires at least 64 real context frames; lossless still accepts a whole
source of 16 frames. Tests cover immediately below, at and above that minimum.
A tiny requested interval from a normal song includes real neighboring context.

## Measurement window investigation

The first stereo 96 kHz switching test flagged one right-channel Opus window at
native frame 960,005 with an estimated lag of −1. Its fixed 2,048-frame window
covered only 21.3 ms, half the observation duration of the original 48 kHz test.
At the higher rate, lossy reconstruction differences made that short estimate
unstable even though the other windows and exact frame accounting agreed.

The independent comparison now uses up to 4,096 frames at 96 kHz, preserving the
original approximately 42.7 ms duration. It still compares against the separate
full source decode and still requires **zero** lag in the proof fixtures. No
presented samples or trim values were changed to pass it. A dedicated test uses
the same troublesome position at all three qualities and deliberately misstates
its position by one native frame in either direction: every window detects the
signed error. The 44.1/48 kHz window lengths and existing tests are unchanged.

This remains a correlation estimate, not a mathematical proof of inaudibility
for arbitrary lossy content. Exact lossless samples, ordered frame coverage and
captured submissions are checked separately. One-frame lossy tails are too short
for an independent lag estimate and remain explicitly reported as such.

### Real music and the codec clock

The first muted album run completed playback with exact frame counts, exact
lossless PCM and an exact capture of submitted audio, but failed the original
strict native-lag assertion. Four of its 96 native comparison windows estimated
±1 frame in Opus 64 sections, with different signs between windows and channels.
Those estimates also appear in the later listening run; they have not been
removed or relabeled as zero.

At 96 kHz, one native frame is half of an Opus frame. A second independent
comparison now decodes the complete original source at 48 kHz, downconverts
the actual submitted 96 kHz lossy PCM to that same clock, and compares there.
For an odd global source start, this diagnostic skips the first native frame
to begin on the source's even phase. It never changes playback PCM or trims.
The diagnostic uses the explicit resampler settings above, validates its output
count, and compares beginning/middle/end windows per channel. All 60 lossy
windows in each album run have zero lag on this codec clock.

The report retains `zero_lag: false` for the native result and lists the four
estimates. The harness accepts this specific result only at 96 kHz: no
inconclusive native windows, all nonzero estimates confined to lossy sections
and strictly smaller than one codec frame, plus evaluable codec-clock windows
with no nonzero lag or inconclusive result. Larger native shifts, lossless
shifts, missing codec evidence and other source rates are rejected. Tests also
inject ±2 native frames (one codec frame) at all qualities and verify that the
codec-clock comparison detects the signed shift.

This is evidence of **codec-resolution waveform alignment and exact native
timeline accounting**, not proof that arbitrary lossy reconstruction preserves
sub-codec-frame waveform phase. The initial muted run's failed assertion is
preserved with its raw capture; `final-capture-checks.json` records its later
verification under the explicit criterion. No successful original report was
invented for that run.

## Native output resources

The macOS helper accepts `--rate 96000`. Its ring holds 12 seconds, capture storage
holds at most 65 seconds, and its 20 ms callbacks contain 1,920 frames. The input
packet bound is 192,001 frames. All storage is allocated before playback; the
callback still performs no allocation, file/pipe I/O, decoding or locking.

Sizing storage by the selected rate preserves the duration budget at 96 kHz.
At 48 kHz the allocation is unchanged; at 44.1 kHz it is now exactly 12 seconds
instead of the previous fixed allocation's approximately 13 seconds. The policy
still targets about eight seconds at each rate. Reports include the ring/capture
bounds. The bounded excerpt, process watchdog and cleanup rules remain in effect.

The helper leaves hardware rate, volume and default route settings unchanged.
A 96 kHz client queue can therefore be converted by CoreAudio to the selected
device's rate. Its capture is the exact submitted PCM **before** that conversion
and test gain, not physical or Bluetooth output.

## Reproduce

```sh
cargo build --release --bins --examples
mkdir -p artifacts
cargo run --release --example fixture -- artifacts/source-96.flac 96000
python3 -B experiments/handoff.py \
  --navidrome /path/to/navidrome \
  --stratum target/release/stratum-poc \
  --checker target/release/examples/handoff_check \
  --source artifacts/source-96.flac \
  --out artifacts/handoff-96 --scenario all
```

Choose a fresh output directory. These runs play no sound and use fresh disposable
Navidrome accounts/libraries on loopback. Native output is still opt-in; see the
[device instructions](device-output-proof.md). Use one scenario and gain zero for
a muted device test. For listening, supply real music with `--source` and optional
`--next-source`; never play the synthetic measurement signal through headphones.

Both tracks must use the same rate. At 96 kHz the plan presents 3,072,016 native
frames (32.000167 seconds), with a one-frame EOF at timeline frame 1,152,007.
No queue reset occurs at the natural track boundary.

## Recorded results

Checked on macOS with the existing FFmpeg 9.0.1 and Navidrome 0.64.2 tools.
Each file scenario presents all 3,072,016 native media frames, with exact
lossless PCM, exact rendered output and zero lag in all 96 native comparison
windows. The final checker also rechecked every capture on the codec clock.

| File scenario | Starvation | Next track ready relative to boundary |
| --- | --- | --- |
| Lossless → lossless | None | 4.492 s before |
| Lossless → Opus 64 | None | 2.072 s before |
| Opus 64 → lossless | None | 5.590 s before |
| Automatic quality | None | 4.478 s before |
| Deliberately late next track | 2.080 s | 2.066 s after |

The late control retains all media and reports 199,680 inserted silence frames;
it does not hide starvation by dropping music. The reverse case has one lossy
EOF frame too short for its own lag estimate. Automatic quality starts lossless,
uses Opus 128 near 8 seconds, Opus 64 near 14 seconds, and returns to Opus 128
near 28 seconds; it does not return to lossless within this excerpt.

The user authorized temporary 96 kHz copies of the actual adjacent Patchwork
tracks, *Safe Room* (24-bit) and *What Are The Odds (feat. Kiyon)* (16-bit).
Their original 48 kHz files remain unchanged. The copies contain 20,550,858 and
22,830,546 frames respectively; upsampling tests this path without adding
recorded detail. Exact lossless comparisons use these temporary 96 kHz sources.

The Bluetooth listen plays the first track's real ending into frame zero of the
second, changing lossless → Opus 64 at the boundary. At queue gain 0.25 it had
zero starvation and zero timeline discontinuities, presented all 3,072,016
frames, and prepared the next track 5.203 seconds early. Lossless and capture
comparisons are exact; codec alignment passes with the four native ±1 estimates
described above. The user reported **“Clean transition.”** The muted run also
had zero starvation and discontinuities, with the same measurement distinction.

The queue runs at 96 kHz; the Bluetooth device reports 44.1 kHz. CoreAudio's
downstream conversion and Bluetooth encoding are outside the PCM capture.
This is not a bit-perfect 96 kHz hardware-output result.

Validation: 18 Rust tests, 51 Python tests, format and Clippy checks, release
build, strict C++ compilation and native helper self-tests at all three rates.
The existing 48 kHz native output/reset controls pass in a muted probe. The
independent handoff oracle rejects seven deliberate corruptions. Exact native
seeks include odd starts, a late 150-second position, tiny whole sources and
the final one-frame tail. No test launches Electron or plays synthetic sound.

Ignored local evidence is retained under `artifacts/high-rate-proof/`, including
the file scenarios, initial muted capture, Bluetooth report, source preparation
hashes, final checker results and tested binaries. The raw scratch run remains
separate. These are local music/test artifacts, not repository content to commit.

## Scope

The core supports 44.1/48/96 kHz mono/stereo 16/24-bit FLAC and PCM WAV. The
handoff/device harness requires stereo. The older `live.py`, `seek.py` and standalone
`navidrome.py` scenario CLIs still use 48 kHz. Exact native 96 kHz seek positions
and late original-track acquisition are checked, but full interactive 96 kHz
rapid-seek/cancellation remains separate work. This same-rate command explicitly rejects mixed-rate track handoffs; the
separate fixed-output experiment linked below supplies an output policy.

Other rates, lossy originals, sustained playback, output switching, queue edits,
production authentication/discovery, stronger source revisions, and Astra
integration are not implemented here. The Navidrome Last-Modified scan/second-
resolution limitation still applies. Production clients also need an explicit
conversion contract; these results concern the tested FFmpeg build and settings.

References: [Opus timing](https://www.rfc-editor.org/rfc/rfc7845.html#section-4)
and [FFmpeg resampler settings](https://ffmpeg.org/ffmpeg-resampler.html).

## Subsequent mixed-rate output experiment

The [mixed-rate proof](mixed-rate-proof.md) adds a separate, explicitly selected
fixed client output clock. It retains native source delivery and the limits
above; the original same-rate handoff command still rejects mixed sources.

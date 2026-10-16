# Mixed source rates on one output clock

This extends the [native 96 kHz proof](high-rate-proof.md) to consecutive tracks
with different native rates. It is an explicit **client output experiment**,
not a change to Stratum's source or representation contract. The original
same-rate handoff experiment continues to reject mixed rates without a policy.

## Output policy

The caller chooses one output clock with `--output-rate`: 44,100, 48,000 or
96,000 Hz. It stays fixed throughout the queue, including track and quality
changes. Each source is fetched, encoded, decoded and trimmed in its own native
clock first. A client converter then maps that PCM to the output clock. Sources
already at the output rate bypass conversion.

This matches the separation Astra already makes between source format and
processed output. The experiment uses FFmpeg `swr`, not Astra's actual resampler;
it is evidence for the transport/client boundary, not an integration test of
Astra. There is no change to Astra's output modes or settings.

Lossless delivery still preserves exact native source PCM. Output resampling
is explicitly reported and is not bit-perfect playback. A future bit-perfect
client must negotiate source/output formats and may need a break when the device
format changes, as previously agreed. This proof never silently converts such a
mode to keep a gapless claim. It implements no bit-perfect mode.

## Three clocks, kept separate

The source request uses integer native frames. Opus has its separate 48 kHz
codec clock, handled by the existing engine. Finally, client output uses the
selected fixed rate. A native segment's frame count is never admitted as an
output count.

The output boundary after track N is the floor of:

```text
output_rate × sum(native_presentation_frames[i] / native_sample_rate[i])
```

The Python plan uses exact rational arithmetic. The independent Rust checker
uses integer ticks at the least common multiple of the supported clocks. Track
output counts are differences between consecutive boundaries. Rounding once per
segment or independently per track would accumulate drift; this policy remains
less than one output frame short of the exact accumulated duration at every
boundary, including a test with 999 alternating-rate tracks.

When an excerpt starts inside a source, the first output position rounds down
to the source's output grid, by less than one output frame. Conversion begins
on an integer phase anchored to source frame zero, with up to 120 ms of real
preceding context plus phase alignment. Real following context is also fetched
when the excerpt ends before EOF. A true source end uses no invented input
samples. Output context and the bounded fractional tail are discarded explicitly;
short conversion output is an error and is never padded.

This is an experimental output quantization policy, not a change to native
request positions or a finalized wire-protocol rule. Mixed rates do not share
a universal native sample grid.

## Continuous conversion and delivery

`output_clock.Converter` keeps one resampler alive for each track across network
segments and quality changes. It drains the filter only after all of that track's
real input/context arrives. It never flushes on starvation. The next track gets
its own conversion state, while the output consumer keeps running on the same
clock with contiguous frame positions and no reset or crossfade.

Conversion runs outside the audio callback. The native helper receives bounded
packets in its already-supported output format. Subprocess pipes are serviced
while writing, have deadlines, and are closed on success or failure. The FFmpeg
CLI has input/output packet batching in addition to filter latency; the adapter
allows a bounded amount of pending output and reports its observed peak. This
is proof tooling, not a proposed production low-latency resampler wrapper.

The existing disposable Navidrome, per-account source sessions, interval engine,
link shaper and quality policy are reused. Every source acquisition still checks
access and stream revision, including cache hits. The two tracks can therefore
have different native rates without making Sidecar resample lossless delivery.
Extra context requests are visible in traffic and frame accounting; they are
not free or hidden whole-song downloads.

## Verification

The independent checker verifies each track's actual source format, native
coverage, original EOF into next-track frame zero, quality, PCM length and
lossless samples. Lossy alignment uses the existing native/Opus-clock rules,
including the documented 96 kHz correlation limits. It separately verifies the
cumulative output mapping and captured output, retaining starvation silence.

A separate one-shot conversion of the concatenated native submissions must
match every output sample from the incremental converter. For all-lossless
tracks, a full conversion of the original source is also compared at the
expected absolute position. This catches a converter that restarts at segment
boundaries or loses phase during a late seek, rather than merely comparing an
output with a copy of itself.

Tests exercise all nine native/output rate pairs, odd positions, short writes,
a one-frame final input, early output before EOF, cleanup and invalid native
ordering. A negative control explicitly shows that independently converting
network segments differs from continuous conversion. The oracle probe injects
wrong clocks, durations and trims; missing/reordered native frames; silent,
missing or repeated output samples; and matching corruption in both an output
file and its capture.

## Reproduce

```sh
cargo build --release --bins --examples
python3 -B experiments/mixed_handoff.py \
  --navidrome /path/to/navidrome \
  --stratum target/release/stratum-poc \
  --checker target/release/examples/mixed_check \
  --source /path/to/first.flac \
  --next-source /path/to/second.flac \
  --output-rate 48000 \
  --scenario mixed \
  --out artifacts/mixed-rate-run
```

Both sources must be supported stereo lossless files and fit the existing proof
bounds. The first must contain at least 13 seconds, the second at least 21.
Use a fresh output directory. The excerpt plays approximately the final 12
seconds of the first into the first 20 seconds of the second. The first track
includes an explicit one-native-frame EOF segment. Requests and consumer time
are measured live; output is a file unless an audio device is explicitly chosen.

The scenarios are `lossless`, `mixed` (lossless → Opus 64 on the impaired link),
`reverse`, `adaptive`, and `late-next`. `all` runs them sequentially. The late
control intentionally delays the second track past its deadline; every media
frame must remain, and the inserted silence must be reported.

For native output, use the helper and explicit device selection described in
[the device proof](device-output-proof.md), with one scenario at a time. The
helper uses the chosen output clock, not either source's rate; hardware settings
remain unchanged. Its capture precedes device conversion and test gain. Never
play the synthetic measurement fixture audibly.

```sh
python3 -B -m unittest discover -s experiments -p 'test_*.py'
python3 -B experiments/mixed_oracle_probe.py \
  --checker target/release/examples/mixed_check \
  --delivery artifacts/mixed-lossless/lossless/delivery.json
```

## Recorded results

Checked on macOS with the same FFmpeg 9.0.1 and Navidrome 0.64.2 tools as
the earlier proofs. All six directed source-rate pairs were exercised. Output
selection spans all three supported clocks; the lower-level conversion tests
cover all nine source/output pairs, including bypass.

| Native sources | Output | Scenario | Starvation | Next output ready |
| --- | --- | --- | --- | --- |
| 44.1 → 96 kHz | 48 kHz | Lossless → Opus 64 | None | 5.197 s early |
| 44.1 → 48 kHz | 48 kHz | Lossless | None | 6.313 s early |
| 48 → 44.1 kHz | 44.1 kHz | Lossless → Opus 64 | None | 5.198 s early |
| 96 → 44.1 kHz | 48 kHz | Opus 64 → lossless | None | 6.219 s early |
| 48 → 96 kHz | 96 kHz | Lossless → Opus 64 | None | 5.188 s early |
| 96 → 48 kHz | 44.1 kHz | Lossless | None | 6.232 s early |
| 44.1 → 96 kHz | 48 kHz | Automatic quality | None | 4.467 s early |
| 96 → 44.1 kHz | 48 kHz | Deliberately late | 2.060 s | 2.053 s late |

The late control retains every media frame and reports 98,880 inserted silence
frames. The automatic run changes from lossless to Opus 128, down to Opus 64,
and back to Opus 128. Resampler state survives those quality changes.

Every run passes exact native lossless checks, independent output conversion
comparisons and captured-output accounting. Three runs using 96 kHz lossy audio
have four or five native correlation estimates at ±1 frame, with zero lag in
all 66 independent Opus-clock windows. Those native estimates remain visible;
this does not strengthen the earlier codec-resolution alignment claim into an
arbitrary sub-codec waveform guarantee. Each track has 42 or 66 evaluable native
windows, including conversion context; a tiny lossy EOF frame remains too short
for its own correlation estimate.

All-lossless output tracks also match an independent full original-source
conversion exactly at the requested output-grid positions. The oracle probe
rejects all ten injected corruptions, including an output and capture changed
identically. The test that restarts conversion on each network segment fails
the continuous-reference comparison, as intended.

The muted native-output run reuses the authorized temporary Patchwork copies:
44.1 kHz *Safe Room* (24-bit) → 96 kHz *What Are The Odds (feat. Kiyon)* (16-bit),
with lossless → Opus 64 at the boundary. Its output queue stays at 48 kHz.
It has zero starvation and zero timeline discontinuities, and next-track output
is ready 5.187 seconds early. Original music hashes remain unchanged.

The authorized Bluetooth listen at queue gain 0.25 also passed: zero starvation,
zero timeline discontinuities, 1,536,012 output frames, and next-track output ready
5.183 seconds early. The user reported **“Clean transition.”** Native lossless
and continuous output checks are exact. The 96 kHz lossy track retains four
±1 native-frame correlation estimates; all 66 independent codec-clock windows
have zero lag. Nothing is shifted to make those estimates disappear.

The Bluetooth device reports 44.1 kHz while the client queue stays at 48 kHz.
CoreAudio's downstream conversion and Bluetooth encoding are outside this
capture. The result is a clean processed-output handoff, not a bit-perfect
hardware-output claim.

Validation: 18 Rust tests, 57 Python tests, format/Clippy checks and release
build. The audio engine, original handoff experiment and native output helper
are unchanged. Local raw evidence, fixtures and tested binaries are retained
in the ignored `artifacts/mixed-rate-proof/` directory.


## Remaining scope

The output-rate choice is explicit test configuration. Automatic hardware-rate
selection, mid-playback output-device changes, mixed channel layouts, arbitrary
rates/codecs, full interactive seek/cancel integration and production resource
policies remain separate work. Existing same-rate core and playback proofs are
preserved. Normal client output conversion does not change native lossless
transport guarantees or remove the known Navidrome source-revision limitations.

References: [FFmpeg resampler options](https://ffmpeg.org/ffmpeg-resampler.html)
and [libswresample state and draining](https://ffmpeg.org/doxygen/trunk/group__lswr.html).

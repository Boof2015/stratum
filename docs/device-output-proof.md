# Native audio-output experiment (macOS)

This extends the [live delivery experiment](live-delivery-proof.md) with real
audio output. The same disposable Navidrome, source-session cache, live HTTP
shaper and quality controller feed a native AudioQueue. Astra is not involved.

## Build and choose an output

Requires the macOS command-line developer tools, plus the existing Navidrome,
FFmpeg, Python and Rust prerequisites. Build the experiment binaries and helper:

```sh
cargo build --release --bins --examples
mkdir -p artifacts
clang++ -std=c++20 -O2 -fblocks -Wall -Wextra -Werror \
  experiments/audio_output_macos.cpp \
  -framework AudioToolbox -framework CoreAudio -framework CoreFoundation \
  -o artifacts/audio-output
artifacts/audio-output --self-test
artifacts/audio-output --list
```

`--list` prints connected stereo outputs and their exact UIDs. It does not start
playback. Use the UID for the intended speakers or headphones. There is no
implicit default device or automatic fallback to another output. The helper does
not change the system default, system volume or nominal sample-rate setting.
It opens an output queue only, without a microphone, browser or Electron process.

## Run one listening test

Reuse `artifacts/navidrome-source.flac` from the earlier acquisition experiment.
Choose a fresh output directory and substitute the device UID from `--list`:

```sh
python3 -B experiments/live.py \
  --navidrome /path/to/navidrome \
  --stratum target/release/stratum-poc \
  --checker target/release/examples/live_check \
  --fixture artifacts/navidrome-source.flac \
  --out artifacts/device-speakers \
  --scenario adaptive \
  --audio-helper artifacts/audio-output \
  --audio-device BuiltInSpeakerDevice \
  --audio-gain 0.25
```

This command **plays audio** after an initial buffer fills. Gain 0.25 is about
−12 dB, applied to this queue only. The ordinary system volume still applies.
Use `--audio-gain 0` for a muted device test. Ctrl-C stops the run. The helper has
its own 95-second lifetime limit; the Python wrapper also terminates and reaps
its child on cancellation or failure. The disposable server has its existing
independent watchdog and cleanup.

The excerpt is just over 32 seconds. HTTP payload delivery drops from 10 Mbit/s
to 192 kbit/s at playback time 5 s and recovers at 17 s. With the tested policy,
audible representation changes occur near media time 12 s (lossless → Opus64),
24 s (Opus128) and 30 s (lossless). These positions are measured outcomes,
not scripted quality choices. Source reads and segment delivery continue after
playback starts. Changing output requires a separate run with that output's UID.

For comfortable headphone listening, replace `--fixture ...` with
`--source '/path/to/music.flac'`. This uses the music's 1:20–1:52 excerpt and the
same real delivery path. Sources must be native 48 kHz stereo, 16/24-bit FLAC or
PCM WAV, long enough for that excerpt plus context, and at most 10 minutes /
96 MiB. The harness does not convert an unsupported source behind your back.
It copies the supplied file into its disposable library; the original stays
untouched. Its initial size estimate uses the actual source duration. Different
music can change the quality-switch positions and correlation results, so the
synthetic fixture remains the deterministic measurement case.

`--scenario forced-lossless` is the negative control under the same network
schedule: it should starve because it cannot lower quality. Use gain zero to
check this without playing another excerpt. Hardware runs reject `--scenario all`
so one command cannot unexpectedly play all three scenarios.

## What the output path measures

```text
live HTTP → decode/trim → framed PCM pipe → bounded native PCM ring
                                                    ↓
                                  AudioQueue → selected device
                                      ↓
                            pre-gain submission capture
```

`experiments/device_output.py` writes ordered integer frame intervals over stdin.
The native helper publishes each interval only when its whole packet is present.
The producer fills a 12-second ring; playback starts with about six seconds and
the unchanged policy aims for eight. Three 20 ms AudioQueue buffers sit after
that ring. Reported policy buffer excludes those buffers, conservatively leaving
up to 60 ms out of its estimate.

The serial output callback copies PCM and records the submitted samples into
preallocated memory. It performs no file/pipe I/O, allocation, locking or decode.
If a quantum is unavailable, it submits silence and leaves the media position
unchanged. It records every such interval. The final short buffer is retained,
and the queue drains before disposal; the helper does not cut off the last segment.
The independent Rust checker compares the captured submission to the delivered
segments plus exactly the recorded silence, and checks lossless PCM and alignment
against the original after playback is over.

The helper checks its selected route, device availability, queue conversion
errors, queue sample time and timeline discontinuities. The client format stays
48 kHz stereo signed 32-bit PCM; CoreAudio handles any output-rate conversion.
`device.json` records the device's rate, which can differ from the client rate.
No hardware rate is forced. Bluetooth may also add its own codec and latency.

Outputs include:

- `report.json`: network, adaptation, source-cache and independent audio checks.
- `adaptive/device.json`: device, queue timing, frames, gain and starvation.
- `adaptive/rendered.s32le`: PCM submitted to the queue, **before** gain/conversion.
- `adaptive/listening.wav`: that same captured submission for later inspection.
- `adaptive/device-stderr.log`: helper errors, if any.

The capture does not record the speakers, Bluetooth link or physical DAC output.
Exact submitted PCM and no ring starvation do not alone prove inaudibility,
bit-perfect output or absence of downstream device glitches. Listening remains
part of this experiment. Queue time measures time since the queue started, so
the final reading can include device drain overhead. It must cover the submitted
duration (within the 60 ms queue depth) and agree with the run's elapsed wall time
within 250 ms. Wall time allows up to three seconds of device overhead. Exact
frame accounting is checked separately. The first Bluetooth run exposed a bad
assertion comparing queue time directly to content duration: it ran 32.329 s
and reported a 32.286 s queue clock for 32.000 s of correct PCM. The corrected
check compares the two clocks and checks content coverage separately; it still
rejects a frozen, prematurely ending, discontinuous or wall-clock-divergent queue.

## Validation and limits

Recorded macOS runs on 2026-10-16, under ignored `artifacts/device-output/`:

| Run | Device rate | Ring starvation | Result |
| --- | --- | --- | --- |
| Speaker adaptive, muted | 48 kHz | 0 | Passed |
| Speaker adaptive, listening | 48 kHz | 0 | Passed; listener confirmed smooth |
| Bluetooth music, first listen | 44.1 kHz | 0 | Audio checks passed; original clock assertion failed as described above |
| Bluetooth music, repeat | 44.1 kHz | 0 | Passed with corrected clock check; listener confirmed smooth |
| Speaker forced-lossless, muted | 48 kHz | 3.48 s | Expected negative control, two starvation intervals |

All five captures contain the full 1,536,016 media frames, pass all 96 alignment
windows, preserve lossless PCM exactly, and match the submitted segments plus
reported starvation. No queue timeline discontinuities were reported. The music
was a native 48 kHz stereo 24-bit FLAC; CoreAudio fed the Bluetooth output at
44.1 kHz. The listener could hear reduced high-end detail at lower
quality but no gap or jump at the switches. This establishes those specific
runs, not inaudibility for every codec transition or track.

The speaker listening run consumed 11,325,661 upstream bytes; both music runs
consumed 10,628,952. Each adaptive run cancelled one endangered lossless response,
retried the same interval as Opus64 and recovered through Opus128 to lossless.
Muted and audible runs use the same data path; only the queue gain differs.

The native `--self-test` exercises starvation without advancing the source,
sample ordering across ring wraparound, the odd final buffer, capture counters,
EOF and rejection of reordered packets. It opens no device. The Python tests
exercise real subprocess pipes for wrong-device selection, startup failure,
invalid counters and cancellation while the status reader is blocked.
The complete Python suite passes 32 tests; the native helper builds with warnings
as errors and its self-test passes. No Rust engine code changed in this step.

This helper deliberately supports only a roughly 32-second excerpt, with at most
65 seconds of captured audio including starvation. It is a
bounded macOS experiment, not a production player or portable client library.
Other source rates/formats, track handoffs, seeking, output switching during a
track, sustained device interruptions and Astra integration remain separate work.

The output uses Apple's [Audio Queue Services](https://developer.apple.com/documentation/audiotoolbox/audio-queue-services)
and [queue timeline](https://developer.apple.com/documentation/audiotoolbox/audioqueuegetcurrenttime(_:_:_:_:)).

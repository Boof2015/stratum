# Seeking and skipping proof

This extends the [device-output experiment](device-output-proof.md) with
cancellation and playback generations. Each seek or skip supersedes the previous
generation. The experiment checks the actual PCM submitted after each reset,
using two tracks in a disposable Navidrome and the same real HTTP bandwidth drop.
It does not change Astra or define a production Stratum protocol.

## Reproduce

Build the Rust binaries and macOS helper using the commands in the device-output
document. For a silent, wall-clock file-sink run:

```sh
python3 -B experiments/seek.py \
  --navidrome /path/to/navidrome \
  --stratum target/release/stratum-poc \
  --checker target/release/examples/live_check \
  --source '/path/to/first-track.flac' \
  --skip-source '/path/to/second-track.flac' \
  --out artifacts/seek-file
```

Both sources must be different native 48 kHz stereo 16/24-bit FLAC/PCM WAV files,
within the live experiment's 10-minute / 96 MiB bounds. The first must cover the
late 2:00 seek plus 32 seconds and codec context. The second currently uses the
same minimum source-length validation as the original live test (about 1:52),
although the actual skip plays around 0:20. Source files are copied into the
temporary server library and left untouched. Use a fresh output directory.

For the two-track fixture setup, the harness matches server entries by file size
and an authenticated original-byte prefix (at most 64 KiB per track). Ambiguous
matches fail. Navidrome's Subsonic paths can be built from tags, so temporary
filenames are not a reliable identity. The first music attempt exposed that
assumption and stopped before playback. These two setup reads are separate from
the reported playback acquisition traffic; this is a test-fixture mapping, not
a proposed production track-identity scheme.

To use a real output, add all of these flags (this **plays audio**):

```sh
  --audio-helper artifacts/audio-output \
  --audio-device 'UID from audio-output --list' \
  --audio-gain 0.25
```

Use gain zero for muted native validation. One native process handles the whole
sequence; no GUI or Electron is started. Output selection is explicit and global
device/volume settings stay unchanged. The helper still has a 95-second lifetime
limit; its wrapper has a separate 90-second watchdog. Ctrl-C stops the experiment.

## The sequence

1. Start track A at frame `80 * 48000 + 37`.
2. With a lossless HTTP body still arriving on the slow link, seek backward to
   `40 * 48000 + 71`. Cancel the socket and server request, flush the old output,
   and retain the client's measured bandwidth estimate.
3. Hold one completed result from that generation just before admission. This
   deliberately models a response/decode completion that cancellation cannot undo.
   Seek to `60 * 48000 + 113`.
4. Once that seek has reached the server, supersede it with a seek to
   `120 * 48000 + 151`. Release the older completed result and require rejection.
   The intermediate 1:00 seek must produce no audio. Native runs also inject one
   obsolete PCM packet directly into the helper, bypassing the client guard, and
   require the native boundary to reject it too.
5. After the latest seek has played for a second, skip to track B at
   `20 * 48000 + 197` while a track-A request is pending. Play four segments
   (384,004 frames, about eight seconds), then drain and stop.

The network schedule is measured from the first playback start, and **does not
reset on seeks**: 10 Mbit/s → 192 kbit/s at five seconds → 10 Mbit/s at 17 seconds.
Triggers depend on real in-flight work, not only timers. The quality controller
uses observed throughput/preparation time, not knowledge of this schedule.

Seeks intentionally interrupt music. A buffering pause at each requested jump is
expected. This proof checks that old audio cannot be admitted after the reset,
the latest request wins, and each resumed segment starts at the correct frame.
It does not claim gaplessness across a deliberate seek.

## Cancellation and output ownership

Every response includes generation, track and source frame identity. The client
validates them before decoding. Changing position shuts down the old request's
socket and sends an explicit cancellation to the local test endpoint. Acquisition,
encoding and client decoding use cancellable process groups; completed work still
passes through a generation check before it can update the quality estimate or
append PCM. That check and append are serialized with changes of position.

The native interactive protocol carries generation/start/frame-count headers.
A reset first stops further callback submissions, immediately stops and disposes
the old AudioQueue, then opens the selected device for the new generation. Only
after this does it acknowledge the new output. The process stays the same; at
most eight output generations are allowed, and this test uses five. An older
generation's PCM is discarded; unexpected future generations are rejected.
Control commands remain readable even after all of the current excerpt's PCM
has been queued, so a seek near its end can still interrupt playback.

The first native attempt found a real reset bug: `AudioQueueStop(true)` can return
buffers through the callback. The callback was attempting to re-enqueue during
the stop. The fix quiesces the serial callback before stopping the queue; the
subsequent native sequence exercises four resets successfully.

Initial playback still buffers three segments (~6 s). After a seek, it buffers
one (~2 s) while the normal target remains eight seconds. The first file run
measured ~4.7 s to resume with the original startup rule; the corrected native
synthetic run measured ~1.7–1.9 s on the slow link, with no starvation after
resuming. These are observed prototype times, not a latency promise. Generation
reset timings and resume timings are recorded separately.

Each generation has its own private source session (up to five 8 MiB caches),
closed at cleanup. Reusing cached source bytes across seeks is future work; the
current proof favors clear ownership when an acquisition is cancelled. Existing
source-identity and authorization limitations still apply.

## Independent checks and artifacts

`report.json` records all generations, requests, cancellation/late-admission
outcomes, resume times and per-generation audio checks. Each `generation-N/`
contains accepted segment PCM, captured output, `delivery.json`, and native
`device.json` where applicable. The checker compares every accepted segment to
the correct source track/offset. It then compares the actual captured submission
to the consumed prefix of those segments plus exactly the recorded starvation.
This permits an intentionally interrupted last segment without allowing unrelated
tail audio, repeated samples or concealed silence.

Captures are PCM submitted to the output queue before gain/device conversion.
The interrupted generation can include up to the queue's already-submitted
buffers; it is not an acoustic record of the last audible sample. A Bluetooth
device may also retain audio already sent to it. The test proves exclusion after
the new generation is accepted by the client/native queue, and listening checks
for audible problems. It cannot retract samples already delivered to hardware.

The deterministic synthetic track-B fixture for the recorded run swaps the
channels of the original 160-second fixture, giving it distinct PCM and identity:

```sh
ffmpeg -nostdin -v error -i artifacts/navidrome-source.flac \
  -map 0:a:0 -af 'pan=stereo|c0=c1|c1=c0' -map_metadata -1 \
  -c:a flac -sample_fmt s32 -bits_per_raw_sample 24 artifacts/seek-second.flac
```

Use real music for headphone listening; the synthetic fixture is a measurement
signal. Track transitions at natural EOF, gapless track changes, arbitrary rates,
progressive decoding before an entire segment arrives, production authentication,
and Astra integration remain separate work.

The short optional device regression probe always uses gain zero:

```sh
python3 -B experiments/output_probe.py --helper artifacts/audio-output \
  --device 'UID from audio-output --list' --out artifacts/output-probe
```

It checks the original two-word PCM packet format and odd final tail, interrupts
a fully queued excerpt, supersedes an empty startup, injects stale PCM, and
requires the new generation's capture to equal its input exactly.

## Recorded results (2026-10-16)

Ignored artifacts are under `artifacts/seek-proof/`:

| Run | Outcome |
| --- | --- |
| `synthetic-file` | All generations and PCM checks passed; original six-second startup rule exposed slow seek resumes |
| `synthetic-native-fast` | All checks passed, including native stale-packet rejection; 72 alignment windows, no starvation |
| `music-bluetooth-mapped` | All checks passed with two user-selected songs; 72 alignment windows, no starvation |
| `output-probe` | Legacy packet/tail, fully queued seek, superseded startup and native stale-packet checks passed |

The Bluetooth listening sequence lasted about 25.5 seconds including startup and
seek pauses. Resumes took 1.72–1.74 seconds from request to queue start under this
profile. The intermediate generation accepted and emitted zero frames; the final
track played all 384,004 frames. The listener confirmed clean jumps and the correct
track change. CoreAudio used the connected device at 44.1 kHz with 48 kHz input.
All completed output captures matched accepted PCM exactly, including interrupted
prefixes. Lossless sections matched the original; all evaluated alignment windows
had zero measured lag. No device timeline discontinuities were reported.

39 Python tests and 9 Rust tests pass; Rust formatting/Clippy and the strict C++
build pass. The native self-test passes. The final fully-queued-control adjustment
was checked with the short output probe after the full music run. Test processes
were stopped. No Astra source or real-server library/settings were changed.

For native 44.1/48/96 kHz sources feeding an explicit fixed client output clock,
see the [mixed-rate seek/skip extension](mixed-seek-proof.md). The original
48 kHz command above retains its defaults.

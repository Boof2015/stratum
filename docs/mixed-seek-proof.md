# Seeking through mixed-rate output conversion

This extends the [mixed-rate output proof](mixed-rate-proof.md) and the earlier
[seek/skip proof](seek-proof.md). Source requests remain native 44.1/48/96 kHz;
one explicitly chosen client output clock stays fixed across seeks and skips.
This is normal processed playback, with no Astra integration or bit-perfect
policy change. The experiment uses disposable Navidrome data.

## Reproduce

```sh
cargo build --release --bins --examples
python3 -B experiments/mixed_seek.py \
  --navidrome /path/to/navidrome \
  --stratum target/release/stratum-poc \
  --checker target/release/examples/mixed_check \
  --source /path/to/first-native-track.flac \
  --skip-source /path/to/different-native-track.flac \
  --output-rate 48000 \
  --out artifacts/mixed-seek-file
```

Default output is a silent wall-clock file sink. Use two different stereo
16/24-bit FLAC or PCM WAV sources within the existing 96 MiB / ten-minute bounds.
The first must exceed 153 seconds and the second 29 seconds. Native and output
rates can each be 44,100, 48,000 or 96,000 Hz. Choose a fresh output directory.
The existing 48 kHz `seek.py` remains usable with its previous defaults.

For the existing macOS helper, add these flags (nonzero gain **plays audio**):

```sh
  --audio-helper artifacts/audio-output \
  --audio-device 'UID from audio-output --list' \
  --audio-gain 0
```

Gain zero validates native submissions silently. One process handles all five
playback generations. Explicit device selection and per-queue gain leave system
volume, the default route and hardware-rate settings unchanged. Use real music
and an explicit readiness response before a listening run, never synthetic
measurement signals. The helper/wrapper lifetime limits still apply.

## Scripted races

Positions are expressed in each source's native frames, including deliberately
odd offsets. The sequence is:

1. Start A at `80 * source_rate + 37`. Interrupt a real lossless download after
   the link slows down five seconds into initial playback.
2. Seek back to `40 * source_rate + 71`. Let playback resume, then hold a real
   converted output block just before its final output-admission check.
3. Seek to `60 * source_rate + 113`. Hold a decoded native block before input
   admission, then supersede this generation with a seek to
   `120 * source_rate + 151`.
4. Release both older completions. Both must be rejected; the intermediate
   generation must have accepted and played zero output frames. Native runs
   also inject an old packet directly into the helper, bypassing the client
   guard, and require native rejection.
5. Once the latest seek resumes, skip during a pending request to track B at
   `20 * B_rate + 197`. Play `8 * B_rate + 9` native frames and finish normally.

The link schedule remains 10 Mbit/s → 192 kbit/s at five seconds → 10 Mbit/s at
17 seconds, measured from initial playback rather than restarted on seeks.
Quality selection is deliberately scripted: initial playback is lossless;
resumed runs feed a lossless context block then Opus 64 presentation blocks
through the same converter. This isolates cancellation at a quality change;
it is not another test of automatic quality selection.

A deliberate seek interrupts music. Startup and buffering pauses at those
requested jumps are expected. The proof checks correct resumed position and
absence of old audio after reset, not gaplessness across a user-requested seek.

## Ownership and cancellation

A generation owns its request ticket, native input sequence, converter and
captured output. A seek advances the generation and resets the output sink under
the same lock used by output admission. Native completion admission checks the
generation before recording PCM or updating bandwidth observations. Conversion
runs outside that lock so a blocked old pipe cannot prevent a newer seek.

The converter checks a cancellation event during input pumping, output pumping,
chunk emission and EOF drain. Its output callback independently checks the
generation under the admission lock. This second check matters: cancellation
can happen after conversion's last check but before its callback reaches the
sink. Closing the network socket alone cannot protect buffered converter audio.

Only the owning worker closes its converter's pipes, selector and process;
other threads signal cancellation. Cancelled converters are killed and reaped
without draining their obsolete tail. The first cleanup regression exposed a
two-second wait for FFmpeg's graceful termination while it waited on its pipe;
cancelled teardown now avoids that delay. Normal completion still drains and
verifies every expected output frame. Never use a cancelled converter for the
new position.

A fresh converter acquires real preceding/following source context, anchored to
source frame zero as in the mixed-rate proof. Each intentional seek starts a new
presentation timeline at output frame zero. The selected output rate is retained.
Natural track handoffs continue using the previous uninterrupted output policy.
The native interactive sink recreates its queue at the same selected rate for
intentional seeks; it does not reset that queue at a natural handoff.

## Independent validation

Every generation records native segments, accepted converted PCM, and captured
consumer output. `mixed_check` has an explicit single-track interrupted mode:

- It checks source identity/format, native positions, contiguity and frame counts.
- Lossless native samples must match the source exactly. Lossy alignment uses
  the existing native and independent Opus-clock checks, including the documented
  96 kHz diagnostic limitation. Native ±1-frame estimates remain visible.
- It checks that actual captured output equals the consumed prefix of admitted
  output, with any recorded starvation represented explicitly.
- Python separately converts the admitted native input in one continuous pass
  and requires every admitted output sample to match that reference. Lossless
  runs also match conversion of the full original source at the requested phase.

An interrupted input block may have been decoded/admitted in full but only partly
written through the converter. Its output must still be exactly the corresponding
continuous prefix. The final skip must consume its entire planned output range.
All converters, worker threads, private source sessions and servers must stop.

As before, native capture is PCM submitted before device gain/conversion, not an
acoustic recording. It cannot retract samples already delivered to Bluetooth or
hardware before the reset. Listening remains useful for that boundary.

The negative oracle probe corrupts copies only:

```sh
python3 -B experiments/mixed_seek_oracle_probe.py \
  --run artifacts/mixed-seek-file \
  --checker target/release/examples/mixed_check \
  --out artifacts/mixed-seek-oracle.json
```

It must reject wrong tracks, wrong native positions, a shifted native block,
shifted output, old output substituted coherently in both admission and capture,
and truncated capture/output. The coherent substitutions ensure that merely
comparing the capture to the same corrupted admission file cannot pass.

## Recorded results (2026-10-16)

Ignored evidence is in `artifacts/mixed-seek-proof/`:

| Run | Source rates → output | Result |
| --- | --- | --- |
| `forward-file` | 44.1 → 96 kHz, fixed 48 kHz | Passed |
| `reverse-48` | 96 → 44.1 kHz, fixed 48 kHz | Passed |
| `forward-441` | 44.1 → 96 kHz, fixed 44.1 kHz | Passed |
| `forward-96` | 44.1 → 96 kHz, fixed 96 kHz | Passed |
| `music-muted` | Temporary Patchwork 44.1 → 96 kHz, fixed 48 kHz | Passed, gain zero |
| `music-bluetooth` | Same temporary music pair and output | Passed, gain 0.25 |

All six runs reject both forced late completions, admit/play zero frames for the
superseded startup, and finish the final track excerpt. No resumed generation
starves. Every captured prefix matches; all converters are closed and reaped.
Both full native runs reject the independently injected stale packet and report
zero queue-timeline discontinuities. Separate short muted resets verify 44.1 and
96 kHz native input clocks. The prior 48 kHz seek proof still passes, and two
retained mixed-rate natural-handoff captures pass the extended checker.

The audible Patchwork sequence used the already-authorized temporary 44.1 kHz
Safe Room and 96 kHz What Are The Odds copies. The listener confirmed **“Clean
jumps and correct track change.”** The final excerpt contains 384,004 output
frames. Bluetooth's hardware clock remained 44.1 kHz downstream of the explicit
48 kHz client clock. Original music hashes and temporary-copy hashes are
unchanged. These are pre-gain queue checks, not a bit-perfect hardware claim.
No further playback is authorized by that one listening response.

18 Rust and 66 Python tests pass; Rust formatting, Clippy and release build pass.
Nine new Python regressions cover both admission gates, their locking, native
response identity, cancellation while pumping and draining, prompt process
cleanup and fresh odd-position conversion. Removing each admission guard or the
cancellation check makes its targeted test fail. The seven corrupted oracle
copies are all rejected.

This completes the fixed-output mixed-rate seek/skip experiment. Source cache
reuse across seeks, sustained playback and queue editing, device-rate changes,
bit-perfect negotiation, other formats and Astra integration remain separate.
The prototype's source-session, discovery and authorization limits are unchanged.

The [sustained silent session](session-proof.md) combines these races with
multi-track playback, an outage and measured resource retirement over time.

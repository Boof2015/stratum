# Sustained silent playback session

This combines the [mixed-rate handoff](mixed-rate-proof.md) and
[seek/skip](mixed-seek-proof.md) proofs in one longer client/endpoint/Navidrome
lifetime. It exercises three native rates, scripted quality changes, repeated
position changes and recovery from a real interruption of segment delivery.
It opens no audio device and changes no Astra code or real-server data.

## Reproduce

```sh
cargo build --release --bins --examples
python3 -B experiments/session.py \
  --navidrome /path/to/navidrome \
  --stratum target/release/stratum-poc \
  --checker target/release/examples/mixed_check \
  --source /path/to/fixture-44100.flac \
  --source /path/to/fixture-48000.flac \
  --source /path/to/fixture-96000.flac \
  --rounds 4 \
  --out artifacts/session-proof-run
```

Use three stereo 16/24-bit FLAC or PCM WAV synthetic fixtures, in native-rate
order 44.1/48/96 kHz. Each contains twelve seconds plus an odd tail, shorter than
thirteen seconds. The recorded fixtures were trimmed from the existing native
measurement signals to `12 * sample_rate + 317` frames. This makes actual source
EOF transitions frequent. Do not play those measurement signals aloud.

One to eight rounds are accepted; four is the default. The disposable server
gets an explicit hard lifetime of `60 + 65 * rounds` seconds. Other experiments
keep their existing 180-second default. Request and generation budgets are
bounded too. Each consumer retains its previous 65-second lifetime limit; the
long session changes generations rather than removing that safety bound.

## Each round

1. Queue four source entries: 44.1, 48, 96, then 44.1 kHz again, with fixed
   48 kHz client output. Each track keeps one converter across its segments.
   Segment quality rotates through lossless, Opus 64 and Opus 128. These are
   scripted changes, not an evaluation of automatic quality selection.
2. On the first round, interrupt a real in-flight body after at least eight
   seconds of consumed audio. The segment endpoint is unavailable for twelve
   seconds, aborting that body and returning HTTP 503 to subsequent attempts.
   The outage must outlast the buffer and produce measurable starvation.
3. Require playback to resume from the same media position. No partial body is
   decoded/admitted. The client retries the identical native interval, retains
   converter state, and records every silence quantum while media is stalled.
4. After two real EOF handoffs have played, hold an old converted block and
   seek backward. Hold the new native decode completion, then rapidly skip to
   the 96 kHz track. Release both obsolete completions and require rejection.
   The superseded seek must admit and play zero frames.
5. Play the final track to completion. Retire all three generations' queues,
   workers, converters and source sessions, then begin the next round in the
   same client and server objects.

The long outage is on the **Sidecar-to-client segment hop**. Navidrome remains
running. This does not test upstream outages, expired authorization, changed
source revisions, DNS changes or proxy discovery. It also does not exercise
Astra's UI, output devices or automatic quality modes.

## Retry and retirement rules

The retry helper catches incomplete bodies, connection interruption/timeouts
and temporary HTTP 502/503/504 responses. It waits 250 ms, 500 ms, then at most
one second between attempts, within a twenty-second retry window. Each HTTP
attempt retains the existing five-second socket timeout. Cancellation interrupts
the wait and prevents another attempt. Hash, format and authorization failures
fail rather than enter that loop. No optimistic partial-body admission occurs.

The short proofs retained old state until their fixed five-generation run ended.
For this longer session:

- `FileTimeline.retire` retains capture counters but clears abandoned PCM and
  releases stopped consumer/thread objects. A new generation can start even when
  the previous completed generation is already retired.
- Source sessions belong to `(generation, source track)`. Sharing one session
  between different tracks would violate source identity. Repeated entries of
  the same source within that generation can reuse its authorized byte cache.
- Retirement cancels a generation, waits for its registered requests to finish,
  closes its caches and removes those session objects. Registration checks the
  cancellation flag under the same lock that protects session removal, so a
  late request cannot reopen a retired cache.
- Converter ownership remains with its worker. Both native completion and final
  converted output still pass generation checks. Retired workers are joined and
  removed from active registries rather than accumulated indefinitely.

Per-interval authorization/revalidation still happens when warm source bytes are
reused. The existing Navidrome Last-Modified/source-revision limitation remains;
this experiment does not turn that validator into a cryptographic revision.

## Resource and audio checks

Every half-second the monitor records the Python client/endpoint process RSS,
open descriptors, threads, retained source sessions/cache bytes, queued PCM,
live converters and descendant process names. RSS is for that combined Python
process; it is not total system memory or the sum of Navidrome/FFmpeg RSS.

During playback, explicit ceilings stop runaway resource use: six 8 MiB source
caches, three live converters, less than 25 seconds of queued audio across active
and retiring consumers, 384 MiB Python RSS, 96 descriptors and 32 threads. At every
round boundary the stronger requirement is zero consumers, queued audio, live
converters, source sessions and cached source bytes, with no encoder/decoder
children. The single disposable Navidrome remains alive between rounds.

Across quiet checkpoints, descriptor spread must be at most two and RSS spread
less than 48 MiB. These are bounded prototype checks, not a guarantee against
all leaks or a production memory target. Diagnostics and captured audio are
intentionally retained on disk; this is not a bounded production disk cache.

The independent Rust checker now accepts an explicit interrupted queue of up to
sixteen tracks while preserving the existing two-track natural-EOF mode. It
checks native source identity/PCM/alignment, cumulative rational output-frame
counts and the captured consumed prefix including all reported starvation. An
unfinished output track cannot be followed by output from a later track.
Python separately compares each emitted prefix to continuous conversion of its
native inputs. The existing 96 kHz Opus-clock diagnostic limits still apply.

Negative controls mutate copies:

```sh
python3 -B experiments/session_oracle_probe.py \
  --run artifacts/session-proof-run \
  --checker target/release/examples/mixed_check \
  --out artifacts/session-oracle.json
```

The verifier must reject hidden starvation, a repeated frame after the outage,
a skipped unfinished track, shifted output, and old-track output substituted
coherently into both admission and capture.

## Recorded results (2026-10-16)

The four-round run kept one client, segment endpoint and disposable Navidrome
alive for **180.46 seconds**. It exercised twelve playback generations, eight
natural source-EOF handoffs before the seeks, and **100 scripted quality
changes**. Four stale native completions and four stale converted completions
were rejected. All four superseded seeks admitted and played zero frames.

The twelve-second outage interrupted a partially received body and refused
thirteen subsequent requests. Fourteen bounded retries recovered the same
interval. The file consumer recorded **6.94 seconds of starvation**; every
subsequent media frame matched the correct continuous timeline. All other
generations had zero starvation. All captured prefixes and source/output checks
passed, including the complete final excerpt of every round.

| Resource | Observed peak | After each round |
| --- | --- | --- |
| Python process RSS | 58.27 MiB | 52.55, 54.39, 57.72, 39.58 MiB |
| Open descriptors | 23 | 8 every round |
| Python threads | 10 | 4 every round, including monitor/server/watchdog |
| Source sessions | 4 | 0 |
| Cached source bytes | 10.93 MiB | 0 |
| Live converters | 2 | 0 |
| Buffered audio | 9.15 seconds | 0 |

Twenty source sessions were closed and removed over 113 registered segment
requests. No encoder/decoder child remained at a quiet checkpoint; only the
same disposable Navidrome was still running. Cleanup then stopped that server
and the monitor. This is evidence for this three-minute scenario, not an
hours-long leak claim.

A one-round smoke run also passed (52.40 seconds, 6.80 seconds of expected
starvation). The preceding mixed-rate seek proof was rerun unchanged in scope
and passed. Two retained natural-handoff captures and the earlier seven seek
corruption controls still pass the extended checker. The five new session
corruptions are rejected. Disabling queue clearing, source closing or the final
output guard makes its targeted regression fail.

**18 Rust and 73 Python tests pass**, along with Rust formatting, Clippy and the
release build. Seven new tests cover transient retry, fatal-error rejection,
cancellation/deadline handling, queue retirement, source ownership/retirement and
late converted output. All runs here were silent; no listening run or device
setting change was needed.

Ignored evidence is under `artifacts/session-proof/`: smoke/full runs, the prior
seek regression, resource samples, oracle/guard controls, fixture hashes and
binaries. Source-session limits, production auth/discovery, upstream failures,
queue editing, other formats, native device changes and Astra integration remain
separate work.

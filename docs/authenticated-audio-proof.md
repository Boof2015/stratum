# Authenticated audio delivery checkpoint

The Rust Sidecar service now delivers real FLAC, Opus 64 and Opus 128 segments
through the access boundary proven in [checkpoint A](access-proof.md). The audio
engine and native-frame contract are unchanged. This remains a **loopback-only
experiment**, with Unix process supervision tested on macOS. It is not a deployable
service or an Astra playback integration.

## Reproduce

```sh
cargo build --release --bin sidecar-access
python3 -B experiments/access_proof.py \
  --navidrome /path/to/navidrome \
  --sidecar target/release/sidecar-access \
  --out artifacts/authenticated-audio-proof --audio
```

Choose a fresh output directory. This uses the same disposable two-library,
two-ordinary-user setup as the access proof. `--audio` generates two 60-second
48 kHz stereo PCM WAVs, adds real encoded-media checks, and saves credential-free
reports and encoded synthetic segments. No audio device, user music, Astra state
or existing server is used. Removing `--audio` retains the original prefix proof.

Run the checked-in HTTP/encoder tests with `cargo test -- --test-threads=1`.
They start the actual service binary and actual FFmpeg encoders, with a controlled
HTTP upstream for fault injection. Unix worker supervision uses process groups;
Windows worker containment is not implemented or claimed.

## Source consistency: an evaluated baseline

Opening playback downloads **one complete original HTTP response** into a private
bounded file. It checks the response status, media type, declared/received length,
and FLAC or WAV signature; then the worker verifies a supported source, hashes it,
and decodes the whole file to validate its exact native frame count. Unsupported
sources return `unsupported_source`, rather than passing through a lossy intermediate.

Every representation of that playback uses this same read-only local copy. The
hash identifies the bytes acquired, and is part of every segment's metadata.
There are no separate range reads to assemble an encoded interval. Access probes
still bypass the cache. A same-size upstream replacement cannot change an already
opened playback; a newly opened playback acquires the new bytes. Neither size nor
Navidrome's weak Last-Modified is presented as a content revision.

**This does not prove an atomic upstream snapshot.** A file rewritten in place
while Navidrome is sending the first response could still produce a valid mixed
file. A hash names the received bytes, not a guaranteed historic server version.
The test changes the source *after* acquisition and verifies frozen playback; it
is not a concurrent-write atomicity test.

Full acquisition and validation add startup delay and upstream traffic. This is a
measured integration baseline, **not a decision that Astra must wait for a whole
track in the product**. Strong progressive revisions or operator-controlled source
access remain alternatives. It should not silently replace the earlier efficient
range experiment or the current Astra streaming path.

## Experimental routes

Use the existing session exchange, then send the bearer in `Authorization`.
All routes inherit rejection of query credentials, Origin and Cookie; no-store;
fixed configured upstream; bounded requests; and generic errors without secrets.

| Route | Result |
| --- | --- |
| `POST /audio/v1/playbacks/{track}` | Stage and validate the original; return 201 with a random playback handle, `source`, `sourcePolicy`, `acquiredBytes`, `acquisitionMs`, `readyMs`, `maxSegmentSeconds` |
| `POST /audio/v1/playback/{handle}/segment` | JSON `{start, frames, quality}` using native frames; return metadata and encoded bytes |
| `DELETE /audio/v1/playback/{handle}` | Release the handle, cancel active work and clear its files; idempotent 204 |
| `DELETE /access/v1/session` | Release every playback in that session as well as the prefix cache |

The segment response is `application/vnd.stratum.segment`: a four-byte big-endian
JSON length, UTF-8 `SegmentMetadata` JSON, then exactly `payload_bytes` encoded
bytes. JSON additionally reports `cache_hit`. Check the payload SHA-256 before
decoding. The length prefix keeps metadata and payload together in one authorized
response, without exposing worker paths or public cache URLs. This envelope is
experimental, not a stable wire format.

`lossless` means re-encoded FLAC preserving decoded source PCM, not original file
bytes. Opus uses a 48 kHz codec clock. For native 44.1/96 kHz, convert the decoded
payload back to the native clock and apply the declared trims exactly once.
Container pre-skip is already applied by the decoder. Presentation intervals stay
in native integer frames. Independent encoded files must not be concatenated as
one continuous encoded stream.

The public capabilities still say `playback: false` for the original access proof,
and separately advertise `audioExperiment: stratum-authenticated-audio-1`, source
policy and bounds. There is no Astra-compatible playback capability yet.

## Authorization, cancellation and recovery

Opening playback and requesting a segment both check `getSong` and a real original
stream byte as the signed-in user. They check again after acquisition/encoding,
before releasing the response. This includes both retained source and encoded
cache hits. A playback handle only resolves inside its owning session. Knowing
another user's handle, source hash or track ID grants no access.

Permission loss invalidates the affected playback. A refused credential revokes
the session. A temporary upstream failure blocks delivery with a temporary error;
a later authorized retry can reuse the frozen source and last encoded segment.
An empty/failed response is not EOF and never causes an automatic track skip.

Response bodies yield at most 16 KiB at a time, checking session and playback
liveness between yields. Logout, expiry and explicit playback close cancel work.
Already delivered or socket-buffered bytes cannot be recalled. There is no push
revocation feed or permission guarantee covering indefinitely retained client audio.

Each encoding/validation job is a supervised subprocess using the existing Rust
engine. Only generated local paths enter its arguments; credentials stay in the
service. Workers and FFmpeg descendants share a process group. A dropped request
signals its supervisor, which kills and reaps the worker before releasing permits
and source ownership. Normal shutdown drains supervisors. Encoders are never
abandoned through uncancellable blocking tasks. Staging directories left by an
interrupted encoder are cleared before reuse.

## Prototype bounds

These are explicit experiment limits, not client cache settings or product defaults.

| Resource | Bound |
| --- | --- |
| Original file | 64 MiB, one complete copy per playback |
| Live playback copies | 2 per session, 4 across the service; slots reserved before acquisition |
| Supported input | FLAC or 16/24-bit PCM WAV; 44.1/48/96 kHz; mono/stereo; at most 30 minutes |
| Requested interval | At most 4 seconds in the native clock; exact EOF enforced |
| Encoders/validators | 2 jobs total; one job per playback, with no unbounded wait queue |
| Worker lifetime | 8 seconds; request and socket deadlines from checkpoint A still apply |
| Worker file writes | OS per-file ceiling of 4 MiB, inherited by FFmpeg; bounded stdio monitoring |
| Retained encodes | Only the last interval/quality per playback, plus its delivery copy; payload at most 4 MiB |
| Original HTTP request | Existing 3-second upstream deadline; unsuitable for slow/large upstream acquisition |
| Session / service lifetime | Existing maximum 600 / 3600 seconds |

Original files therefore occupy at most 256 MiB across active and retiring
playbacks. Encoded files and bounded worker scratch space are additional. There
is no cross-user sharing, persistent disk cache, or background pre-encoding of
whole libraries. Admission returns 429 instead of queuing unlimited source/encoder
work. Explicit close, logout, expiry and normal shutdown clean private storage.
A forced service kill or machine crash may leave a private temporary directory;
restart scavenging is not implemented. These are not crash-recovery guarantees.

New failure codes include `unsupported_source` (415), `source_limit_exceeded` (413),
`playback_unavailable` (404), and `audio_preparation_failed` (422). Access and
upstream failures retain checkpoint A's codes. Invalid/oversized intervals are 400.

## Recorded results — 2026-10-17

- **58 real Navidrome 0.64.2 checks pass**, including the previous 28 access checks.
  Actual encoded responses exercise scoped users, private handles, warm caches,
  library revocation, changed passwords, deleted accounts, logout and an upstream
  outage. Changed passwords/account deletion are discovered through the audio
  route itself. A restored permission can prepare a new playback.
- **46 Rust tests pass**: 22 library tests, 12 existing audio tests, 12 authenticated
  HTTP/encoder tests. The latter cover exact lossless PCM, contiguous intervals
  through a short EOF tail, quality changes, native 44.1/48/96 clocks, warm cache
  authorization, final permission loss after encoding, source mutation, invalid
  upstream media, source/session limits, expiry, held acquisition and encoder
  cancellation, global admission, and delayed chunked acquisition.
- **73 Python regression tests pass.** Formatting, Clippy with warnings denied,
  and the release binary/example build pass.
- Three intentionally broken variants were rejected: missing warm-cache access
  checks, missing final authorization, and missing body liveness checks. Sources
  were restored afterward. The cancellation tests check actual child PIDs and
  resource release, not merely a returned status code.

The 96 kHz waveform check retains the earlier proof's distinction: native lossy
correlation may estimate a one-native-frame difference, while the independent
48 kHz Opus-clock comparison must align at zero. Exact lossless PCM and all native
frame counts remain strict. No real alignment failure is waived.

Measured with the release binary, generated 60-second 16-bit stereo WAVs, local
Navidrome and warm OS caches:

| Scoped user | Original acquired | Acquisition | Source ready | First Opus 128 segment received |
| --- | ---: | ---: | ---: | ---: |
| Alice | 11,520,044 bytes | 5.46 ms | 167.23 ms | 336.83 ms |
| Bob | 11,520,044 bytes | 5.67 ms | 152.21 ms | 319.95 ms |

Source-ready timing includes validation, but excludes the initial access check;
first-segment timing is client wall time from opening playback through receiving
its first requested segment. These are single local runs, not WAN, cold-disk or
music-library performance claims. A separate controlled HTTP test sends eight
chunks with 25 ms delays and verifies the full acquisition delay is included;
subsequent quality changes do not download the original again.

The test client decodes/assembles sample intervals for verification; it is not a
real-time audio sink. Earlier device/gapless/seek proofs remain separate. This
checkpoint does not claim a new live gapless listening run through authenticated
HTTP, adaptive policy integration, or Astra Standard playback. Those integrations
come next, along with deliberate source-policy and transport/trust decisions.

# Stratum protocol reference

This describes the current audio contract and its Navidrome-backed HTTPS API in
Astra Sidecar. Start with the walkthrough to build a client; the later sections
define the response format, frame accounting and lifecycle rules.

Jump to: [walkthrough](#client-walkthrough), [connection trust](#discovery-and-trust),
[HTTP API](#http-reference), [segment format](#segment-envelope),
[decoding](#decoding-and-frame-accounting), [errors and limits](#errors-and-current-service-limits).

The protocol is still experimental. This reference was checked against Stratum
`fe7fd2b` and Astra desktop `a509510` on 2026-10-29. The literal identifiers below
are the ones clients must recognize today, including their `proof` and `poc`
names. They are not a promise of a stable version-1 protocol.

Stratum describes independently decodable representations of exact audio
intervals. Sidecar supplies those intervals using a listener's existing Navidrome
access. Library browsing, favorites, playlists and playback reporting remain
with Navidrome; the streaming API does not replace them.

## Client walkthrough

### 1. Establish the Sidecar connection

Obtain an endpoint through [discovery or manual connection](#discovery-and-trust).
Verify its TLS connection before sending a Navidrome credential or Sidecar
bearer. The Sidecar owner password is not a listener credential.

Throughout this document, `BASE` means the selected HTTPS endpoint, without a
trailing slash. Preserve any path prefix. For example:

```text
BASE = https://music.example/sidecar
Capabilities = https://music.example/sidecar/stratum/access/v1/capabilities
```

Every route below is appended to `BASE`. An absolute-path URL resolver that drops
`/sidecar` would contact the wrong route.

Request `GET /stratum/access/v1/capabilities`, without authentication. Check the
identity against the selected connection, the supported protocol identifiers,
and that both `ready` and `playback` are true. Intersect the advertised qualities
with those the client knows how to decode.

### 2. Create a listener session

Generate a fresh salt and compute the Subsonic token:

```text
token = lowercase_hex(MD5(UTF8(password + salt)))
```

Send `POST /stratum/access/v1/sessions` with exactly
`Content-Type: application/json` and this body. Values in angle brackets in all
examples are placeholders, not usable credentials or hashes.

```json
{
  "username": "listener",
  "token": "<32 hexadecimal characters>",
  "salt": "<fresh random salt>"
}
```

Sidecar validates this against its configured Navidrome instance. A successful
response is HTTP 201:

```json
{
  "session": "<64 lowercase hexadecimal characters>",
  "expiresInSeconds": 600
}
```

Use `Authorization: Bearer <session>` on subsequent authenticated requests.
Keep the bearer private. A password-derived token is also credential material,
even though it does not contain the plaintext password.

### 3. Open a track

Use the track's Navidrome song ID, not a filesystem path or streaming URL:

```text
POST /stratum/audio/v1/playbacks/{trackId}
Authorization: Bearer <session>
```

No body is required. HTTP 201 returns a session-owned playback handle and the
identity of the acquired source. This illustrative response describes 60 seconds
of stereo 44.1 kHz audio:

```json
{
  "playback": "<64 lowercase hexadecimal characters>",
  "source": {
    "sha256": "<64 lowercase hexadecimal characters>",
    "codec": "flac",
    "sample_rate": 44100,
    "channels": 2,
    "bits_per_sample": 16,
    "frames": 2646000,
    "bytes": 5000000
  },
  "sourcePolicy": "complete-private-copy",
  "acquiredBytes": 5000000,
  "acquisitionMs": 120.5,
  "readyMs": 180.2,
  "maxSegmentSeconds": 4
}
```

Sidecar has fetched the complete original from Navidrome before this response.
The client has received only metadata, not the original audio. All subsequent
representations for this handle use that same private source copy.

Validate the source format and limits before allocating a decoder. The source
hash names the acquired file bytes, not a track title or a PCM-only fingerprint.

### 4. Request and decode an interval

For the first four seconds of that source:

```text
POST /stratum/audio/v1/playback/{playback}/segment
Authorization: Bearer <session>
Content-Type: application/json
```

```json
{"start": 0, "frames": 176400, "quality": "lossless"}
```

The response contains a length-prefixed JSON header followed by one FLAC or Ogg
Opus payload. [Validate the envelope](#segment-envelope), decode the independent
payload, and [apply its frame contract](#decoding-and-frame-accounting). Only the
requested presentation interval enters the playback queue.

Request the next interval before the current one is consumed. There is no
server-driven playlist of segment URLs and no stream of unsolicited segments.

### 5. Change quality without replacing earlier intervals

The next request can use another advertised quality:

```json
{"start": 176400, "frames": 176400, "quality": "opus-128"}
```

Then, for example:

```json
{"start": 352800, "frames": 176400, "quality": "lossless"}
```

These three requests cover `[0, 529200)` native frames in order. Earlier
downloaded intervals remain usable. No session restart, new playback handle or
redownload of those intervals is required by a quality change.

Join the decoded presentation intervals, not the encoded files. Do not overlap
their encoder context, apply a crossfade to conceal a boundary, or insert codec
delay as silence. Keep any downstream output resampler running across segment
and quality boundaries.

### 6. Renew, seek and close

Before the session expires, send fresh same-user credentials to
`POST /stratum/access/v1/session` with the existing bearer. Renewal returns HTTP
200 with the **same bearer**, a new lifetime, and all playback handles intact.

A seek is another native-frame interval request; there is no seek endpoint.
Stop obsolete work and prevent late results from entering the new playback
queue. For a next-track handoff, open a second track and prepare its beginning
while the first finishes. Each track has its own source clock starting at zero.

Release a finished or skipped track with
`DELETE /stratum/audio/v1/playback/{playback}`. Release the session with
`DELETE /stratum/access/v1/session`. Both return HTTP 204 when successful.
Session deletion also cancels its playbacks.

## Discovery and trust

The installed service uses HTTPS. The loopback `sidecar-access` proof uses a
different transport and unprefixed routes; it is not a remote deployment model.

### Navidrome discovery

The owner can publish a special internet-radio entry using temporary Navidrome
administrator authorization. A listener reads it through Navidrome's
authenticated `getInternetRadioStations` API. Its descriptor is public to those
listeners and contains no password or bearer:

```json
{
  "version": 1,
  "kind": "astra-sidecar",
  "installation": "<32 hexadecimal characters>",
  "spkiSha256": "<64 hexadecimal characters>",
  "endpoints": [
    {"url": "https://sidecar.home.lan:7843", "trust": "sidecar_key"},
    {"url": "https://music.example/sidecar", "trust": "https_proxy"}
  ]
}
```

The station's `streamUrl` is the first endpoint plus
`/stratum/access/v1/capabilities`. The returned `homePageUrl` is that same URL
followed by `#stratum-v1=` and percent-encoded UTF-8 JSON. The station name is not
its identity. The fragment is metadata; it is not sent in HTTP requests.

The current format permits 1–16 distinct canonical HTTPS endpoint bases, each
at most 1,024 characters, without credentials, query strings or fragments.
Remove trailing slashes when configuring an endpoint. Astra bounds the complete
announcement URL to 16 KiB and rejects malformed marked entries, inconsistent
primary URLs and multiple announcements rather than picking one arbitrarily.

| Trust mode | Verification before credentials |
| --- | --- |
| `sidecar_key` | SHA-256 of the TLS peer's DER SubjectPublicKeyInfo must equal `spkiSha256`. The connection is authenticated by that key, allowing Sidecar's locally issued certificate. |
| `https_proxy` | Normal certificate-chain and hostname verification for the endpoint. The owner has designated this proxy to forward to Sidecar. |

`spkiSha256` is a **public-key fingerprint**, not the fingerprint of the whole
certificate. A proxy's TLS key need not equal Sidecar's key; its authenticated
capabilities still identify the expected Sidecar installation and key.

Verify on the actual TLS connection that will carry credentials. A public JSON
field, or proof received on an unrelated connection, cannot replace that check.
Do not turn a failed direct-key check into an automatic proxy-trust fallback.
Redirects must not forward credentials to another address.

Discovery inherits the trust of the Navidrome connection the user configured,
including an intentionally configured HTTP connection. It is not an independent
ownership proof beyond that connection. Nor does discovery grant music access.

### Manual connection

A client can accept a Sidecar HTTPS address without a radio announcement. It
still needs an approved connection before sending listener credentials.

Astra uses normal HTTPS verification for a valid certificate. For an eligible
locally issued certificate, it inspects the public endpoint without credentials
and asks the user to approve its identity key. It verifies that same key again
before sign-in. Existing approval of the same connection can be reused; changed
keys require a new trust decision. Manual approval is scoped to the selected
Navidrome connection, not made a system-wide certificate exception.

The trust prompt and saved-connection UI are client policy. The ordering—verify
the endpoint, then send credentials—is part of the connection contract.
See [owner discovery](owner-discovery.md) for publication and recovery behavior.

## HTTP reference

The installed listener currently speaks HTTP/1.1 over HTTPS. Reuse verified
connections where possible; repeated handshakes can dominate a short segment's
delivery time. Connection reuse does not extend a session's lifetime.

Listener routes reject **any query string**, `Origin` header or `Cookie` header.
They are native-client routes, not a browser-cookie API. JSON request bodies use
exactly `Content-Type: application/json`; the current parser does not accept a
charset suffix. Credentials and segment bodies are limited to 4 KiB and reject
unknown fields. Do not send credentials in URLs.

Responses carry `Cache-Control: no-store` and `X-Content-Type-Options: nosniff`.
Non-success responses close the connection. Application-managed audio retention
is separate from an HTTP intermediary cache; see [caching](#caching-and-access).
Clients should bound every response and accept only the expected success status
and content type. A proxy may return an HTML error instead of Sidecar JSON.

### Capabilities

`GET /stratum/access/v1/capabilities` returns HTTP 200 JSON; `HEAD` is also
supported. It is available before owner setup completes.

| Field | Current value / meaning |
| --- | --- |
| `service` | `astra-sidecar` |
| `installation` | Persisted installation ID, 32 lowercase hex characters |
| `spkiSha256` | Persisted Sidecar public-key fingerprint, 64 lowercase hex characters |
| `protocol` | `stratum-access-proof-1` |
| `audioExperiment` | `stratum-authenticated-audio-1` |
| `authentication` | `subsonic-token` |
| `sessionRenewal` | `fresh-credentials-same-user` |
| `development` | `true` |
| `ready`, `playback` | Both true when a configured listener engine is available; not proof that a particular user or track is accessible |
| `qualities` | `lossless`, `opus-64`, `opus-128`, `opus-192`, `opus-256`, `opus-320` |
| `audioSource` | `complete-private-copy` |
| `maxSourceBytes` | `2147483648` (2 GiB) |
| `maxSourceSeconds` | `28800` (eight hours) |
| `maxSegmentSeconds` | `4` |
| `probeBytes` | `64`, for the legacy access-proof route; not an audio segment size |

Capability negotiation is required: a client should not infer the quality
ladder from the protocol identifier alone. Astra ignores unknown quality names
and uses the supported intersection. For older services that omit `qualities`,
its compatibility rule is `lossless`, `opus-64`, `opus-128`. An empty or malformed
list is not equivalent to an omitted field.

The legacy authenticated `GET /stratum/access/v1/media/{trackId}` route returns
only a 64-byte access probe. Clients should use the audio routes for playback.

### Sessions

| Method and route | Authentication | Success |
| --- | --- | --- |
| `POST /stratum/access/v1/sessions` | Credential JSON | 201, session and lifetime |
| `POST /stratum/access/v1/session` | Bearer and fresh credential JSON | 200, same session and renewed lifetime |
| `DELETE /stratum/access/v1/session` | Bearer | 204 |

Credential fields are `username`, `token` and `salt`. The username must be
nonempty, at most 128 UTF-8 bytes, without surrounding whitespace or control
characters. The token is exactly 32 ASCII hex characters. The salt is nonempty,
at most 128 UTF-8 bytes, with no control characters. Astra generates 16 random
bytes encoded as hex for each exchange's salt.

Send exactly one `Authorization` header, with the literal prefix `Bearer ` and
the returned 64-character bearer unchanged. Renewal must use the same username.
A bearer alone cannot extend the session. Sidecar verifies credentials again
and replaces the in-memory credential after a successful renewal. It also tests
that Navidrome refuses a bogus sign-in, rather than accepting an upstream that
has authentication bypassed for Sidecar's address.

The current lifetime is 600 seconds. Session expiry, explicit logout and
authentication revocation cancel the session's playbacks. A rejected renewal
due to sign-in/access refusal revokes the session; a temporary upstream failure
does not extend its deadline. A new session cannot recover the old session's
handles. Sessions and handles do not survive a Sidecar restart or replacement
of its configured listener engine.

Use a monotonic deadline, with allowance for request time. Astra anchors it to
the exchange's start and renews around half the returned lifetime; that timing
is its policy, not an additional wire requirement.

### Playback handles and segment requests

| Method and route | Success |
| --- | --- |
| `POST /stratum/audio/v1/playbacks/{trackId}` | 201, playback and source metadata |
| `POST /stratum/audio/v1/playback/{handle}/segment` | 200, binary segment envelope |
| `DELETE /stratum/audio/v1/playback/{handle}` | 204, including an unknown handle within a valid session |

All three require a bearer. A track ID is 1–128 ASCII letters, digits, `_` or `-`.
Playback handles are opaque, random 64-character lowercase hex values, scoped
to the creating session. An open is not idempotent: repeating it can allocate
another handle.

Opening accepts optional `X-Stratum-Priority: playback` or `prefetch`, defaulting
to `playback`. The segment request body is:

```json
{"start": 176400, "frames": 176400, "quality": "opus-128", "priority": "playback"}
```

`priority` is optional and defaults to `playback`; `prefetch` is the other value.
It affects server work scheduling, not the audio identity or presentation time.
It does not guarantee a completion deadline. The quality must be supported.

`start` and `frames` are integer **source-rate frames**. A frame contains one
sample per channel. `start` is zero-based, `frames` is positive, and
`start + frames` must not exceed `source.frames`. A request can cover at most
`source.sample_rate * maxSegmentSeconds` frames. Intervals need not start on
four-second boundaries; the final interval is usually shorter.

The source response fields are shown in the walkthrough. `source.codec` is
`flac`, `pcm_s16le` or `pcm_s24le`; `sample_rate` is 44100, 48000 or 96000;
`channels` is 1 or 2; `bits_per_sample` is 16 or 24. `frames` is the exact native
duration and `bytes` the acquired file size. `acquisitionMs` measures source
fetching; `readyMs` includes preparation and final access checks after admission.
Neither measures delivery to the client or time until audible playback.

An open validates the initial interval, not the entire track's decoded audio.
Later damage can still fail a segment. This implementation requires at least
16 source frames; exceptionally short rate-converted Opus sources may fail
because there is insufficient real filter context. It does not pad them with
invented silence.

## Segment envelope

Successful segment responses use `Content-Type: application/vnd.stratum.segment`:

```text
4 bytes       unsigned big-endian length N
N bytes       UTF-8 JSON metadata
remaining     complete encoded audio payload
```

There is no magic prefix before the length and no trailing second payload. Bound
the metadata before parsing it. Astra accepts at most 16 KiB of metadata and
4 MiB of payload. Reject truncated data, a mismatched payload length or hash,
and metadata that disagrees with the request or opened source. Apply the same
validation to locally cached envelopes.

| Metadata field | Meaning |
| --- | --- |
| `experiment` | Exactly `stratum-audio-poc-2` |
| `source_sha256` | Acquired source hash; must match the open response |
| `encoder` | 64-character hex identity of the encoder configuration/tooling; cache identity, not an executable or URL |
| `quality` | Requested representation |
| `sample_rate`, `channels` | Native source clock and channel count |
| `codec_sample_rate` | Native rate for lossless; 48000 for Opus |
| `codec_decoded_frames` | Frames at the codec clock after container pre-skip/end trim, before native-rate conversion |
| `presentation` | `{start, frames}` requested by the client, at the native clock |
| `encoded_interval` | `{start, frames}` of the source used for encoding, including any context |
| `decoded_frames` | Native frames after decoding and any codec-to-native rate conversion, before presentation trimming |
| `discard_start_frames`, `discard_end_frames` | Native frames to discard from that decoded result |
| `resample_tail_frames` | Measured extra native frames from rate-conversion rounding, currently 0 or 1 |
| `codec_preskip_frames` | Opus container pre-skip at the codec clock; 0 for lossless |
| `source_bytes_preserved` | `false` for every current representation |
| `source_pcm_preserved` | `true` for lossless, `false` for Opus |
| `payload_sha256`, `payload_bytes` | SHA-256 of the encoded payload and its exact byte length |
| `cache_hit` | Server cache diagnostic; may describe reuse inside the playback worker or retained cache |
| `retained_cache_hit` | Optional server retained-cache diagnostic; `true` is added on a retained hit. Absence means unspecified, not a confirmed miss. |

Hashes are lowercase hexadecimal. The payload is an independent FLAC file for
`lossless`, or an Ogg Opus file for an `opus-*` quality. Opus names indicate the
configured audio bitrate in kbps. Context, Ogg framing, metadata and HTTP/TLS add
bytes, so they are not network bandwidth ceilings.

## Decoding and frame accounting

Every representation describes the same native source timeline. A 44.1 kHz
track still uses 44,100-frame seconds when its payload is Opus. The payload's
48 kHz clock is an intermediate decoding clock, not a new seek timeline.

Decode each payload independently:

1. Decode the FLAC or Ogg Opus container. Let the container decoder apply Opus
   pre-skip and end trimming **once**. Check the resulting codec frame count.
2. If the codec and native rates differ, convert that decoded interval to the
   native sample rate. Check against `decoded_frames`.
3. Discard `discard_start_frames` from the front and `discard_end_frames` from
   the end. Enqueue exactly `presentation.frames` native frames.
4. Feed those contiguous presentation frames into the client's output pipeline.
   Conversion to the hardware/output rate is a separate stage.

Do not subtract `codec_preskip_frames` again after a decoder has already applied
it. The discard counts describe PCM **after** that work and native conversion.

The frame identities are:

```text
presentation.end = presentation.start + presentation.frames
encoded.end = encoded_interval.start + encoded_interval.frames

discard_start_frames = presentation.start - encoded_interval.start
decoded_frames = encoded_interval.frames + resample_tail_frames
discard_start_frames + presentation.frames + discard_end_frames = decoded_frames
```

The encoded interval must contain the presentation interval and remain inside
the source. Native conversion can produce the declared extra frame; it must not
be allowed to accumulate as an extra played frame at every boundary.

Current Opus encoding includes about 120 ms of real source context on either
side, clamped at the track edges. Its start is aligned to the source-zero
resampling phase: multiples of 147 native frames at 44.1 kHz, or 2 at 96 kHz.
Alignment can slightly increase the leading context. Lossless normally encodes
only the presentation interval; requests shorter than FLAC's 16-frame minimum
also use real neighboring context. **Read the returned counts; do not hard-code
120 ms as the number to discard.**

Sidecar and Astra currently use FFmpeg's `swr` resampler for codec/native
conversion, with `exact_rational=1`, `async=0`, and `dither_method=none`. Sidecar
measures the native decoded count before returning the metadata. Another decoder
or resampler must meet the declared count and boundary behavior; adding or
dropping samples to disguise a mismatch is not a compatible implementation.

For a concrete 44.1 kHz example, an interior Opus request starting at frame
176400 for 176400 frames uses context `[171108, 358092)`. That is 186984 native
frames, 203520 codec frames, and 5292 native context frames at each end. After
container trimming and native conversion, discarding those two contexts leaves
exactly the requested four seconds. Fractional endings can instead carry a
declared one-frame tail; use the metadata in that case.

Lossless preserves the source's decoded integer PCM for the presentation
interval. It re-encodes those samples as FLAC, so it does not preserve original
file bytes, tags or container structure. Opus is lossy, including when converted
back to a 96 kHz native timeline. An output mode promising bit-perfect playback
must keep lossless and avoid sample-changing output processing.

## Playback lifecycle and cancellation

A playback handle pins an immutable acquired source. Replacing the upstream
file does not change that handle's representations; opening again acquires and
hashes the new bytes. A hash identifies what was received, not a guarantee that
Navidrome served an atomic historic snapshot during an in-place file rewrite.

There is no server playback-position command. The client owns ordering,
buffering, audible quality readouts and timing. It must discard late responses
from obsolete seeks or skipped tracks. Session and playback deletion cancel
server work; cancellation is also checked between response-body chunks. Bytes
already delivered or queued in a socket cannot be recalled.

Only one segment operation at a time is admitted per handle; overlapping work
can return 429. Cancelling a download does not promise that its server work has
already released its slot. Keep retries bounded. On a shared client download,
one reader leaving must not cancel another reader's work; cancel the underlying
transfer when its final reader leaves. That sharing behavior is a client concern.

Take care with cancellation during an open: once the handle has been allocated,
discarding its response can lose the identifier needed to close it. Astra lets
an abandoned open finish within its bounded deadline, then closes the returned
handle before preparing its replacement. If the response is lost entirely, the
session lifetime still bounds the orphan; deleting that session also releases it.

Gapless track handoff requires the client to prepare the next track in time and
join its decoded PCM through the output pipeline. A segment request cannot span
two tracks. Source-rate changes, output-device constraints and output-mode policy
remain the client's responsibility. The protocol supplies exact intervals; it
cannot guarantee a gapless transition when the next audio has not arrived.

## Caching and access

There are three distinct stores:

| Store | Purpose |
| --- | --- |
| Sidecar working storage | Private source copies and temporary encoding work for active playback handles; required even with retained caching off |
| Sidecar retained cache | Completed encoded segment envelopes reused across playbacks; saves encoding work, not the current full-source acquisition on open |
| Client cache | Downloaded, validated intervals available without downloading those bytes again; its retention and offline policy belong to the client |

Sidecar uses the listener's current Navidrome credentials to check song metadata
and access to raw audio before serving it. Access checks run before cache lookup
or encoding and again before delivery. A retained-cache hit is not permission
to read music. Authentication refusal revokes the session; lost track access or
an encoding failure during a segment invalidates that playback. A temporary
upstream outage leaves the frozen source available for a later authorized retry.

Do not identify cached audio by track ID alone. Include the appropriate server
and account scope, source revision/hash, native format, interval and
representation. Server retained entries also incorporate encoder identity.
Astra scopes its local intervals to the approved account/installation and source
revision; it does not persist live playback handles. A partial HTTP response is
not a validated segment. Different qualities of one interval remain distinct.

Sidecar cannot revoke bytes a client already downloaded. Client offline access
and cache invalidation on account changes must be designed explicitly; this API
does not provide DRM or a remote erasure mechanism. Server cache diagnostics also
cannot tell a client that bytes are present in its own cache or playback buffer.

## Errors and current service limits

Application errors use an HTTP status and small JSON body, for example:

```json
{"error": "track_unavailable"}
```

| HTTP | Code | Meaning |
| --- | --- | --- |
| 400 | `invalid_request` | Invalid fields, interval, headers or request shape |
| 401 | `sign_in_required` | Missing, expired or refused listener session/credentials |
| 404 | `track_unavailable` | Track inaccessible or no longer available to this listener |
| 404 | `playback_unavailable` | Handle absent, cancelled or not owned by this session |
| 413 | `source_limit_exceeded` | Original file exceeds the byte ceiling |
| 415 | `unsupported_source` | Unsupported source format or duration |
| 422 | `audio_preparation_failed` | Segment encoding/decoding validation failed |
| 429 | `limit_reached` | Admission or resource concurrency limit |
| 500 | `internal_error` | Internal preparation/storage operation failed |
| 502 | `upstream_response_invalid` | Navidrome answered with an unusable response |
| 502 | `upstream_does_not_validate_credentials` | Navidrome accepted the bogus-credential check |
| 503 | `upstream_unavailable` | Navidrome unreachable or an access-engine request timed out |
| 503 | `working_storage_unavailable` | Insufficient working budget or disk space |
| 503 | `configuration_unavailable` | Installed listener could not load/apply its configuration |
| 503 | `setup_required` | Listener engine not configured |
| 503 | `service_stopping` | Installed listener is shutting down |
| 503 | `request_timed_out` | Installed listener's outer request deadline elapsed |

An unsupported method or a proxy failure may produce a framework/proxy response
outside this table. Treat the status and bounded parsed fields as input; do not
show arbitrary server HTML or credential-bearing response text to a user.

Changing quality does not fix authentication, permission, missing-handle, source
limit or format refusals. Do not walk the quality ladder retrying those errors.
Encoding failure invalidates the handle too. Transient failures can be retried
within a playback deadline, with backoff for resource pressure. Access-engine
429 and working-storage responses include `Retry-After: 1`; the installed
listener's outer admission rejection need not include that header.

These are **current Sidecar implementation limits**, not universal Stratum
requirements. Read advertised limits where available and handle resource refusal:

| Resource | Current limit |
| --- | --- |
| Source file / duration | 2 GiB / eight hours, also constrained by working storage and preparation deadlines |
| Presentation interval | Four seconds of native frames |
| Encoded payload | 4 MiB |
| Active prepared tracks | Four globally, two per session; admission includes tracks still being prepared |
| Listener sessions | 32; 600-second renewable lifetime |
| Session creation/renewal attempts | 30 per minute per service instance |
| Admitted listener requests | Eight concurrently |
| Conversion workers | Owner-configurable automatic or 1–4; one segment operation per playback |
| Working storage | Default 512 MiB; admission reserves source size plus working allowance |
| Handler budget | 25 seconds for playback open, 10 seconds for other requests |
| Original acquisition | 15-second upstream request limit, three-second body-idle limit |
| Encoding worker | Eight-second execution limit |
| HTTPS transport | 64 connections, ten-second TLS/header-read limits, 60-second total socket lifetime |

Handler budgets do not measure the full time until a response body reaches the
client. Client deadlines must account for network delivery as well. Retire pooled
sockets in time for subsequent requests; a long-lived socket can reach the
server's lifetime limit even when it has remained busy.

## What belongs to the client

The wire contract does not choose Automatic versus Original, a startup quality,
upgrade thresholds, prefetch depth or the size of the audible buffer. Astra's
controller uses delivery measurements and playback deadlines; another client
can make different choices while producing the same contiguous timeline.

Useful implementation lessons from Astra's integration:

- Measure time until an interval is usable, not just payload transfer speed.
  Authentication checks, server preparation and TLS setup can dominate it.
- Keep current playback ahead of next-track preparation and artwork traffic.
- Cancel an obsolete slow quality attempt when a cheaper interval can still
  arrive in time. Avoid stacking transport retries under controller retries.
- Play usable cached audio when an upgrade would miss its deadline. A cache
  upgrade should not introduce buffering that using the cache would avoid.
- Continue to assess recovery after a connection improves. A drop to a lower
  quality is not a reason to remain there indefinitely.
- Label the quality of audio being consumed, rather than merely the most recent
  request. Downloaded, decoded/prepared, locally cached and played are distinct.

For a new implementation, verify exact frame accounting at 44.1, 48 and 96 kHz,
first and final intervals, quality boundaries, seeks and mixed-rate track
handoffs. Then test cancellation, expiry/renewal, access loss on a warm cache,
corrupt envelopes and a connection that slows and later recovers. Listening
checks complement frame and lifecycle tests; they do not replace them.

## Implementation and experiment references

The current server sources behind this reference are:

- [Installed HTTPS listener](../src/owner/listener.rs) and [owner service](../src/owner/mod.rs)
- [Authentication, access checks and errors](../src/access/mod.rs)
- [Playback and segment routes](../src/access/audio/mod.rs)
- [Encoder and metadata](../src/media.rs), [native timeline](../src/timeline.rs), and [worker validation](../src/access/audio/worker.rs)
- [Discovery publication](../src/owner/discovery.rs)

In Astra, the corresponding client files are
`src/main/services/stratumDiscovery.ts`, `stratumManual.ts`, `stratumTransport.ts`,
`stratumProofSession.ts`, `stratumProofClient.ts`, and `stratumProofPlayback.ts`.
The `Proof` names are still used by the integrated client. Quality selection,
fetch-ahead and local retention live separately in `stratumQuality.ts`,
`stratumFetchAhead.ts` and `stratumAudioCache.ts`.

Earlier reports record individual experiments and their limits:
[audio decoding](audio-proof.md), [authenticated audio](authenticated-audio-proof.md),
[session renewal](session-renewal-proof.md), [gapless handoffs](handoff-proof.md),
and [mixed source rates](mixed-rate-proof.md). Their checkpoint descriptions can
predate the installed service, expanded ladder and current limits. Use this
reference and the current implementation when building a client.

# Sidecar HTTPS streaming integration

Checkpoint 4a connects the existing authenticated audio engine to the persistent
`sidecar` binary. Owner enrollment, saved connections and discovery publication
now lead to an actual HTTPS audio endpoint. Checkpoint 4b adds
[retained encoded segments and owner cache controls](owner-cache.md), and 4c
adds [conversion limits and working-storage recovery](owner-resources.md).
The [live owner dashboard](owner-dashboard.md) reports activity and resource use.
This is a development server. [Docker and standalone Linux packages](install.md)
are available to build; the production Astra discovery/enable flow is subsequent work.

Build `sidecar` with the repository's Rust toolchain. Audio requires `ffmpeg` and
`ffprobe` on PATH, with FFmpeg's `libopus` encoder. The service/worker implementation
currently supports Unix (tested on macOS and Linux); Windows process/filesystem support is
not implemented.

## Activation and routes

Follow [owner setup](owner-foundation.md) and [connection setup](owner-setup.md).
Once an owner password, a Navidrome URL and at least one listener address are
saved, the running service accepts listener sessions. Publishing discovery is
optional and does not affect permission to use the service. No additional
Navidrome administrator account is retained.

On the same HTTPS port as the owner interface:

| Route | Purpose |
| --- | --- |
| `GET /stratum/access/v1/capabilities` | Public installation/SPKI identity, setup readiness and development limits |
| `POST /stratum/access/v1/sessions` | Validate a listener's Navidrome username/token/salt and issue a session |
| `POST /stratum/access/v1/session` | Renew with fresh credentials for the same account |
| `DELETE /stratum/access/v1/session` | End this listener session and its playback work |
| `POST /stratum/audio/v1/playbacks/{track}` | Acquire and inspect a complete original using this listener's access |
| `POST /stratum/audio/v1/playback/{handle}/segment` | Deliver an encoded, sample-addressed interval |
| `DELETE /stratum/audio/v1/playback/{handle}` | Cancel a playback and release its working files |

The old 64-byte `/stratum/access/v1/media/{track}` probe also remains available for
regression checks. Audio request/response bodies retain the experimental contract
in [authenticated audio](authenticated-audio-proof.md); this is not a new stable
protocol version. Capabilities carry `development: true`. `ready`/`playback` mean
the saved setup permits audio requests, not that Navidrome, FFmpeg or every
advertised address has passed a current health check.

Capabilities also advertise Original and Opus 64/128/192/256/320 through the
[`qualities` field](authenticated-audio-proof.md#quality-negotiation). The higher
bitrates require this Sidecar update; older installations retain their original
three representations with newer Astra clients.

Listener URLs are **bases**: the published capabilities URL appends
`/stratum/access/v1/capabilities`. For a proxy base `https://example.test/audio`,
forward `/audio/stratum/...` to Sidecar's `/stratum/...`, stripping `/audio`.
Preserve methods and bodies. Do not inject an owner cookie or `Origin` into the
listener API. The admin interface remains rooted at `/`; its configured Host and
Origin allowlist is separate. Saving a listener address does not configure a
proxy, DNS, bound port or certificate.

Clients must establish trust before sending credentials: verify the advertised
SPKI for direct connections, or the HTTPS certificate/hostname for an explicitly
trusted proxy endpoint. Public capability fields alone prove no ownership.
The real-server experiment trusts only its temporary installation's certificate;
it never disables TLS verification. Production Astra trust/selection UI remains
to be connected. Existing loopback Astra/proof paths are unchanged.

## Sessions, configuration changes and cleanup

Owner cookies never authorize music. The listener API rejects ambient browser
cookies, `Origin`, and query strings. It accepts explicit bearer headers and
bounded JSON credential requests. Navidrome credentials stay in memory only;
owner passwords and publication credentials do not become listener credentials.

Upstream requests use the saved URL, including its path prefix, with normal
certificate validation for HTTPS. Deliberately configured HTTP is supported.
The client cannot choose an upstream. Redirects and ambient proxies are disabled.
The existing checks for permissive upstream authentication, scoped track access,
real original-stream access and final delivery all remain in force.

Each saved connection revision revokes existing listener sessions and playbacks,
including an edit to listener addresses or a CLI `configure` call. Reconnect with
a fresh listener sign-in. This also prevents A → B → A configuration changes
from reviving a previous session. No credential from an old session is forwarded
to the new Navidrome. Requests check the saved configuration before dispatch;
idle maintenance checks about once per second. A corrupt/unreadable configuration
stops the old listener engine until settings can be read again.

The revision is not a distributed transaction: an upstream request already sent
may complete, and client-buffered audio cannot be recalled. Cancellation prevents
further delivery and disposes of pending work. Restart preserves installation
identity/configuration, but never listener sessions. Shutdown cancels sessions,
allows up to five seconds for HTTP shutdown, and waits for encoder cleanup.

Owner password hashing and discovery's durable writes run off the audio runtime
threads. Resource permits are shared across engine replacements, so changing
settings does not create another independent encoder/source allowance.

## Current limits

### Reusing track preparation

Opening a playback acquires one complete private original and computes its SHA-256
while writing it. It then probes the read-only file, validates its first four
seconds (or the whole file when shorter), and identifies FFmpeg once. Subsequent
segment workers reuse that preparation rather than reading the file again to hash
it or running `ffmpeg -version` for every four-second interval. Each requested
interval still has exact frame validation; damaged audio later in a recording
fails when that interval is requested, rather than delaying startup to decode
the entire track. No silence is substituted for a failed interval.

The preparation belongs to that playback and is passed only through its private
worker job. It is never accepted from a listener or reused across newly acquired
sources. On Unix, file checks include device/inode, size, modification and change
times, and permissions. Changing content while restoring the modification time,
replacing the file, or making the source writable invalidates preparation. The
encoder is bound to the resolved executable and checked before and after use;
replacing it requires a new playback. Segment encoding and frame validation use
that same executable. Full content checks are retained on platforms without the
Unix change clock; Unix remains the supported service platform.

Segment hashes, decode/frame validation, cache repair, per-listener access checks,
worker deadlines, admission and cancellation remain unchanged. Preparation reuse
does not introduce parallel requests or additional conversion workers. A retained
segment can still be served without taking an encoder slot.

### Resource bounds

These development bounds carry forward the proofs, with configurable resources
added in checkpoint 4c. They are not a production capacity guarantee:

- One complete private original per playback, up to 2 GiB and eight hours.
  Supported sources: mono/stereo 16/24-bit FLAC/WAV, 44.1/48/96 kHz.
- Four staged playbacks globally, two per listener session. Conversion concurrency
  is Automatic or a custom 1–4; the loopback proof retains two workers.
  Up to four source-seconds per segment, 4 MiB encoded payload, eight-second worker
  deadline. Serving starts after complete source acquisition and prefix validation.
- Ten-minute sessions, at most 32; renewal uses freshly validated same-account
  credentials. Request admission is bounded to eight and login attempts to 30/minute.
- Complete-original transfers have a 15-second total deadline and a three-second
  body-idle limit. Opening playback has a 25-second handler deadline; other
  upstream requests and listener handlers retain three- and ten-second limits.
  A source below the byte/duration ceiling can still exceed these deadlines on
  a slow Sidecar-to-Navidrome connection. Playback before full acquisition remains
  separate work; listener bandwidth does not set this upstream transfer speed.
- At most 64 accepted HTTPS connections; ten-second TLS handshake/header bounds
  and a 60-second total socket lifetime, including slow response readers.
- Private temporary working directories, with only the most recent encoded
  interval retained per playback. A separate retained cache reuses encoded
  intervals between plays; see its [controls and recovery rules](owner-cache.md).
  New playback acquisition still fetches and hashes the complete original.
  Working files live under the installation, with a separate budget, free-space
  guard and crash recovery; see [resource controls](owner-resources.md).

## Verification

`cargo test` runs the installed binary over verified TLS against a controlled
upstream and real encoders, checking exact lossless PCM, aligned Opus intervals,
scoped access, revocation, configuration cancellation and encoder shutdown.
Unit tests cover setup gating, public/admin route separation, A → B → A changes,
idle maintenance, configuration failures and socket bounds.

For a disposable real Navidrome (no existing server URL is accepted):

```sh
python3 experiments/owner_streaming.py \
  --navidrome /path/to/navidrome \
  --sidecar target/debug/sidecar
```

Requires Python with TLS 1.3 support. It creates two scoped ordinary users and
synthetic WAVs, publishes discovery, verifies the matching HTTPS identity, checks
lossless/Opus delivery and permission changes, and restarts the same installation.
All children have hard deadlines and are stopped; no audio device is used.

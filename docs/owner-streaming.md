# Sidecar HTTPS streaming integration

Checkpoint 4a connects the existing authenticated audio engine to the persistent
`sidecar` binary. Owner enrollment, saved connections and discovery publication
now lead to an actual HTTPS audio endpoint. This is a development server:
retained caching, configurable resource limits, deployment packaging and the
production Astra discovery/enable flow are still subsequent work.

Build `sidecar` with the repository's Rust toolchain. Audio requires `ffmpeg` and
`ffprobe` on PATH, with FFmpeg's `libopus` encoder. The service/worker implementation
currently supports Unix (tested on macOS); Windows process/filesystem support is
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

These are carried forward from the proofs to bound this integration. They are
**not the final owner-facing defaults or storage policy**:

- One complete private original per playback, up to 64 MiB and 30 minutes.
  Supported sources: mono/stereo 16/24-bit FLAC/WAV, 44.1/48/96 kHz.
- Four staged playbacks globally, two per listener session; two encoder workers.
  Up to four source-seconds per segment, 4 MiB encoded payload, eight-second worker
  deadline. Serving still starts after complete source acquisition/validation.
- Ten-minute sessions, at most 32; renewal uses freshly validated same-account
  credentials. Request admission is bounded to eight and login attempts to 30/minute.
- Three-second upstream requests and ten-second listener handlers. These are
  conservative proof deadlines and may reject larger/slower remote acquisitions.
- At most 64 accepted HTTPS connections; ten-second TLS handshake/header bounds
  and a 60-second total socket lifetime, including slow response readers.
- Private temporary working directories, with only the most recent encoded
  interval retained per playback. No reusable retained cache or cache controls yet.
  Graceful cleanup is tested; crash recovery and low-disk policy remain future work.

The retained-cache design remains 5 GB by default, configurable from 1 GB upward
without an arbitrary maximum. Clear/disable confirmations, protection for active
work, separate working-storage accounting and configurable encoder concurrency
belong to the next slice.

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

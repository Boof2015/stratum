# Authenticated Sidecar access checkpoint

This is the first Rust HTTP service boundary for Sidecar. It checks credentials
and track access against Navidrome and enforces those checks before returning
cached data. **It is a loopback-only experiment, not a deployable streaming server.**
The protected endpoint returns only the first 64 original-file bytes. It does not
encode segments, play audio, discover servers, pair devices or integrate with Astra.

## Run the isolated proof

```sh
cargo build --release --bin sidecar-access
python3 -B experiments/access_proof.py \
  --navidrome /path/to/navidrome \
  --sidecar target/release/sidecar-access \
  --out artifacts/access-proof
```

Use a fresh output directory. The proof starts a real Navidrome with synthetic WAV
files, two libraries, an administrator used only for setup, and two ordinary users.
It uses the native admin API to assign/revoke library access, change a test password
and delete a test user. Every service binds to loopback. It never accepts an existing
Navidrome URL or reads Astra credentials, user music or audio devices.

Test state, the disposable database and credentials are deleted on exit. Child
processes have independent watchdogs and are terminated/reaped in `finally` paths.
Only sanitized logs and a credential-free `report.json` are retained. The proof
was run against Navidrome **0.64.2**; other releases are not claimed compatible yet.

The executable also accepts `--config PATH`, containing a small JSON object:

```json
{
  "upstream": "http://127.0.0.1:4533",
  "listen": "127.0.0.1:0",
  "session_seconds": 600,
  "run_seconds": 900,
  "connection_seconds": 30
}
```

That is a configuration shape, not an instruction to point at an existing account.
The proof creates the actual temporary addresses. Only literal loopback IPs and
HTTP are accepted in this checkpoint, with no upstream URL credentials/query/
fragment. A base path is preserved. The chosen listening address is printed once;
no session or credential is printed. Ctrl-C, SIGTERM, or the configured runtime
deadline revoke sessions and shut down. Configuration is immutable during a run;
changing backend requires restarting and invalidates every old bearer.

## API and authorization

| Route | Purpose |
| --- | --- |
| `GET /access/v1/capabilities` | Public protocol marker, explicitly `playback: false` |
| `POST /access/v1/sessions` | Accept `{username, token, salt}` as bounded JSON; validate with Navidrome |
| `DELETE /access/v1/session` | Revoke the bearer session and cancel its work |
| `GET /access/v1/media/{track}` | Return a protected 64-byte prefix; test endpoint only |

After login, use `Authorization: Bearer <session>`; never put credentials in URLs.
The session is a random 256-bit opaque value. Sidecar keeps the validated upstream
credential and private prefix cache only in memory. There is no client-supplied
upstream URL, alternate username or local path on media requests. Unknown login
fields are rejected. Requests with query strings, Origin or Cookie are rejected;
this is a main-process/headless interface, not a browser login API.

Session creation first verifies that deliberately invalid credentials are refused,
to catch an upstream proxy that silently authenticates everybody. This is a
configuration guard, not cryptographic proof of backend identity. Then it validates
the supplied credentials. Every upstream request uses a form POST, with redirects
and environment proxies disabled. Errors contain fixed codes, not upstream bodies,
URLs or credential material. There is no request/access logging middleware.

Before returning either new or cached bytes, Sidecar:

1. Validates the bearer, expiry and track identifier.
2. Calls `getSong` as that user, then checks a real original stream using a one-byte
   range (`format=raw`, `maxBitRate=0`). HTTP 200 containing a Subsonic failure is
   treated as an error, not audio. Range, size, content type and body bounds are checked.
3. Obtains the 64-byte prefix if not privately cached.
4. Repeats track/stream authorization before releasing the result.
5. Checks session liveness again when the response body is polled.

Logout/expiry interrupt outstanding upstream work. Authentication failure revokes
the session; failed track access clears its cached prefix. Upstream outage blocks
warm-cache delivery with a temporary-failure code. It never turns cached bytes into
indefinite permission. Audio already delivered to a client cannot be withdrawn;
these checks do not promise instantaneous revocation of buffered audio.

The token/salt tuple is still a reusable Navidrome credential. Expiring or deleting
the Sidecar session does not revoke that tuple at Navidrome. It is not a scoped
delegation token. Production use needs trusted association and protected transport,
plus a deliberate credential UX. No TLS/pairing claim is made by this service.
See the [released authentication implementation](https://github.com/navidrome/navidrome/blob/v0.64.2/server/subsonic/middlewares.go)
and [library access documentation](https://www.navidrome.org/docs/usage/features/multi-library/).

## Bounds and cleanup

| Resource | Bound |
| --- | --- |
| Accepted TCP connections | 32, including clients that never complete headers |
| Connection lifetime | At most 30 seconds; configurable lower for tests |
| Concurrent request handlers | 8; excess gets 429 |
| Handler lifetime | 10 seconds, including request-body reading |
| Upstream connect / entire request | 2 / 3 seconds |
| Session-creation attempts | 30 per rolling minute, global to this local prototype |
| Sessions | 32, each at most 600 seconds; idle expiry swept every 100 ms |
| Login body / upstream metadata | 4 KiB / 64 KiB |
| Private cached media | 16 prefixes × 64 bytes per session; clear at capacity |
| Service runtime | Default 900 seconds, maximum 3600, followed by bounded shutdown |

There is no encoder, subprocess work queue or on-disk media cache in the service.
The small prefix cache tests authorization only: it has **no strong source revision
validation** and must not be promoted into a playback cache. Cross-session or
cross-user cache sharing is not implemented. Dropping credentials from memory is
not a guarantee of zeroization. A disconnected login may leave an unseen session
until expiry, within the same fixed session bound.

## Verification

Recorded on 2026-10-17: **28 real-Navidrome checks passed** with the release binary.
The full suite passed **32 Rust tests (14 new access tests) and 73 Python tests**;
formatting, Clippy with warnings denied, and the release build were clean. Three
deliberately broken variants were rejected by their tests: a warm-cache permission
bypass, a missing final authorization check, and a missing body/session gate. The
tested source was restored after those controls. All disposable processes exited.

The real-server proof checks original-prefix equality, warm-cache reuse, isolation
between two scoped ordinary users, revoked/restored library access, changed
passwords, deleted accounts, logout, expiration, outage with warm bytes, and old
bearers rejected by another instance/restart. It also checks service logs for the
test's known credentials. The account-update setup sends writable fields explicitly;
native API creation returns an ID rather than a complete account object. Revocation
is tested by moving Alice from library A to B because Navidrome requires ordinary
users to retain at least one library.

Rust tests additionally exercise the actual adapter against controlled upstreams:
HTTP-200 auth errors, wrong range/content type/truncated bodies, oversized metadata,
redirects (the destination receives no request), a proxy overriding authentication,
permission loss during a held fetch, logout after headers but before body delivery,
expiry during work, session/cache/request limits, and stalled HTTP headers. Those
controlled cases supplement the real Navidrome proof; they are not all real-server
fault injections. Existing audio proofs remain separate and silent.

Source consistency, real encoded segments, original-file mutation, efficient
progressive acquisition, production credentials/TLS/discovery and Astra playback
remain the next checkpoints. This service cannot be enabled in Astra yet.

# Sidecar owner-access foundation

This is the first owner-setup checkpoint, separate from `sidecar-access` and the
audio engine. It supplies persistent configuration and HTTPS identity, terminal
or browser password enrollment, owner sessions, and local password recovery.
It is **not a complete streaming deployment**. [Connection setup](owner-setup.md)
is available as checkpoint 2 and [discovery publication](owner-discovery.md) as
checkpoint 3. The full dashboard, retained cache and Docker/systemd packaging
come in subsequent checkpoints. Astra's current streaming integration is unchanged.

## Start an isolated installation

Requires the repository's Rust toolchain. Current filesystem protection supports
Linux/macOS (Unix); Windows service permissions have not been implemented.

```sh
cargo build --bin sidecar
target/debug/sidecar init --data-dir /path/to/sidecar-data
target/debug/sidecar serve --data-dir /path/to/sidecar-data
```

Use a dedicated persistent directory owned by the service account. `init` creates
it with mode 700; an existing directory must already be private. State files are
mode 600. `serve` reads existing configuration and never prompts or initializes a
missing installation. This allows an installer/container entry point to perform
initialization explicitly and use `serve` for every ordinary start.

The default listener is `127.0.0.1:7843`, with administration origins
`https://localhost:7843` and `https://127.0.0.1:7843`. For a LAN installation:

```sh
target/debug/sidecar init --data-dir /path/to/sidecar-data \
  --listen 0.0.0.0:7843 \
  --admin-origin https://music.home.lan:7843 \
  --admin-origin https://192.168.1.72:7843
target/debug/sidecar serve --data-dir /path/to/sidecar-data
```

Those addresses are illustrative; use addresses that reach your host. Multiple
`--admin-origin` arguments are supported. These are exact HTTPS browser origins,
without a trailing slash or path, not the future Navidrome/discovery endpoints.
Origins are an explicit allowlist; forwarded headers are not trusted. Reverse
proxy administration and subpath hosting are not configured by this checkpoint.

On an unclaimed installation, `serve` prints a fresh 30-minute setup code to its
terminal/container log. Enter it on the HTTPS page and choose an owner password
before any setup becomes accessible. The code is single-use and is invalidated
by enrollment, replacement, or an unclaimed service restart. Only its hash is
stored. If it expires, run this locally against the same data directory:

```sh
target/debug/sidecar setup-code --data-dir /path/to/sidecar-data
```

The self-signed certificate produces the expected browser trust warning. Startup
also prints its **certificate SHA-256** fingerprint for comparison. `status`
reports the separate **SPKI SHA-256** fingerprint intended for direct-connection
identity/discovery. No certificate is installed in the OS/browser trust store.

## Terminal enrollment and recovery

```sh
target/debug/sidecar set-password --data-dir /path/to/sidecar-data
target/debug/sidecar reset-password --data-dir /path/to/sidecar-data
```

The first command enrolls an unclaimed installation; the second replaces an
existing owner password. Both prompt twice without echo. Passwords require at
least 12 characters, at most 1024 UTF-8 bytes, and no control characters.
`--password-stdin` supports automation with protected input; it reads until EOF,
removing one terminal newline. There is no command-line password argument.

Recovery works while Sidecar runs. It changes the owner-session generation, so
existing cookies fail on their next protected request without a service restart.
Configuration, installation ID and TLS key/certificate remain unchanged. This
command must run as the Sidecar service account with access to its private data
directory; for Docker it would run inside the container with the same mounted
volume. It does not change Navidrome credentials or listener accounts.

## Persistence and identity

- `config.json`: versioned listen address, exact administration origins and
  connection setup (see the [setup guide](owner-setup.md)).
  Editing it takes effect after restart. Unknown fields/versions are rejected.
- `private.json`: installation ID, certificate/private key, Argon2id password
  record and enrollment state. Do not edit or publish it. Back up both files.
- `state.lock`: coordinates atomic state updates across local commands/server
  processes. Enrollment has exactly one winner.
- `serve.lock`: prevents two servers sharing a data directory. Local recovery
  remains available while this lock is held. Lockfiles stay on disk; locks are
  released by the OS when their file handles/processes close.

Updates write a private temporary file, sync it, rename it over the old state,
then sync the directory. Re-running `init` does not overwrite an existing
configuration/identity. It can finish an initialization interrupted after the
private identity was saved but before configuration was written. Missing private
state alongside an existing configuration, corrupt state, insecure permissions,
or symlinked state files cause an error; they do not reopen enrollment.

Certificates last one year. This checkpoint has explicit renewal using the
existing key and the current administration hostnames:

```sh
target/debug/sidecar renew-certificate --data-dir /path/to/sidecar-data
```

Restart to use the renewed certificate. Its certificate fingerprint changes;
the SPKI identity does not. Browser warnings can recur. Automatic renewal and
address-edit UX remain later work.

## Access boundaries

HTTPS uses TLS 1.3. There is no plaintext HTTP login or public claim-by-first-visit.
Password hashing uses [RustCrypto Argon2id](https://docs.rs/argon2/0.5.3/argon2/),
and certificate generation uses [rcgen](https://docs.rs/rcgen/0.14.10/rcgen/).
Dashboard sessions are random opaque Secure/HttpOnly/SameSite=Strict cookies,
stored hashed in memory and bound to the current password generation. They expire
after 12 hours; restarting signs dashboard sessions out. At most 16 are retained.
Owner cookies are separate from the audio proof's listener sessions.

Mutation requests require a configured HTTPS Origin and JSON content type.
All requests require a configured Host; responses do not enable CORS. A CSP
blocks embedding and external assets, and responses use `Cache-Control: no-store`.
Body/header sizes, request/TLS timeouts and concurrent password work are bounded.
The owner login/claim endpoints share a limit of 10 attempts per minute per
process, including successful attempts. Local reset stays available during that
cooldown. This is a small owner-access service, not a completed public-ingress or
multi-client resource policy.

## Verification

```sh
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test -- --test-threads=1
node --check src/owner/owner.js
```

Store/route tests cover persistence, concurrent enrollment, code expiry/rotation,
corrupt and partial state, file permissions, renewal, origin/cookie boundaries,
rate/concurrency limits, and recovery during a running session. `tests/owner_cli.rs`
runs the actual binary with disposable data, performs real verified HTTPS requests,
resets the password from a separate process, then restarts and signs in again.
Children have deadlines and are killed/reaped on failure. These checks launch no
browser/Electron window, touch no existing server, and play no audio.

The web page now includes the [connection setup flow](owner-setup.md). Full
dashboard visual design remains subsequent work.

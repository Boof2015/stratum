# Sidecar connection setup

Checkpoint 2 adds a resumable Navidrome/listener setup flow to the `sidecar`
binary. Start with [owner access and HTTPS enrollment](owner-foundation.md).
This remains a development preview: saving connections does **not** publish a
radio announcement or start serving music. [Discovery publication](owner-discovery.md)
is now a separate optional step; music serving is still a subsequent checkpoint.

## Browser flow

After owner sign-in:

1. Enter the Navidrome base address that **Sidecar** can reach. HTTP and HTTPS
   are supported, including path prefixes such as `http://navidrome:4533/music`.
   In Docker, `localhost` refers to Sidecar's own container; a shared container
   network hostname may be appropriate instead.
2. **Check and continue** makes a bounded, credential-free OpenSubsonic ping.
   Navidrome identifies itself even when rejecting the missing listener sign-in.
   The page says “Navidrome detected. Sign-in has not been checked.” It never
   treats detection as authentication or permission to read music.
3. Enter a primary HTTPS listener address and optional additional addresses.
   Choose **Direct to Sidecar** or **Through an HTTPS reverse proxy** for each.
   Keep any path prefix. Addresses are checked for valid syntax and duplicates,
   but not for reachability from every future listener.
4. **Save connections** persists the settings and shows the summary. You can
   change either part or recheck Navidrome. Reloading or restarting resumes from
   the saved step; unsaved text is not retained across a page reload.

Direct addresses record `sidecar_key`, for verification against Sidecar's
persistent SPKI identity. Proxy addresses record `https_proxy`, for normal
HTTPS certificate/hostname verification and explicit trust in the owner's
proxy. This checkpoint stores that distinction; the streaming/discovery layer
will enforce it when connected. A failed direct-key check must never silently
fall back to proxy trust.

Listener addresses are **separate from administration origins**. Saving them
neither changes the bound port nor reconfigures the administration allowlist,
certificate, DNS, container network or reverse proxy. The existing admin page
is still hosted at `/`, not a proxy subpath. Listener paths are saved for later
streaming/discovery use. Nothing is announced to Navidrome during this flow.

## Terminal/config alternative

After `init` and `set-password`, create a separate JSON settings file:

```json
{
  "navidrome_url": "http://navidrome:4533/music",
  "listeners": [
    { "url": "https://sidecar.home.lan:7843", "trust": "sidecar_key" },
    { "url": "https://music.example/stratum", "trust": "https_proxy" }
  ]
}
```

```sh
target/debug/sidecar configure --data-dir /path/to/sidecar-data --settings connections.json
target/debug/sidecar check-navidrome --data-dir /path/to/sidecar-data
target/debug/sidecar status --data-dir /path/to/sidecar-data
```

`configure` uses the same model and validation as the web API, and is atomic.
It replaces the connection settings, so include every listener address you want
to keep. It can save while Navidrome is offline and explicitly reports that the
addresses have **not been checked**. An empty listener list permits partial
setup. `check-navidrome --url URL` can check another address without saving it.
Successful output always has `authenticated: false`; failure exits nonzero.
No Navidrome password or administrator token is accepted or stored here.

Settings live under `setup` in private-mode `config.json`. Existing checkpoint-1
configuration remains readable; the absent setup section defaults to the first
step. Use `configure` for live command-line changes. For direct file editing,
stop Sidecar, preserve the schema and file permissions, then restart.
`setup.revision` is service-managed; do not reuse it to work around a conflict.

The web service reads current settings from disk. Every save checks an expected
revision under the same lock used by local commands, then creates a fresh one.
This rejects late probes, concurrent browser saves, and A → B → A changes.
Password recovery also invalidates pending saves. A conflicting web save retains
its draft and offers **Reload saved settings**, rather than overwriting a newer
choice. Resetting the owner password and restarting preserve saved connections.

## Connection checks and limits

- Requests POST only `f=json`, `v=1.16.1`, and `c=AstraSidecar` to
  `<base>/rest/ping.view`. No owner cookies, listener credentials, authorization
  headers or environment HTTP proxies are used. Redirects are not followed.
- HTTPS uses the host's native trusted roots and normal hostname verification.
  There is no certificate-verification bypass. A private Navidrome CA must be
  trusted by the service host, or the owner can deliberately use an appropriate
  HTTP address. This is separate from Sidecar's self-signed browser certificate.
- Checks distinguish unreachable/timed-out servers, HTTPS failures, redirects,
  HTTP/access errors, non-Navidrome responses and API-version errors. Raw server
  HTML/error bodies are never rendered. HTTP status codes are shown separately.
- Two concurrent probes, 3-second connect/6-second overall timeouts, and a
  64 KiB response limit. Setup request bodies are capped at 32 KiB.
- Up to 16 listener addresses, each at most 1024 input bytes. Credentials,
  query strings, fragments, wildcard listen addresses and zero ports are rejected.
  Trailing slashes are normalized while prefixes, order and trust choices remain.

[Publication](owner-discovery.md) uses temporary Navidrome administrator
authorization and binds announcements to the saved settings/identity. Music requests
must still use the listener's own Navidrome permissions. Detection and owner
cookies grant neither privilege.

## Verification

The standard Rust suite covers the actual probe against disposable HTTP/HTTPS
servers, old-config compatibility, persistence, stale revisions, password reset
and logout during a slow probe, route authorization and real CLI/web sharing.
The original audio and authenticated-delivery regressions remain in that suite.

For a real disposable Navidrome check (empty library; no existing server):

```sh
cargo build --bin sidecar
python3 -B experiments/owner_setup.py \
  --navidrome /path/to/navidrome \
  --sidecar target/debug/sidecar
```

Python must have TLS 1.3 support; macOS's bundled Python may not. This checks a
Navidrome `/music` prefix through both CLI and verified owner HTTPS, saving direct
and proxy addresses and rejecting a wrong prefix. Children have hard deadlines,
are stopped on failure, and all service state is temporary. Checked with
Navidrome 0.64.2.

Optional browser check, using built-in Node modules and an installed Chrome:

```sh
node experiments/owner_ui.mjs /path/to/chrome /tmp/sidecar-ui-shots
```

It launches one headless browser with a separate temporary profile and 45-second
cutoff. It renders the real UI with fake API responses, checks failed/pending
requests, reload, trust selections, concurrent-save conflicts, sign-in/out and
narrow-width overflow, and captures screenshots. Backend behavior is covered by
the Rust/real-Navidrome checks. No Electron, existing browser profile or audio is
used. The UI is a functional setup preview; final dashboard styling is later.

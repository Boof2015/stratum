# Sidecar discovery publication

Checkpoint 3 adds optional discovery publication to the owner setup preview.
Finish [owner enrollment](owner-foundation.md) and [connection setup](owner-setup.md)
first. The `sidecar` entry point now serves [authenticated HTTPS audio](owner-streaming.md)
at the published capabilities path. This is still a development deployment with
explicit source/resource limits. Production Astra discovery and enable controls
remain a separate client integration step.

## Owner flow

The last setup step offers **Enable discovery** or **Use manual setup**. These
controls are also available later in **Settings → Discovery**.
Manual setup sends no announcement and asks for no Navidrome administrator
credentials. Listener addresses remain available independently of publication;
when the service is otherwise ready, publication failure must not block manual
connections.

Publishing asks for the administrator username and password of the **saved
Navidrome server**, with the target address shown. These are separate from the
Sidecar owner password. Sidecar detects the Navidrome API, signs in through its
native login endpoint, requires the returned administrator role, then uses the
returned temporary Subsonic credentials for radio operations. Credentials are
kept only in memory for this bounded operation. No Navidrome account, password,
bearer token or Subsonic credential is saved in Sidecar state or returned to the
browser. The browser clears the fields as the request starts, including errors.
This is lifecycle scoping, not a guarantee of cryptographic memory erasure.

The resulting radio entry is named `Stratum — <installation ID>`. Ordinary
Navidrome listeners can read it using their existing authenticated radio API.
Other clients may display it as a radio station; it is not playable radio audio.
The owner page explains that consequence before publication.

- **Update discovery** checks the saved station and updates its
  advertised values when needed. An already matching entry causes no write.
- Editing **Listener addresses** with discovery enabled asks for Navidrome admin
  sign-in as part of Save. **Update and save** publishes and reads back the new
  addresses before committing them locally. Cancel or rejected sign-in keeps the
  old saved addresses; the browser keeps the draft for retry. Unchanged addresses
  need no sign-in when discovery already matches. Manual setups save directly.
- A dropped reply can leave the new announcement on Navidrome while local settings
  are still unchanged. The pending record preserves that outcome for an authorized
  retry, which reads back the announcement before writing again. This is not an
  atomic transaction across the two servers.
- **Remove announcement** verifies ownership, removes that entry, reads the list
  again, and switches to manual setup. Unrelated radio stations are preserved.
- If the saved entry was removed outside Sidecar, publishing stops and offers
  **Recreate announcement**. The CLI equivalent requires `--recreate`.
- Failed operations show a readable reason. Every retry asks for administrator
  authorization again. A cooldown response from Navidrome says to wait.
- If a request may have reached Navidrome but could not be verified, the status
  says so. Retry first reads the actual radio list, avoiding a second create
  after a lost reply. The deterministic station name also uses Navidrome's unique
  name constraint; name matching alone never authorizes an overwrite.

The page distinguishes published, needs updating, manual, failed and unverified
states. Published means the last successful verification; Sidecar does not poll
the radio list using retained administrator credentials. Later outside edits
are detected on the next explicit maintenance operation.

## Terminal operations

```sh
target/debug/sidecar manual-discovery --data-dir /path/to/sidecar-data
target/debug/sidecar publish-discovery --data-dir /path/to/sidecar-data --username owner
target/debug/sidecar remove-discovery --data-dir /path/to/sidecar-data --username owner
```

Publishing/removal prompt once for the Navidrome administrator password without
echo. `--password-stdin` is available for protected automation; passwords are not
accepted in command-line arguments. Navidrome's password is not subjected to the
Sidecar owner's minimum-password-length rule. `status` includes public discovery
status and retry information. Web and CLI share the same operation code/state.

Manual selection cannot silently forget a published or possibly published
announcement. Use removal to withdraw it. If the configured Navidrome address
changes while an announcement may remain at the old address, new publication
stops; removal still targets the saved old address. Withdraw it before changing
servers when possible. There is no automatic migration or “forget all records”
button in this checkpoint. Manual use of the listener address does not require
clearing a failed discovery status.

## Metadata

This checkpoint's version-1 descriptor is a **development protocol**, distinct
from the earlier radio proof's endpoint-string example. It requires explicit
endpoint trust objects. Its contents are public to authenticated Navidrome users:

```json
{
  "version": 1,
  "kind": "astra-sidecar",
  "installation": "<32 hex characters>",
  "spkiSha256": "<64 hex characters>",
  "endpoints": [
    { "url": "https://sidecar.home.lan:7843", "trust": "sidecar_key" },
    { "url": "https://music.example/stratum", "trust": "https_proxy" }
  ]
}
```

Endpoint URLs are **bases**, preserving optional path prefixes. `streamUrl` is
`<primary base>/stratum/access/v1/capabilities`. `homepageUrl` on write (returned
as `homePageUrl`) is that same URL followed by `#stratum-v1=` and percent-encoded
UTF-8 JSON. The encoded descriptor is limited to 16 KiB; overly long address
lists must be shortened or used manually. Unknown versions/kinds/trust values,
invalid identities/URLs, and mismatched stream/descriptor addresses are rejected.

Direct endpoints require the advertised SPKI. Proxy endpoints require normal
HTTPS certificate/hostname verification and the owner's explicit proxy trust.
No failed direct-key check may fall back silently to proxy trust. This checkpoint
publishes the distinction; it does not implement client selection or prove proxy
reachability. The listener's configured Navidrome connection remains the trust
starting point, including an intentionally configured HTTP connection. Discovery
metadata is never a music-access grant.

## Persistence and interrupted operations

`discovery.json` (mode 600) stores the public announcement metadata, Navidrome
address, saved station ID, revision, and any pending mutation/error. It contains
no credentials. An absent file means discovery has not been chosen. Back up it
alongside `config.json` and `private.json`; do not hand-edit it while running.

A public pending record is saved **before** each upstream mutation. Success is
recorded only after reading back the matching result. On restart or lost reply,
the next explicitly authorized operation reconciles that journal. Missing or
malformed state never triggers an automatic remote write. This does not create
a distributed transaction: a request already accepted by Navidrome cannot be
undone merely because the owner closes the browser, signs out, resets the
password, or the process stops. Its actual result must be checked on retry.

A cross-process `discovery.lock` serializes publication and connection saves.
Web operations check both setup and discovery revisions plus owner authorization;
a newer local/browser choice cannot be overwritten by an older request. Logout
cancels an active web operation; password recovery is rechecked before mutation.
Sessions still expire. An operation has a 25-second overall limit, with bounded
individual requests and response bodies. Requests never follow redirects, use
ambient HTTP proxies, or bypass certificate verification. Radio credentials are
sent in POST bodies, not URL query strings.

Station ID alone is insufficient: the entry must still have this installation's
ID and SPKI in valid metadata. A reused ID, same-name ordinary station, or duplicate
announcement produces an error instead of overwriting/deleting an unrelated row.
Publication also refuses competing Stratum announcements rather than creating
an ambiguous choice for clients. An owner can resolve those entries in Navidrome.

The native login behavior is checked against
[Navidrome 0.64.2's implementation](https://github.com/navidrome/navidrome/blob/v0.64.2/server/auth.go).
Radio writes use the [OpenSubsonic radio endpoints](https://opensubsonic.netlify.app/docs/endpoints/createinternetradiostation/).
Other Navidrome versions and external-auth-only deployments have not been tested.

## Verification

The Rust suite tests publish/update/remove, no-op publication, lost create/delete
replies and restart reconciliation, no retained change despite a success reply,
wrong/non-admin/rate-limited sign-in, missing/reused station IDs, duplicates,
backend changes, revision/operation locks, cancellation and password recovery.

The real disposable-server check extends the previous setup proof:

```sh
cargo build --bin sidecar
python3 -B experiments/owner_setup.py \
  --navidrome /path/to/navidrome --sidecar target/debug/sidecar --publication
```

It creates a temporary administrator, ordinary listener and ordinary radio entry,
checks the complete maintenance flow via web and CLI, compares the exact direct/
proxy metadata read by the listener, then checks that no Navidrome credentials
remain in Sidecar's state files. It needs Python with TLS 1.3 support. Only this
disposable fixture raises Navidrome's login rate limit to allow many test sign-ins
in a few seconds. No existing service or account is touched.

The optional single-headless-Chrome `experiments/owner_ui.mjs` harness also checks
manual choice, rejected publication, immediate clearing of credential fields,
publish/removal state refresh, and the existing setup flows. It has a separate
temporary profile and 45-second cutoff. No Electron or audio is launched.

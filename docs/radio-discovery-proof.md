# Navidrome radio announcement experiment

Manual Stratum address entry is a confirmed product option. Radio discovery is
an optional convenience being evaluated alongside it, not a prerequisite for
using Stratum. Server owners configure and associate Sidecar without Astra;
Astra is only the listening client.

On 2026-10-19, **47 checks passed against real Navidrome 0.64.2 (10114574)**.
An owner-created internet radio entry can carry exact Stratum discovery metadata
to ordinary authenticated users, including users with different library access.
Those users cannot alter the entry through either tested API. A client can then
verify an advertised endpoint key using a real TLS handshake.

This is an experiment, not an installed owner setup flow or Astra feature.
No Rust Sidecar code, production transport, player, or saved-source code changed.

## Reproduce

From the Stratum repository, with Python 3.9+, Node with TLS 1.3 support, OpenSSL,
and a separately obtained Navidrome executable:

~~~sh
python3 -B experiments/radio_discovery.py \
  --navidrome /path/to/navidrome \
  --out artifacts/radio-discovery-run
~~~

The output directory must not already exist. The script never downloads or
installs software. Tested with Python 3.9.6, Node 26.5.0, OpenSSL 4.0.3 and the
previously SHA-256-verified Navidrome binary from the source-acquisition proof.
The orchestration uses POSIX process groups, as the earlier access proof does.
It reuses helpers from access_proof.py and navidrome.py.

The test creates one temporary Navidrome process, two empty music libraries, one
administrator and two non-admin users with disjoint library assignments. Two
small Node HTTPS fixtures represent the intended Sidecar identity and a different
identity. All listeners bind to literal 127.0.0.1 on temporary ports. Every child
has a 90-second lifetime limit; HTTP/TLS/subprocess operations are bounded. Normal
or exceptional exit stops the children and removes the temporary database,
accounts, request logs and TLS private keys.

The final report contains public metadata and named check results, not passwords,
Navidrome bearer tokens or private keys. No existing server/profile is used.
There is no Electron, browser window, audio playback or real music access.

## Candidate record

This format was chosen to test storage and discovery, not to establish a wire
protocol:

| Radio field | Experimental contents |
| --- | --- |
| name | A readable “Stratum — discovery experiment” label |
| streamUrl | The first HTTPS capabilities endpoint |
| homepageUrl on write / homePageUrl on read | The endpoint followed by a URL fragment containing versioned, percent-encoded JSON |

The JSON contains version 1, a random installation ID, a SHA-256 fingerprint of
the DER-encoded SubjectPublicKeyInfo, and up to four endpoint URLs. The tested
descriptor was 552 bytes. An actual loopback endpoint plus a hostname with a
path prefix and an IPv6 alternate address survived the Navidrome round trip
exactly. Alternate addresses were metadata only; no external address was fetched.

The candidate parser limits the record to 4096 characters, checks version, IDs
and key fingerprint, rejects credentials/query/fragment in endpoint addresses,
and recognizes the explicit metadata marker rather than just a station name.
An ordinary station is ignored. Multiple valid announcements are treated as
ambiguous instead of selecting the first one.

The example's "/proxy/" path only tests preservation of a path prefix. There is
no reverse proxy in this experiment.

## Verified behavior

- The administrator can create, update and remove the announcement through
  Subsonic. The exact endpoint, key, installation ID and alternate addresses
  reach both ordinary users.
- Both users can also see it as an ordinary row in the native radio API.
- Both users are denied Subsonic create/update/delete with error 50 and native
  POST/PUT/DELETE with HTTP 403. The saved radio list remains unchanged after
  every denied operation.
- Anonymous readers and a wrong-password reader cannot obtain the announcement.
- Creating/listing the entry did not fetch the advertised endpoint.
- The announced key succeeds in a real TLS 1.3 connection. Substituting another
  TLS endpoint or another expected key fails before any HTTP request reaches
  that endpoint.
- Discovery requests contain no Navidrome authorization, cookies or auth query
  parameters. They fetch only public capabilities.
- An ordinary station, malformed or oversized metadata, an unknown version,
  invalid keys, credentials embedded in URLs and unsupported URL schemes do not
  become valid candidates.
- Navidrome enforces unique radio **names**, not unique stream URLs. An identical
  name with the same or another URL returns Subsonic error 0 with a radio.name
  uniqueness error. Different names can create duplicate or conflicting Stratum
  announcements. The test rejects that ambiguity and verifies owner cleanup.
- Owner updates propagate exactly. The previously expected key rejects a
  replacement endpoint; the explicitly selected replacement key verifies it.
  This tests the primitive, not a decided product policy for approving rotation.
- Removing the announcement withdraws discovery without deleting an ordinary
  radio station.
- Test processes stop and temporary secrets/state are removed.

## What the TLS part establishes

radio_tls_probe.mjs is an isolated fixture helper. Its client completes a TLS 1.3
handshake, extracts the peer certificate's public key, and compares its SHA-256
fingerprint with the key read from Navidrome. Only after that exact match does it
send a fixed public HTTP request. A different server claiming the same
installation ID cannot pass with a different private key.

The fixture uses self-signed certificates and explicit key pinning instead of the
system CA store. The TLS socket's rejectUnauthorized:false is scoped to this
experiment and is paired with the mandatory pin check **before any application
request**. Do not copy that flag into an ordinary HTTPS client or change global
certificate verification. The tiny HTTP/1.0 response reader is also fixture-only,
not a proposed production transport.

No extra signed-nonce endpoint was required for this direct TLS proof: TLS
already establishes possession of the matching private key and protects the
connection. A public capabilities response is not an authorization grant.
Existing Sidecar listener authentication and per-track permission checks remain
necessary when this is integrated.

The selected Navidrome connection is the trust starting point, including HTTP
under the user's explicitly accepted source configuration. This run deliberately
used HTTP to Navidrome. It does not make the announcement resistant to active
interception of that connection; no extra mandatory pairing workflow follows
from that inherited limitation.

## Product consequences and remaining work

The announcement appears in normal radio lists. No hiding mechanism was tested.
Other clients may present it as a station, and this experimental endpoint returns
JSON rather than playable audio. Astra could recognize and hide its own entry,
but cannot control how other clients show it. Manual setup avoids that tradeoff.

Owner setup should remember the created station ID and installation identity,
then update that specific entry. A repeated-name error is not sufficient reason
to overwrite an unrelated radio station. Missing entries, deliberate recreation
and duplicate announcements need explicit handling. The experiment obtains its
station IDs by listing after creation; production setup needs to handle that
step deliberately.

Admin authorization is needed for announcement maintenance only; the experiment
does not define credential prompting/storage or a finished setup command.
Normal streaming must not retain or use the publishing administrator's account.

Still unimplemented/unverified:

- Production owner setup, persistent identity, announcement maintenance and
  integration with Astra's manually configured or saved Navidrome sources.
- Direct Sidecar TLS implementation and the trust/identity arrangement when a
  reverse proxy terminates TLS. A proxy's public TLS key may differ from the
  Sidecar application key; the single-pin sample format does not solve that.
- Network reachability, actual alternate-address selection, Docker, tailnets,
  remote DNS and proxy deployments. Preserving their URL strings is not testing
  these networks.
- Credential-carrying sessions over that transport, stable association with the
  configured Navidrome backend, key/backend changes and source lifecycle.
- Other Navidrome versions, other compatible servers, and visual behavior in
  third-party radio clients.

The result supports continuing with owner-controlled radio announcements as a
discovery option. It does not select the final metadata format or replace manual
setup, which remains confirmed.

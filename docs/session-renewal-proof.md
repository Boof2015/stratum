# Session renewal checkpoint

This extends the disposable, literal-loopback access/audio protocol. It does not
yet add pairing, TLS, discovery or real-account setup. Astra has a matching
main-process session owner and a silent integration fixture in its own repository.

## Contract

Capabilities advertises `sessionRenewal: "fresh-credentials-same-user"`.
`POST /access/v1/session`, with the existing bearer header and bounded JSON
`{username, token, salt}`, repeats both authentication checks: deliberately invalid
credentials must fail, and the supplied credentials must succeed. The username
must match the session. Request/response sizes, login rate limits, connection
limits and deadlines are unchanged. The response is `{session, expiresInSeconds}`.

Renewal retains the bearer, private staged sources, current/next playback handles
and cached segments. It replaces the saved upstream tuple and extends the lease.
Bearer possession alone cannot extend the lease. The bearer is not rotated in
this checkpoint; it remains a bearer secret until logout/expiry. Subsonic tuples
are also reusable secrets, despite generating a fresh salt for each exchange.

Inflight requests re-read an extended deadline when the old deadline arrives.
Logout, shutdown and expiry cannot be undone by late authentication responses.
An already expired session must be replaced explicitly, not renewed. Refused
credentials/access or an upstream accepting bogus credentials revoke the session.
A transient upstream error does not extend its expiry. Every media request still
performs its normal permission checks, including warm cached segments.

## Verification

`cargo test --offline` covers renewed private-state identity, current/next encoded
audio after the original expiry without re-fetching their sources, a held fetch
spanning its old deadline, account mismatch, missing/wrong credentials, logout/
shutdown/expiry races, upstream outage, a permissive proxy and rate limiting.
The full Rust suite passed: 28 library, 12 source-session and 13 authenticated-
audio tests (53 total). These tests use controlled HTTP upstreams, not a new real
Navidrome compatibility run.

The matching Astra `scripts/stratum/sessionProof.ts` uses this binary, a disposable
Subsonic protocol server, generated WAV data, fresh MD5/salt exchanges and Astra's
real session owner/PCM adapter. With two-second leases, current/next audio remains
sample-exact through several renewals; staged sources are fetched only once per
playback. Seeking still works and revoked track access blocks delivery. No audio
device is opened.

## Limits / next integration

The Astra owner is an explicit disposable-test path, not yet wired to saved server
credentials or settings. It renews halfway through a lease and shares one pending
exchange across consumers. Failed renewal ends that experimental connection;
reconnect/retry UX remains future work. Abandoned successful logins whose replies
never reach the client remain bounded by server expiry. Runtime and source
staging limits from the access/audio checkpoints still apply.

Before using real credentials: implement owner-provided pairing and protected
transport, bind the trusted Sidecar to the chosen Navidrome source, and connect
source disable/remove/credential changes to explicit session teardown. Pairing
was accepted as the setup option for installations without existing HTTPS; a
reverse proxy must not become a requirement for LAN/Docker users.

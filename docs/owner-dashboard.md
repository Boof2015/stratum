# Sidecar live owner dashboard

After guided setup, the owner page opens **Overview**. Compact connection status,
session and conversion cards, last sent quality, and visual storage meters show
current activity. **Settings** holds connection editing, discovery, cache and
resource controls. Overview links directly to the relevant settings section.

Activity refreshes every two seconds while Overview is visible. Updates stop in
Settings, when the browser tab is hidden, during setup, and after sign-out.
Switching Overview/Settings preserves storage drafts and pending confirmations.
Leaving Settings clears temporary Navidrome administrator credentials.

Healthy status stays compact; connection failures show the reason and a settings
action. Discovery problems also surface on Overview. **About these readings**
contains the technical interpretation. Cache and temporary storage have separate
meters with numeric values and accessible descriptions. Working storage shows
used space over reserved capacity; values above a reduced limit remain truthful
while the bar caps at 100%. There is no arbitrary upper cache-size tier.

This is local operational visibility. Sidecar keeps no listening-history
database and sends no analytics elsewhere.

## What the readings mean

| Reading | Meaning |
| --- | --- |
| Signed-in sessions | Valid listener sessions, including idle ones. A device can have more than one; this is not a device or TCP-connection count. |
| Prepared streams | Successfully prepared playback handles. These include tracks prepared ahead of playback; pending source acquisition is not yet an open stream. |
| Last sent quality | The last encoded audio sent on that stream: Original (lossless), Opus 64 or Opus 128. Before any audio is sent, it says “Prepared; no audio sent yet.” |
| Transfer rate | Encoded payload bytes handed to the HTTP connection, averaged over five one-second buckets. Cached delivery counts too. |
| Conversions | Running and waiting validation/encoding jobs. Retained cache hits do not need an encoder. |
| Storage | Retained cache usage and separate temporary usage/reservations, with their configured limits. |

Sidecar cannot observe the client's actual listening position, pauses or buffered
audio. “Last sent” is not a claim about what the listener is hearing now.
Original means lossless PCM delivery for supported sources, not identical
original file bytes.

The transfer reading excludes response metadata, HTTP/TLS headers, the old
64-byte access probe, and traffic fetching originals from Navidrome. It measures
bytes yielded to the response, not acknowledged receipt or the listener's
available connection speed. Unconsumed/cancelled body chunks are not counted.
The rate becomes zero after the short window without new delivery.

Clients and streams get temporary display numbers. The dashboard does not expose
bearer tokens, usernames, track IDs, source hashes or addresses identifying a
listener. These numbers do not survive restart and are not device identities.
Logout, expiry, playback close and configuration invalidation remove the relevant
rows. A recent transfer can remain in the five-second aggregate after a row
disappears. All activity resets on service restart.

## Health and refresh behavior

Sidecar's setup status and Navidrome's reachability are separate readings.
The Navidrome check reuses the existing credential-free, bounded connection
probe. It accepts Navidrome identifying itself while rejecting absent sign-in;
success does not prove any listener's credentials or library permissions.

The result is cached for 30 seconds, shared across owner tabs, with at most one
check in flight. It shows when the last check completed. A check is started only
when an owner requests the dashboard; hidden/closed dashboards do not schedule
periodic upstream traffic. An already-started check may finish after a tab closes.
Configuration changes discard old results, and a late check cannot replace the
new configuration's health. The probe has a six-second deadline and is cancelled
on service shutdown.

Reachability is not a full diagnosis of storage, FFmpeg, all music formats or
every listener address. Retained-cache access problems are shown in the Storage
section. Existing manual connection checks remain available.

A failed dashboard refresh leaves the previous readings marked out of date and
retries. It does not invent zero sessions or healthy status. Sign-in expiry
returns to the owner sign-in screen. Late responses from a previous page or
sign-in are ignored.

Live refresh changes readings only. It does not overwrite an unsaved cache limit,
conversion choice or working-storage limit, nor dismiss a clear confirmation.
**Refresh usage** updates only readings, preserving drafts and their original
revision guards. Saved-settings revisions remain guarded; a concurrent CLI/browser edit can still
require **Reload saved settings** when saving.

## API and implementation bounds

`GET /admin/api/dashboard` requires the existing owner cookie and administration
boundary checks. Listener bearer tokens do not authorize it. Responses are
`no-store`; owner authorization is rechecked after the snapshot work, so a
password reset or logout during that work wins.

The response contains readiness, `activity.clients` and their streams,
`bytes_per_second`, `rate_window_seconds`, `storage`, and `navidrome` health.
This is a development owner API, not a finalized client protocol.

Activity uses the existing bounded session/playback maps, one small observation
per stream, and five aggregate byte counters. There is no event log or
per-request history queue. Storage snapshots run through the existing bounded
background work allowance; audio delivery does not scan storage. The current
[source and resource limits](owner-streaming.md) remain unchanged.

## Verification

Rust tests cover exact byte accounting, exclusion of metadata and cancelled
chunks, rate decay, owner-only access and password recovery, shared/cached health
checks and stale responses. Installed HTTPS tests use real encoding to check
prepared streams, quality changes, retained hits, logout and restart.

The real disposable Navidrome experiment also checks dashboard authorization,
reported delivery, credential-free reachability and logout. The single-process
browser harness checks live values, unsaved drafts, confirmations, stale/error
recovery, hidden-tab suspension, sign-out and an old response arriving after a
new sign-in. It also covers guided setup, keyboard connection choices, discovery
failures across reload, view navigation, direct settings links, and above-limit
meters. Click helpers require visible enabled controls. Dark/light and narrow
layouts are inspected. No existing servers,
personal music, audio devices or Electron processes are used.

Deployment packaging and Linux/container validation remain subsequent work.

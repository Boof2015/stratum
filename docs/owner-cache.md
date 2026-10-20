# Sidecar retained audio cache

Checkpoint 4b adds a retained encoded-segment cache to the installed HTTPS
service. It reuses prepared FLAC/Opus intervals between playbacks and across
restarts. It does not preconvert the library.

**A new playback still downloads and validates its complete original.** This
slice saves repeat encoding, not Navidrome source traffic. Our Navidrome tests
found that source bytes can change without a reliable timestamp change, so file
size and Last-Modified cannot safely identify a reusable source. Source-fetch
optimization is separate work.

## Owner controls

After connection setup, Overview shows separate cache and temporary-storage
meters. **Settings → Storage** contains the cache controls. The
[live dashboard](owner-dashboard.md) updates readings while visible;
**Refresh usage** in Settings updates readings without replacing unsaved controls
or their original revision guards.

- Retention starts **on**, with a **5 GB** limit. The browser and CLI accept
  custom whole-GB limits from **1 GB** upward, with no arbitrary storage tier
  maximum. Limits use decimal GB and do not reserve disk space.
- Saving a smaller limit evicts the least recently used retained entries until
  usage fits. The service does not touch active working files.
- **Clear cache…** requires a second confirmation. Future listening can
  refill the cache when retention is on.
- Turning retention off requires one combined **Turn off and clear**
  confirmation. Temporary storage remains necessary for active streams.
- **Cancel** changes neither saved settings nor retained content.

Cache changes do not revoke listener sessions or require a service restart.
Different browser tabs and CLI commands share revision-checked settings; a
stale browser save offers a reload instead of overwriting newer settings.
If a saved clear/shrink cannot be applied because of storage access, the page
says so and the service bypasses retention while retrying. It does not claim
that audio was cleared before the operation succeeds.

Terminal equivalents, after owner enrollment:

```sh
sidecar configure-cache --data-dir /path/to/sidecar-data --limit-gb 20
sidecar clear-cache --data-dir /path/to/sidecar-data --confirm
sidecar configure-cache --data-dir /path/to/sidecar-data --disable --confirm
sidecar configure-cache --data-dir /path/to/sidecar-data --enable
```

Without `--confirm`, a clear or transition to disabled fails without changing
anything. When Sidecar is running, the CLI persists the change and the service
applies it on its next request/maintenance pass (normally within a second).
When stopped, the CLI applies it before returning. CLI output distinguishes
these cases. Only the process holding the installation's `serve.lock` opens
the cache.

`GET /admin/api/storage` and `POST /admin/api/storage` require owner
authorization and the existing same-origin protections. POST accepts
`revision`, `enabled`, `limit_bytes`, optional `clear`, and optional `confirmed`.
The API accepts byte limits from 1 GB through JavaScript's exact integer ceiling;
the whole-GB input is a UI/CLI convention. Settings live under `storage` in
`config.json`. Older configurations default automatically. Revision and clear
epoch are service-managed; use the commands/API for changes.

## Identity, permissions and recovery

The private `retained/` directory contains bounded, checksummed packet files.
Keys include the complete verified source hash and format, encoder build,
representation version, requested sample interval and quality. A different
source, encoder, interval or quality gets a different key. Filenames and cache
headers contain no usernames, track IDs, addresses or credentials.

The cache is installation-wide: identical source bytes can reuse an encoding
across authorized listeners. Every playback still acquires its source through
that listener's permissions. Every segment, including a retained hit, checks
Navidrome access before lookup and again before delivery. A cache hit cannot
grant access during an outage, after revocation, or after logout.

Retained hits do not take an encoder slot. Disk lookup/write work is bounded
and runs off the audio runtime threads. Damaged entries become misses and are
regenerated. Cache-write failures do not reject an otherwise prepared segment.
An optional `retained_cache_hit: true` metadata field identifies retained hits
for diagnostics; the existing `cache_hit` remains true for those hits too.

Clear/disable saves a new durable epoch before deleting entries. Startup
finishes an interrupted clear and removes abandoned partial writes. Encoding
jobs hold a revision ticket, so jobs started before a clear cannot refill it
when they finish late, even if retention has since been re-enabled. An active
response owns its own bytes, and source/worker files live outside the retained
directory.

Least recently used order survives restart approximately through file
modification times. Entry bytes, including their small integrity headers,
count against the limit; filesystem allocation/metadata overhead is not included.
On low space, retention first evicts entries and then skips new writes if it
cannot leave a 512 MiB free-space reserve on its volume, plus active working
reservations when they share that volume. The
[resource controls](owner-resources.md) describe working admission and recovery.

## Remaining limits and verification

The source/worker bounds in [HTTPS streaming](owner-streaming.md) still apply.
Working storage can be on a different volume from retained storage. Checkpoint
4c adds [working limits, scheduling and hard-crash cleanup](owner-resources.md);
these controls do not eliminate complete-source acquisition.

The Rust suite checks eviction, large configured limits without reservation,
corruption, symlinks, interrupted clears, late jobs, low-space behavior,
authorization, live CLI/web settings and old-config migration. Real HTTPS/audio
tests verify reuse after restart, cache hits while both encoders are occupied,
permission loss at final delivery, source-content changes with unchanged size,
and clear/disable during active playback.

The bounded owner UI harness checks confirmations and cancellation, custom
limits, save errors, concurrent updates and separate working usage. The real
Navidrome experiment in `experiments/owner_streaming.py` remains a silent
regression check for scoped access and actual FLAC/Opus delivery.

# Sidecar conversion and working-storage controls

Checkpoint 4c gives the installed HTTPS service shared resource limits, bounded
conversion waiting, and recovery of temporary files after a crash. It builds on
the [retained cache](owner-cache.md). These are development defaults for the
current four-playback service, not a claim of production capacity on every host.

## Owner controls

After connection setup, **Settings → Resources** offers:

- **Simultaneous conversions:** Automatic, or a custom maximum of 1–4.
- **Working storage limit:** 512 MB by default, at least 128 MB.
  MB here means 1,000,000 bytes. This limit is separate from retained cache.
- A snapshot of running/waiting jobs and the effective conversion limit.
- Temporary bytes used and capacity reserved against the working limit.
  **Refresh usage** in the Audio cache section updates these snapshots without
  replacing unsaved settings.

Saving a lower limit does not stop current jobs or delete active files. New work
must fit the new limit. Increasing it makes capacity available immediately.
Neither change signs listeners out. The [live dashboard](owner-dashboard.md)
updates the readings while visible without overwriting edits to the controls.

Equivalent commands, after owner enrollment:

```sh
sidecar configure-resources --data-dir /path/to/sidecar-data \
  --conversions automatic --working-mb 512
sidecar configure-resources --data-dir /path/to/sidecar-data --conversions 1
sidecar configure-resources --data-dir /path/to/sidecar-data --working-mb 1024
```

Omitted options retain their saved values. A running service applies a CLI change
on its next request/maintenance pass, normally within a second.
`GET /admin/api/resources` returns the same owner-only snapshot as the storage
API. `POST /admin/api/resources` accepts `revision`, `max_conversions` (`null`
for Automatic), and `working_bytes`. Owner authorization, same-origin checks,
and revision checks apply. A stale save cannot overwrite a newer browser/CLI
choice. Cache and connection revisions remain separate.

The private configuration gains a `resources` section; older configurations
default automatically. The byte API accepts limits through JavaScript's exact
integer ceiling; whole MB is the UI/CLI convention. Use the commands/API for
changes. Service-managed revisions should not be edited.

## Automatic and overload behavior

Automatic takes half the available logical CPU estimate, rounded down, with a
minimum of one and a maximum of four. For example, 2 CPUs gives one job, 4 gives
two, and 8 or more gives four. A failed CPU query falls back to one.
The estimate is read at startup and when resource settings change, not for each
segment. Restart or save Automatic again after changing container CPU limits.

Rust's [`available_parallelism`](https://doc.rust-lang.org/std/thread/fn.available_parallelism.html)
accounts for affinity and CPU quotas where the platform exposes them, with
documented limitations. This is an estimate, not live CPU-load monitoring or an
enforced CPU quota. A single conversion can use CPU capacity needed by another
service, especially on a one-CPU host. Owners can reduce the maximum.
The 1–4 custom range follows the existing development playback bound; broader
capacity needs Linux/container and smaller-machine measurements.

Source inspection/validation and uncached encoding share the same scheduler.
Retained hits bypass it. Already-running jobs finish normally when a limit
shrinks; the scheduler admits no replacement until usage is below the new limit.

The installed service queues at most eight conversion requests for up to one
second each. Pending requests release their place on cancellation. Playback
work takes priority over prefetch, but an oldest request waiting at least
500 ms gets the next available slot to avoid starving prefetch.

Clients can mark intent without changing audio or cache identity:

- Playback creation: optional `X-Stratum-Priority: playback` or `prefetch`.
- Segment JSON: optional `"priority": "playback"` or `"prefetch"`.
- Omitted intent means `playback`. Unknown values are rejected.

Existing Astra requests still use the default; distinguishing their prefetch
requests is a later client integration step. These remain experimental APIs.
A full/expired conversion queue returns HTTP 429, with `Retry-After: 1`.
Other request/session/playback admission limits still apply. The loopback proof
keeps its original two-worker, immediate-busy behavior.

## Working capacity and low disk space

Each playback reserves its advertised original size plus **32 MiB** for bounded
worker files before acquiring the source. Download length is checked against
that size and the existing 64 MiB source ceiling. The reservation remains until
the playback and its last worker release their files, including cancellation.
At most one worker uses a playback's files at a time.

The 512 MB default covers four maximum-size originals plus this headroom:
4 × (64 + 32) MiB, about 403 MB. A 128 MB limit fits one maximum-size playback.
This is conservative capacity accounting, not disk preallocation or a filesystem
quota. Temporary usage reports named file lengths; unlinked subprocess outputs
and filesystem metadata are not included in that snapshot. The reservation
includes scratch headroom for them.

Before admitting a playback, Sidecar requires the volume's free space to cover
the full current reservations plus **512 MiB**. It deliberately does not subtract
already-written source bytes from the reservations. If retained cache shares
that volume, Sidecar first evicts its least recently used entries and rechecks
free space. Cache writes also respect the working reservation. Different-volume
cache is not deleted to make space on the working volume.

If the budget or free-space check still fails, creation returns HTTP 503
`working_storage_unavailable`, with `Retry-After: 1`. Existing playback files are
kept. A later retry can succeed after another playback closes or space is freed.
Other processes can still exhaust disk space after admission; this is a best-effort
guard, not a guarantee against external disk use. It also does not enforce a
process-memory limit.

## Crash cleanup

Installed working files now live under the installation's private
`working/run-XXXXXX/` directory. Each process run owns a file lease. Workers hold
that lease independently and watch a pipe from their parent; if the service is
hard-killed, the worker kills its process group, including FFmpeg descendants.
Normal cancellation, worker deadlines, and graceful shutdown keep their existing
group cleanup and reaping.

On startup, while holding the installation's server lock, Sidecar removes stale
owned run directories only after obtaining their exclusive lease. It waits up
to two seconds for a departing worker, then fails with a retry message rather
than deleting files still in use. Symlinks are not followed; unknown names are
left alone. A run with no lease is removed only if empty.

This cleanup covers the new installation-owned directory. It does not sweep
global temporary directories from older proofs or other installations.
The implementation currently targets Unix and has been exercised on macOS.
Linux/container deployment checks remain ahead.

## Verification and remaining work

Tests exercise scheduling priority, ageing, cancellation, timeout, live limit
changes, working reservations, low-space cache reclamation, old configurations,
stale owner saves, CLI/web sharing, and cache identity across priority changes.
Installed HTTPS tests perform real FLAC/Opus work while limiting storage, close
playbacks to release capacity, and SIGKILL a service during an encode. The crash
test verifies the encoder exits, the next startup removes stale files, and fresh
playback works. Low-space tests inject free-space readings rather than filling
the developer's disk. The bounded browser harness checks successful, failed,
and conflicting resource saves.

A silent sanity measurement used a synthetic 103-second, 96 kHz, 24-bit stereo
WAV (about 59 MB), requesting cold four-second lossless and Opus intervals at
1, 2, and 4 parallel jobs. All finished in roughly 2.1–2.4 seconds in the debug
build on the development Mac, within the existing eight-second worker deadline.
That includes full-source inspection/hash work; it is not a portable throughput
benchmark or a measurement of peak memory/disk use.

Complete-source acquisition and the other [streaming limits](owner-streaming.md)
remain. The [live dashboard](owner-dashboard.md) now reports activity. Progressive
source reuse, deployment packaging, wider machine validation, and production
Astra discovery/enablement are separate work.

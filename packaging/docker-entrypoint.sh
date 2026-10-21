#!/bin/sh
set -eu
umask 077

data=/var/lib/sidecar
if [ "$#" -eq 0 ]; then set -- serve; fi

if [ "$1" = serve ]; then
    # Bootstrap only an empty volume. Partial/corrupt state must fail normally,
    # never silently replace identity or restore default connection settings.
    entries=$(find "$data" -mindepth 1 -maxdepth 1 -print -quit)
    if [ -z "$entries" ]; then
        : "${SIDECAR_ADMIN_ORIGIN:?Set SIDECAR_ADMIN_ORIGIN to the HTTPS dashboard origin, for example https://music.home.lan:7843}"
        sidecar init --data-dir "$data" --listen 0.0.0.0:7843 \
            --admin-origin "$SIDECAR_ADMIN_ORIGIN"
    fi
fi

# Keep Sidecar as PID 1: SIGTERM reaches its normal shutdown/worker cleanup.
# Other commands (status, setup-code, reset-password, ...) use the same volume.
exec sidecar "$@" --data-dir "$data"

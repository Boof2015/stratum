#!/bin/sh
# Run from an extracted, verified Sidecar Linux archive. Never downloads a binary.
set -eu
umask 077
PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin
export PATH

fail() { printf '\n%s\n' "$*" >&2; exit 1; }
usage() {
    echo 'Usage: sudo ./install.sh [--yes] [--admin-origin https://HOST:PORT] [--port PORT]'
    echo 'Installs or updates Sidecar on Linux/systemd. Existing settings are preserved.'
}
origin= port=7843 port_set=no yes=no
while [ "$#" -gt 0 ]; do
    case "$1" in
        --yes) yes=yes; shift ;;
        --admin-origin) [ "$#" -ge 2 ] || fail '--admin-origin needs an address.'; origin=$2; shift 2 ;;
        --port) [ "$#" -ge 2 ] || fail '--port needs a number.'; port=$2; port_set=yes; shift 2 ;;
        --help|-h) usage; exit 0 ;;
        *) usage >&2; exit 1 ;;
    esac
done
[ "$(uname -s)" = Linux ] || fail 'This installer needs Linux with systemd.'
[ "$(id -u)" = 0 ] || fail 'Run this installer as root: sudo ./install.sh'
[ -d /run/systemd/system ] && command -v systemctl >/dev/null || fail 'systemd is not running. For Docker, use the Compose instructions in INSTALL.md.'
command -v flock >/dev/null && command -v runuser >/dev/null || fail 'Install util-linux (flock and runuser), then try again.'
exec 9>/run/sidecar-install.lock
flock -n 9 || fail 'Another Sidecar installation is already running.'
[ "$yes" = yes ] || [ -t 0 ] || fail 'Run interactively, or use --yes (and --admin-origin for a new install).'
package=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
[ -x "$package/sidecar" ] && [ -f "$package/sidecar.service" ] || fail 'Extract the complete archive, then run its install.sh.'
"$package/sidecar" --help >/dev/null || fail 'This Sidecar binary cannot run here. Check the archive architecture and Linux version.'
binary=/usr/local/bin/sidecar
unit=/etc/systemd/system/sidecar.service
data=/var/lib/sidecar
for path in "$binary" "$unit" "$data"; do
    [ ! -L "$path" ] || fail "Refusing to replace a symbolic link: $path"
done

existing=no
if [ -d "$data" ] && [ -n "$(find "$data" -mindepth 1 -maxdepth 1 -print -quit)" ]; then
    existing=yes
    [ -f "$data/config.json" ] && [ -f "$data/private.json" ] || fail 'Sidecar data is incomplete. Restore its configuration/identity from backup before updating; nothing has been replaced.'
    [ -z "$origin" ] && [ "$port_set" = no ] || fail 'This installation already has an address. Update without --admin-origin/--port to keep its settings.'
fi
if [ "$existing" = no ] && [ -f "$unit" ]; then
    fail 'A Sidecar service is already installed but its data is missing. Restore its data before updating; a new identity will not be created.'
fi
if id sidecar >/dev/null 2>&1; then
    [ "$(id -u sidecar)" != 0 ] && [ "$(id -gn sidecar)" = sidecar ] || fail 'The existing sidecar account is not a dedicated service account.'
elif [ "$existing" = yes ]; then
    fail 'Sidecar data exists but its service account is missing. Restore the account before updating.'
fi
if [ -f "$unit" ]; then
    # Do not commandeer a same-named unrelated service. Drop-ins remain untouched.
    grep -qx 'ExecStart=/usr/local/bin/sidecar serve --data-dir /var/lib/sidecar' "$unit" || fail 'This sidecar.service uses a custom command. Update it manually using INSTALL.md.'
fi

printf '\nSidecar installation\n\n'
if [ "$existing" = yes ]; then
    echo 'Update this installation. Settings, password, identity and cache will be kept.'
    echo 'Playback will briefly stop while Sidecar restarts.'
else
    if [ "$yes" = no ] && [ "$port_set" = no ]; then
        printf 'Sidecar port [7843]: '
        IFS= read -r reply || exit 1
        port=${reply:-7843}
    fi
    case "$port" in ''|*[!0-9]*) fail 'Use a port number from 1 to 65535.' ;; esac
    [ "$port" -ge 1 ] && [ "$port" -le 65535 ] || fail 'Use a port number from 1 to 65535.'
    if [ -z "$origin" ] && [ "$yes" = no ]; then
        host=$(hostname -I 2>/dev/null | awk '{print $1}')
        if [ -z "$host" ]; then host=$(hostname); fi
        case "$host" in *:*) host="[$host]" ;; esac
        default="https://$host:$port"
        printf 'Browser address [%s]: ' "$default"
        IFS= read -r reply || exit 1
        origin=${reply:-$default}
    fi
    origin=${origin%/}
    origin=${origin%:443}
    case "$origin" in https://?*) ;; *) fail 'Enter the HTTPS address you will open in your browser, including the port.' ;; esac
    authority=${origin#https://}
    case "$authority" in *[/?#@]*|*[[:space:]]*) fail 'Use an HTTPS host and port only, without a path, username or trailing slash.' ;; esac
    printf '\nInstall Sidecar at %s\n' "$origin"
    echo 'It will run as the sidecar user and start on boot.'
fi
if [ "$yes" = no ]; then
    printf 'Continue? [Y/n]: '
    IFS= read -r reply || exit 1
    case "$reply" in ''|y|Y|yes|Yes) ;; *) echo 'Nothing changed.'; exit 0 ;; esac
fi

log=$(mktemp /tmp/sidecar-install.XXXXXX)
result=$(mktemp /tmp/sidecar-setup.XXXXXX)
backup= staged= old_binary= old_unit= changed=no stopped=no success=no was_active=no was_enabled=no
systemctl is-active --quiet sidecar.service && was_active=yes
systemctl is-enabled --quiet sidecar.service 2>/dev/null && was_enabled=yes
cleanup() {
    code=$?
    trap - EXIT HUP INT TERM
    if [ "$success" = no ]; then
        if [ "$changed" = yes ]; then
            echo 'Startup failed; stopping this attempt and restoring any previous program/service.' >&2
            systemctl stop sidecar.service >>"$log" 2>&1 || :
            if [ -n "$old_binary" ]; then
                if ! mv -f "$old_binary" "$binary" >>"$log" 2>&1; then
                    echo "Could not restore the previous binary; it is saved at $old_binary" >&2
                    old_binary=
                fi
            fi
            if [ -n "$old_unit" ]; then
                if ! cp -p "$old_unit" "$unit" >>"$log" 2>&1; then
                    echo "Could not restore the previous unit; it is saved at $old_unit" >&2
                    old_unit=
                fi
            fi
            systemctl daemon-reload >>"$log" 2>&1 || :
            if [ "$was_enabled" = no ]; then systemctl disable sidecar.service >>"$log" 2>&1 || :; fi
        fi
        if [ "$stopped" = yes ] && [ "$was_active" = yes ]; then
            systemctl start sidecar.service >>"$log" 2>&1 || echo 'The previous service could not restart. Check systemctl status sidecar.' >&2
        fi
        printf '\nInstallation did not finish. Details: %s\n' "$log" >&2
        tail -n 12 "$log" >&2
        [ -z "$backup" ] || printf 'Configuration backup: %s\n' "$backup" >&2
    fi
    rm -f "$result"
    [ -z "$staged" ] || rm -f "$staged"
    [ -z "$old_binary" ] || rm -f "$old_binary"
    [ -z "$old_unit" ] || rm -f "$old_unit"
    exit "$code"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM HUP
step() { printf '%s…\n' "$1"; }
run() { "$@" >>"$log" 2>&1; }

step 'Checking dependencies'
needs_packages=no
command -v ffmpeg >/dev/null && command -v ffprobe >/dev/null || needs_packages=yes
[ -f /etc/ssl/certs/ca-certificates.crt ] || needs_packages=yes
if [ "$needs_packages" = yes ]; then
    command -v apt-get >/dev/null || fail 'Install FFmpeg (with libopus), ffprobe and CA certificates, then run this installer again. Automatic dependencies are supported on Debian/Ubuntu.'
    run apt-get update
    run env DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends ffmpeg ca-certificates
fi
run ffmpeg -version
run ffprobe -version
ffmpeg -hide_banner -encoders >"$result" 2>>"$log"
grep -q ' libopus ' "$result" || fail 'FFmpeg needs the libopus encoder. Install a build that provides it, then retry.'

step 'Preparing Sidecar'
run install -d /usr/local/bin
staged=$(mktemp /usr/local/bin/.sidecar-new.XXXXXX)
run install -m 0755 "$package/sidecar" "$staged"
# The archive may be under /root. Run the service account's checks through this
# public executable path, not through an inaccessible owner's home directory.
if [ "$existing" = yes ]; then
    run runuser -u sidecar -- "$staged" status --data-dir "$data"
fi
if ! id sidecar >/dev/null 2>&1; then
    getent group sidecar >/dev/null || run groupadd --system sidecar
    run useradd --system --gid sidecar --home-dir "$data" --no-create-home --shell /usr/sbin/nologin sidecar
fi
if [ "$existing" = no ]; then
    run install -d -m 0700 -o sidecar -g sidecar "$data"
    run runuser -u sidecar -- "$staged" init --data-dir "$data" --listen "0.0.0.0:$port" --admin-origin "$origin"
fi
if [ -f "$binary" ]; then
    old_binary=$(mktemp /usr/local/bin/.sidecar-previous.XXXXXX)
    run cp -p "$binary" "$old_binary"
fi
if [ -f "$unit" ]; then
    old_unit=$(mktemp /tmp/sidecar-unit.XXXXXX)
    run cp -p "$unit" "$old_unit"
fi
if [ "$was_active" = yes ]; then
    stopped=yes
    run systemctl stop sidecar.service
fi
if [ "$existing" = yes ]; then
    run install -d -m 0700 /var/backups/sidecar
    backup=$(mktemp /var/backups/sidecar/configuration.XXXXXX.tar.gz)
    run tar --exclude=./retained --exclude=./working -czf "$backup" -C "$data" .
fi
changed=yes
run mv -f "$staged" "$binary"
staged=
run install -m 0644 "$package/sidecar.service" "$unit"
run systemctl daemon-reload
run systemctl enable sidecar.service
step 'Starting Sidecar'
run systemctl start sidecar.service
# setup-info verifies local HTTPS using this installation's certificate. Its code
# goes to the terminal only after success, never into the dependency/install log.
if ! runuser -u sidecar -- "$binary" setup-info --data-dir "$data" >"$result" 2>>"$log"; then exit 1; fi
run systemctl is-active --quiet sidecar.service
run systemctl is-enabled --quiet sidecar.service
success=yes
cat "$result"
printf '\nInstallation log: %s\n' "$log"
[ -z "$backup" ] || printf 'Configuration backup: %s\n' "$backup"

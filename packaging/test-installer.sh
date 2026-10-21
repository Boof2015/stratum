#!/bin/sh
# DESTRUCTIVE inside its disposable container only: never run on an installed host.
set -eu
[ -f /.dockerenv ] && [ "${SIDECAR_INSTALLER_TEST:-}" = disposable ] && [ "$(id -u)" = 0 ] || {
    echo 'Run only in the documented disposable installer test container.' >&2; exit 1;
}
[ ! -e /var/lib/sidecar/config.json ] || { echo 'Test container must have no Sidecar state.' >&2; exit 1; }
package=$1
# Emulate the user's Proxmox console flow: archive in root's private home. Also
# exercise creation of the service account rather than borrowing the image's.
cp -r "$package" /root/sidecar-installer-package
package=/root/sidecar-installer-package
chmod 700 /root
if id sidecar >/dev/null 2>&1; then userdel sidecar; fi
rm -f /usr/local/bin/sidecar /etc/systemd/system/sidecar.service
mkdir -p /run/systemd/system /usr/local/sbin
# Real filesystem, service account, Sidecar and HTTPS; only systemd is simulated
# because Docker's PID 1 is not systemd. No privileged container or host mounts.
cat > /usr/local/sbin/systemctl <<'SYSTEMCTL'
#!/bin/sh
set -eu
case "$1" in
    is-active) [ -f /tmp/sidecar-test.pid ] && kill -0 "$(cat /tmp/sidecar-test.pid)" 2>/dev/null ;;
    is-enabled) [ -f /tmp/sidecar-test.enabled ] ;;
    enable) touch /tmp/sidecar-test.enabled ;;
    disable) rm -f /tmp/sidecar-test.enabled ;;
    daemon-reload) : ;;
    stop)
        if [ -f /tmp/sidecar-test.pid ]; then
            pid=$(cat /tmp/sidecar-test.pid)
            kill "$pid" 2>/dev/null || :
            for attempt in 1 2 3 4 5 6 7 8 9 10; do
                kill -0 "$pid" 2>/dev/null || break
                sleep .2
            done
            rm -f /tmp/sidecar-test.pid
        fi ;;
    start)
        if [ -f /tmp/sidecar-test.fail-once ]; then rm /tmp/sidecar-test.fail-once; exit 1; fi
        if "$0" is-active --quiet sidecar; then exit 0; fi
        runuser -u sidecar -- /usr/local/bin/sidecar serve --data-dir /var/lib/sidecar >/tmp/sidecar-test-service.log 2>&1 9>&- &
        echo $! >/tmp/sidecar-test.pid ;;
    *) echo "Unexpected service command: $*" >&2; exit 1 ;;
esac
SYSTEMCTL
chmod +x /usr/local/sbin/systemctl
PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin
export PATH
trap 'systemctl stop sidecar >/dev/null 2>&1 || :' EXIT
check() { echo "PASS $*"; }
reject() { if "$@" > /tmp/install-failure.log 2>&1; then echo "Expected failure: $*" >&2; exit 1; fi; }
run_installer() { sh "$package/install.sh" "$@"; }

reject run_installer
[ ! -e /var/lib/sidecar/config.json ]
check 'noninteractive installs require explicit options'
reject run_installer --yes --admin-origin http://localhost:7843
[ ! -e /var/lib/sidecar/config.json ]
check 'bad addresses rejected before creating state'
# Exercise actual terminal prompts, ending in cancellation.
printf '7843\nhttps://localhost:7843\nn\n' | script -q -e -c "sh $package/install.sh" /dev/null > /tmp/install-cancel.log
[ ! -e /var/lib/sidecar/config.json ]
grep -q 'Nothing changed' /tmp/install-cancel.log
check 'interactive defaults and cancellation leave installation untouched'

run_installer --yes --admin-origin https://localhost:7843 > /tmp/install-first.log
systemctl is-active --quiet sidecar
systemctl is-enabled --quiet sidecar
grep -q 'Open: https://localhost:7843' /tmp/install-first.log
grep -Eq '^[0-9a-f]{64}$' /tmp/install-first.log
grep -q 'valid for 30 minutes' /tmp/install-first.log
[ "$(stat -c %U /var/lib/sidecar/private.json)" = sidecar ]
[ "$(stat -c %a /var/lib/sidecar)" = 700 ]
check 'fresh install runs real HTTPS as service user and prints clean address/code'
install_log=$(sed -n 's/^Installation log: //p' /tmp/install-first.log)
code=$(grep -E '^[0-9a-f]{64}$' /tmp/install-first.log)
! grep -q "$code" "$install_log"
[ "$(stat -c %a "$install_log")" = 600 ]
check 'setup code excluded from private installation log'
printf 'disposable installer test password\n' | runuser -u sidecar -- /usr/local/bin/sidecar set-password --data-dir /var/lib/sidecar --password-stdin >/dev/null
before=$(sha256sum /var/lib/sidecar/private.json /var/lib/sidecar/config.json)
mkdir -p /var/lib/sidecar/retained
printf cache-test > /var/lib/sidecar/retained/installer-test.keep
chown -R sidecar:sidecar /var/lib/sidecar/retained
run_installer --yes > /tmp/install-update.log
[ "$before" = "$(sha256sum /var/lib/sidecar/private.json /var/lib/sidecar/config.json)" ]
grep -q 'existing Sidecar password' /tmp/install-update.log
! grep -q 'Setup code' /tmp/install-update.log
[ -f /var/lib/sidecar/retained/installer-test.keep ]
backup=$(sed -n 's/^Configuration backup: //p' /tmp/install-update.log)
[ "$(stat -c %a "$backup")" = 600 ]
tar -tzf "$backup" | grep -q './private.json'
! tar -tzf "$backup" | grep -q retained
check 'updates preserve identity, password, configuration/cache and create private backup'
reject run_installer --yes --admin-origin https://changed:7843
[ "$before" = "$(sha256sum /var/lib/sidecar/private.json /var/lib/sidecar/config.json)" ]
check 'update refuses unintended address changes'
program=$(sha256sum /usr/local/bin/sidecar)
service=$(sha256sum /etc/systemd/system/sidecar.service)
# Harmless appended ELF data makes this candidate observably different, so a
# missing rollback cannot pass merely because both program copies were identical.
printf '\ninstaller candidate\n' >> "$package/sidecar"
printf '\n# installer candidate\n' >> "$package/sidecar.service"
touch /tmp/sidecar-test.fail-once
reject run_installer --yes
[ "$program" = "$(sha256sum /usr/local/bin/sidecar)" ]
[ "$service" = "$(sha256sum /etc/systemd/system/sidecar.service)" ]
[ "$before" = "$(sha256sum /var/lib/sidecar/private.json /var/lib/sidecar/config.json)" ]
runuser -u sidecar -- /usr/local/bin/sidecar setup-info --data-dir /var/lib/sidecar >/dev/null
systemctl is-active --quiet sidecar
check 'failed restart restores previous binary and a working HTTPS service'
mv /var/lib/sidecar/config.json /var/lib/sidecar/config.saved
reject run_installer --yes
[ ! -f /var/lib/sidecar/config.json ]
mv /var/lib/sidecar/config.saved /var/lib/sidecar/config.json
check 'incomplete state is never silently initialized'
chmod 000 /var/lib/sidecar/private.json
reject run_installer --yes
chmod 600 /var/lib/sidecar/private.json
check 'unreadable state is refused without changing ownership'
systemctl stop sidecar
mv /var/lib/sidecar /var/lib/sidecar-test-saved
reject run_installer --yes --admin-origin https://localhost:7843
[ ! -e /var/lib/sidecar/config.json ]
mv /var/lib/sidecar-test-saved /var/lib/sidecar
check 'missing data from an installed service never creates a replacement identity'
printf '\nInstaller checks passed.\n'

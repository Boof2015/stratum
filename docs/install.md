# Install Sidecar

These are development packages for Linux, alongside an existing Navidrome.
Docker and a standalone binary use the same setup, dashboard and data format.
Astra can discover Sidecar through Navidrome and enable Stratum per server.
The current audio limits still apply (FLAC/WAV, mono/stereo, 44.1/48/96 kHz).

## Docker

From a checkout of Stratum, with Docker Compose available:

```sh
cp .env.example .env
# Edit .env: use the HTTPS address you will open, such as
# SIDECAR_ADMIN_ORIGIN=https://192.168.1.20:7843
docker compose up -d --build
docker compose exec sidecar sidecar setup-info --data-dir /var/lib/sidecar
```

Open that address. Sidecar creates its own HTTPS certificate, so your browser
may show a trust warning. The last command checks local HTTPS and prints the
address and a 30-minute setup code on its own line. Enter the code and choose
your owner password, then follow the Navidrome, listener-address and discovery
steps. No Astra installation is
needed to set up the server.

The image includes FFmpeg and runs as UID/GID **10001**, without root privileges.
The named volume at `/var/lib/sidecar` holds identity, configuration and audio
storage. Keep it when replacing the container. `docker compose down` keeps it;
adding `--volumes` deletes it. The example uses a read-only container filesystem
and a small temporary filesystem for encoder scratch files.

`SIDECAR_ADMIN_ORIGIN` is used **only on an empty volume**. Changing it later
doesn't overwrite saved settings. The origin must include HTTPS and the browser's
port, with no path or trailing slash. To initialize several administration
origins before the first start, use:

```sh
docker compose run --rm sidecar init --listen 0.0.0.0:7843 \
  --admin-origin https://music.home.lan:7843 \
  --admin-origin https://192.168.1.20:7843
docker compose up -d
```

Sidecar listens on container port 7843. If you change the published host port,
include that port in the administration origin and in listener addresses.
For an existing installation, stop Sidecar and edit `listen`/`admin_origins` in
its private `config.json`, keeping its other fields and file permissions. Renew
the certificate after changing administration hostnames. `init` never overwrites
an existing configuration. Partial or damaged state stops startup for repair.

Navidrome's address in setup must be reachable **from the container**. When both
services share a Compose network, use its service name, for example
`http://navidrome:4533`. Otherwise use its reachable LAN/tailnet/proxy address.
`localhost` inside the container refers to Sidecar, not another container or the
host. Joining an existing network is optional; no host networking is required.
Listener addresses are the addresses clients use, usually different from that
internal Navidrome address.

For a bind mount instead of the named volume, create a dedicated directory owned
by UID/GID 10001 with mode 700 and mount it at `/var/lib/sidecar`. The image does
not recursively change ownership or repair permissions on an existing directory.
Use a local filesystem that supports Unix permissions, atomic rename and locks.

## Standalone Linux / systemd

The archive contains `sidecar`, `install.sh`, `sidecar.service`, `LICENSE` and this
guide. Build it on the target Linux architecture with Rust 1.88+:

```sh
sh packaging/build-linux.sh
```

Or export a Linux archive using Docker (native architecture of your Docker host):

```sh
docker build --target archive --output type=local,dest=dist .
```

Each archive has a companion `.sha256` file. From `dist`, verify it with
`sha256sum -c sidecar-0.1.0-linux-aarch64.tar.gz.sha256` (use your actual filename),
then extract the archive with `tar -xzf`. Nothing is installed by the build.

BuildKit's `--platform linux/amd64` or `--platform linux/arm64` selects another
architecture. The Docker build uses a native compiler and the target's Rust/C
toolchains, following [Docker's cross-compilation approach](https://docs.docker.com/build/building/multi-platform/#cross-compilation).
The runtime image's package installation still needs emulation or a matching
builder when its architecture differs. Archives target glibc Linux; use
Debian 12/Ubuntu 24.04 or newer for the Docker-built binary. An archive built on
another distribution inherits that system's libc requirements. FFmpeg is a
separate runtime dependency and must provide `ffmpeg`, `ffprobe` and `libopus`.

On a Debian/Ubuntu server or LXC, run this from the extracted archive:

```sh
sudo ./install.sh
```

If you are already root (for example in the Proxmox console), use `./install.sh`.
It asks for the Sidecar port and browser address, offering detected defaults,
then confirms before installing. It installs missing FFmpeg/CA certificates,
creates a dedicated service account, and starts Sidecar on boot. Navidrome is
left alone. Other systemd distributions can use the installer with FFmpeg,
ffprobe, libopus, CA certificates and util-linux already installed.

After checking Sidecar's local HTTPS service, it prints:

```text
Sidecar is running.

Open: https://192.168.1.20:7843

Setup code (valid for 30 minutes):

<your one-time code>

Enter this code in your browser and choose your Sidecar password.
```

Dependency output goes to a private log whose path is printed at the end; the
setup code is not stored there. If setup was already completed, it asks you to
sign in with your existing password instead. A local check cannot verify your
firewall, DNS or reverse proxy from another computer.

For unattended installation, supply the address explicitly:

```sh
sudo ./install.sh --yes --admin-origin https://192.168.1.20:7843
```

Use `--port 7844` to change the listen port on a **new** installation. The browser
address may use a different port when a reverse proxy forwards it. The service
runs without root/capabilities; no audio device or music-directory mount is
needed. Its unit avoids mount-namespace settings for unprivileged LXC support.
The installer never reinitializes incomplete or unreadable existing state.

## Recovery and updates

Recover an expired first-setup code, or replace an established owner password:

```sh
# Docker: these exec commands run as the image's non-root service user.
docker compose exec sidecar sidecar setup-info --data-dir /var/lib/sidecar
docker compose exec -it sidecar sidecar reset-password --data-dir /var/lib/sidecar

# Standalone:
sudo -u sidecar sidecar setup-info --data-dir /var/lib/sidecar
sudo -u sidecar sidecar reset-password --data-dir /var/lib/sidecar
```

The password prompt is hidden. Recovery signs out dashboard sessions while
preserving connections, cache and identity. It does not change Navidrome accounts.
For automation, use `--password-stdin` with protected input, never a password in
command arguments or Compose environment variables.

Before a manual or Docker update, stop Sidecar and back up its data directory,
keeping ownership and private permissions. The guided installer does this for
you, excluding cached audio. `private.json` includes the identity key and owner
password hash; store backups privately. `config.json`, `private.json` and the
discovery state belong together. Retained audio can be omitted from a backup;
working files are disposable. Never start two installations from the same backup
at once. Restoring without the identity changes what clients must trust.

For Docker, rebuild from the chosen checkout and run `docker compose up -d`
with the same volume. For standalone, extract the new verified archive and run
`sudo ./install.sh` again. It keeps the saved address, password, identity and
cache, makes a private configuration backup in `/var/backups/sidecar`, and replaces
the program and service unit. If startup fails, it restores the previous binary
and unit and restarts a previously running service. It preserves systemd drop-ins;
a unit with a custom command needs a manual update. `--yes` skips the interactive
confirmation. Do not pass new address/port options on an update.
A graceful stop cancels streaming and cleans up workers;
restarting preserves settings/identity but listeners and dashboard sessions sign
in again. A crash releases process locks; stale working files are recovered at
startup. The service's 20-second stop allowance exceeds its normal cleanup time.

Certificates last one year. Renew using the existing identity key, then restart:

```sh
docker compose exec sidecar sidecar renew-certificate --data-dir /var/lib/sidecar
docker compose restart sidecar
# Standalone equivalent:
sudo -u sidecar sidecar renew-certificate --data-dir /var/lib/sidecar
sudo systemctl restart sidecar
```

For reverse proxies, keep the upstream HTTPS connection configured explicitly.
Listener proxy paths and administration origins are separate; see the repository's
`docs/owner-streaming.md` and `docs/owner-foundation.md`. A saved listener URL does
not configure DNS, firewall rules or a proxy.

## Packaging checks (from a checkout)

```sh
docker build --target test --progress=plain .
docker build -t astra-sidecar:local .
python3 experiments/packaging.py --image astra-sidecar:local
```

The test target runs Rust tests on the builder's native architecture as a
non-root Linux user with real FFmpeg and
validates the systemd unit. The packaging smoke test uses disposable Docker
volumes and loopback-only ports with hard timeouts and cleanup. It does not
modify an installed service, access an existing Navidrome or play audio.
It requires Python with TLS 1.3 support. Add `--archive dist/<archive>.tar.gz`
to also check a standalone archive built from the same checkout and architecture.
The guided installer has a separate isolated Linux check:

```sh
docker build --target installer-test -t astra-sidecar:installer-test .
docker run --rm --init --network none astra-sidecar:installer-test
```

It uses real files, accounts and Sidecar HTTPS, with a simulated `systemctl`;
no privileged container or existing installation is used. It covers new installs,
interactive cancellation, preservation on update, backup, failure rollback and
damaged state. Unit validation and this simulation are not a systemd boot test;
that still needs a Linux host/LXC running systemd.

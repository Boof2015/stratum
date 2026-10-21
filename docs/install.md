# Install Sidecar

These are development packages for Linux, alongside an existing Navidrome.
Docker and a standalone binary use the same setup, dashboard and data format.
Astra's normal discovery/enable flow is still being connected; packaging does
not remove the current audio limits (FLAC/WAV, mono/stereo, 44.1/48/96 kHz).

## Docker

From a checkout of Stratum, with Docker Compose available:

```sh
cp .env.example .env
# Edit .env: use the HTTPS address you will open, such as
# SIDECAR_ADMIN_ORIGIN=https://192.168.1.20:7843
docker compose up -d --build
docker compose logs sidecar
```

Open that address. Sidecar creates its own HTTPS certificate, so your browser
will show a trust warning. The logs show its certificate fingerprint and a
30-minute setup code. Enter the code and choose your owner password, then follow
the Navidrome, listener-address and discovery steps. No Astra installation is
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

The archive contains `sidecar`, `sidecar.service` and this guide. Build it on the
target Linux architecture with Rust 1.88+:

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

On a Debian/Ubuntu server or LXC, install FFmpeg and a dedicated service user.
Run these from the extracted archive directory (skip `useradd` if the account
already exists):

```sh
sudo apt-get update
sudo apt-get install --no-install-recommends ffmpeg ca-certificates
sudo useradd --system --user-group --home-dir /var/lib/sidecar \
  --no-create-home --shell /usr/sbin/nologin sidecar
sudo install -m 0755 sidecar /usr/local/bin/sidecar
sudo install -d -m 0700 -o sidecar -g sidecar /var/lib/sidecar
sudo -u sidecar /usr/local/bin/sidecar init --data-dir /var/lib/sidecar \
  --listen 0.0.0.0:7843 --admin-origin https://192.168.1.20:7843
sudo install -m 0644 sidecar.service /etc/systemd/system/sidecar.service
sudo systemctl daemon-reload
sudo systemctl enable --now sidecar
sudo journalctl -u sidecar -n 30 --no-pager
```

Replace the example address with your server's. Use the setup code from the
journal to choose the owner password in your browser. Alternatively, run
`sidecar set-password --data-dir /var/lib/sidecar` as the service user before
starting it. The service starts without interactive prompts and starts on boot.
It runs without root/capabilities; no sound device or music-directory mount is
needed. Its unit avoids mount-namespace sandbox settings so it also fits typical
unprivileged LXC installations.

## Recovery and updates

Recover an expired first-setup code, or replace an established owner password:

```sh
# Docker: these exec commands run as the image's non-root service user.
docker compose exec sidecar sidecar setup-code --data-dir /var/lib/sidecar
docker compose exec -it sidecar sidecar reset-password --data-dir /var/lib/sidecar

# Standalone:
sudo -u sidecar sidecar setup-code --data-dir /var/lib/sidecar
sudo -u sidecar sidecar reset-password --data-dir /var/lib/sidecar
```

The password prompt is hidden. Recovery signs out dashboard sessions while
preserving connections, cache and identity. It does not change Navidrome accounts.
For automation, use `--password-stdin` with protected input, never a password in
command arguments or Compose environment variables.

Before updating, stop Sidecar and back up its data directory, keeping ownership
and private permissions. `private.json` includes the identity key and owner
password hash; store backups privately. `config.json`, `private.json` and the
discovery state belong together. Retained audio can be omitted from a backup;
working files are disposable. Never start two installations from the same backup
at once. Restoring without the identity changes what clients must trust.

For Docker, rebuild from the chosen checkout and run `docker compose up -d`
with the same volume. For standalone, stop the service, replace the binary,
then start it again. A graceful stop cancels streaming and cleans up workers;
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
Unit validation is not a systemd boot test; that still needs a Linux host/LXC
running systemd.

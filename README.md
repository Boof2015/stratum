# Stratum

Stratum lets compatible music players choose streaming quality segment by segment.
When conditions change, the player can adjust upcoming audio while keeping the
segments it has already downloaded.

**Astra Sidecar** is the server implementation in this repository. It runs
alongside an existing Navidrome server, using its music library, accounts and
permissions. Navidrome continues to manage your library; Sidecar handles Stratum
audio delivery.

Stratum and Sidecar are in development. [Astra desktop](https://github.com/Boof2015/astra)
has Stratum support on its **0.8.1 development branch**. The protocol is still
experimental.

## How it works

A compatible player requests independently decodable segments on a shared audio
timeline. Each request can use a different quality, so the player can respond to a
slower connection without replacing the audio it already has. Quality selection
happens in the client; Sidecar provides the available representations.

In Astra, Stratum supports automatic quality adjustment, fixed quality, seeking,
and gapless playback for supported tracks. Astra also caches downloaded segments
locally. Sidecar has its own configurable cache to reuse encoded segments and
reduce repeat conversion work.

## Install Sidecar

You need an existing Navidrome server, somewhere to run Sidecar, and a
Stratum-compatible player. Sidecar can run on the same machine as Navidrome or
another host that can reach it. It retrieves music through Navidrome's API, so it
doesn't need a music-directory mount.

| Installation | Setup |
| --- | --- |
| [Docker Compose](docs/install.md#docker) | Build the image from this repository and start it with Compose. FFmpeg is included in the image. |
| [Standalone Linux / systemd](docs/install.md#standalone-linux--systemd) | Build an x86-64 or ARM64 archive, then run the guided installer. It sets up a service that starts on boot. |

The [installation guide](docs/install.md) currently covers builds from a checkout,
including exporting a Linux archive with Docker. The standalone installer can
install missing dependencies on Debian/Ubuntu and works in a systemd-based LXC.
Other supported systemd distributions need the dependencies installed first.

Both installation paths provide a browser address and a one-time setup code.
Open the address, enter the code, and choose your Sidecar administration password.
The setup then asks for:

1. The Navidrome address **Sidecar can reach**.
2. The HTTPS Sidecar address or addresses **your players can reach**.
3. Automatic discovery or manual connection.

Direct LAN/tailnet connections and reverse proxies are supported. Sidecar's own
HTTPS certificate can produce a browser trust warning during setup. See the
installation guide for address, container-network and proxy configuration.

You don't need Astra installed to set up Sidecar. The administration password is
separate from your Navidrome listener account.

## Connect Astra

In an Astra desktop build with Stratum support, add your Navidrome server as usual,
then open **Settings → Servers → your server → Stratum**. Stratum is opt-in per
server.

- **Discovery:** enable discovery during Sidecar setup, then turn on Stratum in
  Astra. Sidecar publishes its connection details in Navidrome's internet radio
  list; other apps may display that entry as a radio station.
- **Manual connection:** choose **Connect manually…** and enter Sidecar's HTTPS
  address, including any path. Astra asks before trusting a new self-signed
  identity and remembers that decision.

Astra uses your existing Navidrome sign-in. You don't enter the Sidecar
administration password in the player. Unsupported tracks or unavailable Stratum
connections use Astra's usual server streaming path, with the reason shown in
settings. Turning Stratum off keeps a saved manual connection for later use.

## Current support

| Area | Supported |
| --- | --- |
| Music server | Navidrome |
| Packaged server platforms | Linux x86-64 and ARM64, through Docker or standalone glibc builds |
| Source formats | FLAC and 16/24-bit integer PCM WAV |
| Source audio | Mono or stereo, 16/24-bit, at 44.1, 48 or 96 kHz |
| Delivery qualities | Lossless FLAC and Opus at 64, 128, 192, 256 or 320 kbps |
| Source ceiling | Up to 2 GiB and eight hours, subject to available working storage and transfer deadlines |
| Astra output modes | Standard and Exclusive; Bit-perfect uses lossless delivery |

Lossless delivery preserves the decoded source PCM, bit depth and sample rate.
It re-encodes that audio as FLAC; it does not preserve the original file bytes.
Opus is lossy and uses a 48 kHz codec clock.

**Sidecar currently acquires the complete original from Navidrome before serving
segments.** The player receives segments, but the Sidecar-to-Navidrome connection
still needs to fetch that original. A fast connection between those two services
helps startup. Sidecar's retained cache saves encoding work; it doesn't eliminate
that source fetch.

Working storage is separate from the retained cache. Its default 512 MB budget
can limit large tracks before the source ceiling is reached; adjust it under
**Settings → Resources**. The current service allows four prepared playbacks in
total, with at most two per listener session. See [streaming limits](docs/owner-streaming.md#resource-bounds)
and [resource controls](docs/owner-resources.md) for the details.

Lossy source files, other sample rates and multichannel sources aren't currently
supported by Sidecar. Windows server support is not implemented.

## Configuration and maintenance

Sidecar's dashboard shows live connections, prepared streams, delivery activity
and storage use. Settings controls its connections, retained cache and conversion
resources.

- [Connection setup](docs/owner-setup.md) and [discovery maintenance](docs/owner-discovery.md)
- [Audio cache](docs/owner-cache.md) and [working storage / conversion limits](docs/owner-resources.md)
- [Dashboard readings](docs/owner-dashboard.md)
- [Updates, backups, password recovery and certificate renewal](docs/install.md#recovery-and-updates)

## Development and protocol notes

Building from source requires Rust 1.88 or newer. Audio processing requires
`ffmpeg` and `ffprobe`, with the `libopus` encoder available. Development and tests
run on macOS and Linux.

```sh
cargo build --locked --bin sidecar
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test -- --test-threads=1
python3 -B -m unittest discover -s experiments -p 'test_*.py'
```

The Rust tests exercise actual FFmpeg encodes and decodes. These checks don't
launch a player or produce audible playback.

Existing technical notes cover [installed HTTPS delivery](docs/owner-streaming.md),
[audio timing and decoding](docs/audio-proof.md), [session authentication](docs/access-proof.md),
[segment responses and quality negotiation](docs/authenticated-audio-proof.md),
and [discovery metadata](docs/owner-discovery.md#metadata).
These documents include earlier development checkpoints and their limitations;
they are not yet a consolidated, stable protocol specification.

The repository also retains its proof programs and experiment reports:

- [Source fetching from disposable Navidrome](docs/navidrome-proof.md)
- [Bandwidth changes and real-time delivery](docs/live-delivery-proof.md)
- [Seeking and skipping](docs/seek-proof.md)
- [Gapless track handoffs](docs/handoff-proof.md) and [mixed sample rates](docs/mixed-rate-proof.md)
- [Sustained playback and recovery](docs/session-proof.md)

Those reports document what each experiment tested. Some use the earlier
`stratum-poc` or loopback `sidecar-access` programs rather than the installed
`sidecar` service.

## License

The Stratum engine and Astra Sidecar implementation are licensed under the
[GNU Affero General Public License, version 3 only](https://www.gnu.org/licenses/agpl-3.0.html)
(`AGPL-3.0-only`).

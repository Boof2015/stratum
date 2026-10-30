# Sidecar releases

GitHub Actions builds and tests Linux x86-64 and ARM64 packages. Ordinary pushes
to `main`, pull requests and manual **Sidecar** workflow runs produce test
artifacts. A pushed `v…` tag also publishes the tested packages to GitHub Releases
and the tested images to `ghcr.io/boof2015/stratum`.

The workflow is [sidecar.yml](../.github/workflows/sidecar.yml). Publishing is a
separate job, with write permissions only there, after both architectures pass.
No registry password or personal access token is needed: it uses the workflow's
`GITHUB_TOKEN`.

## Checks before publication

Each architecture runs on a matching GitHub-hosted Linux runner. Docker's Debian
12 builder provides the same glibc baseline for both. The checks include:

- Rust formatting, Clippy, and the Rust suite with real FFmpeg, as a non-root user.
- The Python experiment tests and validation of the systemd unit.
- Installation from the generated archive: first setup, interactive cancellation,
  update preservation, backups, failed-start rollback and refusal of damaged state.
- The runtime image: verified HTTPS, owner enrollment, restart, password recovery,
  cache/resource configuration, and availability of FFmpeg, ffprobe and libopus.
- Archive checksums, license inclusion, and a byte-for-byte comparison of the
  standalone executable with the executable in the tested runtime image.

These tests use disposable containers and volumes. They do not contact a personal
Navidrome or play audio. The installer check simulates `systemctl`; a real LXC
installation and boot test remain part of release testing.

CI preserves each tested image as an artifact. The publishing job loads those
images rather than rebuilding them, verifies their IDs, architecture, version and
commit, and joins their pushed digests into one image with both architectures.
It also checks that both archives and images came from the tagged build.

## First CI run

Commit and push the workflow to `main`, then open the repository's **Actions →
Sidecar** run. This runs the checks and leaves downloadable `packages-amd64` and
`packages-arm64` artifacts. It does not publish a release or Docker image.

Download a package artifact, extract its outer Actions ZIP, and verify the
included `.sha256` file before using the Linux archive. Test it in the LXC using
the [normal installer](install.md#standalone-linux--systemd).

## Publishing a version

1. Set the package version in `Cargo.toml` and update the root package entry in
   `Cargo.lock`, for example with `cargo check`. Commit both. Choose a prerelease
   version such as `0.1.0-rc.1` while testing; the tag must match it exactly.
2. Push the commit and let its CI checks finish.
3. Create and push the matching tag. For that example:

   ```sh
   git tag -a v0.1.0-rc.1 -m 'Astra Sidecar 0.1.0-rc.1'
   git push origin v0.1.0-rc.1
   ```

The tag run performs the package checks again. A version mismatch fails before
the builds. Prerelease tags become GitHub prereleases; versions without a
prerelease suffix become ordinary releases. SemVer build metadata (`+…`) is not
accepted because it cannot be used unchanged in a Docker tag.

Published files include:

| File | Purpose |
| --- | --- |
| `sidecar-VERSION-linux-x86_64.tar.gz` | Intel/AMD 64-bit Linux installer archive |
| `sidecar-VERSION-linux-aarch64.tar.gz` | ARM64 Linux installer archive |
| `*.tar.gz.sha256` | Individual binary-archive checksums |
| `stratum-VERSION-source.tar.gz` | Exact tracked project source, including Cargo.lock and build scripts |
| `compose.yaml`, `sidecar.env.example` | Run the published image without a source checkout |
| `container-manifest.json`, `container-images.txt` | Published image index and its architecture digests |
| `SHA256SUMS` | Checksums for release downloads |

The source archive is the tagged project, not a bundle of Rust toolchains,
registry crates or Debian packages. Builds use the dependencies described by
Cargo.lock and the Dockerfile.

The main image is `ghcr.io/boof2015/stratum:VERSION`, without the tag's leading
`v`. Docker selects x86-64 or ARM64 automatically. Architecture-specific
`:VERSION-amd64` and `:VERSION-arm64` tags also exist. There is no floating Docker
`latest` tag; installation and updates select a version explicitly.

After the first image publication, set the GitHub **stratum** package's visibility
to **Public** so users can pull it without signing in. New GHCR packages start
private even when their repository is public. See GitHub's
[package visibility instructions](https://docs.github.com/en/packages/learn-github-packages/configuring-a-packages-access-control-and-visibility).

## Interrupted publication

The GitHub release stays a draft until image publication, manifest verification
and asset uploads finish. If publishing fails, rerun the failed job while its
14-day build artifacts are available. It can complete that draft. Registry
uploads and GitHub releases are separate operations: image tags can exist while
the GitHub release is still a draft.

An already published GitHub release is refused, rather than overwritten. Use a
new version for a correction. Do not move published tags. If artifacts have
expired before a draft finishes, rerun the complete workflow so its images and
archives are built and tested together again.

The workflow definitions and release validation can be checked locally without
running Docker:

```sh
actionlint .github/workflows/sidecar.yml
python3.11 -B -m unittest discover -s packaging -p 'test_*.py'
bash -n packaging/publish.sh
```

The publishing script requires Linux/Bash 4+, Python 3.11+, Docker Buildx and the
GitHub CLI; the publishing runner provides these. **Do not run it as a local
smoke test:** it writes to GitHub Releases and GHCR.

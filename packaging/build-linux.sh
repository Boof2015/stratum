#!/bin/sh
# Native Linux development archive. Does not install or publish anything.
set -eu
if [ "$(uname -s)" != Linux ]; then
    echo 'Build this archive on Linux (or in the documented Linux builder container).' >&2
    exit 1
fi
case "${SIDECAR_TARGET_ARCH:-$(uname -m)}" in
    x86_64|amd64) arch=x86_64 ;;
    aarch64|arm64) arch=aarch64 ;;
    *) echo 'Supported archive architectures: x86_64 and aarch64.' >&2; exit 1 ;;
esac
[ "$#" -le 1 ] || { echo 'Usage: packaging/build-linux.sh [OUTPUT_DIRECTORY]' >&2; exit 1; }
root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
output=${1:-"$root/dist"}
mkdir -p "$output"
output=$(CDPATH= cd -- "$output" && pwd)
build=${SIDECAR_BUILD_TARGET_DIR:-"$root/target/package"}
target=$arch-unknown-linux-gnu
cargo build --locked --release --manifest-path "$root/Cargo.toml" --target-dir "$build" --target "$target" --bin sidecar
version=$(cargo pkgid --manifest-path "$root/Cargo.toml")
version=${version##*#}
version=${version##*@}
name=sidecar-$version-linux-$arch
stage=$(mktemp -d)
trap 'rm -rf "$stage"' EXIT HUP INT TERM
mkdir "$stage/$name"
install -m 0755 "$build/$target/release/sidecar" "$stage/$name/sidecar"
install -m 0644 "$root/packaging/sidecar.service" "$stage/$name/sidecar.service"
install -m 0644 "$root/docs/install.md" "$stage/$name/INSTALL.md"
tar -C "$stage" -czf "$output/$name.tar.gz" "$name"
(cd "$output" && sha256sum "$name.tar.gz" > "$name.tar.gz.sha256")
printf 'Built %s\n' "$output/$name.tar.gz"

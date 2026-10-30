#!/bin/bash
# Called only after both native CI jobs pass. Never rebuilds their artifacts.
set -euo pipefail
: "${RELEASE_TAG:?Run from the manually requested publish job}"
: "${RELEASE_COMMIT:?Missing commit}"
: "${GITHUB_REPOSITORY:?Missing repository}"
version=$(python3 packaging/release.py version --tag "$RELEASE_TAG")
image="ghcr.io/${GITHUB_REPOSITORY,,}"
assets=release-assets
python3 packaging/release.py verify --tag "$RELEASE_TAG" --commit "$RELEASE_COMMIT" --directory artifacts --output "$assets"

# Do not overwrite a published release. A partial draft can be completed on retry.
if draft=$(gh release view "$RELEASE_TAG" --repo "$GITHUB_REPOSITORY" --json isDraft --jq .isDraft); then
    [[ "$draft" == true ]] || { echo 'This version is already published. Use a new version.' >&2; exit 1; }
else
    gh release create "$RELEASE_TAG" --repo "$GITHUB_REPOSITORY" --verify-tag --draft \
        --title "Astra Sidecar $version" --generate-notes
fi

digests=()
for arch in amd64 arm64; do
    gzip -dc "artifacts/image-$arch/image.tar.gz" | docker load
    python3 packaging/release.py check-image --tag "$RELEASE_TAG" --arch "$arch" \
        --commit "$RELEASE_COMMIT" --directory "artifacts/packages-$arch"
    target="$image:$version-$arch"
    docker tag sidecar:ci "$target"
    docker push "$target"
    # Use immutable registry digests when joining the two architectures.
    digest=$(docker image inspect "$target" --format '{{range .RepoDigests}}{{println .}}{{end}}' | awk -v prefix="$image@sha256:" 'index($0, prefix) == 1 {print}')
    [[ "$digest" =~ ^ghcr\.io/[^[:space:]]+@sha256:[0-9a-f]{64}$ ]] || { echo 'Missing unique pushed image digest.' >&2; exit 1; }
    digests+=("$digest")
done
docker buildx imagetools create --tag "$image:$version" "${digests[@]}"
docker buildx imagetools inspect "$image:$version" --raw > "$assets/container-manifest.json"
python3 - "$assets/container-manifest.json" "${digests[@]}" <<'PY'
import json, sys
from pathlib import Path
index = json.loads(Path(sys.argv[1]).read_text())
actual = {(m['platform']['os'], m['platform']['architecture']): m['digest'] for m in index['manifests']}
expected = {('linux', arch): ref.split('@')[1] for arch, ref in zip(['amd64', 'arm64'], sys.argv[2:])}
if actual != expected or len(index['manifests']) != 2:
    raise SystemExit('Published image manifest does not match both tested architectures.')
PY
printf '%s\n' "$image:$version" "${digests[@]}" > "$assets/container-images.txt"

# Exact project source for the tag, including its locked dependency manifest and
# build scripts. This is the tracked repository, never a developer working tree.
git archive --format=tar.gz --prefix="stratum-$version/" "$RELEASE_COMMIT" > "$assets/stratum-$version-source.tar.gz"
cp compose.release.yaml "$assets/compose.yaml"
printf 'SIDECAR_IMAGE=%s\nSIDECAR_VERSION=%s\nSIDECAR_ADMIN_ORIGIN=https://192.168.1.20:7843\n' "$image" "$version" > "$assets/sidecar.env.example"
(cd "$assets" && sha256sum ./*.tar.gz compose.yaml sidecar.env.example container-manifest.json container-images.txt > SHA256SUMS)
gh release upload "$RELEASE_TAG" "$assets"/* --repo "$GITHUB_REPOSITORY" --clobber
prerelease=false
if [[ "$version" == *-* ]]; then prerelease=true; fi
gh release edit "$RELEASE_TAG" --repo "$GITHUB_REPOSITORY" --draft=false --prerelease="$prerelease"

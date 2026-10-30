#!/usr/bin/env python3
"""Validate release identity and the artifacts passed between CI jobs (Python 3.11+)."""
import argparse
import hashlib
import json
from pathlib import Path
import re
import shutil
import subprocess
import tomllib

ROOT = Path(__file__).resolve().parents[1]
ARCHES = {"amd64": "x86_64", "arm64": "aarch64"}


def version(root=ROOT, tag=None):
    value = tomllib.loads((root / "Cargo.toml").read_text())["package"]["version"]
    # Docker tags cannot contain SemVer build metadata. Keep release names portable.
    match = re.fullmatch(r"(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)(?:-([0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*))?", value)
    if not match or len(value) > 100:
        raise ValueError("Use a SemVer version without build metadata in Cargo.toml.")
    if match[4] and any(part.isdigit() and len(part) > 1 and part[0] == "0" for part in match[4].split(".")):
        raise ValueError("Numeric prerelease identifiers cannot have leading zeroes.")
    lock = tomllib.loads((root / "Cargo.lock").read_text())
    package = tomllib.loads((root / "Cargo.toml").read_text())["package"]["name"]
    local = [p for p in lock["package"] if p["name"] == package and "source" not in p]
    if len(local) != 1 or local[0]["version"] != value:
        raise ValueError("Cargo.lock and Cargo.toml must agree on the package version.")
    if tag is not None and tag != "v" + value:
        raise ValueError(f"Release tag must be v{value}, matching Cargo.toml and Cargo.lock.")
    return value


def digest(path):
    with path.open("rb") as source:
        return hashlib.file_digest(source, "sha256").hexdigest()


def commit(value):
    if not re.fullmatch(r"[0-9a-f]{40}", value):
        raise ValueError("Expected the full Git commit ID.")
    return value


def archive_name(value, arch):
    return f"sidecar-{value}-linux-{ARCHES[arch]}.tar.gz"


def inspect_image(name):
    return json.loads(subprocess.check_output(["docker", "image", "inspect", name], text=True))[0]


def check_image(image, arch, value, revision, image_id=None):
    labels = image["Config"].get("Labels") or {}
    if (image["Os"] != "linux" or image["Architecture"] != arch
            or labels.get("org.opencontainers.image.version") != value
            or labels.get("org.opencontainers.image.revision") != revision
            or labels.get("org.opencontainers.image.licenses") != "AGPL-3.0-only"
            or (image_id is not None and image["Id"] != image_id)):
        raise ValueError("Image identity does not match the tested release artifact.")


def record(directory, arch, value, revision, image_name, image_file=None):
    archive = directory / archive_name(value, arch)
    checksum = digest(archive)
    if (directory / (archive.name + ".sha256")).read_text().strip() != f"{checksum}  {archive.name}":
        raise ValueError("Archive checksum does not match its checksum file.")
    image = inspect_image(image_name)
    check_image(image, arch, value, revision)
    data = {"version": value, "commit": revision, "arch": arch,
            "archive": archive.name, "sha256": checksum, "image_id": image["Id"],
            "image_sha256": digest(image_file) if image_file else None}
    (directory / "build.json").write_text(json.dumps(data, indent=2) + "\n")


def verify(artifacts, output, value, revision):
    # Validate both architectures before copying any release assets.
    files = []
    for arch in ARCHES:
        directory = artifacts / ("packages-" + arch)
        data = json.loads((directory / "build.json").read_text())
        expected = archive_name(value, arch)
        if any(data.get(key) != wanted for key, wanted in
               [("version", value), ("commit", revision), ("arch", arch), ("archive", expected)]):
            raise ValueError(f"{arch} package is from a different build or version.")
        archive = directory / expected
        checksum = digest(archive)
        if data.get("sha256") != checksum or (directory / (expected + ".sha256")).read_text().strip() != f"{checksum}  {expected}":
            raise ValueError(f"{arch} archive checksum failed.")
        if not re.fullmatch(r"sha256:[0-9a-f]{64}", data.get("image_id", "")):
            raise ValueError(f"{arch} image ID is invalid.")
        if digest(artifacts / ("image-" + arch) / "image.tar.gz") != data.get("image_sha256"):
            raise ValueError(f"{arch} image checksum failed.")
        files.extend([archive, directory / (expected + ".sha256")])
    output.mkdir(parents=True, exist_ok=True)
    for path in files:
        shutil.copyfile(path, output / path.name)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("command", choices=["version", "record", "verify", "check-image"])
    parser.add_argument("--tag")
    parser.add_argument("--github-output", type=Path)
    parser.add_argument("--arch", choices=ARCHES)
    parser.add_argument("--commit")
    parser.add_argument("--directory", type=Path, default=Path("dist"))
    parser.add_argument("--output", type=Path, default=Path("release-assets"))
    parser.add_argument("--image", default="sidecar:ci")
    parser.add_argument("--image-file", type=Path)
    args = parser.parse_args()
    value = version(tag=args.tag)
    if args.command == "version":
        if args.github_output:
            with args.github_output.open("a") as output:
                output.write(f"version={value}\nprerelease={'true' if '-' in value else 'false'}\n")
        print(value)
        return
    revision = commit(args.commit or "")
    if args.command == "verify":
        verify(args.directory, args.output, value, revision)
    else:
        if not args.arch:
            parser.error("--arch is required")
        if args.command == "record":
            record(args.directory, args.arch, value, revision, args.image, args.image_file)
        else:
            data = json.loads((args.directory / "build.json").read_text())
            check_image(inspect_image(args.image), args.arch, value, revision, data["image_id"])


if __name__ == "__main__":
    main()

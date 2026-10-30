import json
from pathlib import Path
import tempfile
import unittest

import release


class ReleaseValidation(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)

    def package(self, version="0.1.0", locked=None):
        (self.root / "Cargo.toml").write_text(f'[package]\nname = "test"\nversion = "{version}"\n')
        (self.root / "Cargo.lock").write_text(f'[[package]]\nname = "test"\nversion = "{locked or version}"\n')

    def test_release_and_prerelease_must_match_both_manifests(self):
        for version in ["0.1.0", "1.2.3-rc.1", "1.2.3-beta"]:
            self.package(version)
            self.assertEqual(release.version(self.root, "v" + version), version)
            with self.assertRaises(ValueError):
                release.version(self.root, "v9.9.9")
        self.package(locked="0.2.0")
        with self.assertRaisesRegex(ValueError, "Cargo.lock"):
            release.version(self.root)

    def test_rejects_versions_that_cannot_be_used_as_release_tags(self):
        for version in ["01.2.3", "1.2.3+build", "1.2.3-rc.01", "1.2", "1.2.3-"]:
            self.package(version)
            with self.assertRaises(ValueError):
                release.version(self.root)

    def artifacts(self):
        for arch in release.ARCHES:
            directory = self.root / ("packages-" + arch)
            directory.mkdir()
            name = release.archive_name("0.1.0", arch)
            (directory / name).write_bytes(b"archive " + arch.encode())
            checksum = release.digest(directory / name)
            (directory / (name + ".sha256")).write_text(f"{checksum}  {name}\n")
            image = self.root / ("image-" + arch)
            image.mkdir()
            (image / "image.tar.gz").write_bytes(b"image " + arch.encode())
            data = {"version": "0.1.0", "commit": "a" * 40, "arch": arch,
                    "archive": name, "sha256": checksum, "image_id": "sha256:" + "b" * 64,
                    "image_sha256": release.digest(image / "image.tar.gz")}
            (directory / "build.json").write_text(json.dumps(data))

    def verify(self):
        release.verify(self.root, self.root / "output", "0.1.0", "a" * 40)

    def test_matching_artifacts_from_both_architectures_pass(self):
        self.artifacts()
        self.verify()
        self.assertEqual(len(list((self.root / "output").iterdir())), 4)

    def test_wrong_commit_or_archive_name_is_rejected_before_copying(self):
        self.artifacts()
        path = self.root / "packages-arm64/build.json"
        original = json.loads(path.read_text())
        for key, bad in [("commit", "c" * 40), ("archive", "../other.tar.gz"),
                         ("version", "0.2.0"), ("arch", "amd64")]:
            path.write_text(json.dumps({**original, key: bad}))
            with self.assertRaises(ValueError):
                self.verify()
            self.assertFalse((self.root / "output").exists())

    def test_corrupt_image_and_archive_are_rejected(self):
        self.artifacts()
        image = self.root / "image-arm64/image.tar.gz"
        before = image.read_bytes()
        image.write_bytes(b"corrupt")
        with self.assertRaisesRegex(ValueError, "image checksum"):
            self.verify()
        image.write_bytes(before)
        (self.root / "packages-amd64" / release.archive_name("0.1.0", "amd64")).write_bytes(b"corrupt")
        with self.assertRaisesRegex(ValueError, "archive checksum"):
            self.verify()

    def test_loaded_image_must_be_the_tested_image_for_the_right_architecture(self):
        image = {"Os": "linux", "Architecture": "arm64", "Id": "sha256:" + "b" * 64,
                 "Config": {"Labels": {"org.opencontainers.image.version": "0.1.0",
                                        "org.opencontainers.image.revision": "a" * 40,
                                        "org.opencontainers.image.licenses": "AGPL-3.0-only"}}}
        release.check_image(image, "arm64", "0.1.0", "a" * 40, image["Id"])
        for arch, version, revision, identity in [("amd64", "0.1.0", "a" * 40, image["Id"]),
                                                  ("arm64", "0.2.0", "a" * 40, image["Id"]),
                                                  ("arm64", "0.1.0", "a" * 40, "sha256:" + "c" * 64)]:
            with self.assertRaises(ValueError):
                release.check_image(image, arch, version, revision, identity)


if __name__ == "__main__":
    unittest.main()

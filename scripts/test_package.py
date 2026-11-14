"""Hermetic packaging tests; never execute or publish a release binary."""
import hashlib
import json
import importlib.util
from pathlib import Path
import tarfile
import tempfile
import unittest
from unittest.mock import patch

spec = importlib.util.spec_from_file_location("ztail_package", Path(__file__).with_name("package.py"))
pack = importlib.util.module_from_spec(spec)
spec.loader.exec_module(pack)


class PackageTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.root_patch = patch.object(pack, "ROOT", self.root)
        self.root_patch.start()
        self.addCleanup(self.root_patch.stop)
        self.manifest()
        (self.root / "LICENSE").write_text("Test fixture only, not a project license.")
        for file in ["README.md", "CHANGELOG.md"]:
            (self.root / file).write_text("fixture")
        (self.root / "docs").mkdir()
        (self.root / "docs" / "usage.md").write_text("fixture docs")
        self.binary = self.root / "binary with spaces"
        self.binary.write_bytes(b"binary fixture")
        self.info = {"schema_version": 1, "name": "ztail", "version": "0.1.0",
                     "target": "x86_64-unknown-linux-gnu", "features": ["gzip", "plain", "zstd"]}
        def run(binary, *args):
            if args == ("--version",):
                return b"ztail 0.1.0\n"
            if args == ("--build-info",):
                return json.dumps(self.info).encode()
            if args[-1] == "--manpage":
                return b'.TH ZTAIL 1\n.SH NAME\nztail\n'
            return f"# {args[-1]} completion\n".encode()
        self.run_patch = patch.object(pack, "run", side_effect=run)
        self.runner = self.run_patch.start()
        self.addCleanup(self.run_patch.stop)

    def manifest(self, licensed=True):
        text = '[package]\nversion = "0.1.0"\n'
        if licensed:
            text += 'license = "MIT"\n'
        (self.root / "Cargo.toml").write_text(text)

    def package(self, directory="out", **kwargs):
        return pack.package(self.binary, "x86_64-unknown-linux-gnu", "zstd",
                            self.root / directory, tag="v0.1.0", **kwargs)

    def test_reproducible_archive_contents_and_checksum(self):
        first = self.package()
        second = self.package("second")
        self.assertEqual(first.read_bytes(), second.read_bytes())
        checksum = first.with_name(first.name + ".sha256").read_text().split()[0]
        self.assertEqual(checksum, hashlib.sha256(first.read_bytes()).hexdigest())
        with tarfile.open(first) as tar:
            names = tar.getnames()
            prefix = "ztail-0.1.0-x86_64-unknown-linux-gnu-zstd/"
            for shell in pack.SHELL_FILES.values():
                self.assertIn(prefix + "completions/" + shell, names)
            self.assertIn(prefix + "LICENSE", names)
            self.assertIn(prefix + "docs/usage.md", names)
            self.assertIn(prefix + "man/ztail.1", names)
            self.assertEqual(json.load(tar.extractfile(prefix + "BUILD-INFO.json")), self.info)
            self.assertEqual(tar.getmember(prefix + "ztail").mode, 0o755)
            for member in tar.getmembers():
                self.assertEqual(member.mtime, 0)
                self.assertFalse(Path(member.name).is_absolute())
                self.assertNotIn("..", Path(member.name).parts)
        with self.assertRaises(FileExistsError):
            self.package()

    def test_license_gate_and_development_marker(self):
        for remove_text in [False, True]:
            self.manifest(licensed=remove_text)
            if remove_text:
                (self.root / "LICENSE").unlink()
            with self.assertRaisesRegex(ValueError, "release blocked"):
                self.package()
        archive = self.package(allow_unlicensed=True)
        with tarfile.open(archive) as tar:
            self.assertTrue(any(name.endswith("LICENSE-UNSET.txt") for name in tar.getnames()))

    def test_rejects_mismatched_tag_and_binary_version(self):
        with self.assertRaisesRegex(ValueError, "tag must match"):
            pack.package(self.binary, "x86_64-unknown-linux-gnu", "zstd", self.root / "out", "v9.0.0")
        self.runner.return_value = b"ztail 9.0.0\n"
        self.runner.side_effect = None
        with self.assertRaisesRegex(ValueError, "binary version"):
            self.package()

    def test_rejects_unknown_target_and_flavor(self):
        with self.assertRaisesRegex(ValueError, "unsupported"):
            pack.package(self.binary, "../../escape", "zstd", self.root / "out")
        with self.assertRaisesRegex(ValueError, "unsupported"):
            pack.package(self.binary, "x86_64-unknown-linux-gnu", "unknown", self.root / "out")

    def test_rejects_mislabeled_architecture_and_features(self):
        self.info["target"] = "aarch64-unknown-linux-gnu"
        with self.assertRaisesRegex(ValueError, "binary target"):
            self.package()
        self.info["target"] = "x86_64-unknown-linux-gnu"
        self.info["features"] = ["gzip", "plain"]
        with self.assertRaisesRegex(ValueError, "binary features"):
            self.package()
        self.info["features"] = None
        with self.assertRaisesRegex(ValueError, "binary features"):
            self.package()

    def test_rejects_build_info_schema_and_identity(self):
        self.info["schema_version"] = 2
        with self.assertRaisesRegex(ValueError, "schema"):
            self.package()
        self.info["schema_version"] = 1
        self.info["version"] = "9.0.0"
        with self.assertRaisesRegex(ValueError, "version/name mismatch"):
            self.package()


if __name__ == "__main__":
    unittest.main()


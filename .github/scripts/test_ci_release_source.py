"""Provenance verification rejects wrong sources and altered archives."""

import hashlib
import importlib.util
import json
from pathlib import Path
import tempfile
import unittest

spec = importlib.util.spec_from_file_location("source", Path(__file__).with_name("ci-release-source.py"))
source = importlib.util.module_from_spec(spec)
spec.loader.exec_module(source)


class SourceTests(unittest.TestCase):
    def setUp(self):
        temp = tempfile.TemporaryDirectory()
        self.addCleanup(temp.cleanup)
        self.directory = Path(temp.name)
        self.args = (self.directory, "a" * 40, "v1.2.3", "x86_64-unknown-linux-gnu")
        self.name = "boomux-v1.2.3-x86_64-unknown-linux-gnu.tar.gz"
        (self.directory / self.name).write_bytes(b"fixture")
        self.checksum()
        source.process("record", *self.args)

    def checksum(self):
        digest = hashlib.sha256((self.directory / self.name).read_bytes()).hexdigest()
        (self.directory / (self.name + ".sha256")).write_text(f"{digest}  {self.name}\n")

    def test_matching_archive_passes(self):
        source.process("verify", *self.args)

    def test_wrong_source_fails(self):
        with self.assertRaises(ValueError):
            source.process("verify", self.directory, "b" * 40, *self.args[2:])

    def test_altered_archive_fails_even_with_updated_checksum(self):
        (self.directory / self.name).write_bytes(b"altered")
        with self.assertRaises(ValueError):
            source.process("verify", *self.args)
        self.checksum()
        with self.assertRaises(ValueError):
            source.process("verify", *self.args)

    def test_missing_provenance_fails(self):
        (self.directory / "ci-source.json").unlink()
        with self.assertRaises(FileNotFoundError):
            source.process("verify", *self.args)

    def test_wrong_tag_or_target_fails(self):
        for tag, target in [("v1.2.4", self.args[3]), (self.args[2], "aarch64-unknown-linux-gnu")]:
            with self.assertRaises(FileNotFoundError):
                source.process("verify", self.directory, self.args[1], tag, target)

    def test_desktop_provenance_binds_version_despite_unversioned_asset_name(self):
        self.name = "boomux-desktop-x86_64-unknown-linux-gnu.tar.gz"
        (self.directory / self.name).write_bytes(b"desktop fixture")
        self.checksum()
        source.process("record", *self.args, "desktop")
        source.process("verify", *self.args, "desktop")
        with self.assertRaisesRegex(ValueError, "validated release source"):
            source.process("verify", self.directory, self.args[1], "v1.2.4", self.args[3], "desktop")
        record = self.directory / "ci-source.json"
        metadata = json.loads(record.read_text())
        metadata["kind"] = "cli"
        record.write_text(json.dumps(metadata))
        with self.assertRaisesRegex(ValueError, "validated release source"):
            source.process("verify", *self.args, "desktop")

    def test_unknown_artifact_and_unsupported_desktop_target_are_rejected(self):
        for target, kind in [(self.args[3], "unknown"), ("aarch64-unknown-linux-gnu", "desktop")]:
            with self.assertRaisesRegex(ValueError, "artifact kind or target"):
                source.process("record", self.directory, self.args[1], self.args[2], target, kind)


if __name__ == "__main__":
    unittest.main()

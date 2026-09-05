"""Provenance verification rejects wrong sources and altered archives."""

import hashlib
import importlib.util
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


if __name__ == "__main__":
    unittest.main()

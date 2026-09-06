import importlib.util
from pathlib import Path
import tempfile
import unittest

spec = importlib.util.spec_from_file_location("versions", Path(__file__).with_name("check-workspace-version.py"))
versions = importlib.util.module_from_spec(spec)
spec.loader.exec_module(versions)


class WorkspaceVersionTests(unittest.TestCase):
    def test_release_merge_must_include_desktop_and_lockfile(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            (root / "desktop").mkdir()
            (root / "Cargo.toml").write_text('[package]\nversion = "1.9.8"\n')
            (root / ".release-please-manifest.json").write_text('{".": "1.9.8"}')
            for desktop, locked in [("1.9.7", "1.9.7"), ("1.9.8", "1.9.7"), ("1.9.8", "1.9.8")]:
                (root / "desktop/Cargo.toml").write_text(f'[package]\nversion = "{desktop}"\n')
                (root / "Cargo.lock").write_text(
                    '[[package]]\nname = "boomux"\nversion = "1.9.8"\n'
                    f'[[package]]\nname = "boomux-desktop"\nversion = "{locked}"\n')
                if desktop == locked == "1.9.8":
                    versions.check(root)
                else:
                    with self.assertRaisesRegex(ValueError, "Synchronize"):
                        versions.check(root)

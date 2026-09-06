"""Exercise workspace packaging without compiling or invoking a real daemon."""

import hashlib
import io
from pathlib import Path
import runpy
import subprocess
import tarfile
import tempfile
import unittest


PACKAGE = runpy.run_path(str(Path(__file__).with_name("package-release.py")))
RENDER = runpy.run_path(str(Path(__file__).with_name("render-installer.py")))
TARGET = PACKAGE["TARGET"]


class PackageTests(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory(prefix="boomux package ")
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)
        subprocess.run(["git", "init", "-q", self.root], check=True)
        for name, version in [("Cargo.toml", "1.2.3"), ("desktop/Cargo.toml", "1.2.3")]:
            self.write(name, f'[package]\nversion = "{version}"\n')
        self.write("LICENSE", "core license")
        self.write("THIRD_PARTY_NOTICES.md", "notices")
        self.write("desktop/LICENSE", "desktop license")
        self.write("desktop/packaging/share/fixture", "app integration")
        self.write("desktop/packaging/boomux-desktop", "#!/bin/sh\n")
        self.write(f"target/{TARGET}/release/boomux-desktop",
                   "#!/bin/sh\nprintf 'boomux-desktop 1.2.3\\n'\n", executable=True)
        subprocess.run(["git", "-C", self.root, "add", "."], check=True)
        subprocess.run(["git", "-C", self.root, "-c", "user.name=Fixture", "-c",
                        "user.email=fixture@example.invalid", "commit", "-qm", "fixture"], check=True)
        self.name = f"boomux-v1.2.3-{TARGET}"
        self.archive = self.root / (self.name + ".tar.gz")
        self.cli = b"#!/bin/sh\nprintf 'boomux 1.2.3\\n'\n"
        with tarfile.open(self.archive, "w:gz") as tar:
            member = tarfile.TarInfo(self.name + "/boomux")
            member.mode = 0o755
            member.size = len(self.cli)
            tar.addfile(member, io.BytesIO(self.cli))
        Path(str(self.archive) + ".sha256").write_text(
            f"{hashlib.sha256(self.archive.read_bytes()).hexdigest()}  {self.archive.name}\n")

    def write(self, name, text, executable=False):
        path = self.root / name
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(text)
        if executable:
            path.chmod(0o755)

    def test_bundle_retains_identical_cli_and_shared_source(self):
        PACKAGE["package"](self.archive, self.root)
        with tarfile.open(self.root / "dist" / PACKAGE["ASSET"]) as tar:
            self.assertEqual(tar.extractfile("bin/boomux").read(), self.cli)
            self.assertEqual(tar.getmember("bin/boomux-desktop").mode & 0o777, 0o755)
            source = subprocess.check_output(["git", "-C", self.root, "rev-parse", "HEAD"], text=True).strip()
            self.assertIn(f"source {source}\n", tar.extractfile("release.txt").read().decode())
            self.assertIn("THIRD_PARTY_NOTICES.md", tar.getnames())

    def test_mismatched_manifest_rejects_bundle(self):
        self.write("desktop/Cargo.toml", '[package]\nversion = "1.2.4"\n')
        with self.assertRaisesRegex(ValueError, "versions differ"):
            PACKAGE["package"](self.archive, self.root)

    def test_stale_desktop_executable_rejects_bundle(self):
        self.write(f"target/{TARGET}/release/boomux-desktop",
                   "#!/bin/sh\nprintf 'boomux-desktop 1.2.2\\n'\n", executable=True)
        with self.assertRaisesRegex(ValueError, "binary version"):
            PACKAGE["package"](self.archive, self.root)

    def test_corrupt_cli_preserves_previous_bundle(self):
        PACKAGE["package"](self.archive, self.root)
        output = self.root / "dist" / PACKAGE["ASSET"]
        original = output.read_bytes()
        self.archive.write_bytes(b"corrupted")
        with self.assertRaisesRegex(ValueError, "checksum"):
            PACKAGE["package"](self.archive, self.root)
        self.assertEqual(output.read_bytes(), original)

    def test_installer_pins_release_and_preserves_override(self):
        destination = self.root / "installer.sh"
        RENDER["render"]("v1.2.3", destination)
        self.assertIn("version=${BOOMUX_DESKTOP_VERSION:-v1.2.3}", destination.read_text())
        self.assertIn("repository=https://github.com/gardnmi/boomux\n", destination.read_text())
        subprocess.run(["sh", "-n", destination], check=True)
        with self.assertRaisesRegex(ValueError, "strict"):
            RENDER["render"]("v1.2.3; echo invalid", destination)


if __name__ == "__main__":
    unittest.main()

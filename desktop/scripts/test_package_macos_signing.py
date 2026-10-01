import importlib.util
import json
from pathlib import Path
import plistlib
import stat
import os
import shutil
import subprocess
import tempfile
from types import SimpleNamespace
import unittest
import zipfile

SPEC = importlib.util.spec_from_file_location("sign_macos", Path(__file__).with_name("sign-macos.py"))
SIGN = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(SIGN)


class SigningTests(unittest.TestCase):
    @unittest.skipUnless(shutil.which("cc") and os.uname().sysname == "Linux", "Linux launcher fixture")
    def test_native_launcher_preserves_exact_arguments_and_uses_sibling(self):
        with tempfile.TemporaryDirectory(prefix="boomux app with spaces ") as root:
            root = Path(root)
            source = Path(__file__).parents[1] / "packaging/macos/boomux-launcher.c"
            subprocess.run(["cc", "-D_POSIX_C_SOURCE=200809L", "-DBOOMUX_LAUNCHER_TEST", "-std=c11", "-Wall", "-Wextra", "-Werror", str(source), "-o", str(root / "boomux-launcher")], check=True)
            desktop = root / "boomux-desktop"
            desktop.write_text("#!/bin/sh\nprintf '%s\\n' \"$@\"\n")
            desktop.chmod(0o755)
            result = subprocess.check_output([str(root / "boomux-launcher"), "a b", "$(no interpolation)", ""], text=True)
            self.assertEqual(result.splitlines(), ["--macos-launch", "a b", "$(no interpolation)", ""])

    def test_signs_every_executable_before_bundle_and_gates_publication(self):
        calls = []
        def run(args, **kwargs):
            calls.append(args)
            return SimpleNamespace(stdout=json.dumps({"status": "Accepted"}), stderr="TeamIdentifier=ABCDEFGHIJ\n")
        SIGN.sign(Path("stage"), "Developer ID Application: Test", "profile", Path("keychain"), "ABCDEFGHIJ", run)
        signed = [call[-1] for call in calls if call[0].endswith("codesign") and "--sign" in call]
        self.assertEqual(signed, [f"stage/Boomux.app/Contents/MacOS/{name}" for name in SIGN.EXECUTABLES] + ["stage/Boomux.app"])
        for call in calls[:5]:
            self.assertIn("runtime", call)
            self.assertIn("--timestamp", call)
            self.assertNotIn("--deep", call)
        self.assertIn("staple", calls[-4])
        self.assertIn("validate", calls[-3])
        self.assertEqual(calls[-1][0], "/usr/sbin/spctl")
        self.assertTrue(all(call[0].startswith("/usr/") for call in calls))

    def test_rejected_notarization_cannot_staple_or_assess(self):
        calls = []
        def run(args, **kwargs):
            calls.append(args)
            return SimpleNamespace(stdout='{"status":"Invalid"}', stderr="TeamIdentifier=ABCDEFGHIJ\n")
        with self.assertRaisesRegex(ValueError, "not accepted"):
            SIGN.sign(Path("stage"), "Developer ID Application: Test", "profile", Path("keychain"), "ABCDEFGHIJ", run)
        self.assertFalse(any("stapler" in call or "--assess" in call for call in calls))

    def test_wrong_team_is_rejected_before_notarization(self):
        calls = []
        def run(args, **kwargs):
            calls.append(args)
            return SimpleNamespace(stdout="", stderr="TeamIdentifier=ZZZZZZZZZZ\n")
        with self.assertRaisesRegex(ValueError, "Team ID"):
            SIGN.sign(Path("stage"), "Developer ID Application: Test", "profile", Path("keychain"), "ABCDEFGHIJ", run)
        self.assertFalse(any("notarytool" in call for call in calls))

    def test_adhoc_identity_is_never_treated_as_distribution(self):
        with self.assertRaisesRegex(ValueError, "Developer ID"):
            SIGN.sign(Path("stage"), "-", "profile", Path("keychain"), "ABCDEFGHIJ")

    def test_archive_rejects_escape_symlinks_duplicates_and_wrong_prefix(self):
        with tempfile.TemporaryDirectory() as root:
            root = Path(root)
            for name, mode in [("../evil", stat.S_IFREG), ("/evil", stat.S_IFREG), ("other/file", stat.S_IFREG),
                               (SIGN.PREFIX + "/../evil", stat.S_IFREG), (SIGN.PREFIX + "/link", stat.S_IFLNK)]:
                archive = root / "bad.zip"
                with zipfile.ZipFile(archive, "w") as bundle:
                    entry = zipfile.ZipInfo(name)
                    entry.external_attr = (mode | 0o755) << 16
                    bundle.writestr(entry, b"bad")
                with self.assertRaises(ValueError):
                    SIGN.unpack(archive, root / "out")
            self.assertFalse((root / "out").exists())

    def test_provenance_requires_matching_bundle_and_all_helpers(self):
        with tempfile.TemporaryDirectory() as root:
            stage = Path(root)
            contents = stage / "Boomux.app/Contents"
            (contents / "MacOS").mkdir(parents=True)
            metadata = {"source": "a" * 40, "version": "1.2.3", "target": "aarch64-apple-darwin", "distribution": "testing-preview", "notarized": False}
            (stage / "build.json").write_text(json.dumps(metadata))
            info = {"CFBundleIdentifier": SIGN.BUNDLE_ID, "CFBundleShortVersionString": "1.2.3", "CFBundleVersion": "1.2.3", "CFBundleExecutable": "boomux-launcher", "LSMinimumSystemVersion": "15.0"}
            (contents / "Info.plist").write_bytes(plistlib.dumps(info))
            for name in (*SIGN.EXECUTABLES, "boomux-launcher"):
                binary = contents / "MacOS" / name
                binary.write_text("never execute this")
                binary.chmod(0o755)
            self.assertEqual(SIGN.validate(stage, "a" * 40, "1.2.3")[1], metadata)
            with self.assertRaisesRegex(ValueError, "provenance"):
                SIGN.validate(stage, "b" * 40, "1.2.3")
            (contents / "MacOS/webgpu_gateway").unlink()
            with self.assertRaisesRegex(ValueError, "executable"):
                SIGN.validate(stage, "a" * 40, "1.2.3")


if __name__ == "__main__":
    unittest.main()

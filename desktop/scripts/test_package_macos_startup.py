"""Launcher argv/precedence regressions; no real GUI or daemon is invoked."""
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[2]


class LauncherTests(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory(prefix="boomux launcher ")
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)
        self.home = self.root / "home with spaces"
        self.home.mkdir()
        self.env = dict(os.environ, HOME=str(self.home))

    def executable(self, path, text):
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(text)
        path.chmod(0o755)

    def test_macos_launcher_only_delegates_bootstrap_and_keeps_exact_argv(self):
        bundle = self.root / "Boomux preview.app/Contents/MacOS"
        bundle.mkdir(parents=True)
        launcher = bundle / "boomux-launcher"
        shutil.copy2(ROOT / "desktop/packaging/macos/boomux-launcher", launcher)
        launcher.chmod(0o755)
        self.executable(bundle / "boomux", "#!/bin/sh\nexit 91\n")
        # A failed daemon would abort the previous set -e launcher. The new
        # launcher always reaches Desktop's recoverable, bounded bootstrap.
        self.executable(bundle / "boomux-desktop", "#!/bin/sh\nprintf '%s\\n' \"$@\"\n")
        self.env.pop("SHELL", None)
        self.env["PATH"] = "/nonexistent explicit path"
        result = subprocess.run([launcher, "--update-ready", "a b; literal", ""],
                                env=self.env, cwd="/", capture_output=True,
                                text=True, timeout=5, check=True)
        self.assertEqual(result.stdout.splitlines(),
                         ["--macos-launch", "--update-ready", "a b; literal", ""])

    @unittest.skipUnless(os.uname().sysname == "Linux", "Linux launcher uses GNU readlink")
    def test_linux_launcher_keeps_daemon_first_and_matching_bundle_path(self):
        bundle = self.root / "linux bundle"
        launcher = bundle / "bin/boomux-desktop"
        launcher.parent.mkdir(parents=True)
        shutil.copy2(ROOT / "desktop/packaging/boomux-desktop", launcher)
        launcher.chmod(0o755)
        self.executable(bundle / "bin/boomux", "#!/bin/sh\ntest \"$1 $2\" = 'daemon start' || exit 1\nprintf 'daemon\\n'\n")
        self.executable(bundle / "libexec/boomux-desktop", "#!/bin/sh\nprintf 'gui:%s:%s\\n' \"$1\" \"${PATH%%:*}\"\n")
        result = subprocess.run([launcher, "a b; literal"], env=self.env,
                                capture_output=True, text=True, timeout=5, check=True)
        self.assertEqual(result.stdout.splitlines(),
                         ["daemon", f"gui:a b; literal:{bundle / 'bin'}"])
        self.executable(bundle / "bin/boomux", "#!/bin/sh\nexit 9\n")
        failed = subprocess.run([launcher], env=self.env, capture_output=True, timeout=5)
        self.assertEqual(failed.returncode, 9)
        self.assertEqual(failed.stdout, b"")


if __name__ == "__main__":
    unittest.main()

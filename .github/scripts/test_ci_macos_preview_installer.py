"""Exercise installer decisions with isolated command fixtures, without a Mac."""
import hashlib
import os
from pathlib import Path
import subprocess
import tempfile
import unittest

SCRIPT = Path(__file__).with_name('install-macos-preview.sh')


class InstallerTests(unittest.TestCase):
    def run_installer(self, *, system='Darwin', arch='arm64', version='15.7',
                      corrupt=False, signature_failure=False, existing=False):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            home = root / 'home with spaces'
            home.mkdir()
            binaries = root / 'bin'
            binaries.mkdir()
            payload = b'fixture download'
            script = root / 'install.sh'
            script.write_text(SCRIPT.read_text().replace(
                '65088ef421eb25bd35576d059bc37e0f318aa8cefdf12d29f23049da8bd174a4',
                hashlib.sha256(payload).hexdigest()))
            commands = {
                'uname': f'if [ "$1" = -s ]; then echo {system}; else echo {arch}; fi',
                'sw_vers': f'echo {version}',
                'curl': 'while [ "$1" != -o ]; do shift; done\nshift\nprintf "%s" "' + ('bad' if corrupt else payload.decode()) + '" > "$1"',
                'ditto': 'mkdir -p "$4/Boomux macOS Preview/Boomux.app/Contents/MacOS"',
                'codesign': 'exit ' + ('1' if signature_failure else '0'),
                'lipo': 'echo arm64',
            }
            for name, body in commands.items():
                command = binaries / name
                command.write_text('#!/bin/bash\nset -eu\n' + body + '\n')
                command.chmod(0o755)
            destination = home / 'Applications/Boomux Preview-66bca02a.app'
            if existing:
                destination.mkdir(parents=True)
                (destination / 'preserve').write_text('existing app')
            env = {**os.environ, 'HOME': str(home), 'TMPDIR': str(root),
                   'PATH': str(binaries) + os.pathsep + os.environ['PATH']}
            result = subprocess.run(['bash', str(script)], env=env, capture_output=True, text=True)
            installed = destination.exists()
            preserved = (destination / 'preserve').exists()
            self.assertEqual(list(root.glob('boomux-preview.*')), [])
            return result, installed, preserved

    def test_supported_mac_installs_with_spaces_in_home(self):
        result, installed, _ = self.run_installer()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertTrue(installed)

    def test_unsupported_hosts_install_nothing(self):
        for kwargs in [{'system': 'Linux'}, {'arch': 'x86_64'}, {'version': '14.7'}]:
            with self.subTest(**kwargs):
                result, installed, _ = self.run_installer(**kwargs)
                self.assertNotEqual(result.returncode, 0)
                self.assertFalse(installed)

    def test_corrupt_download_installs_nothing(self):
        result, installed, _ = self.run_installer(corrupt=True)
        self.assertIn('checksum mismatch', result.stderr)
        self.assertNotEqual(result.returncode, 0)
        self.assertFalse(installed)

    def test_invalid_signature_installs_nothing(self):
        result, installed, _ = self.run_installer(signature_failure=True)
        self.assertNotEqual(result.returncode, 0)
        self.assertFalse(installed)

    def test_existing_installation_is_preserved(self):
        result, installed, preserved = self.run_installer(existing=True)
        self.assertNotEqual(result.returncode, 0)
        self.assertTrue(installed and preserved)


if __name__ == '__main__':
    unittest.main()

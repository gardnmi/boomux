"""Exercise the rendered universal installer through its public sh --desktop entry."""
import hashlib
import os
from pathlib import Path
import subprocess
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[2]
ARCHIVE = 'boomux-desktop-aarch64-apple-darwin.zip'


class MacReleaseInstallerTests(unittest.TestCase):
    def run_installer(self, *, system='Darwin', arch='arm64', version='15.7',
                      corrupt=False, checksum_name=ARCHIVE, extra_checksum=False,
                      signature_failure=False, existing=False, symlink=False,
                      binary_version='1.2.3', binary_arch='arm64', locked=False):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            home = root / 'home with spaces'
            home.mkdir()
            binaries = root / 'bin'
            binaries.mkdir()
            payload = b'fixture download'
            fixtures = root / 'release'
            fixtures.mkdir()
            (fixtures / ARCHIVE).write_bytes(b'bad' if corrupt else payload)
            (fixtures / (ARCHIVE + '.sha256')).write_text(
                f'{hashlib.sha256(payload).hexdigest()}  {checksum_name}\n'
                + ('unexpected\n' if extra_checksum else ''))
            script = root / 'installer'
            subprocess.run(['bash', str(ROOT / 'packaging/render-installer.sh'),
                            'v1.2.3', str(script)], check=True)
            commands = {
                'uname': f'if [ "$1" = -s ]; then echo {system}; else echo {arch}; fi',
                'sw_vers': f'echo {version}',
                'stat': 'if [ "$2" = %u ]; then id -u; else echo 700; fi',
                'curl': '''while [ "$1" != -o ]; do shift; done
shift
output=$1
shift
printf '%s\\n' "$1" >> "$REQUESTS"
cp "$FIXTURES/${1##*/}" "$output"''',
                'ditto': '''destination="$4/Boomux macOS Preview/Boomux.app/Contents/MacOS"
mkdir -p "$destination"
for name in boomux boomux-desktop; do
    printf '#!/bin/sh\\necho "%s %s"\\n' "$name" "$BINARY_VERSION" > "$destination/$name"
    chmod +x "$destination/$name"
done''',
                'codesign': 'exit ' + ('1' if signature_failure else '0'),
                'lipo': f'echo {binary_arch}',
            }
            for name, body in commands.items():
                command = binaries / name
                command.write_text('#!/bin/sh\nset -eu\n' + body + '\n')
                command.chmod(0o755)
            applications = home / 'Applications'
            applications.mkdir()
            destination = applications / 'Boomux-1.2.3.app'
            if existing:
                destination.mkdir()
                (destination / 'preserve').write_text('existing app')
            if symlink:
                destination.symlink_to(root / 'absent')
            if locked:
                (applications / '.boomux-install.lock').mkdir()
            env = {**os.environ, 'HOME': str(home), 'TMPDIR': str(root),
                   'PATH': str(binaries) + os.pathsep + os.environ['PATH'],
                   'FIXTURES': str(fixtures), 'REQUESTS': str(root / 'requests'),
                   'BINARY_VERSION': binary_version}
            result = subprocess.run(['sh', str(script), '--desktop'], env=env,
                                    capture_output=True, text=True)
            installed = (destination / 'Contents/MacOS/boomux').exists()
            preserved = (destination / 'preserve').exists() or destination.is_symlink()
            self.assertEqual(list(applications.glob('.boomux-install.*')),
                             [applications / '.boomux-install.lock'] if locked else [])
            requests = (root / 'requests').read_text().splitlines() if (root / 'requests').exists() else []
            return result, installed, preserved, requests

    def test_same_desktop_command_installs_matching_mac_release(self):
        result, installed, _, requests = self.run_installer()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertTrue(installed)
        self.assertEqual(requests, [
            f'https://github.com/gardnmi/boomux/releases/download/v1.2.3/{ARCHIVE}',
            f'https://github.com/gardnmi/boomux/releases/download/v1.2.3/{ARCHIVE}.sha256'])
        self.assertIn('experimental', result.stdout)

    def test_unsupported_hosts_download_nothing(self):
        for kwargs in [{'system': 'FreeBSD'}, {'arch': 'x86_64'}, {'version': '14.7'}, {'version': 'unknown'}]:
            with self.subTest(**kwargs):
                result, installed, _, requests = self.run_installer(**kwargs)
                self.assertNotEqual(result.returncode, 0)
                self.assertFalse(installed)
                self.assertEqual(requests, [])

    def test_bad_download_or_bundle_installs_nothing(self):
        for kwargs in [{'corrupt': True}, {'checksum_name': '../other'}, {'extra_checksum': True},
                       {'signature_failure': True}, {'binary_version': '1.2.4'}, {'binary_arch': 'x86_64'}]:
            with self.subTest(**kwargs):
                result, installed, _, _ = self.run_installer(**kwargs)
                self.assertNotEqual(result.returncode, 0)
                self.assertFalse(installed)

    def test_existing_destination_and_lock_are_preserved(self):
        for kwargs in [{'existing': True}, {'symlink': True}, {'locked': True}]:
            with self.subTest(**kwargs):
                result, installed, preserved, requests = self.run_installer(**kwargs)
                self.assertNotEqual(result.returncode, 0)
                self.assertFalse(installed)
                if not kwargs.get('locked'):
                    self.assertTrue(preserved)
                self.assertEqual(requests, [])


if __name__ == '__main__':
    unittest.main()

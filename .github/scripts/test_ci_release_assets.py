"""The single publisher requires all artifacts and an identical bundled CLI."""

import hashlib
import io
import json
import os
from pathlib import Path
import subprocess
import tarfile
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[2]
TAG = 'v1.2.3'
DESKTOP = 'boomux-desktop-x86_64-unknown-linux-gnu.tar.gz'


class ReleaseAssetsTests(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)
        self.sha = subprocess.check_output(['git', 'rev-parse', 'HEAD'], cwd=ROOT, text=True).strip()
        self.cli = b'standalone CLI fixture'
        self.files = []
        for target in ['x86_64-unknown-linux-gnu', 'aarch64-unknown-linux-gnu']:
            name = f'boomux-{TAG}-{target}'
            self.archive(name + '.tar.gz', {name + '/boomux': self.cli})
        self.desktop(self.cli)
        subprocess.run(['bash', 'packaging/render-installer.sh', TAG, self.root / 'boomux-installer.sh'], cwd=ROOT, check=True)
        subprocess.run(['python3', 'desktop/scripts/render-installer.py', TAG, self.root / 'boomux-desktop-installer.sh'], cwd=ROOT, check=True)
        self.files += [self.root / 'boomux-installer.sh', self.root / 'boomux-desktop-installer.sh']
        self.state = self.root / 'state.json'
        self.state.write_text(json.dumps({'draft': True, 'assets': {}, 'uploads': []}))
        (self.root / 'gh').write_text('''#!/usr/bin/python3
import hashlib,json,os,pathlib,sys
path=pathlib.Path(os.environ['FIXTURE_STATE'])
state=json.loads(path.read_text()); args=sys.argv[1:]
if args[:2] == ['release','upload']:
    if os.environ.get('FAIL_UPLOAD'): sys.exit(1)
    asset=pathlib.Path(args[3]); state['assets'][asset.name]='sha256:'+hashlib.sha256(asset.read_bytes()).hexdigest()
    state['uploads'].append(asset.name)
elif any('/assets?' in arg for arg in args):
    if os.environ.get('FAIL_LIST'): sys.exit(1)
    for i,(name,digest) in enumerate(state['assets'].items()): print(name+chr(31)+digest+chr(31)+str(i+1))
elif args[-1] == '.id': print(7)
elif args[-1] == '.draft': print(str(state['draft']).lower())
else: raise AssertionError(args)
path.write_text(json.dumps(state))
''')
        (self.root / 'gh').chmod(0o755)
        self.env = dict(os.environ, PATH=f'{self.root}:/usr/bin:/bin', GH_REPO='fixture/boomux', FIXTURE_STATE=str(self.state))

    def archive(self, name, entries):
        path = self.root / name
        with tarfile.open(path, 'w:gz') as tar:
            for name, value in entries.items():
                member = tarfile.TarInfo(name); member.size = len(value); member.mode = 0o755
                tar.addfile(member, io.BytesIO(value))
        checksum = Path(str(path) + '.sha256')
        checksum.write_text(f'{hashlib.sha256(path.read_bytes()).hexdigest()}  {path.name}\n')
        for item in [path, checksum]:
            if item not in self.files: self.files.append(item)

    def desktop(self, cli, sha=None):
        digest = hashlib.sha256(cli).hexdigest()
        metadata = f'boomux-desktop {TAG[1:]}\nboomux {TAG[1:]}\nsource {sha or self.sha}\nboomux sha256 {digest}\n'
        self.archive(DESKTOP, {'bin/boomux': cli, 'bin/boomux-desktop': b'launcher',
                              'libexec/boomux-desktop': b'GUI', 'release.txt': metadata.encode()})

    def publish(self, expected=True, files=None):
        result = subprocess.run(['bash', '.github/scripts/upload-release-assets.sh', TAG, *(self.files if files is None else files)],
                                cwd=ROOT, env=self.env, capture_output=True, text=True)
        self.assertEqual(result.returncode == 0, expected, result.stdout + result.stderr)
        return json.loads(self.state.read_text())

    def test_complete_release_and_matching_retry(self):
        self.assertEqual(len(self.publish()['uploads']), 8)
        self.assertEqual(len(self.publish()['uploads']), 8)

    def test_partial_release_cannot_upload(self):
        self.assertEqual(self.publish(False, self.files[:-1])['uploads'], [])

    def test_wrong_bundled_cli_cannot_upload(self):
        self.desktop(b'different CLI')
        self.assertEqual(self.publish(False)['uploads'], [])

    def test_wrong_source_cannot_upload(self):
        self.desktop(self.cli, 'b' * 40)
        self.assertEqual(self.publish(False)['uploads'], [])

    def test_corrupt_archive_cannot_upload(self):
        (self.root / DESKTOP).write_bytes(b'corrupt')
        self.assertEqual(self.publish(False)['uploads'], [])

    def test_listing_failure_cannot_upload(self):
        self.env['FAIL_LIST'] = '1'
        self.assertEqual(self.publish(False)['uploads'], [])

    def test_existing_conflict_is_preserved(self):
        state = json.loads(self.state.read_text())
        name = self.files[0].name
        state['assets'][name] = 'sha256:' + '0' * 64
        self.state.write_text(json.dumps(state))
        self.assertEqual(self.publish(False)['assets'][name], 'sha256:' + '0' * 64)

    def test_published_release_is_immutable(self):
        state = json.loads(self.state.read_text()); state['draft'] = False
        self.state.write_text(json.dumps(state))
        self.assertEqual(self.publish(False)['uploads'], [])


if __name__ == '__main__':
    unittest.main()

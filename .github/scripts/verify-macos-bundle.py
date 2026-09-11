"""Verify the release ZIP's embedded source, platform, and app version."""
import json
from pathlib import PurePosixPath
import plistlib
import sys
import zipfile


def verify(tag, sha, archive):
    prefix = 'Boomux macOS Preview/'
    with zipfile.ZipFile(archive) as bundle:
        names = bundle.namelist()
        if len(names) != len(set(names)):
            raise ValueError('duplicate Mac archive entries')
        for name in names:
            path = PurePosixPath(name)
            if path.is_absolute() or '..' in path.parts or '\\' in name:
                raise ValueError('unsafe Mac archive path')
        def read(name):
            entry = bundle.getinfo(prefix + name)
            if entry.file_size > 65536 or (entry.external_attr >> 16) & 0o170000 == 0o120000:
                raise ValueError('invalid Mac metadata entry')
            return bundle.read(entry)
        metadata = json.loads(read('build.json'))
        if metadata != {'source': sha, 'version': tag.removeprefix('v'),
                        'target': 'aarch64-apple-darwin',
                        'distribution': 'testing-preview', 'notarized': False}:
            raise ValueError('Mac metadata differs from the release source or version')
        info = plistlib.loads(read('Boomux.app/Contents/Info.plist'))
        if (info.get('CFBundleShortVersionString') != metadata['version']
                or info.get('CFBundleVersion') != metadata['version']
                or info.get('LSMinimumSystemVersion') != '15.0'):
            raise ValueError('Mac app version or minimum OS differs from release')
        for name in ['boomux', 'boomux-desktop', 'boomux-launcher']:
            entry = bundle.getinfo(prefix + 'Boomux.app/Contents/MacOS/' + name)
            mode = entry.external_attr >> 16
            if mode & 0o170000 != 0o100000 or not mode & 0o111 or entry.file_size == 0:
                raise ValueError('missing regular executable in Mac bundle')


if __name__ == '__main__':
    verify(*sys.argv[1:])

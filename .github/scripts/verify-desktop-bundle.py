"""Verify that CLI and Desktop archives contain the same release binary."""

import hashlib
from pathlib import Path
import re
import sys
import tarfile


def regular(tar, name):
    member = tar.getmember(name)
    if not member.isfile():
        raise ValueError(f'expected regular archive member: {name}')
    return tar.extractfile(member)


def verify(tag, sha, cli_archive, desktop_archive):
    if not re.fullmatch(r'v(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)', tag):
        raise ValueError('invalid release version')
    if not re.fullmatch(r'[0-9a-f]{40}', sha):
        raise ValueError('expected exact source SHA')
    with tarfile.open(cli_archive) as cli, tarfile.open(desktop_archive) as desktop:
        with regular(cli, f'boomux-{tag}-x86_64-unknown-linux-gnu/boomux') as binary:
            cli_digest = hashlib.file_digest(binary, 'sha256').hexdigest()
        with regular(desktop, 'bin/boomux') as binary:
            if hashlib.file_digest(binary, 'sha256').hexdigest() != cli_digest:
                raise ValueError('Desktop contains a different Boomux executable')
        with regular(desktop, 'release.txt') as metadata:
            text = metadata.read(4097).decode()
        expected = f'boomux-desktop {tag[1:]}\nboomux {tag[1:]}\nsource {sha}\nboomux sha256 {cli_digest}\n'
        if text != expected:
            raise ValueError('Desktop metadata differs from the release source or version')
        for name in ['libexec/boomux-desktop', 'bin/boomux-desktop']:
            with regular(desktop, name):
                pass


if __name__ == '__main__':
    verify(*sys.argv[1:])

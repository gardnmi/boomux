"""Bind CI archives to their source commit, version, target, and checksum."""

import hashlib
import json
from pathlib import Path
import re
import sys


def metadata(directory, sha, tag, target):
    if not re.fullmatch(r"[0-9a-f]{40}", sha):
        raise ValueError("source must be an exact commit SHA")
    if not re.fullmatch(r"v(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)", tag):
        raise ValueError("invalid release tag")
    if target not in {"x86_64-unknown-linux-gnu", "aarch64-unknown-linux-gnu"}:
        raise ValueError("unsupported release target")
    name = f"boomux-{tag}-{target}.tar.gz"
    with (directory / name).open("rb") as archive:
        digest = hashlib.file_digest(archive, "sha256").hexdigest()
    if (directory / (name + ".sha256")).read_text().strip() != f"{digest}  {name}":
        raise ValueError("archive checksum mismatch")
    return {"sha": sha, "tag": tag, "target": target, "sha256": digest}


def process(mode, directory, sha, tag, target):
    expected = metadata(directory, sha, tag, target)
    source = directory / "ci-source.json"
    if mode == "record":
        source.write_text(json.dumps(expected) + "\n")
    elif mode == "verify":
        if json.loads(source.read_text()) != expected:
            raise ValueError("CI archive does not match the validated release source")
    else:
        raise ValueError("expected record or verify")


if __name__ == "__main__":
    mode, directory, sha, tag, target = sys.argv[1:]
    process(mode, Path(directory), sha, tag, target)

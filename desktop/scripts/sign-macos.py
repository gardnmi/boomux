"""Sign an already-built preview without executing any code from its archive.

Run only on a trusted macOS signing runner. Credentials are provisioned by the
operator in a temporary keychain, never built or fetched by this script.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path, PurePosixPath
import plistlib
import re
import shutil
import stat
import subprocess
import tempfile
import zipfile

PREFIX = "Boomux macOS Preview"
BUNDLE_ID = "com.boomux.desktop.preview"  # Keep the installed preview's identity stable.
EXECUTABLES = ("boomux", "boomux-desktop", "webgpu_gateway", "boomux-launcher")
MAX_BYTES = 1024 * 1024 * 1024


def unpack(archive, destination):
    total = 0
    with zipfile.ZipFile(archive) as source:
        entries = source.infolist()
        names = [entry.filename for entry in entries]
        if len(entries) > 20000 or len(names) != len(set(names)):
            raise ValueError("oversized or duplicate archive entries")
        for entry in entries:
            path = PurePosixPath(entry.filename)
            mode = entry.external_attr >> 16
            if (not path.parts or path.parts[0] != PREFIX or path.is_absolute()
                    or ".." in path.parts or "\\" in entry.filename
                    or stat.S_ISLNK(mode) or (stat.S_IFMT(mode) not in (0, stat.S_IFREG, stat.S_IFDIR))):
                raise ValueError("unsafe archive entry")
            total += entry.file_size
            if entry.file_size > 300 * 1024 * 1024 or total > MAX_BYTES:
                raise ValueError("archive exceeds extraction limit")
        for entry in entries:
            target = destination.joinpath(*PurePosixPath(entry.filename).parts)
            if entry.is_dir():
                target.mkdir(parents=True, exist_ok=True)
            else:
                target.parent.mkdir(parents=True, exist_ok=True)
                with source.open(entry) as src, target.open("xb") as dst:
                    shutil.copyfileobj(src, dst)
                target.chmod(0o755 if (entry.external_attr >> 16) & 0o111 else 0o644)
    return destination / PREFIX


def validate(stage, sha, version):
    if not re.fullmatch(r"[0-9a-f]{40}", sha) or not re.fullmatch(r"[0-9]+\.[0-9]+\.[0-9]+", version):
        raise ValueError("expected exact source SHA and stable version")
    metadata = json.loads((stage / "build.json").read_text())
    expected = {"source": sha, "version": version, "target": "aarch64-apple-darwin",
                "distribution": "testing-preview", "notarized": False}
    if metadata != expected:
        raise ValueError("build provenance mismatch")
    app = stage / "Boomux.app"
    info = plistlib.loads((app / "Contents/Info.plist").read_bytes())
    if (info.get("CFBundleIdentifier") != BUNDLE_ID or info.get("CFBundleShortVersionString") != version
            or info.get("CFBundleVersion") != version or info.get("LSMinimumSystemVersion") != "15.0"
            or info.get("CFBundleExecutable") != "boomux-launcher"):
        raise ValueError("bundle identity, version or platform mismatch")
    for name in EXECUTABLES:
        path = app / "Contents/MacOS" / name
        if not path.is_file() or not path.stat().st_mode & 0o111:
            raise ValueError("missing executable")
    return app, metadata


def command(args, timeout=120):
    # No shell, dependency install, or executable from the incoming bundle.
    return subprocess.run(args, check=True, timeout=timeout, capture_output=True, text=True)


def sign(stage, identity, profile, keychain, team, run=command):
    if not identity.startswith("Developer ID Application:") or not re.fullmatch(r"[A-Z0-9]{10}", team):
        raise ValueError("explicit Developer ID Application identity and Team ID required")
    app = stage / "Boomux.app"
    flags = ["--force", "--sign", identity, "--options", "runtime", "--timestamp", "--keychain", str(keychain)]
    for name in EXECUTABLES:
        run(["/usr/bin/codesign", *flags, str(app / "Contents/MacOS" / name)])
    run(["/usr/bin/codesign", *flags, str(app)])
    run(["/usr/bin/codesign", "--verify", "--deep", "--strict", str(app)])
    details = run(["/usr/bin/codesign", "--display", "--verbose=4", str(app)])
    if f"TeamIdentifier={team}" not in details.stderr.splitlines():
        raise ValueError("signing identity has unexpected Team ID")
    submission = stage.parent / "notarization.zip"
    run(["/usr/bin/ditto", "-c", "-k", "--sequesterRsrc", "--keepParent", str(app), str(submission)])
    result = run(["/usr/bin/xcrun", "notarytool", "submit", str(submission), "--keychain-profile", profile,
                  "--keychain", str(keychain), "--wait", "--timeout", "20m", "--output-format", "json"], timeout=1250)
    if json.loads(result.stdout).get("status") != "Accepted":
        raise ValueError("notarization was not accepted")
    run(["/usr/bin/xcrun", "stapler", "staple", str(app)])
    run(["/usr/bin/xcrun", "stapler", "validate", str(app)])
    run(["/usr/bin/codesign", "--verify", "--deep", "--strict", str(app)])
    run(["/usr/sbin/spctl", "--assess", "--type", "execute", "--verbose=2", str(app)])


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    for name in ("archive", "output", "source", "version", "identity", "profile", "keychain", "team"):
        parser.add_argument("--" + name, required=True)
    args = parser.parse_args()
    if os.uname().sysname != "Darwin":
        raise SystemExit("Signing and notarization require macOS")
    output = Path(args.output).resolve()
    if output.exists():
        raise SystemExit("Refusing to overwrite a signed artifact")
    output.parent.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix="boomux-sign-") as temporary:
        stage = unpack(args.archive, Path(temporary))
        _, metadata = validate(stage, args.source, args.version)
        sign(stage, args.identity, args.profile, Path(args.keychain), args.team)
        metadata.update(distribution="developer-id", notarized=True)
        (stage / "build.json").write_text(json.dumps(metadata, indent=2) + "\n")
        command(["/usr/bin/ditto", "-c", "-k", "--sequesterRsrc", "--keepParent", str(stage), str(output)])
    with output.open("rb") as source:
        digest = hashlib.file_digest(source, "sha256").hexdigest()
    output.with_suffix(output.suffix + ".sha256").write_text(f"{digest}  {output.name}\n")
    print(output)


if __name__ == "__main__":
    main()

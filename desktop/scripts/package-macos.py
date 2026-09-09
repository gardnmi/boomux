"""Create a self-contained, ad-hoc-signed macOS testing preview."""
import hashlib
import json
from pathlib import Path
import platform
import plistlib
import shutil
import subprocess
import tomllib

ROOT = Path(__file__).resolve().parents[2]

def main():
    if platform.system() != "Darwin":
        raise SystemExit("Package on macOS so signatures and executable smoke checks are native")
    arch = {"arm64": "aarch64", "x86_64": "x86_64"}[platform.machine()]
    version = tomllib.loads((ROOT / "Cargo.toml").read_text())["package"]["version"]
    sha = subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=ROOT, text=True).strip()
    dist = ROOT / "dist/macos"
    dist.mkdir(parents=True, exist_ok=True)
    stage = dist / "Boomux macOS Preview"
    if stage.exists():
        shutil.rmtree(stage)
    contents = stage / "Boomux.app/Contents"
    binaries = contents / "MacOS"
    resources = contents / "Resources"
    binaries.mkdir(parents=True)
    resources.mkdir()
    for name in ["boomux", "boomux-desktop"]:
        source = ROOT / "target/release" / name
        actual = subprocess.check_output([source, "--version"], text=True).strip()
        if actual != f"{name} {version}":
            raise RuntimeError(f"Unexpected version: {actual}")
        shutil.copy2(source, binaries / name)
    shutil.copy2(ROOT / "desktop/packaging/macos/boomux-launcher", binaries / "boomux-launcher")
    (binaries / "boomux-launcher").chmod(0o755)
    (contents / "Info.plist").write_bytes(plistlib.dumps({
        "CFBundleIdentifier": "com.boomux.desktop.preview",
        "CFBundleName": "Boomux",
        "CFBundleDisplayName": "Boomux Preview",
        "CFBundleExecutable": "boomux-launcher",
        "CFBundlePackageType": "APPL",
        "CFBundleShortVersionString": version,
        "CFBundleVersion": version,
        "LSMinimumSystemVersion": "15.0",
        "NSHighResolutionCapable": True,
        "NSPrincipalClass": "NSApplication",
        "NSAppleEventsUsageDescription": "Boomux opens terminal attachments when requested.",
    }))
    for name in ["LICENSE", "THIRD_PARTY_NOTICES.md"]:
        shutil.copy2(ROOT / name, resources / name)
    shutil.copy2(ROOT / "docs/platforms/macos-testing.md", stage / "READ ME FIRST.md")
    metadata = {"source": sha, "version": version, "target": f"{arch}-apple-darwin", "distribution": "testing-preview", "notarized": False}
    (stage / "build.json").write_text(json.dumps(metadata, indent=2) + "\n")
    for name in ["boomux", "boomux-desktop"]:
        subprocess.run(["codesign", "--force", "--sign", "-", str(binaries / name)], check=True)
    subprocess.run(["codesign", "--force", "--sign", "-", str(contents.parent)], check=True)
    subprocess.run(["codesign", "--verify", "--deep", "--strict", str(contents.parent)], check=True)
    subprocess.run([str(binaries / "boomux-desktop"), "--check-runtime"], check=True)
    archive = dist / f"boomux-macos-preview-{arch}-{sha[:8]}.zip"
    subprocess.run(["ditto", "-c", "-k", "--sequesterRsrc", "--keepParent", str(stage), str(archive)], check=True)
    digest = hashlib.file_digest(archive.open("rb"), "sha256").hexdigest()
    archive.with_suffix(".zip.sha256").write_text(f"{digest}  {archive.name}\n")
    print(archive)

if __name__ == "__main__":
    main()

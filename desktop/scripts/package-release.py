"""Package Desktop with the exact CLI bytes from the same release candidate."""

import hashlib
from pathlib import Path
import platform
import shutil
import subprocess
import sys
import tarfile
import tempfile
import tomllib

ROOT = Path(__file__).resolve().parents[2]
TARGET = "x86_64-unknown-linux-gnu"
ASSET = f"boomux-desktop-{TARGET}.tar.gz"


def digest(path):
    with path.open("rb") as source:
        return hashlib.file_digest(source, "sha256").hexdigest()


def package(archive, root=ROOT):
    if (platform.system(), platform.machine()) != ("Linux", "x86_64"):
        raise ValueError("Desktop packaging requires Linux x86_64")
    version = tomllib.loads((root / "Cargo.toml").read_text())["package"]["version"]
    desktop = root / "desktop"
    if tomllib.loads((desktop / "Cargo.toml").read_text())["package"]["version"] != version:
        raise ValueError("Desktop and Boomux versions differ")
    name = f"boomux-v{version}-{TARGET}"
    if archive.name != name + ".tar.gz":
        raise ValueError("CLI archive must match the workspace version and target")
    if Path(str(archive) + ".sha256").read_text().strip() != f"{digest(archive)}  {archive.name}":
        raise ValueError("CLI archive checksum mismatch")
    binary = root / "target" / TARGET / "release/boomux-desktop"
    actual = subprocess.check_output([binary, "--version"], text=True, timeout=10).strip()
    if actual != f"boomux-desktop {version}":
        raise ValueError("Desktop binary version differs from its manifest")
    sha = subprocess.check_output(["git", "-C", root, "rev-parse", "HEAD"], text=True).strip()
    dist = root / "dist"
    dist.mkdir(exist_ok=True)
    with tempfile.TemporaryDirectory(prefix="boomux-package-") as temporary:
        stage = Path(temporary)
        with tarfile.open(archive) as tar:
            tar.extractall(stage / "cli", filter="data")
        cli = stage / "cli" / name / "boomux"
        if cli.is_symlink() or not cli.is_file():
            raise ValueError("CLI archive contains no regular executable")
        if subprocess.check_output([cli, "--version"], text=True, timeout=10).strip() != f"boomux {version}":
            raise ValueError("CLI binary version differs from its archive")
        bundle = stage / "bundle"
        (bundle / "bin").mkdir(parents=True)
        (bundle / "libexec").mkdir()
        shutil.copy2(cli, bundle / "bin/boomux")
        shutil.copy2(binary, bundle / "libexec/boomux-desktop")
        shutil.copy2(desktop / "packaging/boomux-desktop", bundle / "bin/boomux-desktop")
        (bundle / "bin/boomux-desktop").chmod(0o755)
        shutil.copytree(desktop / "packaging/share", bundle / "share")
        shutil.copy2(desktop / "LICENSE", bundle / "LICENSE")
        shutil.copy2(root / "LICENSE", bundle / "LICENSE.boomux")
        shutil.copy2(root / "THIRD_PARTY_NOTICES.md", bundle / "THIRD_PARTY_NOTICES.md")
        (bundle / "release.txt").write_text(
            f"boomux-desktop {version}\nboomux {version}\nsource {sha}\nboomux sha256 {digest(cli)}\n")
        with tarfile.open(dist / ASSET, "w:gz") as tar:
            for path in sorted(bundle.iterdir()):
                tar.add(path, arcname=path.name)
    (dist / (ASSET + ".sha256")).write_text(f"{digest(dist / ASSET)}  {ASSET}\n")


if __name__ == "__main__":
    if len(sys.argv) != 2:
        raise SystemExit("usage: package-release.sh CLI_ARCHIVE")
    package(Path(sys.argv[1]).resolve())

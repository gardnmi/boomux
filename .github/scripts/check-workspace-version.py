"""Reject partial release merges before building the workspace."""

import json
from pathlib import Path
import tomllib


def check(root):
    version = tomllib.loads((root / "Cargo.toml").read_text())["package"]["version"]
    desktop = tomllib.loads((root / "desktop/Cargo.toml").read_text())["package"]["version"]
    released = json.loads((root / ".release-please-manifest.json").read_text())["."]
    packages = tomllib.loads((root / "Cargo.lock").read_text())["package"]
    locked = [p["version"] for p in packages
              if p["name"] in {"boomux", "boomux-desktop"} and "source" not in p]
    if desktop != version or released != version or locked != [version, version]:
        raise ValueError("Synchronize Boomux, Desktop, Cargo.lock, and the release manifest after merging a release PR")


if __name__ == "__main__":
    check(Path(__file__).resolve().parents[2])

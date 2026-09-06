"""Render a release-pinned Desktop installer without changing user overrides."""

from pathlib import Path
import re
import sys


def render(tag, destination):
    if not re.fullmatch(r"v(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)", tag):
        raise ValueError("expected strict vMAJOR.MINOR.PATCH")
    source = Path(__file__).resolve().parents[1] / "install.sh"
    text = source.read_text()
    marker = "version=${BOOMUX_DESKTOP_VERSION:-} # release-version"
    if text.count(marker) != 1:
        raise ValueError("missing unique installer version marker")
    destination.write_text(text.replace(marker, f"version=${{BOOMUX_DESKTOP_VERSION:-{tag}}} # release-version"))
    destination.chmod(0o755)


if __name__ == "__main__":
    render(sys.argv[1], Path(sys.argv[2]))

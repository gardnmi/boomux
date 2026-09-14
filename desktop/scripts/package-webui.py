"""Stage the gateway and its exact runtime assets; no source checkout at runtime."""
from pathlib import Path
import shutil

ASSETS = [
    *[f"poc/webgpu-tiling/{name}" for name in (
        "index.html", "app.js", "desktop-panels.js", "themes.js", "terminal.js",
        "renderer.js", "layout.js", "style.css", "THIRD_PARTY_NOTICES.md",
        "fonts/jetbrains-mono-nerd.woff2", "fonts/OFL.txt")],
    "node_modules/ghostty-web/LICENSE",
    "node_modules/@fontsource/jetbrains-mono/LICENSE",
    "node_modules/ghostty-web/dist/ghostty-web.js",
    "node_modules/ghostty-web/ghostty-vt.wasm",
    "node_modules/@fontsource/jetbrains-mono/files/jetbrains-mono-latin-400-normal.woff2",
]


def stage_webui(root, gateway, binaries, assets):
    if not gateway.is_file():
        raise ValueError(f"Missing web gateway: {gateway}")
    for name in ASSETS:
        source = root / name
        if not source.is_file():
            raise ValueError(f"Missing web UI asset: {source}")
        destination = assets / name
        destination.parent.mkdir(parents=True, exist_ok=True)
        shutil.copy2(source, destination)
    shutil.copy2(gateway, binaries / "webgpu_gateway")

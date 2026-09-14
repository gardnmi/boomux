"""Check a real gateway relocated into Linux/macOS layouts without daemon access."""
import os
from pathlib import Path
import runpy
import subprocess
import tempfile

ROOT = Path(__file__).resolve().parents[2]
PACKAGE = runpy.run_path(str(Path(__file__).with_name("package-webui.py")))
gateway = Path(os.environ["BOOMUX_WEB_GATEWAY"]).resolve()
with tempfile.TemporaryDirectory(prefix="boomux-webui-bundle-") as temporary:
    for binary_dir, asset_dir in [("libexec", "share/boomux/webui"), ("Contents/MacOS", "Contents/Resources/webui")]:
        bundle = Path(temporary) / binary_dir.replace("/", "-")
        binaries, assets = bundle / binary_dir, bundle / asset_dir
        binaries.mkdir(parents=True)
        PACKAGE["stage_webui"](ROOT, gateway, binaries, assets)
        command = [str(binaries / "webgpu_gateway"), "--check-assets"]
        result = subprocess.run(command, cwd=temporary, capture_output=True, text=True, check=True)
        assert str(binaries) in result.stdout, result.stdout
        # A missing asset must fail even while the developer checkout is available.
        (assets / "poc/webgpu-tiling/app.js").unlink()
        assert subprocess.run(command, cwd=temporary, capture_output=True).returncode != 0
print("Relocated Linux/macOS layouts load bundled assets and reject incomplete bundles")

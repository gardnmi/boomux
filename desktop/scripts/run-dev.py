#!/usr/bin/env python3
"""Build and open Desktop against this worktree's isolated development daemon."""

import json
import os
from pathlib import Path
import subprocess
import sys


def main():
    root = Path(__file__).resolve().parents[2]
    subprocess.run(["cargo", "build", "--locked", "-p", "boomux", "-p", "boomux-desktop", "--target-dir", str(root / "target")],
                   cwd=root, check=True)
    target = root / "target/debug"
    env = {key: value for key, value in os.environ.items() if not key.startswith("BOOMUX_")}
    display = env.get("WAYLAND_DISPLAY")
    if display and not Path(display).is_absolute() and env.get("XDG_RUNTIME_DIR"):
        env["WAYLAND_DISPLAY"] = str(Path(env["XDG_RUNTIME_DIR"]) / display)
    for name in ["RUNTIME_DIR", "CONFIG_HOME", "STATE_HOME", "DATA_HOME", "CACHE_HOME"]:
        path = root / "target/desktop-dev" / name.lower()
        path.mkdir(parents=True, exist_ok=True, mode=0o700)
        env[f"XDG_{name}"] = str(path)
    env["PATH"] = str(target) + os.pathsep + env.get("PATH", "")
    print(f"Development runtime: {env['XDG_RUNTIME_DIR']}", flush=True)
    cli = target / "boomux"
    status = subprocess.run([cli, "daemon", "status", "--json"], env=env,
                            check=True, timeout=30, capture_output=True, text=True)
    state = json.loads(status.stdout)["data"]["status"]
    if state not in {"running", "stopped"}:
        raise RuntimeError(f"Unexpected development daemon status: {state}")
    # Start does not reload an existing daemon; explicitly select this build for handoff.
    action = ["restart", "--executable", str(cli)] if state == "running" else ["start"]
    subprocess.run([cli, "daemon", *action], env=env, check=True, timeout=30)
    executable = str(target / "boomux-desktop")
    os.execve(executable, [executable, *sys.argv[1:]], env)


if __name__ == "__main__":
    main()

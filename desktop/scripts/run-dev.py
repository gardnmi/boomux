#!/usr/bin/env python3
"""Build and open Desktop against this worktree's isolated development daemon."""

import argparse
import json
import os
from pathlib import Path
import subprocess


def main():
    parser = argparse.ArgumentParser(description=__doc__, add_help=False)
    parser.add_argument("--release", action="store_true")
    options, desktop_args = parser.parse_known_args()
    root = Path(__file__).resolve().parents[2]
    subprocess.run(["cargo", "build", "--locked", "-p", "boomux", "-p", "boomux-desktop", "--target-dir", str(root / "target"), *(["--release"] if options.release else [])],
                   cwd=root, check=True)
    target = root / "target" / ("release" if options.release else "debug")
    env = {key: value for key, value in os.environ.items() if not key.startswith("BOOMUX_")}
    # Only Boomux reads these overrides. Terminal applications retain the user's
    # XDG configuration, data, cache, runtime services, and authentication.
    for name in ["RUNTIME_DIR", "CONFIG_HOME", "STATE_HOME"]:
        path = root / "target/desktop-dev" / name.lower()
        path.mkdir(parents=True, exist_ok=True, mode=0o700)
        env[f"BOOMUX_{name}"] = str(path)
    env["PATH"] = str(target) + os.pathsep + env.get("PATH", "")
    print(f"Development runtime: {env['BOOMUX_RUNTIME_DIR']}", flush=True)
    cli = target / "boomux"
    status = subprocess.run([cli, "daemon", "status", "--json"], env=env,
                            check=True, timeout=30, capture_output=True, text=True)
    state = json.loads(status.stdout)["data"]["status"]
    if state not in {"running", "stopped"}:
        raise RuntimeError(f"Unexpected development daemon status: {state}")
    # Start does not reload an existing daemon; explicitly select this build for handoff.
    action = ["restart", "--executable", str(cli), "--refresh-environment"] if state == "running" else ["start"]
    subprocess.run([cli, "daemon", *action], env=env, check=True, timeout=30)
    executable = str(target / "boomux-desktop")
    os.execve(executable, [executable, *desktop_args], env)


if __name__ == "__main__":
    main()

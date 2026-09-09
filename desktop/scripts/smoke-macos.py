"""Exercise the packaged macOS app, CLI, and daemon in a private test runtime."""
import json
import os
from pathlib import Path
import platform
import runpy
import subprocess
import tempfile
import time

ROOT = Path(__file__).resolve().parents[2]
helpers = runpy.run_path(str(ROOT / "desktop/scripts/smoke-desktop.py"))
wait_for, stop, created_id = (helpers[name] for name in ["wait_for", "stop", "created_id"])


def main():
    if platform.system() != "Darwin":
        raise SystemExit("This smoke check requires a logged-in macOS graphics session")
    binaries = ROOT / "dist/macos/Boomux macOS Preview/Boomux.app/Contents/MacOS"
    output = ROOT / "dist/macos/smoke"
    output.mkdir(parents=True, exist_ok=True)
    apps, logs = [], []
    with tempfile.TemporaryDirectory(prefix="boomux-smoke-", dir="/tmp") as temporary:
        root = Path(temporary).resolve()
        env = dict(os.environ)
        for name in list(env):
            if name.startswith("BOOMUX_") or name in ["ZDOTDIR", "ENV", "BASH_ENV"]:
                del env[name]
        for name, directory in [("HOME", "home"), ("XDG_RUNTIME_DIR", "run"),
                                ("XDG_CONFIG_HOME", "config"), ("XDG_STATE_HOME", "state")]:
            path = root / directory
            path.mkdir(mode=0o700)
            env[name] = str(path)
        env["PATH"] = str(binaries) + os.pathsep + env.get("PATH", "/usr/bin:/bin")
        env["SHELL"] = "/bin/zsh"

        def cli(*args, check=True):
            result = subprocess.run([binaries / "boomux", *args], env=env, cwd=root,
                                    capture_output=True, text=True, timeout=15)
            if check and result.returncode:
                raise RuntimeError(f"CLI {args}: {result.stderr}")
            return result.stdout

        def launch(name):
            ready = root / f"{name}.ready"
            log = (output / f"{name}.log").open("w")
            logs.append(log)
            app = subprocess.Popen([binaries / "boomux-launcher", "--update-ready", ready],
                                   env=env, cwd=root, stdout=log, stderr=subprocess.STDOUT)
            apps.append(app)
            wait_for(name, ready.exists, [app])
            return app

        def inspect():
            return json.loads(cli("--json", "shell", "inspect", shell_id))["data"]["shell"]

        try:
            app = launch("empty-start")
            status = json.loads(cli("--json", "daemon", "status"))["data"]
            assert status["status"] == "running" and status["pid"] is not None, status
            assert status["socket_path"] == str(root / "run/boomux/daemon.sock"), status
            stop(app)
            workspace = created_id(cli("workspace", "create", "macos-smoke"))
            shell_id = created_id(cli("shell", "create", workspace, "--name", "smoke",
                                      "--cwd", str(root), "--", "/bin/zsh", "-f", "-c",
                                      "printf 'macos-desktop-ready\\n'; exec /bin/sleep 180"))
            env["BOOMUX_DESKTOP_SHELL_ID"] = shell_id
            app = launch("shell-attach")
            wait_for("PTY output", lambda: inspect()["status"] == "running" and
                     (inspect().get("run") or {}).get("output_revision", 0) > 0, [app])
            run = inspect()["run"]["id"]
            settling = time.monotonic() + 3
            wait_for("startup settling", lambda: time.monotonic() >= settling, [app], seconds=5)
            stop(app)
            assert inspect()["run"]["id"] == run and inspect()["status"] == "running"
            app = launch("shell-reattach")
            assert inspect()["run"]["id"] == run and inspect()["status"] == "running"
            cli("daemon", "restart")
            assert inspect()["run"]["id"] == run and inspect()["status"] == "running"
            settling = time.monotonic() + 2
            wait_for("post-restart window", lambda: time.monotonic() >= settling, [app], seconds=5)
            subprocess.run(["/usr/sbin/screencapture", "-x", output / "desktop.png"], timeout=10)
            report = dict(status="passed", macos=platform.mac_ver()[0], chip=platform.machine(),
                          run_id=run, checks=["bundle startup", "native window creation", "PTY output",
                                             "close/reopen preserves ShellRun", "live daemon restart"])
            (output / "result.json").write_text(json.dumps(report, indent=2) + "\n")
            print(json.dumps(report), flush=True)
        finally:
            for app in reversed(apps):
                stop(app)
            cli("daemon", "stop", check=False)
            for log in logs:
                log.close()


if __name__ == "__main__":
    main()

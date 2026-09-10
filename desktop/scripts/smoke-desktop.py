"""Run a release bundle against an isolated X11 or Wayland display and daemon."""

import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import signal
import shlex
import subprocess
import tarfile
import tempfile
import time


def wait_for(description, predicate, processes, seconds=30):
    deadline = time.monotonic() + seconds
    while time.monotonic() < deadline:
        for process in processes:
            if process.poll() is not None:
                code = process.returncode
                detail = signal.Signals(-code).name if code < 0 else str(code)
                raise RuntimeError(f"{description}: process exited with {detail}")
        if predicate():
            return
        time.sleep(0.2)
    raise RuntimeError(f"timed out waiting for {description}")


def wayland_frame_presented(log):
    if 'set_app_id("org.omarchy.boomux-desktop")' not in log:
        return False
    # Follow a toplevel's surface, rather than accepting a cursor buffer or an
    # initial registry sync as evidence that the app submitted a window frame.
    for surface in re.findall(r"get_xdg_surface\([^\n]*wl_surface[@#](\d+)", log):
        attached = re.search(rf"wl_surface[@#]{surface}\.attach\(wl_buffer[@#]\d+", log)
        if attached:
            committed = re.search(rf"wl_surface[@#]{surface}\.commit\(\)", log[attached.end():])
            if committed and re.search(r"wl_callback[@#]\d+\.done\(",
                                       log[attached.end() + committed.end():]):
                return True
    return False


def created_id(output):
    identities = re.findall(r"\(([0-9a-f]{8}(?:-[0-9a-f]{4}){3}-[0-9a-f]{12})\)", output)
    if len(identities) != 1:
        raise RuntimeError(f"expected one exact resource ID in CLI response: {output}")
    return identities[0]


def stop(process):
    if process.poll() is None:
        process.terminate()
        try:
            process.wait(timeout=5)
        except subprocess.TimeoutExpired:
            process.kill()
            process.wait(timeout=5)


def check_config_editor(bundle, env, root):
    """Exercise Desktop's editor bridge through Boomux's real transaction."""
    request = root / "config-editor-request"
    request.mkdir(mode=0o700)
    target = root / "config-editor.toml"
    child_env = dict(env, BOOMUX_CONFIG=str(target))
    child_env["VISUAL"] = shlex.join([
        str(bundle / "libexec/boomux-desktop"), "--boomux-settings-editor", str(request)])
    (request / "target").write_text(str(target))

    def edit(baseline, candidate, expected_success):
        baseline_path = request / "baseline"
        if baseline is None:
            baseline_path.unlink(missing_ok=True)
        else:
            baseline_path.write_text(baseline)
        (request / "candidate").write_text(candidate)
        result = subprocess.run([bundle / "bin/boomux", "config", "edit"], env=child_env,
                                capture_output=True, text=True, timeout=15)
        if (result.returncode == 0) != expected_success:
            raise RuntimeError(f"config editor transaction: {result.stderr}")

    initial = "# user preferences\n[notifications]\nenabled = false # retain comment\n"
    edit(None, initial, True)
    changed = initial.replace("false", "true")
    edit(initial, changed, True)
    if target.read_text() != changed:
        raise RuntimeError("config editor did not commit exact candidate")
    edit(initial, initial, False)  # stale UI snapshot
    edit(changed, "[projects]\nmax_depth = 99\n", False)  # owner validation
    if target.read_text() != changed:
        raise RuntimeError("rejected edit changed the Boomux configuration")
    print("PASS: Boomux settings save, conflict detection, and validation", flush=True)


def assert_emulated(pid):
    executable = Path(f"/proc/{pid}/exe").resolve(strict=True).name
    if not executable.startswith("qemu-x86_64"):
        raise RuntimeError(f"process {pid} escaped CPU emulation: {executable}")


def check_bundle_ownership(bundle, env, root):
    tools = root / "update-tools"
    tools.mkdir()
    curl = tools / "curl"
    curl.write_text("#!/bin/sh\nprevious=\noutput=\nfor argument do\n"
                    "  [ \"$previous\" != --output ] || output=$argument\n  previous=$argument\ndone\n"
                    "[ -n \"$output\" ] || exit 64\n"
                    "printf '%s' '{\"tag_name\":\"v99.0.0\"}' > \"$output\"\n")
    curl.chmod(0o755)
    local_bin = Path(env["HOME"]) / ".local/bin"
    local_bin.mkdir(parents=True)
    linked = local_bin / "boomux"
    linked.symlink_to(bundle / "bin/boomux")
    fixture_env = dict(env, PATH=str(tools) + ":/usr/bin:/bin")
    for executable in [bundle / "bin/boomux", linked]:
        result = subprocess.run([executable, "--json", "update", "status"], env=fixture_env,
                                capture_output=True, text=True, check=True, timeout=20)
        data = json.loads(result.stdout)["data"]
        if data["state"] != "ineligible" or data["install_kind"] == "github_release":
            raise RuntimeError("CLI updater claims ownership of the Desktop bundle")
    print("PASS: bundle-owned CLI and symlink are ineligible for self-update", flush=True)


def restored_layout_matches(document, shell_id, pending_id):
    """Require the saved live arrangement, independent of remapped pane IDs."""
    arrangement = document["arrangements"][document["active"]]
    split = arrangement.get("tree", {}).get("Split", {})
    if split.get("ratio") != 0.31 or not split.get("horizontal"):
        return False
    panes = arrangement["panes"]
    left, right = split["first"]["Pane"], split["second"]["Pane"]
    return (panes[str(left)]["shell"] == shell_id
            and panes[str(right)]["shell"] == pending_id
            and arrangement["focused"] == left
            and arrangement["expanded"] == left
            and len(arrangement["floating"]) == 1
            and arrangement["floating"][0]["rect"][2:] == [300.0, 200.0]
            and panes[str(arrangement["floating"][0]["pane"])]["shell"] == "remote:offline:missing"
            and "minimized-missing" in document["minimized"])


def smoke(backend, archive, output, software_driver=None, cpu_model=None):
    output.mkdir(parents=True, exist_ok=True)
    expected = Path(str(archive) + ".sha256").read_text().split()[0]
    with archive.open("rb") as source:
        actual = hashlib.file_digest(source, "sha256").hexdigest()
    if actual != expected:
        raise RuntimeError("release archive checksum mismatch")

    with tempfile.TemporaryDirectory(prefix="boomux-smoke-") as directory:
        root = Path(directory)
        bundle = root / "bundle"
        bundle.mkdir()
        with tarfile.open(archive) as tar:
            tar.extractall(bundle, filter="data")
        env = {key: value for key, value in os.environ.items()
               if not key.startswith(("BOOMUX_", "HYPRLAND_", "VK_", "ZED_"))
               and key not in {"DISPLAY", "WAYLAND_DISPLAY", "WAYLAND_SOCKET", "WAYLAND_DEBUG",
                               "DBUS_SESSION_BUS_ADDRESS", "DBUS_SESSION_BUS_PID", "XAUTHORITY",
                               "SESSION_MANAGER", "DESKTOP_SESSION", "XDG_CURRENT_DESKTOP"}}
        for name in ["RUNTIME_DIR", "CONFIG_HOME", "STATE_HOME", "DATA_HOME", "CACHE_HOME"]:
            path = root / name.lower()
            path.mkdir(mode=0o700)
            env[f"XDG_{name}"] = str(path)
        env.update(LIBGL_ALWAYS_SOFTWARE="1", GALLIUM_DRIVER="llvmpipe",
                   XDG_SESSION_TYPE=backend, SHELL="/bin/sh", RUST_BACKTRACE="1")
        drivers = ([software_driver.resolve()] if software_driver else
                   sorted(Path("/usr/share/vulkan/icd.d").glob("lvp*.json")))
        if not drivers:
            raise RuntimeError("Mesa lavapipe is required (install mesa-vulkan-drivers)")
        env["VK_DRIVER_FILES"] = str(drivers[0])
        env["VK_ICD_FILENAMES"] = str(drivers[0])
        home = root / "home"
        home.mkdir()
        env["HOME"] = str(home)
        if not cpu_model:
            check_config_editor(bundle, env, root)
            check_bundle_ownership(bundle, env, root)
        processes, apps, logs = [], [], []
        shell_id = None
        emulated_daemon = None
        emulated_bin = root / "emulated-bin"
        if cpu_model:
            emulated_bin.mkdir()
            wrapper = emulated_bin / "boomux"
            wrapper.write_text("#!/bin/sh\nexec " + shlex.join([
                "qemu-x86_64", "-cpu", cpu_model, str(bundle / "bin/boomux")]) + ' "$@"\n')
            wrapper.chmod(0o755)

        def start(command, name, child_env=None, **kwargs):
            log = (output / f"{name}.log").open("wb")
            logs.append(log)
            process = subprocess.Popen(command, env=child_env or env, cwd=root,
                                       stdout=log, stderr=subprocess.STDOUT, **kwargs)
            processes.append(process)
            return process

        def cli(*args, check=True):
            command = [bundle / "bin/boomux", *args]
            if cpu_model:
                command = ["qemu-x86_64", "-cpu", cpu_model, *command]
            result = subprocess.run(command, env=env, cwd=root,
                                    capture_output=True, text=True, timeout=10)
            with (output / "boomux.log").open("a") as log:
                log.write(f"{args!r}\n{result.stdout}{result.stderr}\n")
            if check and result.returncode:
                raise RuntimeError(f"Boomux command failed: {args}: {result.stderr}")
            return result.stdout

        def inspect():
            return json.loads(cli("--json", "shell", "inspect", shell_id))["data"]["shell"]

        try:
            if cpu_model:
                # Start the daemon itself under emulation: wrapping `daemon start`
                # would allow its subsequent exec to run natively on this host.
                emulated_daemon = start(["qemu-x86_64", "-cpu", cpu_model,
                                         str(bundle / "bin/boomux"), "daemon", "run"], "emulated-daemon")
                wait_for("emulated daemon socket", lambda: (Path(env["XDG_RUNTIME_DIR"]) / "boomux/daemon.sock").is_socket(), [emulated_daemon])
            # An owned bus with no activation directories cannot launch the
            # host's portal/desktop services using an inherited session.
            bus_config = root / "dbus.conf"
            bus_config.write_text(
                '<busconfig><type>session</type>'
                f'<listen>unix:tmpdir={env["XDG_RUNTIME_DIR"]}</listen>'
                '<policy context="default"><allow send_destination="*"/>'
                '<allow receive_sender="*"/><allow own="*"/></policy></busconfig>')
            bus_address = root / "bus-address"
            with bus_address.open("wb") as descriptor:
                bus = start(["dbus-daemon", "--nofork", f"--config-file={bus_config}",
                             f"--print-address={descriptor.fileno()}"], "dbus",
                            pass_fds=(descriptor.fileno(),))
            wait_for("private D-Bus", lambda: bus_address.read_text().strip(), [bus])
            env["DBUS_SESSION_BUS_ADDRESS"] = bus_address.read_text().strip()
            # Xvfb chooses a free display number. Weston uses its X11 backend
            # to provide the input seat GPUI requires, still without hardware.
            display_file = root / "display"
            with display_file.open("wb") as descriptor:
                display = start(["Xvfb", "-displayfd", str(descriptor.fileno()), "-screen", "0",
                                 "1280x800x24", "-nolisten", "tcp", "-ac"], "xvfb",
                                pass_fds=(descriptor.fileno(),))
            wait_for("Xvfb readiness", lambda: display_file.read_text().strip(), [display])
            env["DISPLAY"] = ":" + display_file.read_text().strip()
            servers = [bus, display]
            if backend == "wayland":
                weston = start(["weston", "--backend=x11", "--renderer=pixman",
                                "--shell=kiosk-shell.so",
                                "--socket=wayland-smoke", "--no-config", "--idle-time=0",
                                "--width=1280", "--height=800"], "weston")
                servers.append(weston)
                socket = Path(env["XDG_RUNTIME_DIR"]) / "wayland-smoke"
                wait_for("Weston readiness", socket.is_socket, servers)
                env["WAYLAND_DISPLAY"] = "wayland-smoke"
                # No X11 fallback is possible for the application.
                del env["DISPLAY"]

            def launch(name):
                child_env = dict(env)
                if backend == "wayland":
                    child_env["WAYLAND_DEBUG"] = "client"
                command = [bundle / "bin/boomux-desktop"]
                if cpu_model:
                    # QEMU executes ELF binaries, so reproduce the bundled launcher's
                    # environment and daemon-start step before emulating Desktop.
                    cli("daemon", "start")
                    # Desktop also launches CLI helpers (for example update
                    # checks). Keep those invocations inside CPU emulation.
                    child_env["PATH"] = str(emulated_bin) + os.pathsep + child_env.get("PATH", "")
                    command = ["qemu-x86_64", "-cpu", cpu_model, str(bundle / "libexec/boomux-desktop")]
                app = start(command, name, child_env)
                apps.append(app)
                log_path = output / f"{name}.log"

                def visible():
                    if backend == "wayland":
                        return wayland_frame_presented(log_path.read_text(errors="replace"))
                    tree = subprocess.run(["xwininfo", "-root", "-tree"], env=env,
                                          capture_output=True, text=True, timeout=5).stdout
                    (output / f"{name}-windows.txt").write_text(tree)
                    for window in re.findall(r'(0x[0-9a-f]+) "[^"\n]*Boomux Desktop[^"\n]*"', tree):
                        details = subprocess.run(["xwininfo", "-id", window], env=env,
                                                 capture_output=True, text=True, timeout=5).stdout
                        if "Map State: IsViewable" in details:
                            return True
                    return False

                wait_for(f"{backend} window/frame", visible, [*servers, app])
                return app

            # Exercise startup on a clean runtime, including the launcher on native runs.
            app = launch("empty-start")
            status = json.loads(cli("--json", "daemon", "status"))["data"]
            if status["status"] != "running":
                raise RuntimeError(f"launcher did not start Boomux: {status}")
            if status["socket_path"] != str(Path(env["XDG_RUNTIME_DIR"]) / "boomux/daemon.sock"):
                raise RuntimeError("daemon did not use the isolated runtime")
            daemon_pid = status["pid"]
            if cpu_model:
                assert_emulated(emulated_daemon.pid)
                assert_emulated(app.pid)
                if daemon_pid != emulated_daemon.pid:
                    raise RuntimeError("Boomux escaped CPU emulation")
            if daemon_pid is None:
                raise RuntimeError("could not identify the isolated daemon process")
            stop(app)
            workspace_id = created_id(cli("workspace", "create", "desktop-smoke"))
            shell_id = created_id(cli("shell", "create", workspace_id, "--name", "smoke-shell",
                                      "--cwd", str(root), "--", "/bin/sh", "-c",
                                      "printf 'boomux-desktop-smoke-ready\\n'; exec sleep 180"))
            if inspect()["status"] != "pending":
                raise RuntimeError("fixture Shell must be pending before Desktop attaches")
            env["BOOMUX_DESKTOP_SHELL_ID"] = shell_id
            app = launch("shell-attach")
            wait_for("Desktop attachment and PTY output",
                     lambda: inspect()["status"] == "running" and
                     (inspect().get("run") or {}).get("output_revision", 0) > 0,
                     [*servers, app])
            run_id = inspect()["run"]["id"]
            # Keep the real application active long enough to catch startup failures.
            deadline = time.monotonic() + 3
            wait_for("startup settling", lambda: time.monotonic() >= deadline,
                     [*servers, app], seconds=5)
            stop(app)
            after = inspect()
            if after["status"] != "running" or after["run"]["id"] != run_id:
                raise RuntimeError("exiting Desktop did not preserve the exact ShellRun")
            layout_path = Path(env["XDG_STATE_HOME"]) / "boomux-desktop/layout-state.json"
            saved_layout = json.loads(layout_path.read_text())
            pending_id = created_id(cli("shell", "create", workspace_id, "--name", "restore-must-not-start",
                                        "--cwd", str(root), "--", "/bin/sh", "-c", "exit 99"))
            key = "workspace:" + workspace_id
            saved_layout["active"] = key
            saved_layout["minimized"] = ["minimized-missing"]
            saved_layout["arrangements"][key] = dict(
                tree={"Split": {"horizontal": True, "ratio": 0.31, "first": {"Pane": 9}, "second": {"Pane": 13}}},
                floating=[{"pane": 15, "rect": [80.0, 60.0, 300.0, 200.0]}],
                panes={"9": {"shell": shell_id, "workspace": workspace_id},
                       "13": {"shell": pending_id, "workspace": workspace_id},
                       "15": {"shell": "remote:offline:missing", "workspace": workspace_id}},
                focused=9, expanded=9, canvas=[1000.0, 700.0])
            layout_path.write_text(json.dumps(saved_layout))
            env.pop("BOOMUX_DESKTOP_SHELL_ID", None)
            app = launch("shell-reattach")
            def layout_restored():
                document = json.loads(layout_path.read_text())
                return (document["revision"] != saved_layout["revision"]
                        and restored_layout_matches(document, shell_id, pending_id))
            wait_for("internal layout restoration and durable recapture", layout_restored, [*servers, app])
            (output / "restored-layout.json").write_text(layout_path.read_text())
            pending_shell = json.loads(cli("--json", "shell", "inspect", pending_id))["data"]["shell"]
            if pending_shell["status"] != "pending":
                raise RuntimeError("layout restoration started a pending Shell")
            after = inspect()
            if after["status"] != "running" or after["run"]["id"] != run_id:
                raise RuntimeError("reopening Desktop changed the ShellRun")
            if cpu_model:
                assert_emulated(emulated_daemon.pid)
                assert_emulated(app.pid)
            if json.loads(cli("--json", "daemon", "status"))["data"]["pid"] != daemon_pid:
                raise RuntimeError("reopening Desktop replaced the daemon")
            (output / "result.json").write_text(json.dumps(
                dict(backend=backend, cpu_model=cpu_model, emulated_components=["desktop", "daemon", "cli"] if cpu_model else [], shell_id=shell_id, run_id=run_id, status="passed", layout_restored=True,
                     archive_sha256=actual, boomux_version=cli("--version").strip()), indent=2))
            print(f"PASS: {backend} bundle startup, attachment, and ShellRun survival", flush=True)
        finally:
            # Stop clients first, then only the resources in this private runtime.
            for process in reversed(apps):
                stop(process)
            try:
                if shell_id:
                    cli("shell", "close", shell_id, check=False)
                cli("daemon", "stop", check=False)
            finally:
                for process in reversed(processes):
                    stop(process)
                for log in logs:
                    log.close()


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--backend", choices=["x11", "wayland"], required=True)
    parser.add_argument("--archive", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--software-driver", type=Path,
                        help="lavapipe ICD JSON for a non-system Mesa installation")
    parser.add_argument("--cpu-model", choices=["", "Nehalem"], default="",
                        help="emulate Desktop, the foreground daemon, and CLI on an x86 CPU without AVX")
    args = parser.parse_args()

    def interrupted(signum, _frame):
        raise RuntimeError(f"smoke test interrupted by signal {signum}")

    signal.signal(signal.SIGTERM, interrupted)
    smoke(args.backend, args.archive.resolve(), args.output.resolve(), args.software_driver, args.cpu_model)

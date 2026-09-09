"""Development launches use the rebuilt CLI without touching the ordinary daemon."""

import json
import os
from pathlib import Path
import runpy
import subprocess
import stat
import tempfile
import unittest
from unittest.mock import Mock, patch

DEV = runpy.run_path(str(Path(__file__).with_name("run-dev.py")))
ROOT = Path(DEV["__file__"]).resolve().parents[2]
CLI = ROOT / "target/debug/boomux"


class DevelopmentLaunchTests(unittest.TestCase):
    def launch(self, state, failure=None, gh_config=None, release=False):
        target = ROOT / "target" / ("release" if release else "debug")
        cli = target / "boomux"
        status = Mock(stdout=json.dumps({"data": {"status": state}}))
        with patch("sys.argv", ["run-dev.py", *(["--release"] if release else []), "--example-desktop-flag"]), patch.dict(os.environ, {
            "PATH": "/usr/bin", "XDG_RUNTIME_DIR": "/ordinary/runtime",
            "WAYLAND_DISPLAY": "wayland-1", "BOOMUX_CONFIG": "/ordinary/config",
            "XDG_CONFIG_HOME": "/ordinary/config-home",
            "XDG_DATA_HOME": "/ordinary/data-home",
            "XDG_STATE_HOME": "/ordinary/state-home",
            "XDG_CACHE_HOME": "/ordinary/cache-home",
            "HOME": "/ordinary/home", "KIRO_HOME": "/explicit/kiro",
            **({"GH_CONFIG_DIR": gh_config} if gh_config else {}),
        }, clear=True), patch.object(Path, "mkdir"), patch.object(Path, "lstat", return_value=Mock(st_mode=stat.S_IFDIR | 0o700, st_uid=os.getuid())), \
                patch("subprocess.run", side_effect=[Mock(), status, failure or Mock()]) as run, \
                patch("os.execve") as execute:
            if failure:
                with self.assertRaises(subprocess.CalledProcessError):
                    DEV["main"]()
                execute.assert_not_called()
            else:
                DEV["main"]()
                execute.assert_called_once()
                self.assertEqual(execute.call_args.args[1], [str(target / "boomux-desktop"), "--example-desktop-flag"])
                self.assertEqual(execute.call_args.args[0], str(target / "boomux-desktop"))
                self.assertEqual(execute.call_args.args[2], run.call_args.kwargs["env"])
            calls = run.call_args_list
            self.assertEqual("--release" in calls[0].args[0], release)
            self.assertEqual(calls[1].args[0], [cli, "daemon", "status", "--json"])
            for call in calls[1:]:
                env = call.kwargs["env"]
                self.assertEqual(env["BOOMUX_RUNTIME_DIR"], str(DEV["runtime_directory"](ROOT)))
                for name in ["CONFIG_HOME", "STATE_HOME"]:
                    self.assertEqual(env[f"BOOMUX_{name}"], str(ROOT / "target/desktop-dev" / name.lower()))
                self.assertEqual(env["XDG_RUNTIME_DIR"], "/ordinary/runtime")
                for name in ["CONFIG", "STATE", "DATA", "CACHE"]:
                    self.assertEqual(env[f"XDG_{name}_HOME"], f"/ordinary/{name.lower()}-home")
                self.assertEqual(env.get("GH_CONFIG_DIR"), gh_config)
                self.assertEqual(env["WAYLAND_DISPLAY"], "wayland-1")
                self.assertEqual(env["HOME"], "/ordinary/home")
                self.assertEqual(env["KIRO_HOME"], "/explicit/kiro")
                self.assertNotIn("BOOMUX_CONFIG", env)
                self.assertEqual(env["PATH"], str(cli.parent) + os.pathsep + "/usr/bin")
                self.assertTrue(call.kwargs["check"])
                self.assertEqual(call.kwargs["timeout"], 30)
            return calls[-1].args[0]

    def test_long_worktrees_get_short_stable_distinct_runtime_paths(self):
        root = Path("/home/developer/Worktrees/boomux") / ("long-branch-" * 20)
        path = DEV["runtime_directory"](root)
        self.assertLess(len(os.fsencode(path / "boomux/daemon.sock")), 108)
        self.assertEqual(path, DEV["runtime_directory"](root))
        self.assertNotEqual(path, DEV["runtime_directory"](root / "other"))
        self.assertEqual(DEV["runtime_directory"](Path("/short")),
                         Path("/short/target/desktop-dev/runtime_dir"))

    def test_status_failure_exposes_the_underlying_error(self):
        failure = subprocess.CalledProcessError(1, "status", stderr="socket error details")
        with patch("sys.argv", ["run-dev.py"]), patch.object(Path, "mkdir"), \
                patch.object(Path, "lstat", return_value=Mock(st_mode=stat.S_IFDIR | 0o700, st_uid=os.getuid())), \
                patch("subprocess.run", side_effect=[Mock(), failure]), \
                patch("os.execve") as execute:
            with self.assertRaisesRegex(RuntimeError, "socket error details"):
                DEV["main"]()
            execute.assert_not_called()

    def test_runtime_rejects_symlinks_and_nonprivate_directories(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            private = root / "private"
            DEV["prepare_runtime_directory"](private)
            DEV["prepare_runtime_directory"](private)
            link = root / "link"
            link.symlink_to(private, target_is_directory=True)
            with self.assertRaises(RuntimeError):
                DEV["prepare_runtime_directory"](link)
            private.chmod(0o755)
            with self.assertRaises(RuntimeError):
                DEV["prepare_runtime_directory"](private)

    def test_running_daemon_hands_off_to_the_exact_rebuilt_executable(self):
        self.assertEqual(self.launch("running"), [CLI, "daemon", "restart", "--executable", str(CLI), "--refresh-environment"])

    def test_release_handoff_uses_release_binaries_and_the_same_runtime(self):
        cli = ROOT / "target/release/boomux"
        self.assertEqual(self.launch("running", release=True), [cli, "daemon", "restart", "--executable", str(cli), "--refresh-environment"])

    def test_explicit_github_config_is_preserved(self):
        self.launch("stopped", gh_config="/explicit/gh")

    def test_absent_daemon_starts_without_an_unnecessary_restart(self):
        self.assertEqual(self.launch("stopped"), [CLI, "daemon", "start"])

    def test_failed_handoff_aborts_without_stop_or_desktop_launch(self):
        self.launch("running", subprocess.CalledProcessError(1, "restart"))


if __name__ == "__main__":
    unittest.main()

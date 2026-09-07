"""Development launches use the rebuilt CLI without touching the ordinary daemon."""

import json
import os
from pathlib import Path
import runpy
import subprocess
import unittest
from unittest.mock import Mock, patch

DEV = runpy.run_path(str(Path(__file__).with_name("run-dev.py")))
ROOT = Path(DEV["__file__"]).resolve().parents[2]
CLI = ROOT / "target/debug/boomux"


class DevelopmentLaunchTests(unittest.TestCase):
    def launch(self, state, failure=None):
        status = Mock(stdout=json.dumps({"data": {"status": state}}))
        with patch.dict(os.environ, {
            "PATH": "/usr/bin", "XDG_RUNTIME_DIR": "/ordinary/runtime",
            "WAYLAND_DISPLAY": "wayland-1", "BOOMUX_CONFIG": "/ordinary/config",
        }, clear=True), patch.object(Path, "mkdir"), \
                patch("subprocess.run", side_effect=[Mock(), status, failure or Mock()]) as run, \
                patch("os.execve") as execute:
            if failure:
                with self.assertRaises(subprocess.CalledProcessError):
                    DEV["main"]()
                execute.assert_not_called()
            else:
                DEV["main"]()
                execute.assert_called_once()
                self.assertEqual(execute.call_args.args[0], str(ROOT / "target/debug/boomux-desktop"))
                self.assertEqual(execute.call_args.args[2], run.call_args.kwargs["env"])
            calls = run.call_args_list
            self.assertEqual(calls[1].args[0], [CLI, "daemon", "status", "--json"])
            for call in calls[1:]:
                env = call.kwargs["env"]
                for name in ["RUNTIME_DIR", "CONFIG_HOME", "STATE_HOME", "DATA_HOME", "CACHE_HOME"]:
                    self.assertEqual(env[f"XDG_{name}"], str(ROOT / "target/desktop-dev" / name.lower()))
                self.assertEqual(env["WAYLAND_DISPLAY"], "/ordinary/runtime/wayland-1")
                self.assertFalse(any(key.startswith("BOOMUX_") for key in env))
                self.assertEqual(env["PATH"], str(CLI.parent) + os.pathsep + "/usr/bin")
                self.assertTrue(call.kwargs["check"])
                self.assertEqual(call.kwargs["timeout"], 30)
            return calls[-1].args[0]

    def test_running_daemon_hands_off_to_the_exact_rebuilt_executable(self):
        self.assertEqual(self.launch("running"), [CLI, "daemon", "restart", "--executable", str(CLI)])

    def test_absent_daemon_starts_without_an_unnecessary_restart(self):
        self.assertEqual(self.launch("stopped"), [CLI, "daemon", "start"])

    def test_failed_handoff_aborts_without_stop_or_desktop_launch(self):
        self.launch("running", subprocess.CalledProcessError(1, "restart"))


if __name__ == "__main__":
    unittest.main()

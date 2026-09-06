"""Checks that smoke-test readiness cannot pass on an idle or failed client."""

from pathlib import Path
import runpy
import unittest
from unittest.mock import Mock

SMOKE = runpy.run_path(str(Path(__file__).with_name("smoke-desktop.py")))
FRAME = '''
 -> xdg_wm_base#8.get_xdg_surface(new id xdg_surface#12, wl_surface#11)
 -> xdg_toplevel#14.set_app_id("org.omarchy.boomux-desktop")
 -> wl_surface#11.attach(wl_buffer#22, 0, 0)
 -> wl_surface#11.commit()
wl_callback#23.done(12345)
'''


class SmokeEvidenceTests(unittest.TestCase):
    def test_accepts_committed_window_frame(self):
        self.assertTrue(SMOKE["wayland_frame_presented"](FRAME))
        self.assertTrue(SMOKE["wayland_frame_presented"](FRAME.replace("#", "@")))

    def test_rejects_missing_frame_or_wrong_surface(self):
        for log in [FRAME.replace("wl_buffer#22", "nil"),
                    FRAME.replace("wl_surface#11.attach", "wl_surface#99.attach"),
                    FRAME.replace("wl_surface#11.commit()", ""),
                    FRAME.replace("wl_callback#23.done(12345)", ""),
                    FRAME.replace("org.omarchy.boomux-desktop", "another-app")]:
            with self.subTest(log=log):
                self.assertFalse(SMOKE["wayland_frame_presented"](log))

    def test_registry_callback_before_commit_is_not_a_presented_frame(self):
        log = 'wl_callback#2.done(1)\n' + FRAME.replace("wl_callback#23.done(12345)", "")
        self.assertFalse(SMOKE["wayland_frame_presented"](log))

    def test_dead_process_fails_even_with_readiness_evidence(self):
        process = Mock(returncode=1)
        process.poll.return_value = 1
        with self.assertRaisesRegex(RuntimeError, "process exited"):
            SMOKE["wait_for"]("window", lambda: True, [process])

    def test_illegal_instruction_is_reported_by_signal_name(self):
        process = Mock(returncode=-4)
        process.poll.return_value = -4
        with self.assertRaisesRegex(RuntimeError, "SIGILL"):
            SMOKE["wait_for"]("window", lambda: True, [process])

    def test_wait_has_a_deadline(self):
        with self.assertRaisesRegex(RuntimeError, "timed out"):
            SMOKE["wait_for"]("window", lambda: False, [], seconds=0)

    def test_resource_identity_requires_one_explicit_uuid(self):
        identity = "11111111-2222-3333-4444-555555555555"
        self.assertEqual(SMOKE["created_id"](f"Created shell name ({identity})"), identity)
        for output in ["Created shell name", f"({identity}) ({identity})", "(11111111-" + "1" * 27 + ")"]:
            with self.subTest(output=output), self.assertRaises(RuntimeError):
                SMOKE["created_id"](output)


if __name__ == "__main__":
    unittest.main()

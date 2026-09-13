"""A missing or failed native check must never produce a green merge gate."""
import importlib.util
from pathlib import Path
import unittest

spec = importlib.util.spec_from_file_location("result", Path(__file__).with_name("ci-result.py"))
result = importlib.util.module_from_spec(spec)
spec.loader.exec_module(result)


class ResultTests(unittest.TestCase):
    def jobs(self, code="true", desktop="true", package="true"):
        return {
            "changes": {"result": "success", "outputs": {
                "run_code": code, "run_desktop": desktop, "run_package": package}},
            "rust": {"result": "success"},
            "desktop-bundle": {"result": "success" if package == "true" else "skipped"},
            "desktop-smoke": {"result": "success" if package == "true" else "skipped"},
            "macos": {"result": "success"},
        }

    def test_native_failure_cancellation_or_skip_blocks_selected_ci(self):
        for selection in [("true", "true", "true"), ("false", "true", "false"),
                          ("false", "false", "true")]:
            jobs = self.jobs(*selection)
            result.validate(jobs)
            for state in ["failure", "cancelled", "skipped"]:
                with self.subTest(selection=selection, state=state):
                    jobs["macos"]["result"] = state
                    with self.assertRaisesRegex(ValueError, "macos"):
                        result.validate(jobs)

    def test_guidance_and_metadata_only_pr_can_skip_native_ci(self):
        jobs = self.jobs("false", "false", "false")
        jobs["macos"]["result"] = "skipped"
        result.validate(jobs)

    def test_missing_native_dependency_fails_closed(self):
        jobs = self.jobs()
        del jobs["macos"]
        with self.assertRaisesRegex(ValueError, "Missing macos"):
            result.validate(jobs)

    def test_missing_selection_or_failed_classifier_cannot_skip_native_ci(self):
        jobs = self.jobs()
        jobs["changes"]["outputs"] = {}
        jobs["macos"]["result"] = "skipped"
        with self.assertRaisesRegex(ValueError, "macos"):
            result.validate(jobs)
        jobs["changes"]["result"] = "failure"
        with self.assertRaisesRegex(ValueError, "changes"):
            result.validate(jobs)

"""Require successful selected jobs, including native macOS validation."""
import json
import os


def validate(jobs):
    if "macos" not in jobs:
        raise ValueError("Missing macos validation dependency")
    outputs = jobs["changes"]["outputs"]
    package = outputs.get("run_package") != "false"
    macos = any(outputs.get(key) != "false" for key in ("run_code", "run_desktop", "run_package"))
    for name, job in jobs.items():
        expected = "success"
        if not package and name in {"desktop-bundle", "desktop-smoke"}:
            expected = "skipped"
        if not macos and name == "macos":
            expected = "skipped"
        if job["result"] != expected:
            raise ValueError(f"{name}: {job['result']}; expected {expected}")


if __name__ == "__main__":
    validate(json.loads(os.environ["RESULTS"]))

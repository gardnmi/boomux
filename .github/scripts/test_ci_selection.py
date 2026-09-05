"""Exercise classification with real Git diffs and controlled CI evidence."""

import importlib.util
import json
import os
from pathlib import Path
import subprocess
import tempfile
import unittest
from unittest.mock import patch

spec = importlib.util.spec_from_file_location("selection", Path(__file__).with_name("classify-ci.py"))
selection = importlib.util.module_from_spec(spec)
spec.loader.exec_module(selection)


class SelectionTests(unittest.TestCase):
    def setUp(self):
        directory = tempfile.TemporaryDirectory()
        self.addCleanup(directory.cleanup)
        self.previous = Path.cwd()
        self.root = Path(directory.name)
        os.chdir(self.root)
        self.addCleanup(os.chdir, self.previous)
        self.git("init", "-q")
        self.git("config", "user.name", "CI fixture")
        self.git("config", "user.email", "ci@example.invalid")
        self.write_version("1.2.3")
        for path in ["docs/architecture.md", "README.md", "THIRD_PARTY_NOTICES.md", ".agents/skills/boomux/SKILL.md", "src/lib.rs"]:
            self.write(path, "fixture\n")
        self.base = self.commit()

    def git(self, *args):
        return subprocess.check_output(["git", *args], stderr=subprocess.DEVNULL).decode().strip()

    def write(self, path, content):
        file = Path(path)
        file.parent.mkdir(parents=True, exist_ok=True)
        file.write_text(content)

    def write_version(self, version):
        self.write("Cargo.toml", f'[package]\nname = "boomux"\nversion = "{version}"\n')
        self.write("Cargo.lock", f'version = 4\n[[package]]\nname = "boomux"\nversion = "{version}"\n[[package]]\nname = "dependency"\nversion = "2.0.0"\nsource = "registry"\n')
        self.write(".release-please-manifest.json", json.dumps({".": version}))
        self.write("CHANGELOG.md", version)

    def commit(self):
        self.git("add", "-A")
        self.git("commit", "-qm", "fixture")
        return self.git("rev-parse", "HEAD")

    def classify(self, proof=lambda sha: True):
        return selection.classify(self.base, self.commit(), proof)[0]

    def test_documentation_skips_work(self):
        self.write("docs/architecture.md", "updated")
        self.assertEqual(self.classify(), dict.fromkeys(selection.FULL, False))

    def test_embedded_and_packaged_markdown_cannot_skip(self):
        for path in ["README.md", "THIRD_PARTY_NOTICES.md", ".agents/skills/boomux/SKILL.md"]:
            with self.subTest(path=path):
                self.write(path, "updated")
                result = self.classify()
                self.assertTrue(result["run_code"])
                self.assertTrue(result["run_package"])
                self.git("reset", "--hard", self.base)

    def test_deleted_embedded_markdown_cannot_skip(self):
        Path(".agents/skills/boomux/SKILL.md").unlink()
        self.assertTrue(self.classify()["run_code"])

    def test_failed_diff_requires_full_validation(self):
        result, _ = selection.classify("1" * 40, self.base)
        self.assertEqual(result, selection.FULL)

    def test_missing_and_zero_base_require_full_validation(self):
        for base in ["", "0" * 40]:
            self.assertEqual(selection.classify(base, self.base)[0], selection.FULL)

    def test_version_only_with_proof_keeps_packaging(self):
        self.write_version("1.2.4")
        seen = []
        result = self.classify(lambda sha: seen.append(sha) or True)
        self.assertEqual(seen, [self.base])
        self.assertEqual(result, {"run_code": False, "run_package": True, "run_benchmarks": False})

    def test_version_only_without_proof_runs_full_validation(self):
        self.write_version("1.2.4")
        self.assertEqual(self.classify(lambda sha: False), selection.FULL)

    def test_ci_lookup_failure_runs_full_validation(self):
        self.write_version("1.2.4")
        def unavailable(sha):
            raise subprocess.TimeoutExpired("gh", 30)
        self.assertEqual(self.classify(unavailable), selection.FULL)

    def test_release_with_source_change_runs_full_validation(self):
        self.write_version("1.2.4")
        self.write("src/lib.rs", "changed")
        self.assertEqual(self.classify(), selection.FULL)

    def test_release_with_dependency_change_runs_full_validation(self):
        self.write_version("1.2.4")
        lock = Path("Cargo.lock")
        lock.write_text(lock.read_text().replace('"2.0.0"', '"2.0.1"'))
        self.assertEqual(self.classify(), selection.FULL)

    def test_release_with_manifest_change_runs_full_validation(self):
        self.write_version("1.2.4")
        with Path("Cargo.toml").open("a") as manifest:
            manifest.write('edition = "2024"\n')
        self.assertEqual(self.classify(), selection.FULL)

    def test_wrong_release_manifest_runs_full_validation(self):
        self.write_version("1.2.4")
        self.write(".release-please-manifest.json", '{".": "1.2.3"}')
        self.assertEqual(self.classify(), selection.FULL)

    def test_packaging_and_js_changes_skip_benchmark_smoke(self):
        for path in ["packaging/test-installer.sh", "integrations/pi/boomux.js"]:
            self.write(path, "changed")
        self.assertEqual(self.classify(), {"run_code": True, "run_package": True, "run_benchmarks": False})

    def test_rust_and_workflow_changes_keep_benchmarks(self):
        self.write(".github/workflows/ci.yml", "changed")
        self.assertEqual(self.classify(), selection.FULL)

    def test_ci_proof_requires_exact_successful_main_push(self):
        valid = {"id": 123, "head_sha": self.base, "event": "push", "conclusion": "success", "head_branch": "main", "head_repository": {"full_name": "owner/repo"}}
        with patch.dict(os.environ, GITHUB_REPOSITORY="owner/repo", DEFAULT_BRANCH="main"):
            for field, invalid in [(None, None), ("head_sha", "a" * 40), ("event", "pull_request"), ("conclusion", "failure"), ("head_branch", "feature"), ("head_repository", {"full_name": "fork/repo"})]:
                run = dict(valid)
                if field:
                    run[field] = invalid
                jobs = {"jobs": [{"name": "Rust", "conclusion": "success", "steps": [
                    {"name": name, "conclusion": "success"} for name in
                    ["Run Clippy", "Run Rust unit tests", "Run configuration CLI tests", "Run native backend tests"]
                ]}]}
                responses = [json.dumps({"workflow_runs": [run]}).encode(), json.dumps(jobs).encode()]
                with patch.object(selection.subprocess, "check_output", side_effect=responses):
                    self.assertEqual(selection.validated_base(self.base), field is None)
            # A successful workflow with skipped tests cannot justify reuse.
            jobs["jobs"][0]["steps"][-1]["conclusion"] = "skipped"
            responses = [json.dumps({"workflow_runs": [valid]}).encode(), json.dumps(jobs).encode()]
            with patch.object(selection.subprocess, "check_output", side_effect=responses):
                self.assertFalse(selection.validated_base(self.base))


if __name__ == "__main__":
    unittest.main()

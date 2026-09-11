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
        self.write("desktop/Cargo.toml", f'[package]\nname = "boomux-desktop"\nversion = "{version}"\n')
        self.write("Cargo.lock", f'version = 4\n[[package]]\nname = "boomux"\nversion = "{version}"\n[[package]]\nname = "boomux-desktop"\nversion = "{version}"\n[[package]]\nname = "dependency"\nversion = "2.0.0"\nsource = "registry"\n')
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

    def test_website_has_separate_validation(self):
        self.write("website/src/pages/index.astro", "page")
        self.write("website/package-lock.json", "lock")
        self.write(".github/workflows/website.yml", "workflow")
        self.write("docs/ci.md", "guidance")
        self.assertEqual(self.classify(), dict.fromkeys(selection.FULL, False))

    def test_website_cannot_hide_core_changes(self):
        self.write("website/src/pages/index.astro", "page")
        self.write("src/lib.rs", "changed")
        self.assertEqual(self.classify(), selection.FULL)

    def test_website_with_desktop_source_still_checks_desktop(self):
        self.write("website/src/pages/index.astro", "page")
        self.write("desktop/src/main.rs", "changed")
        self.assertEqual(self.classify(), {
            "run_code": False, "run_desktop": True,
            "run_package": False, "run_benchmarks": False,
        })

    def test_moving_packaged_content_to_website_cannot_skip_checks(self):
        Path("website").mkdir()
        self.git("mv", "README.md", "website/README.md")
        self.assertTrue(self.classify()["run_code"])

    def test_desktop_source_and_guidance_select_only_desktop(self):
        self.write("desktop/src/terminal.rs", "changed")
        self.write("docs/desktop/architecture.md", "updated")
        self.write("desktop/AGENTS.md", "updated")
        self.assertEqual(self.classify(), {
            "run_code": False, "run_desktop": True,
            "run_package": False, "run_benchmarks": False,
        })

    def test_desktop_guidance_alone_skips_executable_checks(self):
        self.write("desktop/AGENTS.md", "updated")
        self.assertEqual(self.classify(), dict.fromkeys(selection.FULL, False))

    def test_desktop_source_with_shared_inputs_requires_full_validation(self):
        for path in ["src/client.rs", "Cargo.toml", "Cargo.lock", "desktop/Cargo.toml",
                     "vendor/libghostty-vt-sys/build.rs", ".github/workflows/ci.yml"]:
            with self.subTest(path=path):
                self.write("desktop/src/main.rs", "changed")
                self.write(path, "changed")
                self.assertEqual(self.classify(), selection.FULL)
                self.git("reset", "--hard", self.base)

    def test_renaming_core_source_into_desktop_cannot_skip_core(self):
        Path("desktop/src").mkdir(parents=True)
        self.git("mv", "src/lib.rs", "desktop/src/lib.rs")
        self.assertEqual(self.classify(), selection.FULL)

    def test_deleted_desktop_source_still_requires_desktop_checks(self):
        self.write("desktop/src/terminal.rs", "initial")
        self.base = self.commit()
        Path("desktop/src/terminal.rs").unlink()
        result = self.classify()
        self.assertTrue(result["run_desktop"])
        self.assertFalse(result["run_code"])

    def test_packaged_desktop_inputs_do_not_take_the_source_only_path(self):
        for path in ["desktop/install.sh", "desktop/README.md", "desktop/packaging/boomux-desktop",
                     "desktop/src/unknown.data", "desktop/scripts/package-release.py"]:
            with self.subTest(path=path):
                self.write(path, "changed")
                result = self.classify()
                self.assertTrue(result["run_package"])
                self.assertTrue(result["run_code"])
                self.assertTrue(result["run_desktop"])
                self.git("reset", "--hard", self.base)

    def test_embedded_and_packaged_markdown_cannot_skip(self):
        for path in ["README.md", "THIRD_PARTY_NOTICES.md", ".agents/skills/boomux/SKILL.md", "docs/platforms/macos-testing.md"]:
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
        self.assertEqual(result, {"run_code": False, "run_desktop": False, "run_package": True, "run_benchmarks": False})

    def test_version_only_without_proof_runs_full_validation(self):
        self.write_version("1.2.4")
        self.assertEqual(self.classify(lambda sha: False), selection.FULL)

    def test_ci_lookup_failure_runs_full_validation(self):
        self.write_version("1.2.4")
        def unavailable(sha):
            raise subprocess.TimeoutExpired("gh", 30)
        self.assertEqual(self.classify(unavailable), selection.FULL)

    def test_backend_only_ci_cannot_justify_workspace_reuse(self):
        valid = {"id": 123, "head_sha": self.base, "event": "push", "conclusion": "success", "head_branch": "main", "head_repository": {"full_name": "owner/repo"}}
        jobs = {"jobs": [{"name": "Rust", "conclusion": "success", "steps": [
            {"name": name, "conclusion": "success"} for name in
            ["Run Clippy", "Run Rust unit tests", "Run configuration CLI tests", "Run native backend tests"]]}]}
        with patch.dict(os.environ, GITHUB_REPOSITORY="owner/repo", DEFAULT_BRANCH="main"), patch.object(
            selection.subprocess, "check_output", side_effect=[json.dumps({"workflow_runs": [valid]}).encode(), json.dumps(jobs).encode()]):
            self.assertFalse(selection.validated_base(self.base))

    def test_desktop_only_ci_cannot_justify_workspace_reuse(self):
        valid = {"id": 123, "head_sha": self.base, "event": "push", "conclusion": "success",
                 "head_branch": "main", "head_repository": {"full_name": "owner/repo"}}
        jobs = {"jobs": [{"name": "Rust", "conclusion": "success", "steps": [
            {"name": "Skip code checks when covered or not required", "conclusion": "success"}]},
            {"name": "Desktop Rust", "conclusion": "success", "steps": [
                {"name": name, "conclusion": "success"} for name in ["Run Desktop Clippy", "Run Desktop tests"]]}]}
        with patch.dict(os.environ, GITHUB_REPOSITORY="owner/repo", DEFAULT_BRANCH="main"), patch.object(
            selection.subprocess, "check_output", side_effect=[json.dumps({"workflow_runs": [valid]}).encode(), json.dumps(jobs).encode()]):
            self.assertFalse(selection.validated_base(self.base))

    def test_partial_workspace_version_change_runs_everything(self):
        self.write_version("1.2.4")
        self.write("desktop/Cargo.toml", '[package]\nname = "boomux-desktop"\nversion = "1.2.3"\n')
        self.assertEqual(self.classify(), selection.FULL)

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
        self.assertEqual(self.classify(), {"run_code": True, "run_desktop": True, "run_package": True, "run_benchmarks": False})

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
                for name, steps in {
                    "macos / Native macOS": ["Run native Clippy", "Test descriptor transfer", "Test native process identity", "Test native lifecycle and recovery", "Test native Desktop"],
                    "Desktop Rust": ["Run Desktop Clippy", "Run Desktop tests"],
                    "Integrations": ["Verify embedded web terminal assets", "Run integration tests"],
                    "Dependency policy": ["Audit advisories, licenses, and dependency sources"],
                }.items():
                    jobs["jobs"].append({"name": name, "conclusion": "success", "steps": [
                        {"name": step, "conclusion": "success"} for step in steps]})
                responses = [json.dumps({"workflow_runs": [run]}).encode(), json.dumps(jobs).encode()]
                with patch.object(selection.subprocess, "check_output", side_effect=responses):
                    self.assertEqual(selection.validated_base(self.base), field is None)
            # A successful workflow with skipped tests cannot justify reuse.
            jobs["jobs"][0]["steps"][-1]["conclusion"] = "skipped"
            responses = [json.dumps({"workflow_runs": [valid]}).encode(), json.dumps(jobs).encode()]
            with patch.object(selection.subprocess, "check_output", side_effect=responses):
                self.assertFalse(selection.validated_base(self.base))

    def test_release_pr_defers_artifacts_until_merge(self):
        self.write_version("1.2.4")
        head = self.commit()
        for event in ["pull_request", "merge_group"]:
            with self.subTest(event=event):
                result, _ = selection.classify(self.base, head, lambda sha: True, event=event)
                self.assertEqual(result, dict.fromkeys(selection.FULL, False))
                result, _ = selection.classify(self.base, head, lambda sha: False, event=event)
                self.assertEqual(result, selection.FULL)
        for event in ["push", "workflow_dispatch", ""]:
            self.assertTrue(selection.classify(self.base, head, lambda sha: True, event=event)[0]["run_package"])

    def jobs(self, components):
        return {"jobs": [{"name": selection.COMPONENT_STEPS[c][0], "conclusion": "success", "steps": [
            {"name": step, "conclusion": "success"} for step in selection.COMPONENT_STEPS[c][1]]}
            for c in components]}

    def evidence(self, target, candidates):
        runs = [{"id": i + 1, "head_sha": sha, "event": "push", "conclusion": "success",
                 "head_branch": "main", "head_repository": {"full_name": "owner/repo"}}
                for i, (sha, _) in enumerate(candidates)]
        original = subprocess.check_output
        calls = []
        def request(command, **kwargs):
            if command[0] != "gh":
                return original(command, **kwargs)
            calls.append(command[-1])
            if "/workflows/" in command[-1]:
                return json.dumps({"workflow_runs": runs}).encode()
            run_id = int(command[-1].split("/runs/")[1].split("/")[0])
            return json.dumps(self.jobs(candidates[run_id - 1][1])).encode()
        with patch.dict(os.environ, GITHUB_REPOSITORY="owner/repo", DEFAULT_BRANCH="main"), patch.object(
                selection.subprocess, "check_output", side_effect=request):
            result = selection.validated_base(target)
        return result, calls

    def test_desktop_run_inherits_unchanged_backend_components(self):
        self.write("desktop/src/main.rs", "new Desktop")
        desktop = self.commit()
        result, _ = self.evidence(desktop, [(desktop, {"desktop", "macos"}), (self.base, set(selection.COMPONENT_STEPS))])
        self.assertTrue(result)

    def test_desktop_changes_cannot_reuse_old_macos_validation(self):
        self.write("desktop/src/main.rs", "new Desktop")
        desktop = self.commit()
        self.assertFalse(self.evidence(desktop, [(desktop, {"desktop"}),
                                                (self.base, set(selection.COMPONENT_STEPS))])[0])

    def test_pre_port_ci_cannot_justify_release_reuse(self):
        self.assertFalse(self.evidence(self.base, [(self.base, set(selection.COMPONENT_STEPS) - {"macos"})])[0])

    def test_docs_runs_can_inherit_actual_ancestor_checks(self):
        self.write("docs/ci.md", "new guidance")
        docs = self.commit()
        self.assertTrue(self.evidence(docs, [(docs, set()), (self.base, set(selection.COMPONENT_STEPS))])[0])

    def test_combined_version_and_desktop_changes_preserve_backend_evidence(self):
        self.write_version("1.2.4")
        self.write("desktop/src/main.rs", "new Desktop")
        self.write("docs/ci.md", "guidance")
        head = self.commit()
        self.assertTrue(self.evidence(head, [(head, {"desktop", "macos"}), (self.base, set(selection.COMPONENT_STEPS))])[0])

    def test_shared_changes_and_missing_components_cannot_inherit(self):
        self.write("src/lib.rs", "backend change")
        head = self.commit()
        self.assertFalse(self.evidence(head, [(head, {"desktop"}), (self.base, set(selection.COMPONENT_STEPS))])[0])
        self.assertFalse(self.evidence(self.base, [(self.base, {"desktop", "backend"})])[0])

    def test_dependency_change_cannot_hide_behind_version_bump(self):
        self.write_version("1.2.4")
        lock = Path("Cargo.lock")
        lock.write_text(lock.read_text().replace('"2.0.0"', '"2.0.1"'))
        head = self.commit()
        self.assertEqual(selection.reusable_components(self.base, head), set())

    def test_renamed_core_file_invalidates_inheritance(self):
        Path("desktop/src").mkdir(parents=True)
        self.git("mv", "src/lib.rs", "desktop/src/lib.rs")
        head = self.commit()
        self.assertEqual(selection.reusable_components(self.base, head), set())

    def test_unrelated_successful_commit_is_not_evidence(self):
        self.git("checkout", "--orphan", "other")
        self.write("unrelated", "other root")
        other = self.commit()
        self.assertEqual(selection.reusable_components(other, self.base), set())

    def test_evidence_queries_are_bounded(self):
        result, calls = self.evidence(self.base, [(self.base, set())] * 20)
        self.assertFalse(result)
        self.assertEqual(len(calls), 7)  # one run list, at most six job lists


if __name__ == "__main__":
    unittest.main()

"""Development promotion verifies CI and bytes; retries never replace assets."""
import copy
import hashlib
import importlib.util
import json
import re
from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest
from unittest.mock import patch
import zipfile

spec = importlib.util.spec_from_file_location("development", Path(__file__).with_name("development-release.py"))
development = importlib.util.module_from_spec(spec)
spec.loader.exec_module(development)
REPO = "owner/boomux"
SHA = "a" * 40


class PreviewTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.directory = Path(self.temporary.name)
        self.run = {"id": 123, "run_attempt": 1, "general_ci_run": 789, "head_sha": SHA, "status": "completed",
                    "conclusion": "success", "event": "push", "head_repository": {"full_name": REPO},
                    "repository": {"full_name": REPO}, "path": ".github/workflows/macos-preview.yml",
                    "created_at": "2026-09-10T00:00:00Z"}
        self.jobs = [{"name": name, "conclusion": "success", "steps": [
            {"name": step, "conclusion": "success"} for step in steps]}
            for name, steps in development.REQUIRED_JOBS.items()]
        self.archive = self.directory / f"boomux-macos-preview-aarch64-{SHA[:8]}.zip"
        self.metadata = {"source": SHA, "target": "aarch64-apple-darwin", "version": "1.14.1",
                         "distribution": "testing-preview", "notarized": False}
        self.bundle()
        self.tag = None
        self.release = None
        self.assets = []
        self.mutations = []
        self.uploads = []
        self.fail_upload = False

    def bundle(self, header=bytes.fromhex("cffaedfe0c000001")):
        prefix = "Boomux macOS Preview/"
        with zipfile.ZipFile(self.archive, "w") as bundle:
            bundle.writestr(prefix + "build.json", json.dumps(self.metadata))
            bundle.writestr(prefix + "READ ME FIRST.md", "Testing instructions\n")
            for name in ["boomux", "boomux-desktop"]:
                info = zipfile.ZipInfo(prefix + "Boomux.app/Contents/MacOS/" + name)
                info.external_attr = 0o100755 << 16
                bundle.writestr(info, header)
        self.checksum()

    def checksum(self):
        digest = hashlib.sha256(self.archive.read_bytes()).hexdigest()
        Path(str(self.archive) + ".sha256").write_text(f"{digest}  {self.archive.name}\n")

    def prepared(self):
        package = development.validate_package(self.directory, SHA)
        request = development.publication(self.run, package, REPO)
        (self.directory / "publication.json").write_text(json.dumps(request))
        return request

    def api(self, repo, path, *, method=None, data=None, missing=False):
        self.assertEqual(repo, REPO)
        if method:
            self.mutations.append((path, method, data))
        if path.startswith("git/ref/tags/"):
            return {"object": {"type": "commit", "sha": self.tag}} if self.tag else None
        if path == "git/refs":
            self.tag = data["sha"]
            return {}
        if path.startswith("releases/tags/"):
            # Drafts deliberately only appear in the list endpoint.
            return copy.deepcopy(self.release) if self.release and not self.release["draft"] else None
        if path == "releases?per_page=100":
            return [copy.deepcopy(self.release)] if self.release else []
        if path == "releases" and method == "POST":
            self.release = {"id": 456, **data}
            return copy.deepcopy(self.release)
        if path == "releases/456/assets?per_page=100":
            return copy.deepcopy(self.assets)
        if path == "releases/456":
            if method:
                self.release.update(data)
            return copy.deepcopy(self.release)
        raise AssertionError(path)

    def upload(self, command, **kwargs):
        self.assertEqual(command[:3], ["gh", "release", "upload"])
        self.assertNotIn("--clobber", command)
        if self.fail_upload:
            raise subprocess.CalledProcessError(1, command)
        path = Path(command[4])
        self.uploads.append(path.name)
        self.assets.append({"name": path.name, "digest": "sha256:" + hashlib.sha256(path.read_bytes()).hexdigest()})

    def publish(self):
        with patch.object(development, "source_run", return_value=self.run), patch.object(
                development, "api", side_effect=self.api), patch.object(
                development.subprocess, "run", side_effect=self.upload):
            return development.publish(REPO, "123", self.directory)

    def test_run_requires_successful_same_repository_native_validation(self):
        self.assertEqual(development.validate_run(self.run, self.jobs, REPO, "123"), SHA)
        for field, value in [("status", "in_progress"), ("conclusion", "failure"), ("event", "pull_request"),
                             ("head_sha", "bad"), ("head_repository", {"full_name": "fork/boomux"}),
                             ("path", ".github/workflows/ci.yml"), ("id", 124)]:
            with self.subTest(field=field):
                run = {**self.run, field: value}
                with self.assertRaises(ValueError):
                    development.validate_run(run, self.jobs, REPO, "123")
        self.jobs[0]["steps"][0]["conclusion"] = "skipped"
        with self.assertRaises(ValueError):
            development.validate_run(self.run, self.jobs, REPO, "123")

    def test_general_ci_requires_matching_success_and_aggregate_gate(self):
        run = {**self.run, "id": 789, "event": "pull_request"}
        jobs = {"jobs": [{"name": "CI result", "conclusion": "success"}]}
        with patch.object(development, "api", side_effect=[{"workflow_runs": [run]}, jobs]):
            self.assertEqual(development.validate_general_ci(REPO, SHA), 789)
        for change in [{"head_sha": "b" * 40}, {"conclusion": "failure"}, {"status": "in_progress"},
                       {"head_repository": {"full_name": "fork/boomux"}}]:
            with self.subTest(change=change), patch.object(development, "api", return_value={"workflow_runs": [{**run, **change}]}):
                with self.assertRaises(ValueError):
                    development.validate_general_ci(REPO, SHA)
        with patch.object(development, "api", side_effect=[{"workflow_runs": [run]}, {"jobs": []}]):
            with self.assertRaisesRegex(ValueError, "aggregate"):
                development.validate_general_ci(REPO, SHA)

    def test_checksum_and_source_must_match(self):
        self.archive.write_bytes(self.archive.read_bytes() + b"changed")
        with self.assertRaisesRegex(ValueError, "checksum"):
            development.validate_package(self.directory, SHA)
        self.metadata["source"] = "b" * 40
        self.bundle()
        with self.assertRaisesRegex(ValueError, "metadata"):
            development.validate_package(self.directory, SHA)

    def test_wrong_architecture_is_rejected(self):
        self.bundle(header=b"not MachO")
        with self.assertRaisesRegex(ValueError, "Mach-O"):
            development.validate_package(self.directory, SHA)

    def test_duplicate_metadata_is_rejected(self):
        with self.assertWarns(UserWarning), zipfile.ZipFile(self.archive, "a") as bundle:
            bundle.writestr("Boomux macOS Preview/build.json", json.dumps(self.metadata))
        self.checksum()
        with self.assertRaisesRegex(ValueError, "contents"):
            development.validate_package(self.directory, SHA)

    def test_publishes_prerelease_once_without_rebuild_or_latest(self):
        request = self.prepared()
        url = self.publish()
        self.assertEqual(url, f"https://github.com/{REPO}/releases/tag/preview-macos-20260910.123")
        self.assertEqual(self.tag, SHA)
        self.assertFalse(self.release["draft"])
        self.assertTrue(self.release["prerelease"])
        self.assertEqual(self.release["make_latest"], "false")
        self.assertEqual(self.release["target_commitish"], SHA)
        self.assertEqual(len(self.uploads), 2)
        self.assertEqual(self.publish(), url)
        self.assertEqual(len(self.uploads), 2)
        self.assertEqual(sum(path == "releases" for path, _, _ in self.mutations), 1)
        self.assertIn(request["sha256"], self.release["body"])

    def test_interrupted_draft_upload_resumes(self):
        self.prepared()
        self.fail_upload = True
        with self.assertRaises(subprocess.CalledProcessError):
            self.publish()
        self.assertTrue(self.release["draft"])
        self.fail_upload = False
        self.publish()
        self.assertFalse(self.release["draft"])
        self.assertEqual(sum(path == "releases" for path, _, _ in self.mutations), 1)

    def test_conflicting_tag_is_never_moved(self):
        self.prepared()
        self.tag = "b" * 40
        with self.assertRaisesRegex(ValueError, "different source"):
            self.publish()
        self.assertEqual(self.mutations, [])

    def test_conflicting_asset_is_never_replaced(self):
        request = self.prepared()
        self.tag = SHA
        self.release = {"id": 456, "tag_name": request["tag"], "draft": True, "prerelease": True}
        self.assets = [{"name": request["archive"], "digest": "sha256:" + "0" * 64}]
        with self.assertRaisesRegex(ValueError, "conflicts"):
            self.publish()
        self.assertEqual(self.uploads, [])
        self.assertTrue(self.release["draft"])

    def test_published_incomplete_preview_is_not_modified(self):
        request = self.prepared()
        self.tag = SHA
        self.release = {"id": 456, "tag_name": request["tag"], "draft": False, "prerelease": True}
        with self.assertRaisesRegex(ValueError, "incomplete"):
            self.publish()
        self.assertEqual(self.uploads, [])

    def test_preparation_is_read_only_and_rejects_expired_artifact(self):
        output = self.directory / "prepared"
        artifact = {"name": "boomux-macos-preview", "size_in_bytes": 4096, "expired": True}
        with patch.object(development, "source_run", return_value=self.run), patch.object(
                development, "api", return_value={"artifacts": [artifact]}), patch.object(
                development.subprocess, "run") as download:
            with self.assertRaisesRegex(ValueError, "expired"):
                development.prepare(REPO, "123", output)
            download.assert_not_called()
            artifact["expired"] = False
            def copy(command, **kwargs):
                self.assertEqual(command[:3], ["gh", "run", "download"])
                for name in [self.archive.name, self.archive.name + ".sha256"]:
                    shutil.copy2(self.directory / name, output / name)
            download.side_effect = copy
            request = development.prepare(REPO, "123", output)
            self.assertEqual(request["source"], SHA)
            self.assertTrue((output / "publication.json").exists())

    def test_only_namespaced_preview_drafts_are_excluded_from_stable_gate(self):
        workflow = Path(__file__).parents[1] / "workflows/release-please.yml"
        query = re.search(r"--jq '(\.\[\] \| select\(\.draft == true\).*?)'", workflow.read_text()).group(1)
        releases = [
            {"id": 1, "tag_name": "v1.2.3", "draft": True, "prerelease": False},
            {"id": 2, "tag_name": "preview-macos-20260910.123", "draft": True, "prerelease": True},
            {"id": 3, "tag_name": "preview-macos-20260910.124", "draft": True, "prerelease": False},
            {"id": 4, "tag_name": "v1.3.0-rc.1", "draft": True, "prerelease": True},
            {"id": 5, "tag_name": "v1.2.2", "draft": False, "prerelease": False},
        ]
        result = subprocess.check_output(["jq", "-r", query], input=json.dumps(releases), text=True, timeout=5)
        self.assertEqual(result.splitlines(), ["1", "3", "4"])

    def test_changed_prepared_request_cannot_publish(self):
        request = self.prepared()
        request["source"] = "b" * 40
        (self.directory / "publication.json").write_text(json.dumps(request))
        with self.assertRaisesRegex(ValueError, "Prepared publication"):
            self.publish()
        self.assertEqual(self.mutations, [])


if __name__ == "__main__":
    unittest.main()

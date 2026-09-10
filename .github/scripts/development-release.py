"""Promote a validated macOS preview artifact; never execute or rebuild its code."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import subprocess
import zipfile

MAX_ARCHIVE = 128 * 1024 * 1024
MAX_CONTENT = 512 * 1024 * 1024
REQUIRED_JOBS = {
    "macos": {"Check backend", "Test descriptor transfer", "Test native process identity",
              "Test native lifecycle and recovery"},
    "package": {"Build preview executables", "Package preview", "Exercise packaged application"},
}


def api(repo, path, *, method=None, data=None, missing=False):
    command = ["gh", "api", f"repos/{repo}/{path}"]
    if method:
        command += ["--method", method]
    if data is not None:
        command += ["--input", "-"]
    result = subprocess.run(command, input=json.dumps(data) if data is not None else None,
                            text=True, capture_output=True, timeout=60)
    if result.returncode:
        if missing and "HTTP 404" in result.stderr:
            return None
        raise RuntimeError(f"GitHub request failed: {result.stderr.strip()}")
    return json.loads(result.stdout)


def digest(path):
    with path.open("rb") as stream:
        return hashlib.file_digest(stream, "sha256").hexdigest()


def validate_general_ci(repo, sha):
    runs = api(repo, f"actions/workflows/ci.yml/runs?head_sha={sha}&per_page=20")["workflow_runs"]
    matching = [run for run in runs if run["head_sha"] == sha
                and run["event"] in {"push", "pull_request", "workflow_dispatch"}
                and run["head_repository"]["full_name"] == repo
                and run["repository"]["full_name"] == repo]
    if not matching or matching[0]["status"] != "completed" or matching[0]["conclusion"] != "success":
        raise ValueError("The source revision also needs successful general CI (open a PR or run CI manually)")
    run = matching[0]
    jobs = api(repo, f"actions/runs/{int(run['id'])}/attempts/{int(run['run_attempt'])}/jobs?per_page=100")["jobs"]
    if not any(job["name"] == "CI result" and job["conclusion"] == "success" for job in jobs):
        raise ValueError("General CI did not pass its aggregate required-check gate")
    return run["id"]


def validate_run(run, jobs, repo, run_id):
    if not (run["id"] == int(run_id) and run["status"] == "completed"
            and run["conclusion"] == "success" and run["event"] in {"push", "workflow_dispatch"}
            and run["head_repository"]["full_name"] == repo
            and run["repository"]["full_name"] == repo
            and run["path"] == ".github/workflows/macos-preview.yml"
            and re.fullmatch(r"[0-9a-f]{40}", run["head_sha"])):
        raise ValueError("Expected a successful macOS preview run from this repository")
    for name, steps in REQUIRED_JOBS.items():
        if not any(job["name"] == name and job["conclusion"] == "success"
                   and steps <= {step["name"] for step in job["steps"] if step["conclusion"] == "success"}
                   for job in jobs):
            raise ValueError(f"Missing successful native validation: {name}")
    if not re.match(r"\d{4}-\d{2}-\d{2}T", run["created_at"]):
        raise ValueError("Invalid build date")
    return run["head_sha"]


def source_run(repo, run_id):
    if not re.fullmatch(r"[1-9][0-9]*", str(run_id)):
        raise ValueError("Run ID must be a positive integer")
    run = api(repo, f"actions/runs/{run_id}")
    # Pin job evidence to this completed attempt, including when a run was rerun.
    attempt = int(run["run_attempt"])
    jobs = api(repo, f"actions/runs/{run_id}/attempts/{attempt}/jobs?per_page=100")["jobs"]
    validate_run(run, jobs, repo, run_id)
    run["general_ci_run"] = validate_general_ci(repo, run["head_sha"])
    return run


def validate_package(directory, sha):
    name = f"boomux-macos-preview-aarch64-{sha[:8]}.zip"
    archive = directory / name
    checksum = directory / (name + ".sha256")
    if archive.is_symlink() or checksum.is_symlink() or not 0 < archive.stat().st_size <= MAX_ARCHIVE:
        raise ValueError("Invalid preview archive")
    archive_digest = digest(archive)
    if checksum.stat().st_size > 256 or checksum.read_text().strip() != f"{archive_digest}  {name}":
        raise ValueError("Preview checksum mismatch")
    with zipfile.ZipFile(archive) as bundle:
        members = bundle.infolist()
        names = [item.filename for item in members]
        if len(names) > 1024 or len(set(names)) != len(names) or sum(item.file_size for item in members) > MAX_CONTENT:
            raise ValueError("Invalid preview archive contents")
        prefix = "Boomux macOS Preview/"
        metadata_name = prefix + "build.json"
        if bundle.getinfo(metadata_name).file_size > 16384:
            raise ValueError("Oversized build metadata")
        metadata = json.loads(bundle.read(metadata_name))
        if not (metadata["source"] == sha and metadata["target"] == "aarch64-apple-darwin"
                and metadata["distribution"] == "testing-preview" and metadata["notarized"] is False
                and re.fullmatch(r"\d+\.\d+\.\d+", metadata["version"])):
            raise ValueError("Preview metadata does not match the successful build")
        for executable in ["boomux", "boomux-desktop"]:
            member = bundle.getinfo(prefix + "Boomux.app/Contents/MacOS/" + executable)
            with bundle.open(member) as binary:
                header = binary.read(8)
            if header != bytes.fromhex("cffaedfe0c000001") or not (member.external_attr >> 16) & 0o111:
                raise ValueError("Expected executable ARM64 Mach-O binaries")
        guide = prefix + "READ ME FIRST.md"
        if bundle.getinfo(guide).file_size > 65536:
            raise ValueError("Oversized testing guide")
        testing = bundle.read(guide).decode()
    return {"archive": name, "sha256": archive_digest, "version": metadata["version"], "guide": testing}


def publication(run, package, repo):
    sha = run["head_sha"]
    date = run["created_at"][:10]
    # A separate namespace avoids suggesting these tags are stable SemVer releases.
    tag = f"preview-macos-{date.replace('-', '')}.{run['id']}"
    title = f"macOS Development Preview — {date} ({sha[:8]})"
    notes = f"""Development preview for testing. Not a stable release.

- Apple Silicon (M1 or newer), macOS 15+.
- Matching Desktop and CLI, version {package['version']}.
- Source: `{sha}`.
- Validated Mac build: https://github.com/{repo}/actions/runs/{run['id']}
- General CI: https://github.com/{repo}/actions/runs/{run['general_ci_run']}
- SHA-256: `{package['sha256']}`.
- Ad-hoc signed, not notarized; see the installation steps below.
- Known preview issue: the initial window can extend off-screen on small displays.

Please report reproducible problems at https://github.com/{repo}/issues, including
this preview tag, macOS version, chip, steps, and error text. Do not include secrets.

{package['guide']}
"""
    return {"repo": repo, "run_id": run["id"], "source": sha, "tag": tag,
            "title": title, "archive": package["archive"], "sha256": package["sha256"], "notes": notes}


def prepare(repo, run_id, directory):
    run = source_run(repo, run_id)
    artifacts = api(repo, f"actions/runs/{run_id}/artifacts?per_page=100")["artifacts"]
    matches = [a for a in artifacts if a["name"] == "boomux-macos-preview" and not a["expired"]]
    if len(matches) != 1 or not 0 < matches[0]["size_in_bytes"] <= MAX_ARCHIVE:
        raise ValueError("Missing, expired, ambiguous, or oversized preview artifact; rebuild explicitly")
    directory.mkdir(parents=True, exist_ok=True)
    if any(directory.iterdir()):
        raise ValueError("Preparation directory must be empty")
    subprocess.run(["gh", "run", "download", str(run_id), "--repo", repo,
                    "--name", "boomux-macos-preview", "--dir", str(directory)], check=True, timeout=180)
    package = validate_package(directory, run["head_sha"])
    if {p.name for p in directory.iterdir()} != {package["archive"], package["archive"] + ".sha256"}:
        raise ValueError("Unexpected files in preview artifact")
    request = publication(run, package, repo)
    (directory / "publication.json").write_text(json.dumps(request, indent=2) + "\n")
    (directory / "release-notes.md").write_text(request["notes"])
    return request


def ensure_tag(repo, tag, sha):
    reference = api(repo, f"git/ref/tags/{tag}", missing=True)
    if reference is None:
        api(repo, "git/refs", method="POST", data={"ref": f"refs/tags/{tag}", "sha": sha})
    elif reference["object"]["type"] != "commit" or reference["object"]["sha"] != sha:
        raise ValueError("Existing preview tag has a different source; tags are never moved")


def verify_assets(assets, expected):
    if len({a["name"] for a in assets}) != len(assets):
        raise ValueError("Duplicate release assets")
    for asset in assets:
        if asset["name"] not in expected or asset.get("digest") != "sha256:" + expected[asset["name"]]:
            raise ValueError("Existing release asset conflicts with validated bytes; refusing replacement")


def publish(repo, run_id, directory):
    run = source_run(repo, run_id)
    package = validate_package(directory, run["head_sha"])
    request = publication(run, package, repo)
    if json.loads((directory / "publication.json").read_text()) != request:
        raise ValueError("Prepared publication no longer matches its validated source")
    tag, sha = request["tag"], request["source"]
    ensure_tag(repo, tag, sha)
    release = api(repo, f"releases/tags/{tag}", missing=True)
    if release is None:
        # GitHub can omit drafts from the tag endpoint. Resume an interrupted
        # upload without creating another draft or overwriting existing bytes.
        drafts = [r for r in api(repo, "releases?per_page=100") if r["tag_name"] == tag]
        if len(drafts) > 1:
            raise ValueError("Ambiguous preview drafts")
        release = drafts[0] if drafts else None
    if release is None:
        release = api(repo, "releases", method="POST", data={
            "tag_name": tag, "target_commitish": sha, "name": request["title"], "body": request["notes"],
            "draft": True, "prerelease": True, "make_latest": "false"})
    if release["tag_name"] != tag or release["prerelease"] is not True:
        raise ValueError("Existing release is not this development prerelease")
    release_id = int(release["id"])
    names = [package["archive"], package["archive"] + ".sha256"]
    expected = {name: digest(directory / name) for name in names}
    assets = api(repo, f"releases/{release_id}/assets?per_page=100")
    verify_assets(assets, expected)
    missing = set(names) - {asset["name"] for asset in assets}
    if missing and not release["draft"]:
        raise ValueError("Published preview is incomplete; publish a new preview instead of changing it")
    for name in sorted(missing):
        subprocess.run(["gh", "release", "upload", tag, str(directory / name), "--repo", repo],
                       check=True, timeout=180)
    assets = api(repo, f"releases/{release_id}/assets?per_page=100")
    verify_assets(assets, expected)
    if {asset["name"] for asset in assets} != set(names):
        raise ValueError("Upload verification failed")
    ensure_tag(repo, tag, sha)
    if release["draft"]:
        api(repo, f"releases/{release_id}", method="PATCH",
            data={"draft": False, "prerelease": True, "make_latest": "false"})
    published = api(repo, f"releases/{release_id}")
    if published["draft"] or not published["prerelease"] or published["tag_name"] != tag:
        raise ValueError("Prerelease publication verification failed")
    return f"https://github.com/{repo}/releases/tag/{tag}"


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("mode", choices=["prepare", "publish"])
    parser.add_argument("run_id")
    parser.add_argument("directory", type=Path)
    args = parser.parse_args()
    repo = os.environ["GITHUB_REPOSITORY"]
    if not re.fullmatch(r"[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+", repo):
        raise SystemExit("Invalid repository")
    if args.mode == "prepare":
        result = prepare(repo, args.run_id, args.directory)
        print(json.dumps({key: value for key, value in result.items() if key != "notes"}, indent=2))
    else:
        print(publish(repo, args.run_id, args.directory))

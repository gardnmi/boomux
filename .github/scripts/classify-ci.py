"""Conservative CI selection; unknown inputs always require full validation."""

import json
import os
import re
import subprocess
import tomllib

DOCS = {"AGENTS.md", "CONTEXT.md", "DEVELOPMENT.md", "BENCHMARKING.md",
        "CHANGELOG.md", "SECURITY.md"}
RELEASE_FILES = {"Cargo.toml", "Cargo.lock", ".release-please-manifest.json", "CHANGELOG.md"}
FULL = {"run_code": True, "run_package": True, "run_benchmarks": True}


def git(*args):
    return subprocess.check_output(["git", *args], timeout=30)


def read(ref, path):
    return git("show", f"{ref}:{path}").decode()


def release_only(base, head, paths):
    if not paths <= RELEASE_FILES or not {"Cargo.toml", "Cargo.lock"} <= paths:
        return False
    before = tomllib.loads(read(base, "Cargo.toml"))
    after = tomllib.loads(read(head, "Cargo.toml"))
    old, new = before["package"]["version"], after["package"]["version"]
    if old == new or not re.fullmatch(r"(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)", new):
        return False
    after["package"]["version"] = old
    if before != after:
        return False
    locks = [tomllib.loads(read(ref, "Cargo.lock")) for ref in (base, head)]
    for lock, version in zip(locks, (old, new)):
        roots = [p for p in lock["package"] if p["name"] == "boomux" and "source" not in p]
        if len(roots) != 1 or roots[0]["version"] != version:
            return False
        roots[0]["version"] = old
    if locks[0] != locks[1]:
        return False
    manifests = [json.loads(read(ref, ".release-please-manifest.json")) for ref in (base, head)]
    if manifests[1].get(".") != new:
        return False
    manifests[1]["."] = manifests[0]["."]
    return manifests[0] == manifests[1]


def validated_base(sha):
    repo = os.environ["GITHUB_REPOSITORY"]
    branch = os.environ["DEFAULT_BRANCH"]
    # Bounded run/job queries. A green docs-only run is not test evidence.
    response = subprocess.check_output([
        "gh", "api", f"repos/{repo}/actions/workflows/ci.yml/runs?event=push&head_sha={sha}&status=success&per_page=100"
    ], timeout=30)
    runs = [run for run in json.loads(response)["workflow_runs"]
            if run["head_sha"] == sha and run["event"] == "push"
            and run["conclusion"] == "success" and run["head_branch"] == branch
            and run["head_repository"]["full_name"] == repo]
    if not runs:
        return False
    run_id = int(runs[0]["id"])
    jobs = json.loads(subprocess.check_output([
        "gh", "api", f"repos/{repo}/actions/runs/{run_id}/jobs?per_page=100"
    ], timeout=30))["jobs"]
    required = {"Run Clippy", "Run Rust unit tests", "Run configuration CLI tests", "Run native backend tests"}
    return any(job["name"] == "Rust" and job["conclusion"] == "success"
               and required <= {step["name"] for step in job["steps"] if step["conclusion"] == "success"}
               for job in jobs)



def classify(base, head, proof=validated_base):
    try:
        if not re.fullmatch(r"[0-9a-f]{40}", base) or base == "0" * 40:
            return FULL.copy(), "No usable comparison base; full validation."
        paths = set(git("diff", "--name-only", "--no-renames", "-z", base, head).decode().rstrip("\0").split("\0")) - {""}
        if all(path in DOCS or (path.startswith("docs/") and path.endswith(".md")) for path in paths):
            return dict.fromkeys(FULL, False), "Documentation-only change; no executable or packaged inputs changed."
        if release_only(base, head, paths):
            if proof(base):
                return {"run_code": False, "run_package": True, "run_benchmarks": False}, f"Version-only release; reuse successful push CI for {base}. Build and smoke test the new version."
            return FULL.copy(), "Version-only change has no successful base push CI; full validation."
        # Rust/core/build/CI changes retain benchmark smoke. Packaging and JS-only
        # changes still receive ordinary validation without optimized benchmarks.
        benchmarks = any(path.startswith(("src/", "benches/", "tests/", ".cargo/", ".github/"))
                         or path in {"Cargo.toml", "Cargo.lock", "build.rs", "rust-toolchain", "rust-toolchain.toml"}
                         for path in paths)
        return {"run_code": True, "run_package": True, "run_benchmarks": benchmarks}, "Validate changed executable or packaging inputs."
    except (subprocess.SubprocessError, OSError, ValueError, KeyError, TypeError) as error:
        return FULL.copy(), f"Classification unavailable ({type(error).__name__}); full validation."


if __name__ == "__main__":
    selection, reason = classify(os.environ.get("BASE_SHA", ""), os.environ["HEAD_SHA"])
    with open(os.environ["GITHUB_OUTPUT"], "a") as output:
        for key, value in selection.items():
            print(f"{key}={str(value).lower()}", file=output)
    with open(os.environ["GITHUB_STEP_SUMMARY"], "a") as summary:
        print(reason, file=summary)

"""Conservative CI selection; unknown inputs always require full validation."""

import json
import os
import re
import subprocess
import tomllib
from urllib.parse import quote

DOCS = {"AGENTS.md", "CONTEXT.md", "DEVELOPMENT.md", "BENCHMARKING.md",
        "CHANGELOG.md", "SECURITY.md"}
RELEASE_FILES = {"desktop/Cargo.toml", "Cargo.toml", "Cargo.lock", ".release-please-manifest.json", "CHANGELOG.md"}
FULL = {"run_code": True, "run_desktop": True, "run_package": True, "run_benchmarks": True}


def documentation(path):
    if path == "docs/platforms/macos-testing.md":
        return False  # Shipped inside the macOS app archive.
    return (path in DOCS or path == "desktop/AGENTS.md"
            or (path.startswith("docs/") and path.endswith(".md")))


def website(path):
    # Static marketing assets are not embedded or packaged by either binary.
    # The separate Website workflow owns their build and browser validation.
    return path.startswith("website/") or path == ".github/workflows/website.yml"


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
    members = [tomllib.loads(read(ref, "desktop/Cargo.toml")) for ref in (base, head)]
    if members[0]["package"]["version"] != old or members[1]["package"]["version"] != new:
        return False
    members[1]["package"]["version"] = old
    if members[0] != members[1]:
        return False
    locks = [tomllib.loads(read(ref, "Cargo.lock")) for ref in (base, head)]
    for lock, version in zip(locks, (old, new)):
        for name in ("boomux", "boomux-desktop"):
            roots = [p for p in lock["package"] if p["name"] == name and "source" not in p]
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


COMPONENT_STEPS = {
    "macos": ("macos / Native macOS", {"Run native Clippy", "Test descriptor transfer", "Test native process identity", "Test native lifecycle and recovery", "Test native Desktop"}),
    "backend": ("Rust", {"Run Clippy", "Run Rust unit tests", "Run configuration CLI tests", "Run native backend tests"}),
    "desktop": ("Desktop Rust", {"Run Desktop Clippy", "Run Desktop tests"}),
    "integrations": ("Integrations", {"Verify embedded web terminal assets", "Run integration tests"}),
    "dependencies": ("Dependency policy", {"Audit advisories, licenses, and dependency sources"}),
}


def changed_paths(base, head):
    return set(git("diff", "--name-only", "--no-renames", "-z", base, head).decode().rstrip("\0").split("\0")) - {""}


def reusable_components(source, base):
    """Only proven input-preserving changes can inherit an ancestor's checks."""
    if source == base:
        return set(COMPONENT_STEPS)
    if not re.fullmatch(r"[0-9a-f]{40}", source):
        return set()
    ancestor = subprocess.run(["git", "merge-base", "--is-ancestor", source, base],
                              stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, timeout=30)
    if ancestor.returncode != 0:
        return set()
    paths = changed_paths(source, base)
    desktop = {path for path in paths if path.startswith("desktop/src/") and path.endswith(".rs")}
    # Versions may have changed alongside Desktop source since the last full
    # backend run. Validate the exact metadata transformation before ignoring it.
    metadata = paths - desktop - {path for path in paths if documentation(path) or website(path)}
    if metadata and release_only(source, base, metadata):
        paths -= RELEASE_FILES
    paths = {path for path in paths if not documentation(path) and not website(path)}
    if not paths:
        return set(COMPONENT_STEPS)
    if paths <= desktop:
        return set(COMPONENT_STEPS) - {"desktop", "macos"}
    # Shared manifests, dependencies, CI definitions, renames, or unknown inputs
    # invalidate inherited evidence. A green run or a cache hit alone is no proof.
    return set()


def validated_base(sha, evidence=None):
    repo = os.environ["GITHUB_REPOSITORY"]
    branch = os.environ["DEFAULT_BRANCH"]
    response = subprocess.check_output([
        "gh", "api", f"repos/{repo}/actions/workflows/ci.yml/runs?event=push&branch={quote(branch, safe='')}&status=success&per_page=20"
    ], timeout=30)
    missing = set(COMPONENT_STEPS)
    records = []
    lookups = 0
    for run in json.loads(response)["workflow_runs"][:20]:
        if not (run["event"] == "push" and run["conclusion"] == "success"
                and run["head_branch"] == branch and run["head_repository"]["full_name"] == repo):
            continue
        eligible = reusable_components(run["head_sha"], sha) & missing
        if not eligible:
            continue
        if lookups >= 6:
            break
        lookups += 1
        jobs = json.loads(subprocess.check_output([
            "gh", "api", f"repos/{repo}/actions/runs/{int(run['id'])}/jobs?per_page=100"
        ], timeout=30))["jobs"]
        for component in eligible:
            name, steps = COMPONENT_STEPS[component]
            if any(job["name"] == name and job["conclusion"] == "success"
                   and steps <= {step["name"] for step in job["steps"] if step["conclusion"] == "success"}
                   for job in jobs):
                missing.remove(component)
                records.append((component, run["id"], run["head_sha"]))
        if not missing:
            if evidence is not None:
                evidence.extend(records)
            return True
    return False



def classify(base, head, proof=validated_base, event="push"):
    try:
        if not re.fullmatch(r"[0-9a-f]{40}", base) or base == "0" * 40:
            return FULL.copy(), "No usable comparison base; full validation."
        paths = changed_paths(base, head)
        if all(documentation(path) for path in paths):
            return dict.fromkeys(FULL, False), "Documentation-only change; no executable or packaged inputs changed."
        if all(documentation(path) or website(path) for path in paths):
            return dict.fromkeys(FULL, False), "Website-only change; the Website workflow owns validation."
        if release_only(base, head, paths):
            if proof(base):
                package = event not in {"pull_request", "merge_group"}
                action = "Build and smoke test the merged release version." if package else "Metadata-only PR validation; final artifacts are built after merge."
                return {"run_code": False, "run_desktop": False, "run_package": package, "run_benchmarks": False}, f"Version-only release; reuse proven component checks for {base}. {action}"
            return FULL.copy(), "Version-only change has no successful base push CI; full validation."
        executable_paths = {path for path in paths if not documentation(path) and not website(path)}
        if executable_paths and all(path.startswith("desktop/src/") and path.endswith(".rs")
                                    for path in executable_paths):
            return {"run_code": False, "run_desktop": True, "run_package": False, "run_benchmarks": False}, "Desktop-only Rust change; validate and build Desktop. Backend, dependencies, and packaging inputs are unchanged."
        # Rust/core/build/CI changes retain benchmark smoke. Packaging and JS-only
        # changes still receive ordinary validation without optimized benchmarks.
        benchmarks = any(path.startswith(("src/", "benches/", "tests/", "desktop/src/", "vendor/", ".cargo/", ".github/"))
                         or path in {"Cargo.toml", "Cargo.lock", "build.rs", "rust-toolchain", "rust-toolchain.toml"}
                         for path in paths)
        return {"run_code": True, "run_desktop": True, "run_package": True, "run_benchmarks": benchmarks}, "Validate changed executable or packaging inputs."
    except (subprocess.SubprocessError, OSError, ValueError, KeyError, TypeError) as error:
        return FULL.copy(), f"Classification unavailable ({type(error).__name__}); full validation."


if __name__ == "__main__":
    evidence = []
    selection, reason = classify(os.environ.get("BASE_SHA", ""), os.environ["HEAD_SHA"],
                                 proof=lambda sha: validated_base(sha, evidence),
                                 event=os.environ.get("GITHUB_EVENT_NAME", ""))
    with open(os.environ["GITHUB_OUTPUT"], "a") as output:
        for key, value in selection.items():
            print(f"{key}={str(value).lower()}", file=output)
    with open(os.environ["GITHUB_STEP_SUMMARY"], "a") as summary:
        print(reason, file=summary)
        for component, run_id, sha in sorted(evidence):
            print(f"\n- {component}: main CI run `{run_id}`, source `{sha}`", file=summary)

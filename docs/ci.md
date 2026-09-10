# CI And Release Validation

CI keeps correctness checks on new code and on the merged main commit. Release
version changes reuse that evidence instead of testing the same code at every
release stage.

| Stage | Validation |
| --- | --- |
| Backend/shared-code PR | Formatting, backend and Desktop Clippy/tests, integration fixtures, dependency policy, release packaging on both architectures, Arch compatibility; benchmark smoke when relevant |
| Desktop-only Rust PR | Formatting, Desktop Clippy/tests, and optimized Desktop build; omit unchanged backend tests, integrations, dependency audit, CLI packaging, and backend benchmarks |
| Code merge to main | Same component selection on the actual merged commit; main also saves dependency caches |
| Release Please proposal generation | No builds or test suites; runs after successful main push CI |
| Version-only release PR / merge group | Verify strict metadata-only changes and reusable component evidence; defer release builds until merge. Missing evidence selects full validation. |
| Version-only release merge | Reuse proven correctness checks; build and smoke test both release architectures and the Desktop bundle for the exact new main commit, validate packaging, and check Arch compatibility |
| Automatic publication | Download that main CI run's artifacts; verify source SHA, version, architecture, and checksums; validate pinned omarchy-boomux compatibility; upload and publish |
| Manual release recovery | Rebuild and smoke test the requested draft source, then run consumer compatibility and publication checks; an already published tag retains its compatibility-only path |

PR and main correctness checks cover different integration commits. They remain
separate. Automatic publication does not rebuild or rerun the unit, native,
integration, or benchmark suites. The new version is compiled and smoke tested after the release PR merges,
because the version is embedded in the binary and used by update/packaging
behavior. A packaging failure then blocks publication and requires a fix or
explicit recovery; lightweight release PR checks cannot prove the final binary.

## Conservative Selection

The static marketing site in `website/` has a separate Website workflow with
an Astro production build and desktop/mobile browser tests. Website-only changes
(including its own workflow) do not select Rust or packaging checks. Mixed
changes retain the validation for their affected product components; renames
out of packaged/runtime paths cannot use the website-only skip. The site workflow
also watches its imported repository screenshot. Successful main site builds
deploy to GitHub Pages once Pages is enabled; local builds never publish.

`.github/scripts/classify-ci.py` produces independent decisions for backend
validation, Desktop validation, packaging, and backend benchmark smoke. A failed diff, missing Git base,
unavailable CI evidence, or malformed release metadata requires full validation.
The workflow also defaults to full work if classification fails. Manual CI runs
have no diff base and run all checks. Merge-group events are supported.

Documentation-only skips apply to Markdown under `docs/`, `desktop/AGENTS.md`,
and the explicitly listed root guidance/changelog files. The packaged `README.md`, embedded
`THIRD_PARTY_NOTICES.md`, and `.agents/skills/boomux/SKILL.md` are executable or
packaging inputs and cannot use the documentation skip.

A version-only release must satisfy all of these conditions:

- The diff contains only `Cargo.toml`, `Cargo.lock`, the Release Please manifest,
  and/or `CHANGELOG.md`, with both Cargo files changed.
- Parsed Cargo manifests differ only in the project's strict release version.
- Parsed lockfiles differ only in the source-less Boomux package version;
  dependencies and checksums remain identical.
- The Release Please manifest matches the new version and has no other changes.
- Successful default-branch `CI` push runs provide actual executed steps for
  backend, Desktop, integrations, and dependency policy. Evidence may come from
  different ancestor commits only when their relevant inputs remain unchanged.
  A green documentation-only run contributes no new component evidence.
  PR names, labels, authors, and dependency caches are not proof.

Each CI lookup has a 30-second timeout. Evidence lookup considers one page of
20 recent successful main push runs and at most six job lists of 100 jobs each.
Candidates must belong to this repository and be ancestors of the base commit.
Missing, inaccessible, malformed, or out-of-window evidence selects full
validation. Source or dependency changes mixed into a release PR still receive
full checks. The selection summary records each reused component's run and SHA.

After excluding guidance, a change consisting entirely of `.rs` files under
`desktop/src/` selects Desktop validation only. Deletions still select Desktop;
renames use both old and new paths, so moving backend code cannot skip backend
validation. Desktop formatting and a release build remain checked even when the
backend and bundle jobs are omitted. Full runs build the optimized Desktop in
the bundle job instead of building it twice. Backend/shared changes keep Desktop
checks because Desktop depends on the root crate.

Cargo manifests, the shared lockfile, toolchains, vendored code, workflow changes,
and unknown inputs never take the Desktop-only path. Packaged documentation and
installer/assets still require packaging coverage. A source-only change does not
rebuild the unchanged packaging machinery or CLI architectures; release version
changes always build both CLI architectures and the complete bundle.

A Desktop-only main success supplies Desktop evidence. Backend, integration,
and dependency-policy evidence may be inherited from an earlier successful main
run when the intervening diff contains only Desktop Rust, guidance, website, and
strictly validated project-version changes. Guidance-only runs can inherit all
components. Shared manifests, dependencies, workflow changes, renames out of
core, and unknown inputs invalidate inheritance. This deliberately conservative
input policy does not infer equivalence across arbitrary backend changes.

Component evidence currently controls release correctness reuse as a group:
all required components must be proven, or the release receives full validation.
Packaging is always fresh after a release-version merge; it is not inherited.

Backend Rust source, test, benchmark, Cargo/toolchain/build, and `.github/` changes retain
optimized benchmark smoke. Packaging and JavaScript-only changes still receive
ordinary validation but omit the optimized benchmarks. Clippy's
`--all-targets --all-features` already checks the benchmark targets, so CI does
not also run a separate `cargo check --benches`. Both Criterion smoke suites
share one Cargo invocation and feature set to avoid recompiling Boomux between
them. Weekly/manual Performance
measurements remain independent of these smoke checks.

## Caches And Artifact Handoff

Rust checks, benchmarks, and each native packaging architecture use separate
commit-pinned rust-cache action configurations. Only main push jobs save caches;
PRs restore them. Cargo/toolchain inputs participate in cache invalidation.
Recovery shares the packaging cache key and restores without saving. Caches
accelerate dependency compilation; they are not validation evidence.

Both architecture jobs retain their archives, SHA-256 checksum files, and
`ci-source.json` for 14 days. The source metadata binds the archive digest to
its exact commit, version, and target. Automatic publication downloads artifacts
from the successful triggering run ID, using read-only Actions access, and
verifies that metadata against the release source before forwarding only the
archive and checksum to publication. GitHub documents the cross-run download
inputs in [download-artifact](https://github.com/actions/download-artifact#download-artifacts-from-other-workflow-runs-or-repositories).

Missing, expired, corrupted, or mismatched artifacts fail automatic publication.
There is no silent rebuild fallback. Manually dispatch Release Please with the
draft tag for explicit recovery. Manual dispatch requires a tag and cannot
create a new release proposal outside successful main CI. The existing publisher rejects conflicting
already-uploaded assets instead of replacing them silently.

Jobs retain their existing required-check names when their work is skipped.
Dependent Arch checks do not attempt to download artifacts after failed
packaging. Jobs have explicit timeouts and respect workflow cancellation.

## Validation And Measurements

Run focused workflow-helper fixtures with:

```console
python3 -m unittest discover -s .github/scripts -p 'test_ci_*.py'
```

They exercise actual Git diffs, version-only changes with/without CI proof,
embedded Markdown, dependency changes, failure fallbacks, and artifact source
and digest mismatches. The classifier job runs these fixtures on every event.
Run actionlint for workflow expressions and the focused local checks in
`AGENTS.md`. Opening or updating a PR delegates the complete selected matrix to
CI; a full local run is not a prerequisite. CI/workflow-only changes require the
full hosted pipeline before merging, without redundantly running unchanged Rust
suites locally.

Before this change, main CI run `33948018935` took 6m45s: Rust took 6m34s,
benchmark smoke 6m22s, and x86-64/ARM packaging 2m10s/1m45s. Release run
`33948310403` then took 3m08s, including rebuilding both architectures. Parallel
job durations are not additive. These are observations from one run, not an
estimated or measured improvement. Hosted cache behavior, release metadata
selection, artifact download permissions, and elapsed savings need verification
after deployment.

## Development previews

Development previews are published GitHub prereleases, not scheduled nightlies.
The stable release path remains Linux-only; the initial development publisher
promotes the Apple Silicon macOS preview artifact.

1. Push the development branch and open/update its PR. General CI runs its
   selected checks; the `macOS preview` workflow separately runs native backend
   checks and builds, ad-hoc signs, checks glyph rasterization, and smoke-tests
   the app. Both native and package jobs must pass.
2. Manually run `Publish development preview` with that Mac workflow run ID.
   The workflow becomes available for manual dispatch once its definition lands
   on the default branch. No schedule or automatic publication is enabled.
3. The read-only prepare job requires a completed successful Mac run and
   successful general CI for the same source revision, including `CI result`.
   It rejects fork/PR-triggered Mac builds, expired/missing artifacts, wrong
   architecture/source metadata, and mismatched checksums. It prepares reviewable
   release metadata without executing the downloaded app or rebuilding it.
4. The publication job revalidates the candidate, creates a fixed-source tag
   `preview-macos-YYYYMMDD.<build-run-id>`, uploads the exact ZIP and checksum,
   verifies uploaded digests, then publishes with `prerelease=true` and
   `make_latest=false`. GitHub's stable latest release and Desktop stable update
   checks continue to exclude previews.
5. Testers report the tag, chip, macOS version, and reproduction steps. A new
   build gets a new preview tag; published bytes and source tags are never
   silently replaced. An interrupted draft can be resumed if its existing assets
   match. Missing/expired artifacts require a new build, not a silent fallback.

General PR CI validates the proposed integration commit; the native Mac workflow
builds the branch revision itself. Promotion requires both corresponding checks,
not just a successful packaging job. If general CI is pending or failed,
publication stops before creating a tag or release.

The stable draft-release blocker ignores only prerelease drafts in the
`preview-macos-` namespace, so interrupted preview uploads do not stall stable
release proposals. Other drafts retain the existing blocking behavior.

The same helper can prepare a candidate locally for review without publishing:

```sh
GITHUB_REPOSITORY=gardnmi/boomux python3 .github/scripts/development-release.py prepare "<run-id>" /tmp/boomux-preview-review
```

Use a fresh output directory. The explicit `publish` subcommand performs the
remote mutations and repeats validation. Routine publication should use Actions
so its result and permissions are recorded with the workflow run.

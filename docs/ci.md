# CI And Release Validation

CI keeps correctness checks on new code and on the merged main commit. Release
version changes reuse that evidence instead of testing the same code at every
release stage.

| Stage | Validation |
| --- | --- |
| Backend/shared-code PR | Formatting, Linux backend and Desktop Clippy/tests, native macOS checks and app smoke, integration fixtures, dependency policy, release packaging on both architectures, Arch compatibility; benchmark smoke when relevant |
| Desktop-only Rust PR | Formatting, Linux Desktop Clippy/tests/build, native macOS checks and app smoke; omit unchanged backend tests, integrations, dependency audit, CLI packaging, and backend benchmarks |
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
and the explicitly listed root guidance/changelog files. `docs/platforms/macos-testing.md`
is packaged in the Mac archive and cannot use the documentation-only skip. The packaged `README.md`, embedded
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
  backend, Linux Desktop, native macOS, integrations, and dependency policy. Evidence may come from
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
`desktop/src/` selects Linux Desktop and native macOS validation. Deletions still select Desktop;
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

A Desktop-only main success supplies Linux Desktop and native macOS evidence. Backend, integration,
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

## Native macOS validation

The reusable `macos.yml` workflow is called by `CI` for PRs, merge groups,
main pushes, and manual CI runs whenever backend, Desktop, or packaging work is
selected. Guidance/website-only changes and proven metadata-only release PRs
skip it; release pushes build and smoke-test the versioned app. The existing
required **CI result** gate includes the complete macOS call, so native failures,
cancellations, or unexpected skips block merging and automatic release work.
No separate branch-protection entry or feature-branch trigger is needed.

Both native jobs check out the same revision as the Linux jobs (the proposed
integration commit on PRs). Apple Silicon `macos-15` runs backend/Desktop Clippy,
descriptor and process-identity tests, the selected serial native lifecycle
scenarios, and platform-applicable Desktop tests. The four stable Linux bundle
restart/process fixtures remain in Linux CI because Mac app-bundle updates are
not supported; portable bundle validation still runs on both platforms. Keyboard
fixtures verify Command shortcuts and native Option text on Mac. Lifecycle tests
are compiled before their bounded
scenario deadlines. The package job builds both optimized executables, packages
and ad-hoc signs the app, checks native glyphs and dependencies, and exercises
window creation, Shell survival, and daemon restart. Native and app diagnostics
are retained separately from the ZIP and checksum.

Release evidence reuse now requires the native job's actual successful test
steps. Pre-port Linux-only runs cannot satisfy it, and Desktop source changes
invalidate prior native evidence. Mac compilation and lifecycle coverage remain
required even while distribution is a testing preview. Human keyboard/IME,
Retina, notification, and sleep/wake coverage remains in the testing guide.

The former `macos-preview.yml` branch-push workflow and its exploratory process
API probe have been retired. The backend and Desktop scheduled performance
workflows remain useful independent measurements and use distinct concurrency
groups so they cannot cancel one another.

## Development previews

Development previews are manually published GitHub prereleases, not scheduled
nightlies. Stable assets remain Linux-only. Merging the macOS port does not
advertise notarization, Intel support, or app-bundle automatic updates.

1. Let a **CI** main push complete, or manually dispatch **CI** for the intended
   development branch. Native and package macOS jobs must run and pass. PR and
   merge-group builds validate integration but are not publication candidates.
2. Run **Publish development preview** with that successful CI run ID. It promotes
   the exact ZIP and checksum from the run without rebuilding or executing the
   downloaded app. No automatic publication is enabled.
3. The read-only prepare job verifies same-repository source, successful native
   test steps, packaging smoke, and **CI result**. Missing/expired artifacts,
   source or architecture mismatches, and incorrect checksums reject publication.
4. The publication job revalidates the candidate, creates a fixed-source tag
   `preview-macos-YYYYMMDD.<build-run-id>`, uploads the exact ZIP and checksum,
   verifies uploaded digests, then publishes with `prerelease=true` and
   `make_latest=false`. Stable latest-release and Desktop update checks exclude
   previews.
5. Testers report the tag, chip, macOS version, and reproduction steps. A new
   build gets a new preview tag; published bytes and source tags are never
   replaced. An interrupted draft can resume only when existing assets match.

Previously successful standalone `macos-preview.yml` push/manual runs remain
valid candidates while their artifacts exist, with their original native-step
requirements and matching successful general CI. New candidates come from CI.
The immutable installer for the already-published preview remains pinned to its
original release; it is not a latest-preview updater.

The stable draft-release blocker ignores only prerelease drafts in the
`preview-macos-` namespace, so interrupted preview uploads do not stall stable
release proposals. Other drafts retain the existing blocking behavior.

The same helper can prepare a candidate locally for review without publishing:

```sh
GITHUB_REPOSITORY=gardnmi/boomux python3 .github/scripts/development-release.py prepare "<run-id>" /tmp/boomux-preview-review
```

Use a fresh output directory. The explicit `publish` subcommand performs the
remote mutations and repeats validation. To publish from an experimental branch
without adding a workflow to `main`, authenticate `gh` with release-write access
and run both steps against the same fresh directory:

```sh
GITHUB_REPOSITORY=gardnmi/boomux python3 .github/scripts/development-release.py prepare "<run-id>" /tmp/boomux-preview-publication
GITHUB_REPOSITORY=gardnmi/boomux python3 .github/scripts/development-release.py publish "<run-id>" /tmp/boomux-preview-publication
```

The resulting prerelease and its assets are publicly downloadable. The first
preview also has `.github/scripts/install-macos-preview.sh`: it pins the release,
archive checksum, and installation directory to build `66bca02a`. Share its raw
URL pinned to the installer commit, never a moving branch URL. It installs into
`~/Applications`, verifies the archive and ad-hoc signature, and refuses to
replace an existing app. A newer preview needs an explicitly updated installer.
It does not alter Gatekeeper settings; testers may need to approve the app in
Privacy & Security. Once the dispatcher reaches the default branch, routine
publication can use Actions to record its result and permissions with the run.

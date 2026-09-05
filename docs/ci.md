# CI And Release Validation

CI keeps correctness checks on new code and on the merged main commit. Release
version changes reuse that evidence instead of testing the same code at every
release stage.

| Stage | Validation |
| --- | --- |
| Code PR | Formatting, Clippy, Rust unit/CLI/serial native tests, integration fixtures, dependency policy, release packaging on both architectures, Arch compatibility; benchmark smoke when relevant |
| Code merge to main | Same checks on the actual merged commit; main also saves dependency caches |
| Release Please proposal generation | No builds or test suites; runs after successful main push CI |
| Version-only release PR | Reuse successful base main CI; build and smoke test both release architectures, validate packaging scripts, and check Arch compatibility |
| Version-only release merge | Same selection, with artifacts built for the exact new main commit |
| Automatic publication | Download that main CI run's artifacts; verify source SHA, version, architecture, and checksums; validate pinned omarchy-boomux compatibility; upload and publish |
| Manual release recovery | Rebuild and smoke test the requested draft source, then run consumer compatibility and publication checks; an already published tag retains its compatibility-only path |

PR and main correctness checks cover different integration commits. They remain
separate. Automatic publication does not rebuild or rerun the unit, native,
integration, or benchmark suites. The new version is still compiled and smoke
tested on the release PR and its merge because the version is embedded in the
binary and used by update/packaging behavior.

## Conservative Selection

`.github/scripts/classify-ci.py` produces independent decisions for ordinary
validation, packaging, and benchmark smoke. A failed diff, missing Git base,
unavailable CI evidence, or malformed release metadata requires full validation.
The workflow also defaults to full work if classification fails. Manual CI runs
have no diff base and run all checks. Merge-group events are supported.

Documentation-only skips apply to Markdown under `docs/` and the explicitly
listed root guidance/changelog files. The packaged `README.md`, embedded
`THIRD_PARTY_NOTICES.md`, and `.agents/skills/boomux/SKILL.md` are executable or
packaging inputs and cannot use the documentation skip.

A version-only release must satisfy all of these conditions:

- The diff contains only `Cargo.toml`, `Cargo.lock`, the Release Please manifest,
  and/or `CHANGELOG.md`, with both Cargo files changed.
- Parsed Cargo manifests differ only in the project's strict release version.
- Parsed lockfiles differ only in the source-less Boomux package version;
  dependencies and checksums remain identical.
- The Release Please manifest matches the new version and has no other changes.
- GitHub reports successful `CI` push validation for the exact base SHA on this
  repository's default branch, with the Rust lint/unit/CLI/native steps actually
  executed successfully. A green documentation-only run is not sufficient.
  PR names, labels, and authors are not proof.

Each CI lookup has a 30-second timeout: at most one page of 100 runs and one
page of 100 jobs from the selected successful run. Missing
or inaccessible evidence causes validation to run. Source or dependency changes
mixed into a release PR always receive full checks.

Rust source, test, benchmark, Cargo/toolchain/build, and `.github/` changes retain
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
Run actionlint for workflow expressions, plus the repository validation set in
`AGENTS.md` before opening a PR.

Before this change, main CI run `33948018935` took 6m45s: Rust took 6m34s,
benchmark smoke 6m22s, and x86-64/ARM packaging 2m10s/1m45s. Release run
`33948310403` then took 3m08s, including rebuilding both architectures. Parallel
job durations are not additive. These are observations from one run, not an
estimated or measured improvement. Hosted cache behavior, release metadata
selection, artifact download permissions, and elapsed savings need verification
after deployment.

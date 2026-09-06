# Desktop in shared CI

Root `.github/workflows/ci.yml` validates the backend, integrations, benchmark
fixtures, dependency policy, CLI release candidates, and Desktop. Backend jobs
select `-p boomux`; Desktop jobs select `-p boomux-desktop` and install Zig 0.15.2
and graphics development libraries. `CI result` is the aggregate required check.
Unknown classification inputs fail closed to full validation. Documentation-only
changes skip executable work; packaged README/license changes do not.

The vendored Ghostty build uses Zig's explicit baseline CPU target. Desktop
cache keys include `baseline-v1` to prevent restoring artifacts from the previous
native-CPU build. This makes those artifacts portable across hosted runner CPUs
without fragmenting the cache by CPU model.

PRs, merge groups, main pushes, and manual runs build and test the complete
candidate bundle when executable inputs change. X11 emulates Desktop, the
foreground Boomux daemon, and CLI invocations with QEMU's Nehalem CPU model;
Wayland exercises the native packaged launcher under Weston. These are isolated
runtime/display tests and never contact the user's ordinary daemon. Both paths
verify window readiness, PTY output, and the same ShellRun after reopening.
The native path also checks configuration editing and bundle updater ownership.

Version-only releases may reuse successful base source-test evidence only when
both backend and Desktop checks actually ran, along with integration/dependency
checks. They still rebuild and smoke-test the new version. Automatic publication
reuses the exact main CI artifacts and verifies source/version/target/checksums.
It does not rebuild or repeat successful display tests. Explicit recovery builds
fresh candidates and runs display tests before publication.

The CLI ARM64 and pinned Arch checks remain, as does omarchy-boomux capability
validation. No ARM Desktop, macOS, Windows, musl, or older-glibc support follows
from sharing a repository. The X11 CPU smoke does not exercise graceful daemon
replacement under emulation; native handoff tests cover that lifecycle separately.

Scheduled Desktop measurements remain in `desktop-performance.yml`; backend
benchmarks remain in `performance.yml`. Shared runner timing is diagnostic,
not evidence for a performance claim or a hard regression threshold.

## Repository cutover

The final source revisions and file inventory are recorded in [the handoff](handoff.md).

Before merging the migration, add `CI result` to the existing required status
checks on `main`. Keep existing required checks until the aggregate is reporting
successfully; it also covers Desktop build and display failures. Source workflow
changes alone do not update GitHub branch protection. The old Desktop repository
remains active until a unified release and installer cutover are verified.

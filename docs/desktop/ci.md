# Desktop in shared CI

Root `.github/workflows/ci.yml` validates the backend, integrations, benchmark
fixtures, dependency policy, CLI release candidates, and Desktop. Backend jobs
select `-p boomux`; Desktop jobs select `-p boomux-desktop` and install Zig 0.15.2
and graphics development libraries. Reusable `macos.yml` adds native Apple Silicon
Clippy, lifecycle and Desktop tests, and packaged-app smoke to the same run.
`CI result` is the aggregate required check for both platforms.
Unknown classification inputs fail closed to full validation. Documentation-only
changes skip executable work; packaged README/license changes do not.

The vendored Ghostty build uses Zig's explicit baseline CPU target. Desktop
cache keys include `baseline-v1` to prevent restoring artifacts from the previous
native-CPU build. This makes those artifacts portable across hosted runner CPUs
without fragmenting the cache by CPU model.

PRs, merge groups, main pushes, and manual runs select validation according to
[root CI rules](../ci.md). Linux bundle jobs run when packaging is selected;
Desktop-only source changes still run native Mac tests and app packaging. X11 emulates Desktop, the
foreground Boomux daemon, and CLI invocations with QEMU's Nehalem CPU model;
Wayland exercises the native packaged launcher under Weston. These are isolated
runtime/display tests and never contact the user's ordinary daemon. Both paths
verify window readiness, PTY output, and the same ShellRun after reopening.
The native path also checks configuration editing and bundle updater ownership.

Version-only releases may reuse successful base source-test evidence only when
backend, Linux Desktop, and native macOS checks actually ran, along with
integration/dependency checks. They still rebuild and smoke-test the new version. Automatic publication
reuses the exact main CI artifacts and verifies source/version/target/checksums.
It does not rebuild or repeat successful display tests. Explicit recovery builds
fresh candidates and runs display tests before publication.

The CLI ARM64 and pinned Arch checks remain, as does omarchy-boomux capability
validation. Apple Silicon macOS 15+ is a separately validated testing preview;
its artifacts use manual prerelease publishing. Linux ARM Desktop, Intel Mac,
Windows, musl, and older-glibc support are not advertised. The X11 CPU smoke does not exercise graceful daemon
replacement under emulation; native handoff tests cover that lifecycle separately.

Scheduled Desktop measurements remain in `desktop-performance.yml`; backend
benchmarks remain in `performance.yml`. Shared runner timing is diagnostic,
not evidence for a performance claim or a hard regression threshold.

## Required checks and cutover history

`main` already requires **CI result** alongside the existing backend/release
checks. The aggregate includes Linux Desktop build/display failures and native
macOS failures; this port needs no new branch-protection check names.

The earlier repository migration is recorded in [the handoff](handoff.md).
Those historical cutover steps are not prerequisites for the macOS merge.

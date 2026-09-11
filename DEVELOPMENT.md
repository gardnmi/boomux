# Development Guide

**Jump to:** [Build](#build-locally) · [Desktop development](#native-desktop-development) · [Isolation](#use-an-isolated-daemon) · [Validation](#local-feedback-and-pr-validation) · [Releases](#release-lifecycle)

This guide describes the supported local development loop from repository setup
through release. Product terminology and implementation contracts remain
authoritative in the documents listed under [Repository Orientation](#repository-orientation).

## Prerequisites

The supported release workflow targets Linux. The macOS preview targets Apple
Silicon on macOS 15+ with Xcode command-line tools and Zig 0.15.2 for Desktop;
see [macOS development and validation](docs/platforms/macos.md). Both use the
stable Rust toolchain with `rustfmt` and Clippy. Install the toolchain with:

```console
rustup toolchain install stable --profile minimal --component rustfmt,clippy
```

The complete validation suite also requires:

- `cargo-deny` for dependency policy checks.
- Bun `1.3.14` for integration tests and embedded web-terminal assets.
- Standard Unix development tools used by the packaging tests.
- A current Omarchy/Hyprland environment only for live desktop validation.

## Repository Orientation

Read repository documentation in this order before changing behavior:

1. [`CONTEXT.md`](CONTEXT.md) defines canonical product terminology.
2. [`docs/architecture.md`](docs/architecture.md) maps modules and invariants.
3. Contract documents govern their named interfaces and guarantees.
4. [`docs/adr/`](docs/adr/) records accepted decisions and rationale.
5. [`docs/lifecycle-validation.md`](docs/lifecycle-validation.md) records
   version-specific live compatibility evidence.
6. [`docs/roadmap.md`](docs/roadmap.md) is non-authoritative future intent.

For exact protocol and persistence versions, source and compatibility tests are
authoritative. Start in the owning module from the architecture module map, then
read its colocated tests and relevant contracts.

## Branches And Worktrees

Create changes on a focused branch. Keeping linked worktrees under one root
makes them easy to find; this guide uses:

```text
$HOME/Worktrees/<repository>/<branch-slug>
```

Create and remove linked worktrees through Git so tools such as Lazygit can
discover them:

```console
git worktree add -b <branch> "$HOME/Worktrees/boomux/<branch-slug>" main
git worktree list
git worktree remove "$HOME/Worktrees/boomux/<branch-slug>"
```

Never delete a registered worktree directory directly.

## Build Locally

Build the development binary without modifying the installed release:

```console
cargo build --locked
./target/debug/boomux --version
./target/debug/boomux capabilities --json
```

The binary is `target/debug/boomux`. Use it directly after the first build to
avoid repeated Cargo startup. Use an optimized build only when validating
release behavior or performance:

```console
cargo build --release --locked
./target/release/boomux --version
```

Do not replace `~/.local/bin/boomux` during ordinary development. Development
builds are intentionally ineligible for self-update.

## Native Desktop Development

The root package is the default Cargo workspace member. Existing CLI commands
continue to work without Zig or graphics build dependencies. Desktop uses a path
dependency on that root package and the same lockfile and version.

Install Zig 0.15.2 (`mise install`) and the graphics development libraries listed
in `.github/workflows/desktop-build.yml`. From the repository root:

```console
python3 desktop/scripts/run-dev.py
cargo check -p boomux-desktop --locked
cargo test -p boomux-desktop <test-name> --locked -- --test-threads=1
```

Cargo development builds keep Rust debug information but compile the native
Ghostty terminal core with `ReleaseFast`, avoiding expensive debug integrity
scans during transcript replay. To debug the native core itself, explicitly set
`LIBGHOSTTY_VT_SYS_OPTIMIZE=Debug` when building or running. Changing this setting
rebuilds the native library.

For a performance comparison, run `python3 desktop/scripts/run-dev.py --release`.
This builds and launches optimized Desktop and daemon binaries against the same
isolated development runtime. The first optimized build takes longer; keep the
default debug build for routine iteration. Close the previous Desktop window
before comparing the same workload.

### What The Helper Isolates

The helper builds both executables, sets a matching CLI PATH, and uses Boomux-only
runtime/config/state directories under `target/desktop-dev/`. It sets
`BOOMUX_RUNTIME_DIR`, `BOOMUX_CONFIG_HOME`, and `BOOMUX_STATE_HOME` instead of
changing XDG variables. When a worktree path would exceed the Unix socket path
limit, the helper uses a
private `/tmp/boomux-dev-<uid>-<worktree-hash>` runtime directory instead; config
and state remain under `target/desktop-dev/`. The runtime path is stable for that
worktree and shared by debug and release launches.
These override only Boomux's corresponding XDG roots
(including Desktop settings); terminal applications retain normal configuration,
plugins, credentials, and desktop runtime services. Closing the window
keeps these development sessions alive. After rebuilding, the helper starts an
absent development daemon or gracefully restarts an existing one with the rebuilt
CLI's explicit executable path, preserving compatible live Shells. A failed
handoff aborts the launch rather than falling back to a destructive stop.

After executable handoff, `--refresh-environment` performs an additional graceful
handoff using the launcher's environment, so daemon-owned sound delivery also
uses the normal desktop audio socket. Existing Shell and shared-harness processes
keep their original environments and PIDs.
The helper prints the isolated runtime
path; it does not replace installed binaries or use the ordinary daemon.
### Harness Configuration Is Shared

This isolation does not cover harness configuration under XDG directories, the
user's `HOME`, or explicit harness overrides. Core's automatic integration maintenance can update
those shared integration files. Before testing integration maintenance, use a
temporary home and harness-specific configuration roots; the focused native
integration fixture demonstrates this without touching real harness settings.
See `desktop/AGENTS.md` for additional validation and `docs/desktop/releases.md`
for packaging and headless GUI smoke tests.

When upgrading from the old XDG-isolated helper, existing running Shells retain
their old environment across handoff. Launch the helper from a normal terminal
and create new Shells to get the corrected environment. Existing development
state and preferences stay at the same paths; no application data is copied or
deleted. Do not stop the daemon just to refresh Shell environments.

## Use An Isolated Daemon

By default, a development binary uses the same XDG socket, state, and
configuration locations as the installed release. Isolate manual testing so a
development daemon cannot adopt or terminate ordinary Boomux processes:

```console
export BOOMUX_DEV_ROOT="$(mktemp -d /tmp/boomux-dev.XXXXXX)"

install -d -m 700 \
  "$BOOMUX_DEV_ROOT/runtime" \
  "$BOOMUX_DEV_ROOT/state" \
  "$BOOMUX_DEV_ROOT/config"

export XDG_RUNTIME_DIR="$BOOMUX_DEV_ROOT/runtime"
export XDG_STATE_HOME="$BOOMUX_DEV_ROOT/state"
export XDG_CONFIG_HOME="$BOOMUX_DEV_ROOT/config"
```

Every development command in that shell now uses an isolated daemon socket,
Node identity, durable state, and configuration:

Harness configuration is separate: the XDG variables above do not redirect
`CODEX_HOME`, `CLAUDE_CONFIG_DIR`, `PI_CODING_AGENT_DIR`, `KIRO_HOME`, or their
`HOME`-based defaults. Automatic integration maintenance still uses those paths.

```console
./target/debug/boomux workspace create development
./target/debug/boomux shell create development --cwd "$PWD"
./target/debug/boomux daemon status
```

Run the dashboard from a fresh terminal carrying the same XDG variables:

```console
./target/debug/boomux ui
```

## Edit, Build, And Run

Use this loop for ordinary Rust changes:

```console
cargo fmt --all
cargo build --locked
./target/debug/boomux daemon restart
```

`daemon restart` performs a graceful handoff from the isolated old daemon to the
newly built executable and preserves compatible Shell processes and PTYs. It is
the preferred way to test daemon changes.

If an intentionally incompatible protocol or persistence change prevents
handoff, stop only the isolated development daemon:

```console
./target/debug/boomux daemon stop
```

Stopping a daemon terminates every process it manages. Verify the isolated XDG
variables before running that command.

## Focused Tests

Run the narrowest relevant test while iterating:

```console
cargo test --lib <test-name> --locked -- --test-threads=1
cargo test --test config_cli <test-name> --locked -- --test-threads=1
cargo test --test native_backend <test-name> --locked -- --test-threads=1
```

Native backend tests must remain serial because they exercise process, socket,
PTY, and daemon lifecycle behavior. Use the change-type coverage table in
[`AGENTS.md`](AGENTS.md) to select additional compatibility tests.

## Web And Integration Changes

The public landing page is a separate Astro site in `website/`, not the embedded
web dashboard. See [`website/README.md`](website/README.md) for its local preview,
browser tests, and GitHub Pages setup. Website-only work does not require Rust
builds or live daemon operations.

Install the pinned JavaScript dependencies and rebuild embedded terminal assets:

```console
bun install --frozen-lockfile
bun run build:web-terminal
git diff --exit-code -- \
  assets/mobile-web/terminal.js \
  assets/mobile-web/terminal.css \
  assets/mobile-web/ghostty-vt.wasm
```

Run the integration reducers with:

```console
bun test ./integrations/opencode/boomux.test.js \
  ./integrations/opencode/boomux-tui.test.js \
  ./integrations/pi/boomux.test.js
```

## Performance Benchmarks

Use [`BENCHMARKING.md`](BENCHMARKING.md) for the benchmark tiers, fixture policy,
local commands, and interpretation rules. Before changing a benchmarked hot path,
save a local Criterion baseline on the same machine. For benchmark changes or
local reproduction of a benchmark CI failure, run the deterministic fixtures and
smoke suite below. Ordinary edits leave the full benchmark checks to PR CI:

```console
cargo test --test benchmark_harness --features benchmark-internals --locked
cargo bench --bench core_cpu --bench wire --features benchmark-internals --locked -- --test
```

Criterion timing on shared runners is evidence, not a merge gate. Gungraun provides
the deterministic instruction-count tier and requires Valgrind for execution.

## Local Feedback And PR Validation

Use formatting, `cargo check --locked` (or `cargo check -p boomux-desktop --locked`),
and the narrowest relevant tests during development. Build or run the app when
needed to exercise the changed behavior. A filtered test still needs its test
binary and dependencies compiled; start with a compile check for small edits.
Avoid full suites and release builds for routine UI or implementation changes.

Open or update the PR after focused local checks; CI runs the complete selected
validation matrix. Do not duplicate that matrix locally as a pre-PR gate. Require
all selected CI checks before merging. Use broader local runs to investigate a
specific failure or cross-cutting risk, or when explicitly requested. Build an
optimized binary when measuring performance or validating release/packaging
behavior, rather than on every edit.

The full commands below are a reference for reproducing CI failures or explicitly
requested comprehensive local validation:

```console
cargo fmt --all -- --check
cargo clippy --all-targets --all-features --locked -- -D warnings
cargo test --lib --bins --locked -- --test-threads=1
cargo test --test config_cli --locked -- --test-threads=1
cargo test --test native_backend --locked -- --test-threads=1
cargo test --test benchmark_harness --features benchmark-internals --locked
cargo bench --bench core_cpu --bench wire --features benchmark-internals --locked -- --test
cargo deny check
bun test ./integrations/opencode/boomux.test.js ./integrations/opencode/boomux-tui.test.js ./integrations/pi/boomux.test.js
```

CI selects work by changed inputs and prior validation, as documented in
[`docs/ci.md`](docs/ci.md). Documentation skips use an explicit allowlist;
embedded skill Markdown, notices, and the packaged README still receive checks.
Version-only release changes can reuse successful base CI while building and
smoke testing the new release version. Clippy covers every benchmark target;
optimized benchmark smoke runs for Rust, benchmark, build, and CI changes.

## Compatibility Checklist

Protocol changes require version negotiation, additive defaults or downgrade
behavior, capability updates, mixed-version tests, and protocol-history
documentation when wire behavior changes.

Persisted state changes require a state-version bump, retention of the previous
schema, an explicit migration, invalid-state coverage, and cold-recovery tests.

Process and PTY changes require colocated unit tests plus serial native scenarios.
Host compatibility claims require focused fixtures and live evidence in
`docs/lifecycle-validation.md`.

Across all changes, preserve exact argument vectors, ephemeral attachment
environments, persistence-before-event publication, daemon lock ordering, and
run-scoped Agent lifecycle authority.

## Commits And Pull Requests

Use Conventional Commits for commits and pull request titles:

```text
feat(scope): add capability
fix(scope): correct behavior
docs: explain development workflow
ci: skip builds for documentation-only changes
```

Use `feat` for a user-visible minor release and `fix`, `perf`, or `refactor` for
a patch release. Use `docs`, `test`, `build`, `ci`, or `chore` only when there is
no release-visible product change. Mark breaking changes with `!` and a
`BREAKING CHANGE:` footer.

CI must pass before squash-merging into `main`. The Conventional pull request
title becomes the squash commit consumed by Release Please.

## Release Lifecycle

After CI succeeds for the exact current `main` commit, Release Please creates or
updates the release pull request. For strict version-only PRs, CI checks shared
version metadata and verifies reusable component evidence from successful main
runs; it defers artifact builds until merge. Missing evidence or mixed source
changes select full validation. A Desktop-only main run can inherit unchanged
backend evidence from an earlier main run.

Merging the release PR updates the version and changelog. CI builds and smoke
tests x86_64 and aarch64 artifacts and the Desktop bundle for that exact commit.
It reuses proven correctness checks. The release workflow downloads those exact
artifacts, verifies source metadata and checksums, checks consumer compatibility,
renders installers, and publishes. Packaging failures after merge block
publication. Manual tag dispatch remains reserved for explicit recovery.
See [`docs/ci.md`](docs/ci.md) for the full selection and evidence contract.

### Development previews

macOS validation runs inside the normal **CI** workflow on PRs and `main`.
The required **CI result** includes native tests and packaged-app smoke. See
[CI selection and preview promotion](docs/ci.md#native-macos-validation).

To distribute an Apple Silicon testing preview, wait for a successful **CI**
main push, or manually run **CI** for a development branch. Run **Publish
development preview** with that CI run ID. It promotes the exact tested ZIP
without rebuilding, using a unique `preview-macos-YYYYMMDD.<run-id>` prerelease
tag. PR/merge-group artifacts cannot be published. Regular releases also promote the canonical Mac ZIP from their exact successful
main CI run. The universal installer selects Linux or Apple Silicon macOS. Mac
support remains experimental, ad-hoc signed, and not notarized; automatic
app-bundle updates are not provided.


## Clean Up

Stop the isolated daemon before deleting its temporary state:

```console
./target/debug/boomux daemon stop
rm -rf -- "${BOOMUX_DEV_ROOT:?}"
unset BOOMUX_DEV_ROOT XDG_RUNTIME_DIR XDG_STATE_HOME XDG_CONFIG_HOME
```

Do not use this cleanup sequence unless `BOOMUX_DEV_ROOT` identifies the
temporary development directory created above.

## Security Reports

Report suspected vulnerabilities through GitHub Private Vulnerability Reporting
as described in [`SECURITY.md`](SECURITY.md). Do not include terminal contents,
credentials, private paths, external session IDs, or configuration contents
unless essential and redacted.

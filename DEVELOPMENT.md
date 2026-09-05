# Development Guide

This guide describes the supported local development loop from repository setup
through release. Product terminology and implementation contracts remain
authoritative in the documents listed under [Repository Orientation](#repository-orientation).

## Prerequisites

Boomux development requires Linux and the stable Rust toolchain with `rustfmt`
and Clippy. Install the toolchain with:

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
bun test integrations/opencode/boomux.test.js \
  integrations/opencode/boomux-tui.test.js \
  integrations/pi/boomux.test.js
```

## Performance Benchmarks

Use [`BENCHMARKING.md`](BENCHMARKING.md) for the benchmark tiers, fixture policy,
local commands, and interpretation rules. Before changing a benchmarked hot path,
save a local Criterion baseline on the same machine. Before opening a code PR, run
the deterministic benchmark fixtures and smoke suite:

```console
cargo test --test benchmark_harness --features benchmark-internals --locked
cargo bench --bench core_cpu --bench wire --features benchmark-internals --locked -- --test
```

Criterion timing on shared runners is evidence, not a merge gate. Gungraun provides
the deterministic instruction-count tier and requires Valgrind for execution.

## Complete Validation

Before opening a pull request that changes code, configuration, packaging, or
generated assets, run the same checks as CI:

```console
cargo fmt --all -- --check
cargo clippy --all-targets --all-features --locked -- -D warnings
cargo test --lib --bins --locked -- --test-threads=1
cargo test --test config_cli --locked -- --test-threads=1
cargo test --test native_backend --locked -- --test-threads=1
cargo test --test benchmark_harness --features benchmark-internals --locked
cargo bench --bench core_cpu --bench wire --features benchmark-internals --locked -- --test
cargo deny check
bun test integrations/opencode/boomux.test.js integrations/opencode/boomux-tui.test.js integrations/pi/boomux.test.js
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
updates the release pull request. Merging that pull request updates the package
version and changelog. CI builds and smoke tests x86_64 and aarch64 artifacts
for that exact commit. When only release metadata changed and the base has
successful push CI, it reuses that validation instead of rerunning the test
suites. The release workflow downloads the exact CI artifacts, checks their
source metadata and checksums, validates consumer compatibility, renders the
installer, and publishes the completed release. Manual tag dispatch is reserved
for explicit recovery, including rebuilding when the CI artifacts have expired.
See [`docs/ci.md`](docs/ci.md) for the stage-by-stage contract.

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

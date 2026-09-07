# Repository Workflow

## Start Here

Read the root `AGENTS.md`, `CONTEXT.md`, and `docs/architecture.md` first.
Then read `docs/desktop/architecture.md`, `docs/desktop/performance.md`, the
root ADRs, and `desktop/README.md`. Paths here are relative to the repository
root unless stated otherwise.

Boomux Desktop is a presentation client of the root Boomux package. Core PTY,
persistence, protocol, Workspace, Shell, ShellRun, Agent, and Node authority stays
in the backend. GPUI rendering and input live in `desktop/`.

## Validation

For Desktop Rust-only changes, run:

```console
cargo fmt --all -- --check
cargo clippy -p boomux-desktop --all-targets --all-features --locked -- -D warnings
cargo test -p boomux-desktop --locked -- --test-threads=1
cargo build -p boomux-desktop --release --locked
```

Building `libghostty-vt` requires Zig 0.15.2 on `PATH`. Run the narrowest
relevant test while iterating, then the Desktop set above before opening a PR.
Run `cargo deny check` and the complete root/desktop suite for dependency,
Cargo/toolchain, backend, or vendored changes, as specified by root `AGENTS.md`.
For docs-only or packaging/workflow-script changes, use the root validation
matrix. All selected CI checks must pass before merging.

### Testing By Change Type

| Change | Expected coverage |
| --- | --- |
| Layout or pointer behavior | Focused model tests for geometry, bounds, direction, and transitions |
| Terminal decoding or input | Emulator tests covering the exact sequence, mode, or key encoding |
| Kitty graphics | Decode, placement, cropping, z-order, and bounded-cache tests |
| Boomux attachment behavior | Client tests here plus protocol or native-backend coverage in Boomux when wire behavior changes |
| Performance hot path | Semantic tests and before/after measurements from the same release build and machine |
| Dependency or platform setup | Locked clean build and the relevant Wayland/X11 startup smoke test |

## Performance And Memory

- Keep daemon calls, socket reads, terminal decoding, and expensive image work
  off GPUI's render path.
- Every queue, cache, retained history, image store, retry loop, and background
  task must have an explicit bound or lifecycle owner.
- Apply backpressure when dropping data would corrupt terminal protocol state.
  Never drop arbitrary bytes from escape sequences or Kitty graphics payloads.
- Scale work with open panes and visible damage, not every durable Boomux Shell
  or Agent record.
- Reclaim pane-owned emulator state and GPU images when a pane closes.
- Avoid per-frame allocation proportional to scrollback or total workspace
  history.
- Do not claim a performance improvement without comparable measurements. Record
  the build profile, pane count, terminal dimensions, workload, sample duration,
  RSS/PSS, CPU, and relevant frame behavior.
- Treat idle footprint and incremental cost separately. A strong empty baseline
  does not excuse poor per-pane or busy-output scaling.

Use `desktop/scripts/memory-report.sh <pid>` for a Linux process snapshot. Add a
repeatable fixture before establishing hard regression thresholds.

## Safety And Product Invariants

- `Ctrl+W` closes only the pane and detaches; it must not permanently close the
  Boomux Shell.
- Preserve exact Boomux Shell and ShellRun identities. Never infer identity from
  a display name or terminal text.
- Ordinary terminal input must continue to reach the PTY unless a documented
  desktop action owns the chord.
- A stale or historical Agent is not an active Agent. Present historical records
  only through their explicit attention/history affordance.
- Keep the current layout intact while a pane is temporarily fullscreen or
  lifted for drag-and-drop.
- Do not block the GPUI thread on daemon I/O, terminal decoding, process waits,
  or sleeps.

## Dependencies

- Pin pre-1.0 UI and terminal dependencies deliberately.
- Use the root Boomux path dependency and shared lockfile. Backend changes
  require Desktop compatibility checks in the same pull request.
- Prefer disabling unused default features, especially when they add a runtime,
  media decoder, or platform backend.
- Run `cargo deny check` after dependency changes and document unavoidable
  duplicate or exceptional dependencies.

## Commits And Pull Requests

- Use Conventional Commits: `type(scope): description` or `type: description`.
- Use `feat` for user-visible capability, `fix` for corrections, `perf` for
  measured performance improvements, and `refactor` for behavior-preserving
  internal changes.
- Use `docs`, `test`, `build`, `ci`, or `chore` when there is no release-visible
  product change.
- Keep descriptions imperative, lowercase, and concise.
- PR titles follow the same convention and should describe the complete release
  impact of the squashed change.

## Git Worktrees

- Create agent-managed Git worktrees only under
  `~/Worktrees/boomux/<branch-slug>`.
- Replace `/` in branch names with `-` for `<branch-slug>`.
- Inspect `git worktree list` first and reuse a matching registered worktree.
- Remove completed clean worktrees with `git worktree remove`; never delete a
  registered worktree directory directly.

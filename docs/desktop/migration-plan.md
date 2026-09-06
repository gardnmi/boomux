# Move Boomux Desktop into Boomux

Status: historical design and analysis, 2026-09-06. The workspace implementation
follows [ADR 0015](../adr/0015-consolidate-native-desktop-workspace.md). The
source snapshots and feasibility results below describe the pre-migration state.
Repository cutover and archival remain separate post-release actions.

## Recommendation

Use `gardnmi/boomux` as the only development repository. Keep the existing
Boomux package at its root and import Desktop as a `desktop/` Cargo workspace
member. Maintain one version, one Release Please proposal, one GitHub release,
and one CI workflow with jobs for the backend, Desktop, and the installed bundle.

Continue producing the `boomux` and `boomux-desktop` executables. CLI users and
remote Nodes keep their existing lightweight download. Desktop users download
the bundle containing both executables built from the same commit and lockfile.

The main tradeoff is a shared release schedule: a Desktop feature can advance
the Boomux release version, and a failed required Desktop check blocks the
combined release. This is intentional to reduce separate release management.
Desktop can retain its experimental designation while sharing Boomux's version.

## What was inspected

| Source | Baseline | Findings relevant to migration |
| --- | --- | --- |
| Boomux remote `main` | `0fd34c8`, package 1.9.7 | Root library and CLI package; Rust/native/JS/benchmark tests; Linux x86_64 and ARM64 packaging; Release Please with exact-commit CI artifact reuse |
| Boomux local checkout | `00c5961`, package 1.9.6 | Behind remote `main`; contains untracked `artifacts/` that must remain untouched |
| Desktop remote `main` | `8c45234`, package 0.1.0 | One binary package, pinned Git dependency on Boomux 1.9.5 at `01254e346dd2f4b09ce18d54278f60e58f43e666` |
| Desktop update branch | `f149e0a`, PR #10 | Dismissible update notices; this is the currently checked-out source |
| Desktop CPU fix | `11fcdc0`, PR #9 | Vendored Ghostty sys-crate patch, baseline Zig CPU target, and emulated-CPU smoke coverage |

Neither Desktop PR is merged at the time of analysis. Desktop has no published
GitHub release; Boomux's latest published release is v1.9.7. Refresh these facts
and freeze exact source SHAs when starting implementation.

The analysis covered both projects' ownership documents, package manifests,
lockfiles, source interfaces, CI/release/performance workflows, installers,
packaging helpers, updater ownership rules, and smoke/native test organization.

Desktop has nine Rust source modules, roughly 14,800 lines including tests.
Its backend coupling is concentrated in:

- `desktop/src/terminal.rs`: `boomux::client` and `boomux::protocol` types and
  attachment behavior.
- `desktop/src/main.rs`: resource projections and protocol enums.
- `desktop/src/boomux_settings.rs`: public daemon defaults and bounded
  `boomux config` / `boomux daemon restart` subprocesses.
- `desktop/src/updates.rs`: public `boomux --json update status` and the separate
  Desktop release endpoint.

Comparing the pinned Boomux 1.9.5 source with current `main` found no changes to
`src/client.rs`, `src/protocol.rs`, or `src/config.rs`. Attachment and daemon
internals have changed. That makes source integration promising, but does not
replace native and bundle compatibility tests.

## Proposed layout and build boundary

```text
boomux/
  Cargo.toml                     existing package plus workspace definition
  Cargo.lock                     one reviewed dependency resolution
  src/                           existing backend, CLI, TUI, web gateway
  tests/                         existing backend integration tests
  benches/                       existing backend benchmarks
  integrations/                  existing harness integrations
  assets/                        existing embedded web assets
  packaging/                     existing CLI installer and AUR packaging
  desktop/
    Cargo.toml                   boomux-desktop binary package
    src/                         existing Desktop modules
    packaging/                   launcher, .desktop entry, icon
    scripts/                     Desktop installer/package/smoke fixtures
    install.sh                   maintained Desktop installer source
    AGENTS.md                    Desktop rendering and validation rules
    IMPORT.md                    original repository and exact imported SHAs
  vendor/libghostty-vt-sys/       CPU portability patch and provenance
  docs/desktop/                  Desktop architecture, behavior, performance
  .github/workflows/ci.yml        entry point for all required checks
  .github/workflows/desktop-smoke.yml
  .github/workflows/release-please.yml
```

Keep Boomux at the root: its tests use `CARGO_BIN_EXE_boomux`, embedded assets
have relative paths, and packaging/benchmark tools assume the present layout.
Moving those too would add churn without helping consolidation.

The root workspace addition should start with:

```toml
[workspace]
members = ["desktop"]
default-members = ["."]
resolver = "3"
exclude = ["vendor/libghostty-vt-sys"]

[patch.crates-io]
libghostty-vt-sys = { path = "vendor/libghostty-vt-sys" }
```

Desktop changes its Boomux dependency to `boomux = { path = ".." }`. Keep exact
GPUI/Ghostty versions and Zig 0.15.2. Remove the Git source allowance from the
dependency policy after the Git dependency is gone. The root owns profiles,
patches, the lockfile, and shared tool/dependency policy.
[Cargo documents these workspace rules](https://doc.rust-lang.org/cargo/reference/workspaces.html).

`cargo build --locked -p boomux` continues to build without Zig or display
development libraries. `cargo build --locked -p boomux-desktop` selects the GUI.
Default package selection preserves the ordinary CLI development loop; CI must
explicitly select Desktop or use `--workspace` when it intends to check both.
Cargo may resolve the whole workspace lockfile even for a CLI-only build.

Provide a development helper that builds both executables, places the matching
CLI on the child process's PATH, and uses an isolated XDG runtime/state/config.
`cargo run -p boomux-desktop` alone does not start the daemon today. Do not change
Desktop to call the library's `connect_or_start`: that helper launches
`current_exe()` and assumes it is the Boomux CLI.

### Feasibility evidence and limits

A temporary source copy combined current Boomux, Desktop PR #10, and the
Ghostty vendor patch from PR #9. With the workspace layout above:

- Cargo resolved both packages at 1.9.7 using a local path dependency.
- `cargo metadata --locked --offline --no-deps` succeeded.
- `cargo tree --locked --offline -p boomux` contained no GPUI, Ghostty,
  Fontconfig, Wayland, or xkbcommon packages.

The two lockfiles cannot simply be concatenated. They contain conflicting
locked versions of compatible dependencies, including `thiserror` and the
`futures` family. Using Desktop's lockfile as a starting point resolved the
workspace, but replaced or removed 36 entries from Boomux's current lockfile.
Some of those entries are platform or development dependencies. Review the
actual diff, minimize unrelated changes, and rerun backend validation.

This was dependency-resolution analysis, not a migrated build or runtime test.
No source was moved in either working repository.

## One release and compatible installation paths

Retain Boomux's root Release Please configuration and `vX.Y.Z` tag sequence.
Align Desktop's package version with Boomux when importing; let Conventional
Commits select the next release. Do not reset Boomux's version to Desktop's
0.1.0 or import Desktop's release manifest/bootstrap history.

Use a single root release entry and explicit versions in both manifests.
The pinned action uses Release Please 17.6.0, whose
[Rust strategy](https://github.com/googleapis/release-please/blob/v17.6.0/src/strategies/rust.ts)
updates literal workspace members and the root lockfile. This supports the
proposed root-plus-`desktop` layout. Verify it with an offline release fixture
before relying on it: a Desktop-only feature and a backend-only fix must each
produce one proposal, update both package versions and lock entries, and create
one release. Avoid introducing independent component tags or extra release
entries. General multi-package release configurations have additional
[workspace plugin behavior](https://github.com/googleapis/release-please/blob/main/docs/manifest-releaser.md)
that is unnecessary unless the fixture demonstrates a need here.

The unified release should contain:

| Asset | Contract |
| --- | --- |
| `boomux-vX.Y.Z-x86_64-unknown-linux-gnu.tar.gz` and checksum | Preserve existing CLI updater, SSH bootstrap, and AUR consumers |
| `boomux-vX.Y.Z-aarch64-unknown-linux-gnu.tar.gz` and checksum | Preserve existing ARM64 CLI support |
| `boomux-installer.sh` | Preserve the current CLI installation/setup entry point |
| `boomux-desktop-x86_64-unknown-linux-gnu.tar.gz` and checksum | Preserve Desktop archive structure and asset name; contains both binaries |
| `boomux-desktop-installer.sh` | New canonical release-hosted Desktop installer |

Build the x86_64 CLI once with its official release provenance and reuse those
exact bytes in the Desktop bundle. Verify the CLI digest matches in both
archives. Record source SHA, version, target, artifact kind, and checksum for
each artifact; Desktop's `release.txt` records both executable versions and the
shared source SHA instead of a separate Git dependency revision.

Preserve Desktop's installed directories, `current` link, executable names,
application ID, and `~/.config/boomux-desktop/settings.toml`. Boomux's socket,
state, config, Node identity, and shell identities remain unchanged. No state or
protocol version bump follows merely from moving source files.

The two installation choices remain clear: CLI-only, or Desktop with CLI
included. Preserve an independently installed CLI as the Desktop installer does
today. Retain the launcher's bundled-PATH behavior and reuse a compatible running
daemon. An incompatible daemon requires the existing explicit lifecycle flow.

The CLI self-updater and uninstaller must continue refusing a bundle-owned
executable. Setting `BOOMUX_DISTRIBUTION=github-release` does not grant them
ownership of a Desktop installation. Test both the bundle path and its optional
`~/.local/bin/boomux` symlink; Desktop updates switch the complete bundle and do
not replace one component or restart running sessions automatically.

Move Desktop release discovery to `gardnmi/boomux`. For the official bundle,
show one dismissible application update notice rather than duplicate notices
for two components at the same version. Check that a newer release actually
contains the supported Desktop asset. Preserve saved preferences, deadlines,
bounded output, and manual checking. Separately installed CLI/development
configurations may retain their own version information, without granting
Desktop permission to replace those installations.

Keep a compatibility installer at the old Desktop URL after the first unified
release is available. It should hand off to the canonical Desktop installer and
handle unsupported old version selections explicitly. Old already-built update
checkers still point to the old repository; a README redirect alone cannot fix
them. No published Desktop release exists now, so document a one-time installer
rerun for development installations rather than maintaining a second release
stream. Preserve old tags/assets if any are published before cutover.

## CI and support after migration

Extend Boomux's current CI and exact-commit artifact reuse. Do not copy both
release workflows unchanged. Use explicit package selection for core tests,
Clippy and benchmarks so GUI toolchains stay out of core-only jobs.

| Job | Required coverage |
| --- | --- |
| Core Rust | Existing library/bin tests, serial config/native tests, Clippy |
| Integrations and web | Existing Bun reducers and embedded asset reproducibility |
| Benchmarks | Existing deterministic fixtures and benchmark smoke; preserve scheduled trends |
| Desktop Rust | Desktop Clippy/tests with Zig and graphics build dependencies |
| Dependency policy | Combined locked graph, reviewed license additions, both supported CPU architectures |
| Release candidates | Existing CLI x86_64/ARM64 builds and x86_64 Desktop bundle from one source SHA |
| Bundle smoke | Both executables together under X11/Wayland, settings validation, terminal I/O, close/reopen with the same ShellRun |
| Compatibility | Preserve pinned Arch CLI and omarchy-boomux capability checks; test both bundled executables under the CPU baseline |
| Installer/publication fixtures | Existing tests plus shared artifact provenance, complete asset set, migration paths, and ownership refusal |
| CI result | Always-run aggregate gate requiring every selected job to pass; classification failure selects full validation |

Initially run the complete product validation for executable changes. Keep the
existing safe documentation/version-only exceptions, updated for the workspace.
Fine-grained component skipping can follow after correctness; backend changes
must trigger Desktop compatibility tests because the path dependency changes
immediately. Workflow, lockfile, vendor, packaging, or toolchain changes require
all affected builds and bundle smoke coverage.

The current `classify-ci.py` recognizes only the root package version and root
lock entry for release-only reuse. Extend its fixtures and normalization to both
packages and `desktop/Cargo.toml`. A green backend-only base must never count as
proof of Desktop validation. Packaged README/license changes also require
packaging validation even though they are Markdown.

Preserve verified automatic publication from successful main CI artifacts.
Extend `ci-release-source.py`, which currently knows one CLI archive per target,
to identify Desktop separately. Extend `upload-release-assets.sh`, which
currently requires exactly five CLI assets. Update release notes, recovery
dispatch, artifact download patterns, cache keys, and publisher dependencies.
Require all expected artifacts and compatibility results before publishing the
draft. Missing/expired automatic artifacts must still fail instead of silently
rebuilding; explicit recovery must validate and smoke-test the rebuilt bundle.

The support matrix initially remains CLI x86_64/ARM64 and Desktop x86_64 on the
existing GNU/Linux baseline. Preserve X11/Wayland tests and document Omarchy's
optional Hyprland layer separately from the GPUI application. This migration
does not establish macOS, Windows, Alpine, or older-glibc support.

The CPU test in PR #9 currently emulates only Desktop. Extend it to cover the
Boomux daemon and its launch/re-execution paths as well. An outer QEMU wrapper
does not by itself prove descendant executable coverage: assert which processes
are emulated, or use a full-system VM with a fixed CPU model. Retain native
launcher tests too. Adding a full-bundle pinned Arch GUI smoke job is a sensible
follow-up to substantiate cross-distro support; a repository move alone is not
new compatibility evidence.

## Migration sequence

Implement this as one migration PR in Boomux with reviewable commits. That
avoids merging source, publishing incomplete releases, and only later fixing
the installer or release asset contract.

1. **Freeze and account for source.** Start from current Boomux `main` in a
   registered worktree under `~/Worktrees/boomux/`. Inventory Desktop branches
   and PRs; merge or explicitly port #9 and #10, recording their SHAs. Leave
   both existing working trees and Boomux's untracked artifacts intact.
2. **Import and wire the workspace.** Use an attributed snapshot import with
   `IMPORT.md`; retain the old repository for original commit history. Do not
   import old release tags into Boomux. Add the path dependency, root Ghostty
   patch, reviewed lockfile/policy, and deterministic package selection. Keep
   Desktop modules intact; a large `main.rs` refactor or client-crate extraction
   would obscure migration regressions.
3. **Adapt local tools and CI.** Fix script working directories and shared
   `target/` paths. Remove the second Boomux checkout/revision parsing. Carry
   over installer/smoke/performance tests and expand CI classifier/provenance
   fixtures. Verify core builds in a clean environment without Zig/display SDKs.
4. **Consolidate releases and user entry points.** Add Desktop assets to the
   existing release pipeline, test the one-version proposal, redirect update
   discovery, retain install ownership and settings paths, and document both
   downloads. Check repository required-status rules against the aggregate gate.
5. **Reconcile documentation and verify.** Extend Boomux's canonical glossary
   with Pane/Tile/terminal-core presentation terms. Distinguish GPUI Desktop
   from the existing Hyprland Workspace layer. Supersede Desktop ADR 0001's
   separate-repository decision while retaining daemon/UI ownership boundaries.
   Merge development, dependency, performance, and worktree guidance; remove
   stale exact-Git-pin instructions. Run the complete validation below.
6. **Cut over after a successful unified release.** Update the old Desktop
   README/installer, link or transfer remaining actionable issues, close ported
   PRs with migration references, and disable its competing release jobs.
   Verify installation through both entry points before archiving the old
   repository. Archive only as a distinct final action; keep its history and
   releases available.

Before publication, rollback is reverting the migration PR with existing
installations untouched. After publication, preserve tags/assets and ship a
forward correction; do not rewrite a published release or remove user state.
Keep the former repository available until the new installation route is proven.

## Acceptance criteria

- One fresh checkout builds both executables using a single locked graph;
  neither build checks out the other repository.
- A clean CLI-only build has no Zig, GPUI or native-Ghostty build requirement.
- Both repositories' required Rust, native, JS, benchmark, installer, and
  dependency-policy checks pass with their intended package selection.
- Desktop's layout/input/emulator tests pass; X11 and Wayland launch the exact
  candidate bundle and preserve ShellRun identity across window close/reopen.
- A compatible previous daemon and current Desktop reconnect correctly;
  graceful restart remains owner-controlled and preserves live sessions.
- CPU-baseline evidence covers both application executables, including daemon
  startup. Library checks inspect both packaged ELF files on the stated distro
  baseline.
- CLI and bundled copies for the same target have identical binary digests;
  all release metadata identifies the same commit and version.
- Release Please fixtures prove one proposal/version/tag for changes in either
  component; release-only reuse requires evidence for the complete application.
- Publication fixtures reject partial asset sets, wrong SHAs, digest conflicts,
  and missing smoke evidence; retries preserve matching assets.
- Clean install, update over the old Desktop layout, separately installed CLI,
  paths with spaces, saved dismissals/settings, failed updates, and uninstall
  ownership have coverage. Both documented installer URLs work after cutover.

Do not combine unrelated platform ports, full client/protocol crate extraction,
module cleanup, or a new automatic updater with this migration. Those can be
implemented within the consolidated repository afterward.

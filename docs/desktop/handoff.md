# Desktop source handoff

Desktop feature work is frozen in the standalone repository. New Desktop changes
belong in Boomux. The migration merged in [Boomux PR #367](https://github.com/gardnmi/boomux/pull/367).

The audited source is [Desktop main at 95946c1](https://github.com/gardnmi/boomux-desktop/tree/95946c14413eeac039e619c96368577463c59a53),
including the final merges of #9, #11, and #10. The source tree contains 54 tracked
files. The audit compared their contents with the destinations below; every
source file has an explicit destination. No Desktop PRs were open when checked
on 2026-09-06. See [IMPORT.md](../../desktop/IMPORT.md) for the full source and
Boomux revisions.

The final application changes from #10 and #11 were already present. The audit
restored the grouped weekly Dependabot configuration, retained the old changelog
as history, added Desktop privacy details to the shared security policy, and
carried over the upload-failure release regression. The temporary CPU fingerprint
workaround from an earlier #11 revision was removed before its source merge;
the final source and migration both use baseline CPU builds and cache keys.

## Differences reviewed

- Four Rust modules match exactly: generated names, layout, layout animation,
  and themes. Terminal and settings changes now add embedded agent setup and
  persist its welcome-card dismissal. `boomux_settings.rs`
  changes only comments and an internal error string to refer to the workspace.
- `main.rs` adds `--version`, formats the update card, and removes the separate
  bundled-component notice. `updates.rs` follows the shared Boomux release,
  requires Desktop assets, and offers one bundle update. Dismissal is preserved. Subsequent workspace development adds explicit whole-bundle
  Update/Restart actions and runtime diagnostics; see ADR 0016.
- Packaging, installer, and smoke adaptations use the same workspace version and
  exact CLI candidate. CPU smoke also covers the daemon and CLI helpers.
- Root CI, Release Please, publishing, configuration, lockfile, policy, and
  contributor documentation replace their standalone equivalents. Desktop's
  performance workflow lives in `desktop-performance.yml`. Publication fixtures
  cover complete assets, retry, conflicts, checksums, network errors, and refusal
  to change an already published release.
- Documentation paths and the superseded repository-separation ADR describe the
  shared workspace. The old changelog is explicitly historical; future entries
  belong to the shared root changelog.

## File inventory

Audit totals: 20 exact copies, 20 adapted files, 13 consolidated files, and
1 historical snapshot.

Paths are relative to the standalone source and the Boomux workspace respectively.

| Source | Destination | Treatment |
| --- | --- | --- |
| `.github/dependabot.yml` | `.github/dependabot.yml` | Exact copy |
| `.github/workflows/ci.yml` | `.github/workflows/ci.yml` | Consolidated |
| `.github/workflows/desktop-smoke.yml` | `.github/workflows/desktop-smoke.yml` | Adapted |
| `.github/workflows/performance.yml` | `.github/workflows/desktop-performance.yml` | Adapted |
| `.github/workflows/release-please.yml` | `.github/workflows/release-please.yml` | Consolidated |
| `.github/workflows/release.yml` | `.github/workflows/release-please.yml` | Consolidated |
| `.gitignore` | `.gitignore` | Consolidated |
| `.release-please-manifest.json` | `.release-please-manifest.json` | Consolidated |
| `AGENTS.md` | `desktop/AGENTS.md` | Adapted |
| `CHANGELOG.md` | `docs/desktop/legacy-changelog.md` | Historical snapshot |
| `CONTEXT.md` | `CONTEXT.md` | Consolidated |
| `Cargo.lock` | `Cargo.lock` | Consolidated |
| `Cargo.toml` | `desktop/Cargo.toml` | Adapted |
| `DEVELOPMENT.md` | `DEVELOPMENT.md` | Consolidated |
| `LICENSE` | `desktop/LICENSE` | Exact copy |
| `README.md` | `desktop/README.md` | Adapted |
| `SECURITY.md` | `SECURITY.md` | Consolidated |
| `deny.toml` | `deny.toml` | Consolidated |
| `docs/adr/0001-keep-the-desktop-client-separate.md` | `docs/desktop/adr/0001-keep-the-desktop-client-separate.md` | Adapted |
| `docs/architecture.md` | `docs/desktop/architecture.md` | Adapted |
| `docs/ci.md` | `docs/desktop/ci.md` | Adapted |
| `docs/performance.md` | `docs/desktop/performance.md` | Adapted |
| `docs/releases.md` | `docs/desktop/releases.md` | Adapted |
| `docs/roadmap.md` | `docs/desktop/roadmap.md` | Adapted |
| `install.sh` | `desktop/install.sh` | Adapted |
| `mise.toml` | `mise.toml` | Exact copy |
| `packaging/boomux-desktop` | `desktop/packaging/boomux-desktop` | Exact copy |
| `packaging/share/applications/org.omarchy.boomux-desktop.desktop` | `desktop/packaging/share/applications/org.omarchy.boomux-desktop.desktop` | Exact copy |
| `packaging/share/icons/hicolor/scalable/apps/org.omarchy.boomux-desktop.svg` | `desktop/packaging/share/icons/hicolor/scalable/apps/org.omarchy.boomux-desktop.svg` | Exact copy |
| `release-please-config.json` | `release-please-config.json` | Consolidated |
| `scripts/memory-report.sh` | `desktop/scripts/memory-report.sh` | Exact copy |
| `scripts/package-release.sh` | `desktop/scripts/package-release.sh` | Adapted |
| `scripts/publish-release.sh` | `.github/scripts/upload-release-assets.sh` | Consolidated |
| `scripts/smoke-desktop.py` | `desktop/scripts/smoke-desktop.py` | Adapted |
| `scripts/test-installer.py` | `desktop/scripts/test-installer.py` | Adapted |
| `scripts/test-release.py` | `.github/scripts/test_ci_release_assets.py` | Consolidated |
| `scripts/test-smoke.py` | `desktop/scripts/test-smoke.py` | Exact copy |
| `src/boomux_settings.rs` | `desktop/src/boomux_settings.rs` | Adapted |
| `src/generated_names.rs` | `desktop/src/generated_names.rs` | Exact copy |
| `src/layout.rs` | `desktop/src/layout.rs` | Exact copy |
| `src/layout_badge.rs` | `desktop/src/layout_badge.rs` | Exact copy |
| `src/main.rs` | `desktop/src/main.rs` | Adapted |
| `src/settings.rs` | `desktop/src/settings.rs` | Adapted |
| `src/terminal.rs` | `desktop/src/terminal.rs` | Adapted |
| `src/theme.rs` | `desktop/src/theme.rs` | Exact copy |
| `src/updates.rs` | `desktop/src/updates.rs` | Adapted |
| `vendor/libghostty-vt-sys/Cargo.toml` | `vendor/libghostty-vt-sys/Cargo.toml` | Exact copy |
| `vendor/libghostty-vt-sys/LICENSE` | `vendor/libghostty-vt-sys/LICENSE` | Exact copy |
| `vendor/libghostty-vt-sys/PATCH.md` | `vendor/libghostty-vt-sys/PATCH.md` | Exact copy |
| `vendor/libghostty-vt-sys/README.md` | `vendor/libghostty-vt-sys/README.md` | Exact copy |
| `vendor/libghostty-vt-sys/build.rs` | `vendor/libghostty-vt-sys/build.rs` | Exact copy |
| `vendor/libghostty-vt-sys/src/bindings.rs` | `vendor/libghostty-vt-sys/src/bindings.rs` | Exact copy |
| `vendor/libghostty-vt-sys/src/lib.rs` | `vendor/libghostty-vt-sys/src/lib.rs` | Exact copy |
| `vendor/libghostty-vt-sys/tools/gen_bindings.rs` | `vendor/libghostty-vt-sys/tools/gen_bindings.rs` | Exact copy |

## Remaining repository cutover

Both source feature PRs are already merged; no PR transfer or closure remains.
`CI result` is now required on Boomux main, alongside the seven existing checks
and strict branch freshness. Its first complete run passed before adding the
requirement. After the first
unified release and installer are verified, forward the old installer and archive
the standalone repository. Its source and review history remain available.

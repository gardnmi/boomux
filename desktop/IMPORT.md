# Desktop source provenance

Imported from https://github.com/gardnmi/boomux-desktop under its MIT license.
Original commit and review history remains in that repository.

The final Desktop source for handoff is
`95946c14413eeac039e619c96368577463c59a53` (`main`, audited 2026-09-06).
Its tree is `22405df04c8d6c1221d106c2dc6ae131cfd837bc`. All 54 tracked files
are accounted for in the [handoff inventory](../docs/desktop/handoff.md).
No Desktop PRs were open at this audit; new Desktop work belongs in Boomux.

| Source PR | Final merge | Carried into this workspace |
| --- | --- | --- |
| [#9](https://github.com/gardnmi/boomux-desktop/pull/9) | `0b4cb3cf00c9445455b03ae16141c8ee87ea1201` | Portable Ghostty build and CPU smoke coverage |
| [#11](https://github.com/gardnmi/boomux-desktop/pull/11) | `29b4e250f1d20eaebaec2d12997ec2799d4e2b20` | Distinct shared-Shell Agent labels and regression test |
| [#10](https://github.com/gardnmi/boomux-desktop/pull/10) | `95946c14413eeac039e619c96368577463c59a53` | Dismissible notices, adapted to the shared release |

The earlier PR #11 CPU fingerprint workaround was removed upstream before
merging. Both final Desktop main and this workspace use the explicit baseline
Ghostty target and `baseline-v1` cache keys. The vendored Ghostty files match the
final source exactly; `vendor/libghostty-vt-sys/PATCH.md` records upstream provenance.

The migration started from Boomux `0fd34c8` and now includes main at
`2734e8b` (release 1.9.8), including the Codex lifecycle fix.
Desktop shares Boomux's version, lockfile, CI, and release, while remaining a
presentation client of the daemon.

The shared lockfile starts from Desktop's locked dependency graph and adds the
root package's benchmark/test dependencies. Existing exact UI/terminal pins are
preserved. Some common dependencies resolve to newer compatible versions than
Boomux previously used; the root test suite and workspace feature combination
must both validate this change. Desktop enables serde_json insertion ordering
through GPUI, so backend tests must not assume sorted JSON object iteration.
The combined cargo-deny policy retains Desktop's existing ISC, CC0, and MPL
allowances; duplicate versions remain warnings as in both original repositories.

Post-import work in the migration PR adds unified installation, embedded agent
setup, runtime diagnostics, and user-driven Desktop updates. These are new
Boomux changes, not omitted source PRs; see
[ADR 0016](../docs/adr/0016-unified-installation-and-user-driven-desktop-updates.md).

# Desktop source provenance

Imported from https://github.com/gardnmi/boomux-desktop under its MIT license.
Original commit history remains in that repository.

- Source: `f149e0a` (`feat/update-notices`, PR #10), including dismissible notices.
- CPU portability patch and smoke harness: `11fcdc0` (PR #9).
- Previous Desktop main: `8c45234`.
- Boomux migration base: `0fd34c8` (version 1.9.7).

The root `vendor/libghostty-vt-sys/PATCH.md` records upstream patch provenance.
Desktop is now a workspace member and shares Boomux's version, lockfile, CI,
and release. It remains a presentation client of the daemon.

The shared lockfile starts from Desktop's locked dependency graph and adds the
root package's benchmark/test dependencies. Existing exact UI/terminal pins are
preserved. Some common dependencies resolve to newer compatible versions than
Boomux previously used; the root test suite and workspace feature combination
must both validate this change. Desktop enables serde_json insertion ordering
through GPUI, so backend tests must not assume sorted JSON object iteration.
The combined cargo-deny policy retains Desktop's existing ISC, CC0, and MPL
allowances; duplicate versions remain warnings as in both original repositories.

# Desktop source provenance

Imported from https://github.com/gardnmi/boomux-desktop under its MIT license.
Original commit history remains in that repository.

- Desktop main: `0b4cb3cf00c9445455b03ae16141c8ee87ea1201`, including
  [PR #9](https://github.com/gardnmi/boomux-desktop/pull/9), the CPU portability
  patch and smoke harness (original commit `11fcdc0`).
- [PR #10](https://github.com/gardnmi/boomux-desktop/pull/10):
  `f149e0aa7b984f52077db93e8f0fdf67a099c80d`, dismissible update notices, adapted
  to the unified Boomux release and bundle.
- [PR #11](https://github.com/gardnmi/boomux-desktop/pull/11):
  `0c9db9440a5f0650996460666af245353af803c8`. Its product change at
  `b3a2bc2adc8c406e5a604ef643db9a69e9400701` adds distinct labels for Agents
  sharing a Shell, including its regression test and documentation. This PR is
  stacked on #10; both are included here. Its later CPU cache isolation fix
  protects the older native-CPU Ghostty build on that branch. Here, PR #9's
  `-Dcpu=baseline` patch and the dedicated `baseline-v1` Desktop cache keys
  replace that workaround: cached Ghostty code uses the same portable target
  across runner CPUs, without restoring the old native-CPU cache namespace.
- Boomux migration base: `0fd34c8` (version 1.9.7).

PR #9 was merged and #10/#11 were open when checked on 2026-09-06. Their changes
are carried into this migration; their GitHub discussion and review history
remain in the source repository. After this migration merges, close the two
source PRs with a link to the replacement instead of merging the same changes
again. Recheck their heads before cutover if work continues in the old repo.

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

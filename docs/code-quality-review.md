# Code Quality And Test-Value Review

Reviewed 2026-09-07. This is a subsystem-wide source review with focused
validation, not a claim that every line or runtime configuration was exercised.
Existing uncommitted UI and integration-management work was preserved.

## Coverage And Disposition

| Area | Review focus | Disposition |
| --- | --- | --- |
| Core daemon, attachment, process adapter | Startup/handoff ownership, event and worker ordering, exact argv, bounded queues | Retained lifecycle and cleanup boundaries; no speculative service refactor |
| Protocol and persistence | Compatibility retention, migrations, bounded loading in `state_store.rs` and `node_projection.rs` | Enforced read-time caps as well as metadata size checks; kept migration and ownership coverage |
| Federation and projections | Owner-qualified identity, disposable cache bounds, stale-state presentation | Retained authority distinctions and cache compatibility; shared agent-state counting with the local summary owner |
| CLI and TUI | Generated names, dashboard state projection, retired Session paths | Removed duplicated state-count match; retained Session compatibility pending an explicit removal decision |
| Native Desktop | Module boundaries, bounded terminal mailboxes, naming, removed integration UI | Reused core naming instead of a second catalog/algorithm; left existing layout/input work intact |
| Web dashboard and terminal | Polling/terminal cleanup, timestamp formatting, app-shell tests | Reused the locale formatter; removed cosmetic/source-token assertions while retaining security and asset checks |
| Harness integrations | Process deadlines, stream cleanup, reporting authority, managed ownership and opt-outs | Fixed OpenCode lifecycle and TUI command deadlines; retained independent deployment assets and shared their regression fixtures |
| Host catalogs | Prefix bounds, file selection, legacy reachability | Did not optimize or delete legacy catalog paths without resolving compatibility/cold-recovery consumers |
| Build, packaging, CI and tests | Exact release inputs, checksum/manifest tests, test selection | Retained packaging safety tests; changed integration test commands to explicit `./` paths after reproducing the invocation difference |
| Documentation and ADRs | Removed first-run UI versus current core integration ownership | Corrected stale architecture/performance text and marked the superseded first-run portion of ADR 0016 |

## Implemented Improvements

- **One naming implementation:** Desktop now calls core's shared
  `generated_names::random_excluding`. Catalog contents, naming style, collision
  exclusion, wraparound, and exhaustion semantics are unchanged.
- **One agent-state counter:** Remote dashboard projection uses the same
  `AgentStateCounts::add` implementation as local summaries. No state or authority
  classifications changed.
- **Read-time bounds:** State and Node-cache reads stop at their existing size
  limit plus one byte and reject excess data, even if a file grows after its
  metadata was checked. Non-following/nonblocking opens avoid following a replaced
  symlink or waiting for a special file. Stored schemas are unchanged.
- **Actual command deadlines:** Both OpenCode runners race command completion
  against their deadline, force-kill the owned child on failure, cancel outstanding
  pipe reads, and release reader locks. They no longer depend on a stuck exit
  promise or inherited pipe closing to reject. Four shared regression cases cover
  never-exiting children and output overflow across the two deployed runners.
  The documentation lookup skill informed subprocess handling; its extended lookup
  was unavailable, so the [official Bun subprocess documentation](https://bun.sh/docs/runtime/child-process)
  was checked directly. No new dependency or host API feature was introduced.
- **Avoidable web allocation:** One relative-time formatter is reused across
  dashboard timestamps. The app-shell cache revision advances with the changed
  asset. No measured CPU or memory improvement is claimed.
- **Explicit test paths:** CI and development examples use `./integrations/...`.
  The unprefixed repository-root invocation exited without diagnostics locally;
  the explicit paths passed with the pinned Bun 1.3.14 executable. No test was
  removed or skipped to work around that failure.

## Test Removals And Coverage Justification

| Removed test/assertions | Previously protected | Why removed | Remaining coverage / replacement |
| --- | --- | --- | --- |
| Desktop `generated_names_use_the_boomux_style` | Adjective-noun catalog indexing | Its implementation was removed in favor of core's identical catalog | Core `generated_names_are_stable_for_injected_entropy` checks those cases and another adjective boundary |
| Desktop `generated_names_skip_collisions_and_wrap` | Collision exclusion and wraparound | Duplicated the core algorithm and exact cases | Core test of the same name, plus `exhausted_catalog_returns_none` |
| Self-equality assertion in core `stable_names_repeat_for_the_same_value` | Repeating a pure function call | Added nothing beyond the pinned expected result | The same test retains its known result and distinct-input assertions |
| 13 assertions in web `app_shell_is_installable_and_has_no_inline_code` | Exact branding text, named themes, colors, CSS dimensions/font absence, presence of `Math.min`/`Math.round` in minified JS | Cosmetic snapshots and implementation tokens do not establish runtime correctness | No equivalent cosmetic replacements; retained app-shell wiring, theme persistence, CSP, legal notices, WASM/assets, authorization and interaction checks |

No protocol compatibility, persistence migration, lifecycle authority, concurrency,
ownership, security, recovery, or cleanup test was removed. The four subprocess
regressions protect distinct failure paths rather than target a net test-count
reduction. The earlier integration-management pass's metadata-test removal and
receipt-fixture correction are separate from the removals listed above.

## Validation

- `cargo check --locked` and `cargo check -p boomux-desktop --locked`: passed.
- Focused binary tests for agent summaries, dashboard projection, generated names,
  and the web app shell: 12 passed.
- Focused library tests matching state storage, Node projection, and generated
  names: 27 passed (including matching protocol/worker tests).
- Pinned Bun 1.3.14 with explicit paths for OpenCode lifecycle, TUI claims, and Pi:
  50 passed, including the four new subprocess regressions.
- Direct before/after JavaScript comparison: 21 timestamp cases unchanged,
  including invalid values and second/minute/hour/day boundaries.
- Formatting, `git diff --check`, and shell syntax for the edited CI command:
  passed.

Full Rust/Desktop suites, release builds, live harness/GUI runs, performance
benchmarks, and live filesystem-race injection were not run. Existing focused
migration/cache tests exercised the changed persistence paths. `actionlint` was
not installed, so workflow lint remains for CI; the workflow change is limited
to explicit test-path arguments.

## Remaining Decisions And Risks

1. **Development harness isolation:** `run-dev.py` isolates Boomux's XDG state,
   but HOME-based/explicit harness configuration can still be shared. Decide
   whether development should share those accounts and integrations or use
   separate harness configuration. The current caveat is documented rather than
   silently moving credentials or changing launch behavior.
2. **Retired Session compatibility:** ADR 0014 deliberately retains protocol
   variants and persisted fields. Removing that material needs a compatibility
   floor and migration decision, not a dead-code sweep. Internal UI/catalog
   cleanup should first establish which remaining consumers need it.
3. **Legacy integration adoption:** Unknown older files without ownership receipts
   remain protected. Completely unattended adoption would need an explicitly
   trusted historical-asset policy; guessing ownership from filenames is unsafe.

No commits, PRs, installed release replacements, or changes to real harness
configuration were made by this review.

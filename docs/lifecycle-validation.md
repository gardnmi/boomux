# Lifecycle Integration Validation

> **Status: Version-specific validation record.** This is observed compatibility
> evidence for the named host and Boomux versions, not a timeless host contract.

This record separates observed host behavior from reducer fixtures and intended
semantics. Host compatibility is not inferred from process names, terminal
output, or database recency.

## 2026-08-19

OpenCode `1.18.18` was live-validated with the protocol-42 Shared Harness
Runtime. In a temporary ordinary zsh-backed Boomux Shell, bare zero-argument
interactive `opencode` resolved through the post-startup scoped shim and ran
stock `opencode attach` against the daemon-supervised server on port 4097. A
Session selected through OpenCode's TUI control API produced one claim and Agent
for the exact current ShellRun, and the Boomux web detail API returned the same
Session's exact native OpenCode Web path. The temporary Workspace was then
closed. This validates shared-runtime startup, zsh startup-file ordering, TUI
plugin loading, claim creation, and phone/desktop handoff; graceful handoff and
cold adoption remain compatibility-test evidence rather than live host
validation.

## 2026-08-18

The paired shared-runtime design targets source-visible OpenCode TUI and server
APIs at OpenCode `1.18.18`. Its TUI API path is version-gated and covered by the
repository reducer fixture. The Shared Harness Runtime, scoped shim, claim
reacquisition, and phone/desktop synchronization have not been live-validated;
this record makes no such compatibility claim.

OpenCode `1.18.18` was validated through the experimental mobile gateway's
owner-local structured conversation path. A detail request for one exact local
Agent accepted the PATH wrapper's bounded informational stdout preamble,
revalidated the exported Session ID, and returned the newest 97 normalized
entries from 370 under the serialized response budget. The response omitted the
external Session ID and rendered terminal fallback. No conversation content was
printed during the compatibility check.

A separate exact local Session produced an approximately 189 MiB complete host
export. The gateway did not weaken its 16 MiB source limit for that history and
returned the exact run-scoped terminal fallback instead. This experimental
adapter was subsequently removed in favor of handing exact local Sessions to
OpenCode's native web interface; these results remain historical compatibility
evidence only.

## 2026-08-14

OpenCode `1.18.18` was validated for exact canonical transcript reading from a
completed scheduled execution. The PATH-resolved wrapper and OpenCode CLI wrote
bounded informational preamble lines before the export JSON. Boomux accepted
the preamble, revalidated the exported session identity, and returned 13
structured entries through `session read` without exposing content during the
compatibility check. Canonical transcript reading was subsequently removed from
Boomux; this remains historical compatibility evidence only.

## 2026-08-07

The repository and installed integration assets were byte-for-byte identical.
The host CLIs reported OpenCode `1.18.15` and Pi `0.84.1`. Validation used the
installed release Boomux binary at protocol 15 and state schema 6.

### OpenCode

The following disposable managed runs used
`opencode/deepseek-v4-flash-free`. Agent observations had
`lifecycle_integration` authority and confidence 100.

| Scenario | Shell/run evidence | Agent evidence | Result |
| --- | --- | --- | --- |
| Work and idle | Shell `0113d70a`, run `830b24c2` exited 0 after printing `lifecycle-ok` | Agent `716734d9`, root `ses_02180f6c0ffehnVUiPzdTt5Qp2`, revision 3 `idle` after revision 2 `working` | Validated |
| Root/subagent aggregation | Shell `8a4b8e0f`, run `989d0c7b` visibly completed one General subagent | Exactly one Agent `8134c688` for root `ses_021803c16ffevmmRw3Ea5I1pG4`, revision 11 `idle`; no child Agent was registered | Validated |
| Permission blocker and resolution | Shell `2789827d`, run `b04e9380` used `permission.bash=ask`; noninteractive OpenCode rejected the request | Agent `5491e94a` raised revision 6 `blocked` with `OpenCode awaiting permission (1 pending)`, then revision 7 `working` with `OpenCode permission resolved` | Validated |
| Same-run session reacquisition | Shell `2a0402d2`, run `b564cd1e` invoked two OpenCode processes against one root session | One Agent `d320a1b2` for root `ses_0217c811affeKaEmOvJkW5NFMZ` advanced to revision 6; no duplicate Agent was created | Validated |
| Graceful daemon replacement | Existing interactive shell `4d587b46`, run `f4d92e18`, and Agent `91c24709` survived replacement into protocol 15 unchanged | Exact shell, run, external session, Agent ID, revision 3, and idle observation were preserved | Validated |

The blocked observation also raised protocol-15 durable attention. Its later
working observation did not erase the item, and the queue exposed the raising
revision separately from the current revision.

OpenCode does not expose foreground selection as an inactivity event. Switching
the UI therefore routes later events to their canonical roots but does not imply
that a switched-away root is inactive. A disposable `opencode session delete`
invoked from a separate CLI process did not provide a meaningful same-process
root-deletion event, so permanent `done` remains fixture-validated only. A
controlled live `session.error` also remains unvalidated.

### Pi

Pi had no authenticated model (`pi auth check --provider google` and
`pi auth check --provider openai` returned `not_ready`, and the UI reported no
available models). A disposable protocol-15 managed run still validated host
startup and shutdown behavior:

| Scenario | Shell/run evidence | Agent evidence | Result |
| --- | --- | --- | --- |
| Startup and shutdown | Shell `9194408f`, run `29057624` loaded the refactored checked-in Pi 0.84.1 asset with canonical project session `boomux-lifecycle-refactor` and exited after the expected missing-key error | Agent `17da9996` advanced from revision 1 `idle` to revision 2 `inactive` on `session_shutdown` | Validated |
| Graceful daemon replacement | Existing Pi shell `01542817`, run `595d27ee`, and Agent `a142f597` survived replacement into protocol 15 unchanged | Exact shell, run, external session, Agent ID, revision 1, and idle observation were preserved | Validated |

The Pi 0.84.1 installed declarations and runtime implementation define and await
`session_start`, `agent_start`, `agent_end`, `agent_settled`, and
`session_shutdown`; reload and session replacement send shutdown before start.
One focused unit test routes the documented ordering through the handlers using
only fields Boomux consumes. Separate lifecycle tests verify same-session reuse,
old-session inactivity, new-session identity, settled terminal errors, and quit
inactivity. These are implementation tests, not host conformance tests.

Model-driven `working -> idle`, final provider-error blocking, interactive
`/reload`, and `/resume` remain unvalidated against a live provider. They must
not be represented as live guarantees until an authenticated disposable model
is available.

### Automated Coverage

The validation run was followed by:

```console
bun test integrations/opencode/boomux.test.js
bun test integrations/opencode/boomux-tui.test.js
bun test integrations/pi/boomux.test.js
cargo test
cargo clippy --all-targets --all-features -- -D warnings
```

Reducer tests remain compatibility evidence, not substitutes for the live cases
listed above. Future host-version bumps should append a dated record rather than
silently reusing this one.

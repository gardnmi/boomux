# Lifecycle Integration Validation

> **Status: Version-specific validation record.** This is observed compatibility
> evidence for the named host and Boomux versions, not a timeless host contract.

This record separates observed host behavior from reducer fixtures and intended
semantics. Host compatibility is not inferred from process names, terminal
output, or database recency.

## 2026-08-31

Pi Coding Agent `0.84.4` documentation and the installed session store were
inspected for title compatibility. Pi JSONL files identify a Session in their
`session` header with exact `id` and `cwd`; later `session_info` entries carry the
user-defined `name`. Repository tests validate direct-file and symlink-safe
bounded discovery, exact ID and normalized-directory matching, newest valid name
selection, the existing bounded first-user-message fallback, and title-only
projection. Native and remote-owner tests prove that a matching record enriches
an observed Pi Session while unmatched records create no historical Session.
This establishes local title compatibility only and adds no Pi catalog,
lifecycle authority, or completion claim.

Claude Code `2.1.251` was inspected for title compatibility. Claude documents
Session naming and exact resume but no title-list API; this adapter therefore
uses version-validated, fail-open compatibility with direct project JSONL
transcripts. Observed `ai-title` records include `aiTitle` and `sessionId`, while
transcripts carry the exact cwd and use the Session ID as their filename stem.
Repository tests validate Claude's encoded project-directory mapping, direct
regular-file and symlink-safe bounded discovery, exact filename/session/cwd
agreement, latest valid title selection, sanitization, and title-only projection.
Transcript messages are never used to invent a title, and unmatched records
create no historical Session. This establishes local title compatibility only;
it is not a documented Claude catalog contract and adds no lifecycle, completion,
or Remote Control authority.

Kiro CLI `2.19.1` was inspected with its documented
`kiro-cli chat --list-sessions --format json` interface in an existing Workspace
directory. The JSON response initially associated the exact canonical Session ID
`sess_8e787d45-9429-4822-b6fe-2621bcfac4f1` with `Triage Slack issue thread`.
After later Kiro activity, the same interface and a rebuilt live Boomux daemon
both returned the updated title `Ticket 300765 complete`, while preserving that
same external ID and Boomux lifecycle occurrence. Repository tests validate the
exact command vector, normalized-directory matching, bounded parsing, title
sanitization, malformed and oversized fail-open behavior, and exact-ID merge.
They also prove that an unmatched Kiro listing record creates no historical
Session and that exact durable inspection does not wait for host title discovery.
This establishes local title compatibility only; it adds no cloud or historical
catalog projection, lifecycle authority, completion claim, or native web
handoff.

## 2026-08-25

Kiro CLI `2.18.0` was live-validated in a Boomux-managed ShellRun with the
installed standalone v3 hook asset. Submitting `hi` produced authoritative
`UserPromptSubmit` Working evidence for one canonical Session; after Kiro
rendered its final assistant response and returned to the input prompt, the same
Session produced a `Stop` event. Current Kiro v3 documentation defines Stop as
firing when the agent has completed its turn and finished responding. Boomux
therefore treats Stop as Idle turn completion while retaining final holder
release as Inactive and making no Blocked or Done claim.

## 2026-08-24

Hyprland `0.56.2` was live-validated with the special-Workspace desktop adapter,
which defaulted on in that build, and the Omarchy Quattro Boomux bar plugin
`0.24.0`. An existing
identity-marked terminal was presented in the coordinator-derived named special
Workspace while ordinary Workspaces retained the configured `scrolling` layout;
`hyprctl -j workspaces` reported `tiledLayout: "dwindle"` for the Boomux special
Workspace. Hyprland accepted the exact runtime `hl.workspace_rule` expression,
reported the focused monitor's special Workspace identity, and loaded the
contextual keybindings without configuration errors.

A disposable local Shell was then created through `boomux desktop terminal`.
Hyprland reported its exact initial-title Node/Shell identity and stable window
ID in the Boomux special Workspace, and `boomux desktop close` removed that
exact focused Shell. Repository coverage additionally validates delayed
concurrent window correlation, stale launch-token cleanup, canonical identity
parsing, exact-window close revalidation, singleton cycling, and launcher-free
desktop presentation. Multi-monitor bar placement and remote contextual close
remain test evidence rather than live validation.

The same stable terminal window was moved from its Boomux special Workspace to
ordinary Workspace 2, then returned with `boomux desktop return`. Hyprland
reported the unchanged address, stable window ID, initial-title identity, and
coordinator-derived special Workspace after the move; no terminal or Shell was
created, opened, restarted, or taken over.

`boomux desktop gather` subsequently placed every available local terminal
attachment for the selected `boomux` Workspace into its special Workspace and
reported the unavailable unregistered remote placement as a partial failure.
The focused `agent_boom` terminal was then moved to ordinary Workspace 2 and
returned again with its address and stable window ID unchanged. Plugin `0.24.0`
loaded without QML errors and correlated the active identity-marked terminal
with its Workspace and Shell models.

From ordinary Workspace 1, an exact Agent Shell open with its coordinated
Workspace ID showed the owning Boomux special Workspace and reused the existing
terminal with unchanged address, stable window ID, and PID. No sibling Shell or
Workspace launcher was opened. The plugin generates this placement-aware open
only when the CLI advertises `coordinated_shell_desktop_placement`.

From ordinary Workspace 1, `workspace open boomux --show` revealed the
coordinator-derived special Workspace before attempting every available item.
The existing `agent_boom` terminal remained in that layer, and the unavailable
remote placement was reported without suppressing the local restore. Plugin
`0.24.0` loaded without QML errors; its persistent Workspace and Shell labels
target the exact grouped Workspace model used by the Workspaces panel.

The rebuilt native TUI was then launched in ordinary Workspace 1. Enter on the
selected `agent_boom` item revealed its owning `boomux` special Workspace, and
Enter on the `boomux` Workspace did the same before the full restore attempt.
Repository coverage verifies that a restored local item plus an unavailable
remote operation becomes a successful warning message rather than the red
all-or-nothing error previously rendered in the footer.
After navigation semantics were enabled, the test TUI window was moved to
ordinary Workspace 1 and Enter was repeated on `boomux`; Hyprland showed the
coordinator-derived special Workspace and the TUI window exited, leaving focus
in the restored Workspace.

## 2026-08-20

Kiro CLI `2.18.0` was inspected with its opt-in v3 harness (`kiro-cli --v3`).
The documented standalone hook schema and installed command help establish the
five consumed lifecycle events, canonical `session_id`, exact resume command,
and headless dispatch surface. Repository tests validate bounded hook decoding,
run-scoped authority, bare v3 launch selection, exact managed and recovery argv,
dedicated hook installation, and safe ambiguity handling. This is fixture and
interface compatibility evidence only: no live authenticated Kiro turn,
permission wait, cloud Session, or Kiro Web handoff was exercised. No Blocked,
Idle, Inactive, Done, cloud lifecycle, catalog, or native web capability is
claimed. Kiro documents isolated sub-agent hook execution but no root-execution
field in standalone SessionStart or Stop payloads, so fixtures reduce those
ambiguous boundaries to Unknown. The Stop mapping was superseded by the
2026-08-25 live validation and current v3 hook contract above.

Codex CLI `0.147.0` was inspected against the documented hooks and experimental
app-server interfaces. Repository tests validate bounded hook decoding,
run-scoped authority, lifecycle reduction, exact managed and recovery argv,
merged hook installation, and catalog projection. This is fixture and interface
compatibility evidence only: no live authenticated Codex turn, permission wait,
hook trust flow, or app-server catalog was exercised. No Codex Remote handoff is
claimed because the host does not document an exact thread-specific Remote URL.

Claude Code `2.1.236` was live-validated with the protocol-43 lifecycle plugin
and Remote Control binding. After the skills-directory plugin was installed and
Claude was restarted inside an existing managed Bash ShellRun, Claude's
canonical Session ID produced one exact current Agent at
`lifecycle_integration` authority and confidence 100. Session start, one user
prompt, and the completed turn advanced the observation to revision 3 `idle`;
`boomux integration verify claude --shell ID --json` reported the integration
as verified with one tracked and zero untracked processes.

The same Claude process enabled Remote Control in-session. A later hook observed
the ephemeral bridge identity, and the owner-local Boomux web snapshot exposed
an **Open in Claude** native handoff only on that exact current Agent. The
gateway then published its dashboard and OpenCode routes through Tailscale, and
`tailscale serve status --json` confirmed both owner-managed HTTPS proxies.
The bridge identity itself was not copied into this record. Default-on bare
launch injection, SessionEnd cleanup, cold absence, and graceful binding
transfer remain compatibility-test evidence rather than live host validation.

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
completed managed Agent run. The PATH-resolved wrapper and OpenCode CLI wrote
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

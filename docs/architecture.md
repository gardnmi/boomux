# Architecture

> **Status: Current reference.** This document describes the implemented
> architecture. `CONTEXT.md` is authoritative for domain terminology; source and
> compatibility tests are authoritative for exact protocol and state versions.

## Module Ownership

| Path | Responsibility |
| --- | --- |
| `src/main.rs` | CLI schema, process composition, command dispatch, and dashboard backend orchestration |
| `src/dashboard_projection.rs` | Typed snapshot/session-to-dashboard classification, view construction, and title enrichment |
| `src/protocol.rs` | Versioned control and attachment wire models, framing, and request version requirements |
| `src/client.rs` | Daemon discovery/startup, protocol negotiation, typed management requests, and attachment setup |
| `src/daemon.rs` | `DaemonService` coordination over durable registry, event-stream, shell-runtime, persistence, and handoff owners |
| `src/state_store.rs` | Versioned durable schemas, validation, atomic state storage, and migrations |
| `src/node_identity.rs` | Stable Node identity persistence, federation admission leases, and bounded rekey drain |
| `src/node_registration.rs` | Independently versioned remote Node registrations, identity pinning, admission drain, validation, and atomic storage |
| `src/federation.rs` | Independently versioned federation handshake and verified stdio daemon bridging |
| `src/ssh_bootstrap.rs` | Validated SSH targets, private invocation configuration, deadline-bound remote discovery, and helper compatibility selection |
| `src/handoff.rs`, `src/fd_transfer.rs` | Graceful daemon replacement records and Unix descriptor transfer |
| `src/attach.rs` | Terminal-side raw mode, control frames, live input/output, resize, focus, and reconnect handling |
| `src/terminal.rs` | Selection and launch of native terminal windows through `xdg-terminal-exec` |
| `src/terminal_state.rs` | Shadow VT parsing, bounded reconstruction, logical output, and structured previews |
| `src/terminal_focus.rs` | Stateful parsing and restoration of child focus-reporting mode |
| `src/tui.rs` | Dashboard state, interaction, palette, polling, and Ratatui rendering; no direct daemon transport |
| `src/session_projection.rs` | Projection of daemon Agent state and host catalogs into client-visible sessions |
| `src/integrations.rs` | Integration identity, display metadata, and optional installation, title/catalog, resume, and foreground capabilities |
| `src/host_session_titles.rs` and children | Shared title/catalog policy and host-specific discovery adapters |
| `src/host_session_source.rs` and children | Canonical host source paths, normalization, and secure source lookup |
| `src/integration_management.rs` | Integration inventory, status, setup, verification, install, and uninstall workflows |
| `src/process_adapter.rs` | Exact-argv child supervision and fail-open process-bound Agent observation |
| `src/scheduling.rs` | Bounded canonical cron parsing and occurrence evaluation, IANA timezone and DST policy, prompt bounds, and schedule identity validation |
| `src/config.rs`, `src/projects.rs`, `src/git.rs` | Layered configuration, bounded project discovery, and asynchronous Git metadata |
| `src/cli_output.rs` | Stable `boomux.cli/v1` output and error presentation |
| `src/desktop_notifications.rs` | Bounded fail-open desktop and sound delivery |

## Invariant Index

- **Durable identity and lifecycle:** `CONTEXT.md` and the Protocol section below.
  Primary coverage lives in protocol, daemon, state-store, and native-backend
  tests.
- **Persistence before event publication:** Transition Coordinator below and
  [`event-stream.md`](event-stream.md).
- **Single PTY reader during replacement:**
  [`live-pty-handoff.md`](live-pty-handoff.md), enforced by handoff and native
  replacement tests.
- **Exact run binding and Agent authority:** Agent runtime sections below and
  `CONTEXT.md`; process exit never implies Agent completion.
- **Ephemeral attachment environment:** Daemon and Runtime Semantics sections
  below; startup environment is validated but never persisted or projected.
- **Exact argument vectors:** workspace launchers, process adapters, terminal
  launch, and integration management execute argv directly without shell
  interpretation.
- **Scheduled Agent authority:** [`scheduled-agent-work.md`](scheduled-agent-work.md)
  defines the boundary between manual and timed dispatch, concurrency policy,
  process outcome, and authoritative Agent lifecycle.
- **Remote Node authority:** [`remote-nodes.md`](remote-nodes.md) defines the
  accepted federation boundary: one owning Node remains authoritative, SSH is a
  route, and local cached projections never authorize mutation or lifecycle
  inference. Ad hoc bootstrap and verified transport are implemented; durable
  durable registration is implemented; projection and routed management remain
  tracked by #173.

## Product Boundary

Boomux is a native-terminal session manager, not a terminal emulator or an
embedded multiplexer UI. Each Boomux shell is rendered by one ordinary terminal
window selected through `xdg-terminal-exec`.

```text
terminal emulator
  -> boomux attachment client
  -> Unix socket
  -> Boomux daemon
  -> PTY
  -> child process
```

The terminal emulator remains responsible for rendering, fonts, themes,
selection, clipboard integration, and window behavior. Boomux provides process
persistence across attachment disconnects, naming, grouping, and orchestration.

### Accepted Federation Boundary

Remote Node federation extends this product boundary incrementally. A remote
daemon, not a local SSH process, owns its remote PTYs,
processes, Workspaces, and runtime identities. The local daemon can later retain
a bounded prompt-free projection for its TUI, CLI integrations, and desktop
presentation, but that projection remains separate from the authoritative local
registry and becomes visibly stale when its owner cannot be reached.

The transport follows the existing client/server split: an authenticated fixed
SSH stdio helper verifies a stable remote Node identity, then carries one
ordinary negotiated daemon protocol stream to the remote Unix socket. It does
not expose a TCP listener or the local daemon socket. Remote work continues when
the SSH bridge or local presentation disconnects. The complete accepted contract
and deferred behavior are in [`remote-nodes.md`](remote-nodes.md).

Public `boomux --remote TARGET` performs ad hoc bootstrap only. It
discovers and verifies a compatible helper, or interactively installs one before
opening and pinging a daemon-bound stdio channel. Explicit `boomux node add
ALIAS TARGET` and revision-conditional registration management persist an
identity-pinned route separately; neither mode creates a projection or routes
dashboard or resource-management operations yet.

## Components

### Application

`src/main.rs` owns the CLI, project-name suggestions, dashboard actions, shell
name resolution, and dashboard backend orchestration. `src/dashboard_projection.rs`
converts daemon snapshots and projected sessions into typed TUI view models.
`src/session_projection.rs` is the shared binary projection used by the CLI and
dashboard to combine one daemon snapshot with bounded host session catalogs.
Omitted Shell and Agent Instance names are resolved here to random lowercase
`adjective-noun` values before requests are sent. Generated shell names are
checked against the workspace snapshot and retried on a typed daemon collision;
the resulting concrete names use the ordinary protocol and durable state fields.

### Protocol

`src/protocol.rs` defines the versioned wire model and the typed protocol-feature
registry. Each feature owns its minimum negotiated version and stable capability
names. Request gating and client feature checks refer to that registry rather
than repeating numeric versions. Control messages are JSON with a four-byte
big-endian length prefix. Attachment traffic uses small binary frames for input,
output, resize, and detach events. Old-peer response transforms remain isolated
in the daemon compatibility boundary and use the same feature registry when
deciding which fields, values, and events to downgrade.

The implemented protocol has seven durable identities beneath its implicit
local Node. Federation will qualify each existing identity with a stable owning
Node without rewriting the inner ID:

- A workspace is a globally named shell container with a UUID and an optional
  default working directory for newly created shells. The default is creation
  behavior, not workspace identity; individual shells may use other paths.
- A shell is a durable process slot with a name, startup command, explicit
  working directory, and workspace ID.
- A shell run identifies one process incarnation beneath that durable shell. It
  owns the PTY and child while live and carries a generation, lifecycle
  timestamps, exit reason, and output revision.
  Runs created by Boomux export `BOOMUX_RUN_ID`; a process imported from a
  legacy daemon is marked when its existing environment lacks that variable.
- A workspace launcher is a named, ordered, exact argument-vector command with
  its own working directory. Its identity is durable, but each detached
  invocation is ephemeral and has no PTY or retained runtime state.
- An agent instance identifies one external agent session and is bound to
  exactly one shell run. It owns no process or PTY. Its latest explicit
  observation records state, reporting authority, evidence, confidence,
  revision, and time; completion is terminal and durable.
- An Agent Schedule is a durable recurring-work definition owned by one
  Workspace. It owns its trigger and prompt revisions but no process or Agent
  lifecycle state.
- A Scheduled Execution is one durable manual or timed decision bound to exact
  Schedule revisions. It can later link a ShellRun and Agent Instance without
  merging those identities.

There are no separate tab, pane, and terminal identity layers.

### Client

`src/client.rs` resolves the socket at
`$XDG_RUNTIME_DIR/boomux/daemon.sock`. It starts a detached daemon on demand,
waits for the protocol ping to succeed, and exposes typed management requests.
Its public operations return `ClientError`, which keeps transport, protocol,
remote daemon, local validation, and lifecycle failures structurally distinct.
Protocol negotiation uses typed mismatch and unsupported-version failures rather
than inspecting error messages, and remote errors retain their protocol code
without passing through `io::Error`.
An owner-held file lock prevents concurrent daemons from unlinking each other's
sockets or splitting the registry.

Explicit workspace open is client-side orchestration. The client invokes each
workspace launcher in creation order using its own desktop environment, then
opens native terminal windows for the workspace shells. Launcher processes are
detached into their own sessions and reaped while the invoking client remains
alive, but Boomux does not retain or manage their runtime lifecycle.

### Daemon

`src/daemon.rs` owns all PTY masters and child processes. Its runtime directory
is restricted to the current user and the socket mode is `0600`.

The daemon is composed from state-owning services rather than one shared
registry. `DurableRegistry` owns workspace, shell, launcher, and Agent
collections, their invariants, mutation-specific undo, and persistence
projection. Undo records retain complete affected entities or complete mutable
state rather than mirroring the registry shape, so unrelated mutations do not
clone the registry and new entity fields remain part of rollback automatically.
`EventStream` owns retained events, cursors, long-poll wakeups, and
the transition frontier that orders durable and runtime publication.
`ShellRuntimeManager` owns daemon-wide runtime stopping and focus policy and
operates only on supplied shell/runtime handles. `DaemonService` owns request
dispatch and coordinates transactions that cross those owners. Durable
transactions acquire the mutation gate, persistence gate, `EventStream`
transition frontier, retained event state, durable collection, then applicable
shell/runtime locks. Paths that need only a suffix of that order start at the
first required owner; PTY output releases runtime locks before entering the
`EventStream` publication boundary.

Request handling uses `DaemonError` to retain validation, lifecycle, persistence,
protocol, and internal failure classes until the wire boundary. Stable protocol
codes are selected from those variants directly; transport errors remain on the
connection path and are not reinterpreted as domain failures through
`io::Error` downcasting.

The daemon supports:

- Empty or explicitly populated workspace creation with an optional shell cwd
  default
- Atomic shell creation with an implicit `workspace-N` container when no
  workspace is selected
- Additional shell creation
- Ordered workspace launcher creation, inspection, rename, and removal
- Workspace and shell snapshots
- Shell and workspace rename operations
- Shell and workspace closure
- Bounded VT state and sanitized reconnect reconstruction
- One writable attachment with explicit takeover
- PTY input and resize forwarding
- Pending shell metadata and first-attachment terminal negotiation
- Run-scoped agent registration, idempotent ensure, inspection, and explicit
  state reports

An empty shell specification list remains empty. When an explicit populated
creation is requested, the daemon stages every child before publishing any of
them; a failed spawn kills the staged children. Workspace names are checked for
global uniqueness at publication while the registry is locked.

Shell creation records metadata without immediately starting a process. The
first attachment supplies its ephemeral Unix environment, `TERM`, `COLORTERM`,
terminal program identity, and cell/pixel dimensions; the daemon then creates
the PTY and child. The environment is validated, never persisted or projected,
and is overridden by authoritative terminal-profile and Boomux identity values. Failed startup
leaves the shell pending and retryable. Shell creation may omit a workspace ID.
The daemon then selects the lowest
available `workspace-N` name and publishes the generated workspace and shell as
one operation. Concurrent requests retry name allocation rather than exposing
an ungrouped shell.

### Attachment

`src/attach.rs` runs inside the selected terminal emulator. It enables raw mode,
reports dimensions, and copies bytes in both directions without transforming
live PTY output. Protocol-18 attachments also enable xterm focus reporting and
recognize focus-gained input. Physical focus events are forwarded to the PTY
only while the child has requested focus mode; otherwise they are consumed so
ordinary shells do not receive synthetic escape input. Child mode changes are
tracked across output chunks and reconstruction while Boomux keeps physical
reporting enabled. Each focus gain is also reported through the
controller-authorized attach stream. RAII cleanup restores terminal and
focus-reporting modes when the attachment exits.

The daemon keeps a bounded output queue per active controller. A slow client
drops output rather than blocking the PTY reader and child process.
The listener admits at most 64 concurrent connection handlers and closes newly
accepted sockets while that capacity is exhausted. Management responses and
attachment output use bounded write deadlines. Attachments retain one admission
slot for their lifetime, and their input handler joins the output worker after
either side closes the shared socket, so abandoned clients cannot delay shutdown
indefinitely.
It also feeds a shadow `vt100` parser while forwarding the original PTY bytes
unchanged. Reattachment receives a bounded reconstruction of rendered state,
not historical OSC or graphics commands. Plain reads and structured previews
clone the bounded shadow screen under the per-shell terminal lock, then format
that snapshot after releasing the lock. They traverse physical rows from newest
to oldest and stop once the requested byte, logical-line, and span bounds are
satisfied, so retained history does not extend PTY-writer lock hold time.

### Terminal Launcher

`src/terminal.rs` uses Omarchy's `xdg-terminal-exec` metadata to launch:

```console
boomux __attach <shell-id> --takeover --restart-exited
```

Scheduled Execution opens instead launch `__attach` with the selected exact run
ID and without `--restart-exited`. Protocol 26 carries that expected run through
the attachment handshake. While holding the ordinary attachment mutation and
shell lifecycle boundary, the daemon returns `run_changed` unless the shell is
currently running that exact run. It cannot restart or take over a later run.
Ordinary shell attachments retain their existing restart behavior.

No emulator-specific adapter or compositor window ID is required.
Spawned terminal windows start in independent process sessions with null
standard streams, so exiting the dashboard cannot close their attachments.
The internal attachment process restarts an exited shell only after the terminal
window has spawned, preserving retained output when terminal preparation fails.

### Dashboard

`src/tui.rs` remains a control plane. Its typed model update boundary consumes
dashboard events and returns explicit external effects. The terminal runtime
executes those effects through one backend interface and feeds typed completion
events back into the model; model transitions do not call daemon or terminal
callbacks directly. Rendering remains a function of typed model state. One
daemon snapshot contains each workspace, its launchers, and its shells, avoiding
races between separate list operations. Configured project roots provide
workspace suggestions; selecting one persists its canonical path as the
workspace's default cwd. Git information is still collected independently from
shell directories and cached. A default cwd does not create workspace-level Git
identity, and mixed-directory workspaces remain valid.

The dashboard establishes an atomic event-stream baseline and treats later
events as invalidation signals for authoritative snapshot reprojection. It also
preserves complete Scheduled Execution event payloads in one client-side cache
bounded to 1,000 records. The cache is seeded once with a protocol-25 global
page only when Scheduled Execution Observation is supported. Protocol-23 and
protocol-24 dashboards render scheduling as unsupported and never request their
uncapped execution history. Complete execution-created and execution-changed
records replace cached records only at a higher durable revision; stale and
duplicate revisions are ignored, and schedule removal clears all matching
records. Cursor expiration, stream replacement, and explicit refresh reseed
once. A failed reseed preserves the prior cache, keeps a retry requirement, and
retries before the next event check. Idle checks and unrelated events never
list executions. Selecting an unscoped schedule automatically replaces that
schedule's cache entries with one bounded exact-scope page and retains its
truncation metadata; a scoped selection does not repeat the read. Idle
checks advance only the event cursor. Once per second, event-stream dashboards
refresh one authoritative snapshot while retaining the advanced cursor. This
keeps ephemeral focus and foreground-process hints current without serial
per-shell client requests. The daemon caches foreground inspection per shell run
for one second, so concurrent dashboards reuse the result. Cursor expiration or
cold daemon replacement establishes a new baseline and resets client-side focus
revision tracking when the stream identity changes. Protocol-6 dashboards use a
one-second snapshot fallback for all state because that version predates the
event stream.

Shell snapshots include their additive stored startup argument vector. The
dashboard presents an empty vector as `shell` and a non-empty vector as
`command`, making primary-process exit behavior visible without splitting the
durable shell model. Command rows show the stored argv in their detail column.
Agent presentation takes precedence when the current run has an active Agent or
an exact `opencode` or `pi` foreground hint.

The daemon retains the latest controller-authorized terminal focus gain as
ephemeral workspace, shell, run, and monotonic revision metadata. With
`dashboard.follow_focused_terminal` enabled (the default), the dashboard uses a
new revision as a one-shot selection trigger and resolves both shell and Agent
rows by durable shell ID. Repeated snapshot refreshes do not enforce the
selection, so manual navigation remains usable until another terminal focus
gain. Focus changes are deferred while an overlay or close confirmation is
active. The setting is dashboard-local and disabling it does not stop focus
reporting by attachments.

Selected-kind previews remain read-only. Workspace, launcher, and run metadata
come from the polled snapshot. Shell output uses a bounded plain-text read only
when the selected shell, run ID, or output revision changes. Its viewport follows
the tail by default and preserves the viewed rows when new output arrives while
scrolled. The dashboard omits a contextual preview rather than rendering a
partial panel when the full preview and a usable item table cannot both fit.
Command previews expose argv and run metadata without reading terminal output.
Launcher previews never imply retained invocation state because launcher
processes remain ephemeral.

Schedules have a specialized fourth top-level view and a typed definition row in
their owning workspace. The workspace row has `KIND schedule`, counts as an item
but not a process, and navigates to the exact specialized schedule; it never
represents or opens an execution shell. Typed schedule and execution projections
show friendly triggers, next occurrences, last outcomes, state, scheduler health,
and bounded history. Schedule-owned execution shells are excluded from ordinary
workspace and shell presentation, process counts, restore, and actions. Their
exact linked Agents remain selectable in the Agents view but
expose no ordinary shell actions. The schedule view retains a selected execution
ID across refresh and reorder and always renders it in a selected-containing
focusable history pane. Open and cancellation
actions use that exact selection. Executions are omitted from the
command palette; actionable schedule notices navigate to their exact selection.
The schedule and history panes are side by side at normal widths and stack
vertically below the responsive breakpoint; there is no separate metadata panel.
Protocol-27 dashboards open a private built-in editor only after exact schedule
inspection. It edits name, prompt, trigger preset or custom cron, and timezone;
the timezone control searches the bundled IANA database and can select only a
valid name. Saves carry the inspected revision, failures retain the unsaved private
buffer, and save or cancel drops it from dashboard state. Ordinary projections,
messages, palette entries, events, and diagnostics remain prompt-free.
Opening a Starting or Active record requires exact shell and run IDs, re-fetches
and validates the execution and ownership before terminal launch, then uses the
protocol-26 exact-run attachment handshake to close the post-launch race.
Opening a terminal record freshly resolves its opaque canonical Agent Session
ID, constructs the integration's exact interactive resume argv, and launches it
in an unmanaged native terminal at the retained working directory. It does not
create an ordinary workspace shell, restart the shared schedule-owned shell, or
accept current managed or permanently Done sessions.
Protocol-25 dashboards retain schedule controls and bounded history but disable
exact terminal Open with upgrade-and-restart guidance.
Cancellation requires confirmation and re-fetches the exact execution before
mutation. Active blocked work attaches to its exact run; terminal work resumes
only its exact canonical session. Canonical session links are derived from exact
Agent occurrences, never latest or nearby identities. Boomux does not read or
project host transcript and tool content.
Protocol 25 has no skip-next action, and the dashboard does not emulate one by
pausing and resuming.

Agent sessions are a client-side projection, not a sixth durable daemon
identity. The projection groups stored Agent instances by workspace,
integration, and external session ID, while isolating instances without an
external ID. It retains original shell/run identity and observations even when a
shell no longer exists. UUID v5 IDs use a fixed namespace and a versioned,
length-prefixed encoding of workspace ID, integration, and the external-or-agent
grouping identity. IDs are globally unique and deterministic but opaque to
consumers. Bounded OpenCode root-session catalogs add historical, `unknown`
sessions without fabricating Agent occurrences and merge with a later durable
registration under the same stable ID. Catalog records associate to each
workspace that references their exact normalized directory. The dashboard maps
the active or latest exact match into a durable Agent's contextual preview and
discovers catalogs asynchronously; session CLI listing performs the same bounded
discovery synchronously. Sessions are not a dashboard kind.
The integration descriptor registry is the authority for integration keys,
display names, and optional typed capabilities. A title capability selects its
host adapter and independently declares catalog support. The shared title layer
owns asynchronous cache, refresh, deduplication, sanitization, and fallback
policy; OpenCode and Pi modules own host command execution and title extraction.
Neutral host source modules own shared path normalization and secure catalog
discovery. Title and catalog support remain independent from installation,
foreground recognition, and recovery eligibility, so a future harness can
implement only the capabilities it provides.

Session list/inspect requires a negotiated protocol-12 snapshot because the
projection depends on that complete Agent state model. Protocol 13 adds an
optional Agent `cwd` snapshot field, captured authoritatively from the bound
shell during registration and persisted with the Agent. Projection exposes it as
`source_cwd` separately from retained-shell metadata, retaining exact session
context for interactive resume after shell removal without claiming that the
shell remains openable. Protocol 12 remains usable while a matching shell is retained.

An active Agent instance bound to a shell's exact current run decorates that
shell as an agent-shell row rather than adding a second item. The row retains
the shell's durable identity, name, directory, Git context, and open, rename, and
close actions while displaying Agent lifecycle state and evidence. If multiple
active instances match one run, the most recently observed instance is shown
with an Agent-ID tie-break. Completed, stale-run, and orphaned Agent records
remain available through CLI inspection but do not occupy dashboard rows. A
running shell snapshot may also expose its PTY foreground process name. The
dashboard recognizes exact `opencode` and `pi` as presentation-only agent-shell hints
before a canonical Agent session exists; this hint creates no AgentInstance,
durable state observation, persistence, or events. It displays `untracked` until
lifecycle data exists, then yields to that authoritative observation. `doctor`
checks installed integration assets and reports a running untracked host with
explicit install or restart guidance.

### Agent Skill

The optional vendor-neutral `boomux` Agent Skill documents the complete public
CLI for compatible clients, including discovery, inspection, output reads,
lifecycle operations, native-terminal opening, and daemon management.
`BOOMUX_SHELL_ID` provides current-shell context while exact shell IDs remain
globally addressable within the daemon. The installer safely removes an
untouched legacy `boomux-shells` skill and preserves customized copies.

Read-only CLI integrations use the separate `boomux.cli/v1` JSON envelope rather
than serializing daemon protocol snapshots directly. `boomux capabilities`
advertises supported commands, features, schemas, and error codes without
requiring a daemon. Protocol 6 error responses carry an additive optional code;
clients expose it as `ClientError::Remote(RemoteError)`, while mixed-version
peers retain message compatibility.

### Scheduled Agent Work

Protocol 22 and state schema 9 implement the Agent Schedule identity without
changing ShellRun, Agent Instance, or projected Agent Session semantics. A
schedule belongs to exactly one workspace and snapshots a bounded prompt
revision, explicit working directory, integration, session policy, canonical
five-field cron expression, IANA timezone, and overlap policy. Create, list,
exact inspect, pause, resume, remove, cold recovery, graceful handoff, workspace
closure, and prompt-free events are available. Exact inspection is the only
management response that contains prompt content; protocol-21 and older peers
omit schedule summaries and events while their cursors still advance.

Protocol 23 and state schema 10 add manual Scheduled Execution dispatch. A
prompt-free public record retains one durable claim against exact schedule,
prompt, and trigger revisions, while its exact prompt snapshot remains private
durable dispatch input. The first execution lazily creates one schedule-owned
durable shell; later executions reuse it with distinct ShellRuns. Ordinary shell
or workspace open never starts or restarts that shell, but an active run remains
attachable. Rename, close, and restart are rejected outside schedule or workspace
ownership. State schema 10 explicitly migrates schema-9 shells to user ownership
and schedules to empty execution histories.

The shell stores only the Boomux executable, hidden runner command, and exact
schedule ID. The runner resolves the exact schedule, `BOOMUX_SHELL_ID`, and
`BOOMUX_RUN_ID` claim using a private per-execution capability and invokes
integration-owned argv builders without shell interpretation. The capability is
persisted with private dispatch input, supplied only to that runner's ephemeral
environment, removed before the external host is spawned, and required for claim
resolution and outcome reports. OpenCode
uses `opencode run [--session exact-id] -- prompt`.
Pi uses `pi [--session exact-full-id] --print`, receives the exact prompt on
stdin, and closes stdin; host stdout and stderr remain on the PTY. The runner
retries daemon connections through handoff without starting a daemon.

Scheduling is process orchestration, not lifecycle observation. Spawn failure,
process exit, cancellation, and cold-daemon interruption are Scheduled Execution
outcomes and never imply Agent `working`, `idle`, `blocked`, or `done`. Existing
Agent attention remains the authority when a linked Agent reports `blocked`.
The scheduler does not parse terminal output, answer guarded prompts, inject
input into an active session, or infer a canonical external session.

Fresh is the default session mode and starts a new external Agent Session for
each dispatched execution. Continuation schedules pin one exact existing
integration and external session identity; they never select the latest session
or fall back to fresh work. Manual and timed decisions atomically enforce one
nonterminal execution per schedule and workspace, the configured daemon-wide
bound, and exact continuation leases. Policy refusals are durable skipped
decisions rather than queued work.

New schedules are paused by default, and manual run-now remains available while
paused. Protocol 24 evaluates canonical cron triggers in their stored IANA
timezone and exposes deterministic next occurrences, scheduler health, timed
and skipped decisions, and `[scheduling] max_concurrent`. There is no automatic retry or timeout: an
execution remains active until its process exits, the user cancels it, or runner
or cold-daemon loss interrupts it. Scheduled starts use the daemon's
startup environment as ephemeral input; it is never persisted, and environment
changes require daemon restart. Scheduled-work support extends graceful restart
so the invoking client's validated environment becomes the replacement daemon's
startup environment without entering durable state or the handoff manifest.
Host exit and host-spawn-failure reports are staged on the nonterminal execution;
the exact runner ShellRun EOF publishes `run_exited` before committing the
terminal execution transition. This prevents a later dispatch from observing a
terminal execution while the reusable runner shell is still live.
These limitations and scheduler health must be visible to clients. See
[`scheduled-agent-work.md`](scheduled-agent-work.md) and [ADR 0002](adr/0002-separate-agent-schedules-from-runtime-identity.md).

Protocol 25 and state schema 12 add revision-exact Scheduled Execution
observation. Every retained execution has a positive durable revision; the
schema-12 migration assigns schema-11 records revision 1 without changing any
other field. Exact waits use the event condition variable only for wakeup and
read an event-frontier-owned committed execution map rather than mutable durable
state. The map is replaced from the complete retained execution set only when
successful persistence is published. Persistence in flight, pending durable
batches, and lifecycle event reservations therefore cannot expose revisions that
may roll back. A deadline still returns the last committed exact snapshot without
waiting for blocked storage, and equal terminal process revisions continue
waiting because a canonical Agent link may arrive later.
Protocol 26 adds additive exact-run attachment. `Attach.expected_run_id`
is optional and defaults absent for older clients. When present it requires the
`exact_run_attachment` capability and disables exited-shell restart.
Protocol 27 adds optimistic Agent Schedule definition editing. One atomic request
replaces name, exact prompt, and canonical trigger only while paused and only at
the caller's expected schedule revision. A changed prompt or trigger advances its
component revision; any change advances the schedule revision once. Trigger
changes reset the evaluation frontier to commit time. Exact no-ops do not persist
or publish, active executions retain captured revisions, and update events remain
prompt-free. Protocol-26 peers filter update events without rewinding cursors.
The durable representation is unchanged, so state schema remains 12.
Protocol-25 lists are daemon-bounded, newest-first pages with explicit limit and
truncation. The request limit is optional on the wire; protocol 25 defaults it to
100 and clamps it to 1 through 1,000, while protocol-23 and protocol-24 requests
remain uncapped. List responses carry schedule-keyed next-occurrence projections
for the complete selected schedule scope, independent of the execution page.
Projections are sorted by schedule ID, bounded to 100, and report
`schedule_limit` and `schedules_truncated`. Exact inspection carries its
projection separately from durable execution state; all projection shapes and
metadata are removed when responding below protocol 25.
Each schedule retains all nonterminal records and 100 terminal records, pruned by
ascending requested time and execution ID during ordinary terminalization, cold
recovery, and shutdown. The durable dispatch-key filter remains authoritative
after pruning.

Execution events carry the complete prompt-free revision and remain in the
ordinary 8,192-event journal with 256-event pages. Protocol-23 and protocol-24
visibility filtering is unchanged and filtered events still advance cursors.
Opt-in dispatch-failure and cold-interruption notification categories use the
same bounded fail-open sink as Agent notifications, but deduplicate independently
by execution, revision, and reason. Cold interruption is persisted before sink
setup and delivered only for records newly changed by that recovery.
Dispatch-failure notifications are derived when their durable event batch is
actually published, including after a failed first write and later pending flush.
The prior committed execution snapshot must be nonqualifying, so late Agent links
and other revisions that retain the same terminal state and reason do not notify
again.

Protocol 7 adds a bounded in-memory daemon event journal and atomic output-state
reads. Clients reconnect through stream UUID/event-ID cursors and recover from
retention or cold-restart expiry by requesting a fresh snapshot baseline.
Graceful handoff version 4 transfers retained events before publishing a
`handoff_completed` boundary and resuming PTY readers. See
[`event-stream.md`](event-stream.md).

Protocol 8 adds durable workspace launcher definitions. Protocol-7 clients can
still read workspace snapshots because launcher lists are additive; launcher
events are filtered from protocol-7 event pages while their cursors continue to
advance.

Protocol 9 adds agent instances to workspace and event snapshots and adds exact
ID get, register, and report requests. Protocol-8 and older responses omit agent
snapshot fields and filter agent events while preserving the unfiltered cursor.
The daemon owns agent IDs, observation revisions, timestamps, completion, and
durable storage. External lifecycle integrations own the meaning and evidence
of their reports; this slice does not discover processes, parse terminal output,
wait for agents, or control them.

Protocol 10 adds `EnsureAgent`. Its durable identity key is integration,
external session ID, shell ID, and run ID; the external session ID is mandatory
for ensure. A unique existing match is returned without changing its name,
observation, revision, timestamps, persistence, or event stream. This lets an
integration reload and reacquire the daemon-owned agent ID. A different run is a
different identity. Multiple matching legacy records are accepted only when
exactly one is active; otherwise ensure fails rather than guessing.

Protocol 11 adds an explicit exited-shell restart request. Opening one shell or
restoring a workspace first moves each exited durable shell back to pending, so
its stored argument vector starts as a new run on attachment while preserving
the shell identity and incrementing the run generation. Plain attachment to an
exited run remains non-mutating and can replay its retained terminal state.

External observation authority is ordered lifecycle integration, process
adapter, then terminal heuristic. Lower-authority reports are successful no-ops.
At equal authority an exact duplicate is also a no-op, but a changed report is
accepted, so a source can advance its own state and evidence. The exception is a
same-authority, same-confidence `working` report: evidence-only changes are
successful no-ops because they do not change lifecycle meaning and would put
high-frequency tool activity on the durable persistence path. Higher-authority
reports replace lower-authority observations. `daemon_lifecycle` is a wire and
snapshot value reserved for daemon-originated observations and is not exposed
by the public mutation CLI. Exact retries of an accepted `done` report return
the completed snapshot without another revision, write, or event; conflicting
reports after completion are rejected.

### Explicit Process-Adapter Supervisor

`src/process_adapter.rs` implements the first process-adapter foundation behind:

```console
boomux agent supervise [<name>] --integration <integration> --external-session-id <canonical-root-id> [--shell-id <shell-id>] [--run-id <run-id>] -- <exact argv>
```

An omitted descriptive Agent name is generated as `adjective-noun`. Shell and
run IDs default from `BOOMUX_SHELL_ID` and `BOOMUX_RUN_ID`. The
supervisor validates the supplied identity, spawns the exact argv directly with
inherited stdin, stdout, and stderr, and waits for that child. It returns the
child's exit code, or `128 + signal` for signal termination. There is no PTY,
shell interpretation, output capture, or detached process ownership in this
adapter.

After spawn, it idempotently ensures the integration, external session ID,
shell ID, and run ID key. Both process start and process exit are observations
with state `unknown`, authority `process_adapter`, and confidence 100; evidence
names the child PID and the exit code or signal. A process boundary is evidence
only of a process boundary. It never reports `done` and does not infer
`working`, `blocked`, or `idle` from process existence or termination.

Reporting is fail-open. Ensure, start-report, and exit-report failures warn but
do not terminate the child or alter its exit result; spawn and wait failures are
ordinary supervisor failures. A completed ensured instance receives no further
reports. Lower process-adapter authority cannot overwrite lifecycle-integration
state. Exact-key matching also means records with any different integration,
external session ID, shell ID, or run ID coexist, while a supervisor supplied
the lifecycle integration's complete key reacquires that same durable instance.

This primitive intentionally does not discover an external session identity.
For OpenCode in particular, a process, argv, local database, or API does not
identify the canonical root session selected by the user. Automatic handling of
fresh, continue, fork, or in-process session switching is unsafe and unsupported
unless the caller already possesses the selected canonical root ID. Ordinary
OpenCode should not be wrapped merely to obtain process evidence when the
lifecycle plugin is available; that plugin resolves root ancestry and reports
the stronger lifecycle evidence described below.

### Integration Setup Policy

Boomux prefers authoritative harness integrations over process or rendered-screen
inference for canonical session identity and lifecycle state. This preserves the
distinction between direct harness evidence, process existence, and terminal
heuristics instead of making convenient setup silently weaken session semantics.
Process adapters and any future screen detectors remain explicitly lower
authority and cannot substitute for integration evidence.

The resulting installation cost is handled as a product workflow rather than
left as manual plugin-file management. The unified `integration` commands list
bundled harnesses, inspect host versions, assets, and current-run lifecycle
reporting, and install one or all assets with each write atomic and accompanied
by reload guidance.
Individual host installers remain equivalent shortcuts over the same registry
and safe primitives. `integration setup` provides guided consent and reload
guidance, `integration verify` checks current-run authoritative lifecycle
reporting, and `integration uninstall` safely removes one or all managed assets.

Observed host compatibility and provider-dependent gaps are recorded in
[`lifecycle-validation.md`](lifecycle-validation.md). Focused unit fixtures
exercise only the host fields and ordering Boomux consumes; the record
deliberately distinguishes those tests from transitions observed in real
managed sessions.

### OpenCode Lifecycle Plugin

The bundled plugin is validated against `opencode-ai` `1.18.15`. This is a
compatibility test point rather than a runtime version pin.

`integrations/opencode/boomux.js` is a config-time OpenCode plugin installed by
`boomux opencode install [--force]`. The installer targets
`$XDG_CONFIG_HOME/opencode/plugins/boomux.js`, falling back to
`~/.config/opencode/plugins/boomux.js`. It creates regular directories, rejects
detected symlinks and special targets, leaves identical content alone, and
requires `--force` to replace different regular-file content. OpenCode discovers
the global plugin file without a configuration edit, but must be quit and
restarted after installation or replacement.

The plugin activates only when both `BOOMUX_SHELL_ID` and `BOOMUX_RUN_ID` are
present. It resolves every event's OpenCode ancestry and uses the root session
ID as `external_session_id`; child and subagent events aggregate into that one
root agent instance. Busy/active work, chat, tools, compaction, and resolved
prompts map to `working`; outstanding permission or question requests and
session errors map to `blocked`; only root idle maps to `idle`. Blockers are
tracked as a set. Errors remain latched until their session resumes or is
deleted; later root work clears every root-aggregate error latch because it
demonstrates aggregate recovery, but does not clear outstanding permission or
question blockers. Only
explicit root `session.deleted` maps to `done`: child deletion and process or
shell exit do not complete the instance. Once the derived state is `working`,
later chat, tool, and compaction evidence is coalesced until a meaningful state
transition occurs. This keeps activity bursts off the CLI and durable fsync path.

On first relevant event, or after plugin reload, the plugin calls `agent ensure`
and then reports a changed derived state when the reused durable record does not
already represent `working`, or when another state or authority differs. Calls
use exact argument vectors, a one-second timeout, bounded output, and the stable
JSON envelope. Unmanaged sessions are a no-op; Boomux or ancestry failures are
rate-limited and fail open so OpenCode continues. `run_changed` disables all
later reports for that tracked root.

### Pi Lifecycle Extension

The bundled extension is validated against
`@earendil-works/pi-coding-agent` `0.84.1`. This is a compatibility test point
rather than a runtime version pin.

`integrations/pi/boomux.js` is a global Pi extension installed by
`boomux pi install [--force]`. The installer targets
`$PI_CODING_AGENT_DIR/extensions/boomux.js`, falling back to
`~/.pi/agent/extensions/boomux.js`, with the same regular-file, symlink, atomic
replacement, and `--force` rules as other bundled integrations.

The extension activates only when `BOOMUX_SHELL_ID` and `BOOMUX_RUN_ID` are
present and uses `sessionManager.getSessionId()` as canonical external identity.
`session_start` reports `idle` and `agent_start` reports `working`. `agent_end`
records an error only when the final assistant message has `stopReason: "error"`;
recoverable tool errors and earlier failed attempts do not become blockers.
After automatic retries, compaction, and queued continuations have drained,
`agent_settled` reports the final error as `blocked` or reports `idle`. A later
agent start clears the latched error. `session_shutdown` reports `inactive`
rather than `done` because Pi sessions can be resumed. Inactive records remain
durable but do not decorate dashboard shells. Session switches reset the local
agent ID and ensure the new canonical session identity. Reports are serialized,
use exact argument vectors and bounded JSON output, and fail open with
rate-limited diagnostics. Session shutdown makes one bounded retry so a
transient reporting failure is less likely to leave the old session active.

Protocol 12 adds `inactive`. Protocol-9 through protocol-11 clients receive that
observation as `unknown`, while protocol-12 clients can distinguish a resumable
session that is not currently active from permanent `done` completion. Protocol
13 adds durable Agent working-directory context; older clients receive Agent
snapshots without that additive field.

Protocol 14 adds an exact revision-conditional Agent read. `agent wait` holds the
event coordination boundary while sampling the durable Agent observation, then
uses the event condition variable only as a wake-up signal. Accepted reports are
persisted before publication, so a waiter sees either the old revision or the
complete committed replacement. Unrelated events cause harmless rechecks;
duplicate and lower-authority reports do not advance the revision. Waiters are
not persisted and reconnect with the same durable revision after daemon
replacement.

Protocol 15 adds one durable outstanding attention item per Agent. Accepted
`blocked` and `done` observations capture their reason and full raising
observation; unrelated later states preserve the item until acknowledgment, and
a newer qualifying observation supersedes it. Acknowledgment is conditional on
the captured observation revision, idempotent once empty, and does not mutate
the lifecycle revision used by `agent wait`. Version-5 state migrates with no
outstanding items so upgrading does not reinterpret historical work as unseen.
The CLI projects these records into a deterministic blocked-first queue and
reports fixed lifecycle-state and attention counts per workspace.

Protocol 18 adds native-attachment focus reports and an optional focused
terminal snapshot. The state is non-durable, is accepted only from the current
controller for the current shell run, and uses a monotonic revision so clients
can distinguish a later refocus of the same shell. Protocol-17 responses omit
the additive field. Graceful handoff transfers a still-current focus target;
cold daemon recovery clears it.

Protocol 19 adds optional workspace default working directories and shell
creation without an explicit workspace. Older responses omit the additive
workspace field, and requests using either new behavior require protocol 19.
Protocol 20 adds bounded structured terminal previews, including styles and
modifiers, so the dashboard can render colors and emphasis without replaying
terminal control sequences. Older clients continue to use plain rendered reads.
Protocol 21 adds a targeted focused-terminal read so event-driven dashboards can
refresh non-durable focus without rebuilding the complete registry. Protocols
7-20 use the event stream with a one-second snapshot fallback for ephemeral
fields; protocol 6 retains one-second snapshot refreshes.
Protocol 22 adds workspace-owned Agent Schedule definitions and prompt-free
schedule events. Schedule requests require protocol 22. Older snapshots omit the
additive schedule summaries, and older event readers filter schedule events
without rewinding their cursor. State schema 9 explicitly migrates schema 8
workspaces with empty schedule collections rather than reinterpreting missing
durable fields.
Protocol 23 adds manual Scheduled Execution dispatch, cancellation, prompt-free
execution events, and durable schedule-owned shell identity. Protocol-22 peers
omit execution events, schedule-owned shells, and execution-shell links. State schema 10
explicitly migrates schema 9: existing shells become user-owned and schedules
receive empty execution histories.
Protocol 24 adds timezone-aware timed dispatch, skipped policy outcomes,
deterministic next occurrences, scheduler health, and bounded schedule/workspace/
daemon concurrency. Protocol-23 peers omit scheduler and next-occurrence fields
and filter timed or skipped execution records and events while preserving event
cursors. State schema 11 adds the trigger-revision-qualified durable evaluation
frontier and timed occurrence metadata; its explicit schema-10 migration retains
manual execution history without reinterpreting it as timed work.

Protocol 25 adds positive Scheduled Execution revisions, exact revision-aware
wait, bounded execution list metadata, and independently configured execution
notifications. Protocol-23 and protocol-24 execution visibility remains
unchanged. State schema 12 explicitly assigns revision 1 to every schema-11
execution while retaining all prior fields and data.

Protocol 26 adds the optional exact-run attachment expectation used by Scheduled
Execution terminal opens. Protocol-25 peers retain all observation, history,
and non-Open schedule dashboard behavior.

Protocol 27 adds paused, revision-conditional schedule definition updates and
prompt-free `agent_schedule_updated` events. Protocol-26 peers filter those
events while advancing their cursor. State schema remains 12.

Protocol 28 and Node identity schema 1 establish the stable local Node ID used by
federation. The daemon creates owner-only `node.json` independently from
authoritative `state.json`, preserves malformed or future identity files while
disabling federation, and exposes the identity through a version-gated query.
Cold restart and graceful handoff read the same file; the identity is not
transferred in the handoff manifest. State schema remains 12. Protocol-27 peers
remain local-only and cannot query Node identity.

Protocol 29 adds the same-socket `OpenFederationChannel` request. Federation
handshake version 1 is emitted by the hidden `__federation-stdio` helper only
after the helper's state-root identity matches the identity returned on the exact
daemon socket subsequently used for inner protocol bytes. Protocol-28 peers keep
stable identity queries but cannot open that channel. The SSH launcher/bootstrap,
registration, and projection protocols remain unimplemented under #173.

Protocol 30 adds expected-ID-conditional Node rekey. The daemon excludes restart,
shutdown, and concurrent rekey transitions, closes federation admission, and
atomically replaces `node.json` only after every admitted channel drains within
the bound. Timeout returns `busy`, reopens admission, and preserves the old ID;
rekey cannot be routed through a federation channel. `boomux node rekey` is a
local human-only workflow: it refuses noninteractive input and requires the
operator to type the exact current Node ID before sending the conditional
request.

Protocol 31 and Node registration schema 1 add explicit `node add`, `list`,
`inspect`, `rename`, `retarget`, and `forget`. Registration records contain only
the bounded local alias, exact SSH target, pinned Node ID, monotonic registration
revision, and tombstone epoch in owner-only `node_registrations.json`; discovered
helper paths and credentials are never persisted. Add and retarget use the
authenticated protocol-29 bootstrap before commit. Retarget must prove the
existing pinned identity and rename/retarget require the exact current revision.
Forget performs only a local prepare/drain/commit and cannot contact or stop the
remote authority. Malformed, future-version, oversized, or insecure registration
files are preserved and disable registration routing while the local durable
registry remains available. State schema remains 12, and the federation
handshake continues to describe its transport as `ad_hoc` without assigning that
field durable registration semantics.

Cron day matching preserves syntactic wildcard origin: `*/n` is wildcard-origin,
while numeric lists and ranges remain restricted even when they cover the full
field. Trigger acceptance proves at least one occurrence across a Gregorian
400-year cycle. Enabled snapshot projection propagates evaluator failures rather
than presenting them as a normal absent next occurrence. State schema 11 also
requires durable schedule timestamps to fit Chrono and validates timed IDs,
frontiers, scheduled/requested ordering, and coalescing reason/state combinations.

The scheduler reports active health only while its worker is running after a
successful evaluation and next-occurrence projection. Failure marks it offline
and uses an interruptible 50 ms through 5 second exponential retry delay without
acknowledging deterministic test ticks. Graceful restart transfers active work
even when a newly sampled lower limit is already exceeded; the truthful active
count is reported and admission remains blocked until enough terminal releases.

Opt-in desktop and sound notifications are a daemon-owned projection of committed
Agent state transitions, not durable queue state. A transition from any other
state into `blocked` or `done`, or from `working` into `idle`, schedules one
asynchronous delivery request after persistence and event publication locks are
released. The `working` to `idle` signal represents a completed unit of work but
does not create durable completed attention or make the Agent terminal. Enabled
desktop delivery invokes `notify-send`; enabled sound delivery invokes
`canberra-gtk-play` with a configured freedesktop event ID. Same-state evidence
or confidence revisions do not notify, and restored state is not replayed. Both
channels share one worker and a bounded, non-blocking queue. Delivery is
at-most-once and fail-open: queue saturation, a missing command, desktop-bus or
audio failure, timeout, or non-zero exit neither retries nor changes the
successful Agent mutation.
Notification payloads include only sanitized Agent, workspace, and shell names;
if retained Agent context outlives a removed shell, the shell is identified as
removed rather than suppressing the transition. Notifications never acknowledge
attention, advance an observation revision, or publish lifecycle events.
`notification test` exercises the same configured delivery commands directly
without fabricating or persisting an Agent transition.

Protocol 17 lets a restart request carry the invoking client's resolved
notification settings through the handoff manifest. This prevents a long-lived
daemon's inherited `XDG_CONFIG_HOME` or `BOOMUX_CONFIG` from overriding the
configuration intentionally selected by the restart caller. A protocol-16
daemon is first upgraded with a compatibility handoff, followed by a protocol-17
handoff that applies the settings.
The same two-stage rule applies when a protocol-16, protocol-22, or protocol-23
daemon is upgraded for protocol-24 scheduling settings: after the compatibility
handoff the client renegotiates, then sends complete notification, recovery,
environment, and `max_concurrent` values to the replacement.

### Transition Coordinator

`EventStream` serializes observable runtime transitions through its transition
frontier. A coordinated transaction covers the affected in-memory lifecycle,
durable state, retained event batches, and handoff capture. This gives clients
one ordering boundary instead of independent persistence and event locks.

Durable paths acquire the operation or mutation lock and persistence-ordering
gate before the `EventStream` transition frontier and retained event state. They
prepare an owned, immutable persistence generation while domain locks are held,
then release the transition, event, registry, lifecycle, and terminal locks
before submitting it to one FIFO writer. JSON serialization, temporary-file
writes, fsync, rename, and directory fsync therefore never retain locks required
by PTY readers. Shell close, workspace close, and shutdown use one lifecycle
transaction policy: prepare every runtime stop, finalize visible lifecycle
changes, apply the operation-specific durable removal, persist, then publish.

The remote federation coordinator remains outside this core durable
order. Its order is daemon transition, Node mutation gate, Node persistence gate,
local `EventStream` transition frontier, then Node registry/cache state. It never
acquires core durable, shell lifecycle, runtime, or terminal locks and never
retains a federation lock during SSH I/O. A copied registration revision is
revalidated only after network work and before an atomic cache or registration
commit. Registration changes atomically persist before replacing their in-memory
generation. Notification qualification and delivery occur after every federation
and event lock is released. Later federation delivery issues must preserve the
applicable order as the coordinator is extended.

Failure at any stage restores removed entities and exhaustively compensates every
stopped shell. Already-running processes cannot be resurrected, so their
compensated durable state is pending with a terminated last run; exited shells
recover their exact lifecycle and terminal state. Before preparing stops, the
transaction reserves event-ID capacity for existing pending runtime events, its
commit batch or one possible `run_exited` compensation per target shell,
whichever outcome is larger. Commit and compensation are mutually exclusive. A
natural exit for an unrelated shell that cannot reserve its event only because
of this temporary capacity is retried asynchronously after revalidating its
exact shell, run, and runtime identity; a shell finalized by the lifecycle
transaction makes that retry a no-op.

Durable lifecycle events are published only after their state is persisted. A
failed persistence attempt queues the event batch; background recovery persists
the latest state and publishes each queued batch exactly once. If a close cannot
commit after stopping a runtime, a running shell becomes pending with a
terminated last run, while an already-exited shell recovers its exact exited
lifecycle and terminal state.

While a persistence generation is in flight, PTY readers continue parsing bytes,
advancing output revisions, and delivering controller output. Their runtime
events wait in the ordered publication frontier behind the durable generation.
Writer success publishes the durable batch and then those runtime events; a
rollback removes the rejected durable transition and releases the runtime events.
Monotonic dirty revisions ensure a terminal-history checkpoint made after an
older generation was captured remains eligible for a later retry.

Baseline reads capture their snapshot and event cursor inside the durable
transition boundary, so the cursor describes the exact published cut represented
by the snapshot. Each PTY reader parses bytes, advances its run revision, updates
retained run metadata, and attempts bounded controller delivery using only
per-shell synchronization. It publishes the latest revision at most once per
16-millisecond window, with forced publication at pause, stop, and exit
boundaries. Output events may therefore skip intermediate revisions and event
IDs describe publication order rather than byte-arrival order. Revision-aware
reads use a per-runtime condition variable and do not depend on global event
publication for wakeups.

Agent registration, ensure, reports, and attention acknowledgment use the same
durable mutation coordinator.
Persistence and `agent_registered`, `agent_state_changed`, `agent_completed`, or
`agent_attention_acknowledged` publication therefore share the normal ordering
boundary and baseline snapshots include the exact coordinated cut. Notification
eligibility is derived from the pre-mutation and committed Agent states inside
this boundary, but sink dispatch occurs only after all coordinator and mutation
locks are released.

## Runtime Semantics

Closing a terminal window closes only its socket attachment. The daemon retains
the PTY master and child. Reopening a window acquires the controller and first
receives sanitized reconstructed terminal state followed by live output.

Closing a pending shell removes only metadata. Closing a running shell terminates
its child and disconnects its controller. Closing a workspace terminates its
shells before removing the workspace from the registry.
On Linux, cleanup signals every process still belonging to the shell's session
before reaping the session leader. `boomux daemon stop` applies the same cleanup
to the complete registry and removes the runtime socket.

The daemon atomically writes reproducible registry metadata to
`$XDG_STATE_HOME/boomux/state.json`, falling back to
`~/.local/state/boomux/state.json`. Workspace, launcher, shell, and agent IDs;
 names and grouping; working directories; argument vectors; agent observations and attention;
and last terminal profiles survive restart. The last run record also preserves
its identity and outcome. Recovered shells are pending: Boomux does not claim
that arbitrary processes, mutated environments, or PTYs survive daemon restart
or crash. When enabled, cold recovery substitutes an integration-native resume
command for a uniquely identified, lifecycle-authoritative OpenCode or Pi Agent
from an interrupted run. Ambiguous or invalid candidates use the shell's normal
command instead.

Plain-text terminal history is a separate opt-in recovery field because output
can contain secrets. The shadow terminal checkpoints a UTF-8-safe suffix of at
most 256 KiB per shell while output is active. A new run presents that text as
historical context before its own banner; the text is not replayed to the child
and does not reconstruct terminal modes or process state.

`boomux daemon restart` transfers the existing listener and both ownership locks
to a replacement process through a private, versioned `SCM_RIGHTS` handshake.
Prepare/finalize acknowledgement keeps rollback safe before the irreversible
ownership boundary. Pending shells restore from metadata. Detached running
shells transfer their PTY master, pidfd-backed process identity, terminal
profile, run identity, output revision, and reconstructed VT state without
changing the child PID. Attached clients receive a reconnect request,
acknowledge an input-ordering boundary, and reconnect to the replacement while
remaining in raw mode. Exited shells transfer their final run metadata and
bounded reconstructed terminal state without a PTY, pidfd, or replacement
process. Cold startup and crash recovery still restore shells as pending and do
not preserve live process or PTY ownership.

## Next Technical Steps

Future agent runtime work is tracked in [`roadmap.md`](roadmap.md). The explicit
process-adapter supervisor is only a foundation; automatic and
 integration-specific adapters, terminal heuristics,
and control remain future work.

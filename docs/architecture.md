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
| `src/global_workspace_store.rs` | Independently versioned coordinator Workspace metadata, placement membership, initialization and schema migration, prepared resource and placement-default recovery, and resumable close progress |
| `src/local_shell_journal.rs` | Checksummed owner-only commit journal for local coordinated Shell creation and initial run start across owner and coordinator checkpoints |
| `src/node_identity.rs` | Stable Node identity persistence, federation admission leases, and bounded rekey drain |
| `src/node_registration.rs` | Independently versioned remote Node registrations, identity pinning, admission drain, validation, and atomic storage |
| `src/node_projection.rs` | Disposable owner-only remote projection cache, deterministic bounds, health, generation CAS, quarantine, and atomic storage |
| `src/federation.rs` | Independently versioned federation handshake and verified stdio daemon bridging |
| `src/ssh_bootstrap.rs` | Validated SSH targets, private invocation configuration, bounded interactive authentication presentation, deadline-bound remote discovery, and helper compatibility selection |
| `src/handoff.rs`, `src/fd_transfer.rs` | Graceful daemon replacement records and Unix descriptor transfer |
| `src/attach.rs` | Terminal-side raw mode, control frames, live input/output, resize, focus, takeover waiting, and reconnect handling |
| `src/terminal.rs` | Selection and launch of native terminal windows through `xdg-terminal-exec` |
| `src/hyprland.rs` | Bounded Hyprland client discovery, special-workspace navigation, and exact-address window placement |
| `src/terminal_state.rs` | Shadow VT parsing, bounded reconstruction, logical output, and structured previews |
| `src/terminal_focus.rs` | Stateful parsing and restoration of child focus-reporting mode |
| `src/tui.rs` | Dashboard state, interaction, palette, polling, and Ratatui rendering; no direct daemon transport |
| `src/mobile_web.rs`, `src/web_terminal.rs`, `assets/mobile-web/` | Loopback-only HTTP gateway, Node-qualified Agent projection, exact local attention dismissal, native harness handoff, integration-independent exact-run terminal control, and embedded installable web assets |
| `src/tailscale_serve.rs` | Explicit Tailscale Serve preflight, conflict detection, exact route mutation, and ephemeral ownership cleanup for `boomux web --tailscale` |
| `src/session_projection.rs` | Projection of daemon Agent state and host catalogs into client-visible sessions |
| `src/host_services.rs` | Owner-local project, launcher, integration, Session-catalog, and bounded Git working-context services |
| `src/hook_input.rs` | Shared allowlist for structured absolute cwd and tool-path observations from lifecycle integrations |
| `src/integrations.rs` | Integration identity, display metadata, and optional installation, title/catalog, resume, and foreground capabilities |
| `src/host_session_titles.rs` and children | Shared title/catalog policy and host-specific discovery adapters |
| `src/host_session_source.rs` and children | Canonical host source paths, normalization, and secure source lookup |
| `src/integration_management.rs` | Integration inventory, status, setup, verification, install, and uninstall workflows |
| `src/setup.rs` | Interactive local setup orchestration, Omarchy plugin discovery and lifecycle operations, and ownership-bounded Hyprland binding installation |
| `src/update.rs` | Local release discovery, installation classification, interactive self-update authorization, atomic executable replacement, and daemon handoff verification |
| `src/uninstall.rs` | Interactive release uninstall orchestration, owned-asset cleanup, bounded purge validation, and process shutdown ordering |
| `src/claude_hooks.rs`, `src/codex_hooks.rs`, `src/kiro_hooks.rs` | Bounded Claude Code, Codex, and Kiro hook decoding and lifecycle reduction |
| `src/process_adapter.rs` | Exact-argv child supervision and fail-open process-bound Agent observation |
| `src/config.rs` | Layered configuration resolution, bounded validation, and transactional active-layer editing |
| `src/workspace_selection.rs` | Owner-only local CLI Workspace selection, validation, locking, and atomic persistence |
| `src/projects.rs`, `src/git.rs` | Bounded project discovery and asynchronous Git metadata |
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
  launch, integration management, and configuration editor launch execute argv
  directly without shell interpretation.
- **Local update ownership:** [`local-update.md`](local-update.md) defines the
  official-release eligibility, package-manager refusal, no-downgrade rule,
  atomic replacement, and graceful daemon handoff boundary.
- **Official first installation:** [`install.md`](install.md) defines the
  release-pinned installer, checksum verification, existing-target refusal, and
  interactive setup handoff.
- **Local uninstall ownership:** [`uninstall.md`](uninstall.md) reuses the
  official-release ownership boundary, preserves modified assets and user data
  by default, and removes the executable only after process cleanup.
- **Remote Node authority:** [`remote-nodes.md`](remote-nodes.md) defines the
  accepted federation boundary: one owning Node remains authoritative, SSH is a
  route, and local cached projections never authorize mutation or lifecycle
  inference. Ad hoc bootstrap and verified transport are implemented; durable
  registration, background reduced projection synchronization, and the typed
  routed management described by the current protocol history are implemented.

## Product Boundary

Boomux is a terminal session manager, not a harness-specific transcript UI or an
embedded multiplexer. Each ordinary Boomux shell is rendered by a native
terminal selected through `xdg-terminal-exec`; the optional web gateway embeds a
bounded renderer as another attachment client for exact current Agent runs.

```text
native terminal or web renderer
  -> boomux attachment client
  -> Unix socket
  -> Boomux daemon
  -> PTY
  -> child process
```

The attachment client remains responsible for rendering, fonts, themes,
selection, clipboard integration, and viewport behavior. Boomux provides
process persistence across attachment disconnects, naming, grouping, and
orchestration.

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
opening a daemon-bound stdio channel. The verified-connection boundary performs
exactly one live ping: after connecting a Ready helper, or before transaction
commit for an installed helper. Callers consume that verified result without a
second channel write. One private owned OpenSSH
master binds discovery, authorization, optional transactional replacement and
owner/PID/start-identity claim recovery, process-neutral missing-install rollback,
proof-bound daemon activation, daemon restart, final identity verification, protocol
ping, and marker-only commit to the same
authenticated endpoint. Explicit `boomux node add
ALIAS TARGET` and revision-conditional registration management persist an
identity-pinned route separately. A registration starts one noninteractive
projection worker. Protocol 33 exposes cached projections through the separately
named combined Node snapshot and dashboard while existing snapshot and list
operations remain local-only. Protocol 34 adds closed typed exact-Node private
reads and guarded management; unclassified mutation remains unavailable.
Protocol 35 adds Node-qualified native-terminal attachment and owner-environment
startup for remote pending and exited Shells.
Protocol 36 adds closed typed Node host services and owner-executed exact Agent
Session resume.
Protocol 38 adds coordinator-owned Workspace identity and explicit Node-owned
placements. Coordinator metadata is stored independently from Node-local runtime
state; equal names never establish membership, and adoption and linking require
exact owner identities and revisions.
Protocol 39 adds Node-qualified focused-terminal presentation to the combined
Node snapshot and a prompt-free local invalidation event for responsive
presentation. Focus remains ephemeral and controller-authorized, and is not
persisted in the reduced remote projection cache.
Protocol 40 lets an owner mark a pending Shell with its interrupted prior run
only when startup configuration and exact lifecycle state prove one unambiguous
Agent session can resume. Dashboards present that association as inactive;
protocol-39 responses remove the marker so old peers cannot display stale Agent
state as current.
It also adds coordinator-local dismissal and restore of stale cached Shell
presentation without routing, queuing, or claiming an owner mutation.
Protocol 41 carries the package version from each authenticated federation
handshake into the disposable Node projection and combined Node snapshot. The
Nodes dashboard displays that observed version; protocol-40 responses omit it.
It also adds bounded local Node-upgrade coordination so an explicitly authorized
SSH replacement closes registration admission across activation and commit. The
CLI renews that lease while the transaction runs, releases it after commit,
pre-mutation failure, or confirmed rollback, and retains it across unknown
outcomes. Local daemon restart or stop is rejected until the lease is released
or expires. An uncommitted remote bootstrap lock projects as reconnecting rather
than unsupported while watchdog recovery remains active; a lock without its
exact live watchdog identity projects as stale and requires operator recovery.
Protocol 42 adds the `opencode_shared_runtime_claims` feature. One ephemeral
Node-local OpenCode server generation is daemon-supervised and shared by native
clients. Bounded Agent Session Claims map an exact root Session in that generation
to one exact current ShellRun and ensured Agent Instance while one or more TUI
holders maintain it. Claims are absent from durable state, snapshots,
projections, events, and handoff; a surviving TUI reacquires after graceful
replacement. The runtime process identity and generation do transfer.
Handoff generation 5 accepts generation 4, whose manifest has no shared-runtime
record.
Protocol 43 adds `claude_remote_control_bindings`. Claude hooks may associate a
directly observed bridge session with one exact active local Claude Agent and
ShellRun. Set and exact-get requests are Node-local and unavailable to routed
operations. Bindings are bounded, ephemeral, absent from durable state,
snapshots, events, and remote projections, and add no old-response transform.
Handoff generation 6 accepts generation 5 and transfers valid bindings without
descriptors; generation-5 manifests default to no bindings.
Protocol 44 adds `collaborative_exact_run_attachment`. A distinct exact-run-only
request adds a bounded writable participant without replacing the ordinary
primary controller. It cannot start or resize a PTY, and clients never downgrade
it to takeover on an older daemon. The runtime-only participant map requires no
state or handoff format change. Its `Attached` response includes the primary
terminal profile, and later primary resize frames are copied to collaborators so
their local renderers retain the authoritative PTY grid.
Protocol 45 adds `kiro_exact_launch_holders`. A supervised exact-argv Kiro
launcher acquires one bounded ephemeral capability tied to its PID/start identity
and exact current ShellRun. Kiro hooks can ensure and report canonical Sessions
only through that live holder. Final-holder exit records Inactive at lifecycle
integration authority without reporting Done. That release introduced handoff
generation 7, which accepted generation 6 and transferred live holders and their exact Session/Agent
associations; cold recovery starts with none. Capacity is 256 holders with 16
Sessions per holder, whose maximal handoff encoding remains below the control
frame bound.
Protocol 46 adds `kiro_stop_idle`. Kiro v3 Stop hooks report Idle turn
completion. Protocol-46 clients downgrade Stop to Unknown when connected to a
protocol-45 daemon, preserving the original holder-report admission contract.
The wire shape, durable state, events, and handoff generation are unchanged.
Protocol 47 removes Agent Schedules and Scheduled Executions. Their historical
wire shapes and capabilities are absent from the current protocol. State schema
14 contains no schedule definitions, execution records, private prompts, runner
capabilities, or schedule-owned Shells. Because this is an alpha breaking
change, state schemas 9 through 13 are rejected rather than migrated.
Coordinator Workspace schema 7 likewise rejects schema 6, and disposable Node
cache schemas 3 and 4 are rejected so their projections are rebuilt. Cold
recovery cannot recreate removed scheduled work.
Protocol 47 is also the minimum supported core protocol and advertises
`protocol_47`; protocol-46 clients, daemons, and Nodes are rejected instead of
receiving schedule-free payloads in historical wire shapes. Handoff generation 8
uses `BOOMUXH8` and accepts only generation 8, making the same incompatibility
explicit before descriptor transfer. v0.32/state schema 13 therefore requires a
cold upgrade: stop the old daemon (terminating every managed process), reset the
incompatible runtime, coordinator, journal, selection, and projection state,
remove the old scheduling and scheduled-notification config keys, then install
and start the new binary. [`local-update.md`](local-update.md) defines the exact
operator sequence.
Protocol 48 adds `node_uninstall_coordination`. It atomically consumes an exact
Node maintenance lease into registration removal only after the interactive
client confirms identity-pinned remote uninstall, then best-effort removes the
now-inaccessible disposable projection. The remote mutation remains a fixed SSH
bootstrap operation rather than an arbitrary routed daemon request. Protocol-47
peers retain every prior Node operation but cannot use this atomic completion
primitive.
Protocol 49 adds `workspace_placement_default_cwd`. One exact coordinated
Workspace placement can change its owner-resolved default cwd under the current
global Workspace and owner Workspace revisions. The coordinator persists a
recoverable operation before owner mutation, proves ambiguous completion by an
exact owner Workspace read, then publishes the updated placement mirror. The
coordinator live-verifies protocol-49 owner support before preparing a remote
operation and again on the dispatch helper handshake before durably marking the
owner attempted. Definitive unsupported owners and cold-recovered preparations
that never crossed that attempted boundary leave no recovery state. The
coordinator also cancels the exact preparation after a definitive owner
rejection while retaining transport-ambiguous outcomes for readback recovery.
The owner persists before emitting `workspace_default_cwd_changed`; protocol-48
event readers filter that event while retaining cursor progress. Coordinator
Workspace schema 8 explicitly migrates schema 7 with empty pending and completed
default-cwd operation ledgers. Owner state schema 14 and handoff generation 8 are
unchanged because owner Workspaces already persist `default_cwd`.
Public Agent Session projection was retired after protocol 51. Current binaries
do not advertise Session capabilities or JSON commands, the native dashboard has
no Sessions view, and local or routed list, inspect, resolve, display-name, hide,
open, and resume requests fail with `unsupported_version`. Protocol 13, 50, and
51 wire shapes and state-schema-17 presentation metadata remain decodable during
this compatibility stage so existing state and mixed-version peers fail closed
instead of becoming unreadable. External session IDs remain durable only as
opaque Agent lifecycle, integration-authority, and exact-recovery inputs.

The following protocol 50 and 51 paragraphs describe retained legacy wire and
persistence shapes, not current advertised product capabilities.

Protocol 50 adds `session_display_names` and `session_presentation_context`. The
owner resolves one exact projected Agent Session, requires its owning Workspace
revision, and persists normalized
display-name metadata under the semantic integration plus external-Session or
Agent fallback identity. State schema 15 explicitly migrates schema 14 with empty
metadata and replay ledgers. Each Workspace retains at most 1,024 names and 256
operation replays; names contain at most 160 normalized characters. Each replay
receipt stores the semantic Session identity, integration, request arguments,
and only the immutable accepted mutation result: Session ID, Workspace ID,
nullable explicit user name, resulting Workspace revision, and `changed`. It
never stores a projected summary, harness title, catalog data, lifecycle state,
or occurrences. An exact retry returns that minimal result before projection or
revision validation even after later mutations, restart, or catalog loss. Reuse
of the operation UUID for different arguments fails closed. The owner increments
the Workspace revision, persists before publishing
`agent_session_display_name_changed`, and routes remote requests only over a live
identity-verified protocol-50 connection. Protocol-49 responses clear the
additive override and authoritative projected-occurrence details, default the
Workspace revision to zero, and filter the event while advancing the cursor. A
protocol-50 client normalizes absent projected occurrences from the retained
legacy Agent occurrences returned by a protocol-49 owner.

The same protocol advertises `observed_agent_working_contexts`. State schema 16
explicitly migrates schema 15 with empty working-context lists. While one exact
Agent, Shell, and ShellRun remain active, a private observation request carries
one structured absolute path. The owning Node canonicalizes the existing file or
directory, resolves its Git worktree root, common-repository label, and symbolic
branch or verified detached state, then durably records the observation before
publishing `agent_working_context_observed`. Git probes run outside mutation
locks with null stdin, one-second deadlines, isolated process groups, and 4 KiB
limits on each output stream. Non-repositories and integration-side observation
failures do not alter lifecycle reporting.

Each Agent retains at most eight roots newest-first. Reobserving the current
root/repository/branch tuple is an event-free no-op; observing the same root with
changed metadata replaces and promotes it. Session projection resolves the
registration-time source cwd with the same bounded Git inspection, excludes that
canonical launch root from observed-work presentation, deduplicates the remaining
roots across exact Agent occurrences, reports their total distinct count, and
returns at most the four newest repository/branch/timestamp summaries in lists.
Exact inspection returns up to 64 deduplicated contexts while limiting response-
time Git push and worktree inspection to the first four. The Agent retains its
launch-root observation. The independent nullable `git_branch`
remains owner inspection of the registration-time Session source cwd, so older
clients and launch-context presentation retain their prior meaning. Catalog-only
Sessions receive no fabricated Agent contexts. Exact
outstanding Agent attention references remain matched by occurrence Agent ID.

Protocol 51 adds `session_latest_agent_attribution`,
`session_working_context_push_status`,
`session_working_context_worktree_status`, and `workspace_session_hiding`.
Session summaries may expose the latest occurrence's Agent name separately from
the effective description. For each projected Working Context whose recorded
branch still matches the owner's current symbolic branch, the owner performs
bounded no-fetch response-time inspection. Existing local tracking refs produce
the separate up-to-date, committed-ahead count, or unpublished push status. A
porcelain-v1 status command with branch output and normal untracked files
produces only `staged` and `unstaged_or_untracked` booleans; conflicts may set
both. The two probes fail independently after the shared exact-branch gate. No
file names, file counts, file contents, or behind count are exposed, and neither
status is persisted or published as an event. Protocol-50 responses omit latest
Agent attribution, `push_status`, and `worktree_status` from list, inspect, and
resolved Session shapes.

State schema 17 explicitly migrates schema 16 with empty hidden-Session
tombstones and hide-operation receipts. A hide mutation resolves the unfiltered
owner projection, requires the exact owning Workspace and current revision, and
stores only integration plus external Session identity or exact Agent fallback.
Each Workspace retains at most 1,024 tombstones and 256 immutable replay
receipts. Hiding suppresses protocol-51 list results before response truncation
and makes exact inspect, resolve, open, and resume return `not_found`; it does not
delete host history, Agents, Shells, processes, display-name metadata, or
lifecycle state. A fresh request for an existing tombstone returns `changed:
false` without advancing the Workspace revision or publishing an event. The
first change persists before `agent_session_hidden` publication. Protocol-50
callers retain prior visibility and resume behavior, and filtered event readers
still advance their cursor.

Protocol-49 responses omit working contexts, filter their events and reduced
`session_context` transitions while preserving cursors, and clear the other
protocol-50 presentation additions. Successful Session activation acknowledges
listed attention revisions only after terminal launch; a newer revision fails
closed and remains outstanding.
Remote notification presentation reuses protocol-32 atomic reduced transitions,
so it does not require a later protocol. Node-cache schema 2 adds bounded local
at-most-once individual and reconnect-digest claims with an explicit schema-1
migration. Node-cache schema 3 adds at most 4,096 dismissed Shell IDs per Node
and explicitly migrates schema 2 with an empty dismissal set. Node-cache schema
4 adds the bounded observed helper version with an explicit schema-3 migration.

## Components

### Application

`src/main.rs` owns the CLI, project-name suggestions, dashboard actions, shell
name resolution, and dashboard backend orchestration. `src/dashboard_projection.rs`
converts daemon snapshots and projected sessions into typed TUI view models.
`src/session_projection.rs` is the shared binary projection used by the CLI and
dashboard to combine one daemon snapshot with bounded host session catalogs.
Omitted Workspace names in atomic create, and omitted Shell and Agent Instance
names, are resolved here to random lowercase `adjective-noun` values before
requests are sent. Generated names are collision-excluded from the relevant
snapshot. Generated shell names in existing Workspaces are
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

The protocol qualifies every Node-owned durable identity with a stable owning
Node without rewriting its inner ID. A coordinator-owned global Workspace is a
separate organizing identity whose placements reference exact Node-local
Workspace identities:

- A global Workspace owns its UUID, name, revision, placement membership, and
  close state, but no process or filesystem context.
- A Node-local workspace is a shell container with a UUID and optional default
  working directory. Its default applies only on that owning Node; individual
  resources may use other paths.
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
For a protocol-38 global Workspace, the coordinator first returns explicit
per-placement availability. The client then performs the same owner-side open on
every available placement, using local attachment for the local owner and typed
Node host services plus Node-qualified attachment for remote owners. Failures are
reported per Node and do not replay an ambiguous launcher invocation.

On protocol 38 or newer, the CLI may persist one selected coordinator Workspace ID in
`$XDG_STATE_HOME/boomux/selected-workspace.json`. This is local user preference
state; Node-local owner Workspace IDs are rejected. It is not coordinator
metadata, Node-owned runtime state, or a default
placement. Context-required commands resolve an explicit workspace first, then
the current managed Workspace, then the selected ID. A selected global
Workspace is mapped through a fresh combined snapshot to its exact local owner
Workspace for local name resolution. Creation still independently requires a
sole eligible Node or `--node`; cached remote state never authorizes mutation.
Exact resource IDs remain context-free, and omitted list filters retain their
existing all-workspaces meaning.

### Configuration

`src/config.rs` resolves the global XDG config file first and an optional
`BOOMUX_CONFIG` file second, merging tables field by field. When present,
`BOOMUX_CONFIG` is the active writable layer; otherwise the global file is. A
missing setting inherits from the lower layer or the built-in default.
Configuration is local Node state: it is neither daemon protocol state nor a
remotely routable mutation.

The Node-local `claude.remote_control` launch policy defaults on and is sampled
at daemon start. The owning daemon adds `--remote-control` only to an exact
one-element user Shell command whose executable basename is `claude`; its
    private login-Shell shim applies the same rule only to a zero-argument
    interactive invocation. Stored argv remains unchanged. Launchers, explicit
    Claude arguments, and recovery or Session resume vectors
never receive the flag, and a coordinator's setting cannot govern a remote
owner's launch.

`boomux config path`, `validate`, and `edit` are human-only local commands and do
not start the daemon. Validation parses and semantically resolves all configured
layers. Editing chooses `VISUAL`, then `EDITOR`, then `sensible-editor` with a
`vi` fallback. The selected command is parsed into an exact argument vector and
spawned directly, without a shell.

External edits use a bounded candidate and owner-only temporary files. Commit
requires an owner-validated regular target and verifies its bytes
against the edit baseline immediately before one atomic rename. New targets use
atomic no-replace creation. The candidate is fully validated before commit;
symlinks and detected changes to either loaded layer are rejected, new files are
mode `0600`, existing target mode is preserved, and file plus parent directory
are synchronized after commit.

### Daemon

`src/daemon.rs` owns all PTY masters and child processes. Its runtime directory
is restricted to the current user and the socket mode is `0600`.

The daemon is composed from state-owning services rather than one shared
registry. `DurableRegistry` owns workspace, shell, launcher, and Agent
collections, their invariants, mutation-specific undo, and persistence
projection. Undo records retain complete affected entities or complete mutable
state rather than mirroring the registry shape, so unrelated mutations do not
clone the registry and new entity fields remain part of rollback automatically.
Workspace-owned Session display-name metadata and hidden-Session tombstones are
part of that registry without making projected Agent Sessions durable entities.
Their semantic keys survive projected UUID mechanics and temporary catalog
disappearance, while Workspace closure removes the metadata with its sole owner.
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
The local coordinated Shell transaction extends the nested order with the
global Workspace stage before durable collection mutation and the journal append
after both staged projections are validated. Checkpointing retains the mutation
and persistence gates but captures durable and global snapshots without nesting
their internal locks before clearing the journal.

Request handling uses `DaemonError` to retain validation, lifecycle, persistence,
protocol, and internal failure classes until the wire boundary. Stable protocol
codes are selected from those variants directly; transport errors remain on the
connection path and are not reinterpreted as domain failures through
`io::Error` downcasting.

The daemon supports:

- Empty or explicitly populated workspace creation with an optional shell cwd
  default
- Guarded owner-resolved placement default-cwd changes for future Shell creation
- Atomic shell creation with an implicit `workspace-N` container when no
  workspace is selected
- Additional shell creation
- Ordered workspace launcher creation, inspection, rename, and removal
- Workspace and shell snapshots
- Shell and workspace rename operations
- Shell and workspace closure
- Bounded VT state and sanitized reconnect reconstruction
- One primary writable attachment with explicit takeover and up to four
  collaborative exact-run attachments
- Bounded backpressured output delivery to the primary attachment, while a
  saturated collaborative attachment is disconnected rather than stalling PTY
  output
- Serialized whole-frame PTY input from every participant, with resize authority
  retained exclusively by the primary
- Pending shell metadata and first-attachment terminal negotiation
- Run-scoped agent registration, idempotent ensure, inspection, and explicit
  state reports
- One ephemeral Node-local Shared Harness Runtime generation and bounded exact-run
  OpenCode Agent Session Claims
- Bounded exact-process Kiro Launch Holders and canonical Session associations

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

Protocol-35 owner-environment attachment is a mutually exclusive alternative to
the client Unix environment. The owning daemon constructs startup from its own
captured environment, then applies the validated terminal profile and
authoritative Boomux identity. Node-qualified attachment never sends the
presenting Node's environment. A protocol-34 owner can attach only an
authoritatively preflighted running Shell; it cannot start or restart one
remotely.

Protocol-42 OpenCode coordination derives Workspace identity from the
authoritative Shell and requires an exact current running ShellRun. Claim ensure
is idempotent per runtime generation, root Session, ShellRun, and TUI holder; it
also ensures the durable Agent Instance. Multiple holders for the same mapping
are allowed, while a different current ShellRun for the same runtime Session is
`busy`. Release removes one holder, expiry removes abandoned holders, and the
last holder removes report authority and records the resumable Agent as
Inactive. Run or runtime replacement invalidates the mapping. The bounded claim
map is not persisted, projected, handed off, or event-published, so
`STATE_VERSION` and all durable schemas remain unchanged.

The Shared Harness Runtime is neither a durable resource nor part of the Shell
registry. The daemon starts one generation on the first eligible bare interactive
`opencode` or `boomux web`, supervises it, and retains it across terminal detach
and web-client restart. Graceful handoff transfers its strict PID/start/runtime
identity and generation; cold startup adopts only an exactly matching runtime.
Daemon stop terminates it. A hidden shared launcher establishes a scoped `PATH`
only for eligible Boomux login Shells and is not stable public automation.
When cold recovery selects one exact resumable OpenCode Agent, the replacement
ShellRun uses that same hidden launcher with the canonical Session ID. The TUI
therefore attaches to the current Shared Harness Runtime generation and creates
a fresh exact-run claim. Failure to prepare the shared launch falls back to the
existing standalone exact-Session recovery rather than making the Shell
unopenable. Explicit argument-bearing user invocations remain unchanged.

### Attachment

`src/attach.rs` runs inside the selected terminal emulator. It enables raw mode,
reports dimensions, and copies bytes in both directions without transforming
live PTY output. Protocol-18 attachments also enable xterm focus reporting and
recognize focus-gained input. Physical focus events are forwarded to the PTY
only while the child has requested focus mode; otherwise they are consumed so
ordinary shells do not receive synthetic escape input. Child mode changes are
tracked across output chunks and reconstruction while Boomux keeps physical
reporting enabled. Each focus gain is also reported through the
participant-authorized attach stream. RAII cleanup restores terminal and
focus-reporting modes when the attachment exits. Explicit takeover detaches the
displaced controller so only the newly opened native terminal remains. Graceful
daemon handoff uses the reconnect boundary and reconstructs terminal state after
the replacement daemon becomes available. Other transport failures retain the
bounded reconnect deadline, and an exact-run change remains permanent.

The daemon keeps a bounded output queue per active primary and collaborator. PTY
output fans out without blocking; a slow or disconnected collaborator removes
only itself, while a slow primary retains the existing primary-disconnect
behavior. Primary and collaborator input frames are written under one runtime
serialization boundary so bytes within a frame cannot interleave. Collaborative
resize is ignored; only the primary can update the PTY and retained terminal
dimensions. Explicit takeover detaches the prior primary and all
collaborators; graceful handoff instead reconnects and awaits every participant.
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

`shell create --open` and atomic `workspace create --node NODE --cwd DIRECTORY
--open` prepare and spawn the exact terminal before the coordinated creation
request so window presentation overlaps durable coordinator work. The
hidden attachment waits on an owner-only runtime socket and receives success
only after the parent has received the completed creation response. Creation
failure closes the gate without attaching, so a ShellRun cannot start before
coordinator completion and the waiter never polls or interprets `not_found`.

Node-qualified opens still launch local `xdg-terminal-exec` presentation. Their
hidden `__attach` command carries the exact owner Node ID and unchanged inner
Shell ID. The local daemon opens one identity-verified SSH helper channel and
relays bounded `AttachFrame` values; the remote daemon retains PTY, controller,
focus, takeover, resize, reconstruction, and exact-run authority. Remote
transport loss closes only the local attachment and does not synthesize Shell or
Agent completion. Protocol 39 also records a relayed
focus gain as ephemeral presentation state on the local daemon after forwarding
it to the owner. The presentation identity is the exact owner Node and Shell;
it does not transfer focus authority or enter the remote projection cache.
Presentation recording reflects the physical local focus report after the frame
is written to the owner stream; it is not an acknowledgment that the owner
accepted the frame and is never used as lifecycle evidence.
The presenting daemon publishes a payload-free local invalidation after this
recording so dashboards refresh the combined view on their next event poll. The
owner's corresponding event remains owner-local and is excluded from reduced
projection transitions.

Baseline launching requires no emulator-specific adapter or compositor window
ID. The local desktop layer defaults to `disabled`; an explicit
`desktop.workspace_layer = "hyprland-special"` enables it. On Omarchy,
`boomux setup` recommends the companion plugin and offers this configuration as
separate default-yes consent. When enabled, the local
client decorates initial titles for coordinated Workspace opens, desktop-layer
presentation, and coordinated create-and-open with exact Node and Shell
identity. Direct Shell, dashboard Shell/item, path, and Session opens retain
baseline launch behavior. `src/hyprland.rs` queries
bounded `hyprctl` JSON, reuses matching adapter-opened windows, and moves current
ephemeral window addresses to a special workspace derived from the immutable
coordinator Workspace ID. It invokes `hyprctl` and
the resolved terminal through exact argument vectors without `dispatch exec` or
shell interpolation. Addresses are never persisted, projected, sent to an owner
Node, or treated as resource identity. Query, correlation, or placement failure
after spawn is presentation-only and leaves the terminal open.
Before placement or visibility, the adapter applies an exact runtime Workspace
rule selecting `dwindle` for that Boomux special Workspace; it never changes the
global or ordinary-Workspace layout.

The human-only `desktop toggle`, `desktop next`, `desktop previous`, `desktop
show`, `desktop terminal`, `desktop close`, `desktop pop`, `desktop return`, and
`desktop gather` commands provide compositor and UI entry points. Exact showing and cycling
uses active non-closing coordinator Workspaces in deterministic name/ID order;
outside a Boomux special workspace, next/previous retain ordinary Hyprland
workspace navigation and terminal retains ordinary `xdg-terminal-exec`
behavior. Desktop presentation attaches existing Shells but is not a Workspace
open or restore and does not invoke launchers. No daemon protocol, persistence
schema, or handoff state represents the desktop layer.
Navigation dispatches the already-materialized compositor Workspace before
durably updating the default Workspace selection, and identical selections are
not rewritten, so selection fsync latency does not delay the visual transition.
Contextual close uses the active Hyprland window's immutable initial-title
identity only as correlation. Before lifecycle mutation it requires canonical
Node/Shell IDs, an exact match with the daemon's qualified focused terminal, and
active membership in the visible coordinator Workspace, then revalidates the
captured ephemeral address and Hyprland stable window ID immediately before the
close. A later focus change cannot retarget the action. It fails closed on an
identity, window, or membership mismatch; outside the layer it delegates
ordinary active window closure to Hyprland.
Contextual pop avoids Hyprland pinning inside a Boomux special Workspace because
unpinning a tiled window can return it to the underlying ordinary Workspace. It
uses ordinary float-and-pin behavior outside the Boomux layer.
Contextual return correlates the active window's stable Node/Shell identity,
resolves its owner Workspace through the authoritative Node, requires one unique
active coordinator placement, and moves only that exact window back. It does not
open, restart, take over, or otherwise mutate the Shell. Return requires the
exact qualified identity in the immutable initial title and does not fall back
to matching a mutable human title.
Desktop gather targets the visible Boomux Workspace, or the selected Workspace
when none is visible. It moves matching terminal attachments back into that
layer and opens missing attachments for user-owned Shells without invoking
Workspace launchers.
An exact `open --workspace` request validates the Shell's Node-qualified owner
against an active placement in that coordinated Workspace before presentation.
With the adapter enabled it shows the owning layer and places only that Shell's
terminal; no sibling Shell or launcher is opened.
An explicit coordinated `workspace open --show` reveals the target desktop
layer before performing normal Workspace restore semantics. It therefore opens
all user Shell attachments and invokes every launcher exactly as `workspace
open` does, while keeping the restored presentation in the owning layer.
The native TUI uses the same presentation for coordinated Workspace restore and
for individual Agent/Shell opens. A restore with at least one opened or reused
item reports unavailable placement operations as a nonfatal warning; an attempt
with no successful item remains an error. The TUI remains active in its terminal
after desktop focus moves to the revealed Hyprland Workspace, and refreshes its
model so it is current when the user returns.
Spawned terminal windows start in independent process sessions with null
standard streams, so exiting the dashboard cannot close their attachments.
The internal attachment process restarts an exited shell only after the terminal
window has spawned, preserving retained output when terminal preparation fails.

### Dashboard

`src/tui.rs` remains a control plane. Its typed model update boundary consumes
dashboard events and returns explicit external effects. The terminal runtime
executes those effects through one backend interface and feeds typed completion
events back into the model; model transitions do not call daemon or terminal
callbacks directly. The backend runs effects serially off the terminal thread,
so daemon, SSH, and preview latency cannot delay input or rendering; periodic
refresh and preview reads are single-flight to keep stale work from accumulating.
Rendering remains a function of typed model state. One
daemon snapshot contains each workspace, its launchers, and its shells, avoiding
races between separate list operations. Configured project roots provide
Workspace-name suggestions. Selecting one validates its discovered path on the
local Node and atomically creates the coordinated Workspace, its local placement,
and a first pending Shell with that path as both Shell cwd and placement default.
Arbitrary by-name creation still creates empty coordinator metadata, and older
peers retain the path as the empty local Workspace default. Git information is
still collected independently from item directories and cached. Paths do not
create Workspace-level Git identity, and mixed-directory Workspaces remain valid.

The dashboard establishes an atomic event-stream baseline and treats later
events as invalidation signals for authoritative snapshot reprojection. Idle
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
ephemeral workspace, shell, run, and monotonic revision metadata. Protocol 39
also retains one monotonic presentation revision across local and relayed remote
focus gains and exposes its exact `(node_id, shell_id)` through the combined
Node snapshot. With
`dashboard.follow_focused_terminal` enabled (the default), the dashboard uses a
new revision as a one-shot selection trigger and resolves both shell and Agent
rows by Node-qualified durable Shell identity. Repeated snapshot refreshes do not enforce the
selection, so manual navigation remains usable until another terminal focus
gain. Focus changes are deferred while an overlay or close confirmation is
active. The setting is dashboard-local and disabling it does not stop focus
reporting by attachments.
Temporary projection absence does not reset the client's observed focus
frontier. A replaced event stream or a handoff that changes the negotiated
protocol resets it once. Same-version handoff retains the exact presentation
frontier, while 39-to-38 and 38-to-39 transitions deliberately start the active
protocol's revision domain without suppressing later physical focus gains.
`boomux close --focused` resolves this latest reported Boomux focus, revalidates
the authoritative Shell, and performs a local close or revision-guarded remote
close on its owning Node. It requires protocol 39 so resolution always uses a
Node-qualified identity. It is not an operating-system active-window query: if
a non-Boomux window is active, the latest retained Boomux focus remains the
target. The command fails when no focus has been reported and retains the normal
prohibition against closing the invoking managed Shell from inside itself.

Selected-kind previews remain read-only. Workspace, launcher, and run metadata
come from the polled snapshot. Shell output uses a bounded plain-text read only
when the selected shell, run ID, or output revision changes. Its viewport follows
the tail by default and preserves the viewed rows when new output arrives while
scrolled. The dashboard omits a contextual preview rather than rendering a
partial panel when the full preview and a usable item table cannot both fit.
Command previews expose argv and run metadata without reading terminal output.
Launcher previews never imply retained invocation state because launcher
processes remain ephemeral.

### Mobile Web Dashboard

`boomux web` is a separate presentation client in the CLI process, not a daemon
listener or new authority. It binds only to IPv4 loopback and uses the ordinary
typed client to obtain one combined Node snapshot per refresh. The embedded PWA
projects local authoritative Agents and reduced remote Agents into a deliberately
unstable HTTP view model. Every resource retains its exact `(node_id, agent_id)`
identity, and remote freshness, health, and staleness remain visible.

Each HTTP port owns one Unix control socket in Boomux's private runtime
directory. The socket answers bounded versioned status requests with the exact
loopback or tailnet URLs and accepts a versioned stop request. `boomux web start`
requires a running daemon, launches the foreground gateway in a detached session
with an owner-only runtime log, and waits for that socket to report readiness.
Equivalent starts are idempotent and mismatched options fail closed. `boomux web
stop` waits for graceful shutdown and owner-validated socket cleanup. These
commands do not signal guessed processes, contact the daemon shutdown path, or
stop the Shared Harness Runtime.

`--tailscale` is an explicit deployment mutation. The gateway invokes the
PATH-resolved Tailscale CLI with exact argument vectors, requires a connected
Node with a MagicDNS name, and preflights the current Serve JSON before changing
it. A compatible existing route is reused but never claimed. A conflicting root
handler fails startup before mutation. Missing dashboard HTTPS 443 and an active
OpenCode HTTPS runtime-port route are added independently, and only routes
created by that gateway are recorded in an owner-only, versioned runtime record.
Graceful exit removes those exact root routes; `boomux web stop` also reconciles a record
left by a crashed gateway. Changed or externally owned routes are never removed,
and Boomux never resets unrelated Serve configuration. Tailscale remains
responsible for certificates, tailnet identity, grants, and ACL policy.

Agent visibility follows the Omarchy presentation contract rather than exposing
the complete durable registry. For each
exact current `(node_id, shell_id, run_id)`, the newest non-inactive and non-done
Agent observation is shown; historical, inactive, and done Agents remain only
while they carry outstanding durable attention. A gateway-owned background
projection worker refreshes once per second even without browser clients and
retains an in-memory ephemeral finished marker after observing a local current
Agent transition from working to idle. That marker is not durable attention, is
never derived from an initial idle baseline or remote cache update, and clears
when the Agent leaves idle, ceases to own the exact current run, or the gateway
process restarts. A temporary daemon failure marks the cached response
disconnected without discarding its last bounded projection or finished markers.

The attention mutation route dismisses an exact local Agent alert. It requires a
same-origin JSON request carrying the exact Node ID, Agent ID, and current
attention or lifecycle observation revision. Durable attention is hidden only
after the daemon confirms revision-conditional acknowledgment; an ephemeral
finished marker is cleared only in the gateway that observed it. Remote
projected attention and stale revisions fail closed. A separate route authorizes
terminal control only when the exact Node, Agent, Shell, and ShellRun still
identify a current local Agent card, independent of integration name. It stores
at most 64 one-use grants for 30 seconds. The WebSocket upgrade requires an
allowlisted loopback or active Tailscale dashboard Origin and carries the grant
in a secondary WebSocket subprotocol rather than a URL. Consumption removes the
grant before attachment.
At most four browser terminal bridges may remain active, and each bridge uses
eight-entry transport queues plus a bounded browser write deadline. Protocol 44
also bounds daemon collaborators to four per Shell.
The gateway requests collaborative exact-run attachment without restart or
environment authority and fails closed instead of downgrading on an older daemon,
then translates only bounded PTY input/output, resize, focus, detach, and daemon
reconnect frames. Browser viewport changes cannot resize the PTY or alter the
local logical grid. The renderer initializes from the primary terminal profile,
follows later primary resize frames, and permits viewport scrolling when that
grid exceeds the phone display. On-screen keyboard changes resize and offset the
browser's visible terminal viewport, keeping its tail available without changing
the authoritative grid; deliberate scrollback remains locally scrollable. It
never exposes the daemon attachment token or arbitrary daemon protocol. The
self-hosted `ghostty-web` canvas renderer uses Ghostty's
WASM VT implementation and a self-hosted terminal font; the Agent handoff is a
terminal-only browser history entry, and direct terminal focus is its only input
surface. Browser Back or page backgrounding releases only that collaborator.
Boomux does not parse harness transcripts or infer lifecycle from terminal bytes.
The home-page snapshot remains the complete HTTP Agent projection.

`boomux web` attempts to ensure the same daemon-supervised Shared Harness Runtime
used by eligible native OpenCode TUIs. A typed missing OpenCode executable is an
optional-capability result: the dashboard starts without a runtime or OpenCode
route, while Claude and other presentation remain available. Port conflicts,
runtime exit or timeout, protocol incompatibility, and daemon transport failures
still fail startup. Requested OpenCode configuration is retained separately from
actual runtime availability so detached equivalent starts remain idempotent.
For an authoritative claimed local OpenCode Agent with a canonical external
Session ID and UTF-8 working directory, Boomux
base64url-encodes the directory and constructs OpenCode's exact
`/<directory>/session/<session-id>` route on that Agent's home-page card. The
browser derives a default public origin from its current scheme and hostname plus
the stable runtime port.
`--opencode-web-url` overrides that public origin for the same local runtime; it
does not select an unrelated server. Boomux does not proxy OpenCode, persist its
first-start username/password environment, or advertise local Session identity
in remote projections. Attached clients must use environment consistent with
that first start. The OpenCode origin remains a separate full-control
security boundary behind the user's private TLS/authentication/ACL layer.

In an eligible managed login Shell, only bare zero-argument interactive
`opencode` resolves through the scoped runtime `PATH` shim and internally runs
stock `opencode attach` against the shared server. Arguments, subcommands,
noninteractive execution, use outside Boomux, absolute binary paths, and a
modified `PATH` execute real OpenCode unchanged. Private bash, zsh, and fish
startup adapters apply the shim after normal interactive shell configuration so
startup files cannot accidentally reorder it; other shells retain fail-open
startup behavior. The TUI plugin reactively ensures and releases claims as root
selection switches or forks; the server
plugin resolves a current claim before lifecycle ensure/report. Missing,
expired, conflicting, run-changed, or generation-changed claims fail closed
without rebinding authority. `--pure`, `--mini`, absolute paths, modified PATH,
and conflicting same-Session Shells therefore have no native link. Remote Agents
remain unlinked.

HTTP binds to `127.0.0.1` and is intended to sit behind a user-selected private
access layer. Except for the explicit `--tailscale` Serve integration, Boomux
does not configure remote transport or TLS, and it never defines access policy
or interprets proxy identity headers. Loopback binding prevents LAN or remote
peers from bypassing that external boundary. API responses use `no-store`,
the service worker caches only the public application shell, and a restrictive
content security policy prevents external script, style, object, and framing
origins. This edge does not expose the owner-only daemon socket over TCP.
Web terminal input is remote shell access. WebSocket frames and queues are
bounded to the attachment limit, browser backgrounding releases control, and all
terminal API responses remain outside service-worker and HTTP caches. The
external private access layer must restrict the dashboard to trusted users.

Agent Sessions were formerly a client-side projection over Agent instances and
provider history catalogs. ADR 0014 retires that public resource model. The
dashboard primary kinds are now Workspaces, Agents, Shells, and Nodes; dashboard
startup and refresh do not inspect provider history catalogs. The `session`
command, Session host services, resume services, and Session mutations are not
advertised and current daemons reject their retained legacy request shapes with
`unsupported_version`.

Protocol 51 and state schema 17 remain decodable during this compatibility stage.
Legacy Session wire variants, projection helpers, and persisted presentation
metadata are inert implementation detail, not supported APIs. Exact external
session IDs remain on Agent instances because integrations use them as opaque
run-scoped lifecycle authority and exact cold-recovery input. They do not define
a browseable, resumable, nameable, or hideable Boomux resource.

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
dashboard recognizes exact registered host processes, including `opencode`, `pi`,
`claude`, `codex`, and `kiro-cli`, as presentation-only agent-shell hints
before a canonical Agent session exists; this hint creates no AgentInstance,
durable state observation, persistence, or events. It displays `untracked` until
lifecycle data exists, then yields to that authoritative observation. `doctor`
checks installed integration assets and reports a running untracked host with
explicit install or restart guidance.
Under protocol 40, a pending Shell snapshot exposes its exact interrupted last
run and owner-selected Agent ID only when the owning daemon's startup
configuration and durable state prove one unambiguous lifecycle-authoritative
resumable Agent. The dashboard keeps that exact row's `agent` kind but presents
it as `inactive` until opening starts a new run.
Fresh or ineligible pending Shells have no run marker and remain ordinary Shell
rows.

### Agent Skill

The optional vendor-neutral `boomux` Agent Skill documents the complete public
CLI for compatible clients, including discovery, inspection, output reads,
lifecycle operations, native-terminal opening, and daemon management.
`BOOMUX_SHELL_ID` provides current-shell context. Federated resource identity is
the pair of owning Node ID and unchanged Node-local ID; exact IDs are global only
within one Node. The installer safely removes an
untouched legacy `boomux-shells` skill and preserves customized copies.

Read-only CLI integrations use the separate `boomux.cli/v1` JSON envelope rather
than serializing daemon protocol snapshots directly. `boomux capabilities`
advertises supported commands, features, schemas, and error codes without
requiring a daemon. Protocol 6 error responses carry an additive optional code;
clients expose it as `ClientError::Remote(RemoteError)`, while mixed-version
peers retain message compatibility.

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

The human-only top-level `boomux setup` command composes these existing local
primitives without adding protocol or durable setup state. It requires terminal
input and output, prints its active configuration path, and completes read-only
terminal, daemon, harness, and Omarchy inspection before presenting a concise
machine-specific plan. A missing `xdg-terminal-exec` is a blocker before any
mutation. Harness and modified-asset prompts remain default-no. Setup starts or
confirms the local daemon only during final verification, then reinspects current
assets and prints a readiness receipt, exact restart guidance, and one primary
next action. The in-memory receipt retains current, changed, skipped, warning,
and failed outcomes; failures include a step-specific recovery command, while
reinspection reports any earlier committed desktop state. The optional Agent
Skill remains a separate owned asset.

On an Omarchy installation, setup executes only bounded exact-argument `omarchy`
commands. Plugin inventory comes from `omarchy plugin list --json`; installation
uses the fixed `gardnmi/omarchy-boomux` HTTPS repository and Omarchy retains
plugin lifecycle ownership. A Cargo-private executable without a corresponding
`~/.local/bin/boomux` is rejected before desktop mutation because it may be
absent from the graphical session's `PATH`. After installing or enabling the
plugin, setup runs bounded `omarchy restart shell` so the running shell loads it,
then rechecks that the fixed plugin identity is enabled. Current enabled plugins
and current or compatible user-managed keybindings produce no rerun prompt.
The plugin is presented as the recommended core Omarchy experience rather than
an incidental integration. Once it is enabled, setup separately offers to enable
the default-off Hyprland Workspace layer by updating only
`desktop.workspace_layer` in the active configuration layer through the same
owner validation, baseline revalidation, and atomic commit boundary as
`boomux config edit`. The fixed plugin and Workspace-layer prompts are visibly
recommended and default to yes; declining either preserves the current state.
After a guided local Boomux update commits, the updater revalidates an installed
companion plugin, delegates its update to the fixed exact-argument Omarchy CLI,
and restarts Omarchy Shell when the plugin is enabled. Omarchy remains the plugin
lifecycle owner, and plugin failure cannot roll back the committed executable.
Boomux never edits `/usr/share/omarchy`. The full
desktop binding profile is one marked block in the current user's
`~/.config/hypr/bindings.lua`. Setup reports conflicts before consent, preserves
all bytes outside that block, rejects symlinked, special, non-owned, oversized,
or malformed targets, revalidates the inspected baseline, preserves mode, and
atomically replaces and synchronizes the file. An active Hyprland session is
reloaded and checked with `hyprctl configerrors`; otherwise the bindings take
effect at the next session. The complete historical user-managed Boomux profile
is recognized by its exact panel, focus-release, desktop, Shell-create, and
focused-close actions and remains user-owned without adding a managed block.
Local uninstall removes an unchanged managed binding block and, when detected in
a bounded rechecked inventory, removes the exact Omarchy plugin through the
Omarchy CLI after the overall uninstall confirmation.

Observed host compatibility and provider-dependent gaps are recorded in
[`lifecycle-validation.md`](lifecycle-validation.md). Focused unit fixtures
exercise only the host fields and ordering Boomux consumes; the record
deliberately distinguishes those tests from transitions observed in real
managed sessions.

### Codex Lifecycle Integration

The Codex descriptor targets `@openai/codex` `0.147.0` as a compatibility test
point. Installation merges exact `boomux codex hook` handlers into
`${CODEX_HOME:-$HOME/.codex}/hooks.json`. Unrelated top-level fields, event
groups, and handlers remain untouched. Reads are bounded and reject symlinks and
non-regular targets; writes atomically verify the inspected baseline and retain
the existing mode. A modified Boomux handler requires force repair. Uninstall
removes only Boomux handlers and deletes the file only when no unrelated content
remains. Codex must be restarted and the hook reviewed and trusted with `/hooks`.

Eligible managed Codex invocations pass through a Shell-scoped executable shim
and hidden launcher. Bare chat, `resume`, and `exec` are considered managed chat;
when the installed handlers are current, the launcher prepends `--enable hooks`
and exports `BOOMUX_CODEX_RUN_SCOPED=1`. Option-led invocations including
explicit `--remote`, other Codex subcommands, an absent or modified installation,
and use outside Boomux remain untracked. An exact configured primary executable
is forwarded through `BOOMUX_REAL_CODEX`; typing an absolute path in a login
Shell bypasses the scoped shim. Bash startup clears cached executable paths after
reasserting the shim-first `PATH`, so a prior direct Codex resolution cannot bypass
the scoped launcher. Hooks silently do nothing unless the run-scoped marker and
exact `BOOMUX_SHELL_ID` and `BOOMUX_RUN_ID` are present. An explicitly remote TUI
therefore cannot claim authority inherited from its app-server.

Codex hook `session_id` is the canonical thread identity and ensures the exact
`(codex, thread, shell, run)` Agent key. SessionStart reports Idle, except compact
starts remain Working; prompt, tool, compaction, and subagent activity report
Working; PermissionRequest reports Blocked; Stop reports Idle; and SessionEnd
reports Inactive. Codex hooks never report Done. Input, identity, and reporting
are bounded and fail open for the host.

Exact resume uses `codex resume <thread-id>`. The experimental catalog
adapter starts bounded `codex app-server --stdio`, completes `initialize` and
`initialized`, then requests `thread/list` for the exact normalized workspace
directory. It filters ephemeral or invalid threads, sanitizes name or preview
titles, bounds output, count, and runtime, and fails open without changing Agent
authority. These additions reuse existing protocol Agent, session, and capability
shapes, so they require no protocol or durable-state version
change. Boomux exposes no Codex Remote handoff because Codex documents no exact
thread-specific Remote URL; it does not reinterpret `codex://threads/ID` as a
phone-accessible Remote destination.

### Kiro CLI Lifecycle Integration

The Kiro descriptor targets `kiro-cli` `2.18.0` with its opt-in v3 harness as a
compatibility point. Installation owns the dedicated
`${KIRO_HOME:-$HOME/.kiro}/hooks/boomux.json` file and registers non-deciding
SessionStart, UserPromptSubmit, PreToolUse, PostToolUse, and Stop command hooks.
The ordinary bounded, symlink-safe, atomic integration installer applies because
Boomux does not share that file with unrelated Kiro configuration.

Eligible managed Kiro invocations pass through the common Shell-scoped shim and
hidden launcher. A bare `kiro-cli` becomes `kiro-cli --v3`, while an explicit
leading `--v3` is preserved. The launcher supervises the exact argument vector
with inherited terminal streams and foreground process-group behavior, and
acquires a private daemon-owned Launch Holder
only while the installed asset is current. Only that Kiro process tree receives
the holder capability. Eligible login ShellRuns stage the delegating shim even
when Kiro is not yet installed, so a later installation into their existing
executable search path does not bypass lifecycle integration. Kiro v2 and
service invocations, absolute paths typed in a login Shell,
modified PATHs, absent or modified assets, and use outside Boomux execute stock
Kiro unchanged and untracked. Exact configured executable paths are retained
through `BOOMUX_REAL_KIRO`; private launcher provenance is removed from unrelated
children.

Kiro hook `session_id` is the canonical Session identity and ensures the exact
`(kiro, session, shell, run)` Agent key through its live Launch Holder. Prompt and
tool events report Working. Kiro v3 documents Stop as the boundary where the
agent completed its turn and finished responding, so Stop reports Idle for that
exact Session. Idle is resumable turn completion, not permanent Session
completion. The documented hooks expose no authoritative permission-wait,
session-inactivity, error, or permanent-completion event, so Boomux never derives
Blocked or Done for Kiro. Exact supervised process exit releases its holder; if
no other live holder owns that canonical Session, Boomux reports Inactive at
lifecycle integration authority. Concurrent holders for one Session keep it
active until the final release. Late hooks from dead holders fail closed inside
Boomux and remain fail-open to Kiro. If one Kiro process switches canonical
Sessions without ending the old one, both histories remain truthful and cold
recovery refuses the resulting ambiguity rather than guessing.

Exact resume uses `kiro-cli --v3 chat --resume-id <session-id>`. Cold-recovery
launches use the run-scoped launcher. For each exact normalized Workspace
directory, the title adapter runs bounded
`kiro-cli chat --list-sessions --format json` discovery. A sanitized title may
replace the `Kiro CLI` fallback only for an already-durable Kiro Session with the
same external ID. Listing failures, malformed or oversized output, unsupported
host versions, unmatched IDs, and all additional historical or cloud records
fail open without creating Sessions or changing lifecycle, resume, or persistence
semantics. Although Kiro cloud sessions can be viewed in
Kiro Web and Mobile, Kiro documents no exact browser URL derivable from a local
CLI Session ID, and cloud hooks execute in Kiro's sandbox rather than under the
local ShellRun. Boomux therefore exposes no Kiro native web handoff or cloud
lifecycle authority. Protocol 45 carries holder acquire, hook report, and release
operations. Holders remain absent from durable state, snapshots, projections, and
events, so `STATE_VERSION` is unchanged. Current graceful handoff generation 8 transfers
only holders whose exact process identity remains live; cold recovery inherits no
holder authority.

The daemon checks holder liveness once per second, matching the ordinary UI
observation cadence. Holder operations and graceful handoff reconcile
immediately. Reconciliation uses the ordinary durable mutation coordinator, so
Inactive persistence precedes event publication and wakes event waiters. Failed
release after graceful replacement is therefore eventually observed by the
replacement daemon without another Kiro launch. Reconciliation also reports an
active Kiro Agent without any live holder association as Inactive. This clears
authority after cold recovery and when a pre-protocol-45 Kiro process survives a
Boomux upgrade without weakening holder admission. Routine checks read only
exact PID/start identity; acquire and import retain strict argv and environment
checks. Acquire revalidates the exact current ShellRun and unchanged process
start identity inside that same mutation gate immediately before insertion.
Handoff import independently requires a live
process, a current running ShellRun, and active exact Kiro Agent associations.

The launcher installs Linux `PR_SET_PDEATHSIG` on the exact Kiro child before
exec. It does not install or replace signal handlers. Terminal Ctrl+C retains
ordinary foreground process-group delivery and shell-visible `128 + signal`
status. If the holder alone receives SIGTERM, SIGHUP, or another terminating
signal, holder death causes the kernel to terminate the exact managed child so it
cannot remain orphaned. If descendants survive that immediate child cleanup, the
daemon signals only a process group whose original holder was confirmed as its
leader and whose surviving member still carries the private holder capability;
PID/start validation prevents treating a live or reused holder PID as dead.

### Claude Code Lifecycle Integration

The Claude Code descriptor targets `@anthropic-ai/claude-code` `2.1.236` as a
compatibility test point. It installs the single bundled
`integrations/claude/.claude-plugin/plugin.json` asset at
`${CLAUDE_CONFIG_DIR:-$HOME/.claude}/skills/boomux/.claude-plugin/plugin.json`.
Claude Code discovers that user-scoped skills-directory plugin in place. Its
inline lifecycle hooks use exec-form `boomux claude hook`, receive bounded JSON
on stdin, and make no stdout decision. Outside a managed ShellRun the hook is a
silent no-op. Inside one, Claude's canonical `session_id` and the authoritative
Boomux environment ensure the exact `(claude, session, shell, run)` Agent key.
Subagent hooks retain the root `session_id` and therefore reduce into that Agent
Instance rather than creating separate Agents.

Session start, a completed foreground turn, and an API-error turn report Idle;
prompts, tool work, denied tool permissions, and subagent activity report
Working; permission or user-input waits report Blocked; session end reports
Inactive and never Done. A Stop with reported background tasks or session crons
remains Working. Reports use LifecycleIntegration authority and reuse the existing
protocol Agent operations, so lifecycle reporting needs no Claude-specific wire
request or durable state. Hook failures are fail-open for Claude Code and are
written only to stderr.

While Remote Control is connected, Claude exposes
`CLAUDE_CODE_BRIDGE_SESSION_ID` only to hook subprocesses. The hook synchronizes
that opaque value through protocol 43 after ensuring the exact Agent. Absence
clears the exact binding, and root SessionEnd always clears it. The daemon
validates the Claude integration and current Agent/ShellRun, rejects bridge
collisions, and never persists, event-publishes, or projects the value to another
Node. A graceful handoff revalidates and transfers bindings after importing the
surviving ShellRuns; cold startup has none.

The descriptor recognizes the exact `claude` executable and foreground process
and resumes an exact canonical Session with `claude --resume ID`. It advertises
a title capability but no catalog capability. The title adapter is validated
against Claude Code `2.1.251` and is deliberately fail-open because Claude has no
documented title-list API. It inspects only direct regular `.jsonl` files under
`${CLAUDE_CONFIG_DIR:-$HOME/.claude}/projects/<encoded-cwd>`, rejects symlinks,
and reads bounded prefixes. A title is eligible only when the filename stem,
transcript `sessionId`, and normalized transcript `cwd` agree with the durable
Agent and Workspace. The latest sanitized `ai-title` may enrich that exact
Session; transcript text is never used as a fallback and unmatched files never
project history.

### OpenCode Lifecycle Plugins

The paired bundled plugins target the source-visible OpenCode TUI API and server
event API at the `opencode-ai` `1.18.18` compatibility point. TUI behavior is
version-gated because that API is not a stable public package contract. This is
a compatibility test point rather than a runtime version pin or a claim of live
shared-runtime validation.

`integrations/opencode/boomux.js` is a config-time OpenCode plugin installed by
`boomux opencode install [--force]`. The installer targets
`$XDG_CONFIG_HOME/opencode/plugins/boomux.js`, falling back to
`~/.config/opencode/plugins/boomux.js`. It creates regular directories, rejects
detected symlinks and special targets, leaves identical content alone, and
requires `--force` to replace different regular-file content. OpenCode discovers
the global plugin file without a configuration edit, but must be quit and
restarted after installation or replacement.

The TUI plugin activates only in an eligible managed ShellRun attached to the
current Shared Harness Runtime generation. It reactively claims the selected
root and updates authority after Session switches and forks. The server plugin
resolves every event's OpenCode ancestry and current claim, then uses the root
Session ID as `external_session_id`; child and subagent events aggregate into
that one root Agent Instance. Busy/active work, chat, tools, compaction, and resolved
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

Claim ensure creates or reacquires the durable Agent Instance before server-side
reports are accepted. The server plugin reports a changed derived state when the
reused durable record does not already represent `working`, or when another
state or authority differs. Calls use exact argument vectors, a one-second
timeout, bounded output, and the stable JSON envelope. Unclaimed Sessions are a
no-op; Boomux, ancestry, claim, or version-gating failures are rate-limited and
fail open so OpenCode continues. `run_changed` or runtime-generation replacement
removes report authority rather than redirecting it.

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

Pi advertises a title capability but no catalog capability. Its bounded local
adapter reads direct regular JSONL files from
`${PI_CODING_AGENT_SESSION_DIR:-$PI_CODING_AGENT_DIR/sessions}` (falling back to
`~/.pi/agent/sessions`), rejects symlinks, and requires the `session` header's
exact ID and normalized cwd to match the durable Agent and Workspace. The latest
valid `session_info.name` supplies the title; when absent, the existing bounded
first-user-message fallback applies. Unmatched Pi records never project history.

All five lifecycle integrations may submit working-context observations only
after ensuring or reporting the exact Agent. OpenCode uses its initializer
directory/worktree and structured tool events; Pi uses `ctx.cwd` and typed tool
calls; Claude, Codex, and Kiro use their documented top-level cwd and structured
tool input. Claude additionally registers context-only `CwdChanged` and
`DirectoryAdded` hooks, which do not change lifecycle state. Tool extraction is
restricted by tool family to absolute file/notebook paths, directory-search
paths, or explicit shell workdir/cwd fields. Command text, transcripts, tool
output, arbitrary nested payloads, and relative paths are ignored. Observation
failure is isolated from the successful lifecycle report so host execution
remains fail-open.

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
registration, and projection capabilities were introduced by the later protocol
versions below under #173.

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

Opt-in desktop and sound notifications are a daemon-owned projection of committed
Agent state transitions, not durable queue state. A transition from any other
state into `blocked` or `done`, or from `working` into `idle`, queues one
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
advancing output revisions, and delivering participant output. Their runtime
events wait in the ordered publication frontier behind the durable generation.
Writer success publishes the durable batch and then those runtime events; a
rollback removes the rejected durable transition and releases the runtime events.
Monotonic dirty revisions ensure a terminal-history checkpoint made after an
older generation was captured remains eligible for a later retry.

Baseline reads capture their snapshot and event cursor inside the durable
transition boundary, so the cursor describes the exact published cut represented
by the snapshot. Each PTY reader parses bytes, advances its run revision, updates
retained run metadata, and attempts bounded participant delivery using only
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
the PTY master and child. Reopening a native window acquires the primary
controller and first receives sanitized reconstructed terminal state followed by
live output. Closing a collaborator removes only its own token, queue, and
connection.

Closing a pending shell removes only metadata. Closing a running shell terminates
its child and disconnects every attachment participant. Closing a workspace terminates its
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
command for a uniquely identified, lifecycle-authoritative OpenCode, Pi, Claude,
or Codex Agent from an interrupted run. Ambiguous or invalid candidates use the
shell's normal command instead. OpenCode recovery routes the exact Session
through the Shared Harness Runtime so its replacement ShellRun can establish a
fresh claim; shared-launch preparation failure retains the standalone native
resume fallback.

Plain-text terminal history is a separate opt-in recovery field because output
can contain secrets. The shadow terminal checkpoints a UTF-8-safe suffix of at
most 256 KiB per shell while output is active. A new run presents that text as
historical context after a recovery notice; the text is not replayed to the
child and does not reconstruct terminal modes or process state. New runs without
recovered history begin with no Boomux-injected terminal output.

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
The same graceful boundary transfers the Shared Harness Runtime's strict process
identity and generation but no Agent Session Claims. Surviving TUI holders
reacquire claims after reconnect. Cold startup adopts a runtime only when all
identity checks match; otherwise it starts a new generation when next needed.

## Local Release Updates

`boomux update status` is a local CLI read that discovers the latest stable
release without starting or contacting the daemon. `boomux update` is a
human-only guided mutation. It supports only an official `github-release` build
running from the current user's canonical `~/.local/bin/boomux`; package-managed,
source, development, root-owned, custom-path, unsafe, and unknown installations
are reported but never replaced. The complete contract is
[`local-update.md`](local-update.md).

The updater downloads only the exact GNU/Linux release asset for the current
`x86_64` or `aarch64` architecture through fixed HTTPS endpoints. Existing
bounded release download and checksum validation from `src/ssh_bootstrap.rs` is
shared with remote helper installation. A strict stable semantic-version
comparison prevents downgrade, including when the installed build is newer than
the latest published release.

After explicit confirmation, the updater pins the installed executable and
candidate bytes, smoke-tests the candidate, retains a same-directory hard-link
rollback inode, and atomically renames the candidate over the path. If the daemon
was absent it remains absent. If it was running from that exact path, the updater
uses the existing graceful restart and verifies the replacement process argv,
path, inode, and digest against the pinned candidate. Failed activation restores
the old pathname and coordinates a reverse graceful restart. No updater state is
added to durable daemon state, and no protocol or persistence version changes.

## Next Technical Steps

Future agent runtime work is tracked in [`roadmap.md`](roadmap.md). The explicit
process-adapter supervisor is only a foundation; automatic and
 integration-specific adapters, terminal heuristics,
and control remain future work.

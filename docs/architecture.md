# Architecture

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

## Components

### Application

`src/main.rs` owns the CLI, project-name suggestions, dashboard actions, shell
name resolution, and conversion from daemon snapshots into TUI view models.

### Protocol

`src/protocol.rs` defines the versioned wire model. Control messages are JSON
with a four-byte big-endian length prefix. Attachment traffic uses small binary
frames for input, output, resize, and detach events.

The domain has five durable identities:

- A workspace is a globally named shell container with a UUID. It has no path
  or working directory.
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

The daemon supports:

- Empty or explicitly populated workspace creation
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
first attachment supplies `TERM`, `COLORTERM`, terminal program identity, and
cell/pixel dimensions; the daemon then creates the PTY and child. Failed startup
leaves the shell pending and retryable. Shell creation may omit a workspace ID.
The daemon then selects the lowest
available `workspace-N` name and publishes the generated workspace and shell as
one operation. Concurrent requests retry name allocation rather than exposing
an ungrouped shell.

### Attachment

`src/attach.rs` runs inside the selected terminal emulator. It enables raw mode,
reports dimensions, and copies bytes in both directions without interpreting
keys or transforming live PTY output. An RAII guard restores terminal mode when
the attachment exits.

The daemon keeps a bounded output queue per active controller. A slow client
drops output rather than blocking the PTY reader and child process.
It also feeds a shadow `vt100` parser while forwarding the original PTY bytes
unchanged. Reattachment receives a bounded reconstruction of rendered state,
not historical OSC or graphics commands.

### Terminal Launcher

`src/terminal.rs` uses Omarchy's `xdg-terminal-exec` metadata to launch:

```console
boomux __attach <shell-id> --takeover
```

No emulator-specific adapter or compositor window ID is required.

### Dashboard

`src/tui.rs` remains a control plane. It receives backend-neutral view models
and callback functions rather than opening sockets itself. One daemon snapshot
contains each workspace, its launchers, and its shells, avoiding races between separate list
operations. Configured project roots provide workspace-name suggestions only.
Git information is collected independently from shell directories and cached;
empty or mixed-directory workspaces have no workspace-level directory or Git
identity.

Agent instances appear as workspace counts and read-only rows. Completed rows
remain visible and inspectable; dashboard actions do not open, rename, close, or
otherwise control an agent.

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
new clients expose it through a typed `RemoteError`, while mixed-version peers
retain message compatibility.

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

External observation authority is ordered lifecycle integration, process
adapter, then terminal heuristic. Lower-authority reports are successful no-ops.
At equal authority an exact duplicate is also a no-op, but a changed report is
accepted, so a source can advance its own state and evidence. Higher-authority
reports replace lower-authority observations. `daemon_lifecycle` is a wire and
snapshot value reserved for daemon-originated observations and is not exposed by
the public mutation CLI. Exact retries of an accepted `done` report return the
completed snapshot without another revision, write, or event; conflicting
reports after completion are rejected.

### OpenCode Lifecycle Plugin

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
tracked as a set, and errors remain latched until later work is observed. Only
explicit root `session.deleted` maps to `done`: child deletion and process or
shell exit do not complete the instance.

On first relevant event, or after plugin reload, the plugin calls `agent ensure`
and then reports a changed derived observation when the reused durable record
does not already match. Calls use exact argument vectors, a one-second timeout,
bounded output, and the stable JSON envelope. Unmanaged sessions are a no-op;
Boomux or ancestry failures are rate-limited and fail open so OpenCode continues.
`run_changed` disables all later reports for that tracked root.

### Transition Coordinator

The daemon serializes observable runtime transitions through one coordinator. A
coordinated transition covers the affected in-memory lifecycle, durable state,
retained event batches, and handoff capture. This gives clients one ordering
boundary instead of independent persistence and event locks.

Coordinator paths acquire the operation or mutation lock, transition
coordinator, persistence lock, and event state in that order. Fine-grained
registry, lifecycle, and terminal locks are acquired only where each path needs
them; code does not wait on blocking PTY I/O or process shutdown while holding
the coordinator. Close and shutdown first stop runtimes, then finalize visible
lifecycle changes inside the coordinator.

Durable lifecycle events are published only after their state is persisted. A
failed persistence attempt queues the event batch; background recovery persists
the latest state and publishes each queued batch exactly once. If a close cannot
commit after stopping a runtime, a running shell becomes pending with a
terminated last run, while an already-exited shell recovers its exact exited
lifecycle and terminal state.

Baseline reads capture their snapshot and event cursor inside the same boundary,
so the cursor describes the exact cut represented by the snapshot. PTY bytes are
not persisted per chunk, but output revision mutation and `output_changed`
publication cross the coordinator together. A non-blocking terminal lock keeps
output processing from holding global locks while waiting on terminal snapshots.

Agent registration, ensure, and reports use the same durable mutation coordinator.
Persistence and `agent_registered`, `agent_state_changed`, or
`agent_completed` publication therefore share the normal ordering boundary and
baseline snapshots include the exact coordinated cut.

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
names and grouping; working directories; argument vectors; agent observations;
and last terminal profiles survive restart. The last run record also preserves
its identity and outcome. Recovered
shells are pending: Boomux does not claim that arbitrary
processes, mutated environments, or PTYs survive daemon restart or crash.

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
process. Cold startup and crash recovery remain metadata-only and restore shells
as pending.

## Next Technical Steps

Future agent runtime work is tracked in [`roadmap.md`](roadmap.md). Process
adapters, terminal heuristics, aggregation, waits, notifications, and control are
not part of the first agent runtime slice.

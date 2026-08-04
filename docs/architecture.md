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

The domain has only two durable identities:

- A workspace is a globally named shell container with a UUID. It has no path
  or working directory.
- A shell owns one PTY, child process, name, explicit working directory, and
  workspace ID.

There are no separate tab, pane, and terminal identity layers.

### Client

`src/client.rs` resolves the socket at
`$XDG_RUNTIME_DIR/boomux/daemon.sock`. It starts a detached daemon on demand,
waits for the protocol ping to succeed, and exposes typed management requests.
An owner-held file lock prevents concurrent daemons from unlinking each other's
sockets or splitting the registry.

### Daemon

`src/daemon.rs` owns all PTY masters and child processes. Its runtime directory
is restricted to the current user and the socket mode is `0600`.

The daemon supports:

- Empty or explicitly populated workspace creation
- Atomic shell creation with an implicit `workspace-N` container when no
  workspace is selected
- Additional shell creation
- Workspace and shell snapshots
- Shell and workspace rename operations
- Shell and workspace closure
- Bounded VT state and sanitized reconnect reconstruction
- One writable attachment with explicit takeover
- PTY input and resize forwarding
- Pending shell metadata and first-attachment terminal negotiation

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
contains each workspace and its shells, avoiding races between separate list
operations. Configured project roots provide workspace-name suggestions only.
Git information is collected independently from shell directories and cached;
empty or mixed-directory workspaces have no workspace-level directory or Git
identity.

### Agent Skill

The optional vendor-neutral `boomux` Agent Skill documents the complete public
CLI for compatible clients, including discovery, inspection, output reads,
lifecycle operations, native-terminal opening, and daemon management.
`BOOMUX_SHELL_ID` provides current-shell context while exact shell IDs remain
globally addressable within the daemon. The installer safely removes an
untouched legacy `boomux-shells` skill and preserves customized copies.

## Runtime Semantics

Closing a terminal window closes only its socket attachment. The daemon retains
the PTY master and child. Reopening a window acquires the controller and first
receives sanitized reconstructed terminal state followed by live output.

Closing a pending shell removes only metadata. Closing a running shell terminates
its child and disconnects its controller. Closing a
workspace removes all its shells from the registry before terminating them.
On Linux, cleanup signals every process still belonging to the shell's session
before reaping the session leader. `boomux daemon stop` applies the same cleanup
to the complete registry and removes the runtime socket.

The daemon atomically writes reproducible registry metadata to
`$XDG_STATE_HOME/boomux/state.json`, falling back to
`~/.local/state/boomux/state.json`. Workspace and shell IDs, names, grouping,
shell working directories, startup commands, and last terminal profiles survive
restart. Recovered shells are pending: Boomux does not claim that arbitrary
processes, mutated environments, or PTYs survive daemon restart or crash.

`boomux daemon restart` transfers the existing listener and both ownership locks
to a replacement process through a private, versioned `SCM_RIGHTS` handshake.
Prepare/finalize acknowledgement keeps rollback safe before the irreversible
ownership boundary. Pending shells restore from metadata. Detached running
shells transfer their PTY master, pidfd-backed process identity, terminal
profile, and reconstructed VT state without changing the child PID. Attached
clients receive a reconnect request, acknowledge an input-ordering boundary,
and reconnect to the replacement while remaining in raw mode. Exited shells use
metadata recovery.

## Next Technical Steps

The detailed design, acceptance criteria, and manual test matrix are tracked in
[`native-terminal-follow-up.md`](native-terminal-follow-up.md).

1. Evaluate explicit exited-shell terminal-state transfer separately from live
   process handoff.

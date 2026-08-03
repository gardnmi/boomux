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

`src/main.rs` owns the CLI, project launcher, recipes, dashboard actions, shell
name resolution, and conversion from daemon snapshots into TUI view models.

### Protocol

`src/protocol.rs` defines the versioned wire model. Control messages are JSON
with a four-byte big-endian length prefix. Attachment traffic uses small binary
frames for input, output, resize, and detach events.

The domain has only two durable identities:

- A workspace groups shells under a name and working directory.
- A shell owns one PTY, child process, name, and workspace ID.

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

- Atomic multi-shell workspace creation
- Additional shell creation
- Workspace and shell snapshots
- Shell and workspace rename operations
- Shell and workspace closure
- Bounded output replay
- One writable attachment with explicit takeover
- PTY input and resize forwarding

Workspace creation stages every child before publishing any of them to the
registry. A failed spawn kills the staged children.

### Attachment

`src/attach.rs` runs inside the selected terminal emulator. It enables raw mode,
reports dimensions, and copies bytes in both directions without interpreting
keys or transforming live PTY output. An RAII guard restores terminal mode when
the attachment exits.

The daemon keeps a bounded output queue per active controller. A slow client
drops output rather than blocking the PTY reader and child process.

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
operations. Git information is still collected independently and cached.

### Agent Skill

The optional vendor-neutral Agent Skill teaches compatible clients to call
`boomux shells` and `boomux read`. `BOOMUX_SHELL_ID` provides current-shell
context while exact shell IDs remain globally addressable within the daemon.

## Runtime Semantics

Closing a terminal window closes only its socket attachment. The daemon retains
the PTY master and child. Reopening a window acquires the controller and first
receives retained raw output followed by live output.

Closing a shell terminates its child and disconnects its controller. Closing a
workspace removes all its shells from the registry before terminating them.
On Linux, cleanup signals every process still belonging to the shell's session
before reaping the session leader. `boomux daemon stop` applies the same cleanup
to the complete registry and removes the runtime socket.

Current persistence is deliberately limited to daemon lifetime. Boomux does not
yet write registry metadata or claim that arbitrary processes survive daemon
restart or crash.

## Next Technical Steps

1. Track terminal state with a VT parser and emit a sanitized reconnect snapshot.
2. Negotiate terminal capabilities when the first native attachment creates a
   shell.
3. Persist reproducible workspace metadata atomically under `$XDG_STATE_HOME`.
4. Add graceful daemon restart or live PTY handoff only after the base lifecycle
   is reliable.

# Architecture

## Product Boundary

Boomux is a session experience, not a terminal emulator or process
multiplexer. It maps one Herdr workspace to a named Boomux workspace and each
durable Herdr terminal to one native Ghostty window.

```text
Herdr server
└── workspace: project/feature
    ├── terminal A <-> herdr terminal attach <-> Ghostty window A
    ├── terminal B <-> herdr terminal attach <-> Ghostty window B
    └── terminal C <-> herdr terminal attach <-> Ghostty window C
```

## Why Compose First

Herdr owns PTYs in its background server. Its direct terminal attachment sends
the current rendered state and live ANSI frames, accepts terminal input, and
propagates terminal resize events. Disconnecting removes the controller without
terminating the server-owned process.

Ghostty's Linux `+new-window` action creates an independent top-level GTK
window and can execute `herdr terminal attach <terminal-id>` as that surface's
command. Closing the surface terminates the attachment command while leaving
the Herdr server and terminal alive.

These contracts provide the MVP without source-level integration.

## Components

### CLI

Provides diagnostics, creates named workspaces and terminals, and restores all
terminals belonging to a selected workspace.

### Herdr Client

Uses Herdr's JSON-producing CLI commands for workspace and terminal lifecycle.
Interactive terminal transport remains delegated to the official
`herdr terminal attach` command because Herdr's binary client protocol is
private and versioned.

### Picker

Runs in a normal terminal surface and lists named workspaces. Selecting one
launches a Ghostty window for every terminal belonging to that Herdr workspace.

### Dashboard

Provides a Ratatui overview of workspaces, terminals, directories, and agent
state. It is a control plane only: restoring a workspace still launches native
Ghostty windows rather than embedding terminal sessions in the dashboard. The
dashboard remains open after restoration so it can continue managing other
workspaces. It refreshes from Herdr four times per second and validates the
selected workspace against a fresh snapshot before launching Ghostty windows.
Closing a workspace uses Herdr's atomic workspace close command after explicit
confirmation, terminating every shell in that workspace. Shell creation uses
Herdr tabs, and pane labels provide durable shell names for the dashboard,
Ghostty titles, and prompt integrations.

Dashboard colors use semantic ANSI roles and the terminal's default foreground
and background. This keeps the TUI portable while allowing terminal-level theme
systems such as Omarchy to supply the concrete palette.

## Project Discovery

The dashboard loads global TOML configuration from the XDG config directory and
then merges an optional `BOOMUX_CONFIG` file over it. Project discovery walks
only explicitly configured roots to a bounded depth, recognizes Git worktrees
as well as ordinary repositories through their `.git` marker, and does not
descend into repositories after discovering them.

The resulting canonical paths and source-root labels are passed to the dashboard
as a sorted snapshot. Workspace creation uses a grouped type-to-filter launcher
over that snapshot; all groups share one query, and the selected project's
basename becomes the workspace name. Configuration loading and filesystem
discovery remain outside the TUI so input and rendering do not perform filesystem
scans.

### Ghostty Launcher

Creates native windows with stable human-readable titles. Boomux does not rely
on compositor-specific window IDs or control APIs.

## Known Constraints

- Herdr permits one writable controller per terminal; `--takeover` is explicit.
- Direct interactive attachment is currently Unix-only.
- Ghostty `+new-window` requires its Linux GTK build and session D-Bus.
- Ghostty windows commonly share one application process, although each is an
  independent top-level window.
- Window titles identify sessions for humans but are not durable machine IDs.
- Herdr sends rendered ANSI frames, not the original raw PTY output stream.

None of these constraints blocks the proposed user experience.

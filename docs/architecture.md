# Architecture

## Product Boundary

Boomux is a session experience, not a terminal emulator or process
multiplexer. It maps one Herdr workspace to a named Boomux workspace and each
durable Herdr terminal to one native terminal window.

```text
Herdr server
└── workspace: project/feature
    ├── terminal A <-> herdr terminal attach <-> terminal window A
    ├── terminal B <-> herdr terminal attach <-> terminal window B
    └── terminal C <-> herdr terminal attach <-> terminal window C
```

## Why Compose First

Herdr owns PTYs in its background server. Its direct terminal attachment sends
the current rendered state and live ANSI frames, accepts terminal input, and
propagates terminal resize events. Disconnecting removes the controller without
terminating the server-owned process.

Omarchy's `xdg-terminal-exec` integration resolves the default or explicitly
selected terminal desktop entry and translates common capabilities such as
command execution and titles into that emulator's arguments. Closing the
surface terminates the attachment command while leaving the Herdr server and
terminal alive.

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
launches a native window for every terminal belonging to that Herdr workspace.

### Dashboard

Provides a Ratatui overview of workspaces, terminals, directories, Git state,
and per-terminal agent state. It is a control plane only: restoring a workspace
still launches native terminal windows rather than embedding terminal sessions in
the dashboard. The dashboard remains open after restoration so it can continue
managing other workspaces. It refreshes from Herdr four times per second and
validates the selected workspace against a fresh snapshot before launching
terminal windows.
Repository name, branch, dirty state, and primary or linked worktree information
come from the Git CLI and are cached for two seconds so the faster Herdr refresh
does not repeatedly spawn Git processes.
Closing a workspace uses Herdr's atomic workspace close command after explicit
confirmation, terminating every shell in that workspace. Shell creation uses
Herdr tabs, and pane labels provide durable shell names for the dashboard,
window titles, and prompt integrations.

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

## Workspace Recipes

After project selection, the dashboard presents the built-in single-shell
default followed by validated recipes from Boomux's layered TOML configuration.
Each recipe defines one or more durable terminal names and optional startup
commands.

The first recipe terminal reuses the workspace root pane; later terminals use
Herdr tabs. Boomux labels every tab and pane, applies the workspace and shell
environment variables, and delivers configured startup commands through
`herdr pane run`. Herdr acknowledges command delivery rather than process
liveness. After Herdr returns the root workspace identity, provisioning is
transactional at the workspace boundary: later creation, labeling, or delivery
failures close the new Herdr workspace rather than leaving a partial recipe. A
successful mutation followed by an undecodable root response cannot be safely
rolled back because Boomux has no reliable workspace identity.

### Terminal Launcher

Resolves Omarchy's default terminal or a Boomux-specific XDG desktop entry and
creates native windows with human-readable titles when supported. Boomux does
not rely on compositor-specific window IDs or control APIs.

### Agent Skill

The repository contains a vendor-neutral Agent Skill that teaches compatible
agents to list workspace shells and read retained output through Boomux. The
binary embeds the same skill source and installs it only after an explicit user
command under `~/.agents/skills`. Name resolution uses the invoking Herdr pane
to stay within its Boomux workspace, while exact terminal IDs remain globally
addressable. Boomux delegates scrollback retrieval to `herdr pane read`.

## Known Constraints

- Boomux tracks the exact latest stable Herdr release pinned in `mise.toml`
  until its initial release; other versions are rejected before use.
- Herdr permits one writable controller per terminal; `--takeover` is explicit.
- Direct interactive attachment is currently Unix-only.
- Window launching requires Omarchy's `xdg-terminal-exec` and an installed
  terminal with compatible XDG desktop-entry metadata.
- Terminal capabilities such as stable titles vary by emulator.
- Window titles identify sessions for humans but are not durable machine IDs.
- Herdr sends rendered ANSI frames, not the original raw PTY output stream.

None of these constraints blocks the proposed user experience.

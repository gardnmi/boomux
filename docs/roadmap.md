# Roadmap

Boomux is under active development. This document tracks shipped capabilities
and possible directions for exploration; future ideas are not release
commitments.

## Completed

- [x] Manage daemon-owned workspaces and PTY shells from a live Ratatui
  dashboard.
- [x] Restore a whole workspace into native terminal windows or open only one
  selected terminal.
- [x] Create workspaces from a searchable Git project launcher grouped by
  configured roots.
- [x] Load layered TOML configuration from the XDG config directory and an
  optional `BOOMUX_CONFIG` override.
- [x] Add plain shells, assign durable shell names, and carry those names into
  the dashboard, window titles, and dynamic Starship prompts.
- [x] Close a workspace and all of its shells through an explicit confirmation.
- [x] Follow the active terminal theme through semantic ANSI colors.
- [x] Display repository, branch, dirty state, and primary or linked worktree
  information for each workspace.
- [x] Create workspaces from configurable multi-terminal recipes with a built-in
  single-shell default.
- [x] Validate runtime dependencies, configuration, and project discovery with
  `boomux doctor`.
- [x] Follow Omarchy's default terminal with persistent and per-invocation XDG
  desktop-entry overrides.
- [x] Let agents discover workspace shells and read retained output through a
  vendor-neutral Agent Skill and Boomux CLI commands.
- [x] Replace the external session backend with a Boomux-owned Unix daemon,
  socket protocol, PTY lifecycle, and transparent attachment client.

## Workspace Control

- Reconstruct reconnect state with a VT parser instead of replaying raw bytes.
- Persist reproducible workspace metadata under `$XDG_STATE_HOME`.
- Negotiate terminal capabilities when a shell receives its first attachment.
- Add graceful daemon restart or live PTY handoff.
- Aggregate multiple agent states as counts instead of one workspace-level
  value.
- Notify when an agent becomes blocked, finishes, or needs input.
- Show an opt-in, read-only preview of the selected terminal.
- Search workspaces and actions from a command palette.
- Launch workspace templates such as editor, agent, tests, and LazyGit.
- Duplicate a workspace structure for another branch or worktree.
- Archive inactive workspaces without terminating their shells.

## Desktop Integration

- Restore shells into optional Hyprland layout presets.
- Place workspace groups on selected Hyprland workspaces or monitors.
- Give each Boomux workspace a consistent border color.
- Focus an existing terminal attachment instead of opening another window.
- Offer the dashboard as an optional Hyprland special workspace.

## Agent Workflows

- Record state transitions and show when an agent last changed state.
- Provide an attention queue for blocked and completed agents.
- Send common responses or commands to a selected pane.
- Run hooks, tests, notifications, or focus actions when an agent finishes.

## Distribution And Polish

- Make the Ratatui interface the default and remove the Gum dependency.
- Add a LazyGit-style interactive keybinding panel.
- Support configuration for refresh rate and notifications.
- Package Boomux and its daemon lifecycle for Arch and Omarchy users.
- Validate compatible `xdg-terminal-exec` versions in `boomux doctor`.

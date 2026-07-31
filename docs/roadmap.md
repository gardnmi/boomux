# Product Ideas

Boomux is under active development. This document collects possible directions
for exploration; it is not a release commitment.

## Workspace Control

- Aggregate multiple agent states as counts instead of one workspace-level
  value.
- Notify when an agent becomes blocked, finishes, or needs input.
- Create shells, OpenCode, LazyGit, and custom commands from the dashboard.
- Name individual shells and carry those names into Ghostty titles and prompts.
- Show an opt-in, read-only preview of the selected terminal.
- Search workspaces and actions from a command palette.
- Display repository, branch, dirty state, and worktree information.
- Launch workspace templates such as editor, agent, tests, and LazyGit.
- Duplicate a workspace structure for another branch or worktree.
- Archive inactive workspaces without terminating their shells.

## Desktop Integration

- Restore shells into optional Hyprland layout presets.
- Place workspace groups on selected Hyprland workspaces or monitors.
- Give each Boomux workspace a consistent border color.
- Focus an existing Ghostty attachment instead of opening another window.
- Offer the dashboard as an optional Hyprland special workspace.

## Agent Workflows

- Record state transitions and show when an agent last changed state.
- Provide an attention queue for blocked and completed agents.
- Send common responses or commands to a selected pane.
- Define recipes for OpenCode, Claude, Codex, and custom agents.
- Run hooks, tests, notifications, or focus actions when an agent finishes.

## Distribution And Polish

- Make the Ratatui interface the default and remove the Gum dependency.
- Add a LazyGit-style interactive keybinding panel.
- Support configuration for recipes, refresh rate, and notifications.
- Package Herdr and Boomux for Arch and Omarchy users.
- Validate compatible Herdr and Ghostty versions in `boomux doctor`.

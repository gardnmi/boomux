# Roadmap

Boomux is under active development. This document tracks shipped capabilities
and possible directions for exploration; future ideas are not release
commitments.

## Completed

- [x] Manage daemon-owned workspaces and PTY shells from a live Ratatui
  dashboard.
- [x] Restore a whole workspace into native terminal windows or open only one
  selected terminal.
- [x] Create empty named workspaces from free text or searchable project-name
  suggestions grouped by configured roots, without associating project paths.
- [x] Assign shells created without a workspace to the next available
  `workspace-N` container.
- [x] Load layered TOML configuration from the XDG config directory and an
  optional `BOOMUX_CONFIG` override.
- [x] Add plain shells, assign durable shell names, and carry those names into
  the dashboard, window titles, and dynamic Starship prompts.
- [x] Rename focused workspaces and shells from the dashboard.
- [x] Close a workspace and all of its shells through an explicit confirmation.
- [x] Follow the active terminal theme through semantic ANSI colors.
- [x] Display repository, branch, dirty state, and primary or linked worktree
  information when a workspace's shells share a directory.
- [x] Validate runtime dependencies, configuration, and project discovery with
  `boomux doctor`.
- [x] Follow Omarchy's default terminal with persistent and per-invocation XDG
  desktop-entry overrides.
- [x] Let agents discover workspace shells and read retained output through a
  vendor-neutral Agent Skill and Boomux CLI commands.
- [x] Replace the external session backend with a Boomux-owned Unix daemon,
  socket protocol, PTY lifecycle, and transparent attachment client.
- [x] Exercise native PTY transport, detach, takeover, resize, process cleanup,
  startup locking, and graceful shutdown through an isolated binary-level test.
- [x] Make the Ratatui dashboard the default interface and remove the external
  picker dependency.
- [x] Run an explicit one-off command when creating a shell from a path.
- [x] Start pending shells on first attachment using its terminal environment,
  cell dimensions, and pixel dimensions.
- [x] Expose workspace and shell create, inspect, rename, and close operations
  through explicit CLI command groups.
- [x] Reconstruct reconnect state with a shadow VT parser and expose plain
  rendered scrollback through `boomux read`.
- [x] Persist reproducible workspace and shell metadata atomically under
  `$XDG_STATE_HOME`; restore shells as pending after daemon restart.
- [x] Preserve running shells and reconnect active terminal clients across a
  transactional graceful daemon restart.

## Workspace Control

See [`native-terminal-follow-up.md`](native-terminal-follow-up.md) for the
terminal handshake, VT reconstruction, and restart-persistence plan.

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

Build agent orchestration on explicit process identity and observable daemon
state rather than parsing human CLI tables or treating a durable shell as one
eternal process.

### Foundation

1. [Complete] Add a `ShellRun` identity beneath each durable shell, including
   generation, lifecycle timestamps, exit reason, output revision, and
   `BOOMUX_RUN_ID`.
2. [Complete] Preserve final exited-run metadata and terminal state across
   graceful daemon restart without starting a replacement process implicitly.
3. [Complete] Add stable versioned JSON output, typed errors, and capability
   reporting for integrations.
4. [Complete] Add a monotonic daemon event stream with reconnectable cursors and
   revision-aware output reads.
5. [Complete] Route runtime transitions through one coordinator so persistence
   and events share an ordering boundary.

### Agent Runtime

1. [Complete] Model agent instances separately from shells and runs, with
   explicit state authority, evidence, and confidence.
2. [Complete] Establish authority precedence and explicit OpenCode lifecycle
   integration for `working`, `blocked`, `idle`, and explicit completion.
3. Add process adapters beneath lifecycle-integration authority.
4. Add conservative terminal-screen heuristics beneath process-adapter
   authority, without inferring completion from quiet output or shell exit.
5. Aggregate agent states as workspace counts and provide an explainable,
   persistent attention queue for blocked and completed work.
6. Add notifications and revision-aware `agent wait` and `agent read` commands.
7. Add guarded prompts and common responses only after defining run-scoped
   leases, user-controller precedence, idempotency, and audit events.
8. Run hooks, tests, notifications, or focus actions from durable transitions.

## Distribution And Polish

- Add a LazyGit-style interactive keybinding panel.
- Support configuration for refresh rate and notifications.
- Package Boomux and its daemon lifecycle for Arch and Omarchy users.
- Validate compatible `xdg-terminal-exec` versions in `boomux doctor`.

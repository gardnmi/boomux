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
- [x] Start pending shells on first attachment using its ephemeral full Unix
  environment, cell dimensions, and pixel dimensions without persisting it.
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
6. [Complete] Make installed-binary upgrades restart-safe by falling back to the
   daemon's absolute invocation path when its replaced `/proc/.../exe` target is
   marked deleted.

### Agent Runtime

1. [Complete] Model agent instances separately from shells and runs, with
   explicit state authority, evidence, and confidence.
2. [Complete] Establish authority precedence and explicit OpenCode lifecycle
   integration for `working`, `blocked`, `idle`, and explicit completion.
3. [Complete] Add an explicit-session process supervisor that preserves exact
   argv and inherited stdio, propagates child exit, reports only `unknown`
   process start/exit evidence, and fails open when Boomux reporting fails.
4. [Complete] Project run-scoped Agent instances into globally discoverable
   session metadata with stable list/inspect JSON and human CLI commands.
5. [Complete] Capture and persist each Agent's registration-time working
   directory so canonical transcript lookup survives shell removal and cold
   daemon restart while retained-shell metadata remains semantically accurate.

### Near-Term Priorities

1. [Partially validated; see `lifecycle-validation.md`] Validate the OpenCode and Pi lifecycle integrations during normal work before
   expanding the observation model. Confirm reload identity, session switching,
   root/subagent aggregation where available, blocked prompts, idle transitions,
   explicit completion semantics, and graceful daemon replacement against real
   sessions.
2. [In progress] Build a first-class integration setup workflow that discovers supported
   installed harnesses, explains why authoritative lifecycle access is required,
   previews and obtains consent for configuration changes, installs or updates
   each integration safely, prompts for required harness reloads, and verifies
   canonical identity plus lifecycle reporting end to end. Include status,
   version compatibility, diagnostics, repair, and uninstall paths so stronger
   guarantees do not require users to manage plugin files manually.
   Unified list, status, host-version detection, runtime registration diagnosis,
   and atomic single/all installation are complete; guided verification,
   consented previews, repair guidance, and uninstall remain.
3. [Complete] Aggregate agent states into workspace counts and add an explainable,
   persistent attention queue for blocked and completed work.
4. [Complete] Add revision-aware `agent wait` so scripts can await durable state
   transitions without polling human output.
5. [Complete] Add deduplicated notifications for blocked and completed
   transitions after the attention and wait semantics are established.
6. [Complete] Add explicit cross-harness transcript and tool inspection so Pi can read an OpenCode
   session and OpenCode can read a Pi session. Do not treat bounded rendered
   terminal output as the full session: host adapters must expose canonical
   session identity, bounded messages and tool activity, versioned
   capabilities, and an explicit access policy. Boomux trusts the harness as the
   content boundary and does not apply another redaction pass. Host-specific
   canonical lookup and normalization live behind a shared adapter registry so
    future Claude Code, Codex, and other harness support reuses the same identity,
    bounds, errors, and output contract. Opaque stateless cursors page toward
    older entries while expiring on baseline or source-context changes.
7. [Complete] Add canonical Agent Session context to selected durable Agent rows
   in the Boomux UI without attaching directory-wide history to untracked
   process hints. Keep full inactive and completed workspace history available
   through the session CLI with integration, canonical identity, associated
   shell and run, lifecycle state, and timestamps. Bounded host catalogs also project pre-registration OpenCode
   root-session history without fabricated Agent occurrences, while asynchronous
   adapters provide OpenCode generated titles and Pi names or first-user-message summaries; future adapters
   also provide canonical transcripts and tool activity through bounded,
   versioned CLI output.
8. [Complete] Separate OpenCode and Pi title and transcript adapters from shared
   cache, pagination, cursor, and output policy so future harness support can be
   added through isolated modules and explicit capability registries.

### Deferred Agent Work

- Defer automatic and integration-specific process discovery until a target can
  provide canonical external-session identity without guessing from PID, argv,
  database recency, or API activity. The explicit-session supervisor remains the
  safe fallback.
- Defer terminal-screen heuristics until real usage demonstrates a visibility
  gap that lifecycle integrations and explicit process evidence do not cover.
  Heuristics must remain lower-authority, visibly identified observations rather
  than silently replacing setup for canonical identity and lifecycle evidence.
- Defer guarded prompts and common responses until run-scoped leases,
  user-controller precedence, idempotency, and audit events are defined.
- Defer hooks, tests, focus actions, and other automatic reactions until waits,
  notification deduplication, and durable transition semantics are proven.

## Distribution And Polish

- Add a LazyGit-style interactive keybinding panel.
- Support configuration for refresh rate.
- Package Boomux and its daemon lifecycle for Arch and Omarchy users.
- Validate compatible `xdg-terminal-exec` versions in `boomux doctor`.

# Herdr Integration And Replacement

## Overview

Boomux is currently a control plane around Herdr. Herdr provides almost the
entire persistent-terminal substrate; Boomux adds project and workspace UX,
recipes, dashboard views, native terminal launching, and agent-facing commands.

Herdr's background server owns:

- PTYs and child processes
- Persistent terminal state
- Workspaces, tabs, panes, and terminal identities
- Retained scrollback
- Interactive attachment
- Input and resize forwarding
- Writable-controller takeover
- Agent detection and status metadata

Boomux communicates with Herdr in two ways:

1. Management commands invoke the Herdr CLI and parse JSON responses.
2. Interactive windows run `herdr terminal attach <terminal-id>`.

Boomux does not currently own PTYs, persist process state, or implement a
terminal transport protocol.

## Workspace Creation

Running `boomux .` creates a Herdr workspace and its root terminal with a
command equivalent to:

```console
herdr workspace create \
  --cwd <directory> \
  --label <workspace> \
  --env BOOMUX_WORKSPACE=<workspace> \
  --env BOOMUX_SHELL_NAME=shell-1 \
  --focus
```

Herdr creates the workspace, root tab, pane, PTY, shell process, and terminal
attachment identity. Boomux parses the returned root pane in `src/main.rs`.

Additional shells use:

```console
herdr tab create --workspace <workspace-id> ...
```

Boomux assigns durable names with:

```console
herdr tab rename <tab-id> <name>
herdr pane rename <pane-id> <name>
```

Recipe startup commands use:

```console
herdr pane run <pane-id> <command>
```

Herdr confirms command delivery, not that the resulting process started
successfully or remains alive.

## Identity Model

Herdr exposes four identities that Boomux currently carries directly:

| Identity | Boomux usage |
| --- | --- |
| Workspace ID | Grouping and atomic workspace closure |
| Tab ID | Independent shell creation and cleanup |
| Pane ID | Naming, commands, scrollback, and current context |
| Terminal ID | Interactive attachment and user-facing references |

The `Pane` model in `src/main.rs` contains all four, plus cwd, label, agent,
and agent status.

Herdr can expose different forms of the same identity at different boundaries.
For example, a process may inherit `HERDR_PANE_ID=p_10`, while `herdr pane list`
reports the canonical pane as `w652f705dfae541-3`. Boomux resolves the runtime
identifier through `herdr pane get` before using the canonical workspace
metadata.

## Persistence And Attachment

When Boomux opens a native terminal window, `src/terminal.rs` asks Omarchy which
terminal emulator to use. The process running inside that emulator is still:

```console
herdr terminal attach <terminal-id> --takeover
```

Herdr's attachment client handles:

- Initial rendered terminal state
- Live ANSI frames
- Keyboard input
- Resize events
- Disconnects
- Controller ownership

Closing the native terminal window only terminates the attachment client. The
PTY and process remain alive in the Herdr server. This attachment path is the
hardest part of Herdr to replace.

## Dashboard And Metadata

The dashboard receives its state from:

```console
herdr workspace list
herdr pane list
```

Boomux polls these commands four times per second and converts the responses to
TUI view models. `src/tui.rs` does not invoke Herdr directly, which is a useful
existing replacement seam.

Herdr also supplies:

- Foreground process information
- Detected agent type
- Agent status such as `working`, `blocked`, or `idle`
- Durable pane labels used as Boomux shell names

## Agent Shell Reading

`boomux read` delegates scrollback retrieval to:

```console
herdr pane read <pane-id> \
  --source recent-unwrapped \
  --lines 200 \
  --format text
```

Boomux resolves the shell name and prints Herdr's output. It does not maintain
its own scrollback store.

## Current Herdr Command Surface

Boomux currently depends on approximately fourteen Herdr command shapes:

- `herdr workspace create`
- `herdr workspace list`
- `herdr workspace close`
- `herdr tab create`
- `herdr tab rename`
- `herdr tab close`
- `herdr pane list`
- `herdr pane get`
- `herdr pane read`
- `herdr pane run`
- `herdr pane rename`
- `herdr pane process-info`
- `herdr terminal attach`
- `herdr --version`

Most calls are concentrated in `src/main.rs`, but there is no backend interface
yet. Herdr response types, native IDs, orchestration, and command execution are
mixed with Boomux application logic.

During pre-release development, Boomux intentionally supports only the latest
stable Herdr release. The exact current version is pinned in `mise.toml`,
checked at runtime, and reported by `boomux doctor`. Adopting a newer release
requires deliberately bumping the pin and updating Boomux to its CLI contract.

## Distribution And Licensing

The pinned Herdr release, `v0.7.5`, declares `AGPL-3.0-or-later` in both its
`LICENSE` and `Cargo.toml` files and offers a separate commercial license. The
Herdr repository's current default branch uses Apache-2.0, but that does not by
itself replace the license included with the tagged `v0.7.5` release. Boomux
must evaluate and distribute the exact version it pins under that version's
license unless Herdr's copyright holder confirms different terms.

As of August 3, 2026, Herdr has not published an Apache-2.0 release. The
relicensing commit followed `v0.7.5`, while the default branch remains versioned
as `0.7.5` and is not available through Herdr's normal mise release backend.
Boomux will not pin that unreleased branch. Upgrade Herdr only after a tagged
Apache-2.0 release is available, then validate its protocol and every command
shape listed above before changing `mise.toml`.

The current development setup does not redistribute Herdr. `mise.toml` names
the required version, and users install Herdr independently. Keep that model
for the initial Boomux release: publish Boomux separately, describe Herdr as an
external prerequisite, and do not include a Herdr executable in a Boomux
archive, package, or installer payload.

If Boomux later redistributes an unmodified Herdr executable under the AGPL,
the release process must at minimum:

- Identify Herdr, its exact version, and its `AGPL-3.0-or-later` license.
- Include Herdr's copyright, license, and warranty notices.
- Provide equivalent access to the complete corresponding source at no extra
  charge, with clear directions next to the executable download.
- Keep that source available for as long as the executable is distributed;
  mirroring the exact source and build material is safer than relying only on
  an upstream tag.
- Preserve recipients' AGPL rights without adding conflicting restrictions.
- Mark and publish any Herdr modifications under the AGPL. If a modified Herdr
  supports remote network interaction, also satisfy AGPL section 13's source
  offer requirement.

Separate executables can qualify as an aggregate under AGPL section 5, so
including Herdr does not automatically license every independent work in the
same archive under the AGPL. That boundary is fact-specific, however. Boomux
is designed around Herdr's command protocol and control flow, and Boomux does
not yet declare its own license. Do not rely on the aggregate exception for a
combined commercial or proprietary distribution without legal review.

Before shipping a combined package, choose one of these release gates:

1. Obtain written confirmation that the packaged Herdr release is available
   under Apache-2.0 or another compatible license.
2. Obtain a commercial license from Herdr at `hey@herdr.dev`.
3. Complete an AGPL compliance review covering the package boundary,
   corresponding source, notices, modifications, and Boomux's own license.

This section is engineering guidance for release planning, not legal advice.

## Reusable Parts

The following areas are already mostly independent of Herdr:

- Dashboard rendering and interactions in `src/tui.rs`
- Git metadata in `src/git.rs`
- Project discovery in `src/projects.rs`
- Recipes and configuration in `src/config.rs`
- Omarchy terminal selection in `src/terminal.rs`, after attachment generation
  is generalized
- The Agent Skill's user-facing `boomux shells` and `boomux read` interface

The tightly coupled areas are:

- Herdr JSON response types and command execution in `src/main.rs`
- Workspace, tab, pane, and terminal IDs exposed throughout application logic
- `HERDR_PANE_ID` as the current-shell context
- `herdr terminal attach` as the interactive transport
- Herdr pane labels as Boomux's durable shell names
- Herdr's agent detection and status values
- Herdr as the only owner of workspace and shell state

## Replacement Options

Estimates assume one experienced Rust and Linux terminal engineer, including
tests and documentation but excluding broad cross-platform support.

| Replacement | Realistic effort | Main compromise |
| --- | ---: | --- |
| Basic tmux prototype | 2-3 weeks | Missing agent metadata and exact attachment semantics |
| Production-quality tmux backend | 5-9 weeks | Requires a Boomux registry and behavioral translation |
| Zellij adapter | 6-10 weeks | Poor fit for one pane per native window |
| WezTerm mux | 4-7 weeks | Loses terminal-emulator neutrality |
| Minimal custom PTY daemon | 8-12 weeks | Not feature-compatible yet |
| Custom Herdr-equivalent backend | 4-7 months | Significant terminal and server engineering |
| Mature custom backend | 8-12 months | Recovery, security, protocol stability, and testing |

## Tmux As The Practical Alternative

A tmux backend could map each Boomux shell to an independent tmux session:

| Boomux need | tmux mechanism |
| --- | --- |
| Persistent process | tmux session |
| Create shell | `tmux new-session` |
| Attach | `tmux attach-session` |
| Run command | Startup command or `send-keys` |
| Scrollback | `tmux capture-pane` |
| Name | Session name or user option |
| Close | `tmux kill-session` |
| Process metadata | tmux format fields |

Tmux does not directly provide:

- Herdr's workspace grouping
- Stable separate pane and terminal identities
- Exclusive-controller takeover
- Structured JSON responses
- Agent detection and status
- Atomic multi-shell workspace closure
- Identical unwrapped scrollback behavior

Boomux would need its own persistent registry to group tmux sessions into one
workspace and preserve stable product identities.

Using one tmux session per Boomux workspace appears more natural, but attaching
separate native terminal clients to selected windows can expose shared-session
current-window and layout behavior. One tmux session per Boomux shell avoids
much of that mismatch at the cost of moving workspace grouping into Boomux.

## Custom Backend Requirements

Opening a PTY is not the difficult part. A real Herdr replacement must provide:

1. Background daemon lifecycle
2. PTY allocation and process supervision
3. Stable persistent identities
4. A versioned Unix-socket protocol
5. Attach and reconnect clients
6. Input, resize, and backpressure handling
7. Controller ownership and takeover
8. Terminal-state parsing
9. Bounded scrollback and plain-text extraction
10. Crash recovery and metadata persistence
11. Workspace transactions
12. Foreground process inspection
13. Agent detection and status reporting

The difficult areas are correct terminal state, attachment and reconnection,
resize handling, backpressure, and recovery semantics. This is why a custom
backend is a multi-month project rather than a library swap.

## Migration Constraints

Running Herdr processes cannot realistically be moved into tmux or a custom
daemon. Their controlling PTYs belong to Herdr.

A migration therefore needs a drain period:

- Existing workspaces remain under Herdr.
- New workspaces can use the replacement backend.
- Both backend types appear in the Boomux dashboard.
- Users recreate old workspaces when convenient.
- Herdr is removed after its final live workspace closes.

Labels, directories, and grouping can be imported. The following cannot be
reliably migrated:

- Running jobs
- Shell state and environment mutations
- Agent internal state
- Complete terminal history
- Exact rendered cell state and alternate-screen state

Herdr-owned workspace, tab, pane, and terminal IDs will also change. A future
multi-backend Boomux should use stable Boomux IDs and retain backend-native IDs
as implementation details.

## Recommended Decoupling Sequence

### 1. Capture The Existing Contract

- Add fixtures for every Herdr JSON response.
- Test missing, extra, and null fields and command failures.
- Add command-construction tests for every Herdr command shape.
- Parse `pane process-info` structurally instead of matching a string.
- Make `boomux doctor` verify required Herdr capabilities or versions.

### 2. Extract A Herdr Backend

Move Herdr-specific code into a dedicated backend module that owns:

- `Command::new("herdr")`
- Herdr JSON response types
- Error and exit-status conversion
- Command argument construction
- Current Herdr context resolution
- Attachment command generation

Application and TUI code should not deserialize Herdr responses directly.

### 3. Add Backend-Neutral Domain Types

Introduce Boomux-owned workspace and shell models. Keep Herdr's workspace, tab,
pane, and terminal IDs inside an opaque backend reference rather than exposing
them throughout the application.

### 4. Add A Capability-Aware Backend Interface

A backend contract should cover:

- Health and capabilities
- Workspace and shell snapshots
- Current-shell resolution
- Workspace and shell creation
- Rename and close operations
- Command delivery
- Scrollback reading
- Attachment command generation

Capabilities should explicitly describe support for scrollback, unwrapped
output, agent metadata, process metadata, exclusive controllers, takeover,
atomic workspace close, and durable labels.

### 5. Generalize Terminal Attachment

Change `src/terminal.rs` to accept an executable attachment specification rather
than a Herdr terminal ID. The terminal launcher should only wrap that command
through `xdg-terminal-exec`.

### 6. Introduce Backend-Neutral Context

Inject stable context such as:

```text
BOOMUX_BACKEND=herdr
BOOMUX_WORKSPACE_ID=<stable-boomux-id>
BOOMUX_SHELL_ID=<stable-boomux-id>
```

Prefer these values while temporarily falling back to `HERDR_PANE_ID` for old
workspaces.

### 7. Add A Small Boomux Registry

Store:

- Stable Boomux workspace and shell IDs
- Backend kind and backend-native IDs
- Labels and grouping
- Working directories
- Creation recipes
- Migration aliases

Herdr can remain the source of live process truth while no longer being the
sole owner of Boomux product identity.

### 8. Build A Tmux Proof Of Concept

Validate the hardest contracts before committing to migration:

1. Process survival after all windows close
2. One shell per native terminal window
3. Correct input and resize behavior
4. Independent simultaneous windows
5. Scrollback quality
6. Recipe startup
7. Close and crash recovery

### 9. Support A Dual-Backend Drain

Allow Herdr and the replacement backend to coexist. Existing Herdr workspaces
remain attachable while new workspaces opt into the new backend. Offer to
recreate a workspace under the new backend rather than claiming to migrate live
processes.

## Recommendation

Do not begin with a custom PTY daemon. First extract the Herdr adapter,
introduce backend-neutral identities and context, and generalize attachment
generation. This work should take roughly one to two engineer-weeks and is
valuable even if Herdr remains the permanent backend.

After that extraction, prototype tmux behind the backend interface. Use the
prototype to test whether tmux can preserve Boomux's defining experience: one
independently persistent shell per native terminal window. Consider a custom
backend only if adapter-based attachment semantics cannot satisfy that product
contract.

# Boomux

**Persistent native-terminal workspaces for organizing shells, commands, and
coding agents across projects.**

> [!WARNING]
> Boomux is an early proof of concept. Commands, storage, and session behavior
> may change without migration support.

Boomux is a persistent workspace manager for native terminal windows. It keeps
shells and commands alive after their windows close, groups related work into
restorable workspaces, and shows the lifecycle of supported coding agents
without replacing your terminal emulator.

Your terminal still owns rendering, fonts, selection, clipboard behavior, and
window chrome. Boomux owns the PTY, process lifetime, workspace grouping,
attachments, and durable runtime metadata.

![Boomux workspace dashboard showing shell, command, agent, and launcher kinds](assets/dashboard-workspaces.png)

_The workspace view keeps every kind of work in one place. IDs remain available
for exact CLI operations, but daily navigation uses workspace and item names._

## Why Boomux

- Close a terminal window without killing its shell or foreground process.
- Restore a project workspace instead of rebuilding a collection of windows.
- Run login shells, long-lived commands, desktop launchers, and coding agents
  under one durable model.
- See whether OpenCode or Pi is working, blocked, idle, inactive, or merely an
  untracked foreground process.
- Keep using Ghostty, Alacritty, or another XDG terminal as an ordinary native
  window rather than moving into a pane-based terminal UI.

## Install

Boomux currently targets Unix/Linux and is developed on Omarchy. Install the
Rust toolchain, then install directly from the repository:

```console
cargo install --git https://github.com/gardnmi/boomux --locked
boomux doctor
```

`boomux doctor` checks the daemon, terminal launcher, optional desktop
notifications, and coding-agent integrations. To build a checkout instead:

```console
cargo install --path . --locked
```

## First Five Minutes

From a project directory, create a named workspace and its first login shell:

```console
boomux . --name my-project
```

You are now attached to a Boomux-owned PTY. Start a program, then close the
terminal window. The shell and program continue running in the background.

From a fresh host terminal, open the dashboard:

```console
boomux
```

Select `my-project` and press `Enter` to reopen its shell. Add another native
terminal window without leaving the current one with:

```console
boomux . --name my-project --new
```

Add a durable exact command instead of a login shell by placing its arguments
after `--`:

```console
boomux . --name my-project --new -- cargo watch -x test
```

Boomux executes that argument vector directly. Use an explicit shell such as
`sh -lc` only when you need pipes, redirects, globbing, or variable expansion.

## Mental Model

| Term | Meaning |
| --- | --- |
| **Workspace** | A durable named container for related shells, commands, agents, and launchers. |
| **Shell** | A durable terminal slot. Its PTY and process can outlive every attached window. |
| **Run** | One process incarnation of a shell. Reopening an exited shell starts a new run on the same shell identity. |
| **Attachment** | A native terminal window currently reading from and optionally controlling a shell. Closing it does not close the shell. |
| **Launcher** | A stored desktop command invoked when a workspace opens. It has no PTY or retained output. |
| **Agent** | Lifecycle information reported by an integration for an external coding-agent session bound to an exact shell run. |
| **Session** | Projected OpenCode or Pi history that can be inspected through the CLI; it is not terminal scrollback. |

### Persistence Boundaries

| Event | Managed process | Workspace metadata |
| --- | --- | --- |
| Terminal window closes | Keeps running | Preserved |
| Dashboard quits | Keeps running | Preserved |
| `boomux daemon restart` | Handed off live | Preserved |
| Unexpected daemon exit or host reboot | Cannot remain live | Restored as pending |
| Workspace is explicitly closed | Terminated | Removed |

### Dashboard Kinds

`KIND` describes how an item behaves, not merely which process happens to be in
the foreground:

| Kind | What it represents | What happens on exit |
| --- | --- | --- |
| `shell` | A login shell. Programs run as children of that shell. | `Ctrl-C` normally returns to the prompt; exiting the login shell ends the run. |
| `command` | One exact PTY-backed argument vector, such as `cargo watch`. | Interrupting or exiting the primary command ends the run. |
| `agent` | A shell or command whose current run is presenting an active coding agent. This replaces the underlying row rather than duplicating it. | The shell keeps its identity; lifecycle state comes from the integration, not terminal text. |
| `launcher` | A detached command such as an editor or browser, run when the workspace is explicitly opened. | Boomux does not retain its output, invocation history, or process lifetime. |

Counts are exclusive by visible presentation. When an OpenCode-backed shell is
shown as `agent`, it is not also counted as a `shell` or `command`.

Shell and command status is `pending`, `running`, or `exited`. Agent status may
be `unknown`, `working`, `blocked`, `idle`, `inactive`, or `done`. `untracked`
means Boomux sees a supported foreground host but has not received authoritative
lifecycle reporting; it never guesses `idle` from quiet terminal output.

![Boomux agents view showing tracked and untracked coding-agent shells](assets/dashboard-agents.png)

_The Agents view aggregates current agent presentations across workspaces and
keeps untracked foreground hints visibly distinct from integrated lifecycle
state._

## Coding-Agent Setup

Boomux includes guided setup for OpenCode and Pi:

```console
boomux integration setup opencode
# or
boomux integration setup pi
```

Setup shows host, asset, and runtime status; previews the exact file action;
asks before changing anything; and prints restart and verification guidance.
After restarting the host, launch it inside a Boomux-managed shell and verify
reporting from another terminal:

```console
boomux integration verify opencode --wait-ms 30000
```

If several matching shells are running, Boomux lists each workspace and shell
with a ready-to-run command containing its exact shell ID.

## Everyday Workflows

| Goal | Command |
| --- | --- |
| Create a generated workspace and attach | `boomux .` |
| Create or add to a named workspace | `boomux . --name feature-x` |
| Open in a new native terminal | `boomux . --name feature-x --new` |
| Run one exact command | `boomux . --name feature-x --new -- lazygit` |
| Choose a terminal explicitly | `boomux . --terminal Alacritty.desktop` |
| Open the dashboard | `boomux` or `boomux ui` |

Without `--name`, each invocation creates the next `workspace-N`. With
`--name`, Boomux adds a shell to an existing exact-name workspace or creates
that workspace. `--terminal` implies `--new`; terminal selection uses the CLI
override, then Boomux configuration, then Omarchy's default.

### Dashboard Controls

| Key | Action |
| --- | --- |
| `Tab`, `Shift-Tab`, `1`-`5` | Change view. |
| `h`, `l`, left, right | Move between workspace and item tables. |
| `j`, `k`, up, down | Navigate rows. |
| `Enter` | Restore a workspace, open a shell, or invoke a launcher. |
| `a` | Create a workspace or add a shell, depending on focus. |
| `e` | Rename the selected workspace, shell, or launcher. |
| `x`, then `y` | Close or remove the selected item. |
| `PageUp`, `PageDown`, `Home`, `End` | Browse retained shell preview output. |
| `r` | Refresh immediately. |
| `q`, `Esc` | Quit the dashboard. |

The selected item receives a read-only contextual preview. Shells can show up
to 16 retained terminal rows; commands show exact arguments and run metadata;
launchers explain that output is not retained; and integrated agents show their
current evidence and matching canonical session. Closing the dashboard or a
terminal attachment does not stop managed shells. Closing a workspace does.

## CLI Reference

<details>
<summary>Complete command map</summary>

```console
boomux ui
boomux doctor
boomux capabilities [--json]
boomux list
boomux shells
boomux read <shell-name-or-shell-id> [--lines <count>]
boomux events [--after <cursor>] [--limit <count>] [--wait-ms <milliseconds>]
boomux close <shell-name-or-shell-id>
boomux open <shell-id> [--title <title>] [--takeover]
boomux workspace list
boomux workspace create <name>
boomux workspace open <name-or-id>
boomux workspace inspect <name-or-id>
boomux workspace rename <name-or-id> <new-name>
boomux workspace close <name-or-id>
boomux shell create <workspace-name-or-id> [--name <name>] [--cwd <path>] [-- <command>...]
boomux shell inspect <shell-name-or-id> [--workspace <name-or-id>]
boomux shell rename <shell-name-or-id> <new-name> [--workspace <name-or-id>]
boomux shell close <shell-name-or-id> [--workspace <name-or-id>]
boomux launcher list --workspace <name-or-id>
boomux launcher create <name> --workspace <name-or-id> [--cwd <path>] -- <command>...
boomux launcher inspect <launcher-name-or-id> [--workspace <name-or-id>]
boomux launcher rename <launcher-name-or-id> <new-name> [--workspace <name-or-id>]
boomux launcher remove <launcher-name-or-id> [--workspace <name-or-id>]
boomux agent list [--workspace <name-or-id>]
boomux agent inspect <agent-id>
boomux agent wait <agent-id> --after-revision <revision> [--wait-ms <milliseconds>]
boomux agent register <name> --integration <integration> [--external-session-id <id>] [--shell-id <shell-id>] [--run-id <run-id>] --state <state> --authority <authority> --evidence <evidence> --confidence <0-100>
boomux agent ensure <name> --integration <integration> --external-session-id <id> [--shell-id <shell-id>] [--run-id <run-id>] --state <state> --authority <authority> --evidence <evidence> --confidence <0-100>
boomux agent supervise <name> --integration <integration> --external-session-id <canonical-root-id> [--shell-id <shell-id>] [--run-id <run-id>] -- <command>...
boomux agent report <agent-id> [--shell-id <shell-id>] [--run-id <run-id>] --state <state> --authority <authority> --evidence <evidence> --confidence <0-100>
boomux attention list [--workspace <name-or-id>]
boomux attention acknowledge <agent-id> --observation-revision <revision>
boomux session list [--workspace <name-or-id>]
boomux session inspect <session-id>
boomux session read <session-id> [--before <cursor>] [--limit <entries>] [--max-bytes <bytes>]
boomux integration list
boomux integration status [opencode|pi]
boomux integration install <opencode|pi> [--force] [--dry-run]
boomux integration install --all [--force] [--dry-run]
boomux integration uninstall <opencode|pi> [--force]
boomux integration uninstall --all [--force]
boomux integration setup <opencode|pi> [--yes] [--force]
boomux integration verify <opencode|pi> [--shell <shell-id>] [--wait-ms <milliseconds>]
boomux daemon status
boomux daemon restart
boomux daemon stop
boomux skill install [--force]
boomux opencode install [--force]
boomux pi install [--force]
```

</details>

## Detailed Behavior

<details>
<summary>Shell, launcher, agent, session, and JSON semantics</summary>

`boomux shells` lists shells in the current workspace. `boomux read` and
`boomux close` resolve shell names within that workspace; exact shell IDs work
from anywhere. A shell cannot close itself through the CLI.

The `workspace` and `shell` command groups expose explicit lifecycle operations
for scripts and integrations. `shell create` records a pending shell; its PTY
and process start when the shell is first opened. Shell names require current
workspace context or `--workspace`; IDs remain globally addressable.
Explicitly opening an exited shell, including through dashboard `Enter` or
`workspace open`, starts its stored command as a new run on the same durable
shell identity. Retained output remains readable until that reopen.
New workspace and shell names are limited to 256 UTF-8 bytes so retained event
payloads remain bounded. Existing persisted names remain loadable.

Workspace launchers are durable commands that run on every explicit workspace
open, including dashboard `Enter` and `boomux workspace open`. They are detached
client-side processes without PTYs, retained output, or shell-run history. A
launcher-only workspace is valid. Commands are exact argument vectors, `--cwd`
defaults to the current directory, and multiple launchers run in creation order
before terminal windows open:

```console
boomux workspace create boomux
boomux launcher create editor --workspace boomux --cwd . -- zeditor .
boomux launcher create browser --workspace boomux -- firefox http://localhost:3000
boomux workspace open boomux
```

Opening continues after individual launcher or terminal spawn failures and
reports all failures at the end. Removing a launcher affects future opens only;
Boomux does not track or terminate applications it previously launched.

Protocol 11 adds the explicit exited-shell restart used by open and restore.
Low-level attachment can still inspect a completed run without restarting it.

Agent instances are durable records for external agent sessions. Each instance
has its own exact ID and is bound to exactly one shell run, not merely to a
durable shell. Protocol 10 adds `agent ensure`, an idempotent registration keyed
by integration, external session ID, shell ID, and run ID. It requires
`--external-session-id`; an existing match is returned unchanged, including
after an integration or daemon reload, while a different shell run creates a
different identity. `register`, `ensure`, and `report` require explicit
`unknown`, `working`, `blocked`, `idle`, `inactive`, or `done` state plus authority,
evidence, and confidence.

External reports use this precedence: `lifecycle-integration` over
`process-adapter` over `terminal-heuristic`. A lower-authority report is a
successful no-op. At equal authority, an exact duplicate is a no-op, while any
changed state, evidence, or confidence replaces the observation and increments
its revision. `daemon-lifecycle` exists in snapshots but is reserved for daemon
observations and cannot be supplied to public mutation commands. A `done` report
completes the instance permanently. Retrying the exact completion is an
idempotent success; a different later report is rejected. Completed instances
remain inspectable across daemon restart. Protocol 14 adds `agent wait`: callers
supply the last observed revision and can block until a newer durable observation
is accepted without polling. Protocol 15 records accepted blocked and completed
observations in a durable, blocked-first attention queue. Each item retains the
exact raising evidence, authority, confidence, revision, and timestamp until it
is conditionally acknowledged; later working or idle activity does not erase
unseen work. Workspace output includes fixed Agent state counts and the
outstanding attention count. Boomux does not yet provide terminal heuristics or
agent control.

Optional desktop notifications mirror transitions into `blocked` and `done`.
They do not replace or acknowledge durable attention items. Delivery is an
asynchronous, at-most-once attempt after the Agent mutation commits, and failures
never fail the Agent request. A bounded delivery queue prevents notification
bursts from exhausting daemon resources. Boomux does not replay restored
attention after a daemon start or handoff, and changed evidence for an already
blocked Agent does not generate another notification until the Agent first
leaves that state.

`boomux session list` and `boomux session inspect` project durable Agent
instances and bounded OpenCode root-session catalogs into workspace session
history. Instances are grouped within one
workspace and integration by external session ID; records without one remain
isolated. A session is current only while at least one occurrence is active on
the exact current run of a running retained shell. Otherwise its state is
explicitly last-known. The CLI `description` is the latest stored Boomux Agent
registration name. Catalog-only records use the sanitized OpenCode title, have
state `unknown`, contain no fabricated occurrence, and remain available for
canonical transcript reads while their source directory and host data exist.

Projected session IDs are deterministic, globally unique UUIDs, but are opaque.
Obtain an ID from `session list` and pass that exact value to `session inspect`
or `session read`;
never guess one from an external session ID, description, shell, or Agent ID.
Session projection is client-side metadata over daemon protocol 12 snapshots,
not another persisted daemon entity.

Protocol 13 captures each Agent instance's working directory from its exact
bound shell at registration. Projected occurrences expose this as `source_cwd`,
so canonical transcript lookup can survive shell removal and daemon restart.
The underlying directory and harness transcript data must still exist.

`session read` loads canonical host data for OpenCode and Pi, never terminal
scrollback. It returns the newest bounded suffix of messages, reasoning, and tool
activity in chronological order. Pi reads only the current leaf branch and
combines tool calls with their results. Boomux does not redact host content;
`--limit` and `--max-bytes` explicitly bound the response and report truncation.
When `has_more` is true, pass the opaque `next_cursor` back through `--before`
to read older entries. Cursors preserve the initial normalized snapshot across
appends that leave its normalized prefix unchanged, and expire if existing
entries, the active Pi branch, source context, or adapter normalization changes.
Transcript hosts are registered behind one adapter contract, so future harnesses
such as Claude Code or Codex can supply canonical lookup and normalization while
reusing exact Boomux identity, bounds, errors, and output semantics. Discover the
currently bundled adapters through `session_transcript_integrations` in
`boomux capabilities --json`.

The first explicit process-adapter supervisor runs one exact argument vector:

```console
boomux agent supervise Agent --integration example --external-session-id <canonical-root-id> -- agent-bin --flag
```

`--shell-id` and `--run-id` have the same managed-environment defaults as the
other agent mutations. The command is executed directly, without shell parsing,
and inherits stdin, stdout, and stderr. The supervisor propagates a normal child
exit code and maps signal termination to `128 + signal`. It ensures the exact
integration, external session ID, shell ID, and run ID key, then reports process
start and exit evidence at state `unknown`, authority `process-adapter`, and
confidence 100. Exit evidence includes the child PID and exit code or signal.
Process existence provides no basis for `done`, `working`, `blocked`, or `idle`,
so the supervisor never infers those states.

Reporting is fail-open: ensure or report failures emit a warning but do not stop
the child or replace its exit status. A spawn or wait failure is still a
supervisor command failure. Lifecycle-integration observations outrank these
process-adapter observations and therefore remain unchanged. Identity matching
uses the complete exact key: a supervisor using the lifecycle integration's
same key contributes to that instance, while a different integration, external
session ID, shell ID, or run ID coexists as a distinct instance.

`boomux read` reads plain rendered text from the daemon's shadow VT state. It
understands cursor rewrites and terminal soft wrapping, retains up to 2,000
scrollback rows per shell, and never returns ANSI control sequences.

Read-only integration commands, including `agent list`, `agent inspect`,
`session list`, `session inspect`, and `session read` accept `--json` and emit the stable
`boomux.cli/v1` envelope. Run `boomux capabilities --json` to discover supported
commands, features, and typed error codes without starting the daemon. See
[`docs/cli-json.md`](docs/cli-json.md) for the contract.
Daemon events and revision-aware reads are documented in
[`docs/event-stream.md`](docs/event-stream.md).

</details>

## Configuration

Boomux loads `$XDG_CONFIG_HOME/boomux/config.toml`, falling back to
`~/.config/boomux/config.toml`. `BOOMUX_CONFIG` can provide a final overlay.

```toml
terminal = "Alacritty.desktop"

[projects]
roots = ["~/Projects", "~/Work"]
max_depth = 3

[notifications]
enabled = false
blocked = true
completed = true
```

Notifications are disabled by default and require `notify-send` plus a desktop
notification service. The daemon samples this configuration at startup; run
`boomux daemon restart` after changing it. `boomux doctor` reports whether the
configured command and a plausible desktop bus environment are present. Bodies
contain only the sanitized Agent, workspace, and shell names, never evidence,
working directories, command arguments, external session IDs, or transcript
content.

Directory discovery scans only configured project roots, recognizes ordinary
and linked Git worktrees, and stops descending after finding a repository. The
dashboard uses discovered projects only as quick suggestions for workspace
names. Selecting one does not store or associate its path. Arbitrary text can
also be entered as a workspace name, and every newly created workspace starts
empty.

## Shell Context

Managed processes receive:

```text
BOOMUX_WORKSPACE_ID
BOOMUX_WORKSPACE
BOOMUX_SHELL_ID
BOOMUX_SHELL_NAME
BOOMUX_RUN_ID
```

When an attachment starts a pending or exited shell run, protocol 16 forwards
that attachment client's Unix environment directly to the child. Boomux does
not persist it or expose it in snapshots or events. Terminal-profile values and
the authoritative `BOOMUX_*` identity variables override conflicting client
values. Later attachments never mutate a running process environment.

Workspace launcher invocations instead receive `BOOMUX_WORKSPACE_ID`,
`BOOMUX_WORKSPACE`, `BOOMUX_LAUNCHER_ID`, and `BOOMUX_LAUNCHER_NAME`. They
inherit the invoking client's desktop environment, while shell/run context is
removed.

Workspace and shell IDs remain authoritative after a rename. `BOOMUX_RUN_ID`
identifies one process incarnation and changes when a durable shell starts a
new process after recovery. A live process transferred from a pre-run-identity
daemon receives a daemon-side run identity, but its existing environment cannot
be retrofitted; `shell inspect` reports that compatibility case as
`environment_has_run_id: false`. Agent `register`, `ensure`, and `report` default their
shell and run arguments from `BOOMUX_SHELL_ID` and `BOOMUX_RUN_ID`.
Integrations outside that exact environment must pass both IDs and must not
guess a run from shell status or retained output. A dynamic
Starship segment can call the hidden prompt command:

```toml
[custom.boomux]
command = "boomux prompt"
when = 'test -n "$BOOMUX_SHELL_ID"'
format = '[ 󰊠](bg:blue fg:yellow)[ $output](bg:blue fg:crust)'
```

## Agent Skill

Install the optional vendor-neutral [Agent Skill](https://agentskills.io) with:

```console
boomux skill install
```

It is written to `~/.agents/skills/boomux/SKILL.md` and teaches compatible
agents to discover, inspect, read, create, open, rename, and close Boomux
workspaces and shells, and to inspect and explicitly report run-scoped agent
lifecycle or supervise a process with caller-supplied exact identity, through
the full public CLI. Re-run with `--force` to replace an older customized
installation. An untouched legacy `boomux-shells` skill is removed
automatically; customized legacy content is preserved with a warning.

## Integration Compatibility

Inspect every bundled host integration from one command group:

```console
boomux integration list
boomux integration status
boomux integration status opencode --json
```

Status reports host discovery and version compatibility, the installed Boomux
asset, and current lifecycle reporting independently. Asset state is `missing`,
`current`, `modified`, or `unavailable`. Runtime state is `not_observable` when
the daemon cannot be contacted, `not_running` when no matching foreground host
is present, `reporting` when every matching process has an exact current-run
Agent registration, and `untracked` otherwise. An `unvalidated` host version is
not known to be incompatible; it has simply not been recorded as a Boomux test
point. The `ACTION` column recommends installation, explicit replacement, or a
host restart without performing it.

Install one integration or all bundled integrations with:

```console
boomux integration install opencode
boomux integration install --all
boomux integration install --all --dry-run
```

Installation is individually atomic and idempotent. A modified target is
preserved unless `--force` is supplied, and symlinked or non-regular paths are
rejected. `--dry-run` reports the current state and exact action for every target
without creating directories or changing files. Successful changes print the
required host restart guidance. The
existing `boomux opencode install` and `boomux pi install` commands remain
equivalent host-specific shortcuts.

Remove one integration or all bundled integrations with `boomux integration
uninstall opencode` or `boomux integration uninstall --all`. Uninstall removes
only the bundled asset file and leaves host configuration and directories in
place. Missing assets are accepted. Modified assets require `--force`, while
symlinked and non-regular paths are always rejected.

For an end-to-end guided path, run `boomux integration setup opencode` or
`boomux integration setup pi`. Setup displays host, asset, and runtime status,
previews the exact installation action, asks before changing files, and prints
the restart and verification command. `--yes` accepts a missing-asset install
without prompting; automated replacement requires both `--yes` and `--force`.

After restarting a host, verify that a running managed shell has authoritative
lifecycle reporting:

```console
boomux integration verify opencode
boomux integration verify opencode --shell <shell-id> --wait-ms 30000
```

Verification never treats process or terminal evidence as lifecycle proof. When
several matching host shells are running, Boomux lists each workspace and shell
name with a ready-to-run command containing the required exact shell ID.

Boomux currently validates its bundled integrations against these host releases:

| Integration | Package | Validated version |
| --- | --- | --- |
| OpenCode | `opencode-ai` | `1.18.15` |
| Pi | `@earendil-works/pi-coding-agent` | `0.84.1` |

These versions are compatibility test points, not runtime pins. Older or newer
releases may work when their plugin APIs remain compatible, but are not claimed
as supported until validated. `boomux capabilities --json` exposes the same
matrix under `integration_hosts`.
The scope and evidence behind each test point, including transitions that still
require an authenticated disposable provider, are recorded in
[`docs/lifecycle-validation.md`](docs/lifecycle-validation.md).

## OpenCode Integration

Install the bundled config-time OpenCode lifecycle plugin with:

```console
boomux opencode install
```

The destination is `$XDG_CONFIG_HOME/opencode/plugins/boomux.js`, or
`~/.config/opencode/plugins/boomux.js` when `XDG_CONFIG_HOME` is unset. OpenCode
discovers files in that global plugin directory automatically; the installer
does not edit `opencode.json` or other plugins. An identical file is left
unchanged. Different content requires `--force`, and detected symlinked or
non-regular path components and targets are rejected even with `--force`. Because OpenCode
loads config-time plugins at startup, quit and restart OpenCode after installing
or replacing the plugin. The installer prints this requirement, and `boomux
doctor` reports a foreground OpenCode process without lifecycle registration as
untracked instead of presenting it as idle.

Inside a Boomux-managed shell, the plugin groups each root OpenCode session and
all child/subagent sessions into one durable agent instance keyed by the root
session ID. OpenCode creation, status, chat, tool, compaction, permission/question, error,
idle, and deletion events produce explainable `working`, `blocked`, `idle`, and
`done` observations. Child activity contributes to the root; only root idle can
make it idle, and only explicit deletion of the root reports `done`. Process or
shell exit does not report completion.

The plugin is fail-open: outside a managed Boomux run, when Boomux is
unavailable, or when OpenCode session ancestry cannot be resolved, OpenCode
continues and reporting errors are rate-limited. A `run_changed` response
permanently disables reports for that tracked root so events cannot leak into
another process run.

Do not use automatic OpenCode process discovery to supply `agent supervise`.
OpenCode process identity, argv, database state, and API access do not identify
which canonical root session the user selected. Fresh sessions, continue, fork,
and in-process session switching are unsupported by this supervisor unless the
caller already has the selected canonical root ID and passes it explicitly as
`--external-session-id`. This is not a reason to wrap ordinary OpenCode launches
when the lifecycle plugin is available: the plugin resolves canonical ancestry
and provides stronger, meaningful lifecycle observations.

## Pi Integration

Install the bundled global Pi lifecycle extension with:

```console
boomux pi install
```

The destination is `$PI_CODING_AGENT_DIR/extensions/boomux.js`, or
`~/.pi/agent/extensions/boomux.js` when that environment variable is unset. Pi
discovers the extension automatically. Identical content is left unchanged;
different content requires `--force`, and symlinked or non-regular install paths
are rejected. Restart Pi after installing or replacing the extension.

Inside a Boomux-managed shell, the extension uses Pi's canonical project session
ID and lifecycle hooks. Session start reports `idle`, agent start reports
`working`, and `agent_end` records a final assistant error when present.
`agent_settled` reports that terminal error as `blocked`, or `idle` only after
retries, compaction, and queued continuations have finished. A new agent run
clears the latched error. Session shutdown reports `inactive` rather than
permanent completion because Pi sessions are resumable. Inactive instances
remain durable but do not occupy agent rows until the session starts again.
Calls use exact argument vectors, bounded JSON output, and fail open when Boomux
is unavailable. Session shutdown retries its `inactive` report once after a
transient failure.

## Architecture

```text
Ghostty, Alacritty, or another XDG terminal
  -> boomux __attach <shell-id>
  -> $XDG_RUNTIME_DIR/boomux/daemon.sock
  -> boomux daemon
  -> PTY
  -> shell or application
```

Live PTY bytes pass through unchanged. The attachment client only enables raw
mode, forwards input and resize events, writes output, and restores terminal
mode on exit. See [`docs/architecture.md`](docs/architecture.md) for details.
Shells remain pending until their first attachment reports terminal environment
and dimensions; that profile initializes the PTY and child process. Reproducible
workspace and shell metadata provides crash recovery, while a graceful
`boomux daemon restart` transfers running shells, preserves exited shells and
their final terminal state, and reconnects active clients.
See [`docs/live-pty-handoff.md`](docs/live-pty-handoff.md) for the handoff design.

## POC Limitations

- Live daemon restart preserves pending, running, and exited shells. Active
  attachment clients reconnect cooperatively, while exited shells retain their
  run metadata and bounded final terminal state without starting a new process.
- An unexpected daemon exit cannot hand off live PTYs. Persisted shell metadata
  returns as pending and starts fresh processes when reopened.
- Mutated process environment and in-memory application state are not persisted.
- Reconnection emits at most 1 MiB of sanitized VT reconstruction rather than
  replaying historical PTY bytes. Graphics are omitted from reconstruction.
- Alternate-screen applications restore their current screen, but not their
  alternate-screen history.
- One writable controller is supported per shell; takeover replaces it.
- Slow controllers can lose live output chunks rather than block the child.
- A running shell keeps its first attachment's terminal environment. A later
  attachment with a different `TERM` receives a compatibility warning.
- The native backend currently targets Unix/Linux and Omarchy.

## Development

```console
cargo run
cargo run -- .
cargo run -- . --new
cargo run -- doctor
cargo test
cargo clippy --all-targets -- -D warnings
```

Install the optimized binary with:

```console
cargo install --path . --root ~/.local --force
```

The daemon auto-starts on the first command that needs it. During development,
run `boomux daemon stop` before testing a rebuilt binary because both may use
the same protocol version while containing different code. Stopping the daemon
also terminates every shell it owns.

## Roadmap

See [`docs/roadmap.md`](docs/roadmap.md).

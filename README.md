# Boomux

<p align="center">
  <img src="assets/boomux-cover.png" alt="Boomux: Persistent AI terminals. Native Hyprland windows." width="100%">
</p>

> [!WARNING]
> Boomux is an early proof of concept. Commands, storage, and session behavior
> may change without migration support.

Boomux keeps shells alive in a small background daemon while displaying each
shell in an ordinary native terminal window. The selected terminal emulator
still owns rendering, fonts, selection, clipboard behavior, and window chrome.
Boomux only owns the PTY, process lifetime, workspace grouping, and attachment
transport.

## Usage

Create a shell whose working directory is the requested path and attach in
place. When no workspace name is supplied, Boomux creates the next available
`workspace-N` container automatically:

```console
boomux .
```

Each unnamed invocation creates a new generated workspace. A workspace is a
named container with a UUID; each shell and launcher independently owns its
working directory. Use `--name` to add the shell to an existing named workspace
or create that explicitly named container:

```console
boomux . --name feature-x
```

Run a one-off command instead of the login shell by placing its exact arguments
after `--`:

```console
boomux . -- cargo watch -x test
boomux . --name feature-x --new -- lazygit
```

Boomux executes the command directly without shell parsing. Use an explicit
shell such as `sh -lc` when pipelines, redirects, or variable expansion are
required.

Open the shell in Omarchy's selected terminal instead of attaching in place:

```console
boomux . --new
boomux . --terminal Alacritty.desktop
```

`--terminal` implies `--new`. Selection precedence is the CLI override, Boomux
configuration, then Omarchy's default terminal.

Run Boomux without a path to open the Ratatui dashboard:

```console
boomux
```

The explicit dashboard command remains available as an alias:

```console
boomux ui
```

Dashboard controls:

- `Tab` switches between workspace and shell tables.
- `j`, `k`, and the arrow keys navigate.
- `Enter` restores a workspace, opens the selected shell, or invokes the
  selected launcher.
- `a` creates an empty workspace or adds a shell, depending on the focused
  table. New dashboard shells start in the directory where the dashboard was
  launched.
- `e` renames the selected workspace, shell, or launcher.
- `x`, then `y`, closes the selected workspace or shell, or removes the selected
  launcher.
- `r` refreshes immediately.
- `q` or `Esc` quits.

The selected workspace's shell table also includes configured launchers. A
launcher row shows its command and working directory; pressing `Enter` invokes
only that launcher.

The table also shows registered agent instances, including completed instances.
Agent rows are read-only: they can be inspected but not opened, renamed, or
closed from the dashboard.

Closing a terminal window only disconnects its attachment. The Boomux daemon
retains the PTY and child process until the shell exits, the workspace is
closed, or the daemon stops.

## Commands

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
boomux agent register <name> --integration <integration> [--external-session-id <id>] [--shell-id <shell-id>] [--run-id <run-id>] --state <state> --authority <authority> --evidence <evidence> --confidence <0-100>
boomux agent ensure <name> --integration <integration> --external-session-id <id> [--shell-id <shell-id>] [--run-id <run-id>] --state <state> --authority <authority> --evidence <evidence> --confidence <0-100>
boomux agent supervise <name> --integration <integration> --external-session-id <canonical-root-id> [--shell-id <shell-id>] [--run-id <run-id>] -- <command>...
boomux agent report <agent-id> [--shell-id <shell-id>] [--run-id <run-id>] --state <state> --authority <authority> --evidence <evidence> --confidence <0-100>
boomux daemon status
boomux daemon restart
boomux daemon stop
boomux skill install [--force]
boomux opencode install [--force]
```

`boomux shells` lists shells in the current workspace. `boomux read` and
`boomux close` resolve shell names within that workspace; exact shell IDs work
from anywhere. A shell cannot close itself through the CLI.

The `workspace` and `shell` command groups expose explicit lifecycle operations
for scripts and integrations. `shell create` records a pending shell; its PTY
and process start when the shell is first opened. Shell names require current
workspace context or `--workspace`; IDs remain globally addressable.
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

Agent instances are durable records for external agent sessions. Each instance
has its own exact ID and is bound to exactly one shell run, not merely to a
durable shell. Protocol 10 adds `agent ensure`, an idempotent registration keyed
by integration, external session ID, shell ID, and run ID. It requires
`--external-session-id`; an existing match is returned unchanged, including
after an integration or daemon reload, while a different shell run creates a
different identity. `register`, `ensure`, and `report` require explicit
`unknown`, `working`, `blocked`, `idle`, or `done` state plus authority,
evidence, and confidence.

External reports use this precedence: `lifecycle-integration` over
`process-adapter` over `terminal-heuristic`. A lower-authority report is a
successful no-op. At equal authority, an exact duplicate is a no-op, while any
changed state, evidence, or confidence replaces the observation and increments
its revision. `daemon-lifecycle` exists in snapshots but is reserved for daemon
observations and cannot be supplied to public mutation commands. A `done` report
completes the instance permanently. Retrying the exact completion is an
idempotent success; a different later report is rejected. Completed instances
remain inspectable across daemon restart. Boomux does not yet provide terminal
heuristics, agent waits, agent reads, or agent control.

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

Read-only integration commands, including `agent list` and `agent inspect`,
accept `--json` and emit the stable
`boomux.cli/v1` envelope. Run `boomux capabilities --json` to discover supported
commands, features, and typed error codes without starting the daemon. See
[`docs/cli-json.md`](docs/cli-json.md) for the contract.
Daemon events and revision-aware reads are documented in
[`docs/event-stream.md`](docs/event-stream.md).

## Configuration

Boomux loads `$XDG_CONFIG_HOME/boomux/config.toml`, falling back to
`~/.config/boomux/config.toml`. `BOOMUX_CONFIG` can provide a final overlay.

```toml
terminal = "Alacritty.desktop"

[projects]
roots = ["~/Projects", "~/Work"]
max_depth = 3

```

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
or replacing the plugin.

Inside a Boomux-managed shell, the plugin groups each root OpenCode session and
all child/subagent sessions into one durable agent instance keyed by the root
session ID. OpenCode status, chat, tool, compaction, permission/question, error,
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
cargo run -- .
cargo run -- . --new
cargo run -- ui
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

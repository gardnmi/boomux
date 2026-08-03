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

Each unnamed invocation creates a new generated workspace. A workspace is only
a named shell container with a UUID; each shell independently owns its working
directory. Use `--name` to add the shell to an existing named workspace or
create that explicitly named container:

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
- `Enter` restores a workspace or opens the selected shell.
- `a` creates an empty workspace or adds a shell, depending on the focused
  table. New dashboard shells start in the directory where the dashboard was
  launched.
- `e` renames the selected workspace or shell, depending on focus.
- `x`, then `y`, closes the selected workspace or shell, depending on focus.
- `r` refreshes immediately.
- `q` or `Esc` quits.

Closing a terminal window only disconnects its attachment. The Boomux daemon
retains the PTY and child process until the shell exits, the workspace is
closed, or the daemon stops.

## Commands

```console
boomux ui
boomux doctor
boomux list
boomux shells
boomux read <shell-name-or-shell-id> [--lines <count>]
boomux close <shell-name-or-shell-id>
boomux open <shell-id> [--title <title>] [--takeover]
boomux workspace list
boomux workspace create <name>
boomux workspace inspect <name-or-id>
boomux workspace rename <name-or-id> <new-name>
boomux workspace close <name-or-id>
boomux shell create <workspace-name-or-id> [--name <name>] [--cwd <path>] [-- <command>...]
boomux shell inspect <shell-name-or-id> [--workspace <name-or-id>]
boomux shell rename <shell-name-or-id> <new-name> [--workspace <name-or-id>]
boomux shell close <shell-name-or-id> [--workspace <name-or-id>]
boomux daemon status
boomux daemon stop
boomux skill install
```

`boomux shells` lists shells in the current workspace. `boomux read` and
`boomux close` resolve shell names within that workspace; exact shell IDs work
from anywhere. A shell cannot close itself through the CLI.

The `workspace` and `shell` command groups expose explicit lifecycle operations
for scripts and integrations. `shell create` records a pending shell; its PTY
and process start when the shell is first opened. Shell names require current
workspace context or `--workspace`; IDs remain globally addressable.

`boomux read` reads from the daemon's bounded raw output replay. The current
proof of concept decodes bytes lossily and selects recent newline-delimited
lines. It does not yet interpret ANSI state or terminal soft wrapping.

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
```

IDs remain authoritative after a rename. A dynamic Starship segment can call
the hidden prompt command:

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

It is written to `~/.agents/skills/boomux-shells/SKILL.md` and teaches
compatible agents to discover shells and invoke `boomux read`.

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
and dimensions; that profile initializes the PTY and child process. The
remaining reconnect and persistence work is tracked in
[`docs/native-terminal-follow-up.md`](docs/native-terminal-follow-up.md).

## POC Limitations

- State and PTYs exist only for the daemon's lifetime.
- Restarting or crashing the daemon loses all running shells.
- Reconnection replays at most 1 MiB of raw output, not a reconstructed screen.
- Alternate-screen applications and truncated escape sequences may replay
  imperfectly.
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

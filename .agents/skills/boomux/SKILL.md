---
name: boomux
description: Inspect and manage Boomux persistent terminal workspaces and shells. Use when asked to discover shells, read terminal output, create or open workspaces and shells, inspect status, rename or close targets, or manage the Boomux daemon.
compatibility: Requires boomux on PATH. Some shell-name operations require Boomux workspace context or an explicit --workspace.
metadata:
  author: boomux
  version: "2"
---

# Boomux

Use the Boomux CLI to inspect and manage persistent native-terminal workspaces
and shells. Prefer read-only inspection. Rename, close, takeover, restart, or
stop resources only when the user explicitly requests the operation. Confirm
before an operation that terminates processes or replaces another controller.

## Detect Context

`BOOMUX_SHELL_ID` identifies the current managed shell. IDs are authoritative;
`BOOMUX_WORKSPACE`, `BOOMUX_SHELL_NAME`, and other name variables may be stale
after a rename.

Inside a Boomux shell, discover siblings with:

```console
boomux shells
```

Outside Boomux, discover every shell with:

```console
boomux list
```

Use these commands for structured inspection:

```console
boomux workspace list
boomux workspace inspect "<workspace-name-or-id>"
boomux shell inspect "<shell-name-or-id>" --workspace "<workspace-name-or-id>"
```

Exact shell IDs resolve globally. Shell names resolve in the current workspace,
or through `--workspace` for `shell inspect`, `shell rename`, and `shell close`.
`boomux read` and top-level `boomux close` require an exact shell ID outside a
managed shell. If a name remains ambiguous, ask the user to select a target.

Shell status meanings:

- `pending`: metadata exists; the PTY and process start on first attachment.
- `running`: the process and PTY are live, attached or detached.
- `exited`: the process ended; bounded reconstructed terminal state remains.

## Read Shell Output

```console
boomux read "<shell-name-or-id>" --lines 200
```

`--lines` defaults to 200 and must be at least 1. Increase it when relevant
output is older. The result is bounded, plain rendered VT text without ANSI
sequences. It is not a complete historical log. Primary-screen scrollback is
limited to 2,000 rows; alternate-screen history and graphics are not retained.

## Create And Enter Workspaces

From a fresh host terminal, create a shell for a directory and attach to it:

```console
boomux "/path/to/project"
boomux "/path/to/project" --name "workspace-name"
boomux "/path/to/project" --name "workspace-name" --new
boomux "/path/to/project" --terminal "Alacritty.desktop"
boomux "/path/to/project" -- command arg1 arg2
```

Without `--name`, Boomux creates the next generated workspace. With `--name`, it
adds a shell to an existing exact-name workspace or creates that workspace. A
command after `--` is an exact executable and argument vector; shell operators
such as pipes or redirects work only when explicitly passed through a shell,
for example `-- /bin/sh -lc 'command | command'`.

Open the dashboard or run diagnostics with:

```console
boomux
boomux ui
boomux doctor
```

`boomux` and `boomux ui` must run from a fresh host terminal; they are rejected
when `BOOMUX_SHELL_ID` is set. `boomux doctor` can run in either context.

## Manage Workspaces

```console
boomux workspace list
boomux workspace create "<name>"
boomux workspace inspect "<name-or-id>"
boomux workspace rename "<name-or-id>" "<new-name>"
boomux workspace close "<name-or-id>"
```

`workspace create` creates an empty workspace. `workspace close` removes the
workspace and terminates all of its running shell sessions. A workspace cannot
close itself from one of its own shells.

## Manage Shells

```console
boomux shell create "<workspace-name-or-id>"
boomux shell create "<workspace-name-or-id>" --name "<name>" --cwd "/path"
boomux shell create "<workspace-name-or-id>" --name "<name>" -- command arg
boomux shell inspect "<name-or-id>" --workspace "<workspace-name-or-id>"
boomux shell rename "<name-or-id>" "<new-name>" --workspace "<workspace-name-or-id>"
boomux shell close "<name-or-id>" --workspace "<workspace-name-or-id>"
```

`shell create` records a pending shell. `--cwd` defaults to the current
directory. Omit `--name` to let Boomux generate a unique shell name. A shell
cannot close itself through the CLI.

The contextual close shorthand is:

```console
boomux close "<shell-name-or-shell-id>"
```

## Open Shells

Open an exact shell ID in a new native terminal window:

```console
boomux open "<shell-id>"
boomux open "<shell-id>" --title "<window-title>"
boomux open "<shell-id>" --takeover
boomux --terminal "kitty.desktop" open "<shell-id>"
```

`--takeover` disconnects the current writable controller. Do not use it without
the user's consent.

## Manage The Daemon

```console
boomux daemon status
boomux daemon restart
boomux daemon stop
```

`restart` performs a transactional graceful handoff that preserves pending,
running, and exited shells, including final exited terminal state, and reconnects
active clients. It does not start a new process for an exited shell. `stop`
terminates every managed process session. Confirm before either operation when
the user did not request it explicitly.

## Install Or Update This Skill

```console
boomux skill install
boomux skill install --force
```

The skill is installed at `~/.agents/skills/boomux/SKILL.md`. Use `--force` to
replace different existing content. The installer removes an untouched legacy
`boomux-shells` skill; it preserves and warns about customized legacy content.

## Environment And Integration

Managed processes receive:

```text
BOOMUX_WORKSPACE_ID
BOOMUX_WORKSPACE
BOOMUX_SHELL_ID
BOOMUX_SHELL_NAME
BOOMUX_RUN_ID
```

`BOOMUX_RUN_ID` identifies the current process incarnation. It remains stable
across attachment changes and graceful daemon handoff, but changes when the same
durable shell starts a new process after recovery. A process transferred from a
legacy daemon may have a daemon-side run identity without this environment
variable; inspect `environment_has_run_id` before relying on it.

`boomux prompt` prints `workspace/shell` inside Boomux and nothing outside it.
It is intended for prompt integrations. Use `boomux --help` and
`boomux <command> --help` when exact syntax or newly added options are needed.

Do not invoke private transport commands such as `__attach`, `daemon run`, or
`daemon receive-handoff`; they are implementation details rather than agent
APIs.

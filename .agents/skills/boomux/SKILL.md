---
name: boomux
description: Inspect and manage Boomux persistent terminal workspaces, launchers, shells, run-scoped agent instances, projected sessions, process supervision, and integrations. Use when asked to discover shells, agents, or sessions, read terminal output, supervise an explicitly identified external session, report agent lifecycle state, install the OpenCode or Pi integration, create or open workspaces and shells, inspect status, rename or close targets, or manage the Boomux daemon.
compatibility: Requires boomux on PATH. Some name operations require Boomux workspace context or an explicit --workspace; agent mutation and supervision require exact shell-run context, and supervision requires a caller-supplied canonical external session ID.
metadata:
  author: boomux
  version: "7"
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

For machine-readable inspection, add `--json`. Supported commands emit the
stable `boomux.cli/v1` envelope. Discover the exact command, feature, and
typed-error capabilities without starting the daemon:

```console
boomux capabilities --json
```

Parse `data` on success and `error.code` on a nonzero exit; do not parse human
tables or `error.message`. JSON mutation support includes explicit Agent reports,
attention acknowledgment, and integration installation.

Inspect bundled lifecycle integrations with:

```console
boomux integration list --json
boomux integration status --json
boomux integration status opencode --json
```

Host, asset, and runtime status are independent. `unvalidated` compatibility is
not an incompatibility claim. Never install or replace integration files unless
the user explicitly asks. With that authorization, use `boomux integration
install opencode --json`, `boomux integration install pi --json`, or
`boomux integration install --all --json`; add `--force` only when the user also
authorizes replacing a modified asset. Use `--dry-run` first when the user wants
to inspect exact target paths and planned actions without changing files.
Use `boomux integration uninstall <name> --json` only when the user explicitly
asks to remove an integration; add `--force` only with authorization to remove a
modified asset.
After the user restarts a host, verify authoritative reporting with `boomux
integration verify opencode --json` or `boomux integration verify pi --json`.

Use `boomux events --json` for an immediate snapshot and cursor. Poll again with
`--after CURSOR --wait-ms 30000` to observe later transitions. If
`error.code` is `cursor_expired`, discard the cursor and request a new baseline.
Use `boomux read TARGET --json --run-id RUN_ID --after-revision REVISION
--wait-ms 30000` to wait for run-scoped output changes; handle `run_changed` by
inspecting the shell again.

Discover and inspect durable agent instances with:

```console
boomux agent list --json
boomux agent list --workspace "<workspace-name-or-id>" --json
boomux agent inspect "<exact-agent-id>" --json
boomux agent wait "<exact-agent-id>" --after-revision <revision> --wait-ms 30000 --json
```

Agent lookup is by exact agent ID. Never infer an agent ID or run ID from a
name, shell status, terminal text, a recently seen row, or an external session
ID.

Use `agent wait` after inspecting an exact Agent and retaining its observation
revision. If `changed` is false, repeat with the same revision; if true, retain
the returned newer revision. `revision_ahead` means the caller's context is
invalid and must be reacquired. `daemon_stopping` means reconnect and retry.
Duplicate or lower-authority reports do not satisfy a wait.

Inspect outstanding blocked and completed work with:

```console
boomux attention list --json
boomux attention list --workspace "<workspace-name-or-id>" --json
boomux attention acknowledge "<exact-agent-id>" --observation-revision <revision> --json
```

The queue is durable and ordered blocked before completed, then newest first.
Use the exact Agent ID and raising observation revision returned by `attention
list`; never acknowledge by name or by a later lifecycle revision. A stale
revision fails with `revision_ahead`, while an already empty item returns
`changed: false`. Acknowledgment does not advance lifecycle state or satisfy an
`agent wait`.

Desktop notifications, when the user has opted in, are only a best-effort signal
for new transitions into blocked or done. They do not acknowledge attention,
change an observation revision, or contain lifecycle evidence. Always inspect
the durable queue before acting; do not infer queue state or delivery from a
desktop notification.

Discover projected session metadata with:

```console
boomux session list --json
boomux session list --workspace "<workspace-name-or-id>" --json
boomux session inspect "<exact-session-id>" --json
boomux session read "<exact-session-id>" --limit 100 --max-bytes 1048576 --json
boomux session read "<exact-session-id>" --before "<next-cursor>" --limit 100 --max-bytes 1048576 --json
```

Use the exact opaque session ID returned by `session list`. Never guess or
resolve it from an external session ID, description, shell ID, or Agent ID.
Session state is marked current only when an occurrence is active on the current
run of a running retained shell; otherwise it is last-known. Catalog-only
OpenCode history has state `unknown`, no fabricated occurrence, and a sanitized
host title. Registered-session descriptions remain durable Agent names.
`session read` returns the newest bounded suffix of canonical OpenCode or Pi
messages, reasoning, and tool activity, not terminal scrollback. Pi results
follow its active branch. Inspect `truncated`, `truncated_by`, and per-entry
`truncated` before deciding whether to request larger bounds. Boomux does not
redact host content, so use it only when the user's request authorizes reading
that exact session. Protocol-13 sessions retain a `source_cwd` for transcript
lookup after shell removal; this does not preserve deleted harness data itself.
When `has_more` is true, prepend older pages by passing the exact opaque
`next_cursor` as `--before`; bounds may change between requests. Discard the
cursor and start a fresh read on `cursor_expired`. Do not decode, edit, or reuse
a cursor for another projected session.

Exact shell IDs resolve globally. Shell names resolve in the current workspace,
or through `--workspace` for `shell inspect`, `shell rename`, and `shell close`.
`boomux read` and top-level `boomux close` require an exact shell ID outside a
managed shell. If a name remains ambiguous, ask the user to select a target.

Shell status meanings:

- `pending`: metadata exists; the PTY and process start on first attachment.
- `running`: the process and PTY are live, attached or detached.
- `exited`: the process ended; bounded reconstructed terminal state remains.

The dashboard `KIND` is `shell` when the slot starts a login shell and `command`
when it starts a stored exact argument vector. Interrupting the primary process
of a command ends its run; an active agent presentation takes precedence.

## Report Agent Lifecycle

Use these mutation commands only for a lifecycle integration that directly
observes the external agent session:

```console
boomux agent register "<name>" --integration "<integration>" --external-session-id "<id>" --shell-id "<shell-id>" --run-id "<run-id>" --state working --authority lifecycle-integration --evidence "<direct evidence>" --confidence 100 --json
boomux agent ensure "<name>" --integration "<integration>" --external-session-id "<id>" --shell-id "<shell-id>" --run-id "<run-id>" --state working --authority lifecycle-integration --evidence "<direct evidence>" --confidence 100 --json
boomux agent report "<exact-agent-id>" --shell-id "<same-shell-id>" --run-id "<original-run-id>" --state blocked --authority lifecycle-integration --evidence "<direct evidence>" --confidence 100 --json
boomux agent report "<exact-agent-id>" --shell-id "<same-shell-id>" --run-id "<original-run-id>" --state done --authority lifecycle-integration --evidence "<completion evidence>" --confidence 100 --json
```

`--external-session-id` is optional for `register` and required for `ensure`.
Use `ensure` when an integration needs idempotent identity recovery after a
reload. Its key is integration, external session ID, shell ID, and run ID; a
match returns the existing durable snapshot without applying the supplied name
or observation. Inside the target managed process,
`--shell-id` and `--run-id` default to `BOOMUX_SHELL_ID` and `BOOMUX_RUN_ID`;
otherwise pass both explicitly. Retain the exact agent ID returned by
`register` or `ensure` and use it for every later report. Also retain the original run ID:
never replace it with a newer run from the durable shell.

Every register, ensure, and report command requires an explicit state (`unknown`, `working`,
`blocked`, `idle`, `done`, or `inactive`), external authority (`lifecycle-integration`,
`process-adapter`, or `terminal-heuristic`), nonempty evidence, and confidence
from 0 through 100. `daemon-lifecycle` is reserved and public mutations reject
it. Authority precedence is lifecycle integration over process adapter over
terminal heuristic. Lower-authority and exact duplicate reports are successful
no-ops; equal-authority changed reports update the observation. Report only what
the stated authority actually observed. In particular, report `done` only after direct
lifecycle completion evidence, never because output is quiet, a prompt appears,
or the shell exits. `done` is terminal; retrying the exact completion is safe,
while conflicting later reports are rejected.

If `register`, `ensure`, or `report` returns `run_changed`, stop reporting for that
instance and reacquire exact lifecycle context. Do not guess the replacement
run. Boomux does not yet provide terminal heuristics or agent control.

## Supervise An Explicit Process

Use the process adapter only when the caller already knows the canonical root
external session ID:

```console
boomux agent supervise "<name>" --integration "<integration>" --external-session-id "<canonical-root-id>" --shell-id "<shell-id>" --run-id "<run-id>" -- command arg
```

Inside the target managed process, `--shell-id` and `--run-id` default to
`BOOMUX_SHELL_ID` and `BOOMUX_RUN_ID`. Arguments after `--` are an exact argv,
not shell syntax. The child inherits stdin, stdout, and stderr, and the
supervisor returns its exit code or `128 + signal`.

The supervisor ensures the exact integration, external session ID, shell ID,
and run ID key. It reports child start and exit only as `unknown` with
`process-adapter` authority and PID plus exit-code/signal evidence. It never
infers `done`, `working`, `blocked`, or `idle`. Reporting failures warn and fail
open so the child continues and retains its exit result; spawn and wait failures
still fail the command. Lifecycle-integration authority wins. A different value
in any key field creates or reacquires a distinct coexisting agent instance.

Never automatically wrap OpenCode based on process discovery. Its process,
argv, database, and API do not identify the canonical root session selected by
the user. Fresh, continue, fork, and in-process session switching are unsupported
unless the caller already has that canonical root ID. When the OpenCode
lifecycle plugin is available, use ordinary OpenCode without this wrapper: the
plugin resolves ancestry and reports stronger lifecycle evidence.

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
boomux workspace open "<name-or-id>"
boomux workspace inspect "<name-or-id>"
boomux workspace rename "<name-or-id>" "<new-name>"
boomux workspace close "<name-or-id>"
```

`workspace create` creates an empty workspace. `workspace close` removes the
workspace and terminates all of its running shell sessions. A workspace cannot
close itself from one of its own shells.

`workspace open` invokes every configured workspace launcher in creation order,
then opens all shell terminal windows with takeover. It also supports a
launcher-only workspace. Merely selecting a dashboard row does not invoke
launchers.

## Manage Workspace Launchers

```console
boomux launcher list --workspace "<workspace-name-or-id>"
boomux launcher create "<name>" --workspace "<workspace-name-or-id>" --cwd "/path" -- command arg
boomux launcher inspect "<name-or-id>" --workspace "<workspace-name-or-id>"
boomux launcher rename "<name-or-id>" "<new-name>" --workspace "<workspace-name-or-id>"
boomux launcher remove "<name-or-id>" --workspace "<workspace-name-or-id>"
```

Launchers are durable ordered definitions, but each invocation is a detached,
ephemeral process without a PTY or retained output. Commands are exact argument
vectors; use an explicit shell for pipelines or expansion. `--cwd` defaults to
the current directory. Removing a launcher does not terminate applications from
earlier invocations. Exact launcher IDs resolve globally; names require current
workspace context or `--workspace`.

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
the user's consent. Opening an exited shell explicitly restarts its stored
command as a new run on the same durable shell identity; use `boomux read` when
the goal is only to inspect retained output.

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

## Install The OpenCode Integration

The bundled plugin is validated against `opencode-ai` `1.18.15`; this is a
compatibility test point rather than a runtime pin.

```console
boomux opencode install
boomux opencode install --force
```

The installer writes `$XDG_CONFIG_HOME/opencode/plugins/boomux.js`, falling back
to `~/.config/opencode/plugins/boomux.js`. OpenCode discovers that global
config-time plugin automatically; no `opencode.json` edit is made. Identical
content is left alone, different content requires `--force`, and detected
symlinks or non-regular path targets are rejected. Quit and restart OpenCode after
installing or replacing the plugin.

The plugin activates only in a managed shell run. It identifies one agent by the
root OpenCode session and aggregates child/subagent activity. Work, tool, chat,
and compaction events map to `working`; outstanding permission/questions and
errors map to `blocked`; only root idle maps to `idle`; and only explicit root
session deletion maps to `done`. Child deletion and process exit do not report
completion. Unmanaged or unavailable Boomux is fail-open. If Boomux returns
`run_changed`, reporting for that root is disabled rather than redirected.

## Install The Pi Integration

The bundled extension is validated against
`@earendil-works/pi-coding-agent` `0.84.1`; this is a compatibility test point
rather than a runtime pin.

```console
boomux pi install
boomux pi install --force
```

The installer writes `$PI_CODING_AGENT_DIR/extensions/boomux.js`, falling back
to `~/.pi/agent/extensions/boomux.js`. Identical content is left alone,
different content requires `--force`, and symlinks or non-regular targets are
rejected. Restart Pi after installing or replacing the extension.

The extension activates only in a managed shell run and keys the Agent instance
by Pi's canonical project session ID. Session start reports `idle`; agent start
reports `working`; final assistant errors report `blocked` after the agent fully
settles; and session shutdown reports `inactive` because Pi sessions are
resumable. Inactive records remain durable but do not occupy dashboard agent
rows. Reporting is serialized and fail-open.

## Environment And Integration

Managed processes receive:

```text
BOOMUX_WORKSPACE_ID
BOOMUX_WORKSPACE
BOOMUX_SHELL_ID
BOOMUX_SHELL_NAME
BOOMUX_RUN_ID
```

Protocol 16 starts pending and exited shell runs with the attaching client's
ephemeral Unix environment. Boomux does not persist or project that payload;
terminal-profile and `BOOMUX_*` identity values are authoritative. Reattaching
does not alter an already-running process environment.

Detached launcher invocations inherit the invoking client's environment and
receive `BOOMUX_WORKSPACE_ID`, `BOOMUX_WORKSPACE`, `BOOMUX_LAUNCHER_ID`, and
`BOOMUX_LAUNCHER_NAME`. Shell and run context variables are removed.

`BOOMUX_RUN_ID` identifies the current process incarnation. It remains stable
across attachment changes and graceful daemon handoff, but changes when the same
durable shell starts a new process after recovery. A process transferred from a
legacy daemon may have a daemon-side run identity without this environment
variable; inspect `environment_has_run_id` before relying on it.

For such a legacy run, do not invent `BOOMUX_RUN_ID` or select a run from
history. Obtain the exact daemon-side run ID through `shell inspect --json` and
pass it explicitly only when the lifecycle integration is known to belong to
that same live process.

`boomux prompt` prints `workspace/shell` inside Boomux and nothing outside it.
It is intended for prompt integrations. Use `boomux --help` and
`boomux <command> --help` when exact syntax or newly added options are needed.

Do not invoke private transport commands such as `__attach`, `daemon run`, or
`daemon receive-handoff`; they are implementation details rather than agent
APIs.

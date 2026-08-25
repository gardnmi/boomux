---
name: boomux
description: Inspect and manage Boomux Nodes, coordinated persistent terminal Workspaces, launchers, shells, run-scoped agent instances, attention, projected sessions, recurring Agent schedules, local configuration, notifications, the OpenCode Shared Harness Runtime, process supervision, and integrations. Use when asked to discover Nodes, shells, agents, sessions, or schedules, read terminal output, inspect, validate, or edit local Boomux configuration, manage explicitly authorized recurring Agent prompts, supervise an explicitly identified external session, report agent lifecycle state, configure or test notifications, install or remove an OpenCode, Pi, Claude Code, Codex, or Kiro integration, create or open Workspaces and shells, inspect status, rename or close targets, or manage the local Boomux daemon.
compatibility: Requires boomux on PATH. Federated resource identity is the pair of owning Node ID and Node-local inner ID. Some name operations require Workspace context or an explicit --workspace/--node; agent mutation and supervision require exact shell-run context, and continuation schedules or supervision require caller-supplied exact canonical session identity.
metadata:
  author: boomux
  version: "17"
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

Outside Boomux, discover shells owned by the local Node with:

```console
boomux list
```

Use these commands for structured inspection:

```console
boomux workspace list
boomux workspace inspect "<workspace-name-or-id>"
boomux shell inspect "<shell-name-or-id>" --workspace "<workspace-name-or-id>"
boomux node snapshot --json
```

`node snapshot` is the combined qualified view. Every projected Node-owned
resource identity is `(node_id, inner_id)`; retain both fields together. Never
route a remote inner ID without its owning Node ID. Cached remote rows can be
stale and are suitable only for documented prompt-free summaries, not authority,
private inspection, terminal reads, or mutation.

For machine-readable inspection, add `--json`. Supported commands emit the
stable `boomux.cli/v1` envelope. Discover the exact command, feature, and
typed-error capabilities without starting the daemon:

```console
boomux capabilities --json
```

Parse `data` on success and `error.code` on a nonzero exit; do not parse human
tables or `error.message`. JSON mutation support includes Node registration
management, Agent lifecycle reporting, attention acknowledgment, Schedule
management, execution open/cancellation, and integration management. Treat this
as illustrative; `capabilities.data.json_commands` is authoritative.

Most daemon-backed inspection commands automatically start Boomux when it is
not running. This includes `list`, `shells`, `read`, `events`, workspace, shell,
and launcher inspection, Agent, attention, session, and schedule inspection, and
`doctor`. Use `boomux daemon status` first when starting the daemon would be an
unwanted side effect. `capabilities`, `project list`, `integration list`, and
`integration status` do not start it.

## Manage Nodes And Federation

Inspect or register persistent identity-pinned OpenSSH routes with:

```console
boomux node list --json
boomux node inspect "<node-alias-or-id>" --json
boomux node snapshot --json
boomux node add "<alias>" "<user@host>"
boomux node upgrade "<node>"
boomux node rename "<node>" "<new-alias>" --revision <revision> --json
boomux node retarget "<node>" "<user@host>" --revision <revision> --json
boomux node forget "<node>" --json
boomux node rekey
```

`node add` verifies and pins the remote Node identity. Interactive setup may,
after explicit confirmation, install or replace a remote helper and gracefully
restart an incompatible remote daemon. JSON setup never approves installation;
it returns typed `install_required` or `upgrade_required`. Retargeting must prove
the pinned identity before replacing the route. Forget removes only the local
registration and cached projection; it does not contact or delete the owner.
In the dashboard, `x` on a stale remote Shell dismisses only its cached local
presentation and explicitly warns that the remote process is not closed. On the
Nodes tab, `u` restores dismissed Shells for the selected Node. Online `x`
continues to perform an owner-authoritative close. `U` opens an explicitly
authorized upgrade for the selected remote Node in a native terminal.
`node upgrade` is human-only and requires an interactive terminal. It verifies
the registered Node identity before replacing the helper transactionally and
gracefully restarting its daemon, including when the old helper remains protocol-compatible.
`node rekey` changes this Node's durable identity and is local, interactive, and
human-only; use it only after the command's exact confirmation requirements are
explicitly authorized.

For one unregistered ad hoc route, `boomux --remote "<user@host>" ...` uses a
temporary verified connection and does not persist registration. Registered and
ad hoc routes do not make hostnames or addresses resource identities.

Node-qualified host operations include `project list`, launcher invocation,
integration management, Session catalogs/resume, Schedule and execution
management, and `open`. Preserve the same explicit `--node` on every follow-up
operation. Remote paths, commands, catalogs, and integration assets are resolved
only by their owning Node. Daemon status, restart, and stop remain local and are
not routed through registration.

Inspect bundled lifecycle integrations with:

```console
boomux integration list --json
boomux integration status --json
boomux integration status opencode --json
boomux integration status codex --json
```

Mutation commands include:

```console
boomux integration install opencode --json
boomux integration install --all --json
boomux integration uninstall opencode --json
boomux integration uninstall --all --json
boomux integration setup opencode
boomux integration verify opencode --json
boomux integration setup codex
boomux integration verify codex --json
```

Host, asset, and runtime status are independent. `unvalidated` compatibility is
not an incompatibility claim. `integration status` does not start Boomux, but it
executes each PATH-resolved host's `--version`; avoid it when those executables
are untrusted.

Never install, replace, or remove integration files unless the user explicitly
asks. With that authorization, use `boomux integration install opencode --json`,
`boomux integration install pi --json`, `boomux integration install codex
--json`, or `boomux integration install --all
--json`; add `--force` only when the user also authorizes replacing a modified
asset. Use `--dry-run` to inspect exact paths and actions. Previewing replacement
of modified content requires `--force --dry-run`, which does not write files.
Use `boomux integration uninstall <name> --json` or `boomux integration
uninstall --all --json` only when explicitly requested. Uninstall has no dry-run
mode; add `--force` only with authorization to remove a modified asset.

Batch install and uninstall preflight all targets before the first change but
are not transactions across hosts; a later filesystem failure can leave earlier
targets changed.

For an interactive end-to-end workflow, use `boomux integration setup <name>`.
It inspects current state, previews the target action, confirms mutation, and
prints restart and verification guidance. Do not pass `--yes` unless the user
has authorized installation. Interactive setup can replace modified content
after an explicit yes response; noninteractive replacement requires both
`--yes` and `--force`.

After the user restarts a host, verify authoritative reporting with `boomux
integration verify opencode --json`, `boomux integration verify pi --json`, or
`boomux integration verify codex --json`.
Verification requires a running foreground host and current-run
`lifecycle-integration` evidence. If multiple hosts match, pass `--shell
"<exact-shell-id>"`; shell names are not accepted.

Use this for an immediate snapshot and cursor:

```console
boomux events --json
```

Poll again with `--after CURSOR --wait-ms 30000` to observe later transitions. If
`error.code` is `cursor_expired`, discard the cursor and request a new baseline.
Use `boomux read TARGET --json --run-id RUN_ID --after-revision REVISION
--wait-ms 30000` to wait for run-scoped output changes. Process `changed`,
`status`, `run_id`, and `output_revision` before examining `output`. When
`changed` is false, `output` is intentionally empty and does not mean retained
output is empty; retain the previous rendered state. On `revision_ahead`,
reacquire the shell snapshot. On `run_changed`, do not substitute a new run
implicitly. On `daemon_stopping`, reconnect and repeat with the same run and
revision if still relevant.

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
revision. If `changed` is true, retain the returned newer revision. If it is
false after a timeout and the Agent is still nonterminal, repeat with the same
revision only when continued waiting is desired. Stop waiting when state is
`done`; a completed Agent at the supplied revision returns unchanged
immediately. `inactive` is resumable and may advance later. `revision_ahead`
means the caller's context is invalid and must be reacquired. `daemon_stopping`
means reconnect and retry. Duplicate or lower-authority reports do not satisfy
a wait.

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

Desktop and sound notifications, when the user opts in, are best-effort signals
for new transitions into blocked or done, and for working Agents becoming idle
after completing a unit of work. Idle completion notifications do not create
durable completed attention. Notifications do not acknowledge attention, change
an observation revision, or contain lifecycle evidence. Always inspect the
durable queue before acting; do not infer queue state or delivery from a
notification.

Desktop and sound delivery are independently disabled by default. Desktop uses
`notify-send`; sound uses `canberra-gtk-play` with configured freedesktop sound
event IDs. The top-level blocked and completed category flags filter both.

```toml
[recovery]
resume_agents = true
persist_terminal_history = false

[notifications]
enabled = false
blocked = true
completed = true

[notifications.sound]
enabled = false
blocked = "message-new-instant"
completed = "complete"
```

Cold recovery resumes only a unique OpenCode, Pi, Claude, or Codex external
session reported by the lifecycle integration. An eligible recovered OpenCode Session attaches to
the Shared Harness Runtime and establishes a fresh claim for its replacement
ShellRun; if shared launch preparation is unavailable, exact Session recovery
continues standalone without a web link. Persisted terminal history is disabled by default
because output can contain secrets; enabling it stores up to 256 KiB of
plain-text history per shell in Boomux's user-only state file.

`notifications.enabled` controls desktop delivery;
`notifications.sound.enabled` controls sound. Test all currently configured,
enabled channels without changing Agent state using:

```console
boomux notification test blocked
boomux notification test completed
```

These commands are human-only, do not support `--json`, and fail when the
requested category has no enabled channel. Runtime delivery uses the daemon's
startup-sampled configuration until `boomux daemon restart`. Restart applies
the invoking client's resolved notification settings, even when the old daemon
inherited a different config environment.

Session discovery is not limited to daemon metadata. It may execute the
PATH-resolved OpenCode or Codex CLI and inspect host catalogs in
workspace-derived directories, exposing sanitized but potentially private session titles.
Require authorization appropriate to the host-history metadata before listing
or inspecting sessions.

Discover projected session metadata with:

```console
boomux session list --json
boomux session list --workspace "<workspace-name-or-id>" --json
boomux session inspect "<exact-session-id>" --json
boomux session resume "<exact-session-id>"
boomux session list --node "<node>" --json
boomux session inspect "<exact-session-id>" --node "<same-node>" --json
boomux session resume "<exact-session-id>" --node "<same-node>"
```

Use the exact opaque session ID returned by `session list`. Never guess or
resolve it from an external session ID, description, shell ID, or Agent ID.
Session state is marked current only when an occurrence is active on the current
run of a running retained shell; otherwise it is last-known. Catalog-only
OpenCode or Codex history has state `unknown`, no fabricated occurrence, and a
sanitized host title. Registered-session descriptions remain durable Agent names.
Protocol-13 sessions retain a `source_cwd` after shell removal so an exact
canonical session can be resumed in its original context. Resume launches a
native host process and requires authorization. A remote Session ID is exact
only within its owning Node; keep the same `--node` used for discovery.

## Manage Agent Schedules

Schedule prompts are durable private content. Require authorization before
creating one or using `schedule inspect`, because inspect is the only management
surface that discloses the prompt. Enabling a schedule authorizes future
unattended Agent process and tool activity; never pass `--enabled` or run
`schedule resume` without explicit authorization for that continuing effect.

```console
boomux schedule create "<name>" --workspace "<workspace-name-or-id>" --node "<node>" --cwd "/absolute/owner/path" --integration opencode --prompt-file "/path/to/prompt.txt" --weekdays 09:00 --json
boomux schedule list --json
boomux schedule list --workspace "<workspace-name-or-id>" --json
boomux schedule inspect "<exact-id-or-contextual-name>" --workspace "<workspace-name-or-id>" --json
boomux schedule pause "<exact-id-or-contextual-name>" --workspace "<workspace-name-or-id>" --json
boomux schedule resume "<exact-id-or-contextual-name>" --workspace "<workspace-name-or-id>" --json
boomux schedule run "<exact-id-or-contextual-name>" --workspace "<workspace-name-or-id>" --json
boomux execution list --workspace "<workspace-name-or-id>" --schedule "<schedule-name-or-id>" --limit 100 --json
boomux execution inspect "<exact-execution-id>" --json
boomux execution wait "<exact-execution-id>" --after-revision "<revision>" --wait-ms 30000 --json
boomux execution open "<exact-execution-id>" --json
boomux execution cancel "<exact-execution-id>" --json
boomux schedule remove "<exact-id-or-contextual-name>" --workspace "<workspace-name-or-id>" --json
```

Create requires explicit Workspace, owner-local cwd, integration, exactly one prompt source,
and exactly one trigger source. `--prompt` preserves the accepted bytes;
`--prompt-file` snapshots exact UTF-8, including a final newline, and does not
track later file changes. Use `--cron '<five fields>'`, `--every Nm|Nh`, `--daily
HH:MM`, `--weekdays HH:MM`, or `--weekly DAY@HH:MM`. Omitted `--timezone`
snapshots the system IANA timezone. New schedules default to fresh, paused, and
skip-overlap policy. `--fresh` conflicts with `--continue`; `--paused` conflicts
with `--enabled`.

For continuation, first obtain the exact opaque projected session ID from
`session list --workspace ... --json`, then pass it as `--continue`. It must
resolve in that workspace, expose a canonical external session ID, and match the
selected integration. Never substitute a description, external ID, latest
session, Agent ID, or shell ID.

List, create, pause, resume, remove, run, and all execution responses are
prompt-free. Inspect
returns the private prompt under `data.schedule.prompt`; do not log or repeat it
unless needed for the authorized request. Exact Schedule and execution IDs are
global only within one Node. Names resolve only with explicit `--workspace` or
`BOOMUX_WORKSPACE_ID`; preserve `--node` for remote discovery and every exact
follow-up operation.
Removing a schedule removes its persisted prompt. Workspace close removes every
owned schedule and persisted prompt and must be confirmed with that full scope.

`schedule run` is an explicit process-starting action and requires authorization
for that one execution. It remains available while paused. Use
`--idempotency-key <uuid>` for request retry; otherwise the CLI creates a UUID
before sending. Never retry with a new key when the intent is the same dispatch.
Inspect the returned exact execution ID, and cancel only when process-tree
termination is authorized. Execution exit or cancellation never means Agent
`done`.

`execution open` is also process-starting/presentation behavior. It opens only
the exact active run or exact linked canonical Agent Session; it must not restart
the reusable Schedule shell or substitute a later run or Session.

Execution list responses are newest-first and contain `limit`, `truncated`, and
prompt-free records. Limits are 1-1,000 and default to 100. Each execution has a
positive durable `revision`. Wait with the exact last revision instead of polling
lists: newer state or Agent linkage returns `changed: true`, timeout returns the
same record with `changed: false`, and a future revision fails with
`revision_ahead`. On `daemon_stopping`, reconnect and repeat the same revision.
Do not stop waiting merely because process state is terminal; the canonical Agent
link can arrive later, and its blocked attention remains under `agent inspect` or
`attention list` with the exact Agent ID and observation revision.

Enabled schedules are evaluated by the daemon in their stored timezone. Timed
work runs only while the daemon and user session are active. Offline periods are
recorded as one coalesced missed decision, paused periods are not caught up, and
policy contention is skipped rather than queued. Manual and timed decisions share
one active execution per schedule and workspace, exact continuation exclusion,
and `[scheduling] max_concurrent` (default 4, range 1-64). Use `daemon status`
and `doctor` to inspect scheduler health and whether a config change needs
`daemon restart`.

Cron day-field semantics preserve syntax: `*` and `*/n` are wildcard-origin;
numeric lists/ranges are restricted even when they cover the full field, and two
restricted day fields use OR. Boomux rejects schedules with no occurrence in a
Gregorian 400-year cycle. Treat scheduler `offline` health as not evaluating
timed work even when the daemon still answers other requests.

Exact shell IDs are global only within one Node. Shell names resolve in the current workspace,
or through `--workspace` for `shell inspect`, `shell rename`, and `shell close`.
`boomux read` and targeted top-level `boomux close` require an exact shell ID
outside a managed shell. `boomux close --focused` instead resolves the most
recently focused Boomux terminal, which can remain selected while a non-Boomux
window is active. If a name remains ambiguous, ask the user to select a target.

Shell status meanings:

- `pending`: metadata exists; the PTY and process start on first attachment.
- `running`: the process and PTY are live, attached or detached.
- `exited`: the process ended; bounded reconstructed terminal state remains.

Under protocol 40, an owner-authorized pending Shell with one exact resumable
lifecycle Agent retains `KIND agent` with `STATUS inactive`; opening it starts a
new run and resumes that exact session. Fresh or ineligible pending Shells remain
`KIND shell` or `KIND command`.

The dashboard `KIND` is `shell` when the slot starts a login shell and `command`
when it starts a stored exact argument vector. Interrupting the primary process
of a command ends its run; an active agent presentation takes precedence.

## Report Agent Lifecycle

Use these mutation commands only for a lifecycle integration that directly
observes the external agent session:

```console
boomux agent register [<name>] --integration "<integration>" --external-session-id "<id>" --shell-id "<shell-id>" --run-id "<run-id>" --state working --authority lifecycle-integration --evidence "<direct evidence>" --confidence 100 --json
boomux agent ensure [<name>] --integration "<integration>" --external-session-id "<id>" --shell-id "<shell-id>" --run-id "<run-id>" --state working --authority lifecycle-integration --evidence "<direct evidence>" --confidence 100 --json
boomux agent report "<exact-agent-id>" --shell-id "<same-shell-id>" --run-id "<original-run-id>" --state blocked --authority lifecycle-integration --evidence "<direct evidence>" --confidence 100 --json
boomux agent report "<exact-agent-id>" --shell-id "<same-shell-id>" --run-id "<original-run-id>" --state done --authority lifecycle-integration --evidence "<completion evidence>" --confidence 100 --json
```

Omitting the descriptive Agent name generates a random lowercase
`adjective-noun` name. `--external-session-id` is optional for `register` and required for `ensure`.
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
boomux agent supervise [<name>] --integration "<integration>" --external-session-id "<canonical-root-id>" --shell-id "<shell-id>" --run-id "<run-id>" -- command arg
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
limited to 2,000 rows and reads are capped at 1 MiB. The current alternate
screen can be reconstructed, but alternate-screen history and graphical content
are not retained.

## Create And Enter Workspaces

The local Hyprland special-Workspace layer defaults on. These human-only
commands are suitable for desktop keybindings:

```console
boomux desktop toggle
boomux desktop show "<workspace-name-or-id>"
boomux desktop next
boomux desktop previous
boomux desktop terminal
boomux desktop close
boomux desktop pop
boomux desktop return
boomux desktop gather
```

`toggle` shows or hides the selected coordinated Workspace. `show` targets one
coordinated Workspace by exact name or ID. While that Boomux
layer is visible, next/previous cycle coordinated Workspaces; otherwise they
delegate to ordinary Hyprland workspace navigation. `terminal` creates a Shell
on the local Node in the visible Boomux Workspace, or opens a normal terminal
outside the layer. These are local presentation actions, do not support JSON,
do not invoke Workspace launchers, and do not change remote ownership.
`close` closes the exact focused Boomux Shell in a visible Boomux layer after
qualified focus validation, or closes the ordinary active window outside it.
`pop` floats or tiles the active window without pinning inside a Boomux layer;
outside the layer it retains ordinary float-and-pin behavior.
`return` moves the exact active identity-marked terminal back to its unique
active coordinated Workspace placement without opening or restarting the Shell.

From a fresh host terminal, create a shell for a directory and attach to it:

```console
boomux "/path/to/project"
boomux "/path/to/project" --name "workspace-name"
boomux "/path/to/project" --name "workspace-name" --new
boomux "/path/to/project" --terminal "Alacritty.desktop"
boomux "/path/to/project" -- command arg1 arg2
```

Without `--name`, Boomux creates the next generated workspace and stores the
selected path as its default cwd. With `--name`, it adds a shell to an existing
exact-name workspace or creates that workspace with the selected path as its
default. This shorthand is a local Node-owned workflow and can produce an
external unlinked Workspace under protocol 38; it does not select or join a
coordinated Workspace by equal name. Prefer `workspace create` followed by
Node-qualified first-resource creation when coordination is intended. A command
after `--` is an exact executable and argument vector; shell
operators such as pipes or redirects work only when explicitly passed through a
shell, for example `-- /bin/sh -lc 'command | command'`.

`--terminal` overrides configured terminal selection and implies `--new` for
path opening. Selection otherwise follows Boomux configuration and then normal
`xdg-terminal-exec` policy.

Open the dashboard or run diagnostics with:

```console
boomux
boomux ui
boomux web
boomux doctor
```

`boomux` and `boomux ui` must run from a fresh host terminal; they are rejected
when `BOOMUX_SHELL_ID` is set. `boomux doctor` can run in either context.

`boomux web` serves an experimental read-only Agent PWA on
`127.0.0.1:3737`. Use `--port` to change the loopback port. `--tailscale` is an
explicit mutation that publishes the dashboard and enabled OpenCode runtime
through a connected PATH-resolved Tailscale CLI. It reuses compatible routes,
rejects conflicts, tracks only routes it creates, and removes those exact routes
on graceful exit or `boomux web stop`; it never resets unrelated Serve state or
defines tailnet grants and ACLs. Without the flag, remote publication remains
external.
It uses the Omarchy presentation model: one current user-Shell
Agent per exact run plus historical Agents with durable attention, excluding
schedule-owned Agents. It also presents local working-to-idle transitions as
gateway-owned ephemeral finished alerts, refreshed even without a connected
browser. Its home page is the complete dashboard and cannot send terminal input
or acknowledge attention. It attempts to ensure the Node's loopback Shared
Harness Runtime and links exact local Sessions directly from eligible Agent cards
only while a current Agent Session Claim binds that runtime generation to the
exact ShellRun. If OpenCode is not installed, web startup continues without that
runtime or its Tailscale route; Claude and other presentation remain available.
Conflicts and failures from an installed runtime remain fatal.
`--opencode-web-url URL` is the public origin for that same local runtime, never
an unrelated server. `--no-opencode-web` disables web-triggered runtime startup
and native links. Boomux does not proxy or authenticate that full-control
service. The private access layer owns TLS, authentication, and ACLs. Remote
Agents remain unlinked.
Stop only the default gateway with `boomux web stop`, or an alternate gateway
with `boomux web stop --port PORT`. This leaves the daemon, managed processes,
and Shared Harness Runtime running. The command is safe when no matching gateway
is running.
Use `boomux web start [--tailscale]` for detached operation and `boomux web
status` for passive inspection. Start requires an already running daemon, waits
for readiness, and is idempotent only for equivalent options. Start, status, and
stop support `--json` as `web.start`, `web.status`, and `web.stop`.
`OPENCODE_SERVER_PASSWORD` and `OPENCODE_SERVER_USERNAME` are ephemeral
first-start runtime environment, are never persisted by Boomux, and must remain
consistent for attached clients.

The dashboard has Workspaces, Agents, Shells, Schedules, and Nodes views. `/` or
`:` opens its action and search palette, `?` shows contextual help, `Enter`
restores the selected workspace or opens the selected entry, and `x` followed
by `y` confirms close or removal. On an active coordinated Workspace row, `s`
stores that Workspace as the default context for later context-free commands.
Shell previews are bounded and read-only.
Restoring a workspace has the same launcher, takeover, restart, and
partial-success behavior as `workspace open`. By default, a newly focused
managed terminal selects its owning workspace and shell or Agent row once;
manual navigation remains until another focus change. Press `Space` to pin the
current dashboard selection and pause focus following; press it again to unpin
and catch up to the currently focused terminal.
With the Hyprland desktop layer enabled, Enter on a coordinated Workspace shows
its layer while restoring all items; the dashboard remains active in its
terminal and refreshes for when focus returns. Enter on an Agent or Shell shows
that same owning layer and opens only the selected terminal. Unavailable
placement operations are reported as a warning when at least one item restored.

The Nodes view inspects and refreshes registrations, starts guided setup,
revision-safely renames or retargets routes, and forgets a registration after
confirmation. It is not a resource filter. Coordinated Workspace rows aggregate
persisted placements while every item retains its owner Node; external owner
Workspaces remain distinct and offer explicit adopt/link actions.

Each Agent Schedule appears once in its owning workspace with `KIND schedule`.
That row is the durable definition, not a process; Enter navigates to its exact
Schedules view. Schedule-owned execution shells remain absent from ordinary
workspace rows and process counts.

In Schedules, Left/Right changes between the schedule and history panes, `j`/`k`
navigates the focused pane, and `[`/`]` also selects retained executions by exact
execution ID. `Enter` attaches an exact Starting or Active run. For a completed
execution with a canonical session, it resumes that exact OpenCode, Pi, Claude,
or Codex session in an unmanaged native terminal without adding a workspace row. `u` runs now with a fresh dispatch key, `p`
pauses or resumes future timed work, `c` then
`y` confirms cancellation of the selected exact active execution. Selecting a
schedule automatically loads bounded exact-schedule history. `x` then `y`
confirms removal. `a` shows `boomux
schedule create --help`. Prompt text is shown only after explicit exact editing
or inspection. Schedule-owned shells are excluded from ordinary rows and
workspace restore; their exact Agents remain selectable without ordinary shell
actions. Opening does not acknowledge durable attention. Protocol 25
has no skip-next operation; never emulate one with pause and resume.
Active Open uses a protocol-26 exact-run attachment that cannot restart into a
later run. Protocol-25 dashboards retain schedule controls and history but
disable exact active-run attachment with upgrade-and-restart guidance.

Press `e` to fetch and edit the exact private definition of a paused schedule.
The built-in editor changes name, prompt, trigger preset or custom cron, and IANA
timezone. Its timezone control filters the bundled IANA names as the user types;
arrows select only valid matches. `Ctrl-S` saves with the loaded revision and
`Esc` discards the private buffer. A stale save must be cancelled and reloaded rather than blindly retried.
Trigger edits start future evaluation at commit time, while active executions
retain their captured definition.

## Configuration

Boomux reads `$XDG_CONFIG_HOME/boomux/config.toml`, falling back to
`~/.config/boomux/config.toml`. `BOOMUX_CONFIG` points to an additional
field-level override loaded last. The override is the active writable layer when
set; otherwise the global file is writable. Omitted active-layer fields inherit
from the global file or defaults. Configuration controls Terminal, Projects,
Dashboard, Recovery, Scheduling, Notifications, and Sound groups. Unknown fields
are rejected.
Scheduled dispatch-failure and cold-interruption notification categories are
configured independently as `[notifications] scheduled_dispatch_failed` and
`scheduled_interrupted`; both default to false and never disclose schedule
prompts or acknowledge Agent attention.
Selecting a discovered project in the dashboard uses its name only and creates
empty coordinator metadata. Set
`[dashboard] follow_focused_terminal = false` to disable the default
focus-following behavior.

Inspect or manage the active local layer without starting the daemon:

```console
boomux config path
boomux config validate
boomux config edit
```

These commands are human-only and do not support `--json`. `path` prints the
active writable file and `validate` checks the complete layered result. `edit`
uses `VISUAL`, then `EDITOR`, then `sensible-editor` with a `vi` fallback. The
editor setting is parsed into an exact argv and executed without a shell. Boomux
edits an owner-only temporary copy, validates the bounded candidate, and performs
an owner-validated atomic replacement. Symlinks, non-regular or wrong-owner
targets, concurrent target changes, oversized files, and invalid merged config
are rejected. New files are mode `0600`.

Configuration management is local Node only. These commands cannot inspect or
mutate a registered remote Node's config; use an interactive Boomux client on the
owning Node. Do not attempt remote config mutation through SSH, federation, or
another public Boomux command.

Discover the same configured projects locally without starting the daemon:

```console
boomux project list --json
boomux project list --node "<node>" --json
```

Use the canonical `path` only on the Node that returned it when creating a
Node-hosted item. An empty
list with `roots_configured: false` means no roots are configured; inspect
`warnings` when configured roots cannot be scanned.

## Manage Workspaces

```console
boomux workspace list
boomux workspace create "<name>"
boomux workspace select "<name-or-id>"
boomux workspace current
boomux workspace clear
boomux workspace open "<name-or-id>"
boomux workspace open "<name-or-id>" --show
boomux workspace inspect "<name-or-id>"
boomux workspace rename "<name-or-id>" "<new-name>"
boomux workspace adopt "<external-workspace-name-or-id>" --node "<node>"
boomux workspace link "<global-workspace-name-or-id>" "<owner-workspace-name-or-id>" --node "<node>"
boomux workspace close "<name-or-id>"
boomux workspace retry "<closing-global-workspace-name-or-id>"
```

On protocol 38 or newer, `workspace create` creates empty coordinator metadata.
It does not select a Node or store a path; choose those when creating the first
Shell, launcher, or schedule. Older daemons retain empty local Workspace
creation. Exactly one eligible Node may be selected implicitly; multiple eligible
Nodes require explicit `--node`, while zero means creation is unavailable.
Filesystem paths are interpreted only
by that owner, and its first hosted resource establishes the placement. An
explicit `--node` never falls back to local Shell or launcher creation.

On protocol 38 or newer, `workspace select` persists the exact coordinator
Workspace ID as local owner-only CLI preference state; Node-local owner
Workspace IDs are not selectable. For commands that require workspace context, explicit input
wins, then the current managed Workspace, then the selection. Shell, launcher,
and Schedule creation may omit their workspace while selected, but Node
selection remains independent. Exact resource IDs need no workspace, and an
omitted filter on Agent, attention, Session, Schedule, or execution lists still
means all Workspaces. Agent registration still requires an exact ShellRun.
Selection survives rename, fails visibly after close, and is removed only by
`workspace clear` or replacement with another selection.

Adopt creates a coordinated Workspace from one unlinked owner Workspace. Link's
argument order is global Workspace first, owner Workspace second. Both require a
current eligible owner and fresh identity-pinned revision. Equal names never
establish membership.

`workspace close` terminates every running Shell process and removes launchers,
Schedules and persisted prompts, retained terminal state, and Agent/attention
records after each owner confirms removal. A coordinated close can remain
`closing`; unresolved membership is retained until `workspace retry` or repeated
close succeeds. Canonical OpenCode, Pi, or Codex host data is not deleted. A Workspace
cannot close itself from one of its own Shells. Confirm the exact target and full
multi-Node removal scope first.

`workspace open` is an active, non-transactional restore operation. For a
coordinated Workspace it attempts every currently available placement once and
reports per-Node failures; unavailable membership remains visible, and ambiguous
launcher outcomes are not replayed automatically. Each Shell opens with
takeover, disconnecting its current writable controller, and an exited Shell
restarts as a new run. `workspace open OWNER_ID --node NODE` instead addresses
one owner-local Workspace and requires the exact owner Workspace ID remotely.
Obtain explicit authorization before opening. A launcher-only Workspace is
valid, but an empty Workspace cannot be opened.
For a coordinated Workspace, `--show` reveals its optional desktop layer before
the restore so terminal and launcher windows are presented there. It cannot be
combined with `--node`.

## Manage Workspace Launchers

```console
boomux launcher list --workspace "<workspace-name-or-id>"
boomux launcher create "<name>" --workspace "<workspace-name-or-id>" --node "<node>" --cwd "/owner/path" -- command arg
boomux launcher inspect "<name-or-id>" --workspace "<workspace-name-or-id>"
boomux launcher invoke "<name-or-id>" --workspace "<workspace-name-or-id>"
boomux launcher invoke "<exact-launcher-id>" --node "<node>"
boomux launcher rename "<name-or-id>" "<new-name>" --workspace "<workspace-name-or-id>"
boomux launcher remove "<name-or-id>" --workspace "<workspace-name-or-id>"
```

Launchers are durable ordered definitions, but each invocation is a detached,
ephemeral process without a PTY or retained output. Commands are exact argument
vectors; use an explicit shell for pipelines or expansion. `--cwd` defaults to
the current directory. Removing a launcher does not terminate applications from
earlier invocations. Exact launcher IDs are global only within one Node; local
names require current Workspace context or `--workspace`. Remote invocation
requires the exact launcher ID and its owning `--node`.

## Manage Shells

```console
boomux shell suggest-name "<workspace-name-or-id>" --json
boomux shell suggest-name "<exact-owner-workspace-id>" --node "<node>" --json
boomux shell create "<workspace-name-or-id>" --node "<node>"
boomux shell create "<workspace-name-or-id>" --node "<node>" --name "<name>" --cwd "/owner/path"
boomux shell create "<workspace-name-or-id>" --name "<name>" -- command arg
boomux shell inspect "<name-or-id>" --workspace "<workspace-name-or-id>"
boomux shell rename "<name-or-id>" "<new-name>" --workspace "<workspace-name-or-id>"
boomux shell close "<name-or-id>" --workspace "<workspace-name-or-id>"
```

`shell create` records a pending shell. Without `--cwd`, it uses the workspace
default when present and otherwise the current directory. An unavailable stored
default is an error rather than a silent fallback. Omit `--name` to let Boomux
generate a unique lowercase `adjective-noun` shell name. A shell cannot close itself through the CLI.
Closing one shell terminates its process session and removes its retained
terminal state, but durable Agent records remain in the workspace as historical
occurrences.

`shell suggest-name` returns `boomux.cli/v1` command `shell.suggest-name` with
exact `workspace_id` and nonempty `name`, without creating or reserving a shell.
Use it only as a UI suggestion. Creation can still fail with typed
`already_exists` if another operation claims the name first; request another
suggestion rather than changing or inferring a name.
Remote suggestion requires the exact owner Workspace ID plus `--node`.

The contextual close shorthand is:

```console
boomux close "<shell-name-or-shell-id>"
boomux close --focused
```

Focused close revalidates the exact Shell and routes a remote focused Shell to
its owning Node. It requires daemon protocol 39, fails when no Boomux terminal
has reported focus, and never allows a managed Shell to close itself from inside
that Shell.

## Open Shells

Open an exact shell ID in a new native terminal window:

```console
boomux open "<shell-id>"
boomux open "<shell-id>" --workspace "<coordinated-workspace>"
boomux open "<shell-id>" --title "<window-title>"
boomux open "<shell-id>" --takeover
boomux open "<exact-shell-id>" --node "<node>"
boomux --terminal "kitty.desktop" open "<shell-id>"
```

`--takeover` disconnects the current writable controller. Do not use it without
the user's consent. Opening an exited shell explicitly restarts its stored
command as a new run on the same durable shell identity; use `boomux read` when
the goal is only to inspect retained output. Remote open requires the exact
Node-local Shell ID plus its owning `--node`; never infer either from a name.
`--workspace` validates that the Shell belongs to an active placement in the
named coordinated Workspace. When the Hyprland desktop layer is enabled, it
shows that Workspace and places or reuses only the requested terminal there;
it does not invoke launchers or open sibling Shells.

## Manage The Daemon

```console
boomux daemon status
boomux daemon restart
boomux daemon stop
```

These commands affect only this Node. `restart` performs a transactional graceful handoff that preserves pending,
running, and exited shells, including final exited terminal state, and reconnects
active clients. It also preserves the Shared Harness Runtime PID and generation;
TUI holders reacquire non-transferred Agent Session Claims. It does not start a
new process for an exited shell. `stop` terminates every managed process session
and the Shared Harness Runtime, then stops the daemon; durable definitions
remain. A later daemon-backed command can start Boomux again;
recovered shells are pending and opening them starts new processes. Process
memory, PTYs, and mutated environments do not survive. Confirm before either
operation when the user did not request it explicitly. They do not restart or
stop daemons on registered remote Nodes.

## Install Or Update This Skill

```console
boomux skill install
boomux skill install --force
```

The skill is installed at `~/.agents/skills/boomux/SKILL.md`. Installation has
no dry-run mode. Use `--force` only with authorization to overwrite different
existing content. After every successful install, the installer checks
`~/.agents/skills/boomux-shells`: an exactly untouched single-file legacy skill
is deleted automatically, while customized content or additional files are
preserved with a warning. Obtain authorization for installation and this
possible legacy removal.

## Direct OpenCode Install Shortcut

Prefer `boomux integration setup opencode` for guided status, preview, consent,
restart, and verification guidance. `boomux opencode install` is an equivalent
immediate-write shortcut; use it only after the user authorizes installation at
the resolved global target.

The paired bundled plugins target the source-visible OpenCode TUI and server APIs
at the `opencode-ai` `1.18.18` compatibility point. TUI behavior is version-gated
because that API is not stable. This is not a runtime pin or a claim of live
shared-runtime validation.

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

For seamless use, create or open an ordinary Boomux login Shell and type bare,
zero-argument interactive `opencode`. Only there, the scoped runtime `PATH` shim
redirects internally to stock `opencode attach` for the Node's Shared Harness
Runtime. Arguments, subcommands, noninteractive calls, calls outside Boomux,
absolute paths, and calls after modifying `PATH` execute real OpenCode unchanged.
Do not ask for an Agent ID or invoke a special Boomux attach command.

The TUI plugin reactively claims selected root Sessions and updates claims after
switches and forks. The server plugin identifies one Agent by the claimed root
OpenCode Session and aggregates child/subagent activity. Work, tool, chat,
and compaction events map to `working`; outstanding permission/questions and
errors map to `blocked`; only root idle maps to `idle`; and only explicit root
session deletion maps to `done`. Repeated working activity is coalesced until a
meaningful state transition, so tool bursts do not create evidence-only durable
reports. Child deletion and process exit do not report completion. Unmanaged or
unavailable Boomux is fail-open. `--pure`, `--mini`, absolute paths, modified
`PATH`, and conflicting same-Session Shells fail closed without a claim or web
link. If Boomux returns `run_changed` or the runtime generation changes,
reporting for that root is disabled rather than redirected.

## Direct Pi Install Shortcut

Prefer `boomux integration setup pi` for guided status, preview, consent,
restart, and verification guidance. `boomux pi install` is an equivalent
immediate-write shortcut; use it only after the user authorizes installation at
the resolved global target.

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
reports `working`; after retries, compaction, and queued continuations settle, a
final assistant error reports `blocked` and a successful settle reports `idle`.
Session shutdown reports `inactive` because Pi sessions are resumable and makes
one bounded retry. Inactive records remain durable but do not occupy dashboard
Agent rows. Reporting is serialized and fail-open.

## Codex Integration

Use `boomux integration setup codex` for status, preview, consent, restart, and
verification guidance. The target is `${CODEX_HOME:-$HOME/.codex}/hooks.json`.
Installation and uninstall merge only exact `boomux codex hook` handlers and
preserve unrelated JSON and hooks. Modified Boomux handlers require explicitly
authorized `--force`; never replace the whole file. Restart Codex after a change,
then review and trust the Boomux hook with `/hooks`.

In a managed ShellRun, bare Codex chat, `codex resume`, and `codex exec` are
routed through an internal launcher that enables hooks only while the installed
handlers are current. The hook's `session_id` is the canonical thread identity.
Prompt, tool, compaction, and subagent activity report `working`; permission
waits report `blocked`; Stop reports `idle`; SessionEnd reports `inactive`; no
Codex hook reports `done`. Other subcommands and unscoped processes remain
untracked rather than guessing ownership.

Codex catalog discovery may execute the experimental PATH-resolved `codex
app-server --stdio` interface in workspace-derived directories. It is bounded,
sanitizes names or previews, excludes ephemeral threads, and fails open. Exact
resume uses `codex resume <thread-id>`. Boomux exposes no Codex Remote link because
there is no documented exact thread-specific Remote URL.

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
does not alter an already-running process environment. For protocol-35 remote
attachment, the owner Node supplies its own process environment; the presenting
Node's Unix environment is never forwarded.

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

This hidden command is intended only for prompt integrations:

```console
boomux prompt
```

It prints `workspace/shell` inside Boomux and nothing outside it. Use `boomux --help` and
`boomux <command> --help` when exact syntax or newly added options are needed.

Do not invoke private transport commands such as `__attach`, `daemon run`, or
`daemon receive-handoff`; they are implementation details rather than agent
APIs.

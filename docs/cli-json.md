# CLI JSON Contract

> **Status: Current stable contract.** Incompatible output requires a new schema
> value; human-readable output is not a parsing contract.

Boomux exposes a versioned JSON contract for integrations. Human output remains
the default; pass `--json` to a supported command to emit one
JSON document on stdout:

```json
{
  "schema": "boomux.cli/v1",
  "command": "shell.inspect",
  "data": {}
}
```

The `schema` value defines field semantics independently from the daemon wire
protocol. Fields documented by `boomux.cli/v1` will not change type or meaning.
Future incompatible output will use a different schema value.

## Discovery

`boomux capabilities --json` does not start or contact the daemon. It reports
the CLI version, daemon protocol version, supported JSON schemas and commands,
stable error codes, feature names, and the package and validated version for
each bundled integration host under `integration_hosts`. Validated versions are
compatibility test points, not runtime pins or minimum-version guarantees.
Integration ordering and host metadata come from the same capability descriptor
registry used by installation, foreground recognition, recovery, titles, and
session resume.
Protocol-backed feature names are derived from the same typed registry as
request gating and negotiated client feature checks, so each name has one
minimum protocol version authority.
The `desktop_notifications` feature means this binary supports daemon-owned
notification delivery; it does not imply notifications are enabled or that
`notify-send` and a desktop notification service are available. Configuration is
sampled when the daemon starts and is intentionally outside the JSON capability
contract.
The `sound_notifications` feature similarly advertises optional direct sound
delivery through `canberra-gtk-play`. `boomux notification test` is a human-facing
delivery diagnostic and does not support `--json`.

The following commands support `--json`:

- `boomux capabilities`
- `boomux list`
- `boomux shells`
- `boomux read`
- `boomux events`
- `boomux workspace list`
- `boomux workspace inspect`
- `boomux shell inspect`
- `boomux launcher list`
- `boomux launcher inspect`
- `boomux agent list`
- `boomux agent inspect`
- `boomux agent wait`
- `boomux agent register`
- `boomux agent ensure`
- `boomux agent report`
- `boomux attention list`
- `boomux attention acknowledge`
- `boomux session list`
- `boomux session inspect`
- `boomux schedule create`
- `boomux schedule list`
- `boomux schedule inspect`
- `boomux schedule pause`
- `boomux schedule resume`
- `boomux schedule remove`
- `boomux schedule run`
- `boomux execution list`
- `boomux execution inspect`
- `boomux execution wait`
- `boomux execution cancel`
- `boomux integration list`
- `boomux integration status [opencode|pi]`
- `boomux integration install <opencode|pi>`
- `boomux integration install --all`
- `boomux integration uninstall <opencode|pi>`
- `boomux integration uninstall --all`
- `boomux integration verify <opencode|pi>`
- `boomux daemon status`

JSON mutations are deliberately narrow. Agent register, ensure, and report;
attention acknowledgment; schedule create, pause, resume, remove, and run;
execution cancellation; and
integration install and uninstall support the contract. Other mutation commands
retain human output. Passing `--json` to an unsupported command fails with
`invalid_argument` before performing the operation.

`boomux integration setup <opencode|pi>` is intentionally human-oriented and
does not support `--json`. It composes status inspection, an exact install
preview, confirmation, installation when needed, and restart/verification
guidance. `--yes` skips confirmation for automation; replacing modified content
also requires `--force`.

Command payloads are:

- `capabilities`: CLI/protocol versions, integration host compatibility, plus
  arrays of schemas, commands, features, and error codes.
- `list`: a `shells` array.
- `shells`: workspace identity plus a `shells` array.
- `workspace.list`: a `workspaces` array of `id`, `name`, `shell_count`,
  `launcher_count`, `schedule_count`, `agent_count`, fixed `agent_state_counts`, and
  `attention_count`.
- `workspace.inspect`: one `workspace` object containing `id`, `name`, nullable
  `default_cwd`, and prompt-free `shells`, `launchers`, `schedules`, and `agents`
  arrays.
- `shell.inspect`: one `shell` object.
- `launcher.list`: workspace identity plus a `launchers` array.
- `launcher.inspect`: one `launcher` object.
- `agent.list`: an `agents` array, optionally limited by `--workspace`.
- `agent.inspect`: one `agent` object selected by exact agent ID.
- `agent.wait`: `changed` plus one exact `agent` object after a revision-aware
  conditional read.
- `agent.register`, `agent.ensure`, and `agent.report`: one resulting `agent`
  object. The command field identifies the specific mutation.
- `attention.list`: an `attention` array ordered blocked before completed, then
  newest observation first; `--workspace` limits it to one workspace.
- `attention.acknowledge`: `changed` plus the resulting `agent` object.
- `session.list`: a globally newest-first `sessions` array, optionally limited
  by exact workspace name or ID.
- `session.inspect`: one projected `session` object selected only by exact
  opaque session ID.
- `schedule.create`, `schedule.pause`, and `schedule.resume`: one prompt-free
  `schedule` summary. New schedules default to `fresh` and `paused`; resume
  changes state to `enabled`.
- `schedule.list`: a prompt-free `schedules` array, globally or limited by
  `--workspace`.
- `schedule.inspect`: one exact `schedule` detail and the only schedule command
  that includes its `prompt`.
- `schedule.remove`: `removed: true` plus the removed prompt-free `schedule`
  summary.
- `schedule.run`: one prompt-free `execution`. The CLI generates a UUID
  `dispatch_key` before the request unless `--idempotency-key` supplies one.
- `execution.list`: newest-first prompt-free `executions`, `limit`, `truncated`,
  `schedule_limit`, `schedules_truncated`, and schedule-keyed `schedules`
  next-occurrence projections, optionally filtered by `--workspace` and
  `--schedule`.
- `execution.inspect`: one prompt-free `execution` selected only by exact
  execution ID plus its separate nullable `next_occurrence` projection.
- `execution.cancel`: one prompt-free `execution` selected only by exact
  execution ID.
- `execution.wait`: `changed` plus one prompt-free exact `execution` after a
  revision-aware conditional read.
- `integration.list`: an `integrations` array containing bundled integration
  names, display names, packages, and validated host versions.
- `integration.status`: an `integrations` array containing independent `host`,
  `asset`, and `runtime` status objects. Status does not start the daemon or
  mutate integration files. It executes each PATH-resolved host's `--version`
  command with bounded output and runtime; missing or unhealthy integrations are
  represented as data and do not make status fail.
- `integration.install`: normally, an `integrations` array containing
  `installed`, `replaced`, or `unchanged` results, target paths, and whether a
  host restart is required. With `--dry-run`, `dry_run` is `true` and each entry
  instead contains `current_state`, the planned `action`, `path`, and
  `restart_required`. Each target is changed atomically; `--all` is not a
  transaction across hosts, but every target is preflighted before the first
  write.
- `integration.uninstall`: an `integrations` array containing `removed` or
  `not_installed` results, target paths, and whether a host restart is required.
  Every target is preflighted before the first removal.
- `integration.verify`: `integration`, `verified`, exact `shell_id` and `run_id`,
  plus the nonempty authoritative `agents` array. Failure uses typed
  `not_found`, `ambiguous_target`, `run_changed`, or `timeout` errors.
- `read`: shell/run identity, observed output revision, and rendered output.
- `events`: stream identity, reconnect cursor, optional baseline snapshot, and a
  bounded event array.
- `daemon.status`: `status`, `protocol_version`, `socket_path`, and nullable
  `scheduler`. Scheduler data contains `state`, `max_concurrent`, and
  `active_executions`. State is `active` only for a running worker whose latest
  evaluation and next-occurrence projection succeeded; otherwise it is
  `offline`.

## Integration Data

Integration arrays are ordered `opencode`, then `pi`. List entries contain
`name`, `display_name`, `package`, and `validated_version`.

Protocol 22 advertises `protocol_22`, `agent_schedule_management`, and
`durable_agent_schedules`. Protocol 23 adds `protocol_23`,
`scheduled_execution_dispatch`, `scheduled_execution_cancellation`, and
`schedule_owned_shells`. Protocol 24 adds `protocol_24`,
`timed_schedule_dispatch`, `scheduler_health`, and
`bounded_scheduled_execution_concurrency`.
Protocol 25 adds `protocol_25`, `revision_aware_scheduled_execution_wait`,
`bounded_scheduled_execution_history`, and
`scheduled_execution_notifications`.
Protocol 26 adds `protocol_26` and `exact_run_attachment` for the internal
additive expected-run attachment handshake used by Scheduled Execution opens.
Protocol 27 adds `protocol_27` and `agent_schedule_editing` for paused,
revision-conditional schedule-definition updates. Update responses and events
remain prompt-free; exact inspection is still the only response that discloses
the current prompt.

## Execution Data

Schedule objects include nullable `next_occurrence`, containing
`trigger_revision` and `scheduled_at_ms`. Execution objects contain `id`,
`workspace_id`, `schedule_id`, positive durable `revision`, `state`,
`dispatch_kind`, `dispatch_key`, exact `schedule_revision`, `prompt_revision`,
and `trigger_revision`, `requested_at_ms`, nullable `scheduled_at_ms`, nullable
`coalesced_through_ms`, start/end timestamps, snapshotted `cwd`,
`integration`, and `session`, nullable typed `reason` and `outcome`, and nullable
`shell_id`, `run_id`, `agent_id`, and discovered `external_session_id` links.
They never contain the retained prompt or environment.

`execution list --limit` defaults to 100 and accepts 1 through 1,000. The daemon
applies the bound after all filters and orders by `requested_at_ms` newest first,
then execution ID descending. `truncated` is true when more matching retained
records exist. Protocol-23 and protocol-24 peers retain their existing manual,
timed, and skipped visibility rules; a protocol-25 client locally bounds a list
returned by an older daemon.

On the daemon wire, `ListScheduledExecutions.limit` is optional. Protocol 25
defaults an absent value to 100 and clamps supplied values to 1 through 1,000.
Protocol-23 and protocol-24 requests ignore the field and remain uncapped; their
timed/skipped compatibility filter runs before any list operation. The
protocol-25 list response includes `schedules`, an array of `schedule_id` and
nullable `next_occurrence` projections for the complete selected schedule scope,
including schedules with no history and schedules absent from the execution
page. The array is sorted by schedule ID and independently capped at 100;
`schedule_limit` is 100 and `schedules_truncated` reports omitted schedules.
Exact inspection carries `next_occurrence` beside `execution`. These projections
are current scheduler calculations, not durable execution fields or execution
revision changes, and responses below protocol 25 omit them and return zero/false
projection metadata defaults. A current client connected to protocol 23 or 24
locally applies the caller's execution limit to the old unbounded response and
computes `truncated` from the received matching records.

`execution wait` requires protocol 25, an exact execution ID, and
`--after-revision`. A newer current revision returns immediately with `changed:
true`; an equal revision waits up to `--wait-ms` and returns the unchanged record
with `changed: false`; a future revision fails with `revision_ahead`. Revision
zero returns every existing execution immediately. Equal terminal process state
does not return early because later canonical Agent linkage may advance the
revision. Replacement wakes the request with `daemon_stopping`; reconnect and
repeat the same revision. Waits do not consume event cursors. `wait_ms` remains a
hard deadline while persistence or pending storage is blocked; timeout returns
the last committed exact snapshot and never an unpublished revision.

State is `skipped`, `claimed`, `starting`, `active`, `dispatch_failed`, `exited`,
`cancelled`, or `interrupted`; dispatch kind is `manual` or `timed`. Reasons are
stable safe values `overlap`, `active_session`, `workspace_capacity`,
`global_capacity`, `missed`, `paused_race`, `invalid_target`,
`runner_start_failed`, `host_spawn_failed`,
`cancelled_by_user`, `cold_daemon_recovery`, or
`runner_exited_without_report`; explicit daemon shutdown uses `daemon_shutdown`.
Exit outcomes are tagged
`exit_code` with `code` or `signal` with `signal`. These are process-orchestration
outcomes and never imply Agent `working`, `idle`, `blocked`, or `done`.

Status entries contain those four fields plus `host`, `asset`, `runtime`, and
`recommended_action`.
The `host` object contains `state`, `executable`, `version`, `compatibility`, and
`error`. Host state is `missing`, `available`, or `probe_failed`;
`compatibility` is `validated`, `unvalidated`, or `unknown`. Paths, versions, and
errors that are not available are JSON `null`. A validated version is an
observed compatibility test point, not a version requirement.

The `asset` object contains `state`, `path`, and `error`. Asset state is
`missing`, `current`, `modified`, or `unavailable`. The `runtime` object contains
`state`, `running_processes`, `tracked_processes`, and `untracked_processes`.
Runtime state is `not_observable`, `not_running`, `reporting`, or `untracked`.
Reporting requires an active exact shell/run Agent observation with
`lifecycle_integration` authority; process and terminal evidence do not satisfy
it.

`recommended_action` is `none`, `install`, `replace`, `restart_host`, or
`inspect_error`. Replacement requires explicit `--force`; the recommendation
does not authorize a mutation by itself.

Install entries contain `name`, `result`, `path`, and `restart_required`.
Result is `installed`, `replaced`, or `unchanged`. A modified target fails with
`already_exists` unless `--force` is supplied. Invalid roots or unsafe paths fail
with `invalid_argument` before mutation.

Install preview entries contain `name`, `current_state`, `action`, `path`, and
`restart_required`. Current state is `missing`, `current`, or `modified`; action
is `install`, `replace`, or `unchanged`. Preview performs the same path and
replacement validation as installation but does not create or modify anything.

Uninstall entries contain `name`, `result`, `path`, and `restart_required`.
Result is `removed` or `not_installed`. Only the bundled asset file is removed;
directories and unrelated host configuration are retained. Modified content
requires `--force`, while unsafe paths fail before any removal.

## Shell Data

Shell objects use stable scalar fields: `id`, `workspace_id`, `workspace_name`,
`name`, `cwd`, `owner`, `owner_schedule_id`, `status`, `exit_code`, and `run`.
Owner is `user` or `schedule`; only the latter has a non-null schedule ID and is
protected from direct rename, close, restart, and inactive open. Missing values are JSON `null`,
not omitted or represented as human placeholders. `status` is `pending`,
`running`, or `exited`.

A run object includes `id`, `generation`, `started_at_ms`, `ended_at_ms`,
`exit_reason`, `exit_code`, `output_revision`, and `environment_has_run_id`.
`exit_reason` is `exited`, `terminated`, `interrupted`, or `null`.

Launcher objects include `id`, `workspace_id`, `workspace_name`, `name`, `cwd`,
and `command`. `command` is the exact executable-and-arguments array.

Agent objects include stable fields `id`, `workspace_id`, `workspace_name`,
`shell_id`, `run_id`, `name`, `integration`, `external_session_id`,
`started_at_ms`, `ended_at_ms`, `attention`, and `observation`. The observation contains
`revision`, `state`, `authority`, `evidence`, `confidence`, and
`observed_at_ms`. Missing optional values are JSON `null`.

Agent `state` is `unknown`, `working`, `blocked`, `idle`, `inactive`, or `done`. `authority`
is `lifecycle_integration`, `process_adapter`, `terminal_heuristic`, or
`daemon_lifecycle`; CLI arguments use hyphens instead of underscores. Confidence
is an integer from 0 through 100. Public mutations accept the first three
authorities; `daemon_lifecycle` is reserved for daemon-originated observations.
External precedence is lifecycle integration over process adapter over terminal
heuristic. Observation revisions begin at 1 and increase with each accepted
changed report. Lower-authority and exact duplicate reports return success with
the unchanged snapshot. Equal-authority changed reports update the observation.
`done` is terminal and has a non-null `ended_at_ms`; an exact completion retry is
an unchanged success, while conflicting later reports fail. Completed records
remain durable and inspectable.

The positional name is optional for `agent register` and `agent ensure`. When
omitted, the CLI supplies a random lowercase `adjective-noun` value before the
request. The returned Agent object always contains the concrete durable name.
Explicit names and an existing record returned by `agent.ensure` are unchanged.

Protocol 15 raises one durable outstanding attention item for every accepted
`blocked` or `done` observation. The item records `reason` (`blocked` or
`completed`) and a copy of the exact raising observation, so later working or
idle reports do not erase an unseen blocker. A newer blocked observation or
terminal completion replaces the older item. Existing records migrated from an
older daemon start acknowledged and do not flood the queue.

`attention acknowledge <agent-id> --observation-revision <revision>` removes
only the item raised by that exact revision. A different outstanding revision
fails with `revision_ahead`; an already empty item is an idempotent unchanged
success. Acknowledgment does not alter the Agent lifecycle observation or
satisfy `agent wait`.

`agent.ensure` requires `--external-session-id` and protocol 10. Its identity key
is `integration`, `external_session_id`, `shell_id`, and `run_id`. When that key
already identifies a unique record, ensure returns the stored snapshot without
applying the supplied name or report. This is the intended identity-recovery
path after an integration reload. Otherwise it creates the record with the same
shape and validation as `agent.register`.

`agent.wait` requires protocol 14, an exact Agent ID, and
`--after-revision`. A current revision greater than the supplied revision returns
immediately with `changed: true`; an equal revision waits for at most `--wait-ms`
and returns `changed: false` on timeout. Revision zero therefore returns any
existing Agent immediately. A supplied future revision fails with
`revision_ahead`. A terminal `done` observation at the equal revision returns
unchanged immediately. Inactive sessions remain resumable and may advance later.
No-op ensure calls, duplicate reports, and lower-authority reports do not advance
the revision or satisfy a wait.

## Session Data

Session summaries contain `id`, `workspace_id`, `workspace_name`, `description`,
`integration`, `external_session_id`, `state`, `state_is_current`,
`started_at_ms`, `last_at_ms`, and `occurrence_count`. Registered-session
`description` is the latest stored Boomux Agent registration name. Catalog-only
OpenCode sessions use the sanitized host title, state `unknown`, and zero
occurrences. Missing optional values are JSON `null`.

Inspect includes all summary fields, session-level `source_cwd`, and ordered
`occurrences`. Each occurrence
contains `agent_id`, the original `shell_id` even if that shell was removed,
`retained_shell_name`, `retained_shell_cwd`, `source_cwd`, `run_id`,
`started_at_ms`, `ended_at_ms`, `is_current`, and the full stable Agent
`observation` shape. Retained shell fields are null after shell removal.
`source_cwd` is the registration-time Agent working directory under protocol 13
and can remain available for exact interactive session resume. Protocol-12
snapshots fall back to a currently retained shell directory. State and authority
use the same spellings documented for Agent observations.

Projection groups Agent instances only when workspace, integration, and external
session ID match. An Agent without an external session ID forms its own session.
Bounded OpenCode catalogs add root sessions to workspaces that reference the
same normalized directory. A matching durable Agent merges into the same stable
ID and supplies authoritative lifecycle state and occurrences.
Current state is selected from occurrences that are incomplete, non-inactive,
and bound to the current run of a running retained shell; otherwise state is the
latest stored observation and `state_is_current` is false. List order is newest
activity first, then workspace ID and session ID.

Session IDs are deterministic globally unique UUID v5 values, but consumers must
treat them as opaque. At a semantic level, Boomux hashes a frozen namespace and
versioned, length-prefixed encoding of workspace ID, integration, and grouping
identity (`external:<id>` or `instance:<agent-id>`). This algorithm is frozen to
keep emitted IDs stable, not exposed for callers to reproduce or guess. Only an
exact ID returned by `session.list` resolves; external IDs, descriptions, shell
IDs, and Agent IDs never resolve through `session.inspect`.
All session commands require a negotiated daemon protocol of at least 12 and return
`unsupported_version` before projection against an older daemon.

## Schedule Data

Schedule summaries contain `id`, `workspace_id`, nullable `workspace_name`,
`name`, `cwd`, `integration`, `session_mode`, nullable `external_session_id`,
`cron`, `timezone`, `state`, `overlap_policy`, `revision`, `prompt_revision`,
`trigger_revision`, `created_at_ms`, `updated_at_ms`,
`evaluation_frontier_ms`, nullable `execution_shell_id`, and nullable
`next_occurrence`. A non-null `next_occurrence` contains `trigger_revision` and
`scheduled_at_ms`. Optional values are JSON `null`; paused schedules have a null
next occurrence, while an accepted enabled schedule has a non-null next
occurrence. State is `paused` or `enabled`; session mode is `fresh` or `continue`;
the initial overlap policy is always `skip`.
Schedule management commands require negotiated daemon protocol 22. `schedule
run` and every `execution` command require protocol 23. Unsupported commands
return `unsupported_version` rather than presenting empty data.
`execution wait` specifically requires protocol 25.

Only `schedule.inspect` adds `prompt`. Prompts are intentionally absent from
list, create, pause, resume, remove, run, execution records, capabilities,
events, and errors. Inspection
is an explicit private-content disclosure. A prompt file is read once at create
time as exact UTF-8, including a trailing newline; later file changes do not
alter the persisted prompt revision. Files must be regular and prompts are
bounded to 65,536 UTF-8 bytes.

Create requires explicit `--workspace`, `--cwd`, and `--integration`, exactly
one of `--prompt` or `--prompt-file`, and exactly one trigger source. `--cron`
accepts the canonical numeric five-field subset. `--every Nm|Nh`, `--daily
HH:MM`, `--weekdays HH:MM`, and `--weekly DAY@HH:MM` compile to that canonical
cron representation. `--timezone` accepts an IANA timezone; omission snapshots
the current system IANA timezone or fails if it cannot be resolved. `--fresh`
conflicts with `--continue`, and `--paused` conflicts with `--enabled`.

`--continue` accepts only an exact opaque projected session ID resolved in the
selected workspace. The projection must have a canonical external session ID
and its integration must equal `--integration`; descriptions, external IDs,
latest-session selection, and names are never substitutes.

Exact schedule IDs resolve globally for inspect, pause, resume, and remove.
Schedule names resolve only with explicit `--workspace` or the exact current
`BOOMUX_WORKSPACE_ID`. List is global unless `--workspace` is supplied. Removing
a schedule removes its persisted prompt. Closing a workspace removes all owned
schedules and persisted prompts along with the workspace.

`read` returns `shell_id`, `run_id`, `output_revision`, `changed`, `status`, and
`output`. Output is a JSON string containing the same bounded plain rendered text
as human mode. These fields come from one atomic daemon observation. Conditional
reads can wait for a specific run to advance beyond a supplied revision.

`events` returns an opaque `<stream-uuid>:<event-id>` cursor. See
[`event-stream.md`](event-stream.md) for retention, graceful restart, cold
restart, and revision semantics.

## Errors

JSON failures write one document to stderr and exit nonzero:

```json
{
  "schema": "boomux.cli/v1",
  "command": "shells",
  "error": {
    "code": "not_found",
    "message": "shell not found: ..."
  }
}
```

Failures detected while parsing command-line arguments use `"command": "cli"`
because no valid command was selected.

The stable codes are reported by `boomux capabilities --json`. Messages remain
human-readable context and are not stable parsing targets. `run_changed` means
a supplied run ID no longer matches the run-scoped operation; integrations must
reacquire exact context rather than substitute or guess another run.
The CLI maps typed client failures directly: daemon `RemoteError` codes retain
their stable spelling, while transport, protocol, validation, and lifecycle
variants map to the existing CLI code set without inspecting message text.

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

### Accepted Remote Node Compatibility

Remote Node federation is an incremental extension tracked by #173. Protocol 31
advertises registration management explicitly; clients must not infer support
from a CLI or daemon version string. Registration commands manage local routes
only and do not imply projected reads or routed resource mutation.

Protocol 33 advertises the separately named `node.snapshot` combined read. It
contacts only the local daemon and returns qualified local plus bounded cached
remote projections. Legacy resource lists remain local-only.

Protocol 34 advertises `typed_exact_node_routing` and
`guarded_remote_management`. Dashboard effects carry structured Node-qualified
identities, freshly inspect the owner before destructive confirmation, and use
the resource revision, membership generation, run ID, Schedule/execution
revision, observation revision, or dispatch key required by the operation.
Stale Nodes and Nodes without the capability remain non-actionable.

Protocol 35 advertises `remote_pty_attachment` and
`owner_environment_attachment`. `boomux open SHELL --node SELECTOR` is a
human-facing local presentation command: the selector resolves to an exact
registered Node, the title includes its alias, and the hidden attachment carries
the exact qualified identity. It does not add a JSON method or change legacy
unqualified `open` semantics.

Protocol 36 advertises `typed_node_host_services` and separate capabilities for
remote project discovery, launcher invocation, integration management, Agent
Session catalogs, and exact Session resume. Commands with `--node SELECTOR`
resolve one registered Node and require its live verified protocol-36 service;
unsupported owners fail visibly and are never emulated against local PATH,
configuration, catalogs, or filesystems. Responses are live and transient and do
not update the cached Node projection.

Protocol 37 advertises `remote_agent_schedule_management` and
`remote_scheduled_execution_observation`. Schedule and execution commands accept
`--node SELECTOR`; their JSON adds exact `node_id`. Remote Schedule list is the
prompt-free reduced projection and includes Node freshness, health, observation
time, and scheduler health. Exact reads and all mutations are live owner-routed.

`capabilities` advertises only what the installed local CLI can speak and never
reports negotiated state for a registered Node. `node.list` and `node.inspect`
return registration data only. `node.snapshot` carries each Node's observed
protocol, capabilities, health, and observation time after contacting only the
local daemon. Integrations must keep static CLI support, registration data, and
per-Node runtime support distinct.

Legacy resource commands and JSON methods retain local-Node meaning and
local-only result sets. Protocol-38 unqualified Workspace list, inspect, open,
rename, close, and resource creation resolve coordinated Workspaces first; older
clients still receive only local resources. The separately named combined read
exposes Node identity, health, observation time, and stale state, while every
actionable resource is identified structurally by both its owning Node ID and
unchanged resource ID. Routed commands that accept names resolve them only in an
explicit Node context; `workspace open --node`, `shell suggest-name --node`, and
`launcher invoke --node` require exact owner resource IDs.
`workspace open TARGET --node NODE` accepts the local Node only by exact Node ID,
never by its display alias `local`, so an unlinked local owner Workspace cannot
be confused with a same-ID global Workspace. Registered remote Nodes retain
documented exact-ID or alias resolution.

Cached remote projections can satisfy only documented prompt-free summary reads.
Exact private inspection, terminal output, revision waits, and all mutations
require a live identity-verified connection to the owning Node. Offline writes
are never queued. Transport loss after an ambiguous write without an explicit
wire idempotency key returns a stable unknown-outcome error rather than replaying
the request. Identity change and unavailable-Node failures are also typed errors,
not message-parsed states.

Node registration and SSH bootstrap are separately authorized operations.
`node.add --json` never installs or replaces remote software. When discovery
proves that Boomux is absent it returns `install_required`; when every discovered
candidate is a known published build below the federation floor it returns
`upgrade_required`. Both messages direct the operator to rerun the human command
in an interactive terminal. Indeterminate executable, SSH, authentication,
handshake, identity, and newer-version failures retain their own failure rather
than being reported as either code, and no bootstrap mutation occurs.
After authorization the pinned binary is uploaded to private transaction state,
not the destination, and its current `daemon status --json` client proves the
running daemon executable. Automatic upgrade requires that process executable,
not merely the chosen old helper candidate, to exactly equal the install
destination. Missing or unprovable process identity returns `upgrade_required`
without activation. When no helper is discovered, a runtime probe must prove the
socket path absent before upload and guarded activation repeats the check; an
existing, racing, stale, or unprovable socket returns `install_required` without
destination replacement or stop.
On Linux, upgrade proof includes daemon PID, executable, negotiated protocol, and
socket device/inode. The uploaded current binary revalidates that fingerprint on
one open daemon connection immediately before activation; any race or unsupported
proof boundary returns `upgrade_required` before destination mutation. Bootstrap
platform preflight uses and verifies a fixed absolute command layout, so a
poisoned `PATH` cannot select bootstrap tools; a missing layout is
`bootstrap_unsupported_platform`.
JSON and background bootstrap never mirror raw SSH master stderr. They retain
only its bounded in-memory prefix for classification; overflow terminates the
private master and returns a fixed transport failure.
Bootstrap failures retain stable classes: `bootstrap_authentication_failed`,
`bootstrap_transport_failed`, `bootstrap_malformed_helper`,
`bootstrap_unsupported_platform`, `bootstrap_install_failed`, and
`bootstrap_commit_outcome_unknown`. The last code is retryable and occurs only
when the commit result is lost or malformed after successful installed-helper
handshake and protocol ping; it does not promise rollback, and an exact retry
rediscovers a compatible committed helper. Relative, oversized, or
control-character-containing paths in framed discovery output are
`bootstrap_malformed_helper`, not transport failures. A newer helper
uses `unsupported_version`; changing or conflicting verified identities use
`node_identity_changed` and `node_identity_conflict`. These classes contain only
bounded diagnostics and are not collapsed into `invalid_argument` or `internal`.
Install-stage failures identify a fixed non-secret stage such as stream, backup,
activation, or watchdog readiness. They never include raw remote stderr, paths,
shell startup output, or streamed bytes.
The backup stage also covers rejection of an existing destination that is not an
owner-owned bounded regular executable, metadata or byte-copy mismatch, and
copy/fsync failure. These failures occur before atomic destination activation and
leave the old executable pathname untouched.
Bootstrap returns a connection only after exactly one live protocol ping. Ready
helpers are pinged after connection; installed helpers use the pre-commit ping
and are not pinged again by add, retarget, ad hoc, or dashboard callers. Failure
of the required Ready ping remains a bootstrap failure before registration.
`bootstrap_runtime_unavailable` identifies missing, malformed, unsupported, or
unsafe remote daemon runtime discovery. Linux derives `/run/user/<numeric uid>`
only when the remote environment omits `XDG_RUNTIME_DIR`; no local environment is
forwarded. Post-install status, restart, helper verification, handshake, and ping
failures identify that fixed stage without exposing remote stderr.
An already-active remote bootstrap transaction returns the existing stable
`busy` code before changing the destination.
Per-Node observed capabilities must distinguish passive combined projection from
process-starting, destructive, integration-management, Schedule, and
exact-attachment support. The full compatibility and privacy rules are defined
in [`remote-nodes.md`](remote-nodes.md).

`boomux node rekey` is an implemented local identity-administration command. It
requires an interactive terminal and exact current-ID confirmation, and does not
support `--json`; it cannot be routed through federation.

The following commands support `--json`:

- `boomux capabilities`
- `boomux list`
- `boomux shells`
- `boomux read`
- `boomux events`
- `boomux project list`
- `boomux workspace list`
- `boomux workspace inspect`
- `boomux node add`
- `boomux node list`
- `boomux node inspect`
- `boomux node snapshot [SELECTOR]`
- `boomux node rename`
- `boomux node retarget`
- `boomux node forget`
- `boomux shell suggest-name <workspace-name-or-id>`
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
- `boomux execution open`
- `boomux execution cancel`
- `boomux integration list`
- `boomux integration status [opencode|pi]`
- `boomux integration install <opencode|pi>`
- `boomux integration install --all`
- `boomux integration uninstall <opencode|pi>`
- `boomux integration uninstall --all`
- `boomux integration verify <opencode|pi>`
- `boomux daemon status`

JSON mutations are deliberately narrow. Node registration add, rename, retarget,
and forget; Agent register, ensure, and report;
attention acknowledgment; schedule create, pause, resume, remove, and run;
execution open and cancellation; and
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
- `project.list`: `roots_configured`, a `projects` array, and a `warnings`
  array. Without `--node` this command reads local configuration and the
  filesystem without starting or contacting the daemon. With `--node`, the same
  bounded discovery runs on the verified owner and contacts the local routing
  daemon.
- `workspace.list`: on protocol 38, `workspaces` contains coordinator Workspace
  snapshots and `external_workspaces` contains unlinked qualified owner
  singletons. On older protocols it retains the local summary array of `id`,
  `name`, resource counts, fixed `agent_state_counts`, and `attention_count`.
- `workspace.inspect`: on protocol 38, a global target returns coordinator
  `id`, `revision`, `name`, `closing`, and explicit placements. External or
  older local targets retain the owner Workspace inspection shape.
- `node.list`: an alias-then-Node-ID ordered array of registration objects.
- `node.add`, `node.inspect`, `node.rename`, `node.retarget`, and `node.forget`:
  one registration object with `alias`, exact `target`, pinned `node_id`,
  `revision`, and `tombstone_epoch`. Add and retarget verify the SSH route before
  the local mutation; forget performs no SSH operation.
- `node.snapshot`: a `nodes` array containing the authoritative rich local Node
  and bounded reduced cached remote Nodes. `SELECTOR` is an exact Node ID or
  local alias; omission returns the all-Node overview. The local Node has alias
  `local`. A selector matching both that alias and a registration returns typed
  `ambiguous_target`; exact Node IDs disambiguate. Entries contain `node_id`,
  `alias`, `local`, nullable `route`, nullable `registration_revision`, `health`,
  `current`, `stale`, `observed_at_ms`, nullable
  `observed_protocol_version`, `observed_capabilities`, `scheduler`, and nullable
  `workspace_owner_eligible`, nullable `workspace_owner_unavailable_reason`,
  `local_snapshot`, and `remote_projection` payloads. Every resource `id` and
  relationship ID in those payloads is `{ "node_id": "...", "inner_id":
  "..." }`; inner IDs are unchanged and must not be routed without their Node.
  Protocol 38 also adds `workspaces` and `external_workspaces` arrays.
  `workspaces` contains coordinator-owned `id`, `revision`, `name`, `closing`,
  and explicit placements with Node ID, owner Workspace ID, observed owner
  revision, nullable owner-local Workspace name, nullable owner-local `default_cwd`, and `active`, `close_pending`, or
  `unavailable` state. `external_workspaces` contains unlinked qualified owner
  identity, revision, name, nullable owner-local default cwd, and availability.
  Protocol-37 and older responses omit both additive arrays. Protocol 39 adds
  nullable `focused_terminal` with a monotonic presentation `revision` and exact
  qualified `shell`; protocol-38 responses omit it.
- `shell.suggest-name`: exact resolved `workspace_id` plus a nonempty generated
  `name` that does not match any shell name in that workspace at observation
  time. Protocol-38 responses also include exact `node_id` for local and routed
  owners; older local responses omit it.
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
- `execution.open`: the exact prompt-free `execution` plus `target`, either
  `run` for a starting or active exact-run attachment or `session` for an
  external resume of the execution's exact linked Agent Session. It never
  restarts the reusable Schedule runner shell or substitutes a later run.
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
- `daemon.status`: `status`, `protocol_version`, `socket_path`, nullable `pid`,
  `executable`, `socket_device`, and `socket_inode`, and nullable `scheduler`. On Linux, these identity fields
  are populated only after same-user `SO_PEERCRED` and bounded absolute
  `/proc/<pid>/exe` validation; the executable has a kernel ` (deleted)` suffix
  removed. Other platforms or failed proof return null. Scheduler data contains
  `state`, `max_concurrent`, and
  `active_executions`. State is `active` only for a running worker whose latest
  evaluation and next-occurrence projection succeeded; otherwise it is
  `offline`.

## Project Data

`project.list` discovers the same configured project suggestions used by the
dashboard. Each project contains `name`, its canonical absolute `path`, the
configured-root `group`, and zero-based `group_order`. `roots_configured` is
false when the merged configuration has no project roots, distinguishing that
state from configured roots that currently discover no projects. Recoverable
scan problems and resource-limit notices are returned as human-readable strings
in `warnings`; integrations must not parse warning text.

## Shell Name Suggestions

`boomux shell suggest-name <workspace-name-or-id> --json` returns a lowercase
`adjective-noun` suggestion using the same catalog and collision-exclusion
algorithm as generated shell creation. The suggestion is not reserved. Another
operation can claim it before creation, so a later explicit `shell create
--name <suggestion>` can still fail with `already_exists`; callers must handle
that typed error rather than assume the suggestion grants ownership. If every
generated name is currently in use, suggestion fails with `already_exists`.

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
Protocol 32 adds `protocol_32`, `node_projection_sync`, and
`bounded_remote_node_projections`. Protocol 33 adds `protocol_33`,
`combined_node_snapshot`, and `node_qualified_dashboard`.
Protocol 34 adds `protocol_34`, `typed_exact_node_routing`, and
`guarded_remote_management`.
Protocol 35 adds `protocol_35`, `remote_pty_attachment`, and
`owner_environment_attachment`.
Protocol 36 adds `protocol_36`, `typed_node_host_services`,
`remote_project_discovery`, `remote_launcher_invocation`,
`remote_integration_management`, `remote_agent_session_catalog`, and
`remote_exact_session_resume`.
Protocol 37 adds `protocol_37`, `remote_agent_schedule_management`, and
`remote_scheduled_execution_observation`.
Protocol 38 adds `protocol_38`, `global_workspaces`,
`multi_node_workspace_placements`, `guarded_workspace_adoption`, and
`resumable_workspace_close`.
Protocol 39 adds `protocol_39` and `qualified_focused_terminal`.

Protocol-38 `workspace create` creates empty coordinator metadata without a
default Node or cwd. First global `shell create`, `launcher create`, or
`schedule create` resolves `--node` against eligible owners. If exactly one
owner is eligible it may be used without `--node`; zero or multiple eligible
owners return a typed selection error listing disabled health reasons. The
owner-local cwd is resolved on that Node. Exact argv arrays and private Schedule
prompts are unchanged by coordination. `workspace open`, `close`, `rename`,
`list`, and `inspect` resolve global IDs or names before considering external
local records.
`workspace adopt TARGET --node NODE`, `workspace link GLOBAL OWNER --node NODE`,
and `workspace retry GLOBAL` expose guarded adoption, linking, and unresolved
close retry without requiring the TUI. Repeating `workspace close` for a closing
global Workspace also uses the retry operation. Dashboard project suggestions
contribute only the Workspace name; they create the same empty coordinator
metadata as by-name creation. Node and path selection begins with first-resource
creation.
An explicit `--node` on `shell create` or `launcher create` never falls back to
local mutation. It fails with `unsupported_version` when global Workspaces are
unavailable and with `invalid_argument` when the target is not coordinated.
`workspace adopt`, `link`, and `retry` likewise fail with `unsupported_version`
before target resolution on a pre-38 daemon.
Prepared resource requests carry one caller-stable operation UUID. The client
retries the exact request once after a lost connection, timeout, unknown outcome,
or coordinator persistence failure. The coordinator returns the durable prior
success before evaluating a now-stale revision guard. This replay guarantee lasts
while the success remains among at most the newest 256 completed operations and
within the 1 MiB coordinator-store bound; oldest-first eviction ends the
guarantee. Prepared requests reserve their completed-response footprint within
the 1 MiB store and fail before owner mutation when capacity is unavailable.
Adoption and linking fetch a fresh
protocol-38 combined local snapshot over the admitted identity-pinned route and
require its runtime `global_workspaces` capability before using the owner revision;
cached eligibility cannot authorize them.

Node snapshot health is `unobserved`, `online`, `reconnecting`, `stale`,
`unreachable`, `authentication_required`, `identity_changed`,
`identity_conflict`, or `unsupported`. `current` is true only for a live
identity-verified observation. Cached rows remain visible when stale, but cached
state never authorizes terminal reads, private inspection, Schedule controls, or
mutation. Scheduler health is reported independently for every Node.

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
`session list`, `session inspect`, and human-only `session resume` accept
`--node SELECTOR` under protocol 36. Remote JSON responses add the exact
`node_id`. Resume opens a local native terminal but resolves the opaque ID and
executes the integration argv only on the owner; it creates no ordinary
Workspace or Shell.

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
Node-qualified Schedule and execution commands require protocol 37 on the local
coordinator and owner. Remote JSON responses add exact `node_id`; remote list
presentation also reports Node freshness and per-Node scheduler health. Remote
create reads prompt files locally once, validates cwd and continuation Session on
the owner, and never stores the prompt in the presenting Node's projection.

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

Exact Schedule IDs resolve globally only within the selected Node for inspect,
pause, resume, and remove. Schedule names resolve only with explicit
`--workspace` or the exact current `BOOMUX_WORKSPACE_ID`. List covers the
selected Node unless `--workspace` narrows it. Removing a Schedule removes its
persisted prompt. Closing a Workspace removes all owned Schedules and persisted
prompts along with the Workspace.

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

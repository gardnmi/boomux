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
transcripts.
Protocol-backed feature names are derived from the same typed registry as
request gating and negotiated client feature checks, so each name has one
minimum protocol version authority.
`session_transcript_integrations` lists the integration keys whose descriptors
declare canonical transcript support.
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
- `boomux session read`
- `boomux integration list`
- `boomux integration status [opencode|pi]`
- `boomux integration install <opencode|pi>`
- `boomux integration install --all`
- `boomux integration uninstall <opencode|pi>`
- `boomux integration uninstall --all`
- `boomux integration verify <opencode|pi>`
- `boomux daemon status`

JSON mutations are deliberately narrow: only `agent register`, `agent ensure`,
`agent report`, `attention acknowledge`, `integration install`, and
`integration uninstall` support the contract. Other mutation commands retain
human output. Passing `--json` to an unsupported command fails with
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
  `launcher_count`, `agent_count`, fixed `agent_state_counts`, and
  `attention_count`.
- `workspace.inspect`: one `workspace` object containing `id`, `name`, nullable
  `default_cwd`, and `shells`, `launchers`, and `agents` arrays.
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
- `session.read`: one bounded canonical `transcript` selected only by exact
  opaque session ID.
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
- `daemon.status`: `status`, `protocol_version`, and `socket_path`.

## Integration Data

Integration arrays are ordered `opencode`, then `pi`. List entries contain
`name`, `display_name`, `package`, and `validated_version`.

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
`name`, `cwd`, `status`, `exit_code`, and `run`. Missing values are JSON `null`,
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
and can remain available for canonical transcript lookup. Protocol-12 snapshots
fall back to a currently retained shell directory. State and authority use the
same spellings documented for Agent observations.

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
IDs, and Agent IDs never resolve through `session.inspect` or `session.read`.
All session commands require a negotiated daemon protocol of at least 12 and return
`unsupported_version` before projection against an older daemon.

`session.read` supports OpenCode and Pi sessions with a canonical external
session ID and a retained working directory. It reads OpenCode's export and Pi's
project JSONL rather than terminal scrollback. Pi projection follows the current
leaf parent chain and excludes abandoned branches. Tool-result messages are
combined with their tool calls.

The transcript contains `session_id`, `integration`, `external_session_id`,
`entries`, `returned_entries`, `total_entries`, `content_bytes`, `truncated`,
`truncated_by`, `has_more`, and `next_cursor`. Entries are a newest suffix
returned in chronological order.
Their `type` is `message`, `reasoning`, or `tool`; common fields are `source_id`,
`timestamp_ms`, and `truncated`. Message and reasoning entries add `role` and
`text`. Tool entries add `tool_name`, `tool_call_id`, `status`, `input`, and
`output`; input is compact host JSON encoded as a string. Inapplicable fields are
omitted except `source_id` and `timestamp_ms`, which are JSON `null` when the host
does not provide them.

`--limit` defaults to 100 and accepts 1 through 1,000 entries. `--max-bytes`
defaults to 1 MiB and accepts 1 byte through 4 MiB. `content_bytes` counts UTF-8
bytes in returned text, input, and output fields. `truncated_by` contains `limit`
and/or `max_bytes`; a partially clipped entry also has `truncated: true`. Boomux
does not redact canonical host content. Raw host source inspection is separately
capped at 16 MiB.

When `has_more` is true, `next_cursor` is a non-null opaque string for
`session read --before <cursor>`; otherwise it is JSON `null`. Continuation moves
toward older logical entries, and bounds may change between requests. The cursor
binds the projected session, adapter normalization, retained source context, and
initial normalized transcript. Entries appended after the first page are ignored
only when the normalized baseline remains an exact prefix. Existing-entry edits,
tool-result updates, removals, reordering, Pi branch changes, source-context
changes, and adapter-normalization changes return
`cursor_expired`; callers discard the cursor and request a fresh newest page.
Malformed, oversized, or cross-session cursors return `invalid_argument`.
Pagination remains client-side and does not create daemon cursor state.

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

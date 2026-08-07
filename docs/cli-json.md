# CLI JSON Contract

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
`session_transcript_integrations` lists the integration keys with registered
canonical transcript adapters.

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
- `boomux agent register`
- `boomux agent ensure`
- `boomux agent report`
- `boomux session list`
- `boomux session inspect`
- `boomux session read`
- `boomux daemon status`

JSON mutations are deliberately narrow: only `agent register`, `agent ensure`,
and `agent report` support the contract. Other mutation commands retain human
output. Passing `--json` to an unsupported command fails with
`invalid_argument` before performing the operation.

Command payloads are:

- `capabilities`: CLI/protocol versions, integration host compatibility, plus
  arrays of schemas, commands, features, and error codes.
- `list`: a `shells` array.
- `shells`: workspace identity plus a `shells` array.
- `workspace.list`: a `workspaces` array of `id`, `name`, `shell_count`,
  `launcher_count`, and `agent_count`.
- `workspace.inspect`: one `workspace` object containing `id`, `name`, and
  `shells`, `launchers`, and `agents` arrays.
- `shell.inspect`: one `shell` object.
- `launcher.list`: workspace identity plus a `launchers` array.
- `launcher.inspect`: one `launcher` object.
- `agent.list`: an `agents` array, optionally limited by `--workspace`.
- `agent.inspect`: one `agent` object selected by exact agent ID.
- `agent.register`, `agent.ensure`, and `agent.report`: one resulting `agent`
  object. The command field identifies the specific mutation.
- `session.list`: a globally newest-first `sessions` array, optionally limited
  by exact workspace name or ID.
- `session.inspect`: one projected `session` object selected only by exact
  opaque session ID.
- `session.read`: one bounded canonical `transcript` selected only by exact
  opaque session ID.
- `read`: shell/run identity, observed output revision, and rendered output.
- `events`: stream identity, reconnect cursor, optional baseline snapshot, and a
  bounded event array.
- `daemon.status`: `status`, `protocol_version`, and `socket_path`.

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
`started_at_ms`, `ended_at_ms`, and `observation`. The observation contains
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

`agent.ensure` requires `--external-session-id` and protocol 10. Its identity key
is `integration`, `external_session_id`, `shell_id`, and `run_id`. When that key
already identifies a unique record, ensure returns the stored snapshot without
applying the supplied name or report. This is the intended identity-recovery
path after an integration reload. Otherwise it creates the record with the same
shape and validation as `agent.register`.

## Session Data

Session summaries contain `id`, `workspace_id`, `workspace_name`, `description`,
`integration`, `external_session_id`, `state`, `state_is_current`,
`started_at_ms`, `last_at_ms`, and `occurrence_count`. `description` is the
latest stored Boomux Agent registration name, never a synchronously fetched host
title. Missing optional values are JSON `null`.

Inspect includes all summary fields plus ordered `occurrences`. Each occurrence
contains `agent_id`, the original `shell_id` even if that shell was removed,
`retained_shell_name`, `retained_shell_cwd`, `run_id`, `started_at_ms`,
`ended_at_ms`, `is_current`, and the full stable Agent `observation` shape.
Retained shell fields are null after shell removal. State and authority use the
same spellings documented for Agent observations.

Projection groups Agent instances only when workspace, integration, and external
session ID match. An Agent without an external session ID forms its own session.
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
`entries`, `returned_entries`, `total_entries`, `content_bytes`, `truncated`, and
`truncated_by`. Entries are a newest suffix returned in chronological order.
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

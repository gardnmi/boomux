# CLI JSON Contract

Boomux exposes a versioned JSON contract for read-only integrations. Human
output remains the default; pass `--json` to a supported command to emit one
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
stable error codes, and feature names.

The following commands support `--json`:

- `boomux capabilities`
- `boomux list`
- `boomux shells`
- `boomux read`
- `boomux events`
- `boomux workspace list`
- `boomux workspace inspect`
- `boomux shell inspect`
- `boomux daemon status`

Mutation commands intentionally retain human output for now. Passing `--json`
to an unsupported command fails with `invalid_argument` before performing the
operation.

Command payloads are:

- `capabilities`: CLI/protocol versions plus arrays of schemas, commands,
  features, and error codes.
- `list`: a `shells` array.
- `shells`: workspace identity plus a `shells` array.
- `workspace.list`: a `workspaces` array of `id`, `name`, and `shell_count`.
- `workspace.inspect`: one `workspace` object containing `id`, `name`, and
  `shells`.
- `shell.inspect`: one `shell` object.
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
human-readable context and are not stable parsing targets. Daemon protocol 7
retains the additive protocol-6 error field and adds `cursor_expired`,
`run_changed`, and `revision_ahead`.

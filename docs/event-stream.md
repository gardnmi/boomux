# Daemon Event Stream

Boomux protocol 7 adds bounded long polling for daemon events and atomic,
revision-aware terminal reads. Protocol-6 management clients remain compatible;
new clients negotiate down for legacy requests and reject protocol-7-only
requests with `unsupported_version`.

## Cursors

An event cursor is `<stream-uuid>:<event-id>`. Event IDs increase strictly within
one stream. `boomux events` without `--after` returns an immediate registry
snapshot and a cursor at that baseline. Supplying `--after` returns only newer
events, waits up to `--wait-ms`, and returns a new cursor.

The daemon retains 8,192 events and returns at most 256 per request. A cursor is
invalid when its stream differs, its event was evicted, or it points beyond the
latest event. The typed `cursor_expired` error tells consumers to request a new
baseline.

Retained payloads are bounded as well as counted. New workspace and shell names
are limited to 256 UTF-8 bytes; legacy persisted names remain loadable.

The stream ID, latest ID, and retained events transfer across transactional
graceful daemon restart. Cold startup creates a new stream, so cursors from a
stopped or crashed daemon expire. Event history is intentionally memory-backed;
the durable registry remains the cold-recovery authority.

## Events

The initial event vocabulary is:

- `workspace_created`, `workspace_renamed`, `workspace_closed`
- `shell_created`, `shell_renamed`, `shell_closed`
- `launcher_created`, `launcher_renamed`, `launcher_removed`
- `run_started`, `output_changed`, `run_exited`
- `handoff_completed`

Output events carry run identity and the latest output revision, not raw PTY
bytes. Consumers use revision-aware reads to retrieve the current bounded,
rendered terminal state.

Event IDs provide one total publication order. The daemon transition coordinator
couples durable lifecycle mutation, persistence, and event publication. Events
are published only after their corresponding state is persisted. If persistence
fails, the event batch remains pending; background recovery publishes queued
batches in transition order and exactly once.

The baseline snapshot and cursor are captured under this same coordinator, so no
transition can be published between those observations. PTY bytes are not
persisted on every chunk, but output revision mutation and `output_changed`
publication still cross the transition boundary together.

## Revision Reads

`boomux read --json` returns `run_id`, `output_revision`, `changed`, `status`, and
`output` from one terminal/lifecycle observation. Conditional reads provide both
`--run-id` and `--after-revision`, with optional `--wait-ms`.

- A newer revision returns the complete current rendered output and
  `changed: true`.
- An equal revision waits or returns empty output with `changed: false`.
- A different run returns `run_changed`.
- A future revision returns `revision_ahead`.
- Exit and daemon restart wake waiting reads.

Revisions are run-scoped reader-batch counters, not byte offsets. Rendered output
can rewrite earlier cells, so successful reads return complete bounded state
rather than byte deltas.

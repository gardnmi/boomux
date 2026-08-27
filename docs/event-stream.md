# Daemon Event Stream

> **Status: Current protocol contract.** Source and compatibility tests are
> authoritative for exact version gates; this document defines event and cursor
> semantics.

Boomux protocol 7 added bounded long polling for daemon events and atomic,
revision-aware terminal reads. Protocol 9 adds run-scoped agent snapshots and
events while retaining negotiation with older management clients. Protocol 10
adds idempotent agent ensure without adding an event type. Protocol 11 adds an
explicit exited-shell restart; the subsequent attachment emits the existing
`run_started` event for the new generation. Protocol 12 adds resumable
`inactive` Agent observations; older Agent-capable clients receive them as
`unknown`. Protocol 13 adds the registration-time Agent working directory to
snapshots and Agent events; protocol-12 clients receive the same records without
that additive source context. Protocol 14 adds revision-conditional Agent reads
that reuse the event condition variable for wakeups without consuming or
depending on the retained global event cursor.
Protocol 15 adds durable blocked/completed attention and conditional
acknowledgment. Protocol-14 clients receive Agent snapshots without attention
and do not receive acknowledgment events, while cursors still advance across
those filtered events. Protocol 18 adds the latest non-durable focused terminal
and its monotonic revision to baseline snapshots. It does not add an event type;
protocol-17 clients receive the same baseline without that field.
Protocol 21 adds a targeted focused-terminal read for clients that use events as
registry invalidation while refreshing ephemeral focus independently. Running
shell reads refresh foreground-process hints from a daemon-side one-second cache
shared by concurrent clients.
Protocol 26 adds exact-run attachment.
Protocol 32 adds owner-side reduced Node projection cuts and the local
`node_projection_changed` cache invalidation event.
Protocol 39 adds live Node-qualified focused-terminal presentation to the
combined Node snapshot and the prompt-free
`focused_terminal_presentation_changed` invalidation event. The event wakes
local presentation clients without carrying the focused identity; clients read
the combined snapshot for the current value. It is not copied into another
Node's projection transitions or persisted projection cache. A one-second
ephemeral refresh remains the fallback for legacy or missed invalidations.
Remote notification presentation adds no protocol or event kind. It consumes the
protocol-32 reduced transition batch at the projection-cache boundary and never
copies owner events into the presenting Node's domain journal.
Protocol 43 likewise adds no event kind or baseline field. Claude Remote Control
bindings are ephemeral local handoff presentation state, not durable Agent state
or reduced Node projection data.
Protocol 47 removes Agent Schedules and Scheduled Executions. Current snapshots
and event streams contain no schedule definitions, execution records, scheduler
health, or schedule events. Historical schedule request and event shapes are not
part of the protocol-47 wire contract.
Protocol 49 adds `workspace_default_cwd_changed` after the owner Workspace and
its revision are durably persisted. Protocol-48 clients do not receive the new
event, but their returned cursor still advances across it.

`boomux agent wait <id> --after-revision <revision>` is the preferred way to
await one Agent. It returns on a newer accepted durable observation, returns
unchanged on timeout, rejects future revisions, and wakes with
`daemon_stopping` during replacement. Callers reconnect and repeat with the same
revision; no waiter state is persisted.

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

### Accepted Federation Rules

Remote Node federation keeps the event boundary defined by
[`remote-nodes.md`](remote-nodes.md). Each Node retains its own stream UUID,
event IDs, retention, baseline, and cursor authority. There is no cross-Node
event order, and timestamps cannot be used to fabricate one.

A local federation coordinator persists one independently bounded prompt-free
projection and remote cursor per registered Node. It publishes only a local
projection invalidation after that cache replacement and cursor are persisted;
it does not copy remote domain events into the authoritative local journal.
Local clients can therefore continue using one local cursor that orders local
observations without reinterpreting a remote mutation as local authority.

Remote cursor expiry reseeds only the affected Node. A disconnect marks its
cached projection stale and publishes no Agent completion, attention
acknowledgment, or replacement dispatch. Recovered
baselines update presentation but emit neither historical notifications nor a
reconnect digest because the expired stream cannot prove which transitions were
unseen. Older clients remain local-only and do not receive federation
invalidations when the future capability is filtered.

For a resumable cursor, the presenting Node persists the complete reduced cut and
then uses exact transition revisions only to claim local notification
presentation. Continuous live synchronization can enqueue individual requests;
recovery from stale health can enqueue one per-Node digest. Both paths persist
bounded Node-cache schema-2 claims before enqueue. Cursor expiry, stream
replacement, first baselines, and stale or duplicate revisions enqueue nothing.
The resulting local `node_projection_changed` remains the only journal event.

## Events

The event vocabulary is:

- `workspace_created`, `workspace_renamed`, `workspace_default_cwd_changed`,
  `workspace_closed`
- `shell_created`, `shell_renamed`, `shell_closed`
- `launcher_created`, `launcher_renamed`, `launcher_removed`
- `run_started`, `output_changed`, `run_exited`
- `agent_registered`, `agent_state_changed`, `agent_completed`,
  `agent_attention_acknowledged`
- `node_projection_changed`
- `focused_terminal_presentation_changed`
- `handoff_completed`

Output events carry run identity and the latest output revision, not raw PTY
bytes. Consumers use revision-aware reads to retrieve the current bounded,
rendered terminal state. PTY readers coalesce output publication over a bounded
16-millisecond window, so event revisions may skip intermediate reader
revisions. Pause, stop, and exit boundaries force the latest revision into the
publication frontier.

Agent events carry the complete durable agent snapshot, including exact shell
and run IDs and the latest state, authority, evidence, confidence, observation
revision, and timestamps. Registration emits `agent_registered`; registration
as `done` also emits `agent_completed`. Later reports emit
`agent_state_changed`, except the terminal `done` report emits
`agent_completed`.

An ensure that reuses an identity emits no event and does not change its
observation revision. Lower-authority reports and exact duplicates are also
successful no-ops with no event. Equal-authority reports with changed content
are updates and emit the normal state-change or completion event. Retrying the
exact accepted `done` report is idempotent and emits no second completion event;
other reports against a completed instance are rejected.

`workspace_default_cwd_changed` carries the owner Workspace ID and resolved
absolute default cwd. It does not rewrite existing Shell or launcher cwd values.

Accepted blocked and completed observations also carry an outstanding attention
item in their Agent snapshot. `agent_attention_acknowledged` contains the full
resulting Agent snapshot after the item is removed. The acknowledgment is
conditional on its raising observation revision, persists before publication,
and does not increment the lifecycle observation revision.

Protocol-8 and older event clients do not receive protocol-9 agent snapshots or
agent events. Filtering does not rewrite the journal: their returned cursor
still advances across filtered agent events, preserving the stream's total
publication order. Agent get, register, and report requests require protocol 9;
ensure requires protocol 10.

Protocol-31 and older clients do not receive `node_projection_changed`; their
cursor still advances across the invalidation. A projection cut resumes only
when every remote event through its captured cursor fits within 256 records;
otherwise it returns a fresh reduced baseline and no transition history.

Event IDs provide one total publication order. The daemon transition coordinator
couples durable lifecycle mutation, persistence, and event publication. Events
are published only after their corresponding state is persisted. If persistence
fails, the event batch remains pending; background recovery publishes queued
batches in transition order and exactly once.
Lifecycle event reservations have distinct abort and persistence-transfer exits.
An abort releases and publishes queued runtime events immediately when no durable
batch or persistence operation still blocks publication. A successful lifecycle
mutation transfers the reservation to persistence atomically, keeping runtime
events behind the corresponding durable commit.

The baseline snapshot and cursor are captured under this same coordinator, so no
event can be published between those observations. PTY output revision advances
under per-runtime synchronization and can be newer than the latest published
event at that instant. Every unpublished advance causes a later
`output_changed` carrying that revision or a newer one. Event IDs remain a total
publication order, not a byte-arrival order.

## Revision Reads

`boomux read --json` returns `run_id`, `output_revision`, `changed`, `status`, and
`output` from one terminal/lifecycle observation. Conditional reads provide both
`--run-id` and `--after-revision`, with optional `--wait-ms`.

- A newer revision returns the complete current rendered output and
  `changed: true`.
- An equal revision waits or returns empty output with `changed: false`.
- A different run returns `run_changed`.

Conditional reads wait on the requested runtime directly. They can therefore
return a newer revision before its coalesced `output_changed` event is published.
- A future revision returns `revision_ahead`.
- Exit and daemon restart wake waiting reads.

Revisions are run-scoped reader-batch counters, not byte offsets. Rendered output
can rewrite earlier cells, so successful reads return complete bounded state
rather than byte deltas.

`run_changed` is a guard against observing or reporting against another process
incarnation. Consumers should inspect the shell and decide how to handle the new
run; they must not silently guess or replace the requested run ID.

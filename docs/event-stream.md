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
Protocol 22 adds durable Agent Schedule management. Workspace snapshots include
ordered, prompt-free schedule summaries, while exact schedule inspection is the
only management response that discloses the current prompt.
Protocol 23 adds durable manual Scheduled Executions and schedule-owned shells.
Protocol 24 adds daemon-native timed decisions, skipped policy outcomes,
deterministic next occurrences, and scheduler health.
Protocol 25 adds positive durable execution revisions, bounded execution-list
metadata, and exact revision-aware execution waits.
Protocol 26 adds exact-run attachment, and protocol 27 adds optimistic paused
schedule-definition updates with prompt-free update events.
Protocol 32 adds owner-side reduced Node projection cuts and the local
`node_projection_changed` cache invalidation event.
Protocol 37 adds remote Schedule management and execution observation without a
new event kind: owner events remain owner-local and the presenting Node receives
only its existing projection invalidation.
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

`boomux agent wait <id> --after-revision <revision>` is the preferred way to
await one Agent. It returns on a newer accepted durable observation, returns
unchanged on timeout, rejects future revisions, and wakes with
`daemon_stopping` during replacement. Callers reconnect and repeat with the same
revision; no waiter state is persisted.

`boomux execution wait <id> --after-revision <revision>` has the same
newer/timeout/future-revision and reconnect rules. Unlike terminal Agent `done`,
an equal terminal execution revision continues waiting because a lifecycle
integration may still link the canonical Agent for that exact ShellRun. The wait
uses the event condition variable as a wakeup signal and does not consume the
global cursor. It reads only the execution map installed at the persisted event
publication frontier. Persistence in flight, pending durable storage, and
lifecycle reservations do not extend `wait_ms`: timeout returns `changed: false`
with the last committed exact snapshot, including when the mutable record has an
unpublished revision or has been provisionally pruned.

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
cached projection stale and publishes no Agent completion, execution outcome,
attention acknowledgment, Schedule decision, or replacement dispatch. Recovered
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

- `workspace_created`, `workspace_renamed`, `workspace_closed`
- `shell_created`, `shell_renamed`, `shell_closed`
- `launcher_created`, `launcher_renamed`, `launcher_removed`
- `run_started`, `output_changed`, `run_exited`
- `agent_registered`, `agent_state_changed`, `agent_completed`,
  `agent_attention_acknowledged`
- `agent_schedule_created`, `agent_schedule_updated`, `agent_schedule_paused`,
  `agent_schedule_resumed`, `agent_schedule_removed`
- `scheduled_execution_created`, `scheduled_execution_changed`
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

Accepted blocked and completed observations also carry an outstanding attention
item in their Agent snapshot. `agent_attention_acknowledged` contains the full
resulting Agent snapshot after the item is removed. The acknowledgment is
conditional on its raising observation revision, persists before publication,
and does not increment the lifecycle observation revision.

Schedule create, update, pause, resume, and remove events never contain prompt
content. Create, changed update, and changed pause/resume operations publish the
complete prompt-free schedule summary after persistence. Exact update no-ops and
idempotent pause/resume emit no event and do not advance the schedule revision.
Removing a schedule emits only its workspace and schedule identities.

`scheduled_execution_created` publishes every persisted manual or timed claim or
skip decision.
`scheduled_execution_changed` publishes later prompt-free shell/run binding,
active, dispatch-failed, exited, cancelled, interrupted, and Agent-link changes.
Both carry the complete public execution snapshot and its positive durable
revision and never carry the retained
prompt or daemon startup environment. Process outcomes do not imply Agent
lifecycle observations.

An exact runner ShellRun exit reconciles a still-starting or active execution in
the same persisted event batch. A staged host outcome produces `exited`, a staged
host-spawn failure produces `dispatch_failed`, and an EOF without either produces
`interrupted`. `run_exited` precedes the terminal
`scheduled_execution_changed`. The staged host outcome remains authoritative.

Closing a workspace removes all of its schedules and stored prompts as part of
the workspace transaction. The stream publishes the existing single
`workspace_closed` event; it does not publish one `agent_schedule_removed` event
per owned schedule.

Protocol-8 and older event clients do not receive protocol-9 agent snapshots or
agent events. Filtering does not rewrite the journal: their returned cursor
still advances across filtered agent events, preserving the stream's total
publication order. Agent get, register, and report requests require protocol 9;
ensure requires protocol 10.

Protocol-21 and older clients receive workspace snapshots without schedules and
do not receive schedule events. The returned cursor still advances across those
filtered events, preserving the stream's total publication order. Schedule
management requests require protocol 22.

Protocol-26 and older clients do not receive `agent_schedule_updated`, but their
cursor advances across it. Schedule update requests require protocol 27.

Protocol-22 and older clients do not receive scheduled-execution events, but
their cursor advances across them. Execution dispatch, list, exact inspect, and
cancel requests require protocol 23. Protocol-22 snapshots also omit
schedule-owned shells and execution-shell links rather than presenting those
shells as user-owned. Historical schedule-shell ownership used for this filtering
is retained exactly while the bounded event journal still contains a generic
event for that shell; it cannot be evicted independently of that event.

Protocol-23 clients retain manual non-skipped execution records but omit timed
and skipped records and their events. Snapshot scheduler health and schedule
next-occurrence fields are also omitted. Filtering never rewinds the event
cursor, so an old client can reconnect across hidden scheduler activity without
replaying or stalling the stream.

Protocol-24 and older clients ignore the additive execution revision and bounded
list metadata. Their existing record/event filtering remains unchanged. In all
versions the returned cursor advances across hidden execution events.

Protocol-31 and older clients do not receive `node_projection_changed`; their
cursor still advances across the invalidation. A projection cut resumes only
when every remote event through its captured cursor fits within 256 records;
otherwise it returns a fresh reduced baseline and no transition history.

Manual final eligibility, claim creation, shell/run binding, and runner spawn are
serialized against Agent activity mutations. A protocol-23 client therefore
cannot observe a manual `claimed` creation whose only terminal transition is a
protocol-24-only policy skip. If Agent activity wins first, the execution is
created directly as skipped and its event is hidden consistently while the
protocol-23 cursor still advances.

Event IDs provide one total publication order. The daemon transition coordinator
couples durable lifecycle mutation, persistence, and event publication. Events
are published only after their corresponding state is persisted. If persistence
fails, the event batch remains pending; background recovery publishes queued
batches in transition order and exactly once.
Execution notifications are derived at that actual publication boundary, so a
dispatch failure whose first persistence attempt fails is notified once after a
later pending flush, never before durable commit. Qualification compares each
published execution event with the prior committed snapshot, so revisions that
retain a qualifying terminal state and reason, including late Agent linkage, do
not notify again.

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

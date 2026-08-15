# Scheduled Agent Work

> **Status: Current contract.** Schedule management, manual run-now execution,
> and daemon-native timed dispatch are implemented. This document governs the
> scheduled Agent work tracked by [#146](https://github.com/gardnmi/boomux/issues/146).
> Source and compatibility tests become authoritative for exact protocol,
> persistence, and bound values as each implementation slice ships.

## Purpose

Scheduled Agent work lets a user define a recurring prompt, dispatch it through
a supported lifecycle integration, and inspect what happened. It is a bounded
orchestration facility, not a workflow engine and not a new source of Agent
lifecycle authority.

## Identities And Ownership

An **Agent Schedule** is durable and belongs to exactly one workspace. It owns:

- A daemon-assigned identity and workspace-unique name.
- An explicit working directory.
- A supported integration.
- A bounded prompt snapshot and prompt revision.
- A session mode and any exact continuation identity.
- A trigger expression and timezone.
- Enabled state and dispatch policy.

Closing the owning workspace removes its schedules and bounded Scheduled
Execution history along with the workspace's other retained state. Confirmation
must include that scope. A schedule cannot become global or move implicitly when
its workspace closes.

A **Scheduled Execution** is the durable record of one timed occurrence or
explicit run-now decision. It captures the exact schedule revision, prompt
revision, requested or scheduled time, working directory, integration dispatch
mode, decision, reason, and available runtime references. Schedule edits affect
only later executions when an edit surface is added; the current CLI does not
expose schedule editing.

The identities remain separate:

- A skipped or pre-shell failed Scheduled Execution has no shell or shell run.
- An execution that starts the internal runner is associated with exactly one
  ShellRun of its schedule-owned shell, even if later host launch fails.
- A lifecycle integration may register an Agent Instance for that ShellRun.
- Fresh executions project distinct Agent Sessions.
- Continued executions can project under the same exact Agent Session while
  retaining distinct ShellRuns and Agent Instances.
- Removing bounded execution history does not reinterpret or delete canonical
  host session data.

## Schedule-Owned Shell

The first execution that binds its internal runner lazily creates one durable
shell owned by the Agent Schedule. Later runner-bound executions reuse that shell
and create a new ShellRun for each occurrence. This preserves retained PTY state,
attachment, run-scoped environment identity, and lifecycle integration without
accumulating one durable shell per occurrence.

The shell stores one stable internal Boomux schedule-runner argument vector
containing the schedule identity, not the prompt or revisioned host command. For
each run the runner resolves only the exact persisted execution claim, then asks
the integration adapter to construct and execute the host argument vector without
shell interpretation. It cannot select a later pending claim or rerun the latest
prompt implicitly. Claim resolution and runner reports require one private
per-execution capability. That capability is removed from the external host's
environment before spawn.

The ownership is exclusive:

- The shell is openable for an active execution and remains visible through
  schedule and execution inspection.
- It cannot be independently renamed, repurposed, restarted, or closed.
- Ordinary shell open and workspace open or restore never start or restart it.
- A process starts only after the scheduler has persisted the corresponding
  Scheduled Execution claim.
- Removing an inactive schedule removes its schedule-owned shell and retained
  terminal state. Active removal remains rejected until cancellation or exit.
- Workspace closure terminates an active run and removes the schedule and shell
  under the workspace's confirmed lifecycle transaction.

An exited schedule-owned shell can retain the latest bounded terminal state, but
the bounded Scheduled Execution history is the authority for earlier occurrence
outcomes. Reusing the shell does not reuse a ShellRun or Agent Instance. Each
execution snapshots its effective working directory.

## Prompt Revisions And Privacy

Creation snapshots the prompt content into Boomux's user-only durable
state. A prompt-file option reads the file at that mutation boundary; later file
changes do not silently alter the schedule. Every Scheduled Execution retains
the prompt revision it used; prompt editing is not currently exposed.

Prompt content may contain private instructions or data. It is bounded and:

- Omitted from list output, event payloads, notifications, routine diagnostics,
  and error messages.
- Returned only by an explicit exact-schedule inspection surface that documents
  the disclosure.
- Removed with its schedule according to the workspace ownership rule.
- Never copied into audit reasons or Agent lifecycle evidence by Boomux.

The prompt must reach the external host and therefore crosses the host adapter's
trust boundary. An adapter should avoid argv transport when the host provides an
equivalent private input channel, but some hosts can expose the prompt in process
listings, canonical transcripts, retained terminal output, or host-generated
errors. Setup and inspection must disclose the selected adapter's transport.
Boomux never places prompt text in the stable internal runner argv or an
environment variable, and it does not claim to redact host-owned content.

Every manual or timed Scheduled Execution uses the daemon's startup environment
as ephemeral process input. It is validated for child startup but never
persisted, projected, copied into a schedule revision, or replaced by the
run-now client's environment. A graceful replacement request carries the
invoking client's validated full Unix environment as ephemeral replacement
startup input. The replacement uses it for future executions while already
running processes retain their original environments. That payload is not added
to durable state or the handoff manifest.

Environment changes therefore require `boomux daemon restart`. Diagnostics can
explain that requirement and report a missing executable or required variable
without printing environment values. Schedule inspection cannot expose the
daemon environment.

## Creation And Consent

Creation supports explicit paused and enabled states. When neither is selected,
the schedule is paused. Enabling is a separate authorization for future
unattended process and tool activity. Human and JSON responses must make the
resulting state clear, and interactive setup should preview at least the prompt,
workspace, directory, integration, session mode, trigger, and timezone before
enabling.

Pausing prevents future timed dispatch but does not cancel nonterminal work.
Removing a schedule with a nonterminal execution is rejected until the execution
is explicitly cancelled or exits. Workspace closure retains its existing
stronger semantics and terminates owned managed processes after confirmation.
Run-now is an explicit user dispatch and remains available while a schedule is
paused; it does not enable later timed execution.

## Trigger Contract

The canonical trigger is a validated standard five-field cron expression plus an
explicit IANA timezone. Fields are minute, hour, day of month, month, and day of
week. The initial grammar accepts numeric values, `*`, lists, ranges, and steps;
day of week uses `0` for Sunday. When both day of month and day of week are
restricted, either field matching selects the local time. Seconds, names,
nicknames, and implementation-specific operators such as `?`, `L`, `W`, and `#`
are not accepted.

Restriction follows the field's syntax rather than its expanded values. A
star-origin component such as `*` or `*/2` is wildcard-origin for the standard
day-of-month/day-of-week rule. Numeric values, lists, and ranges remain
restricted, including full ranges such as `1-31` and `0-6`; two restricted day
fields use OR. Duplicate list values are harmless. Creation and restored-state
validation reject a trigger with no occurrence in a Gregorian 400-year cycle.

Manual conveniences such as every, daily, weekdays, and weekly compile to the
same canonical expression rather than creating another persisted trigger model.
Clients expose both a friendly rendering and the exact expression.

When a timezone is omitted, creation resolves the current system IANA timezone
and stores it; the meaning does not float with later system-timezone changes. If
an IANA identity cannot be resolved, creation requires an explicit timezone.

Time behavior is deterministic:

- A nonexistent local time during a forward DST transition is skipped.
- A repeated local time during a backward DST transition fires once at its first
  matching instant.
- A wall-clock correction cannot dispatch the same scheduled occurrence twice.
- Run-now creates an independent decision and does not consume or move the next
  timed occurrence.

The daemon-native scheduler runs only while the Boomux daemon and user session
are alive. It does not catch up work missed during downtime. On recovery it
records one bounded, coalesced missed-period decision per affected schedule and
computes the next future occurrence; it does not materialize an unbounded record
for every missed minute.

A delayed live tick records one coalesced `missed` decision for due instants
before the latest due instant, then evaluates that latest instant normally. A
paused schedule advances no frontier and creates no records. Resuming moves its
frontier to the resume time, so paused time is never caught up. A pause that wins
after evaluation sampled an enabled schedule but before its durable decision is
recorded as `paused_race` rather than dispatched.

Every timed decision has an occurrence key containing the schedule ID, trigger
revision, and selected scheduled UTC instant. Each schedule persists an
evaluation frontier independently of bounded execution history. The frontier
advances monotonically before publication and is not pruned with audit records,
so clock rollback and history pruning cannot make an old occurrence eligible.
Trigger editing is not currently exposed. The persisted trigger revision remains
part of occurrence identity so a future edit cannot reinterpret old decisions.

The occurrence key, Scheduled Execution decision, and evaluation-frontier
advance commit as one durable mutation before the corresponding event is
published. A persistence failure changes none of them. This prevents a crash
from either consuming an occurrence without its audit record or recording a
decision while leaving that occurrence eligible for duplicate dispatch.

## Session Modes

Every schedule selects one mode; creation defaults to Fresh when the caller does
not specify one:

- **Fresh** asks the integration to create a new canonical external Agent
  Session for each dispatched execution.
- **Continue** pins one exact existing integration and canonical external
  session identity selected by the user.

Continue never means newest, most recent, directory-local, or process-local. If
the exact session cannot be reacquired, dispatch fails visibly and does not fall
back to fresh. A durable continuation lease keyed by integration and exact
external session ID prevents Scheduled Executions from using that session
concurrently. The occurrence is also skipped when daemon state contains a
current running Agent Instance for that exact key, or when its dispatch lease is
already occupied. One dispatch eligibility lease serializes Agent
register/ensure/report with the final policy decision, claim creation, shell/run
binding, and runner spawn. An Agent mutation that wins the lease first produces
`Skipped` with `active_session`; a dispatch that wins retains the lease through
spawn eligibility. No intermediate visible claim can later become a policy skip,
and rejection creates no shell or run. Boomux never
injects a prompt into known active user or Agent work and never takes over its
terminal controller.

Boomux cannot prove that an exact session is inactive in an unmanaged process
that provides no lifecycle or lease signal. The UI must describe this limit; the
scheduler does not substitute process, catalog, argv, or terminal heuristics.
Host-native session leases are not currently implemented.

An integration must advertise the applicable fresh or continuation dispatch
capability and construct the exact host argument vector. Unsupported integrations
or modes are rejected before a schedule is enabled. Process names, argv,
database recency, terminal output, and catalog order are not session identity.

## Dispatch And Concurrency

Manual run-now and timed decisions use the same atomic policy:

- At most one nonterminal Scheduled Execution per Agent Schedule.
- At most one nonterminal Scheduled Execution per workspace.
- Four nonterminal Scheduled Executions daemon-wide by default.
- A user-configurable positive bounded daemon-wide limit.
- Skip rather than queue when any applicable lease is occupied.
- No automatic retry after skip, dispatch failure, process failure, or daemon
  interruption.
- No automatic timeout.

`[scheduling] max_concurrent = 4` configures the daemon-wide limit. Accepted
values are 1 through 64. Configuration layers normally, is sampled when the
daemon starts, and is not watched. `boomux daemon restart` applies the invoking
client's resolved value to the replacement daemon; `boomux doctor` reports when
the running sampled value differs. Scheduler health and the active/maximum count
are exposed by `boomux daemon status`.

Health is `active` only while the scheduler worker is running and its latest
evaluation and next-occurrence projection succeeded. It is `offline` while
stopped or after worker failure or panic. Evaluation failures retain any pending
deterministic test-tick acknowledgment and retry with an interruptible nonzero
exponential delay bounded at five seconds; one diagnostic is emitted per failure
streak.

An eligible decision is first persisted as **Claimed**; a policy rejection is
persisted directly as **Skipped** and never owns a dispatch claim. One
per-execution dispatch lease serializes claim binding, spawn, cancellation,
workspace closure, and handoff. Cancellation or closure can revoke a claim
before spawn, and a dispatcher must revalidate the claim under that lease before
binding a process. Once the user-facing cancellation or closure returns, that
claim cannot spawn later.

Claimed acquires and occupies the schedule, workspace, daemon-wide, and any
continuation concurrency leases before the eligibility decision commits. Those
leases remain occupied through Starting and Active and release only on a
terminal outcome. Concurrent decisions therefore cannot each pass eligibility
and later exceed a declared limit.

The claim is the idempotency authority across request retry and event reconnect.
Graceful handoff establishes a dispatch barrier, transfers claimed executions
with or without a live ShellRun, and prevents the old daemon from spawning after
ownership transfer. The replacement either continues the one transferred claim
or observes its terminal outcome. Cold recovery marks a nonterminal claim or
active execution interrupted; it clears any process outcome staged before the
runner's EOF commit and never starts a replacement process implicitly.

Because there is no initial automatic timeout, blocked or hung work can remain
active indefinitely and occupy its schedule, workspace, and daemon concurrency
slots. Clients must show that limitation and provide explicit cancellation.
Adding optional timeouts requires a later contract update rather than silently
changing existing schedules.

## Execution Outcomes And Agent Authority

A Scheduled Execution has one of these observable states or outcomes:

- **Skipped**: policy prevented dispatch, with an overlap, active-session,
  workspace-capacity, global-capacity, missed, paused-race, or invalid-target
  reason. A schedule that remains paused produces no timed decisions.
- **Claimed**: dispatch eligibility and idempotency ownership are durable, but a
  ShellRun is not yet bound.
- **Starting**: the internal runner ShellRun is bound, but the exact external
  target has not yet launched successfully.
- **Dispatch failed**: the scheduler claimed the work but could not create its
  internal runner, reacquire its exact target, or launch the external host. It
  retains a ShellRun only when the runner had already started; that run's later
  exit metadata does not replace the execution's Dispatch failed outcome.
- **Active**: one ShellRun was created and remains live.
- **Exited**: the external host launched successfully and its managed primary
  process later exited, retaining its exit reason and code.
- **Cancelled**: an explicit user action revoked a pre-spawn claim or terminated
  the managed process.
- **Interrupted**: cold daemon loss or internal runner exit without a terminal
  report ended ownership without a known ordinary host outcome.

The runner stages a host exit outcome or host-spawn failure while the execution
remains Starting or Active. Exact runner-shell EOF commits the terminal state and
publishes `run_exited` first, so a subsequent run never races a terminal record
against a still-running reusable shell.

These are orchestration and process outcomes. They do not report or infer Agent
`working`, `blocked`, `idle`, `inactive`, or `done`. In particular, exit code zero
does not imply `done`, and quiet output does not imply `idle`.

When a lifecycle integration registers an Agent Instance for the exact ShellRun,
its observation remains authoritative under the existing precedence rules. A
linked `blocked` observation uses existing durable Agent attention. A pre-spawn
failure or skipped execution cannot fabricate an Agent Instance or attention
revision.

## Observation And Notifications

Every Scheduled Execution has a durable revision beginning at 1. Each committed
shell/run binding, runner state or outcome change, cancellation, interruption,
or Agent link advances it. `execution wait <id> --after-revision <revision>`
returns the complete current prompt-free record immediately when newer, returns
the unchanged record after `--wait-ms`, and rejects a future revision with
`revision_ahead`. Terminal process state does not make an equal-revision wait
return early because the exact ShellRun may acquire its canonical Agent link
later. Waiters are not persisted; `daemon_stopping` tells a caller to reconnect
and repeat the same revision after replacement.

Execution-created and execution-changed events carry the complete prompt-free
record and its revision. They remain reconnectable through the ordinary event
cursor; an execution wait does not consume or advance that cursor. Persistence
commits before either event publication or waiter wakeup.

`[notifications] scheduled_dispatch_failed` and `scheduled_interrupted` are
independent opt-in categories and default to false. The first covers terminal
runner-start and host-spawn failure; the second covers only newly committed
`cold_daemon_recovery` interruption. Delivery contains bounded sanitized
workspace and schedule names plus the exact execution ID, never prompt,
environment, evidence, transcript, or tool content. Deduplication is by exact
execution ID, execution revision, and reason. Delivery is bounded, at-most-once,
and fail-open and neither acknowledges nor fabricates Agent attention.

Cold startup persists all newly interrupted records before installing the sink
and enqueueing their notifications. Already-terminal records are not replayed on
later cold or graceful starts.

## User Control And Permissions

The scheduler can create a process and deliver the schedule's initial prompt. It
cannot send later input, answer permission or question prompts, acknowledge
attention, or react automatically to terminal content. The host integration and
its user-configured permissions remain the authority for tools and filesystem or
network access.

Explicit user cancellation and workspace closure take precedence over scheduler
activity and cannot be undone by a later tick. Pausing affects future ticks only.
Opening or attaching to an execution permits ordinary user interaction but does
not silently alter its schedule or remove its concurrency lease.

`boomux daemon stop` marks Claimed, Starting, or Active Scheduled Executions
Cancelled with `daemon_shutdown` before completing shutdown. Confirmed workspace
closure terminates active owned work and removes its schedule. If an irreversible
stop is followed by failed workspace persistence, the retained execution is
reconciled to Interrupted and its shell remains pending rather than restoring a
false Active process. Graceful daemon restart uses the transfer rule instead and
does not cancel work.

Removing a schedule is rejected while any execution is Claimed, Starting, or
Active. Cancellation can move any of those states to Cancelled; cancelling
Claimed work revokes the dispatch claim without fabricating a process outcome.

## Audit And Bounds

Every timed or run-now decision produces a bounded Scheduled Execution record,
including skips and dispatch failures. Offline gaps use the coalesced record
defined above. Records include untrusted-safe reasons and exact identity links,
but no prompt text, environment, transcript content, tool content, or credentials.

Each schedule retains every nonterminal execution plus the newest 100 terminal
execution records. Pruning removes terminal records first by ascending
`requested_at_ms`, then ascending execution ID, across normal terminalization,
cold recovery, and shutdown; nonterminal records are never removed. Protocol-25
global list responses default to 100 records, accept limits from 1 through 1,000,
return `limit` and `truncated`, and order descending by `requested_at_ms`, then
execution ID. The wire request limit is optional; protocol-23 and protocol-24
requests ignore it and remain uncapped, with their visibility filter applied
before listing. Current next occurrences are returned as separate schedule-keyed
projections rather than execution revision state. Projection selection covers
every schedule in the requested global, workspace, or exact-schedule scope,
regardless of execution history or execution-page position. It is sorted by
schedule ID, independently capped at 100, and reports `schedule_limit` and
`schedules_truncated`. Scheduler events share the 8,192-event journal and
256-event page bound; a
cursor advances across version-filtered events and expires under the ordinary
stream/cold-restart rules.
Pruned dispatch keys remain represented by a bounded durable probabilistic set;
a retry that matches it is rejected with `idempotency_expired` rather than
creating duplicate work. A false positive is therefore a stable explicit
rejection, never an accidental second dispatch.
The per-schedule occurrence frontier is not audit history and is never pruned
while the schedule exists.

The current prompt snapshot is retained with its schedule. An older prompt
snapshot is retained only while a retained Scheduled Execution references that
revision; once no retained execution references it, its content is removed.
Pruning history does not change a nonterminal execution, schedule state, Agent
Instance, Agent attention item, or canonical host session. Durable changes remain
subject to persistence-before-event publication and the daemon lock order.

## Deferred Work

The initial contract excludes:

- Workflow graphs or dependencies between schedules.
- Arbitrary event triggers.
- Queued overlap or catch-up execution.
- Automatic retries.
- Automatic timeout.
- Automatic prompt answers or guarded actions.
- Terminal-screen lifecycle heuristics.
- Automatic commits, merges, or external message delivery by Boomux itself.
- Required systemd, Omarchy, or other desktop-specific activation.

Optional user-service activation remains a post-MVP evidence-based decision in
[#153](https://github.com/gardnmi/boomux/issues/153).

# Cold Shell recovery

Protocol 56 advertises `recover_shells`. `RecoverShells` is a local owner request;
it does not add a routed operation or change persisted state schemas.

## Request and response

`RecoverShells` takes `shells`, an ordered list of one to four unique targets,
and optional `environment`. Each target has `shell_id`, `expected_run_id` (the
previous run), and `profile` (terminal dimensions and metadata). IDs, profiles,
environment, uniqueness, and bounds are validated before starting anything.
The environment is ephemeral startup input and is never persisted.

The owner holds its mutation gate while checking that each Shell is pending and
its exact previous run ended as interrupted. Normally completed or explicitly
terminated runs must be started explicitly. Existing `recovery.resume_agents`
policy selects the exact supported conversation and preserves argument vectors.

`RecoveredShells.results` preserves target order and contains either:

- `started`, with the current `shell` snapshot and new run identity;
- `unavailable`, with `shell_id`, typed `code`, and `message`.

Missing Shells, changed runs, ineligible previous runs, and executable spawn
failures are per-target results. They do not prevent healthy targets starting.
Started runs increment the previous generation once. The client attaches using
the returned exact run, without takeover or permission to restart an exited run.

## Commit and failures

The lock order remains mutation gate, persistence lock, event transaction.
Eligible starts use the existing runtime lifecycle with paused PTY readers.
The owner captures and replaces durable state once for all successful targets,
then publishes existing `run_started` events and releases their readers. A batch
with no successful targets performs no state replacement.

Internal staging or commit failure kills staged runtimes, joins readers, and
restores previous pending runs before releasing the mutation gate. No start
or output events are published for these rolled-back starts. As with existing
create-and-start, a child may execute before commit: external child side effects
cannot be rolled back. This is not an exactly-once execution guarantee.

A crash before commit restores the previous durable identities; after commit,
it restores the new identities as interrupted pending runs. A lost response or
post-commit reader failure is ambiguous. Clients must rediscover owner state,
then attach or recover using current identities. They must not fall back to an
unguarded start. Replaying a committed request yields `run_changed`, preventing
another generation from being created.

## Compatibility and validation

Owners reject this request below protocol 56. New clients negotiate support
before mutation; known older owners keep the individual restore path. Existing
responses and event schemas are unchanged for older clients.

Focused checks cover bounds, eligibility, isolated spawn failures, one durable
write, rollback cleanup, exact conversation resumption, old wire behavior,
negotiation, lost acknowledgements, and crashes on both sides of commit:

```console
cargo test --lib cold_recovery --locked -- --test-threads=1
cargo test --test native_backend cold_recovery --locked -- --test-threads=1
cargo test -p boomux-desktop cold_recovery --locked -- --test-threads=1
```

The native crash barriers are debug-only and require the existing explicit test
hooks. Performance measurements and their limitations are recorded in
[Desktop performance](desktop/performance.md#cold-recovery-diagnostic-2026-09-27).

# Separate Agent Schedules From Runtime Identity

Status: Superseded by protocol 47

Agent Schedules and Scheduled Executions were durable orchestration identities;
they did not reinterpret shells, shell runs, Agent Instances, or projected Agent
Sessions. This separation cost an additional persisted model, but preserved the
authority rule that process activity and exit cannot establish Agent lifecycle
state. It also let skipped and failed dispatches remain observable without
fabricating an Agent or a shell for failures before runner binding.

Each schedule lazily owned one reusable managed shell. Dispatched executions
used distinct runs of that shell, preserving normal PTY attachment and
integration identity without creating one durable shell per occurrence. The
shell could not be started by ordinary workspace restore or managed
independently from its owning schedule.

A continuation schedule pinned one exact canonical external session and skipped
rather than injecting while that session was active. A fresh schedule created a
new external session for each execution. Both modes dispatched through an
integration-owned exact argument vector.

Protocol 47 removed Agent Schedules and Scheduled Executions during Boomux's
alpha period. State versions 9 through 13 and coordinator schema 6 are rejected
rather than migrated.

# Separate Agent Schedules From Runtime Identity

Status: Accepted

Agent Schedules and Scheduled Executions are durable orchestration identities;
they do not reinterpret shells, shell runs, Agent Instances, or projected Agent
Sessions. This separation costs an additional persisted model, but preserves the
existing authority rule that process activity and exit cannot establish Agent
lifecycle state. It also lets skipped and failed dispatches remain observable
without fabricating an Agent or a shell for failures that occur before runner
binding.

Each schedule lazily owns one reusable managed shell. Dispatched executions use
distinct runs of that shell, preserving normal PTY attachment and integration
identity without creating one durable shell per occurrence. The shell cannot be
started by ordinary workspace restore or managed independently from its owning
schedule, so opening a workspace never becomes an implicit schedule trigger.
Its stored command is a stable internal schedule runner, not the revisioned host
prompt or host command; each execution claim snapshots the inputs resolved by
that runner.

A continuation schedule pins one exact canonical external session and skips
rather than injecting when a scheduler lease, exact current Agent occurrence, or
integration-native signal shows that session is active. Unmanaged host activity
can remain unknowable and is never replaced by heuristic inference. A fresh
schedule creates a new external session for each execution. Both modes dispatch
only through an integration capability that can construct the required exact
argument vector; Boomux does not guess session identity or answer guarded
prompts.

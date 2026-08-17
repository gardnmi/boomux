# Domain Glossary

> **Status: Canonical terminology.** These product distinctions govern naming
> and semantics even when implementation names are less precise.

## Boomux Node

A durable authority for the Workspaces and runtime identities it owns. A Node is
identified independently from any hostname, SSH destination, network address,
or other route used to reach it. A route changing does not change Node identity,
and a route resolving to a different Node does not transfer ownership.

Every Workspace belongs to exactly one Node. Resource identity across Nodes is
the pair of owning Node identity and the resource's unchanged Node-local
identity. The Node is an outer name-resolution scope: Workspace names are unique
within a Node, while Shell, launcher, Agent Schedule, and other names retain
their existing Workspace or identity-specific scopes. A projection of another
Node can be stale or unavailable, but never becomes authoritative and cannot
establish lifecycle completion or authorize mutation.

## Workspace Launcher

A durable detached argument-vector command associated with a workspace and invoked on every
explicit open or restore of that workspace. Dashboard selection alone does not invoke it. Each
launcher has a durable identity and name, owns its working directory, and belongs to an ordered
workspace collection. Each invocation is ephemeral, has no PTY, and does not create or appear as a
shell or shell run.

## Shell

A durable workspace slot whose process runs are retained and observable across attachments and
daemon lifecycle transitions. Explicitly opening an exited shell starts its stored command as a
new run on the same shell identity. A shell is distinct from a workspace launcher.

## Command

The dashboard presentation of a durable shell whose stored startup argument vector is non-empty.
Its exact command is the run's primary process, so interrupting or exiting that process ends the
run and closes its attached terminal. A command retains shell identity and run history; it is not a
workspace launcher. An active agent presentation takes precedence over command presentation while
retaining the shell's durable name.

## Agent Instance

A durable identity for one external agent session associated with exactly one shell run. Its
observed state records authority, evidence, confidence, and time. A completed agent instance
remains inspectable; subagents and individual tool calls are not separate agent instances unless
they establish independent external sessions. Integrations can reacquire an instance by ensuring
the key of integration, external session ID, shell ID, and run ID; ensure returns existing durable
state rather than treating its supplied observation as a reload update.

External observation authority descends from lifecycle integration to process adapter to terminal
heuristic. Equal authority may advance an observation; exact duplicate and lower-authority reports
are no-ops. Daemon lifecycle authority is reserved for daemon-originated observations and is not an
external integration authority.

Inactive means a resumable external session is not currently active. It remains durable and can
return to idle or working, but does not decorate a dashboard shell. Done is permanent completion.
A foreground process hint is not an Agent Instance and is presented as Untracked, never Idle.
Catalog-only OpenCode history is a client-side projected session with Unknown state and no
fabricated Agent occurrence.

## Agent Session

The canonical external conversation projected into one workspace from Agent Instances or host
history. It can span multiple shell runs and Agent Instances that share an integration and exact
external session identity, but it owns no process, PTY, or lifecycle observation.

## Agent Schedule

A durable workspace-owned definition for recurring prompt-driven Agent work with fixed execution
context, session policy, and trigger policy. Creating a schedule does not create a shell, shell run,
Agent Instance, or Agent Session. Its first dispatch creates one reusable schedule-owned shell for
later execution runs, and a new schedule is paused until explicitly enabled. Definition edits are
allowed only while paused, require the exact current definition revision, and affect only later
Scheduled Executions; an already-active execution does not prevent editing, and active and
historical executions retain the definition revisions they captured. Editing a trigger starts its future
evaluation at the edit time, so neither the old trigger nor paused time is caught up after resume.

## Scheduled Execution

A durable record of one manual or timed Agent Schedule decision, bound to the exact schedule and
prompt revisions evaluated for that decision. A skipped or pre-shell failed dispatch has no shell
run; an execution whose internal runner started retains that run even if the external host failed
to launch. It may acquire an Agent Instance, whose lifecycle remains authoritative independently
of the execution's process outcome.

## Process Adapter

A process-bound observer for an agent instance whose evidence is limited to process events. The
explicit supervisor executes an exact argument vector with inherited standard streams and reports
start and exit as Unknown at ProcessAdapter authority. Process exit is not agent completion, and a
process adapter cannot infer Done, Working, Blocked, or Idle. It does not discover canonical
external session identity; callers must supply the complete integration, external session, shell,
and run key. Reporting failures are fail-open and lifecycle-integration authority wins.

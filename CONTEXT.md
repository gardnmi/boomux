# Domain Glossary

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

## Process Adapter

A process-bound observer for an agent instance whose evidence is limited to process events. The
explicit supervisor executes an exact argument vector with inherited standard streams and reports
start and exit as Unknown at ProcessAdapter authority. Process exit is not agent completion, and a
process adapter cannot infer Done, Working, Blocked, or Idle. It does not discover canonical
external session identity; callers must supply the complete integration, external session, shell,
and run key. Reporting failures are fail-open and lifecycle-integration authority wins.

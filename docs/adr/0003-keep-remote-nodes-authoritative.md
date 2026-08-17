# Keep Remote Nodes Authoritative

Status: Accepted; implementation pending

Remote Workspaces and their Shells, ShellRuns, Agent Instances, Schedules,
Scheduled Executions, PTYs, and processes remain authoritative on one remote
Boomux Node. Agent Sessions remain projections whose host lookup and underlying
Agent occurrences are Node-local. A local Node may reach remote authority through
an SSH stdio bridge and retain a bounded prompt-free projection for native local
presentation, but it does not import remote resources into its durable registry
or infer state while disconnected. This costs a separate identity, routing,
synchronization, and cache model, but avoids split process ownership,
cross-machine lifecycle inference, and distributed consensus for ordinary
operations.

The rejected alternatives are direct local ownership of remote processes and a
shared central registry. Direct ownership cannot prove process or cancellation
state after transport loss; shared ownership would require consensus before
Boomux could preserve its existing persistence, event, and lifecycle guarantees.
Each resource therefore has one Node authority, while SSH destinations remain
replaceable routes pinned to that stable identity.

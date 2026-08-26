# Coordinate Multi-Node Workspaces

Status: Accepted; implemented

A Workspace is the coordinator-owned place where a user organizes Shells, Agent
Instances, and launchers across one or more Nodes. The
coordinator owns Workspace identity, name, membership, and operation progress,
but is not an execution placement and does not imply a default Node. Every
runtime resource remains authoritative on exactly one explicitly selected Node.

Each Workspace placement retains host-local settings, including its default
working directory. Paths are never assumed to match across Nodes. The first
resource created on a Node establishes that placement. When only one eligible
Node exists, creation can use it directly; when several exist, no Node is
preselected. Existing Node-local Workspaces remain visible but must be adopted
or linked explicitly, and equal names never merge membership.

Opening a Workspace fans out to every available placement and reports per-Node
results without replaying ambiguous starts. Closing retains coordinator metadata
and unresolved membership until guarded removal is confirmed by every owner.
Resource creation durably prepares exact operation and request identity before
owner mutation. Absence during reconciliation is inconclusive, while an observed
exact resource completes placement. A bounded durable success ledger makes
response-loss retry idempotent; concurrent identical handlers share that result,
and concurrent first-placement requests preserve caller identity while using one
canonical owner Workspace. Adoption and linking revalidate the live owner
protocol, runtime capability, and revision rather than trusting cached projection
eligibility. Preparation reserves durable completion capacity before owner
mutation, and project eligibility preflight precedes empty metadata creation. A
durable dispatch phase prevents later absence or route failure from being
misread as proof that an ambiguous owner mutation never happened.
This costs coordinator persistence, placement-aware protocols, and partial
operation tracking, but preserves a task-first user model without distributed
process ownership or consensus between runtime authorities.

The rejected alternatives are Node-owned Workspaces, replicated Workspace
authority, and name-derived grouping. Node-owned Workspaces make machine
placement the user's primary organizing boundary. Replicated authority requires
conflict resolution during partitions. Name-derived grouping can silently join
unrelated resources. Coordinator ownership with Node-owned placements keeps one
metadata authority and one runtime authority for every operation.

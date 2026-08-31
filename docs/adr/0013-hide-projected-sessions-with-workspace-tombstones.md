# Hide Projected Sessions With Workspace Tombstones

Status: Accepted

Agent Sessions are projections assembled from durable Agent Instances and
ephemeral host catalogs. Removing provider history is outside Boomux authority,
while deleting an Agent or Shell would conflate lifecycle ownership with a
presentation preference. Boomux therefore stores a hidden-Session tombstone in
the authoritative owning Workspace.

The durable key is the Workspace plus integration and exact external Session ID.
When no external ID exists, the exact Agent ID is the fallback. A tombstone
survives host-catalog absence, later Agent occurrences, Workspace rename, and
daemon restart, and Workspace closure removes it. Different Workspaces can hide
the same external conversation independently. Hiding never changes harness
history, Agent Instances, Shells, processes, lifecycle observations, or Session
display-name metadata.

Protocol-51 list filtering applies after complete owner projection and before the
1,000-row response bound. Exact inspect, resolve, open, and resume treat a hidden
Session as not found. Protocol-50 callers retain the prior projection and resume
behavior; old peers also advance event cursors across filtered hide events. This
version-dependent behavior permits an additive rolling upgrade without silently
changing an older client's visible catalog.

Mutation requires the exact projected Session, owning Workspace, current
Workspace revision, and canonical operation UUID. The owner retains at most
1,024 tombstones and 256 replay receipts per Workspace. Exact replay returns the
original minimal result. A fresh request for an existing tombstone records its
receipt and returns `changed: false` without incrementing the Workspace revision
or publishing an event. Persistence precedes event publication, and remote writes
require a live identity-verified protocol-51 route. State schema 17 explicitly
migrates schema 16 with empty tombstone and replay lists.

The rejected alternatives are deleting provider history, deleting durable Agents
or Shells, persisting projected UUIDs, coordinator-local filtering, evicting old
tombstones, and queueing offline writes. They respectively exceed Boomux
authority, conflate domain entities, bind storage to projection mechanics, split
owner authority, unexpectedly resurface hidden history, or apply a stale intent.

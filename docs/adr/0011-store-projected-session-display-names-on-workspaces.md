# Store Projected Session Display Names On Workspaces

Status: Accepted

Agent Sessions remain projections assembled from durable Agent Instances and
ephemeral host catalogs. Making the projected Session UUID a durable entity would
give presentation an independent lifecycle authority and would couple storage to
one projection encoding. Boomux instead stores only an explicit user display
name inside the authoritative owning Workspace.

The durable key is the Workspace plus integration and exact external Session ID.
When no external ID exists, the exact Agent ID is the fallback. This lets a new
Agent occurrence for the same external conversation inherit the Workspace-local
name, lets different Workspaces name the same conversation independently, and
retains metadata while a host catalog is temporarily absent. Workspace closure
removes the record. Harness title and transcript storage are never changed.

The effective projected description is the user name, current harness title,
then latest Agent name or generated fallback. Names are normalized and bounded to
160 characters, with at most 1,024 records per Workspace. Mutations resolve the
exact current projection on the owner, require the owning Workspace revision, and
increment that revision. A durable 256-entry per-Workspace operation replay ledger
makes exact retries safe by retaining the semantic identity, explicit request,
and a minimal accepted result containing only Session ID, Workspace ID, nullable
user name, resulting revision, and `changed`. It must not retain a projected
summary, harness title, catalog data, lifecycle state, or occurrences. Replay
does not depend on the current projection, catalog, or revision. Persistence
precedes event publication, and remote
mutation requires a live identity-verified owner route with protocol-50 support;
stale projections never authorize or queue a write.

The native dashboard exposes rename and reset only when the selected Node
advertises `session_display_names`. It routes the exact Node-qualified Session ID
with the listed Workspace revision, marks explicit overrides in the table, and
refreshes the owner projection after success or conflict.

The rejected alternatives are persisting projected UUIDs, modifying harness
history, coordinator-local aliases for remote Sessions, and offline queued
mutation. They respectively bind storage to projection mechanics, violate harness
ownership, split authority, or apply writes after their revision context is stale.

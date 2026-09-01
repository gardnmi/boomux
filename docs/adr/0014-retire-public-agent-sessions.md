# Retire public Agent Sessions without discarding external identity

## Status

Accepted.

## Context

The projected Agent Session feature combined durable Agent lifecycle records,
provider history catalogs, presentation metadata, remote host services, and
historical process resume behind one user-facing resource. That projection was
not a lifecycle authority, but its breadth made it appear equivalent to an Agent
or Shell and required private host-history discovery to populate ordinary UI.

External harness session identity is still required. Lifecycle integrations use
it to correlate reports, OpenCode shared-runtime claims bind it to exact current
ShellRuns, and cold recovery uses it to reconstruct exact resume arguments.
Removing that identity would weaken Agent authority and recovery guarantees.

## Decision

Boomux no longer advertises or exposes Agent Session list, inspect, rename,
reset-name, hide, open, or resume operations. The native dashboard omits the
Sessions view. Local, routed, host-service, and streaming resume requests using
the retained protocol-51 variants fail with `unsupported_version`.

Protocol variants and state-schema-17 Session presentation fields remain
decodable during a compatibility stage. This keeps existing owner state readable
and lets mixed-version peers receive a typed rejection rather than a malformed
frame or failed daemon start. Those fields grant no current capability and may be
removed by a later explicit protocol-floor and state migration.

`AgentInstanceSnapshot.external_session_id`, integration claims and holders,
working-context observations, and exact cold-recovery inputs remain. They are
opaque lifecycle and recovery data, not a user-facing Boomux resource.

## Consequences

- No Session command, tab, JSON command, or Session feature is advertised.
- Provider history catalogs are no longer reachable through supported APIs.
- Existing Session presentation metadata remains inert until a later migration.
- Agent lifecycle authority and exact external-session recovery remain intact.

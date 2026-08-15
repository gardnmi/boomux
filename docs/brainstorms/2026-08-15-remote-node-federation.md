---
date: 2026-08-15
topic: remote-node-federation
status: accepted-plan
---

# Remote Node Federation

## Goal

Make Agents running on SSH machines feel native on the local host. The local
Boomux TUI and Omarchy plugin must present local and remote Workspaces, Agents,
Schedules, attention, and execution state together. Remote terminals must open
in local native terminal windows, while remote work remains alive when the
local host sleeps, restarts, or loses its SSH connection.

## Product Model

A Boomux Node is a durable authority represented by successive daemon
incarnations and the runtime resources it authoritatively owns.
Each Workspace belongs to exactly one Node. Shells, ShellRuns, Agent Instances,
projected Agent Sessions, Agent Schedules, and Scheduled Executions retain their
existing meanings and scopes under the owning Node.

An SSH target is a route to a Node, not its identity. Every Node has a stable
random ID. Federated resource identity is the structured pair of Node ID and
the resource's unchanged Node-local resource ID. The Node is an outer scope:
Workspace names are Node-local, while other resources preserve their existing
Workspace or exact-identity scopes.

The remote daemon owns remote processes, PTYs, lifecycle state, schedules, and
durable attention. The local daemon owns local resources plus a separate,
read-only projection and routing subsystem for registered remote Nodes. Remote
projections never enter the authoritative local registry.

## Herdr Guidance

Follow Herdr's proven remote workflow:

- Accept an explicit SSH target and use ordinary OpenSSH configuration and
  authentication.
- Detect the remote operating system and architecture.
- Discover compatible binaries in PATH and common direct, Homebrew, mise, and
  Nix locations.
- Prompt before installing or replacing remote software; noninteractive setup
  fails without modifying the host.
- Copy the current binary when platforms match or download and checksum-verify
  the matching release asset.
- Stream installation through a temporary remote file, apply executable
  permissions, and atomically rename it.
- Start or connect to a detached remote daemon.
- Carry the existing client protocol over a fixed SSH stdio bridge to the
  remote Unix socket. Do not expose a TCP listener or forward the local daemon
  socket.
- Let remote work continue when the SSH client disconnects and create a new
  bridge on reattachment.

Boomux intentionally extends Herdr in order to support a combined local UI:
registered Nodes are consistency-pinned inside authenticated SSH routes,
synchronized in the background, and
represented by a bounded prompt-free offline cache.

## User Workflows

Ad hoc remote access follows Herdr and does not create a persistent
registration:

```console
boomux --remote workbox
```

Persistent federation requires explicit consent:

```console
boomux node add workbox
boomux node list
boomux node forget workbox
```

`node add` uses the same interactive bootstrap as `--remote`, then pins the
remote Node ID and authorizes background SSH synchronization. `node forget`
removes only the local registration and cache. It never stops the remote
daemon, terminates remote work, or deletes remote state.

Each registration has a unique local alias that cannot use Node-ID syntax.
Selectors accept either that exact alias or an exact Node ID. Clone recovery
requires an explicit local rekey of the authority chosen to receive a new ID;
registrations never follow an identity change automatically.

Existing unqualified commands continue to target the local Node. Names on a
remote Node require explicit context:

```console
boomux workspace inspect home --node workbox
```

New combined list and dashboard surfaces carry Node IDs and display names;
existing command and JSON result sets remain local-only. Internal selection and
routing always use `(node_id, resource_id)`, never list position or an inferred
name.

## SSH Bootstrap And Transport

The foreground bootstrap may interact with SSH for host-key, password,
passphrase, hardware-token, or MFA prompts. Background synchronization uses
`BatchMode=yes` and never opens a hidden credential prompt.

Boomux writes a private temporary SSH configuration that includes the user's
configuration first and then supplies fallback keepalives. Linux and macOS use
a private per-invocation SSH control socket for connection reuse. Targets that
begin with `-` are rejected, and SSH options belong in `~/.ssh/config` rather
than a Boomux target string.

The remote command uses a fixed template and contains no prompt, resource ID, or
Agent argument. Its only variable command component is the discovered, validated
absolute Boomux executable path, encoded with the documented shell-quoting
function:

```text
ssh -T <target> '<remote-boomux> __federation-stdio'
```

An independently versioned federation handshake returns the stable Node ID,
helper version, and connection mode. The helper obtains that identity from, and
binds it to, the exact daemon socket it will proxy rather than reading and
asserting an independent ID. After the local side verifies the pinned identity,
the helper proxies one existing Boomux daemon-protocol connection byte-for-byte.
Core protocol negotiation remains independent for each Node, so compatible
mixed versions do not require a remote restart.

Use one SSH process per logical daemon connection initially. Multiplexing can
be added later without changing identity or resource semantics.

## Persistence And Privacy

Keep federation data separate from authoritative `state.json`:

```text
node.json        stable identity of this Node
nodes.json       local registrations and pinned Node IDs
node-cache.json  disposable prompt-free remote projections
```

Each file has its own schema version, owner validation, size bounds, atomic
replacement, directory and file permissions, and cold-recovery tests. Cache
corruption must not prevent authoritative local state from loading.

The cache retains only the reduced persisted field allowlist defined by
[`remote-nodes.md`](../remote-nodes.md), plus remote event cursors,
synchronization time, and local notification deduplication state. It does not
serialize existing public summary objects wholesale.

The cache must never retain Schedule prompts, terminal output, attachment
environments, runner capabilities, SSH credentials, host session files, restart
environments, or private transport frames.

## Background Synchronization

The local daemon automatically maintains one event-stream connection per
explicitly registered remote Node. It reconnects with bounded exponential
backoff and jitter, stops aggressive retries after authentication or identity
failure, and reports actionable Node health.

Remote event cursors remain internal per Node. One owner-side synchronization
operation captures the reduced projection, bounded executions, cursor, and
resumable transition records at one event cut. A complete cache replacement is
persisted before the local daemon publishes a `NodeProjectionChanged`
invalidation through its existing local event stream. The public local cursor
therefore remains scalar and orders local observations without claiming a
cross-machine causal order.

On remote cursor expiry or cold restart, only that Node is reseeded from a new
prompt-free baseline. Other Nodes remain usable. Disconnect never marks an
Agent done, interrupts a Scheduled Execution, or starts replacement work.

Cached resources remain visible as stale after local restart. All actions are
disabled until the owning Node reconnects and verifies its identity.

## Request Routing

The local gateway never executes a mutation from cached state and never queues
offline mutations. It verifies the pinned Node before forwarding any inner
request bytes, negotiates the remote core protocol, and preserves exact IDs,
run IDs, revisions, dispatch keys, and typed errors.

Ambiguous writes are not retried after transport loss unless the request carries
an explicit wire idempotency key, in which case only that exact key can be
reused. A conditional revision alone is not an automatic retry key. The client
reports an unknown outcome and requires an authoritative refresh when no durable
postcondition proves the exact intent committed.

Full management parity includes Workspaces, Shells, Agents, launchers,
attention, Schedules, Scheduled Executions, project discovery, integration
management, and exact Agent Session resume. Filesystem validation, host
catalogs, executable discovery, launchers, and Agent commands execute on the
owning remote Node, never on the local machine.

Destructive remote UI actions require fresh authoritative reads and conditional
revision, run, or membership guards where the current local operation lacks
them. Confirmation text names the owning Node.

## Native Terminal Attachment

Remote terminals open through local `xdg-terminal-exec` and retain the local
terminal emulator, theme, keyboard, clipboard, title, and desktop behavior.
The local attachment client opens a verified Node channel, relays attachment
frames over SSH, and leaves PTY authority on the remote daemon.

Local attachment environments are not forwarded to remote processes. Starting a
pending or exited remote Shell requires an owner-environment attachment
capability: the request omits arbitrary Unix environment and the remote daemon
constructs it locally before applying validated terminal-profile input. Takeover
and exact-run attachment remain authoritative on the remote daemon.

Remote daemon handoff relays its normal reconnect behavior. Local daemon
handoff asks local clients to reconnect and creates fresh SSH bridges; no remote
PTY descriptor crosses machines.

## Schedules

Agent Schedules belong to and are evaluated by one Node. A remote Schedule
continues while the local host is offline. If the owning remote machine or
daemon is offline, its existing missed-occurrence policy applies.

There is no automatic failover in the initial implementation. Another Node
never substitutes its filesystem, environment, credentials, integration, or
Agent Session. Future failover may be introduced only as an explicit placement
policy with its own portability and authority contract.

Scheduler health and concurrency are displayed per Node. Concurrency is not a
federated global lease.

## Attention And Notifications

The remote Node owns durable attention. Registered local presentation Nodes
independently decide how to notify without acknowledging or modifying remote
attention.

Live synchronized transitions produce ordinary individual local desktop and
sound notifications. After reconnect, outstanding attention updates the TUI
and Omarchy indicator immediately, while one bounded digest summarizes events
that accumulated offline. Historical notifications are not replayed
individually.

Notification deduplication includes Node ID, Agent ID, observation revision,
and reason. Multiple presentation machines may independently notify for the
same remote Node.

## TUI And Omarchy

The built-in dashboard combines local and remote resources, displays Node
badges, supports Node filtering, preserves selection by structured identity,
shows online/reconnecting/stale/offline/authentication/identity states, and
disables unavailable actions. `boomux --remote` focuses the selected remote
Node without hiding the combined overview.

The Omarchy plugin continues invoking only the local Boomux CLI. It never owns
SSH transport or credentials. Add Node opens the guided setup in a local native
terminal. Every row and action carries an exact Node ID, stale rows remain
visible but inert, and remote path/project browsing uses owner-side APIs rather
than QML's local filesystem model.

The plugin may initially retain bounded one-second polling against the local
projection. A later local event wait can reduce latency without adding network
behavior to QML.

## Compatibility And Safety

Add a new core protocol feature for Node management and node-qualified
projection, plus federation protocol version 1 for SSH channel establishment.
Older clients continue to see and mutate only local resources. Existing CLI JSON
methods retain local-only result sets. Static capabilities advertise the local
CLI's new commands, while separately named daemon-backed views report observed
per-Node runtime capabilities.

Required typed failures include unavailable Node, identity changed, ambiguous
Node-scoped target, and unknown mutation outcome.

No SSH or network wait may occur while core mutation, persistence, event,
runtime, or federation registry locks are held. Local daemon stop, restart, or
Node removal never terminates remote work. Remote projections never trigger the
local scheduler or enter the authoritative local registry.

## Delivery Order

1. Define Node semantics, identity, trust, ownership, and failure policy.
2. Add stable Node identity with Herdr-style ad hoc SSH bootstrap and
   daemon-protocol bridging.
3. Add explicit registration, pinning, retargeting, and removal.
4. Add bounded prompt-free background synchronization and offline projection.
5. Add combined Node-aware CLI JSON and TUI presentation.
6. Route safe remote management operations and add stale-action guards.
7. Attach remote PTYs through local native terminals.
8. Route remote host services, project discovery, integration operations, and
   exact Agent Session resume.
9. Project and manage remote Schedules without automatic failover.
10. Deliver remote Agent attention through the local notification subscriber.
11. Add federated Node management to the Omarchy plugin.

## GitHub Delivery

- Tracking epic: [#173](https://github.com/gardnmi/boomux/issues/173)
- [#174](https://github.com/gardnmi/boomux/issues/174) defines federated Node semantics and safety policy.
- [#175](https://github.com/gardnmi/boomux/issues/175) adds stable Node identity, explicit rekey, and Herdr-style remote SSH bootstrap.
- [#176](https://github.com/gardnmi/boomux/issues/176) persists Node registrations and pinning.
- [#177](https://github.com/gardnmi/boomux/issues/177) synchronizes bounded remote projections.
- [#178](https://github.com/gardnmi/boomux/issues/178) combines local and remote Nodes in the TUI.
- [#179](https://github.com/gardnmi/boomux/issues/179) routes safe remote management operations.
- [#180](https://github.com/gardnmi/boomux/issues/180) attaches remote PTYs in local native terminals.
- [#181](https://github.com/gardnmi/boomux/issues/181) routes remote host services and exact session resume.
- [#182](https://github.com/gardnmi/boomux/issues/182) manages remote Schedules and executions.
- [#183](https://github.com/gardnmi/boomux/issues/183) presents remote Agent attention locally.
- [#184](https://github.com/gardnmi/boomux/issues/184) tracks the Omarchy plugin implementation from the Boomux repository.

## Validation

Use a hermetic fake-SSH harness plus two isolated native daemons. Cover
bootstrap confirmation and refusal, platform and binary discovery, checksum and
atomic installation, stable identity and mismatch, duplicate names and inner
IDs, online/stale/offline/reconnect transitions, cache bounds and privacy,
cursor expiry, exact mutation routing, ambiguous writes, PTY input/resize/
takeover/reconnect, local and remote graceful handoff, local-stop preservation
of remote PIDs, remote Schedule continuity, absence of failover, notification
deduplication and digest behavior, mixed protocol versions, and Omarchy routing
and stale-state safeguards.

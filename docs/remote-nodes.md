# Remote Node Federation

> **Status: Accepted design contract; implementation pending.** This document
> defines the authority, identity, privacy, compatibility, and failure semantics
> delivered by [#174](https://github.com/gardnmi/boomux/issues/174) under tracking
> epic [#173](https://github.com/gardnmi/boomux/issues/173). Current source and
> compatibility tests remain authoritative for shipped behavior. No remote Node
> command or protocol capability is implemented until its delivery issue lands.

## Purpose

Remote Node federation lets local Boomux clients present and manage resources
whose processes and durable runtime state remain on an SSH machine. A remote
Agent must continue running when the local host sleeps, restarts, or loses its
SSH connection. Local terminal windows, the TUI, desktop notifications, and the
Omarchy panel should nevertheless behave as native local presentation.

Federation is not shared runtime ownership. Each Node remains a complete Boomux
authority, and SSH is a replaceable transport between authorities and clients.

## Identity And Ownership

A **Boomux Node** has one stable random identifier independent from its hostname,
SSH destination, network address, daemon process incarnation, and event-stream
UUID. The identifier survives cold daemon restart, graceful handoff, and deletion
of every Workspace.

The Node identifier is a consistency identity, not an authentication credential.
OpenSSH host and account authentication establish the security boundary; Boomux
then pins the identifier returned inside that authenticated route to detect an
unexpected state installation. Copying both SSH trust and private Node state is
outside that guarantee and is equivalent to cloning one authority.

Every Workspace belongs to exactly one Node. Its Shells, ShellRuns, launchers,
Agent Instances, projected Agent Sessions, Agent Schedules, and Scheduled
Executions remain under that Node. Inner resource IDs are not rewritten during
projection. Federated identity is the structured pair `(node_id, resource_id)`.

The Node is an additional outer name-resolution scope, not a replacement for
existing scopes. Workspace names are unique within one Node. Shell, launcher,
and Agent Schedule names retain their Workspace scopes, and exact-only identities
remain exact-only. A client resolving a name outside the local Node must supply
explicit Node context. Existing unqualified operations retain local-Node meaning;
federation never changes a legacy request into a remote mutation.

An SSH target is a route, not identity. Successful persistent registration pins
the Node ID returned through an authenticated route. A later connection that
returns another identity fails as `node_identity_changed`; it cannot replace the
registration, discard the old projection, or redirect an operation. Retargeting
must prove the previously pinned identity before it commits.

Repeating the exact alias, target, and verified Node ID is idempotent and returns
the existing registration unchanged. Reusing only an alias, target, or Node ID
with a different registration value returns `already_exists`; alias changes use
the revision-conditional rename operation rather than another add. A Node cannot
register itself as remote. The coordinator allows concurrent channels only when
they report the same pinned Node and current remote stream.
Simultaneous channels that report one Node ID with divergent stream incarnations
put the registration into an identity-conflict state and close every live read,
mutation, attachment, and synchronization channel. Only the previously committed
projection remains visible as conflicted and stale until explicit operator rekey
or route repair. Boomux does not attempt consensus between cloned authorities and
cannot detect a clone that is never observed concurrently.

Rekey is an explicit local identity-administration operation on the authority the
operator chooses to make distinct. It requires owner access and confirmation of
the old Node ID, closes admission, and performs the same bounded drain used by
registration changes. Drain failure reopens admission, returns `busy`, and leaves
the old identity unchanged. After a successful drain it atomically installs a
new random Node ID. Existing inner resource IDs and authoritative state remain
unchanged, but every owned resource intentionally acquires a new federated
`(node_id, resource_id)` identity. Existing registrations do not follow or
rewrite the change: they fail `node_identity_changed`, discard no evidence
automatically, and require explicit forget and add. Rekey cannot be routed
through a conflicted registration or inferred from clone detection.

A registration also has one bounded, nonempty local alias. Aliases are unique
within the local coordinator, mutable only through a
registration-revision-conditional rename, and are neither Node identity nor SSH
target. Node selectors accept an exact Node ID or one exact local alias; aliases
cannot use the canonical Node-ID syntax, so those selector forms are disjoint. No
fuzzy, prefix, or cross-Node resource-name inference is allowed. Alias collision
returns `already_exists`.

## Connection And Registration

Ad hoc remote access accepts one explicit OpenSSH target and creates no durable
registration. Persistent federation is a separate explicit operation that
authorizes identity pinning, background SSH connections, and bounded local
projection storage. Registration alone is read-only authority: process starts,
destructive changes, integration installation, Schedule enablement, and remote
daemon management retain their existing explicit user authorization. Boomux
never scans SSH configuration and automatically connects to every alias.

Interactive setup can use normal OpenSSH authentication and confirmation. It can
discover or install a compatible remote Boomux binary only after showing the
exact target, source, destination, and process impact. Noninteractive setup never
installs, replaces, or stops remote software. Routine background synchronization
is noninteractive and never opens a hidden password, MFA, hardware-token, or
host-key prompt.

The remote bridge command uses a fixed template. No prompt, resource ID,
integration command, or user argument is interpolated into it. Its only variable
command component is an absolute executable path returned by bounded remote
binary discovery, validated against the selected installation, and encoded with
one documented shell-quoting function. SSH options belong to the user's SSH
configuration, the target is passed as one validated argument, and a target
beginning with `-` is invalid. The bridge opens no remote TCP listener and never
exposes the local daemon socket to the remote machine.

Every registration carries a monotonic local registration revision.
Synchronization and routed requests reserve admission and copy that revision
before network I/O, then revalidate it before committing any registration or
cache change. Setup and a candidate retarget probe copy the revision without
joining ordinary request admission, perform network work, and revalidate at their
mutation commit. The registry retains a monotonic tombstone epoch so deleting and
re-adding a registration cannot make an old reservation current. Duplicate
checks are repeated at commit while the Node mutation gate is held.

Forget and retarget use prepare, drain, and commit phases. Prepare closes
admission without changing the revision, then releases the mutation gate.
Already admitted operations retain their reservation and can finish against the
old registration. If the bounded drain cannot reach a completed or explicit
unknown-outcome boundary, the operation reopens admission and returns `busy`
without changing the route, revision, or tombstone epoch. After a successful
drain, commit revalidates the unchanged registration under the gate, advances its
revision or tombstone epoch, and removes the old cache. Retarget then installs the
verified new route with admission closed until a fresh baseline succeeds. It
never reports success while a mutation can still reach the old route.

A small independently versioned federation handshake identifies the remote Node
before any inner request is sent. The helper first connects to the selected
daemon socket and obtains the Node identity from that daemon's required
Node-identity protocol feature; it does not assert a separately read helper ID.
The handshake fails if the helper state root, daemon response, and connected
socket cannot be bound to the same identity. A daemon predating Node identity
cannot participate in federation until it is upgraded.

After verification, the bridge carries one ordinary Boomux daemon protocol
stream over SSH stdin and stdout. Core protocol negotiation, request version
gates, typed errors, response filtering, attachment frames, and remote graceful
handoff retain their existing authority.

## Local Authority And Projection

A local federation coordinator is separate from the authoritative local durable
registry. Remote Workspaces and their children never enter the local registry,
local scheduler, local process manager, or local PTY owner. A successful remote
mutation is not rolled back because local projection persistence fails, and the
local Node does not optimistically mutate its cache from a forwarded response.

Each explicitly registered Node has an independently bounded projection and
remote event cursor. Persisted projection is a dedicated field allowlist rather
than serialization of existing public summary objects:

- Node ID, fixed health code, negotiated capabilities, retry time, and
  synchronization timestamps. Persisted health is limited to bounded enums such
  as `online`, `reconnecting`, `stale`, `unreachable`,
  `authentication_required`, `identity_changed`, `identity_conflict`, and
  `unsupported`; raw SSH or authentication output is not health data.
- Workspace IDs, names, and bounded item/attention counts.
- Shell IDs, Workspace IDs, names, ownership, status, run ID, generation, and
  lifecycle timestamps, but not cwd, argv, foreground process, or terminal data.
- Launcher IDs, Workspace IDs, and names, but not cwd or argv.
- Agent IDs, names, integration, Workspace/Shell/Run links, state, observation
  revision/timestamps, and bounded attention reason/revision, but not evidence,
  external session identity, source cwd, or host catalog data.
- Schedule IDs, Workspace IDs, names, integration, state, canonical trigger,
  timezone, revisions, timestamps, and next occurrence, but not cwd, prompt, or
  continuation identity.
- Bounded execution IDs, Workspace/Schedule IDs, revisions, state, dispatch kind,
  typed reason/outcome, timestamps, and Shell/Run/Agent links, but not cwd,
  session data, prompt, environment, or runner capability.

Live identity-verified reads can return richer fields under their existing
contracts without making them cacheable. “Prompt-free” alone is not a general
privacy guarantee: names, attention, integration metadata, and timestamps remain
sensitive even under this reduced schema, so projection storage is user-only.

The projection never contains terminal output or reconstruction, attachment or
daemon environments, SSH credentials or authentication output, host session
files, private integration transport, or any omitted field above.

The current local alias is read from the registration when constructing a view;
it is not duplicated in disposable projection state. Alias rename therefore has
one persistence owner and becomes visible atomically with the registration
revision.

Projection storage is disposable and separately versioned from authoritative
state. Corruption or capacity exhaustion cannot prevent authoritative local
state from loading or persisting. Node identity, registrations, and cache each
use owner-only directories, bounded schemas, atomic replacement, and explicit
validation.

## Synchronization And Events

The local coordinator maintains at most one synchronization writer and one
noninteractive event connection per registered Node. Ordinary read and mutation
channels never write projection cache. Remote cursors remain independent because
event IDs provide an order only within one daemon stream. There is no cross-Node
event order and timestamps cannot create one.

The owning daemon provides one Node-projection synchronization operation that
captures the reduced persisted field allowlist, bounded latest execution set,
stream UUID, cursor, and bounded transition records at one event-transition cut.
Given a resumable prior cursor, the response contains every transition through
that cut. If they cannot fit or the cursor expired, it explicitly returns a
baseline reseed with no notification-eligible history. Ordinary event long
polling can wake the synchronization writer, but neither event pages nor
independently paged execution lists update the cache themselves.

Each cache commit is one complete atomic generation containing the registration
revision, pinned Node ID, remote stream UUID, cursor, complete projected
snapshot, bounded execution set, health, and synchronization time. A remote
synchronization response is applied entirely or not at all. A reseed obtains the
remote baseline snapshot and cursor from one authoritative cut, then replaces
the whole Node generation. Registration itself requires only verified Node
identity; until the synchronization feature lands and a baseline succeeds, the
registration remains unobserved and unavailable to the data plane. The persisted
cursor never advances independently from every projected record affected through
that position.

Commit is a compare-and-swap over both the copied registration revision and the
expected prior cache generation, including stream UUID and cursor. A stale
response or worker loses that comparison and cannot replace a newer generation;
it must discard its result and reacquire the current baseline.

After that atomic replacement, the coordinator publishes a local projection
invalidation. Local clients can therefore retain one local cursor that orders
observations by the local Node without reinterpreting remote events as locally
authoritative domain events.

Remote cursor expiry or cold restart reseeds only that Node from a fresh
prompt-free baseline. Disconnect marks the projection stale. It does not mark an
Agent done, interrupt an execution, acknowledge attention, run a Schedule,
advance a remote revision, or dispatch replacement work. Cached rows can remain
visible after local restart, but every action is disabled until the owner
reconnects and revalidates identity.

Background connection retries are bounded and use backoff with jitter.
Authentication, identity, and unsupported-version failures stop aggressive
retry and remain actionable Node health rather than generic Agent failure.

The federation lock order is: daemon transition, Node mutation gate, Node
persistence gate, local event-stream transition frontier, then Node
registry/cache state. Paths use only the required suffix. Network I/O and remote
protocol parsing happen before this order with a copied registration revision;
commit revalidates that revision after entering the gates. Core durable registry,
shell lifecycle, runtime, and terminal locks are never acquired by projection
synchronization. Notification qualification and delivery happen only after every
federation and event lock is released.

## Reads, Mutations, And Failure

New explicitly Node-aware read-only views can use persisted remote projections
and must expose their observation time and stale state. Existing commands and
JSON methods retain their local-only result sets; federation does not append
remote rows to a legacy array. Exact private reads, terminal reads, waits, and
every mutation require a live verified channel to the owning Node.

Boomux never queues an offline mutation. Before forwarding, it resolves explicit
Node context, verifies the pinned identity, negotiates the remote core protocol,
and revalidates the exact target where the operation requires it. Exact IDs,
run IDs, revisions, dispatch keys, capability tokens, and typed remote errors are
preserved.

A transport failure after sending a mutation can leave the outcome unknown.
Automatic retry is allowed only for a request with an explicit wire idempotency
key, using the exact same key; the initial known case is Schedule run-now. An
exact conditional revision prevents a second conflicting commit but does not by
itself prove whether the first response was lost, so it is not an automatic
retry key.

After ambiguity, Boomux refreshes authoritative state. It can report success only
when a request-specific durable postcondition, revision, or idempotency record
proves that exact intent committed. Otherwise it returns `outcome_unknown` and
does not replay. Before a mutation family is exposed remotely, its protocol
tests must classify its key, precondition, ambiguity refresh, and retry behavior;
an unclassified mutation remains unavailable through federation.

Destructive remote UI actions require a fresh authoritative read and an atomic
owner-side precondition that covers the confirmed scope. Existing exact run and
Schedule revisions remain valid guards. Workspace, Shell, launcher, rename, and
other operations without an ABA-safe revision or membership generation remain
disabled remotely until their protocol adds one. Name equality or a repeated
fresh read alone is not a sufficient guard. Confirmation identifies the Node and
the guarded revision or run.

Host-local work executes on the owner. Remote paths are validated remotely;
remote launchers and integration commands run remotely; remote project and Agent
Session catalogs are read remotely. The local Node never treats a remote path as
local or starts a remote definition's executable on the local host.

## PTYs And Native Presentation

Remote PTYs remain owned and read by the remote daemon. A local native terminal
can attach through a verified SSH daemon stream and relay the existing bounded
attachment protocol. Input, resize, focus, takeover, run binding, and terminal
reconstruction retain remote authority.

The local attachment environment is not forwarded to the remote child.
Attachment environments, including credentials and local paths, remain ephemeral
and Node-local.

First attachment to a pending or exited remote Shell requires a distinct
owner-environment attachment capability and a version-gated request mode. That
mode carries no arbitrary Unix environment; the remote daemon constructs the
child environment from its own startup environment, then applies only validated
`TERM`, `COLORTERM`, terminal-program identity, cell/pixel dimensions, and its
authoritative Boomux identity values. The owner enforces this mode rather than
trusting the bridge to strip fields. Without the capability, a running remote
Shell can remain attachable where otherwise compatible, but federation refuses
to start or restart it and never sends the local attachment environment.

Remote graceful handoff sends its existing reconnect boundary through the
bridge. Local graceful handoff asks the local attachment to reconnect and opens
a fresh SSH stream after finalization. No remote PTY descriptor, process handle,
or runtime state crosses the local handoff manifest. Rollback returns routing to
the old local daemon without changing remote ownership.

Local handoff uses a federation admission counter separate from the lock order.
It marks admission closed while briefly holding the daemon transition gate, then
releases that gate before joining workers or waiting for admitted operations.
Operations that reserved admission earlier can still acquire the documented
federation lock order and finish a cache commit or reach an explicit
unknown-outcome boundary. The handoff proceeds only after the reservation count
reaches zero; a bounded drain failure reopens admission and aborts replacement.

Prepared replacement code does not start Node workers or deliver projected
notifications before `FINALIZE`; rollback reopens old-daemon admission and
workers. No SSH child, control socket, remote cursor in flight, or notification
queue is transferred as live ownership.

## Schedules And Failover

An Agent Schedule belongs to its Workspace's Node and is evaluated only by that
Node. Its filesystem, environment, integration, session catalog, scheduler
health, occurrence frontier, and concurrency lease are Node-local. Losing the
local presentation connection does not affect remote evaluation or active work.

There is no automatic cross-Node failover in the initial design. A local network
partition establishes only stale or unreachable presentation; it cannot assert
that the owning daemon was offline or synthesize a missed decision. If that
daemon actually stops, it alone applies the documented missed-occurrence policy
when it recovers and projects the resulting durable decision. Another Node cannot
substitute its context or continue an external Agent Session implicitly. Future
failover requires a separate explicit placement and portability contract; the
initial model neither promises nor emulates it.

## Attention And Notifications

The owning Node remains authoritative for durable Agent attention. A local Node
can subscribe to that projected attention for presentation without modifying or
acknowledging it. Multiple presentation Nodes can notify independently.

Live transition records from a synchronization response can produce individual
local desktop and sound notifications under the local subscriber's configured
Agent and Scheduled Execution categories. Reconnection through an unexpired
cursor updates every outstanding attention row but emits at most one bounded
digest per Node. That digest contains fixed counts for blocked/completed Agent
attention and, when their local categories are enabled, Scheduled Execution
dispatch failures and interruptions proven by the resumed records. Historical
notifications and execution failures are not replayed individually.

A baseline reseed after cursor expiry updates the UI but emits no notification or
digest because it cannot prove which transitions occurred and remained unseen.
Before delivery, bounded local deduplication is persisted atomically with the
Node cache. Individual claims include Node ID, event stream, entity ID,
observation or execution revision, category, and reason. A resumed digest claim
also includes its deterministic prior and through cursors plus enabled category
set. Persisting a claim before enqueue preserves at-most-once delivery across a
local crash or handoff at the accepted fail-open cost that a claimed notification
can be lost. Cache pruning and local handoff retain the latest bounded dedup
frontier without turning it into remote attention authority.

Notification failure is fail-open and cannot change remote lifecycle, attention,
Schedule, or execution state.

## Compatibility And Migration

Remote Node behavior requires an advertised protocol feature. Older clients see
and mutate only local resources; unsupported Node fields and projection events
are filtered while their ordinary local cursors retain current semantics. CLI
JSON additions remain capability-gated, node-qualified, and additive within the
stable schema. A client never infers federation support from a version string.

Federation has three independent compatibility boundaries:

- The local CLI and local daemon negotiate the ordinary core protocol. Clients
  predating federation remain local-only.
- The local coordinator and remote helper negotiate the federation handshake
  before an inner request. An absent helper triggers explicit bootstrap; an
  unsupported handshake fails with a typed pre-protocol error and sends no inner
  bytes. Unknown additive handshake fields are ignored only within a negotiated
  compatible federation version.
- The helper and remote daemon negotiate the ordinary core protocol for every
  channel. The daemon must support Node identity before ad hoc access or
  registration, and must additionally support the atomic projection operation
  before synchronization activates. Individual management and attachment
  operations remain gated by their own ordinary protocol features. A
  pre-federation daemon cannot be proxied as a Node merely because it supports an
  unrelated inner operation.

A compatible running remote daemon is not restarted solely because release
versions differ. Handshake round trips, old/new helper pairs, current/minimum
federation-capable remote daemons, old local clients, response filtering, and
cursor advancement all require mixed-version tests before capability
advertisement.

Node identity, registrations, and projections use explicit independent schemas.
Introducing them cannot silently reinterpret authoritative `state.json`. If a
later implementation places any Node field in that durable representation, it
must bump `STATE_VERSION` and provide the ordinary explicit migration and cold
recovery evidence.

Every durable federation schema retains its immediately previous representation
and migrates it explicitly under the appropriate owner lock before advertising
the new capability. Migration is atomic and covered by valid, invalid,
cold-recovery, and graceful-handoff tests. An unsupported future schema is
preserved unchanged and disables federation rather than being reinterpreted or
downgraded.

The first federation-capable startup creates `node.json` once under a dedicated
creation lock using atomic no-replace installation, file and directory fsync,
and owner-only permissions. A missing identity can be recreated only when no
valid identity file exists; external registrations then observe a new ID and
fail closed until explicitly repaired. A malformed or unsupported existing
identity disables federation without blocking authoritative local daemon state
and is never silently replaced.

A malformed or unsupported registration file similarly disables remote routing
while preserving the file for operator recovery. An invalid disposable cache is
quarantined or discarded and reseeded after identity verification. Restoring a
backup that copies Node identity creates a clone subject to the conflict and
rekey rules above. Graceful replacement reads the same identity and registration
stores under their locks; it does not synthesize or transfer a new Node ID.

## Bounds And Non-Goals

Implementations bound registered Nodes, concurrent SSH children, connection
handlers, stderr, frame sizes, projections, execution pages, retry delays, and
shutdown waits. No network wait occurs while core or federation registry,
persistence, event, lifecycle, runtime, or terminal locks are held.

The initial contract excludes:

- Automatic SSH-host discovery or connection.
- Automatic cross-Node Schedule failover.
- Shared resource ownership or distributed consensus.
- Queued offline writes or ambiguous-write replay.
- A global cross-Node scheduler concurrency lease.
- Moving active Shells, Agents, or continuation sessions between Nodes.
- Remote TCP control listeners or forwarding the local daemon socket.
- Treating a local daemon stop, restart, or Node removal as remote process
  authority.

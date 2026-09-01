# Remote Node Federation

> **Status: Current contract.** This document defines the implemented authority,
> identity, privacy, compatibility, and failure semantics delivered incrementally
> under [#174](https://github.com/gardnmi/boomux/issues/174) and tracking epic
> [#173](https://github.com/gardnmi/boomux/issues/173). Current source and
> compatibility tests remain authoritative for shipped behavior. Protocol 28
> implements stable local Node identity; protocol 29, handshake version 1, and
> the hidden stdio helper establish the verified same-socket bridge boundary.
> Protocol 30 and local `boomux node rekey` implement bounded expected-ID rekey
> with exact interactive confirmation. Protocol 31 and registration schema 1
> implement explicit identity-pinned registration management. Protocol 32 and
> Node-cache schema 1 implement bounded background reduced projections. Node-cache
> schema 2 adds local remote-notification and reconnect-digest claims without a
> remote protocol change. Public
> Protocol 33 adds the local-daemon combined Node snapshot and federated
> dashboard. Protocol 34 implements typed exact-Node private
> reads and guarded management operations. Protocol 35 implements Node-qualified
> native-terminal PTY attachment and owner-environment remote startup. Protocol
> 36 implements closed typed owner host services and exact Agent Session resume.
> Protocol 38 implements coordinator-owned Workspace metadata, explicit
> Node-owned placements, guarded adoption and linking, and resumable close
> progress. Node-local runtime state remains authoritative on each owner.
> Protocol 39 implements live Node-qualified focused-terminal presentation in
> the combined Node snapshot without extending persisted remote projections.
> Protocol 40 and Node-cache schema 3 implement bounded coordinator-local
> dismissal and restore of stale cached Shell presentation.
> Protocol 41 and Node-cache schema 4 retain the bounded helper package version
> from an authenticated projection connection for Nodes-view presentation.
> Public `boomux --remote TARGET`
> remains ad hoc.
> Protocol 48 adds explicit registered-Node uninstall coordination. The
> interactive client verifies the pinned owner and canonical user install,
> removes current owner integration assets, stops the proven daemon, and removes
> the helper over one authenticated SSH endpoint. Confirmed remote removal
> atomically consumes local maintenance into registration deletion; uncertain
> outcomes retain the route.
> Protocol 49 adds exact routed Workspace placement default-cwd mutation with
> coordinator prepare/attempt/complete recovery. Coordinator Workspace schema 8
> explicitly migrates schema 7 with empty default-cwd operation ledgers.

## Purpose

Remote Node federation lets local Boomux clients present and manage resources
whose processes and durable runtime state remain on an SSH machine. A remote
Agent must continue running when the local host sleeps, restarts, or loses its
SSH connection. Local terminal windows, the TUI, desktop notifications, and the
Omarchy panel should nevertheless behave as native local presentation.

Federation is not shared runtime ownership. Each Node remains a complete Boomux
authority, and SSH is a replaceable transport between authorities and clients.

The coordinating Node separately owns global Workspace identity, name,
membership, and operation progress. A placement is always an explicit pair of
stable Node ID and owner-local Workspace ID. It may retain an owner-local default
working directory, but paths are never copied to or inferred for another Node.
Equal Workspace names on different Nodes remain unrelated unless the user links
the exact owner identity under revision guards.

Placement eligibility requires a current identity-verified projection and the
protocol-38 `global_workspaces` capability. Selection never defaults when more
than one owner is eligible. Dashboard and CLI selectors show unavailable Nodes
disabled with their health reason. Resource coordination durably prepares stable
operation, requested and canonical owner Workspace, and resource UUIDs before
owner mutation, validates the resulting owner metadata, and persists membership
before success. After an ambiguous channel or coordinator write, the next exact
request reads the prepared owner resource and completes coordinator metadata. A
read that finds no resource retains the pending operation because it may have
raced an in-flight owner mutation. Completed success is replayed from the
coordinator's bounded durable outcome ledger even when the request's original
revision is now stale; no prompt,
attachment environment, or shell-interpolated command enters coordinator state.
Preparation reserves enough physical store capacity for a conservative upper
bound of the completed response, evicting oldest outcomes if necessary. A request
that cannot reserve within 1 MiB fails before any owner mutation. Concurrent
distinct placements atomically enlarge all related pending reservations for the
additional future placement before either new owner dispatch proceeds.
Changing a placement default cwd uses the same authority boundary but a distinct
prepared operation. The request fixes the global Workspace ID and revision,
Node ID, owner Workspace ID and fresh revision, and owner-resolved directory.
Before preparing anything, the coordinator live-verifies that a remote owner
advertises protocol 49 and `workspace_placement_default_cwd`. The dispatch
helper repeats that check on the connection used for mutation before durably
marking the owner attempted. Definitive protocol-48 rejection and cold recovery
of a preparation that never crossed that attempted boundary remove the pending
record.
The owner persists first. Coordinator completion accepts only the unchanged
guarded owner revision or its single updated successor with the exact cwd, then
updates the placement mirror and, for an update, the global revision. Exact
replay returns the bounded durable result; a conflicting operation UUID fails.
Definitive owner rejection cancels the exact preparation; timeout, persistence,
and outcome-unknown responses retain it for exact owner readback.
Existing resources are unchanged, and no offline write is queued.

The dashboard has a dedicated Nodes tab rather than a Node filter. Inspection is
read-only; `R` opens interactive reauthentication for a selected
`authentication_required` Node, alias rename and route retarget use registration
revisions, retarget verifies the pinned identity before mutation, and forget
requires confirmation and removes only the local route. Reauthentication uses
the exact stored route and pinned Node identity, requires an already-compatible
helper and a subsequent prompt-free connection, and never installs, upgrades,
retargets, or changes registration state. Success wakes the selected Node's
existing batch observer without starting an overlapping worker or changing
remote authority. Daemon protocol 38 or newer is required for that explicit
observer wake.
Prepared operations are isolated and serialized by operation UUID, so concurrent
identical handlers return one durable result and cancellation cannot consume an
in-flight or completed success. Distinct first-placement requests retain their
requested owner UUIDs while sharing the canonical owner selected by the first
prepared request. The completed ledger retains at most the newest 256 successes
and may retain fewer to preserve the 1 MiB coordinator-store limit; replay is not
guaranteed after oldest-first eviction. Adoption and linking require a fresh
identity-pinned protocol-38 combined local snapshot under the admitted route.
That live snapshot must advertise runtime `global_workspaces` eligibility and
contains the fresh exact owner revision used for commit. Cached projection
eligibility alone cannot authorize either mutation. The retained internal
compound first-placement request performs the same live capability preflight
before persisting empty metadata; definitive
pre-owner rejection cancels only an existing exact preparation durably marked as
never attempted. After preflight, remote creation persists preparation, then
crosses the attempted boundary in a separate replacement immediately before
owner mutation. The compound first-Workspace-and-Shell primitive uses the same
separate boundaries so proven pre-dispatch failure can remove its newly reserved
metadata. A retry of an older never-attempted
preparation crosses the durable attempted boundary before dispatch. Once set,
later capability, registration, identity, owner-error,
or exact-absence results retain the pending operation and Workspace name because
they cannot disprove an earlier transport-ambiguous mutation. Ambiguous or
network outcomes likewise retain any already-prepared recovery state.

An ordinary Shell whose coordinator and owner are the same Node may instead use
the local cross-owner transaction journal. One checksummed synchronized creation
record commits the owner Shell and coordinator completion; an immediately
attached initial run uses a second synchronized record before attachment
success. The two JSON stores are replayable checkpoints. Subsequent ordinary
requests and graceful handoff checkpoint earlier records before changing their
semantics; protocol negotiation, snapshots, event reads, and mutation-free
combined projection reads are the exceptions needed by immediate attachment. This local
optimization does not apply to remote owner mutations, which retain the durable
prepared-operation and exact-readback sequence above.

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
Agent Instances, and projected Agent Sessions remain under that Node. Inner
resource IDs are not rewritten during projection. Federated identity is the
structured pair `(node_id, resource_id)`.

The Node is an additional outer name-resolution scope, not a replacement for
existing scopes. Workspace names are unique within one Node. Shell and launcher
names retain their Workspace scopes, and exact-only identities remain exact-only.
A client resolving a name outside the local Node must supply
explicit Node context. Legacy unqualified resource operations retain local-Node
meaning; protocol-38 coordinated Workspace commands are the explicit exception
and route only through persisted placement membership. Federation never changes
a legacy request into an unqualified remote mutation.

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
destructive changes, integration installation, and remote daemon management
retain their existing explicit user authorization. Boomux
never scans SSH configuration and automatically connects to every alias.

The implemented registration CLI is `boomux node add ALIAS TARGET`, or guided
interactive `boomux node add` from the dashboard command palette or Omarchy
panel. Registration management continues with `node list`,
`node inspect`, human-only `node reauthenticate`, revision-conditional `node
rename` and `node retarget`, and `node forget`. Reauthentication presents normal
OpenSSH authentication in a terminal, verifies the exact stored route against
the pinned identity and an existing compatible helper, then requests a fresh
background observation without mutating either Node. Human-only `node upgrade
NODE` verifies the pinned identity and, after
confirmation, transactionally replaces a compatible registered helper without
changing the registration. Immediately before activation it acquires a bounded
local maintenance lease, drains admitted operations, and prevents rename,
retarget, forget, projection, and routed operations until remote commit or
rollback completes. The lease expires fail-open if the upgrading client dies.
The CLI renews it during a live transaction; local daemon restart and stop are
busy while it remains active so handoff cannot silently reopen admission.
Successful remote commit, failure before remote mutation, and a synchronously
confirmed rollback release it immediately. Only an ambiguous upgrade or a
rollback whose completion cannot be confirmed leaves it closed until bounded
expiry so the remote watchdog settles before local routing resumes.
Human-only `node uninstall NODE` uses the same admission-closing maintenance
lease after explicit process and data-impact confirmation. It requires an
existing protocol-48 helper at the canonical user install destination, proves a
present daemon executes that exact destination, performs Node-ID-conditional
normal daemon stop, and owner-validates the regular single-link executable
against its pre-confirmation fingerprint before removal. Remote state, config, modified
integrations, and the Agent Skill remain. Confirmed removal atomically deletes
the local registration while admission is still closed, then best-effort removes
its now-inaccessible disposable projection. Any failure retains the registration
and releases maintenance; Boomux never interprets `node forget` as remote
uninstall authority.
Add and retarget complete verified bootstrap before submitting a registration
mutation to the local daemon. The selected helper path is
connection-local and is rediscovered on every later connection; it is not a
registration field. The federation handshake's current `ad_hoc` mode remains a
transport property and is not reinterpreted as registration persistence.

Interactive setup can use normal OpenSSH authentication and confirmation. It can
discover or install a compatible remote Boomux binary only after showing the
exact target, source, destination, and process impact. Noninteractive setup never
installs, replaces, or stops remote software. Routine background synchronization
is noninteractive and never opens a hidden password, MFA, hardware-token, or
host-key prompt.
The interactive OpenSSH master's stderr is teed byte-for-byte to the invoking
terminal while the same bounded prefix is retained in memory for failure
classification. This is the only raw authentication-output presentation: it is
not decoded, logged, persisted, included in diagnostics, or exposed to JSON and
background operation. Password and confirmation prompts continue to use
OpenSSH's `/dev/tty` handling. Batch setup captures the same bounded prefix
silently. If master stderr exceeds 16 KiB, setup kills and reaps the private
master process group and reports a fixed bounded transport failure instead of
continuing with truncated authentication context. Guided terminal setup reports
its outcome and waits for Enter before closing.

One setup owns one explicit OpenSSH master from initial authentication through
discovery, candidate checks, authorization, installation, daemon status and
restart, final identity-pinned handshake, and live channel creation. Every
command is a slave of that private owner-only control socket; an observation from
one endpoint or account can never authorize mutation through another connection.
The private configuration terminates any trailing `Match` scope inherited from
the included user configuration before clearing `SendEnv`, so even an included
file ending in a nonmatching block cannot retain environment forwarding.
Every fixed command that contacts the remote daemon resolves its runtime
environment on that authenticated host. It never forwards or persists the local
environment. An existing remote `XDG_RUNTIME_DIR` must be a bounded safe absolute
path. When it is absent on Linux, Boomux derives `/run/user/<numeric id -u>`;
macOS retains the requirement for an explicit runtime directory. The selected
directory must be a non-symlink directory owned by that numeric user with mode
`0700`, and is exported only for that remote command. These rules cover helper
probes, live federation and host-service channels, provisional proof-bound
activation, daemon status and restart, and rollback/watchdog daemon restoration.
Missing, malformed, unsupported, or unsafe runtime discovery returns
`bootstrap_runtime_unavailable` without including the path or raw remote stderr.
Slave argv also installs a deliberately failing direct-connection fallback, so a
missing control socket cannot make OpenSSH resolve or connect to the target
again. Master disappearance at any stage is therefore a transport failure, not a
route retry.
Ordinary host-key checking remains enabled. Completion, refusal, timeout, and
error terminate and reap the master and remove its private control directory.

Discovery checks every bounded absolute executable candidate. A verified helper
whose daemon identity and negotiated protocol are federation-compatible is used
unchanged, even when another candidate is old. No candidates produces a
first-install plan. When every discovered helper has strict published version
metadata below the protocol-47 floor, planning returns typed `upgrade_required`
with the owner-side cold-upgrade sequence before source selection, upload, or any
remote mutation. An inaccessible candidate,
malformed handshake, unsupported newer protocol, conflicting Node identity, or
indeterminate transport/authentication failure is not version evidence and fails
without an install plan. Interactive install or upgrade presents one plan and
asks once. JSON and other noninteractive invocation returns `install_required`
for a missing helper and performs no remote mutation. An uncommitted bootstrap
lock suppresses only its provisional destination. If no alternate compatible
helper is available, discovery returns typed `busy` rather than proposing
another install; registered projection presents that bounded recovery interval
as `reconnecting`, not `unsupported`. A lock whose recorded watchdog is absent
or dead is reported separately as `upgrade_recovery_required` instead of being
misrepresented as active recovery, and registered projection presents it as
`stale` with the recovery-required error detail.
After authorization, Boomux acquires the remote transaction lock and uploads the
pinned bytes only to the private transaction `new` path. It validates and marks
that executable and starts the rollback watchdog without replacing the discovered
destination. The uploaded current binary then runs `daemon status --json` as a
client of the existing daemon; status does not start a missing daemon. This lets
an old released installed helper participate even when its own CLI predates the
additive process-identity fields. On Linux, the provisional client maps the bound
listener to one unique same-user holder through bounded kernel Unix-socket and
`/proc` descriptor views, validates same-user `/proc` ownership, resolves the
bounded absolute `/proc/<pid>/exe` path after normalizing the kernel's
` (deleted)` suffix, and records the socket device and inode. This does not trust
listener `SO_PEERCRED` retained from an earlier handoff process. Automatic upgrade
requires that proven process executable to equal the install destination exactly.
Immediately before rename, the provisional binary opens one negotiated daemon
connection and binds activation to the exact current holder PID, executable,
protocol, and socket device/inode fingerprint at the activation boundary.
Candidate equality or an earlier unbound status result is not evidence. Missing,
changed, malformed, macOS, or otherwise unprovable identity returns
`upgrade_required` without activating the upload.

When discovery finds no helper, a separate fixed runtime probe must prove that
the daemon socket path is absent before upload. Guarded activation repeats that
check while holding the transaction claim. A socket that appears between checks,
symlink, stale socket, malformed result, or failed probe returns
`install_required` for manual daemon recovery without destination replacement or
stop. Only a missing helper plus both absence checks may activate and be treated
as having no pre-existing daemon.

An install source is either the invoking binary for a matching OS/architecture
or a checksum-verified asset from an explicit published release/protocol matrix.
Matching OS/architecture is not ABI proof. Before showing a current-development
binary plan, Boomux opens the source without following symlinks, rejects special,
empty, changing, or oversized files, compares device, inode, length, mtime, and
ctime metadata before and after the bounded read, and pins the retained bytes and
their SHA-256.
Authorization displays that digest, and installation uses those retained bytes
rather than reopening a mutable executable path. A
published asset must overlap both the federation floor and the invoking local
wire range. If none does, setup fails before authorization or remote mutation
and directs development users to build for the remote target and manually stream
that current build.

Authorized upload first acquires one atomic remote bootstrap lock. A locally
generated unique validated transaction ID names its private upload, backup,
captured pre-install daemon protocol or absence, activation marker,
daemon-contact marker, renewable lease, and watchdog state;
no backup name is shared between transactions. Concurrent
setup fails `busy` before touching the destination. The replacement remains
provisional under a bounded remote lease until explicit ID-matched commit.
An existing destination is accepted only when it is a nonempty, bounded,
owner-owned, non-symlink regular executable. Symlinks, special files,
non-executable files, unsupported ownership, and ambiguous metadata fail at the
fixed backup stage before the destination is changed. The installer copies the
old bytes and preserved mode, owner, group, and timestamps into the private
backup, then rechecks source metadata and compares the complete copy. A failed or
out-of-space copy leaves the old destination inode untouched. On Linux the
completed backup and later destination replacement are synced before progress is
published.
After proof, an idempotent activation command acquires the same claim, validates
the transaction and destination, copies and verifies the backup, records
activation intent, and atomically renames `new` over the destination. It never
renames the running old executable into the transaction.
If activation fails after replacement, compensation first moves the provisional
executable back to `new`, restores the prior destination, synchronizes both, and
only then clears activation and backup markers. The transaction is therefore
again uploaded-only and can be retried exactly; incomplete compensation retains
its markers for explicit rollback or watchdog recovery.
Consequently Linux exposes the old daemon's executable as the deleted prior
destination, and graceful restart's installed-path fallback selects the new
destination rather than the protocol-old backup.
Before returning the upload transaction ID, the uploader requires a readiness marker
from the detached rollback watchdog and records its PID. A failure at any fixed
filesystem, streaming, activation, or watchdog stage emits only a bounded
non-secret stage marker and deterministic exit code. The client maps that marker
to actionable `bootstrap_install_failed` detail; arbitrary remote stderr is never
included in CLI diagnostics.
Post-upload failures similarly name the fixed identity-proof, activation, graceful-restart,
helper-verification, live-handshake, or protocol-ping stage without exposing raw
remote output.
Every bounded post-install stage first atomically replaces the lease value. The
watchdog rolls back only after one complete 180-second interval without a new
value. At expiry it acquires the claim and rereads the lease while renewal is
excluded; a changed token releases the claim and begins a fresh interval. Commit,
explicit rollback, renewal, and watchdog expiry coordinate through the
transaction lock and claim directory. Each claim records a unique owner token,
PID, process-start identity, and heartbeat. A contender may reclaim it only after
a complete unrenewed 180-second lease and proof that the recorded PID/start owner
is gone; a zombie is not treated as active, and only the exact recorded owner may
release a claim. Claim metadata becomes ready only after every field is written;
an interrupted publication can itself be reclaimed after the same bounded age.
Before commit, master or local-process
loss therefore lets the watchdog restore it automatically even when a healthy
transaction spans several lease intervals. ABI/exec failure and every daemon-status, restart, helper,
identity, live-handshake, or protocol-ping failure request rollback; a failed
first installation removes only the destination activated by that transaction.
Rollback never stops a daemon when the pre-install state was absent; an
independently started runtime process survives and may require explicit operator
recovery after filesystem restoration. If a provisional upgrade restarted the daemon,
rollback atomically renames the complete backup over the provisional destination.
When a daemon existed before activation and provisional helper work could have
affected it, rollback gracefully restarts through the restored helper regardless
of later status observations. Rollback is confirmed only after that required
restart succeeds. A restart failure retains the retryable transaction and leaves
local maintenance bounded until watchdog recovery can complete. The explicit
rollback and detached watchdog share the same complete-backup and
activation-intent markers, so a partial backup is never installed.
An uploaded-only transaction has no activation intent, so explicit rollback or
watchdog expiry removes only private transaction state and cannot touch the
destination. Upload and activation acknowledgments are keyed by the caller's
transaction ID; exact retry returns the existing uploaded or activated state.

Commit is attempted only after a fresh helper handshake, final live identity
verification, and an actual protocol ping all succeed. It atomically renames the
complete transaction beneath the lock as a durable committed marker and returns
a framed result. It is idempotent while that marker remains and does not delete
the backup, transaction, or lock. The watchdog interprets the moved transaction
as committed and eventually removes all of them; bounded stale-claim recovery
also guarantees eventual lock cleanup after a commit process is killed. A lost
or malformed commit acknowledgment is therefore
`bootstrap_commit_outcome_unknown`, not a rollback promise: the marker may or may
not have committed, and Boomux must not undo a replacement after its verified
live channel was accepted. Retrying the exact bootstrap rediscovers the
compatible installed helper and succeeds without another installation. While an
uncommitted marker is absent, discovery suppresses only the provisional install
destination so a retry cannot accept a helper that the watchdog will later roll
back. Before the atomic marker the watchdog rolls back; after it the watchdog
cleans up, so transport loss at no commit step can produce a partially finalized
state.
Every verified-bootstrap result has passed exactly one live protocol ping. A
previously Ready helper is connected and pinged before the result is returned. An
installed or upgraded helper is already pinged inside the transaction before
commit, so registration, retarget, ad hoc connection, and dashboard consumers do
not ping that returned handoff-era channel again. The remote may close the channel
immediately after the successful verification ping without invalidating the
verified handshake identity or a completed commit. A Ready helper that cannot
answer its one required ping still fails before any registration mutation.
Automatic and ad hoc bootstrap never upload or restart when every discovered
helper is below protocol 47. Because protocol 47 has no migration from protocol
46, the remote owner must use its existing pre-47 binary to run `boomux daemon
stop`, reset the incompatible owner state and removed scheduling configuration,
then install and start protocol 47 as documented in
[`local-update.md`](local-update.md). Explicit `node upgrade` likewise returns
typed `upgrade_required` with that guidance when no compatible helper exists.
When a registered Node already has a compatible helper, explicit `node upgrade`
may transactionally replace it and restarts any present same-protocol daemon so
the replacement actually runs; the registered Node ID is checked before
activation and again inside the rollback boundary before commit. An absent daemon
is not restarted, and a release-version difference never causes automatic
bootstrap to replace a compatible helper.

The remote bridge command uses a fixed template. No prompt, resource ID,
integration command, or user argument is interpolated into it. Its only variable
command component is an absolute executable path returned by bounded remote
binary discovery, validated against the selected installation, and encoded with
one documented shell-quoting function. SSH options belong to the user's SSH
configuration, the target is passed as one validated argument, and a target
beginning with `-` is invalid. The bridge opens no remote TCP listener and never
exposes the local daemon socket to the remote machine.

Framed executable candidates and install destinations that are relative,
oversized, or contain control characters are malformed helper output and return
`bootstrap_malformed_helper`; they are not transport evidence.

Every registration carries a monotonic local registration revision. Routed
requests reserve admission and copy that revision before network I/O, then
revalidate it before returning or committing a mutation. Background projection
synchronization is read-only on the owner: it copies the revision without
reserving mutation admission, and cache commit requires that exact registration
and admission epoch to remain current and open. Maintenance advances that epoch
as it closes the commit gate, so it need not wait for an in-flight SSH projection
read and that stale result is discarded. Setup and a candidate retarget probe
likewise copy the revision without joining ordinary request admission, perform
network work, and revalidate at their mutation commit. The registry retains a
monotonic tombstone epoch so deleting and re-adding a registration cannot make
an old observation current. Duplicate checks are repeated at commit while the
Node mutation gate is held.

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
local process manager or local PTY owner. A successful remote
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
- Shell IDs, Workspace IDs, names, status, run ID, generation, and lifecycle
  timestamps, but not ownership, cwd, argv, foreground process, or terminal data.
  Protocol 40 includes a pending Shell's interrupted run and selected Agent ID
  only when the owner has authoritatively selected one resumable Agent; older
  negotiated projections omit both markers.
- Launcher IDs, Workspace IDs, and names, but not cwd or argv.
- Agent IDs, names, integration, Workspace/Shell/Run links, state, observation
  revision/timestamps, and bounded attention reason/revision, but not evidence,
  external session identity, source cwd, or host catalog data.

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

Node-cache schema 5 is `node-cache.json` beside, but independent from,
`state.json`, `node.json`, and `node_registrations.json`. It is owner-only,
atomically replaced, and capped at 4 MiB and 128 Nodes. Per Node it accepts at
most 1,024 Workspaces, 4,096 Shells, 4,096 launchers, 4,096 Agents, 4,096
dismissed Shell IDs, and 128 capability identifiers of at most 128 bytes.
Names and identifiers in reduced collections are capped at 256 bytes. An invalid
cache is renamed to `node-cache.corrupt-<uuid>.json` when possible and otherwise
discarded in memory; it never blocks authoritative state startup.

Schema 2 explicitly migrates schema 1 by retaining every cached Node generation,
projection, cursor, capability, and health field and initializing empty local
notification frontiers. Each Node retains at most 512 individual claims and 128
digest claims. Individual claims contain only stream UUID, entity ID, positive
observation revision, typed category, and bounded typed reason;
digest claims contain stream UUID, prior and through cursor IDs, and the sorted
enabled category set. Claims are local presentation state and are removed with
their registered Node cache.
Schema 3 explicitly migrates schema 2 by retaining its complete cache and
initializing an empty dismissal set for every Node. Dismissal is accepted only
for a Shell in a stale or offline cached projection. It persists across restart
and reconnect without changing the owner's synchronization generation, and
filters the Shell plus all reduced Agents linked to it from local views. Visible
item and attention counts are recomputed from that filtered view. It never
creates a routed request or owner mutation. Restore clears the selected Node's
set. A later authoritative projection retains tombstones for Shells still
present and prunes tombstones for Shells it no longer contains.
Schema 4 explicitly migrates schema 3 by initializing the optional observed
helper version. Successful authenticated projection commits retain that bounded
ASCII-graphic version in the same generation as health, capabilities, cursor,
and snapshot.
Schema 5 removes Schedule projections and their notification claims. Schemas 3
and 4 are rejected as disposable caches and rebuilt from authoritative Nodes.

## Synchronization And Events

The local coordinator maintains at most one synchronization writer and one
noninteractive event connection per registered Node. Ordinary read and mutation
channels never write projection cache. Remote cursors remain independent because
event IDs provide an order only within one daemon stream. There is no cross-Node
event order and timestamps cannot create one.

The owning daemon provides one Node-projection synchronization operation that
captures the reduced persisted field allowlist, stream UUID, cursor, and bounded
transition records at one event-transition cut.
Given a resumable prior cursor, the response contains every transition through
that cut. If they cannot fit or the cursor expired, it explicitly returns a
baseline reseed with no notification-eligible history. Ordinary event long
polling can wake the synchronization writer, but event pages do not update the
cache themselves.

Each cache commit is one complete atomic generation containing the registration
revision, pinned Node ID, remote stream UUID, cursor, complete projected snapshot,
health, and synchronization time. A remote
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
Agent done, acknowledge attention, advance a remote revision, or dispatch
replacement work. Cached rows can remain
visible after local restart, but every action is disabled until the owner
reconnects and revalidates identity.

Background connection retries are bounded and use backoff with jitter.
Authentication, identity, and unsupported-version failures stop aggressive
retry and remain actionable Node health rather than generic Agent failure.
The implementation uses two-second bounded discovery and handshake operations,
a two-second projection-response deadline, one-second event-wakeup waits,
and exponential retry from one through sixty seconds plus deterministic
sub-second jitter. Authentication, identity, and unsupported states retry no
faster than sixty seconds plus jitter.

The federation lock order is: daemon transition, Node mutation gate, Node
persistence gate, local event-stream transition frontier, then Node
registry/cache state. Paths use only the required suffix. Network I/O and remote
protocol parsing happen before this order with a copied registration revision;
commit revalidates that revision after entering the gates. Core durable registry,
shell lifecycle, runtime, and terminal locks are never acquired by projection
synchronization. Notification qualification and delivery happen only after every
federation and event lock is released.

## Reads, Mutations, And Failure

Agent Session host services and exact Session resume are retired. Current owners
reject the retained legacy list, inspect, resolve, mutation, and resume wire
variants with `unsupported_version`; external session identity remains internal
to Agent lifecycle and recovery. The historical protocol paragraphs below
describe compatibility shapes, not advertised current operations.

Protocol 34 routes one closed `RoutedOperation` union. It is not an arbitrary
daemon-request envelope: creation, daemon stop/restart, Node rekey, integration
mutation, launcher execution, attachment/session resume, and every unlisted
request cannot be represented. The coordinator reserves the exact registration
revision, verifies the pinned identity and protocol capability before sending
the owner request, performs SSH I/O outside locks, and revalidates the
registration before returning. Routed responses never update projection cache.

Protocol 36 adds a separate closed host-service union. It routes bounded project
discovery, directory validation/resolution, Shell-name suggestion, exact stored
Workspace launcher invocation, integration status/preview/install/verify/remove,
and bounded Agent Session list/inspect/resolve. It has no executable-and-arguments
request and cannot run a caller-supplied command. Launchers are selected by exact
Workspace and launcher IDs, then their stored cwd and argv execute directly on
the owner. Integration commits consume an owner-held expiring preview token and
fail if action, force policy, target path, or observed file state changed.
Ordinary host-service responses retain the two-second verified-channel budget.
Session list and inspect allow twenty seconds so the owner's independently
bounded fifteen-second title/catalog adapters can fail open before the channel
deadline; this changes no request shape or execution authority.

Exact remote Agent Session resume is a distinct streaming request. The owner
freshly resolves the opaque projected Session ID, validates owner cwd, builds the
integration descriptor's exact argv, and owns the unmanaged PTY and child. The
presenting Node launches only its native terminal and relays bounded attachment
frames. No ordinary Workspace/Shell row, transcript, prompt, environment,
credential, stderr capture, cache update, or event is created.

Protocol 50 routes Session display-name mutation through the closed operation
union. The dashboard exposes rename and reset only for Nodes advertising
`session_display_names`, sends the exact Node-qualified Session ID and listed
Workspace revision, and refreshes after completion. The owner keeps the immutable
minimal mutation result in a bounded durable receipt, so exact replay does not
rediscover the Session or consult a host catalog. Receipts contain no projected
summary, harness title, catalog data, lifecycle state, or occurrences.
Workspace-filtered reads scope the owner
snapshot before catalog-directory discovery and never enumerate unrelated
Workspace paths.
The same protocol advertises `session_presentation_context`. Session list and
inspect responses may include exact references to owner-authoritative Agent
attention, a launch Git branch inspected from the owner-resolved source cwd, and
bounded repository/branch contexts previously observed and persisted by exact
Agent occurrences. The owner alone canonicalizes structured paths and runs
bounded Git inspection while the reporting Agent's exact ShellRun is active. The
coordinator never runs Git against a remote path, receives a working-context root
path, or turns those references into coordinator-owned lifecycle state. Reduced
`session_context` transitions carry only Workspace and Agent identity so a
presenting Node can invalidate its live Session query. A successful remote
Session open routes each acknowledgment back to that same live owner with the
exact listed observation revision.

Protocol 51 advertises `session_latest_agent_attribution`,
`session_working_context_push_status`, and
`session_working_context_worktree_status`. The owner may include the latest
occurrence's Agent name and separate push and worktree status for a Working
Context whose branch still matches that owner's canonical current worktree
branch. It performs bounded no-fetch response-time inspection of local tracking
refs plus porcelain-v1 staged and unstaged-or-untracked state. The presenting
Node receives only the two worktree booleans, never file names, file counts, file
contents, or behind count; it never runs Git against a remote path, persists
these derived fields, or publishes events for them. Protocol-50 routed list,
inspect, and resolve responses omit both status objects.

The same protocol routes `workspace_session_hiding` through the closed operation
union. The owner resolves the unfiltered exact Session, validates its Workspace
and current revision, and persists a semantic tombstone before publishing the
event. Protocol-51 list filtering happens before response truncation, and exact
inspect, resolve, open, and resume treat the hidden Session as not found.
Protocol-50 callers retain prior visibility and resume behavior. Remote host
service and resume forwarding therefore negotiate the minimum of the requesting
client and owner versions rather than silently applying protocol-51 visibility to
an older caller.

| Operation | Owner guard | Automatic retry | Ambiguity read and exact postcondition |
| --- | --- | --- | --- |
| Workspace/Shell/launcher/Agent inspect | Exact ID on a fresh verified channel | Yes; read-only | Not applicable |
| Rename Workspace/Shell/launcher | Durable resource revision | No | Exact inspect proves requested name and a later revision |
| Close Workspace/Shell; remove launcher | Durable resource revision | No | Exact inspect returns typed `not_found` |
| Restart exited Shell | Durable Shell revision and exact run ID | No | Exact inspect proves pending state, unchanged definition revision, and the confirmed retained run |
| Acknowledge attention | Exact raising observation revision; empty is idempotent | Yes, with the same revision | Returned Agent retains lifecycle revision and has no matching outstanding item |
| Rename/reset Agent Session display name | Exact projected Session ID, Workspace revision, canonical operation UUID | Yes, only with the same UUID and arguments | Bounded immutable owner receipt returns only Session ID, Workspace ID, explicit user name, resulting revision, and `changed` |
| Hide Agent Session from one Workspace | Exact projected Session ID, owning Workspace revision, canonical operation UUID | Yes, only with the same UUID and arguments | Bounded immutable owner receipt returns only Session ID, Workspace ID, resulting revision, and `changed`; inspect then returns typed `not_found` for protocol 51 |

Any unproved ambiguous write returns `outcome_unknown`; conditional revisions
alone never authorize blind replay. Workspace revisions also act as membership
generations and advance when owned Shell, launcher, or Agent membership changes.
Current state schema 17 persists positive Workspace, Shell, and launcher
revisions, bounded Agent working contexts, and bounded Workspace-owned hidden
Session tombstones and replay receipts. Schema 16 migrates explicitly with empty
hide metadata; schema 15 migrates with empty context lists. Historical state schema 13 is rejected at the
protocol-47 alpha break rather than migrated.

New explicitly Node-aware read-only views can use persisted remote projections
and must expose their observation time and stale state. Existing commands and
JSON methods retain their local-only result sets; federation does not append
remote rows to a legacy array. Exact private reads, terminal reads, waits, and
every mutation require a live verified channel to the owning Node.

The implemented surface is `boomux node snapshot [SELECTOR]`. It emits a rich
authoritative local projection and reduced remote projections with structurally
qualified resource identities. The dashboard consumes the same request, keeps
stale rows visible, exposes all-Node and exact-Node filtering, and never passes
remote records to local Git, host-session catalog, path, terminal-preview, or
mutation code. Only tabled protocol-34 actions are enabled for a current,
non-stale Node; local-only execution and presentation paths remain disabled.
After successful ad hoc `--remote TARGET` verification, Boomux focuses the
matching Node in the local dashboard when that Node is already registered and
the local daemon supports the combined view; otherwise the ad hoc connection
retains its existing connection-only result.

Boomux never queues an offline mutation. Before forwarding, it resolves explicit
Node context, verifies the pinned identity, negotiates the remote core protocol,
and revalidates the exact target where the operation requires it. Exact IDs, run
IDs, revisions, capability tokens, and typed remote errors are preserved.

A transport failure after sending a mutation can leave the outcome unknown.
Automatic retry is allowed only for a request with an explicit wire idempotency
key, using the exact same key. An exact conditional revision prevents a second
conflicting commit but does not by itself prove whether the first response was
lost, so it is not an automatic retry key.

After ambiguity, Boomux refreshes authoritative state. It can report success only
when a request-specific durable postcondition, revision, or idempotency record
proves that exact intent committed. Otherwise it returns `outcome_unknown` and
does not replay. Before a mutation family is exposed remotely, its protocol
tests must classify its key, precondition, ambiguity refresh, and retry behavior;
an unclassified mutation remains unavailable through federation.

Destructive remote UI actions require a fresh authoritative read and an atomic
owner-side precondition that covers the confirmed scope. Existing exact run
revisions remain valid guards. Workspace, Shell, launcher, rename, and
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

Protocol 35 advertises `remote_pty_attachment` and
`owner_environment_attachment`. `AttachNode` carries an exact
`QualifiedIdentity`; the inner `Attach.owner_environment` flag defaults false
for old peers and is rejected when an arbitrary `UnixEnvironment` is also
present. Public `open --node SELECTOR` resolves through the local registration
and launches local presentation with a Node-qualified title. The dashboard opens
current remote Shell rows only when the observed owner capabilities include
remote attachment.

Protocol 39 advertises `qualified_focused_terminal`. A focus frame relayed by a
local attachment is still validated and recorded by the owning daemon. After a
successful forward, the presenting daemon also advances one ephemeral
presentation revision for the exact qualified Shell. The combined Node snapshot
returns that value only while its selected Node view contains the Shell. It is
not written to `node-cache.json`, copied into owner events, or used as lifecycle
or mutation authority. This local physical-focus hint does not wait for a
per-frame owner acknowledgment; the owner independently rejects stale controller
frames under its existing attachment rules. Protocol-38 clients receive the
combined snapshot without this additive field. The presenting daemon emits a
payload-free `focused_terminal_presentation_changed` local invalidation so a
dashboard reads the combined snapshot on its next event poll instead of waiting
for the one-second fallback. The event does not become a reduced owner
transition.

Protocol 40 advertises `cached_projection_dismissal`. In the dashboard, `x` on
an online Shell remains owner-authoritative close. On a stale remote Shell it
instead asks for explicit confirmation to dismiss only cached local
presentation and states that the remote process is not closed. The Nodes-tab
`u` action restores dismissed Shells for the selected registered Node.

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

## Attention And Notifications

The owning Node remains authoritative for durable Agent attention. A local Node
can subscribe to that projected attention for presentation without modifying or
acknowledging it. Multiple presentation Nodes can notify independently.

Live transition records from a synchronization response can produce individual
local desktop and sound notifications under the local subscriber's configured
Agent categories. Reconnection through an unexpired
cursor updates every outstanding attention row but emits at most one bounded
digest per Node. That digest contains fixed counts for blocked/completed Agent
attention. Historical notifications are not replayed individually.

A baseline reseed after cursor expiry updates the UI but emits no notification or
digest because it cannot prove which transitions occurred and remained unseen.
Before delivery, bounded local deduplication is persisted atomically with the
Node cache. Individual claims include Node ID, event stream, entity ID,
observation revision, category, and reason. A resumed digest claim
also includes its deterministic prior and through cursors plus enabled category
set. Persisting a claim before enqueue preserves at-most-once delivery across a
local crash or handoff at the accepted fail-open cost that a claimed notification
can be lost. Cache pruning and local handoff retain the latest bounded dedup
frontier without turning it into remote attention authority.

Notification failure is fail-open and cannot change remote lifecycle or attention
state.

Classification consumes only protocol-32 reduced transitions and the projection
from that same owner-side cut. An Agent transition qualifies only when its exact
observation revision is the current blocked or Done row with matching outstanding
attention. Stale revisions, acknowledgments, disconnects, process or output
changes, and unrelated transitions do not qualify. Because this evidence already
exists in protocol 32, remote
notification presentation adds no protocol version or capability.

The coordinator first persists the complete projection and cursor. After that
cache lock is released it classifies the reduced transition batch, atomically
persists previously unseen claims, and releases all federation/cache locks before
enqueueing delivery. A previously online valid-cursor response produces
individual requests. A valid-cursor response after stale or unreachable health
instead replaces those requests with one digest containing bounded category
counts. A baseline from first observation, cursor expiry, or stream replacement
never produces a digest. Local crash and graceful handoff reload the same claim
frontiers; the live notifier queue and worker are not transferred.

## Compatibility And Migration

Remote Node behavior requires an advertised protocol feature. Older clients see
and mutate only local resources; unsupported Node fields and projection events
are filtered while their ordinary local cursors retain current semantics. CLI
JSON additions remain capability-gated, node-qualified, and additive within the
stable schema. A client never infers federation support from a version string.

Protocol-32 and older clients cannot request the combined snapshot and continue
to receive only local resources from every legacy surface. Protocol 33 adds no
new event kind; protocol-31 filtering of `node_projection_changed` remains the
event compatibility boundary. Protocol 39 adds the local-only
`focused_terminal_presentation_changed` invalidation; older clients do not
receive it, but their cursors advance across the filtered event.
Protocol 49 adds owner-routed default-cwd mutation and the owner-local
`workspace_default_cwd_changed` event. Protocol-48 event readers filter it while
advancing their cursor. A protocol-48 owner cannot receive the guarded request.

Federation has three independent compatibility boundaries:

- The local CLI and local daemon negotiate the ordinary core protocol. The
  supported range is protocol 47 through 49; protocol 46 is rejected.
- The local coordinator and remote helper negotiate the federation handshake
  before an inner request. An absent helper triggers explicit bootstrap; an
  unsupported handshake fails with a typed pre-protocol error and sends no inner
  bytes. Unknown additive handshake fields are ignored only within a negotiated
  compatible federation version.
- The helper and remote daemon negotiate the ordinary core protocol for every
  channel. Protocol 47 is the floor and protocol 49 is current, so a protocol-46
  helper or daemon is never admitted as a Node.

A compatible running remote daemon is not restarted solely because release
versions differ. Automatic and ad hoc bootstrap reject an all-incompatible
pre-47 helper set during planning, before upload or mutation.

v0.32/state schema 13 cannot gracefully hand off into the protocol-47 release on
a remote Node. Upgrade that host with the same cold sequence as a local install:
stop its v0.32 daemon (terminating all processes managed by that Node), reset the
incompatible state files while retaining `node.json` and registrations when
desired, remove `[scheduling]` and the scheduled notification/sound keys from the
remote Node's active config layers, then install and start protocol 47. See
[`local-update.md`](local-update.md). Automatic bootstrap cannot migrate
schema-13 state, replace the helper, restart the daemon, or perform an H7-to-H8
graceful handoff across this boundary.

Protocol 47 is now the attachment compatibility floor for every registered
owner. The historical protocol-34 running-Shell and protocol-35 startup
distinction remains part of the feature history but no older owner is negotiated.
Release-version differences never authorize restarting a protocol-compatible
remote daemon.

Node identity, registrations, and projections use explicit independent schemas.
Introducing them cannot silently reinterpret authoritative `state.json`.
Protocol-36 host-service previews and resumed unmanaged PTYs are transient and
require no state or Node-cache schema change. If a
later implementation places any Node field in that durable representation, it
must bump `STATE_VERSION` and provide the ordinary explicit migration and cold
recovery evidence.
Coordinator Workspace schema 8 adds bounded pending and completed placement
default-cwd operation ledgers. Schema 7 migrates explicitly with both ledgers
empty. Owner `STATE_VERSION` and handoff remain unchanged because `default_cwd`
already belongs to the persisted Workspace shape.

Registration schema 1 is stored in owner-only `node_registrations.json` beside,
but independently from, `state.json` and `node.json`. It stores only alias,
target, pinned Node ID, registration revision, and tombstone epoch. Atomic
replacement fsyncs the new file before rename; records, aliases, targets, and
file size are bounded and list order is deterministic by alias then Node ID.

Durable federation schemas either migrate an explicitly supported predecessor
under the appropriate owner lock or reject it when a breaking alpha boundary
removes data that cannot be retained. Migration is atomic and covered by valid,
invalid, cold-recovery, and graceful-handoff tests. Authoritative unsupported
schemas are preserved unchanged and disable federation rather than being
reinterpreted or downgraded; disposable projection caches may instead be
quarantined or discarded and rebuilt.

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
handlers, stderr, frame sizes, projections, retry delays, and
shutdown waits. No network wait occurs while core or federation registry,
persistence, event, lifecycle, runtime, or terminal locks are held.

The initial contract excludes:

- Automatic SSH-host discovery or connection.
- Shared resource ownership or distributed consensus.
- Queued offline writes or ambiguous-write replay.
- Moving active Shells, Agents, or continuation sessions between Nodes.
- Remote TCP control listeners or forwarding the local daemon socket.
- Treating a local daemon stop, restart, or Node removal as remote process
  authority.

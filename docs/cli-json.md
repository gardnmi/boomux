# CLI JSON Contract

> **Status: Current stable contract.** Incompatible output requires a new schema
> value; human-readable output is not a parsing contract.

Boomux exposes a versioned JSON contract for integrations. Human output remains
the default; pass `--json` to a supported command to emit one
JSON document on stdout:

```json
{
  "schema": "boomux.cli/v1",
  "command": "shell.inspect",
  "data": {}
}
```

The `schema` value defines field semantics independently from the daemon wire
protocol. Fields documented by `boomux.cli/v1` will not change type or meaning.
Future incompatible output will use a different schema value.

## Discovery

`boomux capabilities --json` does not start or contact the daemon. It reports
the CLI version, daemon protocol version, supported JSON schemas and commands,
stable error codes, feature names, and the package and validated version for
each bundled integration host under `integration_hosts`. Validated versions are
compatibility test points, not runtime pins or minimum-version guarantees.
Integration ordering and host metadata come from the same capability descriptor
registry used by installation, foreground recognition, recovery, titles, and
session resume.
Protocol-backed feature names are derived from the same typed registry as
request gating and negotiated client feature checks, so each name has one
minimum protocol version authority.
The `desktop_notifications` feature means this binary supports daemon-owned
notification delivery; it does not imply notifications are enabled or that
`notify-send` and a desktop notification service are available. Configuration is
sampled when the daemon starts and is intentionally outside the JSON capability
contract.
The `sound_notifications` feature similarly advertises optional direct sound
delivery through `canberra-gtk-play`. `boomux notification test` is a human-facing
delivery diagnostic and does not support `--json`.

`boomux config path`, `boomux config validate`, and `boomux config edit` are
also human-only local configuration commands. They do not support `--json`, do
not appear in `json_commands`, and do not provide remote configuration mutation.
`config validate` covers the complete global plus optional `BOOMUX_CONFIG`
layered result without starting the daemon.

`boomux setup` is a human-only local discovery and mutation workflow. It requires
an interactive terminal, does not support `--json`, and is absent from
`json_commands`. Automation must compose the advertised integration status,
install, and uninstall commands instead. The static `guided_setup` feature means
the binary contains this workflow; it does not claim that a supported harness,
Omarchy, Hyprland, `hyprctl`, or the companion plugin is currently available.

`boomux desktop toggle`, `desktop show TARGET`, `desktop next`, `desktop previous`, `desktop terminal`,
`desktop close`, `desktop pop`, `desktop return`, and `desktop gather` are also
human-only. They orchestrate the default local Hyprland
presentation and do not appear in `json_commands`, add a wire request, or expose
compositor window identities through `boomux.cli/v1`.
The static `hyprland_special_workspaces`, `contextual_desktop_terminal`,
`coordinated_shell_desktop_placement`, `desktop_workspace_show`, and
`node_reauthentication` features advertise that the installed CLI contains
these human-facing paths; they do not claim that Hyprland is running, that the
adapter is enabled, or that a registered Node currently needs authentication.
The static `local_update_status`, `guided_local_update`, and
`guided_local_uninstall` features advertise the local release-management
surfaces. They do not claim that the current executable is an eligible official
release installation or that a newer release exists.

### Accepted Remote Node Compatibility

Remote Node federation is an incremental extension tracked by #173. Protocol 31
advertises registration management explicitly; clients must not infer support
from a CLI or daemon version string. Registration commands manage local routes
only and do not imply projected reads or routed resource mutation.

Protocol 33 advertises the separately named `node.snapshot` combined read. It
contacts only the local daemon and returns qualified local plus bounded cached
remote projections. Legacy resource lists remain local-only.

Protocol 34 advertises `typed_exact_node_routing` and
`guarded_remote_management`. Dashboard effects carry structured Node-qualified
identities, freshly inspect the owner before destructive confirmation, and use
the resource revision, membership generation, run ID, or observation revision
required by the operation.
Stale Nodes and Nodes without the capability remain non-actionable.

Protocol 35 advertises `remote_pty_attachment` and
`owner_environment_attachment`. `boomux open SHELL --node SELECTOR` is a
human-facing local presentation command: the selector resolves to an exact
registered Node, the title includes its alias, and the hidden attachment carries
the exact qualified identity. It does not add a JSON method or change legacy
unqualified `open` semantics.

Protocol 36 advertises `typed_node_host_services` and separate capabilities for
remote project discovery, launcher invocation, integration management, Agent
Session catalogs, and exact Session resume. Commands with `--node SELECTOR`
resolve one registered Node and require its live verified protocol-36 service;
unsupported owners fail visibly and are never emulated against local PATH,
configuration, catalogs, or filesystems. Responses are live and transient and do
not update the cached Node projection.

`capabilities` advertises only what the installed local CLI can speak and never
reports negotiated state for a registered Node. `node.list` and `node.inspect`
return registration data only. `node.snapshot` carries each Node's observed
protocol, capabilities, health, and observation time after contacting only the
local daemon. Integrations must keep static CLI support, registration data, and
per-Node runtime support distinct.

Legacy resource commands and JSON methods retain local-Node meaning and
local-only result sets. Protocol-38 unqualified Workspace list, inspect, open,
rename, close, and resource creation resolve coordinated Workspaces first; older
clients still receive only local resources. The separately named combined read
exposes Node identity, health, observation time, and stale state, while every
actionable resource is identified structurally by both its owning Node ID and
unchanged resource ID. Routed commands that accept names resolve them only in an
explicit Node context; `workspace open --node`, `shell suggest-name --node`, and
`launcher invoke --node` require exact owner resource IDs.
`workspace open TARGET --node NODE` accepts the local Node only by exact Node ID,
never by its display alias `local`, so an unlinked local owner Workspace cannot
be confused with a same-ID global Workspace. Registered remote Nodes retain
documented exact-ID or alias resolution.

Human-only `workspace select TARGET`, `workspace current`, and `workspace clear`
manage one local selected coordinator Workspace by exact ID on protocol 38 or
newer; Node-local owner Workspace IDs are not selectable. For commands that otherwise
require workspace context, precedence is explicit workspace, managed-shell or
launcher `BOOMUX_WORKSPACE_ID`, then the selected Workspace. Shell and launcher
name resolution continues to derive the exact current Shell's Workspace before
using the broader environment value. The selection allows workspace omission
for shell and launcher creation but never selects a Node. It does not change
unscoped `agent.list`, `attention.list`, or `session.list` result sets. Exact
resource IDs continue to bypass workspace
context. This is a CLI-only behavior and does not change the wire protocol or
the `boomux.cli/v1` envelope.

Cached remote projections can satisfy only documented prompt-free summary reads.
Exact private inspection, terminal output, revision waits, and all mutations
require a live identity-verified connection to the owning Node. Offline writes
are never queued. Transport loss after an ambiguous write without an explicit
wire idempotency key returns a stable unknown-outcome error rather than replaying
the request. Identity change and unavailable-Node failures are also typed errors,
not message-parsed states.

Node registration and SSH bootstrap are separately authorized operations.
`node.add --json` never installs or replaces remote software. When discovery
proves that Boomux is absent it returns `install_required`; when every discovered
candidate is a known published build below the federation floor it returns
`upgrade_required`. Both messages direct the operator to rerun the human command
in an interactive terminal. Indeterminate executable, SSH, authentication,
handshake, identity, and newer-version failures retain their own failure rather
than being reported as either code, and no bootstrap mutation occurs.
After authorization the pinned binary is uploaded to private transaction state,
not the destination, and its current `daemon status --json` client proves the
running daemon executable. Automatic upgrade requires that process executable,
not merely the chosen old helper candidate, to exactly equal the install
destination. Missing or unprovable process identity returns `upgrade_required`
without activation. When no helper is discovered, a runtime probe must prove the
socket path absent before upload and guarded activation repeats the check; an
existing, racing, stale, or unprovable socket returns `install_required` without
destination replacement or stop.
On Linux, upgrade proof includes daemon PID, executable, negotiated protocol, and
socket device/inode. The uploaded current binary revalidates that fingerprint on
one open daemon connection immediately before activation; any race or unsupported
proof boundary returns `upgrade_required` before destination mutation. Bootstrap
platform preflight uses and verifies a fixed absolute command layout, so a
poisoned `PATH` cannot select bootstrap tools; a missing layout is
`bootstrap_unsupported_platform`.
JSON and background bootstrap never mirror raw SSH master stderr. They retain
only its bounded in-memory prefix for classification; overflow terminates the
private master and returns a fixed transport failure.
Bootstrap failures retain stable classes: `bootstrap_authentication_failed`,
`bootstrap_transport_failed`, `bootstrap_malformed_helper`,
`bootstrap_unsupported_platform`, `bootstrap_install_failed`, and
`bootstrap_commit_outcome_unknown`. The last code is retryable and occurs only
when the commit result is lost or malformed after successful installed-helper
handshake and protocol ping; it does not promise rollback, and an exact retry
rediscovers a compatible committed helper. Relative, oversized, or
control-character-containing paths in framed discovery output are
`bootstrap_malformed_helper`, not transport failures. A newer helper
uses `unsupported_version`; changing or conflicting verified identities use
`node_identity_changed` and `node_identity_conflict`. These classes contain only
bounded diagnostics and are not collapsed into `invalid_argument` or `internal`.
Install-stage failures identify a fixed non-secret stage such as stream, backup,
activation, or watchdog readiness. They never include raw remote stderr, paths,
shell startup output, or streamed bytes.
The backup stage also covers rejection of an existing destination that is not an
owner-owned bounded regular executable, metadata or byte-copy mismatch, and
copy/fsync failure. These failures occur before atomic destination activation and
leave the old executable pathname untouched.
Bootstrap returns a connection only after exactly one live protocol ping. Ready
helpers are pinged after connection; installed helpers use the pre-commit ping
and are not pinged again by add, retarget, ad hoc, or dashboard callers. Failure
of the required Ready ping remains a bootstrap failure before registration.
`bootstrap_runtime_unavailable` identifies missing, malformed, unsupported, or
unsafe remote daemon runtime discovery. Linux derives `/run/user/<numeric uid>`
only when the remote environment omits `XDG_RUNTIME_DIR`; no local environment is
forwarded. Post-install status, restart, helper verification, handshake, and ping
failures identify that fixed stage without exposing remote stderr.
An already-active remote bootstrap transaction returns the existing stable
`busy` code before changing the destination.
`boomux node reauthenticate NODE` is human-only and absent from `json_commands`.
It opens the registration's exact stored SSH route for normal interactive
authentication, requires an already-compatible helper with the pinned Node
identity, requires a subsequent prompt-free verification, revalidates unchanged
registration state, and requests a prompt-free projection retry. It requires
daemon protocol 38 or newer and never installs, upgrades, retargets, or mutates
the registration.
`boomux node upgrade NODE` is a human-only operation and rejects `--json` before
SSH discovery or mutation. It requires a currently compatible helper to verify
the registration's pinned Node ID, asks once after showing source, destination,
and process impact, and rechecks the registration before activation. A changed
binary gracefully restarts any present daemon even when its protocol remains
compatible; the transaction verifies the same Node identity again before commit.
A bounded local maintenance lease drains and closes registration admission from
the final revision check through transaction completion, then reopens it on
successful commit or lease expiry. The CLI renews the lease while active, and
local daemon restart or stop returns `busy` until release or expiry.
Successful commit releases it immediately; a failed or ambiguous upgrade keeps
it closed through bounded expiry so remote watchdog rollback cannot race routing.
`boomux node uninstall NODE` is likewise human-only and rejects `--json` before
SSH discovery. Protocol 48 atomically consumes its exact maintenance lease into
registration removal only after the identity-pinned remote executable has been
removed. Failures retain the registration.
Per-Node observed capabilities must distinguish passive combined projection from
process-starting, destructive, integration-management, and exact-attachment
support. The full compatibility and privacy rules are defined
in [`remote-nodes.md`](remote-nodes.md).

`boomux node rekey` is an implemented local identity-administration command. It
requires an interactive terminal and exact current-ID confirmation, and does not
support `--json`; it cannot be routed through federation.

Configuration follows the same local authority boundary. The active writable
layer is `BOOMUX_CONFIG` when set and the global XDG config file otherwise.
The config commands cannot target a registered remote Node.

The following commands support `--json`:

- `boomux capabilities`
- `boomux list`
- `boomux shells`
- `boomux read`
- `boomux events`
- `boomux update status`
- `boomux project list`
- `boomux workspace list`
- `boomux workspace inspect`
- `boomux node add`
- `boomux node list`
- `boomux node inspect`
- `boomux node snapshot [SELECTOR]`
- `boomux node rename`
- `boomux node retarget`
- `boomux node forget`
- `boomux shell suggest-name <workspace-name-or-id>`
- `boomux shell inspect`
- `boomux launcher list`
- `boomux launcher inspect`
- `boomux agent list`
- `boomux agent inspect`
- `boomux agent wait`
- `boomux agent register`
- `boomux agent ensure`
- `boomux agent report`
- `boomux attention list`
- `boomux attention acknowledge`
- `boomux session list`
- `boomux session inspect`
- `boomux integration list`
- `boomux integration status [opencode|pi|claude|codex]`
- `boomux integration install <opencode|pi|claude|codex>`
- `boomux integration install --all`
- `boomux integration uninstall <opencode|pi|claude|codex>`
- `boomux integration uninstall --all`
- `boomux integration verify <opencode|pi|claude|codex>`
- `boomux opencode claim ensure`
- `boomux opencode claim release`
- `boomux opencode claim report`
- `boomux daemon status`

JSON mutations are deliberately narrow. Node registration add, rename, retarget,
and forget; Agent register, ensure, and report; attention acknowledgment; and
integration install and uninstall support the contract. Other mutation commands
retain human output. Passing `--json` to an unsupported command fails with
`invalid_argument` before performing the operation.

`boomux integration setup <opencode|pi|claude|codex>` is intentionally
human-oriented
and does not support `--json`. It composes status inspection, an exact install
preview, confirmation, installation when needed, and restart/verification
guidance. `--yes` skips confirmation for automation; replacing modified content
also requires `--force`.

Command payloads are:

- `capabilities`: CLI/protocol versions, integration host compatibility, plus
  arrays of schemas, commands, features, and error codes.
- `update.status`: `current`, nullable `latest`, `state`, `install_kind`, `path`,
  nullable `target`, nullable `release_url`, and `recommended_action`. `state`
  is one of `update_available`, `current`, `newer_than_latest`, `ineligible`,
  `unsupported_target`, or `check_failed`. `install_kind` is one of
  `github_release`, `package_managed`, `root_owned`, `source_build`,
  `development_build`, `custom`, or `unknown`. `recommended_action` is one of
  `run_update`, `none`, `keep_current`, `use_package_manager`,
  `install_github_release`, or `retry`. Status performs bounded release discovery
  but does not start or contact the daemon. Network and malformed-release
  failures are represented as `check_failed` data so passive integrations can
  remain fail-open.
- `list`: a `shells` array.
- `shells`: workspace identity plus a `shells` array.
- `project.list`: `roots_configured`, a `projects` array, and a `warnings`
  array. Without `--node` this command reads local configuration and the
  filesystem without starting or contacting the daemon. With `--node`, the same
  bounded discovery runs on the verified owner and contacts the local routing
  daemon.
- `workspace.list`: on protocol 38, `workspaces` contains coordinator Workspace
  snapshots and `external_workspaces` contains unlinked qualified owner
  singletons. On older protocols it retains the local summary array of `id`,
  `name`, resource counts, fixed `agent_state_counts`, and `attention_count`.
- `workspace.inspect`: on protocol 38, a global target returns coordinator
  `id`, `revision`, `name`, `closing`, and explicit placements. External or
  older local targets retain the owner Workspace inspection shape.
- `node.list`: an alias-then-Node-ID ordered array of registration objects.
- `node.add`, `node.inspect`, `node.rename`, `node.retarget`, and `node.forget`:
  one registration object with `alias`, exact `target`, pinned `node_id`,
  `revision`, and `tombstone_epoch`. Add and retarget verify the SSH route before
  the local mutation; forget performs no SSH operation.
- `node.snapshot`: a `nodes` array containing the authoritative rich local Node
  and bounded reduced cached remote Nodes. `SELECTOR` is an exact Node ID or
  local alias; omission returns the all-Node overview. The local Node has alias
  `local`. A selector matching both that alias and a registration returns typed
  `ambiguous_target`; exact Node IDs disambiguate. Entries contain `node_id`,
  `alias`, `local`, nullable `route`, nullable `registration_revision`, `health`,
  `current`, `stale`, `observed_at_ms`, nullable
  `observed_protocol_version`, nullable `observed_helper_version`,
  `observed_capabilities`, and nullable
  `workspace_owner_eligible`, nullable `workspace_owner_unavailable_reason`,
  `local_snapshot`, and `remote_projection` payloads. Every resource `id` and
  relationship ID in those payloads is `{ "node_id": "...", "inner_id":
  "..." }`; inner IDs are unchanged and must not be routed without their Node.
  Protocol 38 also adds `workspaces` and `external_workspaces` arrays.
  `workspaces` contains coordinator-owned `id`, `revision`, `name`, `closing`,
  and explicit placements with Node ID, owner Workspace ID, observed owner
  revision, nullable owner-local Workspace name, nullable owner-local `default_cwd`, and `active`, `close_pending`, or
  `unavailable` state. `external_workspaces` contains unlinked qualified owner
  identity, revision, name, nullable owner-local default cwd, and availability.
  Protocol-37 and older responses omit both additive arrays. Protocol 39 adds
  nullable `focused_terminal` with a monotonic presentation `revision` and exact
  qualified `shell`; protocol-38 responses omit it.
- `shell.suggest-name`: exact resolved `workspace_id` plus a nonempty generated
  `name` that does not match any shell name in that workspace at observation
  time. Protocol-38 responses also include exact `node_id` for local and routed
  owners; older local responses omit it.
- `shell.inspect`: one `shell` object.
- `launcher.list`: workspace identity plus a `launchers` array.
- `launcher.inspect`: one `launcher` object.
- `agent.list`: an `agents` array, optionally limited by `--workspace`.
- `agent.inspect`: one `agent` object selected by exact agent ID.
- `agent.wait`: `changed` plus one exact `agent` object after a revision-aware
  conditional read.
- `agent.register`, `agent.ensure`, and `agent.report`: one resulting `agent`
  object. The command field identifies the specific mutation.
- `attention.list`: an `attention` array ordered blocked before completed, then
  newest observation first; `--workspace` limits it to one workspace.
- `attention.acknowledge`: `changed` plus the resulting `agent` object.
- `session.list`: a globally newest-first `sessions` array, optionally limited
  by exact workspace name or ID.
- `session.inspect`: one projected `session` object selected only by exact
  opaque session ID.
- `integration.list`: an `integrations` array containing bundled integration
  names, display names, packages, and validated host versions.
- `integration.status`: an `integrations` array containing independent `host`,
  `asset`, and `runtime` status objects. Status does not start the daemon or
  mutate integration files. It executes each PATH-resolved host's `--version`
  command with bounded output and runtime; missing or unhealthy integrations are
  represented as data and do not make status fail.
- `integration.install`: normally, an `integrations` array containing
  `installed`, `replaced`, or `unchanged` results, target paths, and whether a
  host restart is required. With `--dry-run`, `dry_run` is `true` and each entry
  instead contains `current_state`, the planned `action`, `path`, and
  `restart_required`. Each target is changed atomically; `--all` is not a
  transaction across hosts, but every target is preflighted before the first
  write. Codex merges only exact Boomux handlers into
  `${CODEX_HOME:-$HOME/.codex}/hooks.json`, preserving unrelated fields and
  handlers; modified Boomux handlers require `--force`. Kiro owns only the
  dedicated `${KIRO_HOME:-$HOME/.kiro}/hooks/boomux.json` asset.
- `integration.uninstall`: an `integrations` array containing `removed` or
  `not_installed` results, target paths, and whether a host restart is required.
  Every target is preflighted before the first removal.
- `integration.verify`: `integration`, `verified`, exact `shell_id` and `run_id`,
  plus the nonempty authoritative `agents` array. Failure uses typed
  `not_found`, `ambiguous_target`, `run_changed`, or `timeout` errors.
- `opencode.claim.ensure`: the complete ephemeral `claim` and ensured durable
  `agent`; `opencode.claim.release`: exact `claim_id` and `released`;
  `opencode.claim.report`: the resulting durable `agent`. These are paired-plugin
  operations, not general Agent-control APIs.
- `read`: shell/run identity, observed output revision, and rendered output.
- `events`: stream identity, reconnect cursor, optional baseline snapshot, and a
  bounded event array.
- `daemon.status`: `status`, `protocol_version`, `socket_path`, nullable `pid`,
  `executable`, `socket_device`, and `socket_inode`. On Linux, these identity fields
  are populated only after the bound listener maps to one unique same-user holder
  through bounded kernel Unix-socket and `/proc` descriptor views, followed by
  bounded absolute `/proc/<pid>/exe` validation. This remains current after the
  listener is transferred between daemon processes; retained `SO_PEERCRED` alone
  is not process identity. The executable has a kernel ` (deleted)` suffix
  removed. Other platforms or failed proof return null.

## Project Data

`project.list` discovers the same configured project suggestions used by the
dashboard. Each project contains `name`, its canonical absolute `path`, the
configured-root `group`, and zero-based `group_order`. `roots_configured` is
false when the merged configuration has no project roots, distinguishing that
state from configured roots that currently discover no projects. Recoverable
scan problems and resource-limit notices are returned as human-readable strings
in `warnings`; integrations must not parse warning text.

## Shell Name Suggestions

`boomux shell suggest-name <workspace-name-or-id> --json` returns a lowercase
`adjective-noun` suggestion using the same catalog and collision-exclusion
algorithm as generated shell creation. The suggestion is not reserved. Another
operation can claim it before creation, so a later explicit `shell create
--name <suggestion>` can still fail with `already_exists`; callers must handle
that typed error rather than assume the suggestion grants ownership. If every
generated name is currently in use, suggestion fails with `already_exists`.

## Integration Data

Integration arrays are ordered `opencode`, then `pi`, then `claude`, then
`codex`. List entries contain `name`, `display_name`, `package`, and
`validated_version`.

Protocol 26 adds `protocol_26` and `exact_run_attachment` for the internal
additive expected-run attachment handshake.
Protocol 32 adds `protocol_32`, `node_projection_sync`, and
`bounded_remote_node_projections`. Protocol 33 adds `protocol_33`,
`combined_node_snapshot`, and `node_qualified_dashboard`.
Protocol 34 adds `protocol_34`, `typed_exact_node_routing`, and
`guarded_remote_management`.
Protocol 35 adds `protocol_35`, `remote_pty_attachment`, and
`owner_environment_attachment`.
Protocol 36 adds `protocol_36`, `typed_node_host_services`,
`remote_project_discovery`, `remote_launcher_invocation`,
`remote_integration_management`, `remote_agent_session_catalog`, and
`remote_exact_session_resume`.
Protocol 38 adds `protocol_38`, `global_workspaces`,
`multi_node_workspace_placements`, `guarded_workspace_adoption`, and
`resumable_workspace_close`.
Protocol 39 adds `protocol_39` and `qualified_focused_terminal`.
Protocol 40 adds `protocol_40`, `recovered_agent_presentation`, and
`cached_projection_dismissal`.
Protocol 41 adds `protocol_41`, `observed_node_helper_version`, and
`node_upgrade_coordination`.
Protocol 42 adds `protocol_42` and
`opencode_shared_runtime_claims`.
Protocol 43 adds `protocol_43` and `claude_remote_control_bindings`. The binding
operations are private local protocol requests and add no public CLI JSON field,
snapshot field, event, or remote Node projection field.
Protocol 44 adds `protocol_44` and `collaborative_exact_run_attachment`. The
attachment request and terminal profile response are private local protocol
messages and add no public CLI JSON field, snapshot field, event, or remote Node
projection field.
Protocol 45 adds `protocol_45` and `kiro_exact_launch_holders`. Holder acquire,
hook report, and release are private local protocol messages and add no public
CLI JSON field, snapshot field, event, or remote Node projection field.
Protocol 46 added `protocol_46` and `kiro_stop_idle`; its Kiro hook report wire
shape was unchanged. Protocol 47 advertises `protocol_47` and is both the current
and minimum supported protocol, so the historical protocol-45/46 downgrade path
is no longer negotiated.
Protocol 47 also removes Agent Schedule and Scheduled Execution commands, JSON
payloads, capabilities, snapshot fields, and event types. Historical schedule
request shapes are not part of the protocol-47 wire contract.
Protocol 48 adds `protocol_48` and `node_uninstall_coordination`. The completion
request is local coordinator state and adds no JSON mutation command, remote
projection field, or arbitrary routed operation.

Protocol-38 `workspace create` creates empty coordinator metadata without a
default Node or cwd. First global `shell create` or `launcher create` resolves
`--node` against eligible owners. If exactly one
owner is eligible it may be used without `--node`; zero or multiple eligible
owners return a typed selection error listing disabled health reasons. The
owner-local cwd is resolved on that Node. Exact argv arrays are unchanged by
coordination. `workspace open`, `close`, `rename`,
`list`, and `inspect` resolve global IDs or names before considering external
local records.
`workspace adopt TARGET --node NODE`, `workspace link GLOBAL OWNER --node NODE`,
and `workspace retry GLOBAL` expose guarded adoption, linking, and unresolved
close retry without requiring the TUI. Repeating `workspace close` for a closing
global Workspace also uses the retry operation. Dashboard project suggestions
contribute only the Workspace name; they create the same empty coordinator
metadata as by-name creation. Node and path selection begins with first-resource
creation.
An explicit `--node` on `shell create` or `launcher create` never falls back to
local mutation. It fails with `unsupported_version` when global Workspaces are
unavailable and with `invalid_argument` when the target is not coordinated.
`workspace adopt`, `link`, and `retry` likewise fail with `unsupported_version`
before target resolution on a pre-38 daemon.
Prepared resource requests carry one caller-stable operation UUID. The client
retries the exact request once after a lost connection, timeout, unknown outcome,
or coordinator persistence failure. The coordinator returns the durable prior
success before evaluating a now-stale revision guard. This replay guarantee lasts
while the success remains among at most the newest 256 completed operations and
within the 1 MiB coordinator-store bound; oldest-first eviction ends the
guarantee. Prepared requests reserve their completed-response footprint within
the 1 MiB store and fail before owner mutation when capacity is unavailable.
Adoption and linking fetch a fresh
protocol-38 combined local snapshot over the admitted identity-pinned route and
require its runtime `global_workspaces` capability before using the owner revision;
cached eligibility cannot authorize them.

Node snapshot health is `unobserved`, `online`, `reconnecting`, `stale`,
`unreachable`, `authentication_required`, `identity_changed`,
`identity_conflict`, or `unsupported`. `current` is true only for a live
identity-verified observation. Cached rows remain visible when stale, but cached
state never authorizes terminal reads, private inspection, or mutation.
The dashboard may dismiss a stale cached Shell from local presentation. This
also hides reduced Agent rows linked to that Shell, persists across daemon
restart and reconnect, and does not close or mutate the owner resource. The
Nodes-tab restore action makes retained projections visible again; authoritative
owner absence removes the corresponding dismissal tombstone.

Status entries contain those four fields plus `host`, `asset`, `runtime`, and
`recommended_action`.
The `host` object contains `state`, `executable`, `version`, `compatibility`, and
`error`. Host state is `missing`, `available`, or `probe_failed`;
`compatibility` is `validated`, `unvalidated`, or `unknown`. Paths, versions, and
errors that are not available are JSON `null`. A validated version is an
observed compatibility test point, not a version requirement.

The `asset` object contains `state`, `path`, and `error`. Asset state is
`missing`, `current`, `modified`, or `unavailable`. The `runtime` object contains
`state`, `running_processes`, `tracked_processes`, and `untracked_processes`.
Runtime state is `not_observable`, `not_running`, `reporting`, or `untracked`.
Reporting requires an active exact shell/run Agent observation with
`lifecycle_integration` authority; process and terminal evidence do not satisfy
it.

`recommended_action` is `none`, `install`, `replace`, `restart_host`, or
`inspect_error`. Replacement requires explicit `--force`; the recommendation
does not authorize a mutation by itself.

Install entries contain `name`, `result`, `path`, and `restart_required`.
Result is `installed`, `replaced`, or `unchanged`. A modified target fails with
`already_exists` unless `--force` is supplied. Invalid roots or unsafe paths fail
with `invalid_argument` before mutation.

Install preview entries contain `name`, `current_state`, `action`, `path`, and
`restart_required`. Current state is `missing`, `current`, or `modified`; action
is `install`, `replace`, or `unchanged`. Preview performs the same path and
replacement validation as installation but does not create or modify anything.

Uninstall entries contain `name`, `result`, `path`, and `restart_required`.
Result is `removed` or `not_installed`. Only the bundled asset file is removed;
directories and unrelated host configuration are retained. Modified content
requires `--force`, while unsafe paths fail before any removal.

## Shell Data

Shell objects use stable scalar fields: `id`, `workspace_id`, `workspace_name`,
`name`, `cwd`, `status`, `exit_code`, and `run`. Missing values are JSON `null`,
not omitted or represented as human placeholders. `status` is `pending`,
`running`, or `exited`.

A run object includes `id`, `generation`, `started_at_ms`, `ended_at_ms`,
`exit_reason`, `exit_code`, `output_revision`, and `environment_has_run_id`.
`exit_reason` is `exited`, `terminated`, `interrupted`, or `null`.
Under protocol 40, a `pending` Shell includes its interrupted previous `run` and
`recovered_agent_id` only when the owning daemon has proven one exact resumable
Agent under its startup configuration. Clients present that exact association as
inactive. A newly created or ineligible pending Shell has `run: null` and no
recovered Agent ID; a retained run is historical and is not live. Protocol-39
responses remove both recovery markers, including from routed responses.

Launcher objects include `id`, `workspace_id`, `workspace_name`, `name`, `cwd`,
and `command`. `command` is the exact executable-and-arguments array.

Agent objects include stable fields `id`, `workspace_id`, `workspace_name`,
`shell_id`, `run_id`, `name`, `integration`, `external_session_id`,
`started_at_ms`, `ended_at_ms`, `attention`, and `observation`. The observation contains
`revision`, `state`, `authority`, `evidence`, `confidence`, and
`observed_at_ms`. Missing optional values are JSON `null`.

Agent `state` is `unknown`, `working`, `blocked`, `idle`, `inactive`, or `done`. `authority`
is `lifecycle_integration`, `process_adapter`, `terminal_heuristic`, or
`daemon_lifecycle`; CLI arguments use hyphens instead of underscores. Confidence
is an integer from 0 through 100. Public mutations accept the first three
authorities; `daemon_lifecycle` is reserved for daemon-originated observations.
External precedence is lifecycle integration over process adapter over terminal
heuristic. Observation revisions begin at 1 and increase with each accepted
changed report. Lower-authority and exact duplicate reports return success with
the unchanged snapshot. Equal-authority changed reports update the observation.
`done` is terminal and has a non-null `ended_at_ms`; an exact completion retry is
an unchanged success, while conflicting later reports fail. Completed records
remain durable and inspectable.

The positional name is optional for `agent register` and `agent ensure`. When
omitted, the CLI supplies a random lowercase `adjective-noun` value before the
request. The returned Agent object always contains the concrete durable name.
Explicit names and an existing record returned by `agent.ensure` are unchanged.

Protocol 15 raises one durable outstanding attention item for every accepted
`blocked` or `done` observation. The item records `reason` (`blocked` or
`completed`) and a copy of the exact raising observation, so later working or
idle reports do not erase an unseen blocker. A newer blocked observation or
terminal completion replaces the older item. Existing records migrated from an
older daemon start acknowledged and do not flood the queue.

`attention acknowledge <agent-id> --observation-revision <revision>` removes
only the item raised by that exact revision. A different outstanding revision
fails with `revision_ahead`; an already empty item is an idempotent unchanged
success. Acknowledgment does not alter the Agent lifecycle observation or
satisfy `agent wait`.

`agent.ensure` requires `--external-session-id` and protocol 10. Its identity key
is `integration`, `external_session_id`, `shell_id`, and `run_id`. When that key
already identifies a unique record, ensure returns the stored snapshot without
applying the supplied name or report. This is the intended identity-recovery
path after an integration reload. Otherwise it creates the record with the same
shape and validation as `agent.register`.

Protocol-42 OpenCode coordination is plugin-facing, not a stable public CLI
automation surface. The hidden shared launcher is likewise an implementation
detail and is absent from `capabilities.data.json_commands`. Paired TUI and
server plugins exchange bounded `boomux.cli/v1` envelopes with these command
descriptors and data shapes:

```json
{
  "schema": "boomux.cli/v1",
  "command": "opencode.claim.ensure",
  "data": {
    "claim": {
      "generation_id": "00000000-0000-0000-0000-000000000000",
      "claim_id": "00000000-0000-0000-0000-000000000000",
      "holder_id": "00000000-0000-0000-0000-000000000000",
      "root_session_id": "ses_exact",
      "workspace_id": "00000000-0000-0000-0000-000000000000",
      "shell_id": "00000000-0000-0000-0000-000000000000",
      "run_id": "00000000-0000-0000-0000-000000000000",
      "agent_id": "00000000-0000-0000-0000-000000000000",
      "holder_count": 1,
      "holder_expires_at_ms": 0
    },
    "agent": {}
  }
}
```

Ensure takes `--generation`, `--holder`, `--root-session-id`, and exact Shell/run
context from arguments or `BOOMUX_SHELL_ID` and `BOOMUX_RUN_ID`. The response
contains the complete bounded `claim` and ensured durable `agent`. Repeating the
exact mapping renews that holder. Another holder may join the same mapping, but a
current mapping to another ShellRun returns `busy`.

```json
{
  "schema": "boomux.cli/v1",
  "command": "opencode.claim.release",
  "data": {
    "claim_id": "00000000-0000-0000-0000-000000000000",
    "released": true
  }
}
```

Release takes exact `--generation`, `--holder`, and `--claim-id`, addresses one
holder, and is idempotent. The final holder removes report authority, not durable
Agent history.

```json
{
  "schema": "boomux.cli/v1",
  "command": "opencode.claim.report",
  "data": {
    "agent": {}
  }
}
```

Report takes `--generation`, `--root-session-id`, lifecycle state, authority,
bounded evidence, and confidence from the server plugin. The daemon resolves the
exact current claim and response `agent`; the plugin cannot supply or move Agent
identity. These requests contain no prompts,
transcripts, credentials, server password, or username. Claim state expires and
is discarded on run or runtime-generation replacement. It is not persisted,
projected, event-published, or transferred during graceful handoff; connected
TUI holders reacquire it afterward.

`agent.wait` requires protocol 14, an exact Agent ID, and
`--after-revision`. A current revision greater than the supplied revision returns
immediately with `changed: true`; an equal revision waits for at most `--wait-ms`
and returns `changed: false` on timeout. Revision zero therefore returns any
existing Agent immediately. A supplied future revision fails with
`revision_ahead`. A terminal `done` observation at the equal revision returns
unchanged immediately. Inactive sessions remain resumable and may advance later.
No-op ensure calls, duplicate reports, and lower-authority reports do not advance
the revision or satisfy a wait.

## Session Data

Session summaries contain `id`, `workspace_id`, `workspace_name`, `description`,
`integration`, `external_session_id`, `state`, `state_is_current`,
`started_at_ms`, `last_at_ms`, and `occurrence_count`. Registered-session
`description` is the latest stored Boomux Agent registration name. Catalog-only
OpenCode or Codex sessions use the sanitized host title, state `unknown`, and
zero occurrences. Missing optional values are JSON `null`.

Inspect includes all summary fields, session-level `source_cwd`, and ordered
`occurrences`. Each occurrence
contains `agent_id`, the original `shell_id` even if that shell was removed,
`retained_shell_name`, `retained_shell_cwd`, `source_cwd`, `run_id`,
`started_at_ms`, `ended_at_ms`, `is_current`, and the full stable Agent
`observation` shape. Retained shell fields are null after shell removal.
`source_cwd` is the registration-time Agent working directory under protocol 13
and can remain available for exact interactive session resume. Protocol-12
snapshots fall back to a currently retained shell directory. State and authority
use the same spellings documented for Agent observations.

Projection groups Agent instances only when workspace, integration, and external
session ID match. An Agent without an external session ID forms its own session.
Bounded OpenCode and Codex catalogs add root sessions to workspaces that
reference the same normalized directory. A matching durable Agent merges into the same stable
ID and supplies authoritative lifecycle state and occurrences.
Current state is selected from occurrences that are incomplete, non-inactive,
and bound to the current run of a running retained shell; otherwise state is the
latest stored observation and `state_is_current` is false. List order is newest
activity first, then workspace ID and session ID.

Session IDs are deterministic globally unique UUID v5 values, but consumers must
treat them as opaque. At a semantic level, Boomux hashes a frozen namespace and
versioned, length-prefixed encoding of workspace ID, integration, and grouping
identity (`external:<id>` or `instance:<agent-id>`). This algorithm is frozen to
keep emitted IDs stable, not exposed for callers to reproduce or guess. Only an
exact ID returned by `session.list` resolves; external IDs, descriptions, shell
IDs, and Agent IDs never resolve through `session.inspect`.
All session commands require a negotiated daemon protocol of at least 12 and return
`unsupported_version` before projection against an older daemon.
`session list`, `session inspect`, and human-only `session resume` accept
`--node SELECTOR` under protocol 36. Remote JSON responses add the exact
`node_id`. Resume opens a local native terminal but resolves the opaque ID and
executes the integration argv only on the owner; it creates no ordinary
Workspace or Shell.

`read` returns `shell_id`, `run_id`, `output_revision`, `changed`, `status`, and
`output`. Output is a JSON string containing the same bounded plain rendered text
as human mode. These fields come from one atomic daemon observation. Conditional
reads can wait for a specific run to advance beyond a supplied revision.

`events` returns an opaque `<stream-uuid>:<event-id>` cursor. See
[`event-stream.md`](event-stream.md) for retention, graceful restart, cold
restart, and revision semantics.

## Errors

JSON failures write one document to stderr and exit nonzero:

```json
{
  "schema": "boomux.cli/v1",
  "command": "shells",
  "error": {
    "code": "not_found",
    "message": "shell not found: ..."
  }
}
```

Failures detected while parsing command-line arguments use `"command": "cli"`
because no valid command was selected.

The stable codes are reported by `boomux capabilities --json`. Messages remain
human-readable context and are not stable parsing targets. `run_changed` means
a supplied run ID no longer matches the run-scoped operation; integrations must
reacquire exact context rather than substitute or guess another run.
The CLI maps typed client failures directly: daemon `RemoteError` codes retain
their stable spelling, while transport, protocol, validation, and lifecycle
variants map to the existing CLI code set without inspecting message text.

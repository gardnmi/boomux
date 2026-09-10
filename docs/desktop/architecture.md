# Architecture

**Jump to:** [Module map](#module-map) · [Threading](#threading-and-backpressure) · [Rendering](#rendering) · [Remotes](#remote-node-entry-points) · [Settings](#shared-boomux-settings)

This is the UI implementation reference. For everyday use, see the
[Desktop guide](../../desktop/README.md).

## System Boundary

Boomux Desktop is a presentation client. Boomux remains the source of truth for
terminal processes and durable resources.

```text
Boomux daemon / PTYs
        │ attach protocol with backpressure
        ▼
per-pane reader ── bounded command queue ── libghostty-vt worker
                                                │ immutable screen snapshot
                                                ▼
                                      GPUI model and image cache
                                                │
                                                ▼
                                      tiled/floating GPU scene
```

The client sends input, focus, and terminal-size frames back through the same
Boomux attachment. Keystrokes enter the pane's bounded emulator command queue,
where Ghostty's reusable key encoder reads the current terminal modes before
encoding and forwarding them; this keeps Kitty keyboard, modifyOtherKeys,
cursor, keypad, and backarrow negotiation ordered with terminal output.
Detaching a pane never implies closing its Boomux Shell.

New terminal creation publishes the attachment before any sidebar overview refresh;
the existing overview worker refreshes resource rows independently. New local
Shells in existing Workspaces use protocol-54 `CreateStartedShell` to combine
creation and first-run persistence, then attach to the returned exact run.
Older owners fall back to pending creation followed by attachment; remote and
new-Workspace creation retain their existing paths. Reattachment
to a running Shell retains the exact run already validated by attach, without a
second owner lookup. Newly started/restarted Shells still resolve their new run.

### Terminal Selection And Clipboard

Selection drag updates use only the source pane's bounds and retain the original
mouse-down anchor. The primary selection updates during the drag. On left-button
release, Desktop copies nonempty selected text from that pane to the system
clipboard once per gesture when `copy_on_select` is enabled (the default).
The preference applies immediately and is saved in Desktop settings. Disabling
it preserves manual copying and primary-selection paste. Pending clipboard work
is a single pane ID, cleared on completion, window deactivation, or Workspace
replacement; no polling or additional terminal snapshots are retained.
Desktop-initiated automatic and manual copies show a source-pane “Copied” badge
for 1.5 seconds, using one replaceable cleanup task per window. Clipboard changes
and terminal output never trigger the badge. Left-button selection gestures are
not forwarded to the harness (current mouse reporting covers wheel events only),
so a Desktop selection cannot simultaneously invoke a harness copy-on-select
handler. Future button forwarding must preserve exclusive gesture ownership.

### Input And Layout Mode

GPUI key contexts separate ordinary terminal input from desktop layout actions.
The default `Terminal` context reserves only explicit lifecycle, clipboard, and
mode-entry commands; other keys reach the pane encoder. `Ctrl+Space` activates
the `Layout` context, where unmodified navigation keys and their Shift/Alt
variants manipulate panes until `Escape` exits. Layout mode blocks new terminal
key presses, repeats, paste, and mouse-wheel reports; releases for keys already
sent still reach their original pane. Double `Ctrl+Space` retains its explicit
pass-through behavior. The leader activates Layout immediately; release before
250 ms latches it, while a longer hold exits on release of Control or Space.

The leader is handled from raw key events, outside the ordinary action binding
dispatcher, so auto-repeat is explicitly ignored even after tracked press state
is cleared. Space release is settled for 50 ms using one cancelable task:
a fresh press during that interval continues the same gesture. This handles
input paths that synthesize repeat as release/press pairs without a repeat flag.
Tap/hold classification uses the release event time, excluding the settling
delay. Control release ends a hold immediately. Window deactivation cancels the
pending release and clears an active hold.

Control-modified Layout bindings support navigation while holding the chord.
Keys pressed in Layout are prevented from repeating into the terminal after
release of the leader; releases for previously forwarded keys still reach their
original pane. Output processing continues while each visible pane
shows a dimming overlay and animated tile icon by default. The saved
`layout_overlay_visible` preference skips constructing these per-pane overlays
when disabled, while retaining the mode badge and input behavior. Overlay removal
shares the badge’s single cancelable exit task; it does not delay restoring input.

Desktop's existing background overview refresh removes Workspaces with no Shells,
including entries found on startup. Read-only discovery remains separate. Each
pass considers at most eight candidates, skips unavailable remote Nodes, reads
the exact Workspace from its owner, and closes it at that observed revision.
Concurrent changes reject the close without an unguarded fallback. Exited and
pending Shells prevent cleanup. This Desktop policy removes Workspace metadata
and history but does not remove project shortcuts or filesystem contents.

## Module Map

- `src/main.rs`: application model, Boomux sidebar projection, input routing,
  pane lifecycle, GPUI elements, terminal cell drawing, and GPU image caching.
- `src/layout.rs`: binary split tree, normalized rectangles, spatial focus,
  pane insertion/removal, swaps, and bounded split ratios.
- `src/terminal.rs`: Boomux discovery/attachment adapter, per-pane terminal
  worker, Ghostty VT state, scrollback, key/paste encoding, and Kitty graphics
  extraction.
- `src/git_panel.rs`: demand-driven Node Git overview, filtering, lower sidebar tab,
  and exact local Shell navigation; see [Git panel](git-panel.md).
- `src/nodes.rs`: read-only Node identity, health, and resource-count presentation
  from the daemon's combined snapshot.
- `src/boomux_settings.rs`: active-layer settings editor and bounded CLI bridge;
  Boomux retains configuration validation and commit authority.
- `src/layout_state.rs`: versioned, bounded Desktop arrangement storage with atomic
  background writes and revision checks preventing stale-instance overwrites.
- `src/layout_persistence.rs`: pane-ID remapping, per-Workspace/Mixed arrangement
  capture/restore, debounced saves, and deferred exact-run attachments. Local
  references are scoped to the verified coordinator Node; remote keys retain
  owner and resource identity. No terminal data or attachment environment is saved.
- `src/settings.rs`: bounded preference loading, validation, and atomic background
  saves of Desktop-owned settings; shared Boomux configuration remains separate.
- `src/layout_badge.rs`: shared animated Layout-mode icons and pane overlays.
- `src/theme.rs`: bounded Omarchy palette loading, semantic application and
  terminal colors, built-in fallback, and the current-theme filesystem watcher.

## Threading And Backpressure

GPUI's thread owns presentation state and must not wait on the daemon, PTY, or
terminal parser. Each attached pane has a socket reader and one terminal worker.
The reader sends byte chunks through a bounded queue. When decoding falls behind,
pressure propagates back through the Boomux socket to the PTY producer; arbitrary
terminal bytes are never discarded because doing so could corrupt escape or
Kitty graphics sequences. A producer clones the queue sender under its mutex,
then releases the mutex before waiting for capacity. Queue saturation must not
prevent GPUI from accessing the sender or cancelling a discarded pane.
Pane focus notifications use a single pending flag and a nonblocking wake marker.
The terminal worker sends the notification, including between replay chunks;
click and hover handlers never acquire the attachment writer or write a focus
frame themselves. Repeated focus requests coalesce while the queue is full.

Discarding a pane cancels its local replay and disconnects the queue sender;
it does not enqueue a blocking stop command. The worker checks cancellation
between 16 KiB decode chunks and releases pending replay data when it exits.
This discards only presentation work for a closed pane, not daemon-owned Shell
output or processes.

After initial attachment and daemon reconnect, the reader requests one redraw
by briefly changing the PTY width and restoring the latest pane dimensions after
100 ms. Repeating identical dimensions may not notify a running TUI. The bounded
settle delay stays on the reader thread; concurrent pane resizing updates the
restored dimensions, and incoming terminal bytes remain subject to backpressure.

The worker coalesces queued commands before publishing a reference-counted,
immutable screen snapshot. A bounded one-event mailbox wakes GPUI only when a
new snapshot or terminal status exists; bursts collapse into one wakeup because
the consumer always reads the newest snapshot. Synchronized-output mode delays
publication until the terminal frame is complete.
When an attachment ends, the worker publishes its final decoded screen before
stopping, including output queued before transport closure. Detachment itself does
not establish command success. The UI watches until the worker closes its update
stream after final publication, rather than stopping at transport closure.

Omarchy theme loading follows the same boundary. A native filesystem watcher
observes `~/.local/state/omarchy/current`, because Omarchy replaces its `theme`
directory atomically. Its capacity-one notification channel is debounced before
a bounded `colors.toml` read runs on the background executor. GPUI installs the
result through atomic semantic color slots and notifies once. Each terminal
worker receives only its latest pending palette through its existing bounded
command path and republishes a screen without reconnecting the Boomux Shell.

## Rendering

Text cells and Kitty image placements come from the same Ghostty terminal state.
The GPUI layer draws background images, cells, and foreground images in z-order,
clips every placement to its pane, and caches GPU images by terminal generation.
Images are explicitly dropped when their generation disappears or their pane
closes. Each pane also owns one shaped-text paint cache keyed by the exact screen
snapshot, selection, and cursor focus. Layout-only animation frames reuse that
cache, while changes to those inputs invalidate it. Window activation and pane
focus determine whether the cursor is filled or outlined; activation changes
request a repaint without waiting for terminal output. Unfocused cursors retain
the underlying cell's foreground and background, and selection takes precedence.

Settings replaces the sidebar resource list while open, so scrolling its controls
does not build or lay out the covered Workspace, Shell, and Agent rows. The
sidebar scroll handle remains window-owned and restores its offset on close.

Both Tree and Tabs draw the binary tile layout and floating layer. The
default Tabs presentation keeps every open pane in that canvas; saved Tree
preferences retain their existing behavior.
`Ctrl+W` still detaches and releases the pane-owned emulator and GPU state, but
the minimized Shell is represented in a restorable strip above the canvas and
is represented only by its restore tab. Tabs hides all nested Shell rows from
the sidebar, leaving Workspace rows as its navigation surface. Restoring a tab
creates a new pane attachment without changing Boomux's durable Shell identity.

## Resource Projection

Workspace rows use split-pane icons, semibold names, and distinct header surfaces;
Shell rows use deeper indentation and regular-weight names. Status indicators
and activation behavior remain separate from this visual hierarchy.

### Project Discovery

The sidebar `+` opens a local creation menu with New Workspace first, followed by
projects discovered through the existing bounded `boomux project list --json`
flow. One background scan runs per opening, without polling or concurrent scans;
loading, empty, warning, and error states remain visible. Settings exposes
`projects.roots` and `projects.max_depth` through the validated core config editor,
with no daemon restart required. Settings and the menu's Add/Manage project folders
action open a native directory-only picker through GPUI's desktop portal support.

Selections append to the effective roots without replacing existing entries;
duplicates are skipped and cancel is a no-op. The manual editor remains available,
including when a desktop file-picker portal is unavailable. Selecting a project creates a new local Workspace named after
the project (adding a numeric suffix on collision), with its default directory
and initial login Shell set to the revalidated project path. It does not infer
membership from an existing Workspace's equal name or launch commands from files.

### Agent Projection

The sidebar is a bounded, read-only Boomux snapshot. Active Agent rows require an
exact current ShellRun. Historical records appear only through explicit Boomux
attention/history semantics; durable records are not assumed to be active.
When multiple visible Agents share a Shell, each row includes a distinguishing
prefix of its exact Boomux Agent ID. Labels do not infer which host thread is
selected or retire an older observation. Clicking either row still opens the
owning Shell; thread selection remains with the host application.
The client keeps a bounded presentation marker for an observed `working` to
`idle` transition so successful completion remains visible until the user
dismisses it. This does not change the Agent lifecycle. Durable attention is
acknowledged against Boomux with the exact Agent ID and observation revision.

Workspace ordering is client-owned presentation state keyed by exact Workspace
identity. Overview refreshes retain the current order, newly discovered
Workspaces append, and drag or keyboard reordering never mutates Boomux's
Workspace authority.

## Remote Node Entry Points

The lower sidebar has Agents, Git, and Remotes tabs. Remotes contains the scrollable
machine cards with selected-machine details and create/update/sign-in controls.
The general connect action sits above the cards, outside any machine's controls;
it is no longer an overflow-menu popover. Node shortcuts apply only while the
sidebar has keyboard focus, so the selected tab does not consume terminal input.
Remote-owned Workspaces appear in the main tree with a monitor icon and machine
status. Desktop keys encode both owner and resource identity, and are decoded
only at the RPC boundary; encoded keys are never passed as owner-local IDs.

Shell creation, attachment, reconnect, rename, close, and attention acknowledgment
use the registered owner's existing guarded APIs. Cached directories are not
invented from local paths. Remote creation resolves the owner's starting directory
and creates the owner-local Workspace and first pending Shell with fresh exact IDs;
ambiguous mutation failures are surfaced without automatic replay.
The initial connect flow creates this Workspace after successful registration.
Open its Shell from the sidebar; creating another Workspace from Remotes also
attaches its first Shell. Multi-placement coordinator metadata is left unchanged.

The Remotes tab retains sign-in actions and adds an explicitly confirmed
remote update action. No background installation or upgrade is performed.
Once remote
Nodes are registered, the existing sidebar subtitle shows a compact Node count
and connection summary. Selection uses stable Node IDs, including when aliases
or routes happen to match. Details show observed health, last observation,
helper version when available, and owner-local Workspace/Shell counts. Cached
counts are explicitly labelled; disconnect does not establish process exit.

The existing window-owned overview worker reads one combined snapshot per
refresh for both local resources and Node summaries. It falls back to local
discovery if federation is unavailable. A failed refresh retains prior Node
summaries but removes their connected presentation. Observation timestamp changes
do not alone repaint an inactive Remotes tab. There is no additional SSH worker,
registration store, or discovery loop in Desktop. Snapshot and registration
bounds remain daemon-owned; the client retains only the latest summary per Node.

### Guided Remote Actions

Connect, update, uninstall, and reauthentication open the matching Boomux CLI's guided
flow in a local daemon-owned Shell, using exact argument vectors. Reauthentication
passes the stable Node ID and leaves route/identity verification to Boomux.
SSH credentials, browser challenges, host verification, installation consent,
and protocol compatibility remain owned by that interactive flow. After the
result acknowledgment, the exact dedicated command Shell/run is removed with a
revision guard. Desktop removes its temporary Workspace only with ephemeral
creation proof, the expected post-removal revision, and no remaining resources.

Ordinary Shells invoking the CLI are not cleanup targets. Remotes does not launch
the separate terminal dashboard. Healthy cards omit generic lifecycle guidance;
unavailable machines retain their observation age and recovery guidance.
The machine-card remove action uses `node uninstall` with the exact Node ID,
retaining its interactive consent, identity/revision checks, and confirmed-removal
registration cleanup. Desktop never substitutes a local forget on failure.
An independent, inline-confirmed **Forget connection only…** action uses the
existing local `ForgetNodeRegistration` request with the selected exact Node ID.

It runs off the UI thread, does not require remote availability, and is never
presented as successful remote uninstall. Concurrent clicks are suppressed;
normal overview refresh removes the forgotten registration's cached rows.

### Remote Attachment

Remote attachment uses `AttachNode`, preserving the exact owner/run on reconnect
and leaving environment ownership on that machine. Local attachment continues
to supply the local ephemeral client environment. This presentation does not
flatten or adopt existing multi-placement coordinator Workspaces.

## Dependency Boundary

Desktop consumes the root Boomux library through `boomux = { path = ".." }`.
The root Cargo workspace owns the shared lockfile and Ghostty CPU-baseline patch.
GPUI and Ghostty remain Desktop dependencies; CLI builds do not require them.
Backend changes are validated against Desktop in the same CI run. A future
focused client/protocol crate can improve the dependency boundary within this
repository without being a prerequisite for the migration.

## Desktop Preferences

Preferences load before GPUI starts, from the XDG configuration directory.
Settings changes submit a complete snapshot to a capacity-one channel; a single
background writer replaces superseded pending snapshots and atomically renames
completed files. A capacity-one result channel reports failures to Settings.
Closing the sender drains the final snapshot and terminates the writer; normal
app shutdown awaits its completion asynchronously within GPUI's shutdown deadline.
Malformed input is preserved and disables saving for that session. The file is
capped at 64 KiB. Desktop preferences never contain Boomux configuration.

## Shared Boomux Settings

The settings UI loads the active file selected by `boomux config path`, after
Boomux validates the layered configuration. Controls show configured values from
the active file, then the global file, then the workspace Boomux defaults. Daemon
defaults come from its public library; CLI-only display defaults mirror the
workspace version and must be reviewed on dependency updates. This is a presentation
projection, not the running daemon's state. Only edited fields are written.

The comment-preserving active draft and global fallback are each capped at 1 MiB.
Text entry is limited to 16 KiB. The UI presents one categorized list with
bordered section groups, label/description rows, compact boolean switches,
segmented choices, and inset editable fields. These are native GPUI-CE controls
using the existing theme palette, not a GPUI Kit dependency. Advanced terminal
setup is placed after the everyday settings.

The Advanced section shows the active core configuration path and opens
`boomux config edit` in a local terminal using the matching CLI. This preserves
the core editor's validation and transactional save behavior; it does not edit
Desktop's separate `boomux-desktop/settings.toml` appearance preferences.

### Configuration Save Transaction

Each completed edit invokes `boomux config edit` with Desktop as its temporary-file editor.
The helper runs before GPUI initialization, checks the original active-layer
snapshot against Boomux's working copy, and writes only the working copy.
Boomux owns validation, ownership checks, inherited-layer conflict checks, and
atomic replacement of the live file. Temporary request files are private to the
user and removed after completion. One load/save may be pending per window.

CLI waits and pipe reads run off GPUI; coreutils timeout owns the subprocess
group, including the helper. Completed daemon-setting edits set one restart reminder. Confirmation appears
when the panel closes or its restart button is clicked, never while choosing
settings. A save finishing after the panel closes also offers confirmation.
The bounded worker invokes only `boomux daemon restart` after confirmation, with
a 30-second outer timeout. Restart uses Boomux's graceful handoff authority.

A persisted Desktop reminder is cleared only after successful restart; it is
a UI reminder, not an independent assertion of the daemon's current config. The bundled smoke test exercises creation, save, conflict rejection, and
owner-side validation failures against the matching Boomux executable.

The direct `toml_edit` dependency pins the version already present through
Boomux, enabling preservation of user comments without adding another version.

## Installation, Setup, And Updates

A window-owned task schedules read-only release checks ten seconds after startup
and every six hours. One bounded worker checks the fixed Boomux GitHub release
endpoint. An official bundle shows one notice only when a newer stable release
contains both the Desktop archive and checksum. Development builds may additionally
query the installed CLI's public `--json update status` command.

Each process has a 20-second deadline and at most 128 KiB retained output; curl
also has a 15-second limit. Closing the window cancels its scheduler; in-flight
work completes within its bounds without retaining the window. Release-notice
dismissals are saved per version and reset by an explicit manual check. Existing settings keys
remain compatible. Completed manual-check results and update errors have a
Dismiss control; active checks and installation progress remain visible until
they finish. No update check installs software or restarts the daemon. Explicit
Update and Restart actions use the transaction below.

`runtime.rs` provides a display-independent `--check-runtime` mode for fixed
system graphics libraries. The installer first checks its glibc baseline, then
both executable versions and this runtime check before committing any active
release change. GPU/driver/display startup remains a separate smoke-test concern.

Desktop does not probe harness installations or offer integration installation
and update notices. Core owns automatic setup, managed-asset refresh, ownership
receipts, and persistent uninstall choices (see the root architecture). There is
no per-window integration worker or Desktop-owned integration preference.

Settings' advanced terminal setup action launches the private `boomux __desktop-setup`
entry point with an exact argument vector in a new daemon-owned Shell. It runs
the same guided setup as `boomux setup`. The CLI keeps agent-integration authority
and prompts; its Ratatui checklist uses the existing Crossterm input path,
restores canonical input before applying selections, and never treats deselection
as uninstall. The legacy `onboarding_complete` preference remains readable but
no longer gates discovery or a generic first-run card.

Setup prints an explicit completion message distinguishing success, remaining
recommended steps, and failures, followed by `Exit and remove this setup Shell?
[Y/n]`. Enter or yes revalidates the exact Shell/run and stored private command,
then requests revision-guarded Shell removal. No keeps the process and output
available and prompts again. EOF or interruption does not authorize removal.
Desktop removes the pane only after output ends and a successful local overview
confirms that Shell is absent. The successful setup creation response also gives
that attachment an ephemeral cleanup receipt containing the owning Node, exact
Workspace ID, and expected revision after removal of its sole setup Shell.

The existing overview worker consumes it once, off the UI thread, verifies the
local Node identity, and uses local `GetWorkspace` and `GuardedCloseWorkspace`
requests. `RouteNodeOperation` is only for registered remote Nodes and must not
be used to address the local owner. Cleanup requires no Shells, launchers, or
Agent history and exactly one revision increment since creation. Edits and adding then removing user resources
therefore preserve even a currently empty Workspace; a mutation racing the final
close is rejected by the owner. There is no unguarded fallback or retry with a
newer revision. Unrelated and reused Workspaces are never selected by name.

Discovery, reattachment, and reopening Desktop do not reconstruct this ownership
receipt; without it the Workspace is retained. A setup attachment failure also
retains its resources rather than invoking unguarded Workspace removal.
Ordinary `boomux setup` does not prompt for Shell removal or remove its caller's
Shell. Cleanup does not turn a failed setup into a successful one.

The ignored `setup_workspace_real_lifecycle_cleans_only_unused_creation` test
requires `BOOMUX_TEST_CLI` pointing to the matching built CLI. It starts a private
fixture daemon and harmless waiting commands, exercises real PTY start, Shell
removal, Desktop output closure and snapshots, then tests unused, reused, and
racing Workspace cleanup without running integration installation.

`bundle_update.rs` owns the local Desktop installation transaction, not PTY or
daemon authority. Release checks report eligibility and a prepared update on the
background executor. A user action runs the embedded installer with `--prepare`;
`pending` is retained without switching `current`. Restart obtains the install
lock, validates owned versioned paths and unchanged selection, delegates graceful
handoff to the candidate CLI, then activates and launches the replacement window.
A bounded window-ready acknowledgment precedes closing the old app. Failure
restores the prior link and delegates reverse handoff to the old CLI; failures
remain visible and retryable. The update worker serializes operations, bounds
process lifetime/output, and never starts per-pane tasks. Existing independent
CLI binaries remain outside its install ownership. A daemon from a different
executable gets a finish-installation reminder after launching a new bundle.

## Edge Resizing

The sidebar exposes a five-pixel right-edge handle. Its preferred width is bounded
to 280–600 logical pixels and stored in Desktop preferences when dragging ends;
older preference files retain the 300-pixel default. The displayed width also
reserves terminal canvas space on narrow windows. Sidebar content, Settings,
menus, and terminal pointer coordinates use the same effective width.

Pane edge handles reuse the existing pointer-drag lifecycle. Floating left/top
resizes preserve the opposite edge, while right/bottom resizes preserve the
origin; all respect canvas bounds and minimum sizes. Tiled handles exist only
where a split borders the selected edge. Direction-aware tree traversal selects
that divider and converts root-relative movement into its local split span.
Twelve-pixel corner targets paint above side handles and resize both axes with
diagonal cursors. Tiled corner targets require adjoining dividers on both axes.
Maximized and transitioning panes omit edge handles. Terminal body selection
and Ctrl-drag behavior retain their existing input paths.

## Internal layout restoration

Layout mutations retain one 250 ms debounce task and one pending writer request;
terminal output and rendering do not generate persistence snapshots. Active drag
state is excluded. Workspace switching captures the outgoing committed tree;
inactive arrangements retain metadata only, not sessions or emulator state.
Files cap at 2 MiB, 256 arrangements, 4096 total panes and depth 64. Invalid or
unsupported files remain untouched and disable writes with a visible notice.

Desktop startup restores the tree and floating geometry before exact-running
attachments. Stopped Shells require explicit user action. Deferred attachments reuse the
existing overview refresh, attempt at most four panes per refresh, and back off
failed attempts up to 30 seconds without per-pane timers. This also allows an
update replacement to attach after the old window releases its terminals. Restoration never authorizes
Shell creation, restart, or attachment takeover. Updates freeze saving and await
the durable snapshot before launching the replacement; failure permits retry.
A per-file lock plus revision comparison prevents stale windows from replacing
newer state. Outer OS window placement remains outside this feature.

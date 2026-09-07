# Architecture

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

GPUI key contexts separate ordinary terminal input from desktop layout actions.
The default `Terminal` context reserves only explicit lifecycle, clipboard, and
mode-entry commands; other keys reach the pane encoder. `Ctrl+Space` activates
the `Layout` context, where unmodified navigation keys and their Shift/Alt
variants manipulate panes until `Escape` exits. Layout mode blocks new terminal
key presses, repeats, paste, and mouse-wheel reports; releases for keys already
sent still reach their original pane. Double `Ctrl+Space` retains its explicit
pass-through behavior. Output processing continues while each visible pane
shows a dimming overlay and animated tile icon. Overlay removal shares the
badge’s single cancelable exit task; it does not delay restoring input.

## Module Map

- `src/main.rs`: application model, Boomux sidebar projection, input routing,
  pane lifecycle, GPUI elements, terminal cell drawing, and GPU image caching.
- `src/layout.rs`: binary split tree, normalized rectangles, spatial focus,
  pane insertion/removal, swaps, and bounded split ratios.
- `src/terminal.rs`: Boomux discovery/attachment adapter, per-pane terminal
  worker, Ghostty VT state, scrollback, key/paste encoding, and Kitty graphics
  extraction.
- `src/nodes.rs`: read-only Node identity, health, and resource-count presentation
  from the daemon's combined snapshot.
- `src/boomux_settings.rs`: active-layer settings editor and bounded CLI bridge;
  Boomux retains configuration validation and commit authority.
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
Kitty graphics sequences.

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
stopping, including output batched with the stop command. Detachment itself does
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
snapshot and selection. Layout-only animation frames reuse that cache, while a
new snapshot or selection invalidates it.

Settings replaces the sidebar resource list while open, so scrolling its controls
does not build or lay out the covered Workspace, Shell, and Agent rows. The
sidebar scroll handle remains window-owned and restores its offset on close.

The default presentation draws the binary tile layout and floating layer. The
optional tabbed-minimization presentation keeps every open pane in that canvas.
`Ctrl+W` still detaches and releases the pane-owned emulator and GPU state, but
the minimized Shell is represented in a restorable strip above the canvas and
is represented only by its restore tab. Tabs hides all nested Shell rows from
the sidebar, leaving Workspace rows as its navigation surface. Restoring a tab
creates a new pane attachment without changing Boomux's durable Shell identity.

## Resource Projection

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

The sidebar overflow menu opens a bounded, scrollable Nodes popover. Once remote
Nodes are registered, the existing sidebar subtitle shows a compact Node count
and connection summary. Selection uses stable Node IDs, including when aliases
or routes happen to match. Details show observed health, last observation,
helper version when available, and owner-local Workspace/Shell counts. Cached
counts are explicitly labelled; disconnect does not establish process exit.

The existing window-owned overview worker reads one combined snapshot per
refresh for both local resources and Node summaries. It falls back to local
discovery if federation is unavailable. A failed refresh retains prior Node
summaries but removes their connected presentation. Observation timestamp changes
do not alone repaint a closed popover. There is no additional SSH worker,
registration store, or discovery loop in Desktop. Snapshot and registration
bounds remain daemon-owned; the client retains only the latest summary per Node.

Add Node and reauthentication open the matching Boomux CLI's existing guided
flow in a local daemon-owned Shell, using exact argument vectors. Reauthentication
passes the stable Node ID and leaves route/identity verification to Boomux.
SSH credentials, browser challenges, host verification, installation consent,
and protocol compatibility remain owned by that interactive flow. These Shells
and their Workspaces follow the existing setup-terminal lifecycle and remain
visible after the command exits until explicitly removed. An Open Boomux
dashboard action provides access to the existing TUI's Nodes tab.

This first native entry point does not yet put remote Shells in the Desktop
canvas. Local Shell/Agent projection and attachment retain their current scope.
Node-qualified remote pane identities, coordinated Workspace presentation, and
connection-loss recovery are the next implementation stages. The popover has its
own input context, closes on Escape or outside click, and blocks terminal input
while open; releases for keys sent before opening still reach their original pane.

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
Text entry is limited to 16 KiB. The UI presents one categorized list with shared control styling.

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

`harness_integrations.rs` checks the matching CLI's `integration status --json`
once at startup and on an explicit Settings recheck. Only supported, successfully
probed local harnesses with missing or differing integration assets produce a
sidebar suggestion. `current` assets remain quiet; `modified` assets require
review because status cannot distinguish an older bundled asset from user edits.
An Install action runs `integration install`; a separate Replace confirmation
allows `--force` for the reviewed integration. The CLI retains all installation,
ownership, and configuration authority. No installation or harness restart runs
as a side effect of discovery. Successful installation displays the integration's
reload instructions. Not now dismisses per window; a manual recheck clears those
bounded dismissals. Settings also exposes status and discovery failures.

One operation per window runs off GPUI, with a 35-second timeout, a one-second
kill grace, and at most 128 KiB retained output. Only bundled integration keys can
become actions. Process argument vectors are exact, JSON envelopes are validated,
and no polling loop, per-pane task, host transcript, or credential cache is added.

Settings' Manual setup action launches the private `boomux __desktop-setup`
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

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

The worker coalesces queued commands before publishing a reference-counted,
immutable screen snapshot. A bounded one-event mailbox wakes GPUI only when a
new snapshot or terminal status exists; bursts collapse into one wakeup because
the consumer always reads the newest snapshot. Synchronized-output mode delays
publication until the terminal frame is complete.

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
The client keeps a bounded presentation marker for an observed `working` to
`idle` transition so successful completion remains visible until the user
dismisses it. This does not change the Agent lifecycle. Durable attention is
acknowledged against Boomux with the exact Agent ID and observation revision.

Workspace ordering is client-owned presentation state keyed by exact Workspace
identity. Overview refreshes retain the current order, newly discovered
Workspaces append, and drag or keyboard reordering never mutates Boomux's
Workspace authority.

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

## Update Notices

A window-owned task schedules read-only release checks ten seconds after startup
and every six hours. One bounded worker checks the fixed Boomux GitHub release
endpoint. An official bundle shows one notice only when a newer stable release
contains both the Desktop archive and checksum. Development builds may additionally
query the installed CLI's public `--json update status` command.

Each process has a 20-second deadline and at most 128 KiB retained output; curl
also has a 15-second limit. Closing the window cancels its scheduler; in-flight
work completes within its bounds without retaining the window. Dismissals are
saved per version and reset by an explicit manual check. Existing settings keys
remain compatible. No update check installs software or restarts the daemon.

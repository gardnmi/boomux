# Boomux Desktop

A fast native tiling terminal workspace for [Boomux](https://github.com/gardnmi/boomux), built with GPUI and Ghostty's terminal core.

> [!WARNING]
> Boomux Desktop is experimental. Official Linux release bundles are available;
> the desktop interface and supported feature set are still evolving.

The prototype uses [GPUI Community Edition](https://gpui-ce.github.io/) and `libghostty-vt`, pinned to released crate versions for reproducible builds. Boomux remains the shell backend: it owns the PTY lifecycle, persistence, reconstruction, and transport, while libghostty interprets those bytes and maintains the reflowing terminal grid rendered by GPUI.

The layout panes are managed inside one GPUI window, and every pane hosts an independent **real local Boomux shell**. Each pane renders its shell's terminal output in GPUI and sends keyboard input and terminal resize events back through Boomux's attachment protocol. This lets us test the product shape without embedding Wayland client buffers or running a terminal emulator process per tile.

An Omarchy Boomux-inspired sidebar presents the local workspace tree, shell status, and current/attention-bearing agents. When an agent settles from working to idle, its row remains marked **finished** until **Dismiss** is clicked; durable Boomux attention is acknowledged with the exact observation revision. Workspace rows expand and collapse. Clicking a shell or agent focuses its existing tile, or attaches its shell in a new tile when it is not already open. The overview refreshes from Boomux in the background without putting daemon requests on the GPUI render path. Nodes and web controls remain outside this proof of concept.

When several visible Agents share a Shell, their rows include distinct Agent ID
prefixes. They represent separate threads, and clicking either row opens the
shared terminal. Select the conversation inside the host application.

## Install release builds

The Linux installer installs Boomux Desktop and its matching Boomux executable
together, without sudo or a local Rust/Zig toolchain. Unified releases have been
available since v1.10.0:

```sh
curl -fsSL https://github.com/gardnmi/boomux/releases/latest/download/boomux-desktop-installer.sh | sh
```

The initial release target is Linux x86-64 with glibc 2.39+, built on Ubuntu 24.04.
It needs the system's graphics drivers and Fontconfig, Wayland/X11, XCB
shape/xfixes, xkbcommon, and Vulkan libraries plus a working GPU driver. The installer checks
minimum glibc, executable versions, and graphics-library availability before
activation; see [runtime packages](../README.md#requirements). Older glibc distributions and musl-based
distributions such as Alpine are not supported by this bundle. macOS, Windows,
and ARM builds are not provided yet.

Launch **Boomux Desktop** from the application menu, or run `boomux-desktop`. The launcher invokes the bundled
`boomux daemon start` before opening the window. Boomux reuses a running daemon
or starts one; closing Desktop leaves the daemon and Shells running.

Commands are linked in `~/.local/bin`. If that directory is absent from your
PATH, add `export PATH="$HOME/.local/bin:$PATH"` to your shell configuration.
The main `boomux-installer.sh` also offers Desktop or CLI-only installation.
Rerun the Desktop installer to download the latest stable release. Updates switch a
`current` link without overwriting running executables or restarting the daemon.
When an existing daemon uses a different executable, the new app offers to
finish updating it through graceful restart. Existing Boomux CLI installations
are preserved. See
[release packaging](../docs/desktop/releases.md) for version selection, install locations,
release preparation, uninstall instructions, and the required platform smoke tests.

Core installs detected harness integrations in the background when the Boomux
service starts, including after an update handoff. Unchanged Boomux-managed
integrations are refreshed to the bundled version automatically. Desktop has no
harness detection, install prompts, or update prompts. User customizations and
uninstall choices are preserved. Running harnesses may need a restart; Codex
still requires its own hook trust approval. See [automatic integration management](../docs/install.md#automatic-integration-management).

The header menu contains **Nodes**, updates, and keyboard shortcuts. The gear
opens Settings. **Settings → Open advanced setup in terminal** remains available
for the guided terminal checklist: **Up/Down** to choose, **Space** to toggle,
**Enter** to apply, and **Esc** to cancel. See the
[checklist controls and defaults](../docs/install.md#harness-checklist).
Setup does not install the optional Omarchy plugin, change Hyprland, or require
an external terminal emulator.
After the completion receipt, **Exit and remove this setup Shell? [Y/n]** defaults
to yes. Enter or `y` removes the dedicated setup Shell and its pane; `n` keeps
the output available and prompts again. Setup failures remain labelled as failures.
Its newly created Workspace is also removed if it is still empty and untouched
apart from that Shell removal. User resources, edits, or uncertain ownership keep
the Workspace. Reattached setup panes without the original creation receipt also
retain it. Ordinary CLI `boomux setup` never removes its Shell or Workspace.

Desktop appearance and behavior preferences save automatically to
`~/.config/boomux-desktop/settings.toml` (`XDG_CONFIG_HOME` is respected).
Settings presents one categorized list and saves changes automatically. A single
restart reminder appears when needed. Finish choosing settings, then
close the panel to choose **Restart now** or **Later**.
Restart preserves running shells and commands. Pane arrangements and window
geometry are not yet saved.
See [preferences and uninstalling](../docs/desktop/releases.md#desktop-integration-and-preferences).

Desktop checks for newer stable application releases shortly after startup
and every six hours. Official installations offer **Update**, **View release**, and **Dismiss**. Dismissal
is saved for that version; a newer release can notify you again. Use **Check for
updates** in the header menu to revisit dismissed notices. Checks do not install software or restart anything. **Update** downloads and
verifies the full application on a worker. Choose **Restart now** to gracefully
restart Boomux and open the updated Desktop, or **Later** to keep working. The
prepared update is retained across restarts. A failed replacement-window launch
restores the old bundle and requests daemon recovery; an error remains visible.
Official bundles show one notice for the complete application. Source and manually
unpacked builds retain release links and are not eligible for in-app installation.

## Run from source

```sh
python3 desktop/scripts/run-dev.py
```

Building the vendored libghostty dependency requires Zig 0.15.2 on `PATH`. It is only needed at build time. If Zig is installed somewhere outside `PATH`, prepend that directory when invoking Cargo:

```sh
PATH=/path/to/zig-0.15.2:$PATH python3 desktop/scripts/run-dev.py
```

Both packages are built from this workspace using a shared lockfile and version.
The development helper builds both binaries, selects the matching CLI, and starts
an isolated development daemon under `target/desktop-dev/`. Run commands from the
repository root. On startup the app selects the most
recently focused local Boomux Shell, falling back to the first available Shell,
then opens every Shell in its Workspace and focuses the selected one. A pending
Shell starts, an exited Shell restarts, and a running Shell is taken over from
its current terminal attachment. There is no intermediate shell picker.

For development and repeatable terminal integration tests, `BOOMUX_DESKTOP_SHELL_ID=<exact-local-shell-id>` overrides the initial shell selection.

`Ctrl + Enter` creates a new Shell and opens it in a new tile. With terminal focus, it uses the focused terminal's Boomux Workspace. With sidebar focus, it uses the selected Workspace or the parent Workspace of the selected Shell or Agent. New Shells use collision-safe random `adjective-noun` names. The sidebar's `+` button creates a randomly named Workspace with its first randomly named Shell and opens it immediately. In the default single-Workspace pane mode, clicking a Workspace or pressing `Enter` while its row is selected opens its non-minimized Shells and collapses every other Workspace in the sidebar. Shells minimized with `Ctrl + W` remain minimized when switching away and returning to a Workspace. In Mixed mode, clicking a Workspace collapses or expands it without collapsing the others. `Space` also collapses or expands the selected Workspace, and **Open workspace** remains available from its `⋮` menu. Clicking an individual minimized Shell opens only that Shell. `Ctrl + W` and the pane header's minimize button close the focused tile and detach from its Shell; the Shell and its process remain alive in Boomux and can be reopened from the sidebar. Plain left-drag on a pane heading moves or rearranges it without affecting terminal text selection. The pane heading also provides float/dock, maximize/restore, and close buttons. Maximize uses the same workspace-scoped fullscreen behavior as Layout mode's `F`; close permanently terminates and removes the Boomux Shell. Closing the app itself has the same detach-only behavior, preserving Boomux's normal persistence. Each Workspace row has a `⋮` menu for opening it, creating a Shell, renaming, or permanently removing the Workspace. Each Shell row has a `⋮` menu for renaming or permanently removing that Shell. Destructive operations require confirmation by default; **Confirm removals** in Settings can disable those prompts.

New Workspaces append to the bottom of the desktop's Workspace list. Drag a
Workspace row in either direction to reorder it; the dragged row follows the
pointer and neighboring rows slide into place using the selected motion speed.
You can also focus it in the sidebar and use `Ctrl+Shift+Up/Down`.

The application window title includes the Workspace owning the focused terminal
pane.

Kitty graphics are decoded by `libghostty-vt` and composited into each GPUI terminal canvas with placement cropping and z-ordering. This is sufficient for raw RGB/RGBA applications such as Terminal Doom. From a Boomux shell whose working directory contains `doom1.wad`, run:

```sh
/home/gardnmi/Projects/terminal-doom/zig-out/bin/terminal-doom
```

The prototype uses Ctrl on Linux so its input reaches the app while it is running under Hyprland; Super combinations are normally intercepted by the real compositor. GPUI's portable `secondary` modifier makes these Command shortcuts on macOS. `F1` opens the complete in-app shortcut reference:

| Shortcut | Behavior |
| --- | --- |
| `F1` | Open or close the keyboard-shortcut help menu |
| `F6` | Move keyboard focus between the sidebar and the active terminal |
| `F2` | Rename the selected sidebar Workspace/Shell, or the focused terminal's Shell |
| `Ctrl + Space` | Tap to toggle Layout mode; hold for 250 ms or longer for temporary Layout until Control or Space is released; double-tap to send Ctrl+Space to the terminal |
| `Ctrl + left drag` | Lift, move, and re-tile a tiled pane |
| `Ctrl + right drag` | Resize a tiled split or floating pane in both axes |
| `Left drag on a pane edge` | Resize a floating pane or the adjoining tiled divider |
| `Left drag on a pane corner` | Resize both axes; tiled corners require two adjoining dividers |
| `Left drag on the sidebar’s right edge` | Resize the sidebar; width is saved |
| Layout: `Arrow keys` | Focus a spatially adjacent pane (`H/K/L` also focus left/up/right) |
| Layout: `Tab` or `Shift + Tab` | Cycle focus through panes forward or backward |
| Layout: `Shift + Arrow keys` or `Shift + H/J/K/L` | Slide-swap a tiled pane, or move a floating pane |
| Layout: `Alt + H/J/K/L` | Precisely resize the focused tiled or floating pane |
| Layout: `Alt + Arrow keys` | Resize the focused tiled or floating pane by a normal step |
| Layout: `Alt + Shift + H/J/K/L` | Resize the focused tiled or floating pane by a large step |
| Layout: `J` or `S` | Toggle the nearest split between horizontal and vertical, like Omarchy’s `Super + J` |
| Layout: `E/R` | Equalize or swap the nearest split |
| Layout: `Alt + Shift + Arrow keys` | Align a floating pane to the corresponding canvas edge |
| Layout: `C` | Center a floating pane |
| Layout: `O/F/B` | Toggle floating, workspace maximize, or the sidebar drawer |
| Layout: `Page Up/Page Down` | Cycle backward or forward through Workspaces in sidebar order |
| Layout: `Escape` | Return to normal terminal input |
| `Ctrl + Enter` | Create a Boomux Shell in the selected or focused Workspace and open it in a new tile |
| `Ctrl + W` | Minimize and detach the focused pane, preserving its Boomux Shell and creating a top tab in Tabs layout |
| `Ctrl + Shift + W` | Permanently remove the selected or focused Boomux Shell |
| `Mouse wheel over terminal` | Scroll through retained terminal history |
| `Shift + Page Up/Page Down` | Scroll terminal history by one viewport, matching Ghostty on Linux |
| `Shift + Home/End` | Jump to the top or bottom of terminal history |
| `Left drag over terminal text` | Select visible terminal cells and publish the selection to the Linux primary clipboard |
| `Ctrl + Shift + C` | Copy the active terminal selection to the system clipboard |
| `Ctrl + Shift + V` | Paste the system clipboard into the focused terminal |
| `Super + C/V` on Omarchy | Universal copy/paste through Omarchy's terminal bindings |
| `Middle click` | Paste the Linux primary selection into the focused terminal |

Terminal keys use Ghostty's negotiated keyboard encoder. `Shift+Enter` uses the
portable Ctrl+J newline representation so it continues to insert a newline in
Codex even after attaching to an existing Shell whose earlier Kitty keyboard
negotiation is unavailable; plain `Enter` still submits. Other legacy keys
retain their conventional encodings.

While the sidebar has keyboard focus, `Up`/`Down` or `J`/`K` moves through
visible rows, `Left`/`Right` or `H`/`L` collapses and expands Workspaces,
`Enter` opens the selected Workspace or Shell, `Space` toggles a Workspace's
expanded state, `Ctrl+Enter` creates a Shell in that row's Workspace, `F2`
renames the selected Workspace or Shell, `Tab` moves between the Workspaces and
Agents sections, and `Escape` or `F6` returns to the terminal.

Focus follows the pointer as it moves over panes. Keyboard focus remains in the
sidebar when the pointer is stationary over a pane and returns to that pane only
after genuine pointer movement. Clicking still focuses a pane and begins any
requested drag operation.

The compact Boomux sidebar header keeps Workspace creation visible and places
Settings, Keyboard Shortcuts, and Hide Sidebar in its overflow menu. Settings
opens as a scrollable sidebar screen with consistent segmented and stepper
controls, and can disable removal confirmations. Layout mode's `B` command opens
the sidebar again after it has been closed. Drag the sidebar's right edge to
the far left to collapse it. When collapsed, drag right from the window's left
edge to reopen it. The sidebar follows the pointer below its normal minimum
width before collapsing at the edge. Its content is clipped at a readable
layout width rather than reflowing into the narrow opening. Releasing
before collapse settles back to the minimum readable width, animated when
motion is enabled. The sidebar retains its preferred width when collapsed.

On Omarchy, Boomux Desktop follows the active system theme automatically. It
loads the semantic palette and terminal ANSI colors from
`~/.local/state/omarchy/current/theme/colors.toml`, then watches Omarchy's
atomically replaced `current` theme directory for live changes. The sidebar,
settings, dialogs, pane chrome, focus treatment, terminal defaults, cursor, and
ANSI palette update without restarting or reconnecting panes. A missing or
invalid Omarchy theme uses Boomux Desktop's built-in palette, which also keeps
the application usable on non-Omarchy systems. Settings reports whether the
Omarchy provider or the fallback is active.

Settings default to **Workspace** pane scope: opening a Workspace replaces the
canvas with its remembered non-minimized Shells, while opening a Shell restores
only that Shell. **Mixed** scope preserves the free-form behavior where Shells
from different Workspaces can share the canvas. Settings can also hide pane
headings and switch the pane layout between **Tiled** (the default) and
**Tabs**. Tabs leaves every open terminal in the tiled and floating canvas.
When `Ctrl+W` minimizes a pane, its detached Boomux Shell appears in a strip
across the top; clicking that tab restores the Shell as a pane. Tabs keeps the
sidebar's Workspace list compact by hiding all nested Shell rows. The strip's
arrow controls reveal overflowed tabs, and each tab has a rename control. Tabs is
Workspace-only: selecting it changes pane
scope to **Workspace** and disables **Mixed** in Settings. Settings can also
switch pane edges between rounded, square, and
mixed. Mixed gives every pane a stable, randomly varied set of corner curves;
the same menu adjusts spacing between tiled panes, the strength of the focused
pane border and heading, and window-motion speed. Motion can be Instant, Fast,
or Smooth (the default) and applies to pane swaps, reflow, and tiled/floating
transitions. In Workspace scope it also slides the outgoing and incoming pane
sets in sidebar order when switching Workspaces. These options affect
presentation only and do not change Boomux resources.

Minimizing and restoring panes follows the selected motion speed. Fast and
Smooth animate the pane and surrounding layout; Instant applies the new layout
immediately.

Sidebar Shell indicators distinguish pane state from process state: `●` is the
focused pane, `◉` is another open pane, and `○` with `minimized` metadata means
the pane is closed while its durable Boomux Shell remains available.

Window spacing applies both between panes and around the workspace canvas; at
0px, panes meet each other and the canvas edges with no inset.

The **Keyboard shortcuts** overflow item opens the same help menu as `F1`. The menu lists shortcuts by section, prioritizes sidebar commands while the sidebar is active, supports the mouse wheel, Arrow keys, `J`/`K`, Page Up/Page Down, Home/End, and closes with `Escape` or `F1`. While it is open, its separate key context prevents pane commands from reaching the terminal or changing the layout behind the overlay.

Attached terminals show a scrollbar on their right edge. Its thumb is derived from libghostty's native `total`, `offset`, and `len` viewport state and can be dragged anywhere in retained history. The scrollbar fades in on hover, remains visible while dragged, fades out after exit, and keeps the normal arrow cursor.

Each pane gives Ghostty a 4 MiB page-memory budget for the primary screen and
scrollback. History is allocated as output arrives; the number of retained
lines varies with terminal width and cell contents. This is separate from
Boomux's durable Shell history and reconstruction on attachment.

When a Boomux terminal tile is focused, ordinary typing and common control and
navigation keys—including `Ctrl+C`, `Ctrl+Arrow`, and `Ctrl+H/J/K/L`—are sent
to the shell. `Ctrl+Space` enters an explicit Layout mode whose persistent
on-canvas indicator remains visible until `Escape` or `Ctrl+Space` exits it.
Holding the chord for at least 250 ms instead makes Layout temporary: releasing
Control or Space exits it. Layout commands also work with Control held, including
arrows and Tab, and key repeat never repeatedly toggles the mode.
The indicator's four tiles spring into place, gently pulse while the mode is
active, and fold together on exit. Focus changes and animated layout actions
replay the tile assembly. It follows the Motion setting; Instant uses a static
badge. Each visible pane dims its terminal contents and shows the animated tile
logo in its center. While Layout mode is active, typing, paste, and terminal
mouse-wheel input are blocked; output continues updating. Exiting immediately
restores input while the overlay fades away.

After a small movement threshold, a tiled pane is temporarily lifted into the floating layer and follows the pointer. Neighboring tiles ease into the freed space, and dropping over the left, right, top, or bottom side of another tile animates the pane back into the resulting tiled layout. A modified click without movement does not mutate geometry. Directional focus and swaps prioritize panes sharing the requested edge before falling back to diagonal candidates. Explicitly floating panes remain floating, rise above other floating panes when focused, and are clamped to the panel during keyboard movement and resizing; Layout mode's `O` command toggles that persistent mode with an eased grow-and-center transition.

Keyboard swaps use the same eased geometry transition, so both affected panes
slide between their previous and new tiled positions.

Workspace maximize temporarily expands the focused pane over the main terminal canvas while keeping the sidebar and its controls visible. Its tiled position or floating bounds remain unchanged and return with the reverse animation when Layout mode's `F` command is pressed again.

In Tabs layout, ordinary geometry and focus commands continue to operate on all
open tiled and floating panes. Tabs consume only the height needed for currently
minimized Shells and disappear when every tab has been restored.

## What this proves

- A binary split tree maps naturally to nested GPUI flex layouts.
- Focus can be calculated spatially instead of relying on DOM/render order.
- Tiled and floating layers can coexist in one GPUI scene.
- Hyprland-like operations can be expressed as ordinary GPUI actions.
- Multiple Boomux shells can be created, attached, reconstructed, rendered, typed into, resized, closed, detached, and reconnected without launching separate terminal windows.
- Kitty graphics capability negotiation, decoded RGB/RGBA images, placement clipping, and z-ordering can be bridged from a Boomux PTY through `libghostty-vt` into GPUI.

## What it does not prove yet

- Full Ghostty rendering parity. GPUI still performs its own cell and image drawing, so advanced cursor styles, PNG transmission, Unicode placeholders, animation, and some Kitty graphics edge cases remain incomplete.
- Mouse reporting for terminal applications, IME, selection spanning unloaded scrollback, hyperlinks, or ligatures.
- Hosting or compositing arbitrary Wayland clients.
- Compositor-grade animation physics and configurable animation curves
- Multiple workspaces and monitors
- Damage tracking, client lifecycle, or compositor security boundaries

The layout model is isolated in `src/layout.rs`; the Boomux lifecycle, attachment, and libghostty terminal adapter live in `src/terminal.rs`.

## Performance

For automated checks and recommended branch protection, see
[continuous integration](../docs/desktop/ci.md).

Boomux Desktop is intended to stay responsive and memory-efficient as humans
and agents create more terminals than traditional single-user workflows. The
desktop client therefore treats bounded state, backpressure, render-path
isolation, and measured regressions as architectural requirements rather than
late optimization work.

Boomux owns server-side scale across Shells and attached clients. Boomux Desktop
owns the incremental cost of each visible terminal pane, decoded image, and
frame. See [docs/performance.md](../docs/desktop/performance.md) for the measurement model
and [docs/architecture.md](../docs/desktop/architecture.md) for the ownership boundary.

## Git overview

Select **Git** beside **Agents** in the lower sidebar or use Layout mode (`Ctrl+Space`, then `G`). Search and refresh sit beside the tabs. Drag the divider above them to resize the section. The selected tab is remembered, and a blocked-Agent count remains visible on Agents while viewing Git. Git connects worktrees to their Shells and Agents and shows local changes, upstream comparisons, and available GitHub PR/check status. See [Git panel](../docs/desktop/git-panel.md) for refresh behavior and status semantics.

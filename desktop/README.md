# Boomux Desktop

A native terminal workspace with Hyprland-inspired movement and keyboard
controls. Built with [GPUI Community Edition](https://gpui-ce.github.io/) and
Ghostty’s terminal core, backed by persistent Boomux Shells.

[Install](#install-release-builds) · [Workspaces](#workspaces-and-shells) ·
[Controls](#controls) · [Settings](#settings) · [Development](#run-from-source)

> [!NOTE]
> Stable Desktop releases support GNU/Linux x86_64 with glibc 2.39+, X11 or
> Wayland, and a working Vulkan driver. Ubuntu 24.04+ and current Arch are the
> runtime baseline. Releases also include experimental Apple Silicon macOS 15+
> builds, ad-hoc signed and not notarized. Shells persist when you close Desktop; internal pane arrangements are saved across restarts. Outer window geometry
> is not saved. See [current limitations](#current-limitations).

## Install Release Builds

Install Desktop and its matching Boomux CLI together:

```sh
curl --proto '=https' --tlsv1.2 -LsSf \
  https://github.com/gardnmi/boomux/releases/latest/download/boomux-installer.sh | sh -s -- --desktop
```

**Requires:** GNU/Linux x86_64 with glibc 2.39+, X11 or Wayland, and Vulkan;
or Apple Silicon macOS 15+. The same command detects the OS. Hyprland is not
required. macOS builds are experimental, ad-hoc signed, and not notarized. See [runtime packages](../README.md#requirements)
and the [installation contract](../docs/install.md).

On Mac, open the installed version of Boomux in `~/Applications`. See the
[Mac guide](../docs/platforms/macos-testing.md) for first launch and updates.

The installer runs without sudo or a local Rust/Zig toolchain. On Linux it adds an
application-menu entry and command links under `~/.local/bin`, preserving any
independent Boomux CLI installation.

On Linux, launch **Boomux Desktop** from the application menu, or run:

```sh
boomux-desktop
```

The launcher starts or reuses the Boomux service automatically.

## Workspaces And Shells

Desktop automatically removes Workspaces that have no Shells, including empty
entries left from earlier use. Opening a project again creates a fresh Workspace;
project shortcuts and files on disk are preserved. Workspace-specific launchers,
folder defaults, and retained Agent history are removed with the Workspace.
Exited Shells still count as Shells until explicitly removed. Unavailable remote
Workspaces remain visible until their owning machine can confirm they are empty.

| To… | Use… |
| --- | --- |
| Create a Workspace | **+ → New workspace** |
| Open a project | Open **+**, type to filter projects by name or path, then choose a project |
| Add project folders | **Settings → Projects → Browse for folders** |
| Create a Shell | **Ctrl + Enter** |
| Rename a Workspace or Shell | Select it, then **F2** |
| Open or manage a Workspace | Its sidebar row or **⋮** menu |
| Reorder Workspaces | Drag a row, or focus it and press **Ctrl + Shift + Up/Down** |

New Shells use generated names. With terminal focus, **Ctrl + Enter** uses that
terminal’s Workspace; with sidebar focus, it uses the selected row’s Workspace.

In the default **Workspace** scope, selecting a Workspace shows its
non-minimized Shells and collapses the other Workspace rows. Restoring an
individual Shell opens only that Shell.

### Minimize Is Not Remove

| Action | Result |
| --- | --- |
| **Ctrl + W** or the pane’s minimize button | Detach the view; keep the Shell running |
| Restore a tab or open a Shell from the sidebar | Reattach to that Shell |
| Quit Desktop | Leave the service and Shells running |
| Pane close button or **Ctrl + Shift + W** | Permanently remove the Shell and terminate its run |
| Remove a Workspace | Remove its managed resources through their owners |

Removal confirmation is enabled by default. **Settings → Confirm removals**
can disable those prompts. Minimized Shells stay minimized when switching
Workspaces and returning.

## Layout And Movement

- **Move:** drag a pane heading, or Ctrl + left-drag within a pane. The pane
  lifts out of the tree, follows your pointer, and re-tiles where you drop it.
- **Resize:** drag a pane edge. Corners resize both axes when adjoining splits
  allow it. Ctrl + right-drag also resizes.
- **Float:** use **O** in layout mode to keep a pane floating.
- **Expand:** use **F** in layout mode to fill the terminal canvas. Press it again
  to restore the pane’s previous position; the sidebar stays available.

Focus follows genuine pointer movement. A stationary pointer over a pane does
not steal keyboard focus from the sidebar.

[Movement demo](../website/public/demos/move.gif) ·
[Resize demo](../website/public/demos/resize.gif) ·
[Videos with playback controls](https://gardnmi.github.io/boomux/#in-motion)

### Tree, Tabs, And Scope

| Setting | Behavior |
| --- | --- |
| **Tabs** — default | Open panes remain tiled or floating. Minimized Shells appear as restore tabs across the top; nested Shell rows are hidden in the sidebar. |
| **Tree** | Shells remain visible under their Workspaces in the sidebar. |
| **Workspace** scope — default | Selecting a Workspace replaces the canvas with that Workspace’s panes. |
| **Mixed** scope | Panes from different Workspaces can share the canvas. Available with Tree, not Tabs. |

Tabs uses Workspace scope. Its strip disappears when no Shells are minimized;
overflow arrows reveal additional tabs, and tabs have rename controls.

## Controls

Press **F1** for the complete in-app reference. **Keyboard shortcuts** in the
header menu opens the same help.

### Enter And Leave Layout Mode

| Shortcut | Action |
| --- | --- |
| Tap **Ctrl + Space** | Toggle layout mode |
| Hold **Ctrl + Space** for at least 250 ms | Enter temporarily; release Control or Space to leave |
| Double-tap **Ctrl + Space** | Send Ctrl + Space to the terminal |
| **Escape** | Leave layout mode |

Layout mode shows a badge and dims the panes by default. Turn off **Layout
overlay** in **Settings → Appearance** to keep terminal contents visible without
the animated pane overlay. The choice saves automatically; the mode badge and
layout controls remain available. Terminal output continues, but
typing, paste, and terminal wheel input are blocked until you leave.
Layout commands also work with Control held during the temporary chord.

### Navigate And Arrange

These shortcuts apply **inside layout mode**.

| Shortcut | Action |
| --- | --- |
| **Arrow keys** | Focus a neighboring pane |
| **H / K / L** | Focus left / up / right |
| **Tab / Shift + Tab** | Cycle pane focus |
| **Shift + Arrow keys** or **Shift + H/J/K/L** | Swap tiled panes or move a floating pane |
| **Alt + Arrow keys** | Resize by a normal step |
| **Alt + H/J/K/L** | Resize by a small step |
| **Alt + Shift + H/J/K/L** | Resize by a large step |
| **J** or **S** | Toggle split orientation |
| **E / R** | Equalize / swap the nearest split |
| **O / F / B** | Toggle floating / expand pane / toggle sidebar |
| **Page Up / Page Down** | Switch Workspaces in sidebar order |

For floating panes, **Alt + Shift + Arrow keys** aligns to a canvas edge and
**C** centers the pane. Note that **J toggles a split**; use Down to focus below.

### Sidebar

| Shortcut | Action |
| --- | --- |
| **F6** | Switch focus between sidebar and terminal |
| **Ctrl + Alt + B** | Show or hide the sidebar |
| **Up/Down** or **J/K** | Navigate visible rows |
| **Left/Right** or **H/L** | Collapse or expand a Workspace |
| **Enter** | Open the selected Workspace or Shell |
| **Space** | Toggle a Workspace’s expanded state |
| **Tab** | Move between Workspace and Agent sections |
| **Escape** or **F6** | Return focus to the terminal |

Drag the sidebar’s right edge to resize it. Drag all the way left to collapse;
drag right from the window’s left edge to reopen. Its preferred width is saved.

### Terminal Input

Outside layout mode, ordinary typing and control keys reach the Shell.
The active terminal shows a filled cursor; unfocused panes and inactive Desktop
windows show a hollow outline.

| Shortcut | Action |
| --- | --- |
| Left-drag over text | Select visible cells; copy to the clipboard on release by default |
| **Ctrl + Shift + C / V** | Copy / paste the system clipboard |
| Middle click | Paste the Linux primary selection |
| Mouse wheel | Scroll retained history |
| **Shift + Page Up / Page Down** | Scroll by a viewport |
| **Shift + Home / End** | Jump to the top / bottom of retained history |

You can also drag the terminal scrollbar. On Omarchy, its universal
**Super + C/V** bindings provide copy/paste.

**Shift + Enter** uses the portable Ctrl + J newline representation; Enter
still submits. Other keys use Ghostty’s negotiated keyboard encoder.

## Sidebar Panels

Drag the divider above **Agents | Git | Remotes** to resize this section.

### Agents

Click an Agent to focus or open its Shell. When an observed working Agent
becomes idle, its row stays marked **finished** until dismissed.

If several Agent threads share a Shell, their rows include distinct Agent ID
prefixes. They open the same terminal; choose the conversation in the harness.

Bundled integrations are prepared automatically by the Boomux service, including
on remote machines. Unchanged managed integrations update with Boomux.

- Customizations and uninstall choices are preserved.
- Already-running harnesses may need restarting to load changes.
- Codex still requires its own hook trust approval.
- Desktop does not ask you to detect, install, or update harness integrations.

See [automatic integration management](../docs/install.md#automatic-integration-management).

### Git Overview

Select **Git** for repositories, worktrees, local changes, upstream comparisons,
and available GitHub PR/check status. Search and refresh sit beside the tabs.

The selected tab is remembered. A blocked-Agent count remains visible while
you view Git. See [Git panel behavior](../docs/desktop/git-panel.md).

### Remotes

Use **+ → New remote workspace…** to choose a machine. Select a connected
machine to create and open a Workspace there, or choose **Connect another
machine…** to set up a new SSH connection. Use Up/Down and Enter to choose;
Escape closes the picker. You can also connect from the Remotes tab.

Setup asks for an SSH address and a display name, using your existing SSH
configuration. Authentication and any installation consent happen in the setup
terminal. After successful setup, press Enter to close setup and open the exact
remote Shell it created. If opening fails, use the sidebar to reopen that Shell
rather than repeating setup.

Remote Workspaces use a machine icon and show connection status. Their Shells
run on that machine. Connection loss does not mean remote work has stopped.

Machine cards start collapsed. Click a header to expand it, or use Enter/Space
when the Remotes panel has keyboard focus. Unavailable machines show a recovery
action even while collapsed: **Sign in…**, **Review update…**, or **Review
connection…**. Update review retains identity verification and installation
consent; older incompatible versions may require manual updates on the machine.

| Machine action | Result |
| --- | --- |
| **New workspace** | Create and open another Workspace on that machine |
| **Update Boomux…** | Open the confirmed remote-update flow |
| Sign-in action | Reauthenticate the selected machine |
| **Forget connection only…** | Remove local registration and cached views; do not contact the machine or stop its work |
| **Remove machine & uninstall Boomux…** | Confirm remote removal, stop its managed processes, and remove the executable and unchanged integrations |

Remote uninstall preserves durable state, configuration, and customizations.
A failed uninstall never silently becomes a local forget. Use **Forget
connection only** if Boomux was already removed or the machine is unreachable.

Removing a Workspace does not uninstall its machine.
See [remote identity and removal guarantees](../docs/remote-nodes.md).

## Settings

Open the **gear** button. Preferences save automatically; settings that require
a service restart produce one reminder after you finish editing.

| Area | Options |
| --- | --- |
| Layout | Tree/Tabs, Workspace/Mixed scope, pane headings |
| Appearance | Rounded/square/mixed corners, pane spacing, focus emphasis |
| Motion | Instant, Fast, or Smooth — the default |
| Clipboard | Copy on select (enabled by default) |
| Projects | Browse for folders and set search depth |
| Notifications | Desktop and sound notifications |
| Advanced | Open service configuration or optional terminal setup |

Motion affects swaps, reflow, minimize/restore, floating transitions, and
Workspace switches. Zero pane spacing removes gaps and canvas insets.

**Settings → Clipboard → Copy on select** copies selected terminal text to the
system clipboard when you release the left mouse button. It defaults to on and
changes take effect immediately. Disable it to copy only with the manual copy
shortcut. Middle-click paste continues to use the primary selection either way.
The preference is saved as `copy_on_select = true` in Desktop's settings file.
Desktop shows a brief **Copied** indicator in the source pane after automatic or
manual selection copying. It does not watch the clipboard or add notifications
for copies performed by a harness. Left-drag selection is currently local to
Desktop; mouse-wheel reporting is separate, so the same selection gesture does
not also invoke a harness's mouse-based copy handler.

### Themes And Saved Preferences

On Omarchy, the active theme updates the interface and terminal palette live.
A missing or invalid theme uses Boomux’s built-in palette. Settings shows which
provider is active.

Desktop preferences live in
`~/.config/boomux-desktop/settings.toml` (`XDG_CONFIG_HOME` is respected).
Service configuration is separate; the Advanced config action opens its
validated editor.

**Internal pane arrangements are saved automatically.** Desktop remembers split
ratios, floating positions/sizes and stacking, focus, expanded panes, minimized
Shells, Workspace ordering, and separate Workspace/Mixed arrangements. Switching
Workspaces restores each saved arrangement.

State lives in `~/.local/state/boomux-desktop/layout-state.json`, respecting
`XDG_STATE_HOME` and the development-only `BOOMUX_STATE_HOME` override. Writes are
atomic and debounced by 250 ms; normal quit and update restart flush the final
snapshot. A crash may lose the most recent unsaved adjustment. Corrupt state is
retained and a notice explains why saving is disabled; close Desktop and move
that file aside to reset layouts. Concurrent Desktop instances cannot overwrite
one another's newer saved state; reopen the older instance if a conflict appears.

Restoration reconnects running Shells without taking over another attachment.
Stopped or unavailable Shells keep a placeholder with an explicit reconnect/start
button. Restoration does not start or restart processes. Floating panes are fit
to the available canvas if its dimensions changed. Outer application window
geometry, terminal selections, and in-progress drag animations are not saved.
See [preferences](../docs/desktop/releases.md#desktop-integration-and-preferences).

### Optional Advanced Setup

**Open advanced setup in terminal** provides the optional checklist:
Up/Down selects, Space toggles, Enter applies, and Escape cancels.

After completion, it asks before removing its dedicated setup Shell. Its
temporary Workspace is removed only if still unused and untouched; reused or
modified Workspaces are kept. Failures remain visible as failures.

Setup does not install the Omarchy plugin or change Hyprland configuration.
See [checklist details](../docs/install.md#harness-checklist).

## Updates

Desktop checks for stable releases after startup and every six hours.
Checks do not download updates or restart anything.
One **Boomux update available** notice covers Desktop and the CLI; dismissing
it hides that release for both. Different detected versions share one version line.

1. Choose **Update** to download and verify the complete bundle.
2. Choose **Restart now** for a graceful handoff, or **Later** to keep working.
3. Use **Check for updates** in the header menu to revisit dismissed notices.

Prepared updates survive app restarts. Compatible handoffs preserve running
Shells. Replacement-window failures restore the old bundle and request service
recovery; errors remain visible.

In-app installation is for eligible official bundles. Source and manually
unpacked builds retain release links. Existing independent CLI installations
keep their own updater.

See [update ownership and older-install migration](../docs/desktop/releases.md#distribution-and-installation).

## Current Limitations

- Desktop is not a compositor and cannot host arbitrary Wayland applications.
- Full Ghostty rendering parity is not implemented. Advanced cursor styles,
  PNG transmission, Unicode image placeholders, and some Kitty graphics cases
  remain incomplete.
- IME, hyperlinks, ligatures, and selection across unloaded scrollback remain
  incomplete.
- Animation curves are not freely configurable.
- Outer application window geometry is not persisted.

Per-pane scrollback uses a 4 MiB Ghostty page-memory budget, allocated as output
arrives. Retained line count varies with terminal width and content. This is
separate from durable Boomux terminal history and is not a total pane-memory cap.

## Run From Source

From the repository root:

```sh
python3 desktop/scripts/run-dev.py
```

The helper builds both binaries and uses an isolated development runtime under
`target/desktop-dev/`. Zig **0.15.2** must be on PATH for the vendored Ghostty
dependency. See [DEVELOPMENT.md](../DEVELOPMENT.md) for all prerequisites.

<details>
<summary>Startup selection and repeatable attachment testing</summary>

Desktop selects the most recently focused local Shell, falling back to the
first available one, then opens its Workspace. Pending Shells start; exited
Shells restart; running Shells are taken over from their current attachment.

Set `BOOMUX_DESKTOP_SHELL_ID=<exact-local-shell-id>` to choose a specific initial
Shell for development tests. Use isolated test resources, not live user work.

</details>

## Architecture And Performance

For other topics, use the [documentation guide](../docs/README.md).

Boomux owns PTYs, persistence, identities, and transport. Desktop owns rendering,
input, layout, and pane resources. Local and remote attachments use Boomux’s
protocol; no external terminal emulator is launched for each tile.

| Module | Responsibility |
| --- | --- |
| `src/layout.rs` | Split tree, spatial focus, and pane geometry |
| `src/terminal.rs` | Attachment adapter and Ghostty terminal worker |
| `src/main.rs` | Application model, input routing, and rendering |

Daemon requests and terminal decoding stay off the GPUI render path.
Queues and caches are bounded, and pane-owned resources are reclaimed on detach.

- [Architecture and ownership](../docs/desktop/architecture.md)
- [Performance measurements and guardrails](../docs/desktop/performance.md)
- [Continuous integration](../docs/desktop/ci.md)

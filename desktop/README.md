# Boomux Desktop

A native terminal workspace with Hyprland-inspired movement and keyboard
controls. Built with [GPUI Community Edition](https://gpui-ce.github.io/) and
Ghostty’s terminal core, backed by persistent Boomux Shells.

[Install](#install-release-builds) · [Workspaces](#workspaces-and-shells) ·
[Controls](#controls) · [Settings](#settings) · [Development](#run-from-source)

> [!NOTE]
> Stable Desktop releases support GNU/Linux x86_64 with glibc 2.39+, X11 or
> Wayland, and a working Vulkan driver. Ubuntu 24.04+ and current Arch are the
> runtime baseline. Shells persist when you close Desktop; pane arrangements
> and window geometry are not yet saved. See [current limitations](#current-limitations).

## Install Release Builds

Install Desktop and its matching Boomux CLI together:

```sh
curl --proto '=https' --tlsv1.2 -LsSf \
  https://github.com/gardnmi/boomux/releases/latest/download/boomux-installer.sh | sh -s -- --desktop
```

**Requires:** GNU/Linux x86_64, glibc 2.39+, X11 or Wayland, and a working Vulkan
driver. Hyprland is not required. See [runtime packages](../README.md#requirements)
and the [installation contract](../docs/install.md).

The installer runs without sudo or a local Rust/Zig toolchain. It adds an
application-menu entry and command links under `~/.local/bin`, preserving any
independent Boomux CLI installation.

Launch **Boomux Desktop** from the application menu, or run:

```sh
boomux-desktop
```

The launcher starts or reuses the Boomux service automatically.

## Workspaces And Shells

| To… | Use… |
| --- | --- |
| Create a Workspace | **+ → New workspace** |
| Open a project | Choose a configured project from **+** |
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

Layout mode shows a badge and dims the panes. Terminal output continues, but
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

| Shortcut | Action |
| --- | --- |
| Left-drag over text | Select visible cells; publish to the primary clipboard |
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

Connect another machine using the general connect action above the machine
cards, or **+ → New remote workspace…**.

Remote Workspaces use a machine icon and show connection status. Their Shells
run on that machine. Connecting creates an initial Workspace and Shell;
open it from the sidebar.

Machine cards start collapsed. Click a header to expand it, or use Enter/Space
when the Remotes panel has keyboard focus.

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
| Projects | Browse for folders and set search depth |
| Notifications | Desktop and sound notifications |
| Advanced | Open service configuration or optional terminal setup |

Motion affects swaps, reflow, minimize/restore, floating transitions, and
Workspace switches. Zero pane spacing removes gaps and canvas insets.

### Themes And Saved Preferences

On Omarchy, the active theme updates the interface and terminal palette live.
A missing or invalid theme uses Boomux’s built-in palette. Settings shows which
provider is active.

Desktop preferences live in
`~/.config/boomux-desktop/settings.toml` (`XDG_CONFIG_HOME` is respected).
Service configuration is separate; the Advanced config action opens its
validated editor.

**Pane arrangements and window geometry are not yet saved.**
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
- Pane arrangements and window geometry are not persisted.

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

# Browser tiling proof of concept

A browser client for the local Boomux daemon, using a binary split tree for
layout, WebGPU for pane surfaces and previews, and the repository-pinned
Ghostty WASM VT engine with Canvas 2D terminal rendering. The daemon owns the
Shell processes; the browser owns its layout and terminal attachments.

From the repository root, install assets and start the gateway:

```sh
bun install --frozen-lockfile
poc/webgpu-tiling/run-daemon.sh --current
```

This connects to an already-running daemon (protocol 54 or newer) without
restarting it. It creates a dedicated **WebGPU playground** Workspace on first
launch and remembers its exact identity under `target/webgpu-poc/`. Set
`POC_WORKSPACE_ID` to select an existing Workspace explicitly. Open
<http://127.0.0.1:4389>; set `POC_PORT` to use another port.

For development without touching your normal daemon, use
`poc/webgpu-tiling/run-daemon.sh --isolated` instead. It starts a separate daemon
with runtime, configuration, and state directories under `target/webgpu-poc/`.
Both modes build the debug gateway and CLI; `CARGO_TARGET_DIR` is supported.
Use Bun 1.3.14 or later for asset installation.

Choose a Workspace in the sidebar to reveal its Shells, then click a Shell to
attach. **New Shell** creates a daemon-owned Bash Shell in the Workspace's default
working directory; its minimal prompt shows the path. **Refresh Shells** updates
the listing. A Shell without a current run must first be started through Boomux.
The pane close button detaches; terminate Shells through the normal Boomux UI or CLI.

Reloading or closing the page keeps Shell processes running. The most recent
Workspace layout is stored in localStorage with exact Node, Shell, and run IDs;
stale runs are never silently replaced. Reattachment restores terminal output.
Compatible graceful daemon restart preserves processes and reconnects attachments;
this does not promise process survival across a machine crash.

Attachments are primary controllers, so browser dimensions resize the actual PTY.
If another terminal controls a Shell, the pane offers **Take control** with an
explicit confirmation. Taking control detaches the previous controller. An already-open browser pane
shows **Take control** as soon as it receives detachment; no page refresh is needed.

The gateway binds only to `127.0.0.1`, validates Host and mutation/WebSocket
Origin, and uses short-lived, one-use attachment grants. It caps active attachments
at 24, pending grants at 64, and concurrent API operations at 8. Transport queues,
frames, and write buffers are bounded; stalled connections detach without killing
Shells. Terminal grids are limited to 500 columns by 200 rows. No terminal output
or attachment environments are saved in browser storage. This is a local-use PoC,
not a remote-access service. It exposes local and registered remote Node Workspaces and Shells. Remote rows
show the owning Node and connection health; cached rows remain visible when stale.
Attachments revalidate the exact ShellRun on the owner, and remote creation uses
the owner's login shell and directory. Discovery failures retain local Workspaces
and show a warning. Full coordinated Workspace placement controls and Desktop
lifecycle controls are not included.

No asset build is required. The footer reports whether WebGPU initialized;
`?fallback` exercises Canvas 2D pane rendering. WebGPU API use is based on the
[official samples](https://webgpu.github.io/webgpu-samples/).

- Drag a heading (or Ctrl-drag a pane) to lift it; other panes reflow.
- Move the pointer over a pane to focus its terminal. Hover focus is suspended
  during layout mode, dragging, resizing, and text selection; a stationary
  pointer does not override keyboard focus when the layout changes.
- Drop near a pane edge to split left, right, above, or below. The preview
  shows the resulting allocation. Dropping outside a target restores the layout.
- Hold Shift when dropping to float. The diamond button toggles floating/tiling.
- Drag a divider to resize; focused dividers also accept arrow keys.
- Double-click a heading or use its expand button to expand/restore.
- Escape cancels a drag/resize or exits expansion in layout mode.
  Outside layout mode, a focused terminal receives Escape normally.
- Add panes (up to 24), detach panes, refresh the Shell listing, or turn motion off.
- Click inside a terminal to type commands. Ctrl+C, ANSI colors, terminal modes,
  selection, paste, and scrollback are handled by Ghostty.

Open the sidebar **Settings** gear and click **Layout mode**, or press **Ctrl+Space** to enable keyboard arrangement.
The visible guide indicates when shortcuts are active:

| Shortcut | Action in layout mode |
| --- | --- |
| Arrows, H/K/L | Focus a neighbor (Down focuses below) |
| Shift+arrows or Shift+H/J/K/L | Swap with a neighboring tile; move floating panes |
| Alt+arrows | Resize by 24 pixels |
| Alt+H/J/K/L / Alt+Shift+H/J/K/L | Resize by 8 / 48 pixels |
| Alt+Shift+arrows | Align a floating pane to the canvas edge |
| Tab / Shift+Tab | Cycle selected pane |
| S or J | Rotate the nearest split |
| E / R | Equalize / swap the nearest split |
| O / F | Toggle floating / expansion |
| Escape | Cancel a gesture, restore expansion, or leave layout mode |

These follow Desktop's basic layout bindings. The prototype uses a toggle only;
Desktop's hold-to-enter and double-tap terminal forwarding are not implemented.
If the OS intercepts Ctrl+Space, use the button. Form inputs retain their keys.

The original standalone PTY experiment is still available:

```sh
bun poc/webgpu-tiling/server.js
```

Open <http://127.0.0.1:4387>. This mode creates private Bun-owned Bash PTYs.
Unlike daemon mode, closing panes, reloading, or **Restart demo** terminates their
ordinary jobs. Its existing lifecycle and cleanup tests apply only to that server.
Commands in both modes execute locally and are not sandboxed.

Floating panes move and resize through keyboard shortcuts but do not yet have
independent pointer resize handles. Desktop's full keyboard layout mode, native
window integration, and production accessibility parity remain unimplemented.
Deep splits can become too small to be useful; production sizing constraints
are future work. The demo does not establish performance parity with GPUI.
The pane compositor renders on changes and during 180 ms reflow animations;
the pinned Ghostty wrapper retains its own per-terminal animation-frame loop.
That idle cost needs measurement and optimization before scaling this design.
GPU storage is fixed-size, and terminal disposal reclaims the emulator and detaches its daemon connection.

Focused model checks:

```sh
node --test poc/webgpu-tiling/layout.test.mjs
```

Browser interaction checks use an existing Playwright installation (for example
the one installed for `website/`). With the server running:

```sh
PLAYWRIGHT_MODULE=/absolute/path/to/playwright/index.mjs \
  HEADED=1 REQUIRE_WEBGPU=1 node poc/webgpu-tiling/browser.test.mjs
```

The fixture launches an isolated Chromium profile with `--enable-unsafe-webgpu`
to exercise GPU support on Linux. It checks both WebGPU and forced Canvas
fallback. Set `CHROMIUM` if Chromium is installed elsewhere. Omit `HEADED` and
`REQUIRE_WEBGPU` for headless interaction checks, where the adapter may be
unavailable. This does not change the user's browser configuration.

Validated locally: four model tests and browser scenarios for moving, canceling,
floating/retiling, double-click expansion, divider resizing, adding/removing,
viewport resize, and keyboard mode, focus, swaps, resizing, floating movement,
expansion, and exit scoping, in both WebGPU and Canvas modes. This is interaction and
rendering-path evidence, not a frame-time or GPU performance benchmark.

Live terminal and cleanup checks (the server must be running):

```sh
PLAYWRIGHT_MODULE=/absolute/path/to/playwright/index.mjs \
  node poc/webgpu-tiling/terminal.test.mjs
```

These exercise keyboard-to-PTY input, ANSI output, process and shell-variable
preservation during movement, `stty size` after resize, layout-key isolation,
Ctrl+C, pane close, repeated restart/page-close cleanup, malformed-input and
output-backlog cleanup, and Origin rejection.
Add `HEADED=1` to exercise a normal Chromium window and `SCREENSHOT=/tmp/poc.png`
to capture the live terminal result. Terminal API behavior follows the pinned
package source; in particular its custom-key handler returns **true** to consume
an event. PTY transport uses [Bun Terminal](https://bun.com/reference/bun/Terminal).

Daemon integration checks require the isolated gateway running on port 4389:

```sh
BOOMUX_RUNTIME_DIR="$PWD/target/webgpu-poc/runtime" \
BOOMUX_CONFIG_HOME="$PWD/target/webgpu-poc/config" \
BOOMUX_STATE_HOME="$PWD/target/webgpu-poc/state" \
POC_BOOMUX_BIN="${CARGO_TARGET_DIR:-$PWD/target}/debug/boomux" \
PLAYWRIGHT_MODULE=/absolute/path/to/playwright/index.mjs \
POC_RESTART=1 node poc/webgpu-tiling/daemon.test.mjs
cargo test --example webgpu_gateway --locked
```

The browser fixture checks process/variable and layout preservation across refresh,
PTY resizing, busy attachments and explicit takeover, detach persistence, rejected
stale identities/Origins, and optionally graceful daemon handoff. It creates and
cleans up only its own Shells and refuses to run against the ordinary runtime.

During pointer resizing, pane geometry and divider hit targets update every frame;
the terminal keeps its current grid until release. Terminal reflow and PTY sizing
commit after layout settles (100 ms debounce), avoiding repeated scrollback reflow
and application redraws while dragging. Escape restores the original split.

Focused resize regression, using synthetic output and intercepted WebSockets so
it never attaches to real Shells (either gateway may serve the page):

```sh
PLAYWRIGHT_MODULE=/absolute/path/to/playwright/index.mjs \
  node poc/webgpu-tiling/resize.test.mjs
```

Local headless Chromium evidence with four terminals, 2,000 long output lines per
terminal, and 40 alternating pointer movements: the previous layout scheduler sent
164 resize messages during the gesture; the updated scheduler sent zero during it
and four on commit. Both runs used the same machine, fixture, and Canvas fallback.
Neither recorded a browser long task; this demonstrates reduced resize work, not
reproduction of every reported freeze or a CPU/memory performance benchmark.

The main canvas uses the full available height. **New Shell** stays beside the
Workspace heading; the bottom sidebar gear contains refresh, layout mode, the
saved animation preference, and renderer details. The Agents/Git/Remotes section
below the Workspace list can be collapsed or resized by dragging its top divider
(or focusing the divider and using the arrow keys).

- **Agents** lists current-run Agents and explicit attention, with Shell navigation;
  historical attention never opens a replacement run. Display is capped at 200 rows.
- **Git** reads the selected Node's daemon Git overview on demand. Expand a worktree
  for its path, staged/modified/untracked counts, upstream divergence, PR summary,
  and associated current Shells. Refresh requests a new observation; an in-progress
  scan is labeled explicitly. One response is retained, with at most 200 worktree rows.
- **Remotes** shows registered remote Nodes, connection health, and route, with
  navigation to a discovered Workspace. Remote registration remains in Desktop.

Activity refresh is explicit; inactive tabs do not poll. The Git endpoint uses the
same bounded operation pool, local Origin validation, and owner-routed host service
as the gateway's other operations, with a two-second response timeout.

Focused activity/settings checks use intercepted fixture data and never attach to
real terminals:

```sh
PLAYWRIGHT_MODULE=/absolute/path/to/playwright/index.mjs \
  node poc/webgpu-tiling/panels.test.mjs
```

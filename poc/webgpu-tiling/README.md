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
restarting it. Workspace conversations and native resume require protocol 55 on the owning Node. It creates a dedicated **WebGPU playground** Workspace on first
launch and remembers its exact identity under `target/webgpu-poc/`. Set
`POC_WORKSPACE_ID` to select an existing Workspace explicitly. Open
<http://127.0.0.1:4389>; set `POC_PORT` to use another port.

For development without touching your normal daemon, use
`poc/webgpu-tiling/run-daemon.sh --isolated` instead. It starts a separate daemon
with runtime, configuration, and state directories under `target/webgpu-poc/`.
Both modes build the debug gateway and CLI; `CARGO_TARGET_DIR` is supported.
Use Bun 1.3.14 or later for asset installation.

Choose a Workspace in the sidebar to reveal its Shells, then click a Shell to
attach. **New Shell** uses the same daemon default-shell specification as Desktop,
in the Workspace's default working directory. Normal startup files initialize
prompts such as Starship. This affects newly created Shells; existing early PoC
Shells retain their recorded minimal Bash command. **Refresh Shells** updates the listing explicitly. A visible browser also follows
bounded daemon event long polls, refreshing metadata without reconnecting terminals.
Select a pending Shell to start it; existing exited runs are never silently restarted.
**−** minimizes/detaches without stopping the Shell. **×** and the sidebar’s
**Remove Shell** action ask for confirmation, then stop processes and remove the
Shell on its owning Node. Failures keep the pane available and report the error.
Removal preflights the displayed run against the live owner and uses a
revision-guarded Shell removal operation.

Reloading or closing the page keeps Shell processes running.
Workspace switching uses Desktop’s 360 ms horizontal slide with quintic ease-out,
following sidebar order. Motion off or reduced-motion preferences switch instantly.
Rapid clicks cancel the prior slide and release its outgoing presentation layer.

Switching Workspaces retains up to two inactive views, with at most 24 terminal
views across the active and retained Workspaces. Returning to a retained view
reuses its canvases, split layout, scroll position, and PTY attachments; it does
not replay history or resize an unchanged grid. Hidden views keep processing
output and takeover events but skip canvas painting. The least recently visited
views are detached and disposed when the limit is exceeded. Minimize explicitly
detaches a Shell, and closing the browser releases all retained attachments.

Up to 16 Workspace layouts (512 KiB total) and 64 minimized Shell references are
stored in localStorage with exact Node, Shell, and run IDs;
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
and show a warning. Local and remote Workspace create/rename/remove, Shell rename/start/remove, and
remote guided workflows are available. Coordinated global placement controls are
not included.

No asset build is required. The footer reports whether WebGPU initialized;
`?fallback` exercises Canvas 2D pane rendering. WebGPU API use is based on the
[official samples](https://webgpu.github.io/webgpu-samples/).

- Drag a heading (or Ctrl + left-drag a pane) to lift it; other panes reflow.
- Ctrl + right-drag anywhere in a pane resizes its nearest tiled splits, or
  the bottom-right corner of a floating pane. Keep Ctrl held to resize another
  pane. Escape restores the original dimensions; ordinary right-click is unchanged.
- Move the pointer over a pane to focus its terminal. Hover focus is suspended
  during layout mode, dragging, resizing, and text selection; a stationary
  pointer does not override keyboard focus when the layout changes.
- Drop near a pane edge to split left, right, above, or below. The preview
  shows the resulting allocation. Dropping outside a target restores the layout.
- Hold Shift when dropping to float. The arrow button toggles floating/tiling.
- Drag a divider to resize; focused dividers also accept arrow keys.
- Double-click a heading or use its expand button to expand/restore.
- Escape cancels a drag/resize or exits expansion in layout mode.
  Outside layout mode, a focused terminal receives Escape normally.
- Add panes (up to 24), detach panes, refresh the Shell listing, or turn motion off.
- Click inside a terminal to type commands. Ctrl+C, ANSI colors, terminal modes,
  selection, paste, and scrollback are handled by Ghostty.

Open the sidebar **⋯** menu and click **Layout mode**, or press **Ctrl+Space** to enable keyboard arrangement.
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

These support Desktop's tap/hold/double-tap leader behavior: tap Ctrl+Space to
toggle, hold at least 250 ms for temporary Layout, or double-tap within 500 ms
without an intervening command to forward Ctrl+Space. Commands also work while
Control remains held.
If the OS intercepts Ctrl+Space, use the button. Form inputs retain their keys.

The original standalone PTY experiment is still available:

```sh
bun poc/webgpu-tiling/server.js
```

Open <http://127.0.0.1:4387>. This mode creates private Bun-owned Bash PTYs.
Unlike daemon mode, closing panes, reloading, or **Restart demo** terminates their
ordinary jobs. Its existing lifecycle and cleanup tests apply only to that server.
Commands in both modes execute locally and are not sandboxed.

Floating and tiled panes have edge/corner pointer resize handles. Native window
integration and production accessibility parity remain unimplemented.
Deep splits can become too small to be useful; production sizing constraints
are future work. The demo does not establish performance parity with GPUI.
The pane compositor renders on changes and during configurable 180/360 ms quintic reflow animations;
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
  navigation to a discovered Workspace. Connect, upgrade, sign-in and uninstall open the same guided CLI workflows as
  Desktop; destructive operations still require confirmation inside that workflow.

Activity updates follow metadata invalidation while the page is visible; inactive
browser pages suspend the event watch. Git scans are explicitly requested, and
terminal output/focus-only events do not trigger snapshot refreshes. The Git endpoint uses the
same bounded operation pool, local Origin validation, and owner-routed host service
as the gateway's other operations, with a two-second response timeout.

Focused activity/settings checks use intercepted fixture data and never attach to
real terminals:

```sh
PLAYWRIGHT_MODULE=/absolute/path/to/playwright/index.mjs \
  node poc/webgpu-tiling/panels.test.mjs
```


## Theme picker

Use **Settings → Appearance → Choose theme** to preview and apply
22 Omarchy themes, or return to the original Boomux palette. Escape cancels a
preview. Left/right arrows move through themes while the picker is open. The
selection persists in this browser and synchronizes across tabs of the same
origin. Theme changes repaint the interface and existing terminals without
reconnecting attachments or clearing scrollback.

Palettes come from the MIT-licensed Omarchy repository; see
[THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md) for the pinned revision and license.
The picker and preview are original code; no Omarchy website implementation or
artwork is bundled.

The pinned Ghostty WASM does not expose runtime palette updates or indexed-color
metadata. Existing terminals translate their startup palette's resolved RGB
colors while painting. Consequently, explicit truecolor values identical to a
startup palette color also change, and duplicate startup colors cannot become
distinct colors later. Other truecolor values are preserved. New terminals use
the selected palette directly. This adapter can be removed when the library
exposes native runtime palette updates.

Focused picker/terminal checks use synthetic output and no real attachments:

```sh
PLAYWRIGHT_MODULE=/absolute/path/to/playwright/index.mjs \
  node poc/webgpu-tiling/themes.test.mjs
```


### Showcase presentation

**Ctrl+Shift+Y** opens/closes the theme options, including when a terminal has
focus. **Enter** applies the previewed theme; **Escape** cancels.
The palette button tooltip shows the shortcut.

The picker always opens as a large three-card showcase carousel, with arrow
buttons, left/right keys, and touch swipes. Click the center preview or
**Apply theme** to select it. The old list view and view toggle are removed;
previously saved view preferences are ignored.
Open `http://127.0.0.1:4390/?theme-showcase` to go straight to the showcase for
recording. Previews contain synthetic Boomux content, not copies of live Shells.

Selection opens the new theme through a 360 ms diagonal center-out wipe, inspired
by the supplied Omarchy site recording and implemented independently with the
browser View Transition API. The terminal canvases and pane compositor repaint
before the new snapshot. Only one transition can run at a time; the browser
releases its snapshots when it ends. Reduced motion, disabled layout animation,
and browsers without this API apply the theme immediately. No website source or
preview artwork is included.

```sh
PLAYWRIGHT_MODULE=/absolute/path/to/playwright/index.mjs \
  node poc/webgpu-tiling/theme-showcase.test.mjs
```

### Minimize panes

The header’s **−** button removes a pane from the canvas. A daemon-backed pane
releases its attachment and leaves its Shell running; reopen the same Shell from
the sidebar using normal attachment/takeover rules. In standalone demo mode,
minimized panes retain their PTY and emulator, and their sidebar rows restore them.
Layout keyboard navigation skips minimized panes.

The synthetic `minimize.test.mjs` check covers both paths and minimizing all panes.

Terminal rendering uses Desktop’s 13px text and 8.4 × 17px cell geometry. The
browser loads a bundled JetBrainsMono Nerd Font, independently of local font
installation. See THIRD_PARTY_NOTICES.md and fonts/OFL.txt. Cell backgrounds cover whole physical pixels to avoid
seams between Starship/Powerline segments at fractional display scaling.
`terminal-background.test.mjs` checks solid ANSI backgrounds at DPR 1, 1.25, and 2;
`POC_DPR=1.25` runs the resize fixture with fractional scaling.


## Desktop interaction parity

Settings now include Tree/Tabs layout, Workspace/Mixed scope (Tabs uses Workspace
scope), headings, Square/Rounded/Mixed edges, pane spacing, focus width, motion
Instant/Fast/Smooth, copy-on-select, and removal confirmation. Preferences are
browser-local. The sidebar is resizable and can be toggled with its menu button
or Layout B. Workspace headings support drag reordering and independent expansion.
Resource edits and removals use in-app dialogs. Pane rename is available through
the pencil, sidebar menu, or F2. Workspace creation/removal/rename is available
through Settings/sidebar menus, including creation on a registered remote Node.

Additional keys:

| Key | Action |
| --- | --- |
| F1 / F2 / F6 | Help / rename / sidebar focus |
| Ctrl+Enter | Create a Shell in the focused Workspace |
| Ctrl+W / Ctrl+Shift+W | Minimize / remove, when delivered by the browser |
| Layout N / M / X | Browser-safe new / minimize / remove alternatives |
| Layout PageUp / PageDown | Previous / next Workspace |
| Layout B / G / C | Sidebar / Git / center floating pane |
| Ctrl+Shift+C/V; Ctrl+Insert / Shift+Insert | Copy / paste |
| Sidebar arrows or HJKL, Home/End | Navigate resources |
| Sidebar Enter / Space / Escape | Open / expand / return to terminal |
| Sidebar Tab / Shift+Tab | Activity / settings section |

Git PR URLs open in a separate browser tab. Agents with attention offer a
revision-checked acknowledgment. Remote actions and the configuration/integration
setup buttons create a dedicated local Setup Workspace with an allowlisted exact
CLI argument vector. No shell interpolation is used. The returned pending Shell
is started explicitly. Setup Workspaces remain available for inspection/removal;
the browser does not infer Desktop's ephemeral automatic-cleanup ownership.

Validation added here includes `desktop-parity.test.mjs`, `desktop-state.test.mjs`,
and `resource-actions.test.mjs`. The last requires the isolated daemon environment
and POC_BOOMUX_BIN, creates only its own fixtures, and removes them in finally.
It does not execute remote install/update/uninstall workflows. Gateway unit tests
check the allowlisted guided commands and exact owner arguments.

Remaining differences: native OS window/theme integration; browser-reserved keys;
Desktop's complete terminal image pipeline and native shaping/selection behavior;
exact Git/Agent visual layout and global placement controls; native accessibility;
Desktop's temporary setup cleanup ownership. Retaining recent browser Workspace
views and debouncing PTY resize are intentional safeguards against replay flashes
and repeated expensive reflow. These are not claims of native performance parity.


### Workspace reorder feedback

Drag a Workspace heading at least six pixels to lift its card. The list previews
its new order immediately, moving neighboring groups with Desktop's configured
quintic easing. A dashed “Drop here” slot stays at the destination. Expanded Shell
rows move with their Workspace. Drag near the list's top or bottom to auto-scroll.
Release inside the list to save; Escape, pointer cancellation, window blur, or a
release outside the list restores the previous order. A normal click still opens
the Workspace. Alt+Up/Down on a focused heading reorders with keyboard feedback.
Reduced motion keeps the card and destination feedback but skips row animation.

The drag moves existing sidebar nodes only. Metadata refresh is deferred until
the drag ends; terminal canvases and attachments are unaffected. The focused
`workspace-reorder.test.mjs` fixture covers preview before drop, persistence,
cancellation, auto-scroll, reduced motion, keyboard movement, and terminal identity.

### Desktop parity update (September 12, 2026)

The PoC is based on Desktop/main `6becfbe`. The header's conversations button
opens a 400px right drawer without resizing the terminal layout. It lists the
selected Workspace's recorded harness conversations, supports search, Recent and
Archived views, pinning, and native Open/Resume. Requests route to the exact owner;
a delayed resume never opens a pane in a different selected Workspace. Retry IDs
remain stable after a failed attempt. Pin/archive preferences are browser-local,
bounded to 4,096 entries, and scoped by Node, Workspace, harness, and session.
They do not delete harness history or synchronize Desktop preferences.

The + menu searches configured projects by name/path and creates a uniquely named
Workspace while retaining previous layouts. Its remote picker lists registered
Nodes and offers connection recovery. Remotes now exposes registration-guarded
connection renaming, version/observation information, update/sign-in recovery, and
a separate confirmed “Forget connection only” action. Forgetting removes local
registration/cache; it does not uninstall or stop work on the remote machine.

Settings includes Desktop's button-hover animation toggle, with reduced-motion
support. Modal input takes priority over terminal/panel shortcuts; PageUp/PageDown
from the sidebar switches Workspaces. The narrow header retains all four controls
without overlapping the brand. Selection edge-autoscroll and foreground-only
faint rendering already exist in the pinned Ghostty web renderer.

Remote maintenance runs the matching guided CLI in a web terminal. Connection
setup uses Desktop's private one-shot result channel to identify the created
remote Shell. After the CLI's acknowledgement removes its exact setup Shell,
the gateway closes the temporary Workspace only at the creation revision plus
that single removal, with no remaining Shells, launchers, or Agent history.
The browser opens the result only if the setup Workspace is still selected.
Launch records are capped at eight; expired records are reclaimed on subsequent
setup requests, and dropping the gateway releases their private sockets. Browser
reserved shortcuts, native window management, OS theme/background integration,
and independently stored presentation preferences remain platform differences.

Focused coverage: `conversations.test.mjs`, `panels.test.mjs`,
`sidebar-resize.test.mjs`, `guided-setup.test.mjs`, and
`cargo test --example webgpu_gateway --locked`.
The conversation fixture covers search/pin/archive, idempotent retry, late replies
across Workspace switches, project creation, and remote action cancellation.

Remote sign-in recovery follows Desktop's September 13 update: Tailscale browser
authentication challenges remain “Sign-in required” after an SSH deadline.
“Sign in…” stays at the top of both collapsed and expanded remote cards and
launches the matching guided CLI against that registered Node's exact identity.
The owning daemon must include the authentication classification fix to publish
that status; the gateway does not infer it from terminal text.
`remote-reauth.test.mjs` covers both entry points and returning from setup.

## Share the tiling UI from Desktop

Build Desktop, the matching CLI, and the web gateway in the same target directory:

```console
bun install --frozen-lockfile
cargo build --locked --bin boomux --example webgpu_gateway
cargo build --locked -p boomux-desktop
```

Use Desktop's **⋯ → Open WebUI** action to start private HTTPS
sharing. The browser opens when ready; the sidebar footer offers **Open WebUI**,
**Copy web UI URL**, and **Stop sharing web UI**. Sharing lasts while Desktop
is open. Stopping sharing disconnects browser attachments but keeps managed
Shells running. Publishing itself creates no Shells or Workspaces.

Tailscale must be connected, with MagicDNS/HTTPS enabled and permission to use
Serve. Access follows your tailnet access policy. The gateway binds only to
`127.0.0.1:4391`; Serve terminates HTTPS on the machine's tailnet name. It tries
HTTPS ports 443, 8443, then 10000 and includes the chosen port in the URL when
needed. Existing handlers and public Funnel listeners are skipped; if all three
ports are occupied, sharing reports an error without replacing any service. Cleanup
removes only routes created by this publisher.

Desktop uses the tiling gateway rather than `boomux web`'s older mobile dashboard.
Release bundles include `webgpu_gateway` beside the Desktop executable, with web
assets under `share/boomux/webui` on Linux or `Contents/Resources/webui` on macOS.
Installed gateways resolve assets relative to their executable and never fall back
to the build machine's checkout. Development gateways in `target/.../examples`
continue to read the source checkout. Guided setup resolves the matching bundled
CLI. Packaging checks every runtime asset and runs `--check-assets` before archiving.

Validate relocation without a daemon or Tailscale mutation:

```console
BOOMUX_WEB_GATEWAY=target/debug/examples/webgpu_gateway python3 desktop/scripts/test-webui-bundle.py
```

For standalone publishing, run the gateway with `--tailscale`. `--desktop`
selects existing Workspaces and ties sharing to stdin lifetime; the Desktop
button supplies it automatically. SIGINT/SIGTERM also clean up owned routes.

Focused mocked-publisher validation (requires an existing compatible daemon):

```console
BOOMUX_WEB_GATEWAY="${CARGO_TARGET_DIR:-target}/debug/examples/webgpu_gateway" python3 poc/webgpu-tiling/tailscale-share.test.py
```

The test replaces Tailscale with a fixture; it never publishes a real route.

Web buttons, tabs, menus, and clickable activity rows use Desktop's slanted
accent hover sweep. Fast motion takes 180 ms and Smooth is capped at 200 ms;
disabling button hover animation, choosing Instant motion, or requesting reduced
motion keeps the feedback immediate. The overlay does not intercept input or
change control dimensions.

The web sidebar uses Desktop's semantic hover and selected-Shell colors,
64px header, 12px inset, 3px control corners, and full-row hover sweeps.
Theme palette previews remain untinted. The expansion affordance is revealed
beside the Workspace overflow control on hover/focus, preserving compact titles.

Open **Settings → Choose theme** to use the showcase picker, or press
**Ctrl+Shift+Y** from anywhere. Closing the picker returns focus to Settings.
The sidebar footer palette icon, activity refresh button, and collapse arrow
are removed; activity tabs and the resize separator remain available.

Desktop shows **Open WebUI** in its three-dot menu while stopped. It starts sharing
and opens the browser; while running, the sidebar footer shows **Open WebUI**, **Copy URL**,
**Stop sharing**, and the Desktop lifetime note. Setup failures include links to
[Tailscale installation](https://tailscale.com/download) and its
[private HTTPS setup guide](https://tailscale.com/docs/features/tailscale-serve).
The sharing footer is hidden when WebUI is stopped.

The web Settings sheet follows Desktop’s grouped sidebar layout, switches, segmented choices, and spacing/focus steppers. Browser appearance preferences save automatically in this browser; shared notification, recovery, and project configuration opens Boomux’s validated configuration editor. These shared fields are not yet editable inline in the web Settings sheet.

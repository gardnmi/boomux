# Browser tiling proof of concept

A standalone movement experiment for Boomux Desktop. It uses the same basic
binary split-tree approach as `desktop/src/layout.rs`, with browser pointer
events and an event-driven WebGPU pane compositor. Each pane runs a real local
Bash PTY through a loopback WebSocket bridge. The repository-pinned
`ghostty-web` uses Ghostty's WASM VT engine with a Canvas 2D terminal renderer;
WebGPU draws the surrounding pane surfaces and previews. It does not connect to
the Boomux daemon or attach to existing Boomux Shells.

From the repository root:

```sh
bun install --frozen-lockfile
bun poc/webgpu-tiling/server.js
```

Use Bun 1.3.14 or later on Linux/macOS. Open <http://127.0.0.1:4387>.
No asset build is required: the server exposes only an explicit list of PoC files,
the installed pinned Ghostty JS/WASM, and the installed terminal font.
The footer reports whether WebGPU initialized. If unavailable, the same demo
uses Canvas 2D; `?fallback` explicitly exercises that path. WebGPU API use is
based on the [official samples](https://webgpu.github.io/webgpu-samples/).

- Drag a heading (or Ctrl-drag a pane) to lift it; other panes reflow.
- Drop near a pane edge to split left, right, above, or below. The preview
  shows the resulting allocation. Dropping outside a target restores the layout.
- Hold Shift when dropping to float. The diamond button toggles floating/tiling.
- Drag a divider to resize; focused dividers also accept arrow keys.
- Double-click a heading or use its expand button to expand/restore.
- Escape cancels a drag/resize or exits expansion in layout mode.
  Outside layout mode, a focused terminal receives Escape normally.
- Add panes (up to 24), close terminal sessions, restart the demo, or turn motion off.
- Click inside a terminal to type commands. Ctrl+C, ANSI colors, terminal modes,
  selection, paste, and scrollback are handled by Ghostty.

Click **Layout mode** or press **Ctrl+Space** to enable keyboard arrangement.
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

The prototype intentionally keeps everything in memory. Closing a pane, reloading,
closing the page, or **Restart demo** closes the associated PTYs and terminates
their ordinary shell jobs. Moving, resizing, floating, or expanding keeps the
same session. This lifetime is deliberately different from Boomux Desktop's
persistent daemon-owned Shells. Explicitly detached jobs are not supervised.
Bash starts without startup files or inherited Boomux/agent identity variables;
the working directory is this worktree. These are real local commands, not a sandbox.

The bridge binds only to `127.0.0.1` and validates both Host and WebSocket Origin.
It caps total sessions at 48, unacknowledged output at 1 MiB per session, input at
64 KiB per message/second, and the grid at 500 columns by 200 rows. A stalled or
overloaded connection closes its session rather than dropping arbitrary terminal
bytes. Output acknowledgments happen after synchronous WASM ingestion, so they
do not depend on animation frames in background tabs. Scrollback is capped at
2,000 lines per pane. No session credentials or attachment environments are stored.
The gateway is an experiment for local use, not a remote-access service.

Floating panes move and resize through keyboard shortcuts but do not yet have
independent pointer resize handles. Desktop's full keyboard layout mode, native
window integration, and production accessibility parity remain unimplemented.
Deep splits can become too small to be useful; production sizing constraints
are future work. The demo does not establish performance parity with GPUI.
The pane compositor renders on changes and during 180 ms reflow animations;
the pinned Ghostty wrapper retains its own per-terminal animation-frame loop.
That idle cost needs measurement and optimization before scaling this design.
GPU storage is fixed-size, and terminal disposal reclaims the emulator and PTY.

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

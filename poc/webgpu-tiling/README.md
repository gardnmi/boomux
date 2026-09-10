# Browser tiling proof of concept

A standalone movement experiment for Boomux Desktop. It uses the same basic
binary split-tree approach as `desktop/src/layout.rs`, with browser pointer
events and an event-driven WebGPU pane compositor. Text and controls are DOM
elements. It does not connect to a daemon; all terminal output is simulated.

From the repository root:

```sh
python3 -m http.server 4387 --bind 127.0.0.1 --directory poc/webgpu-tiling
```

Open <http://127.0.0.1:4387>. No build or dependency installation is required.
The footer reports whether WebGPU initialized. If unavailable, the same demo
uses Canvas 2D; `?fallback` explicitly exercises that path. WebGPU API use is
based on the [official samples](https://webgpu.github.io/webgpu-samples/).

- Drag a heading (or Ctrl-drag a pane) to lift it; other panes reflow.
- Drop near a pane edge to split left, right, above, or below. The preview
  shows the resulting allocation. Dropping outside a target restores the layout.
- Hold Shift when dropping to float. The diamond button toggles floating/tiling.
- Drag a divider to resize; focused dividers also accept arrow keys.
- Double-click a heading or use its expand button to expand/restore.
- Escape cancels a drag/resize or exits expansion.
- Add panes (up to 24), remove them, reset, or turn motion off.

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

The prototype intentionally keeps everything in memory. Reload resets the
layout. Floating panes move but do not yet have independent resize handles.
It does not implement terminal input, Desktop's full keyboard layout mode,
native window integration, or production accessibility parity. Deep splits can
become too small to be useful; production sizing constraints are future work.
The demo does not establish performance parity with GPUI. Idle rendering stops;
frames are requested on interaction, resize, or during a 180 ms reflow animation.
GPU storage is fixed-size and pane count is bounded.

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

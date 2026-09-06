> Historical snapshot from the standalone Desktop repository at
> `95946c14413eeac039e619c96368577463c59a53`. Its “Unreleased” section belongs to that
> repository. New changes use the [shared Boomux changelog](../../CHANGELOG.md).

# Changelog

All notable changes to Boomux Desktop will be documented in this file.

## Unreleased

- Fix prematurely truncated terminal scrollback by supplying a byte budget to
  the pinned Ghostty implementation instead of treating its limit as a row count.

- Put spatial focus, swapping, resizing, alignment, pane cycling, Workspace
  cycling, floating, maximize, and drawer commands behind a visibly indicated
  `Ctrl+Space` Layout mode so ordinary terminal control chords reach the PTY.
- Keep working-to-idle Agents visibly marked as finished until dismissed, and
  acknowledge durable Boomux attention by exact observation revision.
- Preserve modified special keys and key actions through Ghostty's
  terminal-mode-aware keyboard encoder, with a portable `Shift+Enter` newline
  fallback for reattached TUIs whose Kitty keyboard mode cannot be reconstructed.
- Follow Omarchy's active semantic and terminal palette with event-driven live
  reload, bounded theme reads, and a built-in fallback for other systems.
- Forward scroll-wheel events to full-screen terminal applications when they
  enable mouse tracking, while preserving ordinary terminal scrollback.
- Make terminal updates event-driven and coalesced, share immutable screen
  snapshots, cache shaped paint data across layout-only frames, inline common
  cell text, and reuse unchanged Kitty image pixels to reduce idle and animation
  hot-path work.
- Slide outgoing and incoming pane sets between Workspaces using the selected
  Fast or Smooth motion speed while keeping Instant switches immediate.
- Add `Ctrl+PageUp` and `Ctrl+PageDown` shortcuts to cycle through Workspaces in
  sidebar order with wrapping.
- Pin the Boomux client to v1.9.5 and document v1.9.3 as the minimum daemon
  version with the primary-output backpressure required for reliable Kitty
  graphics.
- Establish the standalone Boomux Desktop project from the GPUI tiling-terminal
  proof of concept.
- Add Workspace and Shell action menus, background lifecycle mutations, rename
  dialogs, and confirmed permanent removal without changing `Ctrl+W`'s
  detach-only behavior.
- Replace pane dimension labels with minimize and close controls. Minimize
  detaches while preserving the Boomux Shell; close opens the existing confirmed
  permanent-removal flow.
- Add a pane-header maximize/restore control using the same animated,
  workspace-scoped fullscreen behavior as Layout mode.
- Make pane headings direct drag handles and add a pane-header control for
  animated floating and docking.
- Hide the routine attached label, add rename controls to pane headings and
  minimized tabs, and add explicit overflow navigation to the minimized-tab
  strip.
- Keep successful pane headings free of non-actionable attachment environment
  diagnostics while continuing to surface terminal failures.
- Present Settings as a scrollable sidebar-native screen with consistent
  segmented and stepper controls, and add an enabled-by-default option to
  bypass confirmation prompts for permanent Workspace and Shell removal.
- Remove the shortcut footer and give its height back to the terminal canvas.
- Append newly created Workspaces, preserve presentation order across overview
  refreshes, and support bidirectional row dragging with animated reflow plus
  `Ctrl+Shift+Up/Down` reordering.
- Animate pane minimization and restoration using the selected Fast or Smooth
  motion duration while keeping Instant transitions immediate.
- Add a keyboard-first, scrollable shortcut help menu opened with `F1` or the
  sidebar header's overflow menu.
- Make `Ctrl+Enter` create a Shell in the selected sidebar row's exact
  Workspace while preserving click-to-collapse behavior.
- Make the sidebar's focus-following Workspace and Shell treatment more subtle
  while preserving its stronger keyboard-navigation highlight.
- Include the focused Workspace in the application title and use Boomux-style
  random names when creating Workspaces and Shells.
- Add `Ctrl+Shift+Arrow` aliases for Hyprland-style directional pane swaps and
  floating-pane movement.
- Animate keyboard-driven tiled pane swaps between their old and new positions.
- Add a collapsible sidebar drawer and appearance controls for pane headings and
  rounded or square pane edges.
- Add a playful Mixed edge style with stable per-pane variations in square and
  curved corners.
- Add bounded appearance controls for tiled-pane spacing and focused-pane
  highlight strength.
- Apply window spacing to the workspace's outer inset as well as gaps between
  tiled panes, making 0px genuinely edge-to-edge.
- Change the floating toggle to `Ctrl+O`, enlarge and center newly floating
  panes with an eased transition, and add Instant, Fast, and Smooth motion
  choices for pane reflow and floating transitions, with Smooth as the default.
- Fix terminal scrollbar dragging with explicit press, pointer-delta, and
  release tracking that does not depend on drag-and-drop promotion.
- Coalesce rapid scrollbar seeks to the latest requested row so dragging does
  not pause behind obsolete emulator commands and then catch up.
- Fade terminal scrollbars in on hover and out after exit, keep them visible
  while dragging, and use the normal arrow cursor over their hit area.
- Keep the sidebar and application controls visible while `Ctrl+F` smoothly
  maximizes a pane within the terminal workspace, and remove the redundant
  in-app window title. Keep the restoring pane above its siblings so panes on
  every side animate back symmetrically instead of being visually clipped.
- Remove the remaining static proof-of-concept toolbar, move appearance and
  help controls into the Boomux sidebar, and reclaim its height for terminals.
- Keep keyboard focus in the sidebar when a stationary pointer produces a pane
  hover callback, while preserving focus-follow-mouse after real movement.
- Add Workspace and Mixed pane-scope modes, default to opening all Shells from
  one Workspace, and expose an explicit Open workspace sidebar action.
- Distinguish focused, open, and minimized panes in Shell sidebar rows without
  conflating pane closure with Boomux Shell process state.
- Open a minimized Shell by itself, while opening the Workspace's remembered
  pane set when its row is clicked in single-Workspace mode or activated with
  Enter.
- Preserve each Shell's minimized state while switching between Workspaces
  during the desktop session.
- Collapse other sidebar Workspaces when one is opened or created in
  single-Workspace mode, while preserving independent expansion in Mixed mode.
- Keep the focused pane painted above its siblings throughout slide-swap and
  re-tiling animations.
- Add Hyprland-inspired split rotation, equalization, branch swapping,
  edge-aware neighbor selection, three keyboard resize scales, floating-pane
  edge alignment and centering, focus raising, and forward/backward pane
  cycling.
- Keep the sidebar header compact with a visible new-Workspace button and an
  overflow menu for Settings, Keyboard Shortcuts, and Hide Sidebar.
- Make every keyboard resize scale position-aware by moving the closest
  divider on the requested axis, allowing tiled panes to grow or shrink from
  either side; restore conventional directional resizing for floating panes.
- Add an opt-in, Workspace-only Tabs layout that keeps open windows tiled or
  floating and moves only `Ctrl+W`-minimized Shells into a restorable top strip.
  Show only Workspace rows in the sidebar in Tabs mode, release minimized
  panes' emulator and GPU state, and visibly disable incompatible Mixed scope.
- Give the sidebar overflow menu enough width and a fixed shortcut column so
  labels never collide with their keyboard hints.

# Native remote Node implementation

## Development checkpoint

The first implementation exposes a compact Nodes popover, observed health and
resource counts, and entry points to the matching CLI's guided Add Node and
reauthentication flows. It uses one combined snapshot in the existing background
overview worker and retains local-only discovery when federation is unavailable.
Remote Shells are still accessed through the existing Boomux dashboard.

Validation of this checkpoint: formatting, Desktop Clippy with warnings denied,
122 Desktop tests, and an optimized Desktop build passed. Isolated GUI interaction
checks verified that Ctrl+A in the popover does not launch setup, Escape and
outside clicks return input to the existing terminal, and Add Node starts the
guided SSH prompt without replacing the original ShellRun. No remote host was
contacted by the GUI fixture.

## Open repaint finding before PR

In an isolated Weston kiosk compositor nested inside Xvfb with software rendering,
the new setup Shell accepts input and exposes its prompt through `boomux read`,
but Desktop can retain the initial “Opening terminal…” frame until a pointer
click. The sidebar can likewise retain an older resource count until interaction.
After clicking, the prompt and current count render correctly. The cause and
behavior on a physical Wayland display remain unconfirmed.

Reproduce with the official v1.10.0 CLI and the checkpoint Desktop binary in
private XDG directories: open Nodes, choose Add remote Node, then wait without
moving the pointer. Compare the visible frame with the guided Shell's retained
output. Click the terminal to check whether the frame advances. Test both the
keyboard shortcut and mouse activation before accepting a fix.

Changing the affected tasks to `spawn_in`/`update_in` and calling
`window.refresh()` on completion did not resolve this reproduction; that
attempt was removed. Do not mask the failure with periodic synthetic input,
forced resizing, or an unconditional animation loop. Resolve or isolate this
finding before treating the native entry point as ready to merge.

## Remaining stages

1. Convert Desktop Shell/Agent selection, pane lookup, focus, attention, and
   actions to exact Node-qualified identities. Retain coordinator Workspace
   membership when combining placements; equal labels never merge resources.
2. Open existing remote Shells through `Client::attach_node`, preserving exact
   ShellRun binding and the owner's environment. Show remote host labels and
   disable unavailable actions with their specific health reason.
3. Preserve panes across transport loss, with bounded exact-run reconnection,
   explicit authentication/identity recovery, and no replay of uncertain input.
4. Add native placement and remote-directory selection, followed by a richer
   GUI authentication adapter if the guided terminal flow proves insufficient.

Backend authority remains governed by `CONTEXT.md` and `docs/remote-nodes.md`.

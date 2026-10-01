# Native macOS interactions: implementation and validation

This work depends on the native composed-input adapter. macOS remains an
experimental, largely untested preview. The cases below have **not** been run on
a real Mac by this change; passing model tests or type-checking does not establish
VoiceOver usability or native menu/clipboard reliability.

## Menus and actions

The App, Edit, View and Window menus use GPUI actions. App actions include
Settings (Command-comma), Hide (Command-H), Hide Others (Command-Option-H), Show
All and Quit (Command-Q). Window actions include Minimize (Command-M) and Zoom;
Control-Command-F toggles the native window fullscreen state. This is distinct
from maximizing a terminal pane. Existing Dock reopen, red-window-close behavior
and daemon/Shell lifetime are unchanged.

Edit uses the system Cut/Copy/Paste/Select All selectors so native file-dialog
text fields can retain their own responders. Within Boomux, the menu and Command
shortcuts use the same recipient-aware edit methods. Terminal Cut is unavailable;
it never deletes terminal output or synthesizes a shell command. Terminal Select
All selects the retained terminal history without copying it until Copy is chosen.
Modal and noneditable surfaces do not fall through to terminal paste.

The fixed shortcuts contain Command, leaving terminal Control-C and other control
chords alone. Boomux does not currently expose configurable shortcut bindings;
the native bindings are a single, tested catalog rather than a second user keymap.

## Clipboard ownership and bounds

An asynchronous terminal copy retains a request serial, pane ID, attachment
generation, focus generation and selection. A later edit, selection change,
recipient/focus change, reattachment or pane removal rejects its completion.
Formatting remains on the existing terminal worker with a 4 MiB output limit.

Mac copy and paste use nonblocking submission to that worker. Pointer
copy-on-select retains its existing source-gesture ownership rather than the
menu/shortcut focus ticket, while using the same nonblocking selection sender. Paste is limited to
4 MiB and at most one outstanding clipboard paste per terminal. A second paste
while the first is pending reports a busy message instead of silently replacing
it or growing a queue. Queue rejection releases the pending slot for retry. Paste
encoding reads bracketed-paste mode on the worker, in order with terminal output.
The encoded byte stream is split into protocol frames of at most 1 MiB while
keeping one bracketed-paste envelope and one worker-side attachment writer lock.
Accepted input remains addressed to its original terminal even if focus later
moves; it is not redirected to the newly focused pane.

GPUI's clipboard read may allocate the original platform text before Boomux can
inspect its size. The limit bounds subsequent encoding and retained input; it is
not a claim that AppKit never materializes a larger clipboard item. Images are
not imported. Linux retains its existing primary-selection and clipboard paths.

## Accessibility boundary

On macOS, the focused pane is reported as the active descendant of the workspace.
Terminal output has a Terminal role, a bounded Shell label, a focus action, text
runs and a cursor/selection projection. Pane toolbar icons have explicit action
labels; the destructive icon is labeled Remove Shell. Active native input fields
expose their text and selection separately from terminal output.

Terminal accessibility reads only the current immutable screen snapshot. It is
created on demand when GPUI requests an accessibility tree, limited to 32 KiB of
text, 128 rows and 512 columns per visible pane. Formatting is cached until the
screen changes; the cache holds a weak screen reference so it does not retain old
terminal images or scrollback. Closing the pane releases its cache. AccessKit
still owns bounded copies of text-node data when it builds a tree; this is not a
claim of zero allocation per accessible frame.

Character metadata includes Unicode scalar byte lengths, cell-based positions and
wide/continuation-cell handling. Selection direction is retained, ranges are
clipped to the accessible viewport, and an entirely offscreen selection is not
misreported as a visible caret. Truncation is described in the accessible node.
The surface is read-only output; arbitrary accessibility text replacement is not
an interface for writing into the PTY.

This is a bounded accessibility foundation. Full scrollback browsing, image
alternatives, faithful soft-wrapped-line semantics, extended-grapheme navigation
and complete VoiceOver editing/navigation remain unverified or out of scope.

## Focused checks

Portable clipboard ownership and text projection tests:

```sh
rustc --edition 2024 --test desktop/src/clipboard_routing.rs -o /tmp/boomux-clipboard-tests
/tmp/boomux-clipboard-tests
rustc --edition 2024 --test desktop/src/terminal_accessibility.rs -o /tmp/boomux-accessibility-tests
/tmp/boomux-accessibility-tests
```

With ordinary Desktop dependencies installed, run focused tests:

```sh
cargo check -p boomux-desktop --tests --locked
cargo test -p boomux-desktop clipboard --locked
cargo test -p boomux-desktop terminal_accessibility --locked
cargo test -p boomux-desktop macos_accessibility --locked
cargo test -p boomux-desktop macos_menus --locked
```

The test target type-checks the platform-independent GPUI menu/accessibility
adapter on Linux too. The normal Linux/macOS CI matrix remains required.

## Native acceptance checklist (not yet executed)

Use disposable terminals and record commit, macOS version, hardware, input source
and assistive-technology version for each result.

- Menu click versus Command-X/C/V/A in terminals, every search field, rename and
  settings; empty selections; disabled items; native file-dialog text fields
- Copy from offscreen history, switch panes or attachments before completion,
  close the pane, and repeat copy/paste; no late clipboard overwrite or PTY paste
  into a different recipient
- Small, empty, 4 MiB and oversized clipboard text; slow/full worker queue;
  repeated paste while pending; retry after rejection; bracketed-paste encoding
- Compose text, then use each edit action; no stale IME commit; rename/removal
  dialogs must retain keyboard ownership over panels underneath
- Command-H, Command-Option-H, Command-M, Command-comma, Command-Q, native
  fullscreen, red close and Dock reopen; persistent Shells survive GUI closure
- VoiceOver discovers the focused pane and text field without announcing every
  output frame as a live alert; moving focus via accessibility selects the same
  pane as keyboard/pointer navigation and does not bypass a modal
- Unicode, emoji, wide cells, combining marks, reversed selections, hidden cursor,
  offscreen selection, truncated content, resize and theme transitions: text
  indices and highlighted bounds remain valid
- Linux X11/Wayland control/Alt/Kitty input, layout leader, clipboard, primary
  selection, pane layout and rendering remain unchanged

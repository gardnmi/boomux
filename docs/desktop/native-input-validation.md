# Native macOS text input validation

macOS remains a largely untested experimental preview. Implementing the AppKit
input contract and passing model tests do **not** establish native keyboard,
IME, candidate-window, accessibility, or everyday-use reliability. No native
acceptance run is recorded by this change.

## Implementation boundaries

The macOS-only adapter installs an `EntityInputHandler` for the current terminal,
project/Git/conversation search, rename dialog, or shared-settings field. Marked
text stays in one window-owned, bounded buffer; neither terminal input nor search
filtering/resource changes see it before commit. UTF-16 selections and replacement
ranges are converted without slicing a UTF-8 scalar or surrogate pair. The painted
text provides candidate geometry, selection highlighting and a preedit underline.
Settings fields show at most six lines, following the caret; editing moves over
Unicode scalars, not extended grapheme clusters.

Each painted handler is tied to its recipient and generation. Changing panes,
attachments, modal recipients, or focus invalidates old handlers. Cancellation
restores the committed field value and never sends preedit to a shell. A late
commit after cancellation is ignored, and the adapter explicitly tells
`NSTextInputContext` to discard its marked text. This call runs after releasing the
Workspace borrow because AppKit can call back synchronously; input stays disabled
until it finishes. Verify input-source switching and the system emoji picker after
cancellation on a real Mac.

All printable terminal keys are offered to AppKit, including unmodified dead
keys on international layouts. One pending plain key preserves the existing
Ghostty press/repeat/release path only when its synchronous native commit is
unchanged and unmarked. Marking, replacement, key-up or recipient cancellation
clears that metadata; Option text never inherits a terminal Alt modifier.
Committed native text is one bounded command on
the same terminal worker queue and uses Ghostty's unidentified-text encoding;
it is never bracketed paste and does not invent a physical press/release pair.
Ghostty's pinned implementation preserves pure text in Kitty report-all modes.
Terminal replacement ranges describe only the in-flight preedit; the adapter does
not pretend that previously committed PTY input is an editable document. Terminal
press-and-hold is disabled, while editable fields retain the system accent picker.

Settings → Keyboard → **Option as Alt** defaults to off (`macos_option_as_alt =
false`). Off lets Option produce keyboard-layout characters and dead keys. On
routes Option's base key to terminal Alt shortcuts. Both Option keys use the
same policy because GPUI's keystroke does not identify the modifier side. Editable
fields always use native Option text. Linux does not install this handler, change
its raw-key routing, or show this setting; a shared settings file retains its value.

## Focused automated checks

Portable state/routing tests can run without GPUI or Zig:

```sh
rustc --edition 2024 --test desktop/src/text_input.rs -o /tmp/boomux-text-input-tests
/tmp/boomux-text-input-tests
```

With normal Desktop prerequisites installed:

```sh
cargo check -p boomux-desktop --tests --locked
cargo test -p boomux-desktop text_input::tests --locked
cargo test -p boomux-desktop native_input::native::tests --locked
cargo test -p boomux-desktop native_text_commits --locked
cargo test -p boomux-desktop macos_option_policy --locked
```

The test target also type-checks the adapter's GPUI interfaces on Linux. Keep the
existing terminal keyboard tests and Linux/macOS CI matrix; these new tests do not
replace them. Documentation-build environment switches are not native build or
runtime evidence.

## Manual acceptance checklist (not yet executed)

Use a disposable Workspace and a harmless input recorder/TUI, not a live command
prompt containing valuable work. Record the exact commit, macOS version, hardware,
keyboard layout/input source, test application and observed bytes for each case.

- Japanese conversion, Chinese Pinyin candidates and Korean composition: preedit
  changes send zero terminal bytes; confirmation inserts the final text once
- Escape and Backspace during composition: candidate navigation and cancellation
  do not emit terminal controls or submit a rename/settings dialog
- US Option-E then E, unmodified US-International/Brazilian dead keys,
  French/German Option characters, direct Unicode, emoji and
  multi-codepoint emoji: inspect output and UTF-16 selection/replacement behavior
- Option as Alt off/on, including Shift-Option and Option-arrow shortcuts; editing
  fields keep native characters with either terminal setting
- ASCII and shifted punctuation held down: preserve repeats; Control-C still
  interrupts, Command-C copies; no phantom Kitty key-release events after IME
- Legacy, application cursor, modifyOtherKeys and Kitty flags 1/3/31: regular
  shortcuts retain negotiated encoding; committed text appears once
- Project, Git and conversation searches, rename and settings fields: replace
  a selection, use arrows/Home/End, Shift-selection, Command-A/C/X/V, and compose
  across emoji/surrogate boundaries; filtering changes only on commit
- Switch panes, Workspace, input field, attachment, modal, application and input
  source mid-composition; close the pane/window; no text lands in another target
- Clipboard paste while composing, repeated cancel/reopen, and system emoji
  picker after cancellation; verify no stale commit or permanently disabled input
- Resize, fullscreen, floating panes, sidebar animation, theme transition,
  scrollback, hidden terminal cursor, HiDPI and screen-edge candidate placement:
  underline/caret and candidate window remain attached to the active input
- Linux X11/Wayland: ordinary keys, control/Alt/Kitty shortcuts, layout-leader held
  suppression, existing clipboard behavior and overlay edits remain unchanged

Accessibility integration and extended-grapheme editing remain separate follow-up
work. Do not mark an acceptance case passed from a compile or scripted smoke test.

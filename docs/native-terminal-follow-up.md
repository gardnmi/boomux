# Native Terminal Follow-Up

Date: 2026-08-03

## Purpose

Boomux now owns its PTYs and shell processes, but the native terminal backend is
not complete. Live attachment is transparent; terminal initialization and
reconnection are still approximations.

This document records the remaining work needed to make Boomux shells behave
consistently in Ghostty, Alacritty, Kitty, and other XDG terminal emulators.

## Current Status

| Capability | Status |
| --- | --- |
| Daemon-owned PTYs and child processes | Working |
| Process survival after attachment closes | Working |
| Native windows through `xdg-terminal-exec` | Working |
| Unmodified live input and output | Working |
| Row and column resize after attachment | Working |
| Single-controller takeover | Working |
| Workspace and shell lifecycle | Working |
| Graceful daemon stop and process cleanup | Working |
| First-attachment terminal negotiation | Missing |
| Correct terminal environment at child startup | Missing |
| Initial pixel dimensions | Missing |
| Reconstructed terminal state on reconnect | Missing |
| ANSI-aware logical output for `boomux read` | Missing |
| Metadata recovery after daemon restart | Missing |
| Live PTY handoff to a replacement daemon | Missing |

The current backend is suitable for proof-of-concept testing. It must not yet
claim exact screen restoration or process survival across daemon restart.
Workspace metadata contains only a UUID and name; working directories belong
exclusively to shells. Dashboard project discovery supplies name suggestions
and does not persist project paths. A shell request without a selected workspace
atomically creates the next available `workspace-N` container.

## Current Creation Sequence

Workspace creation itself is empty. The daemon currently starts a process when
an explicit shell is created, before a native terminal attaches:

```text
create shell with explicit working directory
  -> daemon allocates PTY
  -> daemon starts child
  -> native terminal opens
  -> attachment sends rows and columns
```

The child inherits these variables from the process that originally started the
daemon:

```text
TERM
COLORTERM
TERM_PROGRAM
TERM_PROGRAM_VERSION
```

Those values may not describe the terminal that eventually renders the shell.
For example, an Alacritty window may start the daemon and a later Ghostty window
may attach to a child that still identifies its terminal as Alacritty.

This can affect terminfo selection, true-color detection, keyboard protocols,
shell integration, terminal-specific workarounds, and whether an application
attempts graphics protocols. Direct byte pass-through preserves an extension
only after the application decides to emit it.

The PTY also begins with a default size. The attachment corrects rows and
columns afterward, but startup output may already have rendered at the wrong
dimensions. Pixel width and height are currently sent as zero.

## Phase 1: Terminal Handshake

### Target Sequence

Change shell creation to a two-stage operation:

```text
create pending shell metadata
  -> launch or enter native terminal
  -> attachment reports terminal profile and dimensions
  -> daemon allocates PTY
  -> daemon starts child with reported environment
  -> attachment begins live transport
```

### Protocol Model

The initial attachment request should include a bounded, explicit profile:

```rust
struct TerminalProfile {
    term: Option<String>,
    colorterm: Option<String>,
    term_program: Option<String>,
    term_program_version: Option<String>,
    rows: u16,
    cols: u16,
    pixel_width: u16,
    pixel_height: u16,
}
```

Do not forward the attachment client's complete environment. Validate lengths,
reject control characters, and send only known terminal capability fields.

Read cell and pixel dimensions from `TIOCGWINSZ` on Unix. Pixel dimensions may
legitimately remain zero when the emulator or kernel does not report them.

### Shell Lifecycle

Extend the shell state model:

```text
Pending -> Running -> Exited
```

- `Pending` has metadata but no PTY or child.
- The first successful attachment starts the PTY and child atomically.
- A failed spawn leaves the shell pending with an actionable error.
- `Running` stores the terminal profile used to start the process.
- `Exited` retains the exit status and bounded terminal state.

Concurrent first attachments must not spawn the child twice. Startup needs one
daemon-owned transition guarded independently from PTY input and output locks.

### Reattachment Policy

A running process cannot have its startup environment replaced. Boomux should
retain the first terminal profile and compare later attachments against it.

Initial policy:

- Allow matching profiles without warning.
- Allow another emulator when core capabilities are compatible.
- Warn clearly when `TERM` differs.
- Do not silently mutate the running process's environment.

A conservative compatibility profile can be considered later if real emulator
testing shows that warnings are insufficient.

### Acceptance Criteria

- Starting the daemon from Alacritty and first-attaching in Ghostty gives the
  child Ghostty's terminal variables.
- Starting the daemon outside a terminal does not force an empty or `dumb`
  terminal profile on the child.
- `stty size` is correct on the child's first prompt, without waiting for a
  later resize event.
- Pixel dimensions reach the PTY when the terminal reports them.
- Two simultaneous first attachments produce exactly one child.
- Dashboard and CLI snapshots distinguish pending, running, and exited shells.
- Closing a pending shell or workspace does not attempt process cleanup.
- Reattaching through a mismatched emulator follows the documented warning
  policy.

### Open Product Decision

Workspace creation must remain empty. A future explicit shell creation or first
attachment can create pending shell metadata and delay the command until a
terminal profile is available.

Choose one behavior before implementation:

1. Open the explicitly requested shell window immediately so it receives a real
   profile.
2. Keep the explicitly requested shell pending until the user opens it.
3. Permit explicit headless shell startup with a documented conservative
   profile.

Option 1 best matches the native-terminal goal. Option 2 is the smallest model.
Option 3 should be added only for a concrete headless workflow.

## Phase 2: VT Reconnection State

### Current Limitation

The daemon retains up to 1 MiB of raw PTY output and replays it into a new
attachment. Raw output is not terminal state.

Reconnection can be incorrect after:

- Cursor movement or in-place line rewrites
- Screen clearing
- Alternate-screen applications such as editors and TUIs
- Progress indicators
- Replay truncation inside an escape sequence
- OSC title, notification, hyperlink, or clipboard commands
- Terminal graphics
- Soft-wrapped lines

A newly attached terminal may look correct, partially correct, or remain blank
until the application redraws.

### Target Data Flow

Keep the native live path unchanged and parse a shadow copy:

```text
PTY output
  -> attachment output unchanged
  -> bounded VT parser input in parallel
```

The parser should retain:

- Primary and alternate screens
- Visible cells and styles
- Cursor position and visibility
- Relevant terminal modes
- Logical scrollback
- Hard newline versus soft-wrap boundaries

On attachment:

1. Put the client terminal into a known state.
2. Emit a sanitized ANSI reconstruction of retained scrollback and screen state.
3. Restore required modes.
4. Switch to unchanged live PTY output.

Do not replay historical OSC clipboard writes, notifications, or other side
effects merely to reconstruct presentation.

### `boomux read`

`boomux read` currently decodes raw replay bytes lossily and selects recent
newline-delimited fragments. It does not understand cursor rewrites, ANSI state,
or terminal wrapping.

After the VT parser exists, `boomux read` should return logical rendered lines:

- Strip control sequences.
- Preserve intentional hard newlines.
- Join terminal soft wraps for the unwrapped output mode.
- Define whether alternate-screen content is included.
- Retain a bounded history and report truncation when useful.

### Acceptance Criteria

- Reopening an ordinary shell restores its current prompt and useful scrollback.
- Reopening `nvim` or LazyGit reconstructs a coherent current screen or follows
  a documented fallback that requests an application redraw.
- Reconnection does not repeat clipboard writes or desktop notifications.
- Truncation never begins by emitting an incomplete escape sequence.
- Live Kitty graphics, Sixel, hyperlinks, and OSC behavior remain unchanged.
- `boomux read` returns plain logical lines without ANSI escape sequences.
- Slow parsing cannot block the PTY reader or child process.

### Open Engineering Decisions

- Select an existing Rust VT parser rather than implementing ANSI parsing.
- Decide how much primary-screen scrollback to retain per shell.
- Decide whether alternate-screen history is readable or only reconstructable.
- Decide whether graphics payloads are retained, omitted, or restored by asking
  applications to redraw.
- Define the safe reset and reconstruction sequence across supported emulators.

## Phase 3: Restart Persistence

Terminal negotiation and VT reconstruction should be reliable before adding
disk persistence.

Persist only reproducible metadata under `$XDG_STATE_HOME/boomux`:

- Workspace and shell IDs
- Workspace and shell names and grouping
- Shell working directories; workspaces have no path
- Explicit shell startup commands and recipes
- Last terminal profile when useful for diagnostics

Do not claim that arbitrary process state can be serialized. After an ordinary
daemon restart, Boomux can recreate shells from metadata but cannot recover the
original processes or their mutated environment.

Live process survival during an upgrade requires a separate Unix PTY handoff
mechanism. Treat that as a later feature, not part of metadata restoration.

## Manual Test Matrix

Run each scenario in Alacritty and Ghostty first, then Kitty if available:

| Scenario | Expected result |
| --- | --- |
| Plain interactive shell | Prompt, input, resize, detach, and reconnect work |
| `nvim` | Full-screen input and resize work; reconnect behavior is recorded |
| LazyGit | Mouse/keyboard input and alternate-screen behavior work |
| OpenCode or another agent | Long-running process survives window closure |
| Bracketed paste | Content reaches the child unchanged |
| Hyperlink and OSC title | Live behavior reaches the emulator unchanged |
| Controller takeover | Old window disconnects; new window controls the shell |
| Cross-emulator reattach | Compatibility warning and behavior match policy |
| Daemon stop | All owned process sessions terminate and socket is removed |

Record the terminal environment visible inside each child:

```console
printf 'TERM=%s\nCOLORTERM=%s\nTERM_PROGRAM=%s\nTERM_PROGRAM_VERSION=%s\n' \
  "$TERM" "$COLORTERM" "$TERM_PROGRAM" "$TERM_PROGRAM_VERSION"
stty size
```

## Implementation Order

1. Create `feat/terminal-handshake`.
2. Add `TerminalProfile` and `Pending` to the shell protocol and domain model.
3. Move PTY spawn from shell metadata creation to first attachment.
4. Add emulator mismatch diagnostics and acceptance tests.
5. Dogfood Alacritty and Ghostty with the manual matrix.
6. Create a separate VT reconstruction branch.
7. Replace raw `boomux read` extraction with parser-backed logical lines.
8. Add metadata persistence only after terminal behavior is stable.

## Definition Of Done

The native terminal milestone is complete when:

- The child starts with the first attachment's real terminal capabilities and
  dimensions.
- Live input and output remain byte-transparent.
- Reconnection produces deterministic, sanitized screen state.
- `boomux read` exposes parser-backed logical lines.
- Alacritty and Ghostty pass the documented manual matrix.
- Remaining daemon-restart limitations are explicit and tested.

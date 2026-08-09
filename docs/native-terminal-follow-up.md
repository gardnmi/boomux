# Native Terminal Follow-Up

Date: 2026-08-03

## Purpose

Boomux owns its PTYs and shell processes. Live attachment is transparent, and
the terminal initialization, reconstruction, persistence, and graceful handoff
phases described here are implemented.

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
| First-attachment terminal negotiation | Working |
| Correct terminal environment at child startup | Working |
| Initial pixel dimensions | Working when reported by the terminal |
| Reconstructed terminal state on reconnect | Working |
| ANSI-aware logical output for `boomux read` | Working |
| Metadata recovery after daemon restart | Working |
| Live PTY handoff to a replacement daemon | Working |

The current backend reconstructs text VT state but does not claim graphics
restoration. Running processes survive acknowledged graceful daemon restart;
an unexpected daemon exit falls back to reproducible metadata recovery.
Workspace metadata contains only a UUID and name; working directories belong
exclusively to shells. Dashboard project discovery supplies name suggestions
and does not persist project paths. A shell request without a selected workspace
atomically creates the next available `workspace-N` container.

## Current Creation Sequence

Workspace creation itself is empty. Explicit shell creation records pending
metadata and waits for a native terminal attachment:

```text
create pending shell with explicit working directory
  -> native terminal opens
  -> attachment sends terminal profile and dimensions
  -> daemon allocates the correctly sized PTY
  -> daemon starts the child with the reported environment
```

Protocol 16 supplies the attachment client's complete Unix environment
ephemerally, including these terminal-profile variables:

```text
TERM
COLORTERM
TERM_PROGRAM
TERM_PROGRAM_VERSION
```

The daemon validates names and values, clears its own inherited environment,
then applies the client environment. It overrides terminal-profile fields and
`BOOMUX_*` identity fields authoritatively. The payload is not persisted,
included in snapshots, events, or handoff state. Missing values remain unset.
A later attachment does not mutate a running child and receives a
warning when its `TERM` differs from the startup profile.

This can affect terminfo selection, true-color detection, keyboard protocols,
shell integration, terminal-specific workarounds, and whether an application
attempts graphics protocols. Direct byte pass-through preserves an extension
only after the application decides to emit it.

The PTY begins with the first attachment's rows, columns, pixel width, and pixel
height. Pixel dimensions may remain zero when the terminal does not report them.

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

Forward the attachment client's environment only in the startup request that
can create a new run. Preserve Unix bytes, reject invalid names, duplicates, and
NUL bytes, redact the payload from debug output, and never persist it.

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

### Product Decision

Workspace creation must remain empty. A future explicit shell creation or first
attachment can create pending shell metadata and delay the command until a
terminal profile is available.

Boomux uses option 2:

1. Open the explicitly requested shell window immediately so it receives a real
   profile.
2. Keep the explicitly requested shell pending until the user opens it.
3. Permit explicit headless shell startup with a documented conservative
   profile.

Pending shells remain visible and retryable until opened. Headless startup is
not supported without a concrete terminal profile.

## Phase 2: VT Reconnection State

### Implementation

The daemon feeds a shadow `vt100` parser with PTY output while forwarding the
original bytes unchanged. It retains 2,000 primary-screen scrollback rows and
emits at most 1 MiB of safe reconstruction into a new attachment.

Reconnection reconstructs cursor movement, in-place rewrites, screen clearing,
styles, modes, and the current alternate screen. Known limits remain for:

- Terminal graphics
- Alternate-screen history
- Emulator-specific state outside the parser's VT model

Historical OSC title, notification, hyperlink, and clipboard commands are
intentionally omitted so reconnection cannot repeat their side effects.

### Target Data Flow

Keep the native live path unchanged and parse a shadow copy:

```text
PTY output
  -> attachment output unchanged
  -> bounded shadow VT state alongside live delivery
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

`boomux read` returns parser-backed logical rendered lines:

- Strip control sequences.
- Preserve intentional hard newlines.
- Join terminal soft wraps for the unwrapped output mode.
- Include the current alternate screen but not alternate-screen history.
- Retain 2,000 primary-screen scrollback rows.

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

- Decide whether graphics should remain omitted or trigger application redraw.
- Evaluate moving shadow parsing off the PTY reader if profiling shows meaningful
  backpressure.

## Phase 3: Restart Persistence

The daemon atomically persists versioned JSON state at
`$XDG_STATE_HOME/boomux/state.json`, with the XDG default
`~/.local/state/boomux/state.json` when the environment variable is unset.

Persist only reproducible metadata under `$XDG_STATE_HOME/boomux`:

- Workspace and shell IDs
- Workspace and shell names and grouping
- Shell working directories; workspaces have no path
- Explicit shell startup commands
- Last terminal profile when useful for diagnostics

State directories are owner-only, state files are bounded and owner-validated,
and updates use a synced temporary file followed by atomic rename. Invalid or
unsupported state fails startup rather than silently discarding metadata.

Do not claim that arbitrary process state can be serialized. After startup
without a handoff, such as crash recovery, Boomux restores shells as pending and
recreates them on first attachment, but cannot recover the original process or
its mutated environment.

An explicit `boomux daemon restart` instead uses the separate transactional Unix
PTY handoff documented in [`live-pty-handoff.md`](live-pty-handoff.md). It
transfers running processes, preserves static exited-run terminal state, and
reconnects active attachment clients; this does not change the guarantees of
metadata-only recovery.

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
| Graceful restart while detached | Process PID and subsequent input/output survive |
| Graceful restart while attached | Client reconnects without leaving raw mode |
| Graceful restart after shell exit | Run identity, exit status, and final terminal state survive without a new process |
| Daemon stop | All owned process sessions terminate and socket is removed |

Record the terminal environment visible inside each child:

```console
printf 'TERM=%s\nCOLORTERM=%s\nTERM_PROGRAM=%s\nTERM_PROGRAM_VERSION=%s\n' \
  "$TERM" "$COLORTERM" "$TERM_PROGRAM" "$TERM_PROGRAM_VERSION"
stty size
```

## Implementation Order

1. Dogfood Alacritty and Ghostty with the manual matrix.
2. [Complete] Implement VT reconstruction.
3. [Complete] Replace raw `boomux read` extraction with parser-backed logical
   lines.
4. [Complete] Add atomic metadata persistence.
5. [Complete] Add transactional live PTY handoff and active-client reconnection.
6. [Complete] Preserve exited-run metadata and final terminal state during
   graceful handoff.

## Definition Of Done

The native terminal milestone is complete when:

- The child starts with the first attachment's real terminal capabilities and
  dimensions.
- Live input and output remain byte-transparent.
- Reconnection produces deterministic, sanitized screen state.
- `boomux read` exposes parser-backed logical lines.
- Alacritty and Ghostty pass the documented manual matrix.
- Remaining daemon-restart limitations are explicit and tested.

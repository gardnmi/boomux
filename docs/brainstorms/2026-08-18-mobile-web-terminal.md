# Mobile Web Terminal Exploration

> **Status: Exploration, not an accepted design or release commitment.** This
> workstream is separate from opening a harness-native web UI for conversation
> and tool interaction. `CONTEXT.md`, `architecture.md`, protocol contracts, and
> source remain authoritative.

## Goal

Let the Boomux mobile PWA display an exact ShellRun as a real browser terminal,
with terminal-grade VT rendering, Unicode handling, selection, scrollback, and
mobile input ergonomics. Preserve the daemon as the owner of the PTY, process,
run identity, terminal reconstruction, and controller rules.

This is complementary to native harness handoff:

- OpenCode or Pi owns its conversation, tools, permissions, and native web UX.
- Boomux owns Shell and ShellRun lifecycle and may present their terminal.
- Opening a native harness UI must not require an embedded terminal.
- Adding an embedded terminal must not require Boomux to normalize harness
  transcripts.

## Candidate Prior Art

[`coder/ghostty-web`](https://github.com/coder/ghostty-web) is useful prior art,
but adopting that package is not assumed.

As of 2026-08-18 it provides:

- Ghostty's VT parser compiled to WebAssembly.
- An xterm.js-compatible `Terminal` API.
- An HTML5 Canvas renderer with dirty-row rendering. Despite Ghostty's native
  GPU architecture, this project does not currently provide a WebGL renderer.
- Input and resize callbacks suitable for a WebSocket-backed PTY.
- Selection, scrollback, links, a fit addon, and approximately 400 KiB of WASM
  with no runtime dependencies.
- A demo that binds loopback, obtains a per-run same-origin token, validates the
  WebSocket origin, and connects the browser to a real local shell.

The experiment should compare its behavior and API with xterm.js and a possible
future upstream `libghostty` browser distribution. Renderer choice must remain
separate from Boomux transport and authority semantics so Canvas, WebGL, or a
later implementation can be substituted.

## Required Boundaries

### Exact Identity

Every browser terminal is bound to an exact `(node_id, shell_id, run_id)`. It
must fail closed when the Shell starts another run. It must never silently show
or control the replacement run.

### PTY Authority

The daemon remains the only PTY and process owner. Browser code receives output
frames and emits typed terminal actions; it never opens a host PTY, discovers a
process, or infers Agent lifecycle from terminal content.

### Observation And Control

Read-only observation and writable control are distinct capabilities.

- A read-only observer may fit its local renderer but must not resize the shared
  PTY or displace an existing controller.
- Writable input requires an explicit controller lease under the existing
  one-controller rule.
- Takeover must be visible and explicit; merely opening an Agent detail page
  cannot acquire control.
- Controller loss, browser sleep, network loss, and daemon handoff must release
  or reconnect according to a defined lease contract.

### Recovery And Backpressure

The transport needs a bounded initial reconstruction plus ordered incremental
output. Reconnect must reacquire an exact run-scoped baseline rather than replay
unbounded history or assume no output was missed. Slow or backgrounded phones
must not cause unbounded server queues; coalescing or forced resynchronization is
preferable to retaining arbitrary output frames.

### Remote Nodes

Remote cached projections remain prompt-free and non-authoritative. A future
remote terminal must be an owner-routed live operation with the owning Node
validating the exact ShellRun and retaining PTY authority. Terminal bytes must
not enter the coordinator's projection cache.

## Security Model

A writable browser terminal is remote shell access and is substantially more
powerful than the current read-only PWA.

- Keep the HTTP and WebSocket gateway on loopback behind a private access layer; never
  Funnel it.
- Apply access-layer identity, Host, and Origin validation to the WebSocket upgrade
  as well as ordinary HTTP requests.
- Use a short-lived, same-origin, run-bound connection token or equivalent
  upgrade proof. Do not put durable credentials in URLs.
- Keep API responses and terminal bootstrap data out of service-worker and HTTP
  caches.
- Bound frame size, initial state, queued output, input size, and idle lifetime.
- Treat clipboard, paste, file drops, OSC links, title changes, notifications,
  and browser-open requests as separate capabilities rather than generic escape
  sequence side effects.
- Render terminal output without HTML interpretation and retain a restrictive
  content security policy.
- Audit controller acquisition, takeover, input, resize, and release without
  logging private terminal content.

## Mobile UX Questions

- IME, composed Unicode, dead keys, emoji, and complex scripts.
- Software keyboard activation without hidden-input focus traps.
- Touch selection, copy, paste confirmation, and scroll versus terminal mouse
  reporting.
- Rotation, dynamic viewport height, safe areas, device-pixel ratio, and font
  remeasurement.
- Special keys and modifier state without fabricating unsupported chords.
- Accessibility when the visual renderer is Canvas or WebGL.
- Background sleep and reconnect without accidental key replay.
- Whether read-only observation should be the default even when control is
  available.

## Proposed Experiments

1. **Offline renderer harness:** feed existing terminal-state fixtures and live
   captured byte streams into `ghostty-web`, xterm.js, and any upstream Ghostty
   browser API. Compare Unicode, alternate screen, colors, cursor behavior,
   selection, memory, bundle size, and phone performance.
2. **Exact read-only stream:** expose one local current ShellRun through a
   development-only loopback WebSocket. Send a bounded baseline and incremental
   frames with forced resynchronization under backpressure. Do not accept input
   or resize the PTY.
3. **Lifecycle recovery:** validate browser sleep/wake, dropped frames, run
   replacement, exited runs, daemon restart, graceful handoff, and server
   shutdown.
4. **Explicit controller:** only after the observer contract is proven, add a
   deliberate control action, controller lease, visible takeover, resize policy,
   and input audit events.
5. **Owner-routed remote operation:** only after local control is proven, define
   the minimum protocol capability and mixed-version behavior for a remote Node.

## Acceptance Evidence

- VT and Unicode fixture comparison across candidate renderers.
- Bounded-memory tests for slow and disconnected clients.
- Exact-run rejection when a Shell is restarted during connection setup.
- Serial native scenarios for reconnect, controller conflict, graceful daemon
  handoff, process exit, and cleanup.
- Browser tests for WebSocket authentication, origin rejection, input gating,
  resize, phone viewport changes, and service-worker exclusion.
- Live validation on Android and iOS before claiming mobile support.

## Non-Goals For The First Experiment

- Replacing native terminal windows.
- Parsing terminal screens into authoritative Agent state or conversations.
- Combining OpenCode, Pi, or another harness's native web application into the
  terminal renderer.
- Writable remote access, uploads, clipboard synchronization, or terminal mouse
  reporting.
- A protocol or persistence change before the local read-only transport proves
  the required semantics.

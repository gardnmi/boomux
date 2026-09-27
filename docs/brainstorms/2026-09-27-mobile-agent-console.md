# Mobile Agent Console

> **Status: Exploration.** This is a design proposal for the Web UI served by
> Desktop's `webgpu_gateway`. It changes no protocol, persistence, or Agent
> lifecycle contract. `CONTEXT.md`, `docs/architecture.md`, accepted ADRs, and
> source remain authoritative.

## Goal

At the same Web UI URL used on desktop, a phone should open to Agents rather
than the Workspace tiling surface. A user should be able to find an Agent that
needs attention, see its live output, and send a prompt to its exact current
run without disrupting the desktop terminal. The desktop layout remains
available through an explicit **Full workspace** action.

## What this would look like on a phone

The first screen is a short Agent list, with no Workspace tiling controls:

```text
Agents                         Full workspace
Needs you
  Codex · boomux                 Waiting for input
Working
  OpenCode · edge-datapipe-cdc    Active now
Ready
  Pi · notes                     Idle
```

Tapping **Codex** opens one Agent detail screen:

```text
< Agents          Codex · boomux
Waiting for input

[Live output from this exact Agent terminal]
[The terminal stays connected on the desktop]

[ Type a prompt or response...       ] [Send]
[Esc] [Ctrl+C] [Tab] [↑] [↓] [Enter]
[Open native app, if available]
```

**Send** types into that existing Agent's terminal. It does not start another
Agent or send to a merely similar session. The output area is the real terminal
screen, including prompts and tool output. Terminal output is acceptable for this feature; the phone keyboard and input
flow are the priority.

The terminal itself should not receive phone keyboard edits. Tapping output
scrolls, selects, or copies it without summoning the keyboard. A separate,
visible composer accepts normal phone editing, autocorrection, paste, and IME
composition. The keyboard's Enter key inserts a newline; only the **Send**
button submits. A small key row supplies Esc, Ctrl+C, Tab, arrows, and Enter
for interactive TUI questions that are not ordinary prompts.

## Findings

- The current `webgpu_gateway` already returns local and remote Workspaces,
  Shells, and Agents through `/api/snapshot`; `/api/changes` supplies bounded
  updates. `poc/webgpu-tiling/app.js` already builds Agent rows, but on a narrow
  viewport the sidebar containing them is hidden. Its `/api/attach` path
  attaches as a primary terminal controller and can displace another client.
- The separate `boomux web` PWA already has an Agent-only list, exact current
  local Agent authorization, and a collaborative terminal. Its transport
  preserves the native primary controller and accepts input from a bounded
  browser participant. See `src/mobile_web.rs`, `src/web_terminal.rs`, and
  ADR 0008. It is served by another gateway, so it does not currently answer
  the Desktop Web UI URL.
- The existing collaborative terminal is the common live output and input
  contract across harnesses. A terminal screen is not a structured transcript:
  cursor positioning, alternate screens, and the desktop PTY's grid cannot be
  safely reflowed into chat bubbles on a narrow phone.
- OpenCode has a native web UI and session message APIs, but the host installed
  here is 1.18.32 while current upstream documentation describes newer APIs.
  Boomux should continue offering an exact native handoff when available.
  OpenAI's Agents API manages its own sessions; it does not identify or control
  an existing local Codex CLI ShellRun. A generic host-specific transcript
  layer would therefore need separate compatibility work for each harness.
- Mobile keyboards change the visible viewport independently of the layout
  viewport on some browsers. The existing PWA follows `VisualViewport` during
  keyboard and scroll changes; the same behavior is needed for a visible
  composer in the Desktop Web UI.

## Recommended first slice

1. Add an Agent-focused route/view to `webgpu_gateway` on the **same origin**.
   At narrow viewport widths, open it by default. Use viewport layout rather
   than user-agent sniffing, and offer a persistent-in-tab **Full workspace**
   switch so tablets and landscape phones can choose. Keep a direct `/agents`
   route available at every width.
2. Build cards from the existing snapshot, keyed by `(node_id, agent_id,
   shell_id, run_id)`. Put blocked attention first, then working, then idle.
   Include the Workspace, owning Node, harness, last observation, and whether
   the Node is stale. Show historical attention separately; never present it as
   a writable current run. An unavailable remote Node remains visible with a
   clear stale label.
3. Tapping a current local Agent opens one detail view with a bounded terminal
   renderer, live connection state, and a visible `<textarea>` plus **Send**.
   Share the existing collaborative terminal bridge with the Desktop gateway.
   Add an Agent-specific one-use authorization endpoint that revalidates the
   exact Node, Agent, Shell, and ShellRun immediately before attachment. The
   phone cannot become primary or resize the desktop PTY. Close the socket
   when leaving the detail view. Keep remote Agent control out of this slice;
   cached remote projections do not authorize input.
4. Put a visible `<textarea>` outside the terminal renderer. It owns phone
   keyboard focus, editing, paste, autocorrection, and IME composition. The
   terminal renderer must not forward its hidden input field on phone layouts.
   Send only from an explicit button tap, never from Enter or an `input` event.
   Keep the composer above the software keyboard using `VisualViewport` and
   safe-area insets. Do not automatically refocus it after sending.
5. Limit prompt bytes and strip pasted terminal control characters before
   sending through the exact current attachment. For a single line, send text
   and one terminal Enter as one bounded action. Permit multiline only when
   that TUI has enabled bracketed paste; otherwise keep the draft and explain
   why it cannot be sent safely. Keep drafts in memory only, never replay a
   send after reconnect, and disable **Send** when the run changes. Add a
   separate key row for Esc, Ctrl+C, Tab, arrows, and Enter; label these as
   terminal keys rather than prompt actions.
6. Keep the terminal rendering honest: show the authoritative grid and allow
   horizontal pan/zoom where needed. Do not parse VT output into conversation
   messages or claim that the screen contains full history. Where an exact
   harness-native link exists, show **Open in [harness]** for its richer
   conversation view.

This extends one gateway and reuses the existing lifecycle authority. It avoids
starting a second web service or exposing a harness server just to serve the
phone layout. The backend work is sharing the collaborative bridge and adding
Agent-scoped authorization; most visible work is the Agent view, composer, and
mobile viewport behavior.

## Why not the other approaches?

| Approach | Limitation for this feature |
| --- | --- |
| Shrink the current Workspace tiling UI | Hides the Agent list and leaves a desktop grid and primary attachment on a phone. |
| Redirect phones to the older `boomux web` PWA | It already has useful code, but requires another gateway/URL and does not provide the requested prompt composer. Reuse its transport and interaction patterns instead. |
| Build one chat transcript API for every harness | Each harness has different APIs and lifecycle semantics; the PTY is the only shared exact live run. Add native transcript adapters later where a stable, authoritative API exists. |
| Send prompts through an HTTP endpoint without an attachment | It separates input from the output and controller state the user is viewing, increasing the chance of sending to a changed run. |

## Acceptance checks

- At phone width, the first screen contains only Agent navigation and status;
  the desktop Workspace UI remains reachable by choice.
- A local Agent's live output appears without detaching or resizing its native
  terminal. Sending one prompt reaches only the selected exact ShellRun.
- Authorization rejects stale Agent, Shell, or run identities; a run replacement
  disables the composer and does not silently attach to the new run.
- Backgrounding, reload, disconnect, and daemon handoff never replay input.
  Browser queues, output reconstruction, and prompt size remain bounded.
- Remote stale Agents and historical attention are visibly noninteractive.
- Test narrow Chromium and WebKit viewports, keyboard open/close, orientation,
  IME composition, autocorrect, paste, Enter-versus-Send behavior, the key row,
  and live Android and iOS browsers before claiming phone support. Keep browser
  assets and terminal bytes out of persistent caches.

## External references

- [MDN: VisualViewport](https://developer.mozilla.org/en-US/docs/Web/API/VisualViewport)
- [MDN: viewport and interactive widgets](https://developer.mozilla.org/en-US/docs/Web/HTML/Reference/Elements/meta/name/viewport)
- [OpenCode server API](https://dev.opencode.ai/docs/server/)
- [OpenCode web UI](https://dev.opencode.ai/docs/web/)
- [Official OpenAI documentation: Agents API overview](https://developers.openai.com/api/docs/guides/agents-api/overview)

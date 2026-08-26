# Control Current Agent Shells Through Web Terminal

Status: Accepted

Not every Agent harness provides an authoritative web handoff for a local CLI
Session, and a native handoff does not replace access to the exact live terminal
process. Building host-specific transcript adapters would create a large and
unstable compatibility surface. Boomux instead exposes any current local Agent's
existing terminal process through its daemon-owned PTY.

Only an exact current local Agent card can authorize control. The gateway
binds Node, Agent, Shell, and ShellRun identity into a bounded one-use grant that
expires after 30 seconds. A WebSocket must present an allowlisted dashboard
Origin, the fixed versioned protocol, and the grant as a secondary subprotocol.
The grant is consumed before upgrade. The daemon then independently enforces an
protocol-44 collaborative exact-run attachment, with no restart and no
attachment environment. A protocol-44 client fails closed on an older daemon
rather than downgrading to exclusive takeover.

The gateway translates a closed set of bounded terminal messages and never
exposes daemon controller tokens or arbitrary requests. A self-hosted
`ghostty-web` canvas renderer uses Ghostty's WASM VT implementation for
reconstruction and unchanged live PTY output. The current native primary and up
to four daemon-token-keyed collaborators receive nonblocking output fanout and
may submit serialized whole input frames. The primary remains sole PTY resize
authority; browser viewport changes do not alter its logical grid and browser
resize frames are ignored. Browser background or closure releases only that
collaborator without stopping the Agent or displacing the native terminal.
Explicit ordinary takeover detaches the prior primary and every
collaborator. Daemon graceful replacement quiesces every participant through the
existing reconnect boundary and each browser reacquires only the same exact
ShellRun.
The gateway admits at most four active browser terminals, uses eight-entry
per-direction bridge queues and browser write deadlines, and disconnects slow or
saturated clients rather than blocking PTY output.

This is remote shell access, not a transcript projection. Terminal bytes are not
persisted by the gateway, copied into Agent or Node projections, cached by the
service worker, logged, or interpreted as lifecycle evidence. The gateway stays
on loopback; Tailscale or another private access layer owns TLS, identity, and
access policy. Remote projected Nodes are excluded.

Rejected alternatives were:

- Depend on a harness-native web URL, because not every harness exposes one and
  those links are separate Session-specific capabilities when available.
- Build harness-specific protocol or transcript frontends, because Boomux would
  own a growing set of unstable interaction adapters.
- Expose arbitrary Shell IDs through a general WebSocket, because an Agent card
  must establish eligible local Agent context before the daemon validates the
  exact run.
- Put a reusable bearer credential in the WebSocket URL, because URLs are more
  likely to enter logs and history.
- Start or resume an Agent from the browser, because terminal control must never
  create a run, select a nearby Session, or cross a run replacement.

The existing attachment frames remain sufficient, but a distinct request and
capability are required so old daemons cannot reinterpret collaboration as
takeover. The collaborative `Attached` response reports the primary terminal
profile, and primary resize frames flow back to collaborators so every renderer
uses the PTY's authoritative grid. This decision adds protocol 44. Collaborators
are bounded ephemeral runtime state, so it adds no durable state or handoff
format version.

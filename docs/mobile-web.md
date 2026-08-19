# Mobile Web Dashboard

> **Status: Experimental MVP.** The HTTP response shapes are not a stable
> automation contract. The daemon protocol and `boomux.cli/v1` remain the stable
> integration surfaces.

## Goal

`boomux web` provides a phone-friendly, installable view of the current Agents
and outstanding Agent attention known to one coordinating Boomux Node. It is a
read-only presentation layer inspired by mobile agent dashboards: durable
attention is first, active work remains easy to scan, and an Agent detail page
presents lifecycle observations alongside bounded rendered terminal output when
that output is authoritative locally. An optional native handoff opens an exact
authoritative local OpenCode Session in OpenCode Web.

Boomux is not an interactive chat client. It does not parse or normalize harness
transcripts and does not send messages, commands, permission responses, or
question answers. OpenCode's separate native interface owns those interactions.
Rendered Shell output remains terminal state and is labeled as such.

## Start It

Run the gateway on its fixed loopback interface:

```console
boomux web
```

The default URL is `http://127.0.0.1:3737`. Select another loopback port with
`--port`. The command also ensures one daemon-supervised Shared Harness Runtime
generation on `127.0.0.1:4097`; choose another stable loopback port with
`--opencode-web-port`. The same Node-local generation is used by eligible native
OpenCode TUIs and the phone. Restarting `boomux web` does not replace that
generation. Runtime exit withdraws native links until the daemon starts or
strictly cold-adopts a replacement generation.

Set `OPENCODE_SERVER_PASSWORD` and optionally `OPENCODE_SERVER_USERNAME` before
the runtime's first start. They are ephemeral startup environment, are never
persisted by Boomux, and must be supplied consistently to attached clients.
Changing them requires replacing the runtime generation.

The browser derives OpenCode's public origin from the dashboard's scheme and
hostname plus the configured OpenCode port. `--opencode-web-url` overrides that
public origin for the same local daemon runtime; it never selects an unrelated
external OpenCode server. `--no-opencode-web` disables runtime startup from
`boomux web` and native links.

Create or open an ordinary Boomux login Shell and type bare, zero-argument,
interactive `opencode`. A scoped runtime `PATH` shim available only in eligible
managed login Shells redirects that invocation internally to stock
`opencode attach` for the Shared Harness Runtime. Boomux reapplies the shim after
normal bash, zsh, or fish interactive startup configuration. Arguments,
subcommands, noninteractive invocations, commands outside Boomux, absolute
binary paths, and calls after subsequently modifying `PATH` execute the real
OpenCode unchanged. There are no IDs or special Boomux attach commands in the
user workflow.

The TUI plugin reactively claims the selected root Session and updates the claim
after switching or forking. One or more TUI holders can maintain a claim to the
exact current ShellRun; conflicting Shells selecting the same Session fail
closed. The server plugin reports lifecycle only through a current claim. A
detached desktop TUI remains connected to the shared runtime, receives phone
events, and is synchronized when the terminal returns. `--pure`, `--mini`,
absolute paths, modified `PATH`, and otherwise ineligible invocations receive no
claim or native link.

Remote access remains an external deployment concern. Publish both loopback
ports through a private access layer. For example, with Tailscale Serve:

```console
tailscale serve --https=4097 --bg http://127.0.0.1:4097
tailscale serve --https=443 --bg http://127.0.0.1:3737
```

Open the HTTPS URL printed by Tailscale on the phone. Use the browser's **Add to
Home Screen** action to install the progressive web app.

## Presentation Rules

- Local Agents come from the authoritative local snapshot.
- Remote Agents come from each registered Node's bounded reduced projection.
- Every Agent link carries both its owning Node ID and Node-local Agent ID.
- Schedule-owned Agents are excluded.
- One newest non-inactive and non-done Agent is selected for each exact current
  Node, Shell, and run identity.
- Historical, inactive, and done Agents are hidden unless they retain durable
  blocked or completed attention.
- Blocked attention sorts before completed attention, then active and historical
  lifecycle state.
- After the gateway's first baseline, a local current Agent observed moving from
  `working` to `idle` receives an ephemeral finished alert. The gateway refreshes
  its projection once per second even without browser clients, so the alert
  remains available across phone sleep and PWA reconnect while the same Agent
  stays idle and current. It is not fabricated durable attention, does not arise
  from a remote projection, and is lost when the gateway process restarts.
- Active counts require both a working or blocked observation and an exact match
  to the Shell's current run; retained historical observations never become live
  merely because their state label remains active-looking.
- Opening an Agent does not acknowledge attention or change lifecycle state.
- An exact native link is returned only for a local OpenCode Agent with a
  canonical external Session ID, UTF-8 working directory, and current Agent
  Session Claim for the Agent's exact ShellRun and Shared Harness Runtime
  generation. The directory is
  base64url-encoded for OpenCode's
  `/<directory>/session/<session-id>` route.
- Native links are not produced for projected remote Agents because their
  external Session identity and working directory intentionally remain on the
  owner Node. Remote Agents remain unlinked to this Node's runtime.
- Local terminal output is capped at 256 KiB and is read only when the durable
  Shell still has the Agent's exact run as its current run.
- A historical Agent never displays output from a later run of the same Shell.
- Remote terminal output, lifecycle evidence, external Session identity, and
  working directory remain absent from cached projections.
- Browser polling stops while the page is hidden and resumes on visibility or
  focus; the gateway's background projection refresh continues independently.
- A temporary daemon failure retains the last bounded projection with
  `daemon_connected: false` until a later refresh succeeds.

## Security And Privacy

The gateway and Shared Harness Runtime bind only to `127.0.0.1`; there is no
option to bind a LAN or tailnet address. Boomux does not configure or authenticate
the private access layer. That layer owns TLS, authentication, and ACLs
appropriate for both the read-only dashboard and OpenCode's
full-control origin. Public exposure is outside this design.

Terminal output and lifecycle evidence can contain private source, paths,
commands, prompts, credentials, or model responses. Native handoff URLs expose
the local working-directory encoding and canonical Session ID to the authorized
dashboard client and destination OpenCode origin. API responses carry
`Cache-Control: no-store`. The service worker caches only HTML, JavaScript, CSS,
the manifest, and the icon; it never handles `/api/` requests. Browser storage
does not persist Agent snapshots or terminal output.

The HTTP API is an allowlisted projection. It cannot forward arbitrary daemon
requests, send terminal input, acquire a Shell controller, resume a Session,
acknowledge attention, stop processes, or mutate Node registration.

The OpenCode origin is a separate full-control application. Set a nonempty
`OPENCODE_SERVER_PASSWORD` unless the private access layer provides the intended
authentication boundary. Keep both listeners on loopback and never publish them
publicly. Boomux deliberately uses an external link rather than an iframe or
reverse proxy so this boundary remains visible.

## MVP Endpoints

- `GET /api/snapshot` returns the current Node-qualified Agent cards and counts.
- `GET /api/agents/{node_id}/{agent_id}` returns one exact Agent detail and
  lifecycle timeline, eligible local rendered terminal output, and an optional
  exact native OpenCode Web handoff.

These shapes are intentionally experimental. Future native handoff for other
harnesses must preserve each harness's runtime ownership and exact Session
identity rather than introducing transcript adapters or treating terminal
screens as authoritative messages.

## Future Terminal Work

A real browser terminal is tracked separately from native harness web handoff.
Any implementation must bind to an
exact ShellRun, preserve daemon PTY and controller authority, start as a bounded
read-only observer, and keep remote terminal bytes out of cached projections.
See
[`brainstorms/2026-08-18-mobile-web-terminal.md`](brainstorms/2026-08-18-mobile-web-terminal.md).

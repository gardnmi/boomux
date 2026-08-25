# Mobile Web Dashboard

> **Status: Experimental MVP.** The HTTP response shapes are not a stable
> automation contract. The daemon protocol and `boomux.cli/v1` remain the stable
> integration surfaces.

## Goal

`boomux web` provides a phone-friendly, installable view of the current Agents
and outstanding Agent attention known to one coordinating Boomux Node. It is a
bounded presentation layer inspired by mobile agent dashboards: durable
attention is first and active work remains easy to scan. Attention can be
dismissed explicitly. Eligible Agent cards offer native handoffs such as the
exact authoritative local OpenCode Session in OpenCode Web. Every current local
Agent card also offers a self-hosted browser terminal for its exact existing
ShellRun.

Boomux is not an interactive chat client. It does not parse or normalize harness
transcripts. Harness-native interfaces own those interactions. Web terminal
control instead forwards unchanged terminal bytes under the existing daemon PTY
and controller contract.

## Start It

Run the gateway on its fixed loopback interface:

```console
boomux web
```

For desktop integrations or normal background use, start it detached and wait
for readiness:

```console
boomux web start
boomux web status
```

All lifecycle subcommands support the stable JSON envelope with commands
`web.start`, `web.status`, and `web.stop`. Background start requires an already
running daemon and never resurrects a stopped daemon. Repeating an equivalent
start is idempotent, including when the requested OpenCode host is unavailable;
different options on the same HTTP port are rejected. Status reports requested
OpenCode port/URL separately from the active runtime port/URL.

The default URL is `http://127.0.0.1:3737`. Select another loopback port with
`--port`. The command also attempts to ensure one daemon-supervised Shared
Harness Runtime generation on `127.0.0.1:4097`; choose another stable loopback
port with `--opencode-web-port`. If the OpenCode executable is absent, the
dashboard starts without OpenCode links and continues to expose other eligible
native handoffs such as Claude Remote Control. Port conflicts, installed-runtime
startup failures, readiness timeouts, and daemon/protocol failures remain fatal.
The same Node-local generation is used by eligible native OpenCode TUIs and the
phone. Restarting `boomux web` does not replace that generation. The gateway
refreshes its presentation identity from the authoritative daemon and follows a
replacement generation only on its originally requested port. Runtime exit or
an identity mismatch withdraws native links until the daemon starts or strictly
cold-adopts a replacement generation; the browser's normal polling presents an
eligible replacement without a page reload.

Stop the default gateway without stopping the Boomux daemon or any managed
processes:

```console
boomux web stop
```

For a gateway started with another HTTP port, pass the same port to
`boomux web stop --port PORT`. The command is idempotent when that gateway is not
running.

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

An interrupted ShellRun never transfers its claim to a replacement run. When
cold recovery has one exact resumable OpenCode Agent, Boomux instead resumes its
canonical Session through the Shared Harness Runtime and the replacement TUI
establishes a fresh claim for the new run. If shared launch preparation is
unavailable, exact Session recovery remains fail-open as a standalone TUI and
the native link remains absent. User-entered argument-bearing commands such as
`opencode --continue` and `opencode --session ID` continue to bypass the scoped
shim.

To publish the dashboard and any active OpenCode runtime to the current
Tailscale tailnet, opt in when starting the foreground or background gateway:

```console
boomux web --tailscale
boomux web start --tailscale
```

Boomux requires a connected PATH-resolved Tailscale CLI and MagicDNS name. It
reuses compatible existing Serve routes, rejects conflicts before mutation, and
adds only missing root routes for dashboard HTTPS 443 and OpenCode HTTPS on the
active runtime port. When OpenCode is unavailable, only the dashboard route is
published. `Ctrl-C` and `boomux web stop` remove only routes that
invocation created; unrelated and compatible pre-existing Serve routes remain.
A later stop reconciles exact owned routes after a gateway crash. Boomux does
not configure tailnet grants or ACLs.

Open the printed dashboard HTTPS URL on the phone. Use the browser's **Add to
Home Screen** action to install the progressive web app. OpenCode Web is a
separate full-control HTTPS origin; restrict both routes to trusted tailnet
users and configure `OPENCODE_SERVER_PASSWORD` when tailnet policy alone is not
the intended authentication boundary.

## Presentation Rules

- Local Agents come from the authoritative local snapshot.
- Remote Agents come from each registered Node's bounded reduced projection.
- Every Agent card carries both its owning Node ID and Node-local Agent ID.
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
- Opening a native Agent link does not acknowledge attention or change lifecycle
  state.
- Dismiss is available only for a local Agent with durable attention or a
  gateway-owned finished marker. It carries the exact Node ID, Agent ID, and
  current attention or lifecycle observation revision. Durable attention stays
  visible until the daemon confirms acknowledgment; remote and stale requests
  fail closed.
- An exact native OpenCode link is included on a card only for a local OpenCode Agent with
  a canonical external Session ID, UTF-8 working directory, and current Agent
  Session Claim for the Agent's exact ShellRun and Shared Harness Runtime
  generation. The directory is
  base64url-encoded for OpenCode's
  `/<directory>/session/<session-id>` route.
- An exact **Open in Claude** link is included only for a current local Claude
  Agent whose exact Agent/ShellRun has a protocol-43 Remote Control binding
  observed directly by a hook. Its opaque bridge ID is percent-encoded into
  `https://claude.ai/code/<bridge-id>`. No transcript content is read or proxied.
- Codex Agents have no native handoff. Codex does not document an authoritative
  thread-specific Remote URL, and `codex://threads/<thread-id>` is not treated as
  a phone-accessible Remote destination.
- Kiro Agents have no native handoff. Kiro cloud sessions are available through
  Kiro Web and Mobile, but Kiro does not document an exact browser URL derivable
  from a local CLI Session ID, and local hooks do not establish cloud authority.
- Every current local Agent, independent of its current or future integration
  name, offers **Open in Web Terminal** in addition to any native handoff.
  Authorization binds its exact Node, Agent, Shell, and run identities. Opening
  it joins a protocol-44 collaborative exact-run attachment without starting or
  resuming a Session.
  The native primary remains attached, writable, and the sole PTY resize
  authority while both participants receive output and may submit input.
- Native links are not produced for projected remote Agents because their
  external Session identity and working directory intentionally remain on the
  owner Node. Remote Agents remain unlinked to this Node's runtime.
- Lifecycle evidence, external Session identity, and working directory remain
  absent from remote cached projections.
- Browser polling stops while the page is hidden and resumes on visibility or
  focus; the gateway's background projection refresh continues independently.
- The browser theme picker includes the stock Omarchy color palettes without
  depending on an Omarchy installation. It stores only the selected theme slug
  in browser-local storage and defaults to Catppuccin.
- A temporary daemon failure retains the last bounded projection with
  `daemon_connected: false` until a later refresh succeeds.

## Security And Privacy

The gateway and Shared Harness Runtime bind only to `127.0.0.1`; there is no
option to bind a LAN or tailnet address. Boomux does not configure or authenticate
the private access layer. That layer owns TLS, authentication, and ACLs
appropriate for both the bounded dashboard and OpenCode's
full-control origin. Public exposure is outside this design.

Native handoff URLs expose either the local working-directory encoding and
canonical Session ID to the destination OpenCode origin, or the opaque Claude
bridge ID to `claude.ai`.
API responses carry
`Cache-Control: no-store`. The service worker caches only HTML, JavaScript, CSS,
the Ghostty VT WebAssembly module, the manifest, and the icon; it never handles
`/api/` requests. Browser storage does not persist Agent snapshots.

The HTTP API is allowlisted. Attention dismissal accepts same-origin JSON and
either clears the gateway's exact local finished marker or submits the existing
revision-conditional daemon attention acknowledgment. Cross-origin JSON receives
no CORS authorization, non-JSON requests are rejected, and remote projected
Agents cannot be mutated.

Terminal authorization accepts exact identity and dimensions only for a current
local Agent card. The gateway retains at most 64 grants for 30 seconds.
The browser sends the one-use token as a secondary WebSocket subprotocol; it is
never put in the URL. Upgrade requires the fixed protocol and an exact loopback
or active Tailscale dashboard Origin. The gateway requests protocol-44
collaborative exact-run attachment with no restart or environment payload and
never downgrades to takeover on an older daemon. It translates only terminal
input, output, resize, focus, detach, and reconnect and never exposes the daemon
attachment token. The daemon's attached profile and later primary resize frames
set the browser renderer's logical grid. Browser-originated resize frames cannot
change the daemon PTY. Frames are bounded to 1 MiB, each
bridge queue to eight entries, and the gateway to four active browser terminals;
the daemon independently admits at most four collaborators per Shell.

Terminal control is equivalent to remote shell access. Restrict the dashboard to
trusted users through the private access layer. Terminal bytes are not included
in HTTP snapshots, browser storage, service-worker caches, logs, Agent state, or
remote Node projections, and they are never lifecycle evidence.

The OpenCode origin is a separate full-control application. Set a nonempty
`OPENCODE_SERVER_PASSWORD` unless the private access layer provides the intended
authentication boundary. Keep both listeners on loopback and never publish them
publicly. Boomux deliberately uses an external link rather than an iframe or
reverse proxy so this boundary remains visible.

## MVP Endpoints

- `GET /api/snapshot` returns the current Node-qualified Agent cards, counts, and
  optional exact native OpenCode or Claude handoffs. Every current local Agent
  may independently qualify for terminal control.
- `POST /api/attention/dismiss` requires JSON containing the exact local
  `node_id`, `agent_id`, and `observation_revision`. It clears a matching
  ephemeral marker or acknowledges matching durable attention.
- `POST /api/terminal/authorize` requires exact Node, Agent, Shell, and run
  identities plus bounded terminal dimensions and returns one short-lived grant.
- `GET /api/terminal` upgrades only with an allowlisted Origin, the
  `boomux.terminal.v1` subprotocol, and the one-use grant subprotocol.

These shapes are intentionally experimental. Future native handoffs must
preserve each harness's runtime ownership and exact Session identity rather than
introducing transcript adapters or treating terminal screens as authoritative
messages.

## Future Terminal Work

The exact-run browser terminal is available to every current local Agent,
independent of integration name. Generalizing it to owner-routed remote Nodes
remains separate work. Read-only observers would require a distinct daemon
observer contract; collaborative participants are writable and must not be
treated as observers. See
[`brainstorms/2026-08-18-mobile-web-terminal.md`](brainstorms/2026-08-18-mobile-web-terminal.md).

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
that output is authoritative locally.

The MVP is not a structured chat client. Boomux does not currently project host
conversation messages, tool calls, permission requests, or question forms.
Rendered Shell output is terminal state and is labeled as such.

## Start It

Run the gateway on its fixed loopback interface:

```console
boomux web
```

The default URL is `http://127.0.0.1:3737`. Select another loopback port with
`--port`.

To make it available only inside a tailnet, proxy the loopback service with
Tailscale Serve rather than Funnel:

```console
tailscale serve --bg http://127.0.0.1:3737
```

Open the HTTPS URL printed by Tailscale on the phone. Use the browser's **Add to
Home Screen** action to install the progressive web app.

For an exact user restriction, start Boomux with the login Tailscale reports for
that user:

```console
boomux web --trusted-user you@example.com
```

Every request must then contain the exact `Tailscale-User-Login` header inserted
by Tailscale Serve. Direct requests whose exact HTTP Host is `127.0.0.1` or
`localhost` remain available for local verification because they cannot carry
that header. A present mismatched identity is always rejected,
including on a loopback Host. Tagged devices do not receive Tailscale user
identity headers and therefore cannot satisfy this mode through the Serve URL.

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
- After the first browser baseline, a local current Agent observed moving from
  `working` to `idle` receives an ephemeral finished alert. It remains while the
  same Agent stays idle and current. It is not fabricated durable attention,
  does not arise from a remote projection, and is lost when the PWA reloads.
- Active counts require both a working or blocked observation and an exact match
  to the Shell's current run; retained historical observations never become live
  merely because their state label remains active-looking.
- Opening an Agent does not acknowledge attention or change lifecycle state.
- Local terminal output is capped at 256 KiB and is read only when the durable
  Shell still has the Agent's exact run as its current run.
- A historical Agent never displays output from a later run of the same Shell.
- Remote terminal output, lifecycle evidence, external Session identity, and
  working directory remain absent from cached projections.
- Polling stops while the page is hidden and resumes on visibility or focus.

## Security And Privacy

The gateway binds only to `127.0.0.1`; there is no option to bind a LAN or
tailnet address. Tailscale Serve terminates HTTPS and applies tailnet access
policy before proxying to loopback. Funnel would make the service public and is
outside this design.

`--trusted-user` is optional because some tailnets use device tags or access
rules rather than user identity headers. Without it, every process able to reach
the loopback listener and every tailnet identity allowed by the Serve policy can
read the dashboard. Other processes running as the same local user already have
access to the stronger owner-only Boomux socket, but untrusted local software
must still be considered when deciding whether to run the gateway.

Terminal output and lifecycle evidence can contain private source, paths,
commands, prompts, credentials, or model responses. API responses carry
`Cache-Control: no-store`. The service worker caches only HTML, JavaScript, CSS,
the manifest, and the icon; it never handles `/api/` requests. Browser storage
does not persist Agent snapshots or terminal output.

The HTTP API is an allowlisted projection. It cannot forward arbitrary daemon
requests, send terminal input, acquire a Shell controller, resume a Session,
acknowledge attention, stop processes, or mutate Node registration.

## MVP Endpoints

- `GET /api/snapshot` returns the current Node-qualified Agent cards and counts.
- `GET /api/agents/{node_id}/{agent_id}` returns one exact Agent detail, lifecycle
  timeline, and eligible local rendered terminal output.

These shapes are intentionally experimental. Future structured conversation
work should add owner-side integration capabilities and closed routed operations;
it must not parse terminal screens as authoritative messages or persist rich
content in the remote Node projection cache.

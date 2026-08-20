# Bind Shared Harness Sessions To Shell Runs

Status: Accepted

OpenCode 1 native TUIs and OpenCode Web need one live server generation to share
events and selected Sessions. Boomux therefore supervises one ephemeral
Node-local Shared Harness Runtime and binds each claimed external root Session
to one exact current ShellRun and ensured Agent Instance. One or more TUI holders
maintain that bounded Agent Session Claim. Claims are authority only: they are
not persisted, projected, event-published, or moved into Agent identity.

The user opens an ordinary managed login Shell and types bare zero-argument
interactive `opencode`. A Shell-scoped runtime `PATH` shim redirects only that
eligible invocation to stock `opencode attach`. Private bash, zsh, and fish
startup adapters reapply the shim after normal interactive configuration without
changing global shell files; arguments, subcommands,
noninteractive use, absolute paths, modified `PATH`, and use outside Boomux run
the real executable unchanged. A paired TUI plugin reactively claims selected
roots across switches and forks, while a server plugin resolves claims for
lifecycle reports. Source-visible OpenCode TUI API use is version-gated at the
OpenCode `1.18.18` compatibility point.

This design chooses a scoped shim, one Node server, and paired TUI/server plugins.
It preserves normal OpenCode behavior, exact ShellRun authority, shared phone and
desktop events, and a narrow daemon protocol. Conflicting same-Session Shells,
`--pure`, `--mini`, absolute binary paths, and modified `PATH` fail closed rather
than guessing ownership. Remote Agents remain unlinked.

Rejected alternatives were:

- Separate standalone v1 runtimes, because persisted Session equality does not
  synchronize live events.
- One server per Shell, because phone and native clients would fragment runtime
  state and multiply supervision and exposure.
- Command parsing, because reconstructing shell intent would be unsafe and would
  change arguments or subcommand behavior.
- Global shell configuration, because it would alter OpenCode outside eligible
  Boomux login Shells.
- A full OpenCode API proxy, because it would duplicate a broad, versioned,
  full-control security surface.
- Moving Agent identity when selection changes, because durable Agent history
  belongs to the exact ShellRun where it was observed.

The Shared Harness Runtime is ephemeral but survives terminal detach, restart of
`boomux web`, and graceful daemon handoff. Handoff transfers strict runtime identity
and generation but not claims; TUI holders reacquire them. Cold adoption requires
strict identity, and daemon stop terminates the runtime. The loopback OpenCode
origin remains full-control and must sit behind a private access layer that owns
TLS, authentication, and ACLs.

Cold recovery of one exact resumable OpenCode Agent starts a replacement
ShellRun through the internal shared launcher with that canonical Session ID.
The prior claim remains invalid; the replacement TUI establishes a new claim for
the new run. If shared preparation is unavailable, recovery falls back to the
standalone exact-Session command. This does not broaden the scoped user shim:
argument-bearing user invocations still execute stock OpenCode unchanged.

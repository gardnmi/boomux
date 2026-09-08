# Unified installation and user-driven Desktop updates

Status: Accepted, 2026-09-06.

The first-run integration UI below has since been replaced by core automatic
integration maintenance. See [Managed Integration Assets](../architecture.md#managed-integration-assets)
for the current policy. Desktop retains only an optional advanced terminal checklist;
the explicit application-update consent described here is unchanged.

A shared repository and release should provide a clear first installation and
whole-application update flow. The main installer offers Desktop (with the
matching Boomux backend) or CLI only. Explicit mode flags serve automation.
The Desktop script is embedded when rendering that release's main installer;
no second remote installer script is executed during selection.

Guided setup configures optional agent integrations and verifies the daemon. It
no longer requires an external terminal, installs Omarchy plugins, enables the
Hyprland Workspace layer, or writes compositor bindings. The README links to
manual Omarchy plugin installation. Historical owned-asset update/uninstall
behavior is retained for existing plugin installations.

Desktop's first-run card opens that same CLI setup as an embedded terminal and
can be skipped or reopened later. It does not duplicate integration management.

Desktop update discovery remains read-only and dismissal remains version-specific.
Explicit Update prepares a checksum-verified bundle; explicit Restart delegates
graceful handoff to the candidate CLI and waits for its new Desktop window before
closing the old one. Later retains preparation without changing running work.
Activation failure restores the old bundle and requests reverse handoff; failures
are reported. Standalone CLI files are never replaced by a Desktop update.

Installer-owned version directories and current/pending links identify update
ownership. Concurrent installs, changed selections, unsafe modes, unsupported
platforms, and missing runtime libraries fail before activation. PTYs, ShellRun
identities, and daemon lifecycle remain governed by core Boomux contracts.

Executable selection belongs to core Boomux. Protocol 52 adds a validated,
inode-pinned explicit executable handoff; ordinary restart continues to use the
running daemon's installed path. The Desktop worker requests the candidate path,
verifies the result through the CLI, and uses the same API for rollback. Older
daemons require a one-time upgrade through their existing installation owner;
Desktop refuses the switch before restarting them.

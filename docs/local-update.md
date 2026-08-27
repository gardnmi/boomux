# Local Update Contract

> **Status: Current contract.** This document governs local release discovery
> and executable replacement. Remote Node helper replacement remains governed by
> `boomux node upgrade` and `docs/remote-nodes.md`.

## Surface

`boomux update status` performs a bounded latest-release check. It does not start
or contact the daemon. `--json` uses the stable `boomux.cli/v1` command
`update.status`.

`boomux update` is human-only. It requires an interactive terminal, prints the
current and latest versions and exact install path, and requires explicit `yes`
confirmation. When the fixed `io.github.gardnmi.boomux` Omarchy plugin is
installed, that confirmation also names and authorizes its post-install update.
The command has no `--yes`, `--force`, automatic, scheduled, JSON mutation, or
remote mode. Integrations may inspect `update.status` and open the same guided
command in a native terminal; they must not download, authorize, or replace
Boomux themselves.

The non-protocol capabilities are `local_update_status` and
`guided_local_update`. Capability support does not imply that the current
installation is eligible.

## Eligibility

Self-update is available only when all of these are true:

- The binary was compiled by the official release workflow with distribution
  marker `github-release`.
- It is running on GNU/Linux `x86_64` or `aarch64`.
- Current-executable resolution identifies the canonical
  `$HOME/.local/bin/boomux` path.
- The home, `.local`, and `.local/bin` directories and executable are real,
  current-user-owned, non-group/world-writable filesystem objects.
- The executable is a nonempty bounded regular executable with one hard link.

Package paths, root-owned files, source and development builds, custom paths,
symlinks, special files, multiply linked files, unsafe directory chains, and
unprovable installations are ineligible. `boomux-bin` installs `/usr/bin/boomux`
through pacman and must be updated with the AUR helper that installed it. Boomux
never invokes privilege escalation or a package manager.

## Discovery And Integrity

The updater requests only the latest stable release metadata for the fixed
`gardnmi/boomux` repository. Metadata, downloads, redirects, protocols, output,
and elapsed time are bounded. Tags must be strict `vMAJOR.MINOR.PATCH` semantic
versions without prerelease or build metadata. A current version greater than
the latest release is `newer_than_latest` and is never downgraded.

The selected archive and checksum sidecar must exactly name the current release
tag and Rust target. The SHA-256 sidecar must contain one exact filename and one
64-digit digest. Extraction selects only the exact expected binary member, and
the extracted regular file is pinned before authorization becomes activation.
The trust root is the fixed GitHub repository and its HTTPS release publication;
the checksum detects corruption and inconsistent release assets but is not an
independent signing authority.

## Activation And Recovery

### Protocol-47 Alpha Break

v0.32/state schema 13 cannot use guided self-update or graceful daemon handoff
into the schedule-free protocol-47 release. Handoff H8 deliberately rejects the
v0.32 H7 manifest, and protocol-47 clients, daemons, and Nodes do not negotiate
protocol 46. This is an explicit alpha-breaking cold upgrade, not a migration.

Before replacing a v0.32 installation:

1. Run `boomux daemon stop` with the v0.32 binary. This terminates every managed
   process and PTY.
2. Back up, then remove `state.json`, `global_workspaces.json`,
   `local_shell_transactions.log`, `node-cache.json`, and
   `selected-workspace.json` from `$XDG_STATE_HOME/boomux`, or
   `~/.local/state/boomux` when `XDG_STATE_HOME` is unset. The independent
   `node.json` identity and `node_registrations.json` routes may remain.
3. Remove the complete `[scheduling]` table from every active configuration
   layer. Also remove `scheduled_dispatch_failed` and `scheduled_interrupted`
   from `[notifications]` and remove the same two keys from
   `[notifications.sound]`. Apply this to the global
   `$XDG_CONFIG_HOME/boomux/config.toml` (default
   `~/.config/boomux/config.toml`) and any file selected by `BOOMUX_CONFIG`.
4. Install the protocol-47 binary and start Boomux with an ordinary command such
   as `boomux`.

There is no schedule migration, legacy schedule field, or graceful rollback
across this boundary. A retained schema-13 store or removed configuration key is
rejected rather than silently reinterpreted.

The candidate is written with `create_new` in the install directory, synchronized,
and required to report the exact expected `boomux VERSION`. Immediately before
activation, the installed baseline and candidate fingerprints are revalidated.
A hard-link backup retains the old inode, then a same-directory rename atomically
replaces the pathname and synchronizes the parent directory.

If no daemon was running before activation, none is started. The installed path
is revalidated against the candidate and the backup is removed.

If a daemon was running, it must belong to the same user and execute the exact
install path. On Linux, the updater resolves the unique current holder of the
bound listener through the kernel Unix-socket table and that user's bounded
`/proc` descriptor view; it does not treat listener credentials retained from a
prior handoff process as current process identity. The existing graceful handoff
transfers PTYs, process identities, runtime locks, event state, and reconnecting
attachments. Success requires the replacement daemon's exact handoff argv and
`/proc` executable device, inode, length, mode, and SHA-256 to match the pinned
candidate.

Before successful verification, any failure restores the backup pathname and
attempts a reverse graceful handoff to the restored executable. Recovery runs
even when directory synchronization also fails, and combined failures are
reported. Once candidate activation and daemon verification succeed, cleanup
failure cannot truthfully roll back the committed update; Boomux reports a
bounded warning and may leave the hidden backup for manual inspection.

After the executable update commits, an installed companion Omarchy plugin is
revalidated through `omarchy plugin list --json` and updated only by the fixed
exact-argument command `omarchy plugin update io.github.gardnmi.boomux --yes`.
Omarchy retains plugin lifecycle ownership. If the plugin is enabled, Boomux
then runs `omarchy restart shell` so the new plugin code is loaded. A plugin or
shell-restart failure is reported as a partial failure after clearly stating
that the Boomux executable update committed; it cannot roll back the committed
executable. A failed or timed-out Omarchy update command is reported as an
unknown plugin outcome because it may have mutated the plugin checkout before
failing. Missing Omarchy or an uninstalled plugin causes no plugin mutation.
An uninspectable optional plugin inventory is warned about and skipped rather
than blocking the Boomux executable update.

## Release And Package Channels

Release Please remains the version and tag authority. It creates a draft release
without exposing a tag, then its release build matrix
produces native `x86_64-unknown-linux-gnu` and
`aarch64-unknown-linux-gnu` archives, smoke-tests each on a native GitHub-hosted
runner, and publishes assets idempotently. Existing same-digest assets are left
unchanged; a same-name digest conflict fails instead of clobbering a release.
Before atomically publishing the completed draft and tag, the workflow
idempotently appends the reviewed installer and `boomux setup` handoff to the
generated GitHub release notes without replacing Release Please's content.

The AUR package is `boomux-bin` because it consumes prebuilt release artifacts.
Its architecture-specific sources pin both archive checksums. The AUR Git
repository contains generated `PKGBUILD`, `.SRCINFO`, and the post-install setup
reminder; this source repository retains the reviewed templates and deterministic
renderer. AUR publication is a separate maintainer action and never grants the
local updater ownership of pacman-managed files.

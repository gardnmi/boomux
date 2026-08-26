# Uninstall Contract

> **Status: Current contract.** This document governs removal of Boomux-owned
> local release assets and preserved user data.

## Surface

`boomux uninstall` is human-only and requires an interactive terminal. It shows
the exact executable, process impact, owned assets, modified assets that will be
preserved, and data policy before requiring explicit confirmation. It has no
`--yes`, JSON mutation, scheduled, or ad hoc remote mode. The non-protocol
capability is `guided_local_uninstall`.

`boomux uninstall --purge` additionally removes the standard Boomux state and
configuration directories after validating their complete bounded trees. It
does not remove a `BOOMUX_CONFIG` file outside the standard Boomux configuration
directory.

`boomux node uninstall NODE` is a separate human-only operation for an exact
registered Node. It authenticates the stored route, verifies the pinned Node ID,
requires a protocol-48 helper at the canonical owner-controlled user installation,
removes current integration assets while preserving modified assets, stops the
remote daemon, proves the socket absent, and removes the remote executable. It
preserves remote durable state, configuration, and Agent Skill. The local
registration is removed atomically only after confirmed remote removal; failure
or an ambiguous outcome retains the identity-pinned route. Shutdown is
conditional on the unchanged Node ID, and executable removal is bound to the
exact fingerprint authorized before confirmation. Disposable projection cleanup
follows best-effort and any retained cache is inaccessible without its registration.

## Ownership

Self-uninstall uses the same installation ownership boundary as self-update. It
is available only to an official GitHub release at the canonical
`$HOME/.local/bin/boomux` path through an owner-controlled directory chain.
Package-managed, root-owned, source, development, custom, symlinked, multiply
linked, changed, and otherwise unprovable executables are refused. Package
installations must be removed with their owning package manager.

Unchanged bundled integration assets and the unchanged Boomux Agent Skill are
removed. Modified or uninspectable assets are preserved and reported; uninstall
never uses integration force-removal implicitly. Host configuration and session
catalogs owned by OpenCode, Pi, Claude, Codex, or Kiro are not Boomux assets and
are never removed.

## Process And Data Safety

Before filesystem removal, uninstall discovers and stops bounded Boomux web
gateways, reconciles only Tailscale routes owned by those gateways, and requests
normal daemon shutdown. Daemon shutdown terminates managed Shell process groups,
PTYs, projection workers, and the Shared Harness Runtime. Uninstall then holds
the daemon ownership lock until cleanup is complete so another client cannot
start a daemon against partially removed state.

Without `--purge`, durable state, Node identity and registrations, Workspace and
Agent history, and Boomux configuration remain available to a later reinstall.
With `--purge`, the standard state and configuration trees must contain only
current-user-owned regular files and directories, no symbolic links or special
files, and no more than 16,384 entries. Validation completes before either tree
is recursively removed.

The executable fingerprint captured before authorization is revalidated after
process and asset cleanup. The executable is removed last and its parent
directory is synchronized. A changed executable fails closed instead of
removing the new or externally replaced file.

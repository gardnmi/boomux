# Limit Self-Update To Owned Release Binaries

Status: Accepted

Boomux needs one actionable local update flow without allowing an integration or
background process to replace arbitrary executables. Installation provenance and
filesystem ownership cannot be inferred from a version string or writability
alone. Package managers also own upgrade policy and must not be bypassed.

Boomux therefore provides passive `update status` discovery and an explicitly
confirmed `update` command, but no automatic updater. Mutation is limited to an
official `github-release` build at the current user's canonical
`~/.local/bin/boomux`, after owner, path-chain, file-type, link-count, target,
version, checksum, and concurrent-change validation. Package-manager, source,
development, root-owned, custom, and unknown installations are refused without
a force escape hatch.

An eligible update uses same-directory atomic replacement and retains the old
inode until candidate verification and, when applicable, graceful daemon handoff
complete. A running daemon must execute the same path, and the replacement
process is verified against the pinned candidate. The daemon remains stopped
when it was stopped before the update. Remote helper replacement remains a
separate identity-verified `node upgrade` operation.

Release Please remains the release authority and produces native x86_64 and
aarch64 GNU/Linux assets. AUR uses the separate package-manager-owned
`boomux-bin` channel, so those installations receive update guidance rather than
self-replacement. Integrations may launch the guided command but never download,
authorize, or install Boomux themselves.

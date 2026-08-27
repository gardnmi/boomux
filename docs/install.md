# Official Installation Contract

> **Status: Current contract.** This document governs the release-pinned
> `boomux-installer.sh` asset. Package managers and source builds retain their
> own installation ownership.

## Surface

Each stable GitHub release publishes `boomux-installer.sh` alongside the native
GNU/Linux archives. The stable entry point is:

```console
curl --proto '=https' --tlsv1.2 -LsSf \
  https://github.com/gardnmi/boomux/releases/latest/download/boomux-installer.sh | sh
```

The installer supports only `--no-setup`. Unknown arguments fail before network
or filesystem mutation. It never invokes privilege escalation or a package
manager.

## Platform And Destination

The installer accepts only x86_64 and aarch64 GNU/Linux release targets. `HOME`
must be absolute. `HOME`, `~/.local`, and `~/.local/bin` must be real,
current-user-owned directories that are not group/world-writable; missing local
directories are created owner-only. The sole destination is
`~/.local/bin/boomux`.

An existing file or symbolic link at that destination is never replaced, even
if it appears while the installer is running. The operator must use `boomux
update`, the owning package manager, or the workflow that owns a source or
custom installation. This keeps first installation separate from the local
update ownership and graceful daemon handoff contract.

## Integrity

The installer is rendered for one strict `vMAJOR.MINOR.PATCH` release tag. It
downloads only that tag's exact architecture archive and checksum sidecar from
the fixed `gardnmi/boomux` HTTPS repository. Curl is restricted to HTTPS with
TLS 1.2 or newer. The sidecar must validate through `sha256sum`, extraction must
produce the exact expected executable path, and the candidate must print the
embedded release version before installation.

Temporary files are removed on success, failure, or interruption. The verified
candidate is installed executable at the fixed destination only after all
checks pass.

## Setup Handoff

After installation, an available controlling terminal receives:

```text
Run the guided setup now? [Y/n]
```

The default runs the installed binary's human-only `setup` command with terminal
input and output attached directly to that controlling terminal. Declining,
passing `--no-setup`, or running without a controlling terminal prints the exact
absolute setup command instead.

Setup failure does not remove the verified Boomux installation. The installer
reports that installation succeeded, reports setup as incomplete, prints the
exact retry command, and exits nonzero.

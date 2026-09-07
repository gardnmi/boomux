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

The installer accepts `--desktop` or `--cli`, and `--no-setup` for the CLI setup
handoff. Without a choice it prompts on the controlling terminal, defaulting to
Desktop. Noninteractive callers must select a mode explicitly. Unknown or
conflicting arguments fail before network or filesystem mutation. Desktop
installation is embedded from `desktop/install.sh` at release rendering time, so
selection does not execute another remotely downloaded script. It never invokes privilege escalation or a package
manager.

## CLI Platform And Destination

The installer accepts only x86_64 and aarch64 GNU/Linux release targets. `HOME`
must be absolute. `HOME`, `~/.local`, and `~/.local/bin` must be real,
current-user-owned directories that are not group/world-writable; missing local
directories are created owner-only. The sole destination is
`~/.local/bin/boomux`.

The CLI is smoke-tested on the pinned Arch Linux compatibility baseline.
External-terminal opens require `xdg-terminal-exec`; guided setup and Desktop
embedded terminals do not. Omarchy integration is optional and manually installed.

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

### Harness Checklist

Guided setup presents a keyboard checklist of supported AI harness integrations.
Use **Up/Down** to move, **Space** to check or uncheck, and **Enter** to apply the
selection. **Esc**, **Ctrl+C**, or **Ctrl+D** cancels before integration changes.
The checklist scrolls with the selection and redraws on resize; terminals smaller
than 40 columns by 10 rows must be enlarged before confirming.

Available, current integrations start checked and need no installation. Missing
and modified integrations start unchecked, retaining setup's opt-in policy.
Checking a **REPLACE MODIFIED** entry explicitly authorizes replacement of that
integration's files. Missing or unverified harness hosts and unreadable integration
targets are disabled. Setup installs Boomux integrations, not the harness
applications themselves. Unchecking an entry never removes existing files.

Selected integrations install without individual yes/no prompts. The receipt
verifies the selected integrations rather than treating deselected
harnesses as missing requirements. Inspection or installation failures and
required harness restarts remain visible.
The separate Agent Skill keeps its own confirmation, and Desktop's final **[Y/n]** prompt still
controls removal of the dedicated setup Shell. Automation continues to use the
integration CLI commands; guided setup still requires an interactive terminal.

## Desktop Installation And Updates

The Desktop choice includes the matching CLI executable and an application-menu
launcher. It requires Linux x86_64 with glibc 2.39+ and checks fixed graphics
libraries and both executable versions before switching the active bundle.
Neither platform mode invokes package managers or privilege escalation.

The app welcome card offers optional agent setup inside an embedded terminal.
No installer or guided setup action installs Omarchy, modifies Hyprland bindings,
or enables its Workspace layer. See [Desktop releases](desktop/releases.md) for
versioned paths, update preparation, restart/rollback, and uninstall ownership.

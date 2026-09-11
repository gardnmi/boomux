# Official Installation Contract

For the everyday install-and-launch steps, use the
[Desktop guide](../desktop/README.md#install-release-builds).

**Jump to:** [Installer options](#surface) · [Integrity](#integrity) ·
[Integrations](#automatic-integration-management) · [Desktop updates](#desktop-installation-and-updates)

> **Status: Current contract.** This document governs the release-pinned
> `boomux-installer.sh` asset. Package managers and source builds retain their
> own installation ownership.

## Surface

Each stable GitHub release publishes `boomux-installer.sh` alongside the native
GNU/Linux archives and an experimental Apple Silicon macOS Desktop bundle.
The stable entry point is:

```console
curl --proto '=https' --tlsv1.2 -LsSf \
  https://github.com/gardnmi/boomux/releases/latest/download/boomux-installer.sh | sh
```

| Option | Behavior |
| --- | --- |
| `--desktop` | Install Desktop with the matching CLI |
| `--cli` | Install the standalone CLI |
| `--no-setup` | Skip the CLI setup handoff |
| No mode | Prompt on the controlling terminal, defaulting to Desktop |

Noninteractive callers must select a mode. Unknown or conflicting arguments fail
before network or filesystem mutation.

Desktop installation is embedded from `desktop/install.sh` (Linux) and
`desktop/install-macos.sh` (macOS) at release rendering time; mode selection does not fetch a second script. The installer never invokes
privilege escalation or a package manager.

## Desktop Platform Selection

The same `--desktop` command detects the host OS and architecture. Linux supports
x86_64 with the requirements and update ownership in the
[Desktop release contract](desktop/releases.md). macOS supports Apple Silicon
(`arm64`) on macOS 15 or newer. Intel Macs are rejected before downloading.

On macOS it downloads `boomux-desktop-aarch64-apple-darwin.zip` and its checksum
from the installer's pinned release. It verifies SHA-256 with `shasum`, the
ad-hoc bundle signature with `codesign`, ARM64 architecture, and both executable
versions. The app is experimental and **not notarized**.

The destination is `~/Applications/Boomux-<version>.app`. `HOME` and Applications
must be real, current-user-owned directories without group/world write access.
An installation lock serializes installers. Existing apps and symlinks are never
replaced. No command links, login service, or automatic launch are installed.
Open the app in Finder; if blocked, use System Settings → Privacy & Security →
Open Anyway. See the [Mac guide](platforms/macos-testing.md).

To install a newer Mac version, quit Desktop and rerun the command. Old app
versions remain available; do not remove an app whose bundled CLI still owns a
running daemon. App-bundle automatic updates and standalone macOS `--cli`
release assets are not provided.

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
TLS 1.2 or newer. The sidecar must validate through `sha256sum` (Linux) or `shasum` (macOS), extraction must
produce the exact expected executable path, and the candidate must print the
embedded release version before installation.

Temporary files are removed on success, failure, or interruption. The verified
candidate is installed executable at the fixed destination only after all
checks pass.

## Setup Handoff

This handoff applies to CLI installation. Desktop prepares bundled integrations
automatically and keeps advanced setup optional.

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

## Automatic Integration Management

The Boomux service makes one background integration-maintenance pass when it
starts, including after a committed update handoff. It installs missing
bundled integrations without requiring harness detection and updates unchanged
Boomux-managed integrations to the version bundled with that binary. Desktop
does not separately detect harnesses or ask users to install/update integrations.
Integration files are prepared even for tools not yet installed or absent from
the service's PATH, including remote machines started through SSH. Maintenance
does not execute harnesses or interactive shell startup scripts.
Already-running harnesses may need restarting to load changed files. Codex's
own hook trust approval is still required; Boomux does not bypass it.

| Task | Command |
| --- | --- |
| Opt out | `boomux integration uninstall <name>` |
| Opt back in | `boomux integration install <name>` |
| Run maintenance now | `boomux integration sync` |
| Inspect without changes | `boomux integration status <name>` |

Uninstall choices survive service restarts and updates. Removing a previously
managed asset by hand also leaves it off. Sync does not restart the service or
existing Shells. Maintenance failures are logged to service stderr; sync retries them.

### Ownership And Customizations

Versioned ownership receipts beside each integration record its installed
fingerprint, Boomux release, and enabled state. Older binaries do not automatically
downgrade newer managed assets. Current existing integrations are adopted; older
unrecognized files and user customizations are preserved rather than guessed to
be safe to overwrite. Such legacy files require one intentional installation
with `--force` before future updates can be managed. Codex receipts track only
Boomux's handlers, so unrelated hooks are preserved. Keep the hidden
`.boomux-managed.json` receipts to retain ownership and uninstall choices.

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

Desktop prepares integrations automatically. Optional advanced setup remains
available in Settings, inside an embedded terminal.
No installer or guided setup action installs Omarchy, modifies Hyprland bindings,
or enables its Workspace layer. See [Desktop releases](desktop/releases.md) for
versioned paths, update preparation, restart/rollback, and uninstall ownership.

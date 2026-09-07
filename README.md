# Boomux

**Persistent terminal Workspaces, with a native desktop app.**

Boomux keeps terminal processes running after windows close. Use Boomux Desktop
for an integrated tiling terminal, or the CLI with Ghostty, Alacritty, and other
external terminals. Durable Shells belong to coordinated Workspaces that can
span multiple Nodes.

<p align="center">
  <img src="assets/boomux-workspace-desktop.png" width="100%" alt="Boomux persistent side pane beside an active tiled Workspace">
</p>

> [!WARNING]
> For scripts, use only commands listed in `data.json_commands` by
> `boomux capabilities --json`, invoke them with `--json`, and parse their
> `boomux.cli/v1` envelopes. Human-readable output is not a compatibility contract.

## Quick Start

After [installing Boomux](#install-and-update), open **Boomux Desktop** from the
application menu or run `boomux-desktop`. Boomux is included and starts when
needed. Choose **Set up agents** in the welcome card to connect supported coding
agents, or **Start using Boomux** to skip it. Setup remains available in the menu.
It opens in an embedded terminal, previews configuration changes, and asks before
each agent integration or Agent Skill installation. Existing modified files are
preserved unless you explicitly approve replacement.

CLI users can run `boomux setup` for the same agent setup and daemon verification,
then `boomux` for the terminal dashboard. Setup does not require an external
terminal resolver, install Omarchy plugins, or change Hyprland settings.

### Optional Omarchy Plugin

For the separate Omarchy bar icon and persistent side pane, install
[omarchy-boomux](https://github.com/gardnmi/omarchy-boomux#readme) manually using
that project's instructions. It provides its own Workspaces, Shells, Agents, and
Nodes interface. Its README covers installation, keybindings, and optional
Hyprland Workspace presentation. It is not part of the Boomux installer or setup.
Existing plugin installations and user keybindings are left intact by setup.

## Native Desktop

Boomux Desktop is the experimental native GPUI client in [`desktop/`](desktop/).
It shares Boomux's version and release, with both executables included in the
Desktop download. CLI-only installation and remote Nodes retain their existing
lightweight packages.

Install the official Desktop bundle, available since v1.10.0:

```sh
curl -fsSL https://github.com/gardnmi/boomux/releases/latest/download/boomux-desktop-installer.sh | sh
```

The Desktop bundle currently targets Linux x86_64 with glibc, X11 or Wayland,
and system graphics libraries. CLI packages also support ARM64. See the
[Desktop guide](desktop/README.md) and [release contract](docs/desktop/releases.md).
For local development run `python3 desktop/scripts/run-dev.py` from this repository.

## Install And Update

### Requirements

- Desktop: Linux x86_64, glibc **2.39 or newer**, a Wayland or X11 session, and
  Vulkan support. Ubuntu 24.04+ and current Arch are the initial runtime baseline;
  older glibc distributions and musl/Alpine are not supported by this bundle.
- Desktop runtime libraries on Ubuntu/Debian: `libfontconfig1`,
  `libwayland-client0`, `libx11-6`, `libxcb1`, `libxcb-shape0`, `libxcb-xfixes0`,
  `libxkbcommon0`, `libxkbcommon-x11-0`, and `libvulkan1`, plus your GPU's Vulkan
  driver (such as `mesa-vulkan-drivers` for supported Mesa GPUs).
- Arch equivalents: `fontconfig`, `wayland`, `libx11`, `libxcb`, `libxkbcommon`,
  `libxkbcommon-x11`, `vulkan-icd-loader`, and your GPU's Vulkan driver.
- CLI packages: Linux x86_64 or ARM64. Native external-terminal opens additionally
  need `xdg-terminal-exec` and a terminal desktop entry. Those are not required for
  Desktop's embedded terminals or guided agent setup.
- An absolute `XDG_RUNTIME_DIR` for the daemon.

The installer diagnoses unsupported glibc and missing graphics libraries before
activating Desktop. It does not install system packages or use sudo. Rust and Zig
are only needed when building from source. Run the installed bundle's
`libexec/boomux-desktop --check-runtime` to repeat the graphics-library check.
Graphics-driver and display startup are validated separately by X11/Wayland smoke
tests; library availability alone cannot guarantee every GPU works.

Git is optional for persistence. `boomux doctor` retains CLI/external-terminal
checks, including Git. Optional Hyprland Workspace presentation additionally
requires a compatible active Hyprland session and `hyprctl`.

### Latest Release

```console
curl --proto '=https' --tlsv1.2 -LsSf \
  https://github.com/gardnmi/boomux/releases/latest/download/boomux-installer.sh | sh
```

After the first combined release, this command asks whether to install
**Desktop (Boomux included)** or **CLI only**, with Desktop selected by default.
Both choices download a release-pinned, checksum-verified package.

For an explicit choice, append `sh -s -- --desktop` or `sh -s -- --cli` in place
of `sh`. A noninteractive install requires an explicit choice. CLI automation can
use `sh -s -- --cli --no-setup` to skip the interactive setup handoff. Desktop
setup happens on first launch, not inside the installer.
CLI mode offers to run `boomux setup` immediately after installation.

Desktop installs a versioned bundle, an application-menu entry, and command links
under `~/.local/bin`. An existing independent CLI installation is preserved.
CLI-only installation refuses to overwrite existing files; use their owning
updater. If `~/.local/bin` is absent from PATH, add it to your shell configuration.
See the [installation contract](docs/install.md) for exact guarantees.

To inspect and install the release manually, use GitHub CLI (`gh`):

```console
case "$(uname -s):$(uname -m)" in
  Linux:x86_64) target=x86_64-unknown-linux-gnu ;;
  Linux:aarch64) target=aarch64-unknown-linux-gnu ;;
  *) printf 'unsupported operating system or architecture\n' >&2; exit 1 ;;
esac
version=$(gh release view --repo gardnmi/boomux --json tagName --jq .tagName)
gh release download "$version" --repo gardnmi/boomux \
  --pattern "boomux-$version-$target.tar.gz*"
sha256sum --check "boomux-$version-$target.tar.gz.sha256"
tar -xzf "boomux-$version-$target.tar.gz"
install -Dm755 "boomux-$version-$target/boomux" ~/.local/bin/boomux
~/.local/bin/boomux setup
```

### Update

Desktop offers **Update**, **View release**, and **Dismiss** for newer releases.
**Update** downloads and verifies the complete bundle in the background. Once
ready, choose **Restart now** or **Later**. Restart uses Boomux's graceful daemon
handoff, opens the updated window, and then closes the old window; running
terminals survive. Failed window startup restores the previous bundle and asks
Boomux to recover its previous daemon executable. Errors stay visible for retry.
Prepared updates survive app restarts; **Check for updates** revisits a dismissed
notice. There are no automatic downloads or restarts.

Rerunning the Desktop installer also updates the bundle. A running daemon is left
alone; when the new app opens, it offers to finish the daemon handoff. A separate
CLI install keeps its own update ownership. Desktop never replaces it.
Daemons installed before this release need a one-time upgrade through their
existing installation method before Desktop can take over updates. For an eligible
standalone release installation, use `boomux update`. Desktop leaves live terminals
running if that prerequisite is not met. See the [older-install migration steps](docs/desktop/releases.md#distribution-and-installation).

Eligible official release installations at `~/.local/bin/boomux` have an
explicit guided updater:

```console
boomux update status
boomux update
boomux doctor
```

The updater verifies the selected GitHub release asset and checksum before
replacing an eligible official installation. Compatible running daemons use
graceful handoff so managed processes and PTYs survive. If the Omarchy companion
plugin is installed, the same confirmation authorizes updating it after Boomux
and reloading it when enabled. Other installation types must be updated through
their original installer. Boomux never silently downgrades or enables automatic
updates.

Prefer `daemon restart` over `daemon stop`: stopping the daemon terminates every
managed process. Upgrade registered remote Nodes separately with
`boomux node upgrade NODE`.

Use `boomux daemon status` to inspect the daemon without starting it and
`boomux daemon start` to start it explicitly in the background. Starting an
already-running daemon succeeds without replacing it.

To remove an eligible official release installation, use `boomux uninstall`. Add
`--purge` only when you also intend to remove user data. Use
`boomux node uninstall NODE` for an identity-verified remote uninstall. See
[Uninstall](docs/uninstall.md) for ownership and preservation guarantees.

## Workspace Creation

Create a coordinated Workspace and its first Shell from a project directory:

```console
boomux workspace create my-project --node local --cwd . --open
```

This is the same atomic creation used in the quick start. Use the exact local
Node ID from `boomux node snapshot --json` only if `local` is ambiguous with a
registered alias. In Hyprland, when
`desktop.workspace_layer = "hyprland-special"` is enabled, Boomux places the
terminal in the Workspace's coordinator-derived special Workspace.

`boomux workspace create my-project` remains the empty-Workspace form. Add its
first Shell later with
`boomux shell create my-project --node local --cwd . --open`.

For a simpler current-terminal workflow:

```console
boomux . --name my-project
```

This shorthand creates or reuses a Node-local Workspace and attaches the current
terminal. Node-local Workspaces remain external until adopted or linked, so the
shorthand does not establish a coordinated desktop Workspace by itself. Run it
from a fresh, unmanaged terminal; path-opening shorthand is rejected inside an
existing Boomux Shell.

Open the native dashboard at any time:

```console
boomux ui
```

## Core Concepts

| Term | Meaning |
| --- | --- |
| **Node** | A durable host-local authority with stable identity, independent of the route used to reach it. |
| **Workspace** | A durable coordinator-owned place organizing Shells, Agent Instances, and launchers whose placements reference exact Node-local Workspaces. It is not an execution location and implies no default Node. |
| **Desktop Workspace Layer** | Optional local presentation of a coordinated Workspace as a Hyprland special Workspace derived from its coordinator ID. It owns no durable resources. |
| **Shell** | A durable Workspace slot with at most one current process run. Each live run owns its PTY; closing its terminal attachment does not close the Shell. |
| **Command** | The dashboard presentation of a Shell whose stored startup argument vector is nonempty. |
| **Launcher** | A durable exact-argument command invoked on every explicit Workspace open or restore. Each invocation is detached, ephemeral, and has no PTY. |
| **Agent Instance** | A durable identity for one external Agent session associated with one Shell run; process exit alone never establishes completion. |
| **External session ID** | An opaque harness identity retained on an Agent Instance for lifecycle correlation and exact recovery. It is not a Boomux resource or user-facing history object. |

Boomux preserves exact argument vectors and does not add shell interpolation to
launchers or adapters.

## What Persists

| Action | Result |
| --- | --- |
| Close a terminal window or quit the dashboard | Managed Shell runs keep running. |
| Close a Shell | Its current run is terminated and the Shell is removed. |
| Close a Workspace | A successful close terminates managed Shells and removes the Workspace and its retained resources; previously launched detached processes are unaffected. Unconfirmed placement removal leaves it visibly closing for explicit retry. |
| Restart the daemon gracefully | A compatible replacement preserves managed processes through handoff; failure rolls back to the old daemon. |
| Stop the daemon | Every managed process is terminated. |
| Crash or reboot | Managed Shell runs and PTYs are lost. Shells are restored pending; durable definitions and last-run metadata remain. Retained terminal text survives only when terminal-history persistence was enabled. Eligible Agent recovery may use the integration's native resume command. |

## Hyprland Workspace Layer

### Desktop Commands

```console
boomux desktop toggle
boomux desktop show <workspace-name-or-id>
boomux desktop next
boomux desktop previous
boomux desktop terminal
boomux desktop close
boomux desktop pop
boomux desktop return
boomux desktop gather
```

`desktop toggle`, `show`, `next`, and `previous` present Workspace layers without
invoking launchers. If a target layer has no windows, presentation may open its
existing Shell attachments and start pending or exited runs. Use the following
to reveal a layer and perform normal Workspace restore semantics:

```console
boomux workspace open <workspace-name-or-id> --show
```

`desktop terminal` creates a Shell in the visible Boomux layer; outside that
layer it opens an ordinary terminal. `desktop close` permanently closes the
focused Boomux Shell; outside the Boomux layer it closes the ordinary active
window. `pop` and `return` rearrange existing windows without changing Shell
ownership. `gather` also opens missing user-Shell attachments, but does not
invoke launchers or change ownership.

Use `boomux desktop --help` for command behavior. See
[Architecture](docs/architecture.md) for exact placement and restore invariants.

## Common Workflows

```console
# Select the coordinated Workspace used as CLI context
boomux workspace select my-project

# Create and open a Shell on the local Node using that selection
boomux shell create --node local --name dev --cwd . --open

# Change where future local Shells start
boomux workspace set-default-cwd my-project --node local --cwd .

# Store an exact detached launcher on the local Node
boomux launcher create editor --node local --cwd . -- zeditor .

# Inspect output without attaching
boomux read dev --lines 200

# Close a Shell permanently
boomux shell close dev --workspace my-project
```

Changing a placement default affects future Shell creation only when `--cwd` is
omitted. Existing Shell and Launcher working directories do not change, and new
Launchers do not inherit this default.

For coordinated `shell create --open` and atomic
`workspace create --node NODE --cwd DIRECTORY --open`, Boomux may prepare a
terminal while durable creation commits, but attachment remains gated until
creation succeeds. A failed create cannot start a Shell run.

Use `boomux --help` and `boomux <command> --help` for the complete current CLI.

## Native Dashboard

Run `boomux ui` in a terminal. The dashboard provides four primary views:

- **Workspaces**: coordinated tasks, placement state, attention, and ownership.
- **Agents**: current ShellRun-bound Agent lifecycle.
- **Shells**: durable Shell slots, commands, and exact run state.
- **Nodes**: registration, route health, compatibility, and upgrade actions.

Core keys:

| Keys | Action |
| --- | --- |
| Arrow keys or `h/j/k/l` | Navigate |
| `Tab`, `Shift-Tab`, `1`-`4` | Change view |
| `Enter` | Open or activate the selected item |
| `a`, `e`, `x` | Add, rename/edit, or close/remove where available |
| `/` or `:` | Open the command palette |
| `?` | Help |
| `q` | Quit after pending mutations finish |

Terminal previews are read-only.

## Web Dashboard

Serve the installable Agent dashboard on loopback:

```console
boomux web
```

When OpenCode is available, Boomux also ensures a daemon-supervised Shared
Harness Runtime on loopback and advertises its web UI. Use `--no-opencode-web`
to disable it, or `--opencode-web-url URL` to override the public origin for
that same runtime.

Run it detached or inspect/stop it explicitly:

```console
boomux web start
boomux web status
boomux web stop
```

Background start requires an already-running Boomux daemon; it never starts a
stopped daemon.

Publish through Tailscale only when intended:

```console
boomux web --tailscale
# or
boomux web start --tailscale
```

> [!WARNING]
> Web-terminal access is equivalent to shell access. OpenCode Web is a separate
> full-control origin. Restrict both to trusted users and configure their access
> boundaries deliberately.

See [Mobile Web](docs/mobile-web.md) for complete security, lifecycle, and
Tailscale behavior.

## Coding-Agent Integrations

Boomux bundles integrations for OpenCode, Pi, Claude Code, Codex, and Kiro CLI.
Inspect and configure one interactively:

```console
boomux integration list
boomux integration setup opencode
boomux integration status opencode
boomux integration verify opencode
```

Follow the printed host restart, Shell reopening, or hook-trust guidance after
installation. Integrations report lifecycle events; Boomux does not infer
completion from quiet terminal output or process exit, and does not present
conversations as transcripts. Modified or ineligible host invocations remain
untracked rather than receiving fabricated authority.

Install the vendor-neutral Agent Skill manually when desired:

```console
boomux skill install
```

## Remote Nodes

Add or upgrade a Node through the interactive workflow:

```console
boomux node add
boomux node list
boomux node upgrade <node>
boomux node reauthenticate <node>
```

`boomux node add` verifies the remote identity and requires confirmation before
installing or replacing Boomux. JSON and noninteractive requests never authorize
remote installation.
Forgetting a registration removes only the local route; it does not contact or
delete the remote Node.

Cached remote projections are presentation-only. Mutations require a live,
identity-verified owner connection and are never queued for later.

See [Remote Nodes](docs/remote-nodes.md) for routing, bootstrap, upgrade, and
failure semantics.

## Configuration

Boomux loads the global XDG configuration, then, when `BOOMUX_CONFIG` is set,
overlays that active writable layer field by field. Inspect or edit it with:

```console
boomux config path
boomux config validate
boomux config edit
```

`config edit` validates and atomically writes the active local configuration
layer. These commands never mutate remote Node configuration.

Common settings:

```toml
terminal = "Alacritty.desktop"

[projects]
roots = ["~/Projects", "~/Work"]
max_depth = 3

[dashboard]
follow_focused_terminal = true

[desktop]
# Default: "disabled"
# workspace_layer = "hyprland-special"

[recovery]
resume_agents = true
persist_terminal_history = false
```

The Hyprland Workspace layer, desktop and sound notifications, and terminal
history persistence are disabled by default. Enable the Workspace layer manually
only when using Hyprland presentation. Notification,
recovery, and Claude Remote Control settings are sampled at daemon start and
require `boomux daemon restart`; terminal, dashboard, project, and desktop
presentation settings do not.

## Security And Privacy

- Daemon sockets and durable stores are restricted to the current user.
- Attachment startup environments are validated but never persisted or projected.
- Persistent terminal history is opt-in and stores bounded plain text.
- Writable web-terminal access is equivalent to shell access.
- Remote projections never authorize offline writes.
- Omarchy plugins and coding-host integrations execute unsandboxed in their host
  processes; review them before installation.

## Compatibility And Automation

```console
boomux --version
boomux capabilities --json
```

Capabilities inspect the installed CLI without starting or contacting the daemon.
They report the CLI version, its built-in daemon protocol version, static
features, stable JSON commands, and validated integration host versions. Use
`boomux daemon status` and Node views for observed runtime compatibility.
Supported commands emit the `boomux.cli/v1` envelope when invoked with `--json`.

The Hyprland layer is local presentation built on coordinated Workspaces. It adds
no compositor identity to durable state, the daemon protocol, or `boomux.cli/v1`.
See [Architecture](docs/architecture.md) and [CLI JSON](docs/cli-json.md) for exact
versions, downgrade behavior, and protocol history.

## Limitations

- Boomux does not preserve a terminal emulator's tabs, panes, or window layout.
- Shells are not containers; they retain the privileges of their owner account.
- Browser terminal control is limited to exact current local Agent runs.
- Current Omarchy is the supported desktop environment. Other GNU/Linux desktop
  environments are best-effort; official binaries target x86_64 and aarch64.

## Further Documentation

- [Development Guide](DEVELOPMENT.md)
- [Architecture](docs/architecture.md)
- [Security Policy](SECURITY.md)
- [CLI JSON contract](docs/cli-json.md)
- [Local Update](docs/local-update.md)
- [Uninstall](docs/uninstall.md)
- [Remote Nodes](docs/remote-nodes.md)
- [Mobile Web](docs/mobile-web.md)
- [Event Stream](docs/event-stream.md)
- [Live PTY Handoff](docs/live-pty-handoff.md)
- [Lifecycle Validation](docs/lifecycle-validation.md)

## Development

See the [Development Guide](DEVELOPMENT.md) for prerequisites, isolated local
builds, the edit-build-run loop, testing requirements, pull requests, and the
release lifecycle.

## License

Boomux is licensed under the [MIT License](LICENSE). Resolved Rust dependencies
are checked against the repository's [license and advisory policy](deny.toml);
notices for embedded web assets are in [Third-Party Notices](THIRD_PARTY_NOTICES.md).

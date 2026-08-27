# Boomux

**Persistent Workspaces for native terminal windows.**

Boomux keeps Shells and commands running after terminal windows close. It groups
related work into coordinated Workspaces that can span independently
authoritative Nodes while continuing to use Ghostty, Alacritty, or another XDG
terminal as ordinary native windows.

> [!IMPORTANT]
> The Omarchy side pane shown below is provided by
> [omarchy-boomux](https://github.com/gardnmi/omarchy-boomux). Install both
> projects for that desktop integration. Boomux's CLI and native TUI work
> independently.

<p align="center">
  <img src="assets/boomux-workspace-desktop.png" width="100%" alt="Boomux persistent side pane beside an active tiled Workspace">
</p>

> [!WARNING]
> Human-facing commands and presentation may evolve. Supported automation output
> uses the stable `boomux.cli/v1` contract.

## Get Started

After installing the Boomux binary using one of the methods below, run the
guided setup:

```console
boomux setup
```

This is the recommended way to configure Boomux. The wizard discovers supported
Agent harnesses on `PATH`, offers each missing or modified lifecycle integration
separately, and can install the Boomux Agent Skill when it finds a harness. On
Omarchy it can install and enable `omarchy-boomux`, analyze existing keybindings,
warn which bindings would be overridden, and install the full desktop profile.

Every prompt defaults to no and modified assets require explicit replacement.
Rerunning setup is safe: compatible user-managed bindings are recognized, their
reinstall impact is shown, and they remain unchanged unless you confirm the
managed profile installation.

For automation, use the individual `integration status`, `integration install`,
and `skill install` commands instead. `boomux setup` intentionally requires an
interactive terminal and does not support `--json`.

## Install And Update

### Requirements

- Unix or Linux with an absolute `XDG_RUNTIME_DIR`.
- `xdg-terminal-exec` and an available terminal desktop entry.

Git is optional at runtime; without it, repository metadata is empty. The default
desktop Workspace layer additionally requires an active Hyprland session and
compatible `hyprctl`. Outside Hyprland, ordinary Boomux terminal opens remain
native windows.

The release recipe below requires GitHub CLI (`gh`), `sha256sum`, `tar`, and
`install`. Installing from Git requires Git and a Rust toolchain; compiling an
existing source tree requires only the Rust toolchain.

### Latest Release

```console
case "$(uname -m)" in
  x86_64) target=x86_64-unknown-linux-gnu ;;
  aarch64) target=aarch64-unknown-linux-gnu ;;
  *) printf 'unsupported architecture\n' >&2; exit 1 ;;
esac
version=$(gh release view --repo gardnmi/boomux --json tagName --jq .tagName)
gh release download "$version" --repo gardnmi/boomux \
  --pattern "boomux-$version-$target.tar.gz*"
sha256sum --check "boomux-$version-$target.tar.gz.sha256"
tar -xzf "boomux-$version-$target.tar.gz"
install -Dm755 "boomux-$version-$target/boomux" ~/.local/bin/boomux
boomux doctor
```

To install the current development branch instead of a published release:

```console
cargo install --git https://github.com/gardnmi/boomux --locked --root "$HOME/.local"
```

This installs `boomux` at `~/.local/bin/boomux`, matching the release recipe.
Ensure `~/.local/bin` is in `PATH`, then run `boomux setup`. Source installations
may include unreleased changes; use `boomux --version` and
`boomux capabilities --json` to inspect the installed build.

### Update

Official release binaries installed at `~/.local/bin/boomux` have an explicit
guided updater:

```console
boomux update status
boomux update
boomux doctor
```

The updater verifies the fixed GitHub release, exact architecture asset, and
SHA-256 checksum before replacing the executable. If the daemon is running, it
uses graceful handoff so managed processes and PTYs survive. Package-manager,
source, development, root-owned, custom-path, and unsafe installations are never
self-replaced, and Boomux never silently downgrades. Automatic updates are not
enabled.

> [!CAUTION]
> Upgrading from v0.32 to v1.x crosses an incompatible protocol and state
> boundary. It requires a cold upgrade that terminates managed processes. Follow
> the exact [local update procedure](docs/local-update.md#protocol-47-alpha-break).

Prefer `daemon restart` over `daemon stop`: stopping the daemon terminates every
managed process. Upgrade registered remote Nodes separately with
`boomux node upgrade NODE`.

To remove an official release installation, use `boomux uninstall`. Add
`--purge` only when you also intend to remove user data. Use
`boomux node uninstall NODE` for an identity-verified remote uninstall. See
[Uninstall](docs/uninstall.md) for ownership and preservation guarantees.

## Create Your First Workspace

Create a coordinated Workspace and its first Shell from a project directory:

```console
boomux workspace create my-project
boomux shell create my-project --cwd . --open
```

With only the local Node eligible, the Shell establishes its placement and opens
in a native terminal. If multiple Nodes are eligible, add `--node NODE`. In
Hyprland, the default desktop adapter places the terminal in the Workspace's
named Boomux special Workspace.

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
boomux
```

## Core Concepts

| Term | Meaning |
| --- | --- |
| **Node** | A durable host-local authority with stable identity, implemented by a replaceable Boomux daemon. |
| **Workspace** | A durable task grouping. A coordinated Workspace explicitly links owner-Node placements. |
| **Desktop Workspace Layer** | Local presentation of a coordinated Workspace as a Hyprland special Workspace derived from its immutable coordinator ID. It owns no durable resources. |
| **Shell** | A durable Workspace slot with at most one current process run. Each live run owns its PTY; closing its terminal attachment does not close the Shell. |
| **Command** | A Shell with stored exact arguments instead of an interactive login command. |
| **Launcher** | A detached exact-argument command invoked by an explicit Workspace open or restore. Desktop navigation alone never invokes it. |
| **Agent Instance** | A durable identity for one external Agent session associated with one Shell run; process exit alone never establishes completion. |

Boomux preserves exact argument vectors and does not add shell interpolation to
launchers or adapters.

## What Persists

| Action | Result |
| --- | --- |
| Close a terminal window | Its managed Shell run keeps running. |
| Quit the native dashboard | Managed processes keep running. |
| Close a Shell | Its current run is terminated and the Shell is removed. |
| Close a Workspace | Its managed Shells terminate and its launchers are removed. Unresolved remote placement removal leaves the Workspace visibly closing for explicit retry. |
| Restart the daemon gracefully | A compatible replacement preserves managed processes through handoff; failure rolls back to the old daemon. |
| Stop the daemon | Every managed process is terminated. |
| Crash or reboot | Live processes are lost. Durable definitions remain, and later opens start new runs. |

## Hyprland Workspace Layer

In an active Hyprland session, Boomux presents terminal-bearing coordinated
Workspaces on demand as named special Workspaces. Empty and launcher-only
Workspaces have no terminal layer to show.

The adapter defaults on. To opt out while using Hyprland:

```toml
[desktop]
workspace_layer = "disabled"
```

This setting is read by local CLI presentation paths and does not require a
daemon restart.

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

`desktop toggle` and `desktop show` navigate without invoking launchers. Use the
following to reveal a layer and perform normal Workspace restore semantics:

```console
boomux workspace open <workspace-name-or-id> --show
```

See [Architecture](docs/architecture.md) for exact navigation, placement, and
restore semantics.

## Common Workflows

```console
# Select the default coordinated Workspace
boomux workspace select my-project

# Create and open a Shell using that selection
boomux shell create --name dev --cwd . --open

# Store an exact detached launcher
boomux launcher create editor --cwd . -- zeditor .

# Inspect output without attaching
boomux read dev --lines 200

# Close a Shell permanently
boomux shell close dev --workspace my-project
```

`shell create --open` may prepare a terminal while durable creation commits, but
attachment remains gated until creation succeeds. A failed create cannot start a
Shell run.

Use `boomux --help` and `boomux <command> --help` for the complete current CLI.

## Native Dashboard

Run `boomux` in a terminal. The dashboard provides four primary views:

- **Workspaces**: coordinated tasks, placement state, attention, and ownership.
- **Agents**: current Agent lifecycle and canonical Sessions.
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

Run it detached or inspect/stop it explicitly:

```console
boomux web start
boomux web status
boomux web stop
```

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

Restart the coding-agent host after installation when instructed. Integrations
report lifecycle events; they do not infer completion from quiet terminal output
or parse conversations as transcripts. Modified or ineligible host invocations
remain untracked rather than receiving fabricated authority.

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

Setup verifies remote identity and requires confirmation before installation or
replacement. JSON and noninteractive requests never authorize installation.
Forgetting a registration removes only the local route; it does not contact or
delete the remote Node.

Cached remote projections are presentation-only. Mutations require a live,
identity-verified owner connection and are never queued for later.

See [Remote Nodes](docs/remote-nodes.md) for routing, bootstrap, upgrade, and
failure semantics.

## Configuration

Boomux loads the global XDG configuration and optionally overlays the file named
by `BOOMUX_CONFIG`, merging fields individually. Inspect or edit the active
writable layer with:

```console
boomux config path
boomux config validate
boomux config edit
```

`config edit` uses owner-only temporary files, validates the merged result, and
atomically replaces an owner-validated target; new files use mode `0600`. These
local human-only commands do not mutate remote Node configuration.

Common settings:

```toml
terminal = "Alacritty.desktop"

[projects]
roots = ["~/Projects", "~/Work"]
max_depth = 3

[dashboard]
follow_focused_terminal = true

[desktop]
# Default: "hyprland-special"
# workspace_layer = "disabled"

[recovery]
resume_agents = true
persist_terminal_history = false
```

Terminal history persistence is disabled by default because output can contain
secrets. Notifications default conservatively. Daemon-owned settings require
`boomux daemon restart`; local desktop presentation settings do not.

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
They report its version, supported daemon protocol version, static features,
stable JSON commands, and validated integration host versions. Use
`boomux daemon status` and Node views for observed runtime compatibility.
Supported automation commands emit the `boomux.cli/v1` envelope; human-only
interactive and compositor commands reject `--json`.

The Hyprland layer is local presentation built on coordinated Workspaces. It adds
no compositor identity to durable state, the daemon protocol, or `boomux.cli/v1`.
See [Architecture](docs/architecture.md) and [CLI JSON](docs/cli-json.md) for exact
versions, downgrade behavior, and protocol history.

## Limitations

- Boomux does not preserve a terminal emulator's tabs, panes, or window layout.
- Shells are not containers; they retain the privileges of their owner account.
- Agent completion is run-scoped and is never inferred solely from process exit.
- Remote cached state can be stale and is clearly marked non-actionable.
- Browser terminal control is limited to exact current local Agent runs.
- Boomux currently targets Unix/Linux desktop sessions.

## Further Documentation

- [Architecture](docs/architecture.md)
- [CLI JSON contract](docs/cli-json.md)
- [Local Update](docs/local-update.md)
- [Uninstall](docs/uninstall.md)
- [Remote Nodes](docs/remote-nodes.md)
- [Mobile Web](docs/mobile-web.md)
- [Event Stream](docs/event-stream.md)
- [Live PTY Handoff](docs/live-pty-handoff.md)
- [Lifecycle Validation](docs/lifecycle-validation.md)

## Development

Run the core repository checks:

```console
cargo fmt --all -- --check
cargo clippy --all-targets --all-features --locked -- -D warnings
cargo test --lib --bins --locked -- --test-threads=1
cargo test --test native_backend --locked -- --test-threads=1
bun test integrations/opencode/boomux.test.js integrations/opencode/boomux-tui.test.js integrations/pi/boomux.test.js
```

## License

Boomux is licensed under the [MIT License](LICENSE).

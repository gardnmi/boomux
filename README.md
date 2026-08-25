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
> Boomux is pre-1.0. Human-facing commands and presentation may evolve. Durable
> state uses explicit versioned migrations, and supported automation output uses
> the stable `boomux.cli/v1` contract.

## Install And Update

### Requirements

- Unix or Linux with an absolute `XDG_RUNTIME_DIR`.
- `xdg-terminal-exec` and an available terminal desktop entry.

Git is optional; without it, repository metadata is empty. The default desktop
Workspace layer additionally requires an active Hyprland session and compatible
`hyprctl`. Outside Hyprland, ordinary Boomux terminal opens remain native windows.

The release recipe below requires GitHub CLI (`gh`), `sha256sum`, `tar`, and
`install`. Building from source requires Git and a Rust toolchain.

### Latest Release

```console
version=$(gh release view --repo gardnmi/boomux --json tagName --jq .tagName)
gh release download "$version" --repo gardnmi/boomux \
  --pattern "boomux-$version-x86_64-unknown-linux-gnu.tar.gz*"
sha256sum --check "boomux-$version-x86_64-unknown-linux-gnu.tar.gz.sha256"
tar -xzf "boomux-$version-x86_64-unknown-linux-gnu.tar.gz"
install -Dm755 "boomux-$version-x86_64-unknown-linux-gnu/boomux" ~/.local/bin/boomux
boomux doctor
```

To install the current development branch instead of a published release:

```console
cargo install --git https://github.com/gardnmi/boomux --locked
```

Source installations may include unreleased changes. Run `boomux --version` and
`boomux capabilities --json` to inspect the installed build.

### Update

Replace the binary using the release steps above, then activate it without
terminating managed processes:

```console
boomux daemon restart
boomux doctor
```

Prefer `daemon restart` over `daemon stop`: stopping the daemon terminates every
managed process. Upgrade registered remote Nodes separately with
`boomux node upgrade NODE`.

## Quick Start

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
| **Node** | One independently authoritative Boomux daemon and its local resources. |
| **Workspace** | A durable task grouping. A coordinated Workspace explicitly links owner-Node placements. |
| **Desktop Workspace Layer** | Local presentation of a coordinated Workspace as a Hyprland special Workspace derived from its immutable coordinator ID. It owns no durable resources. |
| **Shell** | A durable Workspace slot with at most one current process run. Each live run owns its PTY; closing its terminal attachment does not close the Shell. |
| **Command** | A Shell with stored exact arguments instead of an interactive login command. |
| **Launcher** | A detached exact-argument command invoked by an explicit Workspace open or restore. Desktop navigation alone never invokes it. |
| **Agent Instance** | Run-scoped lifecycle state reported by an integration; not a permanent process-completion record. |
| **Agent Schedule** | Durable recurring Agent work owned by one Workspace and Node. New schedules are paused by default. |

Boomux preserves exact argument vectors and does not add shell interpolation to
launchers, adapters, or scheduled execution.

## What Persists

| Action | Result |
| --- | --- |
| Close a terminal window | Its managed Shell run keeps running. |
| Quit the native dashboard | Managed processes keep running. |
| Close a Shell | Its current run is terminated and the Shell is removed. |
| Close a Workspace | Its managed Shells terminate; its launchers, schedules, and persisted prompts are removed. Unresolved remote placement removal leaves the Workspace visibly closing for explicit retry. |
| Restart the daemon gracefully | A compatible replacement preserves managed processes through handoff; failure rolls back to the old daemon. |
| Stop the daemon | Every managed process is terminated. |
| Crash or reboot | Live processes are lost. Durable definitions remain, and later opens start new runs. |

## Hyprland Workspace Layer

In an active Hyprland session, Boomux presents terminal-bearing coordinated
Workspaces on demand as named special Workspaces. Empty, launcher-only, and
schedule-only Workspaces have no terminal layer to show.

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

- `toggle` shows or hides the selected Workspace. If the layer has no terminal
  windows, Boomux opens or reuses available user-Shell attachments without
  invoking launchers. It does not fill missing siblings once one window exists.
- `show` targets one exact coordinated Workspace with the same behavior.
- `next` and `previous` cycle active, non-closing coordinated Workspaces. Outside
  a Boomux layer they retain ordinary Hyprland Workspace navigation.
- `terminal` creates a Shell in the visible layer or opens a normal terminal
  outside one.
- `close` permanently closes the focused Boomux Shell inside its layer; outside
  one it closes the ordinary active window.
- `pop` floats the active window contextually. `return` moves one exactly
  identified Boomux terminal back to its owner layer.
- `gather` returns the target Workspace's existing Shell windows and opens
  missing user-Shell attachments. It does not invoke launchers.

`desktop show` is navigation, not a full restore. Use the following to reveal a
layer and perform normal Workspace restore semantics, including launchers and
all available placements:

```console
boomux workspace open <workspace-name-or-id> --show
```

With the layer enabled, TUI Workspace restore and individual Agent/Shell opens
use the same presentation. The TUI stays active in its original terminal and
refreshes when you return.

Boomux correlates adapter-opened windows using exact Node and Shell IDs encoded
in immutable initial terminal titles. Those IDs are visible to local compositor
inspection but are not credentials. Hyprland window addresses are ephemeral and
never persisted or treated as Boomux identity.

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

Run `boomux` in a terminal. The dashboard provides five primary views:

- **Workspaces**: coordinated tasks, placement state, attention, and ownership.
- **Shells**: durable Shell slots, commands, launchers, and exact run state.
- **Agents**: current Agent lifecycle and canonical Sessions.
- **Schedules**: definitions, scheduler health, and bounded execution history.
- **Nodes**: registration, route health, compatibility, and upgrade actions.

Core keys:

| Keys | Action |
| --- | --- |
| Arrow keys or `h/j/k/l` | Navigate |
| `Tab`, `Shift-Tab`, `1`-`5` | Change view |
| `Enter` | Open or activate the selected item |
| `a`, `e`, `x` | Add, rename/edit, or close/remove where available |
| `/` or `:` | Open the command palette |
| `?` | Help |
| `q` | Quit after pending mutations finish |

Terminal previews are read-only. Schedule prompts appear only during explicit
inspection or editing of an exact schedule.

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

Boomux reuses compatible Serve routes, rejects conflicts, and removes only routes
it created. It does not configure Tailscale grants or ACLs.

`boomux web` also attempts to start a shared OpenCode runtime. When active,
`--tailscale` publishes that separate full-control origin alongside the dashboard.
Set a nonempty `OPENCODE_SERVER_PASSWORD` before its first start unless tailnet
policy is intentionally the authentication boundary.

The dashboard can dismiss exact local attention and authorize a short-lived
**writable** web terminal for an exact current local Agent Shell run. Web-terminal
control is equivalent to remote shell access. Restrict access to trusted tailnet
users. OpenCode Web is a separate full-control origin and needs its own access
boundary. Remote-Node terminal control is not exposed.

See [Mobile Web](docs/mobile-web.md) for complete security, lifecycle, and
experimental HTTP behavior documentation.

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

Install the vendor-neutral Agent skill separately when desired:

```console
boomux skill install
```

## Scheduled Agent Work

Schedules are durable Workspace-owned recurring Agent prompts. New schedules are
paused unless `--enabled` explicitly authorizes future unattended host and tool
activity.

```console
boomux schedule create review \
  --workspace my-project \
  --cwd . \
  --integration opencode \
  --prompt-file ./review-prompt.txt \
  --weekdays 09:00

boomux schedule run review --workspace my-project
boomux schedule resume review --workspace my-project
```

Timed work runs only while the owning daemon and user session are active. Overlap
is skipped rather than queued, and Boomux adds no automatic retry or timeout.
Routine lists, events, notifications, and execution records omit prompt text;
exact `schedule inspect` and explicit TUI editing can disclose it.

See [Scheduled Agent Work](docs/scheduled-agent-work.md) for triggers, DST,
continuation identity, concurrency, retention, and privacy.

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

[scheduling]
max_concurrent = 4
```

Terminal history persistence is disabled by default because output can contain
secrets. Notifications and schedule notifications are independently configurable
and default conservatively. Daemon-owned settings require `boomux daemon restart`;
local desktop presentation settings do not.

## Security And Privacy

- Daemon sockets and durable stores are restricted to the current user.
- Attachment startup environments are validated but never persisted or projected.
- Schedule prompts are private durable content but are passed to the selected
  external host, which may retain or expose them according to its own behavior.
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
- [Remote Nodes](docs/remote-nodes.md)
- [Scheduled Agent Work](docs/scheduled-agent-work.md)
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

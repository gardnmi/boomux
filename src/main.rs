#![allow(dead_code)]

use std::collections::BTreeSet;
use std::env;
use std::error::Error;
use std::ffi::OsString;
use std::fmt::Write as _;
use std::fs;
use std::io::{self, BufRead, IsTerminal};
use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use clap::error::ErrorKind as ClapErrorKind;
use clap::{Args, CommandFactory, Parser, Subcommand, ValueEnum};
use uuid::Uuid;

use boomux::protocol::{
    AgentAuthority, AgentInstanceSnapshot, AgentRegistrationSpec, AgentReport, AgentState,
    EventCursor, ShellSnapshot, ShellSpec, ShellStatus, Snapshot, WorkspaceLauncherSnapshot,
    WorkspaceLauncherSpec, WorkspaceSnapshot,
};
use boomux::{attach, client, daemon, federation, protocol, ssh_bootstrap};

use crate::integration_management::{
    InstallOutcome, ensure_safe_directory, install_asset_at, regular_file_matches,
    require_absolute_root,
};

mod agent_attention_projection;
mod claude_hooks;
mod cli_output;
mod codex_hooks;
mod config;
mod dashboard_projection;
mod generated_names;
mod git;
mod hook_input;
mod host_session_source;
mod host_session_titles;
mod hyprland;
mod integration_management;
mod kiro_hooks;
mod mobile_web;
mod process_adapter;
mod projects;
mod session_projection;
mod setup;
mod tailscale_serve;
mod terminal;
mod tui;
mod uninstall;
mod update;
mod web_terminal;
mod workspace_selection;

const DASHBOARD_FALLBACK_REFRESH_INTERVAL: Duration = Duration::from_secs(1);
struct DashboardRefresh {
    watch: client::SnapshotWatch,
    last_snapshot_at: Instant,
    negotiated_protocol: u32,
}

impl DashboardRefresh {
    fn baseline(client: &client::Client) -> client::Result<Self> {
        let watch = client::SnapshotWatch::baseline(client)?;
        let negotiated_protocol = client.protocol_version()?;
        Ok(Self {
            watch,
            last_snapshot_at: Instant::now(),
            negotiated_protocol,
        })
    }

    fn snapshot(&self) -> &Snapshot {
        self.watch.snapshot()
    }

    fn check(&mut self, client: &client::Client) -> client::Result<Option<(Snapshot, bool)>> {
        match self.watch.poll(client) {
            Ok(poll) => {
                let handoff_completed = poll.events.iter().any(|event| {
                    matches!(&event.kind, protocol::DaemonEventKind::HandoffCompleted)
                });
                let protocol_changed = if handoff_completed {
                    let negotiated = client.protocol_version()?;
                    update_negotiated_protocol(&mut self.negotiated_protocol, negotiated)
                } else {
                    false
                };
                if poll.changed {
                    self.last_snapshot_at = Instant::now();
                    return Ok(Some((
                        self.watch.snapshot().clone(),
                        poll.stream_changed || protocol_changed,
                    )));
                }
                if self.last_snapshot_at.elapsed() < DASHBOARD_FALLBACK_REFRESH_INTERVAL {
                    return Ok(None);
                }
                if !self.watch.uses_events() {
                    *self = Self::baseline(client)?;
                    return Ok(Some((self.watch.snapshot().clone(), false)));
                }
                *self.watch.snapshot_mut() = client.snapshot()?;
                self.last_snapshot_at = Instant::now();
                Ok(Some((self.watch.snapshot().clone(), false)))
            }
            Err(error) => Err(error),
        }
    }

    fn refresh(&mut self, client: &client::Client) -> client::Result<(Snapshot, bool)> {
        let stream_id = self.watch.stream_id().map(str::to_owned);
        *self = Self::baseline(client)?;
        Ok((
            self.watch.snapshot().clone(),
            self.watch.stream_id() != stream_id.as_deref(),
        ))
    }
}

fn update_negotiated_protocol(current: &mut u32, negotiated: u32) -> bool {
    let changed = *current != negotiated;
    *current = negotiated;
    changed
}

const BOOMUX_SKILL: &str = include_str!("../.agents/skills/boomux/SKILL.md");
#[cfg(test)]
const BOOMUX_OPENCODE_PLUGIN: &str = integration_management::OPENCODE_ASSET;
#[cfg(test)]
const BOOMUX_PI_EXTENSION: &str = integration_management::PI_ASSET;
const NON_PROTOCOL_FEATURES: &[&str] = &[
    "opencode_lifecycle_plugin",
    "pi_lifecycle_extension",
    "claude_lifecycle_plugin",
    "process_adapters",
    "desktop_notifications",
    "sound_notifications",
    "integration_management",
    "persistent_workspace_selection",
    "create_and_open_shell",
    "atomic_workspace_shell_creation",
    "hyprland_special_workspaces",
    "contextual_desktop_terminal",
    "coordinated_shell_desktop_placement",
    "desktop_workspace_show",
    "node_reauthentication",
    "local_update_status",
    "guided_local_update",
    "guided_local_uninstall",
    "guided_setup",
    "explicit_daemon_start",
];
const LEGACY_BOOMUX_SHELLS_SKILL: &str = r#"---
name: boomux-shells
description: Read output and logs from Boomux workspace shells. Use when asked to inspect another shell by name, read shell2, examine terminal output, check logs from another terminal, or inspect a Boomux shell ID.
compatibility: Requires boomux on PATH. Shell-name lookup requires running inside a Boomux-managed shell.
metadata:
  author: boomux
  version: "1"
---

# Boomux Shells

Use Boomux to inspect output from another persistent shell without asking the
user to copy terminal contents.

## Read A Shell

When the user provides a shell name or shell ID, run:

```console
boomux read "<name-or-shell-id>" --lines 200
```

Use the returned text to answer the user's question. Increase `--lines` when
the relevant output is older. This reads Boomux's bounded, plain rendered VT
scrollback. It does not include ANSI sequences or a process's complete
historical log.

## Discover Shells

When the target is missing, unclear, or not found, run:

```console
boomux shells
```

Match the user's wording against the displayed shell names. Ask for
clarification only when multiple shells remain plausible.

Shell names are resolved within the current Boomux workspace. Exact Boomux
shell IDs can be read directly.
"#;
const READ_BYTES: usize = 1024 * 1024;

#[derive(Parser)]
#[command(
    version,
    about = "Native persistent terminal workspaces",
    subcommand_value_name = "SUBCOMMAND",
    after_help = "First run: boomux setup"
)]
struct Cli {
    /// Emit the stable boomux.cli/v1 envelope for commands advertised by capabilities
    #[arg(long, global = true)]
    json: bool,

    /// Connect ad hoc to one Boomux Node through OpenSSH
    #[arg(long, value_name = "TARGET")]
    remote: Option<String>,

    /// Open or create a persistent terminal in this directory
    #[arg(value_name = "PATH")]
    path: Option<PathBuf>,

    /// Name of the workspace group
    #[arg(short, long, requires = "path")]
    name: Option<String>,

    /// Open the terminal in a new window instead of attaching here
    #[arg(long = "new", requires = "path")]
    new_window: bool,

    /// XDG desktop entry to use for windows opened by this invocation
    #[arg(long, global = true, value_name = "DESKTOP_ENTRY")]
    terminal: Option<String>,

    /// Command and arguments to run instead of the login shell
    #[arg(last = true, num_args = 1.., requires = "path", value_name = "COMMAND")]
    startup_command: Vec<String>,

    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand)]
enum Commands {
    /// Open the interactive workspace dashboard
    Ui,
    /// Serve the mobile Agent dashboard and exact-run terminals on loopback
    Web {
        #[command(subcommand)]
        command: Option<WebCommands>,
        /// Loopback port for the HTTP server
        #[arg(long, default_value_t = 3737, value_parser = clap::value_parser!(u16).range(1..))]
        port: u16,
        /// Public base URL of an authenticated OpenCode Web server
        #[arg(long, value_name = "URL")]
        opencode_web_url: Option<String>,
        /// Loopback port for the managed OpenCode Web server
        #[arg(long, default_value_t = 4097, value_parser = clap::value_parser!(u16).range(1..))]
        opencode_web_port: u16,
        /// Do not start or advertise OpenCode Web
        #[arg(long, conflicts_with = "opencode_web_url")]
        no_opencode_web: bool,
        /// Expose the dashboard and OpenCode Web to the current Tailscale tailnet
        #[arg(long, conflicts_with = "opencode_web_url")]
        tailscale: bool,
        #[arg(long, hide = true)]
        require_running_daemon: bool,
    },
    /// Check that Boomux's dependencies and daemon are available
    Doctor,
    /// Discover and configure local Agent harnesses and desktop integration
    Setup,
    /// Report stable integration capabilities without starting the daemon
    Capabilities,
    /// List all managed shells
    List,
    /// List shells in the current Boomux workspace
    Shells,
    /// Read retained output from a shell name or shell ID
    Read {
        target: String,
        #[arg(long, default_value_t = 200, value_parser = clap::value_parser!(u32).range(1..))]
        lines: u32,
        #[arg(long, requires = "after_revision")]
        run_id: Option<String>,
        #[arg(long, requires = "run_id")]
        after_revision: Option<u64>,
        #[arg(long, default_value_t = 0, requires = "after_revision")]
        wait_ms: u32,
    },
    /// Read a bounded batch of daemon events
    Events {
        #[arg(long)]
        after: Option<String>,
        #[arg(long, default_value_t = 256, value_parser = clap::value_parser!(u16).range(1..=256))]
        limit: u16,
        #[arg(long, default_value_t = 0)]
        wait_ms: u32,
    },
    /// Close a shell by name, shell ID, or latest focused Boomux terminal
    Close {
        #[arg(required_unless_present = "focused", conflicts_with = "focused")]
        target: Option<String>,
        /// Close the most recently focused Boomux terminal
        #[arg(long)]
        focused: bool,
    },
    /// Inspect and edit layered Boomux configuration
    Config {
        #[command(subcommand)]
        command: ConfigCommands,
    },
    /// Check for or install an official local Boomux release
    Update {
        #[command(subcommand)]
        command: Option<UpdateCommands>,
    },
    /// Remove this official Boomux release installation
    Uninstall {
        /// Also remove standard Boomux durable state and configuration
        #[arg(long)]
        purge: bool,
    },
    /// Discover configured projects
    Project {
        #[command(subcommand)]
        command: ProjectCommands,
    },
    /// Manage workspaces
    Workspace {
        #[command(subcommand)]
        command: WorkspaceCommands,
    },
    /// Navigate Boomux Workspaces through an optional desktop layer
    Desktop {
        #[command(subcommand)]
        command: DesktopCommands,
    },
    /// Manage this Boomux Node identity
    Node {
        #[command(subcommand)]
        command: NodeCommands,
    },
    /// Manage shells
    Shell {
        #[command(subcommand)]
        command: ShellCommands,
    },
    /// Manage workspace launchers
    Launcher {
        #[command(subcommand)]
        command: LauncherCommands,
    },
    /// Inspect and report external agent runtime state
    Agent {
        #[command(subcommand)]
        command: AgentCommands,
    },
    /// Inspect and acknowledge blocked or completed Agent work
    Attention {
        #[command(subcommand)]
        command: AttentionCommands,
    },
    /// Test configured desktop and sound notification delivery
    Notification {
        #[command(subcommand)]
        command: NotificationCommands,
    },
    /// Legacy Agent Session commands retained for argument compatibility
    #[command(hide = true)]
    Session {
        #[command(subcommand)]
        command: SessionCommands,
    },
    /// Inspect and install supported harness integrations
    Integration {
        #[command(subcommand)]
        command: IntegrationCommands,
    },
    /// Manage the vendor-neutral Boomux Agent Skill
    Skill {
        #[command(subcommand)]
        command: SkillCommands,
    },
    /// Manage the Boomux OpenCode integration
    Opencode {
        #[command(subcommand)]
        command: OpenCodeCommands,
    },
    /// Manage the Boomux Pi integration
    Pi {
        #[command(subcommand)]
        command: PiCommands,
    },
    /// Receive Claude Code lifecycle hooks
    #[command(hide = true)]
    Claude {
        #[command(subcommand)]
        command: ClaudeCommands,
    },
    /// Receive Codex lifecycle hooks
    #[command(hide = true)]
    Codex {
        #[command(subcommand)]
        command: CodexCommands,
    },
    /// Receive Kiro CLI lifecycle hooks
    #[command(hide = true)]
    Kiro {
        #[command(subcommand)]
        command: KiroCommands,
    },
    /// Open a shell in a new terminal window
    Open {
        shell_id: String,
        /// Exact registered Node alias or Node ID
        #[arg(long)]
        node: Option<String>,
        /// Coordinated Workspace to use for optional desktop placement
        #[arg(long, value_name = "COORDINATED_WORKSPACE")]
        workspace: Option<String>,
        #[arg(long)]
        title: Option<String>,
        #[arg(long)]
        takeover: bool,
    },
    /// Print the current Boomux workspace and shell name for prompt integrations
    #[command(hide = true)]
    Prompt,
    /// Manage the background PTY daemon
    Daemon {
        #[command(subcommand)]
        command: DaemonCommands,
    },
    #[command(name = "__attach", hide = true)]
    Attach {
        shell_id: String,
        #[arg(long)]
        node: Option<String>,
        #[arg(long)]
        takeover: bool,
        #[arg(long)]
        restart_exited: bool,
        #[arg(long)]
        expected_run_id: Option<String>,
    },
    #[command(name = "__await-attach", hide = true)]
    AwaitAttach {
        shell_id: String,
        #[arg(long)]
        gate: PathBuf,
        #[arg(long)]
        node: Option<String>,
    },
    #[command(name = "__resume-session", hide = true)]
    ResumeSession {
        session_id: String,
        #[arg(long)]
        node: Option<String>,
    },
    #[command(name = "__federation-stdio", hide = true)]
    FederationStdio,
    #[command(name = "__guided-node-add", hide = true)]
    GuidedNodeAdd,
    #[command(name = "__guided-node-upgrade", hide = true)]
    GuidedNodeUpgrade { selector: String },
    #[command(name = "__guided-node-reauthenticate", hide = true)]
    GuidedNodeReauthenticate { selector: String },
    #[command(name = "__bootstrap-activate", hide = true)]
    BootstrapActivate {
        transaction: String,
        expected_pid: u32,
        expected_protocol: u32,
        expected_executable: String,
        expected_socket_device: u64,
        expected_socket_inode: u64,
    },
    #[command(name = "__uninstall-remote", hide = true)]
    UninstallRemote {
        expected_node_id: String,
        expected_executable: String,
    },
    #[command(name = "__uninstall-fingerprint", hide = true)]
    UninstallFingerprint,
}

#[derive(Subcommand)]
enum WebCommands {
    /// Start the web dashboard as a detached background process
    Start {
        /// Loopback port for the HTTP server
        #[arg(long, default_value_t = 3737, value_parser = clap::value_parser!(u16).range(1..))]
        port: u16,
        /// Public base URL of an authenticated OpenCode Web server
        #[arg(long, value_name = "URL")]
        opencode_web_url: Option<String>,
        /// Loopback port for the managed OpenCode Web server
        #[arg(long, default_value_t = 4097, value_parser = clap::value_parser!(u16).range(1..))]
        opencode_web_port: u16,
        /// Do not start or advertise OpenCode Web
        #[arg(long, conflicts_with = "opencode_web_url")]
        no_opencode_web: bool,
        /// Expose the dashboard and OpenCode Web to the current Tailscale tailnet
        #[arg(long, conflicts_with = "opencode_web_url")]
        tailscale: bool,
    },
    /// Report web dashboard state without starting it
    Status {
        /// Loopback port of the web dashboard
        #[arg(long, default_value_t = 3737, value_parser = clap::value_parser!(u16).range(1..))]
        port: u16,
    },
    /// Stop the web dashboard without stopping the daemon
    Stop {
        /// Loopback port of the web dashboard
        #[arg(long, default_value_t = 3737, value_parser = clap::value_parser!(u16).range(1..))]
        port: u16,
    },
}

#[derive(Subcommand)]
enum ProjectCommands {
    /// List projects discovered from configured roots
    List {
        /// Exact registered Node alias or Node ID
        #[arg(long)]
        node: Option<String>,
    },
}

#[derive(Subcommand)]
enum ConfigCommands {
    /// Print the active writable configuration path
    Path,
    /// Validate all configured layers
    Validate,
    /// Edit and transactionally replace the active writable layer
    Edit,
}

#[derive(Subcommand)]
enum UpdateCommands {
    /// Report local installation and latest release status
    Status,
}

#[derive(Subcommand)]
enum NodeCommands {
    /// Verify and persist a remote Boomux Node route
    Add {
        alias: Option<String>,
        target: Option<String>,
    },
    /// List registered remote Nodes
    List,
    /// Upgrade a registered remote Node after interactive authorization
    Upgrade { selector: String },
    /// Uninstall Boomux from a registered remote Node
    Uninstall { selector: String },
    /// Reauthenticate the stored route to a registered remote Node
    Reauthenticate { selector: String },
    /// Inspect a registered remote Node by alias or exact Node ID
    Inspect { selector: String },
    /// Show the combined Node-qualified local and cached remote projection
    Snapshot { selector: Option<String> },
    /// Rename a registered remote Node alias at an exact revision
    Rename {
        selector: String,
        alias: String,
        #[arg(long)]
        revision: u64,
    },
    /// Verify and replace a registered SSH route at an exact revision
    Retarget {
        selector: String,
        target: String,
        #[arg(long)]
        revision: u64,
    },
    /// Forget a registration without contacting the remote Node
    Forget { selector: String },
    /// Assign this authority a new Node ID after exact confirmation
    Rekey,
}

#[derive(Subcommand)]
enum WorkspaceCommands {
    /// List workspaces
    List,
    /// Select the workspace used when context-required commands omit one
    Select { target: String },
    /// Show the persistently selected CLI workspace
    Current,
    /// Clear the persistently selected CLI workspace
    Clear,
    /// Create an empty workspace, or atomically create its first Shell with --node and --cwd
    Create {
        #[arg(required_unless_present = "node")]
        name: Option<String>,
        /// Exact eligible Node alias or Node ID for the first Shell
        #[arg(long, requires = "cwd")]
        node: Option<String>,
        /// Owner-resolved directory for the placement default and first Shell
        #[arg(long, requires = "node")]
        cwd: Option<PathBuf>,
        /// Open the created Shell after the atomic creation commits
        #[arg(long, requires = "node")]
        open: bool,
    },
    /// Open terminal windows and invoke launchers
    Open {
        /// Coordinated Workspace name/ID, or exact owner Workspace ID with --node
        #[arg(value_name = "WORKSPACE")]
        target: String,
        /// Exact registered Node alias or Node ID
        #[arg(long)]
        node: Option<String>,
        /// Show the coordinated Workspace's desktop layer while restoring it
        #[arg(long)]
        show: bool,
    },
    /// Show a workspace and its shells
    Inspect { target: String },
    /// Rename a workspace
    Rename { target: String, name: String },
    /// Change one placement's default working directory for future resources
    SetDefaultCwd {
        #[arg(value_name = "GLOBAL_WORKSPACE")]
        target: String,
        /// Exact Node alias or Node ID for an existing placement
        #[arg(long)]
        node: String,
        /// Owner-resolved default directory
        #[arg(long)]
        cwd: PathBuf,
    },
    /// Adopt an unlinked Node-local Workspace as a new coordinated Workspace
    Adopt {
        #[arg(value_name = "EXTERNAL_WORKSPACE")]
        target: String,
        /// Exact eligible Node alias or Node ID
        #[arg(long)]
        node: String,
    },
    /// Link an unlinked Node-local Workspace to a coordinated Workspace
    Link {
        #[arg(value_name = "GLOBAL_WORKSPACE")]
        target: String,
        #[arg(value_name = "OWNER_WORKSPACE")]
        owner: String,
        /// Exact eligible Node alias or Node ID
        #[arg(long)]
        node: String,
    },
    /// Close a workspace and all of its shells
    Close { target: String },
    /// Retry unresolved placement removals for a closing Workspace
    Retry {
        #[arg(value_name = "CLOSING_GLOBAL_WORKSPACE")]
        target: String,
    },
}

#[derive(Clone, Subcommand)]
enum DesktopCommands {
    /// Show or hide the selected Boomux Workspace layer
    Toggle,
    /// Select the next Boomux Workspace, or the next regular desktop workspace
    Next,
    /// Select the previous Boomux Workspace, or the previous regular desktop workspace
    Previous,
    /// Select an exact coordinated Boomux Workspace without restoring it
    Show {
        #[arg(value_name = "WORKSPACE")]
        target: String,
    },
    /// Create a Boomux Shell in the visible layer, or open a normal terminal
    Terminal,
    /// Close the focused Boomux Shell, or close the ordinary active window
    Close,
    /// Float the active window contextually without pinning it in a Boomux layer
    Pop,
    /// Return the active Boomux terminal to its coordinated Workspace layer
    Return,
    /// Gather all terminal windows into the selected or visible Boomux Workspace layer
    Gather,
}

#[derive(Subcommand)]
enum LauncherCommands {
    /// List launchers in a workspace
    List {
        #[arg(long, value_name = "NAME_OR_ID")]
        workspace: Option<String>,
    },
    /// Create a launcher in a workspace
    Create {
        name: String,
        /// Coordinated Workspace name or ID when --node is used
        #[arg(long, value_name = "WORKSPACE")]
        workspace: Option<String>,
        /// Exact eligible Node alias or Node ID
        #[arg(long)]
        node: Option<String>,
        #[arg(long, default_value = ".")]
        cwd: PathBuf,
        #[arg(last = true, num_args = 1.., required = true, value_name = "COMMAND")]
        command: Vec<String>,
    },
    /// Show launcher details
    Inspect {
        target: String,
        #[arg(long, value_name = "NAME_OR_ID")]
        workspace: Option<String>,
    },
    /// Invoke a launcher as a detached process
    Invoke {
        /// Launcher name/ID locally, or exact launcher ID with --node
        #[arg(value_name = "LAUNCHER")]
        target: String,
        #[arg(long, value_name = "NAME_OR_ID")]
        workspace: Option<String>,
        /// Exact registered Node alias or Node ID
        #[arg(long)]
        node: Option<String>,
    },
    /// Rename a launcher
    Rename {
        target: String,
        name: String,
        #[arg(long, value_name = "NAME_OR_ID")]
        workspace: Option<String>,
    },
    /// Remove a launcher without affecting previously launched applications
    Remove {
        target: String,
        #[arg(long, value_name = "NAME_OR_ID")]
        workspace: Option<String>,
    },
}

#[derive(Subcommand)]
enum ShellCommands {
    /// Suggest an unused generated name without reserving it
    SuggestName {
        /// Workspace name/ID locally, or exact owner Workspace ID with --node
        #[arg(value_name = "WORKSPACE")]
        workspace: Option<String>,
        /// Exact registered Node alias or Node ID
        #[arg(long)]
        node: Option<String>,
    },
    /// Create a pending shell in a workspace
    Create {
        /// Coordinated Workspace name or ID when --node is used
        #[arg(value_name = "WORKSPACE")]
        workspace: Option<String>,
        /// Exact eligible Node alias or Node ID
        #[arg(long)]
        node: Option<String>,
        #[arg(long)]
        name: Option<String>,
        #[arg(long)]
        cwd: Option<PathBuf>,
        /// Open the exact newly created shell in a native terminal
        #[arg(long)]
        open: bool,
        #[arg(last = true, num_args = 1.., value_name = "COMMAND")]
        command: Vec<String>,
    },
    /// Show shell details
    Inspect {
        target: String,
        #[arg(long, value_name = "NAME_OR_ID")]
        workspace: Option<String>,
    },
    /// Rename a shell
    Rename {
        target: String,
        name: String,
        #[arg(long, value_name = "NAME_OR_ID")]
        workspace: Option<String>,
    },
    /// Close a shell
    Close {
        target: String,
        #[arg(long, value_name = "NAME_OR_ID")]
        workspace: Option<String>,
    },
}

#[derive(Subcommand)]
enum AgentCommands {
    /// List agent instances
    List {
        #[arg(long, value_name = "NAME_OR_ID")]
        workspace: Option<String>,
    },
    /// Show an agent instance by exact ID
    #[command(alias = "get")]
    Inspect { agent_id: String },
    /// Wait for an exact Agent observation revision to advance
    Wait {
        agent_id: String,
        #[arg(long)]
        after_revision: u64,
        #[arg(long, default_value_t = 30_000)]
        wait_ms: u32,
    },
    /// Register an agent instance for a shell run
    Register(AgentRegistrationArgs),
    /// Ensure an idempotent agent instance for a shell run
    Ensure(AgentRegistrationArgs),
    /// Supervise one exact external agent process
    Supervise(AgentSuperviseArgs),
    /// Report state for an agent instance by exact ID
    Report {
        agent_id: String,
        #[arg(long)]
        shell_id: Option<String>,
        #[arg(long)]
        run_id: Option<String>,
        #[arg(long, value_enum)]
        state: CliAgentState,
        #[arg(long, value_enum)]
        authority: CliAgentAuthority,
        #[arg(long)]
        evidence: String,
        #[arg(long, value_parser = clap::value_parser!(u8).range(0..=100))]
        confidence: u8,
    },
    /// Record one host-observed Git working context for an exact Agent occurrence
    #[command(name = "observe-working-context", hide = true)]
    ObserveWorkingContext {
        agent_id: String,
        path: PathBuf,
        #[arg(long)]
        shell_id: Option<String>,
        #[arg(long)]
        run_id: Option<String>,
    },
}

#[derive(Subcommand)]
enum AttentionCommands {
    /// List outstanding attention in priority order
    List {
        #[arg(long, value_name = "NAME_OR_ID")]
        workspace: Option<String>,
    },
    /// Acknowledge one exact attention-raising observation
    Acknowledge {
        agent_id: String,
        #[arg(long)]
        observation_revision: u64,
    },
}

#[derive(Subcommand)]
enum NotificationCommands {
    /// Deliver a test notification through every configured channel
    Test {
        #[arg(value_enum)]
        reason: CliNotificationReason,
    },
}

#[derive(Clone, Copy, ValueEnum)]
enum CliNotificationReason {
    Blocked,
    Completed,
}

impl From<CliNotificationReason> for daemon::NotificationTestReason {
    fn from(reason: CliNotificationReason) -> Self {
        match reason {
            CliNotificationReason::Blocked => Self::Blocked,
            CliNotificationReason::Completed => Self::Completed,
        }
    }
}

#[derive(Subcommand)]
enum SessionCommands {
    /// Retired compatibility syntax
    #[command(hide = true)]
    List {
        #[arg(long, value_name = "NAME_OR_ID")]
        workspace: Option<String>,
        /// Exact registered Node alias or Node ID
        #[arg(long)]
        node: Option<String>,
    },
    /// Retired compatibility syntax
    #[command(alias = "get", hide = true)]
    Inspect {
        session_id: String,
        /// Exact registered Node alias or Node ID
        #[arg(long)]
        node: Option<String>,
    },
    /// Retired compatibility syntax
    #[command(hide = true)]
    Open {
        session_id: String,
        /// Exact registered Node alias or Node ID
        #[arg(long)]
        node: Option<String>,
        /// Coordinated Workspace to use for optional desktop placement
        #[arg(long, value_name = "COORDINATED_WORKSPACE")]
        workspace: Option<String>,
    },
    /// Retired compatibility syntax
    #[command(hide = true)]
    Resume {
        session_id: String,
        /// Exact registered Node alias or Node ID
        #[arg(long)]
        node: Option<String>,
    },
    /// Retired compatibility syntax
    #[command(hide = true)]
    Rename {
        session_id: String,
        name: String,
        #[arg(long)]
        revision: u64,
        /// Exact registered Node alias or Node ID
        #[arg(long)]
        node: Option<String>,
        #[arg(long)]
        operation_id: Option<String>,
    },
    /// Retired compatibility syntax
    #[command(hide = true)]
    ResetName {
        session_id: String,
        #[arg(long)]
        revision: u64,
        /// Exact registered Node alias or Node ID
        #[arg(long)]
        node: Option<String>,
        #[arg(long)]
        operation_id: Option<String>,
    },
    /// Retired compatibility syntax
    #[command(hide = true)]
    Hide {
        session_id: String,
        #[arg(long)]
        workspace: String,
        /// Exact registered Node alias or Node ID
        #[arg(long)]
        node: Option<String>,
    },
}

#[derive(Args)]
struct AgentSuperviseArgs {
    name: Option<String>,
    #[arg(long)]
    integration: String,
    #[arg(long)]
    external_session_id: String,
    #[arg(long)]
    shell_id: Option<String>,
    #[arg(long)]
    run_id: Option<String>,
    #[arg(last = true, num_args = 1.., required = true, value_name = "COMMAND")]
    command: Vec<String>,
}

#[derive(Args)]
struct AgentRegistrationArgs {
    name: Option<String>,
    #[arg(long)]
    integration: String,
    #[arg(long)]
    external_session_id: Option<String>,
    #[arg(long)]
    shell_id: Option<String>,
    #[arg(long)]
    run_id: Option<String>,
    #[arg(long, value_enum)]
    state: CliAgentState,
    #[arg(long, value_enum)]
    authority: CliAgentAuthority,
    #[arg(long)]
    evidence: String,
    #[arg(long, value_parser = clap::value_parser!(u8).range(0..=100))]
    confidence: u8,
}

#[derive(Clone, Copy, ValueEnum)]
enum CliAgentState {
    Unknown,
    Working,
    Blocked,
    Idle,
    Inactive,
    Done,
}

impl From<CliAgentState> for AgentState {
    fn from(state: CliAgentState) -> Self {
        match state {
            CliAgentState::Unknown => Self::Unknown,
            CliAgentState::Working => Self::Working,
            CliAgentState::Blocked => Self::Blocked,
            CliAgentState::Idle => Self::Idle,
            CliAgentState::Inactive => Self::Inactive,
            CliAgentState::Done => Self::Done,
        }
    }
}

#[derive(Clone, Copy, ValueEnum)]
enum CliAgentAuthority {
    LifecycleIntegration,
    ProcessAdapter,
    TerminalHeuristic,
}

impl From<CliAgentAuthority> for AgentAuthority {
    fn from(authority: CliAgentAuthority) -> Self {
        match authority {
            CliAgentAuthority::LifecycleIntegration => Self::LifecycleIntegration,
            CliAgentAuthority::ProcessAdapter => Self::ProcessAdapter,
            CliAgentAuthority::TerminalHeuristic => Self::TerminalHeuristic,
        }
    }
}

#[derive(Subcommand)]
enum SkillCommands {
    /// Install the Boomux CLI skill under ~/.agents/skills
    Install {
        #[arg(long)]
        force: bool,
    },
}

#[derive(Subcommand)]
enum OpenCodeCommands {
    /// Install the Boomux plugin in the global OpenCode configuration
    Install {
        #[arg(long)]
        force: bool,
    },
    /// Internal entry point for a scoped bare OpenCode invocation
    #[command(hide = true)]
    Shared {
        #[arg(long, hide = true)]
        session: Option<String>,
        #[arg(long, hide = true, default_value_t = 4097)]
        port: u16,
    },
    /// Coordinate OpenCode TUI selection claims
    #[command(hide = true)]
    Claim {
        #[command(subcommand)]
        command: OpenCodeClaimCommands,
    },
}

#[derive(Subcommand)]
enum OpenCodeClaimCommands {
    /// Ensure a claim for the selected root OpenCode Session
    Ensure {
        #[arg(long)]
        generation: String,
        #[arg(long)]
        holder: String,
        #[arg(long)]
        root_session_id: String,
        #[arg(long)]
        shell_id: Option<String>,
        #[arg(long)]
        run_id: Option<String>,
    },
    /// Release one exact claim holder
    Release {
        #[arg(long)]
        generation: String,
        #[arg(long)]
        holder: String,
        #[arg(long)]
        claim_id: String,
    },
    /// Report lifecycle state through the selected root Session claim
    Report {
        #[arg(long)]
        generation: String,
        #[arg(long)]
        root_session_id: String,
        #[arg(long, value_enum)]
        state: CliAgentState,
        #[arg(long, value_enum)]
        authority: CliAgentAuthority,
        #[arg(long)]
        evidence: String,
        #[arg(long, value_parser = clap::value_parser!(u8).range(0..=100))]
        confidence: u8,
    },
}

#[derive(Subcommand)]
enum PiCommands {
    /// Install the Boomux extension in the global Pi configuration
    Install {
        #[arg(long)]
        force: bool,
    },
}

#[derive(Subcommand)]
enum ClaudeCommands {
    /// Receive one lifecycle event on standard input
    #[command(hide = true)]
    Hook,
}

#[derive(Subcommand)]
enum CodexCommands {
    /// Receive one lifecycle event on standard input
    #[command(hide = true)]
    Hook,
    /// Launch Codex with run-scoped lifecycle authority
    #[command(hide = true)]
    Launch {
        #[arg(last = true, allow_hyphen_values = true)]
        arguments: Vec<OsString>,
    },
}

#[derive(Subcommand)]
enum KiroCommands {
    /// Receive one lifecycle event on standard input
    #[command(hide = true)]
    Hook,
    /// Launch Kiro v3 with run-scoped lifecycle authority
    #[command(hide = true)]
    Launch {
        #[arg(last = true, allow_hyphen_values = true)]
        arguments: Vec<OsString>,
    },
}

#[derive(Subcommand)]
enum IntegrationCommands {
    /// List integrations bundled with this Boomux binary
    List,
    /// Inspect host, asset, and runtime reporting status
    Status {
        integration: Option<integration_management::IntegrationId>,
        #[arg(long)]
        node: Option<String>,
    },
    /// Install one integration or every bundled integration
    Install {
        #[arg(required_unless_present = "all", conflicts_with = "all")]
        integration: Option<integration_management::IntegrationId>,
        #[arg(long, conflicts_with = "integration")]
        all: bool,
        #[arg(long)]
        force: bool,
        #[arg(long)]
        dry_run: bool,
        #[arg(long)]
        node: Option<String>,
    },
    /// Remove one integration or every bundled integration
    Uninstall {
        #[arg(required_unless_present = "all", conflicts_with = "all")]
        integration: Option<integration_management::IntegrationId>,
        #[arg(long, conflicts_with = "integration")]
        all: bool,
        #[arg(long)]
        force: bool,
        #[arg(long)]
        node: Option<String>,
    },
    /// Inspect, install, and explain verification for one integration
    Setup {
        integration: integration_management::IntegrationId,
        /// Accept the proposed installation without prompting
        #[arg(long)]
        yes: bool,
        /// Allow replacement of a modified integration asset
        #[arg(long)]
        force: bool,
        #[arg(long)]
        node: Option<String>,
    },
    /// Verify authoritative lifecycle reporting in a running host shell
    Verify {
        integration: integration_management::IntegrationId,
        #[arg(long, value_name = "ID")]
        shell: Option<String>,
        #[arg(long, default_value_t = 30_000)]
        wait_ms: u32,
        #[arg(long)]
        node: Option<String>,
    },
}

#[derive(Subcommand)]
enum DaemonCommands {
    /// Start the daemon in the foreground
    #[command(hide = true)]
    Run,
    /// Receive daemon ownership from a running Boomux process
    #[command(hide = true)]
    ReceiveHandoff {
        #[arg(long)]
        channel: i32,
    },
    /// Start the daemon in the background
    Start,
    /// Report whether the daemon is accepting requests
    Status,
    /// Replace the daemon without changing pending workspace state
    Restart,
    /// Stop the daemon and its managed shells
    Stop,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum OutputMode {
    HumanOnly,
    Json,
    PrivateJson,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct CommandDescriptor {
    key: &'static str,
    output: OutputMode,
}

macro_rules! command_keys {
    ($($variant:ident => ($key:literal, $output:ident)),+ $(,)?) => {
        #[derive(Clone, Copy, Debug, PartialEq, Eq)]
        enum CommandKey {
            $($variant),+
        }

        impl CommandKey {
            const ALL: &[Self] = &[$(Self::$variant),+];

            const fn descriptor(self) -> CommandDescriptor {
                match self {
                    $(Self::$variant => CommandDescriptor {
                        key: $key,
                        output: OutputMode::$output,
                    }),+
                }
            }
        }
    };
}

command_keys! {
    Ui => ("ui", HumanOnly),
    Web => ("web", HumanOnly),
    WebStart => ("web.start", Json),
    WebStatus => ("web.status", Json),
    WebStop => ("web.stop", Json),
    Doctor => ("doctor", HumanOnly),
    Setup => ("setup", HumanOnly),
    Capabilities => ("capabilities", Json),
    List => ("list", Json),
    Shells => ("shells", Json),
    Read => ("read", Json),
    Events => ("events", Json),
    Close => ("close", HumanOnly),
    ConfigPath => ("config.path", HumanOnly),
    ConfigValidate => ("config.validate", HumanOnly),
    ConfigEdit => ("config.edit", HumanOnly),
    UpdateStatus => ("update.status", Json),
    Update => ("update", HumanOnly),
    Uninstall => ("uninstall", HumanOnly),
    ProjectList => ("project.list", Json),
    WorkspaceList => ("workspace.list", Json),
    WorkspaceInspect => ("workspace.inspect", Json),
    WorkspaceCreate => ("workspace.create", Json),
    WorkspaceSetDefaultCwd => ("workspace.set-default-cwd", Json),
    Workspace => ("workspace", HumanOnly),
    Desktop => ("desktop", HumanOnly),
    NodeAdd => ("node.add", Json),
    NodeUpgrade => ("node.upgrade", HumanOnly),
    NodeUninstall => ("node.uninstall", HumanOnly),
    NodeReauthenticate => ("node.reauthenticate", HumanOnly),
    NodeList => ("node.list", Json),
    NodeInspect => ("node.inspect", Json),
    NodeSnapshot => ("node.snapshot", Json),
    NodeRename => ("node.rename", Json),
    NodeRetarget => ("node.retarget", Json),
    NodeForget => ("node.forget", Json),
    NodeRekey => ("node.rekey", HumanOnly),
    ShellSuggestName => ("shell.suggest-name", Json),
    ShellInspect => ("shell.inspect", Json),
    Shell => ("shell", HumanOnly),
    LauncherList => ("launcher.list", Json),
    LauncherInspect => ("launcher.inspect", Json),
    Launcher => ("launcher", HumanOnly),
    AgentList => ("agent.list", Json),
    AgentInspect => ("agent.inspect", Json),
    AgentRegister => ("agent.register", Json),
    AgentEnsure => ("agent.ensure", Json),
    AgentSupervise => ("agent.supervise", HumanOnly),
    AgentReport => ("agent.report", Json),
    AgentObserveWorkingContext => ("agent.observe-working-context", PrivateJson),
    AgentWait => ("agent.wait", Json),
    AttentionList => ("attention.list", Json),
    AttentionAcknowledge => ("attention.acknowledge", Json),
    NotificationTest => ("notification.test", HumanOnly),
    IntegrationList => ("integration.list", Json),
    IntegrationStatus => ("integration.status", Json),
    IntegrationInstall => ("integration.install", Json),
    IntegrationUninstall => ("integration.uninstall", Json),
    IntegrationSetup => ("integration.setup", HumanOnly),
    IntegrationVerify => ("integration.verify", Json),
    SessionList => ("session.list", Json),
    SessionInspect => ("session.inspect", Json),
    SessionRename => ("session.rename", Json),
    SessionResetName => ("session.reset-name", Json),
    SessionHide => ("session.hide", Json),
    SessionOpen => ("session.open", HumanOnly),
    SessionResume => ("session.resume", HumanOnly),
    Skill => ("skill", HumanOnly),
    Opencode => ("opencode", HumanOnly),
    OpencodeClaimEnsure => ("opencode.claim.ensure", PrivateJson),
    OpencodeClaimRelease => ("opencode.claim.release", PrivateJson),
    OpencodeClaimReport => ("opencode.claim.report", PrivateJson),
    Pi => ("pi", HumanOnly),
    ClaudeHook => ("claude.hook", HumanOnly),
    CodexHook => ("codex.hook", HumanOnly),
    CodexLaunch => ("codex.launch", HumanOnly),
    KiroHook => ("kiro.hook", HumanOnly),
    KiroLaunch => ("kiro.launch", HumanOnly),
    Open => ("open", HumanOnly),
    Prompt => ("prompt", HumanOnly),
    DaemonStatus => ("daemon.status", Json),
    Daemon => ("daemon", HumanOnly),
    Attach => ("attach", HumanOnly),
    ResumeSessionInternal => ("resume-session-internal", HumanOnly),
    UninstallRemote => ("uninstall-remote", HumanOnly),
    UninstallFingerprint => ("uninstall-fingerprint", HumanOnly),
}

impl Cli {
    fn command_descriptor(&self) -> CommandDescriptor {
        self.command_key().descriptor()
    }

    fn command_key(&self) -> CommandKey {
        match self.command.as_ref() {
            Some(Commands::Web {
                command: Some(WebCommands::Start { .. }),
                ..
            }) => CommandKey::WebStart,
            Some(Commands::Web {
                command: Some(WebCommands::Status { .. }),
                ..
            }) => CommandKey::WebStatus,
            Some(Commands::Web {
                command: Some(WebCommands::Stop { .. }),
                ..
            }) => CommandKey::WebStop,
            Some(Commands::Web { .. }) => CommandKey::Web,
            Some(Commands::Capabilities) => CommandKey::Capabilities,
            Some(Commands::List) => CommandKey::List,
            Some(Commands::Shells) => CommandKey::Shells,
            Some(Commands::Read { .. }) => CommandKey::Read,
            Some(Commands::Events { .. }) => CommandKey::Events,
            Some(Commands::Config {
                command: ConfigCommands::Path,
            }) => CommandKey::ConfigPath,
            Some(Commands::Config {
                command: ConfigCommands::Validate,
            }) => CommandKey::ConfigValidate,
            Some(Commands::Config {
                command: ConfigCommands::Edit,
            }) => CommandKey::ConfigEdit,
            Some(Commands::Update {
                command: Some(UpdateCommands::Status),
            }) => CommandKey::UpdateStatus,
            Some(Commands::Update { command: None }) => CommandKey::Update,
            Some(Commands::Uninstall { .. }) => CommandKey::Uninstall,
            Some(Commands::Project {
                command: ProjectCommands::List { .. },
            }) => CommandKey::ProjectList,
            Some(Commands::Workspace {
                command: WorkspaceCommands::List,
            }) => CommandKey::WorkspaceList,
            Some(Commands::Workspace {
                command: WorkspaceCommands::Inspect { .. },
            }) => CommandKey::WorkspaceInspect,
            Some(Commands::Workspace {
                command: WorkspaceCommands::Create { node: Some(_), .. },
            }) => CommandKey::WorkspaceCreate,
            Some(Commands::Workspace {
                command: WorkspaceCommands::SetDefaultCwd { .. },
            }) => CommandKey::WorkspaceSetDefaultCwd,
            Some(Commands::Node {
                command: NodeCommands::Add { .. },
            }) => CommandKey::NodeAdd,
            Some(Commands::Node {
                command: NodeCommands::List,
            }) => CommandKey::NodeList,
            Some(Commands::Node {
                command: NodeCommands::Upgrade { .. },
            }) => CommandKey::NodeUpgrade,
            Some(Commands::Node {
                command: NodeCommands::Uninstall { .. },
            }) => CommandKey::NodeUninstall,
            Some(Commands::Node {
                command: NodeCommands::Reauthenticate { .. },
            }) => CommandKey::NodeReauthenticate,
            Some(Commands::Node {
                command: NodeCommands::Inspect { .. },
            }) => CommandKey::NodeInspect,
            Some(Commands::Node {
                command: NodeCommands::Snapshot { .. },
            }) => CommandKey::NodeSnapshot,
            Some(Commands::Node {
                command: NodeCommands::Rename { .. },
            }) => CommandKey::NodeRename,
            Some(Commands::Node {
                command: NodeCommands::Retarget { .. },
            }) => CommandKey::NodeRetarget,
            Some(Commands::Node {
                command: NodeCommands::Forget { .. },
            }) => CommandKey::NodeForget,
            Some(Commands::Node {
                command: NodeCommands::Rekey,
            }) => CommandKey::NodeRekey,
            Some(Commands::Shell {
                command: ShellCommands::SuggestName { .. },
            }) => CommandKey::ShellSuggestName,
            Some(Commands::Shell {
                command: ShellCommands::Inspect { .. },
            }) => CommandKey::ShellInspect,
            Some(Commands::Launcher {
                command: LauncherCommands::List { .. },
            }) => CommandKey::LauncherList,
            Some(Commands::Launcher {
                command: LauncherCommands::Inspect { .. },
            }) => CommandKey::LauncherInspect,
            Some(Commands::Agent {
                command: AgentCommands::List { .. },
            }) => CommandKey::AgentList,
            Some(Commands::Agent {
                command: AgentCommands::Inspect { .. },
            }) => CommandKey::AgentInspect,
            Some(Commands::Agent {
                command: AgentCommands::Wait { .. },
            }) => CommandKey::AgentWait,
            Some(Commands::Agent {
                command: AgentCommands::Register(..),
            }) => CommandKey::AgentRegister,
            Some(Commands::Agent {
                command: AgentCommands::Ensure(..),
            }) => CommandKey::AgentEnsure,
            Some(Commands::Agent {
                command: AgentCommands::Supervise(..),
            }) => CommandKey::AgentSupervise,
            Some(Commands::Agent {
                command: AgentCommands::Report { .. },
            }) => CommandKey::AgentReport,
            Some(Commands::Agent {
                command: AgentCommands::ObserveWorkingContext { .. },
            }) => CommandKey::AgentObserveWorkingContext,
            Some(Commands::Attention {
                command: AttentionCommands::List { .. },
            }) => CommandKey::AttentionList,
            Some(Commands::Attention {
                command: AttentionCommands::Acknowledge { .. },
            }) => CommandKey::AttentionAcknowledge,
            Some(Commands::Notification {
                command: NotificationCommands::Test { .. },
            }) => CommandKey::NotificationTest,
            Some(Commands::Session {
                command: SessionCommands::List { .. },
            }) => CommandKey::SessionList,
            Some(Commands::Session {
                command: SessionCommands::Inspect { .. },
            }) => CommandKey::SessionInspect,
            Some(Commands::Session {
                command: SessionCommands::Rename { .. },
            }) => CommandKey::SessionRename,
            Some(Commands::Session {
                command: SessionCommands::ResetName { .. },
            }) => CommandKey::SessionResetName,
            Some(Commands::Session {
                command: SessionCommands::Hide { .. },
            }) => CommandKey::SessionHide,
            Some(Commands::Session {
                command: SessionCommands::Open { .. },
            }) => CommandKey::SessionOpen,
            Some(Commands::Session {
                command: SessionCommands::Resume { .. },
            }) => CommandKey::SessionResume,
            Some(Commands::Integration {
                command: IntegrationCommands::List,
            }) => CommandKey::IntegrationList,
            Some(Commands::Integration {
                command: IntegrationCommands::Status { .. },
            }) => CommandKey::IntegrationStatus,
            Some(Commands::Integration {
                command: IntegrationCommands::Install { .. },
            }) => CommandKey::IntegrationInstall,
            Some(Commands::Integration {
                command: IntegrationCommands::Uninstall { .. },
            }) => CommandKey::IntegrationUninstall,
            Some(Commands::Integration {
                command: IntegrationCommands::Setup { .. },
            }) => CommandKey::IntegrationSetup,
            Some(Commands::Integration {
                command: IntegrationCommands::Verify { .. },
            }) => CommandKey::IntegrationVerify,
            Some(Commands::Daemon {
                command: DaemonCommands::Status,
            }) => CommandKey::DaemonStatus,
            Some(Commands::Workspace {
                command:
                    WorkspaceCommands::Select { .. }
                    | WorkspaceCommands::Current
                    | WorkspaceCommands::Clear
                    | WorkspaceCommands::Create { .. }
                    | WorkspaceCommands::Open { .. }
                    | WorkspaceCommands::Rename { .. }
                    | WorkspaceCommands::Adopt { .. }
                    | WorkspaceCommands::Link { .. }
                    | WorkspaceCommands::Close { .. }
                    | WorkspaceCommands::Retry { .. },
            }) => CommandKey::Workspace,
            Some(Commands::Desktop { .. }) => CommandKey::Desktop,
            Some(Commands::Shell {
                command:
                    ShellCommands::Create { .. }
                    | ShellCommands::Rename { .. }
                    | ShellCommands::Close { .. },
            }) => CommandKey::Shell,
            Some(Commands::Launcher {
                command:
                    LauncherCommands::Create { .. }
                    | LauncherCommands::Invoke { .. }
                    | LauncherCommands::Rename { .. }
                    | LauncherCommands::Remove { .. },
            }) => CommandKey::Launcher,
            Some(Commands::Daemon {
                command:
                    DaemonCommands::Run
                    | DaemonCommands::ReceiveHandoff { .. }
                    | DaemonCommands::Start
                    | DaemonCommands::Restart
                    | DaemonCommands::Stop,
            }) => CommandKey::Daemon,
            Some(Commands::Ui) | None => CommandKey::Ui,
            Some(Commands::Doctor) => CommandKey::Doctor,
            Some(Commands::Setup) => CommandKey::Setup,
            Some(Commands::Close { .. }) => CommandKey::Close,
            Some(Commands::Skill {
                command: SkillCommands::Install { .. },
            }) => CommandKey::Skill,
            Some(Commands::Opencode {
                command:
                    OpenCodeCommands::Claim {
                        command: OpenCodeClaimCommands::Ensure { .. },
                    },
            }) => CommandKey::OpencodeClaimEnsure,
            Some(Commands::Opencode {
                command:
                    OpenCodeCommands::Claim {
                        command: OpenCodeClaimCommands::Release { .. },
                    },
            }) => CommandKey::OpencodeClaimRelease,
            Some(Commands::Opencode {
                command:
                    OpenCodeCommands::Claim {
                        command: OpenCodeClaimCommands::Report { .. },
                    },
            }) => CommandKey::OpencodeClaimReport,
            Some(Commands::Opencode { .. }) => CommandKey::Opencode,
            Some(Commands::Pi {
                command: PiCommands::Install { .. },
            }) => CommandKey::Pi,
            Some(Commands::Claude {
                command: ClaudeCommands::Hook,
            }) => CommandKey::ClaudeHook,
            Some(Commands::Codex {
                command: CodexCommands::Hook,
            }) => CommandKey::CodexHook,
            Some(Commands::Codex {
                command: CodexCommands::Launch { .. },
            }) => CommandKey::CodexLaunch,
            Some(Commands::Kiro {
                command: KiroCommands::Hook,
            }) => CommandKey::KiroHook,
            Some(Commands::Kiro {
                command: KiroCommands::Launch { .. },
            }) => CommandKey::KiroLaunch,
            Some(Commands::Open { .. }) => CommandKey::Open,
            Some(Commands::Prompt) => CommandKey::Prompt,
            Some(Commands::Attach { .. } | Commands::AwaitAttach { .. }) => CommandKey::Attach,
            Some(Commands::ResumeSession { .. }) => CommandKey::ResumeSessionInternal,
            Some(Commands::FederationStdio) => CommandKey::Attach,
            Some(Commands::GuidedNodeAdd) => CommandKey::NodeAdd,
            Some(Commands::GuidedNodeUpgrade { .. }) => CommandKey::NodeUpgrade,
            Some(Commands::GuidedNodeReauthenticate { .. }) => CommandKey::NodeReauthenticate,
            Some(Commands::UninstallRemote { .. }) => CommandKey::UninstallRemote,
            Some(Commands::UninstallFingerprint) => CommandKey::UninstallFingerprint,
            Some(Commands::BootstrapActivate { .. }) => CommandKey::Daemon,
        }
    }
}

fn json_commands() -> impl Iterator<Item = &'static str> {
    CommandKey::ALL.iter().filter_map(|key| {
        let descriptor = key.descriptor();
        (descriptor.output == OutputMode::Json && !descriptor.key.starts_with("session."))
            .then_some(descriptor.key)
    })
}

fn print_json(command: CommandKey, data: serde_json::Value) -> Result<(), Box<dyn Error>> {
    cli_output::print(command.descriptor().key, data)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CliExit {
    Success,
    Child(process_adapter::ProcessExit),
}

impl CliExit {
    fn code(self) -> ExitCode {
        match self {
            Self::Success | Self::Child(process_adapter::ProcessExit::Code(0)) => ExitCode::SUCCESS,
            Self::Child(process_adapter::ProcessExit::Code(code)) => {
                ExitCode::from(u8::try_from(code).unwrap_or(1))
            }
            Self::Child(process_adapter::ProcessExit::Signal(signal)) => {
                ExitCode::from(u8::try_from(128_i32.saturating_add(signal)).unwrap_or(1))
            }
        }
    }
}

fn main() -> ExitCode {
    if env::var_os("BOOMUX_INTERNAL_GUIDED_STOP").as_deref() == Some(std::ffi::OsStr::new("1")) {
        unsafe {
            env::remove_var("BOOMUX_INTERNAL_GUIDED_STOP");
            libc::raise(libc::SIGSTOP);
        }
    }
    let json_requested = requests_json(env::args_os().skip(1));
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(error) => {
            let exit_code = u8::try_from(error.exit_code()).unwrap_or(1);
            if json_requested
                && !matches!(
                    error.kind(),
                    ClapErrorKind::DisplayHelp | ClapErrorKind::DisplayVersion
                )
            {
                cli_output::print_error_message("cli", "invalid_argument", error.to_string());
                return ExitCode::from(exit_code);
            }
            let success = matches!(
                error.kind(),
                ClapErrorKind::DisplayHelp | ClapErrorKind::DisplayVersion
            );
            let _ = error.print();
            return if success {
                ExitCode::SUCCESS
            } else {
                ExitCode::from(exit_code)
            };
        }
    };
    let json = cli.json;
    let command = cli.command_descriptor().key;
    match run(cli) {
        Ok(outcome) => outcome.code(),
        Err(error) => {
            if json {
                cli_output::print_error(command, error.as_ref());
            } else {
                eprintln!("boomux: {error}");
            }
            ExitCode::FAILURE
        }
    }
}

fn requests_json(arguments: impl IntoIterator<Item = OsString>) -> bool {
    arguments
        .into_iter()
        .take_while(|argument| argument != "--")
        .any(|argument| argument == "--json")
}

fn run(cli: Cli) -> Result<CliExit, Box<dyn Error>> {
    let descriptor = cli.command_descriptor();
    if matches!(cli.command.as_ref(), Some(Commands::Session { .. })) {
        return Err(removed_session_error());
    }
    if cli.json && descriptor.output == OutputMode::HumanOnly {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("--json is not supported for {}", descriptor.key),
        )
        .into());
    }
    if let Some(target) = cli.remote.as_deref() {
        if cli.json
            || cli.path.is_some()
            || cli.command.is_some()
            || cli.name.is_some()
            || cli.new_window
            || !cli.startup_command.is_empty()
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "--remote currently accepts only an SSH target; remote management and dashboard routing are delivered separately",
            )
            .into());
        }
        remote_connect(target, cli.terminal.as_deref())?;
        return Ok(CliExit::Success);
    }
    match cli.command.as_ref() {
        Some(Commands::Daemon {
            command: DaemonCommands::Run,
        }) => {
            daemon::run_with_notification_delivery(config::load_notification_settings()?)?;
            return Ok(CliExit::Success);
        }
        Some(Commands::Daemon {
            command: DaemonCommands::ReceiveHandoff { channel },
        }) => {
            daemon::receive_handoff_with_notification_delivery(
                *channel,
                config::load_notification_settings()?,
            )?;
            return Ok(CliExit::Success);
        }
        Some(Commands::Web {
            command: None,
            port,
            opencode_web_url,
            opencode_web_port,
            no_opencode_web,
            tailscale,
            require_running_daemon,
        }) => {
            mobile_web::run(
                *port,
                opencode_web_url.as_deref(),
                *opencode_web_port,
                *no_opencode_web,
                *tailscale,
                *require_running_daemon,
            )?;
            return Ok(CliExit::Success);
        }
        Some(Commands::Web {
            command:
                Some(WebCommands::Start {
                    port,
                    opencode_web_url,
                    opencode_web_port,
                    no_opencode_web,
                    tailscale,
                }),
            ..
        }) => {
            web_start(
                *port,
                opencode_web_url.as_deref(),
                *opencode_web_port,
                *no_opencode_web,
                *tailscale,
                cli.json,
            )?;
            return Ok(CliExit::Success);
        }
        Some(Commands::Web {
            command: Some(WebCommands::Status { port }),
            ..
        }) => {
            web_status(*port, cli.json)?;
            return Ok(CliExit::Success);
        }
        Some(Commands::Web {
            command: Some(WebCommands::Stop { port }),
            ..
        }) => {
            let stopped = mobile_web::stop(*port)?;
            tailscale_serve::cleanup_stale(*port)?;
            if cli.json {
                print_json(
                    CommandKey::WebStop,
                    serde_json::json!({ "running": false, "changed": stopped, "port": port }),
                )?;
            } else if stopped {
                println!("Stopped Boomux web on port {port}");
            } else {
                println!("Boomux web is not running on port {port}");
            }
            return Ok(CliExit::Success);
        }
        Some(Commands::Attach {
            shell_id,
            node,
            takeover,
            restart_exited,
            expected_run_id,
        }) => {
            attach::run(
                shell_id,
                node.as_deref(),
                *takeover,
                *restart_exited,
                expected_run_id.as_deref(),
            )?;
            return Ok(CliExit::Success);
        }
        Some(Commands::AwaitAttach {
            shell_id,
            gate,
            node,
        }) => {
            terminal::await_open_gate(gate)?;
            attach::run(shell_id, node.as_deref(), false, true, None)?;
            return Ok(CliExit::Success);
        }
        Some(Commands::ResumeSession { session_id, node }) => {
            let _ = (session_id, node);
            return Err(removed_session_error());
        }
        Some(Commands::FederationStdio) => {
            federation::run_stdio_helper()?;
            return Ok(CliExit::Success);
        }
        Some(Commands::GuidedNodeAdd) => {
            return guided_node_add().map(CliExit::Child);
        }
        Some(Commands::GuidedNodeUpgrade { selector }) => {
            return guided_node_upgrade(selector).map(CliExit::Child);
        }
        Some(Commands::GuidedNodeReauthenticate { selector }) => {
            return guided_node_reauthenticate(selector).map(CliExit::Child);
        }
        Some(Commands::BootstrapActivate {
            transaction,
            expected_pid,
            expected_protocol,
            expected_executable,
            expected_socket_device,
            expected_socket_inode,
        }) => {
            bootstrap_activate(
                transaction,
                *expected_pid,
                *expected_protocol,
                expected_executable,
                *expected_socket_device,
                *expected_socket_inode,
            )?;
            return Ok(CliExit::Success);
        }
        _ => {}
    }

    if let Some(desktop_entry) = cli.terminal.as_deref() {
        terminal::validate_desktop_entry(desktop_entry)?;
    }
    if let Some(path) = cli.path {
        let new_window = should_open_new_window(cli.new_window, cli.terminal.as_deref());
        let terminal = new_window
            .then(|| effective_terminal(cli.terminal.as_deref()))
            .transpose()?
            .flatten();
        open_directory(
            &path,
            cli.name.as_deref(),
            &cli.startup_command,
            new_window,
            terminal.as_deref(),
        )?;
        return Ok(CliExit::Success);
    }

    let result = match cli.command {
        Some(Commands::Ui) => dashboard(cli.terminal.as_deref()),
        Some(Commands::Web { .. }) => unreachable!(),
        Some(Commands::Doctor) => doctor(cli.terminal.as_deref()),
        Some(Commands::Setup) => setup::guided_setup(),
        Some(Commands::Capabilities) => capabilities(cli.json),
        Some(Commands::List) => list_shells(cli.json),
        Some(Commands::Shells) => list_workspace_shells(cli.json),
        Some(Commands::Read {
            target,
            lines,
            run_id,
            after_revision,
            wait_ms,
        }) => read_shell(
            &target,
            lines,
            cli.json,
            run_id.as_deref(),
            after_revision,
            wait_ms,
        ),
        Some(Commands::Events {
            after,
            limit,
            wait_ms,
        }) => read_events(after.as_deref(), limit, wait_ms, cli.json),
        Some(Commands::Close { target, focused }) => {
            if focused {
                close_focused_shell()
            } else {
                close_shell(target.as_deref().expect("clap requires a close target"))
            }
        }
        Some(Commands::Config { command }) => config_command(command),
        Some(Commands::Update {
            command: Some(UpdateCommands::Status),
        }) => {
            let status = update::status();
            if cli.json {
                print_json(CommandKey::UpdateStatus, serde_json::to_value(status)?)
            } else {
                let value = serde_json::to_value(status)?;
                println!(
                    "current: {}",
                    value["current"].as_str().unwrap_or("unknown")
                );
                println!(
                    "latest: {}",
                    value["latest"].as_str().unwrap_or("unavailable")
                );
                println!(
                    "state: {}",
                    value["state"].as_str().unwrap_or("check_failed")
                );
                println!(
                    "install: {}",
                    value["install_kind"].as_str().unwrap_or("unknown")
                );
                println!("path: {}", value["path"].as_str().unwrap_or("unknown"));
                Ok(())
            }
        }
        Some(Commands::Update { command: None }) => update::guided_update(),
        Some(Commands::Uninstall { purge }) => uninstall::guided_uninstall(purge),
        Some(Commands::Project {
            command: ProjectCommands::List { node },
        }) => list_projects(cli.json, node.as_deref()),
        Some(Commands::Workspace { command }) => {
            workspace_command(command, cli.json, cli.terminal.as_deref())
        }
        Some(Commands::Desktop { command }) => desktop_command(command, cli.terminal.as_deref()),
        Some(Commands::Node { command }) => node_command(command, cli.json),
        Some(Commands::Shell { command }) => {
            shell_command(command, cli.json, cli.terminal.as_deref())
        }
        Some(Commands::Launcher { command }) => launcher_command(command, cli.json),
        Some(Commands::Agent {
            command: AgentCommands::Supervise(arguments),
        }) => return supervise_agent(arguments).map(CliExit::Child),
        Some(Commands::Agent { command }) => agent_command(command, cli.json),
        Some(Commands::Attention { command }) => attention_command(command, cli.json),
        Some(Commands::Notification {
            command: NotificationCommands::Test { reason },
        }) => test_notification(reason),
        Some(Commands::Session { command }) => {
            let _ = command;
            Err(removed_session_error())
        }
        Some(Commands::Integration { command }) => integration_command(command, cli.json),
        Some(Commands::Skill {
            command: SkillCommands::Install { force },
        }) => install_skill(force),
        Some(Commands::Opencode {
            command: OpenCodeCommands::Install { force },
        }) => install_opencode(force),
        Some(Commands::Opencode {
            command: OpenCodeCommands::Shared { session, port },
        }) => launch_shared_opencode(session.as_deref(), port),
        Some(Commands::Opencode {
            command: OpenCodeCommands::Claim { command },
        }) => opencode_claim_command(command, cli.json),
        Some(Commands::Pi {
            command: PiCommands::Install { force },
        }) => install_pi(force),
        Some(Commands::Claude {
            command: ClaudeCommands::Hook,
        }) => {
            if let Err(error) = claude_hook_command() {
                eprintln!("boomux claude hook: {error}");
            }
            Ok(())
        }
        Some(Commands::Codex {
            command: CodexCommands::Hook,
        }) => {
            if let Err(error) = codex_hook_command() {
                eprintln!("boomux codex hook: {error}");
            }
            Ok(())
        }
        Some(Commands::Codex {
            command: CodexCommands::Launch { arguments },
        }) => launch_codex(arguments),
        Some(Commands::Kiro {
            command: KiroCommands::Hook,
        }) => {
            if let Err(error) = kiro_hook_command() {
                eprintln!("boomux kiro hook: {error}");
            }
            Ok(())
        }
        Some(Commands::Kiro {
            command: KiroCommands::Launch { arguments },
        }) => return launch_kiro(arguments).map(CliExit::Child),
        Some(Commands::Open {
            shell_id,
            node,
            workspace,
            title,
            takeover,
        }) => {
            let terminal = effective_terminal(cli.terminal.as_deref())?;
            open_shell(
                &shell_id,
                node.as_deref(),
                workspace.as_deref(),
                title.as_deref(),
                takeover,
                terminal.as_deref(),
            )
        }
        Some(Commands::Prompt) => print_prompt_label(),
        Some(Commands::Daemon { command }) => daemon_control(command, cli.json),
        Some(Commands::Attach { .. } | Commands::AwaitAttach { .. }) => unreachable!(),
        Some(Commands::ResumeSession { .. }) => unreachable!(),
        Some(Commands::FederationStdio) => unreachable!(),
        Some(Commands::GuidedNodeAdd) => unreachable!(),
        Some(Commands::GuidedNodeUpgrade { .. }) => unreachable!(),
        Some(Commands::GuidedNodeReauthenticate { .. }) => unreachable!(),
        Some(Commands::UninstallRemote {
            expected_node_id,
            expected_executable,
        }) => uninstall::remote_uninstall(&expected_node_id, &expected_executable),
        Some(Commands::UninstallFingerprint) => {
            let target = update::uninstall_target()?;
            println!(
                "boomux-uninstall-fingerprint-v1 {}",
                target.authorization_token()
            );
            Ok(())
        }
        Some(Commands::BootstrapActivate { .. }) => unreachable!(),
        None => print_cli_help(),
    };
    result?;
    Ok(CliExit::Success)
}

fn removed_session_error() -> Box<dyn Error> {
    client::ClientError::Protocol(client::ProtocolError::UnsupportedVersion(
        "Agent Session commands have been removed".into(),
    ))
    .into()
}

fn print_cli_help() -> Result<(), Box<dyn Error>> {
    Cli::command().print_help()?;
    println!();
    Ok(())
}

fn remote_connect(target: &str, terminal: Option<&str>) -> Result<(), Box<dyn Error>> {
    let connection = verified_remote_connection(target, true)?;
    println!(
        "Connected to Boomux Node {} (protocol {}, helper {}, {})",
        connection.handshake.node_id,
        connection.handshake.core_protocol_version,
        connection.handshake.helper_version,
        connection.executable.as_str(),
    );
    let node_id = connection.handshake.node_id.clone();
    drop(connection);
    if let Ok(local) = client::connect_or_start()
        && local.node_registrations().is_ok_and(|registrations| {
            registrations
                .iter()
                .any(|registration| registration.node_id == node_id)
        })
        && local
            .supports(protocol::ProtocolFeature::CombinedNodeSnapshot)
            .unwrap_or(false)
    {
        dashboard(terminal)?;
    }
    Ok(())
}

fn verified_remote_connection(
    target: &str,
    allow_interactive_install: bool,
) -> Result<ssh_bootstrap::RemoteConnection, Box<dyn Error>> {
    const TIMEOUT: Duration = Duration::from_secs(120);

    let target = ssh_bootstrap::SshTarget::parse(target)?;
    let interactive =
        allow_interactive_install && io::stdin().is_terminal() && io::stdout().is_terminal();
    let authentication = if interactive {
        ssh_bootstrap::SshAuthenticationMode::Interactive
    } else {
        ssh_bootstrap::SshAuthenticationMode::Batch
    };
    if interactive {
        println!(
            "Connecting to {}. Complete any SSH authentication prompt or login URL shown in this terminal.",
            target.as_str()
        );
        std::io::Write::flush(&mut io::stdout())?;
    }
    let mut session = ssh_bootstrap::BootstrapSession::open(target, authentication, TIMEOUT)
        .map_err(bootstrap_cli_failure)?;
    // Both branches return a channel verified by exactly one protocol ping. An
    // installed channel is pinged before commit inside install_and_connect.
    match session.plan(TIMEOUT).map_err(bootstrap_cli_failure)? {
        ssh_bootstrap::RemoteBootstrapPlan::Ready(helper) => {
            let mut connection = session
                .connect(helper, TIMEOUT)
                .map_err(bootstrap_cli_failure)?;
            connection.ping().map_err(bootstrap_cli_failure)?;
            Ok(connection)
        }
        ssh_bootstrap::RemoteBootstrapPlan::Install(plan) if !interactive => {
            Err(cli_output::failure(
                plan.reason.error_code(),
                plan.reason.noninteractive_message(),
            ))
        }
        ssh_bootstrap::RemoteBootstrapPlan::Install(plan) => {
            println!("Remote target: {}", plan.target.as_str());
            println!("Install source: {}", plan.source.description());
            println!("Install destination: {}", plan.destination.as_str());
            println!(
                "Process impact: the pinned binary is uploaded privately first; the runtime socket must remain absent through guarded activation, and rollback removes only a destination activated by this transaction"
            );
            if !confirm_setup("Install Boomux on this remote target?")? {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "remote Boomux installation was not authorized",
                )
                .into());
            }
            session
                .install_and_connect(&plan, TIMEOUT)
                .map_err(bootstrap_cli_failure)
        }
    }
}

fn bootstrap_cli_failure(error: io::Error) -> Box<dyn Error> {
    cli_output::failure(ssh_bootstrap::error_code(&error), error.to_string())
}

const fn should_release_node_upgrade_maintenance(
    recovery: ssh_bootstrap::BootstrapRecoveryDisposition,
) -> bool {
    !matches!(
        recovery,
        ssh_bootstrap::BootstrapRecoveryDisposition::OutcomeUnknown
    )
}

fn upgrade_node(selector: &str) -> Result<(), Box<dyn Error>> {
    const TIMEOUT: Duration = Duration::from_secs(120);

    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Node upgrade requires an interactive terminal",
        )
        .into());
    }
    let local = client::connect_or_start()?;
    let registration = local.node_registration(selector)?;
    let target = ssh_bootstrap::SshTarget::parse(&registration.target)?;
    let mut session = ssh_bootstrap::BootstrapSession::open(
        target,
        ssh_bootstrap::SshAuthenticationMode::Interactive,
        TIMEOUT,
    )
    .map_err(bootstrap_cli_failure)?;
    let plan = session
        .plan_explicit_upgrade(&registration.node_id, TIMEOUT)
        .map_err(bootstrap_cli_failure)?;

    println!("Node: {} ({})", registration.alias, registration.node_id);
    println!("Remote target: {}", registration.target);
    println!("Install source: {}", plan.source.description());
    println!("Install destination: {}", plan.destination.as_str());
    println!(
        "Process impact: this workflow requires an already protocol-compatible helper; the pinned binary is uploaded privately first, activation requires proof that the running daemon uses this destination, the previous executable is retained for rollback, and any present compatible daemon is restarted"
    );
    if !confirm_setup("Upgrade Boomux on this registered Node?")? {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "remote Boomux upgrade was not authorized",
        )
        .into());
    }

    let (current, maintenance_token) =
        local.begin_node_upgrade_maintenance(&registration.node_id, registration.revision)?;
    if current.target != registration.target || current.node_id != registration.node_id {
        let _ = local.finish_node_upgrade_maintenance(&registration.node_id, &maintenance_token);
        return Err(cli_output::failure(
            "revision_changed",
            "Node registration changed while upgrade authorization was pending; refresh and retry",
        ));
    }
    let upgrade = session.install_and_connect_guarded(&plan, TIMEOUT, || {
        local
            .renew_node_upgrade_maintenance(&registration.node_id, &maintenance_token)
            .map_err(|error| io::Error::other(error.to_string()))
    });
    let connection = match upgrade {
        Ok(connection) => {
            local.finish_node_upgrade_maintenance(&registration.node_id, maintenance_token)?;
            connection
        }
        Err(error) => {
            let recovery = ssh_bootstrap::recovery_disposition(&error);
            if !should_release_node_upgrade_maintenance(recovery) {
                eprintln!(
                    "boomux: remote upgrade outcome is unknown; Node maintenance remains active until bounded expiry while watchdog recovery settles"
                );
            } else {
                match local
                    .finish_node_upgrade_maintenance(&registration.node_id, &maintenance_token)
                {
                    Ok(()) => eprintln!(
                        "boomux: {}; Node maintenance was released",
                        if recovery
                            == ssh_bootstrap::BootstrapRecoveryDisposition::RollbackConfirmed
                        {
                            "remote rollback was confirmed"
                        } else {
                            "remote mutation did not begin"
                        }
                    ),
                    Err(_) => eprintln!(
                        "boomux: the upgrade reached a safe recovery state, but Node maintenance release could not be confirmed and remains bounded by expiry"
                    ),
                }
            }
            return Err(bootstrap_cli_failure(error));
        }
    };
    println!(
        "Upgraded {} to helper {} (protocol {})",
        registration.alias,
        connection.handshake.helper_version,
        connection.handshake.core_protocol_version,
    );
    Ok(())
}

fn uninstall_node(selector: &str) -> Result<(), Box<dyn Error>> {
    const TIMEOUT: Duration = Duration::from_secs(120);

    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Node uninstall requires an interactive terminal",
        )
        .into());
    }
    let local = client::connect_or_start()?;
    if !local.supports(protocol::ProtocolFeature::NodeUninstallCoordination)? {
        return Err(cli_output::failure(
            "unsupported_version",
            "Node uninstall requires daemon protocol 48 or newer",
        ));
    }
    let registration = local.node_registration(selector)?;
    let target = ssh_bootstrap::SshTarget::parse(&registration.target)?;
    let mut session = ssh_bootstrap::BootstrapSession::open(
        target,
        ssh_bootstrap::SshAuthenticationMode::Interactive,
        TIMEOUT,
    )
    .map_err(bootstrap_cli_failure)?;
    let plan = session
        .plan_explicit_uninstall(&registration.node_id, TIMEOUT)
        .map_err(bootstrap_cli_failure)?;

    println!("Node: {} ({})", registration.alias, registration.node_id);
    println!("Remote target: {}", registration.target);
    println!("Install path: {}", plan.destination.as_str());
    println!("Remote helper version: {}", plan.helper_version);
    println!(
        "Process impact: every managed Shell process, PTY, projection worker, and shared runtime on this remote Node will be stopped"
    );
    println!(
        "Data impact: durable remote state and configuration are preserved; current Boomux integration assets are removed and modified assets are preserved"
    );
    for (display_name, path) in &plan.preserved_integrations {
        println!(
            "Preserving modified or uninspectable {display_name} integration asset{}",
            path.as_deref()
                .map(|path| format!(" at {path}"))
                .unwrap_or_default()
        );
    }
    println!(
        "Registration impact: the local registration is removed atomically only after remote executable removal is confirmed"
    );
    if !confirm_setup("Uninstall Boomux from this registered Node?")? {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "remote Boomux uninstall was not authorized",
        )
        .into());
    }

    let current = local.node_registration(&registration.node_id)?;
    if current != registration {
        return Err(cli_output::failure(
            "revision_changed",
            "Node registration changed during uninstall preparation; refresh and retry",
        ));
    }
    let (current, maintenance_token) =
        local.begin_node_upgrade_maintenance(&registration.node_id, registration.revision)?;
    if current != registration {
        let _ = local.finish_node_upgrade_maintenance(&registration.node_id, &maintenance_token);
        return Err(cli_output::failure(
            "revision_changed",
            "Node registration changed while uninstall authorization was pending; refresh and retry",
        ));
    }
    let uninstall = session.uninstall_registered(&plan, TIMEOUT, || {
        local
            .renew_node_upgrade_maintenance(&registration.node_id, &maintenance_token)
            .map_err(|error| io::Error::other(error.to_string()))
    });
    if let Err(error) = uninstall {
        let _ = local.finish_node_upgrade_maintenance(&registration.node_id, &maintenance_token);
        return Err(bootstrap_cli_failure(error));
    }
    local.finish_node_uninstall_maintenance(&registration.node_id, maintenance_token)?;
    println!(
        "Uninstalled Boomux from {} and forgot registration {}",
        registration.target, registration.alias
    );
    Ok(())
}

fn reauthenticate_node(selector: &str) -> Result<(), Box<dyn Error>> {
    const TIMEOUT: Duration = Duration::from_secs(120);

    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Node reauthentication requires an interactive terminal",
        )
        .into());
    }
    let local = client::connect_or_start()?;
    let registration = local.node_registration(selector)?;
    if !protocol::ProtocolFeature::GlobalWorkspaces.is_supported_by(local.protocol_version()?) {
        return Err(cli_output::failure(
            "unsupported_version",
            "Node reauthentication requires daemon protocol 38 or newer",
        ));
    }
    println!("Node: {} ({})", registration.alias, registration.node_id);
    println!("Stored SSH route: {}", registration.target);
    println!(
        "Complete any SSH authentication prompt or login URL shown in this terminal. Boomux will not install, upgrade, retarget, or modify the registration."
    );
    io::Write::flush(&mut io::stdout())?;

    let target = ssh_bootstrap::SshTarget::parse(&registration.target)?;
    let session = ssh_bootstrap::BootstrapSession::open(
        target.clone(),
        ssh_bootstrap::SshAuthenticationMode::Interactive,
        TIMEOUT,
    )
    .map_err(bootstrap_cli_failure)?;
    let connection = session
        .connect_existing_verified(&registration.node_id, TIMEOUT)
        .map_err(bootstrap_cli_failure)?;
    drop(connection);

    println!("Verifying that background observation can reconnect without prompts...");
    io::Write::flush(&mut io::stdout())?;
    let session = ssh_bootstrap::BootstrapSession::open(
        target,
        ssh_bootstrap::SshAuthenticationMode::Batch,
        TIMEOUT,
    )
    .map_err(bootstrap_cli_failure)?;
    let connection = session
        .connect_existing_verified(&registration.node_id, TIMEOUT)
        .map_err(bootstrap_cli_failure)?;

    let current = local.node_registration(&registration.node_id)?;
    if current != registration {
        return Err(cli_output::failure(
            "revision_changed",
            "Node registration changed during reauthentication; refresh and retry",
        ));
    }
    local.force_node_projection_refresh(&registration.node_id)?;
    if local.node_registration(&registration.node_id)? != registration {
        return Err(cli_output::failure(
            "revision_changed",
            "Node registration changed while requesting a fresh observation; refresh and retry",
        ));
    }
    drop(connection);
    println!(
        "Authenticated {} and requested a fresh background observation",
        registration.alias
    );
    Ok(())
}

fn node_command(command: NodeCommands, json: bool) -> Result<(), Box<dyn Error>> {
    match command {
        NodeCommands::Add { alias, target } => {
            let (alias, target) = resolve_node_add_inputs(alias, target, json)?;
            let remote = verified_remote_connection(&target, !json)?;
            let registration = client::connect_or_start()?.add_node_registration(
                alias,
                target,
                remote.handshake.node_id.clone(),
            )?;
            print_node_registration(CommandKey::NodeAdd, &registration, json)
        }
        NodeCommands::List => {
            let client = client::connect_or_start()?;
            let registrations = client.node_registrations()?;
            let projection_supported =
                client.supports(protocol::ProtocolFeature::NodeProjectionSync)?;
            if json {
                let rows = registrations
                    .into_iter()
                    .map(|registration| {
                        let mut value = serde_json::to_value(registration)?;
                        if projection_supported {
                            let health = client.node_projection_health(
                                value["node_id"].as_str().expect("Node ID is a string"),
                            )?;
                            value
                                .as_object_mut()
                                .expect("registration serializes as an object")
                                .insert("projection".into(), serde_json::to_value(health)?);
                        }
                        Ok::<_, Box<dyn Error>>(value)
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                print_json(CommandKey::NodeList, serde_json::to_value(rows)?)
            } else {
                println!("ALIAS\tTARGET\tNODE ID\tREVISION\tPROJECTION");
                for registration in registrations {
                    let health = projection_supported
                        .then(|| client.node_projection_health(&registration.node_id))
                        .transpose()?;
                    println!(
                        "{}\t{}\t{}\t{}\t{}",
                        registration.alias,
                        registration.target,
                        registration.node_id,
                        registration.revision,
                        health
                            .map(|health| format!("{:?}", health.code).to_ascii_lowercase())
                            .unwrap_or_else(|| "unsupported".into()),
                    );
                }
                Ok(())
            }
        }
        NodeCommands::Upgrade { selector } => upgrade_node(&selector),
        NodeCommands::Uninstall { selector } => uninstall_node(&selector),
        NodeCommands::Reauthenticate { selector } => reauthenticate_node(&selector),
        NodeCommands::Inspect { selector } => {
            let client = client::connect_or_start()?;
            let registration = client.node_registration(selector)?;
            let health = client
                .supports(protocol::ProtocolFeature::NodeProjectionSync)?
                .then(|| client.node_projection_health(&registration.node_id))
                .transpose()?;
            if json {
                let mut value = serde_json::to_value(&registration)?;
                if let Some(health) = health {
                    value
                        .as_object_mut()
                        .expect("registration serializes as an object")
                        .insert("projection".into(), serde_json::to_value(health)?);
                }
                print_json(CommandKey::NodeInspect, value)
            } else {
                print_node_registration(CommandKey::NodeInspect, &registration, false)?;
                if let Some(health) = health {
                    println!(
                        "Projection: {}",
                        format!("{:?}", health.code).to_ascii_lowercase()
                    );
                    println!("Projection stale: {}", health.stale);
                    println!("Cache generation: {}", health.cache_generation);
                    if let (Some(stream_id), Some(cursor)) = (health.stream_id, health.cursor) {
                        println!("Remote cursor: {stream_id}:{cursor}");
                    }
                } else {
                    println!("Projection: unsupported");
                }
                Ok(())
            }
        }
        NodeCommands::Snapshot { selector } => {
            let snapshot = client::connect_or_start()?.combined_node_snapshot(selector)?;
            if json {
                let workspaces = snapshot.workspaces.clone();
                let external_workspaces = snapshot.external_workspaces.clone();
                let focused_terminal = snapshot.focused_terminal.clone();
                let nodes = snapshot
                    .nodes
                    .into_iter()
                    .map(node_snapshot_json)
                    .collect::<Result<Vec<_>, _>>()?;
                print_json(
                    CommandKey::NodeSnapshot,
                    serde_json::json!({
                        "nodes": nodes,
                        "workspaces": workspaces,
                        "external_workspaces": external_workspaces,
                        "focused_terminal": focused_terminal,
                    }),
                )
            } else {
                println!("ALIAS\tOWNERSHIP\tHEALTH\tCURRENT\tOBSERVED\tWORKSPACES");
                for node in snapshot.nodes {
                    let workspace_count = node
                        .local_snapshot
                        .as_ref()
                        .map(|snapshot| snapshot.workspaces.len())
                        .or_else(|| {
                            node.remote_projection
                                .as_ref()
                                .map(|projection| projection.workspaces.len())
                        })
                        .unwrap_or(0);
                    println!(
                        "{}\t{}\t{}\t{}\t{}\t{}",
                        node.alias,
                        if node.local { "local" } else { "remote" },
                        format!("{:?}", node.health).to_ascii_lowercase(),
                        node.current,
                        node.observed_at_ms,
                        workspace_count,
                    );
                }
                println!("\nGLOBAL WORKSPACES");
                println!("NAME\tID\tREVISION\tSTATE\tPLACEMENTS");
                for workspace in snapshot.workspaces {
                    println!(
                        "{}\t{}\t{}\t{}\t{}",
                        workspace.name,
                        workspace.id,
                        workspace.revision,
                        if workspace.closing {
                            "closing"
                        } else {
                            "active"
                        },
                        workspace.placements.len(),
                    );
                    for placement in workspace.placements {
                        println!(
                            "  ->\t{}:{}\t{}\t{:?}\t{}",
                            placement.node_id,
                            placement.workspace_id,
                            placement.owner_revision,
                            placement.state,
                            placement
                                .default_cwd
                                .as_deref()
                                .map_or_else(|| "-".into(), |path| path.display().to_string()),
                        );
                    }
                }
                println!("\nEXTERNAL WORKSPACES");
                println!("NAME\tOWNER\tREVISION\tAVAILABLE\tDEFAULT CWD");
                for workspace in snapshot.external_workspaces {
                    println!(
                        "{}\t{}:{}\t{}\t{}\t{}",
                        workspace.name,
                        workspace.identity.node_id,
                        workspace.identity.inner_id,
                        workspace.revision,
                        workspace.available,
                        workspace
                            .default_cwd
                            .as_deref()
                            .map_or_else(|| "-".into(), |path| path.display().to_string()),
                    );
                }
                Ok(())
            }
        }
        NodeCommands::Rename {
            selector,
            alias,
            revision,
        } => {
            let registration =
                client::connect_or_start()?.rename_node_registration(selector, alias, revision)?;
            print_node_registration(CommandKey::NodeRename, &registration, json)
        }
        NodeCommands::Retarget {
            selector,
            target,
            revision,
        } => {
            let local = client::connect_or_start()?;
            let current = local.node_registration(&selector)?;
            if current.revision != revision {
                return Err(cli_output::failure(
                    "revision_changed",
                    format!(
                        "Node registration revision changed: expected {revision}, current {}",
                        current.revision
                    ),
                ));
            }
            let remote = verified_remote_connection(&target, !json)?;
            let registration = local.retarget_node_registration(
                selector,
                target,
                remote.handshake.node_id.clone(),
                revision,
            )?;
            print_node_registration(CommandKey::NodeRetarget, &registration, json)
        }
        NodeCommands::Forget { selector } => {
            let registration = client::connect_or_start()?.forget_node_registration(selector)?;
            print_node_registration(CommandKey::NodeForget, &registration, json)
        }
        NodeCommands::Rekey => rekey_node(),
    }
}

fn guided_node_add() -> Result<process_adapter::ProcessExit, Box<dyn Error>> {
    let executable = env::current_exe()?;
    let stdin = io::stdin();
    let interactive = stdin.is_terminal() && io::stdout().is_terminal();
    let mut input = stdin.lock();
    let mut output = io::stdout().lock();
    guided_node_add_with(
        &executable,
        &mut input,
        &mut output,
        interactive,
        interactive.then(|| stdin.as_raw_fd()),
    )
}

fn guided_node_upgrade(selector: &str) -> Result<process_adapter::ProcessExit, Box<dyn Error>> {
    let executable = env::current_exe()?;
    let stdin = io::stdin();
    let interactive = stdin.is_terminal() && io::stdout().is_terminal();
    let mut input = stdin.lock();
    let mut output = io::stdout().lock();
    guided_node_command_with(
        &executable,
        &["node", "upgrade", selector],
        "Node upgrade",
        &mut input,
        &mut output,
        interactive,
        interactive.then(|| stdin.as_raw_fd()),
    )
}

fn guided_node_reauthenticate(
    selector: &str,
) -> Result<process_adapter::ProcessExit, Box<dyn Error>> {
    let executable = env::current_exe()?;
    let stdin = io::stdin();
    let interactive = stdin.is_terminal() && io::stdout().is_terminal();
    let mut input = stdin.lock();
    let mut output = io::stdout().lock();
    guided_node_command_with(
        &executable,
        &["node", "reauthenticate", selector],
        "Node reauthentication",
        &mut input,
        &mut output,
        interactive,
        interactive.then(|| stdin.as_raw_fd()),
    )
}

fn restore_terminal_foreground(fd: i32, foreground: i32) {
    unsafe {
        let previous = libc::signal(libc::SIGTTOU, libc::SIG_IGN);
        let _ = libc::tcsetpgrp(fd, foreground);
        libc::signal(libc::SIGTTOU, previous);
    }
}

fn guided_node_add_with(
    executable: &Path,
    input: &mut impl BufRead,
    output: &mut impl io::Write,
    wait_for_newline: bool,
    terminal_fd: Option<i32>,
) -> Result<process_adapter::ProcessExit, Box<dyn Error>> {
    guided_node_command_with(
        executable,
        &["node", "add"],
        "Node setup",
        input,
        output,
        wait_for_newline,
        terminal_fd,
    )
}

fn guided_node_command_with(
    executable: &Path,
    arguments: &[&str],
    operation: &str,
    input: &mut impl BufRead,
    output: &mut impl io::Write,
    wait_for_newline: bool,
    terminal_fd: Option<i32>,
) -> Result<process_adapter::ProcessExit, Box<dyn Error>> {
    let mut command = Command::new(executable);
    command.args(arguments);
    if terminal_fd.is_some() {
        command.env("BOOMUX_INTERNAL_GUIDED_STOP", "1");
        unsafe {
            command.pre_exec(|| {
                if libc::setpgid(0, 0) == -1 {
                    Err(io::Error::last_os_error())
                } else {
                    Ok(())
                }
            });
        }
    }
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => {
            writeln!(output, "\n{operation} failed to start: {error}")?;
            hold_guided_result(input, output, wait_for_newline)?;
            return Ok(process_adapter::ProcessExit::Code(1));
        }
    };

    let original_foreground = if let Some(fd) = terminal_fd {
        let foreground = unsafe { libc::tcgetpgrp(fd) };
        let child_group = i32::try_from(child.id()).unwrap_or(i32::MAX);
        let mut stopped_status = 0;
        let stopped = unsafe { libc::waitpid(child_group, &mut stopped_status, libc::WUNTRACED) };
        if foreground == -1
            || stopped != child_group
            || !libc::WIFSTOPPED(stopped_status)
            || unsafe { libc::tcsetpgrp(fd, child_group) } == -1
        {
            let error = io::Error::last_os_error();
            unsafe {
                libc::kill(-child_group, libc::SIGKILL);
            }
            let _ = child.wait();
            writeln!(
                output,
                "\n{operation} failed to acquire the terminal: {error}"
            )?;
            hold_guided_result(input, output, wait_for_newline)?;
            return Ok(process_adapter::ProcessExit::Code(1));
        }
        let continued = if cfg!(test) && env::var_os("BOOMUX_TEST_GUIDED_SIGCONT_FAILURE").is_some()
        {
            Err(io::Error::other("injected SIGCONT failure"))
        } else if unsafe { libc::kill(-child_group, libc::SIGCONT) } == -1 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        };
        if let Err(error) = continued {
            unsafe {
                libc::kill(-child_group, libc::SIGKILL);
            }
            restore_terminal_foreground(fd, foreground);
            let _ = child.wait();
            writeln!(output, "\n{operation} failed to continue: {error}")?;
            hold_guided_result(input, output, wait_for_newline)?;
            return Ok(process_adapter::ProcessExit::Code(1));
        }
        Some((fd, foreground))
    } else {
        None
    };
    let status = child.wait();
    if let Some((fd, foreground)) = original_foreground {
        restore_terminal_foreground(fd, foreground);
    }
    let status = match status {
        Ok(status) => status,
        Err(error) => {
            writeln!(output, "\n{operation} status could not be read: {error}")?;
            hold_guided_result(input, output, wait_for_newline)?;
            return Ok(process_adapter::ProcessExit::Code(1));
        }
    };
    writeln!(
        output,
        "\n{operation} {} ({}).",
        if status.success() {
            "finished"
        } else {
            "failed"
        },
        match status.code() {
            Some(code) => format!("exit {code}"),
            None => format!("signal {}", status.signal().unwrap_or(0)),
        }
    )?;
    hold_guided_result(input, output, wait_for_newline)?;
    Ok(process_exit_from_status(status))
}

fn process_exit_from_status(status: std::process::ExitStatus) -> process_adapter::ProcessExit {
    match status.code() {
        Some(code) => process_adapter::ProcessExit::Code(code),
        None => process_adapter::ProcessExit::Signal(status.signal().unwrap_or(0)),
    }
}

fn hold_guided_result(
    input: &mut impl BufRead,
    output: &mut impl io::Write,
    wait_for_newline: bool,
) -> io::Result<()> {
    if !wait_for_newline {
        writeln!(
            output,
            "No interactive terminal is available; closing without waiting."
        )?;
        return Ok(());
    }
    write!(output, "Press Enter to close. ")?;
    output.flush()?;
    let mut bytes = Vec::new();
    let count = input.read_until(b'\n', &mut bytes)?;
    if count == 0 || bytes.last() != Some(&b'\n') {
        writeln!(output, "\nInput closed before a newline; closing now.")?;
    }
    Ok(())
}

fn resolve_node_add_inputs(
    alias: Option<String>,
    target: Option<String>,
    json: bool,
) -> Result<(String, String), Box<dyn Error>> {
    match (alias, target) {
        (Some(alias), Some(target)) => return Ok((alias, target)),
        (None, None) => {}
        _ => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "provide both ALIAS and TARGET, or omit both for guided setup",
            )
            .into());
        }
    }
    if json || !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "guided Node setup requires an interactive terminal; use: boomux node add ALIAS TARGET",
        )
        .into());
    }

    println!("Add a remote Boomux Node");
    println!("Boomux uses your normal OpenSSH configuration and keeps remote work on its owner.");
    let target = prompt_node_value("SSH target (for example user@workbox): ", None)?;
    let suggested_alias = target.rsplit('@').next().filter(|value| !value.is_empty());
    let alias = prompt_node_value("Local alias", suggested_alias)?;
    Ok((alias, target))
}

fn prompt_node_value(label: &str, default: Option<&str>) -> io::Result<String> {
    match default {
        Some(default) => print!("{label} [{default}]: "),
        None => print!("{label}"),
    }
    std::io::Write::flush(&mut io::stdout())?;
    let mut value = String::new();
    io::stdin().read_line(&mut value)?;
    let value = value.trim();
    if value.is_empty() {
        if let Some(default) = default {
            return Ok(default.to_owned());
        }
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Node setup value cannot be empty",
        ));
    }
    Ok(value.to_owned())
}

fn node_snapshot_json(node: protocol::CombinedNode) -> Result<serde_json::Value, Box<dyn Error>> {
    let protocol::CombinedNode {
        node_id,
        alias,
        local,
        route,
        registration_revision,
        health,
        current,
        stale,
        observed_at_ms,
        observed_protocol_version,
        observed_capabilities,
        observed_helper_version,
        workspace_owner_eligible,
        workspace_owner_unavailable_reason,
        local_snapshot,
        remote_projection,
        ..
    } = node;
    let qualify = |value| qualify_resource_identities(value, &node_id);
    Ok(serde_json::json!({
        "node_id": node_id,
        "alias": alias,
        "local": local,
        "route": route,
        "registration_revision": registration_revision,
        "health": health,
        "current": current,
        "stale": stale,
        "observed_at_ms": observed_at_ms,
        "observed_protocol_version": observed_protocol_version,
        "observed_helper_version": observed_helper_version,
        "observed_capabilities": observed_capabilities,
        "workspace_owner_eligible": workspace_owner_eligible,
        "workspace_owner_unavailable_reason": workspace_owner_unavailable_reason,
        "local_snapshot": local_snapshot.map(serde_json::to_value).transpose()?.map(qualify),
        "remote_projection": remote_projection.map(serde_json::to_value).transpose()?.map(qualify),
    }))
}

fn qualify_resource_identities(mut value: serde_json::Value, node_id: &str) -> serde_json::Value {
    const ID_FIELDS: &[&str] = &[
        "id",
        "workspace_id",
        "shell_id",
        "run_id",
        "launcher_id",
        "agent_id",
    ];
    match &mut value {
        serde_json::Value::Array(values) => {
            for value in values {
                *value = qualify_resource_identities(value.take(), node_id);
            }
        }
        serde_json::Value::Object(object) => {
            for (key, value) in object {
                if ID_FIELDS.contains(&key.as_str()) {
                    if let Some(inner_id) = value.as_str() {
                        *value = serde_json::to_value(protocol::QualifiedIdentity::new(
                            node_id, inner_id,
                        ))
                        .expect("qualified identity serializes");
                    }
                } else {
                    *value = qualify_resource_identities(value.take(), node_id);
                }
            }
        }
        _ => {}
    }
    value
}

fn print_node_registration(
    command: CommandKey,
    registration: &protocol::NodeRegistrationSnapshot,
    json: bool,
) -> Result<(), Box<dyn Error>> {
    if json {
        print_json(command, serde_json::to_value(registration)?)
    } else {
        println!("Alias: {}", registration.alias);
        println!("Target: {}", registration.target);
        println!("Node ID: {}", registration.node_id);
        println!("Revision: {}", registration.revision);
        println!("Tombstone epoch: {}", registration.tombstone_epoch);
        Ok(())
    }
}

fn rekey_node() -> Result<(), Box<dyn Error>> {
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "node rekey requires an interactive terminal",
        )
        .into());
    }
    let client = client::connect()?;
    let current = client.node_identity()?;
    println!("Current Node ID: {current}");
    println!("Rekey changes the federated identity of every resource owned by this Node.");
    print!("Type the current Node ID to continue: ");
    io::Write::flush(&mut io::stdout())?;
    let mut confirmation = String::new();
    io::stdin().read_line(&mut confirmation)?;
    validate_rekey_confirmation(&current, &confirmation)?;
    let replacement = client.rekey_node(&current)?;
    println!("Rekeyed Boomux Node: {current} -> {replacement}");
    println!("Existing remote registrations must forget and add this Node again.");
    Ok(())
}

fn validate_rekey_confirmation(expected: &str, confirmation: &str) -> io::Result<()> {
    if confirmation.trim_end_matches(['\r', '\n']) == expected {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Node rekey confirmation did not match the current Node ID",
        ))
    }
}

fn web_status(port: u16, json: bool) -> Result<(), Box<dyn Error>> {
    let status = mobile_web::status(port)?;
    if json {
        print_json(
            CommandKey::WebStatus,
            status.map_or_else(
                || serde_json::json!({ "running": false, "port": port }),
                |status| serde_json::to_value(status).expect("web status serializes"),
            ),
        )?;
    } else if let Some(status) = status {
        println!("running ({})", status.dashboard_url);
        if let Some(url) = status.opencode_url {
            println!("OpenCode Web: {url}");
        } else if status.opencode_requested_port.is_some() {
            println!("OpenCode Web: unavailable");
        }
    } else {
        println!("not running (port {port})");
    }
    Ok(())
}

fn web_start(
    port: u16,
    opencode_web_url: Option<&str>,
    opencode_web_port: u16,
    no_opencode_web: bool,
    tailscale: bool,
    json: bool,
) -> Result<(), Box<dyn Error>> {
    let requested_opencode_port = (!no_opencode_web).then_some(opencode_web_port);
    if let Some(status) = mobile_web::status(port)? {
        let expected_opencode_url = requested_opencode_port.and_then(|runtime_port| {
            if tailscale {
                None
            } else {
                Some(opencode_web_url.map_or_else(
                    || format!("http://127.0.0.1:{runtime_port}"),
                    |url| url.trim().trim_end_matches('/').to_owned(),
                ))
            }
        });
        let status_requested_opencode_port =
            status.opencode_requested_port.or(status.opencode_port);
        let status_requested_opencode_url = status
            .opencode_requested_url
            .as_deref()
            .or(status.opencode_url.as_deref());
        if status.tailscale != tailscale
            || status_requested_opencode_port != requested_opencode_port
            || expected_opencode_url
                .as_deref()
                .is_some_and(|expected| status_requested_opencode_url != Some(expected))
        {
            return Err(io::Error::new(
                io::ErrorKind::AddrInUse,
                format!("Boomux web is already running with different options on port {port}"),
            )
            .into());
        }
        return print_web_start(status, false, json);
    }

    let daemon = client::connect()?;
    daemon.ping()?;
    let executable = env::current_exe()?;
    let socket = client::socket_path()?;
    let runtime = socket
        .parent()
        .ok_or_else(|| io::Error::other("Boomux daemon socket has no runtime directory"))?;
    let log_path = runtime.join(format!("web-{port}.log"));
    let log = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&log_path)?;
    let metadata = log.metadata()?;
    if !metadata.is_file() || metadata.uid() != unsafe { libc::geteuid() } {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "Boomux web log is not an owner-controlled regular file",
        )
        .into());
    }
    let stderr = log.try_clone()?;
    let mut command = Command::new(executable);
    command
        .args(["web", "--port", &port.to_string()])
        .args(["--opencode-web-port", &opencode_web_port.to_string()])
        .arg("--require-running-daemon")
        .stdin(Stdio::null())
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(stderr));
    if let Some(url) = opencode_web_url {
        command.args(["--opencode-web-url", url]);
    }
    if no_opencode_web {
        command.arg("--no-opencode-web");
    }
    if tailscale {
        command.arg("--tailscale");
    }
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                Err(io::Error::last_os_error())
            } else {
                Ok(())
            }
        });
    }
    let mut child = command.spawn()?;
    let started = Instant::now();
    while started.elapsed() < Duration::from_secs(15) {
        if let Some(status) = mobile_web::status(port)? {
            return print_web_start(status, true, json);
        }
        if let Some(exit) = child.try_wait()? {
            return Err(io::Error::other(format!(
                "Boomux web exited before becoming ready ({exit}); inspect {}",
                log_path.display()
            ))
            .into());
        }
        thread::sleep(Duration::from_millis(100));
    }
    let _ = child.kill();
    let _ = child.wait();
    Err(io::Error::new(
        io::ErrorKind::TimedOut,
        format!(
            "Boomux web did not become ready; inspect {}",
            log_path.display()
        ),
    )
    .into())
}

fn print_web_start(
    status: mobile_web::WebStatus,
    changed: bool,
    json: bool,
) -> Result<(), Box<dyn Error>> {
    if json {
        let mut value = serde_json::to_value(&status)?;
        value["changed"] = serde_json::json!(changed);
        print_json(CommandKey::WebStart, value)?;
    } else if changed {
        println!("Started Boomux web: {}", status.dashboard_url);
    } else {
        println!("Boomux web is already running: {}", status.dashboard_url);
    }
    if !json && status.opencode_requested_port.is_some() && status.opencode_port.is_none() {
        eprintln!("boomux: OpenCode is unavailable; continuing without OpenCode handoffs");
    }
    Ok(())
}

fn daemon_control(command: DaemonCommands, json: bool) -> Result<(), Box<dyn Error>> {
    let running_client = client::connect_if_running()?;
    match command {
        DaemonCommands::Start => {
            if running_client.is_some() {
                println!("Boomux daemon is already running");
            } else {
                client::connect_or_start()?;
                println!("Started Boomux daemon");
            }
        }
        DaemonCommands::Status if running_client.is_none() && json => print_json(
            CommandKey::DaemonStatus,
            serde_json::json!({
                "status": "stopped",
                "protocol_version": null,
                "socket_path": client::socket_path()?.display().to_string(),
                "pid": null,
                "executable": null,
                "socket_device": null,
                "socket_inode": null,
            }),
        )?,
        DaemonCommands::Status if running_client.is_none() => {
            println!("stopped ({})", client::socket_path()?.display());
        }
        DaemonCommands::Status if json => {
            let client = running_client.as_ref().expect("running client checked");
            let identity = daemon_process_identity(client);
            let protocol_version = identity.as_ref().map_or_else(
                || client.protocol_version(),
                |identity| Ok(identity.protocol_version),
            )?;
            print_json(
                CommandKey::DaemonStatus,
                serde_json::json!({
                    "status": "running",
                    "protocol_version": protocol_version,
                    "socket_path": client.socket_path().display().to_string(),
                    "pid": identity.as_ref().map(|identity| identity.pid),
                    "executable": identity.as_ref().and_then(|identity| identity.executable.as_deref()),
                    "socket_device": identity.as_ref().map(|identity| identity.socket_device),
                    "socket_inode": identity.as_ref().map(|identity| identity.socket_inode),
                }),
            )?
        }
        DaemonCommands::Status => {
            let client = running_client.as_ref().expect("running client checked");
            println!(
                "running (protocol {}, {})",
                client.protocol_version()?,
                client.socket_path().display()
            );
        }
        DaemonCommands::Restart => {
            let client = running_client.as_ref().ok_or_else(|| {
                io::Error::new(io::ErrorKind::NotConnected, "Boomux daemon is stopped")
            })?;
            client
                .restart_with_notification_config(config::load_notification_settings()?.into())?;
            println!("Restarted Boomux daemon");
        }
        DaemonCommands::Stop => {
            let client = running_client.as_ref().ok_or_else(|| {
                io::Error::new(io::ErrorKind::NotConnected, "Boomux daemon is stopped")
            })?;
            client.shutdown()?;
            println!("Stopped Boomux daemon");
        }
        DaemonCommands::Run | DaemonCommands::ReceiveHandoff { .. } => unreachable!(),
    }
    Ok(())
}

struct DaemonProcessIdentity {
    pid: u32,
    protocol_version: u32,
    executable: Option<String>,
    socket_device: u64,
    socket_inode: u64,
}

#[cfg(target_os = "linux")]
fn daemon_process_identity(client: &client::Client) -> Option<DaemonProcessIdentity> {
    let socket_before = fs::metadata(client.socket_path()).ok()?;
    let credentials = match client.daemon_process_credentials() {
        Ok(credentials) if credentials.uid == unsafe { libc::geteuid() } => credentials,
        _ => return None,
    };
    let socket_after = fs::metadata(client.socket_path()).ok()?;
    if socket_before.dev() != socket_after.dev() || socket_before.ino() != socket_after.ino() {
        return None;
    }
    let process_directory = PathBuf::from(format!("/proc/{}", credentials.pid));
    if fs::metadata(&process_directory).map_or(true, |metadata| {
        metadata.uid() != credentials.uid || !metadata.is_dir()
    }) {
        return None;
    }
    let executable = fs::read_link(process_directory.join("exe"))
        .ok()
        .and_then(|executable| normalize_daemon_executable(executable.as_os_str().as_bytes()));
    let confirmed = client.daemon_process_credentials().ok()?;
    if confirmed != credentials {
        return None;
    }
    Some(DaemonProcessIdentity {
        pid: credentials.pid,
        protocol_version: credentials.protocol_version,
        executable,
        socket_device: socket_after.dev(),
        socket_inode: socket_after.ino(),
    })
}

#[cfg(target_os = "linux")]
fn normalize_daemon_executable(path: &[u8]) -> Option<String> {
    const MAX_EXECUTABLE_PATH: usize = 4096;
    let mut bytes = path;
    if let Some(path) = bytes.strip_suffix(b" (deleted)") {
        bytes = path;
    }
    if bytes.is_empty()
        || bytes.len() > MAX_EXECUTABLE_PATH
        || bytes.iter().any(u8::is_ascii_control)
    {
        return None;
    }
    let executable = match String::from_utf8(bytes.to_vec()) {
        Ok(executable) => executable,
        Err(_) => return None,
    };
    let executable_path = Path::new(&executable);
    if !executable_path.is_absolute() {
        return None;
    }
    Some(executable)
}

#[cfg(not(target_os = "linux"))]
fn daemon_process_identity(_client: &client::Client) -> Option<DaemonProcessIdentity> {
    None
}

fn bootstrap_activate(
    transaction: &str,
    expected_pid: u32,
    expected_protocol: u32,
    expected_executable: &str,
    expected_socket_device: u64,
    expected_socket_inode: u64,
) -> io::Result<()> {
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (
            transaction,
            expected_pid,
            expected_protocol,
            expected_executable,
            expected_socket_device,
            expected_socket_inode,
        );
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "proof-bound bootstrap activation is unsupported on this platform",
        ));
    }
    #[cfg(target_os = "linux")]
    {
        let suffix = transaction
            .strip_prefix(".boomux.bootstrap.")
            .filter(|suffix| {
                suffix.len() == 8 && suffix.bytes().all(|byte| byte.is_ascii_alphanumeric())
            })
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidInput, "invalid bootstrap transaction")
            })?;
        let _ = suffix;
        let home = env::var_os("HOME")
            .map(PathBuf::from)
            .filter(|home| home.is_absolute())
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "HOME must be absolute"))?;
        let directory = home.join(".local/bin");
        let destination = directory.join("boomux");
        if destination.as_os_str().as_bytes() != expected_executable.as_bytes() {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "bootstrap destination does not match the daemon proof",
            ));
        }
        let provisional = directory.join(transaction).join("new");
        let socket = client::socket_path()?;
        let daemon_client = client::Client::from_socket_path(socket);
        let identity = daemon_process_identity(&daemon_client).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::PermissionDenied,
                "the daemon identity could not be bound to activation",
            )
        })?;
        if identity.pid != expected_pid
            || identity.protocol_version != expected_protocol
            || identity.executable.as_deref() != Some(expected_executable)
            || identity.socket_device != expected_socket_device
            || identity.socket_inode != expected_socket_inode
        {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "the daemon identity changed before activation",
            ));
        }
        fs::rename(&provisional, &destination)?;
        fs::File::open(&destination)?.sync_all()?;
        fs::File::create(directory.join(transaction).join("activated"))?.sync_all()?;
        fs::File::open(&directory)?.sync_all()?;
        Ok(())
    }
}

fn should_open_new_window(new_window: bool, terminal: Option<&str>) -> bool {
    new_window || terminal.is_some()
}

fn config_command(command: ConfigCommands) -> Result<(), Box<dyn Error>> {
    match command {
        ConfigCommands::Path => println!("{}", config::active_path()?.display()),
        ConfigCommands::Validate => {
            let snapshot = config::validate()?;
            let loaded = snapshot.layers.iter().filter(|layer| layer.loaded).count();
            let source = match snapshot.active_source {
                config::ConfigSource::Global => "global",
                config::ConfigSource::Environment => "BOOMUX_CONFIG",
            };
            let effective = snapshot
                .effective
                .path
                .as_deref()
                .map_or_else(|| "defaults".into(), |path| path.display().to_string());
            println!(
                "Configuration is valid ({loaded} loaded layer{}; effective: {effective}; active {source}: {})",
                if loaded == 1 { "" } else { "s" },
                snapshot.active_path.display()
            );
        }
        ConfigCommands::Edit => config::edit()?,
    }
    Ok(())
}

fn effective_terminal(override_entry: Option<&str>) -> Result<Option<String>, Box<dyn Error>> {
    if let Some(entry) = override_entry {
        return Ok(Some(entry.to_owned()));
    }
    Ok(config::load()?.terminal)
}

fn hyprland_special_workspaces_enabled() -> Result<bool, Box<dyn Error>> {
    Ok(hyprland_special_workspaces_configured()? && hyprland::session_available())
}

fn hyprland_special_workspaces_configured() -> Result<bool, Box<dyn Error>> {
    Ok(matches!(
        config::load()?.desktop.workspace_layer,
        config::DesktopWorkspaceLayer::HyprlandSpecial
    ))
}

fn dashboard(terminal_override: Option<&str>) -> Result<(), Box<dyn Error>> {
    ensure_host_terminal()?;
    validate_dashboard_terminal(io::stdin().is_terminal(), io::stdout().is_terminal())?;
    let launch_cwd = resolve_directory(Path::new("."))?;
    let client = client::connect_or_start()?;
    let local_node_id = client.node_identity()?;
    let mut git_cache = git::Cache::default();
    let mut refresh = DashboardRefresh::baseline(&client)?;
    let snapshot = refresh.snapshot().clone();
    let combined = combined_node_snapshot_for_dashboard(&client)?;
    let config = config::load()?;
    let terminal = terminal_override
        .map(str::to_owned)
        .or_else(|| config.terminal.clone());
    let roots_configured = !config.projects.roots.is_empty();
    let discovery = projects::discover(&config.projects);
    let project_context = tui::ProjectContext {
        projects: discovery
            .projects
            .into_iter()
            .map(|project| tui::ProjectView {
                name: project.name,
                path: project.path,
                group: project.group,
                group_order: project.group_order,
            })
            .collect(),
        config_path: config.path.or_else(config::global_config_path),
        warning: (!discovery.warnings.is_empty()).then(|| discovery.warnings.join("; ")),
        roots_configured,
    };

    let initial_state = dashboard_state(snapshot, combined, &refresh, &mut git_cache, false);
    let selected_workspace_id = workspace_selection::load_from_environment()
        .ok()
        .flatten()
        .map(|selection| selection.workspace_id().to_owned());
    let session_local_node_id = local_node_id.clone();
    let session_terminal = terminal.clone();
    tui::run(
        initial_state,
        selected_workspace_id,
        config.dashboard.follow_focused_terminal,
        project_context,
        true,
        move |effect| match effect {
            tui::DashboardEffect::Quit => unreachable!("quit is handled by the dashboard runtime"),
            tui::DashboardEffect::AddNode => tui::DashboardEvent::OperationCompleted(
                terminal::open_node_add(terminal.as_deref())
                    .map(|()| "Opened guided Node setup".into())
                    .map_err(|error| error.to_string()),
            ),
            tui::DashboardEffect::SelectWorkspace { workspace_id } => {
                let result = (|| {
                    let combined = client
                        .combined_node_snapshot(None)
                        .map_err(|error| error.to_string())?;
                    let workspace =
                        find_global_workspace_by_id(&combined.workspaces, &workspace_id)
                            .ok_or_else(|| "Workspace is no longer retained".to_owned())?;
                    if workspace.closing {
                        return Err("a closing Workspace cannot be selected".into());
                    }
                    let selection = workspace_selection::WorkspaceSelection::new(&workspace.id)
                        .map_err(|error| error.to_string())?;
                    workspace_selection::save_from_environment(&selection)
                        .map_err(|error| error.to_string())?;
                    Ok(format!("Set {} as the default Workspace", workspace.name))
                })();
                tui::DashboardEvent::WorkspaceSelectionCompleted {
                    workspace_id,
                    result,
                }
            }
            tui::DashboardEffect::UpgradeNode(node_id) => tui::DashboardEvent::OperationCompleted(
                terminal::open_node_upgrade(terminal.as_deref(), &node_id)
                    .map(|()| "Opened Node upgrade".into())
                    .map_err(|error| error.to_string()),
            ),
            tui::DashboardEffect::ReauthenticateNode(node_id) => {
                tui::DashboardEvent::OperationCompleted(
                    terminal::open_node_reauthenticate(terminal.as_deref(), &node_id)
                        .map(|()| "Opened Node reauthentication".into())
                        .map_err(|error| error.to_string()),
                )
            }
            tui::DashboardEffect::CheckForUpdates => {
                let result = refresh.check(&client).map_err(|error| error.to_string());
                match result {
                    Ok(Some((snapshot, reset_focus_revision))) => {
                        match combined_node_snapshot_for_dashboard(&client) {
                            Ok(combined) => {
                                tui::DashboardEvent::RefreshCompleted(Ok(dashboard_state(
                                    snapshot,
                                    combined,
                                    &refresh,
                                    &mut git_cache,
                                    reset_focus_revision,
                                )))
                            }
                            Err(error) => {
                                tui::DashboardEvent::RefreshCompleted(Err(error.to_string()))
                            }
                        }
                    }
                    Ok(None) => tui::DashboardEvent::UpdateCheckCompleted,
                    Err(error) => tui::DashboardEvent::RefreshCompleted(Err(error)),
                }
            }
            tui::DashboardEffect::RefreshNode(node_id) => tui::DashboardEvent::OperationCompleted(
                client
                    .force_node_projection_refresh(node_id)
                    .map(|_| "Requested a fresh Node observation".into())
                    .map_err(|error| error.to_string()),
            ),
            tui::DashboardEffect::RestoreDismissedShells(node_id) => {
                tui::DashboardEvent::OperationCompleted(
                    client
                        .restore_dismissed_node_projection_shells(node_id)
                        .map(|_| "Restored dismissed cached shells for the Node".into())
                        .map_err(|error| error.to_string()),
                )
            }
            tui::DashboardEffect::RestoreWorkspace(workspace_id) => {
                let result = (|| {
                    let workspace_id = local_dashboard_inner(&workspace_id, &local_node_id)?;
                    let workspace = client
                        .get_workspace(workspace_id)
                        .map_err(|error| error.to_string())?;
                    open_workspace(&workspace, terminal.as_deref(), None, true)
                        .map_err(|error| error.to_string())?;
                    Ok(format!(
                        "Opened {} launcher(s) and {} shell(s) for {}",
                        workspace.launchers.len(),
                        workspace_user_shell_count(&workspace),
                        workspace.name
                    ))
                })();
                tui::DashboardEvent::OperationCompleted(result)
            }
            tui::DashboardEffect::OpenGlobalWorkspace {
                workspace_id,
                expected_revision,
            } => {
                let result = (|| {
                    let combined = client
                        .combined_node_snapshot(None)
                        .map_err(|error| error.to_string())?;
                    let workspace = combined
                        .workspaces
                        .iter()
                        .find(|workspace| workspace.id == workspace_id)
                        .ok_or_else(|| "global Workspace is no longer retained".to_owned())?;
                    if workspace.revision != expected_revision {
                        return Err("global Workspace revision changed; refresh and retry".into());
                    }
                    show_desktop_workspace(workspace).map_err(|error| error.to_string())?;
                    let outcome = attempt_open_coordinated_workspace(
                        &client,
                        &combined,
                        workspace,
                        terminal.as_deref(),
                        true,
                    )
                    .map_err(|error| error.to_string())?;
                    tui_workspace_open_message(&workspace.name, &outcome)
                })();
                tui::DashboardEvent::WorkspaceOpened(result)
            }
            tui::DashboardEffect::RetryGlobalWorkspaceClose { workspace_id } => {
                let result = client
                    .retry_global_workspace_close(workspace_id)
                    .map(|result| {
                        let unresolved = result
                            .placements
                            .iter()
                            .filter(|placement| placement.status == "unresolved")
                            .count();
                        if unresolved == 0 {
                            "Completed Workspace close".into()
                        } else {
                            format!(
                                "Workspace close still has {unresolved} unresolved placement(s)"
                            )
                        }
                    })
                    .map_err(|error| error.to_string());
                tui::DashboardEvent::OperationCompleted(result)
            }
            tui::DashboardEffect::Open(target) => {
                let result = dispatch_dashboard_open(
                    &target,
                    &local_node_id,
                    |shell_id| {
                        if shell_id.node_id == local_node_id || shell_id.node_id.is_empty() {
                            open_dashboard_shell(&client, &shell_id.inner_id, terminal.as_deref())
                                .map_err(|error| error.to_string())
                        } else {
                            open_dashboard_remote_shell(&client, shell_id, terminal.as_deref())
                                .map_err(|error| error.to_string())
                        }
                    },
                    |workspace_id, launcher_id| {
                        let workspace =
                            routed_dashboard_workspace(&client, workspace_id, &local_node_id)?;
                        let launcher =
                            routed_dashboard_launcher(&client, launcher_id, &local_node_id)?;
                        if launcher.workspace_id != workspace.id {
                            return Err(
                                "launcher ownership changed; refresh before invoking".into()
                            );
                        }
                        if workspace_id.node_id == local_node_id || workspace_id.node_id.is_empty()
                        {
                            invoke_workspace_launcher(&workspace, &launcher)
                                .map_err(|error| error.to_string())?;
                        } else {
                            client
                                .route_node_host_service(
                                    &workspace_id.node_id,
                                    protocol::HostServiceOperation::InvokeLauncher {
                                        workspace_id: workspace.id.clone(),
                                        launcher_id: launcher.id.clone(),
                                    },
                                )
                                .map_err(|error| error.to_string())?;
                        }
                        Ok(format!(
                            "Launched {} from {}",
                            launcher.name, workspace.name
                        ))
                    },
                );
                tui::DashboardEvent::OperationCompleted(result)
            }
            tui::DashboardEffect::Close(target) => {
                let result = (|| match &target {
                    tui::CloseTarget::GlobalWorkspace {
                        workspace_id,
                        expected_revision,
                    } => {
                        let result = client
                            .close_global_workspace(workspace_id, *expected_revision)
                            .map_err(|error| error.to_string())?;
                        let unresolved = result
                            .placements
                            .iter()
                            .filter(|placement| placement.status != "closed")
                            .count();
                        if unresolved > 0 {
                            return Err(format!(
                                "Workspace close remains unresolved on {unresolved} Node(s); refresh and retry"
                            ));
                        }
                        Ok("Closed global Workspace across every placement".into())
                    }
                    tui::CloseTarget::Workspace(workspace_id) => {
                        let workspace =
                            routed_dashboard_workspace(&client, workspace_id, &local_node_id)?;
                        let name = workspace.name.clone();
                        if workspace_id.node_id == local_node_id || workspace_id.node_id.is_empty()
                        {
                            client
                                .close_workspace(&workspace_id.inner_id)
                                .map_err(|error| error.to_string())?;
                        } else {
                            routed_dashboard_operation(
                                &client,
                                workspace_id,
                                &local_node_id,
                                protocol::RoutedOperation::CloseWorkspace {
                                    workspace_id: workspace_id.inner_id.clone(),
                                    expected_revision: workspace.revision,
                                },
                            )?;
                        }
                        Ok(format!("Closed {name}, its launchers, and shells"))
                    }
                    tui::CloseTarget::Shell(shell_id) => {
                        let shell = routed_dashboard_shell(&client, shell_id, &local_node_id)?;
                        let name = shell.name.clone();
                        if shell_id.node_id == local_node_id || shell_id.node_id.is_empty() {
                            client
                                .close_shell(&shell_id.inner_id)
                                .map_err(|error| error.to_string())?;
                        } else {
                            routed_dashboard_operation(
                                &client,
                                shell_id,
                                &local_node_id,
                                protocol::RoutedOperation::CloseShell {
                                    shell_id: shell_id.inner_id.clone(),
                                    expected_revision: shell.revision,
                                },
                            )?;
                        }
                        Ok(format!("Closed shell {name}"))
                    }
                    tui::CloseTarget::DismissCachedShell(shell_id) => {
                        client
                            .dismiss_node_projection_shell(&shell_id.node_id, &shell_id.inner_id)
                            .map_err(|error| error.to_string())?;
                        Ok("Dismissed cached shell without closing its remote process".into())
                    }
                    tui::CloseTarget::Launcher(launcher_id) => {
                        let launcher =
                            routed_dashboard_launcher(&client, launcher_id, &local_node_id)?;
                        let name = launcher.name.clone();
                        if launcher_id.node_id == local_node_id || launcher_id.node_id.is_empty() {
                            client
                                .remove_launcher(&launcher_id.inner_id)
                                .map_err(|error| error.to_string())?;
                        } else {
                            routed_dashboard_operation(
                                &client,
                                launcher_id,
                                &local_node_id,
                                protocol::RoutedOperation::RemoveLauncher {
                                    launcher_id: launcher_id.inner_id.clone(),
                                    expected_revision: launcher.revision,
                                },
                            )?;
                        }
                        Ok(format!("Removed launcher {name}"))
                    }
                })();
                tui::DashboardEvent::OperationCompleted(result)
            }
            tui::DashboardEffect::CreateWorkspace { name } => {
                tui::DashboardEvent::OperationCompleted(
                    create_dashboard_workspace(&client, &name).map_err(|error| error.to_string()),
                )
            }
            tui::DashboardEffect::CreateProjectWorkspace { name, default_cwd } => {
                tui::DashboardEvent::OperationCompleted(
                    create_dashboard_project_workspace(
                        &client,
                        &local_node_id,
                        &name,
                        &default_cwd,
                    )
                    .map_err(|error| error.to_string()),
                )
            }
            tui::DashboardEffect::CreateShell(workspace_id) => {
                tui::DashboardEvent::ShellCreationCompleted(
                    local_dashboard_inner(&workspace_id, &local_node_id).and_then(|workspace_id| {
                        create_dashboard_shell(&client, workspace_id, &launch_cwd)
                            .map_err(|error| error.to_string())
                    }),
                )
            }
            tui::DashboardEffect::CreateGlobalShell {
                workspace_id,
                expected_revision,
                node_id,
                owner_workspace_id,
                default_cwd,
            } => {
                let result = (|| {
                    let combined = client
                        .combined_node_snapshot(Some(node_id.clone()))
                        .map_err(|error| error.to_string())?;
                    let node = combined
                        .nodes
                        .iter()
                        .find(|node| node.node_id == node_id)
                        .ok_or_else(|| "selected Node is no longer available".to_owned())?;
                    let cwd = resolve_owner_directory(
                        &client,
                        node,
                        default_cwd.clone().unwrap_or_else(|| PathBuf::from(".")),
                    )
                    .map_err(|error| error.to_string())?;
                    let name = generated_shell_name(std::iter::empty())
                        .map_err(|error| error.to_string())?;
                    let (_, shell) = client
                        .create_global_workspace_shell(
                            Uuid::new_v4().to_string(),
                            workspace_id,
                            expected_revision,
                            node_id,
                            owner_workspace_id,
                            default_cwd.or_else(|| Some(cwd.clone())),
                            Uuid::new_v4().to_string(),
                            shell_spec(name, &cwd, &[]),
                        )
                        .map_err(|error| error.to_string())?;
                    Ok(format!("Created {} on Node {}", shell.name, node.alias))
                })();
                tui::DashboardEvent::ShellCreationCompleted(result)
            }
            tui::DashboardEffect::AdoptExternalWorkspace {
                identity,
                expected_revision,
            } => {
                let result = (|| {
                    let owner = routed_dashboard_workspace(&client, &identity, &local_node_id)?;
                    if expected_revision > 0 && owner.revision != expected_revision {
                        return Err(format!(
                            "owner Workspace revision changed: expected {expected_revision}, current {}",
                            owner.revision
                        ));
                    }
                    let global = client
                        .adopt_node_workspace(identity, owner.revision)
                        .map_err(|error| error.to_string())?;
                    Ok(format!("Adopted {} as a new Workspace", global.name))
                })();
                tui::DashboardEvent::OperationCompleted(result)
            }
            tui::DashboardEffect::LinkExternalWorkspace {
                workspace_id,
                expected_revision,
                identity,
                expected_owner_revision,
            } => {
                let result = (|| {
                    let owner = routed_dashboard_workspace(&client, &identity, &local_node_id)?;
                    if expected_owner_revision > 0 && owner.revision != expected_owner_revision {
                        return Err(format!(
                            "owner Workspace revision changed: expected {expected_owner_revision}, current {}",
                            owner.revision
                        ));
                    }
                    let global = client
                        .link_node_workspace(
                            workspace_id,
                            expected_revision,
                            identity,
                            owner.revision,
                        )
                        .map_err(|error| error.to_string())?;
                    Ok(format!("Linked owner Workspace to {}", global.name))
                })();
                tui::DashboardEvent::OperationCompleted(result)
            }
            tui::DashboardEffect::RetargetNode {
                node_id,
                expected_revision,
                route,
            } => {
                let result = (|| {
                    let remote = verified_remote_connection(&route, true)
                        .map_err(|error| error.to_string())?;
                    if remote.handshake.node_id != node_id {
                        return Err("retargeted route reached a different Node identity".into());
                    }
                    let registration = client
                        .retarget_node_registration(
                            &node_id,
                            route,
                            node_id.clone(),
                            expected_revision,
                        )
                        .map_err(|error| error.to_string())?;
                    Ok(format!("Retargeted Node {}", registration.alias))
                })();
                tui::DashboardEvent::OperationCompleted(result)
            }
            tui::DashboardEffect::ForgetNode { node_id } => {
                tui::DashboardEvent::OperationCompleted(
                    client
                        .forget_node_registration(node_id)
                        .map(|registration| format!("Forgot Node {}", registration.alias))
                        .map_err(|error| error.to_string()),
                )
            }
            tui::DashboardEffect::Rename { target, name } => {
                let result = (|| match &target {
                    tui::RenameTarget::Node {
                        node_id,
                        expected_revision,
                    } => {
                        client
                            .rename_node_registration(node_id, &name, *expected_revision)
                            .map_err(|error| error.to_string())?;
                        Ok(format!("Renamed Node alias to {name}"))
                    }
                    tui::RenameTarget::GlobalWorkspace {
                        workspace_id,
                        expected_revision,
                    } => {
                        client
                            .rename_global_workspace(workspace_id, *expected_revision, &name)
                            .map_err(|error| error.to_string())?;
                        Ok(format!("Renamed workspace to {name}"))
                    }
                    tui::RenameTarget::Workspace(workspace_id) => {
                        let workspace =
                            routed_dashboard_workspace(&client, workspace_id, &local_node_id)?;
                        if workspace_id.node_id == local_node_id || workspace_id.node_id.is_empty()
                        {
                            client
                                .rename_workspace(&workspace_id.inner_id, &name)
                                .map_err(|error| error.to_string())?;
                        } else {
                            routed_dashboard_operation(
                                &client,
                                workspace_id,
                                &local_node_id,
                                protocol::RoutedOperation::RenameWorkspace {
                                    workspace_id: workspace_id.inner_id.clone(),
                                    name: name.clone(),
                                    expected_revision: workspace.revision,
                                },
                            )?;
                        }
                        Ok(format!("Renamed workspace to {name}"))
                    }
                    tui::RenameTarget::Shell(shell_id) => {
                        let shell = routed_dashboard_shell(&client, shell_id, &local_node_id)?;
                        if shell_id.node_id == local_node_id || shell_id.node_id.is_empty() {
                            client
                                .rename_shell(&shell_id.inner_id, &name)
                                .map_err(|error| error.to_string())?;
                        } else {
                            routed_dashboard_operation(
                                &client,
                                shell_id,
                                &local_node_id,
                                protocol::RoutedOperation::RenameShell {
                                    shell_id: shell_id.inner_id.clone(),
                                    name: name.clone(),
                                    expected_revision: shell.revision,
                                },
                            )?;
                        }
                        Ok(format!("Renamed shell to {name}"))
                    }
                    tui::RenameTarget::Launcher(launcher_id) => {
                        let launcher =
                            routed_dashboard_launcher(&client, launcher_id, &local_node_id)?;
                        if launcher_id.node_id == local_node_id || launcher_id.node_id.is_empty() {
                            client
                                .rename_launcher(&launcher_id.inner_id, &name)
                                .map_err(|error| error.to_string())?;
                        } else {
                            routed_dashboard_operation(
                                &client,
                                launcher_id,
                                &local_node_id,
                                protocol::RoutedOperation::RenameLauncher {
                                    launcher_id: launcher_id.inner_id.clone(),
                                    name: name.clone(),
                                    expected_revision: launcher.revision,
                                },
                            )?;
                        }
                        Ok(format!("Renamed launcher to {name}"))
                    }
                })();
                tui::DashboardEvent::OperationCompleted(result)
            }
            tui::DashboardEffect::Refresh => {
                let result = (|| {
                    let (snapshot, reset_focus_revision) = refresh
                        .refresh(&client)
                        .map_err(|error| error.to_string())?;
                    Ok(dashboard_state(
                        snapshot,
                        combined_node_snapshot_for_dashboard(&client)
                            .map_err(|error| error.to_string())?,
                        &refresh,
                        &mut git_cache,
                        reset_focus_revision,
                    ))
                })();
                tui::DashboardEvent::RefreshCompleted(result)
            }
            tui::DashboardEffect::ReadTerminalPreview {
                shell_id,
                run_id,
                output_revision,
            } => tui::DashboardEvent::TerminalPreviewCompleted {
                output: local_dashboard_inner(&shell_id, &local_node_id).and_then(|id| {
                    client
                        .read_shell_preview(id, READ_BYTES, 500)
                        .map_err(|error| error.to_string())
                }),
                shell_id: shell_id.inner_id,
                run_id,
                output_revision,
            },
            tui::DashboardEffect::LoadSessions { .. }
            | tui::DashboardEffect::InspectSession(_)
            | tui::DashboardEffect::ActivateSession(_)
            | tui::DashboardEffect::SetSessionDisplayName { .. } => {
                unreachable!("Session effects use the dedicated dashboard worker")
            }
        },
        move |effect| {
            dashboard_session_effect(effect, &session_local_node_id, session_terminal.as_deref())
        },
    )?;
    Ok(())
}

fn dashboard_state(
    snapshot: Snapshot,
    combined: Option<protocol::CombinedNodeSnapshot>,
    refresh: &DashboardRefresh,
    git_cache: &mut git::Cache,
    reset_focus_revision: bool,
) -> tui::DashboardState {
    let qualified_focused_terminal = combined
        .as_ref()
        .and_then(|snapshot| snapshot.focused_terminal.clone());
    let sessions = session_projection::project_snapshot_with_catalog(&snapshot, None);
    let mut workspaces =
        dashboard_projection::project_with_sessions(&snapshot.workspaces, git_cache, &sessions);
    let mut nodes = Vec::new();
    if let Some(combined) = combined {
        let global_workspaces = combined.workspaces;
        let external_workspaces = combined.external_workspaces;
        for node in combined.nodes {
            let node_view = tui::NodeView {
                id: node.node_id.clone(),
                alias: node.alias.clone(),
                local: node.local,
                route: node.route.clone(),
                registration_revision: node.registration_revision,
                health: node.health,
                current: node.current,
                stale: node.stale,
                observed_at_ms: node.observed_at_ms,
                observed_protocol_version: node.observed_protocol_version,
                observed_helper_version: node.observed_helper_version.clone(),
                observed_capabilities: node.observed_capabilities.clone(),
                workspace_owner_eligible: node.workspace_owner_eligible,
                workspace_owner_unavailable_reason: node.workspace_owner_unavailable_reason.clone(),
            };
            if node.local {
                assign_local_workspace_node(&mut workspaces, &node_view);
            } else {
                let mut remote_workspaces = dashboard_projection::project_remote_node(&node);
                workspaces.append(&mut remote_workspaces);
            }
            nodes.push(node_view);
        }
        workspaces =
            group_dashboard_workspaces(workspaces, &nodes, global_workspaces, external_workspaces);
    }
    if nodes.is_empty() {
        nodes.extend(
            workspaces
                .iter()
                .map(|workspace| workspace.node.clone())
                .take(1),
        );
    }
    sort_dashboard_workspaces(&mut workspaces);
    tui::DashboardState {
        nodes,
        workspaces,
        cached_projection_dismissal: protocol::ProtocolFeature::CachedProjectionDismissal
            .is_supported_by(refresh.negotiated_protocol),
        node_reauthentication: protocol::ProtocolFeature::GlobalWorkspaces
            .is_supported_by(refresh.negotiated_protocol),
        focused_terminal: dashboard_focused_terminal_view(
            refresh.negotiated_protocol,
            qualified_focused_terminal,
            snapshot.focused_terminal,
        ),
        reset_focus_revision,
    }
}

fn dashboard_session_effect(
    effect: tui::DashboardEffect,
    local_node_id: &str,
    terminal_entry: Option<&str>,
) -> tui::DashboardEvent {
    match effect {
        tui::DashboardEffect::LoadSessions { nodes } => {
            tui::DashboardEvent::SessionsLoaded(load_dashboard_sessions(&nodes))
        }
        tui::DashboardEffect::InspectSession(identity) => {
            let result = inspect_session_on_node(&identity, local_node_id).map(|inspection| {
                let action = session_open_action(&inspection).and_then(|action| {
                    if inspection.summary.state_is_current {
                        Ok(action)
                    } else {
                        validate_session_cwd(&identity, local_node_id, &inspection).map(|_| action)
                    }
                });
                Box::new(dashboard_session_details(&identity, inspection, action))
            });
            tui::DashboardEvent::SessionInspected { identity, result }
        }
        tui::DashboardEffect::ActivateSession(identity) => tui::DashboardEvent::OperationCompleted(
            open_exact_session(&identity, local_node_id, terminal_entry, None),
        ),
        tui::DashboardEffect::SetSessionDisplayName {
            identity,
            expected_workspace_revision,
            display_name,
        } => {
            let result = (|| {
                let client = client::connect().map_err(|error| error.to_string())?;
                let operation_id = Uuid::new_v4().to_string();
                let result = if identity.node_id == local_node_id || identity.node_id.is_empty() {
                    client.set_agent_session_display_name(
                        operation_id,
                        identity.inner_id,
                        expected_workspace_revision,
                        display_name.clone(),
                    )
                } else {
                    let result = client
                        .route_node_operation(
                            identity.node_id,
                            protocol::RoutedOperation::SetAgentSessionDisplayName {
                                operation_id,
                                session_id: identity.inner_id,
                                expected_workspace_revision,
                                display_name: display_name.clone(),
                            },
                        )
                        .map_err(|error| error.to_string())?;
                    let protocol::RoutedOperationResult::AgentSessionDisplayName {
                        outcome: result,
                    } = result
                    else {
                        return Err(
                            "Node returned an unexpected Session display-name response".into()
                        );
                    };
                    Ok(result)
                }
                .map_err(|error| error.to_string())?;
                Ok(result)
            })();
            tui::DashboardEvent::SessionDisplayNameMutationCompleted(result)
        }
        effect => panic!("unexpected effect on Session worker: {effect:?}"),
    }
}

fn load_dashboard_sessions(
    nodes: &[tui::SessionNodeView],
) -> Result<tui::SessionLoadResult, String> {
    let client = client::connect().map_err(|error| error.to_string())?;
    let loads = nodes.iter().cloned().map(|node| {
        let operation = protocol::HostServiceOperation::ListAgentSessions { workspace_id: None };
        let result = if node.local {
            client.host_service(operation)
        } else {
            client.route_node_host_service(&node.id, operation)
        };
        let result = match result {
            Ok(protocol::HostServiceResult::AgentSessions {
                sessions: node_sessions,
            }) => Ok(node_sessions),
            Ok(_) => Err("returned an unexpected Session-list response".into()),
            Err(error) => Err(error.to_string()),
        };
        DashboardSessionNodeLoad { node, result }
    });
    aggregate_dashboard_session_loads(loads)
}

struct DashboardSessionNodeLoad {
    node: tui::SessionNodeView,
    result: Result<Vec<protocol::HostAgentSessionSummary>, String>,
}

fn aggregate_dashboard_session_loads(
    loads: impl IntoIterator<Item = DashboardSessionNodeLoad>,
) -> Result<tui::SessionLoadResult, String> {
    let mut sessions = Vec::new();
    let mut warnings = Vec::new();
    let mut successful_nodes = 0usize;
    for load in loads {
        match load.result {
            Ok(node_sessions) => {
                successful_nodes += 1;
                sessions.extend(node_sessions.into_iter().map(|session| {
                    dashboard_session_summary(
                        &load.node.id,
                        &load.node.alias,
                        load.node.session_display_names,
                        session,
                    )
                }));
            }
            Err(error) => warnings.push(format!("Node {}: {error}", load.node.alias)),
        }
    }
    if successful_nodes == 0 && !warnings.is_empty() {
        return Err(format!("Unable to load Sessions: {}", warnings.join("; ")));
    }
    sessions.sort_by(|left, right| {
        right
            .last_at_ms
            .cmp(&left.last_at_ms)
            .then_with(|| left.node_id.cmp(&right.node_id))
            .then_with(|| left.workspace_id.cmp(&right.workspace_id))
            .then_with(|| left.session_id.cmp(&right.session_id))
    });
    Ok(tui::SessionLoadResult { sessions, warnings })
}

fn dashboard_session_summary(
    node_id: &str,
    node_alias: &str,
    session_display_names: bool,
    session: protocol::HostAgentSessionSummary,
) -> tui::DashboardSessionView {
    tui::DashboardSessionView {
        node_id: node_id.to_owned(),
        node_alias: node_alias.to_owned(),
        session_id: session.id,
        workspace_id: session.workspace_id,
        workspace_name: session.workspace_name,
        title: session.description,
        user_display_name: session.user_display_name,
        workspace_revision: session.workspace_revision,
        session_display_names,
        integration: session.integration,
        state: session.state.into(),
        state_is_current: session.state_is_current,
        started_at_ms: session.started_at_ms,
        last_at_ms: session.last_at_ms,
        occurrence_count: session.occurrence_count,
    }
}

fn inspect_session_on_node(
    identity: &protocol::QualifiedIdentity,
    local_node_id: &str,
) -> Result<protocol::HostAgentSessionInspection, String> {
    let client = client::connect().map_err(|error| error.to_string())?;
    let operation = protocol::HostServiceOperation::InspectAgentSession {
        session_id: identity.inner_id.clone(),
    };
    let result = if identity.node_id == local_node_id || identity.node_id.is_empty() {
        client.host_service(operation)
    } else {
        client.route_node_host_service(&identity.node_id, operation)
    }
    .map_err(|error| error.to_string())?;
    match result {
        protocol::HostServiceResult::AgentSession { session } => Ok(session),
        _ => Err("Node returned an unexpected Session inspection response".into()),
    }
}

fn dashboard_session_details(
    identity: &protocol::QualifiedIdentity,
    inspection: protocol::HostAgentSessionInspection,
    action: Result<String, String>,
) -> tui::DashboardSessionDetails {
    let node_alias = if identity.node_id.is_empty() {
        "local".into()
    } else {
        identity.node_id.clone()
    };
    tui::DashboardSessionDetails {
        session: dashboard_session_summary(
            &identity.node_id,
            &node_alias,
            false,
            inspection.summary,
        ),
        source_cwd: inspection.source_cwd,
        occurrences: inspection.occurrences,
        action,
    }
}

fn session_open_action(
    inspection: &protocol::HostAgentSessionInspection,
) -> Result<String, String> {
    if inspection.summary.state == AgentState::Done {
        return Err("Session is permanently done".into());
    }
    if inspection.summary.state_is_current {
        return Ok("open exact current managed Shell".into());
    }
    if inspection.source_cwd.is_none() {
        return Err("Session has no retained source cwd".into());
    }
    let integration = boomux::integrations::by_key(&inspection.summary.integration)
        .ok_or_else(|| "Session uses an unknown integration".to_owned())?;
    if integration.resume.is_none() || inspection.summary.external_session_id.is_none() {
        return Err(format!(
            "{} cannot resume this exact Session",
            integration.display_name
        ));
    }
    Ok("resume exact historical Session".into())
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum SessionOpenPlan {
    Current {
        node_id: Option<String>,
        shell_id: String,
        run_id: String,
        title: String,
        description: String,
    },
    Historical {
        node_id: Option<String>,
        session_id: String,
        title: String,
        description: String,
    },
}

fn session_open_plan(
    identity: &protocol::QualifiedIdentity,
    local_node_id: &str,
    inspection: &protocol::HostAgentSessionInspection,
    workspace: Option<&WorkspaceSnapshot>,
) -> Result<SessionOpenPlan, String> {
    session_open_action(inspection)?;
    let summary = &inspection.summary;
    let node_id = (identity.node_id != local_node_id && !identity.node_id.is_empty())
        .then(|| identity.node_id.clone());
    let title = format!(
        "{} - {} Session",
        summary.workspace_name, summary.integration
    );
    if !summary.state_is_current {
        return Ok(SessionOpenPlan::Historical {
            node_id,
            session_id: identity.inner_id.clone(),
            title,
            description: summary.description.clone(),
        });
    }

    let workspace =
        workspace.ok_or_else(|| "Session owner Workspace is no longer available".to_owned())?;
    if workspace.id != summary.workspace_id {
        return Err("Session owner Workspace identity changed; refusing ambiguity".into());
    }
    let mut current = inspection.occurrences.iter().filter_map(|occurrence| {
        if occurrence.ended_at_ms.is_some()
            || matches!(
                occurrence.observation.state,
                AgentState::Inactive | AgentState::Done
            )
        {
            return None;
        }
        workspace
            .shells
            .iter()
            .find(|shell| {
                shell.id == occurrence.shell_id
                    && shell.status == ShellStatus::Running
                    && shell
                        .run
                        .as_ref()
                        .is_some_and(|run| run.id == occurrence.run_id)
            })
            .map(|shell| (shell.id.clone(), occurrence.run_id.clone()))
    });
    let (shell_id, run_id) = current
        .next()
        .ok_or_else(|| "Session no longer has an exact current managed Shell".to_owned())?;
    if current.next().is_some() {
        return Err("Session has multiple current managed Shells; refusing ambiguity".into());
    }
    Ok(SessionOpenPlan::Current {
        node_id,
        shell_id,
        run_id,
        title,
        description: summary.description.clone(),
    })
}

fn dispatch_session_open<L, R, H>(
    plan: &SessionOpenPlan,
    mut open_local: L,
    mut open_remote: R,
    mut resume: H,
) -> Result<String, String>
where
    L: FnMut(&str, &str, &str) -> Result<(), String>,
    R: FnMut(&str, &str, &str, &str) -> Result<(), String>,
    H: FnMut(Option<&str>, &str, &str) -> Result<(), String>,
{
    match plan {
        SessionOpenPlan::Current {
            node_id,
            shell_id,
            run_id,
            title,
            description,
        } => {
            if let Some(node_id) = node_id {
                open_remote(node_id, shell_id, run_id, title)?;
            } else {
                open_local(shell_id, run_id, title)?;
            }
            Ok(format!("Opened terminal for current Session {description}"))
        }
        SessionOpenPlan::Historical {
            node_id,
            session_id,
            title,
            description,
        } => {
            resume(node_id.as_deref(), session_id, title)?;
            Ok(format!(
                "Opened terminal for historical Session {description}"
            ))
        }
    }
}

fn validate_session_cwd(
    identity: &protocol::QualifiedIdentity,
    local_node_id: &str,
    inspection: &protocol::HostAgentSessionInspection,
) -> Result<PathBuf, String> {
    let cwd = inspection
        .source_cwd
        .clone()
        .ok_or_else(|| "Session has no retained source cwd".to_owned())?;
    let client = client::connect().map_err(|error| error.to_string())?;
    let operation = protocol::HostServiceOperation::ResolveDirectory { path: cwd };
    let result = if identity.node_id == local_node_id || identity.node_id.is_empty() {
        client.host_service(operation)
    } else {
        client.route_node_host_service(&identity.node_id, operation)
    }
    .map_err(|error| format!("Session source cwd is unavailable: {error}"))?;
    match result {
        protocol::HostServiceResult::Directory { path } => Ok(path),
        _ => Err("Node returned an unexpected source cwd response".into()),
    }
}

fn open_exact_session(
    identity: &protocol::QualifiedIdentity,
    local_node_id: &str,
    terminal_entry: Option<&str>,
    coordinated_workspace: Option<&str>,
) -> Result<String, String> {
    let inspection = inspect_session_on_node(identity, local_node_id)?;
    session_open_action(&inspection)?;
    let summary = &inspection.summary;
    let remote = identity.node_id != local_node_id && !identity.node_id.is_empty();
    let (workspace, source_cwd) = if summary.state_is_current {
        let client = client::connect().map_err(|error| error.to_string())?;
        (
            Some(if remote {
                match client
                    .route_node_operation(
                        &identity.node_id,
                        protocol::RoutedOperation::GetWorkspace {
                            workspace_id: summary.workspace_id.clone(),
                        },
                    )
                    .map_err(|error| error.to_string())?
                {
                    protocol::RoutedOperationResult::Workspace { workspace } => workspace,
                    _ => return Err("Node returned an unexpected Workspace response".into()),
                }
            } else {
                client
                    .get_workspace(&summary.workspace_id)
                    .map_err(|error| error.to_string())?
            }),
            None,
        )
    } else {
        (
            None,
            Some(validate_session_cwd(identity, local_node_id, &inspection)?),
        )
    };
    if !summary.state_is_current
        && let Some(target) = coordinated_workspace
    {
        open_managed_historical_session(
            identity,
            &inspection,
            source_cwd.as_deref().expect("historical Session cwd"),
            target,
            terminal_entry,
        )
        .map_err(|error| error.to_string())?;
        let opened = format!(
            "Opened managed terminal for historical Session {}",
            summary.description
        );
        return Ok(with_session_attention_warnings(
            opened,
            acknowledge_session_attentions(identity, local_node_id, &summary.attentions),
        ));
    }
    let placement = coordinated_workspace
        .map(|target| {
            let client = client::connect().map_err(|error| error.to_string())?;
            prepare_session_desktop_placement(&client, target).map_err(|error| error.to_string())
        })
        .transpose()?
        .flatten();
    let plan = session_open_plan(identity, local_node_id, &inspection, workspace.as_ref())?;
    let opened = dispatch_session_open(
        &plan,
        |shell_id, run_id, title| {
            if let Some(workspace_id) = placement.as_deref() {
                terminal::open_exact_run_placed(
                    terminal_entry,
                    shell_id,
                    run_id,
                    title,
                    true,
                    terminal::HyprlandPlacement {
                        workspace_id,
                        node_id: local_node_id,
                        shell_id,
                    },
                )
            } else {
                terminal::open_exact_run(terminal_entry, shell_id, run_id, title, true)
            }
            .map_err(|error| error.to_string())
        },
        |node_id, shell_id, run_id, title| {
            if let Some(workspace_id) = placement.as_deref() {
                terminal::open_remote_exact_run_placed(
                    terminal_entry,
                    node_id,
                    shell_id,
                    run_id,
                    title,
                    true,
                    workspace_id,
                )
            } else {
                terminal::open_remote_exact_run(
                    terminal_entry,
                    node_id,
                    shell_id,
                    run_id,
                    title,
                    true,
                )
            }
            .map_err(|error| error.to_string())
        },
        |node_id, session_id, title| {
            terminal::open_agent_session(terminal_entry, node_id, session_id, title)
                .map_err(|error| error.to_string())
        },
    )?;
    Ok(with_session_attention_warnings(
        opened,
        acknowledge_session_attentions(identity, local_node_id, &summary.attentions),
    ))
}

fn acknowledge_session_attentions(
    identity: &protocol::QualifiedIdentity,
    local_node_id: &str,
    attentions: &[protocol::HostAgentSessionAttention],
) -> Vec<String> {
    if attentions.is_empty() {
        return Vec::new();
    }
    let client = match client::connect() {
        Ok(client) => client,
        Err(error) => return vec![error.to_string()],
    };
    let remote = identity.node_id != local_node_id && !identity.node_id.is_empty();
    attentions
        .iter()
        .filter_map(|attention| {
            let result = if remote {
                client
                    .route_node_operation(
                        &identity.node_id,
                        protocol::RoutedOperation::AcknowledgeAgentAttention {
                            agent_id: attention.agent_id.clone(),
                            observation_revision: attention.observation_revision,
                        },
                    )
                    .map(|_| ())
            } else {
                client
                    .acknowledge_agent_attention(
                        &attention.agent_id,
                        attention.observation_revision,
                    )
                    .map(|_| ())
            };
            result
                .err()
                .map(|error| format!("{}: {error}", attention.agent_id))
        })
        .collect()
}

fn with_session_attention_warnings(opened: String, warnings: Vec<String>) -> String {
    if warnings.is_empty() {
        opened
    } else {
        format!(
            "{opened}; Session opened but attention remains: {}",
            warnings.join("; ")
        )
    }
}

fn open_managed_historical_session(
    identity: &protocol::QualifiedIdentity,
    inspection: &protocol::HostAgentSessionInspection,
    cwd: &Path,
    coordinated_workspace: &str,
    terminal_entry: Option<&str>,
) -> Result<(), Box<dyn Error>> {
    let summary = &inspection.summary;
    let external_session_id = summary.external_session_id.as_deref().ok_or_else(|| {
        cli_output::failure("invalid_argument", "Session has no canonical external ID")
    })?;
    let integration = boomux::integrations::by_key(&summary.integration)
        .ok_or_else(|| cli_output::failure("invalid_argument", "unknown Agent integration"))?;
    let resume = integration.resume.ok_or_else(|| {
        cli_output::failure(
            "invalid_argument",
            "integration does not support exact Session resume",
        )
    })?;
    let resume_command = resume.command(&[], external_session_id).ok_or_else(|| {
        cli_output::failure(
            "invalid_argument",
            "could not build exact Session resume argv",
        )
    })?;
    let client = client::connect()?;
    let context = WorkspaceCreationContext::load(&client)?;
    let selection = context
        .coordinated_owner(coordinated_workspace, Some(&identity.node_id))?
        .ok_or_else(|| {
            cli_output::failure(
                "not_found",
                format!("coordinated Workspace not found: {coordinated_workspace}"),
            )
        })?;
    let boomux_executable = if selection.node.local {
        env::current_exe()?
            .into_os_string()
            .into_string()
            .map_err(|_| "Boomux executable path is not valid UTF-8")?
    } else {
        "boomux".into()
    };
    let command = supervised_session_resume_command(
        &boomux_executable,
        integration.display_name,
        integration.key,
        external_session_id,
        &resume_command,
    );
    let shell_id = Uuid::new_v4().to_string();
    let suffix = Uuid::new_v4().simple().to_string();
    let name = format!("{}-session-{}", summary.integration, &suffix[..8]);
    let title = if selection.node.local {
        format!("{} - {name}", selection.workspace.name)
    } else {
        format!(
            "[{}] {} - {name}",
            selection.node.alias, selection.workspace.name
        )
    };
    let gate = if hyprland_special_workspaces_enabled()? {
        terminal::open_waiting_placed(
            terminal_entry,
            (!selection.node.local).then_some(selection.node.node_id.as_str()),
            &selection.node.node_id,
            &selection.workspace.id,
            &shell_id,
            &title,
        )?
    } else {
        terminal::open_waiting(
            terminal_entry,
            (!selection.node.local).then_some(selection.node.node_id.as_str()),
            &shell_id,
            &title,
        )?
    };
    let owner_default_cwd = selection
        .default_cwd
        .clone()
        .or_else(|| Some(cwd.to_path_buf()));
    let (_, shell) = client.create_global_workspace_shell(
        Uuid::new_v4().to_string(),
        &selection.workspace.id,
        selection.workspace.revision,
        &selection.node.node_id,
        selection.owner_workspace_id,
        owner_default_cwd,
        shell_id,
        shell_spec(name, cwd, &command),
    )?;
    gate.release().map_err(|error| {
        format!(
            "Shell {} was created but its terminal could not attach: {error}",
            shell.id
        )
    })?;
    Ok(())
}

fn supervised_session_resume_command(
    boomux_executable: &str,
    agent_name: &str,
    integration: &str,
    external_session_id: &str,
    resume_command: &[String],
) -> Vec<String> {
    let mut command = vec![
        boomux_executable.to_owned(),
        "agent".into(),
        "supervise".into(),
        agent_name.to_owned(),
        "--integration".into(),
        integration.to_owned(),
        "--external-session-id".into(),
        external_session_id.to_owned(),
        "--".into(),
    ];
    command.extend_from_slice(resume_command);
    command
}

fn assign_local_workspace_node(workspaces: &mut [tui::WorkspaceView], node: &tui::NodeView) {
    for workspace in workspaces {
        workspace.node = node.clone();
        for owner in &mut workspace.item_owners {
            owner.node = node.clone();
        }
    }
}

fn sort_dashboard_workspaces(workspaces: &mut [tui::WorkspaceView]) {
    workspaces.sort_by(|left, right| {
        left.name
            .cmp(&right.name)
            .then_with(|| left.id.cmp(&right.id))
    });
}

fn combined_node_snapshot_for_dashboard(
    client: &client::Client,
) -> Result<Option<protocol::CombinedNodeSnapshot>, client::ClientError> {
    if client.supports(protocol::ProtocolFeature::CombinedNodeSnapshot)? {
        client.combined_node_snapshot(None).map(Some)
    } else {
        Ok(None)
    }
}

fn group_dashboard_workspaces(
    mut owner_workspaces: Vec<tui::WorkspaceView>,
    nodes: &[tui::NodeView],
    global_workspaces: Vec<protocol::GlobalWorkspaceSnapshot>,
    external_workspaces: Vec<protocol::ExternalWorkspaceSnapshot>,
) -> Vec<tui::WorkspaceView> {
    let fallback_node = nodes
        .iter()
        .find(|node| node.local)
        .or_else(|| nodes.first())
        .cloned();
    let mut grouped = Vec::new();
    for global in global_workspaces {
        let mut items = Vec::new();
        let mut item_owners = Vec::new();
        let mut sessions = Vec::new();
        let mut attention = Vec::new();
        let mut counts = agent_attention_projection::AgentStateCounts::default();
        let mut attention_count = 0;
        let placements = global
            .placements
            .iter()
            .map(|placement| {
                let node = nodes
                    .iter()
                    .find(|node| node.id == placement.node_id)
                    .cloned()
                    .unwrap_or_else(|| unavailable_placement_node(&placement.node_id));
                if let Some(index) = owner_workspaces.iter().position(|workspace| {
                    workspace.node.id == placement.node_id && workspace.id == placement.workspace_id
                }) {
                    let owner = owner_workspaces.remove(index);
                    let owner_item_count = owner.items.len();
                    if owner.item_owners.len() == owner_item_count {
                        item_owners.extend(owner.item_owners);
                    } else {
                        item_owners.extend((0..owner_item_count).map(|_| {
                            tui::WorkspaceItemOwnerView {
                                node: owner.node.clone(),
                                workspace_id: owner.id.clone(),
                            }
                        }));
                    }
                    items.extend(owner.items);
                    sessions.extend(owner.sessions);
                    attention.extend(owner.attention.into_iter().map(|mut attention| {
                        attention.node_id = owner.node.id.clone();
                        attention.workspace_id = owner.id.clone();
                        attention
                    }));
                    counts.unknown += owner.agent_state_counts.unknown;
                    counts.working += owner.agent_state_counts.working;
                    counts.blocked += owner.agent_state_counts.blocked;
                    counts.idle += owner.agent_state_counts.idle;
                    counts.inactive += owner.agent_state_counts.inactive;
                    counts.done += owner.agent_state_counts.done;
                    attention_count += owner.attention_count;
                }
                tui::WorkspacePlacementView {
                    node,
                    workspace_id: placement.workspace_id.clone(),
                    owner_revision: placement.owner_revision,
                    default_cwd: placement
                        .default_cwd
                        .as_ref()
                        .map(|cwd| cwd.display().to_string()),
                    state: placement.state,
                }
            })
            .collect::<Vec<_>>();
        let Some(node) = fallback_node
            .clone()
            .or_else(|| placements.first().map(|placement| placement.node.clone()))
        else {
            continue;
        };
        grouped.push(tui::WorkspaceView {
            node,
            id: global.id,
            name: global.name,
            default_cwd: None,
            items,
            sessions,
            agent_state_counts: counts,
            attention_count,
            attention,
            item_owners,
            coordination: tui::WorkspaceCoordinationView::Global {
                revision: global.revision,
                closing: global.closing,
                placements,
            },
        });
    }
    for external in external_workspaces {
        if let Some(workspace) = owner_workspaces.iter_mut().find(|workspace| {
            workspace.node.id == external.identity.node_id
                && workspace.id == external.identity.inner_id
        }) {
            workspace.coordination = tui::WorkspaceCoordinationView::External {
                owner_revision: external.revision,
                available: external.available,
            };
            continue;
        }
        let Some(node) = nodes
            .iter()
            .find(|node| node.id == external.identity.node_id)
            .cloned()
        else {
            continue;
        };
        grouped.push(tui::WorkspaceView {
            node,
            id: external.identity.inner_id,
            name: external.name,
            default_cwd: external
                .default_cwd
                .as_ref()
                .map(|cwd| cwd.display().to_string()),
            items: Vec::new(),
            sessions: Vec::new(),
            agent_state_counts: agent_attention_projection::AgentStateCounts::default(),
            attention_count: 0,
            attention: Vec::new(),
            item_owners: Vec::new(),
            coordination: tui::WorkspaceCoordinationView::External {
                owner_revision: external.revision,
                available: external.available,
            },
        });
    }
    grouped.extend(owner_workspaces);
    grouped
}

fn unavailable_placement_node(node_id: &str) -> tui::NodeView {
    tui::NodeView {
        id: node_id.to_owned(),
        alias: format!(
            "unregistered:{}",
            node_id.chars().take(8).collect::<String>()
        ),
        local: false,
        route: None,
        registration_revision: None,
        health: protocol::NodeProjectionHealthCode::Unobserved,
        current: false,
        stale: true,
        observed_at_ms: 0,
        observed_protocol_version: None,
        observed_helper_version: None,
        observed_capabilities: Vec::new(),
        workspace_owner_eligible: false,
        workspace_owner_unavailable_reason: Some("Node is not registered".into()),
    }
}

fn focused_terminal_view(focused: protocol::FocusedTerminalSnapshot) -> tui::FocusedTerminalView {
    tui::FocusedTerminalView {
        revision: focused.revision,
        node_id: None,
        workspace_id: focused.workspace_id,
        shell_id: focused.shell_id,
    }
}

fn qualified_focused_terminal_view(
    focused: protocol::QualifiedFocusedTerminalSnapshot,
) -> tui::FocusedTerminalView {
    tui::FocusedTerminalView {
        revision: focused.revision,
        node_id: Some(focused.shell.node_id),
        workspace_id: String::new(),
        shell_id: focused.shell.inner_id,
    }
}

fn dashboard_focused_terminal_view(
    negotiated_protocol: u32,
    qualified: Option<protocol::QualifiedFocusedTerminalSnapshot>,
    local: Option<protocol::FocusedTerminalSnapshot>,
) -> Option<tui::FocusedTerminalView> {
    if protocol::ProtocolFeature::QualifiedFocusedTerminal.is_supported_by(negotiated_protocol) {
        qualified.map(qualified_focused_terminal_view)
    } else {
        local.map(focused_terminal_view)
    }
}

fn dispatch_dashboard_open<S, L>(
    target: &tui::OpenTarget,
    _local_node_id: &str,
    mut open_shell: S,
    mut launch: L,
) -> Result<String, String>
where
    S: FnMut(&protocol::QualifiedIdentity) -> Result<String, String>,
    L: FnMut(&protocol::QualifiedIdentity, &protocol::QualifiedIdentity) -> Result<String, String>,
{
    match target {
        tui::OpenTarget::Shell(shell_id) => open_shell(shell_id),
        tui::OpenTarget::Launcher {
            workspace_id,
            launcher_id,
        } => {
            if workspace_id.node_id != launcher_id.node_id {
                return Err("workspace and launcher belong to different Nodes".into());
            }
            launch(workspace_id, launcher_id)
        }
    }
}

fn local_dashboard_inner<'a>(
    identity: &'a protocol::QualifiedIdentity,
    local_node_id: &str,
) -> Result<&'a str, String> {
    if identity.node_id == local_node_id || identity.node_id.is_empty() {
        Ok(&identity.inner_id)
    } else {
        Err(format!(
            "remote Node {} is read-only in this Boomux version",
            identity.node_id
        ))
    }
}

fn routed_dashboard_workspace(
    client: &client::Client,
    identity: &protocol::QualifiedIdentity,
    local_node_id: &str,
) -> Result<protocol::WorkspaceSnapshot, String> {
    if identity.node_id == local_node_id || identity.node_id.is_empty() {
        return client
            .get_workspace(&identity.inner_id)
            .map_err(|error| error.to_string());
    }
    match client
        .route_node_operation(
            &identity.node_id,
            protocol::RoutedOperation::GetWorkspace {
                workspace_id: identity.inner_id.clone(),
            },
        )
        .map_err(|error| error.to_string())?
    {
        protocol::RoutedOperationResult::Workspace { workspace } => Ok(workspace),
        _ => Err("remote Node returned an unexpected workspace response".into()),
    }
}

fn routed_dashboard_shell(
    client: &client::Client,
    identity: &protocol::QualifiedIdentity,
    local_node_id: &str,
) -> Result<protocol::ShellSnapshot, String> {
    if identity.node_id == local_node_id || identity.node_id.is_empty() {
        return client
            .get_shell(&identity.inner_id)
            .map_err(|error| error.to_string());
    }
    match client
        .route_node_operation(
            &identity.node_id,
            protocol::RoutedOperation::GetShell {
                shell_id: identity.inner_id.clone(),
            },
        )
        .map_err(|error| error.to_string())?
    {
        protocol::RoutedOperationResult::Shell { shell } => Ok(shell),
        _ => Err("remote Node returned an unexpected shell response".into()),
    }
}

fn routed_dashboard_launcher(
    client: &client::Client,
    identity: &protocol::QualifiedIdentity,
    local_node_id: &str,
) -> Result<protocol::WorkspaceLauncherSnapshot, String> {
    if identity.node_id == local_node_id || identity.node_id.is_empty() {
        return client
            .get_launcher(&identity.inner_id)
            .map_err(|error| error.to_string());
    }
    match client
        .route_node_operation(
            &identity.node_id,
            protocol::RoutedOperation::GetLauncher {
                launcher_id: identity.inner_id.clone(),
            },
        )
        .map_err(|error| error.to_string())?
    {
        protocol::RoutedOperationResult::Launcher { launcher } => Ok(launcher),
        _ => Err("remote Node returned an unexpected launcher response".into()),
    }
}

fn routed_dashboard_operation(
    client: &client::Client,
    identity: &protocol::QualifiedIdentity,
    local_node_id: &str,
    operation: protocol::RoutedOperation,
) -> Result<protocol::RoutedOperationResult, String> {
    if identity.node_id == local_node_id || identity.node_id.is_empty() {
        return Err("internal error: local operation was sent through Node routing".into());
    }
    client
        .route_node_operation(&identity.node_id, operation)
        .map_err(|error| error.to_string())
}

fn dashboard_session_resume_plan(
    session: &session_projection::SessionProjection,
) -> Result<(PathBuf, Vec<String>), String> {
    if session.state_is_current {
        return Err(
            "Agent Session is already active in a managed shell; open that shell instead".into(),
        );
    }
    if session.state == AgentState::Done {
        return Err("Agent Session is permanently done and cannot be resumed".into());
    }
    let external_session_id = session
        .external_session_id
        .as_deref()
        .ok_or("Agent Session has no canonical external session ID")?;
    let descriptor = boomux::integrations::by_key(&session.integration)
        .ok_or_else(|| format!("unknown Agent integration {}", session.integration))?;
    let resume = descriptor.resume.ok_or_else(|| {
        format!(
            "{} does not support interactive session resume",
            descriptor.display_name
        )
    })?;
    let command = resume.command(&[], external_session_id).ok_or_else(|| {
        format!(
            "{} could not construct a session resume command",
            descriptor.display_name
        )
    })?;
    let cwd = session
        .source_cwd
        .as_deref()
        .ok_or("Agent Session has no retained working directory")?;
    let cwd = resolve_directory(cwd).map_err(|error| error.to_string())?;
    Ok((cwd, command))
}

#[cfg(test)]
fn dashboard_views(
    workspaces: &[WorkspaceSnapshot],
    git_cache: &mut git::Cache,
) -> Vec<tui::WorkspaceView> {
    dashboard_projection::project(workspaces, git_cache)
}

fn open_directory(
    path: &Path,
    requested_name: Option<&str>,
    startup_command: &[String],
    open_in_new_window: bool,
    terminal: Option<&str>,
) -> Result<(), Box<dyn Error>> {
    ensure_host_terminal()?;
    let directory = resolve_directory(path)?;
    let requested_name = requested_name
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(str::to_owned);
    let client = client::connect_or_start()?;
    let snapshot = client.snapshot()?;
    let (shell, workspace_name, created_workspace) = if let Some(name) = requested_name {
        let (shell, created_workspace) =
            if let Some(workspace) = find_workspace(&snapshot.workspaces, &name) {
                (
                    create_generated_shell(&client, &workspace.id, &directory, startup_command)?,
                    false,
                )
            } else {
                (
                    client
                        .create_workspace_with_default_cwd(
                            &name,
                            Some(directory.clone()),
                            vec![shell_spec(
                                generated_names::random(),
                                &directory,
                                startup_command,
                            )],
                        )?
                        .shells
                        .into_iter()
                        .next()
                        .ok_or("new workspace has no shell")?,
                    true,
                )
            };
        (shell, name, created_workspace)
    } else {
        let shell = client.create_shell_with_workspace(shell_spec(
            generated_names::random(),
            &directory,
            startup_command,
        ))?;
        let workspace_name = client.get_workspace(&shell.workspace_id)?.name;
        (shell, workspace_name, true)
    };

    let open_result = if open_in_new_window {
        open_terminal(
            &shell.id,
            &format!("{workspace_name} - {}", shell.name),
            true,
            terminal,
        )
    } else {
        Ok(attach::run(&shell.id, None, true, false, None)?)
    };
    if let Err(open_error) = open_result {
        let rollback = if created_workspace {
            client.close_workspace(&shell.workspace_id)
        } else {
            client.close_shell(&shell.id)
        };
        if let Err(rollback_error) = rollback {
            return Err(format!(
                "{open_error}; additionally could not roll back failed launch: {rollback_error}"
            )
            .into());
        }
        return Err(open_error);
    }
    Ok(())
}

fn shell_spec(name: impl Into<String>, cwd: &Path, command: &[String]) -> ShellSpec {
    ShellSpec {
        name: name.into(),
        command: command.to_vec(),
        cwd: cwd.to_owned(),
    }
}

fn ensure_host_terminal() -> Result<(), Box<dyn Error>> {
    if env::var_os("BOOMUX_SHELL_ID").is_some() {
        Err("already inside a Boomux shell; launch Boomux from a fresh terminal".into())
    } else {
        Ok(())
    }
}

fn validate_dashboard_terminal(
    stdin_is_terminal: bool,
    stdout_is_terminal: bool,
) -> io::Result<()> {
    if stdin_is_terminal && stdout_is_terminal {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "dashboard requires an interactive terminal",
        ))
    }
}

fn resolve_directory(path: &Path) -> Result<PathBuf, Box<dyn Error>> {
    let absolute = if path.is_absolute() {
        path.to_owned()
    } else {
        env::current_dir()?.join(path)
    };
    let resolved = absolute
        .canonicalize()
        .map_err(|error| format!("cannot open {}: {error}", path.display()))?;
    if !resolved.is_dir() {
        return Err(format!("{} is not a directory", path.display()).into());
    }
    Ok(resolved)
}

fn cli_name(name: String, kind: &str) -> Result<String, Box<dyn Error>> {
    let name = name.trim();
    if name.is_empty() {
        return Err(cli_output::failure(
            "invalid_argument",
            format!("{kind} name cannot be empty"),
        ));
    }
    Ok(name.to_owned())
}

fn cli_name_or_generated(name: Option<String>, kind: &str) -> Result<String, Box<dyn Error>> {
    name.map(|name| cli_name(name, kind))
        .transpose()
        .map(|name| name.unwrap_or_else(generated_names::random))
}

fn find_workspace<'a>(
    workspaces: &'a [WorkspaceSnapshot],
    name: &str,
) -> Option<&'a WorkspaceSnapshot> {
    workspaces.iter().find(|workspace| workspace.name == name)
}

fn resolve_workspace_target<'a>(
    workspaces: &'a [WorkspaceSnapshot],
    target: &str,
) -> Result<&'a WorkspaceSnapshot, Box<dyn Error>> {
    workspaces
        .iter()
        .find(|workspace| workspace.id == target)
        .or_else(|| workspaces.iter().find(|workspace| workspace.name == target))
        .ok_or_else(|| cli_output::failure("not_found", format!("workspace not found: {target}")))
}

fn resolve_global_workspace_target<'a>(
    workspaces: &'a [protocol::GlobalWorkspaceSnapshot],
    target: &str,
) -> Option<&'a protocol::GlobalWorkspaceSnapshot> {
    workspaces
        .iter()
        .find(|workspace| workspace.id == target)
        .or_else(|| workspaces.iter().find(|workspace| workspace.name == target))
}

fn find_global_workspace_by_id<'a>(
    workspaces: &'a [protocol::GlobalWorkspaceSnapshot],
    workspace_id: &str,
) -> Option<&'a protocol::GlobalWorkspaceSnapshot> {
    workspaces
        .iter()
        .find(|workspace| workspace.id == workspace_id)
}

fn selected_workspace_id() -> Result<Option<String>, Box<dyn Error>> {
    Ok(workspace_selection::load_from_environment()?
        .map(|selection| selection.workspace_id().to_owned()))
}

fn managed_owner_workspace_id(client: &client::Client) -> Result<Option<String>, Box<dyn Error>> {
    if let Some(shell_id) = env::var("BOOMUX_SHELL_ID")
        .ok()
        .filter(|shell_id| !shell_id.is_empty())
    {
        return Ok(Some(client.get_shell(shell_id)?.workspace_id));
    }
    Ok(env::var("BOOMUX_WORKSPACE_ID")
        .ok()
        .filter(|workspace_id| !workspace_id.is_empty()))
}

fn coordinated_workspace_for_local_owner(
    client: &client::Client,
    owner_workspace_id: &str,
) -> Result<Option<protocol::GlobalWorkspaceSnapshot>, Box<dyn Error>> {
    require_protocol_feature(client, protocol::ProtocolFeature::GlobalWorkspaces)?;
    let combined = client.combined_node_snapshot(None)?;
    let local_node_id = combined
        .nodes
        .iter()
        .find(|node| node.local)
        .map(|node| node.node_id.as_str())
        .ok_or_else(|| cli_output::failure("internal", "local Node identity is unavailable"))?;
    Ok(combined
        .workspaces
        .iter()
        .find(|workspace| {
            workspace.placements.iter().any(|placement| {
                placement.node_id == local_node_id && placement.workspace_id == owner_workspace_id
            })
        })
        .cloned())
}

fn selected_global_workspace(
    client: &client::Client,
) -> Result<Option<protocol::GlobalWorkspaceSnapshot>, Box<dyn Error>> {
    let Some(selected_id) = selected_workspace_id()? else {
        return Ok(None);
    };
    require_protocol_feature(client, protocol::ProtocolFeature::GlobalWorkspaces)?;
    let combined = client.combined_node_snapshot(None)?;
    match find_global_workspace_by_id(&combined.workspaces, &selected_id) {
        Some(workspace) => Ok(Some(workspace.clone())),
        None => Err(cli_output::failure(
            "not_found",
            format!(
                "selected Workspace {selected_id} no longer exists; run `boomux workspace clear`"
            ),
        )),
    }
}

fn selected_owner_workspace_id(client: &client::Client) -> Result<Option<String>, Box<dyn Error>> {
    let Some(workspace) = selected_global_workspace(client)? else {
        return Ok(None);
    };
    if workspace.closing {
        return Err(cli_output::failure(
            "invalid_argument",
            "the selected Workspace is closing",
        ));
    }
    let combined = client.combined_node_snapshot(None)?;
    let local_node_id = combined
        .nodes
        .iter()
        .find(|node| node.local)
        .map(|node| node.node_id.as_str())
        .ok_or_else(|| cli_output::failure("internal", "local Node identity is unavailable"))?;
    workspace
        .placements
        .iter()
        .find(|placement| placement.node_id == local_node_id)
        .map(|placement| Some(placement.workspace_id.clone()))
        .ok_or_else(|| {
            cli_output::failure(
                "context_required",
                format!(
                    "selected Workspace {} has no local placement; provide --node for creation or an explicit owner Workspace",
                    workspace.name
                ),
            )
        })
}

fn selected_owner_workspace_id_for_node(
    client: &client::Client,
    node_id: &str,
) -> Result<Option<String>, Box<dyn Error>> {
    let Some(workspace) = selected_global_workspace(client)? else {
        return Ok(None);
    };
    if workspace.closing {
        return Err(cli_output::failure(
            "invalid_argument",
            "the selected Workspace is closing",
        ));
    }
    workspace
        .placements
        .iter()
        .find(|placement| placement.node_id == node_id)
        .map(|placement| Some(placement.workspace_id.clone()))
        .ok_or_else(|| {
            cli_output::failure(
                "context_required",
                format!(
                    "selected Workspace {} has no placement on the requested Node",
                    workspace.name
                ),
            )
        })
}

fn required_owner_workspace_target(
    client: &client::Client,
    explicit: Option<&str>,
    node_id: &str,
) -> Result<String, Box<dyn Error>> {
    if let Some(explicit) = explicit {
        return Ok(explicit.to_owned());
    }
    owner_workspace_context_for_node(client, node_id)?.ok_or_else(|| {
        cli_output::failure(
            "context_required",
            "owner Workspace is required; provide it explicitly, run inside a coordinated Workspace, or select one with a placement on this Node",
        )
    })
}

fn owner_workspace_context_for_node(
    client: &client::Client,
    node_id: &str,
) -> Result<Option<String>, Box<dyn Error>> {
    if let Some(owner_workspace_id) = managed_owner_workspace_id(client)? {
        let workspace = coordinated_workspace_for_local_owner(client, &owner_workspace_id)?
            .ok_or_else(|| {
                cli_output::failure(
                    "context_required",
                    "the current owner Workspace is not linked to a coordinated Workspace",
                )
            })?;
        return workspace
            .placements
            .iter()
            .find(|placement| placement.node_id == node_id)
            .map(|placement| Some(placement.workspace_id.clone()))
            .ok_or_else(|| {
                cli_output::failure(
                    "context_required",
                    format!(
                        "current Workspace {} has no placement on the requested Node",
                        workspace.name
                    ),
                )
            });
    }
    selected_owner_workspace_id_for_node(client, node_id)
}

fn local_workspace_argument(
    client: &client::Client,
    explicit: Option<&str>,
) -> Result<Option<String>, Box<dyn Error>> {
    if let Some(explicit) = explicit {
        return Ok(Some(explicit.to_owned()));
    }
    if env::var_os("BOOMUX_SHELL_ID").is_some() || env::var_os("BOOMUX_WORKSPACE_ID").is_some() {
        return Ok(None);
    }
    selected_owner_workspace_id(client)
}

fn resource_workspace_argument(
    client: &client::Client,
    explicit: Option<&str>,
    exact_resource: bool,
) -> Result<Option<String>, Box<dyn Error>> {
    if exact_resource {
        return Ok(explicit.map(str::to_owned));
    }
    local_workspace_argument(client, explicit)
}

fn resolve_context_workspace<'a>(
    snapshot: &'a Snapshot,
    explicit: Option<&str>,
) -> Result<&'a WorkspaceSnapshot, Box<dyn Error>> {
    if let Some(explicit) = explicit {
        return resolve_workspace_target(&snapshot.workspaces, explicit);
    }
    if let Ok(shell_id) = env::var("BOOMUX_SHELL_ID") {
        let shell = find_shell(snapshot, &shell_id).ok_or_else(|| {
            cli_output::failure("not_found", "current Boomux shell no longer exists")
        })?;
        return resolve_workspace_target(&snapshot.workspaces, &shell.workspace_id);
    }
    if let Some(workspace_id) = env::var("BOOMUX_WORKSPACE_ID")
        .ok()
        .filter(|workspace_id| !workspace_id.is_empty())
    {
        return resolve_workspace_target(&snapshot.workspaces, &workspace_id);
    }
    Err(cli_output::failure(
        "context_required",
        "workspace context is required; provide --workspace or run `boomux workspace select WORKSPACE`",
    ))
}

fn resolve_combined_node<'a>(
    nodes: &'a [protocol::CombinedNode],
    selector: &str,
) -> Result<&'a protocol::CombinedNode, Box<dyn Error>> {
    let matches = nodes
        .iter()
        .filter(|node| node.node_id == selector || node.alias == selector)
        .collect::<Vec<_>>();
    match matches.as_slice() {
        [node] => Ok(*node),
        [] => Err(cli_output::failure(
            "not_found",
            format!("Node not found: {selector}"),
        )),
        _ => Err(cli_output::failure(
            "ambiguous_target",
            format!("Node selector is ambiguous: {selector}"),
        )),
    }
}

fn resolve_workspace_open_node<'a>(
    nodes: &'a [protocol::CombinedNode],
    selector: &str,
) -> Result<&'a protocol::CombinedNode, Box<dyn Error>> {
    let matches = nodes
        .iter()
        .filter(|node| node.node_id == selector || (!node.local && node.alias == selector))
        .collect::<Vec<_>>();
    match matches.as_slice() {
        [node] => Ok(*node),
        [] => Err(cli_output::failure(
            "not_found",
            format!("Node not found: {selector}"),
        )),
        _ => Err(cli_output::failure(
            "ambiguous_target",
            format!("Node selector is ambiguous: {selector}"),
        )),
    }
}

fn resolve_external_workspace<'a>(
    workspaces: &'a [protocol::ExternalWorkspaceSnapshot],
    node_id: &str,
    target: &str,
) -> Result<&'a protocol::ExternalWorkspaceSnapshot, Box<dyn Error>> {
    workspaces
        .iter()
        .find(|workspace| {
            workspace.identity.node_id == node_id
                && (workspace.identity.inner_id == target || workspace.name == target)
        })
        .ok_or_else(|| {
            cli_output::failure(
                "not_found",
                format!("unlinked owner Workspace not found: {target}"),
            )
        })
}

fn select_eligible_workspace_owner(
    nodes: &[protocol::CombinedNode],
    node_selector: Option<&str>,
) -> Result<protocol::CombinedNode, Box<dyn Error>> {
    if let Some(selector) = node_selector {
        let node = resolve_combined_node(nodes, selector)?;
        if !node.workspace_owner_eligible {
            return Err(cli_output::failure(
                "busy",
                format!(
                    "Node {} is unavailable for Workspace placement: {}",
                    node.alias,
                    node.workspace_owner_unavailable_reason
                        .as_deref()
                        .unwrap_or("unavailable")
                ),
            ));
        }
        return Ok(node.clone());
    }
    let eligible = nodes
        .iter()
        .filter(|node| node.workspace_owner_eligible)
        .collect::<Vec<_>>();
    let [node] = eligible.as_slice() else {
        let status = nodes
            .iter()
            .map(|node| {
                format!(
                    "{} ({})",
                    node.alias,
                    if node.workspace_owner_eligible {
                        "eligible"
                    } else {
                        node.workspace_owner_unavailable_reason
                            .as_deref()
                            .unwrap_or("unavailable")
                    }
                )
            })
            .collect::<Vec<_>>()
            .join(", ");
        return Err(cli_output::failure(
            "ambiguous_target",
            format!(
                "Workspace resource placement requires --node when zero or multiple eligible Nodes exist; Nodes: {status}"
            ),
        ));
    };
    Ok((*node).clone())
}

fn shell_suggestion_json(
    workspace_id: &str,
    name: &str,
    node_id: Option<&str>,
) -> serde_json::Value {
    let mut data = serde_json::json!({
        "workspace_id": workspace_id,
        "name": name,
    });
    if let Some(node_id) = node_id {
        data["node_id"] = serde_json::Value::String(node_id.to_owned());
    }
    data
}

struct CoordinatedOwnerSelection {
    workspace: protocol::GlobalWorkspaceSnapshot,
    node: protocol::CombinedNode,
    owner_workspace_id: String,
    default_cwd: Option<PathBuf>,
}

struct WorkspaceCreationContext {
    global_workspaces_supported: bool,
    combined: Option<protocol::CombinedNodeSnapshot>,
}

impl WorkspaceCreationContext {
    fn load(client: &client::Client) -> Result<Self, Box<dyn Error>> {
        let global_workspaces_supported =
            client.supports(protocol::ProtocolFeature::GlobalWorkspaces)?;
        let combined = global_workspaces_supported
            .then(|| client.combined_node_snapshot(None))
            .transpose()?;
        Ok(Self {
            global_workspaces_supported,
            combined,
        })
    }

    fn required_target(
        &self,
        client: &client::Client,
        explicit: Option<&str>,
        node_requested: bool,
    ) -> Result<String, Box<dyn Error>> {
        if let Some(explicit) = explicit {
            return Ok(explicit.to_owned());
        }
        if let Some(owner_workspace_id) = managed_owner_workspace_id(client)? {
            if !node_requested {
                return Ok(owner_workspace_id);
            }
            let workspace = self
                .coordinated_workspace_for_local_owner(&owner_workspace_id)?
                .ok_or_else(|| {
                    cli_output::failure(
                        "context_required",
                        "the current owner Workspace is not linked to a coordinated Workspace; provide one explicitly",
                    )
                })?;
            return Ok(workspace.id.clone());
        }
        let selected_id = selected_workspace_id()?.ok_or_else(|| {
            cli_output::failure(
                "context_required",
                "workspace is required; provide it explicitly or run `boomux workspace select WORKSPACE`",
            )
        })?;
        if !self.global_workspaces_supported {
            return Err(cli_output::failure(
                "unsupported_version",
                format!(
                    "{} requires daemon protocol {} or newer",
                    protocol::ProtocolFeature::GlobalWorkspaces.requirement(),
                    protocol::ProtocolFeature::GlobalWorkspaces.minimum_version()
                ),
            ));
        }
        let workspace = find_global_workspace_by_id(
            &self.combined.as_ref().expect("supported snapshot").workspaces,
            &selected_id,
        )
        .ok_or_else(|| {
            cli_output::failure(
                "not_found",
                format!(
                    "selected Workspace {selected_id} no longer exists; run `boomux workspace clear`"
                ),
            )
        })?;
        if workspace.closing {
            return Err(cli_output::failure(
                "invalid_argument",
                "the selected Workspace is closing",
            ));
        }
        Ok(workspace.id.clone())
    }

    fn coordinated_workspace_for_local_owner(
        &self,
        owner_workspace_id: &str,
    ) -> Result<Option<&protocol::GlobalWorkspaceSnapshot>, Box<dyn Error>> {
        let combined = self.combined.as_ref().ok_or_else(|| {
            cli_output::failure(
                "unsupported_version",
                "cross-Node Workspace placement requires global Workspace support",
            )
        })?;
        let local_node_id = combined
            .nodes
            .iter()
            .find(|node| node.local)
            .map(|node| node.node_id.as_str())
            .ok_or_else(|| cli_output::failure("internal", "local Node identity is unavailable"))?;
        Ok(combined.workspaces.iter().find(|workspace| {
            workspace.placements.iter().any(|placement| {
                placement.node_id == local_node_id && placement.workspace_id == owner_workspace_id
            })
        }))
    }

    fn coordinated_owner(
        &self,
        workspace_target: &str,
        node_selector: Option<&str>,
    ) -> Result<Option<CoordinatedOwnerSelection>, Box<dyn Error>> {
        let Some(combined) = &self.combined else {
            return Ok(None);
        };
        let Some(workspace) =
            resolve_global_workspace_target(&combined.workspaces, workspace_target).cloned()
        else {
            return Ok(None);
        };
        let node = select_eligible_workspace_owner(&combined.nodes, node_selector)?;
        let placement = workspace
            .placements
            .iter()
            .find(|placement| placement.node_id == node.node_id);
        let owner_workspace_id = placement.map_or_else(
            || Uuid::new_v4().to_string(),
            |placement| placement.workspace_id.clone(),
        );
        let default_cwd = placement.and_then(|placement| placement.default_cwd.clone());
        Ok(Some(CoordinatedOwnerSelection {
            workspace,
            node,
            owner_workspace_id,
            default_cwd,
        }))
    }
}

fn require_protocol_feature(
    client: &client::Client,
    feature: protocol::ProtocolFeature,
) -> Result<(), Box<dyn Error>> {
    if client.supports(feature)? {
        return Ok(());
    }
    Err(cli_output::failure(
        "unsupported_version",
        format!(
            "{} requires daemon protocol {} or newer",
            feature.requirement(),
            feature.minimum_version()
        ),
    ))
}

fn require_explicit_coordinated_owner<T>(
    selection: Option<T>,
    node_selector: Option<&str>,
    global_workspaces_supported: bool,
) -> Result<Option<T>, Box<dyn Error>> {
    if selection.is_some() || node_selector.is_none() {
        return Ok(selection);
    }
    if !global_workspaces_supported {
        return Err(cli_output::failure(
            "unsupported_version",
            format!(
                "explicit Workspace placement requires daemon protocol {} or newer",
                protocol::ProtocolFeature::GlobalWorkspaces.minimum_version()
            ),
        ));
    }
    Err(cli_output::failure(
        "invalid_argument",
        "--node can create a Shell or launcher only in a coordinated Workspace; adopt or link an external Workspace first",
    ))
}

fn resolve_owner_directory(
    client: &client::Client,
    node: &protocol::CombinedNode,
    path: PathBuf,
) -> Result<PathBuf, Box<dyn Error>> {
    if node.local {
        return resolve_directory(&path);
    }
    match client.route_node_host_service(
        &node.node_id,
        protocol::HostServiceOperation::ResolveDirectory { path },
    )? {
        protocol::HostServiceResult::Directory { path } => Ok(path),
        _ => Err("owner Node returned an unexpected directory response".into()),
    }
}

fn create_dashboard_workspace(
    client: &client::Client,
    name: &str,
) -> Result<String, Box<dyn Error>> {
    let name = name.trim();
    if name.is_empty() {
        return Err("workspace name cannot be empty".into());
    }
    if client.supports(protocol::ProtocolFeature::GlobalWorkspaces)? {
        let workspace = client.create_global_workspace(name)?;
        return Ok(format!(
            "Created empty workspace {} ({})",
            workspace.name, workspace.id
        ));
    }
    client.create_workspace_with_default_cwd(name, None, Vec::new())?;
    Ok(format!("Created empty workspace {name}"))
}

fn create_dashboard_project_workspace(
    client: &client::Client,
    local_node_id: &str,
    name: &str,
    project_cwd: &Path,
) -> Result<String, Box<dyn Error>> {
    let name = name.trim();
    if name.is_empty() {
        return Err("workspace name cannot be empty".into());
    }
    let cwd = resolve_directory(project_cwd)?;
    if !client.supports(protocol::ProtocolFeature::GlobalWorkspaces)? {
        client.create_workspace_with_default_cwd(name, Some(cwd), Vec::new())?;
        return Ok(format!("Created empty workspace {name}"));
    }

    let combined = client.combined_node_snapshot(Some(local_node_id.to_owned()))?;
    let local = combined
        .nodes
        .iter()
        .find(|node| node.local && node.node_id == local_node_id)
        .filter(|node| node.workspace_owner_eligible)
        .ok_or("local Node is not available for Workspace resources")?;
    let shell_name = generated_names::stable(name);
    let (workspace, shell) = client.create_global_workspace_with_shell(
        Uuid::new_v4().to_string(),
        Uuid::new_v4().to_string(),
        name,
        local.node_id.clone(),
        Uuid::new_v4().to_string(),
        cwd.clone(),
        Uuid::new_v4().to_string(),
        shell_spec(shell_name, &cwd, &[]),
    )?;
    Ok(format!(
        "Created {} in {} on Node {}",
        shell.name, workspace.name, local.alias
    ))
}

fn create_dashboard_shell(
    client: &client::Client,
    workspace_id: &str,
    launch_cwd: &Path,
) -> Result<String, Box<dyn Error>> {
    let workspace = client.get_workspace(workspace_id)?;
    let cwd = resolve_shell_cwd(workspace.default_cwd.as_deref(), None, launch_cwd)?;
    let shell = create_generated_shell(client, workspace_id, &cwd, &[])?;
    Ok(format!("Created {} in {}", shell.name, workspace.name))
}

fn resolve_shell_cwd(
    default_cwd: Option<&Path>,
    requested_cwd: Option<&Path>,
    fallback_cwd: &Path,
) -> Result<PathBuf, Box<dyn Error>> {
    if let Some(cwd) = requested_cwd {
        return resolve_directory(cwd);
    }
    let Some(default_cwd) = default_cwd else {
        return resolve_directory(fallback_cwd);
    };
    resolve_directory(default_cwd).map_err(|error| {
        format!(
            "workspace default working directory is unavailable: {}: {error}",
            default_cwd.display()
        )
        .into()
    })
}

fn create_generated_shell(
    client: &client::Client,
    workspace_id: &str,
    cwd: &Path,
    command: &[String],
) -> Result<ShellSnapshot, Box<dyn Error>> {
    let mut rejected = BTreeSet::new();
    loop {
        let workspace = client.get_workspace(workspace_id)?;
        let name = generated_shell_name(
            workspace
                .shells
                .iter()
                .map(|shell| shell.name.as_str())
                .chain(rejected.iter().map(String::as_str)),
        )?;
        match client.create_shell(workspace_id, shell_spec(&name, cwd, command)) {
            Ok(shell) => return Ok(shell),
            Err(client::ClientError::Remote(error))
                if error.code == Some(protocol::ErrorCode::AlreadyExists) =>
            {
                rejected.insert(name);
            }
            Err(error) => return Err(error.into()),
        }
    }
}

fn generated_shell_name<'a>(
    unavailable: impl IntoIterator<Item = &'a str>,
) -> Result<String, Box<dyn Error>> {
    generated_names::random_excluding(unavailable).ok_or_else(|| {
        cli_output::failure(
            "already_exists",
            "all generated shell names are already in use",
        )
    })
}

fn generated_workspace_name<'a>(
    unavailable: impl IntoIterator<Item = &'a str>,
) -> Result<String, Box<dyn Error>> {
    generated_names::random_excluding(unavailable).ok_or_else(|| {
        cli_output::failure(
            "already_exists",
            "all generated Workspace names are already in use",
        )
    })
}

fn integration_command(command: IntegrationCommands, json: bool) -> Result<(), Box<dyn Error>> {
    let node = match &command {
        IntegrationCommands::List => None,
        IntegrationCommands::Status { node, .. }
        | IntegrationCommands::Install { node, .. }
        | IntegrationCommands::Uninstall { node, .. }
        | IntegrationCommands::Setup { node, .. }
        | IntegrationCommands::Verify { node, .. } => node.as_deref(),
    };
    if let Some(node) = node {
        let client = client::connect_or_start()?;
        let registration = client.node_registration(node)?;
        return remote_integration_command(&client, &registration, command, json);
    }
    match command {
        IntegrationCommands::List => list_integrations(json),
        IntegrationCommands::Status { integration, .. } => integration_status(integration, json),
        IntegrationCommands::Install {
            integration,
            all,
            force,
            dry_run,
            ..
        } => {
            let integrations = if all {
                integration_management::IntegrationId::all().collect()
            } else {
                vec![integration.ok_or_else(|| {
                    cli_output::failure(
                        "invalid_argument",
                        "integration install requires a name or --all",
                    )
                })?]
            };
            install_integrations(&integrations, force, dry_run, json)
        }
        IntegrationCommands::Uninstall {
            integration,
            all,
            force,
            ..
        } => {
            let integrations = if all {
                integration_management::IntegrationId::all().collect()
            } else {
                vec![integration.ok_or_else(|| {
                    cli_output::failure(
                        "invalid_argument",
                        "integration uninstall requires a name or --all",
                    )
                })?]
            };
            uninstall_integrations(&integrations, force, json)
        }
        IntegrationCommands::Setup {
            integration,
            yes,
            force,
            ..
        } => setup_integration(integration, yes, force),
        IntegrationCommands::Verify {
            integration,
            shell,
            wait_ms,
            ..
        } => verify_integration(integration, shell.as_deref(), wait_ms, json),
    }
}

fn remote_integration_command(
    client: &client::Client,
    registration: &protocol::NodeRegistrationSnapshot,
    command: IntegrationCommands,
    json: bool,
) -> Result<(), Box<dyn Error>> {
    let names = |integration: Option<integration_management::IntegrationId>, all: bool| {
        if all {
            integration_management::IntegrationId::all()
                .map(|id| id.spec().key.to_owned())
                .collect::<Vec<_>>()
        } else {
            integration
                .map(|id| vec![id.spec().key.to_owned()])
                .unwrap_or_default()
        }
    };
    match command {
        IntegrationCommands::List => unreachable!(),
        IntegrationCommands::Status { integration, .. } => {
            let result = client.route_node_host_service(
                &registration.node_id,
                protocol::HostServiceOperation::IntegrationStatus {
                    integration: integration.map(|id| id.spec().key.to_owned()),
                },
            )?;
            let protocol::HostServiceResult::IntegrationStatus { integrations } = result else {
                return Err("remote Node returned an unexpected integration status".into());
            };
            if json {
                let integrations = integrations
                    .into_iter()
                    .map(|status| serde_json::json!({
                        "name": status.name,
                        "display_name": status.display_name,
                        "package": status.package,
                        "validated_version": status.validated_version,
                        "host": { "state": status.host_state, "executable": status.executable, "version": status.version, "compatibility": status.compatibility, "error": status.host_error },
                        "asset": { "state": status.asset_state, "path": status.path, "error": status.asset_error },
                        "runtime": { "state": status.runtime_state, "running_processes": status.running_processes, "tracked_processes": status.tracked_processes, "untracked_processes": status.untracked_processes },
                        "recommended_action": status.recommended_action,
                    }))
                    .collect::<Vec<_>>();
                return print_json(
                    CommandKey::IntegrationStatus,
                    serde_json::json!({ "node_id": registration.node_id, "integrations": integrations }),
                );
            }
            println!("Integration status on Node {}", registration.alias);
            for status in integrations {
                println!(
                    "{}\t{}\t{}\t{}",
                    status.display_name,
                    status.host_state,
                    status.asset_state,
                    status.runtime_state
                );
            }
        }
        IntegrationCommands::Install {
            integration,
            all,
            force,
            dry_run,
            ..
        } => {
            let requested = names(integration, all);
            if requested.is_empty() {
                return Err(cli_output::failure(
                    "invalid_argument",
                    "integration install requires a name or --all",
                ));
            }
            let preview = remote_integration_preview(
                client,
                registration,
                protocol::HostServiceIntegrationAction::Install,
                requested,
                force,
            )?;
            if dry_run {
                if json {
                    return print_json(
                        CommandKey::IntegrationInstall,
                        serde_json::json!({ "node_id": registration.node_id, "dry_run": true, "integrations": preview.plans }),
                    );
                }
                for plan in preview.plans {
                    println!("Plan: {} {}", plan.action, plan.path);
                }
            } else {
                remote_integration_commit(
                    client,
                    registration,
                    preview.token,
                    CommandKey::IntegrationInstall,
                    json,
                )?;
            }
        }
        IntegrationCommands::Uninstall {
            integration,
            all,
            force,
            ..
        } => {
            let requested = names(integration, all);
            if requested.is_empty() {
                return Err(cli_output::failure(
                    "invalid_argument",
                    "integration uninstall requires a name or --all",
                ));
            }
            let preview = remote_integration_preview(
                client,
                registration,
                protocol::HostServiceIntegrationAction::Uninstall,
                requested,
                force,
            )?;
            remote_integration_commit(
                client,
                registration,
                preview.token,
                CommandKey::IntegrationUninstall,
                json,
            )?;
        }
        IntegrationCommands::Setup {
            integration,
            yes,
            force,
            ..
        } => {
            let preview = remote_integration_preview(
                client,
                registration,
                protocol::HostServiceIntegrationAction::Install,
                vec![integration.spec().key.to_owned()],
                force,
            )?;
            for plan in &preview.plans {
                println!("Node: {}", registration.alias);
                println!("Plan: {} {}", plan.action, plan.path);
            }
            if !yes && !confirm_setup("Apply this integration plan on the selected Node?")? {
                println!("No changes made.");
                return Ok(());
            }
            remote_integration_commit(
                client,
                registration,
                preview.token,
                CommandKey::IntegrationInstall,
                false,
            )?;
        }
        IntegrationCommands::Verify {
            integration,
            shell,
            wait_ms,
            ..
        } => {
            let shell_id = shell.ok_or_else(|| {
                cli_output::failure(
                    "context_required",
                    "remote integration verify requires --shell with an exact ID",
                )
            })?;
            let shell = match client.route_node_operation(
                &registration.node_id,
                protocol::RoutedOperation::GetShell {
                    shell_id: shell_id.clone(),
                },
            )? {
                protocol::RoutedOperationResult::Shell { shell } => shell,
                _ => return Err("remote Node returned an unexpected shell response".into()),
            };
            let run_id = shell
                .run
                .as_ref()
                .map(|run| run.id.clone())
                .ok_or_else(|| {
                    cli_output::failure("not_found", "remote integration shell has no active run")
                })?;
            let deadline = Instant::now() + Duration::from_millis(u64::from(wait_ms));
            let agents = loop {
                match client.route_node_host_service(
                    &registration.node_id,
                    protocol::HostServiceOperation::VerifyIntegration {
                        integration: integration.spec().key.to_owned(),
                        shell_id: shell_id.clone(),
                        run_id: run_id.clone(),
                    },
                ) {
                    Ok(protocol::HostServiceResult::IntegrationVerified { agents, .. }) => {
                        break agents;
                    }
                    Ok(_) => {
                        return Err(
                            "remote Node returned an unexpected verification response".into()
                        );
                    }
                    Err(_error) if Instant::now() < deadline => {
                        thread::sleep(Duration::from_millis(100))
                    }
                    Err(error) => return Err(error.into()),
                }
            };
            if json {
                return print_json(
                    CommandKey::IntegrationVerify,
                    serde_json::json!({ "node_id": registration.node_id, "integration": integration.spec().key, "verified": true, "shell_id": shell_id, "run_id": run_id, "agents": agents }),
                );
            }
            println!(
                "Verified {} on Node {}",
                integration.spec().display_name,
                registration.alias
            );
        }
    }
    Ok(())
}

fn remote_integration_preview(
    client: &client::Client,
    registration: &protocol::NodeRegistrationSnapshot,
    action: protocol::HostServiceIntegrationAction,
    integrations: Vec<String>,
    force: bool,
) -> Result<protocol::HostIntegrationMutationPreview, Box<dyn Error>> {
    let result = client.route_node_host_service(
        &registration.node_id,
        protocol::HostServiceOperation::PreviewIntegrationMutation {
            action,
            integrations,
            force,
        },
    )?;
    match result {
        protocol::HostServiceResult::IntegrationMutationPreview { preview } => Ok(preview),
        _ => Err("remote Node returned an unexpected integration preview".into()),
    }
}

fn remote_integration_commit(
    client: &client::Client,
    registration: &protocol::NodeRegistrationSnapshot,
    preview_token: String,
    command: CommandKey,
    json: bool,
) -> Result<(), Box<dyn Error>> {
    let result = client.route_node_host_service(
        &registration.node_id,
        protocol::HostServiceOperation::CommitIntegrationMutation { preview_token },
    )?;
    let protocol::HostServiceResult::IntegrationMutation { integrations } = result else {
        return Err("remote Node returned an unexpected integration mutation".into());
    };
    if json {
        print_json(
            command,
            serde_json::json!({ "node_id": registration.node_id, "integrations": integrations }),
        )
    } else {
        for result in integrations {
            println!("{}: {} {}", result.name, result.result, result.path);
        }
        Ok(())
    }
}

fn verify_integration(
    integration: integration_management::IntegrationId,
    shell_id: Option<&str>,
    wait_ms: u32,
    json: bool,
) -> Result<(), Box<dyn Error>> {
    let client = client::connect().map_err(|error| {
        cli_output::failure(
            "daemon_unavailable",
            format!("cannot verify integration without the daemon: {error}"),
        )
    })?;
    let baseline = client.events(None, 1, 0)?;
    let snapshot = baseline
        .snapshot
        .ok_or_else(|| io::Error::other("event baseline omitted its snapshot"))?;
    let targets = integration_management::verification_targets(&snapshot, integration, shell_id);
    let spec = integration.spec();
    let target = match targets.as_slice() {
        [target] => target.clone(),
        [] if shell_id.is_some() => {
            return Err(cli_output::failure(
                "not_found",
                format!(
                    "shell {} is not a running {} host shell",
                    shell_id.expect("checked above"),
                    spec.key
                ),
            ));
        }
        [] => {
            return Err(cli_output::failure(
                "not_found",
                format!("no running {} host shell found", spec.key),
            ));
        }
        _ => {
            return Err(cli_output::failure(
                "ambiguous_target",
                format_ambiguous_verification_targets(&snapshot, integration, &targets),
            ));
        }
    };
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(u64::from(wait_ms));
    let mut cursor = baseline.cursor;
    let mut snapshot = snapshot;
    loop {
        match integration_management::check_verification_target(&snapshot, integration, &target) {
            integration_management::VerificationCheck::Verified {
                workspace_name,
                agents,
            } => {
                if json {
                    let agents = agents
                        .iter()
                        .map(|agent| cli_output::agent(agent, Some(&workspace_name)))
                        .collect::<Vec<_>>();
                    return print_json(
                        CommandKey::IntegrationVerify,
                        serde_json::json!({
                            "integration": spec.key,
                            "verified": true,
                            "shell_id": target.shell_id,
                            "run_id": target.run_id,
                            "agents": agents,
                        }),
                    );
                }
                println!("Verified {} lifecycle reporting", spec.display_name);
                println!("  {:<12}{}", "Shell", target.shell_id);
                println!("  {:<12}{}", "Run", target.run_id);
                println!("  {:<12}{}", "Agents", agents.len());
                return Ok(());
            }
            integration_management::VerificationCheck::Missing => {
                return Err(cli_output::failure(
                    "not_found",
                    format!(
                        "shell {} is no longer a running {} host shell",
                        target.shell_id, spec.key
                    ),
                ));
            }
            integration_management::VerificationCheck::RunChanged => {
                return Err(cli_output::failure(
                    "run_changed",
                    format!("shell {} started a different run", target.shell_id),
                ));
            }
            integration_management::VerificationCheck::Pending => {}
        }
        let now = std::time::Instant::now();
        if wait_ms == 0 || now >= deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!(
                    "did not observe lifecycle integration reporting for {} in shell {} within {} ms",
                    spec.key, target.shell_id, wait_ms
                ),
            )
            .into());
        }
        let remaining = deadline.saturating_duration_since(now).as_millis();
        let chunk_ms = u32::try_from(remaining.min(30_000))
            .unwrap_or(30_000)
            .max(1);
        let batch = client.events(Some(cursor), 1, chunk_ms)?;
        cursor = batch.cursor;
        snapshot = client.snapshot()?;
    }
}

fn format_ambiguous_verification_targets(
    snapshot: &Snapshot,
    integration: integration_management::IntegrationId,
    targets: &[integration_management::VerificationTarget],
) -> String {
    let spec = integration.spec();
    let mut choices = targets
        .iter()
        .filter_map(|target| {
            snapshot.workspaces.iter().find_map(|workspace| {
                workspace
                    .shells
                    .iter()
                    .find(|shell| shell.id == target.shell_id)
                    .map(|shell| {
                        (
                            workspace.name.as_str(),
                            shell.name.as_str(),
                            target.shell_id.as_str(),
                        )
                    })
            })
        })
        .collect::<Vec<_>>();
    choices.sort_unstable();

    let mut output = format!(
        "multiple running {} host shells found\n\nChoose a shell:",
        spec.display_name
    );
    for (workspace, shell, shell_id) in choices {
        write!(
            output,
            "\n  {workspace} / {shell}\n    boomux integration verify {} --shell {shell_id}",
            spec.key
        )
        .expect("writing to a string cannot fail");
    }
    output
}

fn list_integrations(json: bool) -> Result<(), Box<dyn Error>> {
    let integrations = integration_management::IntegrationId::all()
        .map(integration_management::IntegrationSummary::from)
        .collect::<Vec<_>>();
    if json {
        return print_json(
            CommandKey::IntegrationList,
            serde_json::json!({ "integrations": integrations }),
        );
    }
    print!("{}", format_integration_list(&integrations));
    Ok(())
}

fn format_integration_list(integrations: &[integration_management::IntegrationSummary]) -> String {
    let name_width = integrations
        .iter()
        .map(|integration| integration.name.len())
        .max()
        .unwrap_or(0)
        .max("NAME".len());
    let package_width = integrations
        .iter()
        .map(|integration| integration.package.len())
        .max()
        .unwrap_or(0)
        .max("PACKAGE".len());
    let mut output = String::new();
    writeln!(
        output,
        "{:<name_width$}  {:<package_width$}  VALIDATED VERSION",
        "NAME", "PACKAGE"
    )
    .expect("writing to a string cannot fail");
    for integration in integrations {
        writeln!(
            output,
            "{:<name_width$}  {:<package_width$}  {}",
            integration.name, integration.package, integration.validated_version
        )
        .expect("writing to a string cannot fail");
    }
    output
}

fn integration_status(
    integration: Option<integration_management::IntegrationId>,
    json: bool,
) -> Result<(), Box<dyn Error>> {
    let environment = integration_management::Environment::from_process();
    let snapshot = client::connect().and_then(|client| client.snapshot()).ok();
    let integrations = integration.map_or_else(
        || integration_management::IntegrationId::all().collect(),
        |integration| vec![integration],
    );
    let statuses = integrations
        .into_iter()
        .map(|integration| {
            integration_management::inspect(integration, &environment, snapshot.as_ref())
        })
        .collect::<Vec<_>>();
    if json {
        return print_json(
            CommandKey::IntegrationStatus,
            serde_json::json!({ "integrations": statuses }),
        );
    }
    print!("{}", format_integration_statuses(&statuses));
    for status in &statuses {
        if let Some(error) = status.host.error.as_deref() {
            eprintln!("warning: {} host version: {error}", status.name);
        }
        if let Some(error) = status.asset.error.as_deref() {
            eprintln!("warning: {} integration asset: {error}", status.name);
        }
    }
    Ok(())
}

fn format_integration_statuses(statuses: &[integration_management::IntegrationStatus]) -> String {
    let mut output = String::new();
    for (index, status) in statuses.iter().enumerate() {
        if index > 0 {
            output.push('\n');
        }
        writeln!(output, "{} ({})", status.display_name, status.name)
            .expect("writing to a string cannot fail");
        writeln!(output, "  {:<14}{}", "Host", status.host.state.as_str())
            .expect("writing to a string cannot fail");
        writeln!(
            output,
            "  {:<14}{}",
            "Executable",
            status.host.executable.as_deref().unwrap_or("-")
        )
        .expect("writing to a string cannot fail");
        writeln!(
            output,
            "  {:<14}{}",
            "Version",
            status.host.version.as_deref().unwrap_or("-")
        )
        .expect("writing to a string cannot fail");
        writeln!(
            output,
            "  {:<14}{}",
            "Compatibility", status.host.compatibility
        )
        .expect("writing to a string cannot fail");
        writeln!(output, "  {:<14}{}", "Asset", status.asset.state.as_str())
            .expect("writing to a string cannot fail");
        writeln!(
            output,
            "  {:<14}{}",
            "Runtime",
            format_runtime_status(&status.runtime)
        )
        .expect("writing to a string cannot fail");
        writeln!(
            output,
            "  {:<14}{}",
            "Action",
            format_recommended_action(status.name, status.recommended_action)
        )
        .expect("writing to a string cannot fail");
        writeln!(
            output,
            "  {:<14}{}",
            "Path",
            sanitize_table_cell(status.asset.path.as_deref().unwrap_or("-"))
        )
        .expect("writing to a string cannot fail");
    }
    output
}

fn format_runtime_status(status: &integration_management::RuntimeStatus) -> String {
    if status.running_processes == 0 {
        return status.state.as_str().replace('_', " ");
    }
    format!(
        "{} ({} running, {} reporting, {} untracked)",
        status.state.as_str().replace('_', " "),
        status.running_processes,
        status.tracked_processes,
        status.untracked_processes
    )
}

fn format_recommended_action(
    integration: &str,
    action: integration_management::RecommendedAction,
) -> &'static str {
    match action {
        integration_management::RecommendedAction::None => "none",
        integration_management::RecommendedAction::Install => "install integration",
        integration_management::RecommendedAction::Replace => "replace with --force",
        integration_management::RecommendedAction::RestartHost if integration == "kiro" => {
            "reopen managed ShellRun, then launch bare kiro-cli"
        }
        integration_management::RecommendedAction::RestartHost => "restart host",
        integration_management::RecommendedAction::InspectError => "inspect reported error",
    }
}

fn install_integrations(
    integrations: &[integration_management::IntegrationId],
    force: bool,
    dry_run: bool,
    json: bool,
) -> Result<(), Box<dyn Error>> {
    let environment = integration_management::Environment::from_process();
    let plans = integrations
        .iter()
        .copied()
        .map(|integration| integration_management::plan_install(integration, &environment, force))
        .collect::<Result<Vec<_>, _>>()?;
    if dry_run {
        if json {
            return print_json(
                CommandKey::IntegrationInstall,
                serde_json::json!({ "dry_run": true, "integrations": plans }),
            );
        }
        for plan in &plans {
            let spec = plan.integration.spec();
            let installation = plan.integration.installation();
            match plan.action {
                integration_management::InstallAction::Install => println!(
                    "Would install Boomux {} {} at {}",
                    spec.display_name, installation.asset_name, plan.path
                ),
                integration_management::InstallAction::Replace => println!(
                    "Would replace Boomux {} {} at {}",
                    spec.display_name, installation.asset_name, plan.path
                ),
                integration_management::InstallAction::Unchanged => println!(
                    "Boomux {} {} is already installed at {}",
                    spec.display_name, installation.asset_name, plan.path
                ),
            }
        }
        return Ok(());
    }
    let results = integrations
        .iter()
        .copied()
        .map(|integration| integration_management::install(integration, &environment, force))
        .collect::<Result<Vec<_>, _>>()?;
    if json {
        return print_json(
            CommandKey::IntegrationInstall,
            serde_json::json!({ "integrations": results }),
        );
    }
    print_integration_install_results(&results);
    Ok(())
}

fn print_integration_install_results(results: &[integration_management::InstallResult]) {
    for result in results {
        let spec = result.integration.spec();
        let installation = result.integration.installation();
        match result.result {
            integration_management::InstallOutcome::Unchanged => println!(
                "Boomux {} {} is already installed at {}",
                spec.display_name, installation.asset_name, result.path
            ),
            integration_management::InstallOutcome::Installed => println!(
                "Installed Boomux {} {} at {}",
                spec.display_name, installation.asset_name, result.path
            ),
            integration_management::InstallOutcome::Replaced => println!(
                "Replaced Boomux {} {} at {}",
                spec.display_name, installation.asset_name, result.path
            ),
        }
        if result.restart_required {
            println!("{}", installation.reload_message);
        }
    }
}

fn uninstall_integrations(
    integrations: &[integration_management::IntegrationId],
    force: bool,
    json: bool,
) -> Result<(), Box<dyn Error>> {
    let environment = integration_management::Environment::from_process();
    for integration in integrations {
        integration_management::preflight_uninstall(*integration, &environment, force)?;
    }
    let results = integrations
        .iter()
        .copied()
        .map(|integration| integration_management::uninstall(integration, &environment, force))
        .collect::<Result<Vec<_>, _>>()?;
    if json {
        return print_json(
            CommandKey::IntegrationUninstall,
            serde_json::json!({ "integrations": results }),
        );
    }
    for result in &results {
        let spec = result.integration.spec();
        let installation = result.integration.installation();
        match result.result {
            integration_management::UninstallOutcome::Removed => println!(
                "Removed Boomux {} {} from {}",
                spec.display_name, installation.asset_name, result.path
            ),
            integration_management::UninstallOutcome::NotInstalled => println!(
                "Boomux {} {} is not installed at {}",
                spec.display_name, installation.asset_name, result.path
            ),
        }
        if result.restart_required {
            println!("{}", installation.reload_message);
        }
    }
    Ok(())
}

fn setup_integration(
    integration: integration_management::IntegrationId,
    yes: bool,
    force: bool,
) -> Result<(), Box<dyn Error>> {
    let environment = integration_management::Environment::from_process();
    let snapshot = client::connect().and_then(|client| client.snapshot()).ok();
    let status = integration_management::inspect(integration, &environment, snapshot.as_ref());
    print!(
        "{}",
        format_integration_statuses(std::slice::from_ref(&status))
    );

    let spec = integration.spec();
    let installation = integration.installation();
    if status.asset.state == integration_management::AssetState::Current {
        print_setup_next_step(integration, status.runtime.state);
        return Ok(());
    }
    if status.asset.state == integration_management::AssetState::Unavailable {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            status
                .asset
                .error
                .unwrap_or_else(|| "integration asset could not be inspected".into()),
        )
        .into());
    }

    let replacing = status.asset.state == integration_management::AssetState::Modified;
    let plan = integration_management::plan_install(integration, &environment, replacing || force)?;
    let action = match plan.action {
        integration_management::InstallAction::Install => "install",
        integration_management::InstallAction::Replace => "replace",
        integration_management::InstallAction::Unchanged => "leave unchanged",
    };
    println!(
        "Plan: {action} Boomux {} at {}",
        installation.asset_name, plan.path
    );

    if yes && replacing && !force {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "--yes requires --force to replace a modified integration asset",
        )
        .into());
    }
    if !yes {
        let prompt = if replacing {
            "Replace the modified asset?"
        } else {
            "Install this integration?"
        };
        if !confirm_setup(prompt)? {
            println!("No changes made.");
            return Ok(());
        }
    }

    let result = integration_management::install(integration, &environment, replacing || force)?;
    print_integration_install_results(&[result]);
    println!(
        "After restarting {}, run: boomux integration verify {}",
        spec.display_name, spec.key
    );
    Ok(())
}

fn confirm_setup(prompt: &str) -> io::Result<bool> {
    print!("{prompt} [y/N] ");
    std::io::Write::flush(&mut io::stdout())?;
    let mut response = String::new();
    io::stdin().read_line(&mut response)?;
    Ok(is_setup_confirmation(&response))
}

fn is_setup_confirmation(response: &str) -> bool {
    matches!(response.trim().to_ascii_lowercase().as_str(), "y" | "yes")
}

fn print_setup_next_step(
    integration: integration_management::IntegrationId,
    runtime: integration_management::RuntimeState,
) {
    let spec = integration.spec();
    if runtime == integration_management::RuntimeState::Reporting {
        println!(
            "Boomux lifecycle reporting is ready for {}.",
            spec.display_name
        );
    } else {
        println!(
            "The asset is current. Restart {}, open it in a Boomux-managed shell, then run: boomux integration verify {}",
            spec.display_name, spec.key
        );
    }
}

fn capabilities(json: bool) -> Result<(), Box<dyn Error>> {
    let json_commands = json_commands().collect::<Vec<_>>();
    let features = NON_PROTOCOL_FEATURES
        .iter()
        .copied()
        .chain(protocol::protocol_capabilities())
        .collect::<Vec<_>>();
    let error_codes: &[&str] = &[
        "invalid_argument",
        "not_found",
        "already_exists",
        "busy",
        "daemon_stopping",
        "daemon_unavailable",
        "shell_start_failed",
        "persistence_failed",
        "timeout",
        "unsupported_version",
        "install_required",
        "upgrade_required",
        "bootstrap_authentication_failed",
        "bootstrap_transport_failed",
        "bootstrap_malformed_helper",
        "bootstrap_unsupported_platform",
        "bootstrap_install_failed",
        "bootstrap_commit_outcome_unknown",
        "bootstrap_runtime_unavailable",
        "node_identity_conflict",
        "cursor_expired",
        "run_changed",
        "revision_ahead",
        "idempotency_expired",
        "node_identity_unavailable",
        "node_registration_unavailable",
        "node_identity_changed",
        "revision_changed",
        "context_required",
        "ambiguous_target",
        "unsupported_integration",
        "internal",
        "unknown",
    ];
    let integration_hosts = boomux::integrations::ALL
        .iter()
        .filter_map(|descriptor| {
            let installation = descriptor.installation?;
            Some((
                descriptor.key.to_owned(),
                serde_json::json!({
                    "package": installation.package,
                    "validated_version": installation.validated_version,
                }),
            ))
        })
        .collect::<serde_json::Map<_, _>>();
    if json {
        return print_json(
            CommandKey::Capabilities,
            serde_json::json!({
                "cli_version": env!("CARGO_PKG_VERSION"),
                "daemon_protocol_version": protocol::PROTOCOL_VERSION,
                "json_schemas": [cli_output::SCHEMA],
                "json_commands": json_commands,
                "features": features,
                "integration_hosts": integration_hosts,
                "error_codes": error_codes,
            }),
        );
    }
    println!("CLI VERSION\t{}", env!("CARGO_PKG_VERSION"));
    println!("DAEMON PROTOCOL\t{}", protocol::PROTOCOL_VERSION);
    println!("JSON SCHEMAS\t{}", cli_output::SCHEMA);
    println!("JSON COMMANDS\t{}", json_commands.join(","));
    println!("FEATURES\t{}", features.join(","));
    println!(
        "INTEGRATION HOSTS\t{}",
        integration_management::IntegrationId::all()
            .map(|integration| {
                let spec = integration.spec();
                let installation = integration.installation();
                format!("{}={}", spec.key, installation.validated_version)
            })
            .collect::<Vec<_>>()
            .join(",")
    );
    println!("ERROR CODES\t{}", error_codes.join(","));
    Ok(())
}

fn list_projects(json: bool, node: Option<&str>) -> Result<(), Box<dyn Error>> {
    if let Some(node) = node {
        let client = client::connect_or_start()?;
        let registration = client.node_registration(node)?;
        let protocol::HostServiceResult::Projects { discovery } = client.route_node_host_service(
            &registration.node_id,
            protocol::HostServiceOperation::DiscoverProjects,
        )?
        else {
            return Err("remote Node returned an unexpected project response".into());
        };
        if json {
            return print_json(
                CommandKey::ProjectList,
                serde_json::json!({
                    "node_id": registration.node_id,
                    "roots_configured": discovery.roots_configured,
                    "projects": discovery.projects,
                    "warnings": discovery.warnings,
                }),
            );
        }
        if !discovery.roots_configured {
            println!(
                "No project roots configured on Node {}.",
                registration.alias
            );
        } else if discovery.projects.is_empty() {
            println!("No projects discovered on Node {}.", registration.alias);
        } else {
            println!("GROUP\tNAME\tPATH");
            for project in discovery.projects {
                println!(
                    "{}\t{}\t{}",
                    project.group,
                    project.name,
                    project.path.display()
                );
            }
        }
        for warning in discovery.warnings {
            eprintln!("warning: {warning}");
        }
        return Ok(());
    }
    let config = config::load()?;
    let roots_configured = !config.projects.roots.is_empty();
    let discovery = projects::discover(&config.projects);
    if json {
        let projects = discovery
            .projects
            .iter()
            .map(cli_output::project)
            .collect::<Vec<_>>();
        return print_json(
            CommandKey::ProjectList,
            serde_json::json!({
                "roots_configured": roots_configured,
                "projects": projects,
                "warnings": discovery.warnings,
            }),
        );
    }

    if !roots_configured {
        println!("No project roots configured.");
    } else if discovery.projects.is_empty() {
        println!("No projects discovered.");
    } else {
        println!("GROUP\tNAME\tPATH");
        for project in discovery.projects {
            println!(
                "{}\t{}\t{}",
                project.group,
                project.name,
                project.path.display()
            );
        }
    }
    for warning in discovery.warnings {
        eprintln!("warning: {warning}");
    }
    Ok(())
}

fn list_shells(json: bool) -> Result<(), Box<dyn Error>> {
    let snapshot = client::connect_or_start()?.snapshot()?;
    if json {
        let shells = snapshot
            .workspaces
            .iter()
            .flat_map(|workspace| {
                workspace
                    .shells
                    .iter()
                    .map(|shell| cli_output::shell(shell, Some(&workspace.name)))
            })
            .collect::<Vec<_>>();
        return print_json(CommandKey::List, serde_json::json!({ "shells": shells }));
    }
    println!("WORKSPACE\tNAME\tSHELL ID\tRUN ID\tSTATUS");
    for workspace in snapshot.workspaces {
        for shell in workspace.shells {
            println!(
                "{}\t{}\t{}\t{}\t{}",
                workspace.name,
                shell.name,
                shell.id,
                shell.run.as_ref().map_or("-", |run| run.id.as_str()),
                shell_status(&shell.status)
            );
        }
    }
    Ok(())
}

struct CoordinatedWorkspaceOpenOutcome {
    opened_launchers: usize,
    opened_shells: usize,
    failures: Vec<String>,
}

fn tui_workspace_open_message(
    workspace_name: &str,
    outcome: &CoordinatedWorkspaceOpenOutcome,
) -> Result<String, String> {
    if outcome.failures.is_empty() {
        Ok(format!(
            "Opened {} launcher(s) and {} shell(s) for {workspace_name}",
            outcome.opened_launchers, outcome.opened_shells
        ))
    } else if outcome.opened_launchers + outcome.opened_shells > 0 {
        Ok(format!(
            "Opened {} launcher(s) and {} shell(s) for {workspace_name}; warning: {} operation(s) unavailable",
            outcome.opened_launchers,
            outcome.opened_shells,
            outcome.failures.len()
        ))
    } else {
        Err(format!(
            "Workspace {workspace_name} could not be restored: {}",
            outcome.failures.join("; ")
        ))
    }
}

fn attempt_open_coordinated_workspace(
    client: &client::Client,
    combined: &protocol::CombinedNodeSnapshot,
    workspace: &protocol::GlobalWorkspaceSnapshot,
    terminal_override: Option<&str>,
    invoke_launchers: bool,
) -> Result<CoordinatedWorkspaceOpenOutcome, Box<dyn Error>> {
    let operation = client.open_global_workspace(&workspace.id, workspace.revision)?;
    let terminal = effective_terminal(terminal_override)?;
    let desktop_placement = hyprland_special_workspaces_enabled()?;
    let mut opened_launchers = 0;
    let mut opened_shells = 0;
    let mut failures = Vec::new();
    for placement in operation.placements {
        let node = combined
            .nodes
            .iter()
            .find(|node| node.node_id == placement.node_id);
        let alias = node.map_or(placement.node_id.as_str(), |node| node.alias.as_str());
        if placement.status != "available" {
            failures.push(format!(
                "Node {alias}: {}",
                placement.message.unwrap_or(placement.status)
            ));
            continue;
        }
        let owner = if node.is_some_and(|node| node.local) {
            client.get_workspace(&placement.workspace_id)
        } else {
            client
                .route_node_operation(
                    &placement.node_id,
                    protocol::RoutedOperation::GetWorkspace {
                        workspace_id: placement.workspace_id.clone(),
                    },
                )
                .and_then(|result| match result {
                    protocol::RoutedOperationResult::Workspace { workspace } => Ok(workspace),
                    _ => Err(client::ClientError::Protocol(
                        client::ProtocolError::UnexpectedResponse(Box::new(
                            protocol::Response::RoutedNodeOperation { result },
                        )),
                    )),
                })
        };
        let owner = match owner {
            Ok(owner) => owner,
            Err(error) => {
                failures.push(format!("Node {alias}: {error}"));
                continue;
            }
        };
        if node.is_some_and(|node| node.local) {
            match attempt_open_workspace(
                &owner,
                terminal.as_deref(),
                desktop_placement.then_some((workspace.id.as_str(), placement.node_id.as_str())),
                invoke_launchers,
            ) {
                Ok(outcome) => {
                    opened_launchers += outcome.opened_launchers;
                    opened_shells += outcome.opened_shells;
                    failures.extend(
                        outcome
                            .failures
                            .into_iter()
                            .map(|failure| format!("Node {alias}: {failure}")),
                    );
                }
                Err(error) => failures.push(format!("Node {alias}: {error}")),
            }
            continue;
        }
        if invoke_launchers {
            for launcher in &owner.launchers {
                match client.route_node_host_service(
                    &placement.node_id,
                    protocol::HostServiceOperation::InvokeLauncher {
                        workspace_id: owner.id.clone(),
                        launcher_id: launcher.id.clone(),
                    },
                ) {
                    Ok(_) => opened_launchers += 1,
                    Err(error) => {
                        failures.push(format!("Node {alias} launcher {}: {error}", launcher.name))
                    }
                }
            }
        }
        for shell in &owner.shells {
            let opened = if desktop_placement {
                terminal::open_remote_placed(
                    terminal.as_deref(),
                    &placement.node_id,
                    &shell.id,
                    &format!("[{alias}] {} - {}", workspace.name, shell.name),
                    true,
                    &workspace.id,
                )
            } else {
                terminal::open_remote(
                    terminal.as_deref(),
                    &placement.node_id,
                    &shell.id,
                    &format!("[{alias}] {} - {}", workspace.name, shell.name),
                    true,
                )
            };
            match opened {
                Ok(()) => opened_shells += 1,
                Err(error) => failures.push(format!("Node {alias} shell {}: {error}", shell.name)),
            }
        }
    }
    Ok(CoordinatedWorkspaceOpenOutcome {
        opened_launchers,
        opened_shells,
        failures,
    })
}

fn open_coordinated_workspace(
    client: &client::Client,
    combined: &protocol::CombinedNodeSnapshot,
    workspace: &protocol::GlobalWorkspaceSnapshot,
    terminal_override: Option<&str>,
    invoke_launchers: bool,
) -> Result<(usize, usize), Box<dyn Error>> {
    let outcome = attempt_open_coordinated_workspace(
        client,
        combined,
        workspace,
        terminal_override,
        invoke_launchers,
    )?;
    if outcome.failures.is_empty() {
        Ok((outcome.opened_launchers, outcome.opened_shells))
    } else {
        Err(io::Error::other(format!(
            "Workspace {} opened with per-Node failures: {}",
            workspace.name,
            outcome.failures.join("; ")
        ))
        .into())
    }
}

fn desktop_command(
    command: DesktopCommands,
    terminal_override: Option<&str>,
) -> Result<(), Box<dyn Error>> {
    let configured = hyprland_special_workspaces_configured()?;
    let enabled = configured && hyprland::session_available();
    let move_forward = matches!(&command, DesktopCommands::Next);
    match &command {
        DesktopCommands::Next if !enabled => {
            return hyprland::move_regular_workspace(1).map_err(Into::into);
        }
        DesktopCommands::Previous if !enabled => {
            return hyprland::move_regular_workspace(-1).map_err(Into::into);
        }
        DesktopCommands::Terminal if !enabled => {
            let terminal = effective_terminal(terminal_override)?;
            return terminal::open_plain(terminal.as_deref());
        }
        DesktopCommands::Close if !enabled => {
            return hyprland::close_active_window().map_err(Into::into);
        }
        DesktopCommands::Pop if !hyprland::session_available() => {
            return Err("no active Hyprland session was detected".into());
        }
        DesktopCommands::Toggle | DesktopCommands::Show { .. } | DesktopCommands::Gather
            if !enabled =>
        {
            return Err(if configured {
                cli_output::failure("unavailable", "no active Hyprland session was detected")
            } else {
                cli_output::failure(
                    "invalid_argument",
                    "Hyprland special Workspaces are disabled; set desktop.workspace_layer = \"hyprland-special\"",
                )
            });
        }
        DesktopCommands::Return if !enabled => {
            return Err(if configured {
                cli_output::failure("unavailable", "no active Hyprland session was detected")
            } else {
                cli_output::failure(
                    "invalid_argument",
                    "Hyprland special Workspaces are disabled; set desktop.workspace_layer = \"hyprland-special\"",
                )
            });
        }
        _ => {}
    }

    let active = hyprland::active_boomux_workspace()?;
    match command {
        DesktopCommands::Toggle => {
            if let Some(workspace_id) = active {
                hyprland::toggle_special(&workspace_id)?;
                return Ok(());
            }
            let client = client::connect_or_start()?;
            require_protocol_feature(&client, protocol::ProtocolFeature::GlobalWorkspaces)?;
            let combined = client.combined_node_snapshot(None)?;
            let workspaces = desktop_workspaces(&combined.workspaces);
            let workspace = selected_desktop_workspace(&workspaces)?;
            ensure_desktop_workspace(&client, &combined, workspace, terminal_override)?;
            hyprland::toggle_special(&workspace.id)?;
            select_desktop_workspace(workspace)?;
            println!("Showing Boomux Workspace {}", workspace.name);
        }
        DesktopCommands::Next | DesktopCommands::Previous => {
            let Some(active_id) = active else {
                return hyprland::move_regular_workspace(if move_forward { 1 } else { -1 })
                    .map_err(Into::into);
            };
            let client = client::connect_or_start()?;
            require_protocol_feature(&client, protocol::ProtocolFeature::GlobalWorkspaces)?;
            let combined = client.combined_node_snapshot(None)?;
            let workspaces = desktop_workspaces(&combined.workspaces);
            let current = workspaces
                .iter()
                .position(|workspace| workspace.id == active_id)
                .ok_or_else(|| {
                    cli_output::failure(
                        "not_found",
                        format!("visible Boomux Workspace {active_id} no longer exists"),
                    )
                })?;
            let Some(target) = cycle_desktop_workspace(&workspaces, current, move_forward) else {
                return Ok(());
            };
            ensure_desktop_workspace(&client, &combined, target, terminal_override)?;
            hyprland::toggle_special(&target.id)?;
            select_desktop_workspace(target)?;
            println!("Showing Boomux Workspace {}", target.name);
        }
        DesktopCommands::Show { target } => {
            let client = client::connect_or_start()?;
            require_protocol_feature(&client, protocol::ProtocolFeature::GlobalWorkspaces)?;
            let combined = client.combined_node_snapshot(None)?;
            let workspace = resolve_global_workspace_target(&combined.workspaces, &target)
                .filter(|workspace| !workspace.closing)
                .ok_or_else(|| {
                    cli_output::failure("not_found", format!("Workspace not found: {target}"))
                })?;
            ensure_desktop_workspace(&client, &combined, workspace, terminal_override)?;
            if active.as_deref() != Some(workspace.id.as_str()) {
                hyprland::toggle_special(&workspace.id)?;
            }
            select_desktop_workspace(workspace)?;
            println!("Showing Boomux Workspace {}", workspace.name);
        }
        DesktopCommands::Terminal => {
            let Some(workspace_id) = active else {
                let terminal = effective_terminal(terminal_override)?;
                return terminal::open_plain(terminal.as_deref());
            };
            let client = client::connect_or_start()?;
            require_protocol_feature(&client, protocol::ProtocolFeature::GlobalWorkspaces)?;
            let combined = client.combined_node_snapshot(None)?;
            let workspace = find_global_workspace_by_id(&combined.workspaces, &workspace_id)
                .filter(|workspace| !workspace.closing)
                .ok_or_else(|| {
                    cli_output::failure(
                        "not_found",
                        format!("visible Boomux Workspace {workspace_id} is unavailable"),
                    )
                })?;
            let local_node = combined
                .nodes
                .iter()
                .find(|node| node.local)
                .ok_or_else(|| cli_output::failure("internal", "local Node is unavailable"))?;
            select_desktop_workspace(workspace)?;
            shell_command(
                ShellCommands::Create {
                    workspace: Some(workspace.id.clone()),
                    node: Some(local_node.node_id.clone()),
                    name: None,
                    cwd: None,
                    open: true,
                    command: Vec::new(),
                },
                false,
                terminal_override,
            )?;
        }
        DesktopCommands::Close => {
            let Some(workspace_id) = active else {
                return hyprland::close_active_window().map_err(Into::into);
            };
            let active_shell = hyprland::active_boomux_shell(&workspace_id)?;
            let identity =
                protocol::QualifiedIdentity::new(&active_shell.node_id, &active_shell.shell_id);
            let client = client::connect_or_start()?;
            if !client.supports(protocol::ProtocolFeature::QualifiedFocusedTerminal)? {
                return Err("contextual Shell close requires daemon protocol 39 or newer".into());
            }
            let combined = client.combined_node_snapshot(None)?;
            if combined
                .focused_terminal
                .as_ref()
                .map(|focused| &focused.shell)
                != Some(&identity)
            {
                return Err("active Hyprland window does not match Boomux's focused terminal; focus it and retry".into());
            }
            let message = close_qualified_shell_with_client(
                &client,
                &combined,
                &identity,
                Some((&workspace_id, &active_shell)),
            )
            .map_err(io::Error::other)?;
            println!("{message}");
        }
        DesktopCommands::Pop => {
            hyprland::pop_active_window(active.is_none())?;
        }
        DesktopCommands::Return => {
            let client = client::connect_or_start()?;
            require_protocol_feature(&client, protocol::ProtocolFeature::GlobalWorkspaces)?;
            let combined = client.combined_node_snapshot(None)?;
            let local_node_id = combined
                .nodes
                .iter()
                .find(|node| node.local)
                .map(|node| node.node_id.as_str())
                .ok_or_else(|| cli_output::failure("internal", "local Node is unavailable"))?;
            let active_shell = hyprland::active_shell_window()?;
            let identity =
                protocol::QualifiedIdentity::new(&active_shell.node_id, &active_shell.shell_id);
            let shell = routed_dashboard_shell(&client, &identity, local_node_id)
                .map_err(io::Error::other)?;
            let mut matching = combined.workspaces.iter().filter(|workspace| {
                !workspace.closing
                    && workspace.placements.iter().any(|placement| {
                        placement.node_id == identity.node_id
                            && placement.workspace_id == shell.workspace_id
                            && placement.state == protocol::WorkspacePlacementState::Active
                    })
            });
            let workspace = matching.next().ok_or_else(|| {
                cli_output::failure(
                    "not_found",
                    "the active terminal does not belong to an active coordinated Workspace",
                )
            })?;
            if matching.next().is_some() {
                return Err(cli_output::failure(
                    "internal",
                    "the active terminal belongs to more than one coordinated Workspace",
                ));
            }
            hyprland::return_shell_window(&active_shell, &workspace.id)?;
            select_desktop_workspace(workspace)?;
            println!("Returned terminal to Boomux Workspace {}", workspace.name);
        }
        DesktopCommands::Gather => {
            let client = client::connect_or_start()?;
            require_protocol_feature(&client, protocol::ProtocolFeature::GlobalWorkspaces)?;
            let combined = client.combined_node_snapshot(None)?;
            let workspaces = desktop_workspaces(&combined.workspaces);
            let workspace = if let Some(active_id) = active.as_deref() {
                workspaces
                    .iter()
                    .copied()
                    .find(|workspace| workspace.id == active_id)
                    .ok_or_else(|| {
                        cli_output::failure(
                            "not_found",
                            format!("visible Boomux Workspace {active_id} no longer exists"),
                        )
                    })?
            } else {
                selected_desktop_workspace(&workspaces)?
            };
            gather_desktop_workspace(&client, &combined, workspace, terminal_override)?;
            if active.is_none() {
                hyprland::toggle_special(&workspace.id)?;
            }
            select_desktop_workspace(workspace)?;
            println!("Gathered terminals in Boomux Workspace {}", workspace.name);
        }
    }
    Ok(())
}

fn desktop_workspaces(
    workspaces: &[protocol::GlobalWorkspaceSnapshot],
) -> Vec<&protocol::GlobalWorkspaceSnapshot> {
    let mut workspaces = workspaces
        .iter()
        .filter(|workspace| !workspace.closing)
        .collect::<Vec<_>>();
    workspaces.sort_by(|left, right| left.name.cmp(&right.name).then(left.id.cmp(&right.id)));
    workspaces
}

fn selected_desktop_workspace<'a>(
    workspaces: &'a [&protocol::GlobalWorkspaceSnapshot],
) -> Result<&'a protocol::GlobalWorkspaceSnapshot, Box<dyn Error>> {
    if workspaces.is_empty() {
        return Err(cli_output::failure(
            "not_found",
            "no active coordinated Workspaces are available",
        ));
    }
    let Some(selection) = workspace_selection::load_from_environment()? else {
        return Ok(workspaces[0]);
    };
    Ok(workspaces
        .iter()
        .copied()
        .find(|workspace| workspace.id == selection.workspace_id())
        .unwrap_or(workspaces[0]))
}

fn cycle_desktop_workspace<'a>(
    workspaces: &'a [&protocol::GlobalWorkspaceSnapshot],
    current: usize,
    forward: bool,
) -> Option<&'a protocol::GlobalWorkspaceSnapshot> {
    if workspaces.len() < 2 {
        return None;
    }
    let next = if forward {
        (current + 1) % workspaces.len()
    } else if current == 0 {
        workspaces.len() - 1
    } else {
        current - 1
    };
    Some(workspaces[next])
}

fn select_desktop_workspace(
    workspace: &protocol::GlobalWorkspaceSnapshot,
) -> Result<(), Box<dyn Error>> {
    let selection = workspace_selection::WorkspaceSelection::new(&workspace.id)?;
    if workspace_selection::load_from_environment()?.as_ref() == Some(&selection) {
        return Ok(());
    }
    workspace_selection::save_from_environment(&selection)?;
    Ok(())
}

fn ensure_desktop_workspace(
    client: &client::Client,
    combined: &protocol::CombinedNodeSnapshot,
    workspace: &protocol::GlobalWorkspaceSnapshot,
    terminal_override: Option<&str>,
) -> Result<(), Box<dyn Error>> {
    hyprland::apply_workspace_layout(&workspace.id)?;
    if hyprland::workspace_has_windows(&workspace.id)? {
        return Ok(());
    }
    gather_desktop_workspace(client, combined, workspace, terminal_override)
}

fn gather_desktop_workspace(
    client: &client::Client,
    combined: &protocol::CombinedNodeSnapshot,
    workspace: &protocol::GlobalWorkspaceSnapshot,
    terminal_override: Option<&str>,
) -> Result<(), Box<dyn Error>> {
    if let Err(error) =
        open_coordinated_workspace(client, combined, workspace, terminal_override, false)
    {
        if hyprland::wait_for_workspace_window(&workspace.id).is_err() {
            return Err(error);
        }
        eprintln!("warning: Workspace restored with partial failures: {error}");
    }
    if hyprland::wait_for_workspace_window(&workspace.id).is_err() {
        return Err(cli_output::failure(
            "not_found",
            format!(
                "Workspace {} has no terminal windows to show in the Boomux desktop layer",
                workspace.name
            ),
        ));
    }
    Ok(())
}

fn show_desktop_workspace(
    workspace: &protocol::GlobalWorkspaceSnapshot,
) -> Result<(), Box<dyn Error>> {
    if !hyprland_special_workspaces_enabled()? {
        return Ok(());
    }
    if hyprland::active_boomux_workspace()?.as_deref() != Some(workspace.id.as_str()) {
        hyprland::toggle_special(&workspace.id)?;
    }
    select_desktop_workspace(workspace)
}

fn workspace_command(
    command: WorkspaceCommands,
    json: bool,
    terminal_override: Option<&str>,
) -> Result<(), Box<dyn Error>> {
    if matches!(&command, WorkspaceCommands::Clear) {
        if workspace_selection::clear_from_environment()? {
            println!("Cleared selected workspace");
        } else {
            println!("No workspace was selected");
        }
        return Ok(());
    }
    let current_selection = if matches!(&command, WorkspaceCommands::Current) {
        match workspace_selection::load_from_environment()? {
            Some(selection) => Some(selection),
            None => {
                println!("No workspace selected");
                return Ok(());
            }
        }
    } else {
        None
    };
    let client = client::connect_or_start()?;
    match command {
        WorkspaceCommands::List => {
            if client.supports(protocol::ProtocolFeature::GlobalWorkspaces)? {
                let combined = client.combined_node_snapshot(None)?;
                if json {
                    return print_json(
                        CommandKey::WorkspaceList,
                        serde_json::json!({
                            "workspaces": combined.workspaces,
                            "external_workspaces": combined.external_workspaces,
                        }),
                    );
                }
                println!("NAME\tWORKSPACE ID\tREVISION\tSTATE\tPLACEMENTS");
                for workspace in combined.workspaces {
                    println!(
                        "{}\t{}\t{}\t{}\t{}",
                        workspace.name,
                        workspace.id,
                        workspace.revision,
                        if workspace.closing {
                            "closing"
                        } else {
                            "active"
                        },
                        workspace.placements.len(),
                    );
                }
                if !combined.external_workspaces.is_empty() {
                    println!("\nUNLINKED OWNER WORKSPACES");
                    println!("NAME\tNODE ID\tOWNER WORKSPACE ID\tREVISION\tAVAILABLE");
                    for workspace in combined.external_workspaces {
                        println!(
                            "{}\t{}\t{}\t{}\t{}",
                            workspace.name,
                            workspace.identity.node_id,
                            workspace.identity.inner_id,
                            workspace.revision,
                            workspace.available,
                        );
                    }
                }
                return Ok(());
            }
            let workspaces = client.snapshot()?.workspaces;
            if json {
                let workspaces = workspaces
                    .iter()
                    .map(|workspace| {
                        let summary = agent_attention_projection::summarize_workspace(workspace);
                        cli_output::WorkspaceSummary {
                            id: workspace.id.clone(),
                            name: workspace.name.clone(),
                            shell_count: workspace.shells.len(),
                            launcher_count: workspace.launchers.len(),
                            agent_count: workspace.agents.len(),
                            agent_state_counts: summary.states,
                            attention_count: summary.attention_count,
                        }
                    })
                    .collect::<Vec<_>>();
                return print_json(
                    CommandKey::WorkspaceList,
                    serde_json::json!({ "workspaces": workspaces }),
                );
            }
            println!("NAME\tWORKSPACE ID\tSHELLS\tLAUNCHERS\tAGENTS\tBLOCKED\tDONE\tATTENTION");
            for workspace in workspaces {
                let summary = agent_attention_projection::summarize_workspace(&workspace);
                println!(
                    "{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}",
                    sanitize_table_cell(&workspace.name),
                    workspace.id,
                    workspace.shells.len(),
                    workspace.launchers.len(),
                    workspace.agents.len(),
                    summary.states.blocked,
                    summary.states.done,
                    summary.attention_count,
                );
            }
        }
        WorkspaceCommands::Select { target } => {
            require_protocol_feature(&client, protocol::ProtocolFeature::GlobalWorkspaces)?;
            let combined = client.combined_node_snapshot(None)?;
            let workspace = resolve_global_workspace_target(&combined.workspaces, &target)
                .ok_or_else(|| {
                    cli_output::failure("not_found", format!("Workspace not found: {target}"))
                })?;
            if workspace.closing {
                return Err(cli_output::failure(
                    "invalid_argument",
                    "a closing Workspace cannot be selected",
                ));
            }
            let selection = workspace_selection::WorkspaceSelection::new(&workspace.id)?;
            workspace_selection::save_from_environment(&selection)?;
            println!("Selected workspace {} ({})", workspace.name, workspace.id);
        }
        WorkspaceCommands::Current => {
            let selection = current_selection.expect("selection was loaded before connecting");
            require_protocol_feature(&client, protocol::ProtocolFeature::GlobalWorkspaces)?;
            let combined = client.combined_node_snapshot(None)?;
            let workspace = find_global_workspace_by_id(
                &combined.workspaces,
                selection.workspace_id(),
            )
            .ok_or_else(|| {
                cli_output::failure(
                    "not_found",
                    format!(
                        "selected Workspace {} no longer exists; run `boomux workspace clear`",
                        selection.workspace_id()
                    ),
                )
            })?;
            println!("Selected workspace {} ({})", workspace.name, workspace.id);
        }
        WorkspaceCommands::Clear => unreachable!(),
        WorkspaceCommands::Create {
            name,
            node,
            cwd,
            open,
        } => {
            if let (Some(node), Some(cwd)) = (node, cwd) {
                require_protocol_feature(&client, protocol::ProtocolFeature::GlobalWorkspaces)?;
                let combined = client.combined_node_snapshot(None)?;
                let selected_node = select_eligible_workspace_owner(&combined.nodes, Some(&node))?;
                let cwd = resolve_owner_directory(&client, &selected_node, cwd)?;
                let explicit_name = name.map(|name| cli_name(name, "workspace")).transpose()?;
                let mut rejected_names = BTreeSet::new();
                let (workspace, shell, gate) = loop {
                    let workspace_name = match &explicit_name {
                        Some(name) => name.clone(),
                        None => generated_workspace_name(
                            combined
                                .workspaces
                                .iter()
                                .map(|workspace| workspace.name.as_str())
                                .chain(rejected_names.iter().map(String::as_str)),
                        )?,
                    };
                    let shell_name = generated_shell_name(std::iter::empty())?;
                    let shell_id = Uuid::new_v4().to_string();
                    let global_workspace_id = Uuid::new_v4().to_string();
                    let title = if selected_node.local {
                        format!("{workspace_name} - {shell_name}")
                    } else {
                        format!("[{}] {workspace_name} - {shell_name}", selected_node.alias)
                    };
                    let gate = if open {
                        if hyprland_special_workspaces_enabled()? {
                            Some(terminal::open_waiting_placed(
                                terminal_override,
                                (!selected_node.local).then_some(selected_node.node_id.as_str()),
                                &selected_node.node_id,
                                &global_workspace_id,
                                &shell_id,
                                &title,
                            )?)
                        } else {
                            Some(terminal::open_waiting(
                                terminal_override,
                                (!selected_node.local).then_some(selected_node.node_id.as_str()),
                                &shell_id,
                                &title,
                            )?)
                        }
                    } else {
                        None
                    };
                    match client.create_global_workspace_with_shell(
                        Uuid::new_v4().to_string(),
                        &global_workspace_id,
                        &workspace_name,
                        &selected_node.node_id,
                        Uuid::new_v4().to_string(),
                        cwd.clone(),
                        shell_id,
                        shell_spec(shell_name, &cwd, &[]),
                    ) {
                        Ok((workspace, shell)) => break (workspace, shell, gate),
                        Err(client::ClientError::Remote(error))
                            if explicit_name.is_none()
                                && error.code == Some(protocol::ErrorCode::AlreadyExists) =>
                        {
                            rejected_names.insert(workspace_name);
                        }
                        Err(error) => return Err(error.into()),
                    }
                };
                let placement = workspace
                    .placements
                    .iter()
                    .find(|placement| placement.node_id == selected_node.node_id)
                    .ok_or_else(|| {
                        cli_output::failure(
                            "internal",
                            "created Workspace response omitted the requested placement",
                        )
                    })?;
                let presentation_warning = gate.and_then(|gate| {
                    gate.release().err().map(|error| {
                        format!(
                            "Shell {} was created but its terminal could not attach: {error}",
                            shell.id
                        )
                    })
                });
                if json {
                    return print_json(
                        CommandKey::WorkspaceCreate,
                        serde_json::json!({
                            "workspace": {
                                "id": workspace.id,
                                "name": workspace.name,
                                "revision": workspace.revision,
                            },
                            "placement": {
                                "node_id": placement.node_id,
                                "owner_workspace_id": placement.workspace_id,
                                "default_cwd": placement.default_cwd,
                            },
                            "shell": {
                                "id": shell.id,
                                "name": shell.name,
                                "node_id": selected_node.node_id,
                                "cwd": shell.cwd,
                            },
                            "presentation_warning": presentation_warning,
                        }),
                    );
                }
                println!(
                    "Created workspace {} ({}) with shell {} ({}) on Node {}",
                    workspace.name, workspace.id, shell.name, shell.id, selected_node.alias
                );
                if let Some(warning) = presentation_warning {
                    eprintln!("boomux: warning: {warning}");
                }
                return Ok(());
            }

            let name = cli_name(
                name.expect("clap requires a name without --node"),
                "workspace",
            )?;
            if client.supports(protocol::ProtocolFeature::GlobalWorkspaces)? {
                let workspace = client.create_global_workspace(name)?;
                println!("Created workspace {} ({})", workspace.name, workspace.id);
                return Ok(());
            }
            let workspace = client.create_workspace_with_default_cwd(name, None, Vec::new())?;
            println!("Created workspace {} ({})", workspace.name, workspace.id);
        }
        WorkspaceCommands::Open { target, node, show } => {
            if let Some(node_selector) = node {
                if show {
                    return Err(cli_output::failure(
                        "invalid_argument",
                        "--show requires a coordinated Workspace without --node",
                    ));
                }
                let combined = client.combined_node_snapshot(None)?;
                let selected_node = resolve_workspace_open_node(&combined.nodes, &node_selector)?;
                if selected_node.local {
                    let snapshot = client.snapshot()?;
                    let workspace = resolve_workspace_target(&snapshot.workspaces, &target)?;
                    let terminal = effective_terminal(terminal_override)?;
                    open_workspace(workspace, terminal.as_deref(), None, true)?;
                    println!(
                        "Opened {} launcher(s) and {} shell(s) for {} on Node {}",
                        workspace.launchers.len(),
                        workspace_user_shell_count(workspace),
                        workspace.name,
                        selected_node.alias,
                    );
                    return Ok(());
                }
                let registration = client.node_registration(&selected_node.node_id)?;
                let workspace = match client.route_node_operation(
                    &registration.node_id,
                    protocol::RoutedOperation::GetWorkspace {
                        workspace_id: target.clone(),
                    },
                )? {
                    protocol::RoutedOperationResult::Workspace { workspace } => workspace,
                    _ => return Err("remote Node returned an unexpected workspace response".into()),
                };
                let terminal = effective_terminal(terminal_override)?;
                let mut failures = Vec::new();
                for launcher in &workspace.launchers {
                    if let Err(error) = client.route_node_host_service(
                        &registration.node_id,
                        protocol::HostServiceOperation::InvokeLauncher {
                            workspace_id: workspace.id.clone(),
                            launcher_id: launcher.id.clone(),
                        },
                    ) {
                        failures.push(format!("launcher {}: {error}", launcher.name));
                    }
                }
                for shell in &workspace.shells {
                    if let Err(error) = terminal::open_remote(
                        terminal.as_deref(),
                        &registration.node_id,
                        &shell.id,
                        &format!(
                            "[{}] {} - {}",
                            registration.alias, workspace.name, shell.name
                        ),
                        true,
                    ) {
                        failures.push(format!("shell {}: {error}", shell.name));
                    }
                }
                if !failures.is_empty() {
                    return Err(io::Error::other(format!(
                        "remote workspace opened with failures: {}",
                        failures.join("; ")
                    ))
                    .into());
                }
                println!(
                    "Opened {} launcher(s) and {} shell(s) for {} on Node {}",
                    workspace.launchers.len(),
                    workspace_user_shell_count(&workspace),
                    workspace.name,
                    registration.alias,
                );
                return Ok(());
            }
            if client.supports(protocol::ProtocolFeature::GlobalWorkspaces)? {
                let combined = client.combined_node_snapshot(None)?;
                if let Some(workspace) =
                    resolve_global_workspace_target(&combined.workspaces, &target).cloned()
                {
                    if show {
                        show_desktop_workspace(&workspace)?;
                    }
                    let (launchers, shells) = open_coordinated_workspace(
                        &client,
                        &combined,
                        &workspace,
                        terminal_override,
                        true,
                    )?;
                    println!(
                        "Opened {launchers} launcher(s) and {shells} shell(s) for {}",
                        workspace.name
                    );
                    return Ok(());
                }
            }
            let snapshot = client.snapshot()?;
            let workspace = resolve_workspace_target(&snapshot.workspaces, &target)?;
            let terminal = effective_terminal(terminal_override)?;
            open_workspace(workspace, terminal.as_deref(), None, true)?;
            println!(
                "Opened {} launcher(s) and {} shell(s) for {}",
                workspace.launchers.len(),
                workspace_user_shell_count(workspace),
                workspace.name
            );
        }
        WorkspaceCommands::Inspect { target } => {
            if client.supports(protocol::ProtocolFeature::GlobalWorkspaces)? {
                let combined = client.combined_node_snapshot(None)?;
                if let Some(workspace) =
                    resolve_global_workspace_target(&combined.workspaces, &target)
                {
                    if json {
                        return print_json(
                            CommandKey::WorkspaceInspect,
                            serde_json::json!({ "workspace": workspace }),
                        );
                    }
                    println!("ID\t{}", workspace.id);
                    println!("NAME\t{}", workspace.name);
                    println!("REVISION\t{}", workspace.revision);
                    println!(
                        "STATE\t{}",
                        if workspace.closing {
                            "closing"
                        } else {
                            "active"
                        }
                    );
                    println!("PLACEMENTS\t{}", workspace.placements.len());
                    if !workspace.placements.is_empty() {
                        println!(
                            "\nNODE ID\tOWNER WORKSPACE ID\tOWNER REVISION\tSTATE\tDEFAULT CWD"
                        );
                        for placement in &workspace.placements {
                            println!(
                                "{}\t{}\t{}\t{:?}\t{}",
                                placement.node_id,
                                placement.workspace_id,
                                placement.owner_revision,
                                placement.state,
                                placement
                                    .default_cwd
                                    .as_deref()
                                    .map_or_else(|| "-".into(), |path| path.display().to_string()),
                            );
                        }
                    }
                    return Ok(());
                }
            }
            let snapshot = client.snapshot()?;
            let workspace = resolve_workspace_target(&snapshot.workspaces, &target)?;
            let agent_summary = agent_attention_projection::summarize_workspace(workspace);
            if json {
                let shells = workspace
                    .shells
                    .iter()
                    .map(|shell| cli_output::shell(shell, Some(&workspace.name)))
                    .collect::<Vec<_>>();
                return print_json(
                    CommandKey::WorkspaceInspect,
                    serde_json::json!({
                        "workspace": {
                            "id": workspace.id,
                            "name": workspace.name,
                            "default_cwd": workspace.default_cwd,
                            "shells": shells,
                            "launchers": workspace.launchers.iter()
                                .map(|launcher| cli_output::launcher(launcher, Some(&workspace.name)))
                                .collect::<Vec<_>>(),
                            "agents": workspace.agents.iter()
                                .map(|agent| cli_output::agent(agent, Some(&workspace.name)))
                                .collect::<Vec<_>>(),
                            "agent_state_counts": agent_summary.states,
                            "attention_count": agent_summary.attention_count,
                        }
                    }),
                );
            }
            println!("ID\t{}", workspace.id);
            println!("NAME\t{}", workspace.name);
            println!(
                "DEFAULT CWD\t{}",
                workspace
                    .default_cwd
                    .as_deref()
                    .map_or_else(|| "-".into(), |cwd| cwd.display().to_string())
            );
            println!("SHELLS\t{}", workspace.shells.len());
            println!("LAUNCHERS\t{}", workspace.launchers.len());
            println!("AGENTS\t{}", workspace.agents.len());
            println!("BLOCKED AGENTS\t{}", agent_summary.states.blocked);
            println!("COMPLETED AGENTS\t{}", agent_summary.states.done);
            println!("ATTENTION\t{}", agent_summary.attention_count);
            if !workspace.shells.is_empty() {
                println!("\nNAME\tSHELL ID\tRUN ID\tSTATUS\tCWD");
                for shell in &workspace.shells {
                    println!(
                        "{}\t{}\t{}\t{}\t{}",
                        shell.name,
                        shell.id,
                        shell.run.as_ref().map_or("-", |run| run.id.as_str()),
                        shell_status(&shell.status),
                        shell.cwd.display()
                    );
                }
            }
            if !workspace.launchers.is_empty() {
                println!("\nNAME\tLAUNCHER ID\tCWD\tCOMMAND");
                for launcher in &workspace.launchers {
                    println!(
                        "{}\t{}\t{}\t{}",
                        launcher.name,
                        launcher.id,
                        launcher.cwd.display(),
                        launcher.command.join(" ")
                    );
                }
            }
            if !workspace.agents.is_empty() {
                println!("\nNAME\tAGENT ID\tSHELL ID\tRUN ID\tSTATE\tINTEGRATION");
                for agent in &workspace.agents {
                    println!(
                        "{}\t{}\t{}\t{}\t{}\t{}",
                        agent.name,
                        agent.id,
                        agent.shell_id,
                        agent.run_id,
                        cli_output::agent_state(agent.observation.state),
                        agent.integration
                    );
                }
            }
        }
        WorkspaceCommands::Rename { target, name } => {
            let name = cli_name(name, "workspace")?;
            if client.supports(protocol::ProtocolFeature::GlobalWorkspaces)? {
                let combined = client.combined_node_snapshot(None)?;
                if let Some(workspace) =
                    resolve_global_workspace_target(&combined.workspaces, &target)
                {
                    let renamed =
                        client.rename_global_workspace(&workspace.id, workspace.revision, &name)?;
                    println!("Renamed workspace {} to {}", workspace.name, renamed.name);
                    return Ok(());
                }
            }
            let snapshot = client.snapshot()?;
            let workspace = resolve_workspace_target(&snapshot.workspaces, &target)?;
            client.rename_workspace(&workspace.id, &name)?;
            println!("Renamed workspace {} to {name}", workspace.name);
        }
        WorkspaceCommands::SetDefaultCwd { target, node, cwd } => {
            require_protocol_feature(
                &client,
                protocol::ProtocolFeature::WorkspacePlacementDefaultCwd,
            )?;
            let combined = client.combined_node_snapshot(None)?;
            let workspace = resolve_global_workspace_target(&combined.workspaces, &target)
                .ok_or_else(|| {
                    cli_output::failure("not_found", format!("Workspace not found: {target}"))
                })?;
            let selected_node = resolve_combined_node(&combined.nodes, &node)?;
            let placement = workspace
                .placements
                .iter()
                .find(|placement| placement.node_id == selected_node.node_id)
                .ok_or_else(|| {
                    cli_output::failure(
                        "not_found",
                        "Workspace has no placement on the selected Node",
                    )
                })?;
            let local_node_id = combined
                .nodes
                .iter()
                .find(|candidate| candidate.local)
                .map(|candidate| candidate.node_id.as_str())
                .unwrap_or_default();
            let owner = routed_dashboard_workspace(
                &client,
                &protocol::QualifiedIdentity::new(&placement.node_id, &placement.workspace_id),
                local_node_id,
            )
            .map_err(io::Error::other)?;
            if owner.revision != placement.owner_revision {
                return Err(cli_output::failure(
                    "revision_changed",
                    "owner Workspace changed since the coordinator placement was published",
                ));
            }
            let cwd = resolve_owner_directory(&client, selected_node, cwd)?;
            let result = client.set_global_workspace_default_cwd(
                Uuid::new_v4().to_string(),
                &workspace.id,
                workspace.revision,
                &placement.node_id,
                &placement.workspace_id,
                owner.revision,
                cwd,
            )?;
            if json {
                return print_json(
                    CommandKey::WorkspaceSetDefaultCwd,
                    serde_json::json!({
                        "workspace_id": result.workspace_id,
                        "node_id": result.node_id,
                        "owner_workspace_id": result.owner_workspace_id,
                        "default_cwd": result.default_cwd,
                        "global_revision": result.global_revision,
                        "owner_revision": result.owner_revision,
                        "result": result.result,
                    }),
                );
            }
            println!(
                "{} default cwd for workspace {} on Node {} to {}",
                if result.result == "updated" {
                    "Updated"
                } else {
                    "Kept"
                },
                workspace.name,
                selected_node.alias,
                result.default_cwd.display()
            );
        }
        WorkspaceCommands::Adopt { target, node } => {
            require_protocol_feature(&client, protocol::ProtocolFeature::GlobalWorkspaces)?;
            let combined = client.combined_node_snapshot(None)?;
            let node = resolve_combined_node(&combined.nodes, &node)?;
            if !node.workspace_owner_eligible {
                return Err(cli_output::failure(
                    "busy",
                    node.workspace_owner_unavailable_reason
                        .clone()
                        .unwrap_or_else(|| "owner Node is not eligible for placement".into()),
                ));
            }
            let external =
                resolve_external_workspace(&combined.external_workspaces, &node.node_id, &target)?;
            if !external.available {
                return Err(cli_output::failure(
                    "busy",
                    "unavailable owner Workspace cannot be adopted",
                ));
            }
            let local_node_id = combined
                .nodes
                .iter()
                .find(|candidate| candidate.local)
                .map(|candidate| candidate.node_id.as_str())
                .unwrap_or_default();
            let owner = routed_dashboard_workspace(&client, &external.identity, local_node_id)
                .map_err(io::Error::other)?;
            let workspace =
                client.adopt_node_workspace(external.identity.clone(), owner.revision)?;
            println!("Adopted workspace {} ({})", workspace.name, workspace.id);
        }
        WorkspaceCommands::Link {
            target,
            owner,
            node,
        } => {
            require_protocol_feature(&client, protocol::ProtocolFeature::GlobalWorkspaces)?;
            let combined = client.combined_node_snapshot(None)?;
            let workspace = resolve_global_workspace_target(&combined.workspaces, &target)
                .ok_or_else(|| {
                    cli_output::failure("not_found", format!("workspace not found: {target}"))
                })?;
            let node = resolve_combined_node(&combined.nodes, &node)?;
            if !node.workspace_owner_eligible {
                return Err(cli_output::failure(
                    "busy",
                    node.workspace_owner_unavailable_reason
                        .clone()
                        .unwrap_or_else(|| "owner Node is not eligible for placement".into()),
                ));
            }
            let external =
                resolve_external_workspace(&combined.external_workspaces, &node.node_id, &owner)?;
            if !external.available {
                return Err(cli_output::failure(
                    "busy",
                    "unavailable owner Workspace cannot be linked",
                ));
            }
            let local_node_id = combined
                .nodes
                .iter()
                .find(|candidate| candidate.local)
                .map(|candidate| candidate.node_id.as_str())
                .unwrap_or_default();
            let owner = routed_dashboard_workspace(&client, &external.identity, local_node_id)
                .map_err(io::Error::other)?;
            let workspace = client.link_node_workspace(
                &workspace.id,
                workspace.revision,
                external.identity.clone(),
                owner.revision,
            )?;
            println!("Linked owner Workspace to {}", workspace.name);
        }
        WorkspaceCommands::Close { target } => {
            if client.supports(protocol::ProtocolFeature::GlobalWorkspaces)? {
                let combined = client.combined_node_snapshot(None)?;
                if let Some(workspace) =
                    resolve_global_workspace_target(&combined.workspaces, &target)
                {
                    if let Ok(shell_id) = env::var("BOOMUX_SHELL_ID") {
                        let local_node_id = combined
                            .nodes
                            .iter()
                            .find(|node| node.local)
                            .map(|node| node.node_id.as_str());
                        let current_owner = workspace
                            .placements
                            .iter()
                            .find(|placement| Some(placement.node_id.as_str()) == local_node_id);
                        if let Some(current_owner) = current_owner {
                            match client.get_workspace(&current_owner.workspace_id) {
                                Ok(owner)
                                    if owner.shells.iter().any(|shell| shell.id == shell_id) =>
                                {
                                    return Err(
                                        "cannot close the current workspace from inside it; use the dashboard or another shell"
                                            .into(),
                                    );
                                }
                                Ok(_) => {}
                                Err(client::ClientError::Remote(error))
                                    if error.code == Some(protocol::ErrorCode::NotFound) => {}
                                Err(error) => return Err(error.into()),
                            }
                        }
                    }
                    let result = if workspace.closing {
                        client.retry_global_workspace_close(&workspace.id)?
                    } else {
                        client.close_global_workspace(&workspace.id, workspace.revision)?
                    };
                    let unresolved = result
                        .placements
                        .iter()
                        .filter(|placement| placement.status != "closed")
                        .count();
                    if unresolved > 0 {
                        return Err(io::Error::other(format!(
                            "Workspace {} close remains unresolved on {unresolved} Node(s); retry with the same Workspace ID",
                            workspace.name
                        ))
                        .into());
                    }
                    println!(
                        "Closed workspace {} across all confirmed placements",
                        workspace.name
                    );
                    return Ok(());
                }
            }
            let snapshot = client.snapshot()?;
            let workspace = resolve_workspace_target(&snapshot.workspaces, &target)?;
            if env::var("BOOMUX_SHELL_ID")
                .ok()
                .is_some_and(|shell_id| workspace.shells.iter().any(|shell| shell.id == shell_id))
            {
                return Err(
                    "cannot close the current workspace from inside it; use the dashboard or another shell"
                        .into(),
                );
            }
            client.close_workspace(&workspace.id)?;
            println!("Closed workspace {}", workspace.name);
        }
        WorkspaceCommands::Retry { target } => {
            require_protocol_feature(&client, protocol::ProtocolFeature::GlobalWorkspaces)?;
            let combined = client.combined_node_snapshot(None)?;
            let workspace = resolve_global_workspace_target(&combined.workspaces, &target)
                .ok_or_else(|| {
                    cli_output::failure("not_found", format!("workspace not found: {target}"))
                })?;
            if !workspace.closing {
                return Err(cli_output::failure(
                    "invalid_argument",
                    "Workspace is not awaiting close retry",
                ));
            }
            let result = client.retry_global_workspace_close(&workspace.id)?;
            let unresolved = result
                .placements
                .iter()
                .filter(|placement| placement.status == "unresolved")
                .count();
            println!("Retried Workspace close; {unresolved} placement(s) unresolved");
        }
    }
    Ok(())
}

fn shell_command(
    command: ShellCommands,
    json: bool,
    terminal_override: Option<&str>,
) -> Result<(), Box<dyn Error>> {
    let client = client::connect_or_start()?;
    match command {
        ShellCommands::SuggestName { workspace, node } => {
            if let Some(node) = node {
                let registration = client.node_registration(node)?;
                let workspace = required_owner_workspace_target(
                    &client,
                    workspace.as_deref(),
                    &registration.node_id,
                )?;
                let result = client.route_node_host_service(
                    &registration.node_id,
                    protocol::HostServiceOperation::SuggestShellName {
                        workspace_id: workspace.clone(),
                    },
                )?;
                let protocol::HostServiceResult::ShellName { workspace_id, name } = result else {
                    return Err("remote Node returned an unexpected shell-name response".into());
                };
                if json {
                    return print_json(
                        CommandKey::ShellSuggestName,
                        serde_json::json!({
                            "node_id": registration.node_id,
                            "workspace_id": workspace_id,
                            "name": name,
                        }),
                    );
                }
                println!(
                    "Suggested shell name {name} for {workspace_id} on Node {}",
                    registration.alias
                );
                println!(
                    "This suggestion is not reserved; shell creation can still fail if the name is already in use."
                );
                return Ok(());
            }
            let snapshot = client.snapshot()?;
            let workspace = local_workspace_argument(&client, workspace.as_deref())?;
            let workspace = resolve_context_workspace(&snapshot, workspace.as_deref())?;
            let name =
                generated_shell_name(workspace.shells.iter().map(|shell| shell.name.as_str()))?;
            if json {
                let node_id = if client.supports(protocol::ProtocolFeature::GlobalWorkspaces)? {
                    Some(
                        client
                            .combined_node_snapshot(None)?
                            .nodes
                            .into_iter()
                            .find(|node| node.local)
                            .map(|node| node.node_id)
                            .ok_or_else(|| {
                                cli_output::failure(
                                    "internal",
                                    "local Node identity is unavailable",
                                )
                            })?,
                    )
                } else {
                    None
                };
                return print_json(
                    CommandKey::ShellSuggestName,
                    shell_suggestion_json(&workspace.id, &name, node_id.as_deref()),
                );
            }
            println!(
                "Suggested shell name {name} for {} ({})",
                workspace.name, workspace.id
            );
            println!(
                "This suggestion is not reserved; shell creation can still fail if the name is already in use."
            );
        }
        ShellCommands::Create {
            workspace,
            node,
            name,
            cwd,
            open,
            command,
        } => {
            let context = WorkspaceCreationContext::load(&client)?;
            let workspace =
                context.required_target(&client, workspace.as_deref(), node.is_some())?;
            let selection = context.coordinated_owner(&workspace, node.as_deref())?;
            if let Some(selection) = require_explicit_coordinated_owner(
                selection,
                node.as_deref(),
                context.global_workspaces_supported,
            )? {
                let requested_cwd = cwd
                    .or_else(|| selection.default_cwd.clone())
                    .unwrap_or_else(|| PathBuf::from("."));
                let cwd = resolve_owner_directory(&client, &selection.node, requested_cwd)?;
                let name = match name {
                    Some(name) => cli_name(name, "shell")?,
                    None => generated_shell_name(std::iter::empty())?,
                };
                let shell_id = Uuid::new_v4().to_string();
                let title = if selection.node.local {
                    format!("{} - {name}", selection.workspace.name)
                } else {
                    format!(
                        "[{}] {} - {name}",
                        selection.node.alias, selection.workspace.name
                    )
                };
                let gate = if open {
                    if hyprland_special_workspaces_enabled()? {
                        Some(terminal::open_waiting_placed(
                            terminal_override,
                            (!selection.node.local).then_some(selection.node.node_id.as_str()),
                            &selection.node.node_id,
                            &selection.workspace.id,
                            &shell_id,
                            &title,
                        )?)
                    } else {
                        Some(terminal::open_waiting(
                            terminal_override,
                            (!selection.node.local).then_some(selection.node.node_id.as_str()),
                            &shell_id,
                            &title,
                        )?)
                    }
                } else {
                    None
                };
                let owner_default_cwd = selection.default_cwd.clone().or_else(|| Some(cwd.clone()));
                let (_, shell) = client.create_global_workspace_shell(
                    Uuid::new_v4().to_string(),
                    &selection.workspace.id,
                    selection.workspace.revision,
                    &selection.node.node_id,
                    selection.owner_workspace_id,
                    owner_default_cwd,
                    shell_id,
                    shell_spec(name, &cwd, &command),
                )?;
                println!(
                    "Created pending shell {} ({}) in {} on Node {}",
                    shell.name, shell.id, selection.workspace.name, selection.node.alias
                );
                if let Some(gate) = gate
                    && let Err(error) = gate.release()
                {
                    return Err(format!(
                        "Shell {} was created but its terminal could not attach: {error}",
                        shell.id
                    )
                    .into());
                }
                return Ok(());
            }
            let snapshot = client.snapshot()?;
            let workspace = resolve_workspace_target(&snapshot.workspaces, &workspace)?;
            let cwd = resolve_shell_cwd(
                workspace.default_cwd.as_deref(),
                cwd.as_deref(),
                Path::new("."),
            )?;
            let shell = if let Some(name) = name {
                client.create_shell(
                    &workspace.id,
                    shell_spec(cli_name(name, "shell")?, &cwd, &command),
                )?
            } else {
                create_generated_shell(&client, &workspace.id, &cwd, &command)?
            };
            println!("Created pending shell {} ({})", shell.name, shell.id);
            if open {
                open_terminal(
                    &shell.id,
                    &format!("{} - {}", workspace.name, shell.name),
                    false,
                    terminal_override,
                )?;
            }
        }
        ShellCommands::Inspect { target, workspace } => {
            let snapshot = client.snapshot()?;
            let workspace = resource_workspace_argument(
                &client,
                workspace.as_deref(),
                find_shell(&snapshot, &target).is_some(),
            )?;
            let shell = resolve_cli_shell(&snapshot, &target, workspace.as_deref())?;
            let workspace = snapshot
                .workspaces
                .iter()
                .find(|workspace| workspace.id == shell.workspace_id)
                .ok_or_else(|| {
                    cli_output::failure("not_found", "shell workspace no longer exists")
                })?;
            if json {
                return print_json(
                    CommandKey::ShellInspect,
                    serde_json::json!({
                        "shell": cli_output::shell(shell, Some(&workspace.name)),
                    }),
                );
            }
            println!("ID\t{}", shell.id);
            println!("NAME\t{}", shell.name);
            println!("WORKSPACE\t{}", workspace.name);
            println!("STATUS\t{}", shell_status(&shell.status));
            println!("CWD\t{}", shell.cwd.display());
            if let Some(run) = &shell.run {
                println!("RUN ID\t{}", run.id);
                println!("GENERATION\t{}", run.generation);
                println!("STARTED AT MS\t{}", run.started_at_ms);
                println!(
                    "ENDED AT MS\t{}",
                    run.ended_at_ms
                        .map_or_else(|| "-".into(), |value| value.to_string())
                );
                println!(
                    "EXIT REASON\t{}",
                    run.exit_reason
                        .as_ref()
                        .map_or_else(|| "-".into(), shell_exit_reason)
                );
                println!("OUTPUT REVISION\t{}", run.output_revision);
                println!("ENVIRONMENT HAS RUN ID\t{}", run.environment_has_run_id);
            }
        }
        ShellCommands::Rename {
            target,
            name,
            workspace,
        } => {
            let name = cli_name(name, "shell")?;
            let snapshot = client.snapshot()?;
            let workspace = resource_workspace_argument(
                &client,
                workspace.as_deref(),
                find_shell(&snapshot, &target).is_some(),
            )?;
            let shell = resolve_cli_shell(&snapshot, &target, workspace.as_deref())?;
            client.rename_shell(&shell.id, &name)?;
            println!("Renamed shell {} to {name}", shell.name);
        }
        ShellCommands::Close { target, workspace } => {
            close_shell_with_workspace(&client, &target, workspace.as_deref())?;
        }
    }
    Ok(())
}

fn launcher_command(command: LauncherCommands, json: bool) -> Result<(), Box<dyn Error>> {
    let client = client::connect_or_start()?;
    match command {
        LauncherCommands::List { workspace } => {
            let snapshot = client.snapshot()?;
            let workspace = local_workspace_argument(&client, workspace.as_deref())?;
            let workspace = resolve_context_workspace(&snapshot, workspace.as_deref())?;
            if json {
                let launchers = workspace
                    .launchers
                    .iter()
                    .map(|launcher| cli_output::launcher(launcher, Some(&workspace.name)))
                    .collect::<Vec<_>>();
                return print_json(
                    CommandKey::LauncherList,
                    serde_json::json!({
                        "workspace_id": workspace.id,
                        "workspace_name": workspace.name,
                        "launchers": launchers,
                    }),
                );
            }
            println!("NAME\tLAUNCHER ID\tCWD\tCOMMAND");
            for launcher in &workspace.launchers {
                println!(
                    "{}\t{}\t{}\t{}",
                    launcher.name,
                    launcher.id,
                    launcher.cwd.display(),
                    launcher.command.join(" ")
                );
            }
        }
        LauncherCommands::Create {
            name,
            workspace,
            node,
            cwd,
            command,
        } => {
            let context = WorkspaceCreationContext::load(&client)?;
            let workspace =
                context.required_target(&client, workspace.as_deref(), node.is_some())?;
            let name = cli_name(name, "workspace launcher")?;
            let selection = context.coordinated_owner(&workspace, node.as_deref())?;
            if let Some(selection) = require_explicit_coordinated_owner(
                selection,
                node.as_deref(),
                context.global_workspaces_supported,
            )? {
                let cwd = resolve_owner_directory(&client, &selection.node, cwd)?;
                let owner_default_cwd = selection.default_cwd.clone().or_else(|| Some(cwd.clone()));
                let (_, launcher) = client.create_global_workspace_launcher(
                    Uuid::new_v4().to_string(),
                    &selection.workspace.id,
                    selection.workspace.revision,
                    &selection.node.node_id,
                    selection.owner_workspace_id,
                    owner_default_cwd,
                    Uuid::new_v4().to_string(),
                    WorkspaceLauncherSpec { name, cwd, command },
                )?;
                println!(
                    "Created launcher {} ({}) in {} on Node {}",
                    launcher.name, launcher.id, selection.workspace.name, selection.node.alias
                );
                return Ok(());
            }
            let snapshot = client.snapshot()?;
            let workspace = resolve_workspace_target(&snapshot.workspaces, &workspace)?;
            let launcher = client.create_launcher(
                &workspace.id,
                WorkspaceLauncherSpec {
                    name,
                    cwd: resolve_directory(&cwd)?,
                    command,
                },
            )?;
            println!(
                "Created launcher {} ({}) in {}",
                launcher.name, launcher.id, workspace.name
            );
        }
        LauncherCommands::Inspect { target, workspace } => {
            let snapshot = client.snapshot()?;
            let workspace = resource_workspace_argument(
                &client,
                workspace.as_deref(),
                find_launcher(&snapshot, &target).is_some(),
            )?;
            let launcher = resolve_cli_launcher(&snapshot, &target, workspace.as_deref())?;
            let workspace = snapshot
                .workspaces
                .iter()
                .find(|workspace| workspace.id == launcher.workspace_id)
                .ok_or_else(|| {
                    cli_output::failure("not_found", "launcher workspace no longer exists")
                })?;
            if json {
                return print_json(
                    CommandKey::LauncherInspect,
                    serde_json::json!({
                        "launcher": cli_output::launcher(launcher, Some(&workspace.name)),
                    }),
                );
            }
            println!("ID\t{}", launcher.id);
            println!("NAME\t{}", launcher.name);
            println!("WORKSPACE\t{}", workspace.name);
            println!("CWD\t{}", launcher.cwd.display());
            println!("COMMAND\t{}", launcher.command.join(" "));
        }
        LauncherCommands::Invoke {
            target,
            workspace,
            node,
        } => {
            if let Some(node) = node {
                let registration = client.node_registration(node)?;
                let launcher = match client.route_node_operation(
                    &registration.node_id,
                    protocol::RoutedOperation::GetLauncher {
                        launcher_id: target.clone(),
                    },
                )? {
                    protocol::RoutedOperationResult::Launcher { launcher } => launcher,
                    _ => return Err("remote Node returned an unexpected launcher response".into()),
                };
                if let Some(workspace) = workspace.as_deref()
                    && workspace != launcher.workspace_id
                {
                    return Err(cli_output::failure(
                        "invalid_argument",
                        "launcher is not owned by the requested workspace",
                    ));
                }
                let workspace_snapshot = match client.route_node_operation(
                    &registration.node_id,
                    protocol::RoutedOperation::GetWorkspace {
                        workspace_id: launcher.workspace_id.clone(),
                    },
                )? {
                    protocol::RoutedOperationResult::Workspace { workspace } => workspace,
                    _ => return Err("remote Node returned an unexpected workspace response".into()),
                };
                client.route_node_host_service(
                    &registration.node_id,
                    protocol::HostServiceOperation::InvokeLauncher {
                        workspace_id: launcher.workspace_id.clone(),
                        launcher_id: launcher.id.clone(),
                    },
                )?;
                println!(
                    "Launched {} from {} on Node {}",
                    launcher.name, workspace_snapshot.name, registration.alias
                );
                return Ok(());
            }
            let snapshot = client.snapshot()?;
            let workspace = resource_workspace_argument(
                &client,
                workspace.as_deref(),
                find_launcher(&snapshot, &target).is_some(),
            )?;
            let launcher = resolve_cli_launcher(&snapshot, &target, workspace.as_deref())?;
            let workspace = snapshot
                .workspaces
                .iter()
                .find(|workspace| workspace.id == launcher.workspace_id)
                .ok_or_else(|| {
                    cli_output::failure("not_found", "launcher workspace no longer exists")
                })?;
            invoke_workspace_launcher(workspace, launcher)?;
            println!("Launched {} from {}", launcher.name, workspace.name);
        }
        LauncherCommands::Rename {
            target,
            name,
            workspace,
        } => {
            let name = cli_name(name, "workspace launcher")?;
            let snapshot = client.snapshot()?;
            let workspace = resource_workspace_argument(
                &client,
                workspace.as_deref(),
                find_launcher(&snapshot, &target).is_some(),
            )?;
            let launcher = resolve_cli_launcher(&snapshot, &target, workspace.as_deref())?;
            client.rename_launcher(&launcher.id, &name)?;
            println!("Renamed launcher {} to {name}", launcher.name);
        }
        LauncherCommands::Remove { target, workspace } => {
            let snapshot = client.snapshot()?;
            let workspace = resource_workspace_argument(
                &client,
                workspace.as_deref(),
                find_launcher(&snapshot, &target).is_some(),
            )?;
            let launcher = resolve_cli_launcher(&snapshot, &target, workspace.as_deref())?;
            client.remove_launcher(&launcher.id)?;
            println!("Removed launcher {}", launcher.name);
        }
    }
    Ok(())
}

fn agent_command(command: AgentCommands, json: bool) -> Result<(), Box<dyn Error>> {
    let client = client::connect_or_start()?;
    match command {
        AgentCommands::List { workspace } => {
            let snapshot = client.snapshot()?;
            let workspaces = if let Some(target) = workspace.as_deref() {
                vec![resolve_workspace_target(&snapshot.workspaces, target)?]
            } else {
                snapshot.workspaces.iter().collect()
            };
            if json {
                let agents = workspaces
                    .iter()
                    .flat_map(|workspace| {
                        workspace
                            .agents
                            .iter()
                            .map(|agent| cli_output::agent(agent, Some(&workspace.name)))
                    })
                    .collect::<Vec<_>>();
                return print_json(
                    CommandKey::AgentList,
                    serde_json::json!({ "agents": agents }),
                );
            }
            println!("WORKSPACE\tNAME\tAGENT ID\tSHELL ID\tRUN ID\tSTATE\tCONFIDENCE");
            for workspace in workspaces {
                for agent in &workspace.agents {
                    println!(
                        "{}\t{}\t{}\t{}\t{}\t{}\t{}",
                        workspace.name,
                        agent.name,
                        agent.id,
                        agent.shell_id,
                        agent.run_id,
                        cli_output::agent_state(agent.observation.state),
                        agent.observation.confidence
                    );
                }
            }
        }
        AgentCommands::Inspect { agent_id } => {
            let agent = client.get_agent(agent_id)?;
            let workspace = client.get_workspace(&agent.workspace_id)?;
            if json {
                return print_json(
                    CommandKey::AgentInspect,
                    serde_json::json!({
                        "agent": cli_output::agent(&agent, Some(&workspace.name)),
                    }),
                );
            }
            print_agent(&agent, &workspace.name);
        }
        AgentCommands::Wait {
            agent_id,
            after_revision,
            wait_ms,
        } => {
            let waited = client.wait_agent(agent_id, after_revision, wait_ms)?;
            if json {
                let workspace = client.get_workspace(&waited.agent.workspace_id)?;
                return print_json(
                    CommandKey::AgentWait,
                    serde_json::json!({
                        "changed": waited.changed,
                        "agent": cli_output::agent(&waited.agent, Some(&workspace.name)),
                    }),
                );
            }
            println!("CHANGED\t{}", waited.changed);
            println!("ID\t{}", waited.agent.id);
            println!(
                "STATE\t{}",
                cli_output::agent_state(waited.agent.observation.state)
            );
            println!("REVISION\t{}", waited.agent.observation.revision);
        }
        AgentCommands::Register(arguments) => {
            register_or_ensure_agent(&client, arguments, json, false)?;
        }
        AgentCommands::Ensure(arguments) => {
            register_or_ensure_agent(&client, arguments, json, true)?;
        }
        AgentCommands::Supervise(_) => unreachable!(),
        AgentCommands::Report {
            agent_id,
            shell_id,
            run_id,
            state,
            authority,
            evidence,
            confidence,
        } => {
            let (shell_id, run_id) = resolve_agent_context(
                shell_id,
                run_id,
                env::var("BOOMUX_SHELL_ID").ok(),
                env::var("BOOMUX_RUN_ID").ok(),
            )?;
            let existing = client.get_agent(&agent_id)?;
            if existing.shell_id != shell_id {
                return Err(cli_output::failure(
                    "invalid_argument",
                    format!("agent {agent_id} is not bound to shell {shell_id}"),
                ));
            }
            if existing.run_id != run_id {
                return Err(cli_output::failure(
                    "run_changed",
                    format!("agent {agent_id} is not bound to run {run_id}"),
                ));
            }
            let agent = client.report_agent(
                agent_id,
                run_id,
                AgentReport {
                    state: state.into(),
                    authority: authority.into(),
                    evidence,
                    confidence,
                },
            )?;
            if json {
                let workspace = client.get_workspace(&agent.workspace_id)?;
                return print_json(
                    CommandKey::AgentReport,
                    serde_json::json!({
                        "agent": cli_output::agent(&agent, Some(&workspace.name)),
                    }),
                );
            }
            println!(
                "Reported {} for agent {} (revision {})",
                cli_output::agent_state(agent.observation.state),
                agent.id,
                agent.observation.revision
            );
        }
        AgentCommands::ObserveWorkingContext {
            agent_id,
            path,
            shell_id,
            run_id,
        } => {
            let (shell_id, run_id) = resolve_agent_context(
                shell_id,
                run_id,
                env::var("BOOMUX_SHELL_ID").ok(),
                env::var("BOOMUX_RUN_ID").ok(),
            )?;
            let (agent, changed) =
                client.observe_agent_working_context(agent_id, shell_id, run_id, path)?;
            if json {
                return print_json(
                    CommandKey::AgentObserveWorkingContext,
                    serde_json::json!({
                        "changed": changed,
                        "working_contexts": agent.working_contexts,
                    }),
                );
            }
            println!("CHANGED\t{changed}");
        }
    }
    Ok(())
}

fn attention_command(command: AttentionCommands, json: bool) -> Result<(), Box<dyn Error>> {
    let client = client::connect_or_start()?;
    validate_attention_protocol(client.protocol_version()?)?;
    match command {
        AttentionCommands::List { workspace } => {
            let snapshot = client.snapshot()?;
            let workspace_id = workspace
                .as_deref()
                .map(|target| resolve_workspace_target(&snapshot.workspaces, target))
                .transpose()?
                .map(|workspace| workspace.id.as_str());
            let items = agent_attention_projection::project_attention(&snapshot.workspaces)
                .into_iter()
                .filter(|item| workspace_id.is_none_or(|id| item.workspace_id == id))
                .collect::<Vec<_>>();
            if json {
                let attention = items
                    .iter()
                    .map(|item| {
                        serde_json::json!({
                            "workspace_id": item.workspace_id,
                            "workspace_name": item.workspace_name,
                            "reason": agent_attention_projection::attention_reason(item.attention.reason),
                            "observation": {
                                "revision": item.attention.observation.revision,
                                "state": cli_output::agent_state(item.attention.observation.state),
                                "authority": cli_output::agent_authority(item.attention.observation.authority),
                                "evidence": item.attention.observation.evidence,
                                "confidence": item.attention.observation.confidence,
                                "observed_at_ms": item.attention.observation.observed_at_ms,
                            },
                            "observation_is_current": item.observation_is_current,
                            "shell_is_retained": item.shell_is_retained,
                            "agent": cli_output::agent(&item.agent, Some(&item.workspace_name)),
                        })
                    })
                    .collect::<Vec<_>>();
                return print_json(
                    CommandKey::AttentionList,
                    serde_json::json!({ "attention": attention }),
                );
            }
            println!(
                "WORKSPACE\tREASON\tAGENT\tAGENT ID\tREVISION\tCURRENT\tSHELL\tAUTHORITY\tEVIDENCE"
            );
            for item in items {
                println!(
                    "{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}",
                    sanitize_table_cell(&item.workspace_name),
                    agent_attention_projection::attention_reason(item.attention.reason),
                    sanitize_table_cell(&item.agent.name),
                    item.agent.id,
                    item.attention.observation.revision,
                    item.observation_is_current,
                    if item.shell_is_retained {
                        "retained"
                    } else {
                        "removed"
                    },
                    cli_output::agent_authority(item.attention.observation.authority),
                    sanitize_table_cell(&item.attention.observation.evidence),
                );
            }
        }
        AttentionCommands::Acknowledge {
            agent_id,
            observation_revision,
        } => {
            let acknowledged =
                client.acknowledge_agent_attention(agent_id, observation_revision)?;
            if json {
                let workspace = client.get_workspace(&acknowledged.agent.workspace_id)?;
                return print_json(
                    CommandKey::AttentionAcknowledge,
                    serde_json::json!({
                        "changed": acknowledged.changed,
                        "agent": cli_output::agent(&acknowledged.agent, Some(&workspace.name)),
                    }),
                );
            }
            println!("CHANGED\t{}", acknowledged.changed);
            println!("AGENT ID\t{}", acknowledged.agent.id);
            println!("OBSERVATION REVISION\t{observation_revision}");
        }
    }
    Ok(())
}

fn session_command(
    command: SessionCommands,
    json: bool,
    terminal_override: Option<&str>,
) -> Result<(), Box<dyn Error>> {
    let client = client::connect_or_start()?;
    validate_session_protocol(client.protocol_version()?)?;
    if let SessionCommands::Open {
        session_id,
        node,
        workspace,
    } = &command
    {
        let local_node_id = client.node_identity()?;
        let owner_node_id = match node {
            Some(node) => client.node_registration(node)?.node_id,
            None => local_node_id.clone(),
        };
        let terminal = effective_terminal(terminal_override)?;
        let message = open_exact_session(
            &protocol::QualifiedIdentity::new(owner_node_id, session_id),
            &local_node_id,
            terminal.as_deref(),
            workspace.as_deref(),
        )
        .map_err(io::Error::other)?;
        println!("{message}");
        return Ok(());
    }
    let selected_node = match &command {
        SessionCommands::List { node, .. }
        | SessionCommands::Inspect { node, .. }
        | SessionCommands::Open { node, .. }
        | SessionCommands::Resume { node, .. }
        | SessionCommands::Rename { node, .. }
        | SessionCommands::ResetName { node, .. }
        | SessionCommands::Hide { node, .. } => node.as_deref(),
    };
    if let Some(node) = selected_node {
        let registration = client.node_registration(node)?;
        return remote_session_command(&client, &registration, command, json);
    }
    match command {
        SessionCommands::List { workspace, .. } => {
            let workspace_id = workspace
                .as_deref()
                .map(|target| {
                    let snapshot = client.snapshot()?;
                    Ok::<_, Box<dyn Error>>(
                        resolve_workspace_target(&snapshot.workspaces, target)?
                            .id
                            .clone(),
                    )
                })
                .transpose()?;
            let result = client
                .host_service(protocol::HostServiceOperation::ListAgentSessions { workspace_id })?;
            let protocol::HostServiceResult::AgentSessions { sessions } = result else {
                return Err("daemon returned an unexpected Session-list response".into());
            };
            if json {
                return print_json(
                    CommandKey::SessionList,
                    serde_json::json!({ "sessions": sessions }),
                );
            }
            println!(
                "WORKSPACE\tDESCRIPTION\tSESSION ID\tINTEGRATION\tSTATE\tLAST ACTIVITY MS\tOCCURRENCES"
            );
            for session in sessions {
                println!(
                    "{}\t{}\t{}\t{}\t{} ({})\t{}\t{}",
                    session.workspace_name,
                    session.description,
                    session.id,
                    session.integration,
                    cli_output::agent_state(session.state),
                    if session.state_is_current {
                        "current"
                    } else {
                        "last-known"
                    },
                    session.last_at_ms,
                    session.occurrence_count
                );
            }
        }
        SessionCommands::Inspect { session_id, .. } => {
            let result = client
                .host_service(protocol::HostServiceOperation::InspectAgentSession { session_id })?;
            let protocol::HostServiceResult::AgentSession { session } = result else {
                return Err("daemon returned an unexpected Session response".into());
            };
            print_host_session_inspection(&session, None, json)?;
        }
        SessionCommands::Open { .. } => unreachable!("Session open returns before projection"),
        SessionCommands::Resume { session_id, .. } => {
            let result =
                client.host_service(protocol::HostServiceOperation::InspectAgentSession {
                    session_id: session_id.clone(),
                })?;
            let protocol::HostServiceResult::AgentSession { session } = result else {
                return Err("daemon returned an unexpected Session response".into());
            };
            let terminal = effective_terminal(None)?;
            terminal::open_agent_session(
                terminal.as_deref(),
                None,
                &session_id,
                &format!(
                    "{} - {} session",
                    session.summary.workspace_name, session.summary.integration
                ),
            )?;
            println!("Opened exact {} Agent Session", session.summary.integration);
        }
        SessionCommands::Rename {
            session_id,
            name,
            revision,
            operation_id,
            ..
        } => {
            let result = client.set_agent_session_display_name(
                operation_id.unwrap_or_else(|| Uuid::new_v4().to_string()),
                session_id,
                revision,
                Some(name),
            )?;
            print_session_display_name_result(CommandKey::SessionRename, &result, None, json)?;
        }
        SessionCommands::ResetName {
            session_id,
            revision,
            operation_id,
            ..
        } => {
            let result = client.set_agent_session_display_name(
                operation_id.unwrap_or_else(|| Uuid::new_v4().to_string()),
                session_id,
                revision,
                None,
            )?;
            print_session_display_name_result(CommandKey::SessionResetName, &result, None, json)?;
        }
        SessionCommands::Hide {
            session_id,
            workspace,
            ..
        } => {
            let revision = client.get_workspace(&workspace)?.revision;
            let result = client.hide_agent_session(
                Uuid::new_v4().to_string(),
                session_id,
                workspace,
                revision,
            )?;
            print_session_hide_result(CommandKey::SessionHide, &result, None, json)?;
        }
    }
    Ok(())
}

fn remote_session_command(
    client: &client::Client,
    registration: &protocol::NodeRegistrationSnapshot,
    command: SessionCommands,
    json: bool,
) -> Result<(), Box<dyn Error>> {
    match command {
        SessionCommands::List { workspace, .. } => {
            let result = client.route_node_host_service(
                &registration.node_id,
                protocol::HostServiceOperation::ListAgentSessions {
                    workspace_id: workspace,
                },
            )?;
            let protocol::HostServiceResult::AgentSessions { sessions } = result else {
                return Err("remote Node returned an unexpected session-list response".into());
            };
            if json {
                return print_json(
                    CommandKey::SessionList,
                    serde_json::json!({ "node_id": registration.node_id, "sessions": sessions }),
                );
            }
            println!(
                "WORKSPACE\tDESCRIPTION\tSESSION ID\tINTEGRATION\tSTATE\tLAST ACTIVITY MS\tOCCURRENCES"
            );
            for session in sessions {
                println!(
                    "{}\t{}\t{}\t{}\t{} ({})\t{}\t{}",
                    session.workspace_name,
                    session.description,
                    session.id,
                    session.integration,
                    cli_output::agent_state(session.state),
                    if session.state_is_current {
                        "current"
                    } else {
                        "last-known"
                    },
                    session.last_at_ms,
                    session.occurrence_count,
                );
            }
        }
        SessionCommands::Inspect { session_id, .. } => {
            let result = client.route_node_host_service(
                &registration.node_id,
                protocol::HostServiceOperation::InspectAgentSession { session_id },
            )?;
            let protocol::HostServiceResult::AgentSession { session } = result else {
                return Err("remote Node returned an unexpected session response".into());
            };
            print_host_session_inspection(
                &session,
                Some((&registration.node_id, &registration.alias)),
                json,
            )?;
        }
        SessionCommands::Open { .. } => unreachable!("Session open uses shared exact-open logic"),
        SessionCommands::Resume { session_id, .. } => {
            let result = client.route_node_host_service(
                &registration.node_id,
                protocol::HostServiceOperation::InspectAgentSession {
                    session_id: session_id.clone(),
                },
            )?;
            let protocol::HostServiceResult::AgentSession { session } = result else {
                return Err("remote Node returned an unexpected session response".into());
            };
            let terminal = effective_terminal(None)?;
            terminal::open_agent_session(
                terminal.as_deref(),
                Some(&registration.node_id),
                &session_id,
                &format!(
                    "[{}] {} - {} session",
                    registration.alias, session.summary.workspace_name, session.summary.integration,
                ),
            )?;
            println!(
                "Opened exact {} Agent Session on Node {}",
                session.summary.integration, registration.alias
            );
        }
        SessionCommands::Rename {
            session_id,
            name,
            revision,
            operation_id,
            ..
        } => {
            let result = client.route_node_operation(
                &registration.node_id,
                protocol::RoutedOperation::SetAgentSessionDisplayName {
                    operation_id: operation_id.unwrap_or_else(|| Uuid::new_v4().to_string()),
                    session_id,
                    expected_workspace_revision: revision,
                    display_name: Some(name),
                },
            )?;
            let protocol::RoutedOperationResult::AgentSessionDisplayName { outcome: result } =
                result
            else {
                return Err("remote Node returned an unexpected Session rename response".into());
            };
            print_session_display_name_result(
                CommandKey::SessionRename,
                &result,
                Some(&registration.node_id),
                json,
            )?;
        }
        SessionCommands::ResetName {
            session_id,
            revision,
            operation_id,
            ..
        } => {
            let result = client.route_node_operation(
                &registration.node_id,
                protocol::RoutedOperation::SetAgentSessionDisplayName {
                    operation_id: operation_id.unwrap_or_else(|| Uuid::new_v4().to_string()),
                    session_id,
                    expected_workspace_revision: revision,
                    display_name: None,
                },
            )?;
            let protocol::RoutedOperationResult::AgentSessionDisplayName { outcome: result } =
                result
            else {
                return Err("remote Node returned an unexpected Session reset response".into());
            };
            print_session_display_name_result(
                CommandKey::SessionResetName,
                &result,
                Some(&registration.node_id),
                json,
            )?;
        }
        SessionCommands::Hide {
            session_id,
            workspace,
            ..
        } => {
            let workspace_snapshot = client.route_node_operation(
                &registration.node_id,
                protocol::RoutedOperation::GetWorkspace {
                    workspace_id: workspace.clone(),
                },
            )?;
            let protocol::RoutedOperationResult::Workspace {
                workspace: workspace_snapshot,
            } = workspace_snapshot
            else {
                return Err("remote Node returned an unexpected Workspace response".into());
            };
            let result = client.route_node_operation(
                &registration.node_id,
                protocol::RoutedOperation::HideAgentSession {
                    operation_id: Uuid::new_v4().to_string(),
                    session_id,
                    workspace_id: workspace,
                    expected_workspace_revision: workspace_snapshot.revision,
                },
            )?;
            let protocol::RoutedOperationResult::AgentSessionHidden { outcome: result } = result
            else {
                return Err("remote Node returned an unexpected Session hide response".into());
            };
            print_session_hide_result(
                CommandKey::SessionHide,
                &result,
                Some(&registration.node_id),
                json,
            )?;
        }
    }
    Ok(())
}

fn print_host_session_inspection(
    session: &protocol::HostAgentSessionInspection,
    node: Option<(&str, &str)>,
    json: bool,
) -> Result<(), Box<dyn Error>> {
    if json {
        let mut value = serde_json::to_value(&session.summary)?;
        let object = value
            .as_object_mut()
            .expect("Session summary serializes as object");
        object.insert(
            "source_cwd".into(),
            serde_json::to_value(&session.source_cwd)?,
        );
        object.insert(
            "occurrences".into(),
            serde_json::to_value(&session.projected_occurrences)?,
        );
        let mut data = serde_json::json!({ "session": value });
        if let Some((node_id, _)) = node {
            data.as_object_mut()
                .expect("Session inspection data is an object")
                .insert("node_id".into(), node_id.into());
        }
        return print_json(CommandKey::SessionInspect, data);
    }
    if let Some((_, alias)) = node {
        println!("NODE\t{alias}");
    }
    println!("ID\t{}", session.summary.id);
    println!("WORKSPACE\t{}", session.summary.workspace_name);
    println!("DESCRIPTION\t{}", session.summary.description);
    println!("INTEGRATION\t{}", session.summary.integration);
    println!("OCCURRENCES\t{}", session.summary.occurrence_count);
    Ok(())
}

fn print_session_display_name_result(
    command: CommandKey,
    result: &protocol::AgentSessionDisplayNameResult,
    node_id: Option<&str>,
    json: bool,
) -> Result<(), Box<dyn Error>> {
    if json {
        let mut data = serde_json::json!({ "result": result });
        if let Some(node_id) = node_id {
            data.as_object_mut()
                .expect("Session mutation data is an object")
                .insert("node_id".into(), node_id.into());
        }
        return print_json(command, data);
    }
    println!("SESSION ID\t{}", result.session_id);
    println!("WORKSPACE ID\t{}", result.workspace_id);
    println!(
        "USER DISPLAY NAME\t{}",
        result.user_display_name.as_deref().unwrap_or("-")
    );
    println!("WORKSPACE REVISION\t{}", result.workspace_revision);
    println!("CHANGED\t{}", result.changed);
    Ok(())
}

fn print_session_hide_result(
    command: CommandKey,
    result: &protocol::AgentSessionHideResult,
    node_id: Option<&str>,
    json: bool,
) -> Result<(), Box<dyn Error>> {
    if json {
        let mut data = serde_json::json!({ "result": result });
        if let Some(node_id) = node_id {
            data.as_object_mut()
                .expect("Session mutation data is an object")
                .insert("node_id".into(), node_id.into());
        }
        return print_json(command, data);
    }
    println!("SESSION ID\t{}", result.session_id);
    println!("WORKSPACE ID\t{}", result.workspace_id);
    println!("WORKSPACE REVISION\t{}", result.workspace_revision);
    println!("CHANGED\t{}", result.changed);
    Ok(())
}

fn remote_node_projection(
    client: &client::Client,
    selector: &str,
) -> Result<(protocol::NodeRegistrationSnapshot, protocol::CombinedNode), Box<dyn Error>> {
    let registration = client.node_registration(selector)?;
    let mut nodes = client
        .combined_node_snapshot(Some(registration.node_id.clone()))?
        .nodes;
    let node = nodes.pop().ok_or_else(|| {
        cli_output::failure(
            "not_found",
            "registered Node has no combined projection entry",
        )
    })?;
    Ok((registration, node))
}

fn remote_workspace_from_projection(
    client: &client::Client,
    node: &protocol::CombinedNode,
    target: &str,
) -> Result<protocol::WorkspaceSnapshot, Box<dyn Error>> {
    let projection = node.remote_projection.as_ref().ok_or_else(|| {
        cli_output::failure("not_found", "remote Node has no projected workspace state")
    })?;
    let matches = projection
        .workspaces
        .iter()
        .filter(|workspace| workspace.id == target || workspace.name == target)
        .collect::<Vec<_>>();
    let [workspace] = matches.as_slice() else {
        return Err(cli_output::failure(
            if matches.is_empty() {
                "not_found"
            } else {
                "ambiguous_target"
            },
            format!("remote workspace target did not resolve uniquely: {target}"),
        ));
    };
    match client.route_node_operation(
        &node.node_id,
        protocol::RoutedOperation::GetWorkspace {
            workspace_id: workspace.id.clone(),
        },
    )? {
        protocol::RoutedOperationResult::Workspace { workspace } => Ok(workspace),
        _ => Err("remote Node returned an unexpected workspace response".into()),
    }
}

fn retry_runner_request<T>(
    mut request: impl FnMut(&client::Client) -> client::Result<T>,
) -> Result<T, Box<dyn Error>> {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        match client::connect().and_then(|client| request(&client)) {
            Ok(value) => return Ok(value),
            Err(error) if Instant::now() >= deadline => return Err(Box::new(error)),
            Err(_) => {}
        }
        thread::sleep(Duration::from_millis(25));
    }
}

fn escape_terminal_text(value: &str) -> String {
    value
        .chars()
        .flat_map(|character| {
            if is_bidi_control(character) {
                character.escape_unicode().collect::<Vec<_>>()
            } else {
                character.escape_debug().collect::<Vec<_>>()
            }
        })
        .collect()
}

fn is_bidi_control(character: char) -> bool {
    matches!(
        character,
        '\u{061c}' | '\u{200e}' | '\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}'
    )
}

fn validate_attention_protocol(negotiated: u32) -> Result<(), Box<dyn Error>> {
    let feature = protocol::ProtocolFeature::PersistentAgentAttention;
    feature
        .is_supported_by(negotiated)
        .then_some(())
        .ok_or_else(|| {
            cli_output::failure(
                "unsupported_version",
                format!(
                    "Agent attention requires daemon protocol {}; negotiated {negotiated}",
                    feature.minimum_version()
                ),
            )
        })
}

fn sanitize_table_cell(value: &str) -> String {
    value
        .chars()
        .map(|character| {
            if character.is_control() {
                ' '
            } else {
                character
            }
        })
        .collect()
}

fn validate_session_protocol(negotiated: u32) -> Result<(), Box<dyn Error>> {
    let feature = protocol::ProtocolFeature::InactiveAgentState;
    feature
        .is_supported_by(negotiated)
        .then_some(())
        .ok_or_else(|| {
            cli_output::failure(
                "unsupported_version",
                format!(
                    "session projection requires daemon protocol {}; negotiated {negotiated}",
                    feature.minimum_version()
                ),
            )
        })
}

fn print_session(session: &session_projection::SessionProjection) {
    println!("ID\t{}", session.id);
    println!("WORKSPACE ID\t{}", session.workspace_id);
    println!("WORKSPACE\t{}", session.workspace_name);
    println!("DESCRIPTION\t{}", session.description);
    println!("INTEGRATION\t{}", session.integration);
    println!(
        "EXTERNAL SESSION ID\t{}",
        session.external_session_id.as_deref().unwrap_or("-")
    );
    println!(
        "STATE\t{} ({})",
        cli_output::agent_state(session.state),
        if session.state_is_current {
            "current"
        } else {
            "last-known"
        }
    );
    println!("STARTED AT MS\t{}", session.started_at_ms);
    println!("LAST ACTIVITY MS\t{}", session.last_at_ms);
    println!(
        "SOURCE CWD\t{}",
        session
            .source_cwd
            .as_ref()
            .map_or_else(|| "-".into(), |cwd| cwd.display().to_string())
    );
    println!("OCCURRENCES\t{}", session.occurrences.len());
    for (index, occurrence) in session.occurrences.iter().enumerate() {
        println!();
        println!("OCCURRENCE {}", index + 1);
        println!("AGENT ID\t{}", occurrence.agent_id);
        println!("SHELL ID\t{}", occurrence.shell_id);
        println!(
            "RETAINED SHELL NAME\t{}",
            occurrence.retained_shell_name.as_deref().unwrap_or("-")
        );
        println!(
            "RETAINED SHELL CWD\t{}",
            occurrence
                .retained_shell_cwd
                .as_ref()
                .map_or_else(|| "-".into(), |cwd| cwd.display().to_string())
        );
        println!(
            "SOURCE CWD\t{}",
            occurrence
                .source_cwd
                .as_ref()
                .map_or_else(|| "-".into(), |cwd| cwd.display().to_string())
        );
        println!("RUN ID\t{}", occurrence.run_id);
        println!("STARTED AT MS\t{}", occurrence.started_at_ms);
        println!(
            "ENDED AT MS\t{}",
            occurrence
                .ended_at_ms
                .map_or_else(|| "-".into(), |ended| ended.to_string())
        );
        println!("CURRENT\t{}", occurrence.is_current);
        println!("OBSERVATION REVISION\t{}", occurrence.observation.revision);
        println!(
            "OBSERVATION STATE\t{}",
            cli_output::agent_state(occurrence.observation.state)
        );
        println!(
            "OBSERVATION AUTHORITY\t{}",
            cli_output::agent_authority(occurrence.observation.authority)
        );
        println!("OBSERVATION EVIDENCE\t{}", occurrence.observation.evidence);
        println!(
            "OBSERVATION CONFIDENCE\t{}",
            occurrence.observation.confidence
        );
        println!("OBSERVED AT MS\t{}", occurrence.observation.observed_at_ms);
    }
}

fn supervise_agent(
    arguments: AgentSuperviseArgs,
) -> Result<process_adapter::ProcessExit, Box<dyn Error>> {
    let (shell_id, run_id) = resolve_agent_context(
        arguments.shell_id,
        arguments.run_id,
        env::var("BOOMUX_SHELL_ID").ok(),
        env::var("BOOMUX_RUN_ID").ok(),
    )?;
    Ok(process_adapter::supervise(
        process_adapter::SuperviseSpec {
            name: cli_name_or_generated(arguments.name, "agent")?,
            integration: arguments.integration,
            external_session_id: arguments.external_session_id,
            shell_id,
            run_id,
            command: arguments.command,
        },
    )?)
}

fn register_or_ensure_agent(
    client: &client::Client,
    arguments: AgentRegistrationArgs,
    json: bool,
    ensure: bool,
) -> Result<(), Box<dyn Error>> {
    let (shell_id, run_id) = resolve_agent_context(
        arguments.shell_id,
        arguments.run_id,
        env::var("BOOMUX_SHELL_ID").ok(),
        env::var("BOOMUX_RUN_ID").ok(),
    )?;
    let spec = AgentRegistrationSpec {
        name: cli_name_or_generated(arguments.name, "agent")?,
        integration: arguments.integration,
        external_session_id: arguments.external_session_id,
        report: AgentReport {
            state: arguments.state.into(),
            authority: arguments.authority.into(),
            evidence: arguments.evidence,
            confidence: arguments.confidence,
        },
    };
    let agent = if ensure {
        client.ensure_agent(shell_id, run_id, spec)?
    } else {
        client.register_agent(shell_id, run_id, spec)?
    };
    if json {
        let workspace = client.get_workspace(&agent.workspace_id)?;
        return print_json(
            if ensure {
                CommandKey::AgentEnsure
            } else {
                CommandKey::AgentRegister
            },
            serde_json::json!({
                "agent": cli_output::agent(&agent, Some(&workspace.name)),
            }),
        );
    }
    println!(
        "{} agent {} ({})",
        if ensure { "Ensured" } else { "Registered" },
        agent.name,
        agent.id
    );
    Ok(())
}

fn claude_hook_command() -> Result<(), Box<dyn Error>> {
    let (shell_id, run_id) = match (
        env::var("BOOMUX_SHELL_ID").ok(),
        env::var("BOOMUX_RUN_ID").ok(),
    ) {
        (Some(shell_id), Some(run_id)) => {
            resolve_agent_context(None, None, Some(shell_id), Some(run_id))?
        }
        _ => return Ok(()),
    };
    let update = claude_hooks::read_update(io::stdin().lock())?;
    let initial = update.observation.as_ref();
    let report = AgentReport {
        state: initial.map_or(AgentState::Unknown, |observation| observation.state),
        authority: AgentAuthority::LifecycleIntegration,
        evidence: initial
            .map_or("Claude lifecycle event", |observation| observation.evidence)
            .into(),
        confidence: 100,
    };
    let client = client::connect_or_start()?;
    let mut agent = client.ensure_agent(
        &shell_id,
        &run_id,
        AgentRegistrationSpec {
            name: "Claude Code".into(),
            integration: "claude".into(),
            external_session_id: Some(update.session_id),
            report: report.clone(),
        },
    )?;
    if initial.is_some()
        && agent.ended_at_ms.is_none()
        && (agent.observation.state != report.state
            || agent.observation.authority != report.authority
            || agent.observation.evidence != report.evidence
            || agent.observation.confidence != report.confidence)
    {
        agent = client.report_agent(&agent.id, &run_id, report)?;
    }
    for path in update.working_contexts {
        let _ = client.observe_agent_working_context(&agent.id, &shell_id, &run_id, path);
    }
    let bridge_session_id = if update.session_ended {
        None
    } else {
        match env::var("CLAUDE_CODE_BRIDGE_SESSION_ID") {
            Ok(value) => Some(value),
            Err(env::VarError::NotPresent) => None,
            Err(env::VarError::NotUnicode(_)) => {
                return Err("CLAUDE_CODE_BRIDGE_SESSION_ID must be valid UTF-8".into());
            }
        }
    };
    client.set_claude_remote_control_binding(agent.id, shell_id, run_id, bridge_session_id)?;
    Ok(())
}

fn codex_hook_command() -> Result<(), Box<dyn Error>> {
    if env::var("BOOMUX_CODEX_RUN_SCOPED").as_deref() != Ok("1") {
        return Ok(());
    }
    let (shell_id, run_id) = match (
        env::var("BOOMUX_SHELL_ID").ok(),
        env::var("BOOMUX_RUN_ID").ok(),
    ) {
        (Some(shell_id), Some(run_id)) => {
            resolve_agent_context(None, None, Some(shell_id), Some(run_id))?
        }
        _ => return Ok(()),
    };
    let update = codex_hooks::read_update(io::stdin().lock())?;
    let observation = update.observation;
    let report = AgentReport {
        state: observation.state,
        authority: AgentAuthority::LifecycleIntegration,
        evidence: observation.evidence.into(),
        confidence: 100,
    };
    let client = client::connect_or_start()?;
    let mut agent = client.ensure_agent(
        &shell_id,
        &run_id,
        AgentRegistrationSpec {
            name: "Codex".into(),
            integration: "codex".into(),
            external_session_id: Some(update.session_id),
            report: report.clone(),
        },
    )?;
    if agent.ended_at_ms.is_none()
        && (agent.observation.state != report.state
            || agent.observation.authority != report.authority
            || agent.observation.evidence != report.evidence
            || agent.observation.confidence != report.confidence)
    {
        agent = client.report_agent(&agent.id, &run_id, report)?;
    }
    for path in update.working_contexts {
        let _ = client.observe_agent_working_context(&agent.id, &shell_id, &run_id, path);
    }
    Ok(())
}

fn launch_codex(arguments: Vec<OsString>) -> Result<(), Box<dyn Error>> {
    let executable = resolve_real_codex()?;
    let managed_chat = arguments.is_empty()
        || arguments
            .first()
            .is_some_and(|argument| argument == "resume" || argument == "exec");
    let environment = integration_management::Environment::from_process();
    let hooks_current = integration_management::inspect_without_host_probe(
        integration_management::IntegrationId::CODEX,
        &environment,
        None,
    )
    .asset
    .state
        == integration_management::AssetState::Current;
    let mut command = Command::new(executable);
    sanitize_inherited_opencode_shim(&mut command);
    if managed_chat && hooks_current {
        command
            .args(["--enable", "hooks"])
            .env("BOOMUX_CODEX_RUN_SCOPED", "1");
    } else {
        command.env_remove("BOOMUX_CODEX_RUN_SCOPED");
    }
    command.args(arguments);
    Err(command.exec().into())
}

fn resolve_real_codex() -> io::Result<PathBuf> {
    resolve_real_harness("BOOMUX_REAL_CODEX", "codex", "Codex")
}

fn resolve_real_harness(
    environment_variable: &str,
    executable_name: &str,
    display_name: &str,
) -> io::Result<PathBuf> {
    if let Some(path) = env::var_os(environment_variable).map(PathBuf::from) {
        let metadata = fs::metadata(&path)?;
        if path.is_absolute() && metadata.is_file() && metadata.permissions().mode() & 0o111 != 0 {
            return Ok(path);
        }
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{environment_variable} must identify an absolute regular executable"),
        ));
    }
    let shim_dir = env::var_os("BOOMUX_OPENCODE_SHIM_DIR").map(PathBuf::from);
    env::split_paths(&env::var_os("PATH").unwrap_or_default())
        .filter(|directory| shim_dir.as_deref() != Some(directory.as_path()))
        .map(|directory| directory.join(executable_name))
        .find(|candidate| {
            fs::metadata(candidate).is_ok_and(|metadata| {
                metadata.is_file() && metadata.permissions().mode() & 0o111 != 0
            })
        })
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                format!("{display_name} executable was not found"),
            )
        })
}

fn kiro_hook_command() -> Result<(), Box<dyn Error>> {
    let holder_id = match env::var("BOOMUX_KIRO_LAUNCH_HOLDER") {
        Ok(holder_id) if !holder_id.is_empty() => holder_id,
        _ => return Ok(()),
    };
    let update = kiro_hooks::read_update(io::stdin().lock())?;
    let client = client::connect_or_start()?;
    let observation = update.observation;
    let report = AgentReport {
        state: observation.state,
        authority: AgentAuthority::LifecycleIntegration,
        evidence: observation.evidence.into(),
        confidence: 100,
    };
    let agent = client.report_kiro_hook(holder_id, update.session_id, report)?;
    for path in update.working_contexts {
        let _ =
            client.observe_agent_working_context(&agent.id, &agent.shell_id, &agent.run_id, path);
    }
    Ok(())
}

fn launch_kiro(arguments: Vec<OsString>) -> Result<process_adapter::ProcessExit, Box<dyn Error>> {
    let executable = resolve_real_kiro()?;
    let explicit_v3 = arguments.first().is_some_and(|argument| argument == "--v3")
        && !arguments.iter().any(|argument| argument == "--cloud");
    let managed_v3 = arguments.is_empty() || explicit_v3;
    let environment = integration_management::Environment::from_process();
    let hooks_current = integration_management::inspect_without_host_probe(
        integration_management::IntegrationId::KIRO,
        &environment,
        None,
    )
    .asset
    .state
        == integration_management::AssetState::Current;
    let argv = kiro_argv(executable, arguments, managed_v3 && hooks_current);
    let mut command = Command::new(&argv[0]);
    sanitize_inherited_opencode_shim(&mut command);
    let holder = if managed_v3 && hooks_current {
        match (
            env::var("BOOMUX_SHELL_ID").ok(),
            env::var("BOOMUX_RUN_ID").ok(),
        ) {
            (Some(shell_id), Some(run_id)) => client::connect_or_start()
                .and_then(|client| {
                    client
                        .acquire_kiro_launch_holder(std::process::id(), shell_id, run_id)
                        .map(|holder_id| (client, holder_id))
                })
                .map_err(|error| {
                    eprintln!("boomux: warning: Kiro lifecycle tracking is unavailable: {error}");
                })
                .ok(),
            _ => None,
        }
    } else {
        None
    };
    if let Some((_, holder_id)) = &holder {
        command.env("BOOMUX_KIRO_LAUNCH_HOLDER", holder_id);
    } else {
        command.env_remove("BOOMUX_KIRO_LAUNCH_HOLDER");
    }
    command.args(&argv[1..]);
    let holder_pid = std::process::id() as libc::pid_t;
    // The child stays in the foreground process group for ordinary terminal
    // signals. A direct holder death also terminates the exact managed child.
    unsafe {
        command.pre_exec(move || {
            if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM) == -1 {
                return Err(io::Error::last_os_error());
            }
            if libc::getppid() != holder_pid {
                libc::raise(libc::SIGTERM);
                libc::_exit(128 + libc::SIGTERM);
            }
            Ok(())
        });
    }
    let status = match command.spawn() {
        Ok(mut child) => child.wait(),
        Err(error) => Err(error),
    };
    if let Some((client, holder_id)) = holder
        && let Err(error) = client.release_kiro_launch_holder(holder_id)
    {
        eprintln!("boomux: warning: Kiro lifecycle release failed: {error}");
    }
    let status = status?;
    Ok(process_exit_from_status(status))
}

fn kiro_argv(executable: PathBuf, arguments: Vec<OsString>, managed_v3: bool) -> Vec<OsString> {
    let mut argv = vec![executable.into_os_string()];
    if managed_v3 && arguments.is_empty() {
        argv.push("--v3".into());
    }
    argv.extend(arguments);
    argv
}

fn resolve_real_kiro() -> io::Result<PathBuf> {
    resolve_real_harness("BOOMUX_REAL_KIRO", "kiro-cli", "Kiro")
}

fn resolve_agent_context(
    shell_id: Option<String>,
    run_id: Option<String>,
    environment_shell_id: Option<String>,
    environment_run_id: Option<String>,
) -> Result<(String, String), Box<dyn Error>> {
    let shell_id = agent_context_value("shell ID", shell_id.or(environment_shell_id))?;
    let run_id = agent_context_value("run ID", run_id.or(environment_run_id))?;
    match (shell_id, run_id) {
        (Some(shell_id), Some(run_id)) => Ok((shell_id, run_id)),
        _ => Err(cli_output::failure(
            "context_required",
            "agent commands require both shell and run identity; pass --shell-id and --run-id or set BOOMUX_SHELL_ID and BOOMUX_RUN_ID",
        )),
    }
}

fn agent_context_value(
    kind: &str,
    value: Option<String>,
) -> Result<Option<String>, Box<dyn Error>> {
    value
        .map(|value| {
            let value = value.trim();
            if value.is_empty() {
                Err(cli_output::failure(
                    "invalid_argument",
                    format!("agent {kind} cannot be empty"),
                ))
            } else {
                Ok(value.to_owned())
            }
        })
        .transpose()
}

fn print_agent(agent: &AgentInstanceSnapshot, workspace_name: &str) {
    println!("ID\t{}", agent.id);
    println!("NAME\t{}", agent.name);
    println!("WORKSPACE\t{workspace_name}");
    println!("SHELL ID\t{}", agent.shell_id);
    println!("RUN ID\t{}", agent.run_id);
    println!("INTEGRATION\t{}", agent.integration);
    println!(
        "EXTERNAL SESSION ID\t{}",
        agent.external_session_id.as_deref().unwrap_or("-")
    );
    println!("STARTED AT MS\t{}", agent.started_at_ms);
    println!(
        "ENDED AT MS\t{}",
        agent
            .ended_at_ms
            .map_or_else(|| "-".into(), |value| value.to_string())
    );
    println!("REVISION\t{}", agent.observation.revision);
    println!(
        "STATE\t{}",
        cli_output::agent_state(agent.observation.state)
    );
    println!(
        "AUTHORITY\t{}",
        cli_output::agent_authority(agent.observation.authority)
    );
    println!("EVIDENCE\t{}", agent.observation.evidence);
    println!("CONFIDENCE\t{}", agent.observation.confidence);
    println!("OBSERVED AT MS\t{}", agent.observation.observed_at_ms);
}

fn list_workspace_shells(json: bool) -> Result<(), Box<dyn Error>> {
    let client = client::connect_or_start()?;
    let snapshot = client.snapshot()?;
    let workspace = local_workspace_argument(&client, None)?;
    let workspace = resolve_context_workspace(&snapshot, workspace.as_deref())?;
    if json {
        let shells = workspace
            .shells
            .iter()
            .map(|shell| cli_output::shell(shell, Some(&workspace.name)))
            .collect::<Vec<_>>();
        return print_json(
            CommandKey::Shells,
            serde_json::json!({
                "workspace_id": workspace.id,
                "workspace_name": workspace.name,
                "shells": shells,
            }),
        );
    }
    println!("NAME\tSHELL ID\tRUN ID\tSTATUS");
    for shell in &workspace.shells {
        println!(
            "{}\t{}\t{}\t{}",
            shell.name,
            shell.id,
            shell.run.as_ref().map_or("-", |run| run.id.as_str()),
            shell_status(&shell.status)
        );
    }
    Ok(())
}

fn read_events(
    after: Option<&str>,
    limit: u16,
    wait_ms: u32,
    json: bool,
) -> Result<(), Box<dyn Error>> {
    let after = after.map(parse_event_cursor).transpose()?;
    let batch = client::connect_or_start()?.events(after, limit, wait_ms)?;
    let cursor = format_event_cursor(&batch.cursor);
    if json {
        let snapshot = batch.snapshot.map(json_snapshot).transpose()?;
        return print_json(
            CommandKey::Events,
            serde_json::json!({
                "stream_id": batch.stream_id,
                "cursor": cursor,
                "snapshot": snapshot,
                "events": batch.events,
            }),
        );
    }
    println!("CURSOR\t{cursor}");
    if let Some(snapshot) = batch.snapshot {
        println!("SNAPSHOT\t{}", snapshot.workspaces.len());
    }
    for event in batch.events {
        let value = serde_json::to_value(&event.kind)?;
        println!(
            "{}\t{}\t{}",
            event.id,
            event.at_ms,
            value["event"].as_str().unwrap_or("unknown")
        );
    }
    Ok(())
}

fn parse_event_cursor(value: &str) -> Result<EventCursor, Box<dyn Error>> {
    let (stream_id, event_id) = value.split_once(':').ok_or_else(|| {
        cli_output::failure(
            "invalid_argument",
            "event cursor must have the form <stream-uuid>:<event-id>",
        )
    })?;
    Uuid::parse_str(stream_id).map_err(|_| {
        cli_output::failure("invalid_argument", "event cursor stream ID is invalid")
    })?;
    let event_id = event_id
        .parse::<u64>()
        .map_err(|_| cli_output::failure("invalid_argument", "event cursor event ID is invalid"))?;
    Ok(EventCursor {
        stream_id: stream_id.into(),
        event_id,
    })
}

fn format_event_cursor(cursor: &EventCursor) -> String {
    format!("{}:{}", cursor.stream_id, cursor.event_id)
}

fn json_snapshot(snapshot: Snapshot) -> Result<serde_json::Value, Box<dyn Error>> {
    let workspaces = snapshot
        .workspaces
        .iter()
        .map(|workspace| {
            let shells = workspace
                .shells
                .iter()
                .map(|shell| cli_output::shell(shell, Some(&workspace.name)))
                .collect::<Vec<_>>();
            let launchers = workspace
                .launchers
                .iter()
                .map(|launcher| cli_output::launcher(launcher, Some(&workspace.name)))
                .collect::<Vec<_>>();
            let agents = workspace
                .agents
                .iter()
                .map(|agent| cli_output::agent(agent, Some(&workspace.name)))
                .collect::<Vec<_>>();
            serde_json::json!({
                "id": workspace.id,
                "name": workspace.name,
                "shells": shells,
                "launchers": launchers,
                "agents": agents,
            })
        })
        .collect::<Vec<_>>();
    Ok(serde_json::json!({ "workspaces": workspaces }))
}

fn read_shell(
    target: &str,
    lines: u32,
    json: bool,
    run_id: Option<&str>,
    after_revision: Option<u64>,
    wait_ms: u32,
) -> Result<(), Box<dyn Error>> {
    let client = client::connect_or_start()?;
    let snapshot = client.snapshot()?;
    let workspace =
        resource_workspace_argument(&client, None, find_shell(&snapshot, target).is_some())?;
    let shell = resolve_cli_shell(&snapshot, target, workspace.as_deref())?;
    if json || run_id.is_some() {
        let state = client.read_shell_at(
            &shell.id,
            READ_BYTES,
            run_id.map(str::to_owned),
            after_revision,
            wait_ms,
        )?;
        let output = recent_lines(&String::from_utf8_lossy(&state.bytes), lines as usize);
        if json {
            return print_json(
                CommandKey::Read,
                serde_json::json!({
                    "shell_id": shell.id,
                    "run_id": state.run_id,
                    "output_revision": state.output_revision,
                    "changed": state.changed,
                    "status": shell_status(&state.status),
                    "output": output,
                }),
            );
        }
        print_output(&output);
        return Ok(());
    }
    let bytes = client.read_shell(&shell.id, READ_BYTES)?;
    let output = recent_lines(&String::from_utf8_lossy(&bytes), lines as usize);
    print_output(&output);
    Ok(())
}

fn print_output(output: &str) {
    print!("{output}");
    if !output.is_empty() && !output.ends_with('\n') {
        println!();
    }
}

fn close_shell(target: &str) -> Result<(), Box<dyn Error>> {
    let client = client::connect_or_start()?;
    close_shell_with_workspace(&client, target, None)
}

fn close_focused_shell() -> Result<(), Box<dyn Error>> {
    let client = client::connect_or_start()?;
    let message = close_focused_shell_with_client(&client).map_err(io::Error::other)?;
    println!("{message}");
    Ok(())
}

fn close_focused_shell_with_client(client: &client::Client) -> Result<String, String> {
    if !client
        .supports(protocol::ProtocolFeature::QualifiedFocusedTerminal)
        .map_err(|error| error.to_string())?
    {
        return Err("focused Shell close requires daemon protocol 39 or newer".into());
    }
    let combined = client
        .combined_node_snapshot(None)
        .map_err(|error| error.to_string())?;
    let identity = combined
        .focused_terminal
        .as_ref()
        .ok_or_else(|| "no Boomux terminal has reported focus; focus one and retry".to_owned())?
        .shell
        .clone();
    close_qualified_shell_with_client(client, &combined, &identity, None)
}

fn close_qualified_shell_with_client(
    client: &client::Client,
    combined: &protocol::CombinedNodeSnapshot,
    identity: &protocol::QualifiedIdentity,
    desktop_context: Option<(&str, &hyprland::ActiveShellIdentity)>,
) -> Result<String, String> {
    let local_node_id = combined
        .nodes
        .iter()
        .find(|node| node.local)
        .map(|node| node.node_id.clone())
        .ok_or_else(|| "local Node identity is unavailable".to_owned())?;
    let local = identity.node_id == local_node_id;
    let shell = routed_dashboard_shell(client, identity, &local_node_id)?;
    if local && env::var("BOOMUX_SHELL_ID").ok().as_deref() == Some(shell.id.as_str()) {
        return Err(
            "cannot close the current shell from inside it; use the dashboard or another shell"
                .into(),
        );
    }
    let workspace = routed_dashboard_workspace(
        client,
        &protocol::QualifiedIdentity::new(&identity.node_id, &shell.workspace_id),
        &local_node_id,
    )?;
    if let Some((global_workspace_id, active_window)) = desktop_context {
        let global = find_global_workspace_by_id(&combined.workspaces, global_workspace_id)
            .ok_or_else(|| "visible Boomux Workspace is unavailable".to_owned())?;
        if !global.placements.iter().any(|placement| {
            placement.node_id == identity.node_id
                && placement.workspace_id == shell.workspace_id
                && placement.state == protocol::WorkspacePlacementState::Active
        }) {
            return Err("active terminal does not belong to the visible Boomux Workspace".into());
        }
        hyprland::revalidate_boomux_shell_window(active_window, global_workspace_id)
            .map_err(|error| error.to_string())?;
    }
    if local {
        client
            .close_shell(&shell.id)
            .map_err(|error| error.to_string())?;
    } else {
        match routed_dashboard_operation(
            client,
            identity,
            &local_node_id,
            protocol::RoutedOperation::CloseShell {
                shell_id: shell.id.clone(),
                expected_revision: shell.revision,
            },
        )? {
            protocol::RoutedOperationResult::Ok => {}
            _ => return Err("remote Node returned an unexpected shell close response".into()),
        }
    }
    Ok(format!(
        "Closed focused shell {} from {}",
        shell.name, workspace.name
    ))
}

fn close_shell_with_workspace(
    client: &client::Client,
    target: &str,
    workspace: Option<&str>,
) -> Result<(), Box<dyn Error>> {
    let snapshot = client.snapshot()?;
    let current_shell_id = env::var("BOOMUX_SHELL_ID").ok();
    let workspace =
        resource_workspace_argument(client, workspace, find_shell(&snapshot, target).is_some())?;
    let shell = resolve_cli_shell(&snapshot, target, workspace.as_deref())?;
    if current_shell_id.as_deref() == Some(shell.id.as_str()) {
        return Err(
            "cannot close the current shell from inside it; use the dashboard or another shell"
                .into(),
        );
    }
    let workspace_name = snapshot
        .workspaces
        .iter()
        .find(|workspace| workspace.id == shell.workspace_id)
        .map(|workspace| workspace.name.as_str())
        .unwrap_or("workspace");
    client.close_shell(&shell.id)?;
    println!("Closed shell {} from {workspace_name}", shell.name);
    Ok(())
}

fn resolve_cli_shell<'a>(
    snapshot: &'a Snapshot,
    target: &str,
    workspace: Option<&str>,
) -> Result<&'a ShellSnapshot, Box<dyn Error>> {
    if let Some(shell) = find_shell(snapshot, target) {
        return Ok(shell);
    }
    let workspace = resolve_context_workspace(snapshot, workspace)?;
    resolve_shell_target(snapshot, Some(&workspace.id), target)
}

fn resolve_cli_launcher<'a>(
    snapshot: &'a Snapshot,
    target: &str,
    workspace: Option<&str>,
) -> Result<&'a WorkspaceLauncherSnapshot, Box<dyn Error>> {
    if let Some(launcher) = find_launcher(snapshot, target) {
        return Ok(launcher);
    }
    let workspace = resolve_context_workspace(snapshot, workspace)?;
    workspace
        .launchers
        .iter()
        .find(|launcher| launcher.name == target)
        .ok_or_else(|| {
            let available = workspace
                .launchers
                .iter()
                .map(|launcher| launcher.name.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            cli_output::failure(
                "not_found",
                format!(
                    "launcher {target:?} was not found in this workspace; available launchers: {available}"
                ),
            )
        })
}

fn resolve_shell_target<'a>(
    snapshot: &'a Snapshot,
    current_workspace_id: Option<&str>,
    target: &str,
) -> Result<&'a ShellSnapshot, Box<dyn Error>> {
    if let Some(shell) = find_shell(snapshot, target) {
        return Ok(shell);
    }
    let workspace_id = current_workspace_id.ok_or_else(|| {
        cli_output::failure(
            "context_required",
            format!(
                "shell name {target:?} requires a Boomux shell; use an exact shell ID outside Boomux"
            ),
        )
    })?;
    let workspace = snapshot
        .workspaces
        .iter()
        .find(|workspace| workspace.id == workspace_id)
        .ok_or_else(|| cli_output::failure("not_found", "current workspace no longer exists"))?;
    let matches = workspace
        .shells
        .iter()
        .filter(|shell| shell.name == target)
        .collect::<Vec<_>>();
    match matches.as_slice() {
        [shell] => Ok(shell),
        [] => {
            let available = workspace
                .shells
                .iter()
                .map(|shell| shell.name.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            Err(cli_output::failure(
                "not_found",
                format!(
                    "shell {target:?} was not found in this workspace; available shells: {available}"
                ),
            ))
        }
        _ => Err(cli_output::failure(
            "ambiguous_target",
            format!("shell name {target:?} is ambiguous in this workspace"),
        )),
    }
}

fn recent_lines(text: &str, count: usize) -> String {
    let lines = text.split_inclusive('\n').collect::<Vec<_>>();
    lines[lines.len().saturating_sub(count)..].concat()
}

fn find_shell<'a>(snapshot: &'a Snapshot, id: &str) -> Option<&'a ShellSnapshot> {
    snapshot
        .workspaces
        .iter()
        .flat_map(|workspace| &workspace.shells)
        .find(|shell| shell.id == id)
}

fn find_launcher<'a>(snapshot: &'a Snapshot, id: &str) -> Option<&'a WorkspaceLauncherSnapshot> {
    snapshot
        .workspaces
        .iter()
        .flat_map(|workspace| &workspace.launchers)
        .find(|launcher| launcher.id == id)
}

fn shell_status(status: &ShellStatus) -> &'static str {
    match status {
        ShellStatus::Pending => "pending",
        ShellStatus::Running => "running",
        ShellStatus::Exited { .. } => "exited",
    }
}

fn shell_exit_reason(reason: &boomux::protocol::ShellRunExitReason) -> String {
    match reason {
        boomux::protocol::ShellRunExitReason::Exited { code: Some(code) } => {
            format!("exited ({code})")
        }
        boomux::protocol::ShellRunExitReason::Exited { code: None } => {
            "exited (code unavailable)".into()
        }
        boomux::protocol::ShellRunExitReason::Terminated => "terminated".into(),
        boomux::protocol::ShellRunExitReason::Interrupted => "interrupted".into(),
    }
}

fn open_dashboard_shell(
    client: &client::Client,
    shell_id: &str,
    terminal: Option<&str>,
) -> Result<String, Box<dyn Error>> {
    let shell = client.get_shell(shell_id)?;
    let workspace = client.get_workspace(&shell.workspace_id)?;
    let title = format!("{} - {}", workspace.name, shell.name);
    let placement = if hyprland_special_workspaces_enabled()? {
        let combined = client.combined_node_snapshot(None)?;
        let local_node_id = combined
            .nodes
            .iter()
            .find(|node| node.local)
            .map(|node| node.node_id.as_str())
            .ok_or_else(|| cli_output::failure("internal", "local Node is unavailable"))?;
        coordinated_workspace_for_owner(&combined.workspaces, local_node_id, &shell.workspace_id)?
            .cloned()
            .map(|global| (global, local_node_id.to_owned()))
    } else {
        None
    };
    if let Some((global, local_node_id)) = placement {
        show_desktop_workspace(&global)?;
        terminal::open_placed(
            terminal,
            shell_id,
            &title,
            true,
            terminal::HyprlandPlacement {
                workspace_id: &global.id,
                node_id: &local_node_id,
                shell_id,
            },
        )?;
    } else {
        open_terminal(shell_id, &title, true, terminal)?;
    }
    Ok(format!("Opened {} from {}", shell.name, workspace.name))
}

fn open_dashboard_remote_shell(
    client: &client::Client,
    identity: &protocol::QualifiedIdentity,
    terminal: Option<&str>,
) -> Result<String, Box<dyn Error>> {
    let registration = client.node_registration(&identity.node_id)?;
    let shell = routed_dashboard_shell(client, identity, "").map_err(io::Error::other)?;
    let workspace = match client.route_node_operation(
        &identity.node_id,
        protocol::RoutedOperation::GetWorkspace {
            workspace_id: shell.workspace_id.clone(),
        },
    )? {
        protocol::RoutedOperationResult::Workspace { workspace } => workspace,
        _ => return Err("remote Node returned an unexpected workspace response".into()),
    };
    let title = format!(
        "[{}] {} - {}",
        registration.alias, workspace.name, shell.name
    );
    let placement = if hyprland_special_workspaces_enabled()? {
        let combined = client.combined_node_snapshot(None)?;
        coordinated_workspace_for_owner(
            &combined.workspaces,
            &identity.node_id,
            &shell.workspace_id,
        )?
        .cloned()
    } else {
        None
    };
    if let Some(global) = placement {
        show_desktop_workspace(&global)?;
        terminal::open_remote_placed(
            terminal,
            &identity.node_id,
            &identity.inner_id,
            &title,
            true,
            &global.id,
        )?;
    } else {
        terminal::open_remote(
            terminal,
            &identity.node_id,
            &identity.inner_id,
            &title,
            true,
        )?;
    }
    Ok(format!(
        "Opened {} from {} on Node {}",
        shell.name, workspace.name, registration.alias
    ))
}

fn coordinated_workspace_for_owner<'a>(
    workspaces: &'a [protocol::GlobalWorkspaceSnapshot],
    node_id: &str,
    owner_workspace_id: &str,
) -> Result<Option<&'a protocol::GlobalWorkspaceSnapshot>, Box<dyn Error>> {
    let mut matching = workspaces.iter().filter(|workspace| {
        !workspace.closing
            && workspace.placements.iter().any(|placement| {
                placement.node_id == node_id
                    && placement.workspace_id == owner_workspace_id
                    && placement.state == protocol::WorkspacePlacementState::Active
            })
    });
    let workspace = matching.next();
    if matching.next().is_some() {
        return Err(cli_output::failure(
            "internal",
            "the owner Workspace belongs to more than one coordinated Workspace",
        ));
    }
    Ok(workspace)
}

fn open_workspace(
    workspace: &WorkspaceSnapshot,
    terminal: Option<&str>,
    desktop_placement: Option<(&str, &str)>,
    invoke_launchers: bool,
) -> Result<(), Box<dyn Error>> {
    let outcome = attempt_open_workspace(workspace, terminal, desktop_placement, invoke_launchers)?;
    if !outcome.failures.is_empty() {
        return Err(io::Error::other(format!(
            "workspace {} opened with failures: {}",
            workspace.name,
            outcome.failures.join("; ")
        ))
        .into());
    }
    Ok(())
}

struct LocalWorkspaceOpenOutcome {
    opened_launchers: usize,
    opened_shells: usize,
    failures: Vec<String>,
}

fn attempt_open_workspace(
    workspace: &WorkspaceSnapshot,
    terminal: Option<&str>,
    desktop_placement: Option<(&str, &str)>,
    invoke_launchers: bool,
) -> Result<LocalWorkspaceOpenOutcome, Box<dyn Error>> {
    if workspace_user_shell_count(workspace) == 0 {
        if !invoke_launchers {
            return Err(
                format!("workspace {} has no Shell terminal windows", workspace.name).into(),
            );
        }
        if workspace.launchers.is_empty() {
            return Err(format!("workspace {} has no shells or launchers", workspace.name).into());
        }
    }
    let mut opened_launchers = 0;
    let mut opened_shells = 0;
    let mut failures = Vec::new();
    if invoke_launchers {
        for launcher in &workspace.launchers {
            match invoke_workspace_launcher(workspace, launcher) {
                Ok(()) => opened_launchers += 1,
                Err(error) => failures.push(format!("launcher {}: {error}", launcher.name)),
            }
        }
    }
    for shell in &workspace.shells {
        let opened = if let Some((global_workspace_id, node_id)) = desktop_placement {
            terminal::open_placed(
                terminal,
                &shell.id,
                &format!("{} - {}", workspace.name, shell.name),
                true,
                terminal::HyprlandPlacement {
                    workspace_id: global_workspace_id,
                    node_id,
                    shell_id: &shell.id,
                },
            )
        } else {
            open_terminal(
                &shell.id,
                &format!("{} - {}", workspace.name, shell.name),
                true,
                terminal,
            )
        };
        match opened {
            Ok(()) => opened_shells += 1,
            Err(error) => failures.push(format!("shell {}: {error}", shell.name)),
        }
    }
    Ok(LocalWorkspaceOpenOutcome {
        opened_launchers,
        opened_shells,
        failures,
    })
}

fn workspace_user_shell_count(workspace: &WorkspaceSnapshot) -> usize {
    workspace.shells.len()
}

fn invoke_workspace_launcher(
    workspace: &WorkspaceSnapshot,
    launcher: &WorkspaceLauncherSnapshot,
) -> io::Result<()> {
    let (executable, arguments) = launcher
        .command
        .split_first()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "launcher command is empty"))?;
    let mut command = Command::new(executable);
    sanitize_inherited_opencode_shim(&mut command);
    command
        .args(arguments)
        .current_dir(&launcher.cwd)
        .env("BOOMUX_WORKSPACE_ID", &workspace.id)
        .env("BOOMUX_WORKSPACE", &workspace.name)
        .env("BOOMUX_LAUNCHER_ID", &launcher.id)
        .env("BOOMUX_LAUNCHER_NAME", &launcher.name)
        .env_remove("BOOMUX_SHELL_ID")
        .env_remove("BOOMUX_SHELL_NAME")
        .env_remove("BOOMUX_RUN_ID")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    // The child has not executed user code yet; `setsid` detaches it from the
    // invoking terminal while preserving the client's desktop environment.
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                Err(io::Error::last_os_error())
            } else {
                Ok(())
            }
        });
    }
    let mut child = command.spawn()?;
    thread::Builder::new()
        .name(format!("launcher-reaper-{}", launcher.id))
        .spawn(move || {
            let _ = child.wait();
        })
        .map_err(|error| io::Error::other(format!("could not start launcher reaper: {error}")))?;
    Ok(())
}

fn sanitize_inherited_opencode_shim(command: &mut Command) {
    let shim_dir = env::var_os("BOOMUX_OPENCODE_SHIM_DIR");
    let original_path = env::var_os("BOOMUX_ORIGINAL_PATH");
    let private_tui_config = env::var_os("BOOMUX_OPENCODE_TUI_CONFIG");
    if let Some(original_path) = original_path {
        command.env("PATH", original_path);
    } else if let (Some(path), Some(shim_dir)) = (env::var_os("PATH"), shim_dir.as_deref()) {
        let filtered = env::split_paths(&path)
            .filter(|entry| entry.as_os_str() != shim_dir)
            .collect::<Vec<_>>();
        if let Ok(path) = env::join_paths(filtered) {
            command.env("PATH", path);
        }
    }
    for name in [
        "BOOMUX_REAL_OPENCODE",
        "BOOMUX_REAL_CODEX",
        "BOOMUX_REAL_KIRO",
        "BOOMUX_CODEX_RUN_SCOPED",
        "BOOMUX_KIRO_RUN_SCOPED",
        "BOOMUX_KIRO_LAUNCH_HOLDER",
        "BOOMUX_ORIGINAL_PATH",
        "BOOMUX_OPENCODE_SHIM_DIR",
        "BOOMUX_OPENCODE_TUI_CONFIG",
        "BOOMUX_SHIM_EXECUTABLE",
        "BOOMUX_OPENCODE_SHARED_GENERATION",
        "BOOMUX_OPENCODE_CLAIM_HOLDER",
    ] {
        command.env_remove(name);
    }
    if private_tui_config.is_some()
        && env::var_os("OPENCODE_TUI_CONFIG").as_ref() == private_tui_config.as_ref()
    {
        command.env_remove("OPENCODE_TUI_CONFIG");
    }
}

fn open_shell(
    shell_id: &str,
    node: Option<&str>,
    coordinated_workspace: Option<&str>,
    title: Option<&str>,
    takeover: bool,
    terminal: Option<&str>,
) -> Result<(), Box<dyn Error>> {
    let client = client::connect_or_start()?;
    if let Some(selector) = node {
        let registration = client.node_registration(selector)?;
        let identity = protocol::QualifiedIdentity::new(&registration.node_id, shell_id);
        let shell = routed_dashboard_shell(&client, &identity, "").map_err(io::Error::other)?;
        let workspace = match client.route_node_operation(
            &registration.node_id,
            protocol::RoutedOperation::GetWorkspace {
                workspace_id: shell.workspace_id.clone(),
            },
        )? {
            protocol::RoutedOperationResult::Workspace { workspace } => workspace,
            _ => return Err("remote Node returned an unexpected workspace response".into()),
        };
        let title = title.map(str::to_owned).unwrap_or_else(|| {
            format!(
                "[{}] {} - {}",
                registration.alias, workspace.name, shell.name
            )
        });
        let placement = coordinated_workspace
            .map(|target| {
                prepare_shell_desktop_placement(
                    &client,
                    target,
                    &registration.node_id,
                    &shell.workspace_id,
                )
            })
            .transpose()?
            .flatten();
        return if let Some(workspace_id) = placement {
            terminal::open_remote_placed(
                terminal,
                &registration.node_id,
                shell_id,
                &title,
                takeover,
                &workspace_id,
            )
        } else {
            terminal::open_remote(terminal, &registration.node_id, shell_id, &title, takeover)
        };
    }
    let shell = client.get_shell(shell_id)?;
    let title = title.map(str::to_owned).unwrap_or_else(|| {
        client
            .get_workspace(&shell.workspace_id)
            .map(|workspace| format!("{} - {}", workspace.name, shell.name))
            .unwrap_or_else(|_| format!("Boomux: {}", shell.name))
    });
    let (placement, local_node_id) = if let Some(target) = coordinated_workspace {
        let combined = client.combined_node_snapshot(None)?;
        let local_node_id = combined
            .nodes
            .iter()
            .find(|node| node.local)
            .map(|node| node.node_id.clone())
            .ok_or_else(|| cli_output::failure("internal", "local Node is unavailable"))?;
        let placement = prepare_shell_desktop_placement_from_snapshot(
            target,
            &local_node_id,
            &shell.workspace_id,
            &combined.workspaces,
        )?;
        (placement, local_node_id)
    } else {
        (None, String::new())
    };
    if let Some(workspace_id) = placement {
        terminal::open_placed(
            terminal,
            shell_id,
            &title,
            takeover,
            terminal::HyprlandPlacement {
                workspace_id: &workspace_id,
                node_id: &local_node_id,
                shell_id,
            },
        )
    } else {
        open_terminal(shell_id, &title, takeover, terminal)
    }
}

fn prepare_shell_desktop_placement(
    client: &client::Client,
    target: &str,
    node_id: &str,
    owner_workspace_id: &str,
) -> Result<Option<String>, Box<dyn Error>> {
    let combined = client.combined_node_snapshot(None)?;
    prepare_shell_desktop_placement_from_snapshot(
        target,
        node_id,
        owner_workspace_id,
        &combined.workspaces,
    )
}

fn prepare_session_desktop_placement(
    client: &client::Client,
    target: &str,
) -> Result<Option<String>, Box<dyn Error>> {
    let combined = client.combined_node_snapshot(None)?;
    let workspace =
        resolve_global_workspace_target(&combined.workspaces, target).ok_or_else(|| {
            cli_output::failure(
                "not_found",
                format!("coordinated Workspace not found: {target}"),
            )
        })?;
    if workspace.closing {
        return Err(cli_output::failure(
            "invalid_argument",
            "the selected coordinated Workspace is closing",
        ));
    }
    if !hyprland_special_workspaces_enabled()? {
        return Ok(None);
    }
    if hyprland::active_boomux_workspace()?.as_deref() != Some(workspace.id.as_str()) {
        hyprland::toggle_special(&workspace.id)?;
    }
    select_desktop_workspace(workspace)?;
    Ok(Some(workspace.id.clone()))
}

fn prepare_shell_desktop_placement_from_snapshot(
    target: &str,
    node_id: &str,
    owner_workspace_id: &str,
    workspaces: &[protocol::GlobalWorkspaceSnapshot],
) -> Result<Option<String>, Box<dyn Error>> {
    let workspace =
        resolve_shell_desktop_workspace(target, node_id, owner_workspace_id, workspaces)?;
    if !hyprland_special_workspaces_enabled()? {
        return Ok(None);
    }
    if hyprland::active_boomux_workspace()?.as_deref() != Some(workspace.id.as_str()) {
        hyprland::toggle_special(&workspace.id)?;
    }
    select_desktop_workspace(workspace)?;
    Ok(Some(workspace.id.clone()))
}

fn resolve_shell_desktop_workspace<'a>(
    target: &str,
    node_id: &str,
    owner_workspace_id: &str,
    workspaces: &'a [protocol::GlobalWorkspaceSnapshot],
) -> Result<&'a protocol::GlobalWorkspaceSnapshot, Box<dyn Error>> {
    let workspace = resolve_global_workspace_target(workspaces, target).ok_or_else(|| {
        cli_output::failure(
            "not_found",
            format!("coordinated Workspace not found: {target}"),
        )
    })?;
    if workspace.closing
        || !workspace.placements.iter().any(|placement| {
            placement.node_id == node_id
                && placement.workspace_id == owner_workspace_id
                && placement.state == protocol::WorkspacePlacementState::Active
        })
    {
        return Err(cli_output::failure(
            "invalid_argument",
            "the Shell does not belong to an active placement in that coordinated Workspace",
        ));
    }
    Ok(workspace)
}

fn open_terminal(
    shell_id: &str,
    title: &str,
    takeover: bool,
    terminal: Option<&str>,
) -> Result<(), Box<dyn Error>> {
    terminal::open(terminal, shell_id, title, takeover)
}

fn print_prompt_label() -> Result<(), Box<dyn Error>> {
    let Some(shell_id) = env::var_os("BOOMUX_SHELL_ID").and_then(|value| value.into_string().ok())
    else {
        return Ok(());
    };
    let client = client::connect_or_start()?;
    let shell = client.get_shell(shell_id)?;
    if let Ok(workspace) = client.get_workspace(&shell.workspace_id) {
        println!("{}/{}", workspace.name, shell.name);
    } else {
        println!("{}", shell.name);
    }
    Ok(())
}

fn install_skill(force: bool) -> Result<(), Box<dyn Error>> {
    let home = env::var_os("HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .ok_or("HOME must be an absolute path to install the Boomux skill")?;
    install_skill_at(&home, force)
}

fn install_skill_at(home: &Path, force: bool) -> Result<(), Box<dyn Error>> {
    require_absolute_root(home, "HOME")?;
    preflight_legacy_skill_migration(home)?;
    let directory = ensure_safe_directory(&home.join(".agents/skills/boomux"))?;
    let path = skill_install_path(home);
    let outcome = install_asset_at(&directory, &path, BOOMUX_SKILL, force)?;

    if outcome == InstallOutcome::Unchanged {
        println!("Boomux skill is already installed at {}", path.display());
    } else {
        println!("Installed Boomux skill at {}", path.display());
    }
    migrate_legacy_skill(home)?;
    Ok(())
}

fn preflight_legacy_skill_migration(home: &Path) -> Result<(), Box<dyn Error>> {
    let directory = home.join(".agents/skills/boomux-shells");
    match fs::symlink_metadata(&directory) {
        Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {}
        Ok(_) => {
            return Err(format!(
                "legacy Boomux skill directory is not a regular directory: {}",
                directory.display()
            )
            .into());
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    }
    let path = legacy_skill_install_path(home);
    if let Some(content_matches) = regular_file_matches(&path, LEGACY_BOOMUX_SHELLS_SKILL)? {
        let entries = fs::read_dir(&directory)?.collect::<Result<Vec<_>, _>>()?;
        let _untouched =
            content_matches && entries.len() == 1 && entries[0].file_name() == "SKILL.md";
    }
    Ok(())
}

pub(crate) fn migrate_legacy_skill(home: &Path) -> Result<(), Box<dyn Error>> {
    preflight_legacy_skill_migration(home)?;
    let directory = home.join(".agents/skills/boomux-shells");
    if !directory.exists() {
        return Ok(());
    }
    let path = legacy_skill_install_path(home);
    if let Some(content_matches) = regular_file_matches(&path, LEGACY_BOOMUX_SHELLS_SKILL)? {
        let entries = fs::read_dir(&directory)?.collect::<Result<Vec<_>, _>>()?;
        let untouched =
            content_matches && entries.len() == 1 && entries[0].file_name() == "SKILL.md";
        if untouched {
            fs::remove_file(&path)?;
            if let Some(directory) = path.parent() {
                let _ = fs::remove_dir(directory);
            }
            println!("Removed legacy Boomux shell skill at {}", path.display());
        } else {
            eprintln!(
                "warning: preserved customized legacy Boomux skill at {}; remove it to avoid duplicate guidance",
                path.display()
            );
        }
    }
    Ok(())
}

fn install_opencode(force: bool) -> Result<(), Box<dyn Error>> {
    install_integrations(
        &[integration_management::IntegrationId::Opencode],
        force,
        false,
        false,
    )
}

fn opencode_claim_command(
    command: OpenCodeClaimCommands,
    json: bool,
) -> Result<(), Box<dyn Error>> {
    let client = client::connect_or_start()?;
    match command {
        OpenCodeClaimCommands::Ensure {
            generation,
            holder,
            root_session_id,
            shell_id,
            run_id,
        } => {
            let (shell_id, run_id) = resolve_agent_context(
                shell_id,
                run_id,
                env::var("BOOMUX_SHELL_ID").ok(),
                env::var("BOOMUX_RUN_ID").ok(),
            )?;
            let spec = opencode_claim_registration(&root_session_id);
            let (claim, agent) = client.ensure_opencode_session_claim(
                generation,
                holder,
                root_session_id,
                shell_id,
                run_id,
                spec,
            )?;
            if json {
                let workspace = client.get_workspace(&agent.workspace_id)?;
                return print_json(
                    CommandKey::OpencodeClaimEnsure,
                    serde_json::json!({
                        "claim": claim,
                        "agent": cli_output::agent(&agent, Some(&workspace.name)),
                    }),
                );
            }
            println!("CLAIM ID\t{}", claim.claim_id);
            println!("AGENT ID\t{}", agent.id);
        }
        OpenCodeClaimCommands::Release {
            generation,
            holder,
            claim_id,
        } => {
            let released =
                client.release_opencode_session_claim(generation, holder, claim_id.clone())?;
            if json {
                return print_json(
                    CommandKey::OpencodeClaimRelease,
                    serde_json::json!({ "claim_id": claim_id, "released": released }),
                );
            }
            println!("RELEASED\t{released}");
        }
        OpenCodeClaimCommands::Report {
            generation,
            root_session_id,
            state,
            authority,
            evidence,
            confidence,
        } => {
            let agent = client.report_claimed_opencode_agent(
                generation,
                root_session_id,
                AgentReport {
                    state: state.into(),
                    authority: authority.into(),
                    evidence,
                    confidence,
                },
            )?;
            if json {
                let workspace = client.get_workspace(&agent.workspace_id)?;
                return print_json(
                    CommandKey::OpencodeClaimReport,
                    serde_json::json!({
                        "agent": cli_output::agent(&agent, Some(&workspace.name)),
                    }),
                );
            }
            println!("AGENT ID\t{}", agent.id);
        }
    }
    Ok(())
}

fn opencode_claim_registration(root_session_id: &str) -> AgentRegistrationSpec {
    AgentRegistrationSpec {
        name: "OpenCode".into(),
        integration: "opencode".into(),
        external_session_id: Some(root_session_id.into()),
        report: AgentReport {
            state: AgentState::Unknown,
            authority: AgentAuthority::LifecycleIntegration,
            evidence: "direct TUI selection".into(),
            confidence: 100,
        },
    }
}

#[derive(Debug, PartialEq, Eq)]
struct SharedOpenCodeLaunch {
    executable: PathBuf,
    url: String,
    cwd: PathBuf,
    generation: String,
    holder: String,
    tui_config: PathBuf,
    original_path: Option<OsString>,
    session: Option<String>,
}

struct SharedOpenCodeLaunchInput {
    shell_id: Option<OsString>,
    run_id: Option<OsString>,
    executable: Option<OsString>,
    tui_config: Option<OsString>,
    original_path: Option<OsString>,
    cwd: PathBuf,
    session: Option<String>,
    runtime: protocol::OpenCodeSharedRuntimeSnapshot,
}

impl SharedOpenCodeLaunch {
    fn argv(&self) -> Vec<OsString> {
        let mut argv = vec![
            "attach".into(),
            self.url.clone().into(),
            "--dir".into(),
            self.cwd.clone().into_os_string(),
        ];
        if let Some(session) = &self.session {
            argv.extend(["--session".into(), session.into()]);
        }
        argv
    }
}

fn shared_opencode_launch(
    input: SharedOpenCodeLaunchInput,
) -> Result<SharedOpenCodeLaunch, Box<dyn Error>> {
    let SharedOpenCodeLaunchInput {
        shell_id,
        run_id,
        executable,
        tui_config,
        original_path,
        cwd,
        session,
        runtime,
    } = input;
    for (name, value) in [("BOOMUX_SHELL_ID", shell_id), ("BOOMUX_RUN_ID", run_id)] {
        if value.as_ref().is_none_or(|value| value.is_empty()) {
            return Err(format!("{name} is required").into());
        }
    }
    let executable = PathBuf::from(executable.ok_or("BOOMUX_REAL_OPENCODE is required")?);
    let tui_config = PathBuf::from(tui_config.ok_or("BOOMUX_OPENCODE_TUI_CONFIG is required")?);
    for (name, path, executable_required) in [
        ("BOOMUX_REAL_OPENCODE", &executable, true),
        ("BOOMUX_OPENCODE_TUI_CONFIG", &tui_config, false),
    ] {
        if !path.is_absolute() {
            return Err(format!("{name} must be absolute").into());
        }
        let metadata = fs::metadata(path)?;
        if !metadata.is_file()
            || (executable_required && metadata.permissions().mode() & 0o111 == 0)
        {
            return Err(format!(
                "{name} must name a regular{} file",
                if executable_required {
                    " executable"
                } else {
                    ""
                }
            )
            .into());
        }
    }
    if !cwd.is_absolute() {
        return Err("current working directory must be absolute".into());
    }
    if session.as_deref().is_some_and(str::is_empty) {
        return Err("OpenCode Session ID must not be empty".into());
    }
    Ok(SharedOpenCodeLaunch {
        executable,
        url: runtime.url,
        cwd,
        generation: runtime.generation_id,
        holder: Uuid::new_v4().to_string(),
        tui_config,
        original_path,
        session,
    })
}

fn launch_standalone_recovered_opencode(
    session: &str,
    shared_error: &dyn std::fmt::Display,
) -> Result<(), Box<dyn Error>> {
    if session.is_empty() {
        return Err("OpenCode Session ID must not be empty".into());
    }
    let executable = env::var_os("BOOMUX_REAL_OPENCODE")
        .ok_or("BOOMUX_REAL_OPENCODE is required for standalone recovery")?;
    eprintln!(
        "boomux: shared OpenCode recovery is unavailable ({shared_error}); continuing standalone"
    );
    let mut command = Command::new(executable);
    command
        .args(["--session", session])
        .env_remove("BOOMUX_OPENCODE_SHARED_GENERATION")
        .env_remove("BOOMUX_OPENCODE_CLAIM_HOLDER")
        .env_remove("BOOMUX_REAL_OPENCODE")
        .env_remove("BOOMUX_ORIGINAL_PATH")
        .env_remove("BOOMUX_OPENCODE_SHIM_DIR")
        .env_remove("BOOMUX_OPENCODE_TUI_CONFIG")
        .env_remove("BOOMUX_SHIM_EXECUTABLE")
        .env_remove("BOOMUX_REAL_CLAUDE")
        .env_remove("BOOMUX_CLAUDE_REMOTE_CONTROL")
        .env_remove("BOOMUX_USER_ZDOTDIR");
    if let Some(path) = env::var_os("BOOMUX_ORIGINAL_PATH") {
        command.env("PATH", path);
    }
    Err(command.exec().into())
}

fn launch_shared_opencode(session: Option<&str>, port: u16) -> Result<(), Box<dyn Error>> {
    let shell_id = env::var_os("BOOMUX_SHELL_ID");
    let run_id = env::var_os("BOOMUX_RUN_ID");
    for (name, value) in [
        ("BOOMUX_SHELL_ID", shell_id.as_ref()),
        ("BOOMUX_RUN_ID", run_id.as_ref()),
    ] {
        if value.is_none_or(|value| value.is_empty()) {
            return Err(format!("{name} is required").into());
        }
    }
    let runtime = (|| {
        let client = client::connect_or_start()?;
        match client.get_opencode_shared_runtime()? {
            Some(runtime) => Ok(runtime),
            None => match client.ensure_opencode_shared_runtime(port) {
                Ok(runtime) => Ok(runtime),
                Err(error) => client.get_opencode_shared_runtime()?.ok_or(error),
            },
        }
    })();
    let runtime = match runtime {
        Ok(runtime) => runtime,
        Err(error) if session.is_some() => {
            return launch_standalone_recovered_opencode(session.unwrap(), &error);
        }
        Err(error) => return Err(error.into()),
    };
    let launch = shared_opencode_launch(SharedOpenCodeLaunchInput {
        shell_id,
        run_id,
        executable: env::var_os("BOOMUX_REAL_OPENCODE"),
        tui_config: env::var_os("BOOMUX_OPENCODE_TUI_CONFIG"),
        original_path: env::var_os("BOOMUX_ORIGINAL_PATH"),
        cwd: env::current_dir()?,
        session: session.map(str::to_owned),
        runtime,
    });
    let launch = match launch {
        Ok(launch) => launch,
        Err(error) if session.is_some() => {
            return launch_standalone_recovered_opencode(session.unwrap(), error.as_ref());
        }
        Err(error) => return Err(error),
    };
    let mut command = Command::new(&launch.executable);
    command
        .args(launch.argv())
        .env("BOOMUX_OPENCODE_SHARED_GENERATION", &launch.generation)
        .env("BOOMUX_OPENCODE_CLAIM_HOLDER", &launch.holder)
        .env("OPENCODE_TUI_CONFIG", &launch.tui_config)
        .env_remove("BOOMUX_REAL_OPENCODE")
        .env_remove("BOOMUX_ORIGINAL_PATH")
        .env_remove("BOOMUX_OPENCODE_SHIM_DIR")
        .env_remove("BOOMUX_OPENCODE_TUI_CONFIG")
        .env_remove("BOOMUX_SHIM_EXECUTABLE");
    if let Some(path) = launch.original_path {
        command.env("PATH", path);
    }
    Err(command.exec().into())
}

#[cfg(test)]
fn install_opencode_at(config_root: &Path, force: bool) -> Result<(), Box<dyn Error>> {
    let result = integration_management::install_at(
        integration_management::IntegrationId::Opencode,
        config_root,
        force,
    )?;
    print_integration_install_results(&[result]);
    Ok(())
}

fn install_pi(force: bool) -> Result<(), Box<dyn Error>> {
    install_integrations(
        &[integration_management::IntegrationId::Pi],
        force,
        false,
        false,
    )
}

#[cfg(test)]
fn install_pi_at(config_root: &Path, force: bool) -> Result<(), Box<dyn Error>> {
    let result = integration_management::install_at(
        integration_management::IntegrationId::Pi,
        config_root,
        force,
    )?;
    print_integration_install_results(&[result]);
    Ok(())
}

fn skill_install_path(home: &Path) -> PathBuf {
    home.join(".agents/skills/boomux/SKILL.md")
}

#[cfg(test)]
fn opencode_install_path(config_root: &Path) -> PathBuf {
    integration_management::install_path_at(
        integration_management::IntegrationId::Opencode,
        config_root,
    )
}

#[cfg(test)]
fn pi_install_path(config_root: &Path) -> PathBuf {
    integration_management::install_path_at(integration_management::IntegrationId::Pi, config_root)
}

fn legacy_skill_install_path(home: &Path) -> PathBuf {
    home.join(".agents/skills/boomux-shells/SKILL.md")
}

fn doctor_version_line(version: &str, architecture: &str, operating_system: &str) -> String {
    format!("ok  boomux: {version} ({architecture}-{operating_system})")
}

fn doctor(terminal_override: Option<&str>) -> Result<(), Box<dyn Error>> {
    println!(
        "{}",
        doctor_version_line(
            env!("CARGO_PKG_VERSION"),
            env::consts::ARCH,
            env::consts::OS
        )
    );
    let mut healthy = true;
    let daemon_snapshot = match client::connect_or_start() {
        Ok(client) => {
            println!(
                "ok  daemon: protocol {} ({})",
                client.protocol_version()?,
                client.socket_path().display()
            );
            match client.snapshot() {
                Ok(snapshot) => Some(snapshot),
                Err(error) => {
                    healthy = false;
                    eprintln!("err daemon snapshot: {error}");
                    None
                }
            }
        }
        Err(error) => {
            healthy = false;
            eprintln!("err daemon: {error}");
            None
        }
    };
    for command in ["git"] {
        match Command::new(command).arg("--version").output() {
            Ok(output) if output.status.success() => {
                println!(
                    "ok  {command}: {}",
                    String::from_utf8_lossy(&output.stdout).trim()
                );
            }
            Ok(output) => {
                healthy = false;
                eprintln!("err {command}: exited with {}", output.status);
            }
            Err(error) => {
                healthy = false;
                eprintln!("err {command}: {error}");
            }
        }
    }
    match config::load() {
        Ok(config) => {
            let discovery = projects::discover(&config.projects);
            let count = discovery.projects.len();
            let source = config
                .path
                .map_or_else(|| "defaults".to_owned(), |path| path.display().to_string());
            if discovery.warnings.is_empty() {
                println!("ok  config: {source} ({count} projects)");
            } else {
                healthy = false;
                eprintln!("err config: {source} ({count} projects)");
                for warning in discovery.warnings {
                    eprintln!("    {warning}");
                }
            }
            let terminal = terminal_override.or(config.terminal.as_deref());
            match terminal::selected(terminal) {
                Ok(selected) => println!("ok  terminal: {selected}"),
                Err(error) => {
                    healthy = false;
                    eprintln!("err terminal: {error}");
                }
            }
            match notification_diagnostic(
                &config.notifications,
                executable_on_path("notify-send"),
                plausible_desktop_bus(),
            ) {
                NotificationDiagnostic::Disabled => {
                    println!("ok  notification config: disabled (sampled at daemon start)");
                }
                NotificationDiagnostic::Ready => {
                    println!(
                        "ok  notification config: notify-send and plausible desktop bus context present; restart daemon after changes"
                    );
                }
                NotificationDiagnostic::MissingExecutable => {
                    healthy = false;
                    eprintln!("err notifications: notify-send is not executable on PATH");
                }
                NotificationDiagnostic::MissingDesktopBus => {
                    healthy = false;
                    eprintln!("err notifications: no plausible desktop bus is available");
                }
            }
            match sound_notification_diagnostic(
                &config.notifications,
                executable_on_path("canberra-gtk-play"),
            ) {
                SoundNotificationDiagnostic::Disabled => {
                    println!("ok  notification sound: disabled (sampled at daemon start)");
                }
                SoundNotificationDiagnostic::Ready => {
                    println!(
                        "ok  notification sound: canberra-gtk-play is executable; restart daemon after changes"
                    );
                }
                SoundNotificationDiagnostic::MissingExecutable => {
                    healthy = false;
                    eprintln!(
                        "err notification sound: canberra-gtk-play is not executable on PATH"
                    );
                }
            }
        }
        Err(error) => {
            healthy = false;
            eprintln!("err config: {error}");
        }
    }
    let integration_environment = integration_management::Environment::from_process();
    for integration in integration_management::IntegrationId::all() {
        let status = integration_management::inspect_without_host_probe(
            integration,
            &integration_environment,
            daemon_snapshot.as_ref(),
        );
        healthy &= print_integration_diagnostic(integration, &status);
    }
    if healthy {
        Ok(())
    } else {
        Err("one or more dependency or configuration checks failed".into())
    }
}

fn print_integration_diagnostic(
    integration: integration_management::IntegrationId,
    status: &integration_management::IntegrationStatus,
) -> bool {
    let spec = integration.spec();
    let installation = integration.installation();
    let path = status.asset.path.as_deref().unwrap_or("unresolved path");
    if status.asset.state == integration_management::AssetState::Unavailable {
        eprintln!(
            "err {} integration: cannot inspect {} at {path}: {}",
            spec.key,
            installation.asset_name,
            status.asset.error.as_deref().unwrap_or("unknown error")
        );
        return false;
    }
    if status.runtime.running_processes == 0 {
        println!(
            "ok  {} integration: {} {} at {path}",
            spec.key,
            installation.asset_name,
            status.asset.state.as_str(),
        );
        return true;
    }
    if status.asset.state != integration_management::AssetState::Current {
        eprintln!(
            "err {} integration: {} {} at {path}; run boomux integration install {}{}",
            spec.key,
            installation.asset_name,
            status.asset.state.as_str(),
            spec.key,
            if status.asset.state == integration_management::AssetState::Modified {
                " --force"
            } else {
                ""
            }
        );
        return false;
    }
    if status.runtime.untracked_processes == 0 {
        println!(
            "ok  {} integration: lifecycle registration active",
            spec.key
        );
        true
    } else {
        if integration == integration_management::IntegrationId::KIRO {
            eprintln!(
                "err {} integration: {} foreground process(es) are untracked; reopen the owning managed ShellRun, then launch bare kiro-cli and verify it loads {path}",
                spec.key, status.runtime.untracked_processes
            );
        } else {
            eprintln!(
                "err {} integration: {} foreground process(es) are untracked; restart {} and verify it loads {path}",
                spec.key, status.runtime.untracked_processes, spec.display_name
            );
        }
        false
    }
}

#[derive(Debug, PartialEq, Eq)]
enum NotificationDiagnostic {
    Disabled,
    Ready,
    MissingExecutable,
    MissingDesktopBus,
}

#[derive(Debug, PartialEq, Eq)]
enum SoundNotificationDiagnostic {
    Disabled,
    Ready,
    MissingExecutable,
}

fn notification_diagnostic(
    settings: &daemon::NotificationDeliverySettings,
    executable: bool,
    desktop_bus: bool,
) -> NotificationDiagnostic {
    if !settings.desktop.enabled {
        NotificationDiagnostic::Disabled
    } else if !executable {
        NotificationDiagnostic::MissingExecutable
    } else if !desktop_bus {
        NotificationDiagnostic::MissingDesktopBus
    } else {
        NotificationDiagnostic::Ready
    }
}

fn sound_notification_diagnostic(
    settings: &daemon::NotificationDeliverySettings,
    executable: bool,
) -> SoundNotificationDiagnostic {
    if !settings.sound.enabled {
        SoundNotificationDiagnostic::Disabled
    } else if !executable {
        SoundNotificationDiagnostic::MissingExecutable
    } else {
        SoundNotificationDiagnostic::Ready
    }
}

fn test_notification(reason: CliNotificationReason) -> Result<(), Box<dyn Error>> {
    let settings = config::load_notification_settings()?;
    daemon::test_notification_delivery(&settings, reason.into())?;
    let reason = match reason {
        CliNotificationReason::Blocked => "blocked",
        CliNotificationReason::Completed => "completed",
    };
    println!("Delivered test {reason} notification");
    Ok(())
}

fn executable_on_path(name: &str) -> bool {
    env::var_os("PATH").is_some_and(|path| {
        env::split_paths(&path).any(|directory| {
            fs::metadata(directory.join(name)).is_ok_and(|metadata| {
                metadata.is_file() && metadata.permissions().mode() & 0o111 != 0
            })
        })
    })
}

fn plausible_desktop_bus() -> bool {
    env::var_os("DBUS_SESSION_BUS_ADDRESS").is_some_and(|value| !value.is_empty())
        || env::var_os("XDG_RUNTIME_DIR").is_some_and(|directory| {
            let directory = PathBuf::from(directory);
            directory.is_absolute() && directory.join("bus").exists()
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::integration_management::{opencode_config_root, pi_config_root};
    use std::io::Read;

    #[cfg(target_os = "linux")]
    #[test]
    fn daemon_executable_normalization_is_exact_and_bounded() {
        assert_eq!(
            normalize_daemon_executable(b"/custom/path with spaces/boomux (deleted)"),
            Some("/custom/path with spaces/boomux".into())
        );
        assert_eq!(normalize_daemon_executable(b"relative/boomux"), None);
        assert_eq!(normalize_daemon_executable(b"/bad\npath"), None);
        assert_eq!(normalize_daemon_executable(&vec![b'x'; 4097]), None);
    }

    fn shell(id: &str, workspace_id: &str, name: &str) -> ShellSnapshot {
        ShellSnapshot {
            id: id.into(),
            revision: 1,
            workspace_id: workspace_id.into(),
            name: name.into(),
            cwd: PathBuf::from("/tmp/project"),
            command: Vec::new(),
            status: ShellStatus::Running,
            run: Some(protocol::ShellRunSnapshot {
                id: "r1".into(),
                generation: 1,
                started_at_ms: 1,
                ended_at_ms: None,
                exit_reason: None,
                output_revision: 0,
                environment_has_run_id: true,
            }),
            recovered_agent_id: None,
            foreground_process: None,
        }
    }

    fn workspace(id: &str, name: &str, shells: Vec<ShellSnapshot>) -> WorkspaceSnapshot {
        WorkspaceSnapshot {
            id: id.into(),
            revision: 1,
            name: name.into(),
            default_cwd: None,
            shells,
            launchers: Vec::new(),
            agents: Vec::new(),
        }
    }

    fn coordinated_workspace(
        id: &str,
        name: &str,
        closing: bool,
    ) -> protocol::GlobalWorkspaceSnapshot {
        protocol::GlobalWorkspaceSnapshot {
            id: id.into(),
            revision: 1,
            name: name.into(),
            closing,
            placements: Vec::new(),
        }
    }

    #[test]
    fn desktop_commands_are_human_only_and_parse_every_action() {
        for (action, expected) in [
            ("toggle", DesktopCommands::Toggle),
            ("next", DesktopCommands::Next),
            ("previous", DesktopCommands::Previous),
            ("terminal", DesktopCommands::Terminal),
            ("close", DesktopCommands::Close),
            ("pop", DesktopCommands::Pop),
            ("return", DesktopCommands::Return),
            ("gather", DesktopCommands::Gather),
        ] {
            let cli = Cli::try_parse_from(["boomux", "desktop", action]).unwrap();
            assert_eq!(cli.command_descriptor().key, "desktop");
            assert_eq!(cli.command_descriptor().output, OutputMode::HumanOnly);
            assert!(matches!(
                cli.command,
                Some(Commands::Desktop { command }) if std::mem::discriminant(&command) == std::mem::discriminant(&expected)
            ));
        }
        assert!(Cli::try_parse_from(["boomux", "desktop"]).is_err());

        let cli = Cli::try_parse_from(["boomux", "desktop", "show", "workspace-1"]).unwrap();
        assert!(matches!(
            cli.command,
            Some(Commands::Desktop {
                command: DesktopCommands::Show { target }
            }) if target == "workspace-1"
        ));
    }

    #[test]
    fn shell_open_parses_exact_coordinated_workspace_placement() {
        let cli = Cli::try_parse_from([
            "boomux",
            "open",
            "shell-1",
            "--node",
            "node-1",
            "--workspace",
            "workspace-1",
            "--takeover",
        ])
        .unwrap();
        assert!(matches!(
            cli.command,
            Some(Commands::Open {
                shell_id,
                node: Some(node),
                workspace: Some(workspace),
                takeover: true,
                ..
            }) if shell_id == "shell-1" && node == "node-1" && workspace == "workspace-1"
        ));
    }

    #[test]
    fn workspace_open_parses_desktop_show_request() {
        let cli =
            Cli::try_parse_from(["boomux", "workspace", "open", "workspace-1", "--show"]).unwrap();
        assert!(matches!(
            cli.command,
            Some(Commands::Workspace {
                command: WorkspaceCommands::Open {
                    target,
                    node: None,
                    show: true,
                }
            }) if target == "workspace-1"
        ));
    }

    #[test]
    fn shell_desktop_placement_requires_exact_active_membership() {
        let workspace = protocol::GlobalWorkspaceSnapshot {
            id: Uuid::from_u128(10).to_string(),
            revision: 1,
            name: "project".into(),
            closing: false,
            placements: vec![protocol::WorkspacePlacementSnapshot {
                node_id: "node-1".into(),
                workspace_id: "owner-1".into(),
                owner_workspace_name: Some("project".into()),
                owner_revision: 1,
                default_cwd: None,
                state: protocol::WorkspacePlacementState::Active,
            }],
        };

        assert_eq!(
            resolve_shell_desktop_workspace(
                "project",
                "node-1",
                "owner-1",
                std::slice::from_ref(&workspace),
            )
            .unwrap()
            .id,
            workspace.id
        );
        assert!(
            resolve_shell_desktop_workspace("project", "node-1", "other-owner", &[workspace],)
                .is_err()
        );
    }

    #[test]
    fn tui_workspace_restore_reports_partial_success_as_a_warning() {
        let partial = CoordinatedWorkspaceOpenOutcome {
            opened_launchers: 1,
            opened_shells: 2,
            failures: vec!["remote unavailable".into()],
        };
        let message = tui_workspace_open_message("project", &partial).unwrap();
        assert!(message.contains("Opened 1 launcher(s) and 2 shell(s) for project"));
        assert!(message.contains("warning: 1 operation(s) unavailable"));

        let failed = CoordinatedWorkspaceOpenOutcome {
            opened_launchers: 0,
            opened_shells: 0,
            failures: vec!["remote unavailable".into()],
        };
        assert!(tui_workspace_open_message("project", &failed).is_err());
    }

    #[test]
    fn tui_item_placement_resolves_its_exact_coordinated_workspace() {
        let workspace = protocol::GlobalWorkspaceSnapshot {
            id: Uuid::from_u128(10).to_string(),
            revision: 1,
            name: "project".into(),
            closing: false,
            placements: vec![protocol::WorkspacePlacementSnapshot {
                node_id: "node-1".into(),
                workspace_id: "owner-1".into(),
                owner_workspace_name: Some("project".into()),
                owner_revision: 1,
                default_cwd: None,
                state: protocol::WorkspacePlacementState::Active,
            }],
        };

        assert_eq!(
            coordinated_workspace_for_owner(std::slice::from_ref(&workspace), "node-1", "owner-1",)
                .unwrap()
                .unwrap()
                .id,
            workspace.id
        );
        assert!(
            coordinated_workspace_for_owner(&[workspace], "node-2", "owner-1")
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn desktop_workspace_order_excludes_closing_and_cycles_both_directions() {
        let workspaces = vec![
            coordinated_workspace("550e8400-e29b-41d4-a716-446655440003", "zeta", false),
            coordinated_workspace("550e8400-e29b-41d4-a716-446655440002", "alpha", true),
            coordinated_workspace("550e8400-e29b-41d4-a716-446655440001", "alpha", false),
            coordinated_workspace("550e8400-e29b-41d4-a716-446655440000", "alpha", false),
        ];
        let ordered = desktop_workspaces(&workspaces);
        assert_eq!(
            ordered
                .iter()
                .map(|workspace| workspace.id.as_str())
                .collect::<Vec<_>>(),
            [
                "550e8400-e29b-41d4-a716-446655440000",
                "550e8400-e29b-41d4-a716-446655440001",
                "550e8400-e29b-41d4-a716-446655440003",
            ]
        );
        assert_eq!(
            cycle_desktop_workspace(&ordered, 0, true).unwrap().id,
            ordered[1].id
        );
        assert_eq!(
            cycle_desktop_workspace(&ordered, 0, false).unwrap().id,
            ordered[2].id
        );
        assert_eq!(
            cycle_desktop_workspace(&ordered, 2, true).unwrap().id,
            ordered[0].id
        );
        assert!(cycle_desktop_workspace(&ordered[..1], 0, true).is_none());
    }

    fn combined_node(id: &str, alias: &str, eligible: bool) -> protocol::CombinedNode {
        protocol::CombinedNode {
            node_id: id.into(),
            alias: alias.into(),
            local: alias == "local",
            route: None,
            registration_revision: None,
            health: protocol::NodeProjectionHealthCode::Online,
            current: true,
            stale: false,
            observed_at_ms: 1,
            observed_protocol_version: Some(protocol::PROTOCOL_VERSION),
            observed_helper_version: Some(env!("CARGO_PKG_VERSION").into()),
            observed_capabilities: vec!["global_workspaces".into()],
            workspace_owner_eligible: eligible,
            workspace_owner_unavailable_reason: (!eligible).then(|| "unavailable".into()),
            local_snapshot: None,
            remote_projection: None,
        }
    }

    #[test]
    fn global_workspace_grouping_uses_exact_placements_and_keeps_equal_name_external() {
        let first = workspace("owner-1", "same", vec![shell("shell-1", "owner-1", "one")]);
        let second = workspace(
            "owner-2",
            "different",
            vec![shell("shell-2", "owner-2", "two")],
        );
        let external = workspace("owner-3", "same", Vec::new());
        let mut cache = git::Cache::default();
        let mut views = dashboard_projection::project(&[first, second, external], &mut cache);
        let mut local = views[0].node.clone();
        local.id = Uuid::from_u128(1).to_string();
        let mut remote = local.clone();
        remote.id = Uuid::from_u128(2).to_string();
        remote.alias = "work".into();
        remote.local = false;
        let mut other = remote.clone();
        other.id = Uuid::from_u128(3).to_string();
        other.alias = "other".into();
        views[0].node = local.clone();
        views[1].node = remote.clone();
        views[2].node = other.clone();
        for (view, node) in views.iter_mut().zip([&local, &remote, &other]) {
            view.item_owners = vec![
                tui::WorkspaceItemOwnerView {
                    node: node.clone(),
                    workspace_id: view.id.clone(),
                };
                view.items.len()
            ];
        }
        let global_id = Uuid::from_u128(10).to_string();
        let grouped = group_dashboard_workspaces(
            views,
            &[local.clone(), remote.clone(), other.clone()],
            vec![protocol::GlobalWorkspaceSnapshot {
                id: global_id.clone(),
                revision: 2,
                name: "same".into(),
                closing: false,
                placements: vec![
                    protocol::WorkspacePlacementSnapshot {
                        node_id: local.id.clone(),
                        workspace_id: "owner-1".into(),
                        owner_workspace_name: Some("local-owner".into()),
                        owner_revision: 1,
                        default_cwd: None,
                        state: protocol::WorkspacePlacementState::Active,
                    },
                    protocol::WorkspacePlacementSnapshot {
                        node_id: remote.id.clone(),
                        workspace_id: "owner-2".into(),
                        owner_workspace_name: Some("remote-owner".into()),
                        owner_revision: 1,
                        default_cwd: None,
                        state: protocol::WorkspacePlacementState::Active,
                    },
                    protocol::WorkspacePlacementSnapshot {
                        node_id: Uuid::from_u128(99).to_string(),
                        workspace_id: "missing-owner".into(),
                        owner_workspace_name: Some("missing".into()),
                        owner_revision: 1,
                        default_cwd: None,
                        state: protocol::WorkspacePlacementState::Unavailable,
                    },
                ],
            }],
            vec![protocol::ExternalWorkspaceSnapshot {
                identity: protocol::QualifiedIdentity::new(&other.id, "owner-3"),
                revision: 1,
                name: "same".into(),
                default_cwd: None,
                available: true,
            }],
        );
        assert_eq!(grouped.len(), 2);
        let global = grouped
            .iter()
            .find(|workspace| workspace.id == global_id)
            .unwrap();
        assert_eq!(global.items.len(), 2);
        let tui::WorkspaceCoordinationView::Global { placements, .. } = &global.coordination else {
            panic!("expected global Workspace");
        };
        assert_eq!(placements.len(), 3);
        assert!(placements.iter().any(|placement| {
            placement.workspace_id == "missing-owner"
                && placement.node.alias.starts_with("unregistered:")
                && !placement.node.current
        }));
        assert_eq!(global.item_owners[0].node.id, local.id);
        assert_eq!(global.item_owners[1].node.id, remote.id);
        let singleton = grouped
            .iter()
            .find(|workspace| workspace.id == "owner-3")
            .unwrap();
        assert!(matches!(
            singleton.coordination,
            tui::WorkspaceCoordinationView::External { .. }
        ));
    }

    #[test]
    fn dashboard_workspaces_sort_by_task_name_before_node_alias() {
        let mut cache = git::Cache::default();
        let mut views = dashboard_projection::project(
            &[
                workspace("owner-z", "alpha", Vec::new()),
                workspace("owner-a", "zulu", Vec::new()),
            ],
            &mut cache,
        );
        views[0].node.alias = "z-node".into();
        views[1].node.alias = "a-node".into();
        sort_dashboard_workspaces(&mut views);
        assert_eq!(
            views
                .iter()
                .map(|workspace| workspace.name.as_str())
                .collect::<Vec<_>>(),
            ["alpha", "zulu"]
        );
    }

    fn agent(id: &str, workspace_id: &str, shell_id: &str) -> AgentInstanceSnapshot {
        AgentInstanceSnapshot {
            id: id.into(),
            workspace_id: workspace_id.into(),
            shell_id: shell_id.into(),
            run_id: "r1".into(),
            name: "OpenCode".into(),
            integration: "opencode".into(),
            external_session_id: Some("external-1".into()),
            cwd: Some("/tmp/project".into()),
            started_at_ms: 10,
            ended_at_ms: None,
            attention: None,
            observation: protocol::AgentObservationSnapshot {
                revision: 1,
                state: AgentState::Working,
                authority: AgentAuthority::LifecycleIntegration,
                evidence: "tool call".into(),
                confidence: 95,
                observed_at_ms: 11,
            },
            working_contexts: Vec::new(),
        }
    }

    fn launcher(id: &str, workspace_id: &str, name: &str) -> WorkspaceLauncherSnapshot {
        WorkspaceLauncherSnapshot {
            id: id.into(),
            revision: 1,
            workspace_id: workspace_id.into(),
            name: name.into(),
            cwd: PathBuf::from("/tmp/project"),
            command: vec!["zeditor".into(), ".".into()],
        }
    }

    #[test]
    fn parses_paths_and_native_hidden_commands() {
        let cli = Cli::try_parse_from(["boomux", "."]).unwrap();
        assert_eq!(cli.path, Some(PathBuf::from(".")));
        assert!(Cli::try_parse_from(["boomux", "daemon", "run"]).is_ok());
        assert!(Cli::try_parse_from(["boomux", "daemon", "restart"]).is_ok());
        assert!(Cli::try_parse_from(["boomux", "daemon", "stop"]).is_ok());
        let cli =
            Cli::try_parse_from(["boomux", "__attach", "s1", "--takeover", "--restart-exited"])
                .unwrap();
        assert!(matches!(
            cli.command,
            Some(Commands::Attach {
                takeover: true,
                restart_exited: true,
                ..
            })
        ));
        let cli = Cli::try_parse_from([
            "boomux",
            "__attach",
            "s1",
            "--takeover",
            "--expected-run-id",
            "r1",
        ])
        .unwrap();
        assert!(matches!(
            cli.command,
            Some(Commands::Attach {
                restart_exited: false,
                expected_run_id: Some(run_id),
                ..
            }) if run_id == "r1"
        ));
        let cli = Cli::try_parse_from([
            "boomux",
            "__await-attach",
            "s1",
            "--gate",
            "/run/user/1/gate",
            "--node",
            "node-1",
        ])
        .unwrap();
        assert!(matches!(
            cli.command,
            Some(Commands::AwaitAttach {
                shell_id,
                gate,
                node: Some(node),
            }) if shell_id == "s1" && gate == Path::new("/run/user/1/gate") && node == "node-1"
        ));
    }

    #[test]
    fn config_commands_are_human_only() {
        for (subcommand, key) in [
            ("path", "config.path"),
            ("validate", "config.validate"),
            ("edit", "config.edit"),
        ] {
            let cli = Cli::try_parse_from(["boomux", "config", subcommand]).unwrap();
            assert_eq!(cli.command_descriptor().key, key);
            assert_eq!(cli.command_descriptor().output, OutputMode::HumanOnly);
        }
    }

    #[test]
    fn parses_web_serve_and_targeted_stop() {
        let serve =
            Cli::try_parse_from(["boomux", "web", "--port", "4040", "--tailscale"]).unwrap();
        assert!(matches!(
            serve.command,
            Some(Commands::Web {
                command: None,
                port: 4040,
                tailscale: true,
                ..
            })
        ));
        assert!(
            Cli::try_parse_from([
                "boomux",
                "web",
                "--tailscale",
                "--opencode-web-url",
                "https://example.test"
            ])
            .is_err()
        );

        let stop = Cli::try_parse_from(["boomux", "web", "stop", "--port", "4040"]).unwrap();
        assert!(matches!(
            stop.command,
            Some(Commands::Web {
                command: Some(WebCommands::Stop { port: 4040 }),
                ..
            })
        ));
        let start =
            Cli::try_parse_from(["boomux", "web", "start", "--port", "4040", "--tailscale"])
                .unwrap();
        assert!(matches!(
            start.command,
            Some(Commands::Web {
                command: Some(WebCommands::Start {
                    port: 4040,
                    tailscale: true,
                    ..
                }),
                ..
            })
        ));
        assert!(matches!(
            Cli::try_parse_from(["boomux", "web", "status", "--port", "4040"])
                .unwrap()
                .command,
            Some(Commands::Web {
                command: Some(WebCommands::Status { port: 4040 }),
                ..
            })
        ));
    }

    #[test]
    fn dashboard_requires_terminal_input_and_output() {
        validate_dashboard_terminal(true, true).unwrap();
        assert_eq!(
            validate_dashboard_terminal(false, true).unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
        assert_eq!(
            validate_dashboard_terminal(true, false).unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
    }

    #[test]
    fn parses_node_rekey_and_requires_exact_confirmation() {
        let cli = Cli::try_parse_from(["boomux", "node", "rekey"]).unwrap();
        assert!(matches!(
            cli.command.as_ref(),
            Some(Commands::Node {
                command: NodeCommands::Rekey
            })
        ));
        assert_eq!(cli.command_descriptor().key, "node.rekey");

        let node_id = "550e8400-e29b-41d4-a716-446655440000";
        validate_rekey_confirmation(node_id, &format!("{node_id}\n")).unwrap();
        assert_eq!(
            validate_rekey_confirmation(node_id, "550e8400-e29b-41d4-a716-446655440001\n")
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidInput
        );
    }

    #[test]
    fn node_upgrade_uninstall_and_reauthentication_wrappers_are_human_only() {
        let cli = Cli::try_parse_from(["boomux", "node", "upgrade", "work"]).unwrap();
        assert!(matches!(
            cli.command.as_ref(),
            Some(Commands::Node {
                command: NodeCommands::Upgrade { selector }
            }) if selector == "work"
        ));
        assert_eq!(cli.command_descriptor().key, "node.upgrade");

        let cli = Cli::try_parse_from(["boomux", "node", "uninstall", "work"]).unwrap();
        assert!(matches!(
            cli.command.as_ref(),
            Some(Commands::Node {
                command: NodeCommands::Uninstall { selector }
            }) if selector == "work"
        ));
        assert_eq!(cli.command_descriptor().key, "node.uninstall");
        assert_eq!(cli.command_descriptor().output, OutputMode::HumanOnly);

        let cli = Cli::try_parse_from(["boomux", "node", "reauthenticate", "work"]).unwrap();
        assert!(matches!(
            cli.command.as_ref(),
            Some(Commands::Node {
                command: NodeCommands::Reauthenticate { selector }
            }) if selector == "work"
        ));
        assert_eq!(cli.command_descriptor().key, "node.reauthenticate");
        assert_eq!(cli.command_descriptor().output, OutputMode::HumanOnly);

        let cli =
            Cli::try_parse_from(["boomux", "__guided-node-reauthenticate", "node-id"]).unwrap();
        assert!(matches!(
            cli.command.as_ref(),
            Some(Commands::GuidedNodeReauthenticate { selector }) if selector == "node-id"
        ));
        assert_eq!(cli.command_descriptor().key, "node.reauthenticate");
        assert_eq!(cli.command_descriptor().output, OutputMode::HumanOnly);

        let cli = Cli::try_parse_from(["boomux", "__guided-node-upgrade", "node-id"]).unwrap();
        assert!(matches!(
            cli.command.as_ref(),
            Some(Commands::GuidedNodeUpgrade { selector }) if selector == "node-id"
        ));
        assert_eq!(cli.command_descriptor().key, "node.upgrade");
        assert!(should_release_node_upgrade_maintenance(
            ssh_bootstrap::BootstrapRecoveryDisposition::NoRemoteMutation
        ));
        assert!(should_release_node_upgrade_maintenance(
            ssh_bootstrap::BootstrapRecoveryDisposition::RollbackConfirmed
        ));
        assert!(!should_release_node_upgrade_maintenance(
            ssh_bootstrap::BootstrapRecoveryDisposition::OutcomeUnknown
        ));
    }

    #[test]
    fn local_uninstall_is_human_only_and_purge_is_explicit() {
        let cli = Cli::try_parse_from(["boomux", "uninstall"]).unwrap();
        assert!(matches!(
            cli.command.as_ref(),
            Some(Commands::Uninstall { purge: false })
        ));
        assert_eq!(cli.command_descriptor().key, "uninstall");
        assert_eq!(cli.command_descriptor().output, OutputMode::HumanOnly);

        let cli = Cli::try_parse_from(["boomux", "uninstall", "--purge"]).unwrap();
        assert!(matches!(
            cli.command.as_ref(),
            Some(Commands::Uninstall { purge: true })
        ));

        let node_id = "550e8400-e29b-41d4-a716-446655440000";
        let cli =
            Cli::try_parse_from(["boomux", "__uninstall-remote", node_id, "fingerprint"]).unwrap();
        assert!(matches!(
            cli.command.as_ref(),
            Some(Commands::UninstallRemote { expected_node_id, expected_executable })
                if expected_node_id == node_id && expected_executable == "fingerprint"
        ));
        assert_eq!(cli.command_descriptor().key, "uninstall-remote");
        assert_eq!(cli.command_descriptor().output, OutputMode::HumanOnly);

        let cli = Cli::try_parse_from(["boomux", "__uninstall-fingerprint"]).unwrap();
        assert!(matches!(
            cli.command.as_ref(),
            Some(Commands::UninstallFingerprint)
        ));
    }

    #[test]
    fn guided_setup_is_human_only() {
        let cli = Cli::try_parse_from(["boomux", "setup"]).unwrap();
        assert!(matches!(cli.command.as_ref(), Some(Commands::Setup)));
        assert_eq!(cli.command_descriptor().key, "setup");
        assert_eq!(cli.command_descriptor().output, OutputMode::HumanOnly);
    }

    #[test]
    fn node_add_supports_explicit_and_guided_inputs() {
        let explicit = Cli::try_parse_from(["boomux", "node", "add", "work", "user@host"]).unwrap();
        assert!(matches!(
            explicit.command,
            Some(Commands::Node {
                command: NodeCommands::Add {
                    alias: Some(alias),
                    target: Some(target),
                }
            }) if alias == "work" && target == "user@host"
        ));
        let guided = Cli::try_parse_from(["boomux", "node", "add"]).unwrap();
        assert!(matches!(
            guided.command,
            Some(Commands::Node {
                command: NodeCommands::Add {
                    alias: None,
                    target: None,
                }
            })
        ));
        assert_eq!(
            resolve_node_add_inputs(Some("work".into()), Some("host".into()), false).unwrap(),
            ("work".into(), "host".into())
        );
        assert!(resolve_node_add_inputs(Some("work".into()), None, false).is_err());
        assert!(resolve_node_add_inputs(None, None, true).is_err());

        let held = Cli::try_parse_from(["boomux", "__guided-node-add"]).unwrap();
        assert!(matches!(held.command, Some(Commands::GuidedNodeAdd)));
        assert_eq!(held.command_descriptor().key, "node.add");
    }

    #[test]
    fn guided_node_add_preserves_failure_status_and_distinguishes_newline_from_eof() {
        let directory = env::temp_dir().join(format!("boomux-guided-{}", Uuid::new_v4()));
        fs::create_dir(&directory).unwrap();
        let child = directory.join("child");
        let child_output = directory.join("child-output");
        fs::write(
            &child,
            format!(
                "#!/bin/sh\nprintf 'child failed\\n'\nprintf 'child failed\\n' > '{}'\nexit 7\n",
                child_output.display()
            ),
        )
        .unwrap();
        fs::set_permissions(&child, fs::Permissions::from_mode(0o700)).unwrap();

        let mut newline = io::Cursor::new(b"\n");
        let mut output = Vec::new();
        assert_eq!(
            guided_node_add_with(&child, &mut newline, &mut output, true, None).unwrap(),
            process_adapter::ProcessExit::Code(7)
        );
        let output = String::from_utf8(output).unwrap();
        assert_eq!(fs::read_to_string(&child_output).unwrap(), "child failed\n");
        assert!(output.contains("Node setup failed (exit 7)."));
        assert!(output.contains("Press Enter to close."));
        assert!(!output.contains("Input closed before a newline"));

        let mut eof = io::Cursor::new(b"partial");
        let mut output = Vec::new();
        assert_eq!(
            guided_node_add_with(&child, &mut eof, &mut output, true, None).unwrap(),
            process_adapter::ProcessExit::Code(7)
        );
        assert!(
            String::from_utf8(output)
                .unwrap()
                .contains("Input closed before a newline")
        );

        let mut eof = io::Cursor::new(Vec::<u8>::new());
        let mut output = Vec::new();
        assert_eq!(
            guided_node_add_with(
                &directory.join("missing"),
                &mut eof,
                &mut output,
                true,
                None,
            )
            .unwrap(),
            process_adapter::ProcessExit::Code(1)
        );
        let output = String::from_utf8(output).unwrap();
        assert!(output.contains("failed to start"));
        assert!(output.contains("Press Enter to close."));
        assert!(output.contains("Input closed before a newline"));
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn guided_pty_helper_process() {
        let Some(executable) = env::var_os("BOOMUX_TEST_GUIDED_EXECUTABLE") else {
            return;
        };
        let stdin = io::stdin();
        let mut input = stdin.lock();
        let mut output = io::stdout().lock();
        let outcome = guided_node_add_with(
            Path::new(&executable),
            &mut input,
            &mut output,
            true,
            Some(stdin.as_raw_fd()),
        )
        .unwrap();
        std::process::exit(match outcome {
            process_adapter::ProcessExit::Code(code) => code,
            process_adapter::ProcessExit::Signal(signal) => 128 + signal,
        });
    }

    #[test]
    fn guided_node_add_pty_interrupts_child_and_holds_for_newline() {
        use std::os::fd::{FromRawFd, OwnedFd};

        let directory = env::temp_dir().join(format!("boomux-guided-pty-{}", Uuid::new_v4()));
        fs::create_dir(&directory).unwrap();
        let child = directory.join("child");
        fs::write(
            &child,
            "#!/bin/sh\nkill -STOP $$\nprintf 'child-ready\\n'\nread line\nexit 7\n",
        )
        .unwrap();
        fs::set_permissions(&child, fs::Permissions::from_mode(0o700)).unwrap();

        let mut master = 0;
        let mut slave = 0;
        assert_eq!(
            unsafe {
                libc::openpty(
                    &mut master,
                    &mut slave,
                    std::ptr::null_mut(),
                    std::ptr::null(),
                    std::ptr::null(),
                )
            },
            0
        );
        let master = unsafe { OwnedFd::from_raw_fd(master) };
        let slave = unsafe { OwnedFd::from_raw_fd(slave) };
        let stdin = Stdio::from(slave.try_clone().unwrap());
        let stdout = Stdio::from(slave.try_clone().unwrap());
        let stderr = Stdio::from(slave);
        let mut command = Command::new(env::current_exe().unwrap());
        command
            .args(["--exact", "tests::guided_pty_helper_process", "--nocapture"])
            .env("BOOMUX_TEST_GUIDED_EXECUTABLE", &child)
            .stdin(stdin)
            .stdout(stdout)
            .stderr(stderr);
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() == -1 || libc::ioctl(0, libc::TIOCSCTTY, 0) == -1 {
                    Err(io::Error::last_os_error())
                } else {
                    Ok(())
                }
            });
        }
        let mut wrapper = command.spawn().unwrap();
        let mut master = std::fs::File::from(master);
        let flags = unsafe { libc::fcntl(master.as_raw_fd(), libc::F_GETFL) };
        assert_ne!(flags, -1);
        assert_ne!(
            unsafe { libc::fcntl(master.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK,) },
            -1
        );
        let mut output = Vec::new();
        read_pty_until(
            &mut master,
            &mut output,
            b"child-ready",
            Duration::from_secs(3),
        );
        io::Write::write_all(&mut master, &[3]).unwrap();
        read_pty_until(
            &mut master,
            &mut output,
            b"Press Enter to close.",
            Duration::from_secs(3),
        );
        assert!(wrapper.try_wait().unwrap().is_none());
        io::Write::write_all(&mut master, b"\n").unwrap();
        let status = wrapper.wait().unwrap();
        assert_eq!(status.code(), Some(130));
        let output = String::from_utf8_lossy(&output);
        assert!(output.contains("Node setup failed (signal 2)."));
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn guided_node_add_pty_restores_foreground_and_kills_group_after_sigcont_failure() {
        use std::os::fd::{FromRawFd, OwnedFd};

        let directory = env::temp_dir().join(format!("boomux-guided-cont-{}", Uuid::new_v4()));
        fs::create_dir(&directory).unwrap();
        let child = directory.join("child");
        let resumed = directory.join("resumed");
        fs::write(
            &child,
            format!(
                "#!/bin/sh\nkill -STOP $$\nprintf resumed > '{}'\nsleep 30\n",
                resumed.display()
            ),
        )
        .unwrap();
        fs::set_permissions(&child, fs::Permissions::from_mode(0o700)).unwrap();

        let mut master = 0;
        let mut slave = 0;
        assert_eq!(
            unsafe {
                libc::openpty(
                    &mut master,
                    &mut slave,
                    std::ptr::null_mut(),
                    std::ptr::null(),
                    std::ptr::null(),
                )
            },
            0
        );
        let master = unsafe { OwnedFd::from_raw_fd(master) };
        let slave = unsafe { OwnedFd::from_raw_fd(slave) };
        let mut command = Command::new(env::current_exe().unwrap());
        command
            .args(["--exact", "tests::guided_pty_helper_process", "--nocapture"])
            .env("BOOMUX_TEST_GUIDED_EXECUTABLE", &child)
            .env("BOOMUX_TEST_GUIDED_SIGCONT_FAILURE", "1")
            .stdin(Stdio::from(slave.try_clone().unwrap()))
            .stdout(Stdio::from(slave.try_clone().unwrap()))
            .stderr(Stdio::from(slave));
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() == -1 || libc::ioctl(0, libc::TIOCSCTTY, 0) == -1 {
                    Err(io::Error::last_os_error())
                } else {
                    Ok(())
                }
            });
        }
        let mut wrapper = command.spawn().unwrap();
        let mut master = std::fs::File::from(master);
        let flags = unsafe { libc::fcntl(master.as_raw_fd(), libc::F_GETFL) };
        assert_ne!(flags, -1);
        assert_ne!(
            unsafe { libc::fcntl(master.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) },
            -1
        );
        let mut output = Vec::new();
        read_pty_until(
            &mut master,
            &mut output,
            b"Node setup failed to continue",
            Duration::from_secs(3),
        );
        read_pty_until(
            &mut master,
            &mut output,
            b"Press Enter to close.",
            Duration::from_secs(3),
        );
        assert!(wrapper.try_wait().unwrap().is_none());
        assert!(!resumed.exists());
        io::Write::write_all(&mut master, b"\n").unwrap();
        assert_eq!(wrapper.wait().unwrap().code(), Some(1));
        assert!(!resumed.exists());
        fs::remove_dir_all(directory).unwrap();
    }

    fn read_pty_until(
        master: &mut fs::File,
        output: &mut Vec<u8>,
        expected: &[u8],
        timeout: Duration,
    ) {
        let deadline = Instant::now() + timeout;
        let mut bytes = [0_u8; 1024];
        while !output
            .windows(expected.len())
            .any(|window| window == expected)
        {
            match master.read(&mut bytes) {
                Ok(0) => panic!("PTY closed before {:?}", String::from_utf8_lossy(expected)),
                Ok(count) => output.extend_from_slice(&bytes[..count]),
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    assert!(
                        Instant::now() < deadline,
                        "PTY output: {}",
                        String::from_utf8_lossy(output)
                    );
                    thread::sleep(Duration::from_millis(10));
                }
                Err(error) => panic!("PTY read failed: {error}"),
            }
        }
    }

    #[test]
    fn node_snapshot_is_separate_json_command_with_structural_identities() {
        let cli = Cli::try_parse_from(["boomux", "--json", "node", "snapshot", "work"]).unwrap();
        assert!(matches!(
            cli.command.as_ref(),
            Some(Commands::Node {
                command: NodeCommands::Snapshot {
                    selector: Some(selector)
                }
            }) if selector == "work"
        ));
        assert_eq!(cli.command_descriptor().key, "node.snapshot");

        let qualified = qualify_resource_identities(
            serde_json::json!({
                "id": "resource",
                "workspace_id": "workspace",
                "external_session_id": "private-host-id"
            }),
            "node",
        );
        assert_eq!(
            qualified["id"],
            serde_json::json!({"node_id": "node", "inner_id": "resource"})
        );
        assert_eq!(qualified["workspace_id"]["node_id"], "node");
        assert_eq!(qualified["external_session_id"], "private-host-id");
    }

    #[test]
    fn accepts_path_options_and_named_subcommands() {
        let cli = Cli::try_parse_from(["boomux", ".", "--name", "feature", "--new"]).unwrap();
        assert_eq!(cli.name.as_deref(), Some("feature"));
        assert!(cli.new_window);
        assert!(cli.startup_command.is_empty());
        assert!(Cli::try_parse_from(["boomux", "--name", "feature"]).is_err());
        assert!(matches!(
            Cli::try_parse_from(["boomux", "doctor"]).unwrap().command,
            Some(Commands::Doctor)
        ));
        assert!(matches!(
            Cli::try_parse_from(["boomux", "close", "tests"])
                .unwrap()
                .command,
            Some(Commands::Close { target: Some(target), focused: false }) if target == "tests"
        ));
        assert!(matches!(
            Cli::try_parse_from(["boomux", "close", "--focused"])
                .unwrap()
                .command,
            Some(Commands::Close {
                target: None,
                focused: true
            })
        ));
        assert!(Cli::try_parse_from(["boomux", "close"]).is_err());
        assert!(Cli::try_parse_from(["boomux", "close", "tests", "--focused"]).is_err());
    }

    #[test]
    fn parses_public_workspace_adopt_link_and_retry_commands() {
        let set = Cli::try_parse_from([
            "boomux",
            "--json",
            "workspace",
            "set-default-cwd",
            "global",
            "--node",
            "work",
            "--cwd",
            "/project",
        ])
        .unwrap();
        assert_eq!(set.command_descriptor().key, "workspace.set-default-cwd");
        assert!(matches!(
            set.command,
            Some(Commands::Workspace {
                command: WorkspaceCommands::SetDefaultCwd { target, node, cwd }
            }) if target == "global" && node == "work" && cwd == Path::new("/project")
        ));
        assert!(matches!(
            Cli::try_parse_from(["boomux", "workspace", "adopt", "owner", "--node", "work"])
                .unwrap()
                .command,
            Some(Commands::Workspace {
                command: WorkspaceCommands::Adopt { target, node }
            }) if target == "owner" && node == "work"
        ));
        assert!(matches!(
            Cli::try_parse_from([
                "boomux", "workspace", "link", "global", "owner", "--node", "work"
            ])
            .unwrap()
            .command,
            Some(Commands::Workspace {
                command: WorkspaceCommands::Link { target, owner, node }
            }) if target == "global" && owner == "owner" && node == "work"
        ));
        assert!(matches!(
            Cli::try_parse_from(["boomux", "workspace", "retry", "global"])
                .unwrap()
                .command,
            Some(Commands::Workspace {
                command: WorkspaceCommands::Retry { target }
            }) if target == "global"
        ));
    }

    #[test]
    fn workspace_resource_owner_selection_delegates_sole_and_requires_ambiguous_choice() {
        let local = combined_node("local-id", "local", true);
        let remote = combined_node("remote-id", "work", true);
        assert_eq!(
            select_eligible_workspace_owner(std::slice::from_ref(&local), None)
                .unwrap()
                .node_id,
            "local-id"
        );
        assert!(select_eligible_workspace_owner(&[local.clone(), remote.clone()], None).is_err());
        assert_eq!(
            select_eligible_workspace_owner(&[local, remote], Some("remote-id"))
                .unwrap()
                .node_id,
            "remote-id"
        );

        let context = WorkspaceCreationContext {
            global_workspaces_supported: true,
            combined: Some(protocol::CombinedNodeSnapshot {
                nodes: vec![
                    combined_node("local-id", "local", true),
                    combined_node("remote-id", "work", true),
                ],
                workspaces: vec![protocol::GlobalWorkspaceSnapshot {
                    id: "global-id".into(),
                    revision: 2,
                    name: "project".into(),
                    closing: false,
                    placements: vec![
                        protocol::WorkspacePlacementSnapshot {
                            node_id: "local-id".into(),
                            workspace_id: "local-owner".into(),
                            owner_workspace_name: Some("project".into()),
                            owner_revision: 1,
                            default_cwd: Some("/local".into()),
                            state: protocol::WorkspacePlacementState::Active,
                        },
                        protocol::WorkspacePlacementSnapshot {
                            node_id: "remote-id".into(),
                            workspace_id: "remote-owner".into(),
                            owner_workspace_name: Some("project".into()),
                            owner_revision: 1,
                            default_cwd: Some("/remote".into()),
                            state: protocol::WorkspacePlacementState::Active,
                        },
                    ],
                }],
                external_workspaces: Vec::new(),
                focused_terminal: None,
            }),
        };
        assert_eq!(
            context
                .coordinated_workspace_for_local_owner("local-owner")
                .unwrap()
                .unwrap()
                .id,
            "global-id"
        );
        let selected = context
            .coordinated_owner("project", Some("work"))
            .unwrap()
            .unwrap();
        assert_eq!(selected.owner_workspace_id, "remote-owner");
        assert_eq!(selected.default_cwd.as_deref(), Some(Path::new("/remote")));
    }

    #[test]
    fn explicit_node_placement_never_falls_back_to_local_creation() {
        assert_eq!(
            require_explicit_coordinated_owner(Some(7), Some("work"), true).unwrap(),
            Some(7)
        );
        let external = require_explicit_coordinated_owner::<()>(None, Some("work"), true)
            .unwrap_err()
            .to_string();
        assert!(external.contains("coordinated Workspace"));
        let legacy = require_explicit_coordinated_owner::<()>(None, Some("work"), false)
            .unwrap_err()
            .to_string();
        assert!(legacy.contains("protocol 38"));
        assert!(
            require_explicit_coordinated_owner::<()>(None, None, false)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn workspace_open_node_resolution_requires_exact_local_identity() {
        let local = combined_node("local-id", "local", true);
        let remote = combined_node("remote-id", "work", true);
        let nodes = [local, remote];
        assert!(resolve_workspace_open_node(&nodes, "local").is_err());
        let selected = resolve_workspace_open_node(&nodes, "local-id").unwrap();
        assert!(selected.local);
        assert_eq!(selected.node_id, "local-id");
        assert_eq!(
            resolve_workspace_open_node(&nodes, "work").unwrap().node_id,
            "remote-id"
        );
    }

    #[test]
    fn protocol_thirty_eight_shell_suggestion_identifies_its_exact_local_node() {
        let current = shell_suggestion_json("owner-id", "first", Some("local-node-id"));
        assert_eq!(current["node_id"], "local-node-id");
        assert_eq!(current["workspace_id"], "owner-id");
        let legacy = shell_suggestion_json("owner-id", "first", None);
        assert!(legacy.get("node_id").is_none());
    }

    #[test]
    fn parses_global_json_for_supported_integration_commands() {
        for arguments in [
            vec!["boomux", "--json", "capabilities"],
            vec!["boomux", "list", "--json"],
            vec!["boomux", "workspace", "list", "--json"],
            vec!["boomux", "shell", "suggest-name", "project", "--json"],
            vec![
                "boomux",
                "launcher",
                "list",
                "--workspace",
                "project",
                "--json",
            ],
            vec!["boomux", "daemon", "status", "--json"],
            vec!["boomux", "integration", "list", "--json"],
            vec!["boomux", "integration", "status", "opencode", "--json"],
            vec!["boomux", "integration", "install", "pi", "--json"],
        ] {
            let cli = Cli::try_parse_from(arguments).unwrap();
            assert!(cli.json);
            assert_eq!(cli.command_descriptor().output, OutputMode::Json);
        }
        let cli = Cli::try_parse_from(["boomux", "workspace", "create", "test", "--json"]).unwrap();
        assert_eq!(cli.command_descriptor().output, OutputMode::HumanOnly);
        let cli = Cli::try_parse_from([
            "boomux",
            "workspace",
            "create",
            "--node",
            "local-node",
            "--cwd",
            "/tmp",
            "--json",
        ])
        .unwrap();
        assert_eq!(cli.command_descriptor().key, "workspace.create");
        assert_eq!(cli.command_descriptor().output, OutputMode::Json);
        let cli = Cli::try_parse_from(["boomux", "opencode", "install", "--json"]).unwrap();
        assert_eq!(cli.command_descriptor().output, OutputMode::HumanOnly);
        assert!(
            Cli::try_parse_from([
                "boomux",
                "events",
                "--after",
                "00000000-0000-0000-0000-000000000000:4",
                "--wait-ms",
                "1000",
                "--json",
            ])
            .is_ok()
        );
        assert!(
            Cli::try_parse_from([
                "boomux",
                "read",
                "shell",
                "--run-id",
                "run",
                "--after-revision",
                "4",
                "--wait-ms",
                "1000",
            ])
            .is_ok()
        );
        assert!(Cli::try_parse_from(["boomux", "read", "shell", "--run-id", "run"]).is_err());
    }

    #[test]
    fn parses_exact_startup_command_after_separator() {
        let cli = Cli::try_parse_from([
            "boomux", "--name", "feature", ".", "--", "cargo", "watch", "-x", "test",
        ])
        .unwrap();

        assert_eq!(cli.path, Some(PathBuf::from(".")));
        assert_eq!(cli.startup_command, ["cargo", "watch", "-x", "test"]);
        assert!(cli.command.is_none());
        assert!(Cli::try_parse_from(["boomux", "--", "cargo", "watch"]).is_err());
    }

    #[test]
    fn detects_json_only_before_the_command_separator() {
        assert!(requests_json(
            ["workspace", "list", "--json"].map(OsString::from)
        ));
        assert!(!requests_json(
            [".", "--", "/bin/echo", "--json"].map(OsString::from)
        ));
    }

    #[test]
    fn parses_workspace_and_shell_lifecycle_commands() {
        for arguments in [
            ["boomux", "workspace", "select", "project"].as_slice(),
            ["boomux", "workspace", "current"].as_slice(),
            ["boomux", "workspace", "clear"].as_slice(),
        ] {
            let cli = Cli::try_parse_from(arguments).unwrap();
            assert_eq!(cli.command_descriptor().output, OutputMode::HumanOnly);
        }
        assert!(matches!(
            Cli::try_parse_from(["boomux", "workspace", "create", "project"])
                .unwrap()
                .command,
            Some(Commands::Workspace {
                command: WorkspaceCommands::Create { name: Some(name), node: None, .. }
            }) if name == "project"
        ));
        assert!(Cli::try_parse_from(["boomux", "workspace", "create"]).is_err());
        assert!(
            Cli::try_parse_from(["boomux", "workspace", "create", "project", "--cwd", "/tmp"])
                .is_err()
        );
        assert!(
            Cli::try_parse_from([
                "boomux",
                "workspace",
                "create",
                "project",
                "--node",
                "local-node"
            ])
            .is_err()
        );
        assert!(
            Cli::try_parse_from(["boomux", "workspace", "create", "project", "--open"]).is_err()
        );
        assert!(matches!(
            Cli::try_parse_from([
                "boomux",
                "workspace",
                "create",
                "--node",
                "local-node",
                "--cwd",
                "/tmp",
                "--open",
                "--json",
            ])
            .unwrap()
            .command,
            Some(Commands::Workspace {
                command: WorkspaceCommands::Create {
                    name: None,
                    node: Some(node),
                    cwd: Some(cwd),
                    open: true,
                }
            }) if node == "local-node" && cwd == Path::new("/tmp")
        ));
        let cli = Cli::try_parse_from([
            "boomux",
            "launcher",
            "create",
            "editor",
            "--workspace",
            "project",
            "--cwd",
            "/tmp",
            "--",
            "zeditor",
            ".",
        ])
        .unwrap();
        assert!(matches!(
            cli.command,
            Some(Commands::Launcher {
                command: LauncherCommands::Create {
                    name,
                    workspace,
                    cwd,
                    command,
                    ..
                }
            }) if name == "editor"
                && workspace.as_deref() == Some("project")
                && cwd == Path::new("/tmp")
                && command == ["zeditor", "."]
        ));
        let cli = Cli::try_parse_from([
            "boomux",
            "launcher",
            "invoke",
            "editor",
            "--workspace",
            "project",
        ])
        .unwrap();
        assert_eq!(cli.command_descriptor().output, OutputMode::HumanOnly);
        assert!(matches!(
            cli.command,
            Some(Commands::Launcher {
                command: LauncherCommands::Invoke {
                    target,
                    workspace: Some(workspace),
                    ..
                }
            }) if target == "editor" && workspace == "project"
        ));
        assert!(matches!(
            Cli::try_parse_from(["boomux", "workspace", "open", "project"])
                .unwrap()
                .command,
            Some(Commands::Workspace {
                command: WorkspaceCommands::Open { target, .. }
            }) if target == "project"
        ));
        let cli = Cli::try_parse_from([
            "boomux", "shell", "create", "project", "--name", "tests", "--cwd", "/tmp", "--open",
            "--", "cargo", "test",
        ])
        .unwrap();
        assert!(matches!(
            cli.command,
            Some(Commands::Shell {
                command: ShellCommands::Create {
                    workspace,
                    name: Some(name),
                    cwd,
                    open,
                    command,
                    ..
                }
            }) if workspace.as_deref() == Some("project")
                && name == "tests"
                && cwd.as_deref() == Some(Path::new("/tmp"))
                && open
                && command == ["cargo", "test"]
        ));
        assert!(matches!(
            Cli::try_parse_from(["boomux", "shell", "suggest-name", "project"])
                .unwrap()
                .command,
            Some(Commands::Shell {
                command: ShellCommands::SuggestName { workspace, .. }
            }) if workspace.as_deref() == Some("project")
        ));
        assert!(matches!(
            Cli::try_parse_from(["boomux", "shell", "create", "project"])
                .unwrap()
                .command,
            Some(Commands::Shell {
                command: ShellCommands::Create { cwd: None, .. }
            })
        ));
        assert!(matches!(
            Cli::try_parse_from(["boomux", "shell", "create", "--name", "tests"])
                .unwrap()
                .command,
            Some(Commands::Shell {
                command: ShellCommands::Create {
                    workspace: None,
                    name: Some(name),
                    ..
                }
            }) if name == "tests"
        ));
        assert!(matches!(
            Cli::try_parse_from(["boomux", "launcher", "create", "editor", "--", "editor"])
                .unwrap()
                .command,
            Some(Commands::Launcher {
                command: LauncherCommands::Create {
                    workspace: None,
                    ..
                }
            })
        ));
    }

    #[test]
    fn parses_agent_runtime_commands_and_json_support() {
        let cli = Cli::try_parse_from([
            "boomux",
            "agent",
            "register",
            "opencode",
            "--integration",
            "plugin",
            "--shell-id",
            "s1",
            "--run-id",
            "r1",
            "--state",
            "working",
            "--authority",
            "lifecycle-integration",
            "--evidence",
            "tool call",
            "--confidence",
            "95",
        ])
        .unwrap();
        assert!(matches!(
            cli.command,
            Some(Commands::Agent {
                command: AgentCommands::Register(AgentRegistrationArgs { confidence: 95, .. })
            })
        ));

        let ensure = Cli::try_parse_from([
            "boomux",
            "agent",
            "ensure",
            "OpenCode",
            "--integration",
            "opencode",
            "--external-session-id",
            "session-1",
            "--shell-id",
            "s1",
            "--run-id",
            "r1",
            "--state",
            "working",
            "--authority",
            "lifecycle-integration",
            "--evidence",
            "tool call",
            "--confidence",
            "100",
            "--json",
        ])
        .unwrap();
        assert_eq!(ensure.command_descriptor().key, "agent.ensure");
        assert_eq!(ensure.command_descriptor().output, OutputMode::Json);

        let claim = Cli::try_parse_from([
            "boomux",
            "opencode",
            "claim",
            "ensure",
            "--generation",
            "generation-1",
            "--holder",
            "holder-1",
            "--root-session-id",
            "ses_exact",
            "--shell-id",
            "s1",
            "--run-id",
            "r1",
            "--json",
        ])
        .unwrap();
        assert_eq!(claim.command_descriptor().key, "opencode.claim.ensure");
        assert_eq!(claim.command_descriptor().output, OutputMode::PrivateJson);
        assert!(matches!(
            claim.command,
            Some(Commands::Opencode {
                command: OpenCodeCommands::Claim {
                    command: OpenCodeClaimCommands::Ensure {
                        generation,
                        holder,
                        root_session_id,
                        shell_id: Some(shell_id),
                        run_id: Some(run_id),
                    },
                },
            }) if generation == "generation-1" && holder == "holder-1"
                && root_session_id == "ses_exact" && shell_id == "s1" && run_id == "r1"
        ));
        for (arguments, key) in [
            (
                vec![
                    "boomux",
                    "opencode",
                    "claim",
                    "release",
                    "--generation",
                    "generation-1",
                    "--holder",
                    "holder-1",
                    "--claim-id",
                    "claim-1",
                    "--json",
                ],
                "opencode.claim.release",
            ),
            (
                vec![
                    "boomux",
                    "opencode",
                    "claim",
                    "report",
                    "--generation",
                    "generation-1",
                    "--root-session-id",
                    "ses_exact",
                    "--state",
                    "idle",
                    "--authority",
                    "lifecycle-integration",
                    "--evidence",
                    "session.idle",
                    "--confidence",
                    "100",
                    "--json",
                ],
                "opencode.claim.report",
            ),
        ] {
            let command = Cli::try_parse_from(arguments).unwrap();
            assert_eq!(command.command_descriptor().key, key);
            assert_eq!(command.command_descriptor().output, OutputMode::PrivateJson);
        }

        let cli = Cli::try_parse_from([
            "boomux",
            "agent",
            "list",
            "--workspace",
            "project",
            "--json",
        ])
        .unwrap();
        assert_eq!(cli.command_descriptor().output, OutputMode::Json);
        let cli = Cli::try_parse_from(["boomux", "agent", "get", "a1", "--json"]).unwrap();
        assert_eq!(cli.command_descriptor().key, "agent.inspect");
        assert_eq!(cli.command_descriptor().output, OutputMode::Json);
        let cli = Cli::try_parse_from([
            "boomux",
            "agent",
            "wait",
            "a1",
            "--after-revision",
            "3",
            "--wait-ms",
            "5000",
            "--json",
        ])
        .unwrap();
        assert_eq!(cli.command_descriptor().key, "agent.wait");
        assert_eq!(cli.command_descriptor().output, OutputMode::Json);
        assert!(matches!(
            cli.command,
            Some(Commands::Agent {
                command: AgentCommands::Wait {
                    agent_id,
                    after_revision: 3,
                    wait_ms: 5000,
                }
            }) if agent_id == "a1"
        ));
        let cli = Cli::try_parse_from([
            "boomux",
            "agent",
            "report",
            "a1",
            "--state",
            "done",
            "--authority",
            "lifecycle-integration",
            "--evidence",
            "complete",
            "--confidence",
            "100",
            "--json",
        ])
        .unwrap();
        assert_eq!(cli.command_descriptor().key, "agent.report");
        assert_eq!(cli.command_descriptor().output, OutputMode::Json);
        let register = Cli::try_parse_from([
            "boomux",
            "agent",
            "register",
            "agent",
            "--integration",
            "test",
            "--state",
            "working",
            "--authority",
            "process-adapter",
            "--evidence",
            "running",
            "--confidence",
            "80",
            "--json",
        ])
        .unwrap();
        assert_eq!(register.command_descriptor().key, "agent.register");
        assert_eq!(register.command_descriptor().output, OutputMode::Json);
        let unnamed_register = Cli::try_parse_from([
            "boomux",
            "agent",
            "register",
            "--integration",
            "test",
            "--state",
            "working",
            "--authority",
            "process-adapter",
            "--evidence",
            "running",
            "--confidence",
            "80",
        ])
        .unwrap();
        assert!(matches!(
            unnamed_register.command,
            Some(Commands::Agent {
                command: AgentCommands::Register(AgentRegistrationArgs { name: None, .. })
            })
        ));
        assert!(
            Cli::try_parse_from([
                "boomux",
                "agent",
                "report",
                "a1",
                "--state",
                "done",
                "--authority",
                "daemon-lifecycle",
                "--evidence",
                "complete",
                "--confidence",
                "100",
            ])
            .is_err()
        );
        assert!(
            Cli::try_parse_from([
                "boomux",
                "agent",
                "report",
                "a1",
                "--state",
                "done",
                "--authority",
                "lifecycle-integration",
                "--evidence",
                "complete",
                "--confidence",
                "101",
            ])
            .is_err()
        );

        let supervise = Cli::try_parse_from([
            "boomux",
            "agent",
            "supervise",
            "OpenCode",
            "--integration",
            "opencode",
            "--external-session-id",
            "session-1",
            "--shell-id",
            "s1",
            "--run-id",
            "r1",
            "--",
            "agent-bin",
            "literal; argument",
        ])
        .unwrap();
        assert_eq!(supervise.command_descriptor().key, "agent.supervise");
        assert_eq!(supervise.command_descriptor().output, OutputMode::HumanOnly);
        assert!(matches!(
            supervise.command,
            Some(Commands::Agent {
                command: AgentCommands::Supervise(AgentSuperviseArgs { command, .. })
            }) if command == ["agent-bin", "literal; argument"]
        ));
        let unnamed_supervise = Cli::try_parse_from([
            "boomux",
            "agent",
            "supervise",
            "--integration",
            "opencode",
            "--external-session-id",
            "session-1",
            "--shell-id",
            "s1",
            "--run-id",
            "r1",
            "--",
            "agent-bin",
        ])
        .unwrap();
        assert!(matches!(
            unnamed_supervise.command,
            Some(Commands::Agent {
                command: AgentCommands::Supervise(AgentSuperviseArgs { name: None, .. })
            })
        ));
        assert!(
            Cli::try_parse_from([
                "boomux",
                "agent",
                "supervise",
                "agent",
                "--integration",
                "test",
                "--external-session-id",
                "session-1",
            ])
            .is_err()
        );
        assert!(NON_PROTOCOL_FEATURES.contains(&"process_adapters"));
    }

    #[test]
    fn parses_attention_queue_commands_and_json_support() {
        let list = Cli::try_parse_from([
            "boomux",
            "attention",
            "list",
            "--workspace",
            "project",
            "--json",
        ])
        .unwrap();
        assert_eq!(list.command_descriptor().key, "attention.list");
        assert_eq!(list.command_descriptor().output, OutputMode::Json);

        let acknowledge = Cli::try_parse_from([
            "boomux",
            "attention",
            "acknowledge",
            "a1",
            "--observation-revision",
            "7",
            "--json",
        ])
        .unwrap();
        assert_eq!(
            acknowledge.command_descriptor().key,
            "attention.acknowledge"
        );
        assert_eq!(acknowledge.command_descriptor().output, OutputMode::Json);
        assert_eq!(
            sanitize_table_cell("approval\tneeded\nnow\u{7}"),
            "approval needed now "
        );
    }

    #[test]
    fn parses_notification_test_commands_without_json_support() {
        let blocked = Cli::try_parse_from(["boomux", "notification", "test", "blocked"]).unwrap();
        assert_eq!(blocked.command_descriptor().key, "notification.test");
        assert_eq!(blocked.command_descriptor().output, OutputMode::HumanOnly);
        assert!(matches!(
            blocked.command,
            Some(Commands::Notification {
                command: NotificationCommands::Test {
                    reason: CliNotificationReason::Blocked
                }
            })
        ));

        assert!(Cli::try_parse_from(["boomux", "notification", "test", "unknown"]).is_err());
    }

    #[test]
    fn child_exit_outcomes_map_to_unix_cli_codes() {
        assert_eq!(
            CliExit::Child(process_adapter::ProcessExit::Code(23)).code(),
            ExitCode::from(23)
        );
        assert_eq!(
            CliExit::Child(process_adapter::ProcessExit::Signal(9)).code(),
            ExitCode::from(137)
        );
    }

    #[test]
    fn parses_legacy_session_arguments_for_typed_rejection() {
        let list = Cli::try_parse_from([
            "boomux",
            "session",
            "list",
            "--workspace",
            "project",
            "--json",
        ])
        .unwrap();
        assert_eq!(list.command_descriptor().key, "session.list");
        assert_eq!(list.command_descriptor().output, OutputMode::Json);
        assert!(matches!(
            list.command,
            Some(Commands::Session {
                command: SessionCommands::List {
                    workspace: Some(workspace),
                    ..
                }
            }) if workspace == "project"
        ));

        let inspect =
            Cli::try_parse_from(["boomux", "session", "get", "opaque", "--json"]).unwrap();
        assert_eq!(inspect.command_descriptor().key, "session.inspect");
        assert_eq!(inspect.command_descriptor().output, OutputMode::Json);

        let open = Cli::try_parse_from([
            "boomux",
            "session",
            "open",
            "opaque",
            "--node",
            "workbox",
            "--workspace",
            "active",
        ])
        .unwrap();
        assert_eq!(open.command_descriptor().key, "session.open");
        assert_eq!(open.command_descriptor().output, OutputMode::HumanOnly);
        assert!(matches!(
            open.command,
            Some(Commands::Session {
                command: SessionCommands::Open { session_id, node, workspace }
            }) if session_id == "opaque" && node.as_deref() == Some("workbox")
                && workspace.as_deref() == Some("active")
        ));

        let json_open =
            Cli::try_parse_from(["boomux", "--json", "session", "open", "opaque"]).unwrap();
        let error = run(json_open).unwrap_err();
        assert_eq!(
            cli_output::classify_for_test("session.open", error.as_ref()),
            "unsupported_version"
        );

        let rename = Cli::try_parse_from([
            "boomux",
            "session",
            "rename",
            "opaque",
            "Checkout retry investigation",
            "--revision",
            "11",
            "--node",
            "workbox",
            "--json",
        ])
        .unwrap();
        assert_eq!(rename.command_descriptor().key, "session.rename");
        assert!(matches!(
            rename.command,
            Some(Commands::Session {
                command: SessionCommands::Rename {
                    session_id,
                    name,
                    revision: 11,
                    node: Some(node),
                    ..
                }
            }) if session_id == "opaque"
                && name == "Checkout retry investigation"
                && node == "workbox"
        ));
        let reset = Cli::try_parse_from([
            "boomux",
            "session",
            "reset-name",
            "opaque",
            "--revision",
            "12",
            "--json",
        ])
        .unwrap();
        assert_eq!(reset.command_descriptor().key, "session.reset-name");

        let hide = Cli::try_parse_from([
            "boomux",
            "session",
            "hide",
            "opaque",
            "--workspace",
            "workspace-id",
            "--node",
            "workbox",
            "--json",
        ])
        .unwrap();
        assert_eq!(hide.command_descriptor().key, "session.hide");
        assert!(matches!(
            hide.command,
            Some(Commands::Session {
                command: SessionCommands::Hide {
                    session_id,
                    workspace,
                    node: Some(node),
                }
            }) if session_id == "opaque" && workspace == "workspace-id" && node == "workbox"
        ));
        assert!(Cli::try_parse_from(["boomux", "session", "hide", "opaque"]).is_err());

        assert!(Cli::try_parse_from(["boomux", "session", "read", "opaque"]).is_err());
    }

    #[test]
    fn session_commands_are_not_advertised() {
        let session_help = match Cli::try_parse_from(["boomux", "session", "--help"]) {
            Err(error) => error.to_string(),
            Ok(_) => panic!("Session help must exit before command dispatch"),
        };
        for command in ["open", "list", "rename"] {
            assert!(
                !session_help
                    .lines()
                    .any(|line| line.trim_start().starts_with(&format!("{command} ")))
            );
        }
    }

    #[test]
    fn session_commands_require_protocol_twelve() {
        let error = validate_session_protocol(11).unwrap_err();
        assert_eq!(
            cli_output::classify_for_test("session.list", error.as_ref()),
            "unsupported_version"
        );
        assert!(validate_session_protocol(12).is_ok());
    }

    #[test]
    fn agent_context_requires_nonempty_shell_and_run_ids() {
        assert_eq!(
            resolve_agent_context(
                Some(" explicit-shell ".into()),
                None,
                Some("environment-shell".into()),
                Some(" environment-run ".into()),
            )
            .unwrap(),
            ("explicit-shell".into(), "environment-run".into())
        );
        assert!(resolve_agent_context(Some("s1".into()), None, None, None).is_err());
        assert!(
            resolve_agent_context(Some(" ".into()), Some("r1".into()), Some("s1".into()), None,)
                .is_err()
        );
    }

    #[test]
    fn parses_hidden_claude_hook_command() {
        let cli = Cli::try_parse_from(["boomux", "claude", "hook"]).unwrap();
        assert!(matches!(
            cli.command,
            Some(Commands::Claude {
                command: ClaudeCommands::Hook,
            })
        ));
        assert_eq!(cli.command_descriptor().key, "claude.hook");
        assert_eq!(cli.command_descriptor().output, OutputMode::HumanOnly);
    }

    #[test]
    fn parses_hidden_codex_hook_command() {
        let cli = Cli::try_parse_from(["boomux", "codex", "hook"]).unwrap();
        assert!(matches!(
            cli.command,
            Some(Commands::Codex {
                command: CodexCommands::Hook,
            })
        ));
        assert_eq!(cli.command_descriptor().key, "codex.hook");
        assert_eq!(cli.command_descriptor().output, OutputMode::HumanOnly);
    }

    #[test]
    fn parses_hidden_codex_launch_command_without_interpreting_arguments() {
        let cli = Cli::try_parse_from([
            "boomux",
            "codex",
            "launch",
            "--",
            "resume",
            "thread; literal",
        ])
        .unwrap();
        assert!(matches!(
            cli.command,
            Some(Commands::Codex {
                command: CodexCommands::Launch { ref arguments },
            }) if arguments.as_slice()
                == [OsString::from("resume"), OsString::from("thread; literal")].as_slice()
        ));
        assert_eq!(cli.command_descriptor().key, "codex.launch");
    }

    #[test]
    fn parses_hidden_kiro_commands_without_interpreting_arguments() {
        let hook = Cli::try_parse_from(["boomux", "kiro", "hook"]).unwrap();
        assert!(matches!(
            hook.command,
            Some(Commands::Kiro {
                command: KiroCommands::Hook,
            })
        ));
        assert_eq!(hook.command_descriptor().key, "kiro.hook");

        let launch = Cli::try_parse_from([
            "boomux",
            "kiro",
            "launch",
            "--",
            "--v3",
            "chat",
            "session; literal",
        ])
        .unwrap();
        assert!(matches!(
            launch.command,
            Some(Commands::Kiro {
                command: KiroCommands::Launch { ref arguments },
            }) if arguments.as_slice()
                == [
                    OsString::from("--v3"),
                    OsString::from("chat"),
                    OsString::from("session; literal"),
                ].as_slice()
        ));
        assert_eq!(launch.command_descriptor().key, "kiro.launch");
    }

    #[test]
    fn kiro_supervised_argv_and_exit_status_remain_exact() {
        assert_eq!(
            kiro_argv(
                "/exact/kiro-cli".into(),
                vec!["chat".into(), "literal; value".into()],
                false,
            ),
            ["/exact/kiro-cli", "chat", "literal; value"].map(OsString::from)
        );
        assert_eq!(
            kiro_argv("/exact/kiro-cli".into(), Vec::new(), true),
            ["/exact/kiro-cli", "--v3"].map(OsString::from)
        );
        let status = Command::new("/bin/sh")
            .args(["-c", "exit 23"])
            .status()
            .unwrap();
        assert_eq!(
            process_exit_from_status(status),
            process_adapter::ProcessExit::Code(23)
        );
        let status = Command::new("/bin/sh")
            .args(["-c", "kill -TERM $$"])
            .status()
            .unwrap();
        assert_eq!(
            process_exit_from_status(status),
            process_adapter::ProcessExit::Signal(libc::SIGTERM)
        );
    }

    #[test]
    fn event_snapshot_json_includes_stable_agent_data() {
        let mut project = workspace("w1", "project", vec![shell("s1", "w1", "shell")]);
        project.agents.push(agent("a1", "w1", "s1"));

        let value = json_snapshot(Snapshot {
            workspaces: vec![project],
            focused_terminal: None,
        })
        .unwrap();

        assert_eq!(value["workspaces"][0]["agents"][0]["id"], "a1");
        assert_eq!(
            value["workspaces"][0]["agents"][0]["observation"]["authority"],
            "lifecycle_integration"
        );
    }

    #[test]
    fn resolves_workspaces_and_shell_names_for_cli_commands() {
        let mut project = workspace("w1", "project", vec![shell("s1", "w1", "tests")]);
        project.launchers.push(launcher("l1", "w1", "editor"));
        let snapshot = Snapshot {
            workspaces: vec![workspace("w2", "w1", vec![]), project],
            focused_terminal: None,
        };

        assert_eq!(
            resolve_workspace_target(&snapshot.workspaces, "project")
                .unwrap()
                .id,
            "w1"
        );
        assert_eq!(
            resolve_workspace_target(&snapshot.workspaces, "w1")
                .unwrap()
                .name,
            "project"
        );
        assert_eq!(
            resolve_cli_shell(&snapshot, "tests", Some("project"))
                .unwrap()
                .id,
            "s1"
        );
        assert_eq!(
            resolve_cli_launcher(&snapshot, "editor", Some("project"))
                .unwrap()
                .id,
            "l1"
        );
        assert_eq!(
            resolve_cli_launcher(&snapshot, "l1", None).unwrap().name,
            "editor"
        );
        assert!(resolve_workspace_target(&snapshot.workspaces, "missing").is_err());
    }

    #[test]
    fn persisted_workspace_selection_resolves_only_exact_global_id() {
        let stale_id = Uuid::from_u128(1).to_string();
        let workspace = protocol::GlobalWorkspaceSnapshot {
            id: Uuid::from_u128(2).to_string(),
            revision: 1,
            name: stale_id.clone(),
            closing: false,
            placements: Vec::new(),
        };

        assert!(
            resolve_global_workspace_target(std::slice::from_ref(&workspace), &stale_id).is_some()
        );
        assert!(find_global_workspace_by_id(&[workspace], &stale_id).is_none());
    }

    #[test]
    fn matches_native_workspace_by_name_only() {
        let workspace = workspace("w1", "project", vec![]);
        assert!(find_workspace(&[workspace], "project").is_some());
    }

    #[test]
    fn resolves_explicit_and_generated_cli_names() {
        assert_eq!(
            cli_name_or_generated(Some("  explicit  ".into()), "agent").unwrap(),
            "explicit"
        );
        let generated = cli_name_or_generated(None, "agent").unwrap();
        let (adjective, noun) = generated.split_once('-').unwrap();
        assert!(adjective.bytes().all(|byte| byte.is_ascii_lowercase()));
        assert!(noun.bytes().all(|byte| byte.is_ascii_lowercase()));
    }

    #[test]
    fn generated_shell_name_exhaustion_is_typed() {
        let mut names = Vec::new();
        while let Some(name) = generated_names::random_excluding(names.iter().map(String::as_str)) {
            names.push(name);
        }

        let error = generated_shell_name(names.iter().map(String::as_str)).unwrap_err();
        assert_eq!(
            cli_output::classify_for_test("shell.suggest-name", error.as_ref()),
            "already_exists"
        );
    }

    #[test]
    fn resolves_shell_ids_and_contextual_names() {
        let snapshot = Snapshot {
            workspaces: vec![
                workspace("w1", "one", vec![shell("s1", "w1", "tests")]),
                workspace("w2", "two", vec![shell("s2", "w2", "tests")]),
            ],
            focused_terminal: None,
        };
        assert_eq!(
            resolve_shell_target(&snapshot, None, "s2").unwrap().id,
            "s2"
        );
        assert_eq!(
            resolve_shell_target(&snapshot, Some("w1"), "tests")
                .unwrap()
                .id,
            "s1"
        );
        assert!(resolve_shell_target(&snapshot, None, "tests").is_err());
    }

    #[test]
    fn builds_shell_spec_with_exact_startup_arguments() {
        let command = vec!["sh".into(), "-lc".into(), "printf '%s' test".into()];

        let spec = shell_spec("checks", Path::new("/tmp/project"), &command);

        assert_eq!(spec.name, "checks");
        assert_eq!(spec.cwd, Path::new("/tmp/project"));
        assert_eq!(spec.command, command);
    }

    #[test]
    fn empty_workspace_view_has_no_shells() {
        let views = dashboard_views(
            &[workspace("w1", "empty", Vec::new())],
            &mut git::Cache::default(),
        );

        assert!(views[0].items.is_empty());
        assert!(views[0].sessions.is_empty());
    }

    #[test]
    fn workspace_view_groups_external_session_occurrences_across_runs() {
        let mut second_shell = shell("s2", "w1", "review");
        second_shell.run.as_mut().unwrap().id = "r2".into();
        let mut workspace = workspace(
            "w1",
            "project",
            vec![shell("s1", "w1", "build"), second_shell],
        );
        let mut first = agent("agent-old", "w1", "s1");
        first.started_at_ms = 10;
        first.ended_at_ms = Some(20);
        first.observation.state = AgentState::Inactive;
        first.observation.observed_at_ms = 20;
        let mut second = agent("agent-new", "w1", "s2");
        second.run_id = "r2".into();
        second.started_at_ms = 30;
        second.observation.state = AgentState::Blocked;
        second.observation.evidence = "approval needed".into();
        second.observation.observed_at_ms = 40;
        workspace.agents = vec![second, first];

        let views = dashboard_views(&[workspace], &mut git::Cache::default());

        assert_eq!(views[0].sessions.len(), 1);
        let session = &views[0].sessions[0];
        assert!(Uuid::parse_str(&session.id).is_ok());
        assert_eq!(session.state, tui::AgentDisplayState::Blocked);
        assert!(session.state_is_current);
        assert_eq!(session.runs.len(), 2);
        assert_eq!(session.runs[0].shell_name.as_deref(), Some("build"));
        assert_eq!(session.runs[1].shell_name.as_deref(), Some("review"));
        assert_eq!(
            session.runs[1].directory.as_deref(),
            Some(Path::new("/tmp/project"))
        );
    }

    #[test]
    fn enriches_only_the_matching_integration_and_external_session() {
        let mut workspace = workspace("w1", "project", vec![shell("s1", "w1", "agent")]);
        workspace.agents = vec![agent("agent-1", "w1", "s1")];
        let mut views = dashboard_views(&[workspace], &mut git::Cache::default());

        dashboard_projection::enrich_session_titles_with(
            &mut views,
            |integration, directory, external_id| {
                (integration == "opencode"
                    && directory == Path::new("/tmp/project")
                    && external_id == "external-1")
                    .then(|| "Review async title cache".into())
            },
        );

        assert_eq!(views[0].sessions[0].label, "Review async title cache");
    }

    #[test]
    fn enriches_from_the_newest_available_session_directory() {
        let mut views = vec![tui::WorkspaceView {
            node: tui::NodeView {
                id: String::new(),
                alias: "local".into(),
                local: true,
                route: None,
                registration_revision: None,
                health: protocol::NodeProjectionHealthCode::Online,
                current: true,
                stale: false,
                observed_at_ms: 0,
                observed_protocol_version: Some(protocol::PROTOCOL_VERSION),
                observed_helper_version: Some(env!("CARGO_PKG_VERSION").into()),
                observed_capabilities: Vec::new(),
                workspace_owner_eligible: true,
                workspace_owner_unavailable_reason: None,
            },
            id: "w1".into(),
            name: "project".into(),
            default_cwd: None,
            items: Vec::new(),
            agent_state_counts: agent_attention_projection::AgentStateCounts::default(),
            attention_count: 0,
            attention: Vec::new(),
            item_owners: Vec::new(),
            coordination: tui::WorkspaceCoordinationView::External {
                owner_revision: 1,
                available: true,
            },
            sessions: vec![tui::AgentSessionView {
                id: "session".into(),
                label: "opencode".into(),
                integration: "opencode".into(),
                external_session_id: Some("external-1".into()),
                state: tui::AgentDisplayState::Inactive,
                state_is_current: false,
                last_at_ms: 30,
                source_cwd: Some("/tmp/project".into()),
                runs: vec![
                    tui::AgentSessionRunView {
                        agent_id: "old-agent".into(),
                        shell_name: Some("old-shell".into()),
                        directory: Some("/tmp/project".into()),
                    },
                    tui::AgentSessionRunView {
                        agent_id: "new-agent".into(),
                        shell_name: None,
                        directory: None,
                    },
                ],
            }],
        }];

        dashboard_projection::enrich_session_titles_with(&mut views, |_, directory, _| {
            (directory == Path::new("/tmp/project")).then(|| "Historical title".into())
        });

        assert_eq!(views[0].sessions[0].label, "Historical title");
    }

    #[test]
    fn workspace_view_keeps_agents_without_external_ids_isolated() {
        let mut workspace = workspace("w1", "project", vec![shell("s1", "w1", "agent")]);
        let mut first = agent("agent-a", "w1", "s1");
        first.external_session_id = None;
        let mut second = agent("agent-b", "w1", "s1");
        second.external_session_id = None;
        workspace.agents = vec![first, second];

        let views = dashboard_views(&[workspace], &mut git::Cache::default());

        assert_eq!(views[0].sessions.len(), 2);
        assert!(
            views[0]
                .sessions
                .iter()
                .all(|session| session.runs.len() == 1)
        );
    }

    #[test]
    fn workspace_session_state_prioritizes_active_blocked_occurrences() {
        let mut workspace = workspace("w1", "project", vec![shell("s1", "w1", "agent")]);
        let mut working = agent("working", "w1", "s1");
        working.observation.observed_at_ms = 30;
        let mut blocked = agent("blocked", "w1", "s1");
        blocked.observation.state = AgentState::Blocked;
        blocked.observation.observed_at_ms = 20;
        workspace.agents = vec![working, blocked];

        let views = dashboard_views(&[workspace], &mut git::Cache::default());

        assert_eq!(views[0].sessions[0].state, tui::AgentDisplayState::Blocked);
    }

    #[test]
    fn workspace_session_state_ignores_stale_blocked_occurrence() {
        let mut current_shell = shell("s1", "w1", "agent");
        current_shell.run.as_mut().unwrap().id = "current-run".into();
        let mut workspace = workspace("w1", "project", vec![current_shell]);
        let mut stale = agent("stale", "w1", "s1");
        stale.run_id = "old-run".into();
        stale.observation.state = AgentState::Blocked;
        stale.observation.observed_at_ms = 30;
        let mut current = agent("current", "w1", "s1");
        current.run_id = "current-run".into();
        current.observation.state = AgentState::Working;
        current.observation.observed_at_ms = 20;
        workspace.agents = vec![stale, current];

        let views = dashboard_views(&[workspace], &mut git::Cache::default());

        assert_eq!(views[0].sessions[0].state, tui::AgentDisplayState::Working);
        assert!(views[0].sessions[0].state_is_current);
    }

    #[test]
    fn workspace_session_state_marks_stale_history_as_last_known() {
        let mut workspace = workspace("w1", "project", vec![shell("s1", "w1", "agent")]);
        let mut stale = agent("stale", "w1", "s1");
        stale.run_id = "old-run".into();
        stale.observation.state = AgentState::Blocked;
        workspace.agents = vec![stale];

        let views = dashboard_views(&[workspace], &mut git::Cache::default());

        assert_eq!(views[0].sessions[0].state, tui::AgentDisplayState::Blocked);
        assert!(!views[0].sessions[0].state_is_current);
    }

    #[test]
    fn workspace_session_id_is_stable_when_equal_timestamp_occurrence_is_added() {
        let mut workspace = workspace("w1", "project", vec![shell("s1", "w1", "agent")]);
        let mut first = agent("agent-z", "w1", "s1");
        first.started_at_ms = 10;
        workspace.agents = vec![first.clone()];
        let initial = session_projection::project_workspaces(std::slice::from_ref(&workspace));
        let initial_id = dashboard_projection::session_views(&initial)[0].id.clone();

        let mut added = agent("agent-a", "w1", "s1");
        added.started_at_ms = 10;
        workspace.agents.push(added);
        let projected = session_projection::project_workspaces(std::slice::from_ref(&workspace));
        let sessions = dashboard_projection::session_views(&projected);

        assert_eq!(sessions[0].id, initial_id);
        assert_eq!(sessions[0].runs.len(), 2);
        assert!(Uuid::parse_str(&sessions[0].id).is_ok());
    }

    #[test]
    fn workspace_session_catalog_retains_inactive_and_done_sessions() {
        let mut workspace = workspace("w1", "project", vec![]);
        let mut inactive = agent("inactive", "w1", "removed-a");
        inactive.external_session_id = Some("inactive-session".into());
        inactive.ended_at_ms = Some(20);
        inactive.observation.state = AgentState::Inactive;
        inactive.observation.observed_at_ms = 20;
        let mut done = agent("done", "w1", "removed-b");
        done.external_session_id = Some("done-session".into());
        done.ended_at_ms = Some(30);
        done.observation.state = AgentState::Done;
        done.observation.observed_at_ms = 30;
        workspace.agents = vec![inactive, done];

        let views = dashboard_views(&[workspace], &mut git::Cache::default());

        assert_eq!(views[0].sessions.len(), 2);
        assert!(
            views[0]
                .sessions
                .iter()
                .any(|session| session.state == tui::AgentDisplayState::Inactive)
        );
        assert!(
            views[0]
                .sessions
                .iter()
                .any(|session| session.state == tui::AgentDisplayState::Done)
        );
        assert!(
            views[0]
                .sessions
                .iter()
                .all(|session| session.runs[0].shell_name.is_none())
        );
    }

    #[test]
    fn workspace_view_includes_launcher_details() {
        let mut workspace = workspace("w1", "project", Vec::new());
        workspace.launchers.push(launcher("l1", "w1", "editor"));

        let views = dashboard_views(&[workspace], &mut git::Cache::default());

        let tui::WorkspaceItemView::Launcher(launcher) = &views[0].items[0] else {
            panic!("expected launcher item");
        };
        assert_eq!(launcher.name, "editor");
        assert_eq!(launcher.command, "zeditor .");
        assert_eq!(launcher.argv, ["zeditor", "."]);
        assert_eq!(launcher.directory, "/tmp/project");
    }

    #[test]
    fn workspace_view_classifies_stored_argv_as_a_command() {
        let mut command = shell("s1", "w1", "clock");
        command.command = vec!["watch".into(), "-n".into(), "1".into(), "date".into()];
        let workspace = workspace("w1", "project", vec![command]);

        let views = dashboard_views(&[workspace], &mut git::Cache::default());

        let tui::WorkspaceItemView::Shell(command) = &views[0].items[0] else {
            panic!("expected command-backed shell item");
        };
        assert_eq!(command.name, "clock");
        assert_eq!(command.command, "watch -n 1 date");
        assert_eq!(command.argv, ["watch", "-n", "1", "date"]);
        let run = command.run.as_ref().expect("current run metadata");
        assert_eq!(run.id, "r1");
        assert_eq!(run.generation, 1);
    }

    #[test]
    fn workspace_view_morphs_shell_to_agent_shell_without_adding_a_row() {
        let mut agent_command = shell("s1", "w1", "terminal");
        agent_command.command = vec!["opencode".into()];
        let mut workspace = workspace("w1", "project", vec![agent_command]);
        workspace.launchers.push(launcher("l1", "w1", "editor"));
        workspace.agents.push(agent("a1", "w1", "s1"));

        let views = dashboard_views(&[workspace], &mut git::Cache::default());

        assert_eq!(views[0].items.len(), 2);
        let tui::WorkspaceItemView::AgentShell(tui::AgentShellView { shell, agent, .. }) =
            &views[0].items[0]
        else {
            panic!("expected agent-shell item");
        };
        assert_eq!(shell.id, "s1");
        assert_eq!(shell.name, "terminal");
        assert_eq!(shell.status, "running");
        assert_eq!(shell.directory, "/tmp/project");
        assert_eq!(shell.branch, "-");
        assert_eq!(shell.command, "opencode");
        let agent = agent.as_ref().expect("durable agent");
        assert_eq!(agent.id, "a1");
        assert_eq!(agent.state, tui::AgentDisplayState::Working);
        assert_eq!(
            agent.authority,
            tui::AgentAuthorityDisplay::LifecycleIntegration
        );
        assert_eq!(agent.confidence, 95);
        assert_eq!(agent.updated_at_ms, 11);
        assert_eq!(agent.root_branch, "-");
        assert_eq!(agent.root_worktree, "-");
        assert_eq!(views[0].sessions[0].runs[0].agent_id, "a1");
        assert!(matches!(
            views[0].items[1],
            tui::WorkspaceItemView::Launcher(_)
        ));
    }

    #[test]
    fn workspace_view_hints_exact_opencode_foreground_process_before_first_prompt() {
        let mut hinted = shell("s1", "w1", "terminal");
        hinted.foreground_process = Some("opencode".into());
        let workspace = workspace("w1", "project", vec![hinted]);

        let views = dashboard_views(&[workspace], &mut git::Cache::default());

        let tui::WorkspaceItemView::AgentShell(item) = &views[0].items[0] else {
            panic!("expected hinted agent-shell item");
        };
        assert_eq!(item.shell.id, "s1");
        assert_eq!(item.shell.status, "running");
        assert_eq!(item.shell.name, "terminal");
        assert!(item.agent.is_none());
    }

    #[test]
    fn workspace_view_hints_exact_pi_foreground_process_before_first_prompt() {
        let mut hinted = shell("s1", "w1", "terminal");
        hinted.foreground_process = Some("pi".into());
        let workspace = workspace("w1", "project", vec![hinted]);

        let views = dashboard_views(&[workspace], &mut git::Cache::default());

        let tui::WorkspaceItemView::AgentShell(item) = &views[0].items[0] else {
            panic!("expected hinted agent-shell item");
        };
        assert_eq!(item.shell.name, "terminal");
        assert!(item.agent.is_none());
    }

    #[test]
    fn workspace_view_hints_exact_codex_foreground_process_before_first_prompt() {
        let mut hinted = shell("s1", "w1", "terminal");
        hinted.foreground_process = Some("codex".into());
        let workspace = workspace("w1", "project", vec![hinted]);

        let views = dashboard_views(&[workspace], &mut git::Cache::default());

        let tui::WorkspaceItemView::AgentShell(item) = &views[0].items[0] else {
            panic!("expected hinted agent-shell item");
        };
        assert_eq!(item.shell.name, "terminal");
        assert!(item.agent.is_none());
    }

    #[test]
    fn workspace_view_hints_exact_kiro_foreground_process_before_first_prompt() {
        let mut hinted = shell("s1", "w1", "terminal");
        hinted.foreground_process = Some("kiro-cli".into());
        let workspace = workspace("w1", "project", vec![hinted]);

        let views = dashboard_views(&[workspace], &mut git::Cache::default());

        let tui::WorkspaceItemView::AgentShell(item) = &views[0].items[0] else {
            panic!("expected hinted agent-shell item");
        };
        assert_eq!(item.shell.name, "terminal");
        assert!(item.agent.is_none());
    }

    #[test]
    fn workspace_view_does_not_hint_an_inactive_pi_session() {
        let mut hinted = shell("s1", "w1", "terminal");
        hinted.foreground_process = Some("pi".into());
        let mut workspace = workspace("w1", "project", vec![hinted]);
        let mut inactive = agent("inactive", "w1", "s1");
        inactive.name = "Pi".into();
        inactive.integration = "pi".into();
        inactive.observation.state = AgentState::Inactive;
        workspace.agents.push(inactive);

        let views = dashboard_views(&[workspace], &mut git::Cache::default());

        assert!(matches!(
            views[0].items[0],
            tui::WorkspaceItemView::Shell(_)
        ));
    }

    #[test]
    fn workspace_view_does_not_treat_arbitrary_foreground_process_as_agent() {
        let mut ordinary = shell("s1", "w1", "terminal");
        ordinary.foreground_process = Some("OpenCode".into());
        let workspace = workspace("w1", "project", vec![ordinary]);

        let views = dashboard_views(&[workspace], &mut git::Cache::default());

        assert!(matches!(
            views[0].items[0],
            tui::WorkspaceItemView::Shell(_)
        ));
    }

    #[test]
    fn workspace_view_prefers_lifecycle_agent_over_foreground_hint() {
        let mut hinted = shell("s1", "w1", "terminal");
        hinted.foreground_process = Some("opencode".into());
        let mut workspace = workspace("w1", "project", vec![hinted]);
        let mut durable = agent("a1", "w1", "s1");
        durable.name = "reviewer".into();
        durable.integration = "custom".into();
        durable.observation.state = AgentState::Blocked;
        durable.observation.evidence = "needs approval".into();
        workspace.agents.push(durable);

        let views = dashboard_views(&[workspace], &mut git::Cache::default());

        let tui::WorkspaceItemView::AgentShell(item) = &views[0].items[0] else {
            panic!("expected durable agent-shell item");
        };
        assert_eq!(item.shell.name, "terminal");
        let agent = item.agent.as_ref().expect("durable agent");
        assert_eq!(agent.state, tui::AgentDisplayState::Blocked);
        assert_eq!(agent.evidence, "needs approval");
    }

    #[test]
    fn workspace_view_preserves_live_four_shell_three_agent_shape() {
        let mut hinted = shell("s1", "w1", "hinted");
        hinted.foreground_process = Some("opencode".into());
        let mut workspace = workspace(
            "w1",
            "project",
            vec![
                hinted,
                shell("s2", "w1", "durable-1"),
                shell("s3", "w1", "durable-2"),
                shell("s4", "w1", "ordinary"),
            ],
        );
        workspace.agents = vec![agent("a2", "w1", "s2"), agent("a3", "w1", "s3")];

        let views = dashboard_views(&[workspace], &mut git::Cache::default());

        assert_eq!(views[0].items.len(), 4);
        assert_eq!(
            views[0]
                .items
                .iter()
                .filter(|item| matches!(item, tui::WorkspaceItemView::AgentShell(_)))
                .count(),
            3
        );
        assert_eq!(
            views[0]
                .items
                .iter()
                .filter(|item| matches!(item, tui::WorkspaceItemView::Shell(_)))
                .count(),
            1
        );
    }

    #[test]
    fn workspace_view_selects_latest_active_agent_with_stable_id_tie_break() {
        let mut workspace = workspace("w1", "project", vec![shell("s1", "w1", "terminal")]);
        let mut older = agent("older", "w1", "s1");
        older.observation.observed_at_ms = 20;
        let mut tied_first = agent("agent-a", "w1", "s1");
        tied_first.observation.observed_at_ms = 30;
        tied_first.name = "first".into();
        tied_first.integration = "other".into();
        let mut tied_last = agent("agent-z", "w1", "s1");
        tied_last.observation.observed_at_ms = 30;
        tied_last.observation.evidence = "latest evidence".into();
        tied_last.name = "latest".into();
        tied_last.integration = "other".into();
        workspace.agents = vec![tied_last, older, tied_first];

        let views = dashboard_views(&[workspace], &mut git::Cache::default());

        let tui::WorkspaceItemView::AgentShell(item) = &views[0].items[0] else {
            panic!("expected agent-shell item");
        };
        assert_eq!(item.agent.as_ref().unwrap().evidence, "latest evidence");
    }

    #[test]
    fn workspace_view_ignores_completed_stale_wrong_run_and_orphan_agents() {
        let mut second_shell = shell("s2", "w1", "second");
        second_shell.run.as_mut().unwrap().id = "r2".into();
        let mut workspace = workspace(
            "w1",
            "project",
            vec![shell("s1", "w1", "first"), second_shell],
        );
        let mut ended = agent("ended", "w1", "s1");
        ended.ended_at_ms = Some(12);
        let mut done = agent("done", "w1", "s1");
        done.observation.state = AgentState::Done;
        let mut inactive = agent("inactive", "w1", "s1");
        inactive.observation.state = AgentState::Inactive;
        let mut stale = agent("stale", "w1", "s1");
        stale.run_id = "old-run".into();
        let wrong_pair = agent("wrong-pair", "w1", "s2");
        let orphan = agent("orphan", "w1", "missing");
        let mut wrong_workspace = agent("wrong-workspace", "w2", "s1");
        wrong_workspace.run_id = "r1".into();
        workspace.agents = vec![
            ended,
            done,
            inactive,
            stale,
            wrong_pair,
            orphan,
            wrong_workspace,
        ];

        let views = dashboard_views(&[workspace], &mut git::Cache::default());

        assert_eq!(views[0].items.len(), 2);
        assert!(
            views[0]
                .items
                .iter()
                .all(|item| matches!(item, tui::WorkspaceItemView::Shell(_)))
        );
    }

    #[test]
    fn workspace_view_does_not_present_an_exited_shell_as_an_active_agent() {
        let mut exited = shell("s1", "w1", "agent");
        exited.status = ShellStatus::Exited { code: Some(1) };
        let mut workspace = workspace("w1", "project", vec![exited]);
        workspace.agents.push(agent("a1", "w1", "s1"));

        let views = dashboard_views(&[workspace], &mut git::Cache::default());

        assert!(matches!(
            views[0].items[0],
            tui::WorkspaceItemView::Shell(_)
        ));
    }

    #[test]
    fn dashboard_open_dispatches_only_the_selected_item_type() {
        let mut shell_calls = 0;
        let mut launcher_calls = 0;
        let result = dispatch_dashboard_open(
            &tui::OpenTarget::Launcher {
                workspace_id: "w1".into(),
                launcher_id: "l1".into(),
            },
            "",
            |_| {
                shell_calls += 1;
                Ok("shell".into())
            },
            |workspace_id, launcher_id| {
                launcher_calls += 1;
                assert_eq!(workspace_id, "w1");
                assert_eq!(launcher_id, "l1");
                Ok("launcher".into())
            },
        )
        .unwrap();

        assert_eq!(result, "launcher");
        assert_eq!(shell_calls, 0);
        assert_eq!(launcher_calls, 1);
    }

    #[test]
    fn restoring_empty_workspace_returns_actionable_error() {
        let error =
            open_workspace(&workspace("w1", "empty", Vec::new()), None, None, true).unwrap_err();

        assert!(error.to_string().contains("workspace empty has no shells"));
    }

    #[test]
    fn dashboard_session_resume_plan_uses_exact_host_identity_and_rejects_current_work() {
        let mut session = session_projection::SessionProjection {
            id: "session-1".into(),
            workspace_id: "w1".into(),
            workspace_name: "project".into(),
            integration: "opencode".into(),
            external_session_id: Some("ses_exact".into()),
            description: "review".into(),
            user_display_name: None,
            workspace_revision: 1,
            state: AgentState::Inactive,
            state_is_current: false,
            started_at_ms: 1,
            last_at_ms: 2,
            source_cwd: Some(env::temp_dir()),
            occurrences: Vec::new(),
        };

        let (_, command) = dashboard_session_resume_plan(&session).unwrap();
        assert_eq!(command, ["opencode", "--session", "ses_exact"]);

        session.integration = "claude".into();
        let (_, command) = dashboard_session_resume_plan(&session).unwrap();
        assert_eq!(command, ["claude", "--resume", "ses_exact"]);

        session.integration = "codex".into();
        let (_, command) = dashboard_session_resume_plan(&session).unwrap();
        assert_eq!(command, ["codex", "resume", "ses_exact"]);

        session.state_is_current = true;
        assert!(
            dashboard_session_resume_plan(&session)
                .unwrap_err()
                .contains("already active")
        );
    }

    #[test]
    fn local_workspace_open_counts_successful_siblings_when_one_launcher_fails() {
        let marker = env::temp_dir().join(format!("boomux-launcher-{}", Uuid::new_v4()));
        let mut workspace = workspace("w1", "launchers", Vec::new());
        workspace.launchers = vec![
            WorkspaceLauncherSnapshot {
                id: "l1".into(),
                revision: 1,
                workspace_id: "w1".into(),
                name: "missing".into(),
                cwd: env::temp_dir(),
                command: vec!["/boomux-command-does-not-exist".into()],
            },
            WorkspaceLauncherSnapshot {
                id: "l2".into(),
                revision: 1,
                workspace_id: "w1".into(),
                name: "later".into(),
                cwd: env::temp_dir(),
                command: vec![
                    "/bin/sh".into(),
                    "-c".into(),
                    "printf launched > \"$1\"".into(),
                    "launcher".into(),
                    marker.display().to_string(),
                ],
            },
        ];

        let outcome = attempt_open_workspace(&workspace, None, None, true).unwrap();
        assert_eq!(outcome.opened_launchers, 1);
        assert_eq!(outcome.opened_shells, 0);
        assert_eq!(outcome.failures.len(), 1);
        assert!(outcome.failures[0].contains("launcher missing"));
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        let contents = loop {
            if let Ok(contents) = fs::read_to_string(&marker)
                && contents == "launched"
            {
                break contents;
            }
            if std::time::Instant::now() >= deadline {
                break String::new();
            }
            thread::sleep(std::time::Duration::from_millis(10));
        };
        assert_eq!(contents, "launched");
        if marker.is_file() {
            fs::remove_file(marker).unwrap();
        }
    }

    #[test]
    fn desktop_workspace_restore_does_not_invoke_launchers() {
        let marker = env::temp_dir().join(format!("boomux-desktop-launcher-{}", Uuid::new_v4()));
        let mut workspace = workspace("w1", "launcher-only", Vec::new());
        workspace.launchers = vec![WorkspaceLauncherSnapshot {
            id: "l1".into(),
            revision: 1,
            workspace_id: "w1".into(),
            name: "must-not-run".into(),
            cwd: env::temp_dir(),
            command: vec![
                "/bin/sh".into(),
                "-c".into(),
                "printf launched > \"$1\"".into(),
                "launcher".into(),
                marker.display().to_string(),
            ],
        }];

        let error = open_workspace(&workspace, None, None, false).unwrap_err();

        assert!(error.to_string().contains("has no Shell terminal windows"));
        assert!(!marker.exists());
    }

    #[test]
    fn selects_recent_rendered_lines() {
        assert_eq!(recent_lines("one\ntwo\nthree\n", 2), "two\nthree\n");
        assert_eq!(recent_lines("one\ntwo", 1), "two");
    }

    #[test]
    fn formats_shell_run_exit_codes() {
        assert_eq!(
            shell_exit_reason(&boomux::protocol::ShellRunExitReason::Exited { code: Some(7) }),
            "exited (7)"
        );
        assert_eq!(
            shell_exit_reason(&boomux::protocol::ShellRunExitReason::Exited { code: None }),
            "exited (code unavailable)"
        );
    }

    #[test]
    fn installs_skill_under_vendor_neutral_directory() {
        assert_eq!(
            skill_install_path(Path::new("/home/example")),
            PathBuf::from("/home/example/.agents/skills/boomux/SKILL.md")
        );
        assert_eq!(
            legacy_skill_install_path(Path::new("/home/example")),
            PathBuf::from("/home/example/.agents/skills/boomux-shells/SKILL.md")
        );
    }

    #[test]
    fn parses_opencode_install_and_uses_global_plugin_path() {
        assert!(matches!(
            Cli::try_parse_from(["boomux", "opencode", "install", "--force"])
                .unwrap()
                .command,
            Some(Commands::Opencode {
                command: OpenCodeCommands::Install { force: true }
            })
        ));
        assert_eq!(
            opencode_install_path(Path::new("/config")),
            PathBuf::from("/config/opencode/plugins/boomux.js")
        );
        assert_eq!(
            opencode_config_root(Some("/xdg".into()), Some("/home/example".into())).unwrap(),
            PathBuf::from("/xdg")
        );
        assert_eq!(
            opencode_config_root(None, Some("/home/example".into())).unwrap(),
            PathBuf::from("/home/example/.config")
        );
        assert!(opencode_config_root(Some("relative".into()), None).is_err());
        assert!(install_opencode_at(Path::new("relative"), false).is_err());

        let shared = Cli::try_parse_from(["boomux", "opencode", "shared"]).unwrap();
        assert!(matches!(
            shared.command,
            Some(Commands::Opencode {
                command: OpenCodeCommands::Shared {
                    session: None,
                    port: 4097,
                },
            })
        ));
        assert!(matches!(
            Cli::try_parse_from([
                "boomux",
                "opencode",
                "shared",
                "--session",
                "session-exact"
            ])
            .unwrap()
            .command,
            Some(Commands::Opencode {
                command: OpenCodeCommands::Shared {
                    session: Some(ref session),
                    port: 4097,
                },
            }) if session == "session-exact"
        ));
        assert!(Cli::try_parse_from(["boomux", "opencode", "attach", "agent-exact"]).is_err());
    }

    #[test]
    fn opencode_claim_registration_and_shared_launch_are_exact() {
        assert_eq!(
            opencode_claim_registration("session-root"),
            AgentRegistrationSpec {
                name: "OpenCode".into(),
                integration: "opencode".into(),
                external_session_id: Some("session-root".into()),
                report: AgentReport {
                    state: AgentState::Unknown,
                    authority: AgentAuthority::LifecycleIntegration,
                    evidence: "direct TUI selection".into(),
                    confidence: 100,
                },
            }
        );

        let directory = env::temp_dir().join(format!("boomux-shared-plan-{}", Uuid::new_v4()));
        fs::create_dir_all(&directory).unwrap();
        let executable = directory.join("opencode");
        let tui_config = directory.join("tui.json");
        fs::write(&executable, b"#!/bin/sh\n").unwrap();
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
        fs::write(&tui_config, b"{}\n").unwrap();
        let launch = shared_opencode_launch(SharedOpenCodeLaunchInput {
            shell_id: Some("shell-1".into()),
            run_id: Some("run-1".into()),
            executable: Some(executable.clone().into_os_string()),
            tui_config: Some(tui_config.clone().into_os_string()),
            original_path: Some("/original/bin".into()),
            cwd: directory.clone(),
            session: Some("session-exact".into()),
            runtime: protocol::OpenCodeSharedRuntimeSnapshot {
                generation_id: "generation-1".into(),
                url: "http://127.0.0.1:4097".into(),
                port: 4097,
                pid: Some(42),
            },
        })
        .unwrap();
        assert_eq!(launch.executable, executable);
        assert_eq!(launch.url, "http://127.0.0.1:4097");
        assert_eq!(launch.cwd, directory);
        assert_eq!(launch.generation, "generation-1");
        assert!(Uuid::parse_str(&launch.holder).is_ok());
        assert_eq!(launch.tui_config, tui_config);
        assert_eq!(launch.original_path, Some("/original/bin".into()));
        assert_eq!(
            launch.argv(),
            [
                OsString::from("attach"),
                OsString::from("http://127.0.0.1:4097"),
                OsString::from("--dir"),
                launch.cwd.clone().into_os_string(),
                OsString::from("--session"),
                OsString::from("session-exact"),
            ]
        );
        fs::remove_dir_all(&launch.cwd).unwrap();
    }

    #[test]
    fn parses_unified_integration_management_commands() {
        let list = Cli::try_parse_from(["boomux", "integration", "list"]).unwrap();
        assert!(matches!(
            list.command,
            Some(Commands::Integration {
                command: IntegrationCommands::List
            })
        ));
        assert_eq!(list.command_descriptor().key, "integration.list");

        for command in ["status", "setup", "install", "uninstall"] {
            let parsed = Cli::try_parse_from(["boomux", "integration", command, "claude"])
                .unwrap_or_else(|error| panic!("failed to parse integration {command}: {error}"));
            match parsed.command.unwrap() {
                Commands::Integration {
                    command:
                        IntegrationCommands::Status {
                            integration: Some(integration_management::IntegrationId::Claude),
                            ..
                        },
                }
                | Commands::Integration {
                    command:
                        IntegrationCommands::Setup {
                            integration: integration_management::IntegrationId::Claude,
                            ..
                        },
                }
                | Commands::Integration {
                    command:
                        IntegrationCommands::Install {
                            integration: Some(integration_management::IntegrationId::Claude),
                            ..
                        },
                }
                | Commands::Integration {
                    command:
                        IntegrationCommands::Uninstall {
                            integration: Some(integration_management::IntegrationId::Claude),
                            ..
                        },
                } => {}
                _ => panic!("unexpected parsed Claude integration command"),
            }
        }

        let verify = Cli::try_parse_from([
            "boomux",
            "integration",
            "verify",
            "claude",
            "--shell",
            "shell-1",
        ])
        .unwrap();
        assert!(matches!(
            verify.command,
            Some(Commands::Integration {
                command: IntegrationCommands::Verify {
                    integration: integration_management::IntegrationId::Claude,
                    shell: Some(shell),
                    ..
                }
            }) if shell == "shell-1"
        ));

        let status = Cli::try_parse_from(["boomux", "integration", "status", "pi"]).unwrap();
        assert!(matches!(
            status.command,
            Some(Commands::Integration {
                command: IntegrationCommands::Status {
                    integration: Some(integration_management::IntegrationId::Pi),
                    ..
                }
            })
        ));
        assert_eq!(status.command_descriptor().key, "integration.status");

        let install =
            Cli::try_parse_from(["boomux", "integration", "install", "opencode", "--force"])
                .unwrap();
        assert!(matches!(
            install.command,
            Some(Commands::Integration {
                command: IntegrationCommands::Install {
                    integration: Some(integration_management::IntegrationId::Opencode),
                    all: false,
                    force: true,
                    dry_run: false,
                    ..
                }
            })
        ));
        assert_eq!(install.command_descriptor().key, "integration.install");

        let preview = Cli::try_parse_from([
            "boomux",
            "integration",
            "install",
            "pi",
            "--dry-run",
            "--json",
        ])
        .unwrap();
        assert!(matches!(
            preview.command,
            Some(Commands::Integration {
                command: IntegrationCommands::Install {
                    integration: Some(integration_management::IntegrationId::Pi),
                    all: false,
                    force: false,
                    dry_run: true,
                    ..
                }
            })
        ));
        assert_eq!(preview.command_descriptor().output, OutputMode::Json);

        let uninstall = Cli::try_parse_from([
            "boomux",
            "integration",
            "uninstall",
            "opencode",
            "--force",
            "--json",
        ])
        .unwrap();
        assert!(matches!(
            uninstall.command,
            Some(Commands::Integration {
                command: IntegrationCommands::Uninstall {
                    integration: Some(integration_management::IntegrationId::Opencode),
                    all: false,
                    force: true,
                    ..
                }
            })
        ));
        assert_eq!(uninstall.command_descriptor().key, "integration.uninstall");
        assert_eq!(uninstall.command_descriptor().output, OutputMode::Json);

        let setup =
            Cli::try_parse_from(["boomux", "integration", "setup", "pi", "--yes", "--force"])
                .unwrap();
        assert!(matches!(
            setup.command,
            Some(Commands::Integration {
                command: IntegrationCommands::Setup {
                    integration: integration_management::IntegrationId::Pi,
                    yes: true,
                    force: true,
                    ..
                }
            })
        ));
        assert_eq!(setup.command_descriptor().key, "integration.setup");
        assert_eq!(setup.command_descriptor().output, OutputMode::HumanOnly);

        assert!(Cli::try_parse_from(["boomux", "integration", "install", "--all"]).is_ok());
        assert!(Cli::try_parse_from(["boomux", "integration", "install"]).is_err());
        assert!(Cli::try_parse_from(["boomux", "integration", "install", "pi", "--all",]).is_err());

        let verify = Cli::try_parse_from([
            "boomux",
            "integration",
            "verify",
            "opencode",
            "--shell",
            "s1",
            "--wait-ms",
            "0",
            "--json",
        ])
        .unwrap();
        assert!(matches!(
            verify.command,
            Some(Commands::Integration {
                command: IntegrationCommands::Verify {
                    integration: integration_management::IntegrationId::Opencode,
                    shell: Some(ref shell),
                    wait_ms: 0,
                    ..
                }
            }) if shell == "s1"
        ));
        assert_eq!(verify.command_descriptor().key, "integration.verify");
        assert_eq!(verify.command_descriptor().output, OutputMode::Json);
    }

    #[test]
    fn ambiguous_verification_lists_named_shell_commands() {
        let snapshot = Snapshot {
            workspaces: vec![
                workspace("w2", "Zeta", vec![shell("s2", "w2", "backend")]),
                workspace("w1", "Alpha", vec![shell("s1", "w1", "frontend")]),
            ],
            focused_terminal: None,
        };
        let targets = vec![
            integration_management::VerificationTarget {
                shell_id: "s2".into(),
                run_id: "r2".into(),
            },
            integration_management::VerificationTarget {
                shell_id: "s1".into(),
                run_id: "r1".into(),
            },
        ];

        assert_eq!(
            format_ambiguous_verification_targets(
                &snapshot,
                integration_management::IntegrationId::Opencode,
                &targets,
            ),
            concat!(
                "multiple running OpenCode host shells found\n\n",
                "Choose a shell:\n",
                "  Alpha / frontend\n",
                "    boomux integration verify opencode --shell s1\n",
                "  Zeta / backend\n",
                "    boomux integration verify opencode --shell s2",
            )
        );
    }

    #[test]
    fn setup_confirmation_requires_an_explicit_yes() {
        assert!(is_setup_confirmation("y\n"));
        assert!(is_setup_confirmation(" YES "));
        assert!(!is_setup_confirmation(""));
        assert!(!is_setup_confirmation("no"));
    }

    #[test]
    fn formats_integration_output_without_tab_alignment() {
        let integrations = integration_management::IntegrationId::all()
            .map(integration_management::IntegrationSummary::from)
            .collect::<Vec<_>>();
        assert_eq!(
            format_integration_list(&integrations),
            "NAME      PACKAGE                          VALIDATED VERSION\n\
             opencode  opencode-ai                      1.18.18\n\
             pi        @earendil-works/pi-coding-agent  0.84.1\n\
             claude    @anthropic-ai/claude-code        2.1.236\n\
             codex     @openai/codex                    0.147.0\n\
             kiro      kiro-cli                         2.18.0\n"
        );

        let status = integration_management::IntegrationStatus {
            name: "opencode",
            display_name: "OpenCode",
            package: "opencode-ai",
            validated_version: "1.18.18",
            host: integration_management::HostStatus {
                state: integration_management::HostState::Available,
                executable: Some("/usr/bin/opencode".into()),
                version: Some("1.18.18".into()),
                compatibility: "validated",
                error: None,
            },
            asset: integration_management::AssetStatus {
                state: integration_management::AssetState::Current,
                path: Some("/home/test/.config/opencode/plugins/boomux.js".into()),
                error: None,
            },
            runtime: integration_management::RuntimeStatus {
                state: integration_management::RuntimeState::Untracked,
                running_processes: 3,
                tracked_processes: 2,
                untracked_processes: 1,
            },
            recommended_action: integration_management::RecommendedAction::RestartHost,
        };
        let output = format_integration_statuses(&[status]);
        assert!(output.contains("  Host          available\n"));
        assert!(
            output.contains("  Runtime       untracked (3 running, 2 reporting, 1 untracked)\n")
        );
        assert!(output.contains("  Action        restart host\n"));
        assert_eq!(
            format_recommended_action(
                "kiro",
                integration_management::RecommendedAction::RestartHost
            ),
            "reopen managed ShellRun, then launch bare kiro-cli"
        );
    }

    #[test]
    fn opencode_install_is_idempotent_and_requires_force_for_changes() {
        let config = test_skill_home("opencode-content");
        let plugin = opencode_install_path(&config);
        let herdr = config.join("opencode/plugins/herdr.js");
        let opencode_json = config.join("opencode/opencode.json");
        fs::create_dir_all(herdr.parent().unwrap()).unwrap();
        fs::write(&herdr, "herdr plugin").unwrap();
        fs::write(&opencode_json, "{\"plugin\":[\"herdr\"]}").unwrap();

        install_opencode_at(&config, false).unwrap();
        assert_eq!(fs::read_to_string(&plugin).unwrap(), BOOMUX_OPENCODE_PLUGIN);
        assert_eq!(fs::read_to_string(&herdr).unwrap(), "herdr plugin");
        assert_eq!(
            fs::read_to_string(&opencode_json).unwrap(),
            "{\"plugin\":[\"herdr\"]}"
        );
        install_opencode_at(&config, false).unwrap();
        fs::write(&plugin, "custom plugin").unwrap();
        assert!(install_opencode_at(&config, false).is_err());
        assert_eq!(fs::read_to_string(&plugin).unwrap(), "custom plugin");
        install_opencode_at(&config, true).unwrap();
        assert_eq!(fs::read_to_string(&plugin).unwrap(), BOOMUX_OPENCODE_PLUGIN);

        fs::remove_dir_all(config).unwrap();
    }

    #[test]
    fn parses_pi_install_and_uses_global_extension_path() {
        assert!(matches!(
            Cli::try_parse_from(["boomux", "pi", "install", "--force"])
                .unwrap()
                .command,
            Some(Commands::Pi {
                command: PiCommands::Install { force: true }
            })
        ));
        assert_eq!(
            pi_install_path(Path::new("/pi-agent")),
            PathBuf::from("/pi-agent/extensions/boomux.js")
        );
        assert_eq!(
            pi_config_root(Some("/custom/pi".into()), Some("/home/example".into())).unwrap(),
            PathBuf::from("/custom/pi")
        );
        assert_eq!(
            pi_config_root(None, Some("/home/example".into())).unwrap(),
            PathBuf::from("/home/example/.pi/agent")
        );
        assert_eq!(
            pi_config_root(Some("".into()), Some("/home/example".into())).unwrap(),
            PathBuf::from("/home/example/.pi/agent")
        );
        assert_eq!(
            pi_config_root(Some("~/.config/pi".into()), Some("/home/example".into())).unwrap(),
            PathBuf::from("/home/example/.config/pi")
        );
        assert!(pi_config_root(Some("relative".into()), None).is_err());
        assert!(install_pi_at(Path::new("relative"), false).is_err());
    }

    #[test]
    fn pi_install_is_idempotent_and_requires_force_for_changes() {
        let config = test_skill_home("pi-content");
        let extension = pi_install_path(&config);

        install_pi_at(&config, false).unwrap();
        assert_eq!(fs::read_to_string(&extension).unwrap(), BOOMUX_PI_EXTENSION);
        install_pi_at(&config, false).unwrap();
        fs::write(&extension, "custom extension").unwrap();
        assert!(install_pi_at(&config, false).is_err());
        assert_eq!(fs::read_to_string(&extension).unwrap(), "custom extension");
        install_pi_at(&config, true).unwrap();
        assert_eq!(fs::read_to_string(&extension).unwrap(), BOOMUX_PI_EXTENSION);

        fs::remove_dir_all(config).unwrap();
    }

    #[test]
    fn pi_install_rejects_symlinked_extension_directory() {
        use std::os::unix::fs::symlink;

        let config = test_skill_home("pi-symlink-directory");
        let outside = test_skill_home("pi-symlink-directory-target");
        fs::create_dir_all(&config).unwrap();
        fs::create_dir_all(&outside).unwrap();
        symlink(&outside, config.join("extensions")).unwrap();

        assert!(install_pi_at(&config, true).is_err());
        assert!(fs::read_dir(&outside).unwrap().next().is_none());

        fs::remove_dir_all(config).unwrap();
        fs::remove_dir_all(outside).unwrap();
    }

    #[test]
    fn capabilities_advertise_phase_two_agent_integration_surface() {
        let json_commands = json_commands().collect::<Vec<_>>();
        assert!(json_commands.contains(&"shell.suggest-name"));
        assert!(json_commands.contains(&"workspace.create"));
        assert!(NON_PROTOCOL_FEATURES.contains(&"atomic_workspace_shell_creation"));
        assert!(!NON_PROTOCOL_FEATURES.contains(&"exact_session_open"));
        assert!(!json_commands.contains(&"session.open"));
        for command in [
            "agent.register",
            "agent.ensure",
            "agent.report",
            "agent.wait",
            "attention.list",
            "attention.acknowledge",
        ] {
            assert!(json_commands.contains(&command));
        }
        for command in [
            "session.list",
            "session.inspect",
            "session.rename",
            "session.reset-name",
            "session.hide",
        ] {
            assert!(!json_commands.contains(&command));
        }
        assert!(!protocol::protocol_capabilities().any(|feature| {
            feature == "projected_agent_sessions"
                || feature == "remote_agent_session_catalog"
                || feature == "remote_exact_session_resume"
                || feature.starts_with("session_")
                || feature == "workspace_session_hiding"
        }));
        for command in [
            "integration.list",
            "integration.status",
            "integration.install",
            "integration.uninstall",
            "integration.verify",
        ] {
            assert!(json_commands.contains(&command));
        }
        for feature in [
            "protocol_10",
            "protocol_12",
            "inactive_agent_state",
            "idempotent_agent_ensure",
            "agent_authority_precedence",
            "opencode_lifecycle_plugin",
            "pi_lifecycle_extension",
            "protocol_15",
            "protocol_16",
            "persistent_agent_attention",
            "desktop_notifications",
            "sound_notifications",
            "integration_management",
            "desktop_workspace_show",
            "node_reauthentication",
            "protocol_31",
            "node_registration_management",
            "pinned_node_identity",
        ] {
            assert!(
                NON_PROTOCOL_FEATURES.contains(&feature)
                    || protocol::protocol_capabilities().any(|current| current == feature)
            );
        }
        assert_eq!(
            NON_PROTOCOL_FEATURES
                .iter()
                .filter(|feature| **feature == "desktop_workspace_show")
                .count(),
            1
        );
        assert!(!NON_PROTOCOL_FEATURES.contains(&"workspace_open_desktop_show"));
        assert_eq!(
            integration_management::IntegrationId::Opencode
                .installation()
                .validated_version,
            "1.18.18"
        );
        assert_eq!(
            integration_management::IntegrationId::Pi
                .installation()
                .validated_version,
            "0.84.1"
        );
        assert_eq!(
            integration_management::IntegrationId::Claude
                .installation()
                .validated_version,
            "2.1.236"
        );
        assert_eq!(protocol::PROTOCOL_VERSION, 51);
    }

    #[test]
    fn protocol_thirty_nine_focus_absence_does_not_restore_legacy_local_focus() {
        let local = protocol::FocusedTerminalSnapshot {
            revision: 8,
            workspace_id: "local-workspace".into(),
            shell_id: "local-shell".into(),
            run_id: "local-run".into(),
        };
        assert!(
            dashboard_focused_terminal_view(39, None, Some(local.clone())).is_none(),
            "qualified focus absence is authoritative for protocol 39"
        );
        assert_eq!(
            dashboard_focused_terminal_view(38, None, Some(local))
                .unwrap()
                .shell_id,
            "local-shell"
        );
    }

    #[test]
    fn focus_frontier_resets_once_per_protocol_handoff_transition() {
        let mut negotiated = 38;

        assert!(update_negotiated_protocol(&mut negotiated, 39));
        assert!(!update_negotiated_protocol(&mut negotiated, 39));
        assert!(update_negotiated_protocol(&mut negotiated, 38));
        assert!(!update_negotiated_protocol(&mut negotiated, 38));
    }

    #[test]
    fn command_descriptors_have_unique_names_and_drive_json_capabilities() {
        let mut names = CommandKey::ALL
            .iter()
            .map(|key| key.descriptor().key)
            .collect::<Vec<_>>();
        let command_count = names.len();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), command_count);

        assert_eq!(
            json_commands().collect::<Vec<_>>(),
            [
                "web.start",
                "web.status",
                "web.stop",
                "capabilities",
                "list",
                "shells",
                "read",
                "events",
                "update.status",
                "project.list",
                "workspace.list",
                "workspace.inspect",
                "workspace.create",
                "workspace.set-default-cwd",
                "node.add",
                "node.list",
                "node.inspect",
                "node.snapshot",
                "node.rename",
                "node.retarget",
                "node.forget",
                "shell.suggest-name",
                "shell.inspect",
                "launcher.list",
                "launcher.inspect",
                "agent.list",
                "agent.inspect",
                "agent.register",
                "agent.ensure",
                "agent.report",
                "agent.wait",
                "attention.list",
                "attention.acknowledge",
                "integration.list",
                "integration.status",
                "integration.install",
                "integration.uninstall",
                "integration.verify",
                "daemon.status",
            ]
        );
    }

    #[test]
    fn doctor_reports_version_and_platform() {
        assert_eq!(
            doctor_version_line("1.2.3", "x86_64", "linux"),
            "ok  boomux: 1.2.3 (x86_64-linux)"
        );
    }

    #[test]
    fn notification_doctor_diagnostic_is_deterministic() {
        let disabled = daemon::NotificationDeliverySettings::default();
        assert_eq!(
            notification_diagnostic(&disabled, false, false),
            NotificationDiagnostic::Disabled
        );
        let enabled = daemon::NotificationDeliverySettings {
            desktop: daemon::NotificationSettings {
                enabled: true,
                ..Default::default()
            },
            ..disabled
        };
        assert_eq!(
            notification_diagnostic(&enabled, false, true),
            NotificationDiagnostic::MissingExecutable
        );
        assert_eq!(
            notification_diagnostic(&enabled, true, false),
            NotificationDiagnostic::MissingDesktopBus
        );
        assert_eq!(
            notification_diagnostic(&enabled, true, true),
            NotificationDiagnostic::Ready
        );

        assert_eq!(
            sound_notification_diagnostic(&enabled, false),
            SoundNotificationDiagnostic::Disabled
        );
        let sound_enabled = daemon::NotificationDeliverySettings {
            sound: daemon::NotificationSoundSettings {
                enabled: true,
                ..Default::default()
            },
            ..Default::default()
        };
        assert_eq!(
            sound_notification_diagnostic(&sound_enabled, false),
            SoundNotificationDiagnostic::MissingExecutable
        );
        assert_eq!(
            sound_notification_diagnostic(&sound_enabled, true),
            SoundNotificationDiagnostic::Ready
        );
    }

    #[test]
    fn opencode_install_rejects_symlinks_and_special_targets_even_with_force() {
        use std::os::unix::fs::symlink;

        let config = test_skill_home("opencode-symlink-directory");
        let outside = test_skill_home("opencode-symlink-directory-target");
        fs::create_dir_all(&config).unwrap();
        fs::create_dir_all(&outside).unwrap();
        symlink(&outside, config.join("opencode")).unwrap();
        assert!(install_opencode_at(&config, true).is_err());
        assert!(fs::read_dir(&outside).unwrap().next().is_none());
        fs::remove_dir_all(&config).unwrap();
        fs::remove_dir_all(&outside).unwrap();

        let config = test_skill_home("opencode-symlink-file");
        let outside = test_skill_home("opencode-symlink-file-target");
        let plugin = opencode_install_path(&config);
        fs::create_dir_all(plugin.parent().unwrap()).unwrap();
        fs::write(&outside, "do not replace").unwrap();
        symlink(&outside, &plugin).unwrap();
        assert!(install_opencode_at(&config, true).is_err());
        assert_eq!(fs::read_to_string(&outside).unwrap(), "do not replace");
        fs::remove_dir_all(&config).unwrap();
        fs::remove_file(&outside).unwrap();

        let config = test_skill_home("opencode-special-file");
        let plugin = opencode_install_path(&config);
        fs::create_dir_all(&plugin).unwrap();
        assert!(install_opencode_at(&config, true).is_err());
        assert!(plugin.is_dir());
        fs::remove_dir_all(config).unwrap();
    }

    #[test]
    fn bundled_opencode_plugin_has_lifecycle_contract_without_shell_interpolation() {
        for expected in [
            "session.status",
            "session.idle",
            "session.error",
            "session.deleted",
            "permission.asked",
            "question.asked",
            "BOOMUX_SHELL_ID",
            "BOOMUX_RUN_ID",
            "agent\",\n    \"ensure",
            "agent\",\n    \"report",
            "--json",
            "shell: false",
        ] {
            assert!(
                BOOMUX_OPENCODE_PLUGIN.contains(expected),
                "OpenCode plugin omits {expected}"
            );
        }
        for forbidden in ["Bun.$", "exec(", "sh -c", "${BOOMUX_"] {
            assert!(
                !BOOMUX_OPENCODE_PLUGIN.contains(forbidden),
                "OpenCode plugin contains shell interpolation marker {forbidden}"
            );
        }
    }

    #[test]
    fn bundled_pi_extension_has_lifecycle_contract_without_shell_interpolation() {
        for expected in [
            "session_start",
            "agent_start",
            "agent_end",
            "agent_settled",
            "session_shutdown",
            "stopReason",
            "getSessionId",
            "BOOMUX_SHELL_ID",
            "BOOMUX_RUN_ID",
            "agent\",\n    \"ensure",
            "agent\",\n    \"report",
            "--json",
            "shell: false",
        ] {
            assert!(
                BOOMUX_PI_EXTENSION.contains(expected),
                "Pi extension omits {expected}"
            );
        }
        for forbidden in ["exec(", "sh -c", "${BOOMUX_"] {
            assert!(
                !BOOMUX_PI_EXTENSION.contains(forbidden),
                "Pi extension contains shell interpolation marker {forbidden}"
            );
        }
    }

    #[test]
    fn bundled_skill_covers_every_public_command_group() {
        for command in [
            "boomux ui",
            "boomux doctor",
            "boomux capabilities",
            "boomux list",
            "boomux shells",
            "boomux read",
            "boomux events",
            "boomux close",
            "boomux open",
            "boomux project list",
            "boomux node add",
            "boomux node upgrade",
            "boomux node list",
            "boomux node inspect",
            "boomux node snapshot",
            "boomux node rename",
            "boomux node retarget",
            "boomux node forget",
            "boomux node rekey",
            "boomux workspace list",
            "boomux workspace create",
            "boomux workspace open",
            "boomux workspace inspect",
            "boomux workspace rename",
            "boomux workspace adopt",
            "boomux workspace link",
            "boomux workspace close",
            "boomux workspace retry",
            "boomux shell suggest-name",
            "boomux shell create",
            "boomux shell inspect",
            "boomux shell rename",
            "boomux shell close",
            "boomux launcher list",
            "boomux launcher create",
            "boomux launcher inspect",
            "boomux launcher invoke",
            "boomux launcher rename",
            "boomux launcher remove",
            "boomux agent list",
            "boomux agent inspect",
            "boomux agent wait",
            "boomux agent register",
            "boomux agent ensure",
            "boomux agent report",
            "boomux agent supervise",
            "boomux attention list",
            "boomux attention acknowledge",
            "boomux notification test",
            "boomux integration list",
            "boomux integration status",
            "boomux integration install",
            "boomux integration uninstall",
            "boomux integration setup",
            "boomux integration verify",
            "boomux skill install",
            "boomux opencode install",
            "boomux pi install",
            "boomux daemon status",
            "boomux daemon restart",
            "boomux daemon stop",
            "boomux prompt",
        ] {
            let documented = BOOMUX_SKILL.lines().any(|line| {
                let line = line.trim_start();
                line.strip_prefix(command).is_some_and(|remaining| {
                    remaining.is_empty()
                        || remaining.chars().next().is_some_and(char::is_whitespace)
                })
            });
            assert!(documented, "skill omits command line for {command}");
        }
        assert!(BOOMUX_SKILL.contains("BOOMUX_SHELL_ID"));
        assert!(BOOMUX_SKILL.contains("--workspace"));
        assert!(BOOMUX_SKILL.contains("--node"));
        assert!(BOOMUX_SKILL.contains("--revision"));
        assert!(BOOMUX_SKILL.contains("--terminal"));
        assert!(!BOOMUX_SKILL.contains("boomux session "));
        assert!(!BOOMUX_SKILL.contains("workspace create \"<name>\" --cwd"));
    }

    #[test]
    fn skill_install_migrates_an_untouched_legacy_skill() {
        let home = test_skill_home("migrate");
        let legacy = legacy_skill_install_path(&home);
        fs::create_dir_all(legacy.parent().unwrap()).unwrap();
        fs::write(&legacy, LEGACY_BOOMUX_SHELLS_SKILL).unwrap();

        install_skill_at(&home, false).unwrap();

        assert_eq!(
            fs::read_to_string(skill_install_path(&home)).unwrap(),
            BOOMUX_SKILL
        );
        assert!(!legacy.exists());
        install_skill_at(&home, false).unwrap();
        assert_eq!(
            fs::read_to_string(skill_install_path(&home)).unwrap(),
            BOOMUX_SKILL
        );
        fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn skill_install_requires_force_and_preserves_customized_legacy_content() {
        let home = test_skill_home("customized");
        let skill = skill_install_path(&home);
        let legacy = legacy_skill_install_path(&home);
        fs::create_dir_all(skill.parent().unwrap()).unwrap();
        fs::create_dir_all(legacy.parent().unwrap()).unwrap();
        fs::write(&skill, "older consolidated skill").unwrap();
        fs::write(&legacy, "customized legacy skill").unwrap();

        assert!(install_skill_at(&home, false).is_err());
        assert_eq!(
            fs::read_to_string(&skill).unwrap(),
            "older consolidated skill"
        );
        assert_eq!(
            fs::read_to_string(&legacy).unwrap(),
            "customized legacy skill"
        );
        install_skill_at(&home, true).unwrap();

        assert_eq!(fs::read_to_string(skill).unwrap(), BOOMUX_SKILL);
        assert_eq!(
            fs::read_to_string(legacy).unwrap(),
            "customized legacy skill"
        );
        fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn skill_install_preserves_legacy_skill_with_additional_files() {
        let home = test_skill_home("legacy-resources");
        let legacy = legacy_skill_install_path(&home);
        let reference = legacy.parent().unwrap().join("REFERENCE.md");
        fs::create_dir_all(legacy.parent().unwrap()).unwrap();
        fs::write(&legacy, LEGACY_BOOMUX_SHELLS_SKILL).unwrap();
        fs::write(&reference, "custom reference").unwrap();

        install_skill_at(&home, false).unwrap();

        assert_eq!(
            fs::read_to_string(&legacy).unwrap(),
            LEGACY_BOOMUX_SHELLS_SKILL
        );
        assert_eq!(fs::read_to_string(reference).unwrap(), "custom reference");
        fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn skill_install_rejects_symlinked_directories_and_files() {
        use std::os::unix::fs::symlink;

        let home = test_skill_home("symlink-directory");
        let outside = test_skill_home("symlink-directory-target");
        fs::create_dir_all(&home).unwrap();
        fs::create_dir_all(&outside).unwrap();
        symlink(&outside, home.join(".agents")).unwrap();

        assert!(install_skill_at(&home, true).is_err());
        assert!(fs::read_dir(&outside).unwrap().next().is_none());
        fs::remove_dir_all(&home).unwrap();
        fs::remove_dir_all(&outside).unwrap();

        let home = test_skill_home("symlink-file");
        let outside = test_skill_home("symlink-file-target");
        let skill = skill_install_path(&home);
        fs::create_dir_all(skill.parent().unwrap()).unwrap();
        fs::write(&outside, "do not replace").unwrap();
        symlink(&outside, &skill).unwrap();

        assert!(install_skill_at(&home, true).is_err());
        assert_eq!(fs::read_to_string(&outside).unwrap(), "do not replace");
        fs::remove_dir_all(&home).unwrap();
        fs::remove_file(&outside).unwrap();

        let home = test_skill_home("legacy-symlink");
        let outside = test_skill_home("legacy-symlink-target");
        fs::create_dir_all(home.join(".agents/skills")).unwrap();
        fs::create_dir_all(&outside).unwrap();
        symlink(&outside, home.join(".agents/skills/boomux-shells")).unwrap();

        assert!(install_skill_at(&home, false).is_err());
        assert!(!skill_install_path(&home).exists());
        fs::remove_dir_all(home).unwrap();
        fs::remove_dir_all(outside).unwrap();
    }

    fn inspected_session(
        state: AgentState,
        current: bool,
        cwd: Option<&str>,
    ) -> protocol::HostAgentSessionInspection {
        protocol::HostAgentSessionInspection {
            summary: protocol::HostAgentSessionSummary {
                id: "session-id".into(),
                workspace_id: "workspace-id".into(),
                workspace_name: "workspace".into(),
                description: "review".into(),
                latest_agent_name: None,
                user_display_name: None,
                workspace_revision: 1,
                integration: "opencode".into(),
                external_session_id: Some("external-id".into()),
                state,
                state_is_current: current,
                started_at_ms: 10,
                last_at_ms: 20,
                occurrence_count: 1,
                attentions: Vec::new(),
                git_branch: None,
                working_contexts: Vec::new(),
                working_context_count: 0,
            },
            source_cwd: cwd.map(PathBuf::from),
            occurrences: Vec::new(),
            projected_occurrences: Vec::new(),
        }
    }

    fn host_session_summary(
        id: &str,
        workspace_id: &str,
        last_at_ms: u64,
    ) -> protocol::HostAgentSessionSummary {
        protocol::HostAgentSessionSummary {
            id: id.into(),
            workspace_id: workspace_id.into(),
            workspace_name: format!("workspace-{workspace_id}"),
            description: format!("Session {id}"),
            latest_agent_name: None,
            user_display_name: None,
            workspace_revision: 1,
            integration: "opencode".into(),
            external_session_id: Some(format!("external-{id}")),
            state: AgentState::Inactive,
            state_is_current: false,
            started_at_ms: last_at_ms.saturating_sub(10),
            last_at_ms,
            occurrence_count: 1,
            attentions: Vec::new(),
            git_branch: None,
            working_contexts: Vec::new(),
            working_context_count: 0,
        }
    }

    fn current_inspected_session(
        shell_id: &str,
        run_id: &str,
    ) -> protocol::HostAgentSessionInspection {
        let mut inspection = inspected_session(AgentState::Working, true, Some("/tmp/project"));
        let mut occurrence = agent("agent-id", "workspace-id", shell_id);
        occurrence.run_id = run_id.into();
        inspection.occurrences.push(occurrence);
        inspection
    }

    #[test]
    fn session_open_actions_distinguish_current_historical_and_invalid_sessions() {
        assert_eq!(
            session_open_action(&inspected_session(AgentState::Working, true, None)),
            Ok("open exact current managed Shell".into())
        );
        assert_eq!(
            session_open_action(&inspected_session(
                AgentState::Inactive,
                false,
                Some("/tmp/project")
            )),
            Ok("resume exact historical Session".into())
        );
        assert!(
            session_open_action(&inspected_session(AgentState::Done, false, Some("/tmp")))
                .unwrap_err()
                .contains("permanently done")
        );
        assert!(
            session_open_action(&inspected_session(AgentState::Done, true, Some("/tmp")))
                .unwrap_err()
                .contains("permanently done")
        );
        assert!(
            session_open_action(&inspected_session(AgentState::Inactive, false, None))
                .unwrap_err()
                .contains("source cwd")
        );
    }

    #[test]
    fn dashboard_session_aggregation_retains_partial_node_results_and_qualified_order() {
        let loaded = aggregate_dashboard_session_loads([
            DashboardSessionNodeLoad {
                node: tui::SessionNodeView {
                    id: "a-local".into(),
                    alias: "local".into(),
                    local: true,
                    session_display_names: true,
                },
                result: Ok(vec![host_session_summary("same", "z-workspace", 100)]),
            },
            DashboardSessionNodeLoad {
                node: tui::SessionNodeView {
                    id: "b-remote".into(),
                    alias: "work".into(),
                    local: false,
                    session_display_names: false,
                },
                result: Ok(vec![
                    host_session_summary("newest", "remote-workspace", 200),
                    host_session_summary("same", "a-workspace", 100),
                ]),
            },
            DashboardSessionNodeLoad {
                node: tui::SessionNodeView {
                    id: "c-unavailable".into(),
                    alias: "offline".into(),
                    local: false,
                    session_display_names: false,
                },
                result: Err("authentication required".into()),
            },
        ])
        .unwrap();

        assert_eq!(loaded.sessions.len(), 3);
        assert_eq!(
            loaded
                .sessions
                .iter()
                .map(|session| (
                    session.node_id.as_str(),
                    session.workspace_id.as_str(),
                    session.session_id.as_str(),
                ))
                .collect::<Vec<_>>(),
            vec![
                ("b-remote", "remote-workspace", "newest"),
                ("a-local", "z-workspace", "same"),
                ("b-remote", "a-workspace", "same"),
            ]
        );
        assert_eq!(loaded.sessions[0].node_alias, "work");
        assert_eq!(loaded.sessions[1].node_alias, "local");
        assert_eq!(
            loaded.warnings,
            vec!["Node offline: authentication required"]
        );
    }

    #[test]
    fn dashboard_session_summary_preserves_node_qualified_identity_fields() {
        let details = dashboard_session_details(
            &protocol::QualifiedIdentity::new("owner-node", "session-id"),
            inspected_session(AgentState::Inactive, false, Some("/owner/path")),
            Ok("eligible".into()),
        );

        assert_eq!(details.session.node_id, "owner-node");
        assert_eq!(details.session.session_id, "session-id");
        assert_eq!(details.session.workspace_id, "workspace-id");
        assert_eq!(details.source_cwd, Some(PathBuf::from("/owner/path")));
    }

    #[test]
    fn session_open_plan_routes_exact_local_current_shell_and_run() {
        let inspection = current_inspected_session("local-shell", "r1");
        let workspace = workspace(
            "workspace-id",
            "workspace",
            vec![shell("local-shell", "workspace-id", "agent")],
        );
        let plan = session_open_plan(
            &protocol::QualifiedIdentity::new("local-node", "session-id"),
            "local-node",
            &inspection,
            Some(&workspace),
        )
        .unwrap();
        assert_eq!(
            plan,
            SessionOpenPlan::Current {
                node_id: None,
                shell_id: "local-shell".into(),
                run_id: "r1".into(),
                title: "workspace - opencode Session".into(),
                description: "review".into(),
            }
        );
        let opened = std::cell::RefCell::new(Vec::new());
        let message = dispatch_session_open(
            &plan,
            |shell, run, _| {
                opened
                    .borrow_mut()
                    .push(("local", shell.to_owned(), run.to_owned()));
                Ok(())
            },
            |_, _, _, _| panic!("must not route a local Session remotely"),
            |_, _, _| panic!("must not resume a current Session"),
        )
        .unwrap();
        assert_eq!(
            opened.into_inner(),
            vec![("local", "local-shell".into(), "r1".into())]
        );
        assert_eq!(message, "Opened terminal for current Session review");
    }

    #[test]
    fn session_open_plan_routes_exact_remote_current_owner_and_ignores_local_collision() {
        let inspection = current_inspected_session("same-shell", "r1");
        let owner_workspace = workspace(
            "workspace-id",
            "owner",
            vec![shell("same-shell", "workspace-id", "owner-agent")],
        );
        let remote = session_open_plan(
            &protocol::QualifiedIdentity::new("remote-owner", "same-session"),
            "local-node",
            &inspection,
            Some(&owner_workspace),
        )
        .unwrap();
        let local = session_open_plan(
            &protocol::QualifiedIdentity::new("local-node", "same-session"),
            "local-node",
            &inspection,
            Some(&owner_workspace),
        )
        .unwrap();
        assert!(matches!(
            &remote,
            SessionOpenPlan::Current { node_id: Some(node), shell_id, run_id, .. }
                if node == "remote-owner" && shell_id == "same-shell" && run_id == "r1"
        ));
        assert!(matches!(
            local,
            SessionOpenPlan::Current { node_id: None, .. }
        ));
        let routed = std::cell::RefCell::new(Vec::new());
        dispatch_session_open(
            &remote,
            |_, _, _| panic!("remote Session must not use local attachment"),
            |node, shell, run, _| {
                routed
                    .borrow_mut()
                    .push((node.to_owned(), shell.to_owned(), run.to_owned()));
                Ok(())
            },
            |_, _, _| panic!("current Session must not resume historically"),
        )
        .unwrap();
        assert_eq!(
            routed.into_inner(),
            vec![("remote-owner".into(), "same-shell".into(), "r1".into())]
        );
    }

    #[test]
    fn session_open_plan_resumes_exact_remote_historical_owner_identity() {
        let inspection = inspected_session(AgentState::Inactive, false, Some("/owner/project"));
        let plan = session_open_plan(
            &protocol::QualifiedIdentity::new("remote-owner", "exact-session"),
            "local-node",
            &inspection,
            None,
        )
        .unwrap();
        assert!(matches!(
            &plan,
            SessionOpenPlan::Historical { node_id: Some(node), session_id, .. }
                if node == "remote-owner" && session_id == "exact-session"
        ));
        let resumed = std::cell::RefCell::new(Vec::new());
        dispatch_session_open(
            &plan,
            |_, _, _| panic!("historical Session must not attach locally"),
            |_, _, _, _| panic!("historical Session must not attach remotely"),
            |node, session, _| {
                resumed
                    .borrow_mut()
                    .push((node.map(str::to_owned), session.to_owned()));
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(
            resumed.into_inner(),
            vec![(Some("remote-owner".into()), "exact-session".into())]
        );
    }

    #[test]
    fn session_open_plan_resumes_exact_local_historical_identity() {
        let inspection = inspected_session(AgentState::Inactive, false, Some("/local/project"));
        let plan = session_open_plan(
            &protocol::QualifiedIdentity::new("local-node", "exact-session"),
            "local-node",
            &inspection,
            None,
        )
        .unwrap();
        let resumed = std::cell::RefCell::new(Vec::new());
        dispatch_session_open(
            &plan,
            |_, _, _| panic!("historical Session must not attach locally"),
            |_, _, _, _| panic!("historical Session must not attach remotely"),
            |node, session, _| {
                resumed
                    .borrow_mut()
                    .push((node.map(str::to_owned), session.to_owned()));
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(resumed.into_inner(), vec![(None, "exact-session".into())]);
    }

    #[test]
    fn managed_historical_session_supervision_preserves_exact_identity_and_argv() {
        let command = supervised_session_resume_command(
            "/opt/boomux",
            "OpenCode",
            "opencode",
            "ses_exact; literal",
            &[
                "opencode".into(),
                "--session".into(),
                "ses_exact; literal".into(),
            ],
        );
        assert_eq!(
            command,
            [
                "/opt/boomux",
                "agent",
                "supervise",
                "OpenCode",
                "--integration",
                "opencode",
                "--external-session-id",
                "ses_exact; literal",
                "--",
                "opencode",
                "--session",
                "ses_exact; literal",
            ]
        );
        let parsed = Cli::try_parse_from(command).unwrap();
        assert!(matches!(
            parsed.command,
            Some(Commands::Agent {
                command: AgentCommands::Supervise(AgentSuperviseArgs {
                    name: Some(name),
                    integration,
                    external_session_id,
                    command,
                    ..
                })
            }) if name == "OpenCode"
                && integration == "opencode"
                && external_session_id == "ses_exact; literal"
                && command == ["opencode", "--session", "ses_exact; literal"]
        ));
    }

    #[test]
    fn session_open_plan_rejects_done_missing_and_ambiguous_current_targets() {
        let identity = protocol::QualifiedIdentity::new("local-node", "session-id");
        assert!(
            session_open_plan(
                &identity,
                "local-node",
                &inspected_session(AgentState::Done, false, Some("/tmp")),
                None,
            )
            .unwrap_err()
            .contains("permanently done")
        );
        assert!(
            session_open_plan(
                &identity,
                "local-node",
                &current_inspected_session("missing", "r1"),
                None,
            )
            .unwrap_err()
            .contains("Workspace is no longer available")
        );
        let mut inspection = current_inspected_session("first", "r1");
        inspection
            .occurrences
            .push(agent("second-agent", "workspace-id", "second"));
        let workspace = workspace(
            "workspace-id",
            "workspace",
            vec![
                shell("first", "workspace-id", "first"),
                shell("second", "workspace-id", "second"),
            ],
        );
        assert!(
            session_open_plan(&identity, "local-node", &inspection, Some(&workspace),)
                .unwrap_err()
                .contains("multiple current")
        );
    }

    #[test]
    fn session_open_plan_ignores_ended_and_inactive_occurrences() {
        let identity = protocol::QualifiedIdentity::new("local-node", "session-id");
        let mut inspection = current_inspected_session("current", "r1");
        let mut inactive = agent("inactive-agent", "workspace-id", "inactive");
        inactive.observation.state = AgentState::Inactive;
        let mut ended = agent("ended-agent", "workspace-id", "ended");
        ended.ended_at_ms = Some(12);
        inspection.occurrences.extend([inactive, ended]);
        let workspace = workspace(
            "workspace-id",
            "workspace",
            vec![
                shell("current", "workspace-id", "current"),
                shell("inactive", "workspace-id", "inactive"),
                shell("ended", "workspace-id", "ended"),
            ],
        );

        assert!(matches!(
            session_open_plan(&identity, "local-node", &inspection, Some(&workspace)),
            Ok(SessionOpenPlan::Current { shell_id, run_id, .. })
                if shell_id == "current" && run_id == "r1"
        ));
    }

    fn test_skill_home(label: &str) -> PathBuf {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        env::temp_dir().join(format!(
            "boomux-skill-{label}-{}-{unique}",
            std::process::id()
        ))
    }
}

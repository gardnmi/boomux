use std::collections::BTreeSet;
use std::env;
use std::error::Error;
use std::ffi::OsString;
use std::fmt::Write as _;
use std::fs;
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};
use std::thread;

use clap::error::ErrorKind as ClapErrorKind;
use clap::{Args, Parser, Subcommand, ValueEnum};
use uuid::Uuid;

use boomux::protocol::{
    AgentAuthority, AgentInstanceSnapshot, AgentRegistrationSpec, AgentReport, AgentState,
    EventCursor, ShellSnapshot, ShellSpec, ShellStatus, Snapshot, WorkspaceLauncherSnapshot,
    WorkspaceLauncherSpec, WorkspaceSnapshot,
};
use boomux::{attach, client, daemon, protocol};

use crate::integration_management::{
    InstallOutcome, ensure_safe_directory, install_asset_at, regular_file_matches,
    require_absolute_root,
};

mod agent_attention_projection;
mod cli_output;
mod config;
mod git;
mod host_session_source;
mod host_session_titles;
mod integration_management;
mod process_adapter;
mod projects;
mod session_projection;
mod session_transcript;
mod terminal;
mod tui;

const BOOMUX_SKILL: &str = include_str!("../.agents/skills/boomux/SKILL.md");
#[cfg(test)]
const BOOMUX_OPENCODE_PLUGIN: &str = integration_management::OPENCODE_ASSET;
#[cfg(test)]
const BOOMUX_PI_EXTENSION: &str = integration_management::PI_ASSET;
const MAX_HOST_CATALOG_DIRECTORIES: usize = 8;
const JSON_COMMANDS: &[&str] = &[
    "capabilities",
    "list",
    "shells",
    "read",
    "events",
    "workspace.list",
    "workspace.inspect",
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
    "session.list",
    "session.inspect",
    "session.read",
    "daemon.status",
];
const INTEGRATION_FEATURES: &[&str] = &[
    "typed_errors",
    "shell_run_identity",
    "rendered_scrollback",
    "graceful_live_handoff",
    "graceful_exited_handoff",
    "daemon_events",
    "reconnectable_event_cursors",
    "revision_aware_reads",
    "workspace_launchers",
    "run_scoped_agent_instances",
    "protocol_10",
    "protocol_11",
    "protocol_12",
    "protocol_13",
    "protocol_14",
    "protocol_15",
    "protocol_16",
    "restartable_exited_shells",
    "inactive_agent_state",
    "idempotent_agent_ensure",
    "agent_authority_precedence",
    "opencode_lifecycle_plugin",
    "pi_lifecycle_extension",
    "process_adapters",
    "projected_agent_sessions",
    "canonical_session_transcripts",
    "transcript_pagination",
    "durable_session_source_context",
    "revision_aware_agent_wait",
    "persistent_agent_attention",
    "desktop_notifications",
    "integration_management",
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
    subcommand_value_name = "SUBCOMMAND"
)]
struct Cli {
    /// Emit the stable boomux.cli/v1 JSON envelope
    #[arg(long, global = true)]
    json: bool,

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
    /// Check that Boomux's dependencies and daemon are available
    Doctor,
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
    /// Close a shell by name or shell ID
    Close { target: String },
    /// Manage workspaces
    Workspace {
        #[command(subcommand)]
        command: WorkspaceCommands,
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
    /// Discover projected agent sessions
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
    /// Open a shell in a new terminal window
    Open {
        shell_id: String,
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
        takeover: bool,
        #[arg(long)]
        restart_exited: bool,
    },
}

#[derive(Subcommand)]
enum WorkspaceCommands {
    /// List workspaces
    List,
    /// Create an empty workspace
    Create { name: String },
    /// Open terminal windows and invoke launchers
    Open { target: String },
    /// Show a workspace and its shells
    Inspect { target: String },
    /// Rename a workspace
    Rename { target: String, name: String },
    /// Close a workspace and all of its shells
    Close { target: String },
}

#[derive(Subcommand)]
enum LauncherCommands {
    /// List launchers in a workspace
    List {
        #[arg(long, value_name = "NAME_OR_ID")]
        workspace: String,
    },
    /// Create a launcher in a workspace
    Create {
        name: String,
        #[arg(long, value_name = "NAME_OR_ID")]
        workspace: String,
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
    /// Create a pending shell in a workspace
    Create {
        workspace: String,
        #[arg(long)]
        name: Option<String>,
        #[arg(long, default_value = ".")]
        cwd: PathBuf,
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
enum SessionCommands {
    /// List projected agent sessions
    List {
        #[arg(long, value_name = "NAME_OR_ID")]
        workspace: Option<String>,
    },
    /// Show a projected session by exact opaque ID
    #[command(alias = "get")]
    Inspect { session_id: String },
    /// Read canonical messages and tool activity by exact opaque ID
    Read {
        session_id: String,
        #[arg(long)]
        before: Option<String>,
        #[arg(long, default_value_t = 100, value_parser = clap::value_parser!(u16).range(1..=1000))]
        limit: u16,
        #[arg(long, default_value_t = 1024 * 1024, value_parser = clap::value_parser!(u32).range(1..=4 * 1024 * 1024))]
        max_bytes: u32,
    },
}

#[derive(Args)]
struct AgentSuperviseArgs {
    name: String,
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
    name: String,
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
enum IntegrationCommands {
    /// List integrations bundled with this Boomux binary
    List,
    /// Inspect host, asset, and runtime reporting status
    Status {
        #[arg(value_enum)]
        integration: Option<integration_management::IntegrationId>,
    },
    /// Install one integration or every bundled integration
    Install {
        #[arg(value_enum, required_unless_present = "all", conflicts_with = "all")]
        integration: Option<integration_management::IntegrationId>,
        #[arg(long, conflicts_with = "integration")]
        all: bool,
        #[arg(long)]
        force: bool,
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
    /// Report whether the daemon is accepting requests
    Status,
    /// Replace the daemon without changing pending workspace state
    Restart,
    /// Stop the daemon and its managed shells
    Stop,
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
    let command = command_name(&cli);
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
    if cli.json && !supports_json(&cli) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("--json is not supported for {}", command_name(&cli)),
        )
        .into());
    }
    match cli.command.as_ref() {
        Some(Commands::Daemon {
            command: DaemonCommands::Run,
        }) => {
            daemon::run_with_notifications(config::load_notification_settings()?)?;
            return Ok(CliExit::Success);
        }
        Some(Commands::Daemon {
            command: DaemonCommands::ReceiveHandoff { channel },
        }) => {
            daemon::receive_handoff_with_notifications(
                *channel,
                config::load_notification_settings()?,
            )?;
            return Ok(CliExit::Success);
        }
        Some(Commands::Attach {
            shell_id,
            takeover,
            restart_exited,
        }) => {
            attach::run(shell_id, *takeover, *restart_exited)?;
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
        Some(Commands::Doctor) => doctor(cli.terminal.as_deref()),
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
        Some(Commands::Close { target }) => close_shell(&target),
        Some(Commands::Workspace { command }) => {
            workspace_command(command, cli.json, cli.terminal.as_deref())
        }
        Some(Commands::Shell { command }) => shell_command(command, cli.json),
        Some(Commands::Launcher { command }) => launcher_command(command, cli.json),
        Some(Commands::Agent {
            command: AgentCommands::Supervise(arguments),
        }) => return supervise_agent(arguments).map(CliExit::Child),
        Some(Commands::Agent { command }) => agent_command(command, cli.json),
        Some(Commands::Attention { command }) => attention_command(command, cli.json),
        Some(Commands::Session { command }) => session_command(command, cli.json),
        Some(Commands::Integration { command }) => integration_command(command, cli.json),
        Some(Commands::Skill {
            command: SkillCommands::Install { force },
        }) => install_skill(force),
        Some(Commands::Opencode {
            command: OpenCodeCommands::Install { force },
        }) => install_opencode(force),
        Some(Commands::Pi {
            command: PiCommands::Install { force },
        }) => install_pi(force),
        Some(Commands::Open {
            shell_id,
            title,
            takeover,
        }) => {
            let terminal = effective_terminal(cli.terminal.as_deref())?;
            open_shell(&shell_id, title.as_deref(), takeover, terminal.as_deref())
        }
        Some(Commands::Prompt) => print_prompt_label(),
        Some(Commands::Daemon { command }) => daemon_control(command, cli.json),
        Some(Commands::Attach { .. }) => unreachable!(),
        None => dashboard(cli.terminal.as_deref()),
    };
    result?;
    Ok(CliExit::Success)
}

fn command_name(cli: &Cli) -> &'static str {
    match cli.command.as_ref() {
        Some(Commands::Capabilities) => "capabilities",
        Some(Commands::List) => "list",
        Some(Commands::Shells) => "shells",
        Some(Commands::Read { .. }) => "read",
        Some(Commands::Events { .. }) => "events",
        Some(Commands::Workspace {
            command: WorkspaceCommands::List,
        }) => "workspace.list",
        Some(Commands::Workspace {
            command: WorkspaceCommands::Inspect { .. },
        }) => "workspace.inspect",
        Some(Commands::Shell {
            command: ShellCommands::Inspect { .. },
        }) => "shell.inspect",
        Some(Commands::Launcher {
            command: LauncherCommands::List { .. },
        }) => "launcher.list",
        Some(Commands::Launcher {
            command: LauncherCommands::Inspect { .. },
        }) => "launcher.inspect",
        Some(Commands::Agent {
            command: AgentCommands::List { .. },
        }) => "agent.list",
        Some(Commands::Agent {
            command: AgentCommands::Inspect { .. },
        }) => "agent.inspect",
        Some(Commands::Agent {
            command: AgentCommands::Wait { .. },
        }) => "agent.wait",
        Some(Commands::Agent {
            command: AgentCommands::Register(..),
        }) => "agent.register",
        Some(Commands::Agent {
            command: AgentCommands::Ensure(..),
        }) => "agent.ensure",
        Some(Commands::Agent {
            command: AgentCommands::Supervise(..),
        }) => "agent.supervise",
        Some(Commands::Agent {
            command: AgentCommands::Report { .. },
        }) => "agent.report",
        Some(Commands::Attention {
            command: AttentionCommands::List { .. },
        }) => "attention.list",
        Some(Commands::Attention {
            command: AttentionCommands::Acknowledge { .. },
        }) => "attention.acknowledge",
        Some(Commands::Session {
            command: SessionCommands::List { .. },
        }) => "session.list",
        Some(Commands::Session {
            command: SessionCommands::Inspect { .. },
        }) => "session.inspect",
        Some(Commands::Session {
            command: SessionCommands::Read { .. },
        }) => "session.read",
        Some(Commands::Integration {
            command: IntegrationCommands::List,
        }) => "integration.list",
        Some(Commands::Integration {
            command: IntegrationCommands::Status { .. },
        }) => "integration.status",
        Some(Commands::Integration {
            command: IntegrationCommands::Install { .. },
        }) => "integration.install",
        Some(Commands::Daemon {
            command: DaemonCommands::Status,
        }) => "daemon.status",
        Some(Commands::Workspace { .. }) => "workspace",
        Some(Commands::Shell { .. }) => "shell",
        Some(Commands::Launcher { .. }) => "launcher",
        Some(Commands::Daemon { .. }) => "daemon",
        Some(Commands::Ui) | None => "ui",
        Some(Commands::Doctor) => "doctor",
        Some(Commands::Close { .. }) => "close",
        Some(Commands::Skill { .. }) => "skill",
        Some(Commands::Opencode { .. }) => "opencode",
        Some(Commands::Pi { .. }) => "pi",
        Some(Commands::Open { .. }) => "open",
        Some(Commands::Prompt) => "prompt",
        Some(Commands::Attach { .. }) => "attach",
    }
}

fn supports_json(cli: &Cli) -> bool {
    matches!(
        cli.command.as_ref(),
        Some(
            Commands::Capabilities
                | Commands::List
                | Commands::Shells
                | Commands::Read { .. }
                | Commands::Events { .. }
        ) | Some(Commands::Workspace {
            command: WorkspaceCommands::List | WorkspaceCommands::Inspect { .. }
        }) | Some(Commands::Shell {
            command: ShellCommands::Inspect { .. }
        }) | Some(Commands::Launcher {
            command: LauncherCommands::List { .. } | LauncherCommands::Inspect { .. }
        }) | Some(Commands::Agent {
            command: AgentCommands::List { .. }
                | AgentCommands::Inspect { .. }
                | AgentCommands::Wait { .. }
                | AgentCommands::Register(..)
                | AgentCommands::Ensure(..)
                | AgentCommands::Report { .. }
        }) | Some(Commands::Daemon {
            command: DaemonCommands::Status
        }) | Some(Commands::Attention {
            command: AttentionCommands::List { .. } | AttentionCommands::Acknowledge { .. }
        }) | Some(Commands::Session {
            command: SessionCommands::List { .. }
                | SessionCommands::Inspect { .. }
                | SessionCommands::Read { .. }
        }) | Some(Commands::Integration {
            command: IntegrationCommands::List
                | IntegrationCommands::Status { .. }
                | IntegrationCommands::Install { .. }
        })
    )
}

fn daemon_control(command: DaemonCommands, json: bool) -> Result<(), Box<dyn Error>> {
    let client = client::connect()?;
    match command {
        DaemonCommands::Status if json => {
            let protocol_version = client.protocol_version()?;
            cli_output::print(
                "daemon.status",
                serde_json::json!({
                    "status": "running",
                    "protocol_version": protocol_version,
                    "socket_path": client.socket_path().display().to_string(),
                }),
            )?
        }
        DaemonCommands::Status => println!(
            "running (protocol {}, {})",
            client.protocol_version()?,
            client.socket_path().display()
        ),
        DaemonCommands::Restart => {
            client.restart()?;
            println!("Restarted Boomux daemon");
        }
        DaemonCommands::Stop => {
            client.shutdown()?;
            println!("Stopped Boomux daemon");
        }
        DaemonCommands::Run | DaemonCommands::ReceiveHandoff { .. } => unreachable!(),
    }
    Ok(())
}

fn should_open_new_window(new_window: bool, terminal: Option<&str>) -> bool {
    new_window || terminal.is_some()
}

fn effective_terminal(override_entry: Option<&str>) -> Result<Option<String>, Box<dyn Error>> {
    if let Some(entry) = override_entry {
        return Ok(Some(entry.to_owned()));
    }
    Ok(config::load()?.terminal)
}

fn dashboard(terminal_override: Option<&str>) -> Result<(), Box<dyn Error>> {
    ensure_host_terminal()?;
    let launch_cwd = resolve_directory(Path::new("."))?;
    let client = client::connect_or_start()?;
    let mut git_cache = git::Cache::default();
    let mut title_cache = host_session_titles::Cache::default();
    let mut views = dashboard_views_with_catalog(
        &client.snapshot()?.workspaces,
        &mut git_cache,
        &mut title_cache,
    );
    enrich_session_titles(&mut views, &mut title_cache);
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

    tui::run(
        views,
        project_context,
        tui::Actions {
            on_restore: |workspace_id: &str| {
                let workspace = client
                    .get_workspace(workspace_id)
                    .map_err(|error| error.to_string())?;
                open_workspace(&workspace, terminal.as_deref())
                    .map_err(|error| error.to_string())?;
                Ok(format!(
                    "Opened {} launcher(s) and {} shell(s) for {}",
                    workspace.launchers.len(),
                    workspace.shells.len(),
                    workspace.name
                ))
            },
            on_open: |target: &tui::OpenTarget| {
                dispatch_dashboard_open(
                    target,
                    |shell_id| {
                        open_dashboard_shell(&client, shell_id, terminal.as_deref())
                            .map_err(|error| error.to_string())
                    },
                    |workspace_id, launcher_id| {
                        let workspace = client
                            .get_workspace(workspace_id)
                            .map_err(|error| error.to_string())?;
                        let launcher = client
                            .get_launcher(launcher_id)
                            .map_err(|error| error.to_string())?;
                        invoke_workspace_launcher(&workspace, &launcher)
                            .map_err(|error| error.to_string())?;
                        Ok(format!(
                            "Launched {} from {}",
                            launcher.name, workspace.name
                        ))
                    },
                )
            },
            on_close: |target: &tui::CloseTarget| match target {
                tui::CloseTarget::Workspace(workspace_id) => {
                    let name = client
                        .get_workspace(workspace_id)
                        .map(|workspace| workspace.name)
                        .unwrap_or_else(|_| "workspace".into());
                    client
                        .close_workspace(workspace_id)
                        .map_err(|error| error.to_string())?;
                    Ok(format!(
                        "Closed {name}, its launchers, and all of its shells"
                    ))
                }
                tui::CloseTarget::Shell(shell_id) => {
                    let name = client
                        .get_shell(shell_id)
                        .map(|shell| shell.name)
                        .unwrap_or_else(|_| "shell".into());
                    client
                        .close_shell(shell_id)
                        .map_err(|error| error.to_string())?;
                    Ok(format!("Closed shell {name}"))
                }
                tui::CloseTarget::Launcher(launcher_id) => {
                    let name = client
                        .get_launcher(launcher_id)
                        .map(|launcher| launcher.name)
                        .unwrap_or_else(|_| "launcher".into());
                    client
                        .remove_launcher(launcher_id)
                        .map_err(|error| error.to_string())?;
                    Ok(format!("Removed launcher {name}"))
                }
            },
            on_create_workspace: |name: &str| {
                create_dashboard_workspace(&client, name).map_err(|error| error.to_string())
            },
            on_create_shell: |workspace_id: &str| {
                create_dashboard_shell(&client, workspace_id, &launch_cwd)
                    .map_err(|error| error.to_string())
            },
            on_rename: |target: &tui::RenameTarget, name: &str| match target {
                tui::RenameTarget::Workspace(workspace_id) => {
                    client
                        .rename_workspace(workspace_id, name)
                        .map_err(|error| error.to_string())?;
                    Ok(format!("Renamed workspace to {name}"))
                }
                tui::RenameTarget::Shell(shell_id) => {
                    client
                        .rename_shell(shell_id, name)
                        .map_err(|error| error.to_string())?;
                    Ok(format!("Renamed shell to {name}"))
                }
                tui::RenameTarget::Launcher(launcher_id) => {
                    client
                        .rename_launcher(launcher_id, name)
                        .map_err(|error| error.to_string())?;
                    Ok(format!("Renamed launcher to {name}"))
                }
            },
            on_refresh: || {
                let snapshot = client.snapshot().map_err(|error| error.to_string())?;
                let mut views = dashboard_views_with_catalog(
                    &snapshot.workspaces,
                    &mut git_cache,
                    &mut title_cache,
                );
                enrich_session_titles(&mut views, &mut title_cache);
                Ok(views)
            },
            on_terminal_preview: |shell_id: &str| {
                let bytes = client
                    .read_shell(shell_id, READ_BYTES)
                    .map_err(|error| error.to_string())?;
                Ok(recent_lines(&String::from_utf8_lossy(&bytes), 500))
            },
        },
    )?;
    Ok(())
}

fn dispatch_dashboard_open<S, L>(
    target: &tui::OpenTarget,
    mut open_shell: S,
    mut launch: L,
) -> Result<String, String>
where
    S: FnMut(&str) -> Result<String, String>,
    L: FnMut(&str, &str) -> Result<String, String>,
{
    match target {
        tui::OpenTarget::Shell(shell_id) => open_shell(shell_id),
        tui::OpenTarget::Launcher {
            workspace_id,
            launcher_id,
        } => launch(workspace_id, launcher_id),
    }
}

#[cfg(test)]
fn dashboard_views(
    workspaces: &[WorkspaceSnapshot],
    git_cache: &mut git::Cache,
) -> Vec<tui::WorkspaceView> {
    let sessions = session_projection::project_workspaces(workspaces);
    dashboard_views_from_sessions(workspaces, git_cache, &sessions)
}

fn dashboard_views_with_catalog(
    workspaces: &[WorkspaceSnapshot],
    git_cache: &mut git::Cache,
    title_cache: &mut host_session_titles::Cache,
) -> Vec<tui::WorkspaceView> {
    let catalog = cached_host_catalog(workspaces, title_cache);
    let sessions = session_projection::project_workspaces_with_catalog(workspaces, Some(&catalog));
    dashboard_views_from_sessions(workspaces, git_cache, &sessions)
}

fn dashboard_views_from_sessions(
    workspaces: &[WorkspaceSnapshot],
    git_cache: &mut git::Cache,
    sessions: &[session_projection::SessionProjection],
) -> Vec<tui::WorkspaceView> {
    workspaces
        .iter()
        .map(|workspace| {
            let sessions = workspace_session_views(
                sessions
                    .iter()
                    .filter(|session| session.workspace_id == workspace.id),
            );
            let agent_summary = agent_attention_projection::summarize_workspace(workspace);
            let attention =
                agent_attention_projection::project_attention(std::slice::from_ref(workspace))
                    .into_iter()
                    .next()
                    .map(|item| tui::WorkspaceAttentionView {
                        agent_name: item.agent.name,
                        reason: agent_attention_projection::attention_reason(item.attention.reason)
                            .into(),
                        evidence: item.attention.observation.evidence,
                        observed_at_ms: item.attention.observation.observed_at_ms,
                        observation_is_current: item.observation_is_current,
                    });
            let shells = workspace.shells.iter().map(|shell| {
                let git = git_cache.inspect(&shell.cwd);
                let shell_view = tui::TerminalView {
                    id: shell.id.clone(),
                    name: shell.name.clone(),
                    status: shell_status(&shell.status).into(),
                    directory: shell.cwd.display().to_string(),
                    branch: git.branch,
                    command: shell.command.join(" "),
                    argv: shell.command.clone(),
                    run: shell.run.as_ref().map(|run| tui::TerminalRunView {
                        id: run.id.clone(),
                        generation: run.generation,
                        started_at_ms: run.started_at_ms,
                        ended_at_ms: run.ended_at_ms,
                        exit_reason: run.exit_reason.as_ref().map(shell_exit_reason),
                        output_revision: run.output_revision,
                    }),
                };
                let agent = matches!(shell.status, ShellStatus::Running)
                    .then(|| {
                        shell.run.as_ref().and_then(|run| {
                            workspace
                                .agents
                                .iter()
                                .filter(|agent| {
                                    agent.workspace_id == workspace.id
                                        && agent.shell_id == shell.id
                                        && agent.run_id == run.id
                                        && agent.ended_at_ms.is_none()
                                        && !matches!(
                                            agent.observation.state,
                                            AgentState::Inactive | AgentState::Done
                                        )
                                })
                                .max_by(|left, right| {
                                    left.observation
                                        .observed_at_ms
                                        .cmp(&right.observation.observed_at_ms)
                                        .then_with(|| left.id.cmp(&right.id))
                                })
                        })
                    })
                    .flatten();
                let suppress_foreground_hint = shell.run.as_ref().is_some_and(|run| {
                    workspace.agents.iter().any(|agent| {
                        agent.shell_id == shell.id
                            && agent.run_id == run.id
                            && shell.foreground_process.as_deref()
                                == Some(agent.integration.as_str())
                            && (agent.ended_at_ms.is_some()
                                || matches!(
                                    agent.observation.state,
                                    AgentState::Inactive | AgentState::Done
                                ))
                    })
                });
                match (agent, shell.foreground_process.as_deref()) {
                    (Some(agent), _) => tui::WorkspaceItemView::AgentShell(tui::AgentShellView {
                        shell: shell_view,
                        agent: Some(tui::AgentView {
                            id: agent.id.clone(),
                            state: cli_output::agent_state(agent.observation.state).into(),
                            integration: agent.integration.clone(),
                            external_session_id: agent.external_session_id.clone(),
                            authority: cli_output::agent_authority(agent.observation.authority)
                                .into(),
                            confidence: agent.observation.confidence,
                            evidence: agent.observation.evidence.clone(),
                        }),
                    }),
                    (None, Some("opencode" | "pi")) if !suppress_foreground_hint => {
                        tui::WorkspaceItemView::AgentShell(tui::AgentShellView {
                            shell: shell_view,
                            agent: None,
                        })
                    }
                    (None, _) => tui::WorkspaceItemView::Shell(shell_view),
                }
            });
            let launchers = workspace.launchers.iter().map(|launcher| {
                tui::WorkspaceItemView::Launcher(tui::LauncherView {
                    id: launcher.id.clone(),
                    name: launcher.name.clone(),
                    directory: launcher.cwd.display().to_string(),
                    command: launcher.command.join(" "),
                    argv: launcher.command.clone(),
                })
            });
            tui::WorkspaceView {
                id: workspace.id.clone(),
                name: workspace.name.clone(),
                items: shells.chain(launchers).collect(),
                sessions,
                agent_state_counts: agent_summary.states,
                attention_count: agent_summary.attention_count,
                attention,
            }
        })
        .collect()
}

fn workspace_session_views<'a>(
    sessions: impl IntoIterator<Item = &'a session_projection::SessionProjection>,
) -> Vec<tui::AgentSessionView> {
    sessions
        .into_iter()
        .map(|session| {
            let runs = session
                .occurrences
                .iter()
                .map(|occurrence| tui::AgentSessionRunView {
                    shell_name: occurrence.retained_shell_name.clone(),
                    directory: occurrence.source_cwd.clone(),
                })
                .collect();
            tui::AgentSessionView {
                id: session.id.clone(),
                label: session.description.clone(),
                integration: session.integration.clone(),
                external_session_id: session.external_session_id.clone(),
                state: cli_output::agent_state(session.state).into(),
                state_is_current: session.state_is_current,
                last_at_ms: session.last_at_ms,
                source_cwd: session.source_cwd.clone(),
                runs,
            }
        })
        .collect()
}

fn workspace_source_directories(workspaces: &[WorkspaceSnapshot]) -> BTreeSet<PathBuf> {
    workspaces
        .iter()
        .flat_map(|workspace| {
            workspace
                .shells
                .iter()
                .map(|shell| shell.cwd.clone())
                .chain(
                    workspace
                        .launchers
                        .iter()
                        .map(|launcher| launcher.cwd.clone()),
                )
                .chain(
                    workspace
                        .agents
                        .iter()
                        .filter_map(|agent| agent.cwd.clone()),
                )
        })
        .collect()
}

fn cached_host_catalog(
    workspaces: &[WorkspaceSnapshot],
    cache: &mut host_session_titles::Cache,
) -> Vec<host_session_titles::HostSession> {
    let mut catalog = Vec::new();
    for directory in workspace_source_directories(workspaces)
        .into_iter()
        .take(MAX_HOST_CATALOG_DIRECTORIES)
    {
        for integration in host_session_titles::catalog_integrations() {
            if let Some(sessions) = cache.catalog(integration, &directory) {
                catalog.extend(sessions);
            }
        }
    }
    catalog
}

fn discover_host_catalog(
    workspaces: &[WorkspaceSnapshot],
) -> Vec<host_session_titles::HostSession> {
    let directories = workspace_source_directories(workspaces)
        .into_iter()
        .take(MAX_HOST_CATALOG_DIRECTORIES)
        .collect::<Vec<_>>();
    let requests = directories.into_iter().flat_map(|directory| {
        host_session_titles::catalog_integrations()
            .map(move |integration| (integration, directory.clone()))
    });
    thread::scope(|scope| {
        requests
            .map(|(integration, directory)| {
                scope.spawn(move || host_session_titles::catalog(integration, &directory))
            })
            .collect::<Vec<_>>()
            .into_iter()
            .filter_map(|handle| handle.join().ok().flatten())
            .flatten()
            .collect()
    })
}

fn enrich_session_titles(
    workspaces: &mut [tui::WorkspaceView],
    title_cache: &mut host_session_titles::Cache,
) {
    enrich_session_titles_with(workspaces, |integration, directory, external_session_id| {
        title_cache.title(integration, directory, external_session_id)
    });
}

fn enrich_session_titles_with<F>(workspaces: &mut [tui::WorkspaceView], mut title: F)
where
    F: FnMut(&str, &Path, &str) -> Option<String>,
{
    for session in workspaces
        .iter_mut()
        .flat_map(|workspace| workspace.sessions.iter_mut())
    {
        let Some(external_session_id) = session.external_session_id.as_deref() else {
            continue;
        };
        let Some(directory) = session
            .runs
            .iter()
            .rev()
            .find_map(|run| run.directory.as_deref())
            .or(session.source_cwd.as_deref())
        else {
            continue;
        };
        if let Some(host_title) = title(&session.integration, directory, external_session_id) {
            session.label = host_title;
        }
    }
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
                let shell_name = unique_shell_name("shell", &workspace.shells);
                (
                    client.create_shell(
                        &workspace.id,
                        shell_spec(shell_name, &directory, startup_command),
                    )?,
                    false,
                )
            } else {
                (
                    client
                        .create_workspace(
                            &name,
                            vec![shell_spec("shell-1", &directory, startup_command)],
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
            "shell-1",
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
        Ok(attach::run(&shell.id, true, false)?)
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

fn create_dashboard_workspace(
    client: &client::Client,
    name: &str,
) -> Result<String, Box<dyn Error>> {
    let name = name.trim();
    if name.is_empty() {
        return Err("workspace name cannot be empty".into());
    }
    client.create_workspace(name, Vec::new())?;
    Ok(format!("Created empty workspace {name}"))
}

fn create_dashboard_shell(
    client: &client::Client,
    workspace_id: &str,
    launch_cwd: &Path,
) -> Result<String, Box<dyn Error>> {
    let workspace = client.get_workspace(workspace_id)?;
    let name = unique_shell_name("shell", &workspace.shells);
    client.create_shell(workspace_id, ShellSpec::login(&name, launch_cwd))?;
    Ok(format!("Created {name} in {}", workspace.name))
}

fn unique_shell_name(base_name: &str, shells: &[ShellSnapshot]) -> String {
    let highest_suffix = shells
        .iter()
        .filter_map(|shell| {
            if shell.name == base_name {
                Some(1)
            } else {
                shell
                    .name
                    .strip_prefix(base_name)
                    .and_then(|suffix| suffix.strip_prefix('-'))
                    .and_then(|suffix| suffix.parse::<usize>().ok())
            }
        })
        .max();
    highest_suffix.map_or_else(
        || base_name.to_owned(),
        |suffix| format!("{base_name}-{}", suffix + 1),
    )
}

fn integration_command(command: IntegrationCommands, json: bool) -> Result<(), Box<dyn Error>> {
    match command {
        IntegrationCommands::List => list_integrations(json),
        IntegrationCommands::Status { integration } => integration_status(integration, json),
        IntegrationCommands::Install {
            integration,
            all,
            force,
        } => {
            let integrations = if all {
                integration_management::IntegrationId::ALL.to_vec()
            } else {
                vec![integration.ok_or_else(|| {
                    cli_output::failure(
                        "invalid_argument",
                        "integration install requires a name or --all",
                    )
                })?]
            };
            install_integrations(&integrations, force, json)
        }
    }
}

fn list_integrations(json: bool) -> Result<(), Box<dyn Error>> {
    let integrations = integration_management::IntegrationId::ALL
        .into_iter()
        .map(integration_management::IntegrationSummary::from)
        .collect::<Vec<_>>();
    if json {
        return cli_output::print(
            "integration.list",
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
        || integration_management::IntegrationId::ALL.to_vec(),
        |integration| vec![integration],
    );
    let statuses = integrations
        .into_iter()
        .map(|integration| {
            integration_management::inspect(integration, &environment, snapshot.as_ref())
        })
        .collect::<Vec<_>>();
    if json {
        return cli_output::print(
            "integration.status",
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
            format_recommended_action(status.recommended_action)
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

fn format_recommended_action(action: integration_management::RecommendedAction) -> &'static str {
    match action {
        integration_management::RecommendedAction::None => "none",
        integration_management::RecommendedAction::Install => "install integration",
        integration_management::RecommendedAction::Replace => "replace with --force",
        integration_management::RecommendedAction::RestartHost => "restart host",
        integration_management::RecommendedAction::InspectError => "inspect reported error",
    }
}

fn install_integrations(
    integrations: &[integration_management::IntegrationId],
    force: bool,
    json: bool,
) -> Result<(), Box<dyn Error>> {
    let environment = integration_management::Environment::from_process();
    for integration in integrations {
        integration_management::preflight_install(*integration, &environment, force)?;
    }
    let results = integrations
        .iter()
        .copied()
        .map(|integration| integration_management::install(integration, &environment, force))
        .collect::<Result<Vec<_>, _>>()?;
    if json {
        return cli_output::print(
            "integration.install",
            serde_json::json!({ "integrations": results }),
        );
    }
    print_integration_install_results(&results);
    Ok(())
}

fn print_integration_install_results(results: &[integration_management::InstallResult]) {
    for result in results {
        let spec = result.integration.spec();
        match result.result {
            integration_management::InstallOutcome::Unchanged => println!(
                "Boomux {} {} is already installed at {}",
                spec.display_name, spec.asset_name, result.path
            ),
            integration_management::InstallOutcome::Installed => println!(
                "Installed Boomux {} {} at {}",
                spec.display_name, spec.asset_name, result.path
            ),
            integration_management::InstallOutcome::Replaced => println!(
                "Replaced Boomux {} {} at {}",
                spec.display_name, spec.asset_name, result.path
            ),
        }
        if result.restart_required {
            println!("{}", spec.reload_message);
        }
    }
}

fn capabilities(json: bool) -> Result<(), Box<dyn Error>> {
    let error_codes = [
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
        "cursor_expired",
        "run_changed",
        "revision_ahead",
        "context_required",
        "ambiguous_target",
        "unsupported_integration",
        "session_source_unavailable",
        "session_source_invalid",
        "session_source_too_large",
        "internal",
        "unknown",
    ];
    let integration_hosts = integration_management::IntegrationId::ALL
        .into_iter()
        .map(|integration| {
            let spec = integration.spec();
            (
                spec.name.to_owned(),
                serde_json::json!({
                    "package": spec.package,
                    "validated_version": spec.validated_version,
                }),
            )
        })
        .collect::<serde_json::Map<_, _>>();
    if json {
        return cli_output::print(
            "capabilities",
            serde_json::json!({
                "cli_version": env!("CARGO_PKG_VERSION"),
                "daemon_protocol_version": protocol::PROTOCOL_VERSION,
                "json_schemas": [cli_output::SCHEMA],
                "json_commands": JSON_COMMANDS,
                "features": INTEGRATION_FEATURES,
                "session_transcript_integrations": session_transcript::supported_integrations(),
                "integration_hosts": integration_hosts,
                "error_codes": error_codes,
            }),
        );
    }
    println!("CLI VERSION\t{}", env!("CARGO_PKG_VERSION"));
    println!("DAEMON PROTOCOL\t{}", protocol::PROTOCOL_VERSION);
    println!("JSON SCHEMAS\t{}", cli_output::SCHEMA);
    println!("JSON COMMANDS\t{}", JSON_COMMANDS.join(","));
    println!("FEATURES\t{}", INTEGRATION_FEATURES.join(","));
    println!(
        "SESSION TRANSCRIPT INTEGRATIONS\t{}",
        session_transcript::supported_integrations().join(",")
    );
    println!(
        "INTEGRATION HOSTS\t{}",
        integration_management::IntegrationId::ALL
            .into_iter()
            .map(|integration| {
                let spec = integration.spec();
                format!("{}={}", spec.name, spec.validated_version)
            })
            .collect::<Vec<_>>()
            .join(",")
    );
    println!("ERROR CODES\t{}", error_codes.join(","));
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
        return cli_output::print("list", serde_json::json!({ "shells": shells }));
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

fn workspace_command(
    command: WorkspaceCommands,
    json: bool,
    terminal_override: Option<&str>,
) -> Result<(), Box<dyn Error>> {
    let client = client::connect_or_start()?;
    match command {
        WorkspaceCommands::List => {
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
                return cli_output::print(
                    "workspace.list",
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
        WorkspaceCommands::Create { name } => {
            let name = cli_name(name, "workspace")?;
            let workspace = client.create_workspace(name, Vec::new())?;
            println!("Created workspace {} ({})", workspace.name, workspace.id);
        }
        WorkspaceCommands::Open { target } => {
            let snapshot = client.snapshot()?;
            let workspace = resolve_workspace_target(&snapshot.workspaces, &target)?;
            let terminal = effective_terminal(terminal_override)?;
            open_workspace(workspace, terminal.as_deref())?;
            println!(
                "Opened {} launcher(s) and {} shell(s) for {}",
                workspace.launchers.len(),
                workspace.shells.len(),
                workspace.name
            );
        }
        WorkspaceCommands::Inspect { target } => {
            let snapshot = client.snapshot()?;
            let workspace = resolve_workspace_target(&snapshot.workspaces, &target)?;
            let agent_summary = agent_attention_projection::summarize_workspace(workspace);
            if json {
                let shells = workspace
                    .shells
                    .iter()
                    .map(|shell| cli_output::shell(shell, Some(&workspace.name)))
                    .collect::<Vec<_>>();
                return cli_output::print(
                    "workspace.inspect",
                    serde_json::json!({
                        "workspace": {
                            "id": workspace.id,
                            "name": workspace.name,
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
            let snapshot = client.snapshot()?;
            let workspace = resolve_workspace_target(&snapshot.workspaces, &target)?;
            client.rename_workspace(&workspace.id, &name)?;
            println!("Renamed workspace {} to {name}", workspace.name);
        }
        WorkspaceCommands::Close { target } => {
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
    }
    Ok(())
}

fn shell_command(command: ShellCommands, json: bool) -> Result<(), Box<dyn Error>> {
    let client = client::connect_or_start()?;
    match command {
        ShellCommands::Create {
            workspace,
            name,
            cwd,
            command,
        } => {
            let snapshot = client.snapshot()?;
            let workspace = resolve_workspace_target(&snapshot.workspaces, &workspace)?;
            let name = name
                .map(|name| cli_name(name, "shell"))
                .transpose()?
                .unwrap_or_else(|| unique_shell_name("shell", &workspace.shells));
            let cwd = resolve_directory(&cwd)?;
            let shell = client.create_shell(&workspace.id, shell_spec(name, &cwd, &command))?;
            println!("Created pending shell {} ({})", shell.name, shell.id);
        }
        ShellCommands::Inspect { target, workspace } => {
            let snapshot = client.snapshot()?;
            let shell = resolve_cli_shell(&snapshot, &target, workspace.as_deref())?;
            let workspace = snapshot
                .workspaces
                .iter()
                .find(|workspace| workspace.id == shell.workspace_id)
                .ok_or_else(|| {
                    cli_output::failure("not_found", "shell workspace no longer exists")
                })?;
            if json {
                return cli_output::print(
                    "shell.inspect",
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
            let workspace = resolve_workspace_target(&snapshot.workspaces, &workspace)?;
            if json {
                let launchers = workspace
                    .launchers
                    .iter()
                    .map(|launcher| cli_output::launcher(launcher, Some(&workspace.name)))
                    .collect::<Vec<_>>();
                return cli_output::print(
                    "launcher.list",
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
            cwd,
            command,
        } => {
            let name = cli_name(name, "workspace launcher")?;
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
            let launcher = resolve_cli_launcher(&snapshot, &target, workspace.as_deref())?;
            let workspace = snapshot
                .workspaces
                .iter()
                .find(|workspace| workspace.id == launcher.workspace_id)
                .ok_or_else(|| {
                    cli_output::failure("not_found", "launcher workspace no longer exists")
                })?;
            if json {
                return cli_output::print(
                    "launcher.inspect",
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
        LauncherCommands::Rename {
            target,
            name,
            workspace,
        } => {
            let name = cli_name(name, "workspace launcher")?;
            let snapshot = client.snapshot()?;
            let launcher = resolve_cli_launcher(&snapshot, &target, workspace.as_deref())?;
            client.rename_launcher(&launcher.id, &name)?;
            println!("Renamed launcher {} to {name}", launcher.name);
        }
        LauncherCommands::Remove { target, workspace } => {
            let snapshot = client.snapshot()?;
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
                return cli_output::print("agent.list", serde_json::json!({ "agents": agents }));
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
                return cli_output::print(
                    "agent.inspect",
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
                return cli_output::print(
                    "agent.wait",
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
                return cli_output::print(
                    "agent.report",
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
                return cli_output::print(
                    "attention.list",
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
                return cli_output::print(
                    "attention.acknowledge",
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

fn session_command(command: SessionCommands, json: bool) -> Result<(), Box<dyn Error>> {
    let client = client::connect_or_start()?;
    validate_session_protocol(client.protocol_version()?)?;
    let snapshot = client.snapshot()?;
    match command {
        SessionCommands::List { workspace } => {
            let selected_workspace = workspace
                .as_deref()
                .map(|target| resolve_workspace_target(&snapshot.workspaces, target))
                .transpose()?;
            let catalog = selected_workspace.map_or_else(
                || discover_host_catalog(&snapshot.workspaces),
                |workspace| discover_host_catalog(std::slice::from_ref(workspace)),
            );
            let sessions =
                session_projection::project_snapshot_with_catalog(&snapshot, Some(&catalog));
            let workspace_id = selected_workspace.map(|workspace| workspace.id.as_str());
            let sessions = sessions
                .iter()
                .filter(|session| {
                    workspace_id.is_none_or(|workspace_id| session.workspace_id == workspace_id)
                })
                .collect::<Vec<_>>();
            if json {
                let sessions = sessions
                    .iter()
                    .map(|session| cli_output::session_summary(session))
                    .collect::<Vec<_>>();
                return cli_output::print(
                    "session.list",
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
                    session.occurrences.len()
                );
            }
        }
        SessionCommands::Inspect { session_id } => {
            let catalog = discover_host_catalog(&snapshot.workspaces);
            let sessions =
                session_projection::project_snapshot_with_catalog(&snapshot, Some(&catalog));
            let session = match session_projection::resolve_exact(&sessions, &session_id) {
                Ok(session) => session,
                Err(session_projection::ResolveError::NotFound) => {
                    return Err(cli_output::failure(
                        "not_found",
                        format!("session not found: {session_id}"),
                    ));
                }
                Err(session_projection::ResolveError::DuplicateId) => {
                    return Err(cli_output::failure(
                        "internal",
                        format!("duplicate projected session ID: {session_id}"),
                    ));
                }
            };
            if json {
                return cli_output::print(
                    "session.inspect",
                    serde_json::json!({ "session": cli_output::session(session) }),
                );
            }
            print_session(session);
        }
        SessionCommands::Read {
            session_id,
            before,
            limit,
            max_bytes,
        } => {
            let catalog = discover_host_catalog(&snapshot.workspaces);
            let sessions =
                session_projection::project_snapshot_with_catalog(&snapshot, Some(&catalog));
            let session = match session_projection::resolve_exact(&sessions, &session_id) {
                Ok(session) => session,
                Err(session_projection::ResolveError::NotFound) => {
                    return Err(cli_output::failure(
                        "not_found",
                        format!("session not found: {session_id}"),
                    ));
                }
                Err(session_projection::ResolveError::DuplicateId) => {
                    return Err(cli_output::failure(
                        "internal",
                        format!("duplicate projected session ID: {session_id}"),
                    ));
                }
            };
            let transcript = session_transcript::read(
                session,
                before.as_deref(),
                limit.into(),
                max_bytes as usize,
            )
            .map_err(|error| cli_output::failure(error.code, error.to_string()))?;
            if json {
                return cli_output::print(
                    "session.read",
                    serde_json::json!({ "transcript": transcript }),
                );
            }
            print_transcript(&transcript);
        }
    }
    Ok(())
}

fn validate_attention_protocol(negotiated: u32) -> Result<(), Box<dyn Error>> {
    (negotiated >= 15).then_some(()).ok_or_else(|| {
        cli_output::failure(
            "unsupported_version",
            format!("Agent attention requires daemon protocol 15; negotiated {negotiated}"),
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
    (negotiated >= 12).then_some(()).ok_or_else(|| {
        cli_output::failure(
            "unsupported_version",
            format!("session projection requires daemon protocol 12; negotiated {negotiated}"),
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

fn print_transcript(transcript: &session_transcript::Transcript) {
    println!("SESSION ID\t{}", transcript.session_id);
    println!("INTEGRATION\t{}", transcript.integration);
    println!("EXTERNAL SESSION ID\t{}", transcript.external_session_id);
    println!(
        "ENTRIES\t{} of {}",
        transcript.returned_entries, transcript.total_entries
    );
    println!("CONTENT BYTES\t{}", transcript.content_bytes);
    println!("HAS MORE\t{}", transcript.has_more);
    println!(
        "NEXT CURSOR\t{}",
        transcript.next_cursor.as_deref().unwrap_or("-")
    );
    println!(
        "TRUNCATED\t{}",
        if transcript.truncated {
            transcript.truncated_by.join(",")
        } else {
            "no".to_owned()
        }
    );
    for entry in &transcript.entries {
        println!();
        match entry.kind {
            "tool" => println!(
                "TOOL\t{}\t{}",
                entry.tool_name.as_deref().unwrap_or("unknown"),
                entry.status.as_deref().unwrap_or("unknown")
            ),
            _ => println!(
                "{}\t{}",
                entry.kind.to_uppercase(),
                entry.role.as_deref().unwrap_or("unknown")
            ),
        }
        if let Some(timestamp_ms) = entry.timestamp_ms {
            println!("TIMESTAMP MS\t{timestamp_ms}");
        }
        if let Some(call_id) = entry.tool_call_id.as_deref() {
            println!("CALL ID\t{call_id}");
        }
        if let Some(text) = entry.text.as_deref() {
            println!("{text}");
        }
        if let Some(input) = entry.input.as_deref() {
            println!("INPUT\t{input}");
        }
        if let Some(output) = entry.output.as_deref() {
            println!("OUTPUT\t{output}");
        }
        if entry.truncated {
            println!("ENTRY TRUNCATED\ttrue");
        }
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
            name: arguments.name,
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
        name: cli_name(arguments.name, "agent")?,
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
        return cli_output::print(
            if ensure {
                "agent.ensure"
            } else {
                "agent.register"
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
    let shell = current_shell(&client)?;
    let workspace = client.get_workspace(&shell.workspace_id)?;
    if json {
        let shells = workspace
            .shells
            .iter()
            .map(|shell| cli_output::shell(shell, Some(&workspace.name)))
            .collect::<Vec<_>>();
        return cli_output::print(
            "shells",
            serde_json::json!({
                "workspace_id": workspace.id,
                "workspace_name": workspace.name,
                "shells": shells,
            }),
        );
    }
    println!("NAME\tSHELL ID\tRUN ID\tSTATUS");
    for shell in workspace.shells {
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
        return cli_output::print(
            "events",
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
    let current_workspace_id = env::var("BOOMUX_SHELL_ID")
        .ok()
        .and_then(|id| find_shell(&snapshot, &id).map(|shell| shell.workspace_id.clone()));
    let shell = resolve_shell_target(&snapshot, current_workspace_id.as_deref(), target)?;
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
            return cli_output::print(
                "read",
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

fn close_shell_with_workspace(
    client: &client::Client,
    target: &str,
    workspace: Option<&str>,
) -> Result<(), Box<dyn Error>> {
    let snapshot = client.snapshot()?;
    let current_shell_id = env::var("BOOMUX_SHELL_ID").ok();
    let shell = resolve_cli_shell(&snapshot, target, workspace)?;
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
    let workspace_id = if let Some(workspace) = workspace {
        resolve_workspace_target(&snapshot.workspaces, workspace)?
            .id
            .as_str()
    } else {
        let current_shell_id = env::var("BOOMUX_SHELL_ID").map_err(|_| {
            cli_output::failure(
                "context_required",
                format!(
                    "shell name {target:?} requires --workspace outside a Boomux-managed shell"
                ),
            )
        })?;
        find_shell(snapshot, &current_shell_id)
            .map(|shell| shell.workspace_id.as_str())
            .ok_or_else(|| {
                cli_output::failure("not_found", "current Boomux shell no longer exists")
            })?
    };
    resolve_shell_target(snapshot, Some(workspace_id), target)
}

fn resolve_cli_launcher<'a>(
    snapshot: &'a Snapshot,
    target: &str,
    workspace: Option<&str>,
) -> Result<&'a WorkspaceLauncherSnapshot, Box<dyn Error>> {
    if let Some(launcher) = find_launcher(snapshot, target) {
        return Ok(launcher);
    }
    let workspace_id = if let Some(workspace) = workspace {
        resolve_workspace_target(&snapshot.workspaces, workspace)?
            .id
            .as_str()
    } else {
        let current_shell_id = env::var("BOOMUX_SHELL_ID").map_err(|_| {
            cli_output::failure(
                "context_required",
                format!(
                    "launcher name {target:?} requires --workspace outside a Boomux-managed shell"
                ),
            )
        })?;
        find_shell(snapshot, &current_shell_id)
            .map(|shell| shell.workspace_id.as_str())
            .ok_or_else(|| {
                cli_output::failure("not_found", "current Boomux shell no longer exists")
            })?
    };
    let workspace = snapshot
        .workspaces
        .iter()
        .find(|workspace| workspace.id == workspace_id)
        .ok_or_else(|| cli_output::failure("not_found", "current workspace no longer exists"))?;
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

fn current_shell(client: &client::Client) -> Result<ShellSnapshot, Box<dyn Error>> {
    let shell_id = env::var("BOOMUX_SHELL_ID").map_err(|_| {
        cli_output::failure(
            "context_required",
            "this command must run inside a Boomux-managed shell",
        )
    })?;
    Ok(client.get_shell(shell_id)?)
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
    open_terminal(
        shell_id,
        &format!("{} - {}", workspace.name, shell.name),
        true,
        terminal,
    )?;
    Ok(format!("Opened {} from {}", shell.name, workspace.name))
}

fn open_workspace(
    workspace: &WorkspaceSnapshot,
    terminal: Option<&str>,
) -> Result<(), Box<dyn Error>> {
    if workspace.shells.is_empty() && workspace.launchers.is_empty() {
        return Err(format!("workspace {} has no shells or launchers", workspace.name).into());
    }
    let mut failures = Vec::new();
    for launcher in &workspace.launchers {
        if let Err(error) = invoke_workspace_launcher(workspace, launcher) {
            failures.push(format!("launcher {}: {error}", launcher.name));
        }
    }
    for shell in &workspace.shells {
        if let Err(error) = open_terminal(
            &shell.id,
            &format!("{} - {}", workspace.name, shell.name),
            true,
            terminal,
        ) {
            failures.push(format!("shell {}: {error}", shell.name));
        }
    }
    if !failures.is_empty() {
        return Err(io::Error::other(format!(
            "workspace {} opened with failures: {}",
            workspace.name,
            failures.join("; ")
        ))
        .into());
    }
    Ok(())
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

fn open_shell(
    shell_id: &str,
    title: Option<&str>,
    takeover: bool,
    terminal: Option<&str>,
) -> Result<(), Box<dyn Error>> {
    let client = client::connect_or_start()?;
    let shell = client.get_shell(shell_id)?;
    let title = title.map(str::to_owned).unwrap_or_else(|| {
        client
            .get_workspace(&shell.workspace_id)
            .map(|workspace| format!("{} - {}", workspace.name, shell.name))
            .unwrap_or_else(|_| format!("Boomux: {}", shell.name))
    });
    open_terminal(shell_id, &title, takeover, terminal)
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

fn migrate_legacy_skill(home: &Path) -> Result<(), Box<dyn Error>> {
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
    )
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
    install_integrations(&[integration_management::IntegrationId::Pi], force, false)
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

fn doctor(terminal_override: Option<&str>) -> Result<(), Box<dyn Error>> {
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
                config.notifications,
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
        }
        Err(error) => {
            healthy = false;
            eprintln!("err config: {error}");
        }
    }
    let integration_environment = integration_management::Environment::from_process();
    for integration in integration_management::IntegrationId::ALL {
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
    let path = status.asset.path.as_deref().unwrap_or("unresolved path");
    if status.asset.state == integration_management::AssetState::Unavailable {
        eprintln!(
            "err {} integration: cannot inspect {} at {path}: {}",
            spec.name,
            spec.asset_name,
            status.asset.error.as_deref().unwrap_or("unknown error")
        );
        return false;
    }
    if status.runtime.running_processes == 0 {
        println!(
            "ok  {} integration: {} {} at {path}",
            spec.name,
            spec.asset_name,
            status.asset.state.as_str(),
        );
        return true;
    }
    if status.asset.state != integration_management::AssetState::Current {
        eprintln!(
            "err {} integration: {} {} at {path}; run boomux integration install {}{}",
            spec.name,
            spec.asset_name,
            status.asset.state.as_str(),
            spec.name,
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
            spec.name
        );
        true
    } else {
        eprintln!(
            "err {} integration: {} foreground process(es) are untracked; restart {} and verify it loads {path}",
            spec.name, status.runtime.untracked_processes, spec.display_name
        );
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

fn notification_diagnostic(
    settings: daemon::NotificationSettings,
    executable: bool,
    desktop_bus: bool,
) -> NotificationDiagnostic {
    if !settings.enabled {
        NotificationDiagnostic::Disabled
    } else if !executable {
        NotificationDiagnostic::MissingExecutable
    } else if !desktop_bus {
        NotificationDiagnostic::MissingDesktopBus
    } else {
        NotificationDiagnostic::Ready
    }
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

    fn shell(id: &str, workspace_id: &str, name: &str) -> ShellSnapshot {
        ShellSnapshot {
            id: id.into(),
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
            foreground_process: None,
        }
    }

    fn workspace(id: &str, name: &str, shells: Vec<ShellSnapshot>) -> WorkspaceSnapshot {
        WorkspaceSnapshot {
            id: id.into(),
            name: name.into(),
            shells,
            launchers: Vec::new(),
            agents: Vec::new(),
        }
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
        }
    }

    fn launcher(id: &str, workspace_id: &str, name: &str) -> WorkspaceLauncherSnapshot {
        WorkspaceLauncherSnapshot {
            id: id.into(),
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
            Some(Commands::Close { target }) if target == "tests"
        ));
    }

    #[test]
    fn parses_global_json_for_supported_integration_commands() {
        for arguments in [
            vec!["boomux", "--json", "capabilities"],
            vec!["boomux", "list", "--json"],
            vec!["boomux", "workspace", "list", "--json"],
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
            assert!(supports_json(&cli));
        }
        let cli = Cli::try_parse_from(["boomux", "workspace", "create", "test", "--json"]).unwrap();
        assert!(!supports_json(&cli));
        let cli = Cli::try_parse_from(["boomux", "opencode", "install", "--json"]).unwrap();
        assert!(!supports_json(&cli));
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
        assert!(matches!(
            Cli::try_parse_from(["boomux", "workspace", "create", "project"])
                .unwrap()
                .command,
            Some(Commands::Workspace {
                command: WorkspaceCommands::Create { name }
            }) if name == "project"
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
                }
            }) if name == "editor"
                && workspace == "project"
                && cwd == Path::new("/tmp")
                && command == ["zeditor", "."]
        ));
        assert!(matches!(
            Cli::try_parse_from(["boomux", "workspace", "open", "project"])
                .unwrap()
                .command,
            Some(Commands::Workspace {
                command: WorkspaceCommands::Open { target }
            }) if target == "project"
        ));
        let cli = Cli::try_parse_from([
            "boomux", "shell", "create", "project", "--name", "tests", "--cwd", "/tmp", "--",
            "cargo", "test",
        ])
        .unwrap();
        assert!(matches!(
            cli.command,
            Some(Commands::Shell {
                command: ShellCommands::Create {
                    workspace,
                    name: Some(name),
                    cwd,
                    command,
                }
            }) if workspace == "project"
                && name == "tests"
                && cwd == Path::new("/tmp")
                && command == ["cargo", "test"]
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
        assert_eq!(command_name(&ensure), "agent.ensure");
        assert!(supports_json(&ensure));

        let cli = Cli::try_parse_from([
            "boomux",
            "agent",
            "list",
            "--workspace",
            "project",
            "--json",
        ])
        .unwrap();
        assert!(supports_json(&cli));
        let cli = Cli::try_parse_from(["boomux", "agent", "get", "a1", "--json"]).unwrap();
        assert_eq!(command_name(&cli), "agent.inspect");
        assert!(supports_json(&cli));
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
        assert_eq!(command_name(&cli), "agent.wait");
        assert!(supports_json(&cli));
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
        assert_eq!(command_name(&cli), "agent.report");
        assert!(supports_json(&cli));
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
        assert_eq!(command_name(&register), "agent.register");
        assert!(supports_json(&register));
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
        assert_eq!(command_name(&supervise), "agent.supervise");
        assert!(!supports_json(&supervise));
        assert!(matches!(
            supervise.command,
            Some(Commands::Agent {
                command: AgentCommands::Supervise(AgentSuperviseArgs { command, .. })
            }) if command == ["agent-bin", "literal; argument"]
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
        assert!(INTEGRATION_FEATURES.contains(&"process_adapters"));
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
        assert_eq!(command_name(&list), "attention.list");
        assert!(supports_json(&list));

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
        assert_eq!(command_name(&acknowledge), "attention.acknowledge");
        assert!(supports_json(&acknowledge));
        assert_eq!(
            sanitize_table_cell("approval\tneeded\nnow\u{7}"),
            "approval needed now "
        );
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
    fn parses_session_discovery_commands_and_json_support() {
        let list = Cli::try_parse_from([
            "boomux",
            "session",
            "list",
            "--workspace",
            "project",
            "--json",
        ])
        .unwrap();
        assert_eq!(command_name(&list), "session.list");
        assert!(supports_json(&list));
        assert!(matches!(
            list.command,
            Some(Commands::Session {
                command: SessionCommands::List {
                    workspace: Some(workspace)
                }
            }) if workspace == "project"
        ));

        let inspect =
            Cli::try_parse_from(["boomux", "session", "get", "opaque", "--json"]).unwrap();
        assert_eq!(command_name(&inspect), "session.inspect");
        assert!(supports_json(&inspect));

        let read = Cli::try_parse_from([
            "boomux",
            "session",
            "read",
            "opaque",
            "--before",
            "v1.cursor",
            "--limit",
            "25",
            "--max-bytes",
            "4096",
            "--json",
        ])
        .unwrap();
        assert_eq!(command_name(&read), "session.read");
        assert!(supports_json(&read));
        assert!(matches!(
            read.command,
            Some(Commands::Session {
                command: SessionCommands::Read {
                    session_id,
                    before: Some(before),
                    limit: 25,
                    max_bytes: 4096,
                }
            }) if session_id == "opaque" && before == "v1.cursor"
        ));
        assert!(
            Cli::try_parse_from(["boomux", "session", "read", "opaque", "--limit", "0"]).is_err()
        );
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
    fn event_snapshot_json_includes_stable_agent_data() {
        let mut project = workspace("w1", "project", vec![shell("s1", "w1", "shell")]);
        project.agents.push(agent("a1", "w1", "s1"));

        let value = json_snapshot(Snapshot {
            workspaces: vec![project],
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
    fn matches_native_workspace_by_name_only() {
        let workspace = workspace("w1", "project", vec![]);
        assert!(find_workspace(&[workspace], "project").is_some());
    }

    #[test]
    fn resolves_shell_ids_and_contextual_names() {
        let snapshot = Snapshot {
            workspaces: vec![
                workspace("w1", "one", vec![shell("s1", "w1", "tests")]),
                workspace("w2", "two", vec![shell("s2", "w2", "tests")]),
            ],
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
    fn generates_unique_native_shell_names() {
        let shells = vec![shell("s1", "w1", "shell-1"), shell("s2", "w1", "api")];
        assert_eq!(unique_shell_name("shell", &shells), "shell-2");
        assert_eq!(unique_shell_name("api", &shells), "api-2");
        assert_eq!(unique_shell_name("logs", &shells), "logs");
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
        assert_eq!(session.state, "blocked");
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

        enrich_session_titles_with(&mut views, |integration, directory, external_id| {
            (integration == "opencode"
                && directory == Path::new("/tmp/project")
                && external_id == "external-1")
                .then(|| "Review async title cache".into())
        });

        assert_eq!(views[0].sessions[0].label, "Review async title cache");
    }

    #[test]
    fn enriches_from_the_newest_available_session_directory() {
        let mut views = vec![tui::WorkspaceView {
            id: "w1".into(),
            name: "project".into(),
            items: Vec::new(),
            agent_state_counts: agent_attention_projection::AgentStateCounts::default(),
            attention_count: 0,
            attention: None,
            sessions: vec![tui::AgentSessionView {
                id: "session".into(),
                label: "opencode".into(),
                integration: "opencode".into(),
                external_session_id: Some("external-1".into()),
                state: "inactive".into(),
                state_is_current: false,
                last_at_ms: 30,
                source_cwd: Some("/tmp/project".into()),
                runs: vec![
                    tui::AgentSessionRunView {
                        shell_name: Some("old-shell".into()),
                        directory: Some("/tmp/project".into()),
                    },
                    tui::AgentSessionRunView {
                        shell_name: None,
                        directory: None,
                    },
                ],
            }],
        }];

        enrich_session_titles_with(&mut views, |_, directory, _| {
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

        assert_eq!(views[0].sessions[0].state, "blocked");
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

        assert_eq!(views[0].sessions[0].state, "working");
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

        assert_eq!(views[0].sessions[0].state, "blocked");
        assert!(!views[0].sessions[0].state_is_current);
    }

    #[test]
    fn workspace_session_id_is_stable_when_equal_timestamp_occurrence_is_added() {
        let mut workspace = workspace("w1", "project", vec![shell("s1", "w1", "agent")]);
        let mut first = agent("agent-z", "w1", "s1");
        first.started_at_ms = 10;
        workspace.agents = vec![first.clone()];
        let initial = session_projection::project_workspaces(std::slice::from_ref(&workspace));
        let initial_id = workspace_session_views(&initial)[0].id.clone();

        let mut added = agent("agent-a", "w1", "s1");
        added.started_at_ms = 10;
        workspace.agents.push(added);
        let projected = session_projection::project_workspaces(std::slice::from_ref(&workspace));
        let sessions = workspace_session_views(&projected);

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
                .any(|session| session.state == "inactive")
        );
        assert!(
            views[0]
                .sessions
                .iter()
                .any(|session| session.state == "done")
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
        assert_eq!(agent.state, "working");
        assert_eq!(agent.authority, "lifecycle_integration");
        assert_eq!(agent.confidence, 95);
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
        assert_eq!(agent.id, "a1");
        assert_eq!(agent.state, "blocked");
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
        tied_last.name = "latest".into();
        tied_last.integration = "other".into();
        workspace.agents = vec![tied_last, older, tied_first];

        let views = dashboard_views(&[workspace], &mut git::Cache::default());

        let tui::WorkspaceItemView::AgentShell(item) = &views[0].items[0] else {
            panic!("expected agent-shell item");
        };
        assert_eq!(item.agent.as_ref().unwrap().id, "agent-z");
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
        let error = open_workspace(&workspace("w1", "empty", Vec::new()), None).unwrap_err();

        assert!(error.to_string().contains("workspace empty has no shells"));
    }

    #[test]
    fn workspace_open_continues_after_a_launcher_spawn_failure() {
        let marker = env::temp_dir().join(format!("boomux-launcher-{}", Uuid::new_v4()));
        let mut workspace = workspace("w1", "launchers", Vec::new());
        workspace.launchers = vec![
            WorkspaceLauncherSnapshot {
                id: "l1".into(),
                workspace_id: "w1".into(),
                name: "missing".into(),
                cwd: env::temp_dir(),
                command: vec!["/boomux-command-does-not-exist".into()],
            },
            WorkspaceLauncherSnapshot {
                id: "l2".into(),
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

        let error = open_workspace(&workspace, None).unwrap_err();
        assert!(error.to_string().contains("launcher missing"));
        for _ in 0..100 {
            if marker.is_file() {
                break;
            }
            thread::sleep(std::time::Duration::from_millis(10));
        }
        assert_eq!(fs::read_to_string(&marker).unwrap(), "launched");
        fs::remove_file(marker).unwrap();
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
        assert_eq!(command_name(&list), "integration.list");

        let status = Cli::try_parse_from(["boomux", "integration", "status", "pi"]).unwrap();
        assert!(matches!(
            status.command,
            Some(Commands::Integration {
                command: IntegrationCommands::Status {
                    integration: Some(integration_management::IntegrationId::Pi)
                }
            })
        ));
        assert_eq!(command_name(&status), "integration.status");

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
                }
            })
        ));
        assert_eq!(command_name(&install), "integration.install");

        assert!(Cli::try_parse_from(["boomux", "integration", "install", "--all"]).is_ok());
        assert!(Cli::try_parse_from(["boomux", "integration", "install"]).is_err());
        assert!(Cli::try_parse_from(["boomux", "integration", "install", "pi", "--all",]).is_err());
    }

    #[test]
    fn formats_integration_output_without_tab_alignment() {
        let integrations = integration_management::IntegrationId::ALL
            .into_iter()
            .map(integration_management::IntegrationSummary::from)
            .collect::<Vec<_>>();
        assert_eq!(
            format_integration_list(&integrations),
            "NAME      PACKAGE                          VALIDATED VERSION\n\
             opencode  opencode-ai                      1.18.15\n\
             pi        @earendil-works/pi-coding-agent  0.84.1\n"
        );

        let status = integration_management::IntegrationStatus {
            name: "opencode",
            display_name: "OpenCode",
            package: "opencode-ai",
            validated_version: "1.18.15",
            host: integration_management::HostStatus {
                state: integration_management::HostState::Available,
                executable: Some("/usr/bin/opencode".into()),
                version: Some("1.18.15".into()),
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
        for command in [
            "agent.register",
            "agent.ensure",
            "agent.report",
            "agent.wait",
            "attention.list",
            "attention.acknowledge",
        ] {
            assert!(JSON_COMMANDS.contains(&command));
        }
        for command in ["session.list", "session.inspect", "session.read"] {
            assert!(JSON_COMMANDS.contains(&command));
        }
        for command in [
            "integration.list",
            "integration.status",
            "integration.install",
        ] {
            assert!(JSON_COMMANDS.contains(&command));
        }
        for feature in [
            "protocol_10",
            "protocol_12",
            "inactive_agent_state",
            "idempotent_agent_ensure",
            "agent_authority_precedence",
            "opencode_lifecycle_plugin",
            "pi_lifecycle_extension",
            "canonical_session_transcripts",
            "transcript_pagination",
            "protocol_15",
            "protocol_16",
            "persistent_agent_attention",
            "desktop_notifications",
            "integration_management",
        ] {
            assert!(INTEGRATION_FEATURES.contains(&feature));
        }
        assert_eq!(
            integration_management::IntegrationId::Opencode
                .spec()
                .validated_version,
            "1.18.15"
        );
        assert_eq!(
            integration_management::IntegrationId::Pi
                .spec()
                .validated_version,
            "0.84.1"
        );
        assert_eq!(
            session_transcript::supported_integrations(),
            ["opencode", "pi"]
        );
        assert_eq!(protocol::PROTOCOL_VERSION, 16);
    }

    #[test]
    fn notification_doctor_diagnostic_is_deterministic() {
        let disabled = daemon::NotificationSettings::default();
        assert_eq!(
            notification_diagnostic(disabled, false, false),
            NotificationDiagnostic::Disabled
        );
        let enabled = daemon::NotificationSettings {
            enabled: true,
            ..disabled
        };
        assert_eq!(
            notification_diagnostic(enabled, false, true),
            NotificationDiagnostic::MissingExecutable
        );
        assert_eq!(
            notification_diagnostic(enabled, true, false),
            NotificationDiagnostic::MissingDesktopBus
        );
        assert_eq!(
            notification_diagnostic(enabled, true, true),
            NotificationDiagnostic::Ready
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
            "boomux list",
            "boomux shells",
            "boomux read",
            "boomux close",
            "boomux open",
            "boomux workspace list",
            "boomux workspace create",
            "boomux workspace inspect",
            "boomux workspace rename",
            "boomux workspace close",
            "boomux shell create",
            "boomux shell inspect",
            "boomux shell rename",
            "boomux shell close",
            "boomux session list",
            "boomux session inspect",
            "boomux session read",
            "boomux skill install",
            "boomux opencode install",
            "boomux pi install",
            "boomux daemon status",
            "boomux daemon restart",
            "boomux daemon stop",
            "boomux prompt",
        ] {
            assert!(BOOMUX_SKILL.contains(command), "skill omits {command}");
        }
        assert!(BOOMUX_SKILL.contains("BOOMUX_SHELL_ID"));
        assert!(BOOMUX_SKILL.contains("--workspace"));
        assert!(BOOMUX_SKILL.contains("--terminal"));
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

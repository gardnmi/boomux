use std::env;
use std::error::Error;
use std::fs;
use std::fs::OpenOptions;
use std::io::{self, Write};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};
use std::thread;

use clap::error::ErrorKind as ClapErrorKind;
use clap::{Parser, Subcommand, ValueEnum};
use uuid::Uuid;

use boomux::protocol::{
    AgentAuthority, AgentInstanceSnapshot, AgentRegistrationSpec, AgentReport, AgentState,
    EventCursor, ShellSnapshot, ShellSpec, ShellStatus, Snapshot, WorkspaceLauncherSnapshot,
    WorkspaceLauncherSpec, WorkspaceSnapshot,
};
use boomux::{attach, client, daemon, protocol};

mod cli_output;
mod config;
mod git;
mod projects;
mod terminal;
mod tui;

const BOOMUX_SKILL: &str = include_str!("../.agents/skills/boomux/SKILL.md");
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
    /// Manage the vendor-neutral Boomux Agent Skill
    Skill {
        #[command(subcommand)]
        command: SkillCommands,
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
    /// Register an agent instance for a shell run
    Register {
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
    },
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

#[derive(Clone, Copy, ValueEnum)]
enum CliAgentState {
    Unknown,
    Working,
    Blocked,
    Idle,
    Done,
}

impl From<CliAgentState> for AgentState {
    fn from(state: CliAgentState) -> Self {
        match state {
            CliAgentState::Unknown => Self::Unknown,
            CliAgentState::Working => Self::Working,
            CliAgentState::Blocked => Self::Blocked,
            CliAgentState::Idle => Self::Idle,
            CliAgentState::Done => Self::Done,
        }
    }
}

#[derive(Clone, Copy, ValueEnum)]
enum CliAgentAuthority {
    LifecycleIntegration,
    ProcessAdapter,
    TerminalHeuristic,
    DaemonLifecycle,
}

impl From<CliAgentAuthority> for AgentAuthority {
    fn from(authority: CliAgentAuthority) -> Self {
        match authority {
            CliAgentAuthority::LifecycleIntegration => Self::LifecycleIntegration,
            CliAgentAuthority::ProcessAdapter => Self::ProcessAdapter,
            CliAgentAuthority::TerminalHeuristic => Self::TerminalHeuristic,
            CliAgentAuthority::DaemonLifecycle => Self::DaemonLifecycle,
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

fn main() -> ExitCode {
    let json_requested = env::args_os().any(|argument| argument == "--json");
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
        Ok(()) => ExitCode::SUCCESS,
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

fn run(cli: Cli) -> Result<(), Box<dyn Error>> {
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
        }) => return Ok(daemon::run()?),
        Some(Commands::Daemon {
            command: DaemonCommands::ReceiveHandoff { channel },
        }) => return Ok(daemon::receive_handoff(*channel)?),
        Some(Commands::Attach { shell_id, takeover }) => {
            return Ok(attach::run(shell_id, *takeover)?);
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
        return open_directory(
            &path,
            cli.name.as_deref(),
            &cli.startup_command,
            new_window,
            terminal.as_deref(),
        );
    }

    match cli.command {
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
        Some(Commands::Agent { command }) => agent_command(command, cli.json),
        Some(Commands::Skill {
            command: SkillCommands::Install { force },
        }) => install_skill(force),
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
    }
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
        Some(Commands::Daemon {
            command: DaemonCommands::Status,
        }) => "daemon.status",
        Some(Commands::Workspace { .. }) => "workspace",
        Some(Commands::Shell { .. }) => "shell",
        Some(Commands::Launcher { .. }) => "launcher",
        Some(Commands::Agent { .. }) => "agent",
        Some(Commands::Daemon { .. }) => "daemon",
        Some(Commands::Ui) | None => "ui",
        Some(Commands::Doctor) => "doctor",
        Some(Commands::Close { .. }) => "close",
        Some(Commands::Skill { .. }) => "skill",
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
            command: AgentCommands::List { .. } | AgentCommands::Inspect { .. }
        }) | Some(Commands::Daemon {
            command: DaemonCommands::Status
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
    let views = dashboard_views(&client.snapshot()?.workspaces, &mut git_cache);
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
                Ok(dashboard_views(&snapshot.workspaces, &mut git_cache))
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

fn dashboard_views(
    workspaces: &[WorkspaceSnapshot],
    git_cache: &mut git::Cache,
) -> Vec<tui::WorkspaceView> {
    workspaces
        .iter()
        .map(|workspace| {
            let shells = workspace.shells.iter().map(|shell| {
                let git = git_cache.inspect(&shell.cwd);
                tui::WorkspaceItemView::Shell(tui::TerminalView {
                    id: shell.id.clone(),
                    name: shell.name.clone(),
                    status: shell_status(&shell.status).into(),
                    directory: shell.cwd.display().to_string(),
                    branch: git.branch,
                })
            });
            let launchers = workspace.launchers.iter().map(|launcher| {
                tui::WorkspaceItemView::Launcher(tui::LauncherView {
                    id: launcher.id.clone(),
                    name: launcher.name.clone(),
                    directory: launcher.cwd.display().to_string(),
                    command: launcher.command.join(" "),
                })
            });
            let agents = workspace.agents.iter().map(|agent| {
                let shell_name = workspace
                    .shells
                    .iter()
                    .find(|shell| shell.id == agent.shell_id)
                    .map_or("-", |shell| shell.name.as_str());
                tui::WorkspaceItemView::Agent(tui::AgentView {
                    id: agent.id.clone(),
                    workspace_id: agent.workspace_id.clone(),
                    run_id: agent.run_id.clone(),
                    shell_id: agent.shell_id.clone(),
                    shell_name: shell_name.into(),
                    name: agent.name.clone(),
                    state: cli_output::agent_state(agent.observation.state).into(),
                    integration: agent.integration.clone(),
                    authority: cli_output::agent_authority(agent.observation.authority).into(),
                    confidence: agent.observation.confidence,
                    evidence: agent.observation.evidence.clone(),
                    external_session_id: agent.external_session_id.clone(),
                    started_at_ms: agent.started_at_ms,
                    observed_at_ms: agent.observation.observed_at_ms,
                    ended_at_ms: agent.ended_at_ms,
                })
            });
            tui::WorkspaceView {
                id: workspace.id.clone(),
                name: workspace.name.clone(),
                items: shells.chain(launchers).chain(agents).collect(),
            }
        })
        .collect()
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
    let (shell, workspace_name) = if let Some(name) = requested_name {
        let shell = if let Some(workspace) = find_workspace(&snapshot.workspaces, &name) {
            let shell_name = unique_shell_name("shell", &workspace.shells);
            client.create_shell(
                &workspace.id,
                shell_spec(shell_name, &directory, startup_command),
            )?
        } else {
            client
                .create_workspace(
                    &name,
                    vec![shell_spec("shell-1", &directory, startup_command)],
                )?
                .shells
                .into_iter()
                .next()
                .ok_or("new workspace has no shell")?
        };
        (shell, name)
    } else {
        let shell = client.create_shell_with_workspace(shell_spec(
            "shell-1",
            &directory,
            startup_command,
        ))?;
        let workspace_name = client.get_workspace(&shell.workspace_id)?.name;
        (shell, workspace_name)
    };

    if open_in_new_window {
        open_terminal(
            &shell.id,
            &format!("{workspace_name} - {}", shell.name),
            true,
            terminal,
        )
    } else {
        Ok(attach::run(&shell.id, true)?)
    }
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
        .find(|workspace| workspace.id == target || workspace.name == target)
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

fn capabilities(json: bool) -> Result<(), Box<dyn Error>> {
    let commands = [
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
        "daemon.status",
    ];
    let features = [
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
    ];
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
        "internal",
        "unknown",
    ];
    if json {
        return cli_output::print(
            "capabilities",
            serde_json::json!({
                "cli_version": env!("CARGO_PKG_VERSION"),
                "daemon_protocol_version": protocol::PROTOCOL_VERSION,
                "json_schemas": [cli_output::SCHEMA],
                "json_commands": commands,
                "features": features,
                "error_codes": error_codes,
            }),
        );
    }
    println!("CLI VERSION\t{}", env!("CARGO_PKG_VERSION"));
    println!("DAEMON PROTOCOL\t{}", protocol::PROTOCOL_VERSION);
    println!("JSON SCHEMAS\t{}", cli_output::SCHEMA);
    println!("JSON COMMANDS\t{}", commands.join(","));
    println!("FEATURES\t{}", features.join(","));
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
                    .map(|workspace| cli_output::WorkspaceSummary {
                        id: workspace.id.clone(),
                        name: workspace.name.clone(),
                        shell_count: workspace.shells.len(),
                        launcher_count: workspace.launchers.len(),
                        agent_count: workspace.agents.len(),
                    })
                    .collect::<Vec<_>>();
                return cli_output::print(
                    "workspace.list",
                    serde_json::json!({ "workspaces": workspaces }),
                );
            }
            println!("NAME\tWORKSPACE ID\tSHELLS\tLAUNCHERS\tAGENTS");
            for workspace in workspaces {
                println!(
                    "{}\t{}\t{}\t{}\t{}",
                    workspace.name,
                    workspace.id,
                    workspace.shells.len(),
                    workspace.launchers.len(),
                    workspace.agents.len()
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
                        }
                    }),
                );
            }
            println!("ID\t{}", workspace.id);
            println!("NAME\t{}", workspace.name);
            println!("SHELLS\t{}", workspace.shells.len());
            println!("LAUNCHERS\t{}", workspace.launchers.len());
            println!("AGENTS\t{}", workspace.agents.len());
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
        AgentCommands::Register {
            name,
            integration,
            external_session_id,
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
            let agent = client.register_agent(
                shell_id,
                run_id,
                AgentRegistrationSpec {
                    name: cli_name(name, "agent")?,
                    integration,
                    external_session_id,
                    report: AgentReport {
                        state: state.into(),
                        authority: authority.into(),
                        evidence,
                        confidence,
                    },
                },
            )?;
            println!("Registered agent {} ({})", agent.name, agent.id);
        }
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
    let directory = ensure_skill_directory(home, "boomux")?;
    let path = skill_install_path(home);
    let already_installed = if let Some(existing) = read_regular_file(&path)? {
        if existing == BOOMUX_SKILL {
            true
        } else if !force {
            return Err(format!(
                "{} already exists; rerun with --force to replace it",
                path.display()
            )
            .into());
        } else {
            false
        }
    } else {
        false
    };

    if already_installed {
        println!("Boomux skill is already installed at {}", path.display());
    } else {
        write_skill_atomically(&directory, &path, BOOMUX_SKILL)?;
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
    if let Some(existing) = read_regular_file(&path)? {
        let entries = fs::read_dir(&directory)?.collect::<Result<Vec<_>, _>>()?;
        let untouched = existing == LEGACY_BOOMUX_SHELLS_SKILL
            && entries.len() == 1
            && entries[0].file_name() == "SKILL.md";
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

fn ensure_skill_directory(home: &Path, skill: &str) -> Result<PathBuf, Box<dyn Error>> {
    let mut directory = home.to_owned();
    for component in [".agents", "skills", skill] {
        directory.push(component);
        match fs::symlink_metadata(&directory) {
            Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {}
            Ok(_) => {
                return Err(format!(
                    "skill path component is not a regular directory: {}",
                    directory.display()
                )
                .into());
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                fs::create_dir(&directory)?;
            }
            Err(error) => return Err(error.into()),
        }
    }
    Ok(directory)
}

fn read_regular_file(path: &Path) -> Result<Option<String>, Box<dyn Error>> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_file() && !metadata.file_type().is_symlink() => {
            Ok(Some(fs::read_to_string(path)?))
        }
        Ok(_) => Err(format!("skill path is not a regular file: {}", path.display()).into()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

fn write_skill_atomically(
    directory: &Path,
    path: &Path,
    content: &str,
) -> Result<(), Box<dyn Error>> {
    let temporary = directory.join(format!(".SKILL-{}.tmp", Uuid::new_v4()));
    let result = (|| {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)?;
        file.write_all(content.as_bytes())?;
        file.sync_all()?;
        fs::rename(&temporary, path)?;
        Ok::<(), std::io::Error>(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result.map_err(Into::into)
}

fn skill_install_path(home: &Path) -> PathBuf {
    home.join(".agents/skills/boomux/SKILL.md")
}

fn legacy_skill_install_path(home: &Path) -> PathBuf {
    home.join(".agents/skills/boomux-shells/SKILL.md")
}

fn doctor(terminal_override: Option<&str>) -> Result<(), Box<dyn Error>> {
    let mut healthy = true;
    match client::connect_or_start() {
        Ok(client) => println!(
            "ok  daemon: protocol {} ({})",
            protocol::PROTOCOL_VERSION,
            client.socket_path().display()
        ),
        Err(error) => {
            healthy = false;
            eprintln!("err daemon: {error}");
        }
    }
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
        }
        Err(error) => {
            healthy = false;
            eprintln!("err config: {error}");
        }
    }
    if healthy {
        Ok(())
    } else {
        Err("one or more dependency or configuration checks failed".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shell(id: &str, workspace_id: &str, name: &str) -> ShellSnapshot {
        ShellSnapshot {
            id: id.into(),
            workspace_id: workspace_id.into(),
            name: name.into(),
            cwd: PathBuf::from("/tmp/project"),
            status: ShellStatus::Running,
            run: None,
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
            name: "opencode".into(),
            integration: "plugin".into(),
            external_session_id: Some("external-1".into()),
            started_at_ms: 10,
            ended_at_ms: None,
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
        let cli = Cli::try_parse_from(["boomux", "__attach", "s1", "--takeover"]).unwrap();
        assert!(matches!(
            cli.command,
            Some(Commands::Attach { takeover: true, .. })
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
        ] {
            let cli = Cli::try_parse_from(arguments).unwrap();
            assert!(cli.json);
            assert!(supports_json(&cli));
        }
        let cli = Cli::try_parse_from(["boomux", "workspace", "create", "test", "--json"]).unwrap();
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
                command: AgentCommands::Register { confidence: 95, .. }
            })
        ));

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
        assert!(!supports_json(&cli));
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
            workspaces: vec![project],
        };

        assert_eq!(
            resolve_workspace_target(&snapshot.workspaces, "project")
                .unwrap()
                .id,
            "w1"
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
        assert_eq!(launcher.directory, "/tmp/project");
    }

    #[test]
    fn workspace_view_maps_agent_runtime_details() {
        let mut workspace = workspace("w1", "project", vec![shell("s1", "w1", "terminal")]);
        workspace.agents.push(agent("a1", "w1", "s1"));

        let views = dashboard_views(&[workspace], &mut git::Cache::default());

        let tui::WorkspaceItemView::Agent(agent) = &views[0].items[1] else {
            panic!("expected agent item");
        };
        assert_eq!(agent.name, "opencode");
        assert_eq!(agent.shell_name, "terminal");
        assert_eq!(agent.state, "working");
        assert_eq!(agent.authority, "lifecycle_integration");
        assert_eq!(agent.confidence, 95);
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
            "boomux skill install",
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

use std::env;
use std::error::Error;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

use clap::{Parser, Subcommand};

use boomux::protocol::{ShellSnapshot, ShellSpec, ShellStatus, Snapshot, WorkspaceSnapshot};
use boomux::{attach, client, daemon, protocol};

mod config;
mod git;
mod projects;
mod terminal;
mod tui;

const BOOMUX_SHELLS_SKILL: &str = include_str!("../.agents/skills/boomux-shells/SKILL.md");
const REPLAY_BYTES: usize = 1024 * 1024;

#[derive(Parser)]
#[command(
    version,
    about = "Native persistent terminal workspaces",
    subcommand_value_name = "SUBCOMMAND"
)]
struct Cli {
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
    /// List all managed shells
    List,
    /// List shells in the current Boomux workspace
    Shells,
    /// Read retained output from a shell name or shell ID
    Read {
        target: String,
        #[arg(long, default_value_t = 200, value_parser = clap::value_parser!(u32).range(1..))]
        lines: u32,
    },
    /// Close a shell by name or shell ID
    Close { target: String },
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
enum SkillCommands {
    /// Install the Boomux shell-reading skill under ~/.agents/skills
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
    /// Report whether the daemon is accepting requests
    Status,
    /// Stop the daemon and its managed shells
    Stop,
}

fn main() -> ExitCode {
    match run(Cli::parse()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("boomux: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run(cli: Cli) -> Result<(), Box<dyn Error>> {
    match cli.command.as_ref() {
        Some(Commands::Daemon {
            command: DaemonCommands::Run,
        }) => return Ok(daemon::run()?),
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
        Some(Commands::List) => list_shells(),
        Some(Commands::Shells) => list_workspace_shells(),
        Some(Commands::Read { target, lines }) => read_shell(&target, lines),
        Some(Commands::Close { target }) => close_shell(&target),
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
        Some(Commands::Daemon { command }) => daemon_control(command),
        Some(Commands::Attach { .. }) => unreachable!(),
        None => dashboard(cli.terminal.as_deref()),
    }
}

fn daemon_control(command: DaemonCommands) -> Result<(), Box<dyn Error>> {
    let client = client::connect()?;
    match command {
        DaemonCommands::Status => println!(
            "running (protocol {}, {})",
            protocol::PROTOCOL_VERSION,
            client.socket_path().display()
        ),
        DaemonCommands::Stop => {
            client.shutdown()?;
            println!("Stopped Boomux daemon");
        }
        DaemonCommands::Run => unreachable!(),
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
                let count = workspace.shells.len();
                open_workspace(&workspace, terminal.as_deref())
                    .map_err(|error| error.to_string())?;
                Ok(format!("Restored {count} shell(s) for {}", workspace.name))
            },
            on_open: |shell_id: &str| {
                open_dashboard_shell(&client, shell_id, terminal.as_deref())
                    .map_err(|error| error.to_string())
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
                    Ok(format!("Closed {name} and all of its shells"))
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
            },
            on_refresh: || {
                let snapshot = client.snapshot().map_err(|error| error.to_string())?;
                Ok(dashboard_views(&snapshot.workspaces, &mut git_cache))
            },
        },
    )?;
    Ok(())
}

fn dashboard_views(
    workspaces: &[WorkspaceSnapshot],
    git_cache: &mut git::Cache,
) -> Vec<tui::WorkspaceView> {
    workspaces
        .iter()
        .map(|workspace| {
            let directory = common_shell_cwd(workspace);
            let git = directory
                .map(|directory| git_cache.inspect(directory))
                .unwrap_or_default();
            let terminals = workspace
                .shells
                .iter()
                .map(|shell| tui::TerminalView {
                    id: shell.id.clone(),
                    name: shell.name.clone(),
                    status: shell_status(&shell.status).into(),
                    directory: shell.cwd.display().to_string(),
                })
                .collect();
            tui::WorkspaceView {
                id: workspace.id.clone(),
                name: workspace.name.clone(),
                directory: directory
                    .map(|directory| directory.display().to_string())
                    .unwrap_or_else(|| "-".into()),
                repository: git.repository,
                branch: git.branch,
                git_state: git.state,
                worktree: git.worktree,
                terminals,
            }
        })
        .collect()
}

fn common_shell_cwd(workspace: &WorkspaceSnapshot) -> Option<&Path> {
    let first = workspace.shells.first()?.cwd.as_path();
    workspace
        .shells
        .iter()
        .all(|shell| shell.cwd == first)
        .then_some(first)
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

fn find_workspace<'a>(
    workspaces: &'a [WorkspaceSnapshot],
    name: &str,
) -> Option<&'a WorkspaceSnapshot> {
    workspaces.iter().find(|workspace| workspace.name == name)
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

fn list_shells() -> Result<(), Box<dyn Error>> {
    let snapshot = client::connect_or_start()?.snapshot()?;
    println!("WORKSPACE\tNAME\tSHELL ID\tSTATUS");
    for workspace in snapshot.workspaces {
        for shell in workspace.shells {
            println!(
                "{}\t{}\t{}\t{}",
                workspace.name,
                shell.name,
                shell.id,
                shell_status(&shell.status)
            );
        }
    }
    Ok(())
}

fn list_workspace_shells() -> Result<(), Box<dyn Error>> {
    let client = client::connect_or_start()?;
    let shell = current_shell(&client)?;
    let workspace = client.get_workspace(&shell.workspace_id)?;
    println!("NAME\tSHELL ID\tSTATUS");
    for shell in workspace.shells {
        println!(
            "{}\t{}\t{}",
            shell.name,
            shell.id,
            shell_status(&shell.status)
        );
    }
    Ok(())
}

fn read_shell(target: &str, lines: u32) -> Result<(), Box<dyn Error>> {
    let client = client::connect_or_start()?;
    let snapshot = client.snapshot()?;
    let current_workspace_id = env::var("BOOMUX_SHELL_ID")
        .ok()
        .and_then(|id| find_shell(&snapshot, &id).map(|shell| shell.workspace_id.clone()));
    let shell = resolve_shell_target(&snapshot, current_workspace_id.as_deref(), target)?;
    let bytes = client.read_shell(&shell.id, REPLAY_BYTES)?;
    let output = recent_lines(&String::from_utf8_lossy(&bytes), lines as usize);
    print!("{output}");
    if !output.is_empty() && !output.ends_with('\n') {
        println!();
    }
    Ok(())
}

fn close_shell(target: &str) -> Result<(), Box<dyn Error>> {
    let client = client::connect_or_start()?;
    let snapshot = client.snapshot()?;
    let current_shell_id = env::var("BOOMUX_SHELL_ID").ok();
    let current_workspace_id = current_shell_id
        .as_deref()
        .and_then(|id| find_shell(&snapshot, id).map(|shell| shell.workspace_id.as_str()));
    let shell = resolve_shell_target(&snapshot, current_workspace_id, target)?;
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

fn resolve_shell_target<'a>(
    snapshot: &'a Snapshot,
    current_workspace_id: Option<&str>,
    target: &str,
) -> Result<&'a ShellSnapshot, Box<dyn Error>> {
    if let Some(shell) = find_shell(snapshot, target) {
        return Ok(shell);
    }
    let workspace_id = current_workspace_id.ok_or_else(|| {
        format!(
            "shell name {target:?} requires a Boomux shell; use an exact shell ID outside Boomux"
        )
    })?;
    let workspace = snapshot
        .workspaces
        .iter()
        .find(|workspace| workspace.id == workspace_id)
        .ok_or("current workspace no longer exists")?;
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
            Err(format!(
                "shell {target:?} was not found in this workspace; available shells: {available}"
            )
            .into())
        }
        _ => Err(format!("shell name {target:?} is ambiguous in this workspace").into()),
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

fn current_shell(client: &client::Client) -> Result<ShellSnapshot, Box<dyn Error>> {
    let shell_id = env::var("BOOMUX_SHELL_ID")
        .map_err(|_| "this command must run inside a Boomux-managed shell")?;
    Ok(client.get_shell(shell_id)?)
}

fn shell_status(status: &ShellStatus) -> &'static str {
    match status {
        ShellStatus::Pending => "pending",
        ShellStatus::Running => "running",
        ShellStatus::Exited { .. } => "exited",
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
    if workspace.shells.is_empty() {
        return Err(format!("workspace {} has no shells", workspace.name).into());
    }
    for shell in &workspace.shells {
        open_terminal(
            &shell.id,
            &format!("{} - {}", workspace.name, shell.name),
            true,
            terminal,
        )?;
    }
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
    let path = skill_install_path(&home);
    if path.is_file() {
        let existing = fs::read_to_string(&path)?;
        if existing == BOOMUX_SHELLS_SKILL {
            println!(
                "Boomux shell skill is already installed at {}",
                path.display()
            );
            return Ok(());
        }
        if !force {
            return Err(format!(
                "{} already exists; rerun with --force to replace it",
                path.display()
            )
            .into());
        }
    }
    let directory = path.parent().ok_or("invalid skill installation path")?;
    fs::create_dir_all(directory)?;
    fs::write(&path, BOOMUX_SHELLS_SKILL)?;
    println!("Installed Boomux shell skill at {}", path.display());
    Ok(())
}

fn skill_install_path(home: &Path) -> PathBuf {
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
        }
    }

    fn workspace(id: &str, name: &str, shells: Vec<ShellSnapshot>) -> WorkspaceSnapshot {
        WorkspaceSnapshot {
            id: id.into(),
            name: name.into(),
            shells,
        }
    }

    #[test]
    fn parses_paths_and_native_hidden_commands() {
        let cli = Cli::try_parse_from(["boomux", "."]).unwrap();
        assert_eq!(cli.path, Some(PathBuf::from(".")));
        assert!(Cli::try_parse_from(["boomux", "daemon", "run"]).is_ok());
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
    fn empty_workspace_view_has_no_directory_or_git_values() {
        let views = dashboard_views(
            &[workspace("w1", "empty", Vec::new())],
            &mut git::Cache::default(),
        );

        assert_eq!(views[0].directory, "-");
        assert_eq!(views[0].repository, "-");
        assert_eq!(views[0].branch, "-");
        assert_eq!(views[0].git_state, "-");
        assert_eq!(views[0].worktree, "-");
    }

    #[test]
    fn restoring_empty_workspace_returns_actionable_error() {
        let error = open_workspace(&workspace("w1", "empty", Vec::new()), None).unwrap_err();

        assert!(error.to_string().contains("workspace empty has no shells"));
    }

    #[test]
    fn selects_recent_lossy_replay_lines() {
        assert_eq!(recent_lines("one\ntwo\nthree\n", 2), "two\nthree\n");
        assert_eq!(recent_lines("one\ntwo", 1), "two");
    }

    #[test]
    fn installs_skill_under_vendor_neutral_directory() {
        assert_eq!(
            skill_install_path(Path::new("/home/example")),
            PathBuf::from("/home/example/.agents/skills/boomux-shells/SKILL.md")
        );
    }
}

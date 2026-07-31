use std::env;
use std::error::Error;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};

use clap::{Parser, Subcommand};
use serde::Deserialize;

mod tui;

#[derive(Parser)]
#[command(
    version,
    about = "Native Ghostty windows for persistent Herdr terminals",
    args_conflicts_with_subcommands = true
)]
struct Cli {
    /// Open or create a persistent terminal in this directory
    #[arg(value_name = "PATH")]
    path: Option<PathBuf>,

    /// Name of the workspace group
    #[arg(short, long, requires = "path")]
    name: Option<String>,

    /// Open the terminal in a new Ghostty window instead of attaching here
    #[arg(long = "new", requires = "path")]
    new_window: bool,

    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand)]
enum Commands {
    /// Open the interactive workspace dashboard
    Ui,
    /// Check that Boomux's external dependencies are available
    Doctor,
    /// List panes managed by Herdr
    List,
    /// Open a Herdr terminal in a new Ghostty window
    Open {
        /// Herdr terminal ID, available from `boomux list`
        terminal_id: String,
        /// Stable title shown on the Ghostty window
        #[arg(long)]
        title: Option<String>,
        /// Replace another client currently controlling this terminal
        #[arg(long)]
        takeover: bool,
    },
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
    if let Some(path) = cli.path {
        return open_directory(&path, cli.name.as_deref(), cli.new_window);
    }

    match cli.command {
        Some(Commands::Ui) => dashboard(),
        Some(Commands::Doctor) => doctor(),
        Some(Commands::List) => run_foreground("herdr", &["pane", "list"]),
        Some(Commands::Open {
            terminal_id,
            title,
            takeover,
        }) => open_terminal(&terminal_id, title.as_deref(), takeover),
        None => picker(),
    }
}

#[derive(Deserialize)]
struct PaneListResponse {
    result: PaneListResult,
}

#[derive(Deserialize)]
struct PaneListResult {
    panes: Vec<Pane>,
}

#[derive(Deserialize)]
struct WorkspaceListResponse {
    result: WorkspaceListResult,
}

#[derive(Deserialize)]
struct WorkspaceListResult {
    workspaces: Vec<Workspace>,
}

#[derive(Deserialize)]
struct WorkspaceCreateResponse {
    result: WorkspaceCreateResult,
}

#[derive(Deserialize)]
struct WorkspaceCreateResult {
    root_pane: Pane,
}

#[derive(Deserialize)]
struct TabCreateResponse {
    result: TabCreateResult,
}

#[derive(Deserialize)]
struct TabCreateResult {
    root_pane: Pane,
}

#[derive(Deserialize)]
struct Workspace {
    workspace_id: String,
    label: String,
    agent_status: String,
}

#[derive(Deserialize)]
struct Pane {
    pane_id: String,
    terminal_id: String,
    workspace_id: String,
    cwd: String,
    #[serde(default)]
    agent: Option<String>,
    agent_status: String,
}

struct Choice {
    label: String,
    workspace_id: String,
}

fn picker() -> Result<(), Box<dyn Error>> {
    ensure_host_terminal()?;
    let panes = available_panes()?;
    let workspaces = load_workspaces()?;
    let Some(workspace_id) = choose_workspace(&workspaces, &panes)? else {
        return Ok(());
    };
    let workspace = workspaces
        .iter()
        .find(|workspace| workspace.workspace_id == workspace_id)
        .ok_or("selected workspace no longer exists")?;

    open_workspace(workspace, &panes)
}

fn dashboard() -> Result<(), Box<dyn Error>> {
    ensure_host_terminal()?;
    let views = dashboard_snapshot()?;
    tui::run(
        views,
        |workspace_id| {
            let panes = available_panes().map_err(|error| error.to_string())?;
            let workspaces = load_workspaces().map_err(|error| error.to_string())?;
            let workspace = workspaces
                .iter()
                .find(|workspace| workspace.workspace_id == workspace_id)
                .ok_or_else(|| "selected workspace no longer exists".to_owned())?;
            let shell_count = workspace_panes(&workspace.workspace_id, &panes).count();
            open_workspace(workspace, &panes).map_err(|error| error.to_string())?;
            Ok(format!(
                "Restored {shell_count} shell(s) for {}",
                workspace.label
            ))
        },
        |workspace_id| {
            let workspaces = load_workspaces().map_err(|error| error.to_string())?;
            let Some(workspace) = workspaces
                .iter()
                .find(|workspace| workspace.workspace_id == workspace_id)
            else {
                return Ok("Workspace was already closed".to_owned());
            };
            let label = workspace.label.clone();
            close_workspace(workspace_id).map_err(|error| error.to_string())?;
            Ok(format!("Closed {label} and all of its shells"))
        },
        || dashboard_snapshot().map_err(|error| error.to_string()),
    )?;
    Ok(())
}

fn dashboard_snapshot() -> Result<Vec<tui::WorkspaceView>, Box<dyn Error>> {
    let panes = load_panes()?;
    let workspaces = load_workspaces()?;
    Ok(dashboard_views(&workspaces, &panes))
}

fn dashboard_views(workspaces: &[Workspace], panes: &[Pane]) -> Vec<tui::WorkspaceView> {
    workspaces
        .iter()
        .filter_map(|workspace| {
            let workspace_panes: Vec<_> = workspace_panes(&workspace.workspace_id, panes).collect();
            let directory = workspace_panes.first()?.cwd.clone();
            let terminals = workspace_panes
                .into_iter()
                .map(|pane| tui::TerminalView {
                    id: pane.terminal_id.clone(),
                    kind: pane.agent.clone().unwrap_or_else(|| "shell".into()),
                    status: pane.agent_status.clone(),
                    directory: pane.cwd.clone(),
                })
                .collect();
            Some(tui::WorkspaceView {
                id: workspace.workspace_id.clone(),
                name: workspace.label.clone(),
                status: workspace.agent_status.clone(),
                directory,
                terminals,
            })
        })
        .collect()
}

fn open_directory(
    path: &Path,
    requested_name: Option<&str>,
    open_in_new_window: bool,
) -> Result<(), Box<dyn Error>> {
    ensure_host_terminal()?;
    let directory = resolve_directory(path)?;
    let panes = available_panes()?;
    let workspaces = load_workspaces()?;
    let name = requested_name
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(str::to_owned)
        .unwrap_or_else(|| default_workspace_name(&directory));
    let (terminal_id, shell_number) =
        if let Some(workspace) = find_workspace(&workspaces, &panes, &directory, &name) {
            let shell_number = workspace_panes(&workspace.workspace_id, &panes).count() + 1;
            (
                create_tab_terminal(&workspace.workspace_id, &directory)?.terminal_id,
                shell_number,
            )
        } else {
            (
                create_workspace(&directory, &name)?.root_pane.terminal_id,
                1,
            )
        };

    if open_in_new_window {
        open_terminal(
            &terminal_id,
            Some(&format!("{name} [{shell_number}]")),
            true,
        )?;
    } else if !attach_terminal(&terminal_id)? {
        return Err(format!("could not attach to Herdr terminal {terminal_id}").into());
    }
    Ok(())
}

fn default_workspace_name(directory: &Path) -> String {
    directory
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .unwrap_or("default")
        .to_owned()
}

fn ensure_host_terminal() -> Result<(), Box<dyn Error>> {
    if env::var_os("HERDR_PANE_ID").is_some() {
        Err("already inside a Herdr terminal; launch Boomux from a fresh terminal".into())
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

fn available_panes() -> Result<Vec<Pane>, Box<dyn Error>> {
    Ok(load_panes()?
        .into_iter()
        .filter(|pane| !pane_runs_boomux(&pane.pane_id))
        .collect())
}

fn find_workspace<'a>(
    workspaces: &'a [Workspace],
    panes: &[Pane],
    directory: &Path,
    name: &str,
) -> Option<&'a Workspace> {
    workspaces.iter().find(|workspace| {
        workspace.label == name
            && workspace_panes(&workspace.workspace_id, panes)
                .any(|pane| pane_is_in_directory(pane, directory))
    })
}

fn workspace_panes<'a>(workspace_id: &'a str, panes: &'a [Pane]) -> impl Iterator<Item = &'a Pane> {
    panes
        .iter()
        .filter(move |pane| pane.workspace_id == workspace_id)
}

fn pane_is_in_directory(pane: &Pane, directory: &Path) -> bool {
    Path::new(&pane.cwd)
        .canonicalize()
        .is_ok_and(|cwd| cwd == directory)
}

fn choose_workspace(
    workspaces: &[Workspace],
    panes: &[Pane],
) -> Result<Option<String>, Box<dyn Error>> {
    let choices: Vec<_> = workspaces
        .iter()
        .filter_map(|workspace| {
            let workspace_panes: Vec<_> = workspace_panes(&workspace.workspace_id, panes).collect();
            let directory = workspace_panes.first()?.cwd.as_str();
            let shell_word = if workspace_panes.len() == 1 {
                "shell"
            } else {
                "shells"
            };
            Some(Choice {
                label: format!(
                    "{:<18} {:<8} {:>2} {:<6} {}  ({})",
                    workspace.label,
                    display_agent_status(&workspace.agent_status),
                    workspace_panes.len(),
                    shell_word,
                    directory,
                    workspace.workspace_id
                ),
                workspace_id: workspace.workspace_id.clone(),
            })
        })
        .collect();
    if choices.is_empty() {
        return Err("no saved workspaces; run `boomux PATH` to create one".into());
    }

    let output = Command::new("gum")
        .args(["choose", "--header", "Restore a Boomux workspace"])
        .args(choices.iter().map(|choice| choice.label.as_str()))
        .stdin(Stdio::inherit())
        .stderr(Stdio::inherit())
        .output()?;
    if !output.status.success() {
        return Ok(None);
    }

    let selected = String::from_utf8(output.stdout)?;
    Ok(choices
        .into_iter()
        .find(|choice| choice.label == selected.trim_end())
        .map(|choice| choice.workspace_id))
}

fn create_workspace(cwd: &Path, name: &str) -> Result<WorkspaceCreateResult, Box<dyn Error>> {
    println!("Creating workspace {name} in {}", cwd.display());

    let output = Command::new("herdr")
        .args(["workspace", "create", "--cwd"])
        .arg(cwd)
        .args(["--label", name])
        .arg("--focus")
        .output()?;
    if !output.status.success() {
        let message = String::from_utf8_lossy(&output.stderr);
        return Err(format!("could not create Herdr terminal: {}", message.trim()).into());
    }

    let response: WorkspaceCreateResponse = serde_json::from_slice(&output.stdout)?;
    Ok(response.result)
}

fn create_tab_terminal(workspace_id: &str, cwd: &Path) -> Result<Pane, Box<dyn Error>> {
    let output = Command::new("herdr")
        .args(["tab", "create", "--workspace", workspace_id, "--cwd"])
        .arg(cwd)
        .arg("--focus")
        .output()?;
    if !output.status.success() {
        let message = String::from_utf8_lossy(&output.stderr);
        return Err(format!("could not create Herdr terminal: {}", message.trim()).into());
    }

    let response: TabCreateResponse = serde_json::from_slice(&output.stdout)?;
    Ok(response.result.root_pane)
}

fn open_workspace(workspace: &Workspace, panes: &[Pane]) -> Result<(), Box<dyn Error>> {
    let workspace_panes: Vec<_> = workspace_panes(&workspace.workspace_id, panes).collect();
    if workspace_panes.is_empty() {
        return Err(format!("workspace {} has no terminals", workspace.label).into());
    }

    for (index, pane) in workspace_panes.iter().enumerate() {
        open_terminal(
            &pane.terminal_id,
            Some(&format!("{} [{}]", workspace.label, index + 1)),
            true,
        )?;
    }

    Ok(())
}

fn close_workspace(workspace_id: &str) -> Result<(), Box<dyn Error>> {
    let output = Command::new("herdr")
        .args(["workspace", "close", workspace_id])
        .output()?;
    if output.status.success() {
        Ok(())
    } else {
        let message = String::from_utf8_lossy(&output.stderr);
        Err(format!("could not close Herdr workspace: {}", message.trim()).into())
    }
}

fn pane_runs_boomux(pane_id: &str) -> bool {
    let Ok(output) = Command::new("herdr")
        .args(["pane", "process-info", "--pane", pane_id])
        .output()
    else {
        return false;
    };

    output.status.success()
        && String::from_utf8_lossy(&output.stdout).contains(r#""name":"boomux""#)
}

fn attach_terminal(terminal_id: &str) -> Result<bool, Box<dyn Error>> {
    let status = Command::new("herdr")
        .args(["terminal", "attach", terminal_id, "--takeover"])
        .stderr(Stdio::null())
        .status()?;

    Ok(status.success())
}

fn display_agent_status(status: &str) -> &str {
    if status == "unknown" { "-" } else { status }
}

fn load_panes() -> Result<Vec<Pane>, Box<dyn Error>> {
    let output = Command::new("herdr").args(["pane", "list"]).output()?;
    if !output.status.success() {
        let message = String::from_utf8_lossy(&output.stderr);
        return Err(format!("could not list Herdr terminals: {}", message.trim()).into());
    }

    let response: PaneListResponse = serde_json::from_slice(&output.stdout)?;
    Ok(response.result.panes)
}

fn load_workspaces() -> Result<Vec<Workspace>, Box<dyn Error>> {
    let output = Command::new("herdr").args(["workspace", "list"]).output()?;
    if !output.status.success() {
        let message = String::from_utf8_lossy(&output.stderr);
        return Err(format!("could not list Herdr workspaces: {}", message.trim()).into());
    }

    let response: WorkspaceListResponse = serde_json::from_slice(&output.stdout)?;
    Ok(response.result.workspaces)
}

fn doctor() -> Result<(), Box<dyn Error>> {
    let mut healthy = true;

    for command in ["ghostty", "herdr", "gum"] {
        match Command::new(command).arg("--version").output() {
            Ok(output) if output.status.success() => {
                let version = String::from_utf8_lossy(&output.stdout);
                println!("ok  {command}: {}", version.trim());
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

    if healthy {
        Ok(())
    } else {
        Err("one or more dependencies are unavailable".into())
    }
}

fn open_terminal(
    terminal_id: &str,
    title: Option<&str>,
    takeover: bool,
) -> Result<(), Box<dyn Error>> {
    let title = title
        .map(str::to_owned)
        .unwrap_or_else(|| format!("Boomux: {terminal_id}"));
    let mut command = Command::new("ghostty");
    command
        .arg("+new-window")
        .arg(format!("--title={title}"))
        .args(["-e", "herdr", "terminal", "attach", terminal_id]);

    if takeover {
        command.arg("--takeover");
    }

    let status = command.status()?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("Ghostty failed with {status}").into())
    }
}

fn run_foreground(program: &str, args: &[&str]) -> Result<(), Box<dyn Error>> {
    let status = Command::new(program).args(args).status()?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("{program} failed with {status}").into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_path_like_familiar_project_launchers() {
        let cli = Cli::try_parse_from(["boomux", "."]).unwrap();

        assert_eq!(cli.path, Some(PathBuf::from(".")));
        assert!(cli.name.is_none());
        assert!(!cli.new_window);
        assert!(cli.command.is_none());
    }

    #[test]
    fn accepts_workspace_name_only_with_a_path() {
        let cli = Cli::try_parse_from(["boomux", ".", "--name", "feature-x"]).unwrap();

        assert_eq!(cli.name.as_deref(), Some("feature-x"));
        assert!(Cli::try_parse_from(["boomux", "--name", "feature-x"]).is_err());
    }

    #[test]
    fn accepts_new_window_only_with_a_path() {
        let cli = Cli::try_parse_from(["boomux", ".", "--new"]).unwrap();

        assert!(cli.new_window);
        assert!(Cli::try_parse_from(["boomux", "--new"]).is_err());
        assert!(Cli::try_parse_from(["boomux", ".", "--current"]).is_err());
    }

    #[test]
    fn still_prioritizes_named_subcommands() {
        let cli = Cli::try_parse_from(["boomux", "doctor"]).unwrap();

        assert!(cli.path.is_none());
        assert!(matches!(cli.command, Some(Commands::Doctor)));
    }

    #[test]
    fn resolves_directories_and_rejects_files() {
        let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));

        assert_eq!(resolve_directory(manifest_dir).unwrap(), manifest_dir);
        assert!(resolve_directory(&manifest_dir.join("Cargo.toml")).is_err());
    }

    #[test]
    fn matches_workspace_by_name_and_directory() {
        let directory = Path::new(env!("CARGO_MANIFEST_DIR"));
        let workspaces = vec![Workspace {
            workspace_id: "w1".into(),
            label: "default".into(),
            agent_status: "unknown".into(),
        }];
        let panes = vec![Pane {
            pane_id: "w1:p1".into(),
            terminal_id: "term_123".into(),
            workspace_id: "w1".into(),
            cwd: directory.display().to_string(),
            agent: None,
            agent_status: "unknown".into(),
        }];

        assert!(find_workspace(&workspaces, &panes, directory, "default").is_some());
        assert!(find_workspace(&workspaces, &panes, directory, "other").is_none());
    }

    #[test]
    fn parses_herdr_pane_list() {
        let response: PaneListResponse = serde_json::from_str(
            r#"{
                "result": {
                    "panes": [{
                        "pane_id": "w1:p1",
                        "terminal_id": "term_123",
                        "workspace_id": "w1",
                        "cwd": "/tmp/project",
                        "agent_status": "working"
                    }]
                }
            }"#,
        )
        .unwrap();

        assert_eq!(response.result.panes.len(), 1);
        assert_eq!(response.result.panes[0].pane_id, "w1:p1");
        assert_eq!(response.result.panes[0].terminal_id, "term_123");
    }

    #[test]
    fn hides_unreported_agent_status() {
        assert_eq!(display_agent_status("unknown"), "-");
        assert_eq!(display_agent_status("working"), "working");
    }

    #[test]
    fn parses_created_workspace_root_terminal() {
        let response: WorkspaceCreateResponse = serde_json::from_str(
            r#"{
                "result": {
                    "root_pane": {
                        "pane_id": "w1:p1",
                        "terminal_id": "term_456",
                        "workspace_id": "w1",
                        "cwd": "/tmp/project",
                        "agent_status": "unknown"
                    }
                }
            }"#,
        )
        .unwrap();

        assert_eq!(response.result.root_pane.terminal_id, "term_456");
    }
}

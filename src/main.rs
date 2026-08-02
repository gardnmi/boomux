use std::env;
use std::error::Error;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};

use clap::{Parser, Subcommand};
use serde::Deserialize;

mod config;
mod git;
mod projects;
mod terminal;
mod tui;

#[derive(Parser)]
#[command(
    version,
    about = "Native terminal windows for persistent Herdr terminals"
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
    /// Open a Herdr terminal in a new terminal window
    Open {
        /// Herdr terminal ID, available from `boomux list`
        terminal_id: String,
        /// Stable title shown on the terminal window when supported
        #[arg(long)]
        title: Option<String>,
        /// Replace another client currently controlling this terminal
        #[arg(long)]
        takeover: bool,
    },
    /// Print the current Boomux workspace and shell name for prompt integrations
    #[command(hide = true)]
    Prompt,
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
    if let Some(desktop_entry) = cli.terminal.as_deref() {
        terminal::validate_desktop_entry(desktop_entry)?;
    }
    if let Some(path) = cli.path {
        let open_in_new_window = should_open_new_window(cli.new_window, cli.terminal.as_deref());
        let terminal = open_in_new_window
            .then(|| effective_terminal(cli.terminal.as_deref()))
            .transpose()?
            .flatten();
        return open_directory(
            &path,
            cli.name.as_deref(),
            open_in_new_window,
            terminal.as_deref(),
        );
    }

    match cli.command {
        Some(Commands::Ui) => dashboard(cli.terminal.as_deref()),
        Some(Commands::Doctor) => doctor(cli.terminal.as_deref()),
        Some(Commands::List) => run_foreground("herdr", &["pane", "list"]),
        Some(Commands::Open {
            terminal_id,
            title,
            takeover,
        }) => {
            let terminal = effective_terminal(cli.terminal.as_deref())?;
            open_terminal(
                &terminal_id,
                title.as_deref(),
                takeover,
                terminal.as_deref(),
            )
        }
        Some(Commands::Prompt) => print_prompt_label(),
        None => {
            let terminal = effective_terminal(cli.terminal.as_deref())?;
            picker(terminal.as_deref())
        }
    }
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
    tab_id: String,
    terminal_id: String,
    workspace_id: String,
    cwd: String,
    #[serde(default)]
    label: Option<String>,
    #[serde(default)]
    agent: Option<String>,
    agent_status: String,
}

struct Choice {
    label: String,
    workspace_id: String,
}

fn picker(terminal: Option<&str>) -> Result<(), Box<dyn Error>> {
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

    open_workspace(workspace, &panes, terminal)
}

fn dashboard(terminal_override: Option<&str>) -> Result<(), Box<dyn Error>> {
    ensure_host_terminal()?;
    let mut git_cache = git::Cache::default();
    let views = dashboard_snapshot(&mut git_cache)?;
    let config = config::load()?;
    let terminal = terminal_override
        .map(str::to_owned)
        .or_else(|| config.terminal.clone());
    let recipes = config.recipes.clone();
    let roots_configured = !config.projects.roots.is_empty();
    let discovery = projects::discover(&config.projects);
    let project_views = discovery
        .projects
        .into_iter()
        .map(|project| tui::ProjectView {
            name: project.name,
            path: project.path,
            group: project.group,
            group_order: project.group_order,
        })
        .collect();
    let project_context = tui::ProjectContext {
        projects: project_views,
        recipes: recipes
            .iter()
            .map(|recipe| tui::RecipeView {
                id: recipe.id.clone(),
                label: recipe.label.clone(),
                terminals: recipe
                    .terminals
                    .iter()
                    .map(|terminal| terminal.name.clone())
                    .collect(),
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
                let panes = load_panes().map_err(|error| error.to_string())?;
                let workspaces = load_workspaces().map_err(|error| error.to_string())?;
                let workspace = workspaces
                    .iter()
                    .find(|workspace| workspace.workspace_id == workspace_id)
                    .ok_or_else(|| "selected workspace no longer exists".to_owned())?;
                let shell_count = workspace_panes(&workspace.workspace_id, &panes).count();
                open_workspace(workspace, &panes, terminal.as_deref())
                    .map_err(|error| error.to_string())?;
                Ok(format!(
                    "Restored {shell_count} shell(s) for {}",
                    workspace.label
                ))
            },
            on_open: |terminal_id: &str| {
                open_dashboard_terminal(terminal_id, terminal.as_deref())
                    .map_err(|error| error.to_string())
            },
            on_close: |workspace_id: &str| {
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
            on_create_workspace: |directory: &Path, recipe_id: Option<&str>| {
                create_dashboard_workspace(directory, recipe_id, &recipes)
                    .map_err(|error| error.to_string())
            },
            on_create_shell: |workspace_id: &str| {
                create_dashboard_shell(workspace_id).map_err(|error| error.to_string())
            },
            on_rename: |pane_id: &str, name: &str| {
                rename_pane(pane_id, name).map_err(|error| error.to_string())?;
                Ok(format!("Renamed shell to {name}"))
            },
            on_refresh: || dashboard_snapshot(&mut git_cache).map_err(|error| error.to_string()),
        },
    )?;
    Ok(())
}

fn dashboard_snapshot(
    git_cache: &mut git::Cache,
) -> Result<Vec<tui::WorkspaceView>, Box<dyn Error>> {
    let panes = load_panes()?;
    let workspaces = load_workspaces()?;
    Ok(dashboard_views(&workspaces, &panes, git_cache))
}

fn dashboard_views(
    workspaces: &[Workspace],
    panes: &[Pane],
    git_cache: &mut git::Cache,
) -> Vec<tui::WorkspaceView> {
    workspaces
        .iter()
        .filter_map(|workspace| {
            let workspace_panes: Vec<_> = workspace_panes(&workspace.workspace_id, panes).collect();
            let directory = workspace_panes.first()?.cwd.clone();
            let git = git_cache.inspect(Path::new(&directory));
            let terminals = workspace_panes
                .into_iter()
                .map(|pane| tui::TerminalView {
                    id: pane.terminal_id.clone(),
                    pane_id: pane.pane_id.clone(),
                    name: pane_name(pane).to_owned(),
                    kind: pane.agent.clone().unwrap_or_else(|| "shell".into()),
                    status: pane.agent_status.clone(),
                    directory: pane.cwd.clone(),
                })
                .collect();
            Some(tui::WorkspaceView {
                id: workspace.workspace_id.clone(),
                name: workspace.label.clone(),
                directory,
                repository: git.repository,
                branch: git.branch,
                git_state: git.state,
                worktree: git.worktree,
                terminals,
            })
        })
        .collect()
}

fn open_directory(
    path: &Path,
    requested_name: Option<&str>,
    open_in_new_window: bool,
    terminal: Option<&str>,
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
    let (pane, shell_name, created_workspace) = if let Some(workspace) =
        find_workspace(&workspaces, &panes, &directory, &name)
    {
        let workspace_panes: Vec<_> = workspace_panes(&workspace.workspace_id, &panes).collect();
        let shell_name = unique_shell_name("shell", &workspace_panes);
        (
            create_tab_terminal(
                &workspace.workspace_id,
                &directory,
                &workspace.label,
                &shell_name,
            )?,
            shell_name,
            false,
        )
    } else {
        (
            create_workspace(&directory, &name, "shell-1")?.root_pane,
            "shell-1".into(),
            true,
        )
    };
    if let Err(error) = rename_pane(&pane.pane_id, &shell_name) {
        let cleanup = if created_workspace {
            close_workspace(&pane.workspace_id)
        } else {
            close_tab(&pane.tab_id)
        };
        return Err(with_cleanup_error(error, cleanup));
    }

    if open_in_new_window {
        open_terminal(
            &pane.terminal_id,
            Some(&format!("{name} - {shell_name}")),
            true,
            terminal,
        )?;
    } else if !attach_terminal(&pane.terminal_id)? {
        return Err(format!("could not attach to Herdr terminal {}", pane.terminal_id).into());
    }
    Ok(())
}

fn default_workspace_name(directory: &Path) -> String {
    directory
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "default".into())
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

fn create_workspace(
    cwd: &Path,
    name: &str,
    shell_name: &str,
) -> Result<WorkspaceCreateResult, Box<dyn Error>> {
    let output = workspace_create_command(cwd, name, shell_name).output()?;
    if !output.status.success() {
        let message = String::from_utf8_lossy(&output.stderr);
        return Err(format!("could not create Herdr terminal: {}", message.trim()).into());
    }

    let response: WorkspaceCreateResponse = serde_json::from_slice(&output.stdout)?;
    Ok(response.result)
}

fn workspace_create_command(cwd: &Path, name: &str, shell_name: &str) -> Command {
    let mut command = Command::new("herdr");
    command
        .args(["workspace", "create", "--cwd"])
        .arg(cwd)
        .args(["--label", name])
        .args(["--env", &format!("BOOMUX_WORKSPACE={name}")])
        .args(["--env", &format!("BOOMUX_SHELL_NAME={shell_name}")])
        .arg("--focus");
    command
}

fn create_tab_terminal(
    workspace_id: &str,
    cwd: &Path,
    workspace_name: &str,
    shell_name: &str,
) -> Result<Pane, Box<dyn Error>> {
    let output = tab_create_command(workspace_id, cwd, workspace_name, shell_name).output()?;
    if !output.status.success() {
        let message = String::from_utf8_lossy(&output.stderr);
        return Err(format!("could not create Herdr terminal: {}", message.trim()).into());
    }

    let response: TabCreateResponse = serde_json::from_slice(&output.stdout)?;
    Ok(response.result.root_pane)
}

fn tab_create_command(
    workspace_id: &str,
    cwd: &Path,
    workspace_name: &str,
    shell_name: &str,
) -> Command {
    let mut command = Command::new("herdr");
    command
        .args(["tab", "create", "--workspace", workspace_id, "--cwd"])
        .arg(cwd)
        .args(["--label", shell_name])
        .args(["--env", &format!("BOOMUX_WORKSPACE={workspace_name}")])
        .args(["--env", &format!("BOOMUX_SHELL_NAME={shell_name}")])
        .arg("--focus");
    command
}

fn create_dashboard_workspace(
    directory: &Path,
    recipe_id: Option<&str>,
    recipes: &[config::RecipeConfig],
) -> Result<String, Box<dyn Error>> {
    let cwd = resolve_directory(directory)?;
    let name = default_workspace_name(&cwd);
    let panes = load_panes()?;
    let workspaces = load_workspaces()?;
    if find_workspace(&workspaces, &panes, &cwd, &name).is_some() {
        return Err(format!("workspace {name} already exists in {}", cwd.display()).into());
    }

    let (recipe_label, terminals) = if let Some(recipe_id) = recipe_id {
        let recipe = recipes
            .iter()
            .find(|recipe| recipe.id == recipe_id)
            .ok_or_else(|| format!("recipe {recipe_id} no longer exists"))?;
        (recipe.label.clone(), recipe.terminals.clone())
    } else {
        (
            "Default".into(),
            vec![config::RecipeTerminalConfig {
                name: "shell-1".into(),
                command: None,
            }],
        )
    };

    provision_recipe(
        &terminals,
        |terminal| Ok(create_workspace(&cwd, &name, &terminal.name)?.root_pane),
        |workspace_id, terminal| create_tab_terminal(workspace_id, &cwd, &name, &terminal.name),
        |pane, terminal| {
            configure_recipe_terminal(pane, &terminal.name, terminal.command.as_deref())
        },
        close_workspace,
    )?;
    Ok(format!(
        "Created workspace {name} with {recipe_label} ({} terminal{})",
        terminals.len(),
        if terminals.len() == 1 { "" } else { "s" }
    ))
}

fn provision_recipe<R, T, C, X>(
    terminals: &[config::RecipeTerminalConfig],
    mut create_root: R,
    mut create_tab: T,
    mut configure: C,
    mut cleanup: X,
) -> Result<(), Box<dyn Error>>
where
    R: FnMut(&config::RecipeTerminalConfig) -> Result<Pane, Box<dyn Error>>,
    T: FnMut(&str, &config::RecipeTerminalConfig) -> Result<Pane, Box<dyn Error>>,
    C: FnMut(&Pane, &config::RecipeTerminalConfig) -> Result<(), Box<dyn Error>>,
    X: FnMut(&str) -> Result<(), Box<dyn Error>>,
{
    let first = terminals.first().ok_or("recipe has no terminals")?;
    let root = create_root(first)?;
    let result = (|| {
        configure(&root, first)?;
        for terminal in terminals.iter().skip(1) {
            let pane = create_tab(&root.workspace_id, terminal)?;
            configure(&pane, terminal)?;
        }
        Ok(())
    })();
    if let Err(error) = result {
        return Err(with_cleanup_error(error, cleanup(&root.workspace_id)));
    }
    Ok(())
}

fn configure_recipe_terminal(
    pane: &Pane,
    name: &str,
    command: Option<&str>,
) -> Result<(), Box<dyn Error>> {
    rename_tab(&pane.tab_id, name)?;
    rename_pane(&pane.pane_id, name)?;
    let Some(command) = command else {
        return Ok(());
    };
    let output = Command::new("herdr")
        .args(["pane", "run", &pane.pane_id, command])
        .output()?;
    if output.status.success() {
        Ok(())
    } else {
        let message = String::from_utf8_lossy(&output.stderr);
        Err(format!(
            "could not deliver startup command to {name}: {}",
            message.trim()
        )
        .into())
    }
}

fn rename_tab(tab_id: &str, name: &str) -> Result<(), Box<dyn Error>> {
    let output = Command::new("herdr")
        .args(["tab", "rename", tab_id, name])
        .output()?;
    if output.status.success() {
        Ok(())
    } else {
        let message = String::from_utf8_lossy(&output.stderr);
        Err(format!("could not rename Herdr tab: {}", message.trim()).into())
    }
}

fn create_dashboard_shell(workspace_id: &str) -> Result<String, Box<dyn Error>> {
    let panes = load_panes()?;
    let workspaces = load_workspaces()?;
    let workspace = workspaces
        .iter()
        .find(|workspace| workspace.workspace_id == workspace_id)
        .ok_or("selected workspace no longer exists")?;
    let workspace_panes: Vec<_> = workspace_panes(workspace_id, &panes).collect();
    let cwd = workspace_panes
        .first()
        .ok_or("selected workspace has no terminals")?
        .cwd
        .as_str();
    let shell_name = unique_shell_name("shell", &workspace_panes);
    let pane = create_tab_terminal(workspace_id, Path::new(cwd), &workspace.label, &shell_name)?;
    if let Err(error) = rename_pane(&pane.pane_id, &shell_name) {
        return Err(with_cleanup_error(error, close_tab(&pane.tab_id)));
    }

    Ok(format!("Created {shell_name} in {}", workspace.label))
}

fn unique_shell_name(base_name: &str, panes: &[&Pane]) -> String {
    let highest_suffix = panes
        .iter()
        .filter_map(|pane| pane.label.as_deref())
        .filter_map(|label| {
            if label == base_name {
                Some(1)
            } else {
                label
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

fn rename_pane(pane_id: &str, name: &str) -> Result<(), Box<dyn Error>> {
    let output = Command::new("herdr")
        .args(["pane", "rename", pane_id, name])
        .output()?;
    if output.status.success() {
        Ok(())
    } else {
        let message = String::from_utf8_lossy(&output.stderr);
        Err(format!("could not rename Herdr pane: {}", message.trim()).into())
    }
}

fn pane_name(pane: &Pane) -> &str {
    pane.label
        .as_deref()
        .or(pane.agent.as_deref())
        .unwrap_or("shell")
}

fn with_cleanup_error(
    error: Box<dyn Error>,
    cleanup: Result<(), Box<dyn Error>>,
) -> Box<dyn Error> {
    match cleanup {
        Ok(()) => error,
        Err(cleanup_error) => format!("{error}; cleanup also failed: {cleanup_error}").into(),
    }
}

fn open_dashboard_terminal(
    terminal_id: &str,
    terminal: Option<&str>,
) -> Result<String, Box<dyn Error>> {
    let panes = load_panes()?;
    let pane = panes
        .iter()
        .find(|pane| pane.terminal_id == terminal_id)
        .ok_or("selected terminal no longer exists")?;
    let workspace = load_workspaces()?
        .into_iter()
        .find(|workspace| workspace.workspace_id == pane.workspace_id)
        .ok_or("selected workspace no longer exists")?;
    let shell_name = pane_name(pane);
    open_terminal(
        terminal_id,
        Some(&format!("{} - {shell_name}", workspace.label)),
        true,
        terminal,
    )?;
    Ok(format!("Opened {shell_name} from {}", workspace.label))
}

fn open_workspace(
    workspace: &Workspace,
    panes: &[Pane],
    terminal: Option<&str>,
) -> Result<(), Box<dyn Error>> {
    let workspace_panes: Vec<_> = workspace_panes(&workspace.workspace_id, panes).collect();
    if workspace_panes.is_empty() {
        return Err(format!("workspace {} has no terminals", workspace.label).into());
    }

    for (index, pane) in workspace_panes.iter().enumerate() {
        let shell_name = pane
            .label
            .as_deref()
            .map(str::to_owned)
            .unwrap_or_else(|| format!("shell-{}", index + 1));
        open_terminal(
            &pane.terminal_id,
            Some(&format!("{} - {shell_name}", workspace.label)),
            true,
            terminal,
        )?;
    }

    Ok(())
}

fn print_prompt_label() -> Result<(), Box<dyn Error>> {
    let Some(pane_id) = env::var_os("HERDR_PANE_ID").and_then(|value| value.into_string().ok())
    else {
        return Ok(());
    };
    let panes = load_panes()?;
    let Some(pane) = panes.iter().find(|pane| pane.pane_id == pane_id) else {
        return Ok(());
    };
    let workspace_name = load_workspaces()?
        .into_iter()
        .find(|workspace| workspace.workspace_id == pane.workspace_id)
        .map(|workspace| workspace.label);
    let shell_name = pane_name(pane);
    if let Some(workspace_name) = workspace_name {
        println!("{workspace_name}/{shell_name}");
    } else {
        println!("{shell_name}");
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

fn close_tab(tab_id: &str) -> Result<(), Box<dyn Error>> {
    let output = Command::new("herdr")
        .args(["tab", "close", tab_id])
        .output()?;
    if output.status.success() {
        Ok(())
    } else {
        let message = String::from_utf8_lossy(&output.stderr);
        Err(format!("could not close Herdr tab: {}", message.trim()).into())
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

fn doctor(terminal_override: Option<&str>) -> Result<(), Box<dyn Error>> {
    let mut healthy = true;

    for command in ["herdr", "gum", "git"] {
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

fn open_terminal(
    terminal_id: &str,
    title: Option<&str>,
    takeover: bool,
    terminal: Option<&str>,
) -> Result<(), Box<dyn Error>> {
    let title = title
        .map(str::to_owned)
        .unwrap_or_else(|| format!("Boomux: {terminal_id}"));
    terminal::open(terminal, terminal_id, &title, takeover)
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
    use std::cell::RefCell;

    use super::*;

    fn test_pane(pane_id: &str, tab_id: &str) -> Pane {
        Pane {
            pane_id: pane_id.into(),
            tab_id: tab_id.into(),
            terminal_id: format!("term-{pane_id}"),
            workspace_id: "w1".into(),
            cwd: "/tmp/project".into(),
            label: None,
            agent: None,
            agent_status: "unknown".into(),
        }
    }

    fn command_args(command: &Command) -> Vec<String> {
        command
            .get_args()
            .map(|argument| argument.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn parses_path_like_familiar_project_launchers() {
        let cli = Cli::try_parse_from(["boomux", "."]).unwrap();

        assert_eq!(cli.path, Some(PathBuf::from(".")));
        assert!(cli.name.is_none());
        assert!(!cli.new_window);
        assert!(cli.command.is_none());
    }

    #[cfg(unix)]
    #[test]
    fn derives_a_visible_name_from_non_utf8_directories() {
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStringExt;

        let directory = PathBuf::from(OsString::from_vec(b"/tmp/project-\x80".to_vec()));
        let name = default_workspace_name(&directory);

        assert!(name.starts_with("project-"));
        assert_ne!(name, "default");
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
    fn accepts_a_terminal_override_for_window_launches() {
        let cli = Cli::try_parse_from(["boomux", ".", "--terminal", "Alacritty.desktop"]).unwrap();

        assert_eq!(cli.terminal.as_deref(), Some("Alacritty.desktop"));
        assert!(should_open_new_window(
            cli.new_window,
            cli.terminal.as_deref()
        ));

        let cli = Cli::try_parse_from([
            "boomux",
            "open",
            "term_123",
            "--terminal",
            "Alacritty.desktop",
        ])
        .unwrap();
        assert_eq!(cli.terminal.as_deref(), Some("Alacritty.desktop"));

        let cli =
            Cli::try_parse_from(["boomux", "--terminal", "Alacritty.desktop", "doctor"]).unwrap();
        assert!(matches!(cli.command, Some(Commands::Doctor)));
        assert_eq!(cli.terminal.as_deref(), Some("Alacritty.desktop"));
    }

    #[test]
    fn still_prioritizes_named_subcommands() {
        let cli = Cli::try_parse_from(["boomux", "doctor"]).unwrap();

        assert!(cli.path.is_none());
        assert!(matches!(cli.command, Some(Commands::Doctor)));

        let cli = Cli::try_parse_from(["boomux", "prompt"]).unwrap();
        assert!(matches!(cli.command, Some(Commands::Prompt)));
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
            tab_id: "w1:t1".into(),
            terminal_id: "term_123".into(),
            workspace_id: "w1".into(),
            cwd: directory.display().to_string(),
            label: Some("shell-1".into()),
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
                        "tab_id": "w1:t1",
                        "terminal_id": "term_123",
                        "workspace_id": "w1",
                        "cwd": "/tmp/project",
                        "label": "api",
                        "agent_status": "working"
                    }]
                }
            }"#,
        )
        .unwrap();

        assert_eq!(response.result.panes.len(), 1);
        assert_eq!(response.result.panes[0].pane_id, "w1:p1");
        assert_eq!(response.result.panes[0].terminal_id, "term_123");
        assert_eq!(response.result.panes[0].label.as_deref(), Some("api"));
    }

    #[test]
    fn generates_unique_shell_names() {
        let panes = [
            Pane {
                pane_id: "w1:p1".into(),
                tab_id: "w1:t1".into(),
                terminal_id: "term_1".into(),
                workspace_id: "w1".into(),
                cwd: "/tmp".into(),
                label: Some("shell-1".into()),
                agent: None,
                agent_status: "unknown".into(),
            },
            Pane {
                pane_id: "w1:p2".into(),
                tab_id: "w1:t2".into(),
                terminal_id: "term_2".into(),
                workspace_id: "w1".into(),
                cwd: "/tmp".into(),
                label: Some("api".into()),
                agent: None,
                agent_status: "idle".into(),
            },
        ];

        assert_eq!(
            unique_shell_name("shell", &panes.iter().collect::<Vec<_>>()),
            "shell-2"
        );
        assert_eq!(
            unique_shell_name("api", &panes.iter().collect::<Vec<_>>()),
            "api-2"
        );
        assert_eq!(
            unique_shell_name("logs", &panes.iter().collect::<Vec<_>>()),
            "logs"
        );
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
                        "tab_id": "w1:t1",
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

    #[test]
    fn recipe_provisioning_creates_and_configures_terminals_in_order() {
        let terminals = vec![
            config::RecipeTerminalConfig {
                name: "opencode".into(),
                command: Some("opencode".into()),
            },
            config::RecipeTerminalConfig {
                name: "lazygit".into(),
                command: Some("lazygit".into()),
            },
        ];
        let events = RefCell::new(Vec::new());

        provision_recipe(
            &terminals,
            |terminal| {
                events.borrow_mut().push(format!("root:{}", terminal.name));
                Ok(test_pane("p1", "t1"))
            },
            |workspace_id, terminal| {
                events
                    .borrow_mut()
                    .push(format!("tab:{workspace_id}:{}", terminal.name));
                Ok(test_pane("p2", "t2"))
            },
            |pane, terminal| {
                events
                    .borrow_mut()
                    .push(format!("configure:{}:{}", pane.pane_id, terminal.name));
                Ok(())
            },
            |workspace_id| {
                events.borrow_mut().push(format!("cleanup:{workspace_id}"));
                Ok(())
            },
        )
        .expect("provisioned recipe");

        assert_eq!(
            events.into_inner(),
            [
                "root:opencode",
                "configure:p1:opencode",
                "tab:w1:lazygit",
                "configure:p2:lazygit",
            ]
        );
    }

    #[test]
    fn recipe_provisioning_closes_partial_workspace_after_failure() {
        let terminals = vec![
            config::RecipeTerminalConfig {
                name: "shell".into(),
                command: None,
            },
            config::RecipeTerminalConfig {
                name: "agent".into(),
                command: Some("missing-agent".into()),
            },
        ];
        let cleaned = RefCell::new(Vec::new());

        let error = provision_recipe(
            &terminals,
            |_| Ok(test_pane("p1", "t1")),
            |_, _| Ok(test_pane("p2", "t2")),
            |_, terminal| {
                if terminal.name == "agent" {
                    Err("command delivery failed".into())
                } else {
                    Ok(())
                }
            },
            |workspace_id| {
                cleaned.borrow_mut().push(workspace_id.to_owned());
                Ok(())
            },
        )
        .expect_err("provisioning should fail");

        assert!(error.to_string().contains("command delivery failed"));
        assert_eq!(cleaned.into_inner(), ["w1"]);
    }

    #[test]
    fn recipe_creation_commands_include_labels_and_environment() {
        assert_eq!(
            command_args(&workspace_create_command(
                Path::new("/tmp/project"),
                "project",
                "opencode",
            )),
            [
                "workspace",
                "create",
                "--cwd",
                "/tmp/project",
                "--label",
                "project",
                "--env",
                "BOOMUX_WORKSPACE=project",
                "--env",
                "BOOMUX_SHELL_NAME=opencode",
                "--focus",
            ]
        );
        assert_eq!(
            command_args(&tab_create_command(
                "w1",
                Path::new("/tmp/project"),
                "project",
                "lazygit",
            )),
            [
                "tab",
                "create",
                "--workspace",
                "w1",
                "--cwd",
                "/tmp/project",
                "--label",
                "lazygit",
                "--env",
                "BOOMUX_WORKSPACE=project",
                "--env",
                "BOOMUX_SHELL_NAME=lazygit",
                "--focus",
            ]
        );
    }

    #[test]
    fn recipe_provisioning_reports_cleanup_failures() {
        let terminals = vec![config::RecipeTerminalConfig {
            name: "agent".into(),
            command: Some("opencode".into()),
        }];

        let error = provision_recipe(
            &terminals,
            |_| Ok(test_pane("p1", "t1")),
            |_, _| unreachable!("no additional terminal"),
            |_, _| Err("command delivery failed".into()),
            |_| Err("workspace close failed".into()),
        )
        .expect_err("provisioning should fail");

        assert_eq!(
            error.to_string(),
            "command delivery failed; cleanup also failed: workspace close failed"
        );
    }
}

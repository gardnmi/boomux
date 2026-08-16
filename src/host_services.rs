use std::collections::{BTreeSet, HashSet};
use std::env;
use std::fs;
use std::io;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;

use serde::Deserialize;

use crate::generated_names;
use crate::integration_management::{self, IntegrationId};
use crate::protocol::{
    AgentInstanceSnapshot, HostAgentSessionInspection, HostAgentSessionResumePlan,
    HostAgentSessionSummary, HostIntegrationMutationResult, HostIntegrationPlan,
    HostIntegrationStatus, HostProjectDiscovery, HostProjectSnapshot, HostServiceIntegrationAction,
    MAX_HOST_SERVICE_PROJECTS, MAX_HOST_SERVICE_SESSIONS, MAX_HOST_SERVICE_WARNINGS, Snapshot,
    WorkspaceLauncherSnapshot, WorkspaceSnapshot,
};
use crate::session_projection::{self, SessionProjection};

const MAX_PROJECT_DIRECTORIES: usize = 10_000;
const MAX_PROJECT_ENTRIES: usize = 50_000;
const MAX_CATALOG_DIRECTORIES: usize = 8;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PreparedIntegrationMutation {
    pub action: HostServiceIntegrationAction,
    pub integrations: Vec<String>,
    pub force: bool,
    pub plans: Vec<HostIntegrationPlan>,
}

pub(crate) fn resolve_directory(path: &Path) -> io::Result<PathBuf> {
    let path = if path.is_absolute() {
        path.to_owned()
    } else {
        env::current_dir()?.join(path)
    };
    let path = path.canonicalize()?;
    path.is_dir().then_some(path).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "path is not an existing directory",
        )
    })
}

#[derive(Default, Deserialize)]
struct ProjectConfigFile {
    projects: Option<ProjectConfig>,
}

#[derive(Clone, Default, Deserialize)]
struct ProjectConfig {
    roots: Option<Vec<String>>,
    max_depth: Option<usize>,
}

pub(crate) fn discover_projects() -> io::Result<HostProjectDiscovery> {
    let mut config = ProjectConfig::default();
    for path in project_config_paths() {
        if !path.is_file() {
            continue;
        }
        let parsed: ProjectConfigFile = toml::from_str(&fs::read_to_string(&path)?)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error.to_string()))?;
        if let Some(next) = parsed.projects {
            if next.roots.is_some() {
                config.roots = next.roots;
            }
            if next.max_depth.is_some() {
                config.max_depth = next.max_depth;
            }
        }
    }
    let roots = config.roots.unwrap_or_default();
    let roots_configured = !roots.is_empty();
    let max_depth = config.max_depth.unwrap_or(3);
    if !(1..=10).contains(&max_depth) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "projects.max_depth must be between 1 and 10",
        ));
    }
    let mut projects = Vec::new();
    let mut warnings = Vec::new();
    let mut seen = HashSet::new();
    let mut directories = 0;
    let mut entries = 0;
    for (group_order, configured) in roots.iter().enumerate() {
        let root = expand_root(configured)?;
        if !root.is_dir() {
            push_warning(
                &mut warnings,
                format!("project root is not a directory: {}", root.display()),
            );
            continue;
        }
        let group = root.file_name().map_or_else(
            || root.display().to_string(),
            |name| name.to_string_lossy().into_owned(),
        );
        scan_projects(
            &root,
            0,
            max_depth,
            &group,
            group_order,
            &mut projects,
            &mut seen,
            &mut directories,
            &mut entries,
        );
        if projects.len() >= MAX_HOST_SERVICE_PROJECTS
            || directories >= MAX_PROJECT_DIRECTORIES
            || entries >= MAX_PROJECT_ENTRIES
        {
            break;
        }
    }
    if projects.len() >= MAX_HOST_SERVICE_PROJECTS {
        push_warning(
            &mut warnings,
            format!("project scan stopped after {MAX_HOST_SERVICE_PROJECTS} projects"),
        );
    }
    if directories >= MAX_PROJECT_DIRECTORIES {
        push_warning(
            &mut warnings,
            format!("project scan stopped after {MAX_PROJECT_DIRECTORIES} directories"),
        );
    }
    if entries >= MAX_PROJECT_ENTRIES {
        push_warning(
            &mut warnings,
            format!("project scan stopped after {MAX_PROJECT_ENTRIES} filesystem entries"),
        );
    }
    projects.sort_by(|left, right| {
        left.group_order
            .cmp(&right.group_order)
            .then_with(|| left.name.to_lowercase().cmp(&right.name.to_lowercase()))
            .then_with(|| left.path.cmp(&right.path))
    });
    Ok(HostProjectDiscovery {
        roots_configured,
        projects,
        warnings,
    })
}

fn project_config_paths() -> Vec<PathBuf> {
    let mut paths = Vec::new();
    if let Some(path) = env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .or_else(|| {
            env::var_os("HOME")
                .map(PathBuf::from)
                .filter(|path| path.is_absolute())
                .map(|path| path.join(".config"))
        })
    {
        paths.push(path.join("boomux/config.toml"));
    }
    if let Some(path) = env::var_os("BOOMUX_CONFIG").map(PathBuf::from) {
        paths.push(path);
    }
    paths
}

fn expand_root(root: &str) -> io::Result<PathBuf> {
    let path = if root == "~" {
        owner_home()?
    } else if let Some(relative) = root.strip_prefix("~/") {
        owner_home()?.join(relative)
    } else {
        PathBuf::from(root)
    };
    path.is_absolute().then_some(path).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "project root must be absolute or start with ~",
        )
    })
}

fn owner_home() -> io::Result<PathBuf> {
    env::var_os("HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "HOME must be an absolute path"))
}

#[allow(clippy::too_many_arguments)]
fn scan_projects(
    directory: &Path,
    depth: usize,
    max_depth: usize,
    group: &str,
    group_order: usize,
    projects: &mut Vec<HostProjectSnapshot>,
    seen: &mut HashSet<PathBuf>,
    directories: &mut usize,
    entries: &mut usize,
) {
    if *directories >= MAX_PROJECT_DIRECTORIES
        || *entries >= MAX_PROJECT_ENTRIES
        || projects.len() >= MAX_HOST_SERVICE_PROJECTS
    {
        return;
    }
    *directories += 1;
    if directory.join(".git").exists() {
        if let Ok(path) = directory.canonicalize()
            && seen.insert(path.clone())
        {
            projects.push(HostProjectSnapshot {
                name: path.file_name().map_or_else(
                    || path.display().to_string(),
                    |name| name.to_string_lossy().into_owned(),
                ),
                path,
                group: group.to_owned(),
                group_order,
            });
        }
        return;
    }
    if depth >= max_depth {
        return;
    }
    let Ok(read) = fs::read_dir(directory) else {
        return;
    };
    let mut children = Vec::new();
    for entry in read {
        if *entries >= MAX_PROJECT_ENTRIES {
            break;
        }
        *entries += 1;
        let Ok(entry) = entry else { continue };
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if entry.file_type().is_ok_and(|kind| kind.is_dir())
            && !name.starts_with('.')
            && !matches!(name.as_ref(), "node_modules" | "target")
        {
            children.push(entry.path());
        }
    }
    children.sort();
    for child in children {
        scan_projects(
            &child,
            depth + 1,
            max_depth,
            group,
            group_order,
            projects,
            seen,
            directories,
            entries,
        );
    }
}

fn push_warning(warnings: &mut Vec<String>, warning: String) {
    if warnings.len() < MAX_HOST_SERVICE_WARNINGS {
        warnings.push(warning);
    }
}

pub(crate) fn suggest_shell_name(workspace: &WorkspaceSnapshot) -> io::Result<String> {
    generated_names::random_excluding(workspace.shells.iter().map(|shell| shell.name.as_str()))
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::AlreadyExists,
                "generated shell names are exhausted",
            )
        })
}

pub(crate) fn invoke_launcher(
    workspace: &WorkspaceSnapshot,
    launcher: &WorkspaceLauncherSnapshot,
) -> io::Result<()> {
    if launcher.workspace_id != workspace.id {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "launcher is not owned by workspace",
        ));
    }
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
        .name(format!("host-launcher-reaper-{}", launcher.id))
        .spawn(move || {
            let _ = child.wait();
        })
        .map_err(|error| io::Error::other(format!("could not start launcher reaper: {error}")))?;
    Ok(())
}

fn integration_ids(requested: &[String]) -> io::Result<Vec<IntegrationId>> {
    if requested.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "at least one integration is required",
        ));
    }
    let mut seen = BTreeSet::new();
    requested
        .iter()
        .map(|name| {
            if !seen.insert(name.clone()) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "duplicate integration",
                ));
            }
            name.parse()
                .map_err(|message: String| io::Error::new(io::ErrorKind::InvalidInput, message))
        })
        .collect()
}

pub(crate) fn integration_status(
    integration: Option<&str>,
    snapshot: &Snapshot,
) -> io::Result<Vec<HostIntegrationStatus>> {
    let ids = match integration {
        Some(name) => integration_ids(&[name.to_owned()])?,
        None => IntegrationId::all().collect(),
    };
    let environment = integration_management::Environment::from_process();
    Ok(ids
        .into_iter()
        .map(|id| integration_management::inspect(id, &environment, Some(snapshot)))
        .map(|status| HostIntegrationStatus {
            name: status.name.into(),
            display_name: status.display_name.into(),
            package: status.package.into(),
            validated_version: status.validated_version.into(),
            host_state: status.host.state.as_str().into(),
            executable: status.host.executable,
            version: status.host.version,
            compatibility: status.host.compatibility.into(),
            host_error: status.host.error,
            asset_state: status.asset.state.as_str().into(),
            path: status.asset.path,
            asset_error: status.asset.error,
            runtime_state: status.runtime.state.as_str().into(),
            running_processes: status.runtime.running_processes,
            tracked_processes: status.runtime.tracked_processes,
            untracked_processes: status.runtime.untracked_processes,
            recommended_action: recommended_action_name(status.recommended_action).into(),
        })
        .collect())
}

fn recommended_action_name(action: integration_management::RecommendedAction) -> &'static str {
    use integration_management::RecommendedAction::*;
    match action {
        None => "none",
        Install => "install",
        Replace => "replace",
        RestartHost => "restart_host",
        InspectError => "inspect_error",
    }
}

pub(crate) fn prepare_integration_mutation(
    action: HostServiceIntegrationAction,
    requested: &[String],
    force: bool,
) -> io::Result<PreparedIntegrationMutation> {
    let ids = integration_ids(requested)?;
    let environment = integration_management::Environment::from_process();
    let plans = match action {
        HostServiceIntegrationAction::Install => ids
            .iter()
            .copied()
            .map(|id| integration_management::plan_install(id, &environment, force))
            .map(|result| result.map_err(boxed_error))
            .collect::<io::Result<Vec<_>>>()?
            .into_iter()
            .map(|plan| HostIntegrationPlan {
                name: plan.name.into(),
                current_state: plan.current_state.as_str().into(),
                action: match plan.action {
                    integration_management::InstallAction::Install => "install",
                    integration_management::InstallAction::Replace => "replace",
                    integration_management::InstallAction::Unchanged => "unchanged",
                }
                .into(),
                path: plan.path,
                restart_required: plan.restart_required,
            })
            .collect(),
        HostServiceIntegrationAction::Uninstall => {
            let mut plans = Vec::new();
            for id in ids.iter().copied() {
                integration_management::preflight_uninstall(id, &environment, force)
                    .map_err(boxed_error)?;
                let status =
                    integration_management::inspect_without_host_probe(id, &environment, None);
                plans.push(HostIntegrationPlan {
                    name: status.name.into(),
                    current_state: status.asset.state.as_str().into(),
                    action: if status.asset.state == integration_management::AssetState::Missing {
                        "unchanged"
                    } else {
                        "remove"
                    }
                    .into(),
                    path: status.asset.path.ok_or_else(|| {
                        io::Error::new(
                            io::ErrorKind::InvalidInput,
                            "integration path is unavailable",
                        )
                    })?,
                    restart_required: status.asset.state
                        != integration_management::AssetState::Missing,
                });
            }
            plans
        }
    };
    Ok(PreparedIntegrationMutation {
        action,
        integrations: requested.to_vec(),
        force,
        plans,
    })
}

pub(crate) fn commit_integration_mutation(
    prepared: &PreparedIntegrationMutation,
) -> io::Result<Vec<HostIntegrationMutationResult>> {
    let current =
        prepare_integration_mutation(prepared.action, &prepared.integrations, prepared.force)?;
    if current != *prepared {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "integration target changed after preview; inspect and confirm again",
        ));
    }
    let ids = integration_ids(&prepared.integrations)?;
    let environment = integration_management::Environment::from_process();
    match prepared.action {
        HostServiceIntegrationAction::Install => ids
            .into_iter()
            .map(|id| integration_management::install(id, &environment, prepared.force))
            .map(|result| {
                let result = result.map_err(boxed_error)?;
                Ok(HostIntegrationMutationResult {
                    name: result.name.into(),
                    result: match result.result {
                        integration_management::InstallOutcome::Installed => "installed",
                        integration_management::InstallOutcome::Replaced => "replaced",
                        integration_management::InstallOutcome::Unchanged => "unchanged",
                    }
                    .into(),
                    path: result.path,
                    restart_required: result.restart_required,
                })
            })
            .collect(),
        HostServiceIntegrationAction::Uninstall => ids
            .into_iter()
            .map(|id| integration_management::uninstall(id, &environment, prepared.force))
            .map(|result| {
                let result = result.map_err(boxed_error)?;
                Ok(HostIntegrationMutationResult {
                    name: result.name.into(),
                    result: match result.result {
                        integration_management::UninstallOutcome::Removed => "removed",
                        integration_management::UninstallOutcome::NotInstalled => "not_installed",
                    }
                    .into(),
                    path: result.path,
                    restart_required: result.restart_required,
                })
            })
            .collect(),
    }
}

fn boxed_error(error: Box<dyn std::error::Error>) -> io::Error {
    if let Some(error) = error.downcast_ref::<io::Error>() {
        return io::Error::new(error.kind(), error.to_string());
    }
    io::Error::other(error.to_string())
}

pub(crate) fn verify_integration(
    snapshot: &Snapshot,
    integration: &str,
    shell_id: &str,
    run_id: &str,
) -> io::Result<Vec<AgentInstanceSnapshot>> {
    let id = integration_ids(&[integration.to_owned()])?.remove(0);
    let target = integration_management::VerificationTarget {
        shell_id: shell_id.to_owned(),
        run_id: run_id.to_owned(),
    };
    match integration_management::check_verification_target(snapshot, id, &target) {
        integration_management::VerificationCheck::Verified { agents, .. } => Ok(agents),
        integration_management::VerificationCheck::Pending => Err(io::Error::new(
            io::ErrorKind::WouldBlock,
            "integration has not reported authoritative lifecycle state",
        )),
        integration_management::VerificationCheck::Missing => Err(io::Error::new(
            io::ErrorKind::NotFound,
            "integration host shell is not running",
        )),
        integration_management::VerificationCheck::RunChanged => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "integration host shell run changed",
        )),
    }
}

pub(crate) fn sessions(snapshot: &Snapshot) -> Vec<SessionProjection> {
    let directories = workspace_directories(&snapshot.workspaces)
        .into_iter()
        .take(MAX_CATALOG_DIRECTORIES)
        .collect::<Vec<_>>();
    let catalog = thread::scope(|scope| {
        directories
            .into_iter()
            .flat_map(|directory| {
                crate::host_session_titles::catalog_integrations()
                    .map(move |integration| (integration, directory.clone()))
            })
            .map(|(integration, directory)| {
                scope.spawn(move || crate::host_session_titles::catalog(integration, &directory))
            })
            .collect::<Vec<_>>()
            .into_iter()
            .filter_map(|handle| handle.join().ok().flatten())
            .flatten()
            .collect::<Vec<_>>()
    });
    let mut sessions = session_projection::project_snapshot_with_catalog(snapshot, Some(&catalog));
    sessions.truncate(MAX_HOST_SERVICE_SESSIONS);
    sessions
}

fn workspace_directories(workspaces: &[WorkspaceSnapshot]) -> BTreeSet<PathBuf> {
    workspaces
        .iter()
        .flat_map(|workspace| {
            workspace
                .default_cwd
                .iter()
                .cloned()
                .chain(workspace.shells.iter().map(|shell| shell.cwd.clone()))
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

pub(crate) fn session_summary(session: &SessionProjection) -> HostAgentSessionSummary {
    HostAgentSessionSummary {
        id: session.id.clone(),
        workspace_id: session.workspace_id.clone(),
        workspace_name: session.workspace_name.clone(),
        description: session.description.clone(),
        integration: session.integration.clone(),
        external_session_id: session.external_session_id.clone(),
        state: session.state,
        state_is_current: session.state_is_current,
        started_at_ms: session.started_at_ms,
        last_at_ms: session.last_at_ms,
        occurrence_count: session.occurrences.len(),
    }
}

pub(crate) fn inspect_session(
    snapshot: &Snapshot,
    session_id: &str,
) -> io::Result<HostAgentSessionInspection> {
    let sessions = sessions(snapshot);
    let session = session_projection::resolve_exact(&sessions, session_id).map_err(|error| {
        io::Error::new(
            match error {
                session_projection::ResolveError::NotFound => io::ErrorKind::NotFound,
                session_projection::ResolveError::DuplicateId => io::ErrorKind::InvalidData,
            },
            "exact Agent Session was not found",
        )
    })?;
    let agent_ids = session
        .occurrences
        .iter()
        .map(|occurrence| occurrence.agent_id.as_str())
        .collect::<HashSet<_>>();
    let occurrences = snapshot
        .workspaces
        .iter()
        .flat_map(|workspace| &workspace.agents)
        .filter(|agent| agent_ids.contains(agent.id.as_str()))
        .take(MAX_HOST_SERVICE_SESSIONS)
        .cloned()
        .collect();
    Ok(HostAgentSessionInspection {
        summary: session_summary(session),
        source_cwd: session.source_cwd.clone(),
        occurrences,
    })
}

pub(crate) fn prepare_session_resume(
    snapshot: &Snapshot,
    session_id: &str,
) -> io::Result<HostAgentSessionResumePlan> {
    let sessions = sessions(snapshot);
    let session = session_projection::resolve_exact(&sessions, session_id).map_err(|_| {
        io::Error::new(io::ErrorKind::NotFound, "exact Agent Session was not found")
    })?;
    if session.state_is_current {
        return Err(io::Error::new(
            io::ErrorKind::WouldBlock,
            "Agent Session is already active",
        ));
    }
    if session.state == crate::protocol::AgentState::Done {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Agent Session is permanently done",
        ));
    }
    let external_id = session.external_session_id.as_deref().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "Agent Session has no canonical external ID",
        )
    })?;
    let descriptor = crate::integrations::by_key(&session.integration)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "unknown Agent integration"))?;
    let resume = descriptor.resume.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::Unsupported,
            "integration does not support session resume",
        )
    })?;
    let cwd = resolve_directory(session.source_cwd.as_deref().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "Agent Session has no retained working directory",
        )
    })?)?;
    let argv = resume.command(&[], external_id).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "could not build exact resume argv",
        )
    })?;
    Ok(HostAgentSessionResumePlan {
        session_id: session.id.clone(),
        workspace_id: session.workspace_id.clone(),
        workspace_name: session.workspace_name.clone(),
        integration: session.integration.clone(),
        cwd,
        argv,
    })
}

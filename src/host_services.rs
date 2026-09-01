use std::collections::{BTreeSet, HashMap, HashSet};
use std::env;
use std::fs;
use std::io::{self, Read};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use serde::Deserialize;

use crate::generated_names;
use crate::integration_management::{self, IntegrationId};
#[cfg(test)]
use crate::protocol::{
    AgentAttentionReason, HostAgentSessionAttention, HostAgentSessionInspection,
    HostAgentSessionSummary, HostAgentSessionWorkingContext, HostGitPushStatus,
    HostGitWorktreeStatus, MAX_HOST_SERVICE_SESSIONS, MAX_SESSION_INSPECTION_WORKING_CONTEXTS,
    MAX_SESSION_WORKING_CONTEXTS,
};
use crate::protocol::{
    AgentInstanceSnapshot, AgentWorkingContextSnapshot, HostAgentSessionResumePlan,
    HostIntegrationMutationResult, HostIntegrationPlan, HostIntegrationStatus,
    HostProjectDiscovery, HostProjectSnapshot, HostServiceIntegrationAction,
    MAX_HOST_SERVICE_PROJECTS, MAX_HOST_SERVICE_WARNINGS, Snapshot, WorkspaceLauncherSnapshot,
    WorkspaceSnapshot,
};
use crate::session_projection::{self, SessionProjection};

const MAX_PROJECT_DIRECTORIES: usize = 10_000;
const MAX_PROJECT_ENTRIES: usize = 50_000;
const MAX_CATALOG_DIRECTORIES: usize = 8;
const MAX_GIT_OUTPUT_BYTES: usize = 4 * 1024;
const GIT_COMMAND_TIMEOUT: Duration = Duration::from_secs(1);

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

pub(crate) fn inspect_working_context(
    path: &Path,
) -> io::Result<Option<AgentWorkingContextSnapshot>> {
    if !path.is_absolute() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Agent working-context path must be absolute",
        ));
    }
    let canonical = fs::canonicalize(path)?;
    let directory = if canonical.is_dir() {
        canonical
    } else {
        canonical.parent().map(Path::to_path_buf).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "Agent working-context path has no parent directory",
            )
        })?
    };
    let Some(worktree_root) = git_output(&directory, &["rev-parse", "--show-toplevel"])? else {
        return Ok(None);
    };
    let worktree_root = fs::canonicalize(worktree_root)?;
    let repository = git_output(&worktree_root, &["rev-parse", "--git-common-dir"])?
        .and_then(|common| repository_name(&worktree_root, &common))
        .or_else(|| {
            worktree_root
                .file_name()
                .and_then(|name| name.to_str())
                .map(str::to_owned)
        })
        .ok_or_else(|| io::Error::other("Git worktree has no repository name"))?;
    let branch = match git_output(&worktree_root, &["symbolic-ref", "--short", "-q", "HEAD"])? {
        Some(branch) => branch,
        None => {
            let Some(_) = git_output(&worktree_root, &["rev-parse", "--verify", "HEAD"])? else {
                return Ok(None);
            };
            "detached".to_owned()
        }
    };
    validate_git_label("repository", &repository)?;
    validate_git_label("branch", &branch)?;
    Ok(Some(AgentWorkingContextSnapshot {
        worktree_root,
        repository,
        branch,
        observed_at_ms: 0,
    }))
}

#[cfg(test)]
#[derive(Debug, Default, PartialEq, Eq)]
struct WorkingContextPresentationStatus {
    push_status: Option<HostGitPushStatus>,
    worktree_status: Option<HostGitWorktreeStatus>,
}

#[cfg(test)]
fn inspect_presentation_status(
    context: &AgentWorkingContextSnapshot,
) -> WorkingContextPresentationStatus {
    if context.branch == "detached" {
        return WorkingContextPresentationStatus::default();
    }
    let Ok(Some(branch)) = git_output(
        &context.worktree_root,
        &["symbolic-ref", "--short", "-q", "HEAD"],
    ) else {
        return WorkingContextPresentationStatus::default();
    };
    if branch != context.branch {
        return WorkingContextPresentationStatus::default();
    }

    WorkingContextPresentationStatus {
        push_status: inspect_push_status(context).ok().flatten(),
        worktree_status: inspect_worktree_status(context).ok().flatten(),
    }
}

#[cfg(test)]
fn inspect_push_status(
    context: &AgentWorkingContextSnapshot,
) -> io::Result<Option<HostGitPushStatus>> {
    if let Some(counts) = git_output(
        &context.worktree_root,
        &["rev-list", "--left-right", "--count", "HEAD...@{upstream}"],
    )? {
        let mut counts = counts.split_ascii_whitespace();
        let ahead = counts
            .next()
            .and_then(|count| count.parse::<u64>().ok())
            .ok_or_else(|| io::Error::other("Git ahead count was invalid"))?;
        let _behind = counts
            .next()
            .and_then(|count| count.parse::<u64>().ok())
            .ok_or_else(|| io::Error::other("Git behind count was invalid"))?;
        if counts.next().is_some() {
            return Err(io::Error::other("Git ahead count had extra fields"));
        }
        return Ok(Some(if ahead == 0 {
            HostGitPushStatus::UpToDate
        } else {
            HostGitPushStatus::Ahead {
                commit_count: ahead,
            }
        }));
    }
    Ok(git_output(&context.worktree_root, &["remote"])?.map(|_| HostGitPushStatus::Unpublished))
}

#[cfg(test)]
fn inspect_worktree_status(
    context: &AgentWorkingContextSnapshot,
) -> io::Result<Option<HostGitWorktreeStatus>> {
    let Some(output) = git_output(
        &context.worktree_root,
        &[
            "--no-optional-locks",
            "status",
            "--porcelain=v1",
            "--branch",
            "--untracked-files=normal",
        ],
    )?
    else {
        return Ok(None);
    };
    parse_worktree_status(&output, &context.branch)
}

#[cfg(test)]
fn parse_worktree_status(
    output: &str,
    expected_branch: &str,
) -> io::Result<Option<HostGitWorktreeStatus>> {
    let mut lines = output.lines();
    let branch_header = lines
        .next()
        .and_then(|line| line.strip_prefix("## "))
        .ok_or_else(|| io::Error::other("Git status had no branch header"))?;
    let branch = if let Some(branch) = branch_header
        .strip_prefix("No commits yet on ")
        .or_else(|| branch_header.strip_prefix("Initial commit on "))
    {
        branch
    } else {
        branch_header
            .split_once("...")
            .map_or(branch_header, |(branch, _)| branch)
    };
    if branch != expected_branch {
        return Ok(None);
    }

    let mut status = HostGitWorktreeStatus::default();
    for line in lines {
        let code = line
            .as_bytes()
            .get(..2)
            .ok_or_else(|| io::Error::other("Git status entry had no two-character status code"))?;
        match code {
            b"??" => status.unstaged_or_untracked = true,
            b"!!" => {}
            [index, worktree] => {
                status.staged |= *index != b' ';
                status.unstaged_or_untracked |= *worktree != b' ';
            }
            _ => unreachable!("two-byte status code"),
        }
    }
    Ok(Some(status))
}

fn git_output(directory: &Path, arguments: &[&str]) -> io::Result<Option<String>> {
    let mut child = Command::new("git")
        .arg("-C")
        .arg(directory)
        .args(arguments)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0)
        .spawn()?;
    let process_group = i32::try_from(child.id()).map_err(|_| {
        let _ = child.kill();
        let _ = child.wait();
        io::Error::other("Git process ID exceeded i32")
    })?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| io::Error::other("Git stdout was unavailable"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| io::Error::other("Git stderr was unavailable"))?;
    let (sender, receiver) = mpsc::channel();
    spawn_git_output_reader(0, stdout, sender.clone());
    spawn_git_output_reader(1, stderr, sender.clone());
    drop(sender);

    let deadline = Instant::now() + GIT_COMMAND_TIMEOUT;
    let result = (|| {
        let mut status = None;
        let mut outputs = [None, None];
        loop {
            if status.is_none() {
                status = child.try_wait()?;
            }
            while let Ok((index, output)) = receiver.try_recv() {
                let output = output?;
                if output.len() > MAX_GIT_OUTPUT_BYTES {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "Git output exceeds the size limit",
                    ));
                }
                outputs[index] = Some(output);
            }
            if let (Some(status), [Some(stdout), Some(_stderr)]) = (status, &mut outputs) {
                if !status.success() {
                    return Ok(None);
                }
                return Ok(Some(std::mem::take(stdout)));
            }
            if Instant::now() >= deadline {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "Git metadata inspection timed out",
                ));
            }
            thread::sleep(Duration::from_millis(10));
        }
    })();
    unsafe {
        libc::kill(-process_group, libc::SIGKILL);
    }
    let _ = child.wait();

    let Some(stdout) = result? else {
        return Ok(None);
    };
    let value = String::from_utf8(stdout)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "Git metadata is not UTF-8"))?;
    let value = value.trim();
    Ok((!value.is_empty()).then(|| value.to_owned()))
}

fn spawn_git_output_reader(
    index: usize,
    mut output: impl Read + Send + 'static,
    sender: mpsc::Sender<(usize, io::Result<Vec<u8>>)>,
) {
    thread::spawn(move || {
        let mut bytes = Vec::new();
        let result = output
            .by_ref()
            .take((MAX_GIT_OUTPUT_BYTES + 1) as u64)
            .read_to_end(&mut bytes)
            .map(|_| bytes);
        let _ = sender.send((index, result));
    });
}

fn repository_name(worktree_root: &Path, common_directory: &str) -> Option<String> {
    let common_directory = PathBuf::from(common_directory);
    let common_directory = if common_directory.is_absolute() {
        common_directory
    } else {
        worktree_root.join(common_directory)
    };
    let repository_root = if common_directory
        .file_name()
        .is_some_and(|name| name == ".git")
    {
        common_directory.parent()?
    } else {
        &common_directory
    };
    repository_root
        .file_name()
        .and_then(|name| name.to_str())
        .map(|name| name.strip_suffix(".git").unwrap_or(name).to_owned())
        .filter(|name| !name.is_empty())
}

fn validate_git_label(kind: &str, value: &str) -> io::Result<()> {
    if value.len() > 256 || value.chars().any(char::is_control) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("Git {kind} is invalid"),
        ));
    }
    Ok(())
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

pub(crate) fn durable_sessions(snapshot: &Snapshot) -> Vec<SessionProjection> {
    session_projection::project_snapshot_with_catalog(snapshot, None)
}

pub(crate) fn session_catalog_directories(snapshot: &Snapshot) -> Vec<PathBuf> {
    session_projection::catalog_directories(&snapshot.workspaces)
        .into_iter()
        .take(MAX_CATALOG_DIRECTORIES)
        .collect()
}

pub(crate) fn session_catalog_requests(
    snapshot: &Snapshot,
) -> Vec<crate::host_session_titles::ProjectionRequest> {
    let directories = session_catalog_directories(snapshot);
    let allowed = directories.iter().cloned().collect::<HashSet<_>>();
    let mut requests = BTreeSet::new();
    for directory in &directories {
        for integration in crate::host_session_titles::catalog_integrations() {
            requests.insert(crate::host_session_titles::ProjectionRequest {
                integration: integration.to_owned(),
                directory: directory.clone(),
            });
        }
    }
    for workspace in &snapshot.workspaces {
        let shells = workspace
            .shells
            .iter()
            .map(|shell| (shell.id.as_str(), shell.cwd.as_path()))
            .collect::<HashMap<_, _>>();
        for agent in &workspace.agents {
            if agent.external_session_id.is_none()
                || crate::integrations::by_key(&agent.integration)
                    .and_then(|descriptor| descriptor.titles)
                    .is_none_or(|titles| titles.provides_catalog)
            {
                continue;
            }
            let directory = agent
                .cwd
                .as_deref()
                .or_else(|| shells.get(agent.shell_id.as_str()).copied())
                .and_then(crate::host_session_source::normalize_absolute);
            if let Some(directory) = directory.filter(|directory| allowed.contains(directory)) {
                requests.insert(crate::host_session_titles::ProjectionRequest {
                    integration: agent.integration.clone(),
                    directory,
                });
            }
        }
    }
    requests.into_iter().collect()
}

pub(crate) fn sessions_with_catalog(
    snapshot: &Snapshot,
    catalog: &[crate::host_session_titles::HostSession],
) -> Vec<SessionProjection> {
    session_projection::project_snapshot_with_catalog(snapshot, Some(catalog))
}

#[cfg(test)]
pub(crate) fn session_summaries(
    snapshot: &Snapshot,
    sessions: &[SessionProjection],
) -> Vec<HostAgentSessionSummary> {
    session_summaries_with_context_limit(snapshot, sessions, MAX_SESSION_WORKING_CONTEXTS)
}

#[cfg(test)]
fn session_summaries_with_context_limit(
    snapshot: &Snapshot,
    sessions: &[SessionProjection],
    context_limit: usize,
) -> Vec<HostAgentSessionSummary> {
    let agents = snapshot
        .workspaces
        .iter()
        .flat_map(|workspace| {
            workspace
                .agents
                .iter()
                .map(move |agent| ((workspace.id.as_str(), agent.id.as_str()), agent))
        })
        .collect::<HashMap<_, _>>();
    let mut launch_contexts = HashMap::new();
    sessions
        .iter()
        .map(|session| {
            let launch_context = session.source_cwd.as_ref().and_then(|directory| {
                launch_contexts
                    .entry(directory.clone())
                    .or_insert_with(|| inspect_working_context(directory).ok().flatten())
                    .clone()
            });
            let git_branch = launch_context
                .as_ref()
                .map(|context| context.branch.clone());
            let launch_root = launch_context
                .as_ref()
                .map(|context| context.worktree_root.as_path());
            let mut attentions = session
                .occurrences
                .iter()
                .filter_map(|occurrence| {
                    let agent = agents
                        .get(&(session.workspace_id.as_str(), occurrence.agent_id.as_str()))?;
                    let attention = agent.attention.as_ref()?;
                    Some(HostAgentSessionAttention {
                        agent_id: agent.id.clone(),
                        reason: attention.reason,
                        observation_revision: attention.observation.revision,
                        observed_at_ms: attention.observation.observed_at_ms,
                    })
                })
                .collect::<Vec<_>>();
            let latest_agent_name = session
                .occurrences
                .iter()
                .filter_map(|occurrence| {
                    agents
                        .get(&(session.workspace_id.as_str(), occurrence.agent_id.as_str()))
                        .map(|agent| (agent.started_at_ms, agent.id.as_str(), agent.name.as_str()))
                })
                .max_by(|left, right| left.0.cmp(&right.0).then_with(|| left.1.cmp(right.1)))
                .map(|(_, _, name)| name.to_owned());
            attentions.sort_by(|left, right| {
                attention_rank(left.reason)
                    .cmp(&attention_rank(right.reason))
                    .then_with(|| right.observed_at_ms.cmp(&left.observed_at_ms))
                    .then_with(|| left.agent_id.cmp(&right.agent_id))
            });
            let mut contexts_by_root = HashMap::new();
            for occurrence in &session.occurrences {
                let Some(agent) =
                    agents.get(&(session.workspace_id.as_str(), occurrence.agent_id.as_str()))
                else {
                    continue;
                };
                for context in &agent.working_contexts {
                    if launch_root == Some(context.worktree_root.as_path()) {
                        continue;
                    }
                    let current = contexts_by_root
                        .entry(context.worktree_root.clone())
                        .or_insert(context);
                    if context.observed_at_ms > current.observed_at_ms {
                        *current = context;
                    }
                }
            }
            let working_context_count = contexts_by_root.len();
            let mut working_contexts = contexts_by_root.into_values().collect::<Vec<_>>();
            working_contexts.sort_by(|left, right| {
                right
                    .observed_at_ms
                    .cmp(&left.observed_at_ms)
                    .then_with(|| left.repository.cmp(&right.repository))
                    .then_with(|| left.branch.cmp(&right.branch))
            });
            working_contexts.truncate(context_limit);
            let working_contexts = working_contexts
                .into_iter()
                .enumerate()
                .map(|(index, context)| {
                    let status = if index < MAX_SESSION_WORKING_CONTEXTS {
                        inspect_presentation_status(context)
                    } else {
                        WorkingContextPresentationStatus::default()
                    };
                    HostAgentSessionWorkingContext {
                        repository: context.repository.clone(),
                        branch: context.branch.clone(),
                        observed_at_ms: context.observed_at_ms,
                        push_status: status.push_status,
                        worktree_status: status.worktree_status,
                    }
                })
                .collect();
            session_summary(
                session,
                latest_agent_name,
                attentions,
                git_branch,
                working_contexts,
                working_context_count,
            )
        })
        .collect()
}

#[cfg(test)]
pub(crate) fn session_summary_with_snapshot(
    snapshot: &Snapshot,
    session: &SessionProjection,
) -> HostAgentSessionSummary {
    session_summaries(snapshot, std::slice::from_ref(session))
        .pop()
        .expect("one Session produces one summary")
}

#[cfg(test)]
fn session_inspection_summary_with_snapshot(
    snapshot: &Snapshot,
    session: &SessionProjection,
) -> HostAgentSessionSummary {
    session_summaries_with_context_limit(
        snapshot,
        std::slice::from_ref(session),
        MAX_SESSION_INSPECTION_WORKING_CONTEXTS,
    )
    .pop()
    .expect("one Session produces one inspection summary")
}

#[cfg(test)]
fn session_summary(
    session: &SessionProjection,
    latest_agent_name: Option<String>,
    attentions: Vec<HostAgentSessionAttention>,
    git_branch: Option<String>,
    working_contexts: Vec<HostAgentSessionWorkingContext>,
    working_context_count: usize,
) -> HostAgentSessionSummary {
    HostAgentSessionSummary {
        id: session.id.clone(),
        workspace_id: session.workspace_id.clone(),
        workspace_name: session.workspace_name.clone(),
        description: session.description.clone(),
        latest_agent_name,
        user_display_name: session.user_display_name.clone(),
        workspace_revision: session.workspace_revision,
        integration: session.integration.clone(),
        external_session_id: session.external_session_id.clone(),
        state: session.state,
        state_is_current: session.state_is_current,
        started_at_ms: session.started_at_ms,
        last_at_ms: session.last_at_ms,
        occurrence_count: session.occurrences.len(),
        attentions,
        git_branch,
        working_contexts,
        working_context_count,
    }
}

#[cfg(test)]
fn attention_rank(reason: AgentAttentionReason) -> u8 {
    match reason {
        AgentAttentionReason::Blocked => 0,
        AgentAttentionReason::Completed => 1,
    }
}

#[cfg(test)]
pub(crate) fn inspect_projected_session(
    snapshot: &Snapshot,
    sessions: &[SessionProjection],
    session_id: &str,
) -> io::Result<HostAgentSessionInspection> {
    let session = session_projection::resolve_exact(sessions, session_id).map_err(|error| {
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
        summary: session_inspection_summary_with_snapshot(snapshot, session),
        source_cwd: session.source_cwd.clone(),
        occurrences,
        projected_occurrences: session
            .occurrences
            .iter()
            .map(|occurrence| crate::protocol::HostAgentSessionOccurrence {
                agent_id: occurrence.agent_id.clone(),
                shell_id: occurrence.shell_id.clone(),
                retained_shell_name: occurrence.retained_shell_name.clone(),
                retained_shell_cwd: occurrence.retained_shell_cwd.clone(),
                source_cwd: occurrence.source_cwd.clone(),
                run_id: occurrence.run_id.clone(),
                started_at_ms: occurrence.started_at_ms,
                ended_at_ms: occurrence.ended_at_ms,
                is_current: occurrence.is_current,
                observation: occurrence.observation.clone(),
            })
            .collect(),
    })
}

pub(crate) fn prepare_session_resume(
    sessions: &[SessionProjection],
    session_id: &str,
) -> io::Result<HostAgentSessionResumePlan> {
    let session = session_projection::resolve_exact(sessions, session_id).map_err(|_| {
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

#[cfg(test)]
mod tests {
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::*;
    use crate::protocol::{
        AgentAttentionSnapshot, AgentAuthority, AgentObservationSnapshot, AgentState,
    };
    use crate::session_projection::SessionOccurrence;

    fn catalog_agent(id: &str, integration: &str, cwd: &str) -> AgentInstanceSnapshot {
        AgentInstanceSnapshot {
            id: id.into(),
            workspace_id: "workspace".into(),
            shell_id: format!("shell-{id}"),
            run_id: format!("run-{id}"),
            name: integration.into(),
            integration: integration.into(),
            external_session_id: Some(format!("session-{id}")),
            cwd: Some(cwd.into()),
            started_at_ms: 1,
            ended_at_ms: None,
            observation: AgentObservationSnapshot {
                state: AgentState::Inactive,
                authority: AgentAuthority::LifecycleIntegration,
                evidence: "test".into(),
                confidence: 100,
                observed_at_ms: 1,
                revision: 1,
            },
            attention: None,
            working_contexts: Vec::new(),
        }
    }

    #[test]
    fn session_catalog_plan_limits_title_only_providers_to_matching_agents() {
        let snapshot = Snapshot {
            workspaces: vec![WorkspaceSnapshot {
                id: "workspace".into(),
                revision: 1,
                name: "workspace".into(),
                default_cwd: Some("/unused-default".into()),
                shells: Vec::new(),
                launchers: Vec::new(),
                agents: vec![
                    catalog_agent("open", "opencode", "/repo/open/./"),
                    catalog_agent("pi", "pi", "/repo/pi"),
                ],
            }],
            focused_terminal: None,
        };

        let requests = session_catalog_requests(&snapshot)
            .into_iter()
            .map(|request| (request.integration, request.directory))
            .collect::<BTreeSet<_>>();

        assert_eq!(
            requests,
            BTreeSet::from([
                ("codex".into(), PathBuf::from("/repo/open")),
                ("codex".into(), PathBuf::from("/repo/pi")),
                ("opencode".into(), PathBuf::from("/repo/open")),
                ("opencode".into(), PathBuf::from("/repo/pi")),
                ("pi".into(), PathBuf::from("/repo/pi")),
            ])
        );
    }

    #[test]
    fn working_context_inspection_resolves_owner_git_metadata() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let directory = std::env::temp_dir().join(format!("boomux-working-context-{nonce}"));
        let repository = directory.join("boomux");
        fs::create_dir_all(&repository).unwrap();
        assert!(
            Command::new("git")
                .args(["init", "-q", "-b", "feat/working-contexts"])
                .arg(&repository)
                .status()
                .unwrap()
                .success()
        );

        let context = inspect_working_context(&repository).unwrap().unwrap();

        assert_eq!(context.worktree_root, repository.canonicalize().unwrap());
        assert_eq!(context.repository, "boomux");
        assert_eq!(context.branch, "feat/working-contexts");
        assert_eq!(context.observed_at_ms, 0);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn presentation_status_uses_only_the_current_branch_and_keeps_results_independent() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let directory = std::env::temp_dir().join(format!("boomux-push-status-{nonce}"));
        let remote = directory.join("remote.git");
        let repository = directory.join("boomux");
        fs::create_dir_all(&directory).unwrap();
        assert!(
            Command::new("git")
                .args(["init", "--bare", "-q"])
                .arg(&remote)
                .status()
                .unwrap()
                .success()
        );
        assert!(
            Command::new("git")
                .args(["init", "-q", "-b", "main"])
                .arg(&repository)
                .status()
                .unwrap()
                .success()
        );
        assert!(
            Command::new("git")
                .args([
                    "-C",
                    repository.to_str().unwrap(),
                    "remote",
                    "add",
                    "origin"
                ])
                .arg(&remote)
                .status()
                .unwrap()
                .success()
        );
        fs::write(repository.join("README.md"), "first\n").unwrap();
        assert!(
            Command::new("git")
                .args(["-C", repository.to_str().unwrap(), "add", "README.md",])
                .status()
                .unwrap()
                .success()
        );
        let commit = |message: &str| {
            Command::new("git")
                .args([
                    "-C",
                    repository.to_str().unwrap(),
                    "-c",
                    "user.name=Boomux Test",
                    "-c",
                    "user.email=boomux@example.invalid",
                    "commit",
                    "-q",
                    "-am",
                    message,
                ])
                .status()
                .unwrap()
                .success()
        };
        assert!(commit("first"));
        assert!(
            Command::new("git")
                .args([
                    "-C",
                    repository.to_str().unwrap(),
                    "push",
                    "-q",
                    "-u",
                    "origin",
                    "main",
                ])
                .status()
                .unwrap()
                .success()
        );

        let main = inspect_working_context(&repository).unwrap().unwrap();
        assert_eq!(
            inspect_presentation_status(&main),
            WorkingContextPresentationStatus {
                push_status: Some(HostGitPushStatus::UpToDate),
                worktree_status: Some(HostGitWorktreeStatus {
                    staged: false,
                    unstaged_or_untracked: false,
                }),
            }
        );

        fs::write(repository.join("README.md"), "second\n").unwrap();
        assert_eq!(
            inspect_presentation_status(&main).worktree_status,
            Some(HostGitWorktreeStatus {
                staged: false,
                unstaged_or_untracked: true,
            })
        );
        assert!(
            Command::new("git")
                .args(["-C", repository.to_str().unwrap(), "add", "README.md"])
                .status()
                .unwrap()
                .success()
        );
        assert_eq!(
            inspect_presentation_status(&main).worktree_status,
            Some(HostGitWorktreeStatus {
                staged: true,
                unstaged_or_untracked: false,
            })
        );
        fs::write(repository.join("README.md"), "third\n").unwrap();
        assert_eq!(
            inspect_presentation_status(&main).worktree_status,
            Some(HostGitWorktreeStatus {
                staged: true,
                unstaged_or_untracked: true,
            })
        );
        assert!(
            Command::new("git")
                .args([
                    "-C",
                    repository.to_str().unwrap(),
                    "reset",
                    "--hard",
                    "-q",
                    "HEAD",
                ])
                .status()
                .unwrap()
                .success()
        );
        fs::write(repository.join("untracked.txt"), "untracked\n").unwrap();
        assert_eq!(
            inspect_presentation_status(&main).worktree_status,
            Some(HostGitWorktreeStatus {
                staged: false,
                unstaged_or_untracked: true,
            })
        );
        fs::remove_file(repository.join("untracked.txt")).unwrap();

        fs::write(repository.join("README.md"), "second\n").unwrap();
        assert!(commit("second"));
        assert_eq!(
            inspect_presentation_status(&main),
            WorkingContextPresentationStatus {
                push_status: Some(HostGitPushStatus::Ahead { commit_count: 1 }),
                worktree_status: Some(HostGitWorktreeStatus {
                    staged: false,
                    unstaged_or_untracked: false,
                }),
            }
        );

        let oversized_paths = (0..200)
            .map(|index| repository.join(format!("untracked-status-entry-{index:04}.txt")))
            .collect::<Vec<_>>();
        for path in &oversized_paths {
            fs::write(path, "untracked\n").unwrap();
        }
        let oversized_status = inspect_presentation_status(&main);
        assert_eq!(
            oversized_status.push_status,
            Some(HostGitPushStatus::Ahead { commit_count: 1 })
        );
        assert_eq!(oversized_status.worktree_status, None);
        for path in oversized_paths {
            fs::remove_file(path).unwrap();
        }

        assert!(
            Command::new("git")
                .args([
                    "-C",
                    repository.to_str().unwrap(),
                    "switch",
                    "-q",
                    "-c",
                    "unpublished",
                ])
                .status()
                .unwrap()
                .success()
        );
        let unpublished = inspect_working_context(&repository).unwrap().unwrap();
        assert_eq!(
            inspect_presentation_status(&unpublished),
            WorkingContextPresentationStatus {
                push_status: Some(HostGitPushStatus::Unpublished),
                worktree_status: Some(HostGitWorktreeStatus {
                    staged: false,
                    unstaged_or_untracked: false,
                }),
            }
        );
        assert_eq!(
            inspect_presentation_status(&main),
            WorkingContextPresentationStatus::default()
        );
        assert!(
            Command::new("git")
                .args([
                    "-C",
                    repository.to_str().unwrap(),
                    "remote",
                    "remove",
                    "origin",
                ])
                .status()
                .unwrap()
                .success()
        );
        assert_eq!(
            inspect_presentation_status(&unpublished),
            WorkingContextPresentationStatus {
                push_status: None,
                worktree_status: Some(HostGitWorktreeStatus {
                    staged: false,
                    unstaged_or_untracked: false,
                }),
            }
        );

        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn worktree_status_parser_treats_conflicts_as_staged_and_unstaged() {
        assert_eq!(
            parse_worktree_status("## main\nUU conflicted.txt\n", "main").unwrap(),
            Some(HostGitWorktreeStatus {
                staged: true,
                unstaged_or_untracked: true,
            })
        );
        assert_eq!(
            parse_worktree_status("## other\n M tracked.txt\n", "main").unwrap(),
            None
        );
    }

    #[test]
    fn git_metadata_inspection_bounds_output_and_runtime() {
        let directory = std::env::temp_dir();
        let oversized = git_output(
            &directory,
            &["-c", "alias.large=!head -c 4097 /dev/zero", "large"],
        )
        .unwrap_err();
        assert_eq!(oversized.kind(), io::ErrorKind::InvalidData);

        let started = Instant::now();
        let timeout = git_output(&directory, &["-c", "alias.hang=!sleep 5", "hang"]).unwrap_err();
        assert_eq!(timeout.kind(), io::ErrorKind::TimedOut);
        assert!(started.elapsed() < Duration::from_secs(3));
    }

    fn observation(
        revision: u64,
        state: AgentState,
        observed_at_ms: u64,
    ) -> AgentObservationSnapshot {
        AgentObservationSnapshot {
            revision,
            state,
            authority: AgentAuthority::LifecycleIntegration,
            evidence: "test".into(),
            confidence: 100,
            observed_at_ms,
        }
    }

    #[test]
    fn session_summaries_project_attention_launch_branch_and_other_observed_contexts() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let repository = std::env::temp_dir().join(format!("boomux-session-context-{nonce}"));
        assert!(
            Command::new("git")
                .args(["init", "-q", "-b", "feat/session-radar"])
                .arg(&repository)
                .status()
                .unwrap()
                .success()
        );
        let launch_root = repository.canonicalize().unwrap();
        let blocked = observation(4, AgentState::Blocked, 20);
        let completed = observation(3, AgentState::Done, 30);
        let agents = vec![
            AgentInstanceSnapshot {
                id: "blocked".into(),
                workspace_id: "workspace".into(),
                shell_id: "shell-a".into(),
                run_id: "run-a".into(),
                name: "blocked".into(),
                integration: "opencode".into(),
                external_session_id: Some("external".into()),
                cwd: Some(repository.clone()),
                started_at_ms: 1,
                ended_at_ms: None,
                observation: blocked.clone(),
                attention: Some(AgentAttentionSnapshot {
                    reason: AgentAttentionReason::Blocked,
                    observation: blocked.clone(),
                }),
                working_contexts: vec![
                    AgentWorkingContextSnapshot {
                        worktree_root: launch_root,
                        repository: "boomux-session-context".into(),
                        branch: "feat/session-radar".into(),
                        observed_at_ms: 50,
                    },
                    AgentWorkingContextSnapshot {
                        worktree_root: "/worktrees/boomux".into(),
                        repository: "boomux".into(),
                        branch: "feat/session-radar".into(),
                        observed_at_ms: 40,
                    },
                    AgentWorkingContextSnapshot {
                        worktree_root: "/worktrees/service".into(),
                        repository: "service".into(),
                        branch: "main".into(),
                        observed_at_ms: 25,
                    },
                    AgentWorkingContextSnapshot {
                        worktree_root: "/worktrees/docs".into(),
                        repository: "docs".into(),
                        branch: "main".into(),
                        observed_at_ms: 15,
                    },
                ],
            },
            AgentInstanceSnapshot {
                id: "completed".into(),
                workspace_id: "workspace".into(),
                shell_id: "shell-b".into(),
                run_id: "run-b".into(),
                name: "completed".into(),
                integration: "opencode".into(),
                external_session_id: Some("external".into()),
                cwd: Some(repository.clone()),
                started_at_ms: 2,
                ended_at_ms: Some(30),
                observation: completed.clone(),
                attention: Some(AgentAttentionSnapshot {
                    reason: AgentAttentionReason::Completed,
                    observation: completed.clone(),
                }),
                working_contexts: vec![
                    AgentWorkingContextSnapshot {
                        worktree_root: "/worktrees/omarchy-boomux".into(),
                        repository: "omarchy-boomux".into(),
                        branch: "feat/session-radar".into(),
                        observed_at_ms: 35,
                    },
                    AgentWorkingContextSnapshot {
                        worktree_root: "/worktrees/boomux".into(),
                        repository: "boomux".into(),
                        branch: "feat/session-radar".into(),
                        observed_at_ms: 30,
                    },
                    AgentWorkingContextSnapshot {
                        worktree_root: "/worktrees/client".into(),
                        repository: "client".into(),
                        branch: "main".into(),
                        observed_at_ms: 10,
                    },
                ],
            },
        ];
        let snapshot = Snapshot {
            workspaces: vec![WorkspaceSnapshot {
                id: "workspace".into(),
                revision: 7,
                name: "work".into(),
                default_cwd: None,
                shells: Vec::new(),
                launchers: Vec::new(),
                agents,
            }],
            focused_terminal: None,
        };
        let occurrence =
            |agent_id: &str, observation: AgentObservationSnapshot| SessionOccurrence {
                agent_id: agent_id.into(),
                shell_id: format!("shell-{agent_id}"),
                run_id: format!("run-{agent_id}"),
                started_at_ms: 1,
                ended_at_ms: None,
                observation,
                is_current: false,
                retained_shell_name: None,
                retained_shell_cwd: None,
                source_cwd: Some(repository.clone()),
            };
        let session = SessionProjection {
            id: "session".into(),
            workspace_id: "workspace".into(),
            workspace_name: "work".into(),
            integration: "opencode".into(),
            external_session_id: Some("external".into()),
            description: "Investigate CI".into(),
            user_display_name: None,
            workspace_revision: 7,
            state: AgentState::Blocked,
            state_is_current: false,
            started_at_ms: 1,
            last_at_ms: 30,
            source_cwd: Some(repository.clone()),
            occurrences: vec![
                occurrence("completed", completed),
                occurrence("blocked", blocked),
            ],
        };

        let summary = session_summary_with_snapshot(&snapshot, &session);

        assert_eq!(summary.latest_agent_name.as_deref(), Some("completed"));
        assert_eq!(summary.git_branch.as_deref(), Some("feat/session-radar"));
        assert_eq!(summary.working_context_count, 5);
        assert_eq!(
            summary
                .working_contexts
                .iter()
                .map(|context| (context.repository.as_str(), context.branch.as_str()))
                .collect::<Vec<_>>(),
            vec![
                ("boomux", "feat/session-radar"),
                ("omarchy-boomux", "feat/session-radar"),
                ("service", "main"),
                ("docs", "main"),
            ]
        );
        assert_eq!(
            summary
                .attentions
                .iter()
                .map(|attention| (
                    attention.agent_id.as_str(),
                    attention.reason,
                    attention.observation_revision,
                ))
                .collect::<Vec<_>>(),
            vec![
                ("blocked", AgentAttentionReason::Blocked, 4),
                ("completed", AgentAttentionReason::Completed, 3),
            ]
        );
        let inspection =
            inspect_projected_session(&snapshot, std::slice::from_ref(&session), "session")
                .unwrap();
        assert_eq!(inspection.summary.working_context_count, 5);
        assert_eq!(inspection.summary.working_contexts.len(), 5);
        assert_eq!(inspection.summary.working_contexts[4].repository, "client");
        std::fs::remove_dir_all(repository).unwrap();
    }
}

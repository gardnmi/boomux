use std::env;
use std::error::Error;
use std::ffi::OsString;
use std::fs::{self, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use clap::ValueEnum;
use serde::Serialize;
use uuid::Uuid;

use boomux::protocol::{AgentAuthority, AgentState, ShellStatus, Snapshot};

pub(crate) const OPENCODE_ASSET: &str = include_str!("../integrations/opencode/boomux.js");
pub(crate) const PI_ASSET: &str = include_str!("../integrations/pi/boomux.js");

const VERSION_PROBE_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_VERSION_OUTPUT_BYTES: u64 = 4096;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, ValueEnum)]
#[serde(rename_all = "lowercase")]
pub(crate) enum IntegrationId {
    Opencode,
    Pi,
}

impl IntegrationId {
    pub(crate) const ALL: [Self; 2] = [Self::Opencode, Self::Pi];

    pub(crate) const fn spec(self) -> &'static IntegrationSpec {
        match self {
            Self::Opencode => &OPENCODE,
            Self::Pi => &PI,
        }
    }
}

#[derive(Debug)]
pub(crate) struct IntegrationSpec {
    pub(crate) id: IntegrationId,
    pub(crate) name: &'static str,
    pub(crate) display_name: &'static str,
    pub(crate) package: &'static str,
    pub(crate) validated_version: &'static str,
    pub(crate) asset_name: &'static str,
    pub(crate) content: &'static str,
    pub(crate) executable: &'static str,
    pub(crate) reload_message: &'static str,
}

const OPENCODE: IntegrationSpec = IntegrationSpec {
    id: IntegrationId::Opencode,
    name: "opencode",
    display_name: "OpenCode",
    package: "opencode-ai",
    validated_version: "1.18.15",
    asset_name: "plugin",
    content: OPENCODE_ASSET,
    executable: "opencode",
    reload_message: "Restart any running OpenCode process to activate the plugin",
};

const PI: IntegrationSpec = IntegrationSpec {
    id: IntegrationId::Pi,
    name: "pi",
    display_name: "Pi",
    package: "@earendil-works/pi-coding-agent",
    validated_version: "0.84.1",
    asset_name: "extension",
    content: PI_ASSET,
    executable: "pi",
    reload_message: "Restart any running Pi process to activate the extension",
};

#[derive(Clone, Debug)]
pub(crate) struct Environment {
    home: Option<OsString>,
    xdg_config_home: Option<OsString>,
    pi_coding_agent_dir: Option<OsString>,
    path: Option<OsString>,
}

impl Environment {
    pub(crate) fn from_process() -> Self {
        Self {
            home: env::var_os("HOME"),
            xdg_config_home: env::var_os("XDG_CONFIG_HOME"),
            pi_coding_agent_dir: env::var_os("PI_CODING_AGENT_DIR"),
            path: env::var_os("PATH"),
        }
    }

    #[cfg(test)]
    fn for_test(
        home: Option<OsString>,
        xdg_config_home: Option<OsString>,
        pi_coding_agent_dir: Option<OsString>,
        path: Option<OsString>,
    ) -> Self {
        Self {
            home,
            xdg_config_home,
            pi_coding_agent_dir,
            path,
        }
    }
}

#[derive(Debug, Serialize)]
pub(crate) struct IntegrationSummary {
    pub(crate) name: &'static str,
    pub(crate) display_name: &'static str,
    pub(crate) package: &'static str,
    pub(crate) validated_version: &'static str,
}

impl From<IntegrationId> for IntegrationSummary {
    fn from(id: IntegrationId) -> Self {
        let spec = id.spec();
        Self {
            name: spec.name,
            display_name: spec.display_name,
            package: spec.package,
            validated_version: spec.validated_version,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum AssetState {
    Missing,
    Current,
    Modified,
    Unavailable,
}

impl AssetState {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Missing => "missing",
            Self::Current => "current",
            Self::Modified => "modified",
            Self::Unavailable => "unavailable",
        }
    }
}

#[derive(Debug, Serialize)]
pub(crate) struct AssetStatus {
    pub(crate) state: AssetState,
    pub(crate) path: Option<String>,
    pub(crate) error: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum HostState {
    NotChecked,
    Missing,
    Available,
    ProbeFailed,
}

impl HostState {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::NotChecked => "not_checked",
            Self::Missing => "missing",
            Self::Available => "available",
            Self::ProbeFailed => "probe_failed",
        }
    }
}

#[derive(Debug, Serialize)]
pub(crate) struct HostStatus {
    pub(crate) state: HostState,
    pub(crate) executable: Option<String>,
    pub(crate) version: Option<String>,
    pub(crate) compatibility: &'static str,
    pub(crate) error: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum RuntimeState {
    NotObservable,
    NotRunning,
    Reporting,
    Untracked,
}

impl RuntimeState {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::NotObservable => "not_observable",
            Self::NotRunning => "not_running",
            Self::Reporting => "reporting",
            Self::Untracked => "untracked",
        }
    }
}

#[derive(Debug, Serialize)]
pub(crate) struct RuntimeStatus {
    pub(crate) state: RuntimeState,
    pub(crate) running_processes: usize,
    pub(crate) tracked_processes: usize,
    pub(crate) untracked_processes: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct VerificationTarget {
    pub(crate) shell_id: String,
    pub(crate) run_id: String,
}

#[derive(Debug)]
pub(crate) enum VerificationCheck {
    Verified {
        workspace_name: String,
        agents: Vec<boomux::protocol::AgentInstanceSnapshot>,
    },
    Pending,
    Missing,
    RunChanged,
}

pub(crate) fn verification_targets(
    snapshot: &Snapshot,
    id: IntegrationId,
    shell_id: Option<&str>,
) -> Vec<VerificationTarget> {
    let executable = id.spec().executable;
    let mut targets = snapshot
        .workspaces
        .iter()
        .flat_map(|workspace| &workspace.shells)
        .filter(|shell| {
            matches!(shell.status, ShellStatus::Running)
                && shell.foreground_process.as_deref() == Some(executable)
                && shell_id.is_none_or(|requested| shell.id == requested)
        })
        .filter_map(|shell| {
            shell.run.as_ref().map(|run| VerificationTarget {
                shell_id: shell.id.clone(),
                run_id: run.id.clone(),
            })
        })
        .collect::<Vec<_>>();
    targets.sort_by(|left, right| left.shell_id.cmp(&right.shell_id));
    targets
}

pub(crate) fn check_verification_target(
    snapshot: &Snapshot,
    id: IntegrationId,
    target: &VerificationTarget,
) -> VerificationCheck {
    let spec = id.spec();
    for workspace in &snapshot.workspaces {
        let Some(shell) = workspace
            .shells
            .iter()
            .find(|shell| shell.id == target.shell_id)
        else {
            continue;
        };
        if !matches!(shell.status, ShellStatus::Running)
            || shell.foreground_process.as_deref() != Some(spec.executable)
        {
            return VerificationCheck::Missing;
        }
        let Some(run) = shell.run.as_ref() else {
            return VerificationCheck::Missing;
        };
        if run.id != target.run_id {
            return VerificationCheck::RunChanged;
        }
        let mut agents = workspace
            .agents
            .iter()
            .filter(|agent| authoritative_agent_matches(agent, spec, shell, run))
            .cloned()
            .collect::<Vec<_>>();
        agents.sort_by(|left, right| left.id.cmp(&right.id));
        return if agents.is_empty() {
            VerificationCheck::Pending
        } else {
            VerificationCheck::Verified {
                workspace_name: workspace.name.clone(),
                agents,
            }
        };
    }
    VerificationCheck::Missing
}

#[derive(Debug, Serialize)]
pub(crate) struct IntegrationStatus {
    pub(crate) name: &'static str,
    pub(crate) display_name: &'static str,
    pub(crate) package: &'static str,
    pub(crate) validated_version: &'static str,
    pub(crate) host: HostStatus,
    pub(crate) asset: AssetStatus,
    pub(crate) runtime: RuntimeStatus,
    pub(crate) recommended_action: RecommendedAction,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum RecommendedAction {
    None,
    Install,
    Replace,
    RestartHost,
    InspectError,
}

pub(crate) fn inspect(
    id: IntegrationId,
    environment: &Environment,
    snapshot: Option<&Snapshot>,
) -> IntegrationStatus {
    inspect_with_host_probe(id, environment, snapshot, true)
}

pub(crate) fn inspect_without_host_probe(
    id: IntegrationId,
    environment: &Environment,
    snapshot: Option<&Snapshot>,
) -> IntegrationStatus {
    inspect_with_host_probe(id, environment, snapshot, false)
}

fn inspect_with_host_probe(
    id: IntegrationId,
    environment: &Environment,
    snapshot: Option<&Snapshot>,
    probe_host: bool,
) -> IntegrationStatus {
    let spec = id.spec();
    let asset = inspect_asset(spec, environment);
    let runtime = inspect_runtime(spec, snapshot);
    let recommended_action = recommended_action(asset.state, runtime.state);
    IntegrationStatus {
        name: spec.name,
        display_name: spec.display_name,
        package: spec.package,
        validated_version: spec.validated_version,
        host: if probe_host {
            inspect_host(spec, environment)
        } else {
            HostStatus {
                state: HostState::NotChecked,
                executable: None,
                version: None,
                compatibility: "unknown",
                error: None,
            }
        },
        asset,
        runtime,
        recommended_action,
    }
}

const fn recommended_action(asset: AssetState, runtime: RuntimeState) -> RecommendedAction {
    match asset {
        AssetState::Missing => RecommendedAction::Install,
        AssetState::Modified => RecommendedAction::Replace,
        AssetState::Unavailable => RecommendedAction::InspectError,
        AssetState::Current if matches!(runtime, RuntimeState::Untracked) => {
            RecommendedAction::RestartHost
        }
        AssetState::Current => RecommendedAction::None,
    }
}

fn inspect_asset(spec: &IntegrationSpec, environment: &Environment) -> AssetStatus {
    let path = match install_target(spec.id, environment) {
        Ok(target) => target.path,
        Err(error) => {
            return AssetStatus {
                state: AssetState::Unavailable,
                path: None,
                error: Some(error.to_string()),
            };
        }
    };
    let path_text = path.display().to_string();
    let result = path
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "install path has no parent"))
        .and_then(validate_existing_directory_chain)
        .and_then(|()| inspect_existing_asset(&path, spec.content));
    match result {
        Ok(ExistingAsset::Missing) => AssetStatus {
            state: AssetState::Missing,
            path: Some(path_text),
            error: None,
        },
        Ok(ExistingAsset::Current) => AssetStatus {
            state: AssetState::Current,
            path: Some(path_text),
            error: None,
        },
        Ok(ExistingAsset::Modified) => AssetStatus {
            state: AssetState::Modified,
            path: Some(path_text),
            error: None,
        },
        Err(error) => AssetStatus {
            state: AssetState::Unavailable,
            path: Some(path_text),
            error: Some(error.to_string()),
        },
    }
}

fn inspect_host(spec: &IntegrationSpec, environment: &Environment) -> HostStatus {
    let Some(executable) = executable_on_path(spec.executable, environment.path.as_deref()) else {
        return HostStatus {
            state: HostState::Missing,
            executable: None,
            version: None,
            compatibility: "unknown",
            error: None,
        };
    };
    let executable_text = executable.display().to_string();
    match probe_version(&executable) {
        Ok(output) => {
            let version = version_token(&output);
            let compatibility = if version.as_deref() == Some(spec.validated_version) {
                "validated"
            } else if version.is_some() {
                "unvalidated"
            } else {
                "unknown"
            };
            HostStatus {
                state: HostState::Available,
                executable: Some(executable_text),
                version,
                compatibility,
                error: None,
            }
        }
        Err(error) => HostStatus {
            state: HostState::ProbeFailed,
            executable: Some(executable_text),
            version: None,
            compatibility: "unknown",
            error: Some(error.to_string()),
        },
    }
}

fn inspect_runtime(spec: &IntegrationSpec, snapshot: Option<&Snapshot>) -> RuntimeStatus {
    let Some(snapshot) = snapshot else {
        return RuntimeStatus {
            state: RuntimeState::NotObservable,
            running_processes: 0,
            tracked_processes: 0,
            untracked_processes: 0,
        };
    };
    let running = snapshot.workspaces.iter().flat_map(|workspace| {
        workspace.shells.iter().filter_map(move |shell| {
            (matches!(shell.status, ShellStatus::Running)
                && shell.foreground_process.as_deref() == Some(spec.executable))
            .then_some((workspace, shell))
        })
    });
    let mut running_processes = 0;
    let mut tracked_processes = 0;
    for (workspace, shell) in running {
        running_processes += 1;
        if shell.run.as_ref().is_some_and(|run| {
            workspace
                .agents
                .iter()
                .any(|agent| authoritative_agent_matches(agent, spec, shell, run))
        }) {
            tracked_processes += 1;
        }
    }
    let untracked_processes = running_processes - tracked_processes;
    let state = if running_processes == 0 {
        RuntimeState::NotRunning
    } else if untracked_processes == 0 {
        RuntimeState::Reporting
    } else {
        RuntimeState::Untracked
    };
    RuntimeStatus {
        state,
        running_processes,
        tracked_processes,
        untracked_processes,
    }
}

fn authoritative_agent_matches(
    agent: &boomux::protocol::AgentInstanceSnapshot,
    spec: &IntegrationSpec,
    shell: &boomux::protocol::ShellSnapshot,
    run: &boomux::protocol::ShellRunSnapshot,
) -> bool {
    agent.integration == spec.name
        && agent.shell_id == shell.id
        && agent.run_id == run.id
        && agent.ended_at_ms.is_none()
        && agent.observation.authority == AgentAuthority::LifecycleIntegration
        && !matches!(
            agent.observation.state,
            AgentState::Inactive | AgentState::Done
        )
}

fn executable_on_path(name: &str, path: Option<&std::ffi::OsStr>) -> Option<PathBuf> {
    env::split_paths(path?).find_map(|directory| {
        let candidate = directory.join(name);
        fs::metadata(&candidate)
            .is_ok_and(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
            .then_some(candidate)
    })
}

fn probe_version(executable: &Path) -> io::Result<String> {
    probe_version_with_timeout(executable, VERSION_PROBE_TIMEOUT)
}

fn probe_version_with_timeout(executable: &Path, timeout: Duration) -> io::Result<String> {
    let mut child = Command::new(executable)
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn()?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| io::Error::other("version probe stdout was unavailable"))?;
    let (output_sender, output_receiver) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut output = Vec::new();
        let result = stdout
            .take(MAX_VERSION_OUTPUT_BYTES + 1)
            .read_to_end(&mut output)
            .map(|_| output);
        let _ = output_sender.send(result);
    });
    let process_group = i32::try_from(child.id())
        .map_err(|_| io::Error::other("version probe process ID exceeded i32"))?;
    let deadline = Instant::now() + timeout;
    let result = (|| {
        let mut status = None;
        let mut output = None;
        loop {
            if status.is_none() {
                status = child.try_wait()?;
            }
            if output.is_none() {
                match output_receiver.try_recv() {
                    Ok(result) => output = Some(result?),
                    Err(std::sync::mpsc::TryRecvError::Empty) => {}
                    Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                        return Err(io::Error::other("version probe reader stopped"));
                    }
                }
            }
            if output
                .as_ref()
                .is_some_and(|output| output.len() > MAX_VERSION_OUTPUT_BYTES as usize)
            {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "version output exceeded 4096 bytes",
                ));
            }
            if let (Some(status), Some(output)) = (status, output.as_ref()) {
                if !status.success() {
                    return Err(io::Error::other(format!(
                        "version probe exited with {status}"
                    )));
                }
                return Ok(String::from_utf8_lossy(output).trim().to_owned());
            }
            if Instant::now() >= deadline {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "version probe timed out",
                ));
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    })();
    terminate_process_group(&mut child, process_group);
    result
}

fn terminate_process_group(child: &mut std::process::Child, process_group: i32) {
    // The child creates this process group immediately before exec.
    unsafe {
        libc::kill(-process_group, libc::SIGKILL);
    }
    let deadline = Instant::now() + Duration::from_millis(100);
    while Instant::now() < deadline {
        match child.try_wait() {
            Ok(Some(_)) | Err(_) => return,
            Ok(None) => std::thread::sleep(Duration::from_millis(1)),
        }
    }
}

fn version_token(output: &str) -> Option<String> {
    output.split_whitespace().find_map(|token| {
        let token = token
            .trim_matches(|character: char| !character.is_ascii_alphanumeric() && character != '.');
        let token = token.strip_prefix('v').unwrap_or(token);
        (token
            .chars()
            .next()
            .is_some_and(|character| character.is_ascii_digit())
            && token.contains('.')
            && token
                .chars()
                .all(|character| character.is_ascii_alphanumeric() || ".+-_".contains(character)))
        .then(|| token.to_owned())
    })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum InstallOutcome {
    Installed,
    Replaced,
    Unchanged,
}

#[derive(Debug, Serialize)]
pub(crate) struct InstallResult {
    #[serde(skip)]
    pub(crate) integration: IntegrationId,
    pub(crate) name: &'static str,
    pub(crate) result: InstallOutcome,
    pub(crate) path: String,
    pub(crate) restart_required: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum InstallAction {
    Install,
    Replace,
    Unchanged,
}

#[derive(Debug, Serialize)]
pub(crate) struct InstallPlan {
    #[serde(skip)]
    pub(crate) integration: IntegrationId,
    pub(crate) name: &'static str,
    pub(crate) current_state: AssetState,
    pub(crate) action: InstallAction,
    pub(crate) path: String,
    pub(crate) restart_required: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum UninstallOutcome {
    Removed,
    NotInstalled,
}

#[derive(Debug, Serialize)]
pub(crate) struct UninstallResult {
    #[serde(skip)]
    pub(crate) integration: IntegrationId,
    pub(crate) name: &'static str,
    pub(crate) result: UninstallOutcome,
    pub(crate) path: String,
    pub(crate) restart_required: bool,
}

pub(crate) fn install(
    id: IntegrationId,
    environment: &Environment,
    force: bool,
) -> Result<InstallResult, Box<dyn Error>> {
    let spec = id.spec();
    let target = install_target(id, environment)?;
    let directory = ensure_safe_directory(&target.directory)?;
    let result = install_asset_at(&directory, &target.path, spec.content, force)?;
    Ok(InstallResult {
        integration: id,
        name: spec.name,
        result,
        path: target.path.display().to_string(),
        restart_required: result != InstallOutcome::Unchanged,
    })
}

pub(crate) fn plan_install(
    id: IntegrationId,
    environment: &Environment,
    force: bool,
) -> Result<InstallPlan, Box<dyn Error>> {
    let spec = id.spec();
    let target = install_target(id, environment)?;
    validate_existing_directory_chain(&target.directory)?;
    let existing = inspect_existing_asset(&target.path, spec.content)?;
    let (current_state, action) = match existing {
        ExistingAsset::Missing => (AssetState::Missing, InstallAction::Install),
        ExistingAsset::Current => (AssetState::Current, InstallAction::Unchanged),
        ExistingAsset::Modified if !force => return Err(existing_asset_error(&target.path).into()),
        ExistingAsset::Modified => (AssetState::Modified, InstallAction::Replace),
    };
    Ok(InstallPlan {
        integration: id,
        name: spec.name,
        current_state,
        action,
        path: target.path.display().to_string(),
        restart_required: action != InstallAction::Unchanged,
    })
}

pub(crate) fn preflight_uninstall(
    id: IntegrationId,
    environment: &Environment,
    force: bool,
) -> Result<(), Box<dyn Error>> {
    let spec = id.spec();
    let target = install_target(id, environment)?;
    validate_existing_directory_chain(&target.directory)?;
    if inspect_existing_asset(&target.path, spec.content)? == ExistingAsset::Modified && !force {
        return Err(modified_uninstall_error(&target.path).into());
    }
    Ok(())
}

pub(crate) fn uninstall(
    id: IntegrationId,
    environment: &Environment,
    force: bool,
) -> Result<UninstallResult, Box<dyn Error>> {
    let spec = id.spec();
    let target = install_target(id, environment)?;
    validate_existing_directory_chain(&target.directory)?;
    let existing = inspect_existing_asset(&target.path, spec.content)?;
    let result = match existing {
        ExistingAsset::Missing => UninstallOutcome::NotInstalled,
        ExistingAsset::Modified if !force => {
            return Err(modified_uninstall_error(&target.path).into());
        }
        ExistingAsset::Current | ExistingAsset::Modified => {
            fs::remove_file(&target.path)?;
            UninstallOutcome::Removed
        }
    };
    Ok(UninstallResult {
        integration: id,
        name: spec.name,
        result,
        path: target.path.display().to_string(),
        restart_required: result == UninstallOutcome::Removed,
    })
}

#[cfg(test)]
pub(crate) fn install_at(
    id: IntegrationId,
    config_root: &Path,
    force: bool,
) -> Result<InstallResult, Box<dyn Error>> {
    require_absolute_root(config_root, config_root_name(id))?;
    let spec = id.spec();
    let target = target_at(id, config_root);
    let directory = ensure_safe_directory(&target.directory)?;
    let result = install_asset_at(&directory, &target.path, spec.content, force)?;
    Ok(InstallResult {
        integration: id,
        name: spec.name,
        result,
        path: target.path.display().to_string(),
        restart_required: result != InstallOutcome::Unchanged,
    })
}

#[cfg(test)]
pub(crate) fn install_path_at(id: IntegrationId, config_root: &Path) -> PathBuf {
    target_at(id, config_root).path
}

struct InstallTarget {
    directory: PathBuf,
    path: PathBuf,
}

fn install_target(
    id: IntegrationId,
    environment: &Environment,
) -> Result<InstallTarget, Box<dyn Error>> {
    let root = config_root(id, environment)?;
    Ok(target_at(id, &root))
}

fn config_root(id: IntegrationId, environment: &Environment) -> Result<PathBuf, Box<dyn Error>> {
    match id {
        IntegrationId::Opencode => opencode_config_root(
            environment.xdg_config_home.clone(),
            environment.home.clone(),
        ),
        IntegrationId::Pi => pi_config_root(
            environment.pi_coding_agent_dir.clone(),
            environment.home.clone(),
        ),
    }
}

fn target_at(id: IntegrationId, config_root: &Path) -> InstallTarget {
    match id {
        IntegrationId::Opencode => InstallTarget {
            directory: config_root.join("opencode/plugins"),
            path: config_root.join("opencode/plugins/boomux.js"),
        },
        IntegrationId::Pi => InstallTarget {
            directory: config_root.join("extensions"),
            path: config_root.join("extensions/boomux.js"),
        },
    }
}

#[cfg(test)]
const fn config_root_name(id: IntegrationId) -> &'static str {
    match id {
        IntegrationId::Opencode => "XDG configuration root",
        IntegrationId::Pi => "Pi configuration root",
    }
}

pub(crate) fn opencode_config_root(
    xdg_config_home: Option<OsString>,
    home: Option<OsString>,
) -> Result<PathBuf, Box<dyn Error>> {
    let root = match xdg_config_home.filter(|value| !value.is_empty()) {
        Some(root) => PathBuf::from(root),
        None => PathBuf::from(home.ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "HOME must be set to install the Boomux OpenCode plugin",
            )
        })?)
        .join(".config"),
    };
    require_absolute_root(&root, "XDG configuration root")?;
    Ok(root)
}

pub(crate) fn pi_config_root(
    pi_coding_agent_dir: Option<OsString>,
    home: Option<OsString>,
) -> Result<PathBuf, Box<dyn Error>> {
    let home = || -> Result<PathBuf, Box<dyn Error>> {
        home.clone().map(PathBuf::from).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "HOME must be set to install the Boomux Pi extension",
            )
            .into()
        })
    };
    let root = match pi_coding_agent_dir.filter(|value| !value.is_empty()) {
        Some(root) => {
            let root = PathBuf::from(root);
            if let Ok(suffix) = root.strip_prefix("~") {
                home()?.join(suffix)
            } else {
                root
            }
        }
        None => home()?.join(".pi/agent"),
    };
    require_absolute_root(&root, "Pi configuration root")?;
    Ok(root)
}

pub(crate) fn require_absolute_root(root: &Path, name: &str) -> Result<(), Box<dyn Error>> {
    if !root.is_absolute() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{name} must be an absolute path"),
        )
        .into());
    }
    Ok(())
}

pub(crate) fn ensure_safe_directory(path: &Path) -> Result<PathBuf, Box<dyn Error>> {
    require_absolute_root(path, "install directory")?;
    let mut directory = PathBuf::new();
    for component in path.components() {
        directory.push(component);
        match fs::symlink_metadata(&directory) {
            Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {}
            Ok(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!(
                        "install path component is not a regular directory: {}",
                        directory.display()
                    ),
                )
                .into());
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                fs::create_dir(&directory)?;
            }
            Err(error) => return Err(error.into()),
        }
    }
    Ok(directory)
}

fn validate_existing_directory_chain(path: &Path) -> io::Result<()> {
    require_absolute_root(path, "install directory")
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error.to_string()))?;
    let mut directory = PathBuf::new();
    for component in path.components() {
        directory.push(component);
        match fs::symlink_metadata(&directory) {
            Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {}
            Ok(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!(
                        "install path component is not a regular directory: {}",
                        directory.display()
                    ),
                ));
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ExistingAsset {
    Missing,
    Current,
    Modified,
}

pub(crate) fn regular_file_matches(path: &Path, expected: &str) -> io::Result<Option<bool>> {
    match inspect_existing_asset(path, expected)? {
        ExistingAsset::Missing => Ok(None),
        ExistingAsset::Current => Ok(Some(true)),
        ExistingAsset::Modified => Ok(Some(false)),
    }
}

fn inspect_existing_asset(path: &Path, expected: &str) -> io::Result<ExistingAsset> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_file() && !metadata.file_type().is_symlink() => metadata,
        Ok(_) => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("install path is not a regular file: {}", path.display()),
            ));
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Ok(ExistingAsset::Missing);
        }
        Err(error) => return Err(error),
    };
    if metadata.len() != expected.len() as u64 {
        return Ok(ExistingAsset::Modified);
    }
    let mut contents = Vec::with_capacity(expected.len() + 1);
    fs::File::open(path)?
        .take(expected.len() as u64 + 1)
        .read_to_end(&mut contents)?;
    Ok(if contents == expected.as_bytes() {
        ExistingAsset::Current
    } else {
        ExistingAsset::Modified
    })
}

pub(crate) fn install_asset_at(
    directory: &Path,
    path: &Path,
    content: &str,
    force: bool,
) -> Result<InstallOutcome, Box<dyn Error>> {
    let existing = inspect_existing_asset(path, content)?;
    let outcome = match existing {
        ExistingAsset::Current => return Ok(InstallOutcome::Unchanged),
        ExistingAsset::Modified if !force => return Err(existing_asset_error(path).into()),
        ExistingAsset::Modified => InstallOutcome::Replaced,
        ExistingAsset::Missing => InstallOutcome::Installed,
    };
    write_asset_atomically(directory, path, content)?;
    Ok(outcome)
}

fn existing_asset_error(path: &Path) -> io::Error {
    io::Error::new(
        io::ErrorKind::AlreadyExists,
        format!(
            "{} already exists; rerun with --force to replace it",
            path.display()
        ),
    )
}

fn modified_uninstall_error(path: &Path) -> io::Error {
    io::Error::new(
        io::ErrorKind::AlreadyExists,
        format!(
            "{} contains modified content; rerun with --force to remove it",
            path.display()
        ),
    )
}

fn write_asset_atomically(directory: &Path, path: &Path, content: &str) -> io::Result<()> {
    let temporary = directory.join(format!(".boomux-install-{}.tmp", Uuid::new_v4()));
    let result = (|| {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)?;
        file.write_all(content.as_bytes())?;
        file.sync_all()?;
        fs::rename(&temporary, path)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

#[cfg(test)]
mod tests {
    use std::time::{SystemTime, UNIX_EPOCH};

    use boomux::protocol::{
        AgentAuthority, AgentInstanceSnapshot, AgentObservationSnapshot, ShellRunSnapshot,
        ShellSnapshot, WorkspaceSnapshot,
    };

    use super::*;

    struct TestDirectory(PathBuf);

    impl TestDirectory {
        fn new(name: &str) -> Self {
            let nonce = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("clock after epoch")
                .as_nanos();
            let path = env::temp_dir().join(format!(
                "boomux-integration-{}-{name}-{nonce}",
                std::process::id()
            ));
            fs::create_dir(&path).expect("create test directory");
            Self(path)
        }

        fn executable(&self, name: &str, contents: &str) -> PathBuf {
            let path = self.0.join(name);
            fs::write(&path, contents).expect("write executable");
            fs::set_permissions(&path, fs::Permissions::from_mode(0o755))
                .expect("set executable mode");
            path
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn environment(home: &Path) -> Environment {
        Environment::for_test(
            Some(home.as_os_str().to_owned()),
            None,
            None,
            Some(OsString::new()),
        )
    }

    #[test]
    fn descriptors_have_unique_names_and_expected_metadata() {
        assert_eq!(IntegrationId::Opencode.spec().package, "opencode-ai");
        assert_eq!(IntegrationId::Pi.spec().validated_version, "0.84.1");
        assert_ne!(
            IntegrationId::Opencode.spec().name,
            IntegrationId::Pi.spec().name
        );
    }

    #[test]
    fn resolves_host_configuration_roots() {
        assert_eq!(
            opencode_config_root(Some("/xdg".into()), Some("/home/example".into())).unwrap(),
            Path::new("/xdg")
        );
        assert_eq!(
            opencode_config_root(Some(OsString::new()), Some("/home/example".into())).unwrap(),
            Path::new("/home/example/.config")
        );
        assert_eq!(
            pi_config_root(Some("~/.config/pi".into()), Some("/home/example".into())).unwrap(),
            Path::new("/home/example/.config/pi")
        );
        assert!(pi_config_root(Some("relative".into()), None).is_err());
    }

    #[test]
    fn status_distinguishes_missing_current_and_modified_assets() {
        let home = TestDirectory::new("asset-status");
        let environment = environment(&home.0);
        let missing = inspect(IntegrationId::Opencode, &environment, None);
        assert_eq!(missing.asset.state, AssetState::Missing);
        assert_eq!(missing.recommended_action, RecommendedAction::Install);
        assert!(!home.0.join(".config").exists());

        install(IntegrationId::Opencode, &environment, false).unwrap();
        let current = inspect(IntegrationId::Opencode, &environment, None);
        assert_eq!(current.asset.state, AssetState::Current);
        assert_eq!(current.recommended_action, RecommendedAction::None);

        fs::write(
            install_path_at(IntegrationId::Opencode, &home.0.join(".config")),
            "custom",
        )
        .unwrap();
        let modified = inspect(IntegrationId::Opencode, &environment, None);
        assert_eq!(modified.asset.state, AssetState::Modified);
        assert_eq!(modified.recommended_action, RecommendedAction::Replace);

        assert_eq!(
            recommended_action(AssetState::Current, RuntimeState::Untracked),
            RecommendedAction::RestartHost
        );
    }

    #[test]
    fn unified_install_is_idempotent_and_requires_force() {
        let home = TestDirectory::new("install");
        let environment = environment(&home.0);
        let installed = install(IntegrationId::Pi, &environment, false).unwrap();
        assert_eq!(installed.result, InstallOutcome::Installed);
        assert_eq!(
            install(IntegrationId::Pi, &environment, false)
                .unwrap()
                .result,
            InstallOutcome::Unchanged
        );
        fs::write(&installed.path, "custom").unwrap();
        assert!(install(IntegrationId::Pi, &environment, false).is_err());
        assert_eq!(
            install(IntegrationId::Pi, &environment, true)
                .unwrap()
                .result,
            InstallOutcome::Replaced
        );
    }

    #[test]
    fn install_plan_reports_actions_without_mutating_the_target() {
        let home = TestDirectory::new("install-plan");
        let environment = environment(&home.0);

        let missing = plan_install(IntegrationId::Opencode, &environment, false).unwrap();
        assert_eq!(missing.current_state, AssetState::Missing);
        assert_eq!(missing.action, InstallAction::Install);
        assert!(missing.restart_required);
        assert!(!home.0.join(".config").exists());

        let installed = install(IntegrationId::Opencode, &environment, false).unwrap();
        let current = plan_install(IntegrationId::Opencode, &environment, false).unwrap();
        assert_eq!(current.current_state, AssetState::Current);
        assert_eq!(current.action, InstallAction::Unchanged);
        assert!(!current.restart_required);

        fs::write(&installed.path, "custom").unwrap();
        assert!(plan_install(IntegrationId::Opencode, &environment, false).is_err());
        let replacement = plan_install(IntegrationId::Opencode, &environment, true).unwrap();
        assert_eq!(replacement.current_state, AssetState::Modified);
        assert_eq!(replacement.action, InstallAction::Replace);
        assert_eq!(fs::read_to_string(installed.path).unwrap(), "custom");
    }

    #[test]
    fn uninstall_is_idempotent_and_protects_modified_assets() {
        let home = TestDirectory::new("uninstall");
        let environment = environment(&home.0);
        let installed = install(IntegrationId::Pi, &environment, false).unwrap();
        let directory = Path::new(&installed.path).parent().unwrap().to_owned();

        let removed = uninstall(IntegrationId::Pi, &environment, false).unwrap();
        assert_eq!(removed.result, UninstallOutcome::Removed);
        assert!(removed.restart_required);
        assert!(!Path::new(&removed.path).exists());
        assert!(directory.is_dir());

        let missing = uninstall(IntegrationId::Pi, &environment, false).unwrap();
        assert_eq!(missing.result, UninstallOutcome::NotInstalled);
        assert!(!missing.restart_required);

        install(IntegrationId::Pi, &environment, false).unwrap();
        fs::write(&installed.path, "custom").unwrap();
        assert!(preflight_uninstall(IntegrationId::Pi, &environment, false).is_err());
        assert!(uninstall(IntegrationId::Pi, &environment, false).is_err());
        assert_eq!(fs::read_to_string(&installed.path).unwrap(), "custom");
        assert_eq!(
            uninstall(IntegrationId::Pi, &environment, true)
                .unwrap()
                .result,
            UninstallOutcome::Removed
        );
    }

    #[test]
    fn version_parser_accepts_common_host_output() {
        assert_eq!(version_token("1.18.15\n").as_deref(), Some("1.18.15"));
        assert_eq!(
            version_token("opencode version v1.18.15").as_deref(),
            Some("1.18.15")
        );
        assert_eq!(version_token("unknown"), None);
    }

    #[test]
    fn version_probe_bounds_output_and_runtime() {
        let directory = TestDirectory::new("version-probe");
        let oversized = directory.executable(
            "oversized",
            "#!/bin/sh\ni=0; while [ $i -lt 5000 ]; do printf x; i=$((i + 1)); done\n",
        );
        assert_eq!(
            probe_version_with_timeout(&oversized, Duration::from_secs(1))
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidData
        );

        let hanging = directory.executable("hanging", "#!/bin/sh\nsleep 10\n");
        let started = Instant::now();
        assert_eq!(
            probe_version_with_timeout(&hanging, Duration::from_millis(50))
                .unwrap_err()
                .kind(),
            io::ErrorKind::TimedOut
        );
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn runtime_status_requires_exact_current_run_registration() {
        let shell = ShellSnapshot {
            id: "s1".into(),
            workspace_id: "w1".into(),
            name: "shell".into(),
            cwd: "/repo".into(),
            command: Vec::new(),
            status: ShellStatus::Running,
            run: Some(ShellRunSnapshot {
                id: "r1".into(),
                generation: 1,
                started_at_ms: 1,
                ended_at_ms: None,
                exit_reason: None,
                output_revision: 0,
                environment_has_run_id: true,
            }),
            foreground_process: Some("opencode".into()),
        };
        let mut workspace = WorkspaceSnapshot {
            id: "w1".into(),
            name: "workspace".into(),
            default_cwd: None,
            shells: vec![shell],
            launchers: Vec::new(),
            agents: Vec::new(),
        };
        let snapshot = Snapshot {
            workspaces: vec![workspace.clone()],
            focused_terminal: None,
        };
        assert_eq!(
            inspect_runtime(IntegrationId::Opencode.spec(), Some(&snapshot)).state,
            RuntimeState::Untracked
        );

        workspace.agents.push(AgentInstanceSnapshot {
            id: "a1".into(),
            workspace_id: "w1".into(),
            shell_id: "s1".into(),
            run_id: "r1".into(),
            name: "OpenCode".into(),
            integration: "opencode".into(),
            external_session_id: Some("root".into()),
            cwd: Some("/repo".into()),
            started_at_ms: 1,
            ended_at_ms: None,
            attention: None,
            observation: AgentObservationSnapshot {
                revision: 1,
                state: AgentState::Working,
                authority: AgentAuthority::LifecycleIntegration,
                evidence: "working".into(),
                confidence: 100,
                observed_at_ms: 1,
            },
        });
        let snapshot = Snapshot {
            workspaces: vec![workspace.clone()],
            focused_terminal: None,
        };
        assert_eq!(
            verification_targets(&snapshot, IntegrationId::Opencode, None),
            [VerificationTarget {
                shell_id: "s1".into(),
                run_id: "r1".into(),
            }]
        );
        assert_eq!(
            inspect_runtime(IntegrationId::Opencode.spec(), Some(&snapshot)).state,
            RuntimeState::Reporting
        );
        assert!(matches!(
            check_verification_target(
                &snapshot,
                IntegrationId::Opencode,
                &VerificationTarget {
                    shell_id: "s1".into(),
                    run_id: "r1".into(),
                }
            ),
            VerificationCheck::Verified { agents, .. } if agents.len() == 1
        ));

        workspace.agents[0].observation.authority = AgentAuthority::ProcessAdapter;
        let snapshot = Snapshot {
            workspaces: vec![workspace],
            focused_terminal: None,
        };
        assert_eq!(
            inspect_runtime(IntegrationId::Opencode.spec(), Some(&snapshot)).state,
            RuntimeState::Untracked
        );
        assert!(matches!(
            check_verification_target(
                &snapshot,
                IntegrationId::Opencode,
                &VerificationTarget {
                    shell_id: "s1".into(),
                    run_id: "r1".into(),
                }
            ),
            VerificationCheck::Pending
        ));
    }
}

use std::env;
use std::error::Error;
use std::ffi::OsString;
use std::fs::{self, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::str::FromStr;
use std::time::{Duration, Instant};

use serde::{Serialize, Serializer};
use serde_json::{Map, Value};
use uuid::Uuid;

use boomux::integrations::{InstallTargetKind, InstallationCapability, IntegrationDescriptor};
use boomux::protocol::{AgentAuthority, ShellStatus, Snapshot};

const VERSION_PROBE_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_VERSION_OUTPUT_BYTES: u64 = 4096;
const MAX_CODEX_HOOKS_BYTES: u64 = 1024 * 1024;
const CODEX_HOOK_COMMAND: &str = "boomux codex hook";
const CODEX_HOOK_EVENTS: &[&str] = &[
    "SessionStart",
    "SessionEnd",
    "UserPromptSubmit",
    "PreToolUse",
    "PermissionRequest",
    "PostToolUse",
    "PreCompact",
    "PostCompact",
    "SubagentStart",
    "SubagentStop",
    "Stop",
];

#[cfg(test)]
pub(crate) const OPENCODE_ASSET: &str = boomux::integrations::OPENCODE
    .installation
    .as_ref()
    .expect("OpenCode installation capability")
    .content;
#[cfg(test)]
pub(crate) const PI_ASSET: &str = boomux::integrations::PI
    .installation
    .as_ref()
    .expect("Pi installation capability")
    .content;
#[cfg(test)]
pub(crate) const CLAUDE_ASSET: &str = boomux::integrations::CLAUDE
    .installation
    .as_ref()
    .expect("Claude installation capability")
    .content;
#[cfg(test)]
pub(crate) const CODEX_ASSET: &str = boomux::integrations::CODEX
    .installation
    .as_ref()
    .expect("Codex installation capability")
    .content;
#[cfg(test)]
pub(crate) const KIRO_ASSET: &str = boomux::integrations::KIRO
    .installation
    .as_ref()
    .expect("Kiro installation capability")
    .content;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct IntegrationId(&'static IntegrationDescriptor);

impl IntegrationId {
    pub(crate) const OPENCODE: Self = Self(&boomux::integrations::OPENCODE);
    pub(crate) const PI: Self = Self(&boomux::integrations::PI);
    pub(crate) const CODEX: Self = Self(&boomux::integrations::CODEX);
    pub(crate) const KIRO: Self = Self(&boomux::integrations::KIRO);
    #[allow(non_upper_case_globals)]
    pub(crate) const Opencode: Self = Self::OPENCODE;
    #[allow(non_upper_case_globals)]
    pub(crate) const Pi: Self = Self::PI;
    #[cfg(test)]
    #[allow(non_upper_case_globals)]
    pub(crate) const Codex: Self = Self::CODEX;
    #[cfg(test)]
    #[allow(non_upper_case_globals)]
    pub(crate) const Kiro: Self = Self::KIRO;
    #[cfg(test)]
    #[allow(non_upper_case_globals)]
    pub(crate) const Claude: Self = Self(&boomux::integrations::CLAUDE);

    pub(crate) const fn spec(self) -> &'static IntegrationDescriptor {
        self.0
    }

    pub(crate) const fn installation(self) -> &'static InstallationCapability {
        match self.spec().installation.as_ref() {
            Some(installation) => installation,
            None => panic!("CLI integration must support installation"),
        }
    }

    pub(crate) fn all() -> impl Iterator<Item = Self> {
        boomux::integrations::installable().map(Self)
    }
}

impl FromStr for IntegrationId {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        boomux::integrations::by_key(value)
            .filter(|descriptor| descriptor.installation.is_some())
            .map(Self)
            .ok_or_else(|| format!("unknown installable integration: {value}"))
    }
}

impl Serialize for IntegrationId {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(self.spec().key)
    }
}

#[derive(Clone, Debug)]
pub(crate) struct Environment {
    home: Option<OsString>,
    xdg_config_home: Option<OsString>,
    pi_coding_agent_dir: Option<OsString>,
    claude_config_dir: Option<OsString>,
    codex_home: Option<OsString>,
    kiro_home: Option<OsString>,
    path: Option<OsString>,
}

impl Environment {
    pub(crate) fn from_process() -> Self {
        Self {
            home: env::var_os("HOME"),
            xdg_config_home: env::var_os("XDG_CONFIG_HOME"),
            pi_coding_agent_dir: env::var_os("PI_CODING_AGENT_DIR"),
            claude_config_dir: env::var_os("CLAUDE_CONFIG_DIR"),
            codex_home: env::var_os("CODEX_HOME"),
            kiro_home: env::var_os("KIRO_HOME"),
            path: env::var_os("PATH"),
        }
    }

    #[cfg(test)]
    fn for_test(
        home: Option<OsString>,
        xdg_config_home: Option<OsString>,
        pi_coding_agent_dir: Option<OsString>,
        claude_config_dir: Option<OsString>,
        path: Option<OsString>,
    ) -> Self {
        Self {
            home,
            xdg_config_home,
            pi_coding_agent_dir,
            claude_config_dir,
            codex_home: None,
            kiro_home: None,
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
        let installation = id.installation();
        Self {
            name: spec.key,
            display_name: spec.display_name,
            package: installation.package,
            validated_version: installation.validated_version,
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
    let executable = id
        .spec()
        .foreground
        .expect("CLI integration must recognize its foreground process")
        .process_name;
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
    let descriptor = id.spec();
    let foreground = descriptor
        .foreground
        .expect("CLI integration must recognize its foreground process");
    for workspace in &snapshot.workspaces {
        let Some(shell) = workspace
            .shells
            .iter()
            .find(|shell| shell.id == target.shell_id)
        else {
            continue;
        };
        if !matches!(shell.status, ShellStatus::Running)
            || shell.foreground_process.as_deref() != Some(foreground.process_name)
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
            .filter(|agent| authoritative_agent_matches(agent, descriptor, shell, run))
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
    let descriptor = id.spec();
    let installation = id.installation();
    let asset = inspect_asset(id, installation, environment);
    let runtime = inspect_runtime(descriptor, snapshot);
    let recommended_action = recommended_action(asset.state, runtime.state);
    IntegrationStatus {
        name: descriptor.key,
        display_name: descriptor.display_name,
        package: installation.package,
        validated_version: installation.validated_version,
        host: if probe_host {
            inspect_host(installation, environment)
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

fn inspect_asset(
    id: IntegrationId,
    installation: &InstallationCapability,
    environment: &Environment,
) -> AssetStatus {
    let path = match install_target(id, environment) {
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
        .and_then(|()| {
            if id == IntegrationId::CODEX {
                inspect_codex_hooks(&path)
            } else {
                inspect_existing_asset(&path, installation.content)
            }
        });
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

fn inspect_host(installation: &InstallationCapability, environment: &Environment) -> HostStatus {
    let Some(executable) = executable_on_path(installation.executable, environment.path.as_deref())
    else {
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
            let compatibility = if version.as_deref() == Some(installation.validated_version) {
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

fn inspect_runtime(
    descriptor: &IntegrationDescriptor,
    snapshot: Option<&Snapshot>,
) -> RuntimeStatus {
    let Some(snapshot) = snapshot else {
        return RuntimeStatus {
            state: RuntimeState::NotObservable,
            running_processes: 0,
            tracked_processes: 0,
            untracked_processes: 0,
        };
    };
    let Some(foreground) = descriptor.foreground else {
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
                && shell.foreground_process.as_deref() == Some(foreground.process_name))
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
                .any(|agent| authoritative_agent_matches(agent, descriptor, shell, run))
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
    descriptor: &IntegrationDescriptor,
    shell: &boomux::protocol::ShellSnapshot,
    run: &boomux::protocol::ShellRunSnapshot,
) -> bool {
    agent.integration == descriptor.key
        && agent.observation.authority == AgentAuthority::LifecycleIntegration
        && crate::session_projection::agent_is_active_for_run(agent, &shell.id, &run.id)
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
    let deadline = Instant::now() + timeout;
    let mut child = loop {
        let result = Command::new(executable)
            .arg("--version")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .process_group(0)
            .spawn();
        match result {
            Ok(child) => break child,
            Err(error)
                if error.kind() == io::ErrorKind::ExecutableFileBusy
                    && Instant::now() < deadline =>
            {
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(error) => return Err(error),
        }
    };
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
    let descriptor = id.spec();
    let installation = id.installation();
    let target = install_target(id, environment)?;
    let directory = ensure_safe_directory(&target.directory)?;
    let result = if id == IntegrationId::CODEX {
        install_codex_hooks(&directory, &target.path, force)?
    } else {
        install_asset_at(&directory, &target.path, installation.content, force)?
    };
    Ok(InstallResult {
        integration: id,
        name: descriptor.key,
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
    let descriptor = id.spec();
    let installation = id.installation();
    let target = install_target(id, environment)?;
    validate_existing_directory_chain(&target.directory)?;
    let existing = if id == IntegrationId::CODEX {
        inspect_codex_hooks(&target.path)?
    } else {
        inspect_existing_asset(&target.path, installation.content)?
    };
    let (current_state, action) = match existing {
        ExistingAsset::Missing => (AssetState::Missing, InstallAction::Install),
        ExistingAsset::Current => (AssetState::Current, InstallAction::Unchanged),
        ExistingAsset::Modified if !force => return Err(existing_asset_error(&target.path).into()),
        ExistingAsset::Modified => (AssetState::Modified, InstallAction::Replace),
    };
    Ok(InstallPlan {
        integration: id,
        name: descriptor.key,
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
    let installation = id.installation();
    let target = install_target(id, environment)?;
    validate_existing_directory_chain(&target.directory)?;
    let existing = if id == IntegrationId::CODEX {
        inspect_codex_hooks(&target.path)?
    } else {
        inspect_existing_asset(&target.path, installation.content)?
    };
    if existing == ExistingAsset::Modified && !force {
        return Err(modified_uninstall_error(&target.path).into());
    }
    Ok(())
}

pub(crate) fn uninstall(
    id: IntegrationId,
    environment: &Environment,
    force: bool,
) -> Result<UninstallResult, Box<dyn Error>> {
    let descriptor = id.spec();
    let installation = id.installation();
    let target = install_target(id, environment)?;
    validate_existing_directory_chain(&target.directory)?;
    let existing = if id == IntegrationId::CODEX {
        inspect_codex_hooks(&target.path)?
    } else {
        inspect_existing_asset(&target.path, installation.content)?
    };
    let result = match existing {
        ExistingAsset::Missing => UninstallOutcome::NotInstalled,
        ExistingAsset::Modified if !force => {
            return Err(modified_uninstall_error(&target.path).into());
        }
        ExistingAsset::Current | ExistingAsset::Modified if id == IntegrationId::CODEX => {
            uninstall_codex_hooks(&target.directory, &target.path)?;
            UninstallOutcome::Removed
        }
        ExistingAsset::Current | ExistingAsset::Modified => {
            fs::remove_file(&target.path)?;
            UninstallOutcome::Removed
        }
    };
    Ok(UninstallResult {
        integration: id,
        name: descriptor.key,
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
    let descriptor = id.spec();
    let installation = id.installation();
    let target = target_at(id, config_root);
    let directory = ensure_safe_directory(&target.directory)?;
    let result = if id == IntegrationId::CODEX {
        install_codex_hooks(&directory, &target.path, force)?
    } else {
        install_asset_at(&directory, &target.path, installation.content, force)?
    };
    Ok(InstallResult {
        integration: id,
        name: descriptor.key,
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
    match id.installation().target {
        InstallTargetKind::OpenCode => opencode_config_root(
            environment.xdg_config_home.clone(),
            environment.home.clone(),
        ),
        InstallTargetKind::Pi => pi_config_root(
            environment.pi_coding_agent_dir.clone(),
            environment.home.clone(),
        ),
        InstallTargetKind::Claude => claude_config_root(
            environment.claude_config_dir.clone(),
            environment.home.clone(),
        ),
        InstallTargetKind::Codex => {
            codex_config_root(environment.codex_home.clone(), environment.home.clone())
        }
        InstallTargetKind::Kiro => {
            kiro_config_root(environment.kiro_home.clone(), environment.home.clone())
        }
    }
}

fn target_at(id: IntegrationId, config_root: &Path) -> InstallTarget {
    match id.installation().target {
        InstallTargetKind::OpenCode => InstallTarget {
            directory: config_root.join("opencode/plugins"),
            path: config_root.join("opencode/plugins/boomux.js"),
        },
        InstallTargetKind::Pi => InstallTarget {
            directory: config_root.join("extensions"),
            path: config_root.join("extensions/boomux.js"),
        },
        InstallTargetKind::Claude => InstallTarget {
            directory: config_root.join("skills/boomux/.claude-plugin"),
            path: config_root.join("skills/boomux/.claude-plugin/plugin.json"),
        },
        InstallTargetKind::Codex => InstallTarget {
            directory: config_root.to_owned(),
            path: config_root.join("hooks.json"),
        },
        InstallTargetKind::Kiro => InstallTarget {
            directory: config_root.join("hooks"),
            path: config_root.join("hooks/boomux.json"),
        },
    }
}

#[cfg(test)]
const fn config_root_name(id: IntegrationId) -> &'static str {
    match id.installation().target {
        InstallTargetKind::OpenCode => "XDG configuration root",
        InstallTargetKind::Pi => "Pi configuration root",
        InstallTargetKind::Claude => "Claude configuration root",
        InstallTargetKind::Codex => "Codex configuration root",
        InstallTargetKind::Kiro => "Kiro configuration root",
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

pub(crate) fn claude_config_root(
    claude_config_dir: Option<OsString>,
    home: Option<OsString>,
) -> Result<PathBuf, Box<dyn Error>> {
    let root = match claude_config_dir.filter(|value| !value.is_empty()) {
        Some(root) => PathBuf::from(root),
        None => PathBuf::from(home.ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "HOME must be set to install the Boomux Claude Code plugin",
            )
        })?)
        .join(".claude"),
    };
    require_absolute_root(&root, "Claude configuration root")?;
    Ok(root)
}

pub(crate) fn codex_config_root(
    codex_home: Option<OsString>,
    home: Option<OsString>,
) -> Result<PathBuf, Box<dyn Error>> {
    let root = match codex_home.filter(|value| !value.is_empty()) {
        Some(root) => PathBuf::from(root),
        None => PathBuf::from(home.ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "HOME must be set to install the Boomux Codex hooks",
            )
        })?)
        .join(".codex"),
    };
    require_absolute_root(&root, "Codex configuration root")?;
    Ok(root)
}

pub(crate) fn kiro_config_root(
    kiro_home: Option<OsString>,
    home: Option<OsString>,
) -> Result<PathBuf, Box<dyn Error>> {
    let root = match kiro_home.filter(|value| !value.is_empty()) {
        Some(root) => PathBuf::from(root),
        None => PathBuf::from(home.ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "HOME must be set to install the Boomux Kiro hooks",
            )
        })?)
        .join(".kiro"),
    };
    require_absolute_root(&root, "Kiro configuration root")?;
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

fn inspect_codex_hooks(path: &Path) -> io::Result<ExistingAsset> {
    let Some((document, _, _)) = read_codex_hooks(path)? else {
        return Ok(ExistingAsset::Missing);
    };
    codex_hooks_state(&document)
}

fn read_codex_hooks(path: &Path) -> io::Result<Option<(Value, Vec<u8>, u32)>> {
    let mut file = match OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
    {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("install path is not a regular file: {}", path.display()),
        ));
    }
    if metadata.len() > MAX_CODEX_HOOKS_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "Codex hooks file exceeds {MAX_CODEX_HOOKS_BYTES} bytes: {}",
                path.display()
            ),
        ));
    }
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    Read::by_ref(&mut file)
        .take(MAX_CODEX_HOOKS_BYTES + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_CODEX_HOOKS_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Codex hooks file exceeded its size limit while reading",
        ));
    }
    let document = serde_json::from_slice::<Value>(&bytes).map_err(|error| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("invalid Codex hooks JSON: {error}"),
        )
    })?;
    if !document.is_object() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Codex hooks file must contain a JSON object",
        ));
    }
    Ok(Some((
        document,
        bytes,
        metadata.permissions().mode() & 0o777,
    )))
}

fn codex_hooks_state(document: &Value) -> io::Result<ExistingAsset> {
    let Some(hooks) = document.get("hooks") else {
        return Ok(ExistingAsset::Missing);
    };
    let hooks = hooks.as_object().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "Codex hooks field must contain a JSON object",
        )
    })?;
    let mut found = false;
    let mut invalid = false;
    let mut valid_events = std::collections::HashSet::new();
    for (event, groups) in hooks {
        let Some(groups) = groups.as_array() else {
            if CODEX_HOOK_EVENTS.contains(&event.as_str()) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("Codex {event} hooks must contain an array"),
                ));
            }
            continue;
        };
        for group in groups {
            let Some(handlers) = group.get("hooks").and_then(Value::as_array) else {
                continue;
            };
            for handler in handlers {
                if handler.get("command").and_then(Value::as_str) != Some(CODEX_HOOK_COMMAND) {
                    continue;
                }
                found = true;
                if CODEX_HOOK_EVENTS.contains(&event.as_str())
                    && group.get("matcher").is_none()
                    && handler == &codex_hook_handler(event)
                    && valid_events.insert(event.as_str())
                {
                    continue;
                }
                invalid = true;
            }
        }
    }
    Ok(if !found {
        ExistingAsset::Missing
    } else if !invalid && valid_events.len() == CODEX_HOOK_EVENTS.len() {
        ExistingAsset::Current
    } else {
        ExistingAsset::Modified
    })
}

fn codex_hook_handler(event: &str) -> Value {
    serde_json::json!({
        "type": "command",
        "command": CODEX_HOOK_COMMAND,
        "timeout": if event == "SessionEnd" { 3 } else { 5 },
    })
}

fn install_codex_hooks(
    directory: &Path,
    path: &Path,
    force: bool,
) -> Result<InstallOutcome, Box<dyn Error>> {
    let existing = read_codex_hooks(path)?;
    let state = existing
        .as_ref()
        .map(|(document, _, _)| codex_hooks_state(document))
        .transpose()?
        .unwrap_or(ExistingAsset::Missing);
    match state {
        ExistingAsset::Current => return Ok(InstallOutcome::Unchanged),
        ExistingAsset::Modified if !force => return Err(existing_asset_error(path).into()),
        ExistingAsset::Missing | ExistingAsset::Modified => {}
    }
    let outcome = match state {
        ExistingAsset::Missing => InstallOutcome::Installed,
        ExistingAsset::Modified => InstallOutcome::Replaced,
        ExistingAsset::Current => unreachable!("current Codex hooks returned early"),
    };
    let (mut document, baseline, mode) = existing.unwrap_or_else(|| {
        (
            serde_json::from_str(CODEX_ASSET_FALLBACK).expect("bundled Codex hooks are valid"),
            Vec::new(),
            0o600,
        )
    });
    replace_codex_handlers(&mut document)?;
    let content = codex_hooks_content(&document)?;
    write_merged_asset_atomically(
        directory,
        path,
        &content,
        (!baseline.is_empty()).then_some(baseline.as_slice()),
        mode,
    )?;
    Ok(outcome)
}

const CODEX_ASSET_FALLBACK: &str = include_str!("../integrations/codex/hooks.json");

fn replace_codex_handlers(document: &mut Value) -> io::Result<()> {
    let root = document
        .as_object_mut()
        .expect("validated Codex hooks object");
    let hooks = root
        .entry("hooks")
        .or_insert_with(|| Value::Object(Map::new()))
        .as_object_mut()
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "Codex hooks field must contain a JSON object",
            )
        })?;
    for groups in hooks.values_mut() {
        let Some(groups) = groups.as_array_mut() else {
            continue;
        };
        for group in groups.iter_mut() {
            if let Some(handlers) = group.get_mut("hooks").and_then(Value::as_array_mut) {
                handlers.retain(|handler| {
                    handler.get("command").and_then(Value::as_str) != Some(CODEX_HOOK_COMMAND)
                });
            }
        }
        groups.retain(|group| {
            group
                .get("hooks")
                .and_then(Value::as_array)
                .is_none_or(|handlers| !handlers.is_empty())
        });
    }
    for event in CODEX_HOOK_EVENTS {
        let groups = hooks
            .entry((*event).to_owned())
            .or_insert_with(|| Value::Array(Vec::new()))
            .as_array_mut()
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("Codex {event} hooks must contain an array"),
                )
            })?;
        groups.push(serde_json::json!({ "hooks": [codex_hook_handler(event)] }));
    }
    Ok(())
}

fn uninstall_codex_hooks(directory: &Path, path: &Path) -> io::Result<()> {
    let Some((mut document, baseline, mode)) = read_codex_hooks(path)? else {
        return Ok(());
    };
    remove_codex_handlers(&mut document)?;
    if codex_document_is_boomux_only(&document) {
        verify_asset_baseline(path, Some(&baseline))?;
        fs::remove_file(path)?;
        fs::File::open(directory)?.sync_all()?;
        return Ok(());
    }
    let content = codex_hooks_content(&document)?;
    write_merged_asset_atomically(directory, path, &content, Some(&baseline), mode)
}

fn remove_codex_handlers(document: &mut Value) -> io::Result<()> {
    let Some(hooks) = document.get_mut("hooks") else {
        return Ok(());
    };
    let hooks = hooks.as_object_mut().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "Codex hooks field must contain a JSON object",
        )
    })?;
    for groups in hooks.values_mut() {
        let Some(groups) = groups.as_array_mut() else {
            continue;
        };
        for group in groups.iter_mut() {
            if let Some(handlers) = group.get_mut("hooks").and_then(Value::as_array_mut) {
                handlers.retain(|handler| {
                    handler.get("command").and_then(Value::as_str) != Some(CODEX_HOOK_COMMAND)
                });
            }
        }
        groups.retain(|group| {
            group
                .get("hooks")
                .and_then(Value::as_array)
                .is_none_or(|handlers| !handlers.is_empty())
        });
    }
    hooks.retain(|_, groups| groups.as_array().is_none_or(|groups| !groups.is_empty()));
    Ok(())
}

fn codex_document_is_boomux_only(document: &Value) -> bool {
    let Some(root) = document.as_object() else {
        return false;
    };
    root.iter().all(|(key, value)| match key.as_str() {
        "description" => {
            value.as_str() == Some("Report Codex lifecycle state to Boomux for managed ShellRuns.")
        }
        "hooks" => value.as_object().is_some_and(Map::is_empty),
        _ => false,
    })
}

fn codex_hooks_content(document: &Value) -> io::Result<String> {
    let mut content = serde_json::to_string_pretty(document)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    content.push('\n');
    Ok(content)
}

fn write_merged_asset_atomically(
    directory: &Path,
    path: &Path,
    content: &str,
    baseline: Option<&[u8]>,
    mode: u32,
) -> io::Result<()> {
    let temporary = directory.join(format!(".boomux-install-{}.tmp", Uuid::new_v4()));
    let result = (|| {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(mode)
            .open(&temporary)?;
        file.set_permissions(fs::Permissions::from_mode(mode))?;
        file.write_all(content.as_bytes())?;
        file.sync_all()?;
        verify_asset_baseline(path, baseline)?;
        fs::rename(&temporary, path)?;
        fs::File::open(directory)?.sync_all()?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

fn verify_asset_baseline(path: &Path, baseline: Option<&[u8]>) -> io::Result<()> {
    let current = read_codex_hooks(path)?.map(|(_, bytes, _)| bytes);
    if current.as_deref() != baseline {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!(
                "{} changed while the integration was being updated",
                path.display()
            ),
        ));
    }
    Ok(())
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
    use std::collections::HashSet;
    use std::time::{SystemTime, UNIX_EPOCH};

    use boomux::protocol::{
        AgentAuthority, AgentInstanceSnapshot, AgentObservationSnapshot, AgentState,
        ShellRunSnapshot, ShellSnapshot, WorkspaceSnapshot,
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
            None,
            Some(OsString::new()),
        )
    }

    #[test]
    fn descriptors_have_unique_names_and_expected_metadata() {
        assert_eq!(
            IntegrationId::Opencode.installation().package,
            "opencode-ai"
        );
        assert_eq!(IntegrationId::Pi.installation().validated_version, "0.84.1");
        assert_eq!(
            IntegrationId::Claude.installation().package,
            "@anthropic-ai/claude-code"
        );
        assert_eq!(
            IntegrationId::Claude.installation().validated_version,
            "2.1.236"
        );
        assert_eq!(IntegrationId::Codex.installation().package, "@openai/codex");
        assert_eq!(
            IntegrationId::Codex.installation().validated_version,
            "0.147.0"
        );
        assert_eq!(IntegrationId::Kiro.installation().package, "kiro-cli");
        assert_eq!(
            IntegrationId::Kiro.installation().validated_version,
            "2.18.0"
        );
        assert!(IntegrationId::Claude.spec().titles.is_none());
        assert_eq!(
            IntegrationId::Claude
                .spec()
                .foreground
                .map(|capability| capability.process_name),
            Some("claude")
        );
        assert_ne!(
            IntegrationId::Opencode.spec().key,
            IntegrationId::Pi.spec().key
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
        assert_eq!(
            claude_config_root(Some("/claude".into()), Some("/home/example".into())).unwrap(),
            Path::new("/claude")
        );
        assert_eq!(
            claude_config_root(Some(OsString::new()), Some("/home/example".into())).unwrap(),
            Path::new("/home/example/.claude")
        );
        assert!(claude_config_root(Some("relative".into()), None).is_err());
        assert_eq!(
            codex_config_root(Some("/codex".into()), Some("/home/example".into())).unwrap(),
            Path::new("/codex")
        );
        assert_eq!(
            codex_config_root(Some(OsString::new()), Some("/home/example".into())).unwrap(),
            Path::new("/home/example/.codex")
        );
        assert!(codex_config_root(Some("relative".into()), None).is_err());
        assert_eq!(
            kiro_config_root(Some("/kiro".into()), Some("/home/example".into())).unwrap(),
            Path::new("/kiro")
        );
        assert_eq!(
            kiro_config_root(Some(OsString::new()), Some("/home/example".into())).unwrap(),
            Path::new("/home/example/.kiro")
        );
        assert!(kiro_config_root(Some("relative".into()), None).is_err());
    }

    #[test]
    fn kiro_asset_defines_required_non_deciding_hooks() {
        let document: Value = serde_json::from_str(KIRO_ASSET).unwrap();
        assert_eq!(document["version"], "v1");
        let hooks = document["hooks"].as_array().unwrap();
        assert_eq!(hooks.len(), 5);
        let triggers = hooks
            .iter()
            .map(|hook| hook["trigger"].as_str().unwrap())
            .collect::<HashSet<_>>();
        assert_eq!(
            triggers,
            HashSet::from([
                "SessionStart",
                "UserPromptSubmit",
                "PreToolUse",
                "PostToolUse",
                "Stop",
            ])
        );
        for hook in hooks {
            assert_eq!(hook["action"]["type"], "command");
            assert_eq!(hook["action"]["command"], "boomux kiro hook");
            assert_eq!(hook["timeout"], 5);
            assert!(hook.get("confirm").is_none());
        }
    }

    #[test]
    fn claude_plugin_manifest_uses_exec_hooks_for_required_events() {
        let manifest: serde_json::Value = serde_json::from_str(CLAUDE_ASSET).unwrap();
        assert_eq!(manifest["name"], "boomux");
        let hooks = manifest["hooks"].as_object().unwrap();
        assert_eq!(
            hooks.keys().map(String::as_str).collect::<Vec<_>>(),
            [
                "Notification",
                "PermissionDenied",
                "PermissionRequest",
                "PostToolUse",
                "PostToolUseFailure",
                "PreToolUse",
                "SessionEnd",
                "SessionStart",
                "Stop",
                "StopFailure",
                "SubagentStart",
                "SubagentStop",
                "UserPromptSubmit",
            ]
        );
        for groups in hooks.values() {
            let handlers = groups[0]["hooks"].as_array().unwrap();
            assert_eq!(handlers.len(), 1);
            assert_eq!(handlers[0]["type"], "command");
            assert_eq!(handlers[0]["command"], "boomux");
            assert_eq!(handlers[0]["args"], serde_json::json!(["claude", "hook"]));
            assert_eq!(handlers[0]["timeout"], 5);
        }
    }

    #[test]
    fn claude_status_install_and_uninstall_use_skills_directory_plugin() {
        let home = TestDirectory::new("claude-install");
        let config = home.0.join("claude-config");
        let environment = Environment::for_test(
            Some(home.0.as_os_str().to_owned()),
            None,
            None,
            Some(config.as_os_str().to_owned()),
            Some(OsString::new()),
        );
        let expected = config.join("skills/boomux/.claude-plugin/plugin.json");

        let missing = inspect(IntegrationId::Claude, &environment, None);
        assert_eq!(missing.asset.state, AssetState::Missing);
        assert_eq!(missing.asset.path.as_deref(), expected.to_str());

        let installed = install(IntegrationId::Claude, &environment, false).unwrap();
        assert_eq!(installed.result, InstallOutcome::Installed);
        assert_eq!(Path::new(&installed.path), expected);
        assert_eq!(fs::read_to_string(&expected).unwrap(), CLAUDE_ASSET);
        assert_eq!(
            inspect(IntegrationId::Claude, &environment, None)
                .asset
                .state,
            AssetState::Current
        );

        let removed = uninstall(IntegrationId::Claude, &environment, false).unwrap();
        assert_eq!(removed.result, UninstallOutcome::Removed);
        assert!(!expected.exists());
    }

    #[test]
    fn codex_asset_defines_required_non_deciding_hooks() {
        let document: Value = serde_json::from_str(CODEX_ASSET).unwrap();
        assert_eq!(
            codex_hooks_state(&document).unwrap(),
            ExistingAsset::Current
        );
        let hooks = document["hooks"].as_object().unwrap();
        assert_eq!(hooks.len(), CODEX_HOOK_EVENTS.len());
        for event in CODEX_HOOK_EVENTS {
            let handler = &hooks[*event][0]["hooks"][0];
            assert_eq!(handler["type"], "command");
            assert_eq!(handler["command"], CODEX_HOOK_COMMAND);
            assert_eq!(
                handler["timeout"],
                if *event == "SessionEnd" { 3 } else { 5 }
            );
        }
    }

    #[test]
    fn codex_asset_requires_each_hook_event_exactly_once() {
        let mut document: Value = serde_json::from_str(CODEX_ASSET).unwrap();
        let duplicate = document["hooks"]["PreToolUse"][0].clone();
        document["hooks"]["PermissionRequest"] = Value::Array(Vec::new());
        document["hooks"]["PreToolUse"]
            .as_array_mut()
            .unwrap()
            .push(duplicate);

        assert_eq!(
            codex_hooks_state(&document).unwrap(),
            ExistingAsset::Modified
        );
    }

    #[test]
    fn codex_install_and_uninstall_preserve_unrelated_hooks() {
        let home = TestDirectory::new("codex-merge");
        let config = home.0.join("codex-home");
        fs::create_dir(&config).unwrap();
        let path = config.join("hooks.json");
        fs::write(
            &path,
            r#"{
  "description": "user hooks",
  "custom": true,
  "hooks": {
    "PreToolUse": [{"matcher":"Bash","hooks":[{"type":"command","command":"user policy","timeout":30}]}]
  }
}
"#,
        )
        .unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o640)).unwrap();
        let mut environment = environment(&home.0);
        environment.codex_home = Some(config.clone().into_os_string());

        let before = inspect(IntegrationId::Codex, &environment, None);
        assert_eq!(before.asset.state, AssetState::Missing);
        let installed = install(IntegrationId::Codex, &environment, false).unwrap();
        assert_eq!(installed.result, InstallOutcome::Installed);
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o640
        );
        let installed_document: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(installed_document["description"], "user hooks");
        assert_eq!(installed_document["custom"], true);
        assert_eq!(
            installed_document["hooks"]["PreToolUse"][0]["hooks"][0]["command"],
            "user policy"
        );
        assert_eq!(
            codex_hooks_state(&installed_document).unwrap(),
            ExistingAsset::Current
        );
        assert_eq!(
            install(IntegrationId::Codex, &environment, false)
                .unwrap()
                .result,
            InstallOutcome::Unchanged
        );

        let removed = uninstall(IntegrationId::Codex, &environment, false).unwrap();
        assert_eq!(removed.result, UninstallOutcome::Removed);
        let remaining: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(remaining["description"], "user hooks");
        assert_eq!(remaining["custom"], true);
        assert_eq!(
            remaining["hooks"]["PreToolUse"][0]["hooks"][0]["command"],
            "user policy"
        );
        assert_eq!(
            codex_hooks_state(&remaining).unwrap(),
            ExistingAsset::Missing
        );
    }

    #[test]
    fn codex_force_repairs_only_boomux_handlers_and_standalone_uninstall_removes_file() {
        let home = TestDirectory::new("codex-repair");
        let mut environment = environment(&home.0);
        environment.codex_home = Some(home.0.join("codex-home").into_os_string());
        let installed = install(IntegrationId::Codex, &environment, false).unwrap();
        let path = PathBuf::from(&installed.path);
        let mut document: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        document["hooks"]["Stop"][0]["hooks"][0]["timeout"] = Value::from(99);
        fs::write(&path, codex_hooks_content(&document).unwrap()).unwrap();

        assert_eq!(
            inspect(IntegrationId::Codex, &environment, None)
                .asset
                .state,
            AssetState::Modified
        );
        assert!(install(IntegrationId::Codex, &environment, false).is_err());
        assert_eq!(
            install(IntegrationId::Codex, &environment, true)
                .unwrap()
                .result,
            InstallOutcome::Replaced
        );
        assert_eq!(
            uninstall(IntegrationId::Codex, &environment, false)
                .unwrap()
                .result,
            UninstallOutcome::Removed
        );
        assert!(!path.exists());
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
    fn kiro_install_owns_only_its_dedicated_hook_file() {
        let home = TestDirectory::new("kiro-install");
        let mut environment = environment(&home.0);
        environment.kiro_home = Some(home.0.join("kiro-home").into_os_string());
        let installed = install(IntegrationId::Kiro, &environment, false).unwrap();
        let path = PathBuf::from(&installed.path);
        assert_eq!(path, home.0.join("kiro-home/hooks/boomux.json"));
        assert_eq!(fs::read_to_string(&path).unwrap(), KIRO_ASSET);
        assert_eq!(
            install(IntegrationId::Kiro, &environment, false)
                .unwrap()
                .result,
            InstallOutcome::Unchanged
        );

        fs::write(&path, "custom").unwrap();
        assert!(install(IntegrationId::Kiro, &environment, false).is_err());
        assert_eq!(
            install(IntegrationId::Kiro, &environment, true)
                .unwrap()
                .result,
            InstallOutcome::Replaced
        );
        assert_eq!(
            uninstall(IntegrationId::Kiro, &environment, false)
                .unwrap()
                .result,
            UninstallOutcome::Removed
        );
        assert!(!path.exists());
        assert!(path.parent().unwrap().is_dir());
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
            revision: 1,
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
            recovered_agent_id: None,
            foreground_process: Some("opencode".into()),
        };
        let mut workspace = WorkspaceSnapshot {
            id: "w1".into(),
            revision: 1,
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

use std::env;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
#[cfg(test)]
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::protocol::{
    AgentAttentionSnapshot, AgentObservationSnapshot, AgentScheduleOverlapPolicy,
    AgentScheduleSession, AgentScheduleState, AgentScheduleTrigger, ScheduledExecutionDispatchKind,
    ScheduledExecutionOutcome, ScheduledExecutionReason, ScheduledExecutionState, ShellOwner,
    ShellRunExitReason, TerminalProfile,
};

const STATE_VERSION: u32 = 13;
const VERSION_TWELVE_STATE_VERSION: u32 = 12;
const VERSION_ELEVEN_STATE_VERSION: u32 = 11;
const VERSION_TEN_STATE_VERSION: u32 = 10;
const VERSION_NINE_STATE_VERSION: u32 = 9;
const VERSION_EIGHT_STATE_VERSION: u32 = 8;
const VERSION_SEVEN_STATE_VERSION: u32 = 7;
const PREVIOUS_STATE_VERSION: u32 = 6;
const VERSION_FIVE_STATE_VERSION: u32 = 5;
const VERSION_FOUR_STATE_VERSION: u32 = 4;
const VERSION_THREE_STATE_VERSION: u32 = 3;
const VERSION_TWO_STATE_VERSION: u32 = 2;
const LEGACY_STATE_VERSION: u32 = 1;
const MAX_STATE_BYTES: u64 = 8 * 1024 * 1024;
const DISPATCH_KEY_FILTER_BYTES: usize = 2048;

const fn initial_revision() -> u64 {
    1
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PersistedState {
    version: u32,
    pub(crate) workspaces: Vec<PersistedWorkspace>,
}

impl Default for PersistedState {
    fn default() -> Self {
        Self {
            version: STATE_VERSION,
            workspaces: Vec::new(),
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PersistedWorkspace {
    pub(crate) id: String,
    #[serde(default = "initial_revision")]
    pub(crate) revision: u64,
    pub(crate) name: String,
    pub(crate) default_cwd: Option<PathBuf>,
    pub(crate) shells: Vec<PersistedShell>,
    pub(crate) launchers: Vec<PersistedWorkspaceLauncher>,
    pub(crate) agents: Vec<PersistedAgentInstance>,
    pub(crate) schedules: Vec<PersistedAgentSchedule>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PersistedAgentSchedule {
    pub(crate) id: String,
    pub(crate) name: String,
    pub(crate) cwd: PathBuf,
    pub(crate) integration: String,
    pub(crate) prompt: String,
    pub(crate) session: AgentScheduleSession,
    pub(crate) trigger: AgentScheduleTrigger,
    pub(crate) state: AgentScheduleState,
    pub(crate) overlap_policy: AgentScheduleOverlapPolicy,
    pub(crate) revision: u64,
    pub(crate) prompt_revision: u64,
    pub(crate) trigger_revision: u64,
    pub(crate) created_at_ms: u64,
    pub(crate) updated_at_ms: u64,
    pub(crate) evaluation_frontier_ms: u64,
    pub(crate) evaluation_frontier_trigger_revision: u64,
    pub(crate) execution_shell_id: Option<String>,
    pub(crate) dispatch_key_filter: Vec<u8>,
    pub(crate) executions: Vec<PersistedScheduledExecution>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PersistedScheduledExecution {
    pub(crate) id: String,
    pub(crate) revision: u64,
    pub(crate) state: ScheduledExecutionState,
    pub(crate) dispatch_kind: ScheduledExecutionDispatchKind,
    pub(crate) dispatch_key: String,
    pub(crate) schedule_revision: u64,
    pub(crate) prompt_revision: u64,
    pub(crate) trigger_revision: u64,
    pub(crate) requested_at_ms: u64,
    pub(crate) scheduled_at_ms: Option<u64>,
    pub(crate) coalesced_through_ms: Option<u64>,
    pub(crate) started_at_ms: Option<u64>,
    pub(crate) ended_at_ms: Option<u64>,
    pub(crate) cwd: PathBuf,
    pub(crate) integration: String,
    pub(crate) session: AgentScheduleSession,
    pub(crate) prompt: String,
    pub(crate) runner_token: String,
    pub(crate) reason: Option<ScheduledExecutionReason>,
    pub(crate) outcome: Option<ScheduledExecutionOutcome>,
    pub(crate) shell_id: Option<String>,
    pub(crate) run_id: Option<String>,
    pub(crate) agent_id: Option<String>,
    pub(crate) external_session_id: Option<String>,
}

impl std::fmt::Debug for PersistedScheduledExecution {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PersistedScheduledExecution")
            .field("id", &self.id)
            .field("state", &self.state)
            .field("dispatch_key", &self.dispatch_key)
            .field("prompt", &"<redacted>")
            .finish_non_exhaustive()
    }
}

impl std::fmt::Debug for PersistedAgentSchedule {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PersistedAgentSchedule")
            .field("id", &self.id)
            .field("name", &self.name)
            .field("cwd", &self.cwd)
            .field("integration", &self.integration)
            .field("prompt", &"<redacted>")
            .field("session", &self.session)
            .field("trigger", &self.trigger)
            .field("state", &self.state)
            .field("overlap_policy", &self.overlap_policy)
            .field("revision", &self.revision)
            .field("prompt_revision", &self.prompt_revision)
            .field("trigger_revision", &self.trigger_revision)
            .field("created_at_ms", &self.created_at_ms)
            .field("updated_at_ms", &self.updated_at_ms)
            .field("evaluation_frontier_ms", &self.evaluation_frontier_ms)
            .field("execution_shell_id", &self.execution_shell_id)
            .field("executions", &self.executions)
            .finish()
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PersistedAgentInstance {
    pub(crate) id: String,
    pub(crate) shell_id: String,
    pub(crate) run_id: String,
    pub(crate) name: String,
    pub(crate) integration: String,
    pub(crate) external_session_id: Option<String>,
    pub(crate) cwd: Option<PathBuf>,
    pub(crate) started_at_ms: u64,
    pub(crate) ended_at_ms: Option<u64>,
    pub(crate) observation: AgentObservationSnapshot,
    pub(crate) attention: Option<AgentAttentionSnapshot>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PersistedWorkspaceLauncher {
    pub(crate) id: String,
    #[serde(default = "initial_revision")]
    pub(crate) revision: u64,
    pub(crate) name: String,
    pub(crate) cwd: PathBuf,
    pub(crate) command: Vec<String>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PersistedShell {
    pub(crate) id: String,
    #[serde(default = "initial_revision")]
    pub(crate) revision: u64,
    pub(crate) name: String,
    pub(crate) cwd: PathBuf,
    pub(crate) command: Vec<String>,
    pub(crate) owner: ShellOwner,
    pub(crate) last_run: Option<PersistedShellRun>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PreOwnershipPersistedShell {
    id: String,
    name: String,
    cwd: PathBuf,
    command: Vec<String>,
    last_run: Option<PersistedShellRun>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct VersionNinePersistedState {
    version: u32,
    workspaces: Vec<VersionNinePersistedWorkspace>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct VersionNinePersistedWorkspace {
    id: String,
    name: String,
    default_cwd: Option<PathBuf>,
    shells: Vec<VersionNinePersistedShell>,
    launchers: Vec<PersistedWorkspaceLauncher>,
    agents: Vec<PersistedAgentInstance>,
    schedules: Vec<VersionNinePersistedAgentSchedule>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct VersionNinePersistedShell {
    id: String,
    name: String,
    cwd: PathBuf,
    command: Vec<String>,
    last_run: Option<PersistedShellRun>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct VersionNinePersistedAgentSchedule {
    id: String,
    name: String,
    cwd: PathBuf,
    integration: String,
    prompt: String,
    session: AgentScheduleSession,
    trigger: AgentScheduleTrigger,
    state: AgentScheduleState,
    overlap_policy: AgentScheduleOverlapPolicy,
    revision: u64,
    prompt_revision: u64,
    trigger_revision: u64,
    created_at_ms: u64,
    updated_at_ms: u64,
    evaluation_frontier_ms: u64,
    execution_shell_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PersistedShellRun {
    pub(crate) id: String,
    pub(crate) generation: u64,
    pub(crate) started_at_ms: u64,
    pub(crate) ended_at_ms: Option<u64>,
    pub(crate) exit_reason: Option<ShellRunExitReason>,
    pub(crate) output_revision: u64,
    pub(crate) environment_has_run_id: bool,
    pub(crate) profile: TerminalProfile,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) terminal_history: Option<String>,
}

#[derive(Deserialize)]
struct StateVersion {
    version: u32,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct VersionEightPersistedState {
    version: u32,
    workspaces: Vec<VersionEightPersistedWorkspace>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct VersionEightPersistedWorkspace {
    id: String,
    name: String,
    default_cwd: Option<PathBuf>,
    shells: Vec<PreOwnershipPersistedShell>,
    launchers: Vec<PersistedWorkspaceLauncher>,
    agents: Vec<PersistedAgentInstance>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct VersionSevenPersistedState {
    version: u32,
    workspaces: Vec<VersionSevenPersistedWorkspace>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct VersionSevenPersistedWorkspace {
    id: String,
    name: String,
    default_cwd: Option<PathBuf>,
    shells: Vec<VersionSevenPersistedShell>,
    launchers: Vec<PersistedWorkspaceLauncher>,
    agents: Vec<PersistedAgentInstance>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct VersionSevenPersistedShell {
    id: String,
    name: String,
    cwd: PathBuf,
    command: Vec<String>,
    last_run: Option<VersionSevenPersistedShellRun>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct VersionSevenPersistedShellRun {
    id: String,
    generation: u64,
    started_at_ms: u64,
    ended_at_ms: Option<u64>,
    exit_reason: Option<ShellRunExitReason>,
    output_revision: u64,
    environment_has_run_id: bool,
    profile: TerminalProfile,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PreviousPersistedState {
    version: u32,
    workspaces: Vec<PreviousPersistedWorkspace>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PreviousPersistedWorkspace {
    id: String,
    name: String,
    shells: Vec<PreOwnershipPersistedShell>,
    launchers: Vec<PersistedWorkspaceLauncher>,
    agents: Vec<PersistedAgentInstance>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct VersionFivePersistedState {
    version: u32,
    workspaces: Vec<VersionFivePersistedWorkspace>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct VersionFivePersistedWorkspace {
    id: String,
    name: String,
    shells: Vec<PreOwnershipPersistedShell>,
    launchers: Vec<PersistedWorkspaceLauncher>,
    agents: Vec<PreviousPersistedAgentInstance>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PreviousPersistedAgentInstance {
    id: String,
    shell_id: String,
    run_id: String,
    name: String,
    integration: String,
    external_session_id: Option<String>,
    cwd: Option<PathBuf>,
    started_at_ms: u64,
    ended_at_ms: Option<u64>,
    observation: AgentObservationSnapshot,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct VersionFourPersistedState {
    version: u32,
    workspaces: Vec<VersionFourPersistedWorkspace>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct VersionFourPersistedWorkspace {
    id: String,
    name: String,
    shells: Vec<PreOwnershipPersistedShell>,
    launchers: Vec<PersistedWorkspaceLauncher>,
    agents: Vec<VersionFourPersistedAgentInstance>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct VersionFourPersistedAgentInstance {
    id: String,
    shell_id: String,
    run_id: String,
    name: String,
    integration: String,
    external_session_id: Option<String>,
    started_at_ms: u64,
    ended_at_ms: Option<u64>,
    observation: AgentObservationSnapshot,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct VersionThreePersistedState {
    version: u32,
    workspaces: Vec<VersionThreePersistedWorkspace>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct VersionThreePersistedWorkspace {
    id: String,
    name: String,
    shells: Vec<PreOwnershipPersistedShell>,
    launchers: Vec<PersistedWorkspaceLauncher>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct VersionTwoPersistedState {
    version: u32,
    workspaces: Vec<VersionTwoPersistedWorkspace>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct VersionTwoPersistedWorkspace {
    id: String,
    name: String,
    shells: Vec<PreOwnershipPersistedShell>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LegacyPersistedState {
    version: u32,
    workspaces: Vec<LegacyPersistedWorkspace>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LegacyPersistedWorkspace {
    id: String,
    name: String,
    shells: Vec<LegacyPersistedShell>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LegacyPersistedShell {
    id: String,
    name: String,
    cwd: PathBuf,
    command: Vec<String>,
    last_profile: Option<TerminalProfile>,
}

pub(crate) struct StateStore {
    path: PathBuf,
    _lock: Option<File>,
    #[cfg(test)]
    before_save: Option<Arc<dyn Fn() + Send + Sync>>,
}

impl StateStore {
    pub(crate) fn from_environment() -> io::Result<Self> {
        let lock_path = lock_path_from_environment()?;
        let directory = lock_path
            .parent()
            .ok_or_else(|| io::Error::other("state lock path has no parent"))?;
        secure_state_dir(directory)?;
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(&lock_path)?;
        // The descriptor remains open for the store lifetime and `flock` takes
        // only pointer-free integer arguments.
        if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == -1 {
            let error = io::Error::last_os_error();
            return Err(if matches!(error.raw_os_error(), Some(libc::EWOULDBLOCK)) {
                io::Error::new(
                    io::ErrorKind::AddrInUse,
                    "another Boomux daemon is using this state directory",
                )
            } else {
                error
            });
        }
        Ok(Self {
            path: directory.join("state.json"),
            _lock: Some(lock),
            #[cfg(test)]
            before_save: None,
        })
    }

    pub(crate) fn from_transferred_lock(lock: OwnedFd) -> io::Result<Self> {
        let lock_path = lock_path_from_environment()?;
        let directory = lock_path
            .parent()
            .ok_or_else(|| io::Error::other("state lock path has no parent"))?;
        secure_state_dir(directory)?;
        Ok(Self {
            path: directory.join("state.json"),
            _lock: Some(File::from(lock)),
            #[cfg(test)]
            before_save: None,
        })
    }

    pub(crate) fn lock_descriptor(&self) -> io::Result<BorrowedFd<'_>> {
        self._lock
            .as_ref()
            .map(AsFd::as_fd)
            .ok_or_else(|| io::Error::other("state store has no ownership lock"))
    }

    #[cfg(test)]
    pub(crate) fn at(path: PathBuf) -> Self {
        Self {
            path,
            _lock: None,
            before_save: None,
        }
    }

    #[cfg(test)]
    pub(crate) fn at_with_save_hook(
        path: PathBuf,
        before_save: Arc<dyn Fn() + Send + Sync>,
    ) -> Self {
        Self {
            path,
            _lock: None,
            before_save: Some(before_save),
        }
    }

    #[cfg(test)]
    pub(crate) fn load(&self) -> io::Result<Option<PersistedState>> {
        let (state, migrated) = self.load_deferred()?;
        if migrated && let Some(state) = &state {
            self.save(state)?;
        }
        Ok(state)
    }

    pub(crate) fn load_deferred(&self) -> io::Result<(Option<PersistedState>, bool)> {
        let Some(parent) = self.path.parent() else {
            return Err(io::Error::other("state path has no parent"));
        };
        secure_state_dir(parent)?;
        let metadata = match fs::symlink_metadata(&self.path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok((None, false)),
            Err(error) => return Err(error),
        };
        if !metadata.is_file() || metadata.uid() != effective_uid() {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "boomux state path is not an owned regular file",
            ));
        }
        if metadata.len() > MAX_STATE_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "boomux state file exceeds the size limit",
            ));
        }
        let mut bytes = Vec::with_capacity(metadata.len() as usize);
        File::open(&self.path)?.read_to_end(&mut bytes)?;
        let version: StateVersion = serde_json::from_slice(&bytes).map_err(|error| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("could not parse {}: {error}", self.path.display()),
            )
        })?;
        let (state, migrated) = match version.version {
            STATE_VERSION => (parse_current_state(&bytes, &self.path)?, false),
            VERSION_TWELVE_STATE_VERSION => {
                (migrate_version_twelve_state(&bytes, &self.path)?, true)
            }
            VERSION_ELEVEN_STATE_VERSION => {
                (migrate_version_eleven_state(&bytes, &self.path)?, true)
            }
            VERSION_TEN_STATE_VERSION => (migrate_version_ten_state(&bytes, &self.path)?, true),
            VERSION_NINE_STATE_VERSION => {
                let previous: VersionNinePersistedState = parse_state(&bytes, &self.path)?;
                (migrate_version_nine_state(previous), true)
            }
            VERSION_EIGHT_STATE_VERSION => {
                let previous: VersionEightPersistedState = parse_state(&bytes, &self.path)?;
                (migrate_version_eight_state(previous), true)
            }
            VERSION_SEVEN_STATE_VERSION => {
                let previous: VersionSevenPersistedState = parse_state(&bytes, &self.path)?;
                (migrate_version_seven_state(previous), true)
            }
            PREVIOUS_STATE_VERSION => {
                let previous: PreviousPersistedState = parse_state(&bytes, &self.path)?;
                (migrate_previous_state(previous), true)
            }
            VERSION_FIVE_STATE_VERSION => {
                let previous: VersionFivePersistedState = parse_state(&bytes, &self.path)?;
                (migrate_version_five_state(previous), true)
            }
            VERSION_FOUR_STATE_VERSION => {
                let previous: VersionFourPersistedState = parse_state(&bytes, &self.path)?;
                (migrate_version_four_state(previous), true)
            }
            VERSION_THREE_STATE_VERSION => {
                let previous: VersionThreePersistedState = parse_state(&bytes, &self.path)?;
                (migrate_version_three_state(previous), true)
            }
            VERSION_TWO_STATE_VERSION => {
                let previous: VersionTwoPersistedState = parse_state(&bytes, &self.path)?;
                (migrate_version_two_state(previous), true)
            }
            LEGACY_STATE_VERSION => {
                let legacy: LegacyPersistedState = parse_state(&bytes, &self.path)?;
                (migrate_legacy_state(legacy), true)
            }
            version => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("unsupported Boomux state version {version}; expected {STATE_VERSION}"),
                ));
            }
        };
        Ok((Some(state), migrated))
    }

    pub(crate) fn save(&self, state: &PersistedState) -> io::Result<()> {
        #[cfg(test)]
        if let Some(before_save) = &self.before_save {
            before_save();
        }
        let Some(parent) = self.path.parent() else {
            return Err(io::Error::other("state path has no parent"));
        };
        secure_state_dir(parent)?;
        let bytes = serde_json::to_vec_pretty(state).map_err(io::Error::other)?;
        if bytes.len() as u64 > MAX_STATE_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "boomux state exceeds the size limit",
            ));
        }
        let temporary = parent.join(format!(".state-{}.tmp", Uuid::new_v4()));
        let result = (|| {
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&temporary)?;
            file.write_all(&bytes)?;
            file.sync_all()?;
            fs::rename(&temporary, &self.path)?;
            // Rename is the commit point. Directory fsync improves crash
            // durability but cannot be rolled back if a filesystem rejects it.
            let _ = File::open(parent).and_then(|directory| directory.sync_all());
            Ok(())
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temporary);
        }
        result
    }
}

fn migrate_version_twelve_state(bytes: &[u8], path: &Path) -> io::Result<PersistedState> {
    let mut previous: serde_json::Value = parse_state(bytes, path)?;
    previous["version"] = serde_json::Value::from(STATE_VERSION);
    if let Some(workspaces) = previous["workspaces"].as_array_mut() {
        for workspace in workspaces {
            workspace["revision"] = serde_json::Value::from(1);
            if let Some(shells) = workspace["shells"].as_array_mut() {
                for shell in shells {
                    shell["revision"] = serde_json::Value::from(1);
                }
            }
            if let Some(launchers) = workspace["launchers"].as_array_mut() {
                for launcher in launchers {
                    launcher["revision"] = serde_json::Value::from(1);
                }
            }
        }
    }
    serde_json::from_value(previous).map_err(|error| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("could not migrate {}: {error}", path.display()),
        )
    })
}

fn migrate_version_eleven_state(bytes: &[u8], path: &Path) -> io::Result<PersistedState> {
    let mut previous: serde_json::Value = parse_state(bytes, path)?;
    previous["version"] = serde_json::Value::from(STATE_VERSION);
    if let Some(workspaces) = previous["workspaces"].as_array_mut() {
        for workspace in workspaces {
            if let Some(schedules) = workspace["schedules"].as_array_mut() {
                for schedule in schedules {
                    if let Some(executions) = schedule["executions"].as_array_mut() {
                        for execution in executions {
                            execution["revision"] = serde_json::Value::from(1);
                        }
                    }
                }
            }
        }
    }
    serde_json::from_value(previous).map_err(|error| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("could not migrate {}: {error}", path.display()),
        )
    })
}

fn migrate_version_ten_state(bytes: &[u8], path: &Path) -> io::Result<PersistedState> {
    let mut previous: serde_json::Value = parse_state(bytes, path)?;
    previous["version"] = serde_json::Value::from(VERSION_ELEVEN_STATE_VERSION);
    if let Some(workspaces) = previous["workspaces"].as_array_mut() {
        for workspace in workspaces {
            if let Some(schedules) = workspace["schedules"].as_array_mut() {
                for schedule in schedules {
                    schedule["evaluation_frontier_trigger_revision"] =
                        schedule["trigger_revision"].clone();
                    if let Some(executions) = schedule["executions"].as_array_mut() {
                        for execution in executions {
                            execution["scheduled_at_ms"] = serde_json::Value::Null;
                            execution["coalesced_through_ms"] = serde_json::Value::Null;
                            execution["revision"] = serde_json::Value::from(1);
                        }
                    }
                }
            }
        }
    }
    previous["version"] = serde_json::Value::from(STATE_VERSION);
    serde_json::from_value(previous).map_err(|error| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("could not migrate {}: {error}", path.display()),
        )
    })
}

fn parse_state<T: for<'de> Deserialize<'de>>(bytes: &[u8], path: &Path) -> io::Result<T> {
    serde_json::from_slice(bytes).map_err(|error| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("could not parse {}: {error}", path.display()),
        )
    })
}

fn parse_current_state(bytes: &[u8], path: &Path) -> io::Result<PersistedState> {
    let value: serde_json::Value = parse_state(bytes, path)?;
    let workspaces = value
        .get("workspaces")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "Boomux state has no workspace array",
            )
        })?;
    for workspace in workspaces {
        require_positive_revision(workspace, "workspace")?;
        for shell in workspace
            .get("shells")
            .and_then(serde_json::Value::as_array)
            .into_iter()
            .flatten()
        {
            require_positive_revision(shell, "shell")?;
        }
        for launcher in workspace
            .get("launchers")
            .and_then(serde_json::Value::as_array)
            .into_iter()
            .flatten()
        {
            require_positive_revision(launcher, "workspace launcher")?;
        }
    }
    serde_json::from_value(value).map_err(|error| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("could not parse {}: {error}", path.display()),
        )
    })
}

fn require_positive_revision(value: &serde_json::Value, resource: &str) -> io::Result<()> {
    if value
        .get("revision")
        .and_then(serde_json::Value::as_u64)
        .is_some_and(|revision| revision > 0)
    {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("Boomux state contains a {resource} without a positive revision"),
        ))
    }
}

fn migrate_legacy_state(legacy: LegacyPersistedState) -> PersistedState {
    debug_assert_eq!(legacy.version, LEGACY_STATE_VERSION);
    let migrated_at_ms = unix_time_ms();
    PersistedState {
        version: STATE_VERSION,
        workspaces: legacy
            .workspaces
            .into_iter()
            .map(|workspace| PersistedWorkspace {
                id: workspace.id,
                revision: 1,
                name: workspace.name,
                default_cwd: None,
                shells: workspace
                    .shells
                    .into_iter()
                    .map(|shell| PersistedShell {
                        id: shell.id,
                        revision: 1,
                        name: shell.name,
                        cwd: shell.cwd,
                        command: shell.command,
                        owner: ShellOwner::User,
                        last_run: shell.last_profile.map(|profile| PersistedShellRun {
                            id: Uuid::new_v4().to_string(),
                            generation: 1,
                            started_at_ms: migrated_at_ms,
                            ended_at_ms: None,
                            exit_reason: None,
                            output_revision: 0,
                            environment_has_run_id: false,
                            profile,
                            terminal_history: None,
                        }),
                    })
                    .collect(),
                launchers: Vec::new(),
                agents: Vec::new(),
                schedules: Vec::new(),
            })
            .collect(),
    }
}

fn migrate_version_nine_state(previous: VersionNinePersistedState) -> PersistedState {
    debug_assert_eq!(previous.version, VERSION_NINE_STATE_VERSION);
    PersistedState {
        version: STATE_VERSION,
        workspaces: previous
            .workspaces
            .into_iter()
            .map(|workspace| PersistedWorkspace {
                id: workspace.id,
                revision: 1,
                name: workspace.name,
                default_cwd: workspace.default_cwd,
                shells: workspace
                    .shells
                    .into_iter()
                    .map(|shell| PersistedShell {
                        id: shell.id,
                        revision: 1,
                        name: shell.name,
                        cwd: shell.cwd,
                        command: shell.command,
                        owner: ShellOwner::User,
                        last_run: shell.last_run,
                    })
                    .collect(),
                launchers: workspace.launchers,
                agents: workspace.agents,
                schedules: workspace
                    .schedules
                    .into_iter()
                    .map(|schedule| PersistedAgentSchedule {
                        id: schedule.id,
                        name: schedule.name,
                        cwd: schedule.cwd,
                        integration: schedule.integration,
                        prompt: schedule.prompt,
                        session: schedule.session,
                        trigger: schedule.trigger,
                        state: schedule.state,
                        overlap_policy: schedule.overlap_policy,
                        revision: schedule.revision,
                        prompt_revision: schedule.prompt_revision,
                        trigger_revision: schedule.trigger_revision,
                        created_at_ms: schedule.created_at_ms,
                        updated_at_ms: schedule.updated_at_ms,
                        evaluation_frontier_ms: schedule.evaluation_frontier_ms,
                        evaluation_frontier_trigger_revision: schedule.trigger_revision,
                        execution_shell_id: schedule.execution_shell_id,
                        dispatch_key_filter: vec![0; DISPATCH_KEY_FILTER_BYTES],
                        executions: Vec::new(),
                    })
                    .collect(),
            })
            .collect(),
    }
}

fn migrate_pre_ownership_shell(shell: PreOwnershipPersistedShell) -> PersistedShell {
    PersistedShell {
        id: shell.id,
        revision: 1,
        name: shell.name,
        cwd: shell.cwd,
        command: shell.command,
        owner: ShellOwner::User,
        last_run: shell.last_run,
    }
}

fn migrate_version_eight_state(previous: VersionEightPersistedState) -> PersistedState {
    debug_assert_eq!(previous.version, VERSION_EIGHT_STATE_VERSION);
    PersistedState {
        version: STATE_VERSION,
        workspaces: previous
            .workspaces
            .into_iter()
            .map(|workspace| PersistedWorkspace {
                id: workspace.id,
                revision: 1,
                name: workspace.name,
                default_cwd: workspace.default_cwd,
                shells: workspace
                    .shells
                    .into_iter()
                    .map(migrate_pre_ownership_shell)
                    .collect(),
                launchers: workspace.launchers,
                agents: workspace.agents,
                schedules: Vec::new(),
            })
            .collect(),
    }
}

fn migrate_version_seven_state(previous: VersionSevenPersistedState) -> PersistedState {
    debug_assert_eq!(previous.version, VERSION_SEVEN_STATE_VERSION);
    PersistedState {
        version: STATE_VERSION,
        workspaces: previous
            .workspaces
            .into_iter()
            .map(|workspace| PersistedWorkspace {
                id: workspace.id,
                revision: 1,
                name: workspace.name,
                default_cwd: workspace.default_cwd,
                shells: workspace
                    .shells
                    .into_iter()
                    .map(|shell| PersistedShell {
                        id: shell.id,
                        revision: 1,
                        name: shell.name,
                        cwd: shell.cwd,
                        command: shell.command,
                        owner: ShellOwner::User,
                        last_run: shell.last_run.map(|run| PersistedShellRun {
                            id: run.id,
                            generation: run.generation,
                            started_at_ms: run.started_at_ms,
                            ended_at_ms: run.ended_at_ms,
                            exit_reason: run.exit_reason,
                            output_revision: run.output_revision,
                            environment_has_run_id: run.environment_has_run_id,
                            profile: run.profile,
                            terminal_history: None,
                        }),
                    })
                    .collect(),
                launchers: workspace.launchers,
                agents: workspace.agents,
                schedules: Vec::new(),
            })
            .collect(),
    }
}

fn migrate_previous_state(previous: PreviousPersistedState) -> PersistedState {
    debug_assert_eq!(previous.version, PREVIOUS_STATE_VERSION);
    PersistedState {
        version: STATE_VERSION,
        workspaces: previous
            .workspaces
            .into_iter()
            .map(|workspace| PersistedWorkspace {
                id: workspace.id,
                revision: 1,
                name: workspace.name,
                default_cwd: None,
                shells: workspace
                    .shells
                    .into_iter()
                    .map(migrate_pre_ownership_shell)
                    .collect(),
                launchers: workspace.launchers,
                agents: workspace.agents,
                schedules: Vec::new(),
            })
            .collect(),
    }
}

fn migrate_version_five_state(previous: VersionFivePersistedState) -> PersistedState {
    debug_assert_eq!(previous.version, VERSION_FIVE_STATE_VERSION);
    PersistedState {
        version: STATE_VERSION,
        workspaces: previous
            .workspaces
            .into_iter()
            .map(|workspace| PersistedWorkspace {
                id: workspace.id,
                revision: 1,
                name: workspace.name,
                default_cwd: None,
                shells: workspace
                    .shells
                    .into_iter()
                    .map(migrate_pre_ownership_shell)
                    .collect(),
                launchers: workspace.launchers,
                agents: workspace
                    .agents
                    .into_iter()
                    .map(|agent| PersistedAgentInstance {
                        cwd: agent.cwd,
                        id: agent.id,
                        shell_id: agent.shell_id,
                        run_id: agent.run_id,
                        name: agent.name,
                        integration: agent.integration,
                        external_session_id: agent.external_session_id,
                        started_at_ms: agent.started_at_ms,
                        ended_at_ms: agent.ended_at_ms,
                        observation: agent.observation,
                        attention: None,
                    })
                    .collect(),
                schedules: Vec::new(),
            })
            .collect(),
    }
}

fn migrate_version_four_state(previous: VersionFourPersistedState) -> PersistedState {
    debug_assert_eq!(previous.version, VERSION_FOUR_STATE_VERSION);
    PersistedState {
        version: STATE_VERSION,
        workspaces: previous
            .workspaces
            .into_iter()
            .map(|workspace| {
                let shell_cwds = workspace
                    .shells
                    .iter()
                    .map(|shell| (shell.id.clone(), shell.cwd.clone()))
                    .collect::<std::collections::HashMap<_, _>>();
                PersistedWorkspace {
                    id: workspace.id,
                    revision: 1,
                    name: workspace.name,
                    default_cwd: None,
                    shells: workspace
                        .shells
                        .into_iter()
                        .map(migrate_pre_ownership_shell)
                        .collect(),
                    launchers: workspace.launchers,
                    agents: workspace
                        .agents
                        .into_iter()
                        .map(|agent| PersistedAgentInstance {
                            cwd: shell_cwds.get(&agent.shell_id).cloned(),
                            id: agent.id,
                            shell_id: agent.shell_id,
                            run_id: agent.run_id,
                            name: agent.name,
                            integration: agent.integration,
                            external_session_id: agent.external_session_id,
                            started_at_ms: agent.started_at_ms,
                            ended_at_ms: agent.ended_at_ms,
                            observation: agent.observation,
                            attention: None,
                        })
                        .collect(),
                    schedules: Vec::new(),
                }
            })
            .collect(),
    }
}

fn migrate_version_three_state(previous: VersionThreePersistedState) -> PersistedState {
    debug_assert_eq!(previous.version, VERSION_THREE_STATE_VERSION);
    PersistedState {
        version: STATE_VERSION,
        workspaces: previous
            .workspaces
            .into_iter()
            .map(|workspace| PersistedWorkspace {
                id: workspace.id,
                revision: 1,
                name: workspace.name,
                default_cwd: None,
                shells: workspace
                    .shells
                    .into_iter()
                    .map(migrate_pre_ownership_shell)
                    .collect(),
                launchers: workspace.launchers,
                agents: Vec::new(),
                schedules: Vec::new(),
            })
            .collect(),
    }
}

fn migrate_version_two_state(previous: VersionTwoPersistedState) -> PersistedState {
    debug_assert_eq!(previous.version, VERSION_TWO_STATE_VERSION);
    PersistedState {
        version: STATE_VERSION,
        workspaces: previous
            .workspaces
            .into_iter()
            .map(|workspace| PersistedWorkspace {
                id: workspace.id,
                revision: 1,
                name: workspace.name,
                default_cwd: None,
                shells: workspace
                    .shells
                    .into_iter()
                    .map(migrate_pre_ownership_shell)
                    .collect(),
                launchers: Vec::new(),
                agents: Vec::new(),
                schedules: Vec::new(),
            })
            .collect(),
    }
}

fn unix_time_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

pub(crate) fn state_directory_from_environment() -> io::Result<PathBuf> {
    let root = match env::var_os("XDG_STATE_HOME").filter(|path| !path.is_empty()) {
        Some(path) => PathBuf::from(path),
        None => PathBuf::from(
            env::var_os("HOME")
                .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "HOME is not set"))?,
        )
        .join(".local/state"),
    };
    if !root.is_absolute() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "XDG_STATE_HOME must be an absolute path",
        ));
    }
    Ok(root.join("boomux"))
}

pub(crate) fn lock_path_from_environment() -> io::Result<PathBuf> {
    Ok(state_directory_from_environment()?.join("daemon.lock"))
}

pub(crate) fn secure_state_dir(path: &Path) -> io::Result<()> {
    fs::create_dir_all(path)?;
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.is_dir() || metadata.uid() != effective_uid() {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "boomux state path is not an owned directory",
        ));
    }
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
}

pub(crate) fn effective_uid() -> u32 {
    // `geteuid` has no arguments, pointers, or caller safety requirements.
    unsafe { libc::geteuid() }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{AgentAuthority, AgentState};

    #[test]
    fn atomically_round_trips_state() {
        let directory = env::temp_dir().join(format!("boomux-state-{}", Uuid::new_v4()));
        let store = StateStore::at(directory.join("boomux/state.json"));
        let state = PersistedState {
            version: STATE_VERSION,
            workspaces: vec![PersistedWorkspace {
                id: Uuid::new_v4().to_string(),
                revision: 1,
                name: "saved".into(),
                default_cwd: Some("/tmp/project".into()),
                shells: vec![PersistedShell {
                    id: "shell-1".into(),
                    revision: 1,
                    name: "agent".into(),
                    cwd: "/tmp/project".into(),
                    command: vec!["opencode".into()],
                    owner: ShellOwner::User,
                    last_run: Some(PersistedShellRun {
                        id: "run-1".into(),
                        generation: 1,
                        started_at_ms: 1,
                        ended_at_ms: Some(2),
                        exit_reason: Some(ShellRunExitReason::Interrupted),
                        output_revision: 3,
                        environment_has_run_id: true,
                        profile: TerminalProfile {
                            term: Some("xterm-256color".into()),
                            colorterm: None,
                            term_program: None,
                            term_program_version: None,
                            rows: 24,
                            cols: 80,
                            pixel_width: 0,
                            pixel_height: 0,
                        },
                        terminal_history: Some("bounded output".into()),
                    }),
                }],
                launchers: vec![
                    PersistedWorkspaceLauncher {
                        id: "launcher-1".into(),
                        revision: 1,
                        name: "editor".into(),
                        cwd: "/tmp/project".into(),
                        command: vec!["editor".into(), "".into(), "two words".into()],
                    },
                    PersistedWorkspaceLauncher {
                        id: "launcher-2".into(),
                        revision: 1,
                        name: "server".into(),
                        cwd: "/tmp/project/server".into(),
                        command: vec!["server".into()],
                    },
                ],
                agents: vec![
                    PersistedAgentInstance {
                        id: "agent-1".into(),
                        shell_id: "shell-1".into(),
                        run_id: "run-1".into(),
                        name: "first".into(),
                        integration: "test".into(),
                        external_session_id: Some("external-1".into()),
                        cwd: Some("/tmp/project".into()),
                        started_at_ms: 10,
                        ended_at_ms: None,
                        observation: AgentObservationSnapshot {
                            revision: 2,
                            state: AgentState::Working,
                            authority: AgentAuthority::LifecycleIntegration,
                            evidence: "active".into(),
                            confidence: 90,
                            observed_at_ms: 11,
                        },
                        attention: None,
                    },
                    PersistedAgentInstance {
                        id: "agent-2".into(),
                        shell_id: "shell-2".into(),
                        run_id: "run-2".into(),
                        name: "second".into(),
                        integration: "adapter".into(),
                        external_session_id: None,
                        cwd: None,
                        started_at_ms: 20,
                        ended_at_ms: Some(30),
                        observation: AgentObservationSnapshot {
                            revision: 3,
                            state: AgentState::Done,
                            authority: AgentAuthority::DaemonLifecycle,
                            evidence: "run exited".into(),
                            confidence: 100,
                            observed_at_ms: 30,
                        },
                        attention: Some(AgentAttentionSnapshot {
                            reason: crate::protocol::AgentAttentionReason::Completed,
                            observation: AgentObservationSnapshot {
                                revision: 3,
                                state: AgentState::Done,
                                authority: AgentAuthority::DaemonLifecycle,
                                evidence: "run exited".into(),
                                confidence: 100,
                                observed_at_ms: 30,
                            },
                        }),
                    },
                ],
                schedules: vec![PersistedAgentSchedule {
                    id: "schedule-1".into(),
                    name: "daily review".into(),
                    cwd: "/tmp/project".into(),
                    integration: "opencode".into(),
                    prompt: "Review the current changes".into(),
                    session: AgentScheduleSession::Continue {
                        external_session_id: "external-1".into(),
                    },
                    trigger: AgentScheduleTrigger {
                        cron: "0 9 * * 1-5".into(),
                        timezone: "America/New_York".into(),
                    },
                    state: AgentScheduleState::Enabled,
                    overlap_policy: AgentScheduleOverlapPolicy::Skip,
                    revision: 4,
                    prompt_revision: 2,
                    trigger_revision: 3,
                    created_at_ms: 40,
                    updated_at_ms: 50,
                    evaluation_frontier_ms: 60,
                    evaluation_frontier_trigger_revision: 1,
                    execution_shell_id: Some("shell-1".into()),
                    dispatch_key_filter: vec![0; DISPATCH_KEY_FILTER_BYTES],
                    executions: Vec::new(),
                }],
            }],
        };
        let debug = format!("{state:?}");
        assert!(debug.contains("<redacted>"));
        assert!(!debug.contains("Review the current changes"));

        store.save(&state).unwrap();
        let restored = store.load().unwrap().unwrap();

        assert_eq!(restored.workspaces[0].name, "saved");
        assert_eq!(
            restored.workspaces[0].shells[0]
                .last_run
                .as_ref()
                .and_then(|run| run.terminal_history.as_deref()),
            Some("bounded output")
        );
        assert_eq!(
            restored.workspaces[0].default_cwd.as_deref(),
            Some(Path::new("/tmp/project"))
        );
        assert_eq!(restored.workspaces[0].launchers[0].id, "launcher-1");
        assert_eq!(restored.workspaces[0].launchers[0].name, "editor");
        assert_eq!(
            restored.workspaces[0].launchers[0].cwd,
            Path::new("/tmp/project")
        );
        assert_eq!(
            restored.workspaces[0].launchers[0].command,
            vec![
                String::from("editor"),
                String::new(),
                String::from("two words")
            ]
        );
        assert_eq!(restored.workspaces[0].launchers[1].id, "launcher-2");
        assert_eq!(restored.workspaces[0].agents[0].id, "agent-1");
        assert_eq!(restored.workspaces[0].agents[1].id, "agent-2");
        assert_eq!(
            restored.workspaces[0].agents[1].observation.state,
            AgentState::Done
        );
        assert_eq!(
            restored.workspaces[0].agents[1]
                .attention
                .as_ref()
                .map(|attention| attention.reason),
            Some(crate::protocol::AgentAttentionReason::Completed)
        );
        let schedule = &restored.workspaces[0].schedules[0];
        assert_eq!(schedule.id, "schedule-1");
        assert_eq!(schedule.prompt, "Review the current changes");
        assert_eq!(
            schedule.session,
            AgentScheduleSession::Continue {
                external_session_id: "external-1".into()
            }
        );
        assert_eq!(schedule.trigger.cron, "0 9 * * 1-5");
        assert_eq!(schedule.execution_shell_id.as_deref(), Some("shell-1"));
        assert_eq!(
            fs::metadata(directory.join("boomux/state.json"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn rejects_unsupported_state_versions() {
        let directory = env::temp_dir().join(format!("boomux-state-{}", Uuid::new_v4()));
        let path = directory.join("boomux/state.json");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, br#"{"version":99,"workspaces":[]}"#).unwrap();
        let store = StateStore::at(path);

        let error = store.load().unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn migrates_version_eight_workspaces_without_schedules() {
        let directory = env::temp_dir().join(format!("boomux-state-{}", Uuid::new_v4()));
        let path = directory.join("boomux/state.json");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(
            &path,
            br#"{
  "version": 8,
  "workspaces": [{
    "id": "w1", "name": "version-eight", "default_cwd": "/tmp/project",
    "shells": [{
      "id": "s1", "name": "agent", "cwd": "/tmp/project", "command": ["opencode"],
      "last_run": {
        "id": "r1", "generation": 2, "started_at_ms": 1, "ended_at_ms": 2,
        "exit_reason": {"reason": "interrupted"}, "output_revision": 3,
        "environment_has_run_id": true,
        "profile": {
          "term": "xterm-256color", "colorterm": null, "term_program": null,
          "term_program_version": null, "rows": 24, "cols": 80,
          "pixel_width": 0, "pixel_height": 0
        },
        "terminal_history": "bounded output"
      }
    }],
    "launchers": [{
      "id": "l1", "name": "editor", "cwd": "/tmp/project", "command": ["editor"]
    }],
    "agents": [{
      "id": "a1", "shell_id": "s1", "run_id": "r1", "name": "agent",
      "integration": "opencode", "external_session_id": "external-1",
      "cwd": "/tmp/project", "started_at_ms": 1, "ended_at_ms": null,
      "observation": {
        "revision": 2, "state": "blocked", "authority": "lifecycle_integration",
        "evidence": "question", "confidence": 100, "observed_at_ms": 2
      },
      "attention": {
        "reason": "blocked",
        "observation": {
          "revision": 2, "state": "blocked", "authority": "lifecycle_integration",
          "evidence": "question", "confidence": 100, "observed_at_ms": 2
        }
      }
    }]
  }]
}"#,
        )
        .unwrap();
        let store = StateStore::at(path.clone());

        let migrated = store.load().unwrap().unwrap();

        let workspace = &migrated.workspaces[0];
        assert_eq!(
            workspace.default_cwd.as_deref(),
            Some(Path::new("/tmp/project"))
        );
        assert_eq!(
            workspace.shells[0]
                .last_run
                .as_ref()
                .and_then(|run| run.terminal_history.as_deref()),
            Some("bounded output")
        );
        assert_eq!(workspace.launchers[0].command, ["editor"]);
        assert_eq!(
            workspace.agents[0].external_session_id.as_deref(),
            Some("external-1")
        );
        assert!(workspace.agents[0].attention.is_some());
        assert!(workspace.schedules.is_empty());
        assert!(
            fs::read_to_string(&path)
                .unwrap()
                .contains("\"version\": 13")
        );
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn migrates_version_nine_shell_ownership_and_empty_execution_history() {
        let directory = env::temp_dir().join(format!("boomux-state-{}", Uuid::new_v4()));
        let path = directory.join("boomux/state.json");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(
            &path,
            r#"{
              "version": 9,
              "workspaces": [{
                "id": "workspace-1", "name": "saved", "default_cwd": null,
                "shells": [{"id":"shell-1","name":"shell","cwd":"/tmp","command":[],"last_run":null}],
                "launchers": [], "agents": [],
                "schedules": [{
                  "id":"schedule-1","name":"review","cwd":"/tmp","integration":"opencode",
                  "prompt":"private prompt","session":"fresh",
                  "trigger":{"cron":"0 9 * * *","timezone":"UTC"},"state":"paused",
                  "overlap_policy":"skip","revision":1,"prompt_revision":1,"trigger_revision":1,
                  "created_at_ms":1,"updated_at_ms":1,"evaluation_frontier_ms":1,
                  "execution_shell_id":null
                }]
              }]
            }"#,
        )
        .unwrap();

        let migrated = StateStore::at(path.clone()).load().unwrap().unwrap();
        assert_eq!(migrated.workspaces[0].shells[0].owner, ShellOwner::User);
        assert!(migrated.workspaces[0].schedules[0].executions.is_empty());
        assert!(
            fs::read_to_string(&path)
                .unwrap()
                .contains("\"version\": 13")
        );
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn migrates_version_ten_frontier_revision_and_manual_occurrence_fields() {
        let directory = env::temp_dir().join(format!("boomux-state-{}", Uuid::new_v4()));
        let path = directory.join("boomux/state.json");
        let dispatch_key = Uuid::new_v4().to_string();
        let mut state = PersistedState::default();
        state.workspaces.push(PersistedWorkspace {
            id: Uuid::new_v4().to_string(),
            revision: 1,
            name: "saved".into(),
            default_cwd: None,
            shells: Vec::new(),
            launchers: Vec::new(),
            agents: Vec::new(),
            schedules: vec![PersistedAgentSchedule {
                id: Uuid::new_v4().to_string(),
                name: "review".into(),
                cwd: "/tmp".into(),
                integration: "opencode".into(),
                prompt: "private".into(),
                session: AgentScheduleSession::Fresh,
                trigger: AgentScheduleTrigger {
                    cron: "0 9 * * *".into(),
                    timezone: "UTC".into(),
                },
                state: AgentScheduleState::Paused,
                overlap_policy: AgentScheduleOverlapPolicy::Skip,
                revision: 3,
                prompt_revision: 2,
                trigger_revision: 3,
                created_at_ms: 1,
                updated_at_ms: 2,
                evaluation_frontier_ms: 4,
                evaluation_frontier_trigger_revision: 3,
                execution_shell_id: None,
                dispatch_key_filter: vec![0; DISPATCH_KEY_FILTER_BYTES],
                executions: vec![PersistedScheduledExecution {
                    id: Uuid::new_v4().to_string(),
                    revision: 1,
                    state: ScheduledExecutionState::DispatchFailed,
                    dispatch_kind: ScheduledExecutionDispatchKind::Manual,
                    dispatch_key,
                    schedule_revision: 3,
                    prompt_revision: 2,
                    trigger_revision: 3,
                    requested_at_ms: 3,
                    scheduled_at_ms: None,
                    coalesced_through_ms: None,
                    started_at_ms: None,
                    ended_at_ms: Some(3),
                    cwd: "/tmp".into(),
                    integration: "opencode".into(),
                    session: AgentScheduleSession::Fresh,
                    prompt: "private".into(),
                    runner_token: Uuid::new_v4().to_string(),
                    reason: Some(ScheduledExecutionReason::RunnerStartFailed),
                    outcome: None,
                    shell_id: None,
                    run_id: None,
                    agent_id: None,
                    external_session_id: None,
                }],
            }],
        });
        let mut value = serde_json::to_value(&state).unwrap();
        value["version"] = serde_json::Value::from(10);
        let schedule = &mut value["workspaces"][0]["schedules"][0];
        schedule
            .as_object_mut()
            .unwrap()
            .remove("evaluation_frontier_trigger_revision");
        let execution = &mut schedule["executions"][0];
        execution.as_object_mut().unwrap().remove("scheduled_at_ms");
        execution
            .as_object_mut()
            .unwrap()
            .remove("coalesced_through_ms");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();

        let migrated = StateStore::at(path.clone()).load().unwrap().unwrap();
        let schedule = &migrated.workspaces[0].schedules[0];
        assert_eq!(schedule.evaluation_frontier_trigger_revision, 3);
        assert_eq!(schedule.executions[0].scheduled_at_ms, None);
        assert_eq!(schedule.executions[0].coalesced_through_ms, None);
        assert_eq!(schedule.executions[0].revision, 1);
        assert!(
            fs::read_to_string(&path)
                .unwrap()
                .contains("\"version\": 13")
        );

        let mut version_eleven = serde_json::to_value(&migrated).unwrap();
        version_eleven["version"] = serde_json::Value::from(11);
        version_eleven["workspaces"][0]["schedules"][0]["executions"][0]
            .as_object_mut()
            .unwrap()
            .remove("revision");
        fs::write(&path, serde_json::to_vec(&version_eleven).unwrap()).unwrap();
        let migrated_eleven = StateStore::at(path.clone()).load().unwrap().unwrap();
        assert_eq!(
            migrated_eleven.workspaces[0].schedules[0].executions[0].revision,
            1
        );
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn migrates_version_twelve_resource_revisions() {
        let directory = env::temp_dir().join(format!("boomux-state-{}", Uuid::new_v4()));
        let path = directory.join("boomux/state.json");
        let state = PersistedState {
            version: STATE_VERSION,
            workspaces: vec![PersistedWorkspace {
                id: "workspace".into(),
                revision: 9,
                name: "saved".into(),
                default_cwd: None,
                shells: vec![PersistedShell {
                    id: "shell".into(),
                    revision: 8,
                    name: "main".into(),
                    cwd: "/tmp".into(),
                    command: Vec::new(),
                    owner: ShellOwner::User,
                    last_run: None,
                }],
                launchers: vec![PersistedWorkspaceLauncher {
                    id: "launcher".into(),
                    revision: 7,
                    name: "build".into(),
                    cwd: "/tmp".into(),
                    command: vec!["true".into()],
                }],
                agents: Vec::new(),
                schedules: Vec::new(),
            }],
        };
        let mut value = serde_json::to_value(state).unwrap();
        value["version"] = serde_json::Value::from(12);
        value["workspaces"][0]
            .as_object_mut()
            .unwrap()
            .remove("revision");
        value["workspaces"][0]["shells"][0]
            .as_object_mut()
            .unwrap()
            .remove("revision");
        value["workspaces"][0]["launchers"][0]
            .as_object_mut()
            .unwrap()
            .remove("revision");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();

        let migrated = StateStore::at(path.clone()).load().unwrap().unwrap();
        assert_eq!(migrated.workspaces[0].revision, 1);
        assert_eq!(migrated.workspaces[0].shells[0].revision, 1);
        assert_eq!(migrated.workspaces[0].launchers[0].revision, 1);
        assert!(
            fs::read_to_string(path)
                .unwrap()
                .contains("\"version\": 13")
        );
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn rejects_malformed_current_schedule_schema() {
        let schedule_missing_frontier = r#"{
          "id":"schedule-1","name":"review","cwd":"/tmp/project",
          "integration":"opencode","prompt":"Review changes","session":"fresh",
          "trigger":{"cron":"0 9 * * 1-5","timezone":"UTC"},"state":"paused",
          "overlap_policy":"skip","revision":1,"prompt_revision":1,
          "trigger_revision":1,"created_at_ms":1,"updated_at_ms":1,
          "execution_shell_id":null
        }"#;
        let schedule_with_unknown_field = r#"{
          "id":"schedule-1","name":"review","cwd":"/tmp/project",
          "integration":"opencode","prompt":"Review changes","session":"fresh",
          "trigger":{"cron":"0 9 * * 1-5","timezone":"UTC"},"state":"paused",
          "overlap_policy":"skip","revision":1,"prompt_revision":1,
          "trigger_revision":1,"created_at_ms":1,"updated_at_ms":1,
          "evaluation_frontier_ms":1,"execution_shell_id":null,"workspace_id":"w1"
        }"#;
        let schedule_with_unknown_trigger_field = r#"{
          "id":"schedule-1","name":"review","cwd":"/tmp/project",
          "integration":"opencode","prompt":"Review changes","session":"fresh",
          "trigger":{"cron":"0 9 * * 1-5","timezone":"UTC","private":"ignored"},
          "state":"paused","overlap_policy":"skip","revision":1,"prompt_revision":1,
          "trigger_revision":1,"created_at_ms":1,"updated_at_ms":1,
          "evaluation_frontier_ms":1,"execution_shell_id":null
        }"#;

        for schedule in [
            schedule_missing_frontier,
            schedule_with_unknown_field,
            schedule_with_unknown_trigger_field,
        ] {
            let directory = env::temp_dir().join(format!("boomux-state-{}", Uuid::new_v4()));
            let path = directory.join("boomux/state.json");
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(
                &path,
                format!(
                    r#"{{"version":10,"workspaces":[{{"id":"w1","name":"saved","default_cwd":null,"shells":[],"launchers":[],"agents":[],"schedules":[{schedule}]}}]}}"#
                ),
            )
            .unwrap();

            let error = StateStore::at(path).load().unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::InvalidData);
            fs::remove_dir_all(directory).unwrap();
        }
    }

    #[test]
    fn migrates_version_seven_runs_without_terminal_history() {
        let directory = env::temp_dir().join(format!("boomux-state-{}", Uuid::new_v4()));
        let path = directory.join("boomux/state.json");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(
            &path,
            br#"{
  "version": 7,
  "workspaces": [{
    "id": "w1", "name": "version-seven", "default_cwd": "/tmp/project",
    "shells": [{
      "id": "s1", "name": "agent", "cwd": "/tmp/project", "command": ["opencode"],
      "last_run": {
        "id": "r1", "generation": 1, "started_at_ms": 1, "ended_at_ms": 2,
        "exit_reason": {"reason": "interrupted"}, "output_revision": 3,
        "environment_has_run_id": true,
        "profile": {
          "term": "xterm-256color", "colorterm": null, "term_program": null,
          "term_program_version": null, "rows": 24, "cols": 80,
          "pixel_width": 0, "pixel_height": 0
        }
      }
    }],
    "launchers": [], "agents": []
  }]
}"#,
        )
        .unwrap();
        let store = StateStore::at(path.clone());

        let migrated = store.load().unwrap().unwrap();

        assert!(
            migrated.workspaces[0].shells[0]
                .last_run
                .as_ref()
                .unwrap()
                .terminal_history
                .is_none()
        );
        assert!(migrated.workspaces[0].schedules.is_empty());
        assert!(
            fs::read_to_string(&path)
                .unwrap()
                .contains("\"version\": 13")
        );
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn migrates_version_one_runs_once_and_preserves_the_generated_identity() {
        let directory = env::temp_dir().join(format!("boomux-state-{}", Uuid::new_v4()));
        let path = directory.join("boomux/state.json");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(
            &path,
            br#"{
  "version": 1,
  "workspaces": [{
    "id": "5bb1712a-8d4e-4998-bca2-a79aa673ca8f",
    "name": "legacy",
    "shells": [{
      "id": "75dd46b5-6cd3-407a-813a-13e1b10f3614",
      "name": "agent",
      "cwd": "/tmp",
      "command": ["opencode"],
      "last_profile": {
        "term": "xterm-256color",
        "colorterm": null,
        "term_program": null,
        "term_program_version": null,
        "rows": 24,
        "cols": 80,
        "pixel_width": 0,
        "pixel_height": 0
      }
    }, {
      "id": "93ffbd8b-d0e9-444e-a101-c9141abb3848",
      "name": "pending",
      "cwd": "/tmp",
      "command": [],
      "last_profile": null
    }]
  }]
}"#,
        )
        .unwrap();
        let store = StateStore::at(path.clone());

        let migrated = store.load().unwrap().unwrap();
        let run = migrated.workspaces[0].shells[0].last_run.as_ref().unwrap();
        let run_id = run.id.clone();
        assert!(Uuid::parse_str(&run_id).is_ok());
        assert_eq!(run.generation, 1);
        assert!(!run.environment_has_run_id);
        assert_eq!(run.profile.term.as_deref(), Some("xterm-256color"));
        assert!(migrated.workspaces[0].shells[1].last_run.is_none());
        assert!(
            fs::read_to_string(&path)
                .unwrap()
                .contains("\"version\": 13")
        );
        assert!(migrated.workspaces[0].launchers.is_empty());
        assert!(migrated.workspaces[0].agents.is_empty());
        assert!(migrated.workspaces[0].schedules.is_empty());

        let reloaded = store.load().unwrap().unwrap();
        assert_eq!(
            reloaded.workspaces[0].shells[0]
                .last_run
                .as_ref()
                .unwrap()
                .id,
            run_id
        );
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn migrates_version_two_to_empty_launcher_lists() {
        let directory = env::temp_dir().join(format!("boomux-state-{}", Uuid::new_v4()));
        let path = directory.join("boomux/state.json");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(
            &path,
            br#"{
  "version": 2,
  "workspaces": [{
    "id": "5bb1712a-8d4e-4998-bca2-a79aa673ca8f",
    "name": "version-two",
    "shells": [{
      "id": "75dd46b5-6cd3-407a-813a-13e1b10f3614",
      "name": "agent",
      "cwd": "/tmp",
      "command": ["opencode"],
      "last_run": null
    }]
  }]
}"#,
        )
        .unwrap();
        let store = StateStore::at(path.clone());

        let migrated = store.load().unwrap().unwrap();

        assert_eq!(migrated.workspaces[0].name, "version-two");
        assert_eq!(migrated.workspaces[0].shells[0].name, "agent");
        assert!(migrated.workspaces[0].launchers.is_empty());
        assert!(
            fs::read_to_string(&path)
                .unwrap()
                .contains("\"version\": 13")
        );
        assert!(migrated.workspaces[0].agents.is_empty());
        assert!(migrated.workspaces[0].schedules.is_empty());
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn migrates_version_three_to_empty_agent_lists_and_preserves_launchers() {
        let directory = env::temp_dir().join(format!("boomux-state-{}", Uuid::new_v4()));
        let path = directory.join("boomux/state.json");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(
            &path,
            br#"{
  "version": 3,
  "workspaces": [{
    "id": "w1",
    "name": "version-three",
    "shells": [],
    "launchers": [{
      "id": "l1",
      "name": "editor",
      "cwd": "/tmp/project",
      "command": ["editor", "two words"]
    }]
  }]
}"#,
        )
        .unwrap();
        let store = StateStore::at(path.clone());

        let migrated = store.load().unwrap().unwrap();

        assert_eq!(migrated.workspaces[0].launchers[0].id, "l1");
        assert!(migrated.workspaces[0].agents.is_empty());
        assert!(migrated.workspaces[0].schedules.is_empty());
        assert!(
            fs::read_to_string(&path)
                .unwrap()
                .contains("\"version\": 13")
        );
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn migrates_version_four_agent_cwds_from_retained_shells() {
        let directory = env::temp_dir().join(format!("boomux-state-{}", Uuid::new_v4()));
        let path = directory.join("boomux/state.json");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(
            &path,
            br#"{
  "version": 4,
  "workspaces": [{
    "id": "w1",
    "name": "version-four",
    "shells": [{
      "id": "s1",
      "name": "agent",
      "cwd": "/tmp/project",
      "command": [],
      "last_run": null
    }],
    "launchers": [],
    "agents": [{
      "id": "a1",
      "shell_id": "s1",
      "run_id": "r1",
      "name": "pi",
      "integration": "pi",
      "external_session_id": "external-1",
      "started_at_ms": 1,
      "ended_at_ms": null,
      "observation": {
        "revision": 1,
        "state": "inactive",
        "authority": "lifecycle_integration",
        "evidence": "inactive",
        "confidence": 100,
        "observed_at_ms": 2
      }
    }, {
      "id": "a2",
      "shell_id": "removed",
      "run_id": "r2",
      "name": "old",
      "integration": "pi",
      "external_session_id": "external-2",
      "started_at_ms": 1,
      "ended_at_ms": null,
      "observation": {
        "revision": 1,
        "state": "inactive",
        "authority": "lifecycle_integration",
        "evidence": "inactive",
        "confidence": 100,
        "observed_at_ms": 2
      }
    }]
  }]
}"#,
        )
        .unwrap();
        let store = StateStore::at(path.clone());

        let migrated = store.load().unwrap().unwrap();

        assert_eq!(
            migrated.workspaces[0].agents[0].cwd.as_deref(),
            Some(Path::new("/tmp/project"))
        );
        assert!(migrated.workspaces[0].agents[1].cwd.is_none());
        assert!(migrated.workspaces[0].schedules.is_empty());
        assert!(
            fs::read_to_string(&path)
                .unwrap()
                .contains("\"version\": 13")
        );
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn migrates_version_five_agents_without_historical_attention() {
        let directory = env::temp_dir().join(format!("boomux-state-{}", Uuid::new_v4()));
        let path = directory.join("boomux/state.json");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(
            &path,
            br#"{
  "version": 5,
  "workspaces": [{
    "id": "w1", "name": "version-five", "shells": [], "launchers": [],
    "agents": [{
      "id": "a1", "shell_id": "s1", "run_id": "r1", "name": "agent",
      "integration": "test", "external_session_id": null, "cwd": "/tmp/project",
      "started_at_ms": 1, "ended_at_ms": 3,
      "observation": {
        "revision": 2, "state": "done", "authority": "lifecycle_integration",
        "evidence": "completed before upgrade", "confidence": 100, "observed_at_ms": 3
      }
    }]
  }]
}"#,
        )
        .unwrap();
        let store = StateStore::at(path.clone());

        let migrated = store.load().unwrap().unwrap();

        assert!(migrated.workspaces[0].agents[0].attention.is_none());
        assert!(migrated.workspaces[0].schedules.is_empty());
        let saved = fs::read_to_string(&path).unwrap();
        assert!(saved.contains("\"version\": 13"));
        assert!(saved.contains("\"attention\": null"));
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn migrates_version_six_workspaces_without_default_cwds() {
        let directory = env::temp_dir().join(format!("boomux-state-{}", Uuid::new_v4()));
        let path = directory.join("boomux/state.json");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(
            &path,
            br#"{
  "version": 6,
  "workspaces": [{
    "id": "w1", "name": "version-six", "shells": [], "launchers": [], "agents": []
  }]
}"#,
        )
        .unwrap();
        let store = StateStore::at(path.clone());

        let (migrated, deferred) = store.load_deferred().unwrap();
        let migrated = migrated.unwrap();

        assert!(deferred);
        assert!(migrated.workspaces[0].default_cwd.is_none());
        assert!(migrated.workspaces[0].schedules.is_empty());
        let original = fs::read_to_string(&path).unwrap();
        assert!(original.contains("\"version\": 6"));
        assert!(!original.contains("default_cwd"));
        store.save(&migrated).unwrap();
        let saved = fs::read_to_string(&path).unwrap();
        assert!(saved.contains("\"version\": 13"));
        assert!(saved.contains("\"default_cwd\": null"));
        fs::remove_dir_all(directory).unwrap();
    }
}

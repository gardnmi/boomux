use std::env;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::protocol::{
    AgentAttentionSnapshot, AgentObservationSnapshot, ShellRunExitReason, TerminalProfile,
};

const STATE_VERSION: u32 = 6;
const PREVIOUS_STATE_VERSION: u32 = 5;
const VERSION_FOUR_STATE_VERSION: u32 = 4;
const VERSION_THREE_STATE_VERSION: u32 = 3;
const VERSION_TWO_STATE_VERSION: u32 = 2;
const LEGACY_STATE_VERSION: u32 = 1;
const MAX_STATE_BYTES: u64 = 8 * 1024 * 1024;

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
    pub(crate) name: String,
    pub(crate) shells: Vec<PersistedShell>,
    pub(crate) launchers: Vec<PersistedWorkspaceLauncher>,
    pub(crate) agents: Vec<PersistedAgentInstance>,
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
    pub(crate) name: String,
    pub(crate) cwd: PathBuf,
    pub(crate) command: Vec<String>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PersistedShell {
    pub(crate) id: String,
    pub(crate) name: String,
    pub(crate) cwd: PathBuf,
    pub(crate) command: Vec<String>,
    pub(crate) last_run: Option<PersistedShellRun>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
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
}

#[derive(Deserialize)]
struct StateVersion {
    version: u32,
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
    shells: Vec<PersistedShell>,
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
    shells: Vec<PersistedShell>,
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
    shells: Vec<PersistedShell>,
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
    shells: Vec<PersistedShell>,
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
        Self { path, _lock: None }
    }

    pub(crate) fn load(&self) -> io::Result<Option<PersistedState>> {
        let Some(parent) = self.path.parent() else {
            return Err(io::Error::other("state path has no parent"));
        };
        secure_state_dir(parent)?;
        let metadata = match fs::symlink_metadata(&self.path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
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
        let state = match version.version {
            STATE_VERSION => parse_state(&bytes, &self.path)?,
            PREVIOUS_STATE_VERSION => {
                let previous: PreviousPersistedState = parse_state(&bytes, &self.path)?;
                let state = migrate_previous_state(previous);
                self.save(&state)?;
                state
            }
            VERSION_FOUR_STATE_VERSION => {
                let previous: VersionFourPersistedState = parse_state(&bytes, &self.path)?;
                let state = migrate_version_four_state(previous);
                self.save(&state)?;
                state
            }
            VERSION_THREE_STATE_VERSION => {
                let previous: VersionThreePersistedState = parse_state(&bytes, &self.path)?;
                let state = migrate_version_three_state(previous);
                self.save(&state)?;
                state
            }
            VERSION_TWO_STATE_VERSION => {
                let previous: VersionTwoPersistedState = parse_state(&bytes, &self.path)?;
                let state = migrate_version_two_state(previous);
                self.save(&state)?;
                state
            }
            LEGACY_STATE_VERSION => {
                let legacy: LegacyPersistedState = parse_state(&bytes, &self.path)?;
                let state = migrate_legacy_state(legacy);
                self.save(&state)?;
                state
            }
            version => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("unsupported Boomux state version {version}; expected {STATE_VERSION}"),
                ));
            }
        };
        Ok(Some(state))
    }

    pub(crate) fn save(&self, state: &PersistedState) -> io::Result<()> {
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

fn parse_state<T: for<'de> Deserialize<'de>>(bytes: &[u8], path: &Path) -> io::Result<T> {
    serde_json::from_slice(bytes).map_err(|error| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("could not parse {}: {error}", path.display()),
        )
    })
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
                name: workspace.name,
                shells: workspace
                    .shells
                    .into_iter()
                    .map(|shell| PersistedShell {
                        id: shell.id,
                        name: shell.name,
                        cwd: shell.cwd,
                        command: shell.command,
                        last_run: shell.last_profile.map(|profile| PersistedShellRun {
                            id: Uuid::new_v4().to_string(),
                            generation: 1,
                            started_at_ms: migrated_at_ms,
                            ended_at_ms: None,
                            exit_reason: None,
                            output_revision: 0,
                            environment_has_run_id: false,
                            profile,
                        }),
                    })
                    .collect(),
                launchers: Vec::new(),
                agents: Vec::new(),
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
                name: workspace.name,
                shells: workspace.shells,
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
                    name: workspace.name,
                    shells: workspace.shells,
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
                name: workspace.name,
                shells: workspace.shells,
                launchers: workspace.launchers,
                agents: Vec::new(),
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
                name: workspace.name,
                shells: workspace.shells,
                launchers: Vec::new(),
                agents: Vec::new(),
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

pub(crate) fn lock_path_from_environment() -> io::Result<PathBuf> {
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
    Ok(root.join("boomux/daemon.lock"))
}

fn secure_state_dir(path: &Path) -> io::Result<()> {
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

fn effective_uid() -> u32 {
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
                name: "saved".into(),
                shells: Vec::new(),
                launchers: vec![
                    PersistedWorkspaceLauncher {
                        id: "launcher-1".into(),
                        name: "editor".into(),
                        cwd: "/tmp/project".into(),
                        command: vec!["editor".into(), "".into(), "two words".into()],
                    },
                    PersistedWorkspaceLauncher {
                        id: "launcher-2".into(),
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
            }],
        };

        store.save(&state).unwrap();
        let restored = store.load().unwrap().unwrap();

        assert_eq!(restored.workspaces[0].name, "saved");
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
                .contains("\"version\": 6")
        );
        assert!(migrated.workspaces[0].launchers.is_empty());
        assert!(migrated.workspaces[0].agents.is_empty());

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
                .contains("\"version\": 6")
        );
        assert!(migrated.workspaces[0].agents.is_empty());
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
        assert!(
            fs::read_to_string(&path)
                .unwrap()
                .contains("\"version\": 6")
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
        assert!(
            fs::read_to_string(&path)
                .unwrap()
                .contains("\"version\": 6")
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
        let saved = fs::read_to_string(&path).unwrap();
        assert!(saved.contains("\"version\": 6"));
        assert!(saved.contains("\"attention\": null"));
        fs::remove_dir_all(directory).unwrap();
    }
}

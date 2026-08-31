use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use uuid::Uuid;

use boomux::protocol::{
    AgentInstanceSnapshot, AgentObservationSnapshot, AgentState, ShellStatus, Snapshot,
    WorkspaceSnapshot,
};

use crate::host_session_source::normalize_absolute;
use crate::host_session_titles::HostSession;

// Frozen namespace for opaque boomux projected-session IDs.
const SESSION_ID_NAMESPACE: Uuid = Uuid::from_u128(0x8ea7578e_7532_5d53_91f6_1d210f960b48);

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SessionProjection {
    pub(crate) id: String,
    pub(crate) workspace_id: String,
    pub(crate) workspace_name: String,
    pub(crate) integration: String,
    pub(crate) external_session_id: Option<String>,
    pub(crate) description: String,
    pub(crate) user_display_name: Option<String>,
    pub(crate) workspace_revision: u64,
    pub(crate) state: AgentState,
    pub(crate) state_is_current: bool,
    pub(crate) started_at_ms: u64,
    pub(crate) last_at_ms: u64,
    pub(crate) source_cwd: Option<PathBuf>,
    pub(crate) occurrences: Vec<SessionOccurrence>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SessionDisplayNameMetadata {
    pub(crate) workspace_id: String,
    pub(crate) integration: String,
    pub(crate) external_session_id: Option<String>,
    pub(crate) agent_id: Option<String>,
    pub(crate) display_name: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct HiddenSessionMetadata {
    pub(crate) workspace_id: String,
    pub(crate) integration: String,
    pub(crate) external_session_id: Option<String>,
    pub(crate) agent_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SessionOccurrence {
    pub(crate) agent_id: String,
    pub(crate) shell_id: String,
    pub(crate) run_id: String,
    pub(crate) started_at_ms: u64,
    pub(crate) ended_at_ms: Option<u64>,
    pub(crate) observation: AgentObservationSnapshot,
    pub(crate) is_current: bool,
    pub(crate) retained_shell_name: Option<String>,
    pub(crate) retained_shell_cwd: Option<PathBuf>,
    pub(crate) source_cwd: Option<PathBuf>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ResolveError {
    NotFound,
    DuplicateId,
}

#[cfg(test)]
pub(crate) fn project_workspaces(workspaces: &[WorkspaceSnapshot]) -> Vec<SessionProjection> {
    project_workspaces_with_catalog(workspaces, None)
}

pub(crate) fn project_snapshot_with_catalog(
    snapshot: &Snapshot,
    catalog: Option<&[HostSession]>,
) -> Vec<SessionProjection> {
    project_workspaces_with_catalog(&snapshot.workspaces, catalog)
}

pub(crate) fn project_workspaces_with_catalog(
    workspaces: &[WorkspaceSnapshot],
    catalog: Option<&[HostSession]>,
) -> Vec<SessionProjection> {
    let mut sessions = workspaces
        .iter()
        .flat_map(project_workspace)
        .collect::<Vec<_>>();
    if let Some(catalog) = catalog {
        merge_catalog(workspaces, &mut sessions, catalog);
    }
    sessions.sort_by(|left, right| {
        right
            .last_at_ms
            .cmp(&left.last_at_ms)
            .then_with(|| left.workspace_id.cmp(&right.workspace_id))
            .then_with(|| left.id.cmp(&right.id))
    });
    sessions
}

pub(crate) fn resolve_exact<'a>(
    sessions: &'a [SessionProjection],
    id: &str,
) -> Result<&'a SessionProjection, ResolveError> {
    let mut matches = sessions.iter().filter(|session| session.id == id);
    let session = matches.next().ok_or(ResolveError::NotFound)?;
    if matches.next().is_some() {
        return Err(ResolveError::DuplicateId);
    }
    Ok(session)
}

pub(crate) fn apply_display_names(
    sessions: &mut [SessionProjection],
    metadata: &[SessionDisplayNameMetadata],
) {
    for session in sessions {
        let agent_id = session
            .external_session_id
            .is_none()
            .then(|| {
                session
                    .occurrences
                    .first()
                    .map(|occurrence| occurrence.agent_id.as_str())
            })
            .flatten();
        if let Some(record) = metadata.iter().find(|record| {
            record.workspace_id == session.workspace_id
                && record.integration == session.integration
                && match (&session.external_session_id, &record.external_session_id) {
                    (Some(session_id), Some(record_id)) => session_id == record_id,
                    (None, None) => record.agent_id.as_deref() == agent_id,
                    _ => false,
                }
        }) {
            session.user_display_name = Some(record.display_name.clone());
            session.description = record.display_name.clone();
        }
    }
}

pub(crate) fn filter_hidden(
    sessions: &mut Vec<SessionProjection>,
    metadata: &[HiddenSessionMetadata],
) {
    sessions.retain(|session| {
        let agent_id = session
            .external_session_id
            .is_none()
            .then(|| {
                session
                    .occurrences
                    .first()
                    .map(|occurrence| occurrence.agent_id.as_str())
            })
            .flatten();
        !metadata.iter().any(|record| {
            record.workspace_id == session.workspace_id
                && record.integration == session.integration
                && match (&session.external_session_id, &record.external_session_id) {
                    (Some(session_id), Some(record_id)) => session_id == record_id,
                    (None, None) => record.agent_id.as_deref() == agent_id,
                    _ => false,
                }
        })
    });
}

fn project_workspace(workspace: &WorkspaceSnapshot) -> Vec<SessionProjection> {
    let shells: BTreeMap<_, _> = workspace
        .shells
        .iter()
        .map(|shell| (shell.id.as_str(), shell))
        .collect();
    let current_runs: BTreeMap<_, _> = workspace
        .shells
        .iter()
        .filter(|shell| matches!(shell.status, ShellStatus::Running))
        .filter_map(|shell| {
            shell
                .run
                .as_ref()
                .map(|run| (shell.id.as_str(), run.id.as_str()))
        })
        .collect();
    let mut groups: BTreeMap<(String, String), Vec<&AgentInstanceSnapshot>> = BTreeMap::new();
    for agent in &workspace.agents {
        let identity = match &agent.external_session_id {
            Some(external_id) => format!("external:{external_id}"),
            None => format!("instance:{}", agent.id),
        };
        groups
            .entry((agent.integration.clone(), identity))
            .or_default()
            .push(agent);
    }

    groups
        .into_iter()
        .map(|((integration, identity), mut agents)| {
            agents.sort_by(|left, right| {
                left.started_at_ms
                    .cmp(&right.started_at_ms)
                    .then_with(|| left.id.cmp(&right.id))
            });
            let first = agents[0];
            let latest_agent = agents
                .iter()
                .copied()
                .max_by(|left, right| {
                    left.started_at_ms
                        .cmp(&right.started_at_ms)
                        .then_with(|| left.id.cmp(&right.id))
                })
                .expect("session group is non-empty");
            let latest_observation = agents
                .iter()
                .copied()
                .max_by(|left, right| {
                    left.observation
                        .observed_at_ms
                        .cmp(&right.observation.observed_at_ms)
                        .then_with(|| left.started_at_ms.cmp(&right.started_at_ms))
                        .then_with(|| left.id.cmp(&right.id))
                })
                .expect("session group is non-empty");
            let active = agents
                .iter()
                .copied()
                .filter(|agent| occurrence_is_current(&workspace.id, &current_runs, agent));
            let (state_source, state_is_current) = active
                .min_by(|left, right| {
                    state_priority(left.observation.state)
                        .cmp(&state_priority(right.observation.state))
                        .then_with(|| {
                            right
                                .observation
                                .observed_at_ms
                                .cmp(&left.observation.observed_at_ms)
                        })
                        .then_with(|| left.id.cmp(&right.id))
                })
                .map_or((latest_observation, false), |active| (active, true));
            let last_at_ms = agents
                .iter()
                .map(|agent| {
                    agent
                        .ended_at_ms
                        .unwrap_or(agent.observation.observed_at_ms)
                        .max(agent.observation.observed_at_ms)
                })
                .max()
                .unwrap_or(first.started_at_ms);
            let occurrences: Vec<SessionOccurrence> = agents
                .into_iter()
                .map(|agent| {
                    let retained_shell = shells.get(agent.shell_id.as_str()).copied();
                    SessionOccurrence {
                        agent_id: agent.id.clone(),
                        shell_id: agent.shell_id.clone(),
                        run_id: agent.run_id.clone(),
                        started_at_ms: agent.started_at_ms,
                        ended_at_ms: agent.ended_at_ms,
                        observation: agent.observation.clone(),
                        is_current: occurrence_is_current(&workspace.id, &current_runs, agent),
                        retained_shell_name: retained_shell.map(|shell| shell.name.clone()),
                        retained_shell_cwd: retained_shell.map(|shell| shell.cwd.clone()),
                        source_cwd: agent
                            .cwd
                            .clone()
                            .or_else(|| retained_shell.map(|shell| shell.cwd.clone())),
                    }
                })
                .collect();
            let source_cwd = agents_source_cwd(&occurrences);

            SessionProjection {
                id: stable_session_id(&workspace.id, &integration, &identity),
                workspace_id: workspace.id.clone(),
                workspace_name: workspace.name.clone(),
                integration,
                external_session_id: first.external_session_id.clone(),
                description: latest_agent.name.clone(),
                user_display_name: None,
                workspace_revision: workspace.revision,
                state: state_source.observation.state,
                state_is_current,
                started_at_ms: first.started_at_ms,
                last_at_ms,
                source_cwd,
                occurrences,
            }
        })
        .collect()
}

fn agents_source_cwd(occurrences: &[SessionOccurrence]) -> Option<PathBuf> {
    occurrences
        .iter()
        .rev()
        .find_map(|occurrence| occurrence.source_cwd.clone())
}

fn merge_catalog(
    workspaces: &[WorkspaceSnapshot],
    sessions: &mut Vec<SessionProjection>,
    catalog: &[HostSession],
) {
    let mut workspaces_by_directory: BTreeMap<PathBuf, Vec<usize>> = BTreeMap::new();
    for (index, workspace) in workspaces.iter().enumerate() {
        for directory in workspace_directories(workspace) {
            workspaces_by_directory
                .entry(directory)
                .or_default()
                .push(index);
        }
    }
    let mut sessions_by_identity = BTreeMap::new();
    for (index, session) in sessions.iter().enumerate() {
        if let Some(external_id) = &session.external_session_id {
            sessions_by_identity
                .entry((
                    session.workspace_id.clone(),
                    session.integration.clone(),
                    external_id.clone(),
                ))
                .or_insert(index);
        }
    }

    for record in catalog {
        let Some(record_directory) = normalize_absolute(&record.directory) else {
            continue;
        };
        let Some(matching_workspaces) = workspaces_by_directory.get(&record_directory) else {
            continue;
        };
        for &workspace_index in matching_workspaces {
            let workspace = &workspaces[workspace_index];
            let identity = format!("external:{}", record.root_id);
            let key = (
                workspace.id.clone(),
                record.integration.clone(),
                record.root_id.clone(),
            );
            if let Some(&session_index) = sessions_by_identity.get(&key) {
                let session = &mut sessions[session_index];
                session.description = record.title.clone();
                if session.source_cwd.is_none() {
                    session.source_cwd = Some(record_directory.clone());
                }
                continue;
            }

            if !boomux::integrations::by_key(&record.integration)
                .and_then(|descriptor| descriptor.titles)
                .is_some_and(|titles| titles.provides_catalog)
            {
                continue;
            }

            let session_index = sessions.len();
            sessions.push(SessionProjection {
                id: stable_session_id(&workspace.id, &record.integration, &identity),
                workspace_id: workspace.id.clone(),
                workspace_name: workspace.name.clone(),
                integration: record.integration.clone(),
                external_session_id: Some(record.root_id.clone()),
                description: record.title.clone(),
                user_display_name: None,
                workspace_revision: workspace.revision,
                state: AgentState::Unknown,
                state_is_current: false,
                started_at_ms: record.created_at_ms,
                last_at_ms: record.updated_at_ms,
                source_cwd: Some(record_directory.clone()),
                occurrences: Vec::new(),
            });
            sessions_by_identity.insert(key, session_index);
        }
    }
}

fn workspace_directories(workspace: &WorkspaceSnapshot) -> BTreeSet<PathBuf> {
    workspace
        .shells
        .iter()
        .map(|shell| shell.cwd.as_path())
        .chain(
            workspace
                .launchers
                .iter()
                .map(|launcher| launcher.cwd.as_path()),
        )
        .chain(
            workspace
                .agents
                .iter()
                .filter_map(|agent| agent.cwd.as_deref()),
        )
        .filter_map(normalized_directory)
        .collect()
}

pub(crate) fn catalog_directories(workspaces: &[WorkspaceSnapshot]) -> BTreeSet<PathBuf> {
    workspaces.iter().flat_map(workspace_directories).collect()
}

fn normalized_directory(directory: &Path) -> Option<PathBuf> {
    normalize_absolute(directory)
}

pub(crate) fn agent_is_active_for_run(
    agent: &AgentInstanceSnapshot,
    shell_id: &str,
    run_id: &str,
) -> bool {
    agent.shell_id == shell_id
        && agent.run_id == run_id
        && agent.ended_at_ms.is_none()
        && !matches!(
            agent.observation.state,
            AgentState::Inactive | AgentState::Done
        )
}

fn occurrence_is_current(
    workspace_id: &str,
    current_runs: &BTreeMap<&str, &str>,
    agent: &AgentInstanceSnapshot,
) -> bool {
    agent.workspace_id == workspace_id
        && current_runs
            .get(agent.shell_id.as_str())
            .is_some_and(|run_id| agent_is_active_for_run(agent, &agent.shell_id, run_id))
}

fn stable_session_id(workspace_id: &str, integration: &str, identity: &str) -> String {
    // UUID v5 hashes a version tag followed by u64-length-prefixed UTF-8 fields.
    let mut name = b"boomux.session/v1".to_vec();
    for value in [workspace_id, integration, identity] {
        name.extend_from_slice(&(value.len() as u64).to_be_bytes());
        name.extend_from_slice(value.as_bytes());
    }
    Uuid::new_v5(&SESSION_ID_NAMESPACE, &name).to_string()
}

#[cfg(any(test, feature = "benchmark-internals"))]
pub mod benchmark_support {
    use super::*;
    use boomux::protocol::{AgentAuthority, ShellRunSnapshot, ShellSnapshot};

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub struct SessionSummary {
        pub sessions: usize,
        pub occurrences: usize,
        pub current: usize,
        pub checksum: u64,
    }

    pub struct SessionFixture {
        workspaces: Vec<WorkspaceSnapshot>,
        catalog: Vec<HostSession>,
    }

    pub struct SessionProjectionResult(Vec<SessionProjection>);

    impl SessionProjectionResult {
        pub fn summary(&self) -> SessionSummary {
            SessionSummary {
                sessions: self.0.len(),
                occurrences: self.0.iter().map(|session| session.occurrences.len()).sum(),
                current: self
                    .0
                    .iter()
                    .filter(|session| session.state_is_current)
                    .count(),
                checksum: self.0.iter().fold(0, checksum_session),
            }
        }
    }

    impl SessionFixture {
        pub fn durable(
            workspace_count: usize,
            shells_per_workspace: usize,
            agents_per_shell: usize,
        ) -> Self {
            let workspaces = (0..workspace_count)
                .map(|workspace_index| {
                    let workspace_id = format!("workspace-{workspace_index}");
                    let directory = PathBuf::from(format!("/benchmark/{workspace_id}"));
                    let shells = (0..shells_per_workspace)
                        .map(|shell_index| {
                            shell(
                                &workspace_id,
                                shell_index,
                                directory.clone(),
                                agents_per_shell,
                            )
                        })
                        .collect::<Vec<_>>();
                    let agents = shells
                        .iter()
                        .flat_map(|shell| {
                            (0..agents_per_shell).map(|agent_index| {
                                agent(&workspace_id, shell, agent_index, directory.clone())
                            })
                        })
                        .collect();
                    WorkspaceSnapshot {
                        id: workspace_id.clone(),
                        revision: 1,
                        name: workspace_id,
                        default_cwd: Some(directory),
                        shells,
                        launchers: Vec::new(),
                        agents,
                    }
                })
                .collect();
            Self {
                workspaces,
                catalog: Vec::new(),
            }
        }

        pub fn catalog(workspace_count: usize, record_count: usize, shared: bool) -> Self {
            let shared_directory = PathBuf::from("/benchmark/shared");
            let workspaces = (0..workspace_count)
                .map(|workspace_index| {
                    let workspace_id = format!("workspace-{workspace_index}");
                    let directory = if shared {
                        shared_directory.clone()
                    } else {
                        PathBuf::from(format!("/benchmark/{workspace_id}"))
                    };
                    WorkspaceSnapshot {
                        id: workspace_id.clone(),
                        revision: 1,
                        name: workspace_id.clone(),
                        default_cwd: Some(directory.clone()),
                        shells: vec![shell(&workspace_id, 0, directory, 0)],
                        launchers: Vec::new(),
                        agents: Vec::new(),
                    }
                })
                .collect::<Vec<_>>();
            let catalog = (0..record_count)
                .map(|record_index| HostSession {
                    integration: "opencode".into(),
                    root_id: format!("catalog-{record_index}"),
                    title: format!("Catalog session {record_index}"),
                    directory: if shared {
                        shared_directory.clone()
                    } else {
                        PathBuf::from(format!(
                            "/benchmark/workspace-{}",
                            record_index % workspace_count
                        ))
                    },
                    created_at_ms: record_index as u64,
                    updated_at_ms: record_index as u64 + 1,
                })
                .collect();
            Self {
                workspaces,
                catalog,
            }
        }

        pub fn project(&self) -> SessionProjectionResult {
            SessionProjectionResult(project_workspaces_with_catalog(
                &self.workspaces,
                (!self.catalog.is_empty()).then_some(self.catalog.as_slice()),
            ))
        }
    }

    fn shell(
        workspace_id: &str,
        shell_index: usize,
        directory: PathBuf,
        agents_per_shell: usize,
    ) -> ShellSnapshot {
        let shell_id = format!("{workspace_id}-shell-{shell_index}");
        ShellSnapshot {
            id: shell_id.clone(),
            revision: 1,
            workspace_id: workspace_id.into(),
            name: format!("shell-{shell_index}"),
            cwd: directory,
            command: Vec::new(),
            status: ShellStatus::Running,
            run: Some(ShellRunSnapshot {
                id: format!("{shell_id}-run"),
                generation: 1,
                started_at_ms: 1,
                ended_at_ms: None,
                exit_reason: None,
                output_revision: agents_per_shell as u64,
                environment_has_run_id: true,
            }),
            recovered_agent_id: None,
            foreground_process: None,
        }
    }

    fn agent(
        workspace_id: &str,
        shell: &ShellSnapshot,
        agent_index: usize,
        directory: PathBuf,
    ) -> AgentInstanceSnapshot {
        let run_id = shell
            .run
            .as_ref()
            .expect("benchmark shell has a run")
            .id
            .clone();
        let id = format!("{}-agent-{agent_index}", shell.id);
        AgentInstanceSnapshot {
            id: id.clone(),
            workspace_id: workspace_id.into(),
            shell_id: shell.id.clone(),
            run_id,
            name: id.clone(),
            integration: "opencode".into(),
            external_session_id: Some(id),
            cwd: Some(directory),
            started_at_ms: agent_index as u64 + 1,
            ended_at_ms: None,
            observation: AgentObservationSnapshot {
                revision: 1,
                state: if agent_index.is_multiple_of(2) {
                    AgentState::Working
                } else {
                    AgentState::Idle
                },
                authority: AgentAuthority::LifecycleIntegration,
                evidence: "benchmark fixture".into(),
                confidence: 100,
                observed_at_ms: agent_index as u64 + 2,
            },
            attention: None,
            working_contexts: Vec::new(),
        }
    }

    fn checksum_session(mut checksum: u64, session: &SessionProjection) -> u64 {
        checksum = checksum_string(checksum, &session.id);
        checksum = checksum_string(checksum, &session.workspace_id);
        checksum = checksum_string(checksum, &session.workspace_name);
        checksum = checksum_string(checksum, &session.integration);
        checksum = checksum_optional_string(checksum, session.external_session_id.as_deref());
        checksum = checksum_string(checksum, &session.description);
        checksum = checksum_agent_state(checksum, session.state);
        checksum = checksum_value(checksum, u64::from(session.state_is_current));
        checksum = checksum_value(checksum, session.started_at_ms);
        checksum = checksum_value(checksum, session.last_at_ms);
        checksum = checksum_optional_path(checksum, session.source_cwd.as_deref());
        checksum = checksum_value(checksum, session.occurrences.len() as u64);
        for occurrence in &session.occurrences {
            checksum = checksum_string(checksum, &occurrence.agent_id);
            checksum = checksum_string(checksum, &occurrence.shell_id);
            checksum = checksum_string(checksum, &occurrence.run_id);
            checksum = checksum_value(checksum, occurrence.started_at_ms);
            checksum = checksum_optional_u64(checksum, occurrence.ended_at_ms);
            checksum = checksum_value(checksum, occurrence.observation.revision);
            checksum = checksum_agent_state(checksum, occurrence.observation.state);
            checksum = checksum_authority(checksum, occurrence.observation.authority);
            checksum = checksum_string(checksum, &occurrence.observation.evidence);
            checksum = checksum_value(checksum, u64::from(occurrence.observation.confidence));
            checksum = checksum_value(checksum, occurrence.observation.observed_at_ms);
            checksum = checksum_value(checksum, u64::from(occurrence.is_current));
            checksum =
                checksum_optional_string(checksum, occurrence.retained_shell_name.as_deref());
            checksum = checksum_optional_path(checksum, occurrence.retained_shell_cwd.as_deref());
            checksum = checksum_optional_path(checksum, occurrence.source_cwd.as_deref());
        }
        checksum
    }

    fn checksum_string(checksum: u64, value: &str) -> u64 {
        let mut checksum = checksum_value(checksum, value.len() as u64);
        for byte in value.bytes() {
            checksum = checksum_value(checksum, u64::from(byte));
        }
        checksum
    }

    fn checksum_optional_string(checksum: u64, value: Option<&str>) -> u64 {
        match value {
            Some(value) => checksum_string(checksum_value(checksum, 1), value),
            None => checksum_value(checksum, 0),
        }
    }

    fn checksum_optional_path(checksum: u64, value: Option<&Path>) -> u64 {
        match value {
            Some(value) => checksum_string(
                checksum_value(checksum, 1),
                value.to_string_lossy().as_ref(),
            ),
            None => checksum_value(checksum, 0),
        }
    }

    fn checksum_optional_u64(checksum: u64, value: Option<u64>) -> u64 {
        match value {
            Some(value) => checksum_value(checksum_value(checksum, 1), value),
            None => checksum_value(checksum, 0),
        }
    }

    fn checksum_agent_state(checksum: u64, state: AgentState) -> u64 {
        checksum_value(
            checksum,
            match state {
                AgentState::Unknown => 0,
                AgentState::Working => 1,
                AgentState::Blocked => 2,
                AgentState::Idle => 3,
                AgentState::Inactive => 4,
                AgentState::Done => 5,
            },
        )
    }

    fn checksum_authority(checksum: u64, authority: AgentAuthority) -> u64 {
        checksum_value(
            checksum,
            match authority {
                AgentAuthority::LifecycleIntegration => 0,
                AgentAuthority::ProcessAdapter => 1,
                AgentAuthority::TerminalHeuristic => 2,
                AgentAuthority::DaemonLifecycle => 3,
            },
        )
    }

    fn checksum_value(checksum: u64, value: u64) -> u64 {
        checksum.wrapping_mul(0x100_0000_01b3).wrapping_add(value)
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn benchmark_session_fixtures_have_stable_complete_summaries() {
            let durable = SessionFixture::durable(64, 8, 2);
            let summary = durable.project().summary();
            assert_eq!(
                (summary.sessions, summary.occurrences, summary.current),
                (1_024, 1_024, 1_024)
            );
            assert_ne!(summary.checksum, 0);
            assert_eq!(summary.checksum, 1_011_118_191_847_967_550);
            assert_eq!(durable.project().summary(), summary);

            let unique = SessionFixture::catalog(64, 400, false).project().summary();
            assert_eq!(unique.sessions, 400);
            assert_ne!(unique.checksum, 0);
            assert_eq!(unique.checksum, 8_601_524_830_593_867_508);

            let shared = SessionFixture::catalog(32, 400, true).project().summary();
            assert_eq!((shared.sessions, shared.occurrences), (12_800, 0));
            assert_ne!(shared.checksum, 0);
            assert_eq!(shared.checksum, 6_725_822_761_224_353_157);
        }
    }
}

fn state_priority(state: AgentState) -> u8 {
    match state {
        AgentState::Blocked => 0,
        AgentState::Working => 1,
        AgentState::Idle => 2,
        AgentState::Inactive => 3,
        AgentState::Done => 4,
        AgentState::Unknown => 5,
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;
    use boomux::protocol::{AgentAuthority, ShellRunSnapshot};

    fn catalog_session(id: &str, directory: &str) -> HostSession {
        HostSession {
            integration: "opencode".into(),
            root_id: id.into(),
            title: format!("Catalog {id}"),
            directory: PathBuf::from(directory),
            created_at_ms: 5,
            updated_at_ms: 50,
        }
    }

    fn title_only_record(integration: &str, id: &str, directory: &str) -> HostSession {
        HostSession {
            integration: integration.into(),
            root_id: id.into(),
            title: format!("Host {id}"),
            directory: PathBuf::from(directory),
            created_at_ms: 0,
            updated_at_ms: 0,
        }
    }

    fn workspace(id: &str, agent_ids: &[&str]) -> WorkspaceSnapshot {
        let shell = boomux::protocol::ShellSnapshot {
            id: "shell".into(),
            revision: 1,
            workspace_id: id.into(),
            name: "agent".into(),
            cwd: "/tmp/project".into(),
            command: Vec::new(),
            status: ShellStatus::Running,
            run: Some(ShellRunSnapshot {
                id: "run".into(),
                generation: 1,
                started_at_ms: 1,
                ended_at_ms: None,
                exit_reason: None,
                output_revision: 0,
                environment_has_run_id: true,
            }),
            recovered_agent_id: None,
            foreground_process: None,
        };
        WorkspaceSnapshot {
            id: id.into(),
            revision: 1,
            name: format!("workspace-{id}"),
            default_cwd: None,
            shells: vec![shell],
            launchers: Vec::new(),
            agents: agent_ids
                .iter()
                .enumerate()
                .map(|(index, agent_id)| AgentInstanceSnapshot {
                    id: (*agent_id).into(),
                    workspace_id: id.into(),
                    shell_id: "shell".into(),
                    run_id: "run".into(),
                    name: format!("Agent {index}"),
                    integration: "opencode".into(),
                    external_session_id: Some("external".into()),
                    cwd: Some("/tmp/project".into()),
                    started_at_ms: 10 + index as u64,
                    ended_at_ms: None,
                    attention: None,
                    observation: AgentObservationSnapshot {
                        revision: 1,
                        state: AgentState::Working,
                        authority: AgentAuthority::LifecycleIntegration,
                        evidence: "working".into(),
                        confidence: 100,
                        observed_at_ms: 20 + index as u64,
                    },
                    working_contexts: Vec::new(),
                })
                .collect(),
        }
    }

    #[test]
    fn user_display_name_precedes_catalog_title_and_is_workspace_scoped() {
        let left = workspace("left", &["agent-left", "agent-left-new"]);
        let right = workspace("right", &["agent-right"]);
        let catalog = [catalog_session("external", "/tmp/project")];
        let mut sessions = project_workspaces_with_catalog(&[left, right], Some(&catalog));
        assert!(
            sessions
                .iter()
                .all(|session| session.description == "Catalog external")
        );

        apply_display_names(
            &mut sessions,
            &[SessionDisplayNameMetadata {
                workspace_id: "left".into(),
                integration: "opencode".into(),
                external_session_id: Some("external".into()),
                agent_id: None,
                display_name: "Checkout retry investigation".into(),
            }],
        );

        let left = sessions
            .iter()
            .find(|session| session.workspace_id == "left")
            .unwrap();
        assert_eq!(left.description, "Checkout retry investigation");
        assert_eq!(
            left.user_display_name.as_deref(),
            Some("Checkout retry investigation")
        );
        let right = sessions
            .iter()
            .find(|session| session.workspace_id == "right")
            .unwrap();
        assert_eq!(right.description, "Catalog external");
        assert!(right.user_display_name.is_none());
    }

    #[test]
    fn title_only_records_enrich_exact_observed_sessions_without_creating_history() {
        for integration in ["pi", "claude", "kiro"] {
            let mut workspace = workspace("title-workspace", &["title-agent"]);
            workspace.agents[0].integration = integration.into();
            workspace.agents[0].external_session_id = Some("observed".into());
            let records = [
                title_only_record(integration, "observed", "/tmp/project"),
                title_only_record(integration, "historical", "/tmp/project"),
            ];

            let sessions = project_workspaces_with_catalog(&[workspace], Some(&records));

            assert_eq!(sessions.len(), 1, "{integration}");
            assert_eq!(sessions[0].external_session_id.as_deref(), Some("observed"));
            assert_eq!(sessions[0].description, "Host observed");
            assert_eq!(sessions[0].started_at_ms, 10);
            assert_eq!(sessions[0].occurrences.len(), 1);
        }
    }

    #[test]
    fn hidden_session_keys_use_external_identity_then_exact_agent_fallback() {
        let external = workspace("external-workspace", &["external-agent"]);
        let mut fallback = workspace("fallback-workspace", &["fallback-agent"]);
        fallback.agents[0].external_session_id = None;
        let mut sessions = project_workspaces_with_catalog(&[external, fallback], None);

        filter_hidden(
            &mut sessions,
            &[HiddenSessionMetadata {
                workspace_id: "external-workspace".into(),
                integration: "opencode".into(),
                external_session_id: Some("external".into()),
                agent_id: None,
            }],
        );
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].workspace_id, "fallback-workspace");

        filter_hidden(
            &mut sessions,
            &[HiddenSessionMetadata {
                workspace_id: "fallback-workspace".into(),
                integration: "opencode".into(),
                external_session_id: None,
                agent_id: Some("fallback-agent".into()),
            }],
        );
        assert!(sessions.is_empty());
    }

    #[test]
    fn fallback_name_uses_latest_agent_identity_not_latest_observation() {
        let mut workspace = workspace("w1", &["older", "newer"]);
        workspace.shells.clear();
        workspace.agents[0].observation.state = AgentState::Blocked;
        workspace.agents[0].observation.observed_at_ms = 100;
        workspace.agents[1].observation.state = AgentState::Idle;
        workspace.agents[1].observation.observed_at_ms = 20;

        let sessions = project_workspaces(&[workspace]);

        assert_eq!(sessions[0].description, "Agent 1");
        assert_eq!(sessions[0].state, AgentState::Blocked);
        assert!(!sessions[0].state_is_current);
    }

    #[test]
    fn active_run_match_requires_exact_live_nonterminal_identity() {
        let mut workspace = workspace("w1", &["a1"]);
        let agent = &workspace.agents[0];
        assert!(agent_is_active_for_run(agent, "shell", "run"));
        assert!(!agent_is_active_for_run(agent, "other", "run"));
        assert!(!agent_is_active_for_run(agent, "shell", "other"));

        workspace.agents[0].ended_at_ms = Some(30);
        assert!(!agent_is_active_for_run(
            &workspace.agents[0],
            "shell",
            "run"
        ));
        workspace.agents[0].ended_at_ms = None;
        for state in [AgentState::Inactive, AgentState::Done] {
            workspace.agents[0].observation.state = state;
            assert!(!agent_is_active_for_run(
                &workspace.agents[0],
                "shell",
                "run"
            ));
        }
    }

    #[test]
    fn groups_occurrences_and_resolves_list_id_exactly() {
        let sessions = project_workspaces(&[workspace("w1", &["a1", "a2"])]);
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].occurrences.len(), 2);
        assert_eq!(resolve_exact(&sessions, &sessions[0].id), Ok(&sessions[0]));
        assert_eq!(
            resolve_exact(&sessions, "external"),
            Err(ResolveError::NotFound)
        );
    }

    #[test]
    fn ids_are_globally_unique_across_workspaces() {
        let sessions = project_workspaces(&[workspace("w1", &["a1"]), workspace("w2", &["a2"])]);
        assert_eq!(sessions.len(), 2);
        assert_ne!(sessions[0].id, sessions[1].id);
        assert_eq!(sessions[0].id, "20633f73-82d0-5834-8a27-f11f4c68980d");
    }

    #[test]
    fn global_order_is_newest_then_workspace_and_session_id() {
        let mut older = workspace("w3", &["a2"]);
        older.agents[0].observation.observed_at_ms = 10;
        let mut tied_first = workspace("w1", &["a1"]);
        tied_first.agents[0].observation.observed_at_ms = 30;
        let mut tied_second = workspace("w2", &["a3"]);
        tied_second.agents[0].observation.observed_at_ms = 30;

        let sessions = project_workspaces(&[older, tied_second, tied_first]);
        assert_eq!(
            sessions
                .iter()
                .map(|session| session.workspace_id.as_str())
                .collect::<Vec<_>>(),
            ["w1", "w2", "w3"]
        );
        assert_eq!(sessions[2].last_at_ms, 10);
    }

    #[test]
    fn duplicate_ids_are_rejected_defensively() {
        let mut sessions = project_workspaces(&[workspace("w1", &["a1"])]);
        sessions.push(sessions[0].clone());
        assert_eq!(
            resolve_exact(&sessions, &sessions[0].id),
            Err(ResolveError::DuplicateId)
        );
    }

    #[test]
    fn state_priority_matches_dashboard_attention_order() {
        let states = [
            AgentState::Blocked,
            AgentState::Working,
            AgentState::Idle,
            AgentState::Inactive,
            AgentState::Done,
            AgentState::Unknown,
        ];
        assert_eq!(states.map(state_priority), [0, 1, 2, 3, 4, 5]);
    }

    #[test]
    fn source_cwd_survives_shell_removal_without_making_occurrence_current() {
        let mut workspace = workspace("w1", &["a1"]);
        workspace.shells.clear();

        let sessions = project_workspaces(&[workspace]);
        let occurrence = &sessions[0].occurrences[0];

        assert!(!occurrence.is_current);
        assert!(occurrence.retained_shell_name.is_none());
        assert!(occurrence.retained_shell_cwd.is_none());
        assert_eq!(
            occurrence.source_cwd.as_deref(),
            Some(Path::new("/tmp/project"))
        );
    }

    #[test]
    fn retained_shell_cwd_is_a_compatibility_fallback_for_old_daemons() {
        let mut workspace = workspace("w1", &["a1"]);
        workspace.agents[0].cwd = None;

        let sessions = project_workspaces(&[workspace]);

        assert_eq!(
            sessions[0].occurrences[0].source_cwd.as_deref(),
            Some(Path::new("/tmp/project"))
        );
        assert_eq!(
            sessions[0].source_cwd.as_deref(),
            Some(Path::new("/tmp/project"))
        );
    }

    #[test]
    fn catalog_only_session_requires_one_exact_normalized_workspace_match() {
        let mut matching = workspace("w1", &[]);
        matching.shells[0].cwd = "/tmp/project/./src/..".into();
        let mut unmatched = workspace("w2", &[]);
        unmatched.shells[0].cwd = "/other/project".into();
        let catalog = [catalog_session("catalog-only", "/tmp/project")];

        let sessions = project_workspaces_with_catalog(&[matching, unmatched], Some(&catalog));

        assert_eq!(sessions.len(), 1);
        let session = &sessions[0];
        assert_eq!(session.workspace_id, "w1");
        assert_eq!(session.external_session_id.as_deref(), Some("catalog-only"));
        assert_eq!(session.description, "Catalog catalog-only");
        assert_eq!(session.state, AgentState::Unknown);
        assert!(!session.state_is_current);
        assert!(session.occurrences.is_empty());
        assert_eq!(
            session.source_cwd.as_deref(),
            Some(Path::new("/tmp/project"))
        );
        assert_eq!(session.started_at_ms, 5);
        assert_eq!(session.last_at_ms, 50);
    }

    #[test]
    fn catalog_association_uses_launcher_and_agent_cwds() {
        let mut launcher_match = workspace("launcher", &[]);
        launcher_match.shells.clear();
        launcher_match
            .launchers
            .push(boomux::protocol::WorkspaceLauncherSnapshot {
                id: "launcher".into(),
                revision: 1,
                workspace_id: "launcher".into(),
                name: "build".into(),
                command: vec!["make".into()],
                cwd: "/launcher/repo".into(),
            });
        let mut agent_match = workspace("agent", &["a1"]);
        agent_match.shells.clear();
        agent_match.agents[0].cwd = Some("/agent/repo".into());
        let catalog = [
            catalog_session("from-launcher", "/launcher/repo"),
            catalog_session("from-agent", "/agent/repo"),
        ];

        let sessions =
            project_workspaces_with_catalog(&[launcher_match, agent_match], Some(&catalog));

        assert_eq!(sessions.len(), 3);
        assert!(sessions.iter().any(|session| {
            session.workspace_id == "launcher"
                && session.external_session_id.as_deref() == Some("from-launcher")
        }));
        assert!(sessions.iter().any(|session| {
            session.workspace_id == "agent"
                && session.external_session_id.as_deref() == Some("from-agent")
        }));
    }

    #[test]
    fn shared_catalog_directories_project_into_each_matching_workspace() {
        let first = workspace("w1", &[]);
        let second = workspace("w2", &[]);
        let catalog = [
            catalog_session("ambiguous", "/tmp/project"),
            catalog_session("unmatched", "/elsewhere"),
        ];

        let sessions = project_workspaces_with_catalog(&[first, second], Some(&catalog));
        assert_eq!(sessions.len(), 2);
        assert!(
            sessions
                .iter()
                .all(|session| { session.external_session_id.as_deref() == Some("ambiguous") })
        );
    }

    #[test]
    fn catalog_merges_durable_identity_with_title_and_keeps_durable_lifecycle() {
        let durable = workspace("w1", &["a1"]);
        let durable_only = project_workspaces(std::slice::from_ref(&durable));
        let mut record = catalog_session("external", "/tmp/project");
        record.created_at_ms = 1;
        record.updated_at_ms = 100;

        let merged = project_workspaces_with_catalog(&[durable], Some(&[record]));

        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].id, durable_only[0].id);
        assert_eq!(merged[0].description, "Catalog external");
        assert_eq!(merged[0].state, AgentState::Working);
        assert!(merged[0].state_is_current);
        assert_eq!(merged[0].occurrences.len(), 1);
        assert_eq!(merged[0].source_cwd, durable_only[0].source_cwd);
        assert_eq!(merged[0].started_at_ms, durable_only[0].started_at_ms);
        assert_eq!(merged[0].last_at_ms, durable_only[0].last_at_ms);
    }

    #[test]
    fn catalog_directory_is_the_fallback_when_durable_source_is_missing() {
        let mut durable = workspace("w1", &["a1"]);
        durable.shells.clear();
        durable.agents[0].cwd = None;
        durable
            .launchers
            .push(boomux::protocol::WorkspaceLauncherSnapshot {
                id: "launcher".into(),
                revision: 1,
                workspace_id: "w1".into(),
                name: "agent".into(),
                command: vec!["opencode".into()],
                cwd: "/tmp/project".into(),
            });

        let merged = project_workspaces_with_catalog(
            &[durable],
            Some(&[catalog_session("external", "/tmp/project")]),
        );

        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].occurrences.len(), 1);
        assert!(merged[0].occurrences[0].source_cwd.is_none());
        assert_eq!(
            merged[0].source_cwd.as_deref(),
            Some(Path::new("/tmp/project"))
        );
    }
}

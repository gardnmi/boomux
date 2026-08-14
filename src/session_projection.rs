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
    pub(crate) state: AgentState,
    pub(crate) state_is_current: bool,
    pub(crate) started_at_ms: u64,
    pub(crate) last_at_ms: u64,
    pub(crate) source_cwd: Option<PathBuf>,
    pub(crate) occurrences: Vec<SessionOccurrence>,
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

fn project_workspace(workspace: &WorkspaceSnapshot) -> Vec<SessionProjection> {
    let shells: BTreeMap<_, _> = workspace
        .shells
        .iter()
        .map(|shell| (shell.id.as_str(), shell))
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
            let latest = agents
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
                .filter(|agent| occurrence_is_current(workspace, agent));
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
                .map_or((latest, false), |active| (active, true));
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
                        is_current: occurrence_is_current(workspace, agent),
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
                description: latest.name.clone(),
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
    let workspace_directories = workspaces
        .iter()
        .map(|workspace| (workspace.id.as_str(), workspace_directories(workspace)))
        .collect::<Vec<_>>();

    for record in catalog {
        let Some(record_directory) = normalize_absolute(&record.directory) else {
            continue;
        };
        let matching_workspaces = workspace_directories
            .iter()
            .filter(|(_, directories)| directories.contains(&record_directory))
            .map(|(workspace_id, _)| *workspace_id)
            .collect::<Vec<_>>();
        if matching_workspaces.is_empty() {
            continue;
        }
        for workspace_id in matching_workspaces {
            let Some(workspace) = workspaces
                .iter()
                .find(|workspace| workspace.id == workspace_id)
            else {
                continue;
            };
            let identity = format!("external:{}", record.root_id);
            if let Some(session) = sessions.iter_mut().find(|session| {
                session.workspace_id == workspace.id
                    && session.integration == record.integration
                    && session.external_session_id.as_deref() == Some(record.root_id.as_str())
            }) {
                if session.source_cwd.is_none() {
                    session.source_cwd = Some(record_directory.clone());
                }
                continue;
            }

            sessions.push(SessionProjection {
                id: stable_session_id(&workspace.id, &record.integration, &identity),
                workspace_id: workspace.id.clone(),
                workspace_name: workspace.name.clone(),
                integration: record.integration.clone(),
                external_session_id: Some(record.root_id.clone()),
                description: record.title.clone(),
                state: AgentState::Unknown,
                state_is_current: false,
                started_at_ms: record.created_at_ms,
                last_at_ms: record.updated_at_ms,
                source_cwd: Some(record_directory.clone()),
                occurrences: Vec::new(),
            });
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

fn occurrence_is_current(workspace: &WorkspaceSnapshot, agent: &AgentInstanceSnapshot) -> bool {
    agent.workspace_id == workspace.id
        && workspace.shells.iter().any(|shell| {
            matches!(shell.status, ShellStatus::Running)
                && shell
                    .run
                    .as_ref()
                    .is_some_and(|run| agent_is_active_for_run(agent, &shell.id, &run.id))
        })
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

    fn workspace(id: &str, agent_ids: &[&str]) -> WorkspaceSnapshot {
        let shell = boomux::protocol::ShellSnapshot {
            id: "shell".into(),
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
            foreground_process: None,
        };
        WorkspaceSnapshot {
            id: id.into(),
            name: format!("workspace-{id}"),
            default_cwd: None,
            shells: vec![shell],
            launchers: Vec::new(),
            schedules: Vec::new(),
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
                })
                .collect(),
        }
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
    fn catalog_merges_durable_identity_and_keeps_stable_id_and_durable_source() {
        let durable = workspace("w1", &["a1"]);
        let durable_only = project_workspaces(std::slice::from_ref(&durable));
        let mut record = catalog_session("external", "/tmp/project");
        record.created_at_ms = 1;
        record.updated_at_ms = 100;

        let merged = project_workspaces_with_catalog(&[durable], Some(&[record]));

        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].id, durable_only[0].id);
        assert_eq!(merged[0].description, durable_only[0].description);
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

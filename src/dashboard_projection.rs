use std::path::Path;

use boomux::protocol::{
    AgentAttentionReason, AgentState, ShellRunExitReason, ShellStatus, WorkspaceSnapshot,
};

use crate::agent_attention_projection;
use crate::git;
use crate::host_session_titles;
use crate::session_projection::{self, SessionProjection};
use crate::tui::{
    AgentAuthorityDisplay, AgentDisplayState, AgentSessionRunView, AgentSessionView,
    AgentShellView, AgentView, AttentionReason, LauncherView, NodeView, TerminalKind,
    TerminalRunView, TerminalView, WorkspaceAttentionView, WorkspaceCoordinationView,
    WorkspaceItemOwnerView, WorkspaceItemView, WorkspaceView,
};

fn local_node_placeholder() -> NodeView {
    NodeView {
        id: String::new(),
        alias: "local".into(),
        local: true,
        route: None,
        registration_revision: None,
        health: boomux::protocol::NodeProjectionHealthCode::Online,
        current: true,
        stale: false,
        observed_at_ms: 0,
        observed_protocol_version: None,
        observed_helper_version: Some(env!("CARGO_PKG_VERSION").into()),
        observed_capabilities: Vec::new(),
        workspace_owner_eligible: true,
        workspace_owner_unavailable_reason: None,
    }
}

#[cfg(test)]
pub(crate) fn project(
    workspaces: &[WorkspaceSnapshot],
    git_cache: &mut git::Cache,
) -> Vec<WorkspaceView> {
    let sessions = session_projection::project_workspaces(workspaces);
    project_with_sessions(workspaces, git_cache, &sessions)
}

pub(crate) fn project_with_sessions(
    workspaces: &[WorkspaceSnapshot],
    git_cache: &mut git::Cache,
    sessions: &[SessionProjection],
) -> Vec<WorkspaceView> {
    workspaces
        .iter()
        .map(|workspace| project_workspace(workspace, git_cache, sessions))
        .collect()
}

pub(crate) fn project_remote_node(node: &boomux::protocol::CombinedNode) -> Vec<WorkspaceView> {
    let Some(projection) = node.remote_projection.as_ref() else {
        return Vec::new();
    };
    let node_view = NodeView {
        id: node.node_id.clone(),
        alias: node.alias.clone(),
        local: false,
        route: node.route.clone(),
        registration_revision: node.registration_revision,
        health: node.health,
        current: node.current,
        stale: node.stale,
        observed_at_ms: node.observed_at_ms,
        observed_protocol_version: node.observed_protocol_version,
        observed_helper_version: node.observed_helper_version.clone(),
        observed_capabilities: node.observed_capabilities.clone(),
        workspace_owner_eligible: node.workspace_owner_eligible,
        workspace_owner_unavailable_reason: node.workspace_owner_unavailable_reason.clone(),
    };
    let mut workspaces = projection
        .workspaces
        .iter()
        .map(|workspace| {
            let mut agent_state_counts =
                crate::agent_attention_projection::AgentStateCounts::default();
            for agent in projection
                .agents
                .iter()
                .filter(|agent| agent.workspace_id == workspace.id)
            {
                match agent.state {
                    AgentState::Unknown => agent_state_counts.unknown += 1,
                    AgentState::Working => agent_state_counts.working += 1,
                    AgentState::Blocked => agent_state_counts.blocked += 1,
                    AgentState::Idle => agent_state_counts.idle += 1,
                    AgentState::Inactive => agent_state_counts.inactive += 1,
                    AgentState::Done => agent_state_counts.done += 1,
                }
            }
            let mut items = Vec::new();
            for shell in projection
                .shells
                .iter()
                .filter(|shell| shell.workspace_id == workspace.id)
            {
                let terminal = TerminalView {
                    id: shell.id.clone(),
                    name: shell.name.clone(),
                    status: match shell.status {
                        ShellStatus::Pending => "pending",
                        ShellStatus::Running => "running",
                        ShellStatus::Exited { .. } => "exited",
                    }
                    .into(),
                    directory: "remote path unavailable".into(),
                    repository: String::new(),
                    branch: String::new(),
                    git_state: String::new(),
                    worktree: String::new(),
                    foreground_process: None,
                    kind: TerminalKind::Shell,
                    command: String::new(),
                    argv: Vec::new(),
                    run: shell.run_id.as_ref().map(|run_id| TerminalRunView {
                        id: run_id.clone(),
                        generation: shell.generation.unwrap_or(0),
                        started_at_ms: shell.started_at_ms.unwrap_or(0),
                        ended_at_ms: shell.ended_at_ms,
                        exit_reason: None,
                        output_revision: 0,
                    }),
                };
                let agent = shell.run_id.as_deref().and_then(|run_id| {
                    projection
                        .agents
                        .iter()
                        .filter(|agent| {
                            agent.shell_id == shell.id
                                && agent.run_id == run_id
                                && if matches!(shell.status, ShellStatus::Pending) {
                                    shell.recovered_agent_id.as_deref() == Some(agent.id.as_str())
                                        && agent.ended_at_ms.is_none()
                                        && agent.state != AgentState::Done
                                } else {
                                    !matches!(agent.state, AgentState::Inactive | AgentState::Done)
                                }
                        })
                        .max_by_key(|agent| (agent.observed_at_ms, &agent.id))
                });
                if let Some(agent) = agent {
                    items.push(WorkspaceItemView::AgentShell(AgentShellView {
                        shell: terminal,
                        agent: Some(AgentView {
                            id: agent.id.clone(),
                            state: if matches!(shell.status, ShellStatus::Pending) {
                                AgentDisplayState::Inactive
                            } else {
                                agent.state.into()
                            },
                            integration: agent.integration.clone(),
                            external_session_id: None,
                            authority: AgentAuthorityDisplay::DaemonLifecycle,
                            confidence: 0,
                            evidence: "cached reduced observation".into(),
                            updated_at_ms: agent.observed_at_ms,
                            root_branch: String::new(),
                            root_worktree: String::new(),
                        }),
                    }));
                } else {
                    items.push(WorkspaceItemView::Shell(terminal));
                }
            }
            items.extend(
                projection
                    .launchers
                    .iter()
                    .filter(|launcher| launcher.workspace_id == workspace.id)
                    .map(|launcher| {
                        WorkspaceItemView::Launcher(LauncherView {
                            id: launcher.id.clone(),
                            name: launcher.name.clone(),
                            directory: "remote path unavailable".into(),
                            repository: String::new(),
                            branch: String::new(),
                            git_state: String::new(),
                            worktree: String::new(),
                            command: "cached definition".into(),
                            argv: Vec::new(),
                        })
                    }),
            );
            let item_count = items.len();
            WorkspaceView {
                node: node_view.clone(),
                id: workspace.id.clone(),
                name: workspace.name.clone(),
                default_cwd: None,
                items,
                sessions: Vec::new(),
                agent_state_counts,
                attention_count: workspace.attention_count as usize,
                attention: Vec::new(),
                item_owners: vec![
                    WorkspaceItemOwnerView {
                        node: node_view.clone(),
                        workspace_id: workspace.id.clone(),
                    };
                    item_count
                ],
                coordination: WorkspaceCoordinationView::External {
                    owner_revision: 0,
                    available: node.current && !node.stale,
                },
            }
        })
        .collect::<Vec<_>>();
    workspaces.sort_by(|left, right| {
        left.name
            .cmp(&right.name)
            .then_with(|| left.id.cmp(&right.id))
    });

    workspaces
}

fn project_workspace(
    workspace: &WorkspaceSnapshot,
    git_cache: &mut git::Cache,
    sessions: &[SessionProjection],
) -> WorkspaceView {
    let session_views = session_views(
        sessions
            .iter()
            .filter(|session| session.workspace_id == workspace.id),
    );
    let agent_summary = agent_attention_projection::summarize_workspace(workspace);
    let attention = agent_attention_projection::project_attention(std::slice::from_ref(workspace))
        .into_iter()
        .map(|item| WorkspaceAttentionView {
            node_id: String::new(),
            workspace_id: workspace.id.clone(),
            agent_id: item.agent.id,
            shell_id: item.agent.shell_id,
            agent_name: item.agent.name,
            reason: item.attention.reason.into(),
            evidence: item.attention.observation.evidence,
            observed_at_ms: item.attention.observation.observed_at_ms,
        })
        .collect();
    let shells = workspace
        .shells
        .iter()
        .map(|shell| {
            let git = git_cache.inspect(&shell.cwd);
            let shell_view = TerminalView {
                id: shell.id.clone(),
                name: shell.name.clone(),
                status: shell_status(&shell.status).into(),
                directory: shell.cwd.display().to_string(),
                repository: git.repository,
                branch: git.branch,
                git_state: git.state,
                worktree: git.worktree,
                foreground_process: shell.foreground_process.clone(),
                kind: if shell.command.is_empty() {
                    TerminalKind::Shell
                } else {
                    TerminalKind::Command
                },
                command: shell.command.join(" "),
                argv: shell.command.clone(),
                run: shell.run.as_ref().map(|run| TerminalRunView {
                    id: run.id.clone(),
                    generation: run.generation,
                    started_at_ms: run.started_at_ms,
                    ended_at_ms: run.ended_at_ms,
                    exit_reason: run.exit_reason.as_ref().map(shell_exit_reason),
                    output_revision: run.output_revision,
                }),
            };
            let agent = shell.run.as_ref().and_then(|run| {
                if matches!(shell.status, ShellStatus::Running) {
                    workspace
                        .agents
                        .iter()
                        .filter(|agent| {
                            agent.workspace_id == workspace.id
                                && session_projection::agent_is_active_for_run(
                                    agent, &shell.id, &run.id,
                                )
                        })
                        .max_by(|left, right| {
                            left.observation
                                .observed_at_ms
                                .cmp(&right.observation.observed_at_ms)
                                .then_with(|| left.id.cmp(&right.id))
                        })
                } else if matches!(shell.status, ShellStatus::Pending) {
                    workspace
                        .agents
                        .iter()
                        .filter(|agent| {
                            agent.workspace_id == workspace.id
                                && shell.recovered_agent_id.as_deref() == Some(agent.id.as_str())
                                && agent.shell_id == shell.id
                                && agent.run_id == run.id
                                && agent.ended_at_ms.is_none()
                                && agent.observation.authority
                                    == boomux::protocol::AgentAuthority::LifecycleIntegration
                                && agent.observation.state != AgentState::Done
                        })
                        .max_by(|left, right| {
                            left.observation
                                .observed_at_ms
                                .cmp(&right.observation.observed_at_ms)
                                .then_with(|| left.id.cmp(&right.id))
                        })
                } else {
                    None
                }
            });
            let suppress_foreground_hint = shell.run.as_ref().is_some_and(|run| {
                workspace.agents.iter().any(|agent| {
                    agent.workspace_id == workspace.id
                        && agent.shell_id == shell.id
                        && agent.run_id == run.id
                        && boomux::integrations::by_key(&agent.integration)
                            .and_then(|descriptor| descriptor.foreground)
                            .is_some_and(|foreground| {
                                shell.foreground_process.as_deref() == Some(foreground.process_name)
                            })
                        && !session_projection::agent_is_active_for_run(agent, &shell.id, &run.id)
                })
            });
            let root_git = agent
                .and_then(|agent| {
                    sessions.iter().find(|session| {
                        session
                            .occurrences
                            .iter()
                            .any(|run| run.agent_id == agent.id)
                    })
                })
                .and_then(|session| session.source_cwd.as_deref())
                .map(|directory| git_cache.inspect(directory))
                .unwrap_or_default();
            match (agent, shell.foreground_process.as_deref()) {
                (Some(agent), _) => WorkspaceItemView::AgentShell(AgentShellView {
                    shell: shell_view,
                    agent: Some(AgentView {
                        id: agent.id.clone(),
                        state: if matches!(shell.status, ShellStatus::Pending) {
                            AgentDisplayState::Inactive
                        } else {
                            agent.observation.state.into()
                        },
                        integration: agent.integration.clone(),
                        external_session_id: agent.external_session_id.clone(),
                        authority: agent.observation.authority.into(),
                        confidence: agent.observation.confidence,
                        evidence: agent.observation.evidence.clone(),
                        updated_at_ms: agent.observation.observed_at_ms,
                        root_branch: root_git.branch,
                        root_worktree: root_git.worktree,
                    }),
                }),
                (None, Some(process))
                    if boomux::integrations::by_foreground_process(process).is_some()
                        && !suppress_foreground_hint =>
                {
                    WorkspaceItemView::AgentShell(AgentShellView {
                        shell: shell_view,
                        agent: None,
                    })
                }
                (None, _) => WorkspaceItemView::Shell(shell_view),
            }
        })
        .collect::<Vec<_>>();
    let launchers = workspace.launchers.iter().map(|launcher| {
        let git = git_cache.inspect(&launcher.cwd);
        WorkspaceItemView::Launcher(LauncherView {
            id: launcher.id.clone(),
            name: launcher.name.clone(),
            directory: launcher.cwd.display().to_string(),
            repository: git.repository,
            branch: git.branch,
            git_state: git.state,
            worktree: git.worktree,
            command: launcher.command.join(" "),
            argv: launcher.command.clone(),
        })
    });
    let items = shells.into_iter().chain(launchers).collect::<Vec<_>>();
    let node = local_node_placeholder();
    WorkspaceView {
        node: node.clone(),
        id: workspace.id.clone(),
        name: workspace.name.clone(),
        default_cwd: workspace
            .default_cwd
            .as_ref()
            .map(|cwd| cwd.display().to_string()),
        item_owners: vec![
            WorkspaceItemOwnerView {
                node: node.clone(),
                workspace_id: workspace.id.clone(),
            };
            items.len()
        ],
        items,
        sessions: session_views,
        agent_state_counts: agent_summary.states,
        attention_count: agent_summary.attention_count,
        attention,
        coordination: WorkspaceCoordinationView::External {
            owner_revision: workspace.revision,
            available: true,
        },
    }
}

pub(crate) fn session_views<'a>(
    sessions: impl IntoIterator<Item = &'a SessionProjection>,
) -> Vec<AgentSessionView> {
    sessions
        .into_iter()
        .map(|session| AgentSessionView {
            id: session.id.clone(),
            label: session.description.clone(),
            integration: session.integration.clone(),
            external_session_id: session.external_session_id.clone(),
            state: session.state.into(),
            state_is_current: session.state_is_current,
            last_at_ms: session.last_at_ms,
            source_cwd: session.source_cwd.clone(),
            runs: session
                .occurrences
                .iter()
                .map(|occurrence| AgentSessionRunView {
                    agent_id: occurrence.agent_id.clone(),
                    shell_name: occurrence.retained_shell_name.clone(),
                    directory: occurrence.source_cwd.clone(),
                })
                .collect(),
        })
        .collect()
}

pub(crate) fn enrich_session_titles(
    workspaces: &mut [WorkspaceView],
    title_cache: &mut host_session_titles::Cache,
) {
    enrich_session_titles_with(workspaces, |integration, directory, external_session_id| {
        title_cache.title(integration, directory, external_session_id)
    });
}

pub(crate) fn enrich_session_titles_with<F>(workspaces: &mut [WorkspaceView], mut title: F)
where
    F: FnMut(&str, &Path, &str) -> Option<String>,
{
    for session in workspaces
        .iter_mut()
        .flat_map(|workspace| workspace.sessions.iter_mut())
    {
        let Some(external_session_id) = session.external_session_id.as_deref() else {
            continue;
        };
        let Some(directory) = session
            .runs
            .iter()
            .rev()
            .find_map(|run| run.directory.as_deref())
            .or(session.source_cwd.as_deref())
        else {
            continue;
        };
        if let Some(host_title) = title(&session.integration, directory, external_session_id) {
            session.label = host_title;
        }
    }
}

impl From<AgentState> for AgentDisplayState {
    fn from(state: AgentState) -> Self {
        match state {
            AgentState::Unknown => Self::Unknown,
            AgentState::Working => Self::Working,
            AgentState::Blocked => Self::Blocked,
            AgentState::Idle => Self::Idle,
            AgentState::Inactive => Self::Inactive,
            AgentState::Done => Self::Done,
        }
    }
}

impl From<AgentAttentionReason> for AttentionReason {
    fn from(reason: AgentAttentionReason) -> Self {
        match reason {
            AgentAttentionReason::Blocked => Self::Blocked,
            AgentAttentionReason::Completed => Self::Completed,
        }
    }
}

impl From<boomux::protocol::AgentAuthority> for AgentAuthorityDisplay {
    fn from(authority: boomux::protocol::AgentAuthority) -> Self {
        match authority {
            boomux::protocol::AgentAuthority::DaemonLifecycle => Self::DaemonLifecycle,
            boomux::protocol::AgentAuthority::LifecycleIntegration => Self::LifecycleIntegration,
            boomux::protocol::AgentAuthority::ProcessAdapter => Self::ProcessAdapter,
            boomux::protocol::AgentAuthority::TerminalHeuristic => Self::TerminalHeuristic,
        }
    }
}

fn shell_status(status: &ShellStatus) -> &'static str {
    match status {
        ShellStatus::Pending => "pending",
        ShellStatus::Running => "running",
        ShellStatus::Exited { .. } => "exited",
    }
}

fn shell_exit_reason(reason: &ShellRunExitReason) -> String {
    match reason {
        ShellRunExitReason::Exited { code: Some(code) } => format!("exited ({code})"),
        ShellRunExitReason::Exited { code: None } => "exited (code unavailable)".into(),
        ShellRunExitReason::Terminated => "terminated".into(),
        ShellRunExitReason::Interrupted => "interrupted".into(),
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use boomux::protocol::{
        AgentAuthority, AgentInstanceSnapshot, AgentObservationSnapshot, ShellRunSnapshot,
        ShellSnapshot,
    };

    use super::*;

    fn workspace(command: Vec<String>) -> WorkspaceSnapshot {
        WorkspaceSnapshot {
            id: "workspace-1".into(),
            revision: 1,
            name: "project".into(),
            default_cwd: Some(PathBuf::from("/tmp/project")),
            shells: vec![ShellSnapshot {
                id: "shell-1".into(),
                revision: 1,
                workspace_id: "workspace-1".into(),
                name: "main".into(),
                cwd: PathBuf::from("/tmp/project"),
                command,
                status: ShellStatus::Running,
                run: Some(ShellRunSnapshot {
                    id: "run-1".into(),
                    generation: 1,
                    started_at_ms: 1,
                    ended_at_ms: None,
                    exit_reason: None,
                    output_revision: 0,
                    environment_has_run_id: true,
                }),
                recovered_agent_id: None,
                foreground_process: None,
            }],
            launchers: Vec::new(),
            agents: Vec::new(),
        }
    }

    #[test]
    fn stored_argv_projects_an_explicit_command_kind() {
        let views = project(
            &[workspace(vec!["cargo".into(), "test".into()])],
            &mut git::Cache::default(),
        );

        let WorkspaceItemView::Shell(shell) = &views[0].items[0] else {
            panic!("expected shell-backed item");
        };
        assert_eq!(shell.kind, TerminalKind::Command);
        assert_eq!(shell.argv, ["cargo", "test"]);
    }

    #[test]
    fn current_agent_projects_typed_state_and_inactive_agent_suppresses_hint() {
        let mut workspace = workspace(Vec::new());
        workspace.shells[0].foreground_process = Some("opencode".into());
        workspace.agents.push(AgentInstanceSnapshot {
            id: "agent-1".into(),
            workspace_id: workspace.id.clone(),
            shell_id: "shell-1".into(),
            run_id: "run-1".into(),
            name: "review".into(),
            integration: "opencode".into(),
            external_session_id: Some("session-1".into()),
            cwd: Some(PathBuf::from("/tmp/project")),
            started_at_ms: 1,
            ended_at_ms: None,
            observation: AgentObservationSnapshot {
                revision: 1,
                state: AgentState::Blocked,
                authority: AgentAuthority::LifecycleIntegration,
                evidence: "permission".into(),
                confidence: 100,
                observed_at_ms: 2,
            },
            attention: None,
            working_contexts: Vec::new(),
        });
        let views = project(std::slice::from_ref(&workspace), &mut git::Cache::default());
        let WorkspaceItemView::AgentShell(agent) = &views[0].items[0] else {
            panic!("expected Agent shell");
        };
        assert_eq!(agent.state(), AgentDisplayState::Blocked);

        workspace.agents[0].observation.state = AgentState::Inactive;
        let views = project(std::slice::from_ref(&workspace), &mut git::Cache::default());
        assert!(matches!(views[0].items[0], WorkspaceItemView::Shell(_)));
    }

    #[test]
    fn recovered_pending_shell_keeps_its_exact_agent_kind_as_inactive() {
        let mut workspace = workspace(Vec::new());
        workspace.shells[0].status = ShellStatus::Pending;
        workspace.shells[0].run.as_mut().unwrap().ended_at_ms = Some(2);
        workspace.shells[0].run.as_mut().unwrap().exit_reason =
            Some(ShellRunExitReason::Interrupted);
        workspace.shells[0].recovered_agent_id = Some("agent-1".into());
        workspace.agents.push(AgentInstanceSnapshot {
            id: "agent-1".into(),
            workspace_id: workspace.id.clone(),
            shell_id: "shell-1".into(),
            run_id: "run-1".into(),
            name: "review".into(),
            integration: "opencode".into(),
            external_session_id: Some("session-1".into()),
            cwd: Some(PathBuf::from("/tmp/project")),
            started_at_ms: 1,
            ended_at_ms: None,
            observation: AgentObservationSnapshot {
                revision: 1,
                state: AgentState::Blocked,
                authority: AgentAuthority::LifecycleIntegration,
                evidence: "permission".into(),
                confidence: 100,
                observed_at_ms: 2,
            },
            attention: None,
            working_contexts: Vec::new(),
        });
        let mut distractor = workspace.agents[0].clone();
        distractor.id = "agent-2".into();
        distractor.name = "wrong-agent".into();
        distractor.integration = "native-test".into();
        distractor.external_session_id = None;
        distractor.observation.observed_at_ms = 3;
        workspace.agents.push(distractor);

        let views = project(std::slice::from_ref(&workspace), &mut git::Cache::default());
        let WorkspaceItemView::AgentShell(agent) = &views[0].items[0] else {
            panic!("expected recovered Agent shell");
        };
        assert_eq!(agent.agent.as_ref().unwrap().id, "agent-1");
        assert_eq!(agent.state(), AgentDisplayState::Inactive);
        assert_eq!(agent.shell.status, "pending");

        workspace.shells[0].run = None;
        let views = project(&[workspace], &mut git::Cache::default());
        assert!(matches!(views[0].items[0], WorkspaceItemView::Shell(_)));
    }

    #[test]
    fn remote_recovery_marker_projects_only_as_inactive() {
        let mut node = boomux::protocol::CombinedNode {
            node_id: "node-1".into(),
            alias: "remote".into(),
            local: false,
            route: Some("remote.example".into()),
            registration_revision: Some(1),
            health: boomux::protocol::NodeProjectionHealthCode::Online,
            current: true,
            stale: false,
            observed_at_ms: 2,
            observed_protocol_version: Some(40),
            observed_helper_version: Some("0.18.1".into()),
            observed_capabilities: vec!["recovered_agent_presentation".into()],
            workspace_owner_eligible: true,
            workspace_owner_unavailable_reason: None,
            local_snapshot: None,
            remote_projection: Some(boomux::protocol::NodeProjectionSnapshot {
                node_id: "node-1".into(),
                workspaces: vec![boomux::protocol::NodeProjectionWorkspace {
                    id: "workspace-1".into(),
                    name: "project".into(),
                    item_count: 1,
                    attention_count: 0,
                }],
                shells: vec![boomux::protocol::NodeProjectionShell {
                    id: "shell-1".into(),
                    workspace_id: "workspace-1".into(),
                    name: "agent".into(),
                    status: ShellStatus::Pending,
                    run_id: Some("run-1".into()),
                    generation: Some(1),
                    started_at_ms: Some(1),
                    ended_at_ms: Some(2),
                    recovered_agent_id: Some("agent-1".into()),
                }],
                launchers: Vec::new(),
                agents: vec![boomux::protocol::NodeProjectionAgent {
                    id: "agent-1".into(),
                    workspace_id: "workspace-1".into(),
                    shell_id: "shell-1".into(),
                    run_id: "run-1".into(),
                    name: "review".into(),
                    integration: "opencode".into(),
                    state: AgentState::Blocked,
                    observation_revision: 1,
                    observed_at_ms: 2,
                    started_at_ms: 1,
                    ended_at_ms: None,
                    attention: None,
                }],
            }),
        };

        let projection = node.remote_projection.as_mut().unwrap();
        let mut distractor = projection.agents[0].clone();
        distractor.id = "agent-2".into();
        distractor.name = "wrong-agent".into();
        distractor.integration = "native-test".into();
        distractor.observed_at_ms = 3;
        projection.agents.push(distractor);

        let views = project_remote_node(&node);
        let WorkspaceItemView::AgentShell(agent) = &views[0].items[0] else {
            panic!("expected recovered remote Agent shell");
        };
        assert_eq!(agent.agent.as_ref().unwrap().id, "agent-1");
        assert_eq!(agent.state(), AgentDisplayState::Inactive);

        node.remote_projection.as_mut().unwrap().shells[0].run_id = None;
        let views = project_remote_node(&node);
        assert!(matches!(views[0].items[0], WorkspaceItemView::Shell(_)));
    }

    #[test]
    fn completed_same_run_host_suppresses_hint_but_other_integration_does_not() {
        let mut workspace = workspace(Vec::new());
        workspace.shells[0].foreground_process = Some("opencode".into());
        workspace.agents.push(AgentInstanceSnapshot {
            id: "agent-1".into(),
            workspace_id: workspace.id.clone(),
            shell_id: "shell-1".into(),
            run_id: "run-1".into(),
            name: "review".into(),
            integration: "opencode".into(),
            external_session_id: Some("session-1".into()),
            cwd: None,
            started_at_ms: 1,
            ended_at_ms: Some(2),
            observation: AgentObservationSnapshot {
                revision: 1,
                state: AgentState::Done,
                authority: AgentAuthority::LifecycleIntegration,
                evidence: "done".into(),
                confidence: 100,
                observed_at_ms: 2,
            },
            attention: None,
            working_contexts: Vec::new(),
        });

        let views = project(std::slice::from_ref(&workspace), &mut git::Cache::default());
        assert!(matches!(views[0].items[0], WorkspaceItemView::Shell(_)));

        workspace.agents[0].integration = "pi".into();
        let views = project(std::slice::from_ref(&workspace), &mut git::Cache::default());
        let WorkspaceItemView::AgentShell(agent) = &views[0].items[0] else {
            panic!("expected foreground hint");
        };
        assert_eq!(agent.state(), AgentDisplayState::Untracked);

        workspace.agents[0].integration = "opencode".into();
        workspace.agents[0].workspace_id = "other-workspace".into();
        let views = project(&[workspace], &mut git::Cache::default());
        let WorkspaceItemView::AgentShell(agent) = &views[0].items[0] else {
            panic!("expected foreground hint");
        };
        assert_eq!(agent.state(), AgentDisplayState::Untracked);
    }
}

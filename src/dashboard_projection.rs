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
    AgentShellView, AgentView, AttentionReason, LauncherView, TerminalKind, TerminalRunView,
    TerminalView, WorkspaceAttentionView, WorkspaceItemView, WorkspaceView,
};

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
            let agent = matches!(shell.status, ShellStatus::Running)
                .then(|| {
                    shell.run.as_ref().and_then(|run| {
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
                    })
                })
                .flatten();
            let suppress_foreground_hint = shell.run.as_ref().is_some_and(|run| {
                workspace.agents.iter().any(|agent| {
                    agent.workspace_id == workspace.id
                        && agent.shell_id == shell.id
                        && agent.run_id == run.id
                        && shell.foreground_process.as_deref() == Some(agent.integration.as_str())
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
                        state: agent.observation.state.into(),
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
                (None, Some("opencode" | "pi")) if !suppress_foreground_hint => {
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
    WorkspaceView {
        id: workspace.id.clone(),
        name: workspace.name.clone(),
        default_cwd: workspace
            .default_cwd
            .as_ref()
            .map(|cwd| cwd.display().to_string()),
        items: shells.into_iter().chain(launchers).collect(),
        sessions: session_views,
        agent_state_counts: agent_summary.states,
        attention_count: agent_summary.attention_count,
        attention,
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
            name: "project".into(),
            default_cwd: Some(PathBuf::from("/tmp/project")),
            shells: vec![ShellSnapshot {
                id: "shell-1".into(),
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

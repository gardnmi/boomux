use std::cmp::Ordering;

use boomux::protocol::{
    AgentAttentionReason, AgentAttentionSnapshot, AgentInstanceSnapshot, AgentState,
    WorkspaceSnapshot,
};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize)]
pub(crate) struct AgentStateCounts {
    pub(crate) unknown: usize,
    pub(crate) working: usize,
    pub(crate) blocked: usize,
    pub(crate) idle: usize,
    pub(crate) inactive: usize,
    pub(crate) done: usize,
}

impl AgentStateCounts {
    fn add(&mut self, state: AgentState) {
        match state {
            AgentState::Unknown => self.unknown += 1,
            AgentState::Working => self.working += 1,
            AgentState::Blocked => self.blocked += 1,
            AgentState::Idle => self.idle += 1,
            AgentState::Inactive => self.inactive += 1,
            AgentState::Done => self.done += 1,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct WorkspaceAgentSummary {
    pub(crate) states: AgentStateCounts,
    pub(crate) attention_count: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct AttentionItem {
    pub(crate) workspace_id: String,
    pub(crate) workspace_name: String,
    pub(crate) agent: AgentInstanceSnapshot,
    pub(crate) attention: AgentAttentionSnapshot,
    pub(crate) observation_is_current: bool,
    pub(crate) shell_is_retained: bool,
}

pub(crate) fn summarize_workspace(workspace: &WorkspaceSnapshot) -> WorkspaceAgentSummary {
    let mut summary = WorkspaceAgentSummary::default();
    for agent in &workspace.agents {
        summary.states.add(agent.observation.state);
        summary.attention_count += usize::from(agent.attention.is_some());
    }
    summary
}

pub(crate) fn project_attention(workspaces: &[WorkspaceSnapshot]) -> Vec<AttentionItem> {
    let mut items = workspaces
        .iter()
        .flat_map(|workspace| {
            workspace.agents.iter().filter_map(|agent| {
                let attention = agent.attention.clone()?;
                let shell_is_retained = workspace
                    .shells
                    .iter()
                    .any(|shell| shell.id == agent.shell_id);
                Some(AttentionItem {
                    workspace_id: workspace.id.clone(),
                    workspace_name: workspace.name.clone(),
                    observation_is_current: agent.observation.revision
                        == attention.observation.revision,
                    shell_is_retained,
                    agent: agent.clone(),
                    attention,
                })
            })
        })
        .collect::<Vec<_>>();
    items.sort_by(compare_attention);
    items
}

fn compare_attention(left: &AttentionItem, right: &AttentionItem) -> Ordering {
    attention_rank(left.attention.reason)
        .cmp(&attention_rank(right.attention.reason))
        .then_with(|| {
            right
                .attention
                .observation
                .observed_at_ms
                .cmp(&left.attention.observation.observed_at_ms)
        })
        .then_with(|| left.workspace_id.cmp(&right.workspace_id))
        .then_with(|| left.agent.id.cmp(&right.agent.id))
}

fn attention_rank(reason: AgentAttentionReason) -> u8 {
    match reason {
        AgentAttentionReason::Blocked => 0,
        AgentAttentionReason::Completed => 1,
    }
}

pub(crate) fn attention_reason(reason: AgentAttentionReason) -> &'static str {
    match reason {
        AgentAttentionReason::Blocked => "blocked",
        AgentAttentionReason::Completed => "completed",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use boomux::protocol::{
        AgentAuthority, AgentObservationSnapshot, ShellRunSnapshot, ShellStatus,
        WorkspaceLauncherSnapshot,
    };
    use std::path::PathBuf;

    fn agent(
        id: &str,
        state: AgentState,
        attention: Option<(AgentAttentionReason, u64)>,
    ) -> AgentInstanceSnapshot {
        let observation = AgentObservationSnapshot {
            revision: 3,
            state,
            authority: AgentAuthority::LifecycleIntegration,
            evidence: "current".into(),
            confidence: 100,
            observed_at_ms: 30,
        };
        AgentInstanceSnapshot {
            id: id.into(),
            workspace_id: "w1".into(),
            shell_id: "s1".into(),
            run_id: "r1".into(),
            name: id.into(),
            integration: "test".into(),
            external_session_id: Some(id.into()),
            cwd: Some(PathBuf::from("/tmp")),
            started_at_ms: 1,
            ended_at_ms: (state == AgentState::Done).then_some(30),
            attention: attention.map(|(reason, revision)| AgentAttentionSnapshot {
                reason,
                observation: AgentObservationSnapshot {
                    revision,
                    state: match reason {
                        AgentAttentionReason::Blocked => AgentState::Blocked,
                        AgentAttentionReason::Completed => AgentState::Done,
                    },
                    authority: AgentAuthority::LifecycleIntegration,
                    evidence: "needs attention".into(),
                    confidence: 100,
                    observed_at_ms: revision * 10,
                },
            }),
            observation,
        }
    }

    fn workspace(agents: Vec<AgentInstanceSnapshot>) -> WorkspaceSnapshot {
        WorkspaceSnapshot {
            id: "w1".into(),
            name: "project".into(),
            default_cwd: None,
            shells: vec![boomux::protocol::ShellSnapshot {
                owner: boomux::protocol::ShellOwner::User,
                id: "s1".into(),
                workspace_id: "w1".into(),
                name: "shell".into(),
                cwd: PathBuf::from("/tmp"),
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
                foreground_process: None,
            }],
            launchers: Vec::<WorkspaceLauncherSnapshot>::new(),
            schedules: Vec::new(),
            agents,
        }
    }

    #[test]
    fn counts_every_retained_agent_state_and_outstanding_item() {
        let states = [
            AgentState::Unknown,
            AgentState::Working,
            AgentState::Blocked,
            AgentState::Idle,
            AgentState::Inactive,
            AgentState::Done,
        ];
        let agents = states
            .into_iter()
            .enumerate()
            .map(|(index, state)| {
                agent(
                    &format!("a{index}"),
                    state,
                    (index == 2).then_some((AgentAttentionReason::Blocked, 3)),
                )
            })
            .collect();
        let summary = summarize_workspace(&workspace(agents));
        assert_eq!(
            summary.states,
            AgentStateCounts {
                unknown: 1,
                working: 1,
                blocked: 1,
                idle: 1,
                inactive: 1,
                done: 1
            }
        );
        assert_eq!(summary.attention_count, 1);
    }

    #[test]
    fn queue_is_blocked_first_newest_first_and_explains_stale_observations() {
        let items = project_attention(&[workspace(vec![
            agent(
                "completed",
                AgentState::Done,
                Some((AgentAttentionReason::Completed, 3)),
            ),
            agent(
                "old-blocker",
                AgentState::Working,
                Some((AgentAttentionReason::Blocked, 2)),
            ),
            agent(
                "new-blocker",
                AgentState::Blocked,
                Some((AgentAttentionReason::Blocked, 3)),
            ),
        ])]);
        assert_eq!(
            items
                .iter()
                .map(|item| item.agent.id.as_str())
                .collect::<Vec<_>>(),
            ["new-blocker", "old-blocker", "completed"]
        );
        assert!(items[0].observation_is_current);
        assert!(!items[1].observation_is_current);
        assert!(items.iter().all(|item| item.shell_is_retained));
    }
}

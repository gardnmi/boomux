use std::collections::BTreeMap;
use std::path::Path;

use boomux::protocol::{
    AgentAttentionReason, AgentScheduleState, AgentState, ScheduledExecutionOutcome,
    ScheduledExecutionReason, ScheduledExecutionSnapshot, ScheduledExecutionState,
    ShellRunExitReason, ShellStatus, Snapshot, WorkspaceSnapshot,
};

use crate::agent_attention_projection;
use crate::git;
use crate::host_session_titles;
use crate::session_projection::{self, SessionProjection};
use crate::tui::{
    AgentAuthorityDisplay, AgentDisplayState, AgentSessionRunView, AgentSessionView,
    AgentShellView, AgentView, AttentionReason, ExecutionDisplayState, ExecutionOutcomeDisplay,
    ExecutionReasonDisplay, ExecutionView, LauncherView, NodeView, ScheduleDisplayState,
    ScheduleItemView, ScheduleView, TerminalKind, TerminalRunView, TerminalView,
    WorkspaceAttentionView, WorkspaceCoordinationView, WorkspaceItemOwnerView, WorkspaceItemView,
    WorkspaceView,
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
        observed_capabilities: Vec::new(),
        workspace_owner_eligible: true,
        workspace_owner_unavailable_reason: None,
        scheduler: boomux::protocol::SchedulerHealth {
            state: boomux::protocol::SchedulerState::Offline,
            max_concurrent: 0,
            active_executions: 0,
        },
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

pub(crate) fn project_schedules(
    snapshot: &Snapshot,
    executions: &[ScheduledExecutionSnapshot],
    global_history_truncated: bool,
    scoped_history: &BTreeMap<String, bool>,
) -> Vec<ScheduleView> {
    let sessions = session_projection::project_workspaces_with_catalog(&snapshot.workspaces, None);
    let mut schedules = snapshot
        .workspaces
        .iter()
        .flat_map(|workspace| {
            let sessions = &sessions;
            workspace.schedules.iter().map(move |schedule| {
                let mut history = executions
                    .iter()
                    .filter(|execution| execution.schedule_id == schedule.id)
                    .map(|execution| execution_view(workspace, sessions, execution))
                    .collect::<Vec<_>>();
                history.sort_by(|left, right| {
                    right
                        .requested_at_ms
                        .cmp(&left.requested_at_ms)
                        .then_with(|| right.id.cmp(&left.id))
                });
                let terminal_count = history
                    .iter()
                    .filter(|execution| !execution.state.is_active())
                    .count();
                ScheduleView {
                    node_id: String::new(),
                    node_alias: "local".into(),
                    actionable: true,
                    id: schedule.id.clone(),
                    workspace_id: workspace.id.clone(),
                    workspace: workspace.name.clone(),
                    name: schedule.name.clone(),
                    integration: schedule.integration.clone(),
                    state: match schedule.state {
                        AgentScheduleState::Paused => ScheduleDisplayState::Paused,
                        AgentScheduleState::Enabled => ScheduleDisplayState::Enabled,
                    },
                    friendly_trigger: friendly_trigger(&schedule.trigger.cron),
                    next_occurrence_ms: schedule
                        .next_occurrence
                        .as_ref()
                        .map(|occurrence| occurrence.scheduled_at_ms),
                    executions: history,
                    history_truncated: scoped_history
                        .get(&schedule.id)
                        .copied()
                        .unwrap_or(global_history_truncated),
                    possible_pruning_boundary: terminal_count >= 100,
                    history_scoped: scoped_history.contains_key(&schedule.id),
                    history_complete: scoped_history
                        .get(&schedule.id)
                        .map_or(!global_history_truncated, |truncated| !truncated),
                }
            })
        })
        .collect::<Vec<_>>();
    schedules.sort_by(|left, right| {
        left.workspace
            .cmp(&right.workspace)
            .then_with(|| left.name.cmp(&right.name))
            .then_with(|| left.id.cmp(&right.id))
    });
    schedules
}

pub(crate) fn project_remote_executions(
    executions: &[ScheduledExecutionSnapshot],
) -> Vec<ExecutionView> {
    executions
        .iter()
        .map(|execution| ExecutionView {
            id: execution.id.clone(),
            state: match execution.state {
                ScheduledExecutionState::Skipped => ExecutionDisplayState::Skipped,
                ScheduledExecutionState::Claimed => ExecutionDisplayState::Claimed,
                ScheduledExecutionState::Starting => ExecutionDisplayState::Starting,
                ScheduledExecutionState::Active => ExecutionDisplayState::Active,
                ScheduledExecutionState::DispatchFailed => ExecutionDisplayState::DispatchFailed,
                ScheduledExecutionState::Exited => ExecutionDisplayState::Exited,
                ScheduledExecutionState::Cancelled => ExecutionDisplayState::Cancelled,
                ScheduledExecutionState::Interrupted => ExecutionDisplayState::Interrupted,
            },
            reason: execution.reason.map(|reason| match reason {
                ScheduledExecutionReason::Overlap => ExecutionReasonDisplay::Overlap,
                ScheduledExecutionReason::ActiveSession => ExecutionReasonDisplay::ActiveSession,
                ScheduledExecutionReason::WorkspaceCapacity => {
                    ExecutionReasonDisplay::WorkspaceCapacity
                }
                ScheduledExecutionReason::GlobalCapacity => ExecutionReasonDisplay::GlobalCapacity,
                ScheduledExecutionReason::Missed => ExecutionReasonDisplay::Missed,
                ScheduledExecutionReason::PausedRace => ExecutionReasonDisplay::PausedRace,
                ScheduledExecutionReason::InvalidTarget => ExecutionReasonDisplay::InvalidTarget,
                ScheduledExecutionReason::RunnerStartFailed => {
                    ExecutionReasonDisplay::RunnerStartFailed
                }
                ScheduledExecutionReason::HostSpawnFailed => {
                    ExecutionReasonDisplay::HostSpawnFailed
                }
                ScheduledExecutionReason::CancelledByUser => {
                    ExecutionReasonDisplay::CancelledByUser
                }
                ScheduledExecutionReason::ColdDaemonRecovery => {
                    ExecutionReasonDisplay::ColdDaemonRecovery
                }
                ScheduledExecutionReason::RunnerExitedWithoutReport => {
                    ExecutionReasonDisplay::RunnerExitedWithoutReport
                }
                ScheduledExecutionReason::DaemonShutdown => ExecutionReasonDisplay::DaemonShutdown,
            }),
            outcome: execution.outcome.as_ref().map(|outcome| match outcome {
                ScheduledExecutionOutcome::ExitCode { code } => {
                    ExecutionOutcomeDisplay::ExitCode(*code)
                }
                ScheduledExecutionOutcome::Signal { signal } => {
                    ExecutionOutcomeDisplay::Signal(*signal)
                }
            }),
            requested_at_ms: execution.requested_at_ms,
            shell_id: execution.shell_id.clone(),
            run_id: execution.run_id.clone(),
            agent_id: execution.agent_id.clone(),
            agent_state: None,
            session_id: None,
        })
        .collect()
}

fn execution_view(
    workspace: &WorkspaceSnapshot,
    sessions: &[SessionProjection],
    execution: &ScheduledExecutionSnapshot,
) -> ExecutionView {
    let agent = execution
        .agent_id
        .as_deref()
        .and_then(|agent_id| workspace.agents.iter().find(|agent| agent.id == agent_id));
    let session = execution.agent_id.as_deref().and_then(|agent_id| {
        sessions.iter().find(|session| {
            session.workspace_id == workspace.id
                && session
                    .occurrences
                    .iter()
                    .any(|occurrence| occurrence.agent_id == agent_id)
        })
    });
    ExecutionView {
        id: execution.id.clone(),
        state: match execution.state {
            ScheduledExecutionState::Skipped => ExecutionDisplayState::Skipped,
            ScheduledExecutionState::Claimed => ExecutionDisplayState::Claimed,
            ScheduledExecutionState::Starting => ExecutionDisplayState::Starting,
            ScheduledExecutionState::Active => ExecutionDisplayState::Active,
            ScheduledExecutionState::DispatchFailed => ExecutionDisplayState::DispatchFailed,
            ScheduledExecutionState::Exited => ExecutionDisplayState::Exited,
            ScheduledExecutionState::Cancelled => ExecutionDisplayState::Cancelled,
            ScheduledExecutionState::Interrupted => ExecutionDisplayState::Interrupted,
        },
        reason: execution.reason.map(|reason| match reason {
            ScheduledExecutionReason::Overlap => ExecutionReasonDisplay::Overlap,
            ScheduledExecutionReason::ActiveSession => ExecutionReasonDisplay::ActiveSession,
            ScheduledExecutionReason::WorkspaceCapacity => {
                ExecutionReasonDisplay::WorkspaceCapacity
            }
            ScheduledExecutionReason::GlobalCapacity => ExecutionReasonDisplay::GlobalCapacity,
            ScheduledExecutionReason::Missed => ExecutionReasonDisplay::Missed,
            ScheduledExecutionReason::PausedRace => ExecutionReasonDisplay::PausedRace,
            ScheduledExecutionReason::InvalidTarget => ExecutionReasonDisplay::InvalidTarget,
            ScheduledExecutionReason::RunnerStartFailed => {
                ExecutionReasonDisplay::RunnerStartFailed
            }
            ScheduledExecutionReason::HostSpawnFailed => ExecutionReasonDisplay::HostSpawnFailed,
            ScheduledExecutionReason::CancelledByUser => ExecutionReasonDisplay::CancelledByUser,
            ScheduledExecutionReason::ColdDaemonRecovery => {
                ExecutionReasonDisplay::ColdDaemonRecovery
            }
            ScheduledExecutionReason::RunnerExitedWithoutReport => {
                ExecutionReasonDisplay::RunnerExitedWithoutReport
            }
            ScheduledExecutionReason::DaemonShutdown => ExecutionReasonDisplay::DaemonShutdown,
        }),
        outcome: execution.outcome.as_ref().map(|outcome| match outcome {
            ScheduledExecutionOutcome::ExitCode { code } => {
                ExecutionOutcomeDisplay::ExitCode(*code)
            }
            ScheduledExecutionOutcome::Signal { signal } => {
                ExecutionOutcomeDisplay::Signal(*signal)
            }
        }),
        requested_at_ms: execution.requested_at_ms,
        shell_id: execution.shell_id.clone(),
        run_id: execution.run_id.clone(),
        agent_id: execution.agent_id.clone(),
        agent_state: agent.map(|agent| agent.observation.state.into()),
        session_id: session.map(|session| session.id.clone()),
    }
}

fn friendly_trigger(cron: &str) -> String {
    let fields = cron.split_whitespace().collect::<Vec<_>>();
    match fields.as_slice() {
        ["*", "*", "*", "*", "*"] => "every minute".into(),
        [minute, "*", "*", "*", "*"] if canonical_step(minute, 59).is_some() => {
            format!(
                "every {} minutes",
                canonical_step(minute, 59).expect("checked")
            )
        }
        ["0", "*", "*", "*", "*"] => "every hour".into(),
        ["0", hour, "*", "*", "*"] if canonical_step(hour, 23).is_some() => {
            format!("every {} hours", canonical_step(hour, 23).expect("checked"))
        }
        [minute, hour, "*", "*", "1-5"]
            if canonical_number(minute, 59).is_some() && canonical_number(hour, 23).is_some() =>
        {
            format!(
                "weekdays {:02}:{:02}",
                canonical_number(hour, 23).expect("checked"),
                canonical_number(minute, 59).expect("checked")
            )
        }
        [minute, hour, "*", "*", day]
            if canonical_number(minute, 59).is_some()
                && canonical_number(hour, 23).is_some()
                && canonical_number(day, 6).is_some() =>
        {
            let day = match canonical_number(day, 6).expect("checked") {
                0 => "Sun",
                1 => "Mon",
                2 => "Tue",
                3 => "Wed",
                4 => "Thu",
                5 => "Fri",
                6 => "Sat",
                _ => unreachable!("bounded day"),
            };
            format!(
                "weekly {day} {:02}:{:02}",
                canonical_number(hour, 23).expect("checked"),
                canonical_number(minute, 59).expect("checked")
            )
        }
        [minute, hour, "*", "*", "*"]
            if canonical_number(minute, 59).is_some() && canonical_number(hour, 23).is_some() =>
        {
            format!(
                "daily {:02}:{:02}",
                canonical_number(hour, 23).expect("checked"),
                canonical_number(minute, 59).expect("checked")
            )
        }
        _ => "custom schedule".into(),
    }
}

fn canonical_number(value: &str, maximum: u8) -> Option<u8> {
    let number = value.parse::<u8>().ok()?;
    (number <= maximum && value == number.to_string()).then_some(number)
}

fn canonical_step(value: &str, maximum: u8) -> Option<u8> {
    let step = value.strip_prefix("*/")?;
    let number = canonical_number(step, maximum)?;
    (number > 0).then_some(number)
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

pub(crate) fn project_remote_node(
    node: &boomux::protocol::CombinedNode,
) -> (Vec<WorkspaceView>, Vec<ScheduleView>) {
    let Some(projection) = node.remote_projection.as_ref() else {
        return (Vec::new(), Vec::new());
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
        observed_capabilities: node.observed_capabilities.clone(),
        workspace_owner_eligible: node.workspace_owner_eligible,
        workspace_owner_unavailable_reason: node.workspace_owner_unavailable_reason.clone(),
        scheduler: node.scheduler.clone(),
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
                if matches!(shell.owner, boomux::protocol::ShellOwner::Schedule { .. }) {
                    continue;
                }
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
                        schedule_id: None,
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
            items.extend(
                projection
                    .schedules
                    .iter()
                    .filter(|schedule| schedule.workspace_id == workspace.id)
                    .map(|schedule| {
                        WorkspaceItemView::Schedule(ScheduleItemView {
                            id: schedule.id.clone(),
                            name: schedule.name.clone(),
                            integration: schedule.integration.clone(),
                            state: match schedule.state {
                                AgentScheduleState::Paused => ScheduleDisplayState::Paused,
                                AgentScheduleState::Enabled => ScheduleDisplayState::Enabled,
                            },
                            friendly_trigger: friendly_trigger(&schedule.trigger.cron),
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

    let mut schedules = projection
        .schedules
        .iter()
        .map(|schedule| {
            let workspace = projection
                .workspaces
                .iter()
                .find(|workspace| workspace.id == schedule.workspace_id)
                .map_or("unknown", |workspace| workspace.name.as_str());
            let mut executions = projection
                .executions
                .iter()
                .filter(|execution| execution.schedule_id == schedule.id)
                .map(|execution| ExecutionView {
                    id: execution.id.clone(),
                    state: match execution.state {
                        ScheduledExecutionState::Skipped => ExecutionDisplayState::Skipped,
                        ScheduledExecutionState::Claimed => ExecutionDisplayState::Claimed,
                        ScheduledExecutionState::Starting => ExecutionDisplayState::Starting,
                        ScheduledExecutionState::Active => ExecutionDisplayState::Active,
                        ScheduledExecutionState::DispatchFailed => {
                            ExecutionDisplayState::DispatchFailed
                        }
                        ScheduledExecutionState::Exited => ExecutionDisplayState::Exited,
                        ScheduledExecutionState::Cancelled => ExecutionDisplayState::Cancelled,
                        ScheduledExecutionState::Interrupted => ExecutionDisplayState::Interrupted,
                    },
                    reason: execution.reason.map(|reason| match reason {
                        ScheduledExecutionReason::Overlap => ExecutionReasonDisplay::Overlap,
                        ScheduledExecutionReason::ActiveSession => {
                            ExecutionReasonDisplay::ActiveSession
                        }
                        ScheduledExecutionReason::WorkspaceCapacity => {
                            ExecutionReasonDisplay::WorkspaceCapacity
                        }
                        ScheduledExecutionReason::GlobalCapacity => {
                            ExecutionReasonDisplay::GlobalCapacity
                        }
                        ScheduledExecutionReason::Missed => ExecutionReasonDisplay::Missed,
                        ScheduledExecutionReason::PausedRace => ExecutionReasonDisplay::PausedRace,
                        ScheduledExecutionReason::InvalidTarget => {
                            ExecutionReasonDisplay::InvalidTarget
                        }
                        ScheduledExecutionReason::RunnerStartFailed => {
                            ExecutionReasonDisplay::RunnerStartFailed
                        }
                        ScheduledExecutionReason::HostSpawnFailed => {
                            ExecutionReasonDisplay::HostSpawnFailed
                        }
                        ScheduledExecutionReason::CancelledByUser => {
                            ExecutionReasonDisplay::CancelledByUser
                        }
                        ScheduledExecutionReason::ColdDaemonRecovery => {
                            ExecutionReasonDisplay::ColdDaemonRecovery
                        }
                        ScheduledExecutionReason::RunnerExitedWithoutReport => {
                            ExecutionReasonDisplay::RunnerExitedWithoutReport
                        }
                        ScheduledExecutionReason::DaemonShutdown => {
                            ExecutionReasonDisplay::DaemonShutdown
                        }
                    }),
                    outcome: execution.outcome.as_ref().map(|outcome| match outcome {
                        ScheduledExecutionOutcome::ExitCode { code } => {
                            ExecutionOutcomeDisplay::ExitCode(*code)
                        }
                        ScheduledExecutionOutcome::Signal { signal } => {
                            ExecutionOutcomeDisplay::Signal(*signal)
                        }
                    }),
                    requested_at_ms: execution.requested_at_ms,
                    shell_id: execution.shell_id.clone(),
                    run_id: execution.run_id.clone(),
                    agent_id: execution.agent_id.clone(),
                    agent_state: execution.agent_id.as_deref().and_then(|agent_id| {
                        projection
                            .agents
                            .iter()
                            .find(|agent| agent.id == agent_id)
                            .map(|agent| agent.state.into())
                    }),
                    session_id: None,
                })
                .collect::<Vec<_>>();
            executions.sort_by(|left, right| {
                right
                    .requested_at_ms
                    .cmp(&left.requested_at_ms)
                    .then_with(|| right.id.cmp(&left.id))
            });
            ScheduleView {
                node_id: node.node_id.clone(),
                node_alias: node.alias.clone(),
                actionable: node.current
                    && !node.stale
                    && node
                        .observed_capabilities
                        .iter()
                        .any(|capability| capability == "remote_agent_schedule_management"),
                id: schedule.id.clone(),
                workspace_id: schedule.workspace_id.clone(),
                workspace: workspace.into(),
                name: schedule.name.clone(),
                integration: schedule.integration.clone(),
                state: match schedule.state {
                    AgentScheduleState::Paused => ScheduleDisplayState::Paused,
                    AgentScheduleState::Enabled => ScheduleDisplayState::Enabled,
                },
                friendly_trigger: friendly_trigger(&schedule.trigger.cron),
                next_occurrence_ms: schedule
                    .next_occurrence
                    .as_ref()
                    .map(|value| value.scheduled_at_ms),
                executions,
                history_truncated: projection.executions_truncated,
                possible_pruning_boundary: projection.executions_truncated,
                history_scoped: true,
                history_complete: !projection.executions_truncated,
            }
        })
        .collect::<Vec<_>>();
    schedules.sort_by(|left, right| {
        left.workspace
            .cmp(&right.workspace)
            .then_with(|| left.name.cmp(&right.name))
            .then_with(|| left.id.cmp(&right.id))
    });
    (workspaces, schedules)
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
        .filter_map(|shell| {
            let schedule_id = match &shell.owner {
                boomux::protocol::ShellOwner::User => None,
                boomux::protocol::ShellOwner::Schedule { schedule_id } => Some(schedule_id.clone()),
            };
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
                if schedule_id.is_some() {
                    workspace
                        .agents
                        .iter()
                        .filter(|agent| {
                            agent.workspace_id == workspace.id
                                && agent.shell_id == shell.id
                                && agent.run_id == run.id
                                && !matches!(
                                    agent.observation.state,
                                    AgentState::Inactive | AgentState::Done
                                )
                        })
                        .max_by(|left, right| {
                            left.observation
                                .observed_at_ms
                                .cmp(&right.observation.observed_at_ms)
                                .then_with(|| left.id.cmp(&right.id))
                        })
                } else if matches!(shell.status, ShellStatus::Running) {
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
                (Some(agent), _) => Some(WorkspaceItemView::AgentShell(AgentShellView {
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
                    schedule_id,
                })),
                (None, _) if schedule_id.is_some() => None,
                (None, Some(process))
                    if boomux::integrations::by_foreground_process(process).is_some()
                        && !suppress_foreground_hint =>
                {
                    Some(WorkspaceItemView::AgentShell(AgentShellView {
                        shell: shell_view,
                        agent: None,
                        schedule_id: None,
                    }))
                }
                (None, _) => Some(WorkspaceItemView::Shell(shell_view)),
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
    let schedules = workspace.schedules.iter().map(|schedule| {
        WorkspaceItemView::Schedule(ScheduleItemView {
            id: schedule.id.clone(),
            name: schedule.name.clone(),
            integration: schedule.integration.clone(),
            state: match schedule.state {
                AgentScheduleState::Paused => ScheduleDisplayState::Paused,
                AgentScheduleState::Enabled => ScheduleDisplayState::Enabled,
            },
            friendly_trigger: friendly_trigger(&schedule.trigger.cron),
        })
    });
    let items = shells
        .into_iter()
        .chain(launchers)
        .chain(schedules)
        .collect::<Vec<_>>();
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
        AgentAuthority, AgentInstanceSnapshot, AgentObservationSnapshot,
        AgentScheduleOverlapPolicy, AgentScheduleSession, AgentScheduleSnapshot,
        AgentScheduleTrigger, ScheduledExecutionDispatchKind, ShellOwner, ShellRunSnapshot,
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
                owner: boomux::protocol::ShellOwner::User,
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
            schedules: Vec::new(),
            agents: Vec::new(),
        }
    }

    fn schedule() -> AgentScheduleSnapshot {
        AgentScheduleSnapshot {
            id: "schedule-1".into(),
            workspace_id: "workspace-1".into(),
            name: "review".into(),
            cwd: "/tmp/project".into(),
            integration: "opencode".into(),
            session: AgentScheduleSession::Fresh,
            trigger: AgentScheduleTrigger {
                cron: "30 9 * * 1-5".into(),
                timezone: "UTC".into(),
            },
            state: AgentScheduleState::Enabled,
            overlap_policy: AgentScheduleOverlapPolicy::Skip,
            revision: 1,
            prompt_revision: 2,
            trigger_revision: 1,
            created_at_ms: 1,
            updated_at_ms: 1,
            evaluation_frontier_ms: 1,
            execution_shell_id: Some("shell-1".into()),
            next_occurrence: None,
        }
    }

    fn execution(state: ScheduledExecutionState) -> ScheduledExecutionSnapshot {
        ScheduledExecutionSnapshot {
            id: format!("execution-{state:?}"),
            workspace_id: "workspace-1".into(),
            schedule_id: "schedule-1".into(),
            revision: 1,
            state,
            dispatch_kind: ScheduledExecutionDispatchKind::Manual,
            dispatch_key: "00000000-0000-0000-0000-000000000001".into(),
            schedule_revision: 1,
            prompt_revision: 2,
            trigger_revision: 1,
            requested_at_ms: 1,
            scheduled_at_ms: None,
            coalesced_through_ms: None,
            started_at_ms: None,
            ended_at_ms: None,
            cwd: "/tmp/project".into(),
            integration: "opencode".into(),
            session: AgentScheduleSession::Fresh,
            reason: None,
            outcome: None,
            shell_id: Some("shell-1".into()),
            run_id: Some("run-1".into()),
            agent_id: None,
            external_session_id: None,
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
            observed_capabilities: vec!["recovered_agent_presentation".into()],
            workspace_owner_eligible: true,
            workspace_owner_unavailable_reason: None,
            scheduler: boomux::protocol::SchedulerHealth {
                state: boomux::protocol::SchedulerState::Active,
                max_concurrent: 4,
                active_executions: 0,
            },
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
                    owner: ShellOwner::User,
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
                schedules: Vec::new(),
                executions: Vec::new(),
                executions_truncated: false,
                scheduler: boomux::protocol::SchedulerHealth {
                    state: boomux::protocol::SchedulerState::Active,
                    max_concurrent: 4,
                    active_executions: 0,
                },
            }),
        };

        let projection = node.remote_projection.as_mut().unwrap();
        let mut distractor = projection.agents[0].clone();
        distractor.id = "agent-2".into();
        distractor.name = "wrong-agent".into();
        distractor.integration = "native-test".into();
        distractor.observed_at_ms = 3;
        projection.agents.push(distractor);

        let (views, _) = project_remote_node(&node);
        let WorkspaceItemView::AgentShell(agent) = &views[0].items[0] else {
            panic!("expected recovered remote Agent shell");
        };
        assert_eq!(agent.agent.as_ref().unwrap().id, "agent-1");
        assert_eq!(agent.state(), AgentDisplayState::Inactive);

        node.remote_projection.as_mut().unwrap().shells[0].run_id = None;
        let (views, _) = project_remote_node(&node);
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

    #[test]
    fn schedule_definition_projects_once_while_its_owned_shell_stays_hidden() {
        let mut workspace = workspace(Vec::new());
        workspace.shells[0].owner = ShellOwner::Schedule {
            schedule_id: "schedule-1".into(),
        };
        workspace.schedules.push(schedule());

        let views = project(&[workspace], &mut git::Cache::default());
        let [WorkspaceItemView::Schedule(schedule)] = views[0].items.as_slice() else {
            panic!("expected one schedule definition row");
        };
        assert_eq!(schedule.id, "schedule-1");
        assert_eq!(schedule.name, "review");
        assert_eq!(schedule.state, ScheduleDisplayState::Enabled);
        assert_eq!(schedule.friendly_trigger, "weekdays 09:30");
    }

    #[test]
    fn schedule_owned_shell_with_exact_blocked_agent_projects_only_as_schedule_agent() {
        let mut workspace = workspace(Vec::new());
        workspace.shells[0].owner = ShellOwner::Schedule {
            schedule_id: "schedule-1".into(),
        };
        workspace.schedules.push(schedule());
        workspace.agents.push(AgentInstanceSnapshot {
            id: "scheduled-agent".into(),
            workspace_id: "workspace-1".into(),
            shell_id: "shell-1".into(),
            run_id: "run-1".into(),
            name: "scheduled review".into(),
            integration: "opencode".into(),
            external_session_id: Some("scheduled-session".into()),
            cwd: Some("/tmp/project".into()),
            started_at_ms: 1,
            ended_at_ms: None,
            observation: AgentObservationSnapshot {
                revision: 4,
                state: AgentState::Blocked,
                authority: AgentAuthority::LifecycleIntegration,
                evidence: "approval required".into(),
                confidence: 100,
                observed_at_ms: 4,
            },
            attention: None,
        });

        let views = project(&[workspace], &mut git::Cache::default());
        let [
            WorkspaceItemView::AgentShell(agent),
            WorkspaceItemView::Schedule(schedule),
        ] = views[0].items.as_slice()
        else {
            panic!("expected exact schedule Agent and one schedule definition row");
        };
        assert_eq!(agent.schedule_id.as_deref(), Some("schedule-1"));
        assert_eq!(
            agent.agent.as_ref().map(|agent| agent.id.as_str()),
            Some("scheduled-agent")
        );
        assert_eq!(agent.state(), AgentDisplayState::Blocked);
        assert_eq!(schedule.id, "schedule-1");
    }

    #[test]
    fn friendly_trigger_recognizes_only_canonical_convenience_shapes() {
        for (cron, expected) in [
            ("* * * * *", "every minute"),
            ("*/15 * * * *", "every 15 minutes"),
            ("0 * * * *", "every hour"),
            ("0 */6 * * *", "every 6 hours"),
            ("30 9 * * *", "daily 09:30"),
            ("30 9 * * 1-5", "weekdays 09:30"),
            ("5 8 * * 1", "weekly Mon 08:05"),
        ] {
            assert_eq!(friendly_trigger(cron), expected);
        }
        for cron in [
            "1,2 * * * *",
            "1-5 * * * *",
            "1/2 * * * *",
            "*/05 * * * *",
            "0 9,10 * * *",
            "0 9 * * 1,2",
            "0 9 1 * *",
        ] {
            assert_eq!(friendly_trigger(cron), "custom schedule", "{cron}");
        }
    }

    #[test]
    fn every_typed_execution_state_and_reason_maps_without_string_inference() {
        let mut workspace = workspace(Vec::new());
        workspace.schedules.push(schedule());
        let states = [
            (
                ScheduledExecutionState::Skipped,
                ExecutionDisplayState::Skipped,
            ),
            (
                ScheduledExecutionState::Claimed,
                ExecutionDisplayState::Claimed,
            ),
            (
                ScheduledExecutionState::Starting,
                ExecutionDisplayState::Starting,
            ),
            (
                ScheduledExecutionState::Active,
                ExecutionDisplayState::Active,
            ),
            (
                ScheduledExecutionState::DispatchFailed,
                ExecutionDisplayState::DispatchFailed,
            ),
            (
                ScheduledExecutionState::Exited,
                ExecutionDisplayState::Exited,
            ),
            (
                ScheduledExecutionState::Cancelled,
                ExecutionDisplayState::Cancelled,
            ),
            (
                ScheduledExecutionState::Interrupted,
                ExecutionDisplayState::Interrupted,
            ),
        ];
        let executions = states
            .iter()
            .enumerate()
            .map(|(index, (state, _))| {
                let mut execution = execution(*state);
                execution.id = format!("execution-{index}");
                execution.requested_at_ms = index as u64;
                execution
            })
            .collect::<Vec<_>>();
        let snapshot = Snapshot {
            workspaces: vec![workspace.clone()],
            focused_terminal: None,
            scheduler: None,
        };
        let projected = project_schedules(&snapshot, &executions, false, &BTreeMap::new());
        let mapped = projected[0]
            .executions
            .iter()
            .rev()
            .map(|execution| execution.state)
            .collect::<Vec<_>>();
        assert_eq!(
            mapped,
            states
                .iter()
                .map(|(_, expected)| *expected)
                .collect::<Vec<_>>()
        );

        let reasons = [
            ScheduledExecutionReason::Overlap,
            ScheduledExecutionReason::ActiveSession,
            ScheduledExecutionReason::WorkspaceCapacity,
            ScheduledExecutionReason::GlobalCapacity,
            ScheduledExecutionReason::Missed,
            ScheduledExecutionReason::PausedRace,
            ScheduledExecutionReason::InvalidTarget,
            ScheduledExecutionReason::RunnerStartFailed,
            ScheduledExecutionReason::HostSpawnFailed,
            ScheduledExecutionReason::CancelledByUser,
            ScheduledExecutionReason::ColdDaemonRecovery,
            ScheduledExecutionReason::RunnerExitedWithoutReport,
            ScheduledExecutionReason::DaemonShutdown,
        ];
        for reason in reasons {
            let mut execution = execution(ScheduledExecutionState::Skipped);
            execution.reason = Some(reason);
            assert!(execution_view(&workspace, &[], &execution).reason.is_some());
        }
    }

    #[test]
    fn execution_links_resolve_only_the_exact_agent_and_canonical_session() {
        let mut workspace = workspace(Vec::new());
        workspace.schedules.push(schedule());
        workspace.agents = vec![AgentInstanceSnapshot {
            id: "nearby-agent".into(),
            external_session_id: Some("nearby-session".into()),
            ..workspace
                .agents
                .first()
                .cloned()
                .unwrap_or_else(|| AgentInstanceSnapshot {
                    id: "template".into(),
                    workspace_id: "workspace-1".into(),
                    shell_id: "shell-1".into(),
                    run_id: "run-1".into(),
                    name: "agent".into(),
                    integration: "opencode".into(),
                    external_session_id: None,
                    cwd: Some("/tmp/project".into()),
                    started_at_ms: 1,
                    ended_at_ms: None,
                    observation: AgentObservationSnapshot {
                        revision: 1,
                        state: AgentState::Working,
                        authority: AgentAuthority::LifecycleIntegration,
                        evidence: "work".into(),
                        confidence: 100,
                        observed_at_ms: 1,
                    },
                    attention: None,
                })
        }];
        let mut exact = workspace.agents[0].clone();
        exact.id = "exact-agent".into();
        exact.external_session_id = Some("exact-session".into());
        exact.observation.state = AgentState::Blocked;
        workspace.agents.push(exact);
        let sessions = session_projection::project_workspaces_with_catalog(
            std::slice::from_ref(&workspace),
            None,
        );
        let mut execution = execution(ScheduledExecutionState::Active);
        execution.agent_id = Some("exact-agent".into());
        execution.external_session_id = Some("exact-session".into());

        let view = execution_view(&workspace, &sessions, &execution);
        assert_eq!(view.agent_id.as_deref(), Some("exact-agent"));
        assert_eq!(view.agent_state, Some(AgentDisplayState::Blocked));
        let exact_session = sessions
            .iter()
            .find(|session| {
                session
                    .occurrences
                    .iter()
                    .any(|occurrence| occurrence.agent_id == "exact-agent")
            })
            .unwrap();
        assert_eq!(view.session_id.as_deref(), Some(exact_session.id.as_str()));

        execution.agent_id = Some("missing-agent".into());
        let missing = execution_view(&workspace, &sessions, &execution);
        assert_eq!(missing.agent_state, None);
        assert_eq!(missing.session_id, None);
    }
}

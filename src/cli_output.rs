use std::error::Error;
use std::io;

use serde::Serialize;

use boomux::client::{ClientError, LifecycleError, ProtocolError};
use boomux::protocol::{
    AgentAttentionReason, AgentAuthority, AgentInstanceSnapshot, AgentObservationSnapshot,
    AgentScheduleOverlapPolicy, AgentScheduleSession, AgentScheduleSnapshot, AgentScheduleState,
    AgentState, ErrorCode, ScheduledExecutionOutcome, ScheduledExecutionReason,
    ScheduledExecutionSnapshot, ShellRunExitReason, ShellSnapshot, ShellStatus,
    WorkspaceLauncherSnapshot,
};

use crate::agent_attention_projection::AgentStateCounts;
use crate::session_projection::{SessionOccurrence, SessionProjection};

pub(crate) const SCHEMA: &str = "boomux.cli/v1";

#[derive(Serialize)]
struct Envelope<'a, T> {
    schema: &'static str,
    command: &'a str,
    data: T,
}

#[derive(Serialize)]
struct ErrorEnvelope<'a> {
    schema: &'static str,
    command: &'a str,
    error: ErrorData<'a>,
}

#[derive(Serialize)]
struct ErrorData<'a> {
    code: &'a str,
    message: String,
}

#[derive(Debug)]
pub(crate) struct CliError {
    code: &'static str,
    message: String,
}

impl std::fmt::Display for CliError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl Error for CliError {}

#[derive(Serialize)]
pub(crate) struct ShellData {
    pub(crate) id: String,
    pub(crate) workspace_id: String,
    pub(crate) workspace_name: Option<String>,
    pub(crate) name: String,
    pub(crate) cwd: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(crate) command: Vec<String>,
    pub(crate) owner: &'static str,
    pub(crate) owner_schedule_id: Option<String>,
    pub(crate) status: &'static str,
    pub(crate) exit_code: Option<u32>,
    pub(crate) foreground_process: Option<String>,
    pub(crate) run: Option<RunData>,
}

#[derive(Serialize)]
pub(crate) struct RunData {
    id: String,
    generation: u64,
    started_at_ms: u64,
    ended_at_ms: Option<u64>,
    exit_reason: Option<&'static str>,
    exit_code: Option<u32>,
    output_revision: u64,
    environment_has_run_id: bool,
}

#[derive(Serialize)]
pub(crate) struct WorkspaceSummary {
    pub(crate) id: String,
    pub(crate) name: String,
    pub(crate) shell_count: usize,
    pub(crate) launcher_count: usize,
    pub(crate) schedule_count: usize,
    pub(crate) agent_count: usize,
    pub(crate) agent_state_counts: AgentStateCounts,
    pub(crate) attention_count: usize,
}

#[derive(Serialize)]
pub(crate) struct LauncherData {
    pub(crate) id: String,
    pub(crate) workspace_id: String,
    pub(crate) workspace_name: Option<String>,
    pub(crate) name: String,
    pub(crate) cwd: String,
    pub(crate) command: Vec<String>,
}

#[derive(Serialize)]
pub(crate) struct ScheduleData {
    pub(crate) id: String,
    pub(crate) workspace_id: String,
    pub(crate) workspace_name: Option<String>,
    pub(crate) name: String,
    pub(crate) cwd: String,
    pub(crate) integration: String,
    pub(crate) session_mode: &'static str,
    pub(crate) external_session_id: Option<String>,
    pub(crate) cron: String,
    pub(crate) timezone: String,
    pub(crate) state: &'static str,
    pub(crate) overlap_policy: &'static str,
    pub(crate) revision: u64,
    pub(crate) prompt_revision: u64,
    pub(crate) trigger_revision: u64,
    pub(crate) created_at_ms: u64,
    pub(crate) updated_at_ms: u64,
    pub(crate) evaluation_frontier_ms: u64,
    pub(crate) execution_shell_id: Option<String>,
    pub(crate) next_occurrence: Option<ScheduledOccurrenceData>,
}

#[derive(Serialize)]
pub(crate) struct ScheduledOccurrenceData {
    pub(crate) trigger_revision: u64,
    pub(crate) scheduled_at_ms: u64,
}

#[derive(Serialize)]
pub(crate) struct ScheduleInspectionData {
    #[serde(flatten)]
    pub(crate) schedule: ScheduleData,
    pub(crate) prompt: String,
}

#[derive(Serialize)]
pub(crate) struct ExecutionData {
    pub(crate) id: String,
    pub(crate) workspace_id: String,
    pub(crate) schedule_id: String,
    pub(crate) revision: u64,
    pub(crate) state: &'static str,
    pub(crate) dispatch_kind: &'static str,
    pub(crate) dispatch_key: String,
    pub(crate) schedule_revision: u64,
    pub(crate) prompt_revision: u64,
    pub(crate) trigger_revision: u64,
    pub(crate) requested_at_ms: u64,
    pub(crate) scheduled_at_ms: Option<u64>,
    pub(crate) coalesced_through_ms: Option<u64>,
    pub(crate) started_at_ms: Option<u64>,
    pub(crate) ended_at_ms: Option<u64>,
    pub(crate) cwd: String,
    pub(crate) integration: String,
    pub(crate) session: AgentScheduleSession,
    pub(crate) reason: Option<&'static str>,
    pub(crate) outcome: Option<ExecutionOutcomeData>,
    pub(crate) shell_id: Option<String>,
    pub(crate) run_id: Option<String>,
    pub(crate) agent_id: Option<String>,
    pub(crate) external_session_id: Option<String>,
}

#[derive(Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum ExecutionOutcomeData {
    ExitCode { code: i32 },
    Signal { signal: i32 },
}

#[derive(Serialize)]
pub(crate) struct AgentData {
    pub(crate) id: String,
    pub(crate) workspace_id: String,
    pub(crate) workspace_name: Option<String>,
    pub(crate) shell_id: String,
    pub(crate) run_id: String,
    pub(crate) name: String,
    pub(crate) integration: String,
    pub(crate) external_session_id: Option<String>,
    pub(crate) started_at_ms: u64,
    pub(crate) ended_at_ms: Option<u64>,
    pub(crate) attention: Option<AgentAttentionData>,
    pub(crate) observation: AgentObservationData,
}

#[derive(Serialize)]
pub(crate) struct AgentAttentionData {
    reason: &'static str,
    observation: AgentObservationData,
}

#[derive(Serialize)]
pub(crate) struct AgentObservationData {
    revision: u64,
    state: &'static str,
    authority: &'static str,
    evidence: String,
    confidence: u8,
    observed_at_ms: u64,
}

#[derive(Serialize)]
pub(crate) struct SessionSummaryData {
    pub(crate) id: String,
    pub(crate) workspace_id: String,
    pub(crate) workspace_name: String,
    pub(crate) description: String,
    pub(crate) integration: String,
    pub(crate) external_session_id: Option<String>,
    pub(crate) state: &'static str,
    pub(crate) state_is_current: bool,
    pub(crate) started_at_ms: u64,
    pub(crate) last_at_ms: u64,
    pub(crate) occurrence_count: usize,
}

#[derive(Serialize)]
pub(crate) struct SessionData {
    #[serde(flatten)]
    pub(crate) summary: SessionSummaryData,
    pub(crate) source_cwd: Option<String>,
    pub(crate) occurrences: Vec<SessionOccurrenceData>,
}

#[derive(Serialize)]
pub(crate) struct SessionOccurrenceData {
    pub(crate) agent_id: String,
    pub(crate) shell_id: String,
    pub(crate) retained_shell_name: Option<String>,
    pub(crate) retained_shell_cwd: Option<String>,
    pub(crate) source_cwd: Option<String>,
    pub(crate) run_id: String,
    pub(crate) started_at_ms: u64,
    pub(crate) ended_at_ms: Option<u64>,
    pub(crate) is_current: bool,
    pub(crate) observation: AgentObservationData,
}

pub(crate) fn print<T: Serialize>(command: &str, data: T) -> Result<(), Box<dyn Error>> {
    println!(
        "{}",
        serde_json::to_string_pretty(&Envelope {
            schema: SCHEMA,
            command,
            data,
        })?
    );
    Ok(())
}

pub(crate) fn print_error(command: &str, error: &(dyn Error + 'static)) {
    print_error_message(command, classify_error(command, error), error.to_string());
}

pub(crate) fn print_error_message(command: &str, code: &str, message: impl Into<String>) {
    let envelope = ErrorEnvelope {
        schema: SCHEMA,
        command,
        error: ErrorData {
            code,
            message: message.into(),
        },
    };
    match serde_json::to_string_pretty(&envelope) {
        Ok(json) => eprintln!("{json}"),
        Err(_) => eprintln!(
            "{{\"schema\":\"boomux.cli/v1\",\"command\":\"unknown\",\"error\":{{\"code\":\"internal\",\"message\":\"could not serialize error\"}}}}"
        ),
    }
}

pub(crate) fn failure(code: &'static str, message: impl Into<String>) -> Box<dyn Error> {
    Box::new(CliError {
        code,
        message: message.into(),
    })
}

pub(crate) fn shell(shell: &ShellSnapshot, workspace_name: Option<&str>) -> ShellData {
    let (status, exit_code) = match shell.status {
        ShellStatus::Pending => ("pending", None),
        ShellStatus::Running => ("running", None),
        ShellStatus::Exited { code } => ("exited", code),
    };
    ShellData {
        id: shell.id.clone(),
        workspace_id: shell.workspace_id.clone(),
        workspace_name: workspace_name.map(str::to_owned),
        name: shell.name.clone(),
        cwd: shell.cwd.display().to_string(),
        command: shell.command.clone(),
        owner: match &shell.owner {
            boomux::protocol::ShellOwner::User => "user",
            boomux::protocol::ShellOwner::Schedule { .. } => "schedule",
        },
        owner_schedule_id: match &shell.owner {
            boomux::protocol::ShellOwner::User => None,
            boomux::protocol::ShellOwner::Schedule { schedule_id } => Some(schedule_id.clone()),
        },
        status,
        exit_code,
        foreground_process: shell.foreground_process.clone(),
        run: shell.run.as_ref().map(|run| {
            let (exit_reason, exit_code) = match run.exit_reason.as_ref() {
                Some(ShellRunExitReason::Exited { code }) => (Some("exited"), *code),
                Some(ShellRunExitReason::Terminated) => (Some("terminated"), None),
                Some(ShellRunExitReason::Interrupted) => (Some("interrupted"), None),
                None => (None, None),
            };
            RunData {
                id: run.id.clone(),
                generation: run.generation,
                started_at_ms: run.started_at_ms,
                ended_at_ms: run.ended_at_ms,
                exit_reason,
                exit_code,
                output_revision: run.output_revision,
                environment_has_run_id: run.environment_has_run_id,
            }
        }),
    }
}

pub(crate) fn launcher(
    launcher: &WorkspaceLauncherSnapshot,
    workspace_name: Option<&str>,
) -> LauncherData {
    LauncherData {
        id: launcher.id.clone(),
        workspace_id: launcher.workspace_id.clone(),
        workspace_name: workspace_name.map(str::to_owned),
        name: launcher.name.clone(),
        cwd: launcher.cwd.display().to_string(),
        command: launcher.command.clone(),
    }
}

pub(crate) fn execution(execution: &ScheduledExecutionSnapshot) -> ExecutionData {
    ExecutionData {
        id: execution.id.clone(),
        workspace_id: execution.workspace_id.clone(),
        schedule_id: execution.schedule_id.clone(),
        revision: execution.revision,
        state: match execution.state {
            boomux::protocol::ScheduledExecutionState::Skipped => "skipped",
            boomux::protocol::ScheduledExecutionState::Claimed => "claimed",
            boomux::protocol::ScheduledExecutionState::Starting => "starting",
            boomux::protocol::ScheduledExecutionState::Active => "active",
            boomux::protocol::ScheduledExecutionState::DispatchFailed => "dispatch_failed",
            boomux::protocol::ScheduledExecutionState::Exited => "exited",
            boomux::protocol::ScheduledExecutionState::Cancelled => "cancelled",
            boomux::protocol::ScheduledExecutionState::Interrupted => "interrupted",
        },
        dispatch_kind: match execution.dispatch_kind {
            boomux::protocol::ScheduledExecutionDispatchKind::Manual => "manual",
            boomux::protocol::ScheduledExecutionDispatchKind::Timed => "timed",
        },
        dispatch_key: execution.dispatch_key.clone(),
        schedule_revision: execution.schedule_revision,
        prompt_revision: execution.prompt_revision,
        trigger_revision: execution.trigger_revision,
        requested_at_ms: execution.requested_at_ms,
        scheduled_at_ms: execution.scheduled_at_ms,
        coalesced_through_ms: execution.coalesced_through_ms,
        started_at_ms: execution.started_at_ms,
        ended_at_ms: execution.ended_at_ms,
        cwd: execution.cwd.display().to_string(),
        integration: execution.integration.clone(),
        session: execution.session.clone(),
        reason: execution.reason.map(execution_reason),
        outcome: execution.outcome.as_ref().map(|outcome| match outcome {
            ScheduledExecutionOutcome::ExitCode { code } => {
                ExecutionOutcomeData::ExitCode { code: *code }
            }
            ScheduledExecutionOutcome::Signal { signal } => {
                ExecutionOutcomeData::Signal { signal: *signal }
            }
        }),
        shell_id: execution.shell_id.clone(),
        run_id: execution.run_id.clone(),
        agent_id: execution.agent_id.clone(),
        external_session_id: execution.external_session_id.clone(),
    }
}

pub(crate) fn execution_reason(reason: ScheduledExecutionReason) -> &'static str {
    match reason {
        ScheduledExecutionReason::Overlap => "overlap",
        ScheduledExecutionReason::ActiveSession => "active_session",
        ScheduledExecutionReason::WorkspaceCapacity => "workspace_capacity",
        ScheduledExecutionReason::GlobalCapacity => "global_capacity",
        ScheduledExecutionReason::Missed => "missed",
        ScheduledExecutionReason::PausedRace => "paused_race",
        ScheduledExecutionReason::InvalidTarget => "invalid_target",
        ScheduledExecutionReason::RunnerStartFailed => "runner_start_failed",
        ScheduledExecutionReason::HostSpawnFailed => "host_spawn_failed",
        ScheduledExecutionReason::CancelledByUser => "cancelled_by_user",
        ScheduledExecutionReason::ColdDaemonRecovery => "cold_daemon_recovery",
        ScheduledExecutionReason::RunnerExitedWithoutReport => "runner_exited_without_report",
        ScheduledExecutionReason::DaemonShutdown => "daemon_shutdown",
    }
}

pub(crate) fn schedule(
    schedule: &AgentScheduleSnapshot,
    workspace_name: Option<&str>,
) -> ScheduleData {
    let (session_mode, external_session_id) = match &schedule.session {
        AgentScheduleSession::Fresh => ("fresh", None),
        AgentScheduleSession::Continue {
            external_session_id,
        } => ("continue", Some(external_session_id.clone())),
    };
    ScheduleData {
        id: schedule.id.clone(),
        workspace_id: schedule.workspace_id.clone(),
        workspace_name: workspace_name.map(str::to_owned),
        name: schedule.name.clone(),
        cwd: schedule.cwd.display().to_string(),
        integration: schedule.integration.clone(),
        session_mode,
        external_session_id,
        cron: schedule.trigger.cron.clone(),
        timezone: schedule.trigger.timezone.clone(),
        state: schedule_state(schedule.state),
        overlap_policy: match schedule.overlap_policy {
            AgentScheduleOverlapPolicy::Skip => "skip",
        },
        revision: schedule.revision,
        prompt_revision: schedule.prompt_revision,
        trigger_revision: schedule.trigger_revision,
        created_at_ms: schedule.created_at_ms,
        updated_at_ms: schedule.updated_at_ms,
        evaluation_frontier_ms: schedule.evaluation_frontier_ms,
        execution_shell_id: schedule.execution_shell_id.clone(),
        next_occurrence: schedule.next_occurrence.as_ref().map(|occurrence| {
            ScheduledOccurrenceData {
                trigger_revision: occurrence.trigger_revision,
                scheduled_at_ms: occurrence.scheduled_at_ms,
            }
        }),
    }
}

pub(crate) fn schedule_inspection(
    schedule: &AgentScheduleSnapshot,
    workspace_name: Option<&str>,
    prompt: &str,
) -> ScheduleInspectionData {
    ScheduleInspectionData {
        schedule: self::schedule(schedule, workspace_name),
        prompt: prompt.to_owned(),
    }
}

pub(crate) fn schedule_state(state: AgentScheduleState) -> &'static str {
    match state {
        AgentScheduleState::Paused => "paused",
        AgentScheduleState::Enabled => "enabled",
    }
}

pub(crate) fn agent(agent: &AgentInstanceSnapshot, workspace_name: Option<&str>) -> AgentData {
    AgentData {
        id: agent.id.clone(),
        workspace_id: agent.workspace_id.clone(),
        workspace_name: workspace_name.map(str::to_owned),
        shell_id: agent.shell_id.clone(),
        run_id: agent.run_id.clone(),
        name: agent.name.clone(),
        integration: agent.integration.clone(),
        external_session_id: agent.external_session_id.clone(),
        started_at_ms: agent.started_at_ms,
        ended_at_ms: agent.ended_at_ms,
        attention: agent
            .attention
            .as_ref()
            .map(|attention| AgentAttentionData {
                reason: agent_attention_reason(attention.reason),
                observation: agent_observation(&attention.observation),
            }),
        observation: agent_observation(&agent.observation),
    }
}

pub(crate) fn session_summary(session: &SessionProjection) -> SessionSummaryData {
    SessionSummaryData {
        id: session.id.clone(),
        workspace_id: session.workspace_id.clone(),
        workspace_name: session.workspace_name.clone(),
        description: session.description.clone(),
        integration: session.integration.clone(),
        external_session_id: session.external_session_id.clone(),
        state: agent_state(session.state),
        state_is_current: session.state_is_current,
        started_at_ms: session.started_at_ms,
        last_at_ms: session.last_at_ms,
        occurrence_count: session.occurrences.len(),
    }
}

pub(crate) fn session(session: &SessionProjection) -> SessionData {
    SessionData {
        summary: session_summary(session),
        source_cwd: session
            .source_cwd
            .as_ref()
            .map(|cwd| cwd.display().to_string()),
        occurrences: session.occurrences.iter().map(session_occurrence).collect(),
    }
}

fn session_occurrence(occurrence: &SessionOccurrence) -> SessionOccurrenceData {
    SessionOccurrenceData {
        agent_id: occurrence.agent_id.clone(),
        shell_id: occurrence.shell_id.clone(),
        retained_shell_name: occurrence.retained_shell_name.clone(),
        retained_shell_cwd: occurrence
            .retained_shell_cwd
            .as_ref()
            .map(|cwd| cwd.display().to_string()),
        source_cwd: occurrence
            .source_cwd
            .as_ref()
            .map(|cwd| cwd.display().to_string()),
        run_id: occurrence.run_id.clone(),
        started_at_ms: occurrence.started_at_ms,
        ended_at_ms: occurrence.ended_at_ms,
        is_current: occurrence.is_current,
        observation: agent_observation(&occurrence.observation),
    }
}

fn agent_observation(observation: &AgentObservationSnapshot) -> AgentObservationData {
    AgentObservationData {
        revision: observation.revision,
        state: agent_state(observation.state),
        authority: agent_authority(observation.authority),
        evidence: observation.evidence.clone(),
        confidence: observation.confidence,
        observed_at_ms: observation.observed_at_ms,
    }
}

pub(crate) fn agent_state(state: AgentState) -> &'static str {
    match state {
        AgentState::Unknown => "unknown",
        AgentState::Working => "working",
        AgentState::Blocked => "blocked",
        AgentState::Idle => "idle",
        AgentState::Inactive => "inactive",
        AgentState::Done => "done",
    }
}

pub(crate) fn agent_attention_reason(reason: AgentAttentionReason) -> &'static str {
    match reason {
        AgentAttentionReason::Blocked => "blocked",
        AgentAttentionReason::Completed => "completed",
    }
}

pub(crate) fn agent_authority(authority: AgentAuthority) -> &'static str {
    match authority {
        AgentAuthority::LifecycleIntegration => "lifecycle_integration",
        AgentAuthority::ProcessAdapter => "process_adapter",
        AgentAuthority::TerminalHeuristic => "terminal_heuristic",
        AgentAuthority::DaemonLifecycle => "daemon_lifecycle",
    }
}

fn classify_error(command: &str, error: &(dyn Error + 'static)) -> &'static str {
    let mut current = Some(error);
    while let Some(candidate) = current {
        if let Some(cli) = candidate.downcast_ref::<CliError>() {
            return cli.code;
        }
        if let Some(client) = candidate.downcast_ref::<ClientError>() {
            return classify_client_error(command, client);
        }
        if candidate
            .downcast_ref::<boomux::scheduling::SchedulingError>()
            .is_some()
        {
            return "invalid_argument";
        }
        if let Some(io_error) = candidate.downcast_ref::<io::Error>() {
            return classify_io_error(command, io_error);
        }
        current = candidate.source();
    }
    "internal"
}

fn classify_client_error(command: &str, error: &ClientError) -> &'static str {
    match error {
        ClientError::Transport(error) | ClientError::Validation(error) => {
            classify_io_error(command, error)
        }
        ClientError::Protocol(ProtocolError::UnsupportedVersion(_)) => "unsupported_version",
        ClientError::Protocol(_) => "invalid_argument",
        ClientError::Remote(error) => error.code.map_or("unknown", protocol_error_code),
        ClientError::Lifecycle(LifecycleError::ShutdownTimeout) => "timeout",
        ClientError::Lifecycle(LifecycleError::DaemonStart(_))
        | ClientError::Lifecycle(LifecycleError::DaemonStartTimeout(_)) => "daemon_unavailable",
        ClientError::Lifecycle(LifecycleError::ReplacementStartTimeout(Some(error)))
        | ClientError::Lifecycle(LifecycleError::AttachmentReconnectTimeout(Some(error))) => {
            classify_client_error(command, error)
        }
        ClientError::Lifecycle(LifecycleError::ReplacementStartTimeout(None))
        | ClientError::Lifecycle(LifecycleError::AttachmentReconnectTimeout(None)) => "internal",
    }
}

fn classify_io_error(command: &str, error: &io::Error) -> &'static str {
    if command == "daemon.status"
        && matches!(
            error.kind(),
            io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
        )
    {
        return "daemon_unavailable";
    }
    match error.kind() {
        io::ErrorKind::InvalidInput | io::ErrorKind::InvalidData => "invalid_argument",
        io::ErrorKind::NotFound => "not_found",
        io::ErrorKind::AlreadyExists => "already_exists",
        io::ErrorKind::WouldBlock | io::ErrorKind::AddrInUse => "busy",
        io::ErrorKind::ConnectionAborted => "daemon_stopping",
        io::ErrorKind::ConnectionRefused => "daemon_unavailable",
        io::ErrorKind::TimedOut => "timeout",
        _ => "internal",
    }
}

#[cfg(test)]
pub(crate) fn classify_for_test(command: &str, error: &(dyn Error + 'static)) -> &'static str {
    classify_error(command, error)
}

fn protocol_error_code(code: ErrorCode) -> &'static str {
    match code {
        ErrorCode::InvalidArgument => "invalid_argument",
        ErrorCode::NotFound => "not_found",
        ErrorCode::AlreadyExists => "already_exists",
        ErrorCode::Busy => "busy",
        ErrorCode::DaemonStopping => "daemon_stopping",
        ErrorCode::ShellStartFailed => "shell_start_failed",
        ErrorCode::PersistenceFailed => "persistence_failed",
        ErrorCode::Timeout => "timeout",
        ErrorCode::UnsupportedVersion => "unsupported_version",
        ErrorCode::CursorExpired => "cursor_expired",
        ErrorCode::RunChanged => "run_changed",
        ErrorCode::RevisionAhead => "revision_ahead",
        ErrorCode::IdempotencyExpired => "idempotency_expired",
        ErrorCode::Internal => "internal",
        ErrorCode::Unknown => "unknown",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use boomux::client::RemoteError;
    use boomux::protocol::AgentObservationSnapshot;

    #[test]
    fn agent_json_uses_the_stable_cli_shape() {
        let data = agent(
            &AgentInstanceSnapshot {
                id: "a1".into(),
                workspace_id: "w1".into(),
                shell_id: "s1".into(),
                run_id: "r1".into(),
                name: "opencode".into(),
                integration: "plugin".into(),
                external_session_id: Some("external-1".into()),
                cwd: Some("/tmp/project".into()),
                started_at_ms: 10,
                ended_at_ms: None,
                attention: Some(boomux::protocol::AgentAttentionSnapshot {
                    reason: AgentAttentionReason::Blocked,
                    observation: AgentObservationSnapshot {
                        revision: 1,
                        state: AgentState::Blocked,
                        authority: AgentAuthority::LifecycleIntegration,
                        evidence: "approval needed".into(),
                        confidence: 100,
                        observed_at_ms: 10,
                    },
                }),
                observation: AgentObservationSnapshot {
                    revision: 2,
                    state: AgentState::Working,
                    authority: AgentAuthority::LifecycleIntegration,
                    evidence: "tool call".into(),
                    confidence: 95,
                    observed_at_ms: 11,
                },
            },
            Some("project"),
        );

        let value = serde_json::to_value(data).unwrap();
        assert_eq!(value["workspace_name"], "project");
        assert_eq!(value["observation"]["state"], "working");
        assert_eq!(value["observation"]["authority"], "lifecycle_integration");
        assert_eq!(value["observation"]["confidence"], 95);
        assert_eq!(value["attention"]["reason"], "blocked");
        assert_eq!(value["attention"]["observation"]["revision"], 1);
    }

    #[test]
    fn session_json_uses_null_for_missing_optional_metadata() {
        let data = session(&SessionProjection {
            id: "projected-session".into(),
            workspace_id: "w1".into(),
            workspace_name: "project".into(),
            integration: "plugin".into(),
            external_session_id: None,
            description: "Agent".into(),
            state: AgentState::Inactive,
            state_is_current: false,
            started_at_ms: 10,
            last_at_ms: 11,
            source_cwd: Some("/tmp/project".into()),
            occurrences: vec![SessionOccurrence {
                agent_id: "a1".into(),
                shell_id: "removed-shell".into(),
                run_id: "r1".into(),
                started_at_ms: 10,
                ended_at_ms: None,
                observation: AgentObservationSnapshot {
                    revision: 2,
                    state: AgentState::Inactive,
                    authority: AgentAuthority::DaemonLifecycle,
                    evidence: "shell exited".into(),
                    confidence: 100,
                    observed_at_ms: 11,
                },
                is_current: false,
                retained_shell_name: None,
                retained_shell_cwd: None,
                source_cwd: Some("/tmp/project".into()),
            }],
        });

        let value = serde_json::to_value(data).unwrap();
        assert!(value["external_session_id"].is_null());
        assert_eq!(value["source_cwd"], "/tmp/project");
        assert!(value["occurrences"][0]["retained_shell_name"].is_null());
        assert!(value["occurrences"][0]["retained_shell_cwd"].is_null());
        assert_eq!(value["occurrences"][0]["source_cwd"], "/tmp/project");
        assert!(value["occurrences"][0]["ended_at_ms"].is_null());
        assert_eq!(value["occurrences"][0]["shell_id"], "removed-shell");
        assert_eq!(value["occurrences"][0]["observation"]["state"], "inactive");
    }

    #[test]
    fn schedule_summary_is_prompt_free_and_inspection_discloses_prompt() {
        let snapshot = AgentScheduleSnapshot {
            id: "schedule-1".into(),
            workspace_id: "w1".into(),
            name: "review".into(),
            cwd: "/tmp/project".into(),
            integration: "opencode".into(),
            session: AgentScheduleSession::Fresh,
            trigger: boomux::protocol::AgentScheduleTrigger {
                cron: "0 9 * * 1-5".into(),
                timezone: "UTC".into(),
            },
            state: AgentScheduleState::Paused,
            overlap_policy: AgentScheduleOverlapPolicy::Skip,
            revision: 1,
            prompt_revision: 2,
            trigger_revision: 1,
            created_at_ms: 10,
            updated_at_ms: 11,
            evaluation_frontier_ms: 11,
            execution_shell_id: None,
            next_occurrence: None,
        };
        let private = "private prompt contents\n";

        let summary = serde_json::to_value(schedule(&snapshot, Some("project"))).unwrap();
        assert!(summary.get("prompt").is_none());
        assert!(!summary.to_string().contains(private));
        assert!(summary["external_session_id"].is_null());
        assert!(summary["execution_shell_id"].is_null());
        assert_eq!(summary["session_mode"], "fresh");
        assert_eq!(summary["state"], "paused");

        let inspection =
            serde_json::to_value(schedule_inspection(&snapshot, Some("project"), private)).unwrap();
        assert_eq!(inspection["prompt"], private);
    }

    #[test]
    fn client_errors_convert_to_stable_cli_codes() {
        let remote = ClientError::Remote(RemoteError {
            code: Some(ErrorCode::ShellStartFailed),
            message: "could not start shell".into(),
        });
        let unsupported = ClientError::Protocol(ProtocolError::UnsupportedVersion(
            "daemon does not support this request".into(),
        ));
        let transport = ClientError::Transport(io::Error::new(
            io::ErrorKind::ConnectionRefused,
            "connection refused",
        ));
        let timeout = ClientError::Lifecycle(LifecycleError::ShutdownTimeout);
        let schedule = boomux::scheduling::canonicalize_cron("not a cron").unwrap_err();

        assert_eq!(classify_error("shell.open", &remote), "shell_start_failed");
        assert_eq!(
            classify_error("shell.open", &unsupported),
            "unsupported_version"
        );
        assert_eq!(
            classify_error("shell.open", &transport),
            "daemon_unavailable"
        );
        assert_eq!(classify_error("daemon.stop", &timeout), "timeout");
        assert_eq!(
            classify_error("schedule.create", &schedule),
            "invalid_argument"
        );
    }
}

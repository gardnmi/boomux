use std::io::{self, Read};

use boomux::protocol::AgentState;
use serde::Deserialize;

#[cfg(test)]
use crate::hook_input::MAX_HOOK_INPUT_BYTES;
use crate::hook_input::read_bounded_hook_input;

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
enum HookEvent {
    SessionStart,
    UserPromptSubmit,
    PreToolUse,
    PermissionRequest,
    PermissionDenied,
    PostToolUse,
    PostToolUseFailure,
    Notification,
    SubagentStart,
    SubagentStop,
    Stop,
    StopFailure,
    SessionEnd,
}

#[derive(Debug, Deserialize)]
struct HookInput {
    session_id: String,
    hook_event_name: HookEvent,
    #[serde(default)]
    agent_id: Option<String>,
    #[serde(default)]
    notification_type: Option<String>,
    #[serde(default)]
    background_tasks: Option<Vec<serde_json::Value>>,
    #[serde(default)]
    session_crons: Option<Vec<serde_json::Value>>,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct LifecycleObservation {
    pub(crate) state: AgentState,
    pub(crate) evidence: &'static str,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct HookUpdate {
    pub(crate) session_id: String,
    pub(crate) observation: Option<LifecycleObservation>,
    pub(crate) session_ended: bool,
}

pub(crate) fn read_update(reader: impl Read) -> Result<HookUpdate, Box<dyn std::error::Error>> {
    let input = read_bounded_hook_input(reader, "Claude")?;
    let input: HookInput = serde_json::from_slice(&input).map_err(|error| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("invalid Claude hook input: {error}"),
        )
    })?;
    boomux::scheduling::validate_external_session_id(&input.session_id).map_err(|error| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("invalid Claude session identity: {error}"),
        )
    })?;
    let session_ended = input.hook_event_name == HookEvent::SessionEnd && input.agent_id.is_none();
    let session_id = input.session_id.clone();
    Ok(HookUpdate {
        session_id,
        observation: reduce(input),
        session_ended,
    })
}

fn reduce(input: HookInput) -> Option<LifecycleObservation> {
    let in_subagent = input.agent_id.is_some();
    let (state, evidence) = match input.hook_event_name {
        HookEvent::SessionStart if in_subagent => {
            (AgentState::Working, "Claude subagent session started")
        }
        HookEvent::SessionStart => (AgentState::Idle, "Claude session idle"),
        HookEvent::UserPromptSubmit => (AgentState::Working, "Claude processing prompt"),
        HookEvent::PreToolUse => (AgentState::Working, "Claude using tool"),
        HookEvent::PermissionRequest => (AgentState::Blocked, "Claude awaiting tool permission"),
        HookEvent::PermissionDenied => (AgentState::Working, "Claude tool permission denied"),
        HookEvent::PostToolUse => (AgentState::Working, "Claude tool completed"),
        HookEvent::PostToolUseFailure => (AgentState::Working, "Claude tool failed"),
        HookEvent::Notification => match input.notification_type.as_deref() {
            Some(
                "permission_prompt"
                | "elicitation_dialog"
                | "elicitation_url_dialog"
                | "agent_needs_input",
            ) => (AgentState::Blocked, "Claude awaiting user input"),
            _ => return None,
        },
        HookEvent::SubagentStart => (AgentState::Working, "Claude subagent working"),
        HookEvent::SubagentStop => (AgentState::Working, "Claude subagent stopped"),
        HookEvent::Stop
            if input
                .background_tasks
                .as_ref()
                .is_some_and(|tasks| !tasks.is_empty())
                || input
                    .session_crons
                    .as_ref()
                    .is_some_and(|crons| !crons.is_empty()) =>
        {
            (AgentState::Working, "Claude background work active")
        }
        HookEvent::Stop => (AgentState::Idle, "Claude session idle"),
        HookEvent::StopFailure => (AgentState::Idle, "Claude turn failed"),
        HookEvent::SessionEnd if in_subagent => {
            (AgentState::Working, "Claude subagent session ended")
        }
        HookEvent::SessionEnd => (AgentState::Inactive, "Claude session inactive"),
    };
    Some(LifecycleObservation { state, evidence })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn update(json: &str) -> HookUpdate {
        read_update(json.as_bytes()).unwrap()
    }

    fn observation(json: &str) -> Option<LifecycleObservation> {
        update(json).observation
    }

    #[test]
    fn lifecycle_events_reduce_to_root_session_observations() {
        let cases = [
            ("SessionStart", AgentState::Idle, "Claude session idle"),
            (
                "UserPromptSubmit",
                AgentState::Working,
                "Claude processing prompt",
            ),
            ("PreToolUse", AgentState::Working, "Claude using tool"),
            (
                "PermissionRequest",
                AgentState::Blocked,
                "Claude awaiting tool permission",
            ),
            (
                "PermissionDenied",
                AgentState::Working,
                "Claude tool permission denied",
            ),
            ("PostToolUse", AgentState::Working, "Claude tool completed"),
            (
                "PostToolUseFailure",
                AgentState::Working,
                "Claude tool failed",
            ),
            (
                "SubagentStart",
                AgentState::Working,
                "Claude subagent working",
            ),
            (
                "SubagentStop",
                AgentState::Working,
                "Claude subagent stopped",
            ),
            ("Stop", AgentState::Idle, "Claude session idle"),
            ("StopFailure", AgentState::Idle, "Claude turn failed"),
            (
                "SessionEnd",
                AgentState::Inactive,
                "Claude session inactive",
            ),
        ];
        for (event, state, evidence) in cases {
            let actual = observation(&format!(
                r#"{{"session_id":"root-session","hook_event_name":"{event}","unknown_future_field":true}}"#
            ))
            .unwrap();
            assert_eq!(actual.state, state, "{event}");
            assert_eq!(actual.evidence, evidence, "{event}");
            assert_ne!(actual.state, AgentState::Done);
        }
    }

    #[test]
    fn subagent_session_boundaries_keep_the_root_working() {
        for event in ["SessionStart", "SessionEnd"] {
            let update = update(&format!(
                r#"{{"session_id":"root-session","hook_event_name":"{event}","agent_id":"subagent-1"}}"#
            ));
            let actual = update.observation.unwrap();
            assert_eq!(update.session_id, "root-session");
            assert_eq!(actual.state, AgentState::Working);
            assert!(!update.session_ended);
        }
    }

    #[test]
    fn stop_remains_working_while_background_work_exists() {
        for fields in [
            r#""background_tasks":[{"id":"task-1"}]"#,
            r#""session_crons":[{"id":"cron-1"}]"#,
        ] {
            assert_eq!(
                observation(&format!(
                    r#"{{"session_id":"root","hook_event_name":"Stop",{fields}}}"#
                )),
                Some(LifecycleObservation {
                    state: AgentState::Working,
                    evidence: "Claude background work active",
                })
            );
        }
    }

    #[test]
    fn only_input_notifications_block() {
        assert_eq!(
            observation(
                r#"{"session_id":"root","hook_event_name":"Notification","notification_type":"permission_prompt"}"#
            )
            .unwrap()
            .state,
            AgentState::Blocked
        );
        assert_eq!(
            observation(
                r#"{"session_id":"root","hook_event_name":"Notification","notification_type":"auth_success"}"#
            ),
            None
        );
        assert_eq!(
            update(
                r#"{"session_id":"root","hook_event_name":"Notification","notification_type":"auth_success"}"#
            )
            .session_id,
            "root"
        );
    }

    #[test]
    fn hook_input_and_session_identity_are_bounded() {
        let oversized = vec![b'x'; MAX_HOOK_INPUT_BYTES + 1];
        assert!(read_update(oversized.as_slice()).is_err());
        assert!(
            read_update(
                format!(
                    r#"{{"session_id":"{}","hook_event_name":"SessionStart"}}"#,
                    "x".repeat(boomux::scheduling::MAX_EXTERNAL_SESSION_ID_BYTES + 1)
                )
                .as_bytes()
            )
            .is_err()
        );
    }
}

use std::io::{self, Read};

use boomux::protocol::AgentState;
use serde::Deserialize;

#[cfg(test)]
use crate::hook_input::MAX_HOOK_INPUT_BYTES;
use crate::hook_input::read_bounded_hook_input;

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
enum HookEvent {
    SessionStart,
    SessionEnd,
    UserPromptSubmit,
    PreToolUse,
    PermissionRequest,
    PostToolUse,
    PreCompact,
    PostCompact,
    SubagentStart,
    SubagentStop,
    Stop,
}

#[derive(Debug, Deserialize)]
struct HookInput {
    session_id: String,
    hook_event_name: HookEvent,
    #[serde(default)]
    source: Option<String>,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct LifecycleObservation {
    pub(crate) state: AgentState,
    pub(crate) evidence: &'static str,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct HookUpdate {
    pub(crate) session_id: String,
    pub(crate) observation: LifecycleObservation,
}

pub(crate) fn read_update(reader: impl Read) -> Result<HookUpdate, Box<dyn std::error::Error>> {
    let input = read_bounded_hook_input(reader, "Codex")?;
    let input: HookInput = serde_json::from_slice(&input).map_err(|error| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("invalid Codex hook input: {error}"),
        )
    })?;
    boomux::integrations::validate_external_session_id(&input.session_id).map_err(|error| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("invalid Codex thread identity: {error}"),
        )
    })?;
    let observation = reduce(&input);
    Ok(HookUpdate {
        session_id: input.session_id,
        observation,
    })
}

fn reduce(input: &HookInput) -> LifecycleObservation {
    let (state, evidence) = match input.hook_event_name {
        HookEvent::SessionStart if input.source.as_deref() == Some("compact") => {
            (AgentState::Working, "Codex compacting session")
        }
        HookEvent::SessionStart => (AgentState::Idle, "Codex session idle"),
        HookEvent::SessionEnd => (AgentState::Inactive, "Codex session inactive"),
        HookEvent::UserPromptSubmit => (AgentState::Working, "Codex processing prompt"),
        HookEvent::PreToolUse => (AgentState::Working, "Codex using tool"),
        HookEvent::PermissionRequest => (AgentState::Blocked, "Codex awaiting permission"),
        HookEvent::PostToolUse => (AgentState::Working, "Codex tool completed"),
        HookEvent::PreCompact => (AgentState::Working, "Codex compacting session"),
        HookEvent::PostCompact => (AgentState::Working, "Codex session compacted"),
        HookEvent::SubagentStart => (AgentState::Working, "Codex subagent working"),
        HookEvent::SubagentStop => (AgentState::Working, "Codex subagent stopped"),
        HookEvent::Stop => (AgentState::Idle, "Codex session idle"),
    };
    LifecycleObservation { state, evidence }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn update(event: &str, extra: &str) -> HookUpdate {
        read_update(
            format!(
                r#"{{"session_id":"thread-1","hook_event_name":"{event}"{extra},"unknown":true}}"#
            )
            .as_bytes(),
        )
        .unwrap()
    }

    #[test]
    fn lifecycle_events_reduce_to_root_thread_observations() {
        let cases = [
            ("SessionStart", AgentState::Idle),
            ("SessionEnd", AgentState::Inactive),
            ("UserPromptSubmit", AgentState::Working),
            ("PreToolUse", AgentState::Working),
            ("PermissionRequest", AgentState::Blocked),
            ("PostToolUse", AgentState::Working),
            ("PreCompact", AgentState::Working),
            ("PostCompact", AgentState::Working),
            ("SubagentStart", AgentState::Working),
            ("SubagentStop", AgentState::Working),
            ("Stop", AgentState::Idle),
        ];
        for (event, state) in cases {
            let update = update(event, "");
            assert_eq!(update.session_id, "thread-1");
            assert_eq!(update.observation.state, state, "{event}");
            assert_ne!(update.observation.state, AgentState::Done);
        }
    }

    #[test]
    fn compact_session_start_remains_working() {
        assert_eq!(
            update("SessionStart", r#", "source":"compact""#)
                .observation
                .state,
            AgentState::Working
        );
    }

    #[test]
    fn hook_input_and_thread_identity_are_bounded() {
        let oversized = vec![b'x'; MAX_HOOK_INPUT_BYTES + 1];
        assert!(read_update(oversized.as_slice()).is_err());
        assert!(
            read_update(br#"{"session_id":"bad\nid","hook_event_name":"Stop"}"#.as_slice())
                .is_err()
        );
        assert!(
            read_update(br#"{"session_id":"thread","hook_event_name":"Future"}"#.as_slice())
                .is_err()
        );
    }
}

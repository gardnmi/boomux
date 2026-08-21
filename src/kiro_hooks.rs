use std::io::{self, Read};

use boomux::protocol::AgentState;
use serde::Deserialize;

const MAX_HOOK_INPUT_BYTES: usize = 1024 * 1024;

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
enum HookEvent {
    #[serde(alias = "agentSpawn")]
    SessionStart,
    #[serde(alias = "userPromptSubmit")]
    UserPromptSubmit,
    #[serde(alias = "preToolUse")]
    PreToolUse,
    #[serde(alias = "postToolUse")]
    PostToolUse,
    #[serde(alias = "stop")]
    Stop,
}

#[derive(Debug, Deserialize)]
struct HookInput {
    session_id: String,
    hook_event_name: HookEvent,
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
    let input = read_bounded_input(reader)?;
    let input: HookInput = serde_json::from_slice(&input).map_err(|error| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("invalid Kiro hook input: {error}"),
        )
    })?;
    boomux::scheduling::validate_external_session_id(&input.session_id).map_err(|error| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("invalid Kiro session identity: {error}"),
        )
    })?;
    Ok(HookUpdate {
        session_id: input.session_id,
        observation: reduce(input.hook_event_name),
    })
}

fn read_bounded_input(mut reader: impl Read) -> io::Result<Vec<u8>> {
    let mut input = Vec::new();
    reader
        .by_ref()
        .take((MAX_HOOK_INPUT_BYTES + 1) as u64)
        .read_to_end(&mut input)?;
    if input.len() > MAX_HOOK_INPUT_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("Kiro hook input exceeds {MAX_HOOK_INPUT_BYTES} bytes"),
        ));
    }
    Ok(input)
}

fn reduce(event: HookEvent) -> LifecycleObservation {
    let (state, evidence) = match event {
        HookEvent::SessionStart => (AgentState::Idle, "Kiro session idle"),
        HookEvent::UserPromptSubmit => (AgentState::Working, "Kiro processing prompt"),
        HookEvent::PreToolUse => (AgentState::Working, "Kiro using tool"),
        HookEvent::PostToolUse => (AgentState::Working, "Kiro tool completed"),
        HookEvent::Stop => (AgentState::Idle, "Kiro session idle"),
    };
    LifecycleObservation { state, evidence }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn update(event: &str) -> HookUpdate {
        read_update(
            format!(r#"{{"session_id":"session-1","hook_event_name":"{event}","unknown":true}}"#)
                .as_bytes(),
        )
        .unwrap()
    }

    #[test]
    fn lifecycle_events_reduce_without_inventing_unsupported_states() {
        let cases = [
            ("SessionStart", AgentState::Idle),
            ("UserPromptSubmit", AgentState::Working),
            ("PreToolUse", AgentState::Working),
            ("PostToolUse", AgentState::Working),
            ("Stop", AgentState::Idle),
        ];
        for (event, state) in cases {
            let update = update(event);
            assert_eq!(update.session_id, "session-1");
            assert_eq!(update.observation.state, state, "{event}");
            assert!(!matches!(
                update.observation.state,
                AgentState::Blocked | AgentState::Inactive | AgentState::Done
            ));
        }
    }

    #[test]
    fn documented_legacy_payload_names_remain_decodable() {
        for (event, state) in [
            ("agentSpawn", AgentState::Idle),
            ("userPromptSubmit", AgentState::Working),
            ("preToolUse", AgentState::Working),
            ("postToolUse", AgentState::Working),
            ("stop", AgentState::Idle),
        ] {
            assert_eq!(update(event).observation.state, state, "{event}");
        }
    }

    #[test]
    fn hook_input_and_session_identity_are_bounded() {
        let oversized = vec![b'x'; MAX_HOOK_INPUT_BYTES + 1];
        assert!(read_update(oversized.as_slice()).is_err());
        assert!(
            read_update(br#"{"session_id":"bad\nid","hook_event_name":"Stop"}"#.as_slice())
                .is_err()
        );
        assert!(
            read_update(br#"{"session_id":"session","hook_event_name":"Future"}"#.as_slice())
                .is_err()
        );
    }
}

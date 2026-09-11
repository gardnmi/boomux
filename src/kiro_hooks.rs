use std::io::{self, Read};
use std::path::PathBuf;

use boomux::protocol::AgentState;
use serde::Deserialize;

#[cfg(test)]
use crate::hook_input::MAX_HOOK_INPUT_BYTES;
use crate::hook_input::read_bounded_hook_input;

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
    #[serde(default)]
    cwd: Option<PathBuf>,
    #[serde(default, alias = "toolName")]
    tool_name: Option<String>,
    #[serde(default, alias = "toolInput")]
    tool_input: Option<serde_json::Value>,
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
    pub(crate) working_contexts: Vec<PathBuf>,
}

pub(crate) fn read_update(reader: impl Read) -> Result<HookUpdate, Box<dyn std::error::Error>> {
    let input = read_bounded_hook_input(reader, "Kiro")?;
    let input: HookInput = serde_json::from_slice(&input).map_err(|error| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("invalid Kiro hook input: {error}"),
        )
    })?;
    boomux::integrations::validate_external_session_id(&input.session_id).map_err(|error| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("invalid Kiro session identity: {error}"),
        )
    })?;
    let working_contexts = crate::hook_input::structured_working_contexts(
        input.cwd.clone(),
        input.tool_name.as_deref(),
        input.tool_input.as_ref(),
    );
    Ok(HookUpdate {
        session_id: input.session_id,
        observation: reduce(input.hook_event_name),
        working_contexts,
    })
}

// V2 has no validated turn-idle boundary. Keep its legacy event vocabulary
// separate so a v3 Stop payload cannot silently acquire v2 lifecycle meaning.
pub(crate) fn read_v2_update(reader: impl Read) -> Result<HookUpdate, Box<dyn std::error::Error>> {
    let bytes = read_bounded_hook_input(reader, "Kiro v2")?;
    let value: serde_json::Value = serde_json::from_slice(&bytes)?;
    if !matches!(
        value
            .get("hook_event_name")
            .and_then(serde_json::Value::as_str),
        Some("agentSpawn" | "userPromptSubmit" | "preToolUse" | "postToolUse")
    ) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "unsupported Kiro v2 lifecycle event",
        )
        .into());
    }
    read_update(bytes.as_slice())
}

fn reduce(event: HookEvent) -> LifecycleObservation {
    let (state, evidence) = match event {
        HookEvent::SessionStart => (AgentState::Unknown, "Kiro hook execution started"),
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
    fn lifecycle_events_reduce_to_documented_states() {
        let cases = [
            ("SessionStart", AgentState::Unknown),
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
            ("agentSpawn", AgentState::Unknown),
            ("userPromptSubmit", AgentState::Working),
            ("preToolUse", AgentState::Working),
            ("postToolUse", AgentState::Working),
            ("stop", AgentState::Idle),
        ] {
            assert_eq!(update(event).observation.state, state, "{event}");
        }
    }

    #[test]
    fn v3_structured_cwd_and_tool_paths_are_projected() {
        let update = read_update(
            br#"{"session_id":"session-1","hook_event_name":"PreToolUse","cwd":"/worktrees/boomux","tool_name":"Read","tool_input":{"file_path":"/worktrees/omarchy/Panel.qml"}}"#
                .as_slice(),
        )
        .unwrap();
        assert_eq!(
            update.working_contexts,
            [
                PathBuf::from("/worktrees/boomux"),
                PathBuf::from("/worktrees/omarchy/Panel.qml"),
            ]
        );
    }

    #[test]
    fn v2_accepts_only_observed_activity_boundaries() {
        for (event, state) in [
            ("agentSpawn", AgentState::Unknown),
            ("userPromptSubmit", AgentState::Working),
            ("preToolUse", AgentState::Working),
            ("postToolUse", AgentState::Working),
        ] {
            let payload = format!(r#"{{"session_id":"v2-session","hook_event_name":"{event}"}}"#);
            assert_eq!(
                read_v2_update(payload.as_bytes())
                    .unwrap()
                    .observation
                    .state,
                state
            );
        }
        for event in ["stop", "agentStop", "Stop", "SessionStart", "Future"] {
            let payload = format!(r#"{{"session_id":"v2-session","hook_event_name":"{event}"}}"#);
            assert!(read_v2_update(payload.as_bytes()).is_err());
        }
        assert!(read_v2_update(br#"{"hook_event_name":"agentSpawn"}"#.as_slice()).is_err());
        assert!(read_v2_update(vec![b'x'; MAX_HOOK_INPUT_BYTES + 1].as_slice()).is_err());
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

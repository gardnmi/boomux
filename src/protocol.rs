use std::io::{self, Read, Write};
use std::path::PathBuf;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

pub const PROTOCOL_VERSION: u32 = 18;
pub const MIN_PROTOCOL_VERSION: u32 = 6;
pub const MAX_CONTROL_FRAME: usize = 8 * 1024 * 1024;
pub const MAX_ATTACH_FRAME: usize = 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Envelope<T> {
    pub version: u32,
    pub message: T,
}

impl<T> Envelope<T> {
    pub fn new(message: T) -> Self {
        Self {
            version: PROTOCOL_VERSION,
            message,
        }
    }

    pub fn with_version(version: u32, message: T) -> Self {
        Self { version, message }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Snapshot {
    pub workspaces: Vec<WorkspaceSnapshot>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub focused_terminal: Option<FocusedTerminalSnapshot>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FocusedTerminalSnapshot {
    pub revision: u64,
    pub workspace_id: String,
    pub shell_id: String,
    pub run_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceSnapshot {
    pub id: String,
    pub name: String,
    pub shells: Vec<ShellSnapshot>,
    #[serde(default)]
    pub launchers: Vec<WorkspaceLauncherSnapshot>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub agents: Vec<AgentInstanceSnapshot>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentState {
    Unknown,
    Working,
    Blocked,
    Idle,
    Inactive,
    Done,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentAuthority {
    LifecycleIntegration,
    ProcessAdapter,
    TerminalHeuristic,
    DaemonLifecycle,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentReport {
    pub state: AgentState,
    pub authority: AgentAuthority,
    pub evidence: String,
    pub confidence: u8,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentRegistrationSpec {
    pub name: String,
    pub integration: String,
    pub external_session_id: Option<String>,
    pub report: AgentReport,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentObservationSnapshot {
    pub revision: u64,
    pub state: AgentState,
    pub authority: AgentAuthority,
    pub evidence: String,
    pub confidence: u8,
    pub observed_at_ms: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentAttentionReason {
    Blocked,
    Completed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentAttentionSnapshot {
    pub reason: AgentAttentionReason,
    pub observation: AgentObservationSnapshot,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentInstanceSnapshot {
    pub id: String,
    pub workspace_id: String,
    pub shell_id: String,
    pub run_id: String,
    pub name: String,
    pub integration: String,
    pub external_session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<PathBuf>,
    pub started_at_ms: u64,
    pub ended_at_ms: Option<u64>,
    pub observation: AgentObservationSnapshot,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attention: Option<AgentAttentionSnapshot>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceLauncherSpec {
    pub name: String,
    pub command: Vec<String>,
    pub cwd: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceLauncherSnapshot {
    pub id: String,
    pub workspace_id: String,
    pub name: String,
    pub command: Vec<String>,
    pub cwd: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShellSnapshot {
    pub id: String,
    pub workspace_id: String,
    pub name: String,
    pub cwd: PathBuf,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub command: Vec<String>,
    pub status: ShellStatus,
    #[serde(default)]
    pub run: Option<ShellRunSnapshot>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub foreground_process: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ShellStatus {
    Pending,
    Running,
    Exited { code: Option<u32> },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShellRunSnapshot {
    pub id: String,
    pub generation: u64,
    pub started_at_ms: u64,
    pub ended_at_ms: Option<u64>,
    pub exit_reason: Option<ShellRunExitReason>,
    pub output_revision: u64,
    pub environment_has_run_id: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "reason", rename_all = "snake_case")]
pub enum ShellRunExitReason {
    Exited { code: Option<u32> },
    Terminated,
    Interrupted,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    InvalidArgument,
    NotFound,
    AlreadyExists,
    Busy,
    DaemonStopping,
    ShellStartFailed,
    PersistenceFailed,
    Timeout,
    UnsupportedVersion,
    CursorExpired,
    RunChanged,
    RevisionAhead,
    Internal,
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EventCursor {
    pub stream_id: String,
    pub event_id: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DaemonEvent {
    pub id: u64,
    pub at_ms: u64,
    #[serde(flatten)]
    pub kind: DaemonEventKind,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum DaemonEventKind {
    WorkspaceCreated {
        workspace_id: String,
        name: String,
    },
    WorkspaceRenamed {
        workspace_id: String,
        name: String,
    },
    WorkspaceClosed {
        workspace_id: String,
    },
    ShellCreated {
        workspace_id: String,
        shell_id: String,
        name: String,
    },
    ShellRenamed {
        workspace_id: String,
        shell_id: String,
        name: String,
    },
    ShellClosed {
        workspace_id: Option<String>,
        shell_id: String,
    },
    LauncherCreated {
        workspace_id: String,
        launcher_id: String,
        name: String,
    },
    LauncherRenamed {
        workspace_id: String,
        launcher_id: String,
        name: String,
    },
    LauncherRemoved {
        workspace_id: String,
        launcher_id: String,
    },
    RunStarted {
        workspace_id: String,
        shell_id: String,
        run: ShellRunSnapshot,
    },
    OutputChanged {
        workspace_id: String,
        shell_id: String,
        run_id: String,
        output_revision: u64,
    },
    RunExited {
        workspace_id: String,
        shell_id: String,
        run: ShellRunSnapshot,
    },
    AgentRegistered {
        workspace_id: String,
        shell_id: String,
        agent: AgentInstanceSnapshot,
    },
    AgentStateChanged {
        workspace_id: String,
        shell_id: String,
        agent: AgentInstanceSnapshot,
    },
    AgentCompleted {
        workspace_id: String,
        shell_id: String,
        agent: AgentInstanceSnapshot,
    },
    AgentAttentionAcknowledged {
        workspace_id: String,
        shell_id: String,
        agent: AgentInstanceSnapshot,
    },
    HandoffCompleted,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TerminalProfile {
    pub term: Option<String>,
    pub colorterm: Option<String>,
    pub term_program: Option<String>,
    pub term_program_version: Option<String>,
    pub rows: u16,
    pub cols: u16,
    pub pixel_width: u16,
    pub pixel_height: u16,
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UnixEnvironment {
    pub variables: Vec<UnixEnvironmentVariable>,
}

impl std::fmt::Debug for UnixEnvironment {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("UnixEnvironment")
            .field(
                "variables",
                &format_args!("<redacted: {}>", self.variables.len()),
            )
            .finish()
    }
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UnixEnvironmentVariable {
    pub name: Vec<u8>,
    pub value: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NotificationDeliveryConfig {
    pub desktop_enabled: bool,
    pub sound_enabled: bool,
    pub blocked: bool,
    pub completed: bool,
    pub blocked_sound: String,
    pub completed_sound: String,
}

impl std::fmt::Debug for UnixEnvironmentVariable {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("UnixEnvironmentVariable(<redacted>)")
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShellSpec {
    pub name: String,
    #[serde(default)]
    pub command: Vec<String>,
    pub cwd: PathBuf,
}

impl ShellSpec {
    pub fn login(name: impl Into<String>, cwd: impl Into<PathBuf>) -> Self {
        Self {
            name: name.into(),
            command: Vec::new(),
            cwd: cwd.into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "request", rename_all = "snake_case")]
pub enum Request {
    Ping,
    Restart,
    RestartWithNotificationConfig {
        notifications: NotificationDeliveryConfig,
    },
    Shutdown,
    Snapshot,
    GetWorkspace {
        workspace_id: String,
    },
    GetShell {
        shell_id: String,
    },
    GetLauncher {
        launcher_id: String,
    },
    GetAgent {
        agent_id: String,
    },
    WaitAgent {
        agent_id: String,
        after_revision: u64,
        #[serde(default)]
        wait_ms: u32,
    },
    CreateWorkspace {
        name: String,
        shells: Vec<ShellSpec>,
    },
    CreateShell {
        #[serde(default)]
        workspace_id: Option<String>,
        shell: ShellSpec,
    },
    CreateLauncher {
        workspace_id: String,
        spec: WorkspaceLauncherSpec,
    },
    RegisterAgent {
        shell_id: String,
        run_id: String,
        spec: AgentRegistrationSpec,
    },
    EnsureAgent {
        shell_id: String,
        run_id: String,
        spec: AgentRegistrationSpec,
    },
    ReportAgent {
        agent_id: String,
        run_id: String,
        report: AgentReport,
    },
    AcknowledgeAgentAttention {
        agent_id: String,
        observation_revision: u64,
    },
    ReadShell {
        shell_id: String,
        max_bytes: usize,
    },
    ReadShellAt {
        shell_id: String,
        max_bytes: usize,
        #[serde(default)]
        run_id: Option<String>,
        #[serde(default)]
        after_revision: Option<u64>,
        #[serde(default)]
        wait_ms: u32,
    },
    Events {
        #[serde(default)]
        after: Option<EventCursor>,
        #[serde(default = "default_event_limit")]
        limit: u16,
        #[serde(default)]
        wait_ms: u32,
    },
    RenameWorkspace {
        workspace_id: String,
        name: String,
    },
    RenameShell {
        shell_id: String,
        name: String,
    },
    RenameLauncher {
        launcher_id: String,
        name: String,
    },
    CloseWorkspace {
        workspace_id: String,
    },
    CloseShell {
        shell_id: String,
    },
    RestartShell {
        shell_id: String,
    },
    RemoveLauncher {
        launcher_id: String,
    },
    Attach {
        shell_id: String,
        takeover: bool,
        #[serde(default)]
        restart_exited: bool,
        profile: TerminalProfile,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        environment: Option<UnixEnvironment>,
    },
}

impl Request {
    pub fn minimum_protocol_version(&self) -> u32 {
        match self {
            Self::RestartWithNotificationConfig { .. } => 17,
            Self::Attach {
                environment: Some(_),
                ..
            } => 16,
            Self::AcknowledgeAgentAttention { .. } => 15,
            Self::WaitAgent { .. } => 14,
            Self::RegisterAgent { spec, .. } | Self::EnsureAgent { spec, .. }
                if spec.report.state == AgentState::Inactive =>
            {
                12
            }
            Self::ReportAgent { report, .. } if report.state == AgentState::Inactive => 12,
            Self::RestartShell { .. }
            | Self::Attach {
                restart_exited: true,
                ..
            } => 11,
            Self::EnsureAgent { .. } => 10,
            Self::GetAgent { .. } | Self::RegisterAgent { .. } | Self::ReportAgent { .. } => 9,
            Self::GetLauncher { .. }
            | Self::CreateLauncher { .. }
            | Self::RenameLauncher { .. }
            | Self::RemoveLauncher { .. } => 8,
            Self::ReadShellAt { .. } | Self::Events { .. } => 7,
            Self::Ping
            | Self::Restart
            | Self::Shutdown
            | Self::Snapshot
            | Self::GetWorkspace { .. }
            | Self::GetShell { .. }
            | Self::CreateWorkspace { .. }
            | Self::CreateShell { .. }
            | Self::ReadShell { .. }
            | Self::RenameWorkspace { .. }
            | Self::RenameShell { .. }
            | Self::CloseWorkspace { .. }
            | Self::CloseShell { .. }
            | Self::Attach { .. } => MIN_PROTOCOL_VERSION,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "response", rename_all = "snake_case")]
pub enum Response {
    Pong,
    Snapshot {
        snapshot: Snapshot,
    },
    Workspace {
        workspace: WorkspaceSnapshot,
    },
    Shell {
        shell: ShellSnapshot,
    },
    Launcher {
        launcher: WorkspaceLauncherSnapshot,
    },
    Agent {
        agent: AgentInstanceSnapshot,
    },
    AgentWait {
        agent: AgentInstanceSnapshot,
        changed: bool,
    },
    AgentAttentionAcknowledged {
        agent: AgentInstanceSnapshot,
        changed: bool,
    },
    Output {
        bytes: Vec<u8>,
    },
    OutputState {
        bytes: Vec<u8>,
        run_id: Option<String>,
        output_revision: Option<u64>,
        changed: bool,
        status: ShellStatus,
    },
    Events {
        stream_id: String,
        cursor: EventCursor,
        snapshot: Option<Snapshot>,
        events: Vec<DaemonEvent>,
    },
    Attached {
        token: String,
        reconstruction: Vec<u8>,
        warning: Option<String>,
    },
    Ok,
    Error {
        message: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        code: Option<ErrorCode>,
    },
}

fn default_event_limit() -> u16 {
    256
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AttachFrame {
    Input(Vec<u8>),
    Output(Vec<u8>),
    Resize {
        rows: u16,
        cols: u16,
        pixel_width: u16,
        pixel_height: u16,
    },
    Detached,
    Reconnect,
    ReconnectAck,
    FocusGained,
}

impl AttachFrame {
    const INPUT: u8 = 1;
    const OUTPUT: u8 = 2;
    const RESIZE: u8 = 3;
    const DETACHED: u8 = 4;
    const RECONNECT: u8 = 5;
    const RECONNECT_ACK: u8 = 6;
    const FOCUS_GAINED: u8 = 7;

    pub fn write_to(&self, writer: &mut impl Write) -> io::Result<()> {
        let (kind, payload): (u8, &[u8]) = match self {
            Self::Input(bytes) => (Self::INPUT, bytes),
            Self::Output(bytes) => (Self::OUTPUT, bytes),
            Self::Resize {
                rows,
                cols,
                pixel_width,
                pixel_height,
            } => {
                writer.write_all(&[Self::RESIZE])?;
                writer.write_all(&8_u32.to_be_bytes())?;
                writer.write_all(&rows.to_be_bytes())?;
                writer.write_all(&cols.to_be_bytes())?;
                writer.write_all(&pixel_width.to_be_bytes())?;
                return writer.write_all(&pixel_height.to_be_bytes());
            }
            Self::Detached => (Self::DETACHED, &[]),
            Self::Reconnect => (Self::RECONNECT, &[]),
            Self::ReconnectAck => (Self::RECONNECT_ACK, &[]),
            Self::FocusGained => (Self::FOCUS_GAINED, &[]),
        };
        if payload.len() > MAX_ATTACH_FRAME {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "attach frame too large",
            ));
        }
        writer.write_all(&[kind])?;
        writer.write_all(&(payload.len() as u32).to_be_bytes())?;
        writer.write_all(payload)
    }

    pub fn read_from(reader: &mut impl Read) -> io::Result<Self> {
        let mut kind = [0];
        reader.read_exact(&mut kind)?;
        let mut length = [0; 4];
        reader.read_exact(&mut length)?;
        let length = u32::from_be_bytes(length) as usize;
        if length > MAX_ATTACH_FRAME {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "attach frame too large",
            ));
        }
        let mut payload = vec![0; length];
        reader.read_exact(&mut payload)?;
        match kind[0] {
            Self::INPUT => Ok(Self::Input(payload)),
            Self::OUTPUT => Ok(Self::Output(payload)),
            Self::RESIZE if payload.len() == 8 => Ok(Self::Resize {
                rows: u16::from_be_bytes([payload[0], payload[1]]),
                cols: u16::from_be_bytes([payload[2], payload[3]]),
                pixel_width: u16::from_be_bytes([payload[4], payload[5]]),
                pixel_height: u16::from_be_bytes([payload[6], payload[7]]),
            }),
            Self::DETACHED if payload.is_empty() => Ok(Self::Detached),
            Self::RECONNECT if payload.is_empty() => Ok(Self::Reconnect),
            Self::RECONNECT_ACK if payload.is_empty() => Ok(Self::ReconnectAck),
            Self::FOCUS_GAINED if payload.is_empty() => Ok(Self::FocusGained),
            _ => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid attach frame",
            )),
        }
    }
}

pub fn write_message<T: Serialize>(writer: &mut impl Write, message: &T) -> io::Result<()> {
    let bytes = serde_json::to_vec(message).map_err(io::Error::other)?;
    if bytes.len() > MAX_CONTROL_FRAME {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "control frame too large",
        ));
    }
    writer.write_all(&(bytes.len() as u32).to_be_bytes())?;
    writer.write_all(&bytes)
}

pub fn read_message<T: DeserializeOwned>(reader: &mut impl Read) -> io::Result<T> {
    let mut length = [0; 4];
    reader.read_exact(&mut length)?;
    let length = u32::from_be_bytes(length) as usize;
    if length > MAX_CONTROL_FRAME {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "control frame too large",
        ));
    }
    let mut bytes = vec![0; length];
    reader.read_exact(&mut bytes)?;
    serde_json::from_slice(&bytes).map_err(io::Error::other)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_profile() -> TerminalProfile {
        TerminalProfile {
            term: None,
            colorterm: None,
            term_program: None,
            term_program_version: None,
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        }
    }

    fn test_report(state: AgentState) -> AgentReport {
        AgentReport {
            state,
            authority: AgentAuthority::LifecycleIntegration,
            evidence: "test".into(),
            confidence: 100,
        }
    }

    fn test_registration(state: AgentState) -> AgentRegistrationSpec {
        AgentRegistrationSpec {
            name: "agent".into(),
            integration: "test".into(),
            external_session_id: None,
            report: test_report(state),
        }
    }

    #[derive(Deserialize)]
    struct ProtocolSixShellSnapshot {
        id: String,
        status: ShellStatus,
    }

    #[derive(Deserialize)]
    #[serde(tag = "response", rename_all = "snake_case")]
    enum LegacyResponse {
        Error { message: String },
    }

    #[test]
    fn control_frame_round_trips() {
        let value = Envelope::new(Request::Attach {
            shell_id: "s1".into(),
            takeover: false,
            restart_exited: true,
            profile: TerminalProfile {
                term: Some("xterm-256color".into()),
                colorterm: Some("truecolor".into()),
                term_program: Some("test".into()),
                term_program_version: Some("1".into()),
                rows: 24,
                cols: 80,
                pixel_width: 800,
                pixel_height: 600,
            },
            environment: Some(UnixEnvironment {
                variables: vec![UnixEnvironmentVariable {
                    name: b"NON_UTF8".to_vec(),
                    value: vec![0xff, 0xfe],
                }],
            }),
        });
        let mut bytes = Vec::new();
        write_message(&mut bytes, &value).unwrap();
        assert_eq!(
            read_message::<Envelope<Request>>(&mut bytes.as_slice()).unwrap(),
            value
        );
    }

    #[test]
    fn typed_errors_are_additive_within_protocol_six() {
        let legacy: Response =
            serde_json::from_str(r#"{"response":"error","message":"legacy daemon"}"#).unwrap();
        assert_eq!(
            legacy,
            Response::Error {
                message: "legacy daemon".into(),
                code: None,
            }
        );

        let coded = Response::Error {
            message: "missing".into(),
            code: Some(ErrorCode::NotFound),
        };
        let LegacyResponse::Error { message } =
            serde_json::from_value(serde_json::to_value(coded).unwrap()).unwrap();
        assert_eq!(message, "missing");

        let unknown: Response =
            serde_json::from_str(r#"{"response":"error","message":"future","code":"future_code"}"#)
                .unwrap();
        assert!(matches!(
            unknown,
            Response::Error {
                code: Some(ErrorCode::Unknown),
                ..
            }
        ));
    }

    #[test]
    fn shell_spec_requires_cwd_on_the_wire() {
        let request = r#"{"request":"create_shell","workspace_id":"w1","shell":{"name":"shell","command":[]}}"#;

        assert!(serde_json::from_str::<Request>(request).is_err());
    }

    #[test]
    fn shell_snapshot_defaults_fields_omitted_by_old_daemons() {
        let snapshot = serde_json::from_str::<ShellSnapshot>(
            r#"{"id":"s1","workspace_id":"w1","name":"shell","cwd":"/tmp","status":"running"}"#,
        )
        .unwrap();

        assert!(snapshot.command.is_empty());
        assert!(snapshot.run.is_none());
        assert!(snapshot.foreground_process.is_none());
        assert!(
            serde_json::to_value(snapshot)
                .unwrap()
                .get("foreground_process")
                .is_none()
        );
    }

    #[test]
    fn workspace_snapshot_accepts_protocol_seven_payload_without_launchers() {
        let snapshot = serde_json::from_str::<WorkspaceSnapshot>(
            r#"{"id":"w1","name":"workspace","shells":[]}"#,
        )
        .unwrap();

        assert!(snapshot.launchers.is_empty());
        assert!(snapshot.agents.is_empty());
    }

    #[test]
    fn agent_messages_round_trip_with_snake_case_names() {
        let report = AgentReport {
            state: AgentState::Working,
            authority: AgentAuthority::LifecycleIntegration,
            evidence: "tool call in progress".into(),
            confidence: 95,
        };
        let request = Request::RegisterAgent {
            shell_id: "s1".into(),
            run_id: "r1".into(),
            spec: AgentRegistrationSpec {
                name: "opencode".into(),
                integration: "opencode-plugin".into(),
                external_session_id: Some("external-1".into()),
                report: report.clone(),
            },
        };

        let encoded = serde_json::to_value(&request).unwrap();
        assert_eq!(encoded["request"], "register_agent");
        assert_eq!(encoded["spec"]["report"]["state"], "working");
        assert_eq!(
            encoded["spec"]["report"]["authority"],
            "lifecycle_integration"
        );
        assert_eq!(serde_json::from_value::<Request>(encoded).unwrap(), request);

        let agent = AgentInstanceSnapshot {
            id: "a1".into(),
            workspace_id: "w1".into(),
            shell_id: "s1".into(),
            run_id: "r1".into(),
            name: "opencode".into(),
            integration: "opencode-plugin".into(),
            external_session_id: Some("external-1".into()),
            cwd: Some("/tmp/project".into()),
            started_at_ms: 10,
            ended_at_ms: None,
            observation: AgentObservationSnapshot {
                revision: 1,
                state: report.state,
                authority: report.authority,
                evidence: report.evidence,
                confidence: report.confidence,
                observed_at_ms: 11,
            },
            attention: None,
        };
        let event = DaemonEventKind::AgentStateChanged {
            workspace_id: "w1".into(),
            shell_id: "s1".into(),
            agent,
        };
        let encoded = serde_json::to_value(&event).unwrap();
        assert_eq!(encoded["event"], "agent_state_changed");
        assert_eq!(
            serde_json::from_value::<DaemonEventKind>(encoded).unwrap(),
            event
        );
    }

    #[test]
    fn protocol_version_is_eighteen_with_minimum_six() {
        assert_eq!(PROTOCOL_VERSION, 18);
        assert_eq!(MIN_PROTOCOL_VERSION, 6);
    }

    #[test]
    fn old_snapshot_defaults_focused_terminal() {
        let snapshot: Snapshot = serde_json::from_str(r#"{"workspaces":[]}"#).unwrap();

        assert_eq!(snapshot.focused_terminal, None);
    }

    #[test]
    fn request_minimum_protocol_versions_cover_all_groups() {
        let groups = vec![
            (
                17,
                vec![Request::RestartWithNotificationConfig {
                    notifications: NotificationDeliveryConfig {
                        desktop_enabled: true,
                        sound_enabled: true,
                        blocked: true,
                        completed: true,
                        blocked_sound: "dialog-warning".into(),
                        completed_sound: "complete".into(),
                    },
                }],
            ),
            (
                6,
                vec![
                    Request::Ping,
                    Request::Restart,
                    Request::Shutdown,
                    Request::Snapshot,
                    Request::GetWorkspace {
                        workspace_id: "w1".into(),
                    },
                    Request::GetShell {
                        shell_id: "s1".into(),
                    },
                    Request::CreateWorkspace {
                        name: "workspace".into(),
                        shells: Vec::new(),
                    },
                    Request::CreateShell {
                        workspace_id: Some("w1".into()),
                        shell: ShellSpec::login("shell", "/tmp"),
                    },
                    Request::ReadShell {
                        shell_id: "s1".into(),
                        max_bytes: 1024,
                    },
                    Request::RenameWorkspace {
                        workspace_id: "w1".into(),
                        name: "renamed".into(),
                    },
                    Request::RenameShell {
                        shell_id: "s1".into(),
                        name: "renamed".into(),
                    },
                    Request::CloseWorkspace {
                        workspace_id: "w1".into(),
                    },
                    Request::CloseShell {
                        shell_id: "s1".into(),
                    },
                    Request::Attach {
                        shell_id: "s1".into(),
                        takeover: false,
                        restart_exited: false,
                        profile: test_profile(),
                        environment: None,
                    },
                ],
            ),
            (
                7,
                vec![
                    Request::ReadShellAt {
                        shell_id: "s1".into(),
                        max_bytes: 1024,
                        run_id: None,
                        after_revision: None,
                        wait_ms: 0,
                    },
                    Request::Events {
                        after: None,
                        limit: 1,
                        wait_ms: 0,
                    },
                ],
            ),
            (
                8,
                vec![
                    Request::GetLauncher {
                        launcher_id: "l1".into(),
                    },
                    Request::CreateLauncher {
                        workspace_id: "w1".into(),
                        spec: WorkspaceLauncherSpec {
                            name: "editor".into(),
                            command: vec!["editor".into()],
                            cwd: "/tmp".into(),
                        },
                    },
                    Request::RenameLauncher {
                        launcher_id: "l1".into(),
                        name: "renamed".into(),
                    },
                    Request::RemoveLauncher {
                        launcher_id: "l1".into(),
                    },
                ],
            ),
            (
                9,
                vec![
                    Request::GetAgent {
                        agent_id: "a1".into(),
                    },
                    Request::RegisterAgent {
                        shell_id: "s1".into(),
                        run_id: "r1".into(),
                        spec: test_registration(AgentState::Working),
                    },
                    Request::ReportAgent {
                        agent_id: "a1".into(),
                        run_id: "r1".into(),
                        report: test_report(AgentState::Idle),
                    },
                ],
            ),
            (
                10,
                vec![Request::EnsureAgent {
                    shell_id: "s1".into(),
                    run_id: "r1".into(),
                    spec: test_registration(AgentState::Working),
                }],
            ),
            (
                11,
                vec![
                    Request::RestartShell {
                        shell_id: "s1".into(),
                    },
                    Request::Attach {
                        shell_id: "s1".into(),
                        takeover: false,
                        restart_exited: true,
                        profile: test_profile(),
                        environment: None,
                    },
                ],
            ),
            (
                12,
                vec![
                    Request::RegisterAgent {
                        shell_id: "s1".into(),
                        run_id: "r1".into(),
                        spec: test_registration(AgentState::Inactive),
                    },
                    Request::EnsureAgent {
                        shell_id: "s1".into(),
                        run_id: "r1".into(),
                        spec: test_registration(AgentState::Inactive),
                    },
                    Request::ReportAgent {
                        agent_id: "a1".into(),
                        run_id: "r1".into(),
                        report: test_report(AgentState::Inactive),
                    },
                ],
            ),
            (
                14,
                vec![Request::WaitAgent {
                    agent_id: "a1".into(),
                    after_revision: 1,
                    wait_ms: 0,
                }],
            ),
            (
                15,
                vec![Request::AcknowledgeAgentAttention {
                    agent_id: "a1".into(),
                    observation_revision: 1,
                }],
            ),
            (
                16,
                vec![Request::Attach {
                    shell_id: "s1".into(),
                    takeover: false,
                    restart_exited: true,
                    profile: test_profile(),
                    environment: Some(UnixEnvironment {
                        variables: Vec::new(),
                    }),
                }],
            ),
        ];

        for (expected, requests) in groups {
            for request in requests {
                assert_eq!(
                    request.minimum_protocol_version(),
                    expected,
                    "unexpected minimum protocol version for {request:?}"
                );
            }
        }
    }

    #[test]
    fn attach_environment_is_optional_and_debug_is_redacted() {
        let legacy = r#"{"request":"attach","shell_id":"s1","takeover":false,"profile":{"term":null,"colorterm":null,"term_program":null,"term_program_version":null,"rows":24,"cols":80,"pixel_width":0,"pixel_height":0}}"#;
        let request: Request = serde_json::from_str(legacy).unwrap();
        assert!(matches!(
            request,
            Request::Attach {
                environment: None,
                ..
            }
        ));

        let environment = UnixEnvironment {
            variables: vec![UnixEnvironmentVariable {
                name: b"SECRET_NAME".to_vec(),
                value: b"secret-value".to_vec(),
            }],
        };
        let debug = format!("{environment:?}");
        assert!(!debug.contains("SECRET_NAME"));
        assert!(!debug.contains("secret-value"));
        assert!(debug.contains("redacted"));
    }

    #[test]
    fn agent_wait_messages_round_trip() {
        let request = Request::WaitAgent {
            agent_id: "a1".into(),
            after_revision: 3,
            wait_ms: 30_000,
        };
        let encoded = serde_json::to_value(&request).unwrap();
        assert_eq!(encoded["request"], "wait_agent");
        assert_eq!(serde_json::from_value::<Request>(encoded).unwrap(), request);

        let response = Response::AgentWait {
            agent: AgentInstanceSnapshot {
                id: "a1".into(),
                workspace_id: "w1".into(),
                shell_id: "s1".into(),
                run_id: "r1".into(),
                name: "agent".into(),
                integration: "test".into(),
                external_session_id: Some("external".into()),
                cwd: Some("/tmp/project".into()),
                started_at_ms: 1,
                ended_at_ms: None,
                observation: AgentObservationSnapshot {
                    revision: 4,
                    state: AgentState::Idle,
                    authority: AgentAuthority::LifecycleIntegration,
                    evidence: "idle".into(),
                    confidence: 100,
                    observed_at_ms: 2,
                },
                attention: None,
            },
            changed: true,
        };
        let encoded = serde_json::to_value(&response).unwrap();
        assert_eq!(encoded["response"], "agent_wait");
        assert_eq!(
            serde_json::from_value::<Response>(encoded).unwrap(),
            response
        );
    }

    #[test]
    fn agent_snapshot_defaults_cwd_omitted_by_old_daemons() {
        let agent: AgentInstanceSnapshot = serde_json::from_value(serde_json::json!({
            "id": "a1",
            "workspace_id": "w1",
            "shell_id": "s1",
            "run_id": "r1",
            "name": "agent",
            "integration": "test",
            "external_session_id": "external",
            "started_at_ms": 1,
            "ended_at_ms": null,
            "observation": {
                "revision": 1,
                "state": "working",
                "authority": "lifecycle_integration",
                "evidence": "working",
                "confidence": 100,
                "observed_at_ms": 1
            }
        }))
        .unwrap();

        assert!(agent.cwd.is_none());
        assert!(agent.attention.is_none());
        assert!(serde_json::to_value(agent).unwrap().get("cwd").is_none());
    }

    #[test]
    fn agent_attention_messages_use_snake_case_and_omit_absent_attention() {
        let observation = AgentObservationSnapshot {
            revision: 3,
            state: AgentState::Blocked,
            authority: AgentAuthority::LifecycleIntegration,
            evidence: "needs input".into(),
            confidence: 100,
            observed_at_ms: 4,
        };
        let attention = AgentAttentionSnapshot {
            reason: AgentAttentionReason::Blocked,
            observation: observation.clone(),
        };
        let encoded = serde_json::to_value(&attention).unwrap();
        assert_eq!(encoded["reason"], "blocked");
        assert_eq!(
            serde_json::to_value(AgentAttentionReason::Completed).unwrap(),
            "completed"
        );

        let request = Request::AcknowledgeAgentAttention {
            agent_id: "a1".into(),
            observation_revision: 3,
        };
        assert_eq!(
            serde_json::to_value(&request).unwrap()["request"],
            "acknowledge_agent_attention"
        );
        assert_eq!(
            serde_json::from_value::<Request>(serde_json::to_value(request.clone()).unwrap())
                .unwrap(),
            request
        );

        let mut agent: AgentInstanceSnapshot = serde_json::from_value(serde_json::json!({
            "id": "a1", "workspace_id": "w1", "shell_id": "s1", "run_id": "r1",
            "name": "agent", "integration": "test", "external_session_id": null,
            "started_at_ms": 1, "ended_at_ms": null, "observation": observation
        }))
        .unwrap();
        assert!(agent.attention.is_none());
        assert!(
            serde_json::to_value(&agent)
                .unwrap()
                .get("attention")
                .is_none()
        );
        agent.attention = Some(attention);
        assert_eq!(
            serde_json::to_value(&agent).unwrap()["attention"]["reason"],
            "blocked"
        );
        let response = Response::AgentAttentionAcknowledged {
            agent: agent.clone(),
            changed: true,
        };
        assert_eq!(
            serde_json::to_value(response).unwrap()["response"],
            "agent_attention_acknowledged"
        );
        let event = DaemonEventKind::AgentAttentionAcknowledged {
            workspace_id: "w1".into(),
            shell_id: "s1".into(),
            agent,
        };
        assert_eq!(
            serde_json::to_value(event).unwrap()["event"],
            "agent_attention_acknowledged"
        );
    }

    #[test]
    fn ensure_agent_uses_registration_shape() {
        let request = Request::EnsureAgent {
            shell_id: "s1".into(),
            run_id: "r1".into(),
            spec: AgentRegistrationSpec {
                name: "agent".into(),
                integration: "plugin".into(),
                external_session_id: Some("session-1".into()),
                report: AgentReport {
                    state: AgentState::Working,
                    authority: AgentAuthority::LifecycleIntegration,
                    evidence: "working".into(),
                    confidence: 90,
                },
            },
        };

        let encoded = serde_json::to_value(&request).unwrap();
        assert_eq!(encoded["request"], "ensure_agent");
        assert_eq!(serde_json::from_value::<Request>(encoded).unwrap(), request);
    }

    #[test]
    fn launcher_request_round_trips_exact_argv() {
        let request = Request::CreateLauncher {
            workspace_id: "w1".into(),
            spec: WorkspaceLauncherSpec {
                name: "editor".into(),
                command: vec!["editor".into(), "".into(), "two words".into()],
                cwd: "/tmp/project".into(),
            },
        };

        let encoded = serde_json::to_vec(&request).unwrap();
        let decoded: Request = serde_json::from_slice(&encoded).unwrap();

        assert_eq!(decoded, request);
    }

    #[test]
    fn protocol_six_client_ignores_additive_shell_snapshot_fields() {
        let snapshot = ShellSnapshot {
            id: "s1".into(),
            workspace_id: "w1".into(),
            name: "shell".into(),
            cwd: "/tmp".into(),
            command: vec!["sleep".into(), "1".into()],
            status: ShellStatus::Running,
            run: Some(ShellRunSnapshot {
                id: "r1".into(),
                generation: 1,
                started_at_ms: 1,
                ended_at_ms: None,
                exit_reason: None,
                output_revision: 2,
                environment_has_run_id: true,
            }),
            foreground_process: Some("sleep".into()),
        };

        let legacy: ProtocolSixShellSnapshot =
            serde_json::from_value(serde_json::to_value(snapshot).unwrap()).unwrap();
        assert_eq!(legacy.id, "s1");
        assert_eq!(legacy.status, ShellStatus::Running);
    }

    #[test]
    fn attach_frames_round_trip() {
        let frames = [
            AttachFrame::Input(vec![0, 1, 255]),
            AttachFrame::Resize {
                rows: 24,
                cols: 80,
                pixel_width: 1920,
                pixel_height: 1080,
            },
            AttachFrame::Detached,
            AttachFrame::Reconnect,
            AttachFrame::ReconnectAck,
            AttachFrame::FocusGained,
        ];
        for frame in frames {
            let mut bytes = Vec::new();
            frame.write_to(&mut bytes).unwrap();
            assert_eq!(
                AttachFrame::read_from(&mut bytes.as_slice()).unwrap(),
                frame
            );
        }
        let mut bytes = Vec::new();
        AttachFrame::FocusGained.write_to(&mut bytes).unwrap();
        assert_eq!(bytes, [7, 0, 0, 0, 0]);
    }

    #[test]
    fn rejects_protocol_two_resize_frame() {
        let bytes = [AttachFrame::RESIZE, 0, 0, 0, 4, 0, 24, 0, 80];

        assert!(AttachFrame::read_from(&mut bytes.as_slice()).is_err());
    }
}

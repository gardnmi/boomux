use std::env;
use std::error::Error;
use std::fs::OpenOptions;
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::thread;
use std::time::Duration;

use crate::protocol::{
    self, AgentInstanceSnapshot, AgentRegistrationSpec, AgentReport, AgentScheduleInspection,
    AgentScheduleSnapshot, AgentScheduleSpec, DaemonEvent, Envelope, ErrorCode, EventCursor,
    FocusedTerminalSnapshot, NotificationDeliveryConfig, Request, Response,
    ScheduledExecutionSnapshot, ScheduledRunnerResult, ShellSnapshot, ShellSpec, ShellStatus,
    Snapshot, TerminalPreview, TerminalPreviewLine, TerminalPreviewSpan, TerminalProfile,
    UnixEnvironment, UnixEnvironmentVariable, WorkspaceLauncherSnapshot, WorkspaceLauncherSpec,
    WorkspaceSnapshot,
};

const CONNECT_ATTEMPTS: usize = 40;
const CONNECT_DELAY: Duration = Duration::from_millis(25);
const SHUTDOWN_ATTEMPTS: usize = 200;

pub type Result<T> = std::result::Result<T, ClientError>;

#[derive(Debug)]
pub enum ClientError {
    Transport(io::Error),
    Protocol(ProtocolError),
    Remote(RemoteError),
    Validation(io::Error),
    Lifecycle(LifecycleError),
}

impl std::fmt::Display for ClientError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Transport(error) | Self::Validation(error) => error.fmt(formatter),
            Self::Protocol(error) => error.fmt(formatter),
            Self::Remote(error) => error.fmt(formatter),
            Self::Lifecycle(error) => error.fmt(formatter),
        }
    }
}

impl Error for ClientError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Transport(error) | Self::Validation(error) => Some(error),
            Self::Protocol(error) => Some(error),
            Self::Remote(error) => Some(error),
            Self::Lifecycle(error) => Some(error),
        }
    }
}

impl From<io::Error> for ClientError {
    fn from(error: io::Error) -> Self {
        Self::Transport(error)
    }
}

#[derive(Debug)]
pub enum ProtocolError {
    UnsupportedVersion(String),
    VersionMismatch { expected: u32, actual: u32 },
    InvalidMessage(io::Error),
    EventBaselineMissing,
    UnexpectedResponse(Box<Response>),
}

impl std::fmt::Display for ProtocolError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnsupportedVersion(message) => formatter.write_str(message),
            Self::VersionMismatch { .. } => formatter.write_str("protocol version mismatch"),
            Self::InvalidMessage(error) => error.fmt(formatter),
            Self::EventBaselineMissing => {
                formatter.write_str("event baseline omitted its snapshot")
            }
            Self::UnexpectedResponse(response) => {
                write!(formatter, "unexpected daemon response: {response:?}")
            }
        }
    }
}

impl Error for ProtocolError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::InvalidMessage(error) => Some(error),
            _ => None,
        }
    }
}

#[derive(Debug)]
pub enum LifecycleError {
    DaemonStart(io::Error),
    DaemonStartTimeout(Option<Box<ClientError>>),
    ShutdownTimeout,
    ReplacementStartTimeout(Option<Box<ClientError>>),
    AttachmentReconnectTimeout(Option<Box<ClientError>>),
}

impl std::fmt::Display for LifecycleError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::DaemonStart(error) => write!(formatter, "daemon did not start: {error}"),
            Self::DaemonStartTimeout(Some(error)) | Self::ReplacementStartTimeout(Some(error)) => {
                error.fmt(formatter)
            }
            Self::DaemonStartTimeout(None) => formatter.write_str("daemon did not start"),
            Self::ShutdownTimeout => {
                formatter.write_str("Boomux daemon did not finish shutting down")
            }
            Self::ReplacementStartTimeout(None) => {
                formatter.write_str("replacement daemon did not start")
            }
            Self::AttachmentReconnectTimeout(Some(error)) => error.fmt(formatter),
            Self::AttachmentReconnectTimeout(None) => {
                formatter.write_str("daemon attachment did not reconnect")
            }
        }
    }
}

impl Error for LifecycleError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::DaemonStart(error) => Some(error),
            Self::DaemonStartTimeout(Some(error))
            | Self::ReplacementStartTimeout(Some(error))
            | Self::AttachmentReconnectTimeout(Some(error)) => Some(error.as_ref()),
            _ => None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Client {
    socket_path: PathBuf,
    protocol_version: Arc<AtomicU32>,
}

#[derive(Debug)]
pub struct Attachment {
    pub stream: UnixStream,
    pub protocol_version: u32,
    pub token: String,
    pub reconstruction: Vec<u8>,
    pub warning: Option<String>,
}

#[derive(Debug)]
pub struct OutputState {
    pub bytes: Vec<u8>,
    pub run_id: Option<String>,
    pub output_revision: Option<u64>,
    pub changed: bool,
    pub status: ShellStatus,
}

#[derive(Debug)]
pub struct EventBatch {
    pub stream_id: String,
    pub cursor: EventCursor,
    pub snapshot: Option<Snapshot>,
    pub events: Vec<DaemonEvent>,
}

#[derive(Debug)]
pub struct SnapshotWatch {
    cursor: Option<EventCursor>,
    snapshot: Snapshot,
}

impl SnapshotWatch {
    pub fn baseline(client: &Client) -> Result<Self> {
        if !client.supports(protocol::ProtocolFeature::AtomicOutputReads)? {
            return Ok(Self {
                cursor: None,
                snapshot: client.snapshot()?,
            });
        }
        let batch = client.events(None, 1, 0)?;
        let snapshot = batch
            .snapshot
            .ok_or(ClientError::Protocol(ProtocolError::EventBaselineMissing))?;
        Ok(Self {
            cursor: Some(batch.cursor),
            snapshot,
        })
    }

    pub fn snapshot(&self) -> &Snapshot {
        &self.snapshot
    }

    pub fn snapshot_mut(&mut self) -> &mut Snapshot {
        &mut self.snapshot
    }

    pub fn uses_events(&self) -> bool {
        self.cursor.is_some()
    }

    pub fn poll(&mut self, client: &Client) -> Result<(bool, bool)> {
        let Some(cursor) = self.cursor.clone() else {
            return Ok((false, false));
        };
        let stream_id = cursor.stream_id.clone();
        match client.events(Some(cursor), 256, 0) {
            Ok(batch) => {
                if batch.events.is_empty() {
                    self.cursor = Some(batch.cursor);
                    return Ok((false, false));
                }
                let stream_changed = batch.stream_id != stream_id;
                let snapshot = client.snapshot()?;
                self.cursor = Some(batch.cursor);
                self.snapshot = snapshot;
                Ok((true, stream_changed))
            }
            Err(ClientError::Remote(RemoteError {
                code: Some(ErrorCode::CursorExpired),
                ..
            })) => {
                *self = Self::baseline(client)?;
                Ok((true, self.stream_id() != Some(stream_id.as_str())))
            }
            Err(ClientError::Remote(RemoteError {
                code: Some(ErrorCode::UnsupportedVersion),
                ..
            }))
            | Err(ClientError::Protocol(ProtocolError::UnsupportedVersion(_))) => {
                *self = Self::baseline(client)?;
                Ok((true, self.stream_id() != Some(stream_id.as_str())))
            }
            Err(error) => Err(error),
        }
    }

    pub fn stream_id(&self) -> Option<&str> {
        self.cursor.as_ref().map(|cursor| cursor.stream_id.as_str())
    }
}

#[derive(Debug)]
pub struct AgentWait {
    pub agent: AgentInstanceSnapshot,
    pub changed: bool,
}

#[derive(Debug)]
pub struct AgentAttentionAcknowledgement {
    pub agent: AgentInstanceSnapshot,
    pub changed: bool,
}

#[derive(Debug)]
pub struct RemoteError {
    pub code: Option<ErrorCode>,
    pub message: String,
}

impl std::fmt::Display for RemoteError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl Error for RemoteError {}

pub fn socket_path() -> io::Result<PathBuf> {
    let runtime = env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "XDG_RUNTIME_DIR is not set"))?;
    if !runtime.is_absolute() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "XDG_RUNTIME_DIR must be absolute",
        ));
    }
    Ok(runtime.join("boomux").join("daemon.sock"))
}

pub fn connect_or_start() -> Result<Client> {
    let client = connect_client()?;
    match client.ping() {
        Ok(()) => return Ok(client),
        Err(error) if !daemon_unreachable(&error) => return Err(error),
        Err(_) => {}
    }

    let mut command = Command::new(env::current_exe().map_err(ClientError::Validation)?);
    command
        .args(["daemon", "run"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    // The child has not executed user code yet; `setsid` only detaches it
    // from the launching terminal before exec.
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                Err(io::Error::last_os_error())
            } else {
                Ok(())
            }
        });
    }
    command
        .spawn()
        .map_err(|error| ClientError::Lifecycle(LifecycleError::DaemonStart(error)))?;

    let mut last_error = None;
    for _ in 0..CONNECT_ATTEMPTS {
        match client.ping() {
            Ok(()) => return Ok(client),
            Err(error) if !daemon_unreachable(&error) => return Err(error),
            Err(error) => last_error = Some(error),
        }
        thread::sleep(CONNECT_DELAY);
    }
    Err(ClientError::Lifecycle(LifecycleError::DaemonStartTimeout(
        last_error.map(Box::new),
    )))
}

fn daemon_unreachable(error: &ClientError) -> bool {
    matches!(
        error,
        ClientError::Transport(error)
            if matches!(
                error.kind(),
            io::ErrorKind::NotFound
                | io::ErrorKind::ConnectionRefused
                | io::ErrorKind::ConnectionReset
                | io::ErrorKind::BrokenPipe
                | io::ErrorKind::UnexpectedEof
            )
    )
}

pub fn connect() -> Result<Client> {
    let client = connect_client()?;
    client.ping()?;
    Ok(client)
}

fn connect_client() -> Result<Client> {
    Ok(Client {
        socket_path: socket_path().map_err(ClientError::Validation)?,
        protocol_version: Arc::new(AtomicU32::new(protocol::PROTOCOL_VERSION)),
    })
}

impl Client {
    pub fn from_socket_path(socket_path: PathBuf) -> Self {
        Self {
            socket_path,
            protocol_version: Arc::new(AtomicU32::new(protocol::PROTOCOL_VERSION)),
        }
    }

    pub fn socket_path(&self) -> &Path {
        &self.socket_path
    }

    pub fn protocol_version(&self) -> Result<u32> {
        let _ = self.probe_latest()?;
        Ok(self.protocol_version.load(Ordering::Acquire))
    }

    pub fn supports(&self, feature: protocol::ProtocolFeature) -> Result<bool> {
        Ok(feature.is_supported_by(self.protocol_version()?))
    }

    pub fn request(&self, request: Request) -> Result<Response> {
        self.send(request).map(|(_, _, response)| response)
    }

    fn send(&self, request: Request) -> Result<(UnixStream, u32, Response)> {
        let mut version = self.protocol_version.load(Ordering::Acquire);
        if request
            .required_feature()
            .is_some_and(|feature| !feature.is_supported_by(version))
        {
            self.probe_latest()?;
            version = self.protocol_version.load(Ordering::Acquire);
            if request
                .required_feature()
                .is_some_and(|feature| !feature.is_supported_by(version))
            {
                return Err(unsupported_version("daemon does not support this request"));
            }
        }
        match self.send_with_version(request.clone(), version) {
            Ok((stream, response)) => Ok((stream, version, response)),
            Err(error)
                if version > protocol::MIN_PROTOCOL_VERSION && is_protocol_rejection(&error) =>
            {
                self.probe_latest()?;
                let negotiated = self.protocol_version.load(Ordering::Acquire);
                if request
                    .required_feature()
                    .is_some_and(|feature| !feature.is_supported_by(negotiated))
                {
                    return Err(unsupported_version("daemon does not support this request"));
                }
                self.send_with_version(request, negotiated)
                    .map(|(stream, response)| (stream, negotiated, response))
            }
            Err(error) => Err(error),
        }
    }

    fn send_with_version(&self, request: Request, version: u32) -> Result<(UnixStream, Response)> {
        let mut stream = UnixStream::connect(&self.socket_path).map_err(ClientError::Transport)?;
        protocol::write_message(&mut stream, &Envelope::with_version(version, request))
            .map_err(classify_wire_error)?;
        let response: Envelope<Response> =
            protocol::read_message(&mut stream).map_err(classify_wire_error)?;
        if response.version != version {
            if let Response::Error {
                message,
                code: Some(ErrorCode::UnsupportedVersion),
            } = response.message
            {
                return Err(ClientError::Remote(RemoteError {
                    code: Some(ErrorCode::UnsupportedVersion),
                    message,
                }));
            }
            return Err(ClientError::Protocol(ProtocolError::VersionMismatch {
                expected: version,
                actual: response.version,
            }));
        }
        match response.message {
            Response::Error { message, code } => {
                Err(ClientError::Remote(RemoteError { code, message }))
            }
            response => Ok((stream, response)),
        }
    }

    fn probe_latest(&self) -> Result<bool> {
        for version in (protocol::MIN_PROTOCOL_VERSION..=protocol::PROTOCOL_VERSION).rev() {
            match self.send_with_version(Request::Ping, version) {
                Ok((_, Response::Pong)) => {
                    self.protocol_version.store(version, Ordering::Release);
                    return Ok(version == protocol::PROTOCOL_VERSION);
                }
                Ok((_, response)) => return unexpected(response),
                Err(error) if is_protocol_rejection(&error) => {}
                Err(error) => return Err(error),
            }
        }
        Err(unsupported_version(
            "daemon has no compatible protocol version",
        ))
    }

    pub fn ping(&self) -> Result<()> {
        expect_ok(self.request(Request::Ping)?, Response::Pong)
    }

    pub fn shutdown(&self) -> Result<()> {
        expect_ok(self.request(Request::Shutdown)?, Response::Ok)?;
        for _ in 0..SHUTDOWN_ATTEMPTS {
            if !self.socket_path.exists() && daemon_lock_available(&self.socket_path)? {
                return Ok(());
            }
            thread::sleep(CONNECT_DELAY);
        }
        Err(ClientError::Lifecycle(LifecycleError::ShutdownTimeout))
    }

    pub fn restart(&self) -> Result<()> {
        self.restart_request(Request::Restart)
    }

    pub fn restart_with_notification_config(
        &self,
        notifications: NotificationDeliveryConfig,
    ) -> Result<()> {
        if !self.supports(protocol::ProtocolFeature::ScheduledExecutions)? {
            if self.supports(protocol::ProtocolFeature::RestartNotificationConfig)? {
                self.restart_request(Request::RestartWithNotificationConfig {
                    notifications: notifications.clone(),
                    environment: None,
                })?;
            } else {
                self.restart_request(Request::Restart)?;
            }
        }
        self.restart_request(Request::RestartWithNotificationConfig {
            notifications,
            environment: Some(current_environment()),
        })
    }

    fn restart_request(&self, request: Request) -> Result<()> {
        expect_ok(self.request(request)?, Response::Ok)?;
        let mut last_error = None;
        for _ in 0..CONNECT_ATTEMPTS {
            match self.ping() {
                Ok(()) => return Ok(()),
                Err(error) => last_error = Some(error),
            }
            thread::sleep(CONNECT_DELAY);
        }
        Err(ClientError::Lifecycle(
            LifecycleError::ReplacementStartTimeout(last_error.map(Box::new)),
        ))
    }

    pub fn snapshot(&self) -> Result<Snapshot> {
        match self.request(Request::Snapshot)? {
            Response::Snapshot { snapshot } => Ok(snapshot),
            other => unexpected(other),
        }
    }

    pub fn focused_terminal(&self) -> Result<Option<FocusedTerminalSnapshot>> {
        match self.request(Request::GetFocusedTerminal)? {
            Response::FocusedTerminal { focused_terminal } => Ok(focused_terminal),
            other => unexpected(other),
        }
    }

    pub fn get_workspace(&self, workspace_id: impl Into<String>) -> Result<WorkspaceSnapshot> {
        match self.request(Request::GetWorkspace {
            workspace_id: workspace_id.into(),
        })? {
            Response::Workspace { workspace } => Ok(workspace),
            other => unexpected(other),
        }
    }

    pub fn get_shell(&self, shell_id: impl Into<String>) -> Result<ShellSnapshot> {
        match self.request(Request::GetShell {
            shell_id: shell_id.into(),
        })? {
            Response::Shell { shell } => Ok(shell),
            other => unexpected(other),
        }
    }

    pub fn get_launcher(
        &self,
        launcher_id: impl Into<String>,
    ) -> Result<WorkspaceLauncherSnapshot> {
        match self.request(Request::GetLauncher {
            launcher_id: launcher_id.into(),
        })? {
            Response::Launcher { launcher } => Ok(launcher),
            other => unexpected(other),
        }
    }

    pub fn get_agent(&self, agent_id: impl Into<String>) -> Result<AgentInstanceSnapshot> {
        match self.request(Request::GetAgent {
            agent_id: agent_id.into(),
        })? {
            Response::Agent { agent } => Ok(agent),
            other => unexpected(other),
        }
    }

    pub fn get_agent_schedule(
        &self,
        schedule_id: impl Into<String>,
    ) -> Result<AgentScheduleInspection> {
        match self.request(Request::GetAgentSchedule {
            schedule_id: schedule_id.into(),
        })? {
            Response::AgentScheduleInspection { inspection } => Ok(inspection),
            other => unexpected(other),
        }
    }

    pub fn create_workspace(
        &self,
        name: impl Into<String>,
        shells: Vec<ShellSpec>,
    ) -> Result<WorkspaceSnapshot> {
        self.create_workspace_with_default_cwd(name, None, shells)
    }

    pub fn create_workspace_with_default_cwd(
        &self,
        name: impl Into<String>,
        default_cwd: Option<PathBuf>,
        shells: Vec<ShellSpec>,
    ) -> Result<WorkspaceSnapshot> {
        match self.request(Request::CreateWorkspace {
            name: name.into(),
            default_cwd,
            shells,
        })? {
            Response::Workspace { workspace } => Ok(workspace),
            other => unexpected(other),
        }
    }

    pub fn create_shell(
        &self,
        workspace_id: impl Into<String>,
        shell: ShellSpec,
    ) -> Result<ShellSnapshot> {
        match self.request(Request::CreateShell {
            workspace_id: Some(workspace_id.into()),
            shell,
        })? {
            Response::Shell { shell } => Ok(shell),
            other => unexpected(other),
        }
    }

    pub fn create_shell_with_workspace(&self, shell: ShellSpec) -> Result<ShellSnapshot> {
        match self.request(Request::CreateShell {
            workspace_id: None,
            shell,
        })? {
            Response::Shell { shell } => Ok(shell),
            other => unexpected(other),
        }
    }

    pub fn create_launcher(
        &self,
        workspace_id: impl Into<String>,
        spec: WorkspaceLauncherSpec,
    ) -> Result<WorkspaceLauncherSnapshot> {
        match self.request(Request::CreateLauncher {
            workspace_id: workspace_id.into(),
            spec,
        })? {
            Response::Launcher { launcher } => Ok(launcher),
            other => unexpected(other),
        }
    }

    pub fn create_agent_schedule(
        &self,
        workspace_id: impl Into<String>,
        spec: AgentScheduleSpec,
    ) -> Result<AgentScheduleSnapshot> {
        match self.request(Request::CreateAgentSchedule {
            workspace_id: workspace_id.into(),
            spec,
        })? {
            Response::AgentSchedule { schedule } => Ok(schedule),
            other => unexpected(other),
        }
    }

    pub fn run_agent_schedule(
        &self,
        schedule_id: impl Into<String>,
        dispatch_key: impl Into<String>,
    ) -> Result<ScheduledExecutionSnapshot> {
        match self.request(Request::RunAgentSchedule {
            schedule_id: schedule_id.into(),
            dispatch_key: dispatch_key.into(),
        })? {
            Response::ScheduledExecution { execution } => Ok(execution),
            other => unexpected(other),
        }
    }

    pub fn scheduled_executions(
        &self,
        workspace_id: Option<String>,
        schedule_id: Option<String>,
    ) -> Result<Vec<ScheduledExecutionSnapshot>> {
        match self.request(Request::ListScheduledExecutions {
            workspace_id,
            schedule_id,
        })? {
            Response::ScheduledExecutions { executions } => Ok(executions),
            other => unexpected(other),
        }
    }

    pub fn get_scheduled_execution(
        &self,
        execution_id: impl Into<String>,
    ) -> Result<ScheduledExecutionSnapshot> {
        match self.request(Request::GetScheduledExecution {
            execution_id: execution_id.into(),
        })? {
            Response::ScheduledExecution { execution } => Ok(execution),
            other => unexpected(other),
        }
    }

    pub fn cancel_scheduled_execution(
        &self,
        execution_id: impl Into<String>,
    ) -> Result<ScheduledExecutionSnapshot> {
        match self.request(Request::CancelScheduledExecution {
            execution_id: execution_id.into(),
        })? {
            Response::ScheduledExecution { execution } => Ok(execution),
            other => unexpected(other),
        }
    }

    pub fn resolve_scheduled_execution_claim(
        &self,
        schedule_id: impl Into<String>,
        shell_id: impl Into<String>,
        run_id: impl Into<String>,
        runner_token: impl Into<String>,
    ) -> Result<protocol::ScheduledExecutionClaim> {
        match self.request(Request::ResolveScheduledExecutionClaim {
            schedule_id: schedule_id.into(),
            shell_id: shell_id.into(),
            run_id: run_id.into(),
            runner_token: protocol::ScheduledRunnerCapability::new(runner_token),
        })? {
            Response::ScheduledExecutionClaim { claim } => Ok(claim),
            other => unexpected(other),
        }
    }

    pub fn report_scheduled_runner(
        &self,
        execution_id: impl Into<String>,
        shell_id: impl Into<String>,
        run_id: impl Into<String>,
        runner_token: impl Into<String>,
        result: ScheduledRunnerResult,
    ) -> Result<ScheduledExecutionSnapshot> {
        match self.request(Request::ReportScheduledRunner {
            execution_id: execution_id.into(),
            shell_id: shell_id.into(),
            run_id: run_id.into(),
            runner_token: protocol::ScheduledRunnerCapability::new(runner_token),
            result,
        })? {
            Response::ScheduledExecution { execution } => Ok(execution),
            other => unexpected(other),
        }
    }

    pub fn register_agent(
        &self,
        shell_id: impl Into<String>,
        run_id: impl Into<String>,
        spec: AgentRegistrationSpec,
    ) -> Result<AgentInstanceSnapshot> {
        match self.request(Request::RegisterAgent {
            shell_id: shell_id.into(),
            run_id: run_id.into(),
            spec,
        })? {
            Response::Agent { agent } => Ok(agent),
            other => unexpected(other),
        }
    }

    pub fn ensure_agent(
        &self,
        shell_id: impl Into<String>,
        run_id: impl Into<String>,
        spec: AgentRegistrationSpec,
    ) -> Result<AgentInstanceSnapshot> {
        match self.request(Request::EnsureAgent {
            shell_id: shell_id.into(),
            run_id: run_id.into(),
            spec,
        })? {
            Response::Agent { agent } => Ok(agent),
            other => unexpected(other),
        }
    }

    pub fn report_agent(
        &self,
        agent_id: impl Into<String>,
        run_id: impl Into<String>,
        report: AgentReport,
    ) -> Result<AgentInstanceSnapshot> {
        match self.request(Request::ReportAgent {
            agent_id: agent_id.into(),
            run_id: run_id.into(),
            report,
        })? {
            Response::Agent { agent } => Ok(agent),
            other => unexpected(other),
        }
    }

    pub fn wait_agent(
        &self,
        agent_id: impl Into<String>,
        after_revision: u64,
        wait_ms: u32,
    ) -> Result<AgentWait> {
        let request = Request::WaitAgent {
            agent_id: agent_id.into(),
            after_revision,
            wait_ms,
        };
        if !self.supports(protocol::ProtocolFeature::RevisionAwareAgentWait)? {
            return Err(unsupported_version("daemon does not support Agent wait"));
        }
        match self.request(request)? {
            Response::AgentWait { agent, changed } => Ok(AgentWait { agent, changed }),
            other => unexpected(other),
        }
    }

    pub fn acknowledge_agent_attention(
        &self,
        agent_id: impl Into<String>,
        observation_revision: u64,
    ) -> Result<AgentAttentionAcknowledgement> {
        match self.request(Request::AcknowledgeAgentAttention {
            agent_id: agent_id.into(),
            observation_revision,
        })? {
            Response::AgentAttentionAcknowledged { agent, changed } => {
                Ok(AgentAttentionAcknowledgement { agent, changed })
            }
            other => unexpected(other),
        }
    }

    pub fn read_shell(&self, shell_id: impl Into<String>, max_bytes: usize) -> Result<Vec<u8>> {
        match self.request(Request::ReadShell {
            shell_id: shell_id.into(),
            max_bytes,
        })? {
            Response::Output { bytes } => Ok(bytes),
            other => unexpected(other),
        }
    }

    pub fn read_shell_preview(
        &self,
        shell_id: impl Into<String>,
        max_bytes: usize,
        max_lines: u16,
    ) -> Result<TerminalPreview> {
        let shell_id = shell_id.into();
        if !self.supports(protocol::ProtocolFeature::StructuredTerminalPreview)? {
            let bytes = self.read_shell(shell_id, max_bytes)?;
            let text = String::from_utf8_lossy(&bytes);
            let lines = text.lines().collect::<Vec<_>>();
            let start = lines.len().saturating_sub(usize::from(max_lines));
            return Ok(TerminalPreview {
                lines: lines[start..]
                    .iter()
                    .map(|line| TerminalPreviewLine {
                        spans: vec![TerminalPreviewSpan {
                            text: (*line).to_owned(),
                            style: Default::default(),
                        }],
                    })
                    .collect(),
            });
        }
        match self.request(Request::ReadShellPreview {
            shell_id,
            max_bytes,
            max_lines,
        })? {
            Response::ShellPreview { preview } => Ok(preview),
            other => unexpected(other),
        }
    }

    pub fn read_shell_at(
        &self,
        shell_id: impl Into<String>,
        max_bytes: usize,
        run_id: Option<String>,
        after_revision: Option<u64>,
        wait_ms: u32,
    ) -> Result<OutputState> {
        match self.request(Request::ReadShellAt {
            shell_id: shell_id.into(),
            max_bytes,
            run_id,
            after_revision,
            wait_ms,
        })? {
            Response::OutputState {
                bytes,
                run_id,
                output_revision,
                changed,
                status,
            } => Ok(OutputState {
                bytes,
                run_id,
                output_revision,
                changed,
                status,
            }),
            other => unexpected(other),
        }
    }

    pub fn events(
        &self,
        after: Option<EventCursor>,
        limit: u16,
        wait_ms: u32,
    ) -> Result<EventBatch> {
        match self.request(Request::Events {
            after,
            limit,
            wait_ms,
        })? {
            Response::Events {
                stream_id,
                cursor,
                snapshot,
                events,
            } => Ok(EventBatch {
                stream_id,
                cursor,
                snapshot,
                events,
            }),
            other => unexpected(other),
        }
    }

    pub fn rename_workspace(
        &self,
        workspace_id: impl Into<String>,
        name: impl Into<String>,
    ) -> Result<()> {
        expect_ok(
            self.request(Request::RenameWorkspace {
                workspace_id: workspace_id.into(),
                name: name.into(),
            })?,
            Response::Ok,
        )
    }

    pub fn rename_shell(&self, shell_id: impl Into<String>, name: impl Into<String>) -> Result<()> {
        expect_ok(
            self.request(Request::RenameShell {
                shell_id: shell_id.into(),
                name: name.into(),
            })?,
            Response::Ok,
        )
    }

    pub fn rename_launcher(
        &self,
        launcher_id: impl Into<String>,
        name: impl Into<String>,
    ) -> Result<()> {
        expect_ok(
            self.request(Request::RenameLauncher {
                launcher_id: launcher_id.into(),
                name: name.into(),
            })?,
            Response::Ok,
        )
    }

    pub fn remove_launcher(&self, launcher_id: impl Into<String>) -> Result<()> {
        expect_ok(
            self.request(Request::RemoveLauncher {
                launcher_id: launcher_id.into(),
            })?,
            Response::Ok,
        )
    }

    pub fn pause_agent_schedule(
        &self,
        schedule_id: impl Into<String>,
    ) -> Result<AgentScheduleSnapshot> {
        match self.request(Request::PauseAgentSchedule {
            schedule_id: schedule_id.into(),
        })? {
            Response::AgentSchedule { schedule } => Ok(schedule),
            other => unexpected(other),
        }
    }

    pub fn resume_agent_schedule(
        &self,
        schedule_id: impl Into<String>,
    ) -> Result<AgentScheduleSnapshot> {
        match self.request(Request::ResumeAgentSchedule {
            schedule_id: schedule_id.into(),
        })? {
            Response::AgentSchedule { schedule } => Ok(schedule),
            other => unexpected(other),
        }
    }

    pub fn remove_agent_schedule(&self, schedule_id: impl Into<String>) -> Result<()> {
        expect_ok(
            self.request(Request::RemoveAgentSchedule {
                schedule_id: schedule_id.into(),
            })?,
            Response::Ok,
        )
    }

    pub fn close_workspace(&self, workspace_id: impl Into<String>) -> Result<()> {
        expect_ok(
            self.request(Request::CloseWorkspace {
                workspace_id: workspace_id.into(),
            })?,
            Response::Ok,
        )
    }

    pub fn close_shell(&self, shell_id: impl Into<String>) -> Result<()> {
        expect_ok(
            self.request(Request::CloseShell {
                shell_id: shell_id.into(),
            })?,
            Response::Ok,
        )
    }

    pub fn restart_shell(&self, shell_id: impl Into<String>) -> Result<ShellSnapshot> {
        match self.request(Request::RestartShell {
            shell_id: shell_id.into(),
        })? {
            Response::Shell { shell } => Ok(shell),
            other => unexpected(other),
        }
    }

    pub fn attach(
        &self,
        shell_id: impl Into<String>,
        takeover: bool,
        profile: TerminalProfile,
    ) -> Result<Attachment> {
        self.attach_with_restart(shell_id.into(), takeover, false, profile, None)
    }

    pub fn attach_with_client_environment(
        &self,
        shell_id: impl Into<String>,
        takeover: bool,
        profile: TerminalProfile,
    ) -> Result<Attachment> {
        self.attach_with_restart(
            shell_id.into(),
            takeover,
            false,
            profile,
            Some(current_environment()),
        )
    }

    pub fn attach_restarting(
        &self,
        shell_id: impl Into<String>,
        takeover: bool,
        profile: TerminalProfile,
    ) -> Result<Attachment> {
        self.attach_with_restart(shell_id.into(), takeover, true, profile, None)
    }

    pub fn attach_restarting_with_client_environment(
        &self,
        shell_id: impl Into<String>,
        takeover: bool,
        profile: TerminalProfile,
    ) -> Result<Attachment> {
        self.attach_with_restart(
            shell_id.into(),
            takeover,
            true,
            profile,
            Some(current_environment()),
        )
    }

    fn attach_with_restart(
        &self,
        shell_id: String,
        takeover: bool,
        restart_exited: bool,
        profile: TerminalProfile,
        environment: Option<UnixEnvironment>,
    ) -> Result<Attachment> {
        let (stream, protocol_version, response) = self.send(Request::Attach {
            shell_id,
            takeover,
            restart_exited,
            profile,
            environment,
        })?;
        match response {
            Response::Attached {
                token,
                reconstruction,
                warning,
            } => Ok(Attachment {
                stream,
                protocol_version,
                token,
                reconstruction,
                warning,
            }),
            other => unexpected(other),
        }
    }
}

fn current_environment() -> UnixEnvironment {
    UnixEnvironment {
        variables: env::vars_os()
            .map(|(name, value)| UnixEnvironmentVariable {
                name: name.as_os_str().as_bytes().to_vec(),
                value: value.as_os_str().as_bytes().to_vec(),
            })
            .collect(),
    }
}

fn is_protocol_rejection(error: &ClientError) -> bool {
    matches!(
        error,
        ClientError::Remote(RemoteError {
            code: Some(ErrorCode::UnsupportedVersion),
            ..
        }) | ClientError::Protocol(ProtocolError::VersionMismatch { .. })
    )
}

fn unsupported_version(message: impl Into<String>) -> ClientError {
    ClientError::Protocol(ProtocolError::UnsupportedVersion(message.into()))
}

fn classify_wire_error(error: io::Error) -> ClientError {
    if error.kind() == io::ErrorKind::InvalidData {
        ClientError::Protocol(ProtocolError::InvalidMessage(error))
    } else {
        ClientError::Transport(error)
    }
}

fn daemon_lock_available(socket_path: &Path) -> io::Result<bool> {
    let lock_path = socket_path
        .parent()
        .ok_or_else(|| io::Error::other("daemon socket has no parent"))?
        .join("daemon.lock");
    let lock = match OpenOptions::new().read(true).write(true).open(lock_path) {
        Ok(lock) => lock,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(true),
        Err(error) => return Err(error),
    };
    // The descriptor remains live for both flock operations.
    if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
        let _ = unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_UN) };
        return Ok(true);
    }
    let error = io::Error::last_os_error();
    if error.raw_os_error() == Some(libc::EWOULDBLOCK) {
        Ok(false)
    } else {
        Err(error)
    }
}

fn expect_ok(actual: Response, expected: Response) -> Result<()> {
    if actual == expected {
        Ok(())
    } else {
        unexpected(actual)
    }
}

fn unexpected<T>(response: Response) -> Result<T> {
    Err(ClientError::Protocol(ProtocolError::UnexpectedResponse(
        Box::new(response),
    )))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::net::UnixListener;

    use uuid::Uuid;

    fn reject_protocol(listener: &UnixListener, attempted: u32, supported: u32) {
        let (mut stream, _) = listener.accept().unwrap();
        let request: Envelope<Request> = protocol::read_message(&mut stream).unwrap();
        assert_eq!(request.version, attempted);
        assert!(matches!(
            request.message,
            Request::Ping | Request::Attach { .. }
        ));
        protocol::write_message(
            &mut stream,
            &Envelope::with_version(
                supported,
                Response::Error {
                    message: format!("protocol {attempted} unsupported"),
                    code: Some(ErrorCode::UnsupportedVersion),
                },
            ),
        )
        .unwrap();
    }

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

    #[test]
    fn direct_client_negotiates_before_sending_agent_wait() {
        let directory = env::temp_dir().join(format!("boomux-client-{}", Uuid::new_v4()));
        fs::create_dir_all(&directory).unwrap();
        let socket = directory.join("daemon.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let server = thread::spawn(move || {
            reject_protocol(&listener, 23, 22);
            reject_protocol(&listener, 22, 21);
            reject_protocol(&listener, 21, 20);
            let (mut stream, _) = listener.accept().unwrap();
            let request: Envelope<Request> = protocol::read_message(&mut stream).unwrap();
            assert_eq!(request.version, 20);
            assert!(matches!(request.message, Request::Ping));
            protocol::write_message(
                &mut stream,
                &Envelope::with_version(
                    19,
                    Response::Error {
                        message: "protocol 20 unsupported".into(),
                        code: Some(ErrorCode::UnsupportedVersion),
                    },
                ),
            )
            .unwrap();

            let (mut stream, _) = listener.accept().unwrap();
            let request: Envelope<Request> = protocol::read_message(&mut stream).unwrap();
            assert_eq!(request.version, 19);
            assert!(matches!(request.message, Request::Ping));
            protocol::write_message(
                &mut stream,
                &Envelope::with_version(
                    18,
                    Response::Error {
                        message: "protocol 19 unsupported".into(),
                        code: Some(ErrorCode::UnsupportedVersion),
                    },
                ),
            )
            .unwrap();

            let (mut stream, _) = listener.accept().unwrap();
            let request: Envelope<Request> = protocol::read_message(&mut stream).unwrap();
            assert_eq!(request.version, 18);
            assert!(matches!(request.message, Request::Ping));
            protocol::write_message(
                &mut stream,
                &Envelope::with_version(
                    17,
                    Response::Error {
                        message: "protocol 18 unsupported".into(),
                        code: Some(ErrorCode::UnsupportedVersion),
                    },
                ),
            )
            .unwrap();

            let (mut stream, _) = listener.accept().unwrap();
            let request: Envelope<Request> = protocol::read_message(&mut stream).unwrap();
            assert_eq!(request.version, 17);
            assert!(matches!(request.message, Request::Ping));
            protocol::write_message(
                &mut stream,
                &Envelope::with_version(
                    16,
                    Response::Error {
                        message: "protocol 17 unsupported".into(),
                        code: Some(ErrorCode::UnsupportedVersion),
                    },
                ),
            )
            .unwrap();

            let (mut stream, _) = listener.accept().unwrap();
            let request: Envelope<Request> = protocol::read_message(&mut stream).unwrap();
            assert_eq!(request.version, 16);
            assert!(matches!(request.message, Request::Ping));
            protocol::write_message(
                &mut stream,
                &Envelope::with_version(
                    15,
                    Response::Error {
                        message: "protocol 16 unsupported".into(),
                        code: Some(ErrorCode::UnsupportedVersion),
                    },
                ),
            )
            .unwrap();

            let (mut stream, _) = listener.accept().unwrap();
            let request: Envelope<Request> = protocol::read_message(&mut stream).unwrap();
            assert_eq!(request.version, 15);
            assert!(matches!(request.message, Request::Ping));
            protocol::write_message(
                &mut stream,
                &Envelope::with_version(
                    14,
                    Response::Error {
                        message: "protocol 15 unsupported".into(),
                        code: Some(ErrorCode::UnsupportedVersion),
                    },
                ),
            )
            .unwrap();

            let (mut stream, _) = listener.accept().unwrap();
            let request: Envelope<Request> = protocol::read_message(&mut stream).unwrap();
            assert_eq!(request.version, 14);
            assert!(matches!(request.message, Request::Ping));
            protocol::write_message(
                &mut stream,
                &Envelope::with_version(
                    13,
                    Response::Error {
                        message: "protocol 14 unsupported".into(),
                        code: Some(ErrorCode::UnsupportedVersion),
                    },
                ),
            )
            .unwrap();

            let (mut stream, _) = listener.accept().unwrap();
            let request: Envelope<Request> = protocol::read_message(&mut stream).unwrap();
            assert_eq!(request.version, 13);
            assert!(matches!(request.message, Request::Ping));
            protocol::write_message(&mut stream, &Envelope::with_version(13, Response::Pong))
                .unwrap();
        });
        let client = Client::from_socket_path(socket);

        let error = client.wait_agent("a1", 1, 0).unwrap_err();

        assert!(matches!(
            error,
            ClientError::Protocol(ProtocolError::UnsupportedVersion(ref message))
                if message == "daemon does not support Agent wait"
        ));
        server.join().unwrap();
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn io_errors_convert_to_transport_errors() {
        let error = ClientError::from(io::Error::new(
            io::ErrorKind::ConnectionRefused,
            "connection refused",
        ));

        assert!(matches!(
            error,
            ClientError::Transport(ref error)
                if error.kind() == io::ErrorKind::ConnectionRefused
                    && error.to_string() == "connection refused"
        ));
    }

    #[test]
    fn invalid_wire_data_converts_to_a_protocol_error() {
        let error = classify_wire_error(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid response",
        ));

        assert!(matches!(
            error,
            ClientError::Protocol(ProtocolError::InvalidMessage(ref error))
                if error.to_string() == "invalid response"
        ));
    }

    #[test]
    fn attach_captures_the_full_unix_environment() {
        let expected: std::collections::HashMap<Vec<u8>, Vec<u8>> = env::vars_os()
            .map(|(name, value)| {
                (
                    name.as_os_str().as_bytes().to_vec(),
                    value.as_os_str().as_bytes().to_vec(),
                )
            })
            .collect();
        let directory = env::temp_dir().join(format!("boomux-client-{}", Uuid::new_v4()));
        fs::create_dir_all(&directory).unwrap();
        let socket = directory.join("daemon.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let request: Envelope<Request> = protocol::read_message(&mut stream).unwrap();
            assert_eq!(request.version, 23);
            let Request::Attach {
                environment: Some(environment),
                ..
            } = request.message
            else {
                panic!("attach did not include an environment");
            };
            let actual: std::collections::HashMap<Vec<u8>, Vec<u8>> = environment
                .variables
                .into_iter()
                .map(|variable| (variable.name, variable.value))
                .collect();
            assert_eq!(actual, expected);
            protocol::write_message(
                &mut stream,
                &Envelope::with_version(
                    23,
                    Response::Attached {
                        token: "token".into(),
                        reconstruction: Vec::new(),
                        warning: None,
                    },
                ),
            )
            .unwrap();
        });
        let client = Client::from_socket_path(socket);

        let attachment = client
            .attach_with_client_environment("s1", false, test_profile())
            .unwrap();

        assert_eq!(attachment.token, "token");
        server.join().unwrap();
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn attachment_preserves_the_version_used_for_its_handshake() {
        let directory = env::temp_dir().join(format!("boomux-client-{}", Uuid::new_v4()));
        fs::create_dir_all(&directory).unwrap();
        let socket = directory.join("daemon.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let server = thread::spawn(move || {
            loop {
                let (mut stream, _) = listener.accept().unwrap();
                let request: Envelope<Request> = protocol::read_message(&mut stream).unwrap();
                if request.version > 17 {
                    assert!(matches!(
                        request.message,
                        Request::Ping | Request::Attach { .. }
                    ));
                    protocol::write_message(
                        &mut stream,
                        &Envelope::with_version(
                            request.version - 1,
                            Response::Error {
                                message: format!("protocol {} unsupported", request.version),
                                code: Some(ErrorCode::UnsupportedVersion),
                            },
                        ),
                    )
                    .unwrap();
                    continue;
                }
                assert_eq!(request.version, 17);
                match request.message {
                    Request::Ping => protocol::write_message(
                        &mut stream,
                        &Envelope::with_version(17, Response::Pong),
                    )
                    .unwrap(),
                    Request::Attach { .. } => {
                        protocol::write_message(
                            &mut stream,
                            &Envelope::with_version(
                                17,
                                Response::Attached {
                                    token: "token".into(),
                                    reconstruction: Vec::new(),
                                    warning: None,
                                },
                            ),
                        )
                        .unwrap();
                        break;
                    }
                    request => panic!("unexpected request: {request:?}"),
                }
            }
        });
        let client = Client::from_socket_path(socket);

        let attachment = client.attach("s1", false, test_profile()).unwrap();

        assert_eq!(attachment.protocol_version, 17);
        server.join().unwrap();
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn shell_preview_falls_back_to_plain_output_from_protocol_nineteen() {
        let directory = env::temp_dir().join(format!("boomux-client-{}", Uuid::new_v4()));
        fs::create_dir_all(&directory).unwrap();
        let socket = directory.join("daemon.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let server = thread::spawn(move || {
            reject_protocol(&listener, 23, 22);
            reject_protocol(&listener, 22, 21);
            reject_protocol(&listener, 21, 20);
            let (mut stream, _) = listener.accept().unwrap();
            let request: Envelope<Request> = protocol::read_message(&mut stream).unwrap();
            assert_eq!(request.version, 20);
            assert!(matches!(request.message, Request::Ping));
            protocol::write_message(
                &mut stream,
                &Envelope::with_version(
                    19,
                    Response::Error {
                        message: "protocol 20 unsupported".into(),
                        code: Some(ErrorCode::UnsupportedVersion),
                    },
                ),
            )
            .unwrap();

            let (mut stream, _) = listener.accept().unwrap();
            let request: Envelope<Request> = protocol::read_message(&mut stream).unwrap();
            assert_eq!(request.version, 19);
            assert!(matches!(request.message, Request::Ping));
            protocol::write_message(&mut stream, &Envelope::with_version(19, Response::Pong))
                .unwrap();

            let (mut stream, _) = listener.accept().unwrap();
            let request: Envelope<Request> = protocol::read_message(&mut stream).unwrap();
            assert_eq!(request.version, 19);
            assert!(matches!(request.message, Request::ReadShell { .. }));
            protocol::write_message(
                &mut stream,
                &Envelope::with_version(
                    19,
                    Response::Output {
                        bytes: b"old\nlatest".to_vec(),
                    },
                ),
            )
            .unwrap();
        });
        let client = Client::from_socket_path(socket);

        let preview = client.read_shell_preview("s1", 1024, 1).unwrap();

        assert_eq!(preview.lines.len(), 1);
        assert_eq!(preview.lines[0].spans[0].text, "latest");
        assert_eq!(preview.lines[0].spans[0].style, Default::default());
        server.join().unwrap();
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn negotiates_protocol_seven_without_losing_version_seven_requests() {
        let directory = env::temp_dir().join(format!("boomux-client-{}", Uuid::new_v4()));
        fs::create_dir_all(&directory).unwrap();
        let socket = directory.join("daemon.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let server = thread::spawn(move || {
            reject_protocol(&listener, 23, 22);
            reject_protocol(&listener, 22, 21);
            reject_protocol(&listener, 21, 20);
            let (mut stream, _) = listener.accept().unwrap();
            let request: Envelope<Request> = protocol::read_message(&mut stream).unwrap();
            assert_eq!(request.version, 20);
            assert!(matches!(request.message, Request::Ping));
            protocol::write_message(
                &mut stream,
                &Envelope::with_version(
                    19,
                    Response::Error {
                        message: "expected an older protocol".into(),
                        code: Some(ErrorCode::UnsupportedVersion),
                    },
                ),
            )
            .unwrap();

            let (mut stream, _) = listener.accept().unwrap();
            let request: Envelope<Request> = protocol::read_message(&mut stream).unwrap();
            assert_eq!(request.version, 19);
            assert!(matches!(request.message, Request::Ping));
            protocol::write_message(
                &mut stream,
                &Envelope::with_version(
                    18,
                    Response::Error {
                        message: "expected an older protocol".into(),
                        code: Some(ErrorCode::UnsupportedVersion),
                    },
                ),
            )
            .unwrap();

            let (mut stream, _) = listener.accept().unwrap();
            let request: Envelope<Request> = protocol::read_message(&mut stream).unwrap();
            assert_eq!(request.version, 18);
            assert!(matches!(request.message, Request::Ping));
            protocol::write_message(
                &mut stream,
                &Envelope::with_version(
                    17,
                    Response::Error {
                        message: "expected an older protocol".into(),
                        code: Some(ErrorCode::UnsupportedVersion),
                    },
                ),
            )
            .unwrap();

            let (mut stream, _) = listener.accept().unwrap();
            let request: Envelope<Request> = protocol::read_message(&mut stream).unwrap();
            assert_eq!(request.version, 17);
            assert!(matches!(request.message, Request::Ping));
            protocol::write_message(
                &mut stream,
                &Envelope::with_version(
                    16,
                    Response::Error {
                        message: "expected an older protocol".into(),
                        code: Some(ErrorCode::UnsupportedVersion),
                    },
                ),
            )
            .unwrap();

            let (mut stream, _) = listener.accept().unwrap();
            let request: Envelope<Request> = protocol::read_message(&mut stream).unwrap();
            assert_eq!(request.version, 16);
            assert!(matches!(request.message, Request::Ping));
            protocol::write_message(
                &mut stream,
                &Envelope::with_version(
                    15,
                    Response::Error {
                        message: "expected an older protocol".into(),
                        code: Some(ErrorCode::UnsupportedVersion),
                    },
                ),
            )
            .unwrap();

            let (mut stream, _) = listener.accept().unwrap();
            let request: Envelope<Request> = protocol::read_message(&mut stream).unwrap();
            assert_eq!(request.version, 15);
            assert!(matches!(request.message, Request::Ping));
            protocol::write_message(
                &mut stream,
                &Envelope::with_version(
                    14,
                    Response::Error {
                        message: "expected an older protocol".into(),
                        code: Some(ErrorCode::UnsupportedVersion),
                    },
                ),
            )
            .unwrap();

            let (mut stream, _) = listener.accept().unwrap();
            let request: Envelope<Request> = protocol::read_message(&mut stream).unwrap();
            assert_eq!(request.version, 14);
            assert!(matches!(request.message, Request::Ping));
            protocol::write_message(
                &mut stream,
                &Envelope::with_version(
                    13,
                    Response::Error {
                        message: "expected an older protocol".into(),
                        code: Some(ErrorCode::UnsupportedVersion),
                    },
                ),
            )
            .unwrap();

            let (mut stream, _) = listener.accept().unwrap();
            let request: Envelope<Request> = protocol::read_message(&mut stream).unwrap();
            assert_eq!(request.version, 13);
            assert!(matches!(request.message, Request::Ping));
            protocol::write_message(
                &mut stream,
                &Envelope::with_version(
                    12,
                    Response::Error {
                        message: "expected an older protocol".into(),
                        code: Some(ErrorCode::UnsupportedVersion),
                    },
                ),
            )
            .unwrap();

            let (mut stream, _) = listener.accept().unwrap();
            let request: Envelope<Request> = protocol::read_message(&mut stream).unwrap();
            assert_eq!(request.version, 12);
            assert!(matches!(request.message, Request::Ping));
            protocol::write_message(
                &mut stream,
                &Envelope::with_version(
                    11,
                    Response::Error {
                        message: "expected protocol 7".into(),
                        code: Some(ErrorCode::UnsupportedVersion),
                    },
                ),
            )
            .unwrap();

            let (mut stream, _) = listener.accept().unwrap();
            let request: Envelope<Request> = protocol::read_message(&mut stream).unwrap();
            assert_eq!(request.version, 11);
            assert!(matches!(request.message, Request::Ping));
            protocol::write_message(
                &mut stream,
                &Envelope::with_version(
                    11,
                    Response::Error {
                        message: "expected protocol 7".into(),
                        code: Some(ErrorCode::UnsupportedVersion),
                    },
                ),
            )
            .unwrap();

            let (mut stream, _) = listener.accept().unwrap();
            let request: Envelope<Request> = protocol::read_message(&mut stream).unwrap();
            assert_eq!(request.version, 10);
            assert!(matches!(request.message, Request::Ping));
            protocol::write_message(
                &mut stream,
                &Envelope::with_version(
                    10,
                    Response::Error {
                        message: "expected protocol 7".into(),
                        code: Some(ErrorCode::UnsupportedVersion),
                    },
                ),
            )
            .unwrap();

            let (mut stream, _) = listener.accept().unwrap();
            let request: Envelope<Request> = protocol::read_message(&mut stream).unwrap();
            assert_eq!(request.version, 9);
            assert!(matches!(request.message, Request::Ping));
            protocol::write_message(
                &mut stream,
                &Envelope::with_version(
                    9,
                    Response::Error {
                        message: "expected protocol 7".into(),
                        code: Some(ErrorCode::UnsupportedVersion),
                    },
                ),
            )
            .unwrap();

            let (mut stream, _) = listener.accept().unwrap();
            let request: Envelope<Request> = protocol::read_message(&mut stream).unwrap();
            assert_eq!(request.version, 8);
            assert!(matches!(request.message, Request::Ping));
            protocol::write_message(
                &mut stream,
                &Envelope::with_version(
                    8,
                    Response::Error {
                        message: "expected protocol 7".into(),
                        code: Some(ErrorCode::UnsupportedVersion),
                    },
                ),
            )
            .unwrap();

            let (mut stream, _) = listener.accept().unwrap();
            let request: Envelope<Request> = protocol::read_message(&mut stream).unwrap();
            assert_eq!(request.version, 7);
            assert!(matches!(request.message, Request::Ping));
            protocol::write_message(&mut stream, &Envelope::with_version(7, Response::Pong))
                .unwrap();

            let (mut stream, _) = listener.accept().unwrap();
            let request: Envelope<Request> = protocol::read_message(&mut stream).unwrap();
            assert_eq!(request.version, 7);
            assert!(matches!(request.message, Request::Events { .. }));
            protocol::write_message(
                &mut stream,
                &Envelope::with_version(
                    7,
                    Response::Events {
                        stream_id: "stream".into(),
                        cursor: EventCursor {
                            stream_id: "stream".into(),
                            event_id: 0,
                        },
                        snapshot: None,
                        events: Vec::new(),
                    },
                ),
            )
            .unwrap();

            let (mut stream, _) = listener.accept().unwrap();
            let request: Envelope<Request> = protocol::read_message(&mut stream).unwrap();
            assert_eq!(request.version, 7);
            assert!(matches!(request.message, Request::ReadShellAt { .. }));
            protocol::write_message(
                &mut stream,
                &Envelope::with_version(
                    7,
                    Response::OutputState {
                        bytes: Vec::new(),
                        run_id: None,
                        output_revision: None,
                        changed: true,
                        status: ShellStatus::Pending,
                    },
                ),
            )
            .unwrap();
        });

        let client = Client::from_socket_path(socket);
        assert_eq!(client.protocol_version().unwrap(), 7);
        assert!(client.events(None, 1, 0).is_ok());
        assert!(client.read_shell_at("shell", 1, None, None, 0).is_ok());
        server.join().unwrap();
        fs::remove_dir_all(directory).unwrap();
    }
}

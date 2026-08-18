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
    AgentScheduleSnapshot, AgentScheduleSpec, AgentScheduleUpdate, CombinedNodeSnapshot,
    DaemonEvent, Envelope, ErrorCode, EventCursor, FocusedTerminalSnapshot, LocalNodeMetadata,
    NodeProjectionHealth, NodeRegistrationSnapshot, NotificationDeliveryConfig, Request, Response,
    RoutedOperation, RoutedOperationResult, ScheduledExecutionScheduleProjection,
    ScheduledExecutionSnapshot, ScheduledOccurrence, ScheduledRunnerResult, ShellSnapshot,
    ShellSpec, ShellStatus, Snapshot, TerminalPreview, TerminalPreviewLine, TerminalPreviewSpan,
    TerminalProfile, UnixEnvironment, UnixEnvironmentVariable, WorkspaceLauncherSnapshot,
    WorkspaceLauncherSpec, WorkspaceSnapshot,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DaemonPeerCredentials {
    pub pid: u32,
    pub uid: u32,
    pub protocol_version: u32,
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
pub struct FederationChannel {
    pub stream: UnixStream,
    pub protocol_version: u32,
    pub node_id: String,
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

#[derive(Debug)]
pub struct SnapshotWatchPoll {
    pub changed: bool,
    pub stream_changed: bool,
    pub baseline_replaced: bool,
    pub events: Vec<DaemonEvent>,
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

    pub fn poll(&mut self, client: &Client) -> Result<SnapshotWatchPoll> {
        let Some(cursor) = self.cursor.clone() else {
            return Ok(SnapshotWatchPoll {
                changed: false,
                stream_changed: false,
                baseline_replaced: false,
                events: Vec::new(),
            });
        };
        let stream_id = cursor.stream_id.clone();
        match client.events(Some(cursor), 256, 0) {
            Ok(batch) => {
                if batch.events.is_empty() {
                    self.cursor = Some(batch.cursor);
                    return Ok(SnapshotWatchPoll {
                        changed: false,
                        stream_changed: false,
                        baseline_replaced: false,
                        events: Vec::new(),
                    });
                }
                let stream_changed = batch.stream_id != stream_id;
                let snapshot = client.snapshot()?;
                self.cursor = Some(batch.cursor);
                self.snapshot = snapshot;
                Ok(SnapshotWatchPoll {
                    changed: true,
                    stream_changed,
                    baseline_replaced: false,
                    events: batch.events,
                })
            }
            Err(ClientError::Remote(RemoteError {
                code: Some(ErrorCode::CursorExpired),
                ..
            })) => {
                *self = Self::baseline(client)?;
                Ok(SnapshotWatchPoll {
                    changed: true,
                    stream_changed: self.stream_id() != Some(stream_id.as_str()),
                    baseline_replaced: true,
                    events: Vec::new(),
                })
            }
            Err(ClientError::Remote(RemoteError {
                code: Some(ErrorCode::UnsupportedVersion),
                ..
            }))
            | Err(ClientError::Protocol(ProtocolError::UnsupportedVersion(_))) => {
                *self = Self::baseline(client)?;
                Ok(SnapshotWatchPoll {
                    changed: true,
                    stream_changed: self.stream_id() != Some(stream_id.as_str()),
                    baseline_replaced: true,
                    events: Vec::new(),
                })
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
pub struct ScheduledExecutionWait {
    pub execution: ScheduledExecutionSnapshot,
    pub changed: bool,
}

#[derive(Debug)]
pub struct ScheduledExecutionList {
    pub executions: Vec<ScheduledExecutionSnapshot>,
    pub limit: u16,
    pub truncated: bool,
    pub schedules: Vec<ScheduledExecutionScheduleProjection>,
    pub schedule_limit: u16,
    pub schedules_truncated: bool,
}

#[derive(Debug)]
pub struct ScheduledExecutionInspection {
    pub execution: ScheduledExecutionSnapshot,
    pub next_occurrence: Option<ScheduledOccurrence>,
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

    fn workspace_resource_request(&self, request: Request) -> Result<Response> {
        match self.request(request.clone()) {
            Err(ClientError::Remote(error))
                if matches!(
                    error.code,
                    Some(
                        ErrorCode::PersistenceFailed
                            | ErrorCode::OutcomeUnknown
                            | ErrorCode::Timeout
                    )
                ) =>
            {
                self.request(request)
            }
            Err(ClientError::Transport(error))
                if matches!(
                    error.kind(),
                    io::ErrorKind::ConnectionReset
                        | io::ErrorKind::BrokenPipe
                        | io::ErrorKind::ConnectionAborted
                        | io::ErrorKind::NotConnected
                        | io::ErrorKind::TimedOut
                        | io::ErrorKind::UnexpectedEof
                ) =>
            {
                self.request(request)
            }
            result => result,
        }
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

    #[cfg(target_os = "linux")]
    pub fn daemon_peer_credentials(&self) -> Result<DaemonPeerCredentials> {
        let (stream, protocol_version, response) = self.send(Request::Ping)?;
        expect_ok(response, Response::Pong)?;
        let mut credentials = libc::ucred {
            pid: 0,
            uid: 0,
            gid: 0,
        };
        let mut length = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
        let result = unsafe {
            libc::getsockopt(
                stream.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_PEERCRED,
                std::ptr::from_mut(&mut credentials).cast(),
                &mut length,
            )
        };
        if result == -1 || length as usize != std::mem::size_of::<libc::ucred>() {
            return Err(ClientError::Transport(io::Error::last_os_error()));
        }
        let pid = u32::try_from(credentials.pid).map_err(|_| {
            ClientError::Transport(io::Error::new(
                io::ErrorKind::InvalidData,
                "daemon socket peer returned an invalid PID",
            ))
        })?;
        if pid == 0 {
            return Err(ClientError::Transport(io::Error::new(
                io::ErrorKind::InvalidData,
                "daemon socket peer returned an invalid PID",
            )));
        }
        Ok(DaemonPeerCredentials {
            pid,
            uid: credentials.uid,
            protocol_version,
        })
    }

    pub fn node_identity(&self) -> Result<String> {
        match self.request(Request::GetNodeIdentity)? {
            Response::NodeIdentity { node_id } => Ok(node_id),
            response => unexpected(response),
        }
    }

    pub fn open_federation_channel(&self) -> Result<FederationChannel> {
        let (stream, protocol_version, response) = self.send(Request::OpenFederationChannel)?;
        match response {
            Response::FederationChannel { node_id } => Ok(FederationChannel {
                stream,
                protocol_version,
                node_id,
            }),
            response => unexpected(response),
        }
    }

    pub fn rekey_node(&self, expected_node_id: impl Into<String>) -> Result<String> {
        match self.request(Request::RekeyNode {
            expected_node_id: expected_node_id.into(),
        })? {
            Response::NodeIdentity { node_id } => Ok(node_id),
            response => unexpected(response),
        }
    }

    pub fn add_node_registration(
        &self,
        alias: impl Into<String>,
        target: impl Into<String>,
        node_id: impl Into<String>,
    ) -> Result<NodeRegistrationSnapshot> {
        self.node_registration_response(Request::AddNodeRegistration {
            alias: alias.into(),
            target: target.into(),
            node_id: node_id.into(),
        })
    }

    pub fn node_registrations(&self) -> Result<Vec<NodeRegistrationSnapshot>> {
        match self.request(Request::ListNodeRegistrations)? {
            Response::NodeRegistrations { registrations } => Ok(registrations),
            response => unexpected(response),
        }
    }

    pub fn node_registration(
        &self,
        selector: impl Into<String>,
    ) -> Result<NodeRegistrationSnapshot> {
        self.node_registration_response(Request::GetNodeRegistration {
            selector: selector.into(),
        })
    }

    pub fn node_projection_health(
        &self,
        selector: impl Into<String>,
    ) -> Result<NodeProjectionHealth> {
        match self.request(Request::GetNodeProjectionHealth {
            selector: selector.into(),
        })? {
            Response::NodeProjectionHealth { health } => Ok(health),
            response => unexpected(response),
        }
    }

    pub fn force_node_projection_refresh(
        &self,
        selector: impl Into<String>,
    ) -> Result<NodeProjectionHealth> {
        match self.request(Request::ForceNodeProjectionRefresh {
            selector: selector.into(),
        })? {
            Response::NodeProjectionHealth { health } => Ok(health),
            response => unexpected(response),
        }
    }

    pub fn dismiss_node_projection_shell(
        &self,
        node_id: impl Into<String>,
        shell_id: impl Into<String>,
    ) -> Result<NodeProjectionHealth> {
        match self.request(Request::DismissNodeProjectionShell {
            node_id: node_id.into(),
            shell_id: shell_id.into(),
        })? {
            Response::NodeProjectionHealth { health } => Ok(health),
            response => unexpected(response),
        }
    }

    pub fn restore_dismissed_node_projection_shells(
        &self,
        node_id: impl Into<String>,
    ) -> Result<NodeProjectionHealth> {
        match self.request(Request::RestoreDismissedNodeProjectionShells {
            node_id: node_id.into(),
        })? {
            Response::NodeProjectionHealth { health } => Ok(health),
            response => unexpected(response),
        }
    }

    pub fn combined_node_snapshot(&self, selector: Option<String>) -> Result<CombinedNodeSnapshot> {
        match self.request(Request::GetCombinedNodeSnapshot { selector })? {
            Response::CombinedNodeSnapshot { snapshot } => Ok(snapshot),
            response => unexpected(response),
        }
    }

    pub fn create_global_workspace(
        &self,
        name: impl Into<String>,
    ) -> Result<crate::protocol::GlobalWorkspaceSnapshot> {
        match self.request(Request::CreateGlobalWorkspace { name: name.into() })? {
            Response::GlobalWorkspace { workspace } => Ok(workspace),
            response => unexpected(response),
        }
    }

    pub fn adopt_node_workspace(
        &self,
        identity: crate::protocol::QualifiedIdentity,
        expected_revision: u64,
    ) -> Result<crate::protocol::GlobalWorkspaceSnapshot> {
        match self.request(Request::AdoptNodeWorkspace {
            identity,
            expected_revision,
        })? {
            Response::GlobalWorkspace { workspace } => Ok(workspace),
            response => unexpected(response),
        }
    }

    pub fn link_node_workspace(
        &self,
        global_workspace_id: impl Into<String>,
        expected_global_revision: u64,
        identity: crate::protocol::QualifiedIdentity,
        expected_owner_revision: u64,
    ) -> Result<crate::protocol::GlobalWorkspaceSnapshot> {
        match self.request(Request::LinkNodeWorkspace {
            global_workspace_id: global_workspace_id.into(),
            expected_global_revision,
            identity,
            expected_owner_revision,
        })? {
            Response::GlobalWorkspace { workspace } => Ok(workspace),
            response => unexpected(response),
        }
    }

    pub fn rename_global_workspace(
        &self,
        workspace_id: impl Into<String>,
        expected_revision: u64,
        name: impl Into<String>,
    ) -> Result<crate::protocol::GlobalWorkspaceSnapshot> {
        match self.request(Request::RenameGlobalWorkspace {
            workspace_id: workspace_id.into(),
            expected_revision,
            name: name.into(),
        })? {
            Response::GlobalWorkspace { workspace } => Ok(workspace),
            response => unexpected(response),
        }
    }

    pub fn open_global_workspace(
        &self,
        workspace_id: impl Into<String>,
        expected_revision: u64,
    ) -> Result<crate::protocol::GlobalWorkspaceOperationResult> {
        match self.request(Request::OpenGlobalWorkspace {
            workspace_id: workspace_id.into(),
            expected_revision,
        })? {
            Response::GlobalWorkspaceOperation { result } => Ok(result),
            response => unexpected(response),
        }
    }

    pub fn close_global_workspace(
        &self,
        workspace_id: impl Into<String>,
        expected_revision: u64,
    ) -> Result<crate::protocol::GlobalWorkspaceOperationResult> {
        match self.request(Request::CloseGlobalWorkspace {
            workspace_id: workspace_id.into(),
            expected_revision,
        })? {
            Response::GlobalWorkspaceOperation { result } => Ok(result),
            response => unexpected(response),
        }
    }

    pub fn retry_global_workspace_close(
        &self,
        workspace_id: impl Into<String>,
    ) -> Result<crate::protocol::GlobalWorkspaceOperationResult> {
        match self.request(Request::RetryGlobalWorkspaceClose {
            workspace_id: workspace_id.into(),
        })? {
            Response::GlobalWorkspaceOperation { result } => Ok(result),
            response => unexpected(response),
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub fn create_global_workspace_shell(
        &self,
        operation_id: impl Into<String>,
        global_workspace_id: impl Into<String>,
        expected_global_revision: u64,
        node_id: impl Into<String>,
        owner_workspace_id: impl Into<String>,
        default_cwd: Option<PathBuf>,
        shell_id: impl Into<String>,
        shell: ShellSpec,
    ) -> Result<(crate::protocol::GlobalWorkspaceSnapshot, ShellSnapshot)> {
        match self.workspace_resource_request(Request::CreateGlobalWorkspaceShell {
            operation_id: operation_id.into(),
            global_workspace_id: global_workspace_id.into(),
            expected_global_revision,
            node_id: node_id.into(),
            owner_workspace_id: owner_workspace_id.into(),
            default_cwd,
            shell_id: shell_id.into(),
            shell,
        })? {
            Response::GlobalWorkspaceResource {
                workspace,
                resource: RoutedOperationResult::Shell { shell },
            } => Ok((workspace, shell)),
            response => unexpected(response),
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub fn create_global_workspace_with_shell(
        &self,
        operation_id: impl Into<String>,
        global_workspace_id: impl Into<String>,
        name: impl Into<String>,
        node_id: impl Into<String>,
        owner_workspace_id: impl Into<String>,
        default_cwd: PathBuf,
        shell_id: impl Into<String>,
        shell: ShellSpec,
    ) -> Result<(crate::protocol::GlobalWorkspaceSnapshot, ShellSnapshot)> {
        match self.workspace_resource_request(Request::CreateGlobalWorkspaceWithShell {
            operation_id: operation_id.into(),
            global_workspace_id: global_workspace_id.into(),
            name: name.into(),
            node_id: node_id.into(),
            owner_workspace_id: owner_workspace_id.into(),
            default_cwd,
            shell_id: shell_id.into(),
            shell,
        })? {
            Response::GlobalWorkspaceResource {
                workspace,
                resource: RoutedOperationResult::Shell { shell },
            } => Ok((workspace, shell)),
            response => unexpected(response),
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub fn create_global_workspace_launcher(
        &self,
        operation_id: impl Into<String>,
        global_workspace_id: impl Into<String>,
        expected_global_revision: u64,
        node_id: impl Into<String>,
        owner_workspace_id: impl Into<String>,
        default_cwd: Option<PathBuf>,
        launcher_id: impl Into<String>,
        spec: WorkspaceLauncherSpec,
    ) -> Result<(
        crate::protocol::GlobalWorkspaceSnapshot,
        WorkspaceLauncherSnapshot,
    )> {
        match self.workspace_resource_request(Request::CreateGlobalWorkspaceLauncher {
            operation_id: operation_id.into(),
            global_workspace_id: global_workspace_id.into(),
            expected_global_revision,
            node_id: node_id.into(),
            owner_workspace_id: owner_workspace_id.into(),
            default_cwd,
            launcher_id: launcher_id.into(),
            spec,
        })? {
            Response::GlobalWorkspaceResource {
                workspace,
                resource: RoutedOperationResult::Launcher { launcher },
            } => Ok((workspace, launcher)),
            response => unexpected(response),
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub fn create_global_workspace_agent_schedule(
        &self,
        operation_id: impl Into<String>,
        global_workspace_id: impl Into<String>,
        expected_global_revision: u64,
        node_id: impl Into<String>,
        owner_workspace_id: impl Into<String>,
        default_cwd: Option<PathBuf>,
        schedule_id: impl Into<String>,
        spec: AgentScheduleSpec,
    ) -> Result<(
        crate::protocol::GlobalWorkspaceSnapshot,
        AgentScheduleSnapshot,
    )> {
        match self.workspace_resource_request(Request::CreateGlobalWorkspaceAgentSchedule {
            operation_id: operation_id.into(),
            global_workspace_id: global_workspace_id.into(),
            expected_global_revision,
            node_id: node_id.into(),
            owner_workspace_id: owner_workspace_id.into(),
            default_cwd,
            schedule_id: schedule_id.into(),
            spec,
        })? {
            Response::GlobalWorkspaceResource {
                workspace,
                resource: RoutedOperationResult::AgentSchedule { schedule },
            } => Ok((workspace, schedule)),
            response => unexpected(response),
        }
    }

    pub fn route_node_operation(
        &self,
        node_id: impl Into<String>,
        operation: RoutedOperation,
    ) -> Result<RoutedOperationResult> {
        match self.request(Request::RouteNodeOperation {
            node_id: node_id.into(),
            operation,
        })? {
            Response::RoutedNodeOperation { result } => Ok(result),
            response => unexpected(response),
        }
    }

    pub fn host_service(
        &self,
        operation: crate::protocol::HostServiceOperation,
    ) -> Result<crate::protocol::HostServiceResult> {
        match self.request(Request::HostService { operation })? {
            Response::HostService { result } => Ok(result),
            response => unexpected(response),
        }
    }

    pub fn route_node_host_service(
        &self,
        node_id: impl Into<String>,
        operation: crate::protocol::HostServiceOperation,
    ) -> Result<crate::protocol::HostServiceResult> {
        match self.request(Request::RouteNodeHostService {
            node_id: node_id.into(),
            operation,
        })? {
            Response::HostService { result } => Ok(result),
            response => unexpected(response),
        }
    }

    pub fn rename_node_registration(
        &self,
        selector: impl Into<String>,
        alias: impl Into<String>,
        expected_revision: u64,
    ) -> Result<NodeRegistrationSnapshot> {
        self.node_registration_response(Request::RenameNodeRegistration {
            selector: selector.into(),
            alias: alias.into(),
            expected_revision,
        })
    }

    pub fn rename_local_node_alias(
        &self,
        alias: impl Into<String>,
        expected_revision: u64,
    ) -> Result<LocalNodeMetadata> {
        match self.request(Request::RenameLocalNodeAlias {
            alias: alias.into(),
            expected_revision,
        })? {
            Response::LocalNodeMetadata { node } => Ok(node),
            response => unexpected(response),
        }
    }

    pub fn retarget_node_registration(
        &self,
        selector: impl Into<String>,
        target: impl Into<String>,
        node_id: impl Into<String>,
        expected_revision: u64,
    ) -> Result<NodeRegistrationSnapshot> {
        self.node_registration_response(Request::RetargetNodeRegistration {
            selector: selector.into(),
            target: target.into(),
            node_id: node_id.into(),
            expected_revision,
        })
    }

    pub fn forget_node_registration(
        &self,
        selector: impl Into<String>,
    ) -> Result<NodeRegistrationSnapshot> {
        self.node_registration_response(Request::ForgetNodeRegistration {
            selector: selector.into(),
        })
    }

    pub fn begin_node_upgrade_maintenance(
        &self,
        selector: impl Into<String>,
        expected_revision: u64,
    ) -> Result<(NodeRegistrationSnapshot, String)> {
        match self.request(Request::BeginNodeUpgradeMaintenance {
            selector: selector.into(),
            expected_revision,
        })? {
            Response::NodeUpgradeMaintenance {
                registration,
                token,
            } => Ok((registration, token)),
            response => unexpected(response),
        }
    }

    pub fn finish_node_upgrade_maintenance(
        &self,
        node_id: impl Into<String>,
        token: impl Into<String>,
    ) -> Result<()> {
        expect_ok(
            self.request(Request::FinishNodeUpgradeMaintenance {
                node_id: node_id.into(),
                token: token.into(),
            })?,
            Response::Ok,
        )
    }

    pub fn renew_node_upgrade_maintenance(
        &self,
        node_id: impl Into<String>,
        token: impl Into<String>,
    ) -> Result<()> {
        expect_ok(
            self.request(Request::RenewNodeUpgradeMaintenance {
                node_id: node_id.into(),
                token: token.into(),
            })?,
            Response::Ok,
        )
    }

    fn node_registration_response(&self, request: Request) -> Result<NodeRegistrationSnapshot> {
        match self.request(request)? {
            Response::NodeRegistration { registration } => Ok(registration),
            response => unexpected(response),
        }
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
        if !self.supports(protocol::ProtocolFeature::TimedScheduling)? {
            let mut compatibility = notifications.clone();
            compatibility.max_scheduled_execution_concurrency = 4;
            if self.supports(protocol::ProtocolFeature::ScheduledExecutions)? {
                self.restart_request(Request::RestartWithNotificationConfig {
                    notifications: compatibility,
                    environment: Some(current_environment()),
                })?;
            } else if self.supports(protocol::ProtocolFeature::RestartNotificationConfig)? {
                self.restart_request(Request::RestartWithNotificationConfig {
                    notifications: compatibility,
                    environment: None,
                })?;
            } else {
                self.restart_request(Request::Restart)?;
            }
            self.probe_latest()?;
            if !self.supports(protocol::ProtocolFeature::TimedScheduling)? {
                return Err(unsupported_version(
                    "replacement daemon does not support timed scheduling settings",
                ));
            }
        }
        if (notifications.scheduled_dispatch_failed || notifications.scheduled_interrupted)
            && !self.supports(protocol::ProtocolFeature::ScheduledExecutionObservation)?
        {
            let mut compatibility = notifications.clone();
            compatibility.scheduled_dispatch_failed = false;
            compatibility.scheduled_interrupted = false;
            self.restart_request(Request::RestartWithNotificationConfig {
                notifications: compatibility,
                environment: Some(current_environment()),
            })?;
            self.probe_latest()?;
            if !self.supports(protocol::ProtocolFeature::ScheduledExecutionObservation)? {
                return Err(unsupported_version(
                    "replacement daemon does not support scheduled execution notification settings",
                ));
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

    pub fn update_agent_schedule(
        &self,
        schedule_id: impl Into<String>,
        expected_revision: u64,
        update: AgentScheduleUpdate,
    ) -> Result<AgentScheduleSnapshot> {
        match self.request(Request::UpdateAgentSchedule {
            schedule_id: schedule_id.into(),
            expected_revision,
            update,
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
            Response::ScheduledExecution { execution, .. } => Ok(execution),
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
            limit: None,
        })? {
            Response::ScheduledExecutions { executions, .. } => Ok(executions),
            other => unexpected(other),
        }
    }

    pub fn scheduled_execution_page(
        &self,
        workspace_id: Option<String>,
        schedule_id: Option<String>,
        limit: u16,
    ) -> Result<ScheduledExecutionList> {
        let supports_bounded =
            self.supports(protocol::ProtocolFeature::ScheduledExecutionObservation)?;
        match self.request(Request::ListScheduledExecutions {
            workspace_id,
            schedule_id,
            limit: Some(limit),
        })? {
            Response::ScheduledExecutions {
                mut executions,
                limit: response_limit,
                truncated,
                schedules,
                schedule_limit,
                schedules_truncated,
            } => {
                if supports_bounded {
                    Ok(ScheduledExecutionList {
                        executions,
                        limit: response_limit,
                        truncated,
                        schedules,
                        schedule_limit,
                        schedules_truncated,
                    })
                } else {
                    let limit = limit.clamp(1, protocol::MAX_SCHEDULED_EXECUTION_LIST_LIMIT);
                    let truncated = executions.len() > usize::from(limit);
                    executions.truncate(usize::from(limit));
                    Ok(ScheduledExecutionList {
                        executions,
                        limit,
                        truncated,
                        schedules,
                        schedule_limit: 0,
                        schedules_truncated: false,
                    })
                }
            }
            other => unexpected(other),
        }
    }

    pub fn wait_scheduled_execution(
        &self,
        execution_id: impl Into<String>,
        after_revision: u64,
        wait_ms: u32,
    ) -> Result<ScheduledExecutionWait> {
        if !self.supports(protocol::ProtocolFeature::ScheduledExecutionObservation)? {
            return Err(unsupported_version(
                "daemon does not support scheduled execution wait",
            ));
        }
        match self.request(Request::WaitScheduledExecution {
            execution_id: execution_id.into(),
            after_revision,
            wait_ms,
        })? {
            Response::ScheduledExecutionWait { execution, changed } => {
                Ok(ScheduledExecutionWait { execution, changed })
            }
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
            Response::ScheduledExecution { execution, .. } => Ok(execution),
            other => unexpected(other),
        }
    }

    pub fn inspect_scheduled_execution(
        &self,
        execution_id: impl Into<String>,
    ) -> Result<ScheduledExecutionInspection> {
        match self.request(Request::GetScheduledExecution {
            execution_id: execution_id.into(),
        })? {
            Response::ScheduledExecution {
                execution,
                next_occurrence,
            } => Ok(ScheduledExecutionInspection {
                execution,
                next_occurrence,
            }),
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
            Response::ScheduledExecution { execution, .. } => Ok(execution),
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
            Response::ScheduledExecution { execution, .. } => Ok(execution),
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
        self.attach_with_restart(shell_id.into(), takeover, false, None, profile, None)
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
            None,
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
        self.attach_with_restart(shell_id.into(), takeover, true, None, profile, None)
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
            None,
            profile,
            Some(current_environment()),
        )
    }

    pub fn attach_exact_run_with_client_environment(
        &self,
        shell_id: impl Into<String>,
        expected_run_id: impl Into<String>,
        takeover: bool,
        profile: TerminalProfile,
    ) -> Result<Attachment> {
        self.attach_with_restart(
            shell_id.into(),
            takeover,
            false,
            Some(expected_run_id.into()),
            profile,
            Some(current_environment()),
        )
    }

    pub fn attach_node(
        &self,
        identity: protocol::QualifiedIdentity,
        takeover: bool,
        restart_exited: bool,
        expected_run_id: Option<String>,
        profile: TerminalProfile,
    ) -> Result<Attachment> {
        let (stream, protocol_version, response) = self.send(Request::AttachNode {
            identity,
            takeover,
            restart_exited,
            expected_run_id,
            profile,
        })?;
        attachment_from_response(stream, protocol_version, response)
    }

    pub fn resume_agent_session(
        &self,
        node_id: Option<&str>,
        session_id: impl Into<String>,
        profile: TerminalProfile,
    ) -> Result<Attachment> {
        let session_id = session_id.into();
        let request = match node_id {
            Some(node_id) => Request::ResumeNodeAgentSession {
                node_id: node_id.to_owned(),
                session_id,
                profile,
            },
            None => Request::ResumeAgentSession {
                session_id,
                profile,
            },
        };
        let (stream, protocol_version, response) = self.send(request)?;
        attachment_from_response(stream, protocol_version, response)
    }

    fn attach_with_restart(
        &self,
        shell_id: String,
        takeover: bool,
        restart_exited: bool,
        expected_run_id: Option<String>,
        profile: TerminalProfile,
        environment: Option<UnixEnvironment>,
    ) -> Result<Attachment> {
        let (stream, protocol_version, response) = self.send(Request::Attach {
            shell_id,
            takeover,
            restart_exited,
            expected_run_id,
            profile,
            environment,
            owner_environment: false,
        })?;
        attachment_from_response(stream, protocol_version, response)
    }
}

fn attachment_from_response(
    stream: UnixStream,
    protocol_version: u32,
    response: Response,
) -> Result<Attachment> {
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

    #[test]
    fn workspace_resource_retries_the_identical_request_after_response_loss() {
        let directory = env::temp_dir().join(format!("boomux-client-workspace-{}", Uuid::new_v4()));
        fs::create_dir_all(&directory).unwrap();
        let socket = directory.join("daemon.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let workspace_id = Uuid::from_u128(1).to_string();
        let shell_id = Uuid::from_u128(2).to_string();
        let server_workspace_id = workspace_id.clone();
        let server_shell_id = shell_id.clone();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let first: Envelope<Request> = protocol::read_message(&mut stream).unwrap();
            drop(stream);

            let (mut stream, _) = listener.accept().unwrap();
            let second: Envelope<Request> = protocol::read_message(&mut stream).unwrap();
            assert_eq!(second, first);
            protocol::write_message(
                &mut stream,
                &Envelope::with_version(
                    protocol::PROTOCOL_VERSION,
                    Response::GlobalWorkspaceResource {
                        workspace: protocol::GlobalWorkspaceSnapshot {
                            id: server_workspace_id.clone(),
                            revision: 2,
                            name: "retry".into(),
                            closing: false,
                            placements: Vec::new(),
                        },
                        resource: RoutedOperationResult::Shell {
                            shell: ShellSnapshot {
                                id: server_shell_id,
                                revision: 1,
                                workspace_id: server_workspace_id,
                                name: "retry".into(),
                                cwd: "/tmp".into(),
                                command: Vec::new(),
                                owner: protocol::ShellOwner::User,
                                status: protocol::ShellStatus::Pending,
                                run: None,
                                recovered_agent_id: None,
                                foreground_process: None,
                            },
                        },
                    },
                ),
            )
            .unwrap();
        });
        let client = Client::from_socket_path(socket);
        let (workspace, shell) = client
            .create_global_workspace_shell(
                Uuid::from_u128(3).to_string(),
                &workspace_id,
                1,
                Uuid::from_u128(4).to_string(),
                Uuid::from_u128(5).to_string(),
                Some("/tmp".into()),
                &shell_id,
                ShellSpec {
                    name: "retry".into(),
                    cwd: "/tmp".into(),
                    command: Vec::new(),
                },
            )
            .unwrap();
        assert_eq!(workspace.id, workspace_id);
        assert_eq!(shell.id, shell_id);
        server.join().unwrap();
        fs::remove_dir_all(directory).unwrap();
    }

    fn assert_two_stage_restart_from(old_version: u32) {
        let directory = env::temp_dir().join(format!("boomux-client-restart-{}", Uuid::new_v4()));
        fs::create_dir_all(&directory).unwrap();
        let socket = directory.join("daemon.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let settings = NotificationDeliveryConfig {
            desktop_enabled: true,
            sound_enabled: false,
            blocked: true,
            completed: false,
            blocked_sound: "blocked".into(),
            completed_sound: "completed".into(),
            resume_agents: false,
            persist_terminal_history: true,
            max_scheduled_execution_concurrency: 1,
            ..Default::default()
        };
        let expected_settings = settings.clone();
        let expected_environment = current_environment();
        let server = thread::spawn(move || {
            let mut upgraded = false;
            let mut full_restart_seen = false;
            loop {
                let (mut stream, _) = listener.accept().unwrap();
                let request: Envelope<Request> = protocol::read_message(&mut stream).unwrap();
                if !upgraded {
                    match request.message {
                        Request::Ping if request.version > old_version => {
                            protocol::write_message(
                                &mut stream,
                                &Envelope::with_version(
                                    old_version,
                                    Response::Error {
                                        message: "newer protocol unsupported".into(),
                                        code: Some(ErrorCode::UnsupportedVersion),
                                    },
                                ),
                            )
                            .unwrap();
                        }
                        Request::Ping => {
                            assert_eq!(request.version, old_version);
                            protocol::write_message(
                                &mut stream,
                                &Envelope::with_version(old_version, Response::Pong),
                            )
                            .unwrap();
                        }
                        Request::Restart if old_version == 16 => {
                            upgraded = true;
                            protocol::write_message(
                                &mut stream,
                                &Envelope::with_version(old_version, Response::Ok),
                            )
                            .unwrap();
                        }
                        Request::RestartWithNotificationConfig {
                            notifications,
                            environment,
                        } if old_version >= 17 => {
                            assert_eq!(notifications.max_scheduled_execution_concurrency, 4);
                            assert_eq!(environment.is_some(), old_version >= 23);
                            upgraded = true;
                            protocol::write_message(
                                &mut stream,
                                &Envelope::with_version(old_version, Response::Ok),
                            )
                            .unwrap();
                        }
                        request => panic!("unexpected old-daemon request: {request:?}"),
                    }
                } else {
                    match request.message {
                        Request::Ping => {
                            protocol::write_message(
                                &mut stream,
                                &Envelope::with_version(request.version, Response::Pong),
                            )
                            .unwrap();
                            if full_restart_seen {
                                break;
                            }
                        }
                        Request::RestartWithNotificationConfig {
                            notifications,
                            environment: Some(environment),
                        } => {
                            assert_eq!(request.version, protocol::PROTOCOL_VERSION);
                            assert_eq!(notifications, expected_settings);
                            assert_eq!(environment, expected_environment);
                            full_restart_seen = true;
                            protocol::write_message(
                                &mut stream,
                                &Envelope::with_version(protocol::PROTOCOL_VERSION, Response::Ok),
                            )
                            .unwrap();
                        }
                        request => panic!("unexpected upgraded-daemon request: {request:?}"),
                    }
                }
            }
        });
        let client = Client::from_socket_path(socket);
        client
            .restart_with_notification_config(settings.clone())
            .unwrap();
        server.join().unwrap();
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn mixed_version_restart_applies_full_settings_after_compatibility_handoff() {
        for version in [16, 22, 23] {
            assert_two_stage_restart_from(version);
        }
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
    fn update_agent_schedule_sends_revisioned_private_definition() {
        let directory = env::temp_dir().join(format!("boomux-client-update-{}", Uuid::new_v4()));
        fs::create_dir_all(&directory).unwrap();
        let socket = directory.join("daemon.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let request: Envelope<Request> = protocol::read_message(&mut stream).unwrap();
            assert_eq!(request.version, protocol::PROTOCOL_VERSION);
            let Request::UpdateAgentSchedule {
                schedule_id,
                expected_revision,
                update,
            } = request.message
            else {
                panic!("expected schedule update");
            };
            assert_eq!(schedule_id, "schedule-1");
            assert_eq!(expected_revision, 4);
            assert_eq!(update.prompt, "private prompt");
            protocol::write_message(
                &mut stream,
                &Envelope::with_version(
                    protocol::PROTOCOL_VERSION,
                    Response::AgentSchedule {
                        schedule: AgentScheduleSnapshot {
                            id: schedule_id,
                            workspace_id: "workspace-1".into(),
                            name: update.name,
                            cwd: env::temp_dir(),
                            integration: "opencode".into(),
                            session: protocol::AgentScheduleSession::Fresh,
                            trigger: update.trigger,
                            state: protocol::AgentScheduleState::Paused,
                            overlap_policy: protocol::AgentScheduleOverlapPolicy::Skip,
                            revision: 5,
                            prompt_revision: 5,
                            trigger_revision: 5,
                            created_at_ms: 1,
                            updated_at_ms: 2,
                            evaluation_frontier_ms: 2,
                            execution_shell_id: None,
                            next_occurrence: None,
                        },
                    },
                ),
            )
            .unwrap();
        });
        let client = Client::from_socket_path(socket);

        let updated = client
            .update_agent_schedule(
                "schedule-1",
                4,
                AgentScheduleUpdate {
                    name: "updated".into(),
                    prompt: "private prompt".into(),
                    trigger: protocol::AgentScheduleTrigger {
                        cron: "0 3 * * *".into(),
                        timezone: "UTC".into(),
                    },
                },
            )
            .unwrap();

        assert_eq!(updated.revision, 5);
        assert_eq!(updated.name, "updated");
        server.join().unwrap();
        fs::remove_dir_all(directory).unwrap();
    }

    fn test_scheduled_execution(id: &str, requested_at_ms: u64) -> ScheduledExecutionSnapshot {
        ScheduledExecutionSnapshot {
            id: id.into(),
            workspace_id: "workspace".into(),
            schedule_id: "schedule".into(),
            revision: 1,
            state: protocol::ScheduledExecutionState::DispatchFailed,
            dispatch_kind: protocol::ScheduledExecutionDispatchKind::Manual,
            dispatch_key: format!("key-{id}"),
            schedule_revision: 1,
            prompt_revision: 1,
            trigger_revision: 1,
            requested_at_ms,
            scheduled_at_ms: None,
            coalesced_through_ms: None,
            started_at_ms: None,
            ended_at_ms: Some(requested_at_ms),
            cwd: env::temp_dir(),
            integration: "opencode".into(),
            session: protocol::AgentScheduleSession::Fresh,
            reason: Some(protocol::ScheduledExecutionReason::RunnerStartFailed),
            outcome: None,
            shell_id: None,
            run_id: None,
            agent_id: None,
            external_session_id: None,
        }
    }

    #[test]
    fn current_client_bounds_unbounded_old_daemon_execution_lists() {
        for old_version in [23, 24] {
            let directory =
                env::temp_dir().join(format!("boomux-old-list-{old_version}-{}", Uuid::new_v4()));
            fs::create_dir_all(&directory).unwrap();
            let socket = directory.join("daemon.sock");
            let listener = UnixListener::bind(&socket).unwrap();
            let server = thread::spawn(move || {
                for attempted in ((old_version + 1)..=protocol::PROTOCOL_VERSION).rev() {
                    reject_protocol(&listener, attempted, old_version);
                }
                let (mut stream, _) = listener.accept().unwrap();
                let request: Envelope<Request> = protocol::read_message(&mut stream).unwrap();
                assert_eq!(request.version, old_version);
                assert!(matches!(request.message, Request::Ping));
                protocol::write_message(
                    &mut stream,
                    &Envelope::with_version(old_version, Response::Pong),
                )
                .unwrap();

                let (mut stream, _) = listener.accept().unwrap();
                let request: Envelope<Request> = protocol::read_message(&mut stream).unwrap();
                assert_eq!(request.version, old_version);
                assert!(matches!(
                    request.message,
                    Request::ListScheduledExecutions { limit: Some(2), .. }
                ));
                protocol::write_message(
                    &mut stream,
                    &Envelope::with_version(
                        old_version,
                        Response::ScheduledExecutions {
                            executions: vec![
                                test_scheduled_execution("newest", 3),
                                test_scheduled_execution("middle", 2),
                                test_scheduled_execution("oldest", 1),
                            ],
                            limit: 0,
                            truncated: false,
                            schedules: Vec::new(),
                            schedule_limit: 0,
                            schedules_truncated: false,
                        },
                    ),
                )
                .unwrap();
            });
            let page = Client::from_socket_path(socket)
                .scheduled_execution_page(None, None, 2)
                .unwrap();
            assert_eq!(page.limit, 2);
            assert!(page.truncated);
            assert_eq!(
                page.executions
                    .iter()
                    .map(|execution| execution.id.as_str())
                    .collect::<Vec<_>>(),
                ["newest", "middle"]
            );
            server.join().unwrap();
            fs::remove_dir_all(directory).unwrap();
        }
    }

    #[test]
    fn direct_client_negotiates_before_sending_agent_wait() {
        let directory = env::temp_dir().join(format!("boomux-client-{}", Uuid::new_v4()));
        fs::create_dir_all(&directory).unwrap();
        let socket = directory.join("daemon.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let server = thread::spawn(move || {
            reject_protocol(&listener, protocol::PROTOCOL_VERSION, 41);
            reject_protocol(&listener, 41, 40);
            reject_protocol(&listener, 40, 39);
            reject_protocol(&listener, 39, 38);
            reject_protocol(&listener, 38, 37);
            reject_protocol(&listener, 37, 36);
            reject_protocol(&listener, 36, 35);
            reject_protocol(&listener, 35, 34);
            reject_protocol(&listener, 34, 33);
            reject_protocol(&listener, 33, 32);
            reject_protocol(&listener, 32, 31);
            reject_protocol(&listener, 31, 30);
            reject_protocol(&listener, 30, 29);
            reject_protocol(&listener, 29, 28);
            reject_protocol(&listener, 28, 27);
            reject_protocol(&listener, 27, 26);
            reject_protocol(&listener, 26, 25);
            reject_protocol(&listener, 25, 24);
            reject_protocol(&listener, 24, 23);
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
    fn node_identity_requires_protocol_twenty_eight() {
        let directory = env::temp_dir().join(format!("boomux-client-{}", Uuid::new_v4()));
        fs::create_dir_all(&directory).unwrap();
        let socket = directory.join("daemon.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let request: Envelope<Request> = protocol::read_message(&mut stream).unwrap();
            assert_eq!(request.version, protocol::PROTOCOL_VERSION);
            assert_eq!(request.message, Request::GetNodeIdentity);
            protocol::write_message(
                &mut stream,
                &Envelope::with_version(
                    27,
                    Response::Error {
                        message: "Node identity requires protocol 28".into(),
                        code: Some(ErrorCode::UnsupportedVersion),
                    },
                ),
            )
            .unwrap();

            reject_protocol(&listener, protocol::PROTOCOL_VERSION, 41);
            reject_protocol(&listener, 41, 40);
            reject_protocol(&listener, 40, 39);
            reject_protocol(&listener, 39, 38);
            reject_protocol(&listener, 38, 37);
            reject_protocol(&listener, 37, 36);
            reject_protocol(&listener, 36, 35);
            reject_protocol(&listener, 35, 34);
            reject_protocol(&listener, 34, 33);
            reject_protocol(&listener, 33, 32);
            reject_protocol(&listener, 32, 31);
            reject_protocol(&listener, 31, 30);
            reject_protocol(&listener, 30, 29);
            reject_protocol(&listener, 29, 27);

            let (mut stream, _) = listener.accept().unwrap();
            let request: Envelope<Request> = protocol::read_message(&mut stream).unwrap();
            assert_eq!(request.version, 28);
            assert_eq!(request.message, Request::Ping);
            protocol::write_message(
                &mut stream,
                &Envelope::with_version(
                    27,
                    Response::Error {
                        message: "protocol 28 unsupported".into(),
                        code: Some(ErrorCode::UnsupportedVersion),
                    },
                ),
            )
            .unwrap();

            let (mut stream, _) = listener.accept().unwrap();
            let request: Envelope<Request> = protocol::read_message(&mut stream).unwrap();
            assert_eq!(request.version, 27);
            assert_eq!(request.message, Request::Ping);
            protocol::write_message(&mut stream, &Envelope::with_version(27, Response::Pong))
                .unwrap();
        });

        let client = Client::from_socket_path(socket);
        assert!(matches!(
            client.node_identity(),
            Err(ClientError::Protocol(ProtocolError::UnsupportedVersion(_)))
        ));
        server.join().unwrap();
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn federation_channel_requires_protocol_twenty_nine() {
        let directory = env::temp_dir().join(format!("boomux-client-{}", Uuid::new_v4()));
        fs::create_dir_all(&directory).unwrap();
        let socket = directory.join("daemon.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let request: Envelope<Request> = protocol::read_message(&mut stream).unwrap();
            assert_eq!(request.version, protocol::PROTOCOL_VERSION);
            assert_eq!(request.message, Request::OpenFederationChannel);
            protocol::write_message(
                &mut stream,
                &Envelope::with_version(
                    28,
                    Response::Error {
                        message: "federation channel requires protocol 29".into(),
                        code: Some(ErrorCode::UnsupportedVersion),
                    },
                ),
            )
            .unwrap();

            reject_protocol(&listener, protocol::PROTOCOL_VERSION, 41);
            reject_protocol(&listener, 41, 40);
            reject_protocol(&listener, 40, 39);
            reject_protocol(&listener, 39, 38);
            reject_protocol(&listener, 38, 37);
            reject_protocol(&listener, 37, 36);
            reject_protocol(&listener, 36, 35);
            reject_protocol(&listener, 35, 34);
            reject_protocol(&listener, 34, 33);
            reject_protocol(&listener, 33, 32);
            reject_protocol(&listener, 32, 31);
            reject_protocol(&listener, 31, 30);
            reject_protocol(&listener, 30, 29);
            reject_protocol(&listener, 29, 28);
            let (mut stream, _) = listener.accept().unwrap();
            let request: Envelope<Request> = protocol::read_message(&mut stream).unwrap();
            assert_eq!(request.version, 28);
            assert_eq!(request.message, Request::Ping);
            protocol::write_message(&mut stream, &Envelope::with_version(28, Response::Pong))
                .unwrap();
        });

        let client = Client::from_socket_path(socket);
        assert!(matches!(
            client.open_federation_channel(),
            Err(ClientError::Protocol(ProtocolError::UnsupportedVersion(_)))
        ));
        server.join().unwrap();
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn node_registration_requires_protocol_thirty_one() {
        let directory = env::temp_dir().join(format!("boomux-client-{}", Uuid::new_v4()));
        fs::create_dir_all(&directory).unwrap();
        let socket = directory.join("daemon.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let request: Envelope<Request> = protocol::read_message(&mut stream).unwrap();
            assert_eq!(request.version, protocol::PROTOCOL_VERSION);
            assert_eq!(request.message, Request::ListNodeRegistrations);
            protocol::write_message(
                &mut stream,
                &Envelope::with_version(
                    30,
                    Response::Error {
                        message: "Node registration management requires protocol 31".into(),
                        code: Some(ErrorCode::UnsupportedVersion),
                    },
                ),
            )
            .unwrap();
            reject_protocol(&listener, protocol::PROTOCOL_VERSION, 41);
            reject_protocol(&listener, 41, 40);
            reject_protocol(&listener, 40, 39);
            reject_protocol(&listener, 39, 38);
            reject_protocol(&listener, 38, 37);
            reject_protocol(&listener, 37, 36);
            reject_protocol(&listener, 36, 35);
            reject_protocol(&listener, 35, 34);
            reject_protocol(&listener, 34, 33);
            reject_protocol(&listener, 33, 32);
            reject_protocol(&listener, 32, 31);
            reject_protocol(&listener, 31, 30);
            let (mut stream, _) = listener.accept().unwrap();
            let request: Envelope<Request> = protocol::read_message(&mut stream).unwrap();
            assert_eq!(request.version, 30);
            assert_eq!(request.message, Request::Ping);
            protocol::write_message(&mut stream, &Envelope::with_version(30, Response::Pong))
                .unwrap();
        });

        let client = Client::from_socket_path(socket);
        assert!(matches!(
            client.node_registrations(),
            Err(ClientError::Protocol(ProtocolError::UnsupportedVersion(_)))
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
            assert_eq!(request.version, protocol::PROTOCOL_VERSION);
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
                    protocol::PROTOCOL_VERSION,
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
            reject_protocol(&listener, protocol::PROTOCOL_VERSION, 41);
            reject_protocol(&listener, 41, 40);
            reject_protocol(&listener, 40, 39);
            reject_protocol(&listener, 39, 38);
            reject_protocol(&listener, 38, 37);
            reject_protocol(&listener, 37, 36);
            reject_protocol(&listener, 36, 35);
            reject_protocol(&listener, 35, 34);
            reject_protocol(&listener, 34, 33);
            reject_protocol(&listener, 33, 32);
            reject_protocol(&listener, 32, 31);
            reject_protocol(&listener, 31, 30);
            reject_protocol(&listener, 30, 29);
            reject_protocol(&listener, 29, 28);
            reject_protocol(&listener, 28, 27);
            reject_protocol(&listener, 27, 26);
            reject_protocol(&listener, 26, 25);
            reject_protocol(&listener, 25, 24);
            reject_protocol(&listener, 24, 23);
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
            reject_protocol(&listener, protocol::PROTOCOL_VERSION, 41);
            reject_protocol(&listener, 41, 40);
            reject_protocol(&listener, 40, 39);
            reject_protocol(&listener, 39, 38);
            reject_protocol(&listener, 38, 37);
            reject_protocol(&listener, 37, 36);
            reject_protocol(&listener, 36, 35);
            reject_protocol(&listener, 35, 34);
            reject_protocol(&listener, 34, 33);
            reject_protocol(&listener, 33, 32);
            reject_protocol(&listener, 32, 31);
            reject_protocol(&listener, 31, 30);
            reject_protocol(&listener, 30, 29);
            reject_protocol(&listener, 29, 28);
            reject_protocol(&listener, 28, 27);
            reject_protocol(&listener, 27, 26);
            reject_protocol(&listener, 26, 25);
            reject_protocol(&listener, 25, 24);
            reject_protocol(&listener, 24, 23);
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

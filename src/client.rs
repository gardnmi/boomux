use std::env;
use std::error::Error;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read};
use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use crate::protocol::{
    self, AgentInstanceSnapshot, AgentRegistrationSpec, AgentReport, AgentState,
    ClaudeRemoteControlBindingSnapshot, CombinedNodeSnapshot, DaemonEvent, Envelope, ErrorCode,
    EventCursor, FocusedTerminalSnapshot, NodeProjectionHealth, NodeRegistrationSnapshot,
    NotificationDeliveryConfig, OpenCodeSessionClaimSnapshot, OpenCodeSharedRuntimeSnapshot,
    Request, Response, RoutedOperation, RoutedOperationResult, ShellSnapshot, ShellSpec,
    ShellStatus, Snapshot, TerminalPreview, TerminalPreviewLine, TerminalPreviewSpan,
    TerminalProfile, UnixEnvironment, UnixEnvironmentVariable, WorkspaceLauncherSnapshot,
    WorkspaceLauncherSpec, WorkspaceSnapshot,
};

const CONNECT_ATTEMPTS: usize = 40;
const CONNECT_DELAY: Duration = Duration::from_millis(25);
const SHUTDOWN_ATTEMPTS: usize = 200;
#[cfg(target_os = "linux")]
const MAX_PROC_UNIX_BYTES: u64 = 4 * 1024 * 1024;
#[cfg(target_os = "linux")]
const MAX_PROC_ENTRIES: usize = 65_536;
#[cfg(target_os = "linux")]
const MAX_PROC_FD_ENTRIES: usize = 1_048_576;
#[cfg(target_os = "linux")]
const DAEMON_IDENTITY_TIMEOUT: Duration = Duration::from_secs(2);

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
    pub profile: Option<TerminalProfile>,
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

pub fn connect_if_running() -> Result<Option<Client>> {
    let client = connect_client()?;
    match client.ping() {
        Ok(()) => Ok(Some(client)),
        Err(error) if daemon_unreachable(&error) => Ok(None),
        Err(error) => Err(error),
    }
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
        self.send_with_version_timeout(request, version, None)
    }

    fn send_with_version_timeout(
        &self,
        request: Request,
        version: u32,
        timeout: Option<Duration>,
    ) -> Result<(UnixStream, Response)> {
        let mut stream = UnixStream::connect(&self.socket_path).map_err(ClientError::Transport)?;
        stream
            .set_read_timeout(timeout)
            .map_err(ClientError::Transport)?;
        stream
            .set_write_timeout(timeout)
            .map_err(ClientError::Transport)?;
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

    #[cfg(target_os = "linux")]
    pub fn daemon_process_credentials(&self) -> Result<DaemonPeerCredentials> {
        let socket_before = fs::metadata(&self.socket_path).map_err(ClientError::Transport)?;
        let peer = self.daemon_peer_credentials()?;
        let pid =
            daemon_listener_holder(&self.socket_path, peer.uid).map_err(ClientError::Transport)?;
        let socket_after = fs::metadata(&self.socket_path).map_err(ClientError::Transport)?;
        if socket_before.dev() != socket_after.dev() || socket_before.ino() != socket_after.ino() {
            return Err(ClientError::Transport(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "daemon socket changed during process identification",
            )));
        }
        Ok(DaemonPeerCredentials { pid, ..peer })
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
    pub fn set_global_workspace_default_cwd(
        &self,
        operation_id: impl Into<String>,
        global_workspace_id: impl Into<String>,
        expected_global_revision: u64,
        node_id: impl Into<String>,
        owner_workspace_id: impl Into<String>,
        expected_owner_revision: u64,
        default_cwd: PathBuf,
    ) -> Result<crate::protocol::WorkspaceDefaultCwdResult> {
        match self.workspace_resource_request(Request::SetGlobalWorkspaceDefaultCwd {
            operation_id: operation_id.into(),
            global_workspace_id: global_workspace_id.into(),
            expected_global_revision,
            node_id: node_id.into(),
            owner_workspace_id: owner_workspace_id.into(),
            expected_owner_revision,
            default_cwd,
        })? {
            Response::WorkspaceDefaultCwd { result } => Ok(result),
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

    pub fn set_agent_session_display_name(
        &self,
        operation_id: impl Into<String>,
        session_id: impl Into<String>,
        expected_workspace_revision: u64,
        display_name: Option<String>,
    ) -> Result<crate::protocol::AgentSessionDisplayNameResult> {
        match self.request(Request::SetAgentSessionDisplayName {
            operation_id: operation_id.into(),
            session_id: session_id.into(),
            expected_workspace_revision,
            display_name,
        })? {
            Response::AgentSessionDisplayName { outcome } => Ok(outcome),
            response => unexpected(response),
        }
    }

    pub fn hide_agent_session(
        &self,
        operation_id: impl Into<String>,
        session_id: impl Into<String>,
        workspace_id: impl Into<String>,
        expected_workspace_revision: u64,
    ) -> Result<crate::protocol::AgentSessionHideResult> {
        match self.request(Request::HideAgentSession {
            operation_id: operation_id.into(),
            session_id: session_id.into(),
            workspace_id: workspace_id.into(),
            expected_workspace_revision,
        })? {
            Response::AgentSessionHidden { outcome } => Ok(outcome),
            response => unexpected(response),
        }
    }

    pub fn host_service(
        &self,
        operation: crate::protocol::HostServiceOperation,
    ) -> Result<crate::protocol::HostServiceResult> {
        match self.request(Request::HostService { operation })? {
            Response::HostService { result } => Ok(normalize_host_service_result(result)),
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
            Response::HostService { result } => Ok(normalize_host_service_result(result)),
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

    pub fn finish_node_uninstall_maintenance(
        &self,
        node_id: impl Into<String>,
        token: impl Into<String>,
    ) -> Result<NodeRegistrationSnapshot> {
        self.node_registration_response(Request::FinishNodeUninstallMaintenance {
            node_id: node_id.into(),
            token: token.into(),
        })
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
        self.wait_for_shutdown()
    }

    pub fn shutdown_if_node_identity(&self, expected_node_id: impl Into<String>) -> Result<()> {
        expect_ok(
            self.request(Request::ShutdownIfNodeIdentity {
                expected_node_id: expected_node_id.into(),
            })?,
            Response::Ok,
        )?;
        self.wait_for_shutdown()
    }

    fn wait_for_shutdown(&self) -> Result<()> {
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
        if self.protocol_version()? < protocol::PROTOCOL_VERSION {
            if self.supports(protocol::ProtocolFeature::RestartNotificationConfig)? {
                self.restart_request(Request::RestartWithNotificationConfig {
                    notifications: notifications.clone(),
                    environment: None,
                })?;
            } else {
                self.restart_request(Request::Restart)?;
            }
            self.probe_latest()?;
        }
        self.restart_request(Request::RestartWithNotificationConfig {
            notifications,
            environment: None,
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

    pub fn acquire_kiro_launch_holder(
        &self,
        pid: u32,
        shell_id: impl Into<String>,
        run_id: impl Into<String>,
    ) -> Result<String> {
        match self.request(Request::AcquireKiroLaunchHolder {
            pid,
            shell_id: shell_id.into(),
            run_id: run_id.into(),
        })? {
            Response::KiroLaunchHolder { holder_id } => Ok(holder_id),
            other => unexpected(other),
        }
    }

    pub fn report_kiro_hook(
        &self,
        holder_id: impl Into<String>,
        session_id: impl Into<String>,
        mut report: AgentReport,
    ) -> Result<AgentInstanceSnapshot> {
        let holder_id = holder_id.into();
        let session_id = session_id.into();
        let response = match self.request(Request::ReportKiroHook {
            holder_id: holder_id.clone(),
            session_id: session_id.clone(),
            report: report.clone(),
        }) {
            Err(ClientError::Protocol(ProtocolError::UnsupportedVersion(_)))
                if downgrade_kiro_stop_report(&mut report) =>
            {
                self.request(Request::ReportKiroHook {
                    holder_id,
                    session_id,
                    report,
                })?
            }
            result => result?,
        };
        match response {
            Response::Agent { agent } => Ok(agent),
            other => unexpected(other),
        }
    }

    pub fn release_kiro_launch_holder(&self, holder_id: impl Into<String>) -> Result<bool> {
        match self.request(Request::ReleaseKiroLaunchHolder {
            holder_id: holder_id.into(),
        })? {
            Response::KiroLaunchHolderReleased { released } => Ok(released),
            other => unexpected(other),
        }
    }

    pub fn ensure_opencode_shared_runtime(
        &self,
        port: u16,
    ) -> Result<OpenCodeSharedRuntimeSnapshot> {
        match self.request(Request::EnsureOpenCodeSharedRuntime {
            port,
            environment: Some(current_environment()),
        })? {
            Response::OpenCodeSharedRuntime {
                runtime: Some(runtime),
            } => Ok(runtime),
            Response::OpenCodeSharedRuntime { runtime: None } => {
                Err(ClientError::Validation(io::Error::new(
                    io::ErrorKind::NotFound,
                    "OpenCode executable is unavailable",
                )))
            }
            other => unexpected(other),
        }
    }

    pub fn try_ensure_opencode_shared_runtime(
        &self,
        port: u16,
    ) -> Result<Option<OpenCodeSharedRuntimeSnapshot>> {
        match self.request(Request::EnsureOpenCodeSharedRuntime {
            port,
            environment: Some(current_environment()),
        }) {
            Ok(Response::OpenCodeSharedRuntime { runtime }) => Ok(runtime),
            Err(ClientError::Remote(RemoteError {
                code: Some(ErrorCode::NotFound),
                ..
            })) => Ok(None),
            Err(error) => Err(error),
            Ok(other) => unexpected(other),
        }
    }

    pub fn get_opencode_shared_runtime(&self) -> Result<Option<OpenCodeSharedRuntimeSnapshot>> {
        match self.request(Request::GetOpenCodeSharedRuntime)? {
            Response::OpenCodeSharedRuntime { runtime } => Ok(runtime),
            other => unexpected(other),
        }
    }

    pub fn ensure_opencode_session_claim(
        &self,
        generation_id: impl Into<String>,
        holder_id: impl Into<String>,
        root_session_id: impl Into<String>,
        shell_id: impl Into<String>,
        run_id: impl Into<String>,
        spec: AgentRegistrationSpec,
    ) -> Result<(OpenCodeSessionClaimSnapshot, AgentInstanceSnapshot)> {
        match self.request(Request::EnsureOpenCodeSessionClaim {
            generation_id: generation_id.into(),
            holder_id: holder_id.into(),
            root_session_id: root_session_id.into(),
            shell_id: shell_id.into(),
            run_id: run_id.into(),
            spec,
        })? {
            Response::OpenCodeSessionClaim { claim, agent } => Ok((claim, agent)),
            other => unexpected(other),
        }
    }

    pub fn release_opencode_session_claim(
        &self,
        generation_id: impl Into<String>,
        holder_id: impl Into<String>,
        claim_id: impl Into<String>,
    ) -> Result<bool> {
        match self.request(Request::ReleaseOpenCodeSessionClaim {
            generation_id: generation_id.into(),
            holder_id: holder_id.into(),
            claim_id: claim_id.into(),
        })? {
            Response::OpenCodeSessionClaimReleased { released } => Ok(released),
            other => unexpected(other),
        }
    }

    pub fn resolve_opencode_session_claim(
        &self,
        generation_id: impl Into<String>,
        root_session_id: impl Into<String>,
    ) -> Result<(OpenCodeSessionClaimSnapshot, AgentInstanceSnapshot)> {
        match self.request(Request::ResolveOpenCodeSessionClaim {
            generation_id: generation_id.into(),
            root_session_id: root_session_id.into(),
        })? {
            Response::OpenCodeSessionClaim { claim, agent } => Ok((claim, agent)),
            other => unexpected(other),
        }
    }

    pub fn report_claimed_opencode_agent(
        &self,
        generation_id: impl Into<String>,
        root_session_id: impl Into<String>,
        report: AgentReport,
    ) -> Result<AgentInstanceSnapshot> {
        match self.request(Request::ReportClaimedOpenCodeAgent {
            generation_id: generation_id.into(),
            root_session_id: root_session_id.into(),
            report,
        })? {
            Response::Agent { agent } => Ok(agent),
            other => unexpected(other),
        }
    }

    pub fn set_claude_remote_control_binding(
        &self,
        agent_id: impl Into<String>,
        shell_id: impl Into<String>,
        run_id: impl Into<String>,
        bridge_session_id: Option<String>,
    ) -> Result<Option<ClaudeRemoteControlBindingSnapshot>> {
        match self.request(Request::SetClaudeRemoteControlBinding {
            agent_id: agent_id.into(),
            shell_id: shell_id.into(),
            run_id: run_id.into(),
            bridge_session_id,
        })? {
            Response::ClaudeRemoteControlBinding { binding } => Ok(binding),
            other => unexpected(other),
        }
    }

    pub fn get_claude_remote_control_binding(
        &self,
        agent_id: impl Into<String>,
        shell_id: impl Into<String>,
        run_id: impl Into<String>,
    ) -> Result<Option<ClaudeRemoteControlBindingSnapshot>> {
        match self.request(Request::GetClaudeRemoteControlBinding {
            agent_id: agent_id.into(),
            shell_id: shell_id.into(),
            run_id: run_id.into(),
        })? {
            Response::ClaudeRemoteControlBinding { binding } => Ok(binding),
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

    pub fn observe_agent_working_context(
        &self,
        agent_id: impl Into<String>,
        shell_id: impl Into<String>,
        run_id: impl Into<String>,
        path: impl Into<PathBuf>,
    ) -> Result<(AgentInstanceSnapshot, bool)> {
        match self.request(Request::ObserveAgentWorkingContext {
            agent_id: agent_id.into(),
            shell_id: shell_id.into(),
            run_id: run_id.into(),
            path: path.into(),
        })? {
            Response::AgentWorkingContext { agent, changed } => Ok((agent, changed)),
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

    pub fn attach_exact_run(
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
            None,
        )
    }

    pub fn attach_exact_run_with_timeout(
        &self,
        shell_id: impl Into<String>,
        expected_run_id: impl Into<String>,
        takeover: bool,
        profile: TerminalProfile,
        timeout: Duration,
    ) -> Result<Attachment> {
        let request = Request::Attach {
            shell_id: shell_id.into(),
            takeover,
            restart_exited: false,
            expected_run_id: Some(expected_run_id.into()),
            profile,
            environment: None,
            owner_environment: false,
        };
        let version = self.protocol_version.load(Ordering::Acquire);
        if !protocol::ProtocolFeature::ExactRunAttachment.is_supported_by(version) {
            return Err(unsupported_version(
                "daemon does not support exact run attachment",
            ));
        }
        let (stream, response) = self.send_with_version_timeout(request, version, Some(timeout))?;
        stream
            .set_read_timeout(None)
            .map_err(ClientError::Transport)?;
        stream
            .set_write_timeout(None)
            .map_err(ClientError::Transport)?;
        attachment_from_response(stream, version, response)
    }

    pub fn attach_collaborative_exact_run_with_timeout(
        &self,
        shell_id: impl Into<String>,
        expected_run_id: impl Into<String>,
        profile: TerminalProfile,
        timeout: Duration,
    ) -> Result<Attachment> {
        let version = self.protocol_version.load(Ordering::Acquire);
        if !protocol::ProtocolFeature::CollaborativeExactRunAttachment.is_supported_by(version) {
            return Err(unsupported_version(
                "daemon does not support collaborative exact run attachment",
            ));
        }
        let request = Request::AttachCollaborative {
            shell_id: shell_id.into(),
            expected_run_id: expected_run_id.into(),
            profile,
        };
        let (stream, response) = self.send_with_version_timeout(request, version, Some(timeout))?;
        stream
            .set_read_timeout(None)
            .map_err(ClientError::Transport)?;
        stream
            .set_write_timeout(None)
            .map_err(ClientError::Transport)?;
        attachment_from_response(stream, version, response)
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

fn normalize_host_service_result(
    mut result: crate::protocol::HostServiceResult,
) -> crate::protocol::HostServiceResult {
    if let crate::protocol::HostServiceResult::AgentSession { session } = &mut result
        && session.projected_occurrences.is_empty()
    {
        session.projected_occurrences = session
            .occurrences
            .iter()
            .map(|agent| crate::protocol::HostAgentSessionOccurrence {
                agent_id: agent.id.clone(),
                shell_id: agent.shell_id.clone(),
                retained_shell_name: None,
                retained_shell_cwd: None,
                source_cwd: agent.cwd.clone(),
                run_id: agent.run_id.clone(),
                started_at_ms: agent.started_at_ms,
                ended_at_ms: agent.ended_at_ms,
                is_current: false,
                observation: agent.observation.clone(),
            })
            .collect();
    }
    result
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
            profile,
        } => Ok(Attachment {
            stream,
            protocol_version,
            token,
            reconstruction,
            warning,
            profile,
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

#[derive(Debug)]
pub struct DaemonLockReservation {
    _lock: std::fs::File,
}

pub fn reserve_daemon_lock(socket_path: &Path) -> io::Result<Option<DaemonLockReservation>> {
    let lock_path = socket_path
        .parent()
        .ok_or_else(|| io::Error::other("daemon socket has no parent"))?
        .join("daemon.lock");
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(lock_path)?;
    let metadata = lock.metadata()?;
    if !metadata.is_file() || metadata.uid() != unsafe { libc::geteuid() } || metadata.nlink() != 1
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "daemon lock is not an owner-controlled regular file",
        ));
    }
    // The descriptor remains live for both flock operations.
    if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
        return Ok(Some(DaemonLockReservation { _lock: lock }));
    }
    let error = io::Error::last_os_error();
    if error.raw_os_error() == Some(libc::EWOULDBLOCK) {
        Ok(None)
    } else {
        Err(error)
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
    if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
        let _ = unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_UN) };
        return Ok(true);
    };
    let error = io::Error::last_os_error();
    if error.raw_os_error() == Some(libc::EWOULDBLOCK) {
        Ok(false)
    } else {
        Err(error)
    }
}

fn downgrade_kiro_stop_report(report: &mut AgentReport) -> bool {
    if report.state != AgentState::Idle {
        return false;
    }
    report.state = AgentState::Unknown;
    report.evidence = "Kiro hook execution stopped".into();
    true
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

#[cfg(target_os = "linux")]
fn daemon_listener_holder(socket_path: &Path, uid: u32) -> io::Result<u32> {
    let inode = daemon_listener_inode(socket_path)?;
    let expected = format!("socket:[{inode}]");
    let mut holders = Vec::new();
    let mut processes = fs::read_dir("/proc")?;
    let mut descriptor_count = 0usize;
    let deadline = Instant::now() + DAEMON_IDENTITY_TIMEOUT;
    for _ in 0..MAX_PROC_ENTRIES {
        if Instant::now() >= deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "daemon listener holder inspection timed out",
            ));
        }
        let Some(entry) = processes.next() else {
            return unique_listener_holder(holders);
        };
        let entry = entry?;
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|value| value.parse::<u32>().ok())
        else {
            continue;
        };
        let process_path = entry.path();
        match fs::metadata(&process_path) {
            Ok(metadata) if metadata.is_dir() && metadata.uid() == uid => {}
            Ok(_) => continue,
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::NotFound | io::ErrorKind::PermissionDenied
                ) =>
            {
                continue;
            }
            Err(error) => return Err(error),
        }
        let descriptors = match fs::read_dir(process_path.join("fd")) {
            Ok(descriptors) => descriptors,
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::NotFound | io::ErrorKind::PermissionDenied
                ) =>
            {
                continue;
            }
            Err(error) => return Err(error),
        };
        let mut holder = false;
        for descriptor in descriptors {
            if Instant::now() >= deadline {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "daemon listener holder inspection timed out",
                ));
            }
            descriptor_count = descriptor_count.checked_add(1).ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "process descriptor count overflowed",
                )
            })?;
            if descriptor_count > MAX_PROC_FD_ENTRIES {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "process descriptor tables exceed the identification bound",
                ));
            }
            let descriptor = match descriptor {
                Ok(descriptor) => descriptor,
                Err(_error)
                    if fs::metadata(&process_path)
                        .is_err_and(|current| current.kind() == io::ErrorKind::NotFound) =>
                {
                    break;
                }
                Err(error) => return Err(error),
            };
            match fs::read_link(descriptor.path()) {
                Ok(target) if target.as_os_str().as_bytes() == expected.as_bytes() => {
                    holder = true;
                    break;
                }
                Ok(_) => {}
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::NotFound | io::ErrorKind::PermissionDenied
                    ) => {}
                Err(error) => return Err(error),
            }
        }
        if holder {
            holders.push(pid);
            if holders.len() > 1 {
                return unique_listener_holder(holders);
            }
        }
    }
    if processes.next().is_some() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "process table exceeds the identification bound",
        ));
    }
    unique_listener_holder(holders)
}

#[cfg(target_os = "linux")]
fn unique_listener_holder(mut holders: Vec<u32>) -> io::Result<u32> {
    if holders.len() == 1 {
        Ok(holders.remove(0))
    } else if holders.is_empty() {
        Err(io::Error::new(
            io::ErrorKind::NotFound,
            "daemon listener holder was not found",
        ))
    } else {
        Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "daemon listener has multiple process holders",
        ))
    }
}

#[cfg(target_os = "linux")]
fn daemon_listener_inode(socket_path: &Path) -> io::Result<u64> {
    let mut bytes = Vec::new();
    File::open("/proc/net/unix")?
        .take(MAX_PROC_UNIX_BYTES + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_PROC_UNIX_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Unix socket table exceeds the identification bound",
        ));
    }
    let expected = socket_path.as_os_str().as_bytes();
    let mut found = None;
    for line in bytes.split(|byte| *byte == b'\n') {
        let Some((inode, path)) = parse_proc_unix_entry(line) else {
            continue;
        };
        if path != expected {
            continue;
        }
        if found.replace(inode).is_some_and(|current| current != inode) {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "daemon socket path has multiple kernel identities",
            ));
        }
    }
    found.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "daemon socket was not found in the Unix socket table",
        )
    })
}

#[cfg(target_os = "linux")]
fn parse_proc_unix_entry(line: &[u8]) -> Option<(u64, &[u8])> {
    let mut position = 0;
    let mut inode = None;
    let mut flags = None;
    let mut socket_type = None;
    let mut state = None;
    for field in 0..7 {
        while line.get(position).is_some_and(u8::is_ascii_whitespace) {
            position += 1;
        }
        let start = position;
        while line
            .get(position)
            .is_some_and(|byte| !byte.is_ascii_whitespace())
        {
            position += 1;
        }
        if start == position {
            return None;
        }
        let value = &line[start..position];
        match field {
            3 => flags = parse_proc_hex(value),
            4 => socket_type = parse_proc_hex(value),
            5 => state = parse_proc_hex(value),
            6 => inode = std::str::from_utf8(value).ok()?.parse::<u64>().ok(),
            _ => {}
        }
    }
    while line.get(position).is_some_and(u8::is_ascii_whitespace) {
        position += 1;
    }
    let inode = inode?;
    (flags? & 0x0001_0000 != 0
        && socket_type? == libc::SOCK_STREAM as u64
        && state? == 1
        && position < line.len())
    .then_some((inode, &line[position..]))
}

#[cfg(target_os = "linux")]
fn parse_proc_hex(value: &[u8]) -> Option<u64> {
    u64::from_str_radix(std::str::from_utf8(value).ok()?, 16).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::net::UnixListener;

    use uuid::Uuid;

    #[cfg(target_os = "linux")]
    #[test]
    fn proc_unix_entry_preserves_socket_paths_with_spaces() {
        assert_eq!(
            parse_proc_unix_entry(
                b"0000000000000000: 00000002 00000000 00010000 0001 01 12345 /run/user/1000/path with spaces.sock"
            ),
            Some((12345, b"/run/user/1000/path with spaces.sock".as_slice()))
        );
        assert_eq!(
            parse_proc_unix_entry(b"Num RefCount Protocol Flags Type St Inode Path"),
            None
        );
        assert_eq!(
            parse_proc_unix_entry(
                b"0000000000000000: 00000003 00000000 00000000 0001 03 12346 /run/user/1000/path with spaces.sock"
            ),
            None
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn listener_holder_identity_requires_exactly_one_process() {
        assert_eq!(unique_listener_holder(vec![42]).unwrap(), 42);
        assert_eq!(
            unique_listener_holder(Vec::new()).unwrap_err().kind(),
            io::ErrorKind::NotFound
        );
        assert_eq!(
            unique_listener_holder(vec![42, 43]).unwrap_err().kind(),
            io::ErrorKind::PermissionDenied
        );
    }

    #[test]
    fn optional_runtime_accepts_legacy_typed_absence_without_changing_required_callers() {
        let directory = env::temp_dir().join(format!("boomux-client-runtime-{}", Uuid::new_v4()));
        fs::create_dir_all(&directory).unwrap();
        let socket = directory.join("daemon.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let server = thread::spawn(move || {
            for _ in 0..2 {
                let (mut stream, _) = listener.accept().unwrap();
                let request: Envelope<Request> = protocol::read_message(&mut stream).unwrap();
                assert_eq!(request.version, protocol::PROTOCOL_VERSION);
                assert!(matches!(
                    request.message,
                    Request::EnsureOpenCodeSharedRuntime { .. }
                ));
                protocol::write_message(
                    &mut stream,
                    &Envelope::new(Response::Error {
                        message: "OpenCode executable is unavailable outside the Boomux shim"
                            .into(),
                        code: Some(ErrorCode::NotFound),
                    }),
                )
                .unwrap();
            }
        });
        let client = Client::from_socket_path(socket);

        assert_eq!(
            client.try_ensure_opencode_shared_runtime(4097).unwrap(),
            None
        );
        assert!(matches!(
            client.ensure_opencode_shared_runtime(4097).unwrap_err(),
            ClientError::Remote(RemoteError {
                code: Some(ErrorCode::NotFound),
                ..
            })
        ));

        server.join().unwrap();
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn protocol_forty_six_daemon_is_rejected_without_downgrade() {
        let directory = env::temp_dir().join(format!("boomux-client-{}", Uuid::new_v4()));
        fs::create_dir_all(&directory).unwrap();
        let socket = directory.join("daemon.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let server = thread::spawn(move || {
            for expected in [51, 50, 49, 48, 47] {
                let (mut stream, _) = listener.accept().unwrap();
                let request: Envelope<Request> = protocol::read_message(&mut stream).unwrap();
                assert_eq!(request.version, expected);
                assert_eq!(request.message, Request::Ping);
                protocol::write_message(
                    &mut stream,
                    &Envelope::with_version(
                        46,
                        Response::Error {
                            message: format!("protocol {expected} unsupported"),
                            code: Some(ErrorCode::UnsupportedVersion),
                        },
                    ),
                )
                .unwrap();
            }
        });
        let client = Client::from_socket_path(socket);

        assert!(matches!(
            client.protocol_version(),
            Err(ClientError::Protocol(ProtocolError::UnsupportedVersion(ref message)))
                if message == "daemon has no compatible protocol version"
        ));

        server.join().unwrap();
        fs::remove_dir_all(directory).unwrap();
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
                        profile: None,
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
    fn exact_run_attachment_can_omit_gateway_environment_and_restart_authority() {
        let directory = env::temp_dir().join(format!("boomux-client-{}", Uuid::new_v4()));
        fs::create_dir_all(&directory).unwrap();
        let socket = directory.join("daemon.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let request: Envelope<Request> = protocol::read_message(&mut stream).unwrap();
            assert_eq!(request.version, protocol::PROTOCOL_VERSION);
            assert_eq!(
                request.message,
                Request::Attach {
                    shell_id: "shell-1".into(),
                    takeover: true,
                    restart_exited: false,
                    expected_run_id: Some("run-1".into()),
                    profile: test_profile(),
                    environment: None,
                    owner_environment: false,
                }
            );
            protocol::write_message(
                &mut stream,
                &Envelope::with_version(
                    protocol::PROTOCOL_VERSION,
                    Response::Attached {
                        token: "token".into(),
                        reconstruction: Vec::new(),
                        warning: None,
                        profile: None,
                    },
                ),
            )
            .unwrap();
        });
        let client = Client::from_socket_path(socket);

        let attachment = client
            .attach_exact_run_with_timeout(
                "shell-1",
                "run-1",
                true,
                test_profile(),
                Duration::from_secs(1),
            )
            .unwrap();

        assert_eq!(attachment.token, "token");
        assert_eq!(attachment.stream.read_timeout().unwrap(), None);
        assert_eq!(attachment.stream.write_timeout().unwrap(), None);
        server.join().unwrap();
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn exact_run_attachment_timeout_bounds_a_stalled_gateway_request() {
        let directory = env::temp_dir().join(format!("boomux-client-{}", Uuid::new_v4()));
        fs::create_dir_all(&directory).unwrap();
        let socket = directory.join("daemon.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let _: Envelope<Request> = protocol::read_message(&mut stream).unwrap();
            thread::sleep(Duration::from_millis(100));
        });
        let client = Client::from_socket_path(socket);

        let error = client
            .attach_exact_run_with_timeout(
                "shell-1",
                "run-1",
                true,
                test_profile(),
                Duration::from_millis(10),
            )
            .unwrap_err();

        assert!(matches!(
            error,
            ClientError::Transport(ref error)
                if matches!(error.kind(), io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock)
        ));
        server.join().unwrap();
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn protocol_fifty_client_normalizes_protocol_forty_nine_session_occurrences() {
        let legacy: protocol::HostServiceResult = serde_json::from_value(serde_json::json!({
            "result": "agent_session",
            "session": {
                "summary": {
                    "id": "session-id",
                    "workspace_id": "workspace-id",
                    "workspace_name": "workspace",
                    "description": "legacy",
                    "integration": "opencode",
                    "external_session_id": "external-id",
                    "state": "inactive",
                    "state_is_current": false,
                    "started_at_ms": 10,
                    "last_at_ms": 20,
                    "occurrence_count": 1
                },
                "source_cwd": "/tmp/project",
                "occurrences": [{
                    "id": "agent-id",
                    "workspace_id": "workspace-id",
                    "shell_id": "shell-id",
                    "run_id": "run-id",
                    "name": "legacy",
                    "integration": "opencode",
                    "external_session_id": "external-id",
                    "cwd": "/tmp/project",
                    "started_at_ms": 10,
                    "ended_at_ms": 20,
                    "observation": {
                        "revision": 1,
                        "state": "inactive",
                        "authority": "lifecycle_integration",
                        "evidence": "legacy peer",
                        "confidence": 100,
                        "observed_at_ms": 20
                    }
                }]
            }
        }))
        .unwrap();

        let protocol::HostServiceResult::AgentSession { session } =
            normalize_host_service_result(legacy)
        else {
            panic!("expected Session inspection");
        };
        assert_eq!(session.projected_occurrences.len(), 1);
        let occurrence = &session.projected_occurrences[0];
        assert_eq!(occurrence.agent_id, "agent-id");
        assert_eq!(occurrence.shell_id, "shell-id");
        assert_eq!(occurrence.run_id, "run-id");
        assert_eq!(
            occurrence.source_cwd.as_deref(),
            Some(Path::new("/tmp/project"))
        );
        assert!(!occurrence.is_current);
    }
}

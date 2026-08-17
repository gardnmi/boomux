use std::collections::{HashMap, HashSet, VecDeque};
use std::env;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::OsStringExt;
use std::os::unix::fs::MetadataExt;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};
use std::sync::mpsc::{self, SyncSender, TrySendError};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, TryLockError, Weak};
use std::thread;
use std::time::{Duration, Instant};

use portable_pty::{Child, CommandBuilder, MasterPty, PtySize, native_pty_system};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::client;
use crate::desktop_notifications::{
    DesktopNotificationSink, DisabledNotificationSink, NotificationReason, NotificationRequest,
    NotificationSink, category_enabled, test_delivery,
};
use crate::fd_transfer::send_descriptor;
use crate::handoff;
use crate::node_identity::{NodeIdentityLease, NodeIdentityManager};
use crate::node_projection::NodeProjectionCache;
use crate::node_registration::NodeRegistrationManager;
use crate::protocol::{
    self, AgentAttentionReason, AgentAttentionSnapshot, AgentAuthority, AgentInstanceSnapshot,
    AgentObservationSnapshot, AgentRegistrationSpec, AgentReport, AgentScheduleInspection,
    AgentScheduleOverlapPolicy, AgentScheduleSession, AgentScheduleSnapshot, AgentScheduleSpec,
    AgentScheduleState, AgentScheduleTrigger, AgentScheduleUpdate, AgentState, AttachFrame,
    DaemonEvent, DaemonEventKind, Envelope, ErrorCode, EventCursor, FocusedTerminalSnapshot,
    NodeProjectionAgent, NodeProjectionAttention, NodeProjectionExecution, NodeProjectionLauncher,
    NodeProjectionSchedule, NodeProjectionShell, NodeProjectionSnapshot, NodeProjectionSync,
    NodeProjectionSyncMode, NodeProjectionTransition, NodeProjectionTransitionKind,
    NodeProjectionWorkspace, NotificationDeliveryConfig, Request, Response, RoutedOperation,
    RoutedOperationResult, ScheduledExecutionDispatchKind, ScheduledExecutionOutcome,
    ScheduledExecutionReason, ScheduledExecutionScheduleProjection, ScheduledExecutionSnapshot,
    ScheduledExecutionState, ScheduledOccurrence, ScheduledRunnerResult, SchedulerHealth,
    SchedulerState, ShellOwner, ShellRunExitReason, ShellRunSnapshot, ShellSnapshot, ShellSpec,
    ShellStatus, Snapshot, TerminalPreview, TerminalProfile, UnixEnvironment,
    UnixEnvironmentVariable, WorkspaceLauncherSnapshot, WorkspaceLauncherSpec, WorkspaceSnapshot,
};
use crate::ssh_bootstrap::{self, RemoteBootstrapPlan, SshAuthenticationMode, SshTarget};
use crate::state_store::{
    PersistedAgentInstance, PersistedAgentSchedule, PersistedScheduledExecution, PersistedShell,
    PersistedShellRun, PersistedState, PersistedWorkspace, PersistedWorkspaceLauncher, StateStore,
};
use crate::terminal_state::TerminalState;

const CONTROLLER_QUEUE: usize = 64;
const MAX_CONNECTION_HANDLERS: usize = 64;
const SHUTDOWN_GRACE: Duration = Duration::from_millis(50);
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(2);
const RESPONSE_WRITE_TIMEOUT: Duration = Duration::from_secs(1);
const RESTART_TIMEOUT: Duration = Duration::from_secs(10);
const IO_RETRY_DELAY: Duration = Duration::from_millis(2);
const OUTPUT_PUBLICATION_INTERVAL: Duration = Duration::from_millis(16);
const PERSIST_RETRY_INTERVAL: Duration = Duration::from_secs(1);
const SCHEDULER_RETRY_MIN: Duration = Duration::from_millis(50);
const SCHEDULER_RETRY_MAX: Duration = Duration::from_secs(5);
const TERMINAL_HISTORY_INTERVAL: Duration = Duration::from_secs(5);
const FOREGROUND_PROCESS_CACHE_INTERVAL: Duration = Duration::from_secs(1);
const MAX_TERMINAL_HISTORY_BYTES: usize = 256 * 1024;
const MAX_TERMINAL_ENV_VALUE: usize = 256;
const MAX_NAME_BYTES: usize = 256;
const MAX_AGENT_EVIDENCE_BYTES: usize = 4 * 1024;
const MAX_TERMINAL_ROWS: u16 = 1_000;
pub const MAX_SCHEDULED_EXECUTION_CONCURRENCY: u16 = 64;
const MAX_TERMINAL_COLS: u16 = 1_000;
const MAX_TERMINAL_PREVIEW_LINES: usize = 500;
const MAX_TERMINAL_PREVIEW_SPANS: usize = 20_000;
const MAX_TERMINAL_CELLS: usize = 1_000_000;
const MAX_SHELL_READ_BYTES: usize = 1024 * 1024;
const MAX_FOREGROUND_PROCESS_BYTES: usize = 64;
const MAX_RETAINED_EVENTS: usize = 8_192;
const MAX_TERMINAL_EXECUTIONS_PER_SCHEDULE: usize = 100;
const DISPATCH_KEY_FILTER_BYTES: usize = 2048;
const MAX_EVENT_BATCH: u16 = 256;
const MAX_EVENT_WAIT: Duration = Duration::from_secs(30);
const TRANSITION_IDLE: u8 = 0;
const TRANSITION_RESTART: u8 = 1;
const TRANSITION_SHUTDOWN: u8 = 2;
const TRANSITION_REKEY: u8 = 3;
const NODE_REKEY_DRAIN_TIMEOUT: Duration = Duration::from_millis(250);
const NODE_REGISTRATION_DRAIN_TIMEOUT: Duration = Duration::from_millis(250);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NotificationSettings {
    pub enabled: bool,
    pub blocked: bool,
    pub completed: bool,
    pub scheduled_dispatch_failed: bool,
    pub scheduled_interrupted: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NotificationSoundSettings {
    pub enabled: bool,
    pub blocked: String,
    pub completed: String,
    pub scheduled_dispatch_failed: String,
    pub scheduled_interrupted: String,
}

impl Default for NotificationSoundSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            blocked: "message-new-instant".into(),
            completed: "complete".into(),
            scheduled_dispatch_failed: "dialog-warning".into(),
            scheduled_interrupted: "dialog-warning".into(),
        }
    }
}

impl From<NotificationDeliveryConfig> for NotificationDeliverySettings {
    fn from(config: NotificationDeliveryConfig) -> Self {
        Self {
            desktop: NotificationSettings {
                enabled: config.desktop_enabled,
                blocked: config.blocked,
                completed: config.completed,
                scheduled_dispatch_failed: config.scheduled_dispatch_failed,
                scheduled_interrupted: config.scheduled_interrupted,
            },
            sound: NotificationSoundSettings {
                enabled: config.sound_enabled,
                blocked: config.blocked_sound,
                completed: config.completed_sound,
                scheduled_dispatch_failed: config.scheduled_dispatch_failed_sound,
                scheduled_interrupted: config.scheduled_interrupted_sound,
            },
            resume_agents: config.resume_agents,
            persist_terminal_history: config.persist_terminal_history,
            max_scheduled_execution_concurrency: config.max_scheduled_execution_concurrency,
        }
    }
}

impl From<NotificationDeliverySettings> for NotificationDeliveryConfig {
    fn from(settings: NotificationDeliverySettings) -> Self {
        Self {
            desktop_enabled: settings.desktop.enabled,
            sound_enabled: settings.sound.enabled,
            blocked: settings.desktop.blocked,
            completed: settings.desktop.completed,
            scheduled_dispatch_failed: settings.desktop.scheduled_dispatch_failed,
            scheduled_interrupted: settings.desktop.scheduled_interrupted,
            blocked_sound: settings.sound.blocked,
            completed_sound: settings.sound.completed,
            scheduled_dispatch_failed_sound: settings.sound.scheduled_dispatch_failed,
            scheduled_interrupted_sound: settings.sound.scheduled_interrupted,
            resume_agents: settings.resume_agents,
            persist_terminal_history: settings.persist_terminal_history,
            max_scheduled_execution_concurrency: settings.max_scheduled_execution_concurrency,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NotificationDeliverySettings {
    pub desktop: NotificationSettings,
    pub sound: NotificationSoundSettings,
    pub resume_agents: bool,
    pub persist_terminal_history: bool,
    pub max_scheduled_execution_concurrency: u16,
}

impl Default for NotificationDeliverySettings {
    fn default() -> Self {
        Self {
            desktop: NotificationSettings::default(),
            sound: NotificationSoundSettings::default(),
            resume_agents: true,
            persist_terminal_history: false,
            max_scheduled_execution_concurrency: 4,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NotificationTestReason {
    Blocked,
    Completed,
}

pub fn test_notification_delivery(
    settings: &NotificationDeliverySettings,
    reason: NotificationTestReason,
) -> io::Result<()> {
    test_delivery(
        settings,
        match reason {
            NotificationTestReason::Blocked => NotificationReason::Blocked,
            NotificationTestReason::Completed => NotificationReason::Completed,
        },
    )
}

impl Default for NotificationSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            blocked: true,
            completed: true,
            scheduled_dispatch_failed: false,
            scheduled_interrupted: false,
        }
    }
}

pub fn receive_handoff(channel: i32) -> io::Result<()> {
    receive_handoff_with_notifications(channel, NotificationSettings::default())
}

pub fn receive_handoff_with_notifications(
    channel: i32,
    notification_settings: NotificationSettings,
) -> io::Result<()> {
    receive_handoff_with_notification_delivery(
        channel,
        NotificationDeliverySettings {
            desktop: notification_settings,
            ..Default::default()
        },
    )
}

pub fn receive_handoff_with_notification_delivery(
    channel: i32,
    notification_settings: NotificationDeliverySettings,
) -> io::Result<()> {
    match handoff::receive_bootstrap(channel)? {
        handoff::Bootstrap::Aborted => Ok(()),
        handoff::Bootstrap::Committed {
            mut channel,
            listener,
            runtime_lock,
            state_lock,
            runtimes,
            exited,
            event_stream,
            notifications,
            focused_terminal,
        } => {
            let store = StateStore::from_transferred_lock(state_lock)?;
            let socket_path = client::socket_path()?;
            run_daemon(
                listener,
                File::from(runtime_lock),
                SocketCleanup::disarmed(socket_path),
                store,
                TransferredState {
                    runtimes,
                    exited,
                    events: Some(*event_stream),
                    focused_terminal: focused_terminal.map(|focused| *focused),
                },
                Some(&mut channel),
                notifications
                    .map(Into::into)
                    .unwrap_or(notification_settings),
            )
        }
    }
}

pub fn run() -> io::Result<()> {
    run_with_notifications(NotificationSettings::default())
}

pub fn run_with_notifications(notification_settings: NotificationSettings) -> io::Result<()> {
    run_with_notification_delivery(NotificationDeliverySettings {
        desktop: notification_settings,
        ..Default::default()
    })
}

pub fn run_with_notification_delivery(
    notification_settings: NotificationDeliverySettings,
) -> io::Result<()> {
    let socket_path = client::socket_path()?;
    let runtime_dir = socket_path
        .parent()
        .ok_or_else(|| io::Error::other("socket path has no parent"))?;
    secure_runtime_dir(runtime_dir)?;
    let daemon_lock = acquire_daemon_lock(runtime_dir)?;

    if socket_path.exists() {
        fs::remove_file(&socket_path)?;
    }

    let listener = UnixListener::bind(&socket_path)?;
    let socket_cleanup = SocketCleanup::new(socket_path.clone());
    fs::set_permissions(&socket_path, fs::Permissions::from_mode(0o600))?;
    listener.set_nonblocking(true)?;
    run_daemon(
        listener,
        daemon_lock,
        socket_cleanup,
        StateStore::from_environment()?,
        TransferredState::default(),
        None,
        notification_settings,
    )
}

#[derive(Default)]
struct TransferredState {
    runtimes: Vec<handoff::TransferredRuntime>,
    exited: Vec<handoff::TransferredExited>,
    events: Option<handoff::EventStreamManifest>,
    focused_terminal: Option<FocusedTerminalSnapshot>,
}

fn run_daemon(
    listener: UnixListener,
    daemon_lock: File,
    mut socket_cleanup: SocketCleanup,
    store: StateStore,
    transferred: TransferredState,
    committed: Option<&mut UnixStream>,
    notification_settings: NotificationDeliverySettings,
) -> io::Result<()> {
    validate_notification_delivery_settings(&notification_settings)?;
    let live_handoff = committed.is_some();
    let mut registry = DaemonService::restore(store, live_handoff, transferred.events)?;
    registry.node_identity = match NodeIdentityManager::load_or_create_from_environment() {
        Ok(identity) => Some(identity),
        Err(error) => {
            eprintln!("boomux: federation disabled: {error}");
            None
        }
    };
    let registrations = NodeRegistrationManager::load_from_environment();
    if let Some(reason) = registrations.unavailable_reason()? {
        eprintln!("boomux: Node registration routing disabled: {reason}");
    }
    registry.node_registrations = Some(registrations);
    registry.node_projection_cache = Some(NodeProjectionCache::load_from_environment());
    registry.startup_environment = capture_current_environment();
    registry.configure_scheduler_clock()?;
    registry.notification_settings = notification_settings.clone();
    if !registry.notification_settings.persist_terminal_history {
        registry.clear_terminal_histories()?;
    }
    registry.notification_sink = Arc::new(DesktopNotificationSink::new(notification_settings));
    registry.publish_cold_recovery_notifications();
    let registry = Arc::new(registry);
    let shells = registry.durable.shells()?;
    let (gated_readers, handoff_changed) = registry.runtimes.import_handoff(
        Arc::downgrade(&registry),
        shells,
        transferred.runtimes,
        transferred.exited,
    )?;
    if handoff_changed {
        registry.persist()?;
    }
    registry.import_focused_terminal(transferred.focused_terminal)?;
    if let Some(channel) = committed {
        {
            registry.events.transaction()?.reserve(1)?;
        }
        channel.write_all(&[handoff::PREPARED])?;
        let mut decision = [0];
        channel.read_exact(&mut decision)?;
        match decision[0] {
            handoff::ABORT => return Ok(()),
            handoff::FINALIZE => {
                socket_cleanup.arm();
                if registry.durable.persistence_dirty.load(Ordering::Acquire) {
                    let _ = registry.persist();
                }
                registry
                    .events
                    .publish_runtime_batch(vec![DaemonEventKind::HandoffCompleted])?;
                for runtime in gated_readers {
                    registry.runtimes.resume_reader(&runtime)?;
                }
                registry.resume_claimed_schedule_executions();
                // FINALIZE is the irreversible ownership boundary. Failure to
                // report COMMITTED must not make the old daemon resume.
                let _ = channel.write_all(&[handoff::COMMITTED]);
            }
            _ => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "replacement daemon received an invalid final decision",
                ));
            }
        }
    } else if !gated_readers.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "transferred runtimes require a handoff commit channel",
        ));
    } else if registry.durable.persistence_dirty.load(Ordering::Acquire) {
        registry.persist()?;
    }
    if !live_handoff {
        registry
            .evaluate_schedules(true)
            .map_err(|error| io::Error::other(error.to_string()))?;
    }
    registry.start_scheduler()?;
    registry.start_node_projection_workers()?;
    let shutdown = Arc::new(AtomicBool::new(false));
    let transition = Arc::new(AtomicU8::new(TRANSITION_IDLE));
    let (restart_sender, restart_receiver) = mpsc::channel::<RestartRequest>();
    let mut handlers: Vec<thread::JoinHandle<()>> = Vec::new();
    let mut handed_off = false;
    let mut last_persistence_retry = Instant::now();

    while !shutdown.load(Ordering::Acquire) {
        let mut index = 0;
        while index < handlers.len() {
            if handlers[index].is_finished() {
                let _ = handlers.swap_remove(index).join();
            } else {
                index += 1;
            }
        }
        if registry.durable.persistence_dirty.load(Ordering::Acquire)
            && last_persistence_retry.elapsed() >= PERSIST_RETRY_INTERVAL
        {
            let _ = registry.flush_pending();
            last_persistence_retry = Instant::now();
        }
        match restart_receiver.try_recv() {
            Ok(request) => {
                registry.stop_scheduler()?;
                registry.stop_node_projection_workers()?;
                let result = launch_replacement(
                    &listener,
                    &daemon_lock,
                    &registry,
                    request.notification_settings,
                    request.startup_environment,
                );
                if result.is_ok() {
                    handed_off = true;
                    shutdown.store(true, Ordering::Release);
                } else {
                    transition.store(TRANSITION_IDLE, Ordering::Release);
                    registry.start_scheduler()?;
                    registry.start_node_projection_workers()?;
                }
                let _ = request.reply.send(result);
                continue;
            }
            Err(mpsc::TryRecvError::Disconnected) => {
                return Err(io::Error::other("restart control channel closed"));
            }
            Err(mpsc::TryRecvError::Empty) => {}
        }
        match listener.accept() {
            Ok((stream, _)) => {
                if handlers.len() >= MAX_CONNECTION_HANDLERS {
                    drop(stream);
                    continue;
                }
                let registry = Arc::clone(&registry);
                let shutdown = Arc::clone(&shutdown);
                let transition = Arc::clone(&transition);
                let restart_sender = restart_sender.clone();
                handlers.push(
                    thread::Builder::new()
                        .name("boomux-connection".into())
                        .spawn(move || {
                            let _ = handle_connection(
                                stream,
                                registry,
                                shutdown,
                                transition,
                                restart_sender,
                            );
                        })?,
                );
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(25));
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        }
    }
    for handler in handlers {
        let _ = handler.join();
    }
    registry.stop_scheduler()?;
    registry.stop_node_projection_workers()?;
    let result = if handed_off {
        socket_cleanup.disarm();
        Ok(())
    } else {
        registry.shutdown().map_err(|error| match error {
            DaemonError::Validation(source)
            | DaemonError::Protocol(source)
            | DaemonError::Internal(source)
            | DaemonError::Lifecycle { source, .. }
            | DaemonError::Persistence { source, .. } => source,
        })
    };
    drop(registry);
    drop(listener);
    drop(socket_cleanup);
    drop(daemon_lock);
    result
}

struct RestartRequest {
    reply: SyncSender<DaemonResult<()>>,
    notification_settings: Option<NotificationDeliverySettings>,
    startup_environment: Option<UnixEnvironment>,
}

#[derive(Debug)]
enum DaemonError {
    Validation(io::Error),
    Lifecycle { code: ErrorCode, source: io::Error },
    Persistence { message: String, source: io::Error },
    Protocol(io::Error),
    Internal(io::Error),
}

type DaemonResult<T> = Result<T, DaemonError>;

impl DaemonError {
    fn validation(message: impl Into<String>) -> Self {
        Self::Validation(io::Error::new(io::ErrorKind::InvalidInput, message.into()))
    }

    fn lifecycle(code: ErrorCode, message: impl Into<String>) -> Self {
        Self::Lifecycle {
            code,
            source: io::Error::other(message.into()),
        }
    }

    fn protocol(message: impl Into<String>) -> Self {
        Self::Protocol(io::Error::new(io::ErrorKind::InvalidData, message.into()))
    }

    fn persistence(source: io::Error) -> Self {
        Self::Persistence {
            message: source.to_string(),
            source,
        }
    }

    fn persistence_context(source: io::Error, message: impl Into<String>) -> Self {
        Self::Persistence {
            message: message.into(),
            source,
        }
    }

    fn wire_code(&self) -> ErrorCode {
        match self {
            Self::Validation(_) => ErrorCode::InvalidArgument,
            Self::Lifecycle { code, .. } => *code,
            Self::Persistence { .. } => ErrorCode::PersistenceFailed,
            Self::Protocol(_) => ErrorCode::UnsupportedVersion,
            Self::Internal(_) => ErrorCode::Internal,
        }
    }

    fn into_response(self) -> Response {
        error_response(self.wire_code(), self.to_string())
    }
}

impl From<io::Error> for DaemonError {
    fn from(source: io::Error) -> Self {
        match source.kind() {
            io::ErrorKind::InvalidInput | io::ErrorKind::InvalidData => Self::Validation(source),
            io::ErrorKind::NotFound => Self::Lifecycle {
                code: ErrorCode::NotFound,
                source,
            },
            io::ErrorKind::AlreadyExists => Self::Lifecycle {
                code: ErrorCode::AlreadyExists,
                source,
            },
            io::ErrorKind::WouldBlock | io::ErrorKind::AddrInUse => Self::Lifecycle {
                code: ErrorCode::Busy,
                source,
            },
            io::ErrorKind::ConnectionAborted => Self::Lifecycle {
                code: ErrorCode::DaemonStopping,
                source,
            },
            io::ErrorKind::TimedOut => Self::Lifecycle {
                code: ErrorCode::Timeout,
                source,
            },
            _ => Self::Internal(source),
        }
    }
}

impl std::fmt::Display for DaemonError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Persistence { message, .. } => formatter.write_str(message),
            Self::Validation(source)
            | Self::Protocol(source)
            | Self::Internal(source)
            | Self::Lifecycle { source, .. } => source.fmt(formatter),
        }
    }
}

impl std::error::Error for DaemonError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Persistence { source, .. }
            | Self::Validation(source)
            | Self::Protocol(source)
            | Self::Internal(source)
            | Self::Lifecycle { source, .. } => Some(source),
        }
    }
}

struct SocketCleanup {
    path: PathBuf,
    armed: bool,
}

impl SocketCleanup {
    fn new(path: PathBuf) -> Self {
        Self { path, armed: true }
    }

    fn disarmed(path: PathBuf) -> Self {
        Self { path, armed: false }
    }

    fn arm(&mut self) {
        self.armed = true;
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for SocketCleanup {
    fn drop(&mut self) {
        if self.armed {
            let _ = fs::remove_file(&self.path);
        }
    }
}

fn secure_runtime_dir(path: &Path) -> io::Result<()> {
    fs::create_dir_all(path)?;
    let metadata = fs::symlink_metadata(path)?;
    // `geteuid` has no arguments, pointers, or caller safety requirements.
    if !metadata.is_dir() || metadata.uid() != unsafe { libc::geteuid() } {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "boomux runtime path is not an owned directory",
        ));
    }
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
}

fn capture_current_environment() -> UnixEnvironment {
    UnixEnvironment {
        variables: env::vars_os()
            .map(|(name, value)| protocol::UnixEnvironmentVariable {
                name: name.into_vec(),
                value: value.into_vec(),
            })
            .collect(),
    }
}

fn acquire_daemon_lock(runtime_dir: &Path) -> io::Result<File> {
    let lock_file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(runtime_dir.join("daemon.lock"))?;
    // The descriptor remains open for the daemon lifetime and `flock` takes no
    // ownership of the pointer-free integer arguments.
    if unsafe { libc::flock(lock_file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == -1 {
        let error = io::Error::last_os_error();
        return Err(if matches!(error.raw_os_error(), Some(libc::EWOULDBLOCK)) {
            io::Error::new(io::ErrorKind::AddrInUse, "boomux daemon is already running")
        } else {
            error
        });
    }
    Ok(lock_file)
}

fn launch_replacement(
    listener: &UnixListener,
    daemon_lock: &File,
    registry: &DaemonService,
    notification_settings: Option<NotificationDeliverySettings>,
    startup_environment: Option<UnixEnvironment>,
) -> DaemonResult<()> {
    let _mutation = lock(&registry.mutation_lock)?;
    registry.ensure_running()?;
    registry.runtimes.begin_stopping();
    registry.events.notify();
    registry.notify_output_waiters();
    let mut paused = Vec::new();
    let result = (|| {
        registry.remote_attachments.quiesce()?;
        let shells = registry.durable.shells()?;
        let prepared = registry.runtimes.prepare_handoff(shells)?;
        paused = prepared.paused;
        let transfers = prepared.runtimes;
        let exited = prepared.exited;
        let published = registry.flush_pending()?;
        let event_stream = registry.events.manifest()?;
        if published {
            registry.events.notify();
        }
        let state_lock = registry.state_lock_descriptor()?;
        let focused_terminal = registry.focused_terminal_for_handoff()?;
        launch_replacement_process(
            listener.as_fd(),
            daemon_lock.as_fd(),
            state_lock,
            &transfers,
            &exited,
            &event_stream,
            ReplacementOptions {
                focused_terminal,
                notification_settings,
                startup_environment,
            },
        )
        .map_err(DaemonError::from)
    })();
    if result.is_err() {
        registry.runtimes.cancel_stopping();
        for runtime in paused {
            let _ = registry.runtimes.resume_reader(&runtime);
        }
    }
    result
}

struct ReplacementOptions {
    focused_terminal: Option<FocusedTerminalSnapshot>,
    notification_settings: Option<NotificationDeliverySettings>,
    startup_environment: Option<UnixEnvironment>,
}

fn launch_replacement_process(
    listener: BorrowedFd<'_>,
    runtime_lock: BorrowedFd<'_>,
    state_lock: BorrowedFd<'_>,
    runtimes: &[OutgoingRuntime],
    exited: &[OutgoingExited],
    event_stream: &handoff::EventStreamManifest,
    options: ReplacementOptions,
) -> io::Result<()> {
    let ReplacementOptions {
        focused_terminal,
        notification_settings,
        startup_environment,
    } = options;
    let (mut channel, child_channel) = UnixStream::pair()?;
    let child_channel_fd = child_channel.as_raw_fd();
    let mut command = Command::new(replacement_executable()?);
    if let Some(environment) = startup_environment {
        command.env_clear();
        for variable in environment.variables {
            command.env(
                std::ffi::OsString::from_vec(variable.name),
                std::ffi::OsString::from_vec(variable.value),
            );
        }
    }
    command
        .args([
            "daemon",
            "receive-handoff",
            "--channel",
            &handoff::CHANNEL_FD.to_string(),
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    // Only async-signal-safe descriptor operations run between fork and exec.
    unsafe {
        command.pre_exec(move || {
            if libc::dup2(child_channel_fd, handoff::CHANNEL_FD) == -1
                || libc::fcntl(handoff::CHANNEL_FD, libc::F_SETFD, 0) == -1
            {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut replacement = command.spawn()?;
    drop(child_channel);
    channel.set_read_timeout(Some(HANDSHAKE_TIMEOUT))?;
    channel.set_write_timeout(Some(HANDSHAKE_TIMEOUT))?;

    let result = (|| {
        channel.write_all(handoff::HEADER)?;
        protocol::write_message(
            &mut channel,
            &handoff::Manifest {
                runtimes: runtimes
                    .iter()
                    .map(|runtime| runtime.manifest.clone())
                    .collect(),
                exited: exited.iter().map(|shell| shell.manifest.clone()).collect(),
                event_stream: event_stream.clone(),
                notifications: notification_settings.map(Into::into),
                focused_terminal,
            },
        )?;
        send_descriptor(&channel, listener, handoff::LISTENER_MARKER)?;
        send_descriptor(&channel, runtime_lock, handoff::RUNTIME_LOCK_MARKER)?;
        send_descriptor(&channel, state_lock, handoff::STATE_LOCK_MARKER)?;
        for runtime in runtimes {
            send_descriptor(&channel, runtime.pty.as_fd(), handoff::PTY_MARKER)?;
            send_descriptor(&channel, runtime.pidfd.as_fd(), handoff::PIDFD_MARKER)?;
            protocol::write_message(&mut channel, &runtime.reconstruction)?;
        }
        for shell in exited {
            protocol::write_message(&mut channel, &shell.reconstruction)?;
        }
        let mut acknowledgement = [0];
        channel.read_exact(&mut acknowledgement)?;
        if acknowledgement[0] != handoff::READY {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "replacement daemon did not acknowledge bootstrap readiness",
            ));
        }
        channel.write_all(&[handoff::COMMIT])?;
        channel.read_exact(&mut acknowledgement)?;
        if acknowledgement[0] != handoff::PREPARED {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "replacement daemon did not prepare ownership",
            ));
        }
        // A successful one-byte FINALIZE write is the irreversible boundary.
        // From this point the old daemon must exit even if the final ACK is lost.
        channel.write_all(&[handoff::FINALIZE])?;
        let _ = channel.read_exact(&mut acknowledgement);
        Ok(())
    })();
    if result.is_err() {
        let _ = channel.write_all(&[handoff::ABORT]);
        let _ = replacement.kill();
        let _ = replacement.wait();
    }
    result
}

fn replacement_executable() -> io::Result<PathBuf> {
    let current = env::current_exe()?;
    Ok(select_replacement_executable(
        current,
        env::args_os().next().map(PathBuf::from),
    ))
}

fn select_replacement_executable(current: PathBuf, argument_zero: Option<PathBuf>) -> PathBuf {
    if current.exists() {
        return current;
    }
    if let Some(installed) = current
        .to_str()
        .and_then(|path| path.strip_suffix(" (deleted)"))
        .map(PathBuf::from)
        .filter(|path| path.exists())
    {
        return installed;
    }
    if let Some(argument_zero) = argument_zero
        && argument_zero.is_absolute()
        && argument_zero.exists()
    {
        return argument_zero;
    }
    current
}

fn handle_connection(
    stream: UnixStream,
    registry: Arc<DaemonService>,
    shutdown: Arc<AtomicBool>,
    transition: Arc<AtomicU8>,
    restart_sender: mpsc::Sender<RestartRequest>,
) -> io::Result<()> {
    handle_connection_inner(
        stream,
        registry,
        shutdown,
        transition,
        restart_sender,
        true,
        None,
    )
}

fn handle_connection_inner(
    mut stream: UnixStream,
    registry: Arc<DaemonService>,
    shutdown: Arc<AtomicBool>,
    transition: Arc<AtomicU8>,
    restart_sender: mpsc::Sender<RestartRequest>,
    allow_federation_upgrade: bool,
    federation_lease: Option<NodeIdentityLease>,
) -> io::Result<()> {
    stream.set_read_timeout(Some(HANDSHAKE_TIMEOUT))?;
    let request: Envelope<Request> = protocol::read_message(&mut stream)?;
    stream.set_read_timeout(None)?;
    if !(protocol::MIN_PROTOCOL_VERSION..=protocol::PROTOCOL_VERSION).contains(&request.version) {
        return send_response(
            &mut stream,
            protocol::PROTOCOL_VERSION,
            DaemonError::protocol(format!(
                "protocol version {} is unsupported; expected {}",
                request.version,
                protocol::PROTOCOL_VERSION
            ))
            .into_response(),
        );
    }
    let response_version = request.version;
    if let Some(feature) = request.message.required_feature()
        && !feature.is_supported_by(response_version)
    {
        return send_response(
            &mut stream,
            response_version,
            DaemonError::protocol(unsupported_request_message(feature)).into_response(),
        );
    }

    if matches!(&request.message, Request::OpenFederationChannel) {
        if !allow_federation_upgrade {
            return send_response(
                &mut stream,
                response_version,
                DaemonError::validation("federation channel is already open").into_response(),
            );
        }
        let Some(identity) = registry.node_identity.as_ref() else {
            return send_response(
                &mut stream,
                response_version,
                DaemonError::lifecycle(
                    ErrorCode::NodeIdentityUnavailable,
                    "Boomux Node identity is unavailable",
                )
                .into_response(),
            );
        };
        let lease = match identity.admit() {
            Ok(lease) => lease,
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                return send_response(
                    &mut stream,
                    response_version,
                    DaemonError::from(error).into_response(),
                );
            }
            Err(error) => return Err(error),
        };
        send_response(
            &mut stream,
            response_version,
            Response::FederationChannel {
                node_id: identity.id()?,
            },
        )?;
        stream.set_write_timeout(None)?;
        return handle_connection_inner(
            stream,
            registry,
            shutdown,
            transition,
            restart_sender,
            false,
            Some(lease),
        );
    }

    let _federation_lease = federation_lease;

    if let Request::RekeyNode { expected_node_id } = &request.message {
        if _federation_lease.is_some() {
            return send_response(
                &mut stream,
                response_version,
                DaemonError::validation("Node rekey cannot use a federation channel")
                    .into_response(),
            );
        }
        if transition
            .compare_exchange(
                TRANSITION_IDLE,
                TRANSITION_REKEY,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_err()
        {
            return send_response(
                &mut stream,
                response_version,
                DaemonError::lifecycle(
                    ErrorCode::Busy,
                    "another daemon transition is already in progress",
                )
                .into_response(),
            );
        }
        let response = match registry.node_identity.as_ref() {
            Some(identity) => match identity.rekey(expected_node_id, NODE_REKEY_DRAIN_TIMEOUT) {
                Ok(node_id) => Response::NodeIdentity { node_id },
                Err(error) if error.kind() == io::ErrorKind::TimedOut => {
                    DaemonError::lifecycle(ErrorCode::Busy, error.to_string()).into_response()
                }
                Err(error) => DaemonError::from(error).into_response(),
            },
            None => DaemonError::lifecycle(
                ErrorCode::NodeIdentityUnavailable,
                "Boomux Node identity is unavailable",
            )
            .into_response(),
        };
        transition.store(TRANSITION_IDLE, Ordering::Release);
        return send_response(&mut stream, response_version, response);
    }

    if let Request::AttachNode {
        identity,
        takeover,
        restart_exited,
        expected_run_id,
        profile,
    } = request.message
    {
        return registry.handle_remote_attach(
            stream,
            response_version,
            identity,
            takeover,
            restart_exited,
            expected_run_id,
            profile,
        );
    }

    if let Request::Attach {
        shell_id,
        takeover,
        restart_exited,
        expected_run_id,
        profile,
        environment,
        owner_environment,
    } = request.message
    {
        return registry.runtimes.handle_attach(
            stream,
            response_version,
            &registry,
            &shell_id,
            AttachRequestOptions {
                takeover,
                restart_exited,
                expected_run_id,
                profile,
                environment,
                owner_environment,
            },
        );
    }
    if matches!(request.message, Request::Shutdown) {
        if transition
            .compare_exchange(
                TRANSITION_IDLE,
                TRANSITION_SHUTDOWN,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_err()
        {
            return send_response(
                &mut stream,
                response_version,
                DaemonError::lifecycle(
                    ErrorCode::Busy,
                    "another daemon transition is already in progress",
                )
                .into_response(),
            );
        }
        registry.stop_scheduler()?;
        registry.stop_node_projection_workers()?;
        return match registry.shutdown() {
            Ok(()) => {
                shutdown.store(true, Ordering::Release);
                send_response(&mut stream, response_version, Response::Ok)
            }
            Err(error) => {
                transition.store(TRANSITION_IDLE, Ordering::Release);
                let _ = registry.start_scheduler();
                let _ = registry.start_node_projection_workers();
                send_response(
                    &mut stream,
                    response_version,
                    DaemonError::lifecycle(
                        error.wire_code(),
                        format!("could not stop Boomux daemon: {error}"),
                    )
                    .into_response(),
                )
            }
        };
    }
    let restart_settings = match &request.message {
        Request::Restart => Some((None, None)),
        Request::RestartWithNotificationConfig {
            notifications,
            environment,
        } => {
            if !(1..=MAX_SCHEDULED_EXECUTION_CONCURRENCY)
                .contains(&notifications.max_scheduled_execution_concurrency)
            {
                return send_response(
                    &mut stream,
                    response_version,
                    DaemonError::validation(format!(
                        "scheduling max_concurrent must be between 1 and {MAX_SCHEDULED_EXECUTION_CONCURRENCY}"
                    ))
                    .into_response(),
                );
            }
            if let Some(environment) = environment
                && let Err(error) = validate_unix_environment(environment)
            {
                return send_response(
                    &mut stream,
                    response_version,
                    DaemonError::from(error).into_response(),
                );
            }
            Some((Some(notifications.clone().into()), environment.clone()))
        }
        _ => None,
    };
    if let Some((notification_settings, startup_environment)) = restart_settings {
        if transition
            .compare_exchange(
                TRANSITION_IDLE,
                TRANSITION_RESTART,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_err()
        {
            return send_response(
                &mut stream,
                response_version,
                DaemonError::lifecycle(ErrorCode::Busy, "daemon restart is already in progress")
                    .into_response(),
            );
        }
        let (reply, response) = mpsc::sync_channel(1);
        if restart_sender
            .send(RestartRequest {
                reply,
                notification_settings,
                startup_environment,
            })
            .is_err()
        {
            transition.store(TRANSITION_IDLE, Ordering::Release);
            return Err(io::Error::other("daemon restart coordinator stopped"));
        }
        return match response.recv_timeout(RESTART_TIMEOUT) {
            Ok(Ok(())) => send_response(&mut stream, response_version, Response::Ok),
            Ok(Err(error)) => send_response(&mut stream, response_version, error.into_response()),
            Err(error) => send_response(
                &mut stream,
                response_version,
                DaemonError::lifecycle(
                    ErrorCode::Timeout,
                    format!("daemon restart timed out: {error}"),
                )
                .into_response(),
            ),
        };
    }

    let schedule_semantics_changed = matches!(
        &request.message,
        Request::CreateAgentSchedule { .. }
            | Request::PauseAgentSchedule { .. }
            | Request::ResumeAgentSchedule { .. }
            | Request::RemoveAgentSchedule { .. }
            | Request::GuardedPauseAgentSchedule { .. }
            | Request::GuardedResumeAgentSchedule { .. }
            | Request::GuardedRemoveAgentSchedule { .. }
    );
    let response = match registry.dispatch_arc(request.message, response_version) {
        Ok(response) => response,
        Err(error) => error.into_response(),
    };
    if schedule_semantics_changed && !matches!(response, Response::Error { .. }) {
        registry.wake_scheduler();
    }
    send_response(
        &mut stream,
        response_version,
        response_for_version_with_schedule_shells(
            response,
            response_version,
            &registry
                .schedule_shell_ids_for_downgrade()
                .unwrap_or_default(),
        ),
    )
}

fn validate_notification_delivery_settings(
    settings: &NotificationDeliverySettings,
) -> io::Result<()> {
    if (1..=MAX_SCHEDULED_EXECUTION_CONCURRENCY)
        .contains(&settings.max_scheduled_execution_concurrency)
    {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "scheduling max_concurrent must be between 1 and {MAX_SCHEDULED_EXECUTION_CONCURRENCY}"
            ),
        ))
    }
}

fn unsupported_request_message(feature: protocol::ProtocolFeature) -> String {
    format!(
        "{} requires daemon protocol {}",
        feature.requirement(),
        feature.minimum_version()
    )
}

fn node_registration_error(error: io::Error) -> DaemonError {
    if error.kind() == io::ErrorKind::Unsupported {
        return DaemonError::lifecycle(ErrorCode::NodeRegistrationUnavailable, error.to_string());
    }
    if error.kind() == io::ErrorKind::PermissionDenied
        && error.to_string().contains("different Boomux Node identity")
    {
        return DaemonError::lifecycle(ErrorCode::NodeIdentityChanged, error.to_string());
    }
    if error.kind() == io::ErrorKind::InvalidInput
        && error.to_string().contains("registration revision changed")
    {
        return DaemonError::lifecycle(ErrorCode::RevisionChanged, error.to_string());
    }
    if error.kind() == io::ErrorKind::TimedOut {
        return DaemonError::lifecycle(ErrorCode::Busy, error.to_string());
    }
    if matches!(
        error.kind(),
        io::ErrorKind::PermissionDenied | io::ErrorKind::Other
    ) {
        return DaemonError::persistence(error);
    }
    DaemonError::from(error)
}

#[cfg(test)]
fn response_for_version(response: Response, version: u32) -> Response {
    response_for_version_with_schedule_shells(response, version, &HashSet::new())
}

fn response_for_version_with_schedule_shells(
    response: Response,
    version: u32,
    schedule_shell_ids: &HashSet<String>,
) -> Response {
    let mut response = response;
    if !protocol::ProtocolFeature::NodeProjectionSync.is_supported_by(version)
        && let Response::Events { events, .. } = &mut response
    {
        events.retain(|event| !matches!(event.kind, DaemonEventKind::NodeProjectionChanged { .. }));
    }
    if !protocol::ProtocolFeature::ScheduledExecutionObservation.is_supported_by(version) {
        match &mut response {
            Response::ScheduledExecution {
                next_occurrence, ..
            } => *next_occurrence = None,
            Response::ScheduledExecutions {
                schedules,
                schedule_limit,
                schedules_truncated,
                ..
            } => {
                schedules.clear();
                *schedule_limit = 0;
                *schedules_truncated = false;
            }
            _ => {}
        }
    }
    if !protocol::ProtocolFeature::TimedScheduling.is_supported_by(version) {
        remove_timed_scheduling(&mut response);
    }
    if !protocol::ProtocolFeature::ScheduledExecutions.is_supported_by(version) {
        if let Response::Events { events, .. } = &mut response {
            events.retain(|event| {
                !matches!(
                    &event.kind,
                    DaemonEventKind::ScheduledExecutionCreated { .. }
                        | DaemonEventKind::ScheduledExecutionChanged { .. }
                ) && !event_shell_id(&event.kind)
                    .is_some_and(|shell_id| schedule_shell_ids.contains(shell_id))
            });
        }
        let hide_schedule_shells = |workspace: &mut WorkspaceSnapshot| {
            workspace
                .shells
                .retain(|shell| matches!(shell.owner, ShellOwner::User));
            for schedule in &mut workspace.schedules {
                schedule.execution_shell_id = None;
            }
        };
        match &mut response {
            Response::Snapshot { snapshot } => {
                for workspace in &mut snapshot.workspaces {
                    hide_schedule_shells(workspace);
                }
            }
            Response::Workspace { workspace } => {
                hide_schedule_shells(workspace);
            }
            Response::Shell { shell } if !matches!(shell.owner, ShellOwner::User) => {
                response = Response::Error {
                    code: Some(ErrorCode::NotFound),
                    message: format!("shell not found: {}", shell.id),
                };
            }
            Response::Events {
                snapshot: Some(snapshot),
                ..
            } => {
                for workspace in &mut snapshot.workspaces {
                    hide_schedule_shells(workspace);
                }
            }
            _ => {}
        }
    }
    if !protocol::ProtocolFeature::AgentSchedules.is_supported_by(version) {
        remove_agent_schedules(&mut response);
    }
    if !protocol::ProtocolFeature::AgentScheduleEditing.is_supported_by(version)
        && let Response::Events { events, .. } = &mut response
    {
        events.retain(|event| !matches!(event.kind, DaemonEventKind::AgentScheduleUpdated { .. }));
    }
    if !protocol::ProtocolFeature::WorkspaceDefaultCwd.is_supported_by(version) {
        remove_workspace_default_cwds(&mut response);
    }
    if !protocol::ProtocolFeature::FocusedTerminal.is_supported_by(version) {
        remove_focused_terminal(&mut response);
    }
    if !protocol::ProtocolFeature::PersistentAgentAttention.is_supported_by(version) {
        remove_agent_attention(&mut response);
        if let Response::Events { events, .. } = &mut response {
            events.retain(|event| {
                !matches!(
                    event.kind,
                    DaemonEventKind::AgentAttentionAcknowledged { .. }
                )
            });
        }
    }
    if !protocol::ProtocolFeature::DurableAgentCwd.is_supported_by(version) {
        remove_agent_cwds(&mut response);
    }
    if !protocol::ProtocolFeature::InactiveAgentState.is_supported_by(version) {
        downgrade_inactive_agent_states(&mut response);
    }
    if protocol::ProtocolFeature::AgentInstances.is_supported_by(version) {
        return response;
    }
    match response {
        Response::Snapshot { mut snapshot } => {
            remove_agent_snapshots(&mut snapshot);
            Response::Snapshot { snapshot }
        }
        Response::Workspace { mut workspace } => {
            workspace.agents.clear();
            Response::Workspace { workspace }
        }
        Response::Events {
            stream_id,
            cursor,
            mut snapshot,
            mut events,
        } => {
            if let Some(snapshot) = &mut snapshot {
                remove_agent_snapshots(snapshot);
            }
            events.retain(|event| {
                !matches!(
                    event.kind,
                    DaemonEventKind::AgentRegistered { .. }
                        | DaemonEventKind::AgentStateChanged { .. }
                        | DaemonEventKind::AgentCompleted { .. }
                )
            });
            if !protocol::ProtocolFeature::WorkspaceLaunchers.is_supported_by(version) {
                events.retain(|event| {
                    !matches!(
                        event.kind,
                        DaemonEventKind::LauncherCreated { .. }
                            | DaemonEventKind::LauncherRenamed { .. }
                            | DaemonEventKind::LauncherRemoved { .. }
                    )
                });
            }
            Response::Events {
                stream_id,
                cursor,
                snapshot,
                events,
            }
        }
        response => response,
    }
}

fn remove_timed_scheduling(response: &mut Response) {
    let downgrade_schedule = |schedule: &mut AgentScheduleSnapshot| {
        schedule.next_occurrence = None;
    };
    let downgrade_snapshot = |snapshot: &mut Snapshot| {
        snapshot.scheduler = None;
        for workspace in &mut snapshot.workspaces {
            for schedule in &mut workspace.schedules {
                downgrade_schedule(schedule);
            }
        }
    };
    match response {
        Response::Snapshot { snapshot } => downgrade_snapshot(snapshot),
        Response::Workspace { workspace } => {
            for schedule in &mut workspace.schedules {
                downgrade_schedule(schedule);
            }
        }
        Response::AgentSchedule { schedule } => downgrade_schedule(schedule),
        Response::AgentScheduleInspection { inspection } => {
            downgrade_schedule(&mut inspection.schedule);
        }
        Response::ScheduledExecution { execution, .. }
            if execution.dispatch_kind == ScheduledExecutionDispatchKind::Timed
                || execution.state == ScheduledExecutionState::Skipped =>
        {
            *response = Response::Error {
                code: Some(ErrorCode::NotFound),
                message: format!("scheduled execution not found: {}", execution.id),
            };
        }
        Response::ScheduledExecutions { executions, .. } => executions.retain(|execution| {
            execution.dispatch_kind == ScheduledExecutionDispatchKind::Manual
                && execution.state != ScheduledExecutionState::Skipped
        }),
        Response::Events {
            snapshot, events, ..
        } => {
            if let Some(snapshot) = snapshot {
                downgrade_snapshot(snapshot);
            }
            events.retain(|event| match &event.kind {
                DaemonEventKind::ScheduledExecutionCreated { execution, .. }
                | DaemonEventKind::ScheduledExecutionChanged { execution, .. } => {
                    execution.dispatch_kind == ScheduledExecutionDispatchKind::Manual
                        && execution.state != ScheduledExecutionState::Skipped
                }
                _ => true,
            });
        }
        _ => {}
    }
}

fn event_shell_id(event: &DaemonEventKind) -> Option<&str> {
    match event {
        DaemonEventKind::ShellCreated { shell_id, .. }
        | DaemonEventKind::ShellRenamed { shell_id, .. }
        | DaemonEventKind::ShellClosed { shell_id, .. }
        | DaemonEventKind::RunStarted { shell_id, .. }
        | DaemonEventKind::OutputChanged { shell_id, .. }
        | DaemonEventKind::RunExited { shell_id, .. }
        | DaemonEventKind::AgentRegistered { shell_id, .. }
        | DaemonEventKind::AgentStateChanged { shell_id, .. }
        | DaemonEventKind::AgentCompleted { shell_id, .. }
        | DaemonEventKind::AgentAttentionAcknowledged { shell_id, .. } => Some(shell_id),
        _ => None,
    }
}

fn remove_agent_schedules(response: &mut Response) {
    match response {
        Response::Snapshot { snapshot } => {
            for workspace in &mut snapshot.workspaces {
                workspace.schedules.clear();
            }
        }
        Response::Workspace { workspace } => workspace.schedules.clear(),
        Response::Events {
            snapshot, events, ..
        } => {
            if let Some(snapshot) = snapshot {
                for workspace in &mut snapshot.workspaces {
                    workspace.schedules.clear();
                }
            }
            events.retain(|event| {
                !matches!(
                    event.kind,
                    DaemonEventKind::AgentScheduleCreated { .. }
                        | DaemonEventKind::AgentSchedulePaused { .. }
                        | DaemonEventKind::AgentScheduleResumed { .. }
                        | DaemonEventKind::AgentScheduleUpdated { .. }
                        | DaemonEventKind::AgentScheduleRemoved { .. }
                )
            });
        }
        _ => {}
    }
}

fn remove_workspace_default_cwds(response: &mut Response) {
    match response {
        Response::Snapshot { snapshot } => {
            for workspace in &mut snapshot.workspaces {
                workspace.default_cwd = None;
            }
        }
        Response::Workspace { workspace } => workspace.default_cwd = None,
        Response::Events {
            snapshot: Some(snapshot),
            ..
        } => {
            for workspace in &mut snapshot.workspaces {
                workspace.default_cwd = None;
            }
        }
        _ => {}
    }
}

fn remove_focused_terminal(response: &mut Response) {
    match response {
        Response::Snapshot { snapshot } => snapshot.focused_terminal = None,
        Response::Events {
            snapshot: Some(snapshot),
            ..
        } => snapshot.focused_terminal = None,
        _ => {}
    }
}

fn remove_agent_cwds(response: &mut Response) {
    visit_response_agents(response, &mut |agent| agent.cwd = None);
}

fn remove_agent_attention(response: &mut Response) {
    visit_response_agents(response, &mut |agent| agent.attention = None);
}

fn downgrade_inactive_agent_states(response: &mut Response) {
    visit_response_agents(response, &mut |agent| {
        if agent.observation.state == AgentState::Inactive {
            agent.observation.state = AgentState::Unknown;
        }
    });
}

fn visit_response_agents(
    response: &mut Response,
    visitor: &mut impl FnMut(&mut AgentInstanceSnapshot),
) {
    match response {
        Response::Snapshot { snapshot } => {
            for workspace in &mut snapshot.workspaces {
                for agent in &mut workspace.agents {
                    visitor(agent);
                }
            }
        }
        Response::Workspace { workspace } => {
            for agent in &mut workspace.agents {
                visitor(agent);
            }
        }
        Response::Agent { agent }
        | Response::AgentWait { agent, .. }
        | Response::AgentAttentionAcknowledged { agent, .. } => visitor(agent),
        Response::Events {
            snapshot, events, ..
        } => {
            if let Some(snapshot) = snapshot {
                for workspace in &mut snapshot.workspaces {
                    for agent in &mut workspace.agents {
                        visitor(agent);
                    }
                }
            }
            for event in events {
                match &mut event.kind {
                    DaemonEventKind::AgentRegistered { agent, .. }
                    | DaemonEventKind::AgentStateChanged { agent, .. }
                    | DaemonEventKind::AgentCompleted { agent, .. }
                    | DaemonEventKind::AgentAttentionAcknowledged { agent, .. } => visitor(agent),
                    _ => {}
                }
            }
        }
        _ => {}
    }
}

fn remove_agent_snapshots(snapshot: &mut Snapshot) {
    for workspace in &mut snapshot.workspaces {
        workspace.agents.clear();
    }
}

fn send_response(stream: &mut UnixStream, version: u32, response: Response) -> io::Result<()> {
    stream.set_write_timeout(Some(RESPONSE_WRITE_TIMEOUT))?;
    protocol::write_message(stream, &Envelope::with_version(version, response))
}

fn send_daemon_error(stream: &mut UnixStream, version: u32, error: DaemonError) -> io::Result<()> {
    send_response(stream, version, error.into_response())
}

fn error_response(code: ErrorCode, message: impl Into<String>) -> Response {
    Response::Error {
        message: message.into(),
        code: Some(code),
    }
}

fn operation_is_read(operation: &RoutedOperation) -> bool {
    matches!(
        operation,
        RoutedOperation::GetWorkspace { .. }
            | RoutedOperation::GetShell { .. }
            | RoutedOperation::GetLauncher { .. }
            | RoutedOperation::GetAgent { .. }
            | RoutedOperation::GetAgentSchedule { .. }
            | RoutedOperation::GetScheduledExecution { .. }
    )
}

fn routed_result(response: Response) -> Result<RoutedOperationResult, Box<Response>> {
    match response {
        Response::Workspace { workspace } => Ok(RoutedOperationResult::Workspace { workspace }),
        Response::Shell { shell } => Ok(RoutedOperationResult::Shell { shell }),
        Response::Launcher { launcher } => Ok(RoutedOperationResult::Launcher { launcher }),
        Response::Agent { agent } => Ok(RoutedOperationResult::Agent { agent }),
        Response::AgentSchedule { schedule } => {
            Ok(RoutedOperationResult::AgentSchedule { schedule })
        }
        Response::AgentScheduleInspection { inspection } => {
            Ok(RoutedOperationResult::AgentScheduleInspection { inspection })
        }
        Response::ScheduledExecution {
            execution,
            next_occurrence,
        } => Ok(RoutedOperationResult::ScheduledExecution {
            execution,
            next_occurrence,
        }),
        Response::AgentAttentionAcknowledged { agent, changed } => {
            Ok(RoutedOperationResult::AgentAttentionAcknowledged { agent, changed })
        }
        Response::Ok => Ok(RoutedOperationResult::Ok),
        Response::Error { .. } => Err(Box::new(response)),
        _ => Err(Box::new(error_response(
            ErrorCode::Internal,
            "remote Node returned an unexpected typed response",
        ))),
    }
}

fn routed_postcondition(operation: &RoutedOperation, response: &Response) -> bool {
    match (operation, response) {
        (
            RoutedOperation::RenameWorkspace {
                name,
                expected_revision,
                ..
            },
            Response::Workspace { workspace },
        ) => workspace.name == *name && workspace.revision > *expected_revision,
        (
            RoutedOperation::RenameShell {
                name,
                expected_revision,
                ..
            },
            Response::Shell { shell },
        ) => shell.name == *name && shell.revision > *expected_revision,
        (
            RoutedOperation::RenameLauncher {
                name,
                expected_revision,
                ..
            },
            Response::Launcher { launcher },
        ) => launcher.name == *name && launcher.revision > *expected_revision,
        (
            RoutedOperation::CloseWorkspace { .. }
            | RoutedOperation::CloseShell { .. }
            | RoutedOperation::RemoveLauncher { .. }
            | RoutedOperation::RemoveAgentSchedule { .. },
            Response::Error {
                code: Some(ErrorCode::NotFound),
                ..
            },
        ) => true,
        (
            RoutedOperation::RestartShell {
                expected_revision,
                expected_run_id,
                ..
            },
            Response::Shell { shell },
        ) => {
            shell.revision == *expected_revision
                && shell.status == ShellStatus::Pending
                && shell
                    .run
                    .as_ref()
                    .is_some_and(|run| run.id == *expected_run_id)
        }
        (
            RoutedOperation::PauseAgentSchedule {
                expected_revision, ..
            },
            Response::AgentScheduleInspection { inspection },
        ) => {
            inspection.schedule.revision > *expected_revision
                && inspection.schedule.state == AgentScheduleState::Paused
        }
        (
            RoutedOperation::ResumeAgentSchedule {
                expected_revision, ..
            },
            Response::AgentScheduleInspection { inspection },
        ) => {
            inspection.schedule.revision > *expected_revision
                && inspection.schedule.state == AgentScheduleState::Enabled
        }
        (
            RoutedOperation::UpdateAgentSchedule {
                expected_revision,
                update,
                ..
            },
            Response::AgentScheduleInspection { inspection },
        ) => {
            inspection.schedule.revision > *expected_revision
                && inspection.schedule.name == update.name
                && inspection.schedule.trigger == update.trigger
                && inspection.prompt == update.prompt
        }
        (
            RoutedOperation::CancelScheduledExecution {
                expected_revision, ..
            },
            Response::ScheduledExecution { execution, .. },
        ) => {
            execution.revision > *expected_revision
                && execution.state == ScheduledExecutionState::Cancelled
                && execution.reason == Some(ScheduledExecutionReason::CancelledByUser)
        }
        _ => false,
    }
}

fn proven_routed_result(
    operation: &RoutedOperation,
    response: Response,
) -> Option<RoutedOperationResult> {
    if !routed_postcondition(operation, &response) {
        return None;
    }
    match (operation, response) {
        (
            RoutedOperation::CloseWorkspace { .. }
            | RoutedOperation::CloseShell { .. }
            | RoutedOperation::RemoveLauncher { .. }
            | RoutedOperation::RemoveAgentSchedule { .. },
            Response::Error { .. },
        ) => Some(RoutedOperationResult::Ok),
        (
            RoutedOperation::PauseAgentSchedule { .. }
            | RoutedOperation::ResumeAgentSchedule { .. }
            | RoutedOperation::UpdateAgentSchedule { .. },
            Response::AgentScheduleInspection { inspection },
        ) => Some(RoutedOperationResult::AgentSchedule {
            schedule: inspection.schedule,
        }),
        (_, response) => routed_result(response).ok(),
    }
}

fn send_registered_node_request(
    registration: &crate::protocol::NodeRegistrationSnapshot,
    request: Request,
) -> io::Result<Response> {
    let target = SshTarget::parse(registration.target.clone())?;
    let helper = match ssh_bootstrap::plan_remote_bootstrap(
        target.clone(),
        SshAuthenticationMode::Batch,
        Duration::from_secs(2),
    )? {
        RemoteBootstrapPlan::Ready(helper) => helper,
        RemoteBootstrapPlan::Install(_) => {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "remote helper requires installation",
            ));
        }
    };
    if helper.handshake.node_id != registration.node_id {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "remote Node identity changed",
        ));
    }
    if !protocol::ProtocolFeature::GuardedNodeRouting
        .is_supported_by(helper.handshake.core_protocol_version)
    {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "remote Node does not support guarded routing",
        ));
    }
    let mut remote = ssh_bootstrap::connect_remote(
        target,
        helper,
        SshAuthenticationMode::Batch,
        Duration::from_secs(2),
    )?;
    remote.request(request, Duration::from_secs(2))
}

fn require_guard(actual: u64, expected: u64, resource: &str) -> DaemonResult<()> {
    if actual == expected {
        Ok(())
    } else {
        Err(DaemonError::lifecycle(
            ErrorCode::RevisionAhead,
            format!("{resource} revision is {actual}; guarded operation supplied {expected}"),
        ))
    }
}

fn bump_revision(revision: &Mutex<u64>, resource: &str) -> io::Result<()> {
    let mut revision = lock(revision)?;
    *revision = revision
        .checked_add(1)
        .ok_or_else(|| io::Error::other(format!("{resource} revision exhausted")))?;
    Ok(())
}

fn rollback_bump(revision: &Mutex<u64>) -> io::Result<()> {
    let mut revision = lock(revision)?;
    *revision = revision.saturating_sub(1).max(1);
    Ok(())
}

fn scheduled_execution_response(
    execution: ScheduledExecutionSnapshot,
    response_version: u32,
) -> DaemonResult<Response> {
    if execution.state == ScheduledExecutionState::Skipped
        && !protocol::ProtocolFeature::TimedScheduling.is_supported_by(response_version)
    {
        return Err(DaemonError::lifecycle(
            ErrorCode::Busy,
            "scheduled execution was skipped by the current concurrency policy",
        ));
    }
    Ok(Response::ScheduledExecution {
        execution,
        next_occurrence: None,
    })
}

struct DaemonService {
    node_identity: Option<Arc<NodeIdentityManager>>,
    node_registrations: Option<NodeRegistrationManager>,
    node_projection_cache: Option<NodeProjectionCache>,
    durable: DurableRegistry,
    events: EventStream,
    runtimes: ShellRuntimeManager,
    remote_attachments: RemoteAttachmentManager,
    mutation_lock: Mutex<()>,
    schedule_dispatch_lock: Mutex<()>,
    notification_settings: NotificationDeliverySettings,
    notification_sink: Arc<dyn NotificationSink>,
    cold_recovery_executions: Vec<ScheduledExecutionSnapshot>,
    startup_environment: UnixEnvironment,
    scheduler: SchedulerWorker,
    node_projection_workers: NodeProjectionWorkers,
    clock: Mutex<Arc<dyn SchedulerClock>>,
    #[cfg(test)]
    fail_after_mutation: AtomicBool,
}

#[derive(Default)]
struct RemoteAttachmentManager {
    controllers: Mutex<HashMap<String, RemoteAttachmentController>>,
}

struct RemoteAttachmentController {
    connection: Arc<Mutex<UnixStream>>,
    reconnect_ack: Option<SyncSender<()>>,
}

impl RemoteAttachmentManager {
    fn insert(&self, token: String, connection: UnixStream) -> io::Result<()> {
        lock(&self.controllers)?.insert(
            token,
            RemoteAttachmentController {
                connection: Arc::new(Mutex::new(connection)),
                reconnect_ack: None,
            },
        );
        Ok(())
    }

    fn connection(&self, token: &str) -> io::Result<Option<Arc<Mutex<UnixStream>>>> {
        Ok(lock(&self.controllers)?
            .get(token)
            .map(|controller| Arc::clone(&controller.connection)))
    }

    fn acknowledge(&self, token: &str) -> io::Result<bool> {
        let acknowledge = lock(&self.controllers)?
            .get_mut(token)
            .and_then(|controller| controller.reconnect_ack.take());
        if let Some(acknowledge) = acknowledge {
            let _ = acknowledge.send(());
            return Ok(true);
        }
        Ok(false)
    }

    fn remove(&self, token: &str) {
        if let Ok(mut controllers) = self.controllers.lock() {
            controllers.remove(token);
        }
    }

    fn quiesce(&self) -> io::Result<()> {
        let mut acknowledgements = Vec::new();
        {
            let mut controllers = lock(&self.controllers)?;
            for controller in controllers.values_mut() {
                let (acknowledge, acknowledged) = mpsc::sync_channel(1);
                controller.reconnect_ack = Some(acknowledge);
                AttachFrame::Reconnect.write_to(&mut *lock(&controller.connection)?)?;
                acknowledgements.push(acknowledged);
            }
        }
        for acknowledged in acknowledgements {
            acknowledged.recv_timeout(HANDSHAKE_TIMEOUT).map_err(|_| {
                io::Error::new(
                    io::ErrorKind::TimedOut,
                    "remote attachment did not acknowledge local reconnect",
                )
            })?;
        }
        Ok(())
    }
}

trait SchedulerClock: Send + Sync {
    fn now_ms(&self) -> u64;

    fn take_tick(&self) -> io::Result<Option<u64>> {
        Ok(None)
    }

    fn acknowledge(&self, _generation: u64) -> io::Result<()> {
        Ok(())
    }

    fn record_attempt(&self) -> io::Result<()> {
        Ok(())
    }

    fn record_failure_diagnostic(&self) -> io::Result<()> {
        Ok(())
    }

    fn resample_interval(&self) -> Duration {
        Duration::from_secs(60)
    }
}

struct SystemSchedulerClock;

impl SchedulerClock for SystemSchedulerClock {
    fn now_ms(&self) -> u64 {
        unix_time_ms()
    }
}

#[cfg(debug_assertions)]
struct NativeTestSchedulerClock {
    directory: PathBuf,
    now_ms: AtomicU64,
    generation: AtomicU64,
}

#[cfg(debug_assertions)]
impl NativeTestSchedulerClock {
    fn new(directory: PathBuf) -> io::Result<Self> {
        let (generation, now_ms) = read_native_clock_tick(&directory)?;
        Ok(Self {
            directory,
            now_ms: AtomicU64::new(now_ms),
            generation: AtomicU64::new(generation.saturating_sub(1)),
        })
    }
}

#[cfg(debug_assertions)]
impl SchedulerClock for NativeTestSchedulerClock {
    fn now_ms(&self) -> u64 {
        self.now_ms.load(Ordering::Acquire)
    }

    fn take_tick(&self) -> io::Result<Option<u64>> {
        let (generation, now_ms) = read_native_clock_tick(&self.directory)?;
        if generation <= self.generation.load(Ordering::Acquire) {
            return Ok(None);
        }
        self.now_ms.store(now_ms, Ordering::Release);
        self.generation.store(generation, Ordering::Release);
        write_native_clock_marker(&self.directory.join("seen"), &generation.to_string())?;
        Ok(Some(generation))
    }

    fn acknowledge(&self, generation: u64) -> io::Result<()> {
        write_native_clock_marker(&self.directory.join("ack"), &generation.to_string())
    }

    fn record_attempt(&self) -> io::Result<()> {
        let path = self.directory.join("attempts");
        let attempts = match read_native_clock_marker(&path) {
            Ok(value) => value.trim().parse::<u64>().unwrap_or(0),
            Err(error) if error.kind() == io::ErrorKind::NotFound => 0,
            Err(error) => return Err(error),
        };
        write_native_clock_marker(&path, &attempts.saturating_add(1).to_string())
    }

    fn record_failure_diagnostic(&self) -> io::Result<()> {
        let path = self.directory.join("diagnostics");
        let diagnostics = match read_native_clock_marker(&path) {
            Ok(value) => value.trim().parse::<u64>().unwrap_or(0),
            Err(error) if error.kind() == io::ErrorKind::NotFound => 0,
            Err(error) => return Err(error),
        };
        write_native_clock_marker(&path, &diagnostics.saturating_add(1).to_string())
    }

    fn resample_interval(&self) -> Duration {
        Duration::from_millis(10)
    }
}

#[cfg(debug_assertions)]
fn read_native_clock_tick(directory: &Path) -> io::Result<(u64, u64)> {
    let value = read_native_clock_marker(&directory.join("tick"))?;
    let (generation, now_ms) = value.trim().split_once(' ').ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidData, "native clock tick is malformed")
    })?;
    Ok((
        generation
            .parse()
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "invalid tick generation"))?,
        now_ms
            .parse()
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "invalid tick time"))?,
    ))
}

#[cfg(debug_assertions)]
fn read_native_clock_marker(path: &Path) -> io::Result<String> {
    let mut file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)?;
    validate_native_clock_marker(path, &file.metadata()?)?;
    let mut value = String::new();
    file.read_to_string(&mut value)?;
    Ok(value)
}

#[cfg(debug_assertions)]
fn write_native_clock_marker(path: &Path, value: &str) -> io::Result<()> {
    let mut file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)?;
    validate_native_clock_marker(path, &file.metadata()?)?;
    file.write_all(value.as_bytes())
}

#[cfg(debug_assertions)]
fn validate_native_clock_marker(path: &Path, metadata: &fs::Metadata) -> io::Result<()> {
    if !metadata.is_file()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.mode() & 0o077 != 0
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("native clock marker is unsafe: {}", path.display()),
        ));
    }
    Ok(())
}

#[derive(Default)]
struct SchedulerWorker {
    state: Mutex<SchedulerWorkerState>,
    changed: Condvar,
}

#[derive(Default)]
struct SchedulerWorkerState {
    stop: bool,
    wake: bool,
    running: bool,
    healthy: bool,
    handle: Option<thread::JoinHandle<()>>,
}

#[derive(Default)]
struct NodeProjectionWorkers {
    stop: Arc<AtomicBool>,
    handles: Mutex<HashMap<String, thread::JoinHandle<()>>>,
}

struct DurableRegistry {
    state: Mutex<DurableState>,
    store: Option<Arc<StateStore>>,
    persistence_writer: Option<PersistenceWriter>,
    persist_lock: Mutex<()>,
    persistence_dirty: AtomicBool,
    persistence_revision: AtomicU64,
}

#[derive(Default)]
struct ShellRuntimeManager {
    focus: Mutex<FocusState>,
    stopping: AtomicBool,
}

#[derive(Default)]
struct FocusState {
    revision: u64,
    focused_terminal: Option<FocusedTerminalSnapshot>,
}

enum DurableMutation<T> {
    Changed(T, Vec<DaemonEventKind>),
    Unchanged(T),
}

impl<T> DurableMutation<T> {
    fn map<U>(self, map: impl FnOnce(T) -> U) -> DurableMutation<U> {
        match self {
            Self::Changed(value, events) => DurableMutation::Changed(map(value), events),
            Self::Unchanged(value) => DurableMutation::Unchanged(map(value)),
        }
    }
}

#[derive(Default)]
struct DurableUndoLog {
    records: Vec<DurableUndo>,
}

impl DurableUndoLog {
    fn record(&mut self, undo: DurableUndo) {
        self.records.push(undo);
    }

    fn is_empty(&self) -> bool {
        self.records.is_empty()
    }

    fn previous_agent_state(&self, agent_id: &str) -> Option<AgentState> {
        self.records.iter().rev().find_map(|undo| match undo {
            DurableUndo::AgentState { agent, previous } if agent.id == agent_id => {
                Some(previous.observation.state)
            }
            _ => None,
        })
    }

    fn rollback(self, registry: &DurableRegistry) -> io::Result<()> {
        let mut failure = None;
        for undo in self.records.into_iter().rev() {
            if let Err(error) = registry.rollback(undo)
                && failure.is_none()
            {
                failure = Some(error);
            }
        }
        failure.map_or(Ok(()), Err)
    }
}

enum DurableUndo {
    CreatedWorkspace {
        workspace: Arc<Workspace>,
        shells: Vec<Arc<Shell>>,
    },
    CreatedShell {
        workspace: Arc<Workspace>,
        shell: Arc<Shell>,
    },
    CreatedLauncher {
        workspace: Arc<Workspace>,
        launcher: Arc<WorkspaceLauncher>,
    },
    CreatedSchedule {
        workspace: Arc<Workspace>,
        schedule: Arc<AgentSchedule>,
    },
    RegisteredAgent {
        workspace: Arc<Workspace>,
        agent: Arc<AgentInstance>,
    },
    RenamedWorkspace {
        workspace: Arc<Workspace>,
        previous: String,
        previous_revision: u64,
    },
    RenamedShell {
        shell: Arc<Shell>,
        previous: String,
        previous_revision: u64,
    },
    RenamedLauncher {
        launcher: Arc<WorkspaceLauncher>,
        previous: String,
        previous_revision: u64,
    },
    AgentState {
        agent: Arc<AgentInstance>,
        previous: AgentInstanceState,
    },
    ScheduleState {
        schedule: Arc<AgentSchedule>,
        previous: AgentScheduleMutableState,
    },
    ScheduleExecutions {
        schedule: Arc<AgentSchedule>,
        previous: Vec<Arc<ScheduledExecution>>,
        previous_dispatch_key_filter: Vec<u8>,
        execution: Option<(Arc<ScheduledExecution>, ScheduledExecutionMutableState)>,
    },
    RemovedLauncher {
        workspace: Arc<Workspace>,
        launcher: Arc<WorkspaceLauncher>,
        index: usize,
    },
    RemovedSchedule {
        workspace: Arc<Workspace>,
        schedule: Arc<AgentSchedule>,
        index: usize,
    },
    RemovedShell {
        workspace: Arc<Workspace>,
        shell: Arc<Shell>,
        index: usize,
    },
    RemovedWorkspace {
        workspace: Arc<Workspace>,
        shells: Vec<Arc<Shell>>,
        launchers: Vec<Arc<WorkspaceLauncher>>,
        agents: Vec<Arc<AgentInstance>>,
        schedules: Vec<Arc<AgentSchedule>>,
    },
}

#[derive(Default)]
struct TransitionState {
    pending_durable_events: VecDeque<Vec<DaemonEventKind>>,
    persistence_in_flight: bool,
    in_flight_event_count: usize,
    lifecycle_event_reservation: usize,
    pending_runtime_events: VecDeque<DaemonEventKind>,
}

impl TransitionState {
    fn publication_blocked(&self) -> bool {
        self.persistence_in_flight
            || !self.pending_durable_events.is_empty()
            || self.lifecycle_event_reservation != 0
    }

    fn queue_runtime_event(&mut self, event: DaemonEventKind) {
        if let DaemonEventKind::OutputChanged {
            shell_id, run_id, ..
        } = &event
            && let Some(index) = self.pending_runtime_events.iter().position(|pending| {
                matches!(
                    pending,
                    DaemonEventKind::OutputChanged {
                        shell_id: pending_shell_id,
                        run_id: pending_run_id,
                        ..
                    } if pending_shell_id == shell_id && pending_run_id == run_id
                )
            })
        {
            self.pending_runtime_events.remove(index);
        }
        self.pending_runtime_events.push_back(event);
    }

    fn reserved_event_count(&self) -> usize {
        self.in_flight_event_count
            .saturating_add(
                self.pending_durable_events
                    .iter()
                    .map(Vec::len)
                    .sum::<usize>(),
            )
            .saturating_add(self.pending_runtime_events.len())
            .saturating_add(self.lifecycle_event_reservation)
    }
}

struct PersistenceWriter {
    sender: mpsc::Sender<PersistenceWrite>,
    #[cfg(test)]
    fail_next: Arc<AtomicBool>,
}

struct PersistenceWrite {
    generation: PersistenceGeneration,
    completion: SyncSender<io::Result<()>>,
}

struct PersistenceGeneration {
    revision: u64,
    state: PersistedState,
    executions: Vec<ScheduledExecutionSnapshot>,
}

impl PersistenceWriter {
    fn start(store: Arc<StateStore>) -> Self {
        let (sender, receiver) = mpsc::channel::<PersistenceWrite>();
        #[cfg(test)]
        let fail_next = Arc::new(AtomicBool::new(false));
        #[cfg(test)]
        let writer_fail_next = Arc::clone(&fail_next);
        thread::spawn(move || {
            while let Ok(write) = receiver.recv() {
                #[cfg(test)]
                let result = if writer_fail_next.swap(false, Ordering::AcqRel) {
                    Err(io::Error::other("injected persistence failure"))
                } else {
                    store.save(&write.generation.state)
                };
                #[cfg(not(test))]
                let result = store.save(&write.generation.state);
                let _ = write.completion.send(result);
            }
        });
        Self {
            sender,
            #[cfg(test)]
            fail_next,
        }
    }

    fn save(&self, generation: PersistenceGeneration) -> io::Result<()> {
        let (completion, result) = mpsc::sync_channel(1);
        self.sender
            .send(PersistenceWrite {
                generation,
                completion,
            })
            .map_err(|_| io::Error::other("persistence writer stopped"))?;
        result
            .recv()
            .map_err(|_| io::Error::other("persistence writer stopped"))?
    }

    #[cfg(test)]
    fn fail_next(&self) {
        self.fail_next.store(true, Ordering::Release);
    }
}

struct EventStream {
    state: Mutex<EventStreamState>,
    transitions: Mutex<TransitionState>,
    changed: Condvar,
    lifecycle_active: AtomicBool,
}

struct LifecycleActivity<'a> {
    active: &'a AtomicBool,
}

impl<'a> LifecycleActivity<'a> {
    fn begin(active: &'a AtomicBool) -> Self {
        active.store(true, Ordering::Release);
        Self { active }
    }
}

impl Drop for LifecycleActivity<'_> {
    fn drop(&mut self) {
        self.active.store(false, Ordering::Release);
    }
}

struct EventStreamState {
    stream_id: String,
    latest_id: u64,
    events: VecDeque<DaemonEvent>,
    schedule_shell_ids: HashSet<String>,
    committed_executions: HashMap<String, ScheduledExecutionSnapshot>,
}

struct EventTransaction<'a> {
    transition: MutexGuard<'a, TransitionState>,
    events: MutexGuard<'a, EventStreamState>,
    changed: &'a Condvar,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RunExitRecord {
    Recorded,
    Unchanged,
    Deferred,
}

trait EventTransitionAccess {
    fn drain_runtime_events(&mut self) -> Vec<DaemonEventKind>;
    fn queue_durable_batch(&mut self, batch: Vec<DaemonEventKind>) -> bool;
}

impl EventTransaction<'_> {
    fn reserve(&self, count: usize) -> io::Result<()> {
        EventStream::ensure_capacity(&self.events, count)
    }

    fn reserve_with_pending(&self, count: usize) -> io::Result<()> {
        self.reserve(self.transition.reserved_event_count().saturating_add(count))
    }

    fn capacity_is_blocked_only_by_lifecycle_reservation(&self, count: usize) -> bool {
        let reservation = self.transition.lifecycle_event_reservation;
        reservation != 0
            && self
                .reserve(
                    self.transition
                        .reserved_event_count()
                        .saturating_sub(reservation)
                        .saturating_add(count),
                )
                .is_ok()
    }

    fn pending_durable_batch_count(&self) -> usize {
        self.transition.pending_durable_events.len()
    }

    fn pending_durable_event_count(&self, batch_count: usize) -> usize {
        self.transition
            .pending_durable_events
            .iter()
            .take(batch_count)
            .map(Vec::len)
            .sum()
    }

    fn pending_durable_events(&self) -> Vec<DaemonEventKind> {
        self.transition
            .pending_durable_events
            .iter()
            .flatten()
            .cloned()
            .collect()
    }

    fn take_pending_durable(&mut self, batch_count: usize) -> VecDeque<Vec<DaemonEventKind>> {
        self.transition
            .pending_durable_events
            .drain(..batch_count)
            .collect()
    }

    fn restore_pending_durable(&mut self, pending: VecDeque<Vec<DaemonEventKind>>) {
        for batch in pending.into_iter().rev() {
            self.transition.pending_durable_events.push_front(batch);
        }
    }

    fn begin_persistence(&mut self, event_count: usize) {
        self.transition.persistence_in_flight = true;
        self.transition.in_flight_event_count = event_count;
    }

    fn begin_lifecycle_reservation(&mut self, event_count: usize) {
        debug_assert_eq!(self.transition.lifecycle_event_reservation, 0);
        self.transition.lifecycle_event_reservation = event_count;
    }

    fn replace_committed_executions(&mut self, executions: Vec<ScheduledExecutionSnapshot>) {
        self.events.committed_executions = executions
            .into_iter()
            .map(|execution| (execution.id.clone(), execution))
            .collect();
    }

    fn release_lifecycle_reservation(&mut self) {
        self.transition.lifecycle_event_reservation = 0;
        EventStream::publish_pending_runtime_locked(&mut self.transition, &mut self.events);
        self.changed.notify_all();
    }

    fn transfer_lifecycle_reservation_to_persistence(
        &mut self,
        persistence_event_count: usize,
        retained_reservation: usize,
    ) {
        self.transition.lifecycle_event_reservation = retained_reservation;
        self.begin_persistence(persistence_event_count);
    }

    fn append_batch(&mut self, batch: Vec<DaemonEventKind>) {
        EventStream::append_batch_locked(&mut self.events, batch);
    }

    fn append_pending_durable(&mut self) {
        while let Some(batch) = self.transition.pending_durable_events.pop_front() {
            EventStream::append_batch_locked(&mut self.events, batch);
        }
    }

    fn finish_persistence(&mut self) {
        self.transition.persistence_in_flight = false;
        self.transition.in_flight_event_count = 0;
        EventStream::publish_pending_runtime_locked(&mut self.transition, &mut self.events);
    }

    fn cursor(&self) -> EventCursor {
        EventCursor {
            stream_id: self.events.stream_id.clone(),
            event_id: self.events.latest_id,
        }
    }
}

impl EventTransitionAccess for EventTransaction<'_> {
    fn drain_runtime_events(&mut self) -> Vec<DaemonEventKind> {
        self.transition.pending_runtime_events.drain(..).collect()
    }

    fn queue_durable_batch(&mut self, batch: Vec<DaemonEventKind>) -> bool {
        if batch.is_empty() {
            false
        } else {
            self.transition.pending_durable_events.push_back(batch);
            true
        }
    }
}

impl EventStream {
    fn new() -> Self {
        Self {
            state: Mutex::new(EventStreamState {
                stream_id: Uuid::new_v4().to_string(),
                latest_id: 0,
                events: VecDeque::new(),
                schedule_shell_ids: HashSet::new(),
                committed_executions: HashMap::new(),
            }),
            transitions: Mutex::new(TransitionState::default()),
            changed: Condvar::new(),
            lifecycle_active: AtomicBool::new(false),
        }
    }

    fn from_transfer(transfer: Option<handoff::EventStreamManifest>) -> Self {
        let state = transfer.map_or_else(
            || EventStreamState {
                stream_id: Uuid::new_v4().to_string(),
                latest_id: 0,
                events: VecDeque::new(),
                schedule_shell_ids: HashSet::new(),
                committed_executions: HashMap::new(),
            },
            |transfer| {
                let mut schedule_shell_ids = transfer
                    .schedule_shell_ids
                    .into_iter()
                    .collect::<HashSet<_>>();
                for event in &transfer.events {
                    if let DaemonEventKind::ScheduledExecutionCreated { execution, .. }
                    | DaemonEventKind::ScheduledExecutionChanged { execution, .. } = &event.kind
                        && let Some(shell_id) = &execution.shell_id
                    {
                        schedule_shell_ids.insert(shell_id.clone());
                    }
                }
                let mut state = EventStreamState {
                    stream_id: transfer.stream_id,
                    latest_id: transfer.latest_id,
                    events: transfer.events.into(),
                    schedule_shell_ids,
                    committed_executions: HashMap::new(),
                };
                Self::retain_referenced_schedule_shell_ids(&mut state);
                state
            },
        );
        Self {
            state: Mutex::new(state),
            transitions: Mutex::new(TransitionState::default()),
            changed: Condvar::new(),
            lifecycle_active: AtomicBool::new(false),
        }
    }

    fn ensure_capacity(state: &EventStreamState, count: usize) -> io::Result<()> {
        let count = u64::try_from(count).map_err(|_| io::Error::other("event batch too large"))?;
        state
            .latest_id
            .checked_add(count)
            .map(|_| ())
            .ok_or_else(|| io::Error::other("daemon event ID exhausted"))
    }

    fn transaction(&self) -> io::Result<EventTransaction<'_>> {
        Ok(EventTransaction {
            transition: lock(&self.transitions)?,
            events: lock(&self.state)?,
            changed: &self.changed,
        })
    }

    fn manifest(&self) -> io::Result<handoff::EventStreamManifest> {
        let state = lock(&self.state)?;
        Ok(handoff::EventStreamManifest {
            stream_id: state.stream_id.clone(),
            latest_id: state.latest_id,
            events: state.events.iter().cloned().collect(),
            schedule_shell_ids: state.schedule_shell_ids.iter().cloned().collect(),
        })
    }

    fn schedule_shell_ids(&self) -> io::Result<HashSet<String>> {
        Ok(lock(&self.state)?.schedule_shell_ids.clone())
    }

    fn initialize_committed_executions(
        &self,
        executions: Vec<ScheduledExecutionSnapshot>,
    ) -> io::Result<()> {
        let mut transaction = self.transaction()?;
        transaction.replace_committed_executions(executions);
        Ok(())
    }

    fn wait_for_scheduled_execution(
        &self,
        execution_id: &str,
        after_revision: u64,
        wait_ms: u32,
        stopping: impl Fn() -> bool,
    ) -> DaemonResult<Response> {
        let deadline =
            Instant::now() + Duration::from_millis(u64::from(wait_ms)).min(MAX_EVENT_WAIT);
        loop {
            if stopping() {
                return Err(DaemonError::lifecycle(
                    ErrorCode::DaemonStopping,
                    "Boomux daemon is stopping",
                ));
            }
            let expired = wait_ms == 0 || Instant::now() >= deadline;
            let transition = lock(&self.transitions)?;
            let state = lock(&self.state)?;
            let execution = state
                .committed_executions
                .get(execution_id)
                .cloned()
                .ok_or_else(|| not_found("scheduled execution", execution_id))?;
            if after_revision < execution.revision {
                return Ok(Response::ScheduledExecutionWait {
                    execution,
                    changed: true,
                });
            }
            if after_revision > execution.revision {
                return Err(DaemonError::lifecycle(
                    ErrorCode::RevisionAhead,
                    "requested execution revision is ahead of the current revision",
                ));
            }
            if expired {
                return Ok(Response::ScheduledExecutionWait {
                    execution,
                    changed: false,
                });
            }
            drop(transition);
            let timeout = deadline.saturating_duration_since(Instant::now());
            let (_state, _) = self
                .changed
                .wait_timeout(state, timeout)
                .map_err(|_| io::Error::other("execution wait lock poisoned"))?;
        }
    }

    fn wait_for<T>(
        &self,
        wait_ms: u32,
        stopping: impl Fn() -> bool,
        mut inspect: impl FnMut(bool) -> DaemonResult<Option<T>>,
    ) -> DaemonResult<T> {
        let deadline =
            Instant::now() + Duration::from_millis(u64::from(wait_ms)).min(MAX_EVENT_WAIT);
        loop {
            if stopping() {
                return Err(DaemonError::lifecycle(
                    ErrorCode::DaemonStopping,
                    "Boomux daemon is stopping",
                ));
            }
            let expired = wait_ms == 0 || Instant::now() >= deadline;
            let transition = lock(&self.transitions)?;
            let state = lock(&self.state)?;
            let publication_blocked = transition.publication_blocked();
            if !publication_blocked && let Some(value) = inspect(expired)? {
                return Ok(value);
            }
            drop(transition);
            let timeout = deadline.saturating_duration_since(Instant::now());
            let timeout = if publication_blocked && timeout.is_zero() {
                IO_RETRY_DELAY
            } else {
                timeout
            };
            let (_state, _) = self
                .changed
                .wait_timeout(state, timeout)
                .map_err(|_| io::Error::other("event wait lock poisoned"))?;
        }
    }

    fn append_batch_locked(
        state: &mut EventStreamState,
        kinds: Vec<DaemonEventKind>,
    ) -> Vec<DaemonEvent> {
        debug_assert!(Self::ensure_capacity(state, kinds.len()).is_ok());
        let mut appended = Vec::with_capacity(kinds.len());
        for kind in kinds {
            if let DaemonEventKind::ScheduledExecutionCreated { execution, .. }
            | DaemonEventKind::ScheduledExecutionChanged { execution, .. } = &kind
                && let Some(shell_id) = &execution.shell_id
            {
                state.schedule_shell_ids.insert(shell_id.clone());
            }
            state.latest_id += 1;
            let event = DaemonEvent {
                id: state.latest_id,
                at_ms: unix_time_ms(),
                kind,
            };
            state.events.push_back(event.clone());
            appended.push(event);
        }
        if state.events.len() > MAX_RETAINED_EVENTS {
            let remove = state.events.len() - MAX_RETAINED_EVENTS;
            state.events.drain(..remove);
        }
        Self::retain_referenced_schedule_shell_ids(state);
        appended
    }

    fn retain_referenced_schedule_shell_ids(state: &mut EventStreamState) {
        let referenced_shell_ids = state
            .events
            .iter()
            .filter_map(|event| event_shell_id(&event.kind).map(str::to_owned))
            .collect::<HashSet<_>>();
        state
            .schedule_shell_ids
            .retain(|shell_id| referenced_shell_ids.contains(shell_id));
    }

    #[cfg(test)]
    fn publish(&self, kind: DaemonEventKind) -> io::Result<DaemonEvent> {
        let mut state = lock(&self.state)?;
        Self::ensure_capacity(&state, 1)?;
        let event = Self::append_batch_locked(&mut state, vec![kind])
            .pop()
            .expect("one event was appended");
        drop(state);
        self.changed.notify_all();
        Ok(event)
    }

    fn notify(&self) {
        if let Ok(state) = self.state.lock() {
            self.changed.notify_all();
            drop(state);
        }
    }

    fn read_after(
        &self,
        after: &EventCursor,
        limit: usize,
        wait_ms: u32,
        stopping: impl Fn() -> bool,
    ) -> DaemonResult<Response> {
        let deadline =
            Instant::now() + Duration::from_millis(u64::from(wait_ms)).min(MAX_EVENT_WAIT);
        let mut state = lock(&self.state)?;
        loop {
            let earliest = state
                .events
                .front()
                .map_or(state.latest_id, |event| event.id.saturating_sub(1));
            if after.stream_id != state.stream_id
                || after.event_id < earliest
                || after.event_id > state.latest_id
            {
                return Err(DaemonError::lifecycle(
                    ErrorCode::CursorExpired,
                    "event cursor is no longer available",
                ));
            }
            let events = state
                .events
                .iter()
                .filter(|event| event.id > after.event_id)
                .take(limit)
                .cloned()
                .collect::<Vec<_>>();
            if !events.is_empty() || wait_ms == 0 || Instant::now() >= deadline {
                let event_id = events.last().map_or(after.event_id, |event| event.id);
                return Ok(Response::Events {
                    stream_id: state.stream_id.clone(),
                    cursor: EventCursor {
                        stream_id: state.stream_id.clone(),
                        event_id,
                    },
                    snapshot: None,
                    events,
                });
            }
            if stopping() {
                return Err(DaemonError::lifecycle(
                    ErrorCode::DaemonStopping,
                    "Boomux daemon is stopping",
                ));
            }
            let timeout = deadline.saturating_duration_since(Instant::now());
            let (next, _) = self
                .changed
                .wait_timeout(state, timeout)
                .map_err(|_| io::Error::other("daemon event lock poisoned"))?;
            state = next;
        }
    }

    fn publish_runtime_batch(&self, kinds: Vec<DaemonEventKind>) -> io::Result<()> {
        let mut transition = lock(&self.transitions)?;
        let mut events = lock(&self.state)?;
        Self::ensure_capacity(
            &events,
            transition
                .reserved_event_count()
                .saturating_add(kinds.len()),
        )?;
        if transition.publication_blocked() {
            for kind in kinds {
                transition.queue_runtime_event(kind);
            }
            return Ok(());
        }
        Self::append_batch_locked(&mut events, kinds);
        drop(events);
        self.changed.notify_all();
        Ok(())
    }

    fn publish_pending_runtime_locked(
        transition: &mut TransitionState,
        events: &mut EventStreamState,
    ) {
        if transition.publication_blocked() {
            return;
        }
        while let Some(kind) = transition.pending_runtime_events.pop_front() {
            Self::append_batch_locked(events, vec![kind]);
        }
    }
}

fn projection_transitions(
    state: &EventStreamState,
    after: Option<&EventCursor>,
    through: &EventCursor,
) -> (NodeProjectionSyncMode, Vec<NodeProjectionTransition>) {
    let Some(after) = after else {
        return (NodeProjectionSyncMode::Baseline, Vec::new());
    };
    let earliest = state
        .events
        .front()
        .map_or(state.latest_id, |event| event.id.saturating_sub(1));
    if after.stream_id != state.stream_id
        || after.event_id < earliest
        || after.event_id > through.event_id
    {
        return (NodeProjectionSyncMode::Baseline, Vec::new());
    }
    let events = state
        .events
        .iter()
        .filter(|event| event.id > after.event_id && event.id <= through.event_id)
        .collect::<Vec<_>>();
    if events.len() > usize::from(protocol::MAX_NODE_PROJECTION_TRANSITIONS) {
        return (NodeProjectionSyncMode::Baseline, Vec::new());
    }
    let transitions = events
        .into_iter()
        .filter_map(reduce_projection_transition)
        .collect();
    (NodeProjectionSyncMode::Resumed, transitions)
}

fn reduce_projection_transition(event: &DaemonEvent) -> Option<NodeProjectionTransition> {
    let kind = match &event.kind {
        DaemonEventKind::WorkspaceCreated { workspace_id, .. }
        | DaemonEventKind::WorkspaceRenamed { workspace_id, .. }
        | DaemonEventKind::WorkspaceClosed { workspace_id } => {
            NodeProjectionTransitionKind::Workspace {
                workspace_id: workspace_id.clone(),
            }
        }
        DaemonEventKind::ShellCreated {
            workspace_id,
            shell_id,
            ..
        }
        | DaemonEventKind::ShellRenamed {
            workspace_id,
            shell_id,
            ..
        }
        | DaemonEventKind::RunStarted {
            workspace_id,
            shell_id,
            ..
        }
        | DaemonEventKind::OutputChanged {
            workspace_id,
            shell_id,
            ..
        }
        | DaemonEventKind::RunExited {
            workspace_id,
            shell_id,
            ..
        } => NodeProjectionTransitionKind::Shell {
            workspace_id: workspace_id.clone(),
            shell_id: shell_id.clone(),
        },
        DaemonEventKind::ShellClosed {
            workspace_id: Some(workspace_id),
            shell_id,
        } => NodeProjectionTransitionKind::Shell {
            workspace_id: workspace_id.clone(),
            shell_id: shell_id.clone(),
        },
        DaemonEventKind::ShellClosed {
            workspace_id: None, ..
        } => return None,
        DaemonEventKind::LauncherCreated {
            workspace_id,
            launcher_id,
            ..
        }
        | DaemonEventKind::LauncherRenamed {
            workspace_id,
            launcher_id,
            ..
        }
        | DaemonEventKind::LauncherRemoved {
            workspace_id,
            launcher_id,
        } => NodeProjectionTransitionKind::Launcher {
            workspace_id: workspace_id.clone(),
            launcher_id: launcher_id.clone(),
        },
        DaemonEventKind::AgentRegistered {
            workspace_id,
            agent,
            ..
        }
        | DaemonEventKind::AgentStateChanged {
            workspace_id,
            agent,
            ..
        }
        | DaemonEventKind::AgentCompleted {
            workspace_id,
            agent,
            ..
        }
        | DaemonEventKind::AgentAttentionAcknowledged {
            workspace_id,
            agent,
            ..
        } => NodeProjectionTransitionKind::Agent {
            workspace_id: workspace_id.clone(),
            agent_id: agent.id.clone(),
            revision: agent.observation.revision,
        },
        DaemonEventKind::AgentScheduleCreated {
            workspace_id,
            schedule,
        }
        | DaemonEventKind::AgentSchedulePaused {
            workspace_id,
            schedule,
        }
        | DaemonEventKind::AgentScheduleResumed {
            workspace_id,
            schedule,
        }
        | DaemonEventKind::AgentScheduleUpdated {
            workspace_id,
            schedule,
        } => NodeProjectionTransitionKind::Schedule {
            workspace_id: workspace_id.clone(),
            schedule_id: schedule.id.clone(),
            revision: Some(schedule.revision),
        },
        DaemonEventKind::AgentScheduleRemoved {
            workspace_id,
            schedule_id,
        } => NodeProjectionTransitionKind::Schedule {
            workspace_id: workspace_id.clone(),
            schedule_id: schedule_id.clone(),
            revision: None,
        },
        DaemonEventKind::ScheduledExecutionCreated {
            workspace_id,
            execution,
        }
        | DaemonEventKind::ScheduledExecutionChanged {
            workspace_id,
            execution,
        } => NodeProjectionTransitionKind::Execution {
            workspace_id: workspace_id.clone(),
            execution_id: execution.id.clone(),
            revision: execution.revision,
        },
        DaemonEventKind::HandoffCompleted => NodeProjectionTransitionKind::HandoffCompleted,
        DaemonEventKind::NodeProjectionChanged { .. } => return None,
    };
    Some(NodeProjectionTransition {
        event_id: event.id,
        at_ms: event.at_ms,
        kind,
    })
}

impl DurableRegistry {
    fn workspace(&self, id: &str) -> io::Result<Arc<Workspace>> {
        lock(&self.state)?
            .workspaces
            .get(id)
            .cloned()
            .ok_or_else(|| not_found("workspace", id))
    }

    fn shell(&self, id: &str) -> io::Result<Arc<Shell>> {
        lock(&self.state)?
            .shells
            .get(id)
            .cloned()
            .ok_or_else(|| not_found("shell", id))
    }

    fn launcher(&self, id: &str) -> io::Result<Arc<WorkspaceLauncher>> {
        lock(&self.state)?
            .launchers
            .get(id)
            .cloned()
            .ok_or_else(|| not_found("workspace launcher", id))
    }

    fn agent(&self, id: &str) -> io::Result<Arc<AgentInstance>> {
        lock(&self.state)?
            .agents
            .get(id)
            .cloned()
            .ok_or_else(|| not_found("agent instance", id))
    }

    fn schedule(&self, id: &str) -> io::Result<Arc<AgentSchedule>> {
        lock(&self.state)?
            .schedules
            .get(id)
            .cloned()
            .ok_or_else(|| not_found("agent schedule", id))
    }

    fn execution(&self, id: &str) -> io::Result<Arc<ScheduledExecution>> {
        let schedules = lock(&self.state)?
            .schedules
            .values()
            .cloned()
            .collect::<Vec<_>>();
        for schedule in schedules {
            if let Some(execution) = lock(&schedule.executions)?
                .iter()
                .find(|execution| execution.id == id)
            {
                return Ok(Arc::clone(execution));
            }
        }
        Err(not_found("scheduled execution", id))
    }

    fn scheduled_executions(
        &self,
        workspace_id: Option<&str>,
        schedule_id: Option<&str>,
    ) -> io::Result<Vec<ScheduledExecutionSnapshot>> {
        let schedules = lock(&self.state)?
            .schedules
            .values()
            .cloned()
            .collect::<Vec<_>>();
        let mut snapshots = Vec::new();
        for schedule in schedules {
            if workspace_id.is_some_and(|id| id != schedule.workspace_id)
                || schedule_id.is_some_and(|id| id != schedule.id)
            {
                continue;
            }
            let executions = lock(&schedule.executions)?.clone();
            for execution in executions {
                snapshots.push(execution.snapshot()?);
            }
        }
        snapshots.sort_by(|left, right| {
            right
                .requested_at_ms
                .cmp(&left.requested_at_ms)
                .then_with(|| right.id.cmp(&left.id))
        });
        Ok(snapshots)
    }

    fn scheduled_execution_page(
        &self,
        workspace_id: Option<&str>,
        schedule_id: Option<&str>,
        limit: u16,
    ) -> io::Result<(Vec<ScheduledExecutionSnapshot>, u16, bool)> {
        let limit = limit.clamp(1, protocol::MAX_SCHEDULED_EXECUTION_LIST_LIMIT);
        let mut executions = self.scheduled_executions(workspace_id, schedule_id)?;
        let truncated = executions.len() > usize::from(limit);
        executions.truncate(usize::from(limit));
        Ok((executions, limit, truncated))
    }

    fn scheduled_execution_schedule_projections(
        &self,
        workspace_id: Option<&str>,
        schedule_id: Option<&str>,
    ) -> io::Result<(Vec<ScheduledExecutionScheduleProjection>, u16, bool)> {
        let mut schedules = lock(&self.state)?
            .schedules
            .values()
            .filter(|schedule| {
                workspace_id.is_none_or(|id| id == schedule.workspace_id)
                    && schedule_id.is_none_or(|id| id == schedule.id)
            })
            .cloned()
            .collect::<Vec<_>>();
        schedules.sort_by(|left, right| left.id.cmp(&right.id));
        let limit = protocol::MAX_SCHEDULED_EXECUTION_SCHEDULE_PROJECTIONS;
        let truncated = schedules.len() > usize::from(limit);
        schedules.truncate(usize::from(limit));
        let projections = schedules
            .into_iter()
            .map(|schedule| {
                let snapshot = schedule.snapshot()?;
                Ok(ScheduledExecutionScheduleProjection {
                    schedule_id: snapshot.id,
                    next_occurrence: snapshot.next_occurrence,
                })
            })
            .collect::<io::Result<_>>()?;
        Ok((projections, limit, truncated))
    }

    fn active_scheduled_execution_count(&self) -> io::Result<usize> {
        self.active_scheduled_execution_count_excluding(None)
    }

    fn active_scheduled_execution_count_excluding(
        &self,
        excluding_schedule_id: Option<&str>,
    ) -> io::Result<usize> {
        let schedules = lock(&self.state)?
            .schedules
            .values()
            .filter(|schedule| excluding_schedule_id != Some(schedule.id.as_str()))
            .cloned()
            .collect::<Vec<_>>();
        let mut count = 0;
        for schedule in schedules {
            for execution in lock(&schedule.executions)?.iter() {
                count += usize::from(!lock(&execution.state)?.state.is_terminal());
            }
        }
        Ok(count)
    }

    #[cfg(test)]
    fn claim_schedule_execution(
        &self,
        schedule_id: &str,
        dispatch_key: &str,
    ) -> DaemonResult<(ScheduledExecutionSnapshot, Option<DurableUndo>)> {
        let (execution, undo, _) = self.decide_schedule_execution(
            schedule_id,
            ScheduleDecision {
                dispatch_kind: ScheduledExecutionDispatchKind::Manual,
                dispatch_key: dispatch_key.to_owned(),
                scheduled_at_ms: None,
                coalesced_through_ms: None,
                requested_at_ms: unix_time_ms(),
                forced_skip: None,
            },
            4,
        )?;
        Ok((execution, undo))
    }

    fn decide_schedule_execution(
        &self,
        schedule_id: &str,
        decision: ScheduleDecision,
        max_concurrent: u16,
    ) -> DaemonResult<(ScheduledExecutionSnapshot, Option<DurableUndo>, bool)> {
        let ScheduleDecision {
            dispatch_kind,
            dispatch_key,
            scheduled_at_ms,
            coalesced_through_ms,
            requested_at_ms,
            forced_skip,
        } = decision;
        validate_id("dispatch idempotency key", &dispatch_key)?;
        let schedule = self.schedule(schedule_id)?;
        let existing = lock(&schedule.executions)?
            .iter()
            .find(|execution| execution.dispatch_key == dispatch_key)
            .cloned();
        if let Some(existing) = existing {
            let snapshot = existing.snapshot()?;
            let claimed = snapshot.state == ScheduledExecutionState::Claimed;
            return Ok((snapshot, None, claimed));
        }
        let schedule_state = lock(&schedule.state)?.clone();
        let execution_shell_id = schedule_state.execution_shell_id.clone();
        let mut skip_reason = forced_skip;
        if let Some(shell_id) = execution_shell_id {
            let shell = self.shell(&shell_id)?;
            if matches!(*lock(&shell.lifecycle)?, ShellLifecycle::Running { .. }) {
                skip_reason.get_or_insert(ScheduledExecutionReason::Overlap);
            }
        }
        let mut executions = lock(&schedule.executions)?;
        let mut dispatch_key_filter = lock(&schedule.dispatch_key_filter)?;
        if dispatch_key_was_seen(&dispatch_key_filter, &dispatch_key) {
            return Err(DaemonError::lifecycle(
                ErrorCode::IdempotencyExpired,
                "dispatch idempotency key was used by a pruned execution",
            ));
        }
        if executions
            .iter()
            .any(|execution| lock(&execution.state).is_ok_and(|state| !state.state.is_terminal()))
        {
            skip_reason.get_or_insert(ScheduledExecutionReason::Overlap);
        }
        if skip_reason.is_none()
            && (self.continuation_session_is_active(&schedule)?
                || self.continuation_lease_is_occupied(&schedule)?)
        {
            skip_reason = Some(ScheduledExecutionReason::ActiveSession);
        }
        if skip_reason.is_none()
            && self.workspace_has_nonterminal_execution(&schedule.workspace_id, &schedule.id)?
        {
            skip_reason = Some(ScheduledExecutionReason::WorkspaceCapacity);
        }
        if skip_reason.is_none()
            && self.active_scheduled_execution_count_excluding(Some(&schedule.id))?
                >= usize::from(max_concurrent)
        {
            skip_reason = Some(ScheduledExecutionReason::GlobalCapacity);
        }
        if skip_reason.is_none()
            && validate_schedule_capability(&schedule.integration, &schedule.session).is_err()
        {
            skip_reason = Some(ScheduledExecutionReason::InvalidTarget);
        }
        let state = if skip_reason.is_some() {
            ScheduledExecutionState::Skipped
        } else {
            ScheduledExecutionState::Claimed
        };
        let id = if let Some(scheduled_at_ms) = scheduled_at_ms {
            timed_execution_id(
                &schedule.id,
                schedule_state.trigger_revision,
                scheduled_at_ms,
            )
        } else {
            Uuid::new_v4().to_string()
        };
        let execution = Arc::new(ScheduledExecution {
            id,
            workspace_id: schedule.workspace_id.clone(),
            schedule_id: schedule.id.clone(),
            dispatch_kind,
            dispatch_key: dispatch_key.clone(),
            schedule_revision: schedule_state.revision,
            prompt_revision: schedule_state.prompt_revision,
            trigger_revision: schedule_state.trigger_revision,
            requested_at_ms,
            scheduled_at_ms,
            coalesced_through_ms,
            cwd: schedule.cwd.clone(),
            integration: schedule.integration.clone(),
            session: schedule.session.clone(),
            prompt: schedule_state.prompt.clone(),
            runner_token: Uuid::new_v4().to_string(),
            state: Mutex::new(ScheduledExecutionMutableState {
                revision: 1,
                state,
                started_at_ms: None,
                ended_at_ms: skip_reason.map(|_| requested_at_ms),
                reason: skip_reason,
                outcome: None,
                shell_id: None,
                run_id: None,
                agent_id: None,
                external_session_id: None,
            }),
        });
        let snapshot = execution.snapshot()?;
        let previous = executions.clone();
        let previous_dispatch_key_filter = dispatch_key_filter.clone();
        remember_dispatch_key(&mut dispatch_key_filter, &dispatch_key);
        executions.push(execution);
        if state.is_terminal() {
            prune_terminal_executions(&mut executions);
        }
        drop(dispatch_key_filter);
        drop(executions);
        Ok((
            snapshot,
            Some(DurableUndo::ScheduleExecutions {
                schedule,
                previous,
                previous_dispatch_key_filter,
                execution: None,
            }),
            state == ScheduledExecutionState::Claimed,
        ))
    }

    fn workspace_has_nonterminal_execution(
        &self,
        workspace_id: &str,
        excluding_schedule_id: &str,
    ) -> io::Result<bool> {
        let schedules = lock(&self.state)?
            .schedules
            .values()
            .filter(|schedule| {
                schedule.workspace_id == workspace_id && schedule.id != excluding_schedule_id
            })
            .cloned()
            .collect::<Vec<_>>();
        for schedule in schedules {
            if lock(&schedule.executions)?.iter().any(|execution| {
                lock(&execution.state).is_ok_and(|state| !state.state.is_terminal())
            }) {
                return Ok(true);
            }
        }
        Ok(false)
    }

    fn continuation_lease_is_occupied(&self, schedule: &AgentSchedule) -> io::Result<bool> {
        let AgentScheduleSession::Continue {
            external_session_id,
        } = &schedule.session
        else {
            return Ok(false);
        };
        let schedules = lock(&self.state)?
            .schedules
            .values()
            .filter(|candidate| candidate.id != schedule.id)
            .cloned()
            .collect::<Vec<_>>();
        for candidate in schedules {
            if candidate.integration == schedule.integration
                && matches!(
                    &candidate.session,
                    AgentScheduleSession::Continue { external_session_id: candidate_id }
                        if candidate_id == external_session_id
                )
                && lock(&candidate.executions)?.iter().any(|execution| {
                    lock(&execution.state).is_ok_and(|state| !state.state.is_terminal())
                })
            {
                return Ok(true);
            }
        }
        Ok(false)
    }

    fn continuation_session_is_active(&self, schedule: &AgentSchedule) -> io::Result<bool> {
        let AgentScheduleSession::Continue {
            external_session_id,
        } = &schedule.session
        else {
            return Ok(false);
        };
        let candidates = {
            let state = lock(&self.state)?;
            state
                .agents
                .values()
                .filter(|agent| {
                    agent.integration == schedule.integration
                        && agent.external_session_id.as_deref()
                            == Some(external_session_id.as_str())
                })
                .filter_map(|agent| {
                    state
                        .shells
                        .get(&agent.shell_id)
                        .map(|shell| (Arc::clone(agent), Arc::clone(shell)))
                })
                .collect::<Vec<_>>()
        };
        for (agent, shell) in candidates {
            let agent_state = lock(&agent.state)?;
            if agent_state.ended_at_ms.is_none()
                && !matches!(
                    agent_state.observation.state,
                    AgentState::Inactive | AgentState::Done
                )
                && matches!(
                    &*lock(&shell.lifecycle)?,
                    ShellLifecycle::Running { run, .. } if run.id == agent.run_id
                )
            {
                return Ok(true);
            }
        }
        Ok(false)
    }

    fn contains_shell(&self, shell: &Arc<Shell>) -> io::Result<bool> {
        Ok(lock(&self.state)?
            .shells
            .get(&shell.id)
            .is_some_and(|current| Arc::ptr_eq(current, shell)))
    }

    fn clear_terminal_histories(&self) -> io::Result<bool> {
        let state = lock(&self.state)?;
        let mut changed = false;
        for shell in state.shells.values() {
            if let Some(run) = lock(&shell.last_run)?.as_mut()
                && run.terminal_history.take().is_some()
            {
                changed = true;
            }
        }
        Ok(changed)
    }

    fn agent_resume_command(
        &self,
        shell: &Shell,
        previous_run: &PersistedShellRun,
    ) -> io::Result<Option<Vec<String>>> {
        let state = lock(&self.state)?;
        let mut candidates = Vec::new();
        for agent in state.agents.values() {
            if let Some(identity) = resume_identity(agent, shell, previous_run)? {
                candidates.push(identity);
            }
        }
        candidates.sort();
        candidates.dedup();
        let [(integration, external_session_id)] = candidates.as_slice() else {
            return Ok(None);
        };

        let duplicate = state
            .shells
            .values()
            .try_fold(false, |duplicate, other_shell| {
                if duplicate {
                    return Ok::<_, io::Error>(true);
                }
                if other_shell.id == shell.id {
                    return Ok(false);
                }
                let last_run = lock(&other_shell.last_run)?;
                let Some(last_run) = last_run.as_ref() else {
                    return Ok(false);
                };
                for agent in state.agents.values() {
                    if resume_identity(agent, other_shell, last_run)?.is_some_and(|identity| {
                        identity.0 == *integration && identity.1 == *external_session_id
                    }) {
                        return Ok(true);
                    }
                }
                Ok(false)
            })?;
        if duplicate {
            return Ok(None);
        }

        Ok(crate::integrations::by_key(integration)
            .and_then(|descriptor| descriptor.resume)
            .and_then(|resume| resume.command(&shell.command, external_session_id)))
    }

    fn notification_context(&self, workspace_id: &str, shell_id: &str) -> (String, String) {
        let Ok(state) = self.state.lock() else {
            return ("unknown".into(), "removed".into());
        };
        let workspace = state
            .workspaces
            .get(workspace_id)
            .and_then(|workspace| workspace.name.lock().ok().map(|name| name.clone()))
            .unwrap_or_else(|| "unknown".into());
        let shell = state
            .shells
            .get(shell_id)
            .and_then(|shell| shell.name.lock().ok().map(|name| name.clone()))
            .unwrap_or_else(|| "removed".into());
        (workspace, shell)
    }

    fn schedule_notification_context(
        &self,
        workspace_id: &str,
        schedule_id: &str,
    ) -> (String, String) {
        let Ok(state) = self.state.lock() else {
            return ("unknown".into(), "removed".into());
        };
        let workspace = state
            .workspaces
            .get(workspace_id)
            .and_then(|workspace| workspace.name.lock().ok().map(|name| name.clone()))
            .unwrap_or_else(|| "unknown".into());
        let schedule = state
            .schedules
            .get(schedule_id)
            .and_then(|schedule| schedule.state.lock().ok().map(|state| state.name.clone()))
            .unwrap_or_else(|| "removed".into());
        (workspace, schedule)
    }

    fn create_workspace(
        &self,
        name: String,
        default_cwd: Option<PathBuf>,
        specs: Vec<ShellSpec>,
    ) -> io::Result<(WorkspaceSnapshot, DurableUndo)> {
        validate_name(&name)?;
        if let Some(default_cwd) = &default_cwd {
            validate_cwd(default_cwd)?;
        }
        validate_shell_specs(&specs)?;
        let workspace_id = Uuid::new_v4().to_string();
        let shells = specs
            .into_iter()
            .map(|spec| create_pending_shell(&workspace_id, spec))
            .collect::<io::Result<Vec<_>>>()?;
        let workspace = Arc::new(Workspace {
            id: workspace_id.clone(),
            revision: Mutex::new(1),
            name: Mutex::new(name.clone()),
            default_cwd,
            shell_ids: Mutex::new(shells.iter().map(|shell| shell.id.clone()).collect()),
            launcher_ids: Mutex::new(Vec::new()),
            agent_ids: Mutex::new(Vec::new()),
            schedule_ids: Mutex::new(Vec::new()),
        });
        let snapshot = WorkspaceSnapshot {
            id: workspace_id.clone(),
            revision: 1,
            name: name.clone(),
            default_cwd: workspace.default_cwd.clone(),
            shells: shells
                .iter()
                .map(|shell| shell.snapshot())
                .collect::<io::Result<_>>()?,
            launchers: Vec::new(),
            agents: Vec::new(),
            schedules: Vec::new(),
        };
        let mut state = lock(&self.state)?;
        if state
            .workspaces
            .values()
            .any(|current| current.name.lock().is_ok_and(|current| *current == name))
        {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!("workspace name already exists: {name}"),
            ));
        }
        for shell in &shells {
            state.shells.insert(shell.id.clone(), Arc::clone(shell));
        }
        state
            .workspaces
            .insert(workspace_id, Arc::clone(&workspace));
        Ok((
            snapshot,
            DurableUndo::CreatedWorkspace { workspace, shells },
        ))
    }

    fn create_shell(
        &self,
        workspace_id: &str,
        spec: ShellSpec,
    ) -> io::Result<(ShellSnapshot, DurableUndo)> {
        let workspace = self.workspace(workspace_id)?;
        let shell = create_pending_shell(workspace_id, spec)?;
        let snapshot = shell.snapshot()?;
        let mut state = lock(&self.state)?;
        let Some(current) = state.workspaces.get(workspace_id) else {
            return Err(not_found("workspace", workspace_id));
        };
        if !Arc::ptr_eq(current, &workspace) {
            return Err(not_found("workspace", workspace_id));
        }
        let mut shell_ids = lock(&workspace.shell_ids)?;
        if shell_ids.iter().any(|id| {
            state
                .shells
                .get(id)
                .and_then(|existing| existing.name.lock().ok())
                .is_some_and(|name| *name == snapshot.name)
        }) {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!("shell name already exists: {}", snapshot.name),
            ));
        }
        state.shells.insert(shell.id.clone(), Arc::clone(&shell));
        shell_ids.push(shell.id.clone());
        bump_revision(&workspace.revision, "workspace")?;
        drop(shell_ids);
        drop(state);
        Ok((snapshot, DurableUndo::CreatedShell { workspace, shell }))
    }

    fn create_schedule_shell(
        &self,
        schedule: &Arc<AgentSchedule>,
        command: Vec<String>,
    ) -> io::Result<(Arc<Shell>, DurableUndo)> {
        let workspace = self.workspace(&schedule.workspace_id)?;
        let shell = Arc::new(Shell {
            id: Uuid::new_v4().to_string(),
            revision: Mutex::new(1),
            workspace_id: schedule.workspace_id.clone(),
            name: Mutex::new(format!("schedule-{}", &schedule.id[..8])),
            cwd: schedule.cwd.clone(),
            command,
            owner: ShellOwner::Schedule {
                schedule_id: schedule.id.clone(),
            },
            last_run: Mutex::new(None),
            lifecycle: Mutex::new(ShellLifecycle::Pending),
            foreground_process_cache: Mutex::new(None),
        });
        let mut state = lock(&self.state)?;
        let mut shell_ids = lock(&workspace.shell_ids)?;
        state.shells.insert(shell.id.clone(), Arc::clone(&shell));
        shell_ids.push(shell.id.clone());
        bump_revision(&workspace.revision, "workspace")?;
        drop(shell_ids);
        drop(state);
        Ok((
            shell.clone(),
            DurableUndo::CreatedShell { workspace, shell },
        ))
    }

    fn mutate_execution(
        &self,
        execution: &Arc<ScheduledExecution>,
        mutate: impl FnOnce(&mut ScheduledExecutionMutableState) -> io::Result<()>,
    ) -> io::Result<(ScheduledExecutionSnapshot, DurableUndo)> {
        let schedule = self.schedule(&execution.schedule_id)?;
        let mut executions = lock(&schedule.executions)?;
        if !executions
            .iter()
            .any(|current| Arc::ptr_eq(current, execution))
        {
            return Err(not_found("scheduled execution", &execution.id));
        }
        let previous = executions.clone();
        let previous_dispatch_key_filter = lock(&schedule.dispatch_key_filter)?.clone();
        let mut state = lock(&execution.state)?;
        let previous_state = state.clone();
        mutate(&mut state)?;
        state.revision = state
            .revision
            .checked_add(1)
            .ok_or_else(|| io::Error::other("scheduled execution revision exhausted"))?;
        let snapshot = execution.snapshot_from(&state);
        let became_terminal = state.state.is_terminal();
        drop(state);
        if became_terminal {
            prune_terminal_executions(&mut executions);
        }
        drop(executions);
        Ok((
            snapshot,
            DurableUndo::ScheduleExecutions {
                schedule,
                previous,
                previous_dispatch_key_filter,
                execution: Some((Arc::clone(execution), previous_state)),
            },
        ))
    }

    fn create_shell_with_workspace(
        &self,
        spec: ShellSpec,
    ) -> io::Result<(ShellSnapshot, DurableUndo)> {
        let default_cwd = spec.cwd.clone();
        loop {
            let name = self.next_workspace_name()?;
            match self.create_workspace(name, Some(default_cwd.clone()), vec![spec.clone()]) {
                Ok((workspace, undo)) => {
                    let shell = workspace
                        .shells
                        .into_iter()
                        .next()
                        .expect("implicit workspace is created with one shell");
                    return Ok((shell, undo));
                }
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error),
            }
        }
    }

    fn create_launcher(
        &self,
        workspace_id: &str,
        spec: WorkspaceLauncherSpec,
    ) -> io::Result<(WorkspaceLauncherSnapshot, DurableUndo)> {
        validate_name(&spec.name)?;
        validate_cwd(&spec.cwd)?;
        validate_launcher_command(&spec.command)?;
        let workspace = self.workspace(workspace_id)?;
        let launcher = Arc::new(WorkspaceLauncher {
            id: Uuid::new_v4().to_string(),
            revision: Mutex::new(1),
            workspace_id: workspace_id.into(),
            name: Mutex::new(spec.name),
            cwd: spec.cwd,
            command: spec.command,
        });
        let snapshot = launcher.snapshot()?;
        let mut state = lock(&self.state)?;
        let Some(current) = state.workspaces.get(workspace_id) else {
            return Err(not_found("workspace", workspace_id));
        };
        if !Arc::ptr_eq(current, &workspace) {
            return Err(not_found("workspace", workspace_id));
        }
        let mut launcher_ids = lock(&workspace.launcher_ids)?;
        if launcher_ids.iter().any(|id| {
            state
                .launchers
                .get(id)
                .and_then(|existing| existing.name.lock().ok())
                .is_some_and(|name| *name == snapshot.name)
        }) {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!("workspace launcher name already exists: {}", snapshot.name),
            ));
        }
        state
            .launchers
            .insert(launcher.id.clone(), Arc::clone(&launcher));
        launcher_ids.push(launcher.id.clone());
        bump_revision(&workspace.revision, "workspace")?;
        drop(launcher_ids);
        drop(state);
        Ok((
            snapshot,
            DurableUndo::CreatedLauncher {
                workspace,
                launcher,
            },
        ))
    }

    #[cfg(test)]
    fn create_schedule(
        &self,
        workspace_id: &str,
        spec: AgentScheduleSpec,
    ) -> io::Result<(AgentScheduleSnapshot, DurableUndo)> {
        self.create_schedule_at(workspace_id, spec, unix_time_ms())
    }

    fn create_schedule_at(
        &self,
        workspace_id: &str,
        mut spec: AgentScheduleSpec,
        now: u64,
    ) -> io::Result<(AgentScheduleSnapshot, DurableUndo)> {
        validate_name(&spec.name)?;
        validate_cwd(&spec.cwd)?;
        validate_schedule_spec(&spec)?;
        spec.trigger.cron = crate::scheduling::canonicalize_cron(&spec.trigger.cron)
            .map_err(schedule_validation_error)?;
        spec.trigger.timezone = crate::scheduling::canonicalize_timezone(&spec.trigger.timezone)
            .map_err(schedule_validation_error)?;
        crate::scheduling::CronSchedule::compile(&spec.trigger.cron, &spec.trigger.timezone)
            .and_then(|cron| cron.ensure_possible())
            .map_err(schedule_validation_error)?;
        validate_schedule_capability(&spec.integration, &spec.session)?;
        let workspace = self.workspace(workspace_id)?;
        let schedule = Arc::new(AgentSchedule {
            id: Uuid::new_v4().to_string(),
            workspace_id: workspace_id.into(),
            cwd: spec.cwd,
            integration: spec.integration,
            session: spec.session,
            overlap_policy: spec.overlap_policy,
            created_at_ms: now,
            state: Mutex::new(AgentScheduleMutableState {
                name: spec.name,
                prompt: spec.prompt,
                trigger: spec.trigger,
                prompt_revision: 1,
                trigger_revision: 1,
                state: spec.state,
                revision: 1,
                updated_at_ms: now,
                evaluation_frontier_ms: now,
                evaluation_frontier_trigger_revision: 1,
                execution_shell_id: None,
            }),
            executions: Mutex::new(Vec::new()),
            dispatch_key_filter: Mutex::new(vec![0; DISPATCH_KEY_FILTER_BYTES]),
        });
        let snapshot = schedule.snapshot()?;
        let mut state = lock(&self.state)?;
        let Some(current) = state.workspaces.get(workspace_id) else {
            return Err(not_found("workspace", workspace_id));
        };
        if !Arc::ptr_eq(current, &workspace) {
            return Err(not_found("workspace", workspace_id));
        }
        let mut schedule_ids = lock(&workspace.schedule_ids)?;
        if schedule_ids.len() >= crate::scheduling::MAX_SCHEDULES_PER_WORKSPACE {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "workspace may contain at most {} agent schedules",
                    crate::scheduling::MAX_SCHEDULES_PER_WORKSPACE
                ),
            ));
        }
        if schedule_ids.iter().any(|id| {
            state
                .schedules
                .get(id)
                .and_then(|existing| existing.state.lock().ok())
                .is_some_and(|existing| existing.name == snapshot.name)
        }) {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!("agent schedule name already exists: {}", snapshot.name),
            ));
        }
        state
            .schedules
            .insert(schedule.id.clone(), Arc::clone(&schedule));
        schedule_ids.push(schedule.id.clone());
        bump_revision(&workspace.revision, "workspace")?;
        drop(schedule_ids);
        drop(state);
        Ok((
            snapshot,
            DurableUndo::CreatedSchedule {
                workspace,
                schedule,
            },
        ))
    }

    fn set_schedule_state_at(
        &self,
        schedule_id: &str,
        next: AgentScheduleState,
        expected_revision: Option<u64>,
        requested_at_ms: u64,
    ) -> DaemonResult<(AgentScheduleSnapshot, Option<DurableUndo>)> {
        let schedule = self.schedule(schedule_id)?;
        let mut state = lock(&schedule.state)?;
        if let Some(expected) = expected_revision {
            require_guard(state.revision, expected, "agent schedule")?;
        }
        if state.state == next {
            return Ok((schedule.snapshot_from(&state)?, None));
        }
        let mut compiled_trigger = None;
        if next == AgentScheduleState::Enabled {
            let cron = crate::scheduling::CronSchedule::compile(
                &state.trigger.cron,
                &state.trigger.timezone,
            )
            .map_err(schedule_validation_error)?;
            cron.ensure_possible().map_err(schedule_validation_error)?;
            compiled_trigger = Some(cron);
            validate_schedule_capability(&schedule.integration, &schedule.session)?;
        }
        let previous = state.clone();
        let now = requested_at_ms.max(state.updated_at_ms.saturating_add(1));
        if let Some(cron) = compiled_trigger {
            cron.next_after_ms(now.max(state.evaluation_frontier_ms))
                .map_err(schedule_validation_error)?;
        }
        state.state = next;
        state.revision = state
            .revision
            .checked_add(1)
            .ok_or_else(|| io::Error::other("agent schedule revision exhausted"))?;
        state.updated_at_ms = now;
        if next == AgentScheduleState::Enabled {
            state.evaluation_frontier_ms = now.max(state.evaluation_frontier_ms);
            state.evaluation_frontier_trigger_revision = state.trigger_revision;
        }
        let snapshot = schedule.snapshot_from(&state)?;
        drop(state);
        Ok((
            snapshot,
            Some(DurableUndo::ScheduleState { schedule, previous }),
        ))
    }

    fn update_schedule_at(
        &self,
        schedule_id: &str,
        expected_revision: u64,
        mut update: AgentScheduleUpdate,
        requested_at_ms: u64,
    ) -> DaemonResult<(AgentScheduleSnapshot, Option<DurableUndo>)> {
        validate_name(&update.name)?;
        crate::scheduling::validate_prompt(&update.prompt).map_err(schedule_validation_error)?;
        update.trigger.cron = crate::scheduling::canonicalize_cron(&update.trigger.cron)
            .map_err(schedule_validation_error)?;
        update.trigger.timezone =
            crate::scheduling::canonicalize_timezone(&update.trigger.timezone)
                .map_err(schedule_validation_error)?;
        crate::scheduling::CronSchedule::compile(&update.trigger.cron, &update.trigger.timezone)
            .and_then(|cron| cron.ensure_possible())
            .map_err(schedule_validation_error)?;

        let schedule = self.schedule(schedule_id)?;
        {
            let state = lock(&schedule.state)?;
            if state.revision != expected_revision {
                return Err(DaemonError::lifecycle(
                    ErrorCode::RevisionAhead,
                    format!(
                        "agent schedule revision is {}; update supplied {}",
                        state.revision, expected_revision
                    ),
                ));
            }
            if state.state != AgentScheduleState::Paused {
                return Err(io::Error::new(
                    io::ErrorKind::WouldBlock,
                    "agent schedule must be paused before editing",
                )
                .into());
            }
            if state.name == update.name
                && state.prompt == update.prompt
                && state.trigger == update.trigger
            {
                return Ok((schedule.snapshot_from(&state)?, None));
            }
        }

        let workspace = self.workspace(&schedule.workspace_id)?;
        let durable = lock(&self.state)?;
        let schedule_ids = lock(&workspace.schedule_ids)?;
        if schedule_ids
            .iter()
            .filter(|id| id.as_str() != schedule_id)
            .any(|id| {
                durable
                    .schedules
                    .get(id)
                    .and_then(|existing| existing.state.lock().ok())
                    .is_some_and(|state| state.name == update.name)
            })
        {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!("agent schedule name already exists: {}", update.name),
            )
            .into());
        }
        drop(schedule_ids);
        drop(durable);

        let mut state = lock(&schedule.state)?;
        if state.revision != expected_revision {
            return Err(DaemonError::lifecycle(
                ErrorCode::RevisionAhead,
                "agent schedule changed while preparing the update",
            ));
        }
        if state.state != AgentScheduleState::Paused {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "agent schedule must be paused before editing",
            )
            .into());
        }
        let previous = state.clone();
        let revision = state
            .revision
            .checked_add(1)
            .ok_or_else(|| io::Error::other("agent schedule revision exhausted"))?;
        let prompt_changed = state.prompt != update.prompt;
        let trigger_changed = state.trigger != update.trigger;
        state.name = update.name;
        state.prompt = update.prompt;
        state.trigger = update.trigger;
        state.revision = revision;
        state.updated_at_ms = requested_at_ms.max(state.updated_at_ms.saturating_add(1));
        if prompt_changed {
            state.prompt_revision = revision;
        }
        if trigger_changed {
            state.trigger_revision = revision;
            state.evaluation_frontier_ms = state.updated_at_ms;
            state.evaluation_frontier_trigger_revision = revision;
        }
        let snapshot = schedule.snapshot_from(&state)?;
        drop(state);
        Ok((
            snapshot,
            Some(DurableUndo::ScheduleState { schedule, previous }),
        ))
    }

    fn next_workspace_name(&self) -> io::Result<String> {
        let state = lock(&self.state)?;
        let mut suffix = 1_u64;
        loop {
            let candidate = format!("workspace-{suffix}");
            let exists = state
                .workspaces
                .values()
                .any(|workspace| workspace.name.lock().is_ok_and(|name| *name == candidate));
            if !exists {
                return Ok(candidate);
            }
            suffix = suffix
                .checked_add(1)
                .ok_or_else(|| io::Error::other("workspace name space exhausted"))?;
        }
    }

    fn rename_shell(&self, shell_id: &str, name: String) -> io::Result<Option<DurableUndo>> {
        validate_name(&name)?;
        let shell = self.shell(shell_id)?;
        if !matches!(shell.owner, ShellOwner::User) {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "schedule-owned shells cannot be renamed",
            ));
        }
        let workspace = self.workspace(&shell.workspace_id)?;
        let state = lock(&self.state)?;
        let shell_ids = lock(&workspace.shell_ids)?;
        if shell_ids
            .iter()
            .filter(|id| id.as_str() != shell_id)
            .any(|id| {
                state
                    .shells
                    .get(id)
                    .and_then(|existing| existing.name.lock().ok())
                    .is_some_and(|existing| *existing == name)
            })
        {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!("shell name already exists: {name}"),
            ));
        }
        let mut revision = lock(&shell.revision)?;
        let mut current_name = lock(&shell.name)?;
        if *current_name == name {
            return Ok(None);
        }
        let previous_revision = *revision;
        *revision = revision
            .checked_add(1)
            .ok_or_else(|| io::Error::other("shell revision exhausted"))?;
        let previous = std::mem::replace(&mut *current_name, name);
        drop(current_name);
        drop(revision);
        Ok(Some(DurableUndo::RenamedShell {
            shell,
            previous,
            previous_revision,
        }))
    }

    fn rename_launcher(&self, launcher_id: &str, name: String) -> io::Result<Option<DurableUndo>> {
        validate_name(&name)?;
        let launcher = self.launcher(launcher_id)?;
        let workspace = self.workspace(&launcher.workspace_id)?;
        let state = lock(&self.state)?;
        let launcher_ids = lock(&workspace.launcher_ids)?;
        if launcher_ids
            .iter()
            .filter(|id| id.as_str() != launcher_id)
            .any(|id| {
                state
                    .launchers
                    .get(id)
                    .and_then(|existing| existing.name.lock().ok())
                    .is_some_and(|existing| *existing == name)
            })
        {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!("workspace launcher name already exists: {name}"),
            ));
        }
        let mut revision = lock(&launcher.revision)?;
        let mut current_name = lock(&launcher.name)?;
        if *current_name == name {
            return Ok(None);
        }
        let previous_revision = *revision;
        *revision = revision
            .checked_add(1)
            .ok_or_else(|| io::Error::other("workspace launcher revision exhausted"))?;
        let previous = std::mem::replace(&mut *current_name, name);
        drop(current_name);
        drop(revision);
        Ok(Some(DurableUndo::RenamedLauncher {
            launcher,
            previous,
            previous_revision,
        }))
    }

    fn rename_workspace(
        &self,
        workspace_id: &str,
        name: String,
    ) -> io::Result<Option<DurableUndo>> {
        validate_name(&name)?;
        let workspace = self.workspace(workspace_id)?;
        let state = lock(&self.state)?;
        for current in state.workspaces.values() {
            if !Arc::ptr_eq(current, &workspace) && *lock(&current.name)? == name {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    format!("workspace name already exists: {name}"),
                ));
            }
        }
        let mut revision = lock(&workspace.revision)?;
        let mut current_name = lock(&workspace.name)?;
        if *current_name == name {
            return Ok(None);
        }
        let previous_revision = *revision;
        *revision = revision
            .checked_add(1)
            .ok_or_else(|| io::Error::other("workspace revision exhausted"))?;
        let previous = std::mem::replace(&mut *current_name, name);
        drop(current_name);
        drop(revision);
        Ok(Some(DurableUndo::RenamedWorkspace {
            workspace,
            previous,
            previous_revision,
        }))
    }

    fn register_agent(
        &self,
        shell_id: &str,
        run_id: &str,
        spec: AgentRegistrationSpec,
    ) -> DaemonResult<(AgentInstanceSnapshot, DurableUndo)> {
        validate_agent_registration(&spec)?;
        validate_external_agent_authority(spec.report.authority)?;
        let shell = self.shell(shell_id)?;
        let workspace = self.workspace(&shell.workspace_id)?;
        let started_at_ms = unix_time_ms();
        let ended_at_ms = (spec.report.state == AgentState::Done).then_some(started_at_ms);
        let agent = Arc::new(AgentInstance {
            id: Uuid::new_v4().to_string(),
            workspace_id: shell.workspace_id.clone(),
            shell_id: shell.id.clone(),
            run_id: run_id.into(),
            name: spec.name,
            integration: spec.integration,
            external_session_id: spec.external_session_id,
            cwd: Some(shell.cwd.clone()),
            started_at_ms,
            state: Mutex::new(AgentInstanceState {
                ended_at_ms,
                observation: AgentObservationSnapshot {
                    revision: 1,
                    state: spec.report.state,
                    authority: spec.report.authority,
                    evidence: spec.report.evidence,
                    confidence: spec.report.confidence,
                    observed_at_ms: started_at_ms,
                },
                attention: None,
            }),
        });
        {
            let mut agent_state = lock(&agent.state)?;
            agent_state.attention = attention_for_observation(&agent_state.observation);
        }
        let snapshot = agent.snapshot()?;
        let mut state = lock(&self.state)?;
        let Some(current_shell) = state.shells.get(shell_id) else {
            return Err(not_found("shell", shell_id).into());
        };
        let Some(current_workspace) = state.workspaces.get(&shell.workspace_id) else {
            return Err(not_found("workspace", &shell.workspace_id).into());
        };
        if !Arc::ptr_eq(current_shell, &shell) || !Arc::ptr_eq(current_workspace, &workspace) {
            return Err(not_found("shell", shell_id).into());
        }
        let lifecycle = lock(&shell.lifecycle)?;
        match &*lifecycle {
            ShellLifecycle::Running { run, .. } if run.id == run_id => {}
            ShellLifecycle::Exited { run, .. }
                if run.id == run_id && matches!(shell.owner, ShellOwner::Schedule { .. }) => {}
            ShellLifecycle::Running { .. }
            | ShellLifecycle::Exited { .. }
            | ShellLifecycle::Pending => {
                return Err(DaemonError::lifecycle(
                    ErrorCode::RunChanged,
                    "shell does not have the requested active run",
                ));
            }
            ShellLifecycle::Closed => return Err(not_found("shell", shell_id).into()),
        }
        let mut agent_ids = lock(&workspace.agent_ids)?;
        state.agents.insert(agent.id.clone(), Arc::clone(&agent));
        agent_ids.push(agent.id.clone());
        bump_revision(&workspace.revision, "workspace")?;
        drop(agent_ids);
        drop(lifecycle);
        drop(state);
        Ok((snapshot, DurableUndo::RegisteredAgent { workspace, agent }))
    }

    fn ensure_agent(
        &self,
        shell_id: &str,
        run_id: &str,
        spec: AgentRegistrationSpec,
    ) -> DaemonResult<(AgentInstanceSnapshot, bool, Option<DurableUndo>)> {
        validate_agent_registration(&spec)?;
        validate_external_agent_authority(spec.report.authority)?;
        let external_session_id = spec.external_session_id.as_deref().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "agent external_session_id is required for ensure",
            )
        })?;
        let matches = {
            let state = lock(&self.state)?;
            state
                .agents
                .values()
                .filter(|agent| {
                    agent.integration == spec.integration
                        && agent.external_session_id.as_deref() == Some(external_session_id)
                        && agent.shell_id == shell_id
                        && agent.run_id == run_id
                })
                .cloned()
                .collect::<Vec<_>>()
        };
        let existing = match matches.as_slice() {
            [] => None,
            [agent] => Some(Arc::clone(agent)),
            agents => {
                let mut active = Vec::new();
                for agent in agents {
                    if lock(&agent.state)?.ended_at_ms.is_none() {
                        active.push(Arc::clone(agent));
                    }
                }
                match active.as_slice() {
                    [agent] => Some(Arc::clone(agent)),
                    _ => {
                        return Err(io::Error::new(
                            io::ErrorKind::AlreadyExists,
                            "multiple agent instances match the ensured identity",
                        )
                        .into());
                    }
                }
            }
        };
        if let Some(agent) = existing {
            return Ok((agent.snapshot()?, false, None));
        }
        self.register_agent(shell_id, run_id, spec)
            .map(|(agent, undo)| (agent, true, Some(undo)))
    }

    fn report_agent(
        &self,
        agent_id: &str,
        run_id: &str,
        report: AgentReport,
    ) -> DaemonResult<(AgentInstanceSnapshot, bool, bool, Option<DurableUndo>)> {
        validate_agent_report(&report)?;
        validate_external_agent_authority(report.authority)?;
        let agent = self.agent(agent_id)?;
        if agent.run_id != run_id {
            return Err(DaemonError::lifecycle(
                ErrorCode::RunChanged,
                "agent instance is bound to a different shell run",
            ));
        }
        let mut state = lock(&agent.state)?;
        if state.ended_at_ms.is_some() {
            if observation_matches_report(&state.observation, &report) {
                return Ok((agent.snapshot_from(&state), false, true, None));
            }
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "completed agent instance cannot be reported again",
            )
            .into());
        }
        let repeated_working = state.observation.state == AgentState::Working
            && report.state == AgentState::Working
            && state.observation.authority == report.authority
            && state.observation.confidence == report.confidence;
        if observation_matches_report(&state.observation, &report)
            || repeated_working
            || agent_authority_rank(report.authority)
                < agent_authority_rank(state.observation.authority)
        {
            return Ok((agent.snapshot_from(&state), false, false, None));
        }
        let revision = state
            .observation
            .revision
            .checked_add(1)
            .ok_or_else(|| io::Error::other("agent observation revision exhausted"))?;
        let observed_at_ms = unix_time_ms()
            .max(agent.started_at_ms)
            .max(state.observation.observed_at_ms);
        let completed = report.state == AgentState::Done;
        let previous = state.clone();
        state.observation = AgentObservationSnapshot {
            revision,
            state: report.state,
            authority: report.authority,
            evidence: report.evidence,
            confidence: report.confidence,
            observed_at_ms,
        };
        if let Some(attention) = attention_for_observation(&state.observation) {
            state.attention = Some(attention);
        }
        if completed {
            state.ended_at_ms = Some(observed_at_ms);
        }
        let snapshot = agent.snapshot_from(&state);
        drop(state);
        Ok((
            snapshot,
            true,
            completed,
            Some(DurableUndo::AgentState { agent, previous }),
        ))
    }

    fn acknowledge_agent_attention(
        &self,
        agent_id: &str,
        observation_revision: u64,
    ) -> DaemonResult<(AgentInstanceSnapshot, bool, Option<DurableUndo>)> {
        let agent = self.agent(agent_id)?;
        let mut state = lock(&agent.state)?;
        let Some(attention) = &state.attention else {
            return Ok((agent.snapshot_from(&state), false, None));
        };
        if attention.observation.revision != observation_revision {
            return Err(DaemonError::lifecycle(
                ErrorCode::RevisionAhead,
                format!(
                    "agent attention observation revision is {}; acknowledgment supplied {}",
                    attention.observation.revision, observation_revision
                ),
            ));
        }
        let previous = state.clone();
        state.attention = None;
        let snapshot = agent.snapshot_from(&state);
        drop(state);
        Ok((
            snapshot,
            true,
            Some(DurableUndo::AgentState { agent, previous }),
        ))
    }

    fn link_agent_execution(
        &self,
        agent: &AgentInstanceSnapshot,
    ) -> io::Result<Option<(ScheduledExecutionSnapshot, DurableUndo)>> {
        let shell = self.shell(&agent.shell_id)?;
        let ShellOwner::Schedule { schedule_id } = &shell.owner else {
            return Ok(None);
        };
        let schedule = self.schedule(schedule_id)?;
        if schedule.integration != agent.integration {
            return Ok(None);
        }
        let execution = lock(&schedule.executions)?
            .iter()
            .find(|execution| {
                lock(&execution.state).is_ok_and(|state| {
                    state.shell_id.as_deref() == Some(agent.shell_id.as_str())
                        && state.run_id.as_deref() == Some(agent.run_id.as_str())
                })
            })
            .cloned();
        let Some(execution) = execution else {
            return Ok(None);
        };
        match &execution.session {
            AgentScheduleSession::Fresh => {
                if agent.external_session_id.is_none() {
                    return Ok(None);
                }
            }
            AgentScheduleSession::Continue {
                external_session_id,
            } if agent.external_session_id.as_deref() != Some(external_session_id.as_str()) => {
                return Ok(None);
            }
            AgentScheduleSession::Continue { .. } => {}
        }
        let current = execution.snapshot()?;
        if current.agent_id.as_deref() == Some(agent.id.as_str())
            && current.external_session_id == agent.external_session_id
        {
            return Ok(None);
        }
        if current.agent_id.is_some() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "scheduled execution is already linked to a different Agent",
            ));
        }
        let (snapshot, undo) = self.mutate_execution(&execution, |state| {
            state.agent_id = Some(agent.id.clone());
            state.external_session_id = agent.external_session_id.clone();
            Ok(())
        })?;
        Ok(Some((snapshot, undo)))
    }

    fn workspace_shells(&self, workspace: &Arc<Workspace>) -> io::Result<Vec<Arc<Shell>>> {
        let state = lock(&self.state)?;
        let Some(current) = state.workspaces.get(&workspace.id) else {
            return Err(not_found("workspace", &workspace.id));
        };
        if !Arc::ptr_eq(current, workspace) {
            return Err(not_found("workspace", &workspace.id));
        }
        Ok(lock(&workspace.shell_ids)?
            .iter()
            .filter_map(|id| state.shells.get(id).cloned())
            .collect())
    }

    fn rollback(&self, undo: DurableUndo) -> io::Result<()> {
        match undo {
            DurableUndo::CreatedWorkspace { workspace, shells } => {
                let mut state = lock(&self.state)?;
                if state
                    .workspaces
                    .get(&workspace.id)
                    .is_some_and(|current| Arc::ptr_eq(current, &workspace))
                {
                    state.workspaces.remove(&workspace.id);
                }
                for shell in shells {
                    if state
                        .shells
                        .get(&shell.id)
                        .is_some_and(|current| Arc::ptr_eq(current, &shell))
                    {
                        state.shells.remove(&shell.id);
                    }
                }
            }
            DurableUndo::CreatedShell { workspace, shell } => {
                let mut state = lock(&self.state)?;
                if state
                    .shells
                    .get(&shell.id)
                    .is_some_and(|current| Arc::ptr_eq(current, &shell))
                {
                    state.shells.remove(&shell.id);
                }
                lock(&workspace.shell_ids)?.retain(|id| id != &shell.id);
                rollback_bump(&workspace.revision)?;
            }
            DurableUndo::CreatedLauncher {
                workspace,
                launcher,
            } => {
                let mut state = lock(&self.state)?;
                if state
                    .launchers
                    .get(&launcher.id)
                    .is_some_and(|current| Arc::ptr_eq(current, &launcher))
                {
                    state.launchers.remove(&launcher.id);
                }
                lock(&workspace.launcher_ids)?.retain(|id| id != &launcher.id);
                rollback_bump(&workspace.revision)?;
            }
            DurableUndo::CreatedSchedule {
                workspace,
                schedule,
            } => {
                let mut state = lock(&self.state)?;
                if state
                    .schedules
                    .get(&schedule.id)
                    .is_some_and(|current| Arc::ptr_eq(current, &schedule))
                {
                    state.schedules.remove(&schedule.id);
                }
                lock(&workspace.schedule_ids)?.retain(|id| id != &schedule.id);
                rollback_bump(&workspace.revision)?;
            }
            DurableUndo::RegisteredAgent { workspace, agent } => {
                let mut state = lock(&self.state)?;
                if state
                    .agents
                    .get(&agent.id)
                    .is_some_and(|current| Arc::ptr_eq(current, &agent))
                {
                    state.agents.remove(&agent.id);
                }
                lock(&workspace.agent_ids)?.retain(|id| id != &agent.id);
                rollback_bump(&workspace.revision)?;
            }
            DurableUndo::RenamedWorkspace {
                workspace,
                previous,
                previous_revision,
            } => {
                *lock(&workspace.name)? = previous;
                *lock(&workspace.revision)? = previous_revision;
            }
            DurableUndo::RenamedShell {
                shell,
                previous,
                previous_revision,
            } => {
                *lock(&shell.name)? = previous;
                *lock(&shell.revision)? = previous_revision;
            }
            DurableUndo::RenamedLauncher {
                launcher,
                previous,
                previous_revision,
            } => {
                *lock(&launcher.name)? = previous;
                *lock(&launcher.revision)? = previous_revision;
            }
            DurableUndo::AgentState { agent, previous } => {
                *lock(&agent.state)? = previous;
            }
            DurableUndo::ScheduleState { schedule, previous } => {
                *lock(&schedule.state)? = previous;
            }
            DurableUndo::ScheduleExecutions {
                schedule,
                previous,
                previous_dispatch_key_filter,
                execution,
            } => {
                if let Some((execution, state)) = execution {
                    *lock(&execution.state)? = state;
                }
                *lock(&schedule.executions)? = previous;
                *lock(&schedule.dispatch_key_filter)? = previous_dispatch_key_filter;
            }
            DurableUndo::RemovedLauncher {
                workspace,
                launcher,
                index,
            } => {
                lock(&self.state)?
                    .launchers
                    .insert(launcher.id.clone(), Arc::clone(&launcher));
                lock(&workspace.launcher_ids)?.insert(index, launcher.id.clone());
                rollback_bump(&workspace.revision)?;
            }
            DurableUndo::RemovedSchedule {
                workspace,
                schedule,
                index,
            } => {
                lock(&self.state)?
                    .schedules
                    .insert(schedule.id.clone(), Arc::clone(&schedule));
                lock(&workspace.schedule_ids)?.insert(index, schedule.id.clone());
                rollback_bump(&workspace.revision)?;
            }
            DurableUndo::RemovedShell {
                workspace,
                shell,
                index,
            } => {
                lock(&self.state)?
                    .shells
                    .insert(shell.id.clone(), Arc::clone(&shell));
                lock(&workspace.shell_ids)?.insert(index, shell.id.clone());
                rollback_bump(&workspace.revision)?;
            }
            DurableUndo::RemovedWorkspace {
                workspace,
                shells,
                launchers,
                agents,
                schedules,
            } => {
                let mut state = lock(&self.state)?;
                for shell in shells {
                    state.shells.insert(shell.id.clone(), shell);
                }
                for launcher in launchers {
                    state.launchers.insert(launcher.id.clone(), launcher);
                }
                for agent in agents {
                    state.agents.insert(agent.id.clone(), agent);
                }
                for schedule in schedules {
                    state.schedules.insert(schedule.id.clone(), schedule);
                }
                state.workspaces.insert(workspace.id.clone(), workspace);
            }
        }
        Ok(())
    }

    fn state_lock_descriptor(&self) -> io::Result<BorrowedFd<'_>> {
        self.store
            .as_ref()
            .ok_or_else(|| io::Error::other("registry has no persistent state store"))?
            .lock_descriptor()
    }

    fn mark_persistence_dirty(&self) {
        self.persistence_revision.fetch_add(1, Ordering::AcqRel);
        self.persistence_dirty.store(true, Ordering::Release);
    }

    fn snapshot(&self, focused_terminal: Option<FocusedTerminalSnapshot>) -> io::Result<Snapshot> {
        let mut workspaces: Vec<_> = lock(&self.state)?.workspaces.values().cloned().collect();
        workspaces.sort_by(|a, b| a.id.cmp(&b.id));
        Ok(Snapshot {
            workspaces: workspaces
                .into_iter()
                .map(|workspace| workspace.snapshot(self))
                .collect::<io::Result<_>>()?,
            focused_terminal,
            scheduler: None,
        })
    }

    fn node_projection(
        &self,
        node_id: String,
        scheduler: SchedulerHealth,
    ) -> io::Result<NodeProjectionSnapshot> {
        let (workspaces, shells, launchers, agents, schedules) = {
            let state = lock(&self.state)?;
            (
                state.workspaces.values().cloned().collect::<Vec<_>>(),
                state.shells.values().cloned().collect::<Vec<_>>(),
                state.launchers.values().cloned().collect::<Vec<_>>(),
                state.agents.values().cloned().collect::<Vec<_>>(),
                state.schedules.values().cloned().collect::<Vec<_>>(),
            )
        };
        let mut projected_workspaces = Vec::with_capacity(workspaces.len());
        for workspace in workspaces {
            let item_count = lock(&workspace.shell_ids)?.len()
                + lock(&workspace.launcher_ids)?.len()
                + lock(&workspace.schedule_ids)?.len();
            let agent_ids = lock(&workspace.agent_ids)?.clone();
            let attention_count = agents
                .iter()
                .filter(|agent| agent_ids.contains(&agent.id))
                .filter_map(|agent| lock(&agent.state).ok())
                .filter(|state| state.attention.is_some())
                .count();
            projected_workspaces.push(NodeProjectionWorkspace {
                id: workspace.id.clone(),
                name: lock(&workspace.name)?.clone(),
                item_count: u32::try_from(item_count).unwrap_or(u32::MAX),
                attention_count: u32::try_from(attention_count).unwrap_or(u32::MAX),
            });
        }
        let mut projected_shells = shells
            .iter()
            .map(|shell| shell.node_projection())
            .collect::<io::Result<Vec<_>>>()?;
        let mut projected_launchers = launchers
            .iter()
            .map(|launcher| launcher.node_projection())
            .collect::<io::Result<Vec<_>>>()?;
        let mut projected_agents = agents
            .iter()
            .map(|agent| agent.node_projection())
            .collect::<io::Result<Vec<_>>>()?;
        let mut projected_schedules = schedules
            .iter()
            .map(|schedule| schedule.node_projection())
            .collect::<io::Result<Vec<_>>>()?;
        let mut executions = Vec::new();
        for schedule in schedules {
            for execution in lock(&schedule.executions)?.iter() {
                executions.push(execution.node_projection()?);
            }
        }
        executions.sort_by(|left, right| {
            left.state
                .is_terminal()
                .cmp(&right.state.is_terminal())
                .then_with(|| {
                    right
                        .requested_at_ms
                        .cmp(&left.requested_at_ms)
                        .then_with(|| left.id.cmp(&right.id))
                })
        });
        let execution_limit = usize::from(protocol::MAX_NODE_PROJECTION_EXECUTIONS);
        let executions_truncated = executions.len() > execution_limit;
        executions.truncate(execution_limit);
        projected_workspaces.sort_by(|left, right| left.id.cmp(&right.id));
        projected_shells.sort_by(|left, right| left.id.cmp(&right.id));
        projected_launchers.sort_by(|left, right| left.id.cmp(&right.id));
        projected_agents.sort_by(|left, right| left.id.cmp(&right.id));
        projected_schedules.sort_by(|left, right| left.id.cmp(&right.id));
        Ok(NodeProjectionSnapshot {
            node_id,
            workspaces: projected_workspaces,
            shells: projected_shells,
            launchers: projected_launchers,
            agents: projected_agents,
            schedules: projected_schedules,
            executions,
            executions_truncated,
            scheduler,
        })
    }

    fn shells(&self) -> io::Result<Vec<Arc<Shell>>> {
        Ok(lock(&self.state)?.shells.values().cloned().collect())
    }

    fn schedule_shell_ids(&self) -> io::Result<HashSet<String>> {
        let schedules = lock(&self.state)?
            .schedules
            .values()
            .cloned()
            .collect::<Vec<_>>();
        let mut ids = HashSet::new();
        for schedule in schedules {
            if let Some(shell_id) = lock(&schedule.state)?.execution_shell_id.clone() {
                ids.insert(shell_id);
            }
        }
        Ok(ids)
    }

    fn capture_persisted_state(&self) -> io::Result<PersistenceGeneration> {
        let executions = self.scheduled_executions(None, None)?;
        let (mut workspaces, shells_by_id, launchers_by_id, agents_by_id, schedules_by_id) = {
            let state = lock(&self.state)?;
            (
                state.workspaces.values().cloned().collect::<Vec<_>>(),
                state.shells.clone(),
                state.launchers.clone(),
                state.agents.clone(),
                state.schedules.clone(),
            )
        };
        workspaces.sort_by(|left, right| left.id.cmp(&right.id));
        let mut saved = PersistedState::default();
        for workspace in workspaces {
            let ids = lock(&workspace.shell_ids)?.clone();
            let mut shells = Vec::with_capacity(ids.len());
            for id in ids {
                let Some(shell) = shells_by_id.get(&id) else {
                    continue;
                };
                shells.push(PersistedShell {
                    id: shell.id.clone(),
                    revision: *lock(&shell.revision)?,
                    name: lock(&shell.name)?.clone(),
                    cwd: shell.cwd.clone(),
                    command: shell.command.clone(),
                    owner: shell.owner.clone(),
                    last_run: lock(&shell.last_run)?.clone(),
                });
            }
            let ids = lock(&workspace.launcher_ids)?.clone();
            let mut launchers = Vec::with_capacity(ids.len());
            for id in ids {
                let Some(launcher) = launchers_by_id.get(&id) else {
                    continue;
                };
                launchers.push(PersistedWorkspaceLauncher {
                    id: launcher.id.clone(),
                    revision: *lock(&launcher.revision)?,
                    name: lock(&launcher.name)?.clone(),
                    cwd: launcher.cwd.clone(),
                    command: launcher.command.clone(),
                });
            }
            let ids = lock(&workspace.agent_ids)?.clone();
            let mut agents = Vec::with_capacity(ids.len());
            for id in ids {
                let Some(agent) = agents_by_id.get(&id) else {
                    continue;
                };
                agents.push(agent.persisted()?);
            }
            let ids = lock(&workspace.schedule_ids)?.clone();
            let mut schedules = Vec::with_capacity(ids.len());
            for id in ids {
                let Some(schedule) = schedules_by_id.get(&id) else {
                    continue;
                };
                schedules.push(schedule.persisted()?);
            }
            saved.workspaces.push(PersistedWorkspace {
                id: workspace.id.clone(),
                revision: *lock(&workspace.revision)?,
                name: lock(&workspace.name)?.clone(),
                default_cwd: workspace.default_cwd.clone(),
                shells,
                launchers,
                agents,
                schedules,
            });
        }
        Ok(PersistenceGeneration {
            revision: self.persistence_revision.load(Ordering::Acquire),
            state: saved,
            executions,
        })
    }

    fn write_persisted_state(&self, generation: PersistenceGeneration) -> io::Result<()> {
        let Some(writer) = &self.persistence_writer else {
            return Ok(());
        };
        let revision = generation.revision;
        let result = writer.save(generation);
        if result.is_err() {
            self.persistence_dirty.store(true, Ordering::Release);
        } else if self.persistence_revision.load(Ordering::Acquire) == revision {
            self.persistence_dirty.store(false, Ordering::Release);
        }
        result
    }
}

impl ShellRuntimeManager {
    fn is_stopping(&self) -> bool {
        self.stopping.load(Ordering::Acquire)
    }

    fn begin_stopping(&self) -> bool {
        !self.stopping.swap(true, Ordering::AcqRel)
    }

    fn cancel_stopping(&self) {
        self.stopping.store(false, Ordering::Release);
    }

    fn focused_terminal(&self) -> io::Result<Option<FocusedTerminalSnapshot>> {
        Ok(lock(&self.focus)?.focused_terminal.clone())
    }

    fn record_focus_gained(
        &self,
        workspace_id: String,
        shell_id: String,
        run_id: String,
    ) -> io::Result<()> {
        let mut focus = lock(&self.focus)?;
        let revision = focus
            .revision
            .checked_add(1)
            .ok_or_else(|| io::Error::other("focused terminal revision exhausted"))?;
        focus.revision = revision;
        focus.focused_terminal = Some(FocusedTerminalSnapshot {
            revision,
            workspace_id,
            shell_id,
            run_id,
        });
        Ok(())
    }

    fn import_focused_terminal(
        &self,
        focused_terminal: FocusedTerminalSnapshot,
        current: bool,
    ) -> io::Result<()> {
        let mut focus = lock(&self.focus)?;
        if focused_terminal.revision >= focus.revision {
            focus.revision = focused_terminal.revision;
            focus.focused_terminal = current.then_some(focused_terminal);
        }
        Ok(())
    }

    fn notify_output_waiters(&self, shells: &[Arc<Shell>]) {
        for shell in shells {
            if let Ok(lifecycle) = shell.lifecycle.lock()
                && let ShellLifecycle::Running { runtime, .. } = &*lifecycle
            {
                let _wait = runtime.output_wait.lock();
                runtime.output_changed.notify_all();
            }
        }
    }

    fn read_shell(&self, shell: &Shell, max_bytes: usize) -> io::Result<Vec<u8>> {
        let lifecycle = lock(&shell.lifecycle)?;
        let terminal = match &*lifecycle {
            ShellLifecycle::Pending => return Ok(Vec::new()),
            ShellLifecycle::Running { runtime, .. } => Arc::clone(&runtime.terminal),
            ShellLifecycle::Exited { terminal, .. } => Arc::clone(terminal),
            ShellLifecycle::Closed => return Err(not_found("shell", &shell.id)),
        };
        drop(lifecycle);
        let max_bytes = max_bytes.min(MAX_SHELL_READ_BYTES);
        let snapshot = lock(&terminal)?.snapshot();
        Ok(snapshot.plain_text_suffix(max_bytes).into_bytes())
    }

    fn foreground_process(
        shell: &Shell,
        runtime: &ShellRuntime,
        run: &ShellRunSnapshot,
    ) -> io::Result<Option<String>> {
        let mut cache = lock(&shell.foreground_process_cache)?;
        if let Some((cached_run_id, observed_at, process)) = cache.as_ref()
            && cached_run_id == &run.id
            && observed_at.elapsed() < FOREGROUND_PROCESS_CACHE_INTERVAL
        {
            return Ok(process.clone());
        }
        let process = lock(&runtime.master)
            .ok()
            .and_then(|master| master.foreground_process())
            .or_else(|| {
                let pid = lock(&runtime.process).ok()?.process_id()?;
                foreground_process_for_session_leader(pid)
            });
        *cache = Some((run.id.clone(), Instant::now(), process.clone()));
        Ok(process)
    }

    fn read_shell_preview(
        &self,
        shell: &Shell,
        max_bytes: usize,
        max_lines: u16,
    ) -> io::Result<TerminalPreview> {
        let lifecycle = lock(&shell.lifecycle)?;
        let terminal = match &*lifecycle {
            ShellLifecycle::Pending => return Ok(TerminalPreview::default()),
            ShellLifecycle::Running { runtime, .. } => Arc::clone(&runtime.terminal),
            ShellLifecycle::Exited { terminal, .. } => Arc::clone(terminal),
            ShellLifecycle::Closed => return Err(not_found("shell", &shell.id)),
        };
        drop(lifecycle);
        let snapshot = lock(&terminal)?.snapshot();
        Ok(snapshot.preview(
            max_bytes.min(MAX_SHELL_READ_BYTES),
            usize::from(max_lines).min(MAX_TERMINAL_PREVIEW_LINES),
            MAX_TERMINAL_PREVIEW_SPANS,
        ))
    }

    fn read_shell_at(
        &self,
        service: &DaemonService,
        shell_id: &str,
        max_bytes: usize,
        expected_run_id: Option<&str>,
        after_revision: Option<u64>,
        wait_ms: u32,
    ) -> DaemonResult<Response> {
        if expected_run_id.is_some() != after_revision.is_some() {
            return Err(DaemonError::validation(
                "run_id and after_revision must be provided together",
            ));
        }
        let deadline =
            Instant::now() + Duration::from_millis(u64::from(wait_ms)).min(MAX_EVENT_WAIT);
        loop {
            let shell = service.shell(shell_id)?;
            let lifecycle = lock(&shell.lifecycle)?;
            let (status, run, runtime, terminal) = match &*lifecycle {
                ShellLifecycle::Pending => {
                    if expected_run_id.is_some() {
                        return Err(DaemonError::lifecycle(
                            ErrorCode::RunChanged,
                            "shell no longer has the requested run",
                        ));
                    }
                    return Ok(Response::OutputState {
                        bytes: Vec::new(),
                        run_id: None,
                        output_revision: None,
                        changed: true,
                        status: ShellStatus::Pending,
                    });
                }
                ShellLifecycle::Running { run, runtime, .. } => (
                    ShellStatus::Running,
                    Arc::clone(run),
                    Some(Arc::clone(runtime)),
                    Arc::clone(&runtime.terminal),
                ),
                ShellLifecycle::Exited {
                    code,
                    run,
                    terminal,
                    ..
                } => (
                    ShellStatus::Exited { code: *code },
                    Arc::clone(run),
                    None,
                    Arc::clone(terminal),
                ),
                ShellLifecycle::Closed => return Err(not_found("shell", shell_id).into()),
            };
            drop(lifecycle);
            let terminal_state = lock(&terminal)?;
            let revision = run.output_revision.load(Ordering::Acquire);
            let changed = after_revision.is_none_or(|after| after < revision);
            let snapshot = if changed || after_revision.is_none() {
                Some(terminal_state.snapshot())
            } else {
                None
            };
            drop(terminal_state);
            let bytes = snapshot.map_or_else(Vec::new, |snapshot| {
                snapshot
                    .plain_text_suffix(max_bytes.min(MAX_SHELL_READ_BYTES))
                    .into_bytes()
            });
            let lifecycle = lock(&shell.lifecycle)?;
            let observation_is_current = match (&*lifecycle, &status) {
                (
                    ShellLifecycle::Running {
                        run: current_run,
                        runtime,
                        ..
                    },
                    ShellStatus::Running,
                ) => Arc::ptr_eq(current_run, &run) && Arc::ptr_eq(&runtime.terminal, &terminal),
                (
                    ShellLifecycle::Exited {
                        code,
                        run: current_run,
                        terminal: current_terminal,
                        ..
                    },
                    ShellStatus::Exited { code: current_code },
                ) => {
                    code == current_code
                        && Arc::ptr_eq(current_run, &run)
                        && Arc::ptr_eq(current_terminal, &terminal)
                }
                _ => false,
            };
            drop(lifecycle);
            if !observation_is_current {
                continue;
            }
            if expected_run_id.is_some_and(|expected| expected != run.id) {
                return Err(DaemonError::lifecycle(
                    ErrorCode::RunChanged,
                    "shell run identity changed",
                ));
            }
            if after_revision.is_some_and(|after| after > revision) {
                return Err(DaemonError::lifecycle(
                    ErrorCode::RevisionAhead,
                    "requested output revision is ahead of the current run",
                ));
            }
            if changed || !matches!(status, ShellStatus::Running) || wait_ms == 0 {
                return Ok(Response::OutputState {
                    bytes,
                    run_id: Some(run.id.clone()),
                    output_revision: Some(revision),
                    changed,
                    status,
                });
            }
            if Instant::now() >= deadline {
                return Ok(Response::OutputState {
                    bytes: Vec::new(),
                    run_id: Some(run.id.clone()),
                    output_revision: Some(revision),
                    changed: false,
                    status,
                });
            }
            if self.is_stopping() {
                return Err(DaemonError::lifecycle(
                    ErrorCode::DaemonStopping,
                    "Boomux daemon is stopping",
                ));
            }
            let runtime = runtime.expect("running shell has a runtime");
            let wait = lock(&runtime.output_wait)?;
            if self.is_stopping() {
                return Err(DaemonError::lifecycle(
                    ErrorCode::DaemonStopping,
                    "Boomux daemon is stopping",
                ));
            }
            if run.output_revision.load(Ordering::Acquire) != revision {
                continue;
            }
            let timeout = deadline.saturating_duration_since(Instant::now());
            let _ = runtime
                .output_changed
                .wait_timeout(wait, timeout)
                .map_err(|_| io::Error::other("shell output wait lock poisoned"))?;
        }
    }

    fn stop_runtime(&self, shell: &Shell) -> Result<(), StopRuntimeError> {
        let runtime = {
            let lifecycle = lock(&shell.lifecycle)?;
            match &*lifecycle {
                ShellLifecycle::Pending => return Ok(()),
                ShellLifecycle::Running { runtime, .. } => Some(Arc::clone(runtime)),
                ShellLifecycle::Exited { runtime, .. } => runtime.clone(),
                ShellLifecycle::Closed => return Ok(()),
            }
        };
        let Some(runtime) = runtime else {
            return Ok(());
        };
        if let Some(controller) = lock(&runtime.controller)?.take() {
            let _ = controller.connection.shutdown(std::net::Shutdown::Both);
        }
        self.pause_reader(&runtime)?;
        let result = (|| {
            let mut process = lock(&runtime.process)?;
            let exited = process.try_wait_code()?.is_some();
            let child_pid = process.process_id().map(|pid| pid as libc::pid_t);
            if let Some(session_id) = child_pid {
                signal_session(session_id, libc::SIGHUP);
            }
            thread::sleep(SHUTDOWN_GRACE);
            if let Some(session_id) = child_pid {
                signal_session(session_id, libc::SIGTERM);
            }
            let kill_result = if exited { Ok(()) } else { process.kill() };
            thread::sleep(SHUTDOWN_GRACE);
            if let Some(session_id) = child_pid {
                signal_session(session_id, libc::SIGKILL);
            }
            let wait_result = if exited { Ok(()) } else { process.wait() };
            match (kill_result, wait_result) {
                (_, Ok(())) => Ok(()),
                (Err(error), Err(_)) => Err(error),
                (Ok(()), Err(error)) => Err(error),
            }
        })();
        if let Err(error) = result {
            self.resume_reader(&runtime)?;
            return Err(error.into());
        }
        if let Some(session_id) = lock(&runtime.process)?.process_id()
            && let Err(source) = wait_for_session_descendants(session_id as libc::pid_t)
        {
            let _ = self.stop_reader(&runtime);
            return Err(StopRuntimeError {
                source,
                stopped: true,
            });
        }
        if let Err(source) = self.stop_reader(&runtime) {
            return Err(StopRuntimeError {
                source,
                stopped: true,
            });
        }
        Ok(())
    }

    fn finalize_stop(&self, shell: &Shell) -> io::Result<StopRollback> {
        let mut lifecycle = lock(&shell.lifecycle)?;
        match &*lifecycle {
            ShellLifecycle::Pending => {
                *lifecycle = ShellLifecycle::Closed;
                Ok(StopRollback {
                    lifecycle: ShellLifecycle::Pending,
                    event: None,
                })
            }
            ShellLifecycle::Running {
                profile,
                run,
                runtime,
            } => {
                run.finish(ShellRunExitReason::Terminated)?;
                let persisted = run.persisted(profile.clone())?;
                let event = DaemonEventKind::RunExited {
                    workspace_id: shell.workspace_id.clone(),
                    shell_id: shell.id.clone(),
                    run: run.snapshot()?,
                };
                *lock(&shell.last_run)? = Some(persisted);
                let _wait = lock(&runtime.output_wait)?;
                runtime.output_changed.notify_all();
                drop(_wait);
                *lifecycle = ShellLifecycle::Closed;
                Ok(StopRollback {
                    lifecycle: ShellLifecycle::Pending,
                    event: Some(event),
                })
            }
            ShellLifecycle::Exited {
                profile,
                run,
                code,
                runtime,
                terminal,
            } => {
                let persisted = run.persisted(profile.clone())?;
                *lock(&shell.last_run)? = Some(persisted);
                let rollback = StopRollback {
                    lifecycle: ShellLifecycle::Exited {
                        code: *code,
                        profile: profile.clone(),
                        run: Arc::clone(run),
                        runtime: runtime.clone(),
                        terminal: Arc::clone(terminal),
                    },
                    event: None,
                };
                *lifecycle = ShellLifecycle::Closed;
                Ok(rollback)
            }
            ShellLifecycle::Closed => Ok(StopRollback {
                lifecycle: ShellLifecycle::Closed,
                event: None,
            }),
        }
    }

    fn finalize_run_exit(
        &self,
        shell: &Shell,
        run: &Arc<ShellRun>,
        runtime: &Arc<ShellRuntime>,
        code: Option<u32>,
    ) -> io::Result<Option<DaemonEventKind>> {
        let mut lifecycle = lock(&shell.lifecycle)?;
        let profile = match &*lifecycle {
            ShellLifecycle::Running {
                profile,
                run: current_run,
                runtime: current_runtime,
            } if Arc::ptr_eq(current_run, run) && Arc::ptr_eq(current_runtime, runtime) => {
                profile.clone()
            }
            _ => return Ok(None),
        };
        run.finish(ShellRunExitReason::Exited { code })?;
        *lock(&shell.last_run)? = Some(run.persisted(profile.clone())?);
        *lifecycle = ShellLifecycle::Exited {
            code,
            profile,
            run: Arc::clone(run),
            runtime: Some(Arc::clone(runtime)),
            terminal: Arc::clone(&runtime.terminal),
        };
        drop(lifecycle);
        let _wait = lock(&runtime.output_wait)?;
        runtime.output_changed.notify_all();
        drop(_wait);
        Ok(Some(DaemonEventKind::RunExited {
            workspace_id: shell.workspace_id.clone(),
            shell_id: shell.id.clone(),
            run: run.snapshot()?,
        }))
    }

    fn run_exit_is_current(
        &self,
        shell: &Shell,
        run: &Arc<ShellRun>,
        runtime: &Arc<ShellRuntime>,
    ) -> io::Result<bool> {
        Ok(matches!(
            &*lock(&shell.lifecycle)?,
            ShellLifecycle::Running {
                run: current_run,
                runtime: current_runtime,
                ..
            } if Arc::ptr_eq(current_run, run) && Arc::ptr_eq(current_runtime, runtime)
        ))
    }

    fn restore_stopped(&self, shell: &Shell, rollback: StopRollback) -> io::Result<()> {
        let mut lifecycle = lock(&shell.lifecycle)?;
        if matches!(*lifecycle, ShellLifecycle::Closed) {
            *lifecycle = rollback.lifecycle;
        }
        Ok(())
    }

    fn compensate_stopped(&self, shell: &Shell) -> io::Result<Option<DaemonEventKind>> {
        let rollback = self.finalize_stop(shell)?;
        let event = rollback.event.clone();
        self.restore_stopped(shell, rollback)?;
        Ok(event)
    }

    fn reset_pending(&self, shell: &Shell) -> io::Result<()> {
        let mut lifecycle = lock(&shell.lifecycle)?;
        if matches!(*lifecycle, ShellLifecycle::Closed) {
            *lifecycle = ShellLifecycle::Pending;
        }
        Ok(())
    }

    fn kill(&self, shell: &Shell) -> io::Result<()> {
        self.kill_with_event(shell).map(|_| ())
    }

    fn kill_with_event(&self, shell: &Shell) -> io::Result<Option<DaemonEventKind>> {
        match self.stop_runtime(shell) {
            Ok(()) => {
                let rollback = self.finalize_stop(shell)?;
                Ok(rollback.event)
            }
            Err(error) => {
                if error.stopped {
                    let rollback = self.finalize_stop(shell)?;
                    return match rollback.event {
                        Some(_event) => Err(io::Error::new(
                            error.source.kind(),
                            format!("{}; run exit was finalized", error.source),
                        )),
                        None => Err(error.source),
                    };
                }
                Err(error.source)
            }
        }
    }

    fn pause_reader(&self, runtime: &ShellRuntime) -> io::Result<()> {
        if let Some(reader) = lock(&runtime.reader)?.as_ref() {
            reader.pause()?;
        }
        Ok(())
    }

    fn resume_reader(&self, runtime: &ShellRuntime) -> io::Result<()> {
        if let Some(reader) = lock(&runtime.reader)?.as_ref() {
            reader.resume()?;
        }
        Ok(())
    }

    fn stop_reader(&self, runtime: &ShellRuntime) -> io::Result<()> {
        let reader = lock(&runtime.reader)?.take();
        if let Some(reader) = reader {
            reader.stop()?;
        }
        Ok(())
    }

    fn release_controller(runtime: &ShellRuntime, token: &str) -> io::Result<()> {
        let mut controller = lock(&runtime.controller)?;
        if controller
            .as_ref()
            .is_some_and(|current| current.token == token)
        {
            controller.take();
        }
        Ok(())
    }

    fn quiesce_controllers(&self, runtimes: &[Arc<ShellRuntime>]) -> io::Result<()> {
        let mut acknowledgements = Vec::new();
        for runtime in runtimes {
            let mut controller = lock(&runtime.controller)?;
            let Some(current) = controller.as_mut() else {
                continue;
            };
            let (written, write_acknowledged) = mpsc::sync_channel(1);
            let (client_acknowledge, client_acknowledged) = mpsc::sync_channel(1);
            current.reconnect_ack = Some(client_acknowledge);
            match current
                .output
                .try_send(ControllerOutput::Reconnect(written))
            {
                Ok(()) => acknowledgements.push((write_acknowledged, client_acknowledged)),
                Err(TrySendError::Disconnected(_)) => {
                    if let Some(current) = controller.take() {
                        let _ = current.connection.shutdown(std::net::Shutdown::Both);
                    }
                }
                Err(TrySendError::Full(_)) => {
                    return Err(io::Error::new(
                        io::ErrorKind::WouldBlock,
                        "active controller output queue is full",
                    ));
                }
            }
        }

        for (written, client_acknowledged) in acknowledgements {
            if !written.recv_timeout(HANDSHAKE_TIMEOUT).unwrap_or(false) {
                return Err(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "active controller did not receive reconnect request",
                ));
            }
            if client_acknowledged.recv_timeout(HANDSHAKE_TIMEOUT).is_err() {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "active controller did not acknowledge reconnect request",
                ));
            }
        }

        let deadline = Instant::now() + HANDSHAKE_TIMEOUT;
        while Instant::now() < deadline {
            let mut active = false;
            for runtime in runtimes {
                active |= lock(&runtime.controller)?.is_some();
            }
            if !active {
                return Ok(());
            }
            thread::sleep(IO_RETRY_DELAY);
        }
        Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "active controllers did not reconnect",
        ))
    }

    fn prepare_handoff(&self, shells: Vec<Arc<Shell>>) -> io::Result<PreparedHandoff> {
        let mut runtimes = Vec::new();
        for shell in &shells {
            if let ShellLifecycle::Running { runtime, .. } = &*lock(&shell.lifecycle)? {
                runtimes.push(Arc::clone(runtime));
            }
        }
        self.quiesce_controllers(&runtimes)?;

        let mut paused = Vec::new();
        let result = (|| {
            let mut transfers = Vec::new();
            let mut exited = Vec::new();
            for shell in shells {
                let lifecycle = lock(&shell.lifecycle)?;
                let (profile, run, runtime) = match &*lifecycle {
                    ShellLifecycle::Pending => continue,
                    ShellLifecycle::Running {
                        profile,
                        run,
                        runtime,
                    } => (profile.clone(), Arc::clone(run), Arc::clone(runtime)),
                    ShellLifecycle::Exited {
                        code,
                        profile,
                        run,
                        terminal,
                        ..
                    } => {
                        exited.push(OutgoingExited {
                            manifest: handoff::ExitedManifest {
                                shell_id: shell.id.clone(),
                                run_id: run.id.clone(),
                                output_revision: run.output_revision.load(Ordering::Acquire),
                                profile: profile.clone(),
                                code: *code,
                            },
                            reconstruction: lock(terminal)?.reconstruction(),
                        });
                        continue;
                    }
                    ShellLifecycle::Closed => return Err(not_found("shell", &shell.id)),
                };
                self.pause_reader(&runtime)?;
                paused.push(Arc::clone(&runtime));
                let (pid, pidfd) = lock(&runtime.process)?.transfer_identity()?;
                let pty = lock(&runtime.master)?.descriptor.try_clone()?;
                let reconstruction = lock(&runtime.terminal)?.reconstruction();
                transfers.push(OutgoingRuntime {
                    manifest: handoff::RuntimeManifest {
                        shell_id: shell.id.clone(),
                        run_id: Some(run.id.clone()),
                        output_revision: Some(run.output_revision.load(Ordering::Acquire)),
                        profile,
                        pid,
                    },
                    pty,
                    pidfd,
                    reconstruction,
                });
            }
            Ok((transfers, exited))
        })();
        match result {
            Ok((runtimes, exited)) => Ok(PreparedHandoff {
                runtimes,
                exited,
                paused,
            }),
            Err(error) => {
                for runtime in paused {
                    let _ = self.resume_reader(&runtime);
                }
                Err(error)
            }
        }
    }
}

#[derive(Default)]
struct DurableState {
    workspaces: HashMap<String, Arc<Workspace>>,
    shells: HashMap<String, Arc<Shell>>,
    launchers: HashMap<String, Arc<WorkspaceLauncher>>,
    agents: HashMap<String, Arc<AgentInstance>>,
    schedules: HashMap<String, Arc<AgentSchedule>>,
}

impl Default for DaemonService {
    fn default() -> Self {
        Self {
            node_identity: None,
            node_registrations: None,
            node_projection_cache: None,
            durable: DurableRegistry {
                state: Mutex::new(DurableState::default()),
                store: None,
                persistence_writer: None,
                persist_lock: Mutex::new(()),
                persistence_dirty: AtomicBool::new(false),
                persistence_revision: AtomicU64::new(0),
            },
            events: EventStream::new(),
            runtimes: ShellRuntimeManager {
                focus: Mutex::new(FocusState::default()),
                stopping: AtomicBool::new(false),
            },
            remote_attachments: RemoteAttachmentManager::default(),
            mutation_lock: Mutex::new(()),
            schedule_dispatch_lock: Mutex::new(()),
            notification_settings: NotificationDeliverySettings::default(),
            notification_sink: Arc::new(DisabledNotificationSink),
            cold_recovery_executions: Vec::new(),
            startup_environment: capture_current_environment(),
            scheduler: SchedulerWorker::default(),
            node_projection_workers: NodeProjectionWorkers::default(),
            clock: Mutex::new(Arc::new(SystemSchedulerClock)),
            #[cfg(test)]
            fail_after_mutation: AtomicBool::new(false),
        }
    }
}

struct Workspace {
    id: String,
    revision: Mutex<u64>,
    name: Mutex<String>,
    default_cwd: Option<PathBuf>,
    shell_ids: Mutex<Vec<String>>,
    launcher_ids: Mutex<Vec<String>>,
    agent_ids: Mutex<Vec<String>>,
    schedule_ids: Mutex<Vec<String>>,
}

struct AgentSchedule {
    id: String,
    workspace_id: String,
    cwd: PathBuf,
    integration: String,
    session: AgentScheduleSession,
    overlap_policy: AgentScheduleOverlapPolicy,
    created_at_ms: u64,
    state: Mutex<AgentScheduleMutableState>,
    executions: Mutex<Vec<Arc<ScheduledExecution>>>,
    dispatch_key_filter: Mutex<Vec<u8>>,
}

struct ScheduledExecution {
    id: String,
    workspace_id: String,
    schedule_id: String,
    dispatch_kind: ScheduledExecutionDispatchKind,
    dispatch_key: String,
    schedule_revision: u64,
    prompt_revision: u64,
    trigger_revision: u64,
    requested_at_ms: u64,
    scheduled_at_ms: Option<u64>,
    coalesced_through_ms: Option<u64>,
    cwd: PathBuf,
    integration: String,
    session: AgentScheduleSession,
    prompt: String,
    runner_token: String,
    state: Mutex<ScheduledExecutionMutableState>,
}

#[derive(Clone)]
struct ScheduledExecutionMutableState {
    revision: u64,
    state: ScheduledExecutionState,
    started_at_ms: Option<u64>,
    ended_at_ms: Option<u64>,
    reason: Option<ScheduledExecutionReason>,
    outcome: Option<ScheduledExecutionOutcome>,
    shell_id: Option<String>,
    run_id: Option<String>,
    agent_id: Option<String>,
    external_session_id: Option<String>,
}

struct ScheduleDecision {
    dispatch_kind: ScheduledExecutionDispatchKind,
    dispatch_key: String,
    scheduled_at_ms: Option<u64>,
    coalesced_through_ms: Option<u64>,
    requested_at_ms: u64,
    forced_skip: Option<ScheduledExecutionReason>,
}

#[derive(Clone)]
struct AgentScheduleMutableState {
    name: String,
    prompt: String,
    trigger: AgentScheduleTrigger,
    prompt_revision: u64,
    trigger_revision: u64,
    state: AgentScheduleState,
    revision: u64,
    updated_at_ms: u64,
    evaluation_frontier_ms: u64,
    evaluation_frontier_trigger_revision: u64,
    execution_shell_id: Option<String>,
}

struct AgentInstance {
    id: String,
    workspace_id: String,
    shell_id: String,
    run_id: String,
    name: String,
    integration: String,
    external_session_id: Option<String>,
    cwd: Option<PathBuf>,
    started_at_ms: u64,
    state: Mutex<AgentInstanceState>,
}

#[derive(Clone)]
struct AgentInstanceState {
    ended_at_ms: Option<u64>,
    observation: AgentObservationSnapshot,
    attention: Option<AgentAttentionSnapshot>,
}

struct WorkspaceLauncher {
    id: String,
    revision: Mutex<u64>,
    workspace_id: String,
    name: Mutex<String>,
    cwd: PathBuf,
    command: Vec<String>,
}

struct Shell {
    id: String,
    revision: Mutex<u64>,
    workspace_id: String,
    name: Mutex<String>,
    cwd: PathBuf,
    command: Vec<String>,
    owner: ShellOwner,
    last_run: Mutex<Option<PersistedShellRun>>,
    lifecycle: Mutex<ShellLifecycle>,
    foreground_process_cache: Mutex<Option<(String, Instant, Option<String>)>>,
}

enum ShellLifecycle {
    Pending,
    Running {
        profile: TerminalProfile,
        run: Arc<ShellRun>,
        runtime: Arc<ShellRuntime>,
    },
    Exited {
        code: Option<u32>,
        profile: TerminalProfile,
        run: Arc<ShellRun>,
        runtime: Option<Arc<ShellRuntime>>,
        terminal: Arc<Mutex<TerminalState>>,
    },
    Closed,
}

struct StopRollback {
    lifecycle: ShellLifecycle,
    event: Option<DaemonEventKind>,
}

struct StopRuntimeError {
    source: io::Error,
    stopped: bool,
}

impl From<io::Error> for StopRuntimeError {
    fn from(source: io::Error) -> Self {
        Self {
            source,
            stopped: false,
        }
    }
}

struct ShellRun {
    id: String,
    generation: u64,
    started_at_ms: u64,
    ended: Mutex<Option<ShellRunEnd>>,
    output_revision: AtomicU64,
    environment_has_run_id: bool,
}

#[derive(Clone)]
struct ShellRunEnd {
    ended_at_ms: u64,
    reason: ShellRunExitReason,
}

impl ShellRun {
    fn new(generation: u64) -> Self {
        Self {
            id: Uuid::new_v4().to_string(),
            generation,
            started_at_ms: unix_time_ms(),
            ended: Mutex::new(None),
            output_revision: AtomicU64::new(0),
            environment_has_run_id: true,
        }
    }

    fn from_persisted(run: &PersistedShellRun) -> Self {
        let ended = run
            .ended_at_ms
            .zip(run.exit_reason.clone())
            .map(|(ended_at_ms, reason)| ShellRunEnd {
                ended_at_ms,
                reason,
            });
        Self {
            id: run.id.clone(),
            generation: run.generation,
            started_at_ms: run.started_at_ms,
            ended: Mutex::new(ended),
            output_revision: AtomicU64::new(run.output_revision),
            environment_has_run_id: run.environment_has_run_id,
        }
    }

    fn finish(&self, reason: ShellRunExitReason) -> io::Result<()> {
        let mut ended = lock(&self.ended)?;
        if ended.is_none() {
            *ended = Some(ShellRunEnd {
                ended_at_ms: unix_time_ms().max(self.started_at_ms),
                reason,
            });
        }
        Ok(())
    }

    fn snapshot(&self) -> io::Result<ShellRunSnapshot> {
        let ended = lock(&self.ended)?.clone();
        Ok(ShellRunSnapshot {
            id: self.id.clone(),
            generation: self.generation,
            started_at_ms: self.started_at_ms,
            ended_at_ms: ended.as_ref().map(|end| end.ended_at_ms),
            exit_reason: ended.map(|end| end.reason),
            output_revision: self.output_revision.load(Ordering::Acquire),
            environment_has_run_id: self.environment_has_run_id,
        })
    }

    fn persisted(&self, profile: TerminalProfile) -> io::Result<PersistedShellRun> {
        let snapshot = self.snapshot()?;
        Ok(PersistedShellRun {
            id: snapshot.id,
            generation: snapshot.generation,
            started_at_ms: snapshot.started_at_ms,
            ended_at_ms: snapshot.ended_at_ms,
            exit_reason: snapshot.exit_reason,
            output_revision: snapshot.output_revision,
            environment_has_run_id: snapshot.environment_has_run_id,
            profile,
            terminal_history: None,
        })
    }
}

struct ShellRuntime {
    control: Mutex<()>,
    master: Mutex<PtyMaster>,
    process: Mutex<ManagedProcess>,
    terminal: Arc<Mutex<TerminalState>>,
    controller: Mutex<Option<Controller>>,
    reader: Mutex<Option<ReaderTask>>,
    output_changed: Condvar,
    output_wait: Mutex<()>,
}

enum ManagedProcess {
    Owned(Box<dyn Child + Send + Sync>),
    Imported(ImportedProcess),
}

struct ImportedProcess {
    pid: u32,
    pidfd: OwnedFd,
}

struct OutgoingRuntime {
    manifest: handoff::RuntimeManifest,
    pty: OwnedFd,
    pidfd: OwnedFd,
    reconstruction: Vec<u8>,
}

struct OutgoingExited {
    manifest: handoff::ExitedManifest,
    reconstruction: Vec<u8>,
}

struct PreparedHandoff {
    runtimes: Vec<OutgoingRuntime>,
    exited: Vec<OutgoingExited>,
    paused: Vec<Arc<ShellRuntime>>,
}

struct Controller {
    token: String,
    output: SyncSender<ControllerOutput>,
    connection: UnixStream,
    reconnect_ack: Option<SyncSender<()>>,
}

enum ControllerOutput {
    Data(Vec<u8>),
    Reconnect(SyncSender<bool>),
}

struct PtyMaster {
    descriptor: OwnedFd,
}

struct PtyReader {
    descriptor: OwnedFd,
}

struct ReaderTask {
    commands: mpsc::Sender<ReaderCommand>,
    handle: Mutex<Option<thread::JoinHandle<io::Result<()>>>>,
}

enum ReaderCommand {
    Pause {
        acknowledge: SyncSender<io::Result<()>>,
        cancelled: Arc<AtomicBool>,
    },
    Resume,
    Stop,
}

impl ManagedProcess {
    fn try_wait_code(&mut self) -> io::Result<Option<Option<u32>>> {
        match self {
            Self::Owned(child) => Ok(child.try_wait()?.map(|status| Some(status.exit_code()))),
            Self::Imported(process) => process.has_exited().map(|exited| exited.then_some(None)),
        }
    }

    fn process_id(&self) -> Option<u32> {
        match self {
            Self::Owned(child) => child.process_id(),
            Self::Imported(process) => Some(process.pid),
        }
    }

    fn kill(&mut self) -> io::Result<()> {
        match self {
            Self::Owned(child) => child.kill(),
            Self::Imported(process) => process.send_signal(libc::SIGKILL),
        }
    }

    fn wait(&mut self) -> io::Result<()> {
        match self {
            Self::Owned(child) => child.wait().map(|_| ()),
            Self::Imported(process) => process.wait(),
        }
    }

    fn transfer_identity(&mut self) -> io::Result<(u32, OwnedFd)> {
        if self.try_wait_code()?.is_some() {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "exited shell cannot be transferred as a live runtime",
            ));
        }
        match self {
            Self::Owned(child) => {
                let pid = child
                    .process_id()
                    .ok_or_else(|| io::Error::other("shell process has no PID"))?;
                Ok((pid, open_pidfd(pid)?))
            }
            Self::Imported(process) => Ok((process.pid, process.pidfd.try_clone()?)),
        }
    }
}

fn open_pidfd(pid: u32) -> io::Result<OwnedFd> {
    // pidfd_open creates a stable kernel reference to the live process.
    let descriptor = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) };
    if descriptor == -1 {
        return Err(io::Error::last_os_error());
    }
    // pidfd_open returned a new descriptor with ownership transferred here.
    Ok(unsafe { OwnedFd::from_raw_fd(descriptor as RawFd) })
}

impl ImportedProcess {
    fn has_exited(&self) -> io::Result<bool> {
        let mut descriptor = libc::pollfd {
            fd: self.pidfd.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        // The pollfd points to initialized writable memory for one descriptor.
        let result = unsafe { libc::poll(&mut descriptor, 1, 0) };
        if result == -1 {
            Err(io::Error::last_os_error())
        } else {
            Ok(result > 0 && descriptor.revents & libc::POLLIN != 0)
        }
    }

    fn send_signal(&self, signal: libc::c_int) -> io::Result<()> {
        send_pidfd_signal(self.pidfd.as_fd(), signal)
    }

    fn wait(&self) -> io::Result<()> {
        let deadline = Instant::now() + HANDSHAKE_TIMEOUT;
        while Instant::now() < deadline {
            if self.has_exited()? {
                return Ok(());
            }
            thread::sleep(IO_RETRY_DELAY);
        }
        Err(io::Error::new(
            io::ErrorKind::TimedOut,
            format!("process {} did not exit", self.pid),
        ))
    }
}

fn send_pidfd_signal(pidfd: BorrowedFd<'_>, signal: libc::c_int) -> io::Result<()> {
    // pidfd_send_signal uses the stable process reference held by pidfd.
    let result = unsafe {
        libc::syscall(
            libc::SYS_pidfd_send_signal,
            pidfd.as_raw_fd(),
            signal,
            std::ptr::null::<libc::siginfo_t>(),
            0,
        )
    };
    if result == -1 {
        let error = io::Error::last_os_error();
        if error.raw_os_error() == Some(libc::ESRCH) {
            Ok(())
        } else {
            Err(error)
        }
    } else {
        Ok(())
    }
}

impl PtyMaster {
    fn duplicate(master: &dyn MasterPty) -> io::Result<Self> {
        let descriptor = master
            .as_raw_fd()
            .ok_or_else(|| io::Error::other("PTY master does not expose a file descriptor"))?;
        // F_DUPFD_CLOEXEC creates an independently owned descriptor for the
        // same PTY open file description.
        let duplicated = unsafe { libc::fcntl(descriptor, libc::F_DUPFD_CLOEXEC, 0) };
        if duplicated == -1 {
            return Err(io::Error::last_os_error());
        }
        // fcntl returned a new descriptor whose ownership transfers here.
        let descriptor = unsafe { OwnedFd::from_raw_fd(duplicated) };
        Self::from_descriptor(descriptor)
    }

    fn from_descriptor(descriptor: OwnedFd) -> io::Result<Self> {
        let master = Self { descriptor };
        master.set_nonblocking()?;
        Ok(master)
    }

    fn try_clone_reader(&self) -> io::Result<PtyReader> {
        Ok(PtyReader {
            descriptor: self.descriptor.try_clone()?,
        })
    }

    fn resize(&self, size: PtySize) -> io::Result<()> {
        let size = libc::winsize {
            ws_row: size.rows,
            ws_col: size.cols,
            ws_xpixel: size.pixel_width,
            ws_ypixel: size.pixel_height,
        };
        // The descriptor is a live Unix PTY master and `size` is initialized.
        if unsafe { libc::ioctl(self.descriptor.as_raw_fd(), libc::TIOCSWINSZ, &size) } == -1 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    fn foreground_process(&self) -> Option<String> {
        let mut process_group: libc::pid_t = 0;
        // The descriptor is a live Unix PTY master and process_group is writable.
        if unsafe {
            libc::ioctl(
                self.descriptor.as_raw_fd(),
                libc::TIOCGPGRP,
                &mut process_group,
            )
        } == -1
            || process_group <= 0
        {
            return None;
        }
        read_process_name(process_group as u32)
    }

    fn write(&self, bytes: &[u8]) -> io::Result<usize> {
        // The byte slice is valid for the duration of this write.
        let result = unsafe {
            libc::write(
                self.descriptor.as_raw_fd(),
                bytes.as_ptr().cast(),
                bytes.len(),
            )
        };
        if result == -1 {
            Err(io::Error::last_os_error())
        } else {
            Ok(result as usize)
        }
    }

    fn set_nonblocking(&self) -> io::Result<()> {
        let descriptor = self.descriptor.as_raw_fd();
        // fcntl only reads and updates status flags for this live descriptor.
        let flags = unsafe { libc::fcntl(descriptor, libc::F_GETFL) };
        if flags == -1
            || unsafe { libc::fcntl(descriptor, libc::F_SETFL, flags | libc::O_NONBLOCK) } == -1
        {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
}

impl Read for PtyReader {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        // The mutable slice is valid for the duration of this read.
        let result = unsafe {
            libc::read(
                self.descriptor.as_raw_fd(),
                buffer.as_mut_ptr().cast(),
                buffer.len(),
            )
        };
        if result == -1 {
            let error = io::Error::last_os_error();
            if error.raw_os_error() == Some(libc::EIO) {
                Ok(0)
            } else {
                Err(error)
            }
        } else {
            Ok(result as usize)
        }
    }
}

impl ReaderTask {
    fn is_finished(&self) -> bool {
        self.handle
            .lock()
            .ok()
            .and_then(|handle| handle.as_ref().map(thread::JoinHandle::is_finished))
            .unwrap_or(true)
    }

    fn finish(&self) -> io::Result<()> {
        let handle = lock(&self.handle)?.take();
        match handle {
            Some(handle) => handle
                .join()
                .map_err(|_| io::Error::other("PTY reader thread panicked"))?,
            None => Ok(()),
        }
    }

    fn pause(&self) -> io::Result<()> {
        self.pause_with_timeout(HANDSHAKE_TIMEOUT)
    }

    fn pause_with_timeout(&self, timeout: Duration) -> io::Result<()> {
        if self.is_finished() {
            return self.finish();
        }
        let (acknowledge, acknowledged) = mpsc::sync_channel(1);
        let cancelled = Arc::new(AtomicBool::new(false));
        if self
            .commands
            .send(ReaderCommand::Pause {
                acknowledge,
                cancelled: Arc::clone(&cancelled),
            })
            .is_err()
        {
            return if self.is_finished() {
                self.finish()
            } else {
                Err(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "PTY reader control closed",
                ))
            };
        }
        let deadline = Instant::now() + timeout;
        loop {
            match acknowledged.try_recv() {
                Ok(result) => return result,
                Err(mpsc::TryRecvError::Disconnected) if self.is_finished() => {
                    return self.finish();
                }
                Err(mpsc::TryRecvError::Disconnected) => {
                    return Err(io::Error::new(
                        io::ErrorKind::BrokenPipe,
                        "PTY reader stopped before acknowledging pause",
                    ));
                }
                Err(mpsc::TryRecvError::Empty) if self.is_finished() => return self.finish(),
                Err(mpsc::TryRecvError::Empty) if Instant::now() >= deadline => {
                    cancelled.store(true, Ordering::Release);
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "PTY reader did not pause",
                    ));
                }
                Err(mpsc::TryRecvError::Empty) => thread::sleep(IO_RETRY_DELAY),
            }
        }
    }

    fn resume(&self) -> io::Result<()> {
        if self.is_finished() {
            self.finish()
        } else if self.commands.send(ReaderCommand::Resume).is_ok() {
            Ok(())
        } else {
            Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "PTY reader control closed",
            ))
        }
    }

    fn stop(self) -> io::Result<()> {
        if !self.is_finished() {
            let _ = self.commands.send(ReaderCommand::Stop);
        }
        self.finish()
    }
}

fn workspace_created_events(workspace: &WorkspaceSnapshot) -> Vec<DaemonEventKind> {
    let mut events = vec![DaemonEventKind::WorkspaceCreated {
        workspace_id: workspace.id.clone(),
        name: workspace.name.clone(),
    }];
    events.extend(
        workspace
            .shells
            .iter()
            .map(|shell| DaemonEventKind::ShellCreated {
                workspace_id: workspace.id.clone(),
                shell_id: shell.id.clone(),
                name: shell.name.clone(),
            }),
    );
    events
}

fn scheduler_worker(service: Weak<DaemonService>) {
    let mut unacknowledged_tick = None;
    let mut consecutive_failures = 0_u32;
    let mut failure_logged = false;
    if let Some(service) = service.upgrade()
        && let Ok(mut state) = service.scheduler.state.lock()
    {
        state.running = true;
    }
    loop {
        let Some(service) = service.upgrade() else {
            return;
        };
        let clock = match lock(&service.clock) {
            Ok(clock) => Arc::clone(&clock),
            Err(_) => return,
        };
        let tick = clock.take_tick().map_err(DaemonError::from);
        if let Ok(Some(generation)) = &tick {
            unacknowledged_tick = Some(*generation);
        }
        let attempt = if unacknowledged_tick.is_some() {
            clock.record_attempt().map_err(DaemonError::from)
        } else {
            Ok(())
        };
        let attempt = tick
            .map(|_| ())
            .and(attempt)
            .and_then(|()| service.evaluate_schedules(false))
            .and_then(|()| {
                service
                    .next_scheduled_occurrence_ms()
                    .map_err(DaemonError::from)
            })
            .and_then(|next| {
                if let Some(generation) = unacknowledged_tick {
                    clock.acknowledge(generation).map_err(DaemonError::from)?;
                }
                Ok(next)
            });
        let (wait, retry_deadline) = match &attempt {
            Ok(next) => {
                consecutive_failures = 0;
                failure_logged = false;
                unacknowledged_tick = None;
                let wait = next
                    .map(|next| Duration::from_millis(next.saturating_sub(clock.now_ms())))
                    .map_or_else(
                        || clock.resample_interval(),
                        |until_next| until_next.min(clock.resample_interval()),
                    );
                (wait, None)
            }
            Err(error) => {
                consecutive_failures = consecutive_failures.saturating_add(1);
                if !failure_logged {
                    let _ = clock.record_failure_diagnostic();
                    eprintln!("boomux: scheduler attempt failed: {error}");
                    failure_logged = true;
                }
                let wait = scheduler_retry_delay(consecutive_failures);
                (wait, Some(Instant::now() + wait))
            }
        };
        let mut state = match lock(&service.scheduler.state) {
            Ok(state) => state,
            Err(_) => return,
        };
        state.healthy = attempt.is_ok();
        if state.stop {
            state.running = false;
            state.healthy = false;
            return;
        }
        if state.wake && retry_deadline.is_none() {
            state.wake = false;
            continue;
        }
        state.wake = false;
        let mut remaining = wait;
        loop {
            let Ok((next, _)) = service.scheduler.changed.wait_timeout(state, remaining) else {
                return;
            };
            state = next;
            if state.stop {
                state.running = false;
                state.healthy = false;
                return;
            }
            state.wake = false;
            let Some(deadline) = retry_deadline else {
                break;
            };
            let now = Instant::now();
            if now >= deadline {
                break;
            }
            remaining = deadline.saturating_duration_since(now);
        }
    }
}

fn scheduler_retry_delay(consecutive_failures: u32) -> Duration {
    let shift = consecutive_failures.saturating_sub(1).min(7);
    SCHEDULER_RETRY_MIN
        .checked_mul(1_u32 << shift)
        .unwrap_or(SCHEDULER_RETRY_MAX)
        .min(SCHEDULER_RETRY_MAX)
}

fn node_projection_worker(service: Weak<DaemonService>, node_id: String) {
    let mut failures = 0_u32;
    loop {
        let Some(service) = service.upgrade() else {
            return;
        };
        if service.node_projection_workers.stop.load(Ordering::Acquire) {
            return;
        }
        let registration = match service.node_registrations().and_then(|registrations| {
            registrations
                .inspect(&node_id)
                .map_err(node_registration_error)
        }) {
            Ok(registration) => registration,
            Err(_) => return,
        };
        let (after, expected_generation) = match service.node_projection_cache().and_then(|cache| {
            cache
                .cursor_and_generation(&registration)
                .map_err(DaemonError::from)
        }) {
            Ok(value) => value,
            Err(error) => {
                eprintln!("boomux: could not inspect Node projection cache: {error}");
                return;
            }
        };
        let admitted = service
            .node_registrations()
            .and_then(|registrations| {
                registrations
                    .admit(&registration)
                    .map_err(node_registration_error)
            })
            .unwrap_or(false);
        if !admitted {
            if interruptible_node_sleep(&service, Duration::from_millis(100)) {
                return;
            }
            continue;
        }
        let attempt_at_ms = unix_time_ms();
        let result = fetch_node_projection(&registration, after);
        let mut published_generation = None;
        match result {
            Ok((sync, capabilities)) => {
                let commit = service.node_registrations().and_then(|registrations| {
                    registrations
                        .with_current(&registration, || {
                            service
                                .node_projection_cache
                                .as_ref()
                                .ok_or_else(|| {
                                    io::Error::other("Node projection cache unavailable")
                                })?
                                .commit_projection(
                                    &registration,
                                    expected_generation,
                                    sync.cursor,
                                    sync.projection,
                                    capabilities,
                                    attempt_at_ms,
                                )
                        })
                        .map_err(node_registration_error)
                });
                if let Ok(Some(Some(generation))) = commit {
                    published_generation = Some(generation);
                    failures = 0;
                } else if let Err(error) = commit {
                    eprintln!("boomux: could not commit Node projection: {error}");
                }
            }
            Err((health, error)) => {
                failures = failures.saturating_add(1);
                let delay = node_projection_retry_delay(&node_id, failures, health);
                let retry_at_ms = attempt_at_ms
                    .saturating_add(u64::try_from(delay.as_millis()).unwrap_or(u64::MAX));
                let commit = service.node_registrations().and_then(|registrations| {
                    registrations
                        .with_current(&registration, || {
                            service
                                .node_projection_cache
                                .as_ref()
                                .ok_or_else(|| {
                                    io::Error::other("Node projection cache unavailable")
                                })?
                                .mark_health(
                                    &registration,
                                    expected_generation,
                                    health,
                                    attempt_at_ms,
                                    Some(retry_at_ms),
                                )
                        })
                        .map_err(node_registration_error)
                });
                if let Ok(Some(Some(generation))) = commit {
                    published_generation = Some(generation);
                } else if let Err(error) = commit {
                    eprintln!("boomux: could not commit Node projection health: {error}");
                }
                if failures == 1 {
                    eprintln!(
                        "boomux: Node projection sync for {} failed: {error}",
                        registration.alias
                    );
                }
            }
        }
        service
            .node_registrations()
            .map(|registrations| registrations.release(&registration))
            .ok();
        if let Some(cache_generation) = published_generation {
            let _ = service.events.publish_runtime_batch(vec![
                DaemonEventKind::NodeProjectionChanged {
                    node_id: registration.node_id.clone(),
                    cache_generation,
                },
            ]);
        }
        let delay = if failures == 0 {
            Duration::from_secs(1)
        } else {
            node_projection_retry_delay(
                &node_id,
                failures,
                service
                    .node_projection_cache()
                    .and_then(|cache| cache.health(&registration).map_err(DaemonError::from))
                    .map(|health| health.code)
                    .unwrap_or(crate::protocol::NodeProjectionHealthCode::Unreachable),
            )
        };
        if interruptible_node_sleep(&service, delay) {
            return;
        }
    }
}

fn fetch_node_projection(
    registration: &crate::protocol::NodeRegistrationSnapshot,
    after: Option<EventCursor>,
) -> Result<(NodeProjectionSync, Vec<String>), (crate::protocol::NodeProjectionHealthCode, io::Error)>
{
    use crate::protocol::NodeProjectionHealthCode;
    let target = SshTarget::parse(registration.target.clone())
        .map_err(|error| (NodeProjectionHealthCode::Unreachable, error))?;
    let helper = match ssh_bootstrap::plan_remote_bootstrap(
        target.clone(),
        SshAuthenticationMode::Batch,
        Duration::from_secs(2),
    ) {
        Ok(RemoteBootstrapPlan::Ready(helper)) => helper,
        Ok(RemoteBootstrapPlan::Install(_)) => {
            return Err((
                NodeProjectionHealthCode::Unsupported,
                io::Error::new(
                    io::ErrorKind::Unsupported,
                    "remote helper requires installation",
                ),
            ));
        }
        Err(error) => return Err((classify_node_sync_error(&error), error)),
    };
    if helper.handshake.node_id != registration.node_id {
        return Err((
            NodeProjectionHealthCode::IdentityChanged,
            io::Error::new(
                io::ErrorKind::PermissionDenied,
                "remote Node identity changed",
            ),
        ));
    }
    let version = helper.handshake.core_protocol_version;
    if !protocol::ProtocolFeature::NodeProjectionSync.is_supported_by(version) {
        return Err((
            NodeProjectionHealthCode::Unsupported,
            io::Error::new(
                io::ErrorKind::Unsupported,
                "remote Node does not support projection synchronization",
            ),
        ));
    }
    let mut remote = ssh_bootstrap::connect_remote(
        target,
        helper,
        SshAuthenticationMode::Batch,
        Duration::from_secs(2),
    )
    .map_err(|error| (classify_node_sync_error(&error), error))?;
    let sync = remote
        .node_projection_sync(after, Duration::from_secs(2))
        .map_err(|error| (classify_node_sync_error(&error), error))?;
    if sync.projection.node_id != registration.node_id {
        return Err((
            NodeProjectionHealthCode::IdentityChanged,
            io::Error::new(
                io::ErrorKind::PermissionDenied,
                "projection owner identity changed",
            ),
        ));
    }
    let capabilities = protocol::ProtocolFeature::ALL
        .iter()
        .copied()
        .filter(|feature| feature.is_supported_by(version))
        .flat_map(protocol::ProtocolFeature::capability_names)
        .copied()
        .map(str::to_owned)
        .collect();
    Ok((sync, capabilities))
}

fn classify_node_sync_error(error: &io::Error) -> crate::protocol::NodeProjectionHealthCode {
    use crate::protocol::NodeProjectionHealthCode;
    if error.kind() == io::ErrorKind::Unsupported {
        NodeProjectionHealthCode::Unsupported
    } else if error.kind() == io::ErrorKind::PermissionDenied
        || error
            .to_string()
            .to_ascii_lowercase()
            .contains("authentication")
    {
        NodeProjectionHealthCode::AuthenticationRequired
    } else {
        NodeProjectionHealthCode::Unreachable
    }
}

fn node_projection_retry_delay(
    node_id: &str,
    failures: u32,
    health: crate::protocol::NodeProjectionHealthCode,
) -> Duration {
    use crate::protocol::NodeProjectionHealthCode;
    let base = if matches!(
        health,
        NodeProjectionHealthCode::AuthenticationRequired
            | NodeProjectionHealthCode::IdentityChanged
            | NodeProjectionHealthCode::IdentityConflict
            | NodeProjectionHealthCode::Unsupported
    ) {
        60_u64
    } else {
        1_u64
            .checked_shl(failures.saturating_sub(1).min(6))
            .unwrap_or(60)
            .min(60)
    };
    let jitter = node_id.bytes().fold(u64::from(failures), |value, byte| {
        value.wrapping_mul(33).wrapping_add(u64::from(byte))
    }) % 1_000;
    Duration::from_millis(base.saturating_mul(1_000).saturating_add(jitter))
}

fn interruptible_node_sleep(service: &DaemonService, duration: Duration) -> bool {
    let deadline = Instant::now() + duration;
    while Instant::now() < deadline {
        if service.node_projection_workers.stop.load(Ordering::Acquire) {
            return true;
        }
        thread::sleep(
            deadline
                .saturating_duration_since(Instant::now())
                .min(Duration::from_millis(100)),
        );
    }
    false
}

fn resume_identity(
    agent: &AgentInstance,
    shell: &Shell,
    previous_run: &PersistedShellRun,
) -> io::Result<Option<(String, String)>> {
    if agent.workspace_id != shell.workspace_id
        || agent.shell_id != shell.id
        || agent.run_id != previous_run.id
        || agent.cwd.as_ref() != Some(&shell.cwd)
        || !crate::integrations::by_key(&agent.integration)
            .is_some_and(|descriptor| descriptor.resume.is_some())
    {
        return Ok(None);
    }
    let Some(external_session_id) = agent
        .external_session_id
        .as_ref()
        .filter(|id| !id.is_empty())
    else {
        return Ok(None);
    };
    let state = lock(&agent.state)?;
    if state.ended_at_ms.is_some()
        || state.observation.authority != AgentAuthority::LifecycleIntegration
        || state.observation.state == AgentState::Done
    {
        return Ok(None);
    }
    Ok(Some((
        agent.integration.clone(),
        external_session_id.clone(),
    )))
}

impl DaemonService {
    fn node_identity(&self) -> DaemonResult<&Arc<NodeIdentityManager>> {
        self.node_identity.as_ref().ok_or_else(|| {
            DaemonError::lifecycle(
                ErrorCode::NodeIdentityUnavailable,
                "Boomux Node identity is unavailable",
            )
        })
    }

    fn node_registrations(&self) -> DaemonResult<&NodeRegistrationManager> {
        self.node_registrations.as_ref().ok_or_else(|| {
            DaemonError::lifecycle(
                ErrorCode::NodeRegistrationUnavailable,
                "Boomux Node registrations are unavailable",
            )
        })
    }

    fn node_projection_cache(&self) -> DaemonResult<&NodeProjectionCache> {
        self.node_projection_cache.as_ref().ok_or_else(|| {
            DaemonError::lifecycle(
                ErrorCode::NodeRegistrationUnavailable,
                "Boomux Node projection cache is unavailable",
            )
        })
    }

    #[allow(clippy::too_many_arguments)]
    fn handle_remote_attach(
        &self,
        mut stream: UnixStream,
        response_version: u32,
        identity: protocol::QualifiedIdentity,
        takeover: bool,
        restart_exited: bool,
        expected_run_id: Option<String>,
        profile: TerminalProfile,
    ) -> io::Result<()> {
        if identity.node_id.is_empty() || identity.inner_id.is_empty() {
            return send_response(
                &mut stream,
                response_version,
                DaemonError::validation("remote attachment requires an exact qualified identity")
                    .into_response(),
            );
        }
        if self
            .node_identity()
            .and_then(|node| node.id().map_err(DaemonError::from))
            .is_ok_and(|node_id| node_id == identity.node_id)
        {
            return send_response(
                &mut stream,
                response_version,
                DaemonError::validation("local shells must use the local attachment request")
                    .into_response(),
            );
        }
        if let Err(error) = validate_terminal_profile(&profile) {
            return send_daemon_error(&mut stream, response_version, error.into());
        }
        let registrations = match self.node_registrations() {
            Ok(registrations) => registrations,
            Err(error) => {
                return send_response(&mut stream, response_version, error.into_response());
            }
        };
        let registration = match registrations.inspect(&identity.node_id) {
            Ok(registration) if registration.node_id == identity.node_id => registration,
            Ok(_) => {
                return send_response(
                    &mut stream,
                    response_version,
                    error_response(ErrorCode::NotFound, "exact Node registration not found"),
                );
            }
            Err(error) => {
                return send_response(
                    &mut stream,
                    response_version,
                    node_registration_error(error).into_response(),
                );
            }
        };
        if !registrations.admit(&registration).unwrap_or(false) {
            return send_response(
                &mut stream,
                response_version,
                error_response(
                    ErrorCode::RevisionChanged,
                    "Node registration changed before attachment",
                ),
            );
        }
        let result = self.bridge_remote_attach(
            &mut stream,
            response_version,
            &registration,
            &identity.inner_id,
            takeover,
            restart_exited,
            expected_run_id,
            profile,
        );
        registrations.release(&registration);
        result
    }

    #[allow(clippy::too_many_arguments)]
    fn bridge_remote_attach(
        &self,
        stream: &mut UnixStream,
        response_version: u32,
        registration: &crate::protocol::NodeRegistrationSnapshot,
        shell_id: &str,
        takeover: bool,
        restart_exited: bool,
        expected_run_id: Option<String>,
        profile: TerminalProfile,
    ) -> io::Result<()> {
        let target = SshTarget::parse(registration.target.clone())?;
        let helper = match ssh_bootstrap::plan_remote_bootstrap(
            target.clone(),
            SshAuthenticationMode::Batch,
            HANDSHAKE_TIMEOUT,
        )? {
            RemoteBootstrapPlan::Ready(helper) => helper,
            RemoteBootstrapPlan::Install(_) => {
                return send_response(
                    stream,
                    response_version,
                    error_response(
                        ErrorCode::UnsupportedVersion,
                        "remote helper requires installation",
                    ),
                );
            }
        };
        if helper.handshake.node_id != registration.node_id {
            return send_response(
                stream,
                response_version,
                error_response(
                    ErrorCode::NodeIdentityChanged,
                    "remote Node identity changed",
                ),
            );
        }
        let remote_version = helper.handshake.core_protocol_version;
        let owner_environment =
            protocol::ProtocolFeature::RemotePtyAttachment.is_supported_by(remote_version);
        if !owner_environment {
            let shell = match send_registered_node_request(
                registration,
                Request::GetShell {
                    shell_id: shell_id.to_owned(),
                },
            ) {
                Ok(Response::Shell { shell }) => shell,
                Ok(Response::Error { message, code }) => {
                    return send_response(
                        stream,
                        response_version,
                        Response::Error { message, code },
                    );
                }
                Ok(_) => {
                    return send_response(
                        stream,
                        response_version,
                        error_response(ErrorCode::Internal, "remote shell preflight was invalid"),
                    );
                }
                Err(error) => {
                    return send_response(
                        stream,
                        response_version,
                        error_response(ErrorCode::Timeout, error.to_string()),
                    );
                }
            };
            if shell.status != ShellStatus::Running {
                return send_response(
                    stream,
                    response_version,
                    error_response(
                        ErrorCode::UnsupportedVersion,
                        "remote pending or exited shell requires owner-environment attachment support",
                    ),
                );
            }
        }
        let remote = ssh_bootstrap::connect_remote(
            target,
            helper,
            SshAuthenticationMode::Batch,
            HANDSHAKE_TIMEOUT,
        )?;
        let request = Request::Attach {
            shell_id: shell_id.to_owned(),
            takeover,
            restart_exited: restart_exited && owner_environment,
            expected_run_id,
            profile,
            environment: None,
            owner_environment,
        };
        let (response, mut remote_reader, mut remote_writer) =
            remote.open_attachment(request, HANDSHAKE_TIMEOUT)?;
        let token = match &response {
            Response::Attached { token, .. } => token.clone(),
            _ => return send_response(stream, response_version, response),
        };
        send_response(stream, response_version, response)?;
        let connection = stream.try_clone()?;
        connection.set_write_timeout(Some(RESPONSE_WRITE_TIMEOUT))?;
        self.remote_attachments.insert(token.clone(), connection)?;
        let output_connection = self
            .remote_attachments
            .connection(&token)?
            .ok_or_else(|| io::Error::other("remote attachment registration disappeared"))?;
        let (reconnect_finished, reconnect_wait) = mpsc::sync_channel(1);
        let output = thread::Builder::new()
            .name(format!("boomux-remote-attachment-{shell_id}"))
            .spawn(move || -> io::Result<()> {
                let mut reconnect = false;
                while let Ok(frame) = remote_reader.read_frame() {
                    frame.write_to(&mut *lock(&output_connection)?)?;
                    if matches!(frame, AttachFrame::Reconnect) {
                        reconnect = true;
                        let _ = reconnect_wait.recv_timeout(HANDSHAKE_TIMEOUT);
                        break;
                    }
                    if matches!(frame, AttachFrame::Detached) {
                        break;
                    }
                }
                if !reconnect {
                    let _ = lock(&output_connection)?.shutdown(std::net::Shutdown::Both);
                }
                Ok(())
            })?;
        let input_result = loop {
            let frame = match AttachFrame::read_from(stream) {
                Ok(frame) => frame,
                Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => break Ok(()),
                Err(error) => break Err(error),
            };
            if matches!(frame, AttachFrame::ReconnectAck)
                && self.remote_attachments.acknowledge(&token)?
            {
                let _ = reconnect_finished.try_send(());
                break Ok(());
            }
            let closes = matches!(frame, AttachFrame::Detached | AttachFrame::ReconnectAck);
            remote_writer.write_frame(&frame, RESPONSE_WRITE_TIMEOUT)?;
            if closes {
                if matches!(frame, AttachFrame::ReconnectAck) {
                    let _ = reconnect_finished.try_send(());
                }
                break Ok(());
            }
        };
        drop(remote_writer);
        let _ = stream.shutdown(std::net::Shutdown::Both);
        let output_result = output
            .join()
            .map_err(|_| io::Error::other("remote attachment output thread panicked"))?;
        self.remote_attachments.remove(&token);
        input_result?;
        output_result
    }

    fn route_node_operation(&self, node_id: &str, operation: RoutedOperation) -> Response {
        let registrations = match self.node_registrations() {
            Ok(registrations) => registrations,
            Err(error) => return error.into_response(),
        };
        let registration = match registrations.inspect(node_id) {
            Ok(registration) if registration.node_id == node_id => registration,
            Ok(_) => {
                return error_response(ErrorCode::NotFound, "exact Node registration not found");
            }
            Err(error) => return node_registration_error(error).into_response(),
        };
        match registrations.admit(&registration) {
            Ok(true) => {}
            Ok(false) => {
                return error_response(
                    ErrorCode::RevisionChanged,
                    "Node registration changed before routing",
                );
            }
            Err(error) => return node_registration_error(error).into_response(),
        }
        let mut result = send_registered_node_request(&registration, operation.owner_request());
        if result.is_err() && operation.is_retryable() {
            result = send_registered_node_request(&registration, operation.owner_request());
        }
        let proven = if result.is_err() && !operation.is_retryable() {
            operation.ambiguity_probe().and_then(|probe| {
                send_registered_node_request(&registration, probe)
                    .ok()
                    .and_then(|response| proven_routed_result(&operation, response))
            })
        } else {
            None
        };
        registrations.release(&registration);
        let response = match result {
            Ok(response) => response,
            Err(_) if proven.is_some() => Response::Ok,
            Err(error) if error.kind() == io::ErrorKind::PermissionDenied => {
                return error_response(ErrorCode::NodeIdentityChanged, error.to_string());
            }
            Err(error) if error.kind() == io::ErrorKind::Unsupported => {
                return error_response(ErrorCode::UnsupportedVersion, error.to_string());
            }
            Err(error) => {
                let code = if operation_is_read(&operation) {
                    ErrorCode::Timeout
                } else {
                    ErrorCode::OutcomeUnknown
                };
                return error_response(
                    code,
                    format!("routed operation lost its verified channel: {error}"),
                );
            }
        };
        let current = registrations
            .with_current(&registration, || Ok(()))
            .unwrap_or(None)
            .is_some();
        if !current {
            return error_response(
                if operation_is_read(&operation) {
                    ErrorCode::RevisionChanged
                } else {
                    ErrorCode::OutcomeUnknown
                },
                "Node registration changed while the routed operation was in flight",
            );
        }
        if let Some(result) = proven {
            return Response::RoutedNodeOperation { result };
        }
        match routed_result(response) {
            Ok(result) => Response::RoutedNodeOperation { result },
            Err(response) => *response,
        }
    }

    fn combined_node_snapshot(
        &self,
        selector: Option<&str>,
    ) -> DaemonResult<crate::protocol::CombinedNodeSnapshot> {
        use crate::protocol::{
            CombinedNode, CombinedNodeSnapshot, NodeProjectionHealthCode, SchedulerHealth,
            SchedulerState,
        };

        let local_node_id = self.node_identity()?.id()?;
        let registrations = self
            .node_registrations()?
            .list()
            .map_err(node_registration_error)?;
        let local_selected = selector.is_none()
            || selector.is_some_and(|value| value == "local" || value == local_node_id);
        let remote_selected = registrations
            .iter()
            .filter(|registration| {
                selector.is_none()
                    || selector.is_some_and(|value| {
                        value == registration.alias || value == registration.node_id
                    })
            })
            .collect::<Vec<_>>();
        if selector == Some("local") && !remote_selected.is_empty() {
            return Err(DaemonError::lifecycle(
                ErrorCode::AmbiguousTarget,
                "combined Node selector 'local' matches the local Node and a registered alias; use an exact Node ID",
            ));
        }
        if !local_selected && remote_selected.is_empty() {
            return Err(DaemonError::lifecycle(
                ErrorCode::NotFound,
                "Node selector not found",
            ));
        }

        let mut nodes = Vec::with_capacity(usize::from(local_selected) + remote_selected.len());
        if local_selected {
            let snapshot = self.snapshot()?;
            let scheduler = snapshot.scheduler.clone().unwrap_or(SchedulerHealth {
                state: SchedulerState::Offline,
                max_concurrent: 0,
                active_executions: 0,
            });
            nodes.push(CombinedNode {
                node_id: local_node_id,
                alias: "local".into(),
                local: true,
                health: NodeProjectionHealthCode::Online,
                current: true,
                stale: false,
                observed_at_ms: unix_time_ms(),
                observed_protocol_version: Some(protocol::PROTOCOL_VERSION),
                observed_capabilities: protocol::protocol_capabilities()
                    .map(str::to_owned)
                    .collect(),
                scheduler,
                local_snapshot: Some(snapshot),
                remote_projection: None,
            });
        }
        for registration in remote_selected {
            let view = self.node_projection_cache()?.view(registration)?;
            let scheduler = view
                .projection
                .as_ref()
                .map(|projection| projection.scheduler.clone())
                .unwrap_or(SchedulerHealth {
                    state: SchedulerState::Offline,
                    max_concurrent: 0,
                    active_executions: 0,
                });
            let observed_protocol_version = view
                .health
                .capabilities
                .iter()
                .filter_map(|capability| capability.strip_prefix("protocol_")?.parse().ok())
                .max();
            nodes.push(CombinedNode {
                node_id: registration.node_id.clone(),
                alias: registration.alias.clone(),
                local: false,
                health: view.health.code,
                current: !view.health.stale,
                stale: view.health.stale,
                observed_at_ms: view.health.last_success_at_ms.unwrap_or(0),
                observed_protocol_version,
                observed_capabilities: view.health.capabilities,
                scheduler,
                local_snapshot: None,
                remote_projection: view.projection,
            });
        }
        nodes.sort_by(|left, right| {
            right
                .local
                .cmp(&left.local)
                .then_with(|| left.alias.cmp(&right.alias))
                .then_with(|| left.node_id.cmp(&right.node_id))
        });
        Ok(CombinedNodeSnapshot { nodes })
    }

    fn clock_now_ms(&self) -> u64 {
        lock(&self.clock)
            .map(|clock| clock.now_ms())
            .unwrap_or_else(|_| unix_time_ms())
    }

    fn configure_scheduler_clock(&mut self) -> io::Result<()> {
        #[cfg(debug_assertions)]
        if self.native_test_hooks_enabled()
            && let Some(variable) = self
                .startup_environment
                .variables
                .iter()
                .find(|variable| variable.name == b"BOOMUX_NATIVE_TEST_CLOCK")
        {
            let directory = PathBuf::from(std::ffi::OsString::from_vec(variable.value.clone()));
            let runtime = client::socket_path()?
                .parent()
                .ok_or_else(|| io::Error::other("daemon socket has no runtime directory"))?
                .canonicalize()?;
            let metadata = fs::symlink_metadata(&directory)?;
            let canonical = directory.canonicalize()?;
            if canonical != directory
                || canonical == runtime
                || !canonical.starts_with(&runtime)
                || !metadata.is_dir()
                || metadata.uid() != unsafe { libc::geteuid() }
                || metadata.mode() & 0o077 != 0
            {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "native clock directory must be canonical, owned, private, and beneath the daemon runtime directory",
                ));
            }
            for marker in ["tick", "seen", "ack", "attempts", "diagnostics"] {
                let path = canonical.join(marker);
                match fs::symlink_metadata(&path) {
                    Ok(metadata) => validate_native_clock_marker(&path, &metadata)?,
                    Err(error) if error.kind() == io::ErrorKind::NotFound && marker != "tick" => {}
                    Err(error) => return Err(error),
                }
            }
            self.clock = Mutex::new(Arc::new(NativeTestSchedulerClock::new(canonical)?));
        }
        Ok(())
    }

    fn start_scheduler(self: &Arc<Self>) -> io::Result<()> {
        let mut state = lock(&self.scheduler.state)?;
        if state.handle.is_some() {
            return Ok(());
        }
        state.stop = false;
        state.wake = true;
        state.running = false;
        state.healthy = false;
        let service = Arc::downgrade(self);
        state.handle = Some(
            thread::Builder::new()
                .name("boomux-scheduler".into())
                .spawn(move || scheduler_worker(service))?,
        );
        self.scheduler.changed.notify_all();
        Ok(())
    }

    fn start_node_projection_workers(self: &Arc<Self>) -> io::Result<()> {
        self.node_projection_workers
            .stop
            .store(false, Ordering::Release);
        let registrations = self
            .node_registrations
            .as_ref()
            .ok_or_else(|| io::Error::other("Node registrations unavailable"))?;
        let registrations = match registrations.list() {
            Ok(registrations) => registrations,
            Err(error) => {
                eprintln!("boomux: Node projection synchronization disabled: {error}");
                return Ok(());
            }
        };
        for registration in registrations {
            self.start_node_projection_worker(registration.node_id)?;
        }
        Ok(())
    }

    fn start_node_projection_worker(self: &Arc<Self>, node_id: String) -> io::Result<()> {
        let mut handles = lock(&self.node_projection_workers.handles)?;
        if handles
            .get(&node_id)
            .is_some_and(|handle| !handle.is_finished())
        {
            return Ok(());
        }
        if let Some(handle) = handles.remove(&node_id) {
            let _ = handle.join();
        }
        let service = Arc::downgrade(self);
        let worker_node_id = node_id.clone();
        let handle = thread::Builder::new()
            .name(format!("boomux-node-sync-{node_id}"))
            .spawn(move || node_projection_worker(service, worker_node_id))?;
        handles.insert(node_id, handle);
        Ok(())
    }

    fn stop_node_projection_workers(&self) -> io::Result<()> {
        self.node_projection_workers
            .stop
            .store(true, Ordering::Release);
        let handles = {
            let mut handles = lock(&self.node_projection_workers.handles)?;
            handles
                .drain()
                .map(|(_, handle)| handle)
                .collect::<Vec<_>>()
        };
        for handle in handles {
            handle
                .join()
                .map_err(|_| io::Error::other("Node projection worker panicked"))?;
        }
        Ok(())
    }

    fn stop_scheduler(&self) -> io::Result<()> {
        let handle = {
            let mut state = lock(&self.scheduler.state)?;
            state.stop = true;
            state.wake = true;
            self.scheduler.changed.notify_all();
            state.handle.take()
        };
        if let Some(handle) = handle {
            handle
                .join()
                .map_err(|_| io::Error::other("scheduler worker panicked"))?;
        }
        let mut state = lock(&self.scheduler.state)?;
        state.running = false;
        state.healthy = false;
        Ok(())
    }

    fn wake_scheduler(&self) {
        if let Ok(mut state) = self.scheduler.state.lock() {
            state.wake = true;
            self.scheduler.changed.notify_all();
        }
    }

    fn evaluate_schedules(self: &Arc<Self>, cold_recovery: bool) -> DaemonResult<()> {
        let schedules = lock(&self.durable.state)?
            .schedules
            .values()
            .cloned()
            .collect::<Vec<_>>();
        let now = self.clock_now_ms();
        for schedule in schedules {
            let state = lock(&schedule.state)?.clone();
            if state.state != AgentScheduleState::Enabled {
                continue;
            }
            let sampled_trigger_revision = state.trigger_revision;
            let cron = crate::scheduling::CronSchedule::compile(
                &state.trigger.cron,
                &state.trigger.timezone,
            )
            .map_err(|error| DaemonError::Internal(io::Error::other(error.to_string())))?;
            let first_due = cron
                .next_after_ms(state.evaluation_frontier_ms)
                .map_err(|error| DaemonError::Internal(io::Error::other(error.to_string())))?;
            if first_due > now {
                continue;
            }
            let latest_due = cron.latest_at_or_before_ms(now).map_err(|error| {
                DaemonError::Internal(io::Error::other(format!(
                    "could not evaluate scheduled occurrence: {error}"
                )))
            })?;
            if latest_due < first_due {
                continue;
            }
            self.wait_for_native_pre_dispatch_barrier();
            let _dispatch_eligibility = lock(&self.schedule_dispatch_lock)?;
            let schedule_id = schedule.id.clone();
            let mut dispatch = Vec::new();
            let response = self.durable_mutation_outcome(|undo| {
                let schedule = self.durable.schedule(&schedule_id)?;
                let current = lock(&schedule.state)?.clone();
                if current.trigger_revision != sampled_trigger_revision
                    || current.evaluation_frontier_trigger_revision != sampled_trigger_revision
                    || current.evaluation_frontier_ms >= latest_due
                {
                    return Ok(DurableMutation::Unchanged(Vec::new()));
                }
                let paused_race = current.state != AgentScheduleState::Enabled;
                let mut decisions = Vec::new();
                if cold_recovery || paused_race {
                    decisions.push((
                        first_due,
                        Some(latest_due).filter(|through| *through > first_due),
                        if paused_race {
                            ScheduledExecutionReason::PausedRace
                        } else {
                            ScheduledExecutionReason::Missed
                        },
                    ));
                } else {
                    if first_due < latest_due {
                        let coalesced_through = latest_due
                            .checked_sub(1)
                            .and_then(|ceiling| cron.latest_at_or_before_ms(ceiling).ok())
                            .filter(|through| *through >= first_due);
                        decisions.push((
                            first_due,
                            coalesced_through,
                            ScheduledExecutionReason::Missed,
                        ));
                    }
                    decisions.push((latest_due, None, ScheduledExecutionReason::Missed));
                }

                let mut snapshots = Vec::new();
                for (scheduled_at_ms, coalesced_through_ms, default_reason) in decisions {
                    let is_current =
                        !cold_recovery && !paused_race && scheduled_at_ms == latest_due;
                    let dispatch_key =
                        timed_dispatch_key(&schedule.id, current.trigger_revision, scheduled_at_ms);
                    let (execution, record, claimed) = self.durable.decide_schedule_execution(
                        &schedule.id,
                        ScheduleDecision {
                            dispatch_kind: ScheduledExecutionDispatchKind::Timed,
                            dispatch_key,
                            scheduled_at_ms: Some(scheduled_at_ms),
                            coalesced_through_ms,
                            requested_at_ms: now,
                            forced_skip: (!is_current).then_some(default_reason),
                        },
                        self.notification_settings
                            .max_scheduled_execution_concurrency,
                    )?;
                    if let Some(record) = record {
                        undo.record(record);
                    }
                    if claimed {
                        dispatch.push(execution.id.clone());
                    }
                    snapshots.push(execution);
                }
                let mut schedule_state = lock(&schedule.state)?;
                let previous = schedule_state.clone();
                schedule_state.evaluation_frontier_ms = latest_due;
                schedule_state.evaluation_frontier_trigger_revision =
                    schedule_state.trigger_revision;
                drop(schedule_state);
                undo.record(DurableUndo::ScheduleState { schedule, previous });
                let events = snapshots
                    .iter()
                    .map(|execution| DaemonEventKind::ScheduledExecutionCreated {
                        workspace_id: execution.workspace_id.clone(),
                        execution: execution.clone(),
                    })
                    .collect();
                Ok(DurableMutation::Changed(snapshots, events))
            })?;
            drop(response);
            for execution_id in dispatch.drain(..) {
                self.wait_for_native_claim_barrier();
                let execution = self.durable.execution(&execution_id)?;
                if let Err(error) = self.dispatch_schedule_execution(Arc::clone(&execution)) {
                    let current = execution.snapshot()?;
                    if current.state == ScheduledExecutionState::Claimed {
                        self.terminalize_dispatch_failure(&execution)?;
                    }
                    eprintln!("boomux: timed scheduled dispatch failed: {error}");
                }
            }
        }
        Ok(())
    }

    fn next_scheduled_occurrence_ms(&self) -> io::Result<Option<u64>> {
        let schedules = lock(&self.durable.state)?
            .schedules
            .values()
            .cloned()
            .collect::<Vec<_>>();
        let mut next = None;
        for schedule in schedules {
            let state = lock(&schedule.state)?.clone();
            if state.state != AgentScheduleState::Enabled {
                continue;
            }
            let cron = crate::scheduling::CronSchedule::compile(
                &state.trigger.cron,
                &state.trigger.timezone,
            )
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error.to_string()))?;
            let candidate = cron
                .next_after_ms(state.evaluation_frontier_ms)
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error.to_string()))?;
            next = Some(next.map_or(candidate, |current: u64| current.min(candidate)));
        }
        Ok(next)
    }

    fn dispatch_arc(
        self: &Arc<Self>,
        request: Request,
        response_version: u32,
    ) -> DaemonResult<Response> {
        if matches!(request, Request::AddNodeRegistration { .. }) {
            let response = self.dispatch(request)?;
            if let Response::NodeRegistration { registration } = &response {
                self.start_node_projection_worker(registration.node_id.clone())?;
            }
            return Ok(response);
        }
        if let Request::ListScheduledExecutions {
            workspace_id,
            schedule_id,
            limit,
        } = request
        {
            return self.list_scheduled_executions(
                workspace_id.as_deref(),
                schedule_id.as_deref(),
                limit,
                response_version,
            );
        }
        if let Request::RunAgentSchedule {
            schedule_id,
            dispatch_key,
        } = request
        {
            self.wait_for_native_pre_dispatch_barrier();
            let _dispatch_eligibility = lock(&self.schedule_dispatch_lock)?;
            let response = self.durable_mutation_outcome(|undo| {
                let (execution, record, _) = self.durable.decide_schedule_execution(
                    &schedule_id,
                    ScheduleDecision {
                        dispatch_kind: ScheduledExecutionDispatchKind::Manual,
                        dispatch_key: dispatch_key.clone(),
                        scheduled_at_ms: None,
                        coalesced_through_ms: None,
                        requested_at_ms: self.clock_now_ms(),
                        forced_skip: None,
                    },
                    self.notification_settings
                        .max_scheduled_execution_concurrency,
                )?;
                let Some(record) = record else {
                    return Ok(DurableMutation::Unchanged(Response::ScheduledExecution {
                        execution,
                        next_occurrence: None,
                    }));
                };
                undo.record(record);
                Ok(DurableMutation::Changed(
                    Response::ScheduledExecution {
                        execution: execution.clone(),
                        next_occurrence: None,
                    },
                    vec![DaemonEventKind::ScheduledExecutionCreated {
                        workspace_id: execution.workspace_id.clone(),
                        execution,
                    }],
                ))
            })?;
            let Response::ScheduledExecution { execution, .. } = response else {
                unreachable!("claim returns an execution")
            };
            if execution.state == ScheduledExecutionState::Skipped {
                return scheduled_execution_response(execution, response_version);
            }
            self.wait_for_native_claim_barrier();
            let execution = self.durable.execution(&execution.id)?;
            return match self.dispatch_schedule_execution(Arc::clone(&execution)) {
                Ok(execution) => scheduled_execution_response(execution, response_version),
                Err(error) => {
                    let current = execution.snapshot()?;
                    if current.state == ScheduledExecutionState::Claimed {
                        self.terminalize_dispatch_failure(&execution)
                        .map_err(|transition| {
                            DaemonError::Internal(io::Error::other(format!(
                                "scheduled dispatch failed: {error}; could not record failure: {transition}"
                            )))
                        })?;
                    }
                    Err(error)
                }
            };
        }
        self.dispatch(request)
    }

    fn list_scheduled_executions(
        &self,
        workspace_id: Option<&str>,
        schedule_id: Option<&str>,
        requested_limit: Option<u16>,
        response_version: u32,
    ) -> DaemonResult<Response> {
        let mut executions = self
            .durable
            .scheduled_executions(workspace_id, schedule_id)?;
        if !protocol::ProtocolFeature::TimedScheduling.is_supported_by(response_version) {
            executions.retain(|execution| {
                execution.dispatch_kind == ScheduledExecutionDispatchKind::Manual
                    && execution.state != ScheduledExecutionState::Skipped
            });
        }
        let observation_supported = protocol::ProtocolFeature::ScheduledExecutionObservation
            .is_supported_by(response_version);
        let (limit, truncated) = if observation_supported {
            let limit = requested_limit
                .unwrap_or(protocol::DEFAULT_SCHEDULED_EXECUTION_LIST_LIMIT)
                .clamp(1, protocol::MAX_SCHEDULED_EXECUTION_LIST_LIMIT);
            let truncated = executions.len() > usize::from(limit);
            executions.truncate(usize::from(limit));
            (limit, truncated)
        } else {
            (0, false)
        };
        let mut schedules = Vec::new();
        let mut schedule_limit = 0;
        let mut schedules_truncated = false;
        if observation_supported {
            (schedules, schedule_limit, schedules_truncated) = self
                .durable
                .scheduled_execution_schedule_projections(workspace_id, schedule_id)?;
        }
        Ok(Response::ScheduledExecutions {
            executions,
            limit,
            truncated,
            schedules,
            schedule_limit,
            schedules_truncated,
        })
    }

    fn clear_terminal_histories(&self) -> io::Result<()> {
        if self.durable.clear_terminal_histories()? {
            self.mark_persistence_dirty();
        }
        Ok(())
    }

    #[cfg(debug_assertions)]
    fn native_test_hooks_enabled(&self) -> bool {
        self.startup_environment
            .variables
            .iter()
            .any(|variable| variable.name == b"BOOMUX_NATIVE_TEST_HOOKS" && variable.value == b"1")
    }

    #[cfg(debug_assertions)]
    fn wait_for_native_claim_barrier(&self) {
        if !self.native_test_hooks_enabled() {
            return;
        }
        let Some(variable) = self
            .startup_environment
            .variables
            .iter()
            .find(|variable| variable.name == b"BOOMUX_NATIVE_TEST_CLAIM_BARRIER")
        else {
            return;
        };
        let directory = PathBuf::from(std::ffi::OsString::from_vec(variable.value.clone()));
        let _ = fs::write(directory.join("claimed"), b"claimed");
        while !directory.join("release").exists() && !self.runtimes.is_stopping() {
            thread::sleep(IO_RETRY_DELAY);
        }
    }

    #[cfg(not(debug_assertions))]
    fn wait_for_native_claim_barrier(&self) {}

    #[cfg(debug_assertions)]
    fn wait_for_native_pre_dispatch_barrier(&self) {
        if !self.native_test_hooks_enabled() {
            return;
        }
        let Some(variable) = self
            .startup_environment
            .variables
            .iter()
            .find(|variable| variable.name == b"BOOMUX_NATIVE_TEST_PRE_DISPATCH_BARRIER")
        else {
            return;
        };
        let directory = PathBuf::from(std::ffi::OsString::from_vec(variable.value.clone()));
        let _ = fs::write(directory.join("waiting"), b"waiting");
        while !directory.join("release").exists() && !self.runtimes.is_stopping() {
            thread::sleep(IO_RETRY_DELAY);
        }
    }

    #[cfg(not(debug_assertions))]
    fn wait_for_native_pre_dispatch_barrier(&self) {}

    #[cfg(debug_assertions)]
    fn wait_for_native_start_barrier(&self) {
        if !self.native_test_hooks_enabled() {
            return;
        }
        let Some(variable) = self
            .startup_environment
            .variables
            .iter()
            .find(|variable| variable.name == b"BOOMUX_NATIVE_TEST_START_BARRIER")
        else {
            return;
        };
        let directory = PathBuf::from(std::ffi::OsString::from_vec(variable.value.clone()));
        let _ = fs::write(directory.join("started"), b"started");
        while !directory.join("release").exists() && !self.runtimes.is_stopping() {
            thread::sleep(IO_RETRY_DELAY);
        }
    }

    #[cfg(not(debug_assertions))]
    fn wait_for_native_start_barrier(&self) {}

    #[cfg(debug_assertions)]
    fn wait_for_native_outcome_barrier(&self) {
        if !self.native_test_hooks_enabled() {
            return;
        }
        let Some(variable) = self
            .startup_environment
            .variables
            .iter()
            .find(|variable| variable.name == b"BOOMUX_NATIVE_TEST_OUTCOME_BARRIER")
        else {
            return;
        };
        let directory = PathBuf::from(std::ffi::OsString::from_vec(variable.value.clone()));
        let _ = fs::write(directory.join("outcome"), b"outcome");
        while !directory.join("release").exists() && !self.runtimes.is_stopping() {
            thread::sleep(IO_RETRY_DELAY);
        }
    }

    #[cfg(not(debug_assertions))]
    fn wait_for_native_outcome_barrier(&self) {}

    fn agent_resume_command(
        &self,
        shell: &Shell,
        previous_run: Option<&PersistedShellRun>,
    ) -> io::Result<Option<Vec<String>>> {
        if !self.notification_settings.resume_agents {
            return Ok(None);
        }
        let Some(previous_run) = previous_run
            .filter(|run| matches!(run.exit_reason, Some(ShellRunExitReason::Interrupted)))
        else {
            return Ok(None);
        };
        self.durable.agent_resume_command(shell, previous_run)
    }

    fn change_execution(
        &self,
        execution: &Arc<ScheduledExecution>,
        mutate: impl FnOnce(&mut ScheduledExecutionMutableState) -> io::Result<()>,
    ) -> DaemonResult<ScheduledExecutionSnapshot> {
        self.durable_mutation(|undo| {
            let (snapshot, record) = self.durable.mutate_execution(execution, mutate)?;
            undo.record(record);
            Ok((
                Response::ScheduledExecution {
                    execution: snapshot.clone(),
                    next_occurrence: None,
                },
                vec![DaemonEventKind::ScheduledExecutionChanged {
                    workspace_id: snapshot.workspace_id.clone(),
                    execution: snapshot.clone(),
                }],
            ))
        })
        .and_then(|response| match response {
            Response::ScheduledExecution { execution, .. } => Ok(execution),
            _ => Err(DaemonError::Internal(io::Error::other(
                "execution mutation returned an unexpected response",
            ))),
        })
    }

    fn terminalize_dispatch_failure(
        &self,
        execution: &Arc<ScheduledExecution>,
    ) -> DaemonResult<ScheduledExecutionSnapshot> {
        let _mutation = lock(&self.mutation_lock)?;
        self.ensure_running()?;
        let current = execution.snapshot()?;
        if current.state.is_terminal() {
            return Ok(current);
        }
        let _persistence = lock(&self.durable.persist_lock)?;
        let mut transaction = self.events.transaction()?;
        transaction.reserve_with_pending(1)?;
        let (failed, _) = self.durable.mutate_execution(execution, |state| {
            if !state.state.is_terminal() {
                state.state = ScheduledExecutionState::DispatchFailed;
                state.ended_at_ms = Some(unix_time_ms().max(execution.requested_at_ms));
                state.reason = Some(ScheduledExecutionReason::RunnerStartFailed);
                state.outcome = None;
            }
            Ok(())
        })?;
        let events = vec![DaemonEventKind::ScheduledExecutionChanged {
            workspace_id: failed.workspace_id.clone(),
            execution: failed.clone(),
        }];
        let notifications =
            self.execution_notification_requests(&events, &transaction.events.committed_executions);
        let saved = self.capture_persisted_state()?;
        transaction.begin_persistence(events.len());
        drop(transaction);
        let committed = match self.write_persisted_state(saved) {
            Ok(committed) => committed,
            Err(error) => {
                let mut transaction = self.events.transaction()?;
                transaction.finish_persistence();
                self.queue_durable_batch_locked(events, &mut transaction);
                drop(transaction);
                self.events.notify();
                return Err(DaemonError::persistence(error));
            }
        };
        let mut transaction = self.events.transaction()?;
        transaction.replace_committed_executions(committed);
        transaction.append_batch(events);
        transaction.finish_persistence();
        drop(transaction);
        drop(_persistence);
        drop(_mutation);
        self.events.notify();
        for notification in notifications {
            self.notification_sink.notify(notification);
        }
        Ok(failed)
    }

    fn prepare_schedule_shell(
        &self,
        execution: &Arc<ScheduledExecution>,
    ) -> DaemonResult<Option<Arc<Shell>>> {
        let response = self.durable_mutation(|undo| {
            let schedule = self.schedule(&execution.schedule_id)?;
            let (execution_status, execution_shell_id) = {
                let execution_state = lock(&execution.state)?;
                (execution_state.state, execution_state.shell_id.clone())
            };
            if execution_status != ScheduledExecutionState::Claimed {
                return Ok((
                    Response::ScheduledExecution {
                        execution: execution.snapshot()?,
                        next_occurrence: None,
                    },
                    Vec::new(),
                ));
            }
            if execution_shell_id.is_none()
                && (self.durable.continuation_session_is_active(&schedule)?
                    || self.durable.continuation_lease_is_occupied(&schedule)?)
            {
                let (skipped, record) = self.durable.mutate_execution(execution, |state| {
                    if state.state == ScheduledExecutionState::Claimed && state.shell_id.is_none() {
                        state.state = ScheduledExecutionState::Skipped;
                        state.ended_at_ms =
                            Some(self.clock_now_ms().max(execution.requested_at_ms));
                        state.reason = Some(ScheduledExecutionReason::ActiveSession);
                    }
                    Ok(())
                })?;
                undo.record(record);
                return Ok((
                    Response::ScheduledExecution {
                        execution: skipped.clone(),
                        next_occurrence: None,
                    },
                    vec![DaemonEventKind::ScheduledExecutionChanged {
                        workspace_id: skipped.workspace_id.clone(),
                        execution: skipped,
                    }],
                ));
            }
            if let Some(shell_id) = &execution_shell_id {
                return Ok((
                    Response::Shell {
                        shell: self.shell(shell_id)?.snapshot()?,
                    },
                    Vec::new(),
                ));
            }
            let existing_shell_id = lock(&schedule.state)?.execution_shell_id.clone();
            let (shell, shell_created) = if let Some(shell_id) = &existing_shell_id {
                (self.shell(shell_id)?, false)
            } else {
                let executable = replacement_executable()?;
                let command = vec![
                    executable.to_string_lossy().into_owned(),
                    "__scheduled-runner".into(),
                    schedule.id.clone(),
                ];
                let (shell, record) = self.durable.create_schedule_shell(&schedule, command)?;
                undo.record(record);
                let mut schedule_state = lock(&schedule.state)?;
                if schedule_state.execution_shell_id.is_some() {
                    return Err(DaemonError::lifecycle(
                        ErrorCode::RunChanged,
                        "schedule execution shell changed during preparation",
                    ));
                }
                let previous = schedule_state.clone();
                schedule_state.execution_shell_id = Some(shell.id.clone());
                undo.record(DurableUndo::ScheduleState {
                    schedule: Arc::clone(&schedule),
                    previous,
                });
                (shell, true)
            };
            if shell.workspace_id != execution.workspace_id
                || shell.owner
                    != (ShellOwner::Schedule {
                        schedule_id: schedule.id.clone(),
                    })
            {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "schedule-owned shell does not match its schedule",
                )
                .into());
            }
            let (snapshot, record) = self.durable.mutate_execution(execution, |state| {
                if state.state != ScheduledExecutionState::Claimed || state.shell_id.is_some() {
                    return Err(io::Error::new(
                        io::ErrorKind::ConnectionAborted,
                        "scheduled execution claim changed during shell preparation",
                    ));
                }
                state.shell_id = Some(shell.id.clone());
                Ok(())
            })?;
            undo.record(record);
            let mut events = Vec::new();
            if shell_created {
                events.push(DaemonEventKind::ShellCreated {
                    workspace_id: shell.workspace_id.clone(),
                    shell_id: shell.id.clone(),
                    name: lock(&shell.name)?.clone(),
                });
            }
            events.push(DaemonEventKind::ScheduledExecutionChanged {
                workspace_id: snapshot.workspace_id.clone(),
                execution: snapshot,
            });
            Ok((
                Response::Shell {
                    shell: shell.snapshot()?,
                },
                events,
            ))
        })?;
        match response {
            Response::Shell { shell } => self.shell(&shell.id).map(Some).map_err(Into::into),
            Response::ScheduledExecution { .. } => Ok(None),
            _ => Err(DaemonError::Internal(io::Error::other(
                "schedule shell preparation returned an unexpected response",
            ))),
        }
    }

    fn dispatch_schedule_execution(
        self: &Arc<Self>,
        execution: Arc<ScheduledExecution>,
    ) -> DaemonResult<ScheduledExecutionSnapshot> {
        if execution.snapshot()?.state != ScheduledExecutionState::Claimed {
            return Ok(execution.snapshot()?);
        }
        let Some(shell) = self.prepare_schedule_shell(&execution)? else {
            return execution.snapshot().map_err(Into::into);
        };
        let _mutation = lock(&self.mutation_lock)?;
        self.ensure_running()?;
        {
            let state = lock(&execution.state)?;
            if state.state != ScheduledExecutionState::Claimed
                || state.shell_id.as_deref() != Some(shell.id.as_str())
            {
                return Ok(execution.snapshot_from(&state));
            }
        }
        let old_runtime = match &*lock(&shell.lifecycle)? {
            ShellLifecycle::Exited { runtime, .. } => runtime.clone(),
            ShellLifecycle::Pending => None,
            ShellLifecycle::Running { .. } => {
                return Err(DaemonError::lifecycle(
                    ErrorCode::Busy,
                    "schedule-owned shell is already running",
                ));
            }
            ShellLifecycle::Closed => return Err(not_found("shell", &shell.id).into()),
        };
        if let Some(runtime) = old_runtime {
            self.runtimes.stop_reader(&runtime)?;
            *lock(&shell.lifecycle)? = ShellLifecycle::Pending;
        }
        self.flush_pending()?;
        let _persistence = lock(&self.durable.persist_lock)?;
        let mut transaction = self.events.transaction()?;
        transaction.reserve(2)?;
        let workspace = self.workspace(&shell.workspace_id)?;
        let workspace_name = lock(&workspace.name)?.clone();
        let shell_name = lock(&shell.name)?.clone();
        let profile = TerminalProfile {
            term: Some("xterm-256color".into()),
            colorterm: None,
            term_program: Some("boomux-scheduled".into()),
            term_program_version: None,
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        };
        let previous_run = lock(&shell.last_run)?.clone();
        let generation = previous_run.as_ref().map_or(Ok(1), |run| {
            run.generation
                .checked_add(1)
                .ok_or_else(|| io::Error::other("shell run generation exhausted"))
        })?;
        let run = Arc::new(ShellRun::new(generation));
        let mut runner_environment = self.startup_environment.clone();
        runner_environment
            .variables
            .retain(|variable| !variable.name.starts_with(b"BOOMUX_NATIVE_TEST_"));
        runner_environment.variables.push(UnixEnvironmentVariable {
            name: b"BOOMUX_SCHEDULE_RUNNER_TOKEN".to_vec(),
            value: execution.runner_token.as_bytes().to_vec(),
        });
        let (runtime, reader) = match self.runtimes.spawn_runtime(
            &shell,
            &run,
            RuntimeStart {
                workspace_name: &workspace_name,
                shell_name: &shell_name,
                profile: &profile,
                environment: Some(&runner_environment),
                recovery: RuntimeRecovery::default(),
            },
        ) {
            Ok(runtime) => runtime,
            Err(error) => {
                drop(transaction);
                drop(_persistence);
                drop(_mutation);
                return self.terminalize_dispatch_failure(&execution).map_err(|transition| DaemonError::Internal(io::Error::other(format!(
                    "could not start scheduled runner: {error}; could not record failure: {transition}"
                ))));
            }
        };
        *lock(&shell.lifecycle)? = ShellLifecycle::Running {
            profile: profile.clone(),
            run: Arc::clone(&run),
            runtime: Arc::clone(&runtime),
        };
        *lock(&shell.last_run)? = Some(run.persisted(profile)?);
        if let Err(error) = self.runtimes.start_pty_reader(
            Arc::downgrade(self),
            Arc::clone(&shell),
            Arc::clone(&run),
            Arc::clone(&runtime),
            reader,
            true,
        ) {
            let cleanup = self.runtimes.kill(&shell);
            let _ = self.runtimes.reset_pending(&shell);
            drop(transaction);
            drop(_persistence);
            drop(_mutation);
            return self
                .terminalize_dispatch_failure(&execution)
                .map_err(|transition| {
                    DaemonError::Internal(io::Error::other(format!(
                        "could not start scheduled runner reader: {error}; cleanup: {}; could not record failure: {transition}",
                        cleanup
                            .err()
                            .map_or_else(|| "ok".into(), |error| error.to_string())
                    )))
                });
        }
        let binding = self.durable.mutate_execution(&execution, |state| {
            if state.state != ScheduledExecutionState::Claimed
                || state.shell_id.as_deref() != Some(shell.id.as_str())
            {
                return Err(io::Error::new(
                    io::ErrorKind::ConnectionAborted,
                    "scheduled execution claim was revoked before runner binding",
                ));
            }
            state.state = ScheduledExecutionState::Starting;
            state.run_id = Some(run.id.clone());
            Ok(())
        });
        let (starting, _undo) = match binding {
            Ok(binding) => binding,
            Err(error) => {
                let run_event = self.runtimes.kill_with_event(&shell);
                let _ = self.runtimes.reset_pending(&shell);
                let failed = self
                    .durable
                    .mutate_execution(&execution, |state| {
                        if !state.state.is_terminal() {
                            state.state = ScheduledExecutionState::DispatchFailed;
                            state.run_id.get_or_insert_with(|| run.id.clone());
                            state.ended_at_ms = Some(unix_time_ms().max(execution.requested_at_ms));
                            state.reason = Some(ScheduledExecutionReason::RunnerStartFailed);
                            state.outcome = None;
                        }
                        Ok(())
                    })
                    .map(|(snapshot, _)| snapshot);
                let mut events = Vec::new();
                if let Ok(Some(event)) = run_event {
                    events.push(event);
                }
                if let Ok(snapshot) = failed {
                    events.push(DaemonEventKind::ScheduledExecutionChanged {
                        workspace_id: snapshot.workspace_id.clone(),
                        execution: snapshot,
                    });
                }
                self.queue_durable_batch_locked(events, &mut transaction);
                drop(transaction);
                drop(_persistence);
                drop(_mutation);
                self.events.notify();
                return Err(error.into());
            }
        };
        self.wait_for_native_start_barrier();
        let saved = match self.capture_persisted_state() {
            Ok(saved) => saved,
            Err(error) => {
                let run_event = self.runtimes.kill_with_event(&shell);
                let _ = self.runtimes.reset_pending(&shell);
                let failed = self
                    .durable
                    .mutate_execution(&execution, |state| {
                        state.state = ScheduledExecutionState::DispatchFailed;
                        state.ended_at_ms = Some(unix_time_ms().max(execution.requested_at_ms));
                        state.reason = Some(ScheduledExecutionReason::RunnerStartFailed);
                        state.outcome = None;
                        Ok(())
                    })
                    .map(|(snapshot, _)| snapshot);
                let mut events = Vec::new();
                if let Ok(Some(event)) = run_event {
                    events.push(event);
                }
                if let Ok(snapshot) = failed {
                    events.push(DaemonEventKind::ScheduledExecutionChanged {
                        workspace_id: snapshot.workspace_id.clone(),
                        execution: snapshot,
                    });
                }
                self.queue_durable_batch_locked(events, &mut transaction);
                drop(transaction);
                drop(_persistence);
                drop(_mutation);
                self.events.notify();
                return Err(error.into());
            }
        };
        transaction.begin_persistence(2);
        drop(transaction);
        let committed = match self.write_persisted_state(saved) {
            Ok(committed) => committed,
            Err(error) => {
                let run_event = self.runtimes.kill_with_event(&shell);
                let _ = self.runtimes.reset_pending(&shell);
                let failed = self
                    .durable
                    .mutate_execution(&execution, |state| {
                        state.state = ScheduledExecutionState::DispatchFailed;
                        state.ended_at_ms = Some(unix_time_ms().max(execution.requested_at_ms));
                        state.reason = Some(ScheduledExecutionReason::RunnerStartFailed);
                        state.outcome = None;
                        Ok(())
                    })
                    .map(|(snapshot, _)| snapshot);
                let mut transaction = self.events.transaction()?;
                transaction.finish_persistence();
                let mut events = Vec::new();
                if let Ok(Some(event)) = &run_event {
                    events.push(event.clone());
                }
                if let Ok(snapshot) = &failed {
                    events.push(DaemonEventKind::ScheduledExecutionChanged {
                        workspace_id: snapshot.workspace_id.clone(),
                        execution: snapshot.clone(),
                    });
                }
                self.queue_durable_batch_locked(events, &mut transaction);
                drop(transaction);
                drop(_persistence);
                drop(_mutation);
                self.events.notify();
                return Err(DaemonError::persistence_context(
                error,
                run_event.map_or_else(
                    |cleanup| {
                        format!(
                            "could not persist scheduled runner start; cleanup failed: {cleanup}"
                        )
                    },
                    |_| {
                        failed.map_or_else(
                            |failure| {
                                format!(
                                    "could not persist scheduled runner start; could not record dispatch failure: {failure}"
                                )
                            },
                            |_| "could not persist scheduled runner start".into(),
                        )
                    },
                ),
            ));
            }
        };
        let mut transaction = self.events.transaction()?;
        transaction.replace_committed_executions(committed);
        transaction.append_batch(vec![
            DaemonEventKind::RunStarted {
                workspace_id: shell.workspace_id.clone(),
                shell_id: shell.id.clone(),
                run: run.snapshot()?,
            },
            DaemonEventKind::ScheduledExecutionChanged {
                workspace_id: starting.workspace_id.clone(),
                execution: starting,
            },
        ]);
        transaction.finish_persistence();
        drop(transaction);
        drop(_persistence);
        drop(_mutation);
        self.events.notify();
        self.runtimes.resume_reader(&runtime)?;
        Ok(execution.snapshot()?)
    }

    fn cancel_scheduled_execution(
        &self,
        execution_id: &str,
        expected_revision: Option<u64>,
    ) -> DaemonResult<Response> {
        let _mutation = lock(&self.mutation_lock)?;
        self.ensure_running()?;
        self.flush_pending()?;
        let execution = self.durable.execution(execution_id)?;
        let current = execution.snapshot()?;
        if let Some(expected) = expected_revision {
            require_guard(current.revision, expected, "scheduled execution")?;
        }
        if current.state.is_terminal() {
            return Ok(Response::ScheduledExecution {
                execution: current,
                next_occurrence: None,
            });
        }

        let bound_shell = match current.state {
            ScheduledExecutionState::Claimed => {
                if current.run_id.is_some() {
                    return Err(DaemonError::Internal(io::Error::other(
                        "claimed scheduled execution has a run binding",
                    )));
                }
                None
            }
            ScheduledExecutionState::Starting | ScheduledExecutionState::Active => {
                let shell_id = current.shell_id.as_deref().ok_or_else(|| {
                    DaemonError::Internal(io::Error::other(
                        "started scheduled execution has no shell binding",
                    ))
                })?;
                let run_id = current.run_id.as_deref().ok_or_else(|| {
                    DaemonError::Internal(io::Error::other(
                        "started scheduled execution has no run binding",
                    ))
                })?;
                let shell = self.shell(shell_id)?;
                if !matches!(
                    &*lock(&shell.lifecycle)?,
                    ShellLifecycle::Running { run, .. } if run.id == run_id
                ) {
                    return Err(DaemonError::lifecycle(
                        ErrorCode::RunChanged,
                        "scheduled execution no longer owns the exact shell run",
                    ));
                }
                Some(shell)
            }
            _ => unreachable!("terminal execution returned above"),
        };

        let event_capacity = 1 + usize::from(bound_shell.is_some());
        let mut transaction = self.events.transaction()?;
        transaction.reserve_with_pending(event_capacity)?;
        transaction.begin_lifecycle_reservation(event_capacity);
        drop(transaction);
        let _lifecycle_activity = LifecycleActivity::begin(&self.events.lifecycle_active);

        let mut stop_failure = None;
        let mut run_event = None;
        if let Some(shell) = &bound_shell {
            #[cfg(debug_assertions)]
            if self.native_test_hooks_enabled()
                && self.startup_environment.variables.iter().any(|variable| {
                    variable.name == b"BOOMUX_NATIVE_TEST_CANCEL_STOP_FAILURE"
                        && variable.value == b"1"
                })
            {
                if let Some(variable) =
                    self.startup_environment.variables.iter().find(|variable| {
                        variable.name == b"BOOMUX_NATIVE_TEST_CANCEL_FAILURE_BARRIER"
                    })
                {
                    let barrier =
                        PathBuf::from(std::ffi::OsString::from_vec(variable.value.clone()));
                    fs::write(barrier.join("reserved"), b"reserved")?;
                    while !barrier.join("release").exists() {
                        thread::sleep(IO_RETRY_DELAY);
                    }
                }
                let mut transaction = self.events.transaction()?;
                transaction.release_lifecycle_reservation();
                return Err(DaemonError::Internal(io::Error::other(
                    "injected scheduled cancellation stop failure",
                )));
            }
            if let Err(error) = self.runtimes.stop_runtime(shell) {
                if !error.stopped {
                    let mut transaction = self.events.transaction()?;
                    transaction.release_lifecycle_reservation();
                    return Err(error.source.into());
                }
                stop_failure = Some(error.source);
            }
            match self.runtimes.finalize_stop(shell) {
                Ok(rollback) => run_event = rollback.event,
                Err(error) => {
                    stop_failure.get_or_insert(error);
                }
            }
            if let Err(error) = self.runtimes.reset_pending(shell) {
                stop_failure.get_or_insert(error);
            }
        }

        let terminal_state = if stop_failure.is_some() {
            ScheduledExecutionState::Interrupted
        } else {
            ScheduledExecutionState::Cancelled
        };
        let terminal_reason = if stop_failure.is_some() {
            ScheduledExecutionReason::RunnerExitedWithoutReport
        } else {
            ScheduledExecutionReason::CancelledByUser
        };
        let (cancelled, undo) = self.durable.mutate_execution(&execution, |state| {
            state.state = terminal_state;
            state.ended_at_ms = Some(unix_time_ms().max(execution.requested_at_ms));
            state.reason = Some(terminal_reason);
            state.outcome = None;
            Ok(())
        })?;
        let mut events = Vec::with_capacity(event_capacity);
        if let Some(event) = run_event {
            events.push(event);
        }
        events.push(DaemonEventKind::ScheduledExecutionChanged {
            workspace_id: cancelled.workspace_id.clone(),
            execution: cancelled.clone(),
        });

        let persistence = lock(&self.durable.persist_lock)?;
        let saved = match self.capture_persisted_state() {
            Ok(saved) => saved,
            Err(error) => {
                let mut transaction = self.events.transaction()?;
                if bound_shell.is_none() {
                    let rollback = self.durable.rollback(undo);
                    transaction.release_lifecycle_reservation();
                    return Err(Self::lifecycle_failure(error.into(), rollback));
                }
                self.queue_durable_batch_locked(events, &mut transaction);
                transaction.release_lifecycle_reservation();
                return Err(error.into());
            }
        };
        let mut transaction = self.events.transaction()?;
        transaction.transfer_lifecycle_reservation_to_persistence(events.len(), 0);
        drop(transaction);
        let committed = match self.write_persisted_state(saved) {
            Ok(committed) => committed,
            Err(error) => {
                let mut transaction = self.events.transaction()?;
                transaction.finish_persistence();
                if bound_shell.is_none() {
                    let rollback = self.durable.rollback(undo);
                    return Err(Self::lifecycle_failure(
                        DaemonError::persistence(error),
                        rollback,
                    ));
                }
                self.queue_durable_batch_locked(events, &mut transaction);
                drop(transaction);
                drop(persistence);
                self.events.notify();
                return Err(DaemonError::persistence(error));
            }
        };
        let mut transaction = self.events.transaction()?;
        transaction.replace_committed_executions(committed);
        transaction.append_batch(events);
        transaction.finish_persistence();
        drop(transaction);
        drop(persistence);
        self.events.notify();
        if let Some(error) = stop_failure {
            return Err(DaemonError::Internal(error));
        }
        Ok(Response::ScheduledExecution {
            execution: cancelled,
            next_occurrence: None,
        })
    }

    fn resume_claimed_schedule_executions(self: &Arc<Self>) {
        let schedules = match lock(&self.durable.state) {
            Ok(state) => state.schedules.values().cloned().collect::<Vec<_>>(),
            Err(error) => {
                eprintln!("boomux: could not inspect claimed scheduled executions: {error}");
                return;
            }
        };
        let mut claimed = Vec::new();
        for schedule in schedules {
            let executions = match lock(&schedule.executions) {
                Ok(executions) => executions.clone(),
                Err(error) => {
                    eprintln!(
                        "boomux: could not inspect schedule {}: {error}",
                        schedule.id
                    );
                    continue;
                }
            };
            claimed.extend(executions.into_iter().filter(|execution| {
                lock(&execution.state)
                    .is_ok_and(|state| state.state == ScheduledExecutionState::Claimed)
            }));
        }
        for execution in claimed {
            let _dispatch_eligibility = match lock(&self.schedule_dispatch_lock) {
                Ok(guard) => guard,
                Err(error) => {
                    eprintln!("boomux: could not acquire schedule dispatch lease: {error}");
                    return;
                }
            };
            if let Err(error) = self.dispatch_schedule_execution(Arc::clone(&execution)) {
                eprintln!(
                    "boomux: could not resume scheduled execution {} after handoff: {error}",
                    execution.id
                );
                let _ = self.change_execution(&execution, |state| {
                    if state.state == ScheduledExecutionState::Claimed {
                        state.state = ScheduledExecutionState::DispatchFailed;
                        state.ended_at_ms = Some(unix_time_ms().max(execution.requested_at_ms));
                        state.reason = Some(ScheduledExecutionReason::RunnerStartFailed);
                    }
                    Ok(())
                });
            }
        }
    }

    fn dispatch(&self, request: Request) -> DaemonResult<Response> {
        let _dispatch_eligibility = matches!(
            &request,
            Request::RegisterAgent { .. }
                | Request::EnsureAgent { .. }
                | Request::ReportAgent { .. }
        )
        .then(|| lock(&self.schedule_dispatch_lock))
        .transpose()?;
        match request {
            Request::Ping => Ok(Response::Pong),
            Request::GetNodeIdentity => self
                .node_identity
                .as_ref()
                .ok_or_else(|| {
                    DaemonError::lifecycle(
                        ErrorCode::NodeIdentityUnavailable,
                        "Boomux Node identity is unavailable",
                    )
                })?
                .id()
                .map(|node_id| Response::NodeIdentity { node_id })
                .map_err(DaemonError::from),
            Request::OpenFederationChannel => {
                unreachable!("federation channel is handled before dispatch")
            }
            Request::RekeyNode { .. } => unreachable!("Node rekey is handled before dispatch"),
            Request::AddNodeRegistration {
                alias,
                target,
                node_id,
            } => {
                let local_node_id = self.node_identity()?.id()?;
                self.node_registrations()?
                    .add(alias, target, node_id, &local_node_id)
                    .map(|registration| Response::NodeRegistration { registration })
                    .map_err(node_registration_error)
            }
            Request::ListNodeRegistrations => self
                .node_registrations()?
                .list()
                .map(|registrations| Response::NodeRegistrations { registrations })
                .map_err(node_registration_error),
            Request::GetNodeRegistration { selector } => self
                .node_registrations()?
                .inspect(&selector)
                .map(|registration| Response::NodeRegistration { registration })
                .map_err(node_registration_error),
            Request::RenameNodeRegistration {
                selector,
                alias,
                expected_revision,
            } => self
                .node_registrations()?
                .rename(&selector, alias, expected_revision)
                .map(|registration| Response::NodeRegistration { registration })
                .map_err(node_registration_error),
            Request::RetargetNodeRegistration {
                selector,
                target,
                node_id,
                expected_revision,
            } => {
                let registration = self
                    .node_registrations()?
                    .retarget(
                        &selector,
                        target,
                        &node_id,
                        expected_revision,
                        NODE_REGISTRATION_DRAIN_TIMEOUT,
                    )
                    .map_err(node_registration_error)?;
                if let Err(error) = self.node_projection_cache()?.remove(&registration.node_id) {
                    eprintln!("boomux: could not remove disposable Node projection: {error}");
                }
                Ok(Response::NodeRegistration { registration })
            }
            Request::ForgetNodeRegistration { selector } => {
                let registration = self
                    .node_registrations()?
                    .forget(&selector, NODE_REGISTRATION_DRAIN_TIMEOUT)
                    .map_err(node_registration_error)?;
                if let Err(error) = self.node_projection_cache()?.remove(&registration.node_id) {
                    eprintln!("boomux: could not remove disposable Node projection: {error}");
                }
                Ok(Response::NodeRegistration { registration })
            }
            Request::SyncNodeProjection { after, wait_ms } => Ok(Response::NodeProjectionSync {
                sync: self.node_projection_sync(after, wait_ms)?,
            }),
            Request::GetNodeProjectionHealth { selector } => {
                let registration = self
                    .node_registrations()?
                    .inspect(&selector)
                    .map_err(node_registration_error)?;
                Ok(Response::NodeProjectionHealth {
                    health: self.node_projection_cache()?.health(&registration)?,
                })
            }
            Request::GetCombinedNodeSnapshot { selector } => Ok(Response::CombinedNodeSnapshot {
                snapshot: self.combined_node_snapshot(selector.as_deref())?,
            }),
            Request::RouteNodeOperation { node_id, operation } => {
                Ok(self.route_node_operation(&node_id, operation))
            }
            Request::Restart | Request::RestartWithNotificationConfig { .. } => {
                unreachable!("restart is handled before dispatch")
            }
            Request::Shutdown => unreachable!("shutdown is handled before dispatch"),
            Request::Snapshot => Ok(Response::Snapshot {
                snapshot: self.snapshot()?,
            }),
            Request::GetFocusedTerminal => Ok(Response::FocusedTerminal {
                focused_terminal: self.focused_terminal()?,
            }),
            Request::GetWorkspace { workspace_id } => Ok(Response::Workspace {
                workspace: self.workspace(&workspace_id)?.snapshot(&self.durable)?,
            }),
            Request::GetShell { shell_id } => Ok(Response::Shell {
                shell: self.shell(&shell_id)?.snapshot()?,
            }),
            Request::GetLauncher { launcher_id } => Ok(Response::Launcher {
                launcher: self.launcher(&launcher_id)?.snapshot()?,
            }),
            Request::GetAgent { agent_id } => Ok(Response::Agent {
                agent: self.agent(&agent_id)?.snapshot()?,
            }),
            Request::GetAgentSchedule { schedule_id } => {
                let schedule = self.schedule(&schedule_id)?;
                Ok(Response::AgentScheduleInspection {
                    inspection: schedule.inspection()?,
                })
            }
            Request::ListScheduledExecutions {
                workspace_id,
                schedule_id,
                limit,
            } => {
                let (executions, limit, truncated) = self.durable.scheduled_execution_page(
                    workspace_id.as_deref(),
                    schedule_id.as_deref(),
                    limit.unwrap_or(protocol::DEFAULT_SCHEDULED_EXECUTION_LIST_LIMIT),
                )?;
                Ok(Response::ScheduledExecutions {
                    executions,
                    limit,
                    truncated,
                    schedules: Vec::new(),
                    schedule_limit: 0,
                    schedules_truncated: false,
                })
            }
            Request::GetScheduledExecution { execution_id } => {
                let execution = self.durable.execution(&execution_id)?.snapshot()?;
                let next_occurrence = self
                    .schedule(&execution.schedule_id)?
                    .snapshot()?
                    .next_occurrence;
                Ok(Response::ScheduledExecution {
                    execution,
                    next_occurrence,
                })
            }
            Request::WaitScheduledExecution {
                execution_id,
                after_revision,
                wait_ms,
            } => self.wait_scheduled_execution(&execution_id, after_revision, wait_ms),
            Request::WaitAgent {
                agent_id,
                after_revision,
                wait_ms,
            } => self.wait_agent(&agent_id, after_revision, wait_ms),
            Request::AcknowledgeAgentAttention {
                agent_id,
                observation_revision,
            } => self.durable_mutation_outcome(|undo| {
                let (agent, changed) = self.acknowledge_agent_attention_mutation(
                    undo,
                    &agent_id,
                    observation_revision,
                )?;
                if !changed {
                    return Ok(DurableMutation::Unchanged(
                        Response::AgentAttentionAcknowledged { agent, changed },
                    ));
                }
                let event = DaemonEventKind::AgentAttentionAcknowledged {
                    workspace_id: agent.workspace_id.clone(),
                    shell_id: agent.shell_id.clone(),
                    agent: agent.clone(),
                };
                Ok(DurableMutation::Changed(
                    Response::AgentAttentionAcknowledged { agent, changed },
                    vec![event],
                ))
            }),
            Request::CreateWorkspace {
                name,
                default_cwd,
                shells,
            } => self.durable_mutation(|undo| {
                let workspace = self.create_workspace_mutation(undo, name, default_cwd, shells)?;
                let events = workspace_created_events(&workspace);
                Ok((Response::Workspace { workspace }, events))
            }),
            Request::CreateShell {
                workspace_id,
                shell,
            } => self.durable_mutation(|undo| {
                let implicit_workspace = workspace_id.is_none();
                let shell = self.create_shell_mutation(undo, workspace_id.as_deref(), shell)?;
                let mut events = Vec::new();
                if implicit_workspace {
                    let workspace = self.workspace(&shell.workspace_id)?;
                    events.push(DaemonEventKind::WorkspaceCreated {
                        workspace_id: workspace.id.clone(),
                        name: lock(&workspace.name)?.clone(),
                    });
                }
                events.push(DaemonEventKind::ShellCreated {
                    workspace_id: shell.workspace_id.clone(),
                    shell_id: shell.id.clone(),
                    name: shell.name.clone(),
                });
                Ok((Response::Shell { shell }, events))
            }),
            Request::CreateLauncher { workspace_id, spec } => self.durable_mutation(|undo| {
                let launcher = self.create_launcher_mutation(undo, &workspace_id, spec)?;
                let event = DaemonEventKind::LauncherCreated {
                    workspace_id,
                    launcher_id: launcher.id.clone(),
                    name: launcher.name.clone(),
                };
                Ok((Response::Launcher { launcher }, vec![event]))
            }),
            Request::CreateAgentSchedule { workspace_id, spec } => self.durable_mutation(|undo| {
                let schedule = self.create_schedule_mutation(undo, &workspace_id, spec)?;
                let event = DaemonEventKind::AgentScheduleCreated {
                    workspace_id,
                    schedule: schedule.clone(),
                };
                Ok((Response::AgentSchedule { schedule }, vec![event]))
            }),
            Request::UpdateAgentSchedule {
                schedule_id,
                expected_revision,
                update,
            } => self.durable_mutation_outcome(|undo| {
                let (schedule, record) = self.durable.update_schedule_at(
                    &schedule_id,
                    expected_revision,
                    update,
                    self.clock_now_ms(),
                )?;
                let Some(record) = record else {
                    return Ok(DurableMutation::Unchanged(Response::AgentSchedule {
                        schedule,
                    }));
                };
                undo.record(record);
                Ok(DurableMutation::Changed(
                    Response::AgentSchedule {
                        schedule: schedule.clone(),
                    },
                    vec![DaemonEventKind::AgentScheduleUpdated {
                        workspace_id: schedule.workspace_id.clone(),
                        schedule,
                    }],
                ))
            }),
            Request::RunAgentSchedule { .. } => {
                unreachable!("schedule run is handled with the service Arc")
            }
            Request::CancelScheduledExecution { execution_id } => {
                self.cancel_scheduled_execution(&execution_id, None)
            }
            Request::GuardedCancelScheduledExecution {
                execution_id,
                expected_revision,
            } => self.cancel_scheduled_execution(&execution_id, Some(expected_revision)),
            Request::ResolveScheduledExecutionClaim {
                schedule_id,
                shell_id,
                run_id,
                runner_token,
            } => {
                let schedule = self.schedule(&schedule_id)?;
                let execution = lock(&schedule.executions)?
                    .iter()
                    .find(|execution| {
                        lock(&execution.state).is_ok_and(|state| {
                            matches!(
                                state.state,
                                ScheduledExecutionState::Starting | ScheduledExecutionState::Active
                            ) && state.shell_id.as_deref() == Some(shell_id.as_str())
                                && state.run_id.as_deref() == Some(run_id.as_str())
                                && execution.runner_token == runner_token.as_str()
                        })
                    })
                    .cloned()
                    .ok_or_else(|| {
                        DaemonError::lifecycle(
                            ErrorCode::RunChanged,
                            "no scheduled execution matches the exact runner claim",
                        )
                    })?;
                Ok(Response::ScheduledExecutionClaim {
                    claim: protocol::ScheduledExecutionClaim {
                        execution: execution.snapshot()?,
                        prompt: execution.prompt.clone(),
                    },
                })
            }
            Request::ReportScheduledRunner {
                execution_id,
                shell_id,
                run_id,
                runner_token,
                result,
            } => {
                let staged_outcome = matches!(result, ScheduledRunnerResult::Exited { .. });
                let response = self.durable_mutation_outcome(|undo| {
                    let execution = self.durable.execution(&execution_id)?;
                    let current = execution.snapshot()?;
                    if current.shell_id.as_deref() != Some(shell_id.as_str())
                        || current.run_id.as_deref() != Some(run_id.as_str())
                        || execution.runner_token != runner_token.as_str()
                    {
                        return Err(DaemonError::lifecycle(
                            ErrorCode::RunChanged,
                            "scheduled runner report does not match the exact execution run",
                        ));
                    }
                    if current.state.is_terminal() {
                        return Ok(DurableMutation::Unchanged(Response::ScheduledExecution {
                            execution: current,
                            next_occurrence: None,
                        }));
                    }
                    let unchanged = match &result {
                        ScheduledRunnerResult::Active => {
                            current.state == ScheduledExecutionState::Active
                                && current.started_at_ms.is_some()
                        }
                        ScheduledRunnerResult::SpawnFailed => {
                            current.reason == Some(ScheduledExecutionReason::HostSpawnFailed)
                        }
                        ScheduledRunnerResult::Exited { outcome } => {
                            current.outcome.as_ref() == Some(outcome)
                        }
                    };
                    if unchanged {
                        return Ok(DurableMutation::Unchanged(Response::ScheduledExecution {
                            execution: current,
                            next_occurrence: None,
                        }));
                    }
                    let now = unix_time_ms().max(execution.requested_at_ms);
                    let (snapshot, record) =
                        self.durable.mutate_execution(&execution, |state| {
                            match result {
                                ScheduledRunnerResult::Active => {
                                    state.state = ScheduledExecutionState::Active;
                                    state.started_at_ms.get_or_insert(now);
                                }
                                ScheduledRunnerResult::SpawnFailed => {
                                    state.reason = Some(ScheduledExecutionReason::HostSpawnFailed);
                                }
                                ScheduledRunnerResult::Exited { ref outcome } => {
                                    state.started_at_ms.get_or_insert(now);
                                    state.outcome = Some(outcome.clone());
                                }
                            }
                            Ok(())
                        })?;
                    undo.record(record);
                    Ok(DurableMutation::Changed(
                        Response::ScheduledExecution {
                            execution: snapshot.clone(),
                            next_occurrence: None,
                        },
                        vec![DaemonEventKind::ScheduledExecutionChanged {
                            workspace_id: snapshot.workspace_id.clone(),
                            execution: snapshot,
                        }],
                    ))
                })?;
                if staged_outcome {
                    self.wait_for_native_outcome_barrier();
                }
                Ok(response)
            }
            Request::RegisterAgent {
                shell_id,
                run_id,
                spec,
            } => self.durable_mutation(|undo| {
                let agent = self.register_agent_mutation(undo, &shell_id, &run_id, spec)?;
                let mut events = vec![DaemonEventKind::AgentRegistered {
                    workspace_id: agent.workspace_id.clone(),
                    shell_id: agent.shell_id.clone(),
                    agent: agent.clone(),
                }];
                if agent.ended_at_ms.is_some() {
                    events.push(DaemonEventKind::AgentCompleted {
                        workspace_id: agent.workspace_id.clone(),
                        shell_id: agent.shell_id.clone(),
                        agent: agent.clone(),
                    });
                }
                if let Some((execution, record)) = self.durable.link_agent_execution(&agent)? {
                    undo.record(record);
                    events.push(DaemonEventKind::ScheduledExecutionChanged {
                        workspace_id: execution.workspace_id.clone(),
                        execution,
                    });
                }
                Ok((Response::Agent { agent }, events))
            }),
            Request::EnsureAgent {
                shell_id,
                run_id,
                spec,
            } => self.durable_mutation_outcome(|undo| {
                let (agent, created, record) =
                    self.durable.ensure_agent(&shell_id, &run_id, spec)?;
                if let Some(record) = record {
                    undo.record(record);
                }
                let mut events = Vec::new();
                if created {
                    events.push(DaemonEventKind::AgentRegistered {
                        workspace_id: agent.workspace_id.clone(),
                        shell_id: agent.shell_id.clone(),
                        agent: agent.clone(),
                    });
                    if agent.ended_at_ms.is_some() {
                        events.push(DaemonEventKind::AgentCompleted {
                            workspace_id: agent.workspace_id.clone(),
                            shell_id: agent.shell_id.clone(),
                            agent: agent.clone(),
                        });
                    }
                }
                if let Some((execution, record)) = self.durable.link_agent_execution(&agent)? {
                    undo.record(record);
                    events.push(DaemonEventKind::ScheduledExecutionChanged {
                        workspace_id: execution.workspace_id.clone(),
                        execution,
                    });
                }
                if events.is_empty() {
                    Ok(DurableMutation::Unchanged(Response::Agent { agent }))
                } else {
                    Ok(DurableMutation::Changed(Response::Agent { agent }, events))
                }
            }),
            Request::ReportAgent {
                agent_id,
                run_id,
                report,
            } => self.durable_mutation_outcome(|undo| {
                let (agent, changed, completed) =
                    self.report_agent_mutation(undo, &agent_id, &run_id, report)?;
                if !changed {
                    return Ok(DurableMutation::Unchanged(Response::Agent { agent }));
                }
                let event = if completed {
                    DaemonEventKind::AgentCompleted {
                        workspace_id: agent.workspace_id.clone(),
                        shell_id: agent.shell_id.clone(),
                        agent: agent.clone(),
                    }
                } else {
                    DaemonEventKind::AgentStateChanged {
                        workspace_id: agent.workspace_id.clone(),
                        shell_id: agent.shell_id.clone(),
                        agent: agent.clone(),
                    }
                };
                Ok(DurableMutation::Changed(
                    Response::Agent { agent },
                    vec![event],
                ))
            }),
            Request::ReadShell {
                shell_id,
                max_bytes,
            } => Ok(Response::Output {
                bytes: self.read_shell(&shell_id, max_bytes)?,
            }),
            Request::ReadShellPreview {
                shell_id,
                max_bytes,
                max_lines,
            } => Ok(Response::ShellPreview {
                preview: self.read_shell_preview(&shell_id, max_bytes, max_lines)?,
            }),
            Request::ReadShellAt {
                shell_id,
                max_bytes,
                run_id,
                after_revision,
                wait_ms,
            } => self.read_shell_at(
                &shell_id,
                max_bytes,
                run_id.as_deref(),
                after_revision,
                wait_ms,
            ),
            Request::Events {
                after,
                limit,
                wait_ms,
            } => self.read_events(after.as_ref(), limit, wait_ms),
            Request::RenameWorkspace { workspace_id, name } => {
                self.durable_mutation_outcome(|undo| {
                    Ok(self
                        .rename_workspace_mutation(undo, &workspace_id, name)?
                        .map(|()| Response::Ok))
                })
            }
            Request::GuardedRenameWorkspace {
                workspace_id,
                name,
                expected_revision,
            } => self.durable_mutation_outcome(|undo| {
                let workspace = self.workspace(&workspace_id)?;
                require_guard(*lock(&workspace.revision)?, expected_revision, "workspace")?;
                let mutation = self.rename_workspace_mutation(undo, &workspace_id, name)?;
                let workspace = self.workspace(&workspace_id)?.snapshot(&self.durable)?;
                Ok(mutation.map(|()| Response::Workspace { workspace }))
            }),
            Request::RenameShell { shell_id, name } => self.durable_mutation_outcome(|undo| {
                Ok(self
                    .rename_shell_mutation(undo, &shell_id, name)?
                    .map(|()| Response::Ok))
            }),
            Request::GuardedRenameShell {
                shell_id,
                name,
                expected_revision,
            } => self.durable_mutation_outcome(|undo| {
                let shell = self.shell(&shell_id)?;
                require_guard(*lock(&shell.revision)?, expected_revision, "shell")?;
                let mutation = self.rename_shell_mutation(undo, &shell_id, name)?;
                let shell = self.shell(&shell_id)?.snapshot()?;
                Ok(mutation.map(|()| Response::Shell { shell }))
            }),
            Request::RenameLauncher { launcher_id, name } => {
                self.durable_mutation_outcome(|undo| {
                    Ok(self
                        .rename_launcher_mutation(undo, &launcher_id, name)?
                        .map(|()| Response::Ok))
                })
            }
            Request::GuardedRenameLauncher {
                launcher_id,
                name,
                expected_revision,
            } => self.durable_mutation_outcome(|undo| {
                let launcher = self.launcher(&launcher_id)?;
                require_guard(
                    *lock(&launcher.revision)?,
                    expected_revision,
                    "workspace launcher",
                )?;
                let mutation = self.rename_launcher_mutation(undo, &launcher_id, name)?;
                let launcher = self.launcher(&launcher_id)?.snapshot()?;
                Ok(mutation.map(|()| Response::Launcher { launcher }))
            }),
            Request::CloseWorkspace { workspace_id } => {
                self.close_workspace(&workspace_id)?;
                Ok(Response::Ok)
            }
            Request::GuardedCloseWorkspace {
                workspace_id,
                expected_revision,
            } => {
                self.close_workspace_guarded(&workspace_id, Some(expected_revision))?;
                Ok(Response::Ok)
            }
            Request::CloseShell { shell_id } => {
                self.close_shell(&shell_id)?;
                Ok(Response::Ok)
            }
            Request::GuardedCloseShell {
                shell_id,
                expected_revision,
            } => {
                self.close_shell_guarded(&shell_id, Some(expected_revision))?;
                Ok(Response::Ok)
            }
            Request::RestartShell { shell_id } => Ok(Response::Shell {
                shell: self.restart_shell(&shell_id)?,
            }),
            Request::GuardedRestartShell {
                shell_id,
                expected_revision,
                expected_run_id,
            } => Ok(Response::Shell {
                shell: self.restart_shell_guarded(
                    &shell_id,
                    Some((expected_revision, &expected_run_id)),
                )?,
            }),
            Request::RemoveLauncher { launcher_id } => self.durable_mutation(|undo| {
                let launcher = self.remove_launcher_mutation(undo, &launcher_id)?;
                Ok((
                    Response::Ok,
                    vec![DaemonEventKind::LauncherRemoved {
                        workspace_id: launcher.workspace_id.clone(),
                        launcher_id,
                    }],
                ))
            }),
            Request::GuardedRemoveLauncher {
                launcher_id,
                expected_revision,
            } => self.durable_mutation(|undo| {
                let launcher = self.launcher(&launcher_id)?;
                require_guard(
                    *lock(&launcher.revision)?,
                    expected_revision,
                    "workspace launcher",
                )?;
                let launcher = self.remove_launcher_mutation(undo, &launcher_id)?;
                Ok((
                    Response::Ok,
                    vec![DaemonEventKind::LauncherRemoved {
                        workspace_id: launcher.workspace_id.clone(),
                        launcher_id,
                    }],
                ))
            }),
            Request::PauseAgentSchedule { schedule_id } => {
                self.change_schedule_state(&schedule_id, AgentScheduleState::Paused)
            }
            Request::ResumeAgentSchedule { schedule_id } => {
                self.change_schedule_state(&schedule_id, AgentScheduleState::Enabled)
            }
            Request::GuardedPauseAgentSchedule {
                schedule_id,
                expected_revision,
            } => self.change_schedule_state_guarded(
                &schedule_id,
                AgentScheduleState::Paused,
                Some(expected_revision),
            ),
            Request::GuardedResumeAgentSchedule {
                schedule_id,
                expected_revision,
            } => self.change_schedule_state_guarded(
                &schedule_id,
                AgentScheduleState::Enabled,
                Some(expected_revision),
            ),
            Request::RemoveAgentSchedule { schedule_id } => self.durable_mutation(|undo| {
                let schedule = self.remove_schedule_mutation(undo, &schedule_id)?;
                Ok((
                    Response::Ok,
                    vec![DaemonEventKind::AgentScheduleRemoved {
                        workspace_id: schedule.workspace_id.clone(),
                        schedule_id,
                    }],
                ))
            }),
            Request::GuardedRemoveAgentSchedule {
                schedule_id,
                expected_revision,
            } => self.durable_mutation(|undo| {
                let schedule = self.schedule(&schedule_id)?;
                require_guard(
                    schedule.snapshot()?.revision,
                    expected_revision,
                    "agent schedule",
                )?;
                let schedule = self.remove_schedule_mutation(undo, &schedule_id)?;
                Ok((
                    Response::Ok,
                    vec![DaemonEventKind::AgentScheduleRemoved {
                        workspace_id: schedule.workspace_id.clone(),
                        schedule_id,
                    }],
                ))
            }),
            Request::Attach { .. } | Request::AttachNode { .. } => {
                unreachable!("attach is handled before dispatch")
            }
        }
    }

    fn read_events(
        &self,
        after: Option<&EventCursor>,
        limit: u16,
        wait_ms: u32,
    ) -> DaemonResult<Response> {
        let limit = usize::from(limit.clamp(1, MAX_EVENT_BATCH));
        if after.is_none() {
            let _mutation = lock(&self.mutation_lock)?;
            let published = self.flush_pending()?;
            let transaction = self.events.transaction()?;
            let snapshot = self.snapshot()?;
            let cursor = transaction.cursor();
            let response = Response::Events {
                stream_id: cursor.stream_id.clone(),
                cursor,
                snapshot: Some(snapshot),
                events: Vec::new(),
            };
            drop(transaction);
            if published {
                self.events.notify();
            }
            return Ok(response);
        }
        let after = after.expect("checked above");
        self.events
            .read_after(after, limit, wait_ms, || self.runtimes.is_stopping())
    }

    fn restore(
        store: StateStore,
        live_handoff: bool,
        transferred_events: Option<handoff::EventStreamManifest>,
    ) -> io::Result<Self> {
        let (persisted, migrated) = store.load_deferred()?;
        let persisted = persisted.unwrap_or_default();
        let mut state = DurableState::default();
        let mut workspace_names = HashSet::new();
        let mut run_ids = HashSet::new();
        let mut agent_ids = HashSet::new();
        let mut schedule_ids = HashSet::new();
        let mut execution_ids = HashSet::new();
        let mut linked_agent_ids = HashSet::new();
        let mut recovered_interrupted_run = false;
        let mut cold_recovery_executions = Vec::new();
        let mut cold_recovered_execution_ids = HashSet::new();
        for saved_workspace in persisted.workspaces {
            validate_id("workspace", &saved_workspace.id)?;
            validate_persisted_name(&saved_workspace.name)?;
            if let Some(default_cwd) = &saved_workspace.default_cwd {
                validate_persisted_cwd(default_cwd)?;
            }
            if !workspace_names.insert(saved_workspace.name.clone())
                || state.workspaces.contains_key(&saved_workspace.id)
            {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "Boomux state contains a duplicate workspace",
                ));
            }
            let mut shell_names = HashSet::new();
            let mut shell_ids = Vec::with_capacity(saved_workspace.shells.len());
            for mut saved_shell in saved_workspace.shells {
                validate_id("shell", &saved_shell.id)?;
                validate_persisted_name(&saved_shell.name)?;
                validate_persisted_cwd(&saved_shell.cwd)?;
                if let Some(run) = &mut saved_shell.last_run {
                    validate_id("run", &run.id)?;
                    validate_terminal_profile(&run.profile)?;
                    if run.generation == 0
                        || run.ended_at_ms.is_some() != run.exit_reason.is_some()
                        || run
                            .ended_at_ms
                            .is_some_and(|ended_at_ms| ended_at_ms < run.started_at_ms)
                        || !run_ids.insert(run.id.clone())
                    {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            "Boomux state contains an invalid shell run",
                        ));
                    }
                    if !live_handoff && run.ended_at_ms.is_none() {
                        run.ended_at_ms = Some(unix_time_ms().max(run.started_at_ms));
                        run.exit_reason = Some(ShellRunExitReason::Interrupted);
                        recovered_interrupted_run = true;
                    }
                }
                if !shell_names.insert(saved_shell.name.clone())
                    || state.shells.contains_key(&saved_shell.id)
                {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "Boomux state contains a duplicate shell",
                    ));
                }
                let shell = Arc::new(Shell {
                    id: saved_shell.id,
                    revision: Mutex::new(saved_shell.revision),
                    workspace_id: saved_workspace.id.clone(),
                    name: Mutex::new(saved_shell.name),
                    cwd: saved_shell.cwd,
                    command: saved_shell.command,
                    owner: saved_shell.owner,
                    last_run: Mutex::new(saved_shell.last_run),
                    lifecycle: Mutex::new(ShellLifecycle::Pending),
                    foreground_process_cache: Mutex::new(None),
                });
                shell_ids.push(shell.id.clone());
                state.shells.insert(shell.id.clone(), shell);
            }
            let mut launcher_names = HashSet::new();
            let mut launcher_ids = Vec::with_capacity(saved_workspace.launchers.len());
            for saved_launcher in saved_workspace.launchers {
                validate_id("workspace launcher", &saved_launcher.id)?;
                validate_persisted_name(&saved_launcher.name)?;
                validate_persisted_cwd(&saved_launcher.cwd)?;
                if validate_launcher_command(&saved_launcher.command).is_err() {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "persisted workspace launcher command requires a non-empty executable",
                    ));
                }
                if !launcher_names.insert(saved_launcher.name.clone())
                    || state.launchers.contains_key(&saved_launcher.id)
                {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "Boomux state contains a duplicate workspace launcher",
                    ));
                }
                let launcher = Arc::new(WorkspaceLauncher {
                    id: saved_launcher.id,
                    revision: Mutex::new(saved_launcher.revision),
                    workspace_id: saved_workspace.id.clone(),
                    name: Mutex::new(saved_launcher.name),
                    cwd: saved_launcher.cwd,
                    command: saved_launcher.command,
                });
                launcher_ids.push(launcher.id.clone());
                state.launchers.insert(launcher.id.clone(), launcher);
            }
            let mut workspace_agent_ids = Vec::with_capacity(saved_workspace.agents.len());
            for saved_agent in saved_workspace.agents {
                validate_id("agent", &saved_agent.id)?;
                validate_id("agent shell", &saved_agent.shell_id)?;
                validate_id("agent run", &saved_agent.run_id)?;
                validate_persisted_agent(&saved_agent)?;
                if !agent_ids.insert(saved_agent.id.clone()) {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "Boomux state contains a duplicate agent instance",
                    ));
                }
                let agent = Arc::new(AgentInstance::from_persisted(
                    &saved_workspace.id,
                    saved_agent,
                ));
                workspace_agent_ids.push(agent.id.clone());
                state.agents.insert(agent.id.clone(), agent);
            }
            if saved_workspace.schedules.len() > crate::scheduling::MAX_SCHEDULES_PER_WORKSPACE {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "Boomux state contains too many agent schedules in a workspace",
                ));
            }
            let mut schedule_names = HashSet::new();
            let mut workspace_schedule_ids = Vec::with_capacity(saved_workspace.schedules.len());
            for saved_schedule in saved_workspace.schedules {
                validate_id("agent schedule", &saved_schedule.id)?;
                validate_persisted_schedule(&saved_schedule)?;
                if !schedule_ids.insert(saved_schedule.id.clone())
                    || !schedule_names.insert(saved_schedule.name.clone())
                {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "Boomux state contains a duplicate agent schedule",
                    ));
                }
                let schedule = Arc::new(AgentSchedule::from_persisted(
                    &saved_workspace.id,
                    saved_schedule,
                ));
                if !live_handoff {
                    for execution in lock(&schedule.executions)?.iter() {
                        let mut execution_state = lock(&execution.state)?;
                        if !execution_state.state.is_terminal() {
                            execution_state.state = ScheduledExecutionState::Interrupted;
                            execution_state.revision =
                                execution_state.revision.checked_add(1).ok_or_else(|| {
                                    io::Error::other("scheduled execution revision exhausted")
                                })?;
                            execution_state.ended_at_ms =
                                Some(unix_time_ms().max(execution.requested_at_ms));
                            execution_state.reason =
                                Some(ScheduledExecutionReason::ColdDaemonRecovery);
                            execution_state.outcome = None;
                            recovered_interrupted_run = true;
                            cold_recovered_execution_ids.insert(execution.id.clone());
                        }
                    }
                    let mut executions = lock(&schedule.executions)?;
                    prune_terminal_executions(&mut executions);
                    for execution in executions
                        .iter()
                        .filter(|execution| cold_recovered_execution_ids.contains(&execution.id))
                    {
                        cold_recovery_executions.push(execution.snapshot()?);
                    }
                }
                workspace_schedule_ids.push(schedule.id.clone());
                state.schedules.insert(schedule.id.clone(), schedule);
            }
            for shell_id in &shell_ids {
                let shell = state
                    .shells
                    .get(shell_id)
                    .expect("workspace shell was inserted");
                if let ShellOwner::Schedule { schedule_id } = &shell.owner {
                    let schedule = state.schedules.get(schedule_id).ok_or_else(|| {
                        io::Error::new(
                            io::ErrorKind::InvalidData,
                            "schedule-owned shell references a missing schedule",
                        )
                    })?;
                    if schedule.workspace_id != saved_workspace.id
                        || lock(&schedule.state)?.execution_shell_id.as_deref()
                            != Some(shell.id.as_str())
                    {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            "schedule-owned shell does not match its persisted schedule",
                        ));
                    }
                }
            }
            for schedule_id in &workspace_schedule_ids {
                let schedule = state
                    .schedules
                    .get(schedule_id)
                    .expect("workspace schedule was inserted");
                if let Some(shell_id) = &lock(&schedule.state)?.execution_shell_id {
                    let shell = state.shells.get(shell_id).ok_or_else(|| {
                        io::Error::new(
                            io::ErrorKind::InvalidData,
                            "agent schedule references a missing execution shell",
                        )
                    })?;
                    if shell.owner
                        != (ShellOwner::Schedule {
                            schedule_id: schedule.id.clone(),
                        })
                    {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            "agent schedule references a shell with the wrong owner",
                        ));
                    }
                }
                let schedule_shell_id = lock(&schedule.state)?.execution_shell_id.clone();
                let executions = lock(&schedule.executions)?.clone();
                for execution in executions {
                    if !execution_ids.insert(execution.id.clone()) {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            "Boomux state contains a duplicate scheduled execution",
                        ));
                    }
                    let execution_state = lock(&execution.state)?;
                    if let Some(shell_id) = &execution_state.shell_id
                        && schedule_shell_id.as_deref() != Some(shell_id.as_str())
                    {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            "scheduled execution references a shell outside its schedule",
                        ));
                    }
                    if let Some(agent_id) = &execution_state.agent_id {
                        let agent = state.agents.get(agent_id).ok_or_else(|| {
                            io::Error::new(
                                io::ErrorKind::InvalidData,
                                "scheduled execution references a missing Agent",
                            )
                        })?;
                        if !linked_agent_ids.insert(agent_id.clone())
                            || execution_state.shell_id.as_deref() != Some(agent.shell_id.as_str())
                            || execution_state.run_id.as_deref() != Some(agent.run_id.as_str())
                            || execution.integration != agent.integration
                            || execution_state.external_session_id != agent.external_session_id
                            || matches!(
                                &execution.session,
                                AgentScheduleSession::Continue { external_session_id }
                                    if execution_state.external_session_id.as_deref()
                                        != Some(external_session_id.as_str())
                                        || agent.external_session_id.as_deref()
                                            != Some(external_session_id.as_str())
                            )
                        {
                            return Err(io::Error::new(
                                io::ErrorKind::InvalidData,
                                "scheduled execution has an invalid Agent link",
                            ));
                        }
                    } else if execution_state.external_session_id.is_some() {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            "scheduled execution has a session link without an Agent",
                        ));
                    }
                }
            }
            let workspace = Arc::new(Workspace {
                id: saved_workspace.id.clone(),
                revision: Mutex::new(saved_workspace.revision),
                name: Mutex::new(saved_workspace.name),
                default_cwd: saved_workspace.default_cwd,
                shell_ids: Mutex::new(shell_ids),
                launcher_ids: Mutex::new(launcher_ids),
                agent_ids: Mutex::new(workspace_agent_ids),
                schedule_ids: Mutex::new(workspace_schedule_ids),
            });
            state.workspaces.insert(saved_workspace.id, workspace);
        }
        let store = Arc::new(store);
        let persistence_writer = PersistenceWriter::start(Arc::clone(&store));
        let registry = Self {
            node_identity: None,
            node_registrations: None,
            node_projection_cache: None,
            durable: DurableRegistry {
                state: Mutex::new(state),
                store: Some(store),
                persistence_writer: Some(persistence_writer),
                persist_lock: Mutex::new(()),
                persistence_dirty: AtomicBool::new(migrated),
                persistence_revision: AtomicU64::new(u64::from(migrated)),
            },
            events: EventStream::from_transfer(transferred_events),
            runtimes: ShellRuntimeManager {
                focus: Mutex::new(FocusState::default()),
                stopping: AtomicBool::new(false),
            },
            remote_attachments: RemoteAttachmentManager::default(),
            mutation_lock: Mutex::new(()),
            schedule_dispatch_lock: Mutex::new(()),
            notification_settings: NotificationDeliverySettings::default(),
            notification_sink: Arc::new(DisabledNotificationSink),
            cold_recovery_executions,
            startup_environment: capture_current_environment(),
            scheduler: SchedulerWorker::default(),
            node_projection_workers: NodeProjectionWorkers::default(),
            clock: Mutex::new(Arc::new(SystemSchedulerClock)),
            #[cfg(test)]
            fail_after_mutation: AtomicBool::new(false),
        };
        if recovered_interrupted_run {
            registry.persist()?;
        } else {
            registry.events.initialize_committed_executions(
                registry.durable.scheduled_executions(None, None)?,
            )?;
        }
        Ok(registry)
    }
}

impl ShellRuntimeManager {
    fn import_handoff(
        &self,
        service: Weak<DaemonService>,
        shells: Vec<Arc<Shell>>,
        transferred: Vec<handoff::TransferredRuntime>,
        transferred_exited: Vec<handoff::TransferredExited>,
    ) -> io::Result<(Vec<Arc<ShellRuntime>>, bool)> {
        let shells = shells
            .into_iter()
            .map(|shell| (shell.id.clone(), shell))
            .collect::<HashMap<_, _>>();
        let mut prepared = Vec::with_capacity(transferred.len());
        let mut imported_shell_ids = HashSet::new();
        for transferred in transferred {
            let manifest = transferred.manifest;
            validate_terminal_profile(&manifest.profile)?;
            let shell = shells
                .get(&manifest.shell_id)
                .cloned()
                .ok_or_else(|| not_found("persisted shell", &manifest.shell_id))?;
            imported_shell_ids.insert(shell.id.clone());
            if !matches!(*lock(&shell.lifecycle)?, ShellLifecycle::Pending) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "transferred shell is not pending in restored metadata",
                ));
            }
            let mut saved_run = lock(&shell.last_run)?.clone().ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "transferred shell has no persisted run",
                )
            })?;
            if saved_run.ended_at_ms.is_some()
                || manifest
                    .run_id
                    .as_ref()
                    .is_some_and(|run_id| run_id != &saved_run.id)
            {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "transferred runtime does not match its persisted run",
                ));
            }
            saved_run.profile = manifest.profile.clone();
            if let Some(output_revision) = manifest.output_revision {
                saved_run.output_revision = output_revision;
            }
            let run = Arc::new(ShellRun::from_persisted(&saved_run));
            let stat = fs::read_to_string(format!("/proc/{}/stat", manifest.pid))?;
            if proc_session_id(&stat) != Some(manifest.pid as libc::pid_t) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "transferred process is not its session leader",
                ));
            }
            let process = ImportedProcess {
                pid: manifest.pid,
                pidfd: transferred.pidfd,
            };
            if process.has_exited()? {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "transferred process exited before import",
                ));
            }
            let master = PtyMaster::from_descriptor(transferred.pty)?;
            let reader = master.try_clone_reader()?;
            let mut terminal = TerminalState::new(manifest.profile.rows, manifest.profile.cols);
            terminal.process(&transferred.reconstruction);
            let runtime = Arc::new(ShellRuntime {
                control: Mutex::new(()),
                master: Mutex::new(master),
                process: Mutex::new(ManagedProcess::Imported(process)),
                terminal: Arc::new(Mutex::new(terminal)),
                controller: Mutex::new(None),
                reader: Mutex::new(None),
                output_changed: Condvar::new(),
                output_wait: Mutex::new(()),
            });
            prepared.push((shell, saved_run, run, runtime, reader));
        }
        let mut prepared_exited = Vec::with_capacity(transferred_exited.len());
        for transferred in transferred_exited {
            let manifest = transferred.manifest;
            validate_terminal_profile(&manifest.profile)?;
            let shell = shells
                .get(&manifest.shell_id)
                .cloned()
                .ok_or_else(|| not_found("persisted shell", &manifest.shell_id))?;
            imported_shell_ids.insert(shell.id.clone());
            if !matches!(*lock(&shell.lifecycle)?, ShellLifecycle::Pending) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "transferred exited shell is not pending in restored metadata",
                ));
            }
            let mut saved_run = lock(&shell.last_run)?.clone().ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "transferred exited shell has no persisted run",
                )
            })?;
            if saved_run.id != manifest.run_id
                || saved_run.exit_reason
                    != Some(ShellRunExitReason::Exited {
                        code: manifest.code,
                    })
            {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "transferred exited shell does not match its persisted run",
                ));
            }
            saved_run.profile = manifest.profile.clone();
            saved_run.output_revision = manifest.output_revision;
            let run = Arc::new(ShellRun::from_persisted(&saved_run));
            let mut terminal = TerminalState::new(manifest.profile.rows, manifest.profile.cols);
            terminal.process(&transferred.reconstruction);
            prepared_exited.push((
                shell,
                saved_run,
                run,
                Arc::new(Mutex::new(terminal)),
                manifest,
            ));
        }
        let mut interrupted_untransferred_run = false;
        for shell in shells.values() {
            if imported_shell_ids.contains(&shell.id) {
                continue;
            }
            let mut last_run = lock(&shell.last_run)?;
            if let Some(last_run) = last_run.as_mut()
                && last_run.ended_at_ms.is_none()
            {
                last_run.ended_at_ms = Some(unix_time_ms().max(last_run.started_at_ms));
                last_run.exit_reason = Some(ShellRunExitReason::Interrupted);
                interrupted_untransferred_run = true;
            }
        }
        let mut readers = Vec::with_capacity(prepared.len());
        for (shell, saved_run, run, runtime, reader) in prepared {
            let profile = saved_run.profile.clone();
            *lock(&shell.last_run)? = Some(saved_run);
            *lock(&shell.lifecycle)? = ShellLifecycle::Running {
                profile,
                run: Arc::clone(&run),
                runtime: Arc::clone(&runtime),
            };
            self.start_pty_reader(
                service.clone(),
                shell,
                Arc::clone(&run),
                Arc::clone(&runtime),
                reader,
                true,
            )?;
            readers.push(runtime);
        }
        for (shell, saved_run, run, terminal, manifest) in prepared_exited {
            *lock(&shell.last_run)? = Some(saved_run);
            *lock(&shell.lifecycle)? = ShellLifecycle::Exited {
                code: manifest.code,
                profile: manifest.profile,
                run,
                runtime: None,
                terminal,
            };
        }
        let changed =
            interrupted_untransferred_run || !readers.is_empty() || !imported_shell_ids.is_empty();
        Ok((readers, changed))
    }
}

impl DaemonService {
    fn durable_mutation<T>(
        &self,
        operation: impl FnOnce(&mut DurableUndoLog) -> DaemonResult<(T, Vec<DaemonEventKind>)>,
    ) -> DaemonResult<T> {
        self.durable_mutation_outcome(|undo| {
            let (value, events) = operation(undo)?;
            Ok(DurableMutation::Changed(value, events))
        })
    }

    fn durable_mutation_outcome<T>(
        &self,
        operation: impl FnOnce(&mut DurableUndoLog) -> DaemonResult<DurableMutation<T>>,
    ) -> DaemonResult<T> {
        let mutation = lock(&self.mutation_lock)?;
        self.ensure_running()?;
        self.flush_pending()?;
        let persistence = lock(&self.durable.persist_lock)?;
        let mut transaction = self.events.transaction()?;
        let mut undo = DurableUndoLog::default();
        let outcome = operation(&mut undo);
        #[cfg(test)]
        let outcome = if !undo.is_empty() && self.fail_after_mutation.swap(false, Ordering::AcqRel)
        {
            Err(io::Error::other("injected post-mutation failure").into())
        } else {
            outcome
        };
        match outcome {
            Ok(DurableMutation::Unchanged(value)) if undo.is_empty() => Ok(value),
            Ok(DurableMutation::Unchanged(_)) => {
                let error = io::Error::other("unchanged durable mutation recorded undo");
                Err(Self::mutation_failure(
                    error.into(),
                    undo.rollback(&self.durable),
                ))
            }
            Ok(DurableMutation::Changed(value, kinds)) if !undo.is_empty() => {
                if let Err(error) = transaction.reserve_with_pending(kinds.len()) {
                    return Err(Self::mutation_failure(
                        error.into(),
                        undo.rollback(&self.durable),
                    ));
                }
                let notifications = self.notification_requests(
                    &kinds,
                    &undo,
                    &transaction.events.committed_executions,
                );
                let saved = match self.capture_persisted_state() {
                    Ok(saved) => saved,
                    Err(error) => {
                        return Err(Self::mutation_failure(
                            error.into(),
                            undo.rollback(&self.durable),
                        ));
                    }
                };
                transaction.begin_persistence(kinds.len());
                drop(transaction);
                match self
                    .write_persisted_state(saved)
                    .map_err(DaemonError::persistence)
                {
                    Ok(committed) => {
                        let mut transaction = self.events.transaction()?;
                        transaction.replace_committed_executions(committed);
                        transaction.append_batch(kinds);
                        transaction.finish_persistence();
                        drop(transaction);
                        drop(persistence);
                        self.events.notify();
                        drop(mutation);
                        for notification in notifications {
                            self.notification_sink.notify(notification);
                        }
                        Ok(value)
                    }
                    Err(error) => {
                        let error = Self::mutation_failure(error, undo.rollback(&self.durable));
                        let mut transaction = self.events.transaction()?;
                        transaction.finish_persistence();
                        drop(transaction);
                        self.events.notify();
                        Err(error)
                    }
                }
            }
            Ok(DurableMutation::Changed(_, _)) => {
                Err(io::Error::other("changed durable mutation did not record undo").into())
            }
            Err(error) => Err(Self::mutation_failure(error, undo.rollback(&self.durable))),
        }
    }

    fn mutation_failure(primary: DaemonError, rollback: io::Result<()>) -> DaemonError {
        match rollback {
            Ok(()) => primary,
            Err(rollback) => Self::append_error_context(
                primary,
                format!("durable rollback also failed: {rollback}"),
            ),
        }
    }

    fn append_error_context(primary: DaemonError, context: String) -> DaemonError {
        match primary {
            DaemonError::Validation(source) => DaemonError::Validation(io::Error::new(
                source.kind(),
                format!("{source}; {context}"),
            )),
            DaemonError::Lifecycle { code, source } => DaemonError::Lifecycle {
                code,
                source: io::Error::new(source.kind(), format!("{source}; {context}")),
            },
            DaemonError::Persistence { message, source } => DaemonError::Persistence {
                message: format!("{message}; {context}"),
                source,
            },
            DaemonError::Protocol(source) => DaemonError::Protocol(io::Error::new(
                source.kind(),
                format!("{source}; {context}"),
            )),
            DaemonError::Internal(source) => DaemonError::Internal(io::Error::new(
                source.kind(),
                format!("{source}; {context}"),
            )),
        }
    }

    fn notification_requests(
        &self,
        kinds: &[DaemonEventKind],
        undo: &DurableUndoLog,
        committed_executions: &HashMap<String, ScheduledExecutionSnapshot>,
    ) -> Vec<NotificationRequest> {
        let mut seen = HashSet::new();
        let mut requests = Vec::new();
        for kind in kinds {
            let (workspace_id, shell_id, agent) = match kind {
                DaemonEventKind::AgentRegistered {
                    workspace_id,
                    shell_id,
                    agent,
                }
                | DaemonEventKind::AgentStateChanged {
                    workspace_id,
                    shell_id,
                    agent,
                }
                | DaemonEventKind::AgentCompleted {
                    workspace_id,
                    shell_id,
                    agent,
                } => (workspace_id, shell_id, agent),
                _ => continue,
            };
            let previous_state = undo.previous_agent_state(&agent.id);
            if previous_state == Some(agent.observation.state) {
                continue;
            }
            let current_attention = agent
                .attention
                .as_ref()
                .filter(|attention| attention.observation.revision == agent.observation.revision);
            let reason = match agent.observation.state {
                AgentState::Blocked
                    if current_attention.is_some_and(|attention| {
                        attention.reason == AgentAttentionReason::Blocked
                    }) =>
                {
                    NotificationReason::Blocked
                }
                AgentState::Done
                    if current_attention.is_some_and(|attention| {
                        attention.reason == AgentAttentionReason::Completed
                    }) =>
                {
                    NotificationReason::Completed
                }
                AgentState::Idle if previous_state == Some(AgentState::Working) => {
                    NotificationReason::Completed
                }
                _ => continue,
            };
            if !category_enabled(&self.notification_settings, reason)
                || !seen.insert((agent.id.clone(), agent.observation.revision, reason))
            {
                continue;
            }
            let (workspace, shell) = self.notification_context(workspace_id, shell_id);
            requests.push(NotificationRequest {
                reason,
                agent: agent.name.clone(),
                workspace,
                shell,
            });
        }
        requests.extend(self.execution_notification_requests(kinds, committed_executions));
        requests
    }

    fn execution_notification_requests(
        &self,
        kinds: &[DaemonEventKind],
        committed_executions: &HashMap<String, ScheduledExecutionSnapshot>,
    ) -> Vec<NotificationRequest> {
        let mut seen = HashSet::new();
        let mut published = HashMap::new();
        let mut requests = Vec::new();
        for kind in kinds {
            let DaemonEventKind::ScheduledExecutionChanged {
                workspace_id,
                execution,
            } = kind
            else {
                continue;
            };
            let previous = published
                .get(&execution.id)
                .or_else(|| committed_executions.get(&execution.id));
            let reason = Self::execution_notification_reason(execution);
            let transitioned = reason.is_some()
                && previous
                    .and_then(Self::execution_notification_reason)
                    .is_none();
            published.insert(execution.id.clone(), execution.clone());
            let Some(reason) = reason.filter(|_| transitioned) else {
                continue;
            };
            if !category_enabled(&self.notification_settings, reason)
                || !seen.insert((execution.id.clone(), execution.revision, reason))
            {
                continue;
            }
            let (workspace, schedule) = self
                .durable
                .schedule_notification_context(workspace_id, &execution.schedule_id);
            requests.push(NotificationRequest {
                reason,
                agent: schedule,
                workspace,
                shell: execution.id.clone(),
            });
        }
        requests
    }

    fn execution_notification_reason(
        execution: &ScheduledExecutionSnapshot,
    ) -> Option<NotificationReason> {
        match (execution.state, execution.reason) {
            (
                ScheduledExecutionState::DispatchFailed,
                Some(
                    ScheduledExecutionReason::RunnerStartFailed
                    | ScheduledExecutionReason::HostSpawnFailed,
                ),
            ) => Some(NotificationReason::ScheduledDispatchFailed),
            (
                ScheduledExecutionState::Interrupted,
                Some(ScheduledExecutionReason::ColdDaemonRecovery),
            ) => Some(NotificationReason::ScheduledInterrupted),
            _ => None,
        }
    }

    fn publish_cold_recovery_notifications(&mut self) {
        if !category_enabled(
            &self.notification_settings,
            NotificationReason::ScheduledInterrupted,
        ) {
            self.cold_recovery_executions.clear();
            return;
        }
        let mut seen = HashSet::new();
        for execution in self.cold_recovery_executions.drain(..) {
            if !seen.insert((execution.id.clone(), execution.revision)) {
                continue;
            }
            let (workspace, schedule) = self
                .durable
                .schedule_notification_context(&execution.workspace_id, &execution.schedule_id);
            self.notification_sink.notify(NotificationRequest {
                reason: NotificationReason::ScheduledInterrupted,
                agent: schedule,
                workspace,
                shell: execution.id,
            });
        }
    }

    fn notification_context(&self, workspace_id: &str, shell_id: &str) -> (String, String) {
        self.durable.notification_context(workspace_id, shell_id)
    }

    fn flush_pending(&self) -> DaemonResult<bool> {
        let _persistence = lock(&self.durable.persist_lock)?;
        let mut transaction = self.events.transaction()?;
        if transaction.pending_durable_batch_count() == 0
            && !self.durable.persistence_dirty.load(Ordering::Acquire)
        {
            return Ok(false);
        }
        let pending_count = transaction.pending_durable_batch_count();
        let count = transaction.pending_durable_event_count(pending_count);
        transaction.reserve_with_pending(0)?;
        let saved = self.capture_persisted_state()?;
        let committed_before = transaction.events.committed_executions.clone();
        let pending = transaction.take_pending_durable(pending_count);
        transaction.begin_persistence(count);
        drop(transaction);
        let committed = match self
            .write_persisted_state(saved)
            .map_err(DaemonError::persistence)
        {
            Ok(committed) => committed,
            Err(error) => {
                let mut transaction = self.events.transaction()?;
                transaction.restore_pending_durable(pending);
                transaction.finish_persistence();
                return Err(error);
            }
        };
        let notification_events = pending.iter().flatten().cloned().collect::<Vec<_>>();
        let notifications =
            self.execution_notification_requests(&notification_events, &committed_before);
        let mut transaction = self.events.transaction()?;
        transaction.reserve_with_pending(0)?;
        transaction.replace_committed_executions(committed);
        for batch in pending {
            transaction.append_batch(batch);
        }
        transaction.finish_persistence();
        drop(transaction);
        self.events.notify();
        for notification in notifications {
            self.notification_sink.notify(notification);
        }
        Ok(count != 0)
    }

    fn compensate_stopped_locked<T: EventTransitionAccess>(
        &self,
        shells: &[Arc<Shell>],
        transition: &mut T,
    ) -> io::Result<()> {
        let mut batch = transition.drain_runtime_events();
        let mut failure = None;
        for shell in shells {
            match self.runtimes.compensate_stopped(shell) {
                Ok(Some(event)) => batch.push(event),
                Ok(None) => {}
                Err(error) if failure.is_none() => failure = Some(error),
                Err(_) => {}
            }
            if let Ok(Some(event)) = self.reconcile_irreversibly_stopped_execution(shell) {
                batch.push(event);
            }
        }
        self.queue_durable_batch_locked(batch, transition);
        failure.map_or(Ok(()), Err)
    }

    fn restore_finalized_locked<T: EventTransitionAccess>(
        &self,
        finalized: Vec<(Arc<Shell>, StopRollback)>,
        transition: &mut T,
    ) -> io::Result<()> {
        let mut batch = transition.drain_runtime_events();
        let mut failure = None;
        for (shell, rollback) in finalized {
            if let Some(event) = rollback.event.clone() {
                batch.push(event);
            }
            if let Err(error) = self.runtimes.restore_stopped(&shell, rollback)
                && failure.is_none()
            {
                failure = Some(error);
            }
            if let Ok(Some(event)) = self.reconcile_irreversibly_stopped_execution(&shell) {
                batch.push(event);
            }
        }
        self.queue_durable_batch_locked(batch, transition);
        failure.map_or(Ok(()), Err)
    }

    fn reconcile_irreversibly_stopped_execution(
        &self,
        shell: &Arc<Shell>,
    ) -> io::Result<Option<DaemonEventKind>> {
        let ShellOwner::Schedule { schedule_id } = &shell.owner else {
            return Ok(None);
        };
        let run_id = lock(&shell.last_run)?.as_ref().map(|run| run.id.clone());
        let Some(run_id) = run_id else {
            return Ok(None);
        };
        let schedule = self.schedule(schedule_id)?;
        let executions = lock(&schedule.executions)?.clone();
        let execution = executions.into_iter().find(|execution| {
            lock(&execution.state).is_ok_and(|state| {
                matches!(
                    state.state,
                    ScheduledExecutionState::Starting | ScheduledExecutionState::Active
                ) && state.shell_id.as_deref() == Some(shell.id.as_str())
                    && state.run_id.as_deref() == Some(run_id.as_str())
            })
        });
        let Some(execution) = execution else {
            return Ok(None);
        };
        let now = unix_time_ms().max(execution.requested_at_ms);
        let (snapshot, _) = self.durable.mutate_execution(&execution, |state| {
            state.state = ScheduledExecutionState::Interrupted;
            state.ended_at_ms = Some(now);
            state.reason = Some(ScheduledExecutionReason::RunnerExitedWithoutReport);
            state.outcome = None;
            Ok(())
        })?;
        Ok(Some(DaemonEventKind::ScheduledExecutionChanged {
            workspace_id: snapshot.workspace_id.clone(),
            execution: snapshot,
        }))
    }

    fn lifecycle_failure(primary: DaemonError, compensation: io::Result<()>) -> DaemonError {
        match compensation {
            Ok(()) => primary,
            Err(compensation) => Self::append_error_context(
                primary,
                format!("lifecycle compensation also failed: {compensation}"),
            ),
        }
    }

    fn queue_durable_batch_locked<T: EventTransitionAccess>(
        &self,
        batch: Vec<DaemonEventKind>,
        transition: &mut T,
    ) {
        if transition.queue_durable_batch(batch) {
            self.mark_persistence_dirty();
        }
    }

    fn ensure_running(&self) -> io::Result<()> {
        if self.runtimes.is_stopping() {
            Err(io::Error::new(
                io::ErrorKind::ConnectionAborted,
                "Boomux daemon is stopping",
            ))
        } else {
            Ok(())
        }
    }

    fn notify_output_waiters(&self) {
        if let Ok(shells) = self.durable.shells() {
            self.runtimes.notify_output_waiters(&shells);
        }
    }

    fn state_lock_descriptor(&self) -> io::Result<BorrowedFd<'_>> {
        self.durable.state_lock_descriptor()
    }

    fn persist(&self) -> io::Result<()> {
        let _persist = lock(&self.durable.persist_lock)?;
        let saved = self.durable.capture_persisted_state()?;
        let executions = self.write_persisted_state(saved)?;
        self.events.initialize_committed_executions(executions)
    }

    fn capture_persisted_state(&self) -> io::Result<PersistenceGeneration> {
        self.durable.capture_persisted_state()
    }

    fn write_persisted_state(
        &self,
        generation: PersistenceGeneration,
    ) -> io::Result<Vec<ScheduledExecutionSnapshot>> {
        let executions = generation.executions.clone();
        self.durable.write_persisted_state(generation)?;
        Ok(executions)
    }

    #[cfg(test)]
    fn fail_next_persistence(&self) {
        self.durable
            .persistence_writer
            .as_ref()
            .expect("test registry has persistence")
            .fail_next();
    }

    #[cfg(test)]
    fn fail_after_next_mutation(&self) {
        self.fail_after_mutation.store(true, Ordering::Release);
    }

    fn mark_persistence_dirty(&self) {
        self.durable.mark_persistence_dirty();
    }

    fn try_record_run_exit(
        &self,
        shell: &Arc<Shell>,
        run: &Arc<ShellRun>,
        runtime: &Arc<ShellRuntime>,
        code: Option<u32>,
    ) -> io::Result<RunExitRecord> {
        let _mutation = match self.mutation_lock.try_lock() {
            Ok(mutation) => mutation,
            Err(TryLockError::WouldBlock) => return Ok(RunExitRecord::Deferred),
            Err(TryLockError::Poisoned(_)) => {
                return Err(io::Error::other("daemon mutation lock poisoned"));
            }
        };
        if !self.runtimes.run_exit_is_current(shell, run, runtime)? {
            return Ok(RunExitRecord::Unchanged);
        }
        let _persistence = match self.durable.persist_lock.try_lock() {
            Ok(persistence) => persistence,
            Err(TryLockError::WouldBlock)
                if self.events.lifecycle_active.load(Ordering::Acquire) =>
            {
                return Ok(RunExitRecord::Deferred);
            }
            Err(TryLockError::WouldBlock) => lock(&self.durable.persist_lock)?,
            Err(TryLockError::Poisoned(_)) => {
                return Err(io::Error::other("daemon state lock poisoned"));
            }
        };
        let execution = if let ShellOwner::Schedule { schedule_id } = &shell.owner {
            let schedule = self.schedule(schedule_id)?;
            let executions = lock(&schedule.executions)?.clone();
            executions.into_iter().find(|execution| {
                lock(&execution.state).is_ok_and(|state| {
                    matches!(
                        state.state,
                        ScheduledExecutionState::Starting | ScheduledExecutionState::Active
                    ) && state.shell_id.as_deref() == Some(shell.id.as_str())
                        && state.run_id.as_deref() == Some(run.id.as_str())
                })
            })
        } else {
            None
        };
        let reserve = 1 + usize::from(execution.is_some());
        let mut transaction = self.events.transaction()?;
        if let Err(error) = transaction.reserve_with_pending(reserve) {
            if transaction.capacity_is_blocked_only_by_lifecycle_reservation(reserve) {
                return Ok(RunExitRecord::Deferred);
            }
            return Err(error);
        }
        let Some(exit_event) = self.runtimes.finalize_run_exit(shell, run, runtime, code)? else {
            return Ok(RunExitRecord::Unchanged);
        };
        let mut batch = transaction.drain_runtime_events();
        batch.push(exit_event);
        if let Some(execution) = execution {
            let now = unix_time_ms().max(execution.requested_at_ms);
            let (snapshot, _) = self.durable.mutate_execution(&execution, |state| {
                if matches!(
                    state.state,
                    ScheduledExecutionState::Starting | ScheduledExecutionState::Active
                ) {
                    state.ended_at_ms = Some(now);
                    if state.outcome.is_some() {
                        state.state = ScheduledExecutionState::Exited;
                        state.reason = None;
                    } else if state.reason == Some(ScheduledExecutionReason::HostSpawnFailed) {
                        state.state = ScheduledExecutionState::DispatchFailed;
                    } else {
                        state.state = ScheduledExecutionState::Interrupted;
                        state.reason = Some(ScheduledExecutionReason::RunnerExitedWithoutReport);
                    }
                }
                Ok(())
            })?;
            batch.push(DaemonEventKind::ScheduledExecutionChanged {
                workspace_id: snapshot.workspace_id.clone(),
                execution: snapshot,
            });
        }
        let pending_count =
            transaction.pending_durable_event_count(transaction.pending_durable_batch_count());
        let event_count = pending_count.saturating_add(batch.len());
        let mut notification_events = transaction.pending_durable_events();
        notification_events.extend(batch.iter().cloned());
        let notifications = self.execution_notification_requests(
            &notification_events,
            &transaction.events.committed_executions,
        );
        let saved = self.capture_persisted_state()?;
        transaction.begin_persistence(event_count);
        drop(transaction);
        match self.write_persisted_state(saved) {
            Ok(committed) => {
                let mut transaction = self.events.transaction()?;
                transaction.replace_committed_executions(committed);
                transaction.append_pending_durable();
                transaction.append_batch(batch);
                transaction.finish_persistence();
                drop(transaction);
                self.events.notify();
                self.wake_scheduler();
                drop(_persistence);
                drop(_mutation);
                for notification in notifications {
                    self.notification_sink.notify(notification);
                }
                Ok(RunExitRecord::Recorded)
            }
            Err(error) => {
                let mut transaction = self.events.transaction()?;
                transaction.queue_durable_batch(batch);
                transaction.finish_persistence();
                drop(transaction);
                self.events.notify();
                self.wake_scheduler();
                Err(error)
            }
        }
    }

    fn defer_run_exit(
        registry: Weak<Self>,
        shell: Arc<Shell>,
        run: Arc<ShellRun>,
        runtime: Arc<ShellRuntime>,
        code: Option<u32>,
    ) -> io::Result<()> {
        let shell_id = shell.id.clone();
        thread::Builder::new()
            .name(format!("boomux-exit-retry-{shell_id}"))
            .spawn(move || {
                loop {
                    let Some(service) = registry.upgrade() else {
                        return;
                    };
                    match service.try_record_run_exit(&shell, &run, &runtime, code) {
                        Ok(RunExitRecord::Recorded | RunExitRecord::Unchanged) => return,
                        Ok(RunExitRecord::Deferred) => {
                            drop(service);
                            thread::sleep(IO_RETRY_DELAY);
                        }
                        Err(error) => {
                            eprintln!("boomux: could not persist shell run exit: {error}");
                            return;
                        }
                    }
                }
            })
            .map(|_| ())
    }

    fn snapshot(&self) -> io::Result<Snapshot> {
        let mut snapshot = self.durable.snapshot(self.focused_terminal()?)?;
        let active = self.scheduler.state.lock().ok().is_some_and(|scheduler| {
            scheduler.running
                && scheduler.healthy
                && scheduler
                    .handle
                    .as_ref()
                    .is_some_and(|handle| !handle.is_finished())
        });
        snapshot.scheduler = Some(SchedulerHealth {
            state: if active {
                SchedulerState::Active
            } else {
                SchedulerState::Offline
            },
            max_concurrent: self
                .notification_settings
                .max_scheduled_execution_concurrency,
            active_executions: u16::try_from(self.durable.active_scheduled_execution_count()?)
                .unwrap_or(u16::MAX),
        });
        Ok(snapshot)
    }

    fn node_projection_sync(
        &self,
        after: Option<EventCursor>,
        wait_ms: u32,
    ) -> DaemonResult<NodeProjectionSync> {
        if let Some(cursor) = &after
            && wait_ms != 0
        {
            match self
                .events
                .read_after(cursor, 1, wait_ms, || self.runtimes.is_stopping())
            {
                Ok(_) => {}
                Err(DaemonError::Lifecycle {
                    code: ErrorCode::CursorExpired,
                    ..
                }) => {}
                Err(error) => return Err(error),
            }
        }
        let transaction = self.events.transaction()?;
        let cursor = transaction.cursor();
        let (mode, transitions) =
            projection_transitions(&transaction.events, after.as_ref(), &cursor);
        let node_id = self.node_identity()?.id()?;
        let scheduler = self.scheduler_health()?;
        let projection = self.durable.node_projection(node_id, scheduler)?;
        Ok(NodeProjectionSync {
            mode,
            cursor,
            projection,
            transitions,
        })
    }

    fn scheduler_health(&self) -> io::Result<SchedulerHealth> {
        let active = self.scheduler.state.lock().ok().is_some_and(|scheduler| {
            scheduler.running
                && scheduler.healthy
                && scheduler
                    .handle
                    .as_ref()
                    .is_some_and(|handle| !handle.is_finished())
        });
        Ok(SchedulerHealth {
            state: if active {
                SchedulerState::Active
            } else {
                SchedulerState::Offline
            },
            max_concurrent: self
                .notification_settings
                .max_scheduled_execution_concurrency,
            active_executions: u16::try_from(self.durable.active_scheduled_execution_count()?)
                .unwrap_or(u16::MAX),
        })
    }

    fn schedule_shell_ids_for_downgrade(&self) -> io::Result<HashSet<String>> {
        let mut ids = self.events.schedule_shell_ids()?;
        ids.extend(self.durable.schedule_shell_ids()?);
        Ok(ids)
    }

    fn focused_terminal(&self) -> io::Result<Option<FocusedTerminalSnapshot>> {
        let focused = self.runtimes.focused_terminal()?;
        let Some(focused) = focused else {
            return Ok(None);
        };
        Ok(self.focus_target_is_current(&focused)?.then_some(focused))
    }

    fn focused_terminal_for_handoff(&self) -> io::Result<Option<FocusedTerminalSnapshot>> {
        self.runtimes.focused_terminal()
    }

    fn focus_target_is_current(&self, focused: &FocusedTerminalSnapshot) -> io::Result<bool> {
        let Ok(shell) = self.durable.shell(&focused.shell_id) else {
            return Ok(false);
        };
        if shell.workspace_id != focused.workspace_id {
            return Ok(false);
        }
        let lifecycle = lock(&shell.lifecycle)?;
        let current = match &*lifecycle {
            ShellLifecycle::Running { run, .. } | ShellLifecycle::Exited { run, .. } => {
                run.id == focused.run_id
            }
            ShellLifecycle::Pending | ShellLifecycle::Closed => return Ok(false),
        };
        drop(lifecycle);
        Ok(current && self.durable.contains_shell(&shell)?)
    }

    fn record_focus_gained(
        &self,
        protocol_version: u32,
        shell: &Arc<Shell>,
        run: &Arc<ShellRun>,
        runtime: &Arc<ShellRuntime>,
        token: &str,
    ) -> io::Result<bool> {
        if !protocol::ProtocolFeature::FocusedTerminal.is_supported_by(protocol_version) {
            let minimum_version = protocol::ProtocolFeature::FocusedTerminal.minimum_version();
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("focus frames require daemon protocol {minimum_version}"),
            ));
        }
        let _mutation = lock(&self.mutation_lock)?;
        self.ensure_running()?;
        if !self.durable.contains_shell(shell)? {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "focus frame targets a stale shell",
            ));
        }
        let lifecycle = lock(&shell.lifecycle)?;
        let ShellLifecycle::Running {
            run: current_run,
            runtime: current_runtime,
            ..
        } = &*lifecycle
        else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "focus frame targets a shell that is not running",
            ));
        };
        if !Arc::ptr_eq(current_run, run) || !Arc::ptr_eq(current_runtime, runtime) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "focus frame targets a stale shell run",
            ));
        }
        let run_id = current_run.id.clone();
        drop(lifecycle);
        let authorized = lock(&runtime.controller)?
            .as_ref()
            .is_some_and(|controller| controller.token == token);
        if !authorized {
            return Ok(false);
        }
        self.runtimes
            .record_focus_gained(shell.workspace_id.clone(), shell.id.clone(), run_id)?;
        Ok(true)
    }

    fn import_focused_terminal(
        &self,
        focused_terminal: Option<FocusedTerminalSnapshot>,
    ) -> io::Result<()> {
        let Some(focused_terminal) = focused_terminal else {
            return Ok(());
        };
        if focused_terminal.revision == 0 {
            return Ok(());
        }
        let current = self.focus_target_is_current(&focused_terminal)?;
        self.runtimes
            .import_focused_terminal(focused_terminal, current)
    }

    fn lifecycle_transaction(
        &self,
        stopping: bool,
        select_shells: impl FnOnce() -> DaemonResult<Vec<Arc<Shell>>>,
        durable_apply: impl FnOnce(&mut DurableUndoLog) -> DaemonResult<()>,
        committed_events: impl FnOnce(&[Arc<Shell>]) -> Vec<DaemonEventKind>,
    ) -> DaemonResult<()> {
        let _mutation = lock(&self.mutation_lock)?;
        if stopping {
            if !self.runtimes.begin_stopping() {
                return Ok(());
            }
            self.events.notify();
            self.notify_output_waiters();
        } else {
            self.ensure_running()?;
        }
        let shells = match select_shells() {
            Ok(shells) => shells,
            Err(error) => {
                if stopping {
                    self.runtimes.cancel_stopping();
                }
                return Err(error);
            }
        };
        let committed_events = committed_events(&shells);
        let lifecycle_event_capacity = committed_events.len().max(shells.len());
        let mut transaction = match self.events.transaction() {
            Ok(transaction) => transaction,
            Err(error) => {
                if stopping {
                    self.runtimes.cancel_stopping();
                }
                return Err(error.into());
            }
        };
        if let Err(error) = transaction.reserve_with_pending(lifecycle_event_capacity) {
            if stopping {
                self.runtimes.cancel_stopping();
            }
            return Err(error.into());
        }
        transaction.begin_lifecycle_reservation(lifecycle_event_capacity);
        drop(transaction);
        let _lifecycle_activity = LifecycleActivity::begin(&self.events.lifecycle_active);
        let mut stopped: Vec<Arc<Shell>> = Vec::with_capacity(shells.len());
        for shell in &shells {
            if let Err(error) = self.runtimes.stop_runtime(shell) {
                if error.stopped {
                    stopped.push(Arc::clone(shell));
                }
                let mut transaction = self.events.transaction()?;
                let compensation = self.compensate_stopped_locked(&stopped, &mut transaction);
                transaction.release_lifecycle_reservation();
                if stopping {
                    self.runtimes.cancel_stopping();
                }
                return Err(Self::lifecycle_failure(error.source.into(), compensation));
            }
            stopped.push(Arc::clone(shell));
        }
        if let Err(error) = self.flush_pending() {
            let mut transaction = self.events.transaction()?;
            let compensation = self.compensate_stopped_locked(&stopped, &mut transaction);
            transaction.release_lifecycle_reservation();
            if stopping {
                self.runtimes.cancel_stopping();
            }
            return Err(Self::lifecycle_failure(error, compensation));
        }
        let _persistence = match lock(&self.durable.persist_lock) {
            Ok(persistence) => persistence,
            Err(error) => {
                let mut transaction = self.events.transaction()?;
                let compensation = self.compensate_stopped_locked(&stopped, &mut transaction);
                transaction.release_lifecycle_reservation();
                if stopping {
                    self.runtimes.cancel_stopping();
                }
                return Err(Self::lifecycle_failure(error.into(), compensation));
            }
        };
        let mut transaction = self.events.transaction()?;
        let mut rollbacks = Vec::with_capacity(shells.len());
        for (index, shell) in shells.iter().enumerate() {
            match self.runtimes.finalize_stop(shell) {
                Ok(rollback) => rollbacks.push((Arc::clone(shell), rollback)),
                Err(error) => {
                    let mut compensation =
                        self.restore_finalized_locked(rollbacks, &mut transaction);
                    if let Err(remaining) =
                        self.compensate_stopped_locked(&shells[index..], &mut transaction)
                        && compensation.is_ok()
                    {
                        compensation = Err(remaining);
                    }
                    if stopping {
                        self.runtimes.cancel_stopping();
                    }
                    transaction.release_lifecycle_reservation();
                    return Err(Self::lifecycle_failure(error.into(), compensation));
                }
            }
        }
        let mut undo = DurableUndoLog::default();
        if let Err(error) = durable_apply(&mut undo) {
            let durable = undo.rollback(&self.durable);
            let compensation = self.restore_finalized_locked(rollbacks, &mut transaction);
            if stopping {
                self.runtimes.cancel_stopping();
            }
            transaction.release_lifecycle_reservation();
            return Err(Self::lifecycle_failure(
                Self::lifecycle_failure(error, durable),
                compensation,
            ));
        }
        let saved = match self.capture_persisted_state() {
            Ok(saved) => saved,
            Err(error) => {
                let durable = undo.rollback(&self.durable);
                let runtime = self.restore_finalized_locked(rollbacks, &mut transaction);
                if stopping {
                    self.runtimes.cancel_stopping();
                }
                transaction.release_lifecycle_reservation();
                return Err(Self::lifecycle_failure(
                    Self::lifecycle_failure(error.into(), durable),
                    runtime,
                ));
            }
        };
        transaction.transfer_lifecycle_reservation_to_persistence(
            committed_events.len(),
            lifecycle_event_capacity.saturating_sub(committed_events.len()),
        );
        drop(transaction);
        let committed = match self
            .write_persisted_state(saved)
            .map_err(DaemonError::persistence)
        {
            Ok(committed) => committed,
            Err(error) => {
                let mut transaction = self.events.transaction()?;
                let durable = undo.rollback(&self.durable);
                let runtime = self.restore_finalized_locked(rollbacks, &mut transaction);
                transaction.release_lifecycle_reservation();
                transaction.finish_persistence();
                if stopping {
                    self.runtimes.cancel_stopping();
                }
                return Err(Self::lifecycle_failure(
                    Self::lifecycle_failure(error, durable),
                    runtime,
                ));
            }
        };
        let mut transaction = self.events.transaction()?;
        transaction.replace_committed_executions(committed);
        transaction.append_batch(committed_events);
        transaction.release_lifecycle_reservation();
        transaction.finish_persistence();
        drop(transaction);
        self.events.notify();
        Ok(())
    }

    fn shutdown(&self) -> DaemonResult<()> {
        self.lifecycle_transaction(
            true,
            || Ok(self.durable.shells()?),
            |undo| {
                let executions = self.durable.scheduled_executions(None, None)?;
                for snapshot in executions {
                    if snapshot.state.is_terminal() {
                        continue;
                    }
                    let execution = self.durable.execution(&snapshot.id)?;
                    let (_, record) = self.durable.mutate_execution(&execution, |state| {
                        state.state = ScheduledExecutionState::Cancelled;
                        state.ended_at_ms = Some(unix_time_ms().max(execution.requested_at_ms));
                        state.reason = Some(ScheduledExecutionReason::DaemonShutdown);
                        state.outcome = None;
                        Ok(())
                    })?;
                    undo.record(record);
                }
                Ok(())
            },
            |_| Vec::new(),
        )
    }

    fn workspace(&self, id: &str) -> io::Result<Arc<Workspace>> {
        self.durable.workspace(id)
    }

    fn shell(&self, id: &str) -> io::Result<Arc<Shell>> {
        self.durable.shell(id)
    }

    fn launcher(&self, id: &str) -> io::Result<Arc<WorkspaceLauncher>> {
        self.durable.launcher(id)
    }

    fn agent(&self, id: &str) -> io::Result<Arc<AgentInstance>> {
        self.durable.agent(id)
    }

    fn schedule(&self, id: &str) -> io::Result<Arc<AgentSchedule>> {
        self.durable.schedule(id)
    }

    fn contains_shell(&self, shell: &Arc<Shell>) -> io::Result<bool> {
        self.durable.contains_shell(shell)
    }

    #[cfg(test)]
    fn create_workspace(
        &self,
        name: String,
        specs: Vec<ShellSpec>,
    ) -> io::Result<WorkspaceSnapshot> {
        self.create_workspace_with_default_cwd(name, None, specs)
    }

    #[cfg(test)]
    fn create_workspace_with_default_cwd(
        &self,
        name: String,
        default_cwd: Option<PathBuf>,
        specs: Vec<ShellSpec>,
    ) -> io::Result<WorkspaceSnapshot> {
        self.durable
            .create_workspace(name, default_cwd, specs)
            .map(|(snapshot, _)| snapshot)
    }

    fn create_workspace_mutation(
        &self,
        undo: &mut DurableUndoLog,
        name: String,
        default_cwd: Option<PathBuf>,
        specs: Vec<ShellSpec>,
    ) -> io::Result<WorkspaceSnapshot> {
        let (snapshot, record) = self.durable.create_workspace(name, default_cwd, specs)?;
        undo.record(record);
        Ok(snapshot)
    }

    #[cfg(test)]
    fn create_shell(&self, workspace_id: &str, spec: ShellSpec) -> io::Result<ShellSnapshot> {
        self.durable
            .create_shell(workspace_id, spec)
            .map(|(snapshot, _)| snapshot)
    }

    #[cfg(test)]
    fn create_shell_with_workspace(&self, spec: ShellSpec) -> io::Result<ShellSnapshot> {
        self.durable
            .create_shell_with_workspace(spec)
            .map(|(snapshot, _)| snapshot)
    }

    fn create_shell_mutation(
        &self,
        undo: &mut DurableUndoLog,
        workspace_id: Option<&str>,
        spec: ShellSpec,
    ) -> io::Result<ShellSnapshot> {
        let (snapshot, record) = match workspace_id {
            Some(workspace_id) => self.durable.create_shell(workspace_id, spec)?,
            None => self.durable.create_shell_with_workspace(spec)?,
        };
        undo.record(record);
        Ok(snapshot)
    }

    fn create_launcher_mutation(
        &self,
        undo: &mut DurableUndoLog,
        workspace_id: &str,
        spec: WorkspaceLauncherSpec,
    ) -> io::Result<WorkspaceLauncherSnapshot> {
        let (snapshot, record) = self.durable.create_launcher(workspace_id, spec)?;
        undo.record(record);
        Ok(snapshot)
    }

    fn create_schedule_mutation(
        &self,
        undo: &mut DurableUndoLog,
        workspace_id: &str,
        spec: AgentScheduleSpec,
    ) -> io::Result<AgentScheduleSnapshot> {
        let (snapshot, record) =
            self.durable
                .create_schedule_at(workspace_id, spec, self.clock_now_ms())?;
        undo.record(record);
        Ok(snapshot)
    }

    fn change_schedule_state(
        &self,
        schedule_id: &str,
        next: AgentScheduleState,
    ) -> DaemonResult<Response> {
        self.change_schedule_state_guarded(schedule_id, next, None)
    }

    fn change_schedule_state_guarded(
        &self,
        schedule_id: &str,
        next: AgentScheduleState,
        expected_revision: Option<u64>,
    ) -> DaemonResult<Response> {
        self.durable_mutation_outcome(|undo| {
            let (schedule, record) = self.durable.set_schedule_state_at(
                schedule_id,
                next,
                expected_revision,
                self.clock_now_ms(),
            )?;
            let Some(record) = record else {
                return Ok(DurableMutation::Unchanged(Response::AgentSchedule {
                    schedule,
                }));
            };
            undo.record(record);
            let event = match next {
                AgentScheduleState::Paused => DaemonEventKind::AgentSchedulePaused {
                    workspace_id: schedule.workspace_id.clone(),
                    schedule: schedule.clone(),
                },
                AgentScheduleState::Enabled => DaemonEventKind::AgentScheduleResumed {
                    workspace_id: schedule.workspace_id.clone(),
                    schedule: schedule.clone(),
                },
            };
            Ok(DurableMutation::Changed(
                Response::AgentSchedule { schedule },
                vec![event],
            ))
        })
    }

    #[cfg(test)]
    fn register_agent(
        &self,
        shell_id: &str,
        run_id: &str,
        spec: AgentRegistrationSpec,
    ) -> DaemonResult<AgentInstanceSnapshot> {
        self.durable
            .register_agent(shell_id, run_id, spec)
            .map(|(snapshot, _)| snapshot)
    }

    fn register_agent_mutation(
        &self,
        undo: &mut DurableUndoLog,
        shell_id: &str,
        run_id: &str,
        spec: AgentRegistrationSpec,
    ) -> DaemonResult<AgentInstanceSnapshot> {
        let (snapshot, record) = self.durable.register_agent(shell_id, run_id, spec)?;
        undo.record(record);
        Ok(snapshot)
    }

    #[cfg(test)]
    fn ensure_agent(
        &self,
        shell_id: &str,
        run_id: &str,
        spec: AgentRegistrationSpec,
    ) -> DaemonResult<(AgentInstanceSnapshot, bool)> {
        self.durable
            .ensure_agent(shell_id, run_id, spec)
            .map(|(snapshot, created, _)| (snapshot, created))
    }

    #[cfg(test)]
    fn report_agent(
        &self,
        agent_id: &str,
        run_id: &str,
        report: AgentReport,
    ) -> DaemonResult<(AgentInstanceSnapshot, bool, bool)> {
        self.durable
            .report_agent(agent_id, run_id, report)
            .map(|(snapshot, changed, completed, _)| (snapshot, changed, completed))
    }

    fn report_agent_mutation(
        &self,
        undo: &mut DurableUndoLog,
        agent_id: &str,
        run_id: &str,
        report: AgentReport,
    ) -> DaemonResult<(AgentInstanceSnapshot, bool, bool)> {
        let (snapshot, changed, completed, record) =
            self.durable.report_agent(agent_id, run_id, report)?;
        if let Some(record) = record {
            undo.record(record);
        }
        Ok((snapshot, changed, completed))
    }

    #[cfg(test)]
    fn acknowledge_agent_attention(
        &self,
        agent_id: &str,
        observation_revision: u64,
    ) -> DaemonResult<(AgentInstanceSnapshot, bool)> {
        self.durable
            .acknowledge_agent_attention(agent_id, observation_revision)
            .map(|(snapshot, changed, _)| (snapshot, changed))
    }

    fn acknowledge_agent_attention_mutation(
        &self,
        undo: &mut DurableUndoLog,
        agent_id: &str,
        observation_revision: u64,
    ) -> DaemonResult<(AgentInstanceSnapshot, bool)> {
        let (snapshot, changed, record) = self
            .durable
            .acknowledge_agent_attention(agent_id, observation_revision)?;
        if let Some(record) = record {
            undo.record(record);
        }
        Ok((snapshot, changed))
    }

    fn rename_workspace_mutation(
        &self,
        undo: &mut DurableUndoLog,
        workspace_id: &str,
        name: String,
    ) -> io::Result<DurableMutation<()>> {
        let event_name = name.clone();
        let Some(record) = self.durable.rename_workspace(workspace_id, name)? else {
            return Ok(DurableMutation::Unchanged(()));
        };
        undo.record(record);
        Ok(DurableMutation::Changed(
            (),
            vec![DaemonEventKind::WorkspaceRenamed {
                workspace_id: workspace_id.into(),
                name: event_name,
            }],
        ))
    }

    fn rename_shell_mutation(
        &self,
        undo: &mut DurableUndoLog,
        shell_id: &str,
        name: String,
    ) -> io::Result<DurableMutation<()>> {
        let event_name = name.clone();
        let Some(record) = self.durable.rename_shell(shell_id, name)? else {
            return Ok(DurableMutation::Unchanged(()));
        };
        let workspace_id = match &record {
            DurableUndo::RenamedShell { shell, .. } => shell.workspace_id.clone(),
            _ => unreachable!("shell rename returned the wrong undo record"),
        };
        undo.record(record);
        Ok(DurableMutation::Changed(
            (),
            vec![DaemonEventKind::ShellRenamed {
                workspace_id,
                shell_id: shell_id.into(),
                name: event_name,
            }],
        ))
    }

    fn rename_launcher_mutation(
        &self,
        undo: &mut DurableUndoLog,
        launcher_id: &str,
        name: String,
    ) -> io::Result<DurableMutation<()>> {
        let event_name = name.clone();
        let Some(record) = self.durable.rename_launcher(launcher_id, name)? else {
            return Ok(DurableMutation::Unchanged(()));
        };
        let workspace_id = match &record {
            DurableUndo::RenamedLauncher { launcher, .. } => launcher.workspace_id.clone(),
            _ => unreachable!("launcher rename returned the wrong undo record"),
        };
        undo.record(record);
        Ok(DurableMutation::Changed(
            (),
            vec![DaemonEventKind::LauncherRenamed {
                workspace_id,
                launcher_id: launcher_id.into(),
                name: event_name,
            }],
        ))
    }

    fn remove_launcher_mutation(
        &self,
        undo: &mut DurableUndoLog,
        launcher_id: &str,
    ) -> io::Result<Arc<WorkspaceLauncher>> {
        let mut state = lock(&self.durable.state)?;
        let launcher = state
            .launchers
            .get(launcher_id)
            .cloned()
            .ok_or_else(|| not_found("workspace launcher", launcher_id))?;
        let workspace = state
            .workspaces
            .get(&launcher.workspace_id)
            .cloned()
            .ok_or_else(|| not_found("workspace", &launcher.workspace_id))?;
        let mut launcher_ids = lock(&workspace.launcher_ids)?;
        let index = launcher_ids
            .iter()
            .position(|id| id == launcher_id)
            .ok_or_else(|| not_found("workspace launcher", launcher_id))?;
        state.launchers.remove(launcher_id);
        launcher_ids.remove(index);
        bump_revision(&workspace.revision, "workspace")?;
        drop(launcher_ids);
        drop(state);
        let result = Arc::clone(&launcher);
        undo.record(DurableUndo::RemovedLauncher {
            workspace,
            launcher,
            index,
        });
        Ok(result)
    }

    fn remove_schedule_mutation(
        &self,
        undo: &mut DurableUndoLog,
        schedule_id: &str,
    ) -> io::Result<Arc<AgentSchedule>> {
        let schedule = self.schedule(schedule_id)?;
        if lock(&schedule.executions)?
            .iter()
            .any(|execution| lock(&execution.state).is_ok_and(|state| !state.state.is_terminal()))
        {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "agent schedule has a nonterminal execution",
            ));
        }
        if let Some(shell_id) = lock(&schedule.state)?.execution_shell_id.clone() {
            let shell = self.shell(&shell_id)?;
            if matches!(*lock(&shell.lifecycle)?, ShellLifecycle::Running { .. }) {
                return Err(io::Error::new(
                    io::ErrorKind::WouldBlock,
                    "agent schedule runner shell is still active",
                ));
            }
            undo.record(self.remove_shell_mutation(&shell_id)?);
        }
        let mut state = lock(&self.durable.state)?;
        let schedule = state
            .schedules
            .get(schedule_id)
            .cloned()
            .ok_or_else(|| not_found("agent schedule", schedule_id))?;
        let workspace = state
            .workspaces
            .get(&schedule.workspace_id)
            .cloned()
            .ok_or_else(|| not_found("workspace", &schedule.workspace_id))?;
        let mut schedule_ids = lock(&workspace.schedule_ids)?;
        let index = schedule_ids
            .iter()
            .position(|id| id == schedule_id)
            .ok_or_else(|| not_found("agent schedule", schedule_id))?;
        state.schedules.remove(schedule_id);
        schedule_ids.remove(index);
        bump_revision(&workspace.revision, "workspace")?;
        drop(schedule_ids);
        drop(state);
        let result = Arc::clone(&schedule);
        undo.record(DurableUndo::RemovedSchedule {
            workspace,
            schedule,
            index,
        });
        Ok(result)
    }

    fn read_shell(&self, shell_id: &str, max_bytes: usize) -> io::Result<Vec<u8>> {
        let shell = self.shell(shell_id)?;
        self.runtimes.read_shell(&shell, max_bytes)
    }

    fn read_shell_preview(
        &self,
        shell_id: &str,
        max_bytes: usize,
        max_lines: u16,
    ) -> io::Result<TerminalPreview> {
        let shell = self.shell(shell_id)?;
        self.runtimes
            .read_shell_preview(&shell, max_bytes, max_lines)
    }

    fn wait_agent(
        &self,
        agent_id: &str,
        after_revision: u64,
        wait_ms: u32,
    ) -> DaemonResult<Response> {
        self.events.wait_for(
            wait_ms,
            || self.runtimes.is_stopping(),
            |expired| {
                let agent = self.agent(agent_id)?.snapshot()?;
                let revision = agent.observation.revision;
                if after_revision < revision {
                    return Ok(Some(Response::AgentWait {
                        agent,
                        changed: true,
                    }));
                }
                if after_revision > revision {
                    return Err(DaemonError::lifecycle(
                        ErrorCode::RevisionAhead,
                        "requested Agent revision is ahead of the current observation",
                    ));
                }
                if agent.observation.state == AgentState::Done || expired {
                    return Ok(Some(Response::AgentWait {
                        agent,
                        changed: false,
                    }));
                }
                Ok(None)
            },
        )
    }

    fn wait_scheduled_execution(
        &self,
        execution_id: &str,
        after_revision: u64,
        wait_ms: u32,
    ) -> DaemonResult<Response> {
        self.events
            .wait_for_scheduled_execution(execution_id, after_revision, wait_ms, || {
                self.runtimes.is_stopping()
            })
    }

    fn read_shell_at(
        &self,
        shell_id: &str,
        max_bytes: usize,
        expected_run_id: Option<&str>,
        after_revision: Option<u64>,
        wait_ms: u32,
    ) -> DaemonResult<Response> {
        self.runtimes.read_shell_at(
            self,
            shell_id,
            max_bytes,
            expected_run_id,
            after_revision,
            wait_ms,
        )
    }

    fn remove_shell_mutation(&self, shell_id: &str) -> io::Result<DurableUndo> {
        let mut state = lock(&self.durable.state)?;
        let shell = state
            .shells
            .get(shell_id)
            .cloned()
            .ok_or_else(|| not_found("shell", shell_id))?;
        let workspace = state
            .workspaces
            .get(&shell.workspace_id)
            .cloned()
            .ok_or_else(|| not_found("workspace", &shell.workspace_id))?;
        let mut shell_ids = lock(&workspace.shell_ids)?;
        let index = shell_ids
            .iter()
            .position(|id| id == shell_id)
            .ok_or_else(|| not_found("shell", shell_id))?;
        state.shells.remove(shell_id);
        shell_ids.remove(index);
        bump_revision(&workspace.revision, "workspace")?;
        drop(shell_ids);
        drop(state);
        Ok(DurableUndo::RemovedShell {
            workspace,
            shell,
            index,
        })
    }

    fn remove_workspace_mutation(&self, workspace_id: &str) -> io::Result<DurableUndo> {
        let mut state = lock(&self.durable.state)?;
        let workspace = state
            .workspaces
            .get(workspace_id)
            .cloned()
            .ok_or_else(|| not_found("workspace", workspace_id))?;
        let shell_ids = lock(&workspace.shell_ids)?.clone();
        let launcher_ids = lock(&workspace.launcher_ids)?.clone();
        let agent_ids = lock(&workspace.agent_ids)?.clone();
        let schedule_ids = lock(&workspace.schedule_ids)?.clone();
        let shells = shell_ids
            .iter()
            .filter_map(|id| state.shells.get(id).cloned())
            .collect();
        let launchers = launcher_ids
            .iter()
            .filter_map(|id| state.launchers.get(id).cloned())
            .collect();
        let agents = agent_ids
            .iter()
            .filter_map(|id| state.agents.get(id).cloned())
            .collect();
        let schedules = schedule_ids
            .iter()
            .filter_map(|id| state.schedules.get(id).cloned())
            .collect();
        state.workspaces.remove(workspace_id);
        for id in shell_ids {
            state.shells.remove(&id);
        }
        for id in launcher_ids {
            state.launchers.remove(&id);
        }
        for id in agent_ids {
            state.agents.remove(&id);
        }
        for id in schedule_ids {
            state.schedules.remove(&id);
        }
        drop(state);
        Ok(DurableUndo::RemovedWorkspace {
            workspace,
            shells,
            launchers,
            agents,
            schedules,
        })
    }

    fn close_shell(&self, shell_id: &str) -> DaemonResult<()> {
        self.close_shell_guarded(shell_id, None)
    }

    fn close_shell_guarded(
        &self,
        shell_id: &str,
        expected_revision: Option<u64>,
    ) -> DaemonResult<()> {
        if !matches!(self.shell(shell_id)?.owner, ShellOwner::User) {
            return Err(DaemonError::lifecycle(
                ErrorCode::Busy,
                "schedule-owned shells cannot be closed directly",
            ));
        }
        self.lifecycle_transaction(
            false,
            || {
                let shell = self.shell(shell_id)?;
                if let Some(expected) = expected_revision {
                    require_guard(*lock(&shell.revision)?, expected, "shell")?;
                }
                Ok(vec![shell])
            },
            |undo| {
                undo.record(self.remove_shell_mutation(shell_id)?);
                Ok(())
            },
            |shells| {
                vec![DaemonEventKind::ShellClosed {
                    workspace_id: shells.first().map(|shell| shell.workspace_id.clone()),
                    shell_id: shell_id.into(),
                }]
            },
        )
    }

    fn close_workspace(&self, workspace_id: &str) -> DaemonResult<()> {
        self.close_workspace_guarded(workspace_id, None)
    }

    fn close_workspace_guarded(
        &self,
        workspace_id: &str,
        expected_revision: Option<u64>,
    ) -> DaemonResult<()> {
        self.lifecycle_transaction(
            false,
            || {
                let workspace = self.workspace(workspace_id)?;
                if let Some(expected) = expected_revision {
                    require_guard(*lock(&workspace.revision)?, expected, "workspace")?;
                }
                Ok(self.durable.workspace_shells(&workspace)?)
            },
            |undo| {
                undo.record(self.remove_workspace_mutation(workspace_id)?);
                Ok(())
            },
            |_| {
                vec![DaemonEventKind::WorkspaceClosed {
                    workspace_id: workspace_id.into(),
                }]
            },
        )
    }

    fn restart_shell(&self, shell_id: &str) -> DaemonResult<ShellSnapshot> {
        self.restart_shell_guarded(shell_id, None)
    }

    fn restart_shell_guarded(
        &self,
        shell_id: &str,
        guard: Option<(u64, &str)>,
    ) -> DaemonResult<ShellSnapshot> {
        let _mutation = lock(&self.mutation_lock)?;
        self.ensure_running()?;
        let shell = self.shell(shell_id)?;
        if let Some((expected_revision, expected_run_id)) = guard {
            require_guard(*lock(&shell.revision)?, expected_revision, "shell")?;
            let current_run = shell.snapshot()?.run.map(|run| run.id);
            if current_run.as_deref() != Some(expected_run_id) {
                return Err(DaemonError::lifecycle(
                    ErrorCode::RunChanged,
                    "shell no longer has the confirmed run",
                ));
            }
        }
        if !matches!(shell.owner, ShellOwner::User) {
            return Err(DaemonError::lifecycle(
                ErrorCode::Busy,
                "schedule-owned shells cannot be restarted directly",
            ));
        }
        let old_runtime = {
            let mut lifecycle = lock(&shell.lifecycle)?;
            let old_runtime = match &*lifecycle {
                ShellLifecycle::Pending => {
                    drop(lifecycle);
                    return Ok(shell.snapshot()?);
                }
                ShellLifecycle::Running { .. } => {
                    return Err(DaemonError::lifecycle(
                        ErrorCode::Busy,
                        format!("shell is still running: {shell_id}"),
                    ));
                }
                ShellLifecycle::Exited { runtime, .. } => runtime.clone(),
                ShellLifecycle::Closed => return Err(not_found("shell", shell_id).into()),
            };
            *lifecycle = ShellLifecycle::Pending;
            old_runtime
        };
        if let Some(runtime) = old_runtime {
            self.runtimes.stop_reader(&runtime)?;
        }
        Ok(shell.snapshot()?)
    }
}

impl Workspace {
    fn snapshot(&self, registry: &DurableRegistry) -> io::Result<WorkspaceSnapshot> {
        let (shells, launchers, agents, schedules) = {
            let state = lock(&registry.state)?;
            let shell_ids = lock(&self.shell_ids)?;
            let shells = shell_ids
                .iter()
                .filter_map(|id| state.shells.get(id).cloned())
                .collect::<Vec<_>>();
            let launcher_ids = lock(&self.launcher_ids)?;
            let launchers = launcher_ids
                .iter()
                .filter_map(|id| state.launchers.get(id).cloned())
                .collect::<Vec<_>>();
            let agent_ids = lock(&self.agent_ids)?;
            let agents = agent_ids
                .iter()
                .filter_map(|id| state.agents.get(id).cloned())
                .collect::<Vec<_>>();
            let schedule_ids = lock(&self.schedule_ids)?;
            let schedules = schedule_ids
                .iter()
                .filter_map(|id| state.schedules.get(id).cloned())
                .collect::<Vec<_>>();
            (shells, launchers, agents, schedules)
        };
        let shells = shells
            .iter()
            .map(|shell| shell.snapshot())
            .collect::<io::Result<_>>()?;
        let launchers = launchers
            .iter()
            .map(|launcher| launcher.snapshot())
            .collect::<io::Result<_>>()?;
        let agents = agents
            .iter()
            .map(|agent| agent.snapshot())
            .collect::<io::Result<_>>()?;
        let schedules = schedules
            .iter()
            .map(|schedule| schedule.snapshot())
            .collect::<io::Result<_>>()?;
        Ok(WorkspaceSnapshot {
            id: self.id.clone(),
            revision: *lock(&self.revision)?,
            name: lock(&self.name)?.clone(),
            default_cwd: self.default_cwd.clone(),
            shells,
            launchers,
            agents,
            schedules,
        })
    }
}

impl AgentSchedule {
    fn node_projection(&self) -> io::Result<NodeProjectionSchedule> {
        let state = lock(&self.state)?;
        let next_occurrence = if state.state == AgentScheduleState::Enabled {
            Some(ScheduledOccurrence {
                trigger_revision: state.trigger_revision,
                scheduled_at_ms: crate::scheduling::CronSchedule::compile(
                    &state.trigger.cron,
                    &state.trigger.timezone,
                )
                .and_then(|cron| cron.next_after_ms(state.evaluation_frontier_ms))
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error.to_string()))?,
            })
        } else {
            None
        };
        Ok(NodeProjectionSchedule {
            id: self.id.clone(),
            workspace_id: self.workspace_id.clone(),
            name: state.name.clone(),
            integration: self.integration.clone(),
            state: state.state,
            trigger: state.trigger.clone(),
            revision: state.revision,
            prompt_revision: state.prompt_revision,
            trigger_revision: state.trigger_revision,
            created_at_ms: self.created_at_ms,
            updated_at_ms: state.updated_at_ms,
            next_occurrence,
        })
    }

    fn from_persisted(workspace_id: &str, saved: PersistedAgentSchedule) -> Self {
        let schedule_id = saved.id.clone();
        Self {
            id: saved.id,
            workspace_id: workspace_id.into(),
            cwd: saved.cwd,
            integration: saved.integration,
            session: saved.session,
            overlap_policy: saved.overlap_policy,
            created_at_ms: saved.created_at_ms,
            state: Mutex::new(AgentScheduleMutableState {
                name: saved.name,
                prompt: saved.prompt,
                trigger: saved.trigger,
                prompt_revision: saved.prompt_revision,
                trigger_revision: saved.trigger_revision,
                state: saved.state,
                revision: saved.revision,
                updated_at_ms: saved.updated_at_ms,
                evaluation_frontier_ms: saved.evaluation_frontier_ms,
                evaluation_frontier_trigger_revision: saved.evaluation_frontier_trigger_revision,
                execution_shell_id: saved.execution_shell_id,
            }),
            executions: Mutex::new(
                saved
                    .executions
                    .into_iter()
                    .map(|execution| {
                        Arc::new(ScheduledExecution::from_persisted(
                            workspace_id,
                            &schedule_id,
                            execution,
                        ))
                    })
                    .collect(),
            ),
            dispatch_key_filter: Mutex::new(saved.dispatch_key_filter),
        }
    }

    fn snapshot(&self) -> io::Result<AgentScheduleSnapshot> {
        let state = lock(&self.state)?;
        self.snapshot_from(&state)
    }

    fn snapshot_from(
        &self,
        state: &AgentScheduleMutableState,
    ) -> io::Result<AgentScheduleSnapshot> {
        let next_occurrence = if state.state == AgentScheduleState::Enabled {
            let scheduled_at_ms = crate::scheduling::CronSchedule::compile(
                &state.trigger.cron,
                &state.trigger.timezone,
            )
            .and_then(|cron| cron.next_after_ms(state.evaluation_frontier_ms))
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error.to_string()))?;
            Some(ScheduledOccurrence {
                trigger_revision: state.trigger_revision,
                scheduled_at_ms,
            })
        } else {
            None
        };
        Ok(AgentScheduleSnapshot {
            id: self.id.clone(),
            workspace_id: self.workspace_id.clone(),
            name: state.name.clone(),
            cwd: self.cwd.clone(),
            integration: self.integration.clone(),
            session: self.session.clone(),
            trigger: state.trigger.clone(),
            state: state.state,
            overlap_policy: self.overlap_policy,
            revision: state.revision,
            prompt_revision: state.prompt_revision,
            trigger_revision: state.trigger_revision,
            created_at_ms: self.created_at_ms,
            updated_at_ms: state.updated_at_ms,
            evaluation_frontier_ms: state.evaluation_frontier_ms,
            execution_shell_id: state.execution_shell_id.clone(),
            next_occurrence,
        })
    }

    fn inspection(&self) -> io::Result<AgentScheduleInspection> {
        let state = lock(&self.state)?;
        Ok(AgentScheduleInspection {
            schedule: self.snapshot_from(&state)?,
            prompt: state.prompt.clone(),
        })
    }

    fn persisted(&self) -> io::Result<PersistedAgentSchedule> {
        let state = lock(&self.state)?;
        let snapshot = self.snapshot_from(&state)?;
        let prompt = state.prompt.clone();
        let evaluation_frontier_trigger_revision = state.evaluation_frontier_trigger_revision;
        drop(state);
        let executions = lock(&self.executions)?
            .iter()
            .map(|execution| execution.persisted())
            .collect::<io::Result<_>>()?;
        let dispatch_key_filter = lock(&self.dispatch_key_filter)?.clone();
        Ok(PersistedAgentSchedule {
            id: snapshot.id,
            name: snapshot.name,
            cwd: snapshot.cwd,
            integration: snapshot.integration,
            prompt,
            session: snapshot.session,
            trigger: snapshot.trigger,
            state: snapshot.state,
            overlap_policy: snapshot.overlap_policy,
            revision: snapshot.revision,
            prompt_revision: snapshot.prompt_revision,
            trigger_revision: snapshot.trigger_revision,
            created_at_ms: snapshot.created_at_ms,
            updated_at_ms: snapshot.updated_at_ms,
            evaluation_frontier_ms: snapshot.evaluation_frontier_ms,
            evaluation_frontier_trigger_revision,
            execution_shell_id: snapshot.execution_shell_id,
            dispatch_key_filter,
            executions,
        })
    }
}

impl ScheduledExecution {
    fn node_projection(&self) -> io::Result<NodeProjectionExecution> {
        let state = lock(&self.state)?;
        Ok(NodeProjectionExecution {
            id: self.id.clone(),
            workspace_id: self.workspace_id.clone(),
            schedule_id: self.schedule_id.clone(),
            revision: state.revision,
            state: state.state,
            dispatch_kind: self.dispatch_kind,
            schedule_revision: self.schedule_revision,
            prompt_revision: self.prompt_revision,
            trigger_revision: self.trigger_revision,
            requested_at_ms: self.requested_at_ms,
            scheduled_at_ms: self.scheduled_at_ms,
            started_at_ms: state.started_at_ms,
            ended_at_ms: state.ended_at_ms,
            reason: state.reason,
            outcome: state.outcome.clone(),
            shell_id: state.shell_id.clone(),
            run_id: state.run_id.clone(),
            agent_id: state.agent_id.clone(),
        })
    }

    fn from_persisted(
        workspace_id: &str,
        schedule_id: &str,
        saved: PersistedScheduledExecution,
    ) -> Self {
        Self {
            id: saved.id,
            workspace_id: workspace_id.into(),
            schedule_id: schedule_id.into(),
            dispatch_kind: saved.dispatch_kind,
            dispatch_key: saved.dispatch_key,
            schedule_revision: saved.schedule_revision,
            prompt_revision: saved.prompt_revision,
            trigger_revision: saved.trigger_revision,
            requested_at_ms: saved.requested_at_ms,
            scheduled_at_ms: saved.scheduled_at_ms,
            coalesced_through_ms: saved.coalesced_through_ms,
            cwd: saved.cwd,
            integration: saved.integration,
            session: saved.session,
            prompt: saved.prompt,
            runner_token: saved.runner_token,
            state: Mutex::new(ScheduledExecutionMutableState {
                revision: saved.revision,
                state: saved.state,
                started_at_ms: saved.started_at_ms,
                ended_at_ms: saved.ended_at_ms,
                reason: saved.reason,
                outcome: saved.outcome,
                shell_id: saved.shell_id,
                run_id: saved.run_id,
                agent_id: saved.agent_id,
                external_session_id: saved.external_session_id,
            }),
        }
    }

    fn snapshot(&self) -> io::Result<ScheduledExecutionSnapshot> {
        let state = lock(&self.state)?;
        Ok(self.snapshot_from(&state))
    }

    fn snapshot_from(&self, state: &ScheduledExecutionMutableState) -> ScheduledExecutionSnapshot {
        ScheduledExecutionSnapshot {
            id: self.id.clone(),
            workspace_id: self.workspace_id.clone(),
            schedule_id: self.schedule_id.clone(),
            revision: state.revision,
            state: state.state,
            dispatch_kind: self.dispatch_kind,
            dispatch_key: self.dispatch_key.clone(),
            schedule_revision: self.schedule_revision,
            prompt_revision: self.prompt_revision,
            trigger_revision: self.trigger_revision,
            requested_at_ms: self.requested_at_ms,
            scheduled_at_ms: self.scheduled_at_ms,
            coalesced_through_ms: self.coalesced_through_ms,
            started_at_ms: state.started_at_ms,
            ended_at_ms: state.ended_at_ms,
            cwd: self.cwd.clone(),
            integration: self.integration.clone(),
            session: self.session.clone(),
            reason: state.reason,
            outcome: state.outcome.clone(),
            shell_id: state.shell_id.clone(),
            run_id: state.run_id.clone(),
            agent_id: state.agent_id.clone(),
            external_session_id: state.external_session_id.clone(),
        }
    }

    fn persisted(&self) -> io::Result<PersistedScheduledExecution> {
        let state = lock(&self.state)?;
        Ok(PersistedScheduledExecution {
            id: self.id.clone(),
            revision: state.revision,
            state: state.state,
            dispatch_kind: self.dispatch_kind,
            dispatch_key: self.dispatch_key.clone(),
            schedule_revision: self.schedule_revision,
            prompt_revision: self.prompt_revision,
            trigger_revision: self.trigger_revision,
            requested_at_ms: self.requested_at_ms,
            scheduled_at_ms: self.scheduled_at_ms,
            coalesced_through_ms: self.coalesced_through_ms,
            started_at_ms: state.started_at_ms,
            ended_at_ms: state.ended_at_ms,
            cwd: self.cwd.clone(),
            integration: self.integration.clone(),
            session: self.session.clone(),
            prompt: self.prompt.clone(),
            runner_token: self.runner_token.clone(),
            reason: state.reason,
            outcome: state.outcome.clone(),
            shell_id: state.shell_id.clone(),
            run_id: state.run_id.clone(),
            agent_id: state.agent_id.clone(),
            external_session_id: state.external_session_id.clone(),
        })
    }
}

impl WorkspaceLauncher {
    fn node_projection(&self) -> io::Result<NodeProjectionLauncher> {
        Ok(NodeProjectionLauncher {
            id: self.id.clone(),
            workspace_id: self.workspace_id.clone(),
            name: lock(&self.name)?.clone(),
        })
    }

    fn snapshot(&self) -> io::Result<WorkspaceLauncherSnapshot> {
        Ok(WorkspaceLauncherSnapshot {
            id: self.id.clone(),
            revision: *lock(&self.revision)?,
            workspace_id: self.workspace_id.clone(),
            name: lock(&self.name)?.clone(),
            command: self.command.clone(),
            cwd: self.cwd.clone(),
        })
    }
}

impl AgentInstance {
    fn node_projection(&self) -> io::Result<NodeProjectionAgent> {
        let state = lock(&self.state)?;
        Ok(NodeProjectionAgent {
            id: self.id.clone(),
            workspace_id: self.workspace_id.clone(),
            shell_id: self.shell_id.clone(),
            run_id: self.run_id.clone(),
            name: self.name.clone(),
            integration: self.integration.clone(),
            state: state.observation.state,
            observation_revision: state.observation.revision,
            observed_at_ms: state.observation.observed_at_ms,
            started_at_ms: self.started_at_ms,
            ended_at_ms: state.ended_at_ms,
            attention: state
                .attention
                .as_ref()
                .map(|attention| NodeProjectionAttention {
                    reason: attention.reason,
                    observation_revision: attention.observation.revision,
                    observed_at_ms: attention.observation.observed_at_ms,
                }),
        })
    }

    fn from_persisted(workspace_id: &str, saved: PersistedAgentInstance) -> Self {
        Self {
            id: saved.id,
            workspace_id: workspace_id.into(),
            shell_id: saved.shell_id,
            run_id: saved.run_id,
            name: saved.name,
            integration: saved.integration,
            external_session_id: saved.external_session_id,
            cwd: saved.cwd,
            started_at_ms: saved.started_at_ms,
            state: Mutex::new(AgentInstanceState {
                ended_at_ms: saved.ended_at_ms,
                observation: saved.observation,
                attention: saved.attention,
            }),
        }
    }

    fn snapshot(&self) -> io::Result<AgentInstanceSnapshot> {
        let state = lock(&self.state)?;
        Ok(self.snapshot_from(&state))
    }

    fn snapshot_from(&self, state: &AgentInstanceState) -> AgentInstanceSnapshot {
        AgentInstanceSnapshot {
            id: self.id.clone(),
            workspace_id: self.workspace_id.clone(),
            shell_id: self.shell_id.clone(),
            run_id: self.run_id.clone(),
            name: self.name.clone(),
            integration: self.integration.clone(),
            external_session_id: self.external_session_id.clone(),
            cwd: self.cwd.clone(),
            started_at_ms: self.started_at_ms,
            ended_at_ms: state.ended_at_ms,
            observation: state.observation.clone(),
            attention: state.attention.clone(),
        }
    }

    fn persisted(&self) -> io::Result<PersistedAgentInstance> {
        let snapshot = self.snapshot()?;
        Ok(PersistedAgentInstance {
            id: snapshot.id,
            shell_id: snapshot.shell_id,
            run_id: snapshot.run_id,
            name: snapshot.name,
            integration: snapshot.integration,
            external_session_id: snapshot.external_session_id,
            cwd: snapshot.cwd,
            started_at_ms: snapshot.started_at_ms,
            ended_at_ms: snapshot.ended_at_ms,
            observation: snapshot.observation,
            attention: snapshot.attention,
        })
    }
}

impl Shell {
    fn node_projection(&self) -> io::Result<NodeProjectionShell> {
        let (status, run_id, generation, started_at_ms, ended_at_ms) =
            match &*lock(&self.lifecycle)? {
                ShellLifecycle::Pending => (ShellStatus::Pending, None, None, None, None),
                ShellLifecycle::Running { run, .. } => (
                    ShellStatus::Running,
                    Some(run.id.clone()),
                    Some(run.generation),
                    Some(run.started_at_ms),
                    lock(&run.ended)?.as_ref().map(|end| end.ended_at_ms),
                ),
                ShellLifecycle::Exited { code, run, .. } => (
                    ShellStatus::Exited { code: *code },
                    Some(run.id.clone()),
                    Some(run.generation),
                    Some(run.started_at_ms),
                    lock(&run.ended)?.as_ref().map(|end| end.ended_at_ms),
                ),
                ShellLifecycle::Closed => return Err(not_found("shell", &self.id)),
            };
        Ok(NodeProjectionShell {
            id: self.id.clone(),
            workspace_id: self.workspace_id.clone(),
            name: lock(&self.name)?.clone(),
            owner: self.owner.clone(),
            status,
            run_id,
            generation,
            started_at_ms,
            ended_at_ms,
        })
    }

    fn snapshot(&self) -> io::Result<ShellSnapshot> {
        let (status, run, runtime) = match &*lock(&self.lifecycle)? {
            ShellLifecycle::Pending => (ShellStatus::Pending, None, None),
            ShellLifecycle::Running { run, runtime, .. } => (
                ShellStatus::Running,
                Some(run.snapshot()?),
                Some(Arc::clone(runtime)),
            ),
            ShellLifecycle::Exited { code, run, .. } => (
                ShellStatus::Exited { code: *code },
                Some(run.snapshot()?),
                None,
            ),
            ShellLifecycle::Closed => return Err(not_found("shell", &self.id)),
        };
        let foreground_process = match (runtime, run.as_ref()) {
            (Some(runtime), Some(run)) => {
                ShellRuntimeManager::foreground_process(self, &runtime, run)?
            }
            _ => None,
        };
        Ok(ShellSnapshot {
            id: self.id.clone(),
            revision: *lock(&self.revision)?,
            workspace_id: self.workspace_id.clone(),
            name: lock(&self.name)?.clone(),
            cwd: self.cwd.clone(),
            command: self.command.clone(),
            owner: self.owner.clone(),
            status,
            run,
            foreground_process,
        })
    }
}

fn create_pending_shell(workspace_id: &str, spec: ShellSpec) -> io::Result<Arc<Shell>> {
    validate_name(&spec.name)?;
    validate_cwd(&spec.cwd)?;
    Ok(Arc::new(Shell {
        id: Uuid::new_v4().to_string(),
        revision: Mutex::new(1),
        workspace_id: workspace_id.to_owned(),
        name: Mutex::new(spec.name),
        cwd: spec.cwd,
        command: spec.command,
        owner: ShellOwner::User,
        last_run: Mutex::new(None),
        lifecycle: Mutex::new(ShellLifecycle::Pending),
        foreground_process_cache: Mutex::new(None),
    }))
}

fn initial_terminal_state(
    rows: u16,
    cols: u16,
    workspace_name: &str,
    shell_name: &str,
    history: Option<&str>,
) -> TerminalState {
    let mut terminal = TerminalState::new(rows, cols);
    if let Some(history) = history.filter(|history| !history.is_empty()) {
        terminal.process(b"\x1b[2mBoomux: restored bounded history from previous run\x1b[0m\r\n");
        terminal.process(history.replace('\n', "\r\n").as_bytes());
        if !history.ends_with('\n') {
            terminal.process(b"\r\n");
        }
    }
    terminal.process(format!("\x1b[2mBoomux: {workspace_name}/{shell_name}\x1b[0m\r\n").as_bytes());
    terminal
}

#[derive(Default)]
struct RuntimeRecovery<'a> {
    effective_command: Option<&'a [String]>,
    history: Option<&'a str>,
}

struct RuntimeStart<'a> {
    workspace_name: &'a str,
    shell_name: &'a str,
    profile: &'a TerminalProfile,
    environment: Option<&'a UnixEnvironment>,
    recovery: RuntimeRecovery<'a>,
}

impl ShellRuntimeManager {
    fn spawn_runtime(
        &self,
        shell: &Arc<Shell>,
        run: &ShellRun,
        start: RuntimeStart<'_>,
    ) -> io::Result<(Arc<ShellRuntime>, PtyReader)> {
        let RuntimeStart {
            workspace_name,
            shell_name,
            profile,
            environment,
            recovery,
        } = start;
        let pty = native_pty_system()
            .openpty(PtySize {
                rows: profile.rows,
                cols: profile.cols,
                pixel_width: profile.pixel_width,
                pixel_height: profile.pixel_height,
            })
            .map_err(io::Error::other)?;
        let master = PtyMaster::duplicate(pty.master.as_ref())?;
        let reader = master.try_clone_reader()?;

        let client_shell = environment
            .and_then(|environment| {
                environment
                    .variables
                    .iter()
                    .find(|variable| variable.name == b"SHELL")
                    .map(|variable| std::ffi::OsString::from_vec(variable.value.clone()))
            })
            .or_else(|| {
                environment
                    .is_none()
                    .then(|| env::var_os("SHELL"))
                    .flatten()
            })
            .unwrap_or_else(|| "/bin/sh".into());
        let selected_command = recovery.effective_command.unwrap_or(&shell.command);
        let mut command = if selected_command.is_empty() {
            CommandBuilder::new(client_shell)
        } else {
            let mut command = CommandBuilder::new(&selected_command[0]);
            command.args(&selected_command[1..]);
            command
        };
        command.cwd(&shell.cwd);
        if let Some(environment) = environment {
            command.env_clear();
            for variable in &environment.variables {
                command.env(
                    std::ffi::OsString::from_vec(variable.name.clone()),
                    std::ffi::OsString::from_vec(variable.value.clone()),
                );
            }
        }
        for name in ["TERM", "COLORTERM", "TERM_PROGRAM", "TERM_PROGRAM_VERSION"] {
            command.env_remove(name);
        }
        for (name, value) in [
            ("TERM", profile.term.as_deref()),
            ("COLORTERM", profile.colorterm.as_deref()),
            ("TERM_PROGRAM", profile.term_program.as_deref()),
            (
                "TERM_PROGRAM_VERSION",
                profile.term_program_version.as_deref(),
            ),
        ] {
            if let Some(value) = value {
                command.env(name, value);
            }
        }
        command.env("BOOMUX_WORKSPACE_ID", &shell.workspace_id);
        command.env("BOOMUX_WORKSPACE", workspace_name);
        command.env("BOOMUX_SHELL_ID", &shell.id);
        command.env("BOOMUX_SHELL_NAME", shell_name);
        command.env("BOOMUX_RUN_ID", &run.id);
        let child = pty.slave.spawn_command(command).map_err(io::Error::other)?;
        drop(pty.slave);
        drop(pty.master);

        let terminal = initial_terminal_state(
            profile.rows,
            profile.cols,
            workspace_name,
            shell_name,
            recovery.history,
        );

        Ok((
            Arc::new(ShellRuntime {
                control: Mutex::new(()),
                master: Mutex::new(master),
                process: Mutex::new(ManagedProcess::Owned(child)),
                terminal: Arc::new(Mutex::new(terminal)),
                controller: Mutex::new(None),
                reader: Mutex::new(None),
                output_changed: Condvar::new(),
                output_wait: Mutex::new(()),
            }),
            reader,
        ))
    }

    fn start_pty_reader(
        &self,
        registry: Weak<DaemonService>,
        shell: Arc<Shell>,
        run: Arc<ShellRun>,
        runtime: Arc<ShellRuntime>,
        mut reader: PtyReader,
        start_paused: bool,
    ) -> io::Result<()> {
        let (commands, command_receiver) = mpsc::channel();
        let reader_runtime = Arc::clone(&runtime);
        let reader_run = Arc::clone(&run);
        let handle = thread::Builder::new()
            .name(format!("boomux-pty-{}", shell.id))
            .spawn(move || {
                let mut buffer = [0; 16 * 1024];
                let mut stopped = false;
                let mut paused = start_paused;
                let mut last_history_checkpoint = Instant::now();
                let mut pause_cancellation: Option<Arc<AtomicBool>> = None;
                let mut pending_output_revision = None;
                let mut output_publication_deadline = None;
                loop {
                    if paused {
                        if pause_cancellation
                            .as_ref()
                            .is_some_and(|cancelled| cancelled.load(Ordering::Acquire))
                        {
                            paused = false;
                            pause_cancellation = None;
                            continue;
                        }
                        match command_receiver.recv_timeout(IO_RETRY_DELAY) {
                            Ok(ReaderCommand::Pause {
                                acknowledge,
                                cancelled,
                            }) => {
                                let result = Self::publish_pending_output(
                                    &registry,
                                    &shell,
                                    &reader_run,
                                    &mut pending_output_revision,
                                    &mut output_publication_deadline,
                                );
                                if !cancelled.load(Ordering::Acquire) {
                                    let _ = acknowledge.send(result);
                                }
                            }
                            Ok(ReaderCommand::Resume) => {
                                paused = false;
                                pause_cancellation = None;
                            }
                            Ok(ReaderCommand::Stop) | Err(mpsc::RecvTimeoutError::Disconnected) => {
                                stopped = true;
                                break;
                            }
                            Err(mpsc::RecvTimeoutError::Timeout) => {}
                        }
                        continue;
                    }
                    match command_receiver.try_recv() {
                        Ok(ReaderCommand::Pause {
                            acknowledge,
                            cancelled,
                        }) => {
                            if cancelled.load(Ordering::Acquire) {
                                continue;
                            }
                            if let Err(error) = Self::publish_pending_output(
                                &registry,
                                &shell,
                                &reader_run,
                                &mut pending_output_revision,
                                &mut output_publication_deadline,
                            ) {
                                let _ = acknowledge.send(Err(error));
                                return Err(io::Error::other(
                                    "could not publish output before pause",
                                ));
                            }
                            paused = true;
                            pause_cancellation = Some(Arc::clone(&cancelled));
                            if acknowledge.send(Ok(())).is_err()
                                || cancelled.load(Ordering::Acquire)
                            {
                                paused = false;
                                pause_cancellation = None;
                            }
                            continue;
                        }
                        Ok(ReaderCommand::Resume) => continue,
                        Ok(ReaderCommand::Stop) | Err(mpsc::TryRecvError::Disconnected) => {
                            Self::publish_pending_output(
                                &registry,
                                &shell,
                                &reader_run,
                                &mut pending_output_revision,
                                &mut output_publication_deadline,
                            )?;
                            stopped = true;
                            break;
                        }
                        Err(mpsc::TryRecvError::Empty) => {}
                    }
                    match reader.read(&mut buffer) {
                        Ok(0) => break,
                        Ok(count) => {
                            let bytes = &buffer[..count];
                            let Some(active_registry) = registry.upgrade() else {
                                break;
                            };
                            let Ok(_output_wait) = reader_runtime.output_wait.lock() else {
                                break;
                            };
                            let Ok(mut terminal) = reader_runtime.terminal.lock() else {
                                break;
                            };
                            terminal.process(bytes);
                            let Ok(previous_revision) = reader_run.output_revision.fetch_update(
                                Ordering::AcqRel,
                                Ordering::Acquire,
                                |revision| revision.checked_add(1),
                            ) else {
                                break;
                            };
                            let revision = previous_revision + 1;
                            if let Ok(mut last_run) = shell.last_run.lock()
                                && let Some(last_run) = last_run.as_mut()
                                && last_run.id == reader_run.id
                            {
                                last_run.output_revision = revision;
                            }
                            if let Ok(mut controller) = reader_runtime.controller.lock() {
                                let disconnect = controller.as_ref().is_some_and(|current| {
                                    matches!(
                                        current
                                            .output
                                            .try_send(ControllerOutput::Data(bytes.to_vec())),
                                        Err(TrySendError::Full(_) | TrySendError::Disconnected(_))
                                    )
                                });
                                if disconnect && let Some(current) = controller.take() {
                                    let _ = current.connection.shutdown(std::net::Shutdown::Both);
                                }
                            }
                            drop(terminal);
                            reader_runtime.output_changed.notify_all();
                            drop(_output_wait);
                            pending_output_revision = Some(revision);
                            output_publication_deadline.get_or_insert_with(|| {
                                Instant::now() + OUTPUT_PUBLICATION_INTERVAL
                            });
                            if output_publication_deadline
                                .is_some_and(|deadline| Instant::now() >= deadline)
                                && Self::publish_pending_output(
                                    &registry,
                                    &shell,
                                    &reader_run,
                                    &mut pending_output_revision,
                                    &mut output_publication_deadline,
                                )
                                .is_err()
                            {
                                break;
                            }
                            if active_registry
                                .notification_settings
                                .persist_terminal_history
                                && last_history_checkpoint.elapsed() >= TERMINAL_HISTORY_INTERVAL
                                && Self::checkpoint_terminal_history(
                                    &active_registry,
                                    &shell,
                                    &reader_run,
                                    &reader_runtime,
                                )
                                .is_ok()
                            {
                                last_history_checkpoint = Instant::now();
                            }
                        }
                        Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                        Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                            if output_publication_deadline
                                .is_some_and(|deadline| Instant::now() >= deadline)
                                && Self::publish_pending_output(
                                    &registry,
                                    &shell,
                                    &reader_run,
                                    &mut pending_output_revision,
                                    &mut output_publication_deadline,
                                )
                                .is_err()
                            {
                                break;
                            }
                            let process_exited = reader_runtime
                                .process
                                .lock()
                                .ok()
                                .and_then(|mut process| process.try_wait_code().ok())
                                .flatten()
                                .is_some();
                            if process_exited {
                                break;
                            }
                            thread::sleep(IO_RETRY_DELAY);
                        }
                        Err(_) => break,
                    }
                }
                Self::publish_pending_output(
                    &registry,
                    &shell,
                    &reader_run,
                    &mut pending_output_revision,
                    &mut output_publication_deadline,
                )?;
                let _wait = lock(&reader_runtime.output_wait)?;
                reader_runtime.output_changed.notify_all();
                drop(_wait);
                if stopped {
                    return Ok(());
                }
                let code = reader_runtime
                    .process
                    .lock()
                    .ok()
                    .and_then(|mut process| process.try_wait_code().ok().flatten().flatten());
                if let Some(active_registry) = registry.upgrade() {
                    if active_registry
                        .notification_settings
                        .persist_terminal_history
                    {
                        let _ = Self::checkpoint_terminal_history(
                            &active_registry,
                            &shell,
                            &reader_run,
                            &reader_runtime,
                        );
                    }
                    match active_registry.try_record_run_exit(
                        &shell,
                        &reader_run,
                        &reader_runtime,
                        code,
                    ) {
                        Ok(RunExitRecord::Deferred) => {
                            if let Err(error) = DaemonService::defer_run_exit(
                                Weak::clone(&registry),
                                Arc::clone(&shell),
                                Arc::clone(&reader_run),
                                Arc::clone(&reader_runtime),
                                code,
                            ) {
                                eprintln!("boomux: could not defer shell run exit: {error}");
                            }
                        }
                        Ok(RunExitRecord::Recorded | RunExitRecord::Unchanged) => {}
                        Err(error) => {
                            eprintln!("boomux: could not persist shell run exit: {error}");
                        }
                    }
                }
                let _ = reader_runtime
                    .controller
                    .lock()
                    .map(|mut controller| controller.take());
                Ok(())
            })?;
        *lock(&runtime.reader)? = Some(ReaderTask {
            commands,
            handle: Mutex::new(Some(handle)),
        });
        Ok(())
    }

    fn publish_pending_output(
        registry: &Weak<DaemonService>,
        shell: &Shell,
        run: &ShellRun,
        pending_revision: &mut Option<u64>,
        deadline: &mut Option<Instant>,
    ) -> io::Result<()> {
        let Some(revision) = *pending_revision else {
            return Ok(());
        };
        let Some(registry) = registry.upgrade() else {
            return Ok(());
        };
        registry
            .events
            .publish_runtime_batch(vec![DaemonEventKind::OutputChanged {
                workspace_id: shell.workspace_id.clone(),
                shell_id: shell.id.clone(),
                run_id: run.id.clone(),
                output_revision: revision,
            }])?;
        *pending_revision = None;
        *deadline = None;
        Ok(())
    }

    fn checkpoint_terminal_history(
        registry: &DaemonService,
        shell: &Shell,
        run: &ShellRun,
        runtime: &ShellRuntime,
    ) -> io::Result<()> {
        let snapshot = lock(&runtime.terminal)?.snapshot();
        let history = snapshot.plain_text_suffix(MAX_TERMINAL_HISTORY_BYTES);
        let mut last_run = lock(&shell.last_run)?;
        let Some(last_run) = last_run.as_mut().filter(|saved| saved.id == run.id) else {
            return Ok(());
        };
        if last_run.terminal_history.as_deref() == Some(&history) {
            return Ok(());
        }
        last_run.terminal_history = Some(history);
        registry.mark_persistence_dirty();
        Ok(())
    }
}

struct AttachRequestOptions {
    takeover: bool,
    restart_exited: bool,
    expected_run_id: Option<String>,
    profile: TerminalProfile,
    environment: Option<UnixEnvironment>,
    owner_environment: bool,
}

impl ShellRuntimeManager {
    fn handle_attach(
        &self,
        mut stream: UnixStream,
        response_version: u32,
        registry: &Arc<DaemonService>,
        shell_id: &str,
        options: AttachRequestOptions,
    ) -> io::Result<()> {
        let AttachRequestOptions {
            takeover,
            restart_exited,
            expected_run_id,
            profile,
            environment,
            owner_environment,
        } = options;
        if let Err(error) = validate_terminal_profile(&profile) {
            return send_daemon_error(&mut stream, response_version, error.into());
        }
        if let Some(environment) = &environment
            && let Err(error) = validate_unix_environment(environment)
        {
            return send_daemon_error(&mut stream, response_version, error.into());
        }
        if owner_environment && environment.is_some() {
            return send_response(
                &mut stream,
                response_version,
                DaemonError::validation(
                    "owner-environment attachment cannot include a Unix environment",
                )
                .into_response(),
            );
        }
        let shell = match registry.shell(shell_id) {
            Ok(shell) => shell,
            Err(error) => {
                return send_daemon_error(&mut stream, response_version, error.into());
            }
        };
        if !matches!(shell.owner, ShellOwner::User)
            && (restart_exited || matches!(*lock(&shell.lifecycle)?, ShellLifecycle::Pending))
        {
            return send_response(
                &mut stream,
                response_version,
                DaemonError::lifecycle(
                    ErrorCode::Busy,
                    "schedule-owned shells start only through schedule dispatch",
                )
                .into_response(),
            );
        }
        let mutation = lock(&registry.mutation_lock)?;
        if registry.runtimes.is_stopping() {
            return send_response(
                &mut stream,
                response_version,
                DaemonError::lifecycle(ErrorCode::DaemonStopping, "Boomux daemon is stopping")
                    .into_response(),
            );
        }
        let token = Uuid::new_v4().to_string();
        let (output, receiver) = mpsc::sync_channel(CONTROLLER_QUEUE);
        let connection = stream.try_clone()?;
        if !registry.contains_shell(&shell)? {
            return send_response(
                &mut stream,
                response_version,
                DaemonError::lifecycle(ErrorCode::NotFound, format!("shell not found: {shell_id}"))
                    .into_response(),
            );
        }
        let workspace = registry.workspace(&shell.workspace_id)?;
        let workspace_name = lock(&workspace.name)?.clone();
        let shell_name = lock(&shell.name)?.clone();
        if expected_run_id.is_some() && restart_exited {
            return send_response(
                &mut stream,
                response_version,
                DaemonError::validation("exact run attachment cannot restart an exited shell")
                    .into_response(),
            );
        }
        if let Some(expected_run_id) = expected_run_id.as_deref() {
            let lifecycle = lock(&shell.lifecycle)?;
            if !matches!(
                &*lifecycle,
                ShellLifecycle::Running { run, .. } if run.id == expected_run_id
            ) {
                return send_response(
                    &mut stream,
                    response_version,
                    DaemonError::lifecycle(
                        ErrorCode::RunChanged,
                        "shell is no longer running the expected scheduled execution run",
                    )
                    .into_response(),
                );
            }
        }
        if restart_exited {
            let old_runtime = match &*lock(&shell.lifecycle)? {
                ShellLifecycle::Exited { runtime, .. } => runtime.clone(),
                _ => None,
            };
            if let Some(runtime) = old_runtime {
                self.stop_reader(&runtime)?;
            }
            let mut lifecycle = lock(&shell.lifecycle)?;
            if matches!(*lifecycle, ShellLifecycle::Exited { .. }) {
                *lifecycle = ShellLifecycle::Pending;
            }
        }
        let previous_run = lock(&shell.last_run)?.clone();
        let needs_start = matches!(*lock(&shell.lifecycle)?, ShellLifecycle::Pending);
        let resume_command = needs_start
            .then(|| registry.agent_resume_command(&shell, previous_run.as_ref()))
            .transpose()?
            .flatten();
        let restored_history = needs_start
            .then(|| {
                registry
                    .notification_settings
                    .persist_terminal_history
                    .then(|| previous_run.as_ref()?.terminal_history.clone())
                    .flatten()
            })
            .flatten();
        if needs_start && let Err(error) = registry.flush_pending() {
            return send_daemon_error(&mut stream, response_version, error);
        }
        let persistence = needs_start
            .then(|| lock(&registry.durable.persist_lock))
            .transpose()?;
        let mut event_transaction = needs_start
            .then(|| registry.events.transaction())
            .transpose()?;
        if needs_start {
            event_transaction
                .as_ref()
                .expect("start event transaction is locked")
                .reserve(1)?;
        }
        let (attached_run, runtime, terminal, startup_profile, running, started) = {
            let mut lifecycle = lock(&shell.lifecycle)?;
            let mut started = false;
            if matches!(*lifecycle, ShellLifecycle::Pending) {
                let generation = previous_run.as_ref().map_or(Ok(1), |run| {
                    run.generation
                        .checked_add(1)
                        .ok_or_else(|| io::Error::other("shell run generation exhausted"))
                })?;
                let run = Arc::new(ShellRun::new(generation));
                let (runtime, reader) = match self.spawn_runtime(
                    &shell,
                    &run,
                    RuntimeStart {
                        workspace_name: &workspace_name,
                        shell_name: &shell_name,
                        profile: &profile,
                        environment: if owner_environment {
                            Some(&registry.startup_environment)
                        } else {
                            environment.as_ref()
                        },
                        recovery: RuntimeRecovery {
                            effective_command: resume_command.as_deref(),
                            history: restored_history.as_deref(),
                        },
                    },
                ) {
                    Ok(runtime) => runtime,
                    Err(error) => {
                        return send_response(
                            &mut stream,
                            response_version,
                            DaemonError::lifecycle(
                                ErrorCode::ShellStartFailed,
                                format!("could not start shell: {error}"),
                            )
                            .into_response(),
                        );
                    }
                };
                *lifecycle = ShellLifecycle::Running {
                    profile: profile.clone(),
                    run: Arc::clone(&run),
                    runtime: Arc::clone(&runtime),
                };
                *lock(&shell.last_run)? = Some(run.persisted(profile.clone())?);
                started = true;
                if let Err(error) = self.start_pty_reader(
                    Arc::downgrade(registry),
                    Arc::clone(&shell),
                    run,
                    runtime,
                    reader,
                    true,
                ) {
                    drop(lifecycle);
                    let cleanup = self.kill(&shell);
                    self.reset_pending(&shell)?;
                    return send_response(
                    &mut stream,
                    response_version,
                    DaemonError::lifecycle(
                        ErrorCode::ShellStartFailed,
                        cleanup.map_or_else(
                            |cleanup| {
                                format!(
                                    "could not start shell reader: {error}; process cleanup also failed: {cleanup}"
                                )
                            },
                            |()| format!("could not start shell reader: {error}"),
                        ),
                    )
                    .into_response(),
                );
                }
            }
            match &*lifecycle {
                ShellLifecycle::Running {
                    profile,
                    run,
                    runtime,
                } => (
                    Some(Arc::clone(run)),
                    Some(Arc::clone(runtime)),
                    Arc::clone(&runtime.terminal),
                    profile.clone(),
                    true,
                    started,
                ),
                ShellLifecycle::Exited {
                    profile, terminal, ..
                } => (
                    None,
                    None,
                    Arc::clone(terminal),
                    profile.clone(),
                    false,
                    started,
                ),
                ShellLifecycle::Pending => unreachable!(),
                ShellLifecycle::Closed => {
                    return send_response(
                        &mut stream,
                        response_version,
                        DaemonError::lifecycle(
                            ErrorCode::NotFound,
                            format!("shell not found: {shell_id}"),
                        )
                        .into_response(),
                    );
                }
            }
        };
        let mut committed_executions = None;
        if started {
            let saved = registry.capture_persisted_state()?;
            event_transaction
                .as_mut()
                .expect("start event transaction is locked")
                .begin_persistence(1);
            drop(event_transaction);
            committed_executions = Some(match registry.write_persisted_state(saved) {
                Ok(committed) => committed,
                Err(error) => {
                    let cleanup = self.kill(&shell);
                    self.reset_pending(&shell)?;
                    let mut transaction = registry.events.transaction()?;
                    transaction.finish_persistence();
                    let message = cleanup.map_or_else(
                        |cleanup| {
                            format!(
                                "could not persist started shell: {error}; process cleanup also failed: {cleanup}"
                            )
                        },
                        |()| format!("could not persist started shell: {error}"),
                    );
                    return send_daemon_error(
                        &mut stream,
                        response_version,
                        DaemonError::persistence_context(error, message),
                    );
                }
            });
            event_transaction = Some(registry.events.transaction()?);
        }
        if started {
            let run = shell
                .snapshot()?
                .run
                .ok_or_else(|| io::Error::other("started shell has no run identity"))?;
            let transaction = event_transaction
                .as_mut()
                .expect("start event transaction is locked");
            transaction.replace_committed_executions(
                committed_executions.expect("persisted shell start has committed executions"),
            );
            transaction.append_batch(vec![DaemonEventKind::RunStarted {
                workspace_id: shell.workspace_id.clone(),
                shell_id: shell.id.clone(),
                run,
            }]);
            transaction.finish_persistence();
        }
        drop(event_transaction);
        drop(persistence);
        if started {
            registry.events.notify();
        }
        if started {
            self.resume_reader(runtime.as_ref().expect("started shell has a runtime"))?;
        }
        let warning =
            term_mismatch_warning(startup_profile.term.as_deref(), profile.term.as_deref());
        if !running {
            lock(&terminal)?.resize(profile.rows, profile.cols);
            send_response(
                &mut stream,
                response_version,
                Response::Attached {
                    token,
                    reconstruction: lock(&terminal)?.reconstruction(),
                    warning,
                },
            )?;
            return AttachFrame::Detached.write_to(&mut stream);
        }
        let attached_run = attached_run.expect("running shell has a run");
        let runtime = runtime.expect("running shell has a runtime");
        let control = lock(&runtime.control)?;
        {
            let controller = lock(&runtime.controller)?;
            if controller.is_some() && !takeover {
                return send_response(
                    &mut stream,
                    response_version,
                    DaemonError::lifecycle(
                        ErrorCode::Busy,
                        "shell already has an active controller; use takeover",
                    )
                    .into_response(),
                );
            }
        }
        lock(&runtime.master)?.resize(Self::profile_size(&profile))?;
        Self::update_runtime_dimensions(&shell, &runtime, Self::profile_size(&profile))?;
        lock(&terminal)?.resize(profile.rows, profile.cols);
        // Keep terminal state locked until the controller is installed so the
        // reconstruction ends exactly where live delivery begins.
        let terminal = lock(&terminal)?;
        send_response(
            &mut stream,
            response_version,
            Response::Attached {
                token: token.clone(),
                reconstruction: terminal.reconstruction(),
                warning,
            },
        )?;
        let lifecycle = lock(&shell.lifecycle)?;
        let still_running = matches!(
            &*lifecycle,
            ShellLifecycle::Running {
                runtime: current,
                ..
            } if Arc::ptr_eq(current, &runtime)
        );
        if !still_running {
            return AttachFrame::Detached.write_to(&mut stream);
        }
        let mut output_stream = stream.try_clone()?;
        output_stream.set_write_timeout(Some(RESPONSE_WRITE_TIMEOUT))?;
        {
            let mut controller = lock(&runtime.controller)?;
            if let Some(previous) = controller.take() {
                let _ = previous.connection.shutdown(std::net::Shutdown::Both);
            }
            *controller = Some(Controller {
                token: token.clone(),
                output,
                connection,
                reconnect_ack: None,
            });
        }
        let output_runtime = Arc::clone(&runtime);
        let output_token = token.clone();
        let output_worker = match thread::Builder::new()
            .name(format!("boomux-attachment-{}", shell.id))
            .spawn(move || {
                let mut reconnect = false;
                while let Ok(output) = receiver.recv() {
                    match output {
                        ControllerOutput::Data(bytes) => {
                            if AttachFrame::Output(bytes)
                                .write_to(&mut output_stream)
                                .is_err()
                            {
                                break;
                            }
                        }
                        ControllerOutput::Reconnect(acknowledge) => {
                            reconnect = true;
                            let result = AttachFrame::Reconnect.write_to(&mut output_stream);
                            let _ = acknowledge.send(result.is_ok());
                            if result.is_ok() {
                                while receiver.recv().is_ok() {}
                            }
                            break;
                        }
                    }
                }
                if !reconnect {
                    let _ = AttachFrame::Detached.write_to(&mut output_stream);
                }
                let _ = output_stream.shutdown(std::net::Shutdown::Both);
                let _ = Self::release_controller(&output_runtime, &output_token);
            }) {
            Ok(worker) => worker,
            Err(error) => {
                Self::release_controller(&runtime, &token)?;
                return Err(error);
            }
        };
        drop(mutation);
        drop(lifecycle);
        drop(terminal);
        drop(control);

        let input_result = (|| {
            loop {
                let frame = match AttachFrame::read_from(&mut stream) {
                    Ok(frame) => frame,
                    Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => return Ok(()),
                    Err(error) => return Err(error),
                };
                if matches!(frame, AttachFrame::ReconnectAck) {
                    let acknowledge = {
                        let mut controller = lock(&runtime.controller)?;
                        controller
                            .as_mut()
                            .filter(|controller| controller.token == token)
                            .and_then(|controller| controller.reconnect_ack.take())
                    };
                    if let Some(acknowledge) = acknowledge {
                        let _ = acknowledge.send(());
                    }
                    return Ok(());
                }
                if let AttachFrame::Input(bytes) = frame {
                    if !Self::write_controller_input(&runtime, &token, &bytes)? {
                        return Ok(());
                    }
                    continue;
                }
                if matches!(frame, AttachFrame::FocusGained) {
                    if !registry.record_focus_gained(
                        response_version,
                        &shell,
                        &attached_run,
                        &runtime,
                        &token,
                    )? {
                        return Ok(());
                    }
                    continue;
                }
                let control = lock(&runtime.control)?;
                let authorized = lock(&runtime.controller)?
                    .as_ref()
                    .is_some_and(|controller| controller.token == token);
                if !authorized {
                    return Ok(());
                }
                let result = match frame {
                    AttachFrame::Input(_) => unreachable!(),
                    AttachFrame::Resize {
                        rows,
                        cols,
                        pixel_width,
                        pixel_height,
                    } => {
                        if let Err(error) = validate_terminal_dimensions(rows, cols) {
                            Err(error)
                        } else {
                            let size = PtySize {
                                rows,
                                cols,
                                pixel_width,
                                pixel_height,
                            };
                            lock(&runtime.master)?.resize(size)?;
                            lock(&runtime.terminal)?.resize(rows, cols);
                            Self::update_runtime_dimensions(&shell, &runtime, size)?;
                            Ok(())
                        }
                    }
                    AttachFrame::Detached => return Ok(()),
                    AttachFrame::FocusGained => unreachable!(),
                    AttachFrame::Output(_) | AttachFrame::Reconnect | AttachFrame::ReconnectAck => {
                        Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            "client sent a daemon-only attach frame",
                        ))
                    }
                };
                drop(control);
                result?;
            }
        })();
        let release_result = Self::release_controller(&runtime, &token);
        let _ = stream.shutdown(std::net::Shutdown::Both);
        let output_result = output_worker
            .join()
            .map_err(|_| io::Error::other("attachment output thread panicked"));
        release_result?;
        output_result?;
        input_result
    }

    fn write_controller_input(
        runtime: &ShellRuntime,
        token: &str,
        bytes: &[u8],
    ) -> io::Result<bool> {
        let mut offset = 0;
        while offset < bytes.len() {
            let control = lock(&runtime.control)?;
            let authorized = lock(&runtime.controller)?
                .as_ref()
                .is_some_and(|controller| controller.token == token);
            if !authorized {
                return Ok(false);
            }
            let result = lock(&runtime.master)?.write(&bytes[offset..]);
            drop(control);
            match result {
                Ok(0) => return Err(io::Error::new(io::ErrorKind::WriteZero, "PTY input closed")),
                Ok(count) => offset += count,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    thread::sleep(IO_RETRY_DELAY);
                }
                Err(error) => return Err(error),
            }
        }
        Ok(true)
    }

    fn profile_size(profile: &TerminalProfile) -> PtySize {
        PtySize {
            rows: profile.rows,
            cols: profile.cols,
            pixel_width: profile.pixel_width,
            pixel_height: profile.pixel_height,
        }
    }

    fn update_runtime_dimensions(
        shell: &Shell,
        runtime: &Arc<ShellRuntime>,
        size: PtySize,
    ) -> io::Result<()> {
        let mut lifecycle = lock(&shell.lifecycle)?;
        let profile = match &mut *lifecycle {
            ShellLifecycle::Running {
                profile,
                runtime: current,
                ..
            } if Arc::ptr_eq(current, runtime) => profile,
            _ => return Ok(()),
        };
        profile.rows = size.rows;
        profile.cols = size.cols;
        profile.pixel_width = size.pixel_width;
        profile.pixel_height = size.pixel_height;
        if let Some(last_run) = lock(&shell.last_run)?.as_mut() {
            last_run.profile = profile.clone();
        }
        Ok(())
    }
}

fn validate_terminal_profile(profile: &TerminalProfile) -> io::Result<()> {
    validate_terminal_dimensions(profile.rows, profile.cols)?;
    for value in [
        &profile.term,
        &profile.colorterm,
        &profile.term_program,
        &profile.term_program_version,
    ]
    .into_iter()
    .flatten()
    {
        if value.is_empty()
            || value.len() > MAX_TERMINAL_ENV_VALUE
            || value.chars().any(char::is_control)
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "terminal profile contains an invalid environment value",
            ));
        }
    }
    Ok(())
}

fn validate_unix_environment(environment: &UnixEnvironment) -> io::Result<()> {
    let mut names = HashSet::new();
    for variable in &environment.variables {
        if variable.name.is_empty()
            || variable.name.contains(&0)
            || variable.name.contains(&b'=')
            || !names.insert(variable.name.as_slice())
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "client environment contains an invalid variable name",
            ));
        }
        if variable.value.contains(&0) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "client environment contains an invalid variable value",
            ));
        }
    }
    Ok(())
}

fn validate_terminal_dimensions(rows: u16, cols: u16) -> io::Result<()> {
    if rows == 0
        || cols == 0
        || rows > MAX_TERMINAL_ROWS
        || cols > MAX_TERMINAL_COLS
        || usize::from(rows) * usize::from(cols) > MAX_TERMINAL_CELLS
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "terminal rows and columns must be nonzero and at most {MAX_TERMINAL_ROWS}x{MAX_TERMINAL_COLS}"
            ),
        ));
    }
    Ok(())
}

fn term_mismatch_warning(started: Option<&str>, attached: Option<&str>) -> Option<String> {
    (started != attached).then(|| {
        format!(
            "shell started with TERM={started:?}; attachment reports TERM={attached:?}; process environment is unchanged"
        )
    })
}

fn validate_name(name: &str) -> io::Result<()> {
    if name.trim().is_empty() || name.len() > MAX_NAME_BYTES || name.chars().any(char::is_control) {
        Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "name must be nonempty, contain no control characters, and be at most {MAX_NAME_BYTES} bytes"
            ),
        ))
    } else {
        Ok(())
    }
}

fn validate_persisted_name(name: &str) -> io::Result<()> {
    if name.trim().is_empty() {
        Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "persisted name cannot be empty",
        ))
    } else {
        Ok(())
    }
}

fn unix_time_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

fn signal_session(session_id: libc::pid_t, signal: libc::c_int) {
    for pid in session_processes(session_id) {
        let Ok(pidfd) = open_pidfd(pid as u32) else {
            continue;
        };
        let Ok(stat) = fs::read_to_string(format!("/proc/{pid}/stat")) else {
            continue;
        };
        if proc_session_id(&stat) == Some(session_id) {
            let _ = send_pidfd_signal(pidfd.as_fd(), signal);
        }
    }
}

fn session_processes(session_id: libc::pid_t) -> Vec<libc::pid_t> {
    let Ok(entries) = fs::read_dir("/proc") else {
        return Vec::new();
    };
    let mut processes = Vec::new();
    for entry in entries.flatten() {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<libc::pid_t>().ok())
        else {
            continue;
        };
        let Ok(stat) = fs::read_to_string(entry.path().join("stat")) else {
            continue;
        };
        if proc_session_id(&stat) == Some(session_id) {
            processes.push(pid);
        }
    }
    processes
}

fn wait_for_session_descendants(session_id: libc::pid_t) -> io::Result<()> {
    let deadline = Instant::now() + HANDSHAKE_TIMEOUT;
    while Instant::now() < deadline {
        let has_descendants = session_processes(session_id)
            .into_iter()
            .any(|pid| pid != session_id);
        if !has_descendants {
            return Ok(());
        }
        // Repeat the final pass so descendants forked during an earlier scan
        // cannot escape session cleanup.
        signal_session(session_id, libc::SIGKILL);
        thread::sleep(IO_RETRY_DELAY);
    }
    Err(io::Error::new(
        io::ErrorKind::TimedOut,
        format!("session {session_id} still has descendant processes"),
    ))
}

fn proc_session_id(stat: &str) -> Option<libc::pid_t> {
    stat.rsplit_once(')')?
        .1
        .split_whitespace()
        .nth(3)?
        .parse()
        .ok()
}

fn proc_foreground_process_group(stat: &str) -> Option<libc::pid_t> {
    stat.rsplit_once(')')?
        .1
        .split_whitespace()
        .nth(5)?
        .parse()
        .ok()
}

fn foreground_process_for_session_leader(pid: u32) -> Option<String> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let process_group = proc_foreground_process_group(&stat)?;
    (process_group > 0)
        .then(|| read_process_name(process_group as u32))
        .flatten()
}

fn read_process_name(pid: u32) -> Option<String> {
    let file = File::open(format!("/proc/{pid}/comm")).ok()?;
    let mut bytes = Vec::with_capacity(MAX_FOREGROUND_PROCESS_BYTES + 1);
    file.take((MAX_FOREGROUND_PROCESS_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .ok()?;
    parse_process_name(&bytes)
}

fn validate_shell_specs(specs: &[ShellSpec]) -> io::Result<()> {
    let mut names = HashSet::with_capacity(specs.len());
    for spec in specs {
        validate_name(&spec.name)?;
        if !names.insert(&spec.name) {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!("duplicate shell name: {}", spec.name),
            ));
        }
    }
    Ok(())
}

fn validate_launcher_command(command: &[String]) -> io::Result<()> {
    if command
        .first()
        .is_some_and(|executable| !executable.is_empty())
    {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "workspace launcher command requires a non-empty executable",
        ))
    }
}

fn validate_cwd(cwd: &Path) -> io::Result<()> {
    if cwd.is_absolute() && cwd.is_dir() {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "working directory must be an existing absolute directory: {}",
                cwd.display()
            ),
        ))
    }
}

fn validate_persisted_cwd(cwd: &Path) -> io::Result<()> {
    if cwd.is_absolute() {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "persisted working directory must be absolute: {}",
                cwd.display()
            ),
        ))
    }
}

fn schedule_validation_error(error: crate::scheduling::SchedulingError) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, error.to_string())
}

fn validate_schedule_spec(spec: &AgentScheduleSpec) -> io::Result<()> {
    crate::scheduling::validate_prompt(&spec.prompt).map_err(schedule_validation_error)?;
    crate::scheduling::validate_integration_key(&spec.integration)
        .map_err(schedule_validation_error)?;
    if let AgentScheduleSession::Continue {
        external_session_id,
    } = &spec.session
    {
        crate::scheduling::validate_external_session_id(external_session_id)
            .map_err(schedule_validation_error)?;
    }
    if spec.overlap_policy != AgentScheduleOverlapPolicy::Skip {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "agent schedule overlap policy must be skip",
        ));
    }
    Ok(())
}

fn validate_schedule_capability(
    integration: &str,
    session: &AgentScheduleSession,
) -> io::Result<()> {
    let capability = crate::integrations::by_key(integration)
        .and_then(|descriptor| descriptor.schedule_dispatch)
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "integration does not support scheduled dispatch",
            )
        })?;
    let supported = match session {
        AgentScheduleSession::Fresh => capability.fresh,
        AgentScheduleSession::Continue { .. } => capability.continuation,
    };
    if !supported {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "integration does not support the requested scheduled session mode",
        ));
    }
    Ok(())
}

fn validate_persisted_schedule(schedule: &PersistedAgentSchedule) -> io::Result<()> {
    validate_name(&schedule.name)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error.to_string()))?;
    validate_persisted_cwd(&schedule.cwd)?;
    crate::scheduling::validate_prompt(&schedule.prompt)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error.to_string()))?;
    crate::scheduling::validate_integration_key(&schedule.integration)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error.to_string()))?;
    if let AgentScheduleSession::Continue {
        external_session_id,
    } = &schedule.session
    {
        crate::scheduling::validate_external_session_id(external_session_id)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error.to_string()))?;
    }
    for timestamp in [
        schedule.created_at_ms,
        schedule.updated_at_ms,
        schedule.evaluation_frontier_ms,
    ] {
        crate::scheduling::validate_timestamp_ms(timestamp)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error.to_string()))?;
    }
    let cron = crate::scheduling::canonicalize_cron(&schedule.trigger.cron)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error.to_string()))?;
    let timezone = crate::scheduling::canonicalize_timezone(&schedule.trigger.timezone)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error.to_string()))?;
    crate::scheduling::CronSchedule::compile(&cron, &timezone)
        .and_then(|compiled| {
            compiled.ensure_possible()?;
            if schedule.state == AgentScheduleState::Enabled {
                compiled.next_after_ms(schedule.evaluation_frontier_ms)?;
            }
            Ok(())
        })
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error.to_string()))?;
    if cron != schedule.trigger.cron
        || timezone != schedule.trigger.timezone
        || schedule.overlap_policy != AgentScheduleOverlapPolicy::Skip
        || schedule.revision == 0
        || schedule.prompt_revision == 0
        || schedule.trigger_revision == 0
        || schedule.prompt_revision > schedule.revision
        || schedule.trigger_revision > schedule.revision
        || schedule.updated_at_ms < schedule.created_at_ms
        || schedule.evaluation_frontier_ms < schedule.created_at_ms
        || schedule.evaluation_frontier_trigger_revision != schedule.trigger_revision
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Boomux state contains an invalid agent schedule",
        ));
    }
    if schedule.dispatch_key_filter.len() != DISPATCH_KEY_FILTER_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Boomux state contains an invalid dispatch idempotency filter",
        ));
    }
    if let Some(shell_id) = &schedule.execution_shell_id {
        validate_id("schedule execution shell", shell_id)?;
    }
    let mut execution_ids = HashSet::new();
    let mut dispatch_keys = HashSet::new();
    let mut nonterminal_count = 0;
    for execution in &schedule.executions {
        validate_id("scheduled execution", &execution.id)?;
        validate_id("scheduled execution dispatch key", &execution.dispatch_key)?;
        validate_id("scheduled execution runner token", &execution.runner_token)?;
        validate_persisted_cwd(&execution.cwd)?;
        crate::scheduling::validate_prompt(&execution.prompt)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error.to_string()))?;
        for timestamp in [
            Some(execution.requested_at_ms),
            execution.scheduled_at_ms,
            execution.coalesced_through_ms,
            execution.started_at_ms,
            execution.ended_at_ms,
        ]
        .into_iter()
        .flatten()
        {
            crate::scheduling::validate_timestamp_ms(timestamp)
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error.to_string()))?;
        }
        if !execution_ids.insert(&execution.id)
            || !dispatch_keys.insert(&execution.dispatch_key)
            || !dispatch_key_was_seen(&schedule.dispatch_key_filter, &execution.dispatch_key)
            || execution.schedule_revision == 0
            || execution.prompt_revision == 0
            || execution.trigger_revision == 0
            || execution.prompt_revision > execution.schedule_revision
            || execution.trigger_revision > execution.schedule_revision
            || execution.revision == 0
            || execution.schedule_revision > schedule.revision
            || execution.prompt_revision > schedule.prompt_revision
            || execution.trigger_revision > schedule.trigger_revision
            || execution.cwd != schedule.cwd
            || execution.integration != schedule.integration
            || execution.session != schedule.session
            || match execution.dispatch_kind {
                ScheduledExecutionDispatchKind::Manual => {
                    execution.scheduled_at_ms.is_some() || execution.coalesced_through_ms.is_some()
                }
                ScheduledExecutionDispatchKind::Timed => {
                    execution.scheduled_at_ms.is_none()
                        || execution
                            .scheduled_at_ms
                            .is_some_and(|scheduled| scheduled > execution.requested_at_ms)
                        || execution
                            .scheduled_at_ms
                            .is_some_and(|scheduled| scheduled > schedule.evaluation_frontier_ms)
                        || execution.coalesced_through_ms.is_some_and(|through| {
                            through < execution.scheduled_at_ms.unwrap_or(through)
                                || through > schedule.evaluation_frontier_ms
                        })
                        || execution.scheduled_at_ms.is_some_and(|scheduled_at_ms| {
                            execution.id
                                != timed_execution_id(
                                    &schedule.id,
                                    execution.trigger_revision,
                                    scheduled_at_ms,
                                )
                                || execution.dispatch_key
                                    != timed_dispatch_key(
                                        &schedule.id,
                                        execution.trigger_revision,
                                        scheduled_at_ms,
                                    )
                        })
                }
            }
            || execution
                .started_at_ms
                .is_some_and(|time| time < execution.requested_at_ms)
            || execution
                .ended_at_ms
                .is_some_and(|time| time < execution.requested_at_ms)
            || execution
                .started_at_ms
                .zip(execution.ended_at_ms)
                .is_some_and(|(started, ended)| ended < started)
            || execution.shell_id.is_some() != execution.run_id.is_some()
                && execution.run_id.is_some()
            || execution.agent_id.is_some() != execution.external_session_id.is_some()
            || execution.coalesced_through_ms.is_some()
                && !(execution.dispatch_kind == ScheduledExecutionDispatchKind::Timed
                    && execution.state == ScheduledExecutionState::Skipped
                    && matches!(
                        execution.reason,
                        Some(
                            ScheduledExecutionReason::Missed | ScheduledExecutionReason::PausedRace
                        )
                    ))
            || matches!(
                execution.reason,
                Some(ScheduledExecutionReason::Missed | ScheduledExecutionReason::PausedRace)
            ) && !(execution.dispatch_kind == ScheduledExecutionDispatchKind::Timed
                && execution.state == ScheduledExecutionState::Skipped)
            || matches!(
                &execution.session,
                AgentScheduleSession::Continue { external_session_id }
                    if execution
                        .external_session_id
                        .as_deref()
                        .is_some_and(|linked| linked != external_session_id)
            )
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Boomux state contains an invalid scheduled execution",
            ));
        }
        if let Some(shell_id) = &execution.shell_id {
            validate_id("scheduled execution shell", shell_id)?;
        }
        if let Some(run_id) = &execution.run_id {
            validate_id("scheduled execution run", run_id)?;
        }
        if let Some(agent_id) = &execution.agent_id {
            validate_id("scheduled execution agent", agent_id)?;
        }
        let valid_state = match execution.state {
            ScheduledExecutionState::Skipped => {
                execution.started_at_ms.is_none()
                    && execution.ended_at_ms.is_some()
                    && execution.outcome.is_none()
                    && execution.shell_id.is_none()
                    && execution.run_id.is_none()
                    && execution.agent_id.is_none()
                    && matches!(
                        execution.reason,
                        Some(
                            ScheduledExecutionReason::Overlap
                                | ScheduledExecutionReason::ActiveSession
                                | ScheduledExecutionReason::WorkspaceCapacity
                                | ScheduledExecutionReason::GlobalCapacity
                                | ScheduledExecutionReason::Missed
                                | ScheduledExecutionReason::PausedRace
                                | ScheduledExecutionReason::InvalidTarget
                        )
                    )
            }
            ScheduledExecutionState::Claimed => {
                execution.started_at_ms.is_none()
                    && execution.ended_at_ms.is_none()
                    && execution.run_id.is_none()
                    && execution.reason.is_none()
                    && execution.outcome.is_none()
                    && execution.agent_id.is_none()
            }
            ScheduledExecutionState::Starting => {
                execution.run_id.is_some()
                    && execution.started_at_ms.is_none()
                    && execution.ended_at_ms.is_none()
                    && execution.outcome.is_none()
                    && matches!(
                        execution.reason,
                        None | Some(ScheduledExecutionReason::HostSpawnFailed)
                    )
            }
            ScheduledExecutionState::Active => {
                execution.run_id.is_some()
                    && execution.started_at_ms.is_some()
                    && execution.ended_at_ms.is_none()
                    && execution.reason.is_none()
            }
            ScheduledExecutionState::DispatchFailed => {
                execution.ended_at_ms.is_some()
                    && execution.outcome.is_none()
                    && matches!(
                        execution.reason,
                        Some(
                            ScheduledExecutionReason::RunnerStartFailed
                                | ScheduledExecutionReason::HostSpawnFailed
                        )
                    )
            }
            ScheduledExecutionState::Exited => {
                execution.started_at_ms.is_some()
                    && execution.ended_at_ms.is_some()
                    && execution.outcome.is_some()
                    && execution.reason.is_none()
            }
            ScheduledExecutionState::Cancelled => {
                execution.ended_at_ms.is_some()
                    && execution.outcome.is_none()
                    && matches!(
                        execution.reason,
                        Some(
                            ScheduledExecutionReason::CancelledByUser
                                | ScheduledExecutionReason::DaemonShutdown
                        )
                    )
            }
            ScheduledExecutionState::Interrupted => {
                execution.ended_at_ms.is_some()
                    && execution.outcome.is_none()
                    && matches!(
                        execution.reason,
                        Some(
                            ScheduledExecutionReason::ColdDaemonRecovery
                                | ScheduledExecutionReason::RunnerExitedWithoutReport
                        )
                    )
            }
        };
        if !valid_state {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Boomux state contains an invalid scheduled execution state",
            ));
        }
        nonterminal_count += usize::from(!execution.state.is_terminal());
    }
    if nonterminal_count > 1 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Boomux state contains overlapping scheduled executions",
        ));
    }
    Ok(())
}

fn validate_id(kind: &str, id: &str) -> io::Result<()> {
    Uuid::parse_str(id).map(|_| ()).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("persisted {kind} ID is invalid: {id}"),
        )
    })
}

fn dispatch_key_positions(key: &str, bit_count: usize) -> [usize; 3] {
    let digest = Sha256::digest(key.as_bytes());
    std::array::from_fn(|index| {
        let offset = index * 8;
        let value = u64::from_be_bytes(
            digest[offset..offset + 8]
                .try_into()
                .expect("SHA-256 position is eight bytes"),
        );
        usize::try_from(value % bit_count as u64).unwrap_or(0)
    })
}

fn dispatch_key_was_seen(filter: &[u8], key: &str) -> bool {
    let bit_count = filter.len().saturating_mul(8);
    bit_count != 0
        && dispatch_key_positions(key, bit_count)
            .into_iter()
            .all(|position| filter[position / 8] & (1 << (position % 8)) != 0)
}

fn remember_dispatch_key(filter: &mut [u8], key: &str) {
    let bit_count = filter.len().saturating_mul(8);
    for position in dispatch_key_positions(key, bit_count) {
        filter[position / 8] |= 1 << (position % 8);
    }
}

fn timed_execution_id(schedule_id: &str, trigger_revision: u64, scheduled_at_ms: u64) -> String {
    Uuid::new_v5(
        &Uuid::NAMESPACE_OID,
        format!("boomux:timed-execution:{schedule_id}:{trigger_revision}:{scheduled_at_ms}")
            .as_bytes(),
    )
    .to_string()
}

fn timed_dispatch_key(schedule_id: &str, trigger_revision: u64, scheduled_at_ms: u64) -> String {
    Uuid::new_v5(
        &Uuid::NAMESPACE_OID,
        format!("boomux:timed-dispatch:{schedule_id}:{trigger_revision}:{scheduled_at_ms}")
            .as_bytes(),
    )
    .to_string()
}

fn prune_terminal_executions(executions: &mut Vec<Arc<ScheduledExecution>>) {
    let mut terminal = executions
        .iter()
        .enumerate()
        .filter_map(|(index, candidate)| {
            lock(&candidate.state)
                .ok()
                .filter(|state| state.state.is_terminal())
                .map(|_| (index, candidate.requested_at_ms, candidate.id.as_str()))
        })
        .collect::<Vec<_>>();
    terminal.sort_by(
        |(_, left_requested, left_id), (_, right_requested, right_id)| {
            left_requested
                .cmp(right_requested)
                .then_with(|| left_id.cmp(right_id))
        },
    );
    let remove = terminal
        .len()
        .saturating_sub(MAX_TERMINAL_EXECUTIONS_PER_SCHEDULE);
    let mut remove = terminal
        .into_iter()
        .take(remove)
        .map(|(index, _, _)| index)
        .collect::<Vec<_>>();
    remove.sort_unstable_by(|left, right| right.cmp(left));
    for index in remove {
        executions.remove(index);
    }
}

fn validate_agent_registration(spec: &AgentRegistrationSpec) -> io::Result<()> {
    validate_name(&spec.name)?;
    validate_required_agent_string("integration", &spec.integration, MAX_NAME_BYTES)?;
    if let Some(external_session_id) = &spec.external_session_id {
        validate_required_agent_string("external_session_id", external_session_id, MAX_NAME_BYTES)?;
    }
    validate_agent_report(&spec.report)
}

fn validate_agent_report(report: &AgentReport) -> io::Result<()> {
    validate_required_agent_string("evidence", &report.evidence, MAX_AGENT_EVIDENCE_BYTES)?;
    if report.confidence > 100 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "agent report confidence must be between 0 and 100",
        ));
    }
    Ok(())
}

fn validate_external_agent_authority(authority: AgentAuthority) -> io::Result<()> {
    if authority == AgentAuthority::DaemonLifecycle {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "daemon_lifecycle authority is reserved for daemon observations",
        ));
    }
    Ok(())
}

fn observation_matches_report(
    observation: &AgentObservationSnapshot,
    report: &AgentReport,
) -> bool {
    observation.state == report.state
        && observation.authority == report.authority
        && observation.evidence == report.evidence
        && observation.confidence == report.confidence
}

fn attention_for_observation(
    observation: &AgentObservationSnapshot,
) -> Option<AgentAttentionSnapshot> {
    let reason = match observation.state {
        AgentState::Blocked => AgentAttentionReason::Blocked,
        AgentState::Done => AgentAttentionReason::Completed,
        _ => return None,
    };
    Some(AgentAttentionSnapshot {
        reason,
        observation: observation.clone(),
    })
}

fn agent_authority_rank(authority: AgentAuthority) -> u8 {
    match authority {
        AgentAuthority::LifecycleIntegration => 3,
        AgentAuthority::ProcessAdapter => 2,
        AgentAuthority::TerminalHeuristic => 1,
        AgentAuthority::DaemonLifecycle => 4,
    }
}

fn validate_required_agent_string(kind: &str, value: &str, max_bytes: usize) -> io::Result<()> {
    if value.trim().is_empty() || value.len() > max_bytes {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("agent {kind} must be nonempty and at most {max_bytes} bytes"),
        ));
    }
    Ok(())
}

fn validate_persisted_agent(agent: &PersistedAgentInstance) -> io::Result<()> {
    validate_persisted_name(&agent.name)?;
    validate_agent_registration(&AgentRegistrationSpec {
        name: agent.name.clone(),
        integration: agent.integration.clone(),
        external_session_id: agent.external_session_id.clone(),
        report: AgentReport {
            state: agent.observation.state,
            authority: agent.observation.authority,
            evidence: agent.observation.evidence.clone(),
            confidence: agent.observation.confidence,
        },
    })
    .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error.to_string()))?;
    if let Some(cwd) = &agent.cwd {
        validate_persisted_cwd(cwd)?;
    }
    if agent.observation.revision == 0
        || agent.observation.observed_at_ms < agent.started_at_ms
        || agent
            .ended_at_ms
            .is_some_and(|ended_at_ms| ended_at_ms < agent.started_at_ms)
        || (agent.observation.state == AgentState::Done) != agent.ended_at_ms.is_some()
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Boomux state contains an invalid agent observation",
        ));
    }
    if let Some(attention) = &agent.attention {
        validate_agent_report(&AgentReport {
            state: attention.observation.state,
            authority: attention.observation.authority,
            evidence: attention.observation.evidence.clone(),
            confidence: attention.observation.confidence,
        })
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error.to_string()))?;
        let expected_reason = match attention.observation.state {
            AgentState::Blocked => AgentAttentionReason::Blocked,
            AgentState::Done => AgentAttentionReason::Completed,
            _ => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "Boomux state contains attention for a non-attention agent state",
                ));
            }
        };
        if attention.reason != expected_reason
            || attention.observation.revision == 0
            || attention.observation.revision > agent.observation.revision
            || attention.observation.observed_at_ms < agent.started_at_ms
            || attention.observation.observed_at_ms > agent.observation.observed_at_ms
            || (attention.observation.revision == agent.observation.revision
                && attention.observation != agent.observation)
            || (attention.reason == AgentAttentionReason::Completed
                && attention.observation != agent.observation)
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Boomux state contains invalid agent attention",
            ));
        }
    }
    Ok(())
}

fn not_found(kind: &str, id: &str) -> io::Error {
    io::Error::new(io::ErrorKind::NotFound, format!("{kind} not found: {id}"))
}

fn parse_process_name(bytes: &[u8]) -> Option<String> {
    let name = String::from_utf8_lossy(bytes);
    let mut sanitized = String::new();
    for character in name.trim().chars() {
        let character = if character.is_control() {
            '?'
        } else {
            character
        };
        if sanitized.len() + character.len_utf8() > MAX_FOREGROUND_PROCESS_BYTES {
            break;
        }
        sanitized.push(character);
    }
    (!sanitized.is_empty()).then_some(sanitized)
}

fn lock<T>(mutex: &Mutex<T>) -> io::Result<MutexGuard<'_, T>> {
    mutex
        .lock()
        .map_err(|_| io::Error::other("daemon state lock poisoned"))
}

#[cfg(test)]
mod tests {
    use super::*;

    struct FixedSchedulerClock(AtomicU64);

    impl SchedulerClock for FixedSchedulerClock {
        fn now_ms(&self) -> u64 {
            self.0.load(Ordering::Acquire)
        }
    }

    fn set_scheduler_time(registry: &DaemonService, now_ms: u64) {
        *lock(&registry.clock).unwrap() = Arc::new(FixedSchedulerClock(AtomicU64::new(now_ms)));
    }
    use std::sync::Barrier;

    use crate::protocol::{AgentAuthority, AgentState};

    trait ShellTestControl {
        fn kill(&self) -> io::Result<()>;
    }

    impl ShellTestControl for Shell {
        fn kill(&self) -> io::Result<()> {
            ShellRuntimeManager::default().kill(self)
        }
    }

    trait ReaderTestControl {
        fn pause_reader(&self) -> io::Result<()>;
        fn resume_reader(&self) -> io::Result<()>;
    }

    impl ReaderTestControl for ShellRuntime {
        fn pause_reader(&self) -> io::Result<()> {
            ShellRuntimeManager::default().pause_reader(self)
        }

        fn resume_reader(&self) -> io::Result<()> {
            ShellRuntimeManager::default().resume_reader(self)
        }
    }

    fn spawn_runtime(
        shell: &Arc<Shell>,
        run: &ShellRun,
        workspace_name: &str,
        shell_name: &str,
        profile: &TerminalProfile,
        environment: Option<&UnixEnvironment>,
        recovery: RuntimeRecovery<'_>,
    ) -> io::Result<(Arc<ShellRuntime>, PtyReader)> {
        ShellRuntimeManager::default().spawn_runtime(
            shell,
            run,
            RuntimeStart {
                workspace_name,
                shell_name,
                profile,
                environment,
                recovery,
            },
        )
    }

    fn start_pty_reader(
        registry: Weak<DaemonService>,
        shell: Arc<Shell>,
        run: Arc<ShellRun>,
        runtime: Arc<ShellRuntime>,
        reader: PtyReader,
        start_paused: bool,
    ) -> io::Result<()> {
        ShellRuntimeManager::default().start_pty_reader(
            registry,
            shell,
            run,
            runtime,
            reader,
            start_paused,
        )
    }

    #[derive(Default)]
    struct RecordingNotificationSink {
        requests: Mutex<Vec<NotificationRequest>>,
    }

    impl NotificationSink for RecordingNotificationSink {
        fn notify(&self, request: NotificationRequest) {
            self.requests.lock().unwrap().push(request);
        }
    }

    fn notification_registry(
        settings: NotificationSettings,
    ) -> (DaemonService, Arc<RecordingNotificationSink>) {
        let sink = Arc::new(RecordingNotificationSink::default());
        let registry = DaemonService {
            notification_settings: NotificationDeliverySettings {
                desktop: settings,
                ..Default::default()
            },
            notification_sink: sink.clone(),
            ..DaemonService::default()
        };
        (registry, sink)
    }

    fn profile() -> TerminalProfile {
        TerminalProfile {
            term: Some("xterm-256color".into()),
            colorterm: Some("truecolor".into()),
            term_program: Some("test".into()),
            term_program_version: Some("1".into()),
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        }
    }

    #[test]
    fn new_shell_terminal_starts_with_its_workspace_and_shell_name() {
        let terminal = initial_terminal_state(24, 80, "project", "build", None);

        assert!(terminal.plain_text().contains("Boomux: project/build"));
    }

    #[test]
    fn cold_terminal_history_is_presented_before_the_new_run_banner() {
        let terminal = initial_terminal_state(24, 80, "project", "agent", Some("old output\n"));
        let text = terminal.plain_text();

        assert!(text.contains("restored bounded history from previous run\nold output"));
        assert!(text.find("old output").unwrap() < text.find("Boomux: project/agent").unwrap());
    }

    fn add_recovery_agent(
        registry: &DaemonService,
        shell: &Shell,
        run_id: &str,
        integration: &str,
        external_session_id: &str,
    ) {
        let agent = Arc::new(AgentInstance {
            id: Uuid::new_v4().to_string(),
            workspace_id: shell.workspace_id.clone(),
            shell_id: shell.id.clone(),
            run_id: run_id.into(),
            name: integration.into(),
            integration: integration.into(),
            external_session_id: Some(external_session_id.into()),
            cwd: Some(shell.cwd.clone()),
            started_at_ms: 1,
            state: Mutex::new(AgentInstanceState {
                ended_at_ms: None,
                observation: AgentObservationSnapshot {
                    revision: 1,
                    state: AgentState::Working,
                    authority: AgentAuthority::LifecycleIntegration,
                    evidence: "active before interruption".into(),
                    confidence: 100,
                    observed_at_ms: 1,
                },
                attention: None,
            }),
        });
        lock(&registry.durable.state)
            .unwrap()
            .agents
            .insert(agent.id.clone(), agent);
    }

    fn recovery_shell(
        registry: &DaemonService,
        command: Vec<String>,
    ) -> (Arc<Shell>, PersistedShellRun) {
        let workspace = registry
            .create_workspace(
                "recovery".into(),
                vec![ShellSpec {
                    name: "agent".into(),
                    command,
                    cwd: env::temp_dir(),
                }],
            )
            .unwrap();
        let shell = registry.shell(&workspace.shells[0].id).unwrap();
        let run = PersistedShellRun {
            id: Uuid::new_v4().to_string(),
            generation: 1,
            started_at_ms: 1,
            ended_at_ms: Some(2),
            exit_reason: Some(ShellRunExitReason::Interrupted),
            output_revision: 1,
            environment_has_run_id: true,
            profile: profile(),
            terminal_history: None,
        };
        *lock(&shell.last_run).unwrap() = Some(run.clone());
        (shell, run)
    }

    #[test]
    fn interrupted_authoritative_agent_builds_native_resume_command() {
        let registry = DaemonService::default();
        let (shell, run) = recovery_shell(&registry, vec!["/opt/bin/opencode".into()]);
        add_recovery_agent(&registry, &shell, &run.id, "opencode", "session-1");

        assert_eq!(
            registry.agent_resume_command(&shell, Some(&run)).unwrap(),
            Some(vec![
                "/opt/bin/opencode".into(),
                "--session".into(),
                "session-1".into()
            ])
        );

        let agent = lock(&registry.durable.state)
            .unwrap()
            .agents
            .values()
            .next()
            .unwrap()
            .clone();
        lock(&agent.state).unwrap().observation.authority = AgentAuthority::TerminalHeuristic;
        assert!(
            registry
                .agent_resume_command(&shell, Some(&run))
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn recovery_falls_back_when_agent_identity_is_ambiguous_or_disabled() {
        let mut registry = DaemonService::default();
        let (shell, run) = recovery_shell(&registry, Vec::new());
        add_recovery_agent(&registry, &shell, &run.id, "pi", "session-1");
        add_recovery_agent(&registry, &shell, &run.id, "pi", "session-2");

        assert!(
            registry
                .agent_resume_command(&shell, Some(&run))
                .unwrap()
                .is_none()
        );

        lock(&registry.durable.state)
            .unwrap()
            .agents
            .retain(|_, agent| agent.external_session_id.as_deref() == Some("session-1"));
        registry.notification_settings.resume_agents = false;
        assert!(
            registry
                .agent_resume_command(&shell, Some(&run))
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn disabling_history_persistence_clears_retained_history() {
        let registry = DaemonService::default();
        let (shell, _) = recovery_shell(&registry, Vec::new());
        lock(&shell.last_run)
            .unwrap()
            .as_mut()
            .unwrap()
            .terminal_history = Some("secret output".into());

        registry.clear_terminal_histories().unwrap();

        assert!(
            lock(&shell.last_run)
                .unwrap()
                .as_ref()
                .unwrap()
                .terminal_history
                .is_none()
        );
    }

    fn agent_spec(state: AgentState) -> AgentRegistrationSpec {
        AgentRegistrationSpec {
            name: "test-agent".into(),
            integration: "daemon-test".into(),
            external_session_id: Some("external-1".into()),
            report: AgentReport {
                state,
                authority: AgentAuthority::LifecycleIntegration,
                evidence: "test observation".into(),
                confidence: 90,
            },
        }
    }

    fn agent_report(state: AgentState, authority: AgentAuthority, evidence: &str) -> AgentReport {
        AgentReport {
            state,
            authority,
            evidence: evidence.into(),
            confidence: 90,
        }
    }

    fn running_shell(
        registry: &DaemonService,
    ) -> (WorkspaceSnapshot, Arc<Shell>, Arc<ShellRuntime>) {
        let workspace = registry
            .create_workspace(
                "agents".into(),
                vec![ShellSpec {
                    name: "agent-shell".into(),
                    command: vec!["/bin/sleep".into(), "30".into()],
                    cwd: env::temp_dir(),
                }],
            )
            .unwrap();
        let shell = registry.shell(&workspace.shells[0].id).unwrap();
        let run = Arc::new(ShellRun::new(1));
        let (runtime, _reader) = spawn_runtime(
            &shell,
            &run,
            "agents",
            "agent-shell",
            &profile(),
            None,
            RuntimeRecovery::default(),
        )
        .unwrap();
        *lock(&shell.last_run).unwrap() = Some(run.persisted(profile()).unwrap());
        *lock(&shell.lifecycle).unwrap() = ShellLifecycle::Running {
            profile: profile(),
            run,
            runtime: Arc::clone(&runtime),
        };
        (workspace, shell, runtime)
    }

    fn install_test_controller(runtime: &ShellRuntime, token: &str) {
        let (connection, _peer) = UnixStream::pair().unwrap();
        let (output, _receiver) = mpsc::sync_channel(1);
        *lock(&runtime.controller).unwrap() = Some(Controller {
            token: token.into(),
            output,
            connection,
            reconnect_ack: None,
        });
    }

    #[test]
    fn empty_registry_has_empty_snapshot() {
        assert!(
            DaemonService::default()
                .snapshot()
                .unwrap()
                .workspaces
                .is_empty()
        );
    }

    #[test]
    fn scheduler_health_tracks_running_stopped_and_panicked_workers() {
        let registry = Arc::new(DaemonService::default());
        assert_eq!(
            registry.snapshot().unwrap().scheduler.unwrap().state,
            SchedulerState::Offline
        );
        registry.start_scheduler().unwrap();
        for _ in 0..100 {
            if registry.snapshot().unwrap().scheduler.unwrap().state == SchedulerState::Active {
                break;
            }
            thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(
            registry.snapshot().unwrap().scheduler.unwrap().state,
            SchedulerState::Active
        );
        registry.stop_scheduler().unwrap();
        assert_eq!(
            registry.snapshot().unwrap().scheduler.unwrap().state,
            SchedulerState::Offline
        );

        let panicked = thread::spawn(|| panic!("scheduler test panic"));
        while !panicked.is_finished() {
            thread::yield_now();
        }
        {
            let mut state = lock(&registry.scheduler.state).unwrap();
            state.running = true;
            state.healthy = true;
            state.handle = Some(panicked);
        }
        assert_eq!(
            registry.snapshot().unwrap().scheduler.unwrap().state,
            SchedulerState::Offline
        );
        assert!(registry.stop_scheduler().is_err());
    }

    #[test]
    fn focus_reports_require_protocol_eighteen_and_increment_revision() {
        let registry = DaemonService::default();
        let (_workspace, shell, runtime) = running_shell(&registry);
        let run = match &*lock(&shell.lifecycle).unwrap() {
            ShellLifecycle::Running { run, .. } => Arc::clone(run),
            _ => panic!("expected running shell"),
        };

        assert!(
            registry
                .record_focus_gained(17, &shell, &run, &runtime, "controller")
                .is_err()
        );
        assert!(registry.snapshot().unwrap().focused_terminal.is_none());

        install_test_controller(&runtime, "controller");
        assert!(
            registry
                .record_focus_gained(18, &shell, &run, &runtime, "controller")
                .unwrap()
        );
        assert!(
            registry
                .record_focus_gained(18, &shell, &run, &runtime, "controller")
                .unwrap()
        );

        let focused = registry.snapshot().unwrap().focused_terminal.unwrap();
        assert_eq!(focused.revision, 2);
        assert_eq!(focused.workspace_id, shell.workspace_id);
        assert_eq!(focused.shell_id, shell.id);
        let lifecycle = lock(&shell.lifecycle).unwrap();
        let ShellLifecycle::Running { run, .. } = &*lifecycle else {
            panic!("expected running shell");
        };
        assert_eq!(focused.run_id, run.id);
        drop(lifecycle);

        *lock(&shell.lifecycle).unwrap() = ShellLifecycle::Running {
            profile: profile(),
            run: Arc::new(ShellRun::new(2)),
            runtime: Arc::clone(&runtime),
        };
        assert!(registry.snapshot().unwrap().focused_terminal.is_none());

        lock(&registry.durable.state)
            .unwrap()
            .shells
            .remove(&shell.id);
        assert!(registry.snapshot().unwrap().focused_terminal.is_none());
    }

    #[test]
    fn focus_reports_from_a_replaced_controller_are_ignored() {
        let registry = DaemonService::default();
        let (_workspace, shell, runtime) = running_shell(&registry);
        let run = match &*lock(&shell.lifecycle).unwrap() {
            ShellLifecycle::Running { run, .. } => Arc::clone(run),
            _ => panic!("expected running shell"),
        };

        install_test_controller(&runtime, "old-controller");
        assert!(
            registry
                .record_focus_gained(18, &shell, &run, &runtime, "old-controller")
                .unwrap()
        );
        install_test_controller(&runtime, "current-controller");
        assert!(
            !registry
                .record_focus_gained(18, &shell, &run, &runtime, "old-controller")
                .unwrap()
        );
        assert_eq!(
            registry
                .snapshot()
                .unwrap()
                .focused_terminal
                .unwrap()
                .revision,
            1
        );

        assert!(
            registry
                .record_focus_gained(18, &shell, &run, &runtime, "current-controller")
                .unwrap()
        );
        assert_eq!(
            registry
                .snapshot()
                .unwrap()
                .focused_terminal
                .unwrap()
                .revision,
            2
        );
    }

    #[test]
    fn stale_handoff_focus_target_is_not_imported() {
        let registry = DaemonService::default();

        registry
            .import_focused_terminal(Some(FocusedTerminalSnapshot {
                revision: 9,
                workspace_id: "missing-workspace".into(),
                shell_id: "missing-shell".into(),
                run_id: "missing-run".into(),
            }))
            .unwrap();

        assert!(registry.snapshot().unwrap().focused_terminal.is_none());
        assert_eq!(lock(&registry.runtimes.focus).unwrap().revision, 9);
    }

    #[test]
    fn replacement_finds_new_binary_after_binary_replacement() {
        let directory = env::temp_dir().join(format!("boomux-replacement-{}", Uuid::new_v4()));
        let installed = directory.join("boomux");
        fs::create_dir_all(&directory).unwrap();
        fs::write(&installed, b"replacement").unwrap();

        assert_eq!(
            select_replacement_executable(
                directory.join("boomux (deleted)"),
                Some(PathBuf::from("boomux"))
            ),
            installed
        );

        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn event_batches_are_monotonic_and_paginated() {
        let registry = DaemonService::default();
        let Response::Events {
            cursor, snapshot, ..
        } = registry.read_events(None, 256, 0).unwrap()
        else {
            panic!("expected event baseline");
        };
        assert!(snapshot.is_some());
        registry
            .events
            .publish(DaemonEventKind::WorkspaceClosed {
                workspace_id: "w1".into(),
            })
            .unwrap();
        registry
            .events
            .publish(DaemonEventKind::WorkspaceClosed {
                workspace_id: "w2".into(),
            })
            .unwrap();

        let Response::Events {
            cursor: first_cursor,
            events,
            ..
        } = registry.read_events(Some(&cursor), 1, 0).unwrap()
        else {
            panic!("expected event page");
        };
        assert_eq!(events.len(), 1);
        let Response::Events { events, .. } =
            registry.read_events(Some(&first_cursor), 256, 0).unwrap()
        else {
            panic!("expected second event page");
        };
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].id, first_cursor.event_id + 1);
    }

    #[test]
    fn event_cursor_expires_after_retention() {
        let registry = DaemonService::default();
        let Response::Events { cursor, .. } = registry.read_events(None, 256, 0).unwrap() else {
            panic!("expected event baseline");
        };
        for index in 0..=MAX_RETAINED_EVENTS {
            registry
                .events
                .publish(DaemonEventKind::WorkspaceClosed {
                    workspace_id: format!("w{index}"),
                })
                .unwrap();
        }

        let error = registry.read_events(Some(&cursor), 256, 0).unwrap_err();
        assert_eq!(error.wire_code(), ErrorCode::CursorExpired);
    }

    #[test]
    fn schedule_shell_filter_ownership_tracks_retained_event_lifetime() {
        let stream = EventStream::new();
        let workspace_id = Uuid::new_v4().to_string();
        let mut batch = Vec::with_capacity((MAX_RETAINED_EVENTS + 1) * 2);
        for index in 0..=MAX_RETAINED_EVENTS {
            let shell_id = Uuid::from_u128(index as u128 + 1).to_string();
            batch.push(DaemonEventKind::ScheduledExecutionChanged {
                workspace_id: workspace_id.clone(),
                execution: ScheduledExecutionSnapshot {
                    id: Uuid::new_v4().to_string(),
                    workspace_id: workspace_id.clone(),
                    schedule_id: Uuid::new_v4().to_string(),
                    revision: 1,
                    state: ScheduledExecutionState::Starting,
                    dispatch_kind: ScheduledExecutionDispatchKind::Manual,
                    dispatch_key: Uuid::new_v4().to_string(),
                    schedule_revision: 1,
                    prompt_revision: 1,
                    trigger_revision: 1,
                    requested_at_ms: 1,
                    scheduled_at_ms: Some(index as u64 + 1),
                    coalesced_through_ms: None,
                    started_at_ms: None,
                    ended_at_ms: None,
                    cwd: env::temp_dir(),
                    integration: "opencode".into(),
                    session: AgentScheduleSession::Fresh,
                    reason: None,
                    outcome: None,
                    shell_id: Some(shell_id.clone()),
                    run_id: Some(Uuid::new_v4().to_string()),
                    agent_id: None,
                    external_session_id: None,
                },
            });
            batch.push(DaemonEventKind::ShellCreated {
                workspace_id: workspace_id.clone(),
                shell_id,
                name: format!("schedule-shell-{index}"),
            });
        }

        let (events, cursor, schedule_shell_ids) = {
            let mut state = lock(&stream.state).unwrap();
            EventStream::append_batch_locked(&mut state, batch);
            let retained_generic_ids = state
                .events
                .iter()
                .filter_map(|event| event_shell_id(&event.kind).map(str::to_owned))
                .collect::<HashSet<_>>();
            assert_eq!(state.events.len(), MAX_RETAINED_EVENTS);
            assert_eq!(state.schedule_shell_ids, retained_generic_ids);
            assert!(state.schedule_shell_ids.len() < MAX_RETAINED_EVENTS);
            (
                state.events.iter().cloned().collect::<Vec<_>>(),
                EventCursor {
                    stream_id: state.stream_id.clone(),
                    event_id: state.latest_id,
                },
                state.schedule_shell_ids.clone(),
            )
        };
        let latest_id = cursor.event_id;
        let Response::Events { cursor, events, .. } = response_for_version_with_schedule_shells(
            Response::Events {
                stream_id: cursor.stream_id.clone(),
                cursor,
                snapshot: None,
                events,
            },
            22,
            &schedule_shell_ids,
        ) else {
            panic!("expected downgraded events");
        };
        assert_eq!(cursor.event_id, latest_id);
        assert!(events.is_empty());
    }

    #[test]
    fn blocked_publication_coalesces_output_by_shell_run() {
        let mut transition = TransitionState {
            persistence_in_flight: true,
            ..TransitionState::default()
        };
        for revision in 1..=10_000 {
            transition.queue_runtime_event(DaemonEventKind::OutputChanged {
                workspace_id: "w1".into(),
                shell_id: "s1".into(),
                run_id: "r1".into(),
                output_revision: revision,
            });
        }

        assert_eq!(transition.pending_runtime_events.len(), 1);
        assert!(matches!(
            transition.pending_runtime_events.front(),
            Some(DaemonEventKind::OutputChanged {
                output_revision: 10_000,
                ..
            })
        ));
    }

    #[test]
    fn process_name_is_trimmed_sanitized_and_bounded() {
        assert_eq!(parse_process_name(b"  sleep\n"), Some("sleep".into()));
        assert_eq!(parse_process_name(b" \n\t "), None);
        assert_eq!(parse_process_name(b"bad\0name\n"), Some("bad?name".into()));
        assert_eq!(
            parse_process_name(&[b'x'; MAX_FOREGROUND_PROCESS_BYTES + 1]),
            Some("x".repeat(MAX_FOREGROUND_PROCESS_BYTES))
        );
        assert_eq!(
            proc_foreground_process_group("123 (shell name) S 1 123 123 34826 456 0"),
            Some(456)
        );
    }

    #[test]
    fn pending_and_exited_shell_snapshots_have_no_foreground_process() {
        let shell = create_pending_shell(
            "workspace-id",
            ShellSpec::login("snapshot-test", env::temp_dir()),
        )
        .unwrap();
        assert!(shell.snapshot().unwrap().foreground_process.is_none());

        let run = Arc::new(ShellRun::new(1));
        run.finish(ShellRunExitReason::Exited { code: Some(0) })
            .unwrap();
        *lock(&shell.lifecycle).unwrap() = ShellLifecycle::Exited {
            code: Some(0),
            profile: profile(),
            run,
            runtime: None,
            terminal: Arc::new(Mutex::new(TerminalState::new(24, 80))),
        };
        assert!(shell.snapshot().unwrap().foreground_process.is_none());
    }

    #[test]
    fn running_shell_snapshot_reports_real_pty_foreground_process() {
        let registry = DaemonService::default();
        let (_workspace, shell, _runtime) = running_shell(&registry);

        let deadline = Instant::now() + FOREGROUND_PROCESS_CACHE_INTERVAL + Duration::from_secs(1);
        loop {
            let foreground_process = shell.snapshot().unwrap().foreground_process;
            if foreground_process.as_deref() == Some("sleep") {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "sleep did not become the foreground process; last observed {foreground_process:?}"
            );
            thread::sleep(IO_RETRY_DELAY);
        }
        shell.kill().unwrap();
    }

    #[test]
    fn rejects_duplicate_shell_names_before_spawning() {
        let cwd = env::temp_dir();
        let specs = vec![
            ShellSpec::login("shell", &cwd),
            ShellSpec::login("shell", &cwd),
        ];

        let error = validate_shell_specs(&specs).unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
    }

    #[test]
    fn validates_terminal_profile_bounds() {
        assert!(validate_terminal_profile(&profile()).is_ok());

        let mut invalid = profile();
        invalid.rows = 0;
        assert!(validate_terminal_profile(&invalid).is_err());

        let mut invalid = profile();
        invalid.cols = MAX_TERMINAL_COLS + 1;
        assert!(validate_terminal_profile(&invalid).is_err());

        let mut invalid = profile();
        invalid.term = Some("bad\nterm".into());
        assert!(validate_terminal_profile(&invalid).is_err());

        let mut invalid = profile();
        invalid.term = Some("x".repeat(MAX_TERMINAL_ENV_VALUE + 1));
        assert!(validate_terminal_profile(&invalid).is_err());
    }

    #[test]
    fn validates_unix_environment_without_echoing_payload() {
        let invalid_name = UnixEnvironment {
            variables: vec![protocol::UnixEnvironmentVariable {
                name: b"SECRET=NAME".to_vec(),
                value: b"secret-value".to_vec(),
            }],
        };
        let error = validate_unix_environment(&invalid_name).unwrap_err();
        assert!(!error.to_string().contains("SECRET"));
        assert!(!error.to_string().contains("secret-value"));

        let invalid_value = UnixEnvironment {
            variables: vec![protocol::UnixEnvironmentVariable {
                name: b"VALID_NAME".to_vec(),
                value: b"secret\0value".to_vec(),
            }],
        };
        assert!(validate_unix_environment(&invalid_value).is_err());

        let bytes = UnixEnvironment {
            variables: vec![protocol::UnixEnvironmentVariable {
                name: b"NON_UTF8".to_vec(),
                value: vec![0xff, 0xfe],
            }],
        };
        assert!(validate_unix_environment(&bytes).is_ok());
    }

    #[test]
    fn bounds_new_names_without_rejecting_legacy_persisted_names() {
        let long_name = "x".repeat(MAX_NAME_BYTES + 1);
        assert_eq!(
            validate_name(&long_name).unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
        assert!(validate_persisted_name(&long_name).is_ok());
        assert_eq!(
            validate_name("real\nforged\trow").unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
        assert!(validate_persisted_name("real\nlegacy row").is_ok());
    }

    fn schedule_spec(prompt: &str) -> AgentScheduleSpec {
        AgentScheduleSpec {
            name: "nightly".into(),
            cwd: env::temp_dir(),
            integration: "opencode".into(),
            prompt: prompt.into(),
            session: AgentScheduleSession::Fresh,
            trigger: AgentScheduleTrigger {
                cron: " 0  2 * * * ".into(),
                timezone: "UTC".into(),
            },
            state: AgentScheduleState::Paused,
            overlap_policy: AgentScheduleOverlapPolicy::Skip,
        }
    }

    fn schedule_update(name: &str, prompt: &str, cron: &str) -> AgentScheduleUpdate {
        AgentScheduleUpdate {
            name: name.into(),
            prompt: prompt.into(),
            trigger: AgentScheduleTrigger {
                cron: cron.into(),
                timezone: "UTC".into(),
            },
        }
    }

    #[test]
    fn restored_enabled_schedule_records_a_removed_dispatch_capability_as_invalid_target() {
        let schedule = PersistedAgentSchedule {
            id: Uuid::new_v4().to_string(),
            name: "nightly".into(),
            cwd: env::temp_dir(),
            integration: "unsupported".into(),
            prompt: "review changes".into(),
            session: AgentScheduleSession::Fresh,
            trigger: AgentScheduleTrigger {
                cron: "0 2 * * *".into(),
                timezone: "UTC".into(),
            },
            state: AgentScheduleState::Enabled,
            overlap_policy: AgentScheduleOverlapPolicy::Skip,
            revision: 1,
            prompt_revision: 1,
            trigger_revision: 1,
            created_at_ms: 1,
            updated_at_ms: 1,
            evaluation_frontier_ms: 1,
            evaluation_frontier_trigger_revision: 1,
            execution_shell_id: None,
            dispatch_key_filter: vec![0; DISPATCH_KEY_FILTER_BYTES],
            executions: Vec::new(),
        };

        assert!(validate_persisted_schedule(&schedule).is_ok());

        let registry = DaemonService::default();
        let workspace = registry
            .create_workspace("restored-invalid-target".into(), Vec::new())
            .unwrap();
        let restored = Arc::new(AgentSchedule::from_persisted(&workspace.id, schedule));
        lock(&registry.durable.state)
            .unwrap()
            .schedules
            .insert(restored.id.clone(), Arc::clone(&restored));
        lock(
            &registry
                .durable
                .workspace(&workspace.id)
                .unwrap()
                .schedule_ids,
        )
        .unwrap()
        .push(restored.id.clone());

        let execution = registry
            .durable
            .decide_schedule_execution(
                &restored.id,
                ScheduleDecision {
                    dispatch_kind: ScheduledExecutionDispatchKind::Timed,
                    dispatch_key: timed_dispatch_key(&restored.id, 1, 60_000),
                    scheduled_at_ms: Some(60_000),
                    coalesced_through_ms: None,
                    requested_at_ms: 60_000,
                    forced_skip: None,
                },
                4,
            )
            .unwrap()
            .0;
        assert_eq!(execution.state, ScheduledExecutionState::Skipped);
        assert_eq!(
            execution.reason,
            Some(ScheduledExecutionReason::InvalidTarget)
        );
    }

    #[test]
    fn state_twelve_execution_validation_enforces_exact_state_shapes_and_revisions() {
        let dispatch_key = Uuid::new_v4().to_string();
        let mut filter = vec![0; DISPATCH_KEY_FILTER_BYTES];
        remember_dispatch_key(&mut filter, &dispatch_key);
        let execution = PersistedScheduledExecution {
            id: Uuid::new_v4().to_string(),
            revision: 1,
            state: ScheduledExecutionState::Claimed,
            dispatch_kind: ScheduledExecutionDispatchKind::Manual,
            dispatch_key,
            schedule_revision: 2,
            prompt_revision: 1,
            trigger_revision: 1,
            requested_at_ms: 10,
            scheduled_at_ms: None,
            coalesced_through_ms: None,
            started_at_ms: None,
            ended_at_ms: None,
            cwd: env::temp_dir(),
            integration: "opencode".into(),
            session: AgentScheduleSession::Fresh,
            prompt: "private".into(),
            runner_token: Uuid::new_v4().to_string(),
            reason: None,
            outcome: None,
            shell_id: None,
            run_id: None,
            agent_id: None,
            external_session_id: None,
        };
        let mut schedule = PersistedAgentSchedule {
            id: Uuid::new_v4().to_string(),
            name: "valid".into(),
            cwd: env::temp_dir(),
            integration: "opencode".into(),
            prompt: "private".into(),
            session: AgentScheduleSession::Fresh,
            trigger: AgentScheduleTrigger {
                cron: "0 2 * * *".into(),
                timezone: "UTC".into(),
            },
            state: AgentScheduleState::Paused,
            overlap_policy: AgentScheduleOverlapPolicy::Skip,
            revision: 2,
            prompt_revision: 1,
            trigger_revision: 1,
            created_at_ms: 1,
            updated_at_ms: 2,
            evaluation_frontier_ms: 1,
            evaluation_frontier_trigger_revision: 1,
            execution_shell_id: None,
            dispatch_key_filter: filter,
            executions: vec![execution],
        };
        assert!(validate_persisted_schedule(&schedule).is_ok());

        schedule.executions[0].revision = 0;
        assert!(validate_persisted_schedule(&schedule).is_err());
        schedule.executions[0].revision = 1;

        schedule.executions[0].started_at_ms = Some(10);
        assert!(validate_persisted_schedule(&schedule).is_err());
        schedule.executions[0].started_at_ms = None;
        schedule.executions[0].schedule_revision = 3;
        assert!(validate_persisted_schedule(&schedule).is_err());
        schedule.executions[0].schedule_revision = 2;
        schedule.executions[0].state = ScheduledExecutionState::Active;
        schedule.executions[0].shell_id = Some(Uuid::new_v4().to_string());
        schedule.executions[0].run_id = Some(Uuid::new_v4().to_string());
        schedule.executions[0].started_at_ms = Some(10);
        schedule.executions[0].reason = Some(ScheduledExecutionReason::HostSpawnFailed);
        assert!(validate_persisted_schedule(&schedule).is_err());
    }

    #[test]
    fn state_twelve_validates_timed_matrix_frontier_and_chrono_bounds() {
        let scheduled_at_ms = 60_000;
        let dispatch_key =
            timed_dispatch_key("00000000-0000-0000-0000-000000000001", 1, scheduled_at_ms);
        let mut filter = vec![0; DISPATCH_KEY_FILTER_BYTES];
        remember_dispatch_key(&mut filter, &dispatch_key);
        let execution = PersistedScheduledExecution {
            id: timed_execution_id("00000000-0000-0000-0000-000000000001", 1, scheduled_at_ms),
            revision: 1,
            state: ScheduledExecutionState::Skipped,
            dispatch_kind: ScheduledExecutionDispatchKind::Timed,
            dispatch_key,
            schedule_revision: 1,
            prompt_revision: 1,
            trigger_revision: 1,
            requested_at_ms: 120_000,
            scheduled_at_ms: Some(scheduled_at_ms),
            coalesced_through_ms: Some(120_000),
            started_at_ms: None,
            ended_at_ms: Some(120_000),
            cwd: env::temp_dir(),
            integration: "opencode".into(),
            session: AgentScheduleSession::Fresh,
            prompt: "private".into(),
            runner_token: Uuid::new_v4().to_string(),
            reason: Some(ScheduledExecutionReason::Missed),
            outcome: None,
            shell_id: None,
            run_id: None,
            agent_id: None,
            external_session_id: None,
        };
        let mut schedule = PersistedAgentSchedule {
            id: "00000000-0000-0000-0000-000000000001".into(),
            name: "timed-valid".into(),
            cwd: env::temp_dir(),
            integration: "opencode".into(),
            prompt: "private".into(),
            session: AgentScheduleSession::Fresh,
            trigger: AgentScheduleTrigger {
                cron: "0 2 * * *".into(),
                timezone: "UTC".into(),
            },
            state: AgentScheduleState::Paused,
            overlap_policy: AgentScheduleOverlapPolicy::Skip,
            revision: 1,
            prompt_revision: 1,
            trigger_revision: 1,
            created_at_ms: 1,
            updated_at_ms: 120_000,
            evaluation_frontier_ms: 120_000,
            evaluation_frontier_trigger_revision: 1,
            execution_shell_id: None,
            dispatch_key_filter: filter,
            executions: vec![execution],
        };
        assert!(validate_persisted_schedule(&schedule).is_ok());

        let clone_schedule = |schedule: &PersistedAgentSchedule| {
            serde_json::from_value(serde_json::to_value(schedule).unwrap()).unwrap()
        };
        let valid = clone_schedule(&schedule);
        schedule.executions[0].scheduled_at_ms = Some(120_001);
        assert!(validate_persisted_schedule(&schedule).is_err());
        schedule = clone_schedule(&valid);
        schedule.executions[0].coalesced_through_ms = Some(120_001);
        assert!(validate_persisted_schedule(&schedule).is_err());
        schedule = clone_schedule(&valid);
        schedule.executions[0].coalesced_through_ms = Some(60_000);
        schedule.executions[0].reason = Some(ScheduledExecutionReason::Overlap);
        assert!(validate_persisted_schedule(&schedule).is_err());
        schedule = clone_schedule(&valid);
        schedule.executions[0].dispatch_kind = ScheduledExecutionDispatchKind::Manual;
        schedule.executions[0].scheduled_at_ms = None;
        schedule.executions[0].coalesced_through_ms = None;
        assert!(validate_persisted_schedule(&schedule).is_err());
        schedule = clone_schedule(&valid);
        schedule.executions[0].id = Uuid::new_v4().to_string();
        assert!(validate_persisted_schedule(&schedule).is_err());
        schedule = clone_schedule(&valid);
        schedule.evaluation_frontier_ms = scheduled_at_ms - 1;
        assert!(validate_persisted_schedule(&schedule).is_err());

        let max =
            u64::try_from(chrono::DateTime::<chrono::Utc>::MAX_UTC.timestamp_millis()).unwrap();
        schedule = clone_schedule(&valid);
        schedule.executions[0].scheduled_at_ms = Some(max + 1);
        assert!(validate_persisted_schedule(&schedule).is_err());
        schedule = clone_schedule(&valid);
        schedule.executions[0].coalesced_through_ms = Some(max + 1);
        assert!(validate_persisted_schedule(&schedule).is_err());
        schedule = valid;
        schedule.executions.clear();
        schedule.dispatch_key_filter = vec![0; DISPATCH_KEY_FILTER_BYTES];
        schedule.created_at_ms = max;
        schedule.updated_at_ms = max;
        schedule.evaluation_frontier_ms = max;
        assert!(validate_persisted_schedule(&schedule).is_ok());
        schedule.evaluation_frontier_ms = max + 1;
        assert!(validate_persisted_schedule(&schedule).is_err());
        schedule.evaluation_frontier_ms = max;
        schedule.updated_at_ms = max + 1;
        assert!(validate_persisted_schedule(&schedule).is_err());
        schedule.updated_at_ms = max;
        schedule.created_at_ms = max + 1;
        assert!(validate_persisted_schedule(&schedule).is_err());
    }

    #[test]
    fn malformed_current_state_rejects_linked_continuation_identity_mismatch() {
        let directory = env::temp_dir().join(format!(
            "boomux-invalid-continuation-link-{}",
            Uuid::new_v4()
        ));
        let store = StateStore::at(directory.join("state/state.json"));
        let workspace_id = Uuid::new_v4().to_string();
        let schedule_id = Uuid::new_v4().to_string();
        let shell_id = Uuid::new_v4().to_string();
        let run_id = Uuid::new_v4().to_string();
        let agent_id = Uuid::new_v4().to_string();
        let dispatch_key = Uuid::new_v4().to_string();
        let mut filter = vec![0; DISPATCH_KEY_FILTER_BYTES];
        remember_dispatch_key(&mut filter, &dispatch_key);
        let expected_session_id = "exact-continuation".to_owned();
        let mismatched_session_id = "different-session".to_owned();
        let session = AgentScheduleSession::Continue {
            external_session_id: expected_session_id,
        };
        let schedule = PersistedAgentSchedule {
            id: schedule_id.clone(),
            name: "continued".into(),
            cwd: env::temp_dir(),
            integration: "opencode".into(),
            prompt: "private".into(),
            session: session.clone(),
            trigger: AgentScheduleTrigger {
                cron: "0 2 * * *".into(),
                timezone: "UTC".into(),
            },
            state: AgentScheduleState::Paused,
            overlap_policy: AgentScheduleOverlapPolicy::Skip,
            revision: 1,
            prompt_revision: 1,
            trigger_revision: 1,
            created_at_ms: 1,
            updated_at_ms: 1,
            evaluation_frontier_ms: 1,
            evaluation_frontier_trigger_revision: 1,
            execution_shell_id: Some(shell_id.clone()),
            dispatch_key_filter: filter,
            executions: vec![PersistedScheduledExecution {
                id: Uuid::new_v4().to_string(),
                revision: 1,
                state: ScheduledExecutionState::Active,
                dispatch_kind: ScheduledExecutionDispatchKind::Manual,
                dispatch_key,
                schedule_revision: 1,
                prompt_revision: 1,
                trigger_revision: 1,
                requested_at_ms: 1,
                scheduled_at_ms: None,
                coalesced_through_ms: None,
                started_at_ms: Some(1),
                ended_at_ms: None,
                cwd: env::temp_dir(),
                integration: "opencode".into(),
                session,
                prompt: "private".into(),
                runner_token: Uuid::new_v4().to_string(),
                reason: None,
                outcome: None,
                shell_id: Some(shell_id.clone()),
                run_id: Some(run_id.clone()),
                agent_id: Some(agent_id.clone()),
                external_session_id: Some(mismatched_session_id.clone()),
            }],
        };
        let mut persisted = PersistedState::default();
        persisted.workspaces = vec![PersistedWorkspace {
            id: workspace_id,
            revision: 1,
            name: "continued".into(),
            default_cwd: None,
            shells: vec![PersistedShell {
                id: shell_id.clone(),
                revision: 1,
                name: "scheduled".into(),
                cwd: env::temp_dir(),
                command: vec!["boomux".into()],
                owner: ShellOwner::Schedule { schedule_id },
                last_run: Some(PersistedShellRun {
                    id: run_id.clone(),
                    generation: 1,
                    started_at_ms: 1,
                    ended_at_ms: None,
                    exit_reason: None,
                    output_revision: 0,
                    environment_has_run_id: true,
                    profile: profile(),
                    terminal_history: None,
                }),
            }],
            launchers: Vec::new(),
            agents: vec![PersistedAgentInstance {
                id: agent_id,
                shell_id,
                run_id,
                name: "continued".into(),
                integration: "opencode".into(),
                external_session_id: Some(mismatched_session_id),
                cwd: Some(env::temp_dir()),
                started_at_ms: 1,
                ended_at_ms: None,
                observation: AgentObservationSnapshot {
                    revision: 1,
                    state: AgentState::Idle,
                    authority: AgentAuthority::LifecycleIntegration,
                    evidence: "registered".into(),
                    confidence: 100,
                    observed_at_ms: 1,
                },
                attention: None,
            }],
            schedules: vec![schedule],
        }];
        store.save(&persisted).unwrap();
        assert_eq!(
            DaemonService::restore(store, true, None)
                .err()
                .expect("malformed continuation state was accepted")
                .kind(),
            io::ErrorKind::InvalidData
        );
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn schedule_update_changes_all_fields_and_preserves_execution_snapshots() {
        let registry = DaemonService::default();
        set_scheduler_time(&registry, 100);
        let workspace = registry
            .create_workspace("schedule-update".into(), Vec::new())
            .unwrap();
        let Response::AgentSchedule { schedule } = registry
            .dispatch(Request::CreateAgentSchedule {
                workspace_id: workspace.id,
                spec: schedule_spec("original private prompt"),
            })
            .unwrap()
        else {
            panic!("expected schedule");
        };
        let (execution, _) = registry
            .durable
            .claim_schedule_execution(&schedule.id, &Uuid::new_v4().to_string())
            .unwrap();
        set_scheduler_time(&registry, 200);

        let Response::AgentSchedule { schedule: updated } = registry
            .dispatch(Request::UpdateAgentSchedule {
                schedule_id: schedule.id.clone(),
                expected_revision: schedule.revision,
                update: schedule_update(
                    "morning review",
                    "updated private prompt",
                    " 30  9 * * 1-5 ",
                ),
            })
            .unwrap()
        else {
            panic!("expected updated schedule");
        };

        assert_eq!(updated.name, "morning review");
        assert_eq!(updated.trigger.cron, "30 9 * * 1-5");
        assert_eq!(updated.revision, 2);
        assert_eq!(updated.prompt_revision, 2);
        assert_eq!(updated.trigger_revision, 2);
        assert_eq!(updated.updated_at_ms, 200);
        assert_eq!(updated.evaluation_frontier_ms, 200);
        let inspection = registry
            .schedule(&schedule.id)
            .unwrap()
            .inspection()
            .unwrap();
        assert_eq!(inspection.prompt, "updated private prompt");
        let retained = registry.durable.execution(&execution.id).unwrap();
        assert_eq!(retained.prompt, "original private prompt");
        assert_eq!(retained.schedule_revision, 1);
        assert_eq!(retained.prompt_revision, 1);
        assert_eq!(retained.trigger_revision, 1);
    }

    #[test]
    fn schedule_update_tracks_component_revisions_and_exact_noops() {
        let directory = env::temp_dir().join(format!("boomux-update-noop-{}", Uuid::new_v4()));
        let registry = DaemonService::restore(
            StateStore::at(directory.join("state/state.json")),
            false,
            None,
        )
        .unwrap();
        set_scheduler_time(&registry, 100);
        let workspace = registry
            .create_workspace("schedule-components".into(), Vec::new())
            .unwrap();
        let Response::AgentSchedule { schedule } = registry
            .dispatch(Request::CreateAgentSchedule {
                workspace_id: workspace.id,
                spec: schedule_spec("first prompt"),
            })
            .unwrap()
        else {
            panic!("expected schedule");
        };
        let original_frontier = schedule.evaluation_frontier_ms;

        set_scheduler_time(&registry, 200);
        let Response::AgentSchedule { schedule: renamed } = registry
            .dispatch(Request::UpdateAgentSchedule {
                schedule_id: schedule.id.clone(),
                expected_revision: 1,
                update: schedule_update("renamed", "first prompt", "0 2 * * *"),
            })
            .unwrap()
        else {
            panic!("expected renamed schedule");
        };
        assert_eq!(
            (
                renamed.revision,
                renamed.prompt_revision,
                renamed.trigger_revision
            ),
            (2, 1, 1)
        );
        assert_eq!(renamed.evaluation_frontier_ms, original_frontier);
        let event_id = lock(&registry.events.state).unwrap().latest_id;
        registry.fail_next_persistence();
        let Response::AgentSchedule {
            schedule: unchanged,
        } = registry
            .dispatch(Request::UpdateAgentSchedule {
                schedule_id: schedule.id.clone(),
                expected_revision: 2,
                update: schedule_update("renamed", "first prompt", " 0  2 * * * "),
            })
            .unwrap()
        else {
            panic!("expected unchanged schedule");
        };
        assert_eq!(unchanged, renamed);
        assert_eq!(lock(&registry.events.state).unwrap().latest_id, event_id);

        set_scheduler_time(&registry, 300);
        assert!(
            registry
                .dispatch(Request::UpdateAgentSchedule {
                    schedule_id: schedule.id.clone(),
                    expected_revision: 2,
                    update: schedule_update("renamed", "second prompt", "0 2 * * *"),
                })
                .is_err(),
            "the no-op must not consume the injected persistence failure"
        );
        registry.flush_pending().unwrap();
        let Response::AgentSchedule { schedule: prompted } = registry
            .dispatch(Request::UpdateAgentSchedule {
                schedule_id: schedule.id.clone(),
                expected_revision: 2,
                update: schedule_update("renamed", "second prompt", "0 2 * * *"),
            })
            .unwrap()
        else {
            panic!("expected prompt update");
        };
        assert_eq!(
            (
                prompted.revision,
                prompted.prompt_revision,
                prompted.trigger_revision
            ),
            (3, 3, 1)
        );
        assert_eq!(prompted.evaluation_frontier_ms, original_frontier);

        set_scheduler_time(&registry, 400);
        let Response::AgentSchedule {
            schedule: triggered,
        } = registry
            .dispatch(Request::UpdateAgentSchedule {
                schedule_id: schedule.id,
                expected_revision: 3,
                update: schedule_update("renamed", "second prompt", "15 3 * * *"),
            })
            .unwrap()
        else {
            panic!("expected trigger update");
        };
        assert_eq!(
            (
                triggered.revision,
                triggered.prompt_revision,
                triggered.trigger_revision
            ),
            (4, 3, 4)
        );
        assert_eq!(triggered.evaluation_frontier_ms, 400);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn schedule_update_requires_paused_exact_revision_and_valid_unique_definition() {
        let registry = DaemonService::default();
        let workspace = registry
            .create_workspace("schedule-validation".into(), Vec::new())
            .unwrap();
        let Response::AgentSchedule { schedule } = registry
            .dispatch(Request::CreateAgentSchedule {
                workspace_id: workspace.id.clone(),
                spec: schedule_spec("private"),
            })
            .unwrap()
        else {
            panic!("expected schedule");
        };
        let mut second = schedule_spec("other private");
        second.name = "other".into();
        registry
            .dispatch(Request::CreateAgentSchedule {
                workspace_id: workspace.id,
                spec: second,
            })
            .unwrap();
        let Response::AgentSchedule { schedule: enabled } = registry
            .dispatch(Request::ResumeAgentSchedule {
                schedule_id: schedule.id.clone(),
            })
            .unwrap()
        else {
            panic!("expected enabled schedule");
        };
        let busy = registry
            .dispatch(Request::UpdateAgentSchedule {
                schedule_id: schedule.id.clone(),
                expected_revision: enabled.revision,
                update: schedule_update("changed", "changed", "0 3 * * *"),
            })
            .unwrap_err();
        assert_eq!(busy.wire_code(), ErrorCode::Busy);
        let Response::AgentSchedule { schedule: paused } = registry
            .dispatch(Request::PauseAgentSchedule {
                schedule_id: schedule.id.clone(),
            })
            .unwrap()
        else {
            panic!("expected paused schedule");
        };
        let stale = registry
            .dispatch(Request::UpdateAgentSchedule {
                schedule_id: schedule.id.clone(),
                expected_revision: enabled.revision,
                update: schedule_update("changed", "changed", "0 3 * * *"),
            })
            .unwrap_err();
        assert_eq!(stale.wire_code(), ErrorCode::RevisionAhead);
        let duplicate = registry
            .dispatch(Request::UpdateAgentSchedule {
                schedule_id: schedule.id.clone(),
                expected_revision: paused.revision,
                update: schedule_update("other", "changed", "0 3 * * *"),
            })
            .unwrap_err();
        assert_eq!(duplicate.wire_code(), ErrorCode::AlreadyExists);
        let invalid = registry
            .dispatch(Request::UpdateAgentSchedule {
                schedule_id: schedule.id,
                expected_revision: paused.revision,
                update: schedule_update("changed", "changed", "not a cron"),
            })
            .unwrap_err();
        assert_eq!(invalid.wire_code(), ErrorCode::InvalidArgument);
    }

    #[test]
    fn schedule_update_rolls_back_persistence_and_publishes_prompt_free_event() {
        let directory = env::temp_dir().join(format!("boomux-update-rollback-{}", Uuid::new_v4()));
        let registry = DaemonService::restore(
            StateStore::at(directory.join("state/state.json")),
            false,
            None,
        )
        .unwrap();
        let workspace = registry
            .create_workspace("schedule-rollback".into(), Vec::new())
            .unwrap();
        let Response::AgentSchedule { schedule } = registry
            .dispatch(Request::CreateAgentSchedule {
                workspace_id: workspace.id,
                spec: schedule_spec("original private"),
            })
            .unwrap()
        else {
            panic!("expected schedule");
        };
        let event_id = lock(&registry.events.state).unwrap().latest_id;
        registry.fail_next_persistence();
        assert!(
            registry
                .dispatch(Request::UpdateAgentSchedule {
                    schedule_id: schedule.id.clone(),
                    expected_revision: 1,
                    update: schedule_update("updated", "NEW PRIVATE PROMPT", "0 3 * * *"),
                })
                .is_err()
        );
        assert_eq!(
            registry.schedule(&schedule.id).unwrap().snapshot().unwrap(),
            schedule
        );
        assert_eq!(lock(&registry.events.state).unwrap().latest_id, event_id);
        registry.flush_pending().unwrap();

        registry
            .dispatch(Request::UpdateAgentSchedule {
                schedule_id: schedule.id,
                expected_revision: 1,
                update: schedule_update("updated", "NEW PRIVATE PROMPT", "0 3 * * *"),
            })
            .unwrap();
        let events = lock(&registry.events.state).unwrap().events.clone();
        let event = events.back().unwrap();
        assert!(matches!(
            event.kind,
            DaemonEventKind::AgentScheduleUpdated { .. }
        ));
        assert!(
            !serde_json::to_string(event)
                .unwrap()
                .contains("NEW PRIVATE PROMPT")
        );
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn schedule_management_is_prompt_private_and_idempotent() {
        let registry = DaemonService::default();
        let workspace = registry
            .create_workspace("scheduled".into(), Vec::new())
            .unwrap();
        let prompt = "private schedule instructions";
        let Response::AgentSchedule { schedule } = registry
            .dispatch(Request::CreateAgentSchedule {
                workspace_id: workspace.id.clone(),
                spec: schedule_spec(prompt),
            })
            .unwrap()
        else {
            panic!("expected schedule response");
        };
        assert_eq!(schedule.trigger.cron, "0 2 * * *");
        assert_eq!(schedule.revision, 1);
        assert!(schedule.execution_shell_id.is_none());
        assert_eq!(
            registry.snapshot().unwrap().workspaces[0].schedules,
            std::slice::from_ref(&schedule)
        );
        assert!(
            !serde_json::to_string(&registry.snapshot().unwrap())
                .unwrap()
                .contains(prompt)
        );

        let Response::AgentScheduleInspection { inspection } = registry
            .dispatch(Request::GetAgentSchedule {
                schedule_id: schedule.id.clone(),
            })
            .unwrap()
        else {
            panic!("expected schedule inspection");
        };
        assert_eq!(inspection.prompt, prompt);

        let Response::AgentSchedule { schedule: paused } = registry
            .dispatch(Request::PauseAgentSchedule {
                schedule_id: schedule.id.clone(),
            })
            .unwrap()
        else {
            panic!("expected paused schedule");
        };
        assert_eq!(paused.revision, 1);
        let Response::AgentSchedule { schedule: resumed } = registry
            .dispatch(Request::ResumeAgentSchedule {
                schedule_id: schedule.id.clone(),
            })
            .unwrap()
        else {
            panic!("expected resumed schedule");
        };
        assert_eq!(resumed.revision, 2);
        assert!(resumed.evaluation_frontier_ms >= schedule.evaluation_frontier_ms);
        let Response::AgentSchedule { schedule: repeated } = registry
            .dispatch(Request::ResumeAgentSchedule {
                schedule_id: schedule.id.clone(),
            })
            .unwrap()
        else {
            panic!("expected resumed schedule");
        };
        assert_eq!(repeated, resumed);

        let events = lock(&registry.events.state).unwrap().events.clone();
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(
                    event.kind,
                    DaemonEventKind::AgentScheduleCreated { .. }
                        | DaemonEventKind::AgentSchedulePaused { .. }
                        | DaemonEventKind::AgentScheduleResumed { .. }
                ))
                .count(),
            2
        );
        assert!(!serde_json::to_string(&events).unwrap().contains(prompt));
    }

    #[test]
    fn timed_evaluation_is_idempotent_rollback_safe_and_frontier_authoritative() {
        let registry = Arc::new(DaemonService::default());
        set_scheduler_time(&registry, 0);
        let workspace = registry
            .create_workspace("timed".into(), Vec::new())
            .unwrap();
        let mut spec = schedule_spec("private");
        spec.trigger.cron = "* * * * *".into();
        spec.state = AgentScheduleState::Enabled;
        let Response::AgentSchedule { schedule } = registry
            .dispatch(Request::CreateAgentSchedule {
                workspace_id: workspace.id.clone(),
                spec,
            })
            .unwrap()
        else {
            panic!("expected schedule");
        };

        set_scheduler_time(&registry, 180_000);
        registry.evaluate_schedules(true).unwrap();
        let executions = registry
            .durable
            .scheduled_executions(None, Some(&schedule.id))
            .unwrap();
        assert_eq!(executions.len(), 1);
        assert_eq!(executions[0].state, ScheduledExecutionState::Skipped);
        assert_eq!(executions[0].reason, Some(ScheduledExecutionReason::Missed));
        assert_eq!(executions[0].scheduled_at_ms, Some(60_000));
        assert_eq!(executions[0].coalesced_through_ms, Some(180_000));
        let event_count = lock(&registry.events.state).unwrap().events.len();
        registry.evaluate_schedules(true).unwrap();
        assert_eq!(
            registry
                .durable
                .scheduled_executions(None, Some(&schedule.id))
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            lock(&registry.events.state).unwrap().events.len(),
            event_count
        );

        set_scheduler_time(&registry, 240_000);
        registry.fail_after_next_mutation();
        assert!(registry.evaluate_schedules(true).is_err());
        assert_eq!(
            registry
                .durable
                .schedule(&schedule.id)
                .unwrap()
                .snapshot()
                .unwrap()
                .evaluation_frontier_ms,
            180_000
        );
        assert_eq!(
            lock(&registry.events.state).unwrap().events.len(),
            event_count
        );
        assert_eq!(
            registry
                .durable
                .schedule(&schedule.id)
                .unwrap()
                .snapshot()
                .unwrap()
                .next_occurrence
                .unwrap()
                .scheduled_at_ms,
            240_000
        );
        registry.evaluate_schedules(true).unwrap();
        let after_retry = registry
            .durable
            .scheduled_executions(None, Some(&schedule.id))
            .unwrap();
        assert_eq!(after_retry.len(), 2);
        assert_eq!(after_retry[0].scheduled_at_ms, Some(240_000));
        assert_eq!(
            registry
                .durable
                .schedule(&schedule.id)
                .unwrap()
                .snapshot()
                .unwrap()
                .next_occurrence
                .unwrap()
                .scheduled_at_ms,
            300_000
        );

        set_scheduler_time(&registry, 120_000);
        registry.evaluate_schedules(true).unwrap();
        assert_eq!(
            registry
                .durable
                .scheduled_executions(None, Some(&schedule.id))
                .unwrap()
                .len(),
            2
        );
    }

    #[test]
    fn pause_resume_and_history_pruning_do_not_reenable_old_occurrences() {
        let registry = Arc::new(DaemonService::default());
        set_scheduler_time(&registry, 0);
        let workspace = registry
            .create_workspace("timed-pruning".into(), Vec::new())
            .unwrap();
        let mut spec = schedule_spec("private");
        spec.trigger.cron = "* * * * *".into();
        spec.state = AgentScheduleState::Enabled;
        let Response::AgentSchedule { schedule } = registry
            .dispatch(Request::CreateAgentSchedule {
                workspace_id: workspace.id.clone(),
                spec,
            })
            .unwrap()
        else {
            panic!("expected schedule");
        };
        set_scheduler_time(&registry, 60_000);
        registry
            .change_schedule_state(&schedule.id, AgentScheduleState::Paused)
            .unwrap();
        set_scheduler_time(&registry, 600_000);
        registry.evaluate_schedules(true).unwrap();
        assert!(
            registry
                .durable
                .scheduled_executions(None, Some(&schedule.id))
                .unwrap()
                .is_empty()
        );
        registry
            .change_schedule_state(&schedule.id, AgentScheduleState::Enabled)
            .unwrap();
        for minute in 11..=(11 + MAX_TERMINAL_EXECUTIONS_PER_SCHEDULE as u64) {
            set_scheduler_time(&registry, minute * 60_000);
            registry.evaluate_schedules(true).unwrap();
        }
        let executions = registry
            .durable
            .scheduled_executions(None, Some(&schedule.id))
            .unwrap();
        assert_eq!(executions.len(), MAX_TERMINAL_EXECUTIONS_PER_SCHEDULE);
        let frontier = registry
            .durable
            .schedule(&schedule.id)
            .unwrap()
            .snapshot()
            .unwrap()
            .evaluation_frontier_ms;
        set_scheduler_time(&registry, frontier.saturating_sub(60_000));
        registry.evaluate_schedules(true).unwrap();
        assert_eq!(
            registry
                .durable
                .scheduled_executions(None, Some(&schedule.id))
                .unwrap()
                .len(),
            MAX_TERMINAL_EXECUTIONS_PER_SCHEDULE
        );
    }

    #[test]
    fn manual_and_timed_decisions_share_atomic_capacity_and_continuation_leases() {
        let registry = DaemonService::default();
        let first_workspace = registry
            .create_workspace("capacity-one".into(), Vec::new())
            .unwrap();
        let second_workspace = registry
            .create_workspace("capacity-two".into(), Vec::new())
            .unwrap();
        let (first, _) = registry
            .durable
            .create_schedule(&first_workspace.id, schedule_spec("first"))
            .unwrap();
        let mut second_spec = schedule_spec("second");
        second_spec.name = "second".into();
        let (second, _) = registry
            .durable
            .create_schedule(&first_workspace.id, second_spec)
            .unwrap();
        let mut third_spec = schedule_spec("third");
        third_spec.name = "third".into();
        let (third, _) = registry
            .durable
            .create_schedule(&second_workspace.id, third_spec)
            .unwrap();

        let decide = |schedule_id: &str, max_concurrent| {
            registry
                .durable
                .decide_schedule_execution(
                    schedule_id,
                    ScheduleDecision {
                        dispatch_kind: ScheduledExecutionDispatchKind::Manual,
                        dispatch_key: Uuid::new_v4().to_string(),
                        scheduled_at_ms: None,
                        coalesced_through_ms: None,
                        requested_at_ms: 1,
                        forced_skip: None,
                    },
                    max_concurrent,
                )
                .unwrap()
                .0
        };
        assert_eq!(decide(&first.id, 4).state, ScheduledExecutionState::Claimed);
        assert_eq!(
            decide(&first.id, 4).reason,
            Some(ScheduledExecutionReason::Overlap)
        );
        assert_eq!(
            decide(&second.id, 4).reason,
            Some(ScheduledExecutionReason::WorkspaceCapacity)
        );
        assert_eq!(
            decide(&third.id, 1).reason,
            Some(ScheduledExecutionReason::GlobalCapacity)
        );

        let continuation_registry = DaemonService::default();
        let first_workspace = continuation_registry
            .create_workspace("continuation-one".into(), Vec::new())
            .unwrap();
        let second_workspace = continuation_registry
            .create_workspace("continuation-two".into(), Vec::new())
            .unwrap();
        let mut continuation = schedule_spec("continued");
        continuation.session = AgentScheduleSession::Continue {
            external_session_id: "exact-session".into(),
        };
        let (first, _) = continuation_registry
            .durable
            .create_schedule(&first_workspace.id, continuation.clone())
            .unwrap();
        continuation.name = "continued-two".into();
        let (second, _) = continuation_registry
            .durable
            .create_schedule(&second_workspace.id, continuation)
            .unwrap();
        let decision = |schedule_id: &str| {
            continuation_registry
                .durable
                .decide_schedule_execution(
                    schedule_id,
                    ScheduleDecision {
                        dispatch_kind: ScheduledExecutionDispatchKind::Timed,
                        dispatch_key: timed_dispatch_key(schedule_id, 1, 60_000),
                        scheduled_at_ms: Some(60_000),
                        coalesced_through_ms: None,
                        requested_at_ms: 60_000,
                        forced_skip: None,
                    },
                    4,
                )
                .unwrap()
                .0
        };
        assert_eq!(decision(&first.id).state, ScheduledExecutionState::Claimed);
        let blocked = decision(&second.id);
        assert_eq!(blocked.state, ScheduledExecutionState::Skipped);
        assert_eq!(
            blocked.reason,
            Some(ScheduledExecutionReason::ActiveSession)
        );
        assert!(blocked.shell_id.is_none());
    }

    #[test]
    fn pruned_dispatch_keys_remain_explicitly_rejected() {
        let registry = DaemonService::default();
        let workspace = registry
            .create_workspace("scheduled".into(), Vec::new())
            .unwrap();
        let (schedule, _) = registry
            .durable
            .create_schedule(&workspace.id, schedule_spec("private"))
            .unwrap();
        let first_key = Uuid::from_u128(1).to_string();
        let mut keys = Vec::new();

        for value in 1..=MAX_TERMINAL_EXECUTIONS_PER_SCHEDULE + 1 {
            let key = Uuid::from_u128(value as u128).to_string();
            let (snapshot, _) = registry
                .durable
                .claim_schedule_execution(&schedule.id, &key)
                .unwrap();
            let execution = registry.durable.execution(&snapshot.id).unwrap();
            keys.push((key, snapshot.id));
            registry
                .durable
                .mutate_execution(&execution, |state| {
                    state.state = ScheduledExecutionState::DispatchFailed;
                    state.ended_at_ms = Some(unix_time_ms().max(execution.requested_at_ms));
                    state.reason = Some(ScheduledExecutionReason::RunnerStartFailed);
                    Ok(())
                })
                .unwrap();
        }

        let pruned_key = keys
            .iter()
            .find(|(_, execution_id)| registry.durable.execution(execution_id).is_err())
            .map(|(key, _)| key)
            .unwrap_or(&first_key);
        let error = match registry
            .durable
            .claim_schedule_execution(&schedule.id, pruned_key)
        {
            Err(error) => error,
            Ok(_) => panic!("pruned dispatch key was accepted"),
        };
        assert_eq!(error.wire_code(), ErrorCode::IdempotencyExpired);
    }

    #[test]
    fn execution_wait_and_bounded_list_are_revision_exact() {
        let registry = DaemonService::default();
        let workspace = registry
            .create_workspace("observed".into(), Vec::new())
            .unwrap();
        let (schedule, _) = registry
            .durable
            .create_schedule(&workspace.id, schedule_spec("private"))
            .unwrap();
        let (claimed, _) = registry
            .durable
            .claim_schedule_execution(&schedule.id, &Uuid::new_v4().to_string())
            .unwrap();
        registry
            .events
            .initialize_committed_executions(
                registry.durable.scheduled_executions(None, None).unwrap(),
            )
            .unwrap();
        assert_eq!(claimed.revision, 1);

        let Response::ScheduledExecutionWait { changed, execution } = registry
            .wait_scheduled_execution(&claimed.id, 0, 0)
            .unwrap()
        else {
            panic!("expected execution wait response");
        };
        assert!(changed);
        assert_eq!(execution.revision, 1);
        let Response::ScheduledExecutionWait { changed, .. } = registry
            .wait_scheduled_execution(&claimed.id, 1, 0)
            .unwrap()
        else {
            panic!("expected execution wait timeout");
        };
        assert!(!changed);
        assert_eq!(
            registry
                .wait_scheduled_execution(&claimed.id, 2, 0)
                .unwrap_err()
                .wire_code(),
            ErrorCode::RevisionAhead
        );

        let execution = registry.durable.execution(&claimed.id).unwrap();
        registry
            .durable
            .mutate_execution(&execution, |state| {
                state.state = ScheduledExecutionState::DispatchFailed;
                state.ended_at_ms = Some(execution.requested_at_ms);
                state.reason = Some(ScheduledExecutionReason::RunnerStartFailed);
                Ok(())
            })
            .unwrap();
        let (second, _) = registry
            .durable
            .claim_schedule_execution(&schedule.id, &Uuid::new_v4().to_string())
            .unwrap();
        let second_execution = registry.durable.execution(&second.id).unwrap();
        registry
            .durable
            .mutate_execution(&second_execution, |state| {
                state.state = ScheduledExecutionState::DispatchFailed;
                state.ended_at_ms = Some(second_execution.requested_at_ms);
                state.reason = Some(ScheduledExecutionReason::RunnerStartFailed);
                Ok(())
            })
            .unwrap();
        let (page, limit, truncated) = registry
            .durable
            .scheduled_execution_page(None, None, 1)
            .unwrap();
        assert_eq!(limit, 1);
        assert!(truncated);
        assert_eq!(page[0].revision, 2);
        let expected_newest = if (second.requested_at_ms, second.id.as_str())
            > (claimed.requested_at_ms, claimed.id.as_str())
        {
            second.id
        } else {
            claimed.id
        };
        assert_eq!(page[0].id, expected_newest);
    }

    #[test]
    fn execution_wait_never_observes_in_flight_or_rolled_back_revision() {
        let directory = env::temp_dir().join(format!("boomux-execution-wait-{}", Uuid::new_v4()));
        let gate = Arc::new((Mutex::new((false, false, false)), Condvar::new()));
        let hook_gate = Arc::clone(&gate);
        let store = StateStore::at_with_save_hook(
            directory.join("state/state.json"),
            Arc::new(move || {
                let (state, changed) = &*hook_gate;
                let mut state = state.lock().unwrap();
                if !state.0 {
                    return;
                }
                state.1 = true;
                changed.notify_all();
                while !state.2 {
                    state = changed.wait(state).unwrap();
                }
            }),
        );
        let registry = Arc::new(DaemonService::restore(store, false, None).unwrap());
        let workspace = registry
            .create_workspace("wait-commit".into(), Vec::new())
            .unwrap();
        let (schedule, _) = registry
            .durable
            .create_schedule(&workspace.id, schedule_spec("private"))
            .unwrap();
        let (claimed, _) = registry
            .durable
            .claim_schedule_execution(&schedule.id, &Uuid::new_v4().to_string())
            .unwrap();
        registry
            .events
            .initialize_committed_executions(
                registry.durable.scheduled_executions(None, None).unwrap(),
            )
            .unwrap();
        let execution = registry.durable.execution(&claimed.id).unwrap();
        {
            let (state, _) = &*gate;
            state.lock().unwrap().0 = true;
        }
        let changing_registry = Arc::clone(&registry);
        let changing_execution = Arc::clone(&execution);
        let mutation = thread::spawn(move || {
            changing_registry.change_execution(&changing_execution, |state| {
                state.state = ScheduledExecutionState::DispatchFailed;
                state.ended_at_ms = Some(changing_execution.requested_at_ms);
                state.reason = Some(ScheduledExecutionReason::RunnerStartFailed);
                Ok(())
            })
        });
        {
            let (state, changed) = &*gate;
            let mut state = state.lock().unwrap();
            while !state.1 {
                state = changed.wait(state).unwrap();
            }
        }
        let (waited_tx, waited_rx) = mpsc::sync_channel(1);
        let waiting_registry = Arc::clone(&registry);
        let execution_id = claimed.id.clone();
        let waiter = thread::spawn(move || {
            let result = waiting_registry.wait_scheduled_execution(&execution_id, 1, 25);
            waited_tx.send(result).unwrap();
        });
        let Response::ScheduledExecutionWait { execution, changed } = waited_rx
            .recv_timeout(Duration::from_secs(1))
            .unwrap()
            .unwrap()
        else {
            panic!("expected in-flight execution wait deadline");
        };
        assert!(!changed);
        assert_eq!(execution.revision, 1);
        waiter.join().unwrap();
        {
            let (state, changed) = &*gate;
            let mut state = state.lock().unwrap();
            state.2 = true;
            changed.notify_all();
        }
        mutation.join().unwrap().unwrap();
        let Response::ScheduledExecutionWait { execution, changed } = registry
            .wait_scheduled_execution(&claimed.id, 1, 1_000)
            .unwrap()
        else {
            panic!("expected committed execution wait");
        };
        assert!(changed);
        assert_eq!(execution.revision, 2);

        let (second, _) = registry
            .durable
            .claim_schedule_execution(&schedule.id, &Uuid::new_v4().to_string())
            .unwrap();
        let second_execution = registry.durable.execution(&second.id).unwrap();
        registry
            .events
            .initialize_committed_executions(
                registry.durable.scheduled_executions(None, None).unwrap(),
            )
            .unwrap();
        registry.fail_next_persistence();
        assert!(
            registry
                .change_execution(&second_execution, |state| {
                    state.state = ScheduledExecutionState::DispatchFailed;
                    state.ended_at_ms = Some(second_execution.requested_at_ms);
                    state.reason = Some(ScheduledExecutionReason::RunnerStartFailed);
                    Ok(())
                })
                .is_err()
        );
        let Response::ScheduledExecutionWait { execution, changed } =
            registry.wait_scheduled_execution(&second.id, 1, 0).unwrap()
        else {
            panic!("expected rolled-back execution wait");
        };
        assert!(!changed);
        assert_eq!(execution.revision, 1);

        registry.fail_next_persistence();
        assert!(
            registry
                .terminalize_dispatch_failure(&second_execution)
                .is_err()
        );
        let Response::ScheduledExecutionWait { execution, changed } = registry
            .wait_scheduled_execution(&second.id, 1, 25)
            .unwrap()
        else {
            panic!("expected pending-storage execution wait deadline");
        };
        assert!(!changed);
        assert_eq!(execution.revision, 1);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn lifecycle_reservation_hides_cancellation_revision_until_commit_or_rollback() {
        let registry = DaemonService::default();
        let workspace = registry
            .create_workspace("cancel-frontier".into(), Vec::new())
            .unwrap();
        let (schedule, _) = registry
            .durable
            .create_schedule(&workspace.id, schedule_spec("private"))
            .unwrap();
        let (claimed, _) = registry
            .durable
            .claim_schedule_execution(&schedule.id, &Uuid::new_v4().to_string())
            .unwrap();
        registry
            .events
            .initialize_committed_executions(
                registry.durable.scheduled_executions(None, None).unwrap(),
            )
            .unwrap();
        let mut transaction = registry.events.transaction().unwrap();
        transaction.begin_lifecycle_reservation(1);
        drop(transaction);
        let execution = registry.durable.execution(&claimed.id).unwrap();
        let (_, undo) = registry
            .durable
            .mutate_execution(&execution, |state| {
                state.state = ScheduledExecutionState::Cancelled;
                state.ended_at_ms = Some(execution.requested_at_ms);
                state.reason = Some(ScheduledExecutionReason::CancelledByUser);
                Ok(())
            })
            .unwrap();
        let Response::ScheduledExecutionWait { execution, changed } = registry
            .wait_scheduled_execution(&claimed.id, 1, 0)
            .unwrap()
        else {
            panic!("expected committed cancellation frontier");
        };
        assert!(!changed);
        assert_eq!(execution.state, ScheduledExecutionState::Claimed);
        registry.durable.rollback(undo).unwrap();
        let mut transaction = registry.events.transaction().unwrap();
        transaction.release_lifecycle_reservation();
        drop(transaction);
        let restored = registry
            .durable
            .execution(&claimed.id)
            .unwrap()
            .snapshot()
            .unwrap();
        assert_eq!(restored.revision, 1);
        assert_eq!(restored.state, ScheduledExecutionState::Claimed);
    }

    #[test]
    fn protocol_twenty_three_and_twenty_four_lists_remain_uncapped_before_filtering() {
        let registry = Arc::new(DaemonService::default());
        let workspace = registry
            .create_workspace("mixed-list".into(), Vec::new())
            .unwrap();
        for schedule_index in 0..2 {
            let mut spec = schedule_spec("private");
            spec.name = format!("schedule-{schedule_index}");
            let (schedule, _) = registry
                .durable
                .create_schedule(&workspace.id, spec)
                .unwrap();
            for index in 0..60 {
                let (claimed, _) = registry
                    .durable
                    .claim_schedule_execution(&schedule.id, &Uuid::new_v4().to_string())
                    .unwrap();
                let execution = registry.durable.execution(&claimed.id).unwrap();
                registry
                    .durable
                    .mutate_execution(&execution, |state| {
                        state.state = ScheduledExecutionState::DispatchFailed;
                        state.ended_at_ms = Some(execution.requested_at_ms);
                        state.reason = Some(ScheduledExecutionReason::RunnerStartFailed);
                        Ok(())
                    })
                    .unwrap();
                if index < 10 {
                    registry
                        .durable
                        .decide_schedule_execution(
                            &schedule.id,
                            ScheduleDecision {
                                dispatch_kind: ScheduledExecutionDispatchKind::Timed,
                                dispatch_key: Uuid::new_v4().to_string(),
                                scheduled_at_ms: Some(index + 1),
                                coalesced_through_ms: Some(index + 1),
                                requested_at_ms: index + 1,
                                forced_skip: Some(ScheduledExecutionReason::Missed),
                            },
                            4,
                        )
                        .unwrap();
                }
            }
        }
        let mut empty_spec = schedule_spec("private");
        empty_spec.name = "schedule-without-history".into();
        registry
            .durable
            .create_schedule(&workspace.id, empty_spec)
            .unwrap();
        for (version, supplied_limit, expected) in
            [(23, None, 120), (23, Some(1), 120), (24, None, 140)]
        {
            let Response::ScheduledExecutions {
                executions,
                limit,
                truncated,
                schedules,
                ..
            } = registry
                .dispatch_arc(
                    Request::ListScheduledExecutions {
                        workspace_id: None,
                        schedule_id: None,
                        limit: supplied_limit,
                    },
                    version,
                )
                .unwrap()
            else {
                panic!("expected execution list");
            };
            assert_eq!(executions.len(), expected);
            assert_eq!(limit, 0);
            assert!(!truncated);
            assert!(schedules.is_empty());
        }
        let Response::ScheduledExecutions {
            executions,
            limit,
            truncated,
            schedules,
            schedule_limit,
            schedules_truncated,
        } = registry
            .dispatch_arc(
                Request::ListScheduledExecutions {
                    workspace_id: None,
                    schedule_id: None,
                    limit: Some(1),
                },
                25,
            )
            .unwrap()
        else {
            panic!("expected bounded execution list");
        };
        assert_eq!(executions.len(), 1);
        assert_eq!(limit, 1);
        assert!(truncated);
        assert_eq!(schedules.len(), 3);
        assert!(
            schedules
                .iter()
                .any(|projection| projection.schedule_id != executions[0].schedule_id)
        );
        assert_eq!(schedule_limit, 100);
        assert!(!schedules_truncated);
    }

    #[test]
    fn execution_list_bounds_complete_selected_schedule_projection_scope() {
        let registry = Arc::new(DaemonService::default());
        let workspace = registry
            .create_workspace("projection-bound".into(), Vec::new())
            .unwrap();
        let second_workspace = registry
            .create_workspace("projection-bound-second".into(), Vec::new())
            .unwrap();
        for index in 0..=protocol::MAX_SCHEDULED_EXECUTION_SCHEDULE_PROJECTIONS {
            let mut spec = schedule_spec("private");
            spec.name = format!("schedule-{index:03}");
            let workspace_id = if index < 64 {
                &workspace.id
            } else {
                &second_workspace.id
            };
            registry
                .durable
                .create_schedule(workspace_id, spec)
                .unwrap();
        }
        let Response::ScheduledExecutions {
            executions,
            schedules,
            schedule_limit,
            schedules_truncated,
            ..
        } = registry
            .dispatch_arc(
                Request::ListScheduledExecutions {
                    workspace_id: None,
                    schedule_id: None,
                    limit: Some(1),
                },
                protocol::PROTOCOL_VERSION,
            )
            .unwrap()
        else {
            panic!("expected bounded schedule projections");
        };
        assert!(executions.is_empty());
        assert_eq!(schedule_limit, 100);
        assert_eq!(schedules.len(), 100);
        assert!(schedules_truncated);
        assert!(
            schedules
                .windows(2)
                .all(|pair| pair[0].schedule_id < pair[1].schedule_id)
        );
    }

    #[test]
    fn pending_runner_start_failure_notifies_once_after_recovery_commit() {
        let directory = env::temp_dir().join(format!("boomux-pending-notify-{}", Uuid::new_v4()));
        let sink = Arc::new(RecordingNotificationSink::default());
        let mut registry = DaemonService::restore(
            StateStore::at(directory.join("state/state.json")),
            false,
            None,
        )
        .unwrap();
        registry.notification_settings.desktop = NotificationSettings {
            enabled: true,
            scheduled_dispatch_failed: true,
            ..Default::default()
        };
        registry.notification_sink = sink.clone();
        let workspace = registry
            .create_workspace("runner-failure".into(), Vec::new())
            .unwrap();
        let (schedule, _) = registry
            .durable
            .create_schedule(&workspace.id, schedule_spec("PRIVATE RUNNER PROMPT"))
            .unwrap();
        let (claimed, _) = registry
            .durable
            .claim_schedule_execution(&schedule.id, &Uuid::new_v4().to_string())
            .unwrap();
        let execution = registry.durable.execution(&claimed.id).unwrap();
        registry.fail_next_persistence();
        assert!(registry.terminalize_dispatch_failure(&execution).is_err());
        assert!(sink.requests.lock().unwrap().is_empty());
        assert!(registry.flush_pending().unwrap());
        let failed = execution.snapshot().unwrap();
        assert_eq!(
            failed.reason,
            Some(ScheduledExecutionReason::RunnerStartFailed)
        );
        assert_eq!(failed.revision, 2);
        assert_eq!(
            registry.terminalize_dispatch_failure(&execution).unwrap(),
            failed
        );
        let linked = registry
            .change_execution(&execution, |state| {
                state.agent_id = Some("late-exact-agent".into());
                state.external_session_id = Some("late-exact-session".into());
                Ok(())
            })
            .unwrap();
        assert_eq!(linked.revision, 3);
        assert_eq!(linked.agent_id.as_deref(), Some("late-exact-agent"));
        let requests = sink.requests.lock().unwrap();
        assert_eq!(requests.len(), 1);
        assert_eq!(
            requests[0].reason,
            NotificationReason::ScheduledDispatchFailed
        );
        assert_eq!(requests[0].shell, failed.id);
        assert!(!format!("{:?}", requests[0]).contains("PRIVATE RUNNER PROMPT"));
        drop(requests);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn cold_interruption_notification_does_not_repeat_for_late_agent_link() {
        let (registry, sink) = notification_registry(NotificationSettings {
            enabled: true,
            scheduled_interrupted: true,
            ..Default::default()
        });
        let workspace = registry
            .create_workspace("late-cold-link".into(), Vec::new())
            .unwrap();
        let (schedule, _) = registry
            .durable
            .create_schedule(&workspace.id, schedule_spec("private"))
            .unwrap();
        let (claimed, _) = registry
            .durable
            .claim_schedule_execution(&schedule.id, &Uuid::new_v4().to_string())
            .unwrap();
        registry
            .events
            .initialize_committed_executions(
                registry.durable.scheduled_executions(None, None).unwrap(),
            )
            .unwrap();
        let execution = registry.durable.execution(&claimed.id).unwrap();
        let interrupted = registry
            .change_execution(&execution, |state| {
                state.state = ScheduledExecutionState::Interrupted;
                state.ended_at_ms = Some(execution.requested_at_ms);
                state.reason = Some(ScheduledExecutionReason::ColdDaemonRecovery);
                Ok(())
            })
            .unwrap();
        assert_eq!(interrupted.revision, 2);
        assert_eq!(sink.requests.lock().unwrap().len(), 1);

        let linked = registry
            .change_execution(&execution, |state| {
                state.agent_id = Some("late-cold-agent".into());
                state.external_session_id = Some("late-cold-session".into());
                Ok(())
            })
            .unwrap();
        assert_eq!(linked.revision, 3);
        let requests = sink.requests.lock().unwrap();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].reason, NotificationReason::ScheduledInterrupted);
    }

    #[test]
    fn schedules_restore_remove_with_workspace_and_rollback_on_failure() {
        let directory = env::temp_dir().join(format!("boomux-schedules-{}", Uuid::new_v4()));
        let path = directory.join("state/state.json");
        let registry = DaemonService::restore(StateStore::at(path.clone()), false, None).unwrap();
        let Response::Workspace { workspace } = registry
            .dispatch(Request::CreateWorkspace {
                name: "scheduled".into(),
                default_cwd: None,
                shells: Vec::new(),
            })
            .unwrap()
        else {
            panic!("expected workspace");
        };
        registry.fail_after_next_mutation();
        assert!(
            registry
                .dispatch(Request::CreateAgentSchedule {
                    workspace_id: workspace.id.clone(),
                    spec: schedule_spec("rolled back prompt"),
                })
                .is_err()
        );
        assert!(
            registry.snapshot().unwrap().workspaces[0]
                .schedules
                .is_empty()
        );
        let Response::AgentSchedule { schedule } = registry
            .dispatch(Request::CreateAgentSchedule {
                workspace_id: workspace.id.clone(),
                spec: schedule_spec("persisted private prompt"),
            })
            .unwrap()
        else {
            panic!("expected schedule");
        };
        drop(registry);

        let registry = DaemonService::restore(StateStore::at(path), false, None).unwrap();
        let restored = registry
            .dispatch(Request::GetAgentSchedule {
                schedule_id: schedule.id.clone(),
            })
            .unwrap();
        let Response::AgentScheduleInspection { inspection } = restored else {
            panic!("expected inspection");
        };
        assert_eq!(inspection.prompt, "persisted private prompt");

        registry.fail_after_next_mutation();
        assert!(
            registry
                .dispatch(Request::ResumeAgentSchedule {
                    schedule_id: schedule.id.clone(),
                })
                .is_err()
        );
        assert_eq!(
            registry.schedule(&schedule.id).unwrap().snapshot().unwrap(),
            schedule
        );

        registry.fail_after_next_mutation();
        assert!(
            registry
                .dispatch(Request::RemoveAgentSchedule {
                    schedule_id: schedule.id.clone(),
                })
                .is_err()
        );
        assert!(registry.schedule(&schedule.id).is_ok());

        registry.close_workspace(&workspace.id).unwrap();
        assert!(registry.schedule(&schedule.id).is_err());
        let events = lock(&registry.events.state).unwrap();
        assert_eq!(
            events
                .events
                .iter()
                .filter(|event| matches!(event.kind, DaemonEventKind::WorkspaceClosed { .. }))
                .count(),
            1
        );
        assert!(
            !events
                .events
                .iter()
                .any(|event| matches!(event.kind, DaemonEventKind::AgentScheduleRemoved { .. }))
        );
        drop(events);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn invalid_schedule_input_does_not_mutate_or_disclose_prompt() {
        let registry = DaemonService::default();
        let workspace = registry
            .create_workspace("scheduled".into(), Vec::new())
            .unwrap();
        let prompt = "private invalid prompt";
        let mut spec = schedule_spec(prompt);
        spec.trigger.cron = "not a cron expression".into();
        let error = registry
            .dispatch(Request::CreateAgentSchedule {
                workspace_id: workspace.id.clone(),
                spec,
            })
            .unwrap_err();
        assert!(!error.to_string().contains(prompt));
        assert!(
            registry.snapshot().unwrap().workspaces[0]
                .schedules
                .is_empty()
        );
        assert!(lock(&registry.events.state).unwrap().events.is_empty());

        let mut impossible = schedule_spec(prompt);
        impossible.trigger.cron = "0 0 30 2 *".into();
        let error = registry
            .dispatch(Request::CreateAgentSchedule {
                workspace_id: workspace.id,
                spec: impossible,
            })
            .unwrap_err();
        assert_eq!(error.wire_code(), ErrorCode::InvalidArgument);
        assert!(
            registry.snapshot().unwrap().workspaces[0]
                .schedules
                .is_empty()
        );
    }

    #[test]
    fn protocol_twenty_one_filters_schedules_without_rewinding_cursor() {
        let registry = DaemonService::default();
        let workspace = registry
            .create_workspace("scheduled".into(), Vec::new())
            .unwrap();
        registry
            .dispatch(Request::CreateAgentSchedule {
                workspace_id: workspace.id,
                spec: schedule_spec("private prompt"),
            })
            .unwrap();
        let Response::Events {
            stream_id,
            cursor,
            snapshot,
            events,
        } = registry.read_events(None, 256, 0).unwrap()
        else {
            panic!("expected event baseline");
        };
        let response = response_for_version(
            Response::Events {
                stream_id,
                cursor: cursor.clone(),
                snapshot,
                events,
            },
            21,
        );
        let Response::Events {
            cursor: filtered_cursor,
            snapshot: Some(snapshot),
            events,
            ..
        } = response
        else {
            panic!("expected filtered baseline");
        };
        assert_eq!(filtered_cursor, cursor);
        assert!(snapshot.workspaces[0].schedules.is_empty());
        assert!(events.is_empty());

        let retained = lock(&registry.events.state).unwrap().events.clone();
        let response = response_for_version(
            Response::Events {
                stream_id: cursor.stream_id.clone(),
                cursor: cursor.clone(),
                snapshot: None,
                events: retained.into(),
            },
            21,
        );
        let Response::Events {
            cursor: filtered_cursor,
            events,
            ..
        } = response
        else {
            panic!("expected filtered events");
        };
        assert_eq!(filtered_cursor, cursor);
        assert!(events.is_empty());
    }

    #[test]
    fn protocol_twenty_six_filters_schedule_updates_without_rewinding_cursor() {
        let registry = DaemonService::default();
        let workspace = registry
            .create_workspace("schedule-edit-filter".into(), Vec::new())
            .unwrap();
        let Response::AgentSchedule { schedule } = registry
            .dispatch(Request::CreateAgentSchedule {
                workspace_id: workspace.id,
                spec: schedule_spec("private prompt"),
            })
            .unwrap()
        else {
            panic!("expected schedule");
        };
        let Response::Events {
            cursor: baseline, ..
        } = registry.read_events(None, 256, 0).unwrap()
        else {
            panic!("expected event baseline");
        };
        registry
            .dispatch(Request::UpdateAgentSchedule {
                schedule_id: schedule.id,
                expected_revision: 1,
                update: schedule_update("updated", "new private prompt", "0 3 * * *"),
            })
            .unwrap();
        let current = registry.read_events(Some(&baseline), 256, 0).unwrap();
        let Response::Events {
            cursor: current_cursor,
            events: current_events,
            ..
        } = current.clone()
        else {
            panic!("expected current events");
        };
        assert!(matches!(
            current_events.as_slice(),
            [DaemonEvent {
                kind: DaemonEventKind::AgentScheduleUpdated { .. },
                ..
            }]
        ));

        let Response::Events { cursor, events, .. } = response_for_version(current, 26) else {
            panic!("expected filtered events");
        };
        assert_eq!(cursor, current_cursor);
        assert!(events.is_empty());
    }

    #[test]
    fn protocol_thirty_one_filters_projection_invalidation_without_rewinding_cursor() {
        let cursor = EventCursor {
            stream_id: Uuid::new_v4().to_string(),
            event_id: 4,
        };
        let response = response_for_version(
            Response::Events {
                stream_id: cursor.stream_id.clone(),
                cursor: cursor.clone(),
                snapshot: None,
                events: vec![DaemonEvent {
                    id: 4,
                    at_ms: 10,
                    kind: DaemonEventKind::NodeProjectionChanged {
                        node_id: Uuid::from_u128(2).to_string(),
                        cache_generation: 3,
                    },
                }],
            },
            31,
        );
        let Response::Events {
            cursor: filtered_cursor,
            events,
            ..
        } = response
        else {
            panic!("expected events");
        };
        assert_eq!(filtered_cursor, cursor);
        assert!(events.is_empty());
    }

    #[test]
    fn projection_cut_resumes_exactly_or_reseeds_on_stream_expiry() {
        let events = EventStream::new();
        let baseline = events.transaction().unwrap().cursor();
        events
            .publish(DaemonEventKind::WorkspaceCreated {
                workspace_id: "workspace-1".into(),
                name: "private-name-is-not-copied-into-transition".into(),
            })
            .unwrap();
        let transaction = events.transaction().unwrap();
        let through = transaction.cursor();
        let (mode, transitions) =
            projection_transitions(&transaction.events, Some(&baseline), &through);
        assert_eq!(mode, NodeProjectionSyncMode::Resumed);
        assert!(matches!(
            transitions.as_slice(),
            [NodeProjectionTransition {
                kind: NodeProjectionTransitionKind::Workspace { workspace_id },
                ..
            }] if workspace_id == "workspace-1"
        ));
        let expired = EventCursor {
            stream_id: Uuid::new_v4().to_string(),
            event_id: baseline.event_id,
        };
        let (mode, transitions) =
            projection_transitions(&transaction.events, Some(&expired), &through);
        assert_eq!(mode, NodeProjectionSyncMode::Baseline);
        assert!(transitions.is_empty());
    }

    #[test]
    fn protocol_twenty_three_filters_paginated_timed_events_without_stalling_cursor() {
        let registry = DaemonService::default();
        let workspace = registry
            .create_workspace("timed-pagination".into(), Vec::new())
            .unwrap();
        let (schedule, _) = registry
            .durable
            .create_schedule(&workspace.id, schedule_spec("private"))
            .unwrap();
        let Response::Events {
            cursor: baseline, ..
        } = registry.read_events(None, 256, 0).unwrap()
        else {
            panic!("expected baseline");
        };
        for occurrence in 1..=300_u64 {
            let scheduled_at_ms = occurrence * 60_000;
            let execution = registry
                .durable
                .decide_schedule_execution(
                    &schedule.id,
                    ScheduleDecision {
                        dispatch_kind: ScheduledExecutionDispatchKind::Timed,
                        dispatch_key: timed_dispatch_key(&schedule.id, 1, scheduled_at_ms),
                        scheduled_at_ms: Some(scheduled_at_ms),
                        coalesced_through_ms: None,
                        requested_at_ms: scheduled_at_ms,
                        forced_skip: Some(ScheduledExecutionReason::Missed),
                    },
                    4,
                )
                .unwrap()
                .0;
            registry
                .events
                .publish_runtime_batch(vec![DaemonEventKind::ScheduledExecutionCreated {
                    workspace_id: workspace.id.clone(),
                    execution,
                }])
                .unwrap();
        }

        let first =
            response_for_version(registry.read_events(Some(&baseline), 256, 0).unwrap(), 23);
        let first_json = serde_json::to_value(&first).unwrap();
        assert_eq!(first_json["events"], serde_json::json!([]));
        assert_eq!(first_json["cursor"]["event_id"], baseline.event_id + 256);
        assert!(!first_json.to_string().contains("timed"));
        assert!(!first_json.to_string().contains("skipped"));
        let Response::Events {
            cursor: first_cursor,
            ..
        } = first
        else {
            panic!("expected first filtered page");
        };
        let second = response_for_version(
            registry.read_events(Some(&first_cursor), 256, 0).unwrap(),
            23,
        );
        let second_json = serde_json::to_value(second).unwrap();
        assert_eq!(second_json["events"], serde_json::json!([]));
        assert_eq!(second_json["cursor"]["event_id"], baseline.event_id + 300);
    }

    #[test]
    fn response_writes_time_out_when_the_client_does_not_read() {
        let (mut server, _client) = UnixStream::pair().unwrap();
        let response = Response::Error {
            message: "x".repeat(4 * 1024 * 1024),
            code: Some(ErrorCode::Busy),
        };
        let started = Instant::now();

        let error = send_response(&mut server, protocol::PROTOCOL_VERSION, response).unwrap_err();

        assert!(matches!(
            error.kind(),
            io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
        ));
        assert!(started.elapsed() < Duration::from_secs(3));
    }

    #[test]
    fn daemon_errors_convert_to_stable_codes_at_the_wire_boundary() {
        let cases = [
            (
                io::Error::new(io::ErrorKind::InvalidInput, "invalid request").into(),
                ErrorCode::InvalidArgument,
                "invalid request",
            ),
            (
                DaemonError::lifecycle(ErrorCode::RunChanged, "run changed"),
                ErrorCode::RunChanged,
                "run changed",
            ),
            (
                DaemonError::persistence(io::Error::other("state write failed")),
                ErrorCode::PersistenceFailed,
                "state write failed",
            ),
            (
                DaemonError::protocol("unsupported request"),
                ErrorCode::UnsupportedVersion,
                "unsupported request",
            ),
            (
                io::Error::other("unexpected failure").into(),
                ErrorCode::Internal,
                "unexpected failure",
            ),
        ];

        for (error, expected_code, expected_message) in cases {
            let (mut server, mut client) = UnixStream::pair().unwrap();
            send_daemon_error(&mut server, protocol::PROTOCOL_VERSION, error).unwrap();
            let response: Envelope<Response> = protocol::read_message(&mut client).unwrap();
            assert_eq!(response.version, protocol::PROTOCOL_VERSION);
            assert_eq!(
                response.message,
                Response::Error {
                    message: expected_message.into(),
                    code: Some(expected_code),
                }
            );
        }
    }

    #[test]
    fn failed_persistence_rolls_back_registry_mutation() {
        let directory = env::temp_dir().join(format!("boomux-rollback-{}", Uuid::new_v4()));
        let registry = DaemonService::restore(
            StateStore::at(directory.join("state/state.json")),
            false,
            None,
        )
        .unwrap();
        registry.fail_next_persistence();

        let result = registry.dispatch(Request::CreateWorkspace {
            name: "rolled-back".into(),
            default_cwd: None,
            shells: Vec::new(),
        });

        assert!(result.is_err());
        assert!(registry.snapshot().unwrap().workspaces.is_empty());
        assert!(lock(&registry.events.state).unwrap().events.is_empty());
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn same_name_rename_is_a_noop_without_preparing_persistence() {
        let directory = env::temp_dir().join(format!("boomux-noop-{}", Uuid::new_v4()));
        let registry = DaemonService::restore(
            StateStore::at(directory.join("state/state.json")),
            false,
            None,
        )
        .unwrap();
        let Response::Workspace { workspace } = registry
            .dispatch(Request::CreateWorkspace {
                name: "unchanged".into(),
                default_cwd: None,
                shells: vec![ShellSpec::login("shell", env::temp_dir())],
            })
            .unwrap()
        else {
            panic!("expected workspace");
        };
        let shell = workspace.shells[0].clone();
        let Response::Launcher { launcher } = registry
            .dispatch(Request::CreateLauncher {
                workspace_id: workspace.id.clone(),
                spec: WorkspaceLauncherSpec {
                    name: "launcher".into(),
                    cwd: env::temp_dir(),
                    command: vec!["true".into()],
                },
            })
            .unwrap()
        else {
            panic!("expected launcher");
        };
        let event_id = lock(&registry.events.state).unwrap().latest_id;
        registry.fail_next_persistence();

        registry
            .dispatch(Request::RenameWorkspace {
                workspace_id: workspace.id.clone(),
                name: workspace.name.clone(),
            })
            .unwrap();

        assert_eq!(lock(&registry.events.state).unwrap().latest_id, event_id);
        assert!(
            registry
                .dispatch(Request::RenameWorkspace {
                    workspace_id: workspace.id.clone(),
                    name: "changed".into(),
                })
                .is_err(),
            "the no-op must not consume the injected persistence failure"
        );
        assert_eq!(registry.snapshot().unwrap().workspaces[0].name, "unchanged");
        registry.flush_pending().unwrap();

        registry.fail_next_persistence();
        registry
            .dispatch(Request::RenameShell {
                shell_id: shell.id.clone(),
                name: shell.name.clone(),
            })
            .unwrap();
        assert!(
            registry
                .dispatch(Request::RenameShell {
                    shell_id: shell.id.clone(),
                    name: "changed-shell".into(),
                })
                .is_err(),
            "the shell no-op must not consume the injected persistence failure"
        );
        assert_eq!(
            registry.shell(&shell.id).unwrap().snapshot().unwrap().name,
            "shell"
        );
        registry.flush_pending().unwrap();

        registry.fail_next_persistence();
        registry
            .dispatch(Request::RenameLauncher {
                launcher_id: launcher.id.clone(),
                name: launcher.name.clone(),
            })
            .unwrap();
        assert!(
            registry
                .dispatch(Request::RenameLauncher {
                    launcher_id: launcher.id.clone(),
                    name: "changed-launcher".into(),
                })
                .is_err(),
            "the launcher no-op must not consume the injected persistence failure"
        );
        assert_eq!(
            registry
                .launcher(&launcher.id)
                .unwrap()
                .snapshot()
                .unwrap()
                .name,
            "launcher"
        );
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn post_mutation_errors_rollback_create_rename_and_remove_shapes() {
        let registry = DaemonService::default();
        let assert_rollback = |request| {
            let before = registry.snapshot().unwrap();
            let event_id = lock(&registry.events.state).unwrap().latest_id;
            registry.fail_after_next_mutation();
            assert!(registry.dispatch(request).is_err());
            assert_eq!(registry.snapshot().unwrap(), before);
            assert_eq!(lock(&registry.events.state).unwrap().latest_id, event_id);
        };

        assert_rollback(Request::CreateWorkspace {
            name: "explicit".into(),
            default_cwd: None,
            shells: vec![ShellSpec::login("first", env::temp_dir())],
        });
        assert_rollback(Request::CreateShell {
            workspace_id: None,
            shell: ShellSpec::login("implicit", env::temp_dir()),
        });

        let Response::Workspace { workspace } = registry
            .dispatch(Request::CreateWorkspace {
                name: "retained".into(),
                default_cwd: None,
                shells: vec![ShellSpec::login("shell", env::temp_dir())],
            })
            .unwrap()
        else {
            panic!("expected workspace");
        };
        assert_rollback(Request::CreateShell {
            workspace_id: Some(workspace.id.clone()),
            shell: ShellSpec::login("second", env::temp_dir()),
        });
        assert_rollback(Request::CreateLauncher {
            workspace_id: workspace.id.clone(),
            spec: WorkspaceLauncherSpec {
                name: "temporary".into(),
                cwd: env::temp_dir(),
                command: vec!["true".into()],
            },
        });

        let Response::Launcher { launcher } = registry
            .dispatch(Request::CreateLauncher {
                workspace_id: workspace.id.clone(),
                spec: WorkspaceLauncherSpec {
                    name: "retained-launcher".into(),
                    cwd: env::temp_dir(),
                    command: vec!["true".into()],
                },
            })
            .unwrap()
        else {
            panic!("expected launcher");
        };
        assert_rollback(Request::RenameWorkspace {
            workspace_id: workspace.id.clone(),
            name: "renamed".into(),
        });
        assert_rollback(Request::RenameShell {
            shell_id: workspace.shells[0].id.clone(),
            name: "renamed-shell".into(),
        });
        assert_rollback(Request::RenameLauncher {
            launcher_id: launcher.id.clone(),
            name: "renamed-launcher".into(),
        });
        assert_rollback(Request::RemoveLauncher {
            launcher_id: launcher.id,
        });
    }

    #[test]
    fn post_mutation_errors_rollback_every_agent_mutation_shape() {
        let registry = DaemonService::default();
        let (workspace, shell, _runtime) = running_shell(&registry);
        let run_id = shell.snapshot().unwrap().run.unwrap().id;
        let assert_rollback = |request| {
            let before = registry.snapshot().unwrap();
            let event_id = lock(&registry.events.state).unwrap().latest_id;
            registry.fail_after_next_mutation();
            assert!(registry.dispatch(request).is_err());
            assert_eq!(registry.snapshot().unwrap(), before);
            assert_eq!(lock(&registry.events.state).unwrap().latest_id, event_id);
        };

        assert_rollback(Request::RegisterAgent {
            shell_id: shell.id.clone(),
            run_id: run_id.clone(),
            spec: agent_spec(AgentState::Working),
        });
        let Response::Agent { agent } = registry
            .dispatch(Request::RegisterAgent {
                shell_id: shell.id.clone(),
                run_id: run_id.clone(),
                spec: agent_spec(AgentState::Working),
            })
            .unwrap()
        else {
            panic!("expected agent");
        };
        let mut ensured = agent_spec(AgentState::Working);
        ensured.external_session_id = Some("external-2".into());
        assert_rollback(Request::EnsureAgent {
            shell_id: shell.id.clone(),
            run_id: run_id.clone(),
            spec: ensured,
        });
        assert_rollback(Request::ReportAgent {
            agent_id: agent.id.clone(),
            run_id: run_id.clone(),
            report: agent_spec(AgentState::Blocked).report,
        });
        let Response::Agent { agent } = registry
            .dispatch(Request::ReportAgent {
                agent_id: agent.id,
                run_id,
                report: agent_spec(AgentState::Blocked).report,
            })
            .unwrap()
        else {
            panic!("expected blocked agent");
        };
        assert_rollback(Request::AcknowledgeAgentAttention {
            agent_id: agent.id,
            observation_revision: agent.observation.revision,
        });

        shell.kill().unwrap();
        registry.close_workspace(&workspace.id).unwrap();
    }

    #[test]
    fn failed_shutdown_persistence_compensates_and_cancels_stopping() {
        let directory = env::temp_dir().join(format!("boomux-shutdown-{}", Uuid::new_v4()));
        let registry = DaemonService::restore(
            StateStore::at(directory.join("state/state.json")),
            false,
            None,
        )
        .unwrap();
        let (workspace, shell, _runtime) = running_shell(&registry);
        let (schedule, _) = registry
            .durable
            .create_schedule(&workspace.id, schedule_spec("private"))
            .unwrap();
        for _ in 0..MAX_TERMINAL_EXECUTIONS_PER_SCHEDULE {
            let (snapshot, _) = registry
                .durable
                .claim_schedule_execution(&schedule.id, &Uuid::new_v4().to_string())
                .unwrap();
            let execution = registry.durable.execution(&snapshot.id).unwrap();
            registry
                .durable
                .mutate_execution(&execution, |state| {
                    state.state = ScheduledExecutionState::DispatchFailed;
                    state.ended_at_ms = Some(execution.requested_at_ms);
                    state.reason = Some(ScheduledExecutionReason::RunnerStartFailed);
                    Ok(())
                })
                .unwrap();
        }
        let (active, _) = registry
            .durable
            .claim_schedule_execution(&schedule.id, &Uuid::new_v4().to_string())
            .unwrap();
        let executions_before = registry
            .durable
            .scheduled_executions(None, Some(&schedule.id))
            .unwrap();
        assert_eq!(executions_before.len(), 101);
        registry.fail_next_persistence();

        assert!(registry.shutdown().is_err());

        assert!(!registry.runtimes.is_stopping());
        assert!(registry.workspace(&workspace.id).is_ok());
        assert_eq!(shell.snapshot().unwrap().status, ShellStatus::Pending);
        assert_eq!(
            registry
                .durable
                .scheduled_executions(None, Some(&schedule.id))
                .unwrap(),
            executions_before
        );
        let restored_active = registry
            .durable
            .execution(&active.id)
            .unwrap()
            .snapshot()
            .unwrap();
        assert_eq!(restored_active.revision, 1);
        assert_eq!(restored_active.state, ScheduledExecutionState::Claimed);
        assert_eq!(restored_active.reason, None);
        assert_eq!(restored_active.outcome, None);
        let transitions = lock(&registry.events.transitions).unwrap();
        assert_eq!(transitions.lifecycle_event_reservation, 0);
        assert_eq!(transitions.pending_durable_events.len(), 1);
        assert!(matches!(
            transitions.pending_durable_events[0].as_slice(),
            [DaemonEventKind::RunExited { .. }]
        ));
        drop(transitions);

        registry.close_workspace(&workspace.id).unwrap();
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn shutdown_reserves_pending_runtime_and_per_shell_compensation_capacity() {
        let registry = DaemonService::default();
        let workspace = registry
            .create_workspace(
                "capacity".into(),
                vec![
                    ShellSpec::login("first", env::temp_dir()),
                    ShellSpec::login("second", env::temp_dir()),
                ],
            )
            .unwrap();
        lock(&registry.events.transitions)
            .unwrap()
            .pending_runtime_events
            .push_back(DaemonEventKind::OutputChanged {
                workspace_id: workspace.id.clone(),
                shell_id: workspace.shells[0].id.clone(),
                run_id: "pending-run".into(),
                output_revision: 1,
            });
        lock(&registry.events.state).unwrap().latest_id = u64::MAX - 2;

        assert!(registry.shutdown().is_err());

        assert!(!registry.runtimes.is_stopping());
        assert_eq!(registry.snapshot().unwrap().workspaces.len(), 1);
        assert_eq!(
            lock(&registry.events.transitions)
                .unwrap()
                .lifecycle_event_reservation,
            0
        );
        lock(&registry.events.state).unwrap().latest_id = 0;
        lock(&registry.events.transitions)
            .unwrap()
            .pending_runtime_events
            .clear();
        registry.close_workspace(&workspace.id).unwrap();
    }

    #[test]
    fn deferred_natural_exit_retries_after_unrelated_lifecycle_reservation() {
        let registry = Arc::new(DaemonService::default());
        let (workspace, target, _target_runtime) = running_shell(&registry);
        let unrelated_snapshot = registry
            .create_shell(
                &workspace.id,
                ShellSpec {
                    name: "unrelated".into(),
                    command: vec!["/bin/sleep".into(), "30".into()],
                    cwd: env::temp_dir(),
                },
            )
            .unwrap();
        let unrelated = registry.shell(&unrelated_snapshot.id).unwrap();
        let unrelated_run = Arc::new(ShellRun::new(1));
        let (unrelated_runtime, _reader) = spawn_runtime(
            &unrelated,
            &unrelated_run,
            "agents",
            "unrelated",
            &profile(),
            None,
            RuntimeRecovery::default(),
        )
        .unwrap();
        *lock(&unrelated.last_run).unwrap() = Some(unrelated_run.persisted(profile()).unwrap());
        *lock(&unrelated.lifecycle).unwrap() = ShellLifecycle::Running {
            profile: profile(),
            run: Arc::clone(&unrelated_run),
            runtime: Arc::clone(&unrelated_runtime),
        };
        registry
            .runtimes
            .stop_runtime(&unrelated)
            .map_err(|error| error.source)
            .unwrap();
        lock(&registry.events.state).unwrap().latest_id = u64::MAX - 1;
        let mut transaction = registry.events.transaction().unwrap();
        transaction.reserve_with_pending(1).unwrap();
        transaction.begin_lifecycle_reservation(1);
        drop(transaction);

        let started = Instant::now();
        assert_eq!(
            registry
                .try_record_run_exit(&unrelated, &unrelated_run, &unrelated_runtime, Some(0))
                .unwrap(),
            RunExitRecord::Deferred
        );
        assert!(started.elapsed() < Duration::from_millis(100));
        DaemonService::defer_run_exit(
            Arc::downgrade(&registry),
            Arc::clone(&unrelated),
            Arc::clone(&unrelated_run),
            Arc::clone(&unrelated_runtime),
            Some(0),
        )
        .unwrap();

        let mut transaction = registry.events.transaction().unwrap();
        transaction.release_lifecycle_reservation();
        drop(transaction);
        registry.events.notify();

        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            let exited = matches!(
                unrelated.snapshot().unwrap().status,
                ShellStatus::Exited { code: Some(0) }
            );
            let exit_events = lock(&registry.events.state)
                .unwrap()
                .events
                .iter()
                .filter(|event| {
                    matches!(
                        &event.kind,
                        DaemonEventKind::RunExited { shell_id, .. }
                            if shell_id == &unrelated.id
                    )
                })
                .count();
            if exited && exit_events == 1 {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "deferred run exit did not commit"
            );
            thread::sleep(IO_RETRY_DELAY);
        }
        thread::sleep(Duration::from_millis(20));
        assert_eq!(
            lock(&registry.events.state)
                .unwrap()
                .events
                .iter()
                .filter(|event| {
                    matches!(
                        &event.kind,
                        DaemonEventKind::RunExited { shell_id, .. }
                            if shell_id == &unrelated.id
                    )
                })
                .count(),
            1
        );

        let mut events = lock(&registry.events.state).unwrap();
        events.events.clear();
        events.latest_id = u64::MAX - 1;
        drop(events);
        let (target_run, target_runtime) = match &*lock(&target.lifecycle).unwrap() {
            ShellLifecycle::Running { run, runtime, .. } => (Arc::clone(run), Arc::clone(runtime)),
            _ => panic!("expected running target shell"),
        };
        registry
            .runtimes
            .stop_runtime(&target)
            .map_err(|error| error.source)
            .unwrap();
        let mut transaction = registry.events.transaction().unwrap();
        transaction.reserve_with_pending(1).unwrap();
        transaction.begin_lifecycle_reservation(1);
        drop(transaction);
        assert_eq!(
            registry
                .try_record_run_exit(&target, &target_run, &target_runtime, None)
                .unwrap(),
            RunExitRecord::Deferred
        );
        DaemonService::defer_run_exit(
            Arc::downgrade(&registry),
            Arc::clone(&target),
            target_run,
            target_runtime,
            None,
        )
        .unwrap();
        let _rollback = registry.runtimes.finalize_stop(&target).unwrap();
        let mut transaction = registry.events.transaction().unwrap();
        transaction.release_lifecycle_reservation();
        drop(transaction);
        registry.events.notify();
        thread::sleep(Duration::from_millis(20));
        assert!(
            lock(&registry.events.state)
                .unwrap()
                .events
                .iter()
                .all(|event| !matches!(
                    &event.kind,
                    DaemonEventKind::RunExited { shell_id, .. } if shell_id == &target.id
                ))
        );

        let mut events = lock(&registry.events.state).unwrap();
        events.latest_id = 0;
        events.events.clear();
        drop(events);
        registry.close_workspace(&workspace.id).unwrap();
    }

    #[test]
    fn failed_workspace_close_compensates_every_stopped_shell() {
        let directory =
            env::temp_dir().join(format!("boomux-close-compensation-{}", Uuid::new_v4()));
        let registry = DaemonService::restore(
            StateStore::at(directory.join("state/state.json")),
            false,
            None,
        )
        .unwrap();
        let (workspace, first, _runtime) = running_shell(&registry);
        let second_snapshot = registry
            .create_shell(
                &workspace.id,
                ShellSpec {
                    name: "second".into(),
                    command: vec!["/bin/sleep".into(), "30".into()],
                    cwd: env::temp_dir(),
                },
            )
            .unwrap();
        let second = registry.shell(&second_snapshot.id).unwrap();
        let second_run = Arc::new(ShellRun::new(1));
        let (second_runtime, _reader) = spawn_runtime(
            &second,
            &second_run,
            "agents",
            "second",
            &profile(),
            None,
            RuntimeRecovery::default(),
        )
        .unwrap();
        *lock(&second.last_run).unwrap() = Some(second_run.persisted(profile()).unwrap());
        *lock(&second.lifecycle).unwrap() = ShellLifecycle::Running {
            profile: profile(),
            run: second_run,
            runtime: second_runtime,
        };
        registry.fail_next_persistence();

        assert!(registry.close_workspace(&workspace.id).is_err());

        let restored = registry
            .workspace(&workspace.id)
            .unwrap()
            .snapshot(&registry.durable)
            .unwrap();
        assert_eq!(restored.shells.len(), 2);
        assert!(
            restored
                .shells
                .iter()
                .all(|shell| shell.status == ShellStatus::Pending)
        );
        assert!(first.snapshot().unwrap().run.is_none());
        assert!(second.snapshot().unwrap().run.is_none());
        let transitions = lock(&registry.events.transitions).unwrap();
        assert_eq!(transitions.pending_durable_events.len(), 1);
        assert_eq!(transitions.pending_durable_events[0].len(), 2);
        assert!(
            transitions.pending_durable_events[0]
                .iter()
                .all(|event| matches!(event, DaemonEventKind::RunExited { .. }))
        );
        drop(transitions);
        assert!(lock(&registry.events.state).unwrap().events.is_empty());

        registry.close_workspace(&workspace.id).unwrap();
        let events = lock(&registry.events.state).unwrap();
        assert_eq!(events.events.len(), 3);
        assert!(matches!(
            events.events.back().map(|event| &event.kind),
            Some(DaemonEventKind::WorkspaceClosed { .. })
        ));
        drop(events);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn slow_persistence_does_not_block_pty_draining() {
        let directory = env::temp_dir().join(format!("boomux-slow-store-{}", Uuid::new_v4()));
        let gate = Arc::new((Mutex::new((false, false, false)), Condvar::new()));
        let hook_gate = Arc::clone(&gate);
        let store = StateStore::at_with_save_hook(
            directory.join("state/state.json"),
            Arc::new(move || {
                let (state, changed) = &*hook_gate;
                let mut state = state.lock().unwrap();
                if !state.0 {
                    return;
                }
                state.1 = true;
                changed.notify_all();
                while !state.2 {
                    state = changed.wait(state).unwrap();
                }
            }),
        );
        let registry = Arc::new(DaemonService::restore(store, false, None).unwrap());
        let workspace = registry
            .create_workspace(
                "slow-store".into(),
                vec![ShellSpec {
                    name: "draining".into(),
                    command: vec![
                        "/bin/sh".into(),
                        "-c".into(),
                        "stty -echo; while IFS= read -r line; do printf 'observed:%s\\n' \"$line\"; done"
                            .into(),
                    ],
                    cwd: env::temp_dir(),
                }],
            )
            .unwrap();
        let shell = registry.shell(&workspace.shells[0].id).unwrap();
        let terminal_profile = profile();
        let run = Arc::new(ShellRun::new(1));
        let (runtime, reader) = spawn_runtime(
            &shell,
            &run,
            "slow-store",
            "draining",
            &terminal_profile,
            None,
            RuntimeRecovery::default(),
        )
        .unwrap();
        *lock(&shell.last_run).unwrap() = Some(run.persisted(terminal_profile.clone()).unwrap());
        *lock(&shell.lifecycle).unwrap() = ShellLifecycle::Running {
            profile: terminal_profile,
            run: Arc::clone(&run),
            runtime: Arc::clone(&runtime),
        };
        start_pty_reader(
            Arc::downgrade(&registry),
            Arc::clone(&shell),
            Arc::clone(&run),
            Arc::clone(&runtime),
            reader,
            false,
        )
        .unwrap();
        lock(&gate.0).unwrap().0 = true;

        let mutation_registry = Arc::clone(&registry);
        let workspace_id = workspace.id.clone();
        let mutation = thread::spawn(move || {
            mutation_registry.dispatch(Request::RenameWorkspace {
                workspace_id,
                name: "renamed".into(),
            })
        });
        {
            let (state, changed) = &*gate;
            let state = state.lock().unwrap();
            let (state, timeout) = changed
                .wait_timeout_while(state, Duration::from_secs(2), |state| !state.1)
                .unwrap();
            assert!(!timeout.timed_out(), "persistence write did not start");
            drop(state);
        }

        let previous_revision = run.output_revision.load(Ordering::Acquire);
        lock(&runtime.master)
            .unwrap()
            .write(b"during-save\n")
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline
            && !lock(&runtime.terminal)
                .unwrap()
                .plain_text()
                .contains("observed:during-save")
        {
            thread::sleep(IO_RETRY_DELAY);
        }
        assert!(run.output_revision.load(Ordering::Acquire) > previous_revision);
        assert!(
            lock(&runtime.terminal)
                .unwrap()
                .plain_text()
                .contains("observed:during-save")
        );
        let response = registry
            .read_shell_at(
                &shell.id,
                MAX_SHELL_READ_BYTES,
                Some(&run.id),
                Some(previous_revision),
                500,
            )
            .unwrap();
        assert!(matches!(
            response,
            Response::OutputState { changed: true, .. }
        ));

        {
            let (state, changed) = &*gate;
            let mut state = state.lock().unwrap();
            state.2 = true;
            changed.notify_all();
        }
        assert!(mutation.join().unwrap().is_ok());
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline
            && !lock(&registry.events.state)
                .unwrap()
                .events
                .iter()
                .any(|event| matches!(event.kind, DaemonEventKind::OutputChanged { .. }))
        {
            thread::sleep(IO_RETRY_DELAY);
        }
        let events = lock(&registry.events.state).unwrap();
        let rename = events
            .events
            .iter()
            .position(|event| matches!(event.kind, DaemonEventKind::WorkspaceRenamed { .. }))
            .unwrap();
        let output = events
            .events
            .iter()
            .position(|event| matches!(event.kind, DaemonEventKind::OutputChanged { .. }))
            .unwrap();
        assert!(rename < output);
        drop(events);
        shell.kill().unwrap();
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn independent_pty_readers_process_output_concurrently() {
        let registry = Arc::new(DaemonService::default());
        let mut shells = Vec::new();
        for index in 0..2 {
            let shell = create_pending_shell(
                "workspace-id",
                ShellSpec {
                    name: format!("concurrent-{index}"),
                    command: vec![
                        "/bin/sh".into(),
                        "-c".into(),
                        "stty -echo; while IFS= read -r line; do printf '%s\\n' \"$line\"; done"
                            .into(),
                    ],
                    cwd: env::temp_dir(),
                },
            )
            .unwrap();
            let run = Arc::new(ShellRun::new(1));
            let terminal_profile = profile();
            let (runtime, reader) = spawn_runtime(
                &shell,
                &run,
                "workspace",
                &format!("concurrent-{index}"),
                &terminal_profile,
                None,
                RuntimeRecovery::default(),
            )
            .unwrap();
            *lock(&shell.last_run).unwrap() =
                Some(run.persisted(terminal_profile.clone()).unwrap());
            *lock(&shell.lifecycle).unwrap() = ShellLifecycle::Running {
                profile: terminal_profile,
                run: Arc::clone(&run),
                runtime: Arc::clone(&runtime),
            };
            start_pty_reader(
                Arc::downgrade(&registry),
                Arc::clone(&shell),
                Arc::clone(&run),
                Arc::clone(&runtime),
                reader,
                false,
            )
            .unwrap();
            shells.push((shell, run, runtime));
        }

        for (index, (_, _, runtime)) in shells.iter().enumerate() {
            let input = (0..500)
                .map(|line| format!("shell-{index}-{line}\n"))
                .collect::<String>();
            lock(&runtime.master)
                .unwrap()
                .write(input.as_bytes())
                .unwrap();
        }
        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline
            && shells.iter().enumerate().any(|(index, (_, _, runtime))| {
                !lock(&runtime.terminal)
                    .unwrap()
                    .plain_text()
                    .contains(&format!("shell-{index}-499"))
            })
        {
            thread::sleep(IO_RETRY_DELAY);
        }
        for (index, (shell, run, runtime)) in shells.into_iter().enumerate() {
            assert!(run.output_revision.load(Ordering::Acquire) >= 2);
            assert!(
                lock(&runtime.terminal)
                    .unwrap()
                    .plain_text()
                    .contains(&format!("shell-{index}-499"))
            );
            shell.kill().unwrap();
        }
    }

    #[test]
    fn coordinated_workspace_batch_is_included_in_baseline_cursor() {
        let registry = DaemonService::default();
        let response = registry
            .dispatch(Request::CreateWorkspace {
                name: "coordinated".into(),
                default_cwd: None,
                shells: vec![
                    ShellSpec::login("one", env::temp_dir()),
                    ShellSpec::login("two", env::temp_dir()),
                ],
            })
            .unwrap();
        assert!(matches!(response, Response::Workspace { .. }));

        let Response::Events {
            cursor,
            snapshot: Some(snapshot),
            ..
        } = registry.read_events(None, 256, 0).unwrap()
        else {
            panic!("expected coordinated baseline");
        };
        assert_eq!(snapshot.workspaces.len(), 1);
        assert_eq!(snapshot.workspaces[0].shells.len(), 2);
        assert_eq!(cursor.event_id, 3);
        let Response::Events { events, .. } = registry.read_events(Some(&cursor), 256, 0).unwrap()
        else {
            panic!("expected event page");
        };
        assert!(events.is_empty());
        let event_state = lock(&registry.events.state).unwrap();
        let events = &event_state.events;
        assert!(matches!(
            events[0].kind,
            DaemonEventKind::WorkspaceCreated { .. }
        ));
        assert!(
            events
                .iter()
                .skip(1)
                .all(|event| matches!(event.kind, DaemonEventKind::ShellCreated { .. }))
        );
    }

    #[test]
    fn launcher_mutations_are_coordinated_and_names_are_unique_per_workspace() {
        let registry = DaemonService::default();
        let Response::Workspace { workspace } = registry
            .dispatch(Request::CreateWorkspace {
                name: "launchers".into(),
                default_cwd: None,
                shells: Vec::new(),
            })
            .unwrap()
        else {
            panic!("expected workspace");
        };
        let spec = WorkspaceLauncherSpec {
            name: "editor".into(),
            command: vec!["zeditor".into(), ".".into()],
            cwd: env::temp_dir(),
        };
        let Response::Launcher { launcher } = registry
            .dispatch(Request::CreateLauncher {
                workspace_id: workspace.id.clone(),
                spec: spec.clone(),
            })
            .unwrap()
        else {
            panic!("expected launcher");
        };
        assert!(
            registry
                .dispatch(Request::CreateLauncher {
                    workspace_id: workspace.id.clone(),
                    spec,
                })
                .is_err()
        );
        let snapshot = registry
            .workspace(&workspace.id)
            .unwrap()
            .snapshot(&registry.durable)
            .unwrap();
        assert_eq!(snapshot.launchers, vec![launcher]);
        assert_eq!(
            lock(&registry.events.state)
                .unwrap()
                .events
                .iter()
                .filter(|event| matches!(event.kind, DaemonEventKind::LauncherCreated { .. }))
                .count(),
            1
        );
    }

    #[test]
    fn agent_registration_and_reports_enforce_run_binding_and_complete_monotonically() {
        let registry = DaemonService::default();
        let (workspace, shell, _runtime) = running_shell(&registry);
        let run_id = shell.snapshot().unwrap().run.unwrap().id;

        let wrong_run = registry.dispatch(Request::RegisterAgent {
            shell_id: shell.id.clone(),
            run_id: Uuid::new_v4().to_string(),
            spec: agent_spec(AgentState::Working),
        });
        assert_eq!(wrong_run.unwrap_err().wire_code(), ErrorCode::RunChanged);

        let Response::Agent { agent } = registry
            .dispatch(Request::RegisterAgent {
                shell_id: shell.id.clone(),
                run_id: run_id.clone(),
                spec: agent_spec(AgentState::Working),
            })
            .unwrap()
        else {
            panic!("expected registered agent");
        };
        assert_eq!(agent.cwd.as_deref(), Some(shell.cwd.as_path()));
        assert_eq!(agent.observation.revision, 1);
        assert!(agent.ended_at_ms.is_none());
        assert_eq!(
            registry
                .dispatch(Request::ReportAgent {
                    agent_id: agent.id.clone(),
                    run_id: Uuid::new_v4().to_string(),
                    report: agent_spec(AgentState::Idle).report,
                })
                .unwrap_err()
                .wire_code(),
            ErrorCode::RunChanged
        );

        let Response::Agent { agent } = registry
            .dispatch(Request::ReportAgent {
                agent_id: agent.id.clone(),
                run_id,
                report: agent_spec(AgentState::Done).report,
            })
            .unwrap()
        else {
            panic!("expected completed agent");
        };
        assert_eq!(agent.observation.revision, 2);
        assert_eq!(agent.observation.state, AgentState::Done);
        assert_eq!(
            agent.attention.as_ref().map(|attention| attention.reason),
            Some(AgentAttentionReason::Completed)
        );
        assert_eq!(
            agent.attention.as_ref().unwrap().observation,
            agent.observation
        );
        assert_eq!(agent.ended_at_ms, Some(agent.observation.observed_at_ms));
        assert_eq!(
            registry.snapshot().unwrap().workspaces[0].agents,
            vec![agent.clone()]
        );
        {
            let event_state = lock(&registry.events.state).unwrap();
            assert!(matches!(
                event_state.events[0].kind,
                DaemonEventKind::AgentRegistered { .. }
            ));
            assert!(matches!(
                event_state.events[1].kind,
                DaemonEventKind::AgentCompleted { .. }
            ));
        }
        assert!(
            registry
                .dispatch(Request::ReportAgent {
                    agent_id: agent.id,
                    run_id: agent.run_id,
                    report: agent_spec(AgentState::Idle).report,
                })
                .is_err()
        );

        shell.kill().unwrap();
        registry.close_workspace(&workspace.id).unwrap();
    }

    #[test]
    fn ensure_agent_reuses_identity_without_events_or_revision_changes() {
        let registry = DaemonService::default();
        let (workspace, shell, _runtime) = running_shell(&registry);
        let run_id = shell.snapshot().unwrap().run.unwrap().id;
        let spec = agent_spec(AgentState::Working);

        let Response::Agent { agent: created } = registry
            .dispatch(Request::EnsureAgent {
                shell_id: shell.id.clone(),
                run_id: run_id.clone(),
                spec: spec.clone(),
            })
            .unwrap()
        else {
            panic!("expected ensured agent");
        };
        let event_id = lock(&registry.events.state).unwrap().latest_id;
        let Response::Agent { agent: reused } = registry
            .dispatch(Request::EnsureAgent {
                shell_id: shell.id.clone(),
                run_id,
                spec,
            })
            .unwrap()
        else {
            panic!("expected reused agent");
        };

        assert_eq!(reused, created);
        assert_eq!(lock(&registry.events.state).unwrap().latest_id, event_id);
        assert_eq!(registry.snapshot().unwrap().workspaces[0].agents.len(), 1);
        shell.kill().unwrap();
        registry.close_workspace(&workspace.id).unwrap();
    }

    #[test]
    fn concurrent_ensure_agent_creates_one_identity() {
        let registry = Arc::new(DaemonService::default());
        let (workspace, shell, _runtime) = running_shell(&registry);
        let run_id = shell.snapshot().unwrap().run.unwrap().id;
        let barrier = Arc::new(Barrier::new(3));
        let mut threads = Vec::new();
        for _ in 0..2 {
            let registry = Arc::clone(&registry);
            let shell_id = shell.id.clone();
            let run_id = run_id.clone();
            let barrier = Arc::clone(&barrier);
            threads.push(thread::spawn(move || {
                barrier.wait();
                let Response::Agent { agent } = registry
                    .dispatch(Request::EnsureAgent {
                        shell_id,
                        run_id,
                        spec: agent_spec(AgentState::Working),
                    })
                    .unwrap()
                else {
                    panic!("expected ensured agent");
                };
                agent.id
            }));
        }
        barrier.wait();
        let ids = threads
            .into_iter()
            .map(|thread| thread.join().unwrap())
            .collect::<Vec<_>>();

        assert_eq!(ids[0], ids[1]);
        assert_eq!(registry.snapshot().unwrap().workspaces[0].agents.len(), 1);
        shell.kill().unwrap();
        registry.close_workspace(&workspace.id).unwrap();
    }

    #[test]
    fn ensure_agent_resolves_only_a_unique_active_legacy_match() {
        let registry = DaemonService::default();
        let (workspace, shell, _runtime) = running_shell(&registry);
        let run_id = shell.snapshot().unwrap().run.unwrap().id;
        let completed = registry
            .register_agent(&shell.id, &run_id, agent_spec(AgentState::Done))
            .unwrap();
        let active = registry
            .register_agent(&shell.id, &run_id, agent_spec(AgentState::Working))
            .unwrap();

        let (ensured, created) = registry
            .ensure_agent(&shell.id, &run_id, agent_spec(AgentState::Working))
            .unwrap();
        assert!(!created);
        assert_eq!(ensured.id, active.id);
        assert_ne!(ensured.id, completed.id);

        registry
            .register_agent(&shell.id, &run_id, agent_spec(AgentState::Working))
            .unwrap();
        assert_eq!(
            registry
                .ensure_agent(&shell.id, &run_id, agent_spec(AgentState::Working))
                .unwrap_err()
                .wire_code(),
            ErrorCode::AlreadyExists
        );
        shell.kill().unwrap();
        registry.close_workspace(&workspace.id).unwrap();
    }

    #[test]
    fn ensure_agent_requires_external_id_and_distinguishes_runs() {
        let registry = DaemonService::default();
        let (workspace, shell, runtime) = running_shell(&registry);
        let first_run_id = shell.snapshot().unwrap().run.unwrap().id;
        let mut missing_id = agent_spec(AgentState::Working);
        missing_id.external_session_id = None;
        assert_eq!(
            registry
                .dispatch(Request::EnsureAgent {
                    shell_id: shell.id.clone(),
                    run_id: first_run_id.clone(),
                    spec: missing_id,
                })
                .unwrap_err()
                .wire_code(),
            ErrorCode::InvalidArgument
        );
        let Response::Agent { agent: first } = registry
            .dispatch(Request::EnsureAgent {
                shell_id: shell.id.clone(),
                run_id: first_run_id.clone(),
                spec: agent_spec(AgentState::Working),
            })
            .unwrap()
        else {
            panic!("expected first agent");
        };

        let second_run = Arc::new(ShellRun::new(2));
        *lock(&shell.lifecycle).unwrap() = ShellLifecycle::Running {
            profile: profile(),
            run: Arc::clone(&second_run),
            runtime: Arc::clone(&runtime),
        };
        let Response::Agent { agent: recovered } = registry
            .dispatch(Request::EnsureAgent {
                shell_id: shell.id.clone(),
                run_id: first_run_id,
                spec: agent_spec(AgentState::Working),
            })
            .unwrap()
        else {
            panic!("expected recovered first agent");
        };
        let Response::Agent { agent: second } = registry
            .dispatch(Request::EnsureAgent {
                shell_id: shell.id.clone(),
                run_id: second_run.id.clone(),
                spec: agent_spec(AgentState::Working),
            })
            .unwrap()
        else {
            panic!("expected second agent");
        };

        assert_eq!(recovered.id, first.id);
        assert_ne!(second.id, first.id);
        shell.kill().unwrap();
        registry.close_workspace(&workspace.id).unwrap();
    }

    #[test]
    fn reports_obey_authority_and_idempotent_completion_rules() {
        let registry = DaemonService::default();
        let (workspace, shell, _runtime) = running_shell(&registry);
        let run_id = shell.snapshot().unwrap().run.unwrap().id;
        let mut spec = agent_spec(AgentState::Working);
        spec.report = agent_report(
            AgentState::Working,
            AgentAuthority::ProcessAdapter,
            "process working",
        );
        let Response::Agent { agent } = registry
            .dispatch(Request::RegisterAgent {
                shell_id: shell.id.clone(),
                run_id: run_id.clone(),
                spec,
            })
            .unwrap()
        else {
            panic!("expected agent");
        };
        let registered_event_id = lock(&registry.events.state).unwrap().latest_id;

        for report in [
            agent_report(
                AgentState::Done,
                AgentAuthority::TerminalHeuristic,
                "weak done",
            ),
            agent_report(
                AgentState::Working,
                AgentAuthority::ProcessAdapter,
                "process working",
            ),
            agent_report(
                AgentState::Working,
                AgentAuthority::ProcessAdapter,
                "updated working evidence",
            ),
        ] {
            let Response::Agent { agent: unchanged } = registry
                .dispatch(Request::ReportAgent {
                    agent_id: agent.id.clone(),
                    run_id: run_id.clone(),
                    report,
                })
                .unwrap()
            else {
                panic!("expected unchanged agent");
            };
            assert_eq!(unchanged, agent);
        }
        assert_eq!(
            lock(&registry.events.state).unwrap().latest_id,
            registered_event_id
        );

        let completion = agent_report(
            AgentState::Done,
            AgentAuthority::LifecycleIntegration,
            "lifecycle done",
        );
        let Response::Agent { agent: completed } = registry
            .dispatch(Request::ReportAgent {
                agent_id: agent.id.clone(),
                run_id: run_id.clone(),
                report: completion.clone(),
            })
            .unwrap()
        else {
            panic!("expected completed agent");
        };
        let completion_event_id = lock(&registry.events.state).unwrap().latest_id;
        let Response::Agent { agent: retried } = registry
            .dispatch(Request::ReportAgent {
                agent_id: agent.id.clone(),
                run_id: run_id.clone(),
                report: completion,
            })
            .unwrap()
        else {
            panic!("expected retried completion");
        };
        assert_eq!(retried, completed);
        assert_eq!(
            lock(&registry.events.state).unwrap().latest_id,
            completion_event_id
        );
        assert!(
            registry
                .dispatch(Request::ReportAgent {
                    agent_id: agent.id.clone(),
                    run_id: run_id.clone(),
                    report: agent_report(
                        AgentState::Done,
                        AgentAuthority::LifecycleIntegration,
                        "conflicting done",
                    ),
                })
                .is_err()
        );
        assert!(
            registry
                .dispatch(Request::ReportAgent {
                    agent_id: agent.id,
                    run_id,
                    report: agent_report(
                        AgentState::Done,
                        AgentAuthority::DaemonLifecycle,
                        "external daemon claim",
                    ),
                })
                .is_err()
        );
        shell.kill().unwrap();
        registry.close_workspace(&workspace.id).unwrap();
    }

    #[test]
    fn failed_agent_mutation_restores_observation_revision() {
        let directory = env::temp_dir().join(format!("boomux-agent-undo-{}", Uuid::new_v4()));
        let registry = DaemonService::restore(
            StateStore::at(directory.join("state/state.json")),
            false,
            None,
        )
        .unwrap();
        let (workspace, shell, _runtime) = running_shell(&registry);
        let run_id = shell.snapshot().unwrap().run.unwrap().id;
        let Response::Agent { agent } = registry
            .dispatch(Request::RegisterAgent {
                shell_id: shell.id.clone(),
                run_id: run_id.clone(),
                spec: agent_spec(AgentState::Working),
            })
            .unwrap()
        else {
            panic!("expected agent");
        };
        registry.fail_next_persistence();

        let result = registry.dispatch(Request::ReportAgent {
            agent_id: agent.id.clone(),
            run_id: run_id.clone(),
            report: agent_spec(AgentState::Blocked).report,
        });

        assert!(result.is_err());
        let restored = registry.agent(&agent.id).unwrap().snapshot().unwrap();
        assert_eq!(restored.observation.revision, 1);
        assert_eq!(restored.observation.state, AgentState::Working);
        assert!(restored.attention.is_none());
        shell.kill().unwrap();
        registry.close_workspace(&workspace.id).unwrap();
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn agent_attention_is_raised_preserved_and_superseded_by_completion() {
        let registry = DaemonService::default();
        let (workspace, shell, _runtime) = running_shell(&registry);
        let run_id = shell.snapshot().unwrap().run.unwrap().id;
        let registered = registry
            .register_agent(&shell.id, &run_id, agent_spec(AgentState::Blocked))
            .unwrap();
        let registered_id = registered.id.clone();
        let blocked = registered.attention.clone().unwrap();
        assert_eq!(blocked.reason, AgentAttentionReason::Blocked);
        assert_eq!(blocked.observation, registered.observation);

        let (unchanged, changed, _) = registry
            .report_agent(
                &registered_id,
                &run_id,
                agent_report(
                    AgentState::Working,
                    AgentAuthority::TerminalHeuristic,
                    "weak working",
                ),
            )
            .unwrap();
        assert!(!changed);
        assert_eq!(unchanged.attention.as_ref(), Some(&blocked));

        let (idle, changed, _) = registry
            .report_agent(&registered_id, &run_id, agent_spec(AgentState::Idle).report)
            .unwrap();
        assert!(changed);
        assert_eq!(idle.attention.as_ref(), Some(&blocked));

        let duplicate = agent_spec(AgentState::Idle).report;
        let (idle_again, changed, _) = registry.report_agent(&idle.id, &run_id, duplicate).unwrap();
        assert!(!changed);
        assert_eq!(idle_again.attention.as_ref(), Some(&blocked));

        let (done, changed, completed) = registry
            .report_agent(&idle.id, &run_id, agent_spec(AgentState::Done).report)
            .unwrap();
        assert!(changed && completed);
        let attention = done.attention.unwrap();
        assert_eq!(attention.reason, AgentAttentionReason::Completed);
        assert_eq!(attention.observation, done.observation);
        shell.kill().unwrap();
        registry.close_workspace(&workspace.id).unwrap();
    }

    #[test]
    fn notifications_are_deduplicated_and_follow_current_attention() {
        let (registry, sink) = notification_registry(NotificationSettings {
            enabled: true,
            blocked: true,
            completed: true,
            ..Default::default()
        });
        let (workspace, shell, _runtime) = running_shell(&registry);
        let run_id = shell.snapshot().unwrap().run.unwrap().id;

        let Response::Agent { agent: blocked } = registry
            .dispatch(Request::RegisterAgent {
                shell_id: shell.id.clone(),
                run_id: run_id.clone(),
                spec: agent_spec(AgentState::Blocked),
            })
            .unwrap()
        else {
            panic!("expected blocked agent");
        };
        assert_eq!(sink.requests.lock().unwrap().len(), 1);

        registry
            .dispatch(Request::ReportAgent {
                agent_id: blocked.id.clone(),
                run_id: run_id.clone(),
                report: agent_spec(AgentState::Blocked).report,
            })
            .unwrap();
        registry
            .dispatch(Request::ReportAgent {
                agent_id: blocked.id.clone(),
                run_id: run_id.clone(),
                report: agent_report(
                    AgentState::Blocked,
                    AgentAuthority::LifecycleIntegration,
                    "same blocker with updated evidence",
                ),
            })
            .unwrap();
        registry
            .dispatch(Request::ReportAgent {
                agent_id: blocked.id.clone(),
                run_id: run_id.clone(),
                report: agent_report(
                    AgentState::Working,
                    AgentAuthority::TerminalHeuristic,
                    "lower authority",
                ),
            })
            .unwrap();
        assert_eq!(sink.requests.lock().unwrap().len(), 1);

        let Response::Agent { agent: working } = registry
            .dispatch(Request::ReportAgent {
                agent_id: blocked.id.clone(),
                run_id: run_id.clone(),
                report: agent_spec(AgentState::Working).report,
            })
            .unwrap()
        else {
            panic!("expected working agent");
        };
        assert!(working.attention.is_some());
        assert_eq!(sink.requests.lock().unwrap().len(), 1);

        let Response::Agent { agent: reblocked } = registry
            .dispatch(Request::ReportAgent {
                agent_id: blocked.id.clone(),
                run_id: run_id.clone(),
                report: agent_spec(AgentState::Blocked).report,
            })
            .unwrap()
        else {
            panic!("expected reblocked agent");
        };
        assert_eq!(sink.requests.lock().unwrap().len(), 2);

        registry
            .dispatch(Request::AcknowledgeAgentAttention {
                agent_id: reblocked.id.clone(),
                observation_revision: reblocked.observation.revision,
            })
            .unwrap();
        assert_eq!(sink.requests.lock().unwrap().len(), 2);

        registry
            .dispatch(Request::ReportAgent {
                agent_id: reblocked.id,
                run_id: run_id.clone(),
                report: agent_spec(AgentState::Done).report,
            })
            .unwrap();
        assert_eq!(sink.requests.lock().unwrap().len(), 3);

        registry
            .dispatch(Request::RegisterAgent {
                shell_id: shell.id.clone(),
                run_id,
                spec: agent_spec(AgentState::Done),
            })
            .unwrap();
        let requests = sink.requests.lock().unwrap();
        assert_eq!(requests.len(), 4);
        assert_eq!(requests[0].reason, NotificationReason::Blocked);
        assert_eq!(requests[1].reason, NotificationReason::Blocked);
        assert_eq!(requests[2].reason, NotificationReason::Completed);
        assert_eq!(requests[3].reason, NotificationReason::Completed);
        assert_eq!(requests[0].workspace, "agents");
        assert_eq!(requests[0].shell, "agent-shell");
        drop(requests);

        shell.kill().unwrap();
        registry.close_workspace(&workspace.id).unwrap();
    }

    #[test]
    fn working_to_idle_notifies_without_completed_attention() {
        let (registry, sink) = notification_registry(NotificationSettings {
            enabled: true,
            blocked: true,
            completed: true,
            ..Default::default()
        });
        let (workspace, shell, _runtime) = running_shell(&registry);
        let run_id = shell.snapshot().unwrap().run.unwrap().id;
        let Response::Agent { agent } = registry
            .dispatch(Request::RegisterAgent {
                shell_id: shell.id.clone(),
                run_id: run_id.clone(),
                spec: agent_spec(AgentState::Working),
            })
            .unwrap()
        else {
            panic!("expected working agent");
        };

        let Response::Agent { agent } = registry
            .dispatch(Request::ReportAgent {
                agent_id: agent.id,
                run_id,
                report: agent_spec(AgentState::Idle).report,
            })
            .unwrap()
        else {
            panic!("expected idle agent");
        };

        assert!(agent.attention.is_none());
        let requests = sink.requests.lock().unwrap();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].reason, NotificationReason::Completed);
        drop(requests);
        shell.kill().unwrap();
        registry.close_workspace(&workspace.id).unwrap();
    }

    #[test]
    fn disabled_notifications_do_not_reach_sink() {
        let (registry, sink) = notification_registry(NotificationSettings::default());
        let (workspace, shell, _runtime) = running_shell(&registry);
        let run_id = shell.snapshot().unwrap().run.unwrap().id;
        registry
            .dispatch(Request::RegisterAgent {
                shell_id: shell.id.clone(),
                run_id,
                spec: agent_spec(AgentState::Blocked),
            })
            .unwrap();
        assert!(sink.requests.lock().unwrap().is_empty());
        shell.kill().unwrap();
        registry.close_workspace(&workspace.id).unwrap();
    }

    #[test]
    fn retained_agent_notifications_explain_a_removed_shell() {
        let (registry, sink) = notification_registry(NotificationSettings {
            enabled: true,
            blocked: true,
            completed: true,
            ..Default::default()
        });
        let (workspace, shell, _runtime) = running_shell(&registry);
        let run_id = shell.snapshot().unwrap().run.unwrap().id;
        let Response::Agent { agent } = registry
            .dispatch(Request::RegisterAgent {
                shell_id: shell.id.clone(),
                run_id: run_id.clone(),
                spec: agent_spec(AgentState::Working),
            })
            .unwrap()
        else {
            panic!("expected working agent");
        };
        registry
            .dispatch(Request::CloseShell {
                shell_id: shell.id.clone(),
            })
            .unwrap();
        registry
            .dispatch(Request::ReportAgent {
                agent_id: agent.id,
                run_id,
                report: agent_spec(AgentState::Blocked).report,
            })
            .unwrap();

        let requests = sink.requests.lock().unwrap();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].workspace, "agents");
        assert_eq!(requests[0].shell, "removed");
        drop(requests);
        registry.close_workspace(&workspace.id).unwrap();
    }

    #[test]
    fn failed_persistence_does_not_notify() {
        let directory = env::temp_dir().join(format!("boomux-notification-{}", Uuid::new_v4()));
        let state_directory = directory.join("state");
        let mut registry = DaemonService::restore(
            StateStore::at(state_directory.join("state.json")),
            false,
            None,
        )
        .unwrap();
        let sink = Arc::new(RecordingNotificationSink::default());
        registry.notification_settings = NotificationDeliverySettings {
            desktop: NotificationSettings {
                enabled: true,
                blocked: true,
                completed: true,
                ..Default::default()
            },
            ..Default::default()
        };
        registry.notification_sink = sink.clone();
        let (workspace, shell, _runtime) = running_shell(&registry);
        let run_id = shell.snapshot().unwrap().run.unwrap().id;
        let Response::Agent { agent } = registry
            .dispatch(Request::RegisterAgent {
                shell_id: shell.id.clone(),
                run_id: run_id.clone(),
                spec: agent_spec(AgentState::Working),
            })
            .unwrap()
        else {
            panic!("expected working agent");
        };
        fs::remove_dir_all(&state_directory).unwrap();
        fs::write(&state_directory, b"not a directory").unwrap();

        assert!(
            registry
                .dispatch(Request::ReportAgent {
                    agent_id: agent.id,
                    run_id,
                    report: agent_spec(AgentState::Blocked).report,
                })
                .is_err()
        );
        assert!(sink.requests.lock().unwrap().is_empty());
        shell.kill().unwrap();
        let _ = workspace;
        drop(registry);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn failed_attention_acknowledgment_persistence_restores_attention() {
        let directory = env::temp_dir().join(format!("boomux-attention-{}", Uuid::new_v4()));
        let state_directory = directory.join("state");
        let registry = DaemonService::restore(
            StateStore::at(state_directory.join("state.json")),
            false,
            None,
        )
        .unwrap();
        let (_workspace, shell, _runtime) = running_shell(&registry);
        let run_id = shell.snapshot().unwrap().run.unwrap().id;
        let Response::Agent { agent } = registry
            .dispatch(Request::RegisterAgent {
                shell_id: shell.id.clone(),
                run_id,
                spec: agent_spec(AgentState::Blocked),
            })
            .unwrap()
        else {
            panic!("expected registered agent");
        };
        let event_id = lock(&registry.events.state).unwrap().latest_id;
        fs::remove_dir_all(&state_directory).unwrap();
        fs::write(&state_directory, b"not a directory").unwrap();

        let error = registry
            .dispatch(Request::AcknowledgeAgentAttention {
                agent_id: agent.id.clone(),
                observation_revision: agent.observation.revision,
            })
            .unwrap_err();

        assert_eq!(error.wire_code(), ErrorCode::PersistenceFailed);
        assert!(
            registry
                .agent(&agent.id)
                .unwrap()
                .snapshot()
                .unwrap()
                .attention
                .is_some()
        );
        assert_eq!(lock(&registry.events.state).unwrap().latest_id, event_id);
        shell.kill().unwrap();
        drop(registry);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn agent_attention_acknowledgment_is_conditional_idempotent_and_rollback_safe() {
        let registry = DaemonService::default();
        let (workspace, shell, _runtime) = running_shell(&registry);
        let run_id = shell.snapshot().unwrap().run.unwrap().id;
        let agent = registry
            .register_agent(&shell.id, &run_id, agent_spec(AgentState::Blocked))
            .unwrap();
        let revision = agent.observation.revision;

        let mismatch = registry.acknowledge_agent_attention(&agent.id, revision + 1);
        assert_eq!(mismatch.unwrap_err().wire_code(), ErrorCode::RevisionAhead);
        assert!(
            registry
                .agent(&agent.id)
                .unwrap()
                .snapshot()
                .unwrap()
                .attention
                .is_some()
        );

        registry.fail_after_next_mutation();
        let result = registry.dispatch(Request::AcknowledgeAgentAttention {
            agent_id: agent.id.clone(),
            observation_revision: revision,
        });
        assert!(result.is_err());
        assert!(
            registry
                .agent(&agent.id)
                .unwrap()
                .snapshot()
                .unwrap()
                .attention
                .is_some()
        );

        let event_id = lock(&registry.events.state).unwrap().latest_id;
        let Response::AgentAttentionAcknowledged { agent, changed } = registry
            .dispatch(Request::AcknowledgeAgentAttention {
                agent_id: agent.id.clone(),
                observation_revision: revision,
            })
            .unwrap()
        else {
            panic!("expected attention acknowledgment");
        };
        assert!(changed);
        assert!(agent.attention.is_none());
        assert_eq!(agent.observation.revision, revision);
        assert_eq!(
            lock(&registry.events.state).unwrap().latest_id,
            event_id + 1
        );

        let Response::AgentAttentionAcknowledged { agent, changed } = registry
            .dispatch(Request::AcknowledgeAgentAttention {
                agent_id: agent.id,
                observation_revision: revision + 100,
            })
            .unwrap()
        else {
            panic!("expected idempotent acknowledgment");
        };
        assert!(!changed);
        assert_eq!(agent.observation.revision, revision);
        assert_eq!(
            lock(&registry.events.state).unwrap().latest_id,
            event_id + 1
        );
        shell.kill().unwrap();
        registry.close_workspace(&workspace.id).unwrap();
    }

    #[test]
    fn agent_wait_is_revision_conditional_and_wakes_after_durable_change() {
        let registry = Arc::new(DaemonService::default());
        let (workspace, shell, _runtime) = running_shell(&registry);
        let run_id = shell.snapshot().unwrap().run.unwrap().id;
        let Response::Agent { agent } = registry
            .dispatch(Request::RegisterAgent {
                shell_id: shell.id.clone(),
                run_id: run_id.clone(),
                spec: agent_spec(AgentState::Working),
            })
            .unwrap()
        else {
            panic!("expected registered agent");
        };

        let Response::AgentWait { changed, .. } = registry.wait_agent(&agent.id, 0, 0).unwrap()
        else {
            panic!("expected immediate Agent wait");
        };
        assert!(changed);
        let Response::AgentWait { changed, .. } = registry.wait_agent(&agent.id, 1, 0).unwrap()
        else {
            panic!("expected unchanged Agent wait");
        };
        assert!(!changed);
        assert_eq!(
            registry
                .wait_agent(&agent.id, 2, 0)
                .unwrap_err()
                .wire_code(),
            ErrorCode::RevisionAhead
        );

        let waiting_registry = Arc::clone(&registry);
        let waiting_agent_id = agent.id.clone();
        let waiter =
            thread::spawn(move || waiting_registry.wait_agent(&waiting_agent_id, 1, 2_000));
        thread::sleep(Duration::from_millis(20));
        let Response::Agent { agent: blocked } = registry
            .dispatch(Request::ReportAgent {
                agent_id: agent.id.clone(),
                run_id: run_id.clone(),
                report: agent_spec(AgentState::Blocked).report,
            })
            .unwrap()
        else {
            panic!("expected changed Agent");
        };
        let Response::AgentWait {
            agent: waited,
            changed,
        } = waiter.join().unwrap().unwrap()
        else {
            panic!("expected changed Agent wait");
        };
        assert!(changed);
        assert_eq!(waited, blocked);
        assert_eq!(waited.observation.revision, 2);

        let Response::Agent { agent: done } = registry
            .dispatch(Request::ReportAgent {
                agent_id: agent.id.clone(),
                run_id,
                report: agent_spec(AgentState::Done).report,
            })
            .unwrap()
        else {
            panic!("expected completed Agent");
        };
        let start = Instant::now();
        let Response::AgentWait { changed, .. } = registry
            .wait_agent(&done.id, done.observation.revision, 2_000)
            .unwrap()
        else {
            panic!("expected terminal Agent wait");
        };
        assert!(!changed);
        assert!(start.elapsed() < Duration::from_secs(1));

        shell.kill().unwrap();
        registry.close_workspace(&workspace.id).unwrap();
    }

    #[test]
    fn agent_wait_wakes_with_daemon_stopping_before_registry_cleanup() {
        let registry = Arc::new(DaemonService::default());
        let (workspace, shell, _runtime) = running_shell(&registry);
        let run_id = shell.snapshot().unwrap().run.unwrap().id;
        let Response::Agent { agent } = registry
            .dispatch(Request::RegisterAgent {
                shell_id: shell.id.clone(),
                run_id,
                spec: agent_spec(AgentState::Working),
            })
            .unwrap()
        else {
            panic!("expected registered agent");
        };
        let waiting_registry = Arc::clone(&registry);
        let waiter = thread::spawn(move || waiting_registry.wait_agent(&agent.id, 1, 2_000));
        thread::sleep(Duration::from_millis(20));

        registry.runtimes.begin_stopping();
        registry.events.notify();

        assert_eq!(
            waiter.join().unwrap().unwrap_err().wire_code(),
            ErrorCode::DaemonStopping
        );
        registry.runtimes.cancel_stopping();
        shell.kill().unwrap();
        registry.close_workspace(&workspace.id).unwrap();
    }

    #[test]
    fn agent_instances_restore_from_daemon_persistence() {
        let directory = env::temp_dir().join(format!("boomux-agent-restore-{}", Uuid::new_v4()));
        let path = directory.join("state/state.json");
        let registry = DaemonService::restore(StateStore::at(path.clone()), false, None).unwrap();
        let (_workspace, shell, runtime) = running_shell(&registry);
        let shell_id = shell.id.clone();
        let run_id = shell.snapshot().unwrap().run.unwrap().id;
        let spec = agent_spec(AgentState::Working);
        let Response::Agent { agent: registered } = registry
            .dispatch(Request::EnsureAgent {
                shell_id: shell_id.clone(),
                run_id: run_id.clone(),
                spec: spec.clone(),
            })
            .unwrap()
        else {
            panic!("expected registered agent");
        };
        let Response::Agent { agent } = registry
            .dispatch(Request::ReportAgent {
                agent_id: registered.id,
                run_id: run_id.clone(),
                report: agent_spec(AgentState::Inactive).report,
            })
            .unwrap()
        else {
            panic!("expected inactive agent");
        };
        assert_eq!(agent.observation.state, AgentState::Inactive);
        assert_eq!(agent.ended_at_ms, None);

        shell.kill().unwrap();
        drop(runtime);
        drop(shell);
        drop(registry);

        let restored = DaemonService::restore(StateStore::at(path), false, None).unwrap();
        assert_eq!(
            restored.agent(&agent.id).unwrap().snapshot().unwrap(),
            agent
        );
        assert_eq!(
            restored.snapshot().unwrap().workspaces[0].agents,
            vec![agent.clone()]
        );
        let Response::Agent { agent: ensured } = restored
            .dispatch(Request::EnsureAgent {
                shell_id,
                run_id,
                spec,
            })
            .unwrap()
        else {
            panic!("expected restored ensured agent");
        };
        assert_eq!(ensured, agent);
        let Response::Agent { agent: reactivated } = restored
            .dispatch(Request::ReportAgent {
                agent_id: ensured.id,
                run_id: ensured.run_id,
                report: agent_spec(AgentState::Idle).report,
            })
            .unwrap()
        else {
            panic!("expected reactivated agent");
        };
        assert_eq!(reactivated.id, agent.id);
        assert_eq!(reactivated.observation.state, AgentState::Idle);
        assert_eq!(reactivated.ended_at_ms, None);
        assert_eq!(restored.snapshot().unwrap().workspaces[0].agents.len(), 1);
        drop(restored);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn protocol_eight_responses_hide_agent_snapshots_and_events() {
        let agent = AgentInstanceSnapshot {
            id: "a1".into(),
            workspace_id: "w1".into(),
            shell_id: "s1".into(),
            run_id: "r1".into(),
            name: "agent".into(),
            integration: "test".into(),
            external_session_id: None,
            cwd: Some("/tmp/project".into()),
            started_at_ms: 1,
            ended_at_ms: None,
            observation: AgentObservationSnapshot {
                revision: 1,
                state: AgentState::Working,
                authority: AgentAuthority::LifecycleIntegration,
                evidence: "working".into(),
                confidence: 90,
                observed_at_ms: 1,
            },
            attention: None,
        };
        let workspace = WorkspaceSnapshot {
            id: "w1".into(),
            revision: 1,
            name: "workspace".into(),
            default_cwd: None,
            shells: Vec::new(),
            launchers: Vec::new(),
            agents: vec![agent.clone()],
            schedules: Vec::new(),
        };
        let response = Response::Events {
            stream_id: "stream".into(),
            cursor: EventCursor {
                stream_id: "stream".into(),
                event_id: 2,
            },
            snapshot: Some(Snapshot {
                workspaces: vec![workspace],
                focused_terminal: None,
                scheduler: None,
            }),
            events: vec![
                DaemonEvent {
                    id: 1,
                    at_ms: 1,
                    kind: DaemonEventKind::AgentRegistered {
                        workspace_id: "w1".into(),
                        shell_id: "s1".into(),
                        agent,
                    },
                },
                DaemonEvent {
                    id: 2,
                    at_ms: 2,
                    kind: DaemonEventKind::WorkspaceRenamed {
                        workspace_id: "w1".into(),
                        name: "renamed".into(),
                    },
                },
            ],
        };

        let Response::Events {
            snapshot: Some(snapshot),
            events,
            cursor,
            ..
        } = response_for_version(response, 8)
        else {
            panic!("expected filtered events");
        };
        assert!(snapshot.workspaces[0].agents.is_empty());
        assert_eq!(events.len(), 1);
        assert_eq!(cursor.event_id, 2);
        assert!(matches!(
            events[0].kind,
            DaemonEventKind::WorkspaceRenamed { .. }
        ));
        let encoded =
            serde_json::to_value(response_for_version(Response::Snapshot { snapshot }, 8)).unwrap();
        assert!(encoded["snapshot"]["workspaces"][0].get("agents").is_none());
    }

    #[test]
    fn protocol_seventeen_responses_hide_focused_terminal() {
        let response = Response::Snapshot {
            snapshot: Snapshot {
                workspaces: Vec::new(),
                focused_terminal: Some(FocusedTerminalSnapshot {
                    revision: 1,
                    workspace_id: "w1".into(),
                    shell_id: "s1".into(),
                    run_id: "r1".into(),
                }),
                scheduler: None,
            },
        };

        let Response::Snapshot { snapshot } = response_for_version(response, 17) else {
            panic!("expected snapshot response");
        };
        assert!(snapshot.focused_terminal.is_none());
    }

    #[test]
    fn older_protocol_responses_downgrade_agent_fields() {
        let agent = AgentInstanceSnapshot {
            id: "a1".into(),
            workspace_id: "w1".into(),
            shell_id: "s1".into(),
            run_id: "r1".into(),
            name: "pi".into(),
            integration: "pi".into(),
            external_session_id: Some("session-1".into()),
            cwd: Some("/tmp/project".into()),
            started_at_ms: 1,
            ended_at_ms: None,
            observation: AgentObservationSnapshot {
                revision: 2,
                state: AgentState::Inactive,
                authority: AgentAuthority::LifecycleIntegration,
                evidence: "Pi session inactive".into(),
                confidence: 100,
                observed_at_ms: 2,
            },
            attention: None,
        };

        let Response::Agent { agent: downgraded } = response_for_version(
            Response::Agent {
                agent: agent.clone(),
            },
            11,
        ) else {
            panic!("expected agent response");
        };
        assert_eq!(downgraded.observation.state, AgentState::Unknown);
        assert!(downgraded.cwd.is_none());

        let Response::Agent { agent: current } = response_for_version(
            Response::Agent {
                agent: agent.clone(),
            },
            12,
        ) else {
            panic!("expected agent response");
        };
        assert_eq!(current.observation.state, AgentState::Inactive);
        assert!(current.cwd.is_none());

        let Response::Agent { agent: current } = response_for_version(
            Response::Agent {
                agent: agent.clone(),
            },
            13,
        ) else {
            panic!("expected agent response");
        };
        assert_eq!(current.cwd.as_deref(), Some(Path::new("/tmp/project")));

        let workspace = WorkspaceSnapshot {
            id: "w1".into(),
            revision: 1,
            name: "workspace".into(),
            default_cwd: None,
            shells: Vec::new(),
            launchers: Vec::new(),
            agents: vec![agent.clone()],
            schedules: Vec::new(),
        };
        let Response::Workspace {
            workspace: downgraded_workspace,
        } = response_for_version(
            Response::Workspace {
                workspace: workspace.clone(),
            },
            11,
        )
        else {
            panic!("expected workspace response");
        };
        assert_eq!(
            downgraded_workspace.agents[0].observation.state,
            AgentState::Unknown
        );

        let Response::Events {
            snapshot: Some(snapshot),
            events,
            ..
        } = response_for_version(
            Response::Events {
                stream_id: "stream".into(),
                cursor: EventCursor {
                    stream_id: "stream".into(),
                    event_id: 1,
                },
                snapshot: Some(Snapshot {
                    workspaces: vec![workspace],
                    focused_terminal: None,
                    scheduler: None,
                }),
                events: vec![DaemonEvent {
                    id: 1,
                    at_ms: 1,
                    kind: DaemonEventKind::AgentStateChanged {
                        workspace_id: "w1".into(),
                        shell_id: "s1".into(),
                        agent,
                    },
                }],
            },
            11,
        )
        else {
            panic!("expected events response");
        };
        assert_eq!(
            snapshot.workspaces[0].agents[0].observation.state,
            AgentState::Unknown
        );
        let DaemonEventKind::AgentStateChanged { agent, .. } = &events[0].kind else {
            panic!("expected agent state event");
        };
        assert_eq!(agent.observation.state, AgentState::Unknown);
    }

    #[test]
    fn protocol_fourteen_omits_attention_and_filters_acknowledgment_events() {
        let observation = AgentObservationSnapshot {
            revision: 2,
            state: AgentState::Blocked,
            authority: AgentAuthority::LifecycleIntegration,
            evidence: "blocked".into(),
            confidence: 100,
            observed_at_ms: 2,
        };
        let agent = AgentInstanceSnapshot {
            id: "a1".into(),
            workspace_id: "w1".into(),
            shell_id: "s1".into(),
            run_id: "r1".into(),
            name: "agent".into(),
            integration: "test".into(),
            external_session_id: None,
            cwd: None,
            started_at_ms: 1,
            ended_at_ms: None,
            observation: observation.clone(),
            attention: Some(AgentAttentionSnapshot {
                reason: AgentAttentionReason::Blocked,
                observation,
            }),
        };
        let cursor = EventCursor {
            stream_id: "stream".into(),
            event_id: 2,
        };
        let response = Response::Events {
            stream_id: "stream".into(),
            cursor: cursor.clone(),
            snapshot: Some(Snapshot {
                workspaces: vec![WorkspaceSnapshot {
                    id: "w1".into(),
                    revision: 1,
                    name: "workspace".into(),
                    default_cwd: None,
                    shells: Vec::new(),
                    launchers: Vec::new(),
                    agents: vec![agent.clone()],
                    schedules: Vec::new(),
                }],
                focused_terminal: None,
                scheduler: None,
            }),
            events: vec![DaemonEvent {
                id: 2,
                at_ms: 3,
                kind: DaemonEventKind::AgentAttentionAcknowledged {
                    workspace_id: "w1".into(),
                    shell_id: "s1".into(),
                    agent,
                },
            }],
        };

        let Response::Events {
            cursor: filtered_cursor,
            snapshot: Some(snapshot),
            events,
            ..
        } = response_for_version(response, 14)
        else {
            panic!("expected events response");
        };
        assert_eq!(filtered_cursor, cursor);
        assert!(events.is_empty());
        assert!(snapshot.workspaces[0].agents[0].attention.is_none());
    }

    #[test]
    fn protocol_seven_event_pages_hide_launcher_events() {
        let cursor = EventCursor {
            stream_id: "stream".into(),
            event_id: 2,
        };
        let response = Response::Events {
            stream_id: "stream".into(),
            cursor: cursor.clone(),
            snapshot: None,
            events: vec![
                DaemonEvent {
                    id: 1,
                    at_ms: 1,
                    kind: DaemonEventKind::LauncherCreated {
                        workspace_id: "workspace".into(),
                        launcher_id: "launcher".into(),
                        name: "editor".into(),
                    },
                },
                DaemonEvent {
                    id: 2,
                    at_ms: 2,
                    kind: DaemonEventKind::WorkspaceRenamed {
                        workspace_id: "workspace".into(),
                        name: "renamed".into(),
                    },
                },
            ],
        };
        let Response::Events {
            cursor: filtered_cursor,
            events,
            ..
        } = response_for_version(response, 7)
        else {
            panic!("expected events");
        };
        assert_eq!(filtered_cursor, cursor);
        assert_eq!(events.len(), 1);
        assert!(matches!(
            events[0].kind,
            DaemonEventKind::WorkspaceRenamed { .. }
        ));
    }

    #[test]
    fn pty_reader_pause_forms_an_acknowledged_output_barrier() {
        let shell = create_pending_shell(
            "workspace-id",
            ShellSpec {
                name: "pause-test".into(),
                command: vec![
                    "/bin/sh".into(),
                    "-c".into(),
                    "stty -echo; while IFS= read -r line; do printf 'observed:%s\\n' \"$line\"; done"
                        .into(),
                ],
                cwd: env::temp_dir(),
            },
        )
        .unwrap();
        let terminal_profile = profile();
        let run = Arc::new(ShellRun::new(1));
        let (runtime, reader) = spawn_runtime(
            &shell,
            &run,
            "workspace",
            "pause-test",
            &terminal_profile,
            None,
            RuntimeRecovery::default(),
        )
        .unwrap();
        *lock(&shell.last_run).unwrap() = Some(run.persisted(terminal_profile.clone()).unwrap());
        *lock(&shell.lifecycle).unwrap() = ShellLifecycle::Running {
            profile: terminal_profile,
            run: Arc::clone(&run),
            runtime: Arc::clone(&runtime),
        };
        let registry = Arc::new(DaemonService::default());
        start_pty_reader(
            Arc::downgrade(&registry),
            Arc::clone(&shell),
            run,
            Arc::clone(&runtime),
            reader,
            false,
        )
        .unwrap();

        runtime.pause_reader().unwrap();
        lock(&runtime.master).unwrap().write(b"paused\n").unwrap();
        thread::sleep(Duration::from_millis(50));
        assert!(
            !lock(&runtime.terminal)
                .unwrap()
                .plain_text()
                .contains("observed:paused")
        );

        runtime.resume_reader().unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline
            && !lock(&runtime.terminal)
                .unwrap()
                .plain_text()
                .contains("observed:paused")
        {
            thread::sleep(IO_RETRY_DELAY);
        }
        assert!(
            lock(&runtime.terminal)
                .unwrap()
                .plain_text()
                .contains("observed:paused")
        );
        shell.kill().unwrap();
    }

    #[test]
    fn close_does_not_deadlock_with_a_naturally_exiting_reader() {
        let registry = Arc::new(DaemonService::default());
        let workspace = registry
            .create_workspace(
                "exit-race".into(),
                vec![ShellSpec {
                    name: "short-lived".into(),
                    command: vec!["/bin/sh".into(), "-c".into(), "sleep 0.01".into()],
                    cwd: env::temp_dir(),
                }],
            )
            .unwrap();
        let shell = registry.shell(&workspace.shells[0].id).unwrap();
        let terminal_profile = profile();
        let run = Arc::new(ShellRun::new(1));
        let (runtime, reader) = spawn_runtime(
            &shell,
            &run,
            "exit-race",
            "short-lived",
            &terminal_profile,
            None,
            RuntimeRecovery::default(),
        )
        .unwrap();
        *lock(&shell.last_run).unwrap() = Some(run.persisted(terminal_profile.clone()).unwrap());
        *lock(&shell.lifecycle).unwrap() = ShellLifecycle::Running {
            profile: terminal_profile,
            run: Arc::clone(&run),
            runtime: Arc::clone(&runtime),
        };
        start_pty_reader(
            Arc::downgrade(&registry),
            Arc::clone(&shell),
            run,
            runtime,
            reader,
            false,
        )
        .unwrap();
        let shell_id = shell.id.clone();
        let (completed, completion) = mpsc::sync_channel(1);
        thread::spawn(move || {
            let _ = completed.send(registry.close_shell(&shell_id));
        });

        completion
            .recv_timeout(Duration::from_secs(3))
            .expect("shell close deadlocked with the PTY reader")
            .unwrap();
    }

    #[test]
    fn timed_out_reader_pause_cancels_queued_command() {
        let (commands, receiver) = mpsc::channel();
        let (observed, observation) = mpsc::sync_channel(1);
        let handle = thread::spawn(move || {
            thread::sleep(Duration::from_millis(30));
            if let ReaderCommand::Pause { cancelled, .. } = receiver.recv().unwrap() {
                observed.send(cancelled.load(Ordering::Acquire)).unwrap();
            }
            Ok(())
        });
        let task = ReaderTask {
            commands,
            handle: Mutex::new(Some(handle)),
        };

        let error = task
            .pause_with_timeout(Duration::from_millis(5))
            .unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert!(observation.recv().unwrap());
        task.stop().unwrap();
    }

    #[test]
    fn reports_only_term_mismatches() {
        assert_eq!(term_mismatch_warning(Some("xterm"), Some("xterm")), None);
        assert!(term_mismatch_warning(Some("xterm"), Some("alacritty")).is_some());
        assert!(term_mismatch_warning(None, Some("xterm")).is_some());
    }

    #[test]
    fn run_completion_never_precedes_its_start_timestamp() {
        let run = ShellRun {
            id: Uuid::new_v4().to_string(),
            generation: 1,
            started_at_ms: u64::MAX,
            ended: Mutex::new(None),
            output_revision: AtomicU64::new(0),
            environment_has_run_id: true,
        };

        run.finish(ShellRunExitReason::Interrupted).unwrap();

        assert_eq!(run.snapshot().unwrap().ended_at_ms, Some(u64::MAX));
    }

    #[test]
    fn pathless_workspace_snapshot_has_no_default_cwd_and_no_shells() {
        let registry = DaemonService::default();

        let workspace = registry
            .create_workspace("empty".into(), Vec::new())
            .unwrap();
        let value = serde_json::to_value(&workspace).unwrap();

        assert!(workspace.shells.is_empty());
        assert!(workspace.default_cwd.is_none());
        assert!(value.get("default_cwd").is_none());
    }

    #[test]
    fn guarded_resource_mutations_reject_stale_revisions_without_changing_legacy_semantics() {
        let registry = DaemonService::default();
        let workspace = registry
            .create_workspace("guarded".into(), Vec::new())
            .unwrap();
        let stale = registry
            .dispatch(Request::GuardedRenameWorkspace {
                workspace_id: workspace.id.clone(),
                name: "stale".into(),
                expected_revision: workspace.revision + 1,
            })
            .unwrap_err();
        assert_eq!(stale.wire_code(), ErrorCode::RevisionAhead);
        assert_eq!(
            registry
                .workspace(&workspace.id)
                .unwrap()
                .snapshot(&registry.durable)
                .unwrap()
                .name,
            "guarded"
        );

        registry
            .dispatch(Request::RenameWorkspace {
                workspace_id: workspace.id.clone(),
                name: "legacy".into(),
            })
            .unwrap();
        let current = registry
            .workspace(&workspace.id)
            .unwrap()
            .snapshot(&registry.durable)
            .unwrap();
        assert_eq!(current.name, "legacy");
        assert_eq!(current.revision, workspace.revision + 1);
    }

    #[test]
    fn workspace_snapshot_retains_default_cwd() {
        let registry = DaemonService::default();
        let cwd = env::temp_dir();

        let workspace = registry
            .create_workspace_with_default_cwd("project".into(), Some(cwd.clone()), Vec::new())
            .unwrap();

        assert_eq!(workspace.default_cwd.as_deref(), Some(cwd.as_path()));
    }

    #[test]
    fn protocol_eighteen_responses_hide_workspace_default_cwd() {
        let source_workspace = WorkspaceSnapshot {
            id: "w1".into(),
            revision: 1,
            name: "project".into(),
            default_cwd: Some("/tmp/project".into()),
            shells: Vec::new(),
            launchers: Vec::new(),
            agents: Vec::new(),
            schedules: Vec::new(),
        };

        let Response::Workspace { workspace } = response_for_version(
            Response::Workspace {
                workspace: source_workspace.clone(),
            },
            18,
        ) else {
            panic!("expected workspace response");
        };

        assert!(workspace.default_cwd.is_none());

        let Response::Snapshot { snapshot } = response_for_version(
            Response::Snapshot {
                snapshot: Snapshot {
                    workspaces: vec![source_workspace.clone()],
                    focused_terminal: None,
                    scheduler: None,
                },
            },
            18,
        ) else {
            panic!("expected snapshot response");
        };
        assert!(snapshot.workspaces[0].default_cwd.is_none());

        let Response::Events {
            snapshot: Some(snapshot),
            ..
        } = response_for_version(
            Response::Events {
                stream_id: "stream".into(),
                cursor: EventCursor {
                    stream_id: "stream".into(),
                    event_id: 0,
                },
                snapshot: Some(Snapshot {
                    workspaces: vec![source_workspace],
                    focused_terminal: None,
                    scheduler: None,
                }),
                events: Vec::new(),
            },
            18,
        )
        else {
            panic!("expected events response");
        };
        assert!(snapshot.workspaces[0].default_cwd.is_none());
    }

    #[test]
    fn concurrent_duplicate_workspace_names_publish_only_once() {
        let registry = Arc::new(DaemonService::default());
        let barrier = Arc::new(Barrier::new(3));
        let threads = (0..2)
            .map(|_| {
                let registry = Arc::clone(&registry);
                let barrier = Arc::clone(&barrier);
                thread::spawn(move || {
                    barrier.wait();
                    registry.create_workspace("same".into(), Vec::new())
                })
            })
            .collect::<Vec<_>>();
        barrier.wait();
        let results = threads
            .into_iter()
            .map(|thread| thread.join().unwrap())
            .collect::<Vec<_>>();

        assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
        assert!(results.iter().any(|result| {
            result
                .as_ref()
                .is_err_and(|error| error.kind() == io::ErrorKind::AlreadyExists)
        }));
        assert_eq!(registry.snapshot().unwrap().workspaces.len(), 1);
    }

    #[test]
    fn shell_without_workspace_gets_the_next_generated_workspace_name() {
        let registry = DaemonService::default();
        registry
            .create_workspace("workspace-1".into(), Vec::new())
            .unwrap();

        let shell = registry
            .create_shell_with_workspace(ShellSpec {
                name: "shell-1".into(),
                command: vec!["/bin/sh".into(), "-c".into(), "sleep 5".into()],
                cwd: env::temp_dir(),
            })
            .unwrap();
        let workspace = registry.workspace(&shell.workspace_id).unwrap();

        assert_eq!(&*lock(&workspace.name).unwrap(), "workspace-2");
        assert_eq!(
            workspace.default_cwd.as_deref(),
            Some(env::temp_dir().as_path())
        );
        registry.shutdown().unwrap();
    }

    #[test]
    fn daemon_lock_allows_only_one_owner() {
        let directory = env::temp_dir().join(format!("boomux-lock-test-{}", Uuid::new_v4()));
        fs::create_dir(&directory).unwrap();
        let first = acquire_daemon_lock(&directory).unwrap();

        let error = acquire_daemon_lock(&directory).unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::AddrInUse);
        drop(first);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn registry_closes_shell_without_removing_workspace() {
        let registry = DaemonService::default();
        let workspace = registry
            .create_workspace(
                "test".into(),
                vec![ShellSpec {
                    name: "one".into(),
                    command: vec!["/bin/sh".into(), "-c".into(), "sleep 5".into()],
                    cwd: env::temp_dir(),
                }],
            )
            .unwrap();
        assert_eq!(workspace.shells.len(), 1);
        assert_eq!(registry.snapshot().unwrap().workspaces.len(), 1);

        registry.close_shell(&workspace.shells[0].id).unwrap();
        let snapshot = registry.snapshot().unwrap();
        assert_eq!(snapshot.workspaces.len(), 1);
        assert!(snapshot.workspaces[0].shells.is_empty());

        registry.close_workspace(&workspace.id).unwrap();
        assert!(registry.snapshot().unwrap().workspaces.is_empty());
    }

    #[test]
    fn persisted_attention_rejects_impossible_observation_history() {
        let persisted = |attention: AgentAttentionSnapshot| PersistedAgentInstance {
            id: Uuid::new_v4().to_string(),
            shell_id: Uuid::new_v4().to_string(),
            run_id: Uuid::new_v4().to_string(),
            name: "agent".into(),
            integration: "test".into(),
            external_session_id: Some("session".into()),
            cwd: Some(env::temp_dir()),
            started_at_ms: 10,
            ended_at_ms: None,
            observation: AgentObservationSnapshot {
                revision: 2,
                state: AgentState::Working,
                authority: AgentAuthority::LifecycleIntegration,
                evidence: "working".into(),
                confidence: 100,
                observed_at_ms: 20,
            },
            attention: Some(attention),
        };
        let completed = AgentAttentionSnapshot {
            reason: AgentAttentionReason::Completed,
            observation: AgentObservationSnapshot {
                revision: 1,
                state: AgentState::Done,
                authority: AgentAuthority::LifecycleIntegration,
                evidence: "done".into(),
                confidence: 100,
                observed_at_ms: 15,
            },
        };
        assert_eq!(
            validate_persisted_agent(&persisted(completed))
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidData
        );

        let mismatched_current = AgentAttentionSnapshot {
            reason: AgentAttentionReason::Blocked,
            observation: AgentObservationSnapshot {
                revision: 2,
                state: AgentState::Blocked,
                authority: AgentAuthority::LifecycleIntegration,
                evidence: "different revision contents".into(),
                confidence: 100,
                observed_at_ms: 20,
            },
        };
        assert_eq!(
            validate_persisted_agent(&persisted(mismatched_current))
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidData
        );
    }
}

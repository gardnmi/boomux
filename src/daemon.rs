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
use uuid::Uuid;

use crate::client;
use crate::desktop_notifications::{
    DesktopNotificationSink, DisabledNotificationSink, NotificationReason, NotificationRequest,
    NotificationSink, category_enabled, test_delivery,
};
use crate::fd_transfer::send_descriptor;
use crate::handoff;
use crate::protocol::{
    self, AgentAttentionReason, AgentAttentionSnapshot, AgentAuthority, AgentInstanceSnapshot,
    AgentObservationSnapshot, AgentRegistrationSpec, AgentReport, AgentState, AttachFrame,
    DaemonEvent, DaemonEventKind, Envelope, ErrorCode, EventCursor, NotificationDeliveryConfig,
    Request, Response, ShellRunExitReason, ShellRunSnapshot, ShellSnapshot, ShellSpec, ShellStatus,
    Snapshot, TerminalProfile, UnixEnvironment, WorkspaceLauncherSnapshot, WorkspaceLauncherSpec,
    WorkspaceSnapshot,
};
use crate::state_store::{
    PersistedAgentInstance, PersistedShell, PersistedShellRun, PersistedState, PersistedWorkspace,
    PersistedWorkspaceLauncher, StateStore,
};
use crate::terminal_state::TerminalState;

const CONTROLLER_QUEUE: usize = 64;
const SHUTDOWN_GRACE: Duration = Duration::from_millis(50);
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(2);
const RESTART_TIMEOUT: Duration = Duration::from_secs(10);
const IO_RETRY_DELAY: Duration = Duration::from_millis(2);
const PERSIST_RETRY_INTERVAL: Duration = Duration::from_secs(1);
const MAX_TERMINAL_ENV_VALUE: usize = 256;
const MAX_NAME_BYTES: usize = 256;
const MAX_AGENT_EVIDENCE_BYTES: usize = 4 * 1024;
const MAX_TERMINAL_ROWS: u16 = 1_000;
const MAX_TERMINAL_COLS: u16 = 1_000;
const MAX_TERMINAL_CELLS: usize = 1_000_000;
const MAX_SHELL_READ_BYTES: usize = 1024 * 1024;
const MAX_FOREGROUND_PROCESS_BYTES: usize = 64;
const MAX_RETAINED_EVENTS: usize = 8_192;
const MAX_EVENT_BATCH: u16 = 256;
const MAX_EVENT_WAIT: Duration = Duration::from_secs(30);
const TRANSITION_IDLE: u8 = 0;
const TRANSITION_RESTART: u8 = 1;
const TRANSITION_SHUTDOWN: u8 = 2;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NotificationSettings {
    pub enabled: bool,
    pub blocked: bool,
    pub completed: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NotificationSoundSettings {
    pub enabled: bool,
    pub blocked: String,
    pub completed: String,
}

impl Default for NotificationSoundSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            blocked: "message-new-instant".into(),
            completed: "complete".into(),
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
            },
            sound: NotificationSoundSettings {
                enabled: config.sound_enabled,
                blocked: config.blocked_sound,
                completed: config.completed_sound,
            },
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
            blocked_sound: settings.sound.blocked,
            completed_sound: settings.sound.completed,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct NotificationDeliverySettings {
    pub desktop: NotificationSettings,
    pub sound: NotificationSoundSettings,
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
                    events: Some(event_stream),
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
    let mut registry = Registry::restore(store, committed.is_some(), transferred.events)?;
    registry.notification_settings = notification_settings.clone();
    registry.notification_sink = Arc::new(DesktopNotificationSink::new(notification_settings));
    let registry = Arc::new(registry);
    let gated_readers = registry.import_handoff(transferred.runtimes, transferred.exited)?;
    if let Some(channel) = committed {
        {
            let _transition = lock(&registry.transitions)?;
            let events = lock(&registry.events.state)?;
            EventLog::ensure_capacity(&events, 1)?;
        }
        channel.write_all(&[handoff::PREPARED])?;
        let mut decision = [0];
        channel.read_exact(&mut decision)?;
        match decision[0] {
            handoff::ABORT => return Ok(()),
            handoff::FINALIZE => {
                socket_cleanup.arm();
                registry.publish_runtime_batch(vec![DaemonEventKind::HandoffCompleted])?;
                for runtime in gated_readers {
                    runtime.resume_reader()?;
                }
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
    }
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
        if registry.persistence_dirty.load(Ordering::Acquire)
            && last_persistence_retry.elapsed() >= PERSIST_RETRY_INTERVAL
        {
            let _ = registry.flush_pending();
            last_persistence_retry = Instant::now();
        }
        match restart_receiver.try_recv() {
            Ok(request) => {
                let result = launch_replacement(
                    &listener,
                    &daemon_lock,
                    &registry,
                    request.notification_settings,
                );
                if result.is_ok() {
                    handed_off = true;
                    shutdown.store(true, Ordering::Release);
                } else {
                    transition.store(TRANSITION_IDLE, Ordering::Release);
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
                let registry = Arc::clone(&registry);
                let shutdown = Arc::clone(&shutdown);
                let transition = Arc::clone(&transition);
                let restart_sender = restart_sender.clone();
                handlers.push(thread::spawn(move || {
                    let _ =
                        handle_connection(stream, registry, shutdown, transition, restart_sender);
                }));
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
    let result = if handed_off {
        socket_cleanup.disarm();
        Ok(())
    } else {
        registry.shutdown()
    };
    drop(registry);
    drop(listener);
    drop(socket_cleanup);
    drop(daemon_lock);
    result
}

struct RestartRequest {
    reply: SyncSender<io::Result<()>>,
    notification_settings: Option<NotificationDeliverySettings>,
}

#[derive(Debug)]
struct PersistenceError(io::Error);

#[derive(Debug)]
struct DaemonCodeError {
    code: ErrorCode,
    message: String,
}

impl std::fmt::Display for DaemonCodeError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for DaemonCodeError {}

impl std::fmt::Display for PersistenceError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(formatter)
    }
}

impl std::error::Error for PersistenceError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.0)
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
    registry: &Registry,
    notification_settings: Option<NotificationDeliverySettings>,
) -> io::Result<()> {
    let _mutation = lock(&registry.mutation_lock)?;
    registry.ensure_running()?;
    registry.stopping.store(true, Ordering::Release);
    registry.events.notify();
    let mut paused = Vec::new();
    let result = (|| {
        registry.quiesce_controllers()?;
        let shells = lock(&registry.state)?
            .shells
            .values()
            .cloned()
            .collect::<Vec<_>>();
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
            runtime.pause_reader()?;
            paused.push(Arc::clone(&runtime));
            let (pid, pidfd) = lock(&runtime.process)?.transfer_identity()?;
            let reconstruction = lock(&runtime.terminal)?.reconstruction();
            transfers.push(OutgoingRuntime {
                manifest: handoff::RuntimeManifest {
                    shell_id: shell.id.clone(),
                    run_id: Some(run.id.clone()),
                    output_revision: Some(run.output_revision.load(Ordering::Acquire)),
                    profile,
                    pid,
                },
                runtime,
                pidfd,
                reconstruction,
            });
        }
        let mut transition = lock(&registry.transitions)?;
        let _persistence = lock(&registry.persist_lock)?;
        let mut events = lock(&registry.events.state)?;
        let published = registry.flush_pending_locked(&mut transition, &mut events)?;
        let event_stream = handoff::EventStreamManifest {
            stream_id: events.stream_id.clone(),
            latest_id: events.latest_id,
            events: events.events.iter().cloned().collect(),
        };
        drop(events);
        drop(transition);
        if published {
            registry.events.changed.notify_all();
        }
        let state_lock = registry.state_lock_descriptor()?;
        launch_replacement_process(
            listener.as_fd(),
            daemon_lock.as_fd(),
            state_lock,
            &transfers,
            &exited,
            &event_stream,
            notification_settings,
        )
    })();
    if result.is_err() {
        registry.stopping.store(false, Ordering::Release);
        for runtime in paused {
            let _ = runtime.resume_reader();
        }
    }
    result
}

fn launch_replacement_process(
    listener: BorrowedFd<'_>,
    runtime_lock: BorrowedFd<'_>,
    state_lock: BorrowedFd<'_>,
    runtimes: &[OutgoingRuntime],
    exited: &[OutgoingExited],
    event_stream: &handoff::EventStreamManifest,
    notification_settings: Option<NotificationDeliverySettings>,
) -> io::Result<()> {
    let (mut channel, child_channel) = UnixStream::pair()?;
    let child_channel_fd = child_channel.as_raw_fd();
    let mut command = Command::new(replacement_executable()?);
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
            },
        )?;
        send_descriptor(&channel, listener, handoff::LISTENER_MARKER)?;
        send_descriptor(&channel, runtime_lock, handoff::RUNTIME_LOCK_MARKER)?;
        send_descriptor(&channel, state_lock, handoff::STATE_LOCK_MARKER)?;
        for runtime in runtimes {
            let master = lock(&runtime.runtime.master)?;
            send_descriptor(&channel, master.descriptor.as_fd(), handoff::PTY_MARKER)?;
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
    mut stream: UnixStream,
    registry: Arc<Registry>,
    shutdown: Arc<AtomicBool>,
    transition: Arc<AtomicU8>,
    restart_sender: mpsc::Sender<RestartRequest>,
) -> io::Result<()> {
    stream.set_read_timeout(Some(HANDSHAKE_TIMEOUT))?;
    let request: Envelope<Request> = protocol::read_message(&mut stream)?;
    stream.set_read_timeout(None)?;
    if !(protocol::MIN_PROTOCOL_VERSION..=protocol::PROTOCOL_VERSION).contains(&request.version) {
        return send_response(
            &mut stream,
            protocol::PROTOCOL_VERSION,
            error_response(
                ErrorCode::UnsupportedVersion,
                format!(
                    "protocol version {} is unsupported; expected {}",
                    request.version,
                    protocol::PROTOCOL_VERSION
                ),
            ),
        );
    }
    let response_version = request.version;
    let minimum_version = request.message.minimum_protocol_version();
    if response_version < minimum_version {
        return send_response(
            &mut stream,
            response_version,
            error_response(
                ErrorCode::UnsupportedVersion,
                unsupported_request_message(&request.message, minimum_version),
            ),
        );
    }

    if let Request::Attach {
        shell_id,
        takeover,
        restart_exited,
        profile,
        environment,
    } = request.message
    {
        return handle_attach(
            stream,
            response_version,
            &registry,
            &shell_id,
            AttachRequestOptions {
                takeover,
                restart_exited,
                profile,
                environment,
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
                error_response(
                    ErrorCode::Busy,
                    "another daemon transition is already in progress",
                ),
            );
        }
        return match registry.shutdown() {
            Ok(()) => {
                shutdown.store(true, Ordering::Release);
                send_response(&mut stream, response_version, Response::Ok)
            }
            Err(error) => {
                transition.store(TRANSITION_IDLE, Ordering::Release);
                send_response(
                    &mut stream,
                    response_version,
                    error_response(
                        error_code(&error),
                        format!("could not stop Boomux daemon: {error}"),
                    ),
                )
            }
        };
    }
    let restart_notification_settings = match &request.message {
        Request::Restart => Some(None),
        Request::RestartWithNotificationConfig { notifications } => {
            Some(Some(notifications.clone().into()))
        }
        _ => None,
    };
    if let Some(notification_settings) = restart_notification_settings {
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
                error_response(ErrorCode::Busy, "daemon restart is already in progress"),
            );
        }
        let (reply, response) = mpsc::sync_channel(1);
        if restart_sender
            .send(RestartRequest {
                reply,
                notification_settings,
            })
            .is_err()
        {
            transition.store(TRANSITION_IDLE, Ordering::Release);
            return Err(io::Error::other("daemon restart coordinator stopped"));
        }
        return match response.recv_timeout(RESTART_TIMEOUT) {
            Ok(Ok(())) => send_response(&mut stream, response_version, Response::Ok),
            Ok(Err(error)) => send_response(
                &mut stream,
                response_version,
                error_response(error_code(&error), error.to_string()),
            ),
            Err(error) => send_response(
                &mut stream,
                response_version,
                error_response(
                    ErrorCode::Timeout,
                    format!("daemon restart timed out: {error}"),
                ),
            ),
        };
    }

    let response = match registry.dispatch(request.message) {
        Ok(response) => response,
        Err(error) => error_response(error_code(&error), error.to_string()),
    };
    send_response(
        &mut stream,
        response_version,
        response_for_version(response, response_version),
    )
}

fn unsupported_request_message(request: &Request, minimum_version: u32) -> String {
    match request {
        Request::RegisterAgent { spec, .. } | Request::EnsureAgent { spec, .. }
            if spec.report.state == AgentState::Inactive =>
        {
            "inactive agent state requires daemon protocol 12".into()
        }
        Request::ReportAgent { report, .. } if report.state == AgentState::Inactive => {
            "inactive agent state requires daemon protocol 12".into()
        }
        Request::WaitAgent { .. } => "agent wait requires daemon protocol 14".into(),
        Request::AcknowledgeAgentAttention { .. } => {
            "agent attention acknowledgment requires daemon protocol 15".into()
        }
        Request::Attach {
            environment: Some(_),
            ..
        } => "client environment requires daemon protocol 16".into(),
        _ => format!("request requires daemon protocol {minimum_version}"),
    }
}

fn response_for_version(response: Response, version: u32) -> Response {
    let mut response = response;
    if version < 15 {
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
    if version < 13 {
        remove_agent_cwds(&mut response);
    }
    if version < 12 {
        downgrade_inactive_agent_states(&mut response);
    }
    if version >= 9 {
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
            if version < 8 {
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
    protocol::write_message(stream, &Envelope::with_version(version, response))
}

fn error_response(code: ErrorCode, message: impl Into<String>) -> Response {
    Response::Error {
        message: message.into(),
        code: Some(code),
    }
}

fn error_code(error: &io::Error) -> ErrorCode {
    if let Some(error) = error
        .get_ref()
        .and_then(|error| error.downcast_ref::<DaemonCodeError>())
    {
        return error.code;
    }
    if error
        .get_ref()
        .is_some_and(|error| error.downcast_ref::<PersistenceError>().is_some())
    {
        return ErrorCode::PersistenceFailed;
    }
    match error.kind() {
        io::ErrorKind::InvalidInput | io::ErrorKind::InvalidData => ErrorCode::InvalidArgument,
        io::ErrorKind::NotFound => ErrorCode::NotFound,
        io::ErrorKind::AlreadyExists => ErrorCode::AlreadyExists,
        io::ErrorKind::WouldBlock | io::ErrorKind::AddrInUse => ErrorCode::Busy,
        io::ErrorKind::ConnectionAborted => ErrorCode::DaemonStopping,
        io::ErrorKind::TimedOut => ErrorCode::Timeout,
        _ => ErrorCode::Internal,
    }
}

fn persistence_error(error: io::Error) -> io::Error {
    io::Error::new(error.kind(), PersistenceError(error))
}

fn coded_error(code: ErrorCode, message: impl Into<String>) -> io::Error {
    io::Error::other(DaemonCodeError {
        code,
        message: message.into(),
    })
}

struct Registry {
    state: Mutex<RegistryState>,
    store: Option<StateStore>,
    events: EventLog,
    transitions: Mutex<TransitionState>,
    mutation_lock: Mutex<()>,
    persist_lock: Mutex<()>,
    stopping: AtomicBool,
    persistence_dirty: AtomicBool,
    notification_settings: NotificationDeliverySettings,
    notification_sink: Arc<dyn NotificationSink>,
}

enum DurableMutation<T> {
    Changed(T, Vec<DaemonEventKind>),
    Unchanged(T),
}

#[derive(Default)]
struct TransitionState {
    pending_durable_events: VecDeque<Vec<DaemonEventKind>>,
}

struct EventLog {
    state: Mutex<EventLogState>,
    changed: Condvar,
}

struct EventLogState {
    stream_id: String,
    latest_id: u64,
    events: VecDeque<DaemonEvent>,
}

impl EventLog {
    fn new() -> Self {
        Self {
            state: Mutex::new(EventLogState {
                stream_id: Uuid::new_v4().to_string(),
                latest_id: 0,
                events: VecDeque::new(),
            }),
            changed: Condvar::new(),
        }
    }

    fn from_transfer(transfer: Option<handoff::EventStreamManifest>) -> Self {
        let state = transfer.map_or_else(
            || EventLogState {
                stream_id: Uuid::new_v4().to_string(),
                latest_id: 0,
                events: VecDeque::new(),
            },
            |transfer| EventLogState {
                stream_id: transfer.stream_id,
                latest_id: transfer.latest_id,
                events: transfer.events.into(),
            },
        );
        Self {
            state: Mutex::new(state),
            changed: Condvar::new(),
        }
    }

    fn ensure_capacity(state: &EventLogState, count: usize) -> io::Result<()> {
        let count = u64::try_from(count).map_err(|_| io::Error::other("event batch too large"))?;
        state
            .latest_id
            .checked_add(count)
            .map(|_| ())
            .ok_or_else(|| io::Error::other("daemon event ID exhausted"))
    }

    fn append_batch_locked(
        state: &mut EventLogState,
        kinds: Vec<DaemonEventKind>,
    ) -> Vec<DaemonEvent> {
        debug_assert!(Self::ensure_capacity(state, kinds.len()).is_ok());
        let mut appended = Vec::with_capacity(kinds.len());
        for kind in kinds {
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
        appended
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
}

#[derive(Default)]
struct RegistryState {
    workspaces: HashMap<String, Arc<Workspace>>,
    shells: HashMap<String, Arc<Shell>>,
    launchers: HashMap<String, Arc<WorkspaceLauncher>>,
    agents: HashMap<String, Arc<AgentInstance>>,
}

struct RegistryBackup {
    workspaces: HashMap<String, Arc<Workspace>>,
    shells: HashMap<String, Arc<Shell>>,
    launchers: HashMap<String, Arc<WorkspaceLauncher>>,
    agents: HashMap<String, Arc<AgentInstance>>,
    workspace_names: HashMap<String, String>,
    workspace_shell_ids: HashMap<String, Vec<String>>,
    workspace_launcher_ids: HashMap<String, Vec<String>>,
    workspace_agent_ids: HashMap<String, Vec<String>>,
    shell_names: HashMap<String, String>,
    launcher_names: HashMap<String, String>,
    agent_states: HashMap<String, AgentInstanceState>,
}

impl Default for Registry {
    fn default() -> Self {
        Self {
            state: Mutex::new(RegistryState::default()),
            store: None,
            events: EventLog::new(),
            transitions: Mutex::new(TransitionState::default()),
            mutation_lock: Mutex::new(()),
            persist_lock: Mutex::new(()),
            stopping: AtomicBool::new(false),
            persistence_dirty: AtomicBool::new(false),
            notification_settings: NotificationDeliverySettings::default(),
            notification_sink: Arc::new(DisabledNotificationSink),
        }
    }
}

struct Workspace {
    id: String,
    name: Mutex<String>,
    shell_ids: Mutex<Vec<String>>,
    launcher_ids: Mutex<Vec<String>>,
    agent_ids: Mutex<Vec<String>>,
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
    workspace_id: String,
    name: Mutex<String>,
    cwd: PathBuf,
    command: Vec<String>,
}

struct Shell {
    id: String,
    workspace_id: String,
    name: Mutex<String>,
    cwd: PathBuf,
    command: Vec<String>,
    last_run: Mutex<Option<PersistedShellRun>>,
    lifecycle: Mutex<ShellLifecycle>,
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
    runtime: Arc<ShellRuntime>,
    pidfd: OwnedFd,
    reconstruction: Vec<u8>,
}

struct OutgoingExited {
    manifest: handoff::ExitedManifest,
    reconstruction: Vec<u8>,
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
    handle: thread::JoinHandle<()>,
}

enum ReaderCommand {
    Pause {
        acknowledge: SyncSender<()>,
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
    fn pause(&self) -> io::Result<()> {
        self.pause_with_timeout(HANDSHAKE_TIMEOUT)
    }

    fn pause_with_timeout(&self, timeout: Duration) -> io::Result<()> {
        if self.handle.is_finished() {
            return Ok(());
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
            return self.handle.is_finished().then_some(()).ok_or_else(|| {
                io::Error::new(io::ErrorKind::BrokenPipe, "PTY reader control closed")
            });
        }
        let deadline = Instant::now() + timeout;
        loop {
            match acknowledged.try_recv() {
                Ok(()) => return Ok(()),
                Err(mpsc::TryRecvError::Disconnected) if self.handle.is_finished() => return Ok(()),
                Err(mpsc::TryRecvError::Disconnected) => {
                    return Err(io::Error::new(
                        io::ErrorKind::BrokenPipe,
                        "PTY reader stopped before acknowledging pause",
                    ));
                }
                Err(mpsc::TryRecvError::Empty) if self.handle.is_finished() => return Ok(()),
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
        if self.handle.is_finished() || self.commands.send(ReaderCommand::Resume).is_ok() {
            Ok(())
        } else {
            Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "PTY reader control closed",
            ))
        }
    }

    fn stop(self) -> io::Result<()> {
        if !self.handle.is_finished() {
            let _ = self.commands.send(ReaderCommand::Stop);
        }
        self.handle
            .join()
            .map_err(|_| io::Error::other("PTY reader thread panicked"))
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

impl Registry {
    fn dispatch(&self, request: Request) -> io::Result<Response> {
        match request {
            Request::Ping => Ok(Response::Pong),
            Request::Restart | Request::RestartWithNotificationConfig { .. } => {
                unreachable!("restart is handled before dispatch")
            }
            Request::Shutdown => unreachable!("shutdown is handled before dispatch"),
            Request::Snapshot => Ok(Response::Snapshot {
                snapshot: self.snapshot()?,
            }),
            Request::GetWorkspace { workspace_id } => Ok(Response::Workspace {
                workspace: self.workspace(&workspace_id)?.snapshot(self)?,
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
            Request::WaitAgent {
                agent_id,
                after_revision,
                wait_ms,
            } => self.wait_agent(&agent_id, after_revision, wait_ms),
            Request::AcknowledgeAgentAttention {
                agent_id,
                observation_revision,
            } => self.durable_mutation_outcome(|| {
                let (agent, changed) =
                    self.acknowledge_agent_attention(&agent_id, observation_revision)?;
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
            Request::CreateWorkspace { name, shells } => self.durable_mutation(|| {
                let workspace = self.create_workspace(name, shells)?;
                let events = workspace_created_events(&workspace);
                Ok((Response::Workspace { workspace }, events))
            }),
            Request::CreateShell {
                workspace_id,
                shell,
            } => self.durable_mutation(|| {
                let implicit_workspace = workspace_id.is_none();
                let shell = match workspace_id {
                    Some(workspace_id) => self.create_shell(&workspace_id, shell)?,
                    None => self.create_shell_with_workspace(shell)?,
                };
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
            Request::CreateLauncher { workspace_id, spec } => self.durable_mutation(|| {
                let launcher = self.create_launcher(&workspace_id, spec)?;
                let event = DaemonEventKind::LauncherCreated {
                    workspace_id,
                    launcher_id: launcher.id.clone(),
                    name: launcher.name.clone(),
                };
                Ok((Response::Launcher { launcher }, vec![event]))
            }),
            Request::RegisterAgent {
                shell_id,
                run_id,
                spec,
            } => self.durable_mutation(|| {
                let agent = self.register_agent(&shell_id, &run_id, spec)?;
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
                Ok((Response::Agent { agent }, events))
            }),
            Request::EnsureAgent {
                shell_id,
                run_id,
                spec,
            } => self.durable_mutation_outcome(|| {
                let (agent, created) = self.ensure_agent(&shell_id, &run_id, spec)?;
                if !created {
                    return Ok(DurableMutation::Unchanged(Response::Agent { agent }));
                }
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
                Ok(DurableMutation::Changed(Response::Agent { agent }, events))
            }),
            Request::ReportAgent {
                agent_id,
                run_id,
                report,
            } => self.durable_mutation_outcome(|| {
                let (agent, changed, completed) = self.report_agent(&agent_id, &run_id, report)?;
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
            Request::RenameWorkspace { workspace_id, name } => self.durable_mutation(|| {
                validate_name(&name)?;
                let workspace = self.workspace(&workspace_id)?;
                let state = lock(&self.state)?;
                for current in state.workspaces.values() {
                    if !Arc::ptr_eq(current, &workspace) && *lock(&current.name)? == name {
                        return Err(io::Error::new(
                            io::ErrorKind::AlreadyExists,
                            format!("workspace name already exists: {name}"),
                        ));
                    }
                }
                *lock(&workspace.name)? = name;
                Ok((
                    Response::Ok,
                    vec![DaemonEventKind::WorkspaceRenamed {
                        workspace_id,
                        name: lock(&workspace.name)?.clone(),
                    }],
                ))
            }),
            Request::RenameShell { shell_id, name } => self.durable_mutation(|| {
                self.rename_shell(&shell_id, name.clone())?;
                let shell = self.shell(&shell_id)?;
                Ok((
                    Response::Ok,
                    vec![DaemonEventKind::ShellRenamed {
                        workspace_id: shell.workspace_id.clone(),
                        shell_id,
                        name,
                    }],
                ))
            }),
            Request::RenameLauncher { launcher_id, name } => self.durable_mutation(|| {
                self.rename_launcher(&launcher_id, name.clone())?;
                let launcher = self.launcher(&launcher_id)?;
                Ok((
                    Response::Ok,
                    vec![DaemonEventKind::LauncherRenamed {
                        workspace_id: launcher.workspace_id.clone(),
                        launcher_id,
                        name,
                    }],
                ))
            }),
            Request::CloseWorkspace { workspace_id } => {
                self.close_workspace(&workspace_id)?;
                Ok(Response::Ok)
            }
            Request::CloseShell { shell_id } => {
                self.close_shell(&shell_id)?;
                Ok(Response::Ok)
            }
            Request::RestartShell { shell_id } => Ok(Response::Shell {
                shell: self.restart_shell(&shell_id)?,
            }),
            Request::RemoveLauncher { launcher_id } => self.durable_mutation(|| {
                let launcher = self.remove_launcher(&launcher_id)?;
                Ok((
                    Response::Ok,
                    vec![DaemonEventKind::LauncherRemoved {
                        workspace_id: launcher.workspace_id.clone(),
                        launcher_id,
                    }],
                ))
            }),
            Request::Attach { .. } => unreachable!("attach is handled before dispatch"),
        }
    }

    fn read_events(
        &self,
        after: Option<&EventCursor>,
        limit: u16,
        wait_ms: u32,
    ) -> io::Result<Response> {
        let limit = usize::from(limit.clamp(1, MAX_EVENT_BATCH));
        let deadline =
            Instant::now() + Duration::from_millis(u64::from(wait_ms)).min(MAX_EVENT_WAIT);
        if after.is_none() {
            let mut transition = lock(&self.transitions)?;
            let _persistence = lock(&self.persist_lock)?;
            let mut state = lock(&self.events.state)?;
            let published = self.flush_pending_locked(&mut transition, &mut state)?;
            let snapshot = self.snapshot()?;
            let cursor = EventCursor {
                stream_id: state.stream_id.clone(),
                event_id: state.latest_id,
            };
            let response = Response::Events {
                stream_id: state.stream_id.clone(),
                cursor,
                snapshot: Some(snapshot),
                events: Vec::new(),
            };
            drop(state);
            drop(transition);
            if published {
                self.events.changed.notify_all();
            }
            return Ok(response);
        }
        let mut state = lock(&self.events.state)?;
        let after = after.expect("checked above");
        loop {
            let earliest = state
                .events
                .front()
                .map_or(state.latest_id, |event| event.id.saturating_sub(1));
            if after.stream_id != state.stream_id
                || after.event_id < earliest
                || after.event_id > state.latest_id
            {
                return Err(coded_error(
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
            if self.stopping.load(Ordering::Acquire) {
                return Err(coded_error(
                    ErrorCode::DaemonStopping,
                    "Boomux daemon is stopping",
                ));
            }
            let timeout = deadline.saturating_duration_since(Instant::now());
            let (next, _) = self
                .events
                .changed
                .wait_timeout(state, timeout)
                .map_err(|_| io::Error::other("daemon event lock poisoned"))?;
            state = next;
        }
    }

    fn restore(
        store: StateStore,
        live_handoff: bool,
        transferred_events: Option<handoff::EventStreamManifest>,
    ) -> io::Result<Self> {
        let persisted = store.load()?.unwrap_or_default();
        let mut state = RegistryState::default();
        let mut workspace_names = HashSet::new();
        let mut run_ids = HashSet::new();
        let mut agent_ids = HashSet::new();
        let mut recovered_interrupted_run = false;
        for saved_workspace in persisted.workspaces {
            validate_id("workspace", &saved_workspace.id)?;
            validate_persisted_name(&saved_workspace.name)?;
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
                    workspace_id: saved_workspace.id.clone(),
                    name: Mutex::new(saved_shell.name),
                    cwd: saved_shell.cwd,
                    command: saved_shell.command,
                    last_run: Mutex::new(saved_shell.last_run),
                    lifecycle: Mutex::new(ShellLifecycle::Pending),
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
            let workspace = Arc::new(Workspace {
                id: saved_workspace.id.clone(),
                name: Mutex::new(saved_workspace.name),
                shell_ids: Mutex::new(shell_ids),
                launcher_ids: Mutex::new(launcher_ids),
                agent_ids: Mutex::new(workspace_agent_ids),
            });
            state.workspaces.insert(saved_workspace.id, workspace);
        }
        let registry = Self {
            state: Mutex::new(state),
            store: Some(store),
            events: EventLog::from_transfer(transferred_events),
            transitions: Mutex::new(TransitionState::default()),
            mutation_lock: Mutex::new(()),
            persist_lock: Mutex::new(()),
            stopping: AtomicBool::new(false),
            persistence_dirty: AtomicBool::new(false),
            notification_settings: NotificationDeliverySettings::default(),
            notification_sink: Arc::new(DisabledNotificationSink),
        };
        if recovered_interrupted_run {
            registry.persist()?;
        }
        Ok(registry)
    }

    fn import_handoff(
        self: &Arc<Self>,
        transferred: Vec<handoff::TransferredRuntime>,
        transferred_exited: Vec<handoff::TransferredExited>,
    ) -> io::Result<Vec<Arc<ShellRuntime>>> {
        let state = lock(&self.state)?;
        let mut prepared = Vec::with_capacity(transferred.len());
        let mut imported_shell_ids = HashSet::new();
        for transferred in transferred {
            let manifest = transferred.manifest;
            validate_terminal_profile(&manifest.profile)?;
            let shell = state
                .shells
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
            });
            prepared.push((shell, saved_run, run, runtime, reader));
        }
        let mut prepared_exited = Vec::with_capacity(transferred_exited.len());
        for transferred in transferred_exited {
            let manifest = transferred.manifest;
            validate_terminal_profile(&manifest.profile)?;
            let shell = state
                .shells
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
        for shell in state.shells.values() {
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
        drop(state);

        let mut readers = Vec::with_capacity(prepared.len());
        for (shell, saved_run, run, runtime, reader) in prepared {
            let profile = saved_run.profile.clone();
            *lock(&shell.last_run)? = Some(saved_run);
            *lock(&shell.lifecycle)? = ShellLifecycle::Running {
                profile,
                run: Arc::clone(&run),
                runtime: Arc::clone(&runtime),
            };
            start_pty_reader(
                Arc::downgrade(self),
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
        if interrupted_untransferred_run || !readers.is_empty() || !imported_shell_ids.is_empty() {
            self.persist()?;
        }
        Ok(readers)
    }

    fn durable_mutation<T>(
        &self,
        operation: impl FnOnce() -> io::Result<(T, Vec<DaemonEventKind>)>,
    ) -> io::Result<T> {
        self.durable_mutation_outcome(|| {
            let (value, events) = operation()?;
            Ok(DurableMutation::Changed(value, events))
        })
    }

    fn durable_mutation_outcome<T>(
        &self,
        operation: impl FnOnce() -> io::Result<DurableMutation<T>>,
    ) -> io::Result<T> {
        let mutation = lock(&self.mutation_lock)?;
        let mut transition = lock(&self.transitions)?;
        let persistence = lock(&self.persist_lock)?;
        let mut events = lock(&self.events.state)?;
        self.ensure_running()?;
        self.flush_pending_locked(&mut transition, &mut events)?;
        let backup = self.backup()?;
        match operation() {
            Ok(DurableMutation::Unchanged(value)) => Ok(value),
            Ok(DurableMutation::Changed(value, kinds)) => {
                if let Err(error) = EventLog::ensure_capacity(&events, kinds.len()) {
                    self.restore_backup(backup)?;
                    return Err(error);
                }
                let notifications = self.notification_requests(&kinds, &backup);
                match self.persist_unlocked().map_err(persistence_error) {
                    Ok(()) => {
                        EventLog::append_batch_locked(&mut events, kinds);
                        drop(events);
                        drop(transition);
                        drop(persistence);
                        self.events.changed.notify_all();
                        drop(mutation);
                        for notification in notifications {
                            self.notification_sink.notify(notification);
                        }
                        Ok(value)
                    }
                    Err(error) => {
                        self.restore_backup(backup)?;
                        self.persistence_dirty.store(false, Ordering::Release);
                        Err(error)
                    }
                }
            }
            Err(error) => {
                self.restore_backup(backup)?;
                Err(error)
            }
        }
    }

    fn notification_requests(
        &self,
        kinds: &[DaemonEventKind],
        previous: &RegistryBackup,
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
            let previous_state = previous
                .agent_states
                .get(&agent.id)
                .map(|state| state.observation.state);
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
                || !seen.insert((agent.id.as_str(), agent.observation.revision, reason))
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
        requests
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

    fn flush_pending_locked(
        &self,
        transition: &mut TransitionState,
        events: &mut EventLogState,
    ) -> io::Result<bool> {
        if transition.pending_durable_events.is_empty()
            && !self.persistence_dirty.load(Ordering::Acquire)
        {
            return Ok(false);
        }
        let count = transition.pending_durable_events.iter().map(Vec::len).sum();
        EventLog::ensure_capacity(events, count)?;
        self.persist_unlocked().map_err(persistence_error)?;
        while let Some(batch) = transition.pending_durable_events.pop_front() {
            EventLog::append_batch_locked(events, batch);
        }
        self.events.changed.notify_all();
        Ok(true)
    }

    fn flush_pending(&self) -> io::Result<()> {
        let mut transition = lock(&self.transitions)?;
        let _persistence = lock(&self.persist_lock)?;
        let mut events = lock(&self.events.state)?;
        let published = self.flush_pending_locked(&mut transition, &mut events)?;
        drop(events);
        drop(transition);
        if published {
            self.events.changed.notify_all();
        }
        Ok(())
    }

    fn compensate_stopped_locked(
        &self,
        shells: &[Arc<Shell>],
        transition: &mut TransitionState,
    ) -> io::Result<()> {
        let batch = shells
            .iter()
            .map(|shell| shell.compensate_stopped())
            .collect::<io::Result<Vec<_>>>()?
            .into_iter()
            .flatten()
            .collect::<Vec<_>>();
        self.queue_durable_batch_locked(batch, transition);
        Ok(())
    }

    fn queue_durable_batch_locked(
        &self,
        batch: Vec<DaemonEventKind>,
        transition: &mut TransitionState,
    ) {
        if !batch.is_empty() {
            transition.pending_durable_events.push_back(batch);
            self.persistence_dirty.store(true, Ordering::Release);
        }
    }

    fn publish_runtime_batch(&self, kinds: Vec<DaemonEventKind>) -> io::Result<()> {
        let _transition = lock(&self.transitions)?;
        let mut events = lock(&self.events.state)?;
        EventLog::ensure_capacity(&events, kinds.len())?;
        EventLog::append_batch_locked(&mut events, kinds);
        drop(events);
        self.events.changed.notify_all();
        Ok(())
    }

    fn ensure_running(&self) -> io::Result<()> {
        if self.stopping.load(Ordering::Acquire) {
            Err(io::Error::new(
                io::ErrorKind::ConnectionAborted,
                "Boomux daemon is stopping",
            ))
        } else {
            Ok(())
        }
    }

    fn quiesce_controllers(&self) -> io::Result<()> {
        let runtimes = {
            let state = lock(&self.state)?;
            let mut runtimes = Vec::new();
            for shell in state.shells.values() {
                if let ShellLifecycle::Running { runtime, .. } = &*lock(&shell.lifecycle)? {
                    runtimes.push(Arc::clone(runtime));
                }
            }
            runtimes
        };
        let mut acknowledgements = Vec::new();
        for runtime in &runtimes {
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
            for runtime in &runtimes {
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

    fn state_lock_descriptor(&self) -> io::Result<BorrowedFd<'_>> {
        self.store
            .as_ref()
            .ok_or_else(|| io::Error::other("registry has no persistent state store"))?
            .lock_descriptor()
    }

    fn backup(&self) -> io::Result<RegistryBackup> {
        let state = lock(&self.state)?;
        let mut workspace_names = HashMap::new();
        let mut workspace_shell_ids = HashMap::new();
        let mut workspace_launcher_ids = HashMap::new();
        let mut workspace_agent_ids = HashMap::new();
        for workspace in state.workspaces.values() {
            workspace_names.insert(workspace.id.clone(), lock(&workspace.name)?.clone());
            workspace_shell_ids.insert(workspace.id.clone(), lock(&workspace.shell_ids)?.clone());
            workspace_launcher_ids
                .insert(workspace.id.clone(), lock(&workspace.launcher_ids)?.clone());
            workspace_agent_ids.insert(workspace.id.clone(), lock(&workspace.agent_ids)?.clone());
        }
        let mut shell_names = HashMap::new();
        for shell in state.shells.values() {
            shell_names.insert(shell.id.clone(), lock(&shell.name)?.clone());
        }
        let mut launcher_names = HashMap::new();
        for launcher in state.launchers.values() {
            launcher_names.insert(launcher.id.clone(), lock(&launcher.name)?.clone());
        }
        let mut agent_states = HashMap::new();
        for agent in state.agents.values() {
            agent_states.insert(agent.id.clone(), lock(&agent.state)?.clone());
        }
        Ok(RegistryBackup {
            workspaces: state.workspaces.clone(),
            shells: state.shells.clone(),
            launchers: state.launchers.clone(),
            agents: state.agents.clone(),
            workspace_names,
            workspace_shell_ids,
            workspace_launcher_ids,
            workspace_agent_ids,
            shell_names,
            launcher_names,
            agent_states,
        })
    }

    fn restore_backup(&self, backup: RegistryBackup) -> io::Result<()> {
        let mut state = lock(&self.state)?;
        for workspace in backup.workspaces.values() {
            if let Some(name) = backup.workspace_names.get(&workspace.id) {
                *lock(&workspace.name)? = name.clone();
            }
            if let Some(ids) = backup.workspace_shell_ids.get(&workspace.id) {
                *lock(&workspace.shell_ids)? = ids.clone();
            }
            if let Some(ids) = backup.workspace_launcher_ids.get(&workspace.id) {
                *lock(&workspace.launcher_ids)? = ids.clone();
            }
            if let Some(ids) = backup.workspace_agent_ids.get(&workspace.id) {
                *lock(&workspace.agent_ids)? = ids.clone();
            }
        }
        for shell in backup.shells.values() {
            if let Some(name) = backup.shell_names.get(&shell.id) {
                *lock(&shell.name)? = name.clone();
            }
        }
        for launcher in backup.launchers.values() {
            if let Some(name) = backup.launcher_names.get(&launcher.id) {
                *lock(&launcher.name)? = name.clone();
            }
        }
        for agent in backup.agents.values() {
            if let Some(agent_state) = backup.agent_states.get(&agent.id) {
                *lock(&agent.state)? = agent_state.clone();
            }
        }
        state.workspaces = backup.workspaces;
        state.shells = backup.shells;
        state.launchers = backup.launchers;
        state.agents = backup.agents;
        Ok(())
    }

    fn persist(&self) -> io::Result<()> {
        let _persist = lock(&self.persist_lock)?;
        self.persist_unlocked()
    }

    fn persist_unlocked(&self) -> io::Result<()> {
        let Some(store) = &self.store else {
            return Ok(());
        };
        let state = lock(&self.state)?;
        let mut workspaces = state.workspaces.values().cloned().collect::<Vec<_>>();
        workspaces.sort_by(|left, right| left.id.cmp(&right.id));
        let mut saved = PersistedState::default();
        for workspace in workspaces {
            let ids = lock(&workspace.shell_ids)?.clone();
            let mut shells = Vec::with_capacity(ids.len());
            for id in ids {
                let Some(shell) = state.shells.get(&id) else {
                    continue;
                };
                shells.push(PersistedShell {
                    id: shell.id.clone(),
                    name: lock(&shell.name)?.clone(),
                    cwd: shell.cwd.clone(),
                    command: shell.command.clone(),
                    last_run: lock(&shell.last_run)?.clone(),
                });
            }
            let ids = lock(&workspace.launcher_ids)?.clone();
            let mut launchers = Vec::with_capacity(ids.len());
            for id in ids {
                let Some(launcher) = state.launchers.get(&id) else {
                    continue;
                };
                launchers.push(PersistedWorkspaceLauncher {
                    id: launcher.id.clone(),
                    name: lock(&launcher.name)?.clone(),
                    cwd: launcher.cwd.clone(),
                    command: launcher.command.clone(),
                });
            }
            let ids = lock(&workspace.agent_ids)?.clone();
            let mut agents = Vec::with_capacity(ids.len());
            for id in ids {
                let Some(agent) = state.agents.get(&id) else {
                    continue;
                };
                agents.push(agent.persisted()?);
            }
            saved.workspaces.push(PersistedWorkspace {
                id: workspace.id.clone(),
                name: lock(&workspace.name)?.clone(),
                shells,
                launchers,
                agents,
            });
        }
        drop(state);
        let result = store.save(&saved);
        self.persistence_dirty
            .store(result.is_err(), Ordering::Release);
        result
    }

    fn record_run_exit(
        &self,
        shell: &Arc<Shell>,
        run: &Arc<ShellRun>,
        runtime: &Arc<ShellRuntime>,
        code: Option<u32>,
    ) -> io::Result<()> {
        let mut transition = lock(&self.transitions)?;
        let _persistence = lock(&self.persist_lock)?;
        let mut events = lock(&self.events.state)?;
        let pending_count = transition
            .pending_durable_events
            .iter()
            .map(Vec::len)
            .sum::<usize>();
        EventLog::ensure_capacity(&events, pending_count.saturating_add(1))?;
        let mut lifecycle = lock(&shell.lifecycle)?;
        let profile = match &*lifecycle {
            ShellLifecycle::Running {
                profile,
                run: current_run,
                runtime: current_runtime,
            } if Arc::ptr_eq(current_run, run) && Arc::ptr_eq(current_runtime, runtime) => {
                profile.clone()
            }
            _ => return Ok(()),
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
        let batch = vec![DaemonEventKind::RunExited {
            workspace_id: shell.workspace_id.clone(),
            shell_id: shell.id.clone(),
            run: run.snapshot()?,
        }];
        match self.persist_unlocked().map_err(persistence_error) {
            Ok(()) => {
                while let Some(pending) = transition.pending_durable_events.pop_front() {
                    EventLog::append_batch_locked(&mut events, pending);
                }
                EventLog::append_batch_locked(&mut events, batch);
                drop(events);
                drop(transition);
                self.events.changed.notify_all();
                Ok(())
            }
            Err(error) => {
                transition.pending_durable_events.push_back(batch);
                Err(error)
            }
        }
    }

    fn snapshot(&self) -> io::Result<Snapshot> {
        let mut workspaces: Vec<_> = lock(&self.state)?.workspaces.values().cloned().collect();
        workspaces.sort_by(|a, b| a.id.cmp(&b.id));
        Ok(Snapshot {
            workspaces: workspaces
                .into_iter()
                .map(|workspace| workspace.snapshot(self))
                .collect::<io::Result<_>>()?,
        })
    }

    fn shutdown(&self) -> io::Result<()> {
        let _mutation = lock(&self.mutation_lock)?;
        if self.stopping.swap(true, Ordering::AcqRel) {
            return Ok(());
        }
        self.events.notify();
        let shells = {
            let state = lock(&self.state)?;
            state.shells.values().cloned().collect::<Vec<_>>()
        };
        let mut stopped: Vec<Arc<Shell>> = Vec::with_capacity(shells.len());
        for shell in &shells {
            if let Err(error) = shell.stop_runtime() {
                if error.stopped {
                    stopped.push(Arc::clone(shell));
                }
                let mut transition = lock(&self.transitions)?;
                self.compensate_stopped_locked(&stopped, &mut transition)?;
                self.stopping.store(false, Ordering::Release);
                return Err(error.source);
            }
            stopped.push(Arc::clone(shell));
        }
        let mut transition = lock(&self.transitions)?;
        let _persistence = lock(&self.persist_lock)?;
        let mut events = lock(&self.events.state)?;
        let published = match self.flush_pending_locked(&mut transition, &mut events) {
            Ok(published) => published,
            Err(error) => {
                self.compensate_stopped_locked(&stopped, &mut transition)?;
                self.stopping.store(false, Ordering::Release);
                return Err(error);
            }
        };
        let mut rollbacks = Vec::with_capacity(shells.len());
        for shell in &shells {
            match shell.finalize_stop() {
                Ok(rollback) => rollbacks.push((Arc::clone(shell), rollback)),
                Err(error) => {
                    for (shell, rollback) in rollbacks {
                        shell.restore_stopped(rollback)?;
                    }
                    self.stopping.store(false, Ordering::Release);
                    return Err(error);
                }
            }
        }
        if let Err(error) = self.persist_unlocked().map_err(persistence_error) {
            let batch = rollbacks
                .iter()
                .filter_map(|(_, rollback)| rollback.event.clone())
                .collect();
            for (shell, rollback) in rollbacks {
                shell.restore_stopped(rollback)?;
            }
            self.queue_durable_batch_locked(batch, &mut transition);
            self.stopping.store(false, Ordering::Release);
            return Err(error);
        }
        let mut state = lock(&self.state)?;
        state.workspaces.clear();
        state.shells.clear();
        state.launchers.clear();
        state.agents.clear();
        drop(state);
        drop(events);
        drop(transition);
        if published {
            self.events.changed.notify_all();
        }
        Ok(())
    }

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

    fn contains_shell(&self, shell: &Arc<Shell>) -> io::Result<bool> {
        Ok(lock(&self.state)?
            .shells
            .get(&shell.id)
            .is_some_and(|current| Arc::ptr_eq(current, shell)))
    }

    fn create_workspace(
        &self,
        name: String,
        specs: Vec<ShellSpec>,
    ) -> io::Result<WorkspaceSnapshot> {
        validate_name(&name)?;
        let workspace_id = Uuid::new_v4().to_string();
        validate_shell_specs(&specs)?;

        let mut shells = Vec::with_capacity(specs.len());
        for spec in specs {
            match create_pending_shell(&workspace_id, spec) {
                Ok(shell) => shells.push(shell),
                Err(error) => {
                    for shell in shells {
                        let _ = shell.kill();
                    }
                    return Err(error);
                }
            }
        }

        let workspace_name = name.clone();
        let workspace = Arc::new(Workspace {
            id: workspace_id.clone(),
            name: Mutex::new(name),
            shell_ids: Mutex::new(shells.iter().map(|shell| shell.id.clone()).collect()),
            launcher_ids: Mutex::new(Vec::new()),
            agent_ids: Mutex::new(Vec::new()),
        });
        {
            let mut state = lock(&self.state)?;
            for current in state.workspaces.values() {
                if *lock(&current.name)? == workspace_name {
                    drop(state);
                    for shell in shells {
                        let _ = shell.kill();
                    }
                    return Err(io::Error::new(
                        io::ErrorKind::AlreadyExists,
                        format!("workspace name already exists: {workspace_name}"),
                    ));
                }
            }
            for shell in &shells {
                state.shells.insert(shell.id.clone(), Arc::clone(shell));
            }
            state
                .workspaces
                .insert(workspace_id, Arc::clone(&workspace));
        }
        workspace.snapshot(self)
    }

    fn create_shell(&self, workspace_id: &str, spec: ShellSpec) -> io::Result<ShellSnapshot> {
        let workspace = self.workspace(workspace_id)?;
        let shell = create_pending_shell(workspace_id, spec)?;
        let snapshot = shell.snapshot()?;
        let mut state = lock(&self.state)?;
        let Some(current) = state.workspaces.get(workspace_id) else {
            drop(state);
            let _ = shell.kill();
            return Err(not_found("workspace", workspace_id));
        };
        if !Arc::ptr_eq(current, &workspace) {
            drop(state);
            let _ = shell.kill();
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
            drop(shell_ids);
            drop(state);
            let _ = shell.kill();
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!("shell name already exists: {}", snapshot.name),
            ));
        }
        state.shells.insert(shell.id.clone(), Arc::clone(&shell));
        shell_ids.push(shell.id.clone());
        Ok(snapshot)
    }

    fn create_shell_with_workspace(&self, spec: ShellSpec) -> io::Result<ShellSnapshot> {
        loop {
            let name = self.next_workspace_name()?;
            match self.create_workspace(name, vec![spec.clone()]) {
                Ok(workspace) => {
                    return workspace
                        .shells
                        .into_iter()
                        .next()
                        .ok_or_else(|| io::Error::other("implicit workspace has no shell"));
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
    ) -> io::Result<WorkspaceLauncherSnapshot> {
        validate_name(&spec.name)?;
        validate_cwd(&spec.cwd)?;
        validate_launcher_command(&spec.command)?;
        let workspace = self.workspace(workspace_id)?;
        let launcher = Arc::new(WorkspaceLauncher {
            id: Uuid::new_v4().to_string(),
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
        Ok(snapshot)
    }

    fn register_agent(
        &self,
        shell_id: &str,
        run_id: &str,
        spec: AgentRegistrationSpec,
    ) -> io::Result<AgentInstanceSnapshot> {
        validate_agent_registration(&spec)?;
        validate_external_agent_authority(spec.report.authority)?;
        let shell = self.shell(shell_id)?;
        let lifecycle = lock(&shell.lifecycle)?;
        match &*lifecycle {
            ShellLifecycle::Running { run, .. } if run.id == run_id => {}
            ShellLifecycle::Running { .. }
            | ShellLifecycle::Exited { .. }
            | ShellLifecycle::Pending => {
                return Err(coded_error(
                    ErrorCode::RunChanged,
                    "shell does not have the requested active run",
                ));
            }
            ShellLifecycle::Closed => return Err(not_found("shell", shell_id)),
        }
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
            let mut state = lock(&agent.state)?;
            state.attention = attention_for_observation(&state.observation);
        }
        let snapshot = agent.snapshot()?;
        let mut state = lock(&self.state)?;
        let Some(current_shell) = state.shells.get(shell_id) else {
            return Err(not_found("shell", shell_id));
        };
        let Some(current_workspace) = state.workspaces.get(&shell.workspace_id) else {
            return Err(not_found("workspace", &shell.workspace_id));
        };
        if !Arc::ptr_eq(current_shell, &shell) || !Arc::ptr_eq(current_workspace, &workspace) {
            return Err(not_found("shell", shell_id));
        }
        state.agents.insert(agent.id.clone(), Arc::clone(&agent));
        lock(&workspace.agent_ids)?.push(agent.id.clone());
        drop(lifecycle);
        Ok(snapshot)
    }

    fn ensure_agent(
        &self,
        shell_id: &str,
        run_id: &str,
        spec: AgentRegistrationSpec,
    ) -> io::Result<(AgentInstanceSnapshot, bool)> {
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
                        ));
                    }
                }
            }
        };
        if let Some(agent) = existing {
            return Ok((agent.snapshot()?, false));
        }

        self.register_agent(shell_id, run_id, spec)
            .map(|agent| (agent, true))
    }

    fn report_agent(
        &self,
        agent_id: &str,
        run_id: &str,
        report: AgentReport,
    ) -> io::Result<(AgentInstanceSnapshot, bool, bool)> {
        validate_agent_report(&report)?;
        validate_external_agent_authority(report.authority)?;
        let agent = self.agent(agent_id)?;
        if agent.run_id != run_id {
            return Err(coded_error(
                ErrorCode::RunChanged,
                "agent instance is bound to a different shell run",
            ));
        }
        let mut state = lock(&agent.state)?;
        if state.ended_at_ms.is_some() {
            if observation_matches_report(&state.observation, &report) {
                drop(state);
                return Ok((agent.snapshot()?, false, true));
            }
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "completed agent instance cannot be reported again",
            ));
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
            drop(state);
            return Ok((agent.snapshot()?, false, false));
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
        drop(state);
        Ok((agent.snapshot()?, true, completed))
    }

    fn acknowledge_agent_attention(
        &self,
        agent_id: &str,
        observation_revision: u64,
    ) -> io::Result<(AgentInstanceSnapshot, bool)> {
        let agent = self.agent(agent_id)?;
        let mut state = lock(&agent.state)?;
        let Some(attention) = &state.attention else {
            drop(state);
            return Ok((agent.snapshot()?, false));
        };
        if attention.observation.revision != observation_revision {
            return Err(coded_error(
                ErrorCode::RevisionAhead,
                format!(
                    "agent attention observation revision is {}; acknowledgment supplied {}",
                    attention.observation.revision, observation_revision
                ),
            ));
        }
        state.attention = None;
        drop(state);
        Ok((agent.snapshot()?, true))
    }

    fn rename_launcher(&self, launcher_id: &str, name: String) -> io::Result<()> {
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
        *lock(&launcher.name)? = name;
        Ok(())
    }

    fn remove_launcher(&self, launcher_id: &str) -> io::Result<Arc<WorkspaceLauncher>> {
        let (launcher, workspace) = {
            let mut state = lock(&self.state)?;
            let launcher = state
                .launchers
                .remove(launcher_id)
                .ok_or_else(|| not_found("workspace launcher", launcher_id))?;
            let workspace = state.workspaces.get(&launcher.workspace_id).cloned();
            (launcher, workspace)
        };
        if let Some(workspace) = workspace {
            lock(&workspace.launcher_ids)?.retain(|id| id != launcher_id);
        }
        Ok(launcher)
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

    fn rename_shell(&self, shell_id: &str, name: String) -> io::Result<()> {
        validate_name(&name)?;
        let shell = self.shell(shell_id)?;
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
        *lock(&shell.name)? = name;
        Ok(())
    }

    fn read_shell(&self, shell_id: &str, max_bytes: usize) -> io::Result<Vec<u8>> {
        let shell = self.shell(shell_id)?;
        let lifecycle = lock(&shell.lifecycle)?;
        let terminal = match &*lifecycle {
            ShellLifecycle::Pending => return Ok(Vec::new()),
            ShellLifecycle::Running { runtime, .. } => Arc::clone(&runtime.terminal),
            ShellLifecycle::Exited { terminal, .. } => Arc::clone(terminal),
            ShellLifecycle::Closed => return Err(not_found("shell", shell_id)),
        };
        drop(lifecycle);
        let text = lock(&terminal)?.plain_text();
        Ok(tail_utf8(&text, max_bytes.min(MAX_SHELL_READ_BYTES))
            .as_bytes()
            .to_vec())
    }

    fn wait_agent(
        &self,
        agent_id: &str,
        after_revision: u64,
        wait_ms: u32,
    ) -> io::Result<Response> {
        let deadline =
            Instant::now() + Duration::from_millis(u64::from(wait_ms)).min(MAX_EVENT_WAIT);
        let mut events = lock(&self.events.state)?;
        loop {
            if self.stopping.load(Ordering::Acquire) {
                return Err(coded_error(
                    ErrorCode::DaemonStopping,
                    "Boomux daemon is stopping",
                ));
            }
            let agent = self.agent(agent_id)?.snapshot()?;
            let revision = agent.observation.revision;
            if after_revision < revision {
                return Ok(Response::AgentWait {
                    agent,
                    changed: true,
                });
            }
            if after_revision > revision {
                return Err(coded_error(
                    ErrorCode::RevisionAhead,
                    "requested Agent revision is ahead of the current observation",
                ));
            }
            if agent.observation.state == AgentState::Done
                || wait_ms == 0
                || Instant::now() >= deadline
            {
                return Ok(Response::AgentWait {
                    agent,
                    changed: false,
                });
            }
            let timeout = deadline.saturating_duration_since(Instant::now());
            let (next, _) = self
                .events
                .changed
                .wait_timeout(events, timeout)
                .map_err(|_| io::Error::other("event wait lock poisoned"))?;
            events = next;
        }
    }

    fn read_shell_at(
        &self,
        shell_id: &str,
        max_bytes: usize,
        expected_run_id: Option<&str>,
        after_revision: Option<u64>,
        wait_ms: u32,
    ) -> io::Result<Response> {
        if expected_run_id.is_some() != after_revision.is_some() {
            return Err(coded_error(
                ErrorCode::InvalidArgument,
                "run_id and after_revision must be provided together",
            ));
        }
        let deadline =
            Instant::now() + Duration::from_millis(u64::from(wait_ms)).min(MAX_EVENT_WAIT);
        loop {
            let observed_event_id = lock(&self.events.state)?.latest_id;
            let shell = self.shell(shell_id)?;
            let lifecycle = lock(&shell.lifecycle)?;
            let (status, run, terminal) = match &*lifecycle {
                ShellLifecycle::Pending => {
                    if expected_run_id.is_some() {
                        return Err(coded_error(
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
                    Arc::clone(terminal),
                ),
                ShellLifecycle::Closed => return Err(not_found("shell", shell_id)),
            };
            drop(lifecycle);
            let terminal_state = lock(&terminal)?;
            let revision = run.output_revision.load(Ordering::Acquire);
            let changed = after_revision.is_none_or(|after| after < revision);
            let bytes = if changed || after_revision.is_none() {
                let text = terminal_state.plain_text();
                tail_utf8(&text, max_bytes.min(MAX_SHELL_READ_BYTES))
                    .as_bytes()
                    .to_vec()
            } else {
                Vec::new()
            };
            drop(terminal_state);
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
                return Err(coded_error(
                    ErrorCode::RunChanged,
                    "shell run identity changed",
                ));
            }
            if after_revision.is_some_and(|after| after > revision) {
                return Err(coded_error(
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
            if self.stopping.load(Ordering::Acquire) {
                return Err(coded_error(
                    ErrorCode::DaemonStopping,
                    "Boomux daemon is stopping",
                ));
            }
            let state = lock(&self.events.state)?;
            if self.stopping.load(Ordering::Acquire) {
                return Err(coded_error(
                    ErrorCode::DaemonStopping,
                    "Boomux daemon is stopping",
                ));
            }
            if state.latest_id != observed_event_id {
                continue;
            }
            let timeout = deadline.saturating_duration_since(Instant::now());
            let _ = self
                .events
                .changed
                .wait_timeout(state, timeout)
                .map_err(|_| io::Error::other("daemon event lock poisoned"))?;
        }
    }

    fn remove_shell(&self, shell_id: &str) -> io::Result<Arc<Shell>> {
        let (shell, workspace) = {
            let mut state = lock(&self.state)?;
            let shell = state
                .shells
                .remove(shell_id)
                .ok_or_else(|| not_found("shell", shell_id))?;
            let workspace = state.workspaces.get(&shell.workspace_id).cloned();
            (shell, workspace)
        };
        if let Some(workspace) = workspace {
            lock(&workspace.shell_ids)?.retain(|id| id != shell_id);
        }
        Ok(shell)
    }

    fn remove_workspace(&self, workspace_id: &str) -> io::Result<Vec<Arc<Shell>>> {
        let shells = {
            let mut state = lock(&self.state)?;
            let workspace = state
                .workspaces
                .remove(workspace_id)
                .ok_or_else(|| not_found("workspace", workspace_id))?;
            for id in lock(&workspace.launcher_ids)?.iter() {
                state.launchers.remove(id);
            }
            for id in lock(&workspace.agent_ids)?.iter() {
                state.agents.remove(id);
            }
            let ids = lock(&workspace.shell_ids)?.clone();
            ids.into_iter()
                .filter_map(|id| state.shells.remove(&id))
                .collect::<Vec<_>>()
        };
        Ok(shells)
    }

    fn close_shell(&self, shell_id: &str) -> io::Result<()> {
        let _mutation = lock(&self.mutation_lock)?;
        self.ensure_running()?;
        let backup = self.backup()?;
        let shell = self.shell(shell_id)?;
        if let Err(error) = shell.stop_runtime() {
            if error.stopped {
                let mut transition = lock(&self.transitions)?;
                self.compensate_stopped_locked(std::slice::from_ref(&shell), &mut transition)?;
            }
            return Err(error.source);
        }
        let mut transition = lock(&self.transitions)?;
        let _persistence = lock(&self.persist_lock)?;
        let mut events = lock(&self.events.state)?;
        if let Err(error) = self.flush_pending_locked(&mut transition, &mut events) {
            self.compensate_stopped_locked(std::slice::from_ref(&shell), &mut transition)?;
            return Err(error);
        }
        if let Err(error) = EventLog::ensure_capacity(&events, 1) {
            self.compensate_stopped_locked(std::slice::from_ref(&shell), &mut transition)?;
            return Err(error);
        }
        let rollback = shell.finalize_stop()?;
        self.remove_shell(shell_id)?;
        if let Err(error) = self.persist_unlocked().map_err(persistence_error) {
            self.restore_backup(backup)?;
            let event = rollback.event.clone();
            shell.restore_stopped(rollback)?;
            self.queue_durable_batch_locked(event.into_iter().collect(), &mut transition);
            return Err(error);
        }
        EventLog::append_batch_locked(
            &mut events,
            vec![DaemonEventKind::ShellClosed {
                workspace_id: Some(shell.workspace_id.clone()),
                shell_id: shell_id.into(),
            }],
        );
        drop(events);
        drop(transition);
        self.events.changed.notify_all();
        Ok(())
    }

    fn close_workspace(&self, workspace_id: &str) -> io::Result<()> {
        let _mutation = lock(&self.mutation_lock)?;
        self.ensure_running()?;
        let backup = self.backup()?;
        let workspace = self.workspace(workspace_id)?;
        let shells = {
            let state = lock(&self.state)?;
            let ids = lock(&workspace.shell_ids)?;
            ids.iter()
                .filter_map(|id| state.shells.get(id).cloned())
                .collect::<Vec<_>>()
        };
        let mut stopped: Vec<Arc<Shell>> = Vec::with_capacity(shells.len());
        for shell in &shells {
            if let Err(error) = shell.stop_runtime() {
                if error.stopped {
                    stopped.push(Arc::clone(shell));
                }
                let mut transition = lock(&self.transitions)?;
                self.compensate_stopped_locked(&stopped, &mut transition)?;
                return Err(error.source);
            }
            stopped.push(Arc::clone(shell));
        }
        let mut transition = lock(&self.transitions)?;
        let _persistence = lock(&self.persist_lock)?;
        let mut events = lock(&self.events.state)?;
        if let Err(error) = self.flush_pending_locked(&mut transition, &mut events) {
            self.compensate_stopped_locked(&stopped, &mut transition)?;
            return Err(error);
        }
        if let Err(error) = EventLog::ensure_capacity(&events, 1) {
            self.compensate_stopped_locked(&stopped, &mut transition)?;
            return Err(error);
        }
        let mut rollbacks = Vec::with_capacity(shells.len());
        for shell in &shells {
            match shell.finalize_stop() {
                Ok(rollback) => rollbacks.push((Arc::clone(shell), rollback)),
                Err(error) => {
                    for (shell, rollback) in rollbacks {
                        shell.restore_stopped(rollback)?;
                    }
                    return Err(error);
                }
            }
        }
        self.remove_workspace(workspace_id)?;
        if let Err(error) = self.persist_unlocked().map_err(persistence_error) {
            self.restore_backup(backup)?;
            let batch = rollbacks
                .iter()
                .filter_map(|(_, rollback)| rollback.event.clone())
                .collect();
            for (shell, rollback) in rollbacks {
                shell.restore_stopped(rollback)?;
            }
            self.queue_durable_batch_locked(batch, &mut transition);
            return Err(error);
        }
        EventLog::append_batch_locked(
            &mut events,
            vec![DaemonEventKind::WorkspaceClosed {
                workspace_id: workspace_id.into(),
            }],
        );
        drop(events);
        drop(transition);
        self.events.changed.notify_all();
        Ok(())
    }

    fn restart_shell(&self, shell_id: &str) -> io::Result<ShellSnapshot> {
        let _mutation = lock(&self.mutation_lock)?;
        self.ensure_running()?;
        let shell = self.shell(shell_id)?;
        let _transition = lock(&self.transitions)?;
        let old_runtime = {
            let mut lifecycle = lock(&shell.lifecycle)?;
            let old_runtime = match &*lifecycle {
                ShellLifecycle::Pending => {
                    drop(lifecycle);
                    return shell.snapshot();
                }
                ShellLifecycle::Running { .. } => {
                    return Err(coded_error(
                        ErrorCode::Busy,
                        format!("shell is still running: {shell_id}"),
                    ));
                }
                ShellLifecycle::Exited { runtime, .. } => runtime.clone(),
                ShellLifecycle::Closed => return Err(not_found("shell", shell_id)),
            };
            *lifecycle = ShellLifecycle::Pending;
            old_runtime
        };
        if let Some(runtime) = old_runtime {
            runtime.stop_reader()?;
        }
        shell.snapshot()
    }
}

impl Workspace {
    fn snapshot(&self, registry: &Registry) -> io::Result<WorkspaceSnapshot> {
        let (shells, launchers, agents) = {
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
            (shells, launchers, agents)
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
        Ok(WorkspaceSnapshot {
            id: self.id.clone(),
            name: lock(&self.name)?.clone(),
            shells,
            launchers,
            agents,
        })
    }
}

impl WorkspaceLauncher {
    fn snapshot(&self) -> io::Result<WorkspaceLauncherSnapshot> {
        Ok(WorkspaceLauncherSnapshot {
            id: self.id.clone(),
            workspace_id: self.workspace_id.clone(),
            name: lock(&self.name)?.clone(),
            command: self.command.clone(),
            cwd: self.cwd.clone(),
        })
    }
}

impl AgentInstance {
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
        Ok(AgentInstanceSnapshot {
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
        })
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
        let foreground_process = runtime.and_then(|runtime| {
            lock(&runtime.master)
                .ok()
                .and_then(|master| master.foreground_process())
                .or_else(|| {
                    let pid = lock(&runtime.process).ok()?.process_id()?;
                    foreground_process_for_session_leader(pid)
                })
        });
        Ok(ShellSnapshot {
            id: self.id.clone(),
            workspace_id: self.workspace_id.clone(),
            name: lock(&self.name)?.clone(),
            cwd: self.cwd.clone(),
            command: self.command.clone(),
            status,
            run,
            foreground_process,
        })
    }

    fn kill(&self) -> io::Result<()> {
        match self.stop_runtime() {
            Ok(()) => {
                let _ = self.finalize_stop()?;
                Ok(())
            }
            Err(error) => {
                if error.stopped {
                    let _ = self.finalize_stop()?;
                }
                Err(error.source)
            }
        }
    }

    fn stop_runtime(&self) -> Result<(), StopRuntimeError> {
        let runtime = {
            let lifecycle = lock(&self.lifecycle)?;
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
        runtime.pause_reader()?;
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
            runtime.resume_reader()?;
            return Err(error.into());
        }
        if let Some(session_id) = lock(&runtime.process)?.process_id()
            && let Err(source) = wait_for_session_descendants(session_id as libc::pid_t)
        {
            let _ = runtime.stop_reader();
            return Err(StopRuntimeError {
                source,
                stopped: true,
            });
        }
        if let Err(source) = runtime.stop_reader() {
            return Err(StopRuntimeError {
                source,
                stopped: true,
            });
        }
        Ok(())
    }

    fn finalize_stop(&self) -> io::Result<StopRollback> {
        let mut lifecycle = lock(&self.lifecycle)?;
        let previous = std::mem::replace(&mut *lifecycle, ShellLifecycle::Closed);
        match previous {
            ShellLifecycle::Pending => Ok(StopRollback {
                lifecycle: ShellLifecycle::Pending,
                event: None,
            }),
            ShellLifecycle::Running { profile, run, .. } => {
                run.finish(ShellRunExitReason::Terminated)?;
                *lock(&self.last_run)? = Some(run.persisted(profile.clone())?);
                Ok(StopRollback {
                    lifecycle: ShellLifecycle::Pending,
                    event: Some(DaemonEventKind::RunExited {
                        workspace_id: self.workspace_id.clone(),
                        shell_id: self.id.clone(),
                        run: run.snapshot()?,
                    }),
                })
            }
            ShellLifecycle::Exited {
                profile,
                run,
                code,
                runtime,
                terminal,
            } => {
                *lock(&self.last_run)? = Some(run.persisted(profile.clone())?);
                Ok(StopRollback {
                    lifecycle: ShellLifecycle::Exited {
                        code,
                        profile,
                        run,
                        runtime,
                        terminal,
                    },
                    event: None,
                })
            }
            ShellLifecycle::Closed => Ok(StopRollback {
                lifecycle: ShellLifecycle::Closed,
                event: None,
            }),
        }
    }

    fn restore_stopped(&self, rollback: StopRollback) -> io::Result<()> {
        let mut lifecycle = lock(&self.lifecycle)?;
        if matches!(*lifecycle, ShellLifecycle::Closed) {
            *lifecycle = rollback.lifecycle;
        }
        Ok(())
    }

    fn compensate_stopped(&self) -> io::Result<Option<DaemonEventKind>> {
        let rollback = self.finalize_stop()?;
        let event = rollback.event.clone();
        self.restore_stopped(rollback)?;
        Ok(event)
    }

    fn reset_pending(&self) -> io::Result<()> {
        let mut lifecycle = lock(&self.lifecycle)?;
        if matches!(*lifecycle, ShellLifecycle::Closed) {
            *lifecycle = ShellLifecycle::Pending;
        }
        Ok(())
    }
}

impl ShellRuntime {
    fn release_controller(&self, token: &str) -> io::Result<()> {
        let mut controller = lock(&self.controller)?;
        if controller
            .as_ref()
            .is_some_and(|current| current.token == token)
        {
            controller.take();
        }
        Ok(())
    }

    fn pause_reader(&self) -> io::Result<()> {
        if let Some(reader) = lock(&self.reader)?.as_ref() {
            reader.pause()?;
        }
        Ok(())
    }

    fn resume_reader(&self) -> io::Result<()> {
        if let Some(reader) = lock(&self.reader)?.as_ref() {
            reader.resume()?;
        }
        Ok(())
    }

    fn stop_reader(&self) -> io::Result<()> {
        let reader = lock(&self.reader)?.take();
        if let Some(reader) = reader {
            reader.stop()?;
        }
        Ok(())
    }
}

fn create_pending_shell(workspace_id: &str, spec: ShellSpec) -> io::Result<Arc<Shell>> {
    validate_name(&spec.name)?;
    validate_cwd(&spec.cwd)?;
    Ok(Arc::new(Shell {
        id: Uuid::new_v4().to_string(),
        workspace_id: workspace_id.to_owned(),
        name: Mutex::new(spec.name),
        cwd: spec.cwd,
        command: spec.command,
        last_run: Mutex::new(None),
        lifecycle: Mutex::new(ShellLifecycle::Pending),
    }))
}

fn initial_terminal_state(
    rows: u16,
    cols: u16,
    workspace_name: &str,
    shell_name: &str,
) -> TerminalState {
    let mut terminal = TerminalState::new(rows, cols);
    terminal.process(format!("\x1b[2mBoomux: {workspace_name}/{shell_name}\x1b[0m\r\n").as_bytes());
    terminal
}

fn spawn_runtime(
    shell: &Arc<Shell>,
    run: &ShellRun,
    workspace_name: &str,
    shell_name: &str,
    profile: &TerminalProfile,
    environment: Option<&UnixEnvironment>,
) -> io::Result<(Arc<ShellRuntime>, PtyReader)> {
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
    let mut command = if shell.command.is_empty() {
        CommandBuilder::new(client_shell)
    } else {
        let mut command = CommandBuilder::new(&shell.command[0]);
        command.args(&shell.command[1..]);
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

    let terminal = initial_terminal_state(profile.rows, profile.cols, workspace_name, shell_name);

    Ok((
        Arc::new(ShellRuntime {
            control: Mutex::new(()),
            master: Mutex::new(master),
            process: Mutex::new(ManagedProcess::Owned(child)),
            terminal: Arc::new(Mutex::new(terminal)),
            controller: Mutex::new(None),
            reader: Mutex::new(None),
        }),
        reader,
    ))
}

fn start_pty_reader(
    registry: Weak<Registry>,
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
            let mut pause_cancellation: Option<Arc<AtomicBool>> = None;
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
                            if !cancelled.load(Ordering::Acquire) {
                                let _ = acknowledge.send(());
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
                        paused = true;
                        pause_cancellation = Some(Arc::clone(&cancelled));
                        if acknowledge.send(()).is_err() || cancelled.load(Ordering::Acquire) {
                            paused = false;
                            pause_cancellation = None;
                        }
                        continue;
                    }
                    Ok(ReaderCommand::Resume) => continue,
                    Ok(ReaderCommand::Stop) | Err(mpsc::TryRecvError::Disconnected) => {
                        stopped = true;
                        break;
                    }
                    Err(mpsc::TryRecvError::Empty) => {}
                }
                match reader.read(&mut buffer) {
                    Ok(0) => break,
                    Ok(count) => {
                        let bytes = &buffer[..count];
                        let Some(registry) = registry.upgrade() else {
                            break;
                        };
                        let (transition, mut events, mut terminal) = loop {
                            let Ok(transition) = registry.transitions.lock() else {
                                return;
                            };
                            let Ok(events) = registry.events.state.lock() else {
                                return;
                            };
                            match reader_runtime.terminal.try_lock() {
                                Ok(terminal) => break (transition, events, terminal),
                                Err(TryLockError::WouldBlock) => {
                                    drop(events);
                                    drop(transition);
                                    thread::sleep(IO_RETRY_DELAY);
                                }
                                Err(TryLockError::Poisoned(_)) => return,
                            }
                        };
                        if EventLog::ensure_capacity(&events, 1).is_err() {
                            break;
                        }
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
                        EventLog::append_batch_locked(
                            &mut events,
                            vec![DaemonEventKind::OutputChanged {
                                workspace_id: shell.workspace_id.clone(),
                                shell_id: shell.id.clone(),
                                run_id: reader_run.id.clone(),
                                output_revision: revision,
                            }],
                        );
                        drop(terminal);
                        drop(events);
                        drop(transition);
                        registry.events.changed.notify_all();
                    }
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
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
            if stopped {
                return;
            }
            let code = reader_runtime
                .process
                .lock()
                .ok()
                .and_then(|mut process| process.try_wait_code().ok().flatten().flatten());
            if let Some(registry) = registry.upgrade()
                && let Err(error) =
                    registry.record_run_exit(&shell, &reader_run, &reader_runtime, code)
            {
                eprintln!("boomux: could not persist shell run exit: {error}");
            }
            let _ = reader_runtime
                .controller
                .lock()
                .map(|mut controller| controller.take());
        })?;
    *lock(&runtime.reader)? = Some(ReaderTask { commands, handle });
    Ok(())
}

fn tail_utf8(text: &str, max_bytes: usize) -> &str {
    let mut start = text.len().saturating_sub(max_bytes);
    while !text.is_char_boundary(start) {
        start += 1;
    }
    &text[start..]
}

struct AttachRequestOptions {
    takeover: bool,
    restart_exited: bool,
    profile: TerminalProfile,
    environment: Option<UnixEnvironment>,
}

fn handle_attach(
    mut stream: UnixStream,
    response_version: u32,
    registry: &Arc<Registry>,
    shell_id: &str,
    options: AttachRequestOptions,
) -> io::Result<()> {
    let AttachRequestOptions {
        takeover,
        restart_exited,
        profile,
        environment,
    } = options;
    if let Err(error) = validate_terminal_profile(&profile) {
        return send_response(
            &mut stream,
            response_version,
            error_response(ErrorCode::InvalidArgument, error.to_string()),
        );
    }
    if let Some(environment) = &environment
        && let Err(error) = validate_unix_environment(environment)
    {
        return send_response(
            &mut stream,
            response_version,
            error_response(ErrorCode::InvalidArgument, error.to_string()),
        );
    }
    let shell = match registry.shell(shell_id) {
        Ok(shell) => shell,
        Err(error) => {
            return send_response(
                &mut stream,
                response_version,
                error_response(error_code(&error), error.to_string()),
            );
        }
    };
    let mutation = lock(&registry.mutation_lock)?;
    if registry.stopping.load(Ordering::Acquire) {
        return send_response(
            &mut stream,
            response_version,
            error_response(ErrorCode::DaemonStopping, "Boomux daemon is stopping"),
        );
    }
    let token = Uuid::new_v4().to_string();
    let (output, receiver) = mpsc::sync_channel(CONTROLLER_QUEUE);
    let connection = stream.try_clone()?;
    let mut transition = restart_exited
        .then(|| lock(&registry.transitions))
        .transpose()?;
    if restart_exited {
        let old_runtime = match &*lock(&shell.lifecycle)? {
            ShellLifecycle::Exited { runtime, .. } => runtime.clone(),
            _ => None,
        };
        if let Some(runtime) = old_runtime {
            runtime.stop_reader()?;
        }
        let mut lifecycle = lock(&shell.lifecycle)?;
        if matches!(*lifecycle, ShellLifecycle::Exited { .. }) {
            *lifecycle = ShellLifecycle::Pending;
        }
    }
    let previous_run = lock(&shell.last_run)?.clone();
    let needs_start = matches!(*lock(&shell.lifecycle)?, ShellLifecycle::Pending);
    if needs_start && transition.is_none() {
        transition = Some(lock(&registry.transitions)?);
    }
    let persistence = needs_start
        .then(|| lock(&registry.persist_lock))
        .transpose()?;
    let mut events = needs_start
        .then(|| lock(&registry.events.state))
        .transpose()?;
    let published_pending = if needs_start {
        let published = registry.flush_pending_locked(
            transition.as_mut().expect("start transition is locked"),
            events.as_mut().expect("start event log is locked"),
        )?;
        EventLog::ensure_capacity(events.as_ref().expect("start event log is locked"), 1)?;
        published
    } else {
        false
    };
    let (runtime, terminal, startup_profile, running, started) = {
        let mut lifecycle = lock(&shell.lifecycle)?;
        let mut started = false;
        if !registry.contains_shell(&shell)? {
            return send_response(
                &mut stream,
                response_version,
                error_response(ErrorCode::NotFound, format!("shell not found: {shell_id}")),
            );
        }
        if matches!(*lifecycle, ShellLifecycle::Pending) {
            let workspace = registry.workspace(&shell.workspace_id)?;
            let workspace_name = lock(&workspace.name)?.clone();
            let shell_name = lock(&shell.name)?.clone();
            let generation = previous_run.as_ref().map_or(Ok(1), |run| {
                run.generation
                    .checked_add(1)
                    .ok_or_else(|| io::Error::other("shell run generation exhausted"))
            })?;
            let run = Arc::new(ShellRun::new(generation));
            let (runtime, reader) = match spawn_runtime(
                &shell,
                &run,
                &workspace_name,
                &shell_name,
                &profile,
                environment.as_ref(),
            ) {
                Ok(runtime) => runtime,
                Err(error) => {
                    return send_response(
                        &mut stream,
                        response_version,
                        error_response(
                            ErrorCode::ShellStartFailed,
                            format!("could not start shell: {error}"),
                        ),
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
            if let Err(error) = start_pty_reader(
                Arc::downgrade(registry),
                Arc::clone(&shell),
                run,
                runtime,
                reader,
                true,
            ) {
                drop(lifecycle);
                let cleanup = shell.kill();
                shell.reset_pending()?;
                return send_response(
                    &mut stream,
                    response_version,
                    error_response(
                        ErrorCode::ShellStartFailed,
                        cleanup.map_or_else(
                            |cleanup| {
                                format!(
                                    "could not start shell reader: {error}; process cleanup also failed: {cleanup}"
                                )
                            },
                            |()| format!("could not start shell reader: {error}"),
                        ),
                    ),
                );
            }
        }
        match &*lifecycle {
            ShellLifecycle::Running {
                profile, runtime, ..
            } => (
                Some(Arc::clone(runtime)),
                Arc::clone(&runtime.terminal),
                profile.clone(),
                true,
                started,
            ),
            ShellLifecycle::Exited {
                profile, terminal, ..
            } => (None, Arc::clone(terminal), profile.clone(), false, started),
            ShellLifecycle::Pending => unreachable!(),
            ShellLifecycle::Closed => {
                return send_response(
                    &mut stream,
                    response_version,
                    error_response(ErrorCode::NotFound, format!("shell not found: {shell_id}")),
                );
            }
        }
    };
    if started && let Err(error) = registry.persist_unlocked().map_err(persistence_error) {
        let cleanup = shell.kill();
        shell.reset_pending()?;
        return send_response(
            &mut stream,
            response_version,
            error_response(
                ErrorCode::PersistenceFailed,
                cleanup.map_or_else(
                    |cleanup| {
                        format!(
                            "could not persist started shell: {error}; process cleanup also failed: {cleanup}"
                        )
                    },
                    |()| format!("could not persist started shell: {error}"),
                ),
            ),
        );
    }
    if started {
        let run = shell
            .snapshot()?
            .run
            .ok_or_else(|| io::Error::other("started shell has no run identity"))?;
        let events = events.as_mut().expect("start event log is locked");
        EventLog::append_batch_locked(
            events,
            vec![DaemonEventKind::RunStarted {
                workspace_id: shell.workspace_id.clone(),
                shell_id: shell.id.clone(),
                run,
            }],
        );
    }
    drop(events);
    drop(persistence);
    drop(transition);
    if started || published_pending {
        registry.events.changed.notify_all();
    }
    if started {
        runtime
            .as_ref()
            .expect("started shell has a runtime")
            .resume_reader()?;
    }
    let warning = term_mismatch_warning(startup_profile.term.as_deref(), profile.term.as_deref());
    if !running {
        lock(&terminal)?.resize(profile.rows, profile.cols);
        stream.set_write_timeout(Some(HANDSHAKE_TIMEOUT))?;
        send_response(
            &mut stream,
            response_version,
            Response::Attached {
                token,
                reconstruction: lock(&terminal)?.reconstruction(),
                warning,
            },
        )?;
        stream.set_write_timeout(None)?;
        return AttachFrame::Detached.write_to(&mut stream);
    }
    drop(mutation);
    let runtime = runtime.expect("running shell has a runtime");
    let control = lock(&runtime.control)?;
    if !registry.contains_shell(&shell)? {
        return send_response(
            &mut stream,
            response_version,
            error_response(ErrorCode::NotFound, format!("shell not found: {shell_id}")),
        );
    }
    {
        let controller = lock(&runtime.controller)?;
        if controller.is_some() && !takeover {
            return send_response(
                &mut stream,
                response_version,
                error_response(
                    ErrorCode::Busy,
                    "shell already has an active controller; use takeover",
                ),
            );
        }
    }
    lock(&runtime.master)?.resize(profile_size(&profile))?;
    update_runtime_dimensions(&shell, &runtime, profile_size(&profile))?;
    lock(&terminal)?.resize(profile.rows, profile.cols);
    // Keep terminal state locked until the controller is installed so the
    // reconstruction ends exactly where live delivery begins.
    let terminal = lock(&terminal)?;
    stream.set_write_timeout(Some(HANDSHAKE_TIMEOUT))?;
    send_response(
        &mut stream,
        response_version,
        Response::Attached {
            token: token.clone(),
            reconstruction: terminal.reconstruction(),
            warning,
        },
    )?;
    stream.set_write_timeout(None)?;
    let lifecycle = lock(&shell.lifecycle)?;
    let still_running = registry.contains_shell(&shell)?
        && matches!(
            &*lifecycle,
            ShellLifecycle::Running {
                runtime: current,
                ..
            } if Arc::ptr_eq(current, &runtime)
        );
    if !still_running {
        return AttachFrame::Detached.write_to(&mut stream);
    }
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
    let mut output_stream = stream.try_clone()?;
    let output_runtime = Arc::clone(&runtime);
    let output_token = token.clone();
    thread::spawn(move || {
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
        if reconnect {
            let _ = output_stream.shutdown(std::net::Shutdown::Both);
        } else {
            let _ = AttachFrame::Detached.write_to(&mut output_stream);
        }
        let _ = output_runtime.release_controller(&output_token);
    });
    drop(lifecycle);
    drop(terminal);
    drop(control);

    loop {
        let frame = match AttachFrame::read_from(&mut stream) {
            Ok(frame) => frame,
            Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => break,
            Err(error) => {
                runtime.release_controller(&token)?;
                return Err(error);
            }
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
            break;
        }
        if let AttachFrame::Input(bytes) = frame {
            if !write_controller_input(&runtime, &token, &bytes)? {
                break;
            }
            continue;
        }
        let control = lock(&runtime.control)?;
        let authorized = lock(&runtime.controller)?
            .as_ref()
            .is_some_and(|controller| controller.token == token);
        if !authorized {
            break;
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
                    update_runtime_dimensions(&shell, &runtime, size)?;
                    Ok(())
                }
            }
            AttachFrame::Detached => {
                drop(control);
                break;
            }
            AttachFrame::Output(_) | AttachFrame::Reconnect | AttachFrame::ReconnectAck => {
                Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "client sent a daemon-only attach frame",
                ))
            }
        };
        if let Err(error) = result {
            runtime.release_controller(&token)?;
            return Err(error);
        }
    }
    runtime.release_controller(&token)
}

fn write_controller_input(runtime: &ShellRuntime, token: &str, bytes: &[u8]) -> io::Result<bool> {
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

fn validate_id(kind: &str, id: &str) -> io::Result<()> {
    Uuid::parse_str(id).map(|_| ()).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("persisted {kind} ID is invalid: {id}"),
        )
    })
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
    use std::sync::Barrier;

    use crate::protocol::{AgentAuthority, AgentState};

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
    ) -> (Registry, Arc<RecordingNotificationSink>) {
        let sink = Arc::new(RecordingNotificationSink::default());
        let registry = Registry {
            notification_settings: NotificationDeliverySettings {
                desktop: settings,
                ..Default::default()
            },
            notification_sink: sink.clone(),
            ..Registry::default()
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
        let terminal = initial_terminal_state(24, 80, "project", "build");

        assert!(terminal.plain_text().contains("Boomux: project/build"));
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

    fn running_shell(registry: &Registry) -> (WorkspaceSnapshot, Arc<Shell>, Arc<ShellRuntime>) {
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
        let (runtime, _reader) =
            spawn_runtime(&shell, &run, "agents", "agent-shell", &profile(), None).unwrap();
        *lock(&shell.last_run).unwrap() = Some(run.persisted(profile()).unwrap());
        *lock(&shell.lifecycle).unwrap() = ShellLifecycle::Running {
            profile: profile(),
            run,
            runtime: Arc::clone(&runtime),
        };
        (workspace, shell, runtime)
    }

    #[test]
    fn empty_registry_has_empty_snapshot() {
        assert!(
            Registry::default()
                .snapshot()
                .unwrap()
                .workspaces
                .is_empty()
        );
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
        let registry = Registry::default();
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
        let registry = Registry::default();
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
        assert_eq!(error_code(&error), ErrorCode::CursorExpired);
    }

    #[test]
    fn rendered_output_tail_preserves_utf8_boundaries() {
        assert_eq!(tail_utf8("one-λ", 2), "λ");
        assert_eq!(tail_utf8("one-λ", 3), "-λ");
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
        let registry = Registry::default();
        let (_workspace, shell, _runtime) = running_shell(&registry);

        let deadline = Instant::now() + Duration::from_secs(1);
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

    #[test]
    fn failed_persistence_rolls_back_registry_mutation() {
        let directory = env::temp_dir().join(format!("boomux-rollback-{}", Uuid::new_v4()));
        let state_directory = directory.join("state");
        let registry = Registry::restore(
            StateStore::at(state_directory.join("state.json")),
            false,
            None,
        )
        .unwrap();
        fs::remove_dir(&state_directory).unwrap();
        fs::write(&state_directory, b"not a directory").unwrap();

        let result = registry.dispatch(Request::CreateWorkspace {
            name: "rolled-back".into(),
            shells: Vec::new(),
        });

        assert!(result.is_err());
        assert!(registry.snapshot().unwrap().workspaces.is_empty());
        assert!(lock(&registry.events.state).unwrap().events.is_empty());
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn coordinated_workspace_batch_is_included_in_baseline_cursor() {
        let registry = Registry::default();
        let response = registry
            .dispatch(Request::CreateWorkspace {
                name: "coordinated".into(),
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
        let registry = Registry::default();
        let Response::Workspace { workspace } = registry
            .dispatch(Request::CreateWorkspace {
                name: "launchers".into(),
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
            .snapshot(&registry)
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
        let registry = Registry::default();
        let (workspace, shell, _runtime) = running_shell(&registry);
        let run_id = shell.snapshot().unwrap().run.unwrap().id;

        let wrong_run = registry.dispatch(Request::RegisterAgent {
            shell_id: shell.id.clone(),
            run_id: Uuid::new_v4().to_string(),
            spec: agent_spec(AgentState::Working),
        });
        assert_eq!(error_code(&wrong_run.unwrap_err()), ErrorCode::RunChanged);

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
            error_code(
                &registry
                    .dispatch(Request::ReportAgent {
                        agent_id: agent.id.clone(),
                        run_id: Uuid::new_v4().to_string(),
                        report: agent_spec(AgentState::Idle).report,
                    })
                    .unwrap_err()
            ),
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
        let registry = Registry::default();
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
        let registry = Arc::new(Registry::default());
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
        let registry = Registry::default();
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
                .kind(),
            io::ErrorKind::AlreadyExists
        );
        shell.kill().unwrap();
        registry.close_workspace(&workspace.id).unwrap();
    }

    #[test]
    fn ensure_agent_requires_external_id_and_distinguishes_runs() {
        let registry = Registry::default();
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
                .kind(),
            io::ErrorKind::InvalidInput
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
        let registry = Registry::default();
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
        let registry = Registry::default();
        let (workspace, shell, _runtime) = running_shell(&registry);
        let run_id = shell.snapshot().unwrap().run.unwrap().id;
        let agent = registry
            .register_agent(&shell.id, &run_id, agent_spec(AgentState::Working))
            .unwrap();

        let result: io::Result<()> = registry.durable_mutation(|| {
            registry.report_agent(&agent.id, &run_id, agent_spec(AgentState::Blocked).report)?;
            Err(io::Error::other("force rollback"))
        });

        assert!(result.is_err());
        let restored = registry.agent(&agent.id).unwrap().snapshot().unwrap();
        assert_eq!(restored.observation.revision, 1);
        assert_eq!(restored.observation.state, AgentState::Working);
        assert!(restored.attention.is_none());
        shell.kill().unwrap();
        registry.close_workspace(&workspace.id).unwrap();
    }

    #[test]
    fn agent_attention_is_raised_preserved_and_superseded_by_completion() {
        let registry = Registry::default();
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
        let mut registry = Registry::restore(
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
        let registry = Registry::restore(
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

        assert_eq!(error_code(&error), ErrorCode::PersistenceFailed);
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
        let registry = Registry::default();
        let (workspace, shell, _runtime) = running_shell(&registry);
        let run_id = shell.snapshot().unwrap().run.unwrap().id;
        let agent = registry
            .register_agent(&shell.id, &run_id, agent_spec(AgentState::Blocked))
            .unwrap();
        let revision = agent.observation.revision;

        let mismatch = registry.acknowledge_agent_attention(&agent.id, revision + 1);
        assert_eq!(error_code(&mismatch.unwrap_err()), ErrorCode::RevisionAhead);
        assert!(
            registry
                .agent(&agent.id)
                .unwrap()
                .snapshot()
                .unwrap()
                .attention
                .is_some()
        );

        let result: io::Result<()> = registry.durable_mutation(|| {
            registry.acknowledge_agent_attention(&agent.id, revision)?;
            Err(io::Error::other("force rollback"))
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
        let registry = Arc::new(Registry::default());
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
            error_code(&registry.wait_agent(&agent.id, 2, 0).unwrap_err()),
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
        let registry = Arc::new(Registry::default());
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

        registry.stopping.store(true, Ordering::Release);
        registry.events.notify();

        assert_eq!(
            error_code(&waiter.join().unwrap().unwrap_err()),
            ErrorCode::DaemonStopping
        );
        registry.stopping.store(false, Ordering::Release);
        shell.kill().unwrap();
        registry.close_workspace(&workspace.id).unwrap();
    }

    #[test]
    fn agent_instances_restore_from_daemon_persistence() {
        let directory = env::temp_dir().join(format!("boomux-agent-restore-{}", Uuid::new_v4()));
        let path = directory.join("state/state.json");
        let registry = Registry::restore(StateStore::at(path.clone()), false, None).unwrap();
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

        let restored = Registry::restore(StateStore::at(path), false, None).unwrap();
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
            name: "workspace".into(),
            shells: Vec::new(),
            launchers: Vec::new(),
            agents: vec![agent.clone()],
        };
        let response = Response::Events {
            stream_id: "stream".into(),
            cursor: EventCursor {
                stream_id: "stream".into(),
                event_id: 2,
            },
            snapshot: Some(Snapshot {
                workspaces: vec![workspace],
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
            name: "workspace".into(),
            shells: Vec::new(),
            launchers: Vec::new(),
            agents: vec![agent.clone()],
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
                    name: "workspace".into(),
                    shells: Vec::new(),
                    launchers: Vec::new(),
                    agents: vec![agent.clone()],
                }],
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
        )
        .unwrap();
        *lock(&shell.last_run).unwrap() = Some(run.persisted(terminal_profile.clone()).unwrap());
        *lock(&shell.lifecycle).unwrap() = ShellLifecycle::Running {
            profile: terminal_profile,
            run: Arc::clone(&run),
            runtime: Arc::clone(&runtime),
        };
        let registry = Arc::new(Registry::default());
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
        let registry = Arc::new(Registry::default());
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
        });
        let task = ReaderTask { commands, handle };

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
    fn empty_workspace_snapshot_has_no_cwd_and_no_shells() {
        let registry = Registry::default();

        let workspace = registry
            .create_workspace("empty".into(), Vec::new())
            .unwrap();
        let value = serde_json::to_value(&workspace).unwrap();

        assert!(workspace.shells.is_empty());
        assert!(value.get("cwd").is_none());
    }

    #[test]
    fn concurrent_duplicate_workspace_names_publish_only_once() {
        let registry = Arc::new(Registry::default());
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
        let registry = Registry::default();
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
        let registry = Registry::default();
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

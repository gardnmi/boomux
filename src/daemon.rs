use std::collections::{HashMap, HashSet};
use std::env;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::fs::MetadataExt;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::mpsc::{self, SyncSender, TrySendError};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread;
use std::time::{Duration, Instant};

use portable_pty::{Child, CommandBuilder, MasterPty, PtySize, native_pty_system};
use uuid::Uuid;

use crate::client;
use crate::fd_transfer::send_descriptor;
use crate::handoff;
use crate::protocol::{
    self, AttachFrame, Envelope, Request, Response, ShellSnapshot, ShellSpec, ShellStatus,
    Snapshot, TerminalProfile, WorkspaceSnapshot,
};
use crate::state_store::{PersistedShell, PersistedState, PersistedWorkspace, StateStore};
use crate::terminal_state::TerminalState;

const CONTROLLER_QUEUE: usize = 64;
const SHUTDOWN_GRACE: Duration = Duration::from_millis(50);
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(2);
const RESTART_TIMEOUT: Duration = Duration::from_secs(10);
const IO_RETRY_DELAY: Duration = Duration::from_millis(2);
const MAX_TERMINAL_ENV_VALUE: usize = 256;
const MAX_TERMINAL_ROWS: u16 = 1_000;
const MAX_TERMINAL_COLS: u16 = 1_000;
const MAX_TERMINAL_CELLS: usize = 1_000_000;
const MAX_SHELL_READ_BYTES: usize = 1024 * 1024;
const TRANSITION_IDLE: u8 = 0;
const TRANSITION_RESTART: u8 = 1;
const TRANSITION_SHUTDOWN: u8 = 2;

pub fn receive_handoff(channel: i32) -> io::Result<()> {
    match handoff::receive_bootstrap(channel)? {
        handoff::Bootstrap::Aborted => Ok(()),
        handoff::Bootstrap::Committed {
            mut channel,
            listener,
            runtime_lock,
            state_lock,
            runtimes,
        } => {
            let store = StateStore::from_transferred_lock(state_lock)?;
            let socket_path = client::socket_path()?;
            run_daemon(
                listener,
                File::from(runtime_lock),
                SocketCleanup::disarmed(socket_path),
                store,
                runtimes,
                Some(&mut channel),
            )
        }
    }
}

pub fn run() -> io::Result<()> {
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
        Vec::new(),
        None,
    )
}

fn run_daemon(
    listener: UnixListener,
    daemon_lock: File,
    mut socket_cleanup: SocketCleanup,
    store: StateStore,
    transferred_runtimes: Vec<handoff::TransferredRuntime>,
    committed: Option<&mut UnixStream>,
) -> io::Result<()> {
    let registry = Arc::new(Registry::restore(store)?);
    let gated_readers = registry.import_runtimes(transferred_runtimes)?;
    if let Some(channel) = committed {
        channel.write_all(&[handoff::PREPARED])?;
        let mut decision = [0];
        channel.read_exact(&mut decision)?;
        match decision[0] {
            handoff::ABORT => return Ok(()),
            handoff::FINALIZE => {
                socket_cleanup.arm();
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
    let mut handlers = Vec::new();
    let mut handed_off = false;

    while !shutdown.load(Ordering::Acquire) {
        match restart_receiver.try_recv() {
            Ok(request) => {
                let result = launch_replacement(&listener, &daemon_lock, &registry);
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
) -> io::Result<()> {
    let _mutation = lock(&registry.mutation_lock)?;
    registry.ensure_running()?;
    registry.stopping.store(true, Ordering::Release);
    let mut paused = Vec::new();
    let result = (|| {
        registry.quiesce_controllers()?;
        let state = lock(&registry.state)?;
        let mut transfers = Vec::new();
        for shell in state.shells.values() {
            let (profile, runtime) = match &*lock(&shell.lifecycle)? {
                ShellLifecycle::Pending => continue,
                ShellLifecycle::Running { profile, runtime } => {
                    (profile.clone(), Arc::clone(runtime))
                }
                ShellLifecycle::Exited { .. } => {
                    return Err(io::Error::new(
                        io::ErrorKind::Unsupported,
                        "exited shell runtime transfer is not supported",
                    ));
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
                    profile,
                    pid,
                },
                runtime,
                pidfd,
                reconstruction,
            });
        }
        drop(state);
        let state_lock = registry.state_lock_descriptor()?;
        launch_replacement_process(
            listener.as_fd(),
            daemon_lock.as_fd(),
            state_lock,
            &transfers,
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
) -> io::Result<()> {
    let (mut channel, child_channel) = UnixStream::pair()?;
    let child_channel_fd = child_channel.as_raw_fd();
    let mut command = Command::new(env::current_exe()?);
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
    if request.version != protocol::PROTOCOL_VERSION {
        return send_response(
            &mut stream,
            Response::Error {
                message: format!(
                    "protocol version {} is unsupported; expected {}",
                    request.version,
                    protocol::PROTOCOL_VERSION
                ),
            },
        );
    }

    if let Request::Attach {
        shell_id,
        takeover,
        profile,
    } = request.message
    {
        return handle_attach(stream, &registry, &shell_id, takeover, profile);
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
                Response::Error {
                    message: "another daemon transition is already in progress".into(),
                },
            );
        }
        return match registry.shutdown() {
            Ok(()) => {
                shutdown.store(true, Ordering::Release);
                send_response(&mut stream, Response::Ok)
            }
            Err(error) => {
                transition.store(TRANSITION_IDLE, Ordering::Release);
                send_response(
                    &mut stream,
                    Response::Error {
                        message: format!("could not stop Boomux daemon: {error}"),
                    },
                )
            }
        };
    }
    if matches!(request.message, Request::Restart) {
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
                Response::Error {
                    message: "daemon restart is already in progress".into(),
                },
            );
        }
        let (reply, response) = mpsc::sync_channel(1);
        if restart_sender.send(RestartRequest { reply }).is_err() {
            transition.store(TRANSITION_IDLE, Ordering::Release);
            return Err(io::Error::other("daemon restart coordinator stopped"));
        }
        return match response.recv_timeout(RESTART_TIMEOUT) {
            Ok(Ok(())) => send_response(&mut stream, Response::Ok),
            Ok(Err(error)) => send_response(
                &mut stream,
                Response::Error {
                    message: error.to_string(),
                },
            ),
            Err(error) => send_response(
                &mut stream,
                Response::Error {
                    message: format!("daemon restart timed out: {error}"),
                },
            ),
        };
    }

    let response = registry
        .dispatch(request.message)
        .unwrap_or_else(|error| Response::Error {
            message: error.to_string(),
        });
    send_response(&mut stream, response)
}

fn send_response(stream: &mut UnixStream, response: Response) -> io::Result<()> {
    protocol::write_message(stream, &Envelope::new(response))
}

struct Registry {
    state: Mutex<RegistryState>,
    store: Option<StateStore>,
    mutation_lock: Mutex<()>,
    persist_lock: Mutex<()>,
    stopping: AtomicBool,
}

#[derive(Default)]
struct RegistryState {
    workspaces: HashMap<String, Arc<Workspace>>,
    shells: HashMap<String, Arc<Shell>>,
}

struct RegistryBackup {
    workspaces: HashMap<String, Arc<Workspace>>,
    shells: HashMap<String, Arc<Shell>>,
    workspace_names: HashMap<String, String>,
    workspace_shell_ids: HashMap<String, Vec<String>>,
    shell_names: HashMap<String, String>,
    last_profiles: HashMap<String, Option<TerminalProfile>>,
}

impl Default for Registry {
    fn default() -> Self {
        Self {
            state: Mutex::new(RegistryState::default()),
            store: None,
            mutation_lock: Mutex::new(()),
            persist_lock: Mutex::new(()),
            stopping: AtomicBool::new(false),
        }
    }
}

struct Workspace {
    id: String,
    name: Mutex<String>,
    shell_ids: Mutex<Vec<String>>,
}

struct Shell {
    id: String,
    workspace_id: String,
    name: Mutex<String>,
    cwd: PathBuf,
    command: Vec<String>,
    last_profile: Mutex<Option<TerminalProfile>>,
    lifecycle: Mutex<ShellLifecycle>,
}

enum ShellLifecycle {
    Pending,
    Running {
        profile: TerminalProfile,
        runtime: Arc<ShellRuntime>,
    },
    Exited {
        code: Option<u32>,
        profile: TerminalProfile,
        runtime: Arc<ShellRuntime>,
    },
    Closed,
}

struct ShellRuntime {
    control: Mutex<()>,
    master: Mutex<PtyMaster>,
    process: Mutex<ManagedProcess>,
    terminal: Mutex<TerminalState>,
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

impl Registry {
    fn dispatch(&self, request: Request) -> io::Result<Response> {
        match request {
            Request::Ping => Ok(Response::Pong),
            Request::Restart => unreachable!("restart is handled before dispatch"),
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
            Request::CreateWorkspace { name, shells } => self.durable_mutation(|| {
                Ok(Response::Workspace {
                    workspace: self.create_workspace(name, shells)?,
                })
            }),
            Request::CreateShell {
                workspace_id,
                shell,
            } => self.durable_mutation(|| {
                Ok(Response::Shell {
                    shell: match workspace_id {
                        Some(workspace_id) => self.create_shell(&workspace_id, shell)?,
                        None => self.create_shell_with_workspace(shell)?,
                    },
                })
            }),
            Request::ReadShell {
                shell_id,
                max_bytes,
            } => Ok(Response::Output {
                bytes: self.read_shell(&shell_id, max_bytes)?,
            }),
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
                Ok(Response::Ok)
            }),
            Request::RenameShell { shell_id, name } => self.durable_mutation(|| {
                self.rename_shell(&shell_id, name)?;
                Ok(Response::Ok)
            }),
            Request::CloseWorkspace { workspace_id } => {
                self.close_workspace(&workspace_id)?;
                Ok(Response::Ok)
            }
            Request::CloseShell { shell_id } => {
                self.close_shell(&shell_id)?;
                Ok(Response::Ok)
            }
            Request::Attach { .. } => unreachable!("attach is handled before dispatch"),
        }
    }

    fn restore(store: StateStore) -> io::Result<Self> {
        let persisted = store.load()?.unwrap_or_default();
        let mut state = RegistryState::default();
        let mut workspace_names = HashSet::new();
        for saved_workspace in persisted.workspaces {
            validate_id("workspace", &saved_workspace.id)?;
            validate_name(&saved_workspace.name)?;
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
            for saved_shell in saved_workspace.shells {
                validate_id("shell", &saved_shell.id)?;
                validate_name(&saved_shell.name)?;
                validate_persisted_cwd(&saved_shell.cwd)?;
                if let Some(profile) = &saved_shell.last_profile {
                    validate_terminal_profile(profile)?;
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
                    last_profile: Mutex::new(saved_shell.last_profile),
                    lifecycle: Mutex::new(ShellLifecycle::Pending),
                });
                shell_ids.push(shell.id.clone());
                state.shells.insert(shell.id.clone(), shell);
            }
            let workspace = Arc::new(Workspace {
                id: saved_workspace.id.clone(),
                name: Mutex::new(saved_workspace.name),
                shell_ids: Mutex::new(shell_ids),
            });
            state.workspaces.insert(saved_workspace.id, workspace);
        }
        Ok(Self {
            state: Mutex::new(state),
            store: Some(store),
            mutation_lock: Mutex::new(()),
            persist_lock: Mutex::new(()),
            stopping: AtomicBool::new(false),
        })
    }

    fn import_runtimes(
        &self,
        transferred: Vec<handoff::TransferredRuntime>,
    ) -> io::Result<Vec<Arc<ShellRuntime>>> {
        let state = lock(&self.state)?;
        let mut prepared = Vec::with_capacity(transferred.len());
        for transferred in transferred {
            let manifest = transferred.manifest;
            validate_terminal_profile(&manifest.profile)?;
            let shell = state
                .shells
                .get(&manifest.shell_id)
                .cloned()
                .ok_or_else(|| not_found("persisted shell", &manifest.shell_id))?;
            if !matches!(*lock(&shell.lifecycle)?, ShellLifecycle::Pending) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "transferred shell is not pending in restored metadata",
                ));
            }
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
                terminal: Mutex::new(terminal),
                controller: Mutex::new(None),
                reader: Mutex::new(None),
            });
            prepared.push((shell, manifest.profile, runtime, reader));
        }
        drop(state);

        let mut readers = Vec::with_capacity(prepared.len());
        for (shell, profile, runtime, reader) in prepared {
            *lock(&shell.last_profile)? = Some(profile.clone());
            *lock(&shell.lifecycle)? = ShellLifecycle::Running {
                profile,
                runtime: Arc::clone(&runtime),
            };
            start_pty_reader(shell, Arc::clone(&runtime), reader, true)?;
            readers.push(runtime);
        }
        Ok(readers)
    }

    fn durable_mutation<T>(&self, operation: impl FnOnce() -> io::Result<T>) -> io::Result<T> {
        let _mutation = lock(&self.mutation_lock)?;
        self.ensure_running()?;
        let backup = self.backup()?;
        match operation() {
            Ok(value) => match self.persist() {
                Ok(()) => Ok(value),
                Err(error) => {
                    self.restore_backup(backup)?;
                    Err(error)
                }
            },
            Err(error) => {
                self.restore_backup(backup)?;
                Err(error)
            }
        }
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
        for workspace in state.workspaces.values() {
            workspace_names.insert(workspace.id.clone(), lock(&workspace.name)?.clone());
            workspace_shell_ids.insert(workspace.id.clone(), lock(&workspace.shell_ids)?.clone());
        }
        let mut shell_names = HashMap::new();
        let mut last_profiles = HashMap::new();
        for shell in state.shells.values() {
            shell_names.insert(shell.id.clone(), lock(&shell.name)?.clone());
            last_profiles.insert(shell.id.clone(), lock(&shell.last_profile)?.clone());
        }
        Ok(RegistryBackup {
            workspaces: state.workspaces.clone(),
            shells: state.shells.clone(),
            workspace_names,
            workspace_shell_ids,
            shell_names,
            last_profiles,
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
        }
        for shell in backup.shells.values() {
            if let Some(name) = backup.shell_names.get(&shell.id) {
                *lock(&shell.name)? = name.clone();
            }
            if let Some(profile) = backup.last_profiles.get(&shell.id) {
                *lock(&shell.last_profile)? = profile.clone();
            }
        }
        state.workspaces = backup.workspaces;
        state.shells = backup.shells;
        Ok(())
    }

    fn persist(&self) -> io::Result<()> {
        let Some(store) = &self.store else {
            return Ok(());
        };
        let _persist = lock(&self.persist_lock)?;
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
                    last_profile: lock(&shell.last_profile)?.clone(),
                });
            }
            saved.workspaces.push(PersistedWorkspace {
                id: workspace.id.clone(),
                name: lock(&workspace.name)?.clone(),
                shells,
            });
        }
        drop(state);
        store.save(&saved)
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
        self.stopping.store(true, Ordering::Release);
        let shells = {
            let state = lock(&self.state)?;
            state.shells.values().cloned().collect::<Vec<_>>()
        };
        let mut killed: Vec<Arc<Shell>> = Vec::new();
        for shell in &shells {
            if let Err(error) = shell.kill() {
                for shell in killed {
                    shell.reset_pending()?;
                }
                self.stopping.store(false, Ordering::Release);
                return Err(error);
            }
            killed.push(Arc::clone(shell));
        }
        let mut state = lock(&self.state)?;
        state.workspaces.clear();
        state.shells.clear();
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
        let runtime = match &*lifecycle {
            ShellLifecycle::Pending => return Ok(Vec::new()),
            ShellLifecycle::Running { runtime, .. } | ShellLifecycle::Exited { runtime, .. } => {
                Arc::clone(runtime)
            }
            ShellLifecycle::Closed => return Err(not_found("shell", shell_id)),
        };
        drop(lifecycle);
        let text = lock(&runtime.terminal)?.plain_text();
        Ok(tail_utf8(&text, max_bytes.min(MAX_SHELL_READ_BYTES))
            .as_bytes()
            .to_vec())
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
        shell.kill()?;
        self.remove_shell(shell_id)?;
        if let Err(error) = self.persist() {
            self.restore_backup(backup)?;
            shell.reset_pending()?;
            return Err(error);
        }
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
        let mut killed: Vec<Arc<Shell>> = Vec::new();
        for shell in &shells {
            if let Err(error) = shell.kill() {
                for shell in killed {
                    shell.reset_pending()?;
                }
                return Err(error);
            }
            killed.push(Arc::clone(shell));
        }
        self.remove_workspace(workspace_id)?;
        if let Err(error) = self.persist() {
            self.restore_backup(backup)?;
            for shell in shells {
                shell.reset_pending()?;
            }
            return Err(error);
        }
        Ok(())
    }
}

impl Workspace {
    fn snapshot(&self, registry: &Registry) -> io::Result<WorkspaceSnapshot> {
        let shells = {
            let state = lock(&registry.state)?;
            let ids = lock(&self.shell_ids)?;
            ids.iter()
                .filter_map(|id| state.shells.get(id).cloned())
                .collect::<Vec<_>>()
        };
        let shells = shells
            .iter()
            .map(|shell| shell.snapshot())
            .collect::<io::Result<_>>()?;
        Ok(WorkspaceSnapshot {
            id: self.id.clone(),
            name: lock(&self.name)?.clone(),
            shells,
        })
    }
}

impl Shell {
    fn snapshot(&self) -> io::Result<ShellSnapshot> {
        Ok(ShellSnapshot {
            id: self.id.clone(),
            workspace_id: self.workspace_id.clone(),
            name: lock(&self.name)?.clone(),
            cwd: self.cwd.clone(),
            status: match &*lock(&self.lifecycle)? {
                ShellLifecycle::Pending => ShellStatus::Pending,
                ShellLifecycle::Running { .. } => ShellStatus::Running,
                ShellLifecycle::Exited { code, .. } => ShellStatus::Exited { code: *code },
                ShellLifecycle::Closed => return Err(not_found("shell", &self.id)),
            },
        })
    }

    fn kill(&self) -> io::Result<()> {
        let runtime = {
            let mut lifecycle = lock(&self.lifecycle)?;
            match &*lifecycle {
                ShellLifecycle::Pending => {
                    *lifecycle = ShellLifecycle::Closed;
                    return Ok(());
                }
                ShellLifecycle::Running { runtime, .. }
                | ShellLifecycle::Exited { runtime, .. } => Arc::clone(runtime),
                ShellLifecycle::Closed => return Ok(()),
            }
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
            let result = match (kill_result, wait_result) {
                (_, Ok(())) => Ok(()),
                (Err(error), Err(_)) => Err(error),
                (Ok(()), Err(error)) => Err(error),
            };
            if let Some(session_id) = child_pid {
                wait_for_session_descendants(session_id)?;
            }
            result
        })();
        if let Err(error) = result {
            runtime.resume_reader()?;
            return Err(error);
        }

        runtime.stop_reader()?;
        let mut lifecycle = lock(&self.lifecycle)?;
        let current_runtime = match &*lifecycle {
            ShellLifecycle::Running { runtime, .. } | ShellLifecycle::Exited { runtime, .. } => {
                Some(runtime)
            }
            ShellLifecycle::Pending | ShellLifecycle::Closed => None,
        };
        if current_runtime.is_some_and(|current| Arc::ptr_eq(current, &runtime)) {
            *lifecycle = ShellLifecycle::Closed;
        }
        Ok(())
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
        last_profile: Mutex::new(None),
        lifecycle: Mutex::new(ShellLifecycle::Pending),
    }))
}

fn spawn_runtime(
    shell: &Arc<Shell>,
    workspace_name: &str,
    shell_name: &str,
    profile: &TerminalProfile,
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

    let mut command = if shell.command.is_empty() {
        CommandBuilder::new(env::var_os("SHELL").unwrap_or_else(|| "/bin/sh".into()))
    } else {
        let mut command = CommandBuilder::new(&shell.command[0]);
        command.args(&shell.command[1..]);
        command
    };
    command.cwd(&shell.cwd);
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
    let child = pty.slave.spawn_command(command).map_err(io::Error::other)?;
    drop(pty.slave);
    drop(pty.master);

    Ok((
        Arc::new(ShellRuntime {
            control: Mutex::new(()),
            master: Mutex::new(master),
            process: Mutex::new(ManagedProcess::Owned(child)),
            terminal: Mutex::new(TerminalState::new(profile.rows, profile.cols)),
            controller: Mutex::new(None),
            reader: Mutex::new(None),
        }),
        reader,
    ))
}

fn start_pty_reader(
    shell: Arc<Shell>,
    runtime: Arc<ShellRuntime>,
    mut reader: PtyReader,
    start_paused: bool,
) -> io::Result<()> {
    let (commands, command_receiver) = mpsc::channel();
    let reader_runtime = Arc::clone(&runtime);
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
                        if let Ok(mut terminal) = reader_runtime.terminal.lock() {
                            terminal.process(bytes);
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
                        }
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
            if let Ok(mut lifecycle) = shell.lifecycle.lock()
                && let ShellLifecycle::Running {
                    profile,
                    runtime: current,
                } = &*lifecycle
                && Arc::ptr_eq(current, &reader_runtime)
            {
                *lifecycle = ShellLifecycle::Exited {
                    code,
                    profile: profile.clone(),
                    runtime: Arc::clone(&reader_runtime),
                };
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

fn handle_attach(
    mut stream: UnixStream,
    registry: &Registry,
    shell_id: &str,
    takeover: bool,
    profile: TerminalProfile,
) -> io::Result<()> {
    if let Err(error) = validate_terminal_profile(&profile) {
        return send_response(
            &mut stream,
            Response::Error {
                message: error.to_string(),
            },
        );
    }
    let shell = match registry.shell(shell_id) {
        Ok(shell) => shell,
        Err(error) => {
            return send_response(
                &mut stream,
                Response::Error {
                    message: error.to_string(),
                },
            );
        }
    };
    let mutation = lock(&registry.mutation_lock)?;
    if registry.stopping.load(Ordering::Acquire) {
        return send_response(
            &mut stream,
            Response::Error {
                message: "Boomux daemon is stopping".into(),
            },
        );
    }
    let token = Uuid::new_v4().to_string();
    let (output, receiver) = mpsc::sync_channel(CONTROLLER_QUEUE);
    let connection = stream.try_clone()?;
    let previous_profile = lock(&shell.last_profile)?.clone();
    let (runtime, startup_profile, running, started) = {
        let mut lifecycle = lock(&shell.lifecycle)?;
        let mut started = false;
        if !registry.contains_shell(&shell)? {
            return send_response(
                &mut stream,
                Response::Error {
                    message: format!("shell not found: {shell_id}"),
                },
            );
        }
        if matches!(*lifecycle, ShellLifecycle::Pending) {
            let workspace = registry.workspace(&shell.workspace_id)?;
            let workspace_name = lock(&workspace.name)?.clone();
            let shell_name = lock(&shell.name)?.clone();
            let (runtime, reader) =
                match spawn_runtime(&shell, &workspace_name, &shell_name, &profile) {
                    Ok(runtime) => runtime,
                    Err(error) => {
                        return send_response(
                            &mut stream,
                            Response::Error {
                                message: format!("could not start shell: {error}"),
                            },
                        );
                    }
                };
            *lifecycle = ShellLifecycle::Running {
                profile: profile.clone(),
                runtime: Arc::clone(&runtime),
            };
            *lock(&shell.last_profile)? = Some(profile.clone());
            started = true;
            start_pty_reader(Arc::clone(&shell), runtime, reader, false)?;
        }
        match &*lifecycle {
            ShellLifecycle::Running { profile, runtime } => {
                (Arc::clone(runtime), profile.clone(), true, started)
            }
            ShellLifecycle::Exited {
                profile, runtime, ..
            } => (Arc::clone(runtime), profile.clone(), false, started),
            ShellLifecycle::Pending => unreachable!(),
            ShellLifecycle::Closed => {
                return send_response(
                    &mut stream,
                    Response::Error {
                        message: format!("shell not found: {shell_id}"),
                    },
                );
            }
        }
    };
    if started && let Err(error) = registry.persist() {
        let cleanup = shell.kill();
        if cleanup.is_ok() {
            shell.reset_pending()?;
            *lock(&shell.last_profile)? = previous_profile;
        }
        return send_response(
            &mut stream,
            Response::Error {
                message: cleanup.map_or_else(
                    |cleanup| {
                        format!(
                            "could not persist started shell: {error}; process cleanup also failed: {cleanup}"
                        )
                    },
                    |()| format!("could not persist started shell: {error}"),
                ),
            },
        );
    }
    drop(mutation);
    let warning = term_mismatch_warning(startup_profile.term.as_deref(), profile.term.as_deref());
    let control = lock(&runtime.control)?;
    if !registry.contains_shell(&shell)? {
        return send_response(
            &mut stream,
            Response::Error {
                message: format!("shell not found: {shell_id}"),
            },
        );
    }
    {
        let controller = lock(&runtime.controller)?;
        if controller.is_some() && !takeover {
            return send_response(
                &mut stream,
                Response::Error {
                    message: "shell already has an active controller; use takeover".into(),
                },
            );
        }
    }
    if running {
        lock(&runtime.master)?.resize(profile_size(&profile))?;
        update_runtime_dimensions(&shell, &runtime, profile_size(&profile))?;
    }
    lock(&runtime.terminal)?.resize(profile.rows, profile.cols);
    // Keep terminal state locked until the controller is installed so the
    // reconstruction ends exactly where live delivery begins.
    let terminal = lock(&runtime.terminal)?;
    stream.set_write_timeout(Some(HANDSHAKE_TIMEOUT))?;
    send_response(
        &mut stream,
        Response::Attached {
            token: token.clone(),
            reconstruction: terminal.reconstruction(),
            warning,
        },
    )?;
    stream.set_write_timeout(None)?;
    if !running {
        return AttachFrame::Detached.write_to(&mut stream);
    }
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
        }
        | ShellLifecycle::Exited {
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
    if name.trim().is_empty() {
        Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "name cannot be empty",
        ))
    } else {
        Ok(())
    }
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

fn not_found(kind: &str, id: &str) -> io::Error {
    io::Error::new(io::ErrorKind::NotFound, format!("{kind} not found: {id}"))
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
    fn rendered_output_tail_preserves_utf8_boundaries() {
        assert_eq!(tail_utf8("one-λ", 2), "λ");
        assert_eq!(tail_utf8("one-λ", 3), "-λ");
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
    fn failed_persistence_rolls_back_registry_mutation() {
        let directory = env::temp_dir().join(format!("boomux-rollback-{}", Uuid::new_v4()));
        let state_directory = directory.join("state");
        let registry =
            Registry::restore(StateStore::at(state_directory.join("state.json"))).unwrap();
        fs::remove_dir(&state_directory).unwrap();
        fs::write(&state_directory, b"not a directory").unwrap();

        let result = registry.dispatch(Request::CreateWorkspace {
            name: "rolled-back".into(),
            shells: Vec::new(),
        });

        assert!(result.is_err());
        assert!(registry.snapshot().unwrap().workspaces.is_empty());
        fs::remove_dir_all(directory).unwrap();
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
        let (runtime, reader) =
            spawn_runtime(&shell, "workspace", "pause-test", &terminal_profile).unwrap();
        *lock(&shell.lifecycle).unwrap() = ShellLifecycle::Running {
            profile: terminal_profile,
            runtime: Arc::clone(&runtime),
        };
        start_pty_reader(Arc::clone(&shell), Arc::clone(&runtime), reader, false).unwrap();

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
}

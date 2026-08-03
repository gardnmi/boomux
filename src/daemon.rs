use std::collections::{HashMap, HashSet, VecDeque};
use std::env;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::MetadataExt;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, SyncSender, TrySendError};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread;
use std::time::Duration;

use portable_pty::{Child, CommandBuilder, MasterPty, PtySize, native_pty_system};
use uuid::Uuid;

use crate::client;
use crate::protocol::{
    self, AttachFrame, Envelope, Request, Response, ShellSnapshot, ShellSpec, ShellStatus,
    Snapshot, TerminalProfile, WorkspaceSnapshot,
};

const REPLAY_LIMIT: usize = 1024 * 1024;
const CONTROLLER_QUEUE: usize = 64;
const SHUTDOWN_GRACE: Duration = Duration::from_millis(50);
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(2);
const IO_RETRY_DELAY: Duration = Duration::from_millis(2);
const MAX_TERMINAL_ENV_VALUE: usize = 256;

pub fn run() -> io::Result<()> {
    let socket_path = client::socket_path()?;
    let runtime_dir = socket_path
        .parent()
        .ok_or_else(|| io::Error::other("socket path has no parent"))?;
    secure_runtime_dir(runtime_dir)?;
    let _daemon_lock = acquire_daemon_lock(runtime_dir)?;

    if socket_path.exists() {
        fs::remove_file(&socket_path)?;
    }

    let listener = UnixListener::bind(&socket_path)?;
    let _socket_cleanup = SocketCleanup(socket_path.clone());
    fs::set_permissions(&socket_path, fs::Permissions::from_mode(0o600))?;
    listener.set_nonblocking(true)?;
    let registry = Arc::new(Registry::default());
    let shutdown = Arc::new(AtomicBool::new(false));

    while !shutdown.load(Ordering::Acquire) {
        match listener.accept() {
            Ok((stream, _)) => {
                let registry = Arc::clone(&registry);
                let shutdown = Arc::clone(&shutdown);
                thread::spawn(move || {
                    let _ = handle_connection(stream, registry, shutdown);
                });
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(25));
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        }
    }
    registry.shutdown()
}

struct SocketCleanup(PathBuf);

impl Drop for SocketCleanup {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
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

fn handle_connection(
    mut stream: UnixStream,
    registry: Arc<Registry>,
    shutdown: Arc<AtomicBool>,
) -> io::Result<()> {
    let request: Envelope<Request> = protocol::read_message(&mut stream)?;
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
        send_response(&mut stream, Response::Ok)?;
        shutdown.store(true, Ordering::Release);
        return Ok(());
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

#[derive(Default)]
struct Registry {
    state: Mutex<RegistryState>,
}

#[derive(Default)]
struct RegistryState {
    workspaces: HashMap<String, Arc<Workspace>>,
    shells: HashMap<String, Arc<Shell>>,
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
    master: Mutex<Box<dyn MasterPty + Send>>,
    writer: Mutex<Box<dyn Write + Send>>,
    child: Mutex<Box<dyn Child + Send + Sync>>,
    replay: Mutex<VecDeque<u8>>,
    controller: Mutex<Option<Controller>>,
}

struct Controller {
    token: String,
    output: SyncSender<Vec<u8>>,
    connection: UnixStream,
}

impl Registry {
    fn dispatch(&self, request: Request) -> io::Result<Response> {
        match request {
            Request::Ping => Ok(Response::Pong),
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
            Request::CreateWorkspace { name, shells } => Ok(Response::Workspace {
                workspace: self.create_workspace(name, shells)?,
            }),
            Request::CreateShell {
                workspace_id,
                shell,
            } => Ok(Response::Shell {
                shell: match workspace_id {
                    Some(workspace_id) => self.create_shell(&workspace_id, shell)?,
                    None => self.create_shell_with_workspace(shell)?,
                },
            }),
            Request::ReadShell {
                shell_id,
                max_bytes,
            } => Ok(Response::Output {
                bytes: self.read_shell(&shell_id, max_bytes)?,
            }),
            Request::RenameWorkspace { workspace_id, name } => {
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
            }
            Request::RenameShell { shell_id, name } => {
                self.rename_shell(&shell_id, name)?;
                Ok(Response::Ok)
            }
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
        let shells = {
            let mut state = lock(&self.state)?;
            state.workspaces.clear();
            state
                .shells
                .drain()
                .map(|(_, shell)| shell)
                .collect::<Vec<_>>()
        };
        let mut first_error = None;
        for shell in shells {
            if let Err(error) = shell.kill()
                && first_error.is_none()
            {
                first_error = Some(error);
            }
        }
        first_error.map_or(Ok(()), Err)
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
        let replay = lock(&runtime.replay)?;
        let count = max_bytes.min(replay.len());
        Ok(replay.iter().skip(replay.len() - count).copied().collect())
    }

    fn close_shell(&self, shell_id: &str) -> io::Result<()> {
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
        shell.kill()
    }

    fn close_workspace(&self, workspace_id: &str) -> io::Result<()> {
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
        for shell in shells {
            let _ = shell.kill();
        }
        Ok(())
    }
}

impl Workspace {
    fn snapshot(&self, registry: &Registry) -> io::Result<WorkspaceSnapshot> {
        let ids = lock(&self.shell_ids)?.clone();
        let shells = {
            let state = lock(&registry.state)?;
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
        let runtime =
            match std::mem::replace(&mut *lock(&self.lifecycle)?, ShellLifecycle::Closed) {
                ShellLifecycle::Pending | ShellLifecycle::Closed => return Ok(()),
                ShellLifecycle::Running { runtime, .. }
                | ShellLifecycle::Exited { runtime, .. } => runtime,
            };
        if let Some(controller) = lock(&runtime.controller)?.take() {
            let _ = controller.connection.shutdown(std::net::Shutdown::Both);
        }
        let foreground_group = lock(&runtime.master)?.process_group_leader();
        let mut child = lock(&runtime.child)?;
        if child.try_wait()?.is_some() {
            return Ok(());
        }
        let child_pid = child.process_id().map(|pid| pid as libc::pid_t);
        if let Some(session_id) = child_pid {
            signal_session(session_id, libc::SIGHUP);
        }
        for process_group in [foreground_group, child_pid].into_iter().flatten() {
            // Negative PIDs address a process group. portable-pty starts the
            // child as a session and process-group leader on Unix.
            let _ = unsafe { libc::kill(-process_group, libc::SIGHUP) };
        }
        thread::sleep(SHUTDOWN_GRACE);
        if let Some(session_id) = child_pid {
            signal_session(session_id, libc::SIGTERM);
        }
        let kill_result = child.kill();
        thread::sleep(SHUTDOWN_GRACE);
        if let Some(session_id) = child_pid {
            signal_session(session_id, libc::SIGKILL);
        }
        let wait_result = child.wait();
        match (kill_result, wait_result) {
            (_, Ok(_)) => Ok(()),
            (Err(error), Err(_)) => Err(error),
            (Ok(()), Err(error)) => Err(error),
        }
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
        lifecycle: Mutex::new(ShellLifecycle::Pending),
    }))
}

fn spawn_runtime(
    shell: &Arc<Shell>,
    workspace_name: &str,
    shell_name: &str,
    profile: &TerminalProfile,
) -> io::Result<(Arc<ShellRuntime>, Box<dyn Read + Send>)> {
    let pty = native_pty_system()
        .openpty(PtySize {
            rows: profile.rows,
            cols: profile.cols,
            pixel_width: profile.pixel_width,
            pixel_height: profile.pixel_height,
        })
        .map_err(io::Error::other)?;
    set_nonblocking(pty.master.as_ref())?;
    let writer = pty.master.take_writer().map_err(io::Error::other)?;
    let reader = pty.master.try_clone_reader().map_err(io::Error::other)?;

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

    Ok((
        Arc::new(ShellRuntime {
            control: Mutex::new(()),
            master: Mutex::new(pty.master),
            writer: Mutex::new(writer),
            child: Mutex::new(child),
            replay: Mutex::new(VecDeque::with_capacity(REPLAY_LIMIT)),
            controller: Mutex::new(None),
        }),
        reader,
    ))
}

fn start_pty_reader(
    shell: Arc<Shell>,
    runtime: Arc<ShellRuntime>,
    mut reader: Box<dyn Read + Send>,
) {
    thread::spawn(move || {
        let mut buffer = [0; 16 * 1024];
        loop {
            match reader.read(&mut buffer) {
                Ok(0) => break,
                Ok(count) => {
                    let bytes = &buffer[..count];
                    if let Ok(mut replay) = runtime.replay.lock() {
                        append_replay(&mut replay, bytes);
                        if let Ok(mut controller) = runtime.controller.lock() {
                            let disconnect = controller.as_ref().is_some_and(|current| {
                                matches!(
                                    current.output.try_send(bytes.to_vec()),
                                    Err(TrySendError::Full(_) | TrySendError::Disconnected(_))
                                )
                            });
                            if disconnect && let Some(current) = controller.take() {
                                let _ = current.connection.shutdown(std::net::Shutdown::Both);
                            }
                        }
                    }
                }
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::Interrupted | io::ErrorKind::WouldBlock
                    ) =>
                {
                    if error.kind() == io::ErrorKind::WouldBlock {
                        thread::sleep(IO_RETRY_DELAY);
                    }
                    continue;
                }
                Err(_) => break,
            }
        }
        let code = runtime
            .child
            .lock()
            .ok()
            .and_then(|mut child| child.try_wait().ok().flatten())
            .map(|status| status.exit_code());
        if let Ok(mut lifecycle) = shell.lifecycle.lock()
            && let ShellLifecycle::Running {
                profile,
                runtime: current,
            } = &*lifecycle
            && Arc::ptr_eq(current, &runtime)
        {
            *lifecycle = ShellLifecycle::Exited {
                code,
                profile: profile.clone(),
                runtime: Arc::clone(&runtime),
            };
        }
        let _ = runtime
            .controller
            .lock()
            .map(|mut controller| controller.take());
    });
}

fn append_replay(replay: &mut VecDeque<u8>, bytes: &[u8]) {
    if bytes.len() >= REPLAY_LIMIT {
        replay.clear();
        replay.extend(&bytes[bytes.len() - REPLAY_LIMIT..]);
        return;
    }
    let excess = replay
        .len()
        .saturating_add(bytes.len())
        .saturating_sub(REPLAY_LIMIT);
    replay.drain(..excess);
    replay.extend(bytes);
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
    let token = Uuid::new_v4().to_string();
    let (output, receiver) = mpsc::sync_channel(CONTROLLER_QUEUE);
    let connection = stream.try_clone()?;
    let (runtime, startup_profile, running) = {
        let mut lifecycle = lock(&shell.lifecycle)?;
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
            start_pty_reader(Arc::clone(&shell), runtime, reader);
        }
        match &*lifecycle {
            ShellLifecycle::Running { profile, runtime } => {
                (Arc::clone(runtime), profile.clone(), true)
            }
            ShellLifecycle::Exited {
                profile, runtime, ..
            } => (Arc::clone(runtime), profile.clone(), false),
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
        lock(&runtime.master)?
            .resize(profile_size(&profile))
            .map_err(io::Error::other)?;
    }
    // Keep replay locked until the controller is installed so retained output
    // ends exactly where live delivery begins.
    let replay = lock(&runtime.replay)?;
    stream.set_write_timeout(Some(HANDSHAKE_TIMEOUT))?;
    send_response(
        &mut stream,
        Response::Attached {
            token: token.clone(),
            replay: replay.iter().copied().collect(),
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
        });
    }
    let mut output_stream = stream.try_clone()?;
    let output_runtime = Arc::clone(&runtime);
    let output_token = token.clone();
    thread::spawn(move || {
        for bytes in receiver {
            if AttachFrame::Output(bytes)
                .write_to(&mut output_stream)
                .is_err()
            {
                break;
            }
        }
        let _ = AttachFrame::Detached.write_to(&mut output_stream);
        let _ = output_runtime.release_controller(&output_token);
    });
    drop(lifecycle);
    drop(replay);
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
            } => lock(&runtime.master)?
                .resize(PtySize {
                    rows,
                    cols,
                    pixel_width,
                    pixel_height,
                })
                .map_err(io::Error::other),
            AttachFrame::Detached => {
                drop(control);
                break;
            }
            AttachFrame::Output(_) => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "client sent output frame",
            )),
        };
        if let Err(error) = result {
            runtime.release_controller(&token)?;
            return Err(error);
        }
    }
    runtime.release_controller(&token)
}

fn set_nonblocking(master: &dyn MasterPty) -> io::Result<()> {
    let fd = master
        .as_raw_fd()
        .ok_or_else(|| io::Error::other("PTY master does not expose a file descriptor"))?;
    // `fd` belongs to the live PTY master and fcntl only reads/updates its status flags.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags == -1 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
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
        let result = lock(&runtime.writer)?.write(&bytes[offset..]);
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

fn validate_terminal_profile(profile: &TerminalProfile) -> io::Result<()> {
    if profile.rows == 0 || profile.cols == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "terminal profile rows and columns must be nonzero",
        ));
    }
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
    let Ok(entries) = fs::read_dir("/proc") else {
        return;
    };
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
            // PIDs came from procfs and the signal carries no pointer data.
            let _ = unsafe { libc::kill(pid, signal) };
        }
    }
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
    fn replay_keeps_only_the_tail() {
        let mut replay = VecDeque::new();
        append_replay(&mut replay, &vec![1; REPLAY_LIMIT]);
        append_replay(&mut replay, &[2, 3]);
        assert_eq!(replay.len(), REPLAY_LIMIT);
        assert_eq!(replay[REPLAY_LIMIT - 2], 2);
        assert_eq!(replay[REPLAY_LIMIT - 1], 3);
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
        invalid.term = Some("bad\nterm".into());
        assert!(validate_terminal_profile(&invalid).is_err());

        let mut invalid = profile();
        invalid.term = Some("x".repeat(MAX_TERMINAL_ENV_VALUE + 1));
        assert!(validate_terminal_profile(&invalid).is_err());
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

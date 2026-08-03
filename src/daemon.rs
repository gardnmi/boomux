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
    Snapshot, WorkspaceSnapshot,
};

const REPLAY_LIMIT: usize = 1024 * 1024;
const CONTROLLER_QUEUE: usize = 64;
const SHUTDOWN_GRACE: Duration = Duration::from_millis(50);

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

    if let Request::Attach { shell_id, takeover } = request.message {
        return handle_attach(stream, &registry, &shell_id, takeover);
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
    status: Mutex<ShellStatus>,
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
            match spawn_shell(&workspace_id, &name, spec) {
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
        let shell = spawn_shell(workspace_id, &lock(&workspace.name)?.clone(), spec)?;
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
        let replay = lock(&shell.replay)?;
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
        let state = lock(&registry.state)?;
        let ids = lock(&self.shell_ids)?.clone();
        let shells = ids
            .iter()
            .filter_map(|id| state.shells.get(id))
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
            status: lock(&self.status)?.clone(),
        })
    }

    fn kill(&self) -> io::Result<()> {
        if let Some(controller) = lock(&self.controller)?.take() {
            let _ = controller.connection.shutdown(std::net::Shutdown::Both);
        }
        let foreground_group = lock(&self.master)?.process_group_leader();
        let mut child = lock(&self.child)?;
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

    fn is_controller(&self, token: &str) -> io::Result<bool> {
        Ok(lock(&self.controller)?
            .as_ref()
            .is_some_and(|controller| controller.token == token))
    }

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

fn spawn_shell(
    workspace_id: &str,
    workspace_name: &str,
    spec: ShellSpec,
) -> io::Result<Arc<Shell>> {
    validate_name(&spec.name)?;
    let cwd = spec.cwd;
    validate_cwd(&cwd)?;
    let shell_id = Uuid::new_v4().to_string();
    let pty = native_pty_system()
        .openpty(PtySize::default())
        .map_err(io::Error::other)?;
    let writer = pty.master.take_writer().map_err(io::Error::other)?;
    let reader = pty.master.try_clone_reader().map_err(io::Error::other)?;

    let mut command = if spec.command.is_empty() {
        CommandBuilder::new(env::var_os("SHELL").unwrap_or_else(|| "/bin/sh".into()))
    } else {
        let mut command = CommandBuilder::new(&spec.command[0]);
        command.args(&spec.command[1..]);
        command
    };
    command.cwd(&cwd);
    command.env("BOOMUX_WORKSPACE_ID", workspace_id);
    command.env("BOOMUX_WORKSPACE", workspace_name);
    command.env("BOOMUX_SHELL_ID", &shell_id);
    command.env("BOOMUX_SHELL_NAME", &spec.name);
    let child = pty.slave.spawn_command(command).map_err(io::Error::other)?;
    drop(pty.slave);

    let shell = Arc::new(Shell {
        id: shell_id,
        workspace_id: workspace_id.to_owned(),
        name: Mutex::new(spec.name),
        cwd,
        status: Mutex::new(ShellStatus::Running),
        master: Mutex::new(pty.master),
        writer: Mutex::new(writer),
        child: Mutex::new(child),
        replay: Mutex::new(VecDeque::with_capacity(REPLAY_LIMIT)),
        controller: Mutex::new(None),
    });
    start_pty_reader(Arc::clone(&shell), reader);
    Ok(shell)
}

fn start_pty_reader(shell: Arc<Shell>, mut reader: Box<dyn Read + Send>) {
    thread::spawn(move || {
        let mut buffer = [0; 16 * 1024];
        loop {
            match reader.read(&mut buffer) {
                Ok(0) => break,
                Ok(count) => {
                    let bytes = &buffer[..count];
                    if let Ok(mut replay) = shell.replay.lock() {
                        append_replay(&mut replay, bytes);
                        if let Ok(mut controller) = shell.controller.lock() {
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
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(_) => break,
            }
        }
        let code = shell
            .child
            .lock()
            .ok()
            .and_then(|mut child| child.try_wait().ok().flatten())
            .map(|status| status.exit_code());
        if let Ok(mut status) = shell.status.lock() {
            *status = ShellStatus::Exited { code };
        }
        let _ = shell
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
) -> io::Result<()> {
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
    let replay = {
        // Match the reader's replay-then-controller lock order so replay ends
        // exactly where delivery to this controller begins.
        let replay = lock(&shell.replay)?;
        let mut controller = lock(&shell.controller)?;
        if controller.is_some() && !takeover {
            drop(controller);
            drop(replay);
            return send_response(
                &mut stream,
                Response::Error {
                    message: "shell already has an active controller; use takeover".into(),
                },
            );
        }
        if let Some(previous) = controller.take() {
            let _ = previous.connection.shutdown(std::net::Shutdown::Both);
        }
        *controller = Some(Controller {
            token: token.clone(),
            output,
            connection: stream.try_clone()?,
        });
        replay.iter().copied().collect()
    };

    if let Err(error) = send_response(
        &mut stream,
        Response::Attached {
            token: token.clone(),
            replay,
        },
    ) {
        shell.release_controller(&token)?;
        return Err(error);
    }

    let mut output_stream = stream.try_clone()?;
    let output_shell = Arc::clone(&shell);
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
        let _ = output_shell.release_controller(&output_token);
    });

    loop {
        let frame = match AttachFrame::read_from(&mut stream) {
            Ok(frame) => frame,
            Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => break,
            Err(error) => {
                shell.release_controller(&token)?;
                return Err(error);
            }
        };
        if !shell.is_controller(&token)? {
            break;
        }
        match frame {
            AttachFrame::Input(bytes) => lock(&shell.writer)?.write_all(&bytes)?,
            AttachFrame::Resize { rows, cols } => lock(&shell.master)?
                .resize(PtySize {
                    rows,
                    cols,
                    pixel_width: 0,
                    pixel_height: 0,
                })
                .map_err(io::Error::other)?,
            AttachFrame::Detached => break,
            AttachFrame::Output(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "client sent output frame",
                ));
            }
        }
    }
    shell.release_controller(&token)
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
    fn registry_creates_and_closes_workspace_shells() {
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

        registry.close_workspace(&workspace.id).unwrap();
        assert!(registry.snapshot().unwrap().workspaces.is_empty());
    }
}

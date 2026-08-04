use std::collections::{HashMap, HashSet};
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
use crate::state_store::{PersistedShell, PersistedState, PersistedWorkspace, StateStore};
use crate::terminal_state::TerminalState;

const CONTROLLER_QUEUE: usize = 64;
const SHUTDOWN_GRACE: Duration = Duration::from_millis(50);
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(2);
const IO_RETRY_DELAY: Duration = Duration::from_millis(2);
const MAX_TERMINAL_ENV_VALUE: usize = 256;
const MAX_TERMINAL_ROWS: u16 = 1_000;
const MAX_TERMINAL_COLS: u16 = 1_000;
const MAX_TERMINAL_CELLS: usize = 1_000_000;
const MAX_SHELL_READ_BYTES: usize = 1024 * 1024;

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
    let registry = Arc::new(Registry::restore(StateStore::from_environment()?)?);
    let shutdown = Arc::new(AtomicBool::new(false));
    let mut handlers = Vec::new();

    while !shutdown.load(Ordering::Acquire) {
        match listener.accept() {
            Ok((stream, _)) => {
                let registry = Arc::clone(&registry);
                let shutdown = Arc::clone(&shutdown);
                handlers.push(thread::spawn(move || {
                    let _ = handle_connection(stream, registry, shutdown);
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
    let result = registry.shutdown();
    drop(registry);
    drop(listener);
    drop(_daemon_lock);
    drop(_socket_cleanup);
    result
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
        return match registry.shutdown() {
            Ok(()) => {
                shutdown.store(true, Ordering::Release);
                send_response(&mut stream, Response::Ok)
            }
            Err(error) => send_response(
                &mut stream,
                Response::Error {
                    message: format!("could not stop Boomux daemon: {error}"),
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
    master: Mutex<Box<dyn MasterPty + Send>>,
    writer: Mutex<Box<dyn Write + Send>>,
    child: Mutex<Box<dyn Child + Send + Sync>>,
    terminal: Mutex<TerminalState>,
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
        let foreground_group = lock(&runtime.master)?.process_group_leader();
        let mut child = lock(&runtime.child)?;
        let result = if child.try_wait()?.is_some() {
            Ok(())
        } else {
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
        };
        drop(child);
        if result.is_ok() {
            let mut lifecycle = lock(&self.lifecycle)?;
            let current_runtime = match &*lifecycle {
                ShellLifecycle::Running { runtime, .. }
                | ShellLifecycle::Exited { runtime, .. } => Some(runtime),
                ShellLifecycle::Pending | ShellLifecycle::Closed => None,
            };
            if current_runtime.is_some_and(|current| Arc::ptr_eq(current, &runtime)) {
                *lifecycle = ShellLifecycle::Closed;
            }
        }
        result
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
            terminal: Mutex::new(TerminalState::new(profile.rows, profile.cols)),
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
                    if let Ok(mut terminal) = runtime.terminal.lock() {
                        terminal.process(bytes);
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
            start_pty_reader(Arc::clone(&shell), runtime, reader);
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
        lock(&runtime.master)?
            .resize(profile_size(&profile))
            .map_err(io::Error::other)?;
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
                    lock(&runtime.master)?
                        .resize(PtySize {
                            rows,
                            cols,
                            pixel_width,
                            pixel_height,
                        })
                        .map_err(io::Error::other)?;
                    lock(&runtime.terminal)?.resize(rows, cols);
                    Ok(())
                }
            }
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

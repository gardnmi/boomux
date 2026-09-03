#![allow(dead_code)]

use std::collections::{HashMap, HashSet, VecDeque};
use std::env;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::MetadataExt;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child as StdChild, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};
use std::sync::mpsc::{self, SyncSender, TrySendError};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, TryLockError, Weak};
use std::thread;
use std::time::{Duration, Instant};

use portable_pty::{Child, CommandBuilder, MasterPty, PtySize, native_pty_system};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::client;
use crate::desktop_notifications::{
    DesktopNotificationSink, DisabledNotificationSink, NotificationDigest, NotificationNodeContext,
    NotificationReason, NotificationRequest, NotificationSink, category_enabled, test_delivery,
};
use crate::fd_transfer::send_descriptor;
use crate::global_workspace_store::{
    GlobalWorkspaceStore, PendingResourceKind, PendingWorkspaceResource,
    PreparedDefaultCwdOperation, PreparedWorkspaceResource, PreparedWorkspaceShell,
};
use crate::handoff;
use crate::host_services::{self, PreparedIntegrationMutation};
use crate::local_shell_journal::{
    LocalShellJournal, LocalShellJournalRecord, LocalShellStartTransaction, LocalShellTransaction,
};
use crate::node_identity::{NodeIdentityLease, NodeIdentityManager};
use crate::node_projection::{
    NodeProjectionCache, ProjectionCommit, ProjectionObservation, RemoteDigestClaim,
    RemoteNotificationCategory, RemoteNotificationClaim,
};
use crate::node_registration::NodeRegistrationManager;
use crate::protocol::ClaudeRemoteControlBindingSnapshot;
use crate::protocol::{
    self, AgentAttentionReason, AgentAttentionSnapshot, AgentAuthority, AgentInstanceSnapshot,
    AgentObservationSnapshot, AgentRegistrationSpec, AgentReport, AgentState,
    AgentWorkingContextSnapshot, AttachFrame, DaemonEvent, DaemonEventKind, Envelope, ErrorCode,
    EventCursor, FocusedTerminalSnapshot, HostIntegrationMutationPreview, HostServiceOperation,
    HostServiceResult, MAX_AGENT_WORKING_CONTEXTS, MAX_HOST_SERVICE_SESSIONS, NodeProjectionAgent,
    NodeProjectionAttention, NodeProjectionLauncher, NodeProjectionShell, NodeProjectionSnapshot,
    NodeProjectionSync, NodeProjectionSyncMode, NodeProjectionTransition,
    NodeProjectionTransitionKind, NodeProjectionWorkspace, NotificationDeliveryConfig,
    OpenCodeSessionClaimSnapshot, OpenCodeSharedRuntimeSnapshot, QualifiedFocusedTerminalSnapshot,
    QualifiedIdentity, Request, Response, RoutedOperation, RoutedOperationResult,
    ShellRunExitReason, ShellRunSnapshot, ShellSnapshot, ShellSpec, ShellStatus, Snapshot,
    TerminalPreview, TerminalProfile, UnixEnvironment, UnixEnvironmentVariable,
    WorkspaceLauncherSnapshot, WorkspaceLauncherSpec, WorkspaceSnapshot,
};
use crate::ssh_bootstrap::{self, RemoteBootstrapPlan, SshAuthenticationMode, SshTarget};
#[cfg(debug_assertions)]
use crate::state_store::state_directory_from_environment;
use crate::state_store::{
    PersistedAgentInstance, PersistedHiddenSession, PersistedSessionDisplayName,
    PersistedSessionDisplayNameOperation, PersistedSessionHideOperation, PersistedSessionIdentity,
    PersistedShell, PersistedShellRun, PersistedState, PersistedWorkspace,
    PersistedWorkspaceLauncher, StateStore,
};
use crate::terminal_state::TerminalState;

const CONTROLLER_QUEUE: usize = 64;
const MAX_COLLABORATORS_PER_SHELL: usize = 4;
const MAX_CONNECTION_HANDLERS: usize = 64;
const SHUTDOWN_GRACE: Duration = Duration::from_millis(50);
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(2);
const RESPONSE_WRITE_TIMEOUT: Duration = Duration::from_secs(1);
const KIRO_HOLDER_RECONCILE_INTERVAL: Duration = Duration::from_secs(1);
const RESTART_TIMEOUT: Duration = Duration::from_secs(10);
const IO_RETRY_DELAY: Duration = Duration::from_millis(2);
const OUTPUT_PUBLICATION_INTERVAL: Duration = Duration::from_millis(16);
const PERSIST_RETRY_INTERVAL: Duration = Duration::from_secs(1);
const TERMINAL_HISTORY_INTERVAL: Duration = Duration::from_secs(5);
const FOREGROUND_PROCESS_CACHE_INTERVAL: Duration = Duration::from_secs(1);
const MAX_TERMINAL_HISTORY_BYTES: usize = 256 * 1024;
const MAX_TERMINAL_ENV_VALUE: usize = 256;
const MAX_NAME_BYTES: usize = 256;
const MAX_AGENT_EVIDENCE_BYTES: usize = 4 * 1024;
const MAX_HOST_SERVICE_PREVIEWS: usize = 64;
const HOST_SERVICE_PREVIEW_TTL: Duration = Duration::from_secs(300);
const HOST_SESSION_CATALOG_TTL: Duration = Duration::from_secs(30);
const HOST_SESSION_CATALOG_FAILURE_TTL: Duration = Duration::from_secs(5);
const MAX_HOST_SESSION_CATALOG_ENTRIES: usize = 256;
const REGISTERED_NODE_RESPONSE_TIMEOUT: Duration = Duration::from_secs(2);
const REGISTERED_NODE_SESSION_RESPONSE_TIMEOUT: Duration = Duration::from_secs(20);
const MAX_SESSION_DISPLAY_NAME_CHARS: usize = 160;
const MAX_SESSION_DISPLAY_NAMES_PER_WORKSPACE: usize = 1_024;
const MAX_SESSION_DISPLAY_NAME_OPERATIONS_PER_WORKSPACE: usize = 256;
const MAX_HIDDEN_SESSIONS_PER_WORKSPACE: usize = 1_024;
const MAX_SESSION_HIDE_OPERATIONS_PER_WORKSPACE: usize = 256;
const MAX_TERMINAL_ROWS: u16 = 1_000;
const MAX_TERMINAL_COLS: u16 = 1_000;
const MAX_TERMINAL_PREVIEW_LINES: usize = 500;
const MAX_TERMINAL_PREVIEW_SPANS: usize = 20_000;
const MAX_TERMINAL_CELLS: usize = 1_000_000;
const MAX_SHELL_READ_BYTES: usize = 1024 * 1024;
const MAX_FOREGROUND_PROCESS_BYTES: usize = 64;
const MAX_RETAINED_EVENTS: usize = 8_192;
const DISPATCH_KEY_FILTER_BYTES: usize = 2048;
const MAX_EVENT_BATCH: u16 = 256;
const MAX_EVENT_WAIT: Duration = Duration::from_secs(30);
const TRANSITION_IDLE: u8 = 0;
const TRANSITION_RESTART: u8 = 1;
const TRANSITION_SHUTDOWN: u8 = 2;
const TRANSITION_REKEY: u8 = 3;
const NODE_REKEY_DRAIN_TIMEOUT: Duration = Duration::from_millis(250);
const NODE_REGISTRATION_DRAIN_TIMEOUT: Duration = Duration::from_millis(250);
const NODE_UPGRADE_MAINTENANCE_LEASE: Duration = Duration::from_secs(10 * 60);
const OPENCODE_CLAIM_HOLDER_TTL: Duration = Duration::from_secs(5 * 60);
const OPENCODE_RUNTIME_READINESS_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_UNIX_ENVIRONMENT_VARIABLES: usize = 4_096;
const MAX_UNIX_ENVIRONMENT_BYTES: usize = 1024 * 1024;
const MAX_OPENCODE_CLAIM_ROOTS: usize = 1_024;
const MAX_OPENCODE_CLAIM_HOLDERS: usize = 4_096;
const OPENCODE_TUI: &[u8] = include_bytes!("../integrations/opencode/boomux-tui.tsx");
const OPENCODE_TUI_CORE: &[u8] = include_bytes!("../integrations/opencode/boomux-tui-core.js");
const OPENCODE_TUI_RUNNER: &[u8] = include_bytes!("../integrations/opencode/boomux-tui-runner.js");
const OPENCODE_SHIM: &[u8] = br#"#!/bin/sh
if [ "$#" -eq 0 ] && [ -t 0 ] && [ -t 1 ] && [ -n "${BOOMUX_SHELL_ID-}" ] && [ -n "${BOOMUX_RUN_ID-}" ]; then
  exec "$BOOMUX_SHIM_EXECUTABLE" opencode shared
fi
exec "$BOOMUX_REAL_OPENCODE" "$@"
"#;
const CLAUDE_SHIM: &[u8] = br#"#!/bin/sh
if [ "${BOOMUX_CLAUDE_REMOTE_CONTROL-0}" = 1 ] && [ "$#" -eq 0 ] && [ -t 0 ] && [ -t 1 ] && [ -n "${BOOMUX_SHELL_ID-}" ] && [ -n "${BOOMUX_RUN_ID-}" ]; then
  exec "$BOOMUX_REAL_CLAUDE" --remote-control
fi
exec "$BOOMUX_REAL_CLAUDE" "$@"
"#;
const CODEX_SHIM: &[u8] = br#"#!/bin/sh
exec "$BOOMUX_SHIM_EXECUTABLE" codex launch -- "$@"
"#;
const KIRO_SHIM: &[u8] = br#"#!/bin/sh
exec "$BOOMUX_SHIM_EXECUTABLE" kiro launch -- "$@"
"#;
const OPENCODE_BASH_RC: &[u8] = br#"if [ -r "${HOME}/.bashrc" ]; then
  . "${HOME}/.bashrc"
fi
_boomux_path=()
IFS=: read -r -a _boomux_path <<< "${PATH-}"
_boomux_filtered=()
for _boomux_entry in "${_boomux_path[@]}"; do
  if [ "$_boomux_entry" != "$BOOMUX_OPENCODE_SHIM_DIR" ]; then
    _boomux_filtered+=("$_boomux_entry")
  fi
done
BOOMUX_ORIGINAL_PATH="$(IFS=:; printf '%s' "${_boomux_filtered[*]}")"
PATH="$BOOMUX_OPENCODE_SHIM_DIR${BOOMUX_ORIGINAL_PATH:+:$BOOMUX_ORIGINAL_PATH}"
export BOOMUX_ORIGINAL_PATH PATH
builtin hash -r
unset _boomux_entry _boomux_filtered _boomux_path
"#;
const OPENCODE_ZSH_ENV: &[u8] = br#"if [[ -r "$BOOMUX_USER_ZDOTDIR/.zshenv" ]]; then
  source "$BOOMUX_USER_ZDOTDIR/.zshenv"
fi
typeset -gx BOOMUX_USER_ZDOTDIR="${ZDOTDIR:-$HOME}"
typeset -gx ZDOTDIR="$BOOMUX_OPENCODE_SHIM_DIR"
"#;
const OPENCODE_ZSH_RC: &[u8] = br#"if [[ -r "$BOOMUX_USER_ZDOTDIR/.zshrc" ]]; then
  source "$BOOMUX_USER_ZDOTDIR/.zshrc"
fi
path=("${(@)path:#$BOOMUX_OPENCODE_SHIM_DIR}")
typeset -gx BOOMUX_ORIGINAL_PATH="${(j/:/)path}"
path=("$BOOMUX_OPENCODE_SHIM_DIR" "${path[@]}")
typeset -gx ZDOTDIR="$BOOMUX_USER_ZDOTDIR"
unset BOOMUX_USER_ZDOTDIR
"#;
const OPENCODE_FISH_INIT: &str = "set -l boomux_path; for entry in $PATH; test \"$entry\" = \"$BOOMUX_OPENCODE_SHIM_DIR\"; or set -a boomux_path \"$entry\"; end; set -gx BOOMUX_ORIGINAL_PATH (string join : $boomux_path); set -gx PATH $BOOMUX_OPENCODE_SHIM_DIR $boomux_path; set -e boomux_path";

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
            resume_agents: config.resume_agents,
            persist_terminal_history: config.persist_terminal_history,
            claude_remote_control: true,
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
            resume_agents: settings.resume_agents,
            persist_terminal_history: settings.persist_terminal_history,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NotificationDeliverySettings {
    pub desktop: NotificationSettings,
    pub sound: NotificationSoundSettings,
    pub resume_agents: bool,
    pub persist_terminal_history: bool,
    pub claude_remote_control: bool,
}

impl Default for NotificationDeliverySettings {
    fn default() -> Self {
        Self {
            desktop: NotificationSettings::default(),
            sound: NotificationSoundSettings::default(),
            resume_agents: true,
            persist_terminal_history: false,
            claude_remote_control: true,
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
            presented_focused_terminal,
            opencode_runtime,
            claude_remote_control_bindings,
            kiro_launch_holders,
        } => {
            let store = StateStore::from_transferred_lock(state_lock)?;
            let claude_remote_control = notification_settings.claude_remote_control;
            let mut notification_settings = (*notifications)
                .map(Into::into)
                .unwrap_or(notification_settings);
            notification_settings.claude_remote_control = claude_remote_control;
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
                    presented_focused_terminal: presented_focused_terminal.map(|focused| *focused),
                    opencode_runtime,
                    claude_remote_control_bindings,
                    kiro_launch_holders,
                },
                Some(&mut channel),
                notification_settings,
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
    presented_focused_terminal: Option<QualifiedFocusedTerminalSnapshot>,
    opencode_runtime: Option<handoff::TransferredOpenCodeRuntime>,
    claude_remote_control_bindings: Vec<ClaudeRemoteControlBindingSnapshot>,
    kiro_launch_holders: Vec<handoff::KiroLaunchHolderManifest>,
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
    registry.global_workspaces = match GlobalWorkspaceStore::load_from_environment() {
        Ok(store) => Some(store),
        Err(error) => {
            eprintln!("boomux: global Workspace coordination disabled: {error}");
            None
        }
    };
    registry.local_shell_journal = Some(LocalShellJournal::load_from_environment()?);
    registry.replay_local_shell_transactions()?;
    if let (Some(identity), Some(global_workspaces)) = (
        registry.node_identity.as_ref(),
        registry.global_workspaces.as_ref(),
    ) {
        let node_id = identity.id()?;
        global_workspaces.initialize_local_once(&node_id, &registry.snapshot()?)?;
    }
    registry.startup_environment =
        sanitize_opencode_shim_environment(&capture_current_environment());
    registry.notification_settings = notification_settings.clone();
    if !registry.notification_settings.persist_terminal_history {
        registry.clear_terminal_histories()?;
    }
    registry.notification_sink = Arc::new(DesktopNotificationSink::new(notification_settings));
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
    registry.import_presented_focused_terminal(transferred.presented_focused_terminal)?;
    registry
        .opencode
        .import_handoff(transferred.opencode_runtime)?;
    registry.import_claude_remote_control_bindings(transferred.claude_remote_control_bindings)?;
    registry.import_kiro_launch_holders(transferred.kiro_launch_holders)?;
    #[cfg(debug_assertions)]
    if live_handoff
        && registry.native_test_hooks_enabled()
        && consume_native_test_handoff_import_failure()?
    {
        return Err(io::Error::other("native test rejected handoff import"));
    }
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
    registry.start_node_projection_workers()?;
    let shutdown = Arc::new(AtomicBool::new(false));
    let transition = Arc::new(AtomicU8::new(TRANSITION_IDLE));
    let (restart_sender, restart_receiver) = mpsc::channel::<RestartRequest>();
    let mut handlers: Vec<thread::JoinHandle<()>> = Vec::new();
    let mut handed_off = false;
    let mut last_persistence_retry = Instant::now();
    let mut last_kiro_reconciliation = Instant::now();

    while !shutdown.load(Ordering::Acquire) {
        if last_kiro_reconciliation.elapsed() >= KIRO_HOLDER_RECONCILE_INTERVAL {
            let _ = registry.reconcile_dead_kiro_holders();
            last_kiro_reconciliation = Instant::now();
        }
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
        registry.opencode.reap_exited()?;
        match restart_receiver.try_recv() {
            Ok(request) => {
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

#[cfg(debug_assertions)]
fn consume_native_test_handoff_import_failure() -> io::Result<bool> {
    let marker = state_directory_from_environment()?.join(".native-test-fail-handoff-import");
    match fs::remove_file(marker) {
        Ok(()) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
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

fn opencode_runtime_registration_path() -> io::Result<PathBuf> {
    client::socket_path()?
        .parent()
        .map(|directory| directory.join("opencode-runtime.json"))
        .ok_or_else(|| io::Error::other("Boomux socket path has no parent"))
}

fn process_start_time(stat: &str) -> Option<u64> {
    stat.rsplit_once(')')?
        .1
        .split_whitespace()
        .nth(19)?
        .parse()
        .ok()
}

fn process_argv(pid: u32) -> io::Result<Vec<Vec<u8>>> {
    let bytes = fs::read(format!("/proc/{pid}/cmdline"))?;
    Ok(bytes
        .split(|byte| *byte == 0)
        .filter(|argument| !argument.is_empty())
        .map(<[u8]>::to_vec)
        .collect())
}

fn process_has_environment(pid: u32, name: &[u8], value: &[u8]) -> io::Result<bool> {
    Ok(process_environment_value(pid, name)?.as_deref() == Some(value))
}

fn process_environment_value(pid: u32, name: &[u8]) -> io::Result<Option<Vec<u8>>> {
    let bytes = fs::read(format!("/proc/{pid}/environ"))?;
    Ok(bytes.split(|byte| *byte == 0).find_map(|variable| {
        let separator = variable.iter().position(|byte| *byte == b'=')?;
        (variable[..separator] == *name).then(|| variable[separator + 1..].to_vec())
    }))
}

fn kiro_holder_process_evidence(
    pid: u32,
    shell_id: &str,
    run_id: &str,
) -> DaemonResult<(u64, bool)> {
    if pid == 0 {
        return Err(DaemonError::validation(
            "Kiro launch holder PID must be nonzero",
        ));
    }
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).map_err(|_| {
        DaemonError::lifecycle(
            ErrorCode::NotFound,
            "Kiro launch holder process was not found",
        )
    })?;
    let start_time = process_start_time(&stat).ok_or_else(|| {
        DaemonError::validation("Kiro launch holder process has invalid start identity")
    })?;
    let argv = process_argv(pid).map_err(|_| {
        DaemonError::lifecycle(
            ErrorCode::NotFound,
            "Kiro launch holder argv was not available",
        )
    })?;
    let managed_launcher = argv
        .windows(2)
        .any(|arguments| arguments == [b"kiro".as_slice(), b"launch".as_slice()]);
    let exact_environment = process_has_environment(pid, b"BOOMUX_SHELL_ID", shell_id.as_bytes())
        .unwrap_or(false)
        && process_has_environment(pid, b"BOOMUX_RUN_ID", run_id.as_bytes()).unwrap_or(false);
    if !managed_launcher || !exact_environment {
        return Err(DaemonError::validation(
            "Kiro launch holder does not match the managed launcher ShellRun",
        ));
    }
    Ok((
        start_time,
        proc_process_group(&stat) == Some(pid as libc::pid_t),
    ))
}

fn kiro_holder_is_live(holder: &KiroLaunchHolder) -> bool {
    fs::read_to_string(format!("/proc/{}/stat", holder.pid))
        .ok()
        .and_then(|stat| process_start_time(&stat))
        == Some(holder.start_time)
}

fn proc_process_group(stat: &str) -> Option<libc::pid_t> {
    stat.rsplit_once(')')?
        .1
        .split_whitespace()
        .nth(2)?
        .parse()
        .ok()
}

fn terminate_dead_kiro_holder_group(holder_id: &str, holder: &KiroLaunchHolder) {
    if !holder.process_group_leader {
        return;
    }
    let group = holder.pid as libc::pid_t;
    let authorized = fs::read_dir("/proc").is_ok_and(|entries| {
        entries.filter_map(Result::ok).any(|entry| {
            let Some(pid) = entry
                .file_name()
                .to_str()
                .and_then(|name| name.parse::<u32>().ok())
            else {
                return false;
            };
            fs::read_to_string(format!("/proc/{pid}/stat"))
                .ok()
                .and_then(|stat| proc_process_group(&stat))
                == Some(group)
                && process_has_environment(pid, b"BOOMUX_KIRO_LAUNCH_HOLDER", holder_id.as_bytes())
                    .unwrap_or(false)
        })
    });
    if authorized {
        unsafe {
            libc::kill(-group, libc::SIGTERM);
            libc::kill(-group, libc::SIGKILL);
        }
    }
}

fn kiro_holder_matches_current_run(
    state: &DurableState,
    holder: &KiroLaunchHolder,
) -> io::Result<bool> {
    let Some(shell) = state.shells.get(&holder.shell_id) else {
        return Ok(false);
    };
    Ok(matches!(
        &*lock(&shell.lifecycle)?,
        ShellLifecycle::Running { run, .. } if run.id == holder.run_id
    ))
}

fn prune_dead_kiro_holders(
    holders: &mut HashMap<String, KiroLaunchHolder>,
) -> Vec<(String, KiroLaunchHolder)> {
    let dead = holders
        .iter()
        .filter(|(_, holder)| !kiro_holder_is_live(holder))
        .map(|(id, _)| id.clone())
        .collect::<Vec<_>>();
    dead.into_iter()
        .filter_map(|id| {
            holders.remove(&id).map(|holder| {
                terminate_dead_kiro_holder_group(&id, &holder);
                (id, holder)
            })
        })
        .collect()
}

fn opencode_process_evidence(pid: u32) -> io::Result<(u64, Vec<u8>, Vec<Vec<u8>>)> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat"))?;
    let start_time = process_start_time(&stat)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "invalid process start time"))?;
    let executable = fs::read_link(format!("/proc/{pid}/exe"))?
        .as_os_str()
        .as_bytes()
        .to_vec();
    Ok((start_time, executable, process_argv(pid)?))
}

fn write_opencode_runtime_registration(generation_id: &str, pid: u32, port: u16) -> io::Result<()> {
    let (start_time, executable, argv) = opencode_process_evidence(pid)?;
    let registration = OpenCodeRuntimeRegistration {
        generation_id: generation_id.into(),
        pid,
        port,
        start_time,
        executable,
        argv,
    };
    let bytes = serde_json::to_vec(&registration).map_err(io::Error::other)?;
    atomic_runtime_asset(&opencode_runtime_registration_path()?, &bytes, 0o600)
}

fn remove_opencode_runtime_registration() -> io::Result<()> {
    match fs::remove_file(opencode_runtime_registration_path()?) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

fn read_opencode_runtime_registration() -> io::Result<Option<OpenCodeRuntimeRegistration>> {
    let path = opencode_runtime_registration_path()?;
    let mut file = match OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&path)
    {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    let metadata = file.metadata()?;
    if !metadata.is_file()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.mode() & 0o077 != 0
        || metadata.len() > 64 * 1024
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "OpenCode runtime registration is not an owner-only bounded file",
        ));
    }
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    file.read_to_end(&mut bytes)?;
    serde_json::from_slice(&bytes)
        .map(Some)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

fn adopt_registered_opencode_runtime() -> io::Result<Option<OpenCodeRuntime>> {
    let registration = match read_opencode_runtime_registration() {
        Ok(Some(registration)) => registration,
        Ok(None) => return Ok(None),
        Err(_) => {
            let _ = remove_opencode_runtime_registration();
            return Ok(None);
        }
    };
    let valid_static = registration.port != 0
        && registration.pid != 0
        && Uuid::parse_str(&registration.generation_id).is_ok();
    let evidence = opencode_process_evidence(registration.pid);
    let valid_process = evidence.is_ok_and(|(start_time, executable, argv)| {
        start_time == registration.start_time
            && executable == registration.executable
            && argv == registration.argv
            && is_opencode_serve_argv(&argv, registration.port)
    });
    let stat = fs::read_to_string(format!("/proc/{}/stat", registration.pid));
    let valid_session = stat
        .as_deref()
        .ok()
        .is_some_and(|stat| proc_session_id(stat) == Some(registration.pid as libc::pid_t));
    let valid_generation = process_has_environment(
        registration.pid,
        b"BOOMUX_OPENCODE_SHARED_GENERATION",
        registration.generation_id.as_bytes(),
    )
    .unwrap_or(false);
    if !valid_static || !valid_process || !valid_session || !valid_generation {
        let _ = remove_opencode_runtime_registration();
        return Ok(None);
    }
    let process = ImportedProcess {
        pid: registration.pid,
        pidfd: open_pidfd(registration.pid)?,
    };
    let revalidated = !process.has_exited()?
        && opencode_process_evidence(registration.pid).is_ok_and(
            |(start_time, executable, argv)| {
                start_time == registration.start_time
                    && executable == registration.executable
                    && argv == registration.argv
                    && is_opencode_serve_argv(&argv, registration.port)
            },
        )
        && TcpStream::connect(("127.0.0.1", registration.port)).is_ok()
        && opencode_listener_belongs_to_session(registration.port, registration.pid);
    if !revalidated {
        let _ = remove_opencode_runtime_registration();
        return Ok(None);
    }
    Ok(Some(OpenCodeRuntime {
        generation_id: registration.generation_id,
        port: registration.port,
        pid: registration.pid,
        process: OpenCodeRuntimeProcess::Imported(process),
    }))
}

fn is_opencode_serve_argv(argv: &[Vec<u8>], port: u16) -> bool {
    let port = port.to_string();
    argv.windows(5).any(|arguments| {
        arguments[0] == b"serve"
            && arguments[1] == b"--hostname"
            && arguments[2] == b"127.0.0.1"
            && arguments[3] == b"--port"
            && arguments[4] == port.as_bytes()
    })
}

fn discover_unregistered_opencode_runtime(port: u16) -> io::Result<Option<OpenCodeRuntime>> {
    let Ok(entries) = fs::read_dir("/proc") else {
        return Ok(None);
    };
    let mut discovered = None;
    for entry in entries.flatten() {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<u32>().ok())
        else {
            continue;
        };
        let Ok(stat) = fs::read_to_string(entry.path().join("stat")) else {
            continue;
        };
        if proc_session_id(&stat) != Some(pid as libc::pid_t)
            || !process_argv(pid).is_ok_and(|argv| is_opencode_serve_argv(&argv, port))
            || !opencode_listener_belongs_to_session(port, pid)
        {
            continue;
        }
        let Some(generation_id) =
            process_environment_value(pid, b"BOOMUX_OPENCODE_SHARED_GENERATION")
                .ok()
                .flatten()
                .and_then(|value| String::from_utf8(value).ok())
                .filter(|value| Uuid::parse_str(value).is_ok())
        else {
            continue;
        };
        let process = ImportedProcess {
            pid,
            pidfd: open_pidfd(pid)?,
        };
        if process.has_exited()?
            || !process_argv(pid).is_ok_and(|argv| is_opencode_serve_argv(&argv, port))
            || !opencode_listener_belongs_to_session(port, pid)
        {
            continue;
        }
        if discovered.is_some() {
            return Ok(None);
        }
        discovered = Some(OpenCodeRuntime {
            generation_id,
            port,
            pid,
            process: OpenCodeRuntimeProcess::Imported(process),
        });
    }
    if let Some(runtime) = &discovered {
        write_opencode_runtime_registration(&runtime.generation_id, runtime.pid, runtime.port)?;
    }
    Ok(discovered)
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

fn environment_value(environment: &UnixEnvironment, name: &[u8]) -> Option<std::ffi::OsString> {
    environment
        .variables
        .iter()
        .find(|variable| variable.name == name)
        .map(|variable| std::ffi::OsString::from_vec(variable.value.clone()))
}

fn set_environment_value(
    environment: &mut UnixEnvironment,
    name: &[u8],
    value: impl Into<std::ffi::OsString>,
) {
    environment
        .variables
        .retain(|variable| variable.name != name);
    environment.variables.push(UnixEnvironmentVariable {
        name: name.to_vec(),
        value: value.into().into_vec(),
    });
}

fn sanitize_opencode_shim_environment(environment: &UnixEnvironment) -> UnixEnvironment {
    let shim_dir = environment_value(environment, b"BOOMUX_OPENCODE_SHIM_DIR");
    let original_path = environment_value(environment, b"BOOMUX_ORIGINAL_PATH");
    let private_tui_config = environment_value(environment, b"BOOMUX_OPENCODE_TUI_CONFIG");
    let user_zdotdir = environment_value(environment, b"BOOMUX_USER_ZDOTDIR");
    let mut sanitized = environment.clone();
    const PRIVATE_NAMES: &[&[u8]] = &[
        b"BOOMUX_CLAUDE_REMOTE_CONTROL",
        b"BOOMUX_REAL_CLAUDE",
        b"BOOMUX_REAL_CODEX",
        b"BOOMUX_REAL_KIRO",
        b"BOOMUX_REAL_OPENCODE",
        b"BOOMUX_CODEX_RUN_SCOPED",
        b"BOOMUX_KIRO_RUN_SCOPED",
        b"BOOMUX_KIRO_LAUNCH_HOLDER",
        b"BOOMUX_ORIGINAL_PATH",
        b"BOOMUX_OPENCODE_SHIM_DIR",
        b"BOOMUX_OPENCODE_TUI_CONFIG",
        b"BOOMUX_SHIM_EXECUTABLE",
        b"BOOMUX_OPENCODE_SHARED_GENERATION",
        b"BOOMUX_OPENCODE_CLAIM_HOLDER",
        b"BOOMUX_USER_ZDOTDIR",
    ];
    sanitized
        .variables
        .retain(|variable| !PRIVATE_NAMES.contains(&variable.name.as_slice()));
    if let Some(original_path) = original_path {
        set_environment_value(&mut sanitized, b"PATH", original_path);
    } else if let (Some(path), Some(shim_dir)) =
        (environment_value(&sanitized, b"PATH"), shim_dir.as_deref())
    {
        let filtered = env::split_paths(&path)
            .filter(|entry| entry.as_os_str() != shim_dir)
            .collect::<Vec<_>>();
        if let Ok(path) = env::join_paths(filtered) {
            set_environment_value(&mut sanitized, b"PATH", path);
        }
    }
    if let Some(user_zdotdir) = user_zdotdir {
        set_environment_value(&mut sanitized, b"ZDOTDIR", user_zdotdir);
    }
    if let Some(private_tui_config) = private_tui_config {
        sanitized.variables.retain(|variable| {
            variable.name != b"OPENCODE_TUI_CONFIG"
                || std::ffi::OsStr::from_bytes(&variable.value) != private_tui_config
        });
    }
    sanitized
}

fn resolve_opencode_executable(
    environment: &UnixEnvironment,
    excluded_directory: Option<&Path>,
) -> Option<PathBuf> {
    resolve_executable(environment, excluded_directory, "opencode")
}

fn resolve_codex_executable(
    environment: &UnixEnvironment,
    excluded_directory: Option<&Path>,
) -> Option<PathBuf> {
    resolve_executable(environment, excluded_directory, "codex")
}

fn resolve_kiro_executable(
    environment: &UnixEnvironment,
    excluded_directory: Option<&Path>,
) -> Option<PathBuf> {
    resolve_executable(environment, excluded_directory, "kiro-cli")
}

fn resolve_executable(
    environment: &UnixEnvironment,
    excluded_directory: Option<&Path>,
    executable: &str,
) -> Option<PathBuf> {
    let path = environment_value(environment, b"PATH")?;
    let current_executable = env::current_exe()
        .ok()
        .and_then(|path| path.canonicalize().ok());
    env::split_paths(&path).find_map(|directory| {
        if !directory.is_absolute() || excluded_directory == Some(directory.as_path()) {
            return None;
        }
        let candidate = directory.join(executable);
        let metadata = fs::metadata(&candidate).ok()?;
        if !metadata.is_file() || metadata.permissions().mode() & 0o111 == 0 {
            return None;
        }
        let canonical = candidate.canonicalize().ok()?;
        (Some(&canonical) != current_executable.as_ref()).then_some(candidate)
    })
}

fn opencode_shim_eligible(shell: &Shell, effective_command: &[String]) -> bool {
    shell.command.is_empty() && effective_command.is_empty()
}

fn supervised_opencode_session(effective_command: &[String]) -> Option<&str> {
    if effective_command.len() != 12 {
        return None;
    }
    let external_session_id = &effective_command[7];
    let session_id = &effective_command[11];
    (Path::new(&effective_command[0]).file_name()?.to_str()? == "boomux"
        && effective_command[1] == "agent"
        && effective_command[2] == "supervise"
        && effective_command[4] == "--integration"
        && effective_command[5] == "opencode"
        && effective_command[6] == "--external-session-id"
        && effective_command[8] == "--"
        && Path::new(&effective_command[9]).file_name()?.to_str()? == "opencode"
        && effective_command[10] == "--session"
        && !session_id.is_empty()
        && session_id == external_session_id)
        .then_some(session_id.as_str())
}

fn supervised_shared_opencode_command(
    effective_command: &[String],
    boomux: &str,
) -> Option<Vec<String>> {
    let session_id = supervised_opencode_session(effective_command)?;
    let mut command = effective_command[..9].to_vec();
    command.extend([
        boomux.into(),
        "opencode".into(),
        "shared".into(),
        "--session".into(),
        session_id.into(),
    ]);
    Some(command)
}

fn codex_launch_eligible(_shell: &Shell, effective_command: &[String]) -> bool {
    effective_command.first().is_some_and(|executable| {
        Path::new(executable)
            .file_name()
            .and_then(std::ffi::OsStr::to_str)
            == Some("codex")
    }) && (effective_command.len() == 1
        || effective_command
            .get(1)
            .is_some_and(|argument| matches!(argument.as_str(), "resume" | "exec")))
}

fn kiro_launch_eligible(_shell: &Shell, effective_command: &[String]) -> bool {
    effective_command.first().is_some_and(|executable| {
        Path::new(executable)
            .file_name()
            .and_then(std::ffi::OsStr::to_str)
            == Some("kiro-cli")
    }) && (effective_command.len() == 1
        || effective_command
            .get(1)
            .is_some_and(|argument| argument == "--v3"))
}

fn claude_remote_control_command(
    _shell: &Shell,
    effective_command: &[String],
    recovery_override: bool,
    enabled: bool,
) -> Option<Vec<String>> {
    (enabled
        && !recovery_override
        && effective_command.len() == 1
        && Path::new(&effective_command[0])
            .file_name()
            .and_then(std::ffi::OsStr::to_str)
            == Some("claude"))
    .then(|| vec![effective_command[0].clone(), "--remote-control".into()])
}

fn inject_opencode_shim_environment(
    environment: &UnixEnvironment,
    claude_remote_control: bool,
) -> io::Result<UnixEnvironment> {
    let runtime_root = PathBuf::from(
        environment_value(environment, b"XDG_RUNTIME_DIR").ok_or_else(|| {
            io::Error::new(io::ErrorKind::NotFound, "XDG_RUNTIME_DIR is unavailable")
        })?,
    );
    if !runtime_root.is_absolute() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "XDG_RUNTIME_DIR must be absolute",
        ));
    }
    let shim_dir = runtime_root.join("boomux/shims");
    secure_runtime_dir(&runtime_root.join("boomux"))?;
    secure_runtime_dir(&shim_dir)?;
    let real_opencode = resolve_opencode_executable(environment, Some(&shim_dir));
    let real_claude = resolve_executable(environment, Some(&shim_dir), "claude");
    let real_codex = resolve_codex_executable(environment, Some(&shim_dir));
    let real_kiro = resolve_kiro_executable(environment, Some(&shim_dir));
    let boomux = env::current_exe()?.canonicalize()?;
    let boomux_metadata = fs::metadata(&boomux)?;
    if !boomux.is_absolute()
        || !boomux_metadata.is_file()
        || boomux_metadata.permissions().mode() & 0o111 == 0
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Boomux executable is not an absolute regular executable",
        ));
    }

    atomic_runtime_asset(&shim_dir.join("boomux.bashrc"), OPENCODE_BASH_RC, 0o600)?;
    atomic_runtime_asset(&shim_dir.join(".zshenv"), OPENCODE_ZSH_ENV, 0o600)?;
    atomic_runtime_asset(&shim_dir.join(".zshrc"), OPENCODE_ZSH_RC, 0o600)?;
    if real_opencode.is_some() {
        atomic_runtime_asset(&shim_dir.join("opencode"), OPENCODE_SHIM, 0o700)?;
        atomic_runtime_asset(&shim_dir.join("boomux-tui.tsx"), OPENCODE_TUI, 0o600)?;
        atomic_runtime_asset(
            &shim_dir.join("boomux-tui-core.js"),
            OPENCODE_TUI_CORE,
            0o600,
        )?;
        atomic_runtime_asset(
            &shim_dir.join("boomux-tui-runner.js"),
            OPENCODE_TUI_RUNNER,
            0o600,
        )?;
        atomic_runtime_asset(
            &shim_dir.join("tui.json"),
            b"{\n  \"$schema\": \"https://opencode.ai/tui.json\",\n  \"plugin\": [[\"./boomux-tui.tsx\", {}]]\n}\n",
            0o600,
        )?;
    }
    if real_claude.is_some() {
        atomic_runtime_asset(&shim_dir.join("claude"), CLAUDE_SHIM, 0o700)?;
    }
    if real_codex.is_some() {
        atomic_runtime_asset(&shim_dir.join("codex"), CODEX_SHIM, 0o700)?;
    }
    atomic_runtime_asset(&shim_dir.join("kiro-cli"), KIRO_SHIM, 0o700)?;

    let original_path = environment_value(environment, b"PATH").unwrap_or_default();
    let mut paths = vec![shim_dir.clone()];
    paths.extend(env::split_paths(&original_path));
    let prefixed_path = env::join_paths(paths)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
    let mut injected = environment.clone();
    set_environment_value(&mut injected, b"PATH", prefixed_path);
    set_environment_value(
        &mut injected,
        b"BOOMUX_SHIM_EXECUTABLE",
        boomux.into_os_string(),
    );
    if let Some(real_opencode) = real_opencode {
        set_environment_value(
            &mut injected,
            b"BOOMUX_REAL_OPENCODE",
            real_opencode.into_os_string(),
        );
    }
    if let Some(real_claude) = real_claude {
        set_environment_value(
            &mut injected,
            b"BOOMUX_REAL_CLAUDE",
            real_claude.into_os_string(),
        );
        set_environment_value(
            &mut injected,
            b"BOOMUX_CLAUDE_REMOTE_CONTROL",
            if claude_remote_control { "1" } else { "0" },
        );
    }
    if let Some(real_codex) = real_codex {
        set_environment_value(
            &mut injected,
            b"BOOMUX_REAL_CODEX",
            real_codex.into_os_string(),
        );
    }
    if let Some(real_kiro) = real_kiro {
        set_environment_value(
            &mut injected,
            b"BOOMUX_REAL_KIRO",
            real_kiro.into_os_string(),
        );
    }
    set_environment_value(&mut injected, b"BOOMUX_ORIGINAL_PATH", original_path);
    set_environment_value(
        &mut injected,
        b"BOOMUX_OPENCODE_SHIM_DIR",
        shim_dir.as_os_str(),
    );
    set_environment_value(
        &mut injected,
        b"BOOMUX_OPENCODE_TUI_CONFIG",
        shim_dir.join("tui.json").into_os_string(),
    );
    Ok(injected)
}

fn configure_opencode_shell_startup(
    client_shell: &std::ffi::OsStr,
    environment: &mut UnixEnvironment,
) -> Vec<std::ffi::OsString> {
    let Some(shim_dir) = environment_value(environment, b"BOOMUX_OPENCODE_SHIM_DIR") else {
        return Vec::new();
    };
    let shell_name = Path::new(client_shell).file_name();
    match shell_name.and_then(std::ffi::OsStr::to_str) {
        Some("bash") => vec![
            "--rcfile".into(),
            PathBuf::from(shim_dir)
                .join("boomux.bashrc")
                .into_os_string(),
        ],
        Some("zsh") => {
            let user_zdotdir = environment_value(environment, b"ZDOTDIR")
                .or_else(|| environment_value(environment, b"HOME"))
                .unwrap_or_else(|| "/".into());
            set_environment_value(environment, b"BOOMUX_USER_ZDOTDIR", user_zdotdir);
            set_environment_value(environment, b"ZDOTDIR", shim_dir);
            Vec::new()
        }
        Some("fish") => vec!["--init-command".into(), OPENCODE_FISH_INIT.into()],
        _ => Vec::new(),
    }
}

fn atomic_runtime_asset(path: &Path, content: &[u8], mode: u32) -> io::Result<()> {
    if let Ok(metadata) = fs::symlink_metadata(path)
        && (metadata.file_type().is_symlink() || !metadata.is_file())
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("runtime asset is not a regular file: {}", path.display()),
        ));
    }
    let directory = path.parent().ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput, "runtime asset has no parent")
    })?;
    let temporary = directory.join(format!(".asset-{}", Uuid::new_v4()));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(mode)
        .open(&temporary)?;
    let result = (|| {
        file.write_all(content)?;
        file.sync_all()?;
        fs::set_permissions(&temporary, fs::Permissions::from_mode(mode))?;
        fs::rename(&temporary, path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
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
    registry.reconcile_dead_kiro_holders()?;
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
        let presented_focused_terminal = registry.runtimes.presented_focused_terminal()?;
        let opencode_runtime = registry.opencode.prepare_handoff()?;
        let claude_remote_control_bindings = registry.export_claude_remote_control_bindings()?;
        let kiro_launch_holders = registry.export_kiro_launch_holders()?;
        launch_replacement_process(
            listener.as_fd(),
            daemon_lock.as_fd(),
            state_lock,
            &transfers,
            &exited,
            &event_stream,
            ReplacementOptions {
                focused_terminal,
                presented_focused_terminal,
                notification_settings,
                startup_environment,
                opencode_runtime,
                claude_remote_control_bindings,
                kiro_launch_holders,
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
    presented_focused_terminal: Option<QualifiedFocusedTerminalSnapshot>,
    notification_settings: Option<NotificationDeliverySettings>,
    startup_environment: Option<UnixEnvironment>,
    opencode_runtime: Option<OutgoingOpenCodeRuntime>,
    claude_remote_control_bindings: Vec<ClaudeRemoteControlBindingSnapshot>,
    kiro_launch_holders: Vec<handoff::KiroLaunchHolderManifest>,
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
        presented_focused_terminal,
        notification_settings,
        startup_environment,
        opencode_runtime,
        claude_remote_control_bindings,
        kiro_launch_holders,
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
                presented_focused_terminal,
                opencode_runtime: opencode_runtime
                    .as_ref()
                    .map(|runtime| runtime.manifest.clone()),
                claude_remote_control_bindings,
                kiro_launch_holders,
            },
        )?;
        send_descriptor(&channel, listener, handoff::LISTENER_MARKER)?;
        send_descriptor(&channel, runtime_lock, handoff::RUNTIME_LOCK_MARKER)?;
        send_descriptor(&channel, state_lock, handoff::STATE_LOCK_MARKER)?;
        if let Some(runtime) = &opencode_runtime {
            send_descriptor(
                &channel,
                runtime.pidfd.as_fd(),
                handoff::OPENCODE_PIDFD_MARKER,
            )?;
        }
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

    if let Request::ResumeNodeAgentSession {
        node_id: _,
        session_id: _,
        profile: _,
    } = request.message
    {
        return send_response(
            &mut stream,
            response_version,
            error_response(
                ErrorCode::UnsupportedVersion,
                "Agent Session resume has been removed",
            ),
        );
    }

    if let Request::ResumeAgentSession {
        session_id: _,
        profile: _,
    } = request.message
    {
        return send_response(
            &mut stream,
            response_version,
            error_response(
                ErrorCode::UnsupportedVersion,
                "Agent Session resume has been removed",
            ),
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
                collaborative: false,
            },
        );
    }
    if let Request::AttachCollaborative {
        shell_id,
        expected_run_id,
        profile,
    } = request.message
    {
        return registry.runtimes.handle_attach(
            stream,
            response_version,
            &registry,
            &shell_id,
            AttachRequestOptions {
                takeover: false,
                restart_exited: false,
                expected_run_id: Some(expected_run_id),
                profile,
                environment: None,
                owner_environment: false,
                collaborative: true,
            },
        );
    }
    let shutdown_identity = match &request.message {
        Request::Shutdown => Some(None),
        Request::ShutdownIfNodeIdentity { expected_node_id } => {
            Some(Some(expected_node_id.as_str()))
        }
        _ => None,
    };
    if let Some(expected_node_id) = shutdown_identity {
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
        if let Some(expected_node_id) = expected_node_id {
            match registry
                .node_identity()
                .and_then(|identity| identity.id().map_err(DaemonError::from))
            {
                Ok(node_id) if node_id == expected_node_id => {}
                Ok(_) => {
                    transition.store(TRANSITION_IDLE, Ordering::Release);
                    return send_response(
                        &mut stream,
                        response_version,
                        DaemonError::lifecycle(
                            ErrorCode::NodeIdentityChanged,
                            "daemon Node identity changed from the authorized uninstall target",
                        )
                        .into_response(),
                    );
                }
                Err(error) => {
                    transition.store(TRANSITION_IDLE, Ordering::Release);
                    return send_response(&mut stream, response_version, error.into_response());
                }
            }
        }
        match node_upgrade_maintenance_active(&registry) {
            Ok(true) => {
                transition.store(TRANSITION_IDLE, Ordering::Release);
                return send_response(
                    &mut stream,
                    response_version,
                    DaemonError::lifecycle(
                        ErrorCode::Busy,
                        "a registered Node upgrade is in progress",
                    )
                    .into_response(),
                );
            }
            Ok(false) => {}
            Err(error) => {
                transition.store(TRANSITION_IDLE, Ordering::Release);
                return send_response(&mut stream, response_version, error.into_response());
            }
        }
        registry.stop_node_projection_workers()?;
        return match registry.shutdown() {
            Ok(()) => {
                shutdown.store(true, Ordering::Release);
                send_response(&mut stream, response_version, Response::Ok)
            }
            Err(error) => {
                transition.store(TRANSITION_IDLE, Ordering::Release);
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
        match node_upgrade_maintenance_active(&registry) {
            Ok(true) => {
                transition.store(TRANSITION_IDLE, Ordering::Release);
                return send_response(
                    &mut stream,
                    response_version,
                    DaemonError::lifecycle(
                        ErrorCode::Busy,
                        "a registered Node upgrade is in progress",
                    )
                    .into_response(),
                );
            }
            Ok(false) => {}
            Err(error) => {
                transition.store(TRANSITION_IDLE, Ordering::Release);
                return send_response(&mut stream, response_version, error.into_response());
            }
        }
        if let Err(error) = registry.checkpoint_local_shell_transactions() {
            transition.store(TRANSITION_IDLE, Ordering::Release);
            return send_response(
                &mut stream,
                response_version,
                DaemonError::persistence_context(
                    error,
                    "could not checkpoint local Shell transactions before restart",
                )
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

    if let Request::BeginNodeUpgradeMaintenance {
        selector,
        expected_revision,
    } = &request.message
    {
        let response = registry
            .node_registrations()
            .and_then(|registrations| {
                registrations
                    .begin_upgrade_maintenance_if(
                        selector,
                        *expected_revision,
                        NODE_REGISTRATION_DRAIN_TIMEOUT,
                        NODE_UPGRADE_MAINTENANCE_LEASE,
                        || transition.load(Ordering::Acquire) == TRANSITION_IDLE,
                    )
                    .map_err(node_registration_error)
            })
            .map(|(registration, token)| Response::NodeUpgradeMaintenance {
                registration,
                token,
            })
            .unwrap_or_else(DaemonError::into_response);
        return send_response(&mut stream, response_version, response);
    }

    let response = match registry.dispatch_arc(request.message, response_version) {
        Ok(response) => response,
        Err(error) => error.into_response(),
    };
    let result = send_response(
        &mut stream,
        response_version,
        response_for_version(response, response_version),
    );
    if result.is_ok()
        && registry
            .local_shell_journal
            .as_ref()
            .is_some_and(|journal| journal.is_empty().is_ok_and(|empty| !empty))
    {
        let registry = Arc::clone(&registry);
        let delay = registry.local_shell_checkpoint_delay();
        thread::spawn(move || {
            thread::sleep(delay);
            if let Err(error) = registry.checkpoint_local_shell_transactions() {
                eprintln!("boomux: local Shell transaction checkpoint failed: {error}");
            }
        });
    }
    result
}

fn validate_notification_delivery_settings(
    _settings: &NotificationDeliverySettings,
) -> io::Result<()> {
    Ok(())
}

fn validate_opencode_uuid(label: &str, value: &str) -> DaemonResult<()> {
    Uuid::parse_str(value)
        .map(|_| ())
        .map_err(|_| DaemonError::validation(format!("{label} must be a UUID")))
}

fn validate_opencode_claim_id(label: &str, value: &str) -> DaemonResult<()> {
    validate_required_agent_string(label, value, MAX_NAME_BYTES).map_err(DaemonError::from)
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

fn node_upgrade_maintenance_active(registry: &DaemonService) -> Result<bool, DaemonError> {
    registry
        .node_registrations
        .as_ref()
        .map(NodeRegistrationManager::has_active_upgrade_maintenance)
        .transpose()
        .map(|active| active.unwrap_or(false))
        .map_err(node_registration_error)
}

fn global_workspace_error(error: io::Error) -> DaemonError {
    if error.kind() == io::ErrorKind::InvalidInput && error.to_string().contains("revision changed")
    {
        DaemonError::lifecycle(ErrorCode::RevisionChanged, error.to_string())
    } else {
        DaemonError::from(error)
    }
}

fn response_for_version(response: Response, version: u32) -> Response {
    let mut response = response;
    if !protocol::ProtocolFeature::WorkspaceSessionHiding.is_supported_by(version)
        && let Response::Events { events, .. } = &mut response
    {
        events.retain(|event| !matches!(event.kind, DaemonEventKind::AgentSessionHidden { .. }));
    }
    if !protocol::ProtocolFeature::SessionExtendedPresentation.is_supported_by(version) {
        match &mut response {
            Response::HostService {
                result: HostServiceResult::AgentSessions { sessions },
            } => {
                for session in sessions {
                    session.latest_agent_name = None;
                    for context in &mut session.working_contexts {
                        context.push_status = None;
                        context.worktree_status = None;
                    }
                }
            }
            Response::HostService {
                result: HostServiceResult::AgentSession { session },
            } => {
                session.summary.latest_agent_name = None;
                for context in &mut session.summary.working_contexts {
                    context.push_status = None;
                    context.worktree_status = None;
                }
            }
            Response::HostService {
                result: HostServiceResult::ResolvedAgentSession { session },
            } => {
                session.latest_agent_name = None;
                for context in &mut session.working_contexts {
                    context.push_status = None;
                    context.worktree_status = None;
                }
            }
            _ => {}
        }
    }
    if !protocol::ProtocolFeature::SessionDisplayNames.is_supported_by(version) {
        match &mut response {
            Response::HostService {
                result: HostServiceResult::AgentSessions { sessions },
            } => {
                for session in sessions {
                    session.user_display_name = None;
                    session.workspace_revision = 0;
                }
            }
            Response::HostService {
                result: HostServiceResult::AgentSession { session },
            } => {
                session.summary.user_display_name = None;
                session.summary.workspace_revision = 0;
                session.projected_occurrences.clear();
            }
            Response::HostService {
                result: HostServiceResult::ResolvedAgentSession { session },
            } => {
                session.user_display_name = None;
                session.workspace_revision = 0;
            }
            Response::Events { events, .. } => events.retain(|event| {
                !matches!(
                    event.kind,
                    DaemonEventKind::AgentSessionDisplayNameChanged { .. }
                )
            }),
            _ => {}
        }
    }
    if !protocol::ProtocolFeature::SessionPresentationContext.is_supported_by(version) {
        match &mut response {
            Response::HostService {
                result: HostServiceResult::AgentSessions { sessions },
            } => {
                for session in sessions {
                    session.attentions.clear();
                    session.git_branch = None;
                    session.working_contexts.clear();
                    session.working_context_count = 0;
                }
            }
            Response::HostService {
                result: HostServiceResult::AgentSession { session },
            } => {
                session.summary.attentions.clear();
                session.summary.git_branch = None;
                session.summary.working_contexts.clear();
                session.summary.working_context_count = 0;
            }
            Response::HostService {
                result: HostServiceResult::ResolvedAgentSession { session },
            } => {
                session.attentions.clear();
                session.git_branch = None;
                session.working_contexts.clear();
                session.working_context_count = 0;
            }
            Response::NodeProjectionSync { sync } => sync.transitions.retain(|transition| {
                !matches!(
                    transition.kind,
                    NodeProjectionTransitionKind::SessionContext { .. }
                )
            }),
            Response::Events { events, .. } => events.retain(|event| {
                !matches!(
                    event.kind,
                    DaemonEventKind::AgentWorkingContextObserved { .. }
                )
            }),
            _ => {}
        }
        visit_response_agents(&mut response, &mut |agent| agent.working_contexts.clear());
    }
    if !protocol::ProtocolFeature::QualifiedFocusedTerminal.is_supported_by(version) {
        match &mut response {
            Response::CombinedNodeSnapshot { snapshot } => snapshot.focused_terminal = None,
            Response::Events { events, .. } => events.retain(|event| {
                !matches!(
                    event.kind,
                    DaemonEventKind::FocusedTerminalPresentationChanged
                )
            }),
            _ => {}
        }
    }
    if !protocol::ProtocolFeature::RecoveredAgentPresentation.is_supported_by(version) {
        remove_recovered_agent_presentation(&mut response);
    }
    if !protocol::ProtocolFeature::ObservedNodeHelperVersion.is_supported_by(version) {
        match &mut response {
            Response::CombinedNodeSnapshot { snapshot } => {
                for node in &mut snapshot.nodes {
                    node.observed_helper_version = None;
                }
            }
            Response::NodeProjectionHealth { health } => {
                health.observed_helper_version = None;
            }
            _ => {}
        }
    }
    if !protocol::ProtocolFeature::GlobalWorkspaces.is_supported_by(version) {
        match &mut response {
            Response::CombinedNodeSnapshot { snapshot } => {
                snapshot.workspaces.clear();
                snapshot.external_workspaces.clear();
                for node in &mut snapshot.nodes {
                    node.route = None;
                    node.registration_revision = None;
                    node.workspace_owner_eligible = false;
                    node.workspace_owner_unavailable_reason = None;
                }
            }
            Response::NodeProjectionSync { sync } => sync.capabilities.clear(),
            _ => {}
        }
    }
    if !protocol::ProtocolFeature::WorkspacePlacementDefaultCwd.is_supported_by(version)
        && let Response::Events { events, .. } = &mut response
    {
        events.retain(|event| {
            !matches!(
                event.kind,
                DaemonEventKind::WorkspaceDefaultCwdChanged { .. }
            )
        });
    }
    if !protocol::ProtocolFeature::NodeProjectionSync.is_supported_by(version)
        && let Response::Events { events, .. } = &mut response
    {
        events.retain(|event| !matches!(event.kind, DaemonEventKind::NodeProjectionChanged { .. }));
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
                        | DaemonEventKind::AgentWorkingContextObserved { .. }
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

fn remove_recovered_agent_presentation(response: &mut Response) {
    let clear_shell = |shell: &mut ShellSnapshot| {
        if matches!(shell.status, ShellStatus::Pending) {
            shell.run = None;
            shell.recovered_agent_id = None;
        }
    };
    let clear_workspace = |workspace: &mut WorkspaceSnapshot| {
        for shell in &mut workspace.shells {
            clear_shell(shell);
        }
    };
    let clear_projection = |projection: &mut NodeProjectionSnapshot| {
        for shell in &mut projection.shells {
            if matches!(shell.status, ShellStatus::Pending) {
                shell.run_id = None;
                shell.generation = None;
                shell.started_at_ms = None;
                shell.ended_at_ms = None;
                shell.recovered_agent_id = None;
            }
        }
    };
    let clear_routed = |result: &mut RoutedOperationResult| match result {
        RoutedOperationResult::Workspace { workspace } => clear_workspace(workspace),
        RoutedOperationResult::Shell { shell } => clear_shell(shell),
        _ => {}
    };
    match response {
        Response::Snapshot { snapshot } => {
            for workspace in &mut snapshot.workspaces {
                clear_workspace(workspace);
            }
        }
        Response::Workspace { workspace } => clear_workspace(workspace),
        Response::Shell { shell } => clear_shell(shell),
        Response::Events {
            snapshot: Some(snapshot),
            ..
        } => {
            for workspace in &mut snapshot.workspaces {
                clear_workspace(workspace);
            }
        }
        Response::NodeProjectionSync { sync } => clear_projection(&mut sync.projection),
        Response::GlobalWorkspaceResource { resource, .. }
        | Response::RoutedNodeOperation { result: resource } => clear_routed(resource),
        Response::CombinedNodeSnapshot { snapshot } => {
            for node in &mut snapshot.nodes {
                if let Some(local) = &mut node.local_snapshot {
                    for workspace in &mut local.workspaces {
                        clear_workspace(workspace);
                    }
                }
                if let Some(remote) = &mut node.remote_projection {
                    clear_projection(remote);
                }
            }
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
        | DaemonEventKind::AgentAttentionAcknowledged { shell_id, .. }
        | DaemonEventKind::AgentWorkingContextObserved { shell_id, .. } => Some(shell_id),
        _ => None,
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

fn apply_session_visibility_limit(
    sessions: &mut Vec<crate::session_projection::SessionProjection>,
    hidden: &[crate::session_projection::HiddenSessionMetadata],
    requester_version: u32,
) {
    if protocol::ProtocolFeature::WorkspaceSessionHiding.is_supported_by(requester_version) {
        crate::session_projection::filter_hidden(sessions, hidden);
    }
    sessions.truncate(MAX_HOST_SERVICE_SESSIONS);
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
        | Response::AgentAttentionAcknowledged { agent, .. }
        | Response::AgentWorkingContext { agent, .. } => visitor(agent),
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
                    | DaemonEventKind::AgentAttentionAcknowledged { agent, .. }
                    | DaemonEventKind::AgentWorkingContextObserved { agent, .. } => visitor(agent),
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
    )
}

fn routed_result(response: Response) -> Result<RoutedOperationResult, Box<Response>> {
    match response {
        Response::Workspace { workspace } => Ok(RoutedOperationResult::Workspace { workspace }),
        Response::Shell { shell } => Ok(RoutedOperationResult::Shell { shell }),
        Response::Launcher { launcher } => Ok(RoutedOperationResult::Launcher { launcher }),
        Response::Agent { agent } => Ok(RoutedOperationResult::Agent { agent }),
        Response::AgentAttentionAcknowledged { agent, changed } => {
            Ok(RoutedOperationResult::AgentAttentionAcknowledged { agent, changed })
        }
        Response::AgentSessionDisplayName { outcome } => {
            Ok(RoutedOperationResult::AgentSessionDisplayName { outcome })
        }
        Response::AgentSessionHidden { outcome } => {
            Ok(RoutedOperationResult::AgentSessionHidden { outcome })
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
            RoutedOperation::SetWorkspaceDefaultCwd {
                default_cwd,
                expected_revision,
                ..
            },
            Response::Workspace { workspace },
        ) => {
            workspace.default_cwd.as_ref() == Some(default_cwd)
                && workspace.revision >= *expected_revision
        }
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
            | RoutedOperation::RemoveLauncher { .. },
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
            | RoutedOperation::RemoveLauncher { .. },
            Response::Error { .. },
        ) => Some(RoutedOperationResult::Ok),
        (_, response) => routed_result(response).ok(),
    }
}

fn send_registered_node_request(
    registration: &crate::protocol::NodeRegistrationSnapshot,
    request: Request,
) -> io::Result<Response> {
    send_registered_node_request_for_version(registration, request, protocol::PROTOCOL_VERSION)
}

fn send_registered_node_request_for_version(
    registration: &crate::protocol::NodeRegistrationSnapshot,
    request: Request,
    requester_version: u32,
) -> io::Result<Response> {
    send_registered_node_request_with_timeout_for_version(
        registration,
        request,
        REGISTERED_NODE_RESPONSE_TIMEOUT,
        None,
        requester_version,
    )
}

fn send_registered_node_request_with_timeout(
    registration: &crate::protocol::NodeRegistrationSnapshot,
    request: Request,
    response_timeout: Duration,
    route_feature: Option<protocol::ProtocolFeature>,
) -> io::Result<Response> {
    send_registered_node_request_with_timeout_for_version(
        registration,
        request,
        response_timeout,
        route_feature,
        protocol::PROTOCOL_VERSION,
    )
}

fn send_registered_node_request_with_timeout_for_version(
    registration: &crate::protocol::NodeRegistrationSnapshot,
    request: Request,
    response_timeout: Duration,
    route_feature: Option<protocol::ProtocolFeature>,
    requester_version: u32,
) -> io::Result<Response> {
    send_registered_node_request_with_timeout_for_version_after_handshake(
        registration,
        request,
        response_timeout,
        route_feature,
        requester_version,
        &mut || Ok(()),
    )
}

fn send_registered_node_request_with_timeout_for_version_after_handshake(
    registration: &crate::protocol::NodeRegistrationSnapshot,
    request: Request,
    response_timeout: Duration,
    route_feature: Option<protocol::ProtocolFeature>,
    requester_version: u32,
    before_request: &mut dyn FnMut() -> io::Result<()>,
) -> io::Result<Response> {
    let target = SshTarget::parse(registration.target.clone())?;
    let mut bootstrap = ssh_bootstrap::BootstrapSession::open(
        target,
        SshAuthenticationMode::Batch,
        Duration::from_secs(2),
    )?;
    let helper = match bootstrap.plan(Duration::from_secs(2))? {
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
    let forwarded_version = requester_version.min(helper.handshake.core_protocol_version);
    prepare_supported_owner_request(
        forwarded_version,
        route_feature.or_else(|| request.required_feature()),
        before_request,
    )?;
    let mut remote = bootstrap.connect(helper, Duration::from_secs(2))?;
    remote.request_at_version(request, response_timeout, forwarded_version)
}

fn prepare_supported_owner_request(
    protocol_version: u32,
    feature: Option<protocol::ProtocolFeature>,
    before_request: &mut dyn FnMut() -> io::Result<()>,
) -> io::Result<()> {
    if let Some(feature) = feature
        && !feature.is_supported_by(protocol_version)
    {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            format!("remote Node does not support {}", feature.requirement()),
        ));
    }
    before_request()
}

fn routed_response_timeout(operation: &RoutedOperation) -> Duration {
    let _ = operation;
    Duration::from_secs(2)
}

fn routed_owner_feature(operation: &RoutedOperation) -> Option<protocol::ProtocolFeature> {
    if matches!(
        operation,
        RoutedOperation::SetAgentSessionDisplayName { .. }
    ) {
        return Some(protocol::ProtocolFeature::SessionDisplayNames);
    }
    if matches!(operation, RoutedOperation::HideAgentSession { .. }) {
        return Some(protocol::ProtocolFeature::WorkspaceSessionHiding);
    }
    if matches!(operation, RoutedOperation::SetWorkspaceDefaultCwd { .. }) {
        return Some(protocol::ProtocolFeature::WorkspacePlacementDefaultCwd);
    }
    if matches!(
        operation,
        RoutedOperation::CreateWorkspaceShell { .. }
            | RoutedOperation::CreateWorkspaceLauncher { .. }
    ) {
        return Some(protocol::ProtocolFeature::GlobalWorkspaces);
    }
    None
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

fn default_cwd_owner_error_is_ambiguous(code: Option<ErrorCode>) -> bool {
    matches!(
        code,
        Some(ErrorCode::OutcomeUnknown | ErrorCode::PersistenceFailed | ErrorCode::Timeout)
    )
}

fn request_fingerprint(value: &impl Serialize) -> DaemonResult<String> {
    let bytes = serde_json::to_vec(value).map_err(|error| {
        DaemonError::Internal(io::Error::other(format!(
            "could not fingerprint Workspace operation: {error}"
        )))
    })?;
    Ok(format!("{:x}", Sha256::digest(bytes)))
}

struct WorkspaceRequestFingerprint {
    digest: String,
    bytes: usize,
}

fn workspace_operation_fingerprints(
    request: &Request,
) -> DaemonResult<(Option<WorkspaceRequestFingerprint>, Option<String>)> {
    let exact = matches!(
        request,
        Request::CreateGlobalWorkspaceShell { .. }
            | Request::CreateGlobalWorkspaceWithShell { .. }
            | Request::CreateGlobalWorkspaceLauncher { .. }
    )
    .then(|| {
        let bytes = serde_json::to_vec(request).map_err(|error| {
            DaemonError::Internal(io::Error::other(format!(
                "could not fingerprint Workspace operation: {error}"
            )))
        })?;
        Ok::<_, DaemonError>(WorkspaceRequestFingerprint {
            digest: format!("{:x}", Sha256::digest(&bytes)),
            bytes: bytes.len(),
        })
    })
    .transpose()?;
    let semantic = match request {
        Request::CreateGlobalWorkspaceWithShell {
            name,
            node_id,
            default_cwd,
            shell,
            ..
        } => Some(request_fingerprint(&(
            "project_with_shell",
            name,
            node_id,
            default_cwd,
            shell,
        ))?),
        _ => None,
    };
    Ok((exact, semantic))
}

fn workspace_pre_owner_failure_is_ambiguous(error: &DaemonError) -> bool {
    matches!(
        error.wire_code(),
        ErrorCode::OutcomeUnknown | ErrorCode::PersistenceFailed | ErrorCode::Timeout
    )
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

struct DaemonService {
    node_identity: Option<Arc<NodeIdentityManager>>,
    node_registrations: Option<NodeRegistrationManager>,
    node_projection_cache: Option<NodeProjectionCache>,
    global_workspaces: Option<GlobalWorkspaceStore>,
    local_shell_journal: Option<LocalShellJournal>,
    durable: DurableRegistry,
    events: EventStream,
    runtimes: ShellRuntimeManager,
    opencode: OpenCodeCoordinator,
    kiro: KiroLaunchHolders,
    claude_remote_control: ClaudeRemoteControlBindings,
    remote_attachments: RemoteAttachmentManager,
    host_service_previews: Mutex<HashMap<String, HostServicePreview>>,
    host_session_catalog: HostSessionCatalogCache,
    workspace_operation_locks: Mutex<HashMap<String, Weak<Mutex<()>>>>,
    mutation_lock: Mutex<()>,
    notification_settings: NotificationDeliverySettings,
    notification_sink: Arc<dyn NotificationSink>,
    startup_environment: UnixEnvironment,
    node_projection_workers: NodeProjectionWorkers,
    #[cfg(test)]
    fail_after_mutation: AtomicBool,
}

struct HostServicePreview {
    created_at: Instant,
    prepared: PreparedIntegrationMutation,
}

#[derive(Default)]
struct HostSessionCatalogCache {
    state: Mutex<HostSessionCatalogState>,
    refreshed: Condvar,
}

#[derive(Default)]
struct HostSessionCatalogState {
    entries: HashMap<crate::host_session_titles::ProjectionRequest, CachedHostSessionCatalog>,
    refreshing: bool,
    use_counter: u64,
}

struct CachedHostSessionCatalog {
    sessions: Option<Vec<crate::host_session_titles::HostSession>>,
    inspected_at: Instant,
    last_used: u64,
}

impl HostSessionCatalogCache {
    fn records(
        &self,
        requests: &[crate::host_session_titles::ProjectionRequest],
    ) -> io::Result<Vec<crate::host_session_titles::HostSession>> {
        self.records_with(
            requests,
            &crate::host_session_titles::projection_records_batch,
        )
    }

    fn records_with(
        &self,
        requests: &[crate::host_session_titles::ProjectionRequest],
        discover: &impl Fn(
            &[crate::host_session_titles::ProjectionRequest],
        ) -> Vec<Option<Vec<crate::host_session_titles::HostSession>>>,
    ) -> io::Result<Vec<crate::host_session_titles::HostSession>> {
        loop {
            let mut state = lock(&self.state)?;
            let now = Instant::now();
            let stale = requests
                .iter()
                .filter(|request| {
                    state.entries.get(*request).is_none_or(|entry| {
                        let ttl = if entry.sessions.is_some() {
                            HOST_SESSION_CATALOG_TTL
                        } else {
                            HOST_SESSION_CATALOG_FAILURE_TTL
                        };
                        now.duration_since(entry.inspected_at) >= ttl
                    })
                })
                .cloned()
                .collect::<Vec<_>>();
            if stale.is_empty() {
                state.use_counter = state.use_counter.saturating_add(1);
                let used = state.use_counter;
                let mut sessions = Vec::new();
                for request in requests {
                    if let Some(entry) = state.entries.get_mut(request) {
                        entry.last_used = used;
                        if let Some(records) = &entry.sessions {
                            sessions.extend(records.clone());
                        }
                    }
                }
                return Ok(sessions);
            }
            if state.refreshing {
                state = self
                    .refreshed
                    .wait(state)
                    .map_err(|_| io::Error::other("Session catalog cache lock poisoned"))?;
                drop(state);
                continue;
            }
            state.refreshing = true;
            drop(state);

            let discovered = discover(&stale);
            let mut state = lock(&self.state)?;
            state.use_counter = state.use_counter.saturating_add(1);
            let used = state.use_counter;
            let inspected_at = Instant::now();
            for (request, sessions) in stale.into_iter().zip(discovered) {
                state.entries.insert(
                    request,
                    CachedHostSessionCatalog {
                        sessions,
                        inspected_at,
                        last_used: used,
                    },
                );
            }
            while state.entries.len() > MAX_HOST_SESSION_CATALOG_ENTRIES {
                let Some(oldest) = state
                    .entries
                    .iter()
                    .min_by_key(|(_, entry)| entry.last_used)
                    .map(|(request, _)| request.clone())
                else {
                    break;
                };
                state.entries.remove(&oldest);
            }
            state.refreshing = false;
            self.refreshed.notify_all();
        }
    }

    fn cached_records(&self) -> io::Result<Option<Vec<crate::host_session_titles::HostSession>>> {
        let state = lock(&self.state)?;
        if state.entries.is_empty() {
            return Ok(None);
        }
        Ok(Some(
            state
                .entries
                .values()
                .filter_map(|entry| entry.sessions.as_ref())
                .flatten()
                .cloned()
                .collect(),
        ))
    }
}

#[derive(Default)]
struct ClaudeRemoteControlBindings {
    state: Mutex<HashMap<String, ClaudeRemoteControlBindingSnapshot>>,
}

#[derive(Default)]
struct KiroLaunchHolders {
    state: Mutex<HashMap<String, KiroLaunchHolder>>,
}

#[derive(Clone)]
struct KiroLaunchHolder {
    pid: u32,
    start_time: u64,
    process_group_leader: bool,
    shell_id: String,
    run_id: String,
    sessions: HashMap<String, String>,
}

struct KiroHoldersMutation<'a> {
    state: MutexGuard<'a, HashMap<String, KiroLaunchHolder>>,
    previous: Option<HashMap<String, KiroLaunchHolder>>,
}

impl KiroHoldersMutation<'_> {
    fn new(state: MutexGuard<'_, HashMap<String, KiroLaunchHolder>>) -> KiroHoldersMutation<'_> {
        let previous = state.clone();
        KiroHoldersMutation {
            state,
            previous: Some(previous),
        }
    }

    fn commit(mut self) {
        self.previous = None;
    }
}

impl Drop for KiroHoldersMutation<'_> {
    fn drop(&mut self) {
        if let Some(previous) = self.previous.take() {
            *self.state = previous;
        }
    }
}

#[derive(Default)]
struct OpenCodeCoordinator {
    state: Mutex<OpenCodeCoordinatorState>,
}

#[derive(Default)]
struct OpenCodeCoordinatorState {
    runtime: Option<OpenCodeRuntime>,
    claims: HashMap<String, OpenCodeRootClaim>,
}

struct OpenCodeRuntime {
    generation_id: String,
    port: u16,
    pid: u32,
    process: OpenCodeRuntimeProcess,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct OpenCodeRuntimeRegistration {
    generation_id: String,
    pid: u32,
    port: u16,
    start_time: u64,
    executable: Vec<u8>,
    argv: Vec<Vec<u8>>,
}

enum OpenCodeRuntimeProcess {
    Owned(StdChild),
    Imported(ImportedProcess),
}

#[derive(Clone)]
struct OpenCodeRootClaim {
    claim_id: String,
    workspace_id: String,
    shell_id: String,
    run_id: String,
    agent_id: String,
    selected_holder_id: String,
    holders: HashMap<String, OpenCodeClaimHolder>,
}

#[derive(Clone)]
struct OpenCodeClaimHolder {
    expires_at: Instant,
    expires_at_ms: u64,
}

struct OpenCodeClaimsMutation<'a> {
    state: MutexGuard<'a, OpenCodeCoordinatorState>,
    previous: Option<HashMap<String, OpenCodeRootClaim>>,
}

impl OpenCodeClaimsMutation<'_> {
    fn new(state: MutexGuard<'_, OpenCodeCoordinatorState>) -> OpenCodeClaimsMutation<'_> {
        let previous = state.claims.clone();
        OpenCodeClaimsMutation {
            state,
            previous: Some(previous),
        }
    }

    fn commit(mut self) {
        self.previous = None;
    }
}

impl Drop for OpenCodeClaimsMutation<'_> {
    fn drop(&mut self) {
        if let Some(previous) = self.previous.take() {
            self.state.claims = previous;
        }
    }
}

struct OutgoingOpenCodeRuntime {
    manifest: handoff::OpenCodeRuntimeManifest,
    pidfd: OwnedFd,
}

impl OpenCodeRuntimeProcess {
    fn has_exited(&mut self) -> io::Result<bool> {
        match self {
            Self::Owned(child) => child.try_wait().map(|status| status.is_some()),
            Self::Imported(process) => process.has_exited(),
        }
    }

    fn pidfd(&mut self, pid: u32) -> io::Result<OwnedFd> {
        match self {
            Self::Owned(child) => {
                if child.try_wait()?.is_some() {
                    return Err(io::Error::new(
                        io::ErrorKind::NotFound,
                        "OpenCode shared runtime exited",
                    ));
                }
                open_pidfd(pid)
            }
            Self::Imported(process) => process.pidfd.try_clone(),
        }
    }

    fn wait(&mut self) -> io::Result<()> {
        match self {
            Self::Owned(child) => child.wait().map(|_| ()),
            Self::Imported(process) => process.wait(),
        }
    }
}

impl OpenCodeCoordinatorState {
    fn reap_exited(&mut self) -> io::Result<()> {
        let exited = match self.runtime.as_mut() {
            Some(runtime) => runtime.process.has_exited()?,
            None => false,
        };
        if exited {
            self.runtime = None;
            self.claims.clear();
        }
        Ok(())
    }

    fn prune_claims(&mut self, now: Instant) {
        self.claims.retain(|_, claim| {
            claim.holders.retain(|_, holder| holder.expires_at > now);
            if !claim.holders.contains_key(&claim.selected_holder_id) {
                claim.selected_holder_id = claim.holders.keys().min().cloned().unwrap_or_default();
            }
            !claim.holders.is_empty()
        });
    }

    fn require_generation(&mut self, generation_id: &str) -> DaemonResult<()> {
        self.reap_exited()?;
        if self
            .runtime
            .as_ref()
            .is_some_and(|runtime| runtime.generation_id == generation_id)
        {
            Ok(())
        } else {
            Err(DaemonError::lifecycle(
                ErrorCode::NotFound,
                "OpenCode shared runtime generation is not active",
            ))
        }
    }

    fn holder_count(&self) -> usize {
        self.claims.values().map(|claim| claim.holders.len()).sum()
    }

    fn snapshot(
        &self,
        generation_id: &str,
        root_session_id: &str,
        holder_id: &str,
    ) -> io::Result<OpenCodeSessionClaimSnapshot> {
        let claim = self
            .claims
            .get(root_session_id)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "OpenCode claim not found"))?;
        let holder = claim.holders.get(holder_id).ok_or_else(|| {
            io::Error::new(io::ErrorKind::NotFound, "OpenCode claim holder not found")
        })?;
        Ok(OpenCodeSessionClaimSnapshot {
            generation_id: generation_id.into(),
            claim_id: claim.claim_id.clone(),
            holder_id: holder_id.into(),
            root_session_id: root_session_id.into(),
            workspace_id: claim.workspace_id.clone(),
            shell_id: claim.shell_id.clone(),
            run_id: claim.run_id.clone(),
            agent_id: claim.agent_id.clone(),
            holder_count: u32::try_from(claim.holders.len()).unwrap_or(u32::MAX),
            holder_expires_at_ms: holder.expires_at_ms,
        })
    }
}

impl OpenCodeCoordinator {
    fn reap_exited(&self) -> io::Result<()> {
        lock(&self.state)?.reap_exited()
    }

    fn ensure_runtime(
        &self,
        port: u16,
        environment: Option<&UnixEnvironment>,
    ) -> DaemonResult<OpenCodeSharedRuntimeSnapshot> {
        if port == 0 {
            return Err(DaemonError::validation(
                "OpenCode shared runtime port must be nonzero",
            ));
        }
        let mut state = lock(&self.state)?;
        state.reap_exited()?;
        if let Some(runtime) = &state.runtime {
            if runtime.port != port {
                return Err(DaemonError::lifecycle(
                    ErrorCode::Busy,
                    "OpenCode shared runtime is active on a different port",
                ));
            }
            return Ok(Self::runtime_snapshot(runtime));
        }
        if let Some(runtime) = adopt_registered_opencode_runtime()? {
            let adopted_port = runtime.port;
            state.claims.clear();
            state.runtime = Some(runtime);
            if adopted_port != port {
                return Err(DaemonError::lifecycle(
                    ErrorCode::Busy,
                    "OpenCode shared runtime is active on a different port",
                ));
            }
            return Ok(Self::runtime_snapshot(
                state
                    .runtime
                    .as_ref()
                    .expect("adopted runtime was inserted"),
            ));
        }
        if let Some(runtime) = discover_unregistered_opencode_runtime(port)? {
            state.claims.clear();
            state.runtime = Some(runtime);
            return Ok(Self::runtime_snapshot(
                state
                    .runtime
                    .as_ref()
                    .expect("discovered runtime was inserted"),
            ));
        }
        let probe = TcpListener::bind(("127.0.0.1", port)).map_err(|error| {
            DaemonError::lifecycle(
                ErrorCode::Busy,
                format!("OpenCode shared runtime port {port} is unavailable: {error}"),
            )
        })?;
        drop(probe);

        if let Some(environment) = environment {
            validate_unix_environment(environment)?;
        }
        let inherited_environment = environment
            .cloned()
            .unwrap_or_else(capture_current_environment);
        let sanitized_environment = sanitize_opencode_shim_environment(&inherited_environment);
        let executable =
            resolve_opencode_executable(&sanitized_environment, None).ok_or_else(|| {
                DaemonError::lifecycle(
                    ErrorCode::NotFound,
                    "OpenCode executable is unavailable outside the Boomux shim",
                )
            })?;
        let generation_id = Uuid::new_v4().to_string();
        let port_argument = port.to_string();
        let mut command = Command::new(&executable);
        command
            .args(["serve", "--hostname", "127.0.0.1", "--port", &port_argument])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .env_clear();
        for variable in sanitized_environment.variables {
            if !variable.name.starts_with(b"BOOMUX_") {
                command.env(
                    std::ffi::OsString::from_vec(variable.name),
                    std::ffi::OsString::from_vec(variable.value),
                );
            }
        }
        command.env("BOOMUX_OPENCODE_SHARED_GENERATION", &generation_id);
        // The child has not executed user code; setsid creates the process-tree
        // boundary used by explicit daemon shutdown.
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() == -1 {
                    Err(io::Error::last_os_error())
                } else {
                    Ok(())
                }
            });
        }
        let mut child = command.spawn()?;
        let pid = child.id();
        let deadline = Instant::now() + OPENCODE_RUNTIME_READINESS_TIMEOUT;
        loop {
            if child.try_wait()?.is_some() {
                return Err(DaemonError::lifecycle(
                    ErrorCode::Busy,
                    "OpenCode shared runtime exited before becoming ready",
                ));
            }
            if TcpStream::connect(("127.0.0.1", port)).is_ok()
                && child.try_wait()?.is_none()
                && opencode_listener_belongs_to_session(port, pid)
            {
                break;
            }
            if Instant::now() >= deadline {
                signal_session(pid as libc::pid_t, libc::SIGKILL);
                let _ = child.kill();
                let _ = child.wait();
                return Err(DaemonError::lifecycle(
                    ErrorCode::Timeout,
                    "OpenCode shared runtime did not become ready",
                ));
            }
            thread::sleep(Duration::from_millis(10));
        }
        if let Err(error) = write_opencode_runtime_registration(&generation_id, pid, port) {
            signal_session(pid as libc::pid_t, libc::SIGKILL);
            let _ = child.kill();
            let _ = child.wait();
            return Err(error.into());
        }
        state.claims.clear();
        state.runtime = Some(OpenCodeRuntime {
            generation_id,
            port,
            pid,
            process: OpenCodeRuntimeProcess::Owned(child),
        });
        Ok(Self::runtime_snapshot(
            state.runtime.as_ref().expect("runtime was inserted"),
        ))
    }

    fn get_runtime(&self) -> DaemonResult<Option<OpenCodeSharedRuntimeSnapshot>> {
        let mut state = lock(&self.state)?;
        state.reap_exited()?;
        Ok(state.runtime.as_ref().map(Self::runtime_snapshot))
    }

    fn runtime_snapshot(runtime: &OpenCodeRuntime) -> OpenCodeSharedRuntimeSnapshot {
        OpenCodeSharedRuntimeSnapshot {
            generation_id: runtime.generation_id.clone(),
            url: format!("http://127.0.0.1:{}", runtime.port),
            port: runtime.port,
            pid: Some(runtime.pid),
        }
    }

    fn prepare_handoff(&self) -> io::Result<Option<OutgoingOpenCodeRuntime>> {
        let mut state = lock(&self.state)?;
        state.reap_exited()?;
        state
            .runtime
            .as_mut()
            .map(|runtime| {
                Ok(OutgoingOpenCodeRuntime {
                    manifest: handoff::OpenCodeRuntimeManifest {
                        generation_id: runtime.generation_id.clone(),
                        port: runtime.port,
                        pid: runtime.pid,
                    },
                    pidfd: runtime.process.pidfd(runtime.pid)?,
                })
            })
            .transpose()
    }

    fn import_handoff(
        &self,
        transferred: Option<handoff::TransferredOpenCodeRuntime>,
    ) -> io::Result<()> {
        let mut state = lock(&self.state)?;
        state.runtime = transferred
            .map(|transferred| {
                let manifest = transferred.manifest;
                let stat = fs::read_to_string(format!("/proc/{}/stat", manifest.pid))?;
                if proc_session_id(&stat) != Some(manifest.pid as libc::pid_t) {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "transferred OpenCode process is not its session leader",
                    ));
                }
                let process = ImportedProcess {
                    pid: manifest.pid,
                    pidfd: transferred.pidfd,
                };
                if process.has_exited()? {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "transferred OpenCode process exited before import",
                    ));
                }
                Ok(OpenCodeRuntime {
                    generation_id: manifest.generation_id,
                    port: manifest.port,
                    pid: manifest.pid,
                    process: OpenCodeRuntimeProcess::Imported(process),
                })
            })
            .transpose()?;
        state.claims.clear();
        Ok(())
    }

    fn shutdown(&self) -> io::Result<()> {
        let mut state = lock(&self.state)?;
        let Some(mut runtime) = state.runtime.take() else {
            state.claims.clear();
            remove_opencode_runtime_registration()?;
            return Ok(());
        };
        state.claims.clear();
        signal_session(runtime.pid as libc::pid_t, libc::SIGKILL);
        match &mut runtime.process {
            OpenCodeRuntimeProcess::Owned(child) => {
                if child.try_wait()?.is_none() {
                    child.kill()?;
                }
            }
            OpenCodeRuntimeProcess::Imported(process) => process.send_signal(libc::SIGKILL)?,
        }
        let waited = runtime.process.wait();
        let removed = remove_opencode_runtime_registration();
        waited.and(removed)
    }
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

#[derive(Default)]
struct NodeProjectionWorkers {
    stop: Arc<AtomicBool>,
    handles: Mutex<HashMap<String, thread::JoinHandle<()>>>,
    wake: Mutex<HashSet<String>>,
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
    presented_revision: u64,
    presented_terminal: Option<QualifiedFocusedTerminalSnapshot>,
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
    RegisteredAgent {
        workspace: Arc<Workspace>,
        agent: Arc<AgentInstance>,
    },
    RenamedWorkspace {
        workspace: Arc<Workspace>,
        previous: String,
        previous_revision: u64,
    },
    SetWorkspaceDefaultCwd {
        workspace: Arc<Workspace>,
        previous: Option<PathBuf>,
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
    SessionDisplayNames {
        workspace: Arc<Workspace>,
        previous_names: Vec<PersistedSessionDisplayName>,
        previous_operations: Vec<PersistedSessionDisplayNameOperation>,
        previous_revision: u64,
    },
    HiddenSessions {
        workspace: Arc<Workspace>,
        previous_hidden_sessions: Vec<PersistedHiddenSession>,
        previous_operations: Vec<PersistedSessionHideOperation>,
        previous_revision: u64,
    },
    RemovedLauncher {
        workspace: Arc<Workspace>,
        launcher: Arc<WorkspaceLauncher>,
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
        let duplicate = match &event {
            DaemonEventKind::OutputChanged {
                shell_id, run_id, ..
            } => self.pending_runtime_events.iter().position(|pending| {
                matches!(
                    pending,
                    DaemonEventKind::OutputChanged {
                        shell_id: pending_shell_id,
                        run_id: pending_run_id,
                        ..
                    } if pending_shell_id == shell_id && pending_run_id == run_id
                )
            }),
            DaemonEventKind::NodeProjectionChanged { node_id, .. } => {
                self.pending_runtime_events.iter().position(|pending| {
                    matches!(
                        pending,
                        DaemonEventKind::NodeProjectionChanged {
                            node_id: pending_node_id,
                            ..
                        } if pending_node_id == node_id
                    )
                })
            }
            DaemonEventKind::FocusedTerminalPresentationChanged => {
                self.pending_runtime_events.iter().position(|pending| {
                    matches!(pending, DaemonEventKind::FocusedTerminalPresentationChanged)
                })
            }
            _ => None,
        };
        if let Some(index) = duplicate {
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
            },
            |transfer| EventStreamState {
                stream_id: transfer.stream_id,
                latest_id: transfer.latest_id,
                events: transfer.events.into(),
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
        })
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

    fn append_batch_locked(state: &mut EventStreamState, kinds: Vec<DaemonEventKind>) {
        debug_assert!(Self::ensure_capacity(state, kinds.len()).is_ok());
        for kind in kinds {
            Self::append_one_locked(state, kind);
        }
        Self::retain_latest_locked(state);
    }

    fn append_one_locked(state: &mut EventStreamState, kind: DaemonEventKind) {
        state.latest_id += 1;
        state.events.push_back(DaemonEvent {
            id: state.latest_id,
            at_ms: unix_time_ms(),
            kind,
        });
    }

    fn retain_latest_locked(state: &mut EventStreamState) {
        if state.events.len() > MAX_RETAINED_EVENTS {
            let remove = state.events.len() - MAX_RETAINED_EVENTS;
            state.events.drain(..remove);
        }
    }

    #[cfg(test)]
    fn publish(&self, kind: DaemonEventKind) -> io::Result<DaemonEvent> {
        let mut state = lock(&self.state)?;
        Self::ensure_capacity(&state, 1)?;
        Self::append_batch_locked(&mut state, vec![kind]);
        let event = state
            .events
            .back()
            .cloned()
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
            let offset = usize::try_from(after.event_id.saturating_sub(earliest))
                .unwrap_or(state.events.len());
            let events = state
                .events
                .iter()
                .skip(offset)
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
            Self::append_one_locked(events, kind);
        }
        Self::retain_latest_locked(events);
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
    let event_count = through.event_id.saturating_sub(after.event_id);
    if event_count > u64::from(protocol::MAX_NODE_PROJECTION_TRANSITIONS) {
        return (NodeProjectionSyncMode::Baseline, Vec::new());
    }
    let offset =
        usize::try_from(after.event_id.saturating_sub(earliest)).unwrap_or(state.events.len());
    let transitions = state
        .events
        .iter()
        .skip(offset)
        .take(event_count as usize)
        .filter_map(reduce_projection_transition)
        .collect();
    (NodeProjectionSyncMode::Resumed, transitions)
}

#[cfg(any(test, feature = "benchmark-internals"))]
#[doc(hidden)]
pub mod benchmark_support {
    use super::*;

    const STREAM_ID: &str = "00000000-0000-0000-0000-000000000001";

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub struct EventPageSummary {
        pub count: usize,
        pub first_id: u64,
        pub last_id: u64,
        pub checksum: u64,
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub struct TransitionSummary {
        pub baseline: bool,
        pub count: usize,
        pub checksum: u64,
    }

    pub struct EventFixture {
        stream: EventStream,
        retained: usize,
    }

    pub struct EventPageResult(Response);

    impl EventPageResult {
        pub fn summary(&self) -> EventPageSummary {
            let Response::Events { events, .. } = &self.0 else {
                panic!("benchmark event read returned a non-event response");
            };
            EventPageSummary {
                count: events.len(),
                first_id: events.first().map_or(0, |event| event.id),
                last_id: events.last().map_or(0, |event| event.id),
                checksum: events.iter().fold(0, checksum_event),
            }
        }
    }

    pub struct TransitionResult {
        mode: NodeProjectionSyncMode,
        transitions: Vec<NodeProjectionTransition>,
    }

    impl TransitionResult {
        pub fn summary(&self) -> TransitionSummary {
            TransitionSummary {
                baseline: self.mode == NodeProjectionSyncMode::Baseline,
                count: self.transitions.len(),
                checksum: self.transitions.iter().fold(0, checksum_transition),
            }
        }
    }

    pub struct EventAppendFixture {
        state: EventStreamState,
        kinds: Vec<DaemonEventKind>,
    }

    pub struct EventAppendResult(EventStreamState);

    impl EventAppendResult {
        pub fn summary(&self) -> EventPageSummary {
            summarize_events(&self.0.events)
        }
    }

    impl Clone for EventAppendFixture {
        fn clone(&self) -> Self {
            Self {
                state: EventStreamState {
                    stream_id: self.state.stream_id.clone(),
                    latest_id: self.state.latest_id,
                    events: self.state.events.clone(),
                },
                kinds: self.kinds.clone(),
            }
        }
    }

    impl EventFixture {
        pub fn retained(count: usize) -> Self {
            assert!(count <= MAX_RETAINED_EVENTS);
            let events = (1..=count)
                .map(|index| DaemonEvent {
                    id: index as u64,
                    at_ms: 1_000_000 + index as u64,
                    kind: DaemonEventKind::WorkspaceClosed {
                        workspace_id: format!("workspace-{index}"),
                    },
                })
                .collect();
            Self {
                stream: EventStream::from_transfer(Some(handoff::EventStreamManifest {
                    stream_id: STREAM_ID.into(),
                    latest_id: count as u64,
                    events,
                })),
                retained: count,
            }
        }

        pub fn read_page(&self, distance_from_tail: usize, limit: usize) -> EventPageResult {
            assert!(distance_from_tail <= self.retained);
            let cursor = EventCursor {
                stream_id: STREAM_ID.into(),
                event_id: (self.retained - distance_from_tail) as u64,
            };
            EventPageResult(
                self.stream
                    .read_after(&cursor, limit, 0, || false)
                    .expect("benchmark event cursor remains valid"),
            )
        }

        pub fn projection_cut(&self, distance_from_tail: usize) -> TransitionResult {
            assert!(distance_from_tail <= self.retained);
            let state = lock(&self.stream.state).expect("benchmark event state lock");
            let through = EventCursor {
                stream_id: STREAM_ID.into(),
                event_id: self.retained as u64,
            };
            let after = EventCursor {
                stream_id: STREAM_ID.into(),
                event_id: (self.retained - distance_from_tail) as u64,
            };
            let (mode, transitions) = projection_transitions(&state, Some(&after), &through);
            TransitionResult { mode, transitions }
        }
    }

    impl EventAppendFixture {
        pub fn retained_with_batch(retained: usize, batch: usize) -> Self {
            let fixture = EventFixture::retained(retained);
            let state = lock(&fixture.stream.state)
                .expect("benchmark event state lock")
                .to_owned_for_benchmark();
            let kinds = (0..batch)
                .map(|index| DaemonEventKind::WorkspaceClosed {
                    workspace_id: format!("appended-workspace-{index}"),
                })
                .collect();
            Self { state, kinds }
        }

        pub fn append(mut self) -> EventAppendResult {
            EventStream::append_batch_locked(&mut self.state, self.kinds);
            EventAppendResult(self.state)
        }
    }

    impl EventStreamState {
        fn to_owned_for_benchmark(&self) -> Self {
            Self {
                stream_id: self.stream_id.clone(),
                latest_id: self.latest_id,
                events: self.events.clone(),
            }
        }
    }

    #[derive(Clone)]
    pub struct RuntimeEventFixture {
        events: Vec<DaemonEventKind>,
    }

    pub struct RuntimeEventResult(TransitionState);

    impl RuntimeEventResult {
        pub fn summary(&self) -> EventPageSummary {
            let checksum = self
                .0
                .pending_runtime_events
                .iter()
                .fold(0_u64, |checksum, event| match event {
                    DaemonEventKind::NodeProjectionChanged {
                        node_id,
                        cache_generation,
                    } => checksum_bytes(
                        mix_checksum(checksum, *cache_generation),
                        node_id.as_bytes(),
                    ),
                    DaemonEventKind::FocusedTerminalPresentationChanged => {
                        mix_checksum(checksum, u64::MAX)
                    }
                    _ => checksum,
                });
            EventPageSummary {
                count: self.0.pending_runtime_events.len(),
                first_id: 0,
                last_id: 0,
                checksum,
            }
        }
    }

    impl RuntimeEventFixture {
        pub fn invalidations(nodes: usize, revisions: usize) -> Self {
            let mut events =
                Vec::with_capacity(nodes.saturating_mul(revisions).saturating_add(revisions));
            for revision in 1..=revisions {
                for node in 0..nodes {
                    events.push(DaemonEventKind::NodeProjectionChanged {
                        node_id: format!("node-{node}"),
                        cache_generation: revision as u64,
                    });
                }
                events.push(DaemonEventKind::FocusedTerminalPresentationChanged);
            }
            Self { events }
        }

        pub fn coalesce(self) -> RuntimeEventResult {
            let mut transition = TransitionState {
                persistence_in_flight: true,
                ..TransitionState::default()
            };
            for event in self.events {
                transition.queue_runtime_event(event);
            }
            RuntimeEventResult(transition)
        }
    }

    fn summarize_events(events: &VecDeque<DaemonEvent>) -> EventPageSummary {
        EventPageSummary {
            count: events.len(),
            first_id: events.front().map_or(0, |event| event.id),
            last_id: events.back().map_or(0, |event| event.id),
            checksum: events.iter().fold(0, checksum_event_without_time),
        }
    }

    fn checksum_event(checksum: u64, event: &DaemonEvent) -> u64 {
        checksum_workspace_event(
            mix_checksum(mix_checksum(checksum, event.id), event.at_ms),
            &event.kind,
        )
    }

    fn checksum_event_without_time(checksum: u64, event: &DaemonEvent) -> u64 {
        checksum_workspace_event(mix_checksum(checksum, event.id), &event.kind)
    }

    fn checksum_workspace_event(checksum: u64, kind: &DaemonEventKind) -> u64 {
        let DaemonEventKind::WorkspaceClosed { workspace_id } = kind else {
            panic!("benchmark event fixture contains an unexpected event kind");
        };
        checksum_bytes(checksum, workspace_id.as_bytes())
    }

    fn checksum_transition(checksum: u64, transition: &NodeProjectionTransition) -> u64 {
        let checksum = mix_checksum(
            mix_checksum(checksum, transition.event_id),
            transition.at_ms,
        );
        let NodeProjectionTransitionKind::Workspace { workspace_id } = &transition.kind else {
            panic!("benchmark transition fixture contains an unexpected transition kind");
        };
        checksum_bytes(checksum, workspace_id.as_bytes())
    }

    fn checksum_bytes(mut checksum: u64, bytes: &[u8]) -> u64 {
        for byte in bytes {
            checksum = mix_checksum(checksum, u64::from(*byte));
        }
        checksum
    }

    fn mix_checksum(checksum: u64, value: u64) -> u64 {
        checksum.wrapping_mul(0x100_0000_01b3).wrapping_add(value)
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn benchmark_event_fixtures_have_stable_bounded_results() {
            let events = EventFixture::retained(MAX_RETAINED_EVENTS);
            let page = events.read_page(256, 256).summary();
            assert_eq!(
                (page.count, page.first_id, page.last_id),
                (256, 7_937, 8_192)
            );
            assert_ne!(page.checksum, 0);
            assert_eq!(page.checksum, 14_253_501_652_687_160_347);
            assert_eq!(events.read_page(256, 256).summary(), page);

            let resumed = events.projection_cut(256).summary();
            assert!(!resumed.baseline);
            assert_eq!(resumed.count, 256);
            assert_ne!(resumed.checksum, 0);
            assert_eq!(resumed.checksum, 14_253_501_652_687_160_347);
            assert_eq!(events.projection_cut(256).summary(), resumed);
            assert!(events.projection_cut(257).summary().baseline);

            let appended = EventAppendFixture::retained_with_batch(MAX_RETAINED_EVENTS, 256)
                .append()
                .summary();
            assert_eq!(
                (appended.count, appended.first_id, appended.last_id),
                (MAX_RETAINED_EVENTS, 257, 8_448)
            );
            assert_ne!(appended.checksum, 0);
            assert_eq!(appended.checksum, 8_051_509_862_202_861_121);

            let coalesced = RuntimeEventFixture::invalidations(128, 64)
                .coalesce()
                .summary();
            assert_eq!(coalesced.count, 129);
            assert_eq!(coalesced.checksum, 9_953_927_640_701_513_843);
        }
    }
}

fn reduce_projection_transition(event: &DaemonEvent) -> Option<NodeProjectionTransition> {
    let kind = match &event.kind {
        DaemonEventKind::WorkspaceCreated { workspace_id, .. }
        | DaemonEventKind::WorkspaceRenamed { workspace_id, .. }
        | DaemonEventKind::WorkspaceDefaultCwdChanged { workspace_id, .. }
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
        DaemonEventKind::AgentWorkingContextObserved {
            workspace_id,
            agent,
            ..
        } => NodeProjectionTransitionKind::SessionContext {
            workspace_id: workspace_id.clone(),
            agent_id: agent.id.clone(),
        },
        DaemonEventKind::HandoffCompleted => NodeProjectionTransitionKind::HandoffCompleted,
        DaemonEventKind::NodeProjectionChanged { .. }
        | DaemonEventKind::FocusedTerminalPresentationChanged
        | DaemonEventKind::AgentSessionDisplayNameChanged { .. }
        | DaemonEventKind::AgentSessionHidden { .. } => return None,
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

    fn resumable_agent(
        &self,
        shell: &Shell,
        previous_run: &PersistedShellRun,
    ) -> io::Result<Option<ResumableAgent>> {
        let state = lock(&self.state)?;
        let mut candidates = Vec::new();
        for agent in state.agents.values() {
            if let Some(identity) = resume_identity(agent, shell, previous_run)? {
                candidates.push((agent.id.clone(), identity.0, identity.1));
            }
        }
        candidates.sort();
        let [(agent_id, integration, external_session_id)] = candidates.as_slice() else {
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
            .and_then(|resume| resume.command(&shell.command, external_session_id))
            .map(|command| ResumableAgent {
                agent_id: agent_id.clone(),
                integration: integration.clone(),
                external_session_id: external_session_id.clone(),
                command,
            }))
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
            default_cwd: Mutex::new(default_cwd),
            shell_ids: Mutex::new(shells.iter().map(|shell| shell.id.clone()).collect()),
            launcher_ids: Mutex::new(Vec::new()),
            agent_ids: Mutex::new(Vec::new()),
            session_display_names: Mutex::new(Vec::new()),
            session_display_name_operations: Mutex::new(Vec::new()),
            hidden_sessions: Mutex::new(Vec::new()),
            session_hide_operations: Mutex::new(Vec::new()),
        });
        let snapshot = WorkspaceSnapshot {
            id: workspace_id.clone(),
            revision: 1,
            name: name.clone(),
            default_cwd: lock(&workspace.default_cwd)?.clone(),
            shells: shells
                .iter()
                .map(|shell| shell.snapshot())
                .collect::<io::Result<_>>()?,
            launchers: Vec::new(),
            agents: Vec::new(),
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

    fn create_workspace_exact(
        &self,
        workspace_id: &str,
        name: String,
        default_cwd: Option<PathBuf>,
    ) -> io::Result<(WorkspaceSnapshot, Option<DurableUndo>)> {
        validate_uuid(workspace_id, "workspace ID")?;
        validate_name(&name)?;
        if let Some(default_cwd) = &default_cwd {
            validate_cwd(default_cwd)?;
        }
        if let Ok(existing) = self.workspace(workspace_id) {
            let snapshot = existing.snapshot(self)?;
            if snapshot.name == name && snapshot.default_cwd == default_cwd {
                return Ok((snapshot, None));
            }
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "workspace idempotency identity exists with a different definition",
            ));
        }
        let workspace = Arc::new(Workspace {
            id: workspace_id.to_owned(),
            revision: Mutex::new(1),
            name: Mutex::new(name.clone()),
            default_cwd: Mutex::new(default_cwd.clone()),
            shell_ids: Mutex::new(Vec::new()),
            launcher_ids: Mutex::new(Vec::new()),
            agent_ids: Mutex::new(Vec::new()),
            session_display_names: Mutex::new(Vec::new()),
            session_display_name_operations: Mutex::new(Vec::new()),
            hidden_sessions: Mutex::new(Vec::new()),
            session_hide_operations: Mutex::new(Vec::new()),
        });
        let snapshot = WorkspaceSnapshot {
            id: workspace_id.to_owned(),
            revision: 1,
            name: name.clone(),
            default_cwd,
            shells: Vec::new(),
            launchers: Vec::new(),
            agents: Vec::new(),
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
        state
            .workspaces
            .insert(workspace_id.to_owned(), Arc::clone(&workspace));
        Ok((
            snapshot,
            Some(DurableUndo::CreatedWorkspace {
                workspace,
                shells: Vec::new(),
            }),
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

    fn create_shell_exact(
        &self,
        workspace_id: &str,
        shell_id: &str,
        spec: ShellSpec,
    ) -> io::Result<(ShellSnapshot, Option<DurableUndo>)> {
        validate_uuid(shell_id, "Shell idempotency key")?;
        if let Some(existing) = lock(&self.state)?.shells.get(shell_id).cloned() {
            let snapshot = existing.snapshot()?;
            if snapshot.workspace_id == workspace_id
                && snapshot.name == spec.name
                && snapshot.cwd == spec.cwd
                && snapshot.command == spec.command
            {
                return Ok((snapshot, None));
            }
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "Shell idempotency key exists with a different definition",
            ));
        }
        let workspace = self.workspace(workspace_id)?;
        let shell = create_pending_shell_with_id(workspace_id, shell_id.to_owned(), spec)?;
        let snapshot = shell.snapshot()?;
        let mut state = lock(&self.state)?;
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
        Ok((
            snapshot,
            Some(DurableUndo::CreatedShell { workspace, shell }),
        ))
    }

    fn replay_interrupted_shell_start(
        &self,
        shell_id: &str,
        mut run: PersistedShellRun,
    ) -> io::Result<()> {
        validate_uuid(shell_id, "journal Shell ID")?;
        validate_uuid(&run.id, "journal ShellRun ID")?;
        validate_terminal_profile(&run.profile)?;
        if run.generation == 0 || run.ended_at_ms.is_some() || run.exit_reason.is_some() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "local Shell start journal contains an invalid active run",
            ));
        }
        let state = lock(&self.state)?;
        let shell = state
            .shells
            .get(shell_id)
            .cloned()
            .ok_or_else(|| not_found("journal Shell", shell_id))?;
        let shells = state.shells.values().cloned().collect::<Vec<_>>();
        drop(state);
        if shells.iter().any(|candidate| {
            candidate.id != shell_id
                && candidate
                    .last_run
                    .lock()
                    .is_ok_and(|last_run| last_run.as_ref().is_some_and(|last| last.id == run.id))
        }) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "local Shell start journal reuses a ShellRun ID",
            ));
        }
        let mut last_run = lock(&shell.last_run)?;
        if let Some(existing) = last_run.as_ref() {
            if existing.generation == run.generation {
                if existing.id == run.id
                    && existing.started_at_ms == run.started_at_ms
                    && existing.output_revision >= run.output_revision
                    && existing.environment_has_run_id == run.environment_has_run_id
                    && existing.profile.term == run.profile.term
                    && existing.profile.colorterm == run.profile.colorterm
                    && existing.profile.term_program == run.profile.term_program
                    && existing.profile.term_program_version == run.profile.term_program_version
                {
                    return Ok(());
                }
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "local Shell start journal conflicts with persisted run history",
                ));
            }
            if existing.generation > run.generation {
                return Ok(());
            }
        }
        run.ended_at_ms = Some(unix_time_ms().max(run.started_at_ms));
        run.exit_reason = Some(ShellRunExitReason::Interrupted);
        *last_run = Some(run);
        Ok(())
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

    fn create_launcher_exact(
        &self,
        workspace_id: &str,
        launcher_id: &str,
        spec: WorkspaceLauncherSpec,
    ) -> io::Result<(WorkspaceLauncherSnapshot, Option<DurableUndo>)> {
        validate_uuid(launcher_id, "launcher idempotency key")?;
        if let Some(existing) = lock(&self.state)?.launchers.get(launcher_id).cloned() {
            let snapshot = existing.snapshot()?;
            if snapshot.workspace_id == workspace_id
                && snapshot.name == spec.name
                && snapshot.cwd == spec.cwd
                && snapshot.command == spec.command
            {
                return Ok((snapshot, None));
            }
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "launcher idempotency key exists with a different definition",
            ));
        }
        validate_name(&spec.name)?;
        validate_cwd(&spec.cwd)?;
        validate_launcher_command(&spec.command)?;
        let workspace = self.workspace(workspace_id)?;
        let launcher = Arc::new(WorkspaceLauncher {
            id: launcher_id.to_owned(),
            revision: Mutex::new(1),
            workspace_id: workspace_id.into(),
            name: Mutex::new(spec.name),
            cwd: spec.cwd,
            command: spec.command,
        });
        let snapshot = launcher.snapshot()?;
        let mut state = lock(&self.state)?;
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
            Some(DurableUndo::CreatedLauncher {
                workspace,
                launcher,
            }),
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

    fn set_workspace_default_cwd(
        &self,
        workspace_id: &str,
        default_cwd: PathBuf,
    ) -> io::Result<Option<DurableUndo>> {
        validate_cwd(&default_cwd)?;
        let workspace = self.workspace(workspace_id)?;
        let mut revision = lock(&workspace.revision)?;
        let mut current = lock(&workspace.default_cwd)?;
        if current.as_ref() == Some(&default_cwd) {
            return Ok(None);
        }
        let previous_revision = *revision;
        *revision = revision
            .checked_add(1)
            .ok_or_else(|| io::Error::other("workspace revision exhausted"))?;
        let previous = current.replace(default_cwd);
        drop(current);
        drop(revision);
        Ok(Some(DurableUndo::SetWorkspaceDefaultCwd {
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
                working_contexts: Vec::new(),
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

    fn observe_agent_working_context(
        &self,
        agent_id: &str,
        shell_id: &str,
        run_id: &str,
        mut context: AgentWorkingContextSnapshot,
    ) -> DaemonResult<(AgentInstanceSnapshot, bool, Option<DurableUndo>)> {
        validate_working_context(&context)?;
        let agent = self.agent(agent_id)?;
        if agent.shell_id != shell_id {
            return Err(DaemonError::lifecycle(
                ErrorCode::RunChanged,
                "agent instance is bound to a different shell",
            ));
        }
        if agent.run_id != run_id {
            return Err(DaemonError::lifecycle(
                ErrorCode::RunChanged,
                "agent instance is bound to a different shell run",
            ));
        }
        let shell = self.shell(shell_id)?;
        match &*lock(&shell.lifecycle)? {
            ShellLifecycle::Running { run, .. } if run.id == run_id => {}
            _ => {
                return Err(DaemonError::lifecycle(
                    ErrorCode::RunChanged,
                    "shell does not have the requested active run",
                ));
            }
        }
        let mut state = lock(&agent.state)?;
        if state.working_contexts.first().is_some_and(|current| {
            current.worktree_root == context.worktree_root
                && current.repository == context.repository
                && current.branch == context.branch
        }) {
            return Ok((agent.snapshot_from(&state), false, None));
        }
        let previous = state.clone();
        context.observed_at_ms = unix_time_ms()
            .max(agent.started_at_ms)
            .max(state.observation.observed_at_ms)
            .max(
                state
                    .working_contexts
                    .first()
                    .map_or(0, |current| current.observed_at_ms),
            );
        state
            .working_contexts
            .retain(|current| current.worktree_root != context.worktree_root);
        state.working_contexts.insert(0, context);
        state.working_contexts.truncate(MAX_AGENT_WORKING_CONTEXTS);
        let snapshot = agent.snapshot_from(&state);
        drop(state);
        Ok((
            snapshot,
            true,
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
            DurableUndo::SetWorkspaceDefaultCwd {
                workspace,
                previous,
                previous_revision,
            } => {
                *lock(&workspace.default_cwd)? = previous;
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
            DurableUndo::SessionDisplayNames {
                workspace,
                previous_names,
                previous_operations,
                previous_revision,
            } => {
                *lock(&workspace.session_display_names)? = previous_names;
                *lock(&workspace.session_display_name_operations)? = previous_operations;
                *lock(&workspace.revision)? = previous_revision;
            }
            DurableUndo::HiddenSessions {
                workspace,
                previous_hidden_sessions,
                previous_operations,
                previous_revision,
            } => {
                *lock(&workspace.hidden_sessions)? = previous_hidden_sessions;
                *lock(&workspace.session_hide_operations)? = previous_operations;
                *lock(&workspace.revision)? = previous_revision;
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
        })
    }

    fn node_projection(&self, node_id: String) -> io::Result<NodeProjectionSnapshot> {
        let (workspaces, shells, launchers, agents) = {
            let state = lock(&self.state)?;
            (
                state.workspaces.values().cloned().collect::<Vec<_>>(),
                state.shells.values().cloned().collect::<Vec<_>>(),
                state.launchers.values().cloned().collect::<Vec<_>>(),
                state.agents.values().cloned().collect::<Vec<_>>(),
            )
        };
        let mut projected_workspaces = Vec::with_capacity(workspaces.len());
        for workspace in workspaces {
            let item_count =
                lock(&workspace.shell_ids)?.len() + lock(&workspace.launcher_ids)?.len();
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
        projected_workspaces.sort_by(|left, right| left.id.cmp(&right.id));
        projected_shells.sort_by(|left, right| left.id.cmp(&right.id));
        projected_launchers.sort_by(|left, right| left.id.cmp(&right.id));
        projected_agents.sort_by(|left, right| left.id.cmp(&right.id));
        Ok(NodeProjectionSnapshot {
            node_id,
            workspaces: projected_workspaces,
            shells: projected_shells,
            launchers: projected_launchers,
            agents: projected_agents,
        })
    }

    fn shells(&self) -> io::Result<Vec<Arc<Shell>>> {
        Ok(lock(&self.state)?.shells.values().cloned().collect())
    }

    fn capture_persisted_state(&self) -> io::Result<PersistenceGeneration> {
        let (mut workspaces, shells_by_id, launchers_by_id, agents_by_id) = {
            let state = lock(&self.state)?;
            (
                state.workspaces.values().cloned().collect::<Vec<_>>(),
                state.shells.clone(),
                state.launchers.clone(),
                state.agents.clone(),
            )
        };
        workspaces.sort_by(|left, right| left.id.cmp(&right.id));
        let mut saved = PersistedState::default();
        for workspace in workspaces {
            let (workspace_revision, workspace_default_cwd) =
                workspace.revision_and_default_cwd()?;
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
            saved.workspaces.push(PersistedWorkspace {
                id: workspace.id.clone(),
                revision: workspace_revision,
                name: lock(&workspace.name)?.clone(),
                default_cwd: workspace_default_cwd,
                shells,
                launchers,
                agents,
                session_display_names: lock(&workspace.session_display_names)?.clone(),
                session_display_name_operations: lock(&workspace.session_display_name_operations)?
                    .clone(),
                hidden_sessions: lock(&workspace.hidden_sessions)?.clone(),
                session_hide_operations: lock(&workspace.session_hide_operations)?.clone(),
            });
        }
        Ok(PersistenceGeneration {
            revision: self.persistence_revision.load(Ordering::Acquire),
            state: saved,
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

    fn presented_focused_terminal(&self) -> io::Result<Option<QualifiedFocusedTerminalSnapshot>> {
        Ok(lock(&self.focus)?.presented_terminal.clone())
    }

    fn record_presented_focus(&self, node_id: String, shell_id: String) -> io::Result<()> {
        let mut focus = lock(&self.focus)?;
        let revision = focus
            .presented_revision
            .checked_add(1)
            .ok_or_else(|| io::Error::other("presented terminal focus revision exhausted"))?;
        focus.presented_revision = revision;
        focus.presented_terminal = Some(QualifiedFocusedTerminalSnapshot {
            revision,
            shell: QualifiedIdentity::new(node_id, shell_id),
        });
        Ok(())
    }

    fn import_presented_focus(
        &self,
        node_id: String,
        shell_id: String,
        revision: u64,
    ) -> io::Result<()> {
        let mut focus = lock(&self.focus)?;
        if revision >= focus.presented_revision {
            focus.presented_revision = revision;
            focus.presented_terminal = Some(QualifiedFocusedTerminalSnapshot {
                revision,
                shell: QualifiedIdentity::new(node_id, shell_id),
            });
        }
        Ok(())
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
        for (_, collaborator) in lock(&runtime.collaborators)?.drain() {
            let _ = collaborator.connection.shutdown(std::net::Shutdown::Both);
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
            return Ok(());
        }
        drop(controller);
        lock(&runtime.collaborators)?.remove(token);
        Ok(())
    }

    fn participant_is_authorized(runtime: &ShellRuntime, token: &str) -> io::Result<bool> {
        if Self::participant_is_primary(runtime, token)? {
            return Ok(true);
        }
        Ok(lock(&runtime.collaborators)?.contains_key(token))
    }

    fn participant_is_primary(runtime: &ShellRuntime, token: &str) -> io::Result<bool> {
        Ok(lock(&runtime.controller)?
            .as_ref()
            .is_some_and(|controller| controller.token == token))
    }

    fn fanout_output(runtime: &ShellRuntime, bytes: &[u8]) {
        if let Ok(mut controller) = runtime.controller.lock() {
            let disconnect = controller.as_ref().is_some_and(|current| {
                current
                    .output
                    .send(ControllerOutput::Data(bytes.to_vec()))
                    .is_err()
            });
            if disconnect && let Some(current) = controller.take() {
                let _ = current.connection.shutdown(std::net::Shutdown::Both);
            }
        }
        if let Ok(mut collaborators) = runtime.collaborators.lock() {
            collaborators.retain(|_, collaborator| {
                let connected = collaborator
                    .output
                    .try_send(ControllerOutput::Data(bytes.to_vec()))
                    .is_ok();
                if !connected {
                    let _ = collaborator.connection.shutdown(std::net::Shutdown::Both);
                }
                connected
            });
        }
    }

    fn fanout_collaborator_resize(runtime: &ShellRuntime, size: PtySize) {
        if let Ok(mut collaborators) = runtime.collaborators.lock() {
            collaborators.retain(|_, collaborator| {
                let connected = collaborator
                    .output
                    .try_send(ControllerOutput::Resize {
                        rows: size.rows,
                        cols: size.cols,
                        pixel_width: size.pixel_width,
                        pixel_height: size.pixel_height,
                    })
                    .is_ok();
                if !connected {
                    let _ = collaborator.connection.shutdown(std::net::Shutdown::Both);
                }
                connected
            });
        }
    }

    fn displace_collaborators(runtime: &ShellRuntime) -> io::Result<()> {
        lock(&runtime.collaborators)?.clear();
        Ok(())
    }

    fn quiesce_controllers(&self, runtimes: &[Arc<ShellRuntime>]) -> io::Result<()> {
        let mut acknowledgements = Vec::new();
        for runtime in runtimes {
            let mut controller = lock(&runtime.controller)?;
            if let Some(current) = controller.as_mut() {
                match Self::quiesce_participant(current)? {
                    Some(acknowledgement) => acknowledgements.push(acknowledgement),
                    None => {
                        if let Some(current) = controller.take() {
                            let _ = current.connection.shutdown(std::net::Shutdown::Both);
                        }
                    }
                }
            }
            drop(controller);

            let mut collaborators = lock(&runtime.collaborators)?;
            let mut disconnected = Vec::new();
            for (token, collaborator) in collaborators.iter_mut() {
                match Self::quiesce_participant(collaborator)? {
                    Some(acknowledgement) => acknowledgements.push(acknowledgement),
                    None => disconnected.push(token.clone()),
                }
            }
            for token in disconnected {
                if let Some(collaborator) = collaborators.remove(&token) {
                    let _ = collaborator.connection.shutdown(std::net::Shutdown::Both);
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
                active |= !lock(&runtime.collaborators)?.is_empty();
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

    fn quiesce_participant(
        participant: &mut Controller,
    ) -> io::Result<Option<(mpsc::Receiver<bool>, mpsc::Receiver<()>)>> {
        let (written, write_acknowledged) = mpsc::sync_channel(1);
        let (client_acknowledge, client_acknowledged) = mpsc::sync_channel(1);
        participant.reconnect_ack = Some(client_acknowledge);
        match participant
            .output
            .try_send(ControllerOutput::Reconnect(written))
        {
            Ok(()) => Ok(Some((write_acknowledged, client_acknowledged))),
            Err(TrySendError::Disconnected(_)) => Ok(None),
            Err(TrySendError::Full(_)) => Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "active attachment output queue is full",
            )),
        }
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
}

impl Default for DaemonService {
    fn default() -> Self {
        Self {
            node_identity: None,
            node_registrations: None,
            node_projection_cache: None,
            global_workspaces: None,
            local_shell_journal: None,
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
            opencode: OpenCodeCoordinator::default(),
            kiro: KiroLaunchHolders::default(),
            claude_remote_control: ClaudeRemoteControlBindings::default(),
            remote_attachments: RemoteAttachmentManager::default(),
            host_service_previews: Mutex::new(HashMap::new()),
            host_session_catalog: HostSessionCatalogCache::default(),
            workspace_operation_locks: Mutex::new(HashMap::new()),
            mutation_lock: Mutex::new(()),
            notification_settings: NotificationDeliverySettings::default(),
            notification_sink: Arc::new(DisabledNotificationSink),
            startup_environment: capture_current_environment(),
            node_projection_workers: NodeProjectionWorkers::default(),
            #[cfg(test)]
            fail_after_mutation: AtomicBool::new(false),
        }
    }
}

struct Workspace {
    id: String,
    revision: Mutex<u64>,
    name: Mutex<String>,
    default_cwd: Mutex<Option<PathBuf>>,
    shell_ids: Mutex<Vec<String>>,
    launcher_ids: Mutex<Vec<String>>,
    agent_ids: Mutex<Vec<String>>,
    session_display_names: Mutex<Vec<PersistedSessionDisplayName>>,
    session_display_name_operations: Mutex<Vec<PersistedSessionDisplayNameOperation>>,
    hidden_sessions: Mutex<Vec<PersistedHiddenSession>>,
    session_hide_operations: Mutex<Vec<PersistedSessionHideOperation>>,
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
    working_contexts: Vec<AgentWorkingContextSnapshot>,
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
    collaborators: Mutex<HashMap<String, Controller>>,
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
    Resize {
        rows: u16,
        cols: u16,
        pixel_width: u16,
        pixel_height: u16,
    },
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
        let Some(admission_epoch) = service
            .node_registrations()
            .and_then(|registrations| {
                registrations
                    .observe(&registration)
                    .map_err(node_registration_error)
            })
            .ok()
            .flatten()
        else {
            if interruptible_node_sleep(&service, &node_id, Duration::from_millis(100)) {
                return;
            }
            continue;
        };
        let attempt_at_ms = unix_time_ms();
        let result = fetch_node_projection(&registration, after);
        let mut published_generation = None;
        let mut remote_notifications = Vec::new();
        match result {
            Ok((sync, capabilities, observed_helper_version)) => {
                let commit = service.node_registrations().and_then(|registrations| {
                    registrations
                        .with_observation(&registration, admission_epoch, || {
                            service
                                .node_projection_cache
                                .as_ref()
                                .ok_or_else(|| {
                                    io::Error::other("Node projection cache unavailable")
                                })?
                                .commit_projection(
                                    &registration,
                                    expected_generation,
                                    ProjectionObservation {
                                        cursor: sync.cursor.clone(),
                                        projection: sync.projection.clone(),
                                        capabilities,
                                        helper_version: observed_helper_version,
                                        observed_at_ms: attempt_at_ms,
                                    },
                                )
                        })
                        .map_err(node_registration_error)
                });
                if let Ok(Some(Some(commit))) = commit {
                    published_generation = Some(commit.generation);
                    failures = 0;
                    let (candidates, digest) = remote_notification_candidates(
                        &registration,
                        &sync,
                        &commit,
                        &service.notification_settings,
                    );
                    if !candidates.is_empty() || digest.is_some() {
                        let claims = candidates
                            .iter()
                            .map(|candidate| candidate.claim.clone())
                            .collect::<Vec<_>>();
                        let claimed = service.node_registrations().and_then(|registrations| {
                            registrations
                                .with_observation(&registration, admission_epoch, || {
                                    service
                                        .node_projection_cache
                                        .as_ref()
                                        .ok_or_else(|| {
                                            io::Error::other("Node projection cache unavailable")
                                        })?
                                        .claim_notifications(
                                            &registration,
                                            &sync.cursor,
                                            &claims,
                                            digest.as_ref().map(|digest| &digest.claim),
                                        )
                                })
                                .map_err(node_registration_error)
                        });
                        match claimed {
                            Ok(Some(Some((accepted, digest_accepted)))) => {
                                remote_notifications.extend(
                                    candidates.into_iter().zip(accepted).filter_map(
                                        |(candidate, accepted)| {
                                            accepted.then_some(candidate.request)
                                        },
                                    ),
                                );
                                if digest_accepted && let Some(digest) = digest {
                                    remote_notifications.push(digest.request);
                                }
                            }
                            Ok(_) => {}
                            Err(error) => {
                                eprintln!("boomux: could not claim remote notification: {error}");
                            }
                        }
                    }
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
                        .with_observation(&registration, admission_epoch, || {
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
        if let Some(cache_generation) = published_generation {
            let _ = service.events.publish_runtime_batch(vec![
                DaemonEventKind::NodeProjectionChanged {
                    node_id: registration.node_id.clone(),
                    cache_generation,
                },
            ]);
        }
        for notification in remote_notifications {
            service.notification_sink.notify(notification);
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
        if interruptible_node_sleep(&service, &node_id, delay) {
            return;
        }
    }
}

struct RemoteNotificationCandidate {
    claim: RemoteNotificationClaim,
    request: NotificationRequest,
}

struct RemoteDigestCandidate {
    claim: RemoteDigestClaim,
    request: NotificationRequest,
}

fn remote_notification_candidates(
    registration: &crate::protocol::NodeRegistrationSnapshot,
    sync: &NodeProjectionSync,
    commit: &ProjectionCommit,
    settings: &NotificationDeliverySettings,
) -> (
    Vec<RemoteNotificationCandidate>,
    Option<RemoteDigestCandidate>,
) {
    if sync.mode != NodeProjectionSyncMode::Resumed {
        return (Vec::new(), None);
    }
    let Some(prior) = commit.previous_cursor.as_ref().filter(|prior| {
        prior.stream_id == sync.cursor.stream_id && prior.event_id <= sync.cursor.event_id
    }) else {
        return (Vec::new(), None);
    };
    let node = NotificationNodeContext {
        alias: registration.alias.clone(),
        node_id: registration.node_id.clone(),
    };
    let workspaces = sync
        .projection
        .workspaces
        .iter()
        .map(|workspace| (workspace.id.as_str(), workspace.name.as_str()))
        .collect::<HashMap<_, _>>();
    let shells = sync
        .projection
        .shells
        .iter()
        .map(|shell| (shell.id.as_str(), shell.name.as_str()))
        .collect::<HashMap<_, _>>();
    let mut seen = HashSet::new();
    let mut candidates = Vec::new();
    for transition in &sync.transitions {
        let candidate = match &transition.kind {
            NodeProjectionTransitionKind::Agent {
                workspace_id,
                agent_id,
                revision,
            } => sync
                .projection
                .agents
                .iter()
                .find(|agent| agent.id == *agent_id && agent.observation_revision == *revision)
                .and_then(|agent| {
                    let attention = agent
                        .attention
                        .as_ref()
                        .filter(|attention| attention.observation_revision == *revision)?;
                    let category = match (agent.state, attention.reason) {
                        (AgentState::Blocked, AgentAttentionReason::Blocked) => {
                            RemoteNotificationCategory::AgentBlocked
                        }
                        (AgentState::Done, AgentAttentionReason::Completed) => {
                            RemoteNotificationCategory::AgentCompleted
                        }
                        _ => return None,
                    };
                    let reason = remote_notification_reason(category);
                    category_enabled(settings, reason).then(|| RemoteNotificationCandidate {
                        claim: RemoteNotificationClaim {
                            stream_id: sync.cursor.stream_id.clone(),
                            entity_id: agent.id.clone(),
                            revision: *revision,
                            category,
                            reason: remote_notification_reason_key(category).into(),
                        },
                        request: NotificationRequest {
                            reason,
                            agent: agent.name.clone(),
                            workspace: workspaces
                                .get(workspace_id.as_str())
                                .copied()
                                .unwrap_or(workspace_id)
                                .into(),
                            shell: shells
                                .get(agent.shell_id.as_str())
                                .copied()
                                .unwrap_or(&agent.shell_id)
                                .into(),
                            node: Some(node.clone()),
                            digest: None,
                        },
                    })
                }),
            _ => None,
        };
        if let Some(candidate) = candidate
            && seen.insert(candidate.claim.clone())
        {
            candidates.push(candidate);
        }
    }
    if commit.previous_health == Some(crate::protocol::NodeProjectionHealthCode::Online) {
        return (candidates, None);
    }
    let mut enabled_categories = candidates
        .iter()
        .map(|candidate| candidate.claim.category)
        .collect::<Vec<_>>();
    enabled_categories.sort_unstable();
    enabled_categories.dedup();
    if enabled_categories.is_empty() {
        return (Vec::new(), None);
    }
    let mut counts = NotificationDigest::default();
    for candidate in &candidates {
        let count = match candidate.claim.category {
            RemoteNotificationCategory::AgentBlocked => &mut counts.blocked,
            RemoteNotificationCategory::AgentCompleted => &mut counts.completed,
        };
        *count = count.saturating_add(1);
    }
    let reason = remote_notification_reason(enabled_categories[0]);
    (
        Vec::new(),
        Some(RemoteDigestCandidate {
            claim: RemoteDigestClaim {
                stream_id: sync.cursor.stream_id.clone(),
                prior_cursor: prior.event_id,
                through_cursor: sync.cursor.event_id,
                enabled_categories,
            },
            request: NotificationRequest {
                reason,
                agent: String::new(),
                workspace: String::new(),
                shell: String::new(),
                node: Some(node),
                digest: Some(counts),
            },
        }),
    )
}

fn remote_notification_reason(category: RemoteNotificationCategory) -> NotificationReason {
    match category {
        RemoteNotificationCategory::AgentBlocked => NotificationReason::Blocked,
        RemoteNotificationCategory::AgentCompleted => NotificationReason::Completed,
    }
}

fn remote_notification_reason_key(category: RemoteNotificationCategory) -> &'static str {
    match category {
        RemoteNotificationCategory::AgentBlocked => "blocked",
        RemoteNotificationCategory::AgentCompleted => "completed",
    }
}

fn fetch_node_projection(
    registration: &crate::protocol::NodeRegistrationSnapshot,
    after: Option<EventCursor>,
) -> Result<
    (NodeProjectionSync, Vec<String>, String),
    (crate::protocol::NodeProjectionHealthCode, io::Error),
> {
    use crate::protocol::NodeProjectionHealthCode;
    let target = SshTarget::parse(registration.target.clone())
        .map_err(|error| (NodeProjectionHealthCode::Unreachable, error))?;
    let mut bootstrap = ssh_bootstrap::BootstrapSession::open(
        target,
        SshAuthenticationMode::Batch,
        Duration::from_secs(2),
    )
    .map_err(|error| (classify_node_sync_error(&error), error))?;
    let helper = match bootstrap.plan(Duration::from_secs(2)) {
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
    let mut remote = bootstrap
        .connect(helper, Duration::from_secs(2))
        .map_err(|error| (classify_node_sync_error(&error), error))?;
    let observed_helper_version = remote.handshake.helper_version.clone();
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
    let capabilities = if protocol::ProtocolFeature::GlobalWorkspaces.is_supported_by(version) {
        sync.capabilities.clone()
    } else {
        protocol::ProtocolFeature::ALL
            .iter()
            .copied()
            .filter(|feature| feature.is_supported_by(version))
            .flat_map(protocol::ProtocolFeature::capability_names)
            .copied()
            .map(str::to_owned)
            .collect()
    };
    Ok((sync, capabilities, observed_helper_version))
}

fn classify_node_sync_error(error: &io::Error) -> crate::protocol::NodeProjectionHealthCode {
    use crate::protocol::NodeProjectionHealthCode;
    if crate::ssh_bootstrap::error_code(error) == "upgrade_recovery_required" {
        NodeProjectionHealthCode::Stale
    } else if error.kind() == io::ErrorKind::WouldBlock {
        NodeProjectionHealthCode::Reconnecting
    } else if error.kind() == io::ErrorKind::Unsupported {
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

fn interruptible_node_sleep(service: &DaemonService, node_id: &str, duration: Duration) -> bool {
    let deadline = Instant::now() + duration;
    while Instant::now() < deadline {
        if service.node_projection_workers.stop.load(Ordering::Acquire) {
            return true;
        }
        if lock(&service.node_projection_workers.wake)
            .map(|mut wake| wake.remove(node_id))
            .unwrap_or(false)
        {
            return false;
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

    fn global_workspaces(&self) -> DaemonResult<&GlobalWorkspaceStore> {
        self.global_workspaces.as_ref().ok_or_else(|| {
            DaemonError::lifecycle(
                ErrorCode::PersistenceFailed,
                "global Workspace coordination is unavailable",
            )
        })
    }

    fn local_shell_journal(&self) -> DaemonResult<&LocalShellJournal> {
        self.local_shell_journal.as_ref().ok_or_else(|| {
            DaemonError::lifecycle(
                ErrorCode::PersistenceFailed,
                "local Shell transaction journal is unavailable",
            )
        })
    }

    fn local_shell_checkpoint_delay(&self) -> Duration {
        #[cfg(debug_assertions)]
        if self.native_test_hooks_enabled()
            && let Some(variable) = self.startup_environment.variables.iter().find(|variable| {
                variable.name == b"BOOMUX_NATIVE_TEST_LOCAL_SHELL_CHECKPOINT_DELAY_MS"
            })
            && let Ok(value) = std::str::from_utf8(&variable.value)
            && let Ok(milliseconds) = value.parse::<u64>()
        {
            return Duration::from_millis(milliseconds.min(60_000));
        }
        Duration::from_millis(100)
    }

    fn replay_local_shell_transactions(&self) -> io::Result<()> {
        let Some(journal) = &self.local_shell_journal else {
            return Ok(());
        };
        let records = journal.records()?;
        if records.is_empty() {
            return Ok(());
        }
        let global_workspaces = self
            .global_workspaces()
            .map_err(|error| io::Error::other(error.to_string()))?;
        for record in records.iter().filter_map(|record| match record {
            LocalShellJournalRecord::Create(record) => Some(record),
            LocalShellJournalRecord::Start(_) => None,
        }) {
            let (_, workspace_undo) = self.durable.create_workspace_exact(
                &record.owner_workspace_id,
                record.owner_workspace_name.clone(),
                record.default_cwd.clone(),
            )?;
            let (_, shell_undo) = self.durable.create_shell_exact(
                &record.owner_workspace_id,
                &record.shell_id,
                record.shell.clone(),
            )?;
            drop(workspace_undo);
            drop(shell_undo);
        }
        let mut latest_starts = HashMap::new();
        for record in records.iter().filter_map(|record| match record {
            LocalShellJournalRecord::Create(_) => None,
            LocalShellJournalRecord::Start(record) => Some(record.as_ref()),
        }) {
            latest_starts.insert(record.shell_id.as_str(), record);
        }
        for record in latest_starts.into_values() {
            self.durable
                .replay_interrupted_shell_start(&record.shell_id, record.run.clone())?;
        }
        let saved = self.durable.capture_persisted_state()?;
        self.durable.write_persisted_state(saved)?;
        for record in records.iter().filter_map(|record| match record {
            LocalShellJournalRecord::Create(record) => Some(record),
            LocalShellJournalRecord::Start(_) => None,
        }) {
            let mut owner = self
                .durable
                .workspace(&record.owner_workspace_id)?
                .snapshot(&self.durable)?;
            owner.revision = record.owner_revision;
            let resource = RoutedOperationResult::Shell {
                shell: record.result_shell.clone(),
            };
            let transaction = global_workspaces.transaction()?;
            match transaction.prepare_resource_for_attempt(
                &record.global_workspace_id,
                &record.operation_id,
                &record.request_fingerprint,
                record.request_bytes,
                record.expected_global_revision,
                &record.node_id,
                &record.requested_owner_workspace_id,
                &record.owner_workspace_name,
                record.default_cwd.clone(),
                &record.shell_id,
                PendingResourceKind::Shell,
            )? {
                PreparedWorkspaceResource::Completed(_) => {}
                PreparedWorkspaceResource::Pending { pending, .. } => {
                    if pending.owner_workspace_id != record.owner_workspace_id {
                        return Err(io::Error::new(
                            io::ErrorKind::AlreadyExists,
                            "journal owner Workspace conflicts with coordinator preparation",
                        ));
                    }
                    transaction.complete_resource(&pending, &owner, &resource)?;
                }
            }
            transaction.commit()?;
        }
        global_workspaces.checkpoint()?;
        journal.clear()
    }

    fn checkpoint_local_shell_transactions(&self) -> io::Result<()> {
        let Some(journal) = &self.local_shell_journal else {
            return Ok(());
        };
        if journal.is_empty()? {
            return Ok(());
        }
        let _mutation = lock(&self.mutation_lock)?;
        self.checkpoint_local_shell_transactions_with_mutation()
    }

    fn checkpoint_local_shell_transactions_with_mutation(&self) -> io::Result<()> {
        let Some(journal) = &self.local_shell_journal else {
            return Ok(());
        };
        if journal.is_empty()? {
            return Ok(());
        }
        let _persistence = lock(&self.durable.persist_lock)?;
        let saved = self.durable.capture_persisted_state()?;
        self.durable.write_persisted_state(saved)?;
        self.global_workspaces()
            .map_err(|error| io::Error::other(error.to_string()))?
            .checkpoint()?;
        journal.clear()?;
        Ok(())
    }

    fn handle_session_resume(
        &self,
        mut stream: UnixStream,
        response_version: u32,
        session_id: &str,
        profile: TerminalProfile,
    ) -> io::Result<()> {
        if let Err(error) = validate_terminal_profile(&profile) {
            return send_response(
                &mut stream,
                response_version,
                DaemonError::from(error).into_response(),
            );
        }
        let snapshot = match self.snapshot() {
            Ok(snapshot) => snapshot,
            Err(error) => {
                return send_response(
                    &mut stream,
                    response_version,
                    DaemonError::from(error).into_response(),
                );
            }
        };
        let mut sessions = match self.exact_host_sessions(&snapshot, session_id) {
            Ok(sessions) => sessions,
            Err(error) => {
                return send_response(&mut stream, response_version, error.into_response());
            }
        };
        if protocol::ProtocolFeature::WorkspaceSessionHiding.is_supported_by(response_version) {
            match self.hidden_session_metadata() {
                Ok(metadata) => crate::session_projection::filter_hidden(&mut sessions, &metadata),
                Err(error) => {
                    return send_response(&mut stream, response_version, error.into_response());
                }
            }
        }
        let plan = match host_services::prepare_session_resume(&sessions, session_id) {
            Ok(plan) => plan,
            Err(error) => {
                return send_response(
                    &mut stream,
                    response_version,
                    DaemonError::from(error).into_response(),
                );
            }
        };
        let (executable, arguments) = plan.argv.split_first().ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "session resume argv is empty")
        })?;
        let pty = native_pty_system()
            .openpty(PtySize {
                rows: profile.rows,
                cols: profile.cols,
                pixel_width: profile.pixel_width,
                pixel_height: profile.pixel_height,
            })
            .map_err(io::Error::other)?;
        let master = Arc::new(PtyMaster::duplicate(pty.master.as_ref())?);
        let mut reader = master.try_clone_reader()?;
        let mut command = CommandBuilder::new(executable);
        command.args(arguments);
        command.cwd(&plan.cwd);
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
        for name in [
            "BOOMUX_WORKSPACE_ID",
            "BOOMUX_WORKSPACE",
            "BOOMUX_SHELL_ID",
            "BOOMUX_SHELL_NAME",
            "BOOMUX_RUN_ID",
        ] {
            command.env_remove(name);
        }
        let mut child = pty.slave.spawn_command(command).map_err(io::Error::other)?;
        drop(pty.slave);
        drop(pty.master);
        send_response(
            &mut stream,
            response_version,
            Response::Attached {
                token: Uuid::new_v4().to_string(),
                reconstruction: Vec::new(),
                warning: None,
                profile: None,
            },
        )?;
        let mut output_stream = stream.try_clone()?;
        output_stream.set_write_timeout(Some(RESPONSE_WRITE_TIMEOUT))?;
        let output = thread::Builder::new()
            .name(format!("session-resume-output-{session_id}"))
            .spawn(move || -> io::Result<()> {
                let mut bytes = vec![0; 16 * 1024];
                loop {
                    match reader.read(&mut bytes) {
                        Ok(0) => break,
                        Ok(count) => AttachFrame::Output(bytes[..count].to_vec())
                            .write_to(&mut output_stream)?,
                        Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                            thread::sleep(IO_RETRY_DELAY)
                        }
                        Err(error) if error.raw_os_error() == Some(libc::EIO) => break,
                        Err(error) => return Err(error),
                    }
                }
                let _ = AttachFrame::Detached.write_to(&mut output_stream);
                let _ = output_stream.shutdown(std::net::Shutdown::Both);
                Ok(())
            })?;
        let input_result = 'input: loop {
            match AttachFrame::read_from(&mut stream) {
                Ok(AttachFrame::Input(bytes)) => {
                    let mut remaining = bytes.as_slice();
                    while !remaining.is_empty() {
                        match master.write(remaining) {
                            Ok(0) => break,
                            Ok(count) => remaining = &remaining[count..],
                            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                                thread::sleep(IO_RETRY_DELAY)
                            }
                            Err(error) => break 'input Err(error),
                        }
                    }
                }
                Ok(AttachFrame::Resize {
                    rows,
                    cols,
                    pixel_width,
                    pixel_height,
                }) => master.resize(PtySize {
                    rows,
                    cols,
                    pixel_width,
                    pixel_height,
                })?,
                Ok(AttachFrame::FocusGained) => {}
                Ok(AttachFrame::Detached | AttachFrame::ReconnectAck) => break Ok(()),
                Ok(AttachFrame::Output(_) | AttachFrame::Reconnect) => {
                    break Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "invalid client frame during Agent Session resume",
                    ));
                }
                Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => break Ok(()),
                Err(error) => break Err(error),
            }
        };
        let _ = child.kill();
        let _ = child.wait();
        let _ = stream.shutdown(std::net::Shutdown::Both);
        let output_result = output
            .join()
            .map_err(|_| io::Error::other("session resume output thread panicked"))?;
        input_result?;
        output_result
    }

    fn handle_remote_session_resume(
        &self,
        mut stream: UnixStream,
        response_version: u32,
        node_id: &str,
        session_id: &str,
        profile: TerminalProfile,
    ) -> io::Result<()> {
        let registrations = match self.node_registrations() {
            Ok(registrations) => registrations,
            Err(error) => {
                return send_response(&mut stream, response_version, error.into_response());
            }
        };
        let registration = match registrations.inspect(node_id) {
            Ok(registration) if registration.node_id == node_id => registration,
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
                    "Node registration changed before session resume",
                ),
            );
        }
        let result = self.bridge_remote_session_resume(
            &mut stream,
            response_version,
            &registration,
            session_id,
            profile,
        );
        registrations.release(&registration);
        result
    }

    fn bridge_remote_session_resume(
        &self,
        stream: &mut UnixStream,
        response_version: u32,
        registration: &crate::protocol::NodeRegistrationSnapshot,
        session_id: &str,
        profile: TerminalProfile,
    ) -> io::Result<()> {
        let target = SshTarget::parse(registration.target.clone())?;
        let mut bootstrap = ssh_bootstrap::BootstrapSession::open(
            target,
            SshAuthenticationMode::Batch,
            HANDSHAKE_TIMEOUT,
        )?;
        let helper = match bootstrap.plan(HANDSHAKE_TIMEOUT)? {
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
        let forwarded_version = response_version.min(helper.handshake.core_protocol_version);
        if !protocol::ProtocolFeature::NodeHostServices.is_supported_by(forwarded_version) {
            return send_response(
                stream,
                response_version,
                error_response(
                    ErrorCode::UnsupportedVersion,
                    "remote Node does not support exact Agent Session resume",
                ),
            );
        }
        let remote = bootstrap.connect(helper, HANDSHAKE_TIMEOUT)?;
        let (response, mut remote_reader, mut remote_writer) = remote.open_attachment_at_version(
            Request::ResumeAgentSession {
                session_id: session_id.to_owned(),
                profile,
            },
            HANDSHAKE_TIMEOUT,
            forwarded_version,
        )?;
        if !matches!(response, Response::Attached { .. }) {
            return send_response(stream, response_version, response);
        }
        send_response(stream, response_version, response)?;
        let mut output_connection = stream.try_clone()?;
        output_connection.set_write_timeout(Some(RESPONSE_WRITE_TIMEOUT))?;
        let output = thread::Builder::new()
            .name(format!("remote-session-resume-{session_id}"))
            .spawn(move || -> io::Result<()> {
                while let Ok(frame) = remote_reader.read_frame() {
                    frame.write_to(&mut output_connection)?;
                    if matches!(frame, AttachFrame::Detached) {
                        break;
                    }
                }
                let _ = output_connection.shutdown(std::net::Shutdown::Both);
                Ok(())
            })?;
        let input_result = loop {
            let frame = match AttachFrame::read_from(stream) {
                Ok(frame) => frame,
                Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => break Ok(()),
                Err(error) => break Err(error),
            };
            let closes = matches!(frame, AttachFrame::Detached);
            remote_writer.write_frame(&frame, RESPONSE_WRITE_TIMEOUT)?;
            if closes {
                break Ok(());
            }
        };
        drop(remote_writer);
        let _ = stream.shutdown(std::net::Shutdown::Both);
        let output_result = output
            .join()
            .map_err(|_| io::Error::other("remote session output thread panicked"))?;
        input_result?;
        output_result
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
        let mut bootstrap = ssh_bootstrap::BootstrapSession::open(
            target,
            SshAuthenticationMode::Batch,
            HANDSHAKE_TIMEOUT,
        )?;
        let helper = match bootstrap.plan(HANDSHAKE_TIMEOUT)? {
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
        let remote = bootstrap.connect(helper, HANDSHAKE_TIMEOUT)?;
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
            if matches!(frame, AttachFrame::FocusGained) {
                self.runtimes
                    .record_presented_focus(registration.node_id.clone(), shell_id.to_owned())?;
                let _ = self.events.publish_runtime_batch(vec![
                    DaemonEventKind::FocusedTerminalPresentationChanged,
                ]);
            }
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
        self.route_node_operation_for_version(node_id, operation, protocol::PROTOCOL_VERSION)
    }

    fn route_node_operation_for_version(
        &self,
        node_id: &str,
        operation: RoutedOperation,
        requester_version: u32,
    ) -> Response {
        self.route_node_operation_for_version_after_handshake(
            node_id,
            operation,
            requester_version,
            &mut || Ok(()),
        )
    }

    fn route_node_operation_after_handshake(
        &self,
        node_id: &str,
        operation: RoutedOperation,
        before_request: &mut dyn FnMut() -> io::Result<()>,
    ) -> Response {
        self.route_node_operation_for_version_after_handshake(
            node_id,
            operation,
            protocol::PROTOCOL_VERSION,
            before_request,
        )
    }

    fn route_node_operation_for_version_after_handshake(
        &self,
        node_id: &str,
        operation: RoutedOperation,
        requester_version: u32,
        before_request: &mut dyn FnMut() -> io::Result<()>,
    ) -> Response {
        if matches!(
            operation,
            RoutedOperation::SetAgentSessionDisplayName { .. }
                | RoutedOperation::HideAgentSession { .. }
        ) {
            return error_response(
                ErrorCode::UnsupportedVersion,
                "Agent Session mutation has been removed",
            );
        }
        if let RoutedOperation::SetAgentSessionDisplayName { operation_id, .. } = &operation
            && let Err(error) = validate_uuid(operation_id, "Session display-name operation ID")
        {
            return DaemonError::from(error).into_response();
        }
        if let RoutedOperation::HideAgentSession { operation_id, .. } = &operation
            && let Err(error) = validate_uuid(operation_id, "Session hide operation ID")
        {
            return DaemonError::from(error).into_response();
        }
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
        let response_timeout = routed_response_timeout(&operation);
        let owner_feature = routed_owner_feature(&operation);
        let mut result = send_registered_node_request_with_timeout_for_version_after_handshake(
            &registration,
            operation.owner_request(),
            response_timeout,
            owner_feature,
            requester_version,
            before_request,
        );
        if result.is_err() && operation.is_retryable() {
            result = send_registered_node_request_with_timeout_for_version_after_handshake(
                &registration,
                operation.owner_request(),
                response_timeout,
                owner_feature,
                requester_version,
                before_request,
            );
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

    fn host_sessions(
        &self,
        snapshot: &Snapshot,
        workspace_id: Option<&str>,
    ) -> DaemonResult<Vec<crate::session_projection::SessionProjection>> {
        let scoped;
        let snapshot = if let Some(workspace_id) = workspace_id {
            let workspace = snapshot
                .workspaces
                .iter()
                .find(|workspace| workspace.id == workspace_id)
                .ok_or_else(|| not_found("workspace", workspace_id))?;
            scoped = Snapshot {
                workspaces: vec![workspace.clone()],
                focused_terminal: None,
            };
            &scoped
        } else {
            snapshot
        };
        let requests = host_services::session_catalog_requests(snapshot);
        let catalog = self.host_session_catalog.records(&requests)?;
        let mut sessions = host_services::sessions_with_catalog(snapshot, &catalog);
        crate::session_projection::apply_display_names(
            &mut sessions,
            &self.session_display_name_metadata()?,
        );
        Ok(sessions)
    }

    fn durable_host_sessions(
        &self,
        snapshot: &Snapshot,
    ) -> DaemonResult<Vec<crate::session_projection::SessionProjection>> {
        let mut sessions = host_services::durable_sessions(snapshot);
        crate::session_projection::apply_display_names(
            &mut sessions,
            &self.session_display_name_metadata()?,
        );
        Ok(sessions)
    }

    fn exact_host_sessions(
        &self,
        snapshot: &Snapshot,
        session_id: &str,
    ) -> DaemonResult<Vec<crate::session_projection::SessionProjection>> {
        let durable = self.durable_host_sessions(snapshot)?;
        let durable_contains_session = !matches!(
            crate::session_projection::resolve_exact(&durable, session_id),
            Err(crate::session_projection::ResolveError::NotFound)
        );

        let Some(catalog) = self.host_session_catalog.cached_records()? else {
            return if durable_contains_session {
                Ok(durable)
            } else {
                self.host_sessions(snapshot, None)
            };
        };
        let mut sessions = host_services::sessions_with_catalog(snapshot, &catalog);
        crate::session_projection::apply_display_names(
            &mut sessions,
            &self.session_display_name_metadata()?,
        );
        if durable_contains_session
            || !matches!(
                crate::session_projection::resolve_exact(&sessions, session_id),
                Err(crate::session_projection::ResolveError::NotFound)
            )
        {
            Ok(sessions)
        } else {
            self.host_sessions(snapshot, None)
        }
    }

    fn session_display_name_metadata(
        &self,
    ) -> DaemonResult<Vec<crate::session_projection::SessionDisplayNameMetadata>> {
        let workspaces = lock(&self.durable.state)?
            .workspaces
            .values()
            .cloned()
            .collect::<Vec<_>>();
        let mut metadata = Vec::new();
        for workspace in workspaces {
            for record in lock(&workspace.session_display_names)?.iter() {
                let (external_session_id, agent_id) = match &record.session {
                    PersistedSessionIdentity::External {
                        external_session_id,
                    } => (Some(external_session_id.clone()), None),
                    PersistedSessionIdentity::Instance { agent_id } => {
                        (None, Some(agent_id.clone()))
                    }
                };
                metadata.push(crate::session_projection::SessionDisplayNameMetadata {
                    workspace_id: workspace.id.clone(),
                    integration: record.integration.clone(),
                    external_session_id,
                    agent_id,
                    display_name: record.display_name.clone(),
                });
            }
        }
        Ok(metadata)
    }

    fn hidden_session_metadata(
        &self,
    ) -> DaemonResult<Vec<crate::session_projection::HiddenSessionMetadata>> {
        let workspaces = lock(&self.durable.state)?
            .workspaces
            .values()
            .cloned()
            .collect::<Vec<_>>();
        let mut metadata = Vec::new();
        for workspace in workspaces {
            for record in lock(&workspace.hidden_sessions)?.iter() {
                let (external_session_id, agent_id) = match &record.session {
                    PersistedSessionIdentity::External {
                        external_session_id,
                    } => (Some(external_session_id.clone()), None),
                    PersistedSessionIdentity::Instance { agent_id } => {
                        (None, Some(agent_id.clone()))
                    }
                };
                metadata.push(crate::session_projection::HiddenSessionMetadata {
                    workspace_id: workspace.id.clone(),
                    integration: record.integration.clone(),
                    external_session_id,
                    agent_id,
                });
            }
        }
        Ok(metadata)
    }

    fn host_service_for_version(
        &self,
        operation: HostServiceOperation,
        _requester_version: u32,
    ) -> DaemonResult<HostServiceResult> {
        match operation {
            HostServiceOperation::DiscoverProjects => Ok(HostServiceResult::Projects {
                discovery: host_services::discover_projects().map_err(DaemonError::from)?,
            }),
            HostServiceOperation::ResolveDirectory { path } => Ok(HostServiceResult::Directory {
                path: host_services::resolve_directory(&path).map_err(DaemonError::from)?,
            }),
            HostServiceOperation::SuggestShellName { workspace_id } => {
                let workspace = self.workspace(&workspace_id)?.snapshot(&self.durable)?;
                let name =
                    host_services::suggest_shell_name(&workspace).map_err(DaemonError::from)?;
                Ok(HostServiceResult::ShellName { workspace_id, name })
            }
            HostServiceOperation::InvokeLauncher {
                workspace_id,
                launcher_id,
            } => {
                let workspace = self.workspace(&workspace_id)?.snapshot(&self.durable)?;
                let launcher = self.launcher(&launcher_id)?.snapshot()?;
                host_services::invoke_launcher(&workspace, &launcher).map_err(DaemonError::from)?;
                Ok(HostServiceResult::LauncherInvoked {
                    workspace_id,
                    launcher_id,
                })
            }
            HostServiceOperation::IntegrationStatus { integration } => {
                Ok(HostServiceResult::IntegrationStatus {
                    integrations: host_services::integration_status(
                        integration.as_deref(),
                        &self.snapshot()?,
                    )
                    .map_err(DaemonError::from)?,
                })
            }
            HostServiceOperation::PreviewIntegrationMutation {
                action,
                integrations,
                force,
            } => {
                let prepared =
                    host_services::prepare_integration_mutation(action, &integrations, force)
                        .map_err(DaemonError::from)?;
                let token = Uuid::new_v4().to_string();
                let mut previews = lock(&self.host_service_previews)?;
                previews
                    .retain(|_, preview| preview.created_at.elapsed() < HOST_SERVICE_PREVIEW_TTL);
                if previews.len() >= MAX_HOST_SERVICE_PREVIEWS {
                    return Err(DaemonError::lifecycle(
                        ErrorCode::Busy,
                        "too many uncommitted integration previews",
                    ));
                }
                previews.insert(
                    token.clone(),
                    HostServicePreview {
                        created_at: Instant::now(),
                        prepared: prepared.clone(),
                    },
                );
                Ok(HostServiceResult::IntegrationMutationPreview {
                    preview: HostIntegrationMutationPreview {
                        token,
                        action,
                        force,
                        plans: prepared.plans,
                    },
                })
            }
            HostServiceOperation::CommitIntegrationMutation { preview_token } => {
                let preview = lock(&self.host_service_previews)?.remove(&preview_token);
                let preview = preview.ok_or_else(|| {
                    DaemonError::lifecycle(
                        ErrorCode::NotFound,
                        "integration preview is missing, expired, or already committed",
                    )
                })?;
                if preview.created_at.elapsed() >= HOST_SERVICE_PREVIEW_TTL {
                    return Err(DaemonError::lifecycle(
                        ErrorCode::NotFound,
                        "integration preview expired",
                    ));
                }
                Ok(HostServiceResult::IntegrationMutation {
                    integrations: host_services::commit_integration_mutation(&preview.prepared)
                        .map_err(DaemonError::from)?,
                })
            }
            HostServiceOperation::VerifyIntegration {
                integration,
                shell_id,
                run_id,
            } => Ok(HostServiceResult::IntegrationVerified {
                agents: host_services::verify_integration(
                    &self.snapshot()?,
                    &integration,
                    &shell_id,
                    &run_id,
                )
                .map_err(DaemonError::from)?,
                integration,
                shell_id,
                run_id,
            }),
            HostServiceOperation::ListAgentSessions { .. }
            | HostServiceOperation::InspectAgentSession { .. }
            | HostServiceOperation::ResolveAgentSession { .. } => Err(DaemonError::lifecycle(
                ErrorCode::UnsupportedVersion,
                "Agent Session services have been removed",
            )),
        }
    }

    fn set_agent_session_display_name(
        &self,
        operation_id: String,
        session_id: String,
        expected_workspace_revision: u64,
        display_name: Option<String>,
    ) -> DaemonResult<Response> {
        validate_uuid(&operation_id, "Session display-name operation ID")?;
        let display_name = display_name
            .map(|name| normalize_session_display_name(&name))
            .transpose()?;
        self.durable_mutation_outcome(|undo| {
            let workspaces = lock(&self.durable.state)?
                .workspaces
                .values()
                .cloned()
                .collect::<Vec<_>>();
            for workspace in workspaces {
                if let Some(replayed) = lock(&workspace.session_display_name_operations)?
                    .iter()
                    .find(|operation| operation.operation_id == operation_id)
                    .cloned()
                {
                    if replayed.session_id != session_id
                        || replayed.expected_revision != expected_workspace_revision
                        || replayed.display_name != display_name
                    {
                        return Err(DaemonError::lifecycle(
                            ErrorCode::IdempotencyExpired,
                            "Session display-name operation ID was reused for another request",
                        ));
                    }
                    return Ok(DurableMutation::Unchanged(
                        Response::AgentSessionDisplayName {
                            outcome: replayed.result,
                        },
                    ));
                }
            }
            let snapshot = self.snapshot()?;
            let sessions = self.host_sessions(&snapshot, None)?;
            let session = crate::session_projection::resolve_exact(&sessions, &session_id)
                .map_err(|error| {
                    DaemonError::lifecycle(
                        match error {
                            crate::session_projection::ResolveError::NotFound => {
                                ErrorCode::NotFound
                            }
                            crate::session_projection::ResolveError::DuplicateId => {
                                ErrorCode::AmbiguousTarget
                            }
                        },
                        "exact Agent Session was not found",
                    )
                })?;
            let workspace = self.workspace(&session.workspace_id)?;
            let integration = session.integration.clone();
            require_guard(
                session.workspace_revision,
                expected_workspace_revision,
                "workspace",
            )?;
            let identity = match &session.external_session_id {
                Some(external_session_id) => PersistedSessionIdentity::External {
                    external_session_id: external_session_id.clone(),
                },
                None => PersistedSessionIdentity::Instance {
                    agent_id: session
                        .occurrences
                        .first()
                        .ok_or_else(|| {
                            DaemonError::lifecycle(
                                ErrorCode::NotFound,
                                "Agent Session has no semantic fallback identity",
                            )
                        })?
                        .agent_id
                        .clone(),
                },
            };
            let mut names = lock(&workspace.session_display_names)?;
            let operations = lock(&workspace.session_display_name_operations)?;
            let mut revision = lock(&workspace.revision)?;
            let previous_names = names.clone();
            let previous_operations = operations.clone();
            let previous_revision = *revision;
            let record_index = names
                .iter()
                .position(|record| record.integration == integration && record.session == identity);
            match (record_index, &display_name) {
                (Some(index), Some(name)) => names[index].display_name = name.clone(),
                (Some(index), None) => {
                    names.remove(index);
                }
                (None, Some(name)) => {
                    if names.len() >= MAX_SESSION_DISPLAY_NAMES_PER_WORKSPACE {
                        return Err(DaemonError::lifecycle(
                            ErrorCode::Busy,
                            "Workspace Session display-name limit reached",
                        ));
                    }
                    names.push(PersistedSessionDisplayName {
                        integration: integration.clone(),
                        session: identity.clone(),
                        display_name: name.clone(),
                    });
                }
                (None, None) => {}
            }
            *revision = revision
                .checked_add(1)
                .ok_or_else(|| io::Error::other("workspace revision exhausted"))?;
            let resulting_revision = *revision;
            undo.record(DurableUndo::SessionDisplayNames {
                workspace: Arc::clone(&workspace),
                previous_names,
                previous_operations,
                previous_revision,
            });
            drop(revision);
            drop(operations);
            drop(names);

            let result = protocol::AgentSessionDisplayNameResult {
                session_id: session_id.clone(),
                workspace_id: workspace.id.clone(),
                user_display_name: display_name.clone(),
                workspace_revision: resulting_revision,
                changed: true,
            };
            let mut operations = lock(&workspace.session_display_name_operations)?;
            if operations.len() >= MAX_SESSION_DISPLAY_NAME_OPERATIONS_PER_WORKSPACE {
                operations.remove(0);
            }
            operations.push(PersistedSessionDisplayNameOperation {
                operation_id,
                session_id: session_id.clone(),
                expected_revision: expected_workspace_revision,
                display_name,
                integration,
                session: identity,
                result: result.clone(),
            });
            drop(operations);
            Ok(DurableMutation::Changed(
                Response::AgentSessionDisplayName {
                    outcome: result.clone(),
                },
                vec![DaemonEventKind::AgentSessionDisplayNameChanged {
                    workspace_id: result.workspace_id.clone(),
                    session_id,
                    user_display_name: result.user_display_name.clone(),
                    workspace_revision: result.workspace_revision,
                }],
            ))
        })
    }

    fn hide_agent_session(
        &self,
        operation_id: String,
        session_id: String,
        workspace_id: String,
        expected_workspace_revision: u64,
    ) -> DaemonResult<Response> {
        validate_uuid(&operation_id, "Session hide operation ID")?;
        validate_uuid(&session_id, "Agent Session ID")?;
        validate_uuid(&workspace_id, "workspace ID")?;
        self.durable_mutation_outcome(|undo| {
            let workspaces = lock(&self.durable.state)?
                .workspaces
                .values()
                .cloned()
                .collect::<Vec<_>>();
            for workspace in workspaces {
                if let Some(replayed) = lock(&workspace.session_hide_operations)?
                    .iter()
                    .find(|operation| operation.operation_id == operation_id)
                    .cloned()
                {
                    if replayed.session_id != session_id
                        || replayed.workspace_id != workspace_id
                        || replayed.expected_revision != expected_workspace_revision
                    {
                        return Err(DaemonError::lifecycle(
                            ErrorCode::IdempotencyExpired,
                            "Session hide operation ID was reused for another request",
                        ));
                    }
                    return Ok(DurableMutation::Unchanged(Response::AgentSessionHidden {
                        outcome: replayed.result,
                    }));
                }
            }

            let workspace = self.workspace(&workspace_id)?;
            require_guard(
                *lock(&workspace.revision)?,
                expected_workspace_revision,
                "workspace",
            )?;
            let existing = lock(&workspace.hidden_sessions)?
                .iter()
                .find(|hidden| hidden.session_id == session_id)
                .map(|hidden| (hidden.integration.clone(), hidden.session.clone()));
            let (integration, identity) = match existing {
                Some(identity) => identity,
                None => {
                    let snapshot = self.snapshot()?;
                    let sessions = self.host_sessions(&snapshot, Some(&workspace_id))?;
                    let session = crate::session_projection::resolve_exact(&sessions, &session_id)
                        .map_err(|error| {
                            DaemonError::lifecycle(
                                match error {
                                    crate::session_projection::ResolveError::NotFound => {
                                        ErrorCode::NotFound
                                    }
                                    crate::session_projection::ResolveError::DuplicateId => {
                                        ErrorCode::AmbiguousTarget
                                    }
                                },
                                "exact Agent Session was not found in the requested Workspace",
                            )
                        })?;
                    let identity = match &session.external_session_id {
                        Some(external_session_id) => PersistedSessionIdentity::External {
                            external_session_id: external_session_id.clone(),
                        },
                        None => PersistedSessionIdentity::Instance {
                            agent_id: session
                                .occurrences
                                .first()
                                .ok_or_else(|| {
                                    DaemonError::lifecycle(
                                        ErrorCode::NotFound,
                                        "Agent Session has no semantic fallback identity",
                                    )
                                })?
                                .agent_id
                                .clone(),
                        },
                    };
                    (session.integration.clone(), identity)
                }
            };

            let mut hidden_sessions = lock(&workspace.hidden_sessions)?;
            let mut operations = lock(&workspace.session_hide_operations)?;
            let mut revision = lock(&workspace.revision)?;
            require_guard(*revision, expected_workspace_revision, "workspace")?;
            let changed = !hidden_sessions
                .iter()
                .any(|hidden| hidden.integration == integration && hidden.session == identity);
            if changed && hidden_sessions.len() >= MAX_HIDDEN_SESSIONS_PER_WORKSPACE {
                return Err(DaemonError::lifecycle(
                    ErrorCode::Busy,
                    "Workspace hidden Session limit reached",
                ));
            }
            let resulting_revision = if changed {
                revision
                    .checked_add(1)
                    .ok_or_else(|| io::Error::other("workspace revision exhausted"))?
            } else {
                *revision
            };
            let previous_hidden_sessions = hidden_sessions.clone();
            let previous_operations = operations.clone();
            let previous_revision = *revision;
            undo.record(DurableUndo::HiddenSessions {
                workspace: Arc::clone(&workspace),
                previous_hidden_sessions,
                previous_operations,
                previous_revision,
            });
            if changed {
                hidden_sessions.push(PersistedHiddenSession {
                    session_id: session_id.clone(),
                    integration: integration.clone(),
                    session: identity.clone(),
                });
                *revision = resulting_revision;
            }
            let result = protocol::AgentSessionHideResult {
                session_id: session_id.clone(),
                workspace_id: workspace_id.clone(),
                workspace_revision: resulting_revision,
                changed,
            };
            if operations.len() >= MAX_SESSION_HIDE_OPERATIONS_PER_WORKSPACE {
                operations.remove(0);
            }
            operations.push(PersistedSessionHideOperation {
                operation_id,
                session_id: session_id.clone(),
                workspace_id: workspace_id.clone(),
                expected_revision: expected_workspace_revision,
                integration,
                session: identity,
                result: result.clone(),
            });
            drop(revision);
            drop(operations);
            drop(hidden_sessions);

            let events = changed
                .then_some(DaemonEventKind::AgentSessionHidden {
                    workspace_id,
                    session_id,
                    workspace_revision: resulting_revision,
                })
                .into_iter()
                .collect();
            Ok(DurableMutation::Changed(
                Response::AgentSessionHidden { outcome: result },
                events,
            ))
        })
    }

    fn route_node_host_service(&self, node_id: &str, operation: HostServiceOperation) -> Response {
        self.route_node_host_service_for_version(node_id, operation, protocol::PROTOCOL_VERSION)
    }

    fn route_node_host_service_for_version(
        &self,
        node_id: &str,
        operation: HostServiceOperation,
        requester_version: u32,
    ) -> Response {
        if matches!(
            operation,
            HostServiceOperation::ListAgentSessions { .. }
                | HostServiceOperation::InspectAgentSession { .. }
                | HostServiceOperation::ResolveAgentSession { .. }
        ) {
            return error_response(
                ErrorCode::UnsupportedVersion,
                "Agent Session services have been removed",
            );
        }
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
        let mutation = matches!(
            operation,
            HostServiceOperation::InvokeLauncher { .. }
                | HostServiceOperation::CommitIntegrationMutation { .. }
        );
        let response_timeout = match &operation {
            HostServiceOperation::ListAgentSessions { .. }
            | HostServiceOperation::InspectAgentSession { .. } => {
                REGISTERED_NODE_SESSION_RESPONSE_TIMEOUT
            }
            _ => REGISTERED_NODE_RESPONSE_TIMEOUT,
        };
        let response = send_registered_node_request_with_timeout_for_version(
            &registration,
            Request::HostService {
                operation: operation.clone(),
            },
            response_timeout,
            None,
            requester_version,
        );
        registrations.release(&registration);
        let response = match response {
            Ok(response) => response,
            Err(error) if error.kind() == io::ErrorKind::PermissionDenied => {
                return error_response(ErrorCode::NodeIdentityChanged, error.to_string());
            }
            Err(error) if error.kind() == io::ErrorKind::Unsupported => {
                return error_response(ErrorCode::UnsupportedVersion, error.to_string());
            }
            Err(error) => {
                return error_response(
                    if mutation {
                        ErrorCode::OutcomeUnknown
                    } else {
                        ErrorCode::Timeout
                    },
                    format!("Node host service lost its verified channel: {error}"),
                );
            }
        };
        let current = registrations
            .with_current(&registration, || Ok(()))
            .unwrap_or(None)
            .is_some();
        if !current {
            return error_response(
                if mutation {
                    ErrorCode::OutcomeUnknown
                } else {
                    ErrorCode::RevisionChanged
                },
                "Node registration changed while the host service was in flight",
            );
        }
        match response {
            Response::HostService { result } => Response::HostService { result },
            Response::Error { .. } => response,
            _ => error_response(
                ErrorCode::Internal,
                "remote Node returned an unexpected host-service response",
            ),
        }
    }

    fn owner_workspace(
        &self,
        identity: &protocol::QualifiedIdentity,
    ) -> DaemonResult<WorkspaceSnapshot> {
        let local_node_id = self.node_identity()?.id()?;
        if identity.node_id == local_node_id {
            return Ok(self
                .workspace(&identity.inner_id)?
                .snapshot(&self.durable)?);
        }
        match self.route_node_operation(
            &identity.node_id,
            RoutedOperation::GetWorkspace {
                workspace_id: identity.inner_id.clone(),
            },
        ) {
            Response::RoutedNodeOperation {
                result: RoutedOperationResult::Workspace { workspace },
            } => Ok(workspace),
            Response::Error { message, code } => Err(DaemonError::lifecycle(
                code.unwrap_or(ErrorCode::Internal),
                message,
            )),
            _ => Err(DaemonError::lifecycle(
                ErrorCode::Internal,
                "owner returned an unexpected Workspace response",
            )),
        }
    }

    fn with_live_workspace_node<T>(
        &self,
        node_id: &str,
        operation: impl FnOnce(protocol::CombinedNode) -> DaemonResult<T>,
    ) -> DaemonResult<T> {
        let local_node_id = self.node_identity()?.id()?;
        if node_id == local_node_id {
            let snapshot = self.combined_node_snapshot(Some(&local_node_id))?;
            let node = snapshot
                .nodes
                .into_iter()
                .find(|node| node.local && node.node_id == local_node_id)
                .ok_or_else(|| {
                    DaemonError::lifecycle(ErrorCode::NotFound, "local owner Node not found")
                })?;
            Self::require_live_workspace_capability(&node)?;
            return operation(node);
        }
        let registrations = self.node_registrations()?;
        let registration = registrations
            .inspect(node_id)
            .map_err(node_registration_error)?;
        if registration.node_id != node_id || !registrations.admit(&registration)? {
            return Err(DaemonError::lifecycle(
                ErrorCode::RevisionChanged,
                "owner Node registration changed before live verification",
            ));
        }
        let result = (|| {
            let response = send_registered_node_request_with_timeout(
                &registration,
                Request::GetCombinedNodeSnapshot {
                    selector: Some(registration.node_id.clone()),
                },
                Duration::from_secs(2),
                Some(protocol::ProtocolFeature::GlobalWorkspaces),
            )
            .map_err(|error| {
                DaemonError::lifecycle(
                    match error.kind() {
                        io::ErrorKind::PermissionDenied => ErrorCode::NodeIdentityChanged,
                        io::ErrorKind::Unsupported => ErrorCode::UnsupportedVersion,
                        _ => ErrorCode::Timeout,
                    },
                    format!("live owner verification failed: {error}"),
                )
            })?;
            let current = registrations
                .with_current(&registration, || Ok(()))
                .map_err(node_registration_error)?
                .is_some();
            if !current {
                return Err(DaemonError::lifecycle(
                    ErrorCode::RevisionChanged,
                    "owner Node registration changed during live verification",
                ));
            }
            let snapshot = match response {
                Response::CombinedNodeSnapshot { snapshot } => snapshot,
                Response::Error { message, code } => Err(DaemonError::lifecycle(
                    code.unwrap_or(ErrorCode::Internal),
                    message,
                ))?,
                _ => {
                    return Err(DaemonError::lifecycle(
                        ErrorCode::Internal,
                        "owner returned an unexpected live capability response",
                    ));
                }
            };
            let node = snapshot
                .nodes
                .into_iter()
                .find(|node| node.local && node.node_id == registration.node_id)
                .ok_or_else(|| {
                    DaemonError::lifecycle(
                        ErrorCode::NodeIdentityChanged,
                        "live capability response did not describe the pinned local Node",
                    )
                })?;
            Self::require_live_workspace_capability(&node)?;
            operation(node)
        })();
        registrations.release(&registration);
        result
    }

    fn require_live_workspace_capability(node: &protocol::CombinedNode) -> DaemonResult<()> {
        if node.workspace_owner_eligible
            && node
                .observed_capabilities
                .iter()
                .any(|capability| capability == "global_workspaces")
        {
            Ok(())
        } else {
            Err(DaemonError::lifecycle(
                ErrorCode::UnsupportedVersion,
                node.workspace_owner_unavailable_reason
                    .clone()
                    .unwrap_or_else(|| {
                        "live owner does not advertise coordinated Workspace support".into()
                    }),
            ))
        }
    }

    fn preflight_workspace_owner(&self, node_id: &str) -> DaemonResult<()> {
        self.require_cached_workspace_owner_eligible(node_id)?;
        self.with_live_workspace_node(node_id, |_| Ok(()))
    }

    fn preflight_remote_workspace_feature(
        &self,
        node_id: &str,
        feature: protocol::ProtocolFeature,
    ) -> DaemonResult<()> {
        let local_node_id = self.node_identity()?.id()?;
        if node_id == local_node_id {
            return Ok(());
        }
        self.with_live_workspace_node(node_id, |node| {
            require_capabilities_support_feature(&node.observed_capabilities, feature)
        })
    }

    fn require_cached_workspace_owner_eligible(&self, node_id: &str) -> DaemonResult<()> {
        let snapshot = self.combined_node_snapshot(Some(node_id))?;
        let node = snapshot
            .nodes
            .iter()
            .find(|node| node.node_id == node_id)
            .ok_or_else(|| DaemonError::lifecycle(ErrorCode::NotFound, "owner Node not found"))?;
        if node.workspace_owner_eligible {
            Ok(())
        } else {
            Err(DaemonError::lifecycle(
                ErrorCode::Busy,
                node.workspace_owner_unavailable_reason
                    .clone()
                    .unwrap_or_else(|| "owner Node is not eligible for Workspace placement".into()),
            ))
        }
    }

    fn with_workspace_operation_lock<T>(
        &self,
        operation_id: &str,
        operation: impl FnOnce() -> DaemonResult<T>,
    ) -> DaemonResult<T> {
        let operation_lock = {
            let mut locks = lock(&self.workspace_operation_locks)?;
            locks.retain(|_, operation_lock| operation_lock.strong_count() > 0);
            locks
                .entry(operation_id.to_owned())
                .or_insert_with(Weak::new)
                .upgrade()
                .unwrap_or_else(|| {
                    let operation_lock = Arc::new(Mutex::new(()));
                    locks.insert(operation_id.to_owned(), Arc::downgrade(&operation_lock));
                    operation_lock
                })
        };
        let operation_guard = lock(&operation_lock)?;
        let result = operation();
        drop(operation_guard);
        let mut locks = lock(&self.workspace_operation_locks)?;
        if Arc::strong_count(&operation_lock) == 1 {
            locks.remove(operation_id);
        }
        result
    }

    #[allow(clippy::too_many_arguments)]
    fn create_global_workspace_resource(
        &self,
        operation_id: &str,
        request_fingerprint: &str,
        request_bytes: usize,
        global_workspace_id: &str,
        expected_global_revision: u64,
        node_id: &str,
        owner_workspace_id: &str,
        default_cwd: Option<PathBuf>,
        resource_id: &str,
        kind: PendingResourceKind,
        build: impl FnOnce(&PendingWorkspaceResource) -> RoutedOperation,
    ) -> DaemonResult<(protocol::GlobalWorkspaceSnapshot, RoutedOperationResult)> {
        self.with_workspace_operation_lock(operation_id, || {
            self.create_global_workspace_resource_inner(
                operation_id,
                request_fingerprint,
                request_bytes,
                false,
                global_workspace_id,
                expected_global_revision,
                node_id,
                owner_workspace_id,
                default_cwd,
                resource_id,
                kind,
                build,
            )
        })
    }

    #[allow(clippy::too_many_arguments)]
    fn create_local_global_workspace_shell(
        &self,
        operation_id: &str,
        request_fingerprint: &str,
        request_bytes: usize,
        global_workspace_id: &str,
        expected_global_revision: u64,
        node_id: &str,
        requested_owner_workspace_id: &str,
        default_cwd: Option<PathBuf>,
        shell_id: &str,
        shell_spec: ShellSpec,
    ) -> DaemonResult<(protocol::GlobalWorkspaceSnapshot, RoutedOperationResult)> {
        self.with_workspace_operation_lock(operation_id, || {
            if let Some(completed) = self
                .global_workspaces()?
                .completed_operation(operation_id, request_fingerprint)
                .map_err(global_workspace_error)?
            {
                return Ok((completed.workspace, completed.resource));
            }
            // Admit the revision guard before the slower Node eligibility probe so
            // concurrent first resources can share the owner chosen by the winner.
            let global = self
                .global_workspaces()?
                .get(global_workspace_id)
                .map_err(global_workspace_error)?;
            require_guard(
                global.revision,
                expected_global_revision,
                "global Workspace",
            )?;
            #[cfg(debug_assertions)]
            self.wait_for_native_first_resource_barrier(operation_id)?;
            if global.closing {
                return Err(DaemonError::lifecycle(
                    ErrorCode::Busy,
                    "global Workspace close is in progress",
                ));
            }
            let selected = self.combined_node_snapshot(Some(node_id))?;
            let node = selected
                .nodes
                .iter()
                .find(|node| node.node_id == node_id)
                .ok_or_else(|| {
                    DaemonError::lifecycle(ErrorCode::NotFound, "selected Node not found")
                })?;
            if !node.workspace_owner_eligible {
                return Err(DaemonError::lifecycle(
                    ErrorCode::Busy,
                    node.workspace_owner_unavailable_reason
                        .clone()
                        .unwrap_or_else(|| "selected Node is unavailable".into()),
                ));
            }
            let existing = global
                .placements
                .iter()
                .find(|placement| placement.node_id == node_id);
            let owner = existing
                .map(|placement| {
                    self.owner_workspace(&protocol::QualifiedIdentity::new(
                        node_id,
                        &placement.workspace_id,
                    ))
                })
                .transpose()?;
            let owner_name = owner
                .as_ref()
                .map(|owner| owner.name.as_str())
                .unwrap_or(&global.name)
                .to_owned();
            let owner_cwd = owner
                .as_ref()
                .map(|owner| owner.default_cwd.clone())
                .unwrap_or(default_cwd);

            let _mutation = lock(&self.mutation_lock)?;
            self.ensure_running()?;
            self.flush_pending()?;
            let _persistence = lock(&self.durable.persist_lock)?;
            let mut event_transaction = self.events.transaction()?;
            let global_transaction = self.global_workspaces()?.transaction()?;
            let current_global = global_transaction
                .get(global_workspace_id)
                .map_err(global_workspace_error)?;
            let concurrent_first_placement = current_global.revision
                == global.revision.saturating_add(1)
                && current_global.name == global.name
                && current_global.closing == global.closing
                && current_global.placements.len() == global.placements.len() + 1
                && global
                    .placements
                    .iter()
                    .all(|placement| current_global.placements.contains(placement))
                && current_global.placements.iter().any(|placement| {
                    placement.node_id == node_id && !global.placements.contains(placement)
                });
            let concurrent_owner = concurrent_first_placement.then(|| {
                current_global
                    .placements
                    .iter()
                    .find(|placement| {
                        placement.node_id == node_id && !global.placements.contains(placement)
                    })
                    .expect("concurrent placement was validated")
            });
            let admitted_revision = concurrent_owner
                .map(|_| current_global.revision)
                .unwrap_or(expected_global_revision);
            let admitted_owner_name = concurrent_owner
                .and_then(|placement| placement.owner_workspace_name.as_deref())
                .unwrap_or(&owner_name);
            let admitted_owner_cwd = concurrent_owner
                .map(|placement| placement.default_cwd.clone())
                .unwrap_or(owner_cwd);
            let prepared = global_transaction
                .prepare_resource_for_attempt(
                    global_workspace_id,
                    operation_id,
                    request_fingerprint,
                    request_bytes,
                    admitted_revision,
                    node_id,
                    requested_owner_workspace_id,
                    admitted_owner_name,
                    admitted_owner_cwd,
                    shell_id,
                    PendingResourceKind::Shell,
                )
                .map_err(global_workspace_error)?;
            let pending = match prepared {
                PreparedWorkspaceResource::Completed(completed) => {
                    let completed = *completed;
                    return Ok((completed.workspace, completed.resource));
                }
                PreparedWorkspaceResource::Pending { pending, .. } => *pending,
            };
            let mut undo = DurableUndoLog::default();
            let (workspace, workspace_undo) = self.durable.create_workspace_exact(
                &pending.owner_workspace_id,
                pending.owner_workspace_name.clone(),
                pending.default_cwd.clone(),
            )?;
            let workspace_created = workspace_undo.is_some();
            if let Some(record) = workspace_undo {
                undo.record(record);
            }
            let (shell, shell_undo) = match self.durable.create_shell_exact(
                &pending.owner_workspace_id,
                &pending.resource_id,
                shell_spec.clone(),
            ) {
                Ok(created) => created,
                Err(error) => {
                    return Err(Self::mutation_failure(
                        error.into(),
                        undo.rollback(&self.durable),
                    ));
                }
            };
            let shell_created = shell_undo.is_some();
            if let Some(record) = shell_undo {
                undo.record(record);
            }
            let mut events = Vec::new();
            if workspace_created {
                events.push(DaemonEventKind::WorkspaceCreated {
                    workspace_id: workspace.id,
                    name: workspace.name,
                });
            }
            if shell_created {
                events.push(DaemonEventKind::ShellCreated {
                    workspace_id: pending.owner_workspace_id.clone(),
                    shell_id: shell.id.clone(),
                    name: shell.name.clone(),
                });
            }
            if let Err(error) = event_transaction.reserve_with_pending(events.len()) {
                return Err(Self::mutation_failure(
                    error.into(),
                    undo.rollback(&self.durable),
                ));
            }
            let owner = match self
                .durable
                .workspace(&pending.owner_workspace_id)
                .and_then(|workspace| workspace.snapshot(&self.durable))
            {
                Ok(owner) => owner,
                Err(error) => {
                    return Err(Self::mutation_failure(
                        error.into(),
                        undo.rollback(&self.durable),
                    ));
                }
            };
            let resource = RoutedOperationResult::Shell {
                shell: shell.clone(),
            };
            let completed = match global_transaction.complete_resource(&pending, &owner, &resource)
            {
                Ok(completed) => completed,
                Err(error) => {
                    return Err(Self::mutation_failure(
                        global_workspace_error(error),
                        undo.rollback(&self.durable),
                    ));
                }
            };
            let _saved = match self.capture_persisted_state() {
                Ok(saved) => saved,
                Err(error) => {
                    return Err(Self::mutation_failure(
                        error.into(),
                        undo.rollback(&self.durable),
                    ));
                }
            };
            event_transaction.begin_persistence(events.len());
            drop(event_transaction);
            let transaction = LocalShellTransaction {
                operation_id: operation_id.to_owned(),
                request_fingerprint: request_fingerprint.to_owned(),
                request_bytes,
                global_workspace_id: global_workspace_id.to_owned(),
                expected_global_revision: admitted_revision,
                node_id: node_id.to_owned(),
                requested_owner_workspace_id: requested_owner_workspace_id.to_owned(),
                owner_workspace_id: pending.owner_workspace_id.clone(),
                owner_workspace_name: pending.owner_workspace_name.clone(),
                owner_revision: owner.revision,
                default_cwd: pending.default_cwd.clone(),
                shell_id: shell_id.to_owned(),
                shell: shell_spec,
                result_shell: shell.clone(),
            };
            if let Err(error) = self
                .local_shell_journal()?
                .append(LocalShellJournalRecord::Create(Box::new(transaction)))
            {
                let error = Self::mutation_failure(
                    DaemonError::persistence(error),
                    undo.rollback(&self.durable),
                );
                let mut transaction = self.events.transaction()?;
                transaction.finish_persistence();
                return Err(error);
            }
            global_transaction
                .commit()
                .map_err(DaemonError::persistence)?;
            let mut transaction = self.events.transaction()?;
            transaction.append_batch(events);
            transaction.finish_persistence();
            drop(transaction);
            self.events.notify();
            Ok((completed.workspace, completed.resource))
        })
    }

    #[allow(clippy::too_many_arguments)]
    fn create_global_workspace_resource_inner(
        &self,
        operation_id: &str,
        request_fingerprint: &str,
        request_bytes: usize,
        owner_preflight_complete: bool,
        global_workspace_id: &str,
        expected_global_revision: u64,
        node_id: &str,
        owner_workspace_id: &str,
        default_cwd: Option<PathBuf>,
        resource_id: &str,
        kind: PendingResourceKind,
        build: impl FnOnce(&PendingWorkspaceResource) -> RoutedOperation,
    ) -> DaemonResult<(protocol::GlobalWorkspaceSnapshot, RoutedOperationResult)> {
        if let Some(completed) = self
            .global_workspaces()?
            .completed_operation(operation_id, request_fingerprint)
            .map_err(global_workspace_error)?
        {
            return Ok((completed.workspace, completed.resource));
        }
        let global = self
            .global_workspaces()?
            .get(global_workspace_id)
            .map_err(global_workspace_error)?;
        if !owner_preflight_complete {
            let selected = self.combined_node_snapshot(Some(node_id))?;
            let node = selected
                .nodes
                .iter()
                .find(|node| node.node_id == node_id)
                .ok_or_else(|| {
                    DaemonError::lifecycle(ErrorCode::NotFound, "selected Node not found")
                })?;
            if !node.workspace_owner_eligible {
                return Err(DaemonError::lifecycle(
                    ErrorCode::Busy,
                    node.workspace_owner_unavailable_reason
                        .clone()
                        .unwrap_or_else(|| "selected Node is unavailable".into()),
                ));
            }
        }
        let existing = global
            .placements
            .iter()
            .find(|placement| placement.node_id == node_id);
        let owner = existing
            .map(|placement| {
                self.owner_workspace(&protocol::QualifiedIdentity::new(
                    node_id,
                    &placement.workspace_id,
                ))
            })
            .transpose()?;
        let owner_name = owner
            .as_ref()
            .map(|owner| owner.name.as_str())
            .unwrap_or(&global.name);
        let owner_cwd = owner
            .as_ref()
            .map(|owner| owner.default_cwd.clone())
            .unwrap_or(default_cwd);
        let local_node_id = self.node_identity()?.id()?;
        let prepared = self
            .global_workspaces()?
            .prepare_resource(
                global_workspace_id,
                operation_id,
                request_fingerprint,
                request_bytes,
                expected_global_revision,
                node_id,
                owner_workspace_id,
                owner_name,
                owner_cwd,
                resource_id,
                kind,
            )
            .map_err(global_workspace_error)?;
        let (mut pending, newly_prepared) = match prepared {
            PreparedWorkspaceResource::Completed(completed) => {
                let completed = *completed;
                return Ok((completed.workspace, completed.resource));
            }
            PreparedWorkspaceResource::Pending {
                pending,
                newly_prepared,
            } => (*pending, newly_prepared),
        };
        if !newly_prepared && let Some(resource) = self.pending_owner_resource(&pending)? {
            Self::validate_pending_owner_resource(&pending, &resource)?;
            let owner = self.owner_workspace(&protocol::QualifiedIdentity::new(
                &pending.node_id,
                &pending.owner_workspace_id,
            ))?;
            let completed = self
                .global_workspaces()?
                .complete_resource(&pending, &owner, &resource)
                .map_err(global_workspace_error)?;
            return Ok((completed.workspace, completed.resource));
        }
        pending = self
            .global_workspaces()?
            .mark_owner_attempted(&pending)
            .map_err(global_workspace_error)?;
        let operation = build(&pending);
        let response = if pending.node_id == local_node_id {
            self.dispatch(operation.owner_request())
                .unwrap_or_else(DaemonError::into_response)
        } else {
            self.route_node_operation(&pending.node_id, operation)
        };
        let resource = match response {
            Response::RoutedNodeOperation { result } => result,
            Response::Error { message, code } => {
                if (!pending.creates_workspace || !pending.owner_attempted)
                    && !matches!(
                        code,
                        Some(
                            ErrorCode::OutcomeUnknown
                                | ErrorCode::PersistenceFailed
                                | ErrorCode::Timeout
                        )
                    )
                {
                    self.global_workspaces()?
                        .cancel_resource(&pending)
                        .map_err(global_workspace_error)?;
                }
                return Err(DaemonError::lifecycle(
                    code.unwrap_or(ErrorCode::Internal),
                    message,
                ));
            }
            response => routed_result(response).map_err(|_| {
                DaemonError::lifecycle(
                    ErrorCode::Internal,
                    "owner returned an unexpected resource response",
                )
            })?,
        };
        Self::validate_pending_owner_resource(&pending, &resource)?;
        let resource_workspace_id = match &resource {
            RoutedOperationResult::Shell { shell } => &shell.workspace_id,
            RoutedOperationResult::Launcher { launcher } => &launcher.workspace_id,
            _ => {
                return Err(DaemonError::lifecycle(
                    ErrorCode::Internal,
                    "owner returned an unexpected resource type",
                ));
            }
        };
        if resource_workspace_id != &pending.owner_workspace_id {
            return Err(DaemonError::lifecycle(
                ErrorCode::Internal,
                "owner resource belongs to a different Workspace",
            ));
        }
        let owner = self.owner_workspace(&protocol::QualifiedIdentity::new(
            &pending.node_id,
            &pending.owner_workspace_id,
        ))?;
        if owner.name != pending.owner_workspace_name || owner.default_cwd != pending.default_cwd {
            return Err(DaemonError::lifecycle(
                ErrorCode::OutcomeUnknown,
                "owner Workspace metadata does not match the coordinator request",
            ));
        }
        let completed = self
            .global_workspaces()?
            .complete_resource(&pending, &owner, &resource)
            .map_err(global_workspace_error)?;
        Ok((completed.workspace, completed.resource))
    }

    fn pending_owner_resource(
        &self,
        pending: &PendingWorkspaceResource,
    ) -> DaemonResult<Option<RoutedOperationResult>> {
        let operation = match pending.kind {
            PendingResourceKind::Shell => RoutedOperation::GetShell {
                shell_id: pending.resource_id.clone(),
            },
            PendingResourceKind::Launcher => RoutedOperation::GetLauncher {
                launcher_id: pending.resource_id.clone(),
            },
        };
        let local_node_id = self.node_identity()?.id()?;
        let response = if pending.node_id == local_node_id {
            self.dispatch(operation.owner_request())
                .unwrap_or_else(DaemonError::into_response)
        } else {
            self.route_node_operation(&pending.node_id, operation)
        };
        match response {
            Response::Error {
                code: Some(ErrorCode::NotFound),
                ..
            } => Ok(None),
            Response::Error { message, code } => Err(DaemonError::lifecycle(
                code.unwrap_or(ErrorCode::Internal),
                message,
            )),
            Response::RoutedNodeOperation { result } => Ok(Some(result)),
            response => routed_result(response).map(Some).map_err(|_| {
                DaemonError::lifecycle(
                    ErrorCode::Internal,
                    "owner returned an unexpected pending-resource response",
                )
            }),
        }
    }

    fn reconcile_pending_workspace_resources(&self) {
        let pending = match self
            .global_workspaces()
            .and_then(|store| store.pending_resources().map_err(global_workspace_error))
        {
            Ok(pending) => pending,
            Err(_) => return,
        };
        for pending in pending {
            let recovered = self.with_workspace_operation_lock(&pending.operation_id, || {
                let Some(resource) = self.pending_owner_resource(&pending)? else {
                    self.global_workspaces()?
                        .reconcile_resource(&pending, None)
                        .map_err(global_workspace_error)?;
                    return Ok(());
                };
                Self::validate_pending_owner_resource(&pending, &resource)?;
                let owner = self.owner_workspace(&protocol::QualifiedIdentity::new(
                    &pending.node_id,
                    &pending.owner_workspace_id,
                ))?;
                self.global_workspaces()?
                    .reconcile_resource(&pending, Some((&owner, &resource)))
                    .map_err(global_workspace_error)?;
                Ok::<(), DaemonError>(())
            });
            if let Err(error) = recovered {
                eprintln!(
                    "boomux: prepared Workspace resource {} remains unresolved: {error}",
                    pending.operation_id
                );
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn set_global_workspace_default_cwd(
        &self,
        operation_id: &str,
        global_workspace_id: &str,
        expected_global_revision: u64,
        node_id: &str,
        owner_workspace_id: &str,
        expected_owner_revision: u64,
        default_cwd: PathBuf,
    ) -> DaemonResult<protocol::WorkspaceDefaultCwdResult> {
        self.with_workspace_operation_lock(operation_id, || {
            let requested_default_cwd = default_cwd.clone();
            if let Some(result) = self
                .global_workspaces()?
                .completed_default_cwd(
                    operation_id,
                    global_workspace_id,
                    expected_global_revision,
                    node_id,
                    owner_workspace_id,
                    expected_owner_revision,
                    &requested_default_cwd,
                )
                .map_err(global_workspace_error)?
            {
                return Ok(result);
            }
            let local_node_id = self.node_identity()?.id()?;
            self.preflight_remote_workspace_feature(
                node_id,
                protocol::ProtocolFeature::WorkspacePlacementDefaultCwd,
            )?;
            let default_cwd = if node_id == local_node_id {
                host_services::resolve_directory(&default_cwd).map_err(DaemonError::from)?
            } else {
                match self.route_node_host_service(
                    node_id,
                    HostServiceOperation::ResolveDirectory { path: default_cwd },
                ) {
                    Response::HostService {
                        result: HostServiceResult::Directory { path },
                    } => path,
                    Response::Error { message, code } => {
                        return Err(DaemonError::lifecycle(
                            code.unwrap_or(ErrorCode::Internal),
                            message,
                        ));
                    }
                    _ => {
                        return Err(DaemonError::lifecycle(
                            ErrorCode::Internal,
                            "owner returned an unexpected directory response",
                        ));
                    }
                }
            };
            let prepared = self
                .global_workspaces()?
                .prepare_default_cwd(
                    operation_id,
                    global_workspace_id,
                    expected_global_revision,
                    node_id,
                    owner_workspace_id,
                    expected_owner_revision,
                    requested_default_cwd,
                    default_cwd.clone(),
                )
                .map_err(global_workspace_error)?;
            let mut pending = match prepared {
                PreparedDefaultCwdOperation::Completed(result) => return Ok(result),
                PreparedDefaultCwdOperation::Pending(pending) => pending,
            };
            let owner_identity = protocol::QualifiedIdentity::new(node_id, owner_workspace_id);
            let owner = self.owner_workspace(&owner_identity)?;
            if pending.owner_attempted
                && owner.default_cwd.as_ref() == Some(&pending.default_cwd)
                && matches!(
                    owner.revision.checked_sub(pending.expected_owner_revision),
                    Some(0 | 1)
                )
            {
                return self
                    .global_workspaces()?
                    .complete_default_cwd(&pending, &owner)
                    .map_err(global_workspace_error);
            }
            if owner.revision != expected_owner_revision {
                self.global_workspaces()?
                    .cancel_default_cwd(&pending)
                    .map_err(global_workspace_error)?;
                require_guard(owner.revision, expected_owner_revision, "owner Workspace")?;
            }
            let operation = RoutedOperation::SetWorkspaceDefaultCwd {
                workspace_id: pending.owner_workspace_id.clone(),
                expected_revision: pending.expected_owner_revision,
                default_cwd: pending.default_cwd.clone(),
            };
            let response = if pending.node_id == local_node_id {
                pending = self
                    .global_workspaces()?
                    .mark_default_cwd_owner_attempted(&pending)
                    .map_err(global_workspace_error)?;
                self.dispatch(operation.owner_request())
                    .unwrap_or_else(DaemonError::into_response)
            } else {
                let store = self.global_workspaces()?;
                let pending_node_id = pending.node_id.clone();
                let mut mark_attempted = || {
                    pending = store.mark_default_cwd_owner_attempted(&pending)?;
                    Ok(())
                };
                self.route_node_operation_after_handshake(
                    &pending_node_id,
                    operation,
                    &mut mark_attempted,
                )
            };
            let owner = match response {
                Response::Workspace { workspace } => workspace,
                Response::RoutedNodeOperation {
                    result: RoutedOperationResult::Workspace { workspace },
                } => workspace,
                Response::Error { message, code } => {
                    if !default_cwd_owner_error_is_ambiguous(code) {
                        self.global_workspaces()?
                            .cancel_default_cwd(&pending)
                            .map_err(global_workspace_error)?;
                    } else if !pending.owner_attempted {
                        self.global_workspaces()?
                            .cancel_default_cwd_if_never_attempted(&pending)
                            .map_err(global_workspace_error)?;
                    }
                    return Err(DaemonError::lifecycle(
                        code.unwrap_or(ErrorCode::Internal),
                        message,
                    ));
                }
                _ => {
                    return Err(DaemonError::lifecycle(
                        ErrorCode::Internal,
                        "owner returned an unexpected Workspace response",
                    ));
                }
            };
            self.global_workspaces()?
                .complete_default_cwd(&pending, &owner)
                .map_err(global_workspace_error)
        })
    }

    fn reconcile_pending_default_cwds(&self) {
        let pending = match self.global_workspaces().and_then(|store| {
            store
                .pending_default_cwd_operations()
                .map_err(global_workspace_error)
        }) {
            Ok(pending) => pending,
            Err(_) => return,
        };
        for operation in pending {
            let recovered = self.with_workspace_operation_lock(&operation.operation_id, || {
                if !operation.owner_attempted {
                    self.global_workspaces()?
                        .cancel_default_cwd_if_never_attempted(&operation)
                        .map_err(global_workspace_error)?;
                    return Ok(());
                }
                let owner = self.owner_workspace(&protocol::QualifiedIdentity::new(
                    &operation.node_id,
                    &operation.owner_workspace_id,
                ))?;
                if owner.default_cwd.as_ref() == Some(&operation.default_cwd)
                    && matches!(
                        owner
                            .revision
                            .checked_sub(operation.expected_owner_revision),
                        Some(0 | 1)
                    )
                {
                    self.global_workspaces()?
                        .complete_default_cwd(&operation, &owner)
                        .map_err(global_workspace_error)?;
                }
                Ok::<(), DaemonError>(())
            });
            if let Err(error) = recovered {
                eprintln!(
                    "boomux: prepared Workspace default cwd operation {} remains unresolved: {error}",
                    operation.operation_id
                );
            }
        }
    }

    fn validate_pending_owner_resource(
        pending: &PendingWorkspaceResource,
        resource: &RoutedOperationResult,
    ) -> DaemonResult<()> {
        let (kind, resource_id, workspace_id) = match resource {
            RoutedOperationResult::Shell { shell } => {
                (PendingResourceKind::Shell, &shell.id, &shell.workspace_id)
            }
            RoutedOperationResult::Launcher { launcher } => (
                PendingResourceKind::Launcher,
                &launcher.id,
                &launcher.workspace_id,
            ),
            _ => {
                return Err(DaemonError::lifecycle(
                    ErrorCode::Internal,
                    "owner returned an unexpected prepared resource type",
                ));
            }
        };
        if kind != pending.kind
            || resource_id != &pending.resource_id
            || workspace_id != &pending.owner_workspace_id
        {
            return Err(DaemonError::lifecycle(
                ErrorCode::OutcomeUnknown,
                "owner resource does not match the prepared identity",
            ));
        }
        Ok(())
    }

    fn open_global_workspace(
        &self,
        workspace_id: &str,
        expected_revision: u64,
    ) -> DaemonResult<protocol::GlobalWorkspaceOperationResult> {
        let workspace = self
            .global_workspaces()?
            .get(workspace_id)
            .map_err(global_workspace_error)?;
        require_guard(workspace.revision, expected_revision, "global Workspace")?;
        if workspace.closing {
            return Err(DaemonError::lifecycle(
                ErrorCode::Busy,
                "global Workspace close is in progress",
            ));
        }
        let placements = workspace
            .placements
            .iter()
            .map(|placement| {
                let identity = protocol::QualifiedIdentity::new(
                    placement.node_id.clone(),
                    placement.workspace_id.clone(),
                );
                match self.owner_workspace(&identity) {
                    Ok(owner) if owner.revision >= placement.owner_revision => {
                        protocol::WorkspacePlacementResult {
                            node_id: placement.node_id.clone(),
                            workspace_id: placement.workspace_id.clone(),
                            status: "available".into(),
                            message: None,
                        }
                    }
                    Ok(_) => protocol::WorkspacePlacementResult {
                        node_id: placement.node_id.clone(),
                        workspace_id: placement.workspace_id.clone(),
                        status: "stale".into(),
                        message: Some("owner Workspace revision moved backwards".into()),
                    },
                    Err(error) => protocol::WorkspacePlacementResult {
                        node_id: placement.node_id.clone(),
                        workspace_id: placement.workspace_id.clone(),
                        status: "unavailable".into(),
                        message: Some(error.to_string()),
                    },
                }
            })
            .collect();
        Ok(protocol::GlobalWorkspaceOperationResult {
            workspace,
            placements,
        })
    }

    fn close_global_workspace(
        &self,
        workspace_id: &str,
        expected_revision: Option<u64>,
    ) -> DaemonResult<protocol::GlobalWorkspaceOperationResult> {
        let mut workspace = self
            .global_workspaces()?
            .get(workspace_id)
            .map_err(global_workspace_error)?;
        if let Some(expected_revision) = expected_revision {
            workspace = self
                .global_workspaces()?
                .begin_close(workspace_id, expected_revision)
                .map_err(global_workspace_error)?;
        } else if !workspace.closing {
            return Err(DaemonError::lifecycle(
                ErrorCode::InvalidArgument,
                "global Workspace is not awaiting close retry",
            ));
        }
        if workspace.placements.is_empty() {
            self.global_workspaces()?
                .confirm_empty_closed(workspace_id)
                .map_err(global_workspace_error)?;
            return Ok(protocol::GlobalWorkspaceOperationResult {
                workspace,
                placements: Vec::new(),
            });
        }
        let local_node_id = self.node_identity()?.id()?;
        let mut results = Vec::new();
        for placement in workspace.placements.clone() {
            let identity = protocol::QualifiedIdentity::new(
                placement.node_id.clone(),
                placement.workspace_id.clone(),
            );
            let response = match self.owner_workspace(&identity) {
                Ok(owner) if placement.node_id == local_node_id => self
                    .dispatch(Request::GuardedCloseWorkspace {
                        workspace_id: placement.workspace_id.clone(),
                        expected_revision: owner.revision,
                    })
                    .unwrap_or_else(DaemonError::into_response),
                Ok(owner) => self.route_node_operation(
                    &placement.node_id,
                    RoutedOperation::CloseWorkspace {
                        workspace_id: placement.workspace_id.clone(),
                        expected_revision: owner.revision,
                    },
                ),
                Err(error) => error.into_response(),
            };
            let confirmed = matches!(response, Response::Ok)
                || matches!(
                    response,
                    Response::RoutedNodeOperation {
                        result: RoutedOperationResult::Ok
                    }
                )
                || matches!(
                    response,
                    Response::Error {
                        code: Some(ErrorCode::NotFound),
                        ..
                    }
                );
            if confirmed {
                let remaining = self
                    .global_workspaces()?
                    .confirm_closed(workspace_id, &placement.node_id, &placement.workspace_id)
                    .map_err(global_workspace_error)?;
                if let Some(remaining) = remaining {
                    workspace = remaining;
                } else {
                    workspace.placements.clear();
                }
                results.push(protocol::WorkspacePlacementResult {
                    node_id: placement.node_id,
                    workspace_id: placement.workspace_id,
                    status: "closed".into(),
                    message: None,
                });
            } else {
                let message = match response {
                    Response::Error { message, .. } => message,
                    _ => "owner returned an unexpected close response".into(),
                };
                results.push(protocol::WorkspacePlacementResult {
                    node_id: placement.node_id,
                    workspace_id: placement.workspace_id,
                    status: "unresolved".into(),
                    message: Some(message),
                });
            }
        }
        Ok(protocol::GlobalWorkspaceOperationResult {
            workspace,
            placements: results,
        })
    }

    fn combined_node_snapshot(
        &self,
        selector: Option<&str>,
    ) -> DaemonResult<crate::protocol::CombinedNodeSnapshot> {
        use crate::protocol::{CombinedNode, CombinedNodeSnapshot, NodeProjectionHealthCode};

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
            nodes.push(CombinedNode {
                node_id: local_node_id,
                alias: "local".into(),
                local: true,
                route: None,
                registration_revision: None,
                health: NodeProjectionHealthCode::Online,
                current: true,
                stale: false,
                observed_at_ms: unix_time_ms(),
                observed_protocol_version: Some(protocol::PROTOCOL_VERSION),
                observed_capabilities: self.runtime_protocol_capabilities(),
                observed_helper_version: Some(env!("CARGO_PKG_VERSION").into()),
                workspace_owner_eligible: self.global_workspaces.is_some(),
                workspace_owner_unavailable_reason: self
                    .global_workspaces
                    .is_none()
                    .then(|| "coordinated Workspace storage is unavailable".into()),
                local_snapshot: Some(snapshot),
                remote_projection: None,
            });
        }
        for registration in remote_selected {
            let view = self.node_projection_cache()?.view(registration)?;
            let observed_protocol_version = view
                .health
                .capabilities
                .iter()
                .filter_map(|capability| capability.strip_prefix("protocol_")?.parse().ok())
                .max();
            let workspace_owner_eligible = !view.health.stale
                && view.health.code == NodeProjectionHealthCode::Online
                && view
                    .health
                    .capabilities
                    .iter()
                    .any(|capability| capability == "global_workspaces");
            let workspace_owner_unavailable_reason = (!workspace_owner_eligible).then(|| {
                if view.health.stale || view.health.code != NodeProjectionHealthCode::Online {
                    format!("Node health is {:?}", view.health.code).to_ascii_lowercase()
                } else {
                    "Node does not support coordinated Workspaces".into()
                }
            });
            nodes.push(CombinedNode {
                node_id: registration.node_id.clone(),
                alias: registration.alias.clone(),
                local: false,
                route: Some(registration.target.clone()),
                registration_revision: Some(registration.revision),
                health: view.health.code,
                current: !view.health.stale,
                stale: view.health.stale,
                observed_at_ms: view.health.last_success_at_ms.unwrap_or(0),
                observed_protocol_version,
                observed_capabilities: view.health.capabilities,
                observed_helper_version: view.health.observed_helper_version,
                workspace_owner_eligible,
                workspace_owner_unavailable_reason,
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
        let mut workspaces = self
            .global_workspaces
            .as_ref()
            .map(GlobalWorkspaceStore::list)
            .transpose()?
            .unwrap_or_default();
        let node_current = nodes
            .iter()
            .map(|node| (node.node_id.as_str(), node.current && !node.stale))
            .collect::<HashMap<_, _>>();
        for workspace in &mut workspaces {
            for placement in &mut workspace.placements {
                if !workspace.closing
                    && !node_current
                        .get(placement.node_id.as_str())
                        .copied()
                        .unwrap_or(false)
                {
                    placement.state = protocol::WorkspacePlacementState::Unavailable;
                }
            }
        }
        let linked = workspaces
            .iter()
            .flat_map(|workspace| &workspace.placements)
            .map(|placement| (placement.node_id.clone(), placement.workspace_id.clone()))
            .collect::<HashSet<_>>();
        let mut external_workspaces = Vec::new();
        for node in &nodes {
            if let Some(snapshot) = &node.local_snapshot {
                external_workspaces.extend(
                    snapshot
                        .workspaces
                        .iter()
                        .filter(|workspace| {
                            !linked.contains(&(node.node_id.clone(), workspace.id.clone()))
                        })
                        .map(|workspace| protocol::ExternalWorkspaceSnapshot {
                            identity: protocol::QualifiedIdentity::new(
                                &node.node_id,
                                &workspace.id,
                            ),
                            revision: workspace.revision,
                            name: workspace.name.clone(),
                            default_cwd: workspace.default_cwd.clone(),
                            available: node.current && !node.stale,
                        }),
                );
            }
            if let Some(projection) = &node.remote_projection {
                external_workspaces.extend(
                    projection
                        .workspaces
                        .iter()
                        .filter(|workspace| {
                            !linked.contains(&(node.node_id.clone(), workspace.id.clone()))
                        })
                        .map(|workspace| protocol::ExternalWorkspaceSnapshot {
                            identity: protocol::QualifiedIdentity::new(
                                &node.node_id,
                                &workspace.id,
                            ),
                            revision: 0,
                            name: workspace.name.clone(),
                            default_cwd: None,
                            available: node.current && !node.stale,
                        }),
                );
            }
        }
        external_workspaces.sort_by(|left, right| {
            left.name
                .cmp(&right.name)
                .then_with(|| left.identity.node_id.cmp(&right.identity.node_id))
                .then_with(|| left.identity.inner_id.cmp(&right.identity.inner_id))
        });
        let focused_terminal = self
            .runtimes
            .presented_focused_terminal()?
            .filter(|focused| {
                nodes.iter().any(|node| {
                    node.node_id == focused.shell.node_id
                        && (node.local_snapshot.as_ref().is_some_and(|snapshot| {
                            snapshot.workspaces.iter().any(|workspace| {
                                workspace
                                    .shells
                                    .iter()
                                    .any(|shell| shell.id == focused.shell.inner_id)
                            })
                        }) || node.remote_projection.as_ref().is_some_and(|projection| {
                            projection
                                .shells
                                .iter()
                                .any(|shell| shell.id == focused.shell.inner_id)
                        }))
                })
            });
        Ok(CombinedNodeSnapshot {
            nodes,
            workspaces,
            external_workspaces,
            focused_terminal,
        })
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

    fn force_node_projection_refresh(
        &self,
        selector: &str,
    ) -> DaemonResult<protocol::NodeProjectionHealth> {
        let registration = self
            .node_registrations()?
            .inspect(selector)
            .map_err(node_registration_error)?;
        lock(&self.node_projection_workers.wake)?.insert(registration.node_id.clone());
        self.node_projection_cache()?
            .health(&registration)
            .map_err(DaemonError::from)
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

    fn dispatch_arc(
        self: &Arc<Self>,
        request: Request,
        response_version: u32,
    ) -> DaemonResult<Response> {
        if matches!(request, Request::AddNodeRegistration { .. }) {
            let response = self.dispatch_for_version(request, response_version)?;
            if let Response::NodeRegistration { registration } = &response {
                self.start_node_projection_worker(registration.node_id.clone())?;
            }
            return Ok(response);
        }
        self.dispatch_for_version(request, response_version)
    }

    fn clear_terminal_histories(&self) -> io::Result<()> {
        if self.durable.clear_terminal_histories()? {
            self.mark_persistence_dirty();
        }
        Ok(())
    }

    fn clock_now_ms(&self) -> u64 {
        unix_time_ms()
    }

    #[cfg(debug_assertions)]
    fn wait_for_native_start_barrier(&self) {}

    #[cfg(not(debug_assertions))]
    fn wait_for_native_start_barrier(&self) {}

    #[cfg(debug_assertions)]
    fn wait_for_native_outcome_barrier(&self) {}

    #[cfg(not(debug_assertions))]
    fn wait_for_native_outcome_barrier(&self) {}

    #[cfg(debug_assertions)]
    fn native_test_hooks_enabled(&self) -> bool {
        self.startup_environment
            .variables
            .iter()
            .any(|variable| variable.name == b"BOOMUX_NATIVE_TEST_HOOKS" && variable.value == b"1")
    }

    #[cfg(debug_assertions)]
    fn wait_for_native_first_resource_barrier(&self, operation_id: &str) -> DaemonResult<()> {
        if !self.native_test_hooks_enabled() {
            return Ok(());
        }
        let barrier =
            state_directory_from_environment()?.join(".native-test-first-resource-barrier");
        if !barrier.is_dir() {
            return Ok(());
        }
        fs::write(barrier.join(operation_id), b"")?;
        let deadline = Instant::now() + HANDSHAKE_TIMEOUT;
        while Instant::now() < deadline {
            if fs::read_dir(&barrier)?.count() >= 2 {
                return Ok(());
            }
            thread::sleep(IO_RETRY_DELAY);
        }
        Err(DaemonError::lifecycle(
            ErrorCode::Timeout,
            "native first-resource barrier timed out",
        ))
    }

    fn resumable_agent(
        &self,
        shell: &Shell,
        previous_run: Option<&PersistedShellRun>,
    ) -> io::Result<Option<ResumableAgent>> {
        if !self.notification_settings.resume_agents {
            return Ok(None);
        }
        let Some(previous_run) = previous_run
            .filter(|run| matches!(run.exit_reason, Some(ShellRunExitReason::Interrupted)))
        else {
            return Ok(None);
        };
        self.durable.resumable_agent(shell, previous_run)
    }

    fn acquire_kiro_launch_holder(
        &self,
        pid: u32,
        shell_id: String,
        run_id: String,
    ) -> DaemonResult<Response> {
        validate_id("shell", &shell_id)?;
        validate_id("run", &run_id)?;
        Self::validate_current_running_shell(&self.durable, &shell_id, &run_id)?;
        let (start_time, process_group_leader) =
            kiro_holder_process_evidence(pid, &shell_id, &run_id)?;
        let (response, holders) = self.durable_mutation_outcome(|undo| {
            let state = lock(&self.kiro.state)?;
            let mut holders = KiroHoldersMutation::new(state);
            let removed = prune_dead_kiro_holders(&mut holders.state);
            let events = self.inactivate_released_kiro_sessions(undo, &holders.state, removed)?;
            if holders.state.len() >= protocol::MAX_KIRO_LAUNCH_HOLDERS {
                return Err(DaemonError::lifecycle(
                    ErrorCode::Busy,
                    "Kiro launch holder capacity is exhausted",
                ));
            }
            Self::validate_current_running_shell(&self.durable, &shell_id, &run_id)?;
            if kiro_holder_process_evidence(pid, &shell_id, &run_id)?
                != (start_time, process_group_leader)
            {
                return Err(DaemonError::lifecycle(
                    ErrorCode::NotFound,
                    "Kiro launch holder process identity changed before acquisition",
                ));
            }
            let holder_id = Uuid::new_v4().to_string();
            holders.state.insert(
                holder_id.clone(),
                KiroLaunchHolder {
                    pid,
                    start_time,
                    process_group_leader,
                    shell_id,
                    run_id,
                    sessions: HashMap::new(),
                },
            );
            let value = (Response::KiroLaunchHolder { holder_id }, holders);
            if events.is_empty() {
                Ok(DurableMutation::Unchanged(value))
            } else {
                Ok(DurableMutation::Changed(value, events))
            }
        })?;
        holders.commit();
        Ok(response)
    }

    fn report_kiro_hook(
        &self,
        holder_id: &str,
        session_id: String,
        report: AgentReport,
    ) -> DaemonResult<Response> {
        validate_uuid(holder_id, "Kiro launch holder ID")?;
        crate::integrations::validate_external_session_id(&session_id)
            .map_err(|error| DaemonError::validation(error.to_string()))?;
        validate_agent_report(&report)?;
        if report.authority != AgentAuthority::LifecycleIntegration
            || !matches!(
                report.state,
                AgentState::Unknown | AgentState::Working | AgentState::Idle
            )
        {
            return Err(DaemonError::validation(
                "Kiro hooks may report only Unknown, Working, or Idle at lifecycle integration authority",
            ));
        }
        let (response, holders) = self.durable_mutation_outcome(|undo| {
            let state = lock(&self.kiro.state)?;
            let mut holders = KiroHoldersMutation::new(state);
            let removed = prune_dead_kiro_holders(&mut holders.state);
            let mut events =
                self.inactivate_released_kiro_sessions(undo, &holders.state, removed)?;
            let Some(holder) = holders.state.get(holder_id).cloned() else {
                let error =
                    DaemonError::lifecycle(ErrorCode::NotFound, "Kiro launch holder is not live");
                let value = (Err(error), holders);
                return if events.is_empty() {
                    Ok(DurableMutation::Unchanged(value))
                } else {
                    Ok(DurableMutation::Changed(value, events))
                };
            };
            Self::validate_current_running_shell(&self.durable, &holder.shell_id, &holder.run_id)?;
            if !holder.sessions.contains_key(&session_id)
                && holder.sessions.len() >= protocol::MAX_KIRO_HOLDER_SESSIONS
            {
                return Err(DaemonError::lifecycle(
                    ErrorCode::Busy,
                    "Kiro launch holder Session capacity is exhausted",
                ));
            }
            let spec = AgentRegistrationSpec {
                name: "Kiro CLI".into(),
                integration: "kiro".into(),
                external_session_id: Some(session_id.clone()),
                report: report.clone(),
            };
            let (agent, created, record) =
                self.durable
                    .ensure_agent(&holder.shell_id, &holder.run_id, spec)?;
            if let Some(record) = record {
                undo.record(record);
            }
            if created {
                events.push(DaemonEventKind::AgentRegistered {
                    workspace_id: agent.workspace_id.clone(),
                    shell_id: agent.shell_id.clone(),
                    agent: agent.clone(),
                });
            }
            holders
                .state
                .get_mut(holder_id)
                .expect("live Kiro holder disappeared while locked")
                .sessions
                .insert(session_id, agent.id.clone());
            let (agent, changed, completed) =
                self.report_agent_mutation(undo, &agent.id, &holder.run_id, report)?;
            debug_assert!(!completed);
            if changed {
                events.push(DaemonEventKind::AgentStateChanged {
                    workspace_id: agent.workspace_id.clone(),
                    shell_id: agent.shell_id.clone(),
                    agent: agent.clone(),
                });
            }
            let value = (Ok(Response::Agent { agent }), holders);
            if events.is_empty() {
                Ok(DurableMutation::Unchanged(value))
            } else {
                Ok(DurableMutation::Changed(value, events))
            }
        })?;
        holders.commit();
        response
    }

    fn release_kiro_launch_holder(&self, holder_id: &str) -> DaemonResult<Response> {
        validate_uuid(holder_id, "Kiro launch holder ID")?;
        let (released, holders) = self.durable_mutation_outcome(|undo| {
            let state = lock(&self.kiro.state)?;
            let mut holders = KiroHoldersMutation::new(state);
            let mut removed = prune_dead_kiro_holders(&mut holders.state);
            let released = holders.state.remove(holder_id);
            let found = released.is_some() || removed.iter().any(|(id, _)| id == holder_id);
            if let Some(holder) = released {
                removed.push((holder_id.to_owned(), holder));
            }
            let events = self.inactivate_released_kiro_sessions(undo, &holders.state, removed)?;
            let value = (found, holders);
            if events.is_empty() {
                Ok(DurableMutation::Unchanged(value))
            } else {
                Ok(DurableMutation::Changed(value, events))
            }
        })?;
        holders.commit();
        Ok(Response::KiroLaunchHolderReleased { released })
    }

    fn inactivate_released_kiro_sessions(
        &self,
        undo: &mut DurableUndoLog,
        remaining: &HashMap<String, KiroLaunchHolder>,
        removed: Vec<(String, KiroLaunchHolder)>,
    ) -> DaemonResult<Vec<DaemonEventKind>> {
        let mut events = Vec::new();
        let mut handled = HashSet::new();
        for (_, holder) in removed {
            for (session_id, agent_id) in holder.sessions {
                let key = (
                    holder.shell_id.clone(),
                    holder.run_id.clone(),
                    session_id.clone(),
                );
                if !handled.insert(key)
                    || remaining.values().any(|candidate| {
                        candidate.shell_id == holder.shell_id
                            && candidate.run_id == holder.run_id
                            && candidate.sessions.contains_key(&session_id)
                    })
                {
                    continue;
                }
                if !lock(&self.durable.state)?.agents.contains_key(&agent_id) {
                    continue;
                }
                let report = AgentReport {
                    state: AgentState::Inactive,
                    authority: AgentAuthority::LifecycleIntegration,
                    evidence: "Kiro managed process exited".into(),
                    confidence: 100,
                };
                let (agent, changed, completed) =
                    self.report_agent_mutation(undo, &agent_id, &holder.run_id, report)?;
                debug_assert!(!completed);
                if changed {
                    events.push(DaemonEventKind::AgentStateChanged {
                        workspace_id: agent.workspace_id.clone(),
                        shell_id: agent.shell_id.clone(),
                        agent,
                    });
                }
            }
        }
        Ok(events)
    }

    fn ensure_opencode_session_claim(
        &self,
        generation_id: String,
        holder_id: String,
        root_session_id: String,
        shell_id: String,
        run_id: String,
        spec: AgentRegistrationSpec,
    ) -> DaemonResult<Response> {
        validate_opencode_uuid("OpenCode runtime generation ID", &generation_id)?;
        validate_opencode_claim_id("OpenCode claim holder ID", &holder_id)?;
        validate_opencode_claim_id("OpenCode root session ID", &root_session_id)?;
        validate_id("shell", &shell_id)?;
        validate_id("run", &run_id)?;
        if spec.integration != "opencode"
            || spec.external_session_id.as_deref() != Some(root_session_id.as_str())
        {
            return Err(DaemonError::validation(
                "OpenCode claim spec must use integration opencode and the exact root session ID",
            ));
        }
        validate_agent_registration(&spec)?;
        validate_external_agent_authority(spec.report.authority)?;
        if spec.report.state == AgentState::Done {
            return Err(DaemonError::validation(
                "OpenCode claim spec must describe a non-completed Agent",
            ));
        }

        let now = Instant::now();
        let ((claim, agent), claims) = self.durable_mutation_outcome(|undo| {
            let coordinator = lock(&self.opencode.state)?;
            let mut claims = OpenCodeClaimsMutation::new(coordinator);
            claims.state.require_generation(&generation_id)?;
            claims.state.prune_claims(now);
            if let Some(existing) = claims.state.claims.get(&root_session_id).cloned() {
                let authority = {
                    let durable = lock(&self.durable.state)?;
                    Self::opencode_claim_authority(&durable, &root_session_id, &existing)
                };
                if authority.is_err() {
                    claims.state.claims.remove(&root_session_id);
                } else if existing.shell_id != shell_id || existing.run_id != run_id {
                    return Err(DaemonError::lifecycle(
                        ErrorCode::Busy,
                        "OpenCode root session is claimed by a different ShellRun",
                    ));
                }
            }
            Self::validate_current_running_shell(&self.durable, &shell_id, &run_id)?;
            let holder_already_present = claims
                .state
                .claims
                .values()
                .any(|claim| claim.holders.contains_key(&holder_id));
            let new_root = !claims.state.claims.contains_key(&root_session_id);
            let old_holder_only_root = claims.state.claims.values().any(|claim| {
                claim.holders.len() == 1
                    && claim.holders.contains_key(&holder_id)
                    && !claims.state.claims.contains_key(&root_session_id)
            });
            let projected_roots = claims.state.claims.len() + usize::from(new_root)
                - usize::from(old_holder_only_root);
            if projected_roots > MAX_OPENCODE_CLAIM_ROOTS
                || (claims.state.holder_count() >= MAX_OPENCODE_CLAIM_HOLDERS
                    && !holder_already_present)
            {
                return Err(DaemonError::lifecycle(
                    ErrorCode::Busy,
                    "OpenCode claim capacity is exhausted",
                ));
            }
            let (agent, created, record) = self.durable.ensure_agent(&shell_id, &run_id, spec)?;
            if let Some(record) = record {
                undo.record(record);
            }
            if agent.ended_at_ms.is_some() || agent.observation.state == AgentState::Done {
                return Err(DaemonError::lifecycle(
                    ErrorCode::NotFound,
                    "exact active OpenCode Agent was not found",
                ));
            }
            let mut events = Vec::new();
            if created {
                events.push(DaemonEventKind::AgentRegistered {
                    workspace_id: agent.workspace_id.clone(),
                    shell_id: agent.shell_id.clone(),
                    agent: agent.clone(),
                });
            }
            for claim in claims.state.claims.values_mut() {
                claim.holders.remove(&holder_id);
            }
            let removed_roots = claims
                .state
                .claims
                .iter()
                .filter(|(root, claim)| *root != &root_session_id && claim.holders.is_empty())
                .map(|(root, _)| root.clone())
                .collect::<Vec<_>>();
            let removed = removed_roots
                .into_iter()
                .filter_map(|root| claims.state.claims.remove(&root))
                .collect();
            events.extend(self.inactivate_released_opencode_agents(undo, removed)?);
            let expires_at_ms = unix_time_ms().saturating_add(
                u64::try_from(OPENCODE_CLAIM_HOLDER_TTL.as_millis()).unwrap_or(u64::MAX),
            );
            let claim = claims
                .state
                .claims
                .entry(root_session_id.clone())
                .or_insert_with(|| OpenCodeRootClaim {
                    claim_id: Uuid::new_v4().to_string(),
                    workspace_id: agent.workspace_id.clone(),
                    shell_id: shell_id.clone(),
                    run_id: run_id.clone(),
                    agent_id: agent.id.clone(),
                    selected_holder_id: holder_id.clone(),
                    holders: HashMap::new(),
                });
            claim.workspace_id = agent.workspace_id.clone();
            claim.agent_id = agent.id.clone();
            claim.selected_holder_id = holder_id.clone();
            claim.holders.insert(
                holder_id.clone(),
                OpenCodeClaimHolder {
                    expires_at: now + OPENCODE_CLAIM_HOLDER_TTL,
                    expires_at_ms,
                },
            );
            let claim = claims
                .state
                .snapshot(&generation_id, &root_session_id, &holder_id)?;
            let value = ((claim, agent), claims);
            if events.is_empty() {
                Ok(DurableMutation::Unchanged(value))
            } else {
                Ok(DurableMutation::Changed(value, events))
            }
        })?;
        claims.commit();
        Ok(Response::OpenCodeSessionClaim { claim, agent })
    }

    fn resolve_opencode_session_claim(
        &self,
        generation_id: &str,
        root_session_id: &str,
    ) -> DaemonResult<Response> {
        validate_opencode_uuid("OpenCode runtime generation ID", generation_id)?;
        validate_opencode_claim_id("OpenCode root session ID", root_session_id)?;
        let mut coordinator = lock(&self.opencode.state)?;
        coordinator.require_generation(generation_id)?;
        coordinator.prune_claims(Instant::now());
        let claim = coordinator
            .claims
            .get(root_session_id)
            .cloned()
            .ok_or_else(|| {
                DaemonError::lifecycle(ErrorCode::NotFound, "OpenCode session claim not found")
            })?;
        let agent = {
            let durable = lock(&self.durable.state)?;
            Self::opencode_claim_authority(&durable, root_session_id, &claim)
        };
        let agent = match agent {
            Ok(agent) => agent,
            Err(error) => {
                coordinator.claims.remove(root_session_id);
                return Err(error);
            }
        };
        let snapshot =
            coordinator.snapshot(generation_id, root_session_id, &claim.selected_holder_id)?;
        Ok(Response::OpenCodeSessionClaim {
            claim: snapshot,
            agent,
        })
    }

    fn release_opencode_session_claim(
        &self,
        generation_id: &str,
        holder_id: &str,
        claim_id: &str,
    ) -> DaemonResult<Response> {
        validate_opencode_uuid("OpenCode runtime generation ID", generation_id)?;
        validate_opencode_claim_id("OpenCode claim holder ID", holder_id)?;
        validate_opencode_uuid("OpenCode claim ID", claim_id)?;
        let (released, claims) = self.durable_mutation_outcome(|undo| {
            let coordinator = lock(&self.opencode.state)?;
            let mut claims = OpenCodeClaimsMutation::new(coordinator);
            claims.state.require_generation(generation_id)?;
            claims.state.prune_claims(Instant::now());
            let root = claims.state.claims.iter().find_map(|(root, claim)| {
                (claim.claim_id == claim_id && claim.holders.contains_key(holder_id))
                    .then(|| root.clone())
            });
            let mut removed = Vec::new();
            let released = if let Some(root) = root {
                let claim = claims
                    .state
                    .claims
                    .get_mut(&root)
                    .expect("matching claim disappeared while locked");
                claim.holders.remove(holder_id);
                if claim.holders.is_empty() {
                    removed.push(
                        claims
                            .state
                            .claims
                            .remove(&root)
                            .expect("empty OpenCode claim disappeared while locked"),
                    );
                } else if claim.selected_holder_id == holder_id {
                    claim.selected_holder_id = claim.holders.keys().min().cloned().unwrap();
                }
                true
            } else {
                false
            };
            let events = self.inactivate_released_opencode_agents(undo, removed)?;
            let value = (released, claims);
            if events.is_empty() {
                Ok(DurableMutation::Unchanged(value))
            } else {
                Ok(DurableMutation::Changed(value, events))
            }
        })?;
        claims.commit();
        Ok(Response::OpenCodeSessionClaimReleased { released })
    }

    fn inactivate_released_opencode_agents(
        &self,
        undo: &mut DurableUndoLog,
        removed: Vec<OpenCodeRootClaim>,
    ) -> DaemonResult<Vec<DaemonEventKind>> {
        let mut events = Vec::new();
        let mut handled = HashSet::new();
        for claim in removed {
            if !handled.insert(claim.agent_id.clone())
                || !lock(&self.durable.state)?
                    .agents
                    .contains_key(&claim.agent_id)
            {
                continue;
            }
            let report = AgentReport {
                state: AgentState::Inactive,
                authority: AgentAuthority::LifecycleIntegration,
                evidence: "OpenCode session claim released".into(),
                confidence: 100,
            };
            let (agent, changed, completed) =
                self.report_agent_mutation(undo, &claim.agent_id, &claim.run_id, report)?;
            debug_assert!(!completed);
            if changed {
                events.push(DaemonEventKind::AgentStateChanged {
                    workspace_id: agent.workspace_id.clone(),
                    shell_id: agent.shell_id.clone(),
                    agent,
                });
            }
        }
        Ok(events)
    }

    fn report_claimed_opencode_agent(
        &self,
        generation_id: &str,
        root_session_id: &str,
        report: AgentReport,
    ) -> DaemonResult<Response> {
        validate_opencode_uuid("OpenCode runtime generation ID", generation_id)?;
        validate_opencode_claim_id("OpenCode root session ID", root_session_id)?;
        let (response, claims) = self.durable_mutation_outcome(|undo| {
            let coordinator = lock(&self.opencode.state)?;
            let mut claims = OpenCodeClaimsMutation::new(coordinator);
            claims.state.require_generation(generation_id)?;
            claims.state.prune_claims(Instant::now());
            let claim = claims
                .state
                .claims
                .get(root_session_id)
                .cloned()
                .ok_or_else(|| {
                    DaemonError::lifecycle(ErrorCode::NotFound, "OpenCode session claim not found")
                })?;
            {
                let durable = lock(&self.durable.state)?;
                if let Err(error) =
                    Self::opencode_claim_authority(&durable, root_session_id, &claim)
                {
                    claims.state.claims.remove(root_session_id);
                    return Ok(DurableMutation::Unchanged((Err(error), claims)));
                }
            }
            // Revalidate the exact current run while the mutation gate remains
            // held, immediately before changing the claimed Agent.
            Self::validate_current_running_shell(&self.durable, &claim.shell_id, &claim.run_id)?;
            let (agent, changed, completed) =
                self.report_agent_mutation(undo, &claim.agent_id, &claim.run_id, report)?;
            if completed {
                claims.state.claims.remove(root_session_id);
            }
            let value = (Ok((agent.clone(), completed)), claims);
            if !changed {
                return Ok(DurableMutation::Unchanged(value));
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
            Ok(DurableMutation::Changed(value, vec![event]))
        })?;
        claims.commit();
        let (agent, _completed) = response?;
        Ok(Response::Agent { agent })
    }

    fn validate_current_running_shell(
        durable: &DurableRegistry,
        shell_id: &str,
        run_id: &str,
    ) -> DaemonResult<()> {
        let state = lock(&durable.state)?;
        let shell = state.shells.get(shell_id).ok_or_else(|| {
            DaemonError::lifecycle(ErrorCode::RunChanged, "authority shell does not exist")
        })?;
        if matches!(&*lock(&shell.lifecycle)?, ShellLifecycle::Running { run, .. } if run.id == run_id)
        {
            Ok(())
        } else {
            Err(DaemonError::lifecycle(
                ErrorCode::RunChanged,
                "authority does not match the current running ShellRun",
            ))
        }
    }

    fn opencode_claim_authority(
        state: &DurableState,
        root_session_id: &str,
        claim: &OpenCodeRootClaim,
    ) -> DaemonResult<AgentInstanceSnapshot> {
        let shell = state.shells.get(&claim.shell_id).ok_or_else(|| {
            DaemonError::lifecycle(
                ErrorCode::RunChanged,
                "OpenCode claim shell no longer exists",
            )
        })?;
        if !matches!(&*lock(&shell.lifecycle)?, ShellLifecycle::Running { run, .. } if run.id == claim.run_id)
        {
            return Err(DaemonError::lifecycle(
                ErrorCode::RunChanged,
                "OpenCode claim no longer matches the current running ShellRun",
            ));
        }
        let mut active = Vec::new();
        for agent in state.agents.values().filter(|agent| {
            agent.integration == "opencode"
                && agent.external_session_id.as_deref() == Some(root_session_id)
                && agent.shell_id == claim.shell_id
                && agent.run_id == claim.run_id
        }) {
            let agent_state = lock(&agent.state)?;
            let is_active = agent_state.ended_at_ms.is_none()
                && agent_state.observation.state != AgentState::Done;
            drop(agent_state);
            if is_active {
                active.push(agent.snapshot()?);
            }
        }
        match active.as_slice() {
            [agent] if agent.id == claim.agent_id => Ok(agent.clone()),
            [] => Err(DaemonError::lifecycle(
                ErrorCode::NotFound,
                "exact active OpenCode Agent for claim was not found",
            )),
            _ => Err(DaemonError::lifecycle(
                ErrorCode::AlreadyExists,
                "OpenCode claim Agent is missing or ambiguous",
            )),
        }
    }

    fn set_claude_remote_control_binding(
        &self,
        agent_id: &str,
        shell_id: &str,
        run_id: &str,
        bridge_session_id: Option<String>,
    ) -> DaemonResult<Option<ClaudeRemoteControlBindingSnapshot>> {
        if let Some(bridge_session_id) = bridge_session_id.as_deref() {
            validate_claude_bridge_session_id(bridge_session_id)?;
        }
        let _mutation = lock(&self.mutation_lock)?;
        let durable = lock(&self.durable.state)?;
        if bridge_session_id.is_some() {
            validate_claude_binding_authority(&durable, agent_id, shell_id, run_id)?;
        } else {
            validate_claude_binding_identity(&durable, agent_id, shell_id, run_id)?;
        }
        let mut bindings = lock(&self.claude_remote_control.state)?;
        prune_claude_bindings(&durable, &mut bindings)?;
        let Some(bridge_session_id) = bridge_session_id else {
            bindings.remove(agent_id);
            return Ok(None);
        };
        if bindings.iter().any(|(candidate_agent_id, binding)| {
            candidate_agent_id != agent_id && binding.bridge_session_id == bridge_session_id
        }) {
            return Err(DaemonError::lifecycle(
                ErrorCode::Busy,
                "Claude Remote Control session is already bound to another Agent",
            ));
        }
        if !bindings.contains_key(agent_id)
            && bindings.len() >= protocol::MAX_CLAUDE_REMOTE_CONTROL_BINDINGS
        {
            return Err(DaemonError::lifecycle(
                ErrorCode::Busy,
                "Claude Remote Control binding capacity is exhausted",
            ));
        }
        let binding = ClaudeRemoteControlBindingSnapshot {
            agent_id: agent_id.into(),
            shell_id: shell_id.into(),
            run_id: run_id.into(),
            bridge_session_id,
        };
        bindings.insert(agent_id.into(), binding.clone());
        Ok(Some(binding))
    }

    fn get_claude_remote_control_binding(
        &self,
        agent_id: &str,
        shell_id: &str,
        run_id: &str,
    ) -> DaemonResult<Option<ClaudeRemoteControlBindingSnapshot>> {
        let _mutation = lock(&self.mutation_lock)?;
        let durable = lock(&self.durable.state)?;
        validate_claude_binding_authority(&durable, agent_id, shell_id, run_id)?;
        let mut bindings = lock(&self.claude_remote_control.state)?;
        prune_claude_bindings(&durable, &mut bindings)?;
        Ok(bindings
            .get(agent_id)
            .cloned()
            .filter(|binding| binding.shell_id == shell_id && binding.run_id == run_id))
    }

    fn export_claude_remote_control_bindings(
        &self,
    ) -> DaemonResult<Vec<ClaudeRemoteControlBindingSnapshot>> {
        let durable = lock(&self.durable.state)?;
        let mut bindings = lock(&self.claude_remote_control.state)?;
        prune_claude_bindings(&durable, &mut bindings)?;
        let mut bindings = bindings.values().cloned().collect::<Vec<_>>();
        bindings.sort_by(|left, right| left.agent_id.cmp(&right.agent_id));
        Ok(bindings)
    }

    fn import_claude_remote_control_bindings(
        &self,
        transferred: Vec<ClaudeRemoteControlBindingSnapshot>,
    ) -> io::Result<()> {
        let durable = lock(&self.durable.state)?;
        let mut bindings = lock(&self.claude_remote_control.state)?;
        if !bindings.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "replacement daemon already has Claude Remote Control bindings",
            ));
        }
        for binding in transferred {
            validate_claude_bridge_session_id(&binding.bridge_session_id)?;
            validate_claude_binding_authority(
                &durable,
                &binding.agent_id,
                &binding.shell_id,
                &binding.run_id,
            )
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error.to_string()))?;
            if bindings
                .values()
                .any(|existing| existing.bridge_session_id == binding.bridge_session_id)
            {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "duplicate Claude bridge session ID in handoff",
                ));
            }
            bindings.insert(binding.agent_id.clone(), binding);
        }
        Ok(())
    }

    fn reconcile_dead_kiro_holders(&self) -> DaemonResult<()> {
        let holders = self.durable_mutation_outcome(|undo| {
            let state = lock(&self.kiro.state)?;
            let mut holders = KiroHoldersMutation::new(state);
            let removed = prune_dead_kiro_holders(&mut holders.state);
            let mut events =
                self.inactivate_released_kiro_sessions(undo, &holders.state, removed)?;
            events.extend(self.inactivate_unowned_kiro_agents(undo, &holders.state)?);
            if events.is_empty() {
                Ok(DurableMutation::Unchanged(holders))
            } else {
                Ok(DurableMutation::Changed(holders, events))
            }
        })?;
        holders.commit();
        Ok(())
    }

    fn inactivate_unowned_kiro_agents(
        &self,
        undo: &mut DurableUndoLog,
        holders: &HashMap<String, KiroLaunchHolder>,
    ) -> DaemonResult<Vec<DaemonEventKind>> {
        let owned = holders
            .values()
            .flat_map(|holder| holder.sessions.values().cloned())
            .collect::<HashSet<_>>();
        let candidates = lock(&self.durable.state)?
            .agents
            .values()
            .filter(|agent| agent.integration == "kiro" && !owned.contains(&agent.id))
            .cloned()
            .collect::<Vec<_>>();
        let mut events = Vec::new();
        for agent in candidates {
            let should_inactivate = {
                let state = lock(&agent.state)?;
                state.ended_at_ms.is_none()
                    && state.observation.authority == AgentAuthority::LifecycleIntegration
                    && !matches!(
                        state.observation.state,
                        AgentState::Inactive | AgentState::Done
                    )
            };
            if !should_inactivate {
                continue;
            }
            let report = AgentReport {
                state: AgentState::Inactive,
                authority: AgentAuthority::LifecycleIntegration,
                evidence: "Kiro launch authority unavailable".into(),
                confidence: 100,
            };
            let (agent, changed, completed) =
                self.report_agent_mutation(undo, &agent.id, &agent.run_id, report)?;
            debug_assert!(!completed);
            if changed {
                events.push(DaemonEventKind::AgentStateChanged {
                    workspace_id: agent.workspace_id.clone(),
                    shell_id: agent.shell_id.clone(),
                    agent,
                });
            }
        }
        Ok(events)
    }

    fn export_kiro_launch_holders(&self) -> io::Result<Vec<handoff::KiroLaunchHolderManifest>> {
        let holders = lock(&self.kiro.state)?;
        let mut manifests = holders
            .iter()
            .map(|(holder_id, holder)| handoff::KiroLaunchHolderManifest {
                holder_id: holder_id.clone(),
                pid: holder.pid,
                start_time: holder.start_time,
                process_group_leader: holder.process_group_leader,
                shell_id: holder.shell_id.clone(),
                run_id: holder.run_id.clone(),
                sessions: holder
                    .sessions
                    .iter()
                    .map(|(session, agent)| (session.clone(), agent.clone()))
                    .collect(),
            })
            .collect::<Vec<_>>();
        manifests.sort_by(|left, right| left.holder_id.cmp(&right.holder_id));
        Ok(manifests)
    }

    fn import_kiro_launch_holders(
        &self,
        transferred: Vec<handoff::KiroLaunchHolderManifest>,
    ) -> io::Result<()> {
        let durable = lock(&self.durable.state)?;
        let mut holders = lock(&self.kiro.state)?;
        if !holders.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "replacement daemon already has Kiro launch holders",
            ));
        }
        let mut imported = HashMap::with_capacity(transferred.len());
        for manifest in transferred {
            let holder = KiroLaunchHolder {
                pid: manifest.pid,
                start_time: manifest.start_time,
                process_group_leader: manifest.process_group_leader,
                shell_id: manifest.shell_id,
                run_id: manifest.run_id,
                sessions: manifest.sessions.into_iter().collect(),
            };
            if !kiro_holder_process_evidence(holder.pid, &holder.shell_id, &holder.run_id)
                .is_ok_and(|evidence| evidence == (holder.start_time, holder.process_group_leader))
                || !kiro_holder_matches_current_run(&durable, &holder)?
                || holder.sessions.iter().any(|(session_id, agent_id)| {
                    durable.agents.get(agent_id).is_none_or(|agent| {
                        if agent.integration != "kiro"
                            || agent.external_session_id.as_deref() != Some(session_id)
                            || agent.shell_id != holder.shell_id
                            || agent.run_id != holder.run_id
                        {
                            return true;
                        }
                        match lock(&agent.state) {
                            Ok(state) => {
                                state.ended_at_ms.is_some()
                                    || matches!(
                                        state.observation.state,
                                        AgentState::Inactive | AgentState::Done
                                    )
                            }
                            Err(_) => true,
                        }
                    })
                })
            {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "Kiro launch holder handoff authority is no longer valid",
                ));
            }
            imported.insert(manifest.holder_id, holder);
        }
        *holders = imported;
        Ok(())
    }

    fn dispatch(&self, request: Request) -> DaemonResult<Response> {
        self.dispatch_for_version(request, protocol::PROTOCOL_VERSION)
    }

    fn dispatch_for_version(
        &self,
        request: Request,
        requester_version: u32,
    ) -> DaemonResult<Response> {
        let reconcile_combined_snapshot =
            if matches!(&request, Request::GetCombinedNodeSnapshot { .. }) {
                self.global_workspaces
                    .as_ref()
                    .map(|store| {
                        Ok::<_, io::Error>(
                            !store.pending_resources()?.is_empty()
                                || !store.pending_default_cwd_operations()?.is_empty(),
                        )
                    })
                    .transpose()
                    .map_err(global_workspace_error)?
                    .unwrap_or(false)
            } else {
                false
            };
        let skip_local_shell_frontier = match &request {
            Request::Ping | Request::Snapshot | Request::Events { .. } => true,
            Request::GetCombinedNodeSnapshot { .. } => !reconcile_combined_snapshot,
            _ => false,
        };
        if !skip_local_shell_frontier {
            self.checkpoint_local_shell_transactions()
                .map_err(DaemonError::persistence)?;
        }
        let (workspace_request_fingerprint, workspace_semantic_fingerprint) =
            workspace_operation_fingerprints(&request)?;
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
            Request::BeginNodeUpgradeMaintenance { .. } => {
                unreachable!("Node upgrade maintenance begin is handled before dispatch")
            }
            Request::FinishNodeUpgradeMaintenance { node_id, token } => self
                .node_registrations()?
                .finish_upgrade_maintenance(&node_id, &token)
                .map(|()| Response::Ok)
                .map_err(node_registration_error),
            Request::FinishNodeUninstallMaintenance { node_id, token } => {
                let registration = self
                    .node_registrations()?
                    .finish_uninstall_maintenance(&node_id, &token)
                    .map_err(node_registration_error)?;
                if let Err(error) = self.node_projection_cache()?.remove(&registration.node_id) {
                    eprintln!("boomux: could not remove disposable Node projection: {error}");
                }
                Ok(Response::NodeRegistration { registration })
            }
            Request::RenewNodeUpgradeMaintenance { node_id, token } => self
                .node_registrations()?
                .renew_upgrade_maintenance(&node_id, &token, NODE_UPGRADE_MAINTENANCE_LEASE)
                .map(|()| Response::Ok)
                .map_err(node_registration_error),
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
            Request::ForceNodeProjectionRefresh { selector } => {
                Ok(Response::NodeProjectionHealth {
                    health: self.force_node_projection_refresh(&selector)?,
                })
            }
            Request::DismissNodeProjectionShell { node_id, shell_id } => {
                let registration = self
                    .node_registrations()?
                    .inspect(&node_id)
                    .map_err(node_registration_error)?;
                let (health, changed) = self
                    .node_projection_cache()?
                    .dismiss_shell(&registration, &shell_id)?;
                if changed {
                    let _ = self.events.publish_runtime_batch(vec![
                        DaemonEventKind::NodeProjectionChanged {
                            node_id,
                            cache_generation: health.cache_generation,
                        },
                    ]);
                }
                Ok(Response::NodeProjectionHealth { health })
            }
            Request::RestoreDismissedNodeProjectionShells { node_id } => {
                let registration = self
                    .node_registrations()?
                    .inspect(&node_id)
                    .map_err(node_registration_error)?;
                let (health, changed) = self
                    .node_projection_cache()?
                    .restore_dismissed_shells(&registration)?;
                if changed {
                    let _ = self.events.publish_runtime_batch(vec![
                        DaemonEventKind::NodeProjectionChanged {
                            node_id,
                            cache_generation: health.cache_generation,
                        },
                    ]);
                }
                Ok(Response::NodeProjectionHealth { health })
            }
            Request::GetCombinedNodeSnapshot { selector } => {
                if reconcile_combined_snapshot {
                    self.reconcile_pending_workspace_resources();
                    self.reconcile_pending_default_cwds();
                }
                Ok(Response::CombinedNodeSnapshot {
                    snapshot: self.combined_node_snapshot(selector.as_deref())?,
                })
            }
            Request::CreateGlobalWorkspace { name } => Ok(Response::GlobalWorkspace {
                workspace: self
                    .global_workspaces()?
                    .create(name)
                    .map_err(global_workspace_error)?,
            }),
            Request::AdoptNodeWorkspace {
                identity,
                expected_revision,
            } => {
                self.require_cached_workspace_owner_eligible(&identity.node_id)?;
                let node_id = identity.node_id.clone();
                self.with_live_workspace_node(&identity.node_id, |node| {
                    let owner = node
                        .local_snapshot
                        .and_then(|snapshot| {
                            snapshot
                                .workspaces
                                .into_iter()
                                .find(|workspace| workspace.id == identity.inner_id)
                        })
                        .ok_or_else(|| {
                            DaemonError::lifecycle(
                                ErrorCode::NotFound,
                                "owner Workspace not found in live capability snapshot",
                            )
                        })?;
                    Ok(Response::GlobalWorkspace {
                        workspace: self
                            .global_workspaces()?
                            .adopt(&node_id, &owner, expected_revision)
                            .map_err(global_workspace_error)?,
                    })
                })
            }
            Request::LinkNodeWorkspace {
                global_workspace_id,
                expected_global_revision,
                identity,
                expected_owner_revision,
            } => {
                self.require_cached_workspace_owner_eligible(&identity.node_id)?;
                let node_id = identity.node_id.clone();
                self.with_live_workspace_node(&identity.node_id, |node| {
                    let owner = node
                        .local_snapshot
                        .and_then(|snapshot| {
                            snapshot
                                .workspaces
                                .into_iter()
                                .find(|workspace| workspace.id == identity.inner_id)
                        })
                        .ok_or_else(|| {
                            DaemonError::lifecycle(
                                ErrorCode::NotFound,
                                "owner Workspace not found in live capability snapshot",
                            )
                        })?;
                    Ok(Response::GlobalWorkspace {
                        workspace: self
                            .global_workspaces()?
                            .link(
                                &global_workspace_id,
                                expected_global_revision,
                                &node_id,
                                &owner,
                                expected_owner_revision,
                            )
                            .map_err(global_workspace_error)?,
                    })
                })
            }
            Request::RenameGlobalWorkspace {
                workspace_id,
                expected_revision,
                name,
            } => Ok(Response::GlobalWorkspace {
                workspace: self
                    .global_workspaces()?
                    .rename(&workspace_id, expected_revision, name)
                    .map_err(global_workspace_error)?,
            }),
            Request::OpenGlobalWorkspace {
                workspace_id,
                expected_revision,
            } => Ok(Response::GlobalWorkspaceOperation {
                result: self.open_global_workspace(&workspace_id, expected_revision)?,
            }),
            Request::CloseGlobalWorkspace {
                workspace_id,
                expected_revision,
            } => Ok(Response::GlobalWorkspaceOperation {
                result: self.close_global_workspace(&workspace_id, Some(expected_revision))?,
            }),
            Request::RetryGlobalWorkspaceClose { workspace_id } => {
                Ok(Response::GlobalWorkspaceOperation {
                    result: self.close_global_workspace(&workspace_id, None)?,
                })
            }
            Request::CreateGlobalWorkspaceShell {
                operation_id,
                global_workspace_id,
                expected_global_revision,
                node_id,
                owner_workspace_id,
                default_cwd,
                shell_id,
                shell,
            } => {
                let request = workspace_request_fingerprint
                    .as_ref()
                    .expect("Workspace resource request has a fingerprint");
                let (workspace, resource) = if self.node_identity()?.id()? == node_id {
                    self.create_local_global_workspace_shell(
                        &operation_id,
                        &request.digest,
                        request.bytes,
                        &global_workspace_id,
                        expected_global_revision,
                        &node_id,
                        &owner_workspace_id,
                        default_cwd,
                        &shell_id,
                        shell,
                    )?
                } else {
                    self.create_global_workspace_resource(
                        &operation_id,
                        &request.digest,
                        request.bytes,
                        &global_workspace_id,
                        expected_global_revision,
                        &node_id,
                        &owner_workspace_id,
                        default_cwd,
                        &shell_id,
                        PendingResourceKind::Shell,
                        move |pending| RoutedOperation::CreateWorkspaceShell {
                            workspace_id: pending.owner_workspace_id.clone(),
                            workspace_name: pending.owner_workspace_name.clone(),
                            default_cwd: pending.default_cwd.clone(),
                            shell_id: pending.resource_id.clone(),
                            shell,
                        },
                    )?
                };
                Ok(Response::GlobalWorkspaceResource {
                    workspace,
                    resource,
                })
            }
            Request::CreateGlobalWorkspaceWithShell {
                operation_id,
                global_workspace_id,
                name,
                node_id,
                owner_workspace_id,
                default_cwd,
                shell_id,
                shell,
            } => {
                let request = workspace_request_fingerprint
                    .as_ref()
                    .expect("Workspace resource request has a fingerprint");
                let request_fingerprint = request.digest.as_str();
                let semantic_fingerprint = workspace_semantic_fingerprint
                    .as_deref()
                    .expect("project Workspace request has a semantic fingerprint");
                let (workspace, resource) =
                    self.with_workspace_operation_lock(&operation_id, || {
                        if let Some(completed) = self
                            .global_workspaces()?
                            .completed_operation(&operation_id, request_fingerprint)
                            .map_err(global_workspace_error)?
                        {
                            return Ok((completed.workspace, completed.resource));
                        }
                        if let Err(error) = self.preflight_workspace_owner(&node_id) {
                            if !workspace_pre_owner_failure_is_ambiguous(&error) {
                                self.global_workspaces()?
                                    .cancel_pending_operation_if_never_attempted(
                                        &operation_id,
                                        request_fingerprint,
                                    )
                                    .map_err(global_workspace_error)?;
                            }
                            return Err(error);
                        }
                        let prepared = self
                            .global_workspaces()?
                            .prepare_workspace_shell(
                                &operation_id,
                                request_fingerprint,
                                request.bytes,
                                semantic_fingerprint,
                                &global_workspace_id,
                                &name,
                                &node_id,
                                &owner_workspace_id,
                                default_cwd,
                                &shell_id,
                            )
                            .map_err(global_workspace_error)?;
                        let (prepared_workspace, pending) = match prepared {
                            PreparedWorkspaceShell::Completed(completed) => {
                                let completed = *completed;
                                return Ok((completed.workspace, completed.resource));
                            }
                            PreparedWorkspaceShell::Pending { workspace, pending } => {
                                (workspace, *pending)
                            }
                        };
                        let result = self.create_global_workspace_resource_inner(
                            &pending.operation_id,
                            request_fingerprint,
                            request.bytes,
                            true,
                            &prepared_workspace.id,
                            // Equivalent compound requests retain the revision that
                            // originally authorized their shared preparation.
                            pending.expected_global_revision,
                            &pending.node_id,
                            &pending.requested_owner_workspace_id,
                            pending.default_cwd.clone(),
                            &pending.resource_id,
                            PendingResourceKind::Shell,
                            move |pending| RoutedOperation::CreateWorkspaceShell {
                                workspace_id: pending.owner_workspace_id.clone(),
                                workspace_name: pending.owner_workspace_name.clone(),
                                default_cwd: pending.default_cwd.clone(),
                                shell_id: pending.resource_id.clone(),
                                shell,
                            },
                        );
                        if let Err(error) = &result
                            && !workspace_pre_owner_failure_is_ambiguous(error)
                        {
                            self.global_workspaces()?
                                .cancel_pending_operation_if_never_attempted(
                                    &operation_id,
                                    request_fingerprint,
                                )
                                .map_err(global_workspace_error)?;
                        }
                        result
                    })?;
                Ok(Response::GlobalWorkspaceResource {
                    workspace,
                    resource,
                })
            }
            Request::CreateGlobalWorkspaceLauncher {
                operation_id,
                global_workspace_id,
                expected_global_revision,
                node_id,
                owner_workspace_id,
                default_cwd,
                launcher_id,
                spec,
            } => {
                let request = workspace_request_fingerprint
                    .as_ref()
                    .expect("Workspace resource request has a fingerprint");
                let (workspace, resource) = self.create_global_workspace_resource(
                    &operation_id,
                    &request.digest,
                    request.bytes,
                    &global_workspace_id,
                    expected_global_revision,
                    &node_id,
                    &owner_workspace_id,
                    default_cwd,
                    &launcher_id,
                    PendingResourceKind::Launcher,
                    move |pending| RoutedOperation::CreateWorkspaceLauncher {
                        workspace_id: pending.owner_workspace_id.clone(),
                        workspace_name: pending.owner_workspace_name.clone(),
                        default_cwd: pending.default_cwd.clone(),
                        launcher_id: pending.resource_id.clone(),
                        spec,
                    },
                )?;
                Ok(Response::GlobalWorkspaceResource {
                    workspace,
                    resource,
                })
            }
            Request::SetGlobalWorkspaceDefaultCwd {
                operation_id,
                global_workspace_id,
                expected_global_revision,
                node_id,
                owner_workspace_id,
                expected_owner_revision,
                default_cwd,
            } => Ok(Response::WorkspaceDefaultCwd {
                result: self.set_global_workspace_default_cwd(
                    &operation_id,
                    &global_workspace_id,
                    expected_global_revision,
                    &node_id,
                    &owner_workspace_id,
                    expected_owner_revision,
                    default_cwd,
                )?,
            }),
            Request::RouteNodeOperation { node_id, operation } => {
                Ok(self.route_node_operation_for_version(&node_id, operation, requester_version))
            }
            Request::HostService { operation } => Ok(Response::HostService {
                result: self.host_service_for_version(operation, requester_version)?,
            }),
            Request::RouteNodeHostService { node_id, operation } => Ok(
                self.route_node_host_service_for_version(&node_id, operation, requester_version)
            ),
            Request::Restart | Request::RestartWithNotificationConfig { .. } => {
                unreachable!("restart is handled before dispatch")
            }
            Request::Shutdown | Request::ShutdownIfNodeIdentity { .. } => {
                unreachable!("shutdown is handled before dispatch")
            }
            Request::Snapshot => Ok(Response::Snapshot {
                snapshot: self.snapshot()?,
            }),
            Request::GetFocusedTerminal => Ok(Response::FocusedTerminal {
                focused_terminal: self.focused_terminal()?,
            }),
            Request::GetWorkspace { workspace_id } => {
                let mut workspace = self.workspace(&workspace_id)?.snapshot(&self.durable)?;
                for shell in &mut workspace.shells {
                    self.add_recovery_presentation(shell)?;
                }
                Ok(Response::Workspace { workspace })
            }
            Request::GetShell { shell_id } => {
                let mut shell = self.shell(&shell_id)?.snapshot()?;
                self.add_recovery_presentation(&mut shell)?;
                Ok(Response::Shell { shell })
            }
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
            Request::SetAgentSessionDisplayName { .. } | Request::HideAgentSession { .. } => {
                Err(DaemonError::lifecycle(
                    ErrorCode::UnsupportedVersion,
                    "Agent Session mutation has been removed",
                ))
            }
            Request::CreateWorkspace {
                name,
                default_cwd,
                shells,
            } => self.durable_mutation(|undo| {
                let workspace = self.create_workspace_mutation(undo, name, default_cwd, shells)?;
                let events = workspace_created_events(&workspace);
                Ok((Response::Workspace { workspace }, events))
            }),
            Request::CreateWorkspaceShell {
                workspace_id,
                workspace_name,
                default_cwd,
                shell_id,
                shell,
            } => self.durable_mutation_outcome(|undo| {
                let (workspace, workspace_undo) = self.durable.create_workspace_exact(
                    &workspace_id,
                    workspace_name,
                    default_cwd,
                )?;
                let workspace_created = workspace_undo.is_some();
                if let Some(record) = workspace_undo {
                    undo.record(record);
                }
                let (shell, shell_undo) =
                    self.durable
                        .create_shell_exact(&workspace_id, &shell_id, shell)?;
                let shell_created = shell_undo.is_some();
                if let Some(record) = shell_undo {
                    undo.record(record);
                }
                if !workspace_created && !shell_created {
                    return Ok(DurableMutation::Unchanged(Response::Shell { shell }));
                }
                let mut events = Vec::new();
                if workspace_created {
                    events.push(DaemonEventKind::WorkspaceCreated {
                        workspace_id: workspace.id,
                        name: workspace.name,
                    });
                }
                if shell_created {
                    events.push(DaemonEventKind::ShellCreated {
                        workspace_id,
                        shell_id: shell.id.clone(),
                        name: shell.name.clone(),
                    });
                }
                Ok(DurableMutation::Changed(Response::Shell { shell }, events))
            }),
            Request::CreateWorkspaceLauncher {
                workspace_id,
                workspace_name,
                default_cwd,
                launcher_id,
                spec,
            } => self.durable_mutation_outcome(|undo| {
                let (workspace, workspace_undo) = self.durable.create_workspace_exact(
                    &workspace_id,
                    workspace_name,
                    default_cwd,
                )?;
                let workspace_created = workspace_undo.is_some();
                if let Some(record) = workspace_undo {
                    undo.record(record);
                }
                let (launcher, launcher_undo) =
                    self.durable
                        .create_launcher_exact(&workspace_id, &launcher_id, spec)?;
                let launcher_created = launcher_undo.is_some();
                if let Some(record) = launcher_undo {
                    undo.record(record);
                }
                if !workspace_created && !launcher_created {
                    return Ok(DurableMutation::Unchanged(Response::Launcher { launcher }));
                }
                let mut events = Vec::new();
                if workspace_created {
                    events.push(DaemonEventKind::WorkspaceCreated {
                        workspace_id: workspace.id,
                        name: workspace.name,
                    });
                }
                if launcher_created {
                    events.push(DaemonEventKind::LauncherCreated {
                        workspace_id,
                        launcher_id: launcher.id.clone(),
                        name: launcher.name.clone(),
                    });
                }
                Ok(DurableMutation::Changed(
                    Response::Launcher { launcher },
                    events,
                ))
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
                if events.is_empty() {
                    Ok(DurableMutation::Unchanged(Response::Agent { agent }))
                } else {
                    Ok(DurableMutation::Changed(Response::Agent { agent }, events))
                }
            }),
            Request::AcquireKiroLaunchHolder {
                pid,
                shell_id,
                run_id,
            } => self.acquire_kiro_launch_holder(pid, shell_id, run_id),
            Request::ReportKiroHook {
                holder_id,
                session_id,
                report,
            } => self.report_kiro_hook(&holder_id, session_id, report),
            Request::ReleaseKiroLaunchHolder { holder_id } => {
                self.release_kiro_launch_holder(&holder_id)
            }
            Request::EnsureOpenCodeSharedRuntime { port, environment } => {
                self.ensure_running()?;
                self.opencode
                    .ensure_runtime(port, environment.as_ref())
                    .map(|runtime| Response::OpenCodeSharedRuntime {
                        runtime: Some(runtime),
                    })
            }
            Request::GetOpenCodeSharedRuntime => self
                .opencode
                .get_runtime()
                .map(|runtime| Response::OpenCodeSharedRuntime { runtime }),
            Request::EnsureOpenCodeSessionClaim {
                generation_id,
                holder_id,
                root_session_id,
                shell_id,
                run_id,
                spec,
            } => self.ensure_opencode_session_claim(
                generation_id,
                holder_id,
                root_session_id,
                shell_id,
                run_id,
                spec,
            ),
            Request::ReleaseOpenCodeSessionClaim {
                generation_id,
                holder_id,
                claim_id,
            } => self.release_opencode_session_claim(&generation_id, &holder_id, &claim_id),
            Request::ResolveOpenCodeSessionClaim {
                generation_id,
                root_session_id,
            } => self.resolve_opencode_session_claim(&generation_id, &root_session_id),
            Request::ReportClaimedOpenCodeAgent {
                generation_id,
                root_session_id,
                report,
            } => self.report_claimed_opencode_agent(&generation_id, &root_session_id, report),
            Request::SetClaudeRemoteControlBinding {
                agent_id,
                shell_id,
                run_id,
                bridge_session_id,
            } => self
                .set_claude_remote_control_binding(&agent_id, &shell_id, &run_id, bridge_session_id)
                .map(|binding| Response::ClaudeRemoteControlBinding { binding }),
            Request::GetClaudeRemoteControlBinding {
                agent_id,
                shell_id,
                run_id,
            } => self
                .get_claude_remote_control_binding(&agent_id, &shell_id, &run_id)
                .map(|binding| Response::ClaudeRemoteControlBinding { binding }),
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
            Request::ObserveAgentWorkingContext {
                agent_id,
                shell_id,
                run_id,
                path,
            } => {
                let context = host_services::inspect_working_context(&path)?.ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::NotFound,
                        "Agent working-context path is not inside a Git worktree",
                    )
                })?;
                self.durable_mutation_outcome(|undo| {
                    let (agent, changed) = self.observe_agent_working_context_mutation(
                        undo, &agent_id, &shell_id, &run_id, context,
                    )?;
                    let response = Response::AgentWorkingContext {
                        agent: agent.clone(),
                        changed,
                    };
                    if !changed {
                        return Ok(DurableMutation::Unchanged(response));
                    }
                    let event = DaemonEventKind::AgentWorkingContextObserved {
                        workspace_id: agent.workspace_id.clone(),
                        shell_id: agent.shell_id.clone(),
                        agent,
                    };
                    Ok(DurableMutation::Changed(response, vec![event]))
                })
            }
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
            Request::GuardedSetWorkspaceDefaultCwd {
                workspace_id,
                expected_revision,
                default_cwd,
            } => {
                let default_cwd =
                    host_services::resolve_directory(&default_cwd).map_err(DaemonError::from)?;
                self.durable_mutation_outcome(|undo| {
                    let workspace = self.workspace(&workspace_id)?;
                    require_guard(*lock(&workspace.revision)?, expected_revision, "workspace")?;
                    let Some(record) = self
                        .durable
                        .set_workspace_default_cwd(&workspace_id, default_cwd.clone())?
                    else {
                        return Ok(DurableMutation::Unchanged(Response::Workspace {
                            workspace: workspace.snapshot(&self.durable)?,
                        }));
                    };
                    undo.record(record);
                    let workspace = workspace.snapshot(&self.durable)?;
                    Ok(DurableMutation::Changed(
                        Response::Workspace {
                            workspace: workspace.clone(),
                        },
                        vec![DaemonEventKind::WorkspaceDefaultCwdChanged {
                            workspace_id,
                            default_cwd: workspace
                                .default_cwd
                                .clone()
                                .expect("updated Workspace has a default cwd"),
                        }],
                    ))
                })
            }
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
            Request::Attach { .. }
            | Request::AttachCollaborative { .. }
            | Request::AttachNode { .. }
            | Request::ResumeAgentSession { .. }
            | Request::ResumeNodeAgentSession { .. } => {
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
        let mut recovered_interrupted_run = false;
        for saved_workspace in persisted.workspaces {
            validate_id("workspace", &saved_workspace.id)?;
            validate_persisted_name(&saved_workspace.name)?;
            if let Some(default_cwd) = &saved_workspace.default_cwd {
                validate_persisted_cwd(default_cwd)?;
            }
            validate_persisted_session_display_names(&saved_workspace)?;
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
            let workspace = Arc::new(Workspace {
                id: saved_workspace.id.clone(),
                revision: Mutex::new(saved_workspace.revision),
                name: Mutex::new(saved_workspace.name),
                default_cwd: Mutex::new(saved_workspace.default_cwd),
                shell_ids: Mutex::new(shell_ids),
                launcher_ids: Mutex::new(launcher_ids),
                agent_ids: Mutex::new(workspace_agent_ids),
                session_display_names: Mutex::new(saved_workspace.session_display_names),
                session_display_name_operations: Mutex::new(
                    saved_workspace.session_display_name_operations,
                ),
                hidden_sessions: Mutex::new(saved_workspace.hidden_sessions),
                session_hide_operations: Mutex::new(saved_workspace.session_hide_operations),
            });
            state.workspaces.insert(saved_workspace.id, workspace);
        }
        let store = Arc::new(store);
        let persistence_writer = PersistenceWriter::start(Arc::clone(&store));
        let registry = Self {
            node_identity: None,
            node_registrations: None,
            node_projection_cache: None,
            global_workspaces: None,
            local_shell_journal: None,
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
            opencode: OpenCodeCoordinator::default(),
            kiro: KiroLaunchHolders::default(),
            claude_remote_control: ClaudeRemoteControlBindings::default(),
            remote_attachments: RemoteAttachmentManager::default(),
            host_service_previews: Mutex::new(HashMap::new()),
            host_session_catalog: HostSessionCatalogCache::default(),
            workspace_operation_locks: Mutex::new(HashMap::new()),
            mutation_lock: Mutex::new(()),
            notification_settings: NotificationDeliverySettings::default(),
            notification_sink: Arc::new(DisabledNotificationSink),
            startup_environment: capture_current_environment(),
            node_projection_workers: NodeProjectionWorkers::default(),
            #[cfg(test)]
            fail_after_mutation: AtomicBool::new(false),
        };
        if recovered_interrupted_run {
            registry.persist()?;
        }
        Ok(registry)
    }
}

fn capabilities_support_feature(
    capabilities: &[String],
    feature: protocol::ProtocolFeature,
) -> bool {
    feature
        .capability_names()
        .iter()
        .all(|required| capabilities.iter().any(|capability| capability == required))
}

fn require_capabilities_support_feature(
    capabilities: &[String],
    feature: protocol::ProtocolFeature,
) -> DaemonResult<()> {
    if capabilities_support_feature(capabilities, feature) {
        Ok(())
    } else {
        Err(DaemonError::lifecycle(
            ErrorCode::UnsupportedVersion,
            format!("owner Node does not support {}", feature.requirement()),
        ))
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
                collaborators: Mutex::new(HashMap::new()),
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
        self.checkpoint_local_shell_transactions_with_mutation()
            .map_err(DaemonError::persistence)?;
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
                let notifications = self.notification_requests(&kinds, &undo);
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
                    Ok(()) => {
                        let mut transaction = self.events.transaction()?;
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
                node: None,
                digest: None,
            });
        }
        requests
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
        let pending = transaction.take_pending_durable(pending_count);
        transaction.begin_persistence(count);
        drop(transaction);
        match self
            .write_persisted_state(saved)
            .map_err(DaemonError::persistence)
        {
            Ok(()) => {}
            Err(error) => {
                let mut transaction = self.events.transaction()?;
                transaction.restore_pending_durable(pending);
                transaction.finish_persistence();
                return Err(error);
            }
        }
        let mut transaction = self.events.transaction()?;
        transaction.reserve_with_pending(0)?;
        for batch in pending {
            transaction.append_batch(batch);
        }
        transaction.finish_persistence();
        drop(transaction);
        self.events.notify();
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
        }
        self.queue_durable_batch_locked(batch, transition);
        failure.map_or(Ok(()), Err)
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
        self.write_persisted_state(saved)
    }

    fn capture_persisted_state(&self) -> io::Result<PersistenceGeneration> {
        self.durable.capture_persisted_state()
    }

    fn write_persisted_state(&self, generation: PersistenceGeneration) -> io::Result<()> {
        self.durable.write_persisted_state(generation)
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
        let reserve = 1;
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
        let pending_count =
            transaction.pending_durable_event_count(transaction.pending_durable_batch_count());
        let event_count = pending_count.saturating_add(batch.len());
        let saved = self.capture_persisted_state()?;
        transaction.begin_persistence(event_count);
        drop(transaction);
        match self.write_persisted_state(saved) {
            Ok(()) => {
                let mut transaction = self.events.transaction()?;
                transaction.append_pending_durable();
                transaction.append_batch(batch);
                transaction.finish_persistence();
                drop(transaction);
                self.events.notify();
                drop(_persistence);
                drop(_mutation);
                Ok(RunExitRecord::Recorded)
            }
            Err(error) => {
                let mut transaction = self.events.transaction()?;
                transaction.queue_durable_batch(batch);
                transaction.finish_persistence();
                drop(transaction);
                self.events.notify();
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
        for workspace in &mut snapshot.workspaces {
            for shell in &mut workspace.shells {
                self.add_recovery_presentation(shell)?;
            }
        }
        Ok(snapshot)
    }

    fn add_recovery_presentation(&self, shell: &mut ShellSnapshot) -> io::Result<()> {
        if matches!(shell.status, ShellStatus::Pending)
            && let Some((agent_id, run)) = self.recovery_presentation(&shell.id)?
        {
            shell.run = Some(run);
            shell.recovered_agent_id = Some(agent_id);
        }
        Ok(())
    }

    fn add_recovery_projection(&self, shell: &mut NodeProjectionShell) -> io::Result<()> {
        if matches!(shell.status, ShellStatus::Pending)
            && let Some((agent_id, run)) = self.recovery_presentation(&shell.id)?
        {
            shell.run_id = Some(run.id);
            shell.generation = Some(run.generation);
            shell.started_at_ms = Some(run.started_at_ms);
            shell.ended_at_ms = run.ended_at_ms;
            shell.recovered_agent_id = Some(agent_id);
        }
        Ok(())
    }

    fn recovery_presentation(
        &self,
        shell_id: &str,
    ) -> io::Result<Option<(String, ShellRunSnapshot)>> {
        let shell = self.durable.shell(shell_id)?;
        if !matches!(*lock(&shell.lifecycle)?, ShellLifecycle::Pending) {
            return Ok(None);
        }
        let previous_run = lock(&shell.last_run)?.clone();
        let Some(previous_run) = previous_run.as_ref() else {
            return Ok(None);
        };
        let Some(resumable) = self.resumable_agent(&shell, Some(previous_run))? else {
            return Ok(None);
        };
        if !matches!(*lock(&shell.lifecycle)?, ShellLifecycle::Pending) {
            return Ok(None);
        }
        Ok(Some((
            resumable.agent_id,
            ShellRunSnapshot {
                id: previous_run.id.clone(),
                generation: previous_run.generation,
                started_at_ms: previous_run.started_at_ms,
                ended_at_ms: previous_run.ended_at_ms,
                exit_reason: previous_run.exit_reason.clone(),
                output_revision: previous_run.output_revision,
                environment_has_run_id: previous_run.environment_has_run_id,
            },
        )))
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
        let mut projection = self.durable.node_projection(node_id)?;
        for shell in &mut projection.shells {
            self.add_recovery_projection(shell)?;
        }
        Ok(NodeProjectionSync {
            mode,
            cursor,
            projection,
            transitions,
            capabilities: self.runtime_protocol_capabilities(),
        })
    }

    fn runtime_protocol_capabilities(&self) -> Vec<String> {
        protocol::ProtocolFeature::ALL
            .iter()
            .copied()
            .filter(|feature| {
                !matches!(
                    feature,
                    protocol::ProtocolFeature::GlobalWorkspaces
                        | protocol::ProtocolFeature::WorkspacePlacementDefaultCwd
                ) || self.global_workspaces.is_some()
            })
            .flat_map(protocol::ProtocolFeature::capability_names)
            .copied()
            .map(str::to_owned)
            .collect()
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
        let authorized = ShellRuntimeManager::participant_is_authorized(runtime, token)?;
        if !authorized {
            return Ok(false);
        }
        self.runtimes
            .record_focus_gained(shell.workspace_id.clone(), shell.id.clone(), run_id)?;
        if let Some(identity) = &self.node_identity
            && let Ok(node_id) = identity.id()
        {
            self.runtimes
                .record_presented_focus(node_id, shell.id.clone())?;
            let _ = self
                .events
                .publish_runtime_batch(vec![DaemonEventKind::FocusedTerminalPresentationChanged]);
        }
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
            .import_focused_terminal(focused_terminal.clone(), current)?;
        if current
            && let Some(identity) = &self.node_identity
            && let Ok(node_id) = identity.id()
        {
            self.runtimes.import_presented_focus(
                node_id,
                focused_terminal.shell_id,
                focused_terminal.revision,
            )?;
        }
        Ok(())
    }

    fn import_presented_focused_terminal(
        &self,
        focused_terminal: Option<QualifiedFocusedTerminalSnapshot>,
    ) -> io::Result<()> {
        let Some(focused_terminal) = focused_terminal else {
            return Ok(());
        };
        self.runtimes.import_presented_focus(
            focused_terminal.shell.node_id,
            focused_terminal.shell.inner_id,
            focused_terminal.revision,
        )
    }

    fn lifecycle_transaction(
        &self,
        stopping: bool,
        select_shells: impl FnOnce() -> DaemonResult<Vec<Arc<Shell>>>,
        durable_apply: impl FnOnce(&mut DurableUndoLog) -> DaemonResult<()>,
        committed_events: impl FnOnce(&[Arc<Shell>]) -> Vec<DaemonEventKind>,
    ) -> DaemonResult<()> {
        let _mutation = lock(&self.mutation_lock)?;
        self.checkpoint_local_shell_transactions_with_mutation()
            .map_err(DaemonError::persistence)?;
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
        match self
            .write_persisted_state(saved)
            .map_err(DaemonError::persistence)
        {
            Ok(()) => {}
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
        }
        let mut transaction = self.events.transaction()?;
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
            |_| Ok(()),
            |_| Vec::new(),
        )?;
        self.opencode.shutdown()?;
        Ok(())
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

    fn observe_agent_working_context_mutation(
        &self,
        undo: &mut DurableUndoLog,
        agent_id: &str,
        shell_id: &str,
        run_id: &str,
        context: AgentWorkingContextSnapshot,
    ) -> DaemonResult<(AgentInstanceSnapshot, bool)> {
        let (snapshot, changed, record) = self
            .durable
            .observe_agent_working_context(agent_id, shell_id, run_id, context)?;
        if let Some(record) = record {
            undo.record(record);
        }
        Ok((snapshot, changed))
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
        drop(state);
        Ok(DurableUndo::RemovedWorkspace {
            workspace,
            shells,
            launchers,
            agents,
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
    fn revision_and_default_cwd(&self) -> io::Result<(u64, Option<PathBuf>)> {
        let revision = lock(&self.revision)?;
        let default_cwd = lock(&self.default_cwd)?;
        Ok((*revision, default_cwd.clone()))
    }

    fn snapshot(&self, registry: &DurableRegistry) -> io::Result<WorkspaceSnapshot> {
        let (revision, default_cwd) = self.revision_and_default_cwd()?;
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
            revision,
            name: lock(&self.name)?.clone(),
            default_cwd,
            shells,
            launchers,
            agents,
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
                working_contexts: saved.working_contexts,
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
            working_contexts: state.working_contexts.clone(),
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
            working_contexts: snapshot.working_contexts,
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
            status,
            run_id,
            generation,
            started_at_ms,
            ended_at_ms,
            recovered_agent_id: None,
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
            status,
            run,
            recovered_agent_id: None,
            foreground_process,
        })
    }
}

fn create_pending_shell(workspace_id: &str, spec: ShellSpec) -> io::Result<Arc<Shell>> {
    create_pending_shell_with_id(workspace_id, Uuid::new_v4().to_string(), spec)
}

fn create_pending_shell_with_id(
    workspace_id: &str,
    shell_id: String,
    spec: ShellSpec,
) -> io::Result<Arc<Shell>> {
    validate_name(&spec.name)?;
    validate_cwd(&spec.cwd)?;
    Ok(Arc::new(Shell {
        id: shell_id,
        revision: Mutex::new(1),
        workspace_id: workspace_id.to_owned(),
        name: Mutex::new(spec.name),
        cwd: spec.cwd,
        command: spec.command,
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
    opencode_session_id: Option<&'a str>,
    history: Option<&'a str>,
}

struct ResumableAgent {
    agent_id: String,
    integration: String,
    external_session_id: String,
    command: Vec<String>,
}

struct RuntimeStart<'a> {
    workspace_name: &'a str,
    shell_name: &'a str,
    profile: &'a TerminalProfile,
    environment: Option<&'a UnixEnvironment>,
    recovery: RuntimeRecovery<'a>,
    claude_remote_control: bool,
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
            claude_remote_control,
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

        let selected_command = recovery.effective_command.unwrap_or(&shell.command);
        let claude_command = claude_remote_control_command(
            shell,
            selected_command,
            recovery.effective_command.is_some(),
            claude_remote_control,
        );
        let selected_command = claude_command.as_deref().unwrap_or(selected_command);
        let codex_launch = codex_launch_eligible(shell, selected_command);
        let kiro_launch = kiro_launch_eligible(shell, selected_command);
        let original_environment = environment
            .cloned()
            .unwrap_or_else(capture_current_environment);
        let mut child_environment = sanitize_opencode_shim_environment(&original_environment);
        let mut shared_recovery_command = None;
        let mut supervised_shared_command = None;
        let mut codex_launch_command = None;
        let mut kiro_launch_command = None;
        if let Some(session_id) = recovery.opencode_session_id
            && let Ok(injected) =
                inject_opencode_shim_environment(&child_environment, claude_remote_control)
        {
            child_environment = injected;
            let selected_executable = Path::new(&selected_command[0]);
            let exact_executable = if selected_executable.is_absolute() {
                Some(selected_executable.to_path_buf())
            } else if selected_executable.components().count() > 1 {
                Some(shell.cwd.join(selected_executable))
            } else {
                None
            };
            if let Some(executable) = exact_executable {
                set_environment_value(
                    &mut child_environment,
                    b"BOOMUX_REAL_OPENCODE",
                    executable.into_os_string(),
                );
            }
            if let Some(executable) =
                environment_value(&child_environment, b"BOOMUX_SHIM_EXECUTABLE")
            {
                shared_recovery_command = Some(vec![
                    executable.to_string_lossy().into_owned(),
                    "opencode".into(),
                    "shared".into(),
                    "--session".into(),
                    session_id.into(),
                ]);
            }
        } else if supervised_opencode_session(selected_command).is_some()
            && let Ok(injected) =
                inject_opencode_shim_environment(&child_environment, claude_remote_control)
        {
            child_environment = injected;
            if let Some(executable) =
                environment_value(&child_environment, b"BOOMUX_SHIM_EXECUTABLE")
            {
                supervised_shared_command = supervised_shared_opencode_command(
                    selected_command,
                    &executable.to_string_lossy(),
                );
            }
        } else if codex_launch
            && let Ok(injected) =
                inject_opencode_shim_environment(&child_environment, claude_remote_control)
        {
            child_environment = injected;
            let selected_executable = Path::new(&selected_command[0]);
            let exact_executable = if selected_executable.is_absolute() {
                Some(selected_executable.to_path_buf())
            } else if selected_executable.components().count() > 1 {
                Some(shell.cwd.join(selected_executable))
            } else {
                None
            };
            if let Some(executable) = exact_executable {
                set_environment_value(
                    &mut child_environment,
                    b"BOOMUX_REAL_CODEX",
                    executable.into_os_string(),
                );
            }
            if let Some(executable) =
                environment_value(&child_environment, b"BOOMUX_SHIM_EXECUTABLE")
            {
                let mut command = vec![
                    executable.to_string_lossy().into_owned(),
                    "codex".into(),
                    "launch".into(),
                    "--".into(),
                ];
                command.extend(selected_command[1..].iter().cloned());
                codex_launch_command = Some(command);
            }
        } else if kiro_launch
            && let Ok(injected) =
                inject_opencode_shim_environment(&child_environment, claude_remote_control)
        {
            child_environment = injected;
            let selected_executable = Path::new(&selected_command[0]);
            let exact_executable = if selected_executable.is_absolute() {
                Some(selected_executable.to_path_buf())
            } else if selected_executable.components().count() > 1 {
                Some(shell.cwd.join(selected_executable))
            } else {
                None
            };
            if let Some(executable) = exact_executable {
                set_environment_value(
                    &mut child_environment,
                    b"BOOMUX_REAL_KIRO",
                    executable.into_os_string(),
                );
            }
            if let Some(executable) =
                environment_value(&child_environment, b"BOOMUX_SHIM_EXECUTABLE")
            {
                let mut command = vec![
                    executable.to_string_lossy().into_owned(),
                    "kiro".into(),
                    "launch".into(),
                    "--".into(),
                ];
                command.extend(selected_command[1..].iter().cloned());
                kiro_launch_command = Some(command);
            }
        } else if opencode_shim_eligible(shell, selected_command)
            && let Ok(injected) =
                inject_opencode_shim_environment(&child_environment, claude_remote_control)
        {
            child_environment = injected;
        }
        let selected_command = shared_recovery_command
            .as_deref()
            .or(supervised_shared_command.as_deref())
            .or(codex_launch_command.as_deref())
            .or(kiro_launch_command.as_deref())
            .unwrap_or(selected_command);
        let client_shell = child_environment
            .variables
            .iter()
            .find(|variable| variable.name == b"SHELL")
            .map(|variable| std::ffi::OsString::from_vec(variable.value.clone()))
            .unwrap_or_else(|| "/bin/sh".into());
        let mut command = if selected_command.is_empty() {
            let startup_arguments =
                configure_opencode_shell_startup(&client_shell, &mut child_environment);
            let mut command = CommandBuilder::new(client_shell);
            command.args(startup_arguments);
            command
        } else {
            let mut command = CommandBuilder::new(&selected_command[0]);
            command.args(&selected_command[1..]);
            command
        };
        command.cwd(&shell.cwd);
        command.env_clear();
        for variable in &child_environment.variables {
            command.env(
                std::ffi::OsString::from_vec(variable.name.clone()),
                std::ffi::OsString::from_vec(variable.value.clone()),
            );
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
                collaborators: Mutex::new(HashMap::new()),
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
                            Self::fanout_output(&reader_runtime, bytes);
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
                let _ = reader_runtime
                    .collaborators
                    .lock()
                    .map(|mut collaborators| collaborators.clear());
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
    collaborative: bool,
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
            collaborative,
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
                        "shell is no longer running the expected run",
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
        let resumable_agent = needs_start
            .then(|| registry.resumable_agent(&shell, previous_run.as_ref()))
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
        let journaled_start = if needs_start {
            registry
                .local_shell_journal
                .as_ref()
                .map(|journal| journal.contains_create(shell_id))
                .transpose()?
                .unwrap_or(false)
        } else {
            false
        };
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
                            effective_command: resumable_agent
                                .as_ref()
                                .map(|resumable| resumable.command.as_slice()),
                            opencode_session_id: resumable_agent
                                .as_ref()
                                .filter(|resumable| resumable.integration == "opencode")
                                .map(|resumable| resumable.external_session_id.as_str()),
                            history: restored_history.as_deref(),
                        },
                        claude_remote_control: registry.notification_settings.claude_remote_control,
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
        if started {
            event_transaction
                .as_mut()
                .expect("start event transaction is locked")
                .begin_persistence(1);
            drop(event_transaction);
            let persistence_result = if journaled_start {
                let run = lock(&shell.last_run)?
                    .clone()
                    .ok_or_else(|| io::Error::other("started shell has no persisted run"))?;
                let journal_result = registry
                    .local_shell_journal
                    .as_ref()
                    .ok_or_else(|| io::Error::other("local Shell journal is unavailable"))?
                    .append(LocalShellJournalRecord::Start(Box::new(
                        LocalShellStartTransaction {
                            shell_id: shell.id.clone(),
                            run,
                        },
                    )));
                match journal_result {
                    Ok(()) => Ok(()),
                    Err(journal_error) => (|| {
                        let saved = registry.capture_persisted_state()?;
                        registry.write_persisted_state(saved)?;
                        registry
                            .global_workspaces
                            .as_ref()
                            .ok_or_else(|| io::Error::other("global Workspace store is unavailable"))?
                            .checkpoint()?;
                        registry
                            .local_shell_journal
                            .as_ref()
                            .ok_or_else(|| io::Error::other("local Shell journal is unavailable"))?
                            .reset_after_full_checkpoint()?;
                        Ok(())
                    })()
                        .map_err(|state_error: io::Error| {
                            io::Error::new(
                                state_error.kind(),
                                format!(
                                    "local Shell start journal failed: {journal_error}; state fallback also failed: {state_error}"
                                ),
                            )
                        }),
                }
            } else {
                let saved = registry.capture_persisted_state()?;
                registry.write_persisted_state(saved)
            };
            if let Err(error) = persistence_result {
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
                    profile: None,
                },
            )?;
            return AttachFrame::Detached.write_to(&mut stream);
        }
        let attached_run = attached_run.expect("running shell has a run");
        let runtime = runtime.expect("running shell has a runtime");
        let control = lock(&runtime.control)?;
        if collaborative {
            if lock(&runtime.collaborators)?.len() >= MAX_COLLABORATORS_PER_SHELL {
                return send_response(
                    &mut stream,
                    response_version,
                    DaemonError::lifecycle(
                        ErrorCode::Busy,
                        "shell has reached its collaborative attachment limit",
                    )
                    .into_response(),
                );
            }
        } else {
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
        if !collaborative {
            lock(&runtime.master)?.resize(Self::profile_size(&profile))?;
            Self::update_runtime_dimensions(&shell, &runtime, Self::profile_size(&profile))?;
            lock(&terminal)?.resize(profile.rows, profile.cols);
        }
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
                profile: collaborative.then_some(startup_profile),
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
        if collaborative {
            lock(&runtime.collaborators)?.insert(
                token.clone(),
                Controller {
                    token: token.clone(),
                    output,
                    connection,
                    reconnect_ack: None,
                },
            );
        } else {
            let mut controller = lock(&runtime.controller)?;
            if let Some(previous) = controller.take() {
                drop(previous);
            }
            if takeover {
                Self::displace_collaborators(&runtime)?;
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
                        ControllerOutput::Resize {
                            rows,
                            cols,
                            pixel_width,
                            pixel_height,
                        } => {
                            if (AttachFrame::Resize {
                                rows,
                                cols,
                                pixel_width,
                                pixel_height,
                            })
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
                // A primary controller applies backpressure to the PTY reader so
                // bursty, indivisible protocols such as Kitty graphics are not
                // truncated when the small output queue fills. Drop the receiver
                // before taking the controller lock so a blocked sender can wake
                // up if the socket writer fails.
                drop(receiver);
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
                        let acknowledge = controller
                            .as_mut()
                            .filter(|controller| controller.token == token)
                            .and_then(|controller| controller.reconnect_ack.take());
                        drop(controller);
                        acknowledge.or_else(|| {
                            lock(&runtime.collaborators)
                                .ok()?
                                .get_mut(&token)
                                .and_then(|collaborator| collaborator.reconnect_ack.take())
                        })
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
                let primary = Self::participant_is_primary(&runtime, &token)?;
                let authorized = primary || lock(&runtime.collaborators)?.contains_key(&token);
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
                        if !primary {
                            Ok(())
                        } else if let Err(error) = validate_terminal_dimensions(rows, cols) {
                            Err(error)
                        } else {
                            let size = PtySize {
                                rows,
                                cols,
                                pixel_width,
                                pixel_height,
                            };
                            lock(&runtime.terminal)?.resize(rows, cols);
                            lock(&runtime.master)?.resize(size)?;
                            Self::update_runtime_dimensions(&shell, &runtime, size)?;
                            Self::fanout_collaborator_resize(&runtime, size);
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
        let control = lock(&runtime.control)?;
        if !Self::participant_is_authorized(runtime, token)? {
            return Ok(false);
        }
        let mut offset = 0;
        while offset < bytes.len() {
            let result = lock(&runtime.master)?.write(&bytes[offset..]);
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
        drop(control);
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
    if environment.variables.len() > MAX_UNIX_ENVIRONMENT_VARIABLES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "client environment contains too many variables",
        ));
    }
    let mut names = HashSet::new();
    let mut bytes = 0_usize;
    for variable in &environment.variables {
        bytes = bytes
            .checked_add(variable.name.len())
            .and_then(|bytes| bytes.checked_add(variable.value.len()))
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "client environment is too large",
                )
            })?;
        if bytes > MAX_UNIX_ENVIRONMENT_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "client environment is too large",
            ));
        }
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

fn normalize_session_display_name(name: &str) -> io::Result<String> {
    if name.chars().any(char::is_control) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Session display name cannot contain control characters",
        ));
    }
    let normalized = name.split_whitespace().collect::<Vec<_>>().join(" ");
    if normalized.is_empty() || normalized.chars().count() > MAX_SESSION_DISPLAY_NAME_CHARS {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "Session display name must contain 1 through {MAX_SESSION_DISPLAY_NAME_CHARS} normalized characters"
            ),
        ));
    }
    Ok(normalized)
}

fn validate_persisted_session_display_names(workspace: &PersistedWorkspace) -> io::Result<()> {
    if workspace.session_display_names.len() > MAX_SESSION_DISPLAY_NAMES_PER_WORKSPACE
        || workspace.session_display_name_operations.len()
            > MAX_SESSION_DISPLAY_NAME_OPERATIONS_PER_WORKSPACE
        || workspace.hidden_sessions.len() > MAX_HIDDEN_SESSIONS_PER_WORKSPACE
        || workspace.session_hide_operations.len() > MAX_SESSION_HIDE_OPERATIONS_PER_WORKSPACE
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Boomux state exceeds a Workspace Session display-name bound",
        ));
    }
    let mut identities = HashSet::new();
    for record in &workspace.session_display_names {
        if validate_required_agent_string("integration", &record.integration, MAX_NAME_BYTES)
            .is_err()
            || normalize_session_display_name(&record.display_name)
                .map(|name| name != record.display_name)
                .unwrap_or(true)
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Boomux state contains invalid Session display-name metadata",
            ));
        }
        let identity = match &record.session {
            PersistedSessionIdentity::External {
                external_session_id,
            } => {
                if validate_required_agent_string(
                    "external_session_id",
                    external_session_id,
                    MAX_NAME_BYTES,
                )
                .is_err()
                {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "Boomux state contains an invalid Session external identity",
                    ));
                }
                format!("external:{external_session_id}")
            }
            PersistedSessionIdentity::Instance { agent_id } => {
                if validate_uuid(agent_id, "Agent ID").is_err() {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "Boomux state contains an invalid Session Agent identity",
                    ));
                }
                format!("instance:{agent_id}")
            }
        };
        if !identities.insert((record.integration.as_str(), identity)) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Boomux state contains duplicate Session display-name metadata",
            ));
        }
    }
    let mut operation_ids = HashSet::new();
    let mut previous_resulting_revision = 0;
    for operation in &workspace.session_display_name_operations {
        if validate_uuid(&operation.operation_id, "Session display-name operation ID").is_err()
            || validate_uuid(&operation.session_id, "Agent Session ID").is_err()
            || operation.expected_revision == 0
            || operation.result.workspace_revision <= operation.expected_revision
            || operation.result.workspace_revision <= previous_resulting_revision
            || operation.result.workspace_revision > workspace.revision
            || operation.result.session_id != operation.session_id
            || operation.result.workspace_id != workspace.id
            || operation.result.user_display_name != operation.display_name
            || !operation.result.changed
            || validate_required_agent_string("integration", &operation.integration, MAX_NAME_BYTES)
                .is_err()
            || match &operation.session {
                PersistedSessionIdentity::External {
                    external_session_id,
                } => validate_required_agent_string(
                    "external Session ID",
                    external_session_id,
                    MAX_NAME_BYTES,
                )
                .is_err(),
                PersistedSessionIdentity::Instance { agent_id } => {
                    validate_uuid(agent_id, "Agent ID").is_err()
                }
            }
            || operation.display_name.as_deref().is_some_and(|name| {
                normalize_session_display_name(name)
                    .map(|normalized| normalized != name)
                    .unwrap_or(true)
            })
            || !operation_ids.insert(operation.operation_id.as_str())
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Boomux state contains an invalid Session display-name operation",
            ));
        }
        previous_resulting_revision = operation.result.workspace_revision;
    }

    let mut hidden_identities = HashSet::new();
    let mut hidden_session_ids = HashSet::new();
    for hidden in &workspace.hidden_sessions {
        let identity = persisted_session_identity_key(&hidden.session).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "Boomux state contains invalid hidden Session metadata",
            )
        })?;
        if validate_uuid(&hidden.session_id, "Agent Session ID").is_err()
            || validate_required_agent_string("integration", &hidden.integration, MAX_NAME_BYTES)
                .is_err()
            || !hidden_identities.insert((hidden.integration.as_str(), identity))
            || !hidden_session_ids.insert(hidden.session_id.as_str())
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Boomux state contains invalid hidden Session metadata",
            ));
        }
    }
    let mut operation_ids = HashSet::new();
    let mut previous_resulting_revision = 0;
    for operation in &workspace.session_hide_operations {
        let expected_result_revision = operation
            .expected_revision
            .checked_add(u64::from(operation.result.changed));
        if validate_uuid(&operation.operation_id, "Session hide operation ID").is_err()
            || validate_uuid(&operation.session_id, "Agent Session ID").is_err()
            || validate_uuid(&operation.workspace_id, "workspace ID").is_err()
            || operation.workspace_id != workspace.id
            || operation.expected_revision == 0
            || expected_result_revision != Some(operation.result.workspace_revision)
            || operation.result.workspace_revision < previous_resulting_revision
            || operation.result.workspace_revision > workspace.revision
            || operation.result.session_id != operation.session_id
            || operation.result.workspace_id != workspace.id
            || validate_required_agent_string("integration", &operation.integration, MAX_NAME_BYTES)
                .is_err()
            || persisted_session_identity_key(&operation.session).is_err()
            || !operation_ids.insert(operation.operation_id.as_str())
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Boomux state contains an invalid Session hide operation",
            ));
        }
        previous_resulting_revision = operation.result.workspace_revision;
    }
    Ok(())
}

fn persisted_session_identity_key(identity: &PersistedSessionIdentity) -> io::Result<String> {
    match identity {
        PersistedSessionIdentity::External {
            external_session_id,
        } => {
            validate_required_agent_string(
                "external Session ID",
                external_session_id,
                MAX_NAME_BYTES,
            )?;
            Ok(format!("external:{external_session_id}"))
        }
        PersistedSessionIdentity::Instance { agent_id } => {
            validate_uuid(agent_id, "Agent ID")?;
            Ok(format!("instance:{agent_id}"))
        }
    }
}

fn validate_uuid(value: &str, label: &str) -> io::Result<()> {
    let parsed = Uuid::parse_str(value)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, format!("invalid {label}")))?;
    if parsed.to_string() != value {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{label} must use canonical UUID syntax"),
        ));
    }
    Ok(())
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

fn opencode_listener_belongs_to_session(port: u16, session_id: u32) -> bool {
    let Ok(table) = fs::read_to_string("/proc/net/tcp") else {
        return false;
    };
    let port = format!("{port:04X}");
    let inodes = table
        .lines()
        .skip(1)
        .filter_map(|line| {
            let fields = line.split_whitespace().collect::<Vec<_>>();
            let local_port = fields.get(1)?.rsplit_once(':')?.1;
            (local_port == port && fields.get(3) == Some(&"0A"))
                .then(|| fields.get(9).copied())
                .flatten()
        })
        .collect::<HashSet<_>>();
    if inodes.is_empty() {
        return false;
    }
    session_processes(session_id as libc::pid_t)
        .into_iter()
        .any(|pid| {
            let Ok(descriptors) = fs::read_dir(format!("/proc/{pid}/fd")) else {
                return false;
            };
            descriptors.flatten().any(|descriptor| {
                fs::read_link(descriptor.path())
                    .ok()
                    .and_then(|target| target.into_os_string().into_string().ok())
                    .and_then(|target| {
                        target
                            .strip_prefix("socket:[")
                            .and_then(|target| target.strip_suffix(']'))
                            .map(|inode| inodes.contains(inode))
                    })
                    .unwrap_or(false)
            })
        })
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

fn validate_working_context(context: &AgentWorkingContextSnapshot) -> io::Result<()> {
    if !context.worktree_root.is_absolute()
        || context.worktree_root.as_os_str().as_bytes().len() > 4 * 1024
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Agent working-context root must be a bounded absolute path",
        ));
    }
    validate_required_agent_string("repository", &context.repository, MAX_NAME_BYTES)?;
    validate_required_agent_string("branch", &context.branch, MAX_NAME_BYTES)
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

fn validate_claude_bridge_session_id(bridge_session_id: &str) -> io::Result<()> {
    if bridge_session_id.is_empty()
        || bridge_session_id.len() > protocol::MAX_CLAUDE_BRIDGE_SESSION_ID_BYTES
        || bridge_session_id.chars().any(char::is_control)
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "Claude bridge session ID must be nonempty, control-free, and at most {} bytes",
                protocol::MAX_CLAUDE_BRIDGE_SESSION_ID_BYTES
            ),
        ));
    }
    Ok(())
}

fn validate_claude_binding_authority(
    state: &DurableState,
    agent_id: &str,
    shell_id: &str,
    run_id: &str,
) -> DaemonResult<()> {
    validate_claude_binding_identity(state, agent_id, shell_id, run_id)?;
    if !claude_binding_is_current(
        state,
        &ClaudeRemoteControlBindingSnapshot {
            agent_id: agent_id.into(),
            shell_id: shell_id.into(),
            run_id: run_id.into(),
            bridge_session_id: String::new(),
        },
    )? {
        return Err(DaemonError::lifecycle(
            ErrorCode::RunChanged,
            "Claude Remote Control binding does not match an active current ShellRun",
        ));
    }
    Ok(())
}

fn validate_claude_binding_identity(
    state: &DurableState,
    agent_id: &str,
    shell_id: &str,
    run_id: &str,
) -> DaemonResult<()> {
    validate_uuid(agent_id, "Claude Agent ID")?;
    validate_uuid(shell_id, "Claude Shell ID")?;
    validate_uuid(run_id, "Claude ShellRun ID")?;
    let agent = state
        .agents
        .get(agent_id)
        .ok_or_else(|| not_found("agent", agent_id))?;
    if agent.integration != "claude" {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Claude Remote Control binding requires a Claude Agent",
        )
        .into());
    }
    if agent.shell_id != shell_id || agent.run_id != run_id {
        return Err(DaemonError::lifecycle(
            ErrorCode::RunChanged,
            "Claude Remote Control binding does not match the Agent ShellRun",
        ));
    }
    Ok(())
}

fn claude_binding_is_current(
    state: &DurableState,
    binding: &ClaudeRemoteControlBindingSnapshot,
) -> io::Result<bool> {
    let Some(agent) = state.agents.get(&binding.agent_id) else {
        return Ok(false);
    };
    if agent.integration != "claude"
        || agent.shell_id != binding.shell_id
        || agent.run_id != binding.run_id
    {
        return Ok(false);
    }
    let agent_state = lock(&agent.state)?;
    if agent_state.ended_at_ms.is_some()
        || matches!(
            agent_state.observation.state,
            AgentState::Inactive | AgentState::Done
        )
    {
        return Ok(false);
    }
    drop(agent_state);
    let Some(shell) = state.shells.get(&binding.shell_id) else {
        return Ok(false);
    };
    Ok(matches!(
        &*lock(&shell.lifecycle)?,
        ShellLifecycle::Running { run, .. } if run.id == binding.run_id
    ))
}

fn prune_claude_bindings(
    state: &DurableState,
    bindings: &mut HashMap<String, ClaudeRemoteControlBindingSnapshot>,
) -> io::Result<()> {
    let stale = bindings
        .iter()
        .map(|(agent_id, binding)| {
            claude_binding_is_current(state, binding)
                .map(|current| (!current).then(|| agent_id.clone()))
        })
        .collect::<io::Result<Vec<_>>>()?;
    for agent_id in stale.into_iter().flatten() {
        bindings.remove(&agent_id);
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
    if agent.working_contexts.len() > MAX_AGENT_WORKING_CONTEXTS {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Boomux state contains too many Agent working contexts",
        ));
    }
    let mut roots = HashSet::new();
    let mut previous_observed_at_ms = u64::MAX;
    for context in &agent.working_contexts {
        validate_working_context(context)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error.to_string()))?;
        if context.observed_at_ms < agent.started_at_ms
            || context.observed_at_ms > previous_observed_at_ms
            || !roots.insert(&context.worktree_root)
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Boomux state contains invalid Agent working contexts",
            ));
        }
        previous_observed_at_ms = context.observed_at_ms;
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

    #[test]
    fn active_remote_upgrade_projects_as_reconnecting() {
        assert_eq!(
            classify_node_sync_error(&io::Error::from(io::ErrorKind::WouldBlock)),
            crate::protocol::NodeProjectionHealthCode::Reconnecting
        );
        assert_eq!(
            classify_node_sync_error(&io::Error::from(io::ErrorKind::Unsupported)),
            crate::protocol::NodeProjectionHealthCode::Unsupported
        );
        assert_eq!(
            classify_node_sync_error(&crate::ssh_bootstrap::stale_upgrade_recovery()),
            crate::protocol::NodeProjectionHealthCode::Stale
        );
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
                claude_remote_control: true,
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

    fn test_environment(values: &[(&str, &Path)]) -> UnixEnvironment {
        UnixEnvironment {
            variables: values
                .iter()
                .map(|(name, value)| UnixEnvironmentVariable {
                    name: name.as_bytes().to_vec(),
                    value: value.as_os_str().as_bytes().to_vec(),
                })
                .collect(),
        }
    }

    #[test]
    fn opencode_shim_eligibility_is_limited_to_login_shells() {
        let login =
            create_pending_shell("workspace", ShellSpec::login("login", env::temp_dir())).unwrap();
        assert!(opencode_shim_eligible(&login, &[]));
        assert!(!opencode_shim_eligible(
            &login,
            &["opencode".into(), "--continue".into()]
        ));

        let command = create_pending_shell(
            "workspace",
            ShellSpec {
                name: "command".into(),
                cwd: env::temp_dir(),
                command: vec!["opencode".into()],
            },
        )
        .unwrap();
        assert!(!opencode_shim_eligible(&command, &command.command));
    }

    #[test]
    fn supervised_opencode_resume_is_exactly_recognized_for_shared_launch() {
        let command = vec![
            "/opt/boomux".into(),
            "agent".into(),
            "supervise".into(),
            "OpenCode".into(),
            "--integration".into(),
            "opencode".into(),
            "--external-session-id".into(),
            "session-exact".into(),
            "--".into(),
            "/opt/opencode".into(),
            "--session".into(),
            "session-exact".into(),
        ];
        assert_eq!(supervised_opencode_session(&command), Some("session-exact"));
        assert_eq!(
            supervised_shared_opencode_command(&command, "/new/boomux").unwrap(),
            [
                "/opt/boomux",
                "agent",
                "supervise",
                "OpenCode",
                "--integration",
                "opencode",
                "--external-session-id",
                "session-exact",
                "--",
                "/new/boomux",
                "opencode",
                "shared",
                "--session",
                "session-exact",
            ]
        );

        let mut mismatched = command.clone();
        mismatched[11] = "different-session".into();
        assert_eq!(supervised_opencode_session(&mismatched), None);

        let mut argument_bearing = command;
        argument_bearing.push("--fork".into());
        assert_eq!(supervised_opencode_session(&argument_bearing), None);
    }

    #[test]
    fn claude_remote_control_rewrites_only_bare_commands() {
        let command = create_pending_shell(
            "workspace",
            ShellSpec {
                name: "claude".into(),
                cwd: env::temp_dir(),
                command: vec!["/opt/anthropic/bin/claude".into()],
            },
        )
        .unwrap();
        assert_eq!(
            claude_remote_control_command(&command, &command.command, false, true),
            Some(vec![
                "/opt/anthropic/bin/claude".into(),
                "--remote-control".into(),
            ])
        );
        for (argv, recovery, enabled) in [
            (
                vec!["claude".into(), "--resume".into(), "id".into()],
                false,
                true,
            ),
            (vec!["claude".into()], true, true),
            (vec!["claude".into()], false, false),
            (vec!["claude-wrapper".into()], false, true),
        ] {
            assert!(
                claude_remote_control_command(&command, &argv, recovery, enabled).is_none(),
                "rewrote {argv:?}"
            );
        }
    }

    #[test]
    fn codex_launcher_accepts_only_managed_chat_shapes() {
        let shell = create_pending_shell(
            "workspace",
            ShellSpec {
                name: "codex".into(),
                cwd: env::temp_dir(),
                command: vec!["/opt/openai/codex".into()],
            },
        )
        .unwrap();
        for argv in [
            vec!["/opt/openai/codex".into()],
            vec!["/opt/openai/codex".into(), "resume".into(), "exact".into()],
            vec!["/opt/openai/codex".into(), "exec".into(), "-".into()],
        ] {
            assert!(codex_launch_eligible(&shell, &argv));
        }
        for argv in [
            vec!["codex".into(), "remote-control".into()],
            vec!["codex".into(), "--remote".into(), "unix://".into()],
            vec!["codex-wrapper".into()],
        ] {
            assert!(!codex_launch_eligible(&shell, &argv));
        }
    }

    #[test]
    fn kiro_launcher_accepts_only_bare_or_explicit_v3_shapes() {
        let shell = create_pending_shell(
            "workspace",
            ShellSpec {
                name: "kiro".into(),
                cwd: env::temp_dir(),
                command: vec!["/opt/kiro/bin/kiro-cli".into()],
            },
        )
        .unwrap();
        for argv in [
            vec!["/opt/kiro/bin/kiro-cli".into()],
            vec![
                "/opt/kiro/bin/kiro-cli".into(),
                "--v3".into(),
                "chat".into(),
            ],
        ] {
            assert!(kiro_launch_eligible(&shell, &argv));
        }
        for argv in [
            vec!["kiro-cli".into(), "chat".into()],
            vec!["kiro-cli".into(), "--version".into()],
            vec!["kiro".into()],
        ] {
            assert!(!kiro_launch_eligible(&shell, &argv));
        }
    }

    #[test]
    fn inherited_opencode_shim_provenance_is_stripped_without_losing_identity() {
        let mut environment = test_environment(&[
            ("PATH", Path::new("/runtime/boomux/shims:/usr/bin")),
            ("BOOMUX_ORIGINAL_PATH", Path::new("/usr/local/bin:/usr/bin")),
            (
                "BOOMUX_OPENCODE_SHIM_DIR",
                Path::new("/runtime/boomux/shims"),
            ),
            ("BOOMUX_REAL_OPENCODE", Path::new("/usr/bin/opencode")),
            ("BOOMUX_REAL_CLAUDE", Path::new("/usr/bin/claude")),
            ("BOOMUX_REAL_CODEX", Path::new("/usr/bin/codex")),
            ("BOOMUX_REAL_KIRO", Path::new("/usr/bin/kiro-cli")),
            ("BOOMUX_CODEX_RUN_SCOPED", Path::new("1")),
            ("BOOMUX_KIRO_RUN_SCOPED", Path::new("1")),
            ("BOOMUX_CLAUDE_REMOTE_CONTROL", Path::new("1")),
            (
                "BOOMUX_OPENCODE_TUI_CONFIG",
                Path::new("/runtime/boomux/shims/tui.json"),
            ),
            (
                "OPENCODE_TUI_CONFIG",
                Path::new("/runtime/boomux/shims/tui.json"),
            ),
            ("BOOMUX_SHIM_EXECUTABLE", Path::new("/usr/bin/boomux")),
            ("BOOMUX_OPENCODE_SHARED_GENERATION", Path::new("generation")),
            ("BOOMUX_OPENCODE_CLAIM_HOLDER", Path::new("holder")),
            ("BOOMUX_USER_ZDOTDIR", Path::new("/home/user")),
            ("ZDOTDIR", Path::new("/runtime/boomux/shims")),
            ("BOOMUX_SHELL_ID", Path::new("shell-1")),
            ("BOOMUX_RUN_ID", Path::new("run-1")),
        ]);
        environment.variables.push(UnixEnvironmentVariable {
            name: b"KEEP".to_vec(),
            value: b"value".to_vec(),
        });

        let sanitized = sanitize_opencode_shim_environment(&environment);
        assert_eq!(
            environment_value(&sanitized, b"PATH").as_deref(),
            Some(std::ffi::OsStr::new("/usr/local/bin:/usr/bin"))
        );
        for name in [
            b"BOOMUX_ORIGINAL_PATH".as_slice(),
            b"BOOMUX_OPENCODE_SHIM_DIR",
            b"BOOMUX_REAL_OPENCODE",
            b"BOOMUX_REAL_CLAUDE",
            b"BOOMUX_REAL_CODEX",
            b"BOOMUX_REAL_KIRO",
            b"BOOMUX_CODEX_RUN_SCOPED",
            b"BOOMUX_KIRO_RUN_SCOPED",
            b"BOOMUX_CLAUDE_REMOTE_CONTROL",
            b"BOOMUX_OPENCODE_TUI_CONFIG",
            b"BOOMUX_SHIM_EXECUTABLE",
            b"BOOMUX_OPENCODE_SHARED_GENERATION",
            b"BOOMUX_OPENCODE_CLAIM_HOLDER",
            b"BOOMUX_USER_ZDOTDIR",
            b"OPENCODE_TUI_CONFIG",
        ] {
            assert!(
                environment_value(&sanitized, name).is_none(),
                "retained {name:?}"
            );
        }
        assert_eq!(
            environment_value(&sanitized, b"BOOMUX_SHELL_ID").unwrap(),
            "shell-1"
        );
        assert_eq!(
            environment_value(&sanitized, b"BOOMUX_RUN_ID").unwrap(),
            "run-1"
        );
        assert_eq!(environment_value(&sanitized, b"KEEP").unwrap(), "value");
        assert_eq!(
            environment_value(&sanitized, b"ZDOTDIR").unwrap(),
            "/home/user"
        );
    }

    #[test]
    fn common_shell_startup_adapters_reassert_the_scoped_shim_after_user_config() {
        let mut environment = test_environment(&[
            (
                "BOOMUX_OPENCODE_SHIM_DIR",
                Path::new("/runtime/boomux/shims"),
            ),
            ("HOME", Path::new("/home/user")),
            ("ZDOTDIR", Path::new("/home/user/custom-zsh")),
        ]);

        assert_eq!(
            configure_opencode_shell_startup(Path::new("/bin/bash").as_os_str(), &mut environment),
            vec![
                std::ffi::OsString::from("--rcfile"),
                std::ffi::OsString::from("/runtime/boomux/shims/boomux.bashrc"),
            ]
        );

        assert!(
            configure_opencode_shell_startup(
                Path::new("/usr/bin/zsh").as_os_str(),
                &mut environment
            )
            .is_empty()
        );
        assert_eq!(
            environment_value(&environment, b"BOOMUX_USER_ZDOTDIR").unwrap(),
            "/home/user/custom-zsh"
        );
        assert_eq!(
            environment_value(&environment, b"ZDOTDIR").unwrap(),
            "/runtime/boomux/shims"
        );

        let fish = configure_opencode_shell_startup(
            Path::new("/usr/bin/fish").as_os_str(),
            &mut environment,
        );
        assert_eq!(fish[0], "--init-command");
        assert!(fish[1].to_string_lossy().contains("BOOMUX_ORIGINAL_PATH"));
        assert!(
            configure_opencode_shell_startup(
                Path::new("/usr/bin/unknown-shell").as_os_str(),
                &mut environment
            )
            .is_empty()
        );
        for source in [OPENCODE_BASH_RC, OPENCODE_ZSH_ENV, OPENCODE_ZSH_RC] {
            assert!(
                std::str::from_utf8(source)
                    .unwrap()
                    .contains("BOOMUX_OPENCODE_SHIM_DIR")
            );
        }
        assert!(
            std::str::from_utf8(OPENCODE_BASH_RC)
                .unwrap()
                .contains("builtin hash -r")
        );
    }

    #[test]
    fn shim_assets_are_private_and_forward_exact_arguments() {
        let directory = env::temp_dir().join(format!("boomux-opencode-shim-{}", Uuid::new_v4()));
        let runtime = directory.join("runtime");
        let bin = directory.join("bin");
        fs::create_dir_all(&runtime).unwrap();
        fs::create_dir_all(&bin).unwrap();
        let host = bin.join("opencode");
        fs::write(&host, b"#!/bin/sh\nprintf '<%s>\\n' \"$@\"\n").unwrap();
        fs::set_permissions(&host, fs::Permissions::from_mode(0o700)).unwrap();
        let claude = bin.join("claude");
        fs::write(&claude, b"#!/bin/sh\nprintf '[%s]\\n' \"$@\"\n").unwrap();
        fs::set_permissions(&claude, fs::Permissions::from_mode(0o700)).unwrap();
        let codex = bin.join("codex");
        fs::write(&codex, b"#!/bin/sh\nprintf '{%s}\\n' \"$@\"\n").unwrap();
        fs::set_permissions(&codex, fs::Permissions::from_mode(0o700)).unwrap();
        let kiro = bin.join("kiro-cli");
        fs::write(&kiro, b"#!/bin/sh\nprintf '(%s)\\n' \"$@\"\n").unwrap();
        fs::set_permissions(&kiro, fs::Permissions::from_mode(0o700)).unwrap();
        let environment = test_environment(&[("XDG_RUNTIME_DIR", &runtime), ("PATH", &bin)]);

        let injected = inject_opencode_shim_environment(&environment, true).unwrap();
        let shim =
            PathBuf::from(environment_value(&injected, b"BOOMUX_OPENCODE_SHIM_DIR").unwrap())
                .join("opencode");
        assert_eq!(
            fs::metadata(&shim).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(
            fs::metadata(shim.with_file_name("tui.json"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        let output = Command::new(&shim)
            .args(["", "$(touch should-not-exist)", "semi;colon", "two words"])
            .env("BOOMUX_REAL_OPENCODE", &host)
            .output()
            .unwrap();
        assert!(output.status.success());
        assert_eq!(
            String::from_utf8(output.stdout).unwrap(),
            "<>\n<$(touch should-not-exist)>\n<semi;colon>\n<two words>\n"
        );
        let noninteractive = Command::new(&shim)
            .env("BOOMUX_REAL_OPENCODE", &host)
            .env("BOOMUX_SHELL_ID", "shell-1")
            .env("BOOMUX_RUN_ID", "run-1")
            .output()
            .unwrap();
        assert!(noninteractive.status.success());
        assert_eq!(noninteractive.stdout, b"<>\n");
        assert!(
            std::str::from_utf8(OPENCODE_SHIM)
                .unwrap()
                .contains("exec \"$BOOMUX_SHIM_EXECUTABLE\" opencode shared")
        );
        let claude_shim = shim.with_file_name("claude");
        let explicit = Command::new(&claude_shim)
            .args(["--resume", "exact; id"])
            .env("BOOMUX_REAL_CLAUDE", &claude)
            .env("BOOMUX_CLAUDE_REMOTE_CONTROL", "1")
            .output()
            .unwrap();
        assert!(explicit.status.success());
        assert_eq!(explicit.stdout, b"[--resume]\n[exact; id]\n");
        assert!(
            std::str::from_utf8(CLAUDE_SHIM)
                .unwrap()
                .contains("exec \"$BOOMUX_REAL_CLAUDE\" --remote-control")
        );
        let codex_shim = shim.with_file_name("codex");
        assert_eq!(
            fs::metadata(&codex_shim).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(
            environment_value(&injected, b"BOOMUX_REAL_CODEX").as_deref(),
            Some(codex.as_os_str())
        );
        assert!(
            std::str::from_utf8(CODEX_SHIM)
                .unwrap()
                .contains("codex launch -- \"$@\"")
        );
        let kiro_shim = shim.with_file_name("kiro-cli");
        assert_eq!(
            fs::metadata(&kiro_shim).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(
            environment_value(&injected, b"BOOMUX_REAL_KIRO").as_deref(),
            Some(kiro.as_os_str())
        );
        assert!(
            std::str::from_utf8(KIRO_SHIM)
                .unwrap()
                .contains("kiro launch -- \"$@\"")
        );
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn missing_agent_hosts_still_prepare_the_late_install_kiro_shim() {
        let directory = env::temp_dir().join(format!("boomux-opencode-missing-{}", Uuid::new_v4()));
        let runtime = directory.join("runtime");
        let empty_bin = directory.join("bin");
        fs::create_dir_all(&runtime).unwrap();
        fs::create_dir_all(&empty_bin).unwrap();
        let environment = test_environment(&[("XDG_RUNTIME_DIR", &runtime), ("PATH", &empty_bin)]);
        let sanitized = sanitize_opencode_shim_environment(&environment);
        let injected = inject_opencode_shim_environment(&sanitized, true).unwrap();
        let shim_dir =
            PathBuf::from(environment_value(&injected, b"BOOMUX_OPENCODE_SHIM_DIR").unwrap());
        assert!(shim_dir.join("kiro-cli").is_file());
        assert_eq!(environment_value(&injected, b"BOOMUX_REAL_KIRO"), None);
        assert_eq!(
            env::split_paths(&environment_value(&injected, b"PATH").unwrap())
                .next()
                .as_deref(),
            Some(shim_dir.as_path())
        );
        fs::remove_dir_all(directory).unwrap();
    }

    fn add_recovery_agent(
        registry: &DaemonService,
        shell: &Shell,
        run_id: &str,
        integration: &str,
        external_session_id: &str,
    ) -> String {
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
                working_contexts: Vec::new(),
            }),
        });
        let agent_id = agent.id.clone();
        lock(&registry.durable.state)
            .unwrap()
            .agents
            .insert(agent.id.clone(), agent);
        agent_id
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
        let agent_id = add_recovery_agent(&registry, &shell, &run.id, "opencode", "session-1");
        add_recovery_agent(&registry, &shell, &run.id, "native-test", "other");

        let snapshot = registry.snapshot().unwrap();
        let snapshot = &snapshot.workspaces[0].shells[0];
        assert_eq!(snapshot.status, ShellStatus::Pending);
        assert_eq!(
            snapshot.run.as_ref().map(|run| run.id.as_str()),
            Some(run.id.as_str())
        );
        assert_eq!(
            snapshot.recovered_agent_id.as_deref(),
            Some(agent_id.as_str())
        );
        let mut projection = shell.node_projection().unwrap();
        registry.add_recovery_projection(&mut projection).unwrap();
        assert_eq!(projection.run_id.as_deref(), Some(run.id.as_str()));
        assert_eq!(
            projection.recovered_agent_id.as_deref(),
            Some(agent_id.as_str())
        );

        let resumable = registry
            .resumable_agent(&shell, Some(&run))
            .unwrap()
            .unwrap();
        assert_eq!(resumable.agent_id, agent_id);
        assert_eq!(resumable.integration, "opencode");
        assert_eq!(resumable.external_session_id, "session-1");
        assert_eq!(
            resumable.command,
            ["/opt/bin/opencode", "--session", "session-1"]
        );

        let agent = lock(&registry.durable.state)
            .unwrap()
            .agents
            .get(&agent_id)
            .unwrap()
            .clone();
        lock(&agent.state).unwrap().observation.authority = AgentAuthority::TerminalHeuristic;
        assert!(
            registry
                .resumable_agent(&shell, Some(&run))
                .unwrap()
                .is_none()
        );
        assert!(registry.recovery_presentation(&shell.id).unwrap().is_none());
    }

    #[test]
    fn session_display_name_mutation_replays_and_reset_restores_derived_name() {
        let registry = DaemonService::default();
        let (shell, run) = recovery_shell(&registry, vec!["agent".into()]);
        let agent_id = add_recovery_agent(
            &registry,
            &shell,
            &run.id,
            "native-test",
            "external-session",
        );
        let workspace = registry.workspace(&shell.workspace_id).unwrap();
        lock(&workspace.agent_ids).unwrap().push(agent_id);
        let snapshot = registry.snapshot().unwrap();
        let projected = host_services::sessions_with_catalog(&snapshot, &[]);
        let session_id = projected[0].id.clone();
        let revision = projected[0].workspace_revision;
        let operation_id = Uuid::new_v4().to_string();
        assert!(
            registry
                .set_agent_session_display_name(
                    "not-a-uuid".into(),
                    session_id.clone(),
                    revision,
                    Some("invalid operation".into()),
                )
                .unwrap_err()
                .to_string()
                .contains("invalid Session display-name operation ID")
        );
        let response = registry.route_node_operation(
            "unregistered-node",
            RoutedOperation::SetAgentSessionDisplayName {
                operation_id: "not-a-uuid".into(),
                session_id: session_id.clone(),
                expected_workspace_revision: revision,
                display_name: Some("invalid routed operation".into()),
            },
        );
        assert!(matches!(
            response,
            Response::Error {
                code: Some(ErrorCode::UnsupportedVersion),
                ref message,
            } if message.contains("Agent Session mutation has been removed")
        ));

        let response = registry
            .set_agent_session_display_name(
                operation_id.clone(),
                session_id.clone(),
                revision,
                Some("  Checkout   retry investigation  ".into()),
            )
            .unwrap();
        let Response::AgentSessionDisplayName { outcome: result } = response else {
            panic!("unexpected Session display-name response");
        };
        assert_eq!(
            result.user_display_name.as_deref(),
            Some("Checkout retry investigation")
        );
        assert_eq!(result.session_id, session_id);
        assert_eq!(result.workspace_id, workspace.id);
        assert_eq!(result.workspace_revision, revision + 1);
        assert!(result.changed);
        let accepted = result.clone();

        let replay = registry
            .set_agent_session_display_name(
                operation_id.clone(),
                session_id.clone(),
                revision,
                Some("Checkout retry investigation".into()),
            )
            .unwrap();
        assert_eq!(
            replay,
            Response::AgentSessionDisplayName { outcome: result }
        );
        assert_eq!(
            registry
                .set_agent_session_display_name(
                    operation_id.clone(),
                    session_id.clone(),
                    revision,
                    Some("different request".into()),
                )
                .unwrap_err()
                .wire_code(),
            ErrorCode::IdempotencyExpired
        );

        let response = registry
            .set_agent_session_display_name(
                Uuid::new_v4().to_string(),
                session_id.clone(),
                revision + 1,
                None,
            )
            .unwrap();
        let Response::AgentSessionDisplayName { outcome: result } = response else {
            panic!("unexpected Session reset response");
        };
        assert!(result.user_display_name.is_none());
        assert_eq!(result.workspace_revision, revision + 2);
        assert!(result.changed);
        let snapshot = registry.snapshot().unwrap();
        let sessions = registry
            .host_sessions(&snapshot, Some(&workspace.id))
            .unwrap();
        assert_eq!(sessions[0].description, "native-test");

        let replay = registry
            .set_agent_session_display_name(
                operation_id.clone(),
                session_id.clone(),
                revision,
                Some("Checkout retry investigation".into()),
            )
            .unwrap();
        assert_eq!(
            replay,
            Response::AgentSessionDisplayName { outcome: accepted }
        );

        lock(&workspace.agent_ids).unwrap().clear();
        let replay_without_projection = registry
            .set_agent_session_display_name(
                operation_id,
                session_id,
                revision,
                Some("Checkout retry investigation".into()),
            )
            .unwrap();
        assert_eq!(replay_without_projection, replay);

        registry.close_workspace(&workspace.id).unwrap();
        assert!(
            registry
                .capture_persisted_state()
                .unwrap()
                .state
                .workspaces
                .is_empty()
        );
    }

    #[test]
    fn session_hide_is_workspace_scoped_revision_safe_and_semantically_idempotent() {
        let registry = DaemonService::default();
        let (shell, run) = recovery_shell(&registry, vec!["agent".into()]);
        let agent_id = add_recovery_agent(
            &registry,
            &shell,
            &run.id,
            "native-test",
            "external-session",
        );
        let workspace = registry.workspace(&shell.workspace_id).unwrap();
        lock(&workspace.agent_ids).unwrap().push(agent_id.clone());
        let projected = registry
            .host_sessions(&registry.snapshot().unwrap(), Some(&workspace.id))
            .unwrap();
        let session_id = projected[0].id.clone();
        let revision = projected[0].workspace_revision;
        let operation_id = Uuid::new_v4().to_string();

        let response = registry
            .hide_agent_session(
                operation_id.clone(),
                session_id.clone(),
                workspace.id.clone(),
                revision,
            )
            .unwrap();
        let Response::AgentSessionHidden { outcome: hidden } = response else {
            panic!("unexpected Session hide response");
        };
        assert!(hidden.changed);
        assert_eq!(hidden.workspace_revision, revision + 1);
        assert_eq!(
            registry.agent(&agent_id).unwrap().snapshot().unwrap().id,
            agent_id
        );

        for version in [50, 51] {
            for operation in [
                HostServiceOperation::ListAgentSessions {
                    workspace_id: Some(workspace.id.clone()),
                },
                HostServiceOperation::InspectAgentSession {
                    session_id: session_id.clone(),
                },
            ] {
                assert_eq!(
                    registry
                        .host_service_for_version(operation, version)
                        .unwrap_err()
                        .wire_code(),
                    ErrorCode::UnsupportedVersion
                );
            }
        }

        let replay = registry
            .hide_agent_session(
                operation_id,
                session_id.clone(),
                workspace.id.clone(),
                revision,
            )
            .unwrap();
        assert_eq!(replay, Response::AgentSessionHidden { outcome: hidden });

        assert_eq!(
            registry
                .hide_agent_session(
                    Uuid::new_v4().to_string(),
                    session_id.clone(),
                    workspace.id.clone(),
                    revision,
                )
                .unwrap_err()
                .wire_code(),
            ErrorCode::RevisionAhead
        );

        let fresh = registry
            .hide_agent_session(
                Uuid::new_v4().to_string(),
                session_id.clone(),
                workspace.id.clone(),
                revision + 1,
            )
            .unwrap();
        let Response::AgentSessionHidden { outcome: fresh } = fresh else {
            panic!("unexpected repeated Session hide response");
        };
        assert!(!fresh.changed);
        assert_eq!(fresh.workspace_revision, revision + 1);
        assert_eq!(lock(&workspace.hidden_sessions).unwrap().len(), 1);
        assert_eq!(lock(&workspace.session_hide_operations).unwrap().len(), 2);
        assert!(matches!(
            &lock(&workspace.hidden_sessions).unwrap()[0].session,
            PersistedSessionIdentity::External { external_session_id }
                if external_session_id == "external-session"
        ));
    }

    #[test]
    fn session_display_name_persistence_failure_rolls_back_without_event() {
        let directory = env::temp_dir().join(format!("boomux-session-name-{}", Uuid::new_v4()));
        let registry =
            DaemonService::restore(StateStore::at(directory.join("state.json")), false, None)
                .unwrap();
        let (shell, run) = recovery_shell(&registry, vec!["agent".into()]);
        let agent_id = add_recovery_agent(
            &registry,
            &shell,
            &run.id,
            "native-test",
            "external-session",
        );
        let workspace = registry.workspace(&shell.workspace_id).unwrap();
        lock(&workspace.agent_ids).unwrap().push(agent_id);
        let projected = registry
            .host_sessions(&registry.snapshot().unwrap(), Some(&workspace.id))
            .unwrap();
        let session_id = projected[0].id.clone();
        let revision = projected[0].workspace_revision;
        let events_before = registry.events.manifest().unwrap().events.len();

        registry.fail_next_persistence();
        assert_eq!(
            registry
                .set_agent_session_display_name(
                    Uuid::new_v4().to_string(),
                    session_id.clone(),
                    revision,
                    Some("not committed".into()),
                )
                .unwrap_err()
                .wire_code(),
            ErrorCode::PersistenceFailed
        );

        assert_eq!(*lock(&workspace.revision).unwrap(), revision);
        assert!(lock(&workspace.session_display_names).unwrap().is_empty());
        assert!(
            lock(&workspace.session_display_name_operations)
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            registry.events.manifest().unwrap().events.len(),
            events_before
        );

        registry
            .set_agent_session_display_name(
                Uuid::new_v4().to_string(),
                session_id,
                revision,
                Some("committed".into()),
            )
            .unwrap();
        assert_eq!(
            registry.events.manifest().unwrap().events.len(),
            events_before + 1
        );
        let persisted = registry.capture_persisted_state().unwrap();
        let receipt =
            serde_json::to_value(&persisted.state.workspaces[0].session_display_name_operations[0])
                .unwrap();
        assert_eq!(
            receipt["result"],
            serde_json::json!({
                "session_id": projected[0].id,
                "workspace_id": workspace.id,
                "user_display_name": "committed",
                "workspace_revision": revision + 1,
                "changed": true
            })
        );
        assert!(receipt.to_string().find("description").is_none());
        drop(registry);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn session_hide_persistence_failure_rolls_back_without_event() {
        let directory = env::temp_dir().join(format!("boomux-session-hide-{}", Uuid::new_v4()));
        let registry =
            DaemonService::restore(StateStore::at(directory.join("state.json")), false, None)
                .unwrap();
        let (shell, run) = recovery_shell(&registry, vec!["agent".into()]);
        let agent_id = add_recovery_agent(
            &registry,
            &shell,
            &run.id,
            "native-test",
            "external-session",
        );
        let workspace = registry.workspace(&shell.workspace_id).unwrap();
        lock(&workspace.agent_ids).unwrap().push(agent_id);
        let projected = registry
            .host_sessions(&registry.snapshot().unwrap(), Some(&workspace.id))
            .unwrap();
        let session_id = projected[0].id.clone();
        let revision = projected[0].workspace_revision;
        let events_before = registry.events.manifest().unwrap().events.len();

        registry.fail_next_persistence();
        assert_eq!(
            registry
                .hide_agent_session(
                    Uuid::new_v4().to_string(),
                    session_id.clone(),
                    workspace.id.clone(),
                    revision,
                )
                .unwrap_err()
                .wire_code(),
            ErrorCode::PersistenceFailed
        );
        assert_eq!(*lock(&workspace.revision).unwrap(), revision);
        assert!(lock(&workspace.hidden_sessions).unwrap().is_empty());
        assert!(lock(&workspace.session_hide_operations).unwrap().is_empty());
        assert_eq!(
            registry.events.manifest().unwrap().events.len(),
            events_before
        );

        registry
            .hide_agent_session(
                Uuid::new_v4().to_string(),
                session_id,
                workspace.id.clone(),
                revision,
            )
            .unwrap();
        assert_eq!(
            registry.events.manifest().unwrap().events.len(),
            events_before + 1
        );
        assert_eq!(
            registry.capture_persisted_state().unwrap().state.workspaces[0]
                .hidden_sessions
                .len(),
            1
        );
        drop(registry);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn workspace_scoped_session_catalog_excludes_unrelated_directories() {
        let registry = DaemonService::default();
        let selected_cwd = env::temp_dir().join(format!("selected-{}", Uuid::new_v4()));
        let unrelated_cwd = env::temp_dir().join(format!("unrelated-{}", Uuid::new_v4()));
        fs::create_dir(&selected_cwd).unwrap();
        fs::create_dir(&unrelated_cwd).unwrap();
        let selected = registry
            .create_workspace(
                "selected".into(),
                vec![ShellSpec::login("selected", selected_cwd.clone())],
            )
            .unwrap();
        registry
            .create_workspace(
                "unrelated".into(),
                vec![ShellSpec::login("unrelated", unrelated_cwd.clone())],
            )
            .unwrap();

        registry
            .host_sessions(&registry.snapshot().unwrap(), Some(&selected.id))
            .unwrap();

        let catalog = lock(&registry.host_session_catalog.state).unwrap();
        assert!(
            catalog
                .entries
                .keys()
                .any(|request| request.directory == selected_cwd)
        );
        assert!(
            !catalog
                .entries
                .keys()
                .any(|request| request.directory == unrelated_cwd)
        );
        drop(catalog);
        fs::remove_dir_all(selected_cwd).unwrap();
        fs::remove_dir_all(unrelated_cwd).unwrap();
    }

    #[test]
    fn session_catalog_cache_is_single_flight_without_holding_its_state_lock() {
        use std::sync::Barrier;

        let cache = Arc::new(HostSessionCatalogCache::default());
        let requests = vec![crate::host_session_titles::ProjectionRequest {
            integration: "opencode".into(),
            directory: "/repo".into(),
        }];
        let calls = Arc::new(AtomicU64::new(0));
        let barrier = Arc::new(Barrier::new(2));
        let (started_tx, started_rx) = mpsc::channel();

        let first_cache = Arc::clone(&cache);
        let first_requests = requests.clone();
        let first_calls = Arc::clone(&calls);
        let first_barrier = Arc::clone(&barrier);
        let first = thread::spawn(move || {
            first_cache
                .records_with(&first_requests, &|requests| {
                    first_calls.fetch_add(1, Ordering::SeqCst);
                    started_tx.send(()).unwrap();
                    first_barrier.wait();
                    vec![None; requests.len()]
                })
                .unwrap()
        });
        started_rx.recv_timeout(Duration::from_secs(1)).unwrap();

        let second_cache = Arc::clone(&cache);
        let second_requests = requests.clone();
        let second_calls = Arc::clone(&calls);
        let second = thread::spawn(move || {
            second_cache
                .records_with(&second_requests, &|requests| {
                    second_calls.fetch_add(1, Ordering::SeqCst);
                    vec![None; requests.len()]
                })
                .unwrap()
        });

        assert!(cache.state.try_lock().is_ok());
        barrier.wait();
        assert!(first.join().unwrap().is_empty());
        assert!(second.join().unwrap().is_empty());
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn interrupted_claude_agent_builds_exact_resume_command() {
        let registry = DaemonService::default();
        let (shell, run) = recovery_shell(&registry, vec!["/opt/bin/claude".into()]);
        let agent_id = add_recovery_agent(&registry, &shell, &run.id, "claude", "claude-exact");

        assert_eq!(
            registry.snapshot().unwrap().workspaces[0].shells[0]
                .recovered_agent_id
                .as_deref(),
            Some(agent_id.as_str())
        );
        let resumable = registry
            .resumable_agent(&shell, Some(&run))
            .unwrap()
            .unwrap();
        assert_eq!(resumable.agent_id, agent_id);
        assert_eq!(resumable.integration, "claude");
        assert_eq!(resumable.external_session_id, "claude-exact");
        assert_eq!(
            resumable.command,
            ["/opt/bin/claude", "--resume", "claude-exact"]
        );
    }

    #[test]
    fn interrupted_codex_agent_builds_exact_resume_subcommand() {
        let registry = DaemonService::default();
        let (shell, run) = recovery_shell(&registry, vec!["/opt/bin/codex".into()]);
        let agent_id = add_recovery_agent(&registry, &shell, &run.id, "codex", "codex-exact");

        let resumable = registry
            .resumable_agent(&shell, Some(&run))
            .unwrap()
            .unwrap();
        assert_eq!(resumable.agent_id, agent_id);
        assert_eq!(resumable.integration, "codex");
        assert_eq!(resumable.external_session_id, "codex-exact");
        assert_eq!(
            resumable.command,
            ["/opt/bin/codex", "resume", "codex-exact"]
        );
    }

    #[test]
    fn interrupted_kiro_agent_builds_exact_v3_resume_command() {
        let registry = DaemonService::default();
        let (shell, run) = recovery_shell(&registry, vec!["/opt/kiro/kiro-cli".into()]);
        let agent_id = add_recovery_agent(&registry, &shell, &run.id, "kiro", "kiro-exact");

        let resumable = registry
            .resumable_agent(&shell, Some(&run))
            .unwrap()
            .unwrap();
        assert_eq!(resumable.agent_id, agent_id);
        assert_eq!(resumable.integration, "kiro");
        assert_eq!(resumable.external_session_id, "kiro-exact");
        assert_eq!(
            resumable.command,
            [
                "/opt/kiro/kiro-cli",
                "--v3",
                "chat",
                "--resume-id",
                "kiro-exact",
            ]
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
                .resumable_agent(&shell, Some(&run))
                .unwrap()
                .is_none()
        );
        assert!(registry.recovery_presentation(&shell.id).unwrap().is_none());

        lock(&registry.durable.state)
            .unwrap()
            .agents
            .retain(|_, agent| agent.external_session_id.as_deref() == Some("session-1"));
        registry.notification_settings.resume_agents = false;
        assert!(
            registry
                .resumable_agent(&shell, Some(&run))
                .unwrap()
                .is_none()
        );
        assert!(registry.recovery_presentation(&shell.id).unwrap().is_none());
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

    fn opencode_agent_spec(external_session_id: &str, state: AgentState) -> AgentRegistrationSpec {
        AgentRegistrationSpec {
            name: "opencode-agent".into(),
            integration: "opencode".into(),
            external_session_id: Some(external_session_id.into()),
            report: AgentReport {
                state,
                authority: AgentAuthority::LifecycleIntegration,
                evidence: "OpenCode attachment test".into(),
                confidence: 100,
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

    fn kiro_report(state: AgentState) -> AgentReport {
        AgentReport {
            state,
            authority: AgentAuthority::LifecycleIntegration,
            evidence: "Kiro test hook".into(),
            confidence: 100,
        }
    }

    fn test_kiro_holder(
        registry: &DaemonService,
        shell_id: &str,
        run_id: &str,
    ) -> (String, StdChild) {
        let child = Command::new("/bin/sleep")
            .arg("30")
            .env("BOOMUX_SHELL_ID", shell_id)
            .env("BOOMUX_RUN_ID", run_id)
            .spawn()
            .unwrap();
        let pid = child.id();
        let deadline = Instant::now() + Duration::from_secs(1);
        while Instant::now() < deadline
            && !process_has_environment(pid, b"BOOMUX_RUN_ID", run_id.as_bytes()).unwrap_or(false)
        {
            thread::sleep(Duration::from_millis(1));
        }
        let stat = fs::read_to_string(format!("/proc/{pid}/stat")).unwrap();
        let holder_id = Uuid::new_v4().to_string();
        lock(&registry.kiro.state).unwrap().insert(
            holder_id.clone(),
            KiroLaunchHolder {
                pid,
                start_time: process_start_time(&stat).unwrap(),
                process_group_leader: proc_process_group(&stat) == Some(pid as libc::pid_t),
                shell_id: shell_id.into(),
                run_id: run_id.into(),
                sessions: HashMap::new(),
            },
        );
        (holder_id, child)
    }

    fn report_test_kiro(
        registry: &DaemonService,
        holder_id: &str,
        session_id: &str,
    ) -> AgentInstanceSnapshot {
        let Response::Agent { agent } = registry
            .dispatch(Request::ReportKiroHook {
                holder_id: holder_id.into(),
                session_id: session_id.into(),
                report: kiro_report(AgentState::Working),
            })
            .unwrap()
        else {
            panic!("expected Kiro Agent");
        };
        agent
    }

    #[test]
    fn kiro_hooks_accept_only_documented_lifecycle_states() {
        let registry = DaemonService::default();
        let (_, shell, _) = running_shell(&registry);
        let run_id = shell.snapshot().unwrap().run.unwrap().id;
        let (holder_id, mut process) = test_kiro_holder(&registry, &shell.id, &run_id);

        for state in [AgentState::Unknown, AgentState::Working, AgentState::Idle] {
            let Response::Agent { agent } = registry
                .dispatch(Request::ReportKiroHook {
                    holder_id: holder_id.clone(),
                    session_id: "session-a".into(),
                    report: kiro_report(state),
                })
                .unwrap()
            else {
                panic!("expected Kiro Agent");
            };
            assert_eq!(agent.observation.state, state);
        }
        for state in [AgentState::Blocked, AgentState::Inactive, AgentState::Done] {
            assert!(
                registry
                    .dispatch(Request::ReportKiroHook {
                        holder_id: holder_id.clone(),
                        session_id: "session-a".into(),
                        report: kiro_report(state),
                    })
                    .is_err(),
                "accepted unsupported Kiro state {state:?}"
            );
        }

        process.kill().unwrap();
        process.wait().unwrap();
    }

    #[test]
    fn kiro_reconciliation_inactivates_agents_without_live_holder_authority() {
        let registry = DaemonService::default();
        let (_, shell, _) = running_shell(&registry);
        let run_id = shell.snapshot().unwrap().run.unwrap().id;
        let Response::Agent { agent: orphan } = registry
            .dispatch(Request::RegisterAgent {
                shell_id: shell.id.clone(),
                run_id: run_id.clone(),
                spec: AgentRegistrationSpec {
                    name: "legacy Kiro".into(),
                    integration: "kiro".into(),
                    external_session_id: Some("legacy-session".into()),
                    report: kiro_report(AgentState::Idle),
                },
            })
            .unwrap()
        else {
            panic!("expected registered Kiro Agent");
        };
        let (holder_id, mut process) = test_kiro_holder(&registry, &shell.id, &run_id);
        let owned = report_test_kiro(&registry, &holder_id, "owned-session");

        registry.fail_after_mutation.store(true, Ordering::Release);
        assert!(registry.reconcile_dead_kiro_holders().is_err());
        assert_eq!(
            registry
                .durable
                .agent(&orphan.id)
                .unwrap()
                .snapshot()
                .unwrap()
                .observation
                .state,
            AgentState::Idle
        );

        registry.reconcile_dead_kiro_holders().unwrap();
        let orphan = registry
            .durable
            .agent(&orphan.id)
            .unwrap()
            .snapshot()
            .unwrap();
        assert_eq!(orphan.observation.state, AgentState::Inactive);
        assert_eq!(
            orphan.observation.evidence,
            "Kiro launch authority unavailable"
        );
        assert_eq!(
            registry
                .durable
                .agent(&owned.id)
                .unwrap()
                .snapshot()
                .unwrap()
                .observation
                .state,
            AgentState::Working
        );

        process.kill().unwrap();
        process.wait().unwrap();
    }

    #[test]
    fn kiro_sequential_processes_inactivate_exact_exited_holder_sessions() {
        let registry = DaemonService::default();
        let (_, shell, _) = running_shell(&registry);
        let run_id = shell.snapshot().unwrap().run.unwrap().id;
        let (holder_a, mut process_a) = test_kiro_holder(&registry, &shell.id, &run_id);
        let agent_a = report_test_kiro(&registry, &holder_a, "session-a");
        assert_eq!(agent_a.observation.state, AgentState::Working);

        process_a.kill().unwrap();
        process_a.wait().unwrap();
        let Response::Snapshot { snapshot } = registry.dispatch(Request::Snapshot).unwrap() else {
            panic!("expected snapshot");
        };
        assert_eq!(
            snapshot.workspaces[0].agents[0].observation.state,
            AgentState::Working
        );
        registry.fail_after_mutation.store(true, Ordering::Release);
        assert!(registry.reconcile_dead_kiro_holders().is_err());
        assert_eq!(
            registry
                .durable
                .agent(&agent_a.id)
                .unwrap()
                .snapshot()
                .unwrap()
                .observation
                .state,
            AgentState::Working
        );
        assert!(lock(&registry.kiro.state).unwrap().contains_key(&holder_a));
        registry.reconcile_dead_kiro_holders().unwrap();
        let Response::Snapshot { snapshot } = registry.dispatch(Request::Snapshot).unwrap() else {
            panic!("expected snapshot");
        };
        let inactive_a = snapshot.workspaces[0]
            .agents
            .iter()
            .find(|agent| agent.id == agent_a.id)
            .unwrap();
        assert_eq!(inactive_a.observation.state, AgentState::Inactive);
        assert!(inactive_a.attention.is_none());
        assert!(
            lock(&registry.events.state)
                .unwrap()
                .events
                .iter()
                .any(|event| {
                    matches!(
                        &event.kind,
                        DaemonEventKind::AgentStateChanged { agent, .. }
                            if agent.id == agent_a.id
                                && agent.observation.state == AgentState::Inactive
                    )
                })
        );
        assert!(
            registry
                .dispatch(Request::ReportKiroHook {
                    holder_id: holder_a.clone(),
                    session_id: "session-a".into(),
                    report: kiro_report(AgentState::Working),
                })
                .is_err()
        );

        let (holder_b, mut process_b) = test_kiro_holder(&registry, &shell.id, &run_id);
        let agent_b = report_test_kiro(&registry, &holder_b, "session-b");
        let agents = lock(&registry.durable.state)
            .unwrap()
            .agents
            .values()
            .map(|agent| agent.snapshot().unwrap())
            .collect::<Vec<_>>();
        let current = agents
            .iter()
            .filter(|agent| {
                agent.run_id == run_id
                    && !matches!(
                        agent.observation.state,
                        AgentState::Inactive | AgentState::Done
                    )
            })
            .collect::<Vec<_>>();
        assert_eq!(current.len(), 1);
        assert_eq!(current[0].id, agent_b.id);
        assert!(
            agents
                .iter()
                .all(|agent| agent.observation.state != AgentState::Done)
        );
        process_b.kill().unwrap();
        process_b.wait().unwrap();
    }

    #[test]
    fn kiro_holder_release_survives_its_workspace_removing_the_agent_first() {
        let registry = DaemonService::default();
        let (workspace, shell, _) = running_shell(&registry);
        let run_id = shell.snapshot().unwrap().run.unwrap().id;
        let (holder_id, mut process) = test_kiro_holder(&registry, &shell.id, &run_id);
        let agent = report_test_kiro(&registry, &holder_id, "removed-session");

        registry.close_workspace(&workspace.id).unwrap();
        assert!(registry.durable.agent(&agent.id).is_err());
        assert!(matches!(
            registry.release_kiro_launch_holder(&holder_id).unwrap(),
            Response::KiroLaunchHolderReleased { released: true }
        ));

        assert!(!lock(&registry.kiro.state).unwrap().contains_key(&holder_id));
        process.kill().unwrap();
        process.wait().unwrap();
    }

    #[test]
    fn kiro_sessions_follow_all_and_only_their_live_holders() {
        let registry = DaemonService::default();
        let (_, shell, _) = running_shell(&registry);
        let run_id = shell.snapshot().unwrap().run.unwrap().id;
        let (holder_a, mut process_a) = test_kiro_holder(&registry, &shell.id, &run_id);
        let (holder_b, mut process_b) = test_kiro_holder(&registry, &shell.id, &run_id);
        let shared = report_test_kiro(&registry, &holder_a, "shared-session");
        let shared_again = report_test_kiro(&registry, &holder_b, "shared-session");
        assert_eq!(shared.id, shared_again.id);
        let separate = report_test_kiro(&registry, &holder_b, "separate-session");

        registry
            .dispatch(Request::ReleaseKiroLaunchHolder {
                holder_id: holder_a.clone(),
            })
            .unwrap();
        assert_eq!(
            registry
                .durable
                .agent(&shared.id)
                .unwrap()
                .snapshot()
                .unwrap()
                .observation
                .state,
            AgentState::Working
        );
        registry
            .dispatch(Request::ReleaseKiroLaunchHolder {
                holder_id: holder_b.clone(),
            })
            .unwrap();
        assert_eq!(
            registry
                .durable
                .agent(&shared.id)
                .unwrap()
                .snapshot()
                .unwrap()
                .observation
                .state,
            AgentState::Inactive
        );
        assert_eq!(
            registry
                .durable
                .agent(&separate.id)
                .unwrap()
                .snapshot()
                .unwrap()
                .observation
                .state,
            AgentState::Inactive
        );

        let (holder_c, mut process_c) = test_kiro_holder(&registry, &shell.id, &run_id);
        let reactivated = report_test_kiro(&registry, &holder_c, "shared-session");
        assert_eq!(reactivated.id, shared.id);
        assert_eq!(reactivated.observation.state, AgentState::Working);
        registry
            .dispatch(Request::ReleaseKiroLaunchHolder {
                holder_id: holder_c,
            })
            .unwrap();
        assert_eq!(
            registry
                .durable
                .agent(&shared.id)
                .unwrap()
                .snapshot()
                .unwrap()
                .observation
                .state,
            AgentState::Inactive
        );
        for process in [&mut process_a, &mut process_b, &mut process_c] {
            process.kill().unwrap();
            process.wait().unwrap();
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

    fn install_test_opencode_runtime(registry: &DaemonService) -> String {
        let mut command = Command::new("/bin/sleep");
        command.arg("30");
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() == -1 {
                    Err(io::Error::last_os_error())
                } else {
                    Ok(())
                }
            });
        }
        let child = command.spawn().unwrap();
        let generation_id = Uuid::new_v4().to_string();
        *lock(&registry.opencode.state).unwrap() = OpenCodeCoordinatorState {
            runtime: Some(OpenCodeRuntime {
                generation_id: generation_id.clone(),
                port: 4096,
                pid: child.id(),
                process: OpenCodeRuntimeProcess::Owned(child),
            }),
            claims: HashMap::new(),
        };
        generation_id
    }

    #[test]
    fn claude_remote_control_binding_requires_exact_active_claude_agent() {
        let registry = DaemonService::default();
        let (_, shell, _runtime) = running_shell(&registry);
        let run_id = match &*lock(&shell.lifecycle).unwrap() {
            ShellLifecycle::Running { run, .. } => run.id.clone(),
            _ => unreachable!(),
        };
        let Response::Agent { agent } = registry
            .dispatch(Request::EnsureAgent {
                shell_id: shell.id.clone(),
                run_id: run_id.clone(),
                spec: AgentRegistrationSpec {
                    name: "Claude Code".into(),
                    integration: "claude".into(),
                    external_session_id: Some("claude-session".into()),
                    report: agent_report(
                        AgentState::Idle,
                        AgentAuthority::LifecycleIntegration,
                        "Claude session idle",
                    ),
                },
            })
            .unwrap()
        else {
            panic!("unexpected Agent ensure response");
        };
        let binding = registry
            .set_claude_remote_control_binding(
                &agent.id,
                &shell.id,
                &run_id,
                Some("bridge/exact".into()),
            )
            .unwrap()
            .unwrap();
        assert_eq!(binding.bridge_session_id, "bridge/exact");
        assert_eq!(
            registry
                .get_claude_remote_control_binding(&agent.id, &shell.id, &run_id)
                .unwrap(),
            Some(binding)
        );
        assert!(
            registry
                .set_claude_remote_control_binding(
                    &agent.id,
                    &shell.id,
                    &Uuid::new_v4().to_string(),
                    Some("other".into()),
                )
                .is_err()
        );
        assert!(
            registry
                .set_claude_remote_control_binding(
                    &agent.id,
                    &shell.id,
                    &run_id,
                    Some("bad\nbridge".into()),
                )
                .is_err()
        );
        registry
            .dispatch(Request::ReportAgent {
                agent_id: agent.id.clone(),
                run_id: run_id.clone(),
                report: agent_report(
                    AgentState::Inactive,
                    AgentAuthority::LifecycleIntegration,
                    "Claude session inactive",
                ),
            })
            .unwrap();
        assert!(
            registry
                .set_claude_remote_control_binding(
                    &agent.id,
                    &shell.id,
                    &run_id,
                    Some("inactive".into()),
                )
                .is_err()
        );
        assert_eq!(
            registry
                .set_claude_remote_control_binding(&agent.id, &shell.id, &run_id, None)
                .unwrap(),
            None
        );
        assert!(
            lock(&registry.claude_remote_control.state)
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            registry
                .get_claude_remote_control_binding(&agent.id, &shell.id, &run_id)
                .unwrap_err()
                .wire_code(),
            ErrorCode::RunChanged
        );
    }

    #[test]
    fn opencode_claims_support_renewal_multiple_holders_switch_and_safe_release() {
        let registry = DaemonService::default();
        let (_, shell, _runtime) = running_shell(&registry);
        let generation_id = install_test_opencode_runtime(&registry);
        let run_id = match &*lock(&shell.lifecycle).unwrap() {
            ShellLifecycle::Running { run, .. } => run.id.clone(),
            _ => unreachable!(),
        };
        let holder_one = Uuid::new_v4().to_string();
        let holder_two = Uuid::new_v4().to_string();
        let ensure = |holder_id: &str, root_session_id: &str| match registry
            .dispatch(Request::EnsureOpenCodeSessionClaim {
                generation_id: generation_id.clone(),
                holder_id: holder_id.into(),
                root_session_id: root_session_id.into(),
                shell_id: shell.id.clone(),
                run_id: run_id.clone(),
                spec: opencode_agent_spec(root_session_id, AgentState::Working),
            })
            .unwrap()
        {
            Response::OpenCodeSessionClaim { claim, agent } => (claim, agent),
            response => panic!("unexpected response: {response:?}"),
        };

        let (first, first_agent) = ensure(&holder_one, "ses_shared");
        let (renewed, _) = ensure(&holder_one, "ses_shared");
        assert_eq!(renewed.claim_id, first.claim_id);
        let (shared, _) = ensure(&holder_two, "ses_shared");
        assert_eq!(shared.claim_id, first.claim_id);
        assert_eq!(shared.holder_count, 2);

        let (switched, _) = ensure(&holder_one, "ses_new");
        assert_ne!(switched.claim_id, first.claim_id);
        let stale_release = registry
            .dispatch(Request::ReleaseOpenCodeSessionClaim {
                generation_id: generation_id.clone(),
                holder_id: holder_one.clone(),
                claim_id: first.claim_id,
            })
            .unwrap();
        assert!(matches!(
            stale_release,
            Response::OpenCodeSessionClaimReleased {
                released: false,
                ..
            }
        ));
        let resolved = registry
            .dispatch(Request::ResolveOpenCodeSessionClaim {
                generation_id: generation_id.clone(),
                root_session_id: "ses_shared".into(),
            })
            .unwrap();
        assert!(matches!(
            resolved,
            Response::OpenCodeSessionClaim { agent, .. } if agent.id == first_agent.id
        ));

        registry
            .dispatch(Request::ReportClaimedOpenCodeAgent {
                generation_id: generation_id.clone(),
                root_session_id: "ses_shared".into(),
                report: agent_report(
                    AgentState::Blocked,
                    AgentAuthority::LifecycleIntegration,
                    "claimed report",
                ),
            })
            .unwrap();
        registry
            .dispatch(Request::ReportClaimedOpenCodeAgent {
                generation_id: generation_id.clone(),
                root_session_id: "ses_shared".into(),
                report: agent_report(
                    AgentState::Done,
                    AgentAuthority::LifecycleIntegration,
                    "claimed completion",
                ),
            })
            .unwrap();
        let completed = registry
            .dispatch(Request::ResolveOpenCodeSessionClaim {
                generation_id: generation_id.clone(),
                root_session_id: "ses_shared".into(),
            })
            .unwrap_err();
        assert_eq!(completed.wire_code(), ErrorCode::NotFound);
        registry.opencode.shutdown().unwrap();
    }

    #[test]
    fn opencode_claim_expiry_reclaims_roots_and_holders() {
        let mut state = OpenCodeCoordinatorState::default();
        state.claims.insert(
            "ses_expired".into(),
            OpenCodeRootClaim {
                claim_id: Uuid::new_v4().to_string(),
                workspace_id: Uuid::new_v4().to_string(),
                shell_id: Uuid::new_v4().to_string(),
                run_id: Uuid::new_v4().to_string(),
                agent_id: Uuid::new_v4().to_string(),
                selected_holder_id: "holder".into(),
                holders: HashMap::from([(
                    "holder".into(),
                    OpenCodeClaimHolder {
                        expires_at: Instant::now(),
                        expires_at_ms: 0,
                    },
                )]),
            },
        );

        state.prune_claims(Instant::now());

        assert!(state.claims.is_empty());
        assert_eq!(state.holder_count(), 0);
    }

    #[test]
    fn opencode_final_claim_release_inactivates_agent_transactionally() {
        let registry = DaemonService::default();
        let (_, shell, _runtime) = running_shell(&registry);
        let generation_id = install_test_opencode_runtime(&registry);
        let run_id = shell.snapshot().unwrap().run.unwrap().id;
        let holder_id = Uuid::new_v4().to_string();
        let final_holder_id = Uuid::new_v4().to_string();
        let (claim, agent) = match registry
            .dispatch(Request::EnsureOpenCodeSessionClaim {
                generation_id: generation_id.clone(),
                holder_id: holder_id.clone(),
                root_session_id: "ses_release".into(),
                shell_id: shell.id.clone(),
                run_id: run_id.clone(),
                spec: opencode_agent_spec("ses_release", AgentState::Working),
            })
            .unwrap()
        {
            Response::OpenCodeSessionClaim { claim, agent } => (claim, agent),
            response => panic!("unexpected response: {response:?}"),
        };
        registry
            .dispatch(Request::EnsureOpenCodeSessionClaim {
                generation_id: generation_id.clone(),
                holder_id: final_holder_id.clone(),
                root_session_id: "ses_release".into(),
                shell_id: shell.id.clone(),
                run_id,
                spec: opencode_agent_spec("ses_release", AgentState::Working),
            })
            .unwrap();

        let event_id = lock(&registry.events.state).unwrap().latest_id;
        assert!(matches!(
            registry
                .dispatch(Request::ReleaseOpenCodeSessionClaim {
                    generation_id: generation_id.clone(),
                    holder_id,
                    claim_id: claim.claim_id.clone(),
                })
                .unwrap(),
            Response::OpenCodeSessionClaimReleased { released: true }
        ));
        assert_eq!(
            registry
                .durable
                .agent(&agent.id)
                .unwrap()
                .snapshot()
                .unwrap()
                .observation
                .state,
            AgentState::Working
        );
        assert_eq!(lock(&registry.events.state).unwrap().latest_id, event_id);

        registry.fail_after_mutation.store(true, Ordering::Release);
        registry
            .dispatch(Request::ReleaseOpenCodeSessionClaim {
                generation_id: generation_id.clone(),
                holder_id: final_holder_id.clone(),
                claim_id: claim.claim_id.clone(),
            })
            .unwrap_err();
        assert_eq!(
            registry
                .durable
                .agent(&agent.id)
                .unwrap()
                .snapshot()
                .unwrap()
                .observation
                .state,
            AgentState::Working
        );
        assert!(
            lock(&registry.opencode.state)
                .unwrap()
                .claims
                .contains_key("ses_release")
        );

        assert!(matches!(
            registry
                .dispatch(Request::ReleaseOpenCodeSessionClaim {
                    generation_id: generation_id.clone(),
                    holder_id: final_holder_id.clone(),
                    claim_id: claim.claim_id.clone(),
                })
                .unwrap(),
            Response::OpenCodeSessionClaimReleased { released: true }
        ));
        let inactive = registry
            .durable
            .agent(&agent.id)
            .unwrap()
            .snapshot()
            .unwrap();
        assert_eq!(inactive.observation.state, AgentState::Inactive);
        assert_eq!(
            inactive.observation.evidence,
            "OpenCode session claim released"
        );
        assert!(inactive.ended_at_ms.is_none());
        assert!(inactive.attention.is_none());
        let events = lock(&registry.events.state).unwrap();
        assert_eq!(events.latest_id, event_id + 1);
        assert!(matches!(
            events.events.back().map(|event| &event.kind),
            Some(DaemonEventKind::AgentStateChanged { agent, .. })
                if agent.id == inactive.id && agent.observation.state == AgentState::Inactive
        ));
        drop(events);

        assert!(matches!(
            registry
                .dispatch(Request::ReleaseOpenCodeSessionClaim {
                    generation_id,
                    holder_id: final_holder_id,
                    claim_id: claim.claim_id,
                })
                .unwrap(),
            Response::OpenCodeSessionClaimReleased { released: false }
        ));
        assert_eq!(
            lock(&registry.events.state).unwrap().latest_id,
            event_id + 1
        );
        registry.opencode.shutdown().unwrap();
    }

    #[test]
    fn opencode_switching_a_sole_holder_inactivates_the_previous_agent() {
        let registry = DaemonService::default();
        let (_, shell, _runtime) = running_shell(&registry);
        let generation_id = install_test_opencode_runtime(&registry);
        let run_id = shell.snapshot().unwrap().run.unwrap().id;
        let holder_id = Uuid::new_v4().to_string();
        let ensure = |root_session_id: &str| match registry
            .dispatch(Request::EnsureOpenCodeSessionClaim {
                generation_id: generation_id.clone(),
                holder_id: holder_id.clone(),
                root_session_id: root_session_id.into(),
                shell_id: shell.id.clone(),
                run_id: run_id.clone(),
                spec: opencode_agent_spec(root_session_id, AgentState::Working),
            })
            .unwrap()
        {
            Response::OpenCodeSessionClaim { agent, .. } => agent,
            response => panic!("unexpected response: {response:?}"),
        };

        let previous = ensure("ses_previous");
        let current = ensure("ses_current");

        assert_eq!(
            registry
                .durable
                .agent(&previous.id)
                .unwrap()
                .snapshot()
                .unwrap()
                .observation
                .state,
            AgentState::Inactive
        );
        assert_eq!(current.observation.state, AgentState::Working);
        registry.opencode.shutdown().unwrap();
    }

    #[test]
    fn failed_claim_ensure_leaves_ephemeral_selection_unchanged() {
        let registry = DaemonService::default();
        let (_, shell, _runtime) = running_shell(&registry);
        let generation_id = install_test_opencode_runtime(&registry);
        let run_id = match &*lock(&shell.lifecycle).unwrap() {
            ShellLifecycle::Running { run, .. } => run.id.clone(),
            _ => unreachable!(),
        };
        registry.fail_after_mutation.store(true, Ordering::Release);

        let error = registry
            .dispatch(Request::EnsureOpenCodeSessionClaim {
                generation_id,
                holder_id: Uuid::new_v4().to_string(),
                root_session_id: "ses_rollback".into(),
                shell_id: shell.id.clone(),
                run_id,
                spec: opencode_agent_spec("ses_rollback", AgentState::Working),
            })
            .unwrap_err();

        assert_eq!(error.wire_code(), ErrorCode::Internal);
        assert!(lock(&registry.opencode.state).unwrap().claims.is_empty());
        registry.opencode.shutdown().unwrap();
    }

    #[test]
    fn invalid_or_failed_claim_ensure_rolls_back_all_durable_effects() {
        let registry = DaemonService::default();
        let (workspace, shell, _runtime) = running_shell(&registry);
        let generation_id = install_test_opencode_runtime(&registry);
        let run_id = shell.snapshot().unwrap().run.unwrap().id;
        let baseline = registry.capture_persisted_state().unwrap();
        let baseline_json = serde_json::to_value(&baseline.state).unwrap();
        let baseline_revision = registry
            .workspace(&workspace.id)
            .unwrap()
            .snapshot(&registry.durable)
            .unwrap()
            .revision;
        let baseline_event = lock(&registry.events.state).unwrap().latest_id;

        for (state, inject_failure) in [(AgentState::Done, false), (AgentState::Working, true)] {
            registry
                .fail_after_mutation
                .store(inject_failure, Ordering::Release);
            let result = registry.dispatch(Request::EnsureOpenCodeSessionClaim {
                generation_id: generation_id.clone(),
                holder_id: Uuid::new_v4().to_string(),
                root_session_id: format!("ses_rollback_{state:?}"),
                shell_id: shell.id.clone(),
                run_id: run_id.clone(),
                spec: opencode_agent_spec(&format!("ses_rollback_{state:?}"), state),
            });
            assert!(result.is_err());
            assert_eq!(registry.snapshot().unwrap().workspaces[0].agents.len(), 0);
            assert_eq!(
                registry
                    .workspace(&workspace.id)
                    .unwrap()
                    .snapshot(&registry.durable)
                    .unwrap()
                    .revision,
                baseline_revision
            );
            assert_eq!(
                lock(&registry.events.state).unwrap().latest_id,
                baseline_event
            );
            assert_eq!(
                serde_json::to_value(&registry.capture_persisted_state().unwrap().state).unwrap(),
                baseline_json
            );
            assert!(lock(&registry.opencode.state).unwrap().claims.is_empty());
        }
        registry.opencode.shutdown().unwrap();
    }

    #[test]
    fn claimed_report_revalidates_run_after_waiting_for_mutation_gate() {
        let registry = Arc::new(DaemonService::default());
        let (workspace, shell, runtime) = running_shell(&registry);
        let generation_id = install_test_opencode_runtime(&registry);
        let run_id = shell.snapshot().unwrap().run.unwrap().id;
        registry
            .dispatch(Request::EnsureOpenCodeSessionClaim {
                generation_id: generation_id.clone(),
                holder_id: Uuid::new_v4().to_string(),
                root_session_id: "ses_stale_report".into(),
                shell_id: shell.id.clone(),
                run_id: run_id.clone(),
                spec: opencode_agent_spec("ses_stale_report", AgentState::Working),
            })
            .unwrap();

        let mutation = lock(&registry.mutation_lock).unwrap();
        let reporting_registry = Arc::clone(&registry);
        let report = thread::spawn(move || {
            reporting_registry.dispatch(Request::ReportClaimedOpenCodeAgent {
                generation_id,
                root_session_id: "ses_stale_report".into(),
                report: agent_report(
                    AgentState::Blocked,
                    AgentAuthority::LifecycleIntegration,
                    "must be rejected",
                ),
            })
        });
        thread::sleep(Duration::from_millis(20));
        assert!(registry.opencode.state.try_lock().is_ok());
        let replacement = Arc::new(ShellRun::new(2));
        *lock(&shell.lifecycle).unwrap() = ShellLifecycle::Running {
            profile: profile(),
            run: replacement,
            runtime,
        };
        drop(mutation);

        assert_eq!(
            report.join().unwrap().unwrap_err().wire_code(),
            ErrorCode::RunChanged
        );
        assert!(lock(&registry.opencode.state).unwrap().claims.is_empty());
        registry.opencode.shutdown().unwrap();
        shell.kill().unwrap();
        registry.close_workspace(&workspace.id).unwrap();
    }

    fn test_kiro_launcher_process(shell_id: &str, run_id: &str) -> StdChild {
        let child = Command::new("python3")
            .args(["-c", "import time; time.sleep(30)", "kiro", "launch"])
            .env("BOOMUX_SHELL_ID", shell_id)
            .env("BOOMUX_RUN_ID", run_id)
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(1);
        while Instant::now() < deadline
            && !process_has_environment(child.id(), b"BOOMUX_RUN_ID", run_id.as_bytes())
                .unwrap_or(false)
        {
            thread::sleep(Duration::from_millis(1));
        }
        child
    }

    #[test]
    fn kiro_holder_acquire_revalidates_run_inside_the_mutation_gate() {
        let registry = Arc::new(DaemonService::default());
        let (workspace, shell, runtime) = running_shell(&registry);
        let run_id = shell.snapshot().unwrap().run.unwrap().id;
        let mut holder_process = test_kiro_launcher_process(&shell.id, &run_id);
        let mutation = lock(&registry.mutation_lock).unwrap();
        let acquiring = Arc::clone(&registry);
        let shell_id = shell.id.clone();
        let process_id = holder_process.id();
        let acquire = thread::spawn(move || {
            acquiring.dispatch(Request::AcquireKiroLaunchHolder {
                pid: process_id,
                shell_id,
                run_id,
            })
        });
        thread::sleep(Duration::from_millis(20));
        *lock(&shell.lifecycle).unwrap() = ShellLifecycle::Running {
            profile: profile(),
            run: Arc::new(ShellRun::new(2)),
            runtime,
        };
        drop(mutation);

        assert_eq!(
            acquire.join().unwrap().unwrap_err().wire_code(),
            ErrorCode::RunChanged
        );
        assert!(lock(&registry.kiro.state).unwrap().is_empty());
        holder_process.kill().unwrap();
        holder_process.wait().unwrap();
        shell.kill().unwrap();
        registry.close_workspace(&workspace.id).unwrap();
    }

    #[test]
    fn kiro_handoff_import_rejects_a_noncurrent_shell_run() {
        let registry = DaemonService::default();
        let (workspace, shell, runtime) = running_shell(&registry);
        let run_id = shell.snapshot().unwrap().run.unwrap().id;
        let (holder_id, mut process) = test_kiro_holder(&registry, &shell.id, &run_id);
        report_test_kiro(&registry, &holder_id, "handoff-session");
        let transferred = registry.export_kiro_launch_holders().unwrap();
        lock(&registry.kiro.state).unwrap().clear();
        *lock(&shell.lifecycle).unwrap() = ShellLifecycle::Running {
            profile: profile(),
            run: Arc::new(ShellRun::new(2)),
            runtime,
        };

        assert!(registry.import_kiro_launch_holders(transferred).is_err());
        assert!(lock(&registry.kiro.state).unwrap().is_empty());
        process.kill().unwrap();
        process.wait().unwrap();
        shell.kill().unwrap();
        registry.close_workspace(&workspace.id).unwrap();
    }

    fn install_test_controller(
        runtime: &ShellRuntime,
        token: &str,
    ) -> mpsc::Receiver<ControllerOutput> {
        let (connection, _peer) = UnixStream::pair().unwrap();
        let (output, receiver) = mpsc::sync_channel(1);
        *lock(&runtime.controller).unwrap() = Some(Controller {
            token: token.into(),
            output,
            connection,
            reconnect_ack: None,
        });
        receiver
    }

    fn install_test_collaborator(
        runtime: &ShellRuntime,
        token: &str,
        capacity: usize,
    ) -> mpsc::Receiver<ControllerOutput> {
        let (connection, _peer) = UnixStream::pair().unwrap();
        let (output, receiver) = mpsc::sync_channel(capacity);
        lock(&runtime.collaborators).unwrap().insert(
            token.into(),
            Controller {
                token: token.into(),
                output,
                connection,
                reconnect_ack: None,
            },
        );
        receiver
    }

    #[test]
    fn collaborative_participants_fan_out_release_and_preserve_primary_authority() {
        let registry = DaemonService::default();
        let (workspace, shell, runtime) = running_shell(&registry);
        let _primary = install_test_controller(&runtime, "primary");
        let collaborator = install_test_collaborator(&runtime, "collaborator", 2);

        assert!(ShellRuntimeManager::participant_is_authorized(&runtime, "primary").unwrap());
        assert!(ShellRuntimeManager::participant_is_authorized(&runtime, "collaborator").unwrap());
        assert!(ShellRuntimeManager::participant_is_primary(&runtime, "primary").unwrap());
        assert!(!ShellRuntimeManager::participant_is_primary(&runtime, "collaborator").unwrap());

        ShellRuntimeManager::fanout_output(&runtime, b"shared-output");
        assert!(matches!(
            collaborator.recv().unwrap(),
            ControllerOutput::Data(bytes) if bytes == b"shared-output"
        ));
        ShellRuntimeManager::release_controller(&runtime, "collaborator").unwrap();
        assert!(lock(&runtime.collaborators).unwrap().is_empty());
        assert!(lock(&runtime.controller).unwrap().is_some());

        shell.kill().unwrap();
        registry.close_workspace(&workspace.id).unwrap();
    }

    #[test]
    fn primary_resize_fans_out_to_collaborators_only() {
        let registry = DaemonService::default();
        let (workspace, shell, runtime) = running_shell(&registry);
        let primary = install_test_controller(&runtime, "primary");
        let collaborator = install_test_collaborator(&runtime, "collaborator", 1);
        let size = PtySize {
            rows: 30,
            cols: 100,
            pixel_width: 1_000,
            pixel_height: 600,
        };

        ShellRuntimeManager::fanout_collaborator_resize(&runtime, size);

        assert!(matches!(
            collaborator.recv().unwrap(),
            ControllerOutput::Resize {
                rows: 30,
                cols: 100,
                pixel_width: 1_000,
                pixel_height: 600,
            }
        ));
        assert!(matches!(primary.try_recv(), Err(mpsc::TryRecvError::Empty)));
        shell.kill().unwrap();
        registry.close_workspace(&workspace.id).unwrap();
    }

    #[test]
    fn slow_collaborator_is_removed_without_displacing_primary() {
        let registry = DaemonService::default();
        let (workspace, shell, runtime) = running_shell(&registry);
        let _primary = install_test_controller(&runtime, "primary");
        let collaborator = install_test_collaborator(&runtime, "slow", 1);
        lock(&runtime.collaborators)
            .unwrap()
            .get("slow")
            .unwrap()
            .output
            .try_send(ControllerOutput::Data(b"queued".to_vec()))
            .unwrap();

        ShellRuntimeManager::fanout_output(&runtime, b"new-output");

        assert!(lock(&runtime.collaborators).unwrap().is_empty());
        assert!(lock(&runtime.controller).unwrap().is_some());
        assert!(matches!(
            collaborator.recv().unwrap(),
            ControllerOutput::Data(bytes) if bytes == b"queued"
        ));
        shell.kill().unwrap();
        registry.close_workspace(&workspace.id).unwrap();
    }

    #[test]
    fn primary_output_applies_backpressure_instead_of_disconnect() {
        let registry = DaemonService::default();
        let (workspace, shell, runtime) = running_shell(&registry);
        let primary = install_test_controller(&runtime, "primary");
        lock(&runtime.controller)
            .unwrap()
            .as_ref()
            .unwrap()
            .output
            .send(ControllerOutput::Data(b"queued".to_vec()))
            .unwrap();
        let (started_sender, started_receiver) = mpsc::sync_channel(0);
        let (completed_sender, completed_receiver) = mpsc::sync_channel(0);
        let runtime_for_output = Arc::clone(&runtime);
        let output = thread::spawn(move || {
            started_sender.send(()).unwrap();
            ShellRuntimeManager::fanout_output(&runtime_for_output, b"next");
            completed_sender.send(()).unwrap();
        });

        started_receiver.recv().unwrap();
        assert!(matches!(
            completed_receiver.recv_timeout(Duration::from_millis(100)),
            Err(mpsc::RecvTimeoutError::Timeout)
        ));
        assert!(matches!(
            primary.recv().unwrap(),
            ControllerOutput::Data(bytes) if bytes == b"queued"
        ));
        completed_receiver
            .recv_timeout(Duration::from_secs(1))
            .unwrap();
        output.join().unwrap();
        assert!(matches!(
            primary.recv().unwrap(),
            ControllerOutput::Data(bytes) if bytes == b"next"
        ));
        assert!(lock(&runtime.controller).unwrap().is_some());

        shell.kill().unwrap();
        registry.close_workspace(&workspace.id).unwrap();
    }

    #[test]
    fn disconnected_primary_unblocks_backpressured_output() {
        let registry = DaemonService::default();
        let (workspace, shell, runtime) = running_shell(&registry);
        let primary = install_test_controller(&runtime, "primary");
        lock(&runtime.controller)
            .unwrap()
            .as_ref()
            .unwrap()
            .output
            .send(ControllerOutput::Data(b"queued".to_vec()))
            .unwrap();
        let runtime_for_output = Arc::clone(&runtime);
        let output =
            thread::spawn(move || ShellRuntimeManager::fanout_output(&runtime_for_output, b"next"));

        drop(primary);
        output.join().unwrap();
        assert!(lock(&runtime.controller).unwrap().is_none());

        shell.kill().unwrap();
        registry.close_workspace(&workspace.id).unwrap();
    }

    #[test]
    fn exclusive_takeover_detaches_all_collaborators() {
        let registry = DaemonService::default();
        let (workspace, shell, runtime) = running_shell(&registry);
        let _primary = install_test_controller(&runtime, "primary");
        let first = install_test_collaborator(&runtime, "first", 1);
        let second = install_test_collaborator(&runtime, "second", 1);

        ShellRuntimeManager::displace_collaborators(&runtime).unwrap();

        assert!(first.recv().is_err());
        assert!(second.recv().is_err());
        assert!(lock(&runtime.collaborators).unwrap().is_empty());
        assert!(lock(&runtime.controller).unwrap().is_some());
        shell.kill().unwrap();
        registry.close_workspace(&workspace.id).unwrap();
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
    fn exact_workspace_shell_creation_is_atomic_idempotent_and_collision_safe() {
        let registry = DaemonService::default();
        let workspace_id = Uuid::from_u128(1).to_string();
        let shell_id = Uuid::from_u128(2).to_string();
        let cwd = env::temp_dir();
        let request = Request::CreateWorkspaceShell {
            workspace_id: workspace_id.clone(),
            workspace_name: "coordinated".into(),
            default_cwd: Some(cwd.clone()),
            shell_id: shell_id.clone(),
            shell: ShellSpec {
                name: "shell".into(),
                command: vec!["bash".into(), "-lc".into(), "printf %s safe".into()],
                cwd: cwd.clone(),
            },
        };
        assert!(matches!(
            registry.dispatch(request.clone()).unwrap(),
            Response::Shell { .. }
        ));
        assert!(matches!(
            registry.dispatch(request).unwrap(),
            Response::Shell { .. }
        ));
        let snapshot = registry.snapshot().unwrap();
        assert_eq!(snapshot.workspaces.len(), 1);
        assert_eq!(snapshot.workspaces[0].id, workspace_id);
        assert_eq!(snapshot.workspaces[0].shells.len(), 1);
        assert_eq!(snapshot.workspaces[0].shells[0].id, shell_id);

        let conflict = registry.dispatch(Request::CreateWorkspaceShell {
            workspace_id: snapshot.workspaces[0].id.clone(),
            workspace_name: "coordinated".into(),
            default_cwd: Some(cwd.clone()),
            shell_id: snapshot.workspaces[0].shells[0].id.clone(),
            shell: ShellSpec {
                name: "different".into(),
                command: vec!["bash".into()],
                cwd,
            },
        });
        assert!(conflict.is_err());
        assert_eq!(registry.snapshot().unwrap().workspaces[0].shells.len(), 1);
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

        let _ = install_test_controller(&runtime, "controller");
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
    fn presented_focus_revisions_order_local_and_remote_nodes() {
        let runtimes = ShellRuntimeManager::default();

        runtimes
            .record_presented_focus("local-node".into(), "local-shell".into())
            .unwrap();
        let local_revision = runtimes
            .presented_focused_terminal()
            .unwrap()
            .unwrap()
            .revision;
        runtimes
            .record_presented_focus("remote-node".into(), "remote-shell".into())
            .unwrap();

        let remote = runtimes.presented_focused_terminal().unwrap().unwrap();
        assert!(remote.revision > local_revision);
        assert_eq!(
            remote.shell,
            QualifiedIdentity::new("remote-node", "remote-shell")
        );
    }

    #[test]
    fn focus_reports_from_a_replaced_controller_are_ignored() {
        let registry = DaemonService::default();
        let (_workspace, shell, runtime) = running_shell(&registry);
        let run = match &*lock(&shell.lifecycle).unwrap() {
            ShellLifecycle::Running { run, .. } => Arc::clone(run),
            _ => panic!("expected running shell"),
        };

        let _ = install_test_controller(&runtime, "old-controller");
        assert!(
            registry
                .record_focus_gained(18, &shell, &run, &runtime, "old-controller")
                .unwrap()
        );
        let _ = install_test_controller(&runtime, "current-controller");
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
    fn focus_reports_accept_current_collaborators() {
        let registry = DaemonService::default();
        let (workspace, shell, runtime) = running_shell(&registry);
        let run = match &*lock(&shell.lifecycle).unwrap() {
            ShellLifecycle::Running { run, .. } => Arc::clone(run),
            _ => panic!("expected running shell"),
        };
        let _receiver = install_test_collaborator(&runtime, "collaborator", 1);

        assert!(
            registry
                .record_focus_gained(44, &shell, &run, &runtime, "collaborator")
                .unwrap()
        );
        assert_eq!(
            registry
                .snapshot()
                .unwrap()
                .focused_terminal
                .unwrap()
                .shell_id,
            shell.id
        );

        shell.kill().unwrap();
        registry.close_workspace(&workspace.id).unwrap();
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
    fn blocked_publication_coalesces_snapshot_invalidations() {
        let mut transition = TransitionState {
            persistence_in_flight: true,
            ..TransitionState::default()
        };
        for generation in 1..=10_000 {
            transition.queue_runtime_event(DaemonEventKind::NodeProjectionChanged {
                node_id: "node-1".into(),
                cache_generation: generation,
            });
            transition.queue_runtime_event(DaemonEventKind::FocusedTerminalPresentationChanged);
        }
        transition.queue_runtime_event(DaemonEventKind::NodeProjectionChanged {
            node_id: "node-2".into(),
            cache_generation: 4,
        });

        assert_eq!(transition.pending_runtime_events.len(), 3);
        assert!(
            transition
                .pending_runtime_events
                .iter()
                .any(|event| matches!(
                    event,
                    DaemonEventKind::NodeProjectionChanged {
                        node_id,
                        cache_generation: 10_000,
                    } if node_id == "node-1"
                ))
        );
        assert_eq!(
            transition
                .pending_runtime_events
                .iter()
                .filter(|event| matches!(
                    event,
                    DaemonEventKind::FocusedTerminalPresentationChanged
                ))
                .count(),
            1
        );
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
        assert_eq!(
            proc_process_group("123 (shell name) S 1 123 123 34826 456 0"),
            Some(123)
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
    fn listener_ownership_requires_the_exact_process_session() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let session = unsafe { libc::getsid(0) } as u32;

        assert!(opencode_listener_belongs_to_session(port, session));
        assert!(!opencode_listener_belongs_to_session(port, u32::MAX));
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
    fn normalizes_and_bounds_session_display_name_metadata() {
        assert_eq!(
            normalize_session_display_name("  Checkout   retry  ").unwrap(),
            "Checkout retry"
        );
        assert!(
            normalize_session_display_name(&"x".repeat(MAX_SESSION_DISPLAY_NAME_CHARS + 1))
                .is_err()
        );
        assert!(normalize_session_display_name("forged\nrow").is_err());

        let record = PersistedSessionDisplayName {
            integration: "opencode".into(),
            session: PersistedSessionIdentity::External {
                external_session_id: "external".into(),
            },
            display_name: "Valid name".into(),
        };
        let mut workspace = PersistedWorkspace {
            id: Uuid::new_v4().to_string(),
            revision: 1,
            name: "work".into(),
            default_cwd: None,
            shells: Vec::new(),
            launchers: Vec::new(),
            agents: Vec::new(),
            session_display_names: vec![record.clone()],
            session_display_name_operations: Vec::new(),
            hidden_sessions: Vec::new(),
            session_hide_operations: Vec::new(),
        };
        assert!(validate_persisted_session_display_names(&workspace).is_ok());
        workspace.session_display_names[0].display_name = "not  normalized".into();
        assert_eq!(
            validate_persisted_session_display_names(&workspace)
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidData
        );
        workspace.session_display_names = vec![record; MAX_SESSION_DISPLAY_NAMES_PER_WORKSPACE + 1];
        assert_eq!(
            validate_persisted_session_display_names(&workspace)
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidData
        );
        workspace.session_display_names.clear();
        let hidden = PersistedHiddenSession {
            session_id: Uuid::new_v4().to_string(),
            integration: "opencode".into(),
            session: PersistedSessionIdentity::External {
                external_session_id: "external".into(),
            },
        };
        workspace.hidden_sessions = vec![hidden; MAX_HIDDEN_SESSIONS_PER_WORKSPACE + 1];
        assert_eq!(
            validate_persisted_session_display_names(&workspace)
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidData
        );
    }

    #[test]
    fn hidden_sessions_are_filtered_before_the_list_limit() {
        let sessions = (0..=MAX_HOST_SERVICE_SESSIONS)
            .map(|index| crate::session_projection::SessionProjection {
                id: format!("session-{index}"),
                workspace_id: "workspace".into(),
                workspace_name: "work".into(),
                integration: "opencode".into(),
                external_session_id: Some(format!("external-{index}")),
                description: format!("Session {index}"),
                user_display_name: None,
                workspace_revision: 1,
                state: AgentState::Inactive,
                state_is_current: false,
                started_at_ms: 1,
                last_at_ms: 1,
                source_cwd: None,
                occurrences: Vec::new(),
            })
            .collect::<Vec<_>>();
        let hidden = [crate::session_projection::HiddenSessionMetadata {
            workspace_id: "workspace".into(),
            integration: "opencode".into(),
            external_session_id: Some("external-0".into()),
            agent_id: None,
        }];

        let mut current = sessions.clone();
        apply_session_visibility_limit(&mut current, &hidden, 51);
        assert_eq!(current.len(), MAX_HOST_SERVICE_SESSIONS);
        assert_eq!(current[0].id, "session-1");
        assert_eq!(current.last().unwrap().id, "session-1000");

        let mut old = sessions;
        apply_session_visibility_limit(&mut old, &hidden, 50);
        assert_eq!(old.len(), MAX_HOST_SERVICE_SESSIONS);
        assert_eq!(old[0].id, "session-0");
        assert_eq!(old.last().unwrap().id, "session-999");
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
    fn protocol_forty_nine_filters_session_display_name_fields_and_events() {
        let summary: crate::protocol::HostAgentSessionSummary =
            serde_json::from_value(serde_json::json!({
                "id": "session", "workspace_id": "workspace",
                "workspace_name": "work", "description": "User name",
                "user_display_name": "User name", "workspace_revision": 3,
                "integration": "opencode", "external_session_id": "external",
                "state": "inactive", "state_is_current": false,
                "started_at_ms": 1, "last_at_ms": 2, "occurrence_count": 1,
                "attentions": [{
                    "agent_id": "agent", "reason": "completed",
                    "observation_revision": 3, "observed_at_ms": 2
                }],
                "git_branch": "feat/session-radar",
                "working_contexts": [{
                    "repository": "boomux", "branch": "feat/session-radar",
                    "observed_at_ms": 3
                }],
                "working_context_count": 1
            }))
            .unwrap();
        let cursor = EventCursor {
            stream_id: Uuid::new_v4().to_string(),
            event_id: 9,
        };
        let response = response_for_version(
            Response::Events {
                stream_id: cursor.stream_id.clone(),
                cursor: cursor.clone(),
                snapshot: None,
                events: vec![DaemonEvent {
                    id: 9,
                    at_ms: 1,
                    kind: DaemonEventKind::AgentSessionDisplayNameChanged {
                        workspace_id: "workspace".into(),
                        session_id: "session".into(),
                        user_display_name: Some("User name".into()),
                        workspace_revision: 3,
                    },
                }],
            },
            49,
        );
        let Response::Events {
            cursor: filtered,
            events,
            ..
        } = response
        else {
            panic!("expected events response");
        };
        assert_eq!(filtered, cursor);
        assert!(events.is_empty());

        let response = response_for_version(
            Response::HostService {
                result: HostServiceResult::AgentSessions {
                    sessions: vec![summary.clone()],
                },
            },
            49,
        );
        let Response::HostService {
            result: HostServiceResult::AgentSessions { sessions },
        } = response
        else {
            panic!("expected Session list response");
        };
        assert!(sessions[0].user_display_name.is_none());
        assert_eq!(sessions[0].workspace_revision, 0);
        assert_eq!(sessions[0].description, "User name");
        assert!(sessions[0].attentions.is_empty());
        assert!(sessions[0].git_branch.is_none());
        assert!(sessions[0].working_contexts.is_empty());
        assert_eq!(sessions[0].working_context_count, 0);

        let response = response_for_version(
            Response::HostService {
                result: HostServiceResult::AgentSession {
                    session: crate::protocol::HostAgentSessionInspection {
                        summary,
                        source_cwd: None,
                        occurrences: Vec::new(),
                        projected_occurrences: vec![crate::protocol::HostAgentSessionOccurrence {
                            agent_id: Uuid::new_v4().to_string(),
                            shell_id: Uuid::new_v4().to_string(),
                            retained_shell_name: None,
                            retained_shell_cwd: None,
                            source_cwd: Some("/tmp/project".into()),
                            run_id: Uuid::new_v4().to_string(),
                            started_at_ms: 1,
                            ended_at_ms: None,
                            is_current: false,
                            observation: AgentObservationSnapshot {
                                revision: 1,
                                state: AgentState::Inactive,
                                authority: AgentAuthority::LifecycleIntegration,
                                evidence: "inactive".into(),
                                confidence: 100,
                                observed_at_ms: 2,
                            },
                        }],
                    },
                },
            },
            49,
        );
        let Response::HostService {
            result: HostServiceResult::AgentSession { session },
        } = response
        else {
            panic!("expected Session inspection response");
        };
        assert!(session.projected_occurrences.is_empty());
        assert!(session.summary.user_display_name.is_none());
        assert_eq!(session.summary.workspace_revision, 0);
        assert!(session.summary.attentions.is_empty());
        assert!(session.summary.git_branch.is_none());
        assert!(session.summary.working_contexts.is_empty());
        assert_eq!(session.summary.working_context_count, 0);
    }

    #[test]
    fn protocol_fifty_strips_session_response_time_git_status_from_all_host_shapes() {
        let summary: crate::protocol::HostAgentSessionSummary =
            serde_json::from_value(serde_json::json!({
                "id": "session", "workspace_id": "workspace",
                "workspace_name": "work", "description": "Session",
                "integration": "opencode", "external_session_id": "external",
                "state": "inactive", "state_is_current": false,
                "started_at_ms": 1, "last_at_ms": 2, "occurrence_count": 1,
                "working_contexts": [{
                    "repository": "boomux", "branch": "feat/session-radar",
                    "observed_at_ms": 3,
                    "push_status": { "status": "ahead", "commit_count": 2 },
                    "worktree_status": {
                        "staged": true,
                        "unstaged_or_untracked": true
                    }
                }],
                "working_context_count": 1
            }))
            .unwrap();
        let responses = [
            Response::HostService {
                result: HostServiceResult::AgentSessions {
                    sessions: vec![summary.clone()],
                },
            },
            Response::HostService {
                result: HostServiceResult::AgentSession {
                    session: crate::protocol::HostAgentSessionInspection {
                        summary: summary.clone(),
                        source_cwd: None,
                        occurrences: Vec::new(),
                        projected_occurrences: Vec::new(),
                    },
                },
            },
            Response::HostService {
                result: HostServiceResult::ResolvedAgentSession { session: summary },
            },
        ];

        for response in responses {
            for (version, retained) in [(51, true), (50, false)] {
                let response = response_for_version(response.clone(), version);
                let context = match &response {
                    Response::HostService {
                        result: HostServiceResult::AgentSessions { sessions },
                    } => &sessions[0].working_contexts[0],
                    Response::HostService {
                        result: HostServiceResult::AgentSession { session },
                    } => &session.summary.working_contexts[0],
                    Response::HostService {
                        result: HostServiceResult::ResolvedAgentSession { session },
                    } => &session.working_contexts[0],
                    _ => panic!("expected Agent Session host-service response"),
                };
                assert_eq!(context.push_status.is_some(), retained);
                assert_eq!(context.worktree_status.is_some(), retained);
            }
        }
    }

    #[test]
    fn protocol_fifty_filters_session_hide_events_without_rewinding_cursor() {
        let cursor = EventCursor {
            stream_id: Uuid::new_v4().to_string(),
            event_id: 9,
        };
        let response = Response::Events {
            stream_id: cursor.stream_id.clone(),
            cursor: cursor.clone(),
            snapshot: None,
            events: vec![DaemonEvent {
                id: 9,
                at_ms: 1,
                kind: DaemonEventKind::AgentSessionHidden {
                    workspace_id: Uuid::from_u128(1).to_string(),
                    session_id: Uuid::from_u128(2).to_string(),
                    workspace_revision: 3,
                },
            }],
        };

        let Response::Events {
            cursor: current_cursor,
            events: current_events,
            ..
        } = response_for_version(response.clone(), 51)
        else {
            panic!("expected current events response");
        };
        assert_eq!(current_cursor, cursor);
        assert_eq!(current_events.len(), 1);

        let Response::Events {
            cursor: old_cursor,
            events: old_events,
            ..
        } = response_for_version(response, 50)
        else {
            panic!("expected old events response");
        };
        assert_eq!(old_cursor, cursor);
        assert!(old_events.is_empty());
    }

    #[test]
    fn protocol_thirty_eight_filters_focus_invalidation_without_rewinding_cursor() {
        let cursor = EventCursor {
            stream_id: Uuid::new_v4().to_string(),
            event_id: 5,
        };
        let response = Response::Events {
            stream_id: cursor.stream_id.clone(),
            cursor: cursor.clone(),
            snapshot: None,
            events: vec![DaemonEvent {
                id: 5,
                at_ms: 10,
                kind: DaemonEventKind::FocusedTerminalPresentationChanged,
            }],
        };

        let Response::Events {
            cursor: current_cursor,
            events: current_events,
            ..
        } = response_for_version(response.clone(), 39)
        else {
            panic!("expected current events");
        };
        assert_eq!(current_cursor, cursor);
        assert!(matches!(
            current_events.as_slice(),
            [DaemonEvent {
                kind: DaemonEventKind::FocusedTerminalPresentationChanged,
                ..
            }]
        ));

        let Response::Events {
            cursor: filtered_cursor,
            events,
            ..
        } = response_for_version(response, 38)
        else {
            panic!("expected filtered events");
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
    fn projection_cut_reseeds_only_beyond_transition_limit() {
        let events = EventStream::new();
        let baseline = events.transaction().unwrap().cursor();
        for index in 0..=protocol::MAX_NODE_PROJECTION_TRANSITIONS {
            events
                .publish(DaemonEventKind::WorkspaceClosed {
                    workspace_id: format!("workspace-{index}"),
                })
                .unwrap();
        }
        let transaction = events.transaction().unwrap();
        let through = transaction.cursor();

        let (mode, transitions) =
            projection_transitions(&transaction.events, Some(&baseline), &through);
        assert_eq!(mode, NodeProjectionSyncMode::Baseline);
        assert!(transitions.is_empty());

        let after_first = EventCursor {
            stream_id: baseline.stream_id,
            event_id: baseline.event_id + 1,
        };
        let (mode, transitions) =
            projection_transitions(&transaction.events, Some(&after_first), &through);
        assert_eq!(mode, NodeProjectionSyncMode::Resumed);
        assert_eq!(
            transitions.len(),
            usize::from(protocol::MAX_NODE_PROJECTION_TRANSITIONS)
        );
    }

    fn remote_notification_test_settings() -> NotificationDeliverySettings {
        NotificationDeliverySettings {
            desktop: NotificationSettings {
                enabled: true,
                blocked: true,
                completed: true,
            },
            ..Default::default()
        }
    }

    fn remote_notification_projection(node_id: &str) -> NodeProjectionSnapshot {
        NodeProjectionSnapshot {
            node_id: node_id.into(),
            workspaces: vec![NodeProjectionWorkspace {
                id: "workspace-1".into(),
                name: "project".into(),
                item_count: 4,
                attention_count: 2,
            }],
            shells: vec![NodeProjectionShell {
                id: "shell-1".into(),
                workspace_id: "workspace-1".into(),
                name: "agent-shell".into(),
                status: ShellStatus::Running,
                run_id: Some("run-1".into()),
                generation: Some(1),
                started_at_ms: Some(1),
                ended_at_ms: None,
                recovered_agent_id: None,
            }],
            launchers: Vec::new(),
            agents: vec![
                NodeProjectionAgent {
                    id: "agent-blocked".into(),
                    workspace_id: "workspace-1".into(),
                    shell_id: "shell-1".into(),
                    run_id: "run-1".into(),
                    name: "blocked-agent".into(),
                    integration: "test".into(),
                    state: AgentState::Blocked,
                    observation_revision: 2,
                    observed_at_ms: 2,
                    started_at_ms: 1,
                    ended_at_ms: None,
                    attention: Some(NodeProjectionAttention {
                        reason: AgentAttentionReason::Blocked,
                        observation_revision: 2,
                        observed_at_ms: 2,
                    }),
                },
                NodeProjectionAgent {
                    id: "agent-done".into(),
                    workspace_id: "workspace-1".into(),
                    shell_id: "shell-1".into(),
                    run_id: "run-1".into(),
                    name: "done-agent".into(),
                    integration: "test".into(),
                    state: AgentState::Done,
                    observation_revision: 4,
                    observed_at_ms: 4,
                    started_at_ms: 1,
                    ended_at_ms: Some(4),
                    attention: Some(NodeProjectionAttention {
                        reason: AgentAttentionReason::Completed,
                        observation_revision: 4,
                        observed_at_ms: 4,
                    }),
                },
            ],
        }
    }

    #[test]
    fn reduced_remote_transitions_classify_live_attention_and_one_reconnect_digest() {
        let node_id = Uuid::from_u128(2).to_string();
        let stream_id = Uuid::from_u128(3).to_string();
        let registration = crate::protocol::NodeRegistrationSnapshot {
            alias: "work".into(),
            target: "work.example".into(),
            node_id: node_id.clone(),
            revision: 1,
            tombstone_epoch: 0,
        };
        let sync = NodeProjectionSync {
            mode: NodeProjectionSyncMode::Resumed,
            cursor: EventCursor {
                stream_id: stream_id.clone(),
                event_id: 13,
            },
            projection: remote_notification_projection(&node_id),
            transitions: vec![
                NodeProjectionTransition {
                    event_id: 11,
                    at_ms: 11,
                    kind: NodeProjectionTransitionKind::Agent {
                        workspace_id: "workspace-1".into(),
                        agent_id: "agent-blocked".into(),
                        revision: 2,
                    },
                },
                NodeProjectionTransition {
                    event_id: 12,
                    at_ms: 12,
                    kind: NodeProjectionTransitionKind::Agent {
                        workspace_id: "workspace-1".into(),
                        agent_id: "agent-done".into(),
                        revision: 4,
                    },
                },
            ],
            capabilities: Vec::new(),
        };
        let live = ProjectionCommit {
            generation: 2,
            previous_health: Some(crate::protocol::NodeProjectionHealthCode::Online),
            previous_cursor: Some(EventCursor {
                stream_id: stream_id.clone(),
                event_id: 10,
            }),
        };
        let (requests, digest) = remote_notification_candidates(
            &registration,
            &sync,
            &live,
            &remote_notification_test_settings(),
        );
        assert_eq!(requests.len(), 2);
        assert!(digest.is_none());
        assert_eq!(requests[0].request.reason, NotificationReason::Blocked);
        assert_eq!(requests[1].request.reason, NotificationReason::Completed);
        assert_eq!(requests[0].request.node.as_ref().unwrap().alias, "work");

        let reconnect = ProjectionCommit {
            previous_health: Some(crate::protocol::NodeProjectionHealthCode::Stale),
            ..live
        };
        let (requests, digest) = remote_notification_candidates(
            &registration,
            &sync,
            &reconnect,
            &remote_notification_test_settings(),
        );
        assert!(requests.is_empty());
        let digest = digest.unwrap();
        assert_eq!(digest.claim.prior_cursor, 10);
        assert_eq!(digest.claim.through_cursor, 13);
        assert_eq!(digest.request.digest.as_ref().unwrap().blocked, 1);
        assert_eq!(digest.request.digest.as_ref().unwrap().completed, 1);
    }

    #[test]
    fn baseline_and_stale_reduced_revisions_do_not_notify() {
        let node_id = Uuid::from_u128(2).to_string();
        let stream_id = Uuid::from_u128(3).to_string();
        let registration = crate::protocol::NodeRegistrationSnapshot {
            alias: "work".into(),
            target: "work.example".into(),
            node_id: node_id.clone(),
            revision: 1,
            tombstone_epoch: 0,
        };
        let mut sync = NodeProjectionSync {
            mode: NodeProjectionSyncMode::Baseline,
            cursor: EventCursor {
                stream_id: stream_id.clone(),
                event_id: 8,
            },
            projection: remote_notification_projection(&node_id),
            transitions: Vec::new(),
            capabilities: Vec::new(),
        };
        let commit = ProjectionCommit {
            generation: 2,
            previous_health: Some(crate::protocol::NodeProjectionHealthCode::Stale),
            previous_cursor: Some(EventCursor {
                stream_id,
                event_id: 7,
            }),
        };
        let result = remote_notification_candidates(
            &registration,
            &sync,
            &commit,
            &remote_notification_test_settings(),
        );
        assert!(result.0.is_empty() && result.1.is_none());

        sync.mode = NodeProjectionSyncMode::Resumed;
        sync.transitions.push(NodeProjectionTransition {
            event_id: 8,
            at_ms: 8,
            kind: NodeProjectionTransitionKind::Agent {
                workspace_id: "workspace-1".into(),
                agent_id: "agent-blocked".into(),
                revision: 1,
            },
        });
        let result = remote_notification_candidates(
            &registration,
            &sync,
            &commit,
            &remote_notification_test_settings(),
        );
        assert!(result.0.is_empty() && result.1.is_none());
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
    fn agent_working_contexts_are_exact_deduplicated_and_bounded() {
        let directory = env::temp_dir().join(format!("boomux-agent-context-{}", Uuid::new_v4()));
        let repository = directory.join("boomux");
        fs::create_dir_all(&repository).unwrap();
        assert!(
            Command::new("git")
                .args(["init", "-q", "-b", "feat/working-contexts"])
                .arg(&repository)
                .status()
                .unwrap()
                .success()
        );
        let registry = DaemonService::default();
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
            panic!("expected registered Agent");
        };

        assert_eq!(
            registry
                .dispatch(Request::ObserveAgentWorkingContext {
                    agent_id: agent.id.clone(),
                    shell_id: shell.id.clone(),
                    run_id: Uuid::new_v4().to_string(),
                    path: repository.clone(),
                })
                .unwrap_err()
                .wire_code(),
            ErrorCode::RunChanged
        );
        let event_id = lock(&registry.events.state).unwrap().latest_id;
        let Response::AgentWorkingContext {
            agent: observed,
            changed,
        } = registry
            .dispatch(Request::ObserveAgentWorkingContext {
                agent_id: agent.id.clone(),
                shell_id: shell.id.clone(),
                run_id: run_id.clone(),
                path: repository.clone(),
            })
            .unwrap()
        else {
            panic!("expected working context");
        };
        assert!(changed);
        assert_eq!(observed.working_contexts.len(), 1);
        assert_eq!(observed.working_contexts[0].repository, "boomux");
        assert_eq!(observed.working_contexts[0].branch, "feat/working-contexts");
        assert!(matches!(
            lock(&registry.events.state)
                .unwrap()
                .events
                .back()
                .unwrap()
                .kind,
            DaemonEventKind::AgentWorkingContextObserved { .. }
        ));

        let Response::AgentWorkingContext { changed, .. } = registry
            .dispatch(Request::ObserveAgentWorkingContext {
                agent_id: agent.id.clone(),
                shell_id: shell.id.clone(),
                run_id: run_id.clone(),
                path: repository,
            })
            .unwrap()
        else {
            panic!("expected duplicate working context");
        };
        assert!(!changed);
        assert_eq!(
            lock(&registry.events.state).unwrap().latest_id,
            event_id + 1
        );

        for index in 0..=MAX_AGENT_WORKING_CONTEXTS {
            registry
                .durable
                .observe_agent_working_context(
                    &agent.id,
                    &shell.id,
                    &run_id,
                    AgentWorkingContextSnapshot {
                        worktree_root: format!("/worktrees/repository-{index}").into(),
                        repository: format!("repository-{index}"),
                        branch: "main".into(),
                        observed_at_ms: 0,
                    },
                )
                .unwrap();
        }
        let bounded = registry.agent(&agent.id).unwrap().snapshot().unwrap();
        assert_eq!(bounded.working_contexts.len(), MAX_AGENT_WORKING_CONTEXTS);
        assert_eq!(bounded.working_contexts[0].repository, "repository-8");
        assert!(
            bounded
                .working_contexts
                .iter()
                .all(|context| context.repository != "repository-0")
        );

        shell.kill().unwrap();
        registry.close_workspace(&workspace.id).unwrap();
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn failed_working_context_persistence_rolls_back_without_event() {
        let directory =
            env::temp_dir().join(format!("boomux-agent-context-undo-{}", Uuid::new_v4()));
        let repository = directory.join("boomux");
        fs::create_dir_all(&repository).unwrap();
        assert!(
            Command::new("git")
                .args(["init", "-q", "-b", "feat/working-contexts"])
                .arg(&repository)
                .status()
                .unwrap()
                .success()
        );
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
            panic!("expected registered Agent");
        };
        let event_id = lock(&registry.events.state).unwrap().latest_id;
        registry.fail_next_persistence();

        let error = registry
            .dispatch(Request::ObserveAgentWorkingContext {
                agent_id: agent.id.clone(),
                shell_id: shell.id.clone(),
                run_id: run_id.clone(),
                path: repository.clone(),
            })
            .unwrap_err();

        assert_eq!(error.wire_code(), ErrorCode::PersistenceFailed);
        assert!(
            registry
                .agent(&agent.id)
                .unwrap()
                .snapshot()
                .unwrap()
                .working_contexts
                .is_empty()
        );
        assert_eq!(lock(&registry.events.state).unwrap().latest_id, event_id);
        let Response::AgentWorkingContext { agent, changed } = registry
            .dispatch(Request::ObserveAgentWorkingContext {
                agent_id: agent.id,
                shell_id: shell.id.clone(),
                run_id,
                path: repository,
            })
            .unwrap()
        else {
            panic!("expected working context");
        };
        assert!(changed);
        assert_eq!(agent.working_contexts.len(), 1);

        shell.kill().unwrap();
        registry.close_workspace(&workspace.id).unwrap();
        drop(registry);
        fs::remove_dir_all(directory).unwrap();
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
            working_contexts: Vec::new(),
        };
        let workspace = WorkspaceSnapshot {
            id: "w1".into(),
            revision: 1,
            name: "workspace".into(),
            default_cwd: None,
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
                focused_terminal: None,
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
    fn protocol_thirty_seven_combined_snapshots_hide_coordinator_workspaces() {
        let workspace_id = Uuid::from_u128(1).to_string();
        let node_id = Uuid::from_u128(2).to_string();
        let current_node = protocol::CombinedNode {
            node_id: node_id.clone(),
            alias: "remote".into(),
            local: false,
            route: Some("remote.example".into()),
            registration_revision: Some(7),
            health: protocol::NodeProjectionHealthCode::Online,
            current: true,
            stale: false,
            observed_at_ms: 10,
            observed_protocol_version: Some(37),
            observed_capabilities: vec!["protocol_37".into()],
            observed_helper_version: Some("0.41.0".into()),
            workspace_owner_eligible: true,
            workspace_owner_unavailable_reason: Some("new field".into()),
            local_snapshot: None,
            remote_projection: None,
        };
        let response = Response::CombinedNodeSnapshot {
            snapshot: protocol::CombinedNodeSnapshot {
                nodes: vec![current_node.clone()],
                workspaces: vec![protocol::GlobalWorkspaceSnapshot {
                    id: workspace_id.clone(),
                    revision: 1,
                    name: "work".into(),
                    closing: false,
                    placements: Vec::new(),
                }],
                external_workspaces: vec![protocol::ExternalWorkspaceSnapshot {
                    identity: protocol::QualifiedIdentity::new(node_id.clone(), workspace_id),
                    revision: 1,
                    name: "external".into(),
                    default_cwd: Some("/owner/work".into()),
                    available: true,
                }],
                focused_terminal: Some(protocol::QualifiedFocusedTerminalSnapshot {
                    revision: 9,
                    shell: protocol::QualifiedIdentity::new(node_id.clone(), "shell"),
                }),
            },
        };
        let Response::CombinedNodeSnapshot {
            snapshot: protocol_thirty_eight,
        } = response_for_version(response.clone(), 38)
        else {
            panic!("expected combined Node snapshot");
        };
        assert_eq!(protocol_thirty_eight.workspaces.len(), 1);
        assert_eq!(protocol_thirty_eight.external_workspaces.len(), 1);
        assert!(protocol_thirty_eight.focused_terminal.is_none());
        let Response::CombinedNodeSnapshot { snapshot } = response_for_version(response, 37) else {
            panic!("expected combined Node snapshot");
        };
        assert!(snapshot.workspaces.is_empty());
        assert!(snapshot.external_workspaces.is_empty());
        assert!(snapshot.focused_terminal.is_none());
        let encoded_node = serde_json::to_value(&snapshot.nodes[0]).unwrap();
        assert!(encoded_node.get("route").is_none());
        assert!(encoded_node.get("registration_revision").is_none());
        assert!(encoded_node.get("workspace_owner_eligible").is_none());
        assert!(
            encoded_node
                .get("workspace_owner_unavailable_reason")
                .is_none()
        );
        #[derive(serde::Deserialize)]
        #[serde(deny_unknown_fields)]
        struct ProtocolThirtySevenCombinedNode {
            node_id: String,
            alias: String,
            local: bool,
            health: protocol::NodeProjectionHealthCode,
            current: bool,
            stale: bool,
            observed_at_ms: u64,
            observed_protocol_version: Option<u32>,
            observed_capabilities: Vec<String>,
            local_snapshot: Option<Snapshot>,
            remote_projection: Option<NodeProjectionSnapshot>,
        }
        for version in 33..=37 {
            let Response::CombinedNodeSnapshot { snapshot } = response_for_version(
                Response::CombinedNodeSnapshot {
                    snapshot: protocol::CombinedNodeSnapshot {
                        nodes: vec![current_node.clone()],
                        workspaces: Vec::new(),
                        external_workspaces: Vec::new(),
                        focused_terminal: None,
                    },
                },
                version,
            ) else {
                unreachable!();
            };
            let old: ProtocolThirtySevenCombinedNode =
                serde_json::from_value(serde_json::to_value(&snapshot.nodes[0]).unwrap()).unwrap();
            assert_eq!(old.node_id, node_id);
            assert_eq!(old.alias, "remote");
            assert!(!old.local);
            assert_eq!(old.health, protocol::NodeProjectionHealthCode::Online);
            assert!(old.current);
            assert!(!old.stale);
            assert_eq!(old.observed_at_ms, 10);
            assert_eq!(old.observed_protocol_version, Some(37));
            assert_eq!(old.observed_capabilities, ["protocol_37"]);
            assert!(old.local_snapshot.is_none());
            assert!(old.remote_projection.is_none());
        }
    }

    #[test]
    fn protocol_forty_combined_snapshots_omit_observed_helper_version() {
        let node = protocol::CombinedNode {
            node_id: Uuid::from_u128(2).to_string(),
            alias: "remote".into(),
            local: false,
            route: None,
            registration_revision: None,
            health: protocol::NodeProjectionHealthCode::Online,
            current: true,
            stale: false,
            observed_at_ms: 10,
            observed_protocol_version: Some(41),
            observed_capabilities: vec!["protocol_41".into()],
            observed_helper_version: Some("0.41.0".into()),
            workspace_owner_eligible: false,
            workspace_owner_unavailable_reason: None,
            local_snapshot: None,
            remote_projection: None,
        };
        let response = |version| {
            response_for_version(
                Response::CombinedNodeSnapshot {
                    snapshot: protocol::CombinedNodeSnapshot {
                        nodes: vec![node.clone()],
                        workspaces: Vec::new(),
                        external_workspaces: Vec::new(),
                        focused_terminal: None,
                    },
                },
                version,
            )
        };

        let Response::CombinedNodeSnapshot { snapshot } = response(40) else {
            unreachable!();
        };
        let old = serde_json::to_value(&snapshot.nodes[0]).unwrap();
        assert!(old.get("observed_helper_version").is_none());

        let Response::CombinedNodeSnapshot { snapshot } = response(41) else {
            unreachable!();
        };
        assert_eq!(
            serde_json::to_value(&snapshot.nodes[0]).unwrap()["observed_helper_version"],
            "0.41.0"
        );

        let health = protocol::NodeProjectionHealth {
            code: protocol::NodeProjectionHealthCode::Online,
            stale: false,
            cache_generation: 1,
            stream_id: None,
            cursor: None,
            last_attempt_at_ms: None,
            last_success_at_ms: None,
            retry_at_ms: None,
            capabilities: vec!["protocol_41".into()],
            observed_helper_version: Some("0.41.0".into()),
        };
        let Response::NodeProjectionHealth { health } =
            response_for_version(Response::NodeProjectionHealth { health }, 40)
        else {
            unreachable!();
        };
        assert!(serde_json::to_value(health).unwrap()["observed_helper_version"].is_null());
    }

    #[test]
    fn protocol_forty_preserves_only_negotiated_recovery_presentations() {
        let shell = ShellSnapshot {
            id: "shell-1".into(),
            revision: 1,
            workspace_id: "workspace-1".into(),
            name: "agent".into(),
            cwd: "/tmp/project".into(),
            command: Vec::new(),
            status: ShellStatus::Pending,
            run: Some(ShellRunSnapshot {
                id: "run-1".into(),
                generation: 1,
                started_at_ms: 1,
                ended_at_ms: Some(2),
                exit_reason: Some(ShellRunExitReason::Interrupted),
                output_revision: 3,
                environment_has_run_id: true,
            }),
            recovered_agent_id: Some("agent-1".into()),
            foreground_process: None,
        };
        let workspace = WorkspaceSnapshot {
            id: "workspace-1".into(),
            revision: 1,
            name: "project".into(),
            default_cwd: None,
            shells: vec![shell.clone()],
            launchers: Vec::new(),
            agents: Vec::new(),
        };
        let response = Response::Snapshot {
            snapshot: Snapshot {
                workspaces: vec![workspace.clone()],
                focused_terminal: None,
            },
        };
        let Response::Snapshot { snapshot } = response_for_version(response.clone(), 39) else {
            panic!("expected snapshot");
        };
        assert!(snapshot.workspaces[0].shells[0].run.is_none());
        assert!(
            snapshot.workspaces[0].shells[0]
                .recovered_agent_id
                .is_none()
        );
        let Response::Snapshot { snapshot } = response_for_version(response, 40) else {
            panic!("expected snapshot");
        };
        assert_eq!(
            snapshot.workspaces[0].shells[0].run.as_ref().unwrap().id,
            "run-1"
        );
        assert_eq!(
            snapshot.workspaces[0].shells[0]
                .recovered_agent_id
                .as_deref(),
            Some("agent-1")
        );

        let response = Response::RoutedNodeOperation {
            result: RoutedOperationResult::Workspace {
                workspace: workspace.clone(),
            },
        };
        let Response::RoutedNodeOperation {
            result: RoutedOperationResult::Workspace { workspace },
        } = response_for_version(response, 39)
        else {
            panic!("expected routed workspace");
        };
        assert!(workspace.shells[0].run.is_none());
        assert!(workspace.shells[0].recovered_agent_id.is_none());

        let response = Response::GlobalWorkspaceResource {
            workspace: protocol::GlobalWorkspaceSnapshot {
                id: "global-1".into(),
                revision: 1,
                name: "project".into(),
                closing: false,
                placements: Vec::new(),
            },
            resource: RoutedOperationResult::Shell {
                shell: shell.clone(),
            },
        };
        let Response::GlobalWorkspaceResource {
            resource: RoutedOperationResult::Shell { shell },
            ..
        } = response_for_version(response, 39)
        else {
            panic!("expected global workspace shell");
        };
        assert!(shell.run.is_none());
        assert!(shell.recovered_agent_id.is_none());

        let mut projection = remote_notification_projection("node-1");
        projection.shells[0].status = ShellStatus::Pending;
        projection.shells[0].recovered_agent_id = Some("agent-blocked".into());
        let response = Response::NodeProjectionSync {
            sync: NodeProjectionSync {
                mode: NodeProjectionSyncMode::Baseline,
                cursor: EventCursor {
                    stream_id: "stream".into(),
                    event_id: 1,
                },
                projection,
                transitions: Vec::new(),
                capabilities: vec!["recovered_agent_presentation".into()],
            },
        };
        let Response::NodeProjectionSync { sync } = response_for_version(response.clone(), 39)
        else {
            panic!("expected projection sync");
        };
        assert!(sync.projection.shells[0].run_id.is_none());
        assert!(sync.projection.shells[0].recovered_agent_id.is_none());
        let Response::NodeProjectionSync { sync } = response_for_version(response, 40) else {
            panic!("expected projection sync");
        };
        assert_eq!(sync.projection.shells[0].run_id.as_deref(), Some("run-1"));
        assert_eq!(
            sync.projection.shells[0].recovered_agent_id.as_deref(),
            Some("agent-blocked")
        );
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
            working_contexts: Vec::new(),
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
            working_contexts: Vec::new(),
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
                }],
                focused_terminal: None,
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
    fn protocol_forty_eight_filters_default_cwd_events_but_advances_cursor() {
        let cursor = EventCursor {
            stream_id: "stream".into(),
            event_id: 4,
        };
        let response = Response::Events {
            stream_id: "stream".into(),
            cursor: cursor.clone(),
            snapshot: None,
            events: vec![DaemonEvent {
                id: 4,
                at_ms: 1,
                kind: DaemonEventKind::WorkspaceDefaultCwdChanged {
                    workspace_id: "workspace".into(),
                    default_cwd: "/work".into(),
                },
            }],
        };
        let Response::Events {
            cursor: filtered_cursor,
            events,
            ..
        } = response_for_version(response, 48)
        else {
            panic!("expected events response");
        };
        assert_eq!(filtered_cursor, cursor);
        assert!(events.is_empty());
    }

    #[test]
    fn protocol_forty_nine_filters_working_contexts_without_rewinding_cursors() {
        let context = AgentWorkingContextSnapshot {
            worktree_root: "/worktrees/boomux".into(),
            repository: "boomux".into(),
            branch: "feat/working-contexts".into(),
            observed_at_ms: 5,
        };
        let agent = AgentInstanceSnapshot {
            id: "a1".into(),
            workspace_id: "w1".into(),
            shell_id: "s1".into(),
            run_id: "r1".into(),
            name: "agent".into(),
            integration: "test".into(),
            external_session_id: Some("external".into()),
            cwd: Some("/worktrees/boomux".into()),
            started_at_ms: 1,
            ended_at_ms: None,
            observation: AgentObservationSnapshot {
                revision: 1,
                state: AgentState::Working,
                authority: AgentAuthority::LifecycleIntegration,
                evidence: "working".into(),
                confidence: 100,
                observed_at_ms: 1,
            },
            attention: None,
            working_contexts: vec![context],
        };
        let cursor = EventCursor {
            stream_id: "stream".into(),
            event_id: 5,
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
                }],
                focused_terminal: None,
            }),
            events: vec![DaemonEvent {
                id: 5,
                at_ms: 5,
                kind: DaemonEventKind::AgentWorkingContextObserved {
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
        } = response_for_version(response, 49)
        else {
            panic!("expected events response");
        };
        assert_eq!(filtered_cursor, cursor);
        assert!(events.is_empty());
        assert!(snapshot.workspaces[0].agents[0].working_contexts.is_empty());

        let sync = Response::NodeProjectionSync {
            sync: NodeProjectionSync {
                mode: NodeProjectionSyncMode::Resumed,
                cursor: cursor.clone(),
                projection: remote_notification_projection("node-1"),
                transitions: vec![
                    NodeProjectionTransition {
                        event_id: 4,
                        at_ms: 4,
                        kind: NodeProjectionTransitionKind::HandoffCompleted,
                    },
                    NodeProjectionTransition {
                        event_id: 5,
                        at_ms: 5,
                        kind: NodeProjectionTransitionKind::SessionContext {
                            workspace_id: "w1".into(),
                            agent_id: "a1".into(),
                        },
                    },
                ],
                capabilities: vec!["session_presentation_context".into()],
            },
        };
        let Response::NodeProjectionSync { sync: filtered } =
            response_for_version(sync.clone(), 49)
        else {
            panic!("expected projection sync");
        };
        assert_eq!(filtered.cursor, cursor);
        assert_eq!(filtered.transitions.len(), 1);
        assert!(matches!(
            filtered.transitions[0].kind,
            NodeProjectionTransitionKind::HandoffCompleted
        ));
        let Response::NodeProjectionSync { sync } = response_for_version(sync, 50) else {
            panic!("expected projection sync");
        };
        assert_eq!(sync.transitions.len(), 2);
    }

    #[test]
    fn protocol_forty_eight_owner_fails_default_cwd_feature_preflight() {
        let protocol_forty_eight = protocol::ProtocolFeature::ALL
            .iter()
            .copied()
            .filter(|feature| feature.minimum_version() <= 48)
            .flat_map(protocol::ProtocolFeature::capability_names)
            .map(|capability| (*capability).to_owned())
            .collect::<Vec<_>>();
        assert!(matches!(
            require_capabilities_support_feature(
                &protocol_forty_eight,
                protocol::ProtocolFeature::WorkspacePlacementDefaultCwd,
            ),
            Err(DaemonError::Lifecycle {
                code: ErrorCode::UnsupportedVersion,
                ..
            })
        ));
        let mut protocol_forty_nine = protocol_forty_eight;
        protocol_forty_nine.extend(
            protocol::ProtocolFeature::WorkspacePlacementDefaultCwd
                .capability_names()
                .iter()
                .map(|capability| (*capability).to_owned()),
        );
        assert!(capabilities_support_feature(
            &protocol_forty_nine,
            protocol::ProtocolFeature::WorkspacePlacementDefaultCwd,
        ));
    }

    #[test]
    fn remote_default_cwd_attempt_is_marked_only_after_supported_handshake() {
        let mut attempted = false;
        let unsupported = prepare_supported_owner_request(
            48,
            Some(protocol::ProtocolFeature::WorkspacePlacementDefaultCwd),
            &mut || {
                attempted = true;
                Ok(())
            },
        );
        assert_eq!(unsupported.unwrap_err().kind(), io::ErrorKind::Unsupported);
        assert!(!attempted);

        prepare_supported_owner_request(
            49,
            Some(protocol::ProtocolFeature::WorkspacePlacementDefaultCwd),
            &mut || {
                attempted = true;
                Ok(())
            },
        )
        .unwrap();
        assert!(attempted);
    }

    #[test]
    fn session_display_name_remote_request_requires_protocol_fifty_before_dispatch() {
        let mut dispatched = false;
        let unsupported = prepare_supported_owner_request(
            49,
            Some(protocol::ProtocolFeature::SessionDisplayNames),
            &mut || {
                dispatched = true;
                Ok(())
            },
        );
        assert_eq!(unsupported.unwrap_err().kind(), io::ErrorKind::Unsupported);
        assert!(!dispatched);

        prepare_supported_owner_request(
            50,
            Some(protocol::ProtocolFeature::SessionDisplayNames),
            &mut || {
                dispatched = true;
                Ok(())
            },
        )
        .unwrap();
        assert!(dispatched);
    }

    #[test]
    fn default_cwd_retains_only_ambiguous_owner_errors() {
        for code in [
            ErrorCode::OutcomeUnknown,
            ErrorCode::PersistenceFailed,
            ErrorCode::Timeout,
        ] {
            assert!(default_cwd_owner_error_is_ambiguous(Some(code)));
        }
        for code in [
            ErrorCode::RevisionAhead,
            ErrorCode::UnsupportedVersion,
            ErrorCode::NotFound,
            ErrorCode::Internal,
        ] {
            assert!(!default_cwd_owner_error_is_ambiguous(Some(code)));
        }
        assert!(!default_cwd_owner_error_is_ambiguous(None));
    }

    #[test]
    fn guarded_workspace_default_cwd_changes_only_future_defaults() {
        let root = env::temp_dir().join(format!("boomux-default-cwd-{}", Uuid::new_v4()));
        let old = root.join("old");
        let new = root.join("new");
        fs::create_dir_all(&old).unwrap();
        fs::create_dir_all(&new).unwrap();
        let registry = DaemonService::default();
        let workspace = registry
            .create_workspace_with_default_cwd(
                "work".into(),
                Some(old.clone()),
                vec![ShellSpec::login("existing", old.clone())],
            )
            .unwrap();
        let response = registry
            .dispatch(Request::GuardedSetWorkspaceDefaultCwd {
                workspace_id: workspace.id.clone(),
                expected_revision: workspace.revision,
                default_cwd: new.clone(),
            })
            .unwrap();
        let Response::Workspace { workspace: updated } = response else {
            panic!("expected Workspace response");
        };
        assert_eq!(updated.revision, workspace.revision + 1);
        assert_eq!(updated.default_cwd.as_deref(), Some(new.as_path()));
        assert_eq!(updated.shells[0].cwd, old);

        let Response::Events { cursor, .. } = registry
            .dispatch(Request::Events {
                after: None,
                limit: 256,
                wait_ms: 0,
            })
            .unwrap()
        else {
            panic!("expected event baseline");
        };
        let unchanged = registry
            .dispatch(Request::GuardedSetWorkspaceDefaultCwd {
                workspace_id: workspace.id.clone(),
                expected_revision: updated.revision,
                default_cwd: new,
            })
            .unwrap();
        let Response::Workspace {
            workspace: unchanged,
        } = unchanged
        else {
            panic!("expected unchanged Workspace response");
        };
        assert_eq!(unchanged.revision, updated.revision);
        let Response::Events { events, .. } = registry
            .dispatch(Request::Events {
                after: Some(cursor),
                limit: 256,
                wait_ms: 0,
            })
            .unwrap()
        else {
            panic!("expected event page");
        };
        assert!(events.is_empty());
        let stale = registry.dispatch(Request::GuardedSetWorkspaceDefaultCwd {
            workspace_id: workspace.id,
            expected_revision: workspace.revision,
            default_cwd: root.clone(),
        });
        assert!(matches!(
            stale,
            Err(DaemonError::Lifecycle {
                code: ErrorCode::RevisionAhead,
                ..
            })
        ));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn workspace_snapshot_never_tears_revision_from_default_cwd() {
        let root = env::temp_dir().join(format!("boomux-cwd-snapshot-{}", Uuid::new_v4()));
        let old = root.join("old");
        let new = root.join("new");
        fs::create_dir_all(&old).unwrap();
        fs::create_dir_all(&new).unwrap();
        let registry = Arc::new(DaemonService::default());
        let workspace = registry
            .create_workspace_with_default_cwd("work".into(), Some(old.clone()), Vec::new())
            .unwrap();
        let finished = Arc::new(AtomicBool::new(false));
        let writer_registry = Arc::clone(&registry);
        let writer_finished = Arc::clone(&finished);
        let workspace_id = workspace.id.clone();
        let writer_old = old.clone();
        let writer_new = new.clone();
        let writer = thread::spawn(move || {
            for index in 0..2_000 {
                let cwd = if index % 2 == 0 {
                    writer_new.clone()
                } else {
                    writer_old.clone()
                };
                assert!(
                    writer_registry
                        .durable
                        .set_workspace_default_cwd(&workspace_id, cwd)
                        .unwrap()
                        .is_some()
                );
            }
            writer_finished.store(true, Ordering::Release);
        });
        while !finished.load(Ordering::Acquire) {
            let snapshot = registry
                .workspace(&workspace.id)
                .unwrap()
                .snapshot(&registry.durable)
                .unwrap();
            let expected = if snapshot.revision % 2 == 0 {
                new.as_path()
            } else {
                old.as_path()
            };
            assert_eq!(snapshot.default_cwd.as_deref(), Some(expected));
        }
        writer.join().unwrap();
        fs::remove_dir_all(root).unwrap();
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
    fn unavailable_global_store_suppresses_runtime_coordination_capabilities() {
        let registry = DaemonService::default();
        let capabilities = registry.runtime_protocol_capabilities();
        assert!(!capabilities.iter().any(|capability| {
            protocol::ProtocolFeature::GlobalWorkspaces
                .capability_names()
                .contains(&capability.as_str())
        }));
    }

    #[test]
    fn forced_node_projection_refresh_interrupts_only_the_existing_worker_sleep() {
        let registry = DaemonService::default();
        let node_id = Uuid::from_u128(42).to_string();
        lock(&registry.node_projection_workers.wake)
            .unwrap()
            .insert(node_id.clone());
        let started = Instant::now();
        assert!(!interruptible_node_sleep(
            &registry,
            &node_id,
            Duration::from_secs(1)
        ));
        assert!(started.elapsed() < Duration::from_millis(200));
        assert!(
            lock(&registry.node_projection_workers.wake)
                .unwrap()
                .is_empty()
        );
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
            lock(&workspace.default_cwd).unwrap().as_deref(),
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
            working_contexts: Vec::new(),
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

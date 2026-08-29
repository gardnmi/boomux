use std::fs;
use std::io::{self, Read, Write};
use std::mem::MaybeUninit;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::fs::MetadataExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::client;
use crate::fd_transfer::receive_descriptor;
use crate::protocol::{
    self, ClaudeRemoteControlBindingSnapshot, DaemonEvent, FocusedTerminalSnapshot,
    NotificationDeliveryConfig, QualifiedFocusedTerminalSnapshot, TerminalProfile,
};
use crate::state_store;

const HANDSHAKE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);
pub(crate) const CHANNEL_FD: RawFd = 198;
pub(crate) const HEADER: &[u8; 8] = b"BOOMUXH8";
pub(crate) const LISTENER_MARKER: u8 = 1;
pub(crate) const RUNTIME_LOCK_MARKER: u8 = 2;
pub(crate) const STATE_LOCK_MARKER: u8 = 3;
pub(crate) const READY: u8 = 4;
pub(crate) const ABORT: u8 = 5;
pub(crate) const COMMIT: u8 = 6;
pub(crate) const PREPARED: u8 = 7;
pub(crate) const FINALIZE: u8 = 8;
pub(crate) const COMMITTED: u8 = 9;
pub(crate) const PTY_MARKER: u8 = 10;
pub(crate) const PIDFD_MARKER: u8 = 11;
pub(crate) const OPENCODE_PIDFD_MARKER: u8 = 12;

#[derive(Serialize, Deserialize)]
pub(crate) struct Manifest {
    pub(crate) runtimes: Vec<RuntimeManifest>,
    pub(crate) exited: Vec<ExitedManifest>,
    pub(crate) event_stream: EventStreamManifest,
    #[serde(default)]
    pub(crate) notifications: Option<NotificationDeliveryConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) focused_terminal: Option<FocusedTerminalSnapshot>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) presented_focused_terminal: Option<QualifiedFocusedTerminalSnapshot>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) opencode_runtime: Option<OpenCodeRuntimeManifest>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) claude_remote_control_bindings: Vec<ClaudeRemoteControlBindingSnapshot>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) kiro_launch_holders: Vec<KiroLaunchHolderManifest>,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct EventStreamManifest {
    pub(crate) stream_id: String,
    pub(crate) latest_id: u64,
    pub(crate) events: Vec<DaemonEvent>,
}

#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct RuntimeManifest {
    pub(crate) shell_id: String,
    #[serde(default)]
    pub(crate) run_id: Option<String>,
    #[serde(default)]
    pub(crate) output_revision: Option<u64>,
    pub(crate) profile: TerminalProfile,
    pub(crate) pid: u32,
}

#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct ExitedManifest {
    pub(crate) shell_id: String,
    pub(crate) run_id: String,
    pub(crate) output_revision: u64,
    pub(crate) profile: TerminalProfile,
    pub(crate) code: Option<u32>,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct OpenCodeRuntimeManifest {
    pub(crate) generation_id: String,
    pub(crate) port: u16,
    pub(crate) pid: u32,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct KiroLaunchHolderManifest {
    pub(crate) holder_id: String,
    pub(crate) pid: u32,
    pub(crate) start_time: u64,
    pub(crate) process_group_leader: bool,
    pub(crate) shell_id: String,
    pub(crate) run_id: String,
    pub(crate) sessions: Vec<(String, String)>,
}

pub(crate) struct TransferredRuntime {
    pub(crate) manifest: RuntimeManifest,
    pub(crate) pty: OwnedFd,
    pub(crate) pidfd: OwnedFd,
    pub(crate) reconstruction: Vec<u8>,
}

pub(crate) struct TransferredExited {
    pub(crate) manifest: ExitedManifest,
    pub(crate) reconstruction: Vec<u8>,
}

pub(crate) struct TransferredOpenCodeRuntime {
    pub(crate) manifest: OpenCodeRuntimeManifest,
    pub(crate) pidfd: OwnedFd,
}

pub(crate) enum Bootstrap {
    Aborted,
    Committed {
        channel: UnixStream,
        listener: UnixListener,
        runtime_lock: OwnedFd,
        state_lock: OwnedFd,
        runtimes: Vec<TransferredRuntime>,
        exited: Vec<TransferredExited>,
        event_stream: Box<EventStreamManifest>,
        notifications: Box<Option<NotificationDeliveryConfig>>,
        focused_terminal: Option<Box<FocusedTerminalSnapshot>>,
        presented_focused_terminal: Option<Box<QualifiedFocusedTerminalSnapshot>>,
        opencode_runtime: Option<TransferredOpenCodeRuntime>,
        claude_remote_control_bindings: Vec<ClaudeRemoteControlBindingSnapshot>,
        kiro_launch_holders: Vec<KiroLaunchHolderManifest>,
    },
}

pub(crate) fn receive_bootstrap(channel: RawFd) -> io::Result<Bootstrap> {
    let mut channel = adopt_channel(channel)?;
    channel.set_read_timeout(Some(HANDSHAKE_TIMEOUT))?;
    channel.set_write_timeout(Some(HANDSHAKE_TIMEOUT))?;
    let mut header = [0; HEADER.len()];
    channel.read_exact(&mut header)?;
    if !supported_header(&header) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "replacement bootstrap version is unsupported",
        ));
    }
    let manifest: Manifest = protocol::read_message(&mut channel)?;
    validate_manifest(&manifest)?;
    let Manifest {
        runtimes: runtime_manifests,
        exited: exited_manifests,
        event_stream,
        notifications,
        focused_terminal,
        presented_focused_terminal,
        opencode_runtime,
        claude_remote_control_bindings,
        kiro_launch_holders,
    } = manifest;
    let focused_terminal = focused_terminal.map(Box::new);
    let presented_focused_terminal = presented_focused_terminal.map(Box::new);
    let listener = receive_descriptor(&channel, LISTENER_MARKER)?;
    let runtime_lock = receive_descriptor(&channel, RUNTIME_LOCK_MARKER)?;
    let state_lock = receive_descriptor(&channel, STATE_LOCK_MARKER)?;
    let opencode_runtime = opencode_runtime
        .map(|manifest| {
            let pidfd = receive_descriptor(&channel, OPENCODE_PIDFD_MARKER)?;
            validate_pidfd(&pidfd, manifest.pid)?;
            Ok::<_, io::Error>(TransferredOpenCodeRuntime { manifest, pidfd })
        })
        .transpose()?;
    let mut runtimes = Vec::with_capacity(runtime_manifests.len());
    for manifest in runtime_manifests {
        let pty = receive_descriptor(&channel, PTY_MARKER)?;
        validate_pty(&pty, manifest.pid)?;
        let pidfd = receive_descriptor(&channel, PIDFD_MARKER)?;
        validate_pidfd(&pidfd, manifest.pid)?;
        let reconstruction: Vec<u8> = protocol::read_message(&mut channel)?;
        if reconstruction.len() > protocol::MAX_ATTACH_FRAME {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "transferred terminal reconstruction exceeds the size limit",
            ));
        }
        runtimes.push(TransferredRuntime {
            manifest,
            pty,
            pidfd,
            reconstruction,
        });
    }
    let mut exited = Vec::with_capacity(exited_manifests.len());
    for manifest in exited_manifests {
        let reconstruction: Vec<u8> = protocol::read_message(&mut channel)?;
        if reconstruction.len() > protocol::MAX_ATTACH_FRAME {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "transferred exited terminal reconstruction exceeds the size limit",
            ));
        }
        exited.push(TransferredExited {
            manifest,
            reconstruction,
        });
    }

    let expected_socket = client::socket_path()?;
    let listener = UnixListener::from(listener);
    if listener.local_addr()?.as_pathname() != Some(expected_socket.as_path()) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "transferred listener does not match the Boomux socket",
        ));
    }
    validate_listener(&listener)?;
    let runtime_lock_path = expected_socket
        .parent()
        .ok_or_else(|| io::Error::other("socket path has no parent"))?
        .join("daemon.lock");
    validate_lock(&runtime_lock, &runtime_lock_path)?;
    validate_lock(&state_lock, &state_store::lock_path_from_environment()?)?;

    channel.write_all(&[READY])?;
    let mut decision = [0];
    channel.read_exact(&mut decision)?;
    match decision[0] {
        ABORT => Ok(Bootstrap::Aborted),
        COMMIT => Ok(Bootstrap::Committed {
            channel,
            listener,
            runtime_lock,
            state_lock,
            runtimes,
            exited,
            event_stream: Box::new(event_stream),
            notifications: Box::new(notifications),
            focused_terminal,
            presented_focused_terminal,
            opencode_runtime,
            claude_remote_control_bindings,
            kiro_launch_holders,
        }),
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "replacement bootstrap received an unsupported decision",
        )),
    }
}

fn supported_header(header: &[u8; 8]) -> bool {
    header == HEADER
}

fn validate_pty(descriptor: &OwnedFd, expected_session: u32) -> io::Result<()> {
    let mut pty_number = 0_u32;
    // TIOCGPTN succeeds only for a Unix PTY master and initializes the number.
    if unsafe { libc::ioctl(descriptor.as_raw_fd(), libc::TIOCGPTN, &mut pty_number) } == -1 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "transferred PTY descriptor is invalid: {}",
                io::Error::last_os_error()
            ),
        ));
    }
    let mut session_id = 0 as libc::pid_t;
    // TIOCGSID returns the controlling session associated with this PTY.
    if unsafe { libc::ioctl(descriptor.as_raw_fd(), libc::TIOCGSID, &mut session_id) } == -1
        || session_id != expected_session as libc::pid_t
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "transferred PTY does not match its process session",
        ));
    }
    Ok(())
}

fn validate_pidfd(descriptor: &OwnedFd, expected_pid: u32) -> io::Result<()> {
    let contents = fs::read_to_string(format!("/proc/self/fdinfo/{}", descriptor.as_raw_fd()))?;
    let actual_pid = contents.lines().find_map(|line| {
        line.strip_prefix("Pid:")
            .and_then(|value| value.trim().parse::<u32>().ok())
    });
    if actual_pid == Some(expected_pid) {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "transferred pidfd does not match its process",
        ))
    }
}

fn validate_manifest(manifest: &Manifest) -> io::Result<()> {
    if manifest.opencode_runtime.as_ref().is_some_and(|runtime| {
        runtime.port == 0
            || runtime.pid == 0
            || uuid::Uuid::parse_str(&runtime.generation_id).is_err()
    }) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "handoff manifest contains an invalid OpenCode runtime",
        ));
    }
    if manifest
        .presented_focused_terminal
        .as_ref()
        .is_some_and(|focused| {
            focused.revision == 0
                || uuid::Uuid::parse_str(&focused.shell.node_id).is_err()
                || uuid::Uuid::parse_str(&focused.shell.inner_id).is_err()
        })
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "handoff manifest contains invalid presented terminal focus",
        ));
    }
    if manifest
        .runtimes
        .len()
        .saturating_add(manifest.exited.len())
        > 1_024
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "handoff manifest contains too many runtimes",
        ));
    }
    if manifest.claude_remote_control_bindings.len() > protocol::MAX_CLAUDE_REMOTE_CONTROL_BINDINGS
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "handoff manifest contains too many Claude Remote Control bindings",
        ));
    }
    let mut kiro_holder_ids = std::collections::HashSet::new();
    if manifest.kiro_launch_holders.len() > protocol::MAX_KIRO_LAUNCH_HOLDERS
        || manifest.kiro_launch_holders.iter().any(|holder| {
            let mut session_ids = std::collections::HashSet::new();
            holder.pid == 0
                || holder.start_time == 0
                || uuid::Uuid::parse_str(&holder.holder_id).is_err()
                || !kiro_holder_ids.insert(&holder.holder_id)
                || uuid::Uuid::parse_str(&holder.shell_id).is_err()
                || uuid::Uuid::parse_str(&holder.run_id).is_err()
                || holder.sessions.len() > protocol::MAX_KIRO_HOLDER_SESSIONS
                || holder.sessions.iter().any(|(session_id, agent_id)| {
                    session_id.is_empty()
                        || session_id.len() > protocol::MAX_KIRO_SESSION_ID_BYTES
                        || session_id.chars().any(char::is_control)
                        || !session_ids.insert(session_id)
                        || uuid::Uuid::parse_str(agent_id).is_err()
                })
        })
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "handoff manifest contains invalid Kiro launch holders",
        ));
    }
    let mut binding_agent_ids = std::collections::HashSet::new();
    let mut binding_bridge_ids = std::collections::HashSet::new();
    for binding in &manifest.claude_remote_control_bindings {
        if uuid::Uuid::parse_str(&binding.agent_id).is_err()
            || uuid::Uuid::parse_str(&binding.shell_id).is_err()
            || uuid::Uuid::parse_str(&binding.run_id).is_err()
            || binding.bridge_session_id.is_empty()
            || binding.bridge_session_id.len() > protocol::MAX_CLAUDE_BRIDGE_SESSION_ID_BYTES
            || binding.bridge_session_id.chars().any(char::is_control)
            || !binding_agent_ids.insert(&binding.agent_id)
            || !binding_bridge_ids.insert(&binding.bridge_session_id)
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "handoff manifest contains an invalid Claude Remote Control binding",
            ));
        }
    }
    let mut shell_ids = std::collections::HashSet::new();
    let mut run_ids = std::collections::HashSet::new();
    for runtime in &manifest.runtimes {
        let valid_run = runtime
            .run_id
            .as_ref()
            .is_none_or(|run_id| uuid::Uuid::parse_str(run_id).is_ok() && run_ids.insert(run_id));
        if runtime.pid == 0 || !shell_ids.insert(&runtime.shell_id) || !valid_run {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "handoff manifest contains an invalid runtime",
            ));
        }
    }
    for exited in &manifest.exited {
        let valid_run =
            uuid::Uuid::parse_str(&exited.run_id).is_ok() && run_ids.insert(&exited.run_id);
        if !shell_ids.insert(&exited.shell_id) || !valid_run {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "handoff manifest contains an invalid exited shell",
            ));
        }
    }
    if uuid::Uuid::parse_str(&manifest.event_stream.stream_id).is_err()
        || manifest.event_stream.events.len() > 8_192
        || manifest
            .event_stream
            .events
            .windows(2)
            .any(|events| events[0].id.checked_add(1) != Some(events[1].id))
        || manifest
            .event_stream
            .events
            .last()
            .is_some_and(|event| event.id != manifest.event_stream.latest_id)
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "handoff manifest contains an invalid event stream",
        ));
    }
    Ok(())
}

fn adopt_channel(channel: RawFd) -> io::Result<UnixStream> {
    if channel < 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "handoff channel descriptor must be nonnegative",
        ));
    }
    // Duplication validates the inherited descriptor before Rust adopts it.
    let duplicated = unsafe { libc::fcntl(channel, libc::F_DUPFD_CLOEXEC, 0) };
    if duplicated == -1 {
        return Err(io::Error::last_os_error());
    }
    // fcntl returned a fresh descriptor with unique ownership.
    let descriptor = unsafe { OwnedFd::from_raw_fd(duplicated) };
    // The hidden command contract transfers the inherited endpoint; the
    // validated duplicate is now its sole owner in this process.
    let _ = unsafe { libc::close(channel) };
    let stream = UnixStream::from(descriptor);
    let mut socket_type = 0_i32;
    let mut length = std::mem::size_of_val(&socket_type) as libc::socklen_t;
    // The output pointer and length describe a writable integer.
    if unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_TYPE,
            (&mut socket_type as *mut i32).cast(),
            &mut length,
        )
    } == -1
    {
        return Err(io::Error::last_os_error());
    }
    if socket_type != libc::SOCK_STREAM {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "handoff channel is not a stream socket",
        ));
    }
    Ok(stream)
}

fn validate_listener(listener: &UnixListener) -> io::Result<()> {
    let mut accepting = 0_i32;
    let mut length = std::mem::size_of_val(&accepting) as libc::socklen_t;
    // The output pointer and length describe a writable integer.
    if unsafe {
        libc::getsockopt(
            listener.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_ACCEPTCONN,
            (&mut accepting as *mut i32).cast(),
            &mut length,
        )
    } == -1
    {
        return Err(io::Error::last_os_error());
    }
    if accepting != 1 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "transferred descriptor is not a listening socket",
        ));
    }
    let flags = unsafe { libc::fcntl(listener.as_raw_fd(), libc::F_GETFL) };
    if flags == -1
        || unsafe {
            libc::fcntl(
                listener.as_raw_fd(),
                libc::F_SETFL,
                flags | libc::O_NONBLOCK,
            )
        } == -1
    {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn validate_lock(descriptor: &OwnedFd, path: &Path) -> io::Result<()> {
    let path_metadata = fs::symlink_metadata(path)?;
    let mut descriptor_metadata = MaybeUninit::<libc::stat>::uninit();
    // fstat initializes the supplied stat structure for this owned descriptor.
    if unsafe { libc::fstat(descriptor.as_raw_fd(), descriptor_metadata.as_mut_ptr()) } == -1 {
        return Err(io::Error::last_os_error());
    }
    // fstat succeeded, so the structure is initialized.
    let descriptor_metadata = unsafe { descriptor_metadata.assume_init() };
    if !path_metadata.file_type().is_file()
        || path_metadata.uid() != unsafe { libc::geteuid() }
        || path_metadata.dev() != descriptor_metadata.st_dev
        || path_metadata.ino() != descriptor_metadata.st_ino
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("transferred lock does not match {}", path.display()),
        ));
    }
    // Inherited flock ownership is tied to the transferred open file
    // description. Reacquiring is idempotent for a valid inherited lock and
    // establishes exclusivity if the sender omitted it.
    if unsafe { libc::flock(descriptor.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::fd::IntoRawFd;

    fn event_stream() -> EventStreamManifest {
        EventStreamManifest {
            stream_id: uuid::Uuid::new_v4().to_string(),
            latest_id: 0,
            events: Vec::new(),
        }
    }

    #[test]
    fn old_manifest_defaults_focus_state() {
        let manifest: Manifest = serde_json::from_value(serde_json::json!({
            "runtimes": [],
            "exited": [],
            "event_stream": event_stream(),
            "notifications": null
        }))
        .unwrap();

        assert!(manifest.focused_terminal.is_none());
        assert!(manifest.presented_focused_terminal.is_none());
        assert!(manifest.opencode_runtime.is_none());
        assert!(manifest.claude_remote_control_bindings.is_empty());
        assert!(manifest.kiro_launch_holders.is_empty());
    }

    #[test]
    fn only_h8_bootstrap_header_is_accepted() {
        assert!(supported_header(HEADER));
        assert!(!supported_header(b"BOOMUXH7"));
    }

    #[test]
    fn receive_bootstrap_rejects_h7_before_reading_a_manifest() {
        let (mut sender, receiver) = UnixStream::pair().unwrap();
        sender.write_all(b"BOOMUXH7").unwrap();

        let error = match receive_bootstrap(receiver.into_raw_fd()) {
            Ok(_) => panic!("H7 bootstrap must be rejected"),
            Err(error) => error,
        };
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert_eq!(
            error.to_string(),
            "replacement bootstrap version is unsupported"
        );
    }

    #[test]
    fn manifest_round_trips_focused_terminal() {
        let focused_terminal = FocusedTerminalSnapshot {
            revision: 4,
            workspace_id: "w1".into(),
            shell_id: "s1".into(),
            run_id: "r1".into(),
        };
        let manifest = Manifest {
            runtimes: Vec::new(),
            exited: Vec::new(),
            event_stream: event_stream(),
            notifications: None,
            focused_terminal: Some(focused_terminal.clone()),
            presented_focused_terminal: Some(QualifiedFocusedTerminalSnapshot {
                revision: 5,
                shell: protocol::QualifiedIdentity::new(
                    uuid::Uuid::from_u128(1).to_string(),
                    uuid::Uuid::from_u128(2).to_string(),
                ),
            }),
            opencode_runtime: None,
            claude_remote_control_bindings: Vec::new(),
            kiro_launch_holders: Vec::new(),
        };

        let decoded: Manifest =
            serde_json::from_value(serde_json::to_value(manifest).unwrap()).unwrap();

        assert_eq!(decoded.focused_terminal, Some(focused_terminal));
        assert_eq!(decoded.presented_focused_terminal.unwrap().revision, 5);
    }

    #[test]
    fn manifest_round_trips_bounded_opencode_runtime() {
        let generation_id = uuid::Uuid::new_v4().to_string();
        let manifest = Manifest {
            runtimes: Vec::new(),
            exited: Vec::new(),
            event_stream: event_stream(),
            notifications: None,
            focused_terminal: None,
            presented_focused_terminal: None,
            opencode_runtime: Some(OpenCodeRuntimeManifest {
                generation_id: generation_id.clone(),
                port: 4096,
                pid: 42,
            }),
            claude_remote_control_bindings: Vec::new(),
            kiro_launch_holders: Vec::new(),
        };

        validate_manifest(&manifest).unwrap();
        let decoded: Manifest =
            serde_json::from_value(serde_json::to_value(manifest).unwrap()).unwrap();

        let runtime = decoded.opencode_runtime.unwrap();
        assert_eq!(runtime.generation_id, generation_id);
        assert_eq!(runtime.port, 4096);
        assert_eq!(runtime.pid, 42);
    }

    #[test]
    fn manifest_round_trips_bounded_claude_remote_control_binding() {
        let binding = ClaudeRemoteControlBindingSnapshot {
            agent_id: uuid::Uuid::new_v4().to_string(),
            shell_id: uuid::Uuid::new_v4().to_string(),
            run_id: uuid::Uuid::new_v4().to_string(),
            bridge_session_id: "bridge/exact".into(),
        };
        let manifest = Manifest {
            runtimes: Vec::new(),
            exited: Vec::new(),
            event_stream: event_stream(),
            notifications: None,
            focused_terminal: None,
            presented_focused_terminal: None,
            opencode_runtime: None,
            claude_remote_control_bindings: vec![binding.clone()],
            kiro_launch_holders: Vec::new(),
        };
        validate_manifest(&manifest).unwrap();
        let decoded: Manifest =
            serde_json::from_value(serde_json::to_value(manifest).unwrap()).unwrap();
        assert_eq!(decoded.claude_remote_control_bindings, [binding]);
    }

    #[test]
    fn manifest_round_trips_bounded_kiro_launch_holder() {
        let holder = KiroLaunchHolderManifest {
            holder_id: uuid::Uuid::new_v4().to_string(),
            pid: 42,
            start_time: 99,
            process_group_leader: true,
            shell_id: uuid::Uuid::new_v4().to_string(),
            run_id: uuid::Uuid::new_v4().to_string(),
            sessions: vec![("session-1".into(), uuid::Uuid::new_v4().to_string())],
        };
        let manifest = Manifest {
            runtimes: Vec::new(),
            exited: Vec::new(),
            event_stream: event_stream(),
            notifications: None,
            focused_terminal: None,
            presented_focused_terminal: None,
            opencode_runtime: None,
            claude_remote_control_bindings: Vec::new(),
            kiro_launch_holders: vec![holder.clone()],
        };
        validate_manifest(&manifest).unwrap();
        let decoded: Manifest =
            serde_json::from_value(serde_json::to_value(manifest).unwrap()).unwrap();
        assert_eq!(decoded.kiro_launch_holders[0].holder_id, holder.holder_id);
        assert_eq!(decoded.kiro_launch_holders[0].sessions, holder.sessions);
    }

    #[test]
    fn maximal_kiro_holder_manifest_fits_the_control_frame() {
        let holders = (0..protocol::MAX_KIRO_LAUNCH_HOLDERS)
            .map(|holder_index| KiroLaunchHolderManifest {
                holder_id: uuid::Uuid::new_v4().to_string(),
                pid: u32::MAX,
                start_time: u64::MAX,
                process_group_leader: false,
                shell_id: uuid::Uuid::new_v4().to_string(),
                run_id: uuid::Uuid::new_v4().to_string(),
                sessions: (0..protocol::MAX_KIRO_HOLDER_SESSIONS)
                    .map(|session_index| {
                        let suffix = format!("-{holder_index}-{session_index}");
                        let escaped = (0..protocol::MAX_KIRO_SESSION_ID_BYTES
                            .saturating_sub(suffix.len()))
                            .map(|index| if index % 2 == 0 { '"' } else { '\\' })
                            .collect::<String>();
                        (
                            format!("{escaped}{suffix}"),
                            uuid::Uuid::new_v4().to_string(),
                        )
                    })
                    .collect(),
            })
            .collect::<Vec<_>>();
        assert!(
            holders
                .iter()
                .flat_map(|holder| &holder.sessions)
                .all(
                    |(session_id, _)| session_id.len() == protocol::MAX_KIRO_SESSION_ID_BYTES
                        && session_id.contains('"')
                        && session_id.contains('\\')
                )
        );
        let manifest = Manifest {
            runtimes: Vec::new(),
            exited: Vec::new(),
            event_stream: event_stream(),
            notifications: None,
            focused_terminal: None,
            presented_focused_terminal: None,
            opencode_runtime: None,
            claude_remote_control_bindings: Vec::new(),
            kiro_launch_holders: holders,
        };

        validate_manifest(&manifest).unwrap();
        let encoded = serde_json::to_vec(&manifest).unwrap();
        assert!(encoded.len() + std::mem::size_of::<u32>() <= protocol::MAX_CONTROL_FRAME);
    }
}

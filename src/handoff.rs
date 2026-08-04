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
use crate::protocol::{self, DaemonEvent, TerminalProfile};
use crate::state_store;

const HANDSHAKE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);
pub(crate) const CHANNEL_FD: RawFd = 198;
pub(crate) const HEADER: &[u8; 8] = b"BOOMUXH4";
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

#[derive(Serialize, Deserialize)]
pub(crate) struct Manifest {
    pub(crate) runtimes: Vec<RuntimeManifest>,
    pub(crate) exited: Vec<ExitedManifest>,
    pub(crate) event_stream: EventStreamManifest,
}

#[derive(Clone, Serialize, Deserialize)]
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

pub(crate) enum Bootstrap {
    Aborted,
    Committed {
        channel: UnixStream,
        listener: UnixListener,
        runtime_lock: OwnedFd,
        state_lock: OwnedFd,
        runtimes: Vec<TransferredRuntime>,
        exited: Vec<TransferredExited>,
        event_stream: EventStreamManifest,
    },
}

pub(crate) fn receive_bootstrap(channel: RawFd) -> io::Result<Bootstrap> {
    let mut channel = adopt_channel(channel)?;
    channel.set_read_timeout(Some(HANDSHAKE_TIMEOUT))?;
    channel.set_write_timeout(Some(HANDSHAKE_TIMEOUT))?;
    let mut header = [0; HEADER.len()];
    channel.read_exact(&mut header)?;
    if &header != HEADER {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "replacement bootstrap version is unsupported",
        ));
    }
    let manifest: Manifest = protocol::read_message(&mut channel)?;
    validate_manifest(&manifest)?;
    let event_stream = manifest.event_stream.clone();
    let listener = receive_descriptor(&channel, LISTENER_MARKER)?;
    let runtime_lock = receive_descriptor(&channel, RUNTIME_LOCK_MARKER)?;
    let state_lock = receive_descriptor(&channel, STATE_LOCK_MARKER)?;
    let mut runtimes = Vec::with_capacity(manifest.runtimes.len());
    for manifest in manifest.runtimes {
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
    let mut exited = Vec::with_capacity(manifest.exited.len());
    for manifest in manifest.exited {
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
            event_stream,
        }),
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "replacement bootstrap received an unsupported decision",
        )),
    }
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

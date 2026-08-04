use std::fs;
use std::io::{self, Read, Write};
use std::mem::MaybeUninit;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::fs::MetadataExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;

use crate::client;
use crate::fd_transfer::receive_descriptor;
use crate::state_store;

const HANDSHAKE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);
pub(crate) const CHANNEL_FD: RawFd = 198;
pub(crate) const HEADER: &[u8; 8] = b"BOOMUXH1";
pub(crate) const LISTENER_MARKER: u8 = 1;
pub(crate) const RUNTIME_LOCK_MARKER: u8 = 2;
pub(crate) const STATE_LOCK_MARKER: u8 = 3;
pub(crate) const READY: u8 = 4;
pub(crate) const ABORT: u8 = 5;
pub(crate) const COMMIT: u8 = 6;
pub(crate) const PREPARED: u8 = 7;
pub(crate) const FINALIZE: u8 = 8;
pub(crate) const COMMITTED: u8 = 9;

pub(crate) enum Bootstrap {
    Aborted,
    Committed {
        channel: UnixStream,
        listener: UnixListener,
        runtime_lock: OwnedFd,
        state_lock: OwnedFd,
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
    let listener = receive_descriptor(&channel, LISTENER_MARKER)?;
    let runtime_lock = receive_descriptor(&channel, RUNTIME_LOCK_MARKER)?;
    let state_lock = receive_descriptor(&channel, STATE_LOCK_MARKER)?;

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
        }),
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "replacement bootstrap received an unsupported decision",
        )),
    }
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

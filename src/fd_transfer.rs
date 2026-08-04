use std::io::{self, IoSlice, IoSliceMut};
use std::os::fd::{AsRawFd, BorrowedFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::net::UnixStream;

use nix::errno::Errno;
use nix::sys::socket::{ControlMessage, ControlMessageOwned, MsgFlags, recvmsg, sendmsg};

pub(crate) fn send_descriptor(
    stream: &UnixStream,
    descriptor: BorrowedFd<'_>,
    marker: u8,
) -> io::Result<()> {
    let marker = [marker];
    let data = [IoSlice::new(&marker)];
    let descriptors = [descriptor.as_raw_fd()];
    let control = [ControlMessage::ScmRights(&descriptors)];
    loop {
        match sendmsg::<()>(
            stream.as_raw_fd(),
            &data,
            &control,
            MsgFlags::MSG_NOSIGNAL,
            None,
        ) {
            Ok(1) => return Ok(()),
            Ok(count) => {
                return Err(io::Error::new(
                    io::ErrorKind::WriteZero,
                    format!("descriptor marker write returned {count} bytes"),
                ));
            }
            Err(Errno::EINTR) => continue,
            Err(error) => return Err(errno(error)),
        }
    }
}

pub(crate) fn receive_descriptor(stream: &UnixStream, expected_marker: u8) -> io::Result<OwnedFd> {
    let mut marker = [0_u8];
    let mut data = [IoSliceMut::new(&mut marker)];
    let mut control = nix::cmsg_space!([RawFd; 1]);
    let (bytes, flags, descriptors) = loop {
        match recvmsg::<()>(
            stream.as_raw_fd(),
            &mut data,
            Some(&mut control),
            MsgFlags::MSG_CMSG_CLOEXEC,
        ) {
            Ok(message) => {
                let mut descriptors = Vec::new();
                for control in message.cmsgs() {
                    match control {
                        ControlMessageOwned::ScmRights(received) => {
                            descriptors.extend(received.into_iter().map(|descriptor| {
                                // SCM_RIGHTS returned a new descriptor owned by this process.
                                unsafe { OwnedFd::from_raw_fd(descriptor) }
                            }));
                        }
                        _ => {
                            return Err(io::Error::new(
                                io::ErrorKind::InvalidData,
                                "descriptor transfer contained unexpected control data",
                            ));
                        }
                    }
                }
                break (message.bytes, message.flags, descriptors);
            }
            Err(Errno::EINTR) => continue,
            Err(error) => return Err(errno(error)),
        }
    };

    if bytes == 0 {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "descriptor transfer closed before its marker",
        ));
    }
    if bytes != 1
        || marker[0] != expected_marker
        || flags.intersects(MsgFlags::MSG_TRUNC | MsgFlags::MSG_CTRUNC)
        || descriptors.len() != 1
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid descriptor transfer",
        ));
    }
    descriptors
        .into_iter()
        .next()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "descriptor was not received"))
}

fn errno(error: Errno) -> io::Error {
    io::Error::from_raw_os_error(error as i32)
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Write};
    use std::os::fd::AsFd;

    use super::*;
    use nix::sys::socket::{ControlMessage, MsgFlags, sendmsg};

    const MARKER: u8 = 0x42;

    #[test]
    fn transfers_owned_cloexec_descriptor() {
        let (sender, receiver) = UnixStream::pair().unwrap();
        let (payload, mut peer) = UnixStream::pair().unwrap();

        send_descriptor(&sender, payload.as_fd(), MARKER).unwrap();
        let descriptor = receive_descriptor(&receiver, MARKER).unwrap();
        drop(payload);

        let flags = unsafe { libc::fcntl(descriptor.as_raw_fd(), libc::F_GETFD) };
        assert_ne!(flags, -1);
        assert_ne!(flags & libc::FD_CLOEXEC, 0);
        let mut payload = UnixStream::from(descriptor);
        peer.write_all(b"handoff").unwrap();
        let mut received = [0; 7];
        payload.read_exact(&mut received).unwrap();
        assert_eq!(&received, b"handoff");
    }

    #[test]
    fn rejects_missing_descriptor() {
        let (mut sender, receiver) = UnixStream::pair().unwrap();
        sender.write_all(&[MARKER]).unwrap();

        let error = receive_descriptor(&receiver, MARKER).unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn rejects_wrong_marker() {
        let (sender, receiver) = UnixStream::pair().unwrap();
        let (payload, _peer) = UnixStream::pair().unwrap();
        send_descriptor(&sender, payload.as_fd(), MARKER).unwrap();

        let error = receive_descriptor(&receiver, MARKER + 1).unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn rejects_multiple_descriptors() {
        let (sender, receiver) = UnixStream::pair().unwrap();
        let (first, _first_peer) = UnixStream::pair().unwrap();
        let (second, _second_peer) = UnixStream::pair().unwrap();
        let marker = [MARKER];
        let data = [IoSlice::new(&marker)];
        let descriptors = [first.as_raw_fd(), second.as_raw_fd()];
        let control = [ControlMessage::ScmRights(&descriptors)];
        sendmsg::<()>(
            sender.as_raw_fd(),
            &data,
            &control,
            MsgFlags::MSG_NOSIGNAL,
            None,
        )
        .unwrap();

        let error = receive_descriptor(&receiver, MARKER).unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    }
}

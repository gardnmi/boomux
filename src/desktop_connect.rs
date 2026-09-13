//! One-shot, launch-owned result channel for Desktop's interactive remote setup.
//! No daemon state or terminal output is used to infer which Shell was created.
use crate::protocol::QualifiedIdentity;
use std::fs::{self, DirBuilder};
use std::io;
use std::os::unix::fs::DirBuilderExt;
use std::os::unix::net::UnixDatagram;
use std::path::{Path, PathBuf};

const MAX_RESULT_BYTES: usize = 1024;

pub struct ConnectResultReceiver {
    directory: PathBuf,
    socket: UnixDatagram,
}

impl ConnectResultReceiver {
    pub fn new() -> io::Result<Self> {
        // Keep sockaddr_un short even when the user's runtime/home path is long.
        let directory =
            PathBuf::from("/tmp").join(format!("boomux-connect-{}", uuid::Uuid::new_v4()));
        DirBuilder::new().mode(0o700).create(&directory)?;
        let socket = match UnixDatagram::bind(directory.join("result")) {
            Ok(socket) => socket,
            Err(error) => {
                let _ = fs::remove_dir(&directory);
                return Err(error);
            }
        };
        let receiver = Self { directory, socket };
        receiver.socket.set_nonblocking(true)?;
        Ok(receiver)
    }

    pub fn path(&self) -> PathBuf {
        self.directory.join("result")
    }

    /// Consume once, after the exact setup Shell has been acknowledged and removed.
    pub fn receive(self) -> io::Result<Option<QualifiedIdentity>> {
        let mut bytes = [0; MAX_RESULT_BYTES + 1];
        match self.socket.recv(&mut bytes) {
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => Ok(None),
            Err(error) => Err(error),
            Ok(count) if count > MAX_RESULT_BYTES => {
                Err(io::Error::other("Remote setup result is too large"))
            }
            Ok(count) => {
                let result: QualifiedIdentity = serde_json::from_slice(&bytes[..count])?;
                if result.node_id.is_empty() || result.inner_id.is_empty() {
                    return Err(io::Error::other(
                        "Remote setup result has no Shell identity",
                    ));
                }
                Ok(Some(result))
            }
        }
    }
}

impl Drop for ConnectResultReceiver {
    fn drop(&mut self) {
        let _ = fs::remove_file(self.path());
        let _ = fs::remove_dir(&self.directory);
    }
}

pub fn send_result(path: &Path, identity: &QualifiedIdentity) -> io::Result<()> {
    let bytes = serde_json::to_vec(identity)?;
    if bytes.len() > MAX_RESULT_BYTES {
        return Err(io::Error::other("Remote setup result is too large"));
    }
    let socket = UnixDatagram::unbound()?;
    socket.set_nonblocking(true)?;
    socket.send_to(&bytes, path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn setup_results_are_exact_isolated_and_removed_after_consumption() {
        let first = ConnectResultReceiver::new().unwrap();
        let second = ConnectResultReceiver::new().unwrap();
        let path = first.path();
        let identity = QualifiedIdentity {
            node_id: "owner".into(),
            inner_id: "created-shell".into(),
        };
        send_result(&path, &identity).unwrap();
        assert!(second.receive().unwrap().is_none());
        assert_eq!(first.receive().unwrap(), Some(identity));
        assert!(!path.parent().unwrap().exists());
    }

    #[test]
    fn invalid_or_oversized_results_are_rejected_and_cleaned_up() {
        for bytes in [
            b"not json".to_vec(),
            vec![b'x'; MAX_RESULT_BYTES + 1],
            br#"{"node_id":"","inner_id":"s"}"#.to_vec(),
        ] {
            let receiver = ConnectResultReceiver::new().unwrap();
            let path = receiver.path();
            UnixDatagram::unbound()
                .unwrap()
                .send_to(&bytes, &path)
                .unwrap();
            assert!(receiver.receive().is_err());
            assert!(!path.parent().unwrap().exists());
        }
    }

    #[test]
    fn cancelled_setup_releases_its_result_channel() {
        let receiver = ConnectResultReceiver::new().unwrap();
        let path = receiver.path();
        drop(receiver);
        assert!(!path.parent().unwrap().exists());
        assert!(
            send_result(
                &path,
                &QualifiedIdentity {
                    node_id: "n".into(),
                    inner_id: "s".into()
                }
            )
            .is_err()
        );
    }
}

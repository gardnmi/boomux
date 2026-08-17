use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::state_store::{effective_uid, secure_state_dir, state_directory_from_environment};

const NODE_IDENTITY_VERSION: u32 = 1;
const MAX_NODE_IDENTITY_BYTES: u64 = 4 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct NodeIdentity {
    id: String,
}

impl NodeIdentity {
    pub(crate) fn load_or_create_from_environment() -> io::Result<Self> {
        Self::load_or_create_at(state_directory_from_environment()?.join("node.json"))
    }

    pub(crate) fn id(&self) -> &str {
        &self.id
    }

    fn load_or_create_at(path: PathBuf) -> io::Result<Self> {
        let parent = path
            .parent()
            .ok_or_else(|| io::Error::other("Node identity path has no parent"))?;
        secure_state_dir(parent)?;
        let _lock = acquire_identity_lock(parent)?;
        match load(&path) {
            Ok(identity) => Ok(identity),
            Err(error) if error.kind() == io::ErrorKind::NotFound => create(&path),
            Err(error) => Err(error),
        }
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PersistedNodeIdentity {
    version: u32,
    node_id: String,
}

fn acquire_identity_lock(parent: &Path) -> io::Result<File> {
    let path = parent.join("node.lock");
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata.uid() != effective_uid() {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "Boomux Node identity lock is not an owned regular file",
        ));
    }
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;
    // The file remains open while `flock` is held and the call takes no pointers.
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(file)
}

fn load(path: &Path) -> io::Result<NodeIdentity> {
    let mut file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata.uid() != effective_uid() {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "Boomux Node identity is not an owned regular file",
        ));
    }
    if metadata.mode() & 0o077 != 0 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "Boomux Node identity is not owner-only",
        ));
    }
    if metadata.len() > MAX_NODE_IDENTITY_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Boomux Node identity exceeds the size limit",
        ));
    }
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    file.read_to_end(&mut bytes)?;
    let persisted: PersistedNodeIdentity = serde_json::from_slice(&bytes).map_err(|error| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("could not parse Boomux Node identity: {error}"),
        )
    })?;
    if persisted.version != NODE_IDENTITY_VERSION {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "unsupported Boomux Node identity version {}; expected {NODE_IDENTITY_VERSION}",
                persisted.version
            ),
        ));
    }
    let parsed = Uuid::parse_str(&persisted.node_id).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "Boomux Node identity contains an invalid Node ID",
        )
    })?;
    if parsed.to_string() != persisted.node_id {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Boomux Node identity contains a noncanonical Node ID",
        ));
    }
    Ok(NodeIdentity {
        id: persisted.node_id,
    })
}

fn create(path: &Path) -> io::Result<NodeIdentity> {
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::other("Node identity path has no parent"))?;
    let identity = NodeIdentity {
        id: Uuid::new_v4().to_string(),
    };
    let bytes = serde_json::to_vec_pretty(&PersistedNodeIdentity {
        version: NODE_IDENTITY_VERSION,
        node_id: identity.id.clone(),
    })
    .map_err(io::Error::other)?;
    let temporary = parent.join(format!(".node-{}.tmp", Uuid::new_v4()));
    let result = (|| {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temporary)?;
        file.write_all(&bytes)?;
        file.sync_all()?;
        match fs::hard_link(&temporary, path) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => return load(path),
            Err(error) => return Err(error),
        }
        let _ = File::open(parent).and_then(|directory| directory.sync_all());
        Ok(identity)
    })();
    let _ = fs::remove_file(&temporary);
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Barrier};
    use std::thread;

    fn test_path() -> PathBuf {
        std::env::temp_dir()
            .join(format!("boomux-node-{}", Uuid::new_v4()))
            .join("boomux/node.json")
    }

    #[test]
    fn creates_and_reloads_one_owner_only_identity() {
        let path = test_path();
        let first = NodeIdentity::load_or_create_at(path.clone()).unwrap();
        let second = NodeIdentity::load_or_create_at(path.clone()).unwrap();
        assert_eq!(first, second);
        assert_eq!(fs::symlink_metadata(&path).unwrap().mode() & 0o777, 0o600);
        assert_eq!(
            fs::symlink_metadata(path.parent().unwrap()).unwrap().mode() & 0o777,
            0o700
        );
        fs::remove_dir_all(path.parent().unwrap().parent().unwrap()).unwrap();
    }

    #[test]
    fn concurrent_creation_returns_one_identity() {
        let path = test_path();
        let barrier = Arc::new(Barrier::new(8));
        let handles = (0..8)
            .map(|_| {
                let path = path.clone();
                let barrier = Arc::clone(&barrier);
                thread::spawn(move || {
                    barrier.wait();
                    NodeIdentity::load_or_create_at(path).unwrap()
                })
            })
            .collect::<Vec<_>>();
        let identities = handles
            .into_iter()
            .map(|handle| handle.join().unwrap())
            .collect::<Vec<_>>();
        assert!(identities.iter().all(|identity| identity == &identities[0]));
        fs::remove_dir_all(path.parent().unwrap().parent().unwrap()).unwrap();
    }

    #[test]
    fn malformed_identity_is_preserved() {
        let path = test_path();
        secure_state_dir(path.parent().unwrap()).unwrap();
        fs::write(&path, b"not-json").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        let error = NodeIdentity::load_or_create_at(path.clone()).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert_eq!(fs::read(&path).unwrap(), b"not-json");
        fs::remove_dir_all(path.parent().unwrap().parent().unwrap()).unwrap();
    }

    #[test]
    fn unsupported_identity_is_preserved() {
        let path = test_path();
        secure_state_dir(path.parent().unwrap()).unwrap();
        let bytes = serde_json::to_vec(&PersistedNodeIdentity {
            version: NODE_IDENTITY_VERSION + 1,
            node_id: Uuid::new_v4().to_string(),
        })
        .unwrap();
        fs::write(&path, &bytes).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        let error = NodeIdentity::load_or_create_at(path.clone()).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert_eq!(fs::read(&path).unwrap(), bytes);
        fs::remove_dir_all(path.parent().unwrap().parent().unwrap()).unwrap();
    }

    #[test]
    fn rejects_non_owner_only_identity() {
        let path = test_path();
        secure_state_dir(path.parent().unwrap()).unwrap();
        fs::write(
            &path,
            serde_json::to_vec(&PersistedNodeIdentity {
                version: NODE_IDENTITY_VERSION,
                node_id: Uuid::new_v4().to_string(),
            })
            .unwrap(),
        )
        .unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        let error = NodeIdentity::load_or_create_at(path.clone()).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        fs::remove_dir_all(path.parent().unwrap().parent().unwrap()).unwrap();
    }
}

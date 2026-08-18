use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::state_store::{effective_uid, secure_state_dir, state_directory_from_environment};

const NODE_IDENTITY_VERSION: u32 = 2;
const MAX_NODE_IDENTITY_BYTES: u64 = 4 * 1024;
const MAX_NODE_ALIAS_BYTES: usize = 128;

pub(crate) const DEFAULT_NODE_ALIAS: &str = "local";

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct NodeIdentity {
    id: String,
    alias: String,
    revision: u64,
}

pub(crate) struct NodeIdentityManager {
    path: PathBuf,
    state: Mutex<NodeIdentityState>,
    changed: Condvar,
}

struct NodeIdentityState {
    identity: NodeIdentity,
    admission_open: bool,
    admitted: usize,
}

pub(crate) struct NodeIdentityLease {
    manager: Arc<NodeIdentityManager>,
}

impl NodeIdentity {
    pub(crate) fn load_or_create_from_environment() -> io::Result<Self> {
        Self::load_or_create_at(state_directory_from_environment()?.join("node.json"))
    }

    pub(crate) fn id(&self) -> &str {
        &self.id
    }

    pub(crate) fn alias(&self) -> &str {
        &self.alias
    }

    pub(crate) fn revision(&self) -> u64 {
        self.revision
    }

    fn load_or_create_at(path: PathBuf) -> io::Result<Self> {
        let parent = path
            .parent()
            .ok_or_else(|| io::Error::other("Node identity path has no parent"))?;
        secure_state_dir(parent)?;
        let _lock = acquire_identity_lock(parent)?;
        match load_versioned(&path) {
            Ok(LoadedNodeIdentity::Current(identity)) => Ok(identity),
            Ok(LoadedNodeIdentity::Legacy(identity)) => {
                replace_file(&path, &identity)?;
                Ok(identity)
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => create(&path),
            Err(error) => Err(error),
        }
    }
}

impl NodeIdentityManager {
    pub(crate) fn load_or_create_from_environment() -> io::Result<Arc<Self>> {
        let path = state_directory_from_environment()?.join("node.json");
        let identity = NodeIdentity::load_or_create_at(path.clone())?;
        Ok(Arc::new(Self {
            path,
            state: Mutex::new(NodeIdentityState {
                identity,
                admission_open: true,
                admitted: 0,
            }),
            changed: Condvar::new(),
        }))
    }

    pub(crate) fn id(&self) -> io::Result<String> {
        Ok(self.lock_state()?.identity.id.clone())
    }

    pub(crate) fn metadata(&self) -> io::Result<NodeIdentity> {
        Ok(self.lock_state()?.identity.clone())
    }

    pub(crate) fn rename_alias(
        &self,
        alias: String,
        expected_revision: u64,
    ) -> io::Result<NodeIdentity> {
        validate_node_alias(&alias)?;
        let mut state = self.lock_state()?;
        if state.identity.revision != expected_revision {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "local Node alias revision changed: expected {expected_revision}, current {}",
                    state.identity.revision
                ),
            ));
        }
        if state.identity.alias == alias {
            return Ok(state.identity.clone());
        }
        let revision = state
            .identity
            .revision
            .checked_add(1)
            .ok_or_else(|| io::Error::other("local Node alias revision overflow"))?;
        let replacement = NodeIdentity {
            id: state.identity.id.clone(),
            alias,
            revision,
        };
        persist_replacement(&self.path, &state.identity, &replacement)?;
        state.identity = replacement.clone();
        Ok(replacement)
    }

    pub(crate) fn admit(self: &Arc<Self>) -> io::Result<NodeIdentityLease> {
        let mut state = self.lock_state()?;
        if !state.admission_open {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "Boomux Node federation admission is closed",
            ));
        }
        state.admitted = state
            .admitted
            .checked_add(1)
            .ok_or_else(|| io::Error::other("Node federation admission count overflow"))?;
        drop(state);
        Ok(NodeIdentityLease {
            manager: Arc::clone(self),
        })
    }

    pub(crate) fn rekey(&self, expected_node_id: &str, timeout: Duration) -> io::Result<String> {
        let mut state = self.lock_state()?;
        if state.identity.id != expected_node_id {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "current Node identity does not match the confirmed Node ID",
            ));
        }
        if !state.admission_open {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "Boomux Node federation admission is already closed",
            ));
        }
        state.admission_open = false;
        let deadline = Instant::now() + timeout;
        while state.admitted != 0 {
            let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
                state.admission_open = true;
                self.changed.notify_all();
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "timed out draining active federation channels",
                ));
            };
            let (next, wait) = self
                .changed
                .wait_timeout(state, remaining)
                .map_err(|_| io::Error::other("Node identity lock is poisoned"))?;
            state = next;
            if wait.timed_out() && state.admitted != 0 {
                state.admission_open = true;
                self.changed.notify_all();
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "timed out draining active federation channels",
                ));
            }
        }

        let replacement = match replace(&self.path, expected_node_id) {
            Ok(identity) => identity,
            Err(error) => {
                state.admission_open = true;
                self.changed.notify_all();
                return Err(error);
            }
        };
        state.identity = replacement;
        state.admission_open = true;
        let node_id = state.identity.id.clone();
        self.changed.notify_all();
        Ok(node_id)
    }

    fn lock_state(&self) -> io::Result<std::sync::MutexGuard<'_, NodeIdentityState>> {
        self.state
            .lock()
            .map_err(|_| io::Error::other("Node identity lock is poisoned"))
    }
}

impl Drop for NodeIdentityLease {
    fn drop(&mut self) {
        if let Ok(mut state) = self.manager.state.lock() {
            state.admitted = state.admitted.saturating_sub(1);
            self.manager.changed.notify_all();
        }
    }
}

pub(crate) fn validate_node_alias(alias: &str) -> io::Result<()> {
    if alias.is_empty()
        || alias.len() > MAX_NODE_ALIAS_BYTES
        || alias.chars().any(char::is_control)
        || alias.chars().any(char::is_whitespace)
        || Uuid::parse_str(alias).is_ok_and(|id| id.to_string() == alias)
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Node alias must be bounded, nonempty, whitespace-free, and not a canonical Node ID",
        ));
    }
    Ok(())
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PersistedNodeIdentity {
    version: u32,
    node_id: String,
    alias: String,
    revision: u64,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PersistedNodeIdentityV1 {
    #[serde(rename = "version")]
    _version: u32,
    node_id: String,
}

enum LoadedNodeIdentity {
    Current(NodeIdentity),
    Legacy(NodeIdentity),
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
    match load_versioned(path)? {
        LoadedNodeIdentity::Current(identity) | LoadedNodeIdentity::Legacy(identity) => {
            Ok(identity)
        }
    }
}

fn load_versioned(path: &Path) -> io::Result<LoadedNodeIdentity> {
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
    let value: serde_json::Value = serde_json::from_slice(&bytes).map_err(|error| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("could not parse Boomux Node identity: {error}"),
        )
    })?;
    let version = value.get("version").and_then(serde_json::Value::as_u64);
    let (identity, legacy) = match version {
        Some(1) => {
            let persisted: PersistedNodeIdentityV1 =
                serde_json::from_value(value).map_err(|error| {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("could not parse Boomux Node identity: {error}"),
                    )
                })?;
            (
                NodeIdentity {
                    id: persisted.node_id,
                    alias: DEFAULT_NODE_ALIAS.into(),
                    revision: 1,
                },
                true,
            )
        }
        Some(version) if version == u64::from(NODE_IDENTITY_VERSION) => {
            let persisted: PersistedNodeIdentity =
                serde_json::from_value(value).map_err(|error| {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("could not parse Boomux Node identity: {error}"),
                    )
                })?;
            validate_node_alias(&persisted.alias)
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error.to_string()))?;
            if persisted.revision == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "Boomux Node identity contains an invalid alias revision",
                ));
            }
            (
                NodeIdentity {
                    id: persisted.node_id,
                    alias: persisted.alias,
                    revision: persisted.revision,
                },
                false,
            )
        }
        Some(version) => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "unsupported Boomux Node identity version {version}; expected {NODE_IDENTITY_VERSION}"
                ),
            ));
        }
        None => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Boomux Node identity is missing its version",
            ));
        }
    };
    let parsed = Uuid::parse_str(&identity.id).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "Boomux Node identity contains an invalid Node ID",
        )
    })?;
    if parsed.to_string() != identity.id {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Boomux Node identity contains a noncanonical Node ID",
        ));
    }
    if legacy {
        Ok(LoadedNodeIdentity::Legacy(identity))
    } else {
        Ok(LoadedNodeIdentity::Current(identity))
    }
}

fn create(path: &Path) -> io::Result<NodeIdentity> {
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::other("Node identity path has no parent"))?;
    let identity = NodeIdentity {
        id: Uuid::new_v4().to_string(),
        alias: DEFAULT_NODE_ALIAS.into(),
        revision: 1,
    };
    let bytes = serde_json::to_vec_pretty(&PersistedNodeIdentity {
        version: NODE_IDENTITY_VERSION,
        node_id: identity.id.clone(),
        alias: identity.alias.clone(),
        revision: identity.revision,
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

fn replace(path: &Path, expected_node_id: &str) -> io::Result<NodeIdentity> {
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::other("Node identity path has no parent"))?;
    secure_state_dir(parent)?;
    let _lock = acquire_identity_lock(parent)?;
    let current = load(path)?;
    if current.id != expected_node_id {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "persisted Node identity changed before rekey",
        ));
    }
    let identity = NodeIdentity {
        id: Uuid::new_v4().to_string(),
        alias: current.alias,
        revision: current.revision,
    };
    replace_file(path, &identity)?;
    Ok(identity)
}

fn persist_replacement(
    path: &Path,
    expected: &NodeIdentity,
    replacement: &NodeIdentity,
) -> io::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::other("Node identity path has no parent"))?;
    secure_state_dir(parent)?;
    let _lock = acquire_identity_lock(parent)?;
    if load(path)? != *expected {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "persisted Node identity changed before alias rename",
        ));
    }
    replace_file(path, replacement)
}

fn replace_file(path: &Path, identity: &NodeIdentity) -> io::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::other("Node identity path has no parent"))?;
    let bytes = serde_json::to_vec_pretty(&PersistedNodeIdentity {
        version: NODE_IDENTITY_VERSION,
        node_id: identity.id.clone(),
        alias: identity.alias.clone(),
        revision: identity.revision,
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
        fs::rename(&temporary, path)?;
        let _ = File::open(parent).and_then(|directory| directory.sync_all());
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
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
    fn migrates_schema_one_with_the_default_alias() {
        let path = test_path();
        secure_state_dir(path.parent().unwrap()).unwrap();
        let node_id = Uuid::new_v4().to_string();
        fs::write(
            &path,
            serde_json::to_vec(&PersistedNodeIdentityV1 {
                _version: 1,
                node_id: node_id.clone(),
            })
            .unwrap(),
        )
        .unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();

        let identity = NodeIdentity::load_or_create_at(path.clone()).unwrap();
        assert_eq!(identity.id(), node_id);
        assert_eq!(identity.alias(), DEFAULT_NODE_ALIAS);
        assert_eq!(identity.revision(), 1);
        let persisted: PersistedNodeIdentity =
            serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(persisted.version, NODE_IDENTITY_VERSION);
        assert_eq!(persisted.alias, DEFAULT_NODE_ALIAS);
        fs::remove_dir_all(path.parent().unwrap().parent().unwrap()).unwrap();
    }

    #[test]
    fn alias_rename_is_revision_guarded_and_survives_rekey() {
        let path = test_path();
        let identity = NodeIdentity::load_or_create_at(path.clone()).unwrap();
        let manager = NodeIdentityManager {
            path: path.clone(),
            state: Mutex::new(NodeIdentityState {
                identity: identity.clone(),
                admission_open: true,
                admitted: 0,
            }),
            changed: Condvar::new(),
        };
        let renamed = manager.rename_alias("desktop".into(), 1).unwrap();
        assert_eq!(renamed.alias(), "desktop");
        assert_eq!(renamed.revision(), 2);
        assert_eq!(
            manager.rename_alias("stale".into(), 1).unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
        let replacement = manager
            .rekey(identity.id(), Duration::from_secs(1))
            .unwrap();
        let reloaded = load(&path).unwrap();
        assert_eq!(reloaded.id(), replacement);
        assert_eq!(reloaded.alias(), "desktop");
        assert_eq!(reloaded.revision(), 2);
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
            alias: DEFAULT_NODE_ALIAS.into(),
            revision: 1,
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
                alias: DEFAULT_NODE_ALIAS.into(),
                revision: 1,
            })
            .unwrap(),
        )
        .unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        let error = NodeIdentity::load_or_create_at(path.clone()).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        fs::remove_dir_all(path.parent().unwrap().parent().unwrap()).unwrap();
    }

    #[test]
    fn rekey_drains_admission_and_preserves_old_identity_on_timeout() {
        let path = test_path();
        let identity = NodeIdentity::load_or_create_at(path.clone()).unwrap();
        let manager = Arc::new(NodeIdentityManager {
            path: path.clone(),
            state: Mutex::new(NodeIdentityState {
                identity: identity.clone(),
                admission_open: true,
                admitted: 0,
            }),
            changed: Condvar::new(),
        });
        let lease = manager.admit().unwrap();
        let error = manager
            .rekey(identity.id(), Duration::from_millis(1))
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert_eq!(manager.id().unwrap(), identity.id());
        assert_eq!(load(&path).unwrap(), identity);

        drop(lease);
        let replacement = manager
            .rekey(identity.id(), Duration::from_secs(1))
            .unwrap();
        assert_ne!(replacement, identity.id());
        assert_eq!(manager.id().unwrap(), replacement);
        assert_eq!(load(&path).unwrap().id(), replacement);
        fs::remove_dir_all(path.parent().unwrap().parent().unwrap()).unwrap();
    }
}

use std::env;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::protocol::TerminalProfile;

const STATE_VERSION: u32 = 1;
const MAX_STATE_BYTES: u64 = 8 * 1024 * 1024;

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PersistedState {
    version: u32,
    pub(crate) workspaces: Vec<PersistedWorkspace>,
}

impl Default for PersistedState {
    fn default() -> Self {
        Self {
            version: STATE_VERSION,
            workspaces: Vec::new(),
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PersistedWorkspace {
    pub(crate) id: String,
    pub(crate) name: String,
    pub(crate) shells: Vec<PersistedShell>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PersistedShell {
    pub(crate) id: String,
    pub(crate) name: String,
    pub(crate) cwd: PathBuf,
    pub(crate) command: Vec<String>,
    pub(crate) last_profile: Option<TerminalProfile>,
}

pub(crate) struct StateStore {
    path: PathBuf,
    _lock: Option<File>,
}

impl StateStore {
    pub(crate) fn from_environment() -> io::Result<Self> {
        let root = match env::var_os("XDG_STATE_HOME").filter(|path| !path.is_empty()) {
            Some(path) => PathBuf::from(path),
            None => PathBuf::from(
                env::var_os("HOME")
                    .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "HOME is not set"))?,
            )
            .join(".local/state"),
        };
        if !root.is_absolute() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "XDG_STATE_HOME must be an absolute path",
            ));
        }
        let directory = root.join("boomux");
        secure_state_dir(&directory)?;
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(directory.join("daemon.lock"))?;
        // The descriptor remains open for the store lifetime and `flock` takes
        // only pointer-free integer arguments.
        if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == -1 {
            let error = io::Error::last_os_error();
            return Err(if matches!(error.raw_os_error(), Some(libc::EWOULDBLOCK)) {
                io::Error::new(
                    io::ErrorKind::AddrInUse,
                    "another Boomux daemon is using this state directory",
                )
            } else {
                error
            });
        }
        Ok(Self {
            path: directory.join("state.json"),
            _lock: Some(lock),
        })
    }

    #[cfg(test)]
    pub(crate) fn at(path: PathBuf) -> Self {
        Self { path, _lock: None }
    }

    pub(crate) fn load(&self) -> io::Result<Option<PersistedState>> {
        let Some(parent) = self.path.parent() else {
            return Err(io::Error::other("state path has no parent"));
        };
        secure_state_dir(parent)?;
        let metadata = match fs::symlink_metadata(&self.path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error),
        };
        if !metadata.is_file() || metadata.uid() != effective_uid() {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "boomux state path is not an owned regular file",
            ));
        }
        if metadata.len() > MAX_STATE_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "boomux state file exceeds the size limit",
            ));
        }
        let mut bytes = Vec::with_capacity(metadata.len() as usize);
        File::open(&self.path)?.read_to_end(&mut bytes)?;
        let state: PersistedState = serde_json::from_slice(&bytes).map_err(|error| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("could not parse {}: {error}", self.path.display()),
            )
        })?;
        if state.version != STATE_VERSION {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "unsupported Boomux state version {}; expected {STATE_VERSION}",
                    state.version
                ),
            ));
        }
        Ok(Some(state))
    }

    pub(crate) fn save(&self, state: &PersistedState) -> io::Result<()> {
        let Some(parent) = self.path.parent() else {
            return Err(io::Error::other("state path has no parent"));
        };
        secure_state_dir(parent)?;
        let bytes = serde_json::to_vec_pretty(state).map_err(io::Error::other)?;
        if bytes.len() as u64 > MAX_STATE_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "boomux state exceeds the size limit",
            ));
        }
        let temporary = parent.join(format!(".state-{}.tmp", Uuid::new_v4()));
        let result = (|| {
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&temporary)?;
            file.write_all(&bytes)?;
            file.sync_all()?;
            fs::rename(&temporary, &self.path)?;
            // Rename is the commit point. Directory fsync improves crash
            // durability but cannot be rolled back if a filesystem rejects it.
            let _ = File::open(parent).and_then(|directory| directory.sync_all());
            Ok(())
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temporary);
        }
        result
    }
}

fn secure_state_dir(path: &Path) -> io::Result<()> {
    fs::create_dir_all(path)?;
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.is_dir() || metadata.uid() != effective_uid() {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "boomux state path is not an owned directory",
        ));
    }
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
}

fn effective_uid() -> u32 {
    // `geteuid` has no arguments, pointers, or caller safety requirements.
    unsafe { libc::geteuid() }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn atomically_round_trips_state() {
        let directory = env::temp_dir().join(format!("boomux-state-{}", Uuid::new_v4()));
        let store = StateStore::at(directory.join("boomux/state.json"));
        let state = PersistedState {
            version: STATE_VERSION,
            workspaces: vec![PersistedWorkspace {
                id: Uuid::new_v4().to_string(),
                name: "saved".into(),
                shells: Vec::new(),
            }],
        };

        store.save(&state).unwrap();
        let restored = store.load().unwrap().unwrap();

        assert_eq!(restored.workspaces[0].name, "saved");
        assert_eq!(
            fs::metadata(directory.join("boomux/state.json"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn rejects_unsupported_state_versions() {
        let directory = env::temp_dir().join(format!("boomux-state-{}", Uuid::new_v4()));
        let path = directory.join("boomux/state.json");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, br#"{"version":99,"workspaces":[]}"#).unwrap();
        let store = StateStore::at(path);

        let error = store.load().unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        fs::remove_dir_all(directory).unwrap();
    }
}

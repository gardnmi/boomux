use std::env;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use uuid::Uuid;

const VERSION: u32 = 1;
const MAX_BYTES: u64 = 4 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct WorkspaceSelection {
    workspace_id: String,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PersistedSelection {
    version: u32,
    workspace_id: String,
}

impl WorkspaceSelection {
    pub(crate) fn new(workspace_id: impl Into<String>) -> io::Result<Self> {
        let workspace_id = workspace_id.into();
        let parsed = Uuid::parse_str(&workspace_id).map_err(|_| {
            io::Error::new(io::ErrorKind::InvalidInput, "Workspace ID must be a UUID")
        })?;
        if parsed.to_string() != workspace_id {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Workspace ID must be a canonical UUID",
            ));
        }
        Ok(Self { workspace_id })
    }

    pub(crate) fn workspace_id(&self) -> &str {
        &self.workspace_id
    }
}

pub(crate) fn load_from_environment() -> io::Result<Option<WorkspaceSelection>> {
    load(&selection_path_from_environment()?)
}

pub(crate) fn save_from_environment(selection: &WorkspaceSelection) -> io::Result<()> {
    save(&selection_path_from_environment()?, selection)
}

pub(crate) fn clear_from_environment() -> io::Result<bool> {
    clear(&selection_path_from_environment()?)
}

fn selection_path_from_environment() -> io::Result<PathBuf> {
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
    Ok(root.join("boomux/selected-workspace.json"))
}

fn secure_parent(path: &Path) -> io::Result<&Path> {
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::other("selected Workspace path has no parent"))?;
    fs::create_dir_all(parent)?;
    let metadata = fs::symlink_metadata(parent)?;
    if !metadata.is_dir() || metadata.uid() != effective_uid() {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "selected Workspace directory is not owned by the current user",
        ));
    }
    fs::set_permissions(parent, fs::Permissions::from_mode(0o700))?;
    Ok(parent)
}

fn lock(parent: &Path) -> io::Result<File> {
    let path = parent.join("selected-workspace.lock");
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
            "selected Workspace lock is not an owned regular file",
        ));
    }
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(file)
}

fn load(path: &Path) -> io::Result<Option<WorkspaceSelection>> {
    secure_parent(path)?;
    let file = match OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
    {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata.uid() != effective_uid() {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "selected Workspace file is not an owned regular file",
        ));
    }
    if metadata.mode() & 0o077 != 0 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "selected Workspace file is not owner-only",
        ));
    }
    if metadata.len() > MAX_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "selected Workspace file exceeds the size limit",
        ));
    }
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    file.take(MAX_BYTES + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "selected Workspace file exceeds the size limit",
        ));
    }
    let persisted: PersistedSelection = serde_json::from_slice(&bytes).map_err(|error| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("could not parse selected Workspace: {error}"),
        )
    })?;
    if persisted.version != VERSION {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "unsupported selected Workspace version {}; expected {VERSION}",
                persisted.version
            ),
        ));
    }
    WorkspaceSelection::new(persisted.workspace_id).map(Some)
}

fn save(path: &Path, selection: &WorkspaceSelection) -> io::Result<()> {
    let parent = secure_parent(path)?;
    let _lock = lock(parent)?;
    let bytes = serde_json::to_vec_pretty(&PersistedSelection {
        version: VERSION,
        workspace_id: selection.workspace_id.clone(),
    })
    .map_err(io::Error::other)?;
    let temporary = parent.join(format!(".selected-workspace-{}.tmp", Uuid::new_v4()));
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

fn clear(path: &Path) -> io::Result<bool> {
    let parent = secure_parent(path)?;
    let _lock = lock(parent)?;
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error),
    };
    if !metadata.is_file() || metadata.uid() != effective_uid() {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "selected Workspace file is not an owned regular file",
        ));
    }
    fs::remove_file(path)?;
    let _ = File::open(parent).and_then(|directory| directory.sync_all());
    Ok(true)
}

fn effective_uid() -> u32 {
    unsafe { libc::geteuid() }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn path() -> PathBuf {
        env::temp_dir()
            .join(format!("boomux-workspace-selection-{}", Uuid::new_v4()))
            .join("boomux/selected-workspace.json")
    }

    #[test]
    fn selection_round_trips_owner_only_and_clears() {
        let path = path();
        let selection = WorkspaceSelection::new(Uuid::new_v4().to_string()).unwrap();
        save(&path, &selection).unwrap();
        assert_eq!(load(&path).unwrap(), Some(selection));
        assert_eq!(fs::symlink_metadata(&path).unwrap().mode() & 0o777, 0o600);
        assert!(clear(&path).unwrap());
        assert!(!clear(&path).unwrap());
        fs::remove_dir_all(path.parent().unwrap().parent().unwrap()).unwrap();
    }

    #[test]
    fn malformed_selection_is_preserved_and_can_be_cleared() {
        let path = path();
        secure_parent(&path).unwrap();
        fs::write(&path, b"not-json").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(load(&path).unwrap_err().kind(), io::ErrorKind::InvalidData);
        assert_eq!(fs::read(&path).unwrap(), b"not-json");
        assert!(clear(&path).unwrap());
        fs::remove_dir_all(path.parent().unwrap().parent().unwrap()).unwrap();
    }

    #[test]
    fn rejects_non_owner_only_selection() {
        let path = path();
        let selection = WorkspaceSelection::new(Uuid::new_v4().to_string()).unwrap();
        save(&path, &selection).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        assert_eq!(
            load(&path).unwrap_err().kind(),
            io::ErrorKind::PermissionDenied
        );
        fs::remove_dir_all(path.parent().unwrap().parent().unwrap()).unwrap();
    }

    #[test]
    fn rejects_oversized_and_noncanonical_selections() {
        let path = path();
        secure_parent(&path).unwrap();
        fs::write(&path, vec![b'x'; (MAX_BYTES + 1) as usize]).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(load(&path).unwrap_err().kind(), io::ErrorKind::InvalidData);
        assert!(WorkspaceSelection::new(Uuid::new_v4().to_string().to_uppercase()).is_err());
        fs::remove_dir_all(path.parent().unwrap().parent().unwrap()).unwrap();
    }
}

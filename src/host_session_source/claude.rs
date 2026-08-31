use std::ffi::OsString;
use std::fs::{self, OpenOptions};
use std::io::Read;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use super::normalize_absolute;

pub(crate) const MAX_FILE_BYTES: u64 = 256 * 1024;
const MAX_CATALOG_BYTES: u64 = 4 * 1024 * 1024;

#[derive(Clone, Debug, Default)]
pub(crate) struct Environment {
    config_dir: Option<OsString>,
    home: Option<OsString>,
}

impl Environment {
    pub(crate) fn from_process() -> Self {
        Self {
            config_dir: std::env::var_os("CLAUDE_CONFIG_DIR"),
            home: std::env::var_os("HOME"),
        }
    }

    #[cfg(test)]
    pub(crate) fn for_test(config_dir: Option<OsString>, home: Option<OsString>) -> Self {
        Self { config_dir, home }
    }
}

pub(crate) struct TranscriptPrefix {
    pub(crate) root_id: String,
    pub(crate) bytes: Vec<u8>,
}

pub(crate) fn session_catalog(
    directory: &Path,
    environment: &Environment,
    max_files: usize,
) -> Option<(PathBuf, Vec<TranscriptPrefix>)> {
    let normalized_directory = normalize_absolute(directory)?;
    let catalog_directory = session_directory(&normalized_directory, environment)?;
    let files = catalog_files(&catalog_directory, max_files)?;
    let mut remaining_bytes = MAX_CATALOG_BYTES;
    let mut prefixes = Vec::new();

    for path in files {
        if remaining_bytes == 0 {
            break;
        }
        let root_id = path.file_stem()?.to_str()?.to_owned();
        let limit = MAX_FILE_BYTES.min(remaining_bytes);
        let Some(prefix) = read_regular_file_prefix(&path, limit) else {
            continue;
        };
        remaining_bytes = remaining_bytes.saturating_sub(prefix.scanned_bytes);
        prefixes.push(TranscriptPrefix {
            root_id,
            bytes: prefix.bytes,
        });
    }
    Some((normalized_directory, prefixes))
}

fn session_directory(directory: &Path, environment: &Environment) -> Option<PathBuf> {
    let root = match environment
        .config_dir
        .as_ref()
        .filter(|value| !value.is_empty())
    {
        Some(root) => PathBuf::from(root),
        None => PathBuf::from(
            environment
                .home
                .as_ref()
                .filter(|value| !value.is_empty())?,
        )
        .join(".claude"),
    };
    root.is_absolute().then(|| {
        root.join("projects")
            .join(encoded_project_directory(directory))
    })
}

#[cfg(test)]
pub(crate) fn session_directory_for_test(
    directory: &Path,
    environment: &Environment,
) -> Option<PathBuf> {
    session_directory(directory, environment)
}

fn encoded_project_directory(directory: &Path) -> String {
    directory
        .to_string_lossy()
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() {
                character
            } else {
                '-'
            }
        })
        .collect()
}

fn catalog_files(directory: &Path, max_files: usize) -> Option<Vec<PathBuf>> {
    let mut files = Vec::new();
    for entry in fs::read_dir(directory).ok()?.flatten() {
        let path = entry.path();
        if path
            .extension()
            .is_none_or(|extension| extension != "jsonl")
        {
            continue;
        }
        let Ok(metadata) = fs::symlink_metadata(&path) else {
            continue;
        };
        if !metadata.file_type().is_file() {
            continue;
        }
        files.push((path, metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH)));
    }
    files.sort_by(|(left_path, left_time), (right_path, right_time)| {
        right_time
            .cmp(left_time)
            .then_with(|| left_path.cmp(right_path))
    });
    files.truncate(max_files);
    Some(files.into_iter().map(|(path, _)| path).collect())
}

struct FilePrefix {
    bytes: Vec<u8>,
    scanned_bytes: u64,
}

fn read_regular_file_prefix(path: &Path, limit: u64) -> Option<FilePrefix> {
    let mut file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)
        .ok()?;
    let metadata = file.metadata().ok()?;
    if !metadata.file_type().is_file() {
        return None;
    }
    let mut bytes = Vec::new();
    file.by_ref().take(limit).read_to_end(&mut bytes).ok()?;
    let scanned_bytes = bytes.len() as u64;
    if metadata.len() > scanned_bytes {
        let complete_length = bytes
            .iter()
            .rposition(|byte| *byte == b'\n')
            .map_or(0, |index| index + 1);
        bytes.truncate(complete_length);
    }
    Some(FilePrefix {
        bytes,
        scanned_bytes,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derives_exact_project_directory_from_config_or_home() {
        assert_eq!(
            session_directory_for_test(
                Path::new("/home/test/work.tree_repo"),
                &Environment::for_test(Some("/config/claude".into()), Some("/ignored".into()))
            ),
            Some(PathBuf::from(
                "/config/claude/projects/-home-test-work-tree-repo"
            ))
        );
        assert_eq!(
            session_directory_for_test(
                Path::new("/home/test/repo"),
                &Environment::for_test(Some(OsString::new()), Some("/home/test".into()))
            ),
            Some(PathBuf::from("/home/test/.claude/projects/-home-test-repo"))
        );
        assert!(
            session_directory_for_test(
                Path::new("/repo"),
                &Environment::for_test(Some("relative".into()), None)
            )
            .is_none()
        );
    }
}

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
    coding_agent_dir: Option<OsString>,
    session_dir: Option<OsString>,
    home: Option<OsString>,
}

impl Environment {
    pub(crate) fn from_process() -> Self {
        Self {
            coding_agent_dir: std::env::var_os("PI_CODING_AGENT_DIR"),
            session_dir: std::env::var_os("PI_CODING_AGENT_SESSION_DIR"),
            home: std::env::var_os("HOME"),
        }
    }

    #[cfg(test)]
    pub(crate) fn for_test(
        coding_agent_dir: Option<OsString>,
        session_dir: Option<OsString>,
        home: Option<OsString>,
    ) -> Self {
        Self {
            coding_agent_dir,
            session_dir,
            home,
        }
    }
}

pub(crate) fn session_catalog(
    directory: &Path,
    environment: &Environment,
    max_files: usize,
) -> Option<(PathBuf, Vec<Vec<u8>>)> {
    let normalized_directory = normalize_absolute(directory)?;
    let catalog_directory = session_directory(&normalized_directory, environment)?;
    let files = catalog_files(&catalog_directory, max_files)?;
    let mut remaining_bytes = MAX_CATALOG_BYTES;
    let mut prefixes = Vec::new();

    for path in files {
        if remaining_bytes == 0 {
            break;
        }
        let limit = MAX_FILE_BYTES.min(remaining_bytes);
        let Some(prefix) = read_regular_file_prefix(&path, limit) else {
            continue;
        };
        remaining_bytes = remaining_bytes.saturating_sub(prefix.scanned_bytes);
        prefixes.push(prefix.bytes);
    }
    Some((normalized_directory, prefixes))
}

fn session_directory(directory: &Path, environment: &Environment) -> Option<PathBuf> {
    if let Some(session_directory) = nonempty(environment.session_dir.as_ref()) {
        return expand_home(session_directory, environment.home.as_ref());
    }
    let root = match nonempty(environment.coding_agent_dir.as_ref()) {
        Some(root) => expand_home(root, environment.home.as_ref())?,
        None => PathBuf::from(nonempty(environment.home.as_ref())?).join(".pi/agent"),
    };
    root.is_absolute().then(|| {
        root.join("sessions")
            .join(encoded_session_directory(directory))
    })
}

#[cfg(test)]
pub(crate) fn session_directory_for_test(
    directory: &Path,
    environment: &Environment,
) -> Option<PathBuf> {
    session_directory(directory, environment)
}

fn nonempty(value: Option<&OsString>) -> Option<&OsString> {
    value.filter(|value| !value.is_empty())
}

fn expand_home(value: &OsString, home: Option<&OsString>) -> Option<PathBuf> {
    let path = PathBuf::from(value);
    let expanded = if path == Path::new("~") {
        PathBuf::from(nonempty(home)?)
    } else if let Ok(suffix) = path.strip_prefix("~/") {
        PathBuf::from(nonempty(home)?).join(suffix)
    } else {
        path
    };
    expanded.is_absolute().then_some(expanded)
}

fn encoded_session_directory(directory: &Path) -> String {
    let directory = directory.to_string_lossy();
    let encoded: String = directory
        .trim_start_matches('/')
        .chars()
        .map(|character| match character {
            '/' | '\\' | ':' => '-',
            other => other,
        })
        .collect();
    format!("--{encoded}--")
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

// Prefix-only inspection keeps I/O bounded; session_info names beyond this prefix are unavailable.
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

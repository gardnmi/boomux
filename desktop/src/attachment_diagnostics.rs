//! Bounded, best-effort attachment evidence. Never records output or free-form errors.
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
use std::path::Path;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, SyncSender};
use std::time::{SystemTime, UNIX_EPOCH};

const MAX_BYTES: u64 = 512 * 1024;
const QUEUE_SIZE: usize = 128;
static SENDER: OnceLock<Option<SyncSender<Record>>> = OnceLock::new();
static DROPPED: AtomicU64 = AtomicU64::new(0);

#[derive(serde::Serialize)]
struct Record {
    time_ms: u128,
    pid: u32,
    version: &'static str,
    event: &'static str,
    shell_id: String,
    run_id: Option<String>,
    dropped: u64,
}

pub fn record(event: &'static str, shell: &str, run: Option<&str>) {
    // Attachment fixtures must not write to the user's state directory.
    if cfg!(test) {
        return;
    }
    let sender = SENDER.get_or_init(|| {
        let path = crate::layout_state::path()?.parent()?.to_owned();
        let (sender, receiver) = mpsc::sync_channel::<Record>(QUEUE_SIZE);
        std::thread::Builder::new()
            .name("attachment-diagnostics".into())
            .spawn(move || {
                for mut record in receiver {
                    record.dropped = DROPPED.swap(0, Ordering::Relaxed);
                    if append(&path, &record, MAX_BYTES).is_err() {
                        DROPPED.fetch_add(record.dropped.saturating_add(1), Ordering::Relaxed);
                    }
                }
            })
            .ok()?;
        Some(sender)
    });
    if let Some(sender) = sender {
        let record = Record {
            time_ms: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis(),
            pid: std::process::id(),
            version: env!("CARGO_PKG_VERSION"),
            event,
            shell_id: shell.chars().take(128).collect(),
            run_id: run.map(|run| run.chars().take(128).collect()),
            dropped: 0,
        };
        enqueue(sender, record, &DROPPED);
    }
}

fn enqueue(sender: &SyncSender<Record>, record: Record, dropped: &AtomicU64) {
    if sender.try_send(record).is_err() {
        dropped.fetch_add(1, Ordering::Relaxed);
    }
}

fn private_file(path: &Path) -> io::Result<File> {
    let file = OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.nlink() != 1
        || metadata.mode() & 0o077 != 0
    {
        return Err(io::Error::other("diagnostic file is not private"));
    }
    Ok(file)
}

fn append(directory: &Path, record: &Record, limit: u64) -> io::Result<()> {
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(directory)?;
    let metadata = fs::symlink_metadata(directory)?;
    if !metadata.is_dir() || metadata.uid() != unsafe { libc::geteuid() } {
        return Err(io::Error::other("invalid diagnostic directory"));
    }
    let lock = private_file(&directory.join("attachments.lock"))?;
    // Never queue behind another Desktop process; missing records are counted.
    if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let path = directory.join("attachments.jsonl");
    let mut line = serde_json::to_vec(record)?;
    line.push(b'\n');
    let mut file = private_file(&path)?;
    if file.metadata()?.len() + line.len() as u64 > limit {
        fs::rename(&path, directory.join("attachments.previous.jsonl"))?;
        file = private_file(&path)?;
    }
    file.write_all(&line)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn attachment_diagnostics_drop_and_count_overflow_without_growing_the_queue() {
        let (sender, receiver) = mpsc::sync_channel(2);
        let dropped = AtomicU64::new(0);
        let record = || Record {
            time_ms: 1,
            pid: 1,
            version: "test",
            event: "test",
            shell_id: "s".into(),
            run_id: None,
            dropped: 0,
        };
        for _ in 0..100 {
            enqueue(&sender, record(), &dropped);
        }
        assert_eq!(dropped.load(Ordering::Relaxed), 98);
        assert_eq!(receiver.try_iter().count(), 2);
        drop(receiver);
        enqueue(&sender, record(), &dropped);
        assert_eq!(dropped.load(Ordering::Relaxed), 99);
    }

    #[test]
    fn attachment_diagnostics_rotate_and_reject_symlinks() {
        let dir =
            std::env::temp_dir().join(format!("boomux-attachment-log-{}", uuid::Uuid::new_v4()));
        let record = Record {
            time_ms: 1,
            pid: 1,
            version: "test",
            event: "detached",
            shell_id: "shell".into(),
            run_id: Some("run".into()),
            dropped: 3,
        };
        let limit = (serde_json::to_vec(&record).unwrap().len() + 1) as u64;
        for _ in 0..5 {
            append(&dir, &record, limit).unwrap();
        }
        for name in ["attachments.jsonl", "attachments.previous.jsonl"] {
            let path = dir.join(name);
            assert_eq!(fs::metadata(&path).unwrap().len(), limit);
            let value: serde_json::Value =
                serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
            assert_eq!(value["dropped"], 3);
        }
        let lock = private_file(&dir.join("attachments.lock")).unwrap();
        assert_eq!(
            unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
            0
        );
        assert!(append(&dir, &record, limit).is_err());
        assert_eq!(
            fs::metadata(dir.join("attachments.jsonl")).unwrap().len(),
            limit
        );
        drop(lock);
        append(&dir, &record, limit).unwrap();
        fs::remove_file(dir.join("attachments.jsonl")).unwrap();
        let target = dir.join("private-data");
        fs::write(&target, "unchanged").unwrap();
        std::os::unix::fs::symlink(&target, dir.join("attachments.jsonl")).unwrap();
        assert!(append(&dir, &record, limit).is_err());
        assert_eq!(fs::read_to_string(&target).unwrap(), "unchanged");
        fs::remove_dir_all(dir).unwrap();
    }
}

use std::collections::{HashMap, HashSet};
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::PathBuf;
use std::sync::{Mutex, MutexGuard};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::protocol::{ShellSnapshot, ShellSpec};
use crate::state_store::{
    PersistedShellRun, effective_uid, secure_state_dir, state_directory_from_environment,
};

const VERSION: u32 = 1;
const MAX_BYTES: u64 = 8 * 1024 * 1024;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct LocalShellTransaction {
    pub(crate) operation_id: String,
    pub(crate) request_fingerprint: String,
    pub(crate) request_bytes: usize,
    pub(crate) global_workspace_id: String,
    pub(crate) expected_global_revision: u64,
    pub(crate) node_id: String,
    pub(crate) requested_owner_workspace_id: String,
    pub(crate) owner_workspace_id: String,
    pub(crate) owner_workspace_name: String,
    pub(crate) owner_revision: u64,
    pub(crate) default_cwd: Option<PathBuf>,
    pub(crate) shell_id: String,
    pub(crate) shell: ShellSpec,
    pub(crate) result_shell: ShellSnapshot,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct LocalShellStartTransaction {
    pub(crate) shell_id: String,
    pub(crate) run: PersistedShellRun,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum LocalShellJournalRecord {
    Create(Box<LocalShellTransaction>),
    Start(Box<LocalShellStartTransaction>),
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct JournalEntry {
    version: u32,
    record: LocalShellJournalRecord,
    checksum: String,
}

struct JournalState {
    file: File,
    records: Vec<LocalShellJournalRecord>,
    failed: bool,
}

pub(crate) struct LocalShellJournal {
    state: Mutex<JournalState>,
}

impl LocalShellJournal {
    pub(crate) fn load_from_environment() -> io::Result<Self> {
        let directory = state_directory_from_environment()?;
        Self::load_at(directory)
    }

    fn load_at(directory: PathBuf) -> io::Result<Self> {
        secure_state_dir(&directory)?;
        let path = directory.join("local_shell_transactions.log");
        let existed = path.exists();
        let mut file = OpenOptions::new()
            .read(true)
            .create(true)
            .append(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&path)?;
        let metadata = file.metadata()?;
        if !metadata.is_file() || metadata.uid() != effective_uid() {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "local Shell transaction journal is not an owned regular file",
            ));
        }
        if metadata.mode() & 0o077 != 0 {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "local Shell transaction journal is not owner-only",
            ));
        }
        if metadata.len() > MAX_BYTES {
            return Err(invalid(
                "local Shell transaction journal exceeds the size limit",
            ));
        }
        if !existed {
            file.sync_all()?;
            File::open(&directory)?.sync_all()?;
        }
        let mut bytes = Vec::with_capacity(metadata.len() as usize);
        file.seek(SeekFrom::Start(0))?;
        file.take(MAX_BYTES + 1).read_to_end(&mut bytes)?;
        if bytes.len() as u64 > MAX_BYTES {
            return Err(invalid(
                "local Shell transaction journal exceeds the size limit",
            ));
        }
        let (records, valid_bytes) = parse_records(&bytes)?;
        file = OpenOptions::new()
            .read(true)
            .append(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&path)?;
        if valid_bytes < bytes.len() {
            file.set_len(valid_bytes as u64)?;
            file.sync_data()?;
        }
        Ok(Self {
            state: Mutex::new(JournalState {
                file,
                records,
                failed: false,
            }),
        })
    }

    pub(crate) fn records(&self) -> io::Result<Vec<LocalShellJournalRecord>> {
        let state = self.lock()?;
        ensure_usable(&state)?;
        Ok(state.records.clone())
    }

    pub(crate) fn append(&self, record: LocalShellJournalRecord) -> io::Result<()> {
        let payload = serde_json::to_vec(&record).map_err(io::Error::other)?;
        let entry = JournalEntry {
            version: VERSION,
            checksum: format!("{:x}", Sha256::digest(&payload)),
            record: record.clone(),
        };
        let mut bytes = serde_json::to_vec(&entry).map_err(io::Error::other)?;
        bytes.push(b'\n');
        let mut state = self.lock()?;
        ensure_usable(&state)?;
        let mut candidate = state.records.clone();
        candidate.push(record.clone());
        validate_sequence(&candidate)?;
        let current = state.file.metadata()?.len();
        if current.saturating_add(bytes.len() as u64) > MAX_BYTES {
            return Err(invalid(
                "local Shell transaction journal exceeds the size limit",
            ));
        }
        if let Err(error) = state
            .file
            .write_all(&bytes)
            .and_then(|()| state.file.sync_data())
        {
            if let Err(rollback) = state
                .file
                .set_len(current)
                .and_then(|()| state.file.seek(SeekFrom::End(0)).map(|_| ()))
                .and_then(|()| state.file.sync_data())
            {
                state.failed = true;
                return Err(io::Error::new(
                    error.kind(),
                    format!(
                        "local Shell journal append failed: {error}; rollback also failed: {rollback}"
                    ),
                ));
            }
            return Err(error);
        }
        state.records.push(record);
        Ok(())
    }

    pub(crate) fn contains_create(&self, shell_id: &str) -> io::Result<bool> {
        let state = self.lock()?;
        ensure_usable(&state)?;
        Ok(state.records.iter().any(|record| {
            matches!(record, LocalShellJournalRecord::Create(transaction) if transaction.shell_id == shell_id)
        }))
    }

    pub(crate) fn clear(&self) -> io::Result<()> {
        let mut state = self.lock()?;
        ensure_usable(&state)?;
        clear_state(&mut state)
    }

    pub(crate) fn reset_after_full_checkpoint(&self) -> io::Result<()> {
        let mut state = self.lock()?;
        clear_state(&mut state)?;
        state.failed = false;
        Ok(())
    }

    pub(crate) fn is_empty(&self) -> io::Result<bool> {
        let state = self.lock()?;
        ensure_usable(&state)?;
        Ok(state.records.is_empty())
    }

    fn lock(&self) -> io::Result<MutexGuard<'_, JournalState>> {
        self.state
            .lock()
            .map_err(|_| io::Error::other("local Shell transaction journal lock is poisoned"))
    }
}

fn clear_state(state: &mut JournalState) -> io::Result<()> {
    state.file.set_len(0)?;
    state.file.seek(SeekFrom::Start(0))?;
    state.file.sync_data()?;
    state.records.clear();
    Ok(())
}

fn ensure_usable(state: &JournalState) -> io::Result<()> {
    if state.failed {
        Err(io::Error::other(
            "local Shell transaction journal has an ambiguous failed append",
        ))
    } else {
        Ok(())
    }
}

fn parse_records(bytes: &[u8]) -> io::Result<(Vec<LocalShellJournalRecord>, usize)> {
    let mut records = Vec::new();
    let mut offset = 0;
    for line in bytes.split_inclusive(|byte| *byte == b'\n') {
        if !line.ends_with(b"\n") {
            break;
        }
        offset += line.len();
        let entry: JournalEntry =
            serde_json::from_slice(&line[..line.len() - 1]).map_err(|error| {
                invalid(format!("could not parse local Shell transaction: {error}"))
            })?;
        if entry.version != VERSION {
            return Err(invalid(format!(
                "unsupported local Shell transaction version {}; expected {VERSION}",
                entry.version
            )));
        }
        let payload = serde_json::to_vec(&entry.record).map_err(io::Error::other)?;
        let checksum = format!("{:x}", Sha256::digest(&payload));
        if entry.checksum != checksum {
            return Err(invalid("local Shell transaction checksum does not match"));
        }
        records.push(entry.record);
    }
    validate_sequence(&records)?;
    Ok((records, offset))
}

fn validate_sequence(records: &[LocalShellJournalRecord]) -> io::Result<()> {
    let mut generations = HashMap::<&str, u64>::new();
    let mut run_ids = HashSet::<&str>::new();
    for record in records {
        match record {
            LocalShellJournalRecord::Create(transaction) => {
                if generations
                    .insert(transaction.shell_id.as_str(), 0)
                    .is_some()
                {
                    return Err(invalid("local Shell journal contains a duplicate creation"));
                }
            }
            LocalShellJournalRecord::Start(transaction) => {
                let Some(generation) = generations.get_mut(transaction.shell_id.as_str()) else {
                    return Err(invalid(
                        "local Shell journal start does not follow its creation",
                    ));
                };
                let expected = generation
                    .checked_add(1)
                    .ok_or_else(|| invalid("local Shell journal run generation exhausted"))?;
                if transaction.run.generation != expected
                    || transaction.run.ended_at_ms.is_some()
                    || transaction.run.exit_reason.is_some()
                    || !run_ids.insert(&transaction.run.id)
                {
                    return Err(invalid(
                        "local Shell journal contains an invalid run sequence",
                    ));
                }
                *generation = expected;
            }
        }
    }
    Ok(())
}

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::MetadataExt;
    use uuid::Uuid;

    fn transaction() -> LocalShellTransaction {
        LocalShellTransaction {
            operation_id: Uuid::from_u128(1).to_string(),
            request_fingerprint: format!("{:064x}", 2),
            request_bytes: 1024,
            global_workspace_id: Uuid::from_u128(3).to_string(),
            expected_global_revision: 1,
            node_id: Uuid::from_u128(4).to_string(),
            requested_owner_workspace_id: Uuid::from_u128(5).to_string(),
            owner_workspace_id: Uuid::from_u128(5).to_string(),
            owner_workspace_name: "project".into(),
            owner_revision: 2,
            default_cwd: Some("/tmp".into()),
            shell_id: Uuid::from_u128(6).to_string(),
            shell: ShellSpec::login("shell", "/tmp"),
            result_shell: ShellSnapshot {
                id: Uuid::from_u128(6).to_string(),
                revision: 1,
                workspace_id: Uuid::from_u128(5).to_string(),
                name: "shell".into(),
                cwd: "/tmp".into(),
                command: Vec::new(),
                status: crate::protocol::ShellStatus::Pending,
                run: None,
                recovered_agent_id: None,
                foreground_process: None,
            },
        }
    }

    fn create_record() -> LocalShellJournalRecord {
        LocalShellJournalRecord::Create(Box::new(transaction()))
    }

    fn start_record() -> LocalShellJournalRecord {
        LocalShellJournalRecord::Start(Box::new(LocalShellStartTransaction {
            shell_id: transaction().shell_id,
            run: PersistedShellRun {
                id: Uuid::from_u128(7).to_string(),
                generation: 1,
                started_at_ms: 1,
                ended_at_ms: None,
                exit_reason: None,
                output_revision: 0,
                environment_has_run_id: true,
                profile: crate::protocol::TerminalProfile {
                    term: Some("xterm-256color".into()),
                    colorterm: None,
                    term_program: None,
                    term_program_version: None,
                    rows: 24,
                    cols: 80,
                    pixel_width: 0,
                    pixel_height: 0,
                },
                terminal_history: None,
            },
        }))
    }

    #[test]
    fn records_are_checksummed_and_torn_tail_is_ignored() {
        let record = create_record();
        let payload = serde_json::to_vec(&record).unwrap();
        let entry = JournalEntry {
            version: VERSION,
            record: record.clone(),
            checksum: format!("{:x}", Sha256::digest(payload)),
        };
        let mut bytes = serde_json::to_vec(&entry).unwrap();
        bytes.push(b'\n');
        let valid = bytes.len();
        bytes.extend_from_slice(b"{\"version\":");

        let (records, consumed) = parse_records(&bytes).unwrap();
        assert_eq!(records, vec![record]);
        assert_eq!(consumed, valid);
    }

    #[test]
    fn checksum_mismatch_fails_closed() {
        let entry = JournalEntry {
            version: VERSION,
            record: create_record(),
            checksum: "0".repeat(64),
        };
        let mut bytes = serde_json::to_vec(&entry).unwrap();
        bytes.push(b'\n');
        assert_eq!(
            parse_records(&bytes).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
    }

    #[test]
    fn start_records_require_creation_and_contiguous_unique_runs() {
        let root =
            std::env::temp_dir().join(format!("boomux-local-shell-sequence-{}", Uuid::new_v4()));
        let journal = LocalShellJournal::load_at(root.clone()).unwrap();
        assert_eq!(
            journal.append(start_record()).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
        journal.append(create_record()).unwrap();
        let mut skipped = start_record();
        let LocalShellJournalRecord::Start(transaction) = &mut skipped else {
            unreachable!();
        };
        transaction.run.generation = 2;
        assert_eq!(
            journal.append(skipped).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
        journal.append(start_record()).unwrap();
        assert_eq!(
            journal.append(start_record()).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn journal_round_trips_owner_only_and_clears() {
        let root =
            std::env::temp_dir().join(format!("boomux-local-shell-journal-{}", Uuid::new_v4()));
        let journal = LocalShellJournal::load_at(root.clone()).unwrap();
        journal.append(create_record()).unwrap();
        journal.append(start_record()).unwrap();
        assert_eq!(
            journal.records().unwrap(),
            vec![create_record(), start_record()]
        );
        assert!(journal.contains_create(&transaction().shell_id).unwrap());
        let path = root.join("local_shell_transactions.log");
        assert_eq!(fs::metadata(&path).unwrap().mode() & 0o777, 0o600);
        drop(journal);

        let journal = LocalShellJournal::load_at(root.clone()).unwrap();
        assert_eq!(
            journal.records().unwrap(),
            vec![create_record(), start_record()]
        );
        journal.clear().unwrap();
        assert!(journal.records().unwrap().is_empty());
        assert_eq!(fs::metadata(path).unwrap().len(), 0);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn load_truncates_only_an_incomplete_final_record() {
        let root =
            std::env::temp_dir().join(format!("boomux-torn-shell-journal-{}", Uuid::new_v4()));
        let journal = LocalShellJournal::load_at(root.clone()).unwrap();
        journal.append(create_record()).unwrap();
        drop(journal);
        let path = root.join("local_shell_transactions.log");
        let valid = fs::metadata(&path).unwrap().len();
        let mut file = OpenOptions::new().append(true).open(&path).unwrap();
        file.write_all(b"{\"version\":").unwrap();
        file.sync_data().unwrap();

        let journal = LocalShellJournal::load_at(root.clone()).unwrap();
        assert_eq!(journal.records().unwrap(), vec![create_record()]);
        assert_eq!(fs::metadata(path).unwrap().len(), valid);
        fs::remove_dir_all(root).unwrap();
    }
}

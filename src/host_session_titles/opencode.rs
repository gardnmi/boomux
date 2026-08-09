use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, TryRecvError};
use std::thread;
use std::time::{Duration, Instant};

use super::{HostSession, Inspection, MAX_SESSIONS, TitleAdapter, sanitize_title};
use crate::host_session_source::normalize_absolute;

const COMMAND_TIMEOUT: Duration = Duration::from_secs(15);
pub(super) const MAX_STDOUT_BYTES: u64 = 1024 * 1024;

struct OpenCodeAdapter;

pub(super) static ADAPTER: &dyn TitleAdapter = &OpenCodeAdapter;

impl TitleAdapter for OpenCodeAdapter {
    fn integration(&self) -> &'static str {
        "opencode"
    }

    fn inspect(&self, directory: &Path) -> Option<super::Titles> {
        inspect_catalog(directory).map(|inspection| inspection.titles)
    }
}

pub(super) fn inspect_catalog(directory: &Path) -> Option<Inspection> {
    let stdout = run(directory)?;
    parse_catalog(&stdout)
}

fn run(directory: &Path) -> Option<Vec<u8>> {
    let mut child = Command::new("opencode")
        .args(["--pure", "session", "list", "--format", "json", "-n", "100"])
        .current_dir(directory)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let mut stdout = child.stdout.take()?;
    let (sender, receiver) = mpsc::channel();
    thread::spawn(move || {
        let mut bytes = Vec::new();
        let result = stdout
            .by_ref()
            .take(MAX_STDOUT_BYTES + 1)
            .read_to_end(&mut bytes)
            .map(|_| bytes);
        let _ = sender.send(result);
    });

    let deadline = Instant::now() + COMMAND_TIMEOUT;
    let mut output = None;
    let mut status = None;
    loop {
        if output.is_none() {
            match receiver.try_recv() {
                Ok(Ok(bytes)) => output = Some(bytes),
                Ok(Err(_)) | Err(TryRecvError::Disconnected) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return None;
                }
                Err(TryRecvError::Empty) => {}
            }
        }
        if output
            .as_ref()
            .is_some_and(|bytes| bytes.len() as u64 > MAX_STDOUT_BYTES)
        {
            let _ = child.kill();
            let _ = child.wait();
            return None;
        }
        if status.is_none() {
            status = child.try_wait().ok()?;
        }
        if let (Some(status), Some(bytes)) = (status.as_ref(), output.as_ref()) {
            return status.success().then(|| bytes.clone());
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return None;
        }
        thread::sleep(Duration::from_millis(10));
    }
}

#[derive(serde::Deserialize)]
struct OpenCodeSession {
    id: String,
    title: String,
    updated: u64,
    created: u64,
    directory: String,
}

pub(super) fn parse_catalog(output: &[u8]) -> Option<Inspection> {
    if output.len() as u64 > MAX_STDOUT_BYTES {
        return None;
    }
    let sessions: Vec<OpenCodeSession> = serde_json::from_slice(output).ok()?;
    let mut catalog = Vec::new();
    for session in sessions.into_iter().take(MAX_SESSIONS) {
        if session.id.is_empty() {
            continue;
        }
        let Some(title) = sanitize_title(&session.title) else {
            continue;
        };
        let Some(directory) = normalize_absolute(Path::new(&session.directory)) else {
            continue;
        };
        catalog.push(HostSession {
            integration: "opencode".into(),
            root_id: session.id,
            title,
            directory,
            created_at_ms: session.created,
            updated_at_ms: session.updated,
        });
    }
    let titles = catalog
        .iter()
        .map(|session| (session.root_id.clone(), session.title.clone()))
        .collect();
    Some(Inspection { titles, catalog })
}

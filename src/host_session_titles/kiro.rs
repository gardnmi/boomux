use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, TryRecvError};
use std::thread;
use std::time::{Duration, Instant};

use serde::Deserialize;

use super::{HostSession, Inspection, MAX_SESSIONS, TitleAdapter, sanitize_title};
use crate::host_session_source::normalize_absolute;

const COMMAND_TIMEOUT: Duration = Duration::from_secs(15);
const MAX_STDOUT_BYTES: u64 = 1024 * 1024;

struct KiroAdapter;

pub(super) static ADAPTER: &dyn TitleAdapter = &KiroAdapter;

impl TitleAdapter for KiroAdapter {
    fn inspect(&self, directory: &Path) -> Option<Inspection> {
        inspect(directory)
    }
}

fn inspect(directory: &Path) -> Option<Inspection> {
    let directory = normalize_absolute(directory)?;
    let stdout = run(&directory)?;
    parse_catalog(&stdout, &directory)
}

fn run(directory: &Path) -> Option<Vec<u8>> {
    run_command(
        "kiro-cli",
        &["chat", "--list-sessions", "--format", "json"],
        directory,
        COMMAND_TIMEOUT,
    )
}

fn run_command(
    executable: &str,
    arguments: &[&str],
    directory: &Path,
    timeout: Duration,
) -> Option<Vec<u8>> {
    let mut child = Command::new(executable)
        .args(arguments)
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

    let deadline = Instant::now() + timeout;
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

#[derive(Deserialize)]
struct DirectorySessions {
    cwd: String,
    sessions: Vec<KiroSession>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct KiroSession {
    session_id: String,
    title: String,
}

fn parse_catalog(output: &[u8], requested_directory: &Path) -> Option<Inspection> {
    if output.len() as u64 > MAX_STDOUT_BYTES {
        return None;
    }
    let requested_directory = normalize_absolute(requested_directory)?;
    let directories: Vec<DirectorySessions> = serde_json::from_slice(output).ok()?;
    let sessions = directories.into_iter().find_map(|directory| {
        let directory_path = normalize_absolute(Path::new(&directory.cwd))?;
        (directory_path == requested_directory).then_some(directory.sessions)
    })?;

    let mut catalog = Vec::new();
    for session in sessions.into_iter().take(MAX_SESSIONS) {
        if boomux::integrations::validate_external_session_id(&session.session_id).is_err() {
            continue;
        }
        let Some(title) = sanitize_title(&session.title) else {
            continue;
        };
        catalog.push(HostSession {
            integration: boomux::integrations::KIRO.key.into(),
            root_id: session.session_id,
            title,
            directory: requested_directory.clone(),
            created_at_ms: 0,
            updated_at_ms: 0,
        });
    }
    let titles = catalog
        .iter()
        .map(|session| (session.root_id.clone(), session.title.clone()))
        .collect();
    Some(Inspection { titles, catalog })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_exact_directory_titles_from_current_kiro_json() {
        let output = br#"[{"cwd":"/repo/./src/..","sessions":[{"sessionId":"sess-v3","source":"v3","title":" Triage   Slack issue thread ","updatedAt":"2026-08-31T12:00:00Z","executionTarget":"local"},{"sessionId":"sess-v2","source":"v2","title":"Older session","updatedAt":"2026-08-30T12:00:00Z"}],"complete":true},{"cwd":"/other","sessions":[{"sessionId":"other","title":"Wrong directory"}],"complete":true}]"#;

        let inspection = parse_catalog(output, Path::new("/repo")).unwrap();

        assert_eq!(inspection.catalog.len(), 2);
        assert_eq!(inspection.titles["sess-v3"], "Triage Slack issue thread");
        assert_eq!(inspection.catalog[0].directory, Path::new("/repo"));
        assert!(!inspection.titles.contains_key("other"));
    }

    #[test]
    fn malformed_wrong_directory_and_oversized_responses_fail_open() {
        assert!(parse_catalog(b"not json", Path::new("/repo")).is_none());
        assert!(
            parse_catalog(br#"[{"cwd":"/other","sessions":[]}]"#, Path::new("/repo")).is_none()
        );
        assert!(
            parse_catalog(
                &vec![b'x'; MAX_STDOUT_BYTES as usize + 1],
                Path::new("/repo")
            )
            .is_none()
        );
    }

    #[test]
    fn bounds_records_and_rejects_invalid_identity_or_title() {
        let sessions = (0..MAX_SESSIONS + 2)
            .map(|index| {
                serde_json::json!({
                    "sessionId": format!("session-{index}"),
                    "title": format!("Title {index}"),
                })
            })
            .chain([
                serde_json::json!({"sessionId": " bad", "title": "invalid ID"}),
                serde_json::json!({"sessionId": "empty-title", "title": " \n "}),
            ])
            .collect::<Vec<_>>();
        let output = serde_json::to_vec(&serde_json::json!([{
            "cwd": "/repo",
            "sessions": sessions,
            "complete": true,
        }]))
        .unwrap();

        let inspection = parse_catalog(&output, Path::new("/repo")).unwrap();

        assert_eq!(inspection.catalog.len(), MAX_SESSIONS);
        assert!(inspection.titles.contains_key("session-99"));
        assert!(!inspection.titles.contains_key("session-100"));
    }

    #[test]
    fn command_runtime_is_bounded() {
        let started = Instant::now();
        assert!(
            run_command(
                "/bin/sleep",
                &["5"],
                Path::new("/"),
                Duration::from_millis(50)
            )
            .is_none()
        );
        assert!(started.elapsed() < Duration::from_secs(2));
    }
}

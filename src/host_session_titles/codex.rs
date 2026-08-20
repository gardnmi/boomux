use std::io::{self, BufRead, BufReader, Read, Write};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use serde::Deserialize;
use serde_json::Value;

use super::{HostSession, Inspection, MAX_SESSIONS, TitleAdapter, sanitize_title};
use crate::host_session_source::normalize_absolute;

const COMMAND_TIMEOUT: Duration = Duration::from_secs(15);
const MAX_STDOUT_BYTES: u64 = 1024 * 1024;

struct CodexAdapter;

pub(super) static ADAPTER: &dyn TitleAdapter = &CodexAdapter;

impl TitleAdapter for CodexAdapter {
    fn inspect(&self, directory: &Path) -> Option<Inspection> {
        inspect_catalog(directory)
    }
}

fn inspect_catalog(directory: &Path) -> Option<Inspection> {
    let directory = normalize_absolute(directory)?;
    let stdout = run(&directory)?;
    parse_catalog(&stdout, &directory)
}

fn run(directory: &Path) -> Option<Vec<u8>> {
    let mut child = Command::new("codex")
        .args(["app-server", "--stdio"])
        .current_dir(directory)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let mut stdin = child.stdin.take()?;
    let stdout = child.stdout.take()?;
    let (sender, receiver) = mpsc::channel();
    thread::spawn(move || stream_stdout(stdout, sender));
    let deadline = Instant::now() + COMMAND_TIMEOUT;
    let result = (|| {
        write_request(
            &mut stdin,
            &serde_json::json!({
            "method": "initialize",
            "id": 1,
            "params": {
                "clientInfo": {
                    "name": "boomux",
                    "title": "Boomux",
                    "version": env!("CARGO_PKG_VERSION"),
                }
            }
            }),
        )?;
        wait_for_response(&receiver, 1, deadline)?;
        write_request(
            &mut stdin,
            &serde_json::json!({ "method": "initialized", "params": {} }),
        )?;
        write_request(
            &mut stdin,
            &serde_json::json!({
            "method": "thread/list",
            "id": 2,
            "params": {
                "limit": MAX_SESSIONS,
                "cwd": directory,
                "useStateDbOnly": true,
                "modelProviders": [],
                "sourceKinds": ["cli", "vscode", "exec", "appServer", "unknown"],
            }
            }),
        )?;
        wait_for_response(&receiver, 2, deadline)
    })();
    drop(stdin);
    let _ = child.kill();
    let _ = child.wait();
    result
}

fn stream_stdout(stdout: impl Read, sender: mpsc::Sender<io::Result<Vec<u8>>>) {
    let mut reader = BufReader::new(stdout);
    let mut total = 0u64;
    loop {
        let mut line = Vec::new();
        let remaining = MAX_STDOUT_BYTES.saturating_sub(total) + 1;
        match reader.by_ref().take(remaining).read_until(b'\n', &mut line) {
            Ok(0) => break,
            Ok(_) => {
                total = total.saturating_add(line.len() as u64);
                if total > MAX_STDOUT_BYTES {
                    let _ = sender.send(Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "Codex app-server output exceeded its size limit",
                    )));
                    break;
                }
                if sender.send(Ok(line)).is_err() {
                    break;
                }
            }
            Err(error) => {
                let _ = sender.send(Err(error));
                break;
            }
        }
    }
}

fn write_request(stdin: &mut impl Write, request: &Value) -> Option<()> {
    serde_json::to_writer(&mut *stdin, request).ok()?;
    stdin.write_all(b"\n").ok()?;
    stdin.flush().ok()
}

fn wait_for_response(
    receiver: &mpsc::Receiver<io::Result<Vec<u8>>>,
    id: u64,
    deadline: Instant,
) -> Option<Vec<u8>> {
    loop {
        let remaining = deadline.checked_duration_since(Instant::now())?;
        let line = receiver.recv_timeout(remaining).ok()?.ok()?;
        let message = serde_json::from_slice::<Value>(&line).ok()?;
        if message.get("id").and_then(Value::as_u64) == Some(id) {
            return message.get("error").is_none().then_some(line);
        }
    }
}

#[derive(Deserialize)]
struct ListResponse {
    id: u64,
    #[serde(default)]
    result: Option<ListResult>,
}

#[derive(Deserialize)]
struct ListResult {
    data: Vec<CodexThread>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CodexThread {
    id: String,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    preview: String,
    #[serde(default)]
    ephemeral: bool,
    created_at: u64,
    updated_at: u64,
}

fn parse_catalog(output: &[u8], directory: &Path) -> Option<Inspection> {
    if output.len() as u64 > MAX_STDOUT_BYTES {
        return None;
    }
    let directory = normalize_absolute(directory)?;
    let response = output.split(|byte| *byte == b'\n').find_map(|line| {
        let response = serde_json::from_slice::<ListResponse>(line).ok()?;
        (response.id == 2).then_some(response)
    })?;
    let mut catalog = Vec::new();
    for thread in response.result?.data.into_iter().take(MAX_SESSIONS) {
        if thread.id.is_empty() || thread.ephemeral {
            continue;
        }
        if boomux::scheduling::validate_external_session_id(&thread.id).is_err() {
            continue;
        }
        let Some(title) = thread
            .name
            .as_deref()
            .and_then(sanitize_title)
            .or_else(|| sanitize_title(&thread.preview))
        else {
            continue;
        };
        let (Some(created_at_ms), Some(updated_at_ms)) = (
            thread.created_at.checked_mul(1000),
            thread.updated_at.checked_mul(1000),
        ) else {
            continue;
        };
        catalog.push(HostSession {
            integration: boomux::integrations::CODEX.key.into(),
            root_id: thread.id,
            title,
            directory: directory.clone(),
            created_at_ms,
            updated_at_ms,
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
    fn parses_bounded_thread_list_with_name_and_preview_fallback() {
        let output = br#"{"id":1,"result":{"userAgent":"codex"}}
{"id":2,"result":{"data":[{"id":"thread-1","name":" Named thread ","preview":"ignored","ephemeral":false,"createdAt":10,"updatedAt":20},{"id":"thread-2","name":null,"preview":" Preview thread ","ephemeral":false,"createdAt":30,"updatedAt":40},{"id":"ephemeral","preview":"ignored","ephemeral":true,"createdAt":1,"updatedAt":1}],"nextCursor":null}}
"#;
        let inspection = parse_catalog(output, Path::new("/repo/./src/..")).unwrap();
        assert_eq!(inspection.catalog.len(), 2);
        assert_eq!(inspection.catalog[0].root_id, "thread-1");
        assert_eq!(inspection.catalog[0].title, "Named thread");
        assert_eq!(inspection.catalog[0].directory, Path::new("/repo"));
        assert_eq!(inspection.catalog[0].created_at_ms, 10_000);
        assert_eq!(inspection.titles["thread-2"], "Preview thread");
    }

    #[test]
    fn malformed_missing_and_oversized_responses_fail_open() {
        assert!(parse_catalog(b"not json", Path::new("/repo")).is_none());
        assert!(parse_catalog(br#"{"id":1,"result":{}}"#, Path::new("/repo")).is_none());
        assert!(
            parse_catalog(
                &vec![b'x'; MAX_STDOUT_BYTES as usize + 1],
                Path::new("/repo")
            )
            .is_none()
        );
    }

    #[test]
    fn stdout_reader_bounds_a_line_without_a_newline() {
        let (sender, receiver) = mpsc::channel();
        stream_stdout(
            io::Cursor::new(vec![b'x'; MAX_STDOUT_BYTES as usize + 2]),
            sender,
        );

        assert_eq!(
            receiver.recv().unwrap().unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
    }
}

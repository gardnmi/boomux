use std::collections::HashMap;
use std::error::Error;
use std::fs::OpenOptions;
use std::io::{self, Read};
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, TryRecvError};
use std::thread;
use std::time::{Duration, Instant};

use serde::Serialize;
use serde_json::Value;

use crate::host_session_titles::{
    PiEnvironment, PiSessionFileError, normalize_absolute, pi_session_file,
};
use crate::session_projection::SessionProjection;

const SOURCE_LIMIT_BYTES: u64 = 16 * 1024 * 1024;
const OPENCODE_TIMEOUT: Duration = Duration::from_secs(30);

trait TranscriptAdapter: Sync {
    fn integration(&self) -> &'static str;

    fn read(&self, request: TranscriptRequest<'_>)
    -> Result<Vec<TranscriptEntry>, TranscriptError>;
}

#[derive(Clone, Copy)]
struct TranscriptRequest<'a> {
    directory: &'a Path,
    external_session_id: &'a str,
}

struct OpenCodeAdapter;
struct PiAdapter;

static OPENCODE_ADAPTER: OpenCodeAdapter = OpenCodeAdapter;
static PI_ADAPTER: PiAdapter = PiAdapter;
static ADAPTERS: &[&dyn TranscriptAdapter] = &[&OPENCODE_ADAPTER, &PI_ADAPTER];

#[derive(Debug)]
pub(crate) struct TranscriptError {
    pub(crate) code: &'static str,
    message: String,
}

impl TranscriptError {
    fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

impl std::fmt::Display for TranscriptError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl Error for TranscriptError {}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub(crate) struct TranscriptEntry {
    #[serde(rename = "type")]
    pub(crate) kind: &'static str,
    pub(crate) source_id: Option<String>,
    pub(crate) timestamp_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) role: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) text: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) tool_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) tool_call_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) status: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) input: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) output: Option<String>,
    pub(crate) truncated: bool,
}

#[derive(Debug, Serialize)]
pub(crate) struct Transcript {
    pub(crate) session_id: String,
    pub(crate) integration: String,
    pub(crate) external_session_id: String,
    pub(crate) entries: Vec<TranscriptEntry>,
    pub(crate) returned_entries: usize,
    pub(crate) total_entries: usize,
    pub(crate) content_bytes: usize,
    pub(crate) truncated: bool,
    pub(crate) truncated_by: Vec<&'static str>,
}

pub(crate) fn read(
    session: &SessionProjection,
    limit: usize,
    max_bytes: usize,
) -> Result<Transcript, TranscriptError> {
    read_with_adapters(session, limit, max_bytes, ADAPTERS)
}

pub(crate) fn supported_integrations() -> Vec<&'static str> {
    ADAPTERS
        .iter()
        .map(|adapter| adapter.integration())
        .collect()
}

fn read_with_adapters(
    session: &SessionProjection,
    limit: usize,
    max_bytes: usize,
    adapters: &[&dyn TranscriptAdapter],
) -> Result<Transcript, TranscriptError> {
    let external_session_id = session.external_session_id.as_deref().ok_or_else(|| {
        TranscriptError::new(
            "session_source_unavailable",
            "session has no canonical external session ID",
        )
    })?;
    let directory = session
        .occurrences
        .iter()
        .rev()
        .find_map(|occurrence| occurrence.retained_shell_cwd.as_deref())
        .ok_or_else(|| {
            TranscriptError::new(
                "session_source_unavailable",
                "session has no retained working directory for host lookup",
            )
        })?;

    let adapter = adapters
        .iter()
        .find(|adapter| adapter.integration() == session.integration)
        .ok_or_else(|| {
            TranscriptError::new(
                "unsupported_integration",
                format!(
                    "session transcript integration is not supported: {}",
                    session.integration
                ),
            )
        })?;
    let entries = adapter.read(TranscriptRequest {
        directory,
        external_session_id,
    })?;
    Ok(bound_entries(
        session,
        external_session_id,
        entries,
        limit,
        max_bytes,
    ))
}

impl TranscriptAdapter for OpenCodeAdapter {
    fn integration(&self) -> &'static str {
        "opencode"
    }

    fn read(
        &self,
        request: TranscriptRequest<'_>,
    ) -> Result<Vec<TranscriptEntry>, TranscriptError> {
        let output = run_opencode_export(request.directory, request.external_session_id)?;
        parse_opencode(&output, request.external_session_id)
    }
}

impl TranscriptAdapter for PiAdapter {
    fn integration(&self) -> &'static str {
        "pi"
    }

    fn read(
        &self,
        request: TranscriptRequest<'_>,
    ) -> Result<Vec<TranscriptEntry>, TranscriptError> {
        read_pi(request.directory, request.external_session_id)
    }
}

fn run_opencode_export(
    directory: &Path,
    external_session_id: &str,
) -> Result<Vec<u8>, TranscriptError> {
    let mut child = Command::new("opencode")
        .args(["export", external_session_id])
        .current_dir(directory)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|error| {
            TranscriptError::new(
                "session_source_unavailable",
                format!("could not start OpenCode export: {error}"),
            )
        })?;
    let mut stdout = child.stdout.take().ok_or_else(|| {
        TranscriptError::new(
            "session_source_unavailable",
            "could not capture OpenCode export",
        )
    })?;
    let (sender, receiver) = mpsc::channel();
    thread::spawn(move || {
        let mut bytes = Vec::new();
        let result = stdout
            .by_ref()
            .take(SOURCE_LIMIT_BYTES + 1)
            .read_to_end(&mut bytes)
            .map(|_| bytes);
        let _ = sender.send(result);
    });

    let deadline = Instant::now() + OPENCODE_TIMEOUT;
    let mut output = None;
    let mut status = None;
    loop {
        if output.is_none() {
            match receiver.try_recv() {
                Ok(Ok(bytes)) => output = Some(bytes),
                Ok(Err(error)) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(TranscriptError::new(
                        "session_source_unavailable",
                        format!("could not read OpenCode export: {error}"),
                    ));
                }
                Err(TryRecvError::Disconnected) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(TranscriptError::new(
                        "session_source_unavailable",
                        "OpenCode export reader stopped unexpectedly",
                    ));
                }
                Err(TryRecvError::Empty) => {}
            }
        }
        if output
            .as_ref()
            .is_some_and(|bytes| bytes.len() as u64 > SOURCE_LIMIT_BYTES)
        {
            let _ = child.kill();
            let _ = child.wait();
            return Err(TranscriptError::new(
                "session_source_too_large",
                format!("OpenCode export exceeds {SOURCE_LIMIT_BYTES} bytes"),
            ));
        }
        if status.is_none() {
            status = match child.try_wait() {
                Ok(status) => status,
                Err(error) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(TranscriptError::new(
                        "session_source_unavailable",
                        format!("could not wait for OpenCode export: {error}"),
                    ));
                }
            };
        }
        if let Some(status) = status.as_ref()
            && let Some(bytes) = output.take()
        {
            if status.success() {
                return Ok(bytes);
            }
            return Err(TranscriptError::new(
                "session_source_unavailable",
                format!("OpenCode export exited with {status}"),
            ));
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err(TranscriptError::new("timeout", "OpenCode export timed out"));
        }
        thread::sleep(Duration::from_millis(10));
    }
}

fn parse_opencode(
    output: &[u8],
    external_session_id: &str,
) -> Result<Vec<TranscriptEntry>, TranscriptError> {
    let document: Value = serde_json::from_slice(output).map_err(|error| {
        TranscriptError::new(
            "session_source_invalid",
            format!("OpenCode export is not valid JSON: {error}"),
        )
    })?;
    if document.pointer("/info/id").and_then(Value::as_str) != Some(external_session_id) {
        return Err(TranscriptError::new(
            "session_source_invalid",
            "OpenCode export session ID does not match the requested session",
        ));
    }
    let messages = document
        .get("messages")
        .and_then(Value::as_array)
        .ok_or_else(|| {
            TranscriptError::new(
                "session_source_invalid",
                "OpenCode export has no messages array",
            )
        })?;
    let mut entries = Vec::new();
    for message in messages {
        let role = message.pointer("/info/role").and_then(Value::as_str);
        let message_time = message
            .pointer("/info/time/created")
            .and_then(Value::as_u64);
        let Some(parts) = message.get("parts").and_then(Value::as_array) else {
            continue;
        };
        for part in parts {
            let source_id = part.get("id").and_then(Value::as_str).map(str::to_owned);
            let timestamp_ms = part
                .pointer("/time/start")
                .and_then(Value::as_u64)
                .or(message_time);
            match part.get("type").and_then(Value::as_str) {
                Some("text") => {
                    if let Some(text) = part
                        .get("text")
                        .and_then(Value::as_str)
                        .filter(|text| !text.is_empty())
                    {
                        entries.push(message_entry(
                            "message",
                            source_id,
                            timestamp_ms,
                            role,
                            text,
                        ));
                    }
                }
                Some("reasoning") => {
                    if let Some(text) = part
                        .get("text")
                        .and_then(Value::as_str)
                        .filter(|text| !text.is_empty())
                    {
                        entries.push(message_entry(
                            "reasoning",
                            source_id,
                            timestamp_ms,
                            role,
                            text,
                        ));
                    }
                }
                Some("tool") => entries.push(TranscriptEntry {
                    kind: "tool",
                    source_id,
                    timestamp_ms,
                    role: None,
                    text: None,
                    tool_name: part.get("tool").and_then(Value::as_str).map(str::to_owned),
                    tool_call_id: part
                        .get("callID")
                        .and_then(Value::as_str)
                        .map(str::to_owned),
                    status: part
                        .pointer("/state/status")
                        .and_then(Value::as_str)
                        .map(str::to_owned),
                    input: part.pointer("/state/input").map(compact_json),
                    output: part
                        .pointer("/state/output")
                        .or_else(|| part.pointer("/state/error"))
                        .map(value_text),
                    truncated: false,
                }),
                _ => {}
            }
        }
    }
    Ok(entries)
}

fn read_pi(
    directory: &Path,
    external_session_id: &str,
) -> Result<Vec<TranscriptEntry>, TranscriptError> {
    let environment = PiEnvironment::from_process();
    let path = pi_session_file(directory, external_session_id, &environment).map_err(|error| {
        let (code, message) = match error {
            PiSessionFileError::Unavailable => (
                "session_source_unavailable",
                format!("Pi session file not found for {external_session_id}"),
            ),
            PiSessionFileError::ScanLimit => (
                "session_source_too_large",
                format!("Pi session lookup exceeded its bounded scan for {external_session_id}"),
            ),
        };
        TranscriptError::new(code, message)
    })?;
    let mut file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(&path)
        .map_err(|error| source_io_error("open Pi session file", error))?;
    let metadata = file
        .metadata()
        .map_err(|error| source_io_error("inspect Pi session file", error))?;
    if !metadata.file_type().is_file() {
        return Err(TranscriptError::new(
            "session_source_unavailable",
            "Pi session source is not a regular file",
        ));
    }
    if metadata.len() > SOURCE_LIMIT_BYTES {
        return Err(TranscriptError::new(
            "session_source_too_large",
            format!("Pi session file exceeds {SOURCE_LIMIT_BYTES} bytes"),
        ));
    }
    let mut output = Vec::with_capacity(metadata.len() as usize);
    file.by_ref()
        .take(SOURCE_LIMIT_BYTES + 1)
        .read_to_end(&mut output)
        .map_err(|error| source_io_error("read Pi session file", error))?;
    if output.len() as u64 > SOURCE_LIMIT_BYTES {
        return Err(TranscriptError::new(
            "session_source_too_large",
            format!("Pi session file exceeds {SOURCE_LIMIT_BYTES} bytes"),
        ));
    }
    parse_pi(&output, external_session_id, directory)
}

fn source_io_error(action: &str, error: io::Error) -> TranscriptError {
    TranscriptError::new(
        "session_source_unavailable",
        format!("could not {action}: {error}"),
    )
}

fn parse_pi(
    output: &[u8],
    external_session_id: &str,
    requested_directory: &Path,
) -> Result<Vec<TranscriptEntry>, TranscriptError> {
    let values = output
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
        .map(serde_json::from_slice::<Value>)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| {
            TranscriptError::new(
                "session_source_invalid",
                format!("Pi session contains invalid JSONL: {error}"),
            )
        })?;
    let header = values
        .first()
        .ok_or_else(|| TranscriptError::new("session_source_invalid", "Pi session is empty"))?;
    let expected_directory = normalize_absolute(requested_directory).ok_or_else(|| {
        TranscriptError::new(
            "session_source_invalid",
            "Pi session working directory is not absolute",
        )
    })?;
    let header_directory = header
        .get("cwd")
        .and_then(Value::as_str)
        .and_then(|cwd| normalize_absolute(Path::new(cwd)));
    if header.get("type").and_then(Value::as_str) != Some("session")
        || header.get("id").and_then(Value::as_str) != Some(external_session_id)
        || header_directory.as_ref() != Some(&expected_directory)
    {
        return Err(TranscriptError::new(
            "session_source_invalid",
            "Pi session header does not match the requested session",
        ));
    }

    let session_entries = &values[1..];
    let by_id = session_entries
        .iter()
        .enumerate()
        .filter_map(|(index, entry)| {
            entry
                .get("id")
                .and_then(Value::as_str)
                .map(|id| (id, index))
        })
        .collect::<HashMap<_, _>>();
    let mut branch = Vec::new();
    let mut current = session_entries.len().checked_sub(1);
    while let Some(index) = current {
        if branch.len() >= session_entries.len() {
            return Err(TranscriptError::new(
                "session_source_invalid",
                "Pi session parent chain contains a cycle",
            ));
        }
        let entry = &session_entries[index];
        branch.push(entry);
        current = entry
            .get("parentId")
            .and_then(Value::as_str)
            .and_then(|parent_id| by_id.get(parent_id).copied());
    }
    branch.reverse();

    let tool_results = branch
        .iter()
        .filter_map(|entry| {
            let message = entry.get("message")?;
            if entry.get("type").and_then(Value::as_str) != Some("message")
                || message.get("role").and_then(Value::as_str) != Some("toolResult")
            {
                return None;
            }
            Some((message.get("toolCallId")?.as_str()?, message))
        })
        .collect::<HashMap<_, _>>();
    let mut entries = Vec::new();
    for entry in branch {
        match entry.get("type").and_then(Value::as_str) {
            Some("message") => normalize_pi_message(entry, &tool_results, &mut entries),
            Some("custom_message")
                if entry.get("display").and_then(Value::as_bool) == Some(true) =>
            {
                if let Some(text) = content_text(entry.get("content")) {
                    entries.push(message_entry(
                        "message",
                        string_field(entry, "id"),
                        None,
                        Some("custom"),
                        &text,
                    ));
                }
            }
            Some("compaction" | "branch_summary") => {
                if let Some(summary) = entry.get("summary").and_then(Value::as_str) {
                    entries.push(message_entry(
                        "message",
                        string_field(entry, "id"),
                        None,
                        Some("summary"),
                        summary,
                    ));
                }
            }
            _ => {}
        }
    }
    Ok(entries)
}

fn normalize_pi_message(
    entry: &Value,
    tool_results: &HashMap<&str, &Value>,
    entries: &mut Vec<TranscriptEntry>,
) {
    let Some(message) = entry.get("message") else {
        return;
    };
    let role = message.get("role").and_then(Value::as_str);
    let timestamp_ms = message.get("timestamp").and_then(Value::as_u64);
    let source_id = string_field(entry, "id");
    match role {
        Some("user") => {
            if let Some(text) = content_text(message.get("content")) {
                entries.push(message_entry(
                    "message",
                    source_id,
                    timestamp_ms,
                    role,
                    &text,
                ));
            }
        }
        Some("assistant") => {
            let Some(blocks) = message.get("content").and_then(Value::as_array) else {
                return;
            };
            for block in blocks {
                match block.get("type").and_then(Value::as_str) {
                    Some("text") => {
                        if let Some(text) = block.get("text").and_then(Value::as_str) {
                            entries.push(message_entry(
                                "message",
                                source_id.clone(),
                                timestamp_ms,
                                role,
                                text,
                            ));
                        }
                    }
                    Some("thinking") => {
                        if let Some(text) = block.get("thinking").and_then(Value::as_str) {
                            entries.push(message_entry(
                                "reasoning",
                                source_id.clone(),
                                timestamp_ms,
                                role,
                                text,
                            ));
                        }
                    }
                    Some("toolCall") => {
                        let call_id = block.get("id").and_then(Value::as_str);
                        let result = call_id.and_then(|call_id| tool_results.get(call_id).copied());
                        entries.push(TranscriptEntry {
                            kind: "tool",
                            source_id: source_id.clone(),
                            timestamp_ms,
                            role: None,
                            text: None,
                            tool_name: block.get("name").and_then(Value::as_str).map(str::to_owned),
                            tool_call_id: call_id.map(str::to_owned),
                            status: result.map(|result| {
                                if result.get("isError").and_then(Value::as_bool) == Some(true) {
                                    "error".to_owned()
                                } else {
                                    "completed".to_owned()
                                }
                            }),
                            input: block.get("arguments").map(compact_json),
                            output: result.and_then(|result| content_text(result.get("content"))),
                            truncated: false,
                        });
                    }
                    _ => {}
                }
            }
        }
        Some("bashExecution") => entries.push(TranscriptEntry {
            kind: "tool",
            source_id,
            timestamp_ms,
            role: None,
            text: None,
            tool_name: Some("bash".to_owned()),
            tool_call_id: None,
            status: Some(
                if message.get("cancelled").and_then(Value::as_bool) == Some(true) {
                    "cancelled".to_owned()
                } else if message.get("exitCode").and_then(Value::as_i64) == Some(0) {
                    "completed".to_owned()
                } else {
                    "error".to_owned()
                },
            ),
            input: message
                .get("command")
                .map(|command| compact_json(&serde_json::json!({ "command": command }))),
            output: message.get("output").map(value_text),
            truncated: message.get("truncated").and_then(Value::as_bool) == Some(true),
        }),
        Some("custom") if message.get("display").and_then(Value::as_bool) == Some(true) => {
            if let Some(text) = content_text(message.get("content")) {
                entries.push(message_entry(
                    "message",
                    source_id,
                    timestamp_ms,
                    Some("custom"),
                    &text,
                ));
            }
        }
        _ => {}
    }
}

fn message_entry(
    kind: &'static str,
    source_id: Option<String>,
    timestamp_ms: Option<u64>,
    role: Option<&str>,
    text: &str,
) -> TranscriptEntry {
    TranscriptEntry {
        kind,
        source_id,
        timestamp_ms,
        role: role.map(str::to_owned),
        text: Some(text.to_owned()),
        tool_name: None,
        tool_call_id: None,
        status: None,
        input: None,
        output: None,
        truncated: false,
    }
}

fn content_text(content: Option<&Value>) -> Option<String> {
    let content = content?;
    if let Some(text) = content.as_str() {
        return (!text.is_empty()).then(|| text.to_owned());
    }
    let text = content
        .as_array()?
        .iter()
        .filter(|block| block.get("type").and_then(Value::as_str) == Some("text"))
        .filter_map(|block| block.get("text").and_then(Value::as_str))
        .collect::<Vec<_>>()
        .join("\n");
    (!text.is_empty()).then_some(text)
}

fn string_field(value: &Value, field: &str) -> Option<String> {
    value.get(field).and_then(Value::as_str).map(str::to_owned)
}

fn compact_json(value: &Value) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| "null".to_owned())
}

fn value_text(value: &Value) -> String {
    value
        .as_str()
        .map_or_else(|| compact_json(value), str::to_owned)
}

fn bound_entries(
    session: &SessionProjection,
    external_session_id: &str,
    entries: Vec<TranscriptEntry>,
    limit: usize,
    max_bytes: usize,
) -> Transcript {
    let total_entries = entries.len();
    let entry_start = total_entries.saturating_sub(limit);
    let limit_truncated = entry_start > 0;
    let selected = entries.into_iter().skip(entry_start).collect::<Vec<_>>();
    let selected_entries = selected.len();
    let mut remaining = max_bytes;
    let mut bounded = Vec::new();
    let mut byte_truncated = false;
    for mut entry in selected.into_iter().rev() {
        let bytes = entry.content_bytes();
        if bytes > remaining {
            byte_truncated = true;
            if remaining == 0 {
                break;
            }
            entry.truncate_to(remaining);
            bounded.push(entry);
            break;
        }
        remaining -= bytes;
        bounded.push(entry);
    }
    if bounded.len() < selected_entries {
        byte_truncated = true;
    }
    bounded.reverse();
    let content_bytes = bounded.iter().map(TranscriptEntry::content_bytes).sum();
    let mut truncated_by = Vec::new();
    if limit_truncated {
        truncated_by.push("limit");
    }
    if byte_truncated {
        truncated_by.push("max_bytes");
    }
    Transcript {
        session_id: session.id.clone(),
        integration: session.integration.clone(),
        external_session_id: external_session_id.to_owned(),
        returned_entries: bounded.len(),
        total_entries,
        content_bytes,
        entries: bounded,
        truncated: !truncated_by.is_empty(),
        truncated_by,
    }
}

impl TranscriptEntry {
    fn content_bytes(&self) -> usize {
        [&self.text, &self.input, &self.output]
            .into_iter()
            .filter_map(|value| value.as_ref())
            .map(String::len)
            .sum()
    }

    fn truncate_to(&mut self, mut remaining: usize) {
        for value in [&mut self.text, &mut self.input, &mut self.output]
            .into_iter()
            .filter_map(Option::as_mut)
        {
            if value.len() <= remaining {
                remaining -= value.len();
                continue;
            }
            truncate_utf8(value, remaining);
            remaining = 0;
        }
        self.truncated = true;
    }
}

fn truncate_utf8(value: &mut String, max_bytes: usize) {
    if value.len() <= max_bytes {
        return;
    }
    let mut boundary = max_bytes;
    while !value.is_char_boundary(boundary) {
        boundary -= 1;
    }
    value.truncate(boundary);
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use boomux::protocol::{AgentAuthority, AgentObservationSnapshot, AgentState};

    use super::*;
    use crate::session_projection::SessionOccurrence;

    struct FutureHarnessAdapter;

    impl TranscriptAdapter for FutureHarnessAdapter {
        fn integration(&self) -> &'static str {
            "future-harness"
        }

        fn read(
            &self,
            request: TranscriptRequest<'_>,
        ) -> Result<Vec<TranscriptEntry>, TranscriptError> {
            assert_eq!(request.directory, Path::new("/repo"));
            assert_eq!(request.external_session_id, "external");
            Ok(vec![message_entry(
                "message",
                Some("future-1".into()),
                Some(10),
                Some("assistant"),
                "adapter output",
            )])
        }
    }

    fn session(integration: &str) -> SessionProjection {
        SessionProjection {
            id: "projected".into(),
            workspace_id: "workspace".into(),
            workspace_name: "project".into(),
            integration: integration.into(),
            external_session_id: Some("external".into()),
            description: "Agent".into(),
            state: AgentState::Idle,
            state_is_current: true,
            started_at_ms: 1,
            last_at_ms: 2,
            occurrences: vec![SessionOccurrence {
                agent_id: "agent".into(),
                shell_id: "shell".into(),
                run_id: "run".into(),
                started_at_ms: 1,
                ended_at_ms: None,
                observation: AgentObservationSnapshot {
                    revision: 1,
                    state: AgentState::Idle,
                    authority: AgentAuthority::LifecycleIntegration,
                    evidence: "idle".into(),
                    confidence: 100,
                    observed_at_ms: 2,
                },
                is_current: true,
                retained_shell_name: Some("agent".into()),
                retained_shell_cwd: Some(PathBuf::from("/repo")),
            }],
        }
    }

    #[test]
    fn parses_opencode_text_reasoning_and_completed_tool() {
        let output = br#"{
            "info":{"id":"external"},
            "messages":[
                {"info":{"role":"user","time":{"created":10}},"parts":[
                    {"id":"p1","type":"text","text":"hello"}
                ]},
                {"info":{"role":"assistant","time":{"created":20}},"parts":[
                    {"id":"p2","type":"reasoning","text":"consider"},
                    {"id":"p3","type":"tool","tool":"read","callID":"call-1","state":{"status":"completed","input":{"path":"a"},"output":"contents"}}
                ]}
            ]
        }"#;

        let entries = parse_opencode(output, "external").unwrap();

        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0].kind, "message");
        assert_eq!(entries[0].text.as_deref(), Some("hello"));
        assert_eq!(entries[1].kind, "reasoning");
        assert_eq!(entries[2].tool_name.as_deref(), Some("read"));
        assert_eq!(entries[2].input.as_deref(), Some(r#"{"path":"a"}"#));
        assert_eq!(entries[2].output.as_deref(), Some("contents"));
    }

    #[test]
    fn pi_follows_latest_branch_and_combines_tool_result() {
        let output = br#"{"type":"session","version":3,"id":"external","cwd":"/repo"}
{"type":"message","id":"one","parentId":null,"timestamp":"x","message":{"role":"user","content":"start","timestamp":1}}
{"type":"message","id":"abandoned","parentId":"one","timestamp":"x","message":{"role":"assistant","content":[{"type":"text","text":"old branch"}],"timestamp":2}}
{"type":"message","id":"call","parentId":"one","timestamp":"x","message":{"role":"assistant","content":[{"type":"thinking","thinking":"plan"},{"type":"toolCall","id":"tc1","name":"read","arguments":{"path":"a"}}],"timestamp":3}}
{"type":"message","id":"result","parentId":"call","timestamp":"x","message":{"role":"toolResult","toolCallId":"tc1","toolName":"read","content":[{"type":"text","text":"data"}],"isError":false,"timestamp":4}}
{"type":"message","id":"custom","parentId":"result","timestamp":"x","message":{"role":"custom","customType":"notice","content":"visible notice","display":true,"timestamp":5}}
"#;

        let entries = parse_pi(output, "external", Path::new("/repo")).unwrap();

        assert_eq!(entries.len(), 4);
        assert_eq!(entries[0].text.as_deref(), Some("start"));
        assert_eq!(entries[1].kind, "reasoning");
        assert_eq!(entries[2].kind, "tool");
        assert_eq!(entries[2].status.as_deref(), Some("completed"));
        assert_eq!(entries[2].output.as_deref(), Some("data"));
        assert_eq!(entries[3].role.as_deref(), Some("custom"));
        assert_eq!(entries[3].text.as_deref(), Some("visible notice"));
        assert!(
            entries
                .iter()
                .all(|entry| entry.text.as_deref() != Some("old branch"))
        );
    }

    #[test]
    fn bounds_the_newest_suffix_by_entries_and_utf8_content_bytes() {
        let entries = vec![
            message_entry("message", Some("1".into()), None, Some("user"), "old"),
            message_entry(
                "message",
                Some("2".into()),
                None,
                Some("assistant"),
                "middle",
            ),
            message_entry(
                "message",
                Some("3".into()),
                None,
                Some("assistant"),
                "new-😀",
            ),
        ];

        let transcript = bound_entries(&session("pi"), "external", entries, 2, 8);

        assert_eq!(transcript.total_entries, 3);
        assert_eq!(transcript.returned_entries, 1);
        assert_eq!(transcript.entries[0].source_id.as_deref(), Some("3"));
        assert_eq!(transcript.entries[0].text.as_deref(), Some("new-😀"));
        assert_eq!(transcript.content_bytes, 8);
        assert_eq!(transcript.truncated_by, ["limit", "max_bytes"]);
    }

    #[test]
    fn registered_adapter_uses_the_shared_identity_and_bounding_contract() {
        let adapter = FutureHarnessAdapter;
        let transcript = read_with_adapters(
            &session("future-harness"),
            10,
            7,
            &[&adapter as &dyn TranscriptAdapter],
        )
        .unwrap();

        assert_eq!(transcript.integration, "future-harness");
        assert_eq!(transcript.entries[0].text.as_deref(), Some("adapter"));
        assert_eq!(transcript.truncated_by, ["max_bytes"]);
    }

    #[test]
    fn registry_advertises_every_bundled_adapter() {
        assert_eq!(supported_integrations(), ["opencode", "pi"]);
    }
}

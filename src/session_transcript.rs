use std::collections::HashMap;
use std::error::Error;
use std::fs::OpenOptions;
use std::io::{self, Read};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, TryRecvError};
use std::thread;
use std::time::{Duration, Instant};

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::host_session_titles::{
    PiEnvironment, PiSessionFileError, normalize_absolute, pi_session_file,
};
use crate::session_projection::SessionProjection;

const SOURCE_LIMIT_BYTES: u64 = 16 * 1024 * 1024;
const OPENCODE_TIMEOUT: Duration = Duration::from_secs(30);
const CURSOR_PREFIX: &str = "v1.";
const CURSOR_MAX_BYTES: usize = 4096;

trait TranscriptAdapter: Sync {
    fn integration(&self) -> &'static str;

    fn normalization_revision(&self) -> u32;

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
    pub(crate) has_more: bool,
    pub(crate) next_cursor: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
struct Cursor {
    v: u32,
    n: u32,
    s: String,
    i: String,
    x: String,
    c: u64,
    h: String,
    e: u64,
}

struct PageBounds {
    normalization_revision: u32,
    source_fingerprint: String,
    baseline_count: usize,
    baseline_fingerprint: String,
    end: usize,
    limit: usize,
    max_bytes: usize,
}

pub(crate) fn read(
    session: &SessionProjection,
    before: Option<&str>,
    limit: usize,
    max_bytes: usize,
) -> Result<Transcript, TranscriptError> {
    read_with_adapters(session, before, limit, max_bytes, ADAPTERS)
}

pub(crate) fn supported_integrations() -> Vec<&'static str> {
    ADAPTERS
        .iter()
        .map(|adapter| adapter.integration())
        .collect()
}

fn read_with_adapters(
    session: &SessionProjection,
    before: Option<&str>,
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
        .find_map(|occurrence| occurrence.source_cwd.as_deref())
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
    let source_fingerprint =
        source_context_fingerprint(&session.integration, external_session_id, directory)?;
    let cursor = before.map(decode_cursor).transpose()?;
    if let Some(cursor) = cursor.as_ref() {
        if cursor.s != session.id || cursor.i != session.integration {
            return Err(invalid_cursor("cursor belongs to a different session"));
        }
        if cursor.n != adapter.normalization_revision() || cursor.x != source_fingerprint {
            return Err(expired_cursor());
        }
        if cursor.e > cursor.c {
            return Err(invalid_cursor("cursor entry index is out of range"));
        }
    }
    let entries = adapter.read(TranscriptRequest {
        directory,
        external_session_id,
    })?;
    let (baseline_count, baseline_fingerprint, end) = if let Some(cursor) = cursor {
        let baseline_count = usize::try_from(cursor.c)
            .map_err(|_| invalid_cursor("cursor entry count is out of range"))?;
        let end = usize::try_from(cursor.e)
            .map_err(|_| invalid_cursor("cursor entry index is out of range"))?;
        if entries.len() < baseline_count
            || normalized_prefix_fingerprint(&entries[..baseline_count]) != cursor.h
        {
            return Err(expired_cursor());
        }
        (baseline_count, cursor.h, end)
    } else {
        let baseline_count = entries.len();
        (
            baseline_count,
            normalized_prefix_fingerprint(&entries),
            baseline_count,
        )
    };
    Ok(bound_entries(
        session,
        external_session_id,
        entries,
        PageBounds {
            normalization_revision: adapter.normalization_revision(),
            source_fingerprint,
            baseline_count,
            baseline_fingerprint,
            end,
            limit,
            max_bytes,
        },
    ))
}

impl TranscriptAdapter for OpenCodeAdapter {
    fn integration(&self) -> &'static str {
        "opencode"
    }

    fn normalization_revision(&self) -> u32 {
        1
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

    fn normalization_revision(&self) -> u32 {
        1
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

fn source_context_fingerprint(
    integration: &str,
    external_session_id: &str,
    directory: &Path,
) -> Result<String, TranscriptError> {
    let directory = normalize_absolute(directory).ok_or_else(|| {
        TranscriptError::new(
            "session_source_unavailable",
            "session retained working directory is not absolute",
        )
    })?;
    Ok(hash_fields([
        integration.as_bytes(),
        external_session_id.as_bytes(),
        directory.as_os_str().as_bytes(),
    ]))
}

fn normalized_prefix_fingerprint(entries: &[TranscriptEntry]) -> String {
    let mut hasher = Sha256::new();
    for entry in entries {
        let serialized = serde_json::to_vec(entry).expect("transcript entries serialize");
        hash_field(&mut hasher, &serialized);
    }
    URL_SAFE_NO_PAD.encode(hasher.finalize())
}

fn hash_fields<'a>(fields: impl IntoIterator<Item = &'a [u8]>) -> String {
    let mut hasher = Sha256::new();
    for field in fields {
        hash_field(&mut hasher, field);
    }
    URL_SAFE_NO_PAD.encode(hasher.finalize())
}

fn hash_field(hasher: &mut Sha256, field: &[u8]) {
    hasher.update((field.len() as u64).to_be_bytes());
    hasher.update(field);
}

fn decode_cursor(encoded: &str) -> Result<Cursor, TranscriptError> {
    if encoded.len() > CURSOR_MAX_BYTES {
        return Err(invalid_cursor("cursor is too long"));
    }
    let payload = encoded
        .strip_prefix(CURSOR_PREFIX)
        .ok_or_else(|| invalid_cursor("cursor has an unknown schema"))?;
    let bytes = URL_SAFE_NO_PAD
        .decode(payload)
        .map_err(|_| invalid_cursor("cursor is malformed"))?;
    let cursor: Cursor =
        serde_json::from_slice(&bytes).map_err(|_| invalid_cursor("cursor is malformed"))?;
    if cursor.v != 1 {
        return Err(invalid_cursor("cursor has an unknown schema"));
    }
    Ok(cursor)
}

fn encode_cursor(cursor: &Cursor) -> String {
    let payload = serde_json::to_vec(cursor).expect("cursor serializes");
    format!("{CURSOR_PREFIX}{}", URL_SAFE_NO_PAD.encode(payload))
}

fn invalid_cursor(message: &'static str) -> TranscriptError {
    TranscriptError::new("invalid_argument", message)
}

fn expired_cursor() -> TranscriptError {
    TranscriptError::new("cursor_expired", "transcript cursor has expired")
}

fn bound_entries(
    session: &SessionProjection,
    external_session_id: &str,
    entries: Vec<TranscriptEntry>,
    page: PageBounds,
) -> Transcript {
    let PageBounds {
        normalization_revision,
        source_fingerprint,
        baseline_count,
        baseline_fingerprint,
        end,
        limit,
        max_bytes,
    } = page;
    let total_entries = baseline_count;
    let entry_start = end.saturating_sub(limit);
    let limit_truncated = entry_start > 0;
    let selected = entries
        .into_iter()
        .take(end)
        .skip(entry_start)
        .enumerate()
        .map(|(offset, entry)| (entry_start + offset, entry))
        .collect::<Vec<_>>();
    let selected_entries = selected.len();
    let mut remaining = max_bytes;
    let mut bounded = Vec::new();
    let mut byte_truncated = false;
    let mut oldest_index = end;
    for (index, mut entry) in selected.into_iter().rev() {
        let bytes = entry.content_bytes();
        if bytes > remaining {
            byte_truncated = true;
            if remaining == 0 && !bounded.is_empty() {
                break;
            }
            entry.truncate_to(remaining);
            oldest_index = index;
            bounded.push((index, entry));
            break;
        }
        remaining -= bytes;
        oldest_index = index;
        bounded.push((index, entry));
    }
    if bounded.len() < selected_entries {
        byte_truncated = true;
    }
    bounded.reverse();
    let entries = bounded
        .into_iter()
        .map(|(_, entry)| entry)
        .collect::<Vec<_>>();
    let content_bytes = entries.iter().map(TranscriptEntry::content_bytes).sum();
    let mut truncated_by = Vec::new();
    if limit_truncated {
        truncated_by.push("limit");
    }
    if byte_truncated {
        truncated_by.push("max_bytes");
    }
    let next_end = oldest_index;
    let has_more = next_end > 0;
    let next_cursor = has_more.then(|| {
        encode_cursor(&Cursor {
            v: 1,
            n: normalization_revision,
            s: session.id.clone(),
            i: session.integration.clone(),
            x: source_fingerprint,
            c: baseline_count as u64,
            h: baseline_fingerprint,
            e: next_end as u64,
        })
    });
    Transcript {
        session_id: session.id.clone(),
        integration: session.integration.clone(),
        external_session_id: external_session_id.to_owned(),
        returned_entries: entries.len(),
        total_entries,
        content_bytes,
        entries,
        truncated: !truncated_by.is_empty(),
        truncated_by,
        has_more,
        next_cursor,
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
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicU32, Ordering};

    use boomux::protocol::{AgentAuthority, AgentObservationSnapshot, AgentState};

    use super::*;
    use crate::session_projection::SessionOccurrence;

    struct FutureHarnessAdapter;

    impl TranscriptAdapter for FutureHarnessAdapter {
        fn integration(&self) -> &'static str {
            "future-harness"
        }

        fn normalization_revision(&self) -> u32 {
            1
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

    struct MutableAdapter {
        entries: Mutex<Vec<TranscriptEntry>>,
        revision: AtomicU32,
    }

    impl MutableAdapter {
        fn new(entries: Vec<TranscriptEntry>) -> Self {
            Self {
                entries: Mutex::new(entries),
                revision: AtomicU32::new(1),
            }
        }
    }

    impl TranscriptAdapter for MutableAdapter {
        fn integration(&self) -> &'static str {
            "mutable"
        }

        fn normalization_revision(&self) -> u32 {
            self.revision.load(Ordering::Relaxed)
        }

        fn read(
            &self,
            _request: TranscriptRequest<'_>,
        ) -> Result<Vec<TranscriptEntry>, TranscriptError> {
            Ok(self.entries.lock().unwrap().clone())
        }
    }

    fn entries(ids: &[&str]) -> Vec<TranscriptEntry> {
        ids.iter()
            .map(|id| message_entry("message", Some((*id).into()), None, Some("user"), id))
            .collect()
    }

    fn read_mutable(
        session: &SessionProjection,
        adapter: &MutableAdapter,
        before: Option<&str>,
        limit: usize,
        max_bytes: usize,
    ) -> Result<Transcript, TranscriptError> {
        read_with_adapters(
            session,
            before,
            limit,
            max_bytes,
            &[adapter as &dyn TranscriptAdapter],
        )
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
                source_cwd: Some(PathBuf::from("/repo")),
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

        let transcript = bound_entries(
            &session("pi"),
            "external",
            entries,
            PageBounds {
                normalization_revision: 1,
                source_fingerprint: "source".into(),
                baseline_count: 3,
                baseline_fingerprint: "baseline".into(),
                end: 3,
                limit: 2,
                max_bytes: 8,
            },
        );

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
            None,
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
    fn cursor_roundtrips_and_rejects_bad_representations() {
        let cursor = Cursor {
            v: 1,
            n: 2,
            s: "session".into(),
            i: "integration".into(),
            x: "source".into(),
            c: 10,
            h: "baseline".into(),
            e: 4,
        };
        let encoded = encode_cursor(&cursor);
        let decoded = decode_cursor(&encoded).unwrap();
        assert_eq!(decoded.s, "session");
        assert_eq!(decoded.e, 4);

        for bad in ["garbage", "v1.not-base64", "v2.e30"] {
            assert_eq!(decode_cursor(bad).unwrap_err().code, "invalid_argument");
        }
        let unknown =
            URL_SAFE_NO_PAD.encode(br#"{"v":2,"n":1,"s":"s","i":"i","x":"x","c":0,"h":"h","e":0}"#);
        assert_eq!(
            decode_cursor(&format!("v1.{unknown}")).unwrap_err().code,
            "invalid_argument"
        );
        assert_eq!(
            decode_cursor(&"x".repeat(CURSOR_MAX_BYTES + 1))
                .unwrap_err()
                .code,
            "invalid_argument"
        );
    }

    #[test]
    fn paginates_exactly_in_chronological_pages_with_changed_bounds() {
        let adapter = MutableAdapter::new(entries(&["1", "2", "3", "4", "5"]));
        let session = session("mutable");

        let first = read_mutable(&session, &adapter, None, 2, usize::MAX).unwrap();
        assert_eq!(source_ids(&first), ["4", "5"]);
        assert_eq!(first.total_entries, 5);
        assert_eq!(first.truncated_by, ["limit"]);

        let second = read_mutable(
            &session,
            &adapter,
            first.next_cursor.as_deref(),
            1,
            usize::MAX,
        )
        .unwrap();
        assert_eq!(source_ids(&second), ["3"]);
        assert_eq!(second.total_entries, 5);
        assert_eq!(second.truncated_by, ["limit"]);

        let third = read_mutable(
            &session,
            &adapter,
            second.next_cursor.as_deref(),
            10,
            usize::MAX,
        )
        .unwrap();
        assert_eq!(source_ids(&third), ["1", "2"]);
        assert!(!third.has_more);
        assert!(third.next_cursor.is_none());
        assert!(third.truncated_by.is_empty());
    }

    #[test]
    fn append_is_ignored_but_prefix_mutation_or_removal_expires_cursor() {
        let adapter = MutableAdapter::new(entries(&["1", "2", "3"]));
        let session = session("mutable");
        let first = read_mutable(&session, &adapter, None, 1, usize::MAX).unwrap();
        let cursor = first.next_cursor.unwrap();

        adapter
            .entries
            .lock()
            .unwrap()
            .push(entries(&["4"]).remove(0));
        let continued = read_mutable(&session, &adapter, Some(&cursor), 10, usize::MAX).unwrap();
        assert_eq!(source_ids(&continued), ["1", "2"]);
        assert_eq!(continued.total_entries, 3);

        adapter.entries.lock().unwrap()[0].text = Some("changed".into());
        assert_eq!(
            read_mutable(&session, &adapter, Some(&cursor), 10, usize::MAX)
                .unwrap_err()
                .code,
            "cursor_expired"
        );
        *adapter.entries.lock().unwrap() = entries(&["1", "2"]);
        assert_eq!(
            read_mutable(&session, &adapter, Some(&cursor), 10, usize::MAX)
                .unwrap_err()
                .code,
            "cursor_expired"
        );
    }

    #[test]
    fn pi_tool_results_and_branch_changes_expire_the_baseline() {
        let pending = br#"{"type":"session","version":3,"id":"external","cwd":"/repo"}
{"type":"message","id":"user","parentId":null,"message":{"role":"user","content":"start","timestamp":1}}
{"type":"message","id":"call","parentId":"user","message":{"role":"assistant","content":[{"type":"toolCall","id":"tc1","name":"read","arguments":{"path":"a"}}],"timestamp":2}}
"#;
        let completed = br#"{"type":"session","version":3,"id":"external","cwd":"/repo"}
{"type":"message","id":"user","parentId":null,"message":{"role":"user","content":"start","timestamp":1}}
{"type":"message","id":"call","parentId":"user","message":{"role":"assistant","content":[{"type":"toolCall","id":"tc1","name":"read","arguments":{"path":"a"}}],"timestamp":2}}
{"type":"message","id":"result","parentId":"call","message":{"role":"toolResult","toolCallId":"tc1","content":"data","isError":false,"timestamp":3}}
"#;
        let adapter =
            MutableAdapter::new(parse_pi(pending, "external", Path::new("/repo")).unwrap());
        let session = session("mutable");
        let first = read_mutable(&session, &adapter, None, 1, usize::MAX).unwrap();
        let cursor = first.next_cursor.unwrap();
        *adapter.entries.lock().unwrap() =
            parse_pi(completed, "external", Path::new("/repo")).unwrap();
        assert_eq!(
            read_mutable(&session, &adapter, Some(&cursor), 1, usize::MAX)
                .unwrap_err()
                .code,
            "cursor_expired"
        );

        *adapter.entries.lock().unwrap() = entries(&["root", "old-leaf"]);
        let first = read_mutable(&session, &adapter, None, 1, usize::MAX).unwrap();
        *adapter.entries.lock().unwrap() = entries(&["root", "new-leaf"]);
        assert_eq!(
            read_mutable(
                &session,
                &adapter,
                first.next_cursor.as_deref(),
                1,
                usize::MAX,
            )
            .unwrap_err()
            .code,
            "cursor_expired"
        );
    }

    #[test]
    fn cursor_rejects_wrong_identity_and_expires_for_source_or_normalization() {
        let adapter = MutableAdapter::new(entries(&["1", "2"]));
        let original = session("mutable");
        let first = read_mutable(&original, &adapter, None, 1, usize::MAX).unwrap();
        let cursor = first.next_cursor.unwrap();

        let mut out_of_range = decode_cursor(&cursor).unwrap();
        out_of_range.e = out_of_range.c + 1;
        assert_eq!(
            read_mutable(
                &original,
                &adapter,
                Some(&encode_cursor(&out_of_range)),
                1,
                usize::MAX,
            )
            .unwrap_err()
            .code,
            "invalid_argument"
        );

        let mut wrong = original.clone();
        wrong.id = "other".into();
        assert_eq!(
            read_mutable(&wrong, &adapter, Some(&cursor), 1, usize::MAX)
                .unwrap_err()
                .code,
            "invalid_argument"
        );

        let mut moved = original.clone();
        moved.occurrences[0].source_cwd = Some(PathBuf::from("/other"));
        assert_eq!(
            read_mutable(&moved, &adapter, Some(&cursor), 1, usize::MAX)
                .unwrap_err()
                .code,
            "cursor_expired"
        );

        adapter.revision.store(2, Ordering::Relaxed);
        assert_eq!(
            read_mutable(&original, &adapter, Some(&cursor), 1, usize::MAX)
                .unwrap_err()
                .code,
            "cursor_expired"
        );
    }

    #[test]
    fn byte_clipping_consumes_a_logical_entry_even_at_zero_utf8_bytes() {
        let adapter = MutableAdapter::new(vec![
            message_entry("message", Some("old".into()), None, Some("user"), "old"),
            message_entry("message", Some("emoji".into()), None, Some("user"), "😀"),
        ]);
        let session = session("mutable");

        let first = read_mutable(&session, &adapter, None, 10, 1).unwrap();
        assert_eq!(source_ids(&first), ["emoji"]);
        assert_eq!(first.entries[0].text.as_deref(), Some(""));
        assert_eq!(first.content_bytes, 0);
        assert_eq!(first.truncated_by, ["max_bytes"]);

        let second = read_mutable(
            &session,
            &adapter,
            first.next_cursor.as_deref(),
            10,
            usize::MAX,
        )
        .unwrap();
        assert_eq!(source_ids(&second), ["old"]);
        assert!(!second.has_more);
    }

    #[test]
    fn empty_source_has_no_cursor() {
        let adapter = MutableAdapter::new(Vec::new());
        let transcript = read_mutable(&session("mutable"), &adapter, None, 10, 10).unwrap();
        assert_eq!(transcript.total_entries, 0);
        assert_eq!(transcript.returned_entries, 0);
        assert!(!transcript.has_more);
        assert!(transcript.next_cursor.is_none());
    }

    fn source_ids(transcript: &Transcript) -> Vec<&str> {
        transcript
            .entries
            .iter()
            .map(|entry| entry.source_id.as_deref().unwrap())
            .collect()
    }

    #[test]
    fn registry_advertises_every_bundled_adapter() {
        assert_eq!(supported_integrations(), ["opencode", "pi"]);
    }
}

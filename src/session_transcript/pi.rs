use std::collections::HashMap;
use std::fs::OpenOptions;
use std::io::{self, Read};
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

use serde_json::Value;

use super::{
    SOURCE_LIMIT_BYTES, TranscriptAdapter, TranscriptEntry, TranscriptError, TranscriptRequest,
    compact_json, message_entry, value_text,
};
use crate::host_session_source::{
    normalize_absolute,
    pi::{Environment, SessionFileError, session_file},
};

struct PiAdapter;

pub(super) static ADAPTER: &dyn TranscriptAdapter = &PiAdapter;

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
        read(request.directory, request.external_session_id)
    }
}

fn read(
    directory: &Path,
    external_session_id: &str,
) -> Result<Vec<TranscriptEntry>, TranscriptError> {
    let environment = Environment::from_process();
    let path = session_file(directory, external_session_id, &environment).map_err(|error| {
        let (code, message) = match error {
            SessionFileError::Unavailable => (
                "session_source_unavailable",
                format!("Pi session file not found for {external_session_id}"),
            ),
            SessionFileError::ScanLimit => (
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
    parse(&output, external_session_id, directory)
}

fn source_io_error(action: &str, error: io::Error) -> TranscriptError {
    TranscriptError::new(
        "session_source_unavailable",
        format!("could not {action}: {error}"),
    )
}

pub(super) fn parse(
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
            Some("message") => normalize_message(entry, &tool_results, &mut entries),
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

fn normalize_message(
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

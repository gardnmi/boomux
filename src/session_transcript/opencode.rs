use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, TryRecvError};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::Value;

use super::{
    SOURCE_LIMIT_BYTES, TranscriptAdapter, TranscriptEntry, TranscriptError, TranscriptRequest,
    compact_json, message_entry, value_text,
};

const TIMEOUT: Duration = Duration::from_secs(30);

struct OpenCodeAdapter;

pub(super) static ADAPTER: &dyn TranscriptAdapter = &OpenCodeAdapter;

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
        let output = run_export(request.directory, request.external_session_id)?;
        parse(&output, request.external_session_id)
    }
}

fn run_export(directory: &Path, external_session_id: &str) -> Result<Vec<u8>, TranscriptError> {
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

    let deadline = Instant::now() + TIMEOUT;
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

pub(super) fn parse(
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

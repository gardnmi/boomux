use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom};
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::Value;
use uuid::Uuid;

use super::{
    SOURCE_LIMIT_BYTES, TranscriptAdapter, TranscriptEntry, TranscriptError, TranscriptRequest,
    compact_json, message_entry, value_text,
};

const TIMEOUT: Duration = Duration::from_secs(30);

struct OpenCodeAdapter;

pub(super) static ADAPTER: &dyn TranscriptAdapter = &OpenCodeAdapter;

impl TranscriptAdapter for OpenCodeAdapter {
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
    run_export_command("opencode", &["export", external_session_id], directory)
}

fn run_export_command(
    program: &str,
    arguments: &[&str],
    directory: &Path,
) -> Result<Vec<u8>, TranscriptError> {
    // OpenCode exits without draining piped stdout, so use an unlinked regular file.
    let mut output = export_output()?;
    let child_output = output.try_clone().map_err(|error| {
        TranscriptError::new(
            "session_source_unavailable",
            format!("could not capture OpenCode export: {error}"),
        )
    })?;
    let mut child = Command::new(program)
        .args(arguments)
        .current_dir(directory)
        .stdin(Stdio::null())
        .stdout(Stdio::from(child_output))
        .stderr(Stdio::null())
        .spawn()
        .map_err(|error| {
            TranscriptError::new(
                "session_source_unavailable",
                format!("could not start OpenCode export: {error}"),
            )
        })?;

    let deadline = Instant::now() + TIMEOUT;
    loop {
        let output_len = match output.metadata() {
            Ok(metadata) => metadata.len(),
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(TranscriptError::new(
                    "session_source_unavailable",
                    format!("could not inspect OpenCode export: {error}"),
                ));
            }
        };
        if output_len > SOURCE_LIMIT_BYTES {
            let _ = child.kill();
            let _ = child.wait();
            return Err(TranscriptError::new(
                "session_source_too_large",
                format!("OpenCode export exceeds {SOURCE_LIMIT_BYTES} bytes"),
            ));
        }
        let status = match child.try_wait() {
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
        if let Some(status) = status {
            if status.success() {
                output.seek(SeekFrom::Start(0)).map_err(export_read_error)?;
                let mut bytes = Vec::new();
                output
                    .take(SOURCE_LIMIT_BYTES + 1)
                    .read_to_end(&mut bytes)
                    .map_err(export_read_error)?;
                if bytes.len() as u64 > SOURCE_LIMIT_BYTES {
                    return Err(TranscriptError::new(
                        "session_source_too_large",
                        format!("OpenCode export exceeds {SOURCE_LIMIT_BYTES} bytes"),
                    ));
                }
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

fn export_output() -> Result<File, TranscriptError> {
    let path = std::env::temp_dir().join(format!("boomux-opencode-export-{}", Uuid::new_v4()));
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_CLOEXEC)
        .open(&path)
        .map_err(|error| {
            TranscriptError::new(
                "session_source_unavailable",
                format!("could not create OpenCode export capture: {error}"),
            )
        })?;
    fs::remove_file(path).map_err(|error| {
        TranscriptError::new(
            "session_source_unavailable",
            format!("could not unlink OpenCode export capture: {error}"),
        )
    })?;
    Ok(file)
}

fn export_read_error(error: std::io::Error) -> TranscriptError {
    TranscriptError::new(
        "session_source_unavailable",
        format!("could not read OpenCode export: {error}"),
    )
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn captures_exports_larger_than_a_pipe_buffer() {
        let output =
            run_export_command("/bin/sh", &["-c", "yes x | head -c 524288"], Path::new("/"))
                .expect("capture succeeds");

        assert_eq!(output.len(), 524_288);
        assert!(output.starts_with(b"x\n"));
        assert!(output.ends_with(b"x\n"));
    }
}

use std::collections::HashMap;
use std::path::Path;

use super::{MAX_SESSIONS, TitleAdapter, Titles, sanitize_title};
use crate::host_session_source::{
    normalize_absolute,
    pi::{Environment, session_catalog},
};

struct PiAdapter;

pub(super) static ADAPTER: &dyn TitleAdapter = &PiAdapter;

impl TitleAdapter for PiAdapter {
    fn integration(&self) -> &'static str {
        "pi"
    }

    fn inspect(&self, directory: &Path) -> Option<Titles> {
        inspect(directory, &Environment::from_process())
    }
}

pub(super) fn inspect(directory: &Path, environment: &Environment) -> Option<Titles> {
    let (normalized_directory, prefixes) = session_catalog(directory, environment, MAX_SESSIONS)?;
    let mut titles = HashMap::new();

    for prefix in prefixes {
        if let Some((id, title)) = parse_session(&prefix, &normalized_directory) {
            titles.entry(id).or_insert(title);
        }
    }
    Some(titles)
}

pub(super) fn parse_session(output: &[u8], requested_directory: &Path) -> Option<(String, String)> {
    let mut entries = output
        .split(|byte| *byte == b'\n')
        .filter_map(|line| serde_json::from_slice::<serde_json::Value>(line).ok());
    let header = entries.next()?;
    if header.get("type")?.as_str()? != "session" {
        return None;
    }
    let id = header.get("id")?.as_str()?;
    if id.is_empty() {
        return None;
    }
    let header_directory = normalize_absolute(Path::new(header.get("cwd")?.as_str()?))?;
    if header_directory != requested_directory {
        return None;
    }

    let mut explicit_name = None;
    let mut saw_session_info = false;
    let mut first_user_summary = None;
    for entry in entries {
        match entry.get("type").and_then(serde_json::Value::as_str) {
            Some("session_info") => {
                saw_session_info = true;
                explicit_name = entry
                    .get("name")
                    .and_then(serde_json::Value::as_str)
                    .and_then(sanitize_title);
            }
            Some("message") if first_user_summary.is_none() => {
                let Some(message) = entry.get("message") else {
                    continue;
                };
                if message.get("role").and_then(serde_json::Value::as_str) != Some("user") {
                    continue;
                }
                first_user_summary = message
                    .get("content")
                    .and_then(message_text)
                    .and_then(|text| sanitize_title(&text));
            }
            _ => {}
        }
    }
    match (saw_session_info, explicit_name) {
        (true, Some(name)) => Some(name),
        _ => first_user_summary,
    }
    .map(|title| (id.to_owned(), title))
}

fn message_text(content: &serde_json::Value) -> Option<String> {
    if let Some(text) = content.as_str() {
        return Some(text.to_owned());
    }
    let blocks = content.as_array()?;
    let text = blocks
        .iter()
        .filter(|block| block.get("type").and_then(serde_json::Value::as_str) == Some("text"))
        .filter_map(|block| block.get("text").and_then(serde_json::Value::as_str))
        .collect::<Vec<_>>()
        .join(" ");
    (!text.is_empty()).then_some(text)
}

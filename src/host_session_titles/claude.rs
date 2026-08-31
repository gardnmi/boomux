use std::collections::HashMap;
use std::path::Path;

use super::{HostSession, Inspection, MAX_SESSIONS, TitleAdapter, sanitize_title};
use crate::host_session_source::{
    claude::{Environment, session_catalog},
    normalize_absolute,
};

struct ClaudeAdapter;

pub(super) static ADAPTER: &dyn TitleAdapter = &ClaudeAdapter;

impl TitleAdapter for ClaudeAdapter {
    fn inspect(&self, directory: &Path) -> Option<Inspection> {
        inspect(directory, &Environment::from_process())
    }
}

pub(super) fn inspect(directory: &Path, environment: &Environment) -> Option<Inspection> {
    let (normalized_directory, prefixes) = session_catalog(directory, environment, MAX_SESSIONS)?;
    let mut titles = HashMap::new();
    let mut catalog = Vec::new();

    for prefix in prefixes {
        if titles.contains_key(&prefix.root_id) {
            continue;
        }
        let Some(title) = parse_transcript(&prefix.bytes, &normalized_directory, &prefix.root_id)
        else {
            continue;
        };
        titles.insert(prefix.root_id.clone(), title.clone());
        catalog.push(HostSession {
            integration: boomux::integrations::CLAUDE.key.into(),
            root_id: prefix.root_id,
            title,
            directory: normalized_directory.clone(),
            created_at_ms: 0,
            updated_at_ms: 0,
        });
    }
    Some(Inspection { titles, catalog })
}

pub(super) fn parse_transcript(
    output: &[u8],
    requested_directory: &Path,
    expected_session_id: &str,
) -> Option<String> {
    if boomux::integrations::validate_external_session_id(expected_session_id).is_err() {
        return None;
    }
    let requested_directory = normalize_absolute(requested_directory)?;
    let mut saw_matching_directory = false;
    let mut title = None;

    for entry in output
        .split(|byte| *byte == b'\n')
        .filter_map(|line| serde_json::from_slice::<serde_json::Value>(line).ok())
    {
        if entry.get("sessionId").and_then(serde_json::Value::as_str) != Some(expected_session_id) {
            continue;
        }
        if entry
            .get("cwd")
            .and_then(serde_json::Value::as_str)
            .and_then(|cwd| normalize_absolute(Path::new(cwd)))
            .is_some_and(|cwd| cwd == requested_directory)
        {
            saw_matching_directory = true;
        }
        if entry.get("type").and_then(serde_json::Value::as_str) == Some("ai-title") {
            title = entry
                .get("aiTitle")
                .and_then(serde_json::Value::as_str)
                .and_then(sanitize_title);
        }
    }
    saw_matching_directory.then_some(title).flatten()
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::fs::{PermissionsExt, symlink};
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::*;
    use crate::host_session_source::claude::session_directory_for_test;

    struct TestDirectory(PathBuf);

    impl TestDirectory {
        fn new() -> Self {
            let nonce = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let root = std::env::temp_dir().join(format!(
                "boomux-claude-titles-{}-{nonce}",
                std::process::id()
            ));
            fs::create_dir_all(&root).unwrap();
            Self(root)
        }

        fn environment(&self) -> Environment {
            Environment::for_test(Some(self.0.clone().into_os_string()), None)
        }

        fn project_directory(&self) -> PathBuf {
            let directory =
                session_directory_for_test(Path::new("/repo"), &self.environment()).unwrap();
            fs::create_dir_all(&directory).unwrap();
            directory
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.0).unwrap();
        }
    }

    #[test]
    fn parses_latest_exact_ai_title_for_matching_root_and_directory() {
        let output = br#"not json
{"type":"user","sessionId":"session-one","cwd":"/repo/./src/.."}
{"type":"ai-title","sessionId":"other","aiTitle":"Wrong identity"}
{"type":"ai-title","sessionId":"session-one","aiTitle":" Initial title "}
{"type":"ai-title","sessionId":"session-one","aiTitle":" Final   title "}
"#;

        assert_eq!(
            parse_transcript(output, Path::new("/repo"), "session-one").as_deref(),
            Some("Final title")
        );
        assert!(parse_transcript(output, Path::new("/other"), "session-one").is_none());
        assert!(parse_transcript(output, Path::new("/repo"), "other").is_none());
    }

    #[test]
    fn inspects_only_safe_direct_root_transcripts() {
        let test = TestDirectory::new();
        let directory = test.project_directory();
        fs::write(
            directory.join("session-one.jsonl"),
            "{\"type\":\"user\",\"sessionId\":\"session-one\",\"cwd\":\"/repo\"}\n{\"type\":\"ai-title\",\"sessionId\":\"session-one\",\"aiTitle\":\"Claude title\"}\n",
        )
        .unwrap();
        let target = directory.join("linked-target");
        fs::write(
            &target,
            "{\"type\":\"user\",\"sessionId\":\"linked\",\"cwd\":\"/repo\"}\n{\"type\":\"ai-title\",\"sessionId\":\"linked\",\"aiTitle\":\"Linked\"}\n",
        )
        .unwrap();
        symlink(&target, directory.join("linked.jsonl")).unwrap();
        fs::create_dir(directory.join("directory.jsonl")).unwrap();
        fs::set_permissions(&target, fs::Permissions::from_mode(0o600)).unwrap();

        let inspection = inspect(Path::new("/repo"), &test.environment()).unwrap();

        assert_eq!(inspection.catalog.len(), 1);
        assert_eq!(inspection.catalog[0].integration, "claude");
        assert_eq!(inspection.catalog[0].root_id, "session-one");
        assert_eq!(inspection.catalog[0].title, "Claude title");
        assert_eq!(inspection.catalog[0].directory, Path::new("/repo"));
        assert_eq!(inspection.catalog[0].created_at_ms, 0);
        assert!(!inspection.titles.contains_key("linked"));
    }
}

use std::collections::{HashMap, HashSet};
use std::ffi::OsString;
use std::fs::{self, OpenOptions};
use std::io::Read;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::thread;
use std::time::{Duration, Instant, SystemTime};

const SUCCESS_TTL: Duration = Duration::from_secs(30);
const FAILURE_TTL: Duration = Duration::from_secs(5);
const COMMAND_TIMEOUT: Duration = Duration::from_secs(15);
const MAX_OPENCODE_STDOUT_BYTES: u64 = 1024 * 1024;
const MAX_PI_FILE_BYTES: u64 = 256 * 1024;
const MAX_PI_CATALOG_BYTES: u64 = 4 * 1024 * 1024;
const MAX_PI_TRANSCRIPT_SCAN_BYTES: u64 = 16 * 1024 * 1024;
const MAX_PI_TRANSCRIPT_SCAN_FILES: usize = 4096;
const MAX_SESSIONS: usize = 100;
const MAX_TITLE_CHARS: usize = 160;

type Titles = HashMap<String, String>;
type Inspector = dyn Fn(&str, &Path) -> Option<Titles> + Send + Sync;

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
struct CacheKey {
    integration: String,
    directory: PathBuf,
}

struct CachedTitles {
    value: Option<Titles>,
    inspected_at: Instant,
}

pub(crate) struct Cache {
    entries: HashMap<CacheKey, CachedTitles>,
    pending: HashSet<CacheKey>,
    requests: Sender<CacheKey>,
    results: Receiver<(CacheKey, Option<Titles>)>,
}

impl Default for Cache {
    fn default() -> Self {
        Self::with_inspector(Arc::new(inspect))
    }
}

impl Cache {
    fn with_inspector(inspector: Arc<Inspector>) -> Self {
        let (request_sender, request_receiver) = mpsc::channel::<CacheKey>();
        let (result_sender, result_receiver) = mpsc::channel();
        thread::spawn(move || {
            while let Ok(key) = request_receiver.recv() {
                let titles = inspector(&key.integration, &key.directory);
                if result_sender.send((key, titles)).is_err() {
                    break;
                }
            }
        });
        Self {
            entries: HashMap::new(),
            pending: HashSet::new(),
            requests: request_sender,
            results: result_receiver,
        }
    }

    pub(crate) fn title(
        &mut self,
        integration: &str,
        directory: &Path,
        external_session_id: &str,
    ) -> Option<String> {
        for (key, value) in self.results.try_iter() {
            self.pending.remove(&key);
            self.entries.insert(
                key,
                CachedTitles {
                    value,
                    inspected_at: Instant::now(),
                },
            );
        }

        if !is_supported(integration) {
            return None;
        }

        let key = CacheKey {
            integration: integration.to_owned(),
            directory: directory.to_owned(),
        };
        let fresh = self.entries.get(&key).is_some_and(|cached| {
            cached.inspected_at.elapsed()
                < if cached.value.is_some() {
                    SUCCESS_TTL
                } else {
                    FAILURE_TTL
                }
        });
        if !fresh && !self.pending.contains(&key) && self.requests.send(key.clone()).is_ok() {
            self.pending.insert(key.clone());
        }

        self.entries
            .get(&key)
            .and_then(|cached| cached.value.as_ref())
            .and_then(|titles| titles.get(external_session_id))
            .cloned()
    }
}

fn is_supported(integration: &str) -> bool {
    matches!(integration, "opencode" | "pi")
}

fn inspect(integration: &str, directory: &Path) -> Option<Titles> {
    match integration {
        "opencode" => inspect_opencode(directory),
        "pi" => inspect_pi(directory, &PiEnvironment::from_process()),
        _ => None,
    }
}

fn inspect_opencode(directory: &Path) -> Option<Titles> {
    let stdout = run_opencode(directory)?;
    parse_opencode_titles(&stdout)
}

fn run_opencode(directory: &Path) -> Option<Vec<u8>> {
    let mut child = Command::new("opencode")
        .args(["session", "list", "--format", "json", "-n", "100"])
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
            .take(MAX_OPENCODE_STDOUT_BYTES + 1)
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
            .is_some_and(|bytes| bytes.len() as u64 > MAX_OPENCODE_STDOUT_BYTES)
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
    #[serde(rename = "updated")]
    _updated: serde_json::Value,
    #[serde(rename = "created")]
    _created: serde_json::Value,
    #[serde(rename = "directory")]
    _directory: String,
}

fn parse_opencode_titles(output: &[u8]) -> Option<Titles> {
    if output.len() as u64 > MAX_OPENCODE_STDOUT_BYTES {
        return None;
    }
    let sessions: Vec<OpenCodeSession> = serde_json::from_slice(output).ok()?;
    let mut titles = HashMap::new();
    for session in sessions.into_iter().take(MAX_SESSIONS) {
        if session.id.is_empty() {
            continue;
        }
        if let Some(title) = sanitize_title(&session.title) {
            titles.insert(session.id, title);
        }
    }
    Some(titles)
}

#[derive(Clone, Debug, Default)]
pub(crate) struct PiEnvironment {
    coding_agent_dir: Option<OsString>,
    session_dir: Option<OsString>,
    home: Option<OsString>,
}

impl PiEnvironment {
    pub(crate) fn from_process() -> Self {
        Self {
            coding_agent_dir: std::env::var_os("PI_CODING_AGENT_DIR"),
            session_dir: std::env::var_os("PI_CODING_AGENT_SESSION_DIR"),
            home: std::env::var_os("HOME"),
        }
    }
}

fn inspect_pi(directory: &Path, environment: &PiEnvironment) -> Option<Titles> {
    let normalized_directory = normalize_absolute(directory)?;
    let catalog_directory = pi_session_directory(&normalized_directory, environment)?;
    let files = pi_catalog_files(&catalog_directory)?;
    let mut remaining_bytes = MAX_PI_CATALOG_BYTES;
    let mut titles = HashMap::new();

    for path in files {
        if remaining_bytes == 0 {
            break;
        }
        let limit = MAX_PI_FILE_BYTES.min(remaining_bytes);
        let Some(prefix) = read_regular_file_prefix(&path, limit) else {
            continue;
        };
        remaining_bytes = remaining_bytes.saturating_sub(prefix.scanned_bytes);
        if let Some((id, title)) = parse_pi_session(&prefix.bytes, &normalized_directory) {
            titles.entry(id).or_insert(title);
        }
    }
    Some(titles)
}

pub(crate) fn pi_session_directory(
    directory: &Path,
    environment: &PiEnvironment,
) -> Option<PathBuf> {
    if let Some(session_directory) = nonempty(environment.session_dir.as_ref()) {
        return expand_home(session_directory, environment.home.as_ref());
    }
    let root = match nonempty(environment.coding_agent_dir.as_ref()) {
        Some(root) => expand_home(root, environment.home.as_ref())?,
        None => PathBuf::from(nonempty(environment.home.as_ref())?).join(".pi/agent"),
    };
    root.is_absolute().then(|| {
        root.join("sessions")
            .join(pi_encoded_session_directory(directory))
    })
}

fn nonempty(value: Option<&OsString>) -> Option<&OsString> {
    value.filter(|value| !value.is_empty())
}

fn expand_home(value: &OsString, home: Option<&OsString>) -> Option<PathBuf> {
    let path = PathBuf::from(value);
    let expanded = if path == Path::new("~") {
        PathBuf::from(nonempty(home)?)
    } else if let Ok(suffix) = path.strip_prefix("~/") {
        PathBuf::from(nonempty(home)?).join(suffix)
    } else {
        path
    };
    expanded.is_absolute().then_some(expanded)
}

fn pi_encoded_session_directory(directory: &Path) -> String {
    let directory = directory.to_string_lossy();
    let encoded: String = directory
        .trim_start_matches('/')
        .chars()
        .map(|character| match character {
            '/' | '\\' | ':' => '-',
            other => other,
        })
        .collect();
    format!("--{encoded}--")
}

pub(crate) fn normalize_absolute(path: &Path) -> Option<PathBuf> {
    if !path.is_absolute() {
        return None;
    }
    let mut normalized = PathBuf::from("/");
    for component in path.components() {
        match component {
            Component::RootDir | Component::CurDir => {}
            Component::Normal(component) => normalized.push(component),
            Component::ParentDir => {
                normalized.pop();
            }
            Component::Prefix(_) => return None,
        }
    }
    Some(normalized)
}

fn pi_catalog_files(directory: &Path) -> Option<Vec<PathBuf>> {
    let mut files = Vec::new();
    for entry in fs::read_dir(directory).ok()?.flatten() {
        let path = entry.path();
        if path
            .extension()
            .is_none_or(|extension| extension != "jsonl")
        {
            continue;
        }
        let Ok(metadata) = fs::symlink_metadata(&path) else {
            continue;
        };
        if !metadata.file_type().is_file() {
            continue;
        }
        files.push((path, metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH)));
    }
    files.sort_by(|(left_path, left_time), (right_path, right_time)| {
        right_time
            .cmp(left_time)
            .then_with(|| left_path.cmp(right_path))
    });
    files.truncate(MAX_SESSIONS);
    Some(files.into_iter().map(|(path, _)| path).collect())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PiSessionFileError {
    Unavailable,
    ScanLimit,
}

pub(crate) fn pi_session_file(
    directory: &Path,
    external_session_id: &str,
    environment: &PiEnvironment,
) -> Result<PathBuf, PiSessionFileError> {
    let normalized_directory =
        normalize_absolute(directory).ok_or(PiSessionFileError::Unavailable)?;
    let catalog_directory = pi_session_directory(&normalized_directory, environment)
        .ok_or(PiSessionFileError::Unavailable)?;
    let mut remaining_bytes = MAX_PI_TRANSCRIPT_SCAN_BYTES;
    let mut remaining_files = MAX_PI_TRANSCRIPT_SCAN_FILES;
    for filename_match in [true, false] {
        let entries =
            fs::read_dir(&catalog_directory).map_err(|_| PiSessionFileError::Unavailable)?;
        for entry in entries.flatten() {
            let path = entry.path();
            if pi_session_filename_matches(&path, external_session_id) != filename_match
                || path
                    .extension()
                    .is_none_or(|extension| extension != "jsonl")
                || !fs::symlink_metadata(&path).is_ok_and(|metadata| metadata.file_type().is_file())
            {
                continue;
            }
            if remaining_bytes == 0 || remaining_files == 0 {
                return Err(PiSessionFileError::ScanLimit);
            }
            remaining_files -= 1;
            let Some(prefix) =
                read_regular_file_prefix(&path, remaining_bytes.min(MAX_PI_FILE_BYTES))
            else {
                continue;
            };
            remaining_bytes = remaining_bytes.saturating_sub(prefix.scanned_bytes);
            if pi_header_matches(&prefix.bytes, external_session_id, &normalized_directory) {
                return Ok(path);
            }
        }
    }
    Err(PiSessionFileError::Unavailable)
}

fn pi_header_matches(output: &[u8], external_session_id: &str, directory: &Path) -> bool {
    let Some(header) = output
        .split(|byte| *byte == b'\n')
        .filter_map(|line| serde_json::from_slice::<serde_json::Value>(line).ok())
        .next()
    else {
        return false;
    };
    header.get("type").and_then(serde_json::Value::as_str) == Some("session")
        && header.get("id").and_then(serde_json::Value::as_str) == Some(external_session_id)
        && header
            .get("cwd")
            .and_then(serde_json::Value::as_str)
            .and_then(|cwd| normalize_absolute(Path::new(cwd)))
            .is_some_and(|cwd| cwd == directory)
}

fn pi_session_filename_matches(path: &Path, external_session_id: &str) -> bool {
    if external_session_id.is_empty()
        || path
            .extension()
            .is_none_or(|extension| extension != "jsonl")
    {
        return false;
    }
    path.file_stem()
        .and_then(|stem| stem.to_str())
        .is_some_and(|stem| {
            stem == external_session_id
                || stem
                    .strip_suffix(external_session_id)
                    .is_some_and(|prefix| prefix.ends_with('_'))
        })
}

struct FilePrefix {
    bytes: Vec<u8>,
    scanned_bytes: u64,
}

// Prefix-only inspection keeps I/O bounded; session_info names beyond this prefix are unavailable.
fn read_regular_file_prefix(path: &Path, limit: u64) -> Option<FilePrefix> {
    let mut file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)
        .ok()?;
    let metadata = file.metadata().ok()?;
    if !metadata.file_type().is_file() {
        return None;
    }
    let mut bytes = Vec::new();
    file.by_ref().take(limit).read_to_end(&mut bytes).ok()?;
    let scanned_bytes = bytes.len() as u64;
    if metadata.len() > scanned_bytes {
        let complete_length = bytes
            .iter()
            .rposition(|byte| *byte == b'\n')
            .map_or(0, |index| index + 1);
        bytes.truncate(complete_length);
    }
    Some(FilePrefix {
        bytes,
        scanned_bytes,
    })
}

fn parse_pi_session(output: &[u8], requested_directory: &Path) -> Option<(String, String)> {
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
                    .and_then(pi_message_text)
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

fn pi_message_text(content: &serde_json::Value) -> Option<String> {
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

fn sanitize_title(title: &str) -> Option<String> {
    let sanitized: String = title
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .filter(|character| !character.is_control())
        .take(MAX_TITLE_CHARS)
        .collect();
    (!sanitized.is_empty()).then_some(sanitized)
}

#[cfg(test)]
mod tests {
    use std::fs::{File, FileTimes};
    use std::os::unix::fs::symlink;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::*;

    struct TestDirectory(PathBuf);

    impl TestDirectory {
        fn new() -> Self {
            let nonce = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("clock after epoch")
                .as_nanos();
            let path = std::env::temp_dir().join(format!("boomux-pi-sessions-{nonce}"));
            fs::create_dir(&path).expect("create test directory");
            Self(path)
        }

        fn environment(&self) -> PiEnvironment {
            PiEnvironment {
                session_dir: Some(self.0.clone().into_os_string()),
                home: Some("/home/test".into()),
                ..PiEnvironment::default()
            }
        }

        fn write(&self, name: &str, contents: &str) -> PathBuf {
            let path = self.0.join(name);
            fs::write(&path, contents).expect("write session");
            path
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn pi_header(id: &str, cwd: &str) -> String {
        format!(r#"{{"type":"session","id":"{id}","cwd":"{cwd}"}}"#)
    }

    #[test]
    fn parses_opencode_catalog_by_canonical_session_id() {
        let titles = parse_opencode_titles(
            br#"[
                {"id":"ses_one","title":"Review cache","updated":2,"created":1,"directory":"/repo"},
                {"id":"ses_two","title":"Implement panel","updated":4,"created":3,"directory":"/repo"}
            ]"#,
        )
        .expect("valid catalog");

        assert_eq!(
            titles.get("ses_one").map(String::as_str),
            Some("Review cache")
        );
        assert_eq!(
            titles.get("ses_two").map(String::as_str),
            Some("Implement panel")
        );
    }

    #[test]
    fn sanitizes_and_bounds_titles() {
        assert_eq!(
            sanitize_title("  Review\n\t cache\0  ").as_deref(),
            Some("Review cache")
        );
        let long = "x".repeat(MAX_TITLE_CHARS + 20);
        assert_eq!(
            sanitize_title(&long).expect("title").chars().count(),
            MAX_TITLE_CHARS
        );
        assert_eq!(sanitize_title(" \n\t "), None);
    }

    #[test]
    fn malformed_and_oversized_opencode_catalogs_fail_open() {
        assert!(parse_opencode_titles(b"not json").is_none());
        assert!(
            parse_opencode_titles(&vec![b'x'; MAX_OPENCODE_STDOUT_BYTES as usize + 1]).is_none()
        );
    }

    #[test]
    fn derives_default_pi_directory_with_documented_encoding_and_home_expansion() {
        let environment = PiEnvironment {
            coding_agent_dir: Some("~/.local/pi".into()),
            home: Some("/home/test".into()),
            ..PiEnvironment::default()
        };

        assert_eq!(
            pi_session_directory(Path::new("/home/test/work:tree\\repo"), &environment),
            Some(PathBuf::from(
                "/home/test/.local/pi/sessions/--home-test-work-tree-repo--"
            ))
        );
        assert_eq!(
            pi_session_directory(
                Path::new("/home/test/repo"),
                &PiEnvironment {
                    coding_agent_dir: Some(OsString::new()),
                    home: Some("/home/test".into()),
                    ..PiEnvironment::default()
                }
            ),
            Some(PathBuf::from(
                "/home/test/.pi/agent/sessions/--home-test-repo--"
            ))
        );
    }

    #[test]
    fn custom_pi_session_directory_expands_home_without_recursing() {
        let environment = PiEnvironment {
            session_dir: Some("~/sessions/project".into()),
            home: Some("/home/test".into()),
            ..PiEnvironment::default()
        };

        assert_eq!(
            pi_session_directory(Path::new("/repo"), &environment),
            Some(PathBuf::from("/home/test/sessions/project"))
        );
    }

    #[test]
    fn pi_headers_must_match_the_normalized_requested_directory() {
        let test = TestDirectory::new();
        test.write(
            "sessions.jsonl",
            &format!(
                "{}\n{}\n",
                pi_header("matching", "/repo/./src/.."),
                r#"{"type":"session_info","name":"Matching title"}"#
            ),
        );
        test.write(
            "other.jsonl",
            &format!(
                "{}\n{}\n",
                pi_header("other", "/other"),
                r#"{"type":"session_info","name":"Wrong project"}"#
            ),
        );

        let titles = inspect_pi(Path::new("/repo"), &test.environment()).expect("catalog");

        assert_eq!(
            titles.get("matching").map(String::as_str),
            Some("Matching title")
        );
        assert!(!titles.contains_key("other"));
    }

    #[test]
    fn pi_prefers_the_latest_explicit_session_name() {
        let output = format!(
            "{}\n{}\n{}\n{}\n",
            pi_header("pi-one", "/repo"),
            r#"{"type":"session_info","name":"Initial name"}"#,
            r#"{"type":"message","message":{"role":"user","content":"Fallback prompt"}}"#,
            r#"{"type":"session_info","name":"  Final\n name  "}"#
        );

        assert_eq!(
            parse_pi_session(output.as_bytes(), Path::new("/repo")),
            Some(("pi-one".into(), "Final name".into()))
        );
    }

    #[test]
    fn pi_uses_the_first_user_message_as_a_fallback() {
        let output = format!(
            "not json\n{}\n{}\n{}\n",
            pi_header("pi-one", "/repo"),
            r#"{"type":"message","message":{"role":"assistant","content":"Ignore me"}}"#,
            r#"{"type":"message","message":{"role":"user","content":"  Fix\n the cache  "}}"#
        );

        assert_eq!(
            parse_pi_session(output.as_bytes(), Path::new("/repo")),
            Some(("pi-one".into(), "Fix the cache".into()))
        );
    }

    #[test]
    fn pi_extracts_text_from_user_content_blocks() {
        let output = format!(
            "{}\n{}\n",
            pi_header("pi-blocks", "/repo"),
            r#"{"type":"message","message":{"role":"user","content":[{"type":"image","data":"ignored"},{"type":"text","text":"Review"},{"type":"text","text":"this panel"}]}}"#
        );

        assert_eq!(
            parse_pi_session(output.as_bytes(), Path::new("/repo")),
            Some(("pi-blocks".into(), "Review this panel".into()))
        );
    }

    #[test]
    fn pi_skips_image_only_and_empty_user_messages_before_text() {
        let output = format!(
            "{}\n{}\n{}\n{}\n",
            pi_header("pi-later-text", "/repo"),
            r#"{"type":"message","message":{"role":"user","content":[{"type":"image","data":"ignored"}]}}"#,
            r#"{"type":"message","message":{"role":"user","content":"   "}}"#,
            r#"{"type":"message","message":{"role":"user","content":"Use this summary"}}"#
        );

        assert_eq!(
            parse_pi_session(output.as_bytes(), Path::new("/repo")),
            Some(("pi-later-text".into(), "Use this summary".into()))
        );
    }

    #[test]
    fn pi_empty_latest_explicit_name_clears_an_older_name() {
        let output = format!(
            "{}\n{}\n{}\n{}\n",
            pi_header("pi-cleared", "/repo"),
            r#"{"type":"session_info","name":"Old name"}"#,
            r#"{"type":"message","message":{"role":"user","content":"Prompt fallback"}}"#,
            r#"{"type":"session_info","name":"  \n  "}"#
        );

        assert_eq!(
            parse_pi_session(output.as_bytes(), Path::new("/repo")),
            Some(("pi-cleared".into(), "Prompt fallback".into()))
        );
    }

    #[test]
    fn oversized_pi_file_uses_complete_prefix_and_may_miss_a_late_name() {
        let test = TestDirectory::new();
        let header = pi_header("pi-oversized", "/repo");
        let message =
            r#"{"type":"message","message":{"role":"user","content":"Bounded prefix fallback"}}"#;
        let filler = "x".repeat(MAX_PI_FILE_BYTES as usize);
        test.write(
            "oversized.jsonl",
            &format!(
                "{header}\n{message}\n{filler}\n{{\"type\":\"session_info\",\"name\":\"Late unavailable name\"}}\n"
            ),
        );

        let titles = inspect_pi(Path::new("/repo"), &test.environment()).expect("catalog");

        assert_eq!(
            titles.get("pi-oversized").map(String::as_str),
            Some("Bounded prefix fallback")
        );
    }

    #[test]
    fn pi_skips_malformed_oversized_and_symlinked_files() {
        let test = TestDirectory::new();
        test.write("malformed.jsonl", "{}\nnot a session\n");
        fs::create_dir(test.0.join("special.jsonl")).expect("create special entry");
        let oversized = test.0.join("oversized.jsonl");
        fs::write(&oversized, vec![b'x'; MAX_PI_FILE_BYTES as usize + 1])
            .expect("write oversized session");
        let target = test.write(
            "target.txt",
            &format!(
                "{}\n{}\n",
                pi_header("linked", "/repo"),
                r#"{"type":"session_info","name":"Linked title"}"#
            ),
        );
        symlink(&target, test.0.join("linked.jsonl")).expect("create symlink");
        test.write(
            "valid.jsonl",
            &format!(
                "{}\n{}\n",
                pi_header("valid", "/repo"),
                r#"{"type":"session_info","name":"Valid title"}"#
            ),
        );

        let titles = inspect_pi(Path::new("/repo"), &test.environment()).expect("catalog");

        assert_eq!(titles.len(), 1);
        assert_eq!(titles.get("valid").map(String::as_str), Some("Valid title"));
    }

    #[test]
    fn pi_catalog_uses_only_the_100_newest_files() {
        let test = TestDirectory::new();
        for index in 0..=MAX_SESSIONS {
            let id = format!("pi-{index:03}");
            let header = pi_header(&id, "/repo");
            let path = test.write(
                &format!("session-{index:03}.jsonl"),
                &format!("{header}\n{{\"type\":\"session_info\",\"name\":\"Title {index}\"}}\n"),
            );
            File::options()
                .write(true)
                .open(path)
                .expect("open session timestamp")
                .set_times(
                    FileTimes::new().set_modified(UNIX_EPOCH + Duration::from_secs(index as u64)),
                )
                .expect("set session timestamp");
        }

        let titles = inspect_pi(Path::new("/repo"), &test.environment()).expect("catalog");

        assert_eq!(titles.len(), MAX_SESSIONS);
        assert!(titles.contains_key("pi-100"));
        assert!(!titles.contains_key("pi-000"));
    }

    #[test]
    fn exact_pi_file_lookup_is_not_limited_to_the_newest_catalog_entries() {
        let test = TestDirectory::new();
        let exact = test.write(
            "2026-01-01_external.jsonl",
            &format!("{}\n", pi_header("external", "/repo")),
        );
        for index in 0..MAX_SESSIONS + 1 {
            test.write(
                &format!("2026-02-{index:03}_other-{index}.jsonl"),
                &format!("{}\n", pi_header(&format!("other-{index}"), "/repo")),
            );
        }

        assert_eq!(
            pi_session_file(Path::new("/repo"), "external", &test.environment()),
            Ok(exact)
        );
    }

    #[test]
    fn exact_pi_file_lookup_accepts_noncanonical_filenames_by_header_identity() {
        let test = TestDirectory::new();
        let exact = test.write(
            "custom.jsonl",
            &format!("{}\n", pi_header("external", "/repo")),
        );

        assert_eq!(
            pi_session_file(Path::new("/repo"), "external", &test.environment()),
            Ok(exact)
        );
    }

    #[test]
    fn cache_is_async_deduplicated_and_routes_opencode_and_pi() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let worker_calls = Arc::clone(&calls);
        let (finished_sender, finished_receiver) = mpsc::channel();
        let mut cache = Cache::with_inspector(Arc::new(move |integration, directory| {
            worker_calls
                .lock()
                .expect("calls lock")
                .push((integration.to_owned(), directory.to_owned()));
            let _ = finished_sender.send(integration.to_owned());
            Some(HashMap::from([(
                format!("{integration}-id"),
                format!("{integration} title"),
            )]))
        }));

        assert_eq!(
            cache.title("opencode", Path::new("/repo"), "opencode-id"),
            None
        );
        assert_eq!(
            cache.title("opencode", Path::new("/repo"), "opencode-id"),
            None
        );
        assert_eq!(cache.title("pi", Path::new("/repo"), "pi-id"), None);
        finished_receiver
            .recv_timeout(Duration::from_secs(2))
            .expect("first inspection");
        finished_receiver
            .recv_timeout(Duration::from_secs(2))
            .expect("second inspection");

        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            let opencode = cache.title("opencode", Path::new("/repo"), "opencode-id");
            let pi = cache.title("pi", Path::new("/repo"), "pi-id");
            if opencode.as_deref() == Some("opencode title") && pi.as_deref() == Some("pi title") {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "cache results were not published"
            );
            thread::yield_now();
        }
        assert_eq!(calls.lock().expect("calls lock").len(), 2);
    }

    #[test]
    fn failures_are_cached_and_unsupported_integrations_do_not_request_catalogs() {
        let calls = Arc::new(AtomicUsize::new(0));
        let worker_calls = Arc::clone(&calls);
        let (finished_sender, finished_receiver) = mpsc::channel();
        let mut cache = Cache::with_inspector(Arc::new(move |_, _| {
            worker_calls.fetch_add(1, Ordering::SeqCst);
            let _ = finished_sender.send(());
            None
        }));

        assert_eq!(cache.title("other", Path::new("/repo"), "other-one"), None);
        assert_eq!(cache.title("pi", Path::new("/repo"), "pi-one"), None);
        finished_receiver
            .recv_timeout(Duration::from_secs(2))
            .expect("inspection completed");
        let deadline = Instant::now() + Duration::from_secs(2);
        while !cache.pending.is_empty() {
            let _ = cache.title("pi", Path::new("/repo"), "pi-one");
            assert!(Instant::now() < deadline, "failure was not published");
            thread::yield_now();
        }
        assert_eq!(cache.title("pi", Path::new("/repo"), "pi-one"), None);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }
}

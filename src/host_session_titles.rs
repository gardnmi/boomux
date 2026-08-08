use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;
use std::time::{Duration, Instant};

mod opencode;
mod pi;

#[cfg(test)]
use crate::host_session_source::pi::{
    Environment as PiEnvironment, MAX_FILE_BYTES as MAX_PI_FILE_BYTES,
    session_directory_for_test as pi_session_directory, session_file as pi_session_file,
};
#[cfg(test)]
use opencode::{
    MAX_STDOUT_BYTES as MAX_OPENCODE_STDOUT_BYTES, parse_titles as parse_opencode_titles,
};
#[cfg(test)]
use pi::{inspect as inspect_pi, parse_session as parse_pi_session};

const SUCCESS_TTL: Duration = Duration::from_secs(30);
const FAILURE_TTL: Duration = Duration::from_secs(5);
const MAX_SESSIONS: usize = 100;
const MAX_TITLE_CHARS: usize = 160;

type Titles = HashMap<String, String>;
type Inspector = dyn Fn(&str, &Path) -> Option<Titles> + Send + Sync;

trait TitleAdapter: Sync {
    fn integration(&self) -> &'static str;

    fn inspect(&self, directory: &Path) -> Option<Titles>;
}

static ADAPTERS: &[&dyn TitleAdapter] = &[opencode::ADAPTER, pi::ADAPTER];

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
    ADAPTERS
        .iter()
        .any(|adapter| adapter.integration() == integration)
}

fn inspect(integration: &str, directory: &Path) -> Option<Titles> {
    ADAPTERS
        .iter()
        .find(|adapter| adapter.integration() == integration)?
        .inspect(directory)
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
    use std::ffi::OsString;
    use std::fs::{self, File, FileTimes};
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
            PiEnvironment::for_test(
                None,
                Some(self.0.clone().into_os_string()),
                Some("/home/test".into()),
            )
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
        let environment =
            PiEnvironment::for_test(Some("~/.local/pi".into()), None, Some("/home/test".into()));

        assert_eq!(
            pi_session_directory(Path::new("/home/test/work:tree\\repo"), &environment),
            Some(PathBuf::from(
                "/home/test/.local/pi/sessions/--home-test-work-tree-repo--"
            ))
        );
        assert_eq!(
            pi_session_directory(
                Path::new("/home/test/repo"),
                &PiEnvironment::for_test(Some(OsString::new()), None, Some("/home/test".into()))
            ),
            Some(PathBuf::from(
                "/home/test/.pi/agent/sessions/--home-test-repo--"
            ))
        );
    }

    #[test]
    fn custom_pi_session_directory_expands_home_without_recursing() {
        let environment = PiEnvironment::for_test(
            None,
            Some("~/sessions/project".into()),
            Some("/home/test".into()),
        );

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

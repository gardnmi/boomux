use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;
use std::time::{Duration, Instant};

const REFRESH_INTERVAL: Duration = Duration::from_secs(2);

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Metadata {
    pub(crate) repository: String,
    pub(crate) branch: String,
    pub(crate) state: String,
    pub(crate) worktree: String,
}

impl Default for Metadata {
    fn default() -> Self {
        Self {
            repository: "-".into(),
            branch: "-".into(),
            state: "-".into(),
            worktree: "-".into(),
        }
    }
}

struct CachedMetadata {
    value: Metadata,
    inspected_at: Instant,
}

pub(crate) struct Cache {
    entries: HashMap<PathBuf, CachedMetadata>,
    pending: HashSet<PathBuf>,
    requests: Sender<PathBuf>,
    results: Receiver<(PathBuf, Metadata)>,
}

impl Default for Cache {
    fn default() -> Self {
        Self::with_inspector(Arc::new(|directory| inspect(directory).unwrap_or_default()))
    }
}

impl Cache {
    fn with_inspector(inspector: Arc<dyn Fn(&Path) -> Metadata + Send + Sync>) -> Self {
        let (request_sender, request_receiver) = mpsc::channel::<PathBuf>();
        let (result_sender, result_receiver) = mpsc::channel();
        thread::spawn(move || {
            while let Ok(directory) = request_receiver.recv() {
                let metadata = inspector(&directory);
                if result_sender.send((directory, metadata)).is_err() {
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

    pub(crate) fn inspect(&mut self, directory: &Path) -> Metadata {
        for (directory, value) in self.results.try_iter() {
            self.pending.remove(&directory);
            self.entries.insert(
                directory,
                CachedMetadata {
                    value,
                    inspected_at: Instant::now(),
                },
            );
        }

        if let Some(cached) = self.entries.get(directory)
            && cached.inspected_at.elapsed() < REFRESH_INTERVAL
        {
            return cached.value.clone();
        }

        if !self.pending.contains(directory) {
            let directory = directory.to_owned();
            if self.requests.send(directory.clone()).is_ok() {
                self.pending.insert(directory);
            }
        }
        self.entries
            .get(directory)
            .map(|cached| cached.value.clone())
            .unwrap_or_default()
    }
}

fn inspect(directory: &Path) -> Option<Metadata> {
    let output = Command::new("git")
        .arg("-C")
        .arg(directory)
        .args([
            "rev-parse",
            "--path-format=absolute",
            "--show-toplevel",
            "--git-dir",
            "--git-common-dir",
        ])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }

    let output = String::from_utf8_lossy(&output.stdout);
    let mut lines = output.lines();
    let (Some(root), Some(git_directory), Some(common_directory), None) =
        (lines.next(), lines.next(), lines.next(), lines.next())
    else {
        return None;
    };
    let root = Path::new(root);
    let git_directory = Path::new(git_directory);
    let common_directory = Path::new(common_directory);
    let worktree = worktree_label(git_directory, common_directory);
    let branch = inspect_branch(directory);
    let state = inspect_state(directory).unwrap_or_else(|| "unknown".into());

    Some(Metadata {
        repository: repository_name(root, git_directory, common_directory),
        branch,
        state,
        worktree,
    })
}

fn inspect_branch(directory: &Path) -> String {
    let symbolic = Command::new("git")
        .arg("-C")
        .arg(directory)
        .args(["symbolic-ref", "--short", "-q", "HEAD"])
        .output();
    if let Ok(output) = symbolic
        && output.status.success()
    {
        return String::from_utf8_lossy(&output.stdout).trim().to_owned();
    }
    "detached".into()
}

fn inspect_state(directory: &Path) -> Option<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(directory)
        .args(["status", "--porcelain=v1", "--untracked-files=normal"])
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| summarize_status(&String::from_utf8_lossy(&output.stdout)))
}

fn summarize_status(status: &str) -> String {
    let mut changed = 0;
    let mut conflicts = 0;
    for line in status.lines().filter(|line| line.len() >= 2) {
        changed += 1;
        if matches!(&line[..2], "DD" | "AU" | "UD" | "UA" | "DU" | "AA" | "UU") {
            conflicts += 1;
        }
    }

    if conflicts > 0 {
        format!(
            "{conflicts} conflict{}",
            if conflicts == 1 { "" } else { "s" }
        )
    } else if changed > 0 {
        format!("{changed} changed")
    } else {
        "clean".into()
    }
}

fn worktree_label(git_directory: &Path, common_directory: &Path) -> String {
    if git_directory == common_directory {
        "primary".into()
    } else {
        git_directory
            .file_name()
            .map(|name| format!("linked:{}", name.to_string_lossy()))
            .unwrap_or_else(|| "linked".into())
    }
}

fn repository_name(root: &Path, git_directory: &Path, common_directory: &Path) -> String {
    let repository = if git_directory == common_directory {
        root
    } else if common_directory
        .file_name()
        .is_some_and(|name| name == ".git")
    {
        common_directory.parent().unwrap_or(common_directory)
    } else {
        common_directory
    };
    repository
        .file_name()
        .map(|name| {
            let name = name.to_string_lossy();
            name.strip_suffix(".git")
                .unwrap_or(name.as_ref())
                .to_owned()
        })
        .unwrap_or_else(|| repository.display().to_string())
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::*;

    struct TestRepository(PathBuf);

    impl TestRepository {
        fn unborn() -> Self {
            let nonce = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("clock after epoch")
                .as_nanos();
            let path = std::env::temp_dir().join(format!("boomux-git-{nonce}"));
            let status = Command::new("git")
                .args(["init", "-q", "-b", "unborn"])
                .arg(&path)
                .status()
                .expect("git init");
            assert!(status.success());
            Self(path)
        }
    }

    impl Drop for TestRepository {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn summarizes_clean_changed_and_conflicted_states() {
        assert_eq!(summarize_status(""), "clean");
        assert_eq!(
            summarize_status(" M src/main.rs\n?? notes.md\n"),
            "2 changed"
        );
        assert_eq!(
            summarize_status("UU src/main.rs\nAA Cargo.toml\n"),
            "2 conflicts"
        );
    }

    #[test]
    fn identifies_primary_and_linked_worktrees() {
        assert_eq!(
            worktree_label(Path::new("/repo/.git"), Path::new("/repo/.git")),
            "primary"
        );
        assert_eq!(
            worktree_label(
                Path::new("/repo/.git/worktrees/feature"),
                Path::new("/repo/.git")
            ),
            "linked:feature"
        );
    }

    #[test]
    fn derives_linked_worktree_repository_from_the_common_directory() {
        assert_eq!(
            repository_name(
                Path::new("/worktrees/feature"),
                Path::new("/repo/boomux/.git/worktrees/feature"),
                Path::new("/repo/boomux/.git")
            ),
            "boomux"
        );
        assert_eq!(
            repository_name(
                Path::new("/worktrees/feature"),
                Path::new("/repos/boomux.git/worktrees/feature"),
                Path::new("/repos/boomux.git")
            ),
            "boomux"
        );
    }

    #[test]
    fn cache_returns_stale_values_without_repeating_fresh_inspections() {
        let calls = Arc::new(AtomicUsize::new(0));
        let worker_calls = Arc::clone(&calls);
        let (finished_sender, finished_receiver) = mpsc::channel();
        let mut cache = Cache::with_inspector(Arc::new(move |_| {
            worker_calls.fetch_add(1, Ordering::SeqCst);
            let _ = finished_sender.send(());
            Metadata {
                repository: "boomux".into(),
                ..Metadata::default()
            }
        }));

        assert_eq!(cache.inspect(Path::new("/tmp")).repository, "-");
        finished_receiver
            .recv_timeout(Duration::from_secs(2))
            .expect("inspection completed");

        let deadline = Instant::now() + Duration::from_secs(2);
        while cache.inspect(Path::new("/tmp")).repository != "boomux" {
            assert!(
                Instant::now() < deadline,
                "inspection result was not published"
            );
            thread::yield_now();
        }

        assert_eq!(cache.inspect(Path::new("/tmp")).repository, "boomux");
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn inspects_repositories_without_an_initial_commit() {
        let repository = TestRepository::unborn();

        let metadata = inspect(&repository.0).expect("Git metadata");

        assert_eq!(metadata.branch, "unborn");
        assert_eq!(metadata.state, "clean");
        assert_eq!(metadata.worktree, "primary");
    }
}

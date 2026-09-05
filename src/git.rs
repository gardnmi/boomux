use std::collections::{HashMap, HashSet};
use std::io::{self, Read};
use std::os::fd::AsRawFd;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::thread;
use std::time::{Duration, Instant};

const REFRESH_INTERVAL: Duration = Duration::from_secs(2);
const MAX_CACHE_ENTRIES: usize = 256;
const MAX_PENDING_INSPECTIONS: usize = 64;
const MAX_GIT_OUTPUT_BYTES: usize = 1024 * 1024;
const GIT_COMMAND_TIMEOUT: Duration = Duration::from_secs(1);

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
    accessed_at: Instant,
}

pub(crate) struct Cache {
    entries: HashMap<PathBuf, CachedMetadata>,
    pending: HashSet<PathBuf>,
    requests: SyncSender<PathBuf>,
    stopped: Arc<AtomicBool>,
    results: Receiver<(PathBuf, Metadata)>,
}

impl Default for Cache {
    fn default() -> Self {
        Self::with_inspector(Arc::new(|directory| inspect(directory).unwrap_or_default()))
    }
}

impl Cache {
    fn with_inspector(inspector: Arc<dyn Fn(&Path) -> Metadata + Send + Sync>) -> Self {
        let (request_sender, request_receiver) =
            mpsc::sync_channel::<PathBuf>(MAX_PENDING_INSPECTIONS);
        let (result_sender, result_receiver) = mpsc::sync_channel(MAX_PENDING_INSPECTIONS);
        let stopped = Arc::new(AtomicBool::new(false));
        let worker_stopped = Arc::clone(&stopped);
        thread::spawn(move || {
            while let Ok(directory) = request_receiver.recv() {
                if worker_stopped.load(Ordering::Acquire) {
                    break;
                }
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
            stopped,
            results: result_receiver,
        }
    }

    pub(crate) fn inspect(&mut self, directory: &Path) -> Metadata {
        for (directory, value) in self.results.try_iter() {
            self.pending.remove(&directory);
            if !self.entries.contains_key(&directory)
                && self.entries.len() >= MAX_CACHE_ENTRIES
                && let Some(oldest) = self
                    .entries
                    .iter()
                    .min_by_key(|(_, entry)| entry.accessed_at)
                    .map(|(path, _)| path.clone())
            {
                self.entries.remove(&oldest);
            }
            self.entries.insert(
                directory,
                CachedMetadata {
                    value,
                    inspected_at: Instant::now(),
                    accessed_at: Instant::now(),
                },
            );
        }

        if let Some(cached) = self.entries.get_mut(directory) {
            cached.accessed_at = Instant::now();
            if cached.inspected_at.elapsed() < REFRESH_INTERVAL {
                return cached.value.clone();
            }
        }

        if self.pending.len() < MAX_PENDING_INSPECTIONS && !self.pending.contains(directory) {
            let directory = directory.to_owned();
            if self.requests.try_send(directory.clone()).is_ok() {
                self.pending.insert(directory);
            }
        }
        self.entries
            .get(directory)
            .map(|cached| cached.value.clone())
            .unwrap_or_default()
    }
}

impl Drop for Cache {
    fn drop(&mut self) {
        self.stopped.store(true, Ordering::Release);
    }
}

// Drain a nonblocking pipe in this worker, with one absolute deadline covering
// both process exit and EOF. A descendant keeping stdout open cannot wedge it.
fn command_output(
    command: &mut Command,
    timeout: Duration,
    limit: usize,
) -> io::Result<Option<Vec<u8>>> {
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn()?;
    let group = child.id() as libc::pid_t;
    let result = (|| {
        let mut stdout = child
            .stdout
            .take()
            .ok_or_else(|| io::Error::other("Git stdout is unavailable"))?;
        let descriptor = stdout.as_raw_fd();
        // The pipe remains owned by stdout until the operation finishes.
        let flags = unsafe { libc::fcntl(descriptor, libc::F_GETFL) };
        if flags == -1
            || unsafe { libc::fcntl(descriptor, libc::F_SETFL, flags | libc::O_NONBLOCK) } == -1
        {
            return Err(io::Error::last_os_error());
        }
        let deadline = Instant::now() + timeout;
        let mut bytes = Vec::new();
        let mut buffer = [0u8; 8192];
        let mut eof = false;
        loop {
            if Instant::now() >= deadline {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "Git metadata inspection timed out",
                ));
            }
            if !eof {
                match stdout.read(&mut buffer) {
                    Ok(0) => eof = true,
                    Ok(count) => {
                        if bytes.len().saturating_add(count) > limit {
                            return Err(io::Error::new(
                                io::ErrorKind::InvalidData,
                                "Git output exceeds the size limit",
                            ));
                        }
                        bytes.extend_from_slice(&buffer[..count]);
                        continue;
                    }
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
                    Err(error) => return Err(error),
                }
            }
            if eof {
                // Observe exit without reaping, so its process-group ID cannot
                // be reused before we clean up any remaining Git helpers.
                let mut status: libc::siginfo_t = unsafe { std::mem::zeroed() };
                if unsafe {
                    libc::waitid(
                        libc::P_PID,
                        child.id(),
                        &mut status,
                        libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
                    )
                } == -1
                {
                    let error = io::Error::last_os_error();
                    if error.kind() == io::ErrorKind::Interrupted {
                        continue;
                    }
                    return Err(error);
                }
                if unsafe { status.si_pid() } != 0 {
                    return Ok(bytes);
                }
            }
            let mut descriptor = libc::pollfd {
                fd: if eof { -1 } else { descriptor },
                events: libc::POLLIN,
                revents: 0,
            };
            let timeout = deadline
                .saturating_duration_since(Instant::now())
                .as_millis()
                .min(20) as i32;
            // Only active Git work polls; an idle cache sleeps on its queue.
            if unsafe { libc::poll(&mut descriptor, 1, timeout) } == -1 {
                let error = io::Error::last_os_error();
                if error.kind() != io::ErrorKind::Interrupted {
                    return Err(error);
                }
            }
        }
    })();
    // Git helpers share this dedicated process group. Reap the direct child on
    // every path and close the pipe before accepting the next inspection.
    unsafe {
        libc::kill(-group, libc::SIGKILL);
    }
    let status = child.wait();
    let bytes = result?;
    Ok(status?.success().then_some(bytes))
}

fn git_output(directory: &Path, arguments: &[&str]) -> Option<Vec<u8>> {
    command_output(
        Command::new("git").arg("-C").arg(directory).args(arguments),
        GIT_COMMAND_TIMEOUT,
        MAX_GIT_OUTPUT_BYTES,
    )
    .ok()
    .flatten()
}

fn inspect(directory: &Path) -> Option<Metadata> {
    let output = git_output(
        directory,
        &[
            "rev-parse",
            "--path-format=absolute",
            "--show-toplevel",
            "--git-dir",
            "--git-common-dir",
        ],
    )?;

    let output = String::from_utf8_lossy(&output);
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
    git_output(directory, &["symbolic-ref", "--short", "-q", "HEAD"])
        .map(|output| String::from_utf8_lossy(&output).trim().to_owned())
        .unwrap_or_else(|| "detached".into())
}

fn inspect_state(directory: &Path) -> Option<String> {
    git_output(
        directory,
        &["status", "--porcelain=v1", "--untracked-files=normal"],
    )
    .map(|output| summarize_status(&String::from_utf8_lossy(&output)))
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
    fn command_output_bounds_runtime_output_and_inherited_pipes() {
        for script in ["sleep 10", "sleep 10 & exit 0", "exec 1>&-; sleep 10"] {
            let started = Instant::now();
            let error = command_output(
                Command::new("/bin/sh").args(["-c", script]),
                Duration::from_millis(50),
                100,
            )
            .unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::TimedOut);
            assert!(started.elapsed() < Duration::from_secs(2));
        }
        let error = command_output(
            Command::new("/bin/sh").args(["-c", "while :; do printf '0123456789'; done"]),
            Duration::from_secs(2),
            100,
        )
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert_eq!(
            command_output(
                Command::new("/bin/sh").args(["-c", "printf ready"]),
                Duration::from_secs(1),
                100
            )
            .unwrap(),
            Some(b"ready".to_vec())
        );
    }

    #[test]
    fn cache_bounds_pending_work_and_stops_queued_inspections_on_drop() {
        let (started, start) = mpsc::sync_channel(1);
        let (release, released) = mpsc::sync_channel(1);
        let released = std::sync::Mutex::new(released);
        let calls = Arc::new(AtomicUsize::new(0));
        let worker_calls = Arc::clone(&calls);
        let mut cache = Cache::with_inspector(Arc::new(move |_| {
            worker_calls.fetch_add(1, Ordering::SeqCst);
            started.send(()).unwrap();
            released.lock().unwrap().recv().unwrap();
            Metadata::default()
        }));
        cache.inspect(Path::new("/first"));
        start.recv_timeout(Duration::from_secs(2)).unwrap();
        for index in 0..MAX_PENDING_INSPECTIONS * 4 {
            cache.inspect(&PathBuf::from(format!("/directory-{index}")));
        }
        assert_eq!(cache.pending.len(), MAX_PENDING_INSPECTIONS);
        drop(cache);
        release.send(()).unwrap();
        assert!(start.recv_timeout(Duration::from_secs(2)).is_err());
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn cache_evicts_old_directories() {
        let mut cache = Cache::with_inspector(Arc::new(|_| Metadata::default()));
        for index in 0..MAX_CACHE_ENTRIES + 10 {
            let path = PathBuf::from(format!("/directory-{index}"));
            cache.inspect(&path);
            let deadline = Instant::now() + Duration::from_secs(2);
            while cache.pending.contains(&path) {
                cache.inspect(&path);
                assert!(Instant::now() < deadline);
                thread::yield_now();
            }
        }
        assert_eq!(cache.entries.len(), MAX_CACHE_ENTRIES);
        assert!(!cache.entries.contains_key(Path::new("/directory-0")));
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

//! Explicit owner-local worktree review/removal. No branch or Boomux resource deletion.
use crate::{git_work, protocol::Snapshot};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashSet,
    ffi::OsStr,
    fs,
    os::unix::{ffi::OsStrExt, fs::MetadataExt},
    path::{Path, PathBuf},
    process::Command,
    time::{Duration, Instant},
};

const MAX_WORKTREES: usize = 128;
const LOCAL_CHANGES: &str = "Local changes or untracked files";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Target {
    pub root: PathBuf,
    pub common_dir: PathBuf,
    pub git_dir: PathBuf,
    pub branch: String,
    pub head: String,
    pub device: u64,
    pub inode: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Review {
    pub target: Target,
    pub reasons: Vec<String>,
    pub blockers: Vec<String>,
    pub status: git_work::Status,
    pub activity: Vec<String>,
    pub bytes: u64,
    pub size_complete: bool,
    pub ignored_entries: usize,
    pub pr: String,
}

impl Review {
    pub fn has_local_changes(&self) -> bool {
        self.status.staged + self.status.unstaged + self.status.untracked + self.status.conflicts
            > 0
    }

    /// Only the local-changes guard can be explicitly overridden. All other guards stay mandatory.
    pub fn can_discard_changes(&self) -> bool {
        self.has_local_changes() && self.blockers.iter().all(|b| b == LOCAL_CHANGES)
    }
}

#[derive(Debug)]
struct Registration {
    root: PathBuf,
    primary: bool,
    locked: bool,
}

fn command(path: &Path) -> Command {
    let mut command = Command::new("git");
    command.args(["--no-optional-locks", "-c", "core.fsmonitor=false", "-C"]);
    command.arg(path);
    // Do not inherit index/config/repository redirection from the daemon's parent.
    for (key, _) in std::env::vars_os() {
        if key.as_bytes().starts_with(b"GIT_") {
            command.env_remove(key);
        }
    }
    command
}
fn output(path: &Path, args: &[&OsStr], timeout: Duration) -> Result<Vec<u8>, String> {
    crate::git_metadata::command_output(command(path).args(args), timeout, 1024 * 1024)
        .map_err(|e| e.to_string())?
        .ok_or_else(|| {
            "Git refused the operation; refresh and inspect the worktree with Git".into()
        })
}
fn git(path: &Path, args: &[&str]) -> Result<Vec<u8>, String> {
    output(
        path,
        &args.iter().map(OsStr::new).collect::<Vec<_>>(),
        Duration::from_secs(1),
    )
}
fn text(path: &Path, args: &[&str]) -> Result<String, String> {
    String::from_utf8(git(path, args)?)
        .map(|s| s.trim().to_owned())
        .map_err(|_| "Git returned non-UTF-8 metadata".into())
}
fn git_path(path: &Path, args: &[&str]) -> Result<PathBuf, String> {
    let bytes = git(path, args)?;
    PathBuf::from(OsStr::from_bytes(
        bytes.strip_suffix(b"\n").unwrap_or(&bytes),
    ))
    .canonicalize()
    .map_err(|e| e.to_string())
}
fn registrations(path: &Path) -> Result<Vec<Registration>, String> {
    let bytes = git(path, &["worktree", "list", "--porcelain", "-z"])?;
    let mut rows = Vec::<Registration>::new();
    for field in bytes.split(|b| *b == 0) {
        if let Some(path) = field.strip_prefix(b"worktree ") {
            if rows.len() == MAX_WORKTREES {
                return Err(
                    "Repository has more than 128 worktrees; use Git to narrow cleanup".into(),
                );
            }
            rows.push(Registration {
                root: PathBuf::from(OsStr::from_bytes(path)),
                primary: rows.is_empty(),
                locked: false,
            });
        } else if (field == b"locked" || field.starts_with(b"locked "))
            && let Some(row) = rows.last_mut()
        {
            row.locked = true;
        }
    }
    if rows.is_empty() {
        return Err("No registered worktrees found".into());
    }
    Ok(rows)
}

pub(crate) fn list(path: &Path) -> Result<Vec<PathBuf>, String> {
    if !path.is_absolute() {
        return Err("Choose an absolute repository directory".into());
    }
    Ok(registrations(path)?.into_iter().map(|r| r.root).collect())
}

fn inspect(path: &Path) -> Result<Review, String> {
    if !path.is_absolute() {
        return Err("Worktree path must be absolute".into());
    }
    let root = path.canonicalize().map_err(|e| e.to_string())?;
    let actual_root = git_path(&root, &["rev-parse", "--show-toplevel"])?;
    if root != actual_root
        || fs::symlink_metadata(path)
            .map_err(|e| e.to_string())?
            .file_type()
            .is_symlink()
    {
        return Err("Choose the registered worktree root, not a subdirectory or symlink".into());
    }
    let common_dir = git_path(
        &root,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    )?;
    let git_dir = git_path(&root, &["rev-parse", "--absolute-git-dir"])?;
    let registrations = registrations(&root)?;
    let registration = registrations
        .iter()
        .find(|r| r.root == root)
        .ok_or("Worktree registration changed; scan again")?;
    let (branch, head, status) = git_work::parse_status(&git(
        &root,
        &[
            "status",
            "--porcelain=v2",
            "--branch",
            "-z",
            "--untracked-files=normal",
        ],
    )?)?;
    let metadata = fs::metadata(&root).map_err(|e| e.to_string())?;
    let mut blockers = Vec::new();
    if registration.primary || common_dir == git_dir {
        blockers.push("Primary worktree".into());
    }
    if registration.locked {
        blockers.push("Locked worktree".into());
    }
    if branch == "(detached)" || head == "(initial)" {
        blockers.push("A committed local branch is required to preserve work".into());
    }
    if status.staged + status.unstaged + status.untracked + status.conflicts != 0 {
        blockers.push(LOCAL_CHANGES.into());
    }
    if root.join(".gitmodules").exists() {
        blockers.push("Worktree contains submodule configuration; manage with Git".into());
    }
    let index = git(&root, &["ls-files", "-v", "-z"])?;
    if index.split(|b| *b == 0).any(|entry| {
        entry
            .first()
            .is_some_and(|flag| flag.is_ascii_lowercase() || *flag == b'S')
    }) {
        blockers.push(
            "Index contains assume-unchanged or skip-worktree entries; inspect with Git".into(),
        );
    }
    // Never delete nested worktrees (including ignored repositories) as build output.
    let (bytes, size_complete, nested) = disk_usage(&root);
    if nested {
        blockers.push("Nested repository or cross-filesystem directory".into());
    }
    if !size_complete {
        blockers.push("File scan incomplete; cannot rule out nested repositories".into());
    }
    let ignored = git(
        &root,
        &[
            "ls-files",
            "--others",
            "--ignored",
            "--exclude-standard",
            "--directory",
            "-z",
        ],
    )?;
    let ignored_entries = ignored.split(|b| *b == 0).filter(|s| !s.is_empty()).count();
    Ok(Review {
        target: Target {
            root,
            common_dir,
            git_dir,
            branch,
            head,
            device: metadata.dev(),
            inode: metadata.ino(),
        },
        reasons: Vec::new(),
        blockers,
        status,
        activity: Vec::new(),
        bytes,
        size_complete,
        ignored_entries,
        pr: String::new(),
    })
}

pub(crate) fn review(path: &Path) -> Result<Review, String> {
    let mut review = inspect(path)?;
    let root = &review.target.root;
    if review.status.upstream.is_some() && !review.status.divergence_known {
        // Only an actual missing ref is a gone upstream, not any Git failure.
        let refs = git(
            root,
            &[
                "for-each-ref",
                "--format=%(refname:short)",
                "refs/remotes/",
                "refs/heads/",
            ],
        )?;
        if let Some(upstream) = &review.status.upstream
            && !String::from_utf8_lossy(&refs)
                .lines()
                .any(|r| r == upstream)
        {
            review
                .reasons
                .push("Upstream ref gone (local refs; no fetch)".into());
        }
    }
    let base = text(
        root,
        &["symbolic-ref", "--quiet", "refs/remotes/origin/HEAD"],
    )
    .ok()
    .or_else(|| {
        ["refs/heads/main", "refs/heads/master"]
            .into_iter()
            .find(|base| git(root, &["rev-parse", "--verify", base]).is_ok())
            .map(str::to_owned)
    });
    if let Some(base) = base
        && git(
            root,
            &["merge-base", "--is-ancestor", &review.target.head, &base],
        )
        .is_ok()
    {
        review.reasons.push(format!("Merged into {base}"));
    }
    let pr = git_work::inspect_pr(root, &review.target.branch);
    if pr.error.is_none() {
        review.pr = pr.summary;
        if pr.head.as_ref() == Some(&review.target.head) {
            if let Some(state @ ("MERGED" | "CLOSED")) = pr.state.as_deref() {
                review.reasons.push(format!("PR {}", state.to_lowercase()));
            }
        } else if pr.head.is_some() {
            review.pr.push_str(" · different commit");
        }
    } else {
        review.pr = "PR lookup unavailable".into();
    }
    Ok(review)
}

/// Byte estimate from allocated blocks; never follows symlinks or crosses devices.
/// Bounded DFS retains at most 64 directory iterators and 200,000 inode keys.
fn disk_usage(root: &Path) -> (u64, bool, bool) {
    let started = Instant::now();
    let Ok(meta) = fs::metadata(root) else {
        return (0, false, false);
    };
    let device = meta.dev();
    let mut bytes = meta.blocks().saturating_mul(512);
    let Ok(entries) = fs::read_dir(root) else {
        return (bytes, false, false);
    };
    let mut stack = vec![entries];
    let mut seen = HashSet::new();
    let mut nested = false;
    let mut complete = true;
    while let Some(entries) = stack.last_mut() {
        if started.elapsed() > Duration::from_secs(3) || seen.len() >= 200_000 {
            return (bytes, false, nested);
        }
        let Some(entry) = entries.next() else {
            stack.pop();
            continue;
        };
        let Ok(entry) = entry else {
            complete = false;
            continue;
        };
        let Ok(meta) = fs::symlink_metadata(entry.path()) else {
            complete = false;
            continue;
        };
        if meta.dev() != device {
            nested = true;
            continue;
        }
        if !seen.insert((meta.dev(), meta.ino())) {
            continue;
        }
        if meta.is_dir() && stack.len() == 1 && entry.file_name() == ".git" {
            continue;
        }
        bytes = bytes.saturating_add(meta.blocks().saturating_mul(512));
        if meta.is_dir() {
            if entry.file_name() == ".git"
                || entry.path().join(".git").exists()
                || (entry.path().join("HEAD").is_file() && entry.path().join("objects").is_dir())
            {
                nested = true;
                continue;
            }
            if stack.len() >= 64 {
                complete = false;
                continue;
            }
            match fs::read_dir(entry.path()) {
                Ok(entries) => stack.push(entries),
                Err(_) => complete = false,
            }
        }
    }
    (bytes, complete, nested)
}

pub(crate) fn add_activity(
    review: &mut Review,
    snapshot: &Snapshot,
    live: &[(String, PathBuf)],
    unknown: bool,
) {
    let mut unknown = unknown;
    let root = &review.target.root;
    let within = |path: &Path| {
        path.starts_with(root) || path.canonicalize().is_ok_and(|p| p.starts_with(root))
    };
    for (index, (workspace, shell)) in snapshot
        .workspaces
        .iter()
        .flat_map(|workspace| workspace.shells.iter().map(move |shell| (workspace, shell)))
        .enumerate()
    {
        if workspace.agents.len() > 512 {
            unknown = true;
        }
        if index >= 512 {
            unknown = true;
            break;
        }
        let running = shell.status == crate::protocol::ShellStatus::Running;
        let context = workspace.agents.iter().take(512).any(|a| {
            a.shell_id == shell.id
                && shell.run.as_ref().is_some_and(|r| r.id == a.run_id)
                && !matches!(
                    a.observation.state,
                    crate::protocol::AgentState::Done | crate::protocol::AgentState::Inactive
                )
                && a.working_contexts.iter().any(|c| within(&c.worktree_root))
        });
        let associated = within(&shell.cwd)
            || live
                .iter()
                .any(|(id, path)| id == &shell.id && within(path))
            || context;
        if associated {
            review.activity.push(format!(
                "{} / {} ({})",
                workspace.name,
                shell.name,
                if running { "running" } else { "retained Shell" }
            ));
            if running || context {
                review
                    .blockers
                    .push("Running Shell or active Agent associated with this worktree".into());
            }
        }
    }
    if unknown {
        review
            .blockers
            .push("Could not verify every running Shell's directory".into());
    }
    review.blockers.sort();
    review.blockers.dedup();
}

/// Caller holds the daemon mutation gate through activity validation and removal.
/// Explicit discard overrides only local changes, never other guards or branch retention.
pub(crate) enum RemovalError {
    Refused(String),
    OutcomeUnknown(String),
}

pub(crate) fn remove(
    expected: &Target,
    discard_changes: bool,
    activity: impl FnOnce(&mut Review) -> Result<(), String>,
) -> Result<(), RemovalError> {
    let mut current = inspect(&expected.root).map_err(RemovalError::Refused)?;
    if &current.target != expected {
        return Err(RemovalError::Refused(
            "Worktree identity, branch, or HEAD changed; scan again".into(),
        ));
    }
    activity(&mut current).map_err(RemovalError::Refused)?;
    if discard_changes && current.can_discard_changes() {
        current.blockers.retain(|b| b != LOCAL_CHANGES);
    }
    if !current.blockers.is_empty() {
        return Err(RemovalError::Refused(current.blockers.join("; ")));
    }
    // The size walk can take time; recheck branch identity after it and activity inspection.
    let head =
        text(&expected.root, &["rev-parse", "--verify", "HEAD"]).map_err(RemovalError::Refused)?;
    let branch = text(&expected.root, &["symbolic-ref", "--short", "HEAD"])
        .map_err(RemovalError::Refused)?;
    if head != expected.head || branch != expected.branch {
        return Err(RemovalError::Refused(
            "Branch changed during validation; scan again".into(),
        ));
    }
    let mut args = vec![OsStr::new("worktree"), OsStr::new("remove")];
    if discard_changes {
        args.push(OsStr::new("--force"));
    }
    args.extend([OsStr::new("--"), expected.root.as_os_str()]);
    let result = output(&expected.common_dir, &args, Duration::from_secs(30));
    result.map(|_| ()).map_err(|e| RemovalError::OutcomeUnknown(format!("Removal was not confirmed: {e}. Scan again before retrying; partial removal is possible.")))
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Fixture {
        base: PathBuf,
        repo: PathBuf,
        linked: PathBuf,
    }
    impl Fixture {
        fn new() -> Self {
            let base =
                std::env::temp_dir().join(format!("boomux-cleanup-{}", uuid::Uuid::new_v4()));
            fs::create_dir_all(&base).unwrap();
            let repo = base.join("repo");
            git(&base, &["init", "-q", "-b", "main", repo.to_str().unwrap()]).unwrap();
            git(&repo, &["config", "user.name", "Test"]).unwrap();
            git(&repo, &["config", "user.email", "test@example.com"]).unwrap();
            fs::write(repo.join("tracked"), "original").unwrap();
            fs::write(repo.join(".gitignore"), "cache/\n.env\n").unwrap();
            git(&repo, &["add", "."]).unwrap();
            git(&repo, &["commit", "-qm", "initial"]).unwrap();
            let linked = base.join("linked space\nnewline");
            git(
                &repo,
                &[
                    "worktree",
                    "add",
                    "-qb",
                    "feature",
                    linked.to_str().unwrap(),
                ],
            )
            .unwrap();
            Self { base, repo, linked }
        }
        fn remove(&self, target: &Target) -> Result<(), RemovalError> {
            remove(target, false, |_| Ok(()))
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.base);
        }
    }
    #[test]
    fn cleanup_removes_ignored_output_but_retains_unpushed_branch_and_external_symlink_target() {
        let f = Fixture::new();
        git(&f.linked, &["commit", "--allow-empty", "-qm", "unpushed"]).unwrap();
        fs::create_dir(f.linked.join("cache")).unwrap();
        fs::write(f.linked.join("cache/build"), vec![b'x'; 8192]).unwrap();
        let external = f.base.join("external");
        fs::write(&external, "keep").unwrap();
        std::os::unix::fs::symlink(&external, f.linked.join(".env")).unwrap();
        let review = review(&f.linked).unwrap();
        assert!(review.blockers.is_empty(), "{:?}", review.blockers);
        assert_eq!(review.ignored_entries, 2);
        assert!(review.bytes >= 8192);
        assert!(review.size_complete);
        assert!(f.remove(&review.target).is_ok());
        assert!(!f.linked.exists());
        assert_eq!(
            text(&f.repo, &["rev-parse", "refs/heads/feature"]).unwrap(),
            review.target.head
        );
        assert!(external.exists());
        assert_eq!(list(&f.repo).unwrap(), vec![f.repo.clone()]);
    }
    #[test]
    fn cleanup_revalidates_dirty_locked_changed_and_primary_worktrees() {
        let f = Fixture::new();
        let target = inspect(&f.linked).unwrap().target;
        fs::write(f.linked.join("untracked"), "do not lose").unwrap();
        assert!(matches!(f.remove(&target), Err(RemovalError::Refused(_))));
        fs::remove_file(f.linked.join("untracked")).unwrap();
        git(&f.repo, &["worktree", "lock", f.linked.to_str().unwrap()]).unwrap();
        assert!(matches!(f.remove(&target), Err(RemovalError::Refused(_))));
        git(&f.repo, &["worktree", "unlock", f.linked.to_str().unwrap()]).unwrap();
        git(&f.linked, &["commit", "--allow-empty", "-qm", "changed"]).unwrap();
        assert!(matches!(f.remove(&target), Err(RemovalError::Refused(_))));
        let primary = inspect(&f.repo).unwrap();
        assert!(primary.blockers.iter().any(|s| s == "Primary worktree"));
        assert!(matches!(
            f.remove(&primary.target),
            Err(RemovalError::Refused(_))
        ));
        assert!(f.repo.exists() && f.linked.exists());
    }
    #[test]
    fn cleanup_blocks_hidden_changes_detached_and_nested_repositories() {
        let f = Fixture::new();
        git(
            &f.linked,
            &["update-index", "--assume-unchanged", "tracked"],
        )
        .unwrap();
        fs::write(f.linked.join("tracked"), "hidden changes").unwrap();
        assert!(
            inspect(&f.linked)
                .unwrap()
                .blockers
                .iter()
                .any(|s| s.contains("Index"))
        );
        git(
            &f.linked,
            &["update-index", "--no-assume-unchanged", "tracked"],
        )
        .unwrap();
        git(&f.linked, &["checkout", "--", "tracked"]).unwrap();
        git(&f.linked, &["checkout", "--detach", "-q"]).unwrap();
        assert!(
            inspect(&f.linked)
                .unwrap()
                .blockers
                .iter()
                .any(|s| s.contains("branch"))
        );
        git(&f.linked, &["checkout", "-q", "feature"]).unwrap();
        git(&f.linked, &["init", "-q", "cache/nested"]).unwrap();
        assert!(
            inspect(&f.linked)
                .unwrap()
                .blockers
                .iter()
                .any(|s| s.contains("Nested"))
        );
    }
    #[test]
    fn cleanup_discard_requires_explicit_choice_and_preserves_branch() {
        let f = Fixture::new();
        let target = inspect(&f.linked).unwrap().target;
        fs::write(f.linked.join("tracked"), "staged changes").unwrap();
        git(&f.linked, &["add", "tracked"]).unwrap();
        fs::write(f.linked.join("tracked"), "unstaged changes").unwrap();
        fs::write(f.linked.join("unfinished"), "untracked work").unwrap();
        let dirty = inspect(&f.linked).unwrap();
        assert!(dirty.has_local_changes() && dirty.can_discard_changes());
        assert!(matches!(f.remove(&target), Err(RemovalError::Refused(_))));
        assert!(remove(&target, true, |_| Ok(())).is_ok());
        assert!(!f.linked.exists());
        assert_eq!(
            text(&f.repo, &["rev-parse", "refs/heads/feature"]).unwrap(),
            target.head
        );
    }

    #[test]
    fn cleanup_discard_never_overrides_protected_worktrees() {
        for guard in ["primary", "locked", "nested", "active", "incomplete"] {
            let f = Fixture::new();
            let path = if guard == "primary" {
                &f.repo
            } else {
                &f.linked
            };
            let target = inspect(path).unwrap().target;
            fs::write(path.join("unfinished"), "must remain").unwrap();
            match guard {
                "locked" => {
                    git(&f.repo, &["worktree", "lock", path.to_str().unwrap()]).unwrap();
                }
                "nested" => {
                    git(path, &["init", "-q", "cache/nested"]).unwrap();
                }
                _ => {}
            }
            let result = remove(&target, true, |review| {
                if guard == "active" {
                    review.blockers.push("Running Shell".into());
                }
                if guard == "incomplete" {
                    review.blockers.push("File scan incomplete".into());
                }
                Ok(())
            });
            assert!(matches!(result, Err(RemovalError::Refused(_))), "{guard}");
            assert!(path.join("unfinished").exists());
        }
    }

    #[test]
    fn cleanup_reasons_distinguish_missing_upstream_and_local_merge() {
        let f = Fixture::new();
        let first = review(&f.linked).unwrap();
        assert!(
            first
                .reasons
                .iter()
                .any(|s| s.contains("Merged into refs/heads/main"))
        );
        git(
            &f.linked,
            &["config", "remote.origin.url", f.repo.to_str().unwrap()],
        )
        .unwrap();
        git(
            &f.linked,
            &[
                "config",
                "remote.origin.fetch",
                "+refs/heads/*:refs/remotes/origin/*",
            ],
        )
        .unwrap();
        git(&f.linked, &["config", "branch.feature.remote", "origin"]).unwrap();
        git(
            &f.linked,
            &["config", "branch.feature.merge", "refs/heads/feature"],
        )
        .unwrap();
        let gone = review(&f.linked).unwrap();
        assert!(!gone.status.divergence_known);
        assert!(gone.reasons.iter().any(|s| s.contains("Upstream ref gone")));
        assert!(gone.blockers.is_empty()); // Branch is kept; missing upstream is not proof of publication.
    }

    #[test]
    fn cleanup_activity_rechecked_after_scan_can_refuse_removal() {
        let f = Fixture::new();
        let target = inspect(&f.linked).unwrap().target;
        let result = remove(&target, false, |current| {
            current.blockers.push("new running Shell".into());
            Ok(())
        });
        assert!(matches!(result, Err(RemovalError::Refused(_))));
        assert!(f.linked.exists());
        let mut changed = target.clone();
        changed.inode += 1;
        assert!(matches!(f.remove(&changed), Err(RemovalError::Refused(_))));
        assert!(inspect(&f.linked.join("cache")).is_err());
    }
}

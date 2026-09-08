//! Disposable, owner-local Git observations. Never mutates repositories or Agent state.
use crate::protocol::{AgentState, Snapshot};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, HashMap},
    path::{Path, PathBuf},
    process::Command,
    sync::{Arc, Mutex},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

const MAX_ROOTS: usize = 128;
const MAX_LINKS: usize = 512;
const MAX_OUTPUT: usize = 1024 * 1024;
const MAX_CYCLE: Duration = Duration::from_secs(20);

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Overview {
    pub worktrees: Vec<Worktree>,
    pub warnings: Vec<String>,
    pub refreshing: bool,
    pub observed_at_ms: u64,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Worktree {
    pub root: PathBuf,
    pub common_dir: PathBuf,
    pub repository: String,
    pub branch: String,
    pub head: String,
    pub last_commit: Option<String>,
    pub status: Option<Status>,
    pub error: Option<String>,
    pub shells: Vec<ShellLink>,
    pub agents: Vec<AgentLink>,
    pub branches: Vec<String>,
    pub pr: PullRequest,
    pub observed_at_ms: u64,
}
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Status {
    pub staged: usize,
    pub unstaged: usize,
    pub untracked: usize,
    pub conflicts: usize,
    pub upstream: Option<String>,
    pub divergence_known: bool,
    pub ahead: usize,
    pub behind: usize,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShellLink {
    pub id: String,
    pub run_id: Option<String>,
    pub name: String,
    pub workspace_id: String,
    pub workspace: String,
    pub live_cwd: bool,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentLink {
    pub id: String,
    pub run_id: String,
    pub workspace_id: String,
    pub observed_at_ms: u64,
    pub shell_id: String,
    pub name: String,
    pub state: AgentState,
    pub observed_context: bool,
}
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PullRequest {
    pub error: Option<String>,
    pub checked_at_ms: u64,
    pub summary: String,
    pub url: Option<String>,
    pub head: Option<String>,
    pub observed_at_ms: u64,
}
#[derive(Default)]
struct State {
    overview: Overview,
    requested: Option<Instant>,
    refresh_interval: Duration,
    prs: HashMap<(PathBuf, String), PullRequest>,
}
#[derive(Default)]
pub(crate) struct Service(Arc<Mutex<State>>);

impl Service {
    pub(crate) fn cached_if_fresh(&self, refresh: bool) -> Option<Overview> {
        let state = self.0.lock().unwrap_or_else(|e| e.into_inner());
        (state.overview.refreshing
            || state.requested.is_some_and(|at| {
                at.elapsed()
                    < if refresh {
                        Duration::from_secs(1)
                    } else {
                        state.refresh_interval
                    }
            }))
        .then(|| state.overview.clone())
    }
    pub(crate) fn query(
        &self,
        snapshot: Snapshot,
        live: HashMap<String, PathBuf>,
        refresh: bool,
    ) -> Overview {
        let mut state = self.0.lock().unwrap_or_else(|e| e.into_inner());
        if !state.overview.refreshing
            && state.requested.is_none_or(|at| {
                at.elapsed()
                    >= if refresh {
                        Duration::from_secs(1)
                    } else {
                        state.refresh_interval
                    }
            })
        {
            state.overview.refreshing = true;
            state.requested = Some(Instant::now());
            let shared = Arc::clone(&self.0);
            let previous = state.overview.worktrees.clone();
            let mut prs = std::mem::take(&mut state.prs);
            if refresh {
                for pr in prs.values_mut() {
                    pr.checked_at_ms = 0;
                }
            }
            let spawn = std::thread::Builder::new()
                .name("git-overview".into())
                .spawn(move || {
                    let result = inspect(snapshot, live, &mut prs, previous);
                    let mut state = shared.lock().unwrap_or_else(|e| e.into_inner());
                    let changed = state.overview.worktrees.len() != result.worktrees.len()
                        || state.overview.worktrees.iter().zip(&result.worktrees).any(
                            |(old, new)| {
                                old.root != new.root
                                    || old.branch != new.branch
                                    || old.head != new.head
                                    || old.status != new.status
                                    || old.shells != new.shells
                                    || old.agents != new.agents
                                    || old.pr.summary != new.pr.summary
                                    || old.pr.error != new.pr.error
                                    || old.error != new.error
                            },
                        );
                    state.refresh_interval = Duration::from_secs(if changed { 5 } else { 15 });
                    state.requested = Some(Instant::now());
                    state.overview = result;
                    state.prs = prs;
                });
            if let Err(error) = spawn {
                state.overview.refreshing = false;
                state.overview.warnings = vec![format!("Could not inspect Git: {error}")];
            }
        }
        state.overview.clone()
    }
}
fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}
fn output(path: &Path, args: &[&str]) -> Result<Vec<u8>, String> {
    crate::git_metadata::command_output(
        Command::new("git")
            .arg("--no-optional-locks")
            .arg("-C")
            .arg(path)
            .args(args)
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env_remove("GIT_COMMON_DIR")
            .env_remove("GIT_INDEX_FILE")
            .env_remove("GIT_NAMESPACE"),
        Duration::from_secs(1),
        MAX_OUTPUT,
    )
    .map_err(|e| e.to_string())?
    .ok_or_else(|| "Git inspection unavailable".into())
}
fn string(path: &Path, args: &[&str]) -> Result<String, String> {
    output(path, args).map(|v| String::from_utf8_lossy(&v).trim().to_string())
}
fn identity(path: &Path) -> Result<(PathBuf, PathBuf), String> {
    use std::os::unix::ffi::OsStrExt;
    let path_output = |args: &[&str]| -> Result<PathBuf, String> {
        let bytes = output(path, args)?;
        let bytes = bytes.strip_suffix(b"\n").unwrap_or(&bytes);
        PathBuf::from(std::ffi::OsStr::from_bytes(bytes))
            .canonicalize()
            .map_err(|e| e.to_string())
    };
    Ok((
        path_output(&["rev-parse", "--show-toplevel"])?,
        path_output(&["rev-parse", "--path-format=absolute", "--git-common-dir"])?,
    ))
}
fn inspect(
    snapshot: Snapshot,
    live: HashMap<String, PathBuf>,
    prs: &mut HashMap<(PathBuf, String), PullRequest>,
    previous: Vec<Worktree>,
) -> Overview {
    let started = Instant::now();
    let mut result = Overview::default();
    let mut paths: BTreeMap<PathBuf, (Vec<ShellLink>, Vec<AgentLink>)> = BTreeMap::new();
    let mut count = 0;
    for workspace in snapshot.workspaces {
        for shell in &workspace.shells {
            if count >= MAX_LINKS {
                break;
            }
            count += 1;
            let path = live.get(&shell.id).unwrap_or(&shell.cwd).clone();
            let links = paths.entry(path).or_default();
            links.0.push(ShellLink {
                id: shell.id.clone(),
                run_id: shell.run.as_ref().map(|r| r.id.clone()),
                name: shell.name.clone(),
                workspace_id: workspace.id.clone(),
                workspace: workspace.name.clone(),
                live_cwd: live.contains_key(&shell.id),
            });
            for agent in workspace
                .agents
                .iter()
                .filter(|a| {
                    a.shell_id == shell.id
                        && shell.run.as_ref().is_some_and(|r| r.id == a.run_id)
                        && !matches!(a.observation.state, AgentState::Done | AgentState::Inactive)
                })
                .take(16)
            {
                if count >= MAX_LINKS {
                    break;
                }
                count += 1;
                links.1.push(AgentLink {
                    id: agent.id.clone(),
                    run_id: agent.run_id.clone(),
                    workspace_id: workspace.id.clone(),
                    observed_at_ms: agent.observation.observed_at_ms,
                    shell_id: shell.id.clone(),
                    name: format!("{} · {}", agent.integration, agent.name),
                    state: agent.observation.state,
                    observed_context: false,
                });
            }
        }
        for agent in workspace.agents.iter().filter(|a| {
            !matches!(a.observation.state, AgentState::Done | AgentState::Inactive)
                && workspace.shells.iter().any(|shell| {
                    shell.id == a.shell_id
                        && shell.run.as_ref().is_some_and(|run| run.id == a.run_id)
                })
        }) {
            for context in &agent.working_contexts {
                if count >= MAX_LINKS {
                    break;
                }
                count += 1;
                paths
                    .entry(context.worktree_root.clone())
                    .or_default()
                    .1
                    .push(AgentLink {
                        id: agent.id.clone(),
                        run_id: agent.run_id.clone(),
                        workspace_id: workspace.id.clone(),
                        observed_at_ms: agent.observation.observed_at_ms,
                        shell_id: agent.shell_id.clone(),
                        name: format!("{} · {}", agent.integration, agent.name),
                        state: agent.observation.state,
                        observed_context: true,
                    });
            }
        }
    }
    if count >= MAX_LINKS {
        result
            .warnings
            .push("Showing the first 512 Shell/Agent associations.".into());
    }
    let mut roots: BTreeMap<PathBuf, Worktree> = BTreeMap::new();
    let mut repos = BTreeMap::<PathBuf, PathBuf>::new();
    for (path, (shells, agents)) in paths {
        if started.elapsed() >= MAX_CYCLE || roots.len() >= MAX_ROOTS {
            result
                .warnings
                .push("Git discovery limit reached; some work is not shown.".into());
            break;
        }
        if let Ok((root, common)) = identity(&path) {
            repos.entry(common.clone()).or_insert(root.clone());
            let row = roots
                .entry(root.clone())
                .or_insert_with(|| empty(root, common));
            row.shells.extend(shells);
            for agent in agents {
                if !row
                    .agents
                    .iter()
                    .any(|a| a.id == agent.id && a.observed_context == agent.observed_context)
                {
                    row.agents.push(agent);
                }
            }
        }
    }
    let mut branch_count = 0;
    for (common, root) in repos {
        if started.elapsed() >= MAX_CYCLE {
            break;
        }
        if let Ok(bytes) = output(&root, &["worktree", "list", "--porcelain", "-z"]) {
            for field in bytes.split(|b| *b == 0) {
                if let Some(path) = field.strip_prefix(b"worktree ") {
                    use std::os::unix::ffi::OsStrExt;
                    let path = PathBuf::from(std::ffi::OsStr::from_bytes(path));
                    if roots.len() >= MAX_ROOTS {
                        if !result.warnings.iter().any(|s| s.contains("128 worktrees")) {
                            result
                                .warnings
                                .push("Showing at most 128 worktrees.".into());
                        }
                        break;
                    }
                    roots
                        .entry(path.clone())
                        .or_insert_with(|| empty(path, common.clone()));
                }
            }
        }
        if let Ok(branches) = string(
            &root,
            &[
                "for-each-ref",
                "--count=128",
                "--format=%(refname:short)",
                "refs/heads/",
            ],
        ) {
            if let Some(row) = roots.get_mut(&root) {
                row.branches = branches
                    .lines()
                    .take(MAX_LINKS - branch_count)
                    .map(|s| s.chars().take(256).collect())
                    .collect();
                branch_count += row.branches.len();
            }
        }
    }
    for row in roots.values_mut() {
        if let Some(old) = previous
            .iter()
            .find(|old| old.root == row.root && old.common_dir == row.common_dir)
        {
            row.branch = old.branch.clone();
            row.head = old.head.clone();
            row.last_commit = old.last_commit.clone();
            row.observed_at_ms = old.observed_at_ms;
            row.pr = old.pr.clone();
        }
    }
    let mut order: Vec<_> = roots.keys().cloned().collect();
    order.sort_by_key(|root| roots[root].observed_at_ms);
    for root in order {
        let row = roots.get_mut(&root).expect("discovered worktree");
        if started.elapsed() >= MAX_CYCLE {
            row.error = Some("Refresh budget reached; retry on next refresh".into());
            continue;
        }
        match output(
            &row.root,
            &[
                "status",
                "--porcelain=v2",
                "--branch",
                "-z",
                "--untracked-files=normal",
            ],
        )
        .and_then(|bytes| parse_status(&bytes))
        {
            Ok((branch, head, status)) => {
                row.branch = branch;
                row.head = head;
                row.status = Some(status);
            }
            Err(error) => row.error = Some(error),
        }
        if row.status.is_some() && row.head != "(initial)" {
            row.last_commit = string(
                &row.root,
                &["log", "-1", "--format=%h %s", "--no-show-signature"],
            )
            .ok()
            .map(|s| s.chars().take(256).collect());
        }
        if row.status.is_some() {
            let current_head = string(&row.root, &["rev-parse", "--verify", "HEAD"])
                .unwrap_or_else(|_| "(initial)".into());
            let current_branch = string(&row.root, &["symbolic-ref", "--short", "-q", "HEAD"])
                .unwrap_or_else(|_| "(detached)".into());
            if current_head != row.head || current_branch != row.branch {
                row.status = None;
                row.last_commit = None;
                row.error = Some("Branch changed during inspection; refresh pending".into());
            }
        }
        row.observed_at_ms = now();
        let key = (row.common_dir.clone(), row.branch.clone());
        let pr = prs.entry(key).or_default();
        if now().saturating_sub(pr.checked_at_ms) >= 60_000
            && started.elapsed() < MAX_CYCLE
            && row.status.is_some()
        {
            let next = inspect_pr(&row.root, &row.branch);
            if next.error.is_some() && pr.observed_at_ms != 0 {
                pr.error = next.error;
                pr.checked_at_ms = next.checked_at_ms;
            } else {
                *pr = next;
            }
        }
        row.pr = pr.clone();
    }
    prs.retain(|key, _| {
        roots
            .values()
            .any(|r| r.common_dir == key.0 && r.branch == key.1)
    });
    let checked: std::collections::HashSet<_> = roots
        .values()
        .map(|r| (r.common_dir.clone(), r.branch.clone()))
        .collect();
    for row in roots.values_mut() {
        row.branches
            .retain(|branch| !checked.contains(&(row.common_dir.clone(), branch.clone())));
    }
    result.worktrees = roots.into_values().collect();
    result
        .worktrees
        .sort_by(|a, b| (&a.common_dir, &a.root).cmp(&(&b.common_dir, &b.root)));
    result.observed_at_ms = now();
    result
}
fn empty(root: PathBuf, common_dir: PathBuf) -> Worktree {
    let label = if common_dir.file_name().is_some_and(|n| n == ".git") {
        common_dir.parent().unwrap_or(&common_dir)
    } else {
        &common_dir
    };
    let repository = label
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .trim_end_matches(".git")
        .chars()
        .take(256)
        .collect();
    Worktree {
        root,
        common_dir,
        repository,
        branch: "Unknown".into(),
        head: String::new(),
        last_commit: None,
        status: None,
        error: None,
        shells: vec![],
        agents: vec![],
        branches: vec![],
        pr: PullRequest::default(),
        observed_at_ms: 0,
    }
}
fn parse_status(bytes: &[u8]) -> Result<(String, String, Status), String> {
    let mut status = Status::default();
    let mut branch = None;
    let mut head = String::new();
    let mut records = bytes.split(|b| *b == 0);
    while let Some(record) = records.next() {
        if record.is_empty() {
            continue;
        }
        if let Some(value) = record.strip_prefix(b"# branch.head ") {
            branch = Some(String::from_utf8_lossy(value).into_owned());
        } else if let Some(value) = record.strip_prefix(b"# branch.oid ") {
            head = String::from_utf8_lossy(value).into_owned();
        } else if let Some(value) = record.strip_prefix(b"# branch.upstream ") {
            status.upstream = Some(String::from_utf8_lossy(value).into_owned());
        } else if let Some(value) = record.strip_prefix(b"# branch.ab ") {
            let text = String::from_utf8_lossy(value);
            let (ahead, behind) = text.split_once(' ').ok_or("Invalid ahead/behind")?;
            status.divergence_known = true;
            status.ahead = ahead
                .trim_start_matches('+')
                .parse()
                .map_err(|_| "Invalid ahead")?;
            status.behind = behind
                .trim_start_matches('-')
                .parse()
                .map_err(|_| "Invalid behind")?;
        } else {
            match record[0] {
                b'1' | b'2' => {
                    if record.len() < 5 {
                        return Err("Truncated Git record".into());
                    }
                    status.staged += usize::from(record[2] != b'.');
                    status.unstaged += usize::from(record[3] != b'.');
                    if record[0] == b'2' && records.next().is_none() {
                        return Err("Missing rename source".into());
                    }
                }
                b'?' => status.untracked += 1,
                b'u' => status.conflicts += 1,
                b'#' | b'!' => (),
                _ => return Err("Unknown Git status record".into()),
            }
        }
    }
    let branch = branch.ok_or("Missing Git branch")?;
    if branch.is_empty() || head.is_empty() {
        return Err("Incomplete Git branch metadata".into());
    }
    if branch.len() > 1024
        || head.len() > 128
        || status.upstream.as_ref().is_some_and(|s| s.len() > 1024)
    {
        return Err("Git metadata exceeds label limits".into());
    }
    Ok((branch, head, status))
}
#[derive(Clone, Debug, PartialEq, Eq)]
struct GithubRepo {
    owner: String,
    name: String,
}
fn github_repo(url: &str) -> Option<GithubRepo> {
    let path = url
        .strip_prefix("https://github.com/")
        .or_else(|| url.strip_prefix("git@github.com:"))
        .or_else(|| url.strip_prefix("ssh://git@github.com/"))?;
    let path = path.strip_suffix(".git").unwrap_or(path);
    let (owner, name) = path.split_once('/')?;
    let valid = |part: &str| {
        !part.is_empty()
            && part
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
    };
    (valid(owner) && valid(name)).then(|| GithubRepo {
        owner: owner.into(),
        name: name.into(),
    })
}
fn config(path: &Path, key: &str) -> Option<String> {
    string(path, &["config", "--get", key])
        .ok()
        .filter(|s| !s.is_empty())
}
fn inspect_pr(path: &Path, branch: &str) -> PullRequest {
    let mut pr = PullRequest {
        observed_at_ms: now(),
        checked_at_ms: now(),
        ..Default::default()
    };
    if matches!(branch, "(detached)" | "Unknown") {
        pr.summary = "No branch PR lookup".into();
        return pr;
    }
    let push_remote = config(path, &format!("branch.{branch}.pushRemote"))
        .or_else(|| config(path, "remote.pushDefault"))
        .or_else(|| config(path, &format!("branch.{branch}.remote")))
        .unwrap_or_else(|| "origin".into());
    let Some(head_repo) =
        config(path, &format!("remote.{push_remote}.url")).and_then(|url| github_repo(&url))
    else {
        pr.summary = "PR lookup unavailable: no supported GitHub push remote".into();
        return pr;
    };
    let configured_base = string(
        path,
        &["config", "--get-regexp", r"^remote\..*\.gh-resolved$"],
    )
    .ok()
    .and_then(|text| {
        text.lines().find_map(|line| {
            let (key, value) = line.split_once(' ')?;
            (value == "base")
                .then(|| key.strip_prefix("remote.")?.strip_suffix(".gh-resolved"))
                .flatten()
                .map(str::to_owned)
        })
    });
    let base_repo = configured_base
        .and_then(|name| config(path, &format!("remote.{name}.url")))
        .or_else(|| config(path, "remote.upstream.url"))
        .and_then(|url| github_repo(&url))
        .unwrap_or_else(|| head_repo.clone());
    let target = format!("github.com/{}/{}", base_repo.owner, base_repo.name);
    let result = crate::git_metadata::command_output(Command::new("gh").current_dir(path).args(["pr", "list", "--repo", &target, "--head", branch, "--state", "all", "--limit", "50", "--json", "number,state,isDraft,url,headRefOid,headRepository,headRepositoryOwner,reviewDecision,statusCheckRollup,updatedAt"]).env("GH_PROMPT_DISABLED", "1"), Duration::from_secs(3), MAX_OUTPUT);
    match result {
        Ok(Some(bytes)) => match serde_json::from_slice::<Vec<serde_json::Value>>(&bytes) {
            Ok(rows) => {
                pr = summarize_pr(rows, &head_repo);
                pr.observed_at_ms = now();
                pr.checked_at_ms = now();
            }
            Err(_) => {
                pr.error = Some("Invalid GitHub response".into());
                pr.observed_at_ms = 0;
            }
        },
        _ => {
            pr.error = Some("Check gh authentication or network".into());
            pr.observed_at_ms = 0;
        }
    }
    pr
}
fn summarize_pr(mut rows: Vec<serde_json::Value>, head_repo: &GithubRepo) -> PullRequest {
    let mut pr = PullRequest::default();
    let truncated = rows.len() == 50;
    rows.retain(|r| {
        r["headRepositoryOwner"]["login"]
            .as_str()
            .is_some_and(|owner| owner.eq_ignore_ascii_case(&head_repo.owner))
            && r["headRepository"]["name"]
                .as_str()
                .is_some_and(|name| name.eq_ignore_ascii_case(&head_repo.name))
    });
    rows.sort_by(|a, b| b["updatedAt"].as_str().cmp(&a["updatedAt"].as_str()));
    let open: Vec<_> = rows.iter().filter(|r| r["state"] == "OPEN").collect();
    if open.len() > 1 {
        pr.summary = "Multiple PRs match; inspect on GitHub".into();
    } else if let Some(row) = open.first().copied().or_else(|| rows.first()) {
        pr.summary = format!(
            "#{} {}",
            row["number"],
            if row["isDraft"] == true {
                "Draft"
            } else {
                row["state"].as_str().unwrap_or("Unknown")
            }
        );
        let checks = row["statusCheckRollup"]
            .as_array()
            .map(Vec::as_slice)
            .unwrap_or_default();
        let failing = checks.iter().any(|c| {
            matches!(
                c["conclusion"].as_str().or(c["state"].as_str()),
                Some(
                    "FAILURE"
                        | "ERROR"
                        | "TIMED_OUT"
                        | "CANCELLED"
                        | "ACTION_REQUIRED"
                        | "STARTUP_FAILURE"
                )
            )
        });
        let passed = checks.iter().all(|c| {
            matches!(
                c["conclusion"].as_str().or(c["state"].as_str()),
                Some("SUCCESS" | "NEUTRAL" | "SKIPPED")
            )
        });
        pr.summary.push_str(if checks.is_empty() {
            " · No checks"
        } else if failing {
            " · Checks failed"
        } else if passed {
            " · Checks passed"
        } else {
            " · Checks pending"
        });
        if let Some(review) = row["reviewDecision"].as_str().filter(|s| !s.is_empty()) {
            pr.summary
                .push_str(&format!(" · {}", review.to_lowercase().replace('_', " ")));
        }
        pr.url = row["url"]
            .as_str()
            .filter(|u| u.starts_with("https://github.com/"))
            .map(str::to_owned);
        pr.head = row["headRefOid"].as_str().map(str::to_owned);
    } else {
        pr.summary = if truncated {
            "PR lookup truncated; matching PR unknown"
        } else {
            "No matching PR"
        }
        .into();
    }
    pr
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn git_work_status_handles_renames_conflicts_and_unusual_paths() {
        let bytes = b"# branch.oid abc\0# branch.head feat/test\0# branch.upstream origin/feat/test\0# branch.ab +2 -1\0\
1 M. N... 100644 100644 100644 a b staged\0\
1 .M N... 100644 100644 100644 a b new\nline\0\
2 R. N... 100644 100644 100644 a b R100 new\0? old name\0\
? untracked\nname\0u UU N... 0 0 0 0 a b c conflicted\0";
        let (branch, head, status) = parse_status(bytes).unwrap();
        assert_eq!((branch.as_str(), head.as_str()), ("feat/test", "abc"));
        assert_eq!(
            (
                status.staged,
                status.unstaged,
                status.untracked,
                status.conflicts
            ),
            (2, 1, 1, 1)
        );
        assert_eq!((status.ahead, status.behind), (2, 1));
        assert!(parse_status(b"").is_err());
    }
    #[test]
    fn git_work_missing_upstream_ref_is_not_a_zero_divergence() {
        let (_, _, status) = parse_status(
            b"# branch.oid abc\0# branch.head feature\0# branch.upstream origin/feature\0",
        )
        .unwrap();
        assert_eq!(status.upstream.as_deref(), Some("origin/feature"));
        assert!(!status.divergence_known);
        let (_, _, status) = parse_status(b"# branch.oid abc\0# branch.head feature\0# branch.upstream origin/feature\0# branch.ab +0 -0\0").unwrap();
        assert!(status.divergence_known);
    }

    #[test]
    fn git_work_discovers_real_linked_worktrees_and_unborn_branches() {
        let path = std::env::temp_dir().join(format!("boomux-git-work-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&path).unwrap();
        let repository = path.join("repo");
        assert!(
            Command::new("git")
                .args(["init", "-q", "-b", "main"])
                .arg(&repository)
                .status()
                .unwrap()
                .success()
        );
        let (branch, head, status) = parse_status(
            &output(&repository, &["status", "--porcelain=v2", "--branch", "-z"]).unwrap(),
        )
        .unwrap();
        assert_eq!(branch, "main");
        assert_eq!(head, "(initial)");
        assert_eq!(status.upstream, None);
        string(
            &repository,
            &[
                "-c",
                "user.name=Test",
                "-c",
                "user.email=test@example.com",
                "commit",
                "--allow-empty",
                "-qm",
                "initial",
            ],
        )
        .unwrap();
        let linked = path.join("linked");
        output(
            &repository,
            &["worktree", "add", "-b", "feature", linked.to_str().unwrap()],
        )
        .unwrap();
        let (_, common) = identity(&repository).unwrap();
        assert_eq!(identity(&linked).unwrap().1, common);
        std::fs::write(linked.join("new\nfile"), "work").unwrap();
        let (_, _, status) = parse_status(
            &output(&linked, &["status", "--porcelain=v2", "--branch", "-z"]).unwrap(),
        )
        .unwrap();
        assert_eq!(status.untracked, 1);
        std::fs::remove_dir_all(path).unwrap();
    }
}

#[cfg(test)]
mod pr_tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn git_work_pr_matches_fork_and_distinguishes_unknown_checks() {
        let repo = GithubRepo {
            owner: "me".into(),
            name: "project".into(),
        };
        let unrelated = json!({"number": 1, "state": "OPEN", "headRepositoryOwner": {"login": "other"}, "headRepository": {"name": "project"}});
        let mine = json!({"number": 2, "state": "OPEN", "headRepositoryOwner": {"login": "me"}, "headRepository": {"name": "project"}, "headRefOid": "abc", "url": "https://github.com/upstream/project/pull/2", "statusCheckRollup": [{"status": "COMPLETED", "conclusion": "SUCCESS"}, {"status": "EXPECTED"}] });
        let pr = summarize_pr(vec![unrelated.clone(), mine.clone()], &repo);
        assert!(pr.summary.starts_with("#2 OPEN"));
        assert!(pr.summary.contains("pending"));
        assert_eq!(pr.head.as_deref(), Some("abc"));
        assert!(
            summarize_pr(vec![unrelated], &repo)
                .summary
                .contains("No matching")
        );
        assert!(summarize_pr(vec![mine.clone(), mine], &repo).url.is_none());
    }
    #[test]
    fn git_work_pr_retains_merged_state_and_supports_remote_url_forms() {
        let repo = github_repo("git@github.com:me/project.git").unwrap();
        assert_eq!(
            github_repo("https://github.com/me/project"),
            Some(repo.clone())
        );
        assert_eq!(
            github_repo("ssh://git@github.com/me/project.git"),
            Some(repo.clone())
        );
        assert!(github_repo("https://gitlab.com/me/project.git").is_none());
        assert!(github_repo("https://github.com/me/project/extra").is_none());
        let pr = summarize_pr(
            vec![
                json!({"number": 374, "state": "MERGED", "headRepositoryOwner": {"login":"me"}, "headRepository": {"name": "project"}, "headRefOid": "old"}),
            ],
            &repo,
        );
        assert!(pr.summary.contains("MERGED"));
        assert!(pr.summary.contains("No checks"));
        assert_eq!(pr.head.as_deref(), Some("old"));
    }
}

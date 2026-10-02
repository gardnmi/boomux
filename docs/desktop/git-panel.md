# Git panel

Select **Git** in the lower sidebar, or use **Ctrl + Space**, then **G**.
The shortcut reveals the sidebar when hidden. The panel discovers repositories
from managed Shells and active Agent working contexts.

## Reading The Panel

| Area | Contents |
| --- | --- |
| Repository heading | Repository grouped by owning machine |
| Worktree row | Branch, Agent count, working-tree state, and upstream comparison |
| PR status | Checks/reviews only when a matching PR is available; no placeholder for absent PRs or failed lookups |
| Expanded row | Full path, last commit, upstream, observation ages, linked Shells/Agents, copy-path, and open-PR actions |

Only one row expands at a time. A highlighted guide and background distinguish
it; aligned labels and a separate linked-activity group keep details readable.
Healthy, warning, and error states remain visually distinct.

Other local branches are not listed. Untracked directories count as one entry,
not every file inside them.

## Search, Refresh, And Layout

- Search matches repository, branch, path, and Workspace name. Hiding it clears the query.
- Search and refresh sit beside the tabs, with repositories immediately below.
- Initial discovery explains what is loading; refresh highlights its button without moving rows.
- Drag the divider above the tabs to resize the panel. Its scroll position is independent of Workspaces and Agents.
- The selected tab is remembered. Blocked-Agent counts remain visible while viewing Git.

Git uses only the existing sidebar. Switching tabs does not replace terminal
entities or mutate Shells, Agents, worktrees, branches, or repositories.

## Associations and authority

The Git host service starts with each Shell's stored launch cwd. On Linux, the
owning daemon can instead observe the cwd of its exact current managed process
under the Shell lifecycle and process locks, checking that it has not exited
before and after reading `/proc/<pid>/cwd`. This follows ordinary `cd` in the
managed shell. It does not inspect descendant process trees, interpret an SSH
session's remote directory, or infer an Agent's cwd. Subshells can have a
different cwd; the UI therefore labels this source **Shell process cwd**.
Unavailable process observations fall back to explicitly labeled **launch cwd**.
Neither observation rewrites the Shell's durable launch directory.

Active Agent associations require the exact current ShellRun. The panel shows
an Agent's Shell association separately from its structured, bounded observed
working contexts. Each Agent/ShellRun appears once per worktree, with both sources
listed together when they refer to the same worktree. An observed context is historical evidence of work there, not
proof that the Agent is currently modifying that repository. Inactive and Done
Agents do not decorate current work. Git state never changes Agent lifecycle.

Repositories are grouped by canonical Git common directory on their owning
Node. Registered sibling worktrees are discovered with `git worktree list`,
including ones without a managed Shell. Discovery is seeded by managed work;
there is no whole-machine scan or permanent repository registry. A repository
with no remaining Shell/active Agent association leaves the overview on refresh.

## Status semantics

- **Clean** describes only the working tree. It does not mean pushed or merged.
- **No upstream** is distinct from unpublished work.
- Ahead/behind and **Matches upstream** compare local remote-tracking refs.
  A missing/pruned upstream ref has unavailable comparison, never zero divergence.
  Boomux does not automatically fetch. Observation ages accompany expanded rows.
- Errors and exceeded inspection budgets yield unknown status, never clean.
- GitHub PR state is independent of local HEAD. A differing PR head explicitly
  warns that the PR/checks describe a different commit.
- PR lookup filters by head repository and owner, so same-named fork branches
  cannot acquire each other's PR. Multiple matching open PRs remain ambiguous.
  The newest matching historical PR remains visible after merge or closure.
- Check failures, pending/unknown checks, no checks, and successful checks remain
  distinct. This is a summary of returned checks, not authorization to merge.
- Network/authentication failures preserve the prior PR observation with a stale
  error and its original observation time.

GitHub.com is the initial provider. Its CLI must be installed and authenticated
on the owning Node. Lookup uses the branch's push remote (`pushRemote`,
`remote.pushDefault`, then branch remote, with `origin` fallback). The PR target
uses `gh repo set-default` metadata, then the conventional `upstream` remote,
then the push repository. GitLab, enterprise hosts, unsupported remotes, and
missing authentication leave local Git information usable. No credentials are
copied between Nodes or stored in Boomux state.

## Scheduling and bounds

The client queries only while the Git tab is selected. Socket operations have a
bounded timeout; remote Nodes are queried through the verified host-service
route. Unavailable Nodes retain labeled stale presentation. The first 16 Nodes
are shown with an explicit limit notice when necessary.

Each daemon owns one demand-driven inspection service. Requests return the
cached overview immediately and coalesce behind a single bounded worker. Local
inspection starts at five-second intervals and backs off to fifteen seconds
when semantic results are unchanged. PR observations have a separate sixty-second
cache. Explicit Refresh refreshes both, subject to a one-second minimum interval.
Selecting Agents stops Git polling; no per-Shell timer is created.

A cycle admits at most 512 Shell/Agent associations, 128 worktrees, and 512
additional branch labels. Git commands have a one-second timeout and a 1 MiB
output limit; GitHub CLI calls have a three-second timeout and the same output
limit. Discovery/inspection stops admitting work after a twenty-second cycle
budget (an already admitted operation may finish its bounded commands). Limits
and unknown results are surfaced. Commands use exact argv and owned process
groups; Git inspection disables optional index locks and clears environment
variables that could redirect it into another repository.

The cache is disposable and is rebuilt after daemon replacement. Protocol 53
adds `git_work_overview` through `HostService` / `RouteNodeHostService` and the
corresponding result. Older peers reject the feature before inspection. No
persistence schema changes or durable Git-status events are required.

## Validation

Focused fixtures cover porcelain parsing with renames/newlines/conflicts,
unborn and linked worktrees, fork-aware PR matching, ambiguous PRs and unknown
checks, exact live Shell cwd without changing launch metadata, old-version
rejection, and owner-side remote execution. UI checks must verify terminal
visibility, input, and layout preservation while toggling/resizing the panel.
Comprehensive checks remain in PR CI.

The development launcher preserves `GH_CONFIG_DIR` (or points it at the original
GitHub CLI config directory) before isolating Boomux XDG directories. It does not
copy or modify GitHub credentials.

## Clean Up Worktrees

Choose **Clean up…** beside Refresh to open a review dialog without changing the
terminal layout. It scans up to 128 discovered worktrees on their owning machines.
**Choose repository…** also finds registered worktrees in a repository
without any remaining managed Shell or Agent association. The picker does not
add a permanent repository registration.

The compact review groups worktrees into **Ready for cleanup**, **Needs review**,
and **Protected**, with largest directories first within each group. Ready requires
clean, inactive work with a confirmed merge and no known ahead commits; its
**Select all** action selects only that group. Needs review and Protected start
collapsed. A closed PR or missing upstream alone does not make a worktree Ready.
Clean unmerged work and work with local changes can be selected under Needs review.

Each row shows branch, repository/machine, the main reason, and estimated size.
Expand a row for its full path, local changes, ahead/behind counts from local
upstream refs, ignored entry counts, and associated running or retained Shells. No upstream or an unavailable
comparison means the unpushed count is unknown. Completion hints include a
merged/closed PR at the exact current HEAD, a missing local upstream ref, and
ancestry in the locally detected default branch. These are review hints, not
Agent completion or automatic selection. Boomux does not fetch.

Select directories, choose **Review removal…**, and confirm **Remove directories**.
If any selected worktree has local changes, a separate acknowledgment must be
checked before **Discard changes and remove** becomes available. This permanently
discards its staged, unstaged, and untracked files; retained branches do not
recover uncommitted changes. The discard choice applies only to the selected
dirty worktrees. A clean selection that becomes dirty is refused until rescanned. The confirmation includes the count and approximate
size. Removal deletes ignored files too, including `.env`, dependencies, and
build output. **Local branches and their unpushed commits are retained.** Shells,
panes, Workspace membership, and Agent history are retained. A retained Shell
whose working directory was removed may need its directory changed before it
can start again.

Primary, locked, detached/unborn, nested-repository, cross-filesystem,
submodule-configured, and incompletely inspected worktrees cannot be selected.
Index entries marked assume-unchanged or skip-worktree also block removal.
Associated running Shells or active Agent working contexts block removal;
unavailable running-Shell cwd observations fail closed. Activity covers managed
Shell launch/current process directories and structured Agent contexts, not all
external processes or shell descendants. Stop other tools using the directory
before cleanup.

Immediately before each removal, the owner rechecks registration, canonical
repository/worktree identity, directory device/inode, branch, HEAD, local work,
file traversal, and activity. It runs `git worktree remove -- <path>` while
holding the daemon mutation gate against managed Shell startup. Explicit discard
adds one `--force` only after revalidation, overriding just the local-changes
guard. All other protections still apply; locked worktrees are never unlocked or
double-forced.
Each result is reported independently. Lost responses and partial-removal errors
are never automatically retried; scan again before deciding what to do next.
External Git/filesystem writers are not serialized by Boomux's mutation gate.

Scanning and removal execute off the UI thread, one worktree at a time. **Stop
after current worktree** (or Escape during work) stops scheduling further work;
it does not interrupt a removal already admitted. Closing the dialog or changing
its generation discards late presentation results. No periodic size scan or
persistent cleanup cache is added. Each owner admits one cleanup operation at a
time. The directory walk does not follow symlinks, retains at most 64 directory
iterators and 200,000 inode keys, and stops admitting entries after three seconds.
Incomplete walks remain blocked. Sizes count allocated blocks once per inode
within each worktree; shared blocks between worktrees or caches can reduce the
actual space reclaimed. The main repository's Git directory is excluded.

Protocol 57 adds `ListCleanupWorktrees`, `InspectCleanupWorktree`, and
`RemoveCleanupWorktree` host services plus `git_worktree_cleanup` and
`git_worktree_discard_changes`. Removal's optional `discard_changes` defaults to
false and is omitted on the normal path. Both local and
routed requests require 57; older owners reject before inspecting or removing.
Git inspection commands retain one-second/1 MiB limits, PR lookup uses its
existing three-second limit, and removal is bounded to thirty seconds. Remote
responses allow sixty seconds and the Desktop client sixty-five seconds, with
bounded negotiation probes. Removal transport failures are ambiguous outcomes.
There are no persistence changes or new lifecycle events. Old Git-overview
clients ignore the additive optional PR state field.

### WebUI parity

The WebUI Git panel exposes **Clean up…** with the same Ready for cleanup,
Needs review and Protected groups, expandable details and explicit discard
acknowledgment. Its repository picker accepts an absolute path on the selected
Node. The browser submits the reviewed target to the gateway, which uses the
same bounded, non-replaying owner cleanup client as Desktop. Closing panes or
Shells remains a separate action.

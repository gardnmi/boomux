# Git panel

Boomux Desktop's Git panel shows repositories discovered from managed Shells and
active Agent working-context observations. Open it with the branch icon beside
Settings in the sidebar header or press
`Ctrl+Space`, then `G`. It is a resizable sibling of the terminal canvas; opening,
resizing, filtering, and closing it do not replace terminal entities or mutate
Shells, Agents, worktrees, branches, or repositories. In windows narrower than
1000 logical pixels, the ordinary sidebar temporarily yields its space to the
Git panel so the terminal canvas remains usable. Closing Git restores the sidebar
according to its existing preference.

Each repository has one shared container per owning Node, with compact worktree
rows separated by subtle dividers. Rows show the branch, Agent count, local Git
status, upstream comparison, and GitHub PR/check/review status. Expand a row for
the full path, Shell navigation, individual Agent associations, the last commit,
other local branches, observation ages,
copy-path, and open-PR actions. Untracked directories count as one entry rather
than enumerating every file inside them. Search matches repository, branch,
path, and Workspace name. Filters cover the current Workspace and work needing
attention. The compact icon toolbar reveals search on demand and opens filters
in an overlay; active filters highlight their toolbar button. Hiding search
clears its query. Refresh activity stays in the header without shifting rows.
Only one worktree row expands at a time.

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

The client queries only while the panel is open. Socket operations have a
bounded timeout; remote Nodes are queried through the verified host-service
route. Unavailable Nodes retain labeled stale presentation. The first 16 Nodes
are shown with an explicit limit notice when necessary.

Each daemon owns one demand-driven inspection service. Requests return the
cached overview immediately and coalesce behind a single bounded worker. Local
inspection starts at five-second intervals and backs off to fifteen seconds
when semantic results are unchanged. PR observations have a separate sixty-second
cache. Explicit Refresh refreshes both, subject to a one-second minimum interval.
Closing the panel stops its polling; no per-Shell timer is created.

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

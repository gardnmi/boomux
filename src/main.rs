use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::error::Error;
use std::ffi::OsString;
use std::fmt::Write as _;
use std::fs;
use std::io::{self, IsTerminal, Read};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use clap::error::ErrorKind as ClapErrorKind;
use clap::{ArgGroup, Args, Parser, Subcommand, ValueEnum};
use uuid::Uuid;

use boomux::protocol::{
    AgentAuthority, AgentInstanceSnapshot, AgentRegistrationSpec, AgentReport,
    AgentScheduleOverlapPolicy, AgentScheduleSession, AgentScheduleSnapshot, AgentScheduleSpec,
    AgentScheduleState, AgentScheduleTrigger, AgentScheduleUpdate, AgentState, EventCursor,
    ScheduledExecutionOutcome, ScheduledExecutionSnapshot, ScheduledRunnerResult, ShellSnapshot,
    ShellSpec, ShellStatus, Snapshot, WorkspaceLauncherSnapshot, WorkspaceLauncherSpec,
    WorkspaceSnapshot,
};
use boomux::{attach, client, daemon, federation, protocol, ssh_bootstrap};

use crate::integration_management::{
    InstallOutcome, ensure_safe_directory, install_asset_at, regular_file_matches,
    require_absolute_root,
};

mod agent_attention_projection;
mod cli_output;
mod config;
mod dashboard_projection;
mod generated_names;
mod git;
mod host_session_source;
mod host_session_titles;
mod integration_management;
mod process_adapter;
mod projects;
mod session_projection;
mod terminal;
mod tui;

const DASHBOARD_FALLBACK_REFRESH_INTERVAL: Duration = Duration::from_secs(1);
const DASHBOARD_EXECUTION_CACHE_LIMIT: u16 = 1_000;

struct DashboardRefresh {
    watch: client::SnapshotWatch,
    last_snapshot_at: Instant,
    executions: BTreeMap<String, ScheduledExecutionSnapshot>,
    history_truncated: bool,
    scoped_history: BTreeMap<String, bool>,
    negotiated_protocol: u32,
    needs_reseed: bool,
    reseed_stream_changed: bool,
}

impl DashboardRefresh {
    fn baseline(client: &client::Client) -> client::Result<Self> {
        let watch = client::SnapshotWatch::baseline(client)?;
        let negotiated_protocol = client.protocol_version()?;
        let page = if protocol::ProtocolFeature::ScheduledExecutionObservation
            .is_supported_by(negotiated_protocol)
        {
            Some(client.scheduled_execution_page(None, None, DASHBOARD_EXECUTION_CACHE_LIMIT)?)
        } else {
            None
        };
        Ok(Self {
            watch,
            last_snapshot_at: Instant::now(),
            executions: page
                .as_ref()
                .into_iter()
                .flat_map(|page| page.executions.iter().cloned())
                .map(|execution| (execution.id.clone(), execution))
                .collect(),
            history_truncated: page.as_ref().is_some_and(|page| page.truncated),
            scoped_history: BTreeMap::new(),
            negotiated_protocol,
            needs_reseed: false,
            reseed_stream_changed: false,
        })
    }

    fn snapshot(&self) -> &Snapshot {
        self.watch.snapshot()
    }

    fn check(&mut self, client: &client::Client) -> client::Result<Option<(Snapshot, bool)>> {
        if self.needs_reseed {
            let stream_changed = self.reseed_stream_changed;
            self.reseed_executions(client)?;
            self.last_snapshot_at = Instant::now();
            return Ok(Some((self.watch.snapshot().clone(), stream_changed)));
        }
        match self.watch.poll(client) {
            Ok(poll) => {
                if poll.changed {
                    if poll.stream_changed || poll.baseline_replaced {
                        self.needs_reseed = true;
                        self.reseed_stream_changed |= poll.stream_changed;
                        self.reseed_executions(client)?;
                    } else {
                        self.apply_events(&poll.events);
                    }
                    self.last_snapshot_at = Instant::now();
                    return Ok(Some((self.watch.snapshot().clone(), poll.stream_changed)));
                }
                if self.last_snapshot_at.elapsed() < DASHBOARD_FALLBACK_REFRESH_INTERVAL {
                    return Ok(None);
                }
                if !self.watch.uses_events() {
                    *self = Self::baseline(client)?;
                    return Ok(Some((self.watch.snapshot().clone(), false)));
                }
                *self.watch.snapshot_mut() = client.snapshot()?;
                self.last_snapshot_at = Instant::now();
                Ok(Some((self.watch.snapshot().clone(), false)))
            }
            Err(error) => Err(error),
        }
    }

    fn refresh(&mut self, client: &client::Client) -> client::Result<(Snapshot, bool)> {
        let stream_id = self.watch.stream_id().map(str::to_owned);
        *self = Self::baseline(client)?;
        Ok((
            self.watch.snapshot().clone(),
            self.watch.stream_id() != stream_id.as_deref(),
        ))
    }

    fn executions(&self) -> Vec<ScheduledExecutionSnapshot> {
        let mut executions = self.executions.values().cloned().collect::<Vec<_>>();
        executions.sort_by(|left, right| {
            right
                .requested_at_ms
                .cmp(&left.requested_at_ms)
                .then_with(|| right.id.cmp(&left.id))
        });
        executions
    }

    fn load_schedule_history(
        &mut self,
        client: &client::Client,
        schedule_id: &str,
        limit: u16,
    ) -> client::Result<(Vec<ScheduledExecutionSnapshot>, bool)> {
        let page = client.scheduled_execution_page(None, Some(schedule_id.to_owned()), limit)?;
        self.executions
            .retain(|_, execution| execution.schedule_id != schedule_id);
        for execution in &page.executions {
            self.executions
                .insert(execution.id.clone(), execution.clone());
        }
        self.scoped_history.clear();
        self.scoped_history
            .insert(schedule_id.to_owned(), page.truncated);
        self.bound_execution_cache();
        Ok((page.executions, page.truncated))
    }

    fn reseed_executions(&mut self, client: &client::Client) -> client::Result<()> {
        if !protocol::ProtocolFeature::ScheduledExecutionObservation
            .is_supported_by(self.negotiated_protocol)
        {
            self.executions.clear();
            self.scoped_history.clear();
            self.history_truncated = false;
            self.needs_reseed = false;
            self.reseed_stream_changed = false;
            return Ok(());
        }
        let page = client.scheduled_execution_page(None, None, DASHBOARD_EXECUTION_CACHE_LIMIT)?;
        let executions = page
            .executions
            .into_iter()
            .map(|execution| (execution.id.clone(), execution))
            .collect();
        self.executions = executions;
        self.scoped_history.clear();
        self.history_truncated = page.truncated;
        self.needs_reseed = false;
        self.reseed_stream_changed = false;
        Ok(())
    }

    fn apply_events(&mut self, events: &[protocol::DaemonEvent]) {
        for event in events {
            match &event.kind {
                protocol::DaemonEventKind::ScheduledExecutionCreated { execution, .. }
                | protocol::DaemonEventKind::ScheduledExecutionChanged { execution, .. } => {
                    let should_replace = self
                        .executions
                        .get(&execution.id)
                        .is_none_or(|current| execution.revision > current.revision);
                    if should_replace {
                        self.executions
                            .insert(execution.id.clone(), execution.clone());
                    }
                }
                protocol::DaemonEventKind::AgentScheduleRemoved { schedule_id, .. } => {
                    self.executions
                        .retain(|_, execution| execution.schedule_id != *schedule_id);
                    self.scoped_history.remove(schedule_id);
                }
                _ => {}
            }
        }
        self.bound_execution_cache();
    }

    fn bound_execution_cache(&mut self) {
        while self.executions.len() > usize::from(DASHBOARD_EXECUTION_CACHE_LIMIT) {
            let oldest_terminal = self
                .executions
                .values()
                .filter(|execution| {
                    execution.state.is_terminal()
                        && !self.scoped_history.contains_key(&execution.schedule_id)
                })
                .min_by(|left, right| {
                    left.requested_at_ms
                        .cmp(&right.requested_at_ms)
                        .then_with(|| left.id.cmp(&right.id))
                })
                .map(|execution| execution.id.clone())
                .or_else(|| {
                    self.executions
                        .values()
                        .filter(|execution| execution.state.is_terminal())
                        .min_by(|left, right| {
                            left.requested_at_ms
                                .cmp(&right.requested_at_ms)
                                .then_with(|| left.id.cmp(&right.id))
                        })
                        .map(|execution| execution.id.clone())
                });
            let Some(id) = oldest_terminal else { break };
            self.executions.remove(&id);
        }
    }
}

const BOOMUX_SKILL: &str = include_str!("../.agents/skills/boomux/SKILL.md");
#[cfg(test)]
const BOOMUX_OPENCODE_PLUGIN: &str = integration_management::OPENCODE_ASSET;
#[cfg(test)]
const BOOMUX_PI_EXTENSION: &str = integration_management::PI_ASSET;
const MAX_HOST_CATALOG_DIRECTORIES: usize = 8;
const NON_PROTOCOL_FEATURES: &[&str] = &[
    "opencode_lifecycle_plugin",
    "pi_lifecycle_extension",
    "process_adapters",
    "desktop_notifications",
    "sound_notifications",
    "integration_management",
];
const LEGACY_BOOMUX_SHELLS_SKILL: &str = r#"---
name: boomux-shells
description: Read output and logs from Boomux workspace shells. Use when asked to inspect another shell by name, read shell2, examine terminal output, check logs from another terminal, or inspect a Boomux shell ID.
compatibility: Requires boomux on PATH. Shell-name lookup requires running inside a Boomux-managed shell.
metadata:
  author: boomux
  version: "1"
---

# Boomux Shells

Use Boomux to inspect output from another persistent shell without asking the
user to copy terminal contents.

## Read A Shell

When the user provides a shell name or shell ID, run:

```console
boomux read "<name-or-shell-id>" --lines 200
```

Use the returned text to answer the user's question. Increase `--lines` when
the relevant output is older. This reads Boomux's bounded, plain rendered VT
scrollback. It does not include ANSI sequences or a process's complete
historical log.

## Discover Shells

When the target is missing, unclear, or not found, run:

```console
boomux shells
```

Match the user's wording against the displayed shell names. Ask for
clarification only when multiple shells remain plausible.

Shell names are resolved within the current Boomux workspace. Exact Boomux
shell IDs can be read directly.
"#;
const READ_BYTES: usize = 1024 * 1024;

#[derive(Parser)]
#[command(
    version,
    about = "Native persistent terminal workspaces",
    subcommand_value_name = "SUBCOMMAND"
)]
struct Cli {
    /// Emit the stable boomux.cli/v1 JSON envelope
    #[arg(long, global = true)]
    json: bool,

    /// Connect ad hoc to one Boomux Node through OpenSSH
    #[arg(long, value_name = "TARGET")]
    remote: Option<String>,

    /// Open or create a persistent terminal in this directory
    #[arg(value_name = "PATH")]
    path: Option<PathBuf>,

    /// Name of the workspace group
    #[arg(short, long, requires = "path")]
    name: Option<String>,

    /// Open the terminal in a new window instead of attaching here
    #[arg(long = "new", requires = "path")]
    new_window: bool,

    /// XDG desktop entry to use for windows opened by this invocation
    #[arg(long, global = true, value_name = "DESKTOP_ENTRY")]
    terminal: Option<String>,

    /// Command and arguments to run instead of the login shell
    #[arg(last = true, num_args = 1.., requires = "path", value_name = "COMMAND")]
    startup_command: Vec<String>,

    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand)]
enum Commands {
    /// Open the interactive workspace dashboard
    Ui,
    /// Check that Boomux's dependencies and daemon are available
    Doctor,
    /// Report stable integration capabilities without starting the daemon
    Capabilities,
    /// List all managed shells
    List,
    /// List shells in the current Boomux workspace
    Shells,
    /// Read retained output from a shell name or shell ID
    Read {
        target: String,
        #[arg(long, default_value_t = 200, value_parser = clap::value_parser!(u32).range(1..))]
        lines: u32,
        #[arg(long, requires = "after_revision")]
        run_id: Option<String>,
        #[arg(long, requires = "run_id")]
        after_revision: Option<u64>,
        #[arg(long, default_value_t = 0, requires = "after_revision")]
        wait_ms: u32,
    },
    /// Read a bounded batch of daemon events
    Events {
        #[arg(long)]
        after: Option<String>,
        #[arg(long, default_value_t = 256, value_parser = clap::value_parser!(u16).range(1..=256))]
        limit: u16,
        #[arg(long, default_value_t = 0)]
        wait_ms: u32,
    },
    /// Close a shell by name or shell ID
    Close { target: String },
    /// Discover configured projects
    Project {
        #[command(subcommand)]
        command: ProjectCommands,
    },
    /// Manage workspaces
    Workspace {
        #[command(subcommand)]
        command: WorkspaceCommands,
    },
    /// Manage this Boomux Node identity
    Node {
        #[command(subcommand)]
        command: NodeCommands,
    },
    /// Manage shells
    Shell {
        #[command(subcommand)]
        command: ShellCommands,
    },
    /// Manage workspace launchers
    Launcher {
        #[command(subcommand)]
        command: LauncherCommands,
    },
    /// Inspect and report external agent runtime state
    Agent {
        #[command(subcommand)]
        command: AgentCommands,
    },
    /// Inspect and acknowledge blocked or completed Agent work
    Attention {
        #[command(subcommand)]
        command: AttentionCommands,
    },
    /// Test configured desktop and sound notification delivery
    Notification {
        #[command(subcommand)]
        command: NotificationCommands,
    },
    /// Discover projected agent sessions
    Session {
        #[command(subcommand)]
        command: SessionCommands,
    },
    /// Manage recurring Agent work definitions
    Schedule {
        #[command(subcommand)]
        command: ScheduleCommands,
    },
    /// Inspect and cancel scheduled execution records
    Execution {
        #[command(subcommand)]
        command: ExecutionCommands,
    },
    /// Inspect and install supported harness integrations
    Integration {
        #[command(subcommand)]
        command: IntegrationCommands,
    },
    /// Manage the vendor-neutral Boomux Agent Skill
    Skill {
        #[command(subcommand)]
        command: SkillCommands,
    },
    /// Manage the Boomux OpenCode integration
    Opencode {
        #[command(subcommand)]
        command: OpenCodeCommands,
    },
    /// Manage the Boomux Pi integration
    Pi {
        #[command(subcommand)]
        command: PiCommands,
    },
    /// Open a shell in a new terminal window
    Open {
        shell_id: String,
        /// Exact registered Node alias or Node ID
        #[arg(long)]
        node: Option<String>,
        #[arg(long)]
        title: Option<String>,
        #[arg(long)]
        takeover: bool,
    },
    /// Print the current Boomux workspace and shell name for prompt integrations
    #[command(hide = true)]
    Prompt,
    /// Manage the background PTY daemon
    Daemon {
        #[command(subcommand)]
        command: DaemonCommands,
    },
    #[command(name = "__attach", hide = true)]
    Attach {
        shell_id: String,
        #[arg(long)]
        node: Option<String>,
        #[arg(long)]
        takeover: bool,
        #[arg(long)]
        restart_exited: bool,
        #[arg(long)]
        expected_run_id: Option<String>,
    },
    #[command(name = "__scheduled-runner", hide = true)]
    ScheduledRunner { schedule_id: String },
    #[command(name = "__resume-session", hide = true)]
    ResumeSession {
        session_id: String,
        #[arg(long)]
        node: Option<String>,
    },
    #[command(name = "__federation-stdio", hide = true)]
    FederationStdio,
}

#[derive(Subcommand)]
enum ProjectCommands {
    /// List projects discovered from configured roots
    List {
        /// Exact registered Node alias or Node ID
        #[arg(long)]
        node: Option<String>,
    },
}

#[derive(Subcommand)]
enum NodeCommands {
    /// Verify and persist a remote Boomux Node route
    Add { alias: String, target: String },
    /// List registered remote Nodes
    List,
    /// Inspect a registered remote Node by alias or exact Node ID
    Inspect { selector: String },
    /// Show the combined Node-qualified local and cached remote projection
    Snapshot { selector: Option<String> },
    /// Rename a registered remote Node alias at an exact revision
    Rename {
        selector: String,
        alias: String,
        #[arg(long)]
        revision: u64,
    },
    /// Verify and replace a registered SSH route at an exact revision
    Retarget {
        selector: String,
        target: String,
        #[arg(long)]
        revision: u64,
    },
    /// Forget a registration without contacting the remote Node
    Forget { selector: String },
    /// Assign this authority a new Node ID after exact confirmation
    Rekey,
}

#[derive(Subcommand)]
enum WorkspaceCommands {
    /// List workspaces
    List,
    /// Create an empty workspace
    Create {
        name: String,
        #[arg(long)]
        cwd: Option<PathBuf>,
    },
    /// Open terminal windows and invoke launchers
    Open {
        target: String,
        /// Exact registered Node alias or Node ID
        #[arg(long)]
        node: Option<String>,
    },
    /// Show a workspace and its shells
    Inspect { target: String },
    /// Rename a workspace
    Rename { target: String, name: String },
    /// Close a workspace and all of its shells
    Close { target: String },
}

#[derive(Subcommand)]
enum LauncherCommands {
    /// List launchers in a workspace
    List {
        #[arg(long, value_name = "NAME_OR_ID")]
        workspace: String,
    },
    /// Create a launcher in a workspace
    Create {
        name: String,
        #[arg(long, value_name = "NAME_OR_ID")]
        workspace: String,
        #[arg(long, default_value = ".")]
        cwd: PathBuf,
        #[arg(last = true, num_args = 1.., required = true, value_name = "COMMAND")]
        command: Vec<String>,
    },
    /// Show launcher details
    Inspect {
        target: String,
        #[arg(long, value_name = "NAME_OR_ID")]
        workspace: Option<String>,
    },
    /// Invoke a launcher as a detached process
    Invoke {
        target: String,
        #[arg(long, value_name = "NAME_OR_ID")]
        workspace: Option<String>,
        /// Exact registered Node alias or Node ID
        #[arg(long)]
        node: Option<String>,
    },
    /// Rename a launcher
    Rename {
        target: String,
        name: String,
        #[arg(long, value_name = "NAME_OR_ID")]
        workspace: Option<String>,
    },
    /// Remove a launcher without affecting previously launched applications
    Remove {
        target: String,
        #[arg(long, value_name = "NAME_OR_ID")]
        workspace: Option<String>,
    },
}

#[derive(Subcommand)]
enum ShellCommands {
    /// Suggest an unused generated name without reserving it
    SuggestName {
        workspace: String,
        /// Exact registered Node alias or Node ID
        #[arg(long)]
        node: Option<String>,
    },
    /// Create a pending shell in a workspace
    Create {
        workspace: String,
        #[arg(long)]
        name: Option<String>,
        #[arg(long)]
        cwd: Option<PathBuf>,
        #[arg(last = true, num_args = 1.., value_name = "COMMAND")]
        command: Vec<String>,
    },
    /// Show shell details
    Inspect {
        target: String,
        #[arg(long, value_name = "NAME_OR_ID")]
        workspace: Option<String>,
    },
    /// Rename a shell
    Rename {
        target: String,
        name: String,
        #[arg(long, value_name = "NAME_OR_ID")]
        workspace: Option<String>,
    },
    /// Close a shell
    Close {
        target: String,
        #[arg(long, value_name = "NAME_OR_ID")]
        workspace: Option<String>,
    },
}

#[derive(Subcommand)]
enum AgentCommands {
    /// List agent instances
    List {
        #[arg(long, value_name = "NAME_OR_ID")]
        workspace: Option<String>,
    },
    /// Show an agent instance by exact ID
    #[command(alias = "get")]
    Inspect { agent_id: String },
    /// Wait for an exact Agent observation revision to advance
    Wait {
        agent_id: String,
        #[arg(long)]
        after_revision: u64,
        #[arg(long, default_value_t = 30_000)]
        wait_ms: u32,
    },
    /// Register an agent instance for a shell run
    Register(AgentRegistrationArgs),
    /// Ensure an idempotent agent instance for a shell run
    Ensure(AgentRegistrationArgs),
    /// Supervise one exact external agent process
    Supervise(AgentSuperviseArgs),
    /// Report state for an agent instance by exact ID
    Report {
        agent_id: String,
        #[arg(long)]
        shell_id: Option<String>,
        #[arg(long)]
        run_id: Option<String>,
        #[arg(long, value_enum)]
        state: CliAgentState,
        #[arg(long, value_enum)]
        authority: CliAgentAuthority,
        #[arg(long)]
        evidence: String,
        #[arg(long, value_parser = clap::value_parser!(u8).range(0..=100))]
        confidence: u8,
    },
}

#[derive(Subcommand)]
enum AttentionCommands {
    /// List outstanding attention in priority order
    List {
        #[arg(long, value_name = "NAME_OR_ID")]
        workspace: Option<String>,
    },
    /// Acknowledge one exact attention-raising observation
    Acknowledge {
        agent_id: String,
        #[arg(long)]
        observation_revision: u64,
    },
}

#[derive(Subcommand)]
enum NotificationCommands {
    /// Deliver a test notification through every configured channel
    Test {
        #[arg(value_enum)]
        reason: CliNotificationReason,
    },
}

#[derive(Clone, Copy, ValueEnum)]
enum CliNotificationReason {
    Blocked,
    Completed,
}

impl From<CliNotificationReason> for daemon::NotificationTestReason {
    fn from(reason: CliNotificationReason) -> Self {
        match reason {
            CliNotificationReason::Blocked => Self::Blocked,
            CliNotificationReason::Completed => Self::Completed,
        }
    }
}

#[derive(Subcommand)]
enum SessionCommands {
    /// List projected agent sessions
    List {
        #[arg(long, value_name = "NAME_OR_ID")]
        workspace: Option<String>,
        /// Exact registered Node alias or Node ID
        #[arg(long)]
        node: Option<String>,
    },
    /// Show a projected session by exact opaque ID
    #[command(alias = "get")]
    Inspect {
        session_id: String,
        /// Exact registered Node alias or Node ID
        #[arg(long)]
        node: Option<String>,
    },
    /// Resume one exact projected session in a native terminal
    Resume {
        session_id: String,
        /// Exact registered Node alias or Node ID
        #[arg(long)]
        node: Option<String>,
    },
}

#[derive(Subcommand)]
enum ScheduleCommands {
    /// Create a durable schedule definition
    Create(Box<ScheduleCreateArgs>),
    /// List schedule definitions without disclosing prompts
    List {
        #[arg(long, value_name = "NAME_OR_ID")]
        workspace: Option<String>,
        /// Exact registered Node alias or Node ID
        #[arg(long)]
        node: Option<String>,
    },
    /// Inspect a schedule, including its private prompt
    Inspect {
        target: String,
        #[arg(long, value_name = "NAME_OR_ID")]
        workspace: Option<String>,
        /// Exact registered Node alias or Node ID
        #[arg(long)]
        node: Option<String>,
    },
    /// Mark a schedule paused in durable management state
    Pause {
        target: String,
        #[arg(long, value_name = "NAME_OR_ID")]
        workspace: Option<String>,
        /// Exact registered Node alias or Node ID
        #[arg(long)]
        node: Option<String>,
    },
    /// Enable future timed dispatch; run-now is independent
    Resume {
        target: String,
        #[arg(long, value_name = "NAME_OR_ID")]
        workspace: Option<String>,
        /// Exact registered Node alias or Node ID
        #[arg(long)]
        node: Option<String>,
    },
    /// Remove an inactive schedule and its persisted prompt
    Remove {
        target: String,
        #[arg(long, value_name = "NAME_OR_ID")]
        workspace: Option<String>,
        /// Exact registered Node alias or Node ID
        #[arg(long)]
        node: Option<String>,
    },
    /// Dispatch one execution now, including while paused
    Run {
        target: String,
        #[arg(long, value_name = "NAME_OR_ID")]
        workspace: Option<String>,
        #[arg(long, value_name = "UUID")]
        idempotency_key: Option<Uuid>,
        /// Exact registered Node alias or Node ID
        #[arg(long)]
        node: Option<String>,
    },
}

impl ScheduleCommands {
    fn node(&self) -> Option<&str> {
        match self {
            Self::Create(arguments) => arguments.node.as_deref(),
            Self::List { node, .. }
            | Self::Inspect { node, .. }
            | Self::Pause { node, .. }
            | Self::Resume { node, .. }
            | Self::Remove { node, .. }
            | Self::Run { node, .. } => node.as_deref(),
        }
    }
}

#[derive(Subcommand)]
enum ExecutionCommands {
    /// List prompt-free execution records
    List {
        #[arg(long, value_name = "NAME_OR_ID")]
        workspace: Option<String>,
        #[arg(long, value_name = "SCHEDULE_NAME_OR_ID")]
        schedule: Option<String>,
        #[arg(
            long,
            default_value_t = protocol::DEFAULT_SCHEDULED_EXECUTION_LIST_LIMIT,
            value_parser = clap::value_parser!(u16).range(1..=protocol::MAX_SCHEDULED_EXECUTION_LIST_LIMIT as i64)
        )]
        limit: u16,
        /// Exact registered Node alias or Node ID
        #[arg(long)]
        node: Option<String>,
    },
    /// Inspect one execution by exact ID
    Inspect {
        execution_id: String,
        /// Exact registered Node alias or Node ID
        #[arg(long)]
        node: Option<String>,
    },
    /// Wait for an exact execution revision to advance
    Wait {
        execution_id: String,
        #[arg(long)]
        after_revision: u64,
        #[arg(long, default_value_t = 30_000)]
        wait_ms: u32,
        /// Exact registered Node alias or Node ID
        #[arg(long)]
        node: Option<String>,
    },
    /// Open an execution's exact active run or linked Agent Session
    Open {
        execution_id: String,
        /// Exact registered Node alias or Node ID
        #[arg(long)]
        node: Option<String>,
    },
    /// Cancel one nonterminal execution by exact ID
    Cancel {
        execution_id: String,
        /// Exact registered Node alias or Node ID
        #[arg(long)]
        node: Option<String>,
    },
}

impl ExecutionCommands {
    fn node(&self) -> Option<&str> {
        match self {
            Self::List { node, .. }
            | Self::Inspect { node, .. }
            | Self::Wait { node, .. }
            | Self::Open { node, .. }
            | Self::Cancel { node, .. } => node.as_deref(),
        }
    }
}

#[derive(Args)]
#[command(group(
    ArgGroup::new("prompt_source")
        .required(true)
        .multiple(false)
        .args(["prompt", "prompt_file"])
), group(
    ArgGroup::new("trigger_source")
        .required(true)
        .multiple(false)
        .args(["cron", "every", "daily", "weekdays", "weekly"])
), group(
    ArgGroup::new("session_mode")
        .multiple(false)
        .args(["fresh", "continue_session"])
), group(
    ArgGroup::new("initial_state")
        .multiple(false)
        .args(["paused", "enabled"])
))]
struct ScheduleCreateArgs {
    /// Workspace-unique schedule name
    name: String,
    /// Owning workspace name or ID
    #[arg(long, value_name = "NAME_OR_ID")]
    workspace: String,
    /// Existing working directory to snapshot
    #[arg(long, value_name = "PATH")]
    cwd: PathBuf,
    /// Integration accepted for future scheduled execution
    #[arg(long)]
    integration: String,
    /// Exact inline prompt to persist
    #[arg(long)]
    prompt: Option<String>,
    /// Regular UTF-8 file to snapshot as the prompt
    #[arg(long, value_name = "PATH")]
    prompt_file: Option<PathBuf>,
    /// Canonical five-field cron expression
    #[arg(long)]
    cron: Option<String>,
    /// Minute or hour interval, such as 15m or 6h
    #[arg(long, value_name = "Nm|Nh")]
    every: Option<String>,
    /// Store a daily trigger at this local time
    #[arg(long, value_name = "HH:MM")]
    daily: Option<String>,
    /// Store a weekday trigger at this local time
    #[arg(long, value_name = "HH:MM")]
    weekdays: Option<String>,
    /// Store a weekly trigger for this English day and local time
    #[arg(long, value_name = "DAY@HH:MM")]
    weekly: Option<String>,
    /// IANA timezone; defaults to the resolved system timezone
    #[arg(long, value_name = "IANA_TIMEZONE")]
    timezone: Option<String>,
    /// Plan a new external Agent Session for every future execution
    #[arg(long)]
    fresh: bool,
    /// Pin one exact projected Agent Session
    #[arg(long = "continue", value_name = "PROJECTED_SESSION_ID")]
    continue_session: Option<String>,
    /// Create paused, which is the default
    #[arg(long)]
    paused: bool,
    /// Record consent for future timed dispatch
    #[arg(long)]
    enabled: bool,
    /// Exact registered Node alias or Node ID
    #[arg(long)]
    node: Option<String>,
}

#[derive(Args)]
struct AgentSuperviseArgs {
    name: Option<String>,
    #[arg(long)]
    integration: String,
    #[arg(long)]
    external_session_id: String,
    #[arg(long)]
    shell_id: Option<String>,
    #[arg(long)]
    run_id: Option<String>,
    #[arg(last = true, num_args = 1.., required = true, value_name = "COMMAND")]
    command: Vec<String>,
}

#[derive(Args)]
struct AgentRegistrationArgs {
    name: Option<String>,
    #[arg(long)]
    integration: String,
    #[arg(long)]
    external_session_id: Option<String>,
    #[arg(long)]
    shell_id: Option<String>,
    #[arg(long)]
    run_id: Option<String>,
    #[arg(long, value_enum)]
    state: CliAgentState,
    #[arg(long, value_enum)]
    authority: CliAgentAuthority,
    #[arg(long)]
    evidence: String,
    #[arg(long, value_parser = clap::value_parser!(u8).range(0..=100))]
    confidence: u8,
}

#[derive(Clone, Copy, ValueEnum)]
enum CliAgentState {
    Unknown,
    Working,
    Blocked,
    Idle,
    Inactive,
    Done,
}

impl From<CliAgentState> for AgentState {
    fn from(state: CliAgentState) -> Self {
        match state {
            CliAgentState::Unknown => Self::Unknown,
            CliAgentState::Working => Self::Working,
            CliAgentState::Blocked => Self::Blocked,
            CliAgentState::Idle => Self::Idle,
            CliAgentState::Inactive => Self::Inactive,
            CliAgentState::Done => Self::Done,
        }
    }
}

#[derive(Clone, Copy, ValueEnum)]
enum CliAgentAuthority {
    LifecycleIntegration,
    ProcessAdapter,
    TerminalHeuristic,
}

impl From<CliAgentAuthority> for AgentAuthority {
    fn from(authority: CliAgentAuthority) -> Self {
        match authority {
            CliAgentAuthority::LifecycleIntegration => Self::LifecycleIntegration,
            CliAgentAuthority::ProcessAdapter => Self::ProcessAdapter,
            CliAgentAuthority::TerminalHeuristic => Self::TerminalHeuristic,
        }
    }
}

#[derive(Subcommand)]
enum SkillCommands {
    /// Install the Boomux CLI skill under ~/.agents/skills
    Install {
        #[arg(long)]
        force: bool,
    },
}

#[derive(Subcommand)]
enum OpenCodeCommands {
    /// Install the Boomux plugin in the global OpenCode configuration
    Install {
        #[arg(long)]
        force: bool,
    },
}

#[derive(Subcommand)]
enum PiCommands {
    /// Install the Boomux extension in the global Pi configuration
    Install {
        #[arg(long)]
        force: bool,
    },
}

#[derive(Subcommand)]
enum IntegrationCommands {
    /// List integrations bundled with this Boomux binary
    List,
    /// Inspect host, asset, and runtime reporting status
    Status {
        integration: Option<integration_management::IntegrationId>,
        #[arg(long)]
        node: Option<String>,
    },
    /// Install one integration or every bundled integration
    Install {
        #[arg(required_unless_present = "all", conflicts_with = "all")]
        integration: Option<integration_management::IntegrationId>,
        #[arg(long, conflicts_with = "integration")]
        all: bool,
        #[arg(long)]
        force: bool,
        #[arg(long)]
        dry_run: bool,
        #[arg(long)]
        node: Option<String>,
    },
    /// Remove one integration or every bundled integration
    Uninstall {
        #[arg(required_unless_present = "all", conflicts_with = "all")]
        integration: Option<integration_management::IntegrationId>,
        #[arg(long, conflicts_with = "integration")]
        all: bool,
        #[arg(long)]
        force: bool,
        #[arg(long)]
        node: Option<String>,
    },
    /// Inspect, install, and explain verification for one integration
    Setup {
        integration: integration_management::IntegrationId,
        /// Accept the proposed installation without prompting
        #[arg(long)]
        yes: bool,
        /// Allow replacement of a modified integration asset
        #[arg(long)]
        force: bool,
        #[arg(long)]
        node: Option<String>,
    },
    /// Verify authoritative lifecycle reporting in a running host shell
    Verify {
        integration: integration_management::IntegrationId,
        #[arg(long, value_name = "ID")]
        shell: Option<String>,
        #[arg(long, default_value_t = 30_000)]
        wait_ms: u32,
        #[arg(long)]
        node: Option<String>,
    },
}

#[derive(Subcommand)]
enum DaemonCommands {
    /// Start the daemon in the foreground
    #[command(hide = true)]
    Run,
    /// Receive daemon ownership from a running Boomux process
    #[command(hide = true)]
    ReceiveHandoff {
        #[arg(long)]
        channel: i32,
    },
    /// Report whether the daemon is accepting requests
    Status,
    /// Replace the daemon without changing pending workspace state
    Restart,
    /// Stop the daemon and its managed shells
    Stop,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum OutputMode {
    HumanOnly,
    Json,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct CommandDescriptor {
    key: &'static str,
    output: OutputMode,
}

macro_rules! command_keys {
    ($($variant:ident => ($key:literal, $output:ident)),+ $(,)?) => {
        #[derive(Clone, Copy, Debug, PartialEq, Eq)]
        enum CommandKey {
            $($variant),+
        }

        impl CommandKey {
            const ALL: &[Self] = &[$(Self::$variant),+];

            const fn descriptor(self) -> CommandDescriptor {
                match self {
                    $(Self::$variant => CommandDescriptor {
                        key: $key,
                        output: OutputMode::$output,
                    }),+
                }
            }
        }
    };
}

command_keys! {
    Ui => ("ui", HumanOnly),
    Doctor => ("doctor", HumanOnly),
    Capabilities => ("capabilities", Json),
    List => ("list", Json),
    Shells => ("shells", Json),
    Read => ("read", Json),
    Events => ("events", Json),
    Close => ("close", HumanOnly),
    ProjectList => ("project.list", Json),
    WorkspaceList => ("workspace.list", Json),
    WorkspaceInspect => ("workspace.inspect", Json),
    Workspace => ("workspace", HumanOnly),
    NodeAdd => ("node.add", Json),
    NodeList => ("node.list", Json),
    NodeInspect => ("node.inspect", Json),
    NodeSnapshot => ("node.snapshot", Json),
    NodeRename => ("node.rename", Json),
    NodeRetarget => ("node.retarget", Json),
    NodeForget => ("node.forget", Json),
    NodeRekey => ("node.rekey", HumanOnly),
    ShellSuggestName => ("shell.suggest-name", Json),
    ShellInspect => ("shell.inspect", Json),
    Shell => ("shell", HumanOnly),
    LauncherList => ("launcher.list", Json),
    LauncherInspect => ("launcher.inspect", Json),
    Launcher => ("launcher", HumanOnly),
    AgentList => ("agent.list", Json),
    AgentInspect => ("agent.inspect", Json),
    AgentRegister => ("agent.register", Json),
    AgentEnsure => ("agent.ensure", Json),
    AgentSupervise => ("agent.supervise", HumanOnly),
    AgentReport => ("agent.report", Json),
    AgentWait => ("agent.wait", Json),
    AttentionList => ("attention.list", Json),
    AttentionAcknowledge => ("attention.acknowledge", Json),
    NotificationTest => ("notification.test", HumanOnly),
    IntegrationList => ("integration.list", Json),
    IntegrationStatus => ("integration.status", Json),
    IntegrationInstall => ("integration.install", Json),
    IntegrationUninstall => ("integration.uninstall", Json),
    IntegrationSetup => ("integration.setup", HumanOnly),
    IntegrationVerify => ("integration.verify", Json),
    SessionList => ("session.list", Json),
    SessionInspect => ("session.inspect", Json),
    SessionResume => ("session.resume", HumanOnly),
    ScheduleCreate => ("schedule.create", Json),
    ScheduleList => ("schedule.list", Json),
    ScheduleInspect => ("schedule.inspect", Json),
    SchedulePause => ("schedule.pause", Json),
    ScheduleResume => ("schedule.resume", Json),
    ScheduleRemove => ("schedule.remove", Json),
    ScheduleRun => ("schedule.run", Json),
    ExecutionList => ("execution.list", Json),
    ExecutionInspect => ("execution.inspect", Json),
    ExecutionWait => ("execution.wait", Json),
    ExecutionOpen => ("execution.open", Json),
    ExecutionCancel => ("execution.cancel", Json),
    Skill => ("skill", HumanOnly),
    Opencode => ("opencode", HumanOnly),
    Pi => ("pi", HumanOnly),
    Open => ("open", HumanOnly),
    Prompt => ("prompt", HumanOnly),
    DaemonStatus => ("daemon.status", Json),
    Daemon => ("daemon", HumanOnly),
    Attach => ("attach", HumanOnly),
    ResumeSessionInternal => ("resume-session-internal", HumanOnly),
}

impl Cli {
    fn command_descriptor(&self) -> CommandDescriptor {
        self.command_key().descriptor()
    }

    fn command_key(&self) -> CommandKey {
        match self.command.as_ref() {
            Some(Commands::Capabilities) => CommandKey::Capabilities,
            Some(Commands::List) => CommandKey::List,
            Some(Commands::Shells) => CommandKey::Shells,
            Some(Commands::Read { .. }) => CommandKey::Read,
            Some(Commands::Events { .. }) => CommandKey::Events,
            Some(Commands::Project {
                command: ProjectCommands::List { .. },
            }) => CommandKey::ProjectList,
            Some(Commands::Workspace {
                command: WorkspaceCommands::List,
            }) => CommandKey::WorkspaceList,
            Some(Commands::Workspace {
                command: WorkspaceCommands::Inspect { .. },
            }) => CommandKey::WorkspaceInspect,
            Some(Commands::Node {
                command: NodeCommands::Add { .. },
            }) => CommandKey::NodeAdd,
            Some(Commands::Node {
                command: NodeCommands::List,
            }) => CommandKey::NodeList,
            Some(Commands::Node {
                command: NodeCommands::Inspect { .. },
            }) => CommandKey::NodeInspect,
            Some(Commands::Node {
                command: NodeCommands::Snapshot { .. },
            }) => CommandKey::NodeSnapshot,
            Some(Commands::Node {
                command: NodeCommands::Rename { .. },
            }) => CommandKey::NodeRename,
            Some(Commands::Node {
                command: NodeCommands::Retarget { .. },
            }) => CommandKey::NodeRetarget,
            Some(Commands::Node {
                command: NodeCommands::Forget { .. },
            }) => CommandKey::NodeForget,
            Some(Commands::Node {
                command: NodeCommands::Rekey,
            }) => CommandKey::NodeRekey,
            Some(Commands::Shell {
                command: ShellCommands::SuggestName { .. },
            }) => CommandKey::ShellSuggestName,
            Some(Commands::Shell {
                command: ShellCommands::Inspect { .. },
            }) => CommandKey::ShellInspect,
            Some(Commands::Launcher {
                command: LauncherCommands::List { .. },
            }) => CommandKey::LauncherList,
            Some(Commands::Launcher {
                command: LauncherCommands::Inspect { .. },
            }) => CommandKey::LauncherInspect,
            Some(Commands::Agent {
                command: AgentCommands::List { .. },
            }) => CommandKey::AgentList,
            Some(Commands::Agent {
                command: AgentCommands::Inspect { .. },
            }) => CommandKey::AgentInspect,
            Some(Commands::Agent {
                command: AgentCommands::Wait { .. },
            }) => CommandKey::AgentWait,
            Some(Commands::Agent {
                command: AgentCommands::Register(..),
            }) => CommandKey::AgentRegister,
            Some(Commands::Agent {
                command: AgentCommands::Ensure(..),
            }) => CommandKey::AgentEnsure,
            Some(Commands::Agent {
                command: AgentCommands::Supervise(..),
            }) => CommandKey::AgentSupervise,
            Some(Commands::Agent {
                command: AgentCommands::Report { .. },
            }) => CommandKey::AgentReport,
            Some(Commands::Attention {
                command: AttentionCommands::List { .. },
            }) => CommandKey::AttentionList,
            Some(Commands::Attention {
                command: AttentionCommands::Acknowledge { .. },
            }) => CommandKey::AttentionAcknowledge,
            Some(Commands::Notification {
                command: NotificationCommands::Test { .. },
            }) => CommandKey::NotificationTest,
            Some(Commands::Session {
                command: SessionCommands::List { .. },
            }) => CommandKey::SessionList,
            Some(Commands::Session {
                command: SessionCommands::Inspect { .. },
            }) => CommandKey::SessionInspect,
            Some(Commands::Session {
                command: SessionCommands::Resume { .. },
            }) => CommandKey::SessionResume,
            Some(Commands::Schedule {
                command: ScheduleCommands::Create(..),
            }) => CommandKey::ScheduleCreate,
            Some(Commands::Schedule {
                command: ScheduleCommands::List { .. },
            }) => CommandKey::ScheduleList,
            Some(Commands::Schedule {
                command: ScheduleCommands::Inspect { .. },
            }) => CommandKey::ScheduleInspect,
            Some(Commands::Schedule {
                command: ScheduleCommands::Pause { .. },
            }) => CommandKey::SchedulePause,
            Some(Commands::Schedule {
                command: ScheduleCommands::Resume { .. },
            }) => CommandKey::ScheduleResume,
            Some(Commands::Schedule {
                command: ScheduleCommands::Remove { .. },
            }) => CommandKey::ScheduleRemove,
            Some(Commands::Schedule {
                command: ScheduleCommands::Run { .. },
            }) => CommandKey::ScheduleRun,
            Some(Commands::Execution {
                command: ExecutionCommands::List { .. },
            }) => CommandKey::ExecutionList,
            Some(Commands::Execution {
                command: ExecutionCommands::Inspect { .. },
            }) => CommandKey::ExecutionInspect,
            Some(Commands::Execution {
                command: ExecutionCommands::Wait { .. },
            }) => CommandKey::ExecutionWait,
            Some(Commands::Execution {
                command: ExecutionCommands::Open { .. },
            }) => CommandKey::ExecutionOpen,
            Some(Commands::Execution {
                command: ExecutionCommands::Cancel { .. },
            }) => CommandKey::ExecutionCancel,
            Some(Commands::Integration {
                command: IntegrationCommands::List,
            }) => CommandKey::IntegrationList,
            Some(Commands::Integration {
                command: IntegrationCommands::Status { .. },
            }) => CommandKey::IntegrationStatus,
            Some(Commands::Integration {
                command: IntegrationCommands::Install { .. },
            }) => CommandKey::IntegrationInstall,
            Some(Commands::Integration {
                command: IntegrationCommands::Uninstall { .. },
            }) => CommandKey::IntegrationUninstall,
            Some(Commands::Integration {
                command: IntegrationCommands::Setup { .. },
            }) => CommandKey::IntegrationSetup,
            Some(Commands::Integration {
                command: IntegrationCommands::Verify { .. },
            }) => CommandKey::IntegrationVerify,
            Some(Commands::Daemon {
                command: DaemonCommands::Status,
            }) => CommandKey::DaemonStatus,
            Some(Commands::Workspace {
                command:
                    WorkspaceCommands::Create { .. }
                    | WorkspaceCommands::Open { .. }
                    | WorkspaceCommands::Rename { .. }
                    | WorkspaceCommands::Close { .. },
            }) => CommandKey::Workspace,
            Some(Commands::Shell {
                command:
                    ShellCommands::Create { .. }
                    | ShellCommands::Rename { .. }
                    | ShellCommands::Close { .. },
            }) => CommandKey::Shell,
            Some(Commands::Launcher {
                command:
                    LauncherCommands::Create { .. }
                    | LauncherCommands::Invoke { .. }
                    | LauncherCommands::Rename { .. }
                    | LauncherCommands::Remove { .. },
            }) => CommandKey::Launcher,
            Some(Commands::Daemon {
                command:
                    DaemonCommands::Run
                    | DaemonCommands::ReceiveHandoff { .. }
                    | DaemonCommands::Restart
                    | DaemonCommands::Stop,
            }) => CommandKey::Daemon,
            Some(Commands::Ui) | None => CommandKey::Ui,
            Some(Commands::Doctor) => CommandKey::Doctor,
            Some(Commands::Close { .. }) => CommandKey::Close,
            Some(Commands::Skill {
                command: SkillCommands::Install { .. },
            }) => CommandKey::Skill,
            Some(Commands::Opencode {
                command: OpenCodeCommands::Install { .. },
            }) => CommandKey::Opencode,
            Some(Commands::Pi {
                command: PiCommands::Install { .. },
            }) => CommandKey::Pi,
            Some(Commands::Open { .. }) => CommandKey::Open,
            Some(Commands::Prompt) => CommandKey::Prompt,
            Some(Commands::Attach { .. }) => CommandKey::Attach,
            Some(Commands::ResumeSession { .. }) => CommandKey::ResumeSessionInternal,
            Some(Commands::ScheduledRunner { .. }) => CommandKey::Attach,
            Some(Commands::FederationStdio) => CommandKey::Attach,
        }
    }
}

fn json_commands() -> impl Iterator<Item = &'static str> {
    CommandKey::ALL.iter().filter_map(|key| {
        let descriptor = key.descriptor();
        (descriptor.output == OutputMode::Json).then_some(descriptor.key)
    })
}

fn print_json(command: CommandKey, data: serde_json::Value) -> Result<(), Box<dyn Error>> {
    cli_output::print(command.descriptor().key, data)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CliExit {
    Success,
    Child(process_adapter::ProcessExit),
}

impl CliExit {
    fn code(self) -> ExitCode {
        match self {
            Self::Success | Self::Child(process_adapter::ProcessExit::Code(0)) => ExitCode::SUCCESS,
            Self::Child(process_adapter::ProcessExit::Code(code)) => {
                ExitCode::from(u8::try_from(code).unwrap_or(1))
            }
            Self::Child(process_adapter::ProcessExit::Signal(signal)) => {
                ExitCode::from(u8::try_from(128_i32.saturating_add(signal)).unwrap_or(1))
            }
        }
    }
}

fn main() -> ExitCode {
    let json_requested = requests_json(env::args_os().skip(1));
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(error) => {
            let exit_code = u8::try_from(error.exit_code()).unwrap_or(1);
            if json_requested
                && !matches!(
                    error.kind(),
                    ClapErrorKind::DisplayHelp | ClapErrorKind::DisplayVersion
                )
            {
                cli_output::print_error_message("cli", "invalid_argument", error.to_string());
                return ExitCode::from(exit_code);
            }
            let success = matches!(
                error.kind(),
                ClapErrorKind::DisplayHelp | ClapErrorKind::DisplayVersion
            );
            let _ = error.print();
            return if success {
                ExitCode::SUCCESS
            } else {
                ExitCode::from(exit_code)
            };
        }
    };
    let json = cli.json;
    let command = cli.command_descriptor().key;
    match run(cli) {
        Ok(outcome) => outcome.code(),
        Err(error) => {
            if json {
                cli_output::print_error(command, error.as_ref());
            } else {
                eprintln!("boomux: {error}");
            }
            ExitCode::FAILURE
        }
    }
}

fn requests_json(arguments: impl IntoIterator<Item = OsString>) -> bool {
    arguments
        .into_iter()
        .take_while(|argument| argument != "--")
        .any(|argument| argument == "--json")
}

fn run(cli: Cli) -> Result<CliExit, Box<dyn Error>> {
    let descriptor = cli.command_descriptor();
    if cli.json && descriptor.output != OutputMode::Json {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("--json is not supported for {}", descriptor.key),
        )
        .into());
    }
    if let Some(target) = cli.remote.as_deref() {
        if cli.json
            || cli.path.is_some()
            || cli.command.is_some()
            || cli.name.is_some()
            || cli.new_window
            || !cli.startup_command.is_empty()
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "--remote currently accepts only an SSH target; remote management and dashboard routing are delivered separately",
            )
            .into());
        }
        remote_connect(target, cli.terminal.as_deref())?;
        return Ok(CliExit::Success);
    }
    match cli.command.as_ref() {
        Some(Commands::Daemon {
            command: DaemonCommands::Run,
        }) => {
            daemon::run_with_notification_delivery(config::load_notification_settings()?)?;
            return Ok(CliExit::Success);
        }
        Some(Commands::Daemon {
            command: DaemonCommands::ReceiveHandoff { channel },
        }) => {
            daemon::receive_handoff_with_notification_delivery(
                *channel,
                config::load_notification_settings()?,
            )?;
            return Ok(CliExit::Success);
        }
        Some(Commands::Attach {
            shell_id,
            node,
            takeover,
            restart_exited,
            expected_run_id,
        }) => {
            attach::run(
                shell_id,
                node.as_deref(),
                *takeover,
                *restart_exited,
                expected_run_id.as_deref(),
            )?;
            return Ok(CliExit::Success);
        }
        Some(Commands::ScheduledRunner { schedule_id }) => {
            return scheduled_runner(schedule_id).map(CliExit::Child);
        }
        Some(Commands::ResumeSession { session_id, node }) => {
            attach::run_agent_session(session_id, node.as_deref())?;
            return Ok(CliExit::Success);
        }
        Some(Commands::FederationStdio) => {
            federation::run_stdio_helper()?;
            return Ok(CliExit::Success);
        }
        _ => {}
    }

    if let Some(desktop_entry) = cli.terminal.as_deref() {
        terminal::validate_desktop_entry(desktop_entry)?;
    }
    if let Some(path) = cli.path {
        let new_window = should_open_new_window(cli.new_window, cli.terminal.as_deref());
        let terminal = new_window
            .then(|| effective_terminal(cli.terminal.as_deref()))
            .transpose()?
            .flatten();
        open_directory(
            &path,
            cli.name.as_deref(),
            &cli.startup_command,
            new_window,
            terminal.as_deref(),
        )?;
        return Ok(CliExit::Success);
    }

    let result = match cli.command {
        Some(Commands::Ui) => dashboard(cli.terminal.as_deref(), None),
        Some(Commands::Doctor) => doctor(cli.terminal.as_deref()),
        Some(Commands::Capabilities) => capabilities(cli.json),
        Some(Commands::List) => list_shells(cli.json),
        Some(Commands::Shells) => list_workspace_shells(cli.json),
        Some(Commands::Read {
            target,
            lines,
            run_id,
            after_revision,
            wait_ms,
        }) => read_shell(
            &target,
            lines,
            cli.json,
            run_id.as_deref(),
            after_revision,
            wait_ms,
        ),
        Some(Commands::Events {
            after,
            limit,
            wait_ms,
        }) => read_events(after.as_deref(), limit, wait_ms, cli.json),
        Some(Commands::Close { target }) => close_shell(&target),
        Some(Commands::Project {
            command: ProjectCommands::List { node },
        }) => list_projects(cli.json, node.as_deref()),
        Some(Commands::Workspace { command }) => {
            workspace_command(command, cli.json, cli.terminal.as_deref())
        }
        Some(Commands::Node { command }) => node_command(command, cli.json),
        Some(Commands::Shell { command }) => shell_command(command, cli.json),
        Some(Commands::Launcher { command }) => launcher_command(command, cli.json),
        Some(Commands::Agent {
            command: AgentCommands::Supervise(arguments),
        }) => return supervise_agent(arguments).map(CliExit::Child),
        Some(Commands::Agent { command }) => agent_command(command, cli.json),
        Some(Commands::Attention { command }) => attention_command(command, cli.json),
        Some(Commands::Notification {
            command: NotificationCommands::Test { reason },
        }) => test_notification(reason),
        Some(Commands::Session { command }) => session_command(command, cli.json),
        Some(Commands::Schedule { command }) => schedule_command(command, cli.json),
        Some(Commands::Execution { command }) => {
            execution_command(command, cli.json, cli.terminal.as_deref())
        }
        Some(Commands::Integration { command }) => integration_command(command, cli.json),
        Some(Commands::Skill {
            command: SkillCommands::Install { force },
        }) => install_skill(force),
        Some(Commands::Opencode {
            command: OpenCodeCommands::Install { force },
        }) => install_opencode(force),
        Some(Commands::Pi {
            command: PiCommands::Install { force },
        }) => install_pi(force),
        Some(Commands::Open {
            shell_id,
            node,
            title,
            takeover,
        }) => {
            let terminal = effective_terminal(cli.terminal.as_deref())?;
            open_shell(
                &shell_id,
                node.as_deref(),
                title.as_deref(),
                takeover,
                terminal.as_deref(),
            )
        }
        Some(Commands::Prompt) => print_prompt_label(),
        Some(Commands::Daemon { command }) => daemon_control(command, cli.json),
        Some(Commands::Attach { .. }) => unreachable!(),
        Some(Commands::ResumeSession { .. }) => unreachable!(),
        Some(Commands::ScheduledRunner { .. }) => unreachable!(),
        Some(Commands::FederationStdio) => unreachable!(),
        None => dashboard(cli.terminal.as_deref(), None),
    };
    result?;
    Ok(CliExit::Success)
}

fn remote_connect(target: &str, terminal: Option<&str>) -> Result<(), Box<dyn Error>> {
    let mut connection = verified_remote_connection(target, true)?;
    connection.ping()?;
    println!(
        "Connected to Boomux Node {} (protocol {}, helper {}, {})",
        connection.handshake.node_id,
        connection.handshake.core_protocol_version,
        connection.handshake.helper_version,
        connection.executable.as_str(),
    );
    let node_id = connection.handshake.node_id.clone();
    drop(connection);
    if let Ok(local) = client::connect_or_start()
        && local.node_registrations().is_ok_and(|registrations| {
            registrations
                .iter()
                .any(|registration| registration.node_id == node_id)
        })
        && local
            .supports(protocol::ProtocolFeature::CombinedNodeSnapshot)
            .unwrap_or(false)
    {
        dashboard(terminal, Some(node_id))?;
    }
    Ok(())
}

fn verified_remote_connection(
    target: &str,
    allow_interactive_install: bool,
) -> Result<ssh_bootstrap::RemoteConnection, Box<dyn Error>> {
    const TIMEOUT: Duration = Duration::from_secs(120);

    let target = ssh_bootstrap::SshTarget::parse(target)?;
    let interactive =
        allow_interactive_install && io::stdin().is_terminal() && io::stdout().is_terminal();
    let authentication = if interactive {
        ssh_bootstrap::SshAuthenticationMode::Interactive
    } else {
        ssh_bootstrap::SshAuthenticationMode::Batch
    };
    let helper = match ssh_bootstrap::plan_remote_bootstrap(
        target.clone(),
        authentication,
        TIMEOUT,
    )? {
        ssh_bootstrap::RemoteBootstrapPlan::Ready(helper) => helper,
        ssh_bootstrap::RemoteBootstrapPlan::Install(_plan) if !interactive => {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "remote Boomux installation is required, but noninteractive --remote never modifies remote software",
            )
            .into());
        }
        ssh_bootstrap::RemoteBootstrapPlan::Install(plan) => {
            println!("Remote target: {}", plan.target.as_str());
            println!("Install source: {}", plan.source.description());
            println!("Install destination: {}", plan.destination.as_str());
            println!(
                "Process impact: {}",
                if plan.may_restart_daemon {
                    "the daemon may be gracefully restarted only if the installed helper cannot connect to the running daemon"
                } else {
                    "a detached daemon may be started; no running daemon will be restarted for a release-version difference"
                }
            );
            if !confirm_setup("Install Boomux on this remote target?")? {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "remote Boomux installation was not authorized",
                )
                .into());
            }
            ssh_bootstrap::install_remote(&plan, authentication, TIMEOUT)?
        }
    };
    Ok(ssh_bootstrap::connect_remote(
        target,
        helper,
        authentication,
        TIMEOUT,
    )?)
}

fn node_command(command: NodeCommands, json: bool) -> Result<(), Box<dyn Error>> {
    match command {
        NodeCommands::Add { alias, target } => {
            let mut remote = verified_remote_connection(&target, !json)?;
            remote.ping()?;
            let registration = client::connect_or_start()?.add_node_registration(
                alias,
                target,
                remote.handshake.node_id.clone(),
            )?;
            print_node_registration(CommandKey::NodeAdd, &registration, json)
        }
        NodeCommands::List => {
            let client = client::connect_or_start()?;
            let registrations = client.node_registrations()?;
            let projection_supported =
                client.supports(protocol::ProtocolFeature::NodeProjectionSync)?;
            if json {
                let rows = registrations
                    .into_iter()
                    .map(|registration| {
                        let mut value = serde_json::to_value(registration)?;
                        if projection_supported {
                            let health = client.node_projection_health(
                                value["node_id"].as_str().expect("Node ID is a string"),
                            )?;
                            value
                                .as_object_mut()
                                .expect("registration serializes as an object")
                                .insert("projection".into(), serde_json::to_value(health)?);
                        }
                        Ok::<_, Box<dyn Error>>(value)
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                print_json(CommandKey::NodeList, serde_json::to_value(rows)?)
            } else {
                println!("ALIAS\tTARGET\tNODE ID\tREVISION\tPROJECTION");
                for registration in registrations {
                    let health = projection_supported
                        .then(|| client.node_projection_health(&registration.node_id))
                        .transpose()?;
                    println!(
                        "{}\t{}\t{}\t{}\t{}",
                        registration.alias,
                        registration.target,
                        registration.node_id,
                        registration.revision,
                        health
                            .map(|health| format!("{:?}", health.code).to_ascii_lowercase())
                            .unwrap_or_else(|| "unsupported".into()),
                    );
                }
                Ok(())
            }
        }
        NodeCommands::Inspect { selector } => {
            let client = client::connect_or_start()?;
            let registration = client.node_registration(selector)?;
            let health = client
                .supports(protocol::ProtocolFeature::NodeProjectionSync)?
                .then(|| client.node_projection_health(&registration.node_id))
                .transpose()?;
            if json {
                let mut value = serde_json::to_value(&registration)?;
                if let Some(health) = health {
                    value
                        .as_object_mut()
                        .expect("registration serializes as an object")
                        .insert("projection".into(), serde_json::to_value(health)?);
                }
                print_json(CommandKey::NodeInspect, value)
            } else {
                print_node_registration(CommandKey::NodeInspect, &registration, false)?;
                if let Some(health) = health {
                    println!(
                        "Projection: {}",
                        format!("{:?}", health.code).to_ascii_lowercase()
                    );
                    println!("Projection stale: {}", health.stale);
                    println!("Cache generation: {}", health.cache_generation);
                    if let (Some(stream_id), Some(cursor)) = (health.stream_id, health.cursor) {
                        println!("Remote cursor: {stream_id}:{cursor}");
                    }
                } else {
                    println!("Projection: unsupported");
                }
                Ok(())
            }
        }
        NodeCommands::Snapshot { selector } => {
            let snapshot = client::connect_or_start()?.combined_node_snapshot(selector)?;
            if json {
                let nodes = snapshot
                    .nodes
                    .into_iter()
                    .map(node_snapshot_json)
                    .collect::<Result<Vec<_>, _>>()?;
                print_json(
                    CommandKey::NodeSnapshot,
                    serde_json::json!({ "nodes": nodes }),
                )
            } else {
                println!("ALIAS\tOWNERSHIP\tHEALTH\tCURRENT\tOBSERVED\tWORKSPACES\tSCHEDULER");
                for node in snapshot.nodes {
                    let workspace_count = node
                        .local_snapshot
                        .as_ref()
                        .map(|snapshot| snapshot.workspaces.len())
                        .or_else(|| {
                            node.remote_projection
                                .as_ref()
                                .map(|projection| projection.workspaces.len())
                        })
                        .unwrap_or(0);
                    println!(
                        "{}\t{}\t{}\t{}\t{}\t{}\t{} {}/{}",
                        node.alias,
                        if node.local { "local" } else { "remote" },
                        format!("{:?}", node.health).to_ascii_lowercase(),
                        node.current,
                        node.observed_at_ms,
                        workspace_count,
                        format!("{:?}", node.scheduler.state).to_ascii_lowercase(),
                        node.scheduler.active_executions,
                        node.scheduler.max_concurrent,
                    );
                }
                Ok(())
            }
        }
        NodeCommands::Rename {
            selector,
            alias,
            revision,
        } => {
            let registration =
                client::connect_or_start()?.rename_node_registration(selector, alias, revision)?;
            print_node_registration(CommandKey::NodeRename, &registration, json)
        }
        NodeCommands::Retarget {
            selector,
            target,
            revision,
        } => {
            let local = client::connect_or_start()?;
            let current = local.node_registration(&selector)?;
            if current.revision != revision {
                return Err(cli_output::failure(
                    "revision_changed",
                    format!(
                        "Node registration revision changed: expected {revision}, current {}",
                        current.revision
                    ),
                ));
            }
            let mut remote = verified_remote_connection(&target, !json)?;
            remote.ping()?;
            let registration = local.retarget_node_registration(
                selector,
                target,
                remote.handshake.node_id.clone(),
                revision,
            )?;
            print_node_registration(CommandKey::NodeRetarget, &registration, json)
        }
        NodeCommands::Forget { selector } => {
            let registration = client::connect_or_start()?.forget_node_registration(selector)?;
            print_node_registration(CommandKey::NodeForget, &registration, json)
        }
        NodeCommands::Rekey => rekey_node(),
    }
}

fn node_snapshot_json(node: protocol::CombinedNode) -> Result<serde_json::Value, Box<dyn Error>> {
    let protocol::CombinedNode {
        node_id,
        alias,
        local,
        health,
        current,
        stale,
        observed_at_ms,
        observed_protocol_version,
        observed_capabilities,
        scheduler,
        local_snapshot,
        remote_projection,
    } = node;
    let qualify = |value| qualify_resource_identities(value, &node_id);
    Ok(serde_json::json!({
        "node_id": node_id,
        "alias": alias,
        "local": local,
        "health": health,
        "current": current,
        "stale": stale,
        "observed_at_ms": observed_at_ms,
        "observed_protocol_version": observed_protocol_version,
        "observed_capabilities": observed_capabilities,
        "scheduler": scheduler,
        "local_snapshot": local_snapshot.map(serde_json::to_value).transpose()?.map(qualify),
        "remote_projection": remote_projection.map(serde_json::to_value).transpose()?.map(qualify),
    }))
}

fn qualify_resource_identities(mut value: serde_json::Value, node_id: &str) -> serde_json::Value {
    const ID_FIELDS: &[&str] = &[
        "id",
        "workspace_id",
        "shell_id",
        "run_id",
        "launcher_id",
        "agent_id",
        "schedule_id",
        "execution_id",
        "owner_schedule_id",
    ];
    match &mut value {
        serde_json::Value::Array(values) => {
            for value in values {
                *value = qualify_resource_identities(value.take(), node_id);
            }
        }
        serde_json::Value::Object(object) => {
            for (key, value) in object {
                if ID_FIELDS.contains(&key.as_str()) {
                    if let Some(inner_id) = value.as_str() {
                        *value = serde_json::to_value(protocol::QualifiedIdentity::new(
                            node_id, inner_id,
                        ))
                        .expect("qualified identity serializes");
                    }
                } else {
                    *value = qualify_resource_identities(value.take(), node_id);
                }
            }
        }
        _ => {}
    }
    value
}

fn print_node_registration(
    command: CommandKey,
    registration: &protocol::NodeRegistrationSnapshot,
    json: bool,
) -> Result<(), Box<dyn Error>> {
    if json {
        print_json(command, serde_json::to_value(registration)?)
    } else {
        println!("Alias: {}", registration.alias);
        println!("Target: {}", registration.target);
        println!("Node ID: {}", registration.node_id);
        println!("Revision: {}", registration.revision);
        println!("Tombstone epoch: {}", registration.tombstone_epoch);
        Ok(())
    }
}

fn rekey_node() -> Result<(), Box<dyn Error>> {
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "node rekey requires an interactive terminal",
        )
        .into());
    }
    let client = client::connect()?;
    let current = client.node_identity()?;
    println!("Current Node ID: {current}");
    println!("Rekey changes the federated identity of every resource owned by this Node.");
    print!("Type the current Node ID to continue: ");
    io::Write::flush(&mut io::stdout())?;
    let mut confirmation = String::new();
    io::stdin().read_line(&mut confirmation)?;
    validate_rekey_confirmation(&current, &confirmation)?;
    let replacement = client.rekey_node(&current)?;
    println!("Rekeyed Boomux Node: {current} -> {replacement}");
    println!("Existing remote registrations must forget and add this Node again.");
    Ok(())
}

fn validate_rekey_confirmation(expected: &str, confirmation: &str) -> io::Result<()> {
    if confirmation.trim_end_matches(['\r', '\n']) == expected {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Node rekey confirmation did not match the current Node ID",
        ))
    }
}

fn daemon_control(command: DaemonCommands, json: bool) -> Result<(), Box<dyn Error>> {
    let client = client::connect()?;
    match command {
        DaemonCommands::Status if json => {
            let protocol_version = client.protocol_version()?;
            let scheduler = client.snapshot()?.scheduler;
            print_json(
                CommandKey::DaemonStatus,
                serde_json::json!({
                    "status": "running",
                    "protocol_version": protocol_version,
                    "socket_path": client.socket_path().display().to_string(),
                    "scheduler": scheduler.map(|health| serde_json::json!({
                        "state": match health.state {
                            protocol::SchedulerState::Active => "active",
                            protocol::SchedulerState::Offline => "offline",
                        },
                        "max_concurrent": health.max_concurrent,
                        "active_executions": health.active_executions,
                    })),
                }),
            )?
        }
        DaemonCommands::Status => {
            let scheduler = client.snapshot()?.scheduler;
            println!(
                "running (protocol {}, {})",
                client.protocol_version()?,
                client.socket_path().display()
            );
            if let Some(scheduler) = scheduler {
                println!(
                    "scheduler {} ({}/{} active executions)",
                    match scheduler.state {
                        protocol::SchedulerState::Active => "active",
                        protocol::SchedulerState::Offline => "offline",
                    },
                    scheduler.active_executions,
                    scheduler.max_concurrent
                );
            }
        }
        DaemonCommands::Restart => {
            client
                .restart_with_notification_config(config::load_notification_settings()?.into())?;
            println!("Restarted Boomux daemon");
        }
        DaemonCommands::Stop => {
            client.shutdown()?;
            println!("Stopped Boomux daemon");
        }
        DaemonCommands::Run | DaemonCommands::ReceiveHandoff { .. } => unreachable!(),
    }
    Ok(())
}

fn should_open_new_window(new_window: bool, terminal: Option<&str>) -> bool {
    new_window || terminal.is_some()
}

fn effective_terminal(override_entry: Option<&str>) -> Result<Option<String>, Box<dyn Error>> {
    if let Some(entry) = override_entry {
        return Ok(Some(entry.to_owned()));
    }
    Ok(config::load()?.terminal)
}

fn dashboard(
    terminal_override: Option<&str>,
    initial_node_filter: Option<String>,
) -> Result<(), Box<dyn Error>> {
    ensure_host_terminal()?;
    let launch_cwd = resolve_directory(Path::new("."))?;
    let client = client::connect_or_start()?;
    let local_node_id = client.node_identity()?;
    let mut git_cache = git::Cache::default();
    let mut title_cache = host_session_titles::Cache::default();
    let mut refresh = DashboardRefresh::baseline(&client)?;
    let snapshot = refresh.snapshot().clone();
    let combined = combined_node_snapshot_for_dashboard(&client)?;
    let config = config::load()?;
    let terminal = terminal_override
        .map(str::to_owned)
        .or_else(|| config.terminal.clone());
    let roots_configured = !config.projects.roots.is_empty();
    let discovery = projects::discover(&config.projects);
    let project_context = tui::ProjectContext {
        projects: discovery
            .projects
            .into_iter()
            .map(|project| tui::ProjectView {
                name: project.name,
                path: project.path,
                group: project.group,
                group_order: project.group_order,
            })
            .collect(),
        config_path: config.path.or_else(config::global_config_path),
        warning: (!discovery.warnings.is_empty()).then(|| discovery.warnings.join("; ")),
        roots_configured,
    };

    let mut initial_state = dashboard_state(
        snapshot,
        combined,
        &refresh,
        &mut git_cache,
        &mut title_cache,
        false,
    );
    initial_state.initial_node_filter = initial_node_filter;
    tui::run(
        initial_state,
        config.dashboard.follow_focused_terminal,
        project_context,
        true,
        |effect| match effect {
            tui::DashboardEffect::Quit => unreachable!("quit is handled by the dashboard runtime"),
            tui::DashboardEffect::CheckForUpdates => {
                let result = refresh.check(&client).map_err(|error| error.to_string());
                match result {
                    Ok(Some((snapshot, reset_focus_revision))) => {
                        match combined_node_snapshot_for_dashboard(&client) {
                            Ok(combined) => {
                                tui::DashboardEvent::RefreshCompleted(Ok(dashboard_state(
                                    snapshot,
                                    combined,
                                    &refresh,
                                    &mut git_cache,
                                    &mut title_cache,
                                    reset_focus_revision,
                                )))
                            }
                            Err(error) => {
                                tui::DashboardEvent::RefreshCompleted(Err(error.to_string()))
                            }
                        }
                    }
                    Ok(None) => tui::DashboardEvent::UpdateCheckCompleted,
                    Err(error) => tui::DashboardEvent::RefreshCompleted(Err(error)),
                }
            }
            tui::DashboardEffect::RestoreWorkspace(workspace_id) => {
                let result = (|| {
                    let workspace_id = local_dashboard_inner(&workspace_id, &local_node_id)?;
                    let workspace = client
                        .get_workspace(workspace_id)
                        .map_err(|error| error.to_string())?;
                    open_workspace(&workspace, terminal.as_deref())
                        .map_err(|error| error.to_string())?;
                    Ok(format!(
                        "Opened {} launcher(s) and {} shell(s) for {}",
                        workspace.launchers.len(),
                        workspace_user_shell_count(&workspace),
                        workspace.name
                    ))
                })();
                tui::DashboardEvent::OperationCompleted(result)
            }
            tui::DashboardEffect::Open(target) => {
                let result = dispatch_dashboard_open(
                    &target,
                    &local_node_id,
                    |shell_id| {
                        if shell_id.node_id == local_node_id || shell_id.node_id.is_empty() {
                            open_dashboard_shell(&client, &shell_id.inner_id, terminal.as_deref())
                                .map_err(|error| error.to_string())
                        } else {
                            open_dashboard_remote_shell(&client, shell_id, terminal.as_deref())
                                .map_err(|error| error.to_string())
                        }
                    },
                    |workspace_id, launcher_id| {
                        let workspace =
                            routed_dashboard_workspace(&client, workspace_id, &local_node_id)?;
                        let launcher =
                            routed_dashboard_launcher(&client, launcher_id, &local_node_id)?;
                        if launcher.workspace_id != workspace.id {
                            return Err(
                                "launcher ownership changed; refresh before invoking".into()
                            );
                        }
                        if workspace_id.node_id == local_node_id || workspace_id.node_id.is_empty()
                        {
                            invoke_workspace_launcher(&workspace, &launcher)
                                .map_err(|error| error.to_string())?;
                        } else {
                            client
                                .route_node_host_service(
                                    &workspace_id.node_id,
                                    protocol::HostServiceOperation::InvokeLauncher {
                                        workspace_id: workspace.id.clone(),
                                        launcher_id: launcher.id.clone(),
                                    },
                                )
                                .map_err(|error| error.to_string())?;
                        }
                        Ok(format!(
                            "Launched {} from {}",
                            launcher.name, workspace.name
                        ))
                    },
                );
                tui::DashboardEvent::OperationCompleted(result)
            }
            tui::DashboardEffect::Close(target) => {
                let result = (|| match &target {
                    tui::CloseTarget::Workspace(workspace_id) => {
                        let workspace =
                            routed_dashboard_workspace(&client, workspace_id, &local_node_id)?;
                        let name = workspace.name.clone();
                        if workspace_id.node_id == local_node_id || workspace_id.node_id.is_empty()
                        {
                            client
                                .close_workspace(&workspace_id.inner_id)
                                .map_err(|error| error.to_string())?;
                        } else {
                            routed_dashboard_operation(
                                &client,
                                workspace_id,
                                &local_node_id,
                                protocol::RoutedOperation::CloseWorkspace {
                                    workspace_id: workspace_id.inner_id.clone(),
                                    expected_revision: workspace.revision,
                                },
                            )?;
                        }
                        Ok(format!(
                            "Closed {name}, its launchers, shells, schedules, and persisted prompts"
                        ))
                    }
                    tui::CloseTarget::Shell(shell_id) => {
                        let shell = routed_dashboard_shell(&client, shell_id, &local_node_id)?;
                        let name = shell.name.clone();
                        if shell_id.node_id == local_node_id || shell_id.node_id.is_empty() {
                            client
                                .close_shell(&shell_id.inner_id)
                                .map_err(|error| error.to_string())?;
                        } else {
                            routed_dashboard_operation(
                                &client,
                                shell_id,
                                &local_node_id,
                                protocol::RoutedOperation::CloseShell {
                                    shell_id: shell_id.inner_id.clone(),
                                    expected_revision: shell.revision,
                                },
                            )?;
                        }
                        Ok(format!("Closed shell {name}"))
                    }
                    tui::CloseTarget::Launcher(launcher_id) => {
                        let launcher =
                            routed_dashboard_launcher(&client, launcher_id, &local_node_id)?;
                        let name = launcher.name.clone();
                        if launcher_id.node_id == local_node_id || launcher_id.node_id.is_empty() {
                            client
                                .remove_launcher(&launcher_id.inner_id)
                                .map_err(|error| error.to_string())?;
                        } else {
                            routed_dashboard_operation(
                                &client,
                                launcher_id,
                                &local_node_id,
                                protocol::RoutedOperation::RemoveLauncher {
                                    launcher_id: launcher_id.inner_id.clone(),
                                    expected_revision: launcher.revision,
                                },
                            )?;
                        }
                        Ok(format!("Removed launcher {name}"))
                    }
                    tui::CloseTarget::Schedule(_) => {
                        unreachable!("schedule removal has a typed effect")
                    }
                    tui::CloseTarget::Execution(_) => {
                        unreachable!("execution cancellation has a typed effect")
                    }
                })();
                tui::DashboardEvent::OperationCompleted(result)
            }
            tui::DashboardEffect::CreateWorkspace { name, default_cwd } => {
                tui::DashboardEvent::OperationCompleted(
                    create_dashboard_workspace(&client, &name, default_cwd.as_ref())
                        .map_err(|error| error.to_string()),
                )
            }
            tui::DashboardEffect::CreateShell(workspace_id) => {
                tui::DashboardEvent::OperationCompleted(
                    local_dashboard_inner(&workspace_id, &local_node_id).and_then(|workspace_id| {
                        create_dashboard_shell(&client, workspace_id, &launch_cwd)
                            .map_err(|error| error.to_string())
                    }),
                )
            }
            tui::DashboardEffect::Rename { target, name } => {
                let result = (|| match &target {
                    tui::RenameTarget::Workspace(workspace_id) => {
                        let workspace =
                            routed_dashboard_workspace(&client, workspace_id, &local_node_id)?;
                        if workspace_id.node_id == local_node_id || workspace_id.node_id.is_empty()
                        {
                            client
                                .rename_workspace(&workspace_id.inner_id, &name)
                                .map_err(|error| error.to_string())?;
                        } else {
                            routed_dashboard_operation(
                                &client,
                                workspace_id,
                                &local_node_id,
                                protocol::RoutedOperation::RenameWorkspace {
                                    workspace_id: workspace_id.inner_id.clone(),
                                    name: name.clone(),
                                    expected_revision: workspace.revision,
                                },
                            )?;
                        }
                        Ok(format!("Renamed workspace to {name}"))
                    }
                    tui::RenameTarget::Shell(shell_id) => {
                        let shell = routed_dashboard_shell(&client, shell_id, &local_node_id)?;
                        if shell_id.node_id == local_node_id || shell_id.node_id.is_empty() {
                            client
                                .rename_shell(&shell_id.inner_id, &name)
                                .map_err(|error| error.to_string())?;
                        } else {
                            routed_dashboard_operation(
                                &client,
                                shell_id,
                                &local_node_id,
                                protocol::RoutedOperation::RenameShell {
                                    shell_id: shell_id.inner_id.clone(),
                                    name: name.clone(),
                                    expected_revision: shell.revision,
                                },
                            )?;
                        }
                        Ok(format!("Renamed shell to {name}"))
                    }
                    tui::RenameTarget::Launcher(launcher_id) => {
                        let launcher =
                            routed_dashboard_launcher(&client, launcher_id, &local_node_id)?;
                        if launcher_id.node_id == local_node_id || launcher_id.node_id.is_empty() {
                            client
                                .rename_launcher(&launcher_id.inner_id, &name)
                                .map_err(|error| error.to_string())?;
                        } else {
                            routed_dashboard_operation(
                                &client,
                                launcher_id,
                                &local_node_id,
                                protocol::RoutedOperation::RenameLauncher {
                                    launcher_id: launcher_id.inner_id.clone(),
                                    name: name.clone(),
                                    expected_revision: launcher.revision,
                                },
                            )?;
                        }
                        Ok(format!("Renamed launcher to {name}"))
                    }
                })();
                tui::DashboardEvent::OperationCompleted(result)
            }
            tui::DashboardEffect::Refresh => {
                let result = (|| {
                    let (snapshot, reset_focus_revision) = refresh
                        .refresh(&client)
                        .map_err(|error| error.to_string())?;
                    Ok(dashboard_state(
                        snapshot,
                        combined_node_snapshot_for_dashboard(&client)
                            .map_err(|error| error.to_string())?,
                        &refresh,
                        &mut git_cache,
                        &mut title_cache,
                        reset_focus_revision,
                    ))
                })();
                tui::DashboardEvent::RefreshCompleted(result)
            }
            tui::DashboardEffect::RunSchedule(schedule_id) => {
                let result =
                    if schedule_id.node_id == local_node_id || schedule_id.node_id.is_empty() {
                        run_dashboard_schedule(&client, &schedule_id.inner_id)
                    } else {
                        (|| {
                            routed_dashboard_schedule(&client, &schedule_id, &local_node_id)?;
                            let dispatch_key = Uuid::new_v4().to_string();
                            match routed_dashboard_operation(
                                &client,
                                &schedule_id,
                                &local_node_id,
                                protocol::RoutedOperation::RunAgentSchedule {
                                    schedule_id: schedule_id.inner_id.clone(),
                                    dispatch_key,
                                },
                            )? {
                                protocol::RoutedOperationResult::ScheduledExecution {
                                    execution,
                                    ..
                                } => Ok(format!("Started scheduled execution {}", execution.id)),
                                _ => {
                                    Err("remote Node returned an unexpected schedule run response"
                                        .into())
                                }
                            }
                        })()
                    };
                tui::DashboardEvent::OperationCompleted(result)
            }
            tui::DashboardEffect::PauseSchedule(schedule_id) => {
                let result = (|| {
                    let inspection =
                        routed_dashboard_schedule(&client, &schedule_id, &local_node_id)?;
                    let schedule = if schedule_id.node_id == local_node_id
                        || schedule_id.node_id.is_empty()
                    {
                        client
                            .pause_agent_schedule(&schedule_id.inner_id)
                            .map_err(|error| error.to_string())?
                    } else {
                        match routed_dashboard_operation(
                            &client,
                            &schedule_id,
                            &local_node_id,
                            protocol::RoutedOperation::PauseAgentSchedule {
                                schedule_id: schedule_id.inner_id.clone(),
                                expected_revision: inspection.schedule.revision,
                            },
                        )? {
                            protocol::RoutedOperationResult::AgentSchedule { schedule } => schedule,
                            _ => {
                                return Err(
                                    "remote Node returned an unexpected pause response".into()
                                );
                            }
                        }
                    };
                    Ok(format!("Paused schedule {}", schedule.name))
                })();
                tui::DashboardEvent::OperationCompleted(result)
            }
            tui::DashboardEffect::ResumeSchedule(schedule_id) => {
                let result = (|| {
                    let inspection =
                        routed_dashboard_schedule(&client, &schedule_id, &local_node_id)?;
                    let schedule = if schedule_id.node_id == local_node_id
                        || schedule_id.node_id.is_empty()
                    {
                        client
                            .resume_agent_schedule(&schedule_id.inner_id)
                            .map_err(|error| error.to_string())?
                    } else {
                        match routed_dashboard_operation(
                            &client,
                            &schedule_id,
                            &local_node_id,
                            protocol::RoutedOperation::ResumeAgentSchedule {
                                schedule_id: schedule_id.inner_id.clone(),
                                expected_revision: inspection.schedule.revision,
                            },
                        )? {
                            protocol::RoutedOperationResult::AgentSchedule { schedule } => schedule,
                            _ => {
                                return Err(
                                    "remote Node returned an unexpected resume response".into()
                                );
                            }
                        }
                    };
                    Ok(format!(
                        "Enabled schedule {} for future timed dispatch",
                        schedule.name
                    ))
                })();
                tui::DashboardEvent::OperationCompleted(result)
            }
            tui::DashboardEffect::LoadScheduleEditor { schedule_id } => {
                let result = routed_dashboard_schedule(&client, &schedule_id, &local_node_id).map(
                    |inspection| tui::ScheduleEditInspection {
                        schedule_id: schedule_id.clone(),
                        name: inspection.schedule.name,
                        cron: inspection.schedule.trigger.cron,
                        timezone: inspection.schedule.trigger.timezone,
                        prompt: inspection.prompt,
                        revision: inspection.schedule.revision,
                        paused: inspection.schedule.state == AgentScheduleState::Paused,
                    },
                );
                tui::DashboardEvent::ScheduleEditorLoaded {
                    schedule_id,
                    result,
                }
            }
            tui::DashboardEffect::UpdateSchedule {
                schedule_id,
                expected_revision,
                update,
            } => {
                let definition = AgentScheduleUpdate {
                    name: update.name,
                    prompt: update.prompt,
                    trigger: AgentScheduleTrigger {
                        cron: update.cron,
                        timezone: update.timezone,
                    },
                };
                let result = if schedule_id.node_id == local_node_id
                    || schedule_id.node_id.is_empty()
                {
                    client
                        .update_agent_schedule(&schedule_id.inner_id, expected_revision, definition)
                        .map(|schedule| format!("Updated schedule {}", schedule.name))
                        .map_err(|error| error.to_string())
                } else {
                    routed_dashboard_operation(
                        &client,
                        &schedule_id,
                        &local_node_id,
                        protocol::RoutedOperation::UpdateAgentSchedule {
                            schedule_id: schedule_id.inner_id.clone(),
                            expected_revision,
                            update: definition,
                        },
                    )
                    .and_then(|result| match result {
                        protocol::RoutedOperationResult::AgentSchedule { schedule } => {
                            Ok(format!("Updated schedule {}", schedule.name))
                        }
                        _ => Err(
                            "remote Node returned an unexpected schedule update response".into(),
                        ),
                    })
                };
                tui::DashboardEvent::ScheduleEditorSaved {
                    schedule_id,
                    result,
                }
            }
            tui::DashboardEffect::CancelExecution(execution_id) => {
                let result =
                    if execution_id.node_id == local_node_id || execution_id.node_id.is_empty() {
                        cancel_dashboard_execution(&client, &execution_id.inner_id)
                    } else {
                        (|| {
                            let execution =
                                routed_dashboard_execution(&client, &execution_id, &local_node_id)?;
                            match routed_dashboard_operation(
                                &client,
                                &execution_id,
                                &local_node_id,
                                protocol::RoutedOperation::CancelScheduledExecution {
                                    execution_id: execution_id.inner_id.clone(),
                                    expected_revision: execution.revision,
                                },
                            )? {
                                protocol::RoutedOperationResult::ScheduledExecution {
                                    execution,
                                    ..
                                } => Ok(format!("Cancelled scheduled execution {}", execution.id)),
                                _ => {
                                    Err("remote Node returned an unexpected cancellation response"
                                        .into())
                                }
                            }
                        })()
                    };
                tui::DashboardEvent::OperationCompleted(result)
            }
            tui::DashboardEffect::OpenScheduledExecution { execution_id } => {
                let result = if execution_id.node_id == local_node_id
                    || execution_id.node_id.is_empty()
                {
                    open_scheduled_execution(&client, &execution_id.inner_id, terminal.as_deref())
                        .map(|opened| opened.message)
                        .map_err(|error| error.to_string())
                } else {
                    open_remote_scheduled_execution(&client, &execution_id, terminal.as_deref())
                        .map(|opened| opened.message)
                        .map_err(|error| error.to_string())
                };
                tui::DashboardEvent::OperationCompleted(result)
            }
            tui::DashboardEffect::RemoveSchedule(schedule_id) => {
                let schedule_inner = local_dashboard_inner(&schedule_id, &local_node_id);
                let name = refresh
                    .snapshot()
                    .workspaces
                    .iter()
                    .flat_map(|workspace| &workspace.schedules)
                    .find(|schedule| schedule_inner.as_ref().is_ok_and(|id| schedule.id == **id))
                    .map_or_else(|| "schedule".into(), |schedule| schedule.name.clone());
                let result = if schedule_id.node_id == local_node_id
                    || schedule_id.node_id.is_empty()
                {
                    schedule_inner.and_then(|id| {
                        client
                            .remove_agent_schedule(id)
                            .map_err(|error| error.to_string())
                    })
                } else {
                    (|| {
                        let inspection =
                            routed_dashboard_schedule(&client, &schedule_id, &local_node_id)?;
                        routed_dashboard_operation(
                            &client,
                            &schedule_id,
                            &local_node_id,
                            protocol::RoutedOperation::RemoveAgentSchedule {
                                schedule_id: schedule_id.inner_id.clone(),
                                expected_revision: inspection.schedule.revision,
                            },
                        )?;
                        Ok(())
                    })()
                }
                .map(|()| {
                    format!("Removed schedule {name}, its persisted prompt, and retained history")
                });
                tui::DashboardEvent::OperationCompleted(result)
            }
            tui::DashboardEffect::LoadScheduleHistory { schedule_id, limit } => {
                let result =
                    if schedule_id.node_id == local_node_id || schedule_id.node_id.is_empty() {
                        refresh
                            .load_schedule_history(&client, &schedule_id.inner_id, limit)
                            .map(|(_, truncated)| {
                                let schedules = dashboard_projection::project_schedules(
                                    refresh.snapshot(),
                                    &refresh.executions(),
                                    refresh.history_truncated,
                                    &refresh.scoped_history,
                                );
                                let executions = schedules
                                    .into_iter()
                                    .find(|schedule| schedule.id == schedule_id.inner_id)
                                    .map_or_else(Vec::new, |schedule| schedule.executions);
                                (executions, truncated)
                            })
                            .map_err(|error| error.to_string())
                    } else {
                        client
                            .route_node_operation(
                                &schedule_id.node_id,
                                protocol::RoutedOperation::ListScheduledExecutions {
                                    workspace_id: None,
                                    schedule_id: Some(schedule_id.inner_id.clone()),
                                    limit,
                                },
                            )
                            .map_err(|error| error.to_string())
                            .and_then(|result| match result {
                                protocol::RoutedOperationResult::ScheduledExecutions {
                                    executions,
                                    truncated,
                                    ..
                                } => Ok((
                                    dashboard_projection::project_remote_executions(&executions),
                                    truncated,
                                )),
                                _ => Err(
                                    "remote Node returned an unexpected execution history response"
                                        .into(),
                                ),
                            })
                    };
                tui::DashboardEvent::ScheduleHistoryCompleted {
                    schedule_id,
                    result,
                }
            }
            tui::DashboardEffect::ReadTerminalPreview {
                shell_id,
                run_id,
                output_revision,
            } => tui::DashboardEvent::TerminalPreviewCompleted {
                output: local_dashboard_inner(&shell_id, &local_node_id).and_then(|id| {
                    client
                        .read_shell_preview(id, READ_BYTES, 500)
                        .map_err(|error| error.to_string())
                }),
                shell_id: shell_id.inner_id,
                run_id,
                output_revision,
            },
        },
    )?;
    Ok(())
}

fn dashboard_state(
    snapshot: Snapshot,
    combined: Option<protocol::CombinedNodeSnapshot>,
    refresh: &DashboardRefresh,
    git_cache: &mut git::Cache,
    title_cache: &mut host_session_titles::Cache,
    reset_focus_revision: bool,
) -> tui::DashboardState {
    let schedules_supported = protocol::ProtocolFeature::ScheduledExecutionObservation
        .is_supported_by(refresh.negotiated_protocol);
    let mut workspaces = dashboard_views_with_catalog(&snapshot.workspaces, git_cache, title_cache);
    if !schedules_supported {
        for workspace in &mut workspaces {
            workspace
                .items
                .retain(|item| !matches!(item, tui::WorkspaceItemView::Schedule(_)));
        }
    }
    enrich_session_titles(&mut workspaces, title_cache);
    let scheduling = if !schedules_supported {
        tui::SchedulingView::Unsupported {
            required_protocol: protocol::ProtocolFeature::ScheduledExecutionObservation
                .minimum_version(),
            negotiated: refresh.negotiated_protocol,
        }
    } else {
        match snapshot.scheduler.as_ref() {
            Some(health) => match health.state {
                protocol::SchedulerState::Active => tui::SchedulingView::Active {
                    active: health.active_executions,
                    maximum: health.max_concurrent,
                },
                protocol::SchedulerState::Offline => tui::SchedulingView::Offline {
                    active: health.active_executions,
                    maximum: health.max_concurrent,
                },
            },
            None => tui::SchedulingView::Offline {
                active: 0,
                maximum: 0,
            },
        }
    };
    let mut schedules = if schedules_supported {
        dashboard_projection::project_schedules(
            &snapshot,
            &refresh.executions(),
            refresh.history_truncated,
            &refresh.scoped_history,
        )
    } else {
        Vec::new()
    };
    let mut nodes = Vec::new();
    if let Some(combined) = combined {
        for node in combined.nodes {
            let node_view = tui::NodeView {
                id: node.node_id.clone(),
                alias: node.alias.clone(),
                local: node.local,
                health: node.health,
                current: node.current,
                stale: node.stale,
                observed_at_ms: node.observed_at_ms,
                observed_protocol_version: node.observed_protocol_version,
                observed_capabilities: node.observed_capabilities.clone(),
                scheduler: node.scheduler.clone(),
            };
            if node.local {
                for workspace in &mut workspaces {
                    workspace.node = node_view.clone();
                }
                for schedule in &mut schedules {
                    schedule.node_id = node.node_id.clone();
                    schedule.node_alias = node.alias.clone();
                    schedule.actionable = node.current && !node.stale;
                }
            } else {
                let (mut remote_workspaces, mut remote_schedules) =
                    dashboard_projection::project_remote_node(&node);
                workspaces.append(&mut remote_workspaces);
                schedules.append(&mut remote_schedules);
            }
            nodes.push(node_view);
        }
    }
    if nodes.is_empty() {
        nodes.extend(
            workspaces
                .iter()
                .map(|workspace| workspace.node.clone())
                .take(1),
        );
    }
    workspaces.sort_by(|left, right| {
        left.node
            .alias
            .cmp(&right.node.alias)
            .then_with(|| left.name.cmp(&right.name))
            .then_with(|| left.id.cmp(&right.id))
    });
    schedules.sort_by(|left, right| {
        left.node_alias
            .cmp(&right.node_alias)
            .then_with(|| left.workspace.cmp(&right.workspace))
            .then_with(|| left.name.cmp(&right.name))
            .then_with(|| left.id.cmp(&right.id))
    });
    tui::DashboardState {
        nodes,
        workspaces,
        schedules,
        scheduling,
        exact_run_attachment: protocol::ProtocolFeature::ExactRunAttachment
            .is_supported_by(refresh.negotiated_protocol),
        schedule_editing: protocol::ProtocolFeature::AgentScheduleEditing
            .is_supported_by(refresh.negotiated_protocol),
        focused_terminal: snapshot.focused_terminal.map(focused_terminal_view),
        reset_focus_revision,
        initial_node_filter: None,
    }
}

fn combined_node_snapshot_for_dashboard(
    client: &client::Client,
) -> Result<Option<protocol::CombinedNodeSnapshot>, client::ClientError> {
    if client.supports(protocol::ProtocolFeature::CombinedNodeSnapshot)? {
        client.combined_node_snapshot(None).map(Some)
    } else {
        Ok(None)
    }
}

fn focused_terminal_view(focused: protocol::FocusedTerminalSnapshot) -> tui::FocusedTerminalView {
    tui::FocusedTerminalView {
        revision: focused.revision,
        workspace_id: focused.workspace_id,
        shell_id: focused.shell_id,
    }
}

fn dispatch_dashboard_open<S, L>(
    target: &tui::OpenTarget,
    _local_node_id: &str,
    mut open_shell: S,
    mut launch: L,
) -> Result<String, String>
where
    S: FnMut(&protocol::QualifiedIdentity) -> Result<String, String>,
    L: FnMut(&protocol::QualifiedIdentity, &protocol::QualifiedIdentity) -> Result<String, String>,
{
    match target {
        tui::OpenTarget::Shell(shell_id) => open_shell(shell_id),
        tui::OpenTarget::Launcher {
            workspace_id,
            launcher_id,
        } => {
            if workspace_id.node_id != launcher_id.node_id {
                return Err("workspace and launcher belong to different Nodes".into());
            }
            launch(workspace_id, launcher_id)
        }
    }
}

fn local_dashboard_inner<'a>(
    identity: &'a protocol::QualifiedIdentity,
    local_node_id: &str,
) -> Result<&'a str, String> {
    if identity.node_id == local_node_id || identity.node_id.is_empty() {
        Ok(&identity.inner_id)
    } else {
        Err(format!(
            "remote Node {} is read-only in this Boomux version",
            identity.node_id
        ))
    }
}

fn routed_dashboard_workspace(
    client: &client::Client,
    identity: &protocol::QualifiedIdentity,
    local_node_id: &str,
) -> Result<protocol::WorkspaceSnapshot, String> {
    if identity.node_id == local_node_id || identity.node_id.is_empty() {
        return client
            .get_workspace(&identity.inner_id)
            .map_err(|error| error.to_string());
    }
    match client
        .route_node_operation(
            &identity.node_id,
            protocol::RoutedOperation::GetWorkspace {
                workspace_id: identity.inner_id.clone(),
            },
        )
        .map_err(|error| error.to_string())?
    {
        protocol::RoutedOperationResult::Workspace { workspace } => Ok(workspace),
        _ => Err("remote Node returned an unexpected workspace response".into()),
    }
}

fn routed_dashboard_shell(
    client: &client::Client,
    identity: &protocol::QualifiedIdentity,
    local_node_id: &str,
) -> Result<protocol::ShellSnapshot, String> {
    if identity.node_id == local_node_id || identity.node_id.is_empty() {
        return client
            .get_shell(&identity.inner_id)
            .map_err(|error| error.to_string());
    }
    match client
        .route_node_operation(
            &identity.node_id,
            protocol::RoutedOperation::GetShell {
                shell_id: identity.inner_id.clone(),
            },
        )
        .map_err(|error| error.to_string())?
    {
        protocol::RoutedOperationResult::Shell { shell } => Ok(shell),
        _ => Err("remote Node returned an unexpected shell response".into()),
    }
}

fn routed_dashboard_launcher(
    client: &client::Client,
    identity: &protocol::QualifiedIdentity,
    local_node_id: &str,
) -> Result<protocol::WorkspaceLauncherSnapshot, String> {
    if identity.node_id == local_node_id || identity.node_id.is_empty() {
        return client
            .get_launcher(&identity.inner_id)
            .map_err(|error| error.to_string());
    }
    match client
        .route_node_operation(
            &identity.node_id,
            protocol::RoutedOperation::GetLauncher {
                launcher_id: identity.inner_id.clone(),
            },
        )
        .map_err(|error| error.to_string())?
    {
        protocol::RoutedOperationResult::Launcher { launcher } => Ok(launcher),
        _ => Err("remote Node returned an unexpected launcher response".into()),
    }
}

fn routed_dashboard_schedule(
    client: &client::Client,
    identity: &protocol::QualifiedIdentity,
    local_node_id: &str,
) -> Result<protocol::AgentScheduleInspection, String> {
    if identity.node_id == local_node_id || identity.node_id.is_empty() {
        return client
            .get_agent_schedule(&identity.inner_id)
            .map_err(|error| error.to_string());
    }
    match client
        .route_node_operation(
            &identity.node_id,
            protocol::RoutedOperation::GetAgentSchedule {
                schedule_id: identity.inner_id.clone(),
            },
        )
        .map_err(|error| error.to_string())?
    {
        protocol::RoutedOperationResult::AgentScheduleInspection { inspection } => Ok(inspection),
        _ => Err("remote Node returned an unexpected schedule response".into()),
    }
}

fn routed_dashboard_execution(
    client: &client::Client,
    identity: &protocol::QualifiedIdentity,
    local_node_id: &str,
) -> Result<protocol::ScheduledExecutionSnapshot, String> {
    if identity.node_id == local_node_id || identity.node_id.is_empty() {
        return client
            .get_scheduled_execution(&identity.inner_id)
            .map_err(|error| error.to_string());
    }
    match client
        .route_node_operation(
            &identity.node_id,
            protocol::RoutedOperation::GetScheduledExecution {
                execution_id: identity.inner_id.clone(),
            },
        )
        .map_err(|error| error.to_string())?
    {
        protocol::RoutedOperationResult::ScheduledExecution { execution, .. } => Ok(execution),
        _ => Err("remote Node returned an unexpected execution response".into()),
    }
}

fn routed_dashboard_operation(
    client: &client::Client,
    identity: &protocol::QualifiedIdentity,
    local_node_id: &str,
    operation: protocol::RoutedOperation,
) -> Result<protocol::RoutedOperationResult, String> {
    if identity.node_id == local_node_id || identity.node_id.is_empty() {
        return Err("internal error: local operation was sent through Node routing".into());
    }
    client
        .route_node_operation(&identity.node_id, operation)
        .map_err(|error| error.to_string())
}

fn run_dashboard_schedule(client: &client::Client, schedule_id: &str) -> Result<String, String> {
    client
        .run_agent_schedule(schedule_id, Uuid::new_v4().to_string())
        .map(|execution| format!("Started scheduled execution {}", execution.id))
        .map_err(|error| error.to_string())
}

fn cancel_dashboard_execution(
    client: &client::Client,
    execution_id: &str,
) -> Result<String, String> {
    let execution = client
        .get_scheduled_execution(execution_id)
        .map_err(|error| error.to_string())?;
    if !matches!(
        execution.state,
        protocol::ScheduledExecutionState::Claimed
            | protocol::ScheduledExecutionState::Starting
            | protocol::ScheduledExecutionState::Active
    ) {
        return Err(format!(
            "execution {} is no longer active; refresh before cancelling",
            execution.id
        ));
    }
    client
        .cancel_scheduled_execution(execution_id)
        .map(|execution| format!("Cancelled exact execution {}", execution.id))
        .map_err(|error| error.to_string())
}

struct OpenedScheduledExecution {
    execution: ScheduledExecutionSnapshot,
    target: &'static str,
    message: String,
}

#[derive(Debug)]
enum ScheduledExecutionOpenTarget<'a> {
    Run { shell_id: &'a str, run_id: &'a str },
    Session { agent_id: &'a str },
}

fn scheduled_execution_open_target(
    execution: &ScheduledExecutionSnapshot,
) -> Result<ScheduledExecutionOpenTarget<'_>, (&'static str, &'static str)> {
    if matches!(
        execution.state,
        protocol::ScheduledExecutionState::Starting | protocol::ScheduledExecutionState::Active
    ) {
        let shell_id = execution
            .shell_id
            .as_deref()
            .ok_or(("busy", "scheduled execution has no exact retained shell"))?;
        let run_id = execution
            .run_id
            .as_deref()
            .ok_or(("busy", "scheduled execution has no exact retained run"))?;
        return Ok(ScheduledExecutionOpenTarget::Run { shell_id, run_id });
    }
    execution
        .agent_id
        .as_deref()
        .map(|agent_id| ScheduledExecutionOpenTarget::Session { agent_id })
        .ok_or((
            "not_found",
            "scheduled execution has no exact linked Agent Session to open",
        ))
}

fn open_scheduled_execution(
    client: &client::Client,
    execution_id: &str,
    terminal: Option<&str>,
) -> Result<OpenedScheduledExecution, Box<dyn Error>> {
    let execution = client.get_scheduled_execution(execution_id)?;
    let target = scheduled_execution_open_target(&execution)
        .map_err(|(code, message)| cli_output::failure(code, message))?;
    if let ScheduledExecutionOpenTarget::Run { shell_id, run_id } = target {
        if !client.supports(protocol::ProtocolFeature::ExactRunAttachment)? {
            return Err(cli_output::failure(
                "unsupported_version",
                "opening exact Scheduled Execution runs requires daemon protocol 26; upgrade and restart Boomux",
            ));
        }
        let shell = client.get_shell(shell_id)?;
        validate_dashboard_execution_open(&execution, &shell, shell_id, run_id)
            .map_err(|message| cli_output::failure("busy", message))?;
        let workspace = client.get_workspace(&execution.workspace_id)?;
        terminal::open_exact_run(
            terminal,
            shell_id,
            run_id,
            &format!("{} - {}", workspace.name, shell.name),
            true,
        )?;
        return Ok(OpenedScheduledExecution {
            message: format!(
                "Opened exact execution {} from schedule {}",
                execution.id, execution.schedule_id
            ),
            execution,
            target: "run",
        });
    }

    let ScheduledExecutionOpenTarget::Session { agent_id } = target else {
        unreachable!("run targets return after opening")
    };
    let snapshot = client.snapshot()?;
    let catalog = discover_host_catalog(&snapshot.workspaces);
    let sessions = session_projection::project_snapshot_with_catalog(&snapshot, Some(&catalog));
    let session = sessions
        .iter()
        .find(|session| {
            session.workspace_id == execution.workspace_id
                && session
                    .occurrences
                    .iter()
                    .any(|occurrence| occurrence.agent_id == agent_id)
        })
        .ok_or_else(|| {
            cli_output::failure(
                "not_found",
                "scheduled execution's exact linked Agent Session is no longer available",
            )
        })?;
    let (cwd, command) = dashboard_session_resume_plan(session)
        .map_err(|message| cli_output::failure("invalid_argument", message))?;
    terminal::open_command(
        terminal,
        &cwd,
        &format!(
            "{} - {} session",
            session.workspace_name, session.integration
        ),
        &command,
    )?;
    Ok(OpenedScheduledExecution {
        message: format!(
            "Opened exact {} Agent Session for execution {}",
            session.integration, execution.id
        ),
        execution,
        target: "session",
    })
}

fn open_remote_scheduled_execution(
    client: &client::Client,
    execution_id: &protocol::QualifiedIdentity,
    terminal: Option<&str>,
) -> Result<OpenedScheduledExecution, Box<dyn Error>> {
    let execution =
        routed_dashboard_execution(client, execution_id, "").map_err(io::Error::other)?;
    let target = scheduled_execution_open_target(&execution)
        .map_err(|(code, message)| cli_output::failure(code, message))?;
    if let ScheduledExecutionOpenTarget::Session { agent_id } = target {
        let result = client.route_node_host_service(
            &execution_id.node_id,
            protocol::HostServiceOperation::ResolveAgentSession {
                workspace_id: execution.workspace_id.clone(),
                agent_id: agent_id.to_owned(),
            },
        )?;
        let protocol::HostServiceResult::ResolvedAgentSession { session } = result else {
            return Err("remote Node returned an unexpected Agent Session response".into());
        };
        let registration = client.node_registration(&execution_id.node_id)?;
        terminal::open_agent_session(
            terminal,
            Some(&execution_id.node_id),
            &session.id,
            &format!(
                "[{}] {} - {} session",
                registration.alias, session.workspace_name, session.integration,
            ),
        )?;
        return Ok(OpenedScheduledExecution {
            message: format!(
                "Opened exact {} Agent Session for execution {} on Node {}",
                session.integration, execution.id, registration.alias,
            ),
            execution,
            target: "session",
        });
    }
    let ScheduledExecutionOpenTarget::Run { shell_id, run_id } = target else {
        unreachable!("session targets return after opening")
    };
    let shell_identity = protocol::QualifiedIdentity::new(&execution_id.node_id, shell_id);
    let shell = routed_dashboard_shell(client, &shell_identity, "").map_err(io::Error::other)?;
    validate_dashboard_execution_open(&execution, &shell, shell_id, run_id)
        .map_err(|message| cli_output::failure("busy", message))?;
    let workspace = match client.route_node_operation(
        &execution_id.node_id,
        protocol::RoutedOperation::GetWorkspace {
            workspace_id: execution.workspace_id.clone(),
        },
    )? {
        protocol::RoutedOperationResult::Workspace { workspace } => workspace,
        _ => return Err("remote Node returned an unexpected workspace response".into()),
    };
    let registration = client.node_registration(&execution_id.node_id)?;
    terminal::open_remote_exact_run(
        terminal,
        &execution_id.node_id,
        shell_id,
        run_id,
        &format!(
            "[{}] {} - {}",
            registration.alias, workspace.name, shell.name
        ),
        true,
    )?;
    Ok(OpenedScheduledExecution {
        message: format!(
            "Opened exact execution {} from schedule {} on Node {}",
            execution.id, execution.schedule_id, registration.alias
        ),
        execution,
        target: "run",
    })
}

fn validate_dashboard_execution_open(
    execution: &ScheduledExecutionSnapshot,
    shell: &ShellSnapshot,
    shell_id: &str,
    run_id: &str,
) -> Result<(), String> {
    if !matches!(
        execution.state,
        protocol::ScheduledExecutionState::Starting | protocol::ScheduledExecutionState::Active
    ) {
        return Err("selected scheduled execution is no longer openable".into());
    }
    if execution.shell_id.as_deref() != Some(shell_id)
        || execution.run_id.as_deref() != Some(run_id)
    {
        return Err("scheduled execution links changed; refresh before opening".into());
    }
    if shell.id != shell_id
        || shell.workspace_id != execution.workspace_id
        || shell.owner
            != (protocol::ShellOwner::Schedule {
                schedule_id: execution.schedule_id.clone(),
            })
    {
        return Err("scheduled execution shell ownership no longer matches".into());
    }
    if shell.status != ShellStatus::Running
        || shell.run.as_ref().map(|run| run.id.as_str()) != Some(run_id)
    {
        return Err(
            "scheduled execution shell has moved to a different run; refresh before opening".into(),
        );
    }
    Ok(())
}

fn dashboard_session_resume_plan(
    session: &session_projection::SessionProjection,
) -> Result<(PathBuf, Vec<String>), String> {
    if session.state_is_current {
        return Err(
            "Agent Session is already active in a managed shell; open that shell instead".into(),
        );
    }
    if session.state == AgentState::Done {
        return Err("Agent Session is permanently done and cannot be resumed".into());
    }
    let external_session_id = session
        .external_session_id
        .as_deref()
        .ok_or("Agent Session has no canonical external session ID")?;
    let descriptor = boomux::integrations::by_key(&session.integration)
        .ok_or_else(|| format!("unknown Agent integration {}", session.integration))?;
    let resume = descriptor.resume.ok_or_else(|| {
        format!(
            "{} does not support interactive session resume",
            descriptor.display_name
        )
    })?;
    let command = resume.command(&[], external_session_id).ok_or_else(|| {
        format!(
            "{} could not construct a session resume command",
            descriptor.display_name
        )
    })?;
    let cwd = session
        .source_cwd
        .as_deref()
        .ok_or("Agent Session has no retained working directory")?;
    let cwd = resolve_directory(cwd).map_err(|error| error.to_string())?;
    Ok((cwd, command))
}

#[cfg(test)]
fn dashboard_views(
    workspaces: &[WorkspaceSnapshot],
    git_cache: &mut git::Cache,
) -> Vec<tui::WorkspaceView> {
    dashboard_projection::project(workspaces, git_cache)
}

fn dashboard_views_with_catalog(
    workspaces: &[WorkspaceSnapshot],
    git_cache: &mut git::Cache,
    title_cache: &mut host_session_titles::Cache,
) -> Vec<tui::WorkspaceView> {
    let catalog = cached_host_catalog(workspaces, title_cache);
    let sessions = session_projection::project_workspaces_with_catalog(workspaces, Some(&catalog));
    dashboard_projection::project_with_sessions(workspaces, git_cache, &sessions)
}

fn workspace_source_directories(workspaces: &[WorkspaceSnapshot]) -> BTreeSet<PathBuf> {
    workspaces
        .iter()
        .flat_map(|workspace| {
            workspace
                .default_cwd
                .iter()
                .cloned()
                .chain(workspace.shells.iter().map(|shell| shell.cwd.clone()))
                .chain(
                    workspace
                        .launchers
                        .iter()
                        .map(|launcher| launcher.cwd.clone()),
                )
                .chain(
                    workspace
                        .agents
                        .iter()
                        .filter_map(|agent| agent.cwd.clone()),
                )
        })
        .collect()
}

fn cached_host_catalog(
    workspaces: &[WorkspaceSnapshot],
    cache: &mut host_session_titles::Cache,
) -> Vec<host_session_titles::HostSession> {
    let mut catalog = Vec::new();
    for directory in workspace_source_directories(workspaces)
        .into_iter()
        .take(MAX_HOST_CATALOG_DIRECTORIES)
    {
        for integration in host_session_titles::catalog_integrations() {
            if let Some(sessions) = cache.catalog(integration, &directory) {
                catalog.extend(sessions);
            }
        }
    }
    catalog
}

fn discover_host_catalog(
    workspaces: &[WorkspaceSnapshot],
) -> Vec<host_session_titles::HostSession> {
    let directories = workspace_source_directories(workspaces)
        .into_iter()
        .take(MAX_HOST_CATALOG_DIRECTORIES)
        .collect::<Vec<_>>();
    let requests = directories.into_iter().flat_map(|directory| {
        host_session_titles::catalog_integrations()
            .map(move |integration| (integration, directory.clone()))
    });
    thread::scope(|scope| {
        requests
            .map(|(integration, directory)| {
                scope.spawn(move || host_session_titles::catalog(integration, &directory))
            })
            .collect::<Vec<_>>()
            .into_iter()
            .filter_map(|handle| handle.join().ok().flatten())
            .flatten()
            .collect()
    })
}

fn enrich_session_titles(
    workspaces: &mut [tui::WorkspaceView],
    title_cache: &mut host_session_titles::Cache,
) {
    dashboard_projection::enrich_session_titles(workspaces, title_cache);
}

fn open_directory(
    path: &Path,
    requested_name: Option<&str>,
    startup_command: &[String],
    open_in_new_window: bool,
    terminal: Option<&str>,
) -> Result<(), Box<dyn Error>> {
    ensure_host_terminal()?;
    let directory = resolve_directory(path)?;
    let requested_name = requested_name
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(str::to_owned);
    let client = client::connect_or_start()?;
    let snapshot = client.snapshot()?;
    let (shell, workspace_name, created_workspace) = if let Some(name) = requested_name {
        let (shell, created_workspace) =
            if let Some(workspace) = find_workspace(&snapshot.workspaces, &name) {
                (
                    create_generated_shell(&client, &workspace.id, &directory, startup_command)?,
                    false,
                )
            } else {
                (
                    client
                        .create_workspace_with_default_cwd(
                            &name,
                            Some(directory.clone()),
                            vec![shell_spec(
                                generated_names::random(),
                                &directory,
                                startup_command,
                            )],
                        )?
                        .shells
                        .into_iter()
                        .next()
                        .ok_or("new workspace has no shell")?,
                    true,
                )
            };
        (shell, name, created_workspace)
    } else {
        let shell = client.create_shell_with_workspace(shell_spec(
            generated_names::random(),
            &directory,
            startup_command,
        ))?;
        let workspace_name = client.get_workspace(&shell.workspace_id)?.name;
        (shell, workspace_name, true)
    };

    let open_result = if open_in_new_window {
        open_terminal(
            &shell.id,
            &format!("{workspace_name} - {}", shell.name),
            true,
            terminal,
        )
    } else {
        Ok(attach::run(&shell.id, None, true, false, None)?)
    };
    if let Err(open_error) = open_result {
        let rollback = if created_workspace {
            client.close_workspace(&shell.workspace_id)
        } else {
            client.close_shell(&shell.id)
        };
        if let Err(rollback_error) = rollback {
            return Err(format!(
                "{open_error}; additionally could not roll back failed launch: {rollback_error}"
            )
            .into());
        }
        return Err(open_error);
    }
    Ok(())
}

fn shell_spec(name: impl Into<String>, cwd: &Path, command: &[String]) -> ShellSpec {
    ShellSpec {
        name: name.into(),
        command: command.to_vec(),
        cwd: cwd.to_owned(),
    }
}

fn ensure_host_terminal() -> Result<(), Box<dyn Error>> {
    if env::var_os("BOOMUX_SHELL_ID").is_some() {
        Err("already inside a Boomux shell; launch Boomux from a fresh terminal".into())
    } else {
        Ok(())
    }
}

fn resolve_directory(path: &Path) -> Result<PathBuf, Box<dyn Error>> {
    let absolute = if path.is_absolute() {
        path.to_owned()
    } else {
        env::current_dir()?.join(path)
    };
    let resolved = absolute
        .canonicalize()
        .map_err(|error| format!("cannot open {}: {error}", path.display()))?;
    if !resolved.is_dir() {
        return Err(format!("{} is not a directory", path.display()).into());
    }
    Ok(resolved)
}

fn cli_name(name: String, kind: &str) -> Result<String, Box<dyn Error>> {
    let name = name.trim();
    if name.is_empty() {
        return Err(cli_output::failure(
            "invalid_argument",
            format!("{kind} name cannot be empty"),
        ));
    }
    Ok(name.to_owned())
}

fn cli_name_or_generated(name: Option<String>, kind: &str) -> Result<String, Box<dyn Error>> {
    name.map(|name| cli_name(name, kind))
        .transpose()
        .map(|name| name.unwrap_or_else(generated_names::random))
}

fn find_workspace<'a>(
    workspaces: &'a [WorkspaceSnapshot],
    name: &str,
) -> Option<&'a WorkspaceSnapshot> {
    workspaces.iter().find(|workspace| workspace.name == name)
}

fn resolve_workspace_target<'a>(
    workspaces: &'a [WorkspaceSnapshot],
    target: &str,
) -> Result<&'a WorkspaceSnapshot, Box<dyn Error>> {
    workspaces
        .iter()
        .find(|workspace| workspace.id == target)
        .or_else(|| workspaces.iter().find(|workspace| workspace.name == target))
        .ok_or_else(|| cli_output::failure("not_found", format!("workspace not found: {target}")))
}

fn create_dashboard_workspace(
    client: &client::Client,
    name: &str,
    default_cwd: Option<&PathBuf>,
) -> Result<String, Box<dyn Error>> {
    let name = name.trim();
    if name.is_empty() {
        return Err("workspace name cannot be empty".into());
    }
    client.create_workspace_with_default_cwd(name, default_cwd.cloned(), Vec::new())?;
    Ok(format!("Created empty workspace {name}"))
}

fn create_dashboard_shell(
    client: &client::Client,
    workspace_id: &str,
    launch_cwd: &Path,
) -> Result<String, Box<dyn Error>> {
    let workspace = client.get_workspace(workspace_id)?;
    let cwd = resolve_shell_cwd(workspace.default_cwd.as_deref(), None, launch_cwd)?;
    let shell = create_generated_shell(client, workspace_id, &cwd, &[])?;
    Ok(format!("Created {} in {}", shell.name, workspace.name))
}

fn resolve_shell_cwd(
    default_cwd: Option<&Path>,
    requested_cwd: Option<&Path>,
    fallback_cwd: &Path,
) -> Result<PathBuf, Box<dyn Error>> {
    if let Some(cwd) = requested_cwd {
        return resolve_directory(cwd);
    }
    let Some(default_cwd) = default_cwd else {
        return resolve_directory(fallback_cwd);
    };
    resolve_directory(default_cwd).map_err(|error| {
        format!(
            "workspace default working directory is unavailable: {}: {error}",
            default_cwd.display()
        )
        .into()
    })
}

fn create_generated_shell(
    client: &client::Client,
    workspace_id: &str,
    cwd: &Path,
    command: &[String],
) -> Result<ShellSnapshot, Box<dyn Error>> {
    let mut rejected = BTreeSet::new();
    loop {
        let workspace = client.get_workspace(workspace_id)?;
        let name = generated_shell_name(
            workspace
                .shells
                .iter()
                .map(|shell| shell.name.as_str())
                .chain(rejected.iter().map(String::as_str)),
        )?;
        match client.create_shell(workspace_id, shell_spec(&name, cwd, command)) {
            Ok(shell) => return Ok(shell),
            Err(client::ClientError::Remote(error))
                if error.code == Some(protocol::ErrorCode::AlreadyExists) =>
            {
                rejected.insert(name);
            }
            Err(error) => return Err(error.into()),
        }
    }
}

fn generated_shell_name<'a>(
    unavailable: impl IntoIterator<Item = &'a str>,
) -> Result<String, Box<dyn Error>> {
    generated_names::random_excluding(unavailable).ok_or_else(|| {
        cli_output::failure(
            "already_exists",
            "all generated shell names are already in use",
        )
    })
}

fn integration_command(command: IntegrationCommands, json: bool) -> Result<(), Box<dyn Error>> {
    let node = match &command {
        IntegrationCommands::List => None,
        IntegrationCommands::Status { node, .. }
        | IntegrationCommands::Install { node, .. }
        | IntegrationCommands::Uninstall { node, .. }
        | IntegrationCommands::Setup { node, .. }
        | IntegrationCommands::Verify { node, .. } => node.as_deref(),
    };
    if let Some(node) = node {
        let client = client::connect_or_start()?;
        let registration = client.node_registration(node)?;
        return remote_integration_command(&client, &registration, command, json);
    }
    match command {
        IntegrationCommands::List => list_integrations(json),
        IntegrationCommands::Status { integration, .. } => integration_status(integration, json),
        IntegrationCommands::Install {
            integration,
            all,
            force,
            dry_run,
            ..
        } => {
            let integrations = if all {
                integration_management::IntegrationId::all().collect()
            } else {
                vec![integration.ok_or_else(|| {
                    cli_output::failure(
                        "invalid_argument",
                        "integration install requires a name or --all",
                    )
                })?]
            };
            install_integrations(&integrations, force, dry_run, json)
        }
        IntegrationCommands::Uninstall {
            integration,
            all,
            force,
            ..
        } => {
            let integrations = if all {
                integration_management::IntegrationId::all().collect()
            } else {
                vec![integration.ok_or_else(|| {
                    cli_output::failure(
                        "invalid_argument",
                        "integration uninstall requires a name or --all",
                    )
                })?]
            };
            uninstall_integrations(&integrations, force, json)
        }
        IntegrationCommands::Setup {
            integration,
            yes,
            force,
            ..
        } => setup_integration(integration, yes, force),
        IntegrationCommands::Verify {
            integration,
            shell,
            wait_ms,
            ..
        } => verify_integration(integration, shell.as_deref(), wait_ms, json),
    }
}

fn remote_integration_command(
    client: &client::Client,
    registration: &protocol::NodeRegistrationSnapshot,
    command: IntegrationCommands,
    json: bool,
) -> Result<(), Box<dyn Error>> {
    let names = |integration: Option<integration_management::IntegrationId>, all: bool| {
        if all {
            integration_management::IntegrationId::all()
                .map(|id| id.spec().key.to_owned())
                .collect::<Vec<_>>()
        } else {
            integration
                .map(|id| vec![id.spec().key.to_owned()])
                .unwrap_or_default()
        }
    };
    match command {
        IntegrationCommands::List => unreachable!(),
        IntegrationCommands::Status { integration, .. } => {
            let result = client.route_node_host_service(
                &registration.node_id,
                protocol::HostServiceOperation::IntegrationStatus {
                    integration: integration.map(|id| id.spec().key.to_owned()),
                },
            )?;
            let protocol::HostServiceResult::IntegrationStatus { integrations } = result else {
                return Err("remote Node returned an unexpected integration status".into());
            };
            if json {
                let integrations = integrations
                    .into_iter()
                    .map(|status| serde_json::json!({
                        "name": status.name,
                        "display_name": status.display_name,
                        "package": status.package,
                        "validated_version": status.validated_version,
                        "host": { "state": status.host_state, "executable": status.executable, "version": status.version, "compatibility": status.compatibility, "error": status.host_error },
                        "asset": { "state": status.asset_state, "path": status.path, "error": status.asset_error },
                        "runtime": { "state": status.runtime_state, "running_processes": status.running_processes, "tracked_processes": status.tracked_processes, "untracked_processes": status.untracked_processes },
                        "recommended_action": status.recommended_action,
                    }))
                    .collect::<Vec<_>>();
                return print_json(
                    CommandKey::IntegrationStatus,
                    serde_json::json!({ "node_id": registration.node_id, "integrations": integrations }),
                );
            }
            println!("Integration status on Node {}", registration.alias);
            for status in integrations {
                println!(
                    "{}\t{}\t{}\t{}",
                    status.display_name,
                    status.host_state,
                    status.asset_state,
                    status.runtime_state
                );
            }
        }
        IntegrationCommands::Install {
            integration,
            all,
            force,
            dry_run,
            ..
        } => {
            let requested = names(integration, all);
            if requested.is_empty() {
                return Err(cli_output::failure(
                    "invalid_argument",
                    "integration install requires a name or --all",
                ));
            }
            let preview = remote_integration_preview(
                client,
                registration,
                protocol::HostServiceIntegrationAction::Install,
                requested,
                force,
            )?;
            if dry_run {
                if json {
                    return print_json(
                        CommandKey::IntegrationInstall,
                        serde_json::json!({ "node_id": registration.node_id, "dry_run": true, "integrations": preview.plans }),
                    );
                }
                for plan in preview.plans {
                    println!("Plan: {} {}", plan.action, plan.path);
                }
            } else {
                remote_integration_commit(
                    client,
                    registration,
                    preview.token,
                    CommandKey::IntegrationInstall,
                    json,
                )?;
            }
        }
        IntegrationCommands::Uninstall {
            integration,
            all,
            force,
            ..
        } => {
            let requested = names(integration, all);
            if requested.is_empty() {
                return Err(cli_output::failure(
                    "invalid_argument",
                    "integration uninstall requires a name or --all",
                ));
            }
            let preview = remote_integration_preview(
                client,
                registration,
                protocol::HostServiceIntegrationAction::Uninstall,
                requested,
                force,
            )?;
            remote_integration_commit(
                client,
                registration,
                preview.token,
                CommandKey::IntegrationUninstall,
                json,
            )?;
        }
        IntegrationCommands::Setup {
            integration,
            yes,
            force,
            ..
        } => {
            let preview = remote_integration_preview(
                client,
                registration,
                protocol::HostServiceIntegrationAction::Install,
                vec![integration.spec().key.to_owned()],
                force,
            )?;
            for plan in &preview.plans {
                println!("Node: {}", registration.alias);
                println!("Plan: {} {}", plan.action, plan.path);
            }
            if !yes && !confirm_setup("Apply this integration plan on the selected Node?")? {
                println!("No changes made.");
                return Ok(());
            }
            remote_integration_commit(
                client,
                registration,
                preview.token,
                CommandKey::IntegrationInstall,
                false,
            )?;
        }
        IntegrationCommands::Verify {
            integration,
            shell,
            wait_ms,
            ..
        } => {
            let shell_id = shell.ok_or_else(|| {
                cli_output::failure(
                    "context_required",
                    "remote integration verify requires --shell with an exact ID",
                )
            })?;
            let shell = match client.route_node_operation(
                &registration.node_id,
                protocol::RoutedOperation::GetShell {
                    shell_id: shell_id.clone(),
                },
            )? {
                protocol::RoutedOperationResult::Shell { shell } => shell,
                _ => return Err("remote Node returned an unexpected shell response".into()),
            };
            let run_id = shell
                .run
                .as_ref()
                .map(|run| run.id.clone())
                .ok_or_else(|| {
                    cli_output::failure("not_found", "remote integration shell has no active run")
                })?;
            let deadline = Instant::now() + Duration::from_millis(u64::from(wait_ms));
            let agents = loop {
                match client.route_node_host_service(
                    &registration.node_id,
                    protocol::HostServiceOperation::VerifyIntegration {
                        integration: integration.spec().key.to_owned(),
                        shell_id: shell_id.clone(),
                        run_id: run_id.clone(),
                    },
                ) {
                    Ok(protocol::HostServiceResult::IntegrationVerified { agents, .. }) => {
                        break agents;
                    }
                    Ok(_) => {
                        return Err(
                            "remote Node returned an unexpected verification response".into()
                        );
                    }
                    Err(_error) if Instant::now() < deadline => {
                        thread::sleep(Duration::from_millis(100))
                    }
                    Err(error) => return Err(error.into()),
                }
            };
            if json {
                return print_json(
                    CommandKey::IntegrationVerify,
                    serde_json::json!({ "node_id": registration.node_id, "integration": integration.spec().key, "verified": true, "shell_id": shell_id, "run_id": run_id, "agents": agents }),
                );
            }
            println!(
                "Verified {} on Node {}",
                integration.spec().display_name,
                registration.alias
            );
        }
    }
    Ok(())
}

fn remote_integration_preview(
    client: &client::Client,
    registration: &protocol::NodeRegistrationSnapshot,
    action: protocol::HostServiceIntegrationAction,
    integrations: Vec<String>,
    force: bool,
) -> Result<protocol::HostIntegrationMutationPreview, Box<dyn Error>> {
    let result = client.route_node_host_service(
        &registration.node_id,
        protocol::HostServiceOperation::PreviewIntegrationMutation {
            action,
            integrations,
            force,
        },
    )?;
    match result {
        protocol::HostServiceResult::IntegrationMutationPreview { preview } => Ok(preview),
        _ => Err("remote Node returned an unexpected integration preview".into()),
    }
}

fn remote_integration_commit(
    client: &client::Client,
    registration: &protocol::NodeRegistrationSnapshot,
    preview_token: String,
    command: CommandKey,
    json: bool,
) -> Result<(), Box<dyn Error>> {
    let result = client.route_node_host_service(
        &registration.node_id,
        protocol::HostServiceOperation::CommitIntegrationMutation { preview_token },
    )?;
    let protocol::HostServiceResult::IntegrationMutation { integrations } = result else {
        return Err("remote Node returned an unexpected integration mutation".into());
    };
    if json {
        print_json(
            command,
            serde_json::json!({ "node_id": registration.node_id, "integrations": integrations }),
        )
    } else {
        for result in integrations {
            println!("{}: {} {}", result.name, result.result, result.path);
        }
        Ok(())
    }
}

fn verify_integration(
    integration: integration_management::IntegrationId,
    shell_id: Option<&str>,
    wait_ms: u32,
    json: bool,
) -> Result<(), Box<dyn Error>> {
    let client = client::connect().map_err(|error| {
        cli_output::failure(
            "daemon_unavailable",
            format!("cannot verify integration without the daemon: {error}"),
        )
    })?;
    let baseline = client.events(None, 1, 0)?;
    let snapshot = baseline
        .snapshot
        .ok_or_else(|| io::Error::other("event baseline omitted its snapshot"))?;
    let targets = integration_management::verification_targets(&snapshot, integration, shell_id);
    let spec = integration.spec();
    let target = match targets.as_slice() {
        [target] => target.clone(),
        [] if shell_id.is_some() => {
            return Err(cli_output::failure(
                "not_found",
                format!(
                    "shell {} is not a running {} host shell",
                    shell_id.expect("checked above"),
                    spec.key
                ),
            ));
        }
        [] => {
            return Err(cli_output::failure(
                "not_found",
                format!("no running {} host shell found", spec.key),
            ));
        }
        _ => {
            return Err(cli_output::failure(
                "ambiguous_target",
                format_ambiguous_verification_targets(&snapshot, integration, &targets),
            ));
        }
    };
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(u64::from(wait_ms));
    let mut cursor = baseline.cursor;
    let mut snapshot = snapshot;
    loop {
        match integration_management::check_verification_target(&snapshot, integration, &target) {
            integration_management::VerificationCheck::Verified {
                workspace_name,
                agents,
            } => {
                if json {
                    let agents = agents
                        .iter()
                        .map(|agent| cli_output::agent(agent, Some(&workspace_name)))
                        .collect::<Vec<_>>();
                    return print_json(
                        CommandKey::IntegrationVerify,
                        serde_json::json!({
                            "integration": spec.key,
                            "verified": true,
                            "shell_id": target.shell_id,
                            "run_id": target.run_id,
                            "agents": agents,
                        }),
                    );
                }
                println!("Verified {} lifecycle reporting", spec.display_name);
                println!("  {:<12}{}", "Shell", target.shell_id);
                println!("  {:<12}{}", "Run", target.run_id);
                println!("  {:<12}{}", "Agents", agents.len());
                return Ok(());
            }
            integration_management::VerificationCheck::Missing => {
                return Err(cli_output::failure(
                    "not_found",
                    format!(
                        "shell {} is no longer a running {} host shell",
                        target.shell_id, spec.key
                    ),
                ));
            }
            integration_management::VerificationCheck::RunChanged => {
                return Err(cli_output::failure(
                    "run_changed",
                    format!("shell {} started a different run", target.shell_id),
                ));
            }
            integration_management::VerificationCheck::Pending => {}
        }
        let now = std::time::Instant::now();
        if wait_ms == 0 || now >= deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!(
                    "did not observe lifecycle integration reporting for {} in shell {} within {} ms",
                    spec.key, target.shell_id, wait_ms
                ),
            )
            .into());
        }
        let remaining = deadline.saturating_duration_since(now).as_millis();
        let chunk_ms = u32::try_from(remaining.min(30_000))
            .unwrap_or(30_000)
            .max(1);
        let batch = client.events(Some(cursor), 1, chunk_ms)?;
        cursor = batch.cursor;
        snapshot = client.snapshot()?;
    }
}

fn format_ambiguous_verification_targets(
    snapshot: &Snapshot,
    integration: integration_management::IntegrationId,
    targets: &[integration_management::VerificationTarget],
) -> String {
    let spec = integration.spec();
    let mut choices = targets
        .iter()
        .filter_map(|target| {
            snapshot.workspaces.iter().find_map(|workspace| {
                workspace
                    .shells
                    .iter()
                    .find(|shell| shell.id == target.shell_id)
                    .map(|shell| {
                        (
                            workspace.name.as_str(),
                            shell.name.as_str(),
                            target.shell_id.as_str(),
                        )
                    })
            })
        })
        .collect::<Vec<_>>();
    choices.sort_unstable();

    let mut output = format!(
        "multiple running {} host shells found\n\nChoose a shell:",
        spec.display_name
    );
    for (workspace, shell, shell_id) in choices {
        write!(
            output,
            "\n  {workspace} / {shell}\n    boomux integration verify {} --shell {shell_id}",
            spec.key
        )
        .expect("writing to a string cannot fail");
    }
    output
}

fn list_integrations(json: bool) -> Result<(), Box<dyn Error>> {
    let integrations = integration_management::IntegrationId::all()
        .map(integration_management::IntegrationSummary::from)
        .collect::<Vec<_>>();
    if json {
        return print_json(
            CommandKey::IntegrationList,
            serde_json::json!({ "integrations": integrations }),
        );
    }
    print!("{}", format_integration_list(&integrations));
    Ok(())
}

fn format_integration_list(integrations: &[integration_management::IntegrationSummary]) -> String {
    let name_width = integrations
        .iter()
        .map(|integration| integration.name.len())
        .max()
        .unwrap_or(0)
        .max("NAME".len());
    let package_width = integrations
        .iter()
        .map(|integration| integration.package.len())
        .max()
        .unwrap_or(0)
        .max("PACKAGE".len());
    let mut output = String::new();
    writeln!(
        output,
        "{:<name_width$}  {:<package_width$}  VALIDATED VERSION",
        "NAME", "PACKAGE"
    )
    .expect("writing to a string cannot fail");
    for integration in integrations {
        writeln!(
            output,
            "{:<name_width$}  {:<package_width$}  {}",
            integration.name, integration.package, integration.validated_version
        )
        .expect("writing to a string cannot fail");
    }
    output
}

fn integration_status(
    integration: Option<integration_management::IntegrationId>,
    json: bool,
) -> Result<(), Box<dyn Error>> {
    let environment = integration_management::Environment::from_process();
    let snapshot = client::connect().and_then(|client| client.snapshot()).ok();
    let integrations = integration.map_or_else(
        || integration_management::IntegrationId::all().collect(),
        |integration| vec![integration],
    );
    let statuses = integrations
        .into_iter()
        .map(|integration| {
            integration_management::inspect(integration, &environment, snapshot.as_ref())
        })
        .collect::<Vec<_>>();
    if json {
        return print_json(
            CommandKey::IntegrationStatus,
            serde_json::json!({ "integrations": statuses }),
        );
    }
    print!("{}", format_integration_statuses(&statuses));
    for status in &statuses {
        if let Some(error) = status.host.error.as_deref() {
            eprintln!("warning: {} host version: {error}", status.name);
        }
        if let Some(error) = status.asset.error.as_deref() {
            eprintln!("warning: {} integration asset: {error}", status.name);
        }
    }
    Ok(())
}

fn format_integration_statuses(statuses: &[integration_management::IntegrationStatus]) -> String {
    let mut output = String::new();
    for (index, status) in statuses.iter().enumerate() {
        if index > 0 {
            output.push('\n');
        }
        writeln!(output, "{} ({})", status.display_name, status.name)
            .expect("writing to a string cannot fail");
        writeln!(output, "  {:<14}{}", "Host", status.host.state.as_str())
            .expect("writing to a string cannot fail");
        writeln!(
            output,
            "  {:<14}{}",
            "Executable",
            status.host.executable.as_deref().unwrap_or("-")
        )
        .expect("writing to a string cannot fail");
        writeln!(
            output,
            "  {:<14}{}",
            "Version",
            status.host.version.as_deref().unwrap_or("-")
        )
        .expect("writing to a string cannot fail");
        writeln!(
            output,
            "  {:<14}{}",
            "Compatibility", status.host.compatibility
        )
        .expect("writing to a string cannot fail");
        writeln!(output, "  {:<14}{}", "Asset", status.asset.state.as_str())
            .expect("writing to a string cannot fail");
        writeln!(
            output,
            "  {:<14}{}",
            "Runtime",
            format_runtime_status(&status.runtime)
        )
        .expect("writing to a string cannot fail");
        writeln!(
            output,
            "  {:<14}{}",
            "Action",
            format_recommended_action(status.recommended_action)
        )
        .expect("writing to a string cannot fail");
        writeln!(
            output,
            "  {:<14}{}",
            "Path",
            sanitize_table_cell(status.asset.path.as_deref().unwrap_or("-"))
        )
        .expect("writing to a string cannot fail");
    }
    output
}

fn format_runtime_status(status: &integration_management::RuntimeStatus) -> String {
    if status.running_processes == 0 {
        return status.state.as_str().replace('_', " ");
    }
    format!(
        "{} ({} running, {} reporting, {} untracked)",
        status.state.as_str().replace('_', " "),
        status.running_processes,
        status.tracked_processes,
        status.untracked_processes
    )
}

fn format_recommended_action(action: integration_management::RecommendedAction) -> &'static str {
    match action {
        integration_management::RecommendedAction::None => "none",
        integration_management::RecommendedAction::Install => "install integration",
        integration_management::RecommendedAction::Replace => "replace with --force",
        integration_management::RecommendedAction::RestartHost => "restart host",
        integration_management::RecommendedAction::InspectError => "inspect reported error",
    }
}

fn install_integrations(
    integrations: &[integration_management::IntegrationId],
    force: bool,
    dry_run: bool,
    json: bool,
) -> Result<(), Box<dyn Error>> {
    let environment = integration_management::Environment::from_process();
    let plans = integrations
        .iter()
        .copied()
        .map(|integration| integration_management::plan_install(integration, &environment, force))
        .collect::<Result<Vec<_>, _>>()?;
    if dry_run {
        if json {
            return print_json(
                CommandKey::IntegrationInstall,
                serde_json::json!({ "dry_run": true, "integrations": plans }),
            );
        }
        for plan in &plans {
            let spec = plan.integration.spec();
            let installation = plan.integration.installation();
            match plan.action {
                integration_management::InstallAction::Install => println!(
                    "Would install Boomux {} {} at {}",
                    spec.display_name, installation.asset_name, plan.path
                ),
                integration_management::InstallAction::Replace => println!(
                    "Would replace Boomux {} {} at {}",
                    spec.display_name, installation.asset_name, plan.path
                ),
                integration_management::InstallAction::Unchanged => println!(
                    "Boomux {} {} is already installed at {}",
                    spec.display_name, installation.asset_name, plan.path
                ),
            }
        }
        return Ok(());
    }
    let results = integrations
        .iter()
        .copied()
        .map(|integration| integration_management::install(integration, &environment, force))
        .collect::<Result<Vec<_>, _>>()?;
    if json {
        return print_json(
            CommandKey::IntegrationInstall,
            serde_json::json!({ "integrations": results }),
        );
    }
    print_integration_install_results(&results);
    Ok(())
}

fn print_integration_install_results(results: &[integration_management::InstallResult]) {
    for result in results {
        let spec = result.integration.spec();
        let installation = result.integration.installation();
        match result.result {
            integration_management::InstallOutcome::Unchanged => println!(
                "Boomux {} {} is already installed at {}",
                spec.display_name, installation.asset_name, result.path
            ),
            integration_management::InstallOutcome::Installed => println!(
                "Installed Boomux {} {} at {}",
                spec.display_name, installation.asset_name, result.path
            ),
            integration_management::InstallOutcome::Replaced => println!(
                "Replaced Boomux {} {} at {}",
                spec.display_name, installation.asset_name, result.path
            ),
        }
        if result.restart_required {
            println!("{}", installation.reload_message);
        }
    }
}

fn uninstall_integrations(
    integrations: &[integration_management::IntegrationId],
    force: bool,
    json: bool,
) -> Result<(), Box<dyn Error>> {
    let environment = integration_management::Environment::from_process();
    for integration in integrations {
        integration_management::preflight_uninstall(*integration, &environment, force)?;
    }
    let results = integrations
        .iter()
        .copied()
        .map(|integration| integration_management::uninstall(integration, &environment, force))
        .collect::<Result<Vec<_>, _>>()?;
    if json {
        return print_json(
            CommandKey::IntegrationUninstall,
            serde_json::json!({ "integrations": results }),
        );
    }
    for result in &results {
        let spec = result.integration.spec();
        let installation = result.integration.installation();
        match result.result {
            integration_management::UninstallOutcome::Removed => println!(
                "Removed Boomux {} {} from {}",
                spec.display_name, installation.asset_name, result.path
            ),
            integration_management::UninstallOutcome::NotInstalled => println!(
                "Boomux {} {} is not installed at {}",
                spec.display_name, installation.asset_name, result.path
            ),
        }
        if result.restart_required {
            println!("{}", installation.reload_message);
        }
    }
    Ok(())
}

fn setup_integration(
    integration: integration_management::IntegrationId,
    yes: bool,
    force: bool,
) -> Result<(), Box<dyn Error>> {
    let environment = integration_management::Environment::from_process();
    let snapshot = client::connect().and_then(|client| client.snapshot()).ok();
    let status = integration_management::inspect(integration, &environment, snapshot.as_ref());
    print!(
        "{}",
        format_integration_statuses(std::slice::from_ref(&status))
    );

    let spec = integration.spec();
    let installation = integration.installation();
    if status.asset.state == integration_management::AssetState::Current {
        print_setup_next_step(integration, status.runtime.state);
        return Ok(());
    }
    if status.asset.state == integration_management::AssetState::Unavailable {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            status
                .asset
                .error
                .unwrap_or_else(|| "integration asset could not be inspected".into()),
        )
        .into());
    }

    let replacing = status.asset.state == integration_management::AssetState::Modified;
    let plan = integration_management::plan_install(integration, &environment, replacing || force)?;
    let action = match plan.action {
        integration_management::InstallAction::Install => "install",
        integration_management::InstallAction::Replace => "replace",
        integration_management::InstallAction::Unchanged => "leave unchanged",
    };
    println!(
        "Plan: {action} Boomux {} at {}",
        installation.asset_name, plan.path
    );

    if yes && replacing && !force {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "--yes requires --force to replace a modified integration asset",
        )
        .into());
    }
    if !yes {
        let prompt = if replacing {
            "Replace the modified asset?"
        } else {
            "Install this integration?"
        };
        if !confirm_setup(prompt)? {
            println!("No changes made.");
            return Ok(());
        }
    }

    let result = integration_management::install(integration, &environment, replacing || force)?;
    print_integration_install_results(&[result]);
    println!(
        "After restarting {}, run: boomux integration verify {}",
        spec.display_name, spec.key
    );
    Ok(())
}

fn confirm_setup(prompt: &str) -> io::Result<bool> {
    print!("{prompt} [y/N] ");
    std::io::Write::flush(&mut io::stdout())?;
    let mut response = String::new();
    io::stdin().read_line(&mut response)?;
    Ok(is_setup_confirmation(&response))
}

fn is_setup_confirmation(response: &str) -> bool {
    matches!(response.trim().to_ascii_lowercase().as_str(), "y" | "yes")
}

fn print_setup_next_step(
    integration: integration_management::IntegrationId,
    runtime: integration_management::RuntimeState,
) {
    let spec = integration.spec();
    if runtime == integration_management::RuntimeState::Reporting {
        println!(
            "Boomux lifecycle reporting is ready for {}.",
            spec.display_name
        );
    } else {
        println!(
            "The asset is current. Restart {}, open it in a Boomux-managed shell, then run: boomux integration verify {}",
            spec.display_name, spec.key
        );
    }
}

fn capabilities(json: bool) -> Result<(), Box<dyn Error>> {
    let json_commands = json_commands().collect::<Vec<_>>();
    let features = NON_PROTOCOL_FEATURES
        .iter()
        .copied()
        .chain(protocol::protocol_capabilities())
        .collect::<Vec<_>>();
    let error_codes = [
        "invalid_argument",
        "not_found",
        "already_exists",
        "busy",
        "daemon_stopping",
        "daemon_unavailable",
        "shell_start_failed",
        "persistence_failed",
        "timeout",
        "unsupported_version",
        "cursor_expired",
        "run_changed",
        "revision_ahead",
        "idempotency_expired",
        "node_identity_unavailable",
        "node_registration_unavailable",
        "node_identity_changed",
        "revision_changed",
        "context_required",
        "ambiguous_target",
        "unsupported_integration",
        "internal",
        "unknown",
    ];
    let integration_hosts = boomux::integrations::ALL
        .iter()
        .filter_map(|descriptor| {
            let installation = descriptor.installation?;
            Some((
                descriptor.key.to_owned(),
                serde_json::json!({
                    "package": installation.package,
                    "validated_version": installation.validated_version,
                }),
            ))
        })
        .collect::<serde_json::Map<_, _>>();
    if json {
        return print_json(
            CommandKey::Capabilities,
            serde_json::json!({
                "cli_version": env!("CARGO_PKG_VERSION"),
                "daemon_protocol_version": protocol::PROTOCOL_VERSION,
                "json_schemas": [cli_output::SCHEMA],
                "json_commands": json_commands,
                "features": features,
                "integration_hosts": integration_hosts,
                "error_codes": error_codes,
            }),
        );
    }
    println!("CLI VERSION\t{}", env!("CARGO_PKG_VERSION"));
    println!("DAEMON PROTOCOL\t{}", protocol::PROTOCOL_VERSION);
    println!("JSON SCHEMAS\t{}", cli_output::SCHEMA);
    println!("JSON COMMANDS\t{}", json_commands.join(","));
    println!("FEATURES\t{}", features.join(","));
    println!(
        "INTEGRATION HOSTS\t{}",
        integration_management::IntegrationId::all()
            .map(|integration| {
                let spec = integration.spec();
                let installation = integration.installation();
                format!("{}={}", spec.key, installation.validated_version)
            })
            .collect::<Vec<_>>()
            .join(",")
    );
    println!("ERROR CODES\t{}", error_codes.join(","));
    Ok(())
}

fn list_projects(json: bool, node: Option<&str>) -> Result<(), Box<dyn Error>> {
    if let Some(node) = node {
        let client = client::connect_or_start()?;
        let registration = client.node_registration(node)?;
        let protocol::HostServiceResult::Projects { discovery } = client.route_node_host_service(
            &registration.node_id,
            protocol::HostServiceOperation::DiscoverProjects,
        )?
        else {
            return Err("remote Node returned an unexpected project response".into());
        };
        if json {
            return print_json(
                CommandKey::ProjectList,
                serde_json::json!({
                    "node_id": registration.node_id,
                    "roots_configured": discovery.roots_configured,
                    "projects": discovery.projects,
                    "warnings": discovery.warnings,
                }),
            );
        }
        if !discovery.roots_configured {
            println!(
                "No project roots configured on Node {}.",
                registration.alias
            );
        } else if discovery.projects.is_empty() {
            println!("No projects discovered on Node {}.", registration.alias);
        } else {
            println!("GROUP\tNAME\tPATH");
            for project in discovery.projects {
                println!(
                    "{}\t{}\t{}",
                    project.group,
                    project.name,
                    project.path.display()
                );
            }
        }
        for warning in discovery.warnings {
            eprintln!("warning: {warning}");
        }
        return Ok(());
    }
    let config = config::load()?;
    let roots_configured = !config.projects.roots.is_empty();
    let discovery = projects::discover(&config.projects);
    if json {
        let projects = discovery
            .projects
            .iter()
            .map(cli_output::project)
            .collect::<Vec<_>>();
        return print_json(
            CommandKey::ProjectList,
            serde_json::json!({
                "roots_configured": roots_configured,
                "projects": projects,
                "warnings": discovery.warnings,
            }),
        );
    }

    if !roots_configured {
        println!("No project roots configured.");
    } else if discovery.projects.is_empty() {
        println!("No projects discovered.");
    } else {
        println!("GROUP\tNAME\tPATH");
        for project in discovery.projects {
            println!(
                "{}\t{}\t{}",
                project.group,
                project.name,
                project.path.display()
            );
        }
    }
    for warning in discovery.warnings {
        eprintln!("warning: {warning}");
    }
    Ok(())
}

fn list_shells(json: bool) -> Result<(), Box<dyn Error>> {
    let snapshot = client::connect_or_start()?.snapshot()?;
    if json {
        let shells = snapshot
            .workspaces
            .iter()
            .flat_map(|workspace| {
                workspace
                    .shells
                    .iter()
                    .map(|shell| cli_output::shell(shell, Some(&workspace.name)))
            })
            .collect::<Vec<_>>();
        return print_json(CommandKey::List, serde_json::json!({ "shells": shells }));
    }
    println!("WORKSPACE\tNAME\tSHELL ID\tRUN ID\tSTATUS");
    for workspace in snapshot.workspaces {
        for shell in workspace.shells {
            println!(
                "{}\t{}\t{}\t{}\t{}",
                workspace.name,
                shell.name,
                shell.id,
                shell.run.as_ref().map_or("-", |run| run.id.as_str()),
                shell_status(&shell.status)
            );
        }
    }
    Ok(())
}

fn workspace_command(
    command: WorkspaceCommands,
    json: bool,
    terminal_override: Option<&str>,
) -> Result<(), Box<dyn Error>> {
    let client = client::connect_or_start()?;
    match command {
        WorkspaceCommands::List => {
            let workspaces = client.snapshot()?.workspaces;
            if json {
                let workspaces = workspaces
                    .iter()
                    .map(|workspace| {
                        let summary = agent_attention_projection::summarize_workspace(workspace);
                        cli_output::WorkspaceSummary {
                            id: workspace.id.clone(),
                            name: workspace.name.clone(),
                            shell_count: workspace.shells.len(),
                            launcher_count: workspace.launchers.len(),
                            schedule_count: workspace.schedules.len(),
                            agent_count: workspace.agents.len(),
                            agent_state_counts: summary.states,
                            attention_count: summary.attention_count,
                        }
                    })
                    .collect::<Vec<_>>();
                return print_json(
                    CommandKey::WorkspaceList,
                    serde_json::json!({ "workspaces": workspaces }),
                );
            }
            println!(
                "NAME\tWORKSPACE ID\tSHELLS\tLAUNCHERS\tSCHEDULES\tAGENTS\tBLOCKED\tDONE\tATTENTION"
            );
            for workspace in workspaces {
                let summary = agent_attention_projection::summarize_workspace(&workspace);
                println!(
                    "{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}",
                    sanitize_table_cell(&workspace.name),
                    workspace.id,
                    workspace.shells.len(),
                    workspace.launchers.len(),
                    workspace.schedules.len(),
                    workspace.agents.len(),
                    summary.states.blocked,
                    summary.states.done,
                    summary.attention_count,
                );
            }
        }
        WorkspaceCommands::Create { name, cwd } => {
            let name = cli_name(name, "workspace")?;
            let default_cwd = cwd.as_deref().map(resolve_directory).transpose()?;
            let workspace =
                client.create_workspace_with_default_cwd(name, default_cwd, Vec::new())?;
            println!("Created workspace {} ({})", workspace.name, workspace.id);
        }
        WorkspaceCommands::Open { target, node } => {
            if let Some(node) = node {
                let registration = client.node_registration(node)?;
                let workspace = match client.route_node_operation(
                    &registration.node_id,
                    protocol::RoutedOperation::GetWorkspace {
                        workspace_id: target.clone(),
                    },
                )? {
                    protocol::RoutedOperationResult::Workspace { workspace } => workspace,
                    _ => return Err("remote Node returned an unexpected workspace response".into()),
                };
                let terminal = effective_terminal(terminal_override)?;
                let mut failures = Vec::new();
                for launcher in &workspace.launchers {
                    if let Err(error) = client.route_node_host_service(
                        &registration.node_id,
                        protocol::HostServiceOperation::InvokeLauncher {
                            workspace_id: workspace.id.clone(),
                            launcher_id: launcher.id.clone(),
                        },
                    ) {
                        failures.push(format!("launcher {}: {error}", launcher.name));
                    }
                }
                for shell in workspace
                    .shells
                    .iter()
                    .filter(|shell| matches!(shell.owner, protocol::ShellOwner::User))
                {
                    if let Err(error) = terminal::open_remote(
                        terminal.as_deref(),
                        &registration.node_id,
                        &shell.id,
                        &format!(
                            "[{}] {} - {}",
                            registration.alias, workspace.name, shell.name
                        ),
                        true,
                    ) {
                        failures.push(format!("shell {}: {error}", shell.name));
                    }
                }
                if !failures.is_empty() {
                    return Err(io::Error::other(format!(
                        "remote workspace opened with failures: {}",
                        failures.join("; ")
                    ))
                    .into());
                }
                println!(
                    "Opened {} launcher(s) and {} shell(s) for {} on Node {}",
                    workspace.launchers.len(),
                    workspace_user_shell_count(&workspace),
                    workspace.name,
                    registration.alias,
                );
                return Ok(());
            }
            let snapshot = client.snapshot()?;
            let workspace = resolve_workspace_target(&snapshot.workspaces, &target)?;
            let terminal = effective_terminal(terminal_override)?;
            open_workspace(workspace, terminal.as_deref())?;
            println!(
                "Opened {} launcher(s) and {} shell(s) for {}",
                workspace.launchers.len(),
                workspace_user_shell_count(workspace),
                workspace.name
            );
        }
        WorkspaceCommands::Inspect { target } => {
            let snapshot = client.snapshot()?;
            let workspace = resolve_workspace_target(&snapshot.workspaces, &target)?;
            let agent_summary = agent_attention_projection::summarize_workspace(workspace);
            if json {
                let shells = workspace
                    .shells
                    .iter()
                    .map(|shell| cli_output::shell(shell, Some(&workspace.name)))
                    .collect::<Vec<_>>();
                return print_json(
                    CommandKey::WorkspaceInspect,
                    serde_json::json!({
                        "workspace": {
                            "id": workspace.id,
                            "name": workspace.name,
                            "default_cwd": workspace.default_cwd,
                            "shells": shells,
                            "launchers": workspace.launchers.iter()
                                .map(|launcher| cli_output::launcher(launcher, Some(&workspace.name)))
                                .collect::<Vec<_>>(),
                            "schedules": workspace.schedules.iter()
                                .map(|schedule| cli_output::schedule(schedule, Some(&workspace.name)))
                                .collect::<Vec<_>>(),
                            "agents": workspace.agents.iter()
                                .map(|agent| cli_output::agent(agent, Some(&workspace.name)))
                                .collect::<Vec<_>>(),
                            "agent_state_counts": agent_summary.states,
                            "attention_count": agent_summary.attention_count,
                        }
                    }),
                );
            }
            println!("ID\t{}", workspace.id);
            println!("NAME\t{}", workspace.name);
            println!(
                "DEFAULT CWD\t{}",
                workspace
                    .default_cwd
                    .as_deref()
                    .map_or_else(|| "-".into(), |cwd| cwd.display().to_string())
            );
            println!("SHELLS\t{}", workspace.shells.len());
            println!("LAUNCHERS\t{}", workspace.launchers.len());
            println!("SCHEDULES\t{}", workspace.schedules.len());
            println!("AGENTS\t{}", workspace.agents.len());
            println!("BLOCKED AGENTS\t{}", agent_summary.states.blocked);
            println!("COMPLETED AGENTS\t{}", agent_summary.states.done);
            println!("ATTENTION\t{}", agent_summary.attention_count);
            if !workspace.shells.is_empty() {
                println!("\nNAME\tSHELL ID\tRUN ID\tSTATUS\tCWD");
                for shell in &workspace.shells {
                    println!(
                        "{}\t{}\t{}\t{}\t{}",
                        shell.name,
                        shell.id,
                        shell.run.as_ref().map_or("-", |run| run.id.as_str()),
                        shell_status(&shell.status),
                        shell.cwd.display()
                    );
                }
            }
            if !workspace.launchers.is_empty() {
                println!("\nNAME\tLAUNCHER ID\tCWD\tCOMMAND");
                for launcher in &workspace.launchers {
                    println!(
                        "{}\t{}\t{}\t{}",
                        launcher.name,
                        launcher.id,
                        launcher.cwd.display(),
                        launcher.command.join(" ")
                    );
                }
            }
            if !workspace.schedules.is_empty() {
                println!("\nNAME\tSCHEDULE ID\tSTATE\tINTEGRATION\tCRON\tTIMEZONE");
                for schedule in &workspace.schedules {
                    println!(
                        "{}\t{}\t{}\t{}\t{}\t{}",
                        sanitize_table_cell(&schedule.name),
                        sanitize_table_cell(&schedule.id),
                        cli_output::schedule_state(schedule.state),
                        sanitize_table_cell(&schedule.integration),
                        sanitize_table_cell(&schedule.trigger.cron),
                        sanitize_table_cell(&schedule.trigger.timezone),
                    );
                }
            }
            if !workspace.agents.is_empty() {
                println!("\nNAME\tAGENT ID\tSHELL ID\tRUN ID\tSTATE\tINTEGRATION");
                for agent in &workspace.agents {
                    println!(
                        "{}\t{}\t{}\t{}\t{}\t{}",
                        agent.name,
                        agent.id,
                        agent.shell_id,
                        agent.run_id,
                        cli_output::agent_state(agent.observation.state),
                        agent.integration
                    );
                }
            }
        }
        WorkspaceCommands::Rename { target, name } => {
            let name = cli_name(name, "workspace")?;
            let snapshot = client.snapshot()?;
            let workspace = resolve_workspace_target(&snapshot.workspaces, &target)?;
            client.rename_workspace(&workspace.id, &name)?;
            println!("Renamed workspace {} to {name}", workspace.name);
        }
        WorkspaceCommands::Close { target } => {
            let snapshot = client.snapshot()?;
            let workspace = resolve_workspace_target(&snapshot.workspaces, &target)?;
            if env::var("BOOMUX_SHELL_ID")
                .ok()
                .is_some_and(|shell_id| workspace.shells.iter().any(|shell| shell.id == shell_id))
            {
                return Err(
                    "cannot close the current workspace from inside it; use the dashboard or another shell"
                        .into(),
                );
            }
            client.close_workspace(&workspace.id)?;
            println!(
                "Closed workspace {}; its schedules and persisted prompts were removed",
                workspace.name
            );
        }
    }
    Ok(())
}

fn shell_command(command: ShellCommands, json: bool) -> Result<(), Box<dyn Error>> {
    let client = client::connect_or_start()?;
    match command {
        ShellCommands::SuggestName { workspace, node } => {
            if let Some(node) = node {
                let registration = client.node_registration(node)?;
                let result = client.route_node_host_service(
                    &registration.node_id,
                    protocol::HostServiceOperation::SuggestShellName {
                        workspace_id: workspace.clone(),
                    },
                )?;
                let protocol::HostServiceResult::ShellName { workspace_id, name } = result else {
                    return Err("remote Node returned an unexpected shell-name response".into());
                };
                if json {
                    return print_json(
                        CommandKey::ShellSuggestName,
                        serde_json::json!({
                            "node_id": registration.node_id,
                            "workspace_id": workspace_id,
                            "name": name,
                        }),
                    );
                }
                println!(
                    "Suggested shell name {name} for {workspace_id} on Node {}",
                    registration.alias
                );
                println!(
                    "This suggestion is not reserved; shell creation can still fail if the name is already in use."
                );
                return Ok(());
            }
            let snapshot = client.snapshot()?;
            let workspace = resolve_workspace_target(&snapshot.workspaces, &workspace)?;
            let name =
                generated_shell_name(workspace.shells.iter().map(|shell| shell.name.as_str()))?;
            if json {
                return print_json(
                    CommandKey::ShellSuggestName,
                    serde_json::json!({
                        "workspace_id": workspace.id,
                        "name": name,
                    }),
                );
            }
            println!(
                "Suggested shell name {name} for {} ({})",
                workspace.name, workspace.id
            );
            println!(
                "This suggestion is not reserved; shell creation can still fail if the name is already in use."
            );
        }
        ShellCommands::Create {
            workspace,
            name,
            cwd,
            command,
        } => {
            let snapshot = client.snapshot()?;
            let workspace = resolve_workspace_target(&snapshot.workspaces, &workspace)?;
            let cwd = resolve_shell_cwd(
                workspace.default_cwd.as_deref(),
                cwd.as_deref(),
                Path::new("."),
            )?;
            let shell = if let Some(name) = name {
                client.create_shell(
                    &workspace.id,
                    shell_spec(cli_name(name, "shell")?, &cwd, &command),
                )?
            } else {
                create_generated_shell(&client, &workspace.id, &cwd, &command)?
            };
            println!("Created pending shell {} ({})", shell.name, shell.id);
        }
        ShellCommands::Inspect { target, workspace } => {
            let snapshot = client.snapshot()?;
            let shell = resolve_cli_shell(&snapshot, &target, workspace.as_deref())?;
            let workspace = snapshot
                .workspaces
                .iter()
                .find(|workspace| workspace.id == shell.workspace_id)
                .ok_or_else(|| {
                    cli_output::failure("not_found", "shell workspace no longer exists")
                })?;
            if json {
                return print_json(
                    CommandKey::ShellInspect,
                    serde_json::json!({
                        "shell": cli_output::shell(shell, Some(&workspace.name)),
                    }),
                );
            }
            println!("ID\t{}", shell.id);
            println!("NAME\t{}", shell.name);
            println!("WORKSPACE\t{}", workspace.name);
            println!("STATUS\t{}", shell_status(&shell.status));
            println!("CWD\t{}", shell.cwd.display());
            if let Some(run) = &shell.run {
                println!("RUN ID\t{}", run.id);
                println!("GENERATION\t{}", run.generation);
                println!("STARTED AT MS\t{}", run.started_at_ms);
                println!(
                    "ENDED AT MS\t{}",
                    run.ended_at_ms
                        .map_or_else(|| "-".into(), |value| value.to_string())
                );
                println!(
                    "EXIT REASON\t{}",
                    run.exit_reason
                        .as_ref()
                        .map_or_else(|| "-".into(), shell_exit_reason)
                );
                println!("OUTPUT REVISION\t{}", run.output_revision);
                println!("ENVIRONMENT HAS RUN ID\t{}", run.environment_has_run_id);
            }
        }
        ShellCommands::Rename {
            target,
            name,
            workspace,
        } => {
            let name = cli_name(name, "shell")?;
            let snapshot = client.snapshot()?;
            let shell = resolve_cli_shell(&snapshot, &target, workspace.as_deref())?;
            client.rename_shell(&shell.id, &name)?;
            println!("Renamed shell {} to {name}", shell.name);
        }
        ShellCommands::Close { target, workspace } => {
            close_shell_with_workspace(&client, &target, workspace.as_deref())?;
        }
    }
    Ok(())
}

fn launcher_command(command: LauncherCommands, json: bool) -> Result<(), Box<dyn Error>> {
    let client = client::connect_or_start()?;
    match command {
        LauncherCommands::List { workspace } => {
            let snapshot = client.snapshot()?;
            let workspace = resolve_workspace_target(&snapshot.workspaces, &workspace)?;
            if json {
                let launchers = workspace
                    .launchers
                    .iter()
                    .map(|launcher| cli_output::launcher(launcher, Some(&workspace.name)))
                    .collect::<Vec<_>>();
                return print_json(
                    CommandKey::LauncherList,
                    serde_json::json!({
                        "workspace_id": workspace.id,
                        "workspace_name": workspace.name,
                        "launchers": launchers,
                    }),
                );
            }
            println!("NAME\tLAUNCHER ID\tCWD\tCOMMAND");
            for launcher in &workspace.launchers {
                println!(
                    "{}\t{}\t{}\t{}",
                    launcher.name,
                    launcher.id,
                    launcher.cwd.display(),
                    launcher.command.join(" ")
                );
            }
        }
        LauncherCommands::Create {
            name,
            workspace,
            cwd,
            command,
        } => {
            let name = cli_name(name, "workspace launcher")?;
            let snapshot = client.snapshot()?;
            let workspace = resolve_workspace_target(&snapshot.workspaces, &workspace)?;
            let launcher = client.create_launcher(
                &workspace.id,
                WorkspaceLauncherSpec {
                    name,
                    cwd: resolve_directory(&cwd)?,
                    command,
                },
            )?;
            println!(
                "Created launcher {} ({}) in {}",
                launcher.name, launcher.id, workspace.name
            );
        }
        LauncherCommands::Inspect { target, workspace } => {
            let snapshot = client.snapshot()?;
            let launcher = resolve_cli_launcher(&snapshot, &target, workspace.as_deref())?;
            let workspace = snapshot
                .workspaces
                .iter()
                .find(|workspace| workspace.id == launcher.workspace_id)
                .ok_or_else(|| {
                    cli_output::failure("not_found", "launcher workspace no longer exists")
                })?;
            if json {
                return print_json(
                    CommandKey::LauncherInspect,
                    serde_json::json!({
                        "launcher": cli_output::launcher(launcher, Some(&workspace.name)),
                    }),
                );
            }
            println!("ID\t{}", launcher.id);
            println!("NAME\t{}", launcher.name);
            println!("WORKSPACE\t{}", workspace.name);
            println!("CWD\t{}", launcher.cwd.display());
            println!("COMMAND\t{}", launcher.command.join(" "));
        }
        LauncherCommands::Invoke {
            target,
            workspace,
            node,
        } => {
            if let Some(node) = node {
                let registration = client.node_registration(node)?;
                let launcher = match client.route_node_operation(
                    &registration.node_id,
                    protocol::RoutedOperation::GetLauncher {
                        launcher_id: target.clone(),
                    },
                )? {
                    protocol::RoutedOperationResult::Launcher { launcher } => launcher,
                    _ => return Err("remote Node returned an unexpected launcher response".into()),
                };
                if let Some(workspace) = workspace.as_deref()
                    && workspace != launcher.workspace_id
                {
                    return Err(cli_output::failure(
                        "invalid_argument",
                        "launcher is not owned by the requested workspace",
                    ));
                }
                let workspace_snapshot = match client.route_node_operation(
                    &registration.node_id,
                    protocol::RoutedOperation::GetWorkspace {
                        workspace_id: launcher.workspace_id.clone(),
                    },
                )? {
                    protocol::RoutedOperationResult::Workspace { workspace } => workspace,
                    _ => return Err("remote Node returned an unexpected workspace response".into()),
                };
                client.route_node_host_service(
                    &registration.node_id,
                    protocol::HostServiceOperation::InvokeLauncher {
                        workspace_id: launcher.workspace_id.clone(),
                        launcher_id: launcher.id.clone(),
                    },
                )?;
                println!(
                    "Launched {} from {} on Node {}",
                    launcher.name, workspace_snapshot.name, registration.alias
                );
                return Ok(());
            }
            let snapshot = client.snapshot()?;
            let launcher = resolve_cli_launcher(&snapshot, &target, workspace.as_deref())?;
            let workspace = snapshot
                .workspaces
                .iter()
                .find(|workspace| workspace.id == launcher.workspace_id)
                .ok_or_else(|| {
                    cli_output::failure("not_found", "launcher workspace no longer exists")
                })?;
            invoke_workspace_launcher(workspace, launcher)?;
            println!("Launched {} from {}", launcher.name, workspace.name);
        }
        LauncherCommands::Rename {
            target,
            name,
            workspace,
        } => {
            let name = cli_name(name, "workspace launcher")?;
            let snapshot = client.snapshot()?;
            let launcher = resolve_cli_launcher(&snapshot, &target, workspace.as_deref())?;
            client.rename_launcher(&launcher.id, &name)?;
            println!("Renamed launcher {} to {name}", launcher.name);
        }
        LauncherCommands::Remove { target, workspace } => {
            let snapshot = client.snapshot()?;
            let launcher = resolve_cli_launcher(&snapshot, &target, workspace.as_deref())?;
            client.remove_launcher(&launcher.id)?;
            println!("Removed launcher {}", launcher.name);
        }
    }
    Ok(())
}

fn agent_command(command: AgentCommands, json: bool) -> Result<(), Box<dyn Error>> {
    let client = client::connect_or_start()?;
    match command {
        AgentCommands::List { workspace } => {
            let snapshot = client.snapshot()?;
            let workspaces = if let Some(target) = workspace.as_deref() {
                vec![resolve_workspace_target(&snapshot.workspaces, target)?]
            } else {
                snapshot.workspaces.iter().collect()
            };
            if json {
                let agents = workspaces
                    .iter()
                    .flat_map(|workspace| {
                        workspace
                            .agents
                            .iter()
                            .map(|agent| cli_output::agent(agent, Some(&workspace.name)))
                    })
                    .collect::<Vec<_>>();
                return print_json(
                    CommandKey::AgentList,
                    serde_json::json!({ "agents": agents }),
                );
            }
            println!("WORKSPACE\tNAME\tAGENT ID\tSHELL ID\tRUN ID\tSTATE\tCONFIDENCE");
            for workspace in workspaces {
                for agent in &workspace.agents {
                    println!(
                        "{}\t{}\t{}\t{}\t{}\t{}\t{}",
                        workspace.name,
                        agent.name,
                        agent.id,
                        agent.shell_id,
                        agent.run_id,
                        cli_output::agent_state(agent.observation.state),
                        agent.observation.confidence
                    );
                }
            }
        }
        AgentCommands::Inspect { agent_id } => {
            let agent = client.get_agent(agent_id)?;
            let workspace = client.get_workspace(&agent.workspace_id)?;
            if json {
                return print_json(
                    CommandKey::AgentInspect,
                    serde_json::json!({
                        "agent": cli_output::agent(&agent, Some(&workspace.name)),
                    }),
                );
            }
            print_agent(&agent, &workspace.name);
        }
        AgentCommands::Wait {
            agent_id,
            after_revision,
            wait_ms,
        } => {
            let waited = client.wait_agent(agent_id, after_revision, wait_ms)?;
            if json {
                let workspace = client.get_workspace(&waited.agent.workspace_id)?;
                return print_json(
                    CommandKey::AgentWait,
                    serde_json::json!({
                        "changed": waited.changed,
                        "agent": cli_output::agent(&waited.agent, Some(&workspace.name)),
                    }),
                );
            }
            println!("CHANGED\t{}", waited.changed);
            println!("ID\t{}", waited.agent.id);
            println!(
                "STATE\t{}",
                cli_output::agent_state(waited.agent.observation.state)
            );
            println!("REVISION\t{}", waited.agent.observation.revision);
        }
        AgentCommands::Register(arguments) => {
            register_or_ensure_agent(&client, arguments, json, false)?;
        }
        AgentCommands::Ensure(arguments) => {
            register_or_ensure_agent(&client, arguments, json, true)?;
        }
        AgentCommands::Supervise(_) => unreachable!(),
        AgentCommands::Report {
            agent_id,
            shell_id,
            run_id,
            state,
            authority,
            evidence,
            confidence,
        } => {
            let (shell_id, run_id) = resolve_agent_context(
                shell_id,
                run_id,
                env::var("BOOMUX_SHELL_ID").ok(),
                env::var("BOOMUX_RUN_ID").ok(),
            )?;
            let existing = client.get_agent(&agent_id)?;
            if existing.shell_id != shell_id {
                return Err(cli_output::failure(
                    "invalid_argument",
                    format!("agent {agent_id} is not bound to shell {shell_id}"),
                ));
            }
            if existing.run_id != run_id {
                return Err(cli_output::failure(
                    "run_changed",
                    format!("agent {agent_id} is not bound to run {run_id}"),
                ));
            }
            let agent = client.report_agent(
                agent_id,
                run_id,
                AgentReport {
                    state: state.into(),
                    authority: authority.into(),
                    evidence,
                    confidence,
                },
            )?;
            if json {
                let workspace = client.get_workspace(&agent.workspace_id)?;
                return print_json(
                    CommandKey::AgentReport,
                    serde_json::json!({
                        "agent": cli_output::agent(&agent, Some(&workspace.name)),
                    }),
                );
            }
            println!(
                "Reported {} for agent {} (revision {})",
                cli_output::agent_state(agent.observation.state),
                agent.id,
                agent.observation.revision
            );
        }
    }
    Ok(())
}

fn attention_command(command: AttentionCommands, json: bool) -> Result<(), Box<dyn Error>> {
    let client = client::connect_or_start()?;
    validate_attention_protocol(client.protocol_version()?)?;
    match command {
        AttentionCommands::List { workspace } => {
            let snapshot = client.snapshot()?;
            let workspace_id = workspace
                .as_deref()
                .map(|target| resolve_workspace_target(&snapshot.workspaces, target))
                .transpose()?
                .map(|workspace| workspace.id.as_str());
            let items = agent_attention_projection::project_attention(&snapshot.workspaces)
                .into_iter()
                .filter(|item| workspace_id.is_none_or(|id| item.workspace_id == id))
                .collect::<Vec<_>>();
            if json {
                let attention = items
                    .iter()
                    .map(|item| {
                        serde_json::json!({
                            "workspace_id": item.workspace_id,
                            "workspace_name": item.workspace_name,
                            "reason": agent_attention_projection::attention_reason(item.attention.reason),
                            "observation": {
                                "revision": item.attention.observation.revision,
                                "state": cli_output::agent_state(item.attention.observation.state),
                                "authority": cli_output::agent_authority(item.attention.observation.authority),
                                "evidence": item.attention.observation.evidence,
                                "confidence": item.attention.observation.confidence,
                                "observed_at_ms": item.attention.observation.observed_at_ms,
                            },
                            "observation_is_current": item.observation_is_current,
                            "shell_is_retained": item.shell_is_retained,
                            "agent": cli_output::agent(&item.agent, Some(&item.workspace_name)),
                        })
                    })
                    .collect::<Vec<_>>();
                return print_json(
                    CommandKey::AttentionList,
                    serde_json::json!({ "attention": attention }),
                );
            }
            println!(
                "WORKSPACE\tREASON\tAGENT\tAGENT ID\tREVISION\tCURRENT\tSHELL\tAUTHORITY\tEVIDENCE"
            );
            for item in items {
                println!(
                    "{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}",
                    sanitize_table_cell(&item.workspace_name),
                    agent_attention_projection::attention_reason(item.attention.reason),
                    sanitize_table_cell(&item.agent.name),
                    item.agent.id,
                    item.attention.observation.revision,
                    item.observation_is_current,
                    if item.shell_is_retained {
                        "retained"
                    } else {
                        "removed"
                    },
                    cli_output::agent_authority(item.attention.observation.authority),
                    sanitize_table_cell(&item.attention.observation.evidence),
                );
            }
        }
        AttentionCommands::Acknowledge {
            agent_id,
            observation_revision,
        } => {
            let acknowledged =
                client.acknowledge_agent_attention(agent_id, observation_revision)?;
            if json {
                let workspace = client.get_workspace(&acknowledged.agent.workspace_id)?;
                return print_json(
                    CommandKey::AttentionAcknowledge,
                    serde_json::json!({
                        "changed": acknowledged.changed,
                        "agent": cli_output::agent(&acknowledged.agent, Some(&workspace.name)),
                    }),
                );
            }
            println!("CHANGED\t{}", acknowledged.changed);
            println!("AGENT ID\t{}", acknowledged.agent.id);
            println!("OBSERVATION REVISION\t{observation_revision}");
        }
    }
    Ok(())
}

fn session_command(command: SessionCommands, json: bool) -> Result<(), Box<dyn Error>> {
    let client = client::connect_or_start()?;
    validate_session_protocol(client.protocol_version()?)?;
    let selected_node = match &command {
        SessionCommands::List { node, .. }
        | SessionCommands::Inspect { node, .. }
        | SessionCommands::Resume { node, .. } => node.as_deref(),
    };
    if let Some(node) = selected_node {
        let registration = client.node_registration(node)?;
        return remote_session_command(&client, &registration, command, json);
    }
    let snapshot = client.snapshot()?;
    match command {
        SessionCommands::List { workspace, .. } => {
            let selected_workspace = workspace
                .as_deref()
                .map(|target| resolve_workspace_target(&snapshot.workspaces, target))
                .transpose()?;
            let catalog = selected_workspace.map_or_else(
                || discover_host_catalog(&snapshot.workspaces),
                |workspace| discover_host_catalog(std::slice::from_ref(workspace)),
            );
            let sessions =
                session_projection::project_snapshot_with_catalog(&snapshot, Some(&catalog));
            let workspace_id = selected_workspace.map(|workspace| workspace.id.as_str());
            let sessions = sessions
                .iter()
                .filter(|session| {
                    workspace_id.is_none_or(|workspace_id| session.workspace_id == workspace_id)
                })
                .collect::<Vec<_>>();
            if json {
                let sessions = sessions
                    .iter()
                    .map(|session| cli_output::session_summary(session))
                    .collect::<Vec<_>>();
                return print_json(
                    CommandKey::SessionList,
                    serde_json::json!({ "sessions": sessions }),
                );
            }
            println!(
                "WORKSPACE\tDESCRIPTION\tSESSION ID\tINTEGRATION\tSTATE\tLAST ACTIVITY MS\tOCCURRENCES"
            );
            for session in sessions {
                println!(
                    "{}\t{}\t{}\t{}\t{} ({})\t{}\t{}",
                    session.workspace_name,
                    session.description,
                    session.id,
                    session.integration,
                    cli_output::agent_state(session.state),
                    if session.state_is_current {
                        "current"
                    } else {
                        "last-known"
                    },
                    session.last_at_ms,
                    session.occurrences.len()
                );
            }
        }
        SessionCommands::Inspect { session_id, .. } => {
            let catalog = discover_host_catalog(&snapshot.workspaces);
            let sessions =
                session_projection::project_snapshot_with_catalog(&snapshot, Some(&catalog));
            let session = match session_projection::resolve_exact(&sessions, &session_id) {
                Ok(session) => session,
                Err(session_projection::ResolveError::NotFound) => {
                    return Err(cli_output::failure(
                        "not_found",
                        format!("session not found: {session_id}"),
                    ));
                }
                Err(session_projection::ResolveError::DuplicateId) => {
                    return Err(cli_output::failure(
                        "internal",
                        format!("duplicate projected session ID: {session_id}"),
                    ));
                }
            };
            if json {
                return print_json(
                    CommandKey::SessionInspect,
                    serde_json::json!({ "session": cli_output::session(session) }),
                );
            }
            print_session(session);
        }
        SessionCommands::Resume { session_id, .. } => {
            let catalog = discover_host_catalog(&snapshot.workspaces);
            let sessions =
                session_projection::project_snapshot_with_catalog(&snapshot, Some(&catalog));
            let session =
                session_projection::resolve_exact(&sessions, &session_id).map_err(|_| {
                    cli_output::failure("not_found", format!("session not found: {session_id}"))
                })?;
            let (cwd, command) = dashboard_session_resume_plan(session)
                .map_err(|message| cli_output::failure("invalid_argument", message))?;
            let terminal = effective_terminal(None)?;
            terminal::open_command(
                terminal.as_deref(),
                &cwd,
                &format!(
                    "{} - {} session",
                    session.workspace_name, session.integration
                ),
                &command,
            )?;
            println!("Opened exact {} Agent Session", session.integration);
        }
    }
    Ok(())
}

fn remote_session_command(
    client: &client::Client,
    registration: &protocol::NodeRegistrationSnapshot,
    command: SessionCommands,
    json: bool,
) -> Result<(), Box<dyn Error>> {
    match command {
        SessionCommands::List { workspace, .. } => {
            let result = client.route_node_host_service(
                &registration.node_id,
                protocol::HostServiceOperation::ListAgentSessions {
                    workspace_id: workspace,
                },
            )?;
            let protocol::HostServiceResult::AgentSessions { sessions } = result else {
                return Err("remote Node returned an unexpected session-list response".into());
            };
            if json {
                return print_json(
                    CommandKey::SessionList,
                    serde_json::json!({ "node_id": registration.node_id, "sessions": sessions }),
                );
            }
            println!(
                "WORKSPACE\tDESCRIPTION\tSESSION ID\tINTEGRATION\tSTATE\tLAST ACTIVITY MS\tOCCURRENCES"
            );
            for session in sessions {
                println!(
                    "{}\t{}\t{}\t{}\t{} ({})\t{}\t{}",
                    session.workspace_name,
                    session.description,
                    session.id,
                    session.integration,
                    cli_output::agent_state(session.state),
                    if session.state_is_current {
                        "current"
                    } else {
                        "last-known"
                    },
                    session.last_at_ms,
                    session.occurrence_count,
                );
            }
        }
        SessionCommands::Inspect { session_id, .. } => {
            let result = client.route_node_host_service(
                &registration.node_id,
                protocol::HostServiceOperation::InspectAgentSession { session_id },
            )?;
            let protocol::HostServiceResult::AgentSession { session } = result else {
                return Err("remote Node returned an unexpected session response".into());
            };
            if json {
                let mut value = serde_json::to_value(&session.summary)?;
                let object = value
                    .as_object_mut()
                    .expect("session summary serializes as object");
                object.insert(
                    "source_cwd".into(),
                    serde_json::to_value(&session.source_cwd)?,
                );
                object.insert(
                    "occurrences".into(),
                    serde_json::to_value(&session.occurrences)?,
                );
                return print_json(
                    CommandKey::SessionInspect,
                    serde_json::json!({ "node_id": registration.node_id, "session": value }),
                );
            }
            println!("NODE\t{}", registration.alias);
            println!("ID\t{}", session.summary.id);
            println!("WORKSPACE\t{}", session.summary.workspace_name);
            println!("DESCRIPTION\t{}", session.summary.description);
            println!("INTEGRATION\t{}", session.summary.integration);
            println!("OCCURRENCES\t{}", session.summary.occurrence_count);
        }
        SessionCommands::Resume { session_id, .. } => {
            let result = client.route_node_host_service(
                &registration.node_id,
                protocol::HostServiceOperation::InspectAgentSession {
                    session_id: session_id.clone(),
                },
            )?;
            let protocol::HostServiceResult::AgentSession { session } = result else {
                return Err("remote Node returned an unexpected session response".into());
            };
            let terminal = effective_terminal(None)?;
            terminal::open_agent_session(
                terminal.as_deref(),
                Some(&registration.node_id),
                &session_id,
                &format!(
                    "[{}] {} - {} session",
                    registration.alias, session.summary.workspace_name, session.summary.integration,
                ),
            )?;
            println!(
                "Opened exact {} Agent Session on Node {}",
                session.summary.integration, registration.alias
            );
        }
    }
    Ok(())
}

fn schedule_command(command: ScheduleCommands, json: bool) -> Result<(), Box<dyn Error>> {
    let client = client::connect_or_start()?;
    if let Some(node) = command.node().map(str::to_owned) {
        return remote_schedule_command(&client, command, &node, json);
    }
    validate_schedule_protocol(client.protocol_version()?)?;
    match command {
        ScheduleCommands::Create(arguments) => create_schedule(&client, *arguments, json),
        ScheduleCommands::List { workspace, .. } => {
            let snapshot = client.snapshot()?;
            let selected = workspace
                .as_deref()
                .map(|target| resolve_workspace_target(&snapshot.workspaces, target))
                .transpose()?;
            let schedules = snapshot
                .workspaces
                .iter()
                .filter(|candidate| selected.is_none_or(|selected| candidate.id == selected.id))
                .flat_map(|workspace| {
                    workspace
                        .schedules
                        .iter()
                        .map(move |schedule| (schedule, workspace.name.as_str()))
                })
                .collect::<Vec<_>>();
            if json {
                let schedules = schedules
                    .iter()
                    .map(|(schedule, workspace_name)| {
                        cli_output::schedule(schedule, Some(workspace_name))
                    })
                    .collect::<Vec<_>>();
                return print_json(
                    CommandKey::ScheduleList,
                    serde_json::json!({ "schedules": schedules }),
                );
            }
            println!(
                "WORKSPACE\tNAME\tSCHEDULE ID\tSTATE\tINTEGRATION\tTRIGGER\tTIMEZONE\tSESSION\tNEXT OCCURRENCE MS"
            );
            for (schedule, workspace_name) in schedules {
                println!(
                    "{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}",
                    sanitize_table_cell(workspace_name),
                    sanitize_table_cell(&schedule.name),
                    sanitize_table_cell(&schedule.id),
                    cli_output::schedule_state(schedule.state),
                    sanitize_table_cell(&schedule.integration),
                    sanitize_table_cell(&schedule.trigger.cron),
                    sanitize_table_cell(&schedule.trigger.timezone),
                    sanitize_table_cell(&schedule_session_label(&schedule.session)),
                    schedule
                        .next_occurrence
                        .as_ref()
                        .map(|occurrence| occurrence.scheduled_at_ms.to_string())
                        .as_deref()
                        .unwrap_or("-"),
                );
            }
            if snapshot
                .scheduler
                .as_ref()
                .is_some_and(|scheduler| scheduler.state == protocol::SchedulerState::Offline)
            {
                eprintln!(
                    "Scheduler is offline: next occurrences are projections only. Run `boomux daemon status` and `boomux doctor`; fix configuration or environment and run `boomux daemon restart` before relying on timed dispatch."
                );
            }
            Ok(())
        }
        ScheduleCommands::Inspect {
            target, workspace, ..
        } => {
            let snapshot = client.snapshot()?;
            let schedule = resolve_cli_schedule(&snapshot, &target, workspace.as_deref())?;
            let workspace_name = schedule_workspace_name(&snapshot, schedule);
            let inspection = client.get_agent_schedule(&schedule.id)?;
            if json {
                return print_json(
                    CommandKey::ScheduleInspect,
                    serde_json::json!({
                        "schedule": cli_output::schedule_inspection(
                            &inspection.schedule,
                            workspace_name,
                            &inspection.prompt,
                        ),
                    }),
                );
            }
            print_schedule_inspection(&inspection.schedule, workspace_name, &inspection.prompt);
            Ok(())
        }
        ScheduleCommands::Pause {
            target, workspace, ..
        } => {
            let snapshot = client.snapshot()?;
            let schedule = resolve_cli_schedule(&snapshot, &target, workspace.as_deref())?;
            let workspace_name = schedule_workspace_name(&snapshot, schedule).map(str::to_owned);
            let schedule = client.pause_agent_schedule(&schedule.id)?;
            print_schedule_mutation(
                CommandKey::SchedulePause,
                &schedule,
                workspace_name.as_deref(),
                json,
            )?;
            if !json {
                println!("Paused schedule {}", sanitize_table_cell(&schedule.name));
            }
            Ok(())
        }
        ScheduleCommands::Resume {
            target, workspace, ..
        } => {
            let snapshot = client.snapshot()?;
            let schedule = resolve_cli_schedule(&snapshot, &target, workspace.as_deref())?;
            let workspace_name = schedule_workspace_name(&snapshot, schedule).map(str::to_owned);
            let schedule = client.resume_agent_schedule(&schedule.id)?;
            print_schedule_mutation(
                CommandKey::ScheduleResume,
                &schedule,
                workspace_name.as_deref(),
                json,
            )?;
            if !json {
                println!(
                    "Enabled schedule {} for future timed dispatch; use schedule run for explicit run-now work",
                    sanitize_table_cell(&schedule.name)
                );
            }
            Ok(())
        }
        ScheduleCommands::Remove {
            target, workspace, ..
        } => {
            let snapshot = client.snapshot()?;
            let schedule = resolve_cli_schedule(&snapshot, &target, workspace.as_deref())?;
            let removed =
                cli_output::schedule(schedule, schedule_workspace_name(&snapshot, schedule));
            let name = schedule.name.clone();
            client.remove_agent_schedule(&schedule.id)?;
            if json {
                return print_json(
                    CommandKey::ScheduleRemove,
                    serde_json::json!({ "removed": true, "schedule": removed }),
                );
            }
            println!(
                "Removed schedule {} and its persisted prompt",
                sanitize_table_cell(&name)
            );
            Ok(())
        }
        ScheduleCommands::Run {
            target,
            workspace,
            idempotency_key,
            ..
        } => {
            let feature = protocol::ProtocolFeature::ScheduledExecutions;
            if !feature.is_supported_by(client.protocol_version()?) {
                return Err(cli_output::failure(
                    "unsupported_version",
                    format!(
                        "scheduled execution dispatch requires daemon protocol {}",
                        feature.minimum_version()
                    ),
                ));
            }
            let snapshot = client.snapshot()?;
            let schedule = resolve_cli_schedule(&snapshot, &target, workspace.as_deref())?;
            let execution = client.run_agent_schedule(
                &schedule.id,
                idempotency_key.unwrap_or_else(Uuid::new_v4).to_string(),
            )?;
            print_execution(CommandKey::ScheduleRun, &execution, json)
        }
    }
}

fn remote_node_projection(
    client: &client::Client,
    selector: &str,
) -> Result<(protocol::NodeRegistrationSnapshot, protocol::CombinedNode), Box<dyn Error>> {
    let registration = client.node_registration(selector)?;
    let mut nodes = client
        .combined_node_snapshot(Some(registration.node_id.clone()))?
        .nodes;
    let node = nodes.pop().ok_or_else(|| {
        cli_output::failure(
            "not_found",
            "registered Node has no combined projection entry",
        )
    })?;
    Ok((registration, node))
}

fn remote_workspace_from_projection(
    client: &client::Client,
    node: &protocol::CombinedNode,
    target: &str,
) -> Result<protocol::WorkspaceSnapshot, Box<dyn Error>> {
    let projection = node.remote_projection.as_ref().ok_or_else(|| {
        cli_output::failure("not_found", "remote Node has no projected workspace state")
    })?;
    let matches = projection
        .workspaces
        .iter()
        .filter(|workspace| workspace.id == target || workspace.name == target)
        .collect::<Vec<_>>();
    let [workspace] = matches.as_slice() else {
        return Err(cli_output::failure(
            if matches.is_empty() {
                "not_found"
            } else {
                "ambiguous_target"
            },
            format!("remote workspace target did not resolve uniquely: {target}"),
        ));
    };
    match client.route_node_operation(
        &node.node_id,
        protocol::RoutedOperation::GetWorkspace {
            workspace_id: workspace.id.clone(),
        },
    )? {
        protocol::RoutedOperationResult::Workspace { workspace } => Ok(workspace),
        _ => Err("remote Node returned an unexpected workspace response".into()),
    }
}

fn remote_schedule_inspection(
    client: &client::Client,
    node: &protocol::CombinedNode,
    target: &str,
    workspace: Option<&str>,
) -> Result<protocol::AgentScheduleInspection, Box<dyn Error>> {
    let schedule_id = if uuid::Uuid::parse_str(target).is_ok() {
        target.to_owned()
    } else {
        let workspace = workspace.ok_or_else(|| {
            cli_output::failure(
                "invalid_argument",
                "remote schedule names require --workspace; exact schedule IDs do not",
            )
        })?;
        let workspace = remote_workspace_from_projection(client, node, workspace)?;
        let matches = workspace
            .schedules
            .iter()
            .filter(|schedule| schedule.name == target)
            .collect::<Vec<_>>();
        let [schedule] = matches.as_slice() else {
            return Err(cli_output::failure(
                if matches.is_empty() {
                    "not_found"
                } else {
                    "ambiguous_target"
                },
                format!("remote schedule target did not resolve uniquely: {target}"),
            ));
        };
        schedule.id.clone()
    };
    match client.route_node_operation(
        &node.node_id,
        protocol::RoutedOperation::GetAgentSchedule { schedule_id },
    )? {
        protocol::RoutedOperationResult::AgentScheduleInspection { inspection } => Ok(inspection),
        _ => Err("remote Node returned an unexpected schedule response".into()),
    }
}

fn remote_schedule_command(
    client: &client::Client,
    command: ScheduleCommands,
    selector: &str,
    json: bool,
) -> Result<(), Box<dyn Error>> {
    let feature = protocol::ProtocolFeature::RemoteSchedules;
    if !client.supports(feature)? {
        return Err(cli_output::failure(
            "unsupported_version",
            format!(
                "remote Schedule management requires local daemon protocol {}",
                feature.minimum_version()
            ),
        ));
    }
    let (registration, node) = remote_node_projection(client, selector)?;
    match command {
        ScheduleCommands::List { workspace, .. } => {
            let projection = node.remote_projection.as_ref().ok_or_else(|| {
                cli_output::failure("not_found", "remote Node has no projected Schedule state")
            })?;
            let workspace_id = workspace
                .as_deref()
                .map(|target| remote_workspace_from_projection(client, &node, target))
                .transpose()?
                .map(|workspace| workspace.id);
            let schedules = projection
                .schedules
                .iter()
                .filter(|schedule| {
                    workspace_id
                        .as_deref()
                        .is_none_or(|workspace_id| schedule.workspace_id == workspace_id)
                })
                .map(|schedule| {
                    let workspace_name = projection
                        .workspaces
                        .iter()
                        .find(|workspace| workspace.id == schedule.workspace_id)
                        .map(|workspace| workspace.name.as_str());
                    serde_json::json!({
                        "node_id": node.node_id,
                        "id": schedule.id,
                        "workspace_id": schedule.workspace_id,
                        "workspace_name": workspace_name,
                        "name": schedule.name,
                        "integration": schedule.integration,
                        "cron": schedule.trigger.cron,
                        "timezone": schedule.trigger.timezone,
                        "state": cli_output::schedule_state(schedule.state),
                        "revision": schedule.revision,
                        "prompt_revision": schedule.prompt_revision,
                        "trigger_revision": schedule.trigger_revision,
                        "created_at_ms": schedule.created_at_ms,
                        "updated_at_ms": schedule.updated_at_ms,
                        "next_occurrence": schedule.next_occurrence,
                    })
                })
                .collect::<Vec<_>>();
            if json {
                return print_json(
                    CommandKey::ScheduleList,
                    serde_json::json!({
                        "node_id": registration.node_id,
                        "node_alias": registration.alias,
                        "health": node.health,
                        "current": node.current,
                        "stale": node.stale,
                        "observed_at_ms": node.observed_at_ms,
                        "scheduler": node.scheduler,
                        "schedules": schedules,
                    }),
                );
            }
            println!(
                "NODE\tWORKSPACE\tNAME\tSCHEDULE ID\tSTATE\tINTEGRATION\tTRIGGER\tTIMEZONE\tNEXT OCCURRENCE MS"
            );
            for schedule in &projection.schedules {
                if workspace_id
                    .as_deref()
                    .is_some_and(|workspace_id| schedule.workspace_id != workspace_id)
                {
                    continue;
                }
                let workspace_name = projection
                    .workspaces
                    .iter()
                    .find(|workspace| workspace.id == schedule.workspace_id)
                    .map_or("unknown", |workspace| workspace.name.as_str());
                println!(
                    "{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}",
                    registration.alias,
                    workspace_name,
                    schedule.name,
                    schedule.id,
                    cli_output::schedule_state(schedule.state),
                    schedule.integration,
                    schedule.trigger.cron,
                    schedule.trigger.timezone,
                    schedule
                        .next_occurrence
                        .as_ref()
                        .map(|value| value.scheduled_at_ms.to_string())
                        .unwrap_or_else(|| "-".into()),
                );
            }
            println!(
                "Node {} is {:?}; scheduler {:?} ({}/{} active)",
                registration.alias,
                node.health,
                node.scheduler.state,
                node.scheduler.active_executions,
                node.scheduler.max_concurrent
            );
            Ok(())
        }
        ScheduleCommands::Create(arguments) => {
            let workspace = remote_workspace_from_projection(client, &node, &arguments.workspace)?;
            let name = cli_name(arguments.name, "schedule")?;
            let resolved = client.route_node_host_service(
                &node.node_id,
                protocol::HostServiceOperation::ResolveDirectory {
                    path: arguments.cwd,
                },
            )?;
            let protocol::HostServiceResult::Directory { path: cwd } = resolved else {
                return Err("remote Node returned an unexpected directory response".into());
            };
            let prompt = schedule_prompt(arguments.prompt, arguments.prompt_file.as_deref())?;
            let cron = schedule_cron(
                arguments.cron.as_deref(),
                arguments.every.as_deref(),
                arguments.daily.as_deref(),
                arguments.weekdays.as_deref(),
                arguments.weekly.as_deref(),
            )?;
            let timezone = arguments
                .timezone
                .as_deref()
                .map(boomux::scheduling::canonicalize_timezone)
                .transpose()?
                .map_or_else(boomux::scheduling::resolve_system_timezone, Ok)?;
            let integration = schedule_integration(&arguments.integration)?;
            let session = if let Some(session_id) = arguments.continue_session.as_deref() {
                let result = client.route_node_host_service(
                    &node.node_id,
                    protocol::HostServiceOperation::InspectAgentSession {
                        session_id: session_id.to_owned(),
                    },
                )?;
                let protocol::HostServiceResult::AgentSession { session } = result else {
                    return Err("remote Node returned an unexpected Agent Session response".into());
                };
                if session.summary.workspace_id != workspace.id
                    || session.summary.integration != integration.key
                {
                    return Err(cli_output::failure(
                        "invalid_argument",
                        "continued session must belong to the selected owner Workspace and integration",
                    ));
                }
                let external_session_id = session.summary.external_session_id.ok_or_else(|| {
                    cli_output::failure(
                        "invalid_argument",
                        "continued projected session has no canonical external session ID",
                    )
                })?;
                boomux::scheduling::validate_external_session_id(&external_session_id)?;
                AgentScheduleSession::Continue {
                    external_session_id,
                }
            } else {
                AgentScheduleSession::Fresh
            };
            let spec = AgentScheduleSpec {
                name,
                cwd,
                integration: integration.key.into(),
                prompt,
                session,
                trigger: AgentScheduleTrigger { cron, timezone },
                state: if arguments.enabled {
                    AgentScheduleState::Enabled
                } else {
                    AgentScheduleState::Paused
                },
                overlap_policy: AgentScheduleOverlapPolicy::Skip,
            };
            let schedule = match client.route_node_operation(
                &node.node_id,
                protocol::RoutedOperation::CreateAgentSchedule {
                    workspace_id: workspace.id.clone(),
                    spec,
                },
            )? {
                protocol::RoutedOperationResult::AgentSchedule { schedule } => schedule,
                _ => return Err("remote Node returned an unexpected create response".into()),
            };
            if json {
                return print_json(
                    CommandKey::ScheduleCreate,
                    serde_json::json!({
                        "node_id": registration.node_id,
                        "schedule": cli_output::schedule(&schedule, Some(&workspace.name)),
                    }),
                );
            }
            println!(
                "Created {} schedule {} ({}) on Node {}",
                cli_output::schedule_state(schedule.state),
                schedule.name,
                schedule.id,
                registration.alias
            );
            Ok(())
        }
        ScheduleCommands::Inspect {
            target, workspace, ..
        } => {
            let inspection =
                remote_schedule_inspection(client, &node, &target, workspace.as_deref())?;
            if json {
                return print_json(
                    CommandKey::ScheduleInspect,
                    serde_json::json!({
                        "node_id": registration.node_id,
                        "schedule": cli_output::schedule_inspection(
                            &inspection.schedule,
                            None,
                            &inspection.prompt,
                        ),
                    }),
                );
            }
            println!("Node: {}", registration.alias);
            print_schedule_inspection(&inspection.schedule, None, &inspection.prompt);
            Ok(())
        }
        ScheduleCommands::Pause {
            ref target,
            ref workspace,
            ..
        }
        | ScheduleCommands::Resume {
            ref target,
            ref workspace,
            ..
        }
        | ScheduleCommands::Remove {
            ref target,
            ref workspace,
            ..
        }
        | ScheduleCommands::Run {
            ref target,
            ref workspace,
            ..
        } => {
            let inspection =
                remote_schedule_inspection(client, &node, target, workspace.as_deref())?;
            let (key, operation) = match command {
                ScheduleCommands::Pause { .. } => (
                    CommandKey::SchedulePause,
                    protocol::RoutedOperation::PauseAgentSchedule {
                        schedule_id: inspection.schedule.id.clone(),
                        expected_revision: inspection.schedule.revision,
                    },
                ),
                ScheduleCommands::Resume { .. } => (
                    CommandKey::ScheduleResume,
                    protocol::RoutedOperation::ResumeAgentSchedule {
                        schedule_id: inspection.schedule.id.clone(),
                        expected_revision: inspection.schedule.revision,
                    },
                ),
                ScheduleCommands::Remove { .. } => (
                    CommandKey::ScheduleRemove,
                    protocol::RoutedOperation::RemoveAgentSchedule {
                        schedule_id: inspection.schedule.id.clone(),
                        expected_revision: inspection.schedule.revision,
                    },
                ),
                ScheduleCommands::Run {
                    idempotency_key, ..
                } => (
                    CommandKey::ScheduleRun,
                    protocol::RoutedOperation::RunAgentSchedule {
                        schedule_id: inspection.schedule.id.clone(),
                        dispatch_key: idempotency_key.unwrap_or_else(Uuid::new_v4).to_string(),
                    },
                ),
                _ => unreachable!(),
            };
            let result = client.route_node_operation(&node.node_id, operation)?;
            match result {
                protocol::RoutedOperationResult::AgentSchedule { schedule } => {
                    print_schedule_mutation(key, &schedule, None, json)
                }
                protocol::RoutedOperationResult::ScheduledExecution { execution, .. } => {
                    if json {
                        print_json(
                            key,
                            serde_json::json!({
                                "node_id": registration.node_id,
                                "execution": cli_output::execution(&execution),
                            }),
                        )
                    } else {
                        print_execution(key, &execution, false)
                    }
                }
                protocol::RoutedOperationResult::Ok if key == CommandKey::ScheduleRemove => {
                    if json {
                        print_json(
                            key,
                            serde_json::json!({
                                "node_id": registration.node_id,
                                "removed": true,
                                "schedule": cli_output::schedule(&inspection.schedule, None),
                            }),
                        )
                    } else {
                        println!(
                            "Removed schedule {} on Node {}",
                            inspection.schedule.name, registration.alias
                        );
                        Ok(())
                    }
                }
                _ => Err("remote Node returned an unexpected Schedule response".into()),
            }
        }
    }
}

fn execution_command(
    command: ExecutionCommands,
    json: bool,
    terminal: Option<&str>,
) -> Result<(), Box<dyn Error>> {
    let client = client::connect_or_start()?;
    if let Some(node) = command.node().map(str::to_owned) {
        return remote_execution_command(&client, command, &node, json, terminal);
    }
    let feature = protocol::ProtocolFeature::ScheduledExecutions;
    if !feature.is_supported_by(client.protocol_version()?) {
        return Err(cli_output::failure(
            "unsupported_version",
            format!(
                "scheduled execution inspection requires daemon protocol {}",
                feature.minimum_version()
            ),
        ));
    }
    match command {
        ExecutionCommands::List {
            workspace,
            schedule,
            limit,
            ..
        } => {
            let snapshot = client.snapshot()?;
            let workspace = workspace
                .as_deref()
                .map(|target| resolve_workspace_target(&snapshot.workspaces, target))
                .transpose()?;
            let schedule_id = schedule
                .as_deref()
                .map(|target| {
                    resolve_cli_schedule(
                        &snapshot,
                        target,
                        workspace.map(|workspace| workspace.id.as_str()),
                    )
                    .map(|schedule| schedule.id.clone())
                })
                .transpose()?;
            let page = client.scheduled_execution_page(
                workspace.map(|workspace| workspace.id.clone()),
                schedule_id,
                limit,
            )?;
            if json {
                let executions = page
                    .executions
                    .iter()
                    .map(cli_output::execution)
                    .collect::<Vec<_>>();
                return print_json(
                    CommandKey::ExecutionList,
                    serde_json::json!({
                        "executions": executions,
                        "limit": page.limit,
                        "truncated": page.truncated,
                        "schedule_limit": page.schedule_limit,
                        "schedules_truncated": page.schedules_truncated,
                        "schedules": page.schedules.iter().map(|projection| serde_json::json!({
                            "schedule_id": projection.schedule_id,
                            "next_occurrence": projection.next_occurrence,
                        })).collect::<Vec<_>>(),
                    }),
                );
            }
            println!(
                "STATE\tREASON/OUTCOME\tEXECUTION ID\tSCHEDULE ID\tREQUESTED\tAGENT ID\tSHELL/RUN"
            );
            for execution in page.executions {
                println!(
                    "{}\t{}\t{}\t{}\t{}\t{}\t{}/{}",
                    execution_state(&execution),
                    execution_result_label(&execution),
                    execution.id,
                    execution.schedule_id,
                    execution.requested_at_ms,
                    execution.agent_id.as_deref().unwrap_or("-"),
                    execution.shell_id.as_deref().unwrap_or("-"),
                    execution.run_id.as_deref().unwrap_or("-"),
                );
                if let Some(action) = execution_action(&execution) {
                    println!("ACTION {}\t{}", execution.id, action);
                }
            }
            if page.truncated {
                println!(
                    "Showing newest {} executions; increase --limit to see more retained history.",
                    page.limit
                );
            }
            if page.schedules_truncated {
                println!(
                    "Showing {} schedule projections; narrow by workspace or schedule for the remainder.",
                    page.schedule_limit
                );
            }
            for projection in page.schedules {
                println!(
                    "Next occurrence for schedule {}: {}",
                    projection.schedule_id,
                    projection
                        .next_occurrence
                        .map(|occurrence| occurrence.scheduled_at_ms.to_string())
                        .unwrap_or_else(
                            || "none (paused or scheduler projection unavailable)".into()
                        )
                );
            }
            Ok(())
        }
        ExecutionCommands::Inspect { execution_id, .. } => {
            let inspection = client.inspect_scheduled_execution(execution_id)?;
            if json {
                return print_json(
                    CommandKey::ExecutionInspect,
                    serde_json::json!({
                        "execution": cli_output::execution(&inspection.execution),
                        "next_occurrence": inspection.next_occurrence,
                    }),
                );
            }
            print_execution(CommandKey::ExecutionInspect, &inspection.execution, false)?;
            println!(
                "Next occurrence: {}",
                inspection
                    .next_occurrence
                    .map(|occurrence| occurrence.scheduled_at_ms.to_string())
                    .unwrap_or_else(|| "none (paused or scheduler projection unavailable)".into())
            );
            Ok(())
        }
        ExecutionCommands::Wait {
            execution_id,
            after_revision,
            wait_ms,
            ..
        } => {
            let waited = client.wait_scheduled_execution(execution_id, after_revision, wait_ms)?;
            if json {
                return print_json(
                    CommandKey::ExecutionWait,
                    serde_json::json!({
                        "changed": waited.changed,
                        "execution": cli_output::execution(&waited.execution),
                    }),
                );
            }
            println!("Changed: {}", waited.changed);
            print_execution(CommandKey::ExecutionWait, &waited.execution, false)
        }
        ExecutionCommands::Open { execution_id, .. } => {
            let opened = open_scheduled_execution(&client, &execution_id, terminal)?;
            if json {
                return print_json(
                    CommandKey::ExecutionOpen,
                    serde_json::json!({
                        "execution": cli_output::execution(&opened.execution),
                        "target": opened.target,
                    }),
                );
            }
            println!("{}", opened.message);
            Ok(())
        }
        ExecutionCommands::Cancel { execution_id, .. } => {
            let execution = client.cancel_scheduled_execution(execution_id)?;
            print_execution(CommandKey::ExecutionCancel, &execution, json)
        }
    }
}

fn remote_execution_command(
    client: &client::Client,
    command: ExecutionCommands,
    selector: &str,
    json: bool,
    terminal: Option<&str>,
) -> Result<(), Box<dyn Error>> {
    let feature = protocol::ProtocolFeature::RemoteSchedules;
    if !client.supports(feature)? {
        return Err(cli_output::failure(
            "unsupported_version",
            format!(
                "remote Scheduled Execution management requires local daemon protocol {}",
                feature.minimum_version()
            ),
        ));
    }
    let (registration, node) = remote_node_projection(client, selector)?;
    match command {
        ExecutionCommands::List {
            workspace,
            schedule,
            limit,
            ..
        } => {
            let workspace = workspace
                .as_deref()
                .map(|target| remote_workspace_from_projection(client, &node, target))
                .transpose()?;
            let schedule_id = schedule
                .as_deref()
                .map(|target| {
                    remote_schedule_inspection(
                        client,
                        &node,
                        target,
                        workspace.as_ref().map(|workspace| workspace.id.as_str()),
                    )
                    .map(|inspection| inspection.schedule.id)
                })
                .transpose()?;
            let result = client.route_node_operation(
                &node.node_id,
                protocol::RoutedOperation::ListScheduledExecutions {
                    workspace_id: workspace.map(|workspace| workspace.id),
                    schedule_id,
                    limit,
                },
            )?;
            let protocol::RoutedOperationResult::ScheduledExecutions {
                executions,
                limit,
                truncated,
                schedules,
                schedule_limit,
                schedules_truncated,
            } = result
            else {
                return Err("remote Node returned an unexpected execution list response".into());
            };
            if json {
                return print_json(
                    CommandKey::ExecutionList,
                    serde_json::json!({
                        "node_id": registration.node_id,
                        "executions": executions.iter().map(cli_output::execution).collect::<Vec<_>>(),
                        "limit": limit,
                        "truncated": truncated,
                        "schedule_limit": schedule_limit,
                        "schedules_truncated": schedules_truncated,
                        "schedules": schedules,
                    }),
                );
            }
            println!(
                "NODE\tSTATE\tREASON/OUTCOME\tEXECUTION ID\tSCHEDULE ID\tREQUESTED\tAGENT ID\tSHELL/RUN"
            );
            for execution in executions {
                println!(
                    "{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}/{}",
                    registration.alias,
                    execution_state(&execution),
                    execution_result_label(&execution),
                    execution.id,
                    execution.schedule_id,
                    execution.requested_at_ms,
                    execution.agent_id.as_deref().unwrap_or("-"),
                    execution.shell_id.as_deref().unwrap_or("-"),
                    execution.run_id.as_deref().unwrap_or("-"),
                );
            }
            Ok(())
        }
        ExecutionCommands::Inspect { execution_id, .. } => {
            let execution = routed_remote_execution(client, &node.node_id, &execution_id)?;
            if json {
                return print_json(
                    CommandKey::ExecutionInspect,
                    serde_json::json!({
                        "node_id": registration.node_id,
                        "execution": cli_output::execution(&execution),
                    }),
                );
            }
            println!("Node: {}", registration.alias);
            print_execution(CommandKey::ExecutionInspect, &execution, false)
        }
        ExecutionCommands::Wait {
            execution_id,
            after_revision,
            wait_ms,
            ..
        } => {
            let result = client.route_node_operation(
                &node.node_id,
                protocol::RoutedOperation::WaitScheduledExecution {
                    execution_id,
                    after_revision,
                    wait_ms,
                },
            )?;
            let protocol::RoutedOperationResult::ScheduledExecutionWait { execution, changed } =
                result
            else {
                return Err("remote Node returned an unexpected execution wait response".into());
            };
            if json {
                return print_json(
                    CommandKey::ExecutionWait,
                    serde_json::json!({
                        "node_id": registration.node_id,
                        "changed": changed,
                        "execution": cli_output::execution(&execution),
                    }),
                );
            }
            println!("Node: {}", registration.alias);
            println!("Changed: {changed}");
            print_execution(CommandKey::ExecutionWait, &execution, false)
        }
        ExecutionCommands::Cancel { execution_id, .. } => {
            let execution = routed_remote_execution(client, &node.node_id, &execution_id)?;
            let result = client.route_node_operation(
                &node.node_id,
                protocol::RoutedOperation::CancelScheduledExecution {
                    execution_id,
                    expected_revision: execution.revision,
                },
            )?;
            let protocol::RoutedOperationResult::ScheduledExecution { execution, .. } = result
            else {
                return Err("remote Node returned an unexpected cancellation response".into());
            };
            if json {
                return print_json(
                    CommandKey::ExecutionCancel,
                    serde_json::json!({
                        "node_id": registration.node_id,
                        "execution": cli_output::execution(&execution),
                    }),
                );
            }
            println!("Node: {}", registration.alias);
            print_execution(CommandKey::ExecutionCancel, &execution, false)
        }
        ExecutionCommands::Open { execution_id, .. } => {
            let opened = open_remote_scheduled_execution(
                client,
                &protocol::QualifiedIdentity::new(&node.node_id, execution_id),
                terminal,
            )?;
            if json {
                return print_json(
                    CommandKey::ExecutionOpen,
                    serde_json::json!({
                        "node_id": registration.node_id,
                        "execution": cli_output::execution(&opened.execution),
                        "target": opened.target,
                    }),
                );
            }
            println!("{}", opened.message);
            Ok(())
        }
    }
}

fn routed_remote_execution(
    client: &client::Client,
    node_id: &str,
    execution_id: &str,
) -> Result<ScheduledExecutionSnapshot, Box<dyn Error>> {
    match client.route_node_operation(
        node_id,
        protocol::RoutedOperation::GetScheduledExecution {
            execution_id: execution_id.to_owned(),
        },
    )? {
        protocol::RoutedOperationResult::ScheduledExecution { execution, .. } => Ok(execution),
        _ => Err("remote Node returned an unexpected execution response".into()),
    }
}

fn print_execution(
    command: CommandKey,
    execution: &ScheduledExecutionSnapshot,
    json: bool,
) -> Result<(), Box<dyn Error>> {
    if json {
        return print_json(
            command,
            serde_json::json!({ "execution": cli_output::execution(execution) }),
        );
    }
    println!("Execution {}", execution.id);
    println!("State: {}", execution_state(execution));
    println!("Revision: {}", execution.revision);
    println!("Schedule: {}", execution.schedule_id);
    println!("Dispatch key: {}", execution.dispatch_key);
    println!("Result: {}", execution_result_label(execution));
    if let Some(action) = execution_action(execution) {
        println!("Action: {action}");
    }
    println!("Requested at ms: {}", execution.requested_at_ms);
    if let Some(started_at_ms) = execution.started_at_ms {
        println!("Started at ms: {started_at_ms}");
    }
    if let Some(ended_at_ms) = execution.ended_at_ms {
        println!("Ended at ms: {ended_at_ms}");
    }
    if let Some(shell_id) = &execution.shell_id {
        println!("Shell: {shell_id}");
    }
    if let Some(run_id) = &execution.run_id {
        println!("Run: {run_id}");
    }
    if let Some(agent_id) = &execution.agent_id {
        println!("Agent: {agent_id} (inspect with `boomux agent inspect {agent_id}`)");
    }
    if execution.state == protocol::ScheduledExecutionState::Active {
        println!("No automatic timeout; cancel explicitly if this work is blocked or hung.");
    }
    Ok(())
}

fn execution_result_label(execution: &ScheduledExecutionSnapshot) -> String {
    if let Some(reason) = execution.reason {
        return format!("reason:{}", cli_output::execution_reason(reason));
    }
    match execution.outcome {
        Some(ScheduledExecutionOutcome::ExitCode { code }) => format!("exit_code:{code}"),
        Some(ScheduledExecutionOutcome::Signal { signal }) => format!("signal:{signal}"),
        None => "-".into(),
    }
}

fn execution_action(execution: &ScheduledExecutionSnapshot) -> Option<String> {
    use protocol::ScheduledExecutionReason::*;
    let action = match execution.reason? {
        Overlap => format!(
            "inspect active work with `boomux execution list --schedule {}` and cancel the exact execution if authorized",
            execution.schedule_id
        ),
        ActiveSession => "inspect `boomux attention list` and the linked Agent/session before retrying with `boomux schedule run`".into(),
        WorkspaceCapacity | GlobalCapacity => "run `boomux execution list` to find active work; cancel only an exact authorized execution before retrying".into(),
        Missed => "timed work is not caught up; check `boomux daemon status` and use `boomux schedule run` only for an authorized manual replacement".into(),
        PausedRace => "inspect the schedule and run `boomux schedule resume <schedule> --workspace <workspace>` if future timed work is authorized".into(),
        InvalidTarget => format!(
            "run `boomux integration status {}` and `boomux doctor`, then restart the daemon after fixing the target",
            execution.integration
        ),
        RunnerStartFailed | HostSpawnFailed => "run `boomux doctor` and `boomux daemon status`; fix daemon startup environment or integration setup, then `boomux daemon restart`".into(),
        ColdDaemonRecovery => "the prior process cannot be resumed automatically; inspect this execution and `boomux daemon status` before an authorized `boomux schedule run`".into(),
        RunnerExitedWithoutReport => "inspect the retained shell/run and `boomux doctor` before retrying manually".into(),
        CancelledByUser => "no retry is automatic; use `boomux schedule run` only if a new execution is authorized".into(),
        DaemonShutdown => "restart the daemon and use `boomux schedule run` only if a replacement execution is authorized".into(),
    };
    Some(action)
}

fn execution_state(execution: &ScheduledExecutionSnapshot) -> &'static str {
    use protocol::ScheduledExecutionState::*;
    match execution.state {
        Skipped => "skipped",
        Claimed => "claimed",
        Starting => "starting",
        Active => "active",
        DispatchFailed => "dispatch_failed",
        Exited => "exited",
        Cancelled => "cancelled",
        Interrupted => "interrupted",
    }
}

fn scheduled_runner(schedule_id: &str) -> Result<process_adapter::ProcessExit, Box<dyn Error>> {
    let shell_id = env::var("BOOMUX_SHELL_ID")
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "BOOMUX_SHELL_ID is required"))?;
    let run_id = env::var("BOOMUX_RUN_ID")
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "BOOMUX_RUN_ID is required"))?;
    let runner_token = env::var("BOOMUX_SCHEDULE_RUNNER_TOKEN").map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "BOOMUX_SCHEDULE_RUNNER_TOKEN is required",
        )
    })?;
    let claim = retry_runner_request(|client| {
        client.resolve_scheduled_execution_claim(schedule_id, &shell_id, &run_id, &runner_token)
    })?;
    let descriptor = boomux::integrations::by_key(&claim.execution.integration)
        .and_then(|descriptor| descriptor.schedule_dispatch)
        .ok_or_else(|| io::Error::other("scheduled integration dispatch is unavailable"))?;
    let dispatch = descriptor
        .command(
            &claim.execution.integration,
            &claim.execution.session,
            &claim.prompt,
        )
        .ok_or_else(|| io::Error::other("scheduled integration mode is unavailable"))?;
    let mut command = Command::new(&dispatch.argv[0]);
    command
        .args(&dispatch.argv[1..])
        .current_dir(&claim.execution.cwd)
        .env_remove("BOOMUX_SCHEDULE_RUNNER_TOKEN")
        .stdin(if dispatch.stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::inherit()
        })
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => {
            let _ = retry_runner_request(|client| {
                client.report_scheduled_runner(
                    &claim.execution.id,
                    &shell_id,
                    &run_id,
                    &runner_token,
                    ScheduledRunnerResult::SpawnFailed,
                )
            });
            return Err(io::Error::new(
                error.kind(),
                format!("could not start scheduled host: {error}"),
            )
            .into());
        }
    };
    if let Some(bytes) = dispatch.stdin {
        let mut stdin = child
            .stdin
            .take()
            .ok_or_else(|| io::Error::other("scheduled host stdin is unavailable"))?;
        if let Err(error) = std::io::Write::write_all(&mut stdin, &bytes) {
            drop(stdin);
            let _ = child.kill();
            let _ = child.wait();
            let _ = retry_runner_request(|client| {
                client.report_scheduled_runner(
                    &claim.execution.id,
                    &shell_id,
                    &run_id,
                    &runner_token,
                    ScheduledRunnerResult::SpawnFailed,
                )
            });
            return Err(io::Error::new(
                error.kind(),
                format!("could not write scheduled host prompt: {error}"),
            )
            .into());
        }
        drop(stdin);
    }
    if let Err(error) = retry_runner_request(|client| {
        client.report_scheduled_runner(
            &claim.execution.id,
            &shell_id,
            &run_id,
            &runner_token,
            ScheduledRunnerResult::Active,
        )
    }) {
        let _ = child.kill();
        let _ = child.wait();
        return Err(error);
    }
    let status = child.wait()?;
    let (exit, outcome) = if let Some(code) = status.code() {
        (
            process_adapter::ProcessExit::Code(code),
            ScheduledExecutionOutcome::ExitCode { code },
        )
    } else {
        let signal = status.signal().unwrap_or(0);
        (
            process_adapter::ProcessExit::Signal(signal),
            ScheduledExecutionOutcome::Signal { signal },
        )
    };
    retry_runner_request(|client| {
        client.report_scheduled_runner(
            &claim.execution.id,
            &shell_id,
            &run_id,
            &runner_token,
            ScheduledRunnerResult::Exited {
                outcome: outcome.clone(),
            },
        )
    })?;
    Ok(exit)
}

fn retry_runner_request<T>(
    mut request: impl FnMut(&client::Client) -> client::Result<T>,
) -> Result<T, Box<dyn Error>> {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        match client::connect().and_then(|client| request(&client)) {
            Ok(value) => return Ok(value),
            Err(error) if Instant::now() >= deadline => return Err(Box::new(error)),
            Err(_) => {}
        }
        thread::sleep(Duration::from_millis(25));
    }
}

fn validate_schedule_protocol(negotiated: u32) -> Result<(), Box<dyn Error>> {
    let feature = protocol::ProtocolFeature::AgentSchedules;
    feature
        .is_supported_by(negotiated)
        .then_some(())
        .ok_or_else(|| {
            cli_output::failure(
                "unsupported_version",
                format!(
                    "Agent schedule management requires daemon protocol {}; negotiated {negotiated}",
                    feature.minimum_version()
                ),
            )
        })
}

fn create_schedule(
    client: &client::Client,
    arguments: ScheduleCreateArgs,
    json: bool,
) -> Result<(), Box<dyn Error>> {
    let snapshot = client.snapshot()?;
    let workspace = resolve_workspace_target(&snapshot.workspaces, &arguments.workspace)?;
    let name = cli_name(arguments.name, "schedule")?;
    let cwd = resolve_directory(&arguments.cwd).map_err(|error| {
        cli_output::failure(
            "invalid_argument",
            format!("schedule working directory is invalid: {error}"),
        )
    })?;
    let prompt = schedule_prompt(arguments.prompt, arguments.prompt_file.as_deref())?;
    let cron = schedule_cron(
        arguments.cron.as_deref(),
        arguments.every.as_deref(),
        arguments.daily.as_deref(),
        arguments.weekdays.as_deref(),
        arguments.weekly.as_deref(),
    )?;
    let timezone = arguments
        .timezone
        .as_deref()
        .map(boomux::scheduling::canonicalize_timezone)
        .transpose()?
        .map_or_else(boomux::scheduling::resolve_system_timezone, Ok)?;
    let integration = schedule_integration(&arguments.integration)?;
    let session = if let Some(session_id) = arguments.continue_session.as_deref() {
        let catalog = discover_host_catalog(std::slice::from_ref(workspace));
        let sessions = session_projection::project_workspaces_with_catalog(
            std::slice::from_ref(workspace),
            Some(&catalog),
        );
        let session = session_projection::resolve_exact(&sessions, session_id).map_err(
            |error| match error {
                session_projection::ResolveError::NotFound => cli_output::failure(
                    "not_found",
                    format!(
                        "projected session not found in workspace {}: {session_id}",
                        workspace.name
                    ),
                ),
                session_projection::ResolveError::DuplicateId => cli_output::failure(
                    "internal",
                    format!("duplicate projected session ID: {session_id}"),
                ),
            },
        )?;
        if session.integration != integration.key {
            return Err(cli_output::failure(
                "invalid_argument",
                "continued session integration does not match --integration",
            ));
        }
        let external_session_id = session.external_session_id.as_deref().ok_or_else(|| {
            cli_output::failure(
                "invalid_argument",
                "continued projected session has no canonical external session ID",
            )
        })?;
        boomux::scheduling::validate_external_session_id(external_session_id)?;
        if !integration
            .schedule_dispatch
            .is_some_and(|dispatch| dispatch.continuation)
        {
            return Err(cli_output::failure(
                "unsupported_integration",
                "integration does not support continuation schedule dispatch",
            ));
        }
        AgentScheduleSession::Continue {
            external_session_id: external_session_id.to_owned(),
        }
    } else {
        if !integration
            .schedule_dispatch
            .is_some_and(|dispatch| dispatch.fresh)
        {
            return Err(cli_output::failure(
                "unsupported_integration",
                "integration does not support fresh schedule dispatch",
            ));
        }
        AgentScheduleSession::Fresh
    };
    let spec = AgentScheduleSpec {
        name,
        cwd,
        integration: integration.key.to_owned(),
        prompt,
        session,
        trigger: AgentScheduleTrigger { cron, timezone },
        state: if arguments.enabled {
            AgentScheduleState::Enabled
        } else {
            AgentScheduleState::Paused
        },
        overlap_policy: AgentScheduleOverlapPolicy::Skip,
    };
    let schedule = client.create_agent_schedule(&workspace.id, spec)?;
    if json {
        return print_json(
            CommandKey::ScheduleCreate,
            serde_json::json!({
                "schedule": cli_output::schedule(&schedule, Some(&workspace.name)),
            }),
        );
    }
    println!(
        "Created {} schedule {} ({})",
        cli_output::schedule_state(schedule.state),
        sanitize_table_cell(&schedule.name),
        sanitize_table_cell(&schedule.id)
    );
    if schedule.state == AgentScheduleState::Enabled {
        println!("Timed dispatch is not available yet; use schedule run for explicit run-now work");
    }
    Ok(())
}

fn schedule_prompt(inline: Option<String>, file: Option<&Path>) -> Result<String, Box<dyn Error>> {
    let prompt = match (inline, file) {
        (Some(prompt), None) => prompt,
        (None, Some(path)) => {
            let metadata = fs::symlink_metadata(path).map_err(|error| {
                io::Error::new(error.kind(), format!("cannot inspect prompt file: {error}"))
            })?;
            if !metadata.file_type().is_file() {
                return Err(cli_output::failure(
                    "invalid_argument",
                    "prompt file must be a regular file and not a symlink",
                ));
            }
            let mut file = fs::OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
                .open(path)
                .map_err(|_| {
                    cli_output::failure(
                        "invalid_argument",
                        "prompt file changed or could not be opened safely",
                    )
                })?;
            if !file.metadata()?.file_type().is_file() {
                return Err(cli_output::failure(
                    "invalid_argument",
                    "prompt file changed before it could be read safely",
                ));
            }
            let mut bytes = Vec::new();
            file.by_ref()
                .take((boomux::scheduling::MAX_PROMPT_BYTES + 1) as u64)
                .read_to_end(&mut bytes)?;
            if bytes.len() > boomux::scheduling::MAX_PROMPT_BYTES {
                return Err(cli_output::failure(
                    "invalid_argument",
                    format!(
                        "prompt must be at most {} bytes",
                        boomux::scheduling::MAX_PROMPT_BYTES
                    ),
                ));
            }
            String::from_utf8(bytes).map_err(|_| {
                cli_output::failure("invalid_argument", "prompt file must contain valid UTF-8")
            })?
        }
        _ => {
            return Err(cli_output::failure(
                "invalid_argument",
                "exactly one prompt source is required",
            ));
        }
    };
    boomux::scheduling::validate_prompt(&prompt)?;
    Ok(prompt)
}

fn schedule_cron(
    cron: Option<&str>,
    every: Option<&str>,
    daily: Option<&str>,
    weekdays: Option<&str>,
    weekly: Option<&str>,
) -> Result<String, Box<dyn Error>> {
    let expression = if let Some(cron) = cron {
        boomux::scheduling::canonicalize_cron(cron)?
    } else if let Some(every) = every {
        let (amount, unit) = every.split_at(every.len().saturating_sub(1));
        let amount = amount
            .parse::<u8>()
            .map_err(|_| cli_output::failure("invalid_argument", "--every must use Nm or Nh"))?;
        match unit {
            "m" => boomux::scheduling::every_minutes_cron(amount)?,
            "h" => boomux::scheduling::every_hours_cron(amount)?,
            _ => {
                return Err(cli_output::failure(
                    "invalid_argument",
                    "--every must use Nm or Nh",
                ));
            }
        }
    } else if let Some(time) = daily {
        let (hour, minute) = schedule_time(time)?;
        boomux::scheduling::daily_cron(hour, minute)?
    } else if let Some(time) = weekdays {
        let (hour, minute) = schedule_time(time)?;
        boomux::scheduling::weekdays_cron(hour, minute)?
    } else if let Some(value) = weekly {
        let (day, time) = value.split_once('@').ok_or_else(|| {
            cli_output::failure("invalid_argument", "--weekly must use DAY@HH:MM")
        })?;
        let (hour, minute) = schedule_time(time)?;
        boomux::scheduling::weekly_cron(day, hour, minute)?
    } else {
        return Err(cli_output::failure(
            "invalid_argument",
            "exactly one trigger source is required",
        ));
    };
    Ok(expression)
}

fn schedule_time(value: &str) -> Result<(u8, u8), Box<dyn Error>> {
    let bytes = value.as_bytes();
    if bytes.len() != 5
        || bytes[2] != b':'
        || !bytes[..2].iter().chain(&bytes[3..]).all(u8::is_ascii_digit)
    {
        return Err(cli_output::failure(
            "invalid_argument",
            "schedule time must use HH:MM",
        ));
    }
    Ok((value[..2].parse()?, value[3..].parse()?))
}

fn schedule_integration(
    key: &str,
) -> Result<&'static boomux::integrations::IntegrationDescriptor, Box<dyn Error>> {
    boomux::scheduling::validate_integration_key(key)?;
    boomux::integrations::by_key(key)
        .filter(|integration| integration.schedule_dispatch.is_some())
        .ok_or_else(|| {
            cli_output::failure(
                "unsupported_integration",
                "integration does not support schedule dispatch",
            )
        })
}

fn print_schedule_mutation(
    command: CommandKey,
    schedule: &AgentScheduleSnapshot,
    workspace_name: Option<&str>,
    json: bool,
) -> Result<(), Box<dyn Error>> {
    if json {
        print_json(
            command,
            serde_json::json!({
                "schedule": cli_output::schedule(schedule, workspace_name),
            }),
        )?;
    }
    Ok(())
}

fn print_schedule_inspection(
    schedule: &AgentScheduleSnapshot,
    workspace_name: Option<&str>,
    prompt: &str,
) {
    println!("ID\t{}", sanitize_table_cell(&schedule.id));
    println!("NAME\t{}", sanitize_table_cell(&schedule.name));
    println!(
        "WORKSPACE\t{}",
        sanitize_table_cell(workspace_name.unwrap_or("-"))
    );
    println!("STATE\t{}", cli_output::schedule_state(schedule.state));
    println!(
        "CWD\t{}",
        sanitize_table_cell(&schedule.cwd.display().to_string())
    );
    println!(
        "INTEGRATION\t{}",
        sanitize_table_cell(&schedule.integration)
    );
    println!(
        "SESSION\t{}",
        sanitize_table_cell(&schedule_session_label(&schedule.session))
    );
    println!("CRON\t{}", sanitize_table_cell(&schedule.trigger.cron));
    println!(
        "TIMEZONE\t{}",
        sanitize_table_cell(&schedule.trigger.timezone)
    );
    println!("OVERLAP POLICY\tskip");
    println!("REVISION\t{}", schedule.revision);
    println!("PROMPT REVISION\t{}", schedule.prompt_revision);
    println!("TRIGGER REVISION\t{}", schedule.trigger_revision);
    println!(
        "NEXT OCCURRENCE MS\t{}",
        schedule
            .next_occurrence
            .as_ref()
            .map(|occurrence| occurrence.scheduled_at_ms.to_string())
            .as_deref()
            .unwrap_or("-")
    );
    println!("CREATED AT MS\t{}", schedule.created_at_ms);
    println!("UPDATED AT MS\t{}", schedule.updated_at_ms);
    println!(
        "EVALUATION FRONTIER MS\t{}",
        schedule.evaluation_frontier_ms
    );
    println!(
        "EXECUTION SHELL ID\t{}",
        sanitize_table_cell(schedule.execution_shell_id.as_deref().unwrap_or("-"))
    );
    println!(
        "PROMPT (PRIVATE; ESCAPED FOR TERMINAL SAFETY)\n{}",
        escape_terminal_text(prompt)
    );
}

fn escape_terminal_text(value: &str) -> String {
    value
        .chars()
        .flat_map(|character| {
            if is_bidi_control(character) {
                character.escape_unicode().collect::<Vec<_>>()
            } else {
                character.escape_debug().collect::<Vec<_>>()
            }
        })
        .collect()
}

fn is_bidi_control(character: char) -> bool {
    matches!(
        character,
        '\u{061c}' | '\u{200e}' | '\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}'
    )
}

fn schedule_session_label(session: &AgentScheduleSession) -> String {
    match session {
        AgentScheduleSession::Fresh => "fresh".into(),
        AgentScheduleSession::Continue {
            external_session_id,
        } => format!("continue:{external_session_id}"),
    }
}

fn resolve_cli_schedule<'a>(
    snapshot: &'a Snapshot,
    target: &str,
    workspace: Option<&str>,
) -> Result<&'a AgentScheduleSnapshot, Box<dyn Error>> {
    if let Some(schedule) = find_schedule(snapshot, target) {
        return Ok(schedule);
    }
    let workspace_id = if let Some(workspace) = workspace {
        resolve_workspace_target(&snapshot.workspaces, workspace)?
            .id
            .clone()
    } else {
        env::var("BOOMUX_WORKSPACE_ID").map_err(|_| {
            cli_output::failure(
                "context_required",
                format!("schedule name {target:?} requires --workspace or BOOMUX_WORKSPACE_ID"),
            )
        })?
    };
    let workspace = snapshot
        .workspaces
        .iter()
        .find(|workspace| workspace.id == workspace_id)
        .ok_or_else(|| cli_output::failure("not_found", "current workspace no longer exists"))?;
    workspace
        .schedules
        .iter()
        .find(|schedule| schedule.name == target)
        .ok_or_else(|| {
            cli_output::failure(
                "not_found",
                format!(
                    "schedule {target:?} was not found in workspace {}",
                    workspace.name
                ),
            )
        })
}

fn find_schedule<'a>(snapshot: &'a Snapshot, id: &str) -> Option<&'a AgentScheduleSnapshot> {
    snapshot
        .workspaces
        .iter()
        .flat_map(|workspace| &workspace.schedules)
        .find(|schedule| schedule.id == id)
}

fn schedule_workspace_name<'a>(
    snapshot: &'a Snapshot,
    schedule: &AgentScheduleSnapshot,
) -> Option<&'a str> {
    snapshot
        .workspaces
        .iter()
        .find(|workspace| workspace.id == schedule.workspace_id)
        .map(|workspace| workspace.name.as_str())
}

fn validate_attention_protocol(negotiated: u32) -> Result<(), Box<dyn Error>> {
    let feature = protocol::ProtocolFeature::PersistentAgentAttention;
    feature
        .is_supported_by(negotiated)
        .then_some(())
        .ok_or_else(|| {
            cli_output::failure(
                "unsupported_version",
                format!(
                    "Agent attention requires daemon protocol {}; negotiated {negotiated}",
                    feature.minimum_version()
                ),
            )
        })
}

fn sanitize_table_cell(value: &str) -> String {
    value
        .chars()
        .map(|character| {
            if character.is_control() {
                ' '
            } else {
                character
            }
        })
        .collect()
}

fn validate_session_protocol(negotiated: u32) -> Result<(), Box<dyn Error>> {
    let feature = protocol::ProtocolFeature::InactiveAgentState;
    feature
        .is_supported_by(negotiated)
        .then_some(())
        .ok_or_else(|| {
            cli_output::failure(
                "unsupported_version",
                format!(
                    "session projection requires daemon protocol {}; negotiated {negotiated}",
                    feature.minimum_version()
                ),
            )
        })
}

fn print_session(session: &session_projection::SessionProjection) {
    println!("ID\t{}", session.id);
    println!("WORKSPACE ID\t{}", session.workspace_id);
    println!("WORKSPACE\t{}", session.workspace_name);
    println!("DESCRIPTION\t{}", session.description);
    println!("INTEGRATION\t{}", session.integration);
    println!(
        "EXTERNAL SESSION ID\t{}",
        session.external_session_id.as_deref().unwrap_or("-")
    );
    println!(
        "STATE\t{} ({})",
        cli_output::agent_state(session.state),
        if session.state_is_current {
            "current"
        } else {
            "last-known"
        }
    );
    println!("STARTED AT MS\t{}", session.started_at_ms);
    println!("LAST ACTIVITY MS\t{}", session.last_at_ms);
    println!(
        "SOURCE CWD\t{}",
        session
            .source_cwd
            .as_ref()
            .map_or_else(|| "-".into(), |cwd| cwd.display().to_string())
    );
    println!("OCCURRENCES\t{}", session.occurrences.len());
    for (index, occurrence) in session.occurrences.iter().enumerate() {
        println!();
        println!("OCCURRENCE {}", index + 1);
        println!("AGENT ID\t{}", occurrence.agent_id);
        println!("SHELL ID\t{}", occurrence.shell_id);
        println!(
            "RETAINED SHELL NAME\t{}",
            occurrence.retained_shell_name.as_deref().unwrap_or("-")
        );
        println!(
            "RETAINED SHELL CWD\t{}",
            occurrence
                .retained_shell_cwd
                .as_ref()
                .map_or_else(|| "-".into(), |cwd| cwd.display().to_string())
        );
        println!(
            "SOURCE CWD\t{}",
            occurrence
                .source_cwd
                .as_ref()
                .map_or_else(|| "-".into(), |cwd| cwd.display().to_string())
        );
        println!("RUN ID\t{}", occurrence.run_id);
        println!("STARTED AT MS\t{}", occurrence.started_at_ms);
        println!(
            "ENDED AT MS\t{}",
            occurrence
                .ended_at_ms
                .map_or_else(|| "-".into(), |ended| ended.to_string())
        );
        println!("CURRENT\t{}", occurrence.is_current);
        println!("OBSERVATION REVISION\t{}", occurrence.observation.revision);
        println!(
            "OBSERVATION STATE\t{}",
            cli_output::agent_state(occurrence.observation.state)
        );
        println!(
            "OBSERVATION AUTHORITY\t{}",
            cli_output::agent_authority(occurrence.observation.authority)
        );
        println!("OBSERVATION EVIDENCE\t{}", occurrence.observation.evidence);
        println!(
            "OBSERVATION CONFIDENCE\t{}",
            occurrence.observation.confidence
        );
        println!("OBSERVED AT MS\t{}", occurrence.observation.observed_at_ms);
    }
}

fn supervise_agent(
    arguments: AgentSuperviseArgs,
) -> Result<process_adapter::ProcessExit, Box<dyn Error>> {
    let (shell_id, run_id) = resolve_agent_context(
        arguments.shell_id,
        arguments.run_id,
        env::var("BOOMUX_SHELL_ID").ok(),
        env::var("BOOMUX_RUN_ID").ok(),
    )?;
    Ok(process_adapter::supervise(
        process_adapter::SuperviseSpec {
            name: cli_name_or_generated(arguments.name, "agent")?,
            integration: arguments.integration,
            external_session_id: arguments.external_session_id,
            shell_id,
            run_id,
            command: arguments.command,
        },
    )?)
}

fn register_or_ensure_agent(
    client: &client::Client,
    arguments: AgentRegistrationArgs,
    json: bool,
    ensure: bool,
) -> Result<(), Box<dyn Error>> {
    let (shell_id, run_id) = resolve_agent_context(
        arguments.shell_id,
        arguments.run_id,
        env::var("BOOMUX_SHELL_ID").ok(),
        env::var("BOOMUX_RUN_ID").ok(),
    )?;
    let spec = AgentRegistrationSpec {
        name: cli_name_or_generated(arguments.name, "agent")?,
        integration: arguments.integration,
        external_session_id: arguments.external_session_id,
        report: AgentReport {
            state: arguments.state.into(),
            authority: arguments.authority.into(),
            evidence: arguments.evidence,
            confidence: arguments.confidence,
        },
    };
    let agent = if ensure {
        client.ensure_agent(shell_id, run_id, spec)?
    } else {
        client.register_agent(shell_id, run_id, spec)?
    };
    if json {
        let workspace = client.get_workspace(&agent.workspace_id)?;
        return print_json(
            if ensure {
                CommandKey::AgentEnsure
            } else {
                CommandKey::AgentRegister
            },
            serde_json::json!({
                "agent": cli_output::agent(&agent, Some(&workspace.name)),
            }),
        );
    }
    println!(
        "{} agent {} ({})",
        if ensure { "Ensured" } else { "Registered" },
        agent.name,
        agent.id
    );
    Ok(())
}

fn resolve_agent_context(
    shell_id: Option<String>,
    run_id: Option<String>,
    environment_shell_id: Option<String>,
    environment_run_id: Option<String>,
) -> Result<(String, String), Box<dyn Error>> {
    let shell_id = agent_context_value("shell ID", shell_id.or(environment_shell_id))?;
    let run_id = agent_context_value("run ID", run_id.or(environment_run_id))?;
    match (shell_id, run_id) {
        (Some(shell_id), Some(run_id)) => Ok((shell_id, run_id)),
        _ => Err(cli_output::failure(
            "context_required",
            "agent commands require both shell and run identity; pass --shell-id and --run-id or set BOOMUX_SHELL_ID and BOOMUX_RUN_ID",
        )),
    }
}

fn agent_context_value(
    kind: &str,
    value: Option<String>,
) -> Result<Option<String>, Box<dyn Error>> {
    value
        .map(|value| {
            let value = value.trim();
            if value.is_empty() {
                Err(cli_output::failure(
                    "invalid_argument",
                    format!("agent {kind} cannot be empty"),
                ))
            } else {
                Ok(value.to_owned())
            }
        })
        .transpose()
}

fn print_agent(agent: &AgentInstanceSnapshot, workspace_name: &str) {
    println!("ID\t{}", agent.id);
    println!("NAME\t{}", agent.name);
    println!("WORKSPACE\t{workspace_name}");
    println!("SHELL ID\t{}", agent.shell_id);
    println!("RUN ID\t{}", agent.run_id);
    println!("INTEGRATION\t{}", agent.integration);
    println!(
        "EXTERNAL SESSION ID\t{}",
        agent.external_session_id.as_deref().unwrap_or("-")
    );
    println!("STARTED AT MS\t{}", agent.started_at_ms);
    println!(
        "ENDED AT MS\t{}",
        agent
            .ended_at_ms
            .map_or_else(|| "-".into(), |value| value.to_string())
    );
    println!("REVISION\t{}", agent.observation.revision);
    println!(
        "STATE\t{}",
        cli_output::agent_state(agent.observation.state)
    );
    println!(
        "AUTHORITY\t{}",
        cli_output::agent_authority(agent.observation.authority)
    );
    println!("EVIDENCE\t{}", agent.observation.evidence);
    println!("CONFIDENCE\t{}", agent.observation.confidence);
    println!("OBSERVED AT MS\t{}", agent.observation.observed_at_ms);
}

fn list_workspace_shells(json: bool) -> Result<(), Box<dyn Error>> {
    let client = client::connect_or_start()?;
    let shell = current_shell(&client)?;
    let workspace = client.get_workspace(&shell.workspace_id)?;
    if json {
        let shells = workspace
            .shells
            .iter()
            .map(|shell| cli_output::shell(shell, Some(&workspace.name)))
            .collect::<Vec<_>>();
        return print_json(
            CommandKey::Shells,
            serde_json::json!({
                "workspace_id": workspace.id,
                "workspace_name": workspace.name,
                "shells": shells,
            }),
        );
    }
    println!("NAME\tSHELL ID\tRUN ID\tSTATUS");
    for shell in workspace.shells {
        println!(
            "{}\t{}\t{}\t{}",
            shell.name,
            shell.id,
            shell.run.as_ref().map_or("-", |run| run.id.as_str()),
            shell_status(&shell.status)
        );
    }
    Ok(())
}

fn read_events(
    after: Option<&str>,
    limit: u16,
    wait_ms: u32,
    json: bool,
) -> Result<(), Box<dyn Error>> {
    let after = after.map(parse_event_cursor).transpose()?;
    let batch = client::connect_or_start()?.events(after, limit, wait_ms)?;
    let cursor = format_event_cursor(&batch.cursor);
    if json {
        let snapshot = batch.snapshot.map(json_snapshot).transpose()?;
        return print_json(
            CommandKey::Events,
            serde_json::json!({
                "stream_id": batch.stream_id,
                "cursor": cursor,
                "snapshot": snapshot,
                "events": batch.events,
            }),
        );
    }
    println!("CURSOR\t{cursor}");
    if let Some(snapshot) = batch.snapshot {
        println!("SNAPSHOT\t{}", snapshot.workspaces.len());
    }
    for event in batch.events {
        let value = serde_json::to_value(&event.kind)?;
        println!(
            "{}\t{}\t{}",
            event.id,
            event.at_ms,
            value["event"].as_str().unwrap_or("unknown")
        );
    }
    Ok(())
}

fn parse_event_cursor(value: &str) -> Result<EventCursor, Box<dyn Error>> {
    let (stream_id, event_id) = value.split_once(':').ok_or_else(|| {
        cli_output::failure(
            "invalid_argument",
            "event cursor must have the form <stream-uuid>:<event-id>",
        )
    })?;
    Uuid::parse_str(stream_id).map_err(|_| {
        cli_output::failure("invalid_argument", "event cursor stream ID is invalid")
    })?;
    let event_id = event_id
        .parse::<u64>()
        .map_err(|_| cli_output::failure("invalid_argument", "event cursor event ID is invalid"))?;
    Ok(EventCursor {
        stream_id: stream_id.into(),
        event_id,
    })
}

fn format_event_cursor(cursor: &EventCursor) -> String {
    format!("{}:{}", cursor.stream_id, cursor.event_id)
}

fn json_snapshot(snapshot: Snapshot) -> Result<serde_json::Value, Box<dyn Error>> {
    let workspaces = snapshot
        .workspaces
        .iter()
        .map(|workspace| {
            let shells = workspace
                .shells
                .iter()
                .map(|shell| cli_output::shell(shell, Some(&workspace.name)))
                .collect::<Vec<_>>();
            let launchers = workspace
                .launchers
                .iter()
                .map(|launcher| cli_output::launcher(launcher, Some(&workspace.name)))
                .collect::<Vec<_>>();
            let agents = workspace
                .agents
                .iter()
                .map(|agent| cli_output::agent(agent, Some(&workspace.name)))
                .collect::<Vec<_>>();
            serde_json::json!({
                "id": workspace.id,
                "name": workspace.name,
                "shells": shells,
                "launchers": launchers,
                "agents": agents,
            })
        })
        .collect::<Vec<_>>();
    Ok(serde_json::json!({ "workspaces": workspaces }))
}

fn read_shell(
    target: &str,
    lines: u32,
    json: bool,
    run_id: Option<&str>,
    after_revision: Option<u64>,
    wait_ms: u32,
) -> Result<(), Box<dyn Error>> {
    let client = client::connect_or_start()?;
    let snapshot = client.snapshot()?;
    let current_workspace_id = env::var("BOOMUX_SHELL_ID")
        .ok()
        .and_then(|id| find_shell(&snapshot, &id).map(|shell| shell.workspace_id.clone()));
    let shell = resolve_shell_target(&snapshot, current_workspace_id.as_deref(), target)?;
    if json || run_id.is_some() {
        let state = client.read_shell_at(
            &shell.id,
            READ_BYTES,
            run_id.map(str::to_owned),
            after_revision,
            wait_ms,
        )?;
        let output = recent_lines(&String::from_utf8_lossy(&state.bytes), lines as usize);
        if json {
            return print_json(
                CommandKey::Read,
                serde_json::json!({
                    "shell_id": shell.id,
                    "run_id": state.run_id,
                    "output_revision": state.output_revision,
                    "changed": state.changed,
                    "status": shell_status(&state.status),
                    "output": output,
                }),
            );
        }
        print_output(&output);
        return Ok(());
    }
    let bytes = client.read_shell(&shell.id, READ_BYTES)?;
    let output = recent_lines(&String::from_utf8_lossy(&bytes), lines as usize);
    print_output(&output);
    Ok(())
}

fn print_output(output: &str) {
    print!("{output}");
    if !output.is_empty() && !output.ends_with('\n') {
        println!();
    }
}

fn close_shell(target: &str) -> Result<(), Box<dyn Error>> {
    let client = client::connect_or_start()?;
    close_shell_with_workspace(&client, target, None)
}

fn close_shell_with_workspace(
    client: &client::Client,
    target: &str,
    workspace: Option<&str>,
) -> Result<(), Box<dyn Error>> {
    let snapshot = client.snapshot()?;
    let current_shell_id = env::var("BOOMUX_SHELL_ID").ok();
    let shell = resolve_cli_shell(&snapshot, target, workspace)?;
    if current_shell_id.as_deref() == Some(shell.id.as_str()) {
        return Err(
            "cannot close the current shell from inside it; use the dashboard or another shell"
                .into(),
        );
    }
    let workspace_name = snapshot
        .workspaces
        .iter()
        .find(|workspace| workspace.id == shell.workspace_id)
        .map(|workspace| workspace.name.as_str())
        .unwrap_or("workspace");
    client.close_shell(&shell.id)?;
    println!("Closed shell {} from {workspace_name}", shell.name);
    Ok(())
}

fn resolve_cli_shell<'a>(
    snapshot: &'a Snapshot,
    target: &str,
    workspace: Option<&str>,
) -> Result<&'a ShellSnapshot, Box<dyn Error>> {
    if let Some(shell) = find_shell(snapshot, target) {
        return Ok(shell);
    }
    let workspace_id = if let Some(workspace) = workspace {
        resolve_workspace_target(&snapshot.workspaces, workspace)?
            .id
            .as_str()
    } else {
        let current_shell_id = env::var("BOOMUX_SHELL_ID").map_err(|_| {
            cli_output::failure(
                "context_required",
                format!(
                    "shell name {target:?} requires --workspace outside a Boomux-managed shell"
                ),
            )
        })?;
        find_shell(snapshot, &current_shell_id)
            .map(|shell| shell.workspace_id.as_str())
            .ok_or_else(|| {
                cli_output::failure("not_found", "current Boomux shell no longer exists")
            })?
    };
    resolve_shell_target(snapshot, Some(workspace_id), target)
}

fn resolve_cli_launcher<'a>(
    snapshot: &'a Snapshot,
    target: &str,
    workspace: Option<&str>,
) -> Result<&'a WorkspaceLauncherSnapshot, Box<dyn Error>> {
    if let Some(launcher) = find_launcher(snapshot, target) {
        return Ok(launcher);
    }
    let workspace_id = if let Some(workspace) = workspace {
        resolve_workspace_target(&snapshot.workspaces, workspace)?
            .id
            .as_str()
    } else {
        let current_shell_id = env::var("BOOMUX_SHELL_ID").map_err(|_| {
            cli_output::failure(
                "context_required",
                format!(
                    "launcher name {target:?} requires --workspace outside a Boomux-managed shell"
                ),
            )
        })?;
        find_shell(snapshot, &current_shell_id)
            .map(|shell| shell.workspace_id.as_str())
            .ok_or_else(|| {
                cli_output::failure("not_found", "current Boomux shell no longer exists")
            })?
    };
    let workspace = snapshot
        .workspaces
        .iter()
        .find(|workspace| workspace.id == workspace_id)
        .ok_or_else(|| cli_output::failure("not_found", "current workspace no longer exists"))?;
    workspace
        .launchers
        .iter()
        .find(|launcher| launcher.name == target)
        .ok_or_else(|| {
            let available = workspace
                .launchers
                .iter()
                .map(|launcher| launcher.name.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            cli_output::failure(
                "not_found",
                format!(
                    "launcher {target:?} was not found in this workspace; available launchers: {available}"
                ),
            )
        })
}

fn resolve_shell_target<'a>(
    snapshot: &'a Snapshot,
    current_workspace_id: Option<&str>,
    target: &str,
) -> Result<&'a ShellSnapshot, Box<dyn Error>> {
    if let Some(shell) = find_shell(snapshot, target) {
        return Ok(shell);
    }
    let workspace_id = current_workspace_id.ok_or_else(|| {
        cli_output::failure(
            "context_required",
            format!(
                "shell name {target:?} requires a Boomux shell; use an exact shell ID outside Boomux"
            ),
        )
    })?;
    let workspace = snapshot
        .workspaces
        .iter()
        .find(|workspace| workspace.id == workspace_id)
        .ok_or_else(|| cli_output::failure("not_found", "current workspace no longer exists"))?;
    let matches = workspace
        .shells
        .iter()
        .filter(|shell| shell.name == target)
        .collect::<Vec<_>>();
    match matches.as_slice() {
        [shell] => Ok(shell),
        [] => {
            let available = workspace
                .shells
                .iter()
                .map(|shell| shell.name.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            Err(cli_output::failure(
                "not_found",
                format!(
                    "shell {target:?} was not found in this workspace; available shells: {available}"
                ),
            ))
        }
        _ => Err(cli_output::failure(
            "ambiguous_target",
            format!("shell name {target:?} is ambiguous in this workspace"),
        )),
    }
}

fn recent_lines(text: &str, count: usize) -> String {
    let lines = text.split_inclusive('\n').collect::<Vec<_>>();
    lines[lines.len().saturating_sub(count)..].concat()
}

fn find_shell<'a>(snapshot: &'a Snapshot, id: &str) -> Option<&'a ShellSnapshot> {
    snapshot
        .workspaces
        .iter()
        .flat_map(|workspace| &workspace.shells)
        .find(|shell| shell.id == id)
}

fn find_launcher<'a>(snapshot: &'a Snapshot, id: &str) -> Option<&'a WorkspaceLauncherSnapshot> {
    snapshot
        .workspaces
        .iter()
        .flat_map(|workspace| &workspace.launchers)
        .find(|launcher| launcher.id == id)
}

fn current_shell(client: &client::Client) -> Result<ShellSnapshot, Box<dyn Error>> {
    let shell_id = env::var("BOOMUX_SHELL_ID").map_err(|_| {
        cli_output::failure(
            "context_required",
            "this command must run inside a Boomux-managed shell",
        )
    })?;
    Ok(client.get_shell(shell_id)?)
}

fn shell_status(status: &ShellStatus) -> &'static str {
    match status {
        ShellStatus::Pending => "pending",
        ShellStatus::Running => "running",
        ShellStatus::Exited { .. } => "exited",
    }
}

fn shell_exit_reason(reason: &boomux::protocol::ShellRunExitReason) -> String {
    match reason {
        boomux::protocol::ShellRunExitReason::Exited { code: Some(code) } => {
            format!("exited ({code})")
        }
        boomux::protocol::ShellRunExitReason::Exited { code: None } => {
            "exited (code unavailable)".into()
        }
        boomux::protocol::ShellRunExitReason::Terminated => "terminated".into(),
        boomux::protocol::ShellRunExitReason::Interrupted => "interrupted".into(),
    }
}

fn open_dashboard_shell(
    client: &client::Client,
    shell_id: &str,
    terminal: Option<&str>,
) -> Result<String, Box<dyn Error>> {
    let shell = client.get_shell(shell_id)?;
    if matches!(shell.owner, protocol::ShellOwner::Schedule { .. })
        && shell.status != ShellStatus::Running
    {
        return Err(io::Error::new(
            io::ErrorKind::WouldBlock,
            "schedule-owned shell is attachable only while its execution is active",
        )
        .into());
    }
    let workspace = client.get_workspace(&shell.workspace_id)?;
    open_terminal(
        shell_id,
        &format!("{} - {}", workspace.name, shell.name),
        true,
        terminal,
    )?;
    Ok(format!("Opened {} from {}", shell.name, workspace.name))
}

fn open_dashboard_remote_shell(
    client: &client::Client,
    identity: &protocol::QualifiedIdentity,
    terminal: Option<&str>,
) -> Result<String, Box<dyn Error>> {
    let registration = client.node_registration(&identity.node_id)?;
    let shell = routed_dashboard_shell(client, identity, "").map_err(io::Error::other)?;
    if matches!(shell.owner, protocol::ShellOwner::Schedule { .. })
        && shell.status != ShellStatus::Running
    {
        return Err(io::Error::new(
            io::ErrorKind::WouldBlock,
            "schedule-owned shell is attachable only while its execution is active",
        )
        .into());
    }
    let workspace = match client.route_node_operation(
        &identity.node_id,
        protocol::RoutedOperation::GetWorkspace {
            workspace_id: shell.workspace_id.clone(),
        },
    )? {
        protocol::RoutedOperationResult::Workspace { workspace } => workspace,
        _ => return Err("remote Node returned an unexpected workspace response".into()),
    };
    terminal::open_remote(
        terminal,
        &identity.node_id,
        &identity.inner_id,
        &format!(
            "[{}] {} - {}",
            registration.alias, workspace.name, shell.name
        ),
        true,
    )?;
    Ok(format!(
        "Opened {} from {} on Node {}",
        shell.name, workspace.name, registration.alias
    ))
}

fn open_workspace(
    workspace: &WorkspaceSnapshot,
    terminal: Option<&str>,
) -> Result<(), Box<dyn Error>> {
    if workspace_user_shell_count(workspace) == 0 && workspace.launchers.is_empty() {
        return Err(format!("workspace {} has no shells or launchers", workspace.name).into());
    }
    let mut failures = Vec::new();
    for launcher in &workspace.launchers {
        if let Err(error) = invoke_workspace_launcher(workspace, launcher) {
            failures.push(format!("launcher {}: {error}", launcher.name));
        }
    }
    for shell in workspace
        .shells
        .iter()
        .filter(|shell| matches!(shell.owner, protocol::ShellOwner::User))
    {
        if let Err(error) = open_terminal(
            &shell.id,
            &format!("{} - {}", workspace.name, shell.name),
            true,
            terminal,
        ) {
            failures.push(format!("shell {}: {error}", shell.name));
        }
    }
    if !failures.is_empty() {
        return Err(io::Error::other(format!(
            "workspace {} opened with failures: {}",
            workspace.name,
            failures.join("; ")
        ))
        .into());
    }
    Ok(())
}

fn workspace_user_shell_count(workspace: &WorkspaceSnapshot) -> usize {
    workspace
        .shells
        .iter()
        .filter(|shell| matches!(shell.owner, protocol::ShellOwner::User))
        .count()
}

fn invoke_workspace_launcher(
    workspace: &WorkspaceSnapshot,
    launcher: &WorkspaceLauncherSnapshot,
) -> io::Result<()> {
    let (executable, arguments) = launcher
        .command
        .split_first()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "launcher command is empty"))?;
    let mut command = Command::new(executable);
    command
        .args(arguments)
        .current_dir(&launcher.cwd)
        .env("BOOMUX_WORKSPACE_ID", &workspace.id)
        .env("BOOMUX_WORKSPACE", &workspace.name)
        .env("BOOMUX_LAUNCHER_ID", &launcher.id)
        .env("BOOMUX_LAUNCHER_NAME", &launcher.name)
        .env_remove("BOOMUX_SHELL_ID")
        .env_remove("BOOMUX_SHELL_NAME")
        .env_remove("BOOMUX_RUN_ID")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    // The child has not executed user code yet; `setsid` detaches it from the
    // invoking terminal while preserving the client's desktop environment.
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                Err(io::Error::last_os_error())
            } else {
                Ok(())
            }
        });
    }
    let mut child = command.spawn()?;
    thread::Builder::new()
        .name(format!("launcher-reaper-{}", launcher.id))
        .spawn(move || {
            let _ = child.wait();
        })
        .map_err(|error| io::Error::other(format!("could not start launcher reaper: {error}")))?;
    Ok(())
}

fn open_shell(
    shell_id: &str,
    node: Option<&str>,
    title: Option<&str>,
    takeover: bool,
    terminal: Option<&str>,
) -> Result<(), Box<dyn Error>> {
    let client = client::connect_or_start()?;
    if let Some(selector) = node {
        let registration = client.node_registration(selector)?;
        let identity = protocol::QualifiedIdentity::new(&registration.node_id, shell_id);
        let shell = routed_dashboard_shell(&client, &identity, "").map_err(io::Error::other)?;
        if matches!(shell.owner, protocol::ShellOwner::Schedule { .. })
            && shell.status != ShellStatus::Running
        {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "schedule-owned shell is attachable only while its execution is active",
            )
            .into());
        }
        let workspace = match client.route_node_operation(
            &registration.node_id,
            protocol::RoutedOperation::GetWorkspace {
                workspace_id: shell.workspace_id.clone(),
            },
        )? {
            protocol::RoutedOperationResult::Workspace { workspace } => workspace,
            _ => return Err("remote Node returned an unexpected workspace response".into()),
        };
        let title = title.map(str::to_owned).unwrap_or_else(|| {
            format!(
                "[{}] {} - {}",
                registration.alias, workspace.name, shell.name
            )
        });
        return terminal::open_remote(terminal, &registration.node_id, shell_id, &title, takeover);
    }
    let shell = client.get_shell(shell_id)?;
    if matches!(shell.owner, protocol::ShellOwner::Schedule { .. })
        && shell.status != ShellStatus::Running
    {
        return Err(io::Error::new(
            io::ErrorKind::WouldBlock,
            "schedule-owned shell is attachable only while its execution is active",
        )
        .into());
    }
    let title = title.map(str::to_owned).unwrap_or_else(|| {
        client
            .get_workspace(&shell.workspace_id)
            .map(|workspace| format!("{} - {}", workspace.name, shell.name))
            .unwrap_or_else(|_| format!("Boomux: {}", shell.name))
    });
    open_terminal(shell_id, &title, takeover, terminal)
}

fn open_terminal(
    shell_id: &str,
    title: &str,
    takeover: bool,
    terminal: Option<&str>,
) -> Result<(), Box<dyn Error>> {
    terminal::open(terminal, shell_id, title, takeover)
}

fn print_prompt_label() -> Result<(), Box<dyn Error>> {
    let Some(shell_id) = env::var_os("BOOMUX_SHELL_ID").and_then(|value| value.into_string().ok())
    else {
        return Ok(());
    };
    let client = client::connect_or_start()?;
    let shell = client.get_shell(shell_id)?;
    if let Ok(workspace) = client.get_workspace(&shell.workspace_id) {
        println!("{}/{}", workspace.name, shell.name);
    } else {
        println!("{}", shell.name);
    }
    Ok(())
}

fn install_skill(force: bool) -> Result<(), Box<dyn Error>> {
    let home = env::var_os("HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .ok_or("HOME must be an absolute path to install the Boomux skill")?;
    install_skill_at(&home, force)
}

fn install_skill_at(home: &Path, force: bool) -> Result<(), Box<dyn Error>> {
    require_absolute_root(home, "HOME")?;
    let directory = ensure_safe_directory(&home.join(".agents/skills/boomux"))?;
    let path = skill_install_path(home);
    let outcome = install_asset_at(&directory, &path, BOOMUX_SKILL, force)?;

    if outcome == InstallOutcome::Unchanged {
        println!("Boomux skill is already installed at {}", path.display());
    } else {
        println!("Installed Boomux skill at {}", path.display());
    }
    migrate_legacy_skill(home)?;
    Ok(())
}

fn migrate_legacy_skill(home: &Path) -> Result<(), Box<dyn Error>> {
    let directory = home.join(".agents/skills/boomux-shells");
    match fs::symlink_metadata(&directory) {
        Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {}
        Ok(_) => {
            return Err(format!(
                "legacy Boomux skill directory is not a regular directory: {}",
                directory.display()
            )
            .into());
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    }
    let path = legacy_skill_install_path(home);
    if let Some(content_matches) = regular_file_matches(&path, LEGACY_BOOMUX_SHELLS_SKILL)? {
        let entries = fs::read_dir(&directory)?.collect::<Result<Vec<_>, _>>()?;
        let untouched =
            content_matches && entries.len() == 1 && entries[0].file_name() == "SKILL.md";
        if untouched {
            fs::remove_file(&path)?;
            if let Some(directory) = path.parent() {
                let _ = fs::remove_dir(directory);
            }
            println!("Removed legacy Boomux shell skill at {}", path.display());
        } else {
            eprintln!(
                "warning: preserved customized legacy Boomux skill at {}; remove it to avoid duplicate guidance",
                path.display()
            );
        }
    }
    Ok(())
}

fn install_opencode(force: bool) -> Result<(), Box<dyn Error>> {
    install_integrations(
        &[integration_management::IntegrationId::Opencode],
        force,
        false,
        false,
    )
}

#[cfg(test)]
fn install_opencode_at(config_root: &Path, force: bool) -> Result<(), Box<dyn Error>> {
    let result = integration_management::install_at(
        integration_management::IntegrationId::Opencode,
        config_root,
        force,
    )?;
    print_integration_install_results(&[result]);
    Ok(())
}

fn install_pi(force: bool) -> Result<(), Box<dyn Error>> {
    install_integrations(
        &[integration_management::IntegrationId::Pi],
        force,
        false,
        false,
    )
}

#[cfg(test)]
fn install_pi_at(config_root: &Path, force: bool) -> Result<(), Box<dyn Error>> {
    let result = integration_management::install_at(
        integration_management::IntegrationId::Pi,
        config_root,
        force,
    )?;
    print_integration_install_results(&[result]);
    Ok(())
}

fn skill_install_path(home: &Path) -> PathBuf {
    home.join(".agents/skills/boomux/SKILL.md")
}

#[cfg(test)]
fn opencode_install_path(config_root: &Path) -> PathBuf {
    integration_management::install_path_at(
        integration_management::IntegrationId::Opencode,
        config_root,
    )
}

#[cfg(test)]
fn pi_install_path(config_root: &Path) -> PathBuf {
    integration_management::install_path_at(integration_management::IntegrationId::Pi, config_root)
}

fn legacy_skill_install_path(home: &Path) -> PathBuf {
    home.join(".agents/skills/boomux-shells/SKILL.md")
}

fn doctor_version_line(version: &str, architecture: &str, operating_system: &str) -> String {
    format!("ok  boomux: {version} ({architecture}-{operating_system})")
}

fn doctor(terminal_override: Option<&str>) -> Result<(), Box<dyn Error>> {
    println!(
        "{}",
        doctor_version_line(
            env!("CARGO_PKG_VERSION"),
            env::consts::ARCH,
            env::consts::OS
        )
    );
    let mut healthy = true;
    let daemon_snapshot = match client::connect_or_start() {
        Ok(client) => {
            println!(
                "ok  daemon: protocol {} ({})",
                client.protocol_version()?,
                client.socket_path().display()
            );
            match client.snapshot() {
                Ok(snapshot) => Some(snapshot),
                Err(error) => {
                    healthy = false;
                    eprintln!("err daemon snapshot: {error}");
                    None
                }
            }
        }
        Err(error) => {
            healthy = false;
            eprintln!("err daemon: {error}");
            None
        }
    };
    for command in ["git"] {
        match Command::new(command).arg("--version").output() {
            Ok(output) if output.status.success() => {
                println!(
                    "ok  {command}: {}",
                    String::from_utf8_lossy(&output.stdout).trim()
                );
            }
            Ok(output) => {
                healthy = false;
                eprintln!("err {command}: exited with {}", output.status);
            }
            Err(error) => {
                healthy = false;
                eprintln!("err {command}: {error}");
            }
        }
    }
    match config::load() {
        Ok(config) => {
            let discovery = projects::discover(&config.projects);
            let count = discovery.projects.len();
            let source = config
                .path
                .map_or_else(|| "defaults".to_owned(), |path| path.display().to_string());
            if discovery.warnings.is_empty() {
                println!("ok  config: {source} ({count} projects)");
            } else {
                healthy = false;
                eprintln!("err config: {source} ({count} projects)");
                for warning in discovery.warnings {
                    eprintln!("    {warning}");
                }
            }
            match daemon_snapshot
                .as_ref()
                .and_then(|snapshot| snapshot.scheduler.as_ref())
            {
                Some(scheduler)
                    if scheduler.state == protocol::SchedulerState::Active
                        && scheduler.max_concurrent
                            == config.notifications.max_scheduled_execution_concurrency =>
                {
                    println!(
                        "ok  scheduler: active, max_concurrent={} (sampled at daemon start)",
                        scheduler.max_concurrent
                    );
                }
                Some(scheduler) if scheduler.state == protocol::SchedulerState::Offline => {
                    healthy = false;
                    eprintln!("err scheduler: offline; timed schedules are not being evaluated");
                }
                Some(scheduler) => {
                    healthy = false;
                    eprintln!(
                        "err scheduler: daemon uses max_concurrent={}, config resolves {}; restart daemon after changes",
                        scheduler.max_concurrent,
                        config.notifications.max_scheduled_execution_concurrency
                    );
                }
                None => {
                    healthy = false;
                    eprintln!(
                        "err scheduler: offline or unavailable; timed schedules require the Boomux daemon and user session"
                    );
                }
            }
            let terminal = terminal_override.or(config.terminal.as_deref());
            match terminal::selected(terminal) {
                Ok(selected) => println!("ok  terminal: {selected}"),
                Err(error) => {
                    healthy = false;
                    eprintln!("err terminal: {error}");
                }
            }
            match notification_diagnostic(
                &config.notifications,
                executable_on_path("notify-send"),
                plausible_desktop_bus(),
            ) {
                NotificationDiagnostic::Disabled => {
                    println!("ok  notification config: disabled (sampled at daemon start)");
                }
                NotificationDiagnostic::Ready => {
                    println!(
                        "ok  notification config: notify-send and plausible desktop bus context present; restart daemon after changes"
                    );
                }
                NotificationDiagnostic::MissingExecutable => {
                    healthy = false;
                    eprintln!("err notifications: notify-send is not executable on PATH");
                }
                NotificationDiagnostic::MissingDesktopBus => {
                    healthy = false;
                    eprintln!("err notifications: no plausible desktop bus is available");
                }
            }
            match sound_notification_diagnostic(
                &config.notifications,
                executable_on_path("canberra-gtk-play"),
            ) {
                SoundNotificationDiagnostic::Disabled => {
                    println!("ok  notification sound: disabled (sampled at daemon start)");
                }
                SoundNotificationDiagnostic::Ready => {
                    println!(
                        "ok  notification sound: canberra-gtk-play is executable; restart daemon after changes"
                    );
                }
                SoundNotificationDiagnostic::MissingExecutable => {
                    healthy = false;
                    eprintln!(
                        "err notification sound: canberra-gtk-play is not executable on PATH"
                    );
                }
            }
        }
        Err(error) => {
            healthy = false;
            eprintln!("err config: {error}");
        }
    }
    let integration_environment = integration_management::Environment::from_process();
    for integration in integration_management::IntegrationId::all() {
        let status = integration_management::inspect_without_host_probe(
            integration,
            &integration_environment,
            daemon_snapshot.as_ref(),
        );
        healthy &= print_integration_diagnostic(integration, &status);
    }
    if healthy {
        Ok(())
    } else {
        Err("one or more dependency or configuration checks failed".into())
    }
}

fn print_integration_diagnostic(
    integration: integration_management::IntegrationId,
    status: &integration_management::IntegrationStatus,
) -> bool {
    let spec = integration.spec();
    let installation = integration.installation();
    let path = status.asset.path.as_deref().unwrap_or("unresolved path");
    if status.asset.state == integration_management::AssetState::Unavailable {
        eprintln!(
            "err {} integration: cannot inspect {} at {path}: {}",
            spec.key,
            installation.asset_name,
            status.asset.error.as_deref().unwrap_or("unknown error")
        );
        return false;
    }
    if status.runtime.running_processes == 0 {
        println!(
            "ok  {} integration: {} {} at {path}",
            spec.key,
            installation.asset_name,
            status.asset.state.as_str(),
        );
        return true;
    }
    if status.asset.state != integration_management::AssetState::Current {
        eprintln!(
            "err {} integration: {} {} at {path}; run boomux integration install {}{}",
            spec.key,
            installation.asset_name,
            status.asset.state.as_str(),
            spec.key,
            if status.asset.state == integration_management::AssetState::Modified {
                " --force"
            } else {
                ""
            }
        );
        return false;
    }
    if status.runtime.untracked_processes == 0 {
        println!(
            "ok  {} integration: lifecycle registration active",
            spec.key
        );
        true
    } else {
        eprintln!(
            "err {} integration: {} foreground process(es) are untracked; restart {} and verify it loads {path}",
            spec.key, status.runtime.untracked_processes, spec.display_name
        );
        false
    }
}

#[derive(Debug, PartialEq, Eq)]
enum NotificationDiagnostic {
    Disabled,
    Ready,
    MissingExecutable,
    MissingDesktopBus,
}

#[derive(Debug, PartialEq, Eq)]
enum SoundNotificationDiagnostic {
    Disabled,
    Ready,
    MissingExecutable,
}

fn notification_diagnostic(
    settings: &daemon::NotificationDeliverySettings,
    executable: bool,
    desktop_bus: bool,
) -> NotificationDiagnostic {
    if !settings.desktop.enabled {
        NotificationDiagnostic::Disabled
    } else if !executable {
        NotificationDiagnostic::MissingExecutable
    } else if !desktop_bus {
        NotificationDiagnostic::MissingDesktopBus
    } else {
        NotificationDiagnostic::Ready
    }
}

fn sound_notification_diagnostic(
    settings: &daemon::NotificationDeliverySettings,
    executable: bool,
) -> SoundNotificationDiagnostic {
    if !settings.sound.enabled {
        SoundNotificationDiagnostic::Disabled
    } else if !executable {
        SoundNotificationDiagnostic::MissingExecutable
    } else {
        SoundNotificationDiagnostic::Ready
    }
}

fn test_notification(reason: CliNotificationReason) -> Result<(), Box<dyn Error>> {
    let settings = config::load_notification_settings()?;
    daemon::test_notification_delivery(&settings, reason.into())?;
    let reason = match reason {
        CliNotificationReason::Blocked => "blocked",
        CliNotificationReason::Completed => "completed",
    };
    println!("Delivered test {reason} notification");
    Ok(())
}

fn executable_on_path(name: &str) -> bool {
    env::var_os("PATH").is_some_and(|path| {
        env::split_paths(&path).any(|directory| {
            fs::metadata(directory.join(name)).is_ok_and(|metadata| {
                metadata.is_file() && metadata.permissions().mode() & 0o111 != 0
            })
        })
    })
}

fn plausible_desktop_bus() -> bool {
    env::var_os("DBUS_SESSION_BUS_ADDRESS").is_some_and(|value| !value.is_empty())
        || env::var_os("XDG_RUNTIME_DIR").is_some_and(|directory| {
            let directory = PathBuf::from(directory);
            directory.is_absolute() && directory.join("bus").exists()
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::integration_management::{opencode_config_root, pi_config_root};
    use std::os::unix::net::UnixListener;

    fn shell(id: &str, workspace_id: &str, name: &str) -> ShellSnapshot {
        ShellSnapshot {
            owner: boomux::protocol::ShellOwner::User,
            id: id.into(),
            revision: 1,
            workspace_id: workspace_id.into(),
            name: name.into(),
            cwd: PathBuf::from("/tmp/project"),
            command: Vec::new(),
            status: ShellStatus::Running,
            run: Some(protocol::ShellRunSnapshot {
                id: "r1".into(),
                generation: 1,
                started_at_ms: 1,
                ended_at_ms: None,
                exit_reason: None,
                output_revision: 0,
                environment_has_run_id: true,
            }),
            foreground_process: None,
        }
    }

    fn workspace(id: &str, name: &str, shells: Vec<ShellSnapshot>) -> WorkspaceSnapshot {
        WorkspaceSnapshot {
            id: id.into(),
            revision: 1,
            name: name.into(),
            default_cwd: None,
            shells,
            launchers: Vec::new(),
            agents: Vec::new(),
            schedules: Vec::new(),
        }
    }

    fn schedule(id: &str, workspace_id: &str, name: &str) -> AgentScheduleSnapshot {
        AgentScheduleSnapshot {
            id: id.into(),
            workspace_id: workspace_id.into(),
            name: name.into(),
            cwd: PathBuf::from("/tmp/project"),
            integration: "opencode".into(),
            session: AgentScheduleSession::Fresh,
            trigger: AgentScheduleTrigger {
                cron: "0 9 * * 1-5".into(),
                timezone: "UTC".into(),
            },
            state: AgentScheduleState::Paused,
            overlap_policy: AgentScheduleOverlapPolicy::Skip,
            revision: 1,
            prompt_revision: 1,
            trigger_revision: 1,
            created_at_ms: 10,
            updated_at_ms: 10,
            evaluation_frontier_ms: 10,
            execution_shell_id: None,
            next_occurrence: None,
        }
    }

    fn scheduled_execution(id: &str, schedule_id: &str) -> ScheduledExecutionSnapshot {
        ScheduledExecutionSnapshot {
            id: id.into(),
            workspace_id: "w1".into(),
            schedule_id: schedule_id.into(),
            revision: 1,
            state: protocol::ScheduledExecutionState::Active,
            dispatch_kind: protocol::ScheduledExecutionDispatchKind::Manual,
            dispatch_key: Uuid::new_v4().to_string(),
            schedule_revision: 1,
            prompt_revision: 1,
            trigger_revision: 1,
            requested_at_ms: 20,
            scheduled_at_ms: None,
            coalesced_through_ms: None,
            started_at_ms: Some(21),
            ended_at_ms: None,
            cwd: "/tmp/project".into(),
            integration: "opencode".into(),
            session: AgentScheduleSession::Fresh,
            reason: None,
            outcome: None,
            shell_id: Some("schedule-shell".into()),
            run_id: Some("schedule-run".into()),
            agent_id: None,
            external_session_id: None,
        }
    }

    fn agent(id: &str, workspace_id: &str, shell_id: &str) -> AgentInstanceSnapshot {
        AgentInstanceSnapshot {
            id: id.into(),
            workspace_id: workspace_id.into(),
            shell_id: shell_id.into(),
            run_id: "r1".into(),
            name: "OpenCode".into(),
            integration: "opencode".into(),
            external_session_id: Some("external-1".into()),
            cwd: Some("/tmp/project".into()),
            started_at_ms: 10,
            ended_at_ms: None,
            attention: None,
            observation: protocol::AgentObservationSnapshot {
                revision: 1,
                state: AgentState::Working,
                authority: AgentAuthority::LifecycleIntegration,
                evidence: "tool call".into(),
                confidence: 95,
                observed_at_ms: 11,
            },
        }
    }

    fn launcher(id: &str, workspace_id: &str, name: &str) -> WorkspaceLauncherSnapshot {
        WorkspaceLauncherSnapshot {
            id: id.into(),
            revision: 1,
            workspace_id: workspace_id.into(),
            name: name.into(),
            cwd: PathBuf::from("/tmp/project"),
            command: vec!["zeditor".into(), ".".into()],
        }
    }

    #[test]
    fn dashboard_refresh_uses_one_snapshot_request() {
        let directory =
            env::temp_dir().join(format!("boomux-dashboard-refresh-{}", Uuid::new_v4()));
        fs::create_dir_all(&directory).unwrap();
        let socket = directory.join("daemon.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let initial = Snapshot {
            workspaces: vec![workspace(
                "w1",
                "before",
                vec![shell("s1", "w1", "one"), shell("s2", "w1", "two")],
            )],
            focused_terminal: None,
            scheduler: None,
        };
        let event_refreshed = Snapshot {
            workspaces: vec![workspace(
                "w1",
                "after-event",
                vec![shell("s1", "w1", "one"), shell("s2", "w1", "two")],
            )],
            focused_terminal: None,
            scheduler: None,
        };
        let fallback_refreshed = Snapshot {
            workspaces: vec![workspace(
                "w1",
                "after-fallback",
                vec![shell("s1", "w1", "one"), shell("s2", "w1", "two")],
            )],
            focused_terminal: None,
            scheduler: None,
        };
        let server = thread::spawn({
            let initial = initial.clone();
            let event_refreshed = event_refreshed.clone();
            let fallback_refreshed = fallback_refreshed.clone();
            move || {
                for expected in [
                    "ping",
                    "baseline",
                    "protocol_ping",
                    "bounded_support_ping",
                    "execution_seed",
                    "changed_poll",
                    "event_snapshot",
                    "newer_poll",
                    "newer_snapshot",
                    "duplicate_poll",
                    "duplicate_snapshot",
                    "removed_poll",
                    "removed_snapshot",
                    "idle_poll",
                    "fallback_snapshot",
                ] {
                    let (mut stream, _) = listener.accept().unwrap();
                    let request: protocol::Envelope<protocol::Request> =
                        protocol::read_message(&mut stream).unwrap();
                    let response = match (expected, request.message) {
                        ("ping", protocol::Request::Ping) => protocol::Response::Pong,
                        ("protocol_ping", protocol::Request::Ping)
                        | ("bounded_support_ping", protocol::Request::Ping) => {
                            protocol::Response::Pong
                        }
                        (
                            "baseline",
                            protocol::Request::Events {
                                after: None,
                                wait_ms: 0,
                                ..
                            },
                        ) => protocol::Response::Events {
                            stream_id: "stream-1".into(),
                            cursor: EventCursor {
                                stream_id: "stream-1".into(),
                                event_id: 0,
                            },
                            snapshot: Some(initial.clone()),
                            events: Vec::new(),
                        },
                        (
                            "execution_seed",
                            protocol::Request::ListScheduledExecutions {
                                workspace_id: None,
                                schedule_id: None,
                                limit: Some(DASHBOARD_EXECUTION_CACHE_LIMIT),
                            },
                        ) => {
                            let mut execution =
                                scheduled_execution("execution-event", "schedule-1");
                            execution.revision = 3;
                            protocol::Response::ScheduledExecutions {
                                executions: vec![execution],
                                limit: DASHBOARD_EXECUTION_CACHE_LIMIT,
                                truncated: false,
                                schedules: Vec::new(),
                                schedule_limit:
                                    protocol::MAX_SCHEDULED_EXECUTION_SCHEDULE_PROJECTIONS,
                                schedules_truncated: false,
                            }
                        }
                        (
                            "changed_poll",
                            protocol::Request::Events {
                                after: Some(_),
                                wait_ms: 0,
                                ..
                            },
                        ) => protocol::Response::Events {
                            stream_id: "stream-1".into(),
                            cursor: EventCursor {
                                stream_id: "stream-1".into(),
                                event_id: 1,
                            },
                            snapshot: None,
                            events: vec![protocol::DaemonEvent {
                                id: 1,
                                at_ms: 1,
                                kind: protocol::DaemonEventKind::ScheduledExecutionChanged {
                                    workspace_id: "w1".into(),
                                    execution: {
                                        let mut execution =
                                            scheduled_execution("execution-event", "schedule-1");
                                        execution.revision = 2;
                                        execution
                                    },
                                },
                            }],
                        },
                        ("event_snapshot", protocol::Request::Snapshot) => {
                            protocol::Response::Snapshot {
                                snapshot: event_refreshed.clone(),
                            }
                        }
                        (
                            "newer_poll",
                            protocol::Request::Events {
                                after: Some(_),
                                wait_ms: 0,
                                ..
                            },
                        ) => protocol::Response::Events {
                            stream_id: "stream-1".into(),
                            cursor: EventCursor {
                                stream_id: "stream-1".into(),
                                event_id: 2,
                            },
                            snapshot: None,
                            events: vec![protocol::DaemonEvent {
                                id: 2,
                                at_ms: 2,
                                kind: protocol::DaemonEventKind::ScheduledExecutionChanged {
                                    workspace_id: "w1".into(),
                                    execution: {
                                        let mut execution =
                                            scheduled_execution("execution-event", "schedule-1");
                                        execution.revision = 4;
                                        execution.state =
                                            protocol::ScheduledExecutionState::Starting;
                                        execution
                                    },
                                },
                            }],
                        },
                        ("newer_snapshot", protocol::Request::Snapshot) => {
                            protocol::Response::Snapshot {
                                snapshot: event_refreshed.clone(),
                            }
                        }
                        (
                            "duplicate_poll",
                            protocol::Request::Events {
                                after: Some(_),
                                wait_ms: 0,
                                ..
                            },
                        ) => protocol::Response::Events {
                            stream_id: "stream-1".into(),
                            cursor: EventCursor {
                                stream_id: "stream-1".into(),
                                event_id: 3,
                            },
                            snapshot: None,
                            events: vec![protocol::DaemonEvent {
                                id: 3,
                                at_ms: 3,
                                kind: protocol::DaemonEventKind::ScheduledExecutionChanged {
                                    workspace_id: "w1".into(),
                                    execution: {
                                        let mut execution =
                                            scheduled_execution("execution-event", "schedule-1");
                                        execution.revision = 4;
                                        execution.state =
                                            protocol::ScheduledExecutionState::Cancelled;
                                        execution.reason = Some(
                                            protocol::ScheduledExecutionReason::CancelledByUser,
                                        );
                                        execution.ended_at_ms = Some(3);
                                        execution
                                    },
                                },
                            }],
                        },
                        ("duplicate_snapshot", protocol::Request::Snapshot) => {
                            protocol::Response::Snapshot {
                                snapshot: event_refreshed.clone(),
                            }
                        }
                        (
                            "removed_poll",
                            protocol::Request::Events {
                                after: Some(_),
                                wait_ms: 0,
                                ..
                            },
                        ) => protocol::Response::Events {
                            stream_id: "stream-1".into(),
                            cursor: EventCursor {
                                stream_id: "stream-1".into(),
                                event_id: 4,
                            },
                            snapshot: None,
                            events: vec![protocol::DaemonEvent {
                                id: 4,
                                at_ms: 4,
                                kind: protocol::DaemonEventKind::AgentScheduleRemoved {
                                    workspace_id: "w1".into(),
                                    schedule_id: "schedule-1".into(),
                                },
                            }],
                        },
                        ("removed_snapshot", protocol::Request::Snapshot) => {
                            protocol::Response::Snapshot {
                                snapshot: event_refreshed.clone(),
                            }
                        }
                        (
                            "idle_poll",
                            protocol::Request::Events {
                                after: Some(_),
                                wait_ms: 0,
                                ..
                            },
                        ) => protocol::Response::Events {
                            stream_id: "stream-1".into(),
                            cursor: EventCursor {
                                stream_id: "stream-1".into(),
                                event_id: 1,
                            },
                            snapshot: None,
                            events: Vec::new(),
                        },
                        ("fallback_snapshot", protocol::Request::Snapshot) => {
                            protocol::Response::Snapshot {
                                snapshot: fallback_refreshed.clone(),
                            }
                        }
                        (_, request) => panic!("unexpected {expected} request: {request:?}"),
                    };
                    protocol::write_message(
                        &mut stream,
                        &protocol::Envelope::with_version(request.version, response),
                    )
                    .unwrap();
                }
            }
        });
        let client = client::Client::from_socket_path(socket);
        let mut refresh = DashboardRefresh::baseline(&client).unwrap();

        let (snapshot, stream_changed) = refresh.check(&client).unwrap().unwrap();
        assert_eq!(snapshot, event_refreshed);
        assert!(!stream_changed);
        assert_eq!(refresh.executions().len(), 1);
        assert_eq!(refresh.executions()[0].id, "execution-event");
        assert_eq!(refresh.executions()[0].revision, 3);
        assert_eq!(
            refresh.executions()[0].state,
            protocol::ScheduledExecutionState::Active
        );

        refresh.check(&client).unwrap().unwrap();
        assert_eq!(refresh.executions()[0].revision, 4);
        assert_eq!(
            refresh.executions()[0].state,
            protocol::ScheduledExecutionState::Starting
        );

        refresh.check(&client).unwrap().unwrap();
        assert_eq!(refresh.executions()[0].revision, 4);
        assert_eq!(
            refresh.executions()[0].state,
            protocol::ScheduledExecutionState::Starting
        );

        refresh.check(&client).unwrap().unwrap();
        assert!(refresh.executions().is_empty());

        refresh.last_snapshot_at = Instant::now() - DASHBOARD_FALLBACK_REFRESH_INTERVAL;
        let (snapshot, stream_changed) = refresh.check(&client).unwrap().unwrap();

        assert_eq!(snapshot, fallback_refreshed);
        assert!(!stream_changed);
        server.join().unwrap();
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn dashboard_protocol_23_and_24_do_not_request_unbounded_execution_history() {
        for version in [23, 24] {
            let directory = env::temp_dir().join(format!(
                "boomux-dashboard-old-protocol-{version}-{}",
                Uuid::new_v4()
            ));
            fs::create_dir_all(&directory).unwrap();
            let socket = directory.join("daemon.sock");
            let listener = UnixListener::bind(&socket).unwrap();
            let snapshot = Snapshot {
                workspaces: Vec::new(),
                focused_terminal: None,
                scheduler: None,
            };
            let server = thread::spawn(move || {
                for phase in 0..2 {
                    for requested_version in (version..=protocol::PROTOCOL_VERSION).rev() {
                        let (mut stream, _) = listener.accept().unwrap();
                        let request: protocol::Envelope<protocol::Request> =
                            protocol::read_message(&mut stream).unwrap();
                        assert_eq!(request.version, requested_version);
                        assert!(matches!(request.message, protocol::Request::Ping));
                        protocol::write_message(
                            &mut stream,
                            &protocol::Envelope::with_version(version, protocol::Response::Pong),
                        )
                        .unwrap();
                    }
                    if phase != 0 {
                        continue;
                    }
                    let (mut stream, _) = listener.accept().unwrap();
                    let request: protocol::Envelope<protocol::Request> =
                        protocol::read_message(&mut stream).unwrap();
                    assert!(matches!(
                        request.message,
                        protocol::Request::Events {
                            after: None,
                            wait_ms: 0,
                            ..
                        }
                    ));
                    protocol::write_message(
                        &mut stream,
                        &protocol::Envelope::with_version(
                            version,
                            protocol::Response::Events {
                                stream_id: format!("stream-{version}"),
                                cursor: EventCursor {
                                    stream_id: format!("stream-{version}"),
                                    event_id: 0,
                                },
                                snapshot: Some(snapshot.clone()),
                                events: Vec::new(),
                            },
                        ),
                    )
                    .unwrap();
                }
            });
            let client = client::Client::from_socket_path(socket);
            let refresh = DashboardRefresh::baseline(&client).unwrap();
            assert_eq!(refresh.negotiated_protocol, version);
            assert!(refresh.executions().is_empty());
            server.join().unwrap();
            fs::remove_dir_all(directory).unwrap();
        }
    }

    #[test]
    fn failed_execution_reseed_preserves_cache_and_retries_on_next_check() {
        let directory = env::temp_dir().join(format!("boomux-dashboard-reseed-{}", Uuid::new_v4()));
        fs::create_dir_all(&directory).unwrap();
        let socket = directory.join("daemon.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let snapshot = Snapshot {
            workspaces: Vec::new(),
            focused_terminal: None,
            scheduler: None,
        };
        let server = thread::spawn(move || {
            for expected in [
                "feature_ping",
                "baseline",
                "protocol_ping",
                "seed_support",
                "seed",
                "failed_support",
                "failed_seed",
                "retry_support",
                "retry_seed",
            ] {
                let (mut stream, _) = listener.accept().unwrap();
                let request: protocol::Envelope<protocol::Request> =
                    protocol::read_message(&mut stream).unwrap();
                if expected == "failed_seed" {
                    assert!(matches!(
                        request.message,
                        protocol::Request::ListScheduledExecutions { .. }
                    ));
                    drop(stream);
                    continue;
                }
                let response = match (expected, request.message) {
                    (
                        "feature_ping" | "protocol_ping" | "seed_support" | "failed_support"
                        | "retry_support",
                        protocol::Request::Ping,
                    ) => protocol::Response::Pong,
                    (
                        "baseline",
                        protocol::Request::Events {
                            after: None,
                            wait_ms: 0,
                            ..
                        },
                    ) => protocol::Response::Events {
                        stream_id: "stream-reseed".into(),
                        cursor: EventCursor {
                            stream_id: "stream-reseed".into(),
                            event_id: 0,
                        },
                        snapshot: Some(snapshot.clone()),
                        events: Vec::new(),
                    },
                    ("seed" | "retry_seed", protocol::Request::ListScheduledExecutions { .. }) => {
                        let mut execution = scheduled_execution("execution-1", "schedule-1");
                        execution.revision = if expected == "seed" { 4 } else { 5 };
                        protocol::Response::ScheduledExecutions {
                            executions: vec![execution],
                            limit: DASHBOARD_EXECUTION_CACHE_LIMIT,
                            truncated: false,
                            schedules: Vec::new(),
                            schedule_limit: protocol::MAX_SCHEDULED_EXECUTION_SCHEDULE_PROJECTIONS,
                            schedules_truncated: false,
                        }
                    }
                    (_, request) => panic!("unexpected reseed request: {request:?}"),
                };
                protocol::write_message(
                    &mut stream,
                    &protocol::Envelope::with_version(request.version, response),
                )
                .unwrap();
            }
        });
        let client = client::Client::from_socket_path(socket);
        let mut refresh = DashboardRefresh::baseline(&client).unwrap();
        assert_eq!(refresh.executions()[0].revision, 4);
        refresh.needs_reseed = true;

        assert!(refresh.check(&client).is_err());
        assert!(refresh.needs_reseed);
        assert_eq!(refresh.executions()[0].revision, 4);
        assert!(refresh.check(&client).unwrap().is_some());
        assert!(!refresh.needs_reseed);
        assert_eq!(refresh.executions()[0].revision, 5);
        server.join().unwrap();
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn parses_paths_and_native_hidden_commands() {
        let cli = Cli::try_parse_from(["boomux", "."]).unwrap();
        assert_eq!(cli.path, Some(PathBuf::from(".")));
        assert!(Cli::try_parse_from(["boomux", "daemon", "run"]).is_ok());
        assert!(Cli::try_parse_from(["boomux", "daemon", "restart"]).is_ok());
        assert!(Cli::try_parse_from(["boomux", "daemon", "stop"]).is_ok());
        let cli =
            Cli::try_parse_from(["boomux", "__attach", "s1", "--takeover", "--restart-exited"])
                .unwrap();
        assert!(matches!(
            cli.command,
            Some(Commands::Attach {
                takeover: true,
                restart_exited: true,
                ..
            })
        ));
        let cli = Cli::try_parse_from([
            "boomux",
            "__attach",
            "s1",
            "--takeover",
            "--expected-run-id",
            "r1",
        ])
        .unwrap();
        assert!(matches!(
            cli.command,
            Some(Commands::Attach {
                restart_exited: false,
                expected_run_id: Some(run_id),
                ..
            }) if run_id == "r1"
        ));
    }

    #[test]
    fn parses_node_rekey_and_requires_exact_confirmation() {
        let cli = Cli::try_parse_from(["boomux", "node", "rekey"]).unwrap();
        assert!(matches!(
            cli.command.as_ref(),
            Some(Commands::Node {
                command: NodeCommands::Rekey
            })
        ));
        assert_eq!(cli.command_descriptor().key, "node.rekey");

        let node_id = "550e8400-e29b-41d4-a716-446655440000";
        validate_rekey_confirmation(node_id, &format!("{node_id}\n")).unwrap();
        assert_eq!(
            validate_rekey_confirmation(node_id, "550e8400-e29b-41d4-a716-446655440001\n")
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidInput
        );
    }

    #[test]
    fn node_snapshot_is_separate_json_command_with_structural_identities() {
        let cli = Cli::try_parse_from(["boomux", "--json", "node", "snapshot", "work"]).unwrap();
        assert!(matches!(
            cli.command.as_ref(),
            Some(Commands::Node {
                command: NodeCommands::Snapshot {
                    selector: Some(selector)
                }
            }) if selector == "work"
        ));
        assert_eq!(cli.command_descriptor().key, "node.snapshot");

        let qualified = qualify_resource_identities(
            serde_json::json!({
                "id": "resource",
                "workspace_id": "workspace",
                "external_session_id": "private-host-id"
            }),
            "node",
        );
        assert_eq!(
            qualified["id"],
            serde_json::json!({"node_id": "node", "inner_id": "resource"})
        );
        assert_eq!(qualified["workspace_id"]["node_id"], "node");
        assert_eq!(qualified["external_session_id"], "private-host-id");
    }

    #[test]
    fn accepts_path_options_and_named_subcommands() {
        let cli = Cli::try_parse_from(["boomux", ".", "--name", "feature", "--new"]).unwrap();
        assert_eq!(cli.name.as_deref(), Some("feature"));
        assert!(cli.new_window);
        assert!(cli.startup_command.is_empty());
        assert!(Cli::try_parse_from(["boomux", "--name", "feature"]).is_err());
        assert!(matches!(
            Cli::try_parse_from(["boomux", "doctor"]).unwrap().command,
            Some(Commands::Doctor)
        ));
        assert!(matches!(
            Cli::try_parse_from(["boomux", "close", "tests"])
                .unwrap()
                .command,
            Some(Commands::Close { target }) if target == "tests"
        ));
    }

    #[test]
    fn parses_global_json_for_supported_integration_commands() {
        for arguments in [
            vec!["boomux", "--json", "capabilities"],
            vec!["boomux", "list", "--json"],
            vec!["boomux", "workspace", "list", "--json"],
            vec!["boomux", "shell", "suggest-name", "project", "--json"],
            vec![
                "boomux",
                "launcher",
                "list",
                "--workspace",
                "project",
                "--json",
            ],
            vec!["boomux", "daemon", "status", "--json"],
            vec!["boomux", "integration", "list", "--json"],
            vec!["boomux", "integration", "status", "opencode", "--json"],
            vec!["boomux", "integration", "install", "pi", "--json"],
        ] {
            let cli = Cli::try_parse_from(arguments).unwrap();
            assert!(cli.json);
            assert_eq!(cli.command_descriptor().output, OutputMode::Json);
        }
        let cli = Cli::try_parse_from(["boomux", "workspace", "create", "test", "--json"]).unwrap();
        assert_eq!(cli.command_descriptor().output, OutputMode::HumanOnly);
        let cli = Cli::try_parse_from(["boomux", "opencode", "install", "--json"]).unwrap();
        assert_eq!(cli.command_descriptor().output, OutputMode::HumanOnly);
        assert!(
            Cli::try_parse_from([
                "boomux",
                "events",
                "--after",
                "00000000-0000-0000-0000-000000000000:4",
                "--wait-ms",
                "1000",
                "--json",
            ])
            .is_ok()
        );
        assert!(
            Cli::try_parse_from([
                "boomux",
                "read",
                "shell",
                "--run-id",
                "run",
                "--after-revision",
                "4",
                "--wait-ms",
                "1000",
            ])
            .is_ok()
        );
        assert!(Cli::try_parse_from(["boomux", "read", "shell", "--run-id", "run"]).is_err());
    }

    #[test]
    fn parses_exact_startup_command_after_separator() {
        let cli = Cli::try_parse_from([
            "boomux", "--name", "feature", ".", "--", "cargo", "watch", "-x", "test",
        ])
        .unwrap();

        assert_eq!(cli.path, Some(PathBuf::from(".")));
        assert_eq!(cli.startup_command, ["cargo", "watch", "-x", "test"]);
        assert!(cli.command.is_none());
        assert!(Cli::try_parse_from(["boomux", "--", "cargo", "watch"]).is_err());
    }

    #[test]
    fn detects_json_only_before_the_command_separator() {
        assert!(requests_json(
            ["workspace", "list", "--json"].map(OsString::from)
        ));
        assert!(!requests_json(
            [".", "--", "/bin/echo", "--json"].map(OsString::from)
        ));
    }

    #[test]
    fn parses_workspace_and_shell_lifecycle_commands() {
        assert!(matches!(
            Cli::try_parse_from(["boomux", "workspace", "create", "project"])
                .unwrap()
                .command,
            Some(Commands::Workspace {
                command: WorkspaceCommands::Create { name, cwd: None }
            }) if name == "project"
        ));
        assert!(matches!(
            Cli::try_parse_from([
                "boomux",
                "workspace",
                "create",
                "project",
                "--cwd",
                "/tmp"
            ])
            .unwrap()
            .command,
            Some(Commands::Workspace {
                command: WorkspaceCommands::Create {
                    name,
                    cwd: Some(cwd)
                }
            }) if name == "project" && cwd == Path::new("/tmp")
        ));
        let cli = Cli::try_parse_from([
            "boomux",
            "launcher",
            "create",
            "editor",
            "--workspace",
            "project",
            "--cwd",
            "/tmp",
            "--",
            "zeditor",
            ".",
        ])
        .unwrap();
        assert!(matches!(
            cli.command,
            Some(Commands::Launcher {
                command: LauncherCommands::Create {
                    name,
                    workspace,
                    cwd,
                    command,
                }
            }) if name == "editor"
                && workspace == "project"
                && cwd == Path::new("/tmp")
                && command == ["zeditor", "."]
        ));
        let cli = Cli::try_parse_from([
            "boomux",
            "launcher",
            "invoke",
            "editor",
            "--workspace",
            "project",
        ])
        .unwrap();
        assert_eq!(cli.command_descriptor().output, OutputMode::HumanOnly);
        assert!(matches!(
            cli.command,
            Some(Commands::Launcher {
                command: LauncherCommands::Invoke {
                    target,
                    workspace: Some(workspace),
                    ..
                }
            }) if target == "editor" && workspace == "project"
        ));
        assert!(matches!(
            Cli::try_parse_from(["boomux", "workspace", "open", "project"])
                .unwrap()
                .command,
            Some(Commands::Workspace {
                command: WorkspaceCommands::Open { target, .. }
            }) if target == "project"
        ));
        let cli = Cli::try_parse_from([
            "boomux", "shell", "create", "project", "--name", "tests", "--cwd", "/tmp", "--",
            "cargo", "test",
        ])
        .unwrap();
        assert!(matches!(
            cli.command,
            Some(Commands::Shell {
                command: ShellCommands::Create {
                    workspace,
                    name: Some(name),
                    cwd,
                    command,
                }
            }) if workspace == "project"
                && name == "tests"
                && cwd.as_deref() == Some(Path::new("/tmp"))
                && command == ["cargo", "test"]
        ));
        assert!(matches!(
            Cli::try_parse_from(["boomux", "shell", "suggest-name", "project"])
                .unwrap()
                .command,
            Some(Commands::Shell {
                command: ShellCommands::SuggestName { workspace, .. }
            }) if workspace == "project"
        ));
        assert!(matches!(
            Cli::try_parse_from(["boomux", "shell", "create", "project"])
                .unwrap()
                .command,
            Some(Commands::Shell {
                command: ShellCommands::Create { cwd: None, .. }
            })
        ));
    }

    #[test]
    fn parses_agent_runtime_commands_and_json_support() {
        let cli = Cli::try_parse_from([
            "boomux",
            "agent",
            "register",
            "opencode",
            "--integration",
            "plugin",
            "--shell-id",
            "s1",
            "--run-id",
            "r1",
            "--state",
            "working",
            "--authority",
            "lifecycle-integration",
            "--evidence",
            "tool call",
            "--confidence",
            "95",
        ])
        .unwrap();
        assert!(matches!(
            cli.command,
            Some(Commands::Agent {
                command: AgentCommands::Register(AgentRegistrationArgs { confidence: 95, .. })
            })
        ));

        let ensure = Cli::try_parse_from([
            "boomux",
            "agent",
            "ensure",
            "OpenCode",
            "--integration",
            "opencode",
            "--external-session-id",
            "session-1",
            "--shell-id",
            "s1",
            "--run-id",
            "r1",
            "--state",
            "working",
            "--authority",
            "lifecycle-integration",
            "--evidence",
            "tool call",
            "--confidence",
            "100",
            "--json",
        ])
        .unwrap();
        assert_eq!(ensure.command_descriptor().key, "agent.ensure");
        assert_eq!(ensure.command_descriptor().output, OutputMode::Json);

        let cli = Cli::try_parse_from([
            "boomux",
            "agent",
            "list",
            "--workspace",
            "project",
            "--json",
        ])
        .unwrap();
        assert_eq!(cli.command_descriptor().output, OutputMode::Json);
        let cli = Cli::try_parse_from(["boomux", "agent", "get", "a1", "--json"]).unwrap();
        assert_eq!(cli.command_descriptor().key, "agent.inspect");
        assert_eq!(cli.command_descriptor().output, OutputMode::Json);
        let cli = Cli::try_parse_from([
            "boomux",
            "agent",
            "wait",
            "a1",
            "--after-revision",
            "3",
            "--wait-ms",
            "5000",
            "--json",
        ])
        .unwrap();
        assert_eq!(cli.command_descriptor().key, "agent.wait");
        assert_eq!(cli.command_descriptor().output, OutputMode::Json);
        assert!(matches!(
            cli.command,
            Some(Commands::Agent {
                command: AgentCommands::Wait {
                    agent_id,
                    after_revision: 3,
                    wait_ms: 5000,
                }
            }) if agent_id == "a1"
        ));
        let cli = Cli::try_parse_from([
            "boomux",
            "agent",
            "report",
            "a1",
            "--state",
            "done",
            "--authority",
            "lifecycle-integration",
            "--evidence",
            "complete",
            "--confidence",
            "100",
            "--json",
        ])
        .unwrap();
        assert_eq!(cli.command_descriptor().key, "agent.report");
        assert_eq!(cli.command_descriptor().output, OutputMode::Json);
        let register = Cli::try_parse_from([
            "boomux",
            "agent",
            "register",
            "agent",
            "--integration",
            "test",
            "--state",
            "working",
            "--authority",
            "process-adapter",
            "--evidence",
            "running",
            "--confidence",
            "80",
            "--json",
        ])
        .unwrap();
        assert_eq!(register.command_descriptor().key, "agent.register");
        assert_eq!(register.command_descriptor().output, OutputMode::Json);
        let unnamed_register = Cli::try_parse_from([
            "boomux",
            "agent",
            "register",
            "--integration",
            "test",
            "--state",
            "working",
            "--authority",
            "process-adapter",
            "--evidence",
            "running",
            "--confidence",
            "80",
        ])
        .unwrap();
        assert!(matches!(
            unnamed_register.command,
            Some(Commands::Agent {
                command: AgentCommands::Register(AgentRegistrationArgs { name: None, .. })
            })
        ));
        assert!(
            Cli::try_parse_from([
                "boomux",
                "agent",
                "report",
                "a1",
                "--state",
                "done",
                "--authority",
                "daemon-lifecycle",
                "--evidence",
                "complete",
                "--confidence",
                "100",
            ])
            .is_err()
        );
        assert!(
            Cli::try_parse_from([
                "boomux",
                "agent",
                "report",
                "a1",
                "--state",
                "done",
                "--authority",
                "lifecycle-integration",
                "--evidence",
                "complete",
                "--confidence",
                "101",
            ])
            .is_err()
        );

        let supervise = Cli::try_parse_from([
            "boomux",
            "agent",
            "supervise",
            "OpenCode",
            "--integration",
            "opencode",
            "--external-session-id",
            "session-1",
            "--shell-id",
            "s1",
            "--run-id",
            "r1",
            "--",
            "agent-bin",
            "literal; argument",
        ])
        .unwrap();
        assert_eq!(supervise.command_descriptor().key, "agent.supervise");
        assert_eq!(supervise.command_descriptor().output, OutputMode::HumanOnly);
        assert!(matches!(
            supervise.command,
            Some(Commands::Agent {
                command: AgentCommands::Supervise(AgentSuperviseArgs { command, .. })
            }) if command == ["agent-bin", "literal; argument"]
        ));
        let unnamed_supervise = Cli::try_parse_from([
            "boomux",
            "agent",
            "supervise",
            "--integration",
            "opencode",
            "--external-session-id",
            "session-1",
            "--shell-id",
            "s1",
            "--run-id",
            "r1",
            "--",
            "agent-bin",
        ])
        .unwrap();
        assert!(matches!(
            unnamed_supervise.command,
            Some(Commands::Agent {
                command: AgentCommands::Supervise(AgentSuperviseArgs { name: None, .. })
            })
        ));
        assert!(
            Cli::try_parse_from([
                "boomux",
                "agent",
                "supervise",
                "agent",
                "--integration",
                "test",
                "--external-session-id",
                "session-1",
            ])
            .is_err()
        );
        assert!(NON_PROTOCOL_FEATURES.contains(&"process_adapters"));
    }

    #[test]
    fn parses_attention_queue_commands_and_json_support() {
        let list = Cli::try_parse_from([
            "boomux",
            "attention",
            "list",
            "--workspace",
            "project",
            "--json",
        ])
        .unwrap();
        assert_eq!(list.command_descriptor().key, "attention.list");
        assert_eq!(list.command_descriptor().output, OutputMode::Json);

        let acknowledge = Cli::try_parse_from([
            "boomux",
            "attention",
            "acknowledge",
            "a1",
            "--observation-revision",
            "7",
            "--json",
        ])
        .unwrap();
        assert_eq!(
            acknowledge.command_descriptor().key,
            "attention.acknowledge"
        );
        assert_eq!(acknowledge.command_descriptor().output, OutputMode::Json);
        assert_eq!(
            sanitize_table_cell("approval\tneeded\nnow\u{7}"),
            "approval needed now "
        );
    }

    #[test]
    fn parses_notification_test_commands_without_json_support() {
        let blocked = Cli::try_parse_from(["boomux", "notification", "test", "blocked"]).unwrap();
        assert_eq!(blocked.command_descriptor().key, "notification.test");
        assert_eq!(blocked.command_descriptor().output, OutputMode::HumanOnly);
        assert!(matches!(
            blocked.command,
            Some(Commands::Notification {
                command: NotificationCommands::Test {
                    reason: CliNotificationReason::Blocked
                }
            })
        ));

        assert!(Cli::try_parse_from(["boomux", "notification", "test", "unknown"]).is_err());
    }

    #[test]
    fn child_exit_outcomes_map_to_unix_cli_codes() {
        assert_eq!(
            CliExit::Child(process_adapter::ProcessExit::Code(23)).code(),
            ExitCode::from(23)
        );
        assert_eq!(
            CliExit::Child(process_adapter::ProcessExit::Signal(9)).code(),
            ExitCode::from(137)
        );
    }

    #[test]
    fn parses_session_discovery_commands_and_json_support() {
        let list = Cli::try_parse_from([
            "boomux",
            "session",
            "list",
            "--workspace",
            "project",
            "--json",
        ])
        .unwrap();
        assert_eq!(list.command_descriptor().key, "session.list");
        assert_eq!(list.command_descriptor().output, OutputMode::Json);
        assert!(matches!(
            list.command,
            Some(Commands::Session {
                command: SessionCommands::List {
                    workspace: Some(workspace),
                    ..
                }
            }) if workspace == "project"
        ));

        let inspect =
            Cli::try_parse_from(["boomux", "session", "get", "opaque", "--json"]).unwrap();
        assert_eq!(inspect.command_descriptor().key, "session.inspect");
        assert_eq!(inspect.command_descriptor().output, OutputMode::Json);

        assert!(Cli::try_parse_from(["boomux", "session", "read", "opaque"]).is_err());
    }

    #[test]
    fn session_commands_require_protocol_twelve() {
        let error = validate_session_protocol(11).unwrap_err();
        assert_eq!(
            cli_output::classify_for_test("session.list", error.as_ref()),
            "unsupported_version"
        );
        assert!(validate_session_protocol(12).is_ok());
    }

    #[test]
    fn agent_context_requires_nonempty_shell_and_run_ids() {
        assert_eq!(
            resolve_agent_context(
                Some(" explicit-shell ".into()),
                None,
                Some("environment-shell".into()),
                Some(" environment-run ".into()),
            )
            .unwrap(),
            ("explicit-shell".into(), "environment-run".into())
        );
        assert!(resolve_agent_context(Some("s1".into()), None, None, None).is_err());
        assert!(
            resolve_agent_context(Some(" ".into()), Some("r1".into()), Some("s1".into()), None,)
                .is_err()
        );
    }

    #[test]
    fn event_snapshot_json_includes_stable_agent_data() {
        let mut project = workspace("w1", "project", vec![shell("s1", "w1", "shell")]);
        project.agents.push(agent("a1", "w1", "s1"));

        let value = json_snapshot(Snapshot {
            workspaces: vec![project],
            focused_terminal: None,
            scheduler: None,
        })
        .unwrap();

        assert_eq!(value["workspaces"][0]["agents"][0]["id"], "a1");
        assert_eq!(
            value["workspaces"][0]["agents"][0]["observation"]["authority"],
            "lifecycle_integration"
        );
    }

    #[test]
    fn resolves_workspaces_and_shell_names_for_cli_commands() {
        let mut project = workspace("w1", "project", vec![shell("s1", "w1", "tests")]);
        project.launchers.push(launcher("l1", "w1", "editor"));
        let snapshot = Snapshot {
            workspaces: vec![workspace("w2", "w1", vec![]), project],
            focused_terminal: None,
            scheduler: None,
        };

        assert_eq!(
            resolve_workspace_target(&snapshot.workspaces, "project")
                .unwrap()
                .id,
            "w1"
        );
        assert_eq!(
            resolve_workspace_target(&snapshot.workspaces, "w1")
                .unwrap()
                .name,
            "project"
        );
        assert_eq!(
            resolve_cli_shell(&snapshot, "tests", Some("project"))
                .unwrap()
                .id,
            "s1"
        );
        assert_eq!(
            resolve_cli_launcher(&snapshot, "editor", Some("project"))
                .unwrap()
                .id,
            "l1"
        );
        assert_eq!(
            resolve_cli_launcher(&snapshot, "l1", None).unwrap().name,
            "editor"
        );
        assert!(resolve_workspace_target(&snapshot.workspaces, "missing").is_err());
    }

    #[test]
    fn matches_native_workspace_by_name_only() {
        let workspace = workspace("w1", "project", vec![]);
        assert!(find_workspace(&[workspace], "project").is_some());
    }

    #[test]
    fn resolves_explicit_and_generated_cli_names() {
        assert_eq!(
            cli_name_or_generated(Some("  explicit  ".into()), "agent").unwrap(),
            "explicit"
        );
        let generated = cli_name_or_generated(None, "agent").unwrap();
        let (adjective, noun) = generated.split_once('-').unwrap();
        assert!(adjective.bytes().all(|byte| byte.is_ascii_lowercase()));
        assert!(noun.bytes().all(|byte| byte.is_ascii_lowercase()));
    }

    #[test]
    fn generated_shell_name_exhaustion_is_typed() {
        let mut names = Vec::new();
        while let Some(name) = generated_names::random_excluding(names.iter().map(String::as_str)) {
            names.push(name);
        }

        let error = generated_shell_name(names.iter().map(String::as_str)).unwrap_err();
        assert_eq!(
            cli_output::classify_for_test("shell.suggest-name", error.as_ref()),
            "already_exists"
        );
    }

    #[test]
    fn resolves_shell_ids_and_contextual_names() {
        let snapshot = Snapshot {
            workspaces: vec![
                workspace("w1", "one", vec![shell("s1", "w1", "tests")]),
                workspace("w2", "two", vec![shell("s2", "w2", "tests")]),
            ],
            focused_terminal: None,
            scheduler: None,
        };
        assert_eq!(
            resolve_shell_target(&snapshot, None, "s2").unwrap().id,
            "s2"
        );
        assert_eq!(
            resolve_shell_target(&snapshot, Some("w1"), "tests")
                .unwrap()
                .id,
            "s1"
        );
        assert!(resolve_shell_target(&snapshot, None, "tests").is_err());
    }

    #[test]
    fn builds_shell_spec_with_exact_startup_arguments() {
        let command = vec!["sh".into(), "-lc".into(), "printf '%s' test".into()];

        let spec = shell_spec("checks", Path::new("/tmp/project"), &command);

        assert_eq!(spec.name, "checks");
        assert_eq!(spec.cwd, Path::new("/tmp/project"));
        assert_eq!(spec.command, command);
    }

    #[test]
    fn empty_workspace_view_has_no_shells() {
        let views = dashboard_views(
            &[workspace("w1", "empty", Vec::new())],
            &mut git::Cache::default(),
        );

        assert!(views[0].items.is_empty());
        assert!(views[0].sessions.is_empty());
    }

    #[test]
    fn workspace_view_groups_external_session_occurrences_across_runs() {
        let mut second_shell = shell("s2", "w1", "review");
        second_shell.run.as_mut().unwrap().id = "r2".into();
        let mut workspace = workspace(
            "w1",
            "project",
            vec![shell("s1", "w1", "build"), second_shell],
        );
        let mut first = agent("agent-old", "w1", "s1");
        first.started_at_ms = 10;
        first.ended_at_ms = Some(20);
        first.observation.state = AgentState::Inactive;
        first.observation.observed_at_ms = 20;
        let mut second = agent("agent-new", "w1", "s2");
        second.run_id = "r2".into();
        second.started_at_ms = 30;
        second.observation.state = AgentState::Blocked;
        second.observation.evidence = "approval needed".into();
        second.observation.observed_at_ms = 40;
        workspace.agents = vec![second, first];

        let views = dashboard_views(&[workspace], &mut git::Cache::default());

        assert_eq!(views[0].sessions.len(), 1);
        let session = &views[0].sessions[0];
        assert!(Uuid::parse_str(&session.id).is_ok());
        assert_eq!(session.state, tui::AgentDisplayState::Blocked);
        assert!(session.state_is_current);
        assert_eq!(session.runs.len(), 2);
        assert_eq!(session.runs[0].shell_name.as_deref(), Some("build"));
        assert_eq!(session.runs[1].shell_name.as_deref(), Some("review"));
        assert_eq!(
            session.runs[1].directory.as_deref(),
            Some(Path::new("/tmp/project"))
        );
    }

    #[test]
    fn enriches_only_the_matching_integration_and_external_session() {
        let mut workspace = workspace("w1", "project", vec![shell("s1", "w1", "agent")]);
        workspace.agents = vec![agent("agent-1", "w1", "s1")];
        let mut views = dashboard_views(&[workspace], &mut git::Cache::default());

        dashboard_projection::enrich_session_titles_with(
            &mut views,
            |integration, directory, external_id| {
                (integration == "opencode"
                    && directory == Path::new("/tmp/project")
                    && external_id == "external-1")
                    .then(|| "Review async title cache".into())
            },
        );

        assert_eq!(views[0].sessions[0].label, "Review async title cache");
    }

    #[test]
    fn enriches_from_the_newest_available_session_directory() {
        let mut views = vec![tui::WorkspaceView {
            node: tui::NodeView {
                id: String::new(),
                alias: "local".into(),
                local: true,
                health: protocol::NodeProjectionHealthCode::Online,
                current: true,
                stale: false,
                observed_at_ms: 0,
                observed_protocol_version: None,
                observed_capabilities: Vec::new(),
                scheduler: protocol::SchedulerHealth {
                    state: protocol::SchedulerState::Active,
                    max_concurrent: 4,
                    active_executions: 0,
                },
            },
            id: "w1".into(),
            name: "project".into(),
            default_cwd: None,
            items: Vec::new(),
            agent_state_counts: agent_attention_projection::AgentStateCounts::default(),
            attention_count: 0,
            attention: Vec::new(),
            sessions: vec![tui::AgentSessionView {
                id: "session".into(),
                label: "opencode".into(),
                integration: "opencode".into(),
                external_session_id: Some("external-1".into()),
                state: tui::AgentDisplayState::Inactive,
                state_is_current: false,
                last_at_ms: 30,
                source_cwd: Some("/tmp/project".into()),
                runs: vec![
                    tui::AgentSessionRunView {
                        agent_id: "old-agent".into(),
                        shell_name: Some("old-shell".into()),
                        directory: Some("/tmp/project".into()),
                    },
                    tui::AgentSessionRunView {
                        agent_id: "new-agent".into(),
                        shell_name: None,
                        directory: None,
                    },
                ],
            }],
        }];

        dashboard_projection::enrich_session_titles_with(&mut views, |_, directory, _| {
            (directory == Path::new("/tmp/project")).then(|| "Historical title".into())
        });

        assert_eq!(views[0].sessions[0].label, "Historical title");
    }

    #[test]
    fn workspace_view_keeps_agents_without_external_ids_isolated() {
        let mut workspace = workspace("w1", "project", vec![shell("s1", "w1", "agent")]);
        let mut first = agent("agent-a", "w1", "s1");
        first.external_session_id = None;
        let mut second = agent("agent-b", "w1", "s1");
        second.external_session_id = None;
        workspace.agents = vec![first, second];

        let views = dashboard_views(&[workspace], &mut git::Cache::default());

        assert_eq!(views[0].sessions.len(), 2);
        assert!(
            views[0]
                .sessions
                .iter()
                .all(|session| session.runs.len() == 1)
        );
    }

    #[test]
    fn workspace_session_state_prioritizes_active_blocked_occurrences() {
        let mut workspace = workspace("w1", "project", vec![shell("s1", "w1", "agent")]);
        let mut working = agent("working", "w1", "s1");
        working.observation.observed_at_ms = 30;
        let mut blocked = agent("blocked", "w1", "s1");
        blocked.observation.state = AgentState::Blocked;
        blocked.observation.observed_at_ms = 20;
        workspace.agents = vec![working, blocked];

        let views = dashboard_views(&[workspace], &mut git::Cache::default());

        assert_eq!(views[0].sessions[0].state, tui::AgentDisplayState::Blocked);
    }

    #[test]
    fn workspace_session_state_ignores_stale_blocked_occurrence() {
        let mut current_shell = shell("s1", "w1", "agent");
        current_shell.run.as_mut().unwrap().id = "current-run".into();
        let mut workspace = workspace("w1", "project", vec![current_shell]);
        let mut stale = agent("stale", "w1", "s1");
        stale.run_id = "old-run".into();
        stale.observation.state = AgentState::Blocked;
        stale.observation.observed_at_ms = 30;
        let mut current = agent("current", "w1", "s1");
        current.run_id = "current-run".into();
        current.observation.state = AgentState::Working;
        current.observation.observed_at_ms = 20;
        workspace.agents = vec![stale, current];

        let views = dashboard_views(&[workspace], &mut git::Cache::default());

        assert_eq!(views[0].sessions[0].state, tui::AgentDisplayState::Working);
        assert!(views[0].sessions[0].state_is_current);
    }

    #[test]
    fn workspace_session_state_marks_stale_history_as_last_known() {
        let mut workspace = workspace("w1", "project", vec![shell("s1", "w1", "agent")]);
        let mut stale = agent("stale", "w1", "s1");
        stale.run_id = "old-run".into();
        stale.observation.state = AgentState::Blocked;
        workspace.agents = vec![stale];

        let views = dashboard_views(&[workspace], &mut git::Cache::default());

        assert_eq!(views[0].sessions[0].state, tui::AgentDisplayState::Blocked);
        assert!(!views[0].sessions[0].state_is_current);
    }

    #[test]
    fn workspace_session_id_is_stable_when_equal_timestamp_occurrence_is_added() {
        let mut workspace = workspace("w1", "project", vec![shell("s1", "w1", "agent")]);
        let mut first = agent("agent-z", "w1", "s1");
        first.started_at_ms = 10;
        workspace.agents = vec![first.clone()];
        let initial = session_projection::project_workspaces(std::slice::from_ref(&workspace));
        let initial_id = dashboard_projection::session_views(&initial)[0].id.clone();

        let mut added = agent("agent-a", "w1", "s1");
        added.started_at_ms = 10;
        workspace.agents.push(added);
        let projected = session_projection::project_workspaces(std::slice::from_ref(&workspace));
        let sessions = dashboard_projection::session_views(&projected);

        assert_eq!(sessions[0].id, initial_id);
        assert_eq!(sessions[0].runs.len(), 2);
        assert!(Uuid::parse_str(&sessions[0].id).is_ok());
    }

    #[test]
    fn workspace_session_catalog_retains_inactive_and_done_sessions() {
        let mut workspace = workspace("w1", "project", vec![]);
        let mut inactive = agent("inactive", "w1", "removed-a");
        inactive.external_session_id = Some("inactive-session".into());
        inactive.ended_at_ms = Some(20);
        inactive.observation.state = AgentState::Inactive;
        inactive.observation.observed_at_ms = 20;
        let mut done = agent("done", "w1", "removed-b");
        done.external_session_id = Some("done-session".into());
        done.ended_at_ms = Some(30);
        done.observation.state = AgentState::Done;
        done.observation.observed_at_ms = 30;
        workspace.agents = vec![inactive, done];

        let views = dashboard_views(&[workspace], &mut git::Cache::default());

        assert_eq!(views[0].sessions.len(), 2);
        assert!(
            views[0]
                .sessions
                .iter()
                .any(|session| session.state == tui::AgentDisplayState::Inactive)
        );
        assert!(
            views[0]
                .sessions
                .iter()
                .any(|session| session.state == tui::AgentDisplayState::Done)
        );
        assert!(
            views[0]
                .sessions
                .iter()
                .all(|session| session.runs[0].shell_name.is_none())
        );
    }

    #[test]
    fn workspace_view_includes_launcher_details() {
        let mut workspace = workspace("w1", "project", Vec::new());
        workspace.launchers.push(launcher("l1", "w1", "editor"));

        let views = dashboard_views(&[workspace], &mut git::Cache::default());

        let tui::WorkspaceItemView::Launcher(launcher) = &views[0].items[0] else {
            panic!("expected launcher item");
        };
        assert_eq!(launcher.name, "editor");
        assert_eq!(launcher.command, "zeditor .");
        assert_eq!(launcher.argv, ["zeditor", "."]);
        assert_eq!(launcher.directory, "/tmp/project");
    }

    #[test]
    fn workspace_view_classifies_stored_argv_as_a_command() {
        let mut command = shell("s1", "w1", "clock");
        command.command = vec!["watch".into(), "-n".into(), "1".into(), "date".into()];
        let workspace = workspace("w1", "project", vec![command]);

        let views = dashboard_views(&[workspace], &mut git::Cache::default());

        let tui::WorkspaceItemView::Shell(command) = &views[0].items[0] else {
            panic!("expected command-backed shell item");
        };
        assert_eq!(command.name, "clock");
        assert_eq!(command.command, "watch -n 1 date");
        assert_eq!(command.argv, ["watch", "-n", "1", "date"]);
        let run = command.run.as_ref().expect("current run metadata");
        assert_eq!(run.id, "r1");
        assert_eq!(run.generation, 1);
    }

    #[test]
    fn workspace_view_morphs_shell_to_agent_shell_without_adding_a_row() {
        let mut agent_command = shell("s1", "w1", "terminal");
        agent_command.command = vec!["opencode".into()];
        let mut workspace = workspace("w1", "project", vec![agent_command]);
        workspace.launchers.push(launcher("l1", "w1", "editor"));
        workspace.agents.push(agent("a1", "w1", "s1"));

        let views = dashboard_views(&[workspace], &mut git::Cache::default());

        assert_eq!(views[0].items.len(), 2);
        let tui::WorkspaceItemView::AgentShell(tui::AgentShellView { shell, agent, .. }) =
            &views[0].items[0]
        else {
            panic!("expected agent-shell item");
        };
        assert_eq!(shell.id, "s1");
        assert_eq!(shell.name, "terminal");
        assert_eq!(shell.status, "running");
        assert_eq!(shell.directory, "/tmp/project");
        assert_eq!(shell.branch, "-");
        assert_eq!(shell.command, "opencode");
        let agent = agent.as_ref().expect("durable agent");
        assert_eq!(agent.id, "a1");
        assert_eq!(agent.state, tui::AgentDisplayState::Working);
        assert_eq!(
            agent.authority,
            tui::AgentAuthorityDisplay::LifecycleIntegration
        );
        assert_eq!(agent.confidence, 95);
        assert_eq!(agent.updated_at_ms, 11);
        assert_eq!(agent.root_branch, "-");
        assert_eq!(agent.root_worktree, "-");
        assert_eq!(views[0].sessions[0].runs[0].agent_id, "a1");
        assert!(matches!(
            views[0].items[1],
            tui::WorkspaceItemView::Launcher(_)
        ));
    }

    #[test]
    fn workspace_view_hints_exact_opencode_foreground_process_before_first_prompt() {
        let mut hinted = shell("s1", "w1", "terminal");
        hinted.foreground_process = Some("opencode".into());
        let workspace = workspace("w1", "project", vec![hinted]);

        let views = dashboard_views(&[workspace], &mut git::Cache::default());

        let tui::WorkspaceItemView::AgentShell(item) = &views[0].items[0] else {
            panic!("expected hinted agent-shell item");
        };
        assert_eq!(item.shell.id, "s1");
        assert_eq!(item.shell.status, "running");
        assert_eq!(item.shell.name, "terminal");
        assert!(item.agent.is_none());
    }

    #[test]
    fn workspace_view_hints_exact_pi_foreground_process_before_first_prompt() {
        let mut hinted = shell("s1", "w1", "terminal");
        hinted.foreground_process = Some("pi".into());
        let workspace = workspace("w1", "project", vec![hinted]);

        let views = dashboard_views(&[workspace], &mut git::Cache::default());

        let tui::WorkspaceItemView::AgentShell(item) = &views[0].items[0] else {
            panic!("expected hinted agent-shell item");
        };
        assert_eq!(item.shell.name, "terminal");
        assert!(item.agent.is_none());
    }

    #[test]
    fn workspace_view_does_not_hint_an_inactive_pi_session() {
        let mut hinted = shell("s1", "w1", "terminal");
        hinted.foreground_process = Some("pi".into());
        let mut workspace = workspace("w1", "project", vec![hinted]);
        let mut inactive = agent("inactive", "w1", "s1");
        inactive.name = "Pi".into();
        inactive.integration = "pi".into();
        inactive.observation.state = AgentState::Inactive;
        workspace.agents.push(inactive);

        let views = dashboard_views(&[workspace], &mut git::Cache::default());

        assert!(matches!(
            views[0].items[0],
            tui::WorkspaceItemView::Shell(_)
        ));
    }

    #[test]
    fn workspace_view_does_not_treat_arbitrary_foreground_process_as_agent() {
        let mut ordinary = shell("s1", "w1", "terminal");
        ordinary.foreground_process = Some("OpenCode".into());
        let workspace = workspace("w1", "project", vec![ordinary]);

        let views = dashboard_views(&[workspace], &mut git::Cache::default());

        assert!(matches!(
            views[0].items[0],
            tui::WorkspaceItemView::Shell(_)
        ));
    }

    #[test]
    fn workspace_view_prefers_lifecycle_agent_over_foreground_hint() {
        let mut hinted = shell("s1", "w1", "terminal");
        hinted.foreground_process = Some("opencode".into());
        let mut workspace = workspace("w1", "project", vec![hinted]);
        let mut durable = agent("a1", "w1", "s1");
        durable.name = "reviewer".into();
        durable.integration = "custom".into();
        durable.observation.state = AgentState::Blocked;
        durable.observation.evidence = "needs approval".into();
        workspace.agents.push(durable);

        let views = dashboard_views(&[workspace], &mut git::Cache::default());

        let tui::WorkspaceItemView::AgentShell(item) = &views[0].items[0] else {
            panic!("expected durable agent-shell item");
        };
        assert_eq!(item.shell.name, "terminal");
        let agent = item.agent.as_ref().expect("durable agent");
        assert_eq!(agent.state, tui::AgentDisplayState::Blocked);
        assert_eq!(agent.evidence, "needs approval");
    }

    #[test]
    fn workspace_view_preserves_live_four_shell_three_agent_shape() {
        let mut hinted = shell("s1", "w1", "hinted");
        hinted.foreground_process = Some("opencode".into());
        let mut workspace = workspace(
            "w1",
            "project",
            vec![
                hinted,
                shell("s2", "w1", "durable-1"),
                shell("s3", "w1", "durable-2"),
                shell("s4", "w1", "ordinary"),
            ],
        );
        workspace.agents = vec![agent("a2", "w1", "s2"), agent("a3", "w1", "s3")];

        let views = dashboard_views(&[workspace], &mut git::Cache::default());

        assert_eq!(views[0].items.len(), 4);
        assert_eq!(
            views[0]
                .items
                .iter()
                .filter(|item| matches!(item, tui::WorkspaceItemView::AgentShell(_)))
                .count(),
            3
        );
        assert_eq!(
            views[0]
                .items
                .iter()
                .filter(|item| matches!(item, tui::WorkspaceItemView::Shell(_)))
                .count(),
            1
        );
    }

    #[test]
    fn workspace_view_selects_latest_active_agent_with_stable_id_tie_break() {
        let mut workspace = workspace("w1", "project", vec![shell("s1", "w1", "terminal")]);
        let mut older = agent("older", "w1", "s1");
        older.observation.observed_at_ms = 20;
        let mut tied_first = agent("agent-a", "w1", "s1");
        tied_first.observation.observed_at_ms = 30;
        tied_first.name = "first".into();
        tied_first.integration = "other".into();
        let mut tied_last = agent("agent-z", "w1", "s1");
        tied_last.observation.observed_at_ms = 30;
        tied_last.observation.evidence = "latest evidence".into();
        tied_last.name = "latest".into();
        tied_last.integration = "other".into();
        workspace.agents = vec![tied_last, older, tied_first];

        let views = dashboard_views(&[workspace], &mut git::Cache::default());

        let tui::WorkspaceItemView::AgentShell(item) = &views[0].items[0] else {
            panic!("expected agent-shell item");
        };
        assert_eq!(item.agent.as_ref().unwrap().evidence, "latest evidence");
    }

    #[test]
    fn workspace_view_ignores_completed_stale_wrong_run_and_orphan_agents() {
        let mut second_shell = shell("s2", "w1", "second");
        second_shell.run.as_mut().unwrap().id = "r2".into();
        let mut workspace = workspace(
            "w1",
            "project",
            vec![shell("s1", "w1", "first"), second_shell],
        );
        let mut ended = agent("ended", "w1", "s1");
        ended.ended_at_ms = Some(12);
        let mut done = agent("done", "w1", "s1");
        done.observation.state = AgentState::Done;
        let mut inactive = agent("inactive", "w1", "s1");
        inactive.observation.state = AgentState::Inactive;
        let mut stale = agent("stale", "w1", "s1");
        stale.run_id = "old-run".into();
        let wrong_pair = agent("wrong-pair", "w1", "s2");
        let orphan = agent("orphan", "w1", "missing");
        let mut wrong_workspace = agent("wrong-workspace", "w2", "s1");
        wrong_workspace.run_id = "r1".into();
        workspace.agents = vec![
            ended,
            done,
            inactive,
            stale,
            wrong_pair,
            orphan,
            wrong_workspace,
        ];

        let views = dashboard_views(&[workspace], &mut git::Cache::default());

        assert_eq!(views[0].items.len(), 2);
        assert!(
            views[0]
                .items
                .iter()
                .all(|item| matches!(item, tui::WorkspaceItemView::Shell(_)))
        );
    }

    #[test]
    fn workspace_view_does_not_present_an_exited_shell_as_an_active_agent() {
        let mut exited = shell("s1", "w1", "agent");
        exited.status = ShellStatus::Exited { code: Some(1) };
        let mut workspace = workspace("w1", "project", vec![exited]);
        workspace.agents.push(agent("a1", "w1", "s1"));

        let views = dashboard_views(&[workspace], &mut git::Cache::default());

        assert!(matches!(
            views[0].items[0],
            tui::WorkspaceItemView::Shell(_)
        ));
    }

    #[test]
    fn dashboard_open_dispatches_only_the_selected_item_type() {
        let mut shell_calls = 0;
        let mut launcher_calls = 0;
        let result = dispatch_dashboard_open(
            &tui::OpenTarget::Launcher {
                workspace_id: "w1".into(),
                launcher_id: "l1".into(),
            },
            "",
            |_| {
                shell_calls += 1;
                Ok("shell".into())
            },
            |workspace_id, launcher_id| {
                launcher_calls += 1;
                assert_eq!(workspace_id, "w1");
                assert_eq!(launcher_id, "l1");
                Ok("launcher".into())
            },
        )
        .unwrap();

        assert_eq!(result, "launcher");
        assert_eq!(shell_calls, 0);
        assert_eq!(launcher_calls, 1);
    }

    #[test]
    fn dashboard_run_now_backend_generates_a_fresh_dispatch_key() {
        let directory = env::temp_dir().join(format!("boomux-dashboard-run-{}", Uuid::new_v4()));
        fs::create_dir_all(&directory).unwrap();
        let socket = directory.join("daemon.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let server = thread::spawn(move || {
            let mut keys = Vec::new();
            for index in 0..2 {
                let (mut stream, _) = listener.accept().unwrap();
                let request: protocol::Envelope<protocol::Request> =
                    protocol::read_message(&mut stream).unwrap();
                let protocol::Request::RunAgentSchedule {
                    schedule_id,
                    dispatch_key,
                } = request.message
                else {
                    panic!("expected run schedule request");
                };
                assert_eq!(schedule_id, "schedule-1");
                Uuid::parse_str(&dispatch_key).unwrap();
                keys.push(dispatch_key.clone());
                let mut execution =
                    scheduled_execution(&format!("execution-{index}"), "schedule-1");
                execution.dispatch_key = dispatch_key;
                protocol::write_message(
                    &mut stream,
                    &protocol::Envelope::with_version(
                        request.version,
                        protocol::Response::ScheduledExecution {
                            execution,
                            next_occurrence: None,
                        },
                    ),
                )
                .unwrap();
            }
            keys
        });
        let client = client::Client::from_socket_path(socket);

        assert!(
            run_dashboard_schedule(&client, "schedule-1")
                .unwrap()
                .contains("execution-0")
        );
        assert!(
            run_dashboard_schedule(&client, "schedule-1")
                .unwrap()
                .contains("execution-1")
        );
        let keys = server.join().unwrap();
        assert_ne!(keys[0], keys[1]);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn dashboard_cancel_backend_revalidates_exact_execution_state() {
        let directory = env::temp_dir().join(format!("boomux-dashboard-cancel-{}", Uuid::new_v4()));
        fs::create_dir_all(&directory).unwrap();
        let socket = directory.join("daemon.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let request: protocol::Envelope<protocol::Request> =
                protocol::read_message(&mut stream).unwrap();
            assert!(matches!(
                request.message,
                protocol::Request::GetScheduledExecution { ref execution_id }
                    if execution_id == "execution-1"
            ));
            let mut execution = scheduled_execution("execution-1", "schedule-1");
            execution.state = protocol::ScheduledExecutionState::Exited;
            execution.ended_at_ms = Some(22);
            execution.outcome = Some(ScheduledExecutionOutcome::ExitCode { code: 0 });
            protocol::write_message(
                &mut stream,
                &protocol::Envelope::with_version(
                    request.version,
                    protocol::Response::ScheduledExecution {
                        execution,
                        next_occurrence: None,
                    },
                ),
            )
            .unwrap();
        });
        let client = client::Client::from_socket_path(socket);

        let error = cancel_dashboard_execution(&client, "execution-1").unwrap_err();
        assert!(error.contains("no longer active"));
        server.join().unwrap();
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn restoring_empty_workspace_returns_actionable_error() {
        let error = open_workspace(&workspace("w1", "empty", Vec::new()), None).unwrap_err();

        assert!(error.to_string().contains("workspace empty has no shells"));
    }

    #[test]
    fn workspace_restore_excludes_schedule_owned_shells_from_openability_and_counts() {
        let mut scheduled = shell("schedule-shell", "w1", "scheduled");
        scheduled.owner = protocol::ShellOwner::Schedule {
            schedule_id: "schedule-1".into(),
        };
        let workspace = workspace("w1", "scheduled-only", vec![scheduled]);
        assert_eq!(workspace_user_shell_count(&workspace), 0);
        let error = open_workspace(&workspace, None).unwrap_err();
        assert!(error.to_string().contains("has no shells or launchers"));
    }

    #[test]
    fn dashboard_session_resume_plan_uses_exact_host_identity_and_rejects_current_work() {
        let mut session = session_projection::SessionProjection {
            id: "session-1".into(),
            workspace_id: "w1".into(),
            workspace_name: "project".into(),
            integration: "opencode".into(),
            external_session_id: Some("ses_exact".into()),
            description: "review".into(),
            state: AgentState::Inactive,
            state_is_current: false,
            started_at_ms: 1,
            last_at_ms: 2,
            source_cwd: Some(env::temp_dir()),
            occurrences: Vec::new(),
        };

        let (_, command) = dashboard_session_resume_plan(&session).unwrap();
        assert_eq!(command, ["opencode", "--session", "ses_exact"]);

        session.state_is_current = true;
        assert!(
            dashboard_session_resume_plan(&session)
                .unwrap_err()
                .contains("already active")
        );
    }

    #[test]
    fn exact_execution_open_rejects_a_reused_schedule_shell_run() {
        let mut execution = scheduled_execution("execution-1", "schedule-1");
        execution.shell_id = Some("schedule-shell".into());
        execution.run_id = Some("execution-run".into());
        let mut shell = shell("schedule-shell", "w1", "scheduled");
        shell.owner = protocol::ShellOwner::Schedule {
            schedule_id: "schedule-1".into(),
        };
        shell.run.as_mut().unwrap().id = "execution-run".into();
        assert!(
            validate_dashboard_execution_open(
                &execution,
                &shell,
                "schedule-shell",
                "execution-run"
            )
            .is_ok()
        );

        shell.run.as_mut().unwrap().id = "later-run".into();
        let error = validate_dashboard_execution_open(
            &execution,
            &shell,
            "schedule-shell",
            "execution-run",
        )
        .unwrap_err();
        assert!(error.contains("different run"));
    }

    #[test]
    fn execution_open_target_uses_exact_run_or_linked_agent_identity() {
        let mut execution = scheduled_execution("execution-1", "schedule-1");
        execution.shell_id = Some("schedule-shell".into());
        execution.run_id = Some("schedule-run".into());
        assert!(matches!(
            scheduled_execution_open_target(&execution),
            Ok(ScheduledExecutionOpenTarget::Run {
                shell_id: "schedule-shell",
                run_id: "schedule-run"
            })
        ));

        execution.run_id = None;
        assert_eq!(
            scheduled_execution_open_target(&execution).unwrap_err().0,
            "busy"
        );

        execution.state = protocol::ScheduledExecutionState::Exited;
        execution.agent_id = Some("agent-1".into());
        assert!(matches!(
            scheduled_execution_open_target(&execution),
            Ok(ScheduledExecutionOpenTarget::Session {
                agent_id: "agent-1"
            })
        ));

        execution.agent_id = None;
        assert_eq!(
            scheduled_execution_open_target(&execution).unwrap_err().0,
            "not_found"
        );
    }

    #[test]
    fn workspace_open_continues_after_a_launcher_spawn_failure() {
        let marker = env::temp_dir().join(format!("boomux-launcher-{}", Uuid::new_v4()));
        let mut workspace = workspace("w1", "launchers", Vec::new());
        workspace.launchers = vec![
            WorkspaceLauncherSnapshot {
                id: "l1".into(),
                revision: 1,
                workspace_id: "w1".into(),
                name: "missing".into(),
                cwd: env::temp_dir(),
                command: vec!["/boomux-command-does-not-exist".into()],
            },
            WorkspaceLauncherSnapshot {
                id: "l2".into(),
                revision: 1,
                workspace_id: "w1".into(),
                name: "later".into(),
                cwd: env::temp_dir(),
                command: vec![
                    "/bin/sh".into(),
                    "-c".into(),
                    "printf launched > \"$1\"".into(),
                    "launcher".into(),
                    marker.display().to_string(),
                ],
            },
        ];

        let error = open_workspace(&workspace, None).unwrap_err();
        assert!(error.to_string().contains("launcher missing"));
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        let contents = loop {
            if let Ok(contents) = fs::read_to_string(&marker)
                && contents == "launched"
            {
                break contents;
            }
            if std::time::Instant::now() >= deadline {
                break String::new();
            }
            thread::sleep(std::time::Duration::from_millis(10));
        };
        assert_eq!(contents, "launched");
        if marker.is_file() {
            fs::remove_file(marker).unwrap();
        }
    }

    #[test]
    fn selects_recent_rendered_lines() {
        assert_eq!(recent_lines("one\ntwo\nthree\n", 2), "two\nthree\n");
        assert_eq!(recent_lines("one\ntwo", 1), "two");
    }

    #[test]
    fn formats_shell_run_exit_codes() {
        assert_eq!(
            shell_exit_reason(&boomux::protocol::ShellRunExitReason::Exited { code: Some(7) }),
            "exited (7)"
        );
        assert_eq!(
            shell_exit_reason(&boomux::protocol::ShellRunExitReason::Exited { code: None }),
            "exited (code unavailable)"
        );
    }

    #[test]
    fn installs_skill_under_vendor_neutral_directory() {
        assert_eq!(
            skill_install_path(Path::new("/home/example")),
            PathBuf::from("/home/example/.agents/skills/boomux/SKILL.md")
        );
        assert_eq!(
            legacy_skill_install_path(Path::new("/home/example")),
            PathBuf::from("/home/example/.agents/skills/boomux-shells/SKILL.md")
        );
    }

    #[test]
    fn parses_opencode_install_and_uses_global_plugin_path() {
        assert!(matches!(
            Cli::try_parse_from(["boomux", "opencode", "install", "--force"])
                .unwrap()
                .command,
            Some(Commands::Opencode {
                command: OpenCodeCommands::Install { force: true }
            })
        ));
        assert_eq!(
            opencode_install_path(Path::new("/config")),
            PathBuf::from("/config/opencode/plugins/boomux.js")
        );
        assert_eq!(
            opencode_config_root(Some("/xdg".into()), Some("/home/example".into())).unwrap(),
            PathBuf::from("/xdg")
        );
        assert_eq!(
            opencode_config_root(None, Some("/home/example".into())).unwrap(),
            PathBuf::from("/home/example/.config")
        );
        assert!(opencode_config_root(Some("relative".into()), None).is_err());
        assert!(install_opencode_at(Path::new("relative"), false).is_err());
    }

    #[test]
    fn parses_unified_integration_management_commands() {
        let list = Cli::try_parse_from(["boomux", "integration", "list"]).unwrap();
        assert!(matches!(
            list.command,
            Some(Commands::Integration {
                command: IntegrationCommands::List
            })
        ));
        assert_eq!(list.command_descriptor().key, "integration.list");

        let status = Cli::try_parse_from(["boomux", "integration", "status", "pi"]).unwrap();
        assert!(matches!(
            status.command,
            Some(Commands::Integration {
                command: IntegrationCommands::Status {
                    integration: Some(integration_management::IntegrationId::Pi),
                    ..
                }
            })
        ));
        assert_eq!(status.command_descriptor().key, "integration.status");

        let install =
            Cli::try_parse_from(["boomux", "integration", "install", "opencode", "--force"])
                .unwrap();
        assert!(matches!(
            install.command,
            Some(Commands::Integration {
                command: IntegrationCommands::Install {
                    integration: Some(integration_management::IntegrationId::Opencode),
                    all: false,
                    force: true,
                    dry_run: false,
                    ..
                }
            })
        ));
        assert_eq!(install.command_descriptor().key, "integration.install");

        let preview = Cli::try_parse_from([
            "boomux",
            "integration",
            "install",
            "pi",
            "--dry-run",
            "--json",
        ])
        .unwrap();
        assert!(matches!(
            preview.command,
            Some(Commands::Integration {
                command: IntegrationCommands::Install {
                    integration: Some(integration_management::IntegrationId::Pi),
                    all: false,
                    force: false,
                    dry_run: true,
                    ..
                }
            })
        ));
        assert_eq!(preview.command_descriptor().output, OutputMode::Json);

        let uninstall = Cli::try_parse_from([
            "boomux",
            "integration",
            "uninstall",
            "opencode",
            "--force",
            "--json",
        ])
        .unwrap();
        assert!(matches!(
            uninstall.command,
            Some(Commands::Integration {
                command: IntegrationCommands::Uninstall {
                    integration: Some(integration_management::IntegrationId::Opencode),
                    all: false,
                    force: true,
                    ..
                }
            })
        ));
        assert_eq!(uninstall.command_descriptor().key, "integration.uninstall");
        assert_eq!(uninstall.command_descriptor().output, OutputMode::Json);

        let setup =
            Cli::try_parse_from(["boomux", "integration", "setup", "pi", "--yes", "--force"])
                .unwrap();
        assert!(matches!(
            setup.command,
            Some(Commands::Integration {
                command: IntegrationCommands::Setup {
                    integration: integration_management::IntegrationId::Pi,
                    yes: true,
                    force: true,
                    ..
                }
            })
        ));
        assert_eq!(setup.command_descriptor().key, "integration.setup");
        assert_eq!(setup.command_descriptor().output, OutputMode::HumanOnly);

        assert!(Cli::try_parse_from(["boomux", "integration", "install", "--all"]).is_ok());
        assert!(Cli::try_parse_from(["boomux", "integration", "install"]).is_err());
        assert!(Cli::try_parse_from(["boomux", "integration", "install", "pi", "--all",]).is_err());

        let verify = Cli::try_parse_from([
            "boomux",
            "integration",
            "verify",
            "opencode",
            "--shell",
            "s1",
            "--wait-ms",
            "0",
            "--json",
        ])
        .unwrap();
        assert!(matches!(
            verify.command,
            Some(Commands::Integration {
                command: IntegrationCommands::Verify {
                    integration: integration_management::IntegrationId::Opencode,
                    shell: Some(ref shell),
                    wait_ms: 0,
                    ..
                }
            }) if shell == "s1"
        ));
        assert_eq!(verify.command_descriptor().key, "integration.verify");
        assert_eq!(verify.command_descriptor().output, OutputMode::Json);
    }

    #[test]
    fn ambiguous_verification_lists_named_shell_commands() {
        let snapshot = Snapshot {
            workspaces: vec![
                workspace("w2", "Zeta", vec![shell("s2", "w2", "backend")]),
                workspace("w1", "Alpha", vec![shell("s1", "w1", "frontend")]),
            ],
            focused_terminal: None,
            scheduler: None,
        };
        let targets = vec![
            integration_management::VerificationTarget {
                shell_id: "s2".into(),
                run_id: "r2".into(),
            },
            integration_management::VerificationTarget {
                shell_id: "s1".into(),
                run_id: "r1".into(),
            },
        ];

        assert_eq!(
            format_ambiguous_verification_targets(
                &snapshot,
                integration_management::IntegrationId::Opencode,
                &targets,
            ),
            concat!(
                "multiple running OpenCode host shells found\n\n",
                "Choose a shell:\n",
                "  Alpha / frontend\n",
                "    boomux integration verify opencode --shell s1\n",
                "  Zeta / backend\n",
                "    boomux integration verify opencode --shell s2",
            )
        );
    }

    #[test]
    fn setup_confirmation_requires_an_explicit_yes() {
        assert!(is_setup_confirmation("y\n"));
        assert!(is_setup_confirmation(" YES "));
        assert!(!is_setup_confirmation(""));
        assert!(!is_setup_confirmation("no"));
    }

    #[test]
    fn formats_integration_output_without_tab_alignment() {
        let integrations = integration_management::IntegrationId::all()
            .map(integration_management::IntegrationSummary::from)
            .collect::<Vec<_>>();
        assert_eq!(
            format_integration_list(&integrations),
            "NAME      PACKAGE                          VALIDATED VERSION\n\
             opencode  opencode-ai                      1.18.15\n\
             pi        @earendil-works/pi-coding-agent  0.84.1\n"
        );

        let status = integration_management::IntegrationStatus {
            name: "opencode",
            display_name: "OpenCode",
            package: "opencode-ai",
            validated_version: "1.18.15",
            host: integration_management::HostStatus {
                state: integration_management::HostState::Available,
                executable: Some("/usr/bin/opencode".into()),
                version: Some("1.18.15".into()),
                compatibility: "validated",
                error: None,
            },
            asset: integration_management::AssetStatus {
                state: integration_management::AssetState::Current,
                path: Some("/home/test/.config/opencode/plugins/boomux.js".into()),
                error: None,
            },
            runtime: integration_management::RuntimeStatus {
                state: integration_management::RuntimeState::Untracked,
                running_processes: 3,
                tracked_processes: 2,
                untracked_processes: 1,
            },
            recommended_action: integration_management::RecommendedAction::RestartHost,
        };
        let output = format_integration_statuses(&[status]);
        assert!(output.contains("  Host          available\n"));
        assert!(
            output.contains("  Runtime       untracked (3 running, 2 reporting, 1 untracked)\n")
        );
        assert!(output.contains("  Action        restart host\n"));
    }

    #[test]
    fn opencode_install_is_idempotent_and_requires_force_for_changes() {
        let config = test_skill_home("opencode-content");
        let plugin = opencode_install_path(&config);
        let herdr = config.join("opencode/plugins/herdr.js");
        let opencode_json = config.join("opencode/opencode.json");
        fs::create_dir_all(herdr.parent().unwrap()).unwrap();
        fs::write(&herdr, "herdr plugin").unwrap();
        fs::write(&opencode_json, "{\"plugin\":[\"herdr\"]}").unwrap();

        install_opencode_at(&config, false).unwrap();
        assert_eq!(fs::read_to_string(&plugin).unwrap(), BOOMUX_OPENCODE_PLUGIN);
        assert_eq!(fs::read_to_string(&herdr).unwrap(), "herdr plugin");
        assert_eq!(
            fs::read_to_string(&opencode_json).unwrap(),
            "{\"plugin\":[\"herdr\"]}"
        );
        install_opencode_at(&config, false).unwrap();
        fs::write(&plugin, "custom plugin").unwrap();
        assert!(install_opencode_at(&config, false).is_err());
        assert_eq!(fs::read_to_string(&plugin).unwrap(), "custom plugin");
        install_opencode_at(&config, true).unwrap();
        assert_eq!(fs::read_to_string(&plugin).unwrap(), BOOMUX_OPENCODE_PLUGIN);

        fs::remove_dir_all(config).unwrap();
    }

    #[test]
    fn parses_pi_install_and_uses_global_extension_path() {
        assert!(matches!(
            Cli::try_parse_from(["boomux", "pi", "install", "--force"])
                .unwrap()
                .command,
            Some(Commands::Pi {
                command: PiCommands::Install { force: true }
            })
        ));
        assert_eq!(
            pi_install_path(Path::new("/pi-agent")),
            PathBuf::from("/pi-agent/extensions/boomux.js")
        );
        assert_eq!(
            pi_config_root(Some("/custom/pi".into()), Some("/home/example".into())).unwrap(),
            PathBuf::from("/custom/pi")
        );
        assert_eq!(
            pi_config_root(None, Some("/home/example".into())).unwrap(),
            PathBuf::from("/home/example/.pi/agent")
        );
        assert_eq!(
            pi_config_root(Some("".into()), Some("/home/example".into())).unwrap(),
            PathBuf::from("/home/example/.pi/agent")
        );
        assert_eq!(
            pi_config_root(Some("~/.config/pi".into()), Some("/home/example".into())).unwrap(),
            PathBuf::from("/home/example/.config/pi")
        );
        assert!(pi_config_root(Some("relative".into()), None).is_err());
        assert!(install_pi_at(Path::new("relative"), false).is_err());
    }

    #[test]
    fn pi_install_is_idempotent_and_requires_force_for_changes() {
        let config = test_skill_home("pi-content");
        let extension = pi_install_path(&config);

        install_pi_at(&config, false).unwrap();
        assert_eq!(fs::read_to_string(&extension).unwrap(), BOOMUX_PI_EXTENSION);
        install_pi_at(&config, false).unwrap();
        fs::write(&extension, "custom extension").unwrap();
        assert!(install_pi_at(&config, false).is_err());
        assert_eq!(fs::read_to_string(&extension).unwrap(), "custom extension");
        install_pi_at(&config, true).unwrap();
        assert_eq!(fs::read_to_string(&extension).unwrap(), BOOMUX_PI_EXTENSION);

        fs::remove_dir_all(config).unwrap();
    }

    #[test]
    fn pi_install_rejects_symlinked_extension_directory() {
        use std::os::unix::fs::symlink;

        let config = test_skill_home("pi-symlink-directory");
        let outside = test_skill_home("pi-symlink-directory-target");
        fs::create_dir_all(&config).unwrap();
        fs::create_dir_all(&outside).unwrap();
        symlink(&outside, config.join("extensions")).unwrap();

        assert!(install_pi_at(&config, true).is_err());
        assert!(fs::read_dir(&outside).unwrap().next().is_none());

        fs::remove_dir_all(config).unwrap();
        fs::remove_dir_all(outside).unwrap();
    }

    #[test]
    fn schedule_parser_enforces_sources_and_descriptors() {
        let valid = [
            "boomux",
            "schedule",
            "create",
            "review",
            "--workspace",
            "project",
            "--cwd",
            "/tmp",
            "--integration",
            "opencode",
            "--prompt",
            "review exactly\n",
            "--weekdays",
            "09:30",
            "--continue",
            "projected-session",
            "--enabled",
            "--json",
        ];
        let cli = Cli::try_parse_from(valid).unwrap();
        assert_eq!(cli.command_descriptor().key, "schedule.create");
        assert!(matches!(
            cli.command,
            Some(Commands::Schedule {
                command: ScheduleCommands::Create(arguments)
            }) if arguments.prompt.as_deref() == Some("review exactly\n")
                && arguments.continue_session.as_deref() == Some("projected-session")
                && arguments.enabled
        ));

        for arguments in [
            vec!["boomux", "schedule", "list", "--json"],
            vec!["boomux", "schedule", "inspect", "id", "--json"],
            vec!["boomux", "schedule", "pause", "id", "--json"],
            vec!["boomux", "schedule", "resume", "id", "--json"],
            vec!["boomux", "schedule", "remove", "id", "--json"],
            vec!["boomux", "execution", "open", "execution-id", "--json"],
        ] {
            assert_eq!(
                Cli::try_parse_from(arguments)
                    .unwrap()
                    .command_descriptor()
                    .output,
                OutputMode::Json
            );
        }

        let required = [
            "boomux",
            "schedule",
            "create",
            "review",
            "--workspace",
            "project",
            "--cwd",
            "/tmp",
            "--integration",
            "opencode",
        ];
        assert!(Cli::try_parse_from(required.into_iter().chain(["--daily", "09:00"])).is_err());
        assert!(
            Cli::try_parse_from(required.into_iter().chain([
                "--prompt",
                "one",
                "--prompt-file",
                "/tmp/prompt",
                "--daily",
                "09:00",
            ]))
            .is_err()
        );
        assert!(
            Cli::try_parse_from(required.into_iter().chain([
                "--prompt",
                "one",
                "--daily",
                "09:00",
                "--cron",
                "0 9 * * *",
            ]))
            .is_err()
        );
        assert!(
            Cli::try_parse_from(required.into_iter().chain([
                "--prompt",
                "one",
                "--daily",
                "09:00",
                "--fresh",
                "--continue",
                "session",
            ]))
            .is_err()
        );
        assert!(
            Cli::try_parse_from(required.into_iter().chain([
                "--prompt",
                "one",
                "--daily",
                "09:00",
                "--paused",
                "--enabled",
            ]))
            .is_err()
        );
    }

    #[test]
    fn schedule_helpers_preserve_prompt_files_and_resolve_targets_safely() {
        use std::os::unix::fs::symlink;
        use std::os::unix::net::UnixListener;

        assert!(validate_schedule_protocol(21).is_err());
        assert!(validate_schedule_protocol(22).is_ok());
        assert_eq!(
            schedule_cron(None, Some("15m"), None, None, None).unwrap(),
            "*/15 * * * *"
        );
        assert_eq!(
            schedule_cron(None, None, None, Some("17:30"), None).unwrap(),
            "30 17 * * 1-5"
        );
        assert_eq!(
            schedule_cron(None, None, None, None, Some("mon@08:00")).unwrap(),
            "0 8 * * 1"
        );
        assert!(schedule_cron(None, None, Some("9:00"), None, None).is_err());

        let directory = test_skill_home("schedule-prompt");
        fs::create_dir_all(&directory).unwrap();
        let path = directory.join("prompt.txt");
        fs::write(&path, b"private instructions\n").unwrap();
        assert_eq!(
            schedule_prompt(None, Some(&path)).unwrap(),
            "private instructions\n"
        );
        assert!(schedule_prompt(None, Some(&directory)).is_err());
        let link = directory.join("prompt-link");
        symlink(&path, &link).unwrap();
        let error = schedule_prompt(None, Some(&link)).unwrap_err();
        assert_eq!(
            cli_output::classify_for_test("schedule.create", error.as_ref()),
            "invalid_argument"
        );
        let socket = directory.join("prompt.sock");
        let _listener = UnixListener::bind(&socket).unwrap();
        let error = schedule_prompt(None, Some(&socket)).unwrap_err();
        assert_eq!(
            cli_output::classify_for_test("schedule.create", error.as_ref()),
            "invalid_argument"
        );
        assert_eq!(
            escape_terminal_text("line one\nsecret\u{1b}]52;c;payload\u{7}"),
            "line one\\nsecret\\u{1b}]52;c;payload\\u{7}"
        );
        fs::write(&path, vec![b'x'; boomux::scheduling::MAX_PROMPT_BYTES + 1]).unwrap();
        let error = schedule_prompt(None, Some(&path)).unwrap_err().to_string();
        assert!(!error.contains(&"x".repeat(100)));
        fs::remove_dir_all(directory).unwrap();

        let mut first = workspace("w1", "first", Vec::new());
        first.schedules.push(schedule("schedule-1", "w1", "review"));
        let mut second = workspace("w2", "second", Vec::new());
        second
            .schedules
            .push(schedule("schedule-2", "w2", "review"));
        let snapshot = Snapshot {
            workspaces: vec![first, second],
            focused_terminal: None,
            scheduler: None,
        };
        assert_eq!(
            resolve_cli_schedule(&snapshot, "schedule-2", None)
                .unwrap()
                .workspace_id,
            "w2"
        );
        assert_eq!(
            resolve_cli_schedule(&snapshot, "review", Some("first"))
                .unwrap()
                .id,
            "schedule-1"
        );
    }

    #[test]
    fn capabilities_advertise_phase_two_agent_integration_surface() {
        let json_commands = json_commands().collect::<Vec<_>>();
        assert!(json_commands.contains(&"shell.suggest-name"));
        for command in [
            "agent.register",
            "agent.ensure",
            "agent.report",
            "agent.wait",
            "attention.list",
            "attention.acknowledge",
        ] {
            assert!(json_commands.contains(&command));
        }
        for command in ["session.list", "session.inspect"] {
            assert!(json_commands.contains(&command));
        }
        for command in [
            "integration.list",
            "integration.status",
            "integration.install",
            "integration.uninstall",
            "integration.verify",
        ] {
            assert!(json_commands.contains(&command));
        }
        for command in [
            "schedule.create",
            "schedule.list",
            "schedule.inspect",
            "schedule.pause",
            "schedule.resume",
            "schedule.remove",
        ] {
            assert!(json_commands.contains(&command));
        }
        for feature in [
            "protocol_10",
            "protocol_12",
            "inactive_agent_state",
            "idempotent_agent_ensure",
            "agent_authority_precedence",
            "opencode_lifecycle_plugin",
            "pi_lifecycle_extension",
            "protocol_15",
            "protocol_16",
            "persistent_agent_attention",
            "desktop_notifications",
            "sound_notifications",
            "integration_management",
            "protocol_31",
            "node_registration_management",
            "pinned_node_identity",
        ] {
            assert!(
                NON_PROTOCOL_FEATURES.contains(&feature)
                    || protocol::protocol_capabilities().any(|current| current == feature)
            );
        }
        assert_eq!(
            integration_management::IntegrationId::Opencode
                .installation()
                .validated_version,
            "1.18.15"
        );
        assert_eq!(
            integration_management::IntegrationId::Pi
                .installation()
                .validated_version,
            "0.84.1"
        );
        assert_eq!(protocol::PROTOCOL_VERSION, 37);
    }

    #[test]
    fn command_descriptors_have_unique_names_and_drive_json_capabilities() {
        let mut names = CommandKey::ALL
            .iter()
            .map(|key| key.descriptor().key)
            .collect::<Vec<_>>();
        let command_count = names.len();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), command_count);

        assert_eq!(
            json_commands().collect::<Vec<_>>(),
            [
                "capabilities",
                "list",
                "shells",
                "read",
                "events",
                "project.list",
                "workspace.list",
                "workspace.inspect",
                "node.add",
                "node.list",
                "node.inspect",
                "node.snapshot",
                "node.rename",
                "node.retarget",
                "node.forget",
                "shell.suggest-name",
                "shell.inspect",
                "launcher.list",
                "launcher.inspect",
                "agent.list",
                "agent.inspect",
                "agent.register",
                "agent.ensure",
                "agent.report",
                "agent.wait",
                "attention.list",
                "attention.acknowledge",
                "integration.list",
                "integration.status",
                "integration.install",
                "integration.uninstall",
                "integration.verify",
                "session.list",
                "session.inspect",
                "schedule.create",
                "schedule.list",
                "schedule.inspect",
                "schedule.pause",
                "schedule.resume",
                "schedule.remove",
                "schedule.run",
                "execution.list",
                "execution.inspect",
                "execution.wait",
                "execution.open",
                "execution.cancel",
                "daemon.status",
            ]
        );
    }

    #[test]
    fn doctor_reports_version_and_platform() {
        assert_eq!(
            doctor_version_line("1.2.3", "x86_64", "linux"),
            "ok  boomux: 1.2.3 (x86_64-linux)"
        );
    }

    #[test]
    fn notification_doctor_diagnostic_is_deterministic() {
        let disabled = daemon::NotificationDeliverySettings::default();
        assert_eq!(
            notification_diagnostic(&disabled, false, false),
            NotificationDiagnostic::Disabled
        );
        let enabled = daemon::NotificationDeliverySettings {
            desktop: daemon::NotificationSettings {
                enabled: true,
                ..Default::default()
            },
            ..disabled
        };
        assert_eq!(
            notification_diagnostic(&enabled, false, true),
            NotificationDiagnostic::MissingExecutable
        );
        assert_eq!(
            notification_diagnostic(&enabled, true, false),
            NotificationDiagnostic::MissingDesktopBus
        );
        assert_eq!(
            notification_diagnostic(&enabled, true, true),
            NotificationDiagnostic::Ready
        );

        assert_eq!(
            sound_notification_diagnostic(&enabled, false),
            SoundNotificationDiagnostic::Disabled
        );
        let sound_enabled = daemon::NotificationDeliverySettings {
            sound: daemon::NotificationSoundSettings {
                enabled: true,
                ..Default::default()
            },
            ..Default::default()
        };
        assert_eq!(
            sound_notification_diagnostic(&sound_enabled, false),
            SoundNotificationDiagnostic::MissingExecutable
        );
        assert_eq!(
            sound_notification_diagnostic(&sound_enabled, true),
            SoundNotificationDiagnostic::Ready
        );
    }

    #[test]
    fn opencode_install_rejects_symlinks_and_special_targets_even_with_force() {
        use std::os::unix::fs::symlink;

        let config = test_skill_home("opencode-symlink-directory");
        let outside = test_skill_home("opencode-symlink-directory-target");
        fs::create_dir_all(&config).unwrap();
        fs::create_dir_all(&outside).unwrap();
        symlink(&outside, config.join("opencode")).unwrap();
        assert!(install_opencode_at(&config, true).is_err());
        assert!(fs::read_dir(&outside).unwrap().next().is_none());
        fs::remove_dir_all(&config).unwrap();
        fs::remove_dir_all(&outside).unwrap();

        let config = test_skill_home("opencode-symlink-file");
        let outside = test_skill_home("opencode-symlink-file-target");
        let plugin = opencode_install_path(&config);
        fs::create_dir_all(plugin.parent().unwrap()).unwrap();
        fs::write(&outside, "do not replace").unwrap();
        symlink(&outside, &plugin).unwrap();
        assert!(install_opencode_at(&config, true).is_err());
        assert_eq!(fs::read_to_string(&outside).unwrap(), "do not replace");
        fs::remove_dir_all(&config).unwrap();
        fs::remove_file(&outside).unwrap();

        let config = test_skill_home("opencode-special-file");
        let plugin = opencode_install_path(&config);
        fs::create_dir_all(&plugin).unwrap();
        assert!(install_opencode_at(&config, true).is_err());
        assert!(plugin.is_dir());
        fs::remove_dir_all(config).unwrap();
    }

    #[test]
    fn bundled_opencode_plugin_has_lifecycle_contract_without_shell_interpolation() {
        for expected in [
            "session.status",
            "session.idle",
            "session.error",
            "session.deleted",
            "permission.asked",
            "question.asked",
            "BOOMUX_SHELL_ID",
            "BOOMUX_RUN_ID",
            "agent\",\n    \"ensure",
            "agent\",\n    \"report",
            "--json",
            "shell: false",
        ] {
            assert!(
                BOOMUX_OPENCODE_PLUGIN.contains(expected),
                "OpenCode plugin omits {expected}"
            );
        }
        for forbidden in ["Bun.$", "exec(", "sh -c", "${BOOMUX_"] {
            assert!(
                !BOOMUX_OPENCODE_PLUGIN.contains(forbidden),
                "OpenCode plugin contains shell interpolation marker {forbidden}"
            );
        }
    }

    #[test]
    fn bundled_pi_extension_has_lifecycle_contract_without_shell_interpolation() {
        for expected in [
            "session_start",
            "agent_start",
            "agent_end",
            "agent_settled",
            "session_shutdown",
            "stopReason",
            "getSessionId",
            "BOOMUX_SHELL_ID",
            "BOOMUX_RUN_ID",
            "agent\",\n    \"ensure",
            "agent\",\n    \"report",
            "--json",
            "shell: false",
        ] {
            assert!(
                BOOMUX_PI_EXTENSION.contains(expected),
                "Pi extension omits {expected}"
            );
        }
        for forbidden in ["exec(", "sh -c", "${BOOMUX_"] {
            assert!(
                !BOOMUX_PI_EXTENSION.contains(forbidden),
                "Pi extension contains shell interpolation marker {forbidden}"
            );
        }
    }

    #[test]
    fn bundled_skill_covers_every_public_command_group() {
        for command in [
            "boomux ui",
            "boomux doctor",
            "boomux capabilities",
            "boomux list",
            "boomux shells",
            "boomux read",
            "boomux events",
            "boomux close",
            "boomux open",
            "boomux project list",
            "boomux workspace list",
            "boomux workspace create",
            "boomux workspace open",
            "boomux workspace inspect",
            "boomux workspace rename",
            "boomux workspace close",
            "boomux shell create",
            "boomux shell inspect",
            "boomux shell rename",
            "boomux shell close",
            "boomux launcher list",
            "boomux launcher create",
            "boomux launcher inspect",
            "boomux launcher invoke",
            "boomux launcher rename",
            "boomux launcher remove",
            "boomux agent list",
            "boomux agent inspect",
            "boomux agent wait",
            "boomux agent register",
            "boomux agent ensure",
            "boomux agent report",
            "boomux agent supervise",
            "boomux attention list",
            "boomux attention acknowledge",
            "boomux notification test",
            "boomux session list",
            "boomux session inspect",
            "boomux integration list",
            "boomux integration status",
            "boomux integration install",
            "boomux integration uninstall",
            "boomux integration setup",
            "boomux integration verify",
            "boomux skill install",
            "boomux opencode install",
            "boomux pi install",
            "boomux daemon status",
            "boomux daemon restart",
            "boomux daemon stop",
            "boomux prompt",
        ] {
            let documented = BOOMUX_SKILL.lines().any(|line| {
                let line = line.trim_start();
                line.strip_prefix(command).is_some_and(|remaining| {
                    remaining.is_empty()
                        || remaining.chars().next().is_some_and(char::is_whitespace)
                })
            });
            assert!(documented, "skill omits command line for {command}");
        }
        assert!(BOOMUX_SKILL.contains("BOOMUX_SHELL_ID"));
        assert!(BOOMUX_SKILL.contains("--workspace"));
        assert!(BOOMUX_SKILL.contains("--terminal"));
    }

    #[test]
    fn skill_install_migrates_an_untouched_legacy_skill() {
        let home = test_skill_home("migrate");
        let legacy = legacy_skill_install_path(&home);
        fs::create_dir_all(legacy.parent().unwrap()).unwrap();
        fs::write(&legacy, LEGACY_BOOMUX_SHELLS_SKILL).unwrap();

        install_skill_at(&home, false).unwrap();

        assert_eq!(
            fs::read_to_string(skill_install_path(&home)).unwrap(),
            BOOMUX_SKILL
        );
        assert!(!legacy.exists());
        install_skill_at(&home, false).unwrap();
        assert_eq!(
            fs::read_to_string(skill_install_path(&home)).unwrap(),
            BOOMUX_SKILL
        );
        fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn skill_install_requires_force_and_preserves_customized_legacy_content() {
        let home = test_skill_home("customized");
        let skill = skill_install_path(&home);
        let legacy = legacy_skill_install_path(&home);
        fs::create_dir_all(skill.parent().unwrap()).unwrap();
        fs::create_dir_all(legacy.parent().unwrap()).unwrap();
        fs::write(&skill, "older consolidated skill").unwrap();
        fs::write(&legacy, "customized legacy skill").unwrap();

        assert!(install_skill_at(&home, false).is_err());
        assert_eq!(
            fs::read_to_string(&skill).unwrap(),
            "older consolidated skill"
        );
        assert_eq!(
            fs::read_to_string(&legacy).unwrap(),
            "customized legacy skill"
        );
        install_skill_at(&home, true).unwrap();

        assert_eq!(fs::read_to_string(skill).unwrap(), BOOMUX_SKILL);
        assert_eq!(
            fs::read_to_string(legacy).unwrap(),
            "customized legacy skill"
        );
        fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn skill_install_preserves_legacy_skill_with_additional_files() {
        let home = test_skill_home("legacy-resources");
        let legacy = legacy_skill_install_path(&home);
        let reference = legacy.parent().unwrap().join("REFERENCE.md");
        fs::create_dir_all(legacy.parent().unwrap()).unwrap();
        fs::write(&legacy, LEGACY_BOOMUX_SHELLS_SKILL).unwrap();
        fs::write(&reference, "custom reference").unwrap();

        install_skill_at(&home, false).unwrap();

        assert_eq!(
            fs::read_to_string(&legacy).unwrap(),
            LEGACY_BOOMUX_SHELLS_SKILL
        );
        assert_eq!(fs::read_to_string(reference).unwrap(), "custom reference");
        fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn skill_install_rejects_symlinked_directories_and_files() {
        use std::os::unix::fs::symlink;

        let home = test_skill_home("symlink-directory");
        let outside = test_skill_home("symlink-directory-target");
        fs::create_dir_all(&home).unwrap();
        fs::create_dir_all(&outside).unwrap();
        symlink(&outside, home.join(".agents")).unwrap();

        assert!(install_skill_at(&home, true).is_err());
        assert!(fs::read_dir(&outside).unwrap().next().is_none());
        fs::remove_dir_all(&home).unwrap();
        fs::remove_dir_all(&outside).unwrap();

        let home = test_skill_home("symlink-file");
        let outside = test_skill_home("symlink-file-target");
        let skill = skill_install_path(&home);
        fs::create_dir_all(skill.parent().unwrap()).unwrap();
        fs::write(&outside, "do not replace").unwrap();
        symlink(&outside, &skill).unwrap();

        assert!(install_skill_at(&home, true).is_err());
        assert_eq!(fs::read_to_string(&outside).unwrap(), "do not replace");
        fs::remove_dir_all(&home).unwrap();
        fs::remove_file(&outside).unwrap();
    }

    fn test_skill_home(label: &str) -> PathBuf {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        env::temp_dir().join(format!(
            "boomux-skill-{label}-{}-{unique}",
            std::process::id()
        ))
    }
}

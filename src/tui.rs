use std::io;
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crossterm::event::{
    self, DisableBracketedPaste, EnableBracketedPaste, Event, KeyCode, KeyEventKind, KeyModifiers,
};
use crossterm::execute;
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::symbols::Marker;
use ratatui::text::{Line, Span};
use ratatui::widgets::canvas::{Canvas, Circle, Line as CanvasLine};
use ratatui::widgets::{
    Block, Cell, Clear, List, ListItem, ListState, Paragraph, Row, Table, TableState, Wrap,
};
use unicode_width::UnicodeWidthStr;

use crate::agent_attention_projection::AgentStateCounts;
use crate::protocol::{
    NodeProjectionHealthCode, QualifiedIdentity, SchedulerHealth, TerminalColor, TerminalPreview,
    TerminalPreviewLine, TerminalStyle,
};

const BASE: Color = Color::Reset;
const OVERLAY: Color = Color::DarkGray;
const TEXT: Color = Color::Reset;
const SUBTEXT: Color = Color::DarkGray;
const TEAL: Color = Color::Cyan;
const BLUE: Color = Color::Blue;
const GREEN: Color = Color::Green;
const YELLOW: Color = Color::Yellow;
const RED: Color = Color::Red;
const UPDATE_CHECK_INTERVAL: Duration = Duration::from_millis(250);
const INTRO_POLL_INTERVAL: Duration = Duration::from_millis(16);
const FUSE_FRAME_DURATION: Duration = Duration::from_millis(40);
const EXPLOSION_FRAME_DURATION: Duration = Duration::from_millis(40);
const HOP_FRAME_COUNT: usize = 24;
const FUSE_FRAME_COUNT: usize = 51;
const EXPLOSION_FRAME_COUNT: usize = 60;
const FIREBALL_FRAME_COUNT: usize = 10;
const WORD_DISPERSE_START: usize = 48;
const BOOMUX_SMOKE: [&str; 5] = [
    "####. .###. .###. #...# #...# #...#",
    "#...# #...# #...# ##.## #...# .#.#.",
    "####. #...# #...# #.#.# #...# ..#..",
    "#...# #...# #...# #...# #...# .#.#.",
    "####. .###. .###. #...# .###. #...#",
];
const TERMINAL_PREVIEW_ROWS: usize = 16;
const TERMINAL_PREVIEW_SCROLL_STEP: usize = 12;
const PREVIEW_RESERVED_ITEM_HEIGHT: u16 = 6;
const AGENT_TABLE_HEADERS: [&str; 9] = [
    "STATUS",
    "UPDATED",
    "WORKSPACE",
    "NODE",
    "SHELL",
    "HARNESS",
    "TASK",
    "ROOT BRANCH",
    "ROOT WORKTREE",
];
const SHELL_TABLE_HEADERS: [&str; 9] = [
    "STATUS",
    "RUN",
    "WORKSPACE",
    "NODE",
    "SHELL",
    "KIND",
    "PROCESS",
    "BRANCH",
    "WORKTREE",
];
const ITEM_TABLE_HEADERS: [&str; 7] = [
    "KIND", "STATUS", "NAME", "NODE", "ACTIVITY", "BRANCH", "WORKTREE",
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum BombAnimationFrame {
    Fuse(usize),
    Explosion(usize),
    Finished,
}

fn bomb_animation_frame(elapsed: Duration) -> BombAnimationFrame {
    let fuse_duration = FUSE_FRAME_DURATION * FUSE_FRAME_COUNT as u32;
    if elapsed < fuse_duration {
        return BombAnimationFrame::Fuse(
            (elapsed.as_millis() / FUSE_FRAME_DURATION.as_millis()) as usize,
        );
    }

    let explosion_elapsed = elapsed - fuse_duration;
    let explosion = (explosion_elapsed.as_millis() / EXPLOSION_FRAME_DURATION.as_millis()) as usize;
    if explosion < EXPLOSION_FRAME_COUNT {
        BombAnimationFrame::Explosion(explosion)
    } else {
        BombAnimationFrame::Finished
    }
}

fn fuse_burn_progress(stage: usize) -> f64 {
    stage.saturating_sub(HOP_FRAME_COUNT).min(26) as f64 / 26.0
}

fn hop_height(progress: f64) -> f64 {
    if progress < 0.55 {
        (progress / 0.55 * std::f64::consts::PI).sin() * 7.0
    } else {
        ((progress - 0.55) / 0.45 * std::f64::consts::PI).sin() * 3.5
    }
}

fn smoke_word_lines(stage: usize) -> Vec<Line<'static>> {
    let disperse = stage
        .saturating_sub(WORD_DISPERSE_START)
        .min(EXPLOSION_FRAME_COUNT - WORD_DISPERSE_START) as f64
        / (EXPLOSION_FRAME_COUNT - WORD_DISPERSE_START) as f64;
    BOOMUX_SMOKE
        .iter()
        .enumerate()
        .map(|(row, line)| {
            let text = line
                .chars()
                .enumerate()
                .map(|(column, cell)| {
                    if cell != '#' {
                        return ' ';
                    }
                    let vanishes_at = 0.35 + ((row * 17 + column * 11) % 60) as f64 / 100.0;
                    if disperse > vanishes_at {
                        ' '
                    } else if (row + column + stage / 3).is_multiple_of(4) {
                        '▒'
                    } else {
                        '▓'
                    }
                })
                .collect::<String>();
            Line::styled(
                text,
                Style::new().fg(Color::Gray).add_modifier(Modifier::BOLD),
            )
        })
        .collect()
}

#[derive(Clone)]
pub(crate) struct NodeView {
    pub(crate) id: String,
    pub(crate) alias: String,
    pub(crate) local: bool,
    pub(crate) route: Option<String>,
    pub(crate) registration_revision: Option<u64>,
    pub(crate) health: NodeProjectionHealthCode,
    pub(crate) current: bool,
    pub(crate) stale: bool,
    pub(crate) observed_at_ms: u64,
    pub(crate) observed_protocol_version: Option<u32>,
    pub(crate) observed_helper_version: Option<String>,
    pub(crate) observed_capabilities: Vec<String>,
    pub(crate) workspace_owner_eligible: bool,
    pub(crate) workspace_owner_unavailable_reason: Option<String>,
    pub(crate) scheduler: SchedulerHealth,
}

#[derive(Clone)]
pub(crate) struct WorkspaceView {
    pub(crate) node: NodeView,
    pub(crate) id: String,
    pub(crate) name: String,
    pub(crate) default_cwd: Option<String>,
    pub(crate) items: Vec<WorkspaceItemView>,
    pub(crate) sessions: Vec<AgentSessionView>,
    pub(crate) agent_state_counts: AgentStateCounts,
    pub(crate) attention_count: usize,
    pub(crate) attention: Vec<WorkspaceAttentionView>,
    pub(crate) item_owners: Vec<WorkspaceItemOwnerView>,
    pub(crate) coordination: WorkspaceCoordinationView,
}

#[derive(Clone)]
pub(crate) struct WorkspaceItemOwnerView {
    pub(crate) node: NodeView,
    pub(crate) workspace_id: String,
}

#[derive(Clone)]
pub(crate) struct WorkspacePlacementView {
    pub(crate) node: NodeView,
    pub(crate) workspace_id: String,
    pub(crate) owner_revision: u64,
    pub(crate) default_cwd: Option<String>,
    pub(crate) state: crate::protocol::WorkspacePlacementState,
}

#[derive(Clone)]
pub(crate) enum WorkspaceCoordinationView {
    Global {
        revision: u64,
        closing: bool,
        placements: Vec<WorkspacePlacementView>,
    },
    External {
        owner_revision: u64,
        available: bool,
    },
}

pub(crate) struct DashboardState {
    pub(crate) nodes: Vec<NodeView>,
    pub(crate) workspaces: Vec<WorkspaceView>,
    pub(crate) schedules: Vec<ScheduleView>,
    pub(crate) scheduling: SchedulingView,
    pub(crate) exact_run_attachment: bool,
    pub(crate) schedule_editing: bool,
    pub(crate) cached_projection_dismissal: bool,
    pub(crate) focused_terminal: Option<FocusedTerminalView>,
    pub(crate) reset_focus_revision: bool,
}

pub(crate) struct ScheduleEditInspection {
    pub(crate) schedule_id: QualifiedIdentity,
    pub(crate) name: String,
    pub(crate) cron: String,
    pub(crate) timezone: String,
    pub(crate) prompt: String,
    pub(crate) revision: u64,
    pub(crate) paused: bool,
}

#[derive(Clone, PartialEq, Eq)]
pub(crate) struct ScheduleEditUpdate {
    pub(crate) name: String,
    pub(crate) cron: String,
    pub(crate) timezone: String,
    pub(crate) prompt: String,
}

impl std::fmt::Debug for ScheduleEditUpdate {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ScheduleEditUpdate")
            .field("name", &self.name)
            .field("cron", &self.cron)
            .field("timezone", &self.timezone)
            .field("prompt", &"<redacted>")
            .finish()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum SchedulingView {
    Unsupported {
        required_protocol: u32,
        negotiated: u32,
    },
    Active {
        active: u16,
        maximum: u16,
    },
    Offline {
        active: u16,
        maximum: u16,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ScheduleView {
    pub(crate) node_id: String,
    pub(crate) node_alias: String,
    pub(crate) actionable: bool,
    pub(crate) id: String,
    pub(crate) workspace_id: String,
    pub(crate) workspace: String,
    pub(crate) name: String,
    pub(crate) integration: String,
    pub(crate) state: ScheduleDisplayState,
    pub(crate) friendly_trigger: String,
    pub(crate) next_occurrence_ms: Option<u64>,
    pub(crate) executions: Vec<ExecutionView>,
    pub(crate) history_truncated: bool,
    pub(crate) possible_pruning_boundary: bool,
    pub(crate) history_scoped: bool,
    pub(crate) history_complete: bool,
}

impl ScheduleView {
    fn qualify(&self, inner_id: impl Into<String>) -> QualifiedIdentity {
        QualifiedIdentity::new(self.node_id.clone(), inner_id)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ScheduleDisplayState {
    Paused,
    Enabled,
}

impl ScheduleDisplayState {
    const fn label(self) -> &'static str {
        match self {
            Self::Paused => "paused",
            Self::Enabled => "enabled",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ExecutionView {
    pub(crate) id: String,
    pub(crate) state: ExecutionDisplayState,
    pub(crate) reason: Option<ExecutionReasonDisplay>,
    pub(crate) outcome: Option<ExecutionOutcomeDisplay>,
    pub(crate) requested_at_ms: u64,
    pub(crate) shell_id: Option<String>,
    pub(crate) run_id: Option<String>,
    pub(crate) agent_id: Option<String>,
    pub(crate) agent_state: Option<AgentDisplayState>,
    pub(crate) session_id: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ExecutionDisplayState {
    Skipped,
    Claimed,
    Starting,
    Active,
    DispatchFailed,
    Exited,
    Cancelled,
    Interrupted,
}

impl ExecutionDisplayState {
    const fn label(self) -> &'static str {
        match self {
            Self::Skipped => "skipped",
            Self::Claimed => "claimed",
            Self::Starting => "starting",
            Self::Active => "active",
            Self::DispatchFailed => "failed",
            Self::Exited => "exited",
            Self::Cancelled => "cancelled",
            Self::Interrupted => "interrupted",
        }
    }

    pub(crate) const fn is_active(self) -> bool {
        matches!(self, Self::Claimed | Self::Starting | Self::Active)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ExecutionReasonDisplay {
    Overlap,
    ActiveSession,
    WorkspaceCapacity,
    GlobalCapacity,
    Missed,
    PausedRace,
    InvalidTarget,
    RunnerStartFailed,
    HostSpawnFailed,
    CancelledByUser,
    ColdDaemonRecovery,
    RunnerExitedWithoutReport,
    DaemonShutdown,
}

impl ExecutionReasonDisplay {
    const fn label(self) -> &'static str {
        match self {
            Self::Overlap => "overlap",
            Self::ActiveSession => "active session",
            Self::WorkspaceCapacity => "workspace capacity",
            Self::GlobalCapacity => "global capacity",
            Self::Missed => "missed",
            Self::PausedRace => "paused race",
            Self::InvalidTarget => "invalid target",
            Self::RunnerStartFailed => "runner start failed",
            Self::HostSpawnFailed => "host spawn failed",
            Self::CancelledByUser => "cancelled by user",
            Self::ColdDaemonRecovery => "cold daemon recovery",
            Self::RunnerExitedWithoutReport => "runner exited without report",
            Self::DaemonShutdown => "daemon shutdown",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ExecutionOutcomeDisplay {
    ExitCode(i32),
    Signal(i32),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct FocusedTerminalView {
    pub(crate) revision: u64,
    pub(crate) node_id: Option<String>,
    pub(crate) workspace_id: String,
    pub(crate) shell_id: String,
}

#[derive(Clone)]
pub(crate) struct WorkspaceAttentionView {
    pub(crate) node_id: String,
    pub(crate) workspace_id: String,
    pub(crate) agent_id: String,
    pub(crate) shell_id: String,
    pub(crate) agent_name: String,
    pub(crate) reason: AttentionReason,
    pub(crate) evidence: String,
    pub(crate) observed_at_ms: u64,
}

#[derive(Clone)]
pub(crate) struct AgentSessionView {
    pub(crate) id: String,
    pub(crate) label: String,
    pub(crate) integration: String,
    pub(crate) external_session_id: Option<String>,
    pub(crate) state: AgentDisplayState,
    pub(crate) state_is_current: bool,
    pub(crate) last_at_ms: u64,
    pub(crate) source_cwd: Option<PathBuf>,
    pub(crate) runs: Vec<AgentSessionRunView>,
}

#[derive(Clone)]
pub(crate) struct AgentSessionRunView {
    pub(crate) agent_id: String,
    pub(crate) shell_name: Option<String>,
    pub(crate) directory: Option<PathBuf>,
}

#[derive(Clone)]
#[allow(clippy::large_enum_variant)]
pub(crate) enum WorkspaceItemView {
    Shell(TerminalView),
    AgentShell(AgentShellView),
    Launcher(LauncherView),
    Schedule(ScheduleItemView),
}

#[derive(Clone)]
pub(crate) struct ScheduleItemView {
    pub(crate) id: String,
    pub(crate) name: String,
    pub(crate) integration: String,
    pub(crate) state: ScheduleDisplayState,
    pub(crate) friendly_trigger: String,
}

#[derive(Clone)]
pub(crate) struct AgentShellView {
    pub(crate) shell: TerminalView,
    pub(crate) agent: Option<AgentView>,
    pub(crate) schedule_id: Option<String>,
}

impl AgentShellView {
    pub(crate) fn state(&self) -> AgentDisplayState {
        self.agent
            .as_ref()
            .map_or(AgentDisplayState::Untracked, |agent| agent.state)
    }
}

#[derive(Clone)]
pub(crate) struct AgentView {
    pub(crate) id: String,
    pub(crate) state: AgentDisplayState,
    pub(crate) integration: String,
    pub(crate) external_session_id: Option<String>,
    pub(crate) authority: AgentAuthorityDisplay,
    pub(crate) confidence: u8,
    pub(crate) evidence: String,
    pub(crate) updated_at_ms: u64,
    pub(crate) root_branch: String,
    pub(crate) root_worktree: String,
}

#[derive(Clone)]
pub(crate) struct LauncherView {
    pub(crate) id: String,
    pub(crate) name: String,
    pub(crate) directory: String,
    pub(crate) repository: String,
    pub(crate) branch: String,
    pub(crate) git_state: String,
    pub(crate) worktree: String,
    pub(crate) command: String,
    pub(crate) argv: Vec<String>,
}

#[derive(Clone)]
pub(crate) struct ProjectView {
    pub(crate) name: String,
    pub(crate) path: PathBuf,
    pub(crate) group: String,
    pub(crate) group_order: usize,
}

pub(crate) struct ProjectContext {
    pub(crate) projects: Vec<ProjectView>,
    pub(crate) config_path: Option<PathBuf>,
    pub(crate) warning: Option<String>,
    pub(crate) roots_configured: bool,
}

#[derive(Clone)]
pub(crate) struct TerminalView {
    pub(crate) id: String,
    pub(crate) name: String,
    pub(crate) status: String,
    pub(crate) directory: String,
    pub(crate) repository: String,
    pub(crate) branch: String,
    pub(crate) git_state: String,
    pub(crate) worktree: String,
    pub(crate) foreground_process: Option<String>,
    pub(crate) kind: TerminalKind,
    pub(crate) command: String,
    pub(crate) argv: Vec<String>,
    pub(crate) run: Option<TerminalRunView>,
}

#[derive(Clone)]
pub(crate) struct TerminalRunView {
    pub(crate) id: String,
    pub(crate) generation: u64,
    pub(crate) started_at_ms: u64,
    pub(crate) ended_at_ms: Option<u64>,
    pub(crate) exit_reason: Option<String>,
    pub(crate) output_revision: u64,
}

impl TerminalView {
    fn detail(&self) -> &str {
        match self.kind {
            TerminalKind::Shell => &self.branch,
            TerminalKind::Command => &self.command,
        }
    }

    fn process(&self) -> &str {
        match self.kind {
            TerminalKind::Shell => self.foreground_process.as_deref().unwrap_or("shell"),
            TerminalKind::Command => &self.command,
        }
    }

    fn table_status(&self) -> String {
        if self.status != "exited" {
            return self.status.clone();
        }
        match self.run.as_ref().and_then(|run| run.exit_reason.as_deref()) {
            Some("exited (code unavailable)") | None => "exited".into(),
            Some(reason) => reason
                .strip_prefix("exited (")
                .and_then(|reason| reason.strip_suffix(')'))
                .map_or_else(|| reason.to_owned(), |code| format!("exit {code}")),
        }
    }
}

impl WorkspaceView {
    fn qualify(&self, inner_id: impl Into<String>) -> QualifiedIdentity {
        QualifiedIdentity::new(self.node.id.clone(), inner_id)
    }

    fn actionable(&self) -> bool {
        self.node.current
            && !self.node.stale
            && (self.node.local
                || self
                    .node
                    .observed_capabilities
                    .iter()
                    .any(|capability| capability == "guarded_remote_management"))
    }

    fn local_actionable(&self) -> bool {
        self.node.local && self.node.current && !self.node.stale
    }

    fn shell_count(&self) -> usize {
        self.items
            .iter()
            .filter(|item| item.ordinary_visible() && item.kind() == ItemKind::Shell)
            .count()
    }

    fn command_count(&self) -> usize {
        self.items
            .iter()
            .filter(|item| item.ordinary_visible() && item.kind() == ItemKind::Command)
            .count()
    }

    fn launcher_count(&self) -> usize {
        self.items
            .iter()
            .filter(|item| item.ordinary_visible() && item.kind() == ItemKind::Launcher)
            .count()
    }

    fn agent_count(&self) -> usize {
        self.items
            .iter()
            .filter(|item| item.ordinary_visible() && item.kind() == ItemKind::Agent)
            .count()
    }

    fn schedule_count(&self) -> usize {
        self.items
            .iter()
            .filter(|item| item.kind() == ItemKind::Schedule)
            .count()
    }

    fn process_count(&self) -> usize {
        self.items
            .iter()
            .filter(|item| {
                item.ordinary_visible()
                    && matches!(
                        item,
                        WorkspaceItemView::Shell(_) | WorkspaceItemView::AgentShell(_)
                    )
            })
            .count()
    }

    fn ordinary_item_count(&self) -> usize {
        self.items
            .iter()
            .filter(|item| item.ordinary_visible())
            .count()
    }

    fn item_owner(&self, index: usize) -> (&NodeView, &str) {
        self.item_owners
            .get(index)
            .map_or((&self.node, self.id.as_str()), |owner| {
                (&owner.node, owner.workspace_id.as_str())
            })
    }

    fn qualify_item(&self, index: usize, inner_id: impl Into<String>) -> QualifiedIdentity {
        QualifiedIdentity::new(self.item_owner(index).0.id.clone(), inner_id)
    }

    fn qualify_item_workspace(&self, index: usize) -> QualifiedIdentity {
        let (node, workspace_id) = self.item_owner(index);
        QualifiedIdentity::new(node.id.clone(), workspace_id)
    }

    fn item_actionable(&self, index: usize) -> bool {
        let node = self.item_owner(index).0;
        node.current
            && !node.stale
            && (node.local
                || node
                    .observed_capabilities
                    .iter()
                    .any(|capability| capability == "guarded_remote_management"))
    }

    fn item_dismissible(&self, index: usize) -> bool {
        let node = self.item_owner(index).0;
        !node.local && (!node.current || node.stale)
    }

    fn item_shell_attachable(&self, index: usize) -> bool {
        let node = self.item_owner(index).0;
        (node.local && node.current && !node.stale)
            || (node.current
                && !node.stale
                && node
                    .observed_capabilities
                    .iter()
                    .any(|capability| capability == "remote_pty_attachment"))
    }

    fn item_launcher_invokable(&self, index: usize) -> bool {
        let node = self.item_owner(index).0;
        (node.local && node.current && !node.stale)
            || (node.current
                && !node.stale
                && node
                    .observed_capabilities
                    .iter()
                    .any(|capability| capability == "typed_node_host_services"))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ItemKind {
    Agent,
    Launcher,
    Shell,
    Command,
    Schedule,
}

impl WorkspaceItemView {
    fn ordinary_visible(&self) -> bool {
        !matches!(self, Self::AgentShell(agent) if agent.schedule_id.is_some())
    }

    fn kind(&self) -> ItemKind {
        match self {
            Self::AgentShell(_) => ItemKind::Agent,
            Self::Launcher(_) => ItemKind::Launcher,
            Self::Schedule(_) => ItemKind::Schedule,
            Self::Shell(shell) => shell.kind.into(),
        }
    }

    fn name(&self) -> &str {
        match self {
            Self::Shell(shell) => &shell.name,
            Self::AgentShell(agent) => &agent.shell.name,
            Self::Launcher(launcher) => &launcher.name,
            Self::Schedule(schedule) => &schedule.name,
        }
    }

    fn status(&self) -> &str {
        match self {
            Self::Shell(shell) => &shell.status,
            Self::AgentShell(agent) => agent.state().label(),
            Self::Launcher(_) => "launcher",
            Self::Schedule(schedule) => schedule.state.label(),
        }
    }

    fn id(&self) -> &str {
        match self {
            Self::Shell(shell) => &shell.id,
            Self::AgentShell(agent) => &agent.shell.id,
            Self::Launcher(launcher) => &launcher.id,
            Self::Schedule(schedule) => &schedule.id,
        }
    }
}

impl ItemKind {
    const fn label(self) -> &'static str {
        match self {
            Self::Agent => "agent",
            Self::Launcher => "launcher",
            Self::Shell => "shell",
            Self::Command => "command",
            Self::Schedule => "schedule",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TerminalKind {
    Shell,
    Command,
}

impl TerminalKind {
    const fn label(self) -> &'static str {
        match self {
            Self::Shell => "shell",
            Self::Command => "command",
        }
    }
}

impl From<TerminalKind> for ItemKind {
    fn from(kind: TerminalKind) -> Self {
        match kind {
            TerminalKind::Shell => Self::Shell,
            TerminalKind::Command => Self::Command,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AgentDisplayState {
    Unknown,
    Working,
    Blocked,
    Idle,
    Inactive,
    Done,
    Untracked,
}

impl AgentDisplayState {
    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::Unknown => "unknown",
            Self::Working => "working",
            Self::Blocked => "blocked",
            Self::Idle => "idle",
            Self::Inactive => "inactive",
            Self::Done => "done",
            Self::Untracked => "untracked",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AttentionReason {
    Blocked,
    Completed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AgentAuthorityDisplay {
    DaemonLifecycle,
    LifecycleIntegration,
    ProcessAdapter,
    TerminalHeuristic,
}

impl AgentAuthorityDisplay {
    const fn label(self) -> &'static str {
        match self {
            Self::DaemonLifecycle => "daemon_lifecycle",
            Self::LifecycleIntegration => "lifecycle_integration",
            Self::ProcessAdapter => "process_adapter",
            Self::TerminalHeuristic => "terminal_heuristic",
        }
    }
}

impl AttentionReason {
    const fn label(self) -> &'static str {
        match self {
            Self::Blocked => "blocked",
            Self::Completed => "completed",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum DashboardEffect {
    Quit,
    AddNode,
    UpgradeNode(String),
    SelectWorkspace {
        workspace_id: String,
    },
    RestoreWorkspace(QualifiedIdentity),
    Open(OpenTarget),
    Close(CloseTarget),
    CreateWorkspace {
        name: String,
    },
    CreateShell(QualifiedIdentity),
    CreateGlobalShell {
        workspace_id: String,
        expected_revision: u64,
        node_id: String,
        owner_workspace_id: String,
        default_cwd: Option<PathBuf>,
    },
    AdoptExternalWorkspace {
        identity: QualifiedIdentity,
        expected_revision: u64,
    },
    OpenGlobalWorkspace {
        workspace_id: String,
        expected_revision: u64,
    },
    RetryGlobalWorkspaceClose {
        workspace_id: String,
    },
    LinkExternalWorkspace {
        workspace_id: String,
        expected_revision: u64,
        identity: QualifiedIdentity,
        expected_owner_revision: u64,
    },
    RetargetNode {
        node_id: String,
        expected_revision: u64,
        route: String,
    },
    ForgetNode {
        node_id: String,
    },
    Rename {
        target: RenameTarget,
        name: String,
    },
    CheckForUpdates,
    RefreshNode(String),
    RestoreDismissedShells(String),
    Refresh,
    RunSchedule(QualifiedIdentity),
    PauseSchedule(QualifiedIdentity),
    ResumeSchedule(QualifiedIdentity),
    CancelExecution(QualifiedIdentity),
    OpenScheduledExecution {
        execution_id: QualifiedIdentity,
    },
    RemoveSchedule(QualifiedIdentity),
    LoadScheduleHistory {
        schedule_id: QualifiedIdentity,
        limit: u16,
    },
    LoadScheduleEditor {
        schedule_id: QualifiedIdentity,
    },
    UpdateSchedule {
        schedule_id: QualifiedIdentity,
        expected_revision: u64,
        update: ScheduleEditUpdate,
    },
    ReadTerminalPreview {
        shell_id: QualifiedIdentity,
        run_id: Option<String>,
        output_revision: u64,
    },
}

impl DashboardEffect {
    fn must_finish_before_quit(&self) -> bool {
        !matches!(
            self,
            Self::Quit
                | Self::CheckForUpdates
                | Self::RefreshNode(_)
                | Self::Refresh
                | Self::LoadScheduleHistory { .. }
                | Self::LoadScheduleEditor { .. }
                | Self::ReadTerminalPreview { .. }
        )
    }
}

pub(crate) enum DashboardEvent {
    KeyPressed {
        code: KeyCode,
        modifiers: KeyModifiers,
    },
    RefreshElapsed,
    PreviewRequested,
    UpdateCheckCompleted,
    OperationCompleted(Result<String, String>),
    WorkspaceSelectionCompleted {
        workspace_id: String,
        result: Result<String, String>,
    },
    ShellCreationCompleted(Result<String, String>),
    RefreshCompleted(Result<DashboardState, String>),
    ScheduleHistoryCompleted {
        schedule_id: QualifiedIdentity,
        result: Result<(Vec<ExecutionView>, bool), String>,
    },
    ScheduleEditorLoaded {
        schedule_id: QualifiedIdentity,
        result: Result<ScheduleEditInspection, String>,
    },
    ScheduleEditorSaved {
        schedule_id: QualifiedIdentity,
        result: Result<String, String>,
    },
    TextPasted(String),
    TerminalPreviewCompleted {
        shell_id: String,
        run_id: Option<String>,
        output_revision: u64,
        output: Result<TerminalPreview, String>,
    },
}

pub(crate) trait DashboardBackend {
    fn execute(&mut self, effect: DashboardEffect) -> DashboardEvent;
}

impl<F> DashboardBackend for F
where
    F: FnMut(DashboardEffect) -> DashboardEvent,
{
    fn execute(&mut self, effect: DashboardEffect) -> DashboardEvent {
        self(effect)
    }
}

struct DashboardRuntime {
    effects: Sender<DashboardEffect>,
    completions: Receiver<(DashboardEffect, DashboardEvent)>,
    update_check_in_flight: bool,
    preview_in_flight: bool,
    operations_in_flight: usize,
}

impl DashboardRuntime {
    fn spawn(mut backend: impl DashboardBackend + Send + 'static) -> Self {
        let (effect_sender, effect_receiver) = mpsc::channel::<DashboardEffect>();
        let (completion_sender, completion_receiver) = mpsc::channel();
        thread::spawn(move || {
            while let Ok(effect) = effect_receiver.recv() {
                let completed_effect = effect.clone();
                let event = backend.execute(effect);
                if completion_sender.send((completed_effect, event)).is_err() {
                    break;
                }
            }
        });
        Self {
            effects: effect_sender,
            completions: completion_receiver,
            update_check_in_flight: false,
            preview_in_flight: false,
            operations_in_flight: 0,
        }
    }

    fn dispatch(&mut self, effects: Vec<DashboardEffect>) -> io::Result<bool> {
        for effect in effects {
            if effect == DashboardEffect::Quit {
                if self.can_quit() {
                    return Ok(true);
                }
                continue;
            }
            if (effect == DashboardEffect::CheckForUpdates && self.update_check_in_flight)
                || (matches!(effect, DashboardEffect::ReadTerminalPreview { .. })
                    && self.preview_in_flight)
            {
                continue;
            }
            match &effect {
                DashboardEffect::CheckForUpdates => self.update_check_in_flight = true,
                DashboardEffect::ReadTerminalPreview { .. } => self.preview_in_flight = true,
                _ => {}
            }
            if effect.must_finish_before_quit() {
                self.operations_in_flight += 1;
            }
            self.effects.send(effect).map_err(|_| {
                io::Error::new(io::ErrorKind::BrokenPipe, "dashboard backend stopped")
            })?;
        }
        Ok(false)
    }

    fn drain(&mut self, app: &mut App) -> io::Result<bool> {
        loop {
            match self.completions.try_recv() {
                Ok((effect, event)) => {
                    match effect {
                        DashboardEffect::CheckForUpdates => self.update_check_in_flight = false,
                        DashboardEffect::ReadTerminalPreview { .. } => {
                            self.preview_in_flight = false;
                        }
                        _ => {}
                    }
                    if effect.must_finish_before_quit() {
                        self.operations_in_flight = self.operations_in_flight.saturating_sub(1);
                    }
                    let effects = app.update(event);
                    if self.dispatch(effects)? {
                        return Ok(true);
                    }
                }
                Err(TryRecvError::Empty) => return Ok(false),
                Err(TryRecvError::Disconnected) => {
                    return Err(io::Error::new(
                        io::ErrorKind::BrokenPipe,
                        "dashboard backend stopped",
                    ));
                }
            }
        }
    }

    fn has_in_flight_effect(&self) -> bool {
        self.update_check_in_flight || self.preview_in_flight || self.operations_in_flight > 0
    }

    fn can_quit(&self) -> bool {
        self.operations_in_flight == 0
    }
}

struct App {
    nodes: Vec<NodeView>,
    all_workspaces: Vec<WorkspaceView>,
    workspaces: Vec<WorkspaceView>,
    all_schedules: Vec<ScheduleView>,
    schedules: Vec<ScheduleView>,
    scheduling: SchedulingView,
    exact_run_attachment: bool,
    schedule_editing: bool,
    cached_projection_dismissal: bool,
    selected_execution_id: Option<String>,
    execution_state: TableState,
    workspace_state: TableState,
    item_state: TableState,
    global_state: TableState,
    node_state: TableState,
    primary_tab: PrimaryTab,
    focus: Focus,
    mode: Mode,
    message: Option<Message>,
    pending_shell_creation: Option<String>,
    pending_close: Option<PendingClose>,
    project_context: ProjectContext,
    terminal_preview: Option<TerminalPreviewState>,
    follow_focused_terminal: bool,
    selection_pinned: bool,
    selected_workspace_id: Option<String>,
    observed_focus_revision: Option<u64>,
}

struct TerminalPreviewState {
    shell_id: String,
    run_id: Option<String>,
    output_revision: u64,
    output: Result<TerminalPreview, String>,
    scroll_from_bottom: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Focus {
    Workspaces,
    Items,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PrimaryTab {
    Workspaces,
    Agents,
    Shells,
    Schedules,
    Nodes,
}

impl PrimaryTab {
    const ALL: [Self; 5] = [
        Self::Workspaces,
        Self::Agents,
        Self::Shells,
        Self::Schedules,
        Self::Nodes,
    ];

    fn kind(self) -> Option<ItemKind> {
        match self {
            Self::Workspaces => None,
            Self::Agents => Some(ItemKind::Agent),
            Self::Shells => Some(ItemKind::Shell),
            Self::Schedules => None,
            Self::Nodes => None,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Workspaces => "WORKSPACES",
            Self::Agents => "AGENTS",
            Self::Shells => "SHELLS",
            Self::Schedules => "SCHEDULES",
            Self::Nodes => "NODES",
        }
    }
}

fn shortcut_tab(key: char) -> Option<PrimaryTab> {
    PrimaryTab::ALL
        .get(key.to_digit(10)?.checked_sub(1)? as usize)
        .copied()
}

#[derive(Clone)]
struct ItemIdentity {
    workspace_id: QualifiedIdentity,
    item_id: QualifiedIdentity,
    kind: ItemIdentityKind,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ItemIdentityKind {
    Shell,
    Launcher,
    Schedule,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum OpenTarget {
    Shell(QualifiedIdentity),
    Launcher {
        workspace_id: QualifiedIdentity,
        launcher_id: QualifiedIdentity,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum RenameTarget {
    GlobalWorkspace {
        workspace_id: String,
        expected_revision: u64,
    },
    Node {
        node_id: String,
        expected_revision: u64,
    },
    Workspace(QualifiedIdentity),
    Shell(QualifiedIdentity),
    Launcher(QualifiedIdentity),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum CloseTarget {
    GlobalWorkspace {
        workspace_id: String,
        expected_revision: u64,
    },
    Workspace(QualifiedIdentity),
    Shell(QualifiedIdentity),
    DismissCachedShell(QualifiedIdentity),
    Launcher(QualifiedIdentity),
    Schedule(QualifiedIdentity),
    Execution(QualifiedIdentity),
}

impl RenameTarget {
    fn label(&self) -> &'static str {
        match self {
            Self::GlobalWorkspace { .. } => "workspace",
            Self::Node { .. } => "Node alias",
            Self::Workspace(_) => "workspace",
            Self::Shell(_) => "shell",
            Self::Launcher(_) => "launcher",
        }
    }
}

enum Mode {
    Normal,
    PickProject(ProjectPicker),
    Palette(CommandPalette),
    Help,
    Rename {
        target: RenameTarget,
        input: String,
    },
    SelectWorkspaceNode(WorkspaceNodePicker),
    LinkWorkspace(LinkWorkspacePicker),
    InspectNode(NodeView),
    RetargetNode {
        node_id: String,
        expected_revision: u64,
        input: String,
    },
    ConfirmForgetNode(NodeView),
    EditSchedule(ScheduleEditor),
}

struct LinkWorkspacePicker {
    identity: QualifiedIdentity,
    expected_owner_revision: u64,
    workspaces: Vec<(String, String, u64)>,
    selected: Option<usize>,
}

struct WorkspaceNodePicker {
    workspace_id: String,
    workspace_name: String,
    expected_revision: u64,
    placements: Vec<WorkspacePlacementView>,
    nodes: Vec<NodeView>,
    selected: Option<usize>,
}

impl WorkspaceNodePicker {
    fn move_selection(&mut self, forwards: bool) {
        let eligible = self
            .nodes
            .iter()
            .enumerate()
            .filter_map(|(index, node)| node.workspace_owner_eligible.then_some(index))
            .collect::<Vec<_>>();
        if eligible.is_empty() {
            self.selected = None;
            return;
        }
        self.selected = Some(
            match self
                .selected
                .and_then(|selected| eligible.iter().position(|index| *index == selected))
            {
                Some(position) if forwards => eligible[(position + 1) % eligible.len()],
                Some(0) => *eligible.last().expect("eligible Node"),
                Some(position) => eligible[position - 1],
                None if forwards => eligible[0],
                None => *eligible.last().expect("eligible Node"),
            },
        );
    }

    fn effect(&self) -> Option<DashboardEffect> {
        let node = self.nodes.get(self.selected?)?;
        if !node.workspace_owner_eligible {
            return None;
        }
        let placement = self
            .placements
            .iter()
            .find(|placement| placement.node.id == node.id);
        Some(DashboardEffect::CreateGlobalShell {
            workspace_id: self.workspace_id.clone(),
            expected_revision: self.expected_revision,
            node_id: node.id.clone(),
            owner_workspace_id: placement.map_or_else(
                || uuid::Uuid::new_v4().to_string(),
                |placement| placement.workspace_id.clone(),
            ),
            default_cwd: placement
                .and_then(|placement| placement.default_cwd.as_deref())
                .map(PathBuf::from),
        })
    }
}

impl LinkWorkspacePicker {
    fn move_selection(&mut self, forwards: bool) {
        if self.workspaces.is_empty() {
            self.selected = None;
            return;
        }
        self.selected = Some(match self.selected {
            Some(index) if forwards => (index + 1) % self.workspaces.len(),
            Some(0) => self.workspaces.len() - 1,
            Some(index) => index - 1,
            None if forwards => 0,
            None => self.workspaces.len() - 1,
        });
    }

    fn effect(&self) -> Option<DashboardEffect> {
        let (workspace_id, _, expected_revision) = self.workspaces.get(self.selected?)?;
        Some(DashboardEffect::LinkExternalWorkspace {
            workspace_id: workspace_id.clone(),
            expected_revision: *expected_revision,
            identity: self.identity.clone(),
            expected_owner_revision: self.expected_owner_revision,
        })
    }
}

struct ScheduleEditor {
    schedule_id: QualifiedIdentity,
    expected_revision: u64,
    field: ScheduleEditorField,
    preset: ScheduleTriggerPreset,
    name: String,
    cron: String,
    timezone: String,
    timezone_query: String,
    prompt: String,
    cursor: usize,
    error: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ScheduleEditorField {
    Name,
    Trigger,
    Cron,
    Timezone,
    Prompt,
}

impl ScheduleEditorField {
    const ALL: [Self; 5] = [
        Self::Name,
        Self::Trigger,
        Self::Cron,
        Self::Timezone,
        Self::Prompt,
    ];
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ScheduleTriggerPreset {
    EveryMinute,
    Hourly,
    Daily,
    Weekdays,
    Weekly,
    Custom,
}

impl ScheduleTriggerPreset {
    const ALL: [Self; 6] = [
        Self::EveryMinute,
        Self::Hourly,
        Self::Daily,
        Self::Weekdays,
        Self::Weekly,
        Self::Custom,
    ];

    const fn label(self) -> &'static str {
        match self {
            Self::EveryMinute => "every minute",
            Self::Hourly => "hourly",
            Self::Daily => "daily 09:00",
            Self::Weekdays => "weekdays 09:00",
            Self::Weekly => "weekly Mon 09:00",
            Self::Custom => "custom cron",
        }
    }

    const fn cron(self) -> Option<&'static str> {
        match self {
            Self::EveryMinute => Some("* * * * *"),
            Self::Hourly => Some("0 * * * *"),
            Self::Daily => Some("0 9 * * *"),
            Self::Weekdays => Some("0 9 * * 1-5"),
            Self::Weekly => Some("0 9 * * 1"),
            Self::Custom => None,
        }
    }

    fn from_cron(cron: &str) -> Self {
        Self::ALL
            .into_iter()
            .find(|preset| preset.cron() == Some(cron))
            .unwrap_or(Self::Custom)
    }
}

impl ScheduleEditor {
    fn cycle_field(&mut self, backwards: bool) {
        let index = ScheduleEditorField::ALL
            .iter()
            .position(|field| *field == self.field)
            .expect("schedule editor field");
        self.field = if backwards {
            ScheduleEditorField::ALL
                [(index + ScheduleEditorField::ALL.len() - 1) % ScheduleEditorField::ALL.len()]
        } else {
            ScheduleEditorField::ALL[(index + 1) % ScheduleEditorField::ALL.len()]
        };
        self.cursor = self.active_text().map_or(0, String::len);
        self.error = None;
    }

    fn cycle_preset(&mut self, backwards: bool) {
        let index = ScheduleTriggerPreset::ALL
            .iter()
            .position(|preset| *preset == self.preset)
            .expect("schedule trigger preset");
        self.preset = if backwards {
            ScheduleTriggerPreset::ALL
                [(index + ScheduleTriggerPreset::ALL.len() - 1) % ScheduleTriggerPreset::ALL.len()]
        } else {
            ScheduleTriggerPreset::ALL[(index + 1) % ScheduleTriggerPreset::ALL.len()]
        };
        if let Some(cron) = self.preset.cron() {
            self.cron = cron.into();
        }
        self.error = None;
    }

    fn active_text_mut(&mut self) -> Option<&mut String> {
        match self.field {
            ScheduleEditorField::Name => Some(&mut self.name),
            ScheduleEditorField::Cron => Some(&mut self.cron),
            ScheduleEditorField::Timezone => None,
            ScheduleEditorField::Prompt => Some(&mut self.prompt),
            ScheduleEditorField::Trigger => None,
        }
    }

    fn active_text(&self) -> Option<&String> {
        match self.field {
            ScheduleEditorField::Name => Some(&self.name),
            ScheduleEditorField::Cron => Some(&self.cron),
            ScheduleEditorField::Timezone => None,
            ScheduleEditorField::Prompt => Some(&self.prompt),
            ScheduleEditorField::Trigger => None,
        }
    }

    fn insert_text(&mut self, text: &str) {
        if self.field == ScheduleEditorField::Timezone {
            self.timezone_query
                .extend(text.chars().filter(|character| !character.is_control()));
            self.select_timezone_match(false, false);
            return;
        }
        let prompt = self.field == ScheduleEditorField::Prompt;
        let value = if prompt {
            text.replace("\r\n", "\n").replace('\r', "\n")
        } else {
            text.chars()
                .filter(|character| *character != '\r' && *character != '\n')
                .collect()
        };
        let cursor = self.cursor;
        if let Some(target) = self.active_text_mut() {
            let cursor = cursor.min(target.len());
            target.insert_str(cursor, &value);
            self.cursor = cursor + value.len();
            self.error = None;
        }
    }

    fn backspace(&mut self) {
        if self.field == ScheduleEditorField::Timezone {
            self.timezone_query.pop();
            self.select_timezone_match(false, false);
            return;
        }
        let cursor = self.cursor;
        if let Some(target) = self.active_text_mut()
            && cursor > 0
        {
            let previous = target[..cursor]
                .char_indices()
                .next_back()
                .map_or(0, |(index, _)| index);
            target.drain(previous..cursor);
            self.cursor = previous;
            self.error = None;
        }
    }

    fn delete(&mut self) {
        let cursor = self.cursor;
        if let Some(target) = self.active_text_mut()
            && cursor < target.len()
        {
            let next = target[cursor..]
                .char_indices()
                .nth(1)
                .map_or(target.len(), |(index, _)| cursor + index);
            target.drain(cursor..next);
            self.error = None;
        }
    }

    fn move_cursor(&mut self, backwards: bool) {
        let Some(target) = self.active_text() else {
            return;
        };
        self.cursor = if backwards {
            target[..self.cursor.min(target.len())]
                .char_indices()
                .next_back()
                .map_or(0, |(index, _)| index)
        } else if self.cursor >= target.len() {
            target.len()
        } else {
            target[self.cursor..]
                .char_indices()
                .nth(1)
                .map_or(target.len(), |(index, _)| self.cursor + index)
        };
    }

    fn timezone_matches(&self) -> Vec<&'static str> {
        let query = self.timezone_query.to_ascii_lowercase();
        chrono_tz::TZ_VARIANTS
            .iter()
            .map(|timezone| timezone.name())
            .filter(|name| query.is_empty() || name.to_ascii_lowercase().contains(&query))
            .collect()
    }

    fn select_timezone_match(&mut self, backwards: bool, cycle: bool) {
        let matches = self.timezone_matches();
        if matches.is_empty() {
            self.error = Some("No matching IANA timezone".into());
            return;
        }
        let current = matches
            .iter()
            .position(|timezone| *timezone == self.timezone)
            .unwrap_or(0);
        let index = if !cycle {
            0
        } else if backwards {
            (current + matches.len() - 1) % matches.len()
        } else {
            (current + 1) % matches.len()
        };
        self.timezone = matches[index].into();
        self.error = None;
    }

    fn move_to_line_edge(&mut self, end: bool) {
        let Some(target) = self.active_text() else {
            return;
        };
        let cursor = self.cursor.min(target.len());
        self.cursor = if end {
            target[cursor..]
                .find('\n')
                .map_or(target.len(), |offset| cursor + offset)
        } else {
            target[..cursor].rfind('\n').map_or(0, |index| index + 1)
        };
    }

    fn save_effect(&mut self) -> Option<DashboardEffect> {
        let error = if self.name.trim().is_empty() || self.name.trim() != self.name {
            Some("Name must be nonempty without surrounding whitespace")
        } else if self.prompt.is_empty() {
            Some("Prompt must be nonempty")
        } else if self.cron.split_whitespace().count() != 5 {
            Some("Cron must contain exactly five fields")
        } else if self.timezone.trim().is_empty() || self.timezone.trim() != self.timezone {
            Some("Timezone must be a nonempty IANA name without surrounding whitespace")
        } else if boomux::scheduling::canonicalize_timezone(&self.timezone).is_err() {
            Some("Timezone must be selected from the IANA timezone list")
        } else {
            None
        };
        if let Some(error) = error {
            self.error = Some(error.into());
            return None;
        }
        Some(DashboardEffect::UpdateSchedule {
            schedule_id: self.schedule_id.clone(),
            expected_revision: self.expected_revision,
            update: ScheduleEditUpdate {
                name: self.name.clone(),
                cron: self.cron.clone(),
                timezone: self.timezone.clone(),
                prompt: self.prompt.clone(),
            },
        })
    }
}

struct ProjectPicker {
    mode: WorkspaceCreationMode,
    projects: Vec<ProjectView>,
    matches: Vec<usize>,
    state: ListState,
    query: String,
    config_path: Option<PathBuf>,
    warning: Option<String>,
    roots_configured: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum WorkspaceCreationMode {
    ByName,
    Project,
}

struct CommandPalette {
    entries: Vec<PaletteEntry>,
    matches: Vec<usize>,
    state: ListState,
    query: String,
}

struct PaletteEntry {
    action_group: PaletteActionGroup,
    kind_group: PaletteKindGroup,
    label: String,
    detail: String,
    keywords: String,
    command: PaletteCommand,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum PaletteActionGroup {
    QuickAccess,
    GoTo,
    Open,
    Create,
    Rename,
    Close,
    Manage,
    Help,
}

impl PaletteActionGroup {
    fn label(self) -> &'static str {
        match self {
            Self::QuickAccess => "QUICK ACCESS",
            Self::GoTo => "GO TO",
            Self::Open => "OPEN",
            Self::Create => "CREATE",
            Self::Rename => "RENAME",
            Self::Close => "CLOSE",
            Self::Manage => "MANAGE",
            Self::Help => "HELP",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum PaletteKindGroup {
    BlockedAgents,
    Attention,
    Nodes,
    Workspaces,
    Agents,
    Shells,
    Commands,
    Launchers,
    Schedules,
    ScheduleNotices,
    Dashboard,
}

impl PaletteKindGroup {
    fn label(self) -> &'static str {
        match self {
            Self::BlockedAgents => "BLOCKED AGENTS",
            Self::Attention => "ATTENTION",
            Self::Nodes => "NODES",
            Self::Workspaces => "WORKSPACES",
            Self::Agents => "AGENTS",
            Self::Shells => "SHELLS",
            Self::Commands => "COMMANDS",
            Self::Launchers => "LAUNCHERS",
            Self::Schedules => "SCHEDULES",
            Self::ScheduleNotices => "SCHEDULE NOTICES",
            Self::Dashboard => "DASHBOARD",
        }
    }
}

#[derive(Clone)]
enum PaletteCommand {
    AddNode,
    CreateWorkspace,
    ShowHelp,
    Workspace {
        workspace_id: QualifiedIdentity,
        action: WorkspacePaletteAction,
    },
    Item {
        identity: ItemIdentity,
        action: ItemPaletteAction,
    },
    Attention {
        workspace_id: QualifiedIdentity,
        shell_id: QualifiedIdentity,
        agent_id: QualifiedIdentity,
    },
    Schedule {
        schedule_id: QualifiedIdentity,
        action: SchedulePaletteAction,
    },
}

#[derive(Clone, Copy)]
enum WorkspacePaletteAction {
    GoTo,
    Restore,
    AddShell,
    Rename,
    Close,
}

#[derive(Clone, Copy)]
enum ItemPaletteAction {
    GoTo,
    Open,
    Rename,
    Close,
}

#[derive(Clone)]
enum SchedulePaletteAction {
    GoTo,
    Run,
    PauseResume,
    SelectExecution(String),
    Remove,
}

struct Message {
    text: String,
    error: bool,
}

impl Message {
    fn from_result(result: Result<String, String>) -> Self {
        match result {
            Ok(text) => Self { text, error: false },
            Err(text) => Self { text, error: true },
        }
    }
}

struct PendingClose {
    target: CloseTarget,
    name: String,
    shell_count: usize,
    launcher_count: usize,
}

impl ProjectPicker {
    fn new(context: &ProjectContext) -> Self {
        let mut picker = Self {
            mode: if context.roots_configured {
                WorkspaceCreationMode::Project
            } else {
                WorkspaceCreationMode::ByName
            },
            projects: context.projects.clone(),
            matches: Vec::new(),
            state: ListState::default(),
            query: String::new(),
            config_path: context.config_path.clone(),
            warning: context.warning.clone(),
            roots_configured: context.roots_configured,
        };
        picker.update_matches();
        picker
    }

    fn update_matches(&mut self) {
        let query = self.query.to_lowercase();
        let mut matches: Vec<_> = self
            .projects
            .iter()
            .enumerate()
            .filter_map(|(index, project)| {
                project_match_score(project, &query).map(|score| (index, score))
            })
            .collect();
        matches.sort_by_key(|(index, score)| (self.projects[*index].group_order, *score));
        self.matches = matches.into_iter().map(|(index, _)| index).collect();
        self.state.select(
            (self.mode == WorkspaceCreationMode::Project && !self.matches.is_empty()).then_some(0),
        );
    }

    fn custom_name(&self) -> Option<&str> {
        let name = self.query.trim();
        (!name.is_empty()).then_some(name)
    }

    fn toggle_mode(&mut self) {
        self.mode = match self.mode {
            WorkspaceCreationMode::ByName => WorkspaceCreationMode::Project,
            WorkspaceCreationMode::Project => WorkspaceCreationMode::ByName,
        };
        self.update_matches();
    }

    fn selected(&self) -> Option<&ProjectView> {
        if self.mode != WorkspaceCreationMode::Project {
            return None;
        }
        self.state
            .selected()
            .and_then(|index| self.matches.get(index))
            .and_then(|index| self.projects.get(*index))
    }

    fn next(&mut self) {
        if self.mode != WorkspaceCreationMode::Project || self.matches.is_empty() {
            return;
        }
        let next = self
            .state
            .selected()
            .map_or(0, |index| (index + 1) % self.matches.len());
        self.state.select(Some(next));
    }

    fn previous(&mut self) {
        if self.mode != WorkspaceCreationMode::Project || self.matches.is_empty() {
            return;
        }
        let previous = self.state.selected().map_or(0, |index| {
            if index == 0 {
                self.matches.len() - 1
            } else {
                index - 1
            }
        });
        self.state.select(Some(previous));
    }
}

impl CommandPalette {
    #[cfg(test)]
    fn new(workspaces: &[WorkspaceView]) -> Self {
        Self::new_with_schedules(
            workspaces,
            &[],
            &SchedulingView::Unsupported {
                required_protocol: 25,
                negotiated: 0,
            },
        )
    }

    fn new_with_schedules(
        workspaces: &[WorkspaceView],
        schedules: &[ScheduleView],
        scheduling: &SchedulingView,
    ) -> Self {
        let mut entries = vec![
            PaletteEntry {
                action_group: PaletteActionGroup::Create,
                kind_group: PaletteKindGroup::Nodes,
                label: "Add remote Node".into(),
                detail: "open guided SSH setup in a new terminal".into(),
                keywords: "connect register ssh machine host".into(),
                command: PaletteCommand::AddNode,
            },
            PaletteEntry {
                action_group: PaletteActionGroup::Create,
                kind_group: PaletteKindGroup::Workspaces,
                label: "Create workspace".into(),
                detail: "choose a project suggestion or enter a name".into(),
                keywords: "add new project".into(),
                command: PaletteCommand::CreateWorkspace,
            },
            PaletteEntry {
                action_group: PaletteActionGroup::Help,
                kind_group: PaletteKindGroup::Dashboard,
                label: "Show dashboard help".into(),
                detail: "keys, kinds, states, and attention".into(),
                keywords: "explain keyboard shortcuts question".into(),
                command: PaletteCommand::ShowHelp,
            },
        ];
        let mut attention_entries = Vec::new();
        for workspace in workspaces {
            let workspace_keywords = format!("workspace {} {}", workspace.name, workspace.id);
            for (action, action_group) in [
                (WorkspacePaletteAction::GoTo, PaletteActionGroup::GoTo),
                (WorkspacePaletteAction::Restore, PaletteActionGroup::Open),
                (WorkspacePaletteAction::AddShell, PaletteActionGroup::Create),
                (WorkspacePaletteAction::Rename, PaletteActionGroup::Rename),
                (WorkspacePaletteAction::Close, PaletteActionGroup::Close),
            ] {
                if !workspace.actionable() && !matches!(action, WorkspacePaletteAction::GoTo) {
                    continue;
                }
                entries.push(PaletteEntry {
                    action_group,
                    kind_group: if matches!(action, WorkspacePaletteAction::AddShell) {
                        PaletteKindGroup::Shells
                    } else {
                        PaletteKindGroup::Workspaces
                    },
                    label: if matches!(action, WorkspacePaletteAction::AddShell) {
                        format!("Add shell to {}", workspace.name)
                    } else {
                        workspace.name.clone()
                    },
                    detail: format!(
                        "{} items, {} attention",
                        workspace.ordinary_item_count(),
                        workspace.attention_count
                    ),
                    keywords: workspace_keywords.clone(),
                    command: PaletteCommand::Workspace {
                        workspace_id: workspace.qualify(&workspace.id),
                        action,
                    },
                });
            }

            for (item_index, item) in workspace.items.iter().enumerate() {
                if !item.ordinary_visible() {
                    continue;
                }
                let kind = item.kind();
                if kind == ItemKind::Schedule {
                    continue;
                }
                let identity = ItemIdentity {
                    workspace_id: workspace.qualify_item_workspace(item_index),
                    item_id: workspace.qualify_item(item_index, item.id()),
                    kind: if kind == ItemKind::Launcher {
                        ItemIdentityKind::Launcher
                    } else {
                        ItemIdentityKind::Shell
                    },
                };
                let keywords = format!(
                    "{} {} {} {} {} {}",
                    workspace.name,
                    workspace.id,
                    kind.label(),
                    item.name(),
                    item.status(),
                    item.id()
                );
                let kind_group = match kind {
                    ItemKind::Agent => PaletteKindGroup::Agents,
                    ItemKind::Shell => PaletteKindGroup::Shells,
                    ItemKind::Command => PaletteKindGroup::Commands,
                    ItemKind::Launcher => PaletteKindGroup::Launchers,
                    ItemKind::Schedule => unreachable!("schedule items use typed schedule entries"),
                };
                for (action, action_group) in [
                    (ItemPaletteAction::GoTo, PaletteActionGroup::GoTo),
                    (ItemPaletteAction::Open, PaletteActionGroup::Open),
                    (ItemPaletteAction::Rename, PaletteActionGroup::Rename),
                    (ItemPaletteAction::Close, PaletteActionGroup::Close),
                ] {
                    if !workspace.item_actionable(item_index)
                        && !matches!(action, ItemPaletteAction::GoTo)
                    {
                        continue;
                    }
                    entries.push(PaletteEntry {
                        action_group,
                        kind_group,
                        label: format!("{} / {}", workspace.name, item.name()),
                        detail: item.status().into(),
                        keywords: keywords.clone(),
                        command: PaletteCommand::Item {
                            identity: identity.clone(),
                            action,
                        },
                    });
                }
                if matches!(item, WorkspaceItemView::AgentShell(agent) if agent.state() == AgentDisplayState::Blocked)
                {
                    entries.push(PaletteEntry {
                        action_group: PaletteActionGroup::QuickAccess,
                        kind_group: PaletteKindGroup::BlockedAgents,
                        label: format!("{} / {}", workspace.name, item.name()),
                        detail: "blocked".into(),
                        keywords: format!("filter current needs input {keywords}"),
                        command: PaletteCommand::Item {
                            identity,
                            action: ItemPaletteAction::GoTo,
                        },
                    });
                }
            }

            for attention in &workspace.attention {
                attention_entries.push((
                    usize::from(attention.reason != AttentionReason::Blocked),
                    attention.observed_at_ms,
                    workspace.id.clone(),
                    attention.shell_id.clone(),
                    PaletteEntry {
                        action_group: PaletteActionGroup::QuickAccess,
                        kind_group: PaletteKindGroup::Attention,
                        label: format!("{} / {}", workspace.name, attention.agent_name),
                        detail: format!("{}: {}", attention.reason.label(), attention.evidence),
                        keywords: format!(
                            "attention unseen outstanding {} {} {} {}",
                            attention.reason.label(),
                            attention.evidence,
                            workspace.name,
                            attention.shell_id
                        ),
                        command: PaletteCommand::Attention {
                            workspace_id: QualifiedIdentity::new(
                                if attention.node_id.is_empty() {
                                    workspace.node.id.clone()
                                } else {
                                    attention.node_id.clone()
                                },
                                if attention.workspace_id.is_empty() {
                                    workspace.id.clone()
                                } else {
                                    attention.workspace_id.clone()
                                },
                            ),
                            shell_id: QualifiedIdentity::new(
                                if attention.node_id.is_empty() {
                                    workspace.node.id.clone()
                                } else {
                                    attention.node_id.clone()
                                },
                                &attention.shell_id,
                            ),
                            agent_id: QualifiedIdentity::new(
                                if attention.node_id.is_empty() {
                                    workspace.node.id.clone()
                                } else {
                                    attention.node_id.clone()
                                },
                                &attention.agent_id,
                            ),
                        },
                    },
                ));
            }
        }
        attention_entries.sort_by(|left, right| {
            left.0
                .cmp(&right.0)
                .then_with(|| right.1.cmp(&left.1))
                .then_with(|| left.2.cmp(&right.2))
                .then_with(|| left.3.cmp(&right.3))
        });
        entries.extend(attention_entries.into_iter().map(|entry| entry.4));

        for schedule in schedules {
            let keywords = format!(
                "schedule {} {} {} {} {}",
                schedule.name,
                schedule.id,
                schedule.workspace,
                schedule.integration,
                schedule.state.label()
            );
            for (action, action_group, detail) in [
                (
                    SchedulePaletteAction::GoTo,
                    PaletteActionGroup::GoTo,
                    "inspect",
                ),
                (
                    SchedulePaletteAction::Run,
                    PaletteActionGroup::Manage,
                    "run now",
                ),
                (
                    SchedulePaletteAction::PauseResume,
                    PaletteActionGroup::Manage,
                    if schedule.state == ScheduleDisplayState::Paused {
                        "resume"
                    } else {
                        "pause"
                    },
                ),
                (
                    SchedulePaletteAction::Remove,
                    PaletteActionGroup::Close,
                    "remove with confirmation",
                ),
            ] {
                if !schedule.actionable && !matches!(action, SchedulePaletteAction::GoTo) {
                    continue;
                }
                entries.push(PaletteEntry {
                    action_group,
                    kind_group: PaletteKindGroup::Schedules,
                    label: format!("{} / {}", schedule.workspace, schedule.name),
                    detail: detail.into(),
                    keywords: keywords.clone(),
                    command: PaletteCommand::Schedule {
                        schedule_id: schedule.qualify(&schedule.id),
                        action,
                    },
                });
            }
            if let Some(execution) = schedule.executions.first()
                && (execution.state == ExecutionDisplayState::DispatchFailed
                    || execution.state == ExecutionDisplayState::Interrupted
                    || execution.state == ExecutionDisplayState::Skipped
                    || execution.agent_state == Some(AgentDisplayState::Blocked))
            {
                entries.push(PaletteEntry {
                    action_group: PaletteActionGroup::QuickAccess,
                    kind_group: PaletteKindGroup::ScheduleNotices,
                    label: format!("{} / {}", schedule.workspace, schedule.name),
                    detail: execution_summary(execution),
                    keywords: format!("schedule notice failed skipped missed blocked {keywords}"),
                    command: PaletteCommand::Schedule {
                        schedule_id: schedule.qualify(&schedule.id),
                        action: SchedulePaletteAction::SelectExecution(execution.id.clone()),
                    },
                });
            }
        }
        if matches!(scheduling, SchedulingView::Offline { .. }) {
            entries.push(PaletteEntry {
                action_group: PaletteActionGroup::QuickAccess,
                kind_group: PaletteKindGroup::ScheduleNotices,
                label: "Scheduler offline".into(),
                detail: "timed dispatch is not reliable; inspect daemon status and doctor".into(),
                keywords: "schedule scheduler offline daemon doctor restart".into(),
                command: schedules
                    .first()
                    .map_or(PaletteCommand::ShowHelp, |schedule| {
                        PaletteCommand::Schedule {
                            schedule_id: schedule.qualify(&schedule.id),
                            action: SchedulePaletteAction::GoTo,
                        }
                    }),
            });
        }

        let mut palette = Self {
            entries,
            matches: Vec::new(),
            state: ListState::default(),
            query: String::new(),
        };
        palette.update_matches();
        palette
    }

    fn update_matches(&mut self) {
        let query = self.query.to_lowercase();
        let mut matches = self
            .entries
            .iter()
            .enumerate()
            .filter_map(|(index, entry)| {
                palette_match_score(entry, &query).map(|score| (index, score))
            })
            .collect::<Vec<_>>();
        matches.sort_by_key(|(index, score)| {
            let entry = &self.entries[*index];
            (entry.action_group, entry.kind_group, *score, *index)
        });
        self.matches = matches.into_iter().map(|(index, _)| index).collect();
        self.state.select((!self.matches.is_empty()).then_some(0));
    }

    fn selected_command(&self) -> Option<PaletteCommand> {
        self.state
            .selected()
            .and_then(|selected| self.matches.get(selected))
            .and_then(|entry| self.entries.get(*entry))
            .map(|entry| entry.command.clone())
    }

    fn next(&mut self) {
        if self.matches.is_empty() {
            return;
        }
        let next = self
            .state
            .selected()
            .map_or(0, |index| (index + 1) % self.matches.len());
        self.state.select(Some(next));
    }

    fn previous(&mut self) {
        if self.matches.is_empty() {
            return;
        }
        let previous = self.state.selected().map_or(0, |index| {
            if index == 0 {
                self.matches.len() - 1
            } else {
                index - 1
            }
        });
        self.state.select(Some(previous));
    }
}

fn palette_match_score(entry: &PaletteEntry, query: &str) -> Option<usize> {
    if query.is_empty() {
        return Some(0);
    }
    let candidate = format!(
        "{} {} {} {} {}",
        entry.action_group.label(),
        entry.kind_group.label(),
        entry.label,
        entry.detail,
        entry.keywords
    )
    .to_lowercase();
    query.split_whitespace().try_fold(0, |score, token| {
        candidate
            .find(token)
            .map(|index| score + index)
            .or_else(|| {
                candidate
                    .split_whitespace()
                    .any(|word| is_subsequence(token, word))
                    .then_some(score + candidate.len())
            })
    })
}

fn project_match_score(project: &ProjectView, query: &str) -> Option<(u8, usize)> {
    if query.is_empty() {
        return Some((0, 0));
    }
    let name = project.name.to_lowercase();
    let path = project.path.to_string_lossy().to_lowercase();
    if name.starts_with(query) {
        Some((0, name.len()))
    } else if let Some(index) = name.find(query) {
        Some((1, index))
    } else if let Some(index) = path.find(query) {
        Some((2, index))
    } else if is_subsequence(query, &name) {
        Some((3, name.len()))
    } else if is_subsequence(query, &path) {
        Some((4, path.len()))
    } else {
        None
    }
}

fn is_subsequence(query: &str, candidate: &str) -> bool {
    let mut query = query.chars();
    let mut expected = query.next();
    for character in candidate.chars() {
        if expected == Some(character) {
            expected = query.next();
            if expected.is_none() {
                return true;
            }
        }
    }
    expected.is_none()
}

impl App {
    fn new(workspaces: Vec<WorkspaceView>, project_context: ProjectContext) -> Self {
        let mut workspace_state = TableState::default();
        let mut item_state = TableState::default();
        if !workspaces.is_empty() {
            workspace_state.select(Some(0));
            if !workspaces[0].items.is_empty() {
                item_state.select(Some(0));
            }
        }
        let nodes = workspaces.iter().fold(Vec::new(), |mut nodes, workspace| {
            if !nodes
                .iter()
                .any(|node: &NodeView| node.id == workspace.node.id)
            {
                nodes.push(workspace.node.clone());
            }
            nodes
        });
        let has_nodes = !nodes.is_empty();
        Self {
            nodes,
            all_workspaces: workspaces.clone(),
            workspaces,
            all_schedules: Vec::new(),
            schedules: Vec::new(),
            scheduling: SchedulingView::Unsupported {
                required_protocol: 22,
                negotiated: 0,
            },
            exact_run_attachment: false,
            schedule_editing: false,
            cached_projection_dismissal: false,
            selected_execution_id: None,
            execution_state: TableState::default(),
            workspace_state,
            item_state,
            global_state: TableState::default(),
            node_state: TableState::default().with_selected(has_nodes.then_some(0)),
            primary_tab: PrimaryTab::Workspaces,
            focus: Focus::Workspaces,
            mode: Mode::Normal,
            message: None,
            pending_shell_creation: None,
            pending_close: None,
            project_context,
            terminal_preview: None,
            follow_focused_terminal: false,
            selection_pinned: false,
            selected_workspace_id: None,
            observed_focus_revision: None,
        }
    }

    fn enable_focus_following(&mut self, focused_terminal: Option<&FocusedTerminalView>) {
        self.follow_focused_terminal = true;
        self.apply_focused_terminal(focused_terminal);
    }

    fn apply_focused_terminal(&mut self, focused_terminal: Option<&FocusedTerminalView>) {
        if !self.follow_focused_terminal || self.selection_pinned {
            return;
        }
        let Some(focused) = focused_terminal else {
            return;
        };
        if self
            .observed_focus_revision
            .is_some_and(|revision| revision >= focused.revision)
        {
            return;
        }
        if !matches!(self.mode, Mode::Normal) || self.pending_close.is_some() {
            return;
        }
        let Some((workspace_index, item_index)) =
            self.workspaces
                .iter()
                .enumerate()
                .find_map(|(workspace_index, workspace)| {
                    workspace
                        .items
                        .iter()
                        .enumerate()
                        .find(|(item_index, item)| {
                            let (node, owner_workspace_id) = workspace.item_owner(*item_index);
                            focused
                                .node_id
                                .as_ref()
                                .map_or(node.local, |node_id| node.id == *node_id)
                                && (focused.node_id.is_some()
                                    || owner_workspace_id == focused.workspace_id)
                                && !matches!(item, WorkspaceItemView::Launcher(_))
                                && item.id() == focused.shell_id
                                && (self.primary_tab != PrimaryTab::Workspaces
                                    || item.ordinary_visible())
                        })
                        .map(|(item_index, _)| (workspace_index, item_index))
                })
        else {
            return;
        };
        let item = &self.workspaces[workspace_index].items[item_index];
        if self.primary_tab != PrimaryTab::Workspaces
            && self.primary_tab.kind() != Some(item.kind())
        {
            return;
        }
        self.observed_focus_revision = Some(focused.revision);
        if self.primary_tab == PrimaryTab::Workspaces {
            self.workspace_state.select(Some(workspace_index));
            self.item_state.select(
                self.workspaces[workspace_index]
                    .items
                    .iter()
                    .take(item_index + 1)
                    .filter(|item| item.ordinary_visible())
                    .count()
                    .checked_sub(1),
            );
        } else {
            let identity = item_identity(&self.workspaces[workspace_index], item_index, item);
            self.global_state
                .select(self.global_item_position(&identity));
        }
        self.set_focus(Focus::Items);
    }

    fn toggle_selection_pin(&mut self) {
        if !self.follow_focused_terminal {
            return;
        }
        self.selection_pinned = !self.selection_pinned;
        if !self.selection_pinned {
            self.observed_focus_revision = None;
        }
    }

    fn selected_index(&self) -> Option<usize> {
        self.workspace_state.selected()
    }

    fn selected(&self) -> Option<&WorkspaceView> {
        self.selected_index()
            .and_then(|index| self.workspaces.get(index))
    }

    fn selected_item(&self) -> Option<&WorkspaceItemView> {
        self.selected_item_location()
            .and_then(|(workspace, item)| self.workspaces.get(workspace)?.items.get(item))
    }

    fn selected_schedule(&self) -> Option<&ScheduleView> {
        (self.primary_tab == PrimaryTab::Schedules)
            .then(|| self.global_state.selected())
            .flatten()
            .and_then(|index| self.schedules.get(index))
    }

    fn selected_execution(&self) -> Option<&ExecutionView> {
        let schedule = self.selected_schedule()?;
        let id = self.selected_execution_id.as_deref()?;
        schedule
            .executions
            .iter()
            .find(|execution| execution.id == id)
    }

    fn selected_schedule_history_effect(&self) -> Option<DashboardEffect> {
        self.selected_schedule()
            .filter(|schedule| !schedule.history_scoped)
            .map(|schedule| DashboardEffect::LoadScheduleHistory {
                schedule_id: schedule.qualify(&schedule.id),
                limit: 100,
            })
    }

    fn sync_selected_execution(&mut self) {
        let retained = self.selected_schedule().is_some_and(|schedule| {
            self.selected_execution_id.as_deref().is_some_and(|id| {
                schedule
                    .executions
                    .iter()
                    .any(|execution| execution.id == id)
            })
        });
        if !retained {
            self.selected_execution_id = self
                .selected_schedule()
                .and_then(|schedule| schedule.executions.first())
                .map(|execution| execution.id.clone());
        }
        let selected = self.selected_schedule().and_then(|schedule| {
            self.selected_execution_id.as_deref().and_then(|id| {
                schedule
                    .executions
                    .iter()
                    .position(|execution| execution.id == id)
            })
        });
        self.execution_state.select(selected);
    }

    fn cycle_execution(&mut self, older: bool) {
        let Some(schedule) = self.selected_schedule() else {
            return;
        };
        if schedule.executions.is_empty() {
            self.selected_execution_id = None;
            return;
        }
        let current = self
            .selected_execution_id
            .as_deref()
            .and_then(|id| {
                schedule
                    .executions
                    .iter()
                    .position(|execution| execution.id == id)
            })
            .unwrap_or(0);
        let next = if older {
            (current + 1) % schedule.executions.len()
        } else if current == 0 {
            schedule.executions.len() - 1
        } else {
            current - 1
        };
        self.selected_execution_id = Some(schedule.executions[next].id.clone());
        self.execution_state.select(Some(next));
        self.message = None;
    }

    fn selected_item_location(&self) -> Option<(usize, usize)> {
        if self.primary_tab == PrimaryTab::Schedules {
            return None;
        }
        if self.primary_tab == PrimaryTab::Workspaces {
            let workspace_index = self.workspace_state.selected()?;
            let ordinal = self.item_state.selected()?;
            let item_index = self
                .workspaces
                .get(workspace_index)?
                .items
                .iter()
                .enumerate()
                .filter(|(_, item)| item.ordinary_visible())
                .nth(ordinal)?
                .0;
            return Some((workspace_index, item_index));
        }
        self.global_item_location(self.global_state.selected()?)
    }

    fn selected_item_workspace(&self) -> Option<&WorkspaceView> {
        let (workspace, _) = self.selected_item_location()?;
        self.workspaces.get(workspace)
    }

    fn global_item_location(&self, ordinal: usize) -> Option<(usize, usize)> {
        let kind = self.primary_tab.kind()?;
        self.workspaces
            .iter()
            .enumerate()
            .flat_map(|(workspace_index, workspace)| {
                workspace
                    .items
                    .iter()
                    .enumerate()
                    .filter(move |(_, item)| item.kind() == kind)
                    .map(move |(item_index, _)| (workspace_index, item_index))
            })
            .nth(ordinal)
    }

    fn global_item_count(&self) -> usize {
        if self.primary_tab == PrimaryTab::Nodes {
            return self.nodes.len();
        }
        if self.primary_tab == PrimaryTab::Schedules {
            return self.schedules.len();
        }
        let Some(kind) = self.primary_tab.kind() else {
            return 0;
        };
        self.workspaces
            .iter()
            .flat_map(|workspace| &workspace.items)
            .filter(|item| item.kind() == kind)
            .count()
    }

    fn select_tab(&mut self, tab: PrimaryTab) {
        self.primary_tab = tab;
        if tab == PrimaryTab::Workspaces {
            self.focus = Focus::Workspaces;
            return;
        }
        if tab == PrimaryTab::Nodes {
            self.focus = Focus::Workspaces;
            self.node_state
                .select((!self.nodes.is_empty()).then_some(0));
            self.message = None;
            return;
        }
        self.focus = if tab == PrimaryTab::Schedules {
            Focus::Workspaces
        } else {
            Focus::Items
        };
        self.global_state
            .select((self.global_item_count() > 0).then_some(0));
        if tab == PrimaryTab::Schedules {
            self.sync_selected_execution();
        }
        self.message = None;
    }

    fn cycle_tab(&mut self, backwards: bool) {
        let index = PrimaryTab::ALL
            .iter()
            .position(|tab| *tab == self.primary_tab)
            .expect("primary tab");
        let next = if backwards {
            (index + PrimaryTab::ALL.len() - 1) % PrimaryTab::ALL.len()
        } else {
            (index + 1) % PrimaryTab::ALL.len()
        };
        self.select_tab(PrimaryTab::ALL[next]);
    }

    fn next(&mut self) {
        if self.primary_tab == PrimaryTab::Schedules && self.focus == Focus::Items {
            self.cycle_execution(true);
            return;
        }
        if self.primary_tab != PrimaryTab::Workspaces {
            if self.primary_tab == PrimaryTab::Nodes {
                if !self.nodes.is_empty() {
                    let next = self
                        .node_state
                        .selected()
                        .map_or(0, |index| (index + 1) % self.nodes.len());
                    self.node_state.select(Some(next));
                }
                return;
            }
            let item_count = self.global_item_count();
            if item_count > 0 {
                let next = self
                    .global_state
                    .selected()
                    .map_or(0, |index| (index + 1) % item_count);
                self.global_state.select(Some(next));
            }
            self.message = None;
            if self.primary_tab == PrimaryTab::Schedules {
                self.sync_selected_execution();
            }
            return;
        }
        match self.focus {
            Focus::Workspaces => {
                if self.workspaces.is_empty() {
                    return;
                }
                let next = self
                    .selected_index()
                    .map_or(0, |index| (index + 1) % self.workspaces.len());
                self.workspace_state.select(Some(next));
                self.select_first_details();
            }
            Focus::Items => {
                let item_count = self
                    .selected()
                    .map_or(0, WorkspaceView::ordinary_item_count);
                if item_count == 0 {
                    return;
                }
                let next = self
                    .item_state
                    .selected()
                    .map_or(0, |index| (index + 1) % item_count);
                self.item_state.select(Some(next));
            }
        }
        self.message = None;
    }

    fn previous(&mut self) {
        if self.primary_tab == PrimaryTab::Schedules && self.focus == Focus::Items {
            self.cycle_execution(false);
            return;
        }
        if self.primary_tab != PrimaryTab::Workspaces {
            if self.primary_tab == PrimaryTab::Nodes {
                if !self.nodes.is_empty() {
                    let previous = self.node_state.selected().map_or(0, |index| {
                        if index == 0 {
                            self.nodes.len() - 1
                        } else {
                            index - 1
                        }
                    });
                    self.node_state.select(Some(previous));
                }
                return;
            }
            let item_count = self.global_item_count();
            if item_count > 0 {
                let previous = self.global_state.selected().map_or(0, |index| {
                    if index == 0 {
                        item_count - 1
                    } else {
                        index - 1
                    }
                });
                self.global_state.select(Some(previous));
            }
            self.message = None;
            if self.primary_tab == PrimaryTab::Schedules {
                self.sync_selected_execution();
            }
            return;
        }
        match self.focus {
            Focus::Workspaces => {
                if self.workspaces.is_empty() {
                    return;
                }
                let previous = self.selected_index().map_or(0, |index| {
                    if index == 0 {
                        self.workspaces.len() - 1
                    } else {
                        index - 1
                    }
                });
                self.workspace_state.select(Some(previous));
                self.select_first_details();
            }
            Focus::Items => {
                let item_count = self
                    .selected()
                    .map_or(0, WorkspaceView::ordinary_item_count);
                if item_count == 0 {
                    return;
                }
                let previous = self.item_state.selected().map_or(0, |index| {
                    if index == 0 {
                        item_count - 1
                    } else {
                        index - 1
                    }
                });
                self.item_state.select(Some(previous));
            }
        }
        self.message = None;
    }

    fn set_focus(&mut self, focus: Focus) {
        self.focus = focus;
        self.message = None;
    }

    fn open_palette(&mut self) {
        self.mode = Mode::Palette(CommandPalette::new_with_schedules(
            &self.workspaces,
            &self.schedules,
            &self.scheduling,
        ));
        self.message = None;
    }

    fn select_workspace(&mut self, workspace_id: &QualifiedIdentity, focus: Focus) -> bool {
        let Some(index) = self.workspaces.iter().position(|workspace| {
            workspace.node.id == workspace_id.node_id && workspace.id == workspace_id.inner_id
        }) else {
            self.message = Some(Message {
                text: "workspace is no longer available".into(),
                error: true,
            });
            return false;
        };
        self.select_tab(PrimaryTab::Workspaces);
        self.workspace_state.select(Some(index));
        self.select_first_details();
        self.set_focus(focus);
        true
    }

    fn select_item_identity(&mut self, identity: &ItemIdentity) -> bool {
        let Some(workspace_index) = self.workspaces.iter().position(|workspace| {
            workspace.items.iter().enumerate().any(|(item_index, _)| {
                let (node, workspace_id) = workspace.item_owner(item_index);
                node.id == identity.workspace_id.node_id
                    && workspace_id == identity.workspace_id.inner_id
            })
        }) else {
            self.message = Some(Message {
                text: "item workspace is no longer available".into(),
                error: true,
            });
            return false;
        };
        let Some(item_ordinal) = self.workspaces[workspace_index]
            .items
            .iter()
            .enumerate()
            .filter(|(_, item)| item.ordinary_visible())
            .position(|(item_index, item)| {
                let owner = self.workspaces[workspace_index].item_owner(item_index);
                owner.0.id == identity.workspace_id.node_id
                    && owner.1 == identity.workspace_id.inner_id
                    && item_matches(item, identity)
            })
        else {
            self.select_tab(PrimaryTab::Workspaces);
            self.workspace_state.select(Some(workspace_index));
            self.select_first_details();
            self.message = Some(Message {
                text: "item is no longer available; selected its workspace".into(),
                error: true,
            });
            return false;
        };
        self.select_tab(PrimaryTab::Workspaces);
        self.workspace_state.select(Some(workspace_index));
        self.item_state.select(Some(item_ordinal));
        self.set_focus(Focus::Items);
        true
    }

    fn select_schedule_id(&mut self, schedule_id: &QualifiedIdentity) -> bool {
        let Some(index) = self.schedules.iter().position(|schedule| {
            schedule.node_id == schedule_id.node_id && schedule.id == schedule_id.inner_id
        }) else {
            self.message = Some(Message {
                text: "schedule is no longer available".into(),
                error: true,
            });
            return false;
        };
        self.select_tab(PrimaryTab::Schedules);
        self.global_state.select(Some(index));
        self.sync_selected_execution();
        true
    }

    fn select_execution_id(&mut self, execution_id: &str) -> bool {
        let Some(schedule) = self.selected_schedule() else {
            return false;
        };
        if !schedule
            .executions
            .iter()
            .any(|execution| execution.id == execution_id)
        {
            self.message = Some(Message {
                text: "execution is no longer retained in this schedule history".into(),
                error: true,
            });
            return false;
        }
        self.selected_execution_id = Some(execution_id.to_owned());
        self.sync_selected_execution();
        true
    }

    fn handle_focus_key(&mut self, key: KeyCode) -> bool {
        if self.primary_tab != PrimaryTab::Workspaces && self.primary_tab != PrimaryTab::Schedules {
            return false;
        }
        let focus = match key {
            KeyCode::Left if self.primary_tab == PrimaryTab::Schedules => Focus::Workspaces,
            KeyCode::Left | KeyCode::Char('h') => Focus::Workspaces,
            KeyCode::Right | KeyCode::Char('l') => Focus::Items,
            _ => return false,
        };
        self.set_focus(focus);
        true
    }

    fn select_first_details(&mut self) {
        self.item_state.select(
            self.selected()
                .is_some_and(|workspace| workspace.ordinary_item_count() > 0)
                .then_some(0),
        );
    }

    fn request_rename(&mut self) -> Option<DashboardEffect> {
        if self.primary_tab == PrimaryTab::Nodes {
            if let Some(node) = self
                .node_state
                .selected()
                .and_then(|index| self.nodes.get(index))
                .filter(|node| !node.local)
                && let Some(expected_revision) = node.registration_revision
            {
                self.mode = Mode::Rename {
                    target: RenameTarget::Node {
                        node_id: node.id.clone(),
                        expected_revision,
                    },
                    input: String::new(),
                };
            }
            return None;
        }
        let schedule_id = if self.primary_tab == PrimaryTab::Schedules {
            self.selected_schedule()
                .map(|schedule| schedule.qualify(&schedule.id))
        } else {
            match self.selected_item() {
                Some(WorkspaceItemView::Schedule(schedule)) => {
                    self.selected_item_location().map(|(workspace, item)| {
                        self.workspaces[workspace].qualify_item(item, &schedule.id)
                    })
                }
                _ => None,
            }
        };
        if let Some(schedule_id) = schedule_id {
            if !self.schedule_editing {
                self.message = Some(Message {
                    text:
                        "Schedule editing requires daemon protocol 27; upgrade and restart Boomux"
                            .into(),
                    error: false,
                });
                return None;
            }
            if self.primary_tab != PrimaryTab::Schedules && !self.select_schedule_id(&schedule_id) {
                return None;
            }
            self.message = None;
            return Some(DashboardEffect::LoadScheduleEditor { schedule_id });
        }
        if self.primary_tab == PrimaryTab::Schedules {
            self.message = Some(Message {
                text: "No schedule is selected".into(),
                error: false,
            });
            return None;
        }
        let target = if self.primary_tab != PrimaryTab::Workspaces {
            self.selected_item_location().and_then(|(workspace, item)| {
                let workspace = &self.workspaces[workspace];
                workspace
                    .item_actionable(item)
                    .then(|| workspace.items.get(item))
                    .flatten()
                    .filter(|item| item.ordinary_visible())
                    .and_then(|value| item_rename_target(workspace, item, value))
            })
        } else {
            match self.focus {
                Focus::Workspaces => self
                    .selected()
                    .filter(|workspace| workspace.actionable())
                    .and_then(|workspace| match workspace.coordination {
                        WorkspaceCoordinationView::Global {
                            revision,
                            closing: false,
                            ..
                        } => Some(RenameTarget::GlobalWorkspace {
                            workspace_id: workspace.id.clone(),
                            expected_revision: revision,
                        }),
                        WorkspaceCoordinationView::External {
                            available: true, ..
                        } => Some(RenameTarget::Workspace(workspace.qualify(&workspace.id))),
                        WorkspaceCoordinationView::Global { .. }
                        | WorkspaceCoordinationView::External { .. } => None,
                    }),
                Focus::Items => self.selected_item_location().and_then(|(workspace, item)| {
                    let workspace = &self.workspaces[workspace];
                    workspace
                        .item_actionable(item)
                        .then(|| workspace.items.get(item))
                        .flatten()
                        .filter(|item| item.ordinary_visible())
                        .and_then(|value| item_rename_target(workspace, item, value))
                }),
            }
        };
        if let Some(target) = target {
            self.mode = Mode::Rename {
                target,
                input: String::new(),
            };
            self.message = None;
        }
        None
    }

    fn request_add(&mut self) -> Option<DashboardEffect> {
        if self.primary_tab == PrimaryTab::Nodes {
            return Some(DashboardEffect::AddNode);
        }
        if self.primary_tab == PrimaryTab::Schedules {
            self.message = Some(Message {
                text: "Create schedules with `boomux schedule create --help` (new schedules are paused by default)".into(),
                error: false,
            });
            return None;
        }
        if self.primary_tab != PrimaryTab::Workspaces {
            return None;
        }
        match self.focus {
            Focus::Workspaces => {
                self.mode = Mode::PickProject(ProjectPicker::new(&self.project_context));
                self.message = None;
                None
            }
            Focus::Items => {
                if self.pending_shell_creation.is_some() {
                    return None;
                }
                let workspace = self.selected()?.clone();
                if let WorkspaceCoordinationView::Global {
                    revision,
                    placements,
                    ..
                } = &workspace.coordination
                {
                    let eligible = self
                        .nodes
                        .iter()
                        .filter(|node| node.workspace_owner_eligible)
                        .cloned()
                        .collect::<Vec<_>>();
                    if let [node] = eligible.as_slice() {
                        self.begin_shell_creation(&workspace.name, &node.alias);
                        return Some(global_shell_effect(&workspace, *revision, placements, node));
                    }
                    self.mode = Mode::SelectWorkspaceNode(WorkspaceNodePicker {
                        workspace_id: workspace.id.clone(),
                        workspace_name: workspace.name.clone(),
                        expected_revision: *revision,
                        placements: placements.clone(),
                        nodes: self.nodes.clone(),
                        selected: None,
                    });
                    self.message = None;
                    return None;
                }
                let effect = workspace
                    .local_actionable()
                    .then(|| DashboardEffect::CreateShell(workspace.qualify(&workspace.id)));
                if effect.is_some() {
                    self.begin_shell_creation(&workspace.name, &workspace.node.alias);
                }
                effect
            }
        }
    }

    fn begin_shell_creation(&mut self, workspace_name: &str, node_alias: &str) {
        self.message = None;
        self.pending_shell_creation = Some(format!(
            "Creating Shell in {workspace_name} on Node {node_alias}..."
        ));
    }

    fn adopt_selected_external(&self) -> Option<DashboardEffect> {
        let workspace = self.selected()?;
        let WorkspaceCoordinationView::External {
            owner_revision,
            available,
        } = workspace.coordination
        else {
            return None;
        };
        (available && workspace.node.workspace_owner_eligible).then(|| {
            DashboardEffect::AdoptExternalWorkspace {
                identity: workspace.qualify(&workspace.id),
                expected_revision: owner_revision,
            }
        })
    }

    fn link_selected_external(&mut self) {
        let Some(workspace) = self.selected().cloned() else {
            return;
        };
        let WorkspaceCoordinationView::External {
            owner_revision,
            available,
        } = workspace.coordination
        else {
            return;
        };
        if !available || !workspace.node.workspace_owner_eligible {
            self.message = Some(Message {
                text: workspace
                    .node
                    .workspace_owner_unavailable_reason
                    .clone()
                    .unwrap_or_else(|| "Unavailable external Workspaces cannot be linked".into()),
                error: true,
            });
            return;
        }
        let workspaces = self
            .workspaces
            .iter()
            .filter_map(|candidate| match candidate.coordination {
                WorkspaceCoordinationView::Global {
                    revision,
                    closing: false,
                    ..
                } => Some((candidate.id.clone(), candidate.name.clone(), revision)),
                WorkspaceCoordinationView::Global { .. }
                | WorkspaceCoordinationView::External { .. } => None,
            })
            .collect::<Vec<_>>();
        self.mode = Mode::LinkWorkspace(LinkWorkspacePicker {
            identity: workspace.qualify(&workspace.id),
            expected_owner_revision: owner_revision,
            workspaces,
            selected: None,
        });
        self.message = None;
    }

    fn create_workspace(&mut self, name: &str) -> Option<DashboardEffect> {
        self.mode = Mode::Normal;
        Some(DashboardEffect::CreateWorkspace {
            name: name.to_owned(),
        })
    }

    fn rename(&mut self, target: RenameTarget, name: String) -> DashboardEffect {
        self.mode = Mode::Normal;
        DashboardEffect::Rename { target, name }
    }

    fn restore_selected(&self) -> Option<DashboardEffect> {
        if self.primary_tab != PrimaryTab::Workspaces {
            return None;
        }
        let workspace = self.selected()?;
        match workspace.coordination {
            WorkspaceCoordinationView::Global {
                revision,
                closing: false,
                ..
            } => Some(DashboardEffect::OpenGlobalWorkspace {
                workspace_id: workspace.id.clone(),
                expected_revision: revision,
            }),
            WorkspaceCoordinationView::Global { closing: true, .. } => {
                Some(DashboardEffect::RetryGlobalWorkspaceClose {
                    workspace_id: workspace.id.clone(),
                })
            }
            WorkspaceCoordinationView::External {
                available: true, ..
            } if workspace.local_actionable() => Some(DashboardEffect::RestoreWorkspace(
                workspace.qualify(&workspace.id),
            )),
            WorkspaceCoordinationView::External { .. } => None,
        }
    }

    fn open_selected_item(&self) -> Option<DashboardEffect> {
        if matches!(self.selected_item(), Some(WorkspaceItemView::AgentShell(agent)) if agent.schedule_id.is_some())
        {
            return None;
        }
        let (workspace_index, item_index) = self.selected_item_location()?;
        let workspace = &self.workspaces[workspace_index];
        let workspace_id = workspace.qualify_item_workspace(item_index);
        let target = workspace
            .items
            .get(item_index)
            .and_then(|item| match item {
                WorkspaceItemView::Shell(shell) => workspace
                    .item_shell_attachable(item_index)
                    .then(|| OpenTarget::Shell(workspace.qualify_item(item_index, &shell.id))),
                WorkspaceItemView::AgentShell(agent_shell) => {
                    workspace.item_shell_attachable(item_index).then(|| {
                        OpenTarget::Shell(workspace.qualify_item(item_index, &agent_shell.shell.id))
                    })
                }
                WorkspaceItemView::Launcher(launcher) => workspace
                    .item_launcher_invokable(item_index)
                    .then(|| OpenTarget::Launcher {
                        workspace_id,
                        launcher_id: workspace.qualify_item(item_index, &launcher.id),
                    }),
                WorkspaceItemView::Schedule(_) => None,
            })?;
        Some(DashboardEffect::Open(target))
    }

    fn activate_selected_item(&mut self) -> Option<DashboardEffect> {
        let schedule_id = match self.selected_item() {
            Some(WorkspaceItemView::Schedule(schedule)) => {
                self.selected_item_location().map(|(workspace, item)| {
                    self.workspaces[workspace].qualify_item(item, &schedule.id)
                })
            }
            _ => None,
        };
        if let Some(schedule_id) = schedule_id {
            if self.select_schedule_id(&schedule_id) {
                return self.selected_schedule_history_effect();
            }
            return None;
        }
        self.open_selected_item()
    }

    fn request_close(&mut self) {
        if self.primary_tab == PrimaryTab::Nodes {
            if let Some(node) = self
                .node_state
                .selected()
                .and_then(|index| self.nodes.get(index))
                .filter(|node| !node.local)
                .cloned()
            {
                self.mode = Mode::ConfirmForgetNode(node);
            }
            return;
        }
        if self.primary_tab == PrimaryTab::Schedules {
            self.pending_close = self
                .selected_schedule()
                .filter(|schedule| schedule.actionable)
                .map(|schedule| PendingClose {
                    target: CloseTarget::Schedule(schedule.qualify(&schedule.id)),
                    name: schedule.name.clone(),
                    shell_count: 0,
                    launcher_count: 0,
                });
            return;
        }
        self.pending_close = if self.primary_tab != PrimaryTab::Workspaces {
            self.selected_item_location().and_then(|(workspace, item)| {
                let workspace = &self.workspaces[workspace];
                workspace
                    .items
                    .get(item)
                    .filter(|item| item.ordinary_visible())
                    .and_then(|value| {
                        item_pending_removal(
                            workspace,
                            item,
                            value,
                            self.cached_projection_dismissal,
                        )
                    })
            })
        } else {
            match self.focus {
                Focus::Workspaces => self
                    .selected()
                    .filter(|workspace| workspace.actionable())
                    .and_then(|workspace| {
                        let target = match workspace.coordination {
                            WorkspaceCoordinationView::Global {
                                revision,
                                closing: false,
                                ..
                            } => CloseTarget::GlobalWorkspace {
                                workspace_id: workspace.id.clone(),
                                expected_revision: revision,
                            },
                            WorkspaceCoordinationView::External {
                                available: true, ..
                            } => CloseTarget::Workspace(workspace.qualify(&workspace.id)),
                            WorkspaceCoordinationView::Global { .. }
                            | WorkspaceCoordinationView::External { .. } => return None,
                        };
                        Some(PendingClose {
                            target,
                            name: workspace.name.clone(),
                            shell_count: workspace.process_count(),
                            launcher_count: workspace.launcher_count(),
                        })
                    }),
                Focus::Items => self.selected_item_location().and_then(|(workspace, item)| {
                    let workspace = &self.workspaces[workspace];
                    workspace
                        .items
                        .get(item)
                        .filter(|item| item.ordinary_visible())
                        .and_then(|value| {
                            item_pending_removal(
                                workspace,
                                item,
                                value,
                                self.cached_projection_dismissal,
                            )
                        })
                }),
            }
        };
    }

    fn inspect_selected_node(&mut self) {
        if let Some(node) = self
            .node_state
            .selected()
            .and_then(|index| self.nodes.get(index))
            .cloned()
        {
            self.mode = Mode::InspectNode(node);
        }
    }

    fn retarget_selected_node(&mut self) {
        if let Some(node) = self
            .node_state
            .selected()
            .and_then(|index| self.nodes.get(index))
            .filter(|node| !node.local)
            && let Some(expected_revision) = node.registration_revision
        {
            self.mode = Mode::RetargetNode {
                node_id: node.id.clone(),
                expected_revision,
                input: node.route.clone().unwrap_or_default(),
            };
        }
    }

    fn cancel_close(&mut self) {
        self.pending_close = None;
    }

    fn confirm_close(&mut self) -> Option<DashboardEffect> {
        let pending = self.pending_close.take()?;
        match pending.target {
            CloseTarget::Schedule(id) => Some(DashboardEffect::RemoveSchedule(id)),
            CloseTarget::Execution(id) => Some(DashboardEffect::CancelExecution(id)),
            target => Some(DashboardEffect::Close(target)),
        }
    }

    fn request_cancel_execution(&mut self, execution_id: QualifiedIdentity, label: String) {
        self.pending_close = Some(PendingClose {
            target: CloseTarget::Execution(execution_id),
            name: label,
            shell_count: 0,
            launcher_count: 0,
        });
    }

    fn select_workspace_as_default(&self) -> Option<DashboardEffect> {
        if self.primary_tab != PrimaryTab::Workspaces || self.focus != Focus::Workspaces {
            return None;
        }
        let workspace = self.selected()?;
        if !matches!(
            workspace.coordination,
            WorkspaceCoordinationView::Global { closing: false, .. }
        ) || self.selected_workspace_id.as_deref() == Some(&workspace.id)
        {
            return None;
        }
        Some(DashboardEffect::SelectWorkspace {
            workspace_id: workspace.id.clone(),
        })
    }

    fn update(&mut self, event: DashboardEvent) -> Vec<DashboardEffect> {
        match event {
            DashboardEvent::KeyPressed { code, modifiers } => {
                return self.update_key(code, modifiers).into_iter().collect();
            }
            DashboardEvent::RefreshElapsed => return vec![DashboardEffect::CheckForUpdates],
            DashboardEvent::PreviewRequested => {
                return self.terminal_preview_effect().into_iter().collect();
            }
            DashboardEvent::UpdateCheckCompleted => {}
            DashboardEvent::OperationCompleted(result) => {
                self.message = Some(Message::from_result(result));
                return vec![DashboardEffect::Refresh];
            }
            DashboardEvent::WorkspaceSelectionCompleted {
                workspace_id,
                result,
            } => {
                if result.is_ok() {
                    self.selected_workspace_id = Some(workspace_id);
                }
                self.message = Some(Message::from_result(result));
            }
            DashboardEvent::ShellCreationCompleted(result) => {
                self.pending_shell_creation = None;
                self.message = Some(Message::from_result(result));
                return vec![DashboardEffect::Refresh];
            }
            DashboardEvent::RefreshCompleted(result) => {
                let succeeded = result.is_ok();
                self.apply_refresh(result);
                if succeeded {
                    return self
                        .selected_schedule_history_effect()
                        .into_iter()
                        .collect();
                }
            }
            DashboardEvent::ScheduleHistoryCompleted {
                schedule_id,
                result,
            } => match result {
                Ok((executions, truncated)) => {
                    if let Some(schedule) = self.schedules.iter_mut().find(|schedule| {
                        schedule.node_id == schedule_id.node_id
                            && schedule.id == schedule_id.inner_id
                    }) {
                        schedule.executions = executions;
                        schedule.history_truncated = truncated;
                        schedule.history_scoped = true;
                        schedule.history_complete = !truncated;
                        self.sync_selected_execution();
                    }
                }
                Err(text) => self.message = Some(Message { text, error: true }),
            },
            DashboardEvent::ScheduleEditorLoaded {
                schedule_id,
                result,
            } => match result {
                Ok(inspection) if inspection.schedule_id == schedule_id && inspection.paused => {
                    let cursor = inspection.name.len();
                    self.mode = Mode::EditSchedule(ScheduleEditor {
                        schedule_id: inspection.schedule_id,
                        expected_revision: inspection.revision,
                        field: ScheduleEditorField::Name,
                        preset: ScheduleTriggerPreset::from_cron(&inspection.cron),
                        name: inspection.name,
                        cron: inspection.cron,
                        timezone: inspection.timezone,
                        timezone_query: String::new(),
                        prompt: inspection.prompt,
                        cursor,
                        error: None,
                    });
                    self.message = None;
                }
                Ok(_) => {
                    self.message = Some(Message {
                        text: "Pause the schedule with p before editing".into(),
                        error: false,
                    });
                }
                Err(text) => self.message = Some(Message { text, error: true }),
            },
            DashboardEvent::ScheduleEditorSaved {
                schedule_id,
                result,
            } => match result {
                Ok(text) if matches!(&self.mode, Mode::EditSchedule(editor) if editor.schedule_id == schedule_id) =>
                {
                    self.mode = Mode::Normal;
                    self.message = Some(Message { text, error: false });
                    return vec![DashboardEffect::Refresh];
                }
                Ok(_) => {}
                Err(text) => {
                    if let Mode::EditSchedule(editor) = &mut self.mode
                        && editor.schedule_id == schedule_id
                    {
                        editor.error = Some(text);
                    }
                }
            },
            DashboardEvent::TextPasted(text) => {
                if let Mode::EditSchedule(editor) = &mut self.mode {
                    editor.insert_text(&text);
                }
            }
            DashboardEvent::TerminalPreviewCompleted {
                shell_id,
                run_id,
                output_revision,
                output,
            } => self.apply_terminal_preview(shell_id, run_id, output_revision, output),
        }
        Vec::new()
    }

    fn update_key(&mut self, code: KeyCode, modifiers: KeyModifiers) -> Option<DashboardEffect> {
        if modifiers.contains(KeyModifiers::CONTROL) && code == KeyCode::Char('c') {
            return Some(DashboardEffect::Quit);
        }
        if self.pending_close.is_some() {
            if !modifiers.is_empty() {
                return None;
            }
            return match code {
                KeyCode::Char('y') => self.confirm_close(),
                KeyCode::Char('n') | KeyCode::Esc => {
                    self.cancel_close();
                    None
                }
                _ => None,
            };
        }
        if matches!(self.mode, Mode::Help) {
            handle_help_key(self, code, modifiers);
            return None;
        }
        if matches!(self.mode, Mode::Palette(_)) {
            return handle_palette_key(self, code, modifiers)
                .and_then(|command| execute_palette_command(self, command));
        }
        if !matches!(self.mode, Mode::Normal) {
            return handle_mode_key(self, code, modifiers);
        }
        if !normal_mode_modifiers_supported(code, modifiers) {
            return None;
        }
        match code {
            KeyCode::Char('q') | KeyCode::Esc => return Some(DashboardEffect::Quit),
            KeyCode::Down | KeyCode::Char('j') => {
                self.next();
                if self.primary_tab == PrimaryTab::Schedules && self.focus == Focus::Workspaces {
                    return self.selected_schedule_history_effect();
                }
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.previous();
                if self.primary_tab == PrimaryTab::Schedules && self.focus == Focus::Workspaces {
                    return self.selected_schedule_history_effect();
                }
            }
            KeyCode::PageUp => self.scroll_terminal_preview_up(),
            KeyCode::PageDown => self.scroll_terminal_preview_down(),
            KeyCode::Home => self.scroll_terminal_preview_to_start(),
            KeyCode::End => self.scroll_terminal_preview_to_end(),
            KeyCode::Enter => {
                if self.primary_tab == PrimaryTab::Nodes {
                    self.inspect_selected_node();
                    return None;
                }
                if self.primary_tab == PrimaryTab::Schedules {
                    return self.open_selected_schedule_link();
                }
                return if self.primary_tab != PrimaryTab::Workspaces {
                    self.open_selected_item()
                } else {
                    match self.focus {
                        Focus::Workspaces => self.restore_selected(),
                        Focus::Items => self.activate_selected_item(),
                    }
                };
            }
            KeyCode::Char('r') if self.primary_tab == PrimaryTab::Nodes => {
                return self
                    .node_state
                    .selected()
                    .and_then(|index| self.nodes.get(index))
                    .filter(|node| !node.local)
                    .map(|node| DashboardEffect::RefreshNode(node.id.clone()))
                    .or(Some(DashboardEffect::Refresh));
            }
            KeyCode::Char('r') => return Some(DashboardEffect::Refresh),
            KeyCode::Char('u') if self.primary_tab == PrimaryTab::Nodes => {
                if !self.cached_projection_dismissal {
                    return None;
                }
                return self
                    .node_state
                    .selected()
                    .and_then(|index| self.nodes.get(index))
                    .filter(|node| !node.local)
                    .map(|node| DashboardEffect::RestoreDismissedShells(node.id.clone()));
            }
            KeyCode::Char('[') if self.primary_tab == PrimaryTab::Schedules => {
                self.cycle_execution(false);
            }
            KeyCode::Char(']') if self.primary_tab == PrimaryTab::Schedules => {
                self.cycle_execution(true);
            }
            KeyCode::Char('u') if self.primary_tab == PrimaryTab::Schedules => {
                return self
                    .selected_schedule()
                    .filter(|schedule| schedule.actionable)
                    .map(|schedule| DashboardEffect::RunSchedule(schedule.qualify(&schedule.id)));
            }
            KeyCode::Char('U') if self.primary_tab == PrimaryTab::Nodes => {
                return self
                    .node_state
                    .selected()
                    .and_then(|index| self.nodes.get(index))
                    .filter(|node| !node.local)
                    .map(|node| DashboardEffect::UpgradeNode(node.id.clone()));
            }
            KeyCode::Char('p') if self.primary_tab == PrimaryTab::Schedules => {
                return self
                    .selected_schedule()
                    .filter(|schedule| schedule.actionable)
                    .map(|schedule| match schedule.state {
                        ScheduleDisplayState::Paused => {
                            DashboardEffect::ResumeSchedule(schedule.qualify(&schedule.id))
                        }
                        ScheduleDisplayState::Enabled => {
                            DashboardEffect::PauseSchedule(schedule.qualify(&schedule.id))
                        }
                    });
            }
            KeyCode::Char('c') if self.primary_tab == PrimaryTab::Schedules => {
                if let Some(execution) = self
                    .selected_execution()
                    .filter(|execution| execution.state.is_active())
                    .cloned()
                {
                    let node_id = self
                        .selected_schedule()
                        .map(|schedule| schedule.node_id.clone())?;
                    self.request_cancel_execution(
                        QualifiedIdentity::new(node_id, &execution.id),
                        format!("execution {}", short_id(&execution.id)),
                    );
                }
                return None;
            }
            KeyCode::Char('x') => self.request_close(),
            KeyCode::Char('a') => return self.request_add(),
            KeyCode::Char('s') => return self.select_workspace_as_default(),
            KeyCode::Char('d') if self.primary_tab == PrimaryTab::Workspaces => {
                return self.adopt_selected_external();
            }
            KeyCode::Char('L') if self.primary_tab == PrimaryTab::Workspaces => {
                self.link_selected_external();
            }
            KeyCode::Char('e') => return self.request_rename(),
            KeyCode::Char('t') if self.primary_tab == PrimaryTab::Nodes => {
                self.retarget_selected_node();
            }
            KeyCode::Char(' ') => self.toggle_selection_pin(),
            KeyCode::Char('/' | ':') => self.open_palette(),
            KeyCode::Char('?') => self.mode = Mode::Help,
            KeyCode::Tab => {
                self.cycle_tab(false);
                return self.selected_schedule_history_effect();
            }
            KeyCode::BackTab => {
                self.cycle_tab(true);
                return self.selected_schedule_history_effect();
            }
            KeyCode::Char(key) if shortcut_tab(key).is_some() => {
                self.select_tab(shortcut_tab(key).expect("validated tab shortcut"));
                return self.selected_schedule_history_effect();
            }
            key if self.handle_focus_key(key) => {}
            _ => {}
        }
        None
    }

    fn apply_refresh(&mut self, result: Result<DashboardState, String>) {
        match result {
            Ok(state) => {
                self.nodes = state.nodes;
                self.all_workspaces = state.workspaces;
                self.all_schedules = state.schedules;
                let workspaces = self.all_workspaces.clone();
                let schedules = self.all_schedules.clone();
                self.node_state.select(
                    (!self.nodes.is_empty()).then_some(
                        self.node_state
                            .selected()
                            .unwrap_or(0)
                            .min(self.nodes.len().saturating_sub(1)),
                    ),
                );
                self.replace_workspaces(workspaces);
                self.replace_schedules(schedules);
                self.scheduling = state.scheduling;
                self.exact_run_attachment = state.exact_run_attachment;
                self.schedule_editing = state.schedule_editing;
                if state.reset_focus_revision {
                    self.observed_focus_revision = None;
                }
                self.apply_focused_terminal(state.focused_terminal.as_ref());
            }
            Err(text) => self.message = Some(Message { text, error: true }),
        }
    }

    fn terminal_preview_effect(&mut self) -> Option<DashboardEffect> {
        let selected = if self.primary_tab == PrimaryTab::Workspaces && self.focus != Focus::Items {
            None
        } else {
            let (workspace_index, item_index) = self.selected_item_location()?;
            let workspace = &self.workspaces[workspace_index];
            if !workspace.item_actionable(item_index) {
                return None;
            }
            workspace.items.get(item_index).and_then(|item| match item {
                WorkspaceItemView::Shell(shell) if shell.kind == TerminalKind::Shell => Some((
                    workspace.qualify_item(item_index, &shell.id),
                    shell.run.as_ref().map(|run| run.id.clone()),
                    shell.run.as_ref().map_or(0, |run| run.output_revision),
                )),
                WorkspaceItemView::Shell(_)
                | WorkspaceItemView::AgentShell(_)
                | WorkspaceItemView::Launcher(_)
                | WorkspaceItemView::Schedule(_) => None,
            })
        };
        let Some((shell_id, run_id, output_revision)) = selected else {
            self.terminal_preview = None;
            return None;
        };
        if self.terminal_preview.as_ref().is_some_and(|preview| {
            preview.shell_id == shell_id.inner_id
                && preview.run_id == run_id
                && preview.output_revision == output_revision
        }) {
            return None;
        }
        Some(DashboardEffect::ReadTerminalPreview {
            shell_id,
            run_id,
            output_revision,
        })
    }

    fn apply_terminal_preview(
        &mut self,
        shell_id: String,
        run_id: Option<String>,
        output_revision: u64,
        output: Result<TerminalPreview, String>,
    ) {
        let scroll_from_bottom = self
            .terminal_preview
            .as_ref()
            .filter(|preview| {
                preview.shell_id == shell_id
                    && preview.run_id == run_id
                    && preview.scroll_from_bottom > 0
            })
            .map_or(0, |preview| {
                let previous_count = preview
                    .output
                    .as_ref()
                    .ok()
                    .map_or(0, |output| terminal_output_lines(output).len());
                let next_count = output
                    .as_ref()
                    .ok()
                    .map_or(0, |output| terminal_output_lines(output).len());
                preview
                    .scroll_from_bottom
                    .saturating_add(next_count.saturating_sub(previous_count))
            });
        self.terminal_preview = Some(TerminalPreviewState {
            output,
            shell_id,
            run_id,
            output_revision,
            scroll_from_bottom,
        });
    }

    fn scroll_terminal_preview_up(&mut self) {
        let Some(preview) = self.terminal_preview.as_mut() else {
            return;
        };
        let line_count = preview
            .output
            .as_ref()
            .ok()
            .map_or(0, |output| terminal_output_lines(output).len());
        let max_scroll = line_count.saturating_sub(TERMINAL_PREVIEW_ROWS);
        preview.scroll_from_bottom = preview
            .scroll_from_bottom
            .saturating_add(TERMINAL_PREVIEW_SCROLL_STEP)
            .min(max_scroll);
    }

    fn scroll_terminal_preview_down(&mut self) {
        let Some(preview) = self.terminal_preview.as_mut() else {
            return;
        };
        preview.scroll_from_bottom = preview
            .scroll_from_bottom
            .saturating_sub(TERMINAL_PREVIEW_SCROLL_STEP);
    }

    fn scroll_terminal_preview_to_start(&mut self) {
        let Some(preview) = self.terminal_preview.as_mut() else {
            return;
        };
        let line_count = preview
            .output
            .as_ref()
            .ok()
            .map_or(0, |output| terminal_output_lines(output).len());
        preview.scroll_from_bottom = line_count.saturating_sub(TERMINAL_PREVIEW_ROWS);
    }

    fn scroll_terminal_preview_to_end(&mut self) {
        if let Some(preview) = self.terminal_preview.as_mut() {
            preview.scroll_from_bottom = 0;
        }
    }

    fn terminal_preview_is_available(&self) -> bool {
        self.terminal_preview
            .as_ref()
            .is_some_and(|preview| preview.output.is_ok())
    }

    fn replace_workspaces(&mut self, workspaces: Vec<WorkspaceView>) {
        let selected_id = self
            .selected()
            .map(|workspace| workspace.qualify(&workspace.id));
        let selected_item = self.workspace_item_identity();
        let selected_global_item = self.global_item_identity();
        let previous_index = self.selected_index().unwrap_or(0);
        let selected_index = selected_id
            .and_then(|id| {
                workspaces.iter().position(|workspace| {
                    workspace.node.id == id.node_id && workspace.id == id.inner_id
                })
            })
            .or_else(|| (!workspaces.is_empty()).then(|| previous_index.min(workspaces.len() - 1)));

        self.workspaces = workspaces;
        self.workspace_state.select(selected_index);
        let item_index = self.selected().and_then(|workspace| {
            selected_item
                .and_then(|target| {
                    workspace
                        .items
                        .iter()
                        .filter(|item| item.ordinary_visible())
                        .position(|item| item_matches(item, &target))
                })
                .or_else(|| (workspace.ordinary_item_count() > 0).then_some(0))
        });
        self.item_state.select(item_index);
        if self.primary_tab != PrimaryTab::Workspaces && self.primary_tab != PrimaryTab::Schedules {
            let global_index = selected_global_item
                .and_then(|target| self.global_item_position(&target))
                .or_else(|| (self.global_item_count() > 0).then_some(0));
            self.global_state.select(global_index);
        }
        if self.workspaces.is_empty() {
            self.focus = Focus::Workspaces;
        }
    }

    fn replace_schedules(&mut self, schedules: Vec<ScheduleView>) {
        let selected = self
            .selected_schedule()
            .map(|schedule| schedule.qualify(&schedule.id));
        let previous = self.global_state.selected().unwrap_or(0);
        self.schedules = schedules;
        if self.primary_tab == PrimaryTab::Schedules {
            self.global_state.select(
                selected
                    .and_then(|id| {
                        self.schedules.iter().position(|schedule| {
                            schedule.node_id == id.node_id && schedule.id == id.inner_id
                        })
                    })
                    .or_else(|| {
                        (!self.schedules.is_empty()).then(|| previous.min(self.schedules.len() - 1))
                    }),
            );
            self.sync_selected_execution();
        }
    }

    fn open_selected_schedule_link(&mut self) -> Option<DashboardEffect> {
        let node_id = self
            .selected_schedule()
            .filter(|schedule| schedule.actionable)
            .map(|schedule| schedule.node_id.clone())?;
        let execution = self.selected_execution()?.clone();
        if execution.state.is_active() && !self.exact_run_attachment {
            self.message = Some(Message {
                text: "Opening exact Scheduled Execution runs requires daemon protocol 26; upgrade and restart Boomux"
                    .into(),
                error: false,
            });
            return None;
        }
        Some(DashboardEffect::OpenScheduledExecution {
            execution_id: QualifiedIdentity::new(node_id, execution.id),
        })
    }

    fn select_agent_id(&mut self, agent_id: &QualifiedIdentity) -> bool {
        let Some((workspace_index, item_index)) = self.workspaces.iter().enumerate().find_map(|(workspace_index, workspace)| {
            workspace.items.iter().enumerate().find_map(|(item_index, item)| {
                (workspace.item_owner(item_index).0.id == agent_id.node_id
                    && matches!(item, WorkspaceItemView::AgentShell(agent) if agent.agent.as_ref().is_some_and(|agent| agent.id == agent_id.inner_id)))
                    .then_some((workspace_index, item_index))
            })
        }) else {
            return false;
        };
        self.select_tab(PrimaryTab::Agents);
        let identity = item_identity(
            &self.workspaces[workspace_index],
            item_index,
            &self.workspaces[workspace_index].items[item_index],
        );
        self.global_state
            .select(self.global_item_position(&identity));
        true
    }

    fn workspace_item_identity(&self) -> Option<ItemIdentity> {
        let (workspace_index, item_index) = self.selected_item_location()?;
        let workspace = self.workspaces.get(workspace_index)?;
        Some(item_identity(
            workspace,
            item_index,
            workspace.items.get(item_index)?,
        ))
    }

    fn global_item_identity(&self) -> Option<ItemIdentity> {
        if self.primary_tab == PrimaryTab::Workspaces {
            return None;
        }
        let (workspace, item) = self.selected_item_location()?;
        Some(item_identity(
            &self.workspaces[workspace],
            item,
            &self.workspaces[workspace].items[item],
        ))
    }

    fn global_item_position(&self, identity: &ItemIdentity) -> Option<usize> {
        (0..self.global_item_count()).position(|ordinal| {
            self.global_item_location(ordinal)
                .is_some_and(|(workspace, item)| {
                    let workspace = &self.workspaces[workspace];
                    let owner = workspace.item_owner(item);
                    item_matches(&workspace.items[item], identity)
                        && owner.0.id == identity.workspace_id.node_id
                        && owner.1 == identity.workspace_id.inner_id
                })
        })
    }
}

fn item_identity(
    workspace: &WorkspaceView,
    item_index: usize,
    item: &WorkspaceItemView,
) -> ItemIdentity {
    let (item_id, kind) = match item {
        WorkspaceItemView::Shell(shell) => (shell.id.clone(), ItemIdentityKind::Shell),
        WorkspaceItemView::AgentShell(agent) => (agent.shell.id.clone(), ItemIdentityKind::Shell),
        WorkspaceItemView::Launcher(launcher) => (launcher.id.clone(), ItemIdentityKind::Launcher),
        WorkspaceItemView::Schedule(schedule) => (schedule.id.clone(), ItemIdentityKind::Schedule),
    };
    ItemIdentity {
        workspace_id: workspace.qualify_item_workspace(item_index),
        item_id: workspace.qualify_item(item_index, item_id),
        kind,
    }
}

fn global_shell_effect(
    workspace: &WorkspaceView,
    expected_revision: u64,
    placements: &[WorkspacePlacementView],
    node: &NodeView,
) -> DashboardEffect {
    let placement = placements
        .iter()
        .find(|placement| placement.node.id == node.id);
    DashboardEffect::CreateGlobalShell {
        workspace_id: workspace.id.clone(),
        expected_revision,
        node_id: node.id.clone(),
        owner_workspace_id: placement.map_or_else(
            || uuid::Uuid::new_v4().to_string(),
            |placement| placement.workspace_id.clone(),
        ),
        default_cwd: placement
            .and_then(|placement| placement.default_cwd.as_deref())
            .map(PathBuf::from),
    }
}

fn item_matches(item: &WorkspaceItemView, identity: &ItemIdentity) -> bool {
    match item {
        WorkspaceItemView::Shell(shell) => {
            identity.kind == ItemIdentityKind::Shell && shell.id == identity.item_id.inner_id
        }
        WorkspaceItemView::AgentShell(agent) => {
            identity.kind == ItemIdentityKind::Shell && agent.shell.id == identity.item_id.inner_id
        }
        WorkspaceItemView::Launcher(launcher) => {
            identity.kind == ItemIdentityKind::Launcher && launcher.id == identity.item_id.inner_id
        }
        WorkspaceItemView::Schedule(schedule) => {
            identity.kind == ItemIdentityKind::Schedule && schedule.id == identity.item_id.inner_id
        }
    }
}

fn item_rename_target(
    workspace: &WorkspaceView,
    item_index: usize,
    item: &WorkspaceItemView,
) -> Option<RenameTarget> {
    match item {
        WorkspaceItemView::Shell(shell) => Some(RenameTarget::Shell(
            workspace.qualify_item(item_index, &shell.id),
        )),
        WorkspaceItemView::AgentShell(agent) => Some(RenameTarget::Shell(
            workspace.qualify_item(item_index, &agent.shell.id),
        )),
        WorkspaceItemView::Launcher(launcher) => Some(RenameTarget::Launcher(
            workspace.qualify_item(item_index, &launcher.id),
        )),
        WorkspaceItemView::Schedule(_) => None,
    }
}

fn item_pending_close(
    workspace: &WorkspaceView,
    item_index: usize,
    item: &WorkspaceItemView,
) -> PendingClose {
    match item {
        WorkspaceItemView::Shell(shell) => PendingClose {
            target: CloseTarget::Shell(workspace.qualify_item(item_index, &shell.id)),
            name: shell.name.clone(),
            shell_count: 1,
            launcher_count: 0,
        },
        WorkspaceItemView::AgentShell(agent) => PendingClose {
            target: CloseTarget::Shell(workspace.qualify_item(item_index, &agent.shell.id)),
            name: agent.shell.name.clone(),
            shell_count: 1,
            launcher_count: 0,
        },
        WorkspaceItemView::Launcher(launcher) => PendingClose {
            target: CloseTarget::Launcher(workspace.qualify_item(item_index, &launcher.id)),
            name: launcher.name.clone(),
            shell_count: 0,
            launcher_count: 1,
        },
        WorkspaceItemView::Schedule(schedule) => PendingClose {
            target: CloseTarget::Schedule(workspace.qualify_item(item_index, &schedule.id)),
            name: schedule.name.clone(),
            shell_count: 0,
            launcher_count: 0,
        },
    }
}

fn item_pending_removal(
    workspace: &WorkspaceView,
    item_index: usize,
    item: &WorkspaceItemView,
    dismissal_supported: bool,
) -> Option<PendingClose> {
    if workspace.item_actionable(item_index) {
        return Some(item_pending_close(workspace, item_index, item));
    }
    if !dismissal_supported || !workspace.item_dismissible(item_index) {
        return None;
    }
    let (id, name) = match item {
        WorkspaceItemView::Shell(shell) => (&shell.id, &shell.name),
        WorkspaceItemView::AgentShell(agent) => (&agent.shell.id, &agent.shell.name),
        WorkspaceItemView::Launcher(_) | WorkspaceItemView::Schedule(_) => return None,
    };
    Some(PendingClose {
        target: CloseTarget::DismissCachedShell(workspace.qualify_item(item_index, id)),
        name: name.clone(),
        shell_count: 0,
        launcher_count: 0,
    })
}

pub(crate) fn run<B: DashboardBackend + Send + 'static>(
    state: DashboardState,
    selected_workspace_id: Option<String>,
    follow_focused_terminal: bool,
    project_context: ProjectContext,
    play_intro: bool,
    backend: B,
) -> io::Result<()> {
    let mut terminal = ratatui::try_init().map_err(|error| {
        io::Error::new(
            error.kind(),
            format!("dashboard requires an interactive terminal: {error}"),
        )
    })?;
    if let Err(error) = execute!(io::stdout(), EnableBracketedPaste) {
        let _ = ratatui::try_restore();
        return Err(error);
    }
    let mut app = App::new(state.workspaces, project_context);
    app.selected_workspace_id = selected_workspace_id;
    app.nodes = state.nodes;
    app.node_state.select((!app.nodes.is_empty()).then_some(0));
    app.all_schedules = state.schedules.clone();
    app.schedules = state.schedules;
    app.scheduling = state.scheduling;
    app.exact_run_attachment = state.exact_run_attachment;
    app.schedule_editing = state.schedule_editing;
    app.cached_projection_dismissal = state.cached_projection_dismissal;
    if follow_focused_terminal {
        app.enable_focus_following(state.focused_terminal.as_ref());
    }
    let result = if play_intro {
        play_bomb_animation(&mut terminal).and_then(|()| run_loop(&mut terminal, app, backend))
    } else {
        run_loop(&mut terminal, app, backend)
    };
    let paste_result = execute!(io::stdout(), DisableBracketedPaste);
    let restore_result = ratatui::try_restore();
    paste_result?;
    restore_result?;
    result
}

fn play_bomb_animation(terminal: &mut ratatui::DefaultTerminal) -> io::Result<()> {
    let started = Instant::now();
    loop {
        let animation_frame = bomb_animation_frame(started.elapsed());
        if animation_frame == BombAnimationFrame::Finished {
            return Ok(());
        }
        terminal.draw(|frame| render_bomb_animation(frame, animation_frame))?;

        if event::poll(INTRO_POLL_INTERVAL)?
            && matches!(event::read()?, Event::Key(key) if key.kind == KeyEventKind::Press)
        {
            return Ok(());
        }
    }
}

fn run_loop<B>(terminal: &mut ratatui::DefaultTerminal, mut app: App, backend: B) -> io::Result<()>
where
    B: DashboardBackend + Send + 'static,
{
    let mut runtime = DashboardRuntime::spawn(backend);
    let mut last_refresh = Instant::now();
    loop {
        if runtime.drain(&mut app)? {
            return Ok(());
        }
        if last_refresh.elapsed() >= UPDATE_CHECK_INTERVAL {
            let effects = app.update(DashboardEvent::RefreshElapsed);
            if runtime.dispatch(effects)? {
                return Ok(());
            }
            last_refresh = Instant::now();
        }
        let effects = app.update(DashboardEvent::PreviewRequested);
        if runtime.dispatch(effects)? {
            return Ok(());
        }
        terminal.draw(|frame| render(frame, &mut app))?;

        let poll_interval = if runtime.has_in_flight_effect() {
            INTRO_POLL_INTERVAL
        } else {
            UPDATE_CHECK_INTERVAL
        };
        if !event::poll(poll_interval)? {
            continue;
        }
        let effects = match event::read()? {
            Event::Key(key) if key.kind == KeyEventKind::Press => {
                app.update(DashboardEvent::KeyPressed {
                    code: key.code,
                    modifiers: key.modifiers,
                })
            }
            Event::Paste(text) => app.update(DashboardEvent::TextPasted(text)),
            _ => continue,
        };
        if effects.contains(&DashboardEffect::Quit) {
            if runtime.can_quit() {
                return Ok(());
            }
            app.message = Some(Message {
                text: "Wait for the current operation to finish before quitting".into(),
                error: false,
            });
            continue;
        }
        if !effects.is_empty() && runtime.dispatch(effects)? {
            return Ok(());
        }
        last_refresh = Instant::now();
    }
}

fn execute_palette_command(app: &mut App, command: PaletteCommand) -> Option<DashboardEffect> {
    match command {
        PaletteCommand::AddNode => Some(DashboardEffect::AddNode),
        PaletteCommand::CreateWorkspace => {
            app.mode = Mode::PickProject(ProjectPicker::new(&app.project_context));
            None
        }
        PaletteCommand::ShowHelp => {
            app.mode = Mode::Help;
            None
        }
        PaletteCommand::Workspace {
            workspace_id,
            action,
        } => {
            let focus = if matches!(action, WorkspacePaletteAction::AddShell) {
                Focus::Items
            } else {
                Focus::Workspaces
            };
            if !app.select_workspace(&workspace_id, focus) {
                return None;
            }
            match action {
                WorkspacePaletteAction::GoTo => None,
                WorkspacePaletteAction::Restore => app.restore_selected(),
                WorkspacePaletteAction::AddShell => app.request_add(),
                WorkspacePaletteAction::Rename => {
                    app.request_rename();
                    None
                }
                WorkspacePaletteAction::Close => {
                    app.request_close();
                    None
                }
            }
        }
        PaletteCommand::Item { identity, action } => {
            if !app.select_item_identity(&identity) {
                return None;
            }
            match action {
                ItemPaletteAction::GoTo => None,
                ItemPaletteAction::Open => app.open_selected_item(),
                ItemPaletteAction::Rename => {
                    app.request_rename();
                    None
                }
                ItemPaletteAction::Close => {
                    app.request_close();
                    None
                }
            }
        }
        PaletteCommand::Attention {
            workspace_id,
            shell_id,
            agent_id,
        } => {
            if app.select_agent_id(&agent_id) {
                return None;
            }
            let identity = ItemIdentity {
                workspace_id: workspace_id.clone(),
                item_id: shell_id,
                kind: ItemIdentityKind::Shell,
            };
            if !app.select_item_identity(&identity) {
                if app.select_workspace(&workspace_id, Focus::Workspaces) {
                    app.message = Some(Message {
                        text:
                            "attention source shell is no longer retained; selected its workspace"
                                .into(),
                        error: false,
                    });
                } else {
                    app.message = Some(Message {
                        text: "attention source workspace is no longer available".into(),
                        error: true,
                    });
                }
            }
            None
        }
        PaletteCommand::Schedule {
            schedule_id,
            action,
        } => {
            if !app.select_schedule_id(&schedule_id) {
                return None;
            }
            match action {
                SchedulePaletteAction::GoTo => None,
                SchedulePaletteAction::Run => Some(DashboardEffect::RunSchedule(schedule_id)),
                SchedulePaletteAction::PauseResume => {
                    app.selected_schedule()
                        .map(|schedule| match schedule.state {
                            ScheduleDisplayState::Paused => {
                                DashboardEffect::ResumeSchedule(schedule_id)
                            }
                            ScheduleDisplayState::Enabled => {
                                DashboardEffect::PauseSchedule(schedule_id)
                            }
                        })
                }
                SchedulePaletteAction::SelectExecution(execution_id) => {
                    app.select_execution_id(&execution_id);
                    None
                }
                SchedulePaletteAction::Remove => {
                    app.request_close();
                    None
                }
            }
        }
    }
}

fn handle_help_key(app: &mut App, key: KeyCode, modifiers: KeyModifiers) {
    if modifiers.is_empty() && key == KeyCode::Char('/') {
        app.open_palette();
    } else if modifiers.difference(KeyModifiers::SHIFT).is_empty()
        && matches!(key, KeyCode::Esc | KeyCode::Char('?') | KeyCode::Char('q'))
    {
        app.mode = Mode::Normal;
    }
}

fn normal_mode_modifiers_supported(code: KeyCode, modifiers: KeyModifiers) -> bool {
    modifiers.is_empty()
        || (modifiers == KeyModifiers::SHIFT
            && matches!(code, KeyCode::BackTab | KeyCode::Char('?' | ':')))
}

fn handle_palette_key(
    app: &mut App,
    key: KeyCode,
    modifiers: KeyModifiers,
) -> Option<PaletteCommand> {
    let Mode::Palette(mut palette) = std::mem::replace(&mut app.mode, Mode::Normal) else {
        return None;
    };
    if !modifiers.difference(KeyModifiers::SHIFT).is_empty() {
        app.mode = Mode::Palette(palette);
        return None;
    }
    match key {
        KeyCode::Enter => match palette.selected_command() {
            Some(command) => Some(command),
            None => {
                app.mode = Mode::Palette(palette);
                None
            }
        },
        KeyCode::Esc => None,
        KeyCode::Down => {
            palette.next();
            app.mode = Mode::Palette(palette);
            None
        }
        KeyCode::Up => {
            palette.previous();
            app.mode = Mode::Palette(palette);
            None
        }
        KeyCode::Backspace => {
            palette.query.pop();
            palette.update_matches();
            app.mode = Mode::Palette(palette);
            None
        }
        KeyCode::Char(character) => {
            palette.query.push(character);
            palette.update_matches();
            app.mode = Mode::Palette(palette);
            None
        }
        _ => {
            app.mode = Mode::Palette(palette);
            None
        }
    }
}

fn handle_mode_key(
    app: &mut App,
    key: KeyCode,
    modifiers: KeyModifiers,
) -> Option<DashboardEffect> {
    let mode = std::mem::replace(&mut app.mode, Mode::Normal);
    let save = modifiers == KeyModifiers::CONTROL && key == KeyCode::Char('s');
    if !modifiers.difference(KeyModifiers::SHIFT).is_empty() && !save {
        app.mode = mode;
        return None;
    }
    match mode {
        Mode::Normal => None,
        Mode::Palette(_) | Mode::Help => None,
        Mode::PickProject(mut picker) => match key {
            KeyCode::Enter
                if picker.mode == WorkspaceCreationMode::ByName
                    && picker.custom_name().is_some() =>
            {
                let name = picker
                    .custom_name()
                    .expect("nonempty workspace name")
                    .to_owned();
                app.create_workspace(&name)
            }
            KeyCode::Enter if picker.selected().is_some() => {
                let project = picker.selected().expect("selected project").clone();
                app.create_workspace(&project.name)
            }
            KeyCode::Enter => {
                app.mode = Mode::PickProject(picker);
                None
            }
            KeyCode::Esc => None,
            KeyCode::Tab | KeyCode::BackTab => {
                picker.toggle_mode();
                app.mode = Mode::PickProject(picker);
                None
            }
            KeyCode::Down => {
                picker.next();
                app.mode = Mode::PickProject(picker);
                None
            }
            KeyCode::Up => {
                picker.previous();
                app.mode = Mode::PickProject(picker);
                None
            }
            KeyCode::Backspace => {
                picker.query.pop();
                picker.update_matches();
                app.mode = Mode::PickProject(picker);
                None
            }
            KeyCode::Char(character) => {
                picker.query.push(character);
                picker.update_matches();
                app.mode = Mode::PickProject(picker);
                None
            }
            _ => {
                app.mode = Mode::PickProject(picker);
                None
            }
        },
        Mode::SelectWorkspaceNode(mut picker) => match key {
            KeyCode::Enter => match picker.effect() {
                Some(effect) => {
                    let node_alias = picker
                        .selected
                        .and_then(|selected| picker.nodes.get(selected))
                        .map(|node| node.alias.clone())
                        .expect("Shell creation requires a selected Node");
                    app.begin_shell_creation(&picker.workspace_name, &node_alias);
                    Some(effect)
                }
                None => {
                    app.mode = Mode::SelectWorkspaceNode(picker);
                    None
                }
            },
            KeyCode::Down | KeyCode::Char('j') => {
                picker.move_selection(true);
                app.mode = Mode::SelectWorkspaceNode(picker);
                None
            }
            KeyCode::Up | KeyCode::Char('k') => {
                picker.move_selection(false);
                app.mode = Mode::SelectWorkspaceNode(picker);
                None
            }
            KeyCode::Esc => None,
            _ => {
                app.mode = Mode::SelectWorkspaceNode(picker);
                None
            }
        },
        Mode::LinkWorkspace(mut picker) => match key {
            KeyCode::Enter => match picker.effect() {
                Some(effect) => Some(effect),
                None => {
                    app.mode = Mode::LinkWorkspace(picker);
                    None
                }
            },
            KeyCode::Down | KeyCode::Char('j') => {
                picker.move_selection(true);
                app.mode = Mode::LinkWorkspace(picker);
                None
            }
            KeyCode::Up | KeyCode::Char('k') => {
                picker.move_selection(false);
                app.mode = Mode::LinkWorkspace(picker);
                None
            }
            KeyCode::Esc => None,
            _ => {
                app.mode = Mode::LinkWorkspace(picker);
                None
            }
        },
        Mode::InspectNode(node) => match key {
            KeyCode::Esc | KeyCode::Enter => None,
            _ => {
                app.mode = Mode::InspectNode(node);
                None
            }
        },
        Mode::RetargetNode {
            node_id,
            expected_revision,
            mut input,
        } => match key {
            KeyCode::Enter if !input.trim().is_empty() => Some(DashboardEffect::RetargetNode {
                node_id,
                expected_revision,
                route: input.trim().to_owned(),
            }),
            KeyCode::Esc => None,
            KeyCode::Backspace => {
                input.pop();
                app.mode = Mode::RetargetNode {
                    node_id,
                    expected_revision,
                    input,
                };
                None
            }
            KeyCode::Char(character) => {
                input.push(character);
                app.mode = Mode::RetargetNode {
                    node_id,
                    expected_revision,
                    input,
                };
                None
            }
            _ => {
                app.mode = Mode::RetargetNode {
                    node_id,
                    expected_revision,
                    input,
                };
                None
            }
        },
        Mode::ConfirmForgetNode(node) => match key {
            KeyCode::Char('y') => Some(DashboardEffect::ForgetNode { node_id: node.id }),
            KeyCode::Char('n') | KeyCode::Esc => None,
            _ => {
                app.mode = Mode::ConfirmForgetNode(node);
                None
            }
        },
        Mode::Rename { target, mut input } => match key {
            KeyCode::Enter if !input.trim().is_empty() => {
                let name = input.trim().to_owned();
                Some(app.rename(target, name))
            }
            KeyCode::Esc => None,
            KeyCode::Backspace => {
                input.pop();
                app.mode = Mode::Rename { target, input };
                None
            }
            KeyCode::Char(character) => {
                input.push(character);
                app.mode = Mode::Rename { target, input };
                None
            }
            _ => {
                app.mode = Mode::Rename { target, input };
                None
            }
        },
        Mode::EditSchedule(mut editor) => match key {
            KeyCode::Char('s') if save => {
                let effect = editor.save_effect();
                app.mode = Mode::EditSchedule(editor);
                effect
            }
            KeyCode::Esc => None,
            KeyCode::Tab => {
                editor.cycle_field(false);
                app.mode = Mode::EditSchedule(editor);
                None
            }
            KeyCode::BackTab => {
                editor.cycle_field(true);
                app.mode = Mode::EditSchedule(editor);
                None
            }
            KeyCode::Left | KeyCode::Up if editor.field == ScheduleEditorField::Trigger => {
                editor.cycle_preset(true);
                app.mode = Mode::EditSchedule(editor);
                None
            }
            KeyCode::Right | KeyCode::Down if editor.field == ScheduleEditorField::Trigger => {
                editor.cycle_preset(false);
                app.mode = Mode::EditSchedule(editor);
                None
            }
            KeyCode::Left | KeyCode::Up if editor.field == ScheduleEditorField::Timezone => {
                editor.select_timezone_match(true, true);
                app.mode = Mode::EditSchedule(editor);
                None
            }
            KeyCode::Right | KeyCode::Down if editor.field == ScheduleEditorField::Timezone => {
                editor.select_timezone_match(false, true);
                app.mode = Mode::EditSchedule(editor);
                None
            }
            KeyCode::Left => {
                editor.move_cursor(true);
                app.mode = Mode::EditSchedule(editor);
                None
            }
            KeyCode::Right => {
                editor.move_cursor(false);
                app.mode = Mode::EditSchedule(editor);
                None
            }
            KeyCode::Home => {
                editor.move_to_line_edge(false);
                app.mode = Mode::EditSchedule(editor);
                None
            }
            KeyCode::End => {
                editor.move_to_line_edge(true);
                app.mode = Mode::EditSchedule(editor);
                None
            }
            KeyCode::Enter if editor.field == ScheduleEditorField::Prompt => {
                editor.insert_text("\n");
                app.mode = Mode::EditSchedule(editor);
                None
            }
            KeyCode::Enter => {
                editor.cycle_field(false);
                app.mode = Mode::EditSchedule(editor);
                None
            }
            KeyCode::Backspace => {
                editor.backspace();
                app.mode = Mode::EditSchedule(editor);
                None
            }
            KeyCode::Delete => {
                editor.delete();
                app.mode = Mode::EditSchedule(editor);
                None
            }
            KeyCode::Char(character) => {
                if editor.field == ScheduleEditorField::Cron {
                    editor.preset = ScheduleTriggerPreset::Custom;
                }
                editor.insert_text(&character.to_string());
                app.mode = Mode::EditSchedule(editor);
                None
            }
            _ => {
                app.mode = Mode::EditSchedule(editor);
                None
            }
        },
    }
}

fn render(frame: &mut Frame, app: &mut App) {
    let area = frame.area();
    frame.render_widget(Block::new().style(Style::new().bg(BASE)), area);

    let [tabs_area, dashboard_area, footer_area] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Fill(1),
        Constraint::Length(1),
    ])
    .areas(area);

    render_tabs(frame, tabs_area, app);
    if app.primary_tab == PrimaryTab::Nodes {
        render_nodes(frame, dashboard_area, app);
    } else if app.primary_tab == PrimaryTab::Schedules {
        render_schedules(frame, dashboard_area, app);
    } else if app.primary_tab != PrimaryTab::Workspaces {
        render_global_items(frame, dashboard_area, app);
    } else if dashboard_area.width >= 114 {
        let [workspace_area, terminal_area] =
            Layout::horizontal([Constraint::Length(30), Constraint::Fill(1)]).areas(dashboard_area);
        render_workspaces(frame, workspace_area, app);
        render_items(frame, terminal_area, app);
    } else {
        let [workspace_area, terminal_area] =
            Layout::vertical([Constraint::Percentage(32), Constraint::Fill(1)])
                .areas(dashboard_area);
        render_workspaces(frame, workspace_area, app);
        render_items(frame, terminal_area, app);
    }
    render_footer(frame, footer_area, app);
    match &mut app.mode {
        Mode::PickProject(picker) => render_project_picker(frame, area, picker),
        Mode::Palette(palette) => render_command_palette(frame, area, palette),
        Mode::Help => render_help_overlay(frame, area, app),
        Mode::SelectWorkspaceNode(picker) => render_workspace_node_picker(frame, area, picker),
        Mode::LinkWorkspace(picker) => render_link_workspace_picker(frame, area, picker),
        Mode::InspectNode(node) => render_node_inspection(frame, area, node),
        Mode::RetargetNode { input, .. } => render_node_retarget(frame, area, input),
        Mode::ConfirmForgetNode(node) => render_node_forget_confirmation(frame, area, node),
        Mode::EditSchedule(editor) => render_schedule_editor(frame, area, editor),
        Mode::Normal | Mode::Rename { .. } => {}
    }
}

fn render_node_inspection(frame: &mut Frame, area: Rect, node: &NodeView) {
    let popup = centered_rect(area, 70, 72);
    frame.render_widget(Clear, popup);
    let lines = vec![
        Line::from(format!("ID           {}", node.id)),
        Line::from(format!("Alias        {}", node.alias)),
        Line::from(format!(
            "Route        {}",
            node.route.as_deref().unwrap_or("local")
        )),
        Line::from(format!(
            "Revision     {}",
            node.registration_revision
                .map_or_else(|| "-".into(), |value| value.to_string())
        )),
        Line::from(format!("Health       {}", node_health_label(node.health))),
        Line::from(format!("Current      {}", node.current)),
        Line::from(format!("Stale        {}", node.stale)),
        Line::from(format!(
            "Protocol     {}",
            node.observed_protocol_version
                .map_or_else(|| "-".into(), |value| value.to_string())
        )),
        Line::from(format!(
            "Version      {}",
            node.observed_helper_version.as_deref().unwrap_or("-")
        )),
        Line::from(format!(
            "Last sync    {}",
            if node.observed_at_ms == 0 {
                "never".into()
            } else {
                compact_recency(node.observed_at_ms)
            }
        )),
        Line::from(format!(
            "Scheduler    {:?} {}/{}",
            node.scheduler.state, node.scheduler.active_executions, node.scheduler.max_concurrent
        )),
        Line::from(format!(
            "Placement    {}",
            if node.workspace_owner_eligible {
                "eligible"
            } else {
                node.workspace_owner_unavailable_reason
                    .as_deref()
                    .unwrap_or("unavailable")
            }
        )),
        Line::from(format!(
            "Capabilities {}",
            node.observed_capabilities.join(", ")
        )),
        Line::from(""),
        Line::styled("enter/esc close", Style::new().fg(SUBTEXT)),
    ];
    frame.render_widget(
        Paragraph::new(lines)
            .block(
                Block::bordered()
                    .title(" Node inspection ")
                    .border_style(Style::new().fg(TEAL)),
            )
            .wrap(Wrap { trim: false }),
        popup,
    );
}

fn render_node_retarget(frame: &mut Frame, area: Rect, input: &str) {
    let popup = centered_rect(area, 68, 24);
    frame.render_widget(Clear, popup);
    frame.render_widget(
        Paragraph::new(format!(
            "New SSH route:\n\n{input}_\n\nenter verify and retarget  esc cancel"
        ))
        .block(
            Block::bordered()
                .title(" Retarget Node ")
                .border_style(Style::new().fg(YELLOW)),
        ),
        popup,
    );
}

fn render_node_forget_confirmation(frame: &mut Frame, area: Rect, node: &NodeView) {
    let popup = centered_rect(area, 64, 24);
    frame.render_widget(Clear, popup);
    frame.render_widget(
        Paragraph::new(format!(
            "Forget Node {} ({})?\n\nThis removes only the local route and projection. Remote work continues.\n\ny confirm  n/esc cancel",
            node.alias, node.id
        ))
        .block(Block::bordered().title(" Confirm forget ").border_style(Style::new().fg(RED)))
        .wrap(Wrap { trim: false }),
        popup,
    );
}

fn render_workspace_node_picker(frame: &mut Frame, area: Rect, picker: &WorkspaceNodePicker) {
    let popup = centered_rect(area, 76, 70);
    frame.render_widget(Clear, popup);
    let block = Block::bordered()
        .title(format!(
            " Place first resource for {} ",
            picker.workspace_name
        ))
        .border_style(Style::new().fg(TEAL));
    let inner = block.inner(popup);
    frame.render_widget(block, popup);
    let [notice, table_area, help] = Layout::vertical([
        Constraint::Length(2),
        Constraint::Fill(1),
        Constraint::Length(1),
    ])
    .areas(inner);
    frame.render_widget(
        Paragraph::new("No Node is preselected. Unavailable owners remain visible but disabled.")
            .style(Style::new().fg(SUBTEXT)),
        notice,
    );
    let rows = picker.nodes.iter().map(|node| {
        let status = if node.workspace_owner_eligible {
            "eligible".into()
        } else {
            node.workspace_owner_unavailable_reason
                .clone()
                .unwrap_or_else(|| "unavailable".into())
        };
        Row::new([
            node.alias.clone(),
            node_health_label(node.health).into(),
            status,
            node.route.clone().unwrap_or_else(|| "local".into()),
        ])
        .style(Style::new().fg(if node.workspace_owner_eligible {
            TEXT
        } else {
            OVERLAY
        }))
    });
    let mut state = TableState::default().with_selected(picker.selected);
    frame.render_stateful_widget(
        Table::new(
            rows,
            [
                Constraint::Length(16),
                Constraint::Length(18),
                Constraint::Min(24),
                Constraint::Length(24),
            ],
        )
        .header(Row::new(["NODE", "HEALTH", "PLACEMENT", "ROUTE"]).style(Style::new().fg(BLUE)))
        .row_highlight_style(Style::new().add_modifier(Modifier::REVERSED))
        .highlight_symbol("> "),
        table_area,
        &mut state,
    );
    frame.render_widget(
        Paragraph::new("j/k select eligible Node  enter confirm  esc cancel")
            .style(Style::new().fg(SUBTEXT)),
        help,
    );
}

fn render_link_workspace_picker(frame: &mut Frame, area: Rect, picker: &LinkWorkspacePicker) {
    let popup = centered_rect(area, 60, 60);
    frame.render_widget(Clear, popup);
    let block = Block::bordered()
        .title(" Link external Workspace to ")
        .border_style(Style::new().fg(TEAL));
    let inner = block.inner(popup);
    frame.render_widget(block, popup);
    let [table_area, help] =
        Layout::vertical([Constraint::Fill(1), Constraint::Length(1)]).areas(inner);
    let rows = picker
        .workspaces
        .iter()
        .map(|(id, name, revision)| Row::new([name.clone(), short_id(id), revision.to_string()]));
    let mut state = TableState::default().with_selected(picker.selected);
    frame.render_stateful_widget(
        Table::new(
            rows,
            [
                Constraint::Min(20),
                Constraint::Length(10),
                Constraint::Length(10),
            ],
        )
        .header(Row::new(["WORKSPACE", "ID", "REVISION"]).style(Style::new().fg(BLUE)))
        .row_highlight_style(Style::new().add_modifier(Modifier::REVERSED))
        .highlight_symbol("> "),
        table_area,
        &mut state,
    );
    frame.render_widget(
        Paragraph::new("j/k select  enter guarded link  esc cancel")
            .style(Style::new().fg(SUBTEXT)),
        help,
    );
}

fn render_schedule_editor(frame: &mut Frame, area: Rect, editor: &ScheduleEditor) {
    let popup = if area.width < 96 || area.height < 30 {
        area
    } else {
        centered_rect(area, 82, 92)
    };
    frame.render_widget(Clear, popup);
    frame.render_widget(Block::new().style(Style::new().bg(BASE)), popup);
    let outer = Block::bordered()
        .title(" Edit paused schedule ")
        .border_style(Style::new().fg(TEAL));
    let inner = outer.inner(popup);
    frame.render_widget(outer, popup);
    let error_height = u16::from(editor.error.is_some());
    let [
        notice_area,
        name_area,
        trigger_area,
        cron_area,
        timezone_area,
        prompt_area,
        error_area,
        help_area,
    ] = Layout::vertical([
        Constraint::Length(2),
        Constraint::Length(3),
        Constraint::Length(3),
        Constraint::Length(3),
        Constraint::Length(5),
        Constraint::Fill(1),
        Constraint::Length(error_height),
        Constraint::Length(1),
    ])
    .areas(inner);
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(
                "PRIVATE",
                Style::new().fg(YELLOW).add_modifier(Modifier::BOLD),
            ),
            Span::raw("  Changes apply only to future executions. Active work is unchanged."),
        ])),
        notice_area,
    );

    let name_inner = render_editor_control(
        frame,
        name_area,
        " Name ",
        editor.field == ScheduleEditorField::Name,
        Line::from(safe_editor_text(&editor.name)),
    );
    render_editor_control(
        frame,
        trigger_area,
        " Trigger preset · arrows choose ",
        editor.field == ScheduleEditorField::Trigger,
        Line::from(vec![
            Span::styled("‹ ", Style::new().fg(SUBTEXT)),
            Span::styled(
                editor.preset.label(),
                Style::new().fg(TEXT).add_modifier(Modifier::BOLD),
            ),
            Span::styled(" ›", Style::new().fg(SUBTEXT)),
        ]),
    );
    let cron_inner = render_editor_control(
        frame,
        cron_area,
        " Cron · five fields ",
        editor.field == ScheduleEditorField::Cron,
        Line::from(safe_editor_text(&editor.cron)),
    );

    let timezone_matches = editor.timezone_matches();
    let next_timezone = timezone_matches
        .iter()
        .position(|timezone| *timezone == editor.timezone)
        .and_then(|index| timezone_matches.get((index + 1) % timezone_matches.len()))
        .copied()
        .unwrap_or("none");
    let timezone_inner = render_editor_control(
        frame,
        timezone_area,
        " Timezone · type to search, arrows choose ",
        editor.field == ScheduleEditorField::Timezone,
        Line::from(editor.timezone.clone()),
    );
    if timezone_inner.height > 1 {
        frame.render_widget(
            Paragraph::new(vec![
                Line::from(format!(
                    "Search: {}",
                    safe_editor_text(&editor.timezone_query)
                )),
                Line::from(Span::styled(
                    format!("{} matches · next: {next_timezone}", timezone_matches.len()),
                    Style::new().fg(SUBTEXT),
                )),
            ]),
            Rect::new(
                timezone_inner.x,
                timezone_inner.y + 1,
                timezone_inner.width,
                timezone_inner.height - 1,
            ),
        );
    }

    let prompt_block = Block::bordered()
        .title(" Prompt · private multiline ")
        .border_style(editor_control_color(
            editor.field == ScheduleEditorField::Prompt,
        ));
    let prompt_inner = prompt_block.inner(prompt_area);
    frame.render_widget(prompt_block, prompt_area);
    let prompt_lines = editor.prompt.split('\n').collect::<Vec<_>>();
    let cursor = editor.cursor.min(editor.prompt.len());
    let cursor_line = editor.prompt[..cursor]
        .bytes()
        .filter(|byte| *byte == b'\n')
        .count();
    let visible_rows = usize::from(prompt_inner.height.max(1));
    let start = cursor_line.saturating_sub(visible_rows - 1);
    let visible = prompt_lines
        .iter()
        .skip(start)
        .take(visible_rows)
        .map(|line| Line::from(safe_editor_text(line)))
        .collect::<Vec<_>>();
    frame.render_widget(Paragraph::new(visible), prompt_inner);

    if let Some(error) = &editor.error {
        frame.render_widget(
            Paragraph::new(Span::styled(safe_editor_text(error), Style::new().fg(RED))),
            error_area,
        );
    }
    frame.render_widget(
        Paragraph::new(Span::styled(
            "Tab/Shift-Tab fields  arrows select/move  Enter next/newline  Ctrl-S save  Esc cancel",
            Style::new().fg(SUBTEXT),
        )),
        help_area,
    );

    let cursor_position = match editor.field {
        ScheduleEditorField::Name => text_cursor_position(name_inner, &editor.name, editor.cursor),
        ScheduleEditorField::Cron => text_cursor_position(cron_inner, &editor.cron, editor.cursor),
        ScheduleEditorField::Timezone => Some((
            timezone_inner
                .x
                .saturating_add(8 + editor.timezone_query.width() as u16)
                .min(timezone_inner.right().saturating_sub(1)),
            timezone_inner.y.saturating_add(1),
        )),
        ScheduleEditorField::Prompt => {
            let line_start = editor.prompt[..cursor]
                .rfind('\n')
                .map_or(0, |index| index + 1);
            let column = editor.prompt[line_start..cursor].width() as u16;
            Some((
                prompt_inner
                    .x
                    .saturating_add(column)
                    .min(prompt_inner.right().saturating_sub(1)),
                prompt_inner
                    .y
                    .saturating_add(cursor_line.saturating_sub(start) as u16)
                    .min(prompt_inner.bottom().saturating_sub(1)),
            ))
        }
        ScheduleEditorField::Trigger => None,
    };
    if let Some(position) = cursor_position {
        frame.set_cursor_position(position);
    }
}

fn editor_control_color(focused: bool) -> Style {
    Style::new().fg(if focused { TEAL } else { OVERLAY })
}

fn render_editor_control<'a>(
    frame: &mut Frame,
    area: Rect,
    title: &'a str,
    focused: bool,
    value: Line<'a>,
) -> Rect {
    let block = Block::bordered()
        .title(title)
        .border_style(editor_control_color(focused));
    let inner = block.inner(area);
    frame.render_widget(Paragraph::new(value).block(block), area);
    inner
}

fn text_cursor_position(area: Rect, text: &str, cursor: usize) -> Option<(u16, u16)> {
    (area.width > 0 && area.height > 0).then(|| {
        let column = text[..cursor.min(text.len())].width() as u16;
        (
            area.x
                .saturating_add(column)
                .min(area.right().saturating_sub(1)),
            area.y,
        )
    })
}

fn safe_editor_text(text: &str) -> String {
    text.chars()
        .map(|character| match character {
            '\n' => '\n',
            '\t' => ' ',
            character if character.is_control() => '�',
            character => character,
        })
        .collect()
}

fn render_bomb_animation(frame: &mut Frame, animation_frame: BombAnimationFrame) {
    let area = frame.area();
    frame.render_widget(Block::new().style(Style::new().bg(BASE)), area);

    let width = area.width.min(72);
    let height = area.height.min(30);
    let animation_area = Rect::new(
        area.x + area.width.saturating_sub(width) / 2,
        area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    );
    let canvas_height = height.saturating_sub(1);
    let canvas_area = Rect::new(
        animation_area.x,
        animation_area.y,
        animation_area.width,
        canvas_height,
    );
    match animation_frame {
        BombAnimationFrame::Fuse(stage) => render_lit_bomb(frame, canvas_area, stage),
        BombAnimationFrame::Explosion(stage) => render_explosion(frame, canvas_area, stage),
        BombAnimationFrame::Finished => {}
    }
    if height > 0 {
        let footer_area = Rect::new(
            animation_area.x,
            animation_area.y + canvas_height,
            animation_area.width,
            1,
        );
        frame.render_widget(
            Paragraph::new("press any key to skip")
                .style(Style::new().fg(SUBTEXT))
                .centered(),
            footer_area,
        );
    }
}

fn render_lit_bomb(frame: &mut Frame, area: Rect, stage: usize) {
    let progress = fuse_burn_progress(stage);
    let hopping = stage < HOP_FRAME_COUNT;
    let hop_progress = stage.min(HOP_FRAME_COUNT - 1) as f64 / (HOP_FRAME_COUNT - 1) as f64;
    let canvas = Canvas::default()
        .marker(Marker::Braille)
        .x_bounds([-45.0, 45.0])
        .y_bounds([-30.0, 35.0])
        .paint(move |ctx| {
            let wave = (progress * std::f64::consts::TAU * 1.45).sin();
            let body_x = if hopping {
                43.0 * (1.0 - hop_progress)
            } else {
                wave * 2.8 * (1.0 - progress * 0.45)
            };
            let bob = if !hopping {
                wave.abs()
            } else {
                hop_height(hop_progress)
            };
            let swallow = ((progress - 0.82) / 0.18).clamp(0.0, 1.0);
            let squeeze = swallow * swallow;
            let radius_x = 12.5 + squeeze * 1.8;
            let radius_y = 12.5 - squeeze * 2.8;
            let center_y = -10.0 + bob - squeeze;
            let rotation = if hopping { 0.0 } else { wave * 0.12 };
            let rotate = |x: f64, y: f64| {
                (
                    body_x + x * rotation.cos() - y * rotation.sin(),
                    center_y + x * rotation.sin() + y * rotation.cos(),
                )
            };

            let outline = (0..=64)
                .map(|index| {
                    let angle = index as f64 / 64.0 * std::f64::consts::TAU;
                    (
                        body_x + angle.cos() * radius_x,
                        center_y + angle.sin() * radius_y,
                    )
                })
                .collect::<Vec<_>>();
            for points in outline.windows(2) {
                ctx.draw(&CanvasLine::new(
                    points[0].0,
                    points[0].1,
                    points[1].0,
                    points[1].1,
                    TEAL,
                ));
            }
            for points in outline.windows(2) {
                let inset = |point: (f64, f64)| {
                    (
                        body_x + (point.0 - body_x) * 0.94,
                        center_y + (point.1 - center_y) * 0.94,
                    )
                };
                let first = inset(points[0]);
                let second = inset(points[1]);
                ctx.draw(&CanvasLine::new(first.0, first.1, second.0, second.1, TEAL));
            }

            let cap = [
                rotate(-4.0, radius_y - 1.0),
                rotate(-3.6, radius_y + 3.2),
                rotate(3.6, radius_y + 3.2),
                rotate(4.0, radius_y - 1.0),
                rotate(-4.0, radius_y - 1.0),
            ];
            for points in cap.windows(2) {
                ctx.draw(&CanvasLine::new(
                    points[0].0,
                    points[0].1,
                    points[1].0,
                    points[1].1,
                    TEAL,
                ));
            }

            let fuse_start = rotate(0.0, radius_y + 3.2);
            let curve = |t: f64| {
                let one_minus_t = 1.0 - t;
                rotate(
                    2.0 * one_minus_t * t * -5.0 + t * t * -3.0,
                    radius_y + 3.2 + 2.0 * one_minus_t * t * 6.0 + t * t * 11.0,
                )
            };
            let fuse_end = (1.0 - progress / 0.82).clamp(0.0, 1.0);
            let mut spark = fuse_start;
            if progress < 0.82 {
                let fuse = (0..=16)
                    .map(|index| curve(fuse_end * index as f64 / 16.0))
                    .collect::<Vec<_>>();
                for points in fuse.windows(2) {
                    ctx.draw(&CanvasLine::new(
                        points[0].0,
                        points[0].1,
                        points[1].0,
                        points[1].1,
                        TEAL,
                    ));
                }
                spark = curve(fuse_end);
            } else if swallow < 1.0 {
                spark = (body_x, fuse_start.1 + (center_y - fuse_start.1) * swallow);
            }

            if swallow < 1.0 {
                let pulse = if stage.is_multiple_of(2) { 2.4 } else { 1.7 };
                ctx.draw(&Circle {
                    x: spark.0,
                    y: spark.1,
                    radius: pulse,
                    color: YELLOW,
                });
                for angle in [0.0_f64, 0.8, 1.6, 2.4] {
                    ctx.draw(&CanvasLine::new(
                        spark.0 + angle.cos() * 1.5,
                        spark.1 + angle.sin() * 1.5,
                        spark.0 + angle.cos() * 3.5,
                        spark.1 + angle.sin() * 3.5,
                        YELLOW,
                    ));
                }
            }

            let shadow = (0..=30)
                .map(|index| {
                    let angle = index as f64 / 30.0 * std::f64::consts::TAU;
                    (body_x + angle.cos() * 11.0, -23.5 + angle.sin() * 0.9)
                })
                .collect::<Vec<_>>();
            for points in shadow.windows(2) {
                ctx.draw(&CanvasLine::new(
                    points[0].0,
                    points[0].1,
                    points[1].0,
                    points[1].1,
                    SUBTEXT,
                ));
            }
            ctx.layer();
            for (x, y) in [rotate(-3.0, 3.0), rotate(3.0, 3.0)] {
                ctx.print(
                    x,
                    y,
                    Span::styled("•", Style::new().fg(TEAL).add_modifier(Modifier::BOLD)),
                );
            }
        });
    frame.render_widget(canvas, area);
}

fn render_explosion(frame: &mut Frame, area: Rect, stage: usize) {
    let canvas = Canvas::default()
        .marker(Marker::Braille)
        .x_bounds([-45.0, 45.0])
        .y_bounds([-30.0, 35.0])
        .paint(move |ctx| {
            if !(3..FIREBALL_FRAME_COUNT).contains(&stage) {
                return;
            }
            let cloud = [
                (-18.0, -4.0, 10.0),
                (-13.0, 8.0, 12.0),
                (-4.0, -10.0, 13.0),
                (-2.0, 3.0, 16.0),
                (7.0, -8.0, 12.0),
                (11.0, 5.0, 14.0),
                (19.0, -1.0, 9.0),
                (2.0, 15.0, 11.0),
                (-15.0, 16.0, 8.0),
                (16.0, 16.0, 7.0),
            ];
            let progress = (stage - 2) as f64 / (FIREBALL_FRAME_COUNT - 3) as f64;
            let scale = 1.0 - (1.0 - progress).powi(3);
            for (index, (x, y, radius)) in cloud.into_iter().enumerate() {
                ctx.draw(&Circle {
                    x: x * scale,
                    y: y * scale,
                    radius: radius * scale,
                    color: if index.is_multiple_of(3) {
                        YELLOW
                    } else if index.is_multiple_of(2) {
                        RED
                    } else {
                        Color::Gray
                    },
                });
            }
        });
    frame.render_widget(canvas, area);

    if stage >= FIREBALL_FRAME_COUNT {
        let disperse = stage.saturating_sub(WORD_DISPERSE_START) as f64
            / (EXPLOSION_FRAME_COUNT - WORD_DISPERSE_START) as f64;
        let rise = (disperse.clamp(0.0, 1.0) * 3.0) as u16;
        let word_y = area.y + area.height.saturating_sub(5) / 2;
        let word_area = Rect::new(
            area.x,
            word_y.saturating_sub(rise),
            area.width,
            5.min(area.height),
        );
        frame.render_widget(
            Paragraph::new(smoke_word_lines(stage)).centered(),
            word_area,
        );
    }
}

fn render_project_picker(frame: &mut Frame, area: Rect, picker: &mut ProjectPicker) {
    let popup_area = centered_rect(area, 76, 65);
    frame.render_widget(Clear, popup_area);
    frame.render_widget(Block::new().style(Style::new().bg(BASE)), popup_area);
    let [search_area, list_area, help_area] = Layout::vertical([
        Constraint::Length(3),
        Constraint::Fill(1),
        Constraint::Length(2),
    ])
    .areas(popup_area);

    let selected_tab = Style::new().fg(BASE).bg(TEAL).add_modifier(Modifier::BOLD);
    let idle_tab = Style::new().fg(SUBTEXT);
    let search = Paragraph::new(format!("> {}_", picker.query)).block(
        Block::bordered()
            .title(Line::from(vec![
                Span::raw(" Create workspace  "),
                Span::styled(
                    " BY NAME ",
                    if picker.mode == WorkspaceCreationMode::ByName {
                        selected_tab
                    } else {
                        idle_tab
                    },
                ),
                Span::raw(" "),
                Span::styled(
                    " FROM PROJECT ",
                    if picker.mode == WorkspaceCreationMode::Project {
                        selected_tab
                    } else {
                        idle_tab
                    },
                ),
                Span::raw(" "),
            ]))
            .border_style(Style::new().fg(TEAL)),
    );
    frame.render_widget(search, search_area);

    if picker.mode == WorkspaceCreationMode::ByName {
        let name = picker
            .custom_name()
            .unwrap_or("Type a workspace name above");
        let name_style = if picker.custom_name().is_some() {
            Style::new().fg(TEXT).add_modifier(Modifier::BOLD)
        } else {
            Style::new().fg(SUBTEXT)
        };
        frame.render_widget(
            Paragraph::new(vec![
                Line::default(),
                Line::from(Span::styled(
                    "  Create a workspace by name",
                    Style::new().fg(TEAL).add_modifier(Modifier::BOLD),
                )),
                Line::default(),
                Line::from(vec![
                    Span::styled("  NAME       ", Style::new().fg(SUBTEXT)),
                    Span::styled(name.to_owned(), name_style),
                ]),
                Line::from(vec![
                    Span::styled("  PROJECT    ", Style::new().fg(SUBTEXT)),
                    Span::styled("No project directory", Style::new().fg(SUBTEXT)),
                ]),
                Line::default(),
                Line::from(Span::styled(
                    "  Add shells or launchers after creating the workspace.",
                    Style::new().fg(SUBTEXT),
                )),
            ])
            .block(
                Block::bordered()
                    .title(" By name ")
                    .border_style(Style::new().fg(OVERLAY)),
            ),
            list_area,
        );
    } else {
        let items = if picker.matches.is_empty() {
            let message = if !picker.roots_configured {
                let path = picker.config_path.as_deref().map_or_else(
                    || "config.toml".to_owned(),
                    |path| path.display().to_string(),
                );
                format!("No project suggestions. Add [projects] roots to {path}")
            } else if picker.query.is_empty() {
                "No project suggestions discovered".to_owned()
            } else {
                "No matching project suggestions".to_owned()
            };
            vec![ListItem::new(Span::styled(
                message,
                Style::new().fg(SUBTEXT),
            ))]
        } else {
            let mut previous_group = None;
            picker
                .matches
                .iter()
                .filter_map(|index| picker.projects.get(*index))
                .map(|project| {
                    let group = if previous_group.as_deref() == Some(project.group.as_str()) {
                        String::new()
                    } else {
                        previous_group = Some(project.group.clone());
                        project.group.clone()
                    };
                    ListItem::new(Line::from(vec![
                        Span::styled(
                            format!("{group:<14}"),
                            Style::new().fg(TEAL).add_modifier(Modifier::BOLD),
                        ),
                        Span::styled(
                            format!("{:<24}", project.name),
                            Style::new().fg(TEXT).add_modifier(Modifier::BOLD),
                        ),
                        Span::styled(project.path.display().to_string(), Style::new().fg(SUBTEXT)),
                    ]))
                })
                .collect()
        };
        let list = List::new(items)
            .block(
                Block::bordered()
                    .title(format!(" {} project suggestions ", picker.matches.len()))
                    .border_style(Style::new().fg(OVERLAY)),
            )
            .highlight_symbol("> ")
            .highlight_style(Style::new().fg(TEXT).add_modifier(Modifier::REVERSED));
        frame.render_stateful_widget(list, list_area, &mut picker.state);
    }

    let action_help = if picker.mode == WorkspaceCreationMode::ByName {
        Line::from(vec![
            Span::styled(" type", Style::new().fg(TEAL)),
            Span::raw(" workspace name  "),
            Span::styled("enter", Style::new().fg(GREEN)),
            Span::raw(" create workspace by name"),
        ])
    } else if let Some(warning) = &picker.warning {
        Line::from(Span::styled(format!(" {warning}"), Style::new().fg(YELLOW)))
    } else {
        Line::from(vec![
            Span::styled(" type", Style::new().fg(TEAL)),
            Span::raw(" filter  "),
            Span::styled("up/down", Style::new().fg(BLUE)),
            Span::raw(" choose  "),
            Span::styled("enter", Style::new().fg(GREEN)),
            Span::raw(" create from project"),
        ])
    };
    let help = vec![
        Line::from(vec![
            Span::styled(" tab", Style::new().fg(BLUE)),
            Span::raw(" switch mode  "),
            Span::styled("esc", Style::new().fg(RED)),
            Span::raw(" cancel"),
        ]),
        action_help,
    ];
    frame.render_widget(Paragraph::new(help).style(Style::new().bg(BASE)), help_area);
}

fn render_command_palette(frame: &mut Frame, area: Rect, palette: &mut CommandPalette) {
    let popup_area = centered_rect(area, 82, 72);
    frame.render_widget(Clear, popup_area);
    frame.render_widget(Block::new().style(Style::new().bg(BASE)), popup_area);
    let [search_area, list_area, help_area] = Layout::vertical([
        Constraint::Length(3),
        Constraint::Fill(1),
        Constraint::Length(1),
    ])
    .areas(popup_area);

    frame.render_widget(
        Paragraph::new(format!("> {}_", palette.query)).block(
            Block::bordered()
                .title(" Command palette ")
                .border_style(Style::new().fg(TEAL)),
        ),
        search_area,
    );

    let mut selected_row = None;
    let items = if palette.matches.is_empty() {
        vec![ListItem::new(Span::styled(
            "No matching actions",
            Style::new().fg(SUBTEXT),
        ))]
    } else {
        let mut items = Vec::new();
        let mut previous_action = None;
        let mut previous_kind = None;
        for (match_position, index) in palette.matches.iter().enumerate() {
            let Some(entry) = palette.entries.get(*index) else {
                continue;
            };
            if previous_action != Some(entry.action_group) {
                if previous_action.is_some() {
                    items.push(ListItem::new(""));
                }
                items.push(ListItem::new(Span::styled(
                    entry.action_group.label(),
                    Style::new().fg(TEAL).add_modifier(Modifier::BOLD),
                )));
                previous_action = Some(entry.action_group);
                previous_kind = None;
            }
            if previous_kind != Some(entry.kind_group) {
                items.push(ListItem::new(Span::styled(
                    format!("  {}", entry.kind_group.label()),
                    Style::new().fg(BLUE).add_modifier(Modifier::BOLD),
                )));
                previous_kind = Some(entry.kind_group);
            }
            if palette.state.selected() == Some(match_position) {
                selected_row = Some(items.len());
            }
            items.push(ListItem::new(Line::from(vec![
                Span::styled(
                    format!("    {:<42}", entry.label),
                    Style::new().fg(TEXT).add_modifier(Modifier::BOLD),
                ),
                Span::styled(entry.detail.clone(), Style::new().fg(SUBTEXT)),
            ])));
        }
        items
    };
    let list = List::new(items)
        .block(
            Block::bordered()
                .title(format!(" {} actions ", palette.matches.len()))
                .border_style(Style::new().fg(OVERLAY)),
        )
        .highlight_symbol("> ")
        .highlight_style(Style::new().fg(TEXT).add_modifier(Modifier::REVERSED));
    let mut render_state = ListState::default().with_selected(selected_row);
    frame.render_stateful_widget(list, list_area, &mut render_state);
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(" type", Style::new().fg(TEAL)),
            Span::raw(" filter, including 'blocked' or 'attention'  "),
            Span::styled("up/down", Style::new().fg(BLUE)),
            Span::raw(" select  "),
            Span::styled("enter", Style::new().fg(GREEN)),
            Span::raw(" run  "),
            Span::styled("esc", Style::new().fg(RED)),
            Span::raw(" cancel"),
        ]))
        .style(Style::new().bg(BASE)),
        help_area,
    );
}

fn render_help_overlay(frame: &mut Frame, area: Rect, app: &App) {
    let popup_area = if area.width < 100 || area.height < 30 {
        area
    } else {
        centered_rect(area, 82, 78)
    };
    frame.render_widget(Clear, popup_area);
    frame.render_widget(Block::new().style(Style::new().bg(BASE)), popup_area);
    let [content_area, footer_area] =
        Layout::vertical([Constraint::Fill(1), Constraint::Length(1)]).areas(popup_area);
    let lines = help_lines(app);
    frame.render_widget(
        Paragraph::new(lines)
            .block(
                Block::bordered()
                    .title(" Dashboard help ")
                    .border_style(Style::new().fg(TEAL)),
            )
            .wrap(Wrap { trim: false }),
        content_area,
    );
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(" ?/q/esc", Style::new().fg(RED)),
            Span::raw(" close help  "),
            Span::styled("/", Style::new().fg(TEAL)),
            Span::raw(" command palette"),
        ]))
        .style(Style::new().bg(BASE)),
        footer_area,
    );
}

fn help_lines(app: &App) -> Vec<Line<'static>> {
    let mut lines = vec![
        Line::from(Span::styled(
            "FIND AND ACT",
            Style::new().fg(TEAL).add_modifier(Modifier::BOLD),
        )),
        Line::from("  / or :   search workspaces, items, and actions"),
        Line::from("  blocked  filter palette results to currently blocked agents"),
        Line::from("  attention filter palette results to outstanding durable attention"),
        Line::from("  Enter    restore a workspace, open an item, or inspect a schedule"),
        Line::from("  a/e/x    add, rename, or request confirmed close/remove"),
        Line::from("  Tab/1-4 change view; h/l change pane; j/k navigate"),
        Line::from(""),
        Line::from(Span::styled(
            "SELECTED CONTEXT",
            Style::new().fg(BLUE).add_modifier(Modifier::BOLD),
        )),
    ];
    if app.follow_focused_terminal {
        lines.insert(
            7,
            Line::from(if app.selection_pinned {
                "  Space    unpin selection and resume focused-terminal following"
            } else {
                "  Space    pin selection and pause focused-terminal following"
            }),
        );
    }
    if app.primary_tab == PrimaryTab::Schedules {
        if let Some(schedule) = app.selected_schedule() {
            lines.extend([
                Line::from(format!("  schedule: {} / {}", schedule.workspace, schedule.name)),
                Line::from("  Left/Right changes schedule/history pane; j/k moves within the focused pane."),
                Line::from("  [ and ] also select newer/older retained executions by exact execution ID."),
                Line::from("  The selected schedule's bounded history loads automatically."),
                Line::from("  Enter attaches an active run or resumes a completed exact Agent Session."),
                Line::from("  e edits the exact private definition while paused; Ctrl-S saves with revision protection."),
                Line::from("  u runs now; p pauses/resumes; c cancels only its exact active execution."),
                Line::from("  No skip-next action exists in protocol 25."),
            ]);
        } else {
            lines.push(Line::from(
                "  No schedule selected. Use `boomux schedule create --help`.",
            ));
        }
    } else if app.primary_tab == PrimaryTab::Workspaces && app.focus == Focus::Workspaces {
        if let Some(workspace) = app.selected() {
            lines.extend([
                Line::from(format!("  workspace: {}", workspace.name)),
                Line::from("  Enter restores its launchers and terminal windows."),
                Line::from("  Closing it terminates retained shells and removes launchers."),
            ]);
            if matches!(
                workspace.coordination,
                WorkspaceCoordinationView::Global { closing: false, .. }
            ) {
                lines.push(Line::from(
                    if app.selected_workspace_id.as_deref() == Some(&workspace.id) {
                        "  This is the default Workspace for context-free commands."
                    } else {
                        "  s sets this as the default Workspace for context-free commands."
                    },
                ));
            }
            if workspace.attention_count > 0 {
                lines.push(Line::from(format!(
                    "  attention: {} unseen blocked/completed observation(s)",
                    workspace.attention_count
                )));
            }
        } else {
            lines.push(Line::from("  No workspace selected."));
        }
    } else if let Some(item) = app.selected_item() {
        if matches!(item, WorkspaceItemView::AgentShell(agent) if agent.schedule_id.is_some()) {
            lines.extend([
                Line::from(format!("  agent: {}", item.name())),
                Line::from("  This exact Agent belongs to a scheduled execution."),
                Line::from(
                    "  Ordinary shell open, rename, restart, and close actions are disabled.",
                ),
                Line::from(
                    "  Use the Schedules view to open or cancel the exact linked execution.",
                ),
            ]);
            return lines;
        }
        let (summary, exit) = match item.kind() {
            ItemKind::Shell => (
                "A shell is a durable login-shell PTY slot.",
                "Ctrl-C normally returns to its prompt; exiting the login shell ends the run.",
            ),
            ItemKind::Command => (
                "A command is a durable PTY slot with one exact startup argument vector.",
                "Interrupting or exiting its primary command ends the run.",
            ),
            ItemKind::Agent => (
                "An agent is the current presentation of an underlying shell or command.",
                "Its state comes from lifecycle integration evidence, never quiet output.",
            ),
            ItemKind::Launcher => (
                "A launcher is a detached command invoked when its workspace opens.",
                "Boomux retains no launcher output, invocation history, or process lifetime.",
            ),
            ItemKind::Schedule => (
                "A schedule is a durable recurring Agent work definition.",
                "It is not a shell or process; Enter opens its specialized history and controls.",
            ),
        };
        lines.extend([
            Line::from(format!("  {}: {}", item.kind().label(), item.name())),
            Line::from(format!("  status: {}", item.status())),
            Line::from(format!("  {summary}")),
            Line::from(format!("  {exit}")),
        ]);
        if matches!(item, WorkspaceItemView::AgentShell(agent) if agent.state() == AgentDisplayState::Untracked)
        {
            lines.push(Line::from(
                "  Untracked means a supported foreground host has no authoritative report.",
            ));
        } else if matches!(item, WorkspaceItemView::AgentShell(agent) if agent.state() == AgentDisplayState::Blocked)
        {
            lines.push(Line::from(
                "  Blocked means the current Agent observation requires user input.",
            ));
        }
    } else {
        lines.push(Line::from("  No item selected."));
    }
    lines.extend([
        Line::from(""),
        Line::from(Span::styled(
            "STATE QUICK REFERENCE",
            Style::new().fg(YELLOW).add_modifier(Modifier::BOLD),
        )),
        Line::from("  pending/running/exited describe shell process runs."),
        Line::from("  working/blocked/idle describe authoritative active Agent state."),
        Line::from("  inactive is resumable; done is permanent completion."),
        Line::from("  Attention is durable unseen blocked/completed work and may be stale."),
    ]);
    lines
}

fn centered_rect(area: Rect, width_percent: u16, height_percent: u16) -> Rect {
    let [vertical] = Layout::vertical([Constraint::Percentage(height_percent)])
        .flex(ratatui::layout::Flex::Center)
        .areas(area);
    let [horizontal] = Layout::horizontal([Constraint::Percentage(width_percent)])
        .flex(ratatui::layout::Flex::Center)
        .areas(vertical);
    horizontal
}

fn workspace_display_name(workspace: &WorkspaceView) -> String {
    if workspace.node.local {
        return workspace.name.clone();
    }
    let health = node_health_label(workspace.node.health);
    let freshness = if workspace.node.stale || !workspace.node.current {
        " stale"
    } else {
        ""
    };
    format!(
        "[{} {health}{freshness}] {}",
        workspace.node.alias, workspace.name
    )
}

fn workspace_table_display_name(workspace: &WorkspaceView, duplicate_name: bool) -> String {
    if matches!(
        workspace.coordination,
        WorkspaceCoordinationView::Global { .. }
    ) || (workspace.node.local && !duplicate_name)
    {
        return workspace.name.clone();
    }
    let health = node_health_label(workspace.node.health);
    let freshness = if workspace.node.stale || !workspace.node.current {
        " stale"
    } else {
        ""
    };
    format!(
        "[{} {health}{freshness}] {}",
        workspace.node.alias, workspace.name
    )
}

fn node_health_label(health: NodeProjectionHealthCode) -> &'static str {
    match health {
        NodeProjectionHealthCode::Unobserved => "unobserved",
        NodeProjectionHealthCode::Online => "online",
        NodeProjectionHealthCode::Reconnecting => "reconnecting",
        NodeProjectionHealthCode::Stale => "stale",
        NodeProjectionHealthCode::Unreachable => "unreachable",
        NodeProjectionHealthCode::AuthenticationRequired => "auth required",
        NodeProjectionHealthCode::IdentityChanged => "identity changed",
        NodeProjectionHealthCode::IdentityConflict => "identity conflict",
        NodeProjectionHealthCode::Unsupported => "unsupported",
    }
}

fn render_tabs(frame: &mut Frame, area: Rect, app: &App) {
    if area.width < 100 {
        let spans = PrimaryTab::ALL
            .iter()
            .enumerate()
            .flat_map(|(index, tab)| {
                let style = if *tab == app.primary_tab {
                    Style::new().fg(BASE).bg(BLUE).add_modifier(Modifier::BOLD)
                } else {
                    Style::new().fg(SUBTEXT)
                };
                [
                    Span::styled(format!(" {} {} ", index + 1, tab.label()), style),
                    Span::raw(" "),
                ]
            })
            .collect::<Vec<_>>();
        frame.render_widget(Paragraph::new(Line::from(spans)), area);
        return;
    }
    let workspace_style = if app.primary_tab == PrimaryTab::Workspaces {
        Style::new().fg(BASE).bg(TEAL).add_modifier(Modifier::BOLD)
    } else {
        Style::new().fg(TEAL).add_modifier(Modifier::BOLD)
    };
    let mut spans = vec![
        Span::styled(
            " BOOMUX  ",
            Style::new().fg(TEAL).add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!(" 1 WORKSPACES {} ", app.workspaces.len()),
            workspace_style,
        ),
        Span::raw("      "),
    ];
    spans.extend(
        PrimaryTab::ALL
            .iter()
            .enumerate()
            .skip(1)
            .flat_map(|(index, tab)| {
                let count: usize = match tab {
                    PrimaryTab::Agents => app
                        .workspaces
                        .iter()
                        .flat_map(|workspace| &workspace.items)
                        .filter(|item| item.kind() == ItemKind::Agent)
                        .count(),
                    PrimaryTab::Shells => {
                        app.workspaces.iter().map(WorkspaceView::shell_count).sum()
                    }
                    PrimaryTab::Schedules => app.schedules.len(),
                    PrimaryTab::Nodes => app.nodes.len(),
                    PrimaryTab::Workspaces => unreachable!("workspace tab is rendered separately"),
                };
                let style = if *tab == app.primary_tab {
                    Style::new().fg(BASE).bg(BLUE).add_modifier(Modifier::BOLD)
                } else {
                    Style::new().fg(SUBTEXT)
                };
                [
                    Span::styled(format!("{} {} {count}", index + 1, tab.label()), style),
                    Span::raw("  "),
                ]
            }),
    );
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn render_nodes(frame: &mut Frame, area: Rect, app: &mut App) {
    let rows = app.nodes.iter().map(|node| {
        let protocol = node
            .observed_protocol_version
            .map_or_else(|| "-".into(), |version| version.to_string());
        let version = node
            .observed_helper_version
            .clone()
            .unwrap_or_else(|| "-".into());
        let last_sync = if node.observed_at_ms == 0 {
            "never".into()
        } else {
            compact_recency(node.observed_at_ms)
        };
        let scheduler = match node.scheduler.state {
            crate::protocol::SchedulerState::Active => "active",
            crate::protocol::SchedulerState::Offline => "offline",
        };
        Row::new(vec![
            node.alias.clone(),
            version,
            node_health_label(node.health).into(),
            node.route.clone().unwrap_or_else(|| "local".into()),
            protocol,
            last_sync,
            scheduler.into(),
            node.observed_capabilities.join(","),
        ])
    });
    let table = Table::new(
        rows,
        [
            Constraint::Length(14),
            Constraint::Length(12),
            Constraint::Length(18),
            Constraint::Length(24),
            Constraint::Length(9),
            Constraint::Length(12),
            Constraint::Length(11),
            Constraint::Fill(1),
        ],
    )
    .header(
        Row::new([
            "ALIAS",
            "VERSION",
            "HEALTH",
            "ROUTE",
            "PROTOCOL",
            "LAST SYNC",
            "SCHEDULER",
            "CAPABILITIES",
        ])
        .style(Style::new().fg(BLUE).add_modifier(Modifier::BOLD)),
    )
    .block(
        Block::bordered()
            .title(" Nodes ")
            .border_style(Style::new().fg(OVERLAY)),
    )
    .row_highlight_style(Style::new().fg(TEXT).add_modifier(Modifier::REVERSED))
    .highlight_symbol("> ");
    frame.render_stateful_widget(table, area, &mut app.node_state);
}

fn render_workspaces(frame: &mut Frame, area: ratatui::layout::Rect, app: &mut App) {
    let preview = (app.focus == Focus::Workspaces && area.height >= 12)
        .then(|| app.selected().map(workspace_preview))
        .flatten();
    let (table_area, preview_area) = preview.as_ref().map_or((area, None), |preview| {
        let preview_height = (preview.content_height + 2)
            .min(area.height.saturating_sub(6))
            .max(3);
        let [table_area, preview_area] =
            Layout::vertical([Constraint::Fill(1), Constraint::Length(preview_height)]).areas(area);
        (table_area, Some(preview_area))
    });
    let rows = if app.workspaces.is_empty() {
        vec![Row::new([Cell::from(Span::styled(
            "No workspaces. Press a to create one.",
            Style::new().fg(SUBTEXT),
        ))])]
    } else {
        app.workspaces
            .iter()
            .map(|workspace| {
                let duplicate_name = app
                    .workspaces
                    .iter()
                    .filter(|candidate| candidate.name == workspace.name)
                    .count()
                    > 1;
                Row::new([Cell::from(workspace_table_display_name(
                    workspace,
                    duplicate_name,
                ))])
            })
            .collect()
    };
    let table = Table::new(rows, [Constraint::Min(8)])
        .header(Row::new(["NAME"]).style(Style::new().fg(BLUE).add_modifier(Modifier::BOLD)))
        .block(
            Block::bordered()
                .title(format!(" Workspaces ({}) ", app.workspaces.len()))
                .border_style(Style::new().fg(if app.focus == Focus::Workspaces {
                    TEAL
                } else {
                    OVERLAY
                })),
        )
        .row_highlight_style(
            Style::new()
                .fg(TEXT)
                .add_modifier(Modifier::BOLD | Modifier::REVERSED),
        )
        .highlight_symbol("> ");

    frame.render_stateful_widget(table, table_area, &mut app.workspace_state);
    if let (Some(preview), Some(preview_area)) = (preview, preview_area) {
        render_contextual_preview(frame, preview_area, preview);
    }
}

fn render_schedules(frame: &mut Frame, area: Rect, app: &mut App) {
    let (table_area, history_area) = if app.selected_schedule().is_some() {
        if area.width >= 130 {
            let [schedules, history] =
                Layout::horizontal([Constraint::Percentage(55), Constraint::Percentage(45)])
                    .areas(area);
            (schedules, Some(history))
        } else {
            let [schedules, history] =
                Layout::vertical([Constraint::Percentage(50), Constraint::Percentage(50)])
                    .areas(area);
            (schedules, Some(history))
        }
    } else {
        (area, None)
    };
    let show_trigger = table_area.width >= 60;
    let show_node = app.nodes.len() > 1;
    let show_workspace = table_area.width >= 74;
    let show_integration = table_area.width >= 88;
    let mut headers = vec!["NAME"];
    if show_node {
        headers.push("NODE");
    }
    if show_trigger {
        headers.push("TRIGGER");
    }
    headers.extend(["NEXT", "LAST", "STATE"]);
    if show_workspace {
        headers.push("WORKSPACE");
    }
    if show_integration {
        headers.push("INTEGRATION");
    }
    if app.schedules.is_empty() {
        let message = match app.scheduling {
            SchedulingView::Unsupported {
                required_protocol,
                negotiated,
            } => format!(
                "Schedules require daemon protocol {required_protocol}; negotiated {negotiated}. Upgrade and restart Boomux."
            ),
            SchedulingView::Active { .. } | SchedulingView::Offline { .. } => {
                "No schedules. Run `boomux schedule create --help` to create a paused schedule."
                    .into()
            }
        };
        let block = Block::bordered()
            .title(" Schedules (0) ")
            .border_style(Style::new().fg(TEAL));
        let inner = block.inner(area);
        frame.render_widget(block, area);
        frame.render_widget(
            Paragraph::new(message)
                .style(Style::new().fg(SUBTEXT))
                .wrap(Wrap { trim: false }),
            inner,
        );
        return;
    }
    let row_values: Vec<Vec<String>> = {
        app.schedules
            .iter()
            .map(|schedule| {
                let last = schedule.executions.first().map_or_else(
                    || {
                        if schedule.history_complete {
                            "never run".into()
                        } else {
                            "history unknown".into()
                        }
                    },
                    execution_summary,
                );
                let next = schedule.next_occurrence_ms.map_or_else(
                    || {
                        if schedule.state == ScheduleDisplayState::Paused {
                            "paused".into()
                        } else {
                            "unavailable".into()
                        }
                    },
                    occurrence_recency,
                );
                let mut values = vec![if show_node || schedule.node_alias == "local" {
                    schedule.name.clone()
                } else {
                    format!("[{}] {}", schedule.node_alias, schedule.name)
                }];
                if show_node {
                    values.push(schedule.node_alias.clone());
                }
                if show_trigger {
                    values.push(schedule.friendly_trigger.clone());
                }
                values.extend([next, last, schedule.state.label().into()]);
                if show_workspace {
                    values.push(schedule.workspace.clone());
                }
                if show_integration {
                    values.push(schedule.integration.clone());
                }
                values
            })
            .collect()
    };
    let widths = schedule_column_widths(table_area.width, &headers, &row_values);
    let rows: Vec<_> = row_values
        .into_iter()
        .map(|values| {
            Row::new(values.into_iter().enumerate().map(|(index, value)| {
                let styled = if headers.get(index) == Some(&"LAST")
                    || headers.get(index) == Some(&"STATE")
                {
                    Span::styled(value.clone(), Style::new().fg(status_color(&value)))
                } else {
                    Span::raw(value)
                };
                Cell::from(styled)
            }))
        })
        .collect();
    let health = app
        .selected_schedule()
        .and_then(|schedule| {
            app.nodes
                .iter()
                .find(|node| node.id == schedule.node_id)
                .map(|node| {
                    format!(
                        "{} {} {}/{}",
                        node.alias,
                        match node.scheduler.state {
                            boomux::protocol::SchedulerState::Active => "active",
                            boomux::protocol::SchedulerState::Offline => "offline",
                        },
                        node.scheduler.active_executions,
                        node.scheduler.max_concurrent,
                    )
                })
        })
        .unwrap_or_else(|| match app.scheduling {
            SchedulingView::Unsupported { .. } => "unsupported".into(),
            SchedulingView::Active { active, maximum } => format!("active {active}/{maximum}"),
            SchedulingView::Offline { active, maximum } => format!("offline {active}/{maximum}"),
        });
    let table = Table::new(rows, widths)
        .header(Row::new(headers).style(Style::new().fg(BLUE).add_modifier(Modifier::BOLD)))
        .column_spacing(1)
        .block(
            Block::bordered()
                .title(format!(
                    " Schedules ({}) · scheduler {health} ",
                    app.schedules.len()
                ))
                .border_style(Style::new().fg(if app.focus == Focus::Workspaces {
                    TEAL
                } else {
                    OVERLAY
                })),
        )
        .row_highlight_style(
            Style::new()
                .fg(TEXT)
                .add_modifier(Modifier::BOLD | Modifier::REVERSED),
        )
        .highlight_symbol("> ");
    frame.render_stateful_widget(table, table_area, &mut app.global_state);

    if let Some(history_area) = history_area
        && let Some(schedule) = app.selected_schedule().cloned()
    {
        render_execution_history(
            frame,
            history_area,
            &schedule,
            app.focus == Focus::Items,
            &mut app.execution_state,
        );
    }
}

fn render_execution_history(
    frame: &mut Frame,
    area: Rect,
    schedule: &ScheduleView,
    focused: bool,
    state: &mut TableState,
) {
    let history_label = schedule_history_label(schedule);
    let rows = schedule.executions.iter().map(|execution| {
        let links = [
            execution
                .agent_id
                .as_deref()
                .map(|id| format!("Agent {}", short_id(id))),
            execution
                .session_id
                .as_deref()
                .map(|id| format!("session {}", short_id(id))),
        ]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join(" · ");
        Row::new([
            Cell::from(compact_recency(execution.requested_at_ms)),
            Cell::from(Span::styled(
                execution_summary(execution),
                Style::new().fg(status_color(&execution_summary(execution))),
            )),
            Cell::from(short_id(&execution.id)),
            Cell::from(links),
        ])
    });
    let table = Table::new(
        rows,
        [
            Constraint::Length(9),
            Constraint::Length(18),
            Constraint::Length(10),
            Constraint::Min(12),
        ],
    )
    .header(Row::new(["WHEN", "RESULT", "EXECUTION", "LINKS"]).style(Style::new().fg(BLUE)))
    .column_spacing(1)
    .block(
        Block::bordered()
            .title(format!(
                " History · {history_label} · {} records ",
                schedule.executions.len()
            ))
            .border_style(Style::new().fg(if focused { TEAL } else { OVERLAY })),
    )
    .row_highlight_style(
        Style::new()
            .fg(TEXT)
            .add_modifier(Modifier::BOLD | Modifier::REVERSED),
    )
    .highlight_symbol("> ");
    frame.render_stateful_widget(table, area, state);
}

fn schedule_history_label(schedule: &ScheduleView) -> &'static str {
    if schedule.history_truncated {
        "newest retained page is truncated"
    } else if schedule.possible_pruning_boundary {
        "oldest retained record may be a pruning boundary"
    } else if schedule.history_scoped {
        "schedule-scoped bounded history"
    } else {
        "recent bounded history"
    }
}

fn execution_summary(execution: &ExecutionView) -> String {
    if let Some(reason) = execution.reason {
        return if reason == ExecutionReasonDisplay::Missed {
            "missed".into()
        } else {
            format!("{}: {}", execution.state.label(), reason.label())
        };
    }
    match execution.outcome {
        Some(ExecutionOutcomeDisplay::ExitCode(0)) => "exited: 0".into(),
        Some(ExecutionOutcomeDisplay::ExitCode(code)) => format!("failed: exit {code}"),
        Some(ExecutionOutcomeDisplay::Signal(signal)) => format!("failed: signal {signal}"),
        None => execution.state.label().into(),
    }
}

fn occurrence_recency(timestamp_ms: u64) -> String {
    let seconds = timestamp_ms.saturating_sub(current_time_ms()) / 1_000;
    match seconds {
        0..=59 => "<1m".into(),
        60..=3_599 => format!("in {}m", seconds / 60),
        3_600..=86_399 => format!("in {}h", seconds / 3_600),
        _ => format!("in {}d", seconds / 86_400),
    }
}

fn render_global_items(frame: &mut Frame, area: Rect, app: &mut App) {
    let title = format!(
        " {} ({}) ",
        app.primary_tab.label(),
        app.global_item_count()
    );
    let block = Block::bordered()
        .title(title)
        .border_style(Style::new().fg(TEAL));
    let inner = block.inner(area);
    let contextual_panel = selected_item_preview(app).filter(|panel| {
        inner.height
            >= panel
                .content_height
                .saturating_add(2)
                .saturating_add(PREVIEW_RESERVED_ITEM_HEIGHT)
    });
    let (items_inner, preview_area) = contextual_panel.as_ref().map_or((inner, None), |panel| {
        let panel_height = panel.content_height + 2;
        let [items_area, preview_area] =
            Layout::vertical([Constraint::Fill(1), Constraint::Length(panel_height)]).areas(inner);
        (items_area, Some(preview_area))
    });
    let (rows, widths, header) = if app.primary_tab == PrimaryTab::Agents {
        let values: Vec<[String; 9]> = app
            .workspaces
            .iter()
            .flat_map(|workspace| {
                workspace
                    .items
                    .iter()
                    .enumerate()
                    .filter_map(move |(item_index, item)| {
                        let WorkspaceItemView::AgentShell(agent) = item else {
                            return None;
                        };
                        let task = matched_agent_session(workspace, agent)
                            .and_then(session_task_label)
                            .unwrap_or("-");
                        let (updated, integration, branch, worktree) =
                            agent.agent.as_ref().map_or_else(
                                || ("-".into(), "-".into(), "-".into(), "-".into()),
                                |view| {
                                    (
                                        compact_recency(view.updated_at_ms),
                                        view.integration.clone(),
                                        view.root_branch.clone(),
                                        view.root_worktree.clone(),
                                    )
                                },
                            );
                        Some([
                            agent.state().label().to_owned(),
                            updated,
                            workspace_display_name(workspace),
                            workspace.item_owner(item_index).0.alias.clone(),
                            agent.shell.name.clone(),
                            integration,
                            task.to_owned(),
                            branch,
                            worktree,
                        ])
                    })
            })
            .collect();
        let widths = agent_column_widths(items_inner.width, &values);
        let rows: Vec<_> = values
            .into_iter()
            .map(
                |[
                    status,
                    updated,
                    workspace,
                    node,
                    shell,
                    integration,
                    task,
                    branch,
                    worktree,
                ]| {
                    Row::new([
                        Cell::from(Span::styled(
                            status.clone(),
                            Style::new().fg(status_color(&status)),
                        )),
                        Cell::from(updated),
                        Cell::from(workspace),
                        Cell::from(node),
                        Cell::from(shell),
                        Cell::from(integration),
                        Cell::from(task),
                        Cell::from(branch),
                        Cell::from(worktree),
                    ])
                },
            )
            .collect();
        (rows, widths, AGENT_TABLE_HEADERS.to_vec())
    } else if app.primary_tab == PrimaryTab::Shells {
        let values: Vec<[String; 9]> = app
            .workspaces
            .iter()
            .flat_map(|workspace| {
                workspace
                    .items
                    .iter()
                    .enumerate()
                    .filter_map(move |(item_index, item)| {
                        let WorkspaceItemView::Shell(shell) = item else {
                            return None;
                        };
                        Some([
                            shell.table_status(),
                            shell
                                .run
                                .as_ref()
                                .map_or_else(|| "-".into(), |run| format!("#{}", run.generation)),
                            workspace_display_name(workspace),
                            workspace.item_owner(item_index).0.alias.clone(),
                            shell.name.clone(),
                            shell.kind.label().into(),
                            shell.process().into(),
                            shell.branch.clone(),
                            shell.worktree.clone(),
                        ])
                    })
            })
            .collect();
        let widths = shell_column_widths(items_inner.width, &values);
        let rows = values
            .into_iter()
            .map(
                |[
                    status,
                    run,
                    workspace,
                    node,
                    shell,
                    kind,
                    process,
                    branch,
                    worktree,
                ]| {
                    Row::new([
                        Cell::from(Span::styled(
                            status.clone(),
                            Style::new().fg(status_color(&status)),
                        )),
                        Cell::from(run),
                        Cell::from(workspace),
                        Cell::from(node),
                        Cell::from(shell),
                        Cell::from(kind),
                        Cell::from(process),
                        Cell::from(branch),
                        Cell::from(worktree),
                    ])
                },
            )
            .collect();
        (rows, widths, SHELL_TABLE_HEADERS.to_vec())
    } else {
        let kind = app.primary_tab.kind().expect("global tab kind");
        let rows: Vec<_> = app
            .workspaces
            .iter()
            .flat_map(|workspace| {
                workspace
                    .items
                    .iter()
                    .filter(move |item| item.kind() == kind)
                    .map(move |item| {
                        let mut cells = vec![Cell::from(workspace_display_name(workspace))];
                        match item {
                            WorkspaceItemView::Shell(shell) => cells.extend([
                                Cell::from(shell.name.clone()),
                                Cell::from(Span::styled(
                                    shell.status.clone(),
                                    Style::new().fg(status_color(&shell.status)),
                                )),
                                Cell::from(shell.directory.clone()),
                                Cell::from(shell.detail().to_owned()),
                            ]),
                            WorkspaceItemView::AgentShell(agent) => cells.extend([
                                Cell::from(agent.shell.name.clone()),
                                Cell::from(Span::styled(
                                    agent.state().label().to_owned(),
                                    Style::new().fg(agent_row_color(agent.state())),
                                )),
                                Cell::from(agent.shell.directory.clone()),
                                Cell::from(agent.agent.as_ref().map_or_else(
                                    || format!("foreground process | {}", agent.shell.branch),
                                    |view| {
                                        format!(
                                            "{} | {} | {} / {} {}%",
                                            view.evidence,
                                            agent.shell.branch,
                                            view.integration,
                                            view.authority.label(),
                                            view.confidence
                                        )
                                    },
                                )),
                            ]),
                            WorkspaceItemView::Launcher(launcher) => cells.extend([
                                Cell::from(launcher.name.clone()),
                                Cell::from("-"),
                                Cell::from(launcher.directory.clone()),
                                Cell::from(launcher.command.clone()),
                            ]),
                            WorkspaceItemView::Schedule(_) => {
                                unreachable!("schedules use their specialized global view")
                            }
                        }
                        Row::new(cells)
                    })
            })
            .collect();
        (
            rows,
            global_column_widths(items_inner.width),
            vec!["WORKSPACE", "NAME", "STATUS", "DIRECTORY", "DETAIL"],
        )
    };
    let table = Table::new(rows, widths)
        .header(Row::new(header).style(Style::new().fg(BLUE).add_modifier(Modifier::BOLD)))
        .column_spacing(1)
        .row_highlight_style(
            Style::new()
                .fg(TEXT)
                .add_modifier(Modifier::BOLD | Modifier::REVERSED),
        )
        .highlight_symbol("> ");
    frame.render_widget(block, area);
    frame.render_stateful_widget(table, items_inner, &mut app.global_state);
    if let (Some(panel), Some(panel_area)) = (contextual_panel, preview_area) {
        render_contextual_preview(frame, panel_area, panel);
    }
}

fn render_items(frame: &mut Frame, area: ratatui::layout::Rect, app: &mut App) {
    let selected = app
        .workspace_state
        .selected()
        .and_then(|index| app.workspaces.get(index));
    let title = selected.map_or_else(
        || " Items ".to_owned(),
        |workspace| {
            format!(
                " Items: {} ({}) ",
                workspace.name,
                workspace.ordinary_item_count()
            )
        },
    );
    let block = Block::bordered().title(title).border_style(Style::new().fg(
        if app.focus == Focus::Items {
            TEAL
        } else {
            OVERLAY
        },
    ));
    let inner = block.inner(area);
    let contextual_panel = (app.focus == Focus::Items)
        .then(|| selected_item_preview(app))
        .flatten()
        .filter(|panel| {
            inner.height
                >= panel
                    .content_height
                    .saturating_add(2)
                    .saturating_add(PREVIEW_RESERVED_ITEM_HEIGHT)
        });
    let (items_inner, preview_area) = contextual_panel.as_ref().map_or((inner, None), |panel| {
        let panel_height = panel.content_height + 2;
        let [items_area, preview_area] =
            Layout::vertical([Constraint::Fill(1), Constraint::Length(panel_height)]).areas(inner);
        (items_area, Some(preview_area))
    });
    let values: Vec<[String; 7]> = selected
        .into_iter()
        .flat_map(|workspace| {
            workspace
                .items
                .iter()
                .enumerate()
                .filter(|(_, item)| item.ordinary_visible())
                .map(|(item_index, item)| {
                    let node = workspace.item_owner(item_index).0.alias.clone();
                    match item {
                        WorkspaceItemView::Shell(terminal) => [
                            terminal.kind.label().into(),
                            terminal.table_status(),
                            terminal.name.clone(),
                            node,
                            terminal.process().into(),
                            terminal.branch.clone(),
                            terminal.worktree.clone(),
                        ],
                        WorkspaceItemView::AgentShell(agent_shell) => {
                            let (activity, branch, worktree) =
                                agent_shell.agent.as_ref().map_or_else(
                                    || {
                                        (
                                            agent_shell.shell.process().to_owned(),
                                            agent_shell.shell.branch.clone(),
                                            agent_shell.shell.worktree.clone(),
                                        )
                                    },
                                    |agent| {
                                        (
                                            matched_agent_session(workspace, agent_shell)
                                                .and_then(session_task_label)
                                                .unwrap_or(&agent.integration)
                                                .to_owned(),
                                            agent.root_branch.clone(),
                                            agent.root_worktree.clone(),
                                        )
                                    },
                                );
                            [
                                "agent".into(),
                                agent_shell.state().label().into(),
                                agent_shell.shell.name.clone(),
                                node,
                                activity,
                                branch,
                                worktree,
                            ]
                        }
                        WorkspaceItemView::Launcher(launcher) => [
                            "launcher".into(),
                            "ready".into(),
                            launcher.name.clone(),
                            node,
                            launcher.command.clone(),
                            launcher.branch.clone(),
                            launcher.worktree.clone(),
                        ],
                        WorkspaceItemView::Schedule(schedule) => [
                            "schedule".into(),
                            schedule.state.label().into(),
                            schedule.name.clone(),
                            node,
                            format!("{} · {}", schedule.integration, schedule.friendly_trigger),
                            "-".into(),
                            "-".into(),
                        ],
                    }
                })
        })
        .collect();
    let widths = item_column_widths(items_inner.width, &values);
    let rows = values
        .into_iter()
        .map(|[kind, status, name, node, activity, branch, worktree]| {
            let kind_color = match kind.as_str() {
                "agent" => TEAL,
                "command" | "launcher" | "schedule" => YELLOW,
                _ => TEXT,
            };
            Row::new([
                Cell::from(Span::styled(kind, Style::new().fg(kind_color))),
                Cell::from(Span::styled(
                    status.clone(),
                    Style::new().fg(status_color(&status)),
                )),
                Cell::from(name),
                Cell::from(node),
                Cell::from(activity),
                Cell::from(branch),
                Cell::from(worktree),
            ])
        });
    frame.render_widget(block, area);
    let table = Table::new(rows, widths)
        .header(
            Row::new(ITEM_TABLE_HEADERS).style(Style::new().fg(BLUE).add_modifier(Modifier::BOLD)),
        )
        .column_spacing(1)
        .row_highlight_style(
            Style::new()
                .fg(TEXT)
                .add_modifier(Modifier::BOLD | Modifier::REVERSED),
        )
        .highlight_symbol("> ");
    frame.render_stateful_widget(table, items_inner, &mut app.item_state);
    if let (Some(panel), Some(panel_area)) = (contextual_panel, preview_area) {
        render_contextual_preview(frame, panel_area, panel);
    }
}

struct ContextualPreview {
    title: String,
    content: PreviewContent,
    content_height: u16,
}

enum PreviewContent {
    Lines(Vec<Line<'static>>),
}

fn selected_item_preview(app: &App) -> Option<ContextualPreview> {
    match app.selected_item()? {
        WorkspaceItemView::AgentShell(agent) => agent_session_preview(app, agent),
        WorkspaceItemView::Shell(terminal) => terminal_preview(app, terminal),
        WorkspaceItemView::Launcher(launcher) => Some(launcher_preview(launcher)),
        WorkspaceItemView::Schedule(schedule) => Some(schedule_item_preview(schedule)),
    }
}

fn workspace_preview(workspace: &WorkspaceView) -> ContextualPreview {
    let counts = workspace.agent_state_counts;
    let mut lines = Vec::new();
    if let Some(default_cwd) = &workspace.default_cwd {
        lines.push(Line::from(vec![
            Span::styled("default  ", Style::new().fg(SUBTEXT)),
            Span::raw(default_cwd.clone()),
        ]));
    }
    match &workspace.coordination {
        WorkspaceCoordinationView::Global {
            revision,
            closing,
            placements,
        } => {
            lines.push(Line::from(format!(
                "global revision {revision} · {}",
                if *closing { "closing" } else { "active" }
            )));
            lines.extend(placements.iter().map(|placement| {
                Line::from(format!(
                    "{} · {:?} · owner r{} · {}",
                    placement.node.alias,
                    placement.state,
                    placement.owner_revision,
                    placement.default_cwd.as_deref().unwrap_or("no default cwd")
                ))
            }));
        }
        WorkspaceCoordinationView::External {
            owner_revision,
            available,
        } => lines.push(Line::from(format!(
            "external owner revision {owner_revision} · {}",
            if *available {
                "available"
            } else {
                "unavailable"
            }
        ))),
    }
    lines.extend([
        Line::from(format!(
            "{:<9}{:<3}{:<9}{}",
            "shell",
            workspace.shell_count(),
            "command",
            workspace.command_count()
        )),
        Line::from(format!(
            "{:<9}{:<3}{:<9}{}",
            "launcher",
            workspace.launcher_count(),
            "agent",
            workspace.agent_count()
        )),
        Line::from(format!("{:<9}{}", "schedule", workspace.schedule_count())),
        Line::from(Span::styled(
            format!(
                "{:<9}{:<3}{:<9}{}",
                "working", counts.working, "blocked", counts.blocked
            ),
            Style::new().fg(SUBTEXT),
        )),
        Line::from(Span::styled(
            format!(
                "{:<9}{:<3}{:<9}{}",
                "idle", counts.idle, "done", counts.done
            ),
            Style::new().fg(SUBTEXT),
        )),
    ]);
    ContextualPreview {
        title: format!(" {} overview ", workspace.name),
        content_height: lines.len() as u16,
        content: PreviewContent::Lines(lines),
    }
}

fn schedule_item_preview(schedule: &ScheduleItemView) -> ContextualPreview {
    ContextualPreview {
        title: " Schedule definition ".into(),
        content_height: 4,
        content: PreviewContent::Lines(vec![
            Line::from(vec![
                Span::styled("state       ", Style::new().fg(SUBTEXT)),
                Span::raw(schedule.state.label()),
            ]),
            Line::from(vec![
                Span::styled("trigger     ", Style::new().fg(SUBTEXT)),
                Span::raw(schedule.friendly_trigger.clone()),
            ]),
            Line::from(vec![
                Span::styled("integration ", Style::new().fg(SUBTEXT)),
                Span::raw(schedule.integration.clone()),
            ]),
            Line::from(Span::styled(
                "Enter opens this schedule's history and controls.",
                Style::new().fg(SUBTEXT),
            )),
        ]),
    }
}

fn launcher_preview(launcher: &LauncherView) -> ContextualPreview {
    ContextualPreview {
        title: " Launcher configuration ".into(),
        content_height: 4,
        content: PreviewContent::Lines(vec![
            Line::from(vec![
                Span::styled("cwd  ", Style::new().fg(SUBTEXT)),
                Span::raw(launcher.directory.clone()),
            ]),
            Line::from(vec![
                Span::styled("argv ", Style::new().fg(SUBTEXT)),
                Span::raw(format_argv(&launcher.argv)),
            ]),
            Line::from(vec![
                Span::styled("git  ", Style::new().fg(SUBTEXT)),
                Span::raw(launcher.repository.clone()),
                Span::styled("  branch ", Style::new().fg(SUBTEXT)),
                Span::raw(launcher.branch.clone()),
                Span::styled("  state ", Style::new().fg(SUBTEXT)),
                Span::raw(launcher.git_state.clone()),
                Span::styled("  worktree ", Style::new().fg(SUBTEXT)),
                Span::raw(launcher.worktree.clone()),
            ]),
            Line::from(Span::styled(
                "Detached invocation; output and run history are not retained",
                Style::new().fg(SUBTEXT),
            )),
        ]),
    }
}

fn terminal_preview(app: &App, terminal: &TerminalView) -> Option<ContextualPreview> {
    let is_command = terminal.kind == TerminalKind::Command;
    let mut lines = Vec::new();
    if is_command {
        lines.push(terminal_preview_field(
            "COMMAND",
            format_argv(&terminal.argv),
        ));
    }
    lines.push(terminal_preview_field("PATH", terminal.directory.clone()));
    lines.push(terminal_preview_field(
        "GIT",
        format!(
            "{}  ·  {}  ·  {}  ·  {}",
            terminal.repository, terminal.branch, terminal.git_state, terminal.worktree
        ),
    ));
    let run_detail = terminal.run.as_ref().map_or_else(
        || format!("{}  ·  no run yet", terminal.table_status()),
        |run| {
            let timing = run.ended_at_ms.map_or_else(
                || format!("started {}", compact_recency(run.started_at_ms)),
                |ended| format!("ended {}", compact_recency(ended)),
            );
            format!(
                "{}  ·  generation {}  ·  {timing}  ·  id {}",
                terminal.table_status(),
                run.generation,
                short_id(&run.id)
            )
        },
    );
    lines.push(Line::from(vec![
        terminal_preview_label("RUN"),
        Span::styled(
            run_detail,
            Style::new()
                .fg(status_color(&terminal.table_status()))
                .add_modifier(Modifier::BOLD),
        ),
    ]));

    if let Some(preview) = app
        .terminal_preview
        .as_ref()
        .filter(|preview| !is_command && preview.shell_id == terminal.id)
    {
        match &preview.output {
            Ok(output) if terminal_preview_is_empty(output) => {
                lines.push(terminal_preview_field("OUTPUT", "no terminal output"))
            }
            Ok(output) => {
                let viewport =
                    terminal_viewport(output, TERMINAL_PREVIEW_ROWS, preview.scroll_from_bottom);
                lines.push(Line::from(vec![
                    terminal_preview_label("OUTPUT"),
                    Span::styled(
                        format!(
                            "lines {}-{} of {}  ·  ",
                            viewport.start + 1,
                            viewport.end,
                            viewport.total
                        ),
                        Style::new().fg(SUBTEXT),
                    ),
                    Span::styled(
                        if viewport.following {
                            "following"
                        } else {
                            "scrolled"
                        },
                        Style::new()
                            .fg(if viewport.following { GREEN } else { YELLOW })
                            .add_modifier(Modifier::BOLD),
                    ),
                ]));
                lines.extend(viewport.lines.into_iter().map(terminal_preview_line));
            }
            Err(error) => lines.push(Line::from(vec![
                terminal_preview_label("OUTPUT"),
                Span::styled(format!("unavailable: {error}"), Style::new().fg(YELLOW)),
            ])),
        }
    }
    let workspace = app
        .selected_item_workspace()
        .map_or("-", |workspace| workspace.name.as_str());
    Some(ContextualPreview {
        title: if is_command {
            format!(" Command · {workspace} / {} ", terminal.name)
        } else {
            format!(" Shell · {workspace} / {} ", terminal.name)
        },
        content_height: if is_command {
            lines.len() as u16
        } else {
            (TERMINAL_PREVIEW_ROWS + 4) as u16
        },
        content: PreviewContent::Lines(lines),
    })
}

fn terminal_preview_field(label: &'static str, value: impl Into<String>) -> Line<'static> {
    Line::from(vec![terminal_preview_label(label), Span::raw(value.into())])
}

fn terminal_preview_label(label: &'static str) -> Span<'static> {
    Span::styled(format!("{label:<10}"), Style::new().fg(SUBTEXT))
}

struct TerminalViewport {
    lines: Vec<TerminalPreviewLine>,
    start: usize,
    end: usize,
    total: usize,
    following: bool,
}

fn terminal_output_lines(output: &TerminalPreview) -> Vec<TerminalPreviewLine> {
    let mut lines = output.lines.clone();
    for line in &mut lines {
        while let Some(span) = line.spans.last_mut() {
            let trimmed = span.text.trim_end().len();
            span.text.truncate(trimmed);
            if span.text.is_empty() {
                line.spans.pop();
            } else {
                break;
            }
        }
    }
    let first = lines
        .iter()
        .position(|line| !terminal_preview_line_is_empty(line))
        .unwrap_or(lines.len());
    let end = lines
        .iter()
        .rposition(|line| !terminal_preview_line_is_empty(line))
        .map_or(first, |last| last + 1);
    lines.drain(end..);
    lines.drain(..first);
    lines
}

fn terminal_viewport(
    output: &TerminalPreview,
    height: usize,
    scroll_from_bottom: usize,
) -> TerminalViewport {
    let lines = terminal_output_lines(output);
    let total = lines.len();
    let latest_start = total.saturating_sub(height);
    let scroll_from_bottom = scroll_from_bottom.min(latest_start);
    let start = latest_start - scroll_from_bottom;
    let end = (start + height).min(total);
    TerminalViewport {
        lines: lines[start..end].to_vec(),
        start,
        end,
        total,
        following: scroll_from_bottom == 0,
    }
}

fn terminal_preview_is_empty(preview: &TerminalPreview) -> bool {
    preview.lines.iter().all(terminal_preview_line_is_empty)
}

fn terminal_preview_line_is_empty(line: &TerminalPreviewLine) -> bool {
    line.spans.iter().all(|span| span.text.trim().is_empty())
}

fn terminal_preview_line(line: TerminalPreviewLine) -> Line<'static> {
    let mut spans = Vec::with_capacity(line.spans.len() + 1);
    spans.push(Span::raw(" "));
    spans.extend(
        line.spans
            .into_iter()
            .map(|span| Span::styled(span.text, terminal_style(span.style))),
    );
    Line::from(spans)
}

fn terminal_style(style: TerminalStyle) -> Style {
    let mut modifiers = Modifier::empty();
    if style.bold {
        modifiers |= Modifier::BOLD;
    }
    if style.dim {
        modifiers |= Modifier::DIM;
    }
    if style.italic {
        modifiers |= Modifier::ITALIC;
    }
    if style.underline {
        modifiers |= Modifier::UNDERLINED;
    }
    if style.inverse {
        modifiers |= Modifier::REVERSED;
    }
    Style::new()
        .fg(terminal_color(style.foreground))
        .bg(terminal_color(style.background))
        .add_modifier(modifiers)
}

fn terminal_color(color: TerminalColor) -> Color {
    match color {
        TerminalColor::Default => Color::Reset,
        TerminalColor::Indexed(index) => Color::Indexed(index),
        TerminalColor::Rgb { red, green, blue } => Color::Rgb(red, green, blue),
    }
}

fn format_argv(argv: &[String]) -> String {
    format!("{argv:?}")
}

fn agent_session_preview(app: &App, agent_shell: &AgentShellView) -> Option<ContextualPreview> {
    let agent = agent_shell.agent.as_ref()?;
    let workspace = app.selected_item_workspace()?;
    let session = matched_agent_session(workspace, agent_shell)?;
    let label = best_session_label(session);
    let external_identity = session
        .external_session_id
        .as_deref()
        .map(short_id)
        .unwrap_or_else(|| short_id(&session.id));
    let shell = session
        .runs
        .last()
        .and_then(|run| run.shell_name.as_deref())
        .unwrap_or(if session.runs.is_empty() {
            "catalog only"
        } else {
            "removed shell"
        });
    let occurrences = session.runs.len();
    let state = agent_shell.state();
    let currency = if state == AgentDisplayState::Inactive {
        "resumable"
    } else {
        "current"
    };
    let root_directory = session
        .source_cwd
        .as_deref()
        .map_or_else(|| "-".into(), |path| path.display().to_string());
    let mut lines = vec![
        preview_field("TASK", label),
        Line::from(vec![
            preview_label("STATUS"),
            Span::styled(
                state.label(),
                Style::new()
                    .fg(session_state_color(state))
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!(
                    "  ·  {currency}  ·  updated {}",
                    compact_recency(agent.updated_at_ms)
                ),
                Style::new().fg(SUBTEXT),
            ),
        ]),
    ];
    if !session.state_is_current {
        lines.push(Line::from(vec![
            preview_label("OBSERVED"),
            Span::styled(
                session.state.label(),
                Style::new()
                    .fg(session_state_color(session.state))
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!(
                    "  ·  last known  ·  updated {}",
                    compact_recency(session.last_at_ms)
                ),
                Style::new().fg(SUBTEXT),
            ),
        ]));
    }
    lines.extend([
        preview_field(
            "SESSION",
            format!(
                "{external_identity}  ·  {occurrences} occurrence{}  ·  shell {shell}",
                if occurrences == 1 { "" } else { "s" }
            ),
        ),
        preview_field("ROOT", root_directory),
    ]);
    if agent.root_branch != "-" || agent.root_worktree != "-" {
        lines.push(preview_field(
            "GIT",
            format!(
                "branch {}  ·  worktree {}",
                agent.root_branch, agent.root_worktree
            ),
        ));
    }
    lines.extend([
        preview_field("EVIDENCE", agent.evidence.clone()),
        preview_field(
            "SOURCE",
            format!(
                "{}  ·  confidence {}%",
                agent.authority.label().replace('_', " "),
                agent.confidence
            ),
        ),
    ]);
    let content_height = lines.len() as u16;
    Some(ContextualPreview {
        title: format!(" {} session ", integration_display_name(&agent.integration)),
        content: PreviewContent::Lines(lines),
        content_height,
    })
}

fn preview_field(label: &'static str, value: impl Into<String>) -> Line<'static> {
    Line::from(vec![preview_label(label), Span::raw(value.into())])
}

fn preview_label(label: &'static str) -> Span<'static> {
    Span::styled(format!("{label:<10}"), Style::new().fg(SUBTEXT))
}

fn matched_agent_session<'a>(
    workspace: &'a WorkspaceView,
    agent_shell: &AgentShellView,
) -> Option<&'a AgentSessionView> {
    let agent = agent_shell.agent.as_ref()?;
    workspace
        .sessions
        .iter()
        .filter(|session| {
            session.runs.iter().any(|run| run.agent_id == agent.id)
                || (session.integration == agent.integration
                    && agent.external_session_id.is_some()
                    && session.external_session_id == agent.external_session_id)
        })
        .max_by(|left, right| {
            left.state_is_current
                .cmp(&right.state_is_current)
                .then_with(|| left.last_at_ms.cmp(&right.last_at_ms))
                .then_with(|| left.id.cmp(&right.id))
        })
}

fn render_contextual_preview(frame: &mut Frame, area: Rect, preview: ContextualPreview) {
    let block = Block::bordered()
        .title(preview.title)
        .border_style(Style::new().fg(OVERLAY));
    let PreviewContent::Lines(lines) = preview.content;
    frame.render_widget(Paragraph::new(lines).block(block), area);
}

fn best_session_label(session: &AgentSessionView) -> String {
    if let Some(label) = session_task_label(session) {
        return label.to_owned();
    }
    let identity = session
        .external_session_id
        .as_deref()
        .map(short_id)
        .unwrap_or_else(|| short_id(&session.id));
    session
        .runs
        .iter()
        .rev()
        .find_map(|run| run.shell_name.as_deref())
        .map_or_else(
            || {
                format!(
                    "{} {identity}",
                    integration_display_name(&session.integration)
                )
            },
            |shell| format!("{shell} ({identity})"),
        )
}

fn session_task_label(session: &AgentSessionView) -> Option<&str> {
    let label = session.label.trim();
    (!label.is_empty()
        && !label.eq_ignore_ascii_case(&session.integration)
        && !label.eq_ignore_ascii_case(integration_display_name(&session.integration)))
    .then_some(label)
}

fn integration_display_name(integration: &str) -> &str {
    boomux::integrations::display_name(integration)
}

fn item_column_widths(width: u16, rows: &[[String; 7]]) -> Vec<Constraint> {
    let caps = if width >= 140 {
        [10, 12, 24, 14, 52, 32, 24]
    } else if width >= 100 {
        [10, 11, 18, 12, 36, 24, 20]
    } else {
        [8, 10, 14, 9, 24, 18, 16]
    };
    let minimums = ITEM_TABLE_HEADERS.map(|header| header.len() as u16);
    let mut widths: [u16; 7] = std::array::from_fn(|index| {
        rows.iter()
            .map(|row| row[index].chars().count() as u16)
            .max()
            .unwrap_or(0)
            .max(minimums[index])
            .saturating_add(2)
            .min(caps[index])
    });

    // Six column gaps and the highlight marker also consume table width.
    let available = width.saturating_sub(8);
    let mut overflow = widths.iter().sum::<u16>().saturating_sub(available);
    for index in [4, 6, 5, 2, 3, 1, 0] {
        let reduction = widths[index].saturating_sub(minimums[index]).min(overflow);
        widths[index] -= reduction;
        overflow -= reduction;
    }

    widths.into_iter().map(Constraint::Length).collect()
}

fn schedule_column_widths(width: u16, headers: &[&str], rows: &[Vec<String>]) -> Vec<Constraint> {
    let mut widths: Vec<u16> = headers
        .iter()
        .enumerate()
        .map(|(index, header)| {
            let cap = match *header {
                "NAME" | "TRIGGER" => 24,
                "NEXT" => 13,
                "LAST" => 18,
                "STATE" => 10,
                "WORKSPACE" => 20,
                "INTEGRATION" => 16,
                _ => header.len() as u16,
            };
            rows.iter()
                .filter_map(|row| row.get(index))
                .map(|value| value.chars().count() as u16)
                .max()
                .unwrap_or(0)
                .max(header.len() as u16)
                .saturating_add(2)
                .min(cap)
        })
        .collect();

    // Borders, column gaps, and the highlight marker consume table width. Keep
    // two additional cells free so the final header is not clipped.
    let available = width.saturating_sub(headers.len() as u16 + 5);
    let mut overflow = widths.iter().sum::<u16>().saturating_sub(available);
    for name in [
        "TRIGGER",
        "NAME",
        "WORKSPACE",
        "INTEGRATION",
        "LAST",
        "NEXT",
        "STATE",
    ] {
        let Some(index) = headers.iter().position(|header| *header == name) else {
            continue;
        };
        let reduction = widths[index]
            .saturating_sub(headers[index].len() as u16)
            .min(overflow);
        widths[index] -= reduction;
        overflow -= reduction;
    }

    widths.into_iter().map(Constraint::Length).collect()
}

fn shell_column_widths(width: u16, rows: &[[String; 9]]) -> Vec<Constraint> {
    let caps = if width >= 160 {
        [12, 6, 24, 14, 20, 9, 40, 32, 24]
    } else if width >= 120 {
        [12, 6, 18, 12, 16, 9, 28, 24, 20]
    } else if width >= 90 {
        [11, 5, 14, 10, 13, 8, 20, 18, 16]
    } else {
        [10, 4, 11, 8, 10, 7, 14, 12, 13]
    };
    let minimums = SHELL_TABLE_HEADERS.map(|header| header.len() as u16);
    let mut widths: [u16; 9] = std::array::from_fn(|index| {
        rows.iter()
            .map(|row| row[index].chars().count() as u16)
            .max()
            .unwrap_or(0)
            .max(minimums[index])
            .saturating_add(2)
            .min(caps[index])
    });

    // Eight column gaps and the highlight marker also consume table width.
    let available = width.saturating_sub(10);
    let mut overflow = widths.iter().sum::<u16>().saturating_sub(available);
    for index in [6, 8, 2, 4, 7, 3, 5, 0, 1] {
        let reduction = widths[index].saturating_sub(minimums[index]).min(overflow);
        widths[index] -= reduction;
        overflow -= reduction;
    }

    widths.into_iter().map(Constraint::Length).collect()
}

fn global_column_widths(width: u16) -> Vec<Constraint> {
    let (workspace, name, status, detail, directory_min, directory_max) = if width >= 120 {
        (20, 18, 10, 30, 16, 42)
    } else {
        (12, 12, 8, 12, 8, 42)
    };
    // Four column gaps and the highlight marker also consume table width.
    let fixed = workspace + name + status + detail + 6;
    let directory = width
        .saturating_sub(fixed)
        .clamp(directory_min, directory_max);
    vec![
        Constraint::Length(workspace),
        Constraint::Length(name),
        Constraint::Length(status),
        Constraint::Length(directory),
        Constraint::Length(detail),
    ]
}

fn agent_column_widths(width: u16, rows: &[[String; 9]]) -> Vec<Constraint> {
    let caps = if width >= 160 {
        [10, 9, 24, 14, 16, 16, 52, 36, 24]
    } else if width >= 140 {
        [10, 9, 20, 12, 16, 16, 44, 28, 20]
    } else if width >= 100 {
        [9, 8, 14, 10, 12, 14, 32, 18, 16]
    } else {
        [8, 7, 11, 8, 10, 13, 24, 12, 13]
    };
    let minimums = AGENT_TABLE_HEADERS.map(|header| header.len() as u16);
    let mut widths: [u16; 9] = std::array::from_fn(|index| {
        rows.iter()
            .map(|row| row[index].chars().count() as u16)
            .max()
            .unwrap_or(0)
            .max(minimums[index])
            .saturating_add(2)
            .min(caps[index])
    });

    // Eight column gaps and the highlight marker also consume table width.
    let available = width.saturating_sub(10);
    let mut overflow = widths.iter().sum::<u16>().saturating_sub(available);
    for index in [6, 2, 7, 4, 8, 5, 3, 0, 1] {
        let reduction = widths[index].saturating_sub(minimums[index]).min(overflow);
        widths[index] -= reduction;
        overflow -= reduction;
    }

    widths.into_iter().map(Constraint::Length).collect()
}

fn short_id(id: &str) -> String {
    id.chars().take(8).collect()
}

fn compact_recency(timestamp_ms: u64) -> String {
    let seconds = current_time_ms().saturating_sub(timestamp_ms) / 1_000;
    match seconds {
        0..=59 => "now".into(),
        60..=3_599 => format!("{}m ago", seconds / 60),
        3_600..=86_399 => format!("{}h ago", seconds / 3_600),
        _ => format!("{}d ago", seconds / 86_400),
    }
}

fn current_time_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

fn agent_row_color(state: AgentDisplayState) -> Color {
    match state {
        AgentDisplayState::Untracked => YELLOW,
        AgentDisplayState::Unknown
        | AgentDisplayState::Working
        | AgentDisplayState::Blocked
        | AgentDisplayState::Idle
        | AgentDisplayState::Inactive
        | AgentDisplayState::Done => TEAL,
    }
}

fn session_state_color(state: AgentDisplayState) -> Color {
    match state {
        AgentDisplayState::Blocked => RED,
        AgentDisplayState::Working => TEAL,
        AgentDisplayState::Idle => GREEN,
        AgentDisplayState::Inactive | AgentDisplayState::Untracked => SUBTEXT,
        AgentDisplayState::Done => BLUE,
        AgentDisplayState::Unknown => YELLOW,
    }
}

fn render_footer(frame: &mut Frame, area: ratatui::layout::Rect, app: &App) {
    let line = if let Some(pending) = &app.pending_close {
        let prompt = match pending.target {
            CloseTarget::GlobalWorkspace { .. } => format!(
                " Close global workspace '{}', fan out guarded removal across {} shell(s) and {} launcher(s)?  ",
                pending.name, pending.shell_count, pending.launcher_count
            ),
            CloseTarget::Workspace(_) => format!(
                " Close workspace '{}', terminate {} shell(s), and remove {} launcher(s)?  ",
                pending.name, pending.shell_count, pending.launcher_count
            ),
            CloseTarget::Shell(_) => {
                format!(
                    " Close shell '{}' and terminate its process?  ",
                    pending.name
                )
            }
            CloseTarget::DismissCachedShell(_) => format!(
                " Dismiss cached shell '{}'? Its remote process will not be closed.  ",
                pending.name
            ),
            CloseTarget::Launcher(_) => {
                format!(" Remove launcher '{}'?  ", pending.name)
            }
            CloseTarget::Schedule(_) => format!(
                " Remove schedule '{}' and its persisted prompt and retained execution history?  ",
                pending.name
            ),
            CloseTarget::Execution(_) => format!(
                " Cancel '{}' and terminate its exact managed process tree?  ",
                pending.name
            ),
        };
        Line::from(vec![
            Span::styled(prompt, Style::new().fg(YELLOW).add_modifier(Modifier::BOLD)),
            Span::styled("y", Style::new().fg(RED)),
            Span::styled(" confirm  ", Style::new().fg(SUBTEXT)),
            Span::styled("n/esc", Style::new().fg(GREEN)),
            Span::styled(" cancel", Style::new().fg(SUBTEXT)),
        ])
    } else if let Mode::Rename { target, input } = &app.mode {
        Line::from(vec![
            Span::styled(
                format!(" New {} name: ", target.label()),
                Style::new().fg(YELLOW),
            ),
            Span::styled(format!("{input}_"), Style::new().fg(TEXT)),
            Span::styled("  enter", Style::new().fg(GREEN)),
            Span::raw(" rename  "),
            Span::styled("esc", Style::new().fg(RED)),
            Span::raw(" cancel"),
        ])
    } else if let Some(text) = &app.pending_shell_creation {
        Line::from(Span::styled(
            format!(" {text}"),
            Style::new().fg(YELLOW).add_modifier(Modifier::BOLD),
        ))
    } else if let Some(message) = &app.message {
        Line::from(Span::styled(
            format!(" {}", message.text),
            Style::new().fg(if message.error { RED } else { GREEN }),
        ))
    } else {
        let launcher_selected = matches!(app.selected_item(), Some(WorkspaceItemView::Launcher(_)));
        let offline_shell_selected =
            app.selected_item_location()
                .is_some_and(|(workspace, item)| {
                    app.workspaces[workspace].item_dismissible(item)
                        && matches!(
                            app.workspaces[workspace].items.get(item),
                            Some(WorkspaceItemView::Shell(_) | WorkspaceItemView::AgentShell(_))
                        )
                });
        let dismiss_selected = app.cached_projection_dismissal && offline_shell_selected;
        if app.primary_tab == PrimaryTab::Nodes {
            let mut spans = vec![
                Span::styled(" j/k", Style::new().fg(TEAL)),
                Span::styled(" navigate  ", Style::new().fg(SUBTEXT)),
                Span::styled("a", Style::new().fg(GREEN)),
                Span::styled(" add Node  ", Style::new().fg(SUBTEXT)),
                Span::styled("r", Style::new().fg(BLUE)),
                Span::styled(" retry/refresh  ", Style::new().fg(SUBTEXT)),
            ];
            if app.cached_projection_dismissal {
                spans.extend([
                    Span::styled("u", Style::new().fg(GREEN)),
                    Span::styled(" restore dismissed  ", Style::new().fg(SUBTEXT)),
                ]);
            }
            spans.extend([
                Span::styled("U", Style::new().fg(GREEN)),
                Span::styled(" upgrade  ", Style::new().fg(SUBTEXT)),
            ]);
            spans.extend([
                Span::styled("enter", Style::new().fg(GREEN)),
                Span::styled(" inspect  ", Style::new().fg(SUBTEXT)),
                Span::styled("e", Style::new().fg(YELLOW)),
                Span::styled(" rename  ", Style::new().fg(SUBTEXT)),
                Span::styled("t", Style::new().fg(YELLOW)),
                Span::styled(" retarget  ", Style::new().fg(SUBTEXT)),
                Span::styled("x", Style::new().fg(RED)),
                Span::styled(" forget  ", Style::new().fg(SUBTEXT)),
                Span::styled("tab/shift-tab", Style::new().fg(TEAL)),
                Span::styled(" views  ", Style::new().fg(SUBTEXT)),
                Span::styled("1-5", Style::new().fg(TEAL)),
                Span::styled(" select view  ", Style::new().fg(SUBTEXT)),
                Span::styled("/", Style::new().fg(TEAL)),
                Span::styled(" palette  ", Style::new().fg(SUBTEXT)),
                Span::styled("q", Style::new().fg(RED)),
                Span::styled(" quit", Style::new().fg(SUBTEXT)),
            ]);
            frame.render_widget(Paragraph::new(Line::from(spans)), area);
            return;
        }
        if app.primary_tab == PrimaryTab::Schedules {
            let paused = app
                .selected_schedule()
                .is_some_and(|schedule| schedule.state == ScheduleDisplayState::Paused);
            let active = app
                .selected_execution()
                .is_some_and(|execution| execution.state.is_active());
            let line = Line::from(vec![
                Span::styled(" j/k", Style::new().fg(TEAL)),
                Span::styled(
                    if app.focus == Focus::Items {
                        " execution  "
                    } else {
                        " schedule  "
                    },
                    Style::new().fg(SUBTEXT),
                ),
                Span::styled("←/→", Style::new().fg(TEAL)),
                Span::styled(" panes  ", Style::new().fg(SUBTEXT)),
                Span::styled("[/]", Style::new().fg(TEAL)),
                Span::styled(" execution  ", Style::new().fg(SUBTEXT)),
                Span::styled("Enter", Style::new().fg(GREEN)),
                Span::styled(" open  ", Style::new().fg(SUBTEXT)),
                Span::styled("u", Style::new().fg(GREEN)),
                Span::styled(" run now  ", Style::new().fg(SUBTEXT)),
                Span::styled("p", Style::new().fg(YELLOW)),
                Span::styled(
                    if paused { " resume  " } else { " pause  " },
                    Style::new().fg(SUBTEXT),
                ),
                Span::styled(if active { "c" } else { "-" }, Style::new().fg(RED)),
                Span::styled(" cancel active  ", Style::new().fg(SUBTEXT)),
                Span::styled("e", Style::new().fg(YELLOW)),
                Span::styled(
                    if paused { " edit  " } else { " pause first  " },
                    Style::new().fg(SUBTEXT),
                ),
                Span::styled("a", Style::new().fg(GREEN)),
                Span::styled(" create help  ", Style::new().fg(SUBTEXT)),
                Span::styled("x", Style::new().fg(RED)),
                Span::styled(" remove  ", Style::new().fg(SUBTEXT)),
                Span::styled("q", Style::new().fg(RED)),
                Span::styled(" quit", Style::new().fg(SUBTEXT)),
            ]);
            frame.render_widget(Paragraph::new(line), area);
            return;
        }
        if matches!(app.selected_item(), Some(WorkspaceItemView::Schedule(_))) {
            frame.render_widget(
                Paragraph::new(Line::from(vec![
                    Span::styled(" j/k", Style::new().fg(TEAL)),
                    Span::styled(" navigate  ", Style::new().fg(SUBTEXT)),
                    Span::styled("Enter", Style::new().fg(GREEN)),
                    Span::styled(" inspect schedule  ", Style::new().fg(SUBTEXT)),
                    Span::styled("x", Style::new().fg(RED)),
                    Span::styled(" remove  ", Style::new().fg(SUBTEXT)),
                    Span::styled("e", Style::new().fg(YELLOW)),
                    Span::styled(" edit  ", Style::new().fg(SUBTEXT)),
                    Span::styled("/", Style::new().fg(TEAL)),
                    Span::styled(" palette  ", Style::new().fg(SUBTEXT)),
                    Span::styled("q", Style::new().fg(RED)),
                    Span::styled(" quit", Style::new().fg(SUBTEXT)),
                ])),
                area,
            );
            return;
        }
        if matches!(app.selected_item(), Some(WorkspaceItemView::AgentShell(agent)) if agent.schedule_id.is_some())
        {
            frame.render_widget(
                Paragraph::new(Line::from(vec![
                    Span::styled(" j/k", Style::new().fg(TEAL)),
                    Span::styled(" navigate  ", Style::new().fg(SUBTEXT)),
                    Span::styled("schedule-owned Agent", Style::new().fg(YELLOW)),
                    Span::styled(
                        "; use Schedules for exact open/cancel  ",
                        Style::new().fg(SUBTEXT),
                    ),
                    Span::styled("/", Style::new().fg(TEAL)),
                    Span::styled(" palette  ", Style::new().fg(SUBTEXT)),
                    Span::styled("q", Style::new().fg(RED)),
                    Span::styled(" quit", Style::new().fg(SUBTEXT)),
                ])),
                area,
            );
            return;
        }
        let mut spans = vec![
            Span::styled(" j/k", Style::new().fg(TEAL)),
            Span::styled(
                if app.primary_tab == PrimaryTab::Workspaces {
                    " navigate  tab/shift-tab views  h/l panes  "
                } else {
                    " navigate  tab/shift-tab views  1-5 select view  "
                },
                Style::new().fg(SUBTEXT),
            ),
            Span::styled("/", Style::new().fg(TEAL)),
            Span::styled(" palette  ", Style::new().fg(SUBTEXT)),
            Span::styled("?", Style::new().fg(BLUE)),
            Span::styled(" help  ", Style::new().fg(SUBTEXT)),
        ];
        if app.follow_focused_terminal {
            spans.extend([
                Span::styled(
                    if app.selection_pinned {
                        "PINNED"
                    } else {
                        "space"
                    },
                    Style::new()
                        .fg(if app.selection_pinned { YELLOW } else { BLUE })
                        .add_modifier(if app.selection_pinned {
                            Modifier::BOLD
                        } else {
                            Modifier::empty()
                        }),
                ),
                Span::styled(
                    if app.selection_pinned {
                        " unpin  "
                    } else {
                        " pin selection  "
                    },
                    Style::new().fg(SUBTEXT),
                ),
            ]);
        }
        if app.primary_tab == PrimaryTab::Workspaces {
            if app.focus == Focus::Workspaces
                && let Some(workspace) = app.selected()
                && matches!(
                    workspace.coordination,
                    WorkspaceCoordinationView::Global { closing: false, .. }
                )
            {
                let selected = app.selected_workspace_id.as_deref() == Some(&workspace.id);
                spans.extend([
                    Span::styled(
                        if selected { "DEFAULT" } else { "s" },
                        Style::new()
                            .fg(if selected { YELLOW } else { GREEN })
                            .add_modifier(if selected {
                                Modifier::BOLD
                            } else {
                                Modifier::empty()
                            }),
                    ),
                    Span::styled(
                        if selected {
                            " selected  "
                        } else {
                            " set default  "
                        },
                        Style::new().fg(SUBTEXT),
                    ),
                ]);
            }
            spans.extend([
                Span::styled("a", Style::new().fg(GREEN)),
                Span::styled(
                    if app.focus == Focus::Workspaces {
                        " create workspace  "
                    } else {
                        " add shell  "
                    },
                    Style::new().fg(SUBTEXT),
                ),
            ]);
            if app.focus == Focus::Workspaces
                && app.selected().is_some_and(|workspace| {
                    matches!(
                        workspace.coordination,
                        WorkspaceCoordinationView::External {
                            available: true,
                            ..
                        }
                    )
                })
            {
                spans.extend([
                    Span::styled("d", Style::new().fg(GREEN)),
                    Span::styled(" adopt as new  ", Style::new().fg(SUBTEXT)),
                    Span::styled("L", Style::new().fg(GREEN)),
                    Span::styled(" link existing  ", Style::new().fg(SUBTEXT)),
                ]);
            }
        }
        spans.extend([
            Span::styled("e", Style::new().fg(YELLOW)),
            Span::styled(
                if app.primary_tab == PrimaryTab::Workspaces && app.focus == Focus::Workspaces {
                    " rename workspace  "
                } else if launcher_selected {
                    " rename launcher  "
                } else {
                    " rename shell  "
                },
                Style::new().fg(SUBTEXT),
            ),
            Span::styled("enter", Style::new().fg(GREEN)),
            Span::styled(
                if app.primary_tab == PrimaryTab::Workspaces && app.focus == Focus::Workspaces {
                    " restore workspace  "
                } else if launcher_selected {
                    " launch command  "
                } else {
                    " open shell  "
                },
                Style::new().fg(SUBTEXT),
            ),
        ]);
        if app.terminal_preview_is_available() {
            spans.extend([
                Span::styled("pgup/dn", Style::new().fg(TEAL)),
                Span::styled(" output  ", Style::new().fg(SUBTEXT)),
                Span::styled("home/end", Style::new().fg(TEAL)),
                Span::styled(" oldest/follow  ", Style::new().fg(SUBTEXT)),
            ]);
        }
        spans.extend([
            Span::styled("r", Style::new().fg(BLUE)),
            Span::styled(" refresh  ", Style::new().fg(SUBTEXT)),
        ]);
        if !offline_shell_selected || app.cached_projection_dismissal {
            spans.extend([
                Span::styled("x", Style::new().fg(RED)),
                Span::styled(
                    if app.primary_tab == PrimaryTab::Workspaces && app.focus == Focus::Workspaces {
                        " close workspace  "
                    } else if launcher_selected {
                        " remove launcher  "
                    } else if dismiss_selected {
                        " dismiss cached shell  "
                    } else {
                        " close shell  "
                    },
                    Style::new().fg(SUBTEXT),
                ),
            ]);
        }
        spans.extend([
            Span::styled("q", Style::new().fg(RED)),
            Span::styled(" quit", Style::new().fg(SUBTEXT)),
        ]);
        Line::from(spans)
    };
    frame.render_widget(Paragraph::new(line), area);
}

fn status_color(status: &str) -> Color {
    match status.split_whitespace().next().unwrap_or(status) {
        "pending" | "untracked" => YELLOW,
        "exited" | "exit" | "terminated" | "interrupted" => SUBTEXT,
        _ => TEAL,
    }
}

#[cfg(test)]
mod tests {

    use super::*;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn app() -> App {
        App::new(vec![workspace("w1", "boomux")], project_context())
    }

    #[test]
    fn blocked_refresh_does_not_delay_navigation_and_remains_single_flight() {
        let mut app = app();
        let mut second = app.workspaces[0].items[0].clone();
        let WorkspaceItemView::Shell(shell) = &mut second else {
            unreachable!();
        };
        shell.id = "term_2".into();
        shell.name = "second".into();
        app.workspaces[0].items.push(second);
        focus_items(&mut app);
        assert_eq!(app.item_state.selected(), Some(0));

        let (started_sender, started_receiver) = mpsc::channel();
        let (release_sender, release_receiver) = mpsc::channel();
        let mut runtime = DashboardRuntime::spawn(move |effect: DashboardEffect| {
            started_sender.send(effect.clone()).unwrap();
            release_receiver.recv().unwrap();
            match effect {
                DashboardEffect::CheckForUpdates => DashboardEvent::UpdateCheckCompleted,
                effect => panic!("unexpected effect: {effect:?}"),
            }
        });

        assert!(
            !runtime
                .dispatch(vec![DashboardEffect::CheckForUpdates])
                .unwrap()
        );
        assert_eq!(
            started_receiver
                .recv_timeout(Duration::from_secs(1))
                .unwrap(),
            DashboardEffect::CheckForUpdates
        );
        assert!(
            !runtime
                .dispatch(vec![DashboardEffect::CheckForUpdates])
                .unwrap()
        );

        let effects = app.update(DashboardEvent::KeyPressed {
            code: KeyCode::Down,
            modifiers: KeyModifiers::NONE,
        });
        assert!(effects.is_empty());
        assert_eq!(app.item_state.selected(), Some(1));

        release_sender.send(()).unwrap();
        let deadline = Instant::now() + Duration::from_secs(1);
        while runtime.update_check_in_flight && Instant::now() < deadline {
            runtime.drain(&mut app).unwrap();
            thread::sleep(Duration::from_millis(1));
        }
        assert!(!runtime.update_check_in_flight);
        assert!(matches!(
            started_receiver.try_recv(),
            Err(mpsc::TryRecvError::Empty)
        ));
    }

    #[test]
    fn mutation_must_finish_before_dashboard_can_quit() {
        let mut app = app();
        let (started_sender, started_receiver) = mpsc::channel();
        let (release_sender, release_receiver) = mpsc::channel();
        let mut runtime = DashboardRuntime::spawn(move |effect: DashboardEffect| match effect {
            DashboardEffect::Rename { .. } => {
                started_sender.send(()).unwrap();
                release_receiver.recv().unwrap();
                DashboardEvent::OperationCompleted(Ok("renamed".into()))
            }
            DashboardEffect::Refresh => DashboardEvent::RefreshCompleted(Err("ignored".into())),
            effect => panic!("unexpected effect: {effect:?}"),
        });

        runtime
            .dispatch(vec![DashboardEffect::Rename {
                target: RenameTarget::Workspace("workspace".into()),
                name: "renamed".into(),
            }])
            .unwrap();
        started_receiver
            .recv_timeout(Duration::from_secs(1))
            .unwrap();
        assert!(!runtime.can_quit());

        release_sender.send(()).unwrap();
        let deadline = Instant::now() + Duration::from_secs(1);
        while !runtime.can_quit() && Instant::now() < deadline {
            runtime.drain(&mut app).unwrap();
            thread::sleep(Duration::from_millis(1));
        }
        assert!(runtime.can_quit());
    }

    #[test]
    fn bomb_animation_advances_through_fuse_and_explosion_frames() {
        assert_eq!(
            bomb_animation_frame(Duration::ZERO),
            BombAnimationFrame::Fuse(0)
        );
        assert_eq!(
            bomb_animation_frame(FUSE_FRAME_DURATION - Duration::from_millis(1)),
            BombAnimationFrame::Fuse(0)
        );
        assert_eq!(
            bomb_animation_frame(FUSE_FRAME_DURATION),
            BombAnimationFrame::Fuse(1)
        );
        assert_eq!(
            bomb_animation_frame(FUSE_FRAME_DURATION * 13),
            BombAnimationFrame::Fuse(13)
        );

        let fuse_duration = FUSE_FRAME_DURATION * FUSE_FRAME_COUNT as u32;
        assert_eq!(
            bomb_animation_frame(fuse_duration),
            BombAnimationFrame::Explosion(0)
        );
        assert_eq!(
            bomb_animation_frame(fuse_duration + EXPLOSION_FRAME_DURATION * 10),
            BombAnimationFrame::Explosion(10)
        );
        assert_eq!(
            bomb_animation_frame(
                fuse_duration + EXPLOSION_FRAME_DURATION * EXPLOSION_FRAME_COUNT as u32
            ),
            BombAnimationFrame::Finished
        );
    }

    #[test]
    fn fuse_burns_along_its_path_toward_the_bomb() {
        assert_eq!(fuse_burn_progress(0), 0.0);
        assert_eq!(fuse_burn_progress(HOP_FRAME_COUNT - 1), 0.0);
        assert_eq!(fuse_burn_progress(FUSE_FRAME_COUNT - 1), 1.0);
        assert!(fuse_burn_progress(30) < fuse_burn_progress(45));
    }

    #[test]
    fn entrance_uses_two_upright_diminishing_hops() {
        assert!(hop_height(0.275) > hop_height(0.775));
        assert!(hop_height(0.275) > 6.5);
        assert!(hop_height(0.775) > 3.0);
        assert!(hop_height(0.0).abs() < 1e-12);
        assert!(hop_height(1.0).abs() < 1e-12);
    }

    #[test]
    fn smoke_wordmark_is_a_clear_filled_five_row_glyph() {
        let lines = smoke_word_lines(FIREBALL_FRAME_COUNT);
        let text = lines
            .iter()
            .flat_map(|line| line.spans.iter())
            .map(|span| span.content.as_ref())
            .collect::<String>();

        assert_eq!(lines.len(), BOOMUX_SMOKE.len());
        assert!(
            text.chars()
                .filter(|cell| matches!(cell, '▓' | '▒'))
                .count()
                > 60
        );
        assert!(!text.contains('○'));
    }

    #[test]
    fn bomb_animation_now_lasts_long_enough_to_read() {
        let duration = FUSE_FRAME_DURATION * FUSE_FRAME_COUNT as u32
            + EXPLOSION_FRAME_DURATION * EXPLOSION_FRAME_COUNT as u32;
        assert!(duration >= Duration::from_secs(4));
    }

    #[test]
    fn bomb_animation_renders_centered_at_common_terminal_size() {
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();

        terminal
            .draw(|frame| render_bomb_animation(frame, BombAnimationFrame::Fuse(HOP_FRAME_COUNT)))
            .unwrap();

        let buffer = terminal.backend().buffer();
        let text: String = buffer.content().iter().map(|cell| cell.symbol()).collect();
        assert_eq!(text.matches('•').count(), 2);
        assert!(text.contains("press any key to skip"));
        let non_blank_cells = buffer
            .content()
            .iter()
            .filter(|cell| cell.symbol() != " ")
            .count();
        assert!(
            non_blank_cells > 60,
            "bomb should remain visible without dominating the terminal"
        );
    }

    #[test]
    fn explosion_renders_blast_then_smoke() {
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();

        terminal
            .draw(|frame| render_bomb_animation(frame, BombAnimationFrame::Explosion(8)))
            .unwrap();
        let blast_cells = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .filter(|cell| cell.symbol() != " ")
            .count();

        terminal
            .draw(|frame| render_bomb_animation(frame, BombAnimationFrame::Explosion(20)))
            .unwrap();
        let smoke_cells = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .filter(|cell| cell.symbol() != " ")
            .count();

        terminal
            .draw(|frame| render_bomb_animation(frame, BombAnimationFrame::Explosion(59)))
            .unwrap();
        let dispersed_cells = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .filter(|cell| cell.symbol() != " ")
            .count();

        assert!(blast_cells > 70);
        assert!(smoke_cells > 70);
        assert!(dispersed_cells < smoke_cells);
    }

    #[test]
    fn fireball_cuts_directly_to_the_smoke_wordmark() {
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();

        terminal
            .draw(|frame| {
                render_bomb_animation(frame, BombAnimationFrame::Explosion(FIREBALL_FRAME_COUNT))
            })
            .unwrap();
        let text: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect();

        assert!(text.contains('▓'));
    }

    fn project_context() -> ProjectContext {
        ProjectContext {
            projects: vec![
                ProjectView {
                    name: "alpha".into(),
                    path: "/tmp/alpha".into(),
                    group: "Projects".into(),
                    group_order: 0,
                },
                ProjectView {
                    name: "boomux".into(),
                    path: "/tmp/boomux".into(),
                    group: "Work".into(),
                    group_order: 1,
                },
            ],
            config_path: Some("/tmp/config.toml".into()),
            warning: None,
            roots_configured: true,
        }
    }

    fn workspace(id: &str, name: &str) -> WorkspaceView {
        WorkspaceView {
            node: NodeView {
                id: String::new(),
                alias: "local".into(),
                local: true,
                route: None,
                registration_revision: None,
                health: NodeProjectionHealthCode::Online,
                current: true,
                stale: false,
                observed_at_ms: current_time_ms(),
                observed_protocol_version: Some(crate::protocol::PROTOCOL_VERSION),
                observed_helper_version: Some(env!("CARGO_PKG_VERSION").into()),
                observed_capabilities: Vec::new(),
                workspace_owner_eligible: true,
                workspace_owner_unavailable_reason: None,
                scheduler: SchedulerHealth {
                    state: crate::protocol::SchedulerState::Active,
                    max_concurrent: 4,
                    active_executions: 0,
                },
            },
            id: id.into(),
            name: name.into(),
            default_cwd: None,
            items: vec![WorkspaceItemView::Shell(TerminalView {
                id: "term_1".into(),
                name: "agent".into(),
                status: "running".into(),
                directory: "/tmp/boomux".into(),
                repository: "boomux".into(),
                branch: "main".into(),
                git_state: "clean".into(),
                worktree: "primary".into(),
                foreground_process: Some("bash".into()),
                kind: TerminalKind::Shell,
                command: String::new(),
                argv: Vec::new(),
                run: None,
            })],
            sessions: Vec::new(),
            agent_state_counts: AgentStateCounts::default(),
            attention_count: 0,
            attention: Vec::new(),
            item_owners: Vec::new(),
            coordination: WorkspaceCoordinationView::External {
                owner_revision: 1,
                available: true,
            },
        }
    }

    fn set_shell_id(workspace: &mut WorkspaceView, shell_id: &str) {
        match &mut workspace.items[0] {
            WorkspaceItemView::Shell(shell) => shell.id = shell_id.into(),
            WorkspaceItemView::AgentShell(agent) => agent.shell.id = shell_id.into(),
            WorkspaceItemView::Launcher(_) | WorkspaceItemView::Schedule(_) => {
                panic!("expected shell item")
            }
        }
    }

    fn agent() -> AgentView {
        AgentView {
            id: "agent-active".into(),
            state: AgentDisplayState::Working,
            integration: "opencode".into(),
            external_session_id: Some("external-active".into()),
            authority: AgentAuthorityDisplay::LifecycleIntegration,
            confidence: 95,
            evidence: "tool call in progress".into(),
            updated_at_ms: current_time_ms(),
            root_branch: "feat/agents".into(),
            root_worktree: "linked:agents".into(),
        }
    }

    fn agent_shell() -> AgentShellView {
        AgentShellView {
            shell: TerminalView {
                id: "term_1".into(),
                name: "agent".into(),
                status: "running".into(),
                directory: "/tmp/boomux".into(),
                repository: "boomux".into(),
                branch: "main".into(),
                git_state: "clean".into(),
                worktree: "primary".into(),
                foreground_process: Some("opencode".into()),
                kind: TerminalKind::Shell,
                command: String::new(),
                argv: Vec::new(),
                run: None,
            },
            agent: Some(agent()),
            schedule_id: None,
        }
    }

    fn session(id: &str, state: AgentDisplayState) -> AgentSessionView {
        AgentSessionView {
            id: id.into(),
            label: "OpenCode review".into(),
            integration: "opencode".into(),
            external_session_id: Some(format!("external-{id}")),
            state,
            state_is_current: true,
            last_at_ms: 30,
            source_cwd: Some("/tmp/boomux".into()),
            runs: vec![AgentSessionRunView {
                agent_id: format!("agent-{id}"),
                shell_name: Some("agent".into()),
                directory: Some("/tmp/boomux".into()),
            }],
        }
    }

    fn execution(id: &str, state: ExecutionDisplayState) -> ExecutionView {
        ExecutionView {
            id: id.into(),
            state,
            reason: None,
            outcome: None,
            requested_at_ms: current_time_ms(),
            shell_id: Some("schedule-shell".into()),
            run_id: Some("schedule-run".into()),
            agent_id: Some("schedule-agent".into()),
            agent_state: Some(AgentDisplayState::Working),
            session_id: Some("schedule-session".into()),
        }
    }

    fn schedule_view() -> ScheduleView {
        ScheduleView {
            node_id: String::new(),
            node_alias: "local".into(),
            actionable: true,
            id: "schedule-1".into(),
            workspace_id: "w1".into(),
            workspace: "boomux".into(),
            name: "nightly review".into(),
            integration: "opencode".into(),
            state: ScheduleDisplayState::Paused,
            friendly_trigger: "weekdays 09:30".into(),
            next_occurrence_ms: None,
            executions: vec![execution("execution-1", ExecutionDisplayState::Active)],
            history_truncated: false,
            possible_pruning_boundary: false,
            history_scoped: false,
            history_complete: true,
        }
    }

    #[test]
    fn qualified_selection_and_actions_do_not_cross_colliding_nodes() {
        let mut local = workspace("same-workspace", "same");
        local.node.id = "00000000-0000-0000-0000-000000000001".into();
        let mut remote = local.clone();
        remote.node.id = "00000000-0000-0000-0000-000000000002".into();
        remote.node.alias = "work".into();
        remote.node.local = false;
        remote.node.current = false;
        remote.node.stale = true;
        remote.node.health = NodeProjectionHealthCode::Stale;
        let mut app = App::new(vec![local.clone(), remote.clone()], project_context());
        app.cached_projection_dismissal = true;
        app.workspace_state.select(Some(1));
        app.item_state.select(Some(0));

        app.replace_workspaces(vec![remote, local]);

        assert_eq!(
            app.selected().unwrap().node.id,
            "00000000-0000-0000-0000-000000000002"
        );
        assert_eq!(app.selected_item().unwrap().id(), "term_1");
        assert_eq!(app.restore_selected(), None);
        assert_eq!(app.open_selected_item(), None);
        app.focus = Focus::Items;
        app.request_close();
        assert!(matches!(
            app.pending_close,
            Some(PendingClose {
                target: CloseTarget::DismissCachedShell(ref id),
                ..
            }) if id.node_id == "00000000-0000-0000-0000-000000000002"
                && id.inner_id == "term_1"
        ));
        let rendered = rendered_text(&mut app, 140, 36);
        assert!(rendered.contains("Dismiss cached shell"));
        assert!(rendered.contains("remote process will not be closed"));
        assert!(matches!(
            app.confirm_close(),
            Some(DashboardEffect::Close(CloseTarget::DismissCachedShell(id)))
                if id.node_id == "00000000-0000-0000-0000-000000000002"
                    && id.inner_id == "term_1"
        ));
        app.cached_projection_dismissal = false;
        app.request_close();
        assert!(app.pending_close.is_none());
        let rendered = rendered_text(&mut app, 140, 36);
        assert!(!rendered.contains("dismiss cached shell"));
        assert!(!rendered.contains("close shell"));

        assert_eq!(app.workspaces.len(), 2);
    }

    #[test]
    fn focus_following_selects_the_exact_remote_node_when_shell_ids_collide() {
        let mut local = workspace("same-workspace", "local");
        local.node.id = "00000000-0000-0000-0000-000000000001".into();
        let mut remote = local.clone();
        remote.node.id = "00000000-0000-0000-0000-000000000002".into();
        remote.node.alias = "work".into();
        remote.node.local = false;
        let mut app = App::new(vec![local, remote], project_context());

        app.enable_focus_following(Some(&FocusedTerminalView {
            revision: 1,
            node_id: Some("00000000-0000-0000-0000-000000000002".into()),
            workspace_id: String::new(),
            shell_id: "term_1".into(),
        }));

        assert_eq!(
            app.selected().map(|workspace| workspace.node.id.as_str()),
            Some("00000000-0000-0000-0000-000000000002")
        );
        assert_eq!(
            app.selected_item().map(WorkspaceItemView::id),
            Some("term_1")
        );
    }

    #[test]
    fn every_remote_health_code_is_unmistakable_wide_and_compact() {
        for health in [
            NodeProjectionHealthCode::Unobserved,
            NodeProjectionHealthCode::Online,
            NodeProjectionHealthCode::Reconnecting,
            NodeProjectionHealthCode::Stale,
            NodeProjectionHealthCode::Unreachable,
            NodeProjectionHealthCode::AuthenticationRequired,
            NodeProjectionHealthCode::IdentityChanged,
            NodeProjectionHealthCode::IdentityConflict,
            NodeProjectionHealthCode::Unsupported,
        ] {
            let mut remote = workspace("same-workspace", "same");
            remote.node.id = "00000000-0000-0000-0000-000000000002".into();
            remote.node.alias = "work".into();
            remote.node.local = false;
            remote.node.health = health;
            remote.node.current = health == NodeProjectionHealthCode::Online;
            remote.node.stale = health != NodeProjectionHealthCode::Online;
            let mut wide = App::new(vec![remote.clone()], project_context());
            let mut compact = App::new(vec![remote], project_context());
            let label = node_health_label(health);
            let wide = rendered_text(&mut wide, 180, 24);
            let compact = rendered_text(&mut compact, 60, 20);
            assert!(
                wide.contains(&format!("[work {label}")),
                "{health:?}: {wide}"
            );
            assert!(
                compact.contains(&format!("[work {label}")),
                "{health:?}: {compact}"
            );
        }
    }

    #[test]
    fn nodes_tab_renders_route_protocol_health_and_scheduler() {
        let mut remote = workspace("remote-workspace", "remote");
        remote.node.id = "00000000-0000-0000-0000-000000000002".into();
        remote.node.alias = "work".into();
        remote.node.local = false;
        remote.node.route = Some("user@workbox".into());
        remote.node.health = NodeProjectionHealthCode::Reconnecting;
        remote.node.observed_protocol_version = Some(38);
        remote.node.observed_helper_version = Some("0.17.2".into());
        remote.node.observed_capabilities = vec!["global_workspaces".into()];
        let mut app = App::new(vec![remote], project_context());
        app.select_tab(PrimaryTab::Nodes);
        assert_eq!(app.request_add(), Some(DashboardEffect::AddNode));

        let rendered = rendered_text(&mut app, 180, 24);
        for expected in [
            "ALIAS",
            "VERSION",
            "HEALTH",
            "ROUTE",
            "PROTOCOL",
            "SCHEDULER",
            "work",
            "reconnecting",
            "user@workbox",
            "38",
            "0.17.2",
            "global_workspaces",
            "add Node",
            "retry/refresh",
            "upgrade",
        ] {
            assert!(
                rendered.contains(expected),
                "missing {expected}: {rendered}"
            );
        }
    }

    #[test]
    fn multi_node_workspace_table_omits_node_column_and_qualifies_external_names() {
        let mut local = workspace("global-workspace", "local work");
        local.coordination = WorkspaceCoordinationView::Global {
            revision: 1,
            closing: false,
            placements: Vec::new(),
        };
        let local_external = workspace("local-workspace", "local work");
        let mut remote = workspace("remote-workspace", "remote work");
        remote.node.id = "00000000-0000-0000-0000-000000000002".into();
        remote.node.alias = "work".into();
        remote.node.local = false;
        let mut app = App::new(vec![local, local_external, remote], project_context());
        focus_items(&mut app);

        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal
            .draw(|frame| render_workspaces(frame, frame.area(), &mut app))
            .unwrap();
        let rendered = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(rendered.contains("NAME"));
        assert!(!rendered.contains("NODE"));
        assert!(rendered.contains("local work"));
        assert!(rendered.contains("[local online] local work"));
        assert!(rendered.contains("[work online] remote work"));
    }

    #[test]
    fn first_resource_node_picker_has_no_default_and_disables_unavailable_nodes() {
        let mut global = workspace("global-1", "coordinated");
        global.items.clear();
        global.coordination = WorkspaceCoordinationView::Global {
            revision: 4,
            closing: false,
            placements: Vec::new(),
        };
        let mut local = global.node.clone();
        local.id = "00000000-0000-0000-0000-000000000001".into();
        local.workspace_owner_eligible = true;
        let mut remote = local.clone();
        remote.id = "00000000-0000-0000-0000-000000000002".into();
        remote.alias = "work".into();
        remote.local = false;
        remote.route = Some("user@work".into());
        let mut unavailable = remote.clone();
        unavailable.id = "00000000-0000-0000-0000-000000000003".into();
        unavailable.alias = "offline".into();
        unavailable.workspace_owner_eligible = false;
        unavailable.workspace_owner_unavailable_reason = Some("Node health is unreachable".into());
        let mut app = App::new(vec![global], project_context());
        app.nodes = vec![local, remote, unavailable];
        app.focus = Focus::Items;

        assert_eq!(app.request_add(), None);
        let Mode::SelectWorkspaceNode(picker) = &app.mode else {
            panic!("expected Node placement picker");
        };
        assert_eq!(picker.selected, None);
        let rendered = rendered_text(&mut app, 140, 36);
        assert!(rendered.contains("No Node is preselected"));
        assert!(rendered.contains("offline"));
        assert!(rendered.contains("Node health is unreachable"));

        assert_eq!(app.update_key(KeyCode::Down, KeyModifiers::NONE), None);
        assert_eq!(app.update_key(KeyCode::Down, KeyModifiers::NONE), None);
        let effect = app.update_key(KeyCode::Enter, KeyModifiers::NONE);
        assert!(
            matches!(effect, Some(DashboardEffect::CreateGlobalShell { node_id, .. }) if node_id == "00000000-0000-0000-0000-000000000002")
        );
        assert_eq!(
            app.pending_shell_creation.as_deref(),
            Some("Creating Shell in coordinated on Node work...")
        );
        assert!(
            rendered_text(&mut app, 140, 36)
                .contains("Creating Shell in coordinated on Node work...")
        );
    }

    #[test]
    fn nodes_tab_management_actions_are_exact_and_forget_is_confirmed() {
        let mut remote = workspace("remote-workspace", "remote");
        remote.node.id = "00000000-0000-0000-0000-000000000002".into();
        remote.node.alias = "work".into();
        remote.node.local = false;
        remote.node.route = Some("user@work".into());
        remote.node.registration_revision = Some(7);
        let mut app = App::new(vec![remote], project_context());
        app.select_tab(PrimaryTab::Nodes);
        assert_eq!(app.update_key(KeyCode::Char('u'), KeyModifiers::NONE), None);
        assert!(!rendered_text(&mut app, 180, 24).contains("restore dismissed"));
        app.cached_projection_dismissal = true;

        assert!(matches!(
            app.update_key(KeyCode::Char('r'), KeyModifiers::NONE),
            Some(DashboardEffect::RefreshNode(node_id))
                if node_id == "00000000-0000-0000-0000-000000000002"
        ));
        assert!(matches!(
            app.update_key(KeyCode::Char('u'), KeyModifiers::NONE),
            Some(DashboardEffect::RestoreDismissedShells(node_id))
                if node_id == "00000000-0000-0000-0000-000000000002"
        ));
        assert!(matches!(
            app.update_key(KeyCode::Char('U'), KeyModifiers::NONE),
            Some(DashboardEffect::UpgradeNode(node_id))
                if node_id == "00000000-0000-0000-0000-000000000002"
        ));

        assert_eq!(app.update_key(KeyCode::Enter, KeyModifiers::NONE), None);
        assert!(matches!(app.mode, Mode::InspectNode(_)));
        app.update_key(KeyCode::Esc, KeyModifiers::NONE);
        app.update_key(KeyCode::Char('e'), KeyModifiers::NONE);
        assert!(matches!(
            app.mode,
            Mode::Rename {
                target: RenameTarget::Node {
                    expected_revision: 7,
                    ..
                },
                ..
            }
        ));
        let effect = app.update_key(KeyCode::Char('n'), KeyModifiers::NONE);
        assert_eq!(effect, None);
        let effect = app.update_key(KeyCode::Enter, KeyModifiers::NONE);
        assert!(
            matches!(effect, Some(DashboardEffect::Rename { target: RenameTarget::Node { expected_revision: 7, .. }, name }) if name == "n")
        );

        app.mode = Mode::Normal;
        app.update_key(KeyCode::Char('t'), KeyModifiers::NONE);
        assert!(matches!(
            app.mode,
            Mode::RetargetNode {
                expected_revision: 7,
                ..
            }
        ));
        app.mode = Mode::Normal;
        app.update_key(KeyCode::Char('x'), KeyModifiers::NONE);
        assert!(matches!(app.mode, Mode::ConfirmForgetNode(_)));
        let effect = app.update_key(KeyCode::Char('y'), KeyModifiers::NONE);
        assert!(
            matches!(effect, Some(DashboardEffect::ForgetNode { node_id }) if node_id == "00000000-0000-0000-0000-000000000002")
        );
    }

    #[test]
    fn external_workspace_adopt_and_link_actions_are_explicit_and_revision_guarded() {
        let mut global = workspace("global-1", "canonical");
        global.coordination = WorkspaceCoordinationView::Global {
            revision: 6,
            closing: false,
            placements: Vec::new(),
        };
        let mut external = workspace("owner-1", "external");
        external.node.id = "00000000-0000-0000-0000-000000000002".into();
        external.node.alias = "work".into();
        external.node.local = false;
        external.coordination = WorkspaceCoordinationView::External {
            owner_revision: 9,
            available: true,
        };
        let mut app = App::new(vec![global, external], project_context());
        let external_index = app
            .workspaces
            .iter()
            .position(|workspace| workspace.id == "owner-1")
            .unwrap();
        app.workspace_state.select(Some(external_index));

        assert!(matches!(
            app.adopt_selected_external(),
            Some(DashboardEffect::AdoptExternalWorkspace {
                expected_revision: 9,
                identity: QualifiedIdentity { ref node_id, ref inner_id },
            }) if node_id == "00000000-0000-0000-0000-000000000002" && inner_id == "owner-1"
        ));
        app.link_selected_external();
        let Mode::LinkWorkspace(picker) = &app.mode else {
            panic!("expected guarded link picker");
        };
        assert_eq!(picker.selected, None);
        app.update_key(KeyCode::Down, KeyModifiers::NONE);
        let effect = app.update_key(KeyCode::Enter, KeyModifiers::NONE);
        assert!(matches!(
            effect,
            Some(DashboardEffect::LinkExternalWorkspace {
                workspace_id,
                expected_revision: 6,
                expected_owner_revision: 9,
                ..
            }) if workspace_id == "global-1"
        ));
    }

    #[test]
    fn external_workspace_actions_require_owner_placement_capability() {
        let mut external = workspace("owner-1", "old-peer");
        external.node.local = false;
        external.node.workspace_owner_eligible = false;
        external.node.workspace_owner_unavailable_reason =
            Some("Node does not support coordinated Workspaces".into());
        external.coordination = WorkspaceCoordinationView::External {
            owner_revision: 0,
            available: true,
        };
        let mut app = App::new(vec![external], project_context());
        assert_eq!(app.adopt_selected_external(), None);
        app.link_selected_external();
        assert!(matches!(
            app.message,
            Some(Message { ref text, error: true })
                if text.contains("does not support coordinated Workspaces")
        ));
    }

    #[test]
    fn closing_global_workspace_enter_retries_unresolved_close() {
        let mut global = workspace("global-1", "closing");
        global.coordination = WorkspaceCoordinationView::Global {
            revision: 8,
            closing: true,
            placements: Vec::new(),
        };
        let mut app = App::new(vec![global], project_context());
        assert!(matches!(
            app.update_key(KeyCode::Enter, KeyModifiers::NONE),
            Some(DashboardEffect::RetryGlobalWorkspaceClose { workspace_id })
                if workspace_id == "global-1"
        ));
    }

    fn schedule_item() -> WorkspaceItemView {
        WorkspaceItemView::Schedule(ScheduleItemView {
            id: "schedule-1".into(),
            name: "nightly review".into(),
            integration: "opencode".into(),
            state: ScheduleDisplayState::Paused,
            friendly_trigger: "weekdays 09:30".into(),
        })
    }

    fn schedule_app() -> App {
        let mut app = app();
        app.schedules = vec![schedule_view()];
        app.scheduling = SchedulingView::Active {
            active: 1,
            maximum: 4,
        };
        app.exact_run_attachment = true;
        app.schedule_editing = true;
        app.select_tab(PrimaryTab::Schedules);
        app
    }

    fn rendered_text(app: &mut App, width: u16, height: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|frame| render(frame, app)).unwrap();
        terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect()
    }

    fn focus_items(app: &mut App) {
        app.set_focus(Focus::Items);
    }

    fn text_preview(text: &str) -> TerminalPreview {
        TerminalPreview {
            lines: text
                .split('\n')
                .map(|line| TerminalPreviewLine {
                    spans: vec![crate::protocol::TerminalPreviewSpan {
                        text: line.to_owned(),
                        style: TerminalStyle::default(),
                    }],
                })
                .collect(),
        }
    }

    fn preview_text(line: &TerminalPreviewLine) -> String {
        line.spans.iter().map(|span| span.text.as_str()).collect()
    }

    fn refresh_terminal_preview(
        app: &mut App,
        read: &mut impl FnMut(&str) -> Result<TerminalPreview, String>,
    ) {
        let Some(DashboardEffect::ReadTerminalPreview {
            shell_id,
            run_id,
            output_revision,
        }) = app.terminal_preview_effect()
        else {
            return;
        };
        let output = read(&shell_id.inner_id);
        app.update(DashboardEvent::TerminalPreviewCompleted {
            shell_id: shell_id.inner_id,
            run_id,
            output_revision,
            output,
        });
    }

    #[test]
    fn focus_following_selects_shell_once_per_revision() {
        let mut one = workspace("w1", "one");
        set_shell_id(&mut one, "s1");
        let mut two = workspace("w2", "two");
        two.items[0] = WorkspaceItemView::AgentShell(agent_shell());
        set_shell_id(&mut two, "s2");
        let mut app = App::new(vec![one, two], project_context());
        let focused = FocusedTerminalView {
            revision: 1,
            node_id: None,
            workspace_id: "w2".into(),
            shell_id: "s2".into(),
        };

        app.enable_focus_following(Some(&focused));
        assert_eq!(
            app.selected().map(|workspace| workspace.id.as_str()),
            Some("w2")
        );
        assert_eq!(app.selected_item().map(WorkspaceItemView::id), Some("s2"));
        assert_eq!(app.focus, Focus::Items);

        app.set_focus(Focus::Workspaces);
        app.previous();
        assert_eq!(
            app.selected().map(|workspace| workspace.id.as_str()),
            Some("w1")
        );
        app.apply_focused_terminal(Some(&focused));
        assert_eq!(
            app.selected().map(|workspace| workspace.id.as_str()),
            Some("w1")
        );

        app.apply_focused_terminal(Some(&FocusedTerminalView {
            revision: 2,
            ..focused.clone()
        }));
        assert_eq!(
            app.selected().map(|workspace| workspace.id.as_str()),
            Some("w2")
        );

        app.apply_focused_terminal(None);
        app.apply_focused_terminal(Some(&FocusedTerminalView {
            revision: 1,
            node_id: None,
            workspace_id: "w1".into(),
            shell_id: "s1".into(),
        }));
        assert_eq!(
            app.selected().map(|workspace| workspace.id.as_str()),
            Some("w2")
        );
        assert_eq!(app.observed_focus_revision, Some(2));
    }

    #[test]
    fn focus_following_stays_in_the_current_global_tab() {
        let mut first_agent = workspace("w1", "first agent");
        first_agent.items[0] = WorkspaceItemView::AgentShell(agent_shell());
        set_shell_id(&mut first_agent, "agent-1");
        let mut first_shell = workspace("w2", "first shell");
        set_shell_id(&mut first_shell, "shell-1");
        let mut second_agent = workspace("w3", "second agent");
        second_agent.items[0] = WorkspaceItemView::AgentShell(agent_shell());
        set_shell_id(&mut second_agent, "agent-2");
        let mut second_shell = workspace("w4", "second shell");
        set_shell_id(&mut second_shell, "shell-2");
        let mut app = App::new(
            vec![first_agent, first_shell, second_agent, second_shell],
            project_context(),
        );
        app.enable_focus_following(None);

        app.select_tab(PrimaryTab::Agents);
        app.apply_focused_terminal(Some(&FocusedTerminalView {
            revision: 1,
            node_id: None,
            workspace_id: "w3".into(),
            shell_id: "agent-2".into(),
        }));
        assert_eq!(app.primary_tab, PrimaryTab::Agents);
        assert_eq!(
            app.selected_item().map(WorkspaceItemView::id),
            Some("agent-2")
        );

        app.apply_focused_terminal(Some(&FocusedTerminalView {
            revision: 2,
            node_id: None,
            workspace_id: "w2".into(),
            shell_id: "shell-1".into(),
        }));
        assert_eq!(app.primary_tab, PrimaryTab::Agents);
        assert_eq!(app.observed_focus_revision, Some(1));
        assert_eq!(
            app.selected_item().map(WorkspaceItemView::id),
            Some("agent-2")
        );

        app.select_tab(PrimaryTab::Shells);
        app.apply_focused_terminal(Some(&FocusedTerminalView {
            revision: 3,
            node_id: None,
            workspace_id: "w4".into(),
            shell_id: "shell-2".into(),
        }));
        assert_eq!(app.primary_tab, PrimaryTab::Shells);
        assert_eq!(
            app.selected_item().map(WorkspaceItemView::id),
            Some("shell-2")
        );

        app.apply_focused_terminal(Some(&FocusedTerminalView {
            revision: 4,
            node_id: None,
            workspace_id: "w1".into(),
            shell_id: "agent-1".into(),
        }));
        assert_eq!(app.primary_tab, PrimaryTab::Shells);
        assert_eq!(app.observed_focus_revision, Some(3));
        assert_eq!(
            app.selected_item().map(WorkspaceItemView::id),
            Some("shell-2")
        );
    }

    #[test]
    fn focus_following_can_be_disabled_and_defers_during_overlays() {
        let mut one = workspace("w1", "one");
        set_shell_id(&mut one, "s1");
        let mut two = workspace("w2", "two");
        set_shell_id(&mut two, "s2");
        let focused = FocusedTerminalView {
            revision: 1,
            node_id: None,
            workspace_id: "w2".into(),
            shell_id: "s2".into(),
        };
        let mut disabled = App::new(vec![one, two], project_context());
        disabled.apply_focused_terminal(Some(&focused));
        assert_eq!(
            disabled.selected().map(|workspace| workspace.id.as_str()),
            Some("w1")
        );

        disabled.enable_focus_following(None);
        disabled.mode = Mode::Help;
        disabled.apply_focused_terminal(Some(&focused));
        assert_eq!(
            disabled.selected().map(|workspace| workspace.id.as_str()),
            Some("w1")
        );
        assert_eq!(disabled.observed_focus_revision, None);
        disabled.mode = Mode::Normal;
        disabled.apply_focused_terminal(Some(&focused));
        assert_eq!(
            disabled.selected().map(|workspace| workspace.id.as_str()),
            Some("w2")
        );
    }

    #[test]
    fn pinned_selection_ignores_focus_revisions_until_unpinned() {
        let mut one = workspace("w1", "one");
        set_shell_id(&mut one, "s1");
        let mut two = workspace("w2", "two");
        set_shell_id(&mut two, "s2");
        let mut app = App::new(vec![one, two], project_context());
        app.enable_focus_following(Some(&FocusedTerminalView {
            revision: 1,
            node_id: None,
            workspace_id: "w1".into(),
            shell_id: "s1".into(),
        }));

        app.toggle_selection_pin();
        app.apply_focused_terminal(Some(&FocusedTerminalView {
            revision: 2,
            node_id: None,
            workspace_id: "w2".into(),
            shell_id: "s2".into(),
        }));
        app.apply_focused_terminal(None);

        assert!(app.selection_pinned);
        assert_eq!(app.observed_focus_revision, Some(1));
        assert_eq!(app.selected_item().map(WorkspaceItemView::id), Some("s1"));

        app.toggle_selection_pin();
        assert_eq!(app.observed_focus_revision, None);
        app.apply_focused_terminal(Some(&FocusedTerminalView {
            revision: 2,
            node_id: None,
            workspace_id: "w2".into(),
            shell_id: "s2".into(),
        }));

        assert!(!app.selection_pinned);
        assert_eq!(app.observed_focus_revision, Some(2));
        assert_eq!(app.selected_item().map(WorkspaceItemView::id), Some("s2"));
    }

    #[test]
    fn selection_pin_is_inert_when_focus_following_is_disabled() {
        let mut app = app();

        app.toggle_selection_pin();

        assert!(!app.selection_pinned);
    }

    #[test]
    fn unresolved_focus_revision_is_retried_after_projection_refresh() {
        let mut one = workspace("w1", "one");
        set_shell_id(&mut one, "s1");
        let focused = FocusedTerminalView {
            revision: 1,
            node_id: None,
            workspace_id: "w2".into(),
            shell_id: "s2".into(),
        };
        let mut app = App::new(vec![one], project_context());
        app.enable_focus_following(Some(&focused));
        assert_eq!(app.observed_focus_revision, None);

        let mut two = workspace("w2", "two");
        set_shell_id(&mut two, "s2");
        app.replace_workspaces(vec![workspace("w1", "one"), two]);
        app.apply_focused_terminal(Some(&focused));

        assert_eq!(app.observed_focus_revision, Some(1));
        assert_eq!(
            app.selected().map(|workspace| workspace.id.as_str()),
            Some("w2")
        );
    }

    fn hinted_agent_shell() -> AgentShellView {
        AgentShellView {
            shell: TerminalView {
                id: "term_1".into(),
                name: "agent".into(),
                status: "running".into(),
                directory: "/tmp/boomux".into(),
                repository: "boomux".into(),
                branch: "main".into(),
                git_state: "clean".into(),
                worktree: "primary".into(),
                foreground_process: Some("opencode".into()),
                kind: TerminalKind::Shell,
                command: String::new(),
                argv: Vec::new(),
                run: None,
            },
            agent: None,
            schedule_id: None,
        }
    }

    fn terminal(id: &str, name: &str, command: &str) -> WorkspaceItemView {
        WorkspaceItemView::Shell(TerminalView {
            id: id.into(),
            name: name.into(),
            status: "running".into(),
            directory: format!("/tmp/{name}"),
            repository: name.into(),
            branch: "main".into(),
            git_state: "clean".into(),
            worktree: "primary".into(),
            foreground_process: Some("bash".into()),
            kind: if command.is_empty() {
                TerminalKind::Shell
            } else {
                TerminalKind::Command
            },
            command: command.into(),
            argv: command.split_whitespace().map(str::to_owned).collect(),
            run: None,
        })
    }

    fn launcher_view(id: &str, name: &str) -> WorkspaceItemView {
        WorkspaceItemView::Launcher(LauncherView {
            id: id.into(),
            name: name.into(),
            directory: format!("/tmp/{name}"),
            repository: name.into(),
            branch: "main".into(),
            git_state: "clean".into(),
            worktree: "primary".into(),
            command: format!("run-{name}"),
            argv: vec![format!("run-{name}")],
        })
    }

    #[test]
    fn navigation_wraps() {
        let mut app = app();

        app.next();
        assert_eq!(app.selected_index(), Some(0));
        app.previous();
        assert_eq!(app.selected_index(), Some(0));
    }

    #[test]
    fn shell_ids_are_shortened_to_eight_characters() {
        assert_eq!(short_id("12345678-1234-1234-1234-123456789abc"), "12345678");
        assert_eq!(short_id("short"), "short");
    }

    #[test]
    fn mixed_item_columns_fit_content_and_shrink_activity_first() {
        let rows = [[
            "launcher",
            "ready",
            "editor",
            "work",
            "zeditor --foreground .",
            "feature/workspace-items",
            "linked:workspace-items",
        ]
        .map(str::to_owned)];
        assert_eq!(
            item_column_widths(180, &rows),
            vec![
                Constraint::Length(10),
                Constraint::Length(8),
                Constraint::Length(8),
                Constraint::Length(6),
                Constraint::Length(24),
                Constraint::Length(25),
                Constraint::Length(24),
            ]
        );
        assert_eq!(
            item_column_widths(70, &rows),
            vec![
                Constraint::Length(8),
                Constraint::Length(8),
                Constraint::Length(8),
                Constraint::Length(6),
                Constraint::Length(8),
                Constraint::Length(16),
                Constraint::Length(8),
            ]
        );
    }

    #[test]
    fn schedule_columns_fit_content_and_shrink_trigger_first() {
        let headers = [
            "NAME",
            "TRIGGER",
            "NEXT",
            "LAST",
            "STATE",
            "WORKSPACE",
            "INTEGRATION",
        ];
        let rows = vec![vec![
            "manual-test".into(),
            "a deliberately long hourly trigger".into(),
            "paused".into(),
            "exited: 0".into(),
            "paused".into(),
            "boomux".into(),
            "opencode".into(),
        ]];
        assert_eq!(
            schedule_column_widths(180, &headers, &rows),
            vec![
                Constraint::Length(13),
                Constraint::Length(24),
                Constraint::Length(8),
                Constraint::Length(11),
                Constraint::Length(8),
                Constraint::Length(11),
                Constraint::Length(13),
            ]
        );
        assert_eq!(
            schedule_column_widths(88, &headers, &rows),
            vec![
                Constraint::Length(13),
                Constraint::Length(12),
                Constraint::Length(8),
                Constraint::Length(11),
                Constraint::Length(8),
                Constraint::Length(11),
                Constraint::Length(13),
            ]
        );
    }

    #[test]
    fn shell_columns_fit_content_and_shrink_process_first() {
        let rows = [[
            "interrupted",
            "#12",
            "edge-datapipe-support",
            "work",
            "integration-tests",
            "command",
            "cargo test --all-targets --release",
            "fix/confluent-direct-download",
            "linked:confluent-direct-download",
        ]
        .map(str::to_owned)];
        assert_eq!(
            shell_column_widths(240, &rows),
            vec![
                Constraint::Length(12),
                Constraint::Length(5),
                Constraint::Length(23),
                Constraint::Length(6),
                Constraint::Length(19),
                Constraint::Length(9),
                Constraint::Length(36),
                Constraint::Length(31),
                Constraint::Length(24),
            ]
        );
        assert_eq!(
            shell_column_widths(80, &rows),
            vec![
                Constraint::Length(10),
                Constraint::Length(4),
                Constraint::Length(9),
                Constraint::Length(6),
                Constraint::Length(7),
                Constraint::Length(7),
                Constraint::Length(7),
                Constraint::Length(12),
                Constraint::Length(8),
            ]
        );
    }

    #[test]
    fn shell_rows_distinguish_processes_and_exit_outcomes() {
        let mut shell = match terminal("shell", "editor", "") {
            WorkspaceItemView::Shell(shell) => shell,
            _ => unreachable!(),
        };
        shell.foreground_process = Some("nvim".into());
        assert_eq!(shell.kind, TerminalKind::Shell);
        assert_eq!(shell.process(), "nvim");

        let mut command = match terminal("command", "tests", "cargo test") {
            WorkspaceItemView::Shell(shell) => shell,
            _ => unreachable!(),
        };
        command.status = "exited".into();
        command.foreground_process = None;
        command.run = Some(TerminalRunView {
            id: "run-3".into(),
            generation: 3,
            started_at_ms: 1,
            ended_at_ms: Some(2),
            exit_reason: Some("exited (1)".into()),
            output_revision: 4,
        });
        assert_eq!(command.kind, TerminalKind::Command);
        assert_eq!(command.process(), "cargo test");
        assert_eq!(command.table_status(), "exit 1");
    }

    #[test]
    fn agent_columns_fit_content_and_shrink_task_first() {
        let rows = [[
            "idle",
            "7h ago",
            "edge-datapipe-support",
            "work",
            "agent",
            "opencode",
            "Check Slack tickets against GitHub releases",
            "fix/confluent-direct-download",
            "primary",
        ]
        .map(str::to_owned)];
        assert_eq!(
            agent_column_widths(240, &rows),
            vec![
                Constraint::Length(8),
                Constraint::Length(9),
                Constraint::Length(23),
                Constraint::Length(6),
                Constraint::Length(7),
                Constraint::Length(10),
                Constraint::Length(45),
                Constraint::Length(31),
                Constraint::Length(15),
            ]
        );
        assert_eq!(
            agent_column_widths(78, &rows),
            vec![
                Constraint::Length(8),
                Constraint::Length(7),
                Constraint::Length(9),
                Constraint::Length(4),
                Constraint::Length(5),
                Constraint::Length(7),
                Constraint::Length(4),
                Constraint::Length(11),
                Constraint::Length(13),
            ]
        );
    }

    #[test]
    fn pending_shells_use_attention_color() {
        assert_eq!(status_color("pending"), YELLOW);
        assert_eq!(status_color("running"), TEAL);
        assert_eq!(status_color("exited"), SUBTEXT);
        assert_eq!(status_color("exit 1"), SUBTEXT);
        assert_eq!(status_color("interrupted"), SUBTEXT);
    }

    #[test]
    fn terminal_navigation_uses_the_focused_table() {
        let mut app = app();

        focus_items(&mut app);
        assert_eq!(app.focus, Focus::Items);
        app.next();

        assert_eq!(app.item_state.selected(), Some(0));
        assert!(matches!(
            app.selected_item(),
            Some(WorkspaceItemView::Shell(shell)) if shell.name == "agent"
        ));
    }

    #[test]
    fn directional_keys_can_select_each_pane() {
        let mut app = app();

        assert!(app.handle_focus_key(KeyCode::Char('l')));
        assert_eq!(app.focus, Focus::Items);
        assert!(app.handle_focus_key(KeyCode::Left));
        assert_eq!(app.focus, Focus::Workspaces);
        assert!(app.handle_focus_key(KeyCode::Right));
        assert_eq!(app.focus, Focus::Items);
        assert!(app.handle_focus_key(KeyCode::Char('h')));
        assert_eq!(app.focus, Focus::Workspaces);
        assert!(!app.handle_focus_key(KeyCode::Char('x')));
    }

    #[test]
    fn tab_and_backtab_cycle_primary_views() {
        let mut app = app();

        app.cycle_tab(false);
        assert_eq!(app.primary_tab, PrimaryTab::Agents);
        app.cycle_tab(false);
        assert_eq!(app.primary_tab, PrimaryTab::Shells);
        app.cycle_tab(true);
        assert_eq!(app.primary_tab, PrimaryTab::Agents);
        app.cycle_tab(true);
        assert_eq!(app.primary_tab, PrimaryTab::Workspaces);
        assert_eq!(app.focus, Focus::Workspaces);
    }

    #[test]
    fn numeric_shortcuts_match_primary_tab_order() {
        assert_eq!(
            ('1'..='5').filter_map(shortcut_tab).collect::<Vec<_>>(),
            PrimaryTab::ALL
        );
        assert_eq!(shortcut_tab('0'), None);
        assert_eq!(shortcut_tab('6'), None);
    }

    #[test]
    fn normal_mode_accepts_shift_backtab_but_rejects_other_modified_keys() {
        assert!(normal_mode_modifiers_supported(
            KeyCode::BackTab,
            KeyModifiers::SHIFT
        ));
        assert!(normal_mode_modifiers_supported(
            KeyCode::BackTab,
            KeyModifiers::NONE
        ));
        assert!(!normal_mode_modifiers_supported(
            KeyCode::Tab,
            KeyModifiers::SHIFT
        ));
        assert!(!normal_mode_modifiers_supported(
            KeyCode::Char('1'),
            KeyModifiers::CONTROL
        ));
    }

    #[test]
    fn direct_tab_selection_selects_first_matching_item_or_none() {
        let mut app = app();

        app.select_tab(PrimaryTab::Agents);
        assert!(app.global_state.selected().is_none());

        app.select_tab(PrimaryTab::Shells);
        assert_eq!(app.primary_tab, PrimaryTab::Shells);
        assert_eq!(app.global_state.selected(), Some(0));
        assert!(matches!(
            app.selected_item(),
            Some(WorkspaceItemView::Shell(shell)) if shell.id == "term_1"
        ));
    }

    #[test]
    fn presentation_kind_counts_are_exclusive() {
        let mut workspace = workspace("w1", "mixed");
        let mut agent_with_command = agent_shell();
        agent_with_command.shell.command = "opencode --continue".into();
        workspace.items = vec![
            terminal("shell-1", "shell", ""),
            terminal("command-1", "clock", "watch date"),
            WorkspaceItemView::AgentShell(agent_with_command),
            launcher_view("launcher-1", "editor"),
        ];

        assert_eq!(workspace.shell_count(), 1);
        assert_eq!(workspace.command_count(), 1);
        assert_eq!(workspace.agent_count(), 1);
        assert_eq!(workspace.launcher_count(), 1);
    }

    #[test]
    fn tabs_render_exclusive_counts_and_workspace_table_renders_names() {
        let backend = TestBackend::new(180, 24);
        let mut terminal_backend = Terminal::new(backend).unwrap();
        let mut mixed = workspace("w1", "mixed");
        mixed.items = vec![
            terminal("shell-1", "shell", ""),
            terminal("command-1", "clock", "watch date"),
            WorkspaceItemView::AgentShell(agent_shell()),
            launcher_view("launcher-1", "editor"),
        ];
        mixed
            .sessions
            .push(session("durable-1", AgentDisplayState::Working));
        mixed.agent_state_counts.blocked = 1;
        mixed.agent_state_counts.done = 1;
        mixed.attention_count = 2;
        let mut app = App::new(vec![mixed], project_context());

        terminal_backend
            .draw(|frame| render(frame, &mut app))
            .unwrap();
        let buffer = terminal_backend.backend().buffer();
        let lines: Vec<String> = (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect()
            })
            .collect();
        let text = lines.join("\n");

        assert!(text.contains("WORKSPACES 1"));
        assert!(text.contains("AGENTS 1"));
        assert!(!text.contains("SESSIONS"));
        assert!(!text.contains("LAUNCHERS 1"));
        assert!(text.contains("SHELLS 1"));
        assert!(!text.contains("COMMANDS 1"));
        assert!(!text.contains("active agents"));
        let workspace_tab = text.find("WORKSPACES 1").expect("workspace tab");
        let agent_tab = text.find("AGENTS 1").expect("agent tab");
        assert!(workspace_tab < agent_tab);
        assert!(!text.contains("NODES:"));
        assert!(!text.contains("NODE:all"));
        assert!(lines.iter().any(|line| line.contains("> mixed")));

        app.select_tab(PrimaryTab::Agents);
        terminal_backend
            .draw(|frame| render(frame, &mut app))
            .unwrap();
        let agent_text: String = terminal_backend
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        assert!(agent_text.contains("AGENTS (1)"));
    }

    #[test]
    fn refresh_keeps_terminal_focus_for_an_empty_workspace() {
        let mut empty = workspace("w1", "empty");
        empty.items.clear();
        let mut app = App::new(vec![empty], project_context());
        focus_items(&mut app);

        let mut refreshed = workspace("w1", "empty");
        refreshed.items.clear();
        app.replace_workspaces(vec![refreshed]);

        assert_eq!(app.focus, Focus::Items);
        assert!(app.item_state.selected().is_none());
    }

    #[test]
    fn refresh_preserves_and_repairs_unified_item_selection() {
        let launcher = || LauncherView {
            id: "launcher-1".into(),
            name: "editor".into(),
            directory: "/tmp/boomux".into(),
            repository: "boomux".into(),
            branch: "main".into(),
            git_state: "clean".into(),
            worktree: "primary".into(),
            command: "zeditor .".into(),
            argv: vec!["zeditor".into(), ".".into()],
        };
        let mut initial = workspace("w1", "boomux");
        initial.items.push(WorkspaceItemView::Launcher(launcher()));
        let mut app = App::new(vec![initial], project_context());
        focus_items(&mut app);
        app.next();
        assert!(matches!(
            app.selected_item(),
            Some(WorkspaceItemView::Launcher(current)) if current.id == "launcher-1"
        ));

        let mut refreshed = workspace("w1", "boomux");
        refreshed.items.push(WorkspaceItemView::Shell(TerminalView {
            id: "term_2".into(),
            name: "tests".into(),
            status: "pending".into(),
            directory: "/tmp/boomux".into(),
            repository: "boomux".into(),
            branch: "main".into(),
            git_state: "clean".into(),
            worktree: "primary".into(),
            foreground_process: None,
            kind: TerminalKind::Shell,
            command: String::new(),
            argv: Vec::new(),
            run: None,
        }));
        refreshed
            .items
            .push(WorkspaceItemView::Launcher(launcher()));
        app.replace_workspaces(vec![refreshed]);
        assert_eq!(app.item_state.selected(), Some(2));
        assert!(matches!(
            app.selected_item(),
            Some(WorkspaceItemView::Launcher(current)) if current.id == "launcher-1"
        ));

        app.replace_workspaces(vec![workspace("w1", "boomux")]);
        assert_eq!(app.item_state.selected(), Some(0));
        assert!(matches!(
            app.selected_item(),
            Some(WorkspaceItemView::Shell(_))
        ));
    }

    #[test]
    fn refresh_preserves_selection_when_hint_morphs_to_durable_agent() {
        let mut initial = workspace("w1", "boomux");
        initial.items[0] = WorkspaceItemView::AgentShell(hinted_agent_shell());
        let mut app = App::new(vec![initial], project_context());
        focus_items(&mut app);

        let mut refreshed = workspace("w1", "boomux");
        refreshed.items[0] = WorkspaceItemView::AgentShell(agent_shell());
        refreshed.items.insert(
            0,
            WorkspaceItemView::Launcher(LauncherView {
                id: "launcher-1".into(),
                name: "editor".into(),
                directory: "/tmp/boomux".into(),
                repository: "boomux".into(),
                branch: "main".into(),
                git_state: "clean".into(),
                worktree: "primary".into(),
                command: "true".into(),
                argv: vec!["true".into()],
            }),
        );
        app.replace_workspaces(vec![refreshed]);

        assert_eq!(app.item_state.selected(), Some(1));
        assert!(matches!(
            app.selected_item(),
            Some(WorkspaceItemView::AgentShell(current))
                if current.shell.id == "term_1" && current.agent.is_some()
        ));
    }

    #[test]
    fn empty_terminal_focus_can_create_the_first_shell() {
        let mut empty = workspace("w1", "empty");
        empty.items.clear();
        let mut app = App::new(vec![empty], project_context());
        focus_items(&mut app);
        assert_eq!(
            app.request_add(),
            Some(DashboardEffect::CreateShell("w1".into()))
        );
    }

    #[test]
    fn project_suggestion_creates_an_empty_workspace_regardless_of_nodes() {
        let mut app = app();
        let mut remote = app.nodes[0].clone();
        remote.id = "remote-node".into();
        remote.alias = "remote".into();
        remote.local = false;
        app.nodes.push(remote);
        assert!(app.request_add().is_none());
        for character in "alp".chars() {
            handle_mode_key(&mut app, KeyCode::Char(character), KeyModifiers::NONE);
        }
        assert_eq!(
            handle_mode_key(&mut app, KeyCode::Enter, KeyModifiers::NONE),
            Some(DashboardEffect::CreateWorkspace {
                name: "alpha".into(),
            })
        );
        assert!(matches!(app.mode, Mode::Normal));
    }

    #[test]
    fn arbitrary_text_creates_trimmed_workspace_name() {
        let mut app = app();
        app.request_add();
        handle_mode_key(&mut app, KeyCode::Tab, KeyModifiers::NONE);
        if let Mode::PickProject(picker) = &mut app.mode {
            picker.query = "  custom workspace  ".into();
            picker.update_matches();
            assert!(picker.selected().is_none());
        }
        assert_eq!(
            handle_mode_key(&mut app, KeyCode::Enter, KeyModifiers::NONE),
            Some(DashboardEffect::CreateWorkspace {
                name: "custom workspace".into(),
            })
        );
    }

    #[test]
    fn by_name_creation_wins_even_when_its_name_matches_a_project() {
        let mut app = app();
        app.request_add();
        handle_mode_key(&mut app, KeyCode::Tab, KeyModifiers::NONE);
        if let Mode::PickProject(picker) = &mut app.mode {
            picker.query = "alpha".into();
            picker.update_matches();
            assert_eq!(picker.mode, WorkspaceCreationMode::ByName);
            assert!(picker.selected().is_none());
        }

        assert_eq!(
            handle_mode_key(&mut app, KeyCode::Enter, KeyModifiers::NONE),
            Some(DashboardEffect::CreateWorkspace {
                name: "alpha".into(),
            })
        );
    }

    #[test]
    fn project_search_matches_subsequences() {
        let mut picker = ProjectPicker::new(&project_context());
        picker.query = "bmx".into();
        picker.update_matches();

        assert_eq!(picker.matches.len(), 1);
        assert_eq!(
            picker.selected().map(|project| project.name.as_str()),
            Some("boomux")
        );
    }

    #[test]
    fn project_search_preserves_root_groups() {
        let mut picker = ProjectPicker::new(&project_context());
        picker.query = "tmp".into();
        picker.update_matches();

        let groups: Vec<_> = picker
            .matches
            .iter()
            .map(|index| picker.projects[*index].group.as_str())
            .collect();
        assert_eq!(groups, ["Projects", "Work"]);
    }

    #[test]
    fn picker_defaults_to_projects_only_when_roots_are_configured() {
        assert_eq!(
            ProjectPicker::new(&project_context()).mode,
            WorkspaceCreationMode::Project
        );

        let mut context = project_context();
        context.roots_configured = false;
        assert_eq!(
            ProjectPicker::new(&context).mode,
            WorkspaceCreationMode::ByName
        );
    }

    #[test]
    fn project_search_ignores_modified_characters() {
        let mut app = app();
        app.request_add();

        handle_mode_key(&mut app, KeyCode::Char('c'), KeyModifiers::CONTROL);

        let Mode::PickProject(picker) = &app.mode else {
            panic!("expected project picker");
        };
        assert!(picker.query.is_empty());
    }

    #[test]
    fn project_search_accepts_shifted_characters() {
        let mut app = app();
        app.request_add();

        handle_mode_key(&mut app, KeyCode::Char('_'), KeyModifiers::SHIFT);

        let Mode::PickProject(picker) = &app.mode else {
            panic!("expected project picker");
        };
        assert_eq!(picker.query, "_");
    }

    #[test]
    fn command_palette_finds_blocked_agents_and_attention() {
        let mut workspace = workspace("w1", "review");
        let mut agent = agent_shell();
        agent.agent.as_mut().unwrap().state = AgentDisplayState::Blocked;
        workspace.items[0] = WorkspaceItemView::AgentShell(agent);
        workspace.attention = vec![WorkspaceAttentionView {
            node_id: String::new(),
            workspace_id: workspace.id.clone(),
            agent_id: "agent-active".into(),
            shell_id: "term_1".into(),
            agent_name: "review-agent".into(),
            reason: AttentionReason::Blocked,
            evidence: "approval required".into(),
            observed_at_ms: 1,
        }];
        workspace.attention_count = 1;
        let mut palette = CommandPalette::new(&[workspace]);

        palette.query = "blocked".into();
        palette.update_matches();
        let labels = palette
            .matches
            .iter()
            .map(|index| palette.entries[*index].label.as_str())
            .collect::<Vec<_>>();
        assert!(labels.contains(&"review / agent"));

        palette.query = "attention approval".into();
        palette.update_matches();
        assert_eq!(palette.matches.len(), 1);
        assert!(matches!(
            palette.selected_command(),
            Some(PaletteCommand::Attention { ref shell_id, .. }) if shell_id == "term_1"
        ));
    }

    #[test]
    fn command_palette_opens_guided_node_setup() {
        let mut palette = CommandPalette::new(&[]);
        palette.query = "add remote node".into();
        palette.update_matches();
        assert!(matches!(
            palette.selected_command(),
            Some(PaletteCommand::AddNode)
        ));
        let mut app = App::new(Vec::new(), project_context());
        assert_eq!(
            execute_palette_command(&mut app, PaletteCommand::AddNode),
            Some(DashboardEffect::AddNode)
        );
    }

    #[test]
    fn command_palette_does_not_fuzzy_match_across_words() {
        let entry = PaletteEntry {
            action_group: PaletteActionGroup::Help,
            kind_group: PaletteKindGroup::Dashboard,
            label: "l o c k e d".into(),
            detail: String::new(),
            keywords: String::new(),
            command: PaletteCommand::CreateWorkspace,
        };

        assert_eq!(palette_match_score(&entry, "blocked"), None);
    }

    #[test]
    fn command_palette_orders_attention_by_global_urgency() {
        let mut completed = workspace("w1", "first");
        completed.attention = vec![WorkspaceAttentionView {
            node_id: String::new(),
            workspace_id: completed.id.clone(),
            agent_id: "completed-agent-id".into(),
            shell_id: "completed-shell".into(),
            agent_name: "completed-agent".into(),
            reason: AttentionReason::Completed,
            evidence: "finished".into(),
            observed_at_ms: 20,
        }];
        let mut blocked = workspace("w2", "second");
        blocked.attention = vec![WorkspaceAttentionView {
            node_id: String::new(),
            workspace_id: blocked.id.clone(),
            agent_id: "blocked-agent-id".into(),
            shell_id: "blocked-shell".into(),
            agent_name: "blocked-agent".into(),
            reason: AttentionReason::Blocked,
            evidence: "approval required".into(),
            observed_at_ms: 10,
        }];
        let mut palette = CommandPalette::new(&[completed, blocked]);

        palette.query = "attention".into();
        palette.update_matches();

        assert!(matches!(
            palette.selected_command(),
            Some(PaletteCommand::Attention { ref workspace_id, .. }) if workspace_id == "w2"
        ));
    }

    #[test]
    fn command_palette_starts_at_the_first_grouped_action() {
        let palette = CommandPalette::new(&[workspace("w1", "boomux")]);
        let selected = palette
            .state
            .selected()
            .and_then(|position| palette.matches.get(position))
            .and_then(|index| palette.entries.get(*index))
            .unwrap();

        assert_eq!(selected.action_group, PaletteActionGroup::GoTo);
        assert_eq!(selected.kind_group, PaletteKindGroup::Workspaces);
        assert_eq!(selected.label, "boomux");
    }

    #[test]
    fn command_palette_accepts_search_input_and_escape() {
        let mut app = app();
        app.open_palette();
        for character in "agent".chars() {
            assert!(
                handle_palette_key(&mut app, KeyCode::Char(character), KeyModifiers::NONE,)
                    .is_none()
            );
        }
        let Mode::Palette(palette) = &app.mode else {
            panic!("palette closed while entering a query");
        };
        assert_eq!(palette.query, "agent");
        assert!(!palette.matches.is_empty());

        assert!(handle_palette_key(&mut app, KeyCode::Esc, KeyModifiers::NONE).is_none());
        assert!(matches!(app.mode, Mode::Normal));
        assert!(normal_mode_modifiers_supported(
            KeyCode::Char('?'),
            KeyModifiers::SHIFT,
        ));
    }

    #[test]
    fn help_slash_opens_the_command_palette() {
        let mut app = app();
        app.mode = Mode::Help;

        handle_help_key(&mut app, KeyCode::Char('/'), KeyModifiers::NONE);

        assert!(matches!(app.mode, Mode::Palette(_)));
    }

    #[test]
    fn palette_open_reuses_typed_item_dispatch() {
        let mut app = app();
        let identity = ItemIdentity {
            workspace_id: "w1".into(),
            item_id: "term_1".into(),
            kind: ItemIdentityKind::Shell,
        };
        assert_eq!(
            execute_palette_command(
                &mut app,
                PaletteCommand::Item {
                    identity: identity.clone(),
                    action: ItemPaletteAction::Open,
                }
            ),
            Some(DashboardEffect::Open(OpenTarget::Shell("term_1".into())))
        );
        assert_eq!(app.focus, Focus::Items);

        assert!(
            execute_palette_command(
                &mut app,
                PaletteCommand::Item {
                    identity: identity.clone(),
                    action: ItemPaletteAction::Rename,
                }
            )
            .is_none()
        );
        assert!(matches!(
            app.mode,
            Mode::Rename {
                target: RenameTarget::Shell(ref id),
                ..
            } if id == "term_1"
        ));

        app.mode = Mode::Normal;
        execute_palette_command(
            &mut app,
            PaletteCommand::Item {
                identity,
                action: ItemPaletteAction::Close,
            },
        );
        assert!(matches!(
            app.pending_close,
            Some(PendingClose {
                target: CloseTarget::Shell(ref id),
                ..
            }) if id == "term_1"
        ));
    }

    #[test]
    fn attention_jump_falls_back_when_shell_is_not_retained() {
        let mut app = app();

        execute_palette_command(
            &mut app,
            PaletteCommand::Attention {
                workspace_id: "w1".into(),
                shell_id: "removed".into(),
                agent_id: "removed-agent".into(),
            },
        );
        assert_eq!(app.focus, Focus::Workspaces);
        assert!(app.message.as_ref().is_some_and(|message| {
            !message.error && message.text.contains("no longer retained")
        }));
    }

    #[test]
    fn attention_jump_reports_when_workspace_is_not_retained() {
        let mut app = app();

        execute_palette_command(
            &mut app,
            PaletteCommand::Attention {
                workspace_id: "removed".into(),
                shell_id: "removed".into(),
                agent_id: "removed-agent".into(),
            },
        );

        assert!(app.message.as_ref().is_some_and(|message| {
            message.error && message.text.contains("workspace is no longer available")
        }));
    }

    #[test]
    fn palette_and_help_render_contextual_overlays() {
        let backend = TestBackend::new(140, 40);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = app();
        app.open_palette();
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        let palette_text: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        assert!(palette_text.contains("Command palette"));
        assert!(palette_text.contains("Create workspace"));
        assert!(palette_text.contains("GO TO"));
        assert!(palette_text.contains("SHELLS"));
        assert!(palette_text.contains("boomux / agent"));

        app.mode = Mode::Help;
        app.set_focus(Focus::Items);
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        let help_text: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        assert!(help_text.contains("Dashboard help"));
        assert!(help_text.contains("shell: agent"));
        assert!(help_text.contains("durable login-shell PTY slot"));
        assert!(help_text.contains("attention"));

        app.workspaces[0].items[0] = WorkspaceItemView::AgentShell(hinted_agent_shell());
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        let untracked_help: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        assert!(untracked_help.contains("agent: agent"));
        assert!(untracked_help.contains("no authoritative report"));
    }

    #[test]
    fn help_renders_state_reference_at_common_terminal_size() {
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = app();
        app.mode = Mode::Help;

        terminal.draw(|frame| render(frame, &mut app)).unwrap();

        let text: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        assert!(text.contains("STATE QUICK REFERENCE"));
        assert!(text.contains("Attention is durable"));
        assert!(text.contains("command palette"));
    }

    #[test]
    fn footer_and_help_show_selection_pin_state() {
        let mut app = app();
        app.enable_focus_following(None);
        let mut terminal = Terminal::new(TestBackend::new(180, 34)).unwrap();

        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        let footer_text: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        assert!(footer_text.contains("space pin selection"));

        app.toggle_selection_pin();
        app.mode = Mode::Help;
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        let pinned_text: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        assert!(pinned_text.contains("unpin selection and resume focused-terminal following"));
    }

    #[test]
    fn workspace_default_action_selects_only_an_active_coordinated_workspace() {
        let mut app = app();
        assert!(
            app.update(DashboardEvent::KeyPressed {
                code: KeyCode::Char('s'),
                modifiers: KeyModifiers::NONE,
            })
            .is_empty()
        );
        app.workspaces[0].coordination = WorkspaceCoordinationView::Global {
            revision: 1,
            closing: true,
            placements: Vec::new(),
        };
        assert!(
            app.update(DashboardEvent::KeyPressed {
                code: KeyCode::Char('s'),
                modifiers: KeyModifiers::NONE,
            })
            .is_empty()
        );
        let WorkspaceCoordinationView::Global { closing, .. } = &mut app.workspaces[0].coordination
        else {
            unreachable!();
        };
        *closing = false;

        assert_eq!(
            app.update(DashboardEvent::KeyPressed {
                code: KeyCode::Char('s'),
                modifiers: KeyModifiers::NONE,
            }),
            vec![DashboardEffect::SelectWorkspace {
                workspace_id: "w1".into(),
            }]
        );
        assert!(
            app.update(DashboardEvent::WorkspaceSelectionCompleted {
                workspace_id: "w1".into(),
                result: Ok("Set boomux as the default Workspace".into()),
            })
            .is_empty()
        );
        assert_eq!(app.selected_workspace_id.as_deref(), Some("w1"));
        assert!(app.message.as_ref().is_some_and(|message| {
            !message.error && message.text == "Set boomux as the default Workspace"
        }));
        assert!(
            app.update(DashboardEvent::KeyPressed {
                code: KeyCode::Char('s'),
                modifiers: KeyModifiers::NONE,
            })
            .is_empty()
        );

        app.message = None;
        let rendered = rendered_text(&mut app, 180, 34);
        assert!(rendered.contains("DEFAULT selected"));
        assert!(help_lines(&app).iter().any(|line| {
            line.to_string()
                .contains("default Workspace for context-free commands")
        }));
    }

    #[test]
    fn failed_workspace_default_action_preserves_the_previous_selection() {
        let mut app = app();
        app.selected_workspace_id = Some("previous".into());

        app.update(DashboardEvent::WorkspaceSelectionCompleted {
            workspace_id: "w1".into(),
            result: Err("selection failed".into()),
        });

        assert_eq!(app.selected_workspace_id.as_deref(), Some("previous"));
        assert!(
            app.message
                .as_ref()
                .is_some_and(|message| { message.error && message.text == "selection failed" })
        );
    }

    #[test]
    fn add_creates_a_shell_from_terminal_focus() {
        let mut app = app();
        focus_items(&mut app);

        assert_eq!(
            app.request_add(),
            Some(DashboardEffect::CreateShell("w1".into()))
        );
        assert!(matches!(app.mode, Mode::Normal));
        assert_eq!(
            app.pending_shell_creation.as_deref(),
            Some("Creating Shell in boomux on Node local...")
        );
        assert_eq!(app.request_add(), None);
    }

    #[test]
    fn shell_creation_completion_replaces_the_pending_indicator_and_refreshes() {
        let mut app = app();
        focus_items(&mut app);
        app.request_add();

        assert_eq!(
            app.update(DashboardEvent::ShellCreationCompleted(Ok(
                "Created first on Node local".into()
            ))),
            vec![DashboardEffect::Refresh]
        );
        assert!(app.pending_shell_creation.is_none());
        assert!(app.message.as_ref().is_some_and(|message| {
            !message.error && message.text == "Created first on Node local"
        }));
    }

    #[test]
    fn enter_on_terminal_opens_only_the_selected_shell() {
        let mut app = app();
        focus_items(&mut app);

        assert_eq!(
            app.update(DashboardEvent::KeyPressed {
                code: KeyCode::Enter,
                modifiers: KeyModifiers::NONE,
            }),
            vec![DashboardEffect::Open(OpenTarget::Shell("term_1".into()))]
        );
    }

    #[test]
    fn completed_operation_updates_model_and_requests_refresh() {
        let mut app = app();

        assert_eq!(
            app.update(DashboardEvent::OperationCompleted(Err("failed".into()))),
            vec![DashboardEffect::Refresh]
        );
        assert!(
            app.message
                .as_ref()
                .is_some_and(|message| { message.error && message.text == "failed" })
        );
    }

    #[test]
    fn idle_refresh_tick_checks_for_updates_without_requesting_a_snapshot() {
        let mut app = app();

        assert_eq!(
            app.update(DashboardEvent::RefreshElapsed),
            vec![DashboardEffect::CheckForUpdates]
        );
        assert!(app.update(DashboardEvent::UpdateCheckCompleted).is_empty());
    }

    #[test]
    fn control_c_produces_quit_effect_in_every_mode() {
        let mut app = app();
        app.mode = Mode::Help;

        assert_eq!(
            app.update(DashboardEvent::KeyPressed {
                code: KeyCode::Char('c'),
                modifiers: KeyModifiers::CONTROL,
            }),
            vec![DashboardEffect::Quit]
        );
    }

    #[test]
    fn enter_on_launcher_invokes_only_the_selected_launcher() {
        let mut app = app();
        app.workspaces[0]
            .items
            .push(WorkspaceItemView::Launcher(LauncherView {
                id: "launcher-1".into(),
                name: "editor".into(),
                directory: "/tmp/boomux".into(),
                repository: "boomux".into(),
                branch: "main".into(),
                git_state: "clean".into(),
                worktree: "primary".into(),
                command: "zeditor .".into(),
                argv: vec!["zeditor".into(), ".".into()],
            }));
        focus_items(&mut app);
        app.next();
        assert_eq!(
            app.open_selected_item(),
            Some(DashboardEffect::Open(OpenTarget::Launcher {
                workspace_id: "w1".into(),
                launcher_id: "launcher-1".into(),
            }))
        );
    }

    #[test]
    fn launcher_row_dispatches_rename_and_remove_targets() {
        let launcher = LauncherView {
            id: "launcher-1".into(),
            name: "editor".into(),
            directory: "/tmp/boomux".into(),
            repository: "boomux".into(),
            branch: "main".into(),
            git_state: "clean".into(),
            worktree: "primary".into(),
            command: "zeditor .".into(),
            argv: vec!["zeditor".into(), ".".into()],
        };
        let mut app = app();
        app.workspaces[0]
            .items
            .push(WorkspaceItemView::Launcher(launcher));
        focus_items(&mut app);
        app.next();

        app.request_rename();
        assert!(matches!(
            app.mode,
            Mode::Rename {
                target: RenameTarget::Launcher(ref id),
                ..
            } if id == "launcher-1"
        ));
        app.mode = Mode::Normal;
        app.request_close();
        assert!(matches!(
            app.pending_close,
            Some(PendingClose {
                target: CloseTarget::Launcher(ref id),
                ..
            }) if id == "launcher-1"
        ));
    }

    #[test]
    fn agent_shell_rows_dispatch_shell_open_rename_and_close_actions() {
        let mut app = app();
        app.workspaces[0].items = vec![WorkspaceItemView::AgentShell(agent_shell())];
        focus_items(&mut app);
        let effect = app.open_selected_item();
        app.request_rename();
        assert!(matches!(
            app.mode,
            Mode::Rename {
                target: RenameTarget::Shell(ref id),
                ..
            } if id == "term_1"
        ));
        app.mode = Mode::Normal;
        app.request_close();

        assert_eq!(
            effect,
            Some(DashboardEffect::Open(OpenTarget::Shell("term_1".into())))
        );
        assert!(matches!(
            app.pending_close,
            Some(PendingClose {
                target: CloseTarget::Shell(ref id),
                ..
            }) if id == "term_1"
        ));
    }

    #[test]
    fn rename_mode_dispatches_the_selected_shell_and_name() {
        let mut app = app();
        focus_items(&mut app);
        app.request_rename();

        for character in ['a', 'p', 'i'] {
            handle_mode_key(&mut app, KeyCode::Char(character), KeyModifiers::NONE);
        }
        assert_eq!(
            handle_mode_key(&mut app, KeyCode::Enter, KeyModifiers::NONE),
            Some(DashboardEffect::Rename {
                target: RenameTarget::Shell("term_1".into()),
                name: "api".into(),
            })
        );
        assert!(matches!(app.mode, Mode::Normal));
    }

    #[test]
    fn rename_mode_dispatches_the_selected_workspace_and_name() {
        let mut app = app();

        app.request_rename();
        assert!(matches!(
            app.mode,
            Mode::Rename {
                target: RenameTarget::Workspace(ref id),
                ..
            } if id == "w1"
        ));
        for character in "renamed".chars() {
            handle_mode_key(&mut app, KeyCode::Char(character), KeyModifiers::NONE);
        }
        assert_eq!(
            handle_mode_key(&mut app, KeyCode::Enter, KeyModifiers::NONE),
            Some(DashboardEffect::Rename {
                target: RenameTarget::Workspace("w1".into()),
                name: "renamed".into(),
            })
        );
    }

    #[test]
    fn restore_keeps_app_active_and_reports_success() {
        let mut app = app();
        assert_eq!(
            app.restore_selected(),
            Some(DashboardEffect::RestoreWorkspace("w1".into()))
        );
        let effects = app.update(DashboardEvent::OperationCompleted(Ok(
            "Restored workspace".into()
        )));
        assert_eq!(effects, vec![DashboardEffect::Refresh]);
        let message = app.message.expect("restore message");
        assert_eq!(message.text, "Restored workspace");
        assert!(!message.error);
    }

    #[test]
    fn closing_a_workspace_requires_confirmation() {
        let mut app = app();

        app.request_close();
        let pending = app.pending_close.as_ref().expect("pending close");
        assert_eq!(pending.target, CloseTarget::Workspace("w1".into()));
        assert_eq!(pending.shell_count, 1);

        app.cancel_close();
        assert!(app.pending_close.is_none());
        app.request_close();
        let effect = app.confirm_close();

        assert_eq!(
            effect,
            Some(DashboardEffect::Close(CloseTarget::Workspace("w1".into())))
        );
        assert!(app.pending_close.is_none());
        app.update(DashboardEvent::OperationCompleted(Ok(
            "Closed workspace".into()
        )));
        let message = app.message.expect("close message");
        assert_eq!(message.text, "Closed workspace");
        assert!(!message.error);
    }

    #[test]
    fn closing_a_shell_uses_terminal_focus() {
        let mut app = app();
        focus_items(&mut app);

        app.request_close();
        let pending = app.pending_close.as_ref().expect("pending close");
        assert_eq!(pending.target, CloseTarget::Shell("term_1".into()));
        assert_eq!(pending.name, "agent");

        assert_eq!(
            app.confirm_close(),
            Some(DashboardEffect::Close(CloseTarget::Shell("term_1".into())))
        );
        assert!(app.pending_close.is_none());
    }

    #[test]
    fn refresh_preserves_the_selected_workspace() {
        let mut app = App::new(
            vec![workspace("w1", "one"), workspace("w2", "two")],
            project_context(),
        );
        app.next();

        app.replace_workspaces(vec![workspace("w2", "two"), workspace("w3", "three")]);

        assert_eq!(
            app.selected().map(|workspace| workspace.id.as_str()),
            Some("w2")
        );
    }

    #[test]
    fn refresh_removes_stale_workspaces_and_repairs_selection() {
        let mut app = App::new(
            vec![workspace("w1", "one"), workspace("w2", "two")],
            project_context(),
        );
        app.next();

        app.replace_workspaces(vec![workspace("w1", "one")]);
        assert_eq!(
            app.selected().map(|workspace| workspace.id.as_str()),
            Some("w1")
        );

        app.replace_workspaces(Vec::new());
        assert!(app.selected().is_none());
    }

    #[test]
    fn dashboard_renders_to_test_backend() {
        let backend = TestBackend::new(120, 34);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = app();

        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        let text: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        assert!(text.contains("ACTIVITY"));
        assert!(text.contains("BRANCH"));
        assert!(!text.contains("term_1"));
        assert!(text.contains("SHELLS"));
        assert!(text.contains("Items: boomux (1)"));
        assert!(!text.contains("DIRTY"));
        assert!(text.contains("WORKTREE"));
    }

    #[test]
    fn dashboard_renders_selected_workspace_launchers() {
        let backend = TestBackend::new(120, 34);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = app();
        app.workspaces[0]
            .items
            .push(WorkspaceItemView::Launcher(LauncherView {
                id: "launcher-12345678".into(),
                name: "editor".into(),
                directory: "/tmp/boomux".into(),
                repository: "boomux".into(),
                branch: "main".into(),
                git_state: "clean".into(),
                worktree: "primary".into(),
                command: "zeditor .".into(),
                argv: vec!["zeditor".into(), ".".into()],
            }));

        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        let text: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        assert!(text.contains("Items: boomux (2)"));
        assert!(!text.contains("Launchers: boomux"));
        assert!(text.contains("ACTIVITY"));
        assert!(text.contains("ready"));
        assert!(text.contains("editor"));
        assert!(text.contains("zeditor ."));
        assert!(text.contains("launcher"));
    }

    #[test]
    fn launcher_focus_scrolls_mixed_workspace_details() {
        let backend = TestBackend::new(120, 34);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = app();
        app.workspaces[0].items.extend((0..20).map(|index| {
            WorkspaceItemView::Launcher(LauncherView {
                id: format!("launcher-{index:08}"),
                name: format!("launcher-{index}"),
                directory: "/tmp/boomux".into(),
                repository: "boomux".into(),
                branch: "main".into(),
                git_state: "clean".into(),
                worktree: "primary".into(),
                command: format!("command-{index}"),
                argv: vec![format!("command-{index}")],
            })
        }));
        focus_items(&mut app);
        assert_eq!(app.focus, Focus::Items);
        for _ in 0..20 {
            app.next();
        }

        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        let text: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        assert_eq!(app.item_state.selected(), Some(20));
        assert!(text.contains("command-19"));
    }

    #[test]
    fn dashboard_renders_one_actionable_agent_shell_row_with_counts_and_details() {
        let backend = TestBackend::new(180, 34);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = app();
        let mut agent_shell = agent_shell();
        agent_shell.shell.name = "keepname".into();
        app.workspaces[0].items[0] = WorkspaceItemView::AgentShell(agent_shell);
        app.workspaces[0]
            .sessions
            .push(session("active", AgentDisplayState::Working));
        focus_items(&mut app);

        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        let text: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect();

        assert_eq!(app.workspaces[0].agent_count(), 1);
        assert_eq!(app.workspaces[0].shell_count(), 0);
        assert!(text.contains("AGENTS 1"));
        assert!(text.contains("SHELLS 0"));
        assert!(text.contains("Items: boomux (1)"));
        assert!(text.contains("KIND"));
        assert!(text.contains("agent"));
        assert!(text.contains("keepname"));
        assert!(text.contains("working"));
        assert!(!text.contains("term_1"));
        assert!(!text.contains("agent-1"));
        assert!(text.contains("OpenCode review"));
        assert!(text.contains("feat/agents"));
        assert!(text.contains("linked:agents"));
        assert!(text.contains("rename shell"));
        assert!(text.contains("open shell"));
        assert!(text.contains("close shell"));
    }

    #[test]
    fn dashboard_renders_command_kind_and_stored_argv() {
        let backend = TestBackend::new(120, 34);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = app();
        let WorkspaceItemView::Shell(command) = &mut app.workspaces[0].items[0] else {
            panic!("expected shell item");
        };
        command.name = "clock".into();
        command.kind = TerminalKind::Command;
        command.command = "watch -n 1 date".into();

        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        let text: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect();

        assert!(text.contains("command"));
        assert!(text.contains("clock"));
        assert!(text.contains("watch -n 1 date"));
    }

    #[test]
    fn compact_dashboard_renders_hinted_agent_without_ids() {
        let backend = TestBackend::new(80, 20);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = app();
        let mut agent_shell = hinted_agent_shell();
        agent_shell.shell.name = "keepname".into();
        app.workspaces[0].items[0] = WorkspaceItemView::AgentShell(agent_shell);
        app.workspaces[0].sessions = vec![session("active", AgentDisplayState::Working)];
        focus_items(&mut app);

        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        let buffer = terminal.backend().buffer();
        let lines: Vec<String> = (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect()
            })
            .collect();

        assert_eq!(app.workspaces[0].agent_count(), 1);
        assert!(!lines.iter().any(|line| line.contains("term_1")));
        assert!(!lines.iter().any(|line| line.contains("agent-1")));
        assert!(lines.iter().any(|line| line.contains("opencode")));
        assert!(lines.iter().any(|line| line.contains("keepname")));
        assert!(lines.iter().any(|line| line.contains("untracked")));
        assert!(!lines.iter().any(|line| line.contains("idle")));
        assert!(!lines.iter().any(|line| line.contains("OpenCode session")));
    }

    #[test]
    fn launcher_only_workspace_focuses_its_detail_pane() {
        let mut app = app();
        app.workspaces[0].items.clear();
        app.workspaces[0]
            .items
            .push(WorkspaceItemView::Launcher(LauncherView {
                id: "launcher".into(),
                name: "editor".into(),
                directory: "/tmp/boomux".into(),
                repository: "boomux".into(),
                branch: "main".into(),
                git_state: "clean".into(),
                worktree: "primary".into(),
                command: "zeditor .".into(),
                argv: vec!["zeditor".into(), ".".into()],
            }));

        focus_items(&mut app);
        assert_eq!(app.focus, Focus::Items);
        assert!(matches!(
            app.selected_item(),
            Some(WorkspaceItemView::Launcher(launcher)) if launcher.name == "editor"
        ));
    }

    #[test]
    fn wide_dashboard_keeps_workspace_summary_and_shell_details() {
        let backend = TestBackend::new(180, 34);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = app();

        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        let text: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        assert!(text.contains("Workspaces (1)"));
        assert!(text.contains("shell    1  command  0"));
        assert!(text.contains("launcher 0  agent    0"));
        assert!(text.contains("working  0  blocked  0"));
        assert!(text.contains("idle     0  done     0"));
        assert!(text.contains("agent"));
        assert!(text.contains("ACTIVITY"));
        assert!(text.contains("main"));
        assert!(!text.contains("REPOSITORY"));
    }

    #[test]
    fn selected_durable_agent_renders_only_its_canonical_session() {
        let backend = TestBackend::new(180, 34);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = app();
        app.workspaces[0].items[0] = WorkspaceItemView::AgentShell(agent_shell());
        focus_items(&mut app);
        let now = current_time_ms();
        let mut active = session("active", AgentDisplayState::Working);
        active.label = "Current work".into();
        active.last_at_ms = now;
        let mut recent = session("recent", AgentDisplayState::Inactive);
        recent.label = "Recent review".into();
        recent.external_session_id = Some("external-active".into());
        recent.state_is_current = false;
        recent.last_at_ms = now + 1;
        let mut week = session("week", AgentDisplayState::Done);
        week.label = "Finished build".into();
        week.state_is_current = false;
        week.last_at_ms = now - 2 * 24 * 60 * 60 * 1_000;
        let mut older = session("older", AgentDisplayState::Inactive);
        older.label = "Dormant review".into();
        older.state_is_current = false;
        older.last_at_ms = now - 8 * 24 * 60 * 60 * 1_000;
        let mut pi = session("pi", AgentDisplayState::Done);
        pi.integration = "pi".into();
        pi.label = "Pi session must be filtered".into();
        app.workspaces[0].sessions = vec![active, recent, week, older, pi];

        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        let buffer = terminal.backend().buffer();
        let lines: Vec<String> = (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect()
            })
            .collect();
        let text = lines.join("\n");

        assert!(text.contains("OpenCode session"));
        assert!(text.contains("Current work"));
        assert!(!text.contains("Recent review"));
        assert!(!text.contains("Dormant review"));
        assert!(!text.contains("Finished build"));
        assert!(!text.contains("Pi session must be filtered"));
        assert!(text.contains("Items: boomux (1)"));
        assert!(
            lines
                .iter()
                .any(|line| line.contains("TASK") && line.contains("Current work"))
        );
        assert!(lines.iter().any(|line| {
            line.contains("STATUS")
                && line.contains("working")
                && line.contains("current")
                && line.contains("updated now")
        }));
        assert!(lines.iter().any(|line| {
            line.contains("SESSION")
                && line.contains("external")
                && line.contains("1 occurrence")
                && line.contains("shell agent")
        }));
        assert!(
            lines
                .iter()
                .any(|line| line.contains("ROOT") && line.contains("/tmp/boomux"))
        );
        assert!(lines.iter().any(|line| {
            line.contains("GIT") && line.contains("feat/agents") && line.contains("linked:agents")
        }));
        assert!(
            lines.iter().any(|line| {
                line.contains("EVIDENCE") && line.contains("tool call in progress")
            })
        );
        assert!(lines.iter().any(|line| {
            line.contains("SOURCE")
                && line.contains("lifecycle integration")
                && line.contains("confidence 95%")
        }));
        assert!(!text.contains("first "));
        assert!(!text.contains("observed "));

        app.workspaces[0].sessions[0].state_is_current = false;
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        let latest_text: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        assert!(latest_text.contains("Recent review"));
        assert!(!latest_text.contains("Current work"));
    }

    #[test]
    fn agent_session_preview_distinguishes_inactive_from_last_observed_idle() {
        let mut workspace = workspace("w1", "boomux");
        workspace.items[0] = WorkspaceItemView::AgentShell(agent_shell());
        let mut catalog = session("catalog", AgentDisplayState::Idle);
        catalog.external_session_id = Some("external-active".into());
        catalog.state_is_current = false;
        catalog.source_cwd = None;
        catalog.runs.clear();
        workspace.sessions = vec![catalog];
        let mut app = App::new(vec![workspace], project_context());
        focus_items(&mut app);
        {
            let WorkspaceItemView::AgentShell(agent_shell) = &mut app.workspaces[0].items[0] else {
                unreachable!();
            };
            let agent = agent_shell.agent.as_mut().expect("durable Agent");
            agent.state = AgentDisplayState::Inactive;
            agent.root_branch = "-".into();
            agent.root_worktree = "-".into();
            agent_shell.shell.status = "pending".into();
        }

        let WorkspaceItemView::AgentShell(agent_shell) = &app.workspaces[0].items[0] else {
            unreachable!();
        };
        let preview = agent_session_preview(&app, agent_shell).expect("session preview");
        let PreviewContent::Lines(lines) = preview.content;
        let text = lines
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert_eq!(preview.content_height, 7);
        assert!(text.contains("STATUS    inactive  ·  resumable"));
        assert!(text.contains("OBSERVED  idle  ·  last known"));
        assert!(text.contains("SESSION   external  ·  0 occurrences  ·  shell catalog only"));
        assert!(text.contains("ROOT      -"));
        assert!(!text.contains("GIT"));

        app.workspaces[0].sessions[0]
            .runs
            .push(AgentSessionRunView {
                agent_id: "agent-active".into(),
                shell_name: None,
                directory: None,
            });
        let WorkspaceItemView::AgentShell(agent_shell) = &app.workspaces[0].items[0] else {
            unreachable!();
        };
        let preview = agent_session_preview(&app, agent_shell).expect("session preview");
        let PreviewContent::Lines(lines) = preview.content;
        let session_line = lines[3]
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<String>();
        assert!(session_line.contains("1 occurrence  ·  shell removed shell"));
    }

    #[test]
    fn current_idle_session_uses_idle_as_the_primary_status() {
        let mut workspace = workspace("w1", "boomux");
        let mut shell = agent_shell();
        shell.agent.as_mut().unwrap().state = AgentDisplayState::Idle;
        workspace.items[0] = WorkspaceItemView::AgentShell(shell);
        let mut current = session("active", AgentDisplayState::Idle);
        current.external_session_id = Some("external-active".into());
        workspace.sessions = vec![current];
        let mut app = App::new(vec![workspace], project_context());
        focus_items(&mut app);

        let WorkspaceItemView::AgentShell(agent_shell) = &app.workspaces[0].items[0] else {
            unreachable!();
        };
        let preview = agent_session_preview(&app, agent_shell).expect("session preview");
        let PreviewContent::Lines(lines) = preview.content;
        let text = lines
            .iter()
            .flat_map(|line| line.spans.iter())
            .map(|span| span.content.as_ref())
            .collect::<String>();

        assert!(text.contains("STATUS    idle  ·  current"));
        assert!(!text.contains("OBSERVED"));
    }

    #[test]
    fn narrow_agent_session_preview_keeps_all_labels_visible() {
        let backend = TestBackend::new(80, 34);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = app();
        let mut shell = agent_shell();
        shell.agent.as_mut().unwrap().state = AgentDisplayState::Inactive;
        shell.shell.status = "pending".into();
        app.workspaces[0].items[0] = WorkspaceItemView::AgentShell(shell);
        let mut historical = session("active", AgentDisplayState::Idle);
        historical.state_is_current = false;
        app.workspaces[0].sessions.push(historical);
        focus_items(&mut app);

        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        let text = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        for label in [
            "TASK", "STATUS", "OBSERVED", "SESSION", "ROOT", "GIT", "EVIDENCE", "SOURCE",
        ] {
            assert!(text.contains(label), "missing {label}");
        }
    }

    #[test]
    fn global_kind_view_aggregates_items_across_workspaces() {
        let backend = TestBackend::new(140, 24);
        let mut terminal_backend = Terminal::new(backend).unwrap();
        let mut one = workspace("w1", "one");
        one.items = vec![terminal("shell-one", "alpha-shell", "")];
        let mut two = workspace("w2", "two");
        two.items = vec![terminal("shell-two", "beta-shell", "")];
        let mut app = App::new(vec![one, two], project_context());
        app.select_tab(PrimaryTab::Shells);

        terminal_backend
            .draw(|frame| render(frame, &mut app))
            .unwrap();
        let text: String = terminal_backend
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect();

        assert_eq!(app.global_item_count(), 2);
        assert!(text.contains("SHELLS (2)"));
        assert!(text.contains("WORKSPACE"));
        assert!(text.contains("PROCESS"));
        assert!(text.contains("WORKTREE"));
        assert!(text.contains("alpha-shell"));
        assert!(text.contains("beta-shell"));
        assert!(text.contains("one"));
        assert!(text.contains("two"));
    }

    #[test]
    fn narrow_global_view_keeps_all_shell_columns_visible() {
        let backend = TestBackend::new(80, 20);
        let mut terminal_backend = Terminal::new(backend).unwrap();
        let mut workspace = workspace("w1", "one");
        workspace.items = vec![terminal("shell-one", "alpha", "")];
        let mut app = App::new(vec![workspace], project_context());
        app.select_tab(PrimaryTab::Shells);

        terminal_backend
            .draw(|frame| render(frame, &mut app))
            .unwrap();
        let text: String = terminal_backend
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect();

        assert!(text.contains("WORKSPACE"));
        assert!(text.contains("PROCESS"));
        assert!(text.contains("WORKTREE"));
        assert!(!text.contains("DETAIL"));
        assert!(text.contains("running"));
        assert!(!text.contains("shell-on"));
    }

    #[test]
    fn narrow_global_agent_view_keeps_all_headers_visible() {
        let backend = TestBackend::new(80, 20);
        let mut terminal_backend = Terminal::new(backend).unwrap();
        let mut workspace = workspace("w1", "one");
        workspace.items = vec![WorkspaceItemView::AgentShell(agent_shell())];
        let mut app = App::new(vec![workspace], project_context());
        app.select_tab(PrimaryTab::Agents);

        terminal_backend
            .draw(|frame| render(frame, &mut app))
            .unwrap();
        let text: String = terminal_backend
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect();

        assert!(text.contains("STATUS"));
        assert!(text.contains("HARNESS"));
        assert!(text.contains("ROOT BRANCH"));
        assert!(text.contains("ROOT WORKTREE"));
    }

    #[test]
    fn refresh_preserves_global_selection_by_workspace_and_item_identity() {
        let mut one = workspace("w1", "one");
        one.items = vec![terminal("same-id", "first", "")];
        let mut two = workspace("w2", "two");
        two.items = vec![terminal("same-id", "second", "")];
        let mut app = App::new(vec![one, two], project_context());
        app.select_tab(PrimaryTab::Shells);
        app.next();

        let mut refreshed_two = workspace("w2", "two");
        refreshed_two.items = vec![
            terminal("new", "new", ""),
            terminal("same-id", "second", ""),
        ];
        let mut refreshed_one = workspace("w1", "one");
        refreshed_one.items = vec![terminal("same-id", "first", "")];
        app.replace_workspaces(vec![refreshed_two, refreshed_one]);

        assert_eq!(app.global_state.selected(), Some(1));
        assert!(matches!(
            app.selected_item(),
            Some(WorkspaceItemView::Shell(shell)) if shell.name == "second"
        ));
        assert_eq!(
            app.selected_item_workspace()
                .map(|workspace| workspace.id.as_str()),
            Some("w2")
        );
    }

    #[test]
    fn global_agent_uses_owning_workspace_session_context() {
        let backend = TestBackend::new(180, 34);
        let mut terminal_backend = Terminal::new(backend).unwrap();
        let mut one = workspace("w1", "one");
        one.items.clear();
        let mut wrong = session("wrong", AgentDisplayState::Working);
        wrong.label = "Wrong workspace session".into();
        one.sessions.push(wrong);
        let mut two = workspace("w2", "two");
        two.items = vec![WorkspaceItemView::AgentShell(agent_shell())];
        let mut right = session("right", AgentDisplayState::Working);
        right.external_session_id = Some("external-active".into());
        right.label = "Owning workspace session".into();
        two.sessions.push(right);
        let mut app = App::new(vec![one, two], project_context());
        app.select_tab(PrimaryTab::Agents);

        terminal_backend
            .draw(|frame| render(frame, &mut app))
            .unwrap();
        let buffer = terminal_backend.backend().buffer();
        let lines: Vec<String> = (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect()
            })
            .collect();
        let text = lines.join("\n");
        let row = lines
            .iter()
            .find(|line| line.contains("working") && line.contains("two"))
            .expect("tracked agent row");

        assert!(text.contains("STATUS"));
        assert!(text.contains("UPDATED"));
        assert!(text.contains("WORKSPACE"));
        assert!(text.contains("SHELL"));
        assert!(text.contains("HARNESS"));
        assert!(text.contains("TASK"));
        assert!(text.contains("ROOT BRANCH"));
        assert!(text.contains("ROOT WORKTREE"));
        assert!(row.contains("Owning workspace session"));
        assert!(row.contains("opencode"));
        assert!(row.contains("feat/agents"));
        assert!(row.contains("linked:agents"));
        assert!(!text.contains("Wrong workspace session"));
        assert!(!text.contains("DETAIL"));
    }

    #[test]
    fn agents_table_leaves_untracked_metadata_unknown() {
        let backend = TestBackend::new(100, 20);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut workspace = workspace("w1", "project");
        workspace.items = vec![WorkspaceItemView::AgentShell(hinted_agent_shell())];
        let mut app = App::new(vec![workspace], project_context());
        app.select_tab(PrimaryTab::Agents);

        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        let lines: Vec<String> = (0..terminal.backend().buffer().area.height)
            .map(|y| {
                (0..terminal.backend().buffer().area.width)
                    .map(|x| terminal.backend().buffer()[(x, y)].symbol())
                    .collect()
            })
            .collect();
        let row = lines
            .iter()
            .find(|line| line.contains("project") && line.contains("agent"))
            .expect("untracked agent row");

        assert!(row.contains("untrack"));
        assert!(row.contains("project"));
        assert!(row.contains("agent"));
        assert!(row.matches('-').count() >= 4);
    }

    #[test]
    fn shell_and_launcher_render_kind_previews_without_session_history() {
        let backend = TestBackend::new(180, 34);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = app();
        app.workspaces[0]
            .sessions
            .push(session("hidden", AgentDisplayState::Done));
        focus_items(&mut app);

        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        let shell_text: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        assert!(shell_text.contains("Shell · boomux / agent"));
        assert!(shell_text.contains("PATH"));
        assert!(shell_text.contains("GIT"));
        assert!(shell_text.contains("RUN"));
        assert!(shell_text.contains("/tmp/boomux"));
        assert!(!shell_text.contains("OpenCode session"));

        app.workspaces[0]
            .items
            .push(WorkspaceItemView::Launcher(LauncherView {
                id: "launcher".into(),
                name: "editor".into(),
                directory: "/tmp/boomux".into(),
                repository: "boomux".into(),
                branch: "main".into(),
                git_state: "clean".into(),
                worktree: "primary".into(),
                command: "editor .".into(),
                argv: vec!["editor".into(), ".".into()],
            }));
        focus_items(&mut app);
        app.next();
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        let launcher_text: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        assert!(launcher_text.contains("Launcher configuration"));
        assert!(launcher_text.contains("[\"editor\", \".\"]"));
        assert!(launcher_text.contains("output and run history are not retained"));
        assert!(!launcher_text.contains("OpenCode session"));
    }

    #[test]
    fn command_preview_preserves_argument_boundaries() {
        let backend = TestBackend::new(180, 34);
        let mut backend_terminal = Terminal::new(backend).unwrap();
        let mut workspace = workspace("w1", "commands");
        workspace.items = vec![terminal("command", "format", "printf")];
        let WorkspaceItemView::Shell(command) = &mut workspace.items[0] else {
            unreachable!();
        };
        command.argv = vec!["printf".into(), "a b".into(), String::new()];
        let mut app = App::new(vec![workspace], project_context());
        focus_items(&mut app);
        let reads = std::cell::Cell::new(0);
        refresh_terminal_preview(&mut app, &mut |_| {
            reads.set(reads.get() + 1);
            Ok(text_preview("command output"))
        });

        backend_terminal
            .draw(|frame| render(frame, &mut app))
            .unwrap();
        let text: String = backend_terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect();

        assert!(text.contains("Command · commands / format"));
        assert!(text.contains("COMMAND"));
        assert!(text.contains("[\"printf\", \"a b\", \"\"]"));
        assert!(!text.contains("OUTPUT"));
        assert!(!text.contains("pgup/dn"));
        assert_eq!(reads.get(), 0);
        assert!(app.terminal_preview.is_none());
    }

    #[test]
    fn shell_preview_labels_running_and_exited_runs() {
        let mut app = app();
        focus_items(&mut app);
        let now = current_time_ms();
        {
            let WorkspaceItemView::Shell(shell) = &mut app.workspaces[0].items[0] else {
                unreachable!();
            };
            shell.run = Some(TerminalRunView {
                id: "135c1aee-long".into(),
                generation: 3,
                started_at_ms: now,
                ended_at_ms: None,
                exit_reason: None,
                output_revision: 1,
            });
        }
        let WorkspaceItemView::Shell(shell) = &app.workspaces[0].items[0] else {
            unreachable!();
        };
        let preview = terminal_preview(&app, shell).expect("shell preview");
        let PreviewContent::Lines(lines) = preview.content;
        let text = lines
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(preview.title.contains("Shell · boomux / agent"));
        assert!(text.contains("PATH      /tmp/boomux"));
        assert!(text.contains("GIT       boomux  ·  main  ·  clean  ·  primary"));
        assert!(
            text.contains("RUN       running  ·  generation 3  ·  started now  ·  id 135c1aee")
        );

        {
            let WorkspaceItemView::Shell(shell) = &mut app.workspaces[0].items[0] else {
                unreachable!();
            };
            shell.status = "exited".into();
            let run = shell.run.as_mut().unwrap();
            run.ended_at_ms = Some(now);
            run.exit_reason = Some("exited (1)".into());
        }
        let WorkspaceItemView::Shell(shell) = &app.workspaces[0].items[0] else {
            unreachable!();
        };
        let preview = terminal_preview(&app, shell).expect("shell preview");
        let PreviewContent::Lines(lines) = preview.content;
        let run_line = lines[2]
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<String>();
        assert!(run_line.contains("RUN       exit 1  ·  generation 3  ·  ended now"));
    }

    #[test]
    fn narrow_shell_preview_keeps_labeled_metadata_visible() {
        let backend = TestBackend::new(80, 64);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = app();
        focus_items(&mut app);
        {
            let WorkspaceItemView::Shell(shell) = &mut app.workspaces[0].items[0] else {
                unreachable!();
            };
            shell.run = Some(TerminalRunView {
                id: "run-1".into(),
                generation: 1,
                started_at_ms: current_time_ms(),
                ended_at_ms: None,
                exit_reason: None,
                output_revision: 1,
            });
        }
        refresh_terminal_preview(&mut app, &mut |_| Ok(text_preview("output")));

        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        let text = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        for label in ["PATH", "GIT", "RUN", "OUTPUT"] {
            assert!(text.contains(label), "missing {label}");
        }
    }

    #[test]
    fn terminal_output_preview_reads_only_when_selection_or_revision_changes() {
        let mut app = app();
        focus_items(&mut app);
        let WorkspaceItemView::Shell(shell) = &mut app.workspaces[0].items[0] else {
            unreachable!();
        };
        shell.run = Some(TerminalRunView {
            id: "run-1".into(),
            generation: 1,
            started_at_ms: current_time_ms(),
            ended_at_ms: None,
            exit_reason: None,
            output_revision: 4,
        });
        let calls = std::cell::Cell::new(0);
        let mut read = |_: &str| {
            calls.set(calls.get() + 1);
            Ok(text_preview("first\nlatest"))
        };

        refresh_terminal_preview(&mut app, &mut read);
        refresh_terminal_preview(&mut app, &mut read);
        assert_eq!(calls.get(), 1);

        let WorkspaceItemView::Shell(shell) = &mut app.workspaces[0].items[0] else {
            unreachable!();
        };
        shell.run.as_mut().unwrap().output_revision = 5;
        refresh_terminal_preview(&mut app, &mut read);
        assert_eq!(calls.get(), 2);
        let WorkspaceItemView::Shell(shell) = &mut app.workspaces[0].items[0] else {
            unreachable!();
        };
        shell.run.as_mut().unwrap().id = "run-2".into();
        refresh_terminal_preview(&mut app, &mut read);
        assert_eq!(calls.get(), 3);

        let backend = TestBackend::new(180, 34);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        let text: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        assert!(text.contains("OUTPUT"));
        assert!(!text.contains("revision 5"));
        assert!(text.contains("following"));
        assert!(text.contains("latest"));
        assert!(text.contains("pgup/dn"));
    }

    #[test]
    fn failed_terminal_preview_is_not_retried_without_a_revision_change() {
        let mut app = app();
        focus_items(&mut app);
        let effect = app.terminal_preview_effect().unwrap();
        let DashboardEffect::ReadTerminalPreview {
            shell_id,
            run_id,
            output_revision,
        } = effect
        else {
            unreachable!();
        };
        app.apply_terminal_preview(
            shell_id.inner_id,
            run_id,
            output_revision,
            Err("preview unavailable".into()),
        );

        assert!(app.terminal_preview_effect().is_none());
    }

    #[test]
    fn terminal_viewport_trims_edges_and_scrolls_from_the_tail() {
        let output = text_preview("\nold\n\nrecent one  \nrecent two\n\n");

        assert_eq!(
            terminal_output_lines(&output)
                .iter()
                .map(preview_text)
                .collect::<Vec<_>>(),
            ["old", "", "recent one", "recent two"]
        );
        assert!(terminal_output_lines(&text_preview("\n \n")).is_empty());

        let following = terminal_viewport(&output, 3, 0);
        assert_eq!(
            following.lines.iter().map(preview_text).collect::<Vec<_>>(),
            ["", "recent one", "recent two"]
        );
        assert_eq!((following.start, following.end, following.total), (1, 4, 4));
        assert!(following.following);

        let scrolled = terminal_viewport(&output, 2, 2);
        assert_eq!(
            scrolled.lines.iter().map(preview_text).collect::<Vec<_>>(),
            ["old", ""]
        );
        assert!(!scrolled.following);
    }

    #[test]
    fn terminal_preview_renders_structured_colors_and_modifiers() {
        let mut terminal = Terminal::new(TestBackend::new(4, 1)).unwrap();
        let line = TerminalPreviewLine {
            spans: vec![crate::protocol::TerminalPreviewSpan {
                text: "X".into(),
                style: TerminalStyle {
                    foreground: TerminalColor::Indexed(196),
                    background: TerminalColor::Rgb {
                        red: 1,
                        green: 2,
                        blue: 3,
                    },
                    bold: true,
                    italic: true,
                    inverse: true,
                    ..TerminalStyle::default()
                },
            }],
        };

        terminal
            .draw(|frame| {
                frame.render_widget(Paragraph::new(terminal_preview_line(line)), frame.area())
            })
            .unwrap();

        let cell = &terminal.backend().buffer()[(1, 0)];
        assert_eq!(cell.symbol(), "X");
        assert_eq!(cell.fg, Color::Indexed(196));
        assert_eq!(cell.bg, Color::Rgb(1, 2, 3));
        assert!(cell.modifier.contains(Modifier::BOLD));
        assert!(cell.modifier.contains(Modifier::ITALIC));
        assert!(cell.modifier.contains(Modifier::REVERSED));
    }

    #[test]
    fn terminal_preview_scroll_controls_preserve_position_as_output_arrives() {
        let mut app = app();
        focus_items(&mut app);
        let WorkspaceItemView::Shell(shell) = &mut app.workspaces[0].items[0] else {
            unreachable!();
        };
        shell.run = Some(TerminalRunView {
            id: "run-1".into(),
            generation: 1,
            started_at_ms: current_time_ms(),
            ended_at_ms: None,
            exit_reason: None,
            output_revision: 1,
        });
        let output = |count: usize| {
            text_preview(
                &(1..=count)
                    .map(|line| format!("line {line}"))
                    .collect::<Vec<_>>()
                    .join("\n"),
            )
        };

        refresh_terminal_preview(&mut app, &mut |_| Ok(output(40)));
        app.scroll_terminal_preview_up();
        assert_eq!(
            app.terminal_preview.as_ref().unwrap().scroll_from_bottom,
            12
        );
        app.scroll_terminal_preview_to_start();
        assert_eq!(
            app.terminal_preview.as_ref().unwrap().scroll_from_bottom,
            24
        );
        app.scroll_terminal_preview_down();
        assert_eq!(
            app.terminal_preview.as_ref().unwrap().scroll_from_bottom,
            12
        );

        let WorkspaceItemView::Shell(shell) = &mut app.workspaces[0].items[0] else {
            unreachable!();
        };
        shell.run.as_mut().unwrap().output_revision = 2;
        refresh_terminal_preview(&mut app, &mut |_| Ok(output(42)));
        assert_eq!(
            app.terminal_preview.as_ref().unwrap().scroll_from_bottom,
            14
        );

        app.scroll_terminal_preview_to_end();
        assert_eq!(app.terminal_preview.as_ref().unwrap().scroll_from_bottom, 0);
    }

    #[test]
    fn shell_preview_uses_sixteen_rows_or_hides_when_height_is_insufficient() {
        let mut app = app();
        focus_items(&mut app);
        let WorkspaceItemView::Shell(shell) = &mut app.workspaces[0].items[0] else {
            unreachable!();
        };
        shell.run = Some(TerminalRunView {
            id: "run-1".into(),
            generation: 1,
            started_at_ms: current_time_ms(),
            ended_at_ms: None,
            exit_reason: None,
            output_revision: 1,
        });
        let output = text_preview(
            &(1..=30)
                .map(|line| format!("viewport line {line}"))
                .collect::<Vec<_>>()
                .join("\n"),
        );
        refresh_terminal_preview(&mut app, &mut |_| Ok(output.clone()));

        let mut wide = Terminal::new(TestBackend::new(180, 40)).unwrap();
        wide.draw(|frame| render(frame, &mut app)).unwrap();
        let wide_text: String = wide
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        assert!(wide_text.contains("Shell · boomux / agent"));
        assert!(wide_text.contains("OUTPUT"));
        assert!(wide_text.contains("lines 15-30 of 30"));
        assert!(wide_text.contains("following"));
        assert!(!wide_text.contains("revision"));
        assert!(wide_text.contains("viewport line 15"));
        assert!(wide_text.contains("viewport line 30"));
        assert!(!wide_text.contains("viewport line 14"));

        let mut short = Terminal::new(TestBackend::new(180, 24)).unwrap();
        short.draw(|frame| render(frame, &mut app)).unwrap();
        let short_text: String = short
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        assert!(!short_text.contains("Shell · boomux / agent"));
        assert!(short_text.contains("Items: boomux"));
    }

    #[test]
    fn workspace_preview_omits_attention_details() {
        let backend = TestBackend::new(140, 34);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut workspace = workspace("w1", "review");
        workspace.attention = vec![WorkspaceAttentionView {
            node_id: String::new(),
            workspace_id: workspace.id.clone(),
            agent_id: "agent-active".into(),
            shell_id: "term_1".into(),
            agent_name: "review-agent".into(),
            reason: AttentionReason::Blocked,
            evidence: "approval required".into(),
            observed_at_ms: current_time_ms(),
        }];
        workspace.attention_count = 1;
        let mut app = App::new(vec![workspace], project_context());

        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        let text: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect();

        assert!(text.contains("review overview"));
        assert!(text.contains("working"));
        assert!(text.contains("blocked"));
        assert!(!text.contains("blocked: review-agent"));
        assert!(!text.contains("approval required"));
        assert!(!text.contains("No outstanding attention"));
    }

    #[test]
    fn generic_agent_names_fall_back_to_shell_and_identity() {
        let mut view = session("generic", AgentDisplayState::Idle);
        view.label = "opencode".into();
        view.external_session_id = Some("ses_123456789".into());

        assert_eq!(session_task_label(&view), None);
        assert_eq!(best_session_label(&view), "agent (ses_1234)");
    }

    #[test]
    fn pi_sessions_keep_the_shell_and_identity_fallback() {
        let mut view = session("pi-generic", AgentDisplayState::Idle);
        view.integration = "pi".into();
        view.label = "Pi".into();
        view.external_session_id = Some("pi_123456789".into());

        assert_eq!(session_task_label(&view), None);
        assert_eq!(best_session_label(&view), "agent (pi_12345)");
    }

    #[test]
    fn narrow_dashboard_stacks_workspace_and_shell_details() {
        let backend = TestBackend::new(80, 34);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = app();

        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        let text: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        assert!(text.contains("Workspaces (1)"));
        assert!(text.contains("Items: boomux (1)"));
        assert!(text.contains("ACTIVITY"));
        assert!(text.contains("WORKTREE"));
        assert!(text.contains("main"));
        assert!(!text.contains("term_1"));
        assert!(!text.contains("DIRTY"));
        assert!(text.contains("WORKTREE"));
    }

    #[test]
    fn empty_dashboard_explains_how_to_create_a_workspace() {
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = App::new(Vec::new(), project_context());

        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        let text: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect();

        assert!(text.contains("No workspaces. Press a to create one."));
    }

    #[test]
    fn horizontal_breakpoint_keeps_every_shell_column_visible() {
        let backend = TestBackend::new(108, 34);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = app();

        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        let text: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        assert!(text.contains("NAME"));
        assert!(text.contains("STATUS"));
        assert!(text.contains("ACTIVITY"));
        assert!(text.contains("WORKTREE"));
        assert!(!text.contains("term_1"));
    }

    #[test]
    fn project_launcher_renders_to_test_backend() {
        let backend = TestBackend::new(120, 34);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = app();
        app.request_add();

        terminal.draw(|frame| render(frame, &mut app)).unwrap();
    }

    #[test]
    fn project_launcher_shows_explicit_by_name_and_project_modes() {
        let backend = TestBackend::new(120, 34);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = app();
        app.request_add();
        if let Mode::PickProject(picker) = &mut app.mode {
            picker.toggle_mode();
        }
        if let Mode::PickProject(picker) = &mut app.mode {
            picker.query = "alpha".into();
            picker.update_matches();
        }

        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        let text: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect();

        assert!(text.contains("BY NAME"));
        assert!(text.contains("FROM PROJECT"));
        assert!(text.contains("Create a workspace by name"));
        assert!(text.contains("alpha"));
    }

    #[test]
    fn schedule_tab_dispatches_only_typed_valid_actions() {
        let mut app = schedule_app();
        assert_eq!(
            app.update(DashboardEvent::KeyPressed {
                code: KeyCode::Char('u'),
                modifiers: KeyModifiers::NONE
            }),
            vec![DashboardEffect::RunSchedule("schedule-1".into())]
        );
        assert_eq!(
            app.update(DashboardEvent::KeyPressed {
                code: KeyCode::Char('p'),
                modifiers: KeyModifiers::NONE
            }),
            vec![DashboardEffect::ResumeSchedule("schedule-1".into())]
        );
        app.schedules[0].state = ScheduleDisplayState::Enabled;
        assert_eq!(
            app.update(DashboardEvent::KeyPressed {
                code: KeyCode::Char('p'),
                modifiers: KeyModifiers::NONE
            }),
            vec![DashboardEffect::PauseSchedule("schedule-1".into())]
        );
        assert!(
            app.update(DashboardEvent::KeyPressed {
                code: KeyCode::Char('c'),
                modifiers: KeyModifiers::NONE
            })
            .is_empty()
        );
        assert!(matches!(
            app.pending_close,
            Some(PendingClose { target: CloseTarget::Execution(ref id), .. }) if id == "execution-1"
        ));
        assert_eq!(
            app.update(DashboardEvent::KeyPressed {
                code: KeyCode::Char('y'),
                modifiers: KeyModifiers::NONE
            }),
            vec![DashboardEffect::CancelExecution("execution-1".into())]
        );
        assert!(
            app.update(DashboardEvent::KeyPressed {
                code: KeyCode::Char('h'),
                modifiers: KeyModifiers::NONE
            })
            .is_empty()
        );
        assert!(
            app.update(DashboardEvent::KeyPressed {
                code: KeyCode::Char('s'),
                modifiers: KeyModifiers::NONE
            })
            .is_empty()
        );

        app.update(DashboardEvent::KeyPressed {
            code: KeyCode::Char('x'),
            modifiers: KeyModifiers::NONE,
        });
        assert!(
            matches!(app.pending_close, Some(PendingClose { target: CloseTarget::Schedule(ref id), .. }) if id == "schedule-1")
        );
        assert_eq!(
            app.update(DashboardEvent::KeyPressed {
                code: KeyCode::Char('y'),
                modifiers: KeyModifiers::NONE
            }),
            vec![DashboardEffect::RemoveSchedule("schedule-1".into())]
        );
    }

    #[test]
    fn selecting_a_schedule_loads_its_history_automatically() {
        let mut app = App::new(Vec::new(), project_context());
        app.schedules = vec![schedule_view()];
        app.scheduling = SchedulingView::Active {
            active: 0,
            maximum: 4,
        };

        assert_eq!(
            app.update(DashboardEvent::KeyPressed {
                code: KeyCode::Char('4'),
                modifiers: KeyModifiers::NONE,
            }),
            vec![DashboardEffect::LoadScheduleHistory {
                schedule_id: "schedule-1".into(),
                limit: 100,
            }]
        );
        app.update(DashboardEvent::ScheduleHistoryCompleted {
            schedule_id: "schedule-1".into(),
            result: Ok((app.schedules[0].executions.clone(), false)),
        });
        assert!(app.schedules[0].history_scoped);

        let mut second = schedule_view();
        second.id = "schedule-2".into();
        second.name = "weekly review".into();
        app.schedules.push(second);
        assert_eq!(
            app.update(DashboardEvent::KeyPressed {
                code: KeyCode::Char('j'),
                modifiers: KeyModifiers::NONE,
            }),
            vec![DashboardEffect::LoadScheduleHistory {
                schedule_id: "schedule-2".into(),
                limit: 100,
            }]
        );
    }

    #[test]
    fn workspace_schedule_row_is_a_definition_link_not_a_process() {
        let mut app = schedule_app();
        app.workspaces[0].items.push(schedule_item());
        app.select_tab(PrimaryTab::Workspaces);
        app.set_focus(Focus::Items);
        app.item_state.select(Some(1));

        assert_eq!(app.workspaces[0].ordinary_item_count(), 2);
        assert_eq!(app.workspaces[0].process_count(), 1);
        assert_eq!(app.workspaces[0].schedule_count(), 1);
        let text = rendered_text(&mut app, 180, 34);
        assert!(text.contains("schedule"));
        assert!(text.contains("nightly review"));
        assert!(text.contains("Schedule definition"));
        assert!(text.contains("weekdays 09:30"));

        assert_eq!(
            app.update(DashboardEvent::KeyPressed {
                code: KeyCode::Enter,
                modifiers: KeyModifiers::NONE,
            }),
            vec![DashboardEffect::LoadScheduleHistory {
                schedule_id: "schedule-1".into(),
                limit: 100,
            }]
        );
        assert_eq!(app.primary_tab, PrimaryTab::Schedules);
        assert_eq!(
            app.selected_schedule().map(|schedule| schedule.id.as_str()),
            Some("schedule-1")
        );

        app.select_tab(PrimaryTab::Workspaces);
        app.set_focus(Focus::Items);
        app.item_state.select(Some(1));
        assert_eq!(
            app.request_rename(),
            Some(DashboardEffect::LoadScheduleEditor {
                schedule_id: "schedule-1".into()
            })
        );
        assert_eq!(app.primary_tab, PrimaryTab::Schedules);
        app.select_tab(PrimaryTab::Workspaces);
        app.set_focus(Focus::Items);
        app.item_state.select(Some(1));
        app.request_close();
        assert!(matches!(
            app.pending_close,
            Some(PendingClose { target: CloseTarget::Schedule(ref id), .. }) if id == "schedule-1"
        ));

        let palette =
            CommandPalette::new_with_schedules(&app.workspaces, &app.schedules, &app.scheduling);
        assert!(!palette.entries.iter().any(|entry| {
            matches!(
                entry.command,
                PaletteCommand::Item {
                    ref identity,
                    ..
                } if identity.kind == ItemIdentityKind::Schedule
            )
        }));
    }

    #[test]
    fn schedule_editor_is_private_revision_aware_and_paused_only() {
        let mut app = schedule_app();
        assert_eq!(
            app.request_rename(),
            Some(DashboardEffect::LoadScheduleEditor {
                schedule_id: "schedule-1".into()
            })
        );
        app.update(DashboardEvent::ScheduleEditorLoaded {
            schedule_id: "schedule-1".into(),
            result: Ok(ScheduleEditInspection {
                schedule_id: "schedule-1".into(),
                name: "nightly review".into(),
                cron: "30 9 * * 1-5".into(),
                timezone: "America/New_York".into(),
                prompt: "private prompt".into(),
                revision: 7,
                paused: true,
            }),
        });
        let editor_text = rendered_text(&mut app, 100, 30);
        assert!(editor_text.contains("Edit paused schedule"));
        assert!(editor_text.contains("private prompt"));
        assert!(editor_text.contains("America/New_York"));
        let Mode::EditSchedule(editor) = &mut app.mode else {
            panic!("expected schedule editor");
        };
        editor.field = ScheduleEditorField::Prompt;
        editor.cursor = editor.prompt.len();
        app.update(DashboardEvent::KeyPressed {
            code: KeyCode::Left,
            modifiers: KeyModifiers::NONE,
        });
        app.update(DashboardEvent::KeyPressed {
            code: KeyCode::Char('!'),
            modifiers: KeyModifiers::NONE,
        });
        app.update(DashboardEvent::KeyPressed {
            code: KeyCode::End,
            modifiers: KeyModifiers::NONE,
        });
        app.update(DashboardEvent::TextPasted("\nnext line".into()));
        let effects = app.update(DashboardEvent::KeyPressed {
            code: KeyCode::Char('s'),
            modifiers: KeyModifiers::CONTROL,
        });
        let [
            DashboardEffect::UpdateSchedule {
                schedule_id,
                expected_revision,
                update,
            },
        ] = effects.as_slice()
        else {
            panic!("expected schedule update");
        };
        assert_eq!(schedule_id, "schedule-1");
        assert_eq!(*expected_revision, 7);
        assert_eq!(update.prompt, "private promp!t\nnext line");
        assert!(!format!("{effects:?}").contains("private prompt"));

        app.update(DashboardEvent::ScheduleEditorSaved {
            schedule_id: "schedule-1".into(),
            result: Err("agent schedule revision is 8; update expected 7".into()),
        });
        assert!(matches!(app.mode, Mode::EditSchedule(_)));
        let effects = app.update(DashboardEvent::ScheduleEditorSaved {
            schedule_id: "schedule-1".into(),
            result: Ok("Updated schedule nightly review".into()),
        });
        assert_eq!(effects, vec![DashboardEffect::Refresh]);
        assert!(matches!(app.mode, Mode::Normal));

        app.update(DashboardEvent::ScheduleEditorLoaded {
            schedule_id: "schedule-1".into(),
            result: Ok(ScheduleEditInspection {
                schedule_id: "schedule-1".into(),
                name: "nightly review".into(),
                cron: "30 9 * * 1-5".into(),
                timezone: "America/New_York".into(),
                prompt: "discard me".into(),
                revision: 8,
                paused: false,
            }),
        });
        assert!(matches!(app.mode, Mode::Normal));
        assert!(
            app.message
                .as_ref()
                .is_some_and(|message| message.text.contains("Pause"))
        );
    }

    #[test]
    fn schedule_editor_timezone_search_only_selects_iana_names() {
        let mut editor = ScheduleEditor {
            schedule_id: "schedule-1".into(),
            expected_revision: 7,
            field: ScheduleEditorField::Timezone,
            preset: ScheduleTriggerPreset::Weekdays,
            name: "nightly review".into(),
            cron: "30 9 * * 1-5".into(),
            timezone: "America/New_York".into(),
            timezone_query: String::new(),
            prompt: "private prompt".into(),
            cursor: 0,
            error: None,
        };

        editor.insert_text("tokyo");
        assert_eq!(editor.timezone, "Asia/Tokyo");
        assert!(editor.error.is_none());

        editor.timezone_query.clear();
        editor.insert_text("definitely-not-a-timezone");
        assert_eq!(editor.timezone, "Asia/Tokyo");
        assert_eq!(editor.error.as_deref(), Some("No matching IANA timezone"));

        editor.timezone_query = "america".into();
        editor.select_timezone_match(false, false);
        editor.select_timezone_match(false, true);
        assert!(editor.timezone.starts_with("America/"));
        assert!(editor.timezone.parse::<chrono_tz::Tz>().is_ok());
    }

    #[test]
    fn schedule_editor_renders_bordered_controls_and_a_real_cursor() {
        let mut app = schedule_app();
        app.mode = Mode::EditSchedule(ScheduleEditor {
            schedule_id: "schedule-1".into(),
            expected_revision: 7,
            field: ScheduleEditorField::Name,
            preset: ScheduleTriggerPreset::Weekdays,
            name: "nightly review".into(),
            cron: "30 9 * * 1-5".into(),
            timezone: "America/New_York".into(),
            timezone_query: String::new(),
            prompt: "private prompt".into(),
            cursor: "nightly review".len(),
            error: None,
        });
        let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();

        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        let text: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect();

        assert!(text.contains("Name"));
        assert!(text.contains("Trigger preset"));
        assert!(text.contains("Timezone · type to search"));
        assert!(text.contains("Prompt · private multiline"));
        assert!(!text.contains("nightly review_"));
        assert!(terminal.get_cursor_position().unwrap().x > 0);

        let compact = rendered_text(&mut app, 60, 20);
        assert!(compact.contains("Edit paused schedule"));
        assert!(compact.contains("Timezone · type to search"));
    }

    #[test]
    fn schedule_creation_is_actionable_cli_help_without_fabricating_a_form() {
        let mut app = schedule_app();
        assert!(
            app.update(DashboardEvent::KeyPressed {
                code: KeyCode::Char('a'),
                modifiers: KeyModifiers::NONE
            })
            .is_empty()
        );
        assert!(
            app.message
                .as_ref()
                .is_some_and(|message| message.text.contains("boomux schedule create --help"))
        );
    }

    #[test]
    fn execution_selection_is_exact_and_stable_across_history_reorder() {
        let mut app = schedule_app();
        app.schedules[0]
            .executions
            .push(execution("execution-2", ExecutionDisplayState::Active));
        app.sync_selected_execution();
        assert_eq!(
            app.selected_execution()
                .map(|execution| execution.id.as_str()),
            Some("execution-1")
        );

        assert_eq!(app.focus, Focus::Workspaces);
        app.update(DashboardEvent::KeyPressed {
            code: KeyCode::Right,
            modifiers: KeyModifiers::NONE,
        });
        assert_eq!(app.focus, Focus::Items);
        app.update(DashboardEvent::KeyPressed {
            code: KeyCode::Char('j'),
            modifiers: KeyModifiers::NONE,
        });
        assert_eq!(
            app.selected_execution()
                .map(|execution| execution.id.as_str()),
            Some("execution-2")
        );
        assert_eq!(
            app.update(DashboardEvent::KeyPressed {
                code: KeyCode::Enter,
                modifiers: KeyModifiers::NONE,
            }),
            vec![DashboardEffect::OpenScheduledExecution {
                execution_id: "execution-2".into(),
            }]
        );
        let mut reordered = app.schedules[0].clone();
        reordered.executions.swap(0, 1);
        app.replace_schedules(vec![reordered]);
        assert_eq!(
            app.selected_execution()
                .map(|execution| execution.id.as_str()),
            Some("execution-2")
        );

        app.update(DashboardEvent::KeyPressed {
            code: KeyCode::Char('c'),
            modifiers: KeyModifiers::NONE,
        });
        assert!(matches!(
            app.pending_close,
            Some(PendingClose { target: CloseTarget::Execution(ref id), .. }) if id == "execution-2"
        ));
    }

    #[test]
    fn schedule_refresh_clears_selection_when_the_last_schedule_disappears() {
        let mut app = schedule_app();

        app.replace_schedules(Vec::new());

        assert_eq!(app.global_state.selected(), None);
        assert_eq!(app.selected_execution(), None);
    }

    #[test]
    fn command_palette_omits_execution_history() {
        let app = schedule_app();
        let palette =
            CommandPalette::new_with_schedules(&app.workspaces, &app.schedules, &app.scheduling);
        assert!(
            !palette
                .entries
                .iter()
                .any(|entry| entry.keywords.contains("execution execution-1"))
        );
    }

    #[test]
    fn selected_execution_is_visible_in_full_and_compact_schedule_layouts() {
        let mut app = schedule_app();
        app.schedules[0].executions = (0..7)
            .map(|index| {
                execution(
                    &format!("selected-{index}-execution"),
                    ExecutionDisplayState::Exited,
                )
            })
            .collect();
        assert!(app.select_execution_id("selected-5-execution"));

        for (width, height) in [(80, 20), (60, 16)] {
            let text = rendered_text(&mut app, width, height);
            assert!(
                text.contains("selected"),
                "selected execution is not represented at {width}x{height}"
            );
            assert!(
                text.contains("exited"),
                "selected execution status is not visible at {width}x{height}"
            );
        }
        assert_eq!(app.execution_state.selected(), Some(5));
    }

    #[test]
    fn open_execution_delegates_exact_id_for_run_or_session_resolution() {
        for state in [
            ExecutionDisplayState::Starting,
            ExecutionDisplayState::Active,
        ] {
            let mut app = schedule_app();
            app.schedules[0].executions[0].state = state;
            assert_eq!(
                app.open_selected_schedule_link(),
                Some(DashboardEffect::OpenScheduledExecution {
                    execution_id: "execution-1".into()
                })
            );
        }

        let mut app = schedule_app();
        app.schedules[0].executions[0].state = ExecutionDisplayState::Exited;
        assert_eq!(
            app.open_selected_schedule_link(),
            Some(DashboardEffect::OpenScheduledExecution {
                execution_id: "execution-1".into()
            })
        );
    }

    #[test]
    fn protocol_25_keeps_schedules_usable_but_disables_exact_open() {
        let mut app = schedule_app();
        app.exact_run_attachment = false;

        assert!(app.open_selected_schedule_link().is_none());
        assert!(app.message.as_ref().is_some_and(|message| {
            message.text.contains("protocol 26") && message.text.contains("upgrade and restart")
        }));
        let compact = rendered_text(&mut app, 60, 16);
        assert!(compact.contains("History"));
    }

    #[test]
    fn blocked_active_schedule_execution_opens_its_exact_run() {
        let mut app = schedule_app();
        app.schedules[0].executions[0].agent_state = Some(AgentDisplayState::Blocked);

        assert_eq!(
            app.update(DashboardEvent::KeyPressed {
                code: KeyCode::Enter,
                modifiers: KeyModifiers::NONE
            }),
            vec![DashboardEffect::OpenScheduledExecution {
                execution_id: "execution-1".into(),
            }]
        );
    }

    #[test]
    fn schedule_agent_is_visible_for_exact_agent_navigation_but_has_no_ordinary_actions() {
        let mut workspace = workspace("w1", "scheduled");
        let mut scheduled_agent = agent_shell();
        scheduled_agent.shell.id = "schedule-shell".into();
        scheduled_agent.shell.name = "scheduled-runner".into();
        scheduled_agent.agent.as_mut().unwrap().id = "schedule-agent".into();
        scheduled_agent.schedule_id = Some("schedule-1".into());
        workspace.items = vec![WorkspaceItemView::AgentShell(scheduled_agent)];
        let mut app = App::new(vec![workspace], project_context());

        let workspaces = rendered_text(&mut app, 114, 24);
        assert!(!workspaces.contains("scheduled-runner"));
        assert_eq!(app.workspaces[0].ordinary_item_count(), 0);

        app.select_tab(PrimaryTab::Agents);
        let agents = rendered_text(&mut app, 114, 24);
        assert!(agents.contains("scheduled"));
        assert!(app.open_selected_item().is_none());
        app.request_rename();
        assert!(matches!(app.mode, Mode::Normal));
        app.request_close();
        assert!(app.pending_close.is_none());
    }

    #[test]
    fn schedule_history_is_applied_only_by_its_explicit_completion_event() {
        let mut app = schedule_app();
        app.update(DashboardEvent::OperationCompleted(Ok(
            "Paused schedule nightly review".into(),
        )));
        let history = vec![execution(
            "history-execution",
            ExecutionDisplayState::Exited,
        )];
        app.update(DashboardEvent::ScheduleHistoryCompleted {
            schedule_id: "schedule-1".into(),
            result: Ok((history, true)),
        });
        assert_eq!(app.schedules[0].executions[0].id, "history-execution");
        assert!(app.schedules[0].history_scoped);
        assert!(app.schedules[0].history_truncated);
        assert_eq!(
            app.message.as_ref().map(|message| message.text.as_str()),
            Some("Paused schedule nightly review")
        );
    }

    #[test]
    fn schedule_palette_has_schedule_and_notice_actions_but_no_execution_history_or_content() {
        let mut schedule = schedule_view();
        schedule.executions[0].state = ExecutionDisplayState::DispatchFailed;
        schedule.executions[0].reason = Some(ExecutionReasonDisplay::HostSpawnFailed);
        let mut palette = CommandPalette::new_with_schedules(
            &[workspace("w1", "boomux")],
            &[schedule],
            &SchedulingView::Offline {
                active: 0,
                maximum: 4,
            },
        );
        for (query, group) in [
            ("schedule nightly", PaletteKindGroup::Schedules),
            ("notice failed", PaletteKindGroup::ScheduleNotices),
        ] {
            palette.query = query.into();
            palette.update_matches();
            assert!(
                palette
                    .matches
                    .iter()
                    .any(|index| palette.entries[*index].kind_group == group)
            );
        }
    }

    #[test]
    fn schedule_rendering_preserves_wide_common_breakpoints_and_compact_layouts() {
        for (width, height) in [(180, 34), (114, 24), (113, 24), (80, 24), (60, 16)] {
            let mut app = schedule_app();
            let text = rendered_text(&mut app, width, height);
            assert!(
                text.contains("SCHEDULES"),
                "missing tab at {width}x{height}"
            );
            assert!(
                text.contains("nightly review"),
                "missing row at {width}x{height}"
            );
            assert!(
                text.contains("never") || text.contains("active"),
                "missing outcome at {width}x{height}"
            );
            assert!(
                !text.contains("schedule-shell"),
                "leaked full shell ID at {width}x{height}"
            );
            if width == 80 && height == 24 {
                assert!(text.contains("[/]"), "missing execution navigation help");
            }
        }
        let mut wide = schedule_app();
        let wide = rendered_text(&mut wide, 180, 34);
        for column in [
            "TRIGGER",
            "NEXT",
            "LAST",
            "STATE",
            "WORKSPACE",
            "INTEGRATION",
        ] {
            assert!(wide.contains(column), "missing schedule column {column}");
        }
        assert!(wide.contains("History"));
        assert!(!wide.contains("PROMPT REV"));
        assert!(!wide.contains("no timeout"));
        assert!(!wide.contains("America/New_York"));
    }

    #[test]
    fn schedule_empty_unsupported_offline_and_history_boundaries_are_truthful() {
        let mut app = App::new(Vec::new(), project_context());
        app.select_tab(PrimaryTab::Schedules);
        let unsupported = rendered_text(&mut app, 80, 24);
        assert!(unsupported.contains("require daemon protocol"));

        app.scheduling = SchedulingView::Offline {
            active: 0,
            maximum: 4,
        };
        let empty = rendered_text(&mut app, 80, 24);
        assert!(empty.contains("schedule create --help"));

        app.schedules = vec![schedule_view()];
        app.select_tab(PrimaryTab::Schedules);
        app.schedules[0].history_truncated = true;
        let truncated = rendered_text(&mut app, 114, 24);
        assert!(truncated.contains("page is truncated"));
        app.schedules[0].history_truncated = false;
        app.schedules[0].possible_pruning_boundary = true;
        let pruned = rendered_text(&mut app, 114, 24);
        assert!(pruned.contains("pruning boundary"));

        app.schedules[0].executions.clear();
        app.schedules[0].history_complete = false;
        app.sync_selected_execution();
        let unknown = rendered_text(&mut app, 114, 24);
        assert!(unknown.contains("history unknown"));
        assert!(!unknown.contains("never run"));
        app.schedules[0].history_scoped = true;
        app.schedules[0].history_complete = true;
        let complete = rendered_text(&mut app, 114, 24);
        assert!(complete.contains("never run"));
    }
}

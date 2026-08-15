use std::io;
use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::symbols::Marker;
use ratatui::text::{Line, Span};
use ratatui::widgets::canvas::{Canvas, Circle, Line as CanvasLine};
use ratatui::widgets::{
    Block, Cell, Clear, List, ListItem, ListState, Paragraph, Row, Table, TableState, Wrap,
};

use crate::agent_attention_projection::AgentStateCounts;
use crate::protocol::{TerminalColor, TerminalPreview, TerminalPreviewLine, TerminalStyle};

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
const AGENT_TABLE_HEADERS: [&str; 7] = [
    "STATUS",
    "UPDATED",
    "WORKSPACE",
    "SHELL",
    "TASK",
    "ROOT BRANCH",
    "ROOT WORKTREE",
];
const SHELL_TABLE_HEADERS: [&str; 8] = [
    "STATUS",
    "RUN",
    "WORKSPACE",
    "SHELL",
    "KIND",
    "PROCESS",
    "BRANCH",
    "WORKTREE",
];
const ITEM_TABLE_HEADERS: [&str; 6] = ["KIND", "STATUS", "NAME", "ACTIVITY", "BRANCH", "WORKTREE"];

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

pub(crate) struct WorkspaceView {
    pub(crate) id: String,
    pub(crate) name: String,
    pub(crate) default_cwd: Option<String>,
    pub(crate) items: Vec<WorkspaceItemView>,
    pub(crate) sessions: Vec<AgentSessionView>,
    pub(crate) agent_state_counts: AgentStateCounts,
    pub(crate) attention_count: usize,
    pub(crate) attention: Vec<WorkspaceAttentionView>,
}

pub(crate) struct DashboardState {
    pub(crate) workspaces: Vec<WorkspaceView>,
    pub(crate) schedules: Vec<ScheduleView>,
    pub(crate) scheduling: SchedulingView,
    pub(crate) exact_run_attachment: bool,
    pub(crate) focused_terminal: Option<FocusedTerminalView>,
    pub(crate) reset_focus_revision: bool,
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
    pub(crate) id: String,
    pub(crate) workspace_id: String,
    pub(crate) workspace: String,
    pub(crate) name: String,
    pub(crate) integration: String,
    pub(crate) cwd: String,
    pub(crate) state: ScheduleDisplayState,
    pub(crate) friendly_trigger: String,
    pub(crate) exact_trigger: String,
    pub(crate) timezone: String,
    pub(crate) next_occurrence_ms: Option<u64>,
    pub(crate) prompt_revision: u64,
    pub(crate) executions: Vec<ExecutionView>,
    pub(crate) history_truncated: bool,
    pub(crate) possible_pruning_boundary: bool,
    pub(crate) history_scoped: bool,
    pub(crate) history_complete: bool,
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
    pub(crate) prompt_revision: u64,
    pub(crate) shell_id: Option<String>,
    pub(crate) run_id: Option<String>,
    pub(crate) agent_id: Option<String>,
    pub(crate) agent_state: Option<AgentDisplayState>,
    pub(crate) session_id: Option<String>,
    pub(crate) transcript_available: bool,
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

impl ExecutionView {
    fn is_openable(&self) -> bool {
        matches!(
            self.state,
            ExecutionDisplayState::Starting | ExecutionDisplayState::Active
        ) && self.shell_id.is_some()
            && self.run_id.is_some()
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
pub(crate) struct TranscriptView {
    pub(crate) execution_id: String,
    pub(crate) session_id: String,
    pub(crate) lines: Vec<String>,
    pub(crate) truncated: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct FocusedTerminalView {
    pub(crate) revision: u64,
    pub(crate) workspace_id: String,
    pub(crate) shell_id: String,
}

pub(crate) struct WorkspaceAttentionView {
    pub(crate) agent_id: String,
    pub(crate) shell_id: String,
    pub(crate) agent_name: String,
    pub(crate) reason: AttentionReason,
    pub(crate) evidence: String,
    pub(crate) observed_at_ms: u64,
}

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

pub(crate) struct AgentSessionRunView {
    pub(crate) agent_id: String,
    pub(crate) shell_name: Option<String>,
    pub(crate) directory: Option<PathBuf>,
}

#[allow(clippy::large_enum_variant)]
pub(crate) enum WorkspaceItemView {
    Shell(TerminalView),
    AgentShell(AgentShellView),
    Launcher(LauncherView),
}

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

    fn process_count(&self) -> usize {
        self.items
            .iter()
            .filter(|item| {
                item.ordinary_visible() && !matches!(item, WorkspaceItemView::Launcher(_))
            })
            .count()
    }

    fn ordinary_item_count(&self) -> usize {
        self.items
            .iter()
            .filter(|item| item.ordinary_visible())
            .count()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ItemKind {
    Agent,
    Launcher,
    Shell,
    Command,
}

impl WorkspaceItemView {
    fn ordinary_visible(&self) -> bool {
        !matches!(self, Self::AgentShell(agent) if agent.schedule_id.is_some())
    }

    fn kind(&self) -> ItemKind {
        match self {
            Self::AgentShell(_) => ItemKind::Agent,
            Self::Launcher(_) => ItemKind::Launcher,
            Self::Shell(shell) => shell.kind.into(),
        }
    }

    fn name(&self) -> &str {
        match self {
            Self::Shell(shell) => &shell.name,
            Self::AgentShell(agent) => &agent.shell.name,
            Self::Launcher(launcher) => &launcher.name,
        }
    }

    fn status(&self) -> &str {
        match self {
            Self::Shell(shell) => &shell.status,
            Self::AgentShell(agent) => agent.state().label(),
            Self::Launcher(_) => "launcher",
        }
    }

    fn id(&self) -> &str {
        match self {
            Self::Shell(shell) => &shell.id,
            Self::AgentShell(agent) => &agent.shell.id,
            Self::Launcher(launcher) => &launcher.id,
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
    RestoreWorkspace(String),
    Open(OpenTarget),
    Close(CloseTarget),
    CreateWorkspace {
        name: String,
        default_cwd: Option<PathBuf>,
    },
    CreateShell(String),
    Rename {
        target: RenameTarget,
        name: String,
    },
    CheckForUpdates,
    Refresh,
    RunSchedule(String),
    PauseSchedule(String),
    ResumeSchedule(String),
    CancelExecution(String),
    OpenScheduledExecution {
        execution_id: String,
        shell_id: String,
        run_id: String,
    },
    RemoveSchedule(String),
    LoadScheduleHistory {
        schedule_id: String,
        limit: u16,
    },
    ReadExecutionTranscript {
        session_id: String,
        execution_id: String,
    },
    ReadTerminalPreview {
        shell_id: String,
        run_id: Option<String>,
        output_revision: u64,
    },
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
    RefreshCompleted(Result<DashboardState, String>),
    ScheduleHistoryCompleted {
        schedule_id: String,
        result: Result<(Vec<ExecutionView>, bool), String>,
    },
    TranscriptCompleted(Result<TranscriptView, String>),
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

struct App {
    workspaces: Vec<WorkspaceView>,
    schedules: Vec<ScheduleView>,
    scheduling: SchedulingView,
    exact_run_attachment: bool,
    selected_execution_id: Option<String>,
    workspace_state: TableState,
    item_state: TableState,
    global_state: TableState,
    primary_tab: PrimaryTab,
    focus: Focus,
    mode: Mode,
    message: Option<Message>,
    pending_close: Option<PendingClose>,
    project_context: ProjectContext,
    terminal_preview: Option<TerminalPreviewState>,
    transcript: Option<TranscriptView>,
    transcript_scroll_from_bottom: usize,
    follow_focused_terminal: bool,
    selection_pinned: bool,
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
}

impl PrimaryTab {
    const ALL: [Self; 4] = [
        Self::Workspaces,
        Self::Agents,
        Self::Shells,
        Self::Schedules,
    ];

    fn kind(self) -> Option<ItemKind> {
        match self {
            Self::Workspaces => None,
            Self::Agents => Some(ItemKind::Agent),
            Self::Shells => Some(ItemKind::Shell),
            Self::Schedules => None,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Workspaces => "WORKSPACES",
            Self::Agents => "AGENTS",
            Self::Shells => "SHELLS",
            Self::Schedules => "SCHEDULES",
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
    workspace_id: String,
    item_id: String,
    launcher: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum OpenTarget {
    Shell(String),
    Launcher {
        workspace_id: String,
        launcher_id: String,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum RenameTarget {
    Workspace(String),
    Shell(String),
    Launcher(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum CloseTarget {
    Workspace(String),
    Shell(String),
    Launcher(String),
    Schedule(String),
    Execution(String),
}

impl RenameTarget {
    fn label(&self) -> &'static str {
        match self {
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
    Transcript,
    Rename { target: RenameTarget, input: String },
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
    Workspaces,
    Agents,
    Shells,
    Commands,
    Launchers,
    Schedules,
    Executions,
    ScheduleNotices,
    Dashboard,
}

impl PaletteKindGroup {
    fn label(self) -> &'static str {
        match self {
            Self::BlockedAgents => "BLOCKED AGENTS",
            Self::Attention => "ATTENTION",
            Self::Workspaces => "WORKSPACES",
            Self::Agents => "AGENTS",
            Self::Shells => "SHELLS",
            Self::Commands => "COMMANDS",
            Self::Launchers => "LAUNCHERS",
            Self::Schedules => "SCHEDULES",
            Self::Executions => "EXECUTIONS",
            Self::ScheduleNotices => "SCHEDULE NOTICES",
            Self::Dashboard => "DASHBOARD",
        }
    }
}

#[derive(Clone)]
enum PaletteCommand {
    CreateWorkspace,
    ShowHelp,
    Workspace {
        workspace_id: String,
        action: WorkspacePaletteAction,
    },
    Item {
        identity: ItemIdentity,
        action: ItemPaletteAction,
    },
    Attention {
        workspace_id: String,
        shell_id: String,
        agent_id: String,
    },
    Schedule {
        schedule_id: String,
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
    CancelExecution(String),
    SelectExecution(String),
    OpenExecution(String),
    LoadHistory,
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
            false,
        )
    }

    fn new_with_schedules(
        workspaces: &[WorkspaceView],
        schedules: &[ScheduleView],
        scheduling: &SchedulingView,
        exact_run_attachment: bool,
    ) -> Self {
        let mut entries = vec![
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
                        workspace_id: workspace.id.clone(),
                        action,
                    },
                });
            }

            for item in &workspace.items {
                if !item.ordinary_visible() {
                    continue;
                }
                let kind = item.kind();
                let identity = ItemIdentity {
                    workspace_id: workspace.id.clone(),
                    item_id: item.id().to_owned(),
                    launcher: kind == ItemKind::Launcher,
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
                };
                for (action, action_group) in [
                    (ItemPaletteAction::GoTo, PaletteActionGroup::GoTo),
                    (ItemPaletteAction::Open, PaletteActionGroup::Open),
                    (ItemPaletteAction::Rename, PaletteActionGroup::Rename),
                    (ItemPaletteAction::Close, PaletteActionGroup::Close),
                ] {
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
                            workspace_id: workspace.id.clone(),
                            shell_id: attention.shell_id.clone(),
                            agent_id: attention.agent_id.clone(),
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
                    SchedulePaletteAction::LoadHistory,
                    PaletteActionGroup::Manage,
                    "load scoped history",
                ),
                (
                    SchedulePaletteAction::Remove,
                    PaletteActionGroup::Close,
                    "remove with confirmation",
                ),
            ] {
                entries.push(PaletteEntry {
                    action_group,
                    kind_group: PaletteKindGroup::Schedules,
                    label: format!("{} / {}", schedule.workspace, schedule.name),
                    detail: detail.into(),
                    keywords: keywords.clone(),
                    command: PaletteCommand::Schedule {
                        schedule_id: schedule.id.clone(),
                        action,
                    },
                });
            }
            for execution in schedule.executions.iter().take(5) {
                entries.push(PaletteEntry {
                    action_group: PaletteActionGroup::GoTo,
                    kind_group: PaletteKindGroup::Executions,
                    label: format!(
                        "{} / {} / {}",
                        schedule.workspace,
                        schedule.name,
                        short_id(&execution.id)
                    ),
                    detail: execution_summary(execution),
                    keywords: format!(
                        "execution {} {} {keywords}",
                        execution.id,
                        execution.state.label()
                    ),
                    command: PaletteCommand::Schedule {
                        schedule_id: schedule.id.clone(),
                        action: SchedulePaletteAction::SelectExecution(execution.id.clone()),
                    },
                });
                if exact_run_attachment && execution.is_openable() {
                    entries.push(PaletteEntry {
                        action_group: PaletteActionGroup::Open,
                        kind_group: PaletteKindGroup::Executions,
                        label: format!(
                            "Open {} / {} / {}",
                            schedule.workspace,
                            schedule.name,
                            short_id(&execution.id)
                        ),
                        detail: "open exact retained execution run".into(),
                        keywords: format!("open execution {} {keywords}", execution.id),
                        command: PaletteCommand::Schedule {
                            schedule_id: schedule.id.clone(),
                            action: SchedulePaletteAction::OpenExecution(execution.id.clone()),
                        },
                    });
                }
                if execution.state.is_active() {
                    entries.push(PaletteEntry {
                        action_group: PaletteActionGroup::Manage,
                        kind_group: PaletteKindGroup::Executions,
                        label: format!(
                            "Cancel {} / {} / {}",
                            schedule.workspace,
                            schedule.name,
                            short_id(&execution.id)
                        ),
                        detail: "confirm cancellation of this exact execution".into(),
                        keywords: format!("execution active cancel {} {keywords}", execution.id),
                        command: PaletteCommand::Schedule {
                            schedule_id: schedule.id.clone(),
                            action: SchedulePaletteAction::CancelExecution(execution.id.clone()),
                        },
                    });
                }
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
                        schedule_id: schedule.id.clone(),
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
                            schedule_id: schedule.id.clone(),
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
        Self {
            workspaces,
            schedules: Vec::new(),
            scheduling: SchedulingView::Unsupported {
                required_protocol: 22,
                negotiated: 0,
            },
            exact_run_attachment: false,
            selected_execution_id: None,
            workspace_state,
            item_state,
            global_state: TableState::default(),
            primary_tab: PrimaryTab::Workspaces,
            focus: Focus::Workspaces,
            mode: Mode::Normal,
            message: None,
            pending_close: None,
            project_context,
            terminal_preview: None,
            transcript: None,
            transcript_scroll_from_bottom: 0,
            follow_focused_terminal: false,
            selection_pinned: false,
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
            self.observed_focus_revision = None;
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
        let Some(workspace_index) = self
            .workspaces
            .iter()
            .position(|workspace| workspace.id == focused.workspace_id)
        else {
            return;
        };
        let Some(item_index) = self.workspaces[workspace_index]
            .items
            .iter()
            .position(|item| {
                !matches!(item, WorkspaceItemView::Launcher(_))
                    && item.id() == focused.shell_id
                    && (self.primary_tab != PrimaryTab::Workspaces || item.ordinary_visible())
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
            let identity = item_identity(&self.workspaces[workspace_index], item);
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
        self.message = None;
    }

    fn transcript_max_scroll(&self) -> usize {
        self.transcript.as_ref().map_or(0, |transcript| {
            transcript
                .lines
                .iter()
                .map(String::len)
                .sum::<usize>()
                .saturating_add(transcript.lines.len())
        })
    }

    fn scroll_transcript_up(&mut self, lines: usize) {
        self.transcript_scroll_from_bottom = self
            .transcript_scroll_from_bottom
            .saturating_add(lines)
            .min(self.transcript_max_scroll());
    }

    fn scroll_transcript_down(&mut self, lines: usize) {
        self.transcript_scroll_from_bottom =
            self.transcript_scroll_from_bottom.saturating_sub(lines);
    }

    fn scroll_transcript_to_start(&mut self) {
        self.transcript_scroll_from_bottom = self.transcript_max_scroll();
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
        self.focus = Focus::Items;
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
        if self.primary_tab != PrimaryTab::Workspaces {
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
        if self.primary_tab != PrimaryTab::Workspaces {
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
            self.exact_run_attachment,
        ));
        self.message = None;
    }

    fn select_workspace(&mut self, workspace_id: &str, focus: Focus) -> bool {
        let Some(index) = self
            .workspaces
            .iter()
            .position(|workspace| workspace.id == workspace_id)
        else {
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
        let Some(workspace_index) = self
            .workspaces
            .iter()
            .position(|workspace| workspace.id == identity.workspace_id)
        else {
            self.message = Some(Message {
                text: "item workspace is no longer available".into(),
                error: true,
            });
            return false;
        };
        let Some(item_index) = self.workspaces[workspace_index]
            .items
            .iter()
            .position(|item| item_matches(item, identity))
        else {
            self.select_workspace(&identity.workspace_id, Focus::Workspaces);
            self.message = Some(Message {
                text: "item is no longer available; selected its workspace".into(),
                error: true,
            });
            return false;
        };
        self.select_tab(PrimaryTab::Workspaces);
        self.workspace_state.select(Some(workspace_index));
        self.item_state.select(Some(item_index));
        self.set_focus(Focus::Items);
        true
    }

    fn select_schedule_id(&mut self, schedule_id: &str) -> bool {
        let Some(index) = self
            .schedules
            .iter()
            .position(|schedule| schedule.id == schedule_id)
        else {
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
        true
    }

    fn handle_focus_key(&mut self, key: KeyCode) -> bool {
        if self.primary_tab != PrimaryTab::Workspaces {
            return false;
        }
        let focus = match key {
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

    fn request_rename(&mut self) {
        if self.primary_tab == PrimaryTab::Schedules {
            self.message = Some(Message {
                text: "Schedule editing is not available; remove and recreate it with `boomux schedule create --help`".into(),
                error: false,
            });
            return;
        }
        let target = if self.primary_tab != PrimaryTab::Workspaces {
            self.selected_item()
                .filter(|item| item.ordinary_visible())
                .map(item_rename_target)
        } else {
            match self.focus {
                Focus::Workspaces => self
                    .selected()
                    .map(|workspace| RenameTarget::Workspace(workspace.id.clone())),
                Focus::Items => self
                    .selected_item()
                    .filter(|item| item.ordinary_visible())
                    .map(item_rename_target),
            }
        };
        if let Some(target) = target {
            self.mode = Mode::Rename {
                target,
                input: String::new(),
            };
            self.message = None;
        }
    }

    fn request_add(&mut self) -> Option<DashboardEffect> {
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
                let workspace_id = self.selected().map(|workspace| workspace.id.clone())?;
                Some(DashboardEffect::CreateShell(workspace_id))
            }
        }
    }

    fn create_workspace(&mut self, name: &str, default_cwd: Option<PathBuf>) -> DashboardEffect {
        self.mode = Mode::Normal;
        DashboardEffect::CreateWorkspace {
            name: name.to_owned(),
            default_cwd,
        }
    }

    fn rename(&mut self, target: RenameTarget, name: String) -> DashboardEffect {
        self.mode = Mode::Normal;
        DashboardEffect::Rename { target, name }
    }

    fn restore_selected(&self) -> Option<DashboardEffect> {
        if self.primary_tab != PrimaryTab::Workspaces {
            return None;
        }
        self.selected()
            .map(|workspace| DashboardEffect::RestoreWorkspace(workspace.id.clone()))
    }

    fn open_selected_item(&self) -> Option<DashboardEffect> {
        if matches!(self.selected_item(), Some(WorkspaceItemView::AgentShell(agent)) if agent.schedule_id.is_some())
        {
            return None;
        }
        let workspace_id = self
            .selected_item_workspace()
            .map(|workspace| workspace.id.clone())?;
        let target = self.selected_item().map(|item| match item {
            WorkspaceItemView::Shell(shell) => OpenTarget::Shell(shell.id.clone()),
            WorkspaceItemView::AgentShell(agent_shell) => {
                OpenTarget::Shell(agent_shell.shell.id.clone())
            }
            WorkspaceItemView::Launcher(launcher) => OpenTarget::Launcher {
                workspace_id,
                launcher_id: launcher.id.clone(),
            },
        })?;
        Some(DashboardEffect::Open(target))
    }

    fn request_close(&mut self) {
        if self.primary_tab == PrimaryTab::Schedules {
            self.pending_close = self.selected_schedule().map(|schedule| PendingClose {
                target: CloseTarget::Schedule(schedule.id.clone()),
                name: schedule.name.clone(),
                shell_count: 0,
                launcher_count: 0,
            });
            return;
        }
        self.pending_close = if self.primary_tab != PrimaryTab::Workspaces {
            self.selected_item()
                .filter(|item| item.ordinary_visible())
                .map(item_pending_close)
        } else {
            match self.focus {
                Focus::Workspaces => self.selected().map(|workspace| PendingClose {
                    target: CloseTarget::Workspace(workspace.id.clone()),
                    name: workspace.name.clone(),
                    shell_count: workspace.process_count(),
                    launcher_count: workspace.launcher_count(),
                }),
                Focus::Items => self
                    .selected_item()
                    .filter(|item| item.ordinary_visible())
                    .map(item_pending_close),
            }
        };
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

    fn request_cancel_execution(&mut self, execution_id: String, label: String) {
        self.pending_close = Some(PendingClose {
            target: CloseTarget::Execution(execution_id),
            name: label,
            shell_count: 0,
            launcher_count: 0,
        });
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
            DashboardEvent::RefreshCompleted(result) => self.apply_refresh(result),
            DashboardEvent::ScheduleHistoryCompleted {
                schedule_id,
                result,
            } => match result {
                Ok((executions, truncated)) => {
                    if let Some(schedule) = self
                        .schedules
                        .iter_mut()
                        .find(|schedule| schedule.id == schedule_id)
                    {
                        schedule.executions = executions;
                        schedule.history_truncated = truncated;
                        schedule.history_scoped = true;
                        schedule.history_complete = !truncated;
                        self.message = Some(Message {
                            text: "Loaded bounded history for the selected schedule".into(),
                            error: false,
                        });
                        self.sync_selected_execution();
                    }
                }
                Err(text) => self.message = Some(Message { text, error: true }),
            },
            DashboardEvent::TranscriptCompleted(result) => match result {
                Ok(transcript) => {
                    self.transcript = Some(transcript);
                    self.transcript_scroll_from_bottom = 0;
                    self.mode = Mode::Transcript;
                }
                Err(text) => self.message = Some(Message { text, error: true }),
            },
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
        if matches!(self.mode, Mode::Transcript) {
            if modifiers.difference(KeyModifiers::SHIFT).is_empty() {
                match code {
                    KeyCode::Esc | KeyCode::Char('q' | 't') => {
                        self.mode = Mode::Normal;
                        self.transcript = None;
                    }
                    KeyCode::Up | KeyCode::Char('k') => self.scroll_transcript_up(1),
                    KeyCode::Down | KeyCode::Char('j') => self.scroll_transcript_down(1),
                    KeyCode::PageUp => self.scroll_transcript_up(10),
                    KeyCode::PageDown => self.scroll_transcript_down(10),
                    KeyCode::Home => self.scroll_transcript_to_start(),
                    KeyCode::End => self.transcript_scroll_from_bottom = 0,
                    _ => {}
                }
            }
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
            KeyCode::Down | KeyCode::Char('j') => self.next(),
            KeyCode::Up | KeyCode::Char('k') => self.previous(),
            KeyCode::PageUp => self.scroll_terminal_preview_up(),
            KeyCode::PageDown => self.scroll_terminal_preview_down(),
            KeyCode::Home => self.scroll_terminal_preview_to_start(),
            KeyCode::End => self.scroll_terminal_preview_to_end(),
            KeyCode::Enter => {
                if self.primary_tab == PrimaryTab::Schedules {
                    return self.open_selected_schedule_link();
                }
                return if self.primary_tab != PrimaryTab::Workspaces {
                    self.open_selected_item()
                } else {
                    match self.focus {
                        Focus::Workspaces => self.restore_selected(),
                        Focus::Items => self.open_selected_item(),
                    }
                };
            }
            KeyCode::Char('r') => return Some(DashboardEffect::Refresh),
            KeyCode::Char('[') if self.primary_tab == PrimaryTab::Schedules => {
                self.cycle_execution(false);
            }
            KeyCode::Char(']') if self.primary_tab == PrimaryTab::Schedules => {
                self.cycle_execution(true);
            }
            KeyCode::Char('u') if self.primary_tab == PrimaryTab::Schedules => {
                return self
                    .selected_schedule()
                    .map(|schedule| DashboardEffect::RunSchedule(schedule.id.clone()));
            }
            KeyCode::Char('p') if self.primary_tab == PrimaryTab::Schedules => {
                return self
                    .selected_schedule()
                    .map(|schedule| match schedule.state {
                        ScheduleDisplayState::Paused => {
                            DashboardEffect::ResumeSchedule(schedule.id.clone())
                        }
                        ScheduleDisplayState::Enabled => {
                            DashboardEffect::PauseSchedule(schedule.id.clone())
                        }
                    });
            }
            KeyCode::Char('c') if self.primary_tab == PrimaryTab::Schedules => {
                if let Some(execution) = self
                    .selected_execution()
                    .filter(|execution| execution.state.is_active())
                    .cloned()
                {
                    self.request_cancel_execution(
                        execution.id.clone(),
                        format!("execution {}", short_id(&execution.id)),
                    );
                }
                return None;
            }
            KeyCode::Char('h') if self.primary_tab == PrimaryTab::Schedules => {
                return self.selected_schedule().map(|schedule| {
                    DashboardEffect::LoadScheduleHistory {
                        schedule_id: schedule.id.clone(),
                        limit: 100,
                    }
                });
            }
            KeyCode::Char('t') if self.primary_tab == PrimaryTab::Schedules => {
                return self
                    .selected_execution()
                    .filter(|execution| {
                        execution.transcript_available && execution.session_id.is_some()
                    })
                    .map(|execution| DashboardEffect::ReadExecutionTranscript {
                        session_id: execution
                            .session_id
                            .clone()
                            .expect("transcript-capable execution has a session"),
                        execution_id: execution.id.clone(),
                    });
            }
            KeyCode::Char('x') => self.request_close(),
            KeyCode::Char('a') => return self.request_add(),
            KeyCode::Char('e') => self.request_rename(),
            KeyCode::Char(' ') => self.toggle_selection_pin(),
            KeyCode::Char('/' | ':') => self.open_palette(),
            KeyCode::Char('?') => self.mode = Mode::Help,
            KeyCode::Tab => self.cycle_tab(false),
            KeyCode::BackTab => self.cycle_tab(true),
            KeyCode::Char(key) if shortcut_tab(key).is_some() => {
                self.select_tab(shortcut_tab(key).expect("validated tab shortcut"));
            }
            key if self.handle_focus_key(key) => {}
            _ => {}
        }
        None
    }

    fn apply_refresh(&mut self, result: Result<DashboardState, String>) {
        match result {
            Ok(state) => {
                self.replace_workspaces(state.workspaces);
                self.replace_schedules(state.schedules);
                self.scheduling = state.scheduling;
                self.exact_run_attachment = state.exact_run_attachment;
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
            self.selected_item().and_then(|item| match item {
                WorkspaceItemView::Shell(shell) if shell.kind == TerminalKind::Shell => Some((
                    shell.id.clone(),
                    shell.run.as_ref().map(|run| run.id.clone()),
                    shell.run.as_ref().map_or(0, |run| run.output_revision),
                )),
                WorkspaceItemView::Shell(_)
                | WorkspaceItemView::AgentShell(_)
                | WorkspaceItemView::Launcher(_) => None,
            })
        };
        let Some((shell_id, run_id, output_revision)) = selected else {
            self.terminal_preview = None;
            return None;
        };
        if self.terminal_preview.as_ref().is_some_and(|preview| {
            preview.shell_id == shell_id
                && preview.run_id == run_id
                && preview.output_revision == output_revision
                && preview.output.is_ok()
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
        let selected_id = self.selected().map(|workspace| workspace.id.clone());
        let selected_item = self.workspace_item_identity();
        let selected_global_item = self.global_item_identity();
        let previous_index = self.selected_index().unwrap_or(0);
        let selected_index = selected_id
            .and_then(|id| workspaces.iter().position(|workspace| workspace.id == id))
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
        if self.primary_tab != PrimaryTab::Workspaces {
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
        let selected = self.selected_schedule().map(|schedule| schedule.id.clone());
        let previous = self.global_state.selected().unwrap_or(0);
        self.schedules = schedules;
        if self.primary_tab == PrimaryTab::Schedules {
            self.global_state.select(
                selected
                    .and_then(|id| self.schedules.iter().position(|schedule| schedule.id == id))
                    .or_else(|| {
                        (!self.schedules.is_empty())
                            .then_some(previous.min(self.schedules.len() - 1))
                    }),
            );
            self.sync_selected_execution();
        }
    }

    fn open_selected_schedule_link(&mut self) -> Option<DashboardEffect> {
        let execution = self.selected_execution()?.clone();
        if execution.agent_state == Some(AgentDisplayState::Blocked) {
            if let Some(agent_id) = execution.agent_id.as_deref()
                && self.select_agent_id(agent_id)
            {
                return None;
            }
            self.message = Some(Message {
                text: format!(
                    "Exact linked Agent {} is not a current dashboard row; inspect it with `boomux agent inspect {}`",
                    execution.agent_id.as_deref().unwrap_or("-"),
                    execution.agent_id.as_deref().unwrap_or("-")
                ),
                error: false,
            });
            return None;
        }
        self.open_selected_execution_run()
    }

    fn open_selected_execution_run(&mut self) -> Option<DashboardEffect> {
        let execution = self.selected_execution()?.clone();
        if !self.exact_run_attachment {
            self.message = Some(Message {
                text: "Opening exact Scheduled Execution runs requires daemon protocol 26; upgrade and restart Boomux"
                    .into(),
                error: false,
            });
            return None;
        }
        if !execution.is_openable() {
            self.message = Some(Message {
                text: "Selected execution is not a Starting or Active exact shell run".into(),
                error: false,
            });
            return None;
        }
        match (execution.shell_id, execution.run_id) {
            (Some(shell_id), Some(run_id)) => Some(DashboardEffect::OpenScheduledExecution {
                execution_id: execution.id,
                shell_id,
                run_id,
            }),
            _ => {
                self.message = Some(Message {
                    text: "Selected execution has no exact retained shell run to open".into(),
                    error: false,
                });
                None
            }
        }
    }

    fn select_agent_id(&mut self, agent_id: &str) -> bool {
        let Some((workspace_index, item_index)) = self.workspaces.iter().enumerate().find_map(|(workspace_index, workspace)| {
            workspace.items.iter().enumerate().find_map(|(item_index, item)| {
                matches!(item, WorkspaceItemView::AgentShell(agent) if agent.agent.as_ref().is_some_and(|agent| agent.id == agent_id))
                    .then_some((workspace_index, item_index))
            })
        }) else {
            return false;
        };
        self.select_tab(PrimaryTab::Agents);
        let identity = item_identity(
            &self.workspaces[workspace_index],
            &self.workspaces[workspace_index].items[item_index],
        );
        self.global_state
            .select(self.global_item_position(&identity));
        true
    }

    fn workspace_item_identity(&self) -> Option<ItemIdentity> {
        let workspace = self.selected()?;
        let item = workspace.items.get(self.item_state.selected()?)?;
        Some(item_identity(workspace, item))
    }

    fn global_item_identity(&self) -> Option<ItemIdentity> {
        if self.primary_tab == PrimaryTab::Workspaces {
            return None;
        }
        let (workspace, item) = self.selected_item_location()?;
        Some(item_identity(
            &self.workspaces[workspace],
            &self.workspaces[workspace].items[item],
        ))
    }

    fn global_item_position(&self, identity: &ItemIdentity) -> Option<usize> {
        (0..self.global_item_count()).position(|ordinal| {
            self.global_item_location(ordinal)
                .is_some_and(|(workspace, item)| {
                    let workspace = &self.workspaces[workspace];
                    item_matches(&workspace.items[item], identity)
                        && workspace.id == identity.workspace_id
                })
        })
    }
}

fn item_identity(workspace: &WorkspaceView, item: &WorkspaceItemView) -> ItemIdentity {
    let (item_id, launcher) = match item {
        WorkspaceItemView::Shell(shell) => (shell.id.clone(), false),
        WorkspaceItemView::AgentShell(agent) => (agent.shell.id.clone(), false),
        WorkspaceItemView::Launcher(launcher) => (launcher.id.clone(), true),
    };
    ItemIdentity {
        workspace_id: workspace.id.clone(),
        item_id,
        launcher,
    }
}

fn item_matches(item: &WorkspaceItemView, identity: &ItemIdentity) -> bool {
    match item {
        WorkspaceItemView::Shell(shell) => !identity.launcher && shell.id == identity.item_id,
        WorkspaceItemView::AgentShell(agent) => {
            !identity.launcher && agent.shell.id == identity.item_id
        }
        WorkspaceItemView::Launcher(launcher) => {
            identity.launcher && launcher.id == identity.item_id
        }
    }
}

fn item_rename_target(item: &WorkspaceItemView) -> RenameTarget {
    match item {
        WorkspaceItemView::Shell(shell) => RenameTarget::Shell(shell.id.clone()),
        WorkspaceItemView::AgentShell(agent) => RenameTarget::Shell(agent.shell.id.clone()),
        WorkspaceItemView::Launcher(launcher) => RenameTarget::Launcher(launcher.id.clone()),
    }
}

fn item_pending_close(item: &WorkspaceItemView) -> PendingClose {
    match item {
        WorkspaceItemView::Shell(shell) => PendingClose {
            target: CloseTarget::Shell(shell.id.clone()),
            name: shell.name.clone(),
            shell_count: 1,
            launcher_count: 0,
        },
        WorkspaceItemView::AgentShell(agent) => PendingClose {
            target: CloseTarget::Shell(agent.shell.id.clone()),
            name: agent.shell.name.clone(),
            shell_count: 1,
            launcher_count: 0,
        },
        WorkspaceItemView::Launcher(launcher) => PendingClose {
            target: CloseTarget::Launcher(launcher.id.clone()),
            name: launcher.name.clone(),
            shell_count: 0,
            launcher_count: 1,
        },
    }
}

pub(crate) fn run<B: DashboardBackend>(
    state: DashboardState,
    follow_focused_terminal: bool,
    project_context: ProjectContext,
    play_intro: bool,
    backend: B,
) -> io::Result<()> {
    let mut terminal = ratatui::init();
    let mut app = App::new(state.workspaces, project_context);
    app.schedules = state.schedules;
    app.scheduling = state.scheduling;
    app.exact_run_attachment = state.exact_run_attachment;
    if follow_focused_terminal {
        app.enable_focus_following(state.focused_terminal.as_ref());
    }
    let result = if play_intro {
        play_bomb_animation(&mut terminal).and_then(|()| run_loop(&mut terminal, app, backend))
    } else {
        run_loop(&mut terminal, app, backend)
    };
    ratatui::restore();
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

fn run_loop<B: DashboardBackend>(
    terminal: &mut ratatui::DefaultTerminal,
    mut app: App,
    mut backend: B,
) -> io::Result<()> {
    let mut last_refresh = Instant::now();
    loop {
        if last_refresh.elapsed() >= UPDATE_CHECK_INTERVAL {
            let effects = app.update(DashboardEvent::RefreshElapsed);
            execute_effects(&mut app, &mut backend, effects);
            last_refresh = Instant::now();
        }
        let effects = app.update(DashboardEvent::PreviewRequested);
        execute_effects(&mut app, &mut backend, effects);
        terminal.draw(|frame| render(frame, &mut app))?;

        if !event::poll(Duration::from_millis(250))? {
            continue;
        }
        let Event::Key(key) = event::read()? else {
            continue;
        };
        if key.kind != KeyEventKind::Press {
            continue;
        }
        let effects = app.update(DashboardEvent::KeyPressed {
            code: key.code,
            modifiers: key.modifiers,
        });
        if effects.contains(&DashboardEffect::Quit) {
            return Ok(());
        }
        if !effects.is_empty() {
            execute_effects(&mut app, &mut backend, effects);
        }
        // Keep navigation responsive by waiting for an idle input window before
        // performing the next synchronous background refresh.
        last_refresh = Instant::now();
    }
}

fn execute_effects(
    app: &mut App,
    backend: &mut impl DashboardBackend,
    effects: Vec<DashboardEffect>,
) {
    let mut pending = effects;
    while let Some(effect) = pending.pop() {
        if effect == DashboardEffect::Quit {
            continue;
        }
        pending.extend(app.update(backend.execute(effect)));
    }
}

fn execute_palette_command(app: &mut App, command: PaletteCommand) -> Option<DashboardEffect> {
    match command {
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
                launcher: false,
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
                SchedulePaletteAction::OpenExecution(execution_id) => {
                    if !app.select_execution_id(&execution_id) {
                        return None;
                    }
                    app.open_selected_execution_run()
                }
                SchedulePaletteAction::CancelExecution(execution_id) => {
                    if !app.select_execution_id(&execution_id) {
                        return None;
                    }
                    let execution = app
                        .selected_execution()
                        .filter(|execution| execution.state.is_active())
                        .cloned()?;
                    app.request_cancel_execution(
                        execution.id.clone(),
                        format!("execution {}", short_id(&execution.id)),
                    );
                    None
                }
                SchedulePaletteAction::LoadHistory => Some(DashboardEffect::LoadScheduleHistory {
                    schedule_id,
                    limit: 100,
                }),
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
    if !modifiers.difference(KeyModifiers::SHIFT).is_empty() {
        app.mode = mode;
        return None;
    }
    match mode {
        Mode::Normal => None,
        Mode::Palette(_) | Mode::Help | Mode::Transcript => None,
        Mode::PickProject(mut picker) => match key {
            KeyCode::Enter
                if picker.mode == WorkspaceCreationMode::ByName
                    && picker.custom_name().is_some() =>
            {
                let name = picker
                    .custom_name()
                    .expect("nonempty workspace name")
                    .to_owned();
                Some(app.create_workspace(&name, None))
            }
            KeyCode::Enter if picker.selected().is_some() => {
                let project = picker.selected().expect("selected project").clone();
                Some(app.create_workspace(&project.name, Some(project.path)))
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
    if app.primary_tab == PrimaryTab::Schedules {
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
        Mode::Transcript => render_transcript(frame, area, app),
        Mode::Normal | Mode::Rename { .. } => {}
    }
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
        Line::from("  Enter    restore a workspace, open a shell, or invoke a launcher"),
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
                Line::from("  u runs now; p pauses/resumes; c cancels only its exact active execution."),
                Line::from("  [ and ] select newer/older retained executions by exact execution ID."),
                Line::from("  h explicitly loads scoped bounded history; t explicitly reads linked host content."),
                Line::from("  Enter opens an exact retained shell or navigates to the exact blocked Agent."),
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
        Span::styled("      ALL: ", Style::new().fg(SUBTEXT)),
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
            .map(|workspace| Row::new([Cell::from(workspace.name.as_str())]))
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
    let detail_height = if area.height >= 18 && app.selected_schedule().is_some() {
        11.min(area.height.saturating_sub(6))
    } else if area.height >= 4 && app.selected_schedule().is_some() {
        3
    } else {
        0
    };
    let [table_area, detail_area] =
        Layout::vertical([Constraint::Fill(1), Constraint::Length(detail_height)]).areas(area);
    let (headers, widths): (Vec<&str>, Vec<Constraint>) = if area.width >= 140 {
        (
            vec![
                "NAME",
                "TRIGGER",
                "NEXT",
                "LAST",
                "STATE",
                "WORKSPACE",
                "INTEGRATION",
            ],
            vec![
                Constraint::Length(22),
                Constraint::Length(25),
                Constraint::Length(13),
                Constraint::Length(18),
                Constraint::Length(9),
                Constraint::Length(20),
                Constraint::Min(10),
            ],
        )
    } else if area.width >= 114 {
        (
            vec!["NAME", "TRIGGER", "NEXT", "LAST", "STATE", "WORKSPACE"],
            vec![
                Constraint::Length(19),
                Constraint::Length(23),
                Constraint::Length(12),
                Constraint::Length(17),
                Constraint::Length(9),
                Constraint::Min(14),
            ],
        )
    } else if area.width >= 80 {
        (
            vec!["NAME", "TRIGGER", "NEXT", "LAST", "STATE"],
            vec![
                Constraint::Length(17),
                Constraint::Length(20),
                Constraint::Length(11),
                Constraint::Length(16),
                Constraint::Min(8),
            ],
        )
    } else {
        (
            vec!["NAME", "NEXT", "LAST", "STATE"],
            vec![
                Constraint::Length(16),
                Constraint::Length(10),
                Constraint::Length(15),
                Constraint::Min(8),
            ],
        )
    };
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
    let rows: Vec<_> = {
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
                let mut values = vec![schedule.name.clone()];
                if area.width >= 80 {
                    values.push(schedule.friendly_trigger.clone());
                }
                values.extend([next, last, schedule.state.label().into()]);
                if area.width >= 114 {
                    values.push(schedule.workspace.clone());
                }
                if area.width >= 140 {
                    values.push(schedule.integration.clone());
                }
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
            .collect()
    };
    let health = match app.scheduling {
        SchedulingView::Unsupported { .. } => "unsupported".into(),
        SchedulingView::Active { active, maximum } => format!("active {active}/{maximum}"),
        SchedulingView::Offline { active, maximum } => format!("offline {active}/{maximum}"),
    };
    let table = Table::new(rows, widths)
        .header(Row::new(headers).style(Style::new().fg(BLUE).add_modifier(Modifier::BOLD)))
        .column_spacing(1)
        .block(
            Block::bordered()
                .title(format!(
                    " Schedules ({}) · scheduler {health} ",
                    app.schedules.len()
                ))
                .border_style(Style::new().fg(TEAL)),
        )
        .row_highlight_style(
            Style::new()
                .fg(TEXT)
                .add_modifier(Modifier::BOLD | Modifier::REVERSED),
        )
        .highlight_symbol("> ");
    frame.render_stateful_widget(table, table_area, &mut app.global_state);

    if detail_height > 0
        && let Some(schedule) = app.selected_schedule()
    {
        render_schedule_detail(
            frame,
            detail_area,
            schedule,
            &app.scheduling,
            app.selected_execution_id.as_deref(),
            app.exact_run_attachment,
        );
    }
}

fn render_schedule_detail(
    frame: &mut Frame,
    area: Rect,
    schedule: &ScheduleView,
    scheduling: &SchedulingView,
    selected_execution_id: Option<&str>,
    exact_run_attachment: bool,
) {
    if area.height <= 4 {
        let line = selected_execution_id
            .and_then(|id| {
                schedule
                    .executions
                    .iter()
                    .find(|execution| execution.id == id)
            })
            .map_or_else(
                || "No retained execution selected · h history".into(),
                |execution| {
                    let mut actions = Vec::new();
                    if exact_run_attachment && execution.is_openable() {
                        actions.push("Enter open");
                    }
                    if execution.state.is_active() {
                        actions.push("c cancel");
                    }
                    if execution.transcript_available && execution.session_id.is_some() {
                        actions.push("t transcript");
                    }
                    if actions.is_empty() {
                        actions.push("no execution action");
                    }
                    format!(
                        "> {}  {}  ·  {}",
                        short_id(&execution.id),
                        execution_summary(execution),
                        actions.join(" · ")
                    )
                },
            );
        frame.render_widget(
            Paragraph::new(line).block(
                Block::bordered()
                    .title(format!(" {} · selected execution ", schedule.name))
                    .border_style(Style::new().fg(OVERLAY)),
            ),
            area,
        );
        return;
    }
    let health = match scheduling {
        SchedulingView::Unsupported { .. } => "unsupported".to_owned(),
        SchedulingView::Active { active, maximum } => format!("active ({active}/{maximum})"),
        SchedulingView::Offline { active, maximum } => {
            format!("offline ({active}/{maximum}); timed dispatch is not reliable")
        }
    };
    let mut lines = vec![
        preview_field("PROMPT REV", schedule.prompt_revision.to_string()),
        preview_field(
            "TRIGGER",
            format!("{}  ·  {}", schedule.exact_trigger, schedule.timezone),
        ),
        preview_field(
            "POLICY",
            "skip overlap  ·  no automatic retry  ·  no timeout",
        ),
        preview_field("SCHEDULER", health),
    ];
    if let Some(execution) = selected_execution_id.and_then(|id| {
        schedule
            .executions
            .iter()
            .find(|execution| execution.id == id)
    }) {
        lines.push(preview_field("ACTION", execution_explanation(execution)));
    } else if schedule.history_complete {
        lines.push(preview_field(
            "ACTION",
            if schedule.state == ScheduleDisplayState::Paused {
                "Never run; press u for one authorized run or p to enable future timed work"
            } else {
                "Never run; waiting for the next occurrence, or press u for an authorized run now"
            },
        ));
    } else {
        lines.push(preview_field(
            "ACTION",
            "Execution history is incomplete; press h to load exact scoped history",
        ));
    }
    let history_label = if schedule.history_truncated {
        "newest page is truncated; press h for exact scoped history"
    } else if schedule.possible_pruning_boundary {
        "oldest retained record may be a pruning boundary"
    } else if schedule.history_scoped {
        "scoped bounded history"
    } else {
        "recent bounded history"
    };
    lines.push(preview_field("HISTORY", history_label));
    for execution in selected_execution_window(&schedule.executions, selected_execution_id, 3) {
        let links = [
            execution
                .shell_id
                .as_deref()
                .map(|id| format!("shell {}", short_id(id))),
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
        lines.push(Line::from(format!(
            "{} {}  {:<16} {}{}",
            if selected_execution_id == Some(execution.id.as_str()) {
                ">"
            } else {
                " "
            },
            compact_recency(execution.requested_at_ms),
            execution_summary(execution),
            short_id(&execution.id),
            if links.is_empty() {
                String::new()
            } else {
                format!("  ·  {links}")
            }
        )));
    }
    frame.render_widget(
        Paragraph::new(lines).block(
            Block::bordered()
                .title(format!(" {} · {} ", schedule.workspace, schedule.name))
                .border_style(Style::new().fg(OVERLAY)),
        ),
        area,
    );
}

fn selected_execution_window<'a>(
    executions: &'a [ExecutionView],
    selected_execution_id: Option<&str>,
    limit: usize,
) -> &'a [ExecutionView] {
    if executions.is_empty() || limit == 0 {
        return &[];
    }
    let length = limit.min(executions.len());
    let selected = selected_execution_id
        .and_then(|id| executions.iter().position(|execution| execution.id == id))
        .unwrap_or(0);
    let start = selected
        .saturating_sub(length / 2)
        .min(executions.len() - length);
    &executions[start..start + length]
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

fn execution_explanation(execution: &ExecutionView) -> String {
    if execution.agent_state == Some(AgentDisplayState::Blocked) {
        return format!(
            "Blocked Agent {}; Enter navigates only to that exact linked Agent",
            execution
                .agent_id
                .as_deref()
                .map(short_id)
                .unwrap_or_else(|| "-".into())
        );
    }
    match execution.reason {
        Some(ExecutionReasonDisplay::Overlap) => "Skipped because this schedule already had active work; cancel only the exact active execution".into(),
        Some(ExecutionReasonDisplay::ActiveSession) => "Skipped because the exact continuation session was active; inspect its linked Agent/session".into(),
        Some(ExecutionReasonDisplay::WorkspaceCapacity) => "Skipped because another execution occupied this workspace".into(),
        Some(ExecutionReasonDisplay::GlobalCapacity) => "Skipped at the daemon concurrency limit".into(),
        Some(ExecutionReasonDisplay::Missed) => "Missed while the scheduler was unavailable; no catch-up or retry is automatic".into(),
        Some(ExecutionReasonDisplay::PausedRace) => "Pause won during evaluation; resume only to authorize future occurrences".into(),
        Some(ExecutionReasonDisplay::InvalidTarget) => "Target is invalid; inspect integration status and daemon diagnostics".into(),
        Some(ExecutionReasonDisplay::RunnerStartFailed | ExecutionReasonDisplay::HostSpawnFailed) => "Dispatch failed; inspect daemon and integration health before a new manual run".into(),
        Some(ExecutionReasonDisplay::CancelledByUser) => "Cancelled explicitly; no replacement run is automatic".into(),
        Some(ExecutionReasonDisplay::ColdDaemonRecovery | ExecutionReasonDisplay::RunnerExitedWithoutReport | ExecutionReasonDisplay::DaemonShutdown) => "Execution was interrupted; inspect the exact retained links before retrying".into(),
        None if execution.state.is_active() => "Active with no automatic timeout; press c to cancel this exact execution".into(),
        None => "No action required; press h for bounded scoped history".into(),
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

fn render_transcript(frame: &mut Frame, area: Rect, app: &mut App) {
    let Some(transcript) = app.transcript.as_ref() else {
        return;
    };
    let popup = if area.width < 100 || area.height < 30 {
        area
    } else {
        centered_rect(area, 86, 82)
    };
    frame.render_widget(Clear, popup);
    let block = Block::bordered()
        .title(" Transcript and tool activity ")
        .border_style(Style::new().fg(TEAL));
    let inner = block.inner(popup);
    frame.render_widget(block, popup);
    let [header_area, body_area, help_area] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Fill(1),
        Constraint::Length(1),
    ])
    .areas(inner);
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            format!(
                "Explicit bounded read · execution {} · session {}",
                short_id(&transcript.execution_id),
                short_id(&transcript.session_id)
            ),
            Style::new().fg(SUBTEXT),
        ))),
        header_area,
    );
    let mut lines = transcript
        .lines
        .iter()
        .cloned()
        .map(Line::from)
        .collect::<Vec<_>>();
    if transcript.truncated {
        lines.push(Line::from(Span::styled(
            "Content is truncated by dashboard bounds",
            Style::new().fg(YELLOW),
        )));
    }
    let total_rows = wrapped_line_count(&lines, body_area.width);
    let content = Paragraph::new(lines).wrap(Wrap { trim: false });
    let visible_rows = body_area.height as usize;
    let max_scroll = total_rows.saturating_sub(visible_rows);
    app.transcript_scroll_from_bottom = app.transcript_scroll_from_bottom.min(max_scroll);
    let offset = max_scroll.saturating_sub(app.transcript_scroll_from_bottom);
    frame.render_widget(
        content.scroll((offset.min(u16::MAX as usize) as u16, 0)),
        body_area,
    );
    let end = (offset + visible_rows).min(total_rows);
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(
                format!(
                    " rows {}-{end}/{total_rows}  ",
                    offset.saturating_add(1).min(end)
                ),
                Style::new().fg(if offset == max_scroll { GREEN } else { YELLOW }),
            ),
            Span::styled("↑/↓", Style::new().fg(TEAL)),
            Span::styled(" scroll  ", Style::new().fg(SUBTEXT)),
            Span::styled("pgup/pgdn", Style::new().fg(TEAL)),
            Span::styled(" page  ", Style::new().fg(SUBTEXT)),
            Span::styled("home/end", Style::new().fg(TEAL)),
            Span::styled(" oldest/newest  ", Style::new().fg(SUBTEXT)),
            Span::styled("q/esc/t", Style::new().fg(RED)),
            Span::styled(" close", Style::new().fg(SUBTEXT)),
        ])),
        help_area,
    );
}

fn wrapped_line_count(lines: &[Line<'_>], width: u16) -> usize {
    let width = usize::from(width.max(1));
    lines
        .iter()
        .map(|line| line.width().max(1).div_ceil(width))
        .sum()
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
        let values: Vec<[String; 7]> = app
            .workspaces
            .iter()
            .flat_map(|workspace| {
                workspace.items.iter().filter_map(move |item| {
                    let WorkspaceItemView::AgentShell(agent) = item else {
                        return None;
                    };
                    let task = matched_agent_session(workspace, agent)
                        .and_then(session_task_label)
                        .unwrap_or("-");
                    let (updated, branch, worktree) = agent.agent.as_ref().map_or_else(
                        || ("-".into(), "-".into(), "-".into()),
                        |view| {
                            (
                                compact_recency(view.updated_at_ms),
                                view.root_branch.clone(),
                                view.root_worktree.clone(),
                            )
                        },
                    );
                    Some([
                        agent.state().label().to_owned(),
                        updated,
                        workspace.name.clone(),
                        agent.shell.name.clone(),
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
                |[status, updated, workspace, shell, task, branch, worktree]| {
                    Row::new([
                        Cell::from(Span::styled(
                            status.clone(),
                            Style::new().fg(status_color(&status)),
                        )),
                        Cell::from(updated),
                        Cell::from(workspace),
                        Cell::from(shell),
                        Cell::from(task),
                        Cell::from(branch),
                        Cell::from(worktree),
                    ])
                },
            )
            .collect();
        (rows, widths, AGENT_TABLE_HEADERS.to_vec())
    } else if app.primary_tab == PrimaryTab::Shells {
        let values: Vec<[String; 8]> = app
            .workspaces
            .iter()
            .flat_map(|workspace| {
                workspace.items.iter().filter_map(move |item| {
                    let WorkspaceItemView::Shell(shell) = item else {
                        return None;
                    };
                    Some([
                        shell.table_status(),
                        shell
                            .run
                            .as_ref()
                            .map_or_else(|| "-".into(), |run| format!("#{}", run.generation)),
                        workspace.name.clone(),
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
                        let mut cells = vec![Cell::from(workspace.name.clone())];
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
    let values: Vec<[String; 6]> = selected
        .into_iter()
        .flat_map(|workspace| {
            workspace
                .items
                .iter()
                .filter(|item| item.ordinary_visible())
                .map(|item| match item {
                    WorkspaceItemView::Shell(terminal) => [
                        terminal.kind.label().into(),
                        terminal.table_status(),
                        terminal.name.clone(),
                        terminal.process().into(),
                        terminal.branch.clone(),
                        terminal.worktree.clone(),
                    ],
                    WorkspaceItemView::AgentShell(agent_shell) => {
                        let (activity, branch, worktree) = agent_shell.agent.as_ref().map_or_else(
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
                            activity,
                            branch,
                            worktree,
                        ]
                    }
                    WorkspaceItemView::Launcher(launcher) => [
                        "launcher".into(),
                        "ready".into(),
                        launcher.name.clone(),
                        launcher.command.clone(),
                        launcher.branch.clone(),
                        launcher.worktree.clone(),
                    ],
                })
        })
        .collect();
    let widths = item_column_widths(items_inner.width, &values);
    let rows = values
        .into_iter()
        .map(|[kind, status, name, activity, branch, worktree]| {
            let kind_color = match kind.as_str() {
                "agent" => TEAL,
                "command" | "launcher" => YELLOW,
                _ => TEXT,
            };
            Row::new([
                Cell::from(Span::styled(kind, Style::new().fg(kind_color))),
                Cell::from(Span::styled(
                    status.clone(),
                    Style::new().fg(status_color(&status)),
                )),
                Cell::from(name),
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
    let currency = if session.state_is_current {
        "current"
    } else {
        "last known"
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
                session.state.label(),
                Style::new()
                    .fg(session_state_color(session.state))
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
        preview_field(
            "SESSION",
            format!(
                "{external_identity}  ·  {occurrences} occurrence{}  ·  shell {shell}",
                if occurrences == 1 { "" } else { "s" }
            ),
        ),
        preview_field("ROOT", root_directory),
    ];
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

fn item_column_widths(width: u16, rows: &[[String; 6]]) -> Vec<Constraint> {
    let caps = if width >= 140 {
        [10, 12, 24, 52, 32, 24]
    } else if width >= 100 {
        [10, 11, 18, 36, 24, 20]
    } else {
        [8, 10, 14, 24, 18, 16]
    };
    let minimums = ITEM_TABLE_HEADERS.map(|header| header.len() as u16);
    let mut widths: [u16; 6] = std::array::from_fn(|index| {
        rows.iter()
            .map(|row| row[index].chars().count() as u16)
            .max()
            .unwrap_or(0)
            .max(minimums[index])
            .saturating_add(2)
            .min(caps[index])
    });

    // Five column gaps and the highlight marker also consume table width.
    let available = width.saturating_sub(7);
    let mut overflow = widths.iter().sum::<u16>().saturating_sub(available);
    for index in [3, 5, 4, 2, 1, 0] {
        let reduction = widths[index].saturating_sub(minimums[index]).min(overflow);
        widths[index] -= reduction;
        overflow -= reduction;
    }

    widths.into_iter().map(Constraint::Length).collect()
}

fn shell_column_widths(width: u16, rows: &[[String; 8]]) -> Vec<Constraint> {
    let caps = if width >= 160 {
        [12, 6, 24, 20, 9, 40, 32, 24]
    } else if width >= 120 {
        [12, 6, 18, 16, 9, 28, 24, 20]
    } else if width >= 90 {
        [11, 5, 14, 13, 8, 20, 18, 16]
    } else {
        [10, 4, 11, 10, 7, 14, 12, 13]
    };
    let minimums = SHELL_TABLE_HEADERS.map(|header| header.len() as u16);
    let mut widths: [u16; 8] = std::array::from_fn(|index| {
        rows.iter()
            .map(|row| row[index].chars().count() as u16)
            .max()
            .unwrap_or(0)
            .max(minimums[index])
            .saturating_add(2)
            .min(caps[index])
    });

    // Seven column gaps and the highlight marker also consume table width.
    let available = width.saturating_sub(9);
    let mut overflow = widths.iter().sum::<u16>().saturating_sub(available);
    for index in [5, 7, 2, 3, 6, 4, 0, 1] {
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

fn agent_column_widths(width: u16, rows: &[[String; 7]]) -> Vec<Constraint> {
    let caps = if width >= 160 {
        [10, 9, 24, 16, 52, 36, 24]
    } else if width >= 140 {
        [10, 9, 20, 16, 44, 28, 20]
    } else if width >= 100 {
        [9, 8, 14, 12, 32, 18, 16]
    } else {
        [8, 7, 11, 10, 24, 12, 13]
    };
    let minimums = AGENT_TABLE_HEADERS.map(|header| header.len() as u16);
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
    for index in [4, 2, 5, 3, 6, 0, 1] {
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
    } else if let Some(message) = &app.message {
        Line::from(Span::styled(
            format!(" {}", message.text),
            Style::new().fg(if message.error { RED } else { GREEN }),
        ))
    } else {
        let launcher_selected = matches!(app.selected_item(), Some(WorkspaceItemView::Launcher(_)));
        if app.primary_tab == PrimaryTab::Schedules {
            let paused = app
                .selected_schedule()
                .is_some_and(|schedule| schedule.state == ScheduleDisplayState::Paused);
            let active = app
                .selected_execution()
                .is_some_and(|execution| execution.state.is_active());
            let transcript = app
                .selected_execution()
                .is_some_and(|execution| execution.transcript_available);
            let line = Line::from(vec![
                Span::styled(" j/k", Style::new().fg(TEAL)),
                Span::styled(" schedule  ", Style::new().fg(SUBTEXT)),
                Span::styled("[/]", Style::new().fg(TEAL)),
                Span::styled(" execution  ", Style::new().fg(SUBTEXT)),
                Span::styled("u", Style::new().fg(GREEN)),
                Span::styled(" run now  ", Style::new().fg(SUBTEXT)),
                Span::styled("p", Style::new().fg(YELLOW)),
                Span::styled(
                    if paused { " resume  " } else { " pause  " },
                    Style::new().fg(SUBTEXT),
                ),
                Span::styled(if active { "c" } else { "-" }, Style::new().fg(RED)),
                Span::styled(" cancel active  ", Style::new().fg(SUBTEXT)),
                Span::styled("h", Style::new().fg(BLUE)),
                Span::styled(" history  ", Style::new().fg(SUBTEXT)),
                Span::styled(if transcript { "t" } else { "-" }, Style::new().fg(TEAL)),
                Span::styled(" transcript  ", Style::new().fg(SUBTEXT)),
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
                    " navigate  tab/shift-tab views  1-4 select view  "
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
        spans.extend([
            Span::styled("x", Style::new().fg(RED)),
            Span::styled(
                if app.primary_tab == PrimaryTab::Workspaces && app.focus == Focus::Workspaces {
                    " close workspace  "
                } else if launcher_selected {
                    " remove launcher  "
                } else {
                    " close shell  "
                },
                Style::new().fg(SUBTEXT),
            ),
        ]);
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
        }
    }

    fn set_shell_id(workspace: &mut WorkspaceView, shell_id: &str) {
        match &mut workspace.items[0] {
            WorkspaceItemView::Shell(shell) => shell.id = shell_id.into(),
            WorkspaceItemView::AgentShell(agent) => agent.shell.id = shell_id.into(),
            WorkspaceItemView::Launcher(_) => panic!("expected shell item"),
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
            prompt_revision: 3,
            shell_id: Some("schedule-shell".into()),
            run_id: Some("schedule-run".into()),
            agent_id: Some("schedule-agent".into()),
            agent_state: Some(AgentDisplayState::Working),
            session_id: Some("schedule-session".into()),
            transcript_available: true,
        }
    }

    fn schedule_view() -> ScheduleView {
        ScheduleView {
            id: "schedule-1".into(),
            workspace_id: "w1".into(),
            workspace: "boomux".into(),
            name: "nightly review".into(),
            integration: "opencode".into(),
            cwd: "/tmp/boomux".into(),
            state: ScheduleDisplayState::Paused,
            friendly_trigger: "weekdays 09:30".into(),
            exact_trigger: "30 9 * * 1-5".into(),
            timezone: "America/New_York".into(),
            next_occurrence_ms: None,
            prompt_revision: 3,
            executions: vec![execution("execution-1", ExecutionDisplayState::Active)],
            history_truncated: false,
            possible_pruning_boundary: false,
            history_scoped: false,
            history_complete: true,
        }
    }

    fn schedule_app() -> App {
        let mut app = app();
        app.schedules = vec![schedule_view()];
        app.scheduling = SchedulingView::Active {
            active: 1,
            maximum: 4,
        };
        app.exact_run_attachment = true;
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
        let output = read(&shell_id);
        app.update(DashboardEvent::TerminalPreviewCompleted {
            shell_id,
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
            workspace_id: "w1".into(),
            shell_id: "s1".into(),
        }));
        assert_eq!(
            app.selected().map(|workspace| workspace.id.as_str()),
            Some("w1")
        );
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
            workspace_id: "w1".into(),
            shell_id: "s1".into(),
        }));

        app.toggle_selection_pin();
        app.apply_focused_terminal(Some(&FocusedTerminalView {
            revision: 2,
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
                Constraint::Length(8),
                Constraint::Length(18),
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
                Constraint::Length(11),
                Constraint::Length(10),
                Constraint::Length(7),
                Constraint::Length(7),
                Constraint::Length(12),
                Constraint::Length(10),
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
            "agent",
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
                Constraint::Length(7),
                Constraint::Length(45),
                Constraint::Length(31),
                Constraint::Length(15),
            ]
        );
        assert_eq!(
            agent_column_widths(80, &rows),
            vec![
                Constraint::Length(8),
                Constraint::Length(7),
                Constraint::Length(11),
                Constraint::Length(7),
                Constraint::Length(14),
                Constraint::Length(12),
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
            ('1'..='4').filter_map(shortcut_tab).collect::<Vec<_>>(),
            PrimaryTab::ALL
        );
        assert_eq!(shortcut_tab('0'), None);
        assert_eq!(shortcut_tab('5'), None);
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
        let aggregate_label = text.find("ALL:").expect("aggregate label");
        let agent_tab = text.find("AGENTS 1").expect("agent tab");
        assert!(workspace_tab < aggregate_label && aggregate_label < agent_tab);
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
    fn project_suggestion_creates_workspace_with_default_cwd() {
        let mut app = app();
        assert!(app.request_add().is_none());
        for character in "alp".chars() {
            handle_mode_key(&mut app, KeyCode::Char(character), KeyModifiers::NONE);
        }
        assert_eq!(
            handle_mode_key(&mut app, KeyCode::Enter, KeyModifiers::NONE),
            Some(DashboardEffect::CreateWorkspace {
                name: "alpha".into(),
                default_cwd: Some("/tmp/alpha".into()),
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
                default_cwd: None,
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
                default_cwd: None,
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
            agent_id: "completed-agent-id".into(),
            shell_id: "completed-shell".into(),
            agent_name: "completed-agent".into(),
            reason: AttentionReason::Completed,
            evidence: "finished".into(),
            observed_at_ms: 20,
        }];
        let mut blocked = workspace("w2", "second");
        blocked.attention = vec![WorkspaceAttentionView {
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
            launcher: false,
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
    fn add_creates_a_shell_from_terminal_focus() {
        let mut app = app();
        focus_items(&mut app);

        assert_eq!(
            app.request_add(),
            Some(DashboardEffect::CreateShell("w1".into()))
        );
        assert!(matches!(app.mode, Mode::Normal));
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
    fn agent_session_preview_labels_historical_and_missing_context() {
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
            agent.root_branch = "-".into();
            agent.root_worktree = "-".into();
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
        assert_eq!(preview.content_height, 6);
        assert!(text.contains("STATUS    idle  ·  last known"));
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
        let session_line = lines[2]
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<String>();
        assert!(session_line.contains("1 occurrence  ·  shell removed shell"));
    }

    #[test]
    fn narrow_agent_session_preview_keeps_all_labels_visible() {
        let backend = TestBackend::new(80, 34);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = app();
        app.workspaces[0].items[0] = WorkspaceItemView::AgentShell(agent_shell());
        app.workspaces[0]
            .sessions
            .push(session("active", AgentDisplayState::Working));
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
            "TASK", "STATUS", "SESSION", "ROOT", "GIT", "EVIDENCE", "SOURCE",
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
        assert!(text.contains("TASK"));
        assert!(text.contains("ROOT BRANCH"));
        assert!(text.contains("ROOT WORKTREE"));
        assert!(row.contains("Owning workspace session"));
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
        assert_eq!(
            app.update(DashboardEvent::KeyPressed {
                code: KeyCode::Char('h'),
                modifiers: KeyModifiers::NONE
            }),
            vec![DashboardEffect::LoadScheduleHistory {
                schedule_id: "schedule-1".into(),
                limit: 100
            }]
        );
        assert_eq!(
            app.update(DashboardEvent::KeyPressed {
                code: KeyCode::Char('t'),
                modifiers: KeyModifiers::NONE
            }),
            vec![DashboardEffect::ReadExecutionTranscript {
                session_id: "schedule-session".into(),
                execution_id: "execution-1".into()
            }]
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

        app.update(DashboardEvent::KeyPressed {
            code: KeyCode::Char(']'),
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
                shell_id: "schedule-shell".into(),
                run_id: "schedule-run".into(),
            }]
        );
        assert_eq!(
            app.update(DashboardEvent::KeyPressed {
                code: KeyCode::Char('t'),
                modifiers: KeyModifiers::NONE,
            }),
            vec![DashboardEffect::ReadExecutionTranscript {
                session_id: "schedule-session".into(),
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
    fn execution_palette_commands_carry_and_select_exact_execution_ids() {
        let mut app = schedule_app();
        app.schedules[0]
            .executions
            .push(execution("execution-2", ExecutionDisplayState::Active));
        let palette = CommandPalette::new_with_schedules(
            &app.workspaces,
            &app.schedules,
            &app.scheduling,
            app.exact_run_attachment,
        );
        let select = palette
            .entries
            .iter()
            .find_map(|entry| match &entry.command {
                PaletteCommand::Schedule {
                    schedule_id,
                    action: SchedulePaletteAction::SelectExecution(execution_id),
                } if execution_id == "execution-2" => {
                    Some((schedule_id.clone(), execution_id.clone()))
                }
                _ => None,
            });
        assert_eq!(select, Some(("schedule-1".into(), "execution-2".into())));

        assert!(
            execute_palette_command(
                &mut app,
                PaletteCommand::Schedule {
                    schedule_id: "schedule-1".into(),
                    action: SchedulePaletteAction::CancelExecution("execution-2".into()),
                },
            )
            .is_none()
        );
        assert_eq!(app.selected_execution_id.as_deref(), Some("execution-2"));
        assert!(matches!(
            app.pending_close,
            Some(PendingClose { target: CloseTarget::Execution(ref id), .. }) if id == "execution-2"
        ));
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
        assert!(
            execute_palette_command(
                &mut app,
                PaletteCommand::Schedule {
                    schedule_id: "schedule-1".into(),
                    action: SchedulePaletteAction::SelectExecution("selected-5-execution".into(),),
                },
            )
            .is_none()
        );

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
        assert!(
            selected_execution_window(
                &app.schedules[0].executions,
                Some("selected-5-execution"),
                3,
            )
            .iter()
            .any(|execution| execution.id == "selected-5-execution")
        );
    }

    #[test]
    fn open_execution_requires_starting_or_active_state_and_exact_run_links() {
        for state in [
            ExecutionDisplayState::Skipped,
            ExecutionDisplayState::Claimed,
            ExecutionDisplayState::DispatchFailed,
            ExecutionDisplayState::Exited,
            ExecutionDisplayState::Cancelled,
            ExecutionDisplayState::Interrupted,
        ] {
            let mut app = schedule_app();
            app.schedules[0].executions[0].state = state;
            assert!(
                app.open_selected_schedule_link().is_none(),
                "state {state:?}"
            );
            let palette = CommandPalette::new_with_schedules(
                &app.workspaces,
                &app.schedules,
                &app.scheduling,
                app.exact_run_attachment,
            );
            assert!(!palette.entries.iter().any(|entry| matches!(
                entry.command,
                PaletteCommand::Schedule {
                    action: SchedulePaletteAction::OpenExecution(_),
                    ..
                }
            )));
        }

        for state in [
            ExecutionDisplayState::Starting,
            ExecutionDisplayState::Active,
        ] {
            let mut app = schedule_app();
            app.schedules[0].executions[0].state = state;
            assert!(matches!(
                app.open_selected_schedule_link(),
                Some(DashboardEffect::OpenScheduledExecution { .. })
            ));
            app.schedules[0].executions[0].run_id = None;
            assert!(app.open_selected_schedule_link().is_none());
        }
    }

    #[test]
    fn protocol_25_keeps_schedules_usable_but_disables_exact_open() {
        let mut app = schedule_app();
        app.exact_run_attachment = false;

        assert!(app.open_selected_schedule_link().is_none());
        assert!(app.message.as_ref().is_some_and(|message| {
            message.text.contains("protocol 26") && message.text.contains("upgrade and restart")
        }));
        assert_eq!(
            app.update(DashboardEvent::KeyPressed {
                code: KeyCode::Char('h'),
                modifiers: KeyModifiers::NONE,
            }),
            vec![DashboardEffect::LoadScheduleHistory {
                schedule_id: "schedule-1".into(),
                limit: 100,
            }]
        );
        let palette = CommandPalette::new_with_schedules(
            &app.workspaces,
            &app.schedules,
            &app.scheduling,
            app.exact_run_attachment,
        );
        assert!(!palette.entries.iter().any(|entry| matches!(
            entry.command,
            PaletteCommand::Schedule {
                action: SchedulePaletteAction::OpenExecution(_),
                ..
            }
        )));
        let compact = rendered_text(&mut app, 60, 16);
        assert!(compact.contains("execution"));
        assert!(!compact.contains("Enter open"));
    }

    #[test]
    fn blocked_schedule_navigation_uses_the_exact_linked_agent_id() {
        let mut wrong = workspace("w1", "wrong");
        wrong.items[0] = WorkspaceItemView::AgentShell(agent_shell());
        let mut right = workspace("w2", "right");
        let mut exact = agent_shell();
        exact.agent.as_mut().unwrap().id = "schedule-agent".into();
        exact.agent.as_mut().unwrap().state = AgentDisplayState::Blocked;
        exact.schedule_id = Some("schedule-1".into());
        right.items[0] = WorkspaceItemView::AgentShell(exact);
        let mut app = App::new(vec![wrong, right], project_context());
        let mut schedule = schedule_view();
        schedule.executions[0].agent_state = Some(AgentDisplayState::Blocked);
        app.schedules = vec![schedule];
        app.select_tab(PrimaryTab::Schedules);

        assert!(
            app.update(DashboardEvent::KeyPressed {
                code: KeyCode::Enter,
                modifiers: KeyModifiers::NONE
            })
            .is_empty()
        );
        assert_eq!(app.primary_tab, PrimaryTab::Agents);
        assert!(
            matches!(app.selected_item(), Some(WorkspaceItemView::AgentShell(agent)) if agent.agent.as_ref().is_some_and(|agent| agent.id == "schedule-agent"))
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
    fn schedule_history_and_transcript_are_applied_only_by_explicit_completion_events() {
        let mut app = schedule_app();
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
        assert!(app.transcript.is_none());

        app.update(DashboardEvent::TranscriptCompleted(Ok(TranscriptView {
            execution_id: "history-execution".into(),
            session_id: "schedule-session".into(),
            lines: vec!["TOOL cargo [completed]".into()],
            truncated: true,
        })));
        assert!(matches!(app.mode, Mode::Transcript));
        assert!(rendered_text(&mut app, 80, 24).contains("TOOL cargo"));
    }

    #[test]
    fn transcript_overlay_starts_at_newest_and_supports_scroll_navigation() {
        let mut app = schedule_app();
        app.update(DashboardEvent::TranscriptCompleted(Ok(TranscriptView {
            execution_id: "execution-1".into(),
            session_id: "schedule-session".into(),
            lines: (0..40)
                .map(|index| format!("transcript-line-{index:02}"))
                .collect(),
            truncated: false,
        })));

        let newest = rendered_text(&mut app, 60, 16);
        assert!(newest.contains("transcript-line-39"));
        assert!(!newest.contains("transcript-line-00"));
        assert!(newest.contains("rows"));
        assert!(newest.contains("pgup/pgdn"));

        app.update(DashboardEvent::KeyPressed {
            code: KeyCode::Home,
            modifiers: KeyModifiers::NONE,
        });
        let oldest = rendered_text(&mut app, 60, 16);
        assert!(oldest.contains("transcript-line-00"));
        assert!(!oldest.contains("transcript-line-39"));

        app.update(DashboardEvent::KeyPressed {
            code: KeyCode::PageDown,
            modifiers: KeyModifiers::NONE,
        });
        let paged = rendered_text(&mut app, 60, 16);
        assert!(!paged.contains("transcript-line-00"));
        app.update(DashboardEvent::KeyPressed {
            code: KeyCode::End,
            modifiers: KeyModifiers::NONE,
        });
        assert!(rendered_text(&mut app, 60, 16).contains("transcript-line-39"));
    }

    #[test]
    fn schedule_palette_has_schedule_execution_and_notice_actions_but_no_content() {
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
            true,
        );
        for (query, group) in [
            ("schedule nightly", PaletteKindGroup::Schedules),
            ("execution failed", PaletteKindGroup::Executions),
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
        assert!(
            palette
                .entries
                .iter()
                .all(|entry| !entry.keywords.contains("transcript content"))
        );
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
            assert!(wide.contains(column));
        }
        assert!(wide.contains("PROMPT REV"));
        assert!(wide.contains("no timeout"));
        assert!(wide.contains("America/New_York"));
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

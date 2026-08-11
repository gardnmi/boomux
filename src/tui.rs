use std::io;
use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
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
const REFRESH_INTERVAL: Duration = Duration::from_millis(250);
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
    pub(crate) focused_terminal: Option<FocusedTerminalView>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct FocusedTerminalView {
    pub(crate) revision: u64,
    pub(crate) workspace_id: String,
    pub(crate) shell_id: String,
}

pub(crate) struct WorkspaceAttentionView {
    pub(crate) shell_id: String,
    pub(crate) agent_name: String,
    pub(crate) reason: String,
    pub(crate) evidence: String,
    pub(crate) observed_at_ms: u64,
}

pub(crate) struct AgentSessionView {
    pub(crate) id: String,
    pub(crate) label: String,
    pub(crate) integration: String,
    pub(crate) external_session_id: Option<String>,
    pub(crate) state: String,
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
}

impl AgentShellView {
    fn status(&self) -> &str {
        self.agent
            .as_ref()
            .map_or("untracked", |agent| agent.state.as_str())
    }
}

pub(crate) struct AgentView {
    pub(crate) id: String,
    pub(crate) state: String,
    pub(crate) integration: String,
    pub(crate) external_session_id: Option<String>,
    pub(crate) authority: String,
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
    pub(crate) branch: String,
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
    fn kind(&self) -> &'static str {
        if self.command.is_empty() {
            "shell"
        } else {
            "command"
        }
    }

    fn detail(&self) -> &str {
        if self.command.is_empty() {
            &self.branch
        } else {
            &self.command
        }
    }
}

impl WorkspaceView {
    fn shell_count(&self) -> usize {
        self.items
            .iter()
            .filter(|item| item.kind() == ItemKind::Shell)
            .count()
    }

    fn command_count(&self) -> usize {
        self.items
            .iter()
            .filter(|item| item.kind() == ItemKind::Command)
            .count()
    }

    fn launcher_count(&self) -> usize {
        self.items
            .iter()
            .filter(|item| item.kind() == ItemKind::Launcher)
            .count()
    }

    fn agent_count(&self) -> usize {
        self.items
            .iter()
            .filter(|item| item.kind() == ItemKind::Agent)
            .count()
    }

    fn process_count(&self) -> usize {
        self.items
            .iter()
            .filter(|item| !matches!(item, WorkspaceItemView::Launcher(_)))
            .count()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ItemKind {
    Agent,
    Launcher,
    Shell,
    Command,
}

impl WorkspaceItemView {
    fn kind(&self) -> ItemKind {
        match self {
            Self::AgentShell(_) => ItemKind::Agent,
            Self::Launcher(_) => ItemKind::Launcher,
            Self::Shell(shell) if shell.command.is_empty() => ItemKind::Shell,
            Self::Shell(_) => ItemKind::Command,
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
            Self::AgentShell(agent) => agent.status(),
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

pub(crate) struct Actions<R, O, C, W, N, E, F, P> {
    pub(crate) on_restore: R,
    pub(crate) on_open: O,
    pub(crate) on_close: C,
    pub(crate) on_create_workspace: W,
    pub(crate) on_create_shell: N,
    pub(crate) on_rename: E,
    pub(crate) on_refresh: F,
    pub(crate) on_terminal_preview: P,
}

struct App {
    workspaces: Vec<WorkspaceView>,
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
}

impl PrimaryTab {
    const ALL: [Self; 3] = [Self::Workspaces, Self::Agents, Self::Shells];

    fn kind(self) -> Option<ItemKind> {
        match self {
            Self::Workspaces => None,
            Self::Agents => Some(ItemKind::Agent),
            Self::Shells => Some(ItemKind::Shell),
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Workspaces => "WORKSPACES",
            Self::Agents => "AGENTS",
            Self::Shells => "SHELLS",
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
    fn new(workspaces: &[WorkspaceView]) -> Self {
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
                        workspace.items.len(),
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
                if kind == ItemKind::Agent && item.status() == "blocked" {
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
                    usize::from(attention.reason != "blocked"),
                    attention.observed_at_ms,
                    workspace.id.clone(),
                    attention.shell_id.clone(),
                    PaletteEntry {
                        action_group: PaletteActionGroup::QuickAccess,
                        kind_group: PaletteKindGroup::Attention,
                        label: format!("{} / {}", workspace.name, attention.agent_name),
                        detail: format!("{}: {}", attention.reason, attention.evidence),
                        keywords: format!(
                            "attention unseen outstanding {} {} {} {}",
                            attention.reason,
                            attention.evidence,
                            workspace.name,
                            attention.shell_id
                        ),
                        command: PaletteCommand::Attention {
                            workspace_id: workspace.id.clone(),
                            shell_id: attention.shell_id.clone(),
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
                !matches!(item, WorkspaceItemView::Launcher(_)) && item.id() == focused.shell_id
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
            self.item_state.select(Some(item_index));
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

    fn selected_item_location(&self) -> Option<(usize, usize)> {
        if self.primary_tab == PrimaryTab::Workspaces {
            return Some((
                self.workspace_state.selected()?,
                self.item_state.selected()?,
            ));
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
                let item_count = self.selected().map_or(0, |workspace| workspace.items.len());
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
                let item_count = self.selected().map_or(0, |workspace| workspace.items.len());
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
        self.mode = Mode::Palette(CommandPalette::new(&self.workspaces));
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
                .is_some_and(|workspace| !workspace.items.is_empty())
                .then_some(0),
        );
    }

    fn request_rename(&mut self) {
        let target = if self.primary_tab != PrimaryTab::Workspaces {
            self.selected_item().map(item_rename_target)
        } else {
            match self.focus {
                Focus::Workspaces => self
                    .selected()
                    .map(|workspace| RenameTarget::Workspace(workspace.id.clone())),
                Focus::Items => self.selected_item().map(item_rename_target),
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

    fn request_add<F>(&mut self, on_create_shell: &mut F) -> bool
    where
        F: FnMut(&str) -> Result<String, String>,
    {
        if self.primary_tab != PrimaryTab::Workspaces {
            return false;
        }
        match self.focus {
            Focus::Workspaces => {
                self.mode = Mode::PickProject(ProjectPicker::new(&self.project_context));
                self.message = None;
                false
            }
            Focus::Items => {
                let Some(workspace_id) = self.selected().map(|workspace| workspace.id.clone())
                else {
                    return false;
                };
                self.message = Some(Message::from_result(on_create_shell(&workspace_id)));
                true
            }
        }
    }

    fn create_workspace<F>(
        &mut self,
        name: &str,
        default_cwd: Option<&PathBuf>,
        on_create_workspace: &mut F,
    ) where
        F: FnMut(&str, Option<&PathBuf>) -> Result<String, String>,
    {
        self.mode = Mode::Normal;
        self.message = Some(Message::from_result(on_create_workspace(name, default_cwd)));
    }

    fn rename<F>(&mut self, target: &RenameTarget, name: &str, on_rename: &mut F)
    where
        F: FnMut(&RenameTarget, &str) -> Result<String, String>,
    {
        self.mode = Mode::Normal;
        self.message = Some(Message::from_result(on_rename(target, name)));
    }

    fn restore_selected<F>(&mut self, on_restore: &mut F)
    where
        F: FnMut(&str) -> Result<String, String>,
    {
        if self.primary_tab != PrimaryTab::Workspaces {
            return;
        }
        let Some(workspace_id) = self.selected().map(|workspace| workspace.id.clone()) else {
            return;
        };
        self.message = Some(Message::from_result(on_restore(&workspace_id)));
    }

    fn open_selected_item<F>(&mut self, on_open: &mut F) -> bool
    where
        F: FnMut(&OpenTarget) -> Result<String, String>,
    {
        let Some(workspace_id) = self
            .selected_item_workspace()
            .map(|workspace| workspace.id.clone())
        else {
            return false;
        };
        let Some(target) = self.selected_item().map(|item| match item {
            WorkspaceItemView::Shell(shell) => OpenTarget::Shell(shell.id.clone()),
            WorkspaceItemView::AgentShell(agent_shell) => {
                OpenTarget::Shell(agent_shell.shell.id.clone())
            }
            WorkspaceItemView::Launcher(launcher) => OpenTarget::Launcher {
                workspace_id,
                launcher_id: launcher.id.clone(),
            },
        }) else {
            return false;
        };
        self.message = Some(Message::from_result(on_open(&target)));
        true
    }

    fn request_close(&mut self) {
        self.pending_close = if self.primary_tab != PrimaryTab::Workspaces {
            self.selected_item().map(item_pending_close)
        } else {
            match self.focus {
                Focus::Workspaces => self.selected().map(|workspace| PendingClose {
                    target: CloseTarget::Workspace(workspace.id.clone()),
                    name: workspace.name.clone(),
                    shell_count: workspace.process_count(),
                    launcher_count: workspace.launcher_count(),
                }),
                Focus::Items => self.selected_item().map(item_pending_close),
            }
        };
    }

    fn cancel_close(&mut self) {
        self.pending_close = None;
    }

    fn confirm_close<F>(&mut self, on_close: &mut F)
    where
        F: FnMut(&CloseTarget) -> Result<String, String>,
    {
        let Some(pending) = self.pending_close.take() else {
            return;
        };
        self.message = Some(Message::from_result(on_close(&pending.target)));
    }

    fn refresh<F>(&mut self, on_refresh: &mut F)
    where
        F: FnMut() -> Result<DashboardState, String>,
    {
        match on_refresh() {
            Ok(state) => {
                self.replace_workspaces(state.workspaces);
                self.apply_focused_terminal(state.focused_terminal.as_ref());
            }
            Err(text) => self.message = Some(Message { text, error: true }),
        }
    }

    fn refresh_terminal_preview<P>(&mut self, on_preview: &mut P)
    where
        P: FnMut(&str) -> Result<TerminalPreview, String>,
    {
        let selected = if self.primary_tab == PrimaryTab::Workspaces && self.focus != Focus::Items {
            None
        } else {
            self.selected_item().and_then(|item| match item {
                WorkspaceItemView::Shell(shell) if shell.command.is_empty() => Some((
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
            return;
        };
        if self.terminal_preview.as_ref().is_some_and(|preview| {
            preview.shell_id == shell_id
                && preview.run_id == run_id
                && preview.output_revision == output_revision
                && preview.output.is_ok()
        }) {
            return;
        }
        let output = on_preview(&shell_id);
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
                        .position(|item| item_matches(item, &target))
                })
                .or_else(|| (!workspace.items.is_empty()).then_some(0))
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

pub(crate) fn run<R, O, C, W, N, E, F, P>(
    state: DashboardState,
    follow_focused_terminal: bool,
    project_context: ProjectContext,
    actions: Actions<R, O, C, W, N, E, F, P>,
) -> io::Result<()>
where
    R: FnMut(&str) -> Result<String, String>,
    O: FnMut(&OpenTarget) -> Result<String, String>,
    C: FnMut(&CloseTarget) -> Result<String, String>,
    W: FnMut(&str, Option<&PathBuf>) -> Result<String, String>,
    N: FnMut(&str) -> Result<String, String>,
    E: FnMut(&RenameTarget, &str) -> Result<String, String>,
    F: FnMut() -> Result<DashboardState, String>,
    P: FnMut(&str) -> Result<TerminalPreview, String>,
{
    let mut terminal = ratatui::init();
    let mut app = App::new(state.workspaces, project_context);
    if follow_focused_terminal {
        app.enable_focus_following(state.focused_terminal.as_ref());
    }
    let result = run_loop(&mut terminal, app, actions);
    ratatui::restore();
    result
}

fn run_loop<R, O, C, W, N, E, F, P>(
    terminal: &mut ratatui::DefaultTerminal,
    mut app: App,
    mut actions: Actions<R, O, C, W, N, E, F, P>,
) -> io::Result<()>
where
    R: FnMut(&str) -> Result<String, String>,
    O: FnMut(&OpenTarget) -> Result<String, String>,
    C: FnMut(&CloseTarget) -> Result<String, String>,
    W: FnMut(&str, Option<&PathBuf>) -> Result<String, String>,
    N: FnMut(&str) -> Result<String, String>,
    E: FnMut(&RenameTarget, &str) -> Result<String, String>,
    F: FnMut() -> Result<DashboardState, String>,
    P: FnMut(&str) -> Result<TerminalPreview, String>,
{
    let mut last_refresh = Instant::now();
    loop {
        if last_refresh.elapsed() >= REFRESH_INTERVAL {
            app.refresh(&mut actions.on_refresh);
            last_refresh = Instant::now();
        }
        app.refresh_terminal_preview(&mut actions.on_terminal_preview);
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
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            return Ok(());
        }

        if app.pending_close.is_some() {
            if !key.modifiers.is_empty() {
                continue;
            }
            match key.code {
                KeyCode::Char('y') => {
                    app.confirm_close(&mut actions.on_close);
                    app.refresh(&mut actions.on_refresh);
                    last_refresh = Instant::now();
                }
                KeyCode::Char('n') | KeyCode::Esc => app.cancel_close(),
                _ => {}
            }
            continue;
        }

        if matches!(app.mode, Mode::Help) {
            handle_help_key(&mut app, key.code, key.modifiers);
            continue;
        }

        if matches!(app.mode, Mode::Palette(_)) {
            if let Some(command) = handle_palette_key(&mut app, key.code, key.modifiers)
                && execute_palette_command(&mut app, command, &mut actions)
            {
                app.refresh(&mut actions.on_refresh);
                last_refresh = Instant::now();
            }
            continue;
        }

        if !matches!(app.mode, Mode::Normal) {
            if handle_mode_key(
                &mut app,
                key.code,
                key.modifiers,
                &mut actions.on_create_workspace,
                &mut actions.on_rename,
            ) {
                app.refresh(&mut actions.on_refresh);
                last_refresh = Instant::now();
            }
            continue;
        }
        if !normal_mode_modifiers_supported(key.code, key.modifiers) {
            continue;
        }

        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => return Ok(()),
            KeyCode::Down | KeyCode::Char('j') => app.next(),
            KeyCode::Up | KeyCode::Char('k') => app.previous(),
            KeyCode::PageUp => app.scroll_terminal_preview_up(),
            KeyCode::PageDown => app.scroll_terminal_preview_down(),
            KeyCode::Home => app.scroll_terminal_preview_to_start(),
            KeyCode::End => app.scroll_terminal_preview_to_end(),
            KeyCode::Enter => {
                let dispatched = if app.primary_tab != PrimaryTab::Workspaces {
                    app.open_selected_item(&mut actions.on_open)
                } else {
                    match app.focus {
                        Focus::Workspaces => {
                            app.restore_selected(&mut actions.on_restore);
                            true
                        }
                        Focus::Items => app.open_selected_item(&mut actions.on_open),
                    }
                };
                if dispatched {
                    app.refresh(&mut actions.on_refresh);
                    last_refresh = Instant::now();
                }
            }
            KeyCode::Char('r') => {
                app.refresh(&mut actions.on_refresh);
                last_refresh = Instant::now();
            }
            KeyCode::Char('x') => app.request_close(),
            KeyCode::Char('a') => {
                if app.request_add(&mut actions.on_create_shell) {
                    app.refresh(&mut actions.on_refresh);
                    last_refresh = Instant::now();
                }
            }
            KeyCode::Char('e') => app.request_rename(),
            KeyCode::Char(' ') => app.toggle_selection_pin(),
            KeyCode::Char('/' | ':') => app.open_palette(),
            KeyCode::Char('?') => app.mode = Mode::Help,
            KeyCode::Tab => app.cycle_tab(false),
            KeyCode::BackTab => app.cycle_tab(true),
            KeyCode::Char(key) if shortcut_tab(key).is_some() => {
                app.select_tab(shortcut_tab(key).expect("validated tab shortcut"));
            }
            key if app.handle_focus_key(key) => {}
            _ => {}
        }
    }
}

fn execute_palette_command<R, O, C, W, N, E, F, P>(
    app: &mut App,
    command: PaletteCommand,
    actions: &mut Actions<R, O, C, W, N, E, F, P>,
) -> bool
where
    R: FnMut(&str) -> Result<String, String>,
    O: FnMut(&OpenTarget) -> Result<String, String>,
    C: FnMut(&CloseTarget) -> Result<String, String>,
    W: FnMut(&str, Option<&PathBuf>) -> Result<String, String>,
    N: FnMut(&str) -> Result<String, String>,
    E: FnMut(&RenameTarget, &str) -> Result<String, String>,
    F: FnMut() -> Result<DashboardState, String>,
    P: FnMut(&str) -> Result<TerminalPreview, String>,
{
    match command {
        PaletteCommand::CreateWorkspace => {
            app.mode = Mode::PickProject(ProjectPicker::new(&app.project_context));
            false
        }
        PaletteCommand::ShowHelp => {
            app.mode = Mode::Help;
            false
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
                return false;
            }
            match action {
                WorkspacePaletteAction::GoTo => false,
                WorkspacePaletteAction::Restore => {
                    app.restore_selected(&mut actions.on_restore);
                    true
                }
                WorkspacePaletteAction::AddShell => app.request_add(&mut actions.on_create_shell),
                WorkspacePaletteAction::Rename => {
                    app.request_rename();
                    false
                }
                WorkspacePaletteAction::Close => {
                    app.request_close();
                    false
                }
            }
        }
        PaletteCommand::Item { identity, action } => {
            if !app.select_item_identity(&identity) {
                return false;
            }
            match action {
                ItemPaletteAction::GoTo => false,
                ItemPaletteAction::Open => app.open_selected_item(&mut actions.on_open),
                ItemPaletteAction::Rename => {
                    app.request_rename();
                    false
                }
                ItemPaletteAction::Close => {
                    app.request_close();
                    false
                }
            }
        }
        PaletteCommand::Attention {
            workspace_id,
            shell_id,
        } => {
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
            false
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

fn handle_mode_key<W, E>(
    app: &mut App,
    key: KeyCode,
    modifiers: KeyModifiers,
    on_create_workspace: &mut W,
    on_rename: &mut E,
) -> bool
where
    W: FnMut(&str, Option<&PathBuf>) -> Result<String, String>,
    E: FnMut(&RenameTarget, &str) -> Result<String, String>,
{
    let mode = std::mem::replace(&mut app.mode, Mode::Normal);
    if !modifiers.difference(KeyModifiers::SHIFT).is_empty() {
        app.mode = mode;
        return false;
    }
    match mode {
        Mode::Normal => false,
        Mode::Palette(_) | Mode::Help => false,
        Mode::PickProject(mut picker) => match key {
            KeyCode::Enter
                if picker.mode == WorkspaceCreationMode::ByName
                    && picker.custom_name().is_some() =>
            {
                let name = picker
                    .custom_name()
                    .expect("nonempty workspace name")
                    .to_owned();
                app.create_workspace(&name, None, on_create_workspace);
                true
            }
            KeyCode::Enter if picker.selected().is_some() => {
                let project = picker.selected().expect("selected project").clone();
                app.create_workspace(&project.name, Some(&project.path), on_create_workspace);
                true
            }
            KeyCode::Enter => {
                app.mode = Mode::PickProject(picker);
                false
            }
            KeyCode::Esc => false,
            KeyCode::Tab | KeyCode::BackTab => {
                picker.toggle_mode();
                app.mode = Mode::PickProject(picker);
                false
            }
            KeyCode::Down => {
                picker.next();
                app.mode = Mode::PickProject(picker);
                false
            }
            KeyCode::Up => {
                picker.previous();
                app.mode = Mode::PickProject(picker);
                false
            }
            KeyCode::Backspace => {
                picker.query.pop();
                picker.update_matches();
                app.mode = Mode::PickProject(picker);
                false
            }
            KeyCode::Char(character) => {
                picker.query.push(character);
                picker.update_matches();
                app.mode = Mode::PickProject(picker);
                false
            }
            _ => {
                app.mode = Mode::PickProject(picker);
                false
            }
        },
        Mode::Rename { target, mut input } => match key {
            KeyCode::Enter if !input.trim().is_empty() => {
                let name = input.trim().to_owned();
                app.rename(&target, &name, on_rename);
                true
            }
            KeyCode::Esc => false,
            KeyCode::Backspace => {
                input.pop();
                app.mode = Mode::Rename { target, input };
                false
            }
            KeyCode::Char(character) => {
                input.push(character);
                app.mode = Mode::Rename { target, input };
                false
            }
            _ => {
                app.mode = Mode::Rename { target, input };
                false
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
    if app.primary_tab != PrimaryTab::Workspaces {
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
        Mode::Normal | Mode::Rename { .. } => {}
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
        Line::from("  Tab/1-3 change view; h/l change pane; j/k navigate"),
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
    if app.primary_tab == PrimaryTab::Workspaces && app.focus == Focus::Workspaces {
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
        if item.status() == "untracked" {
            lines.push(Line::from(
                "  Untracked means a supported foreground host has no authoritative report.",
            ));
        } else if item.status() == "blocked" {
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
                    PrimaryTab::Agents => {
                        app.workspaces.iter().map(WorkspaceView::agent_count).sum()
                    }
                    PrimaryTab::Shells => {
                        app.workspaces.iter().map(WorkspaceView::shell_count).sum()
                    }
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
    let rows = app
        .workspaces
        .iter()
        .map(|workspace| Row::new([Cell::from(workspace.name.as_str())]));
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
                        agent.status().to_owned(),
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
                                    agent.status().to_owned(),
                                    Style::new().fg(status_color(agent.status())),
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
                                            view.authority,
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
    let header = Row::new(["KIND", "NAME", "STATUS", "DIRECTORY", "DETAIL"])
        .style(Style::new().fg(BLUE).add_modifier(Modifier::BOLD));
    let selected = app
        .workspace_state
        .selected()
        .and_then(|index| app.workspaces.get(index));
    let title = selected.map_or_else(
        || " Items ".to_owned(),
        |workspace| format!(" Items: {} ({}) ", workspace.name, workspace.items.len()),
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
    let rows: Vec<_> = selected
        .into_iter()
        .flat_map(|workspace| {
            workspace.items.iter().map(|item| match item {
                WorkspaceItemView::Shell(terminal) => Row::new(vec![
                    Cell::from(Span::styled(
                        terminal.kind(),
                        Style::new().fg(if terminal.command.is_empty() {
                            TEXT
                        } else {
                            YELLOW
                        }),
                    )),
                    Cell::from(terminal.name.as_str()),
                    Cell::from(Span::styled(
                        terminal.status.as_str(),
                        Style::new().fg(status_color(&terminal.status)),
                    )),
                    Cell::from(terminal.directory.as_str()),
                    Cell::from(terminal.detail()),
                ]),
                WorkspaceItemView::AgentShell(agent_shell) => Row::new(vec![
                    Cell::from(Span::styled("agent", Style::new().fg(TEAL))),
                    Cell::from(agent_shell.shell.name.as_str()),
                    Cell::from(Span::styled(
                        agent_shell.status(),
                        Style::new().fg(status_color(agent_shell.status())),
                    )),
                    Cell::from(agent_shell.shell.directory.as_str()),
                    Cell::from(agent_shell.agent.as_ref().map_or_else(
                        || format!("foreground process | {}", agent_shell.shell.branch),
                        |agent| {
                            format!(
                                "{} | {} | {} / {} {}%",
                                agent.evidence,
                                agent_shell.shell.branch,
                                agent.integration,
                                agent.authority,
                                agent.confidence
                            )
                        },
                    )),
                ]),
                WorkspaceItemView::Launcher(launcher) => Row::new(vec![
                    Cell::from(Span::styled("launcher", Style::new().fg(YELLOW))),
                    Cell::from(launcher.name.as_str()),
                    Cell::from("-"),
                    Cell::from(launcher.directory.as_str()),
                    Cell::from(launcher.command.as_str()),
                ]),
            })
        })
        .collect();
    let widths = shell_column_widths(items_inner.width);
    frame.render_widget(block, area);
    let table = Table::new(rows, widths)
        .header(header)
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
    AgentSession(Vec<Row<'static>>),
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
        content_height: 3,
        content: PreviewContent::Lines(vec![
            Line::from(vec![
                Span::styled("cwd  ", Style::new().fg(SUBTEXT)),
                Span::raw(launcher.directory.clone()),
            ]),
            Line::from(vec![
                Span::styled("argv ", Style::new().fg(SUBTEXT)),
                Span::raw(format_argv(&launcher.argv)),
            ]),
            Line::from(Span::styled(
                "Detached invocation; output and run history are not retained",
                Style::new().fg(SUBTEXT),
            )),
        ]),
    }
}

fn terminal_preview(app: &App, terminal: &TerminalView) -> Option<ContextualPreview> {
    let is_command = !terminal.command.is_empty();
    let mut lines = Vec::new();
    if is_command {
        lines.push(Line::from(vec![
            Span::styled("argv ", Style::new().fg(SUBTEXT)),
            Span::raw(format_argv(&terminal.argv)),
        ]));
    }
    lines.push(Line::from(vec![
        Span::styled("cwd  ", Style::new().fg(SUBTEXT)),
        Span::raw(terminal.directory.clone()),
        Span::styled("  branch ", Style::new().fg(SUBTEXT)),
        Span::raw(terminal.branch.clone()),
    ]));
    let run_detail = terminal.run.as_ref().map_or_else(
        || "no run yet".to_owned(),
        |run| {
            let outcome = run
                .exit_reason
                .as_deref()
                .map_or_else(String::new, |reason| format!("{reason}  "));
            let timing = run.ended_at_ms.map_or_else(
                || format!("started {}", compact_recency(run.started_at_ms)),
                |ended| format!("ended {}", compact_recency(ended)),
            );
            format!(
                "{outcome}run {}  generation {}  {timing}",
                short_id(&run.id),
                run.generation
            )
        },
    );
    lines.push(Line::from(vec![
        Span::styled(
            terminal.status.clone(),
            Style::new()
                .fg(status_color(&terminal.status))
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(format!("  {run_detail}"), Style::new().fg(SUBTEXT)),
    ]));

    if let Some(preview) = app
        .terminal_preview
        .as_ref()
        .filter(|preview| !is_command && preview.shell_id == terminal.id)
    {
        match &preview.output {
            Ok(output) if terminal_preview_is_empty(output) => lines.push(Line::from(vec![
                Span::styled(" Output ", Style::new().fg(BASE).bg(BLUE)),
                Span::styled(" no terminal output", Style::new().fg(SUBTEXT)),
            ])),
            Ok(output) => {
                let viewport =
                    terminal_viewport(output, TERMINAL_PREVIEW_ROWS, preview.scroll_from_bottom);
                lines.push(Line::from(vec![
                    Span::styled(" Output ", Style::new().fg(BASE).bg(BLUE)),
                    Span::styled(
                        format!(
                            " {}-{} / {}  revision {}  ",
                            viewport.start + 1,
                            viewport.end,
                            viewport.total,
                            preview.output_revision
                        ),
                        Style::new().fg(SUBTEXT),
                    ),
                    Span::styled(
                        if viewport.following {
                            "FOLLOW"
                        } else {
                            "SCROLLED"
                        },
                        Style::new()
                            .fg(if viewport.following { GREEN } else { YELLOW })
                            .add_modifier(Modifier::BOLD),
                    ),
                ]));
                lines.extend(viewport.lines.into_iter().map(terminal_preview_line));
            }
            Err(error) => lines.push(Line::from(Span::styled(
                format!("Output unavailable: {error}"),
                Style::new().fg(YELLOW),
            ))),
        }
    }
    Some(ContextualPreview {
        title: if is_command {
            format!(" Command: {} ", terminal.name)
        } else {
            format!(" Shell: {} ", terminal.name)
        },
        content_height: if is_command {
            lines.len() as u16
        } else {
            (TERMINAL_PREVIEW_ROWS + 3) as u16
        },
        content: PreviewContent::Lines(lines),
    })
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
    let rows = vec![
        Row::new([
            Cell::from(Span::styled(
                session_state_symbol(&session.state),
                Style::new().fg(session_state_color(&session.state)),
            )),
            Cell::from(vec![
                Line::from(Span::styled(
                    label,
                    Style::new().add_modifier(Modifier::BOLD),
                )),
                Line::from(Span::styled(
                    format!(
                        "{shell}  {external_identity}  {occurrences} occurrence{}  {currency}",
                        if occurrences == 1 { "" } else { "s" }
                    ),
                    Style::new().fg(SUBTEXT),
                )),
                Line::from(Span::styled(
                    format!(
                        "root {root_directory}  {}  {}",
                        agent.root_branch, agent.root_worktree
                    ),
                    Style::new().fg(SUBTEXT),
                )),
                Line::from(Span::styled(
                    format!(
                        "{}  {} {}%",
                        agent.evidence, agent.authority, agent.confidence
                    ),
                    Style::new().fg(SUBTEXT),
                )),
            ]),
            Cell::from(session.state.clone()),
            Cell::from(compact_recency(agent.updated_at_ms)),
        ])
        .height(4),
    ];
    Some(ContextualPreview {
        title: format!(" {} session ", integration_display_name(&agent.integration)),
        content: PreviewContent::AgentSession(rows),
        content_height: 4,
    })
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
    match preview.content {
        PreviewContent::AgentSession(rows) => {
            let table = Table::new(
                rows,
                [
                    Constraint::Length(2),
                    Constraint::Fill(1),
                    Constraint::Length(10),
                    Constraint::Length(9),
                ],
            )
            .column_spacing(1)
            .block(block);
            frame.render_widget(table, area);
        }
        PreviewContent::Lines(lines) => {
            frame.render_widget(Paragraph::new(lines).block(block), area);
        }
    }
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
    match integration {
        "opencode" => "OpenCode",
        "pi" => "Pi",
        other => other,
    }
}

fn shell_column_widths(width: u16) -> Vec<Constraint> {
    let (name, status, detail, directory_min, directory_max) = if width >= 120 {
        (18, 10, 30, 24, 42)
    } else {
        (16, 10, 18, 16, 42)
    };
    let kind = 8;
    // Four column gaps and the highlight marker also consume table width.
    let fixed = kind + name + status + detail + 6;
    let directory = width
        .saturating_sub(fixed)
        .clamp(directory_min, directory_max);
    vec![
        Constraint::Length(kind),
        Constraint::Length(name),
        Constraint::Length(status),
        Constraint::Length(directory),
        Constraint::Length(detail),
    ]
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

fn session_state_symbol(state: &str) -> &'static str {
    match state {
        "blocked" => "!",
        "working" => ">",
        "idle" => ".",
        "inactive" => "-",
        "done" => "x",
        _ => "?",
    }
}

fn session_state_color(state: &str) -> Color {
    match state {
        "blocked" => RED,
        "working" => TEAL,
        "idle" => GREEN,
        "inactive" => SUBTEXT,
        "done" => BLUE,
        _ => YELLOW,
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
    match status {
        "pending" | "untracked" => YELLOW,
        "exited" => SUBTEXT,
        _ => TEAL,
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use super::*;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn app() -> App {
        App::new(vec![workspace("w1", "boomux")], project_context())
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
                branch: "main".into(),
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
            state: "working".into(),
            integration: "opencode".into(),
            external_session_id: Some("external-active".into()),
            authority: "lifecycle_integration".into(),
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
                branch: "main".into(),
                command: String::new(),
                argv: Vec::new(),
                run: None,
            },
            agent: Some(agent()),
        }
    }

    fn session(id: &str, state: &str) -> AgentSessionView {
        AgentSessionView {
            id: id.into(),
            label: "OpenCode review".into(),
            integration: "opencode".into(),
            external_session_id: Some(format!("external-{id}")),
            state: state.into(),
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

    fn focus_items(app: &mut App) {
        app.set_focus(Focus::Items);
    }

    fn successful_text(_: &str) -> Result<String, String> {
        Ok(String::new())
    }

    fn successful_preview(_: &str) -> Result<TerminalPreview, String> {
        Ok(TerminalPreview::default())
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

    fn successful_workspace(_: &str, _: Option<&PathBuf>) -> Result<String, String> {
        Ok(String::new())
    }

    fn successful_open(_: &OpenTarget) -> Result<String, String> {
        Ok(String::new())
    }

    fn successful_close(_: &CloseTarget) -> Result<String, String> {
        Ok(String::new())
    }

    fn successful_rename(_: &RenameTarget, _: &str) -> Result<String, String> {
        Ok(String::new())
    }

    fn empty_refresh() -> Result<DashboardState, String> {
        Ok(DashboardState {
            workspaces: Vec::new(),
            focused_terminal: None,
        })
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
                branch: "main".into(),
                command: String::new(),
                argv: Vec::new(),
                run: None,
            },
            agent: None,
        }
    }

    fn terminal(id: &str, name: &str, command: &str) -> WorkspaceItemView {
        WorkspaceItemView::Shell(TerminalView {
            id: id.into(),
            name: name.into(),
            status: "running".into(),
            directory: format!("/tmp/{name}"),
            branch: "main".into(),
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
    fn wide_shell_columns_are_bounded_instead_of_absorbing_extra_space() {
        assert_eq!(
            shell_column_widths(180),
            vec![
                Constraint::Length(8),
                Constraint::Length(18),
                Constraint::Length(10),
                Constraint::Length(42),
                Constraint::Length(30),
            ]
        );
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
                Constraint::Length(6),
                Constraint::Length(7),
                Constraint::Length(21),
                Constraint::Length(5),
                Constraint::Length(43),
                Constraint::Length(29),
                Constraint::Length(13),
            ]
        );
        assert_eq!(
            agent_column_widths(80, &rows),
            vec![
                Constraint::Length(6),
                Constraint::Length(7),
                Constraint::Length(11),
                Constraint::Length(5),
                Constraint::Length(18),
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
            ('1'..='3').filter_map(shortcut_tab).collect::<Vec<_>>(),
            PrimaryTab::ALL
        );
        assert_eq!(shortcut_tab('0'), None);
        assert_eq!(shortcut_tab('4'), None);
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
        mixed.sessions.push(session("durable-1", "working"));
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
            branch: "main".into(),
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
        let mut selected_workspace = None;

        let changed = app.request_add(&mut |workspace_id| {
            selected_workspace = Some(workspace_id.to_owned());
            Ok("Created shell".into())
        });

        assert!(changed);
        assert_eq!(selected_workspace.as_deref(), Some("w1"));
    }

    #[test]
    fn project_suggestion_creates_workspace_with_default_cwd() {
        let mut app = app();
        let mut created = None;

        assert!(!app.request_add(&mut |_| Ok(String::new())));
        for character in "alp".chars() {
            handle_mode_key(
                &mut app,
                KeyCode::Char(character),
                KeyModifiers::NONE,
                &mut |_, _| Ok(String::new()),
                &mut |_, _| Ok(String::new()),
            );
        }
        let changed = handle_mode_key(
            &mut app,
            KeyCode::Enter,
            KeyModifiers::NONE,
            &mut |name, default_cwd| {
                created = Some((name.to_owned(), default_cwd.cloned()));
                Ok("Created workspace".into())
            },
            &mut |_, _| Ok(String::new()),
        );

        assert!(changed);
        assert_eq!(created, Some(("alpha".into(), Some("/tmp/alpha".into()))));
        assert!(matches!(app.mode, Mode::Normal));
    }

    #[test]
    fn arbitrary_text_creates_trimmed_workspace_name() {
        let mut app = app();
        let mut created = None;
        app.request_add(&mut |_| Ok(String::new()));
        handle_mode_key(
            &mut app,
            KeyCode::Tab,
            KeyModifiers::NONE,
            &mut |_, _| Ok(String::new()),
            &mut |_, _| Ok(String::new()),
        );
        if let Mode::PickProject(picker) = &mut app.mode {
            picker.query = "  custom workspace  ".into();
            picker.update_matches();
            assert!(picker.selected().is_none());
        }
        let changed = handle_mode_key(
            &mut app,
            KeyCode::Enter,
            KeyModifiers::NONE,
            &mut |name, default_cwd| {
                created = Some((name.to_owned(), default_cwd.cloned()));
                Ok("Created workspace".into())
            },
            &mut |_, _| Ok(String::new()),
        );

        assert!(changed);
        assert_eq!(created, Some(("custom workspace".into(), None)));
    }

    #[test]
    fn by_name_creation_wins_even_when_its_name_matches_a_project() {
        let mut app = app();
        let mut created = None;
        app.request_add(&mut |_| Ok(String::new()));
        handle_mode_key(
            &mut app,
            KeyCode::Tab,
            KeyModifiers::NONE,
            &mut |_, _| Ok(String::new()),
            &mut |_, _| Ok(String::new()),
        );
        if let Mode::PickProject(picker) = &mut app.mode {
            picker.query = "alpha".into();
            picker.update_matches();
            assert_eq!(picker.mode, WorkspaceCreationMode::ByName);
            assert!(picker.selected().is_none());
        }

        let changed = handle_mode_key(
            &mut app,
            KeyCode::Enter,
            KeyModifiers::NONE,
            &mut |name, default_cwd| {
                created = Some((name.to_owned(), default_cwd.cloned()));
                Ok("Created workspace".into())
            },
            &mut |_, _| Ok(String::new()),
        );

        assert!(changed);
        assert_eq!(created, Some(("alpha".into(), None)));
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
        app.request_add(&mut |_| Ok(String::new()));

        handle_mode_key(
            &mut app,
            KeyCode::Char('c'),
            KeyModifiers::CONTROL,
            &mut |_, _| Ok(String::new()),
            &mut |_, _| Ok(String::new()),
        );

        let Mode::PickProject(picker) = &app.mode else {
            panic!("expected project picker");
        };
        assert!(picker.query.is_empty());
    }

    #[test]
    fn project_search_accepts_shifted_characters() {
        let mut app = app();
        app.request_add(&mut |_| Ok(String::new()));

        handle_mode_key(
            &mut app,
            KeyCode::Char('_'),
            KeyModifiers::SHIFT,
            &mut |_, _| Ok(String::new()),
            &mut |_, _| Ok(String::new()),
        );

        let Mode::PickProject(picker) = &app.mode else {
            panic!("expected project picker");
        };
        assert_eq!(picker.query, "_");
    }

    #[test]
    fn command_palette_finds_blocked_agents_and_attention() {
        let mut workspace = workspace("w1", "review");
        let mut agent = agent_shell();
        agent.agent.as_mut().unwrap().state = "blocked".into();
        workspace.items[0] = WorkspaceItemView::AgentShell(agent);
        workspace.attention = vec![WorkspaceAttentionView {
            shell_id: "term_1".into(),
            agent_name: "review-agent".into(),
            reason: "blocked".into(),
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
            shell_id: "completed-shell".into(),
            agent_name: "completed-agent".into(),
            reason: "completed".into(),
            evidence: "finished".into(),
            observed_at_ms: 20,
        }];
        let mut blocked = workspace("w2", "second");
        blocked.attention = vec![WorkspaceAttentionView {
            shell_id: "blocked-shell".into(),
            agent_name: "blocked-agent".into(),
            reason: "blocked".into(),
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
        let opened = RefCell::new(Vec::new());
        let mut app = app();
        let identity = ItemIdentity {
            workspace_id: "w1".into(),
            item_id: "term_1".into(),
            launcher: false,
        };
        let mut actions = Actions {
            on_restore: successful_text,
            on_open: |target: &OpenTarget| {
                opened.borrow_mut().push(target.clone());
                Ok("opened".into())
            },
            on_close: successful_close,
            on_create_workspace: successful_workspace,
            on_create_shell: successful_text,
            on_rename: successful_rename,
            on_refresh: empty_refresh,
            on_terminal_preview: successful_preview,
        };

        assert!(execute_palette_command(
            &mut app,
            PaletteCommand::Item {
                identity: identity.clone(),
                action: ItemPaletteAction::Open,
            },
            &mut actions,
        ));
        assert_eq!(
            opened.borrow().as_slice(),
            &[OpenTarget::Shell("term_1".into())]
        );
        assert_eq!(app.focus, Focus::Items);

        assert!(!execute_palette_command(
            &mut app,
            PaletteCommand::Item {
                identity: identity.clone(),
                action: ItemPaletteAction::Rename,
            },
            &mut actions,
        ));
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
            &mut actions,
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
        let mut actions = Actions {
            on_restore: successful_text,
            on_open: successful_open,
            on_close: successful_close,
            on_create_workspace: successful_workspace,
            on_create_shell: successful_text,
            on_rename: successful_rename,
            on_refresh: empty_refresh,
            on_terminal_preview: successful_preview,
        };

        execute_palette_command(
            &mut app,
            PaletteCommand::Attention {
                workspace_id: "w1".into(),
                shell_id: "removed".into(),
            },
            &mut actions,
        );
        assert_eq!(app.focus, Focus::Workspaces);
        assert!(app.message.as_ref().is_some_and(|message| {
            !message.error && message.text.contains("no longer retained")
        }));
    }

    #[test]
    fn attention_jump_reports_when_workspace_is_not_retained() {
        let mut app = app();
        let mut actions = Actions {
            on_restore: successful_text,
            on_open: successful_open,
            on_close: successful_close,
            on_create_workspace: successful_workspace,
            on_create_shell: successful_text,
            on_rename: successful_rename,
            on_refresh: empty_refresh,
            on_terminal_preview: successful_preview,
        };

        execute_palette_command(
            &mut app,
            PaletteCommand::Attention {
                workspace_id: "removed".into(),
                shell_id: "removed".into(),
            },
            &mut actions,
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
        let mut workspace_id = None;
        focus_items(&mut app);

        let changed = app.request_add(&mut |selected_workspace_id| {
            workspace_id = Some(selected_workspace_id.to_owned());
            Ok("Created shell".into())
        });

        assert!(changed);
        assert_eq!(workspace_id.as_deref(), Some("w1"));
        assert!(matches!(app.mode, Mode::Normal));
    }

    #[test]
    fn enter_on_terminal_opens_only_the_selected_shell() {
        let mut app = app();
        let mut opened = None;
        focus_items(&mut app);

        app.open_selected_item(&mut |target| {
            opened = Some(target.clone());
            Ok("Opened shell".into())
        });

        assert_eq!(opened, Some(OpenTarget::Shell("term_1".into())));
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
                command: "zeditor .".into(),
                argv: vec!["zeditor".into(), ".".into()],
            }));
        focus_items(&mut app);
        app.next();
        let mut opened = None;

        app.open_selected_item(&mut |target| {
            opened = Some(target.clone());
            Ok("Launched editor".into())
        });

        assert_eq!(
            opened,
            Some(OpenTarget::Launcher {
                workspace_id: "w1".into(),
                launcher_id: "launcher-1".into(),
            })
        );
    }

    #[test]
    fn launcher_row_dispatches_rename_and_remove_targets() {
        let launcher = LauncherView {
            id: "launcher-1".into(),
            name: "editor".into(),
            directory: "/tmp/boomux".into(),
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
        let mut opened = None;

        let dispatched = app.open_selected_item(&mut |target| {
            opened = Some(target.clone());
            Ok(String::new())
        });
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

        assert!(dispatched);
        assert_eq!(opened, Some(OpenTarget::Shell("term_1".into())));
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
        let mut renamed = None;
        focus_items(&mut app);
        app.request_rename();

        for character in ['a', 'p', 'i'] {
            handle_mode_key(
                &mut app,
                KeyCode::Char(character),
                KeyModifiers::NONE,
                &mut |_, _| Ok(String::new()),
                &mut |_, _| Ok(String::new()),
            );
        }
        let changed = handle_mode_key(
            &mut app,
            KeyCode::Enter,
            KeyModifiers::NONE,
            &mut |_, _| Ok(String::new()),
            &mut |target, name| {
                renamed = Some((target.clone(), name.to_owned()));
                Ok("Renamed shell".into())
            },
        );

        assert!(changed);
        assert_eq!(
            renamed,
            Some((RenameTarget::Shell("term_1".into()), "api".into()))
        );
        assert!(matches!(app.mode, Mode::Normal));
    }

    #[test]
    fn rename_mode_dispatches_the_selected_workspace_and_name() {
        let mut app = app();
        let mut renamed = None;

        app.request_rename();
        assert!(matches!(
            app.mode,
            Mode::Rename {
                target: RenameTarget::Workspace(ref id),
                ..
            } if id == "w1"
        ));
        for character in "renamed".chars() {
            handle_mode_key(
                &mut app,
                KeyCode::Char(character),
                KeyModifiers::NONE,
                &mut |_, _| Ok(String::new()),
                &mut |_, _| Ok(String::new()),
            );
        }
        let changed = handle_mode_key(
            &mut app,
            KeyCode::Enter,
            KeyModifiers::NONE,
            &mut |_, _| Ok(String::new()),
            &mut |target, name| {
                renamed = Some((target.clone(), name.to_owned()));
                Ok("Renamed workspace".into())
            },
        );

        assert!(changed);
        assert_eq!(
            renamed,
            Some((RenameTarget::Workspace("w1".into()), "renamed".into()))
        );
    }

    #[test]
    fn restore_keeps_app_active_and_reports_success() {
        let mut app = app();
        let mut restored = None;

        app.restore_selected(&mut |workspace_id| {
            restored = Some(workspace_id.to_owned());
            Ok("Restored workspace".into())
        });

        assert_eq!(restored.as_deref(), Some("w1"));
        let message = app.message.expect("restore message");
        assert_eq!(message.text, "Restored workspace");
        assert!(!message.error);
    }

    #[test]
    fn closing_a_workspace_requires_confirmation() {
        let mut app = app();
        let mut closed = None;

        app.request_close();
        let pending = app.pending_close.as_ref().expect("pending close");
        assert_eq!(pending.target, CloseTarget::Workspace("w1".into()));
        assert_eq!(pending.shell_count, 1);

        app.cancel_close();
        assert!(app.pending_close.is_none());
        app.request_close();
        app.confirm_close(&mut |target| {
            closed = Some(target.clone());
            Ok("Closed workspace".into())
        });

        assert_eq!(closed, Some(CloseTarget::Workspace("w1".into())));
        assert!(app.pending_close.is_none());
        let message = app.message.expect("close message");
        assert_eq!(message.text, "Closed workspace");
        assert!(!message.error);
    }

    #[test]
    fn closing_a_shell_uses_terminal_focus() {
        let mut app = app();
        let mut closed = None;
        focus_items(&mut app);

        app.request_close();
        let pending = app.pending_close.as_ref().expect("pending close");
        assert_eq!(pending.target, CloseTarget::Shell("term_1".into()));
        assert_eq!(pending.name, "agent");

        app.confirm_close(&mut |target| {
            closed = Some(target.clone());
            Ok("Closed shell".into())
        });

        assert_eq!(closed, Some(CloseTarget::Shell("term_1".into())));
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
        assert!(text.contains("DETAIL"));
        assert!(text.contains("DIRECTORY"));
        assert!(!text.contains("term_1"));
        assert!(text.contains("SHELLS"));
        assert!(text.contains("Items: boomux (1)"));
        assert!(!text.contains("DIRTY"));
        assert!(!text.contains("WORKTREE"));
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
        assert!(text.contains("DETAIL"));
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
        assert!(text.contains("tool call"));
        assert!(text.contains("main"));
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
        app.workspaces[0].sessions = vec![session("active", "working")];
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
        assert!(lines.iter().any(|line| line.contains("foreground process")));
        assert!(lines.iter().any(|line| line.contains("keepname")));
        assert!(!lines.iter().any(|line| line.contains("opencode")));
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
        assert!(text.contains("DIRECTORY"));
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
        let mut active = session("active", "working");
        active.label = "Current work".into();
        active.last_at_ms = now;
        let mut recent = session("recent", "inactive");
        recent.label = "Recent review".into();
        recent.external_session_id = Some("external-active".into());
        recent.state_is_current = false;
        recent.last_at_ms = now + 1;
        let mut week = session("week", "done");
        week.label = "Finished build".into();
        week.state_is_current = false;
        week.last_at_ms = now - 2 * 24 * 60 * 60 * 1_000;
        let mut older = session("older", "inactive");
        older.label = "Dormant review".into();
        older.state_is_current = false;
        older.last_at_ms = now - 8 * 24 * 60 * 60 * 1_000;
        let mut pi = session("pi", "done");
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
        assert!(lines.iter().any(|line| {
            line.contains("Current work") && line.contains("working") && line.contains("now")
        }));
        assert!(lines.iter().any(|line| {
            line.contains("agent")
                && line.contains("external")
                && line.contains("1 occurrence")
                && line.contains("current")
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
        assert!(text.contains("alpha-shell"));
        assert!(text.contains("beta-shell"));
        assert!(text.contains("one"));
        assert!(text.contains("two"));
    }

    #[test]
    fn narrow_global_view_keeps_all_aggregate_columns_visible() {
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
        assert!(text.contains("DIRECTORY"));
        assert!(text.contains("DETAIL"));
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
        let mut wrong = session("wrong", "working");
        wrong.label = "Wrong workspace session".into();
        one.sessions.push(wrong);
        let mut two = workspace("w2", "two");
        two.items = vec![WorkspaceItemView::AgentShell(agent_shell())];
        let mut right = session("right", "working");
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
        app.workspaces[0].sessions.push(session("hidden", "done"));
        focus_items(&mut app);

        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        let shell_text: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        assert!(shell_text.contains("Shell: agent"));
        assert!(shell_text.contains("/tmp/boomux"));
        assert!(!shell_text.contains("OpenCode session"));

        app.workspaces[0]
            .items
            .push(WorkspaceItemView::Launcher(LauncherView {
                id: "launcher".into(),
                name: "editor".into(),
                directory: "/tmp/boomux".into(),
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
        app.refresh_terminal_preview(&mut |_| {
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

        assert!(text.contains("Command: format"));
        assert!(text.contains("[\"printf\", \"a b\", \"\"]"));
        assert!(!text.contains(" Output "));
        assert!(!text.contains("pgup/dn"));
        assert_eq!(reads.get(), 0);
        assert!(app.terminal_preview.is_none());
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

        app.refresh_terminal_preview(&mut read);
        app.refresh_terminal_preview(&mut read);
        assert_eq!(calls.get(), 1);

        let WorkspaceItemView::Shell(shell) = &mut app.workspaces[0].items[0] else {
            unreachable!();
        };
        shell.run.as_mut().unwrap().output_revision = 5;
        app.refresh_terminal_preview(&mut read);
        assert_eq!(calls.get(), 2);
        let WorkspaceItemView::Shell(shell) = &mut app.workspaces[0].items[0] else {
            unreachable!();
        };
        shell.run.as_mut().unwrap().id = "run-2".into();
        app.refresh_terminal_preview(&mut read);
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
        assert!(text.contains("Output"));
        assert!(text.contains("revision 5"));
        assert!(text.contains("FOLLOW"));
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

        app.refresh_terminal_preview(&mut |_| Ok(output(40)));
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
        app.refresh_terminal_preview(&mut |_| Ok(output(42)));
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
        app.refresh_terminal_preview(&mut |_| Ok(output.clone()));

        let mut wide = Terminal::new(TestBackend::new(180, 40)).unwrap();
        wide.draw(|frame| render(frame, &mut app)).unwrap();
        let wide_text: String = wide
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        assert!(wide_text.contains("Shell: agent"));
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
        assert!(!short_text.contains("Shell: agent"));
        assert!(short_text.contains("Items: boomux"));
    }

    #[test]
    fn workspace_preview_omits_attention_details() {
        let backend = TestBackend::new(140, 34);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut workspace = workspace("w1", "review");
        workspace.attention = vec![WorkspaceAttentionView {
            shell_id: "term_1".into(),
            agent_name: "review-agent".into(),
            reason: "blocked".into(),
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
        let mut view = session("generic", "idle");
        view.label = "opencode".into();
        view.external_session_id = Some("ses_123456789".into());

        assert_eq!(session_task_label(&view), None);
        assert_eq!(best_session_label(&view), "agent (ses_1234)");
    }

    #[test]
    fn pi_sessions_keep_the_shell_and_identity_fallback() {
        let mut view = session("pi-generic", "idle");
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
        assert!(text.contains("DIRECTORY"));
        assert!(text.contains("DETAIL"));
        assert!(text.contains("main"));
        assert!(!text.contains("term_1"));
        assert!(!text.contains("DIRTY"));
        assert!(!text.contains("WORKTREE"));
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
        assert!(text.contains("DIRECTORY"));
        assert!(text.contains("DETAIL"));
        assert!(!text.contains("term_1"));
    }

    #[test]
    fn project_launcher_renders_to_test_backend() {
        let backend = TestBackend::new(120, 34);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = app();
        app.request_add(&mut |_| Ok(String::new()));

        terminal.draw(|frame| render(frame, &mut app)).unwrap();
    }

    #[test]
    fn project_launcher_shows_explicit_by_name_and_project_modes() {
        let backend = TestBackend::new(120, 34);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = app();
        app.request_add(&mut |_| Ok(String::new()));
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
}

use std::io;
use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, Cell, Clear, List, ListItem, ListState, Paragraph, Row, Table, TableState,
};

use crate::agent_attention_projection::AgentStateCounts;

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

pub(crate) struct WorkspaceView {
    pub(crate) id: String,
    pub(crate) name: String,
    pub(crate) items: Vec<WorkspaceItemView>,
    pub(crate) sessions: Vec<AgentSessionView>,
    pub(crate) agent_state_counts: AgentStateCounts,
    pub(crate) attention_count: usize,
}

pub(crate) struct AgentSessionView {
    pub(crate) id: String,
    pub(crate) label: String,
    pub(crate) integration: String,
    pub(crate) external_session_id: Option<String>,
    pub(crate) state: String,
    pub(crate) state_is_current: bool,
    pub(crate) last_at_ms: u64,
    pub(crate) runs: Vec<AgentSessionRunView>,
}

pub(crate) struct AgentSessionRunView {
    pub(crate) shell_id: Option<String>,
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
            .map_or("idle", |agent| agent.state.as_str())
    }
}

pub(crate) struct AgentView {
    pub(crate) id: String,
    pub(crate) state: String,
    pub(crate) integration: String,
    pub(crate) authority: String,
    pub(crate) confidence: u8,
    pub(crate) evidence: String,
}

pub(crate) struct LauncherView {
    pub(crate) id: String,
    pub(crate) name: String,
    pub(crate) directory: String,
    pub(crate) command: String,
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

    fn session_count(&self) -> usize {
        self.sessions.len()
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
}

pub(crate) struct Actions<R, O, C, W, N, E, F> {
    pub(crate) on_restore: R,
    pub(crate) on_open: O,
    pub(crate) on_close: C,
    pub(crate) on_create_workspace: W,
    pub(crate) on_create_shell: N,
    pub(crate) on_rename: E,
    pub(crate) on_refresh: F,
}

struct App {
    workspaces: Vec<WorkspaceView>,
    workspace_state: TableState,
    item_state: TableState,
    global_state: TableState,
    session_state: TableState,
    primary_tab: PrimaryTab,
    focus: Focus,
    mode: Mode,
    message: Option<Message>,
    pending_close: Option<PendingClose>,
    project_context: ProjectContext,
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
    Sessions,
    Launchers,
    Shells,
    Commands,
}

impl PrimaryTab {
    const ALL: [Self; 6] = [
        Self::Workspaces,
        Self::Agents,
        Self::Sessions,
        Self::Launchers,
        Self::Shells,
        Self::Commands,
    ];

    fn kind(self) -> Option<ItemKind> {
        match self {
            Self::Workspaces => None,
            Self::Agents => Some(ItemKind::Agent),
            Self::Sessions => None,
            Self::Launchers => Some(ItemKind::Launcher),
            Self::Shells => Some(ItemKind::Shell),
            Self::Commands => Some(ItemKind::Command),
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Workspaces => "WORKSPACES",
            Self::Agents => "AGENTS",
            Self::Sessions => "SESSIONS",
            Self::Launchers => "LAUNCHERS",
            Self::Shells => "SHELLS",
            Self::Commands => "COMMANDS",
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

#[derive(Clone)]
struct SessionIdentity {
    workspace_id: String,
    session_id: String,
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
    Rename { target: RenameTarget, input: String },
}

struct ProjectPicker {
    projects: Vec<ProjectView>,
    matches: Vec<usize>,
    state: ListState,
    query: String,
    config_path: Option<PathBuf>,
    warning: Option<String>,
    roots_configured: bool,
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
        self.state.select((!self.matches.is_empty()).then_some(0));
    }

    fn selected(&self) -> Option<&ProjectView> {
        self.state
            .selected()
            .and_then(|index| self.matches.get(index))
            .and_then(|index| self.projects.get(*index))
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
            session_state: TableState::default(),
            primary_tab: PrimaryTab::Workspaces,
            focus: Focus::Workspaces,
            mode: Mode::Normal,
            message: None,
            pending_close: None,
            project_context,
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

    fn global_session_locations(&self) -> Vec<(usize, usize)> {
        let now_ms = current_time_ms();
        let mut locations: Vec<_> = self
            .workspaces
            .iter()
            .enumerate()
            .flat_map(|(workspace_index, workspace)| {
                workspace
                    .sessions
                    .iter()
                    .enumerate()
                    .map(move |(session_index, session)| {
                        (
                            workspace_index,
                            session_index,
                            session_category(session, now_ms),
                        )
                    })
            })
            .collect();
        locations.sort_by(
            |(left_workspace, left_session, left_category),
             (right_workspace, right_session, right_category)| {
                let left = &self.workspaces[*left_workspace].sessions[*left_session];
                let right = &self.workspaces[*right_workspace].sessions[*right_session];
                session_category_order(*left_category)
                    .cmp(&session_category_order(*right_category))
                    .then_with(|| right.last_at_ms.cmp(&left.last_at_ms))
                    .then_with(|| {
                        self.workspaces[*left_workspace]
                            .id
                            .cmp(&self.workspaces[*right_workspace].id)
                    })
                    .then_with(|| left.id.cmp(&right.id))
            },
        );
        locations
            .into_iter()
            .map(|(workspace, session, _)| (workspace, session))
            .collect()
    }

    fn global_session_location(&self, ordinal: usize) -> Option<(usize, usize)> {
        self.global_session_locations().get(ordinal).copied()
    }

    fn selected_session(&self) -> Option<(&WorkspaceView, &AgentSessionView)> {
        let (workspace, session) = self.global_session_location(self.session_state.selected()?)?;
        Some((
            self.workspaces.get(workspace)?,
            self.workspaces.get(workspace)?.sessions.get(session)?,
        ))
    }

    fn global_session_count(&self) -> usize {
        self.workspaces
            .iter()
            .map(WorkspaceView::session_count)
            .sum()
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
            return;
        }
        self.focus = Focus::Items;
        if tab == PrimaryTab::Sessions {
            self.session_state
                .select((self.global_session_count() > 0).then_some(0));
        } else {
            self.global_state
                .select((self.global_item_count() > 0).then_some(0));
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
            let sessions = self.primary_tab == PrimaryTab::Sessions;
            let item_count = if sessions {
                self.global_session_count()
            } else {
                self.global_item_count()
            };
            if item_count > 0 {
                let state = if sessions {
                    &mut self.session_state
                } else {
                    &mut self.global_state
                };
                let next = state.selected().map_or(0, |index| (index + 1) % item_count);
                state.select(Some(next));
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
            let sessions = self.primary_tab == PrimaryTab::Sessions;
            let item_count = if sessions {
                self.global_session_count()
            } else {
                self.global_item_count()
            };
            if item_count > 0 {
                let state = if sessions {
                    &mut self.session_state
                } else {
                    &mut self.global_state
                };
                let previous = state.selected().map_or(0, |index| {
                    if index == 0 {
                        item_count - 1
                    } else {
                        index - 1
                    }
                });
                state.select(Some(previous));
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
        if self.primary_tab == PrimaryTab::Sessions {
            return;
        }
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

    fn create_workspace<F>(&mut self, name: &str, on_create_workspace: &mut F)
    where
        F: FnMut(&str) -> Result<String, String>,
    {
        self.mode = Mode::Normal;
        self.message = Some(Message::from_result(on_create_workspace(name)));
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

    fn open_selected_session<F>(&mut self, on_open: &mut F) -> bool
    where
        F: FnMut(&OpenTarget) -> Result<String, String>,
    {
        let Some(shell_id) = self.selected_session().and_then(|(_, session)| {
            latest_existing_session_run(session)
                .and_then(|run| run.shell_id.as_deref())
                .map(str::to_owned)
        }) else {
            return false;
        };
        self.message = Some(Message::from_result(on_open(&OpenTarget::Shell(shell_id))));
        true
    }

    fn request_close(&mut self) {
        if self.primary_tab == PrimaryTab::Sessions {
            return;
        }
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
        F: FnMut() -> Result<Vec<WorkspaceView>, String>,
    {
        match on_refresh() {
            Ok(workspaces) => self.replace_workspaces(workspaces),
            Err(text) => self.message = Some(Message { text, error: true }),
        }
    }

    fn replace_workspaces(&mut self, workspaces: Vec<WorkspaceView>) {
        let selected_id = self.selected().map(|workspace| workspace.id.clone());
        let selected_item = self.workspace_item_identity();
        let selected_global_item = self.global_item_identity();
        let selected_global_session = self.global_session_identity();
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
        if self.primary_tab == PrimaryTab::Sessions {
            let session_index = selected_global_session
                .and_then(|target| self.global_session_position(&target))
                .or_else(|| (self.global_session_count() > 0).then_some(0));
            self.session_state.select(session_index);
        } else if self.primary_tab != PrimaryTab::Workspaces {
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
        if matches!(
            self.primary_tab,
            PrimaryTab::Workspaces | PrimaryTab::Sessions
        ) {
            return None;
        }
        let (workspace, item) = self.selected_item_location()?;
        Some(item_identity(
            &self.workspaces[workspace],
            &self.workspaces[workspace].items[item],
        ))
    }

    fn global_session_identity(&self) -> Option<SessionIdentity> {
        if self.primary_tab != PrimaryTab::Sessions {
            return None;
        }
        let (workspace, session) = self.global_session_location(self.session_state.selected()?)?;
        Some(SessionIdentity {
            workspace_id: self.workspaces[workspace].id.clone(),
            session_id: self.workspaces[workspace].sessions[session].id.clone(),
        })
    }

    fn global_session_position(&self, identity: &SessionIdentity) -> Option<usize> {
        self.global_session_locations()
            .iter()
            .position(|(workspace, session)| {
                self.workspaces[*workspace].id == identity.workspace_id
                    && self.workspaces[*workspace].sessions[*session].id == identity.session_id
            })
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

pub(crate) fn run<R, O, C, W, N, E, F>(
    workspaces: Vec<WorkspaceView>,
    project_context: ProjectContext,
    actions: Actions<R, O, C, W, N, E, F>,
) -> io::Result<()>
where
    R: FnMut(&str) -> Result<String, String>,
    O: FnMut(&OpenTarget) -> Result<String, String>,
    C: FnMut(&CloseTarget) -> Result<String, String>,
    W: FnMut(&str) -> Result<String, String>,
    N: FnMut(&str) -> Result<String, String>,
    E: FnMut(&RenameTarget, &str) -> Result<String, String>,
    F: FnMut() -> Result<Vec<WorkspaceView>, String>,
{
    let mut terminal = ratatui::init();
    let result = run_loop(
        &mut terminal,
        App::new(workspaces, project_context),
        actions,
    );
    ratatui::restore();
    result
}

fn run_loop<R, O, C, W, N, E, F>(
    terminal: &mut ratatui::DefaultTerminal,
    mut app: App,
    mut actions: Actions<R, O, C, W, N, E, F>,
) -> io::Result<()>
where
    R: FnMut(&str) -> Result<String, String>,
    O: FnMut(&OpenTarget) -> Result<String, String>,
    C: FnMut(&CloseTarget) -> Result<String, String>,
    W: FnMut(&str) -> Result<String, String>,
    N: FnMut(&str) -> Result<String, String>,
    E: FnMut(&RenameTarget, &str) -> Result<String, String>,
    F: FnMut() -> Result<Vec<WorkspaceView>, String>,
{
    let mut last_refresh = Instant::now();
    loop {
        if last_refresh.elapsed() >= REFRESH_INTERVAL {
            app.refresh(&mut actions.on_refresh);
            last_refresh = Instant::now();
        }
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
            KeyCode::Enter => {
                let dispatched = if app.primary_tab == PrimaryTab::Sessions {
                    app.open_selected_session(&mut actions.on_open)
                } else if app.primary_tab != PrimaryTab::Workspaces {
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

fn normal_mode_modifiers_supported(code: KeyCode, modifiers: KeyModifiers) -> bool {
    modifiers.is_empty() || (code == KeyCode::BackTab && modifiers == KeyModifiers::SHIFT)
}

fn handle_mode_key<W, E>(
    app: &mut App,
    key: KeyCode,
    modifiers: KeyModifiers,
    on_create_workspace: &mut W,
    on_rename: &mut E,
) -> bool
where
    W: FnMut(&str) -> Result<String, String>,
    E: FnMut(&RenameTarget, &str) -> Result<String, String>,
{
    let mode = std::mem::replace(&mut app.mode, Mode::Normal);
    if !modifiers.difference(KeyModifiers::SHIFT).is_empty() {
        app.mode = mode;
        return false;
    }
    match mode {
        Mode::Normal => false,
        Mode::PickProject(mut picker) => match key {
            KeyCode::Enter if picker.selected().is_some() => {
                let name = picker.selected().expect("selected project").name.clone();
                app.create_workspace(&name, on_create_workspace);
                true
            }
            KeyCode::Enter if !picker.query.trim().is_empty() => {
                let name = picker.query.trim().to_owned();
                app.create_workspace(&name, on_create_workspace);
                true
            }
            KeyCode::Enter => {
                app.mode = Mode::PickProject(picker);
                false
            }
            KeyCode::Esc => false,
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
    if app.primary_tab == PrimaryTab::Sessions {
        render_global_sessions(frame, dashboard_area, app);
    } else if app.primary_tab != PrimaryTab::Workspaces {
        render_global_items(frame, dashboard_area, app);
    } else if dashboard_area.width >= 114 {
        let [workspace_area, terminal_area] =
            Layout::horizontal([Constraint::Length(42), Constraint::Fill(1)]).areas(dashboard_area);
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
    if let Mode::PickProject(picker) = &mut app.mode {
        render_project_picker(frame, area, picker);
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

    let search = Paragraph::new(format!("> {}_", picker.query)).block(
        Block::bordered()
            .title(" Create workspace ")
            .border_style(Style::new().fg(TEAL)),
    );
    frame.render_widget(search, search_area);

    let items: Vec<_> = if picker.matches.is_empty() {
        let message = if !picker.roots_configured {
            let path = picker.config_path.as_deref().map_or_else(
                || "config.toml".to_owned(),
                |path| path.display().to_string(),
            );
            format!("No project suggestions. Add [projects] roots to {path}")
        } else if picker.query.is_empty() {
            "No project suggestions discovered".to_owned()
        } else {
            "No matching suggestion; Enter creates this workspace name".to_owned()
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

    let help = picker.warning.as_ref().map_or_else(
        || {
            Line::from(vec![
                Span::styled(" type", Style::new().fg(TEAL)),
                Span::raw(" name or filter  "),
                Span::styled("up/down", Style::new().fg(BLUE)),
                Span::raw(" select  "),
                Span::styled("enter", Style::new().fg(GREEN)),
                Span::raw(" create  "),
                Span::styled("esc", Style::new().fg(RED)),
                Span::raw(" cancel"),
            ])
        },
        |warning| Line::from(Span::styled(format!(" {warning}"), Style::new().fg(YELLOW))),
    );
    frame.render_widget(Paragraph::new(help).style(Style::new().bg(BASE)), help_area);
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
                    PrimaryTab::Sessions => app
                        .workspaces
                        .iter()
                        .map(WorkspaceView::session_count)
                        .sum(),
                    PrimaryTab::Launchers => app
                        .workspaces
                        .iter()
                        .map(WorkspaceView::launcher_count)
                        .sum(),
                    PrimaryTab::Shells => {
                        app.workspaces.iter().map(WorkspaceView::shell_count).sum()
                    }
                    PrimaryTab::Commands => app
                        .workspaces
                        .iter()
                        .map(WorkspaceView::command_count)
                        .sum(),
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
    let rows = app.workspaces.iter().map(|workspace| {
        Row::new([
            Cell::from(workspace.name.as_str()),
            Cell::from(workspace.shell_count().to_string()),
            Cell::from(workspace.command_count().to_string()),
            Cell::from(workspace.launcher_count().to_string()),
            Cell::from(workspace.agent_count().to_string()),
            Cell::from(workspace.agent_state_counts.blocked.to_string()),
            Cell::from(workspace.agent_state_counts.done.to_string()),
            Cell::from(workspace.attention_count.to_string()),
        ])
    });
    let table = Table::new(
        rows,
        [
            Constraint::Min(8),
            Constraint::Length(2),
            Constraint::Length(3),
            Constraint::Length(3),
            Constraint::Length(2),
            Constraint::Length(3),
            Constraint::Length(2),
            Constraint::Length(1),
        ],
    )
    .header(
        Row::new(["NAME", "SH", "CMD", "LCH", "AG", "BLK", "DN", "!"])
            .style(Style::new().fg(BLUE).add_modifier(Modifier::BOLD)),
    )
    .column_spacing(1)
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

    frame.render_stateful_widget(table, area, &mut app.workspace_state);
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
    let contextual_panel = (app.primary_tab == PrimaryTab::Agents && inner.height >= 9)
        .then(|| contextual_session_panel(app))
        .flatten();
    let (items_inner, sessions_area) = contextual_panel.as_ref().map_or((inner, None), |panel| {
        let panel_height = (panel.content_height + 2)
            .min(inner.height.saturating_sub(6))
            .max(3);
        let [items_area, sessions_area] =
            Layout::vertical([Constraint::Fill(1), Constraint::Length(panel_height)]).areas(inner);
        (items_area, Some(sessions_area))
    });
    let show_full_ids = items_inner.width >= 150;
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
                            Cell::from(if show_full_ids {
                                shell.id.clone()
                            } else {
                                short_id(&shell.id)
                            }),
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
                            Cell::from(if show_full_ids {
                                agent.shell.id.clone()
                            } else {
                                short_id(&agent.shell.id)
                            }),
                        ]),
                        WorkspaceItemView::Launcher(launcher) => cells.extend([
                            Cell::from(launcher.name.clone()),
                            Cell::from("-"),
                            Cell::from(launcher.directory.clone()),
                            Cell::from(launcher.command.clone()),
                            Cell::from(if show_full_ids {
                                launcher.id.clone()
                            } else {
                                short_id(&launcher.id)
                            }),
                        ]),
                    }
                    Row::new(cells)
                })
        })
        .collect();
    let table_area = if show_full_ids {
        items_inner
    } else {
        let [detail_area, table_area] =
            Layout::vertical([Constraint::Length(1), Constraint::Fill(1)]).areas(items_inner);
        let detail = app.selected_item().map_or_else(
            || {
                vec![Line::from(Span::styled(
                    " No item selected",
                    Style::new().fg(SUBTEXT),
                ))]
            },
            item_detail_lines,
        );
        frame.render_widget(Paragraph::new(detail), detail_area);
        table_area
    };
    let widths = global_column_widths(items_inner.width, show_full_ids);
    let table = Table::new(rows, widths)
        .header(
            Row::new(["WORKSPACE", "NAME", "STATUS", "DIRECTORY", "DETAIL", "ID"])
                .style(Style::new().fg(BLUE).add_modifier(Modifier::BOLD)),
        )
        .column_spacing(1)
        .row_highlight_style(
            Style::new()
                .fg(TEXT)
                .add_modifier(Modifier::BOLD | Modifier::REVERSED),
        )
        .highlight_symbol("> ");
    frame.render_widget(block, area);
    frame.render_stateful_widget(table, table_area, &mut app.global_state);
    if let (Some(panel), Some(panel_area)) = (contextual_panel, sessions_area) {
        render_contextual_sessions(frame, panel_area, panel);
    }
}

fn render_global_sessions(frame: &mut Frame, area: Rect, app: &mut App) {
    let now_ms = current_time_ms();
    let locations = app.global_session_locations();
    let compact = area.width < 120;
    let mut previous_category = None;
    let rows = locations.iter().map(|(workspace_index, session_index)| {
        let workspace = &app.workspaces[*workspace_index];
        let session = &workspace.sessions[*session_index];
        let category = session_category(session, now_ms);
        let category_label = if previous_category == Some(category) {
            ""
        } else {
            previous_category = Some(category);
            category.label()
        };
        let latest_shell = latest_existing_session_run(session)
            .and_then(|run| run.shell_name.as_deref())
            .unwrap_or("removed shell");
        let identity = session
            .external_session_id
            .as_deref()
            .map(short_id)
            .unwrap_or_else(|| short_id(&session.id));
        let state = Cell::from(Line::from(vec![
            Span::styled(
                session_state_symbol(&session.state),
                Style::new().fg(session_state_color(&session.state)),
            ),
            Span::raw(format!(" {}", session.state)),
        ]));
        if compact {
            Row::new(vec![
                Cell::from(category_label),
                Cell::from(workspace.name.clone()),
                Cell::from(best_session_label(session)),
                state,
                Cell::from(latest_shell.to_owned()),
                Cell::from(compact_recency(session.last_at_ms)),
            ])
        } else {
            Row::new(vec![
                Cell::from(category_label),
                Cell::from(workspace.name.clone()),
                Cell::from(best_session_label(session)),
                Cell::from(integration_display_name(&session.integration).to_owned()),
                state,
                Cell::from(latest_shell.to_owned()),
                Cell::from(compact_recency(session.last_at_ms)),
                Cell::from(identity),
            ])
        }
    });
    let (header, widths) = if compact {
        (
            Row::new([
                "ACTIVITY",
                "WORKSPACE",
                "DESCRIPTION",
                "STATE",
                "SHELL",
                "LAST",
            ]),
            vec![
                Constraint::Length(8),
                Constraint::Length(11),
                Constraint::Fill(1),
                Constraint::Length(10),
                Constraint::Length(13),
                Constraint::Length(7),
            ],
        )
    } else {
        (
            Row::new([
                "ACTIVITY",
                "WORKSPACE",
                "DESCRIPTION",
                "INTEGRATION",
                "STATE",
                "LATEST SHELL",
                "RECENCY",
                "ID",
            ]),
            vec![
                Constraint::Length(13),
                Constraint::Length(16),
                Constraint::Fill(1),
                Constraint::Length(10),
                Constraint::Length(11),
                Constraint::Length(18),
                Constraint::Length(9),
                Constraint::Length(8),
            ],
        )
    };
    let table = Table::new(rows, widths)
        .header(header.style(Style::new().fg(BLUE).add_modifier(Modifier::BOLD)))
        .column_spacing(1)
        .block(
            Block::bordered()
                .title(format!(" SESSIONS ({}) ", locations.len()))
                .border_style(Style::new().fg(TEAL)),
        )
        .row_highlight_style(
            Style::new()
                .fg(TEXT)
                .add_modifier(Modifier::BOLD | Modifier::REVERSED),
        )
        .highlight_symbol("> ");
    frame.render_stateful_widget(table, area, &mut app.session_state);
}

fn latest_existing_session_run(session: &AgentSessionView) -> Option<&AgentSessionRunView> {
    session.runs.iter().rev().find(|run| run.shell_id.is_some())
}

fn render_items(frame: &mut Frame, area: ratatui::layout::Rect, app: &mut App) {
    let header = Row::new(["KIND", "NAME", "STATUS", "DIRECTORY", "DETAIL", "ID"])
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
    let contextual_panel = (inner.height >= 9)
        .then(|| contextual_session_panel(app))
        .flatten();
    let (items_inner, sessions_area) = contextual_panel.as_ref().map_or((inner, None), |panel| {
        let panel_height = (panel.content_height + 2)
            .min(inner.height.saturating_sub(6))
            .max(3);
        let [items_area, sessions_area] =
            Layout::vertical([Constraint::Fill(1), Constraint::Length(panel_height)]).areas(inner);
        (items_area, Some(sessions_area))
    });
    let show_full_ids = items_inner.width >= 150;
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
                    Cell::from(if show_full_ids {
                        terminal.id.clone()
                    } else {
                        short_id(&terminal.id)
                    }),
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
                    Cell::from(if show_full_ids {
                        agent_shell.shell.id.clone()
                    } else {
                        short_id(&agent_shell.shell.id)
                    }),
                ]),
                WorkspaceItemView::Launcher(launcher) => Row::new(vec![
                    Cell::from(Span::styled("launcher", Style::new().fg(YELLOW))),
                    Cell::from(launcher.name.as_str()),
                    Cell::from("-"),
                    Cell::from(launcher.directory.as_str()),
                    Cell::from(launcher.command.as_str()),
                    Cell::from(if show_full_ids {
                        launcher.id.clone()
                    } else {
                        short_id(&launcher.id)
                    }),
                ]),
            })
        })
        .collect();
    let widths = shell_column_widths(items_inner.width, show_full_ids);
    frame.render_widget(block, area);
    let table_area = if show_full_ids {
        items_inner
    } else {
        let [detail_area, table_area] =
            Layout::vertical([Constraint::Length(1), Constraint::Fill(1)]).areas(items_inner);
        let detail = app.selected_item().map_or_else(
            || {
                vec![Line::from(Span::styled(
                    " No item selected",
                    Style::new().fg(SUBTEXT),
                ))]
            },
            item_detail_lines,
        );
        frame.render_widget(Paragraph::new(detail), detail_area);
        table_area
    };
    let table = Table::new(rows, widths)
        .header(header)
        .column_spacing(1)
        .row_highlight_style(
            Style::new()
                .fg(TEXT)
                .add_modifier(Modifier::BOLD | Modifier::REVERSED),
        )
        .highlight_symbol("> ");
    frame.render_stateful_widget(table, table_area, &mut app.item_state);
    if let (Some(panel), Some(panel_area)) = (contextual_panel, sessions_area) {
        render_contextual_sessions(frame, panel_area, panel);
    }
}

struct ContextualSessionPanel {
    title: String,
    rows: Vec<Row<'static>>,
    content_height: u16,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SessionCategory {
    Active,
    Last24Hours,
    Last7Days,
    Older,
}

impl SessionCategory {
    const ALL: [Self; 4] = [
        Self::Active,
        Self::Last24Hours,
        Self::Last7Days,
        Self::Older,
    ];

    fn label(self) -> &'static str {
        match self {
            Self::Active => "ACTIVE",
            Self::Last24Hours => "LAST 24 HOURS",
            Self::Last7Days => "LAST 7 DAYS",
            Self::Older => "OLDER",
        }
    }
}

fn session_category_order(category: SessionCategory) -> u8 {
    match category {
        SessionCategory::Active => 0,
        SessionCategory::Last24Hours => 1,
        SessionCategory::Last7Days => 2,
        SessionCategory::Older => 3,
    }
}

fn session_category(session: &AgentSessionView, now_ms: u64) -> SessionCategory {
    if session.state_is_current {
        return SessionCategory::Active;
    }
    match now_ms.saturating_sub(session.last_at_ms) {
        0..=86_400_000 => SessionCategory::Last24Hours,
        86_400_001..=604_800_000 => SessionCategory::Last7Days,
        _ => SessionCategory::Older,
    }
}

fn contextual_session_panel(app: &App) -> Option<ContextualSessionPanel> {
    let WorkspaceItemView::AgentShell(agent_shell) = app.selected_item()? else {
        return None;
    };
    let agent = agent_shell.agent.as_ref()?;
    let workspace = app.selected_item_workspace()?;
    let sessions: Vec<_> = workspace
        .sessions
        .iter()
        .filter(|session| session.integration == agent.integration)
        .collect();
    if sessions.is_empty() {
        return None;
    }
    let now_ms = current_time_ms();
    let mut rows = Vec::new();
    let mut content_height = 0;
    for category in SessionCategory::ALL {
        let categorized: Vec<_> = sessions
            .iter()
            .copied()
            .filter(|session| session_category(session, now_ms) == category)
            .collect();
        if categorized.is_empty() {
            continue;
        }
        let categorized_count = categorized.len() as u16;
        rows.push(
            Row::new([
                Cell::from(""),
                Cell::from(category.label()),
                Cell::from(""),
                Cell::from(""),
            ])
            .style(Style::new().fg(BLUE).add_modifier(Modifier::BOLD)),
        );
        content_height += 1;
        rows.extend(categorized.into_iter().map(|session| {
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
                .unwrap_or("removed shell");
            let occurrences = session.runs.len();
            let currency = if session.state_is_current {
                "current"
            } else {
                "last known"
            };
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
                ]),
                Cell::from(session.state.clone()),
                Cell::from(compact_recency(session.last_at_ms)),
            ])
            .height(2)
        }));
        content_height += categorized_count * 2;
    }
    Some(ContextualSessionPanel {
        title: format!(
            " {} sessions ",
            integration_display_name(&agent.integration)
        ),
        rows,
        content_height,
    })
}

fn best_session_label(session: &AgentSessionView) -> String {
    let label = session.label.trim();
    if !label.is_empty()
        && !label.eq_ignore_ascii_case(&session.integration)
        && !label.eq_ignore_ascii_case(integration_display_name(&session.integration))
    {
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

fn integration_display_name(integration: &str) -> &str {
    match integration {
        "opencode" => "OpenCode",
        "pi" => "Pi",
        other => other,
    }
}

fn render_contextual_sessions(frame: &mut Frame, area: Rect, panel: ContextualSessionPanel) {
    let table = Table::new(
        panel.rows,
        [
            Constraint::Length(2),
            Constraint::Fill(1),
            Constraint::Length(10),
            Constraint::Length(9),
        ],
    )
    .column_spacing(1)
    .block(
        Block::bordered()
            .title(panel.title)
            .border_style(Style::new().fg(OVERLAY)),
    );
    frame.render_widget(table, area);
}

fn item_detail_lines(item: &WorkspaceItemView) -> Vec<Line<'_>> {
    match item {
        WorkspaceItemView::Shell(shell) => {
            let mut spans = vec![
                Span::styled(" ID ", Style::new().fg(SUBTEXT)),
                Span::styled(shell.id.as_str(), Style::new().fg(TEXT)),
            ];
            if !shell.command.is_empty() {
                spans.extend([
                    Span::styled("  Command ", Style::new().fg(SUBTEXT)),
                    Span::styled(shell.command.as_str(), Style::new().fg(TEXT)),
                ]);
            }
            vec![Line::from(spans)]
        }
        WorkspaceItemView::Launcher(launcher) => vec![Line::from(vec![
            Span::styled(" ID ", Style::new().fg(SUBTEXT)),
            Span::styled(launcher.id.as_str(), Style::new().fg(TEXT)),
        ])],
        WorkspaceItemView::AgentShell(agent_shell) => vec![Line::from(match &agent_shell.agent {
            Some(agent) => format!(
                " Shell {}  Agent {}  Branch {}",
                agent_shell.shell.id, agent.id, agent_shell.shell.branch
            ),
            None => format!(
                " Shell {}  Branch {}",
                agent_shell.shell.id, agent_shell.shell.branch
            ),
        })],
    }
}

fn shell_column_widths(width: u16, show_full_ids: bool) -> Vec<Constraint> {
    let (name, status, detail, id, directory_min, directory_max) = if show_full_ids {
        (18, 10, 30, 36, 24, 42)
    } else {
        (16, 10, 18, 8, 16, 36)
    };
    let kind = 8;
    // Five column gaps and the highlight marker also consume table width.
    let fixed = kind + name + status + detail + id + 7;
    let directory = width
        .saturating_sub(fixed)
        .clamp(directory_min, directory_max);
    vec![
        Constraint::Length(kind),
        Constraint::Length(name),
        Constraint::Length(status),
        Constraint::Length(directory),
        Constraint::Length(detail),
        Constraint::Length(id),
    ]
}

fn global_column_widths(width: u16, show_full_ids: bool) -> Vec<Constraint> {
    let (workspace, name, status, detail, id, directory_min, directory_max) = if show_full_ids {
        (20, 18, 10, 30, 36, 16, 42)
    } else {
        (12, 12, 8, 12, 8, 8, 28)
    };
    // Five column gaps and the highlight marker also consume table width.
    let fixed = workspace + name + status + detail + id + 7;
    let directory = width
        .saturating_sub(fixed)
        .clamp(directory_min, directory_max);
    vec![
        Constraint::Length(workspace),
        Constraint::Length(name),
        Constraint::Length(status),
        Constraint::Length(directory),
        Constraint::Length(detail),
        Constraint::Length(id),
    ]
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
        if app.primary_tab == PrimaryTab::Sessions {
            let shell_available = app.selected_session().is_some_and(|(_, session)| {
                session.runs.iter().rev().any(|run| run.shell_id.is_some())
            });
            let open_help = if app.session_state.selected().is_none() {
                " no session selected  "
            } else if shell_available {
                " open latest shell  "
            } else {
                " unavailable (no shell)  "
            };
            let line = Line::from(vec![
                Span::styled(" j/k", Style::new().fg(TEAL)),
                Span::styled(
                    " navigate  tab/shift-tab views  1-6 select view  ",
                    Style::new().fg(SUBTEXT),
                ),
                Span::styled("enter", Style::new().fg(GREEN)),
                Span::styled(open_help, Style::new().fg(SUBTEXT)),
                Span::styled("r", Style::new().fg(BLUE)),
                Span::styled(" refresh  ", Style::new().fg(SUBTEXT)),
                Span::styled("q", Style::new().fg(RED)),
                Span::styled(" quit", Style::new().fg(SUBTEXT)),
            ]);
            frame.render_widget(Paragraph::new(line), area);
            return;
        }
        let launcher_selected = matches!(app.selected_item(), Some(WorkspaceItemView::Launcher(_)));
        let mut spans = vec![
            Span::styled(" j/k", Style::new().fg(TEAL)),
            Span::styled(
                if app.primary_tab == PrimaryTab::Workspaces {
                    " navigate  tab/shift-tab views  h/l panes  "
                } else {
                    " navigate  tab/shift-tab views  1-6 select view  "
                },
                Style::new().fg(SUBTEXT),
            ),
        ];
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
        "pending" => YELLOW,
        "exited" => SUBTEXT,
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
            items: vec![WorkspaceItemView::Shell(TerminalView {
                id: "term_1".into(),
                name: "agent".into(),
                status: "running".into(),
                directory: "/tmp/boomux".into(),
                branch: "main".into(),
                command: String::new(),
            })],
            sessions: Vec::new(),
            agent_state_counts: AgentStateCounts::default(),
            attention_count: 0,
        }
    }

    fn agent() -> AgentView {
        AgentView {
            id: "agent-1".into(),
            state: "working".into(),
            integration: "opencode".into(),
            authority: "lifecycle_integration".into(),
            confidence: 95,
            evidence: "tool call in progress".into(),
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
            runs: vec![AgentSessionRunView {
                shell_id: Some("term_1".into()),
                shell_name: Some("agent".into()),
                directory: Some("/tmp/boomux".into()),
            }],
        }
    }

    fn focus_items(app: &mut App) {
        app.set_focus(Focus::Items);
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
        })
    }

    fn launcher_view(id: &str, name: &str) -> WorkspaceItemView {
        WorkspaceItemView::Launcher(LauncherView {
            id: id.into(),
            name: name.into(),
            directory: format!("/tmp/{name}"),
            command: format!("run-{name}"),
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
            shell_column_widths(180, true),
            vec![
                Constraint::Length(8),
                Constraint::Length(18),
                Constraint::Length(10),
                Constraint::Length(42),
                Constraint::Length(30),
                Constraint::Length(36),
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
        assert_eq!(app.primary_tab, PrimaryTab::Sessions);
        app.cycle_tab(false);
        assert_eq!(app.primary_tab, PrimaryTab::Launchers);
        app.cycle_tab(true);
        assert_eq!(app.primary_tab, PrimaryTab::Sessions);
        app.cycle_tab(true);
        assert_eq!(app.primary_tab, PrimaryTab::Agents);
        app.cycle_tab(true);
        assert_eq!(app.primary_tab, PrimaryTab::Workspaces);
    }

    #[test]
    fn numeric_shortcuts_match_primary_tab_order() {
        assert_eq!(
            ('1'..='6').filter_map(shortcut_tab).collect::<Vec<_>>(),
            PrimaryTab::ALL
        );
        assert_eq!(shortcut_tab('0'), None);
        assert_eq!(shortcut_tab('7'), None);
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
        app.workspaces[0]
            .items
            .push(launcher_view("launch-1", "editor"));

        app.select_tab(PrimaryTab::Launchers);
        assert_eq!(app.primary_tab, PrimaryTab::Launchers);
        assert_eq!(app.global_state.selected(), Some(0));
        assert!(matches!(
            app.selected_item(),
            Some(WorkspaceItemView::Launcher(launcher)) if launcher.id == "launch-1"
        ));

        app.select_tab(PrimaryTab::Commands);
        assert!(app.global_state.selected().is_none());
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
    fn tabs_and_workspace_table_render_exclusive_counts() {
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
        assert!(text.contains("SESSIONS 1"));
        assert!(text.contains("LAUNCHERS 1"));
        assert!(text.contains("SHELLS 1"));
        assert!(text.contains("COMMANDS 1"));
        assert!(!text.contains("active agents"));
        for header in ["SH", "CMD", "LCH", "AG", "BLK", "DN", "!"] {
            assert!(text.contains(header), "missing {header}");
        }
        let workspace_tab = text.find("WORKSPACES 1").expect("workspace tab");
        let aggregate_label = text.find("ALL:").expect("aggregate label");
        let agent_tab = text.find("AGENTS 1").expect("agent tab");
        assert!(workspace_tab < aggregate_label && aggregate_label < agent_tab);
        assert!(
            lines
                .iter()
                .any(|line| line.contains("mixed") && line.matches('1').count() >= 4)
        );

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
        assert!(!agent_text.contains("| 1 SESSIONS"));
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
    fn project_suggestion_creates_workspace_by_name_only() {
        let mut app = app();
        let mut created = None;

        assert!(!app.request_add(&mut |_| Ok(String::new())));
        for character in "alp".chars() {
            handle_mode_key(
                &mut app,
                KeyCode::Char(character),
                KeyModifiers::NONE,
                &mut |_| Ok(String::new()),
                &mut |_, _| Ok(String::new()),
            );
        }
        let changed = handle_mode_key(
            &mut app,
            KeyCode::Enter,
            KeyModifiers::NONE,
            &mut |name| {
                created = Some(name.to_owned());
                Ok("Created workspace".into())
            },
            &mut |_, _| Ok(String::new()),
        );

        assert!(changed);
        assert_eq!(created.as_deref(), Some("alpha"));
        assert!(matches!(app.mode, Mode::Normal));
    }

    #[test]
    fn arbitrary_text_creates_trimmed_workspace_name() {
        let mut app = app();
        let mut created = None;
        app.request_add(&mut |_| Ok(String::new()));
        if let Mode::PickProject(picker) = &mut app.mode {
            picker.query = "  custom workspace  ".into();
            picker.update_matches();
            assert!(picker.selected().is_none());
        }
        let changed = handle_mode_key(
            &mut app,
            KeyCode::Enter,
            KeyModifiers::NONE,
            &mut |name| {
                created = Some(name.to_owned());
                Ok("Created workspace".into())
            },
            &mut |_, _| Ok(String::new()),
        );

        assert!(changed);
        assert_eq!(created.as_deref(), Some("custom workspace"));
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
    fn project_search_ignores_modified_characters() {
        let mut app = app();
        app.request_add(&mut |_| Ok(String::new()));

        handle_mode_key(
            &mut app,
            KeyCode::Char('c'),
            KeyModifiers::CONTROL,
            &mut |_| Ok(String::new()),
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
            &mut |_| Ok(String::new()),
            &mut |_, _| Ok(String::new()),
        );

        let Mode::PickProject(picker) = &app.mode else {
            panic!("expected project picker");
        };
        assert_eq!(picker.query, "_");
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
                &mut |_| Ok(String::new()),
                &mut |_, _| Ok(String::new()),
            );
        }
        let changed = handle_mode_key(
            &mut app,
            KeyCode::Enter,
            KeyModifiers::NONE,
            &mut |_| Ok(String::new()),
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
                &mut |_| Ok(String::new()),
                &mut |_, _| Ok(String::new()),
            );
        }
        let changed = handle_mode_key(
            &mut app,
            KeyCode::Enter,
            KeyModifiers::NONE,
            &mut |_| Ok(String::new()),
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
        assert!(text.contains("ID"));
        assert!(text.contains("ID term_1"));
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
        assert!(text.contains("Shell term_1  Agent agent-1"));
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
    fn compact_dashboard_renders_hinted_agent_in_one_detail_line() {
        let backend = TestBackend::new(80, 20);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = app();
        let mut agent_shell = hinted_agent_shell();
        agent_shell.shell.name = "keepname".into();
        app.workspaces[0].items[0] = WorkspaceItemView::AgentShell(agent_shell);
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
        assert!(lines.iter().any(|line| line.contains("Shell term_1")));
        assert!(!lines.iter().any(|line| line.contains("Agent agent-1")));
        assert!(lines.iter().any(|line| line.contains("foreground process")));
        assert!(lines.iter().any(|line| line.contains("keepname")));
        assert!(!lines.iter().any(|line| line.contains("opencode")));
        assert!(lines.iter().any(|line| line.contains("idle")));
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
        assert!(text.contains("agent"));
        assert!(text.contains("DIRECTORY"));
        assert!(text.contains("main"));
        assert!(!text.contains("REPOSITORY"));
    }

    #[test]
    fn selected_durable_agent_renders_filtered_categorized_sessions() {
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
        recent.state_is_current = false;
        recent.last_at_ms = now - 2 * 60 * 60 * 1_000;
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

        assert!(text.contains("OpenCode sessions"));
        assert!(text.contains("ACTIVE"));
        assert!(text.contains("LAST 24 HOURS"));
        assert!(text.contains("LAST 7 DAYS"));
        assert!(text.contains("OLDER"));
        assert!(text.contains("Current work"));
        assert!(text.contains("Recent review"));
        assert!(text.contains("Dormant review"));
        assert!(text.contains("Finished build"));
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
        assert!(!text.contains("tool call in progress"));
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
    fn global_sessions_render_grouped_and_sorted_across_workspaces() {
        let backend = TestBackend::new(180, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let now = current_time_ms();
        let mut one = workspace("w1", "one");
        let mut older_active = session("active-old", "idle");
        older_active.label = "Active old".into();
        older_active.last_at_ms = now - 2_000;
        let mut recent = session("recent", "inactive");
        recent.label = "Recent session".into();
        recent.state_is_current = false;
        recent.last_at_ms = now - 60_000;
        one.sessions = vec![recent, older_active];
        let mut two = workspace("w2", "two");
        let mut newer_active = session("active-new", "working");
        newer_active.label = "Active new".into();
        newer_active.last_at_ms = now - 1_000;
        let mut weekly = session("weekly", "done");
        weekly.label = "Weekly session".into();
        weekly.state_is_current = false;
        weekly.last_at_ms = now - 2 * 86_400_000;
        two.sessions = vec![weekly, newer_active];
        let mut app = App::new(vec![one, two], project_context());
        app.select_tab(PrimaryTab::Sessions);

        let labels: Vec<_> = app
            .global_session_locations()
            .into_iter()
            .map(|(workspace, session)| app.workspaces[workspace].sessions[session].label.as_str())
            .collect();
        assert_eq!(
            labels,
            [
                "Active new",
                "Active old",
                "Recent session",
                "Weekly session"
            ]
        );
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        let text: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect();

        assert!(text.contains("SESSIONS (4)"));
        assert!(text.contains("ACTIVITY"));
        assert!(text.contains("WORKSPACE"));
        assert!(text.contains("DESCRIPTION"));
        assert!(text.contains("LATEST SHELL"));
        assert_eq!(text.matches("ACTIVE").count(), 1);
        assert_eq!(text.matches("LAST 24 HOURS").count(), 1);
        assert_eq!(text.matches("LAST 7 DAYS").count(), 1);
        assert!(text.find("Active new").unwrap() < text.find("Active old").unwrap());
    }

    #[test]
    fn narrow_global_sessions_keep_core_context_visible() {
        let backend = TestBackend::new(100, 20);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut workspace = workspace("w1", "project");
        let mut view = session("session", "idle");
        view.label = "Review narrow layout".into();
        workspace.sessions.push(view);
        let mut app = App::new(vec![workspace], project_context());
        app.select_tab(PrimaryTab::Sessions);

        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        let text: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect();

        assert!(text.contains("ACTIVITY"));
        assert!(text.contains("WORKSPACE"));
        assert!(text.contains("DESCRIPTION"));
        assert!(text.contains("STATE"));
        assert!(text.contains("SHELL"));
        assert!(text.contains("LAST"));
        assert!(text.contains("Review narrow layout"));
        assert!(text.contains("project"));
        assert!(text.contains("agent"));
    }

    #[test]
    fn global_session_navigation_and_refresh_preserve_session_identity() {
        let mut one = workspace("w1", "one");
        one.sessions = vec![session("one", "working")];
        let mut two = workspace("w2", "two");
        two.sessions = vec![session("two", "working")];
        let mut app = App::new(vec![one, two], project_context());
        app.select_tab(PrimaryTab::Sessions);
        assert_eq!(app.session_state.selected(), Some(0));
        app.next();
        assert_eq!(
            app.selected_session()
                .map(|(_, session)| session.id.as_str()),
            Some("two")
        );
        app.previous();
        assert_eq!(
            app.selected_session()
                .map(|(_, session)| session.id.as_str()),
            Some("one")
        );
        app.next();

        let mut refreshed_two = workspace("w2", "two");
        refreshed_two.sessions = vec![session("new", "working"), session("two", "working")];
        let mut refreshed_one = workspace("w1", "one");
        refreshed_one.sessions = vec![session("one", "working")];
        app.replace_workspaces(vec![refreshed_two, refreshed_one]);

        assert_eq!(
            app.selected_session()
                .map(|(workspace, _)| workspace.id.as_str()),
            Some("w2")
        );
        assert_eq!(
            app.selected_session()
                .map(|(_, session)| session.id.as_str()),
            Some("two")
        );
    }

    #[test]
    fn session_open_uses_newest_still_existing_shell() {
        let mut workspace = workspace("w1", "one");
        let mut view = session("session", "inactive");
        view.runs = vec![
            AgentSessionRunView {
                shell_id: Some("old-shell".into()),
                shell_name: Some("old".into()),
                directory: None,
            },
            AgentSessionRunView {
                shell_id: Some("new-shell".into()),
                shell_name: Some("new".into()),
                directory: None,
            },
            AgentSessionRunView {
                shell_id: None,
                shell_name: None,
                directory: None,
            },
        ];
        workspace.sessions.push(view);
        let mut app = App::new(vec![workspace], project_context());
        app.select_tab(PrimaryTab::Sessions);
        let mut opened = None;

        assert!(app.open_selected_session(&mut |target| {
            opened = Some(target.clone());
            Ok(String::new())
        }));
        assert_eq!(opened, Some(OpenTarget::Shell("new-shell".into())));
    }

    #[test]
    fn unavailable_session_has_no_open_or_mutation_actions() {
        let backend = TestBackend::new(140, 20);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut workspace = workspace("w1", "one");
        let mut view = session("removed", "inactive");
        view.runs[0].shell_id = None;
        view.runs[0].shell_name = None;
        workspace.sessions.push(view);
        let mut app = App::new(vec![workspace], project_context());
        app.select_tab(PrimaryTab::Sessions);
        let mut opened = false;

        assert!(!app.open_selected_session(&mut |_| {
            opened = true;
            Ok(String::new())
        }));
        app.request_rename();
        app.request_close();
        assert!(!app.request_add(&mut |_| Ok(String::new())));
        assert!(!opened);
        assert!(matches!(app.mode, Mode::Normal));
        assert!(app.pending_close.is_none());

        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        let text: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        assert!(text.contains("unavailable (no shell)"), "{text:?}");
        assert!(!text.contains("rename shell"));
        assert!(!text.contains("close shell"));
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
        assert!(text.contains("ID"));
        assert!(text.contains("shell-on"));
    }

    #[test]
    fn global_launcher_actions_dispatch_exact_item_and_owner() {
        let mut one = workspace("w1", "one");
        one.items = vec![launcher_view("same-id", "first")];
        let mut two = workspace("w2", "two");
        two.items = vec![launcher_view("same-id", "second")];
        let mut app = App::new(vec![one, two], project_context());
        app.select_tab(PrimaryTab::Launchers);
        app.next();
        let mut opened = None;

        app.open_selected_item(&mut |target| {
            opened = Some(target.clone());
            Ok(String::new())
        });
        app.request_rename();
        assert!(matches!(
            app.mode,
            Mode::Rename { target: RenameTarget::Launcher(ref id), .. } if id == "same-id"
        ));
        app.mode = Mode::Normal;
        app.request_close();

        assert_eq!(
            opened,
            Some(OpenTarget::Launcher {
                workspace_id: "w2".into(),
                launcher_id: "same-id".into(),
            })
        );
        assert!(matches!(
            app.pending_close,
            Some(PendingClose { target: CloseTarget::Launcher(ref id), ref name, .. })
                if id == "same-id" && name == "second"
        ));
    }

    #[test]
    fn refresh_preserves_global_selection_by_workspace_and_item_identity() {
        let mut one = workspace("w1", "one");
        one.items = vec![launcher_view("same-id", "first")];
        let mut two = workspace("w2", "two");
        two.items = vec![launcher_view("same-id", "second")];
        let mut app = App::new(vec![one, two], project_context());
        app.select_tab(PrimaryTab::Launchers);
        app.next();

        let mut refreshed_two = workspace("w2", "two");
        refreshed_two.items = vec![
            launcher_view("new", "new"),
            launcher_view("same-id", "second"),
        ];
        let mut refreshed_one = workspace("w1", "one");
        refreshed_one.items = vec![launcher_view("same-id", "first")];
        app.replace_workspaces(vec![refreshed_two, refreshed_one]);

        assert_eq!(app.global_state.selected(), Some(1));
        assert!(matches!(
            app.selected_item(),
            Some(WorkspaceItemView::Launcher(launcher)) if launcher.name == "second"
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
        right.label = "Owning workspace session".into();
        two.sessions.push(right);
        let mut app = App::new(vec![one, two], project_context());
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

        assert!(text.contains("Owning workspace session"));
        assert!(!text.contains("Wrong workspace session"));
    }

    #[test]
    fn ordinary_shell_and_launcher_hide_contextual_sessions() {
        let backend = TestBackend::new(180, 34);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = app();
        app.workspaces[0].sessions.push(session("hidden", "done"));

        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        let shell_text: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        assert!(!shell_text.contains("OpenCode sessions"));

        app.workspaces[0]
            .items
            .push(WorkspaceItemView::Launcher(LauncherView {
                id: "launcher".into(),
                name: "editor".into(),
                directory: "/tmp/boomux".into(),
                command: "editor .".into(),
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
        assert!(!launcher_text.contains("OpenCode sessions"));
    }

    #[test]
    fn session_categories_are_non_overlapping_at_boundaries() {
        let now = 1_000_000_000;
        let mut view = session("category", "idle");
        view.state_is_current = true;
        assert_eq!(session_category(&view, now), SessionCategory::Active);
        view.state_is_current = false;
        view.last_at_ms = now - 86_400_000;
        assert_eq!(session_category(&view, now), SessionCategory::Last24Hours);
        view.last_at_ms -= 1;
        assert_eq!(session_category(&view, now), SessionCategory::Last7Days);
        view.last_at_ms = now - 604_800_001;
        assert_eq!(session_category(&view, now), SessionCategory::Older);
    }

    #[test]
    fn generic_agent_names_fall_back_to_shell_and_identity() {
        let mut view = session("generic", "idle");
        view.label = "opencode".into();
        view.external_session_id = Some("ses_123456789".into());

        assert_eq!(best_session_label(&view), "agent (ses_1234)");
    }

    #[test]
    fn pi_sessions_keep_the_shell_and_identity_fallback() {
        let mut view = session("pi-generic", "idle");
        view.integration = "pi".into();
        view.label = "Pi".into();
        view.external_session_id = Some("pi_123456789".into());

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
        assert!(text.contains("ID"));
        assert!(text.contains("main"));
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
        assert!(text.contains("ID"));
        assert!(text.contains("ID term_1"));
    }

    #[test]
    fn project_launcher_renders_to_test_backend() {
        let backend = TestBackend::new(120, 34);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = app();
        app.request_add(&mut |_| Ok(String::new()));

        terminal.draw(|frame| render(frame, &mut app)).unwrap();
    }
}

use std::io;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, Cell, Clear, List, ListItem, ListState, Paragraph, Row, Table, TableState,
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
const REFRESH_INTERVAL: Duration = Duration::from_millis(250);

pub(crate) struct WorkspaceView {
    pub(crate) id: String,
    pub(crate) name: String,
    pub(crate) items: Vec<WorkspaceItemView>,
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
            .filter(|item| {
                matches!(
                    item,
                    WorkspaceItemView::Shell(_) | WorkspaceItemView::AgentShell(_)
                )
            })
            .count()
    }

    fn launcher_count(&self) -> usize {
        self.items
            .iter()
            .filter(|item| matches!(item, WorkspaceItemView::Launcher(_)))
            .count()
    }

    fn agent_count(&self) -> usize {
        self.items
            .iter()
            .filter(|item| matches!(item, WorkspaceItemView::AgentShell(_)))
            .count()
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

enum ItemIdentity {
    Shell(String),
    Launcher(String),
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
        let workspace = self.selected()?;
        let index = self.item_state.selected()?;
        workspace.items.get(index)
    }

    fn next(&mut self) {
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

    fn toggle_focus(&mut self) {
        self.focus = match self.focus {
            Focus::Workspaces => Focus::Items,
            Focus::Items => Focus::Workspaces,
        };
        self.message = None;
    }

    fn set_focus(&mut self, focus: Focus) {
        self.focus = focus;
        self.message = None;
    }

    fn handle_focus_key(&mut self, key: KeyCode) -> bool {
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
        let target = match self.focus {
            Focus::Workspaces => self
                .selected()
                .map(|workspace| RenameTarget::Workspace(workspace.id.clone())),
            Focus::Items => self.selected_item().map(|item| match item {
                WorkspaceItemView::Shell(shell) => RenameTarget::Shell(shell.id.clone()),
                WorkspaceItemView::AgentShell(agent_shell) => {
                    RenameTarget::Shell(agent_shell.shell.id.clone())
                }
                WorkspaceItemView::Launcher(launcher) => {
                    RenameTarget::Launcher(launcher.id.clone())
                }
            }),
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
        let Some(workspace_id) = self.selected().map(|workspace| workspace.id.clone()) else {
            return;
        };
        self.message = Some(Message::from_result(on_restore(&workspace_id)));
    }

    fn open_selected_item<F>(&mut self, on_open: &mut F) -> bool
    where
        F: FnMut(&OpenTarget) -> Result<String, String>,
    {
        let Some(workspace_id) = self.selected().map(|workspace| workspace.id.clone()) else {
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
        self.pending_close = match self.focus {
            Focus::Workspaces => self.selected().map(|workspace| PendingClose {
                target: CloseTarget::Workspace(workspace.id.clone()),
                name: workspace.name.clone(),
                shell_count: workspace.shell_count(),
                launcher_count: workspace.launcher_count(),
            }),
            Focus::Items => self.selected_item().map(|item| match item {
                WorkspaceItemView::Shell(shell) => PendingClose {
                    target: CloseTarget::Shell(shell.id.clone()),
                    name: shell.name.clone(),
                    shell_count: 1,
                    launcher_count: 0,
                },
                WorkspaceItemView::AgentShell(agent_shell) => PendingClose {
                    target: CloseTarget::Shell(agent_shell.shell.id.clone()),
                    name: agent_shell.shell.name.clone(),
                    shell_count: 1,
                    launcher_count: 0,
                },
                WorkspaceItemView::Launcher(launcher) => PendingClose {
                    target: CloseTarget::Launcher(launcher.id.clone()),
                    name: launcher.name.clone(),
                    shell_count: 0,
                    launcher_count: 1,
                },
            }),
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
        let selected_item = self.selected_item().map(|item| match item {
            WorkspaceItemView::Shell(shell) => ItemIdentity::Shell(shell.id.clone()),
            WorkspaceItemView::AgentShell(agent_shell) => {
                ItemIdentity::Shell(agent_shell.shell.id.clone())
            }
            WorkspaceItemView::Launcher(launcher) => ItemIdentity::Launcher(launcher.id.clone()),
        });
        let previous_index = self.selected_index().unwrap_or(0);
        let selected_index = selected_id
            .and_then(|id| workspaces.iter().position(|workspace| workspace.id == id))
            .or_else(|| (!workspaces.is_empty()).then(|| previous_index.min(workspaces.len() - 1)));

        self.workspaces = workspaces;
        self.workspace_state.select(selected_index);
        let item_index = self.selected().and_then(|workspace| {
            selected_item
                .and_then(|target| {
                    match target {
                    ItemIdentity::Shell(id) => workspace.items.iter().position(|item| match item {
                        WorkspaceItemView::Shell(shell) => shell.id == id,
                        WorkspaceItemView::AgentShell(agent_shell) => agent_shell.shell.id == id,
                        WorkspaceItemView::Launcher(_) => false,
                    }),
                    ItemIdentity::Launcher(id) => workspace.items.iter().position(|item| {
                        matches!(item, WorkspaceItemView::Launcher(launcher) if launcher.id == id)
                    }),
                }
                })
                .or_else(|| (!workspace.items.is_empty()).then_some(0))
        });
        self.item_state.select(item_index);
        if self.workspaces.is_empty() {
            self.focus = Focus::Workspaces;
        }
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
        if !key.modifiers.is_empty() {
            continue;
        }

        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => return Ok(()),
            KeyCode::Down | KeyCode::Char('j') => app.next(),
            KeyCode::Up | KeyCode::Char('k') => app.previous(),
            KeyCode::Enter => {
                let dispatched = match app.focus {
                    Focus::Workspaces => {
                        app.restore_selected(&mut actions.on_restore);
                        true
                    }
                    Focus::Items => app.open_selected_item(&mut actions.on_open),
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
            KeyCode::Tab => app.toggle_focus(),
            key if app.handle_focus_key(key) => {}
            _ => {}
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

    let [header_area, dashboard_area, footer_area] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Fill(1),
        Constraint::Length(1),
    ])
    .areas(area);

    render_header(frame, header_area, app);
    if dashboard_area.width >= 114 {
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

fn render_header(frame: &mut Frame, area: ratatui::layout::Rect, app: &App) {
    let shell_count: usize = app.workspaces.iter().map(WorkspaceView::shell_count).sum();
    let launcher_count: usize = app
        .workspaces
        .iter()
        .map(WorkspaceView::launcher_count)
        .sum();
    let agent_count: usize = app.workspaces.iter().map(WorkspaceView::agent_count).sum();
    let line = Line::from(vec![
        Span::styled(
            " BOOMUX ",
            Style::new().fg(TEAL).add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!("{} workspaces", app.workspaces.len()),
            Style::new().fg(GREEN),
        ),
        Span::styled("  |  ", Style::new().fg(OVERLAY)),
        Span::styled(format!("{shell_count} shells"), Style::new().fg(BLUE)),
        Span::styled("  |  ", Style::new().fg(OVERLAY)),
        Span::styled(
            format!("{launcher_count} launchers"),
            Style::new().fg(YELLOW),
        ),
        Span::styled("  |  ", Style::new().fg(OVERLAY)),
        Span::styled(format!("{agent_count} agents"), Style::new().fg(TEAL)),
        Span::styled(
            "    tab/h/l/arrows switches panes",
            Style::new().fg(SUBTEXT),
        ),
    ]);
    frame.render_widget(Paragraph::new(line).style(Style::new().bg(BASE)), area);
}

fn render_workspaces(frame: &mut Frame, area: ratatui::layout::Rect, app: &mut App) {
    let rows = app.workspaces.iter().map(|workspace| {
        Row::new([
            Cell::from(workspace.name.as_str()),
            Cell::from(workspace.shell_count().to_string()),
            Cell::from(workspace.launcher_count().to_string()),
            Cell::from(workspace.agent_count().to_string()),
        ])
    });
    let table = Table::new(
        rows,
        [
            Constraint::Min(8),
            Constraint::Length(6),
            Constraint::Length(9),
            Constraint::Length(6),
        ],
    )
    .header(
        Row::new(["NAME", "SHELLS", "LAUNCHERS", "AGENTS"])
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
    let show_full_ids = inner.width >= 150;
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
    let widths = shell_column_widths(inner.width, show_full_ids);
    frame.render_widget(block, area);
    let table_area = if show_full_ids {
        inner
    } else {
        let [detail_area, table_area] =
            Layout::vertical([Constraint::Length(1), Constraint::Fill(1)]).areas(inner);
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

fn short_id(id: &str) -> String {
    id.chars().take(8).collect()
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
                " navigate  tab/h/l/arrows focus  ",
                Style::new().fg(SUBTEXT),
            ),
            Span::styled("a", Style::new().fg(GREEN)),
            Span::styled(
                if app.focus == Focus::Workspaces {
                    " create workspace  "
                } else {
                    " add shell  "
                },
                Style::new().fg(SUBTEXT),
            ),
        ];
        spans.extend([
            Span::styled("e", Style::new().fg(YELLOW)),
            Span::styled(
                if app.focus == Focus::Workspaces {
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
                if app.focus == Focus::Workspaces {
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
                if app.focus == Focus::Workspaces {
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

        app.toggle_focus();
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
    fn refresh_keeps_terminal_focus_for_an_empty_workspace() {
        let mut empty = workspace("w1", "empty");
        empty.items.clear();
        let mut app = App::new(vec![empty], project_context());
        app.toggle_focus();

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
        app.toggle_focus();
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
        app.toggle_focus();

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
        app.toggle_focus();
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
        app.toggle_focus();

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
        app.toggle_focus();

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
        app.toggle_focus();
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
        app.toggle_focus();
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
        app.toggle_focus();
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
        app.toggle_focus();
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
        app.toggle_focus();

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
        app.toggle_focus();
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
        let backend = TestBackend::new(120, 34);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = app();
        let mut agent_shell = agent_shell();
        agent_shell.shell.name = "keepname".into();
        app.workspaces[0].items[0] = WorkspaceItemView::AgentShell(agent_shell);
        app.toggle_focus();

        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        let text: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect();

        assert_eq!(app.workspaces[0].agent_count(), 1);
        assert_eq!(app.workspaces[0].shell_count(), 1);
        assert!(text.contains("1 agents"));
        assert!(text.contains("1 shells"));
        assert!(text.contains("AGENTS"));
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
        app.toggle_focus();

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

        app.toggle_focus();
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

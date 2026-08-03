use std::io;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
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
    pub(crate) directory: String,
    pub(crate) repository: String,
    pub(crate) branch: String,
    pub(crate) git_state: String,
    pub(crate) worktree: String,
    pub(crate) terminals: Vec<TerminalView>,
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
    terminal_state: TableState,
    focus: Focus,
    mode: Mode,
    message: Option<Message>,
    pending_close: Option<PendingClose>,
    project_context: ProjectContext,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Focus {
    Workspaces,
    Terminals,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum RenameTarget {
    Workspace(String),
    Shell(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum CloseTarget {
    Workspace(String),
    Shell(String),
}

impl RenameTarget {
    fn label(&self) -> &'static str {
        match self {
            Self::Workspace(_) => "workspace",
            Self::Shell(_) => "shell",
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
        let mut terminal_state = TableState::default();
        if !workspaces.is_empty() {
            workspace_state.select(Some(0));
            if !workspaces[0].terminals.is_empty() {
                terminal_state.select(Some(0));
            }
        }
        Self {
            workspaces,
            workspace_state,
            terminal_state,
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

    fn selected_terminal(&self) -> Option<&TerminalView> {
        self.selected().and_then(|workspace| {
            self.terminal_state
                .selected()
                .and_then(|index| workspace.terminals.get(index))
        })
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
                self.select_first_terminal();
            }
            Focus::Terminals => {
                let terminal_count = self
                    .selected()
                    .map_or(0, |workspace| workspace.terminals.len());
                if terminal_count == 0 {
                    return;
                }
                let next = self
                    .terminal_state
                    .selected()
                    .map_or(0, |index| (index + 1) % terminal_count);
                self.terminal_state.select(Some(next));
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
                self.select_first_terminal();
            }
            Focus::Terminals => {
                let terminal_count = self
                    .selected()
                    .map_or(0, |workspace| workspace.terminals.len());
                if terminal_count == 0 {
                    return;
                }
                let previous = self.terminal_state.selected().map_or(0, |index| {
                    if index == 0 {
                        terminal_count - 1
                    } else {
                        index - 1
                    }
                });
                self.terminal_state.select(Some(previous));
            }
        }
        self.message = None;
    }

    fn toggle_focus(&mut self) {
        self.focus = match self.focus {
            Focus::Workspaces => Focus::Terminals,
            Focus::Terminals => Focus::Workspaces,
        };
        self.message = None;
    }

    fn select_first_terminal(&mut self) {
        self.terminal_state.select(
            self.selected()
                .is_some_and(|workspace| !workspace.terminals.is_empty())
                .then_some(0),
        );
    }

    fn request_rename(&mut self) {
        let target = match self.focus {
            Focus::Workspaces => self
                .selected()
                .map(|workspace| RenameTarget::Workspace(workspace.id.clone())),
            Focus::Terminals => self
                .selected_terminal()
                .map(|terminal| RenameTarget::Shell(terminal.id.clone())),
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
            Focus::Terminals => {
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

    fn open_selected_terminal<F>(&mut self, on_open: &mut F)
    where
        F: FnMut(&str) -> Result<String, String>,
    {
        let Some(terminal_id) = self.selected_terminal().map(|terminal| terminal.id.clone()) else {
            return;
        };
        self.message = Some(Message::from_result(on_open(&terminal_id)));
    }

    fn request_close(&mut self) {
        self.pending_close = match self.focus {
            Focus::Workspaces => self.selected().map(|workspace| PendingClose {
                target: CloseTarget::Workspace(workspace.id.clone()),
                name: workspace.name.clone(),
                shell_count: workspace.terminals.len(),
            }),
            Focus::Terminals => self.selected_terminal().map(|terminal| PendingClose {
                target: CloseTarget::Shell(terminal.id.clone()),
                name: terminal.name.clone(),
                shell_count: 1,
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
        let selected_terminal_id = self.selected_terminal().map(|terminal| terminal.id.clone());
        let previous_index = self.selected_index().unwrap_or(0);
        let selected_index = selected_id
            .and_then(|id| workspaces.iter().position(|workspace| workspace.id == id))
            .or_else(|| (!workspaces.is_empty()).then(|| previous_index.min(workspaces.len() - 1)));

        self.workspaces = workspaces;
        self.workspace_state.select(selected_index);
        let terminal_index = self.selected().and_then(|workspace| {
            selected_terminal_id
                .and_then(|id| {
                    workspace
                        .terminals
                        .iter()
                        .position(|terminal| terminal.id == id)
                })
                .or_else(|| (!workspace.terminals.is_empty()).then_some(0))
        });
        self.terminal_state.select(terminal_index);
        if self.focus == Focus::Terminals && self.workspaces.is_empty() {
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
    O: FnMut(&str) -> Result<String, String>,
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
    O: FnMut(&str) -> Result<String, String>,
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
                match app.focus {
                    Focus::Workspaces => app.restore_selected(&mut actions.on_restore),
                    Focus::Terminals => app.open_selected_terminal(&mut actions.on_open),
                }
                app.refresh(&mut actions.on_refresh);
                last_refresh = Instant::now();
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

    let [header_area, workspace_area, terminal_area, footer_area] = Layout::vertical([
        Constraint::Length(3),
        Constraint::Percentage(38),
        Constraint::Fill(1),
        Constraint::Length(1),
    ])
    .areas(area);

    render_header(frame, header_area, app);
    render_workspaces(frame, workspace_area, app);
    render_terminals(frame, terminal_area, app);
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
    let shell_count: usize = app
        .workspaces
        .iter()
        .map(|workspace| workspace.terminals.len())
        .sum();
    let line = Line::from(vec![
        Span::styled(
            " BOOMUX DASHBOARD ",
            Style::new().fg(TEAL).add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!("{} workspaces", app.workspaces.len()),
            Style::new().fg(GREEN),
        ),
        Span::styled("  |  ", Style::new().fg(OVERLAY)),
        Span::styled(format!("{shell_count} shells"), Style::new().fg(BLUE)),
        Span::styled("    Tab switches tables", Style::new().fg(SUBTEXT)),
    ]);
    frame.render_widget(
        Paragraph::new(line).block(
            Block::bordered()
                .border_style(Style::new().fg(TEAL))
                .style(Style::new().bg(BASE)),
        ),
        area,
    );
}

fn render_workspaces(frame: &mut Frame, area: ratatui::layout::Rect, app: &mut App) {
    let (header, rows, widths) = if area.width >= 140 {
        let rows: Vec<_> = app
            .workspaces
            .iter()
            .enumerate()
            .map(|(index, workspace)| {
                Row::new(vec![
                    Cell::from((index + 1).to_string()),
                    Cell::from(workspace.name.as_str()),
                    Cell::from(workspace.repository.as_str()),
                    Cell::from(workspace.branch.as_str()),
                    git_state_cell(&workspace.git_state),
                    Cell::from(workspace.worktree.as_str()),
                    Cell::from(workspace.terminals.len().to_string()),
                    Cell::from(workspace.directory.as_str()),
                ])
            })
            .collect();
        (
            Row::new([
                "#",
                "NAME",
                "REPOSITORY",
                "BRANCH",
                "DIRTY",
                "WORKTREE",
                "SHELLS",
                "DIRECTORY",
            ]),
            rows,
            vec![
                Constraint::Length(4),
                Constraint::Length(18),
                Constraint::Length(18),
                Constraint::Length(22),
                Constraint::Length(12),
                Constraint::Length(18),
                Constraint::Length(8),
                Constraint::Min(24),
            ],
        )
    } else if area.width >= 100 {
        let rows: Vec<_> = app
            .workspaces
            .iter()
            .enumerate()
            .map(|(index, workspace)| {
                let name = if workspace.repository == "-" || workspace.repository == workspace.name
                {
                    workspace.name.clone()
                } else {
                    format!("{} / {}", workspace.name, workspace.repository)
                };
                Row::new(vec![
                    Cell::from((index + 1).to_string()),
                    Cell::from(name),
                    Cell::from(workspace.branch.as_str()),
                    git_state_cell(&workspace.git_state),
                    Cell::from(workspace.worktree.as_str()),
                    Cell::from(workspace.terminals.len().to_string()),
                ])
            })
            .collect();
        (
            Row::new([
                "#",
                "WORKSPACE / REPOSITORY",
                "BRANCH",
                "DIRTY",
                "WORKTREE",
                "SHELLS",
            ]),
            rows,
            vec![
                Constraint::Length(4),
                Constraint::Min(20),
                Constraint::Length(18),
                Constraint::Length(12),
                Constraint::Length(18),
                Constraint::Length(7),
            ],
        )
    } else {
        let rows: Vec<_> = app
            .workspaces
            .iter()
            .enumerate()
            .map(|(index, workspace)| {
                let name = if workspace.repository == "-" || workspace.repository == workspace.name
                {
                    workspace.name.clone()
                } else {
                    format!("{} / {}", workspace.name, workspace.repository)
                };
                Row::new(vec![
                    Cell::from((index + 1).to_string()),
                    Cell::from(Text::from(vec![
                        Line::from(name),
                        Line::from(format!(
                            "{} shell{}",
                            workspace.terminals.len(),
                            if workspace.terminals.len() == 1 {
                                ""
                            } else {
                                "s"
                            }
                        )),
                    ])),
                    Cell::from(Text::from(vec![
                        Line::from(workspace.branch.as_str()),
                        Line::from(vec![
                            Span::styled(
                                workspace.git_state.as_str(),
                                Style::new().fg(git_state_color(&workspace.git_state)),
                            ),
                            Span::raw(" | "),
                            Span::raw(workspace.worktree.as_str()),
                        ]),
                    ])),
                ])
                .height(2)
            })
            .collect();
        (
            Row::new(["#", "WORKSPACE / REPOSITORY", "GIT DETAILS"]),
            rows,
            vec![
                Constraint::Length(4),
                Constraint::Min(24),
                Constraint::Length(34),
            ],
        )
    };
    let table = Table::new(rows, widths)
        .header(header.style(Style::new().fg(BLUE).add_modifier(Modifier::BOLD)))
        .column_spacing(1)
        .block(
            Block::bordered()
                .title(" Workspaces ")
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

fn git_state_cell(state: &str) -> Cell<'_> {
    Cell::from(Span::styled(state, Style::new().fg(git_state_color(state))))
}

fn render_terminals(frame: &mut Frame, area: ratatui::layout::Rect, app: &mut App) {
    let header = Row::new(["#", "NAME", "STATUS", "DIRECTORY", "SHELL ID"])
        .style(Style::new().fg(BLUE).add_modifier(Modifier::BOLD));
    let selected = app
        .workspace_state
        .selected()
        .and_then(|index| app.workspaces.get(index));
    let rows: Vec<_> = selected
        .into_iter()
        .flat_map(|workspace| workspace.terminals.iter())
        .enumerate()
        .map(|(index, terminal)| {
            Row::new(vec![
                Cell::from((index + 1).to_string()),
                Cell::from(terminal.name.as_str()),
                Cell::from(Span::styled(
                    terminal.status.as_str(),
                    Style::new().fg(status_color(&terminal.status)),
                )),
                Cell::from(terminal.directory.as_str()),
                Cell::from(terminal.id.as_str()),
            ])
        })
        .collect();
    let title = selected.map_or_else(
        || " Terminals ".to_owned(),
        |workspace| format!(" Terminals: {} ", workspace.name),
    );
    let table = Table::new(
        rows,
        [
            Constraint::Length(4),
            Constraint::Length(18),
            Constraint::Length(12),
            Constraint::Min(24),
            Constraint::Length(24),
        ],
    )
    .header(header)
    .column_spacing(1)
    .block(Block::bordered().title(title).border_style(Style::new().fg(
        if app.focus == Focus::Terminals {
            TEAL
        } else {
            OVERLAY
        },
    )))
    .row_highlight_style(
        Style::new()
            .fg(TEXT)
            .add_modifier(Modifier::BOLD | Modifier::REVERSED),
    )
    .highlight_symbol("> ");
    frame.render_stateful_widget(table, area, &mut app.terminal_state);
}

fn render_footer(frame: &mut Frame, area: ratatui::layout::Rect, app: &App) {
    let line = if let Some(pending) = &app.pending_close {
        let prompt = match pending.target {
            CloseTarget::Workspace(_) => format!(
                " Close workspace '{}' and terminate {} shell(s)?  ",
                pending.name, pending.shell_count
            ),
            CloseTarget::Shell(_) => {
                format!(
                    " Close shell '{}' and terminate its process?  ",
                    pending.name
                )
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
        let mut spans = vec![
            Span::styled(" j/k", Style::new().fg(TEAL)),
            Span::styled(" navigate  tab focus  ", Style::new().fg(SUBTEXT)),
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
                } else {
                    " rename shell  "
                },
                Style::new().fg(SUBTEXT),
            ),
        ]);
        spans.extend([
            Span::styled("enter", Style::new().fg(GREEN)),
            Span::styled(
                if app.focus == Focus::Workspaces {
                    " restore workspace  "
                } else {
                    " open shell  "
                },
                Style::new().fg(SUBTEXT),
            ),
            Span::styled("r", Style::new().fg(BLUE)),
            Span::styled(" refresh  ", Style::new().fg(SUBTEXT)),
            Span::styled("x", Style::new().fg(RED)),
            Span::styled(
                if app.focus == Focus::Workspaces {
                    " close workspace  "
                } else {
                    " close shell  "
                },
                Style::new().fg(SUBTEXT),
            ),
            Span::styled("q", Style::new().fg(RED)),
            Span::styled(" quit", Style::new().fg(SUBTEXT)),
        ]);
        Line::from(spans)
    };
    frame.render_widget(Paragraph::new(line), area);
}

fn status_color(status: &str) -> Color {
    match status {
        "exited" => SUBTEXT,
        _ => TEAL,
    }
}

fn git_state_color(state: &str) -> Color {
    if state == "clean" {
        GREEN
    } else if state.contains("conflict") {
        RED
    } else if state == "-" || state == "unknown" {
        SUBTEXT
    } else {
        YELLOW
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
            directory: "/tmp/boomux".into(),
            repository: "boomux".into(),
            branch: "main".into(),
            git_state: "clean".into(),
            worktree: "primary".into(),
            terminals: vec![TerminalView {
                id: "term_1".into(),
                name: "agent".into(),
                status: "running".into(),
                directory: "/tmp/boomux".into(),
            }],
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
    fn git_states_use_semantic_colors() {
        assert_eq!(git_state_color("clean"), GREEN);
        assert_eq!(git_state_color("3 changed"), YELLOW);
        assert_eq!(git_state_color("1 conflict"), RED);
        assert_eq!(git_state_color("-"), SUBTEXT);
    }

    #[test]
    fn terminal_navigation_uses_the_focused_table() {
        let mut app = app();

        app.toggle_focus();
        assert_eq!(app.focus, Focus::Terminals);
        app.next();

        assert_eq!(app.terminal_state.selected(), Some(0));
        assert_eq!(
            app.selected_terminal()
                .map(|terminal| terminal.name.as_str()),
            Some("agent")
        );
    }

    #[test]
    fn refresh_keeps_terminal_focus_for_an_empty_workspace() {
        let mut empty = workspace("w1", "empty");
        empty.terminals.clear();
        let mut app = App::new(vec![empty], project_context());
        app.toggle_focus();

        let mut refreshed = workspace("w1", "empty");
        refreshed.terminals.clear();
        app.replace_workspaces(vec![refreshed]);

        assert_eq!(app.focus, Focus::Terminals);
        assert!(app.terminal_state.selected().is_none());
    }

    #[test]
    fn empty_terminal_focus_can_create_the_first_shell() {
        let mut empty = workspace("w1", "empty");
        empty.terminals.clear();
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

        app.open_selected_terminal(&mut |terminal_id| {
            opened = Some(terminal_id.to_owned());
            Ok("Opened shell".into())
        });

        assert_eq!(opened.as_deref(), Some("term_1"));
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
        assert!(text.contains("BRANCH"));
        assert!(text.contains("DIRTY"));
        assert!(text.contains("WORKTREE"));
        assert!(text.contains("SHELLS"));
        assert!(text.contains("primary"));
    }

    #[test]
    fn wide_dashboard_renders_repository_and_directory_columns() {
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
        assert!(text.contains("REPOSITORY"));
        assert!(text.contains("DIRECTORY"));
    }

    #[test]
    fn narrow_dashboard_keeps_workspace_and_git_details_visible() {
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
        assert!(text.contains("WORKSPACE / REPOSITORY"));
        assert!(text.contains("GIT DETAILS"));
        assert!(text.contains("main"));
        assert!(text.contains("clean"));
        assert!(text.contains("primary"));
        assert!(text.contains("1 shell"));
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

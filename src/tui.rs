use std::env;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crossterm::event::{self, Event, KeyCode, KeyEventKind};
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
const MAX_DIRECTORY_ENTRIES: usize = 500;
const MAX_RECENT_DIRECTORIES: usize = 10;

pub(crate) struct WorkspaceView {
    pub(crate) id: String,
    pub(crate) name: String,
    pub(crate) status: String,
    pub(crate) directory: String,
    pub(crate) terminals: Vec<TerminalView>,
}

pub(crate) struct DirectoryContext {
    pub(crate) launch_directory: PathBuf,
    pub(crate) recent_directories: Vec<PathBuf>,
}

pub(crate) struct TerminalView {
    pub(crate) id: String,
    pub(crate) pane_id: String,
    pub(crate) name: String,
    pub(crate) kind: String,
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
    directory_context: DirectoryContext,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Focus {
    Workspaces,
    Terminals,
}

enum Mode {
    Normal,
    PickDirectory(DirectoryPicker),
    Rename { pane_id: String, input: String },
}

struct DirectoryPicker {
    entries: Vec<DirectoryEntry>,
    state: ListState,
    browsing: Option<PathBuf>,
    error: Option<String>,
}

struct DirectoryEntry {
    label: String,
    path: PathBuf,
}

struct Message {
    text: String,
    error: bool,
}

struct PendingClose {
    id: String,
    name: String,
    shell_count: usize,
}

impl DirectoryPicker {
    fn new(context: &DirectoryContext, selected_directory: Option<&str>) -> Self {
        let mut entries = Vec::new();
        push_directory_entry(&mut entries, "current", context.launch_directory.clone());
        if let Some(directory) = selected_directory {
            push_directory_entry(&mut entries, "selected workspace", PathBuf::from(directory));
        }
        for directory in &context.recent_directories {
            push_directory_entry(&mut entries, "recent", directory.clone());
        }
        let mut state = ListState::default();
        if !entries.is_empty() {
            state.select(Some(0));
        }
        Self {
            entries,
            state,
            browsing: None,
            error: None,
        }
    }

    fn selected_path(&self) -> Option<&Path> {
        self.state
            .selected()
            .and_then(|index| self.entries.get(index))
            .map(|entry| entry.path.as_path())
    }

    fn next(&mut self) {
        if self.entries.is_empty() {
            return;
        }
        let next = self
            .state
            .selected()
            .map_or(0, |index| (index + 1) % self.entries.len());
        self.state.select(Some(next));
    }

    fn previous(&mut self) {
        if self.entries.is_empty() {
            return;
        }
        let previous = self.state.selected().map_or(0, |index| {
            if index == 0 {
                self.entries.len() - 1
            } else {
                index - 1
            }
        });
        self.state.select(Some(previous));
    }

    fn browse_selected(&mut self) {
        let Some(directory) = self.selected_path().map(Path::to_owned) else {
            return;
        };
        self.show_directory(directory);
    }

    fn browse_parent(&mut self) {
        let Some(parent) = self
            .browsing
            .as_deref()
            .or_else(|| self.selected_path())
            .and_then(Path::parent)
            .map(Path::to_owned)
        else {
            return;
        };
        self.show_directory(parent);
    }

    fn show_directory(&mut self, directory: PathBuf) {
        match child_directories(&directory) {
            Ok(children) => {
                let mut entries = vec![DirectoryEntry {
                    label: "use this directory".into(),
                    path: directory.clone(),
                }];
                entries.extend(children.into_iter().map(|path| {
                    DirectoryEntry {
                        label: path
                            .file_name()
                            .and_then(|name| name.to_str())
                            .unwrap_or("directory")
                            .to_owned(),
                        path,
                    }
                }));
                self.entries = entries;
                self.state.select(Some(0));
                self.browsing = Some(directory);
                self.error = None;
            }
            Err(error) => self.error = Some(error.to_string()),
        }
    }
}

fn child_directories(directory: &Path) -> io::Result<Vec<PathBuf>> {
    let mut children: Vec<_> = fs::read_dir(directory)?
        .take(MAX_DIRECTORY_ENTRIES)
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.is_dir())
        .collect();
    children.sort_by_cached_key(|path| {
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or_default()
            .to_lowercase()
    });
    Ok(children)
}

fn push_directory_entry(entries: &mut Vec<DirectoryEntry>, label: &str, path: PathBuf) {
    if path.is_dir() && !entries.iter().any(|entry| entry.path == path) {
        entries.push(DirectoryEntry {
            label: label.to_owned(),
            path,
        });
    }
}

impl App {
    fn new(workspaces: Vec<WorkspaceView>, directory_context: DirectoryContext) -> Self {
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
            directory_context,
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

    fn remember_directory(&mut self, directory: &Path) {
        self.directory_context
            .recent_directories
            .retain(|recent| recent != directory);
        self.directory_context
            .recent_directories
            .insert(0, directory.to_owned());
        self.directory_context
            .recent_directories
            .truncate(MAX_RECENT_DIRECTORIES);
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
        if self.focus == Focus::Terminals
            && let Some(terminal) = self.selected_terminal()
        {
            self.mode = Mode::Rename {
                pane_id: terminal.pane_id.clone(),
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
                let selected_directory =
                    self.selected().map(|workspace| workspace.directory.clone());
                self.mode = Mode::PickDirectory(DirectoryPicker::new(
                    &self.directory_context,
                    selected_directory.as_deref(),
                ));
                self.message = None;
                false
            }
            Focus::Terminals => {
                let Some(workspace_id) = self.selected().map(|workspace| workspace.id.clone())
                else {
                    return false;
                };
                self.message = Some(match on_create_shell(&workspace_id) {
                    Ok(text) => Message { text, error: false },
                    Err(text) => Message { text, error: true },
                });
                true
            }
        }
    }

    fn create_workspace<F>(&mut self, directory: &Path, on_create_workspace: &mut F)
    where
        F: FnMut(&Path) -> Result<String, String>,
    {
        self.mode = Mode::Normal;
        self.message = Some(match on_create_workspace(directory) {
            Ok(text) => {
                self.remember_directory(directory);
                Message { text, error: false }
            }
            Err(text) => Message { text, error: true },
        });
    }

    fn rename_shell<F>(&mut self, pane_id: &str, name: &str, on_rename: &mut F)
    where
        F: FnMut(&str, &str) -> Result<String, String>,
    {
        self.mode = Mode::Normal;
        self.message = Some(match on_rename(pane_id, name) {
            Ok(text) => Message { text, error: false },
            Err(text) => Message { text, error: true },
        });
    }

    fn restore_selected<F>(&mut self, on_restore: &mut F)
    where
        F: FnMut(&str) -> Result<String, String>,
    {
        let Some((workspace_id, directory)) = self
            .selected()
            .map(|workspace| (workspace.id.clone(), workspace.directory.clone()))
        else {
            return;
        };
        self.message = Some(match on_restore(&workspace_id) {
            Ok(text) => {
                self.remember_directory(Path::new(&directory));
                Message { text, error: false }
            }
            Err(text) => Message { text, error: true },
        });
    }

    fn open_selected_terminal<F>(&mut self, on_open: &mut F)
    where
        F: FnMut(&str) -> Result<String, String>,
    {
        let Some((terminal_id, directory)) = self
            .selected_terminal()
            .map(|terminal| (terminal.id.clone(), terminal.directory.clone()))
        else {
            return;
        };
        self.message = Some(match on_open(&terminal_id) {
            Ok(text) => {
                self.remember_directory(Path::new(&directory));
                Message { text, error: false }
            }
            Err(text) => Message { text, error: true },
        });
    }

    fn request_close(&mut self) {
        self.pending_close = self.selected().map(|workspace| PendingClose {
            id: workspace.id.clone(),
            name: workspace.name.clone(),
            shell_count: workspace.terminals.len(),
        });
    }

    fn cancel_close(&mut self) {
        self.pending_close = None;
    }

    fn confirm_close<F>(&mut self, on_close: &mut F)
    where
        F: FnMut(&str) -> Result<String, String>,
    {
        let Some(pending) = self.pending_close.take() else {
            return;
        };
        self.message = Some(match on_close(&pending.id) {
            Ok(text) => Message { text, error: false },
            Err(text) => Message { text, error: true },
        });
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
        if self.focus == Focus::Terminals && terminal_index.is_none() {
            self.focus = Focus::Workspaces;
        }
    }
}

pub(crate) fn run<R, O, C, W, N, E, F>(
    workspaces: Vec<WorkspaceView>,
    directory_context: DirectoryContext,
    actions: Actions<R, O, C, W, N, E, F>,
) -> io::Result<()>
where
    R: FnMut(&str) -> Result<String, String>,
    O: FnMut(&str) -> Result<String, String>,
    C: FnMut(&str) -> Result<String, String>,
    W: FnMut(&Path) -> Result<String, String>,
    N: FnMut(&str) -> Result<String, String>,
    E: FnMut(&str, &str) -> Result<String, String>,
    F: FnMut() -> Result<Vec<WorkspaceView>, String>,
{
    let mut terminal = ratatui::init();
    let result = run_loop(
        &mut terminal,
        App::new(workspaces, directory_context),
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
    C: FnMut(&str) -> Result<String, String>,
    W: FnMut(&Path) -> Result<String, String>,
    N: FnMut(&str) -> Result<String, String>,
    E: FnMut(&str, &str) -> Result<String, String>,
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

        if app.pending_close.is_some() {
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
                &mut actions.on_create_workspace,
                &mut actions.on_rename,
            ) {
                app.refresh(&mut actions.on_refresh);
                last_refresh = Instant::now();
            }
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
    on_create_workspace: &mut W,
    on_rename: &mut E,
) -> bool
where
    W: FnMut(&Path) -> Result<String, String>,
    E: FnMut(&str, &str) -> Result<String, String>,
{
    let mode = std::mem::replace(&mut app.mode, Mode::Normal);
    match mode {
        Mode::Normal => false,
        Mode::PickDirectory(mut picker) => match key {
            KeyCode::Enter if picker.selected_path().is_some() => {
                let directory = picker
                    .selected_path()
                    .expect("selected directory")
                    .to_owned();
                app.create_workspace(&directory, on_create_workspace);
                true
            }
            KeyCode::Esc => false,
            KeyCode::Down | KeyCode::Char('j') => {
                picker.next();
                app.mode = Mode::PickDirectory(picker);
                false
            }
            KeyCode::Up | KeyCode::Char('k') => {
                picker.previous();
                app.mode = Mode::PickDirectory(picker);
                false
            }
            KeyCode::Right | KeyCode::Char('l') => {
                picker.browse_selected();
                app.mode = Mode::PickDirectory(picker);
                false
            }
            KeyCode::Left | KeyCode::Char('h') | KeyCode::Backspace => {
                picker.browse_parent();
                app.mode = Mode::PickDirectory(picker);
                false
            }
            _ => {
                app.mode = Mode::PickDirectory(picker);
                false
            }
        },
        Mode::Rename { pane_id, mut input } => match key {
            KeyCode::Enter if !input.trim().is_empty() => {
                let name = input.trim().to_owned();
                app.rename_shell(&pane_id, &name, on_rename);
                true
            }
            KeyCode::Esc => false,
            KeyCode::Backspace => {
                input.pop();
                app.mode = Mode::Rename { pane_id, input };
                false
            }
            KeyCode::Char(character) => {
                input.push(character);
                app.mode = Mode::Rename { pane_id, input };
                false
            }
            _ => {
                app.mode = Mode::Rename { pane_id, input };
                false
            }
        },
    }
}

fn render(frame: &mut Frame, app: &mut App) {
    let area = frame.area();
    frame.render_widget(Block::new().style(Style::new().bg(BASE)), area);

    let [
        header_area,
        workspace_area,
        terminal_area,
        metrics_area,
        footer_area,
    ] = Layout::vertical([
        Constraint::Length(3),
        Constraint::Percentage(38),
        Constraint::Fill(1),
        Constraint::Length(5),
        Constraint::Length(1),
    ])
    .areas(area);

    render_header(frame, header_area, app);
    render_workspaces(frame, workspace_area, app);
    render_terminals(frame, terminal_area, app);
    render_metrics(frame, metrics_area, app);
    render_footer(frame, footer_area, app);
    if let Mode::PickDirectory(picker) = &mut app.mode {
        render_directory_picker(frame, area, picker);
    }
}

fn render_directory_picker(frame: &mut Frame, area: Rect, picker: &mut DirectoryPicker) {
    let popup_area = centered_rect(area, 80, 72);
    frame.render_widget(Clear, popup_area);
    frame.render_widget(Block::new().style(Style::new().bg(BASE)), popup_area);
    let [list_area, help_area] =
        Layout::vertical([Constraint::Fill(1), Constraint::Length(2)]).areas(popup_area);
    let items = picker.entries.iter().map(|entry| {
        ListItem::new(Line::from(vec![
            Span::styled(format!("{:<20}", entry.label), Style::new().fg(TEAL)),
            Span::styled(display_path(&entry.path), Style::new().fg(TEXT)),
        ]))
    });
    let title = picker.browsing.as_ref().map_or_else(
        || " Create workspace: quick locations ".to_owned(),
        |directory| format!(" Browse: {} ", display_path(directory)),
    );
    let list = List::new(items)
        .block(
            Block::bordered()
                .title(title)
                .border_style(Style::new().fg(TEAL)),
        )
        .highlight_symbol("> ")
        .highlight_style(
            Style::new()
                .fg(TEXT)
                .add_modifier(Modifier::BOLD | Modifier::REVERSED),
        );
    frame.render_stateful_widget(list, list_area, &mut picker.state);

    let help = if let Some(error) = &picker.error {
        Line::from(Span::styled(format!(" {error}"), Style::new().fg(RED)))
    } else {
        Line::from(vec![
            Span::styled(" enter", Style::new().fg(GREEN)),
            Span::raw(" create  "),
            Span::styled("l/right", Style::new().fg(BLUE)),
            Span::raw(" browse  "),
            Span::styled("h/left", Style::new().fg(YELLOW)),
            Span::raw(" parent  "),
            Span::styled("esc", Style::new().fg(RED)),
            Span::raw(" cancel"),
        ])
    };
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

fn display_path(path: &Path) -> String {
    let Some(home) = env::var_os("HOME").map(PathBuf::from) else {
        return path.display().to_string();
    };
    if path == home {
        "~".into()
    } else if let Ok(relative) = path.strip_prefix(&home) {
        format!("~/{}", relative.display())
    } else {
        path.display().to_string()
    }
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
    let header = Row::new(["#", "NAME", "STATUS", "SHELLS", "DIRECTORY", "ID"])
        .style(Style::new().fg(BLUE).add_modifier(Modifier::BOLD));
    let rows = app.workspaces.iter().enumerate().map(|(index, workspace)| {
        Row::new(vec![
            Cell::from((index + 1).to_string()),
            Cell::from(workspace.name.as_str()),
            Cell::from(status_span(&workspace.status)),
            Cell::from(workspace.terminals.len().to_string()),
            Cell::from(workspace.directory.as_str()),
            Cell::from(workspace.id.as_str()),
        ])
    });
    let table = Table::new(
        rows,
        [
            Constraint::Length(4),
            Constraint::Length(20),
            Constraint::Length(12),
            Constraint::Length(8),
            Constraint::Min(24),
            Constraint::Length(12),
        ],
    )
    .header(header)
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

fn render_terminals(frame: &mut Frame, area: ratatui::layout::Rect, app: &mut App) {
    let header = Row::new(["#", "NAME", "TYPE", "STATUS", "DIRECTORY", "TERMINAL ID"])
        .style(Style::new().fg(BLUE).add_modifier(Modifier::BOLD));
    let rows: Vec<_> = app
        .selected()
        .into_iter()
        .flat_map(|workspace| workspace.terminals.iter())
        .enumerate()
        .map(|(index, terminal)| {
            Row::new(vec![
                Cell::from((index + 1).to_string()),
                Cell::from(terminal.name.clone()),
                Cell::from(terminal.kind.clone()),
                Cell::from(Span::styled(
                    status_label(&terminal.status).to_owned(),
                    Style::new().fg(status_color(&terminal.status)),
                )),
                Cell::from(terminal.directory.clone()),
                Cell::from(terminal.id.clone()),
            ])
        })
        .collect();
    let title = app.selected().map_or_else(
        || " Terminals ".to_owned(),
        |workspace| format!(" Terminals: {} ", workspace.name),
    );
    let table = Table::new(
        rows,
        [
            Constraint::Length(4),
            Constraint::Length(18),
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

fn render_metrics(frame: &mut Frame, area: ratatui::layout::Rect, app: &App) {
    let [name_area, id_area, shells_area, status_area] = Layout::horizontal([
        Constraint::Ratio(1, 4),
        Constraint::Ratio(1, 4),
        Constraint::Ratio(1, 4),
        Constraint::Ratio(1, 4),
    ])
    .areas(area);
    let selected = app.selected();

    render_metric(
        frame,
        name_area,
        "Workspace",
        selected.map_or("-", |workspace| workspace.name.as_str()),
        TEXT,
    );
    render_metric(
        frame,
        id_area,
        "Herdr ID",
        selected.map_or("-", |workspace| workspace.id.as_str()),
        BLUE,
    );
    let shell_count = selected
        .map(|workspace| workspace.terminals.len().to_string())
        .unwrap_or_else(|| "0".into());
    render_metric(frame, shells_area, "Shells", &shell_count, GREEN);
    let status = selected.map_or("-", |workspace| workspace.status.as_str());
    render_metric(
        frame,
        status_area,
        "Agent State",
        status,
        status_color(status),
    );
}

fn render_metric(
    frame: &mut Frame,
    area: ratatui::layout::Rect,
    title: &str,
    value: &str,
    color: Color,
) {
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            format!(" {value}"),
            Style::new().fg(color).add_modifier(Modifier::BOLD),
        )))
        .block(
            Block::bordered()
                .title(format!(" {title} "))
                .border_style(Style::new().fg(OVERLAY)),
        ),
        area,
    );
}

fn render_footer(frame: &mut Frame, area: ratatui::layout::Rect, app: &App) {
    let line = if let Some(pending) = &app.pending_close {
        Line::from(vec![
            Span::styled(
                format!(
                    " Close '{}' and terminate {} shell(s)?  ",
                    pending.name, pending.shell_count
                ),
                Style::new().fg(YELLOW).add_modifier(Modifier::BOLD),
            ),
            Span::styled("y", Style::new().fg(RED)),
            Span::styled(" confirm  ", Style::new().fg(SUBTEXT)),
            Span::styled("n/esc", Style::new().fg(GREEN)),
            Span::styled(" cancel", Style::new().fg(SUBTEXT)),
        ])
    } else if let Mode::Rename { input, .. } = &app.mode {
        Line::from(vec![
            Span::styled(" New shell name: ", Style::new().fg(YELLOW)),
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
                    " add workspace  "
                } else {
                    " add shell  "
                },
                Style::new().fg(SUBTEXT),
            ),
        ];
        if app.focus == Focus::Terminals {
            spans.extend([
                Span::styled("e", Style::new().fg(YELLOW)),
                Span::styled(" rename  ", Style::new().fg(SUBTEXT)),
            ]);
        }
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
            Span::styled(" close workspace  ", Style::new().fg(SUBTEXT)),
            Span::styled("q", Style::new().fg(RED)),
            Span::styled(" quit", Style::new().fg(SUBTEXT)),
        ]);
        Line::from(spans)
    };
    frame.render_widget(Paragraph::new(line), area);
}

fn status_span(status: &str) -> Span<'_> {
    Span::styled(status_label(status), Style::new().fg(status_color(status)))
}

fn status_label(status: &str) -> &str {
    if status == "unknown" { "-" } else { status }
}

fn status_color(status: &str) -> Color {
    match status {
        "working" => YELLOW,
        "blocked" => RED,
        "idle" => GREEN,
        "unknown" | "-" => SUBTEXT,
        _ => TEAL,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn app() -> App {
        App::new(vec![workspace("w1", "boomux")], directory_context())
    }

    fn directory_context() -> DirectoryContext {
        DirectoryContext {
            launch_directory: "/tmp".into(),
            recent_directories: Vec::new(),
        }
    }

    fn workspace(id: &str, name: &str) -> WorkspaceView {
        WorkspaceView {
            id: id.into(),
            name: name.into(),
            status: "working".into(),
            directory: "/tmp/boomux".into(),
            terminals: vec![TerminalView {
                id: "term_1".into(),
                pane_id: "w1:p1".into(),
                name: "agent".into(),
                kind: "opencode".into(),
                status: "working".into(),
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
    fn add_creates_a_workspace_from_workspace_focus() {
        let mut app = app();
        let mut created = None;

        assert!(!app.request_add(&mut |_| Ok(String::new())));
        assert!(matches!(app.mode, Mode::PickDirectory(_)));
        let changed = handle_mode_key(
            &mut app,
            KeyCode::Enter,
            &mut |directory| {
                created = Some(directory.to_owned());
                Ok("Created workspace".into())
            },
            &mut |_, _| Ok(String::new()),
        );

        assert!(changed);
        assert_eq!(created.as_deref(), Some(Path::new("/tmp")));
        assert!(matches!(app.mode, Mode::Normal));
    }

    #[test]
    fn created_workspace_is_available_as_a_recent_directory() {
        let mut app = app();

        app.create_workspace(Path::new("/"), &mut |_| Ok("Created workspace".into()));
        assert!(!app.request_add(&mut |_| Ok(String::new())));

        let Mode::PickDirectory(picker) = &app.mode else {
            panic!("expected directory picker");
        };
        assert!(
            picker
                .entries
                .iter()
                .any(|entry| entry.label == "recent" && entry.path == Path::new("/"))
        );
    }

    #[test]
    fn directory_picker_browses_children_and_parent_lazily() {
        let mut picker = DirectoryPicker::new(&directory_context(), None);

        picker.browse_parent();
        assert_eq!(picker.browsing.as_deref(), Some(Path::new("/")));

        let mut picker = DirectoryPicker::new(&directory_context(), None);
        picker.browse_selected();
        assert_eq!(picker.browsing.as_deref(), Some(Path::new("/tmp")));
        assert_eq!(picker.selected_path(), Some(Path::new("/tmp")));

        picker.browse_parent();
        assert_eq!(picker.browsing.as_deref(), Some(Path::new("/")));
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
        assert_eq!(
            app.directory_context.recent_directories.first(),
            Some(&PathBuf::from("/tmp/boomux"))
        );
    }

    #[test]
    fn rename_mode_dispatches_the_selected_pane_and_name() {
        let mut app = app();
        let mut renamed = None;
        app.toggle_focus();
        app.request_rename();

        for character in ['a', 'p', 'i'] {
            handle_mode_key(
                &mut app,
                KeyCode::Char(character),
                &mut |_| Ok(String::new()),
                &mut |_, _| Ok(String::new()),
            );
        }
        let changed = handle_mode_key(
            &mut app,
            KeyCode::Enter,
            &mut |_| Ok(String::new()),
            &mut |pane_id, name| {
                renamed = Some((pane_id.to_owned(), name.to_owned()));
                Ok("Renamed shell".into())
            },
        );

        assert!(changed);
        assert_eq!(renamed, Some(("w1:p1".into(), "api".into())));
        assert!(matches!(app.mode, Mode::Normal));
    }

    #[test]
    fn rename_requires_terminal_focus() {
        let mut app = app();

        app.request_rename();

        assert!(matches!(app.mode, Mode::Normal));
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
        assert_eq!(
            app.directory_context.recent_directories.first(),
            Some(&PathBuf::from("/tmp/boomux"))
        );
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
        assert_eq!(pending.id, "w1");
        assert_eq!(pending.shell_count, 1);

        app.cancel_close();
        assert!(app.pending_close.is_none());
        app.request_close();
        app.confirm_close(&mut |workspace_id| {
            closed = Some(workspace_id.to_owned());
            Ok("Closed workspace".into())
        });

        assert_eq!(closed.as_deref(), Some("w1"));
        assert!(app.pending_close.is_none());
        let message = app.message.expect("close message");
        assert_eq!(message.text, "Closed workspace");
        assert!(!message.error);
    }

    #[test]
    fn refresh_preserves_the_selected_workspace() {
        let mut app = App::new(
            vec![workspace("w1", "one"), workspace("w2", "two")],
            directory_context(),
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
            directory_context(),
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
    }
}

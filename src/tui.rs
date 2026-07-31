use std::io;
use std::time::{Duration, Instant};

use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Cell, Paragraph, Row, Table, TableState};

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
    pub(crate) status: String,
    pub(crate) directory: String,
    pub(crate) terminals: Vec<TerminalView>,
}

pub(crate) struct TerminalView {
    pub(crate) id: String,
    pub(crate) kind: String,
    pub(crate) status: String,
    pub(crate) directory: String,
}

struct App {
    workspaces: Vec<WorkspaceView>,
    workspace_state: TableState,
    message: Option<Message>,
    pending_close: Option<PendingClose>,
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

impl App {
    fn new(workspaces: Vec<WorkspaceView>) -> Self {
        let mut workspace_state = TableState::default();
        if !workspaces.is_empty() {
            workspace_state.select(Some(0));
        }
        Self {
            workspaces,
            workspace_state,
            message: None,
            pending_close: None,
        }
    }

    fn selected_index(&self) -> Option<usize> {
        self.workspace_state.selected()
    }

    fn selected(&self) -> Option<&WorkspaceView> {
        self.selected_index()
            .and_then(|index| self.workspaces.get(index))
    }

    fn next(&mut self) {
        if self.workspaces.is_empty() {
            return;
        }
        let next = self
            .selected_index()
            .map_or(0, |index| (index + 1) % self.workspaces.len());
        self.workspace_state.select(Some(next));
    }

    fn previous(&mut self) {
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
    }

    fn restore_selected<F>(&mut self, on_restore: &mut F)
    where
        F: FnMut(&str) -> Result<String, String>,
    {
        let Some(workspace_id) = self.selected().map(|workspace| workspace.id.clone()) else {
            return;
        };
        self.message = Some(match on_restore(&workspace_id) {
            Ok(text) => Message { text, error: false },
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
        let previous_index = self.selected_index().unwrap_or(0);
        let selected_index = selected_id
            .and_then(|id| workspaces.iter().position(|workspace| workspace.id == id))
            .or_else(|| (!workspaces.is_empty()).then(|| previous_index.min(workspaces.len() - 1)));

        self.workspaces = workspaces;
        self.workspace_state.select(selected_index);
    }
}

pub(crate) fn run<R, C, F>(
    workspaces: Vec<WorkspaceView>,
    on_restore: R,
    on_close: C,
    on_refresh: F,
) -> io::Result<()>
where
    R: FnMut(&str) -> Result<String, String>,
    C: FnMut(&str) -> Result<String, String>,
    F: FnMut() -> Result<Vec<WorkspaceView>, String>,
{
    let mut terminal = ratatui::init();
    let result = run_loop(
        &mut terminal,
        App::new(workspaces),
        on_restore,
        on_close,
        on_refresh,
    );
    ratatui::restore();
    result
}

fn run_loop<R, C, F>(
    terminal: &mut ratatui::DefaultTerminal,
    mut app: App,
    mut on_restore: R,
    mut on_close: C,
    mut on_refresh: F,
) -> io::Result<()>
where
    R: FnMut(&str) -> Result<String, String>,
    C: FnMut(&str) -> Result<String, String>,
    F: FnMut() -> Result<Vec<WorkspaceView>, String>,
{
    let mut last_refresh = Instant::now();
    loop {
        if last_refresh.elapsed() >= REFRESH_INTERVAL {
            app.refresh(&mut on_refresh);
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
                    app.confirm_close(&mut on_close);
                    app.refresh(&mut on_refresh);
                    last_refresh = Instant::now();
                }
                KeyCode::Char('n') | KeyCode::Esc => app.cancel_close(),
                _ => {}
            }
            continue;
        }

        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => return Ok(()),
            KeyCode::Down | KeyCode::Char('j') => app.next(),
            KeyCode::Up | KeyCode::Char('k') => app.previous(),
            KeyCode::Enter => {
                app.restore_selected(&mut on_restore);
                app.refresh(&mut on_refresh);
                last_refresh = Instant::now();
            }
            KeyCode::Char('r') => {
                app.refresh(&mut on_refresh);
                last_refresh = Instant::now();
            }
            KeyCode::Char('x') => app.request_close(),
            _ => {}
        }
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
        Span::styled(
            "    Enter restores selected workspace",
            Style::new().fg(SUBTEXT),
        ),
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
            .border_style(Style::new().fg(OVERLAY)),
    )
    .row_highlight_style(
        Style::new()
            .fg(TEXT)
            .add_modifier(Modifier::BOLD | Modifier::REVERSED),
    )
    .highlight_symbol("> ");

    frame.render_stateful_widget(table, area, &mut app.workspace_state);
}

fn render_terminals(frame: &mut Frame, area: ratatui::layout::Rect, app: &App) {
    let header = Row::new(["#", "TYPE", "STATUS", "DIRECTORY", "TERMINAL ID"])
        .style(Style::new().fg(BLUE).add_modifier(Modifier::BOLD));
    let rows = app
        .selected()
        .into_iter()
        .flat_map(|workspace| workspace.terminals.iter())
        .enumerate()
        .map(|(index, terminal)| {
            Row::new(vec![
                Cell::from((index + 1).to_string()),
                Cell::from(terminal.kind.as_str()),
                Cell::from(status_span(&terminal.status)),
                Cell::from(terminal.directory.as_str()),
                Cell::from(terminal.id.as_str()),
            ])
        });
    let title = app.selected().map_or_else(
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
    .block(
        Block::bordered()
            .title(title)
            .border_style(Style::new().fg(OVERLAY)),
    );
    frame.render_widget(table, area);
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
    } else if let Some(message) = &app.message {
        Line::from(Span::styled(
            format!(" {}", message.text),
            Style::new().fg(if message.error { RED } else { GREEN }),
        ))
    } else {
        Line::from(vec![
            Span::styled(" j/k", Style::new().fg(TEAL)),
            Span::styled(" or arrows navigate  ", Style::new().fg(SUBTEXT)),
            Span::styled("enter", Style::new().fg(GREEN)),
            Span::styled(" restore workspace  ", Style::new().fg(SUBTEXT)),
            Span::styled("r", Style::new().fg(BLUE)),
            Span::styled(" refresh  ", Style::new().fg(SUBTEXT)),
            Span::styled("x", Style::new().fg(RED)),
            Span::styled(" close  ", Style::new().fg(SUBTEXT)),
            Span::styled("q", Style::new().fg(RED)),
            Span::styled(" quit", Style::new().fg(SUBTEXT)),
        ])
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
        App::new(vec![workspace("w1", "boomux")])
    }

    fn workspace(id: &str, name: &str) -> WorkspaceView {
        WorkspaceView {
            id: id.into(),
            name: name.into(),
            status: "working".into(),
            directory: "/tmp/boomux".into(),
            terminals: vec![TerminalView {
                id: "term_1".into(),
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
        let mut app = App::new(vec![workspace("w1", "one"), workspace("w2", "two")]);
        app.next();

        app.replace_workspaces(vec![workspace("w2", "two"), workspace("w3", "three")]);

        assert_eq!(
            app.selected().map(|workspace| workspace.id.as_str()),
            Some("w2")
        );
    }

    #[test]
    fn refresh_removes_stale_workspaces_and_repairs_selection() {
        let mut app = App::new(vec![workspace("w1", "one"), workspace("w2", "two")]);
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

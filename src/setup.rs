use std::env;
use std::error::Error;
use std::fs::{self, OpenOptions};
use std::io::{self, IsTerminal, Read, Write};
use std::ops::Range;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::{
    layout::{Constraint, Layout},
    style::{Modifier, Style},
    widgets::{List, ListItem, ListState, Paragraph, Wrap},
};
use serde::Deserialize;
use uuid::Uuid;

use boomux::client;
use boomux::protocol::{Request, Response, ShellSnapshot, ShellStatus};

use crate::integration_management::{self, AssetState, HostState, IntegrationId};

struct HarnessChoice {
    integration: IntegrationId,
    host: HostState,
    asset: AssetState,
    path: Option<String>,
    selected: bool,
}

impl HarnessChoice {
    fn enabled(&self) -> bool {
        self.host == HostState::Available && self.asset != AssetState::Unavailable
    }

    fn description(&self) -> &'static str {
        match (self.host, self.asset) {
            (HostState::Missing, _) => "host not found (unavailable)",
            (HostState::ProbeFailed | HostState::NotChecked, _) => {
                "host not verified (unavailable)"
            }
            (_, AssetState::Unavailable) => "integration inspection failed (unavailable)",
            (_, AssetState::Current) => "integration current; keep existing files",
            (_, AssetState::Missing) => "install Boomux integration",
            (_, AssetState::Modified) => "REPLACE MODIFIED integration files",
        }
    }
}

struct HarnessChecklist {
    choices: Vec<HarnessChoice>,
    cursor: ListState,
    confirmed: bool,
}

impl HarnessChecklist {
    fn new(statuses: &[(IntegrationId, integration_management::IntegrationStatus)]) -> Self {
        let choices = statuses
            .iter()
            .map(|(integration, status)| HarnessChoice {
                integration: *integration,
                host: status.host.state,
                asset: status.asset.state,
                path: status.asset.path.clone(),
                selected: status.host.state == HostState::Available
                    && status.asset.state == AssetState::Current,
            })
            .collect::<Vec<_>>();
        let focused = choices
            .iter()
            .position(HarnessChoice::enabled)
            .or_else(|| (!choices.is_empty()).then_some(0));
        Self {
            choices,
            cursor: ListState::default().with_selected(focused),
            confirmed: false,
        }
    }

    fn key(&mut self, key: KeyEvent) -> io::Result<bool> {
        if key.kind == KeyEventKind::Release {
            return Ok(false);
        }
        if key.code == KeyCode::Esc
            || (key.modifiers.contains(KeyModifiers::CONTROL)
                && matches!(key.code, KeyCode::Char('c' | 'd')))
        {
            self.confirmed = false;
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "harness selection cancelled; no integrations changed",
            ));
        }
        if !key.modifiers.is_empty() {
            return Ok(false);
        }
        match key.code {
            KeyCode::Enter => {
                self.confirmed = true;
                return Ok(true);
            }
            KeyCode::Up | KeyCode::Down if !self.choices.is_empty() => {
                let count = self.choices.len();
                let current = self.cursor.selected().unwrap_or(0);
                self.cursor.select(Some(if key.code == KeyCode::Up {
                    (current + count - 1) % count
                } else {
                    (current + 1) % count
                }));
            }
            KeyCode::Char(' ') if key.kind == KeyEventKind::Press => {
                if let Some(choice) = self
                    .cursor
                    .selected()
                    .and_then(|index| self.choices.get_mut(index))
                    && choice.enabled()
                {
                    choice.selected = !choice.selected;
                }
            }
            _ => {}
        }
        Ok(false)
    }

    fn install_with<T>(&self, index: usize, install: impl FnOnce(bool) -> T) -> Option<T> {
        let choice = self.choices.get(index)?;
        (self.confirmed
            && choice.selected
            && choice.enabled()
            && matches!(choice.asset, AssetState::Missing | AssetState::Modified))
        .then(|| install(choice.asset == AssetState::Modified))
    }

    fn render(&mut self, frame: &mut ratatui::Frame) -> bool {
        let area = frame.area();
        if area.width < 40 || area.height < 10 {
            frame.render_widget(
                Paragraph::new("Resize to at least 40x10. Esc cancels.").wrap(Wrap { trim: false }),
                area,
            );
            return false;
        }
        let compact = area.height < 14;
        let [heading, list, details, controls] = Layout::vertical([
            Constraint::Length(if compact { 1 } else { 3 }),
            Constraint::Min(1),
            Constraint::Length(if compact { 2 } else { 4 }),
            Constraint::Length(3),
        ])
        .areas(area);
        frame.render_widget(
            Paragraph::new(
                "AI harness integrations\nSelect Boomux integrations, not harness applications.",
            ),
            heading,
        );
        let items = self
            .choices
            .iter()
            .map(|choice| {
                ListItem::new(format!(
                    "[{}] {} - {}",
                    if choice.selected {
                        "x"
                    } else if choice.enabled() {
                        " "
                    } else {
                        "-"
                    },
                    choice.integration.spec().display_name,
                    choice.description()
                ))
                .style(if choice.enabled() {
                    Style::default()
                } else {
                    Style::default().add_modifier(Modifier::DIM)
                })
            })
            .collect::<Vec<_>>();
        frame.render_stateful_widget(
            List::new(items)
                .highlight_symbol("> ")
                .highlight_style(Style::default().add_modifier(Modifier::REVERSED)),
            list,
            &mut self.cursor,
        );
        if let Some(choice) = self
            .cursor
            .selected()
            .and_then(|index| self.choices.get(index))
        {
            frame.render_widget(
                Paragraph::new(format!(
                    "{}\nPath: {}\nUnchecked integrations are never removed.",
                    choice.description(),
                    choice.path.as_deref().unwrap_or("unavailable")
                ))
                .wrap(Wrap { trim: false }),
                details,
            );
        }
        frame.render_widget(Paragraph::new("Up/Down move | Space toggle\nEnter apply | Esc cancel\nChecked replacements overwrite files.")
            .wrap(Wrap { trim: false }), controls);
        true
    }

    fn choose(&mut self) -> io::Result<()> {
        let mut terminal = ratatui::try_init().inspect_err(|_| {
            let _ = ratatui::try_restore();
        })?;
        let result: io::Result<()> = (|| {
            loop {
                let mut usable = false;
                terminal.draw(|frame| usable = self.render(frame))?;
                if let Event::Key(key) = event::read()?
                    && (usable
                        || key.code == KeyCode::Esc
                        || key.modifiers.contains(KeyModifiers::CONTROL))
                    && self.key(key)?
                {
                    return Ok(());
                }
            }
        })();
        // Restore canonical input and the original screen before any install or Y/n prompt.
        let restored = ratatui::try_restore();
        result?;
        restored
    }
}

const COMMAND_TIMEOUT: Duration = Duration::from_secs(30);
const PLUGIN_INSTALL_TIMEOUT: Duration = Duration::from_secs(120);
const MAX_COMMAND_OUTPUT: u64 = 1024 * 1024;
const MAX_BINDINGS_BYTES: u64 = 1024 * 1024;
const OMARCHY_PLUGIN_ID: &str = "io.github.gardnmi.boomux";
const BINDINGS_BEGIN: &str = "-- BEGIN BOOMUX MANAGED KEYBINDINGS";
const BINDINGS_END: &str = "-- END BOOMUX MANAGED KEYBINDINGS";
// Historical profile retained to recognize and safely remove existing owned bindings.
const MANAGED_BINDINGS: &str = r#"-- BEGIN BOOMUX MANAGED KEYBINDINGS
hl.unbind("SUPER + B")
hl.unbind("SUPER + A")
hl.unbind("SUPER + LEFT")
hl.unbind("SUPER + RIGHT")
hl.unbind("SUPER + UP")
hl.unbind("SUPER + DOWN")
hl.unbind("SUPER + TAB")
hl.unbind("SUPER + SHIFT + TAB")
hl.unbind("SUPER + RETURN")
hl.unbind("SUPER + O")
hl.unbind("SUPER + ALT + B")
hl.unbind("SUPER + ALT + R")
hl.unbind("SUPER + CTRL + RETURN")
hl.unbind("SUPER + CTRL + W")

o.bind("SUPER + B", "Toggle Boomux panel", "omarchy-shell io.github.gardnmi.boomux toggle", { release = true })
o.bind("SUPER + A", "Focus Boomux panel", "omarchy-shell io.github.gardnmi.boomux focus", { release = true })

local function boomux_focus_away(direction)
  return function()
    hl.exec_cmd("omarchy-shell io.github.gardnmi.boomux releaseFocus")
    hl.dispatch(hl.dsp.focus({ direction = direction }))
  end
end

o.bind("SUPER + LEFT", "Focus on left window", boomux_focus_away("l"))
o.bind("SUPER + RIGHT", "Focus on right window", boomux_focus_away("r"))
o.bind("SUPER + UP", "Focus on above window", boomux_focus_away("u"))
o.bind("SUPER + DOWN", "Focus on below window", boomux_focus_away("d"))
o.bind("SUPER + TAB", "Next Boomux workspace", "boomux desktop next")
o.bind("SUPER + SHIFT + TAB", "Previous Boomux workspace", "boomux desktop previous")
o.bind("SUPER + RETURN", "Contextual terminal", "boomux desktop terminal")
o.bind("SUPER + O", "Pop window contextually", "boomux desktop pop")
o.bind("SUPER + ALT + B", "Return terminal to Boomux workspace", "boomux desktop return")
o.bind("SUPER + ALT + R", "Gather Boomux workspace terminals", "boomux desktop gather")
o.bind("SUPER + CTRL + RETURN", "New Boomux Shell", "boomux shell create --open")
o.bind("SUPER + CTRL + W", "Permanently close focused Boomux terminal", "boomux close --focused")
-- END BOOMUX MANAGED KEYBINDINGS
"#;

#[derive(Debug, Deserialize)]
struct OmarchyPlugin {
    id: String,
    enabled: bool,
}

struct CommandOutput {
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

pub(crate) enum OmarchyPluginUpdateOutcome {
    NotInstalled,
    Updated,
    UpdatedAndReloaded,
    UpdateOutcomeUnknown(io::Error),
    UpdatedButReloadStateUnknown(io::Error),
    UpdatedButReloadFailed(io::Error),
}

struct BindingsPlan {
    path: PathBuf,
    baseline: Option<Vec<u8>>,
    content: Vec<u8>,
    mode: u32,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SetupOutcomeKind {
    Current,
    Changed,
    Skipped,
    Warning,
    Failed,
}

struct SetupOutcome {
    kind: SetupOutcomeKind,
    label: String,
    message: String,
    recovery: Option<String>,
}

impl SetupOutcome {
    fn new(kind: SetupOutcomeKind, label: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            kind,
            label: label.into(),
            message: message.into(),
            recovery: None,
        }
    }

    fn failed(
        label: impl Into<String>,
        error: impl std::fmt::Display,
        recovery: impl Into<String>,
    ) -> Self {
        Self {
            kind: SetupOutcomeKind::Failed,
            label: label.into(),
            message: error.to_string(),
            recovery: Some(recovery.into()),
        }
    }
}

#[derive(Clone, Copy)]
enum ApplyOutcome {
    Current,
    Changed,
    Skipped,
}

fn colors_enabled() -> bool {
    io::stdout().is_terminal()
        && env::var_os("NO_COLOR").is_none()
        && env::var("TERM").is_ok_and(|term| term != "dumb")
}

fn paint(code: &str, text: impl AsRef<str>) -> String {
    let text = text.as_ref();
    if colors_enabled() {
        format!("\x1b[{code}m{text}\x1b[0m")
    } else {
        text.to_owned()
    }
}

fn section(title: &str) {
    println!("\n{}", paint("1;36", format!("-- {title} --")));
}

fn status(marker: &str, color: &str, label: &str, value: impl AsRef<str>) {
    println!(
        "  {} {:<20} {}",
        paint(color, format!("[{marker}]")),
        label,
        value.as_ref()
    );
}

fn detail(value: impl AsRef<str>) {
    println!("       {}", paint("2", value));
}

pub(crate) fn desktop_setup() -> Result<(), Box<dyn Error>> {
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        return Err(io::Error::other("Desktop setup requires an interactive terminal").into());
    }
    let shell_id = env::var("BOOMUX_SHELL_ID")?;
    let run_id = env::var("BOOMUX_RUN_ID")?;
    let executable = env::current_exe()?;
    let client = client::connect()?;
    desktop_setup_close_request(
        &client.get_shell(&shell_id)?,
        &shell_id,
        &run_id,
        &executable,
    )?;

    let result = guided_setup();
    finish_desktop_setup(
        result,
        &mut io::stdin().lock(),
        &mut io::stdout().lock(),
        || {
            let shell = client.get_shell(&shell_id)?;
            let request = desktop_setup_close_request(&shell, &shell_id, &run_id, &executable)?;
            match client.request(request)? {
                Response::Ok => Ok(()),
                response => Err(io::Error::other(format!(
                    "unexpected cleanup response: {response:?}"
                ))
                .into()),
            }
        },
    )
}

fn desktop_setup_close_request(
    shell: &ShellSnapshot,
    shell_id: &str,
    run_id: &str,
    executable: &Path,
) -> io::Result<Request> {
    if shell.id != shell_id
        || run_id.is_empty()
        || shell.status != ShellStatus::Running
        || !shell
            .run
            .as_ref()
            .is_some_and(|run| run.id == run_id && run.ended_at_ms.is_none())
        || shell.command.len() != 2
        || Path::new(&shell.command[0]) != executable
        || shell.command[1] != "__desktop-setup"
    {
        return Err(io::Error::other(
            "refusing cleanup outside the exact dedicated Desktop setup Shell/run",
        ));
    }
    Ok(Request::GuardedCloseShell {
        shell_id: shell.id.clone(),
        expected_revision: shell.revision,
    })
}

fn finish_desktop_setup(
    result: Result<(), Box<dyn Error>>,
    input: &mut impl io::BufRead,
    output: &mut impl Write,
    mut close: impl FnMut() -> Result<(), Box<dyn Error>>,
) -> Result<(), Box<dyn Error>> {
    if let Err(error) = &result {
        writeln!(output, "\nSetup completed with failures: {error}")?;
    }
    loop {
        write!(output, "\nExit and remove this setup Shell? [Y/n] ")?;
        output.flush()?;
        let mut answer = String::new();
        io::BufRead::read_line(&mut (&mut *input).take(64), &mut answer)?;
        if !answer.ends_with('\n') {
            return Err(io::Error::other(
                "confirmation input closed or exceeded its limit; setup Shell retained",
            )
            .into());
        }
        match answer.trim().to_ascii_lowercase().as_str() {
            "" | "y" | "yes" => {
                writeln!(output, "Removing this setup Shell...")?;
                output.flush()?;
                close()?;
                return result;
            }
            "n" | "no" => writeln!(
                output,
                "Setup Shell retained. Review the output; answer Y when ready to remove it."
            )?,
            _ => writeln!(output, "Please answer Y or n.")?,
        }
    }
}

pub(crate) fn guided_setup() -> Result<(), Box<dyn Error>> {
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Boomux setup requires an interactive terminal; use integration status and integration install for automation",
        )
        .into());
    }

    println!("{}", paint("1;35", "BOOMUX"));
    println!("{}", paint("1", "Set up this machine"));
    println!(
        "{}",
        paint(
            "2",
            format!(
                "v{}  |  config {}",
                env!("CARGO_PKG_VERSION"),
                crate::config::active_path()?.display()
            )
        )
    );
    println!(
        "{}",
        paint(
            "2",
            "Inspect first, confirm every change, then verify the finished setup."
        )
    );

    section("Inspecting System");
    let daemon_was_running = client::connect().is_ok();
    status(
        if daemon_was_running { "ok" } else { "--" },
        if daemon_was_running { "32" } else { "2" },
        "Daemon",
        if daemon_was_running {
            "already running"
        } else {
            "will start during verification"
        },
    );

    let environment = integration_management::Environment::from_process();
    let statuses = IntegrationId::all()
        .map(|integration| {
            (
                integration,
                integration_management::inspect(integration, &environment, None),
            )
        })
        .collect::<Vec<_>>();
    let detected = statuses
        .iter()
        .filter(|(_, status)| status.host.state != HostState::Missing)
        .count();

    let skill_before = if detected > 0 {
        let skill = required_home()?.join(".agents/skills/boomux/SKILL.md");
        Some(integration_management::regular_file_matches(
            &skill,
            crate::BOOMUX_SKILL,
        )?)
    } else {
        None
    };
    section("Setup Plan");
    for (integration_id, integration) in &statuses {
        if integration.host.state == HostState::Missing {
            continue;
        }
        let (marker, color, plan) = match integration.asset.state {
            AssetState::Current => ("ok", "32", "integration current"),
            AssetState::Missing => ("->", "36", "install integration"),
            AssetState::Modified => ("!!", "33", "replace only if checked in the checklist"),
            AssetState::Unavailable => ("xx", "31", "inspection must be repaired"),
        };
        status(marker, color, integration.display_name, plan);
        if let Some(path) = integration.asset.path.as_deref() {
            detail(format!("path: {path}"));
        } else if matches!(
            integration.asset.state,
            AssetState::Missing | AssetState::Modified
        ) {
            let force = integration.asset.state == AssetState::Modified;
            if let Ok(plan) =
                integration_management::plan_install(*integration_id, &environment, force)
            {
                detail(format!("path: {}", plan.path));
            }
        }
    }
    if detected == 0 {
        status(
            "--",
            "2",
            "Agent lifecycle",
            "no supported harnesses detected",
        );
    } else if let Some(skill) = skill_before {
        let (marker, color, plan) = match skill {
            Some(true) => ("ok", "32", "Agent Skill current"),
            Some(false) => ("!!", "33", "replace Agent Skill only with confirmation"),
            None => ("->", "36", "offer Agent Skill installation"),
        };
        status(marker, color, "Agent Skill", plan);
        detail(format!(
            "path: {}",
            required_home()?
                .join(".agents/skills/boomux/SKILL.md")
                .display()
        ));
    }
    status(
        "->",
        "36",
        "Verification",
        "start or confirm the local daemon",
    );
    section("Agent Harnesses");
    if detected == 0 {
        status("--", "2", "Harnesses", "none found on PATH");
    }
    let mut checklist = HarnessChecklist::new(&statuses);
    if detected > 0 {
        checklist.choose()?;
    }
    let mut outcomes = Vec::new();
    let mut changed_harnesses = Vec::new();
    for (index, (integration, integration_status)) in statuses.into_iter().enumerate() {
        if integration_status.host.state == HostState::Missing {
            continue;
        }
        let (marker, color) = match integration_status.asset.state {
            AssetState::Current => ("ok", "32"),
            AssetState::Missing => ("->", "36"),
            AssetState::Modified => ("!!", "33"),
            AssetState::Unavailable => ("xx", "31"),
        };
        status(
            marker,
            color,
            integration_status.display_name,
            format!(
                "host {} | integration {}",
                integration_status.host.state.as_str().replace('_', " "),
                integration_status.asset.state.as_str()
            ),
        );
        if let Some(version) = integration_status.host.version.as_deref() {
            detail(format!(
                "version {version} ({})",
                integration_status.host.compatibility
            ));
        }
        if integration_status.asset.state == AssetState::Current {
            outcomes.push(SetupOutcome::new(
                SetupOutcomeKind::Current,
                integration_status.display_name,
                "integration current",
            ));
            continue;
        }
        if integration_status.asset.state == AssetState::Unavailable {
            outcomes.push(SetupOutcome::failed(
                integration_status.display_name,
                format!(
                    "integration could not be inspected: {}",
                    integration_status
                        .asset
                        .error
                        .as_deref()
                        .unwrap_or("unknown error")
                ),
                format!(
                    "`boomux integration status {} --json`",
                    integration.spec().key
                ),
            ));
            continue;
        }
        if let Some(result) = checklist.install_with(index, |force| {
            integration_management::plan_install(integration, &environment, force)?;
            integration_management::install(integration, &environment, force)
        }) {
            match result {
                Ok(result) => {
                    let changed =
                        result.result != integration_management::InstallOutcome::Unchanged;
                    let message = if changed {
                        "integration installed"
                    } else {
                        "integration current"
                    };
                    status("ok", "32", integration_status.display_name, message);
                    detail(format!("path: {}", result.path));
                    outcomes.push(SetupOutcome::new(
                        if changed {
                            SetupOutcomeKind::Changed
                        } else {
                            SetupOutcomeKind::Current
                        },
                        integration_status.display_name,
                        message,
                    ));
                    if result.restart_required {
                        changed_harnesses.push(integration_status.display_name);
                        detail(integration.installation().reload_message);
                    }
                }
                Err(error) => outcomes.push(SetupOutcome::failed(
                    integration_status.display_name,
                    error,
                    format!("`boomux integration install {}`", integration.spec().key),
                )),
            }
        } else {
            status("--", "2", integration_status.display_name, "skipped");
            outcomes.push(SetupOutcome::new(
                SetupOutcomeKind::Skipped,
                integration_status.display_name,
                "integration skipped",
            ));
        }
    }

    if detected > 0 {
        match setup_agent_skill() {
            Ok(outcome) => outcomes.push(apply_outcome(
                "Agent Skill",
                outcome,
                "current",
                "installed",
                "skipped",
            )),
            Err(error) => {
                outcomes.push(SetupOutcome::failed("Agent Skill", error, "`boomux setup`"))
            }
        }
    }

    section("Verification");
    let daemon_ready = match client::connect_or_start() {
        Ok(_) => {
            status(
                "ok",
                "32",
                "Daemon",
                if daemon_was_running {
                    "running"
                } else {
                    "started"
                },
            );
            outcomes.push(SetupOutcome::new(
                if daemon_was_running {
                    SetupOutcomeKind::Current
                } else {
                    SetupOutcomeKind::Changed
                },
                "Daemon",
                if daemon_was_running {
                    "running"
                } else {
                    "started"
                },
            ));
            true
        }
        Err(error) => {
            status("xx", "31", "Daemon", "could not be started");
            outcomes.push(SetupOutcome::failed(
                "Daemon",
                error,
                "run `boomux doctor`, then `boomux setup`",
            ));
            false
        }
    };

    let selected_integrations = checklist
        .choices
        .iter()
        .filter(|choice| choice.selected && choice.enabled())
        .map(|choice| choice.integration)
        .collect::<Vec<_>>();
    let recommended_ready = render_setup_receipt(
        &environment,
        &selected_integrations,
        &changed_harnesses,
        daemon_ready,
        &mut outcomes,
    );

    let failures = outcomes
        .iter()
        .filter(|outcome| outcome.kind == SetupOutcomeKind::Failed)
        .count();
    if failures == 0 {
        section("Next");
        detail("Open Boomux Desktop from the application menu, or run `boomux` for the dashboard.");
        detail("Run `boomux doctor` at any time to check system health.");
        if !recommended_ready {
            detail("Run `boomux setup` again to finish skipped recommended steps.");
        }
        print!("{}", setup_completion(recommended_ready, false));
        return Ok(());
    }
    for failure in outcomes
        .iter()
        .filter(|outcome| outcome.kind == SetupOutcomeKind::Failed)
    {
        eprintln!(
            "  {} {}: {}",
            paint("31", "[xx]"),
            failure.label,
            failure.message
        );
        if let Some(recovery) = &failure.recovery {
            eprintln!("       Recovery: {recovery}");
        }
    }
    print!("{}", setup_completion(false, true));
    Err(io::Error::other(format!(
        "setup completed with {} failure{}",
        failures,
        if failures == 1 { "" } else { "s" }
    ))
    .into())
}

fn setup_completion(ready: bool, failed: bool) -> &'static str {
    if failed {
        "\nSetup finished with failures. Review the errors and recovery steps above.\n"
    } else if ready {
        "\nSetup completed successfully.\n"
    } else {
        "\nSetup finished; recommended steps remain. Review the receipt above.\n"
    }
}

fn apply_outcome(
    label: impl Into<String>,
    outcome: ApplyOutcome,
    current: impl Into<String>,
    changed: impl Into<String>,
    skipped: impl Into<String>,
) -> SetupOutcome {
    let label = label.into();
    match outcome {
        ApplyOutcome::Current => SetupOutcome::new(SetupOutcomeKind::Current, label, current),
        ApplyOutcome::Changed => SetupOutcome::new(SetupOutcomeKind::Changed, label, changed),
        ApplyOutcome::Skipped => SetupOutcome::new(SetupOutcomeKind::Skipped, label, skipped),
    }
}

fn render_setup_receipt(
    environment: &integration_management::Environment,
    selected_integrations: &[IntegrationId],
    changed_harnesses: &[&str],
    daemon_ready: bool,
    outcomes: &mut Vec<SetupOutcome>,
) -> bool {
    let selected_count = selected_integrations.len();
    let installed_integrations = selected_integrations
        .iter()
        .copied()
        .map(|integration| integration_management::inspect(integration, environment, None))
        .filter(|integration| {
            integration.host.state == HostState::Available
                && integration.asset.state == AssetState::Current
        })
        .count();
    let integrations_ready = selected_count == 0
        || installed_integrations == selected_count && changed_harnesses.is_empty();
    if selected_count == 0 {
        outcomes.push(SetupOutcome::new(
            SetupOutcomeKind::Skipped,
            "Agent lifecycle",
            "no integrations selected",
        ));
    } else if installed_integrations != selected_count
        && changed_harnesses.is_empty()
        && !outcomes
            .iter()
            .any(|outcome| outcome.kind == SetupOutcomeKind::Failed)
    {
        outcomes.push(SetupOutcome::new(
            SetupOutcomeKind::Warning,
            "Agent lifecycle",
            format!("{installed_integrations} of {selected_count} selected integrations verified"),
        ));
    }
    if !changed_harnesses.is_empty() {
        outcomes.push(SetupOutcome::new(
            SetupOutcomeKind::Warning,
            "Harness restart",
            format!("restart to load {}", changed_harnesses.join(", ")),
        ));
    }

    let skill_ready = if selected_count == 0 {
        true
    } else {
        match required_home()
            .map(|home| home.join(".agents/skills/boomux/SKILL.md"))
            .and_then(|path| {
                integration_management::regular_file_matches(&path, crate::BOOMUX_SKILL)
                    .map_err(|error| io::Error::other(error.to_string()))
            }) {
            Ok(Some(true)) => true,
            Ok(_) => false,
            Err(error) => {
                outcomes.push(SetupOutcome::failed(
                    "Agent Skill verification",
                    error,
                    "`boomux setup`",
                ));
                false
            }
        }
    };
    let failed = outcomes
        .iter()
        .any(|outcome| outcome.kind == SetupOutcomeKind::Failed);
    let complete = daemon_ready && !failed && integrations_ready && skill_ready;

    println!(
        "\n{}",
        paint(
            if complete { "1;32" } else { "1;36" },
            if complete {
                "BOOMUX IS READY"
            } else {
                "BOOMUX SETUP RECEIPT"
            }
        )
    );
    for outcome in outcomes.iter() {
        let (marker, color) = match outcome.kind {
            SetupOutcomeKind::Current | SetupOutcomeKind::Changed => ("ok", "32"),
            SetupOutcomeKind::Skipped => ("--", "2"),
            SetupOutcomeKind::Warning => ("!!", "33"),
            SetupOutcomeKind::Failed => ("xx", "31"),
        };
        status(marker, color, &outcome.label, &outcome.message);
        if let Some(recovery) = &outcome.recovery {
            detail(format!("Recovery: {recovery}"));
        }
    }
    status(
        if complete { "ok" } else { "!!" },
        if complete { "32" } else { "33" },
        "Setup",
        if complete {
            "this machine is ready"
        } else if !failed {
            "recommended steps remain"
        } else {
            "completed with failures"
        },
    );

    complete
}

fn setup_agent_skill() -> Result<ApplyOutcome, Box<dyn Error>> {
    let home = required_home()?;
    let path = home.join(".agents/skills/boomux/SKILL.md");
    match integration_management::regular_file_matches(&path, crate::BOOMUX_SKILL)? {
        Some(true) => {
            status("ok", "32", "Agent Skill", "current");
            detail(path.display().to_string());
            crate::migrate_legacy_skill(&home)?;
            Ok(ApplyOutcome::Current)
        }
        state => {
            let modified = state == Some(false);
            let prompt = if modified {
                status("!!", "33", "Agent Skill", "modified");
                detail(format!("Plan: replace {}", path.display()));
                "Replace the modified Boomux Agent Skill?"
            } else {
                status("->", "36", "Agent Skill", "not installed");
                detail(format!("Plan: install {}", path.display()));
                "Install the Boomux Agent Skill?"
            };
            if confirm(prompt)? {
                crate::install_skill(modified)?;
                Ok(ApplyOutcome::Changed)
            } else {
                status("--", "2", "Agent Skill", "skipped");
                Ok(ApplyOutcome::Skipped)
            }
        }
    }
}

fn required_home() -> io::Result<PathBuf> {
    env::var_os("HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "HOME must be absolute"))
}

fn is_executable_file(path: &Path) -> bool {
    fs::metadata(path)
        .is_ok_and(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
}

fn confirm(prompt: &str) -> io::Result<bool> {
    confirm_with_default(prompt, false)
}

fn confirm_with_default(prompt: &str, default_yes: bool) -> io::Result<bool> {
    print!(
        "  {} {} ",
        paint("1;33", prompt),
        paint("2", if default_yes { "[Y/n]" } else { "[y/N]" })
    );
    io::stdout().flush()?;
    read_confirmation(&mut io::stdin().lock(), default_yes)
}

fn read_confirmation(reader: &mut impl io::BufRead, default_yes: bool) -> io::Result<bool> {
    let mut response = String::new();
    if reader.read_line(&mut response)? == 0 {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "confirmation input closed before a choice was made",
        ));
    }
    let response = response.trim().to_ascii_lowercase();
    Ok(if response.is_empty() {
        default_yes
    } else {
        matches!(response.as_str(), "y" | "yes")
    })
}

fn executable_on_path(name: &str) -> Option<PathBuf> {
    env::split_paths(&env::var_os("PATH")?).find_map(|directory| {
        let candidate = directory.join(name);
        is_executable_file(&candidate).then_some(candidate)
    })
}

fn omarchy_plugins(executable: &Path) -> io::Result<Vec<OmarchyPlugin>> {
    let output = run_command(executable, &["plugin", "list", "--json"], COMMAND_TIMEOUT)?;
    serde_json::from_slice(&output.stdout).map_err(|error| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("Omarchy returned invalid plugin inventory: {error}"),
        )
    })
}

pub(crate) fn installed_omarchy_plugin() -> io::Result<Option<PathBuf>> {
    let Some(executable) = executable_on_path("omarchy") else {
        return Ok(None);
    };
    Ok(omarchy_plugins(&executable)?
        .iter()
        .any(|plugin| plugin.id == OMARCHY_PLUGIN_ID)
        .then_some(executable))
}

pub(crate) fn remove_omarchy_plugin(executable: &Path) -> io::Result<bool> {
    if !omarchy_plugins(executable)?
        .iter()
        .any(|plugin| plugin.id == OMARCHY_PLUGIN_ID)
    {
        return Ok(false);
    }
    run_command(
        executable,
        &["plugin", "remove", OMARCHY_PLUGIN_ID, "--yes"],
        COMMAND_TIMEOUT,
    )?;
    Ok(true)
}

pub(crate) fn update_omarchy_plugin(executable: &Path) -> io::Result<OmarchyPluginUpdateOutcome> {
    let Some(_) = omarchy_plugins(executable)?
        .into_iter()
        .find(|plugin| plugin.id == OMARCHY_PLUGIN_ID)
    else {
        return Ok(OmarchyPluginUpdateOutcome::NotInstalled);
    };
    if let Err(error) = run_command(
        executable,
        &["plugin", "update", OMARCHY_PLUGIN_ID, "--yes"],
        PLUGIN_INSTALL_TIMEOUT,
    ) {
        return Ok(OmarchyPluginUpdateOutcome::UpdateOutcomeUnknown(error));
    }
    let plugin = match omarchy_plugins(executable) {
        Ok(plugins) => plugins
            .into_iter()
            .find(|plugin| plugin.id == OMARCHY_PLUGIN_ID),
        Err(error) => {
            return Ok(OmarchyPluginUpdateOutcome::UpdatedButReloadStateUnknown(
                error,
            ));
        }
    };
    if plugin.is_some_and(|plugin| plugin.enabled) {
        return match run_command(executable, &["restart", "shell"], PLUGIN_INSTALL_TIMEOUT) {
            Ok(_) => Ok(OmarchyPluginUpdateOutcome::UpdatedAndReloaded),
            Err(error) => Ok(OmarchyPluginUpdateOutcome::UpdatedButReloadFailed(error)),
        };
    }
    Ok(OmarchyPluginUpdateOutcome::Updated)
}

fn run_command(
    executable: &Path,
    arguments: &[&str],
    timeout: Duration,
) -> io::Result<CommandOutput> {
    let mut child = Command::new(executable)
        .args(arguments)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0)
        .spawn()?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| io::Error::other("command stdout was unavailable"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| io::Error::other("command stderr was unavailable"))?;
    let stdout = spawn_reader(stdout);
    let stderr = spawn_reader(stderr);
    let process_group = i32::try_from(child.id())
        .map_err(|_| io::Error::other("command process ID exceeded i32"))?;
    let deadline = Instant::now() + timeout;
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if Instant::now() >= deadline {
            terminate_process_group(&mut child, process_group);
            return Err(io::Error::new(io::ErrorKind::TimedOut, "command timed out"));
        }
        thread::sleep(Duration::from_millis(10));
    };
    terminate_process_group(&mut child, process_group);
    let stdout = receive_output(stdout)?;
    let stderr = receive_output(stderr)?;
    if !status.success() {
        let detail = String::from_utf8_lossy(&stderr);
        return Err(io::Error::other(format!(
            "{} exited with {status}: {}",
            executable.display(),
            detail.trim()
        )));
    }
    Ok(CommandOutput { stdout, stderr })
}

fn spawn_reader(reader: impl Read + Send + 'static) -> mpsc::Receiver<io::Result<Vec<u8>>> {
    let (sender, receiver) = mpsc::channel();
    thread::spawn(move || {
        let mut output = Vec::new();
        let result = reader
            .take(MAX_COMMAND_OUTPUT + 1)
            .read_to_end(&mut output)
            .map(|_| output);
        let _ = sender.send(result);
    });
    receiver
}

fn receive_output(receiver: mpsc::Receiver<io::Result<Vec<u8>>>) -> io::Result<Vec<u8>> {
    let output = receiver
        .recv_timeout(Duration::from_secs(1))
        .map_err(|_| io::Error::other("command output reader stopped"))??;
    if output.len() > MAX_COMMAND_OUTPUT as usize {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "command output exceeded 1 MiB",
        ));
    }
    Ok(output)
}

fn terminate_process_group(child: &mut std::process::Child, process_group: i32) {
    unsafe {
        libc::kill(-process_group, libc::SIGKILL);
    }
    let _ = child.wait();
}

pub(crate) fn managed_bindings_status() -> io::Result<(PathBuf, Option<bool>)> {
    let path = bindings_directory()?.join("bindings.lua");
    let (baseline, _) = read_owned_bindings(&path)?;
    let Some(baseline) = baseline else {
        return Ok((path, None));
    };
    let range = match managed_block_range(&baseline) {
        Ok(Some(range)) => range,
        Ok(None) => return Ok((path, None)),
        Err(_) => return Ok((path, Some(false))),
    };
    Ok((path, Some(&baseline[range] == MANAGED_BINDINGS.as_bytes())))
}

pub(crate) fn remove_managed_bindings() -> io::Result<bool> {
    let directory = integration_management::ensure_safe_directory(&bindings_directory()?)
        .map_err(|error| io::Error::other(error.to_string()))?;
    let path = directory.join("bindings.lua");
    let (baseline, mode) = read_owned_bindings(&path)?;
    let Some(baseline) = baseline else {
        return Ok(false);
    };
    let Some(range) = managed_block_range(&baseline)? else {
        return Ok(false);
    };
    if &baseline[range.clone()] != MANAGED_BINDINGS.as_bytes() {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "Boomux managed keybindings were modified",
        ));
    }
    let mut content = baseline.clone();
    content.drain(range);
    let plan = BindingsPlan {
        path,
        baseline: Some(baseline),
        content,
        mode,
    };
    commit_bindings(&plan)?;
    Ok(true)
}

fn bindings_directory() -> io::Result<PathBuf> {
    Ok(required_home()?.join(".config/hypr"))
}

fn read_owned_bindings(path: &Path) -> io::Result<(Option<Vec<u8>>, u32)> {
    let file = match OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
    {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok((None, 0o600)),
        Err(error) if error.raw_os_error() == Some(libc::ELOOP) => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("bindings path is a symbolic link: {}", path.display()),
            ));
        }
        Err(error) => return Err(error),
    };
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("bindings path is not a regular file: {}", path.display()),
        ));
    }
    if metadata.uid() != unsafe { libc::geteuid() } {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "bindings file is not owned by the current user: {}",
                path.display()
            ),
        ));
    }
    if metadata.len() > MAX_BINDINGS_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "bindings file exceeds 1 MiB",
        ));
    }
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    file.take(MAX_BINDINGS_BYTES + 1).read_to_end(&mut bytes)?;
    if bytes.len() > MAX_BINDINGS_BYTES as usize {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "bindings file exceeds 1 MiB",
        ));
    }
    Ok((Some(bytes), metadata.permissions().mode() & 0o7777))
}

fn managed_block_range(source: &[u8]) -> io::Result<Option<Range<usize>>> {
    let begins = find_all(source, BINDINGS_BEGIN.as_bytes());
    let ends = find_all(source, BINDINGS_END.as_bytes());
    match (begins.as_slice(), ends.as_slice()) {
        ([], []) => Ok(None),
        ([begin], [end]) if begin < end => {
            let block_end = end + BINDINGS_END.len();
            let block_end = block_end + usize::from(source[block_end..].starts_with(b"\n"));
            Ok(Some(*begin..block_end))
        }
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "bindings file contains incomplete or duplicate Boomux managed markers",
        )),
    }
}

fn find_all(haystack: &[u8], needle: &[u8]) -> Vec<usize> {
    haystack
        .windows(needle.len())
        .enumerate()
        .filter_map(|(index, window)| (window == needle).then_some(index))
        .collect()
}

fn commit_bindings(plan: &BindingsPlan) -> io::Result<()> {
    let expected_directory = plan
        .path
        .parent()
        .ok_or_else(|| io::Error::other("bindings path has no parent"))?;
    let directory = integration_management::ensure_safe_directory(expected_directory)
        .map_err(|error| io::Error::other(error.to_string()))?;
    let (current, _) = read_owned_bindings(&plan.path)?;
    require_bindings_baseline(plan, &current)?;
    let temporary = directory.join(format!(".boomux-bindings-{}.tmp", Uuid::new_v4()));
    let result = (|| {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(plan.mode)
            .open(&temporary)?;
        file.set_permissions(fs::Permissions::from_mode(plan.mode))?;
        file.write_all(&plan.content)?;
        file.sync_all()?;
        let (current, _) = read_owned_bindings(&plan.path)?;
        require_bindings_baseline(plan, &current)?;
        fs::rename(&temporary, &plan.path)?;
        fs::File::open(directory)?.sync_all()?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

fn require_bindings_baseline(plan: &BindingsPlan, current: &Option<Vec<u8>>) -> io::Result<()> {
    if current == &plan.baseline {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!("{} changed after setup inspection", plan.path.display()),
        ))
    }
}

#[cfg(test)]
mod tests {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::symlink;
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::*;

    fn harness_checklist(states: &[(HostState, AssetState)]) -> HarnessChecklist {
        let statuses = IntegrationId::all()
            .zip(states)
            .map(|(id, &(host, asset))| {
                (
                    id,
                    integration_management::IntegrationStatus {
                        name: id.spec().key,
                        display_name: id.spec().display_name,
                        package: id.installation().package,
                        validated_version: id.installation().validated_version,
                        host: integration_management::HostStatus {
                            state: host,
                            executable: None,
                            version: None,
                            compatibility: "test",
                            error: None,
                        },
                        asset: integration_management::AssetStatus {
                            state: asset,
                            path: Some("/config/integration".into()),
                            error: None,
                        },
                        runtime: integration_management::RuntimeStatus {
                            state: integration_management::RuntimeState::NotObservable,
                            running_processes: 0,
                            tracked_processes: 0,
                            untracked_processes: 0,
                        },
                        recommended_action: integration_management::RecommendedAction::None,
                    },
                )
            })
            .collect::<Vec<_>>();
        HarnessChecklist::new(&statuses)
    }

    #[test]
    fn harness_checklist_defaults_and_apply_require_explicit_selection() {
        let mut checklist = harness_checklist(&[
            (HostState::Available, AssetState::Missing),
            (HostState::Available, AssetState::Current),
            (HostState::Available, AssetState::Modified),
            (HostState::Missing, AssetState::Missing),
            (HostState::Available, AssetState::Unavailable),
        ]);
        assert_eq!(
            checklist
                .choices
                .iter()
                .map(|choice| choice.selected)
                .collect::<Vec<_>>(),
            [false, true, false, false, false]
        );
        for key in [
            KeyCode::Up,
            KeyCode::Char(' '),
            KeyCode::Down,
            KeyCode::Char(' '),
            KeyCode::Down,
            KeyCode::Char(' '),
            KeyCode::Down,
            KeyCode::Char(' '),
        ] {
            assert!(
                !checklist
                    .key(KeyEvent::new(key, KeyModifiers::NONE))
                    .unwrap()
            );
        }
        assert!(
            checklist
                .install_with(0, |_| panic!("must wait for Enter"))
                .is_none()
        );
        assert!(
            checklist
                .key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
                .unwrap()
        );
        let mut calls = Vec::new();
        for index in 0..checklist.choices.len() {
            checklist.install_with(index, |force| calls.push((index, force)));
        }
        assert_eq!(calls, [(0, false), (2, true)]);
        assert_eq!(
            checklist
                .install_with(0, |_| Err::<(), _>("install failed"))
                .unwrap()
                .unwrap_err(),
            "install failed"
        );
    }

    #[test]
    fn harness_checklist_enter_alone_does_not_install_or_replace() {
        let mut checklist = harness_checklist(&[
            (HostState::Available, AssetState::Missing),
            (HostState::Available, AssetState::Current),
            (HostState::Available, AssetState::Modified),
        ]);
        checklist
            .key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .unwrap();
        for index in 0..checklist.choices.len() {
            assert!(
                checklist
                    .install_with(index, |_| panic!(
                        "new and replacement installs must be opted into"
                    ))
                    .is_none()
            );
        }
    }

    #[test]
    fn harness_checklist_disabled_rows_and_cancel_never_install() {
        for host in [
            HostState::Missing,
            HostState::ProbeFailed,
            HostState::NotChecked,
        ] {
            let mut checklist = harness_checklist(&[(host, AssetState::Missing)]);
            checklist
                .key(KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE))
                .unwrap();
            checklist
                .key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
                .unwrap();
            assert!(
                checklist
                    .install_with(0, |_| panic!("disabled host"))
                    .is_none()
            );
        }
        for cancel in [
            KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
            KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
            KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL),
        ] {
            let mut checklist = harness_checklist(&[(HostState::Available, AssetState::Missing)]);
            checklist
                .key(KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE))
                .unwrap();
            assert_eq!(
                checklist.key(cancel).unwrap_err().kind(),
                io::ErrorKind::Interrupted
            );
            assert!(
                checklist
                    .install_with(0, |_| panic!("cancelled selection"))
                    .is_none()
            );
        }
        let mut empty = harness_checklist(&[]);
        assert!(
            !empty
                .key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE))
                .unwrap()
        );
        assert!(
            empty
                .key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
                .unwrap()
        );
        assert!(
            empty
                .install_with(0, |_| panic!("empty selection"))
                .is_none()
        );
    }

    #[test]
    fn harness_checklist_ignores_modified_release_and_repeat_toggles() {
        let mut checklist = harness_checklist(&[(HostState::Available, AssetState::Missing)]);
        for key in [
            KeyEvent::new_with_kind(KeyCode::Char(' '), KeyModifiers::NONE, KeyEventKind::Repeat),
            KeyEvent::new_with_kind(KeyCode::Enter, KeyModifiers::NONE, KeyEventKind::Release),
            KeyEvent::new(KeyCode::Enter, KeyModifiers::ALT),
        ] {
            assert!(!checklist.key(key).unwrap());
        }
        assert!(!checklist.choices[0].selected);
        assert!(!checklist.confirmed);
    }

    #[test]
    fn harness_checklist_renders_controls_replacement_consent_and_small_terminal_guidance() {
        for (width, height) in [(80, 24), (40, 10), (20, 5)] {
            let mut checklist = harness_checklist(&[(HostState::Available, AssetState::Modified)]);
            let mut terminal =
                ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, height)).unwrap();
            let mut usable = false;
            terminal
                .draw(|frame| usable = checklist.render(frame))
                .unwrap();
            let text = (0..height)
                .map(|y| {
                    (0..width)
                        .map(|x| terminal.backend().buffer()[(x, y)].symbol())
                        .collect::<String>()
                })
                .collect::<Vec<_>>()
                .join("\n");
            assert_eq!(usable, width >= 40 && height >= 10);
            if usable {
                assert!(text.contains("[ ]"), "{text}");
                assert!(text.contains("REPLACE MODIFIED"), "{text}");
                assert!(text.contains("Space toggle"), "{text}");
                assert!(text.contains("Enter apply"), "{text}");
                assert!(
                    text.contains("Checked replacements overwrite files."),
                    "{text}"
                );
            } else {
                assert!(text.contains("Resize to at least"), "{text}");
            }
        }
        let mut checklist = harness_checklist(&[(HostState::Available, AssetState::Missing); 5]);
        checklist.cursor.select(Some(4));
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(40, 10)).unwrap();
        terminal
            .draw(|frame| {
                assert!(checklist.render(frame));
            })
            .unwrap();
        assert!(
            checklist.cursor.offset() > 0,
            "compact list must scroll to the focused harness"
        );
    }

    struct TestDirectory(PathBuf);

    impl TestDirectory {
        fn new() -> Self {
            let nonce = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let path = env::temp_dir().join(format!("boomux-setup-{}-{nonce}", std::process::id()));
            fs::create_dir(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn bindings_commit_preserves_mode_and_rejects_concurrent_changes() {
        let directory = TestDirectory::new();
        let path = directory.0.join("bindings.lua");
        let baseline = b"-- user\n".to_vec();
        fs::write(&path, &baseline).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o640)).unwrap();
        let content = b"-- remaining user bindings\n".to_vec();
        let plan = BindingsPlan {
            path: path.clone(),
            baseline: Some(baseline),
            content: content.clone(),
            mode: 0o640,
        };
        commit_bindings(&plan).unwrap();
        assert_eq!(fs::read(&path).unwrap(), content);
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o640
        );

        let stale = BindingsPlan {
            baseline: Some(content),
            content: b"replacement".to_vec(),
            ..plan
        };
        fs::write(&path, b"user changed it").unwrap();
        assert_eq!(
            commit_bindings(&stale).unwrap_err().kind(),
            io::ErrorKind::AlreadyExists
        );
        assert_eq!(fs::read(&path).unwrap(), b"user changed it");
    }

    #[test]
    fn bindings_inspection_rejects_symlinks() {
        let directory = TestDirectory::new();
        let target = directory.0.join("target");
        let path = directory.0.join("bindings.lua");
        fs::write(&target, b"user").unwrap();
        symlink(&target, &path).unwrap();
        assert_eq!(
            read_owned_bindings(&path).unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
    }

    #[test]
    fn bindings_inspection_rejects_fifos_without_blocking() {
        let directory = TestDirectory::new();
        let path = directory.0.join("bindings.lua");
        let path_bytes = CString::new(path.as_os_str().as_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(path_bytes.as_ptr(), 0o600) }, 0);
        assert_eq!(
            read_owned_bindings(&path).unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
    }

    #[test]
    fn bounded_command_preserves_exact_arguments() {
        let directory = TestDirectory::new();
        let executable = directory.0.join("probe");
        fs::write(&executable, b"#!/bin/sh\nprintf '%s\\0' \"$@\"\n").unwrap();
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o755)).unwrap();
        let output = run_command(
            &executable,
            &["value with spaces", "$(not-executed)", "semi;colon"],
            Duration::from_secs(1),
        )
        .unwrap();
        assert_eq!(
            output.stdout.split(|byte| *byte == 0).collect::<Vec<_>>(),
            [
                b"value with spaces".as_slice(),
                b"$(not-executed)".as_slice(),
                b"semi;colon".as_slice(),
                b"".as_slice(),
            ]
        );
    }

    #[test]
    fn recommended_confirmation_accepts_enter_but_rejects_eof() {
        assert!(read_confirmation(&mut io::Cursor::new(b"\n"), true).unwrap());
        let error = read_confirmation(&mut io::Cursor::new([]), true).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::UnexpectedEof);
    }

    #[test]
    fn desktop_setup_confirmation_requires_yes_and_keeps_failures_distinct() {
        for answer in ["\n", "y\n", "YES\n", "n\ny\n", "invalid\ny\n"] {
            let mut closed = 0;
            let mut output = Vec::new();
            finish_desktop_setup(Ok(()), &mut io::Cursor::new(answer), &mut output, || {
                closed += 1;
                Ok(())
            })
            .unwrap();
            assert_eq!(closed, 1);
            let output = String::from_utf8(output).unwrap();
            assert!(output.contains("Exit and remove this setup Shell? [Y/n]"));
            if answer.starts_with("n\n") {
                assert!(output.contains("Setup Shell retained."));
            }
        }
        for answer in ["", "y", "n\n", "invalid\n"] {
            let mut output = Vec::new();
            assert!(
                finish_desktop_setup(Ok(()), &mut io::Cursor::new(answer), &mut output, || {
                    panic!("input without confirmation must not remove a Shell")
                })
                .is_err()
            );
        }
        let mut output = Vec::new();
        let error = finish_desktop_setup(
            Err(io::Error::other("integration failed").into()),
            &mut io::Cursor::new("\n"),
            &mut output,
            || Ok(()),
        )
        .unwrap_err();
        assert_eq!(error.to_string(), "integration failed");
        let output = String::from_utf8(output).unwrap();
        assert!(output.contains("Setup completed with failures: integration failed"));
        assert!(!output.contains("successfully"));

        let error =
            finish_desktop_setup(Ok(()), &mut io::Cursor::new("y\n"), &mut Vec::new(), || {
                Err(io::Error::other("Shell revision changed").into())
            })
            .unwrap_err();
        assert_eq!(error.to_string(), "Shell revision changed");
        assert!(
            finish_desktop_setup(
                Ok(()),
                &mut io::Cursor::new(format!("{}\n", "y".repeat(64))),
                &mut Vec::new(),
                || panic!("oversized confirmation must not remove a Shell"),
            )
            .is_err()
        );
    }

    #[test]
    fn desktop_setup_cleanup_rejects_other_commands_runs_and_shells() {
        let mut shell: ShellSnapshot = serde_json::from_value(serde_json::json!({
            "id": "setup-shell", "revision": 7, "workspace_id": "workspace",
            "name": "Set up agents", "cwd": "/tmp", "status": "running",
            "command": ["/bin/boomux", "__desktop-setup"],
            "run": {"id": "setup-run", "generation": 1, "started_at_ms": 1,
                "ended_at_ms": null, "exit_reason": null, "output_revision": 0,
                "environment_has_run_id": true}
        }))
        .unwrap();
        let request = |shell: &ShellSnapshot| {
            desktop_setup_close_request(shell, "setup-shell", "setup-run", Path::new("/bin/boomux"))
        };
        assert_eq!(
            request(&shell).unwrap(),
            Request::GuardedCloseShell {
                shell_id: "setup-shell".into(),
                expected_revision: 7,
            }
        );
        shell.command[1] = "setup".into();
        assert!(request(&shell).is_err());
        shell.command = vec![];
        assert!(request(&shell).is_err());
        shell.command = vec!["/other/boomux".into(), "__desktop-setup".into()];
        assert!(request(&shell).is_err());
        shell.command[0] = "/bin/boomux".into();
        shell.run.as_mut().unwrap().id = "replacement-run".into();
        assert!(request(&shell).is_err());
        shell.run.as_mut().unwrap().id = "setup-run".into();
        shell.id = "unrelated-shell".into();
        assert!(request(&shell).is_err());
        shell.id = "setup-shell".into();
        shell.status = ShellStatus::Exited { code: Some(0) };
        assert!(request(&shell).is_err());
    }

    #[test]
    fn setup_completion_distinguishes_success_remaining_steps_and_failures() {
        for (ready, failed, expected) in [
            (true, false, "Setup completed successfully."),
            (false, false, "Setup finished; recommended steps remain."),
            (false, true, "Setup finished with failures."),
            (true, true, "Setup finished with failures."),
        ] {
            let message = setup_completion(ready, failed);
            assert!(message.contains(expected), "{message}");
            assert_eq!(message.contains("successfully"), ready && !failed);
        }
    }

    #[test]
    fn omarchy_plugin_removal_rechecks_inventory_and_uses_the_exact_id() {
        let directory = TestDirectory::new();
        let executable = directory.0.join("omarchy");
        fs::write(
            &executable,
            b"#!/bin/sh\nmarker=$0.removed\ncase \"$*\" in\n  'plugin list --json')\n    if [ -e \"$marker\" ]; then printf '[]\\n'; else printf '[{\"id\":\"io.github.gardnmi.boomux\",\"enabled\":true}]\\n'; fi ;;\n  'plugin remove io.github.gardnmi.boomux --yes') : > \"$marker\" ;;\n  *) exit 97 ;;\nesac\n",
        )
        .unwrap();
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o755)).unwrap();

        assert!(remove_omarchy_plugin(&executable).unwrap());
        assert!(executable.with_extension("removed").exists());
        assert!(!remove_omarchy_plugin(&executable).unwrap());
    }

    #[test]
    fn omarchy_plugin_update_rechecks_inventory_and_restarts_an_enabled_plugin() {
        let directory = TestDirectory::new();
        let executable = directory.0.join("omarchy");
        fs::write(
            &executable,
            b"#!/bin/sh\nlog=$0.log\nfor argument do printf '<%s>\\n' \"$argument\" >> \"$log\"; done\nprintf '%s\\n' -- >> \"$log\"\ncase \"$*\" in\n  'plugin list --json') printf '[{\"id\":\"io.github.gardnmi.boomux\",\"enabled\":true}]\\n' ;;\n  'plugin update io.github.gardnmi.boomux --yes') ;;\n  'restart shell') ;;\n  *) exit 97 ;;\nesac\n",
        )
        .unwrap();
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o755)).unwrap();

        assert!(matches!(
            update_omarchy_plugin(&executable).unwrap(),
            OmarchyPluginUpdateOutcome::UpdatedAndReloaded
        ));
        assert_eq!(
            fs::read_to_string(executable.with_extension("log")).unwrap(),
            "<plugin>\n<list>\n<--json>\n--\n<plugin>\n<update>\n<io.github.gardnmi.boomux>\n<--yes>\n--\n<plugin>\n<list>\n<--json>\n--\n<restart>\n<shell>\n--\n"
        );
    }

    #[test]
    fn omarchy_plugin_update_skips_absent_plugin() {
        let directory = TestDirectory::new();
        let executable = directory.0.join("omarchy");
        fs::write(
            &executable,
            b"#!/bin/sh\ncase \"$*\" in\n  'plugin list --json') printf '[]\\n' ;;\n  *) exit 97 ;;\nesac\n",
        )
        .unwrap();
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o755)).unwrap();

        assert!(matches!(
            update_omarchy_plugin(&executable).unwrap(),
            OmarchyPluginUpdateOutcome::NotInstalled
        ));
    }

    #[test]
    fn omarchy_plugin_update_does_not_restart_a_disabled_plugin() {
        let directory = TestDirectory::new();
        let executable = directory.0.join("omarchy");
        fs::write(
            &executable,
            b"#!/bin/sh\nlog=$0.log\nprintf '%s\\n' \"$*\" >> \"$log\"\ncase \"$*\" in\n  'plugin list --json') printf '[{\"id\":\"io.github.gardnmi.boomux\",\"enabled\":false}]\\n' ;;\n  'plugin update io.github.gardnmi.boomux --yes') ;;\n  *) exit 97 ;;\nesac\n",
        )
        .unwrap();
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o755)).unwrap();

        assert!(matches!(
            update_omarchy_plugin(&executable).unwrap(),
            OmarchyPluginUpdateOutcome::Updated
        ));
        assert_eq!(
            fs::read_to_string(executable.with_extension("log")).unwrap(),
            "plugin list --json\nplugin update io.github.gardnmi.boomux --yes\nplugin list --json\n"
        );
    }

    #[test]
    fn omarchy_plugin_update_rechecks_enabled_state_after_update() {
        let directory = TestDirectory::new();
        let executable = directory.0.join("omarchy");
        fs::write(
            &executable,
            b"#!/bin/sh\nmarker=$0.updated\ncase \"$*\" in\n  'plugin list --json')\n    if [ -e \"$marker\" ]; then enabled=true; else enabled=false; fi\n    printf '[{\"id\":\"io.github.gardnmi.boomux\",\"enabled\":%s}]\\n' \"$enabled\" ;;\n  'plugin update io.github.gardnmi.boomux --yes') : > \"$marker\" ;;\n  'restart shell') ;;\n  *) exit 97 ;;\nesac\n",
        )
        .unwrap();
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o755)).unwrap();

        assert!(matches!(
            update_omarchy_plugin(&executable).unwrap(),
            OmarchyPluginUpdateOutcome::UpdatedAndReloaded
        ));
    }

    #[test]
    fn omarchy_plugin_update_distinguishes_a_reload_failure() {
        let directory = TestDirectory::new();
        let executable = directory.0.join("omarchy");
        fs::write(
            &executable,
            b"#!/bin/sh\ncase \"$*\" in\n  'plugin list --json') printf '[{\"id\":\"io.github.gardnmi.boomux\",\"enabled\":true}]\\n' ;;\n  'plugin update io.github.gardnmi.boomux --yes') ;;\n  'restart shell') exit 42 ;;\n  *) exit 97 ;;\nesac\n",
        )
        .unwrap();
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o755)).unwrap();

        assert!(matches!(
            update_omarchy_plugin(&executable).unwrap(),
            OmarchyPluginUpdateOutcome::UpdatedButReloadFailed(_)
        ));
    }

    #[test]
    fn omarchy_plugin_update_preserves_an_unknown_command_outcome() {
        let directory = TestDirectory::new();
        let executable = directory.0.join("omarchy");
        fs::write(
            &executable,
            b"#!/bin/sh\ncase \"$*\" in\n  'plugin list --json') printf '[{\"id\":\"io.github.gardnmi.boomux\",\"enabled\":true}]\\n' ;;\n  'plugin update io.github.gardnmi.boomux --yes') exit 42 ;;\n  *) exit 97 ;;\nesac\n",
        )
        .unwrap();
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o755)).unwrap();

        assert!(matches!(
            update_omarchy_plugin(&executable).unwrap(),
            OmarchyPluginUpdateOutcome::UpdateOutcomeUnknown(_)
        ));
    }
}

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

use serde::Deserialize;
use uuid::Uuid;

use boomux::client;

use crate::integration_management::{self, AssetState, HostState, InstallAction, IntegrationId};

const COMMAND_TIMEOUT: Duration = Duration::from_secs(30);
const PLUGIN_INSTALL_TIMEOUT: Duration = Duration::from_secs(120);
const MAX_COMMAND_OUTPUT: u64 = 1024 * 1024;
const MAX_BINDINGS_BYTES: u64 = 1024 * 1024;
const OMARCHY_PLUGIN_ID: &str = "io.github.gardnmi.boomux";
const OMARCHY_PLUGIN_URL: &str = "https://github.com/gardnmi/omarchy-boomux.git";
const BINDINGS_BEGIN: &str = "-- BEGIN BOOMUX MANAGED KEYBINDINGS";
const BINDINGS_END: &str = "-- END BOOMUX MANAGED KEYBINDINGS";
const BINDING_KEYS: &[&str] = &[
    "SUPER + B",
    "SUPER + A",
    "SUPER + LEFT",
    "SUPER + RIGHT",
    "SUPER + UP",
    "SUPER + DOWN",
    "SUPER + TAB",
    "SUPER + SHIFT + TAB",
    "SUPER + RETURN",
    "SUPER + O",
    "SUPER + ALT + B",
    "SUPER + ALT + R",
    "SUPER + CTRL + RETURN",
    "SUPER + CTRL + W",
];
const COMPATIBLE_BINDING_SNIPPETS: &[&[u8]] = &[
    b"omarchy-shell io.github.gardnmi.boomux toggle",
    b"omarchy-shell io.github.gardnmi.boomux focus",
    b"omarchy-shell io.github.gardnmi.boomux releaseFocus",
    b"o.bind(\"SUPER + LEFT\", \"Focus on left window\"",
    b"o.bind(\"SUPER + RIGHT\", \"Focus on right window\"",
    b"o.bind(\"SUPER + UP\", \"Focus on above window\"",
    b"o.bind(\"SUPER + DOWN\", \"Focus on below window\"",
    b"\"boomux desktop next\"",
    b"\"boomux desktop previous\"",
    b"\"boomux desktop terminal\"",
    b"\"boomux desktop pop\"",
    b"\"boomux desktop return\"",
    b"\"boomux desktop gather\"",
    b"\"boomux shell create --open\"",
    b"\"boomux close --focused\"",
];

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
    changed: bool,
    modified: bool,
    compatible_unmanaged: bool,
}

struct SetupReadiness {
    complete: bool,
    plugin_enabled: bool,
    keybindings_ready: bool,
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

#[derive(Default)]
struct DesktopSetupOutcomes {
    plugin: Option<ApplyOutcome>,
    workspace_layer: Option<ApplyOutcome>,
    keybindings: Option<ApplyOutcome>,
}

struct DesktopSetupPlan {
    workspace_enabled: bool,
    bindings_ready: bool,
    bindings_path: PathBuf,
    conflicts: Vec<String>,
}

#[derive(Debug)]
struct DesktopPartialFailure {
    message: String,
    committed_message: &'static str,
    failure_label: &'static str,
    recovery: &'static str,
}

impl std::fmt::Display for DesktopPartialFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl Error for DesktopPartialFailure {}

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
    let Some(terminal_resolver) = executable_on_path("xdg-terminal-exec") else {
        status("xx", "31", "Terminal resolver", "not found on PATH");
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "xdg-terminal-exec is required; install it and rerun `boomux setup`",
        )
        .into());
    };
    status(
        "ok",
        "32",
        "Terminal resolver",
        terminal_resolver.display().to_string(),
    );
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

    let omarchy_plan = if let Some(omarchy) = executable_on_path("omarchy") {
        let version = run_command(&omarchy, &["version"], COMMAND_TIMEOUT)?;
        ensure_omarchy_can_resolve_boomux()?;
        status(
            "ok",
            "32",
            "Desktop",
            String::from_utf8_lossy(&version.stdout).trim(),
        );
        Some(omarchy_plugins(&omarchy)?)
    } else {
        status("--", "2", "Desktop", "Omarchy not detected");
        None
    };

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

    for (integration, integration_status) in &statuses {
        if integration_status.host.state != HostState::Missing
            && matches!(
                integration_status.asset.state,
                AssetState::Missing | AssetState::Modified
            )
        {
            integration_management::plan_install(
                *integration,
                &environment,
                integration_status.asset.state == AssetState::Modified,
            )?;
        }
    }
    let skill_before = if detected > 0 {
        let skill = required_home()?.join(".agents/skills/boomux/SKILL.md");
        Some(integration_management::regular_file_matches(
            &skill,
            crate::BOOMUX_SKILL,
        )?)
    } else {
        None
    };
    let (desktop_plan, desktop_plan_error) = if omarchy_plan.is_some() {
        let omarchy = executable_on_path("omarchy")
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "omarchy disappeared"))?;
        match inspect_omarchy_desktop_plan(&omarchy) {
            Ok(plan) => (Some(plan), None),
            Err(error) => (None, Some(error.to_string())),
        }
    } else {
        (None, None)
    };

    section("Setup Plan");
    if let Some(plugins) = omarchy_plan.as_deref() {
        let plugin = plugins.iter().find(|plugin| plugin.id == OMARCHY_PLUGIN_ID);
        status(
            if plugin.is_some_and(|plugin| plugin.enabled) {
                "ok"
            } else {
                "->"
            },
            if plugin.is_some_and(|plugin| plugin.enabled) {
                "32"
            } else {
                "36"
            },
            "Companion pane",
            match plugin {
                Some(plugin) if plugin.enabled => "plugin enabled",
                Some(_) => "enable recommended plugin",
                None => "install and enable recommended plugin",
            },
        );
        detail(format!("source: {OMARCHY_PLUGIN_URL}"));
        let workspace_enabled = desktop_plan
            .as_ref()
            .is_some_and(|plan| plan.workspace_enabled);
        status(
            if workspace_enabled { "ok" } else { "->" },
            if workspace_enabled { "32" } else { "36" },
            "Workspace layer",
            if workspace_enabled {
                "enabled"
            } else {
                "enable recommended Hyprland presentation"
            },
        );
        detail(format!(
            "config: {}",
            crate::config::active_path()?.display()
        ));
        let bindings_ready = desktop_plan
            .as_ref()
            .is_some_and(|plan| plan.bindings_ready);
        status(
            if bindings_ready { "ok" } else { "--" },
            if bindings_ready { "32" } else { "2" },
            "Keybindings",
            if bindings_ready {
                "current compatible profile"
            } else {
                "optional; install only with confirmation"
            },
        );
        if let Some(desktop_plan) = desktop_plan.as_ref() {
            detail(format!("path: {}", desktop_plan.bindings_path.display()));
            for conflict in &desktop_plan.conflicts {
                detail(format!("conflict: {conflict}"));
            }
        } else {
            detail(format!(
                "desktop plan unavailable: {}",
                desktop_plan_error.as_deref().unwrap_or("unknown error")
            ));
        }
    } else {
        status("--", "2", "Companion pane", "Omarchy not detected");
    }
    for (integration_id, integration) in &statuses {
        if integration.host.state == HostState::Missing {
            continue;
        }
        let (marker, color, plan) = match integration.asset.state {
            AssetState::Current => ("ok", "32", "integration current"),
            AssetState::Missing => ("->", "36", "install integration"),
            AssetState::Modified => ("!!", "33", "replace only with confirmation"),
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
            let plan = integration_management::plan_install(*integration_id, &environment, force)?;
            detail(format!("path: {}", plan.path));
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
    detail("Recommended Omarchy choices default to yes.");
    detail("Modified assets, replacements, and optional keybindings default to no.");

    let skip_desktop = if let Some(error) = desktop_plan_error.as_deref() {
        status("!!", "33", "Desktop blocker", error);
        detail("No desktop change will be attempted unless inspection succeeds.");
        if confirm_recommended("Skip unavailable optional desktop setup and continue?")? {
            true
        } else {
            return Err(io::Error::other(format!(
                "desktop setup inspection failed: {error}; fix it and rerun `boomux setup`"
            ))
            .into());
        }
    } else {
        false
    };

    section("Agent Harnesses");
    if detected == 0 {
        status("--", "2", "Harnesses", "none found on PATH");
    }
    let mut outcomes = Vec::new();
    let mut changed_harnesses = Vec::new();
    for (integration, integration_status) in statuses {
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
        let force = integration_status.asset.state == AssetState::Modified;
        let plan = match integration_management::plan_install(integration, &environment, force) {
            Ok(plan) => plan,
            Err(error) => {
                outcomes.push(SetupOutcome::failed(
                    integration_status.display_name,
                    error,
                    format!(
                        "`boomux integration status {} --json`",
                        integration.spec().key
                    ),
                ));
                continue;
            }
        };
        let (action, prompt) = match plan.action {
            InstallAction::Install => (
                "Install",
                format!(
                    "Install the {} integration?",
                    integration_status.display_name
                ),
            ),
            InstallAction::Replace => (
                "Replace modified",
                format!(
                    "Replace the modified {} integration?",
                    integration_status.display_name
                ),
            ),
            InstallAction::Unchanged => continue,
        };
        detail(format!("Plan: {action} asset at {}", plan.path));
        if confirm(&prompt)? {
            match integration_management::install(integration, &environment, force) {
                Ok(result) => {
                    status(
                        "ok",
                        "32",
                        integration_status.display_name,
                        "integration installed",
                    );
                    detail(format!("path: {}", result.path));
                    outcomes.push(SetupOutcome::new(
                        SetupOutcomeKind::Changed,
                        integration_status.display_name,
                        "integration installed",
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

    let desktop_result = if skip_desktop {
        if omarchy_plan.as_deref().is_some_and(|plugins| {
            plugins
                .iter()
                .any(|plugin| plugin.id == OMARCHY_PLUGIN_ID && plugin.enabled)
        }) {
            outcomes.push(SetupOutcome::new(
                SetupOutcomeKind::Warning,
                "Omarchy plugin",
                "enabled; later desktop verification skipped",
            ));
        }
        outcomes.push(SetupOutcome::new(
            SetupOutcomeKind::Skipped,
            "Omarchy desktop",
            "skipped because read-only inspection failed",
        ));
        Ok(DesktopSetupOutcomes::default())
    } else {
        setup_omarchy()
    };
    match desktop_result {
        Ok(desktop) => {
            if let Some(outcome) = desktop.plugin {
                outcomes.push(apply_outcome(
                    "Omarchy plugin",
                    outcome,
                    "enabled",
                    "installed and loaded",
                    "skipped",
                ));
            }
            if let Some(outcome) = desktop.workspace_layer {
                outcomes.push(apply_outcome(
                    "Workspace layer",
                    outcome,
                    "enabled",
                    "enabled",
                    "skipped",
                ));
            }
            if let Some(outcome) = desktop.keybindings {
                outcomes.push(apply_outcome(
                    "Keybindings",
                    outcome,
                    "current compatible profile",
                    "installed",
                    "skipped",
                ));
            }
        }
        Err(error) => {
            if let Some(partial) = error.downcast_ref::<DesktopPartialFailure>() {
                outcomes.push(SetupOutcome::new(
                    SetupOutcomeKind::Changed,
                    "Omarchy plugin",
                    partial.committed_message,
                ));
                outcomes.push(SetupOutcome::failed(
                    partial.failure_label,
                    partial,
                    partial.recovery,
                ));
            } else {
                outcomes.push(SetupOutcome::failed(
                    "Omarchy desktop",
                    error,
                    "run `omarchy plugin list --json`, then `boomux setup`",
                ));
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

    let recommended_ready = render_setup_receipt(
        &environment,
        detected,
        &changed_harnesses,
        daemon_ready,
        skip_desktop,
        &mut outcomes,
    );

    let failures = outcomes
        .iter()
        .filter(|outcome| outcome.kind == SetupOutcomeKind::Failed)
        .count();
    if failures == 0 {
        section("Next");
        if recommended_ready.plugin_enabled {
            if recommended_ready.keybindings_ready {
                detail("Press Super+B to open Boomux, then press + to create a Workspace.");
            } else {
                detail("Open Boomux from the Omarchy bar, then press + to create a Workspace.");
            }
            detail("If the pane is not visible yet, run `omarchy restart shell` once.");
        } else {
            detail("Run `boomux` to open the dashboard and create your first Workspace.");
        }
        detail("Run `boomux doctor` at any time to check system health.");
        if !recommended_ready.complete {
            detail("Run `boomux setup` again to finish skipped recommended steps.");
        }
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
    Err(io::Error::other(format!(
        "setup completed with {} failure{}",
        failures,
        if failures == 1 { "" } else { "s" }
    ))
    .into())
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
    detected: usize,
    changed_harnesses: &[&str],
    daemon_ready: bool,
    skip_desktop: bool,
    outcomes: &mut Vec<SetupOutcome>,
) -> SetupReadiness {
    let installed_integrations = IntegrationId::all()
        .map(|integration| integration_management::inspect(integration, environment, None))
        .filter(|integration| {
            integration.host.state == HostState::Available
                && integration.asset.state == AssetState::Current
        })
        .count();
    let integrations_ready =
        detected == 0 || installed_integrations == detected && changed_harnesses.is_empty();
    if detected == 0 {
        outcomes.push(SetupOutcome::new(
            SetupOutcomeKind::Skipped,
            "Agent lifecycle",
            "no harnesses detected",
        ));
    } else if installed_integrations != detected
        && changed_harnesses.is_empty()
        && !outcomes
            .iter()
            .any(|outcome| outcome.kind == SetupOutcomeKind::Failed)
    {
        outcomes.push(SetupOutcome::new(
            SetupOutcomeKind::Warning,
            "Agent lifecycle",
            format!("{installed_integrations} of {detected} integrations verified"),
        ));
    }
    if !changed_harnesses.is_empty() {
        outcomes.push(SetupOutcome::new(
            SetupOutcomeKind::Warning,
            "Harness restart",
            format!("restart to load {}", changed_harnesses.join(", ")),
        ));
    }

    let skill_ready = if detected == 0 {
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
    let mut plugin_enabled = false;
    let mut workspace_layer_enabled = false;
    let mut keybindings_ready = false;
    let omarchy = executable_on_path("omarchy");
    if skip_desktop {
        plugin_enabled = outcomes.iter().any(|outcome| {
            outcome.label == "Omarchy plugin"
                && matches!(
                    outcome.kind,
                    SetupOutcomeKind::Current
                        | SetupOutcomeKind::Changed
                        | SetupOutcomeKind::Warning
                )
        });
    } else if let Some(omarchy) = omarchy.as_deref() {
        plugin_enabled = match omarchy_plugins(omarchy) {
            Ok(plugins) => plugins
                .iter()
                .any(|plugin| plugin.id == OMARCHY_PLUGIN_ID && plugin.enabled),
            Err(error) => {
                outcomes.push(SetupOutcome::failed(
                    "Omarchy plugin verification",
                    error,
                    "run `omarchy plugin list --json`, then `boomux setup`",
                ));
                false
            }
        };
        let desktop_failed = outcomes.iter().any(|outcome| {
            outcome.kind == SetupOutcomeKind::Failed && outcome.label == "Omarchy desktop"
        });
        if plugin_enabled
            && !outcomes
                .iter()
                .any(|outcome| outcome.label == "Omarchy plugin")
        {
            outcomes.push(SetupOutcome::new(
                if desktop_failed {
                    SetupOutcomeKind::Warning
                } else {
                    SetupOutcomeKind::Current
                },
                "Omarchy plugin",
                if desktop_failed {
                    "enabled before a later desktop step failed"
                } else {
                    "installed and enabled"
                },
            ));
        }
        if plugin_enabled {
            workspace_layer_enabled = match crate::config::load() {
                Ok(config) => {
                    config.desktop.workspace_layer
                        == crate::config::DesktopWorkspaceLayer::HyprlandSpecial
                }
                Err(error) => {
                    outcomes.push(SetupOutcome::failed(
                        "Workspace layer verification",
                        error,
                        "run `boomux config validate`, then `boomux setup`",
                    ));
                    false
                }
            };
            if workspace_layer_enabled
                && !outcomes
                    .iter()
                    .any(|outcome| outcome.label == "Workspace layer")
            {
                outcomes.push(SetupOutcome::new(
                    if desktop_failed {
                        SetupOutcomeKind::Warning
                    } else {
                        SetupOutcomeKind::Current
                    },
                    "Workspace layer",
                    if desktop_failed {
                        "enabled before a later desktop step failed"
                    } else {
                        "enabled"
                    },
                ));
            }
            let keybindings = match bindings_plan() {
                Ok(plan) => Some(plan),
                Err(error) => {
                    outcomes.push(SetupOutcome::failed(
                        "Keybindings verification",
                        error,
                        "`boomux setup`",
                    ));
                    None
                }
            };
            keybindings_ready = keybindings.as_ref().is_some_and(|plan| !plan.changed);
            let compatible_unmanaged = keybindings
                .as_ref()
                .is_some_and(|plan| plan.compatible_unmanaged);
            if keybindings_ready
                && !outcomes
                    .iter()
                    .any(|outcome| outcome.label == "Keybindings")
            {
                outcomes.push(SetupOutcome::new(
                    if desktop_failed {
                        SetupOutcomeKind::Warning
                    } else {
                        SetupOutcomeKind::Current
                    },
                    "Keybindings",
                    if desktop_failed {
                        "compatible state preserved after a later failure"
                    } else if compatible_unmanaged {
                        "compatible user-managed profile"
                    } else {
                        "managed profile ready"
                    },
                ));
            }
        }
    } else {
        outcomes.push(SetupOutcome::new(
            SetupOutcomeKind::Skipped,
            "Omarchy desktop",
            "not detected",
        ));
    }

    let desktop_ready =
        skip_desktop || omarchy.is_none() || plugin_enabled && workspace_layer_enabled;
    let failed = outcomes
        .iter()
        .any(|outcome| outcome.kind == SetupOutcomeKind::Failed);
    let complete = daemon_ready && !failed && integrations_ready && skill_ready && desktop_ready;

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

    SetupReadiness {
        complete,
        plugin_enabled,
        keybindings_ready,
    }
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

fn setup_omarchy() -> Result<DesktopSetupOutcomes, Box<dyn Error>> {
    section("Desktop Integration");
    let Some(omarchy) = executable_on_path("omarchy") else {
        status("--", "2", "Omarchy", "not detected");
        detail("Desktop plugin and keybindings were skipped.");
        return Ok(DesktopSetupOutcomes::default());
    };
    let version = run_command(&omarchy, &["version"], COMMAND_TIMEOUT)?;
    status(
        "ok",
        "32",
        "Omarchy",
        String::from_utf8_lossy(&version.stdout).trim(),
    );
    ensure_omarchy_can_resolve_boomux()?;
    let hyprland_active =
        env::var_os("HYPRLAND_INSTANCE_SIGNATURE").is_some_and(|value| !value.is_empty());
    status(
        if hyprland_active { "ok" } else { "--" },
        if hyprland_active { "32" } else { "2" },
        "Hyprland session",
        if hyprland_active {
            "active"
        } else {
            "not active"
        },
    );

    let plugins = omarchy_plugins(&omarchy)?;
    let (plugin_enabled, plugin_changed, plugin_outcome) = match plugins
        .iter()
        .find(|plugin| plugin.id == OMARCHY_PLUGIN_ID)
    {
        Some(plugin) if plugin.enabled => {
            status(
                "ok",
                "32",
                "omarchy-boomux",
                "recommended core experience installed and enabled",
            );
            detail("Enabled in the Omarchy plugin inventory; no change is needed.");
            (true, false, ApplyOutcome::Current)
        }
        Some(_) => {
            status(
                "!!",
                "33",
                "omarchy-boomux",
                "recommended core experience is disabled",
            );
            detail("Enabling the plugin also restarts Omarchy Shell so it is loaded.");
            if confirm_recommended("Enable and load the recommended omarchy-boomux plugin?")? {
                preflight_omarchy_desktop(&omarchy)?;
                run_command(
                    &omarchy,
                    &["plugin", "enable", OMARCHY_PLUGIN_ID],
                    COMMAND_TIMEOUT,
                )?;
                status("ok", "32", "omarchy-boomux", "enabled");
                (true, true, ApplyOutcome::Changed)
            } else {
                (false, false, ApplyOutcome::Skipped)
            }
        }
        None => {
            status(
                "->",
                "36",
                "omarchy-boomux",
                "recommended core Omarchy experience",
            );
            detail("Keep Workspaces, Shells, Agents, and Nodes available in a persistent pane.");
            detail("Plugins run as unsandboxed code inside the Omarchy shell.");
            detail(format!("Source: {OMARCHY_PLUGIN_URL}"));
            detail("Installation also restarts Omarchy Shell so the plugin is loaded.");
            if confirm_recommended(
                "Install, enable, and load the recommended omarchy-boomux plugin?",
            )? {
                preflight_omarchy_desktop(&omarchy)?;
                run_command(
                    &omarchy,
                    &["plugin", "add", OMARCHY_PLUGIN_URL, "--enable", "--yes"],
                    PLUGIN_INSTALL_TIMEOUT,
                )?;
                status("ok", "32", "omarchy-boomux", "installed and enabled");
                (true, true, ApplyOutcome::Changed)
            } else {
                (false, false, ApplyOutcome::Skipped)
            }
        }
    };
    if plugin_changed {
        run_command(&omarchy, &["restart", "shell"], PLUGIN_INSTALL_TIMEOUT).map_err(|error| {
            Box::new(DesktopPartialFailure {
                message: format!("plugin is enabled, but Omarchy Shell did not restart: {error}"),
                committed_message: "enabled before shell reload failed",
                failure_label: "Omarchy Shell reload",
                recovery: "`omarchy restart shell`",
            }) as Box<dyn Error>
        })?;
        status("ok", "32", "Omarchy Shell", "restarted with plugin loaded");
        let plugins = omarchy_plugins(&omarchy).map_err(|error| {
            Box::new(DesktopPartialFailure {
                message: format!(
                    "plugin was enabled and the shell restarted, but inventory verification failed: {error}"
                ),
                committed_message: "enabled and shell reloaded before verification failed",
                failure_label: "Omarchy plugin verification",
                recovery: "run `omarchy plugin list --json`, then `omarchy restart shell` if the plugin is not visible",
            }) as Box<dyn Error>
        })?;
        if !plugins
            .iter()
            .any(|plugin| plugin.id == OMARCHY_PLUGIN_ID && plugin.enabled)
        {
            return Err(Box::new(DesktopPartialFailure {
                message: "Omarchy did not report the companion plugin as enabled after the change"
                    .into(),
                committed_message: "enabled and shell reloaded before verification failed",
                failure_label: "Omarchy plugin verification",
                recovery: "run `omarchy plugin list --json`, then `omarchy restart shell`",
            }));
        }
    }
    if !plugin_enabled {
        status("--", "2", "Keybindings", "skipped; plugin is not enabled");
        return Ok(DesktopSetupOutcomes {
            plugin: Some(plugin_outcome),
            workspace_layer: None,
            keybindings: None,
        });
    }

    let workspace_layer = setup_hyprland_workspace_layer()?;

    let plan = bindings_plan()?;
    if !plan.changed {
        if plan.compatible_unmanaged {
            status("ok", "32", "Keybindings", "compatible user-managed profile");
            detail("Existing bindings work and remain user-owned unless you reinstall them.");

            let inventory = run_command(
                &omarchy,
                &["menu", "keybindings", "--print"],
                COMMAND_TIMEOUT,
            )?;
            let inventory = String::from_utf8_lossy(&inventory.stdout);
            let conflicts = binding_conflicts(&inventory);
            if !conflicts.is_empty() {
                status(
                    "!!",
                    "33",
                    "Reinstall impact",
                    "these existing bindings would be overridden",
                );
                for conflict in conflicts {
                    detail(conflict);
                }
            }
        } else {
            status("ok", "32", "Keybindings", "current managed profile");
        }

        detail("No changes are needed.");
        return Ok(DesktopSetupOutcomes {
            plugin: Some(plugin_outcome),
            workspace_layer: Some(workspace_layer),
            keybindings: Some(ApplyOutcome::Current),
        });
    } else {
        let inventory = run_command(
            &omarchy,
            &["menu", "keybindings", "--print"],
            COMMAND_TIMEOUT,
        )?;
        let inventory = String::from_utf8_lossy(&inventory.stdout);
        let conflicts = binding_conflicts(&inventory);
        if plan.modified {
            status("!!", "33", "Keybindings", "managed profile modified");
        } else if conflicts.is_empty() {
            status("->", "36", "Keybindings", "full profile not installed");
        } else {
            status("!!", "33", "Keybindings", "conflicts require replacement");
            for conflict in conflicts {
                detail(conflict);
            }
        }
        let prompt = if plan.modified {
            "Replace the modified Boomux keybinding profile?"
        } else {
            "Install the full Boomux keybinding profile?"
        };
        if !confirm(prompt)? {
            status("--", "2", "Keybindings", "skipped");
            return Ok(DesktopSetupOutcomes {
                plugin: Some(plugin_outcome),
                workspace_layer: Some(workspace_layer),
                keybindings: Some(ApplyOutcome::Skipped),
            });
        }
    }
    commit_bindings(&plan)?;
    status("ok", "32", "Keybindings", "installed");
    detail(plan.path.display().to_string());

    let hyprctl = hyprland_active
        .then(|| executable_on_path("hyprctl"))
        .flatten();
    let validation = if hyprland_active {
        hyprctl.as_deref().map_or_else(
            || {
                Err(io::Error::new(
                    io::ErrorKind::NotFound,
                    "HYPRLAND_INSTANCE_SIGNATURE is set but hyprctl is unavailable",
                ))
            },
            validate_hyprland_config,
        )
    } else {
        detail("Hyprland is not active; bindings will load at the next session.");
        Ok(())
    };
    if let Err(error) = validation {
        rollback_bindings(&plan).map_err(|rollback| {
            io::Error::other(format!(
                "{error}; additionally failed to restore prior keybindings: {rollback}"
            ))
        })?;
        if let Some(hyprctl) = hyprctl.as_deref() {
            validate_hyprland_config(hyprctl).map_err(|rollback| {
                io::Error::other(format!(
                    "{error}; prior keybindings were restored but Hyprland failed to reload them: {rollback}"
                ))
            })?;
        }
        return Err(error.into());
    }
    if hyprland_active {
        status("ok", "32", "Hyprland config", "reloaded without errors");
    }
    Ok(DesktopSetupOutcomes {
        plugin: Some(plugin_outcome),
        workspace_layer: Some(workspace_layer),
        keybindings: Some(ApplyOutcome::Changed),
    })
}

fn setup_hyprland_workspace_layer() -> Result<ApplyOutcome, Box<dyn Error>> {
    if crate::config::load()?.desktop.workspace_layer
        == crate::config::DesktopWorkspaceLayer::HyprlandSpecial
    {
        status("ok", "32", "Workspace layer", "enabled");
        return Ok(ApplyOutcome::Current);
    }

    status(
        "->",
        "36",
        "Workspace layer",
        "recommended for the core Omarchy experience",
    );
    detail("Present coordinated Boomux Workspaces as named Hyprland special Workspaces.");
    if !confirm_recommended("Enable the recommended Hyprland Workspace layer?")? {
        status("--", "2", "Workspace layer", "kept disabled");
        return Ok(ApplyOutcome::Skipped);
    }

    let path = crate::config::enable_hyprland_workspace_layer()?;
    status("ok", "32", "Workspace layer", "enabled");
    detail(format!("config: {}", path.display()));
    Ok(ApplyOutcome::Changed)
}

fn preflight_omarchy_desktop(omarchy: &Path) -> Result<(), Box<dyn Error>> {
    inspect_omarchy_desktop_plan(omarchy).map(|_| ())
}

fn inspect_omarchy_desktop_plan(omarchy: &Path) -> Result<DesktopSetupPlan, Box<dyn Error>> {
    let workspace_enabled = crate::config::load()?.desktop.workspace_layer
        == crate::config::DesktopWorkspaceLayer::HyprlandSpecial;
    let bindings = bindings_plan()?;
    let conflicts = if bindings.changed || bindings.compatible_unmanaged {
        let inventory = run_command(
            omarchy,
            &["menu", "keybindings", "--print"],
            COMMAND_TIMEOUT,
        )?;
        binding_conflicts(&String::from_utf8_lossy(&inventory.stdout))
            .into_iter()
            .map(str::to_owned)
            .collect()
    } else {
        Vec::new()
    };
    if bindings.changed && env::var_os("HYPRLAND_INSTANCE_SIGNATURE").is_some() {
        executable_on_path("hyprctl").ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                "HYPRLAND_INSTANCE_SIGNATURE is set but hyprctl is unavailable",
            )
        })?;
    }
    Ok(DesktopSetupPlan {
        workspace_enabled,
        bindings_ready: !bindings.changed,
        bindings_path: bindings.path,
        conflicts,
    })
}

fn ensure_omarchy_can_resolve_boomux() -> io::Result<()> {
    let home = required_home()?;
    let cargo_home = env::var_os("CARGO_HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".cargo"));
    let executable = env::current_exe()?;
    let desktop_executable = home.join(".local/bin/boomux");
    if executable.starts_with(cargo_home.join("bin")) && !is_executable_file(&desktop_executable) {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!(
                "{} is available to this shell but Omarchy may not include {} in its graphical PATH; install the official Boomux release at {} before configuring desktop integration",
                executable.display(),
                cargo_home.join("bin").display(),
                desktop_executable.display()
            ),
        ));
    }
    Ok(())
}

fn is_executable_file(path: &Path) -> bool {
    fs::metadata(path)
        .is_ok_and(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
}

fn validate_hyprland_config(hyprctl: &Path) -> io::Result<()> {
    run_command(hyprctl, &["reload"], COMMAND_TIMEOUT)?;
    let errors = run_command(hyprctl, &["configerrors"], COMMAND_TIMEOUT)?;
    let errors = String::from_utf8_lossy(&errors.stdout);
    if errors.trim().is_empty() {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("Hyprland reported configuration errors:\n{}", errors.trim()),
        ))
    }
}

fn confirm(prompt: &str) -> io::Result<bool> {
    confirm_with_default(prompt, false)
}

fn confirm_recommended(prompt: &str) -> io::Result<bool> {
    confirm_with_default(prompt, true)
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

fn bindings_plan() -> Result<BindingsPlan, Box<dyn Error>> {
    let path = bindings_directory()?.join("bindings.lua");
    let (baseline, mode) = read_owned_bindings(&path)?;
    let source = baseline.as_deref().unwrap_or_default();
    let range = managed_block_range(source)?;
    let compatible_unmanaged = range.is_none() && compatible_unmanaged_profile(source);
    let modified = range
        .as_ref()
        .is_some_and(|range| &source[range.clone()] != MANAGED_BINDINGS.as_bytes());
    let content = render_bindings(source)?;
    if content.len() > MAX_BINDINGS_BYTES as usize {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "bindings file would exceed 1 MiB after setup",
        )
        .into());
    }
    let changed = !compatible_unmanaged && baseline.as_deref() != Some(content.as_slice());
    Ok(BindingsPlan {
        path,
        baseline,
        content,
        mode,
        changed,
        modified,
        compatible_unmanaged,
    })
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
        changed: true,
        modified: false,
        compatible_unmanaged: false,
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

fn render_bindings(source: &[u8]) -> io::Result<Vec<u8>> {
    match managed_block_range(source)? {
        None => {
            let mut content = source.to_vec();
            if !content.is_empty() && !content.ends_with(b"\n") {
                content.push(b'\n');
            }
            if !content.is_empty() {
                content.push(b'\n');
            }
            content.extend_from_slice(MANAGED_BINDINGS.as_bytes());
            Ok(content)
        }
        Some(range) => {
            let mut content = Vec::with_capacity(source.len() + MANAGED_BINDINGS.len());
            content.extend_from_slice(&source[..range.start]);
            content.extend_from_slice(MANAGED_BINDINGS.as_bytes());
            content.extend_from_slice(&source[range.end..]);
            Ok(content)
        }
    }
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

fn compatible_unmanaged_profile(source: &[u8]) -> bool {
    COMPATIBLE_BINDING_SNIPPETS.iter().all(|snippet| {
        source
            .windows(snippet.len())
            .any(|window| window == *snippet)
    })
}

fn binding_conflicts(inventory: &str) -> Vec<&str> {
    inventory
        .lines()
        .filter(|line| {
            let binding = line.split('→').next().unwrap_or(line).trim();
            let binding = normalized_binding(binding);
            BINDING_KEYS
                .iter()
                .any(|key| normalized_binding(key) == binding)
        })
        .collect()
}

fn normalized_binding(binding: &str) -> String {
    binding
        .chars()
        .filter(|character| !character.is_ascii_whitespace() && *character != '+')
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

fn rollback_bindings(plan: &BindingsPlan) -> io::Result<()> {
    let (current, _) = read_owned_bindings(&plan.path)?;
    if current.as_deref() != Some(plan.content.as_slice()) {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!("{} changed before rollback", plan.path.display()),
        ));
    }
    match &plan.baseline {
        Some(baseline) => {
            let rollback = BindingsPlan {
                path: plan.path.clone(),
                baseline: current,
                content: baseline.clone(),
                mode: plan.mode,
                changed: true,
                modified: false,
                compatible_unmanaged: false,
            };
            commit_bindings(&rollback)
        }
        None => {
            fs::remove_file(&plan.path)?;
            fs::File::open(
                plan.path
                    .parent()
                    .ok_or_else(|| io::Error::other("bindings path has no parent"))?,
            )?
            .sync_all()
        }
    }
}

#[cfg(test)]
mod tests {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::symlink;
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::*;

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
    fn managed_bindings_append_replace_and_reject_bad_markers() {
        let original = b"-- user binding\no.bind(\"SUPER + Q\", \"User\", \"true\")\n";
        let installed = render_bindings(original).unwrap();
        assert!(installed.starts_with(original));
        assert!(
            installed
                .windows(b"boomux desktop gather".len())
                .any(|window| window == b"boomux desktop gather")
        );
        assert_eq!(render_bindings(&installed).unwrap(), installed);

        let modified = String::from_utf8(installed.clone())
            .unwrap()
            .replace("Toggle Boomux panel", "Custom panel")
            .into_bytes();
        let repaired = render_bindings(&modified).unwrap();
        assert_eq!(repaired, installed);
        assert!(render_bindings(BINDINGS_BEGIN.as_bytes()).is_err());
        assert!(
            render_bindings(format!("{BINDINGS_BEGIN}\n{BINDINGS_END}\n{BINDINGS_END}").as_bytes())
                .is_err()
        );
    }

    #[test]
    fn managed_bindings_preserve_non_utf8_user_bytes() {
        let original = b"-- user\n\xff\xfe\n";
        let installed = render_bindings(original).unwrap();
        assert!(installed.starts_with(original));
        let range = managed_block_range(&installed).unwrap().unwrap();
        let mut removed = installed;
        removed.drain(range);
        assert_eq!(removed, b"-- user\n\xff\xfe\n\n");
    }

    #[test]
    fn conflict_inventory_selects_only_managed_keys() {
        let inventory =
            "SUPER + B        → Browser\nSUPER + Q        → User\nSUPER CTRL + W   → Close\n";
        assert_eq!(
            binding_conflicts(inventory),
            vec!["SUPER + B        → Browser", "SUPER CTRL + W   → Close"]
        );
    }

    #[test]
    fn complete_unmanaged_profile_is_compatible_but_partial_profile_is_not() {
        let complete = COMPATIBLE_BINDING_SNIPPETS
            .iter()
            .flat_map(|snippet| snippet.iter().copied().chain(*b"\n"))
            .collect::<Vec<_>>();
        assert!(compatible_unmanaged_profile(&complete));

        let partial =
            &complete[..complete.len() - COMPATIBLE_BINDING_SNIPPETS.last().unwrap().len()];
        assert!(!compatible_unmanaged_profile(partial));
    }

    #[test]
    fn bindings_commit_preserves_mode_and_rejects_concurrent_changes() {
        let directory = TestDirectory::new();
        let path = directory.0.join("bindings.lua");
        let baseline = b"-- user\n".to_vec();
        fs::write(&path, &baseline).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o640)).unwrap();
        let content = render_bindings(b"-- user\n").unwrap();
        let plan = BindingsPlan {
            path: path.clone(),
            baseline: Some(baseline),
            content: content.clone(),
            mode: 0o640,
            changed: true,
            modified: false,
            compatible_unmanaged: false,
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
    fn bindings_rollback_restores_existing_and_missing_baselines() {
        let directory = TestDirectory::new();
        let existing = directory.0.join("existing.lua");
        fs::write(&existing, b"before").unwrap();
        let existing_plan = BindingsPlan {
            path: existing.clone(),
            baseline: Some(b"before".to_vec()),
            content: b"after".to_vec(),
            mode: 0o640,
            changed: true,
            modified: false,
            compatible_unmanaged: false,
        };
        commit_bindings(&existing_plan).unwrap();
        rollback_bindings(&existing_plan).unwrap();
        assert_eq!(fs::read(&existing).unwrap(), b"before");

        let missing = directory.0.join("missing.lua");
        let missing_plan = BindingsPlan {
            path: missing.clone(),
            baseline: None,
            content: b"created".to_vec(),
            mode: 0o600,
            changed: true,
            modified: false,
            compatible_unmanaged: false,
        };
        commit_bindings(&missing_plan).unwrap();
        rollback_bindings(&missing_plan).unwrap();
        assert!(!missing.exists());
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

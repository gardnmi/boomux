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

    println!("{}", paint("1;35", "BOOMUX SETUP"));
    println!(
        "{}",
        paint(
            "2",
            "Discover harnesses, lifecycle integrations, and desktop support."
        )
    );
    println!(
        "{}",
        paint(
            "2",
            "Daemon readiness is automatic; configuration changes require confirmation."
        )
    );

    section("System Check");
    if let Some(terminal_resolver) = executable_on_path("xdg-terminal-exec") {
        status(
            "ok",
            "32",
            "Terminal resolver",
            terminal_resolver.display().to_string(),
        );
    } else {
        status("!!", "33", "Terminal resolver", "not found on PATH");
    }
    let daemon_was_running = client::connect().is_ok();
    client::connect_or_start()?;
    status(
        "ok",
        "32",
        "Daemon",
        if daemon_was_running {
            "already running"
        } else {
            "started"
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

    section("Agent Harnesses");
    if detected == 0 {
        status("--", "2", "Harnesses", "none found on PATH");
    }
    let mut failures = Vec::new();
    let mut changed_harnesses = 0usize;
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
            continue;
        }
        if integration_status.asset.state == AssetState::Unavailable {
            failures.push(format!(
                "{} integration could not be inspected: {}",
                integration_status.display_name,
                integration_status
                    .asset
                    .error
                    .as_deref()
                    .unwrap_or("unknown error")
            ));
            continue;
        }
        let force = integration_status.asset.state == AssetState::Modified;
        let plan = match integration_management::plan_install(integration, &environment, force) {
            Ok(plan) => plan,
            Err(error) => {
                failures.push(format!(
                    "{} integration: {error}",
                    integration_status.display_name
                ));
                continue;
            }
        };
        let action = match plan.action {
            InstallAction::Install => "Install",
            InstallAction::Replace => "Replace modified",
            InstallAction::Unchanged => continue,
        };
        detail(format!("Plan: {action} asset at {}", plan.path));
        if confirm(&format!(
            "{action} the {} integration?",
            integration_status.display_name
        ))? {
            match integration_management::install(integration, &environment, force) {
                Ok(result) => {
                    status(
                        "ok",
                        "32",
                        integration_status.display_name,
                        "integration installed",
                    );
                    detail(format!("path: {}", result.path));
                    if result.restart_required {
                        changed_harnesses += 1;
                        detail(integration.installation().reload_message);
                    }
                }
                Err(error) => failures.push(format!(
                    "{} integration: {error}",
                    integration_status.display_name
                )),
            }
        } else {
            status("--", "2", integration_status.display_name, "skipped");
        }
    }

    if detected > 0
        && let Err(error) = setup_agent_skill()
    {
        failures.push(format!("Agent Skill: {error}"));
    }

    if let Err(error) = setup_omarchy() {
        failures.push(format!("Omarchy desktop setup: {error}"));
    }

    section("Summary");
    if failures.is_empty() {
        status("ok", "32", "Setup", "completed without errors");
        if changed_harnesses > 0 {
            detail("Restart changed harnesses before verifying lifecycle reporting.");
        } else {
            detail("No harness restart is required.");
        }
        return Ok(());
    }
    for failure in &failures {
        eprintln!("  {} {failure}", paint("31", "[xx]"));
    }
    Err(io::Error::other(format!(
        "setup completed with {} failure{}",
        failures.len(),
        if failures.len() == 1 { "" } else { "s" }
    ))
    .into())
}

fn setup_agent_skill() -> Result<(), Box<dyn Error>> {
    let home = required_home()?;
    let path = home.join(".agents/skills/boomux/SKILL.md");
    match integration_management::regular_file_matches(&path, crate::BOOMUX_SKILL)? {
        Some(true) => {
            status("ok", "32", "Agent Skill", "current");
            detail(path.display().to_string());
            Ok(())
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
                crate::install_skill(modified)
            } else {
                status("--", "2", "Agent Skill", "skipped");
                Ok(())
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

fn setup_omarchy() -> Result<(), Box<dyn Error>> {
    section("Desktop Integration");
    let Some(omarchy) = executable_on_path("omarchy") else {
        status("--", "2", "Omarchy", "not detected");
        detail("Desktop plugin and keybindings were skipped.");
        return Ok(());
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
    let (plugin_enabled, plugin_changed) = match plugins
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
            detail("Reload the shell if the plugin was installed after Omarchy Shell started.");
            (
                true,
                confirm("Restart Omarchy Shell and reload omarchy-boomux?")?,
            )
        }
        Some(_) => {
            status(
                "!!",
                "33",
                "omarchy-boomux",
                "recommended core experience is disabled",
            );
            detail("Enabling the plugin also restarts Omarchy Shell so it is loaded.");
            if confirm("Enable and load the recommended omarchy-boomux plugin?")? {
                run_command(
                    &omarchy,
                    &["plugin", "enable", OMARCHY_PLUGIN_ID],
                    COMMAND_TIMEOUT,
                )?;
                status("ok", "32", "omarchy-boomux", "enabled");
                (true, true)
            } else {
                (false, false)
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
            if confirm("Install, enable, and load the recommended omarchy-boomux plugin?")? {
                run_command(
                    &omarchy,
                    &["plugin", "add", OMARCHY_PLUGIN_URL, "--enable", "--yes"],
                    PLUGIN_INSTALL_TIMEOUT,
                )?;
                status("ok", "32", "omarchy-boomux", "installed and enabled");
                (true, true)
            } else {
                (false, false)
            }
        }
    };
    if plugin_changed {
        run_command(&omarchy, &["restart", "shell"], PLUGIN_INSTALL_TIMEOUT)?;
        status("ok", "32", "Omarchy Shell", "restarted with plugin loaded");
    }
    if !plugin_enabled {
        status("--", "2", "Keybindings", "skipped; plugin is not enabled");
        return Ok(());
    }

    setup_hyprland_workspace_layer()?;

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

        if !confirm("Reinstall the standard Boomux keybinding profile?")? {
            status("--", "2", "Keybindings", "kept unchanged");
            return Ok(());
        }
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
            return Ok(());
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
    Ok(())
}

fn setup_hyprland_workspace_layer() -> Result<(), Box<dyn Error>> {
    if crate::config::load()?.desktop.workspace_layer
        == crate::config::DesktopWorkspaceLayer::HyprlandSpecial
    {
        status("ok", "32", "Workspace layer", "enabled");
        return Ok(());
    }

    status(
        "->",
        "36",
        "Workspace layer",
        "recommended for the core Omarchy experience",
    );
    detail("Present coordinated Boomux Workspaces as named Hyprland special Workspaces.");
    if !confirm("Enable the recommended Hyprland Workspace layer?")? {
        status("--", "2", "Workspace layer", "kept disabled");
        return Ok(());
    }

    let path = crate::config::enable_hyprland_workspace_layer()?;
    status("ok", "32", "Workspace layer", "enabled");
    detail(format!("config: {}", path.display()));
    Ok(())
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
    print!("  {} {} ", paint("1;33", prompt), paint("2", "[y/N]"));
    io::stdout().flush()?;
    let mut response = String::new();
    io::stdin().read_line(&mut response)?;
    Ok(matches!(
        response.trim().to_ascii_lowercase().as_str(),
        "y" | "yes"
    ))
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

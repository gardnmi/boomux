use std::env;
use std::error::Error;
use std::fmt;
use std::io::{self, Read};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use serde::Deserialize;
use uuid::Uuid;

const SPECIAL_PREFIX: &str = "boomux-";
const SPECIAL_WORKSPACE_PREFIX: &str = "special:boomux-";
const WINDOW_PREFIX: &str = "boomux:shell:";
const COMMAND_TIMEOUT: Duration = Duration::from_secs(1);
const WINDOW_WAIT: Duration = Duration::from_secs(2);
const WINDOW_RETRY: Duration = Duration::from_millis(50);
const MAX_JSON_BYTES: usize = 4 * 1024 * 1024;
const MAX_DIAGNOSTIC_BYTES: usize = 8 * 1024;
const MAX_MONITORS: usize = 64;
const MAX_CLIENTS: usize = 4096;
const MAX_LAUNCH_TOKENS: usize = 4096;

#[derive(Debug)]
pub(crate) struct HyprlandError(String);

pub(crate) enum ShellWindowReuse {
    Absent,
    Reused,
    ExistingPlacementFailed(HyprlandError),
}

pub(crate) enum LaunchToken {
    Acquired(std::fs::File),
    Held,
}

impl fmt::Display for HyprlandError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl Error for HyprlandError {}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
struct WorkspaceRef {
    id: i64,
    name: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Monitor {
    focused: bool,
    special_workspace: WorkspaceRef,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
struct ClientWindow {
    address: String,
    workspace: WorkspaceRef,
    initial_title: String,
    #[serde(default)]
    stable_id: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ActiveWindow {
    address: String,
    workspace: WorkspaceRef,
    initial_title: String,
    stable_id: String,
    floating: bool,
    pinned: bool,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct ActiveShellIdentity {
    pub(crate) address: String,
    pub(crate) stable_id: String,
    pub(crate) node_id: String,
    pub(crate) shell_id: String,
    initial_title: String,
    workspace_name: String,
}

pub(crate) fn session_available() -> bool {
    env::var_os("HYPRLAND_INSTANCE_SIGNATURE").is_some_and(|value| !value.is_empty())
}

pub(crate) fn special_workspace_name(workspace_id: &str) -> Result<String, HyprlandError> {
    canonical_uuid(workspace_id, "Workspace ID")?;
    Ok(format!("{SPECIAL_PREFIX}{workspace_id}"))
}

fn special_workspace_selector(workspace_id: &str) -> Result<String, HyprlandError> {
    Ok(format!("special:{}", special_workspace_name(workspace_id)?))
}

pub(crate) fn active_boomux_workspace() -> Result<Option<String>, HyprlandError> {
    let monitors: Vec<Monitor> = query_json(&["-j", "monitors"], MAX_MONITORS)?;
    let mut focused = monitors.into_iter().filter(|monitor| monitor.focused);
    let monitor = focused
        .next()
        .ok_or_else(|| HyprlandError("Hyprland did not report a focused monitor".into()))?;
    if focused.next().is_some() {
        return Err(HyprlandError(
            "Hyprland reported more than one focused monitor".into(),
        ));
    }
    parse_active_special(&monitor.special_workspace)
}

pub(crate) fn toggle_special(workspace_id: &str) -> Result<(), HyprlandError> {
    apply_workspace_layout(workspace_id)?;
    dispatch(toggle_expression(workspace_id)?)
}

pub(crate) fn move_regular_workspace(direction: i8) -> Result<(), HyprlandError> {
    let workspace = match direction.cmp(&0) {
        std::cmp::Ordering::Greater => "e+1",
        std::cmp::Ordering::Less => "e-1",
        std::cmp::Ordering::Equal => {
            return Err(HyprlandError("workspace direction must not be zero".into()));
        }
    };
    dispatch(format!("hl.dsp.focus({{ workspace = \"{workspace}\" }})"))
}

pub(crate) fn close_active_window() -> Result<(), HyprlandError> {
    dispatch("hl.dsp.window.close({ window = \"activewindow\" })".into())
}

pub(crate) fn pop_active_window(pin: bool) -> Result<(), HyprlandError> {
    let window: ActiveWindow = query_object(&["-j", "activewindow"])?;
    validate_address(&window.address)?;
    for expression in pop_expressions(&window.address, window.floating, window.pinned, pin)? {
        dispatch(expression)?;
    }
    Ok(())
}

pub(crate) fn active_boomux_shell(
    workspace_id: &str,
) -> Result<ActiveShellIdentity, HyprlandError> {
    let target = special_workspace_selector(workspace_id)?;
    let window = active_shell_window()?;
    if window.workspace_name != target {
        return Err(HyprlandError(
            "the active window is not in the visible Boomux Workspace".into(),
        ));
    }
    Ok(window)
}

pub(crate) fn active_shell_window() -> Result<ActiveShellIdentity, HyprlandError> {
    let window: ActiveWindow = query_object(&["-j", "activewindow"])?;
    validate_address(&window.address)?;
    if window.stable_id.is_empty() {
        return Err(HyprlandError(
            "the active Hyprland window has no stable identity".into(),
        ));
    }
    let (node_id, shell_id) = parse_shell_title(&window.initial_title)?;
    Ok(ActiveShellIdentity {
        address: window.address,
        stable_id: window.stable_id,
        node_id,
        shell_id,
        initial_title: window.initial_title,
        workspace_name: window.workspace.name,
    })
}

pub(crate) fn return_shell_window(
    window: &ActiveShellIdentity,
    workspace_id: &str,
) -> Result<(), HyprlandError> {
    let matching = clients()?
        .into_iter()
        .find(|client| client.address == window.address && client.stable_id == window.stable_id);
    let client = matching.ok_or_else(|| {
        HyprlandError("the active Boomux terminal window is no longer available".into())
    })?;
    if client.initial_title != window.initial_title {
        return Err(HyprlandError(
            "the active Boomux terminal window changed before return".into(),
        ));
    }
    move_window(&window.address, workspace_id)
}

pub(crate) fn revalidate_boomux_shell_window(
    window: &ActiveShellIdentity,
    workspace_id: &str,
) -> Result<(), HyprlandError> {
    let target = special_workspace_selector(workspace_id)?;
    revalidate_shell_window(&clients()?, window, &target)
}

fn revalidate_shell_window(
    clients: &[ClientWindow],
    window: &ActiveShellIdentity,
    target: &str,
) -> Result<(), HyprlandError> {
    let matching = clients
        .iter()
        .find(|client| client.address == window.address && client.stable_id == window.stable_id);
    let client = matching.ok_or_else(|| {
        HyprlandError("the focused Boomux terminal window is no longer available".into())
    })?;
    let (node_id, shell_id) = parse_shell_title(&client.initial_title)?;
    if client.workspace.name != target || node_id != window.node_id || shell_id != window.shell_id {
        return Err(HyprlandError(
            "the focused Boomux terminal window changed before close".into(),
        ));
    }
    Ok(())
}

pub(crate) fn shell_title(
    node_id: &str,
    shell_id: &str,
    human_title: &str,
) -> Result<String, HyprlandError> {
    canonical_uuid(node_id, "Node ID")?;
    canonical_uuid(shell_id, "Shell ID")?;
    Ok(format!(
        "{}{}:{} | {human_title}",
        WINDOW_PREFIX, node_id, shell_id
    ))
}

pub(crate) fn workspace_has_windows(workspace_id: &str) -> Result<bool, HyprlandError> {
    let target = special_workspace_selector(workspace_id)?;
    Ok(clients()?
        .iter()
        .any(|client| client.workspace.name == target))
}

pub(crate) fn wait_for_workspace_window(workspace_id: &str) -> Result<(), HyprlandError> {
    let deadline = Instant::now() + WINDOW_WAIT;
    loop {
        if workspace_has_windows(workspace_id)? {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(HyprlandError(
                "Hyprland did not report a terminal in the Boomux Workspace".into(),
            ));
        }
        thread::sleep(WINDOW_RETRY);
    }
}

pub(crate) fn acquire_launch_lock() -> Result<std::fs::File, HyprlandError> {
    let parent = runtime_parent()?;
    let path = parent.join("hyprland-terminal-open.lock");
    let file = open_owned_lock(&path)?;
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } == -1 {
        return Err(HyprlandError(format!(
            "could not lock Hyprland terminal launch: {}",
            io::Error::last_os_error()
        )));
    }
    cleanup_launch_tokens(&parent)?;
    Ok(file)
}

pub(crate) fn acquire_launch_token(
    node_id: &str,
    shell_id: &str,
) -> Result<LaunchToken, HyprlandError> {
    canonical_uuid(node_id, "Node ID")?;
    canonical_uuid(shell_id, "Shell ID")?;
    let path = runtime_parent()?.join(format!("hyprland-terminal-{node_id}-{shell_id}.lock"));
    let file = open_owned_lock(&path)?;
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
        return Ok(LaunchToken::Acquired(file));
    }
    let error = io::Error::last_os_error();
    if error.kind() == io::ErrorKind::WouldBlock {
        Ok(LaunchToken::Held)
    } else {
        Err(HyprlandError(format!(
            "could not reserve Hyprland terminal launch: {error}"
        )))
    }
}

fn runtime_parent() -> Result<std::path::PathBuf, HyprlandError> {
    let parent = crate::client::socket_path()
        .map_err(|error| HyprlandError(format!("Boomux runtime is unavailable: {error}")))?
        .parent()
        .ok_or_else(|| HyprlandError("Boomux runtime socket has no parent".into()))?
        .to_owned();
    Ok(parent)
}

fn open_owned_lock(path: &std::path::Path) -> Result<std::fs::File, HyprlandError> {
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
        .map_err(|error| HyprlandError(format!("could not open Hyprland launch lock: {error}")))?;
    let metadata = file.metadata().map_err(|error| {
        HyprlandError(format!("could not inspect Hyprland launch lock: {error}"))
    })?;
    if !metadata.is_file() || metadata.uid() != unsafe { libc::geteuid() } {
        return Err(HyprlandError(
            "Hyprland launch lock is not an owned regular file".into(),
        ));
    }
    file.set_permissions(std::fs::Permissions::from_mode(0o600))
        .map_err(|error| {
            HyprlandError(format!("could not secure Hyprland launch lock: {error}"))
        })?;
    Ok(file)
}

fn cleanup_launch_tokens(parent: &std::path::Path) -> Result<(), HyprlandError> {
    let entries = std::fs::read_dir(parent).map_err(|error| {
        HyprlandError(format!("could not inspect Hyprland launch tokens: {error}"))
    })?;
    let mut token_count = 0;
    for entry in entries {
        let entry = entry.map_err(|error| {
            HyprlandError(format!("could not inspect Hyprland launch token: {error}"))
        })?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if !name.starts_with("hyprland-terminal-") || name == "hyprland-terminal-open.lock" {
            continue;
        }
        token_count += 1;
        if token_count > MAX_LAUNCH_TOKENS {
            return Err(HyprlandError(
                "Hyprland launch token count exceeded the limit".into(),
            ));
        }
        let path = entry.path();
        let token = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&path)
            .map_err(|error| {
                HyprlandError(format!("could not open Hyprland launch token: {error}"))
            })?;
        let metadata = token.metadata().map_err(|error| {
            HyprlandError(format!("could not inspect Hyprland launch token: {error}"))
        })?;
        if !metadata.is_file() || metadata.uid() != unsafe { libc::geteuid() } {
            return Err(HyprlandError(
                "Hyprland launch token is not an owned regular file".into(),
            ));
        }
        if unsafe { libc::flock(token.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
            std::fs::remove_file(&path).map_err(|error| {
                HyprlandError(format!(
                    "could not remove stale Hyprland launch token: {error}"
                ))
            })?;
        } else if io::Error::last_os_error().kind() != io::ErrorKind::WouldBlock {
            return Err(HyprlandError(format!(
                "could not inspect Hyprland launch token lock: {}",
                io::Error::last_os_error()
            )));
        }
    }
    Ok(())
}

pub(crate) fn place_existing_shell_windows(
    node_id: &str,
    shell_id: &str,
    workspace_id: &str,
) -> Result<ShellWindowReuse, HyprlandError> {
    let prefix = shell_title_prefix(node_id, shell_id)?;
    let target = special_workspace_selector(workspace_id)?;
    let matching = clients()?
        .into_iter()
        .filter(|client| client.initial_title.starts_with(&prefix))
        .collect::<Vec<_>>();
    for client in &matching {
        if client.workspace.name != target
            && let Err(error) = move_window(&client.address, workspace_id)
        {
            return Ok(ShellWindowReuse::ExistingPlacementFailed(error));
        }
    }
    Ok(if matching.is_empty() {
        ShellWindowReuse::Absent
    } else {
        ShellWindowReuse::Reused
    })
}

pub(crate) fn wait_and_place_shell_window(
    node_id: &str,
    shell_id: &str,
    workspace_id: &str,
) -> Result<(), HyprlandError> {
    let deadline = Instant::now() + WINDOW_WAIT;
    loop {
        match place_existing_shell_windows(node_id, shell_id, workspace_id)? {
            ShellWindowReuse::Absent => {}
            ShellWindowReuse::Reused => return Ok(()),
            ShellWindowReuse::ExistingPlacementFailed(error) => return Err(error),
        }
        if Instant::now() >= deadline {
            return Err(HyprlandError(
                "Hyprland did not report the launched terminal window".into(),
            ));
        }
        thread::sleep(WINDOW_RETRY);
    }
}

fn shell_title_prefix(node_id: &str, shell_id: &str) -> Result<String, HyprlandError> {
    canonical_uuid(node_id, "Node ID")?;
    canonical_uuid(shell_id, "Shell ID")?;
    Ok(format!("{}{}:{} | ", WINDOW_PREFIX, node_id, shell_id))
}

fn parse_shell_title(title: &str) -> Result<(String, String), HyprlandError> {
    let identity = title
        .strip_prefix(WINDOW_PREFIX)
        .and_then(|value| value.split_once(" | ").map(|(identity, _)| identity))
        .ok_or_else(|| HyprlandError("the active window is not a Boomux terminal".into()))?;
    let (node_id, shell_id) = identity
        .split_once(':')
        .ok_or_else(|| HyprlandError("the active Boomux terminal identity is invalid".into()))?;
    canonical_uuid(node_id, "Node ID")?;
    canonical_uuid(shell_id, "Shell ID")?;
    Ok((node_id.into(), shell_id.into()))
}

fn move_window(address: &str, workspace_id: &str) -> Result<(), HyprlandError> {
    apply_workspace_layout(workspace_id)?;
    dispatch(move_expression(address, workspace_id)?)
}

fn apply_workspace_layout(workspace_id: &str) -> Result<(), HyprlandError> {
    evaluate(layout_expression(workspace_id)?)
}

fn toggle_expression(workspace_id: &str) -> Result<String, HyprlandError> {
    let name = special_workspace_name(workspace_id)?;
    Ok(format!("hl.dsp.workspace.toggle_special(\"{name}\")"))
}

fn move_expression(address: &str, workspace_id: &str) -> Result<String, HyprlandError> {
    validate_address(address)?;
    let target = special_workspace_selector(workspace_id)?;
    Ok(format!(
        "hl.dsp.window.move({{ workspace = \"{target}\", follow = false, window = \"address:{address}\" }})"
    ))
}

fn layout_expression(workspace_id: &str) -> Result<String, HyprlandError> {
    let target = special_workspace_selector(workspace_id)?;
    Ok(format!(
        "hl.workspace_rule({{ workspace = \"{target}\", layout = \"dwindle\" }})"
    ))
}

fn pop_expressions(
    address: &str,
    floating: bool,
    pinned: bool,
    pin: bool,
) -> Result<Vec<String>, HyprlandError> {
    validate_address(address)?;
    let window = format!("address:{address}");
    let pin_expression = || format!("hl.dsp.window.pin({{ window = \"{window}\" }})");
    let float_expression =
        || format!("hl.dsp.window.float({{ window = \"{window}\", action = \"toggle\" }})");
    let tag_expression =
        |tag: &str| format!("hl.dsp.window.tag({{ window = \"{window}\", tag = \"{tag}\" }})");
    if pin && pinned {
        return Ok(vec![
            pin_expression(),
            float_expression(),
            tag_expression("-pop"),
        ]);
    }
    if pin {
        return Ok(vec![
            float_expression(),
            format!("hl.dsp.window.resize({{ window = \"{window}\", x = 1300, y = 900 }})"),
            format!("hl.dsp.window.center({{ window = \"{window}\" }})"),
            pin_expression(),
            format!("hl.dsp.window.alter_zorder({{ window = \"{window}\", mode = \"top\" }})"),
            tag_expression("+pop"),
        ]);
    }
    let mut expressions = Vec::new();
    if pinned {
        expressions.push(pin_expression());
    }
    expressions.push(float_expression());
    if !floating {
        expressions.extend([
            format!("hl.dsp.window.resize({{ window = \"{window}\", x = 1300, y = 900 }})"),
            format!("hl.dsp.window.center({{ window = \"{window}\" }})"),
        ]);
    }
    expressions.push(tag_expression("-pop"));
    Ok(expressions)
}

fn clients() -> Result<Vec<ClientWindow>, HyprlandError> {
    query_json(&["-j", "clients"], MAX_CLIENTS)
}

fn parse_active_special(workspace: &WorkspaceRef) -> Result<Option<String>, HyprlandError> {
    if workspace.id == 0 && workspace.name.is_empty() {
        return Ok(None);
    }
    if workspace.id >= 0 {
        return Err(HyprlandError(
            "Hyprland reported an invalid active special workspace".into(),
        ));
    }
    let Some(workspace_id) = workspace.name.strip_prefix(SPECIAL_WORKSPACE_PREFIX) else {
        return Ok(None);
    };
    canonical_uuid(workspace_id, "Boomux special Workspace ID")?;
    Ok(Some(workspace_id.to_owned()))
}

fn canonical_uuid(value: &str, label: &str) -> Result<(), HyprlandError> {
    let parsed =
        Uuid::parse_str(value).map_err(|_| HyprlandError(format!("{label} is not a UUID")))?;
    if parsed.to_string() != value {
        return Err(HyprlandError(format!("{label} is not canonical")));
    }
    Ok(())
}

fn validate_address(address: &str) -> Result<(), HyprlandError> {
    let Some(hex) = address.strip_prefix("0x") else {
        return Err(HyprlandError("Hyprland window address is invalid".into()));
    };
    if hex.is_empty() || hex.len() > 16 || !hex.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(HyprlandError("Hyprland window address is invalid".into()));
    }
    Ok(())
}

fn dispatch(expression: String) -> Result<(), HyprlandError> {
    run_ok("dispatch", expression)
}

fn evaluate(expression: String) -> Result<(), HyprlandError> {
    run_ok("eval", expression)
}

fn run_ok(operation: &str, expression: String) -> Result<(), HyprlandError> {
    let output = run_hyprctl(&[operation, &expression], MAX_DIAGNOSTIC_BYTES)?;
    if output != b"ok\n" && output != b"ok" {
        return Err(HyprlandError(format!(
            "Hyprland rejected the {operation}: {}",
            String::from_utf8_lossy(&output).trim()
        )));
    }
    Ok(())
}

fn query_json<T: for<'de> Deserialize<'de>>(
    arguments: &[&str],
    max_items: usize,
) -> Result<Vec<T>, HyprlandError> {
    let output = run_hyprctl(arguments, MAX_JSON_BYTES)?;
    let values: Vec<T> = serde_json::from_slice(&output)
        .map_err(|error| HyprlandError(format!("Hyprland returned invalid JSON: {error}")))?;
    if values.len() > max_items {
        return Err(HyprlandError(
            "Hyprland response exceeded the item limit".into(),
        ));
    }
    Ok(values)
}

fn query_object<T: for<'de> Deserialize<'de>>(arguments: &[&str]) -> Result<T, HyprlandError> {
    let output = run_hyprctl(arguments, MAX_JSON_BYTES)?;
    serde_json::from_slice(&output)
        .map_err(|error| HyprlandError(format!("Hyprland returned invalid JSON: {error}")))
}

fn run_hyprctl(arguments: &[&str], max_stdout: usize) -> Result<Vec<u8>, HyprlandError> {
    if !session_available() {
        return Err(HyprlandError(
            "no active Hyprland session was detected".into(),
        ));
    }
    let mut child = Command::new("hyprctl")
        .args(arguments)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| HyprlandError(format!("could not start hyprctl: {error}")))?;
    let stdout = child.stdout.take().expect("piped stdout");
    let stderr = child.stderr.take().expect("piped stderr");
    let stdout_reader = thread::spawn(move || read_bounded(stdout, max_stdout));
    let stderr_reader = thread::spawn(move || read_bounded(stderr, MAX_DIAGNOSTIC_BYTES));
    let deadline = Instant::now() + COMMAND_TIMEOUT;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(10)),
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(HyprlandError("hyprctl timed out".into()));
            }
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(HyprlandError(format!(
                    "could not wait for hyprctl: {error}"
                )));
            }
        }
    };
    let stdout = stdout_reader
        .join()
        .map_err(|_| HyprlandError("hyprctl stdout reader panicked".into()))?
        .map_err(|error| HyprlandError(format!("could not read hyprctl stdout: {error}")))?;
    let stderr = stderr_reader
        .join()
        .map_err(|_| HyprlandError("hyprctl stderr reader panicked".into()))?
        .map_err(|error| HyprlandError(format!("could not read hyprctl stderr: {error}")))?;
    if !status.success() {
        return Err(HyprlandError(format!(
            "hyprctl failed: {}",
            String::from_utf8_lossy(&stderr).trim()
        )));
    }
    Ok(stdout)
}

fn read_bounded(mut reader: impl Read, limit: usize) -> io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    reader
        .by_ref()
        .take(u64::try_from(limit).unwrap_or(u64::MAX) + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() > limit {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "output exceeded the size limit",
        ));
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    const WORKSPACE_ID: &str = "550e8400-e29b-41d4-a716-446655440000";
    const NODE_ID: &str = "550e8400-e29b-41d4-a716-446655440001";
    const SHELL_ID: &str = "550e8400-e29b-41d4-a716-446655440002";

    #[test]
    fn workspace_names_are_stable_and_identity_based() {
        assert_eq!(
            special_workspace_name(WORKSPACE_ID).unwrap(),
            "boomux-550e8400-e29b-41d4-a716-446655440000"
        );
        assert!(special_workspace_name("project").is_err());
    }

    #[test]
    fn active_special_workspace_requires_the_reserved_canonical_identity() {
        assert_eq!(
            parse_active_special(&WorkspaceRef {
                id: 0,
                name: String::new(),
            })
            .unwrap(),
            None
        );
        assert_eq!(
            parse_active_special(&WorkspaceRef {
                id: -99,
                name: format!("special:boomux-{WORKSPACE_ID}"),
            })
            .unwrap()
            .as_deref(),
            Some(WORKSPACE_ID)
        );
        assert_eq!(
            parse_active_special(&WorkspaceRef {
                id: -98,
                name: "special:scratchpad".into(),
            })
            .unwrap(),
            None
        );
        assert!(
            parse_active_special(&WorkspaceRef {
                id: -97,
                name: "special:boomux-not-an-id".into(),
            })
            .is_err()
        );
    }

    #[test]
    fn shell_titles_keep_machine_identity_separate_from_human_text() {
        assert_eq!(
            shell_title(NODE_ID, SHELL_ID, "project - agent").unwrap(),
            format!("boomux:shell:{NODE_ID}:{SHELL_ID} | project - agent")
        );
        assert!(shell_title("node;bad", SHELL_ID, "title").is_err());
        assert_eq!(
            parse_shell_title(&format!(
                "boomux:shell:{NODE_ID}:{SHELL_ID} | renamed terminal"
            ))
            .unwrap(),
            (NODE_ID.into(), SHELL_ID.into())
        );
        for invalid in [
            "ordinary terminal",
            "project - agent",
            "boomux:shell:not-a-node:not-a-shell | title",
            "boomux:shell:550e8400-e29b-41d4-a716-446655440001 | title",
            &format!("prefix boomux:shell:{NODE_ID}:{SHELL_ID} | title"),
        ] {
            assert!(parse_shell_title(invalid).is_err(), "{invalid}");
        }
    }

    #[test]
    fn window_addresses_are_strictly_validated() {
        assert!(validate_address("0x1234abcdef").is_ok());
        for invalid in ["1234", "0x", "0xxyz", "0x1234567890abcdef0"] {
            assert!(validate_address(invalid).is_err(), "{invalid}");
        }
    }

    #[test]
    fn dispatch_expressions_keep_validated_values_in_fixed_lua_shapes() {
        assert_eq!(
            toggle_expression(WORKSPACE_ID).unwrap(),
            format!("hl.dsp.workspace.toggle_special(\"boomux-{WORKSPACE_ID}\")")
        );
        assert_eq!(
            move_expression("0x1234abcd", WORKSPACE_ID).unwrap(),
            format!(
                "hl.dsp.window.move({{ workspace = \"special:boomux-{WORKSPACE_ID}\", follow = false, window = \"address:0x1234abcd\" }})"
            )
        );
        assert!(move_expression("0x1\"); os.execute(\"bad\")", WORKSPACE_ID).is_err());
        assert_eq!(
            layout_expression(WORKSPACE_ID).unwrap(),
            format!(
                "hl.workspace_rule({{ workspace = \"special:boomux-{WORKSPACE_ID}\", layout = \"dwindle\" }})"
            )
        );
    }

    #[test]
    fn contextual_pop_avoids_pinning_boomux_windows() {
        let address = "0x1234abcd";
        let floating = pop_expressions(address, false, false, false).unwrap();
        assert_eq!(floating.len(), 4);
        assert!(floating[0].contains("window.float"));
        assert!(floating[1].contains("window.resize"));
        assert!(floating[2].contains("window.center"));
        assert!(
            floating
                .iter()
                .all(|expression| !expression.contains("window.pin"))
        );

        let tiled = pop_expressions(address, true, false, false).unwrap();
        assert_eq!(tiled.len(), 2);
        assert!(tiled[0].contains("window.float"));
        assert!(
            tiled
                .iter()
                .all(|expression| !expression.contains("window.pin"))
        );

        let ordinary = pop_expressions(address, false, false, true).unwrap();
        assert!(
            ordinary
                .iter()
                .any(|expression| expression.contains("window.pin"))
        );
        assert!(pop_expressions("not-an-address", false, false, false).is_err());
    }

    #[test]
    fn launch_token_cleanup_removes_only_unlocked_owned_files() {
        let parent = env::temp_dir().join(format!("boomux-hyprland-tokens-{}", Uuid::new_v4()));
        std::fs::create_dir(&parent).unwrap();
        let stale = parent.join("hyprland-terminal-stale.lock");
        let held = parent.join("hyprland-terminal-held.lock");
        std::fs::write(&stale, []).unwrap();
        std::fs::write(&held, []).unwrap();
        let held_file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&held)
            .unwrap();
        assert_eq!(
            unsafe { libc::flock(held_file.as_raw_fd(), libc::LOCK_EX) },
            0
        );

        cleanup_launch_tokens(&parent).unwrap();
        assert!(!stale.exists());
        assert!(held.exists());

        drop(held_file);
        cleanup_launch_tokens(&parent).unwrap();
        assert!(!held.exists());
        std::fs::remove_dir(parent).unwrap();
    }

    #[test]
    fn close_revalidation_pins_the_exact_stable_window_and_shell() {
        let target = format!("special:boomux-{WORKSPACE_ID}");
        let window = ActiveShellIdentity {
            address: "0x1234abcd".into(),
            stable_id: "window-1".into(),
            node_id: NODE_ID.into(),
            shell_id: SHELL_ID.into(),
            initial_title: shell_title(NODE_ID, SHELL_ID, "terminal").unwrap(),
            workspace_name: target.clone(),
        };
        let client = ClientWindow {
            address: window.address.clone(),
            stable_id: window.stable_id.clone(),
            workspace: WorkspaceRef {
                id: -99,
                name: target.clone(),
            },
            initial_title: shell_title(NODE_ID, SHELL_ID, "terminal").unwrap(),
        };
        assert!(revalidate_shell_window(std::slice::from_ref(&client), &window, &target).is_ok());

        let mut replaced = client.clone();
        replaced.stable_id = "window-2".into();
        assert!(revalidate_shell_window(&[replaced], &window, &target).is_err());
        let mut moved = client.clone();
        moved.workspace.name = "special:scratchpad".into();
        assert!(revalidate_shell_window(&[moved], &window, &target).is_err());
        let mut retitled = client;
        retitled.initial_title = shell_title(NODE_ID, WORKSPACE_ID, "terminal").unwrap();
        assert!(revalidate_shell_window(&[retitled], &window, &target).is_err());
    }
}

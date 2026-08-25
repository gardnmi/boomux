use std::env;
use std::error::Error;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::process::CommandExt;
use std::os::unix::{fs::PermissionsExt, net::UnixListener, net::UnixStream};
use std::path::{Path, PathBuf};
use std::process::{self, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant};
use uuid::Uuid;

static TEMPORARY_DIRECTORY_ID: AtomicU64 = AtomicU64::new(0);
const OPEN_GATE_TIMEOUT: Duration = Duration::from_secs(10);
const OPEN_GATE_RETRY: Duration = Duration::from_millis(10);

pub(crate) struct HyprlandPlacement<'a> {
    pub(crate) workspace_id: &'a str,
    pub(crate) node_id: &'a str,
    pub(crate) shell_id: &'a str,
}

pub(crate) struct OpenGate {
    listener: UnixListener,
    path: PathBuf,
    released: bool,
}

impl OpenGate {
    fn new() -> io::Result<Self> {
        let parent = crate::client::socket_path()?
            .parent()
            .ok_or_else(|| io::Error::other("Boomux runtime socket has no parent"))?
            .to_owned();
        let id = Uuid::new_v4().simple().to_string();
        let path = parent.join(format!("open-{}.sock", &id[..16]));
        Self::bind(path)
    }

    fn bind(path: PathBuf) -> io::Result<Self> {
        let listener = UnixListener::bind(&path)?;
        let gate = Self {
            listener,
            path,
            released: false,
        };
        fs::set_permissions(&gate.path, fs::Permissions::from_mode(0o600))?;
        gate.listener.set_nonblocking(true)?;
        Ok(gate)
    }

    pub(crate) fn release(mut self) -> io::Result<()> {
        let deadline = Instant::now() + OPEN_GATE_TIMEOUT;
        loop {
            match self.listener.accept() {
                Ok((mut stream, _)) => {
                    use std::io::Write;
                    stream.write_all(&[1])?;
                    self.released = true;
                    return Ok(());
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    if Instant::now() >= deadline {
                        return Err(io::Error::new(
                            io::ErrorKind::TimedOut,
                            "terminal did not become ready for the created Shell",
                        ));
                    }
                    thread::sleep(OPEN_GATE_RETRY);
                }
                Err(error) => return Err(error),
            }
        }
    }
}

impl Drop for OpenGate {
    fn drop(&mut self) {
        if !self.released
            && let Ok((mut stream, _)) = self.listener.accept()
        {
            use std::io::Write;
            let _ = stream.write_all(&[0]);
        }
        let _ = fs::remove_file(&self.path);
    }
}

pub(crate) fn validate_desktop_entry(entry: &str) -> Result<(), Box<dyn Error>> {
    let (desktop_entry, action) = entry
        .split_once(':')
        .map_or((entry, None), |(entry, action)| (entry, Some(action)));
    let invalid = entry.trim() != entry
        || desktop_entry.is_empty()
        || !desktop_entry.ends_with(".desktop")
        || desktop_entry.starts_with('-')
        || desktop_entry.contains(['/', '\\'])
        || entry.chars().any(char::is_whitespace)
        || action.is_some_and(|action| action.is_empty() || action.contains(':'));
    if invalid {
        Err(format!("invalid terminal desktop entry {entry:?}").into())
    } else {
        Ok(())
    }
}

pub(crate) fn selected(desktop_entry: Option<&str>) -> Result<String, Box<dyn Error>> {
    let preference = desktop_entry.map(TemporaryPreference::new).transpose()?;
    selected_with_preference(desktop_entry, preference.as_ref())
}

pub(crate) fn open(
    desktop_entry: Option<&str>,
    shell_id: &str,
    title: &str,
    takeover: bool,
) -> Result<(), Box<dyn Error>> {
    open_with_expected_run(desktop_entry, shell_id, None, title, takeover, None)
}

pub(crate) fn open_placed(
    desktop_entry: Option<&str>,
    shell_id: &str,
    title: &str,
    takeover: bool,
    placement: HyprlandPlacement<'_>,
) -> Result<(), Box<dyn Error>> {
    open_with_expected_run_and_placement(
        desktop_entry,
        shell_id,
        None,
        title,
        takeover,
        None,
        Some(placement),
    )
}

pub(crate) fn open_remote(
    desktop_entry: Option<&str>,
    node_id: &str,
    shell_id: &str,
    title: &str,
    takeover: bool,
) -> Result<(), Box<dyn Error>> {
    open_with_expected_run(
        desktop_entry,
        shell_id,
        Some(node_id),
        title,
        takeover,
        None,
    )
}

pub(crate) fn open_remote_placed(
    desktop_entry: Option<&str>,
    node_id: &str,
    shell_id: &str,
    title: &str,
    takeover: bool,
    workspace_id: &str,
) -> Result<(), Box<dyn Error>> {
    open_with_expected_run_and_placement(
        desktop_entry,
        shell_id,
        Some(node_id),
        title,
        takeover,
        None,
        Some(HyprlandPlacement {
            workspace_id,
            node_id,
            shell_id,
        }),
    )
}

pub(crate) fn open_waiting(
    desktop_entry: Option<&str>,
    node_id: Option<&str>,
    shell_id: &str,
    title: &str,
) -> Result<OpenGate, Box<dyn Error>> {
    let gate = OpenGate::new()?;
    let executable = attachment_executable()?;
    let arguments = waiting_attachment_arguments(&gate.path, shell_id, node_id);
    launch(
        desktop_entry,
        title,
        None,
        executable.as_os_str(),
        &arguments,
        None,
        false,
    )?;
    Ok(gate)
}

pub(crate) fn open_waiting_placed(
    desktop_entry: Option<&str>,
    remote_node_id: Option<&str>,
    local_node_id: &str,
    workspace_id: &str,
    shell_id: &str,
    title: &str,
) -> Result<OpenGate, Box<dyn Error>> {
    let gate = OpenGate::new()?;
    let executable = attachment_executable()?;
    let arguments = waiting_attachment_arguments(&gate.path, shell_id, remote_node_id);
    launch(
        desktop_entry,
        title,
        None,
        executable.as_os_str(),
        &arguments,
        Some(HyprlandPlacement {
            workspace_id,
            node_id: remote_node_id.unwrap_or(local_node_id),
            shell_id,
        }),
        false,
    )?;
    Ok(gate)
}

pub(crate) fn await_open_gate(path: &Path) -> io::Result<()> {
    use std::io::Read;

    let mut stream = UnixStream::connect(path)?;
    let mut status = [0];
    stream.read_exact(&mut status)?;
    if status == [1] {
        Ok(())
    } else {
        Err(io::Error::other("Shell creation failed before attachment"))
    }
}

pub(crate) fn open_exact_run(
    desktop_entry: Option<&str>,
    shell_id: &str,
    expected_run_id: &str,
    title: &str,
    takeover: bool,
) -> Result<(), Box<dyn Error>> {
    open_with_expected_run(
        desktop_entry,
        shell_id,
        None,
        title,
        takeover,
        Some(expected_run_id),
    )
}

pub(crate) fn open_remote_exact_run(
    desktop_entry: Option<&str>,
    node_id: &str,
    shell_id: &str,
    expected_run_id: &str,
    title: &str,
    takeover: bool,
) -> Result<(), Box<dyn Error>> {
    open_with_expected_run(
        desktop_entry,
        shell_id,
        Some(node_id),
        title,
        takeover,
        Some(expected_run_id),
    )
}

pub(crate) fn open_command(
    desktop_entry: Option<&str>,
    cwd: &Path,
    title: &str,
    command: &[String],
) -> Result<(), Box<dyn Error>> {
    let (program, arguments) =
        terminal_command_arguments(command).ok_or("terminal command cannot be empty")?;
    launch(
        desktop_entry,
        title,
        Some(cwd),
        OsStr::new(program),
        &arguments,
        None,
        false,
    )
}

pub(crate) fn open_agent_session(
    desktop_entry: Option<&str>,
    node_id: Option<&str>,
    session_id: &str,
    title: &str,
) -> Result<(), Box<dyn Error>> {
    let executable = attachment_executable()?;
    let mut arguments = vec!["__resume-session".into(), session_id.into()];
    if let Some(node_id) = node_id {
        arguments.extend(["--node".into(), node_id.into()]);
    }
    launch(
        desktop_entry,
        title,
        None,
        executable.as_os_str(),
        &arguments,
        None,
        false,
    )
}

fn terminal_command_arguments(command: &[String]) -> Option<(&OsStr, Vec<OsString>)> {
    let (program, arguments) = command.split_first()?;
    (!program.is_empty()).then(|| {
        (
            OsStr::new(program),
            arguments.iter().map(OsString::from).collect(),
        )
    })
}

fn open_with_expected_run(
    desktop_entry: Option<&str>,
    shell_id: &str,
    node_id: Option<&str>,
    title: &str,
    takeover: bool,
    expected_run_id: Option<&str>,
) -> Result<(), Box<dyn Error>> {
    open_with_expected_run_and_placement(
        desktop_entry,
        shell_id,
        node_id,
        title,
        takeover,
        expected_run_id,
        None,
    )
}

fn open_with_expected_run_and_placement(
    desktop_entry: Option<&str>,
    shell_id: &str,
    node_id: Option<&str>,
    title: &str,
    takeover: bool,
    expected_run_id: Option<&str>,
    placement: Option<HyprlandPlacement<'_>>,
) -> Result<(), Box<dyn Error>> {
    let executable = attachment_executable()?;
    let mut arguments = attachment_arguments(shell_id, node_id, expected_run_id);
    if takeover {
        arguments.push("--takeover".into());
    }
    launch(
        desktop_entry,
        title,
        None,
        executable.as_os_str(),
        &arguments,
        placement,
        true,
    )
}

pub fn open_node_add(desktop_entry: Option<&str>) -> Result<(), Box<dyn Error>> {
    let arguments = node_add_arguments();
    launch(
        desktop_entry,
        "Add Boomux Node",
        None,
        attachment_executable()?.as_os_str(),
        &arguments,
        None,
        false,
    )
}

pub fn open_node_upgrade(desktop_entry: Option<&str>, node_id: &str) -> Result<(), Box<dyn Error>> {
    let arguments = node_upgrade_arguments(node_id);
    launch(
        desktop_entry,
        "Upgrade Boomux Node",
        None,
        attachment_executable()?.as_os_str(),
        &arguments,
        None,
        false,
    )
}

fn node_add_arguments() -> [OsString; 1] {
    ["__guided-node-add".into()]
}

fn node_upgrade_arguments(node_id: &str) -> [OsString; 2] {
    ["__guided-node-upgrade".into(), node_id.into()]
}

pub(crate) fn open_plain(desktop_entry: Option<&str>) -> Result<(), Box<dyn Error>> {
    let preference = desktop_entry.map(TemporaryPreference::new).transpose()?;
    let selected = if desktop_entry.is_some() {
        selected_with_preference(desktop_entry, preference.as_ref())?
    } else {
        "configured terminal".to_owned()
    };
    let output = configured_command(preference.as_ref())
        .arg(r"--print-cmd=\0")
        .output()?;
    if !output.status.success() {
        let message = String::from_utf8_lossy(&output.stderr);
        return Err(format!("could not prepare {selected}: {}", message.trim()).into());
    }
    spawn_terminal(parse_nul_arguments(&output.stdout)?, None, &selected, None)
}

fn launch(
    desktop_entry: Option<&str>,
    title: &str,
    cwd: Option<&Path>,
    program: &OsStr,
    arguments: &[OsString],
    placement: Option<HyprlandPlacement<'_>>,
    reuse_existing: bool,
) -> Result<(), Box<dyn Error>> {
    let preference = desktop_entry.map(TemporaryPreference::new).transpose()?;
    let selected = if desktop_entry.is_some() {
        selected_with_preference(desktop_entry, preference.as_ref())?
    } else {
        "configured terminal".to_owned()
    };
    let resolved_title = placement
        .as_ref()
        .map(|placement| crate::hyprland::shell_title(placement.node_id, placement.shell_id, title))
        .transpose()?;
    let launch_lock = placement
        .as_ref()
        .map(|_| crate::hyprland::acquire_launch_lock())
        .transpose()?;
    if reuse_existing && let Some(placement) = placement.as_ref() {
        match crate::hyprland::place_existing_shell_windows(
            placement.node_id,
            placement.shell_id,
            placement.workspace_id,
        ) {
            Ok(crate::hyprland::ShellWindowReuse::Reused) => return Ok(()),
            Ok(crate::hyprland::ShellWindowReuse::ExistingPlacementFailed(error)) => {
                return Err(format!("existing terminal could not be moved: {error}").into());
            }
            Ok(crate::hyprland::ShellWindowReuse::Absent) => {}
            Err(error) => return Err(format!("could not inspect Hyprland windows: {error}").into()),
        }
    }
    let launch_token = if let Some(placement) = placement.as_ref() {
        match crate::hyprland::acquire_launch_token(placement.node_id, placement.shell_id)? {
            crate::hyprland::LaunchToken::Acquired(token) => Some(token),
            crate::hyprland::LaunchToken::Held if reuse_existing => return Ok(()),
            crate::hyprland::LaunchToken::Held => {
                return Err("a terminal launch is already pending for this Shell".into());
            }
        }
    } else {
        None
    };
    let mut resolver = configured_command(preference.as_ref());
    resolver
        .arg(r"--print-cmd=\0")
        .arg(format!(
            "--title={}",
            resolved_title.as_deref().unwrap_or(title)
        ))
        .arg("--")
        .arg(program)
        .args(arguments);
    if let Some(cwd) = cwd {
        resolver.current_dir(cwd);
    }
    let output = resolver.output()?;
    if !output.status.success() {
        let message = String::from_utf8_lossy(&output.stderr);
        return Err(format!("could not prepare {selected}: {}", message.trim()).into());
    }
    spawn_terminal(
        parse_nul_arguments(&output.stdout)?,
        cwd,
        &selected,
        launch_token.as_ref(),
    )?;
    if let Some(placement) = placement
        && let Err(error) = crate::hyprland::wait_and_place_shell_window(
            placement.node_id,
            placement.shell_id,
            placement.workspace_id,
        )
    {
        eprintln!("warning: terminal opened without Hyprland placement: {error}");
    }
    drop(launch_lock);
    Ok(())
}

fn spawn_terminal(
    arguments: Vec<OsString>,
    cwd: Option<&Path>,
    selected: &str,
    launch_token: Option<&fs::File>,
) -> Result<(), Box<dyn Error>> {
    let (program, arguments) = arguments
        .split_first()
        .ok_or("xdg-terminal-exec returned an empty terminal command")?;
    let mut command = Command::new(program);
    command
        .args(arguments)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    if let Some(cwd) = cwd {
        command.current_dir(cwd);
    }
    let inherited_token = launch_token
        .map(|token| {
            let descriptor = unsafe { libc::fcntl(token.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 3) };
            if descriptor == -1 {
                Err(io::Error::last_os_error())
            } else {
                // The duplicate stays close-on-exec in the multithreaded parent.
                Ok(unsafe { fs::File::from_raw_fd(descriptor) })
            }
        })
        .transpose()?;
    let inherited_token_fd = inherited_token.as_ref().map(AsRawFd::as_raw_fd);
    // The child has not executed user code yet; `setsid` detaches the terminal
    // window, and only this child clears close-on-exec for the launch token.
    unsafe {
        command.pre_exec(move || {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            if let Some(descriptor) = inherited_token_fd {
                let flags = libc::fcntl(descriptor, libc::F_GETFD);
                if flags == -1
                    || libc::fcntl(descriptor, libc::F_SETFD, flags & !libc::FD_CLOEXEC) == -1
                {
                    return Err(std::io::Error::last_os_error());
                }
            }
            Ok(())
        });
    }
    command
        .spawn()
        .map_err(|error| format!("could not launch {selected}: {error}"))?;
    drop(inherited_token);
    Ok(())
}

fn attachment_arguments(
    shell_id: &str,
    node_id: Option<&str>,
    expected_run_id: Option<&str>,
) -> Vec<OsString> {
    let mut arguments = vec!["__attach".into(), shell_id.into()];
    if let Some(node_id) = node_id {
        arguments.extend(["--node".into(), node_id.into()]);
    }
    if let Some(expected_run_id) = expected_run_id {
        arguments.extend(["--expected-run-id".into(), expected_run_id.into()]);
    } else {
        arguments.push("--restart-exited".into());
    }
    arguments
}

fn waiting_attachment_arguments(
    gate: &Path,
    shell_id: &str,
    node_id: Option<&str>,
) -> Vec<OsString> {
    let mut arguments = vec![
        "__await-attach".into(),
        shell_id.into(),
        "--gate".into(),
        gate.as_os_str().to_owned(),
    ];
    if let Some(node_id) = node_id {
        arguments.extend(["--node".into(), node_id.into()]);
    }
    arguments
}

fn attachment_executable() -> io::Result<PathBuf> {
    Ok(select_attachment_executable(env::current_exe()?))
}

fn select_attachment_executable(current: PathBuf) -> PathBuf {
    if current.exists() {
        return current;
    }
    current
        .to_str()
        .and_then(|path| path.strip_suffix(" (deleted)"))
        .map(PathBuf::from)
        .filter(|path| path.exists())
        .unwrap_or(current)
}

fn selected_with_preference(
    requested: Option<&str>,
    preference: Option<&TemporaryPreference>,
) -> Result<String, Box<dyn Error>> {
    if let Some(entry) = requested {
        validate_desktop_entry(entry)?;
    }
    let output = configured_command(preference)
        .arg("--print-id")
        .output()
        .map_err(|error| format!("could not run xdg-terminal-exec: {error}"))?;
    if !output.status.success() {
        let message = String::from_utf8_lossy(&output.stderr);
        return Err(format!("could not resolve an Omarchy terminal: {}", message.trim()).into());
    }
    let selected = String::from_utf8(output.stdout)?.trim().to_owned();
    if selected.is_empty() {
        return Err("xdg-terminal-exec did not select a terminal".into());
    }
    if let Some(requested) = requested
        && selected != requested
    {
        return Err(format!(
            "terminal desktop entry {requested:?} is unavailable (xdg-terminal-exec selected {selected:?} instead)"
        )
        .into());
    }
    Ok(selected)
}

fn configured_command(preference: Option<&TemporaryPreference>) -> Command {
    let mut command = Command::new("xdg-terminal-exec");
    if let Some(preference) = preference {
        command
            .env("XDG_CONFIG_HOME", &preference.directory)
            .env("XTE_CACHE_ENABLED", "false");
    }
    command
}

fn parse_nul_arguments(output: &[u8]) -> Result<Vec<OsString>, Box<dyn Error>> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStringExt;

        let arguments = output
            .split(|byte| *byte == 0)
            .filter(|argument| !argument.is_empty())
            .map(|argument| OsString::from_vec(argument.to_vec()))
            .collect::<Vec<_>>();
        if arguments.is_empty() {
            Err("xdg-terminal-exec returned an empty terminal command".into())
        } else {
            Ok(arguments)
        }
    }
    #[cfg(not(unix))]
    {
        let _ = output;
        Err("terminal launching is supported only on Unix".into())
    }
}

struct TemporaryPreference {
    directory: PathBuf,
}

impl TemporaryPreference {
    fn new(entry: &str) -> Result<Self, Box<dyn Error>> {
        validate_desktop_entry(entry)?;
        let parent = env::var_os("XDG_RUNTIME_DIR")
            .map(PathBuf::from)
            .filter(|path| path.is_absolute())
            .unwrap_or_else(env::temp_dir);
        let id = TEMPORARY_DIRECTORY_ID.fetch_add(1, Ordering::Relaxed);
        let directory = parent.join(format!("boomux-{}-{id}", process::id()));
        fs::create_dir(&directory)?;
        if let Err(error) = fs::write(directory.join("xdg-terminals.list"), format!("{entry}\n")) {
            let _ = fs::remove_dir(&directory);
            return Err(error.into());
        }
        Ok(Self { directory })
    }
}

impl Drop for TemporaryPreference {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.directory);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsStr;

    #[test]
    fn validates_desktop_entry_ids_and_actions() {
        assert!(validate_desktop_entry("Alacritty.desktop").is_ok());
        assert!(validate_desktop_entry("terminal.desktop:new-window").is_ok());

        for invalid in [
            "",
            "Alacritty",
            "-bad.desktop",
            "bad/name.desktop",
            "bad.desktop:",
        ] {
            assert!(validate_desktop_entry(invalid).is_err());
        }
    }

    #[test]
    fn parses_nul_delimited_commands() {
        let arguments = parse_nul_arguments(b"alacritty\0-e\0boomux\0__attach\0").unwrap();

        assert_eq!(
            arguments,
            ["alacritty", "-e", "boomux", "__attach"]
                .map(OsStr::new)
                .map(OsStr::to_owned)
        );
    }

    #[test]
    fn exact_run_attachment_arguments_never_enable_restart() {
        assert_eq!(
            attachment_arguments("shell-1", None, Some("run-1")),
            ["__attach", "shell-1", "--expected-run-id", "run-1"]
                .map(OsStr::new)
                .map(OsStr::to_owned)
        );
        assert_eq!(
            attachment_arguments("shell-1", None, None),
            ["__attach", "shell-1", "--restart-exited"]
                .map(OsStr::new)
                .map(OsStr::to_owned)
        );
        assert_eq!(
            attachment_arguments("shell-1", Some("node-1"), Some("run-1")),
            [
                "__attach",
                "shell-1",
                "--node",
                "node-1",
                "--expected-run-id",
                "run-1",
            ]
            .map(OsStr::new)
            .map(OsStr::to_owned)
        );
    }

    #[test]
    fn waiting_attachment_is_exact_and_node_qualified() {
        assert_eq!(
            waiting_attachment_arguments(Path::new("/run/user/1/gate"), "shell-1", Some("node-1")),
            [
                "__await-attach",
                "shell-1",
                "--gate",
                "/run/user/1/gate",
                "--node",
                "node-1",
            ]
            .map(OsStr::new)
            .map(OsStr::to_owned)
        );
    }

    #[test]
    fn open_gate_releases_only_after_success() {
        let path = env::temp_dir().join(format!("boomux-open-gate-{}", process::id()));
        let listener = UnixListener::bind(&path).unwrap();
        listener.set_nonblocking(true).unwrap();
        let gate = OpenGate {
            listener,
            path: path.clone(),
            released: false,
        };
        let waiter = thread::spawn(move || await_open_gate(&path));

        gate.release().unwrap();
        waiter.join().unwrap().unwrap();
    }

    #[test]
    fn bound_open_gate_is_owner_only_and_removed_on_drop() {
        use std::os::unix::fs::MetadataExt;

        let path = env::temp_dir().join(format!("boomux-owned-open-gate-{}", process::id()));
        let gate = OpenGate::bind(path.clone()).unwrap();
        let metadata = fs::symlink_metadata(&path).unwrap();
        assert_eq!(metadata.mode() & 0o777, 0o600);

        drop(gate);
        assert!(!path.exists());
    }

    #[test]
    fn dropped_open_gate_rejects_attachment() {
        use std::io::Read;
        use std::sync::mpsc;

        let path = env::temp_dir().join(format!("boomux-failed-open-gate-{}", process::id()));
        let listener = UnixListener::bind(&path).unwrap();
        listener.set_nonblocking(true).unwrap();
        let gate = OpenGate {
            listener,
            path: path.clone(),
            released: false,
        };
        let (connected_tx, connected_rx) = mpsc::channel();
        let waiter = thread::spawn(move || {
            let mut stream = UnixStream::connect(path).unwrap();
            connected_tx.send(()).unwrap();
            let mut status = [1];
            stream.read_exact(&mut status).unwrap();
            status
        });
        connected_rx.recv().unwrap();

        drop(gate);
        assert_eq!(waiter.join().unwrap(), [0]);
    }

    #[test]
    fn guided_node_add_uses_only_the_hidden_wrapper_argument() {
        assert_eq!(node_add_arguments(), [OsString::from("__guided-node-add")]);
    }

    #[test]
    fn guided_node_upgrade_preserves_the_exact_node_id_argument() {
        assert_eq!(
            node_upgrade_arguments("node;still-one-argument"),
            [
                OsString::from("__guided-node-upgrade"),
                OsString::from("node;still-one-argument"),
            ]
        );
    }

    #[test]
    fn external_terminal_command_preserves_exact_arguments() {
        let command = ["opencode", "--session", "ses_exact; rm -rf /"].map(str::to_owned);
        let (program, arguments) = terminal_command_arguments(&command).unwrap();

        assert_eq!(program, OsStr::new("opencode"));
        assert_eq!(
            arguments,
            ["--session", "ses_exact; rm -rf /"]
                .map(OsStr::new)
                .map(OsStr::to_owned)
        );
        assert!(terminal_command_arguments(&[]).is_none());
        assert!(terminal_command_arguments(&[String::new()]).is_none());
    }

    #[test]
    fn attachment_finds_installed_binary_after_replacement() {
        let installed = PathBuf::from("/bin/sh");

        assert_eq!(
            select_attachment_executable(PathBuf::from("/bin/sh (deleted)")),
            installed
        );
    }
}

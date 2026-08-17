use std::env;
use std::error::Error;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::io;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{self, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};

static TEMPORARY_DIRECTORY_ID: AtomicU64 = AtomicU64::new(0);

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
    )
}

fn node_add_arguments() -> [OsString; 1] {
    ["__guided-node-add".into()]
}

fn launch(
    desktop_entry: Option<&str>,
    title: &str,
    cwd: Option<&Path>,
    program: &OsStr,
    arguments: &[OsString],
) -> Result<(), Box<dyn Error>> {
    let preference = desktop_entry.map(TemporaryPreference::new).transpose()?;
    let selected = selected_with_preference(desktop_entry, preference.as_ref())?;
    let mut resolver = configured_command(preference.as_ref());
    resolver
        .arg(r"--print-cmd=\0")
        .arg(format!("--title={title}"))
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
    let arguments = parse_nul_arguments(&output.stdout)?;
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
    // The child has not executed user code yet; `setsid` detaches the terminal
    // window from the dashboard process and its controlling terminal.
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                Err(std::io::Error::last_os_error())
            } else {
                Ok(())
            }
        });
    }
    command
        .spawn()
        .map_err(|error| format!("could not launch {selected}: {error}"))?;
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
    fn guided_node_add_uses_only_the_hidden_wrapper_argument() {
        assert_eq!(node_add_arguments(), [OsString::from("__guided-node-add")]);
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

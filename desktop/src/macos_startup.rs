//! Finder's environment is not a terminal login environment. Bootstrap in a
//! separate invocation before GPUI exists; pass only PATH/SHELL/HOME to the GUI.
//! No environment or shell output is written to disk.
use std::ffi::{OsStr, OsString};
use std::io::{self, Read};
use std::os::fd::AsRawFd;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const SYSTEM_PATH: &str = "/usr/bin:/bin:/usr/sbin:/sbin";
const OUTPUT_LIMIT: usize = 64 * 1024;
const PATH_MARKER: &[u8] = b"\0BOOMUX_PATH\0";
const WARNING_FLAG: &str = "--macos-startup-warning";

#[derive(Debug, PartialEq)]
enum Failure {
    Spawn,
    Io,
    Timeout,
    OutputLimit,
    Exit,
}

/// A private process group and nonblocking pipe bound both the subprocess and
/// its output. Observe exit without reaping so group cleanup cannot hit a reused
/// PID. A grandchild holding stdout open cannot keep startup waiting forever.
fn bounded_output(command: &mut Command, timeout: Duration) -> Result<Vec<u8>, Failure> {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    command.process_group(0);
    let mut child = command.spawn().map_err(|_| Failure::Spawn)?;
    let result = (|| {
        let mut stdout = child.stdout.take().ok_or(Failure::Io)?;
        let fd = stdout.as_raw_fd();
        // SAFETY: stdout owns this live descriptor for the duration of this call.
        unsafe {
            let flags = libc::fcntl(fd, libc::F_GETFL);
            if flags < 0 || libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) < 0 {
                return Err(Failure::Io);
            }
        }
        let deadline = Instant::now() + timeout;
        let mut output = Vec::new();
        let mut exited = false;
        loop {
            let mut buffer = [0; 4096];
            loop {
                if Instant::now() >= deadline {
                    return Err(Failure::Timeout);
                }
                match stdout.read(&mut buffer) {
                    Ok(0) => break,
                    Ok(length) => {
                        if output.len() + length > OUTPUT_LIMIT {
                            return Err(Failure::OutputLimit);
                        }
                        output.extend_from_slice(&buffer[..length]);
                    }
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                    Err(_) => return Err(Failure::Io),
                }
            }
            // Drain once more after exit so the final write isn't lost, but do
            // not wait for EOF from descendants spawned by a startup script.
            if exited {
                return Ok(output);
            }
            // SAFETY: waitid initializes siginfo, WNOWAIT keeps our child PID
            // reserved until the process group is killed and the child reaped.
            let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
            let result = unsafe {
                libc::waitid(
                    libc::P_PID,
                    child.id(),
                    &mut info,
                    libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
                )
            };
            if result < 0 {
                if io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(Failure::Io);
            }
            exited = unsafe { info.si_pid() } != 0;
            if !exited {
                std::thread::sleep(Duration::from_millis(10));
            }
        }
    })();
    // SAFETY: the unreaped child still anchors this private process group.
    unsafe {
        libc::kill(-(child.id() as i32), libc::SIGKILL);
    }
    let status = child.wait().map_err(|_| Failure::Io)?;
    result.and_then(|output| {
        if status.success() {
            Ok(output)
        } else {
            Err(Failure::Exit)
        }
    })
}

fn login_path(
    shell: &OsStr,
    home: &Path,
    inherited: Option<&OsStr>,
    timeout: Duration,
) -> Result<OsString, Failure> {
    let mut command = Command::new(shell);
    // A fixed command, never interpolated paths or arguments. printenv emits
    // only the exported PATH (including fish's colon-joined path variable).
    command
        .args(["-ilc", "printf '\\0BOOMUX_PATH\\0'; /usr/bin/printenv PATH"])
        .current_dir(home)
        .env("SHELL", shell)
        .env("HOME", home)
        .env("PATH", inherited.unwrap_or(OsStr::new(SYSTEM_PATH)));
    let output = bounded_output(&mut command, timeout)?;
    parse_login_path(&output).ok_or(Failure::Io)
}

fn parse_login_path(output: &[u8]) -> Option<OsString> {
    let start = output
        .windows(PATH_MARKER.len())
        .rposition(|bytes| bytes == PATH_MARKER)?
        + PATH_MARKER.len();
    let value = output[start..].strip_suffix(b"\n")?;
    (!value.is_empty() && !value.contains(&0)).then(|| OsString::from_vec(value.to_vec()))
}

fn selected_shell(explicit: Option<&OsStr>, account: Option<&OsStr>) -> OsString {
    explicit
        .filter(|value| !value.is_empty())
        .or_else(|| account.filter(|value| Path::new(value).is_absolute()))
        .unwrap_or(OsStr::new("/bin/zsh"))
        .to_owned()
}

/// Retain an explicit/custom launch PATH ahead of discovered entries. Finder's
/// standard system-only PATH is fallback; otherwise it masks version managers.
fn combined_path(bundle: &Path, inherited: Option<&OsStr>, login: Option<&OsStr>) -> OsString {
    let inherited = inherited.filter(|path| !path.is_empty());
    let custom = inherited.filter(|path| *path != OsStr::new(SYSTEM_PATH));
    let mut paths = vec![bundle.to_path_buf()];
    for value in [
        custom,
        login,
        inherited,
        Some(OsStr::new("/opt/homebrew/bin:/usr/local/bin")),
        Some(OsStr::new(SYSTEM_PATH)),
    ]
    .into_iter()
    .flatten()
    {
        for path in std::env::split_paths(value) {
            // Never add the working directory implicitly to a GUI tool search.
            if path.is_absolute() && !paths.contains(&path) {
                paths.push(path);
            }
        }
    }
    std::env::join_paths(paths).expect("PATH components cannot contain colons")
}

#[cfg(target_os = "macos")]
fn account_environment() -> Option<(OsString, PathBuf)> {
    use std::ffi::CStr;
    // This helper is called in its own bounded process: even a directory-service
    // lookup that stops responding cannot hold up the GUI indefinitely.
    let mut record: libc::passwd = unsafe { std::mem::zeroed() };
    let mut result = std::ptr::null_mut();
    let mut buffer = vec![0; OUTPUT_LIMIT];
    let code = unsafe {
        libc::getpwuid_r(
            libc::getuid(),
            &mut record,
            buffer.as_mut_ptr(),
            buffer.len(),
            &mut result,
        )
    };
    if code != 0 || result.is_null() || record.pw_shell.is_null() || record.pw_dir.is_null() {
        return None;
    }
    // SAFETY: getpwuid_r succeeded; these NUL-terminated strings live in buffer.
    let shell = unsafe { CStr::from_ptr(record.pw_shell) }.to_bytes();
    let home = unsafe { CStr::from_ptr(record.pw_dir) }.to_bytes();
    Some((
        OsString::from_vec(shell.to_vec()),
        PathBuf::from(OsString::from_vec(home.to_vec())),
    ))
}

struct LaunchEnvironment {
    shell: Option<OsString>,
    home: Option<OsString>,
    path: Option<OsString>,
}

fn prepare_gui(
    executable: &Path,
    args: impl Iterator<Item = OsString>,
    environment: LaunchEnvironment,
) -> Command {
    let bundle = executable.parent().expect("executable has a directory");
    let explicit_shell = environment.shell.filter(|s| !s.is_empty());
    let explicit_home = environment.home.filter(|s| !s.is_empty());
    let inherited_path = environment.path;
    let mut warnings = Vec::new();
    let account = if explicit_shell.is_none() || explicit_home.is_none() {
        bounded_output(
            Command::new(executable).arg("--macos-account-environment"),
            Duration::from_secs(2),
        )
        .ok()
        .and_then(|output| {
            let mut values = output.split(|byte| *byte == 0);
            Some((
                OsString::from_vec(values.next()?.to_vec()),
                OsString::from_vec(values.next()?.to_vec()),
            ))
        })
    } else {
        None
    };
    if explicit_shell.is_none()
        && account
            .as_ref()
            .is_none_or(|(shell, _)| !Path::new(shell).is_absolute())
    {
        warnings.push("account");
    }
    let shell = selected_shell(
        explicit_shell.as_deref(),
        account.as_ref().map(|(shell, _)| shell.as_os_str()),
    );
    let home = explicit_home
        .or_else(|| account.map(|(_, home)| home))
        .map(PathBuf::from)
        .filter(|home| home.is_absolute() && home.is_dir());
    let login = home.as_deref().and_then(|home| {
        match login_path(
            &shell,
            home,
            inherited_path.as_deref(),
            Duration::from_secs(3),
        ) {
            Ok(path) => Some(path),
            Err(_) => {
                warnings.push("environment");
                None
            }
        }
    });
    if home.is_none() {
        warnings.push("home");
    }
    let path = combined_path(bundle, inherited_path.as_deref(), login.as_deref());
    let mut daemon = Command::new(bundle.join("boomux"));
    daemon
        .args(["daemon", "start"])
        .env("PATH", &path)
        .env("SHELL", &shell);
    if let Some(home) = &home {
        daemon.env("HOME", home).current_dir(home);
    }
    if bounded_output(&mut daemon, Duration::from_secs(10)).is_err() {
        warnings.push("daemon");
    }
    let mut gui = Command::new(executable);
    gui.args(args).env("PATH", path).env("SHELL", shell);
    if let Some(home) = home {
        gui.env("HOME", &home).current_dir(home);
    }
    if !warnings.is_empty() {
        gui.arg(WARNING_FLAG).arg(warnings.join(","));
    }
    gui
}

#[cfg(target_os = "macos")]
fn bootstrap(args: impl Iterator<Item = OsString>) -> io::Error {
    let executable = match std::env::current_exe() {
        Ok(path) => path,
        Err(error) => return error,
    };
    prepare_gui(
        &executable,
        args,
        LaunchEnvironment {
            shell: std::env::var_os("SHELL"),
            home: std::env::var_os("HOME"),
            path: std::env::var_os("PATH"),
        },
    )
    .exec()
}

/// The bundled CLI is an exact sibling, even when it is missing. Do not silently
/// start a different installed CLI from PATH after a broken/incomplete download.
#[cfg(target_os = "macos")]
pub fn cli_program() -> OsString {
    std::env::current_exe()
        .ok()
        .and_then(|path| {
            let directory = path.parent()?;
            (directory.file_name()? == "MacOS" && directory.parent()?.file_name()? == "Contents")
                .then(|| directory.join("boomux").into_os_string())
        })
        .unwrap_or_else(|| "boomux".into())
}

pub fn warning(args: impl Iterator<Item = OsString>) -> Option<String> {
    let mut args = args;
    while let Some(arg) = args.next() {
        if arg != WARNING_FLAG {
            continue;
        }
        let value = args.next()?;
        let mut messages = Vec::new();
        for code in value.to_str()?.split(',').take(4) {
            let message = match code {
                "account" => {
                    "Could not read your account shell; using /bin/zsh. Set SHELL when launching from Terminal to override it."
                }
                "environment" => {
                    "Login-shell PATH discovery failed or exceeded its 3-second limit. Using the launch PATH and standard locations. Check shell startup files, then quit and reopen Boomux."
                }
                "home" => {
                    "Your home directory is unavailable. Check HOME, then quit and reopen Boomux."
                }
                "daemon" => {
                    "Boomux could not start its bundled daemon. It will retry in the background. Check the bundled CLI and configuration; the app can stay open while you fix them."
                }
                _ => continue,
            };
            if !messages.contains(&message) {
                messages.push(message);
            }
        }
        return (!messages.is_empty()).then(|| messages.join(" "));
    }
    None
}

#[cfg(target_os = "macos")]
pub fn dispatch() {
    match std::env::args_os().nth(1).as_deref() {
        Some(value) if value == "--macos-account-environment" => {
            use std::io::Write;
            let Some((shell, home)) = account_environment() else {
                std::process::exit(1)
            };
            let mut output = io::stdout().lock();
            for value in [shell.as_os_str(), home.as_os_str()] {
                if output
                    .write_all(value.as_bytes())
                    .and_then(|()| output.write_all(&[0]))
                    .is_err()
                {
                    std::process::exit(1);
                }
            }
            std::process::exit(0);
        }
        Some(value) if value == "--macos-launch" => {
            let error = bootstrap(std::env::args_os().skip(2));
            eprintln!("Could not launch Boomux Desktop: {error}");
            let _ = Command::new("/usr/bin/osascript").args(["-e", "display alert \"Boomux could not open\" message \"The bundled Desktop executable could not start. Reinstall the app and try again.\" as critical"]).status();
            std::process::exit(1);
        }
        _ => (),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fixture(PathBuf);

    impl Fixture {
        fn new() -> Self {
            use std::sync::atomic::{AtomicUsize, Ordering};
            static NEXT: AtomicUsize = AtomicUsize::new(0);
            let root = std::env::temp_dir().join(format!(
                "boomux startup {} {}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir(&root).unwrap();
            Self(root)
        }

        fn script(&self, name: &str, script: &str) -> PathBuf {
            use std::os::unix::fs::PermissionsExt;
            let path = self.0.join(name);
            std::fs::write(&path, format!("#!/bin/sh\n{script}\n")).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
            path
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn bootstrap_uses_account_shell_and_keeps_gui_after_missing_cli() {
        let fixture = Fixture::new();
        let shell = fixture.script(
            "preferred shell",
            "printf 'startup noise\\n\\0BOOMUX_PATH\\0/login tools:/bin\\n'",
        );
        // The helper receives an exact argv, with no path-to-source conversion.
        let executable = fixture.script("boomux-desktop", "test \"$1\" = --macos-account-environment || exit 1\nprintf '%s\\0%s\\0' \"$(dirname \"$0\")/preferred shell\" \"$(dirname \"$0\")\"");
        let gui = prepare_gui(
            &executable,
            ["--update-ready".into(), "path with spaces".into()].into_iter(),
            LaunchEnvironment {
                shell: None,
                home: None,
                path: Some(SYSTEM_PATH.into()),
            },
        );
        let environment = gui.get_envs().collect::<std::collections::HashMap<_, _>>();
        assert_eq!(environment.len(), 3); // HOME, SHELL, PATH only
        assert_eq!(environment[OsStr::new("SHELL")], Some(shell.as_os_str()));
        assert_eq!(environment[OsStr::new("HOME")], Some(fixture.0.as_os_str()));
        assert_eq!(
            std::env::split_paths(environment[OsStr::new("PATH")].unwrap()).next(),
            Some(fixture.0.clone())
        );
        assert_eq!(
            gui.get_args().collect::<Vec<_>>(),
            [
                OsStr::new("--update-ready"),
                OsStr::new("path with spaces"),
                OsStr::new(WARNING_FLAG),
                OsStr::new("daemon")
            ]
        );
        assert_eq!(gui.get_current_dir(), Some(fixture.0.as_path()));
        // Daemon errors also preserve the GUI and give a recoverable warning.
        fixture.script("boomux", "exit 7");
        let gui = prepare_gui(
            &executable,
            [].into_iter(),
            LaunchEnvironment {
                shell: Some(shell.into_os_string()),
                home: Some(fixture.0.clone().into_os_string()),
                path: None,
            },
        );
        assert_eq!(gui.get_args().last(), Some(OsStr::new("daemon")));
    }

    #[test]
    fn bootstrap_preserves_explicit_shell_home_and_custom_path() {
        let fixture = Fixture::new();
        fixture.script("boomux", "test \"$1 $2\" = 'daemon start'");
        let shell = fixture.script("custom shell", "test \"$1\" = -ilc || exit 1\nexport SECRET_FROM_STARTUP=not-imported\nprintf '\\0BOOMUX_PATH\\0/login tools:/bin\\n'");
        let executable = fixture.script("boomux-desktop", "exit 99"); // no account lookup
        let gui = prepare_gui(
            &executable,
            ["argument; literal".into()].into_iter(),
            LaunchEnvironment {
                shell: Some(shell.clone().into_os_string()),
                home: Some(fixture.0.clone().into_os_string()),
                path: Some("/explicit/bin:/bin".into()),
            },
        );
        assert_eq!(
            gui.get_args().collect::<Vec<_>>(),
            [OsStr::new("argument; literal")]
        );
        let environment = gui.get_envs().collect::<std::collections::HashMap<_, _>>();
        assert_eq!(environment.len(), 3);
        assert_eq!(environment[OsStr::new("SHELL")], Some(shell.as_os_str()));
        let path = environment[OsStr::new("PATH")].unwrap();
        assert_eq!(
            std::env::split_paths(path).nth(1),
            Some(PathBuf::from("/explicit/bin"))
        );
        assert!(std::env::split_paths(path).any(|p| p == Path::new("/login tools")));
    }

    #[test]
    fn failing_login_shell_still_produces_a_gui_command() {
        let fixture = Fixture::new();
        fixture.script("boomux", "exit 0");
        let shell = fixture.script("bad shell", "exit 1");
        let gui = prepare_gui(
            &fixture.0.join("boomux-desktop"),
            [].into_iter(),
            LaunchEnvironment {
                shell: Some(shell.into_os_string()),
                home: Some(fixture.0.clone().into_os_string()),
                path: None,
            },
        );
        assert_eq!(gui.get_args().last(), Some(OsStr::new("environment")));
    }

    #[test]
    fn explicit_shell_precedes_account_shell_and_fallback() {
        assert_eq!(
            selected_shell(
                Some(OsStr::new("/custom shell/fish")),
                Some(OsStr::new("/bin/bash"))
            ),
            "/custom shell/fish"
        );
        for shell in ["/bin/bash", "/bin/zsh", "/opt/homebrew/bin/fish"] {
            assert_eq!(selected_shell(None, Some(OsStr::new(shell))), shell);
        }
        assert_eq!(
            selected_shell(Some(OsStr::new("")), Some(OsStr::new("relative"))),
            "/bin/zsh"
        );
        assert_eq!(selected_shell(None, None), "/bin/zsh");
    }

    #[test]
    fn bundled_cli_precedes_custom_path_and_login_tools() {
        let path = combined_path(
            Path::new("/Applications/Boomux app/Contents/MacOS"),
            Some(OsStr::new("/custom/bin:/bin")),
            Some(OsStr::new("/version manager/bin:/custom/bin:/bin")),
        );
        let paths = std::env::split_paths(&path).collect::<Vec<_>>();
        assert_eq!(
            paths[..4],
            [
                PathBuf::from("/Applications/Boomux app/Contents/MacOS"),
                PathBuf::from("/custom/bin"),
                PathBuf::from("/bin"),
                PathBuf::from("/version manager/bin")
            ]
        );
        assert_eq!(
            paths
                .iter()
                .filter(|path| *path == Path::new("/custom/bin"))
                .count(),
            1
        );
        let finder = combined_path(
            Path::new("/bundle"),
            Some(OsStr::new(SYSTEM_PATH)),
            Some(OsStr::new("/version manager/bin:/bin")),
        );
        assert_eq!(
            std::env::split_paths(&finder).nth(1).unwrap(),
            Path::new("/version manager/bin")
        );
        let fallback = combined_path(
            Path::new("/bundle"),
            Some(OsStr::new(":relative:/bin")),
            None,
        );
        assert!(std::env::split_paths(&fallback).all(|path| path.is_absolute()));
    }

    #[test]
    fn login_output_ignores_startup_noise_and_keeps_raw_path_bytes() {
        assert_eq!(
            parse_login_path(b"startup noise\n\0BOOMUX_PATH\0/a b:/bin\n"),
            Some("/a b:/bin".into())
        );
        assert_eq!(
            parse_login_path(b"\0BOOMUX_PATH\0/raw\xff\n")
                .unwrap()
                .as_bytes(),
            b"/raw\xff"
        );
        for output in [
            b"/bin\n".as_slice(),
            b"\0BOOMUX_PATH\0\n",
            b"\0BOOMUX_PATH\0/a\0b\n",
        ] {
            assert!(parse_login_path(output).is_none());
        }
    }

    #[test]
    fn bounded_shell_failure_timeout_and_output_flood() {
        let run = |script| {
            bounded_output(
                Command::new("/bin/sh").args(["-c", script]),
                Duration::from_millis(100),
            )
        };
        assert_eq!(run("printf success"), Ok(b"success".to_vec()));
        assert_eq!(run("exit 4"), Err(Failure::Exit));
        assert_eq!(run("sleep 20"), Err(Failure::Timeout));
        assert_eq!(
            run("while :; do printf '0123456789012345678901234567890123456789'; done"),
            Err(Failure::OutputLimit)
        );
        assert_eq!(run("sleep 20 & printf complete"), Ok(b"complete".to_vec()));
        assert_eq!(
            bounded_output(
                &mut Command::new("/no/boomux-helper-here"),
                Duration::from_millis(100)
            ),
            Err(Failure::Spawn)
        );
    }

    #[test]
    fn login_shell_inherits_overrides_and_exports_only_path() {
        // bash is present in Linux and macOS CI; the login shell runs without a
        // terminal and the result contains PATH alone, never a bulk env dump.
        let path = login_path(
            OsStr::new("/bin/bash"),
            Path::new("/tmp"),
            Some(OsStr::new("/a path with spaces:/usr/bin:/bin")),
            Duration::from_secs(3),
        )
        .unwrap();
        assert!(!path.as_bytes().contains(&0));
        assert!(std::env::split_paths(&path).any(|p| p == Path::new("/bin")));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn native_account_and_login_profiles_supply_shell_and_tools() {
        let (shell, home) = account_environment().expect("current macOS account");
        assert!(Path::new(&shell).is_absolute());
        assert!(home.is_absolute());
        let fixture = Fixture::new();
        for (shell, profile) in [("/bin/bash", ".bash_profile"), ("/bin/zsh", ".zprofile")] {
            std::fs::write(
                fixture.0.join(profile),
                "export PATH=\"/login profile tools:$PATH\"\n",
            )
            .unwrap();
            let path = login_path(
                OsStr::new(shell),
                &fixture.0,
                Some(OsStr::new(SYSTEM_PATH)),
                Duration::from_secs(3),
            )
            .unwrap();
            assert!(
                std::env::split_paths(&path).any(|path| path == Path::new("/login profile tools")),
                "{shell} did not read {profile}"
            );
        }
    }

    #[test]
    fn startup_warning_is_bounded_static_and_recoverable() {
        let warning = warning(
            [
                OsString::from("--update-ready"),
                "some path".into(),
                WARNING_FLAG.into(),
                "environment,daemon,unknown".into(),
            ]
            .into_iter(),
        )
        .unwrap();
        assert!(warning.contains("3-second"));
        assert!(warning.contains("retry in the background"));
        assert!(!warning.contains("unknown"));
        assert!(self::warning([WARNING_FLAG.into(), "user output".into()].into_iter()).is_none());
    }
}

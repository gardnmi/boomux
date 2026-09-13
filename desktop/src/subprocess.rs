//! Bounded subprocess execution without a GNU coreutils dependency on macOS.
use std::ffi::OsStr;
use std::process::Command;

pub fn command(seconds: u32, program: impl AsRef<OsStr>) -> Command {
    #[cfg(target_os = "linux")]
    {
        let mut command = Command::new("timeout");
        command
            .args(["--kill-after=1s", &format!("{seconds}s")])
            .arg(program);
        command
    }
    #[cfg(target_os = "macos")]
    {
        let mut command = Command::new(std::env::current_exe().expect("Desktop executable path"));
        command
            .arg("--bounded-command")
            .arg(seconds.to_string())
            .arg(program);
        command
    }
}

/// Runs before GPUI or any threads are initialized. The child's unreaped PID
/// anchors its private process group until cleanup has completed.
pub fn dispatch() -> bool {
    if std::env::args_os().nth(1).as_deref() != Some(OsStr::new("--bounded-command")) {
        return false;
    }
    use std::os::unix::process::{CommandExt, ExitStatusExt};
    use std::time::{Duration, Instant};
    let mut args = std::env::args_os().skip(2);
    let seconds = args
        .next()
        .and_then(|s| s.to_str()?.parse::<u64>().ok())
        .filter(|n| (1..=300).contains(n))
        .unwrap_or(10);
    let Some(program) = args.next() else {
        std::process::exit(125);
    };
    let mut command = Command::new(program);
    command.args(args);
    unsafe {
        command.pre_exec(|| {
            if libc::setpgid(0, 0) < 0 {
                Err(std::io::Error::last_os_error())
            } else {
                Ok(())
            }
        });
    }
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(127);
        }
    };
    let deadline = Instant::now() + Duration::from_secs(seconds);
    loop {
        match child.try_wait() {
            Ok(Some(status)) => std::process::exit(
                status
                    .code()
                    .unwrap_or_else(|| 128 + status.signal().unwrap_or(1)),
            ),
            Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                std::process::exit(125);
            }
            Ok(None) => (),
        }
        if Instant::now() >= deadline {
            unsafe {
                libc::kill(-(child.id() as i32), libc::SIGKILL);
            }
            let _ = child.wait();
            std::process::exit(124);
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

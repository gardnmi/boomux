//! Event-driven parent-death cleanup for Kiro on Darwin, which has no PDEATHSIG.
use std::env;
use std::ffi::OsStr;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::process::{Child, Command, ExitCode, ExitStatus};

const FLAG: &str = "__macos-parent-guard";

pub fn wrap(command: Command) -> io::Result<Command> {
    let mut guarded = Command::new(boomux::platform::current_executable()?);
    guarded.args([FLAG, &std::process::id().to_string()]);
    guarded.arg(command.get_program()).args(command.get_args());
    for (name, value) in command.get_envs() {
        if let Some(value) = value {
            guarded.env(name, value);
        } else {
            guarded.env_remove(name);
        }
    }
    if let Some(cwd) = command.get_current_dir() {
        guarded.current_dir(cwd);
    }
    Ok(guarded)
}

struct ManagedChild(Child);
impl Drop for ManagedChild {
    fn drop(&mut self) {
        // Until wait/try_wait reaps it, this exact child anchors its PID.
        if self.0.try_wait().ok().flatten().is_none() {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
}

fn register_exit(queue: &OwnedFd, pid: u32) -> io::Result<()> {
    let event = libc::kevent {
        ident: pid as usize,
        filter: libc::EVFILT_PROC,
        flags: libc::EV_ADD | libc::EV_ENABLE,
        fflags: libc::NOTE_EXIT,
        data: 0,
        udata: std::ptr::null_mut(),
    };
    if unsafe {
        libc::kevent(
            queue.as_raw_fd(),
            &event,
            1,
            std::ptr::null_mut(),
            0,
            std::ptr::null(),
        )
    } < 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn run() -> io::Result<ExitStatus> {
    let mut args = env::args_os().skip(2);
    let parent: u32 = args
        .next()
        .and_then(|s| s.to_str()?.parse().ok())
        .filter(|pid| *pid > 1 && *pid == unsafe { libc::getppid() } as u32)
        .ok_or_else(|| io::Error::other("parent guard has no live originating parent"))?;
    let program = args
        .next()
        .ok_or_else(|| io::Error::other("parent guard has no command"))?;
    let raw = unsafe { libc::kqueue() };
    if raw < 0 {
        return Err(io::Error::last_os_error());
    }
    let queue = unsafe { OwnedFd::from_raw_fd(raw) };
    if unsafe { libc::fcntl(raw, libc::F_SETFD, libc::FD_CLOEXEC) } < 0 {
        return Err(io::Error::last_os_error());
    }
    register_exit(&queue, parent)?;
    if unsafe { libc::getppid() } as u32 != parent {
        return Err(io::Error::other(
            "originating parent exited before child launch",
        ));
    }
    // Stay alive to reap the child if the foreground group receives a signal.
    // Restore the original dispositions in the actual command before exec.
    let dispositions = [libc::SIGINT, libc::SIGTERM, libc::SIGHUP]
        .map(|signal| (signal, unsafe { libc::signal(signal, libc::SIG_IGN) }));
    let mut command = Command::new(program);
    command.args(args);
    unsafe {
        command.pre_exec(move || {
            for (signal, handler) in dispositions {
                if libc::signal(signal, handler) == libc::SIG_ERR {
                    return Err(io::Error::last_os_error());
                }
            }
            Ok(())
        });
    }
    let mut child = ManagedChild(command.spawn()?);
    if let Err(error) = register_exit(&queue, child.0.id()) {
        if let Some(status) = child.0.try_wait()? {
            return Ok(status);
        }
        return Err(error);
    }
    loop {
        if let Some(status) = child.0.try_wait()? {
            return Ok(status);
        }
        let mut events: [libc::kevent; 2] = unsafe { std::mem::zeroed() };
        let count = unsafe {
            libc::kevent(
                raw,
                std::ptr::null(),
                0,
                events.as_mut_ptr(),
                2,
                std::ptr::null(),
            )
        };
        if count < 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(error);
        }
        if events[..count as usize]
            .iter()
            .any(|event| event.ident == parent as usize)
        {
            child.0.kill()?;
            return child.0.wait();
        }
    }
}

pub fn dispatch() -> Option<ExitCode> {
    if env::args_os().nth(1).as_deref() != Some(OsStr::new(FLAG)) {
        return None;
    }
    Some(match run() {
        Ok(status) => ExitCode::from(
            status
                .code()
                .unwrap_or_else(|| 128 + status.signal().unwrap_or(1)) as u8,
        ),
        Err(error) => {
            eprintln!("boomux: parent guard: {error}");
            ExitCode::FAILURE
        }
    })
}

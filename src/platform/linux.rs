use super::ProcessSnapshot;
use std::fs;
use std::io;
use std::os::fd::{AsRawFd, BorrowedFd, FromRawFd, OwnedFd};
use std::path::PathBuf;

pub type ProcessHandle = OwnedFd;

pub fn default_runtime_root() -> io::Result<PathBuf> {
    Err(io::Error::new(
        io::ErrorKind::NotFound,
        "XDG_RUNTIME_DIR is not set",
    ))
}

pub fn process_snapshot(pid: u32) -> io::Result<ProcessSnapshot> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat"))?;
    let fields = stat
        .rsplit_once(')')
        .ok_or_else(|| io::Error::other("invalid process stat"))?
        .1
        .split_whitespace();
    let number = |index: usize| -> io::Result<i64> {
        fields
            .clone()
            .nth(index)
            .and_then(|s| s.parse().ok())
            .ok_or_else(|| io::Error::other("invalid process stat field"))
    };
    Ok(ProcessSnapshot {
        start_time: number(19)? as u64,
        session: number(3)? as i32,
        group: number(2)? as i32,
        foreground_group: number(5)? as i32,
    })
}

pub fn process_ids() -> io::Result<Vec<u32>> {
    Ok(fs::read_dir("/proc")?
        .filter_map(Result::ok)
        .filter_map(|entry| entry.file_name().to_str()?.parse().ok())
        .collect())
}

pub fn process_executable(pid: u32) -> io::Result<PathBuf> {
    fs::read_link(format!("/proc/{pid}/exe"))
}

pub fn process_cwd(pid: u32) -> io::Result<PathBuf> {
    fs::read_link(format!("/proc/{pid}/cwd"))
}

pub fn process_argv(pid: u32) -> io::Result<Vec<Vec<u8>>> {
    Ok(fs::read(format!("/proc/{pid}/cmdline"))?
        .split(|b| *b == 0)
        .filter(|s| !s.is_empty())
        .map(<[u8]>::to_vec)
        .collect())
}

pub fn process_environment_value(pid: u32, name: &[u8]) -> io::Result<Option<Vec<u8>>> {
    let bytes = fs::read(format!("/proc/{pid}/environ"))?;
    Ok(bytes.split(|b| *b == 0).find_map(|s| {
        let i = s.iter().position(|b| *b == b'=')?;
        (s[..i] == *name).then(|| s[i + 1..].to_vec())
    }))
}

pub fn process_name(pid: u32) -> io::Result<Vec<u8>> {
    use std::io::Read;
    let mut bytes = Vec::new();
    fs::File::open(format!("/proc/{pid}/comm"))?
        .take(4097)
        .read_to_end(&mut bytes)?;
    Ok(bytes)
}

pub fn open_process(pid: u32) -> io::Result<ProcessHandle> {
    let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) };
    if fd < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(unsafe { OwnedFd::from_raw_fd(fd as i32) })
    }
}

pub fn import_process(fd: OwnedFd, pid: u32) -> io::Result<ProcessHandle> {
    let info = fs::read_to_string(format!("/proc/self/fdinfo/{}", fd.as_raw_fd()))?;
    let actual = info
        .lines()
        .find_map(|line| line.strip_prefix("Pid:")?.trim().parse::<u32>().ok());
    if actual != Some(pid) {
        return Err(io::Error::other(
            "transferred process handle does not match its PID",
        ));
    }
    Ok(fd)
}

pub fn signal_process(fd: BorrowedFd<'_>, signal: i32) -> io::Result<()> {
    let result = unsafe {
        libc::syscall(
            libc::SYS_pidfd_send_signal,
            fd.as_raw_fd(),
            signal,
            std::ptr::null::<libc::siginfo_t>(),
            0,
        )
    };
    if result == -1 {
        let error = io::Error::last_os_error();
        if error.raw_os_error() != Some(libc::ESRCH) {
            return Err(error);
        }
    }
    Ok(())
}

pub fn executable_path(file: &fs::File) -> io::Result<PathBuf> {
    Ok(PathBuf::from(format!("/proc/self/fd/{}", file.as_raw_fd())))
}

pub fn native_executable_magic(magic: [u8; 4]) -> bool {
    magic == *b"\x7fELF"
}

pub fn rename_noreplace(
    from_dir: i32,
    from: &std::ffi::CStr,
    to_dir: i32,
    to: &std::ffi::CStr,
) -> io::Result<()> {
    if unsafe {
        libc::renameat2(
            from_dir,
            from.as_ptr(),
            to_dir,
            to.as_ptr(),
            libc::RENAME_NOREPLACE,
        )
    } < 0
    {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

pub fn current_executable() -> io::Result<PathBuf> {
    std::env::current_exe()
}

pub fn process_wait_fd(handle: &ProcessHandle) -> i32 {
    handle.as_raw_fd()
}

use super::ProcessSnapshot;
use std::ffi::{CStr, OsString};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::mem::{MaybeUninit, size_of};
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStringExt;
use std::os::unix::fs::{FileExt, MetadataExt, OpenOptionsExt};
use std::path::PathBuf;

const MAX_PROCESSES: usize = 262_144;
const MAX_PROCESS_ARGS: usize = 4 * 1024 * 1024;

// Darwin's PROC_PIDUNIQIDENTIFIERINFO ABI (proc_info_private.h). uniqueid
// survives exec; idversion changes on exec and is checked by audit-token signals.
#[repr(C)]
#[derive(Clone, Copy)]
struct UniqueInfo {
    uuid: [u8; 16],
    unique: u64,
    parent_unique: u64,
    version: i32,
    parent_version: i32,
    reserved: [u64; 2],
}

fn pid_info<T>(pid: u32, flavor: i32) -> io::Result<T> {
    let mut value = MaybeUninit::<T>::zeroed();
    let n = unsafe {
        libc::proc_pidinfo(
            pid as i32,
            flavor,
            0,
            value.as_mut_ptr().cast(),
            size_of::<T>() as i32,
        )
    };
    if n != size_of::<T>() as i32 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { value.assume_init() })
}

fn unique_info(pid: u32) -> io::Result<UniqueInfo> {
    pid_info(pid, 17)
}

pub fn default_runtime_root() -> io::Result<PathBuf> {
    Ok(PathBuf::from(format!("/tmp/boomux-{}", unsafe {
        libc::geteuid()
    })))
}

pub fn prepare_default_runtime_root() -> io::Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    let path = default_runtime_root()?;
    match fs::DirBuilder::new().mode(0o700).create(&path) {
        Ok(()) => (),
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => (),
        Err(e) => return Err(e),
    }
    let meta = fs::symlink_metadata(path)?;
    if !meta.is_dir() || meta.uid() != unsafe { libc::geteuid() } || meta.mode() & 0o077 != 0 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "unsafe macOS runtime root",
        ));
    }
    Ok(())
}

pub fn process_snapshot(pid: u32) -> io::Result<ProcessSnapshot> {
    let info: libc::proc_bsdinfo = pid_info(pid, libc::PROC_PIDTBSDINFO)?;
    let session = unsafe { libc::getsid(pid as i32) };
    if session < 0 || info.pbi_status == 5 {
        return Err(io::Error::from_raw_os_error(libc::ESRCH));
    }
    Ok(ProcessSnapshot {
        start_time: info.pbi_start_tvsec * 1_000_000 + info.pbi_start_tvusec,
        session,
        group: info.pbi_pgid as i32,
        foreground_group: info.e_tpgid as i32,
    })
}

pub fn process_ids() -> io::Result<Vec<u32>> {
    let mut ids = vec![0u32; MAX_PROCESSES];
    let n = unsafe { libc::proc_listpids(1, 0, ids.as_mut_ptr().cast(), (ids.len() * 4) as i32) };
    if n <= 0 {
        return Err(io::Error::last_os_error());
    }
    if n as usize >= ids.len() * 4 {
        return Err(io::Error::other("process inventory exceeds bound"));
    }
    ids.truncate(n as usize / 4);
    ids.retain(|pid| *pid != 0);
    Ok(ids)
}

pub fn process_executable(pid: u32) -> io::Result<PathBuf> {
    let mut bytes = vec![0u8; 4096];
    let n =
        unsafe { libc::proc_pidpath(pid as i32, bytes.as_mut_ptr().cast(), bytes.len() as u32) };
    if n <= 0 {
        return Err(io::Error::last_os_error());
    }
    bytes.truncate(bytes.iter().position(|b| *b == 0).unwrap_or(n as usize));
    Ok(PathBuf::from(OsString::from_vec(bytes)))
}

pub fn process_cwd(pid: u32) -> io::Result<PathBuf> {
    let info: libc::proc_vnodepathinfo = pid_info(pid, libc::PROC_PIDVNODEPATHINFO)?;
    let bytes: Vec<u8> = info
        .pvi_cdir
        .vip_path
        .iter()
        .flatten()
        .map(|c| *c as u8)
        .take_while(|c| *c != 0)
        .collect();
    Ok(PathBuf::from(OsString::from_vec(bytes)))
}

struct ProcessArguments {
    argv: Vec<Vec<u8>>,
    environment: Vec<Vec<u8>>,
}

fn process_arguments(pid: u32) -> io::Result<ProcessArguments> {
    let mut mib = [libc::CTL_KERN, libc::KERN_PROCARGS2, pid as i32];
    // Darwin rejects buffers larger than ARG_MAX, even when the process has
    // only a few arguments. Query the required length before allocating.
    let mut len = 0usize;
    if unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            3,
            std::ptr::null_mut(),
            &mut len,
            std::ptr::null_mut(),
            0,
        )
    } < 0
    {
        return Err(io::Error::last_os_error());
    }
    if !(4..=MAX_PROCESS_ARGS).contains(&len) {
        return Err(io::Error::other("process arguments exceed bound"));
    }
    let mut bytes = vec![0u8; len];
    let rc = unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            3,
            bytes.as_mut_ptr().cast(),
            &mut len,
            std::ptr::null_mut(),
            0,
        )
    };
    if rc < 0 {
        return Err(io::Error::last_os_error());
    }
    bytes.truncate(len);
    if bytes.len() < 4 {
        return Err(io::Error::other("missing process arguments"));
    }
    let argc = i32::from_ne_bytes(bytes[..4].try_into().unwrap());
    if !(0..=65_536).contains(&argc) {
        return Err(io::Error::other("invalid argument count"));
    }
    let mut offset = 4;
    // Executable path, then alignment NULs, then exactly argc arguments.
    offset += bytes[offset..]
        .iter()
        .position(|b| *b == 0)
        .ok_or_else(|| io::Error::other("invalid process path"))?;
    while offset < bytes.len() && bytes[offset] == 0 {
        offset += 1;
    }
    let mut args = Vec::new();
    for _ in 0..argc {
        let end = offset
            + bytes[offset..]
                .iter()
                .position(|b| *b == 0)
                .ok_or_else(|| io::Error::other("truncated process arguments"))?;
        args.push(bytes[offset..end].to_vec());
        offset = end + 1;
    }
    let vars = bytes[offset..]
        .split(|b| *b == 0)
        .take_while(|s| !s.is_empty())
        .map(<[u8]>::to_vec)
        .collect();
    Ok(ProcessArguments {
        argv: args,
        environment: vars,
    })
}

pub fn process_argv(pid: u32) -> io::Result<Vec<Vec<u8>>> {
    process_arguments(pid).map(|arguments| arguments.argv)
}
pub fn process_environment_value(pid: u32, name: &[u8]) -> io::Result<Option<Vec<u8>>> {
    Ok(process_arguments(pid)?
        .environment
        .into_iter()
        .find_map(|s| {
            let i = s.iter().position(|b| *b == b'=')?;
            (s[..i] == *name).then(|| s[i + 1..].to_vec())
        }))
}
pub fn process_name(pid: u32) -> io::Result<Vec<u8>> {
    let info: libc::proc_bsdinfo = pid_info(pid, libc::PROC_PIDTBSDINFO)?;
    Ok(info
        .pbi_comm
        .iter()
        .map(|c| *c as u8)
        .take_while(|c| *c != 0)
        .collect())
}

/// The descriptor passed through handoff contains an immutable identity record;
/// the kqueue stays local to each reader and is re-created on import.
pub struct ProcessHandle {
    identity: OwnedFd,
    monitor: OwnedFd,
}
impl AsFd for ProcessHandle {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.identity.as_fd()
    }
}
impl AsRawFd for ProcessHandle {
    fn as_raw_fd(&self) -> i32 {
        self.identity.as_raw_fd()
    }
}
pub fn process_wait_fd(handle: &ProcessHandle) -> i32 {
    handle.monitor.as_raw_fd()
}
impl ProcessHandle {
    pub fn try_clone(&self) -> io::Result<Self> {
        Ok(Self {
            identity: self.identity.try_clone()?,
            monitor: self.monitor.try_clone()?,
        })
    }
}

fn identity(fd: BorrowedFd<'_>) -> io::Result<(u32, u64)> {
    let file = File::from(fd.try_clone_to_owned()?);
    let meta = file.metadata()?;
    if !meta.is_file()
        || meta.len() != 16
        || meta.uid() != unsafe { libc::geteuid() }
        || meta.mode() & 0o077 != 0
    {
        return Err(io::Error::other("invalid process identity descriptor"));
    }
    let mut bytes = [0u8; 16];
    file.read_exact_at(&mut bytes, 0)?;
    if &bytes[..4] != b"BMP1" {
        return Err(io::Error::other("invalid process identity marker"));
    }
    Ok((
        u32::from_le_bytes(bytes[4..8].try_into().unwrap()),
        u64::from_le_bytes(bytes[8..].try_into().unwrap()),
    ))
}

pub fn open_process(pid: u32) -> io::Result<ProcessHandle> {
    let info = unique_info(pid)?;
    let root = super::runtime_root()?.join("boomux");
    let path = root.join(format!(".process-{}", uuid::Uuid::new_v4()));
    let mut writer = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)?;
    let result = (|| {
        writer.write_all(b"BMP1")?;
        writer.write_all(&pid.to_le_bytes())?;
        writer.write_all(&info.unique.to_le_bytes())?;
        let fd = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&path)?;
        import_process(fd.into(), pid)
    })();
    let _ = fs::remove_file(path);
    result
}

pub fn import_process(fd: OwnedFd, pid: u32) -> io::Result<ProcessHandle> {
    let (actual, unique) = identity(fd.as_fd())?;
    if actual != pid || unique_info(pid)?.unique != unique {
        return Err(io::Error::other("transferred process identity changed"));
    }
    let raw = unsafe { libc::kqueue() };
    if raw < 0 {
        return Err(io::Error::last_os_error());
    }
    let monitor = unsafe { OwnedFd::from_raw_fd(raw) };
    if unsafe { libc::fcntl(raw, libc::F_SETFD, libc::FD_CLOEXEC) } < 0 {
        return Err(io::Error::last_os_error());
    }
    let event = libc::kevent {
        ident: pid as usize,
        filter: libc::EVFILT_PROC,
        flags: libc::EV_ADD | libc::EV_ENABLE,
        fflags: libc::NOTE_EXIT,
        data: 0,
        udata: std::ptr::null_mut(),
    };
    if unsafe { libc::kevent(raw, &event, 1, std::ptr::null_mut(), 0, std::ptr::null()) } < 0 {
        return Err(io::Error::last_os_error());
    }
    if unique_info(pid)?.unique != unique {
        return Err(io::Error::other(
            "process changed during monitor registration",
        ));
    }
    Ok(ProcessHandle {
        identity: fd,
        monitor,
    })
}

pub fn signal_process(fd: BorrowedFd<'_>, signal: i32) -> io::Result<()> {
    let (pid, expected) = identity(fd)?;
    let info = match unique_info(pid) {
        Ok(info) if info.unique == expected => info,
        Ok(_) => return Ok(()),
        Err(error) if matches!(error.raw_os_error(), Some(libc::ESRCH)) => return Ok(()),
        Err(error) => return Err(error),
    };
    // Resolve Apple's identity-checked operation explicitly. Never fall back to
    // kill(pid), which could signal a recycled PID after the identity check.
    type Signal = unsafe extern "C" fn(*mut [u32; 8], i32) -> i32;
    let symbol =
        unsafe { libc::dlsym(libc::RTLD_DEFAULT, c"proc_signal_with_audittoken".as_ptr()) };
    if symbol.is_null() {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "macOS lacks identity-checked process signaling",
        ));
    }
    let signal_fn: Signal = unsafe { std::mem::transmute(symbol) };
    let mut token = [0u32; 8];
    token[5] = pid;
    token[7] = info.version as u32;
    let rc = unsafe { signal_fn(&mut token, signal) };
    if rc != 0 && rc != libc::ESRCH {
        return Err(io::Error::from_raw_os_error(rc));
    }
    Ok(())
}

pub fn executable_path(file: &File) -> io::Result<PathBuf> {
    let mut path = [0i8; 1024];
    if unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETPATH, path.as_mut_ptr()) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(PathBuf::from(OsString::from_vec(
        unsafe { CStr::from_ptr(path.as_ptr()) }.to_bytes().to_vec(),
    )))
}
pub fn native_executable_magic(magic: [u8; 4]) -> bool {
    matches!(
        magic,
        [0xcf, 0xfa, 0xed, 0xfe]
            | [0xfe, 0xed, 0xfa, 0xcf]
            | [0xca, 0xfe, 0xba, 0xbe]
            | [0xca, 0xfe, 0xba, 0xbf]
    )
}

pub fn validate_pty(fd: &OwnedFd, pid: u32) -> io::Result<()> {
    let mut name = [0i8; 128];
    if unsafe {
        libc::ioctl(
            fd.as_raw_fd(),
            libc::TIOCPTYGNAME as libc::c_ulong,
            name.as_mut_ptr(),
        )
    } < 0
    {
        return Err(io::Error::other(
            "transferred descriptor is not a PTY master",
        ));
    }
    let name = unsafe { CStr::from_ptr(name.as_ptr()) };
    let path = PathBuf::from(OsString::from_vec(name.to_bytes().to_vec()));
    let device = fs::metadata(path)?.rdev();
    let info: libc::proc_bsdinfo = pid_info(pid, libc::PROC_PIDTBSDINFO)?;
    if device != info.e_tdev as u64 || unsafe { libc::getsid(pid as i32) } != pid as i32 {
        return Err(io::Error::other(
            "transferred PTY does not belong to the Shell session",
        ));
    }
    Ok(())
}

fn lsof(arguments: &[&str]) -> io::Result<Vec<u8>> {
    use std::io::Read;
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};
    let mut child = Command::new("/usr/sbin/lsof")
        .args(arguments)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?;
    let stdout = child.stdout.take().unwrap();
    let reader = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        stdout
            .take(1_048_577)
            .read_to_end(&mut bytes)
            .map(|_| bytes)
    });
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        if child.try_wait()?.is_some() {
            break;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            let _ = reader.join();
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "process descriptor inspection timed out",
            ));
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let bytes = reader
        .join()
        .map_err(|_| io::Error::other("process descriptor reader failed"))??;
    if bytes.len() > 1_048_576 {
        return Err(io::Error::other(
            "process descriptor inventory exceeds bound",
        ));
    }
    Ok(bytes)
}

pub fn listener_belongs_to_session(port: u16, session: u32) -> bool {
    let Ok(bytes) = lsof(&["-nP", "-i", &format!("TCP:{port}"), "-sTCP:LISTEN", "-Fp"]) else {
        return false;
    };
    bytes
        .split(|b| *b == b'\n')
        .filter_map(|line| {
            std::str::from_utf8(line.strip_prefix(b"p")?)
                .ok()?
                .parse::<u32>()
                .ok()
        })
        .any(|pid| process_snapshot(pid).is_ok_and(|s| s.session == session as i32))
}

pub fn rename_noreplace(from_dir: i32, from: &CStr, to_dir: i32, to: &CStr) -> io::Result<()> {
    if unsafe {
        libc::renameatx_np(
            from_dir,
            from.as_ptr(),
            to_dir,
            to.as_ptr(),
            libc::RENAME_EXCL,
        )
    } < 0
    {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

pub fn peer_uid(stream: &std::os::unix::net::UnixStream) -> io::Result<u32> {
    let mut uid = 0;
    let mut gid = 0;
    if unsafe { libc::getpeereid(stream.as_raw_fd(), &mut uid, &mut gid) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(uid)
}

pub fn peer_credentials(stream: &std::os::unix::net::UnixStream) -> io::Result<(u32, u32)> {
    let mut uid = 0;
    let mut gid = 0;
    let mut pid = 0i32;
    let mut size = size_of::<i32>() as libc::socklen_t;
    if unsafe { libc::getpeereid(stream.as_raw_fd(), &mut uid, &mut gid) } < 0
        || unsafe {
            libc::getsockopt(
                stream.as_raw_fd(),
                0,
                libc::LOCAL_PEERPID,
                (&mut pid as *mut i32).cast(),
                &mut size,
            )
        } < 0
        || size as usize != size_of::<i32>()
        || pid <= 0
    {
        return Err(io::Error::other("cannot verify daemon socket peer"));
    }
    Ok((pid as u32, uid))
}

pub fn daemon_listener_holder(path: &std::path::Path, uid: u32) -> io::Result<u32> {
    let path = path
        .to_str()
        .ok_or_else(|| io::Error::other("invalid socket path"))?;
    let bytes = lsof(&["-nP", "-a", "-U", "-Fpn", "--", path])?;
    let mut candidates = Vec::new();
    for pid in bytes.split(|b| *b == b'\n').filter_map(|line| {
        std::str::from_utf8(line.strip_prefix(b"p")?)
            .ok()?
            .parse::<u32>()
            .ok()
    }) {
        let Ok(info) = pid_info::<libc::proc_bsdinfo>(pid, libc::PROC_PIDTBSDINFO) else {
            continue;
        };
        if info.pbi_uid != uid {
            continue;
        }
        let Ok(args) = process_argv(pid) else {
            continue;
        };
        if args
            .windows(2)
            .any(|a| a[0] == b"daemon" && (a[1] == b"run" || a[1] == b"receive-handoff"))
        {
            candidates.push(pid);
        }
    }
    candidates.sort_unstable();
    candidates.dedup();
    if candidates.len() != 1 {
        return Err(io::Error::other("daemon listener has no unique owner"));
    }
    Ok(candidates[0])
}

/// Darwin cannot exec an open file descriptor. A private hard link pins the
/// inspected inode across pathname replacement. Cross-filesystem linking fails
/// before ownership transfer instead of falling back to a mutable pathname.
pub struct ExecutablePin {
    pub path: PathBuf,
    pub original: PathBuf,
    armed: bool,
}
impl ExecutablePin {
    pub fn prepare(file: &File) -> io::Result<Self> {
        let original = executable_path(file)?;
        let root = super::runtime_root()?.join("boomux");
        let path = root.join(format!(".exec-{}", uuid::Uuid::new_v4()));
        fs::hard_link(&original, &path)?;
        let pin = Self {
            path,
            original,
            armed: true,
        };
        let pinned = file.metadata()?;
        let linked = fs::symlink_metadata(&pin.path)?;
        if !linked.is_file() || pinned.dev() != linked.dev() || pinned.ino() != linked.ino() {
            return Err(io::Error::other(
                "replacement changed before executable pinning",
            ));
        }
        Ok(pin)
    }
    pub fn commit(&mut self) {
        self.armed = false;
    }
}
impl Drop for ExecutablePin {
    fn drop(&mut self) {
        if self.armed {
            let _ = fs::remove_file(&self.path);
        }
    }
}

fn is_executable_pin(path: &std::path::Path) -> bool {
    path.file_name()
        .and_then(|s| s.to_str())
        .and_then(|s| s.strip_prefix(".exec-"))
        .is_some_and(|s| uuid::Uuid::parse_str(s).is_ok())
}

pub fn current_executable() -> io::Result<PathBuf> {
    let actual = std::env::current_exe()?;
    if is_executable_pin(&actual) {
        let path = std::env::args_os()
            .next()
            .map(PathBuf::from)
            .filter(|p| p.is_absolute())
            .ok_or_else(|| io::Error::other("missing original daemon executable path"))?;
        return Ok(path);
    }
    Ok(actual)
}

pub fn daemon_executable_path(pid: u32) -> io::Result<PathBuf> {
    let actual = process_executable(pid)?;
    if is_executable_pin(&actual) {
        let args = process_argv(pid)?;
        if args.len() == 5 && args[1] == b"daemon" && args[2] == b"receive-handoff" {
            let original = PathBuf::from(OsString::from_vec(args[0].clone()));
            if original.is_absolute() {
                return Ok(original);
            }
        }
        return Err(io::Error::other("invalid pinned daemon command"));
    }
    Ok(actual)
}

pub fn running_executable_pin() -> io::Result<Option<ExecutablePin>> {
    let path = std::env::current_exe()?;
    let root = super::runtime_root()?.join("boomux").canonicalize()?;
    if is_executable_pin(&path) && path.parent() == Some(root.as_path()) {
        Ok(Some(ExecutablePin {
            original: current_executable()?,
            path,
            armed: true,
        }))
    } else {
        Ok(None)
    }
}

/// Called only after acquiring the cold-start daemon ownership lock. No live
/// daemon can own these crash leftovers. Work and retained files stay bounded.
pub fn remove_stale_executable_pins(root: &std::path::Path) -> io::Result<()> {
    let current = std::env::current_exe()?;
    let mut count = 0;
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        if !is_executable_pin(&entry.path()) || entry.path() == current {
            continue;
        }
        count += 1;
        if count > 1024 {
            return Err(io::Error::other("too many stale executable pins"));
        }
        let meta = fs::symlink_metadata(entry.path())?;
        if !meta.is_file() || meta.uid() != unsafe { libc::geteuid() } {
            return Err(io::Error::other("unsafe stale executable pin"));
        }
        fs::remove_file(entry.path())?;
    }
    Ok(())
}

/// Darwin exposes SO_ACCEPTCONN through proc_pidfdinfo, not getsockopt.
/// The public proc_info.h prefix is sufficient; reserve bounded space for its
/// trailing protocol union and reject short/incompatible replies.
pub fn validate_listener(fd: BorrowedFd<'_>) -> io::Result<()> {
    #[repr(C)]
    struct FileInfo {
        open_flags: u32,
        status: u32,
        offset: i64,
        kind: i32,
        guard_flags: u32,
    }
    #[repr(C)]
    struct SocketPrefix {
        file: FileInfo,
        stat: libc::vinfo_stat,
        socket: u64,
        pcb: u64,
        kind: i32,
        protocol: i32,
        family: i32,
        options: i16,
        linger: i16,
    }
    #[repr(C)]
    struct Buffer {
        prefix: SocketPrefix,
        remaining: [u64; 256],
    }
    let mut buffer = MaybeUninit::<Buffer>::zeroed();
    let count = unsafe {
        libc::proc_pidfdinfo(
            libc::getpid(),
            fd.as_raw_fd(),
            3,
            buffer.as_mut_ptr().cast(),
            size_of::<Buffer>() as i32,
        )
    };
    if count < size_of::<SocketPrefix>() as i32 {
        return Err(io::Error::other(
            "cannot inspect transferred listener state",
        ));
    }
    let buffer = unsafe { buffer.assume_init() };
    if buffer.prefix.kind != libc::SOCK_STREAM
        || buffer.prefix.family != libc::AF_UNIX
        || i32::from(buffer.prefix.options) & libc::SO_ACCEPTCONN == 0
    {
        return Err(io::Error::other(
            "transferred descriptor is not a Unix listener",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn executable_alias_runs_the_pinned_mach_o() {
        prepare_default_runtime_root().unwrap();
        let root = super::super::runtime_root().unwrap().join("boomux");
        fs::create_dir_all(root).unwrap();
        let executable = File::open(std::env::current_exe().unwrap()).unwrap();
        let pin = ExecutablePin::prepare(&executable).unwrap();
        let output = std::process::Command::new(&pin.path)
            .arg("--help")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "pinned executable: {:?}; {}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        );
        let alias = pin.path.clone();
        drop(pin);
        assert!(!alias.exists());
    }
    #[test]
    fn identity_transfer_and_signaling_preserve_exact_process() {
        prepare_default_runtime_root().unwrap();
        let root = super::super::runtime_root().unwrap().join("boomux");
        fs::create_dir_all(root).unwrap();
        let mut child = std::process::Command::new("/bin/sleep")
            .arg("30")
            .spawn()
            .unwrap();
        let handle = open_process(child.id()).unwrap();
        assert_eq!(
            process_argv(child.id()).unwrap(),
            [b"/bin/sleep".to_vec(), b"30".to_vec()]
        );
        let mut idle = libc::pollfd {
            fd: process_wait_fd(&handle),
            events: libc::POLLIN,
            revents: 0,
        };
        assert_eq!(unsafe { libc::poll(&mut idle, 1, 0) }, 0);
        let duplicate = handle.as_fd().try_clone_to_owned().unwrap();
        let imported = import_process(duplicate, child.id()).unwrap();
        assert!(
            import_process(
                handle.as_fd().try_clone_to_owned().unwrap(),
                std::process::id()
            )
            .is_err()
        );
        signal_process(imported.as_fd(), libc::SIGKILL).unwrap();
        assert!(!child.wait().unwrap().success());
        let mut event = libc::pollfd {
            fd: process_wait_fd(&imported),
            events: libc::POLLIN,
            revents: 0,
        };
        assert_eq!(unsafe { libc::poll(&mut event, 1, 1000) }, 1);
        assert_ne!(event.revents & libc::POLLIN, 0);
        // A stale identity must not turn into a PID-only kill.
        signal_process(handle.as_fd(), libc::SIGKILL).unwrap();
    }
}

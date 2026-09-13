//! Host-local OS operations. These do not own Boomux resource or lifecycle state.
use std::io;
use std::path::PathBuf;

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
pub use linux::*;
#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
pub use macos::*;

#[derive(Debug)]
pub struct ProcessSnapshot {
    pub start_time: u64,
    pub session: i32,
    pub group: i32,
    pub foreground_group: i32,
}

pub fn runtime_root() -> io::Result<PathBuf> {
    std::env::var_os("BOOMUX_RUNTIME_DIR")
        .or_else(|| std::env::var_os("XDG_RUNTIME_DIR"))
        .map(PathBuf::from)
        .map(Ok)
        .unwrap_or_else(default_runtime_root)
}

/// Enumerate relative to an already validated directory descriptor. Names and
/// metadata never resolve through a pathname that could be exchanged by rename.
pub fn directory_entries(directory: i32, limit: usize) -> io::Result<Vec<(String, libc::stat)>> {
    use std::ffi::CStr;
    let duplicate = unsafe { libc::fcntl(directory, libc::F_DUPFD_CLOEXEC, 0) };
    if duplicate < 0 {
        return Err(io::Error::last_os_error());
    }
    let stream = unsafe { libc::fdopendir(duplicate) };
    if stream.is_null() {
        unsafe {
            libc::close(duplicate);
        }
        return Err(io::Error::last_os_error());
    }
    struct Directory(*mut libc::DIR);
    impl Drop for Directory {
        fn drop(&mut self) {
            unsafe {
                libc::closedir(self.0);
            }
        }
    }
    let stream = Directory(stream);
    unsafe {
        libc::rewinddir(stream.0);
    }
    let mut entries = Vec::new();
    loop {
        #[cfg(target_os = "linux")]
        let errno = unsafe { libc::__errno_location() };
        #[cfg(target_os = "macos")]
        let errno = unsafe { libc::__error() };
        unsafe {
            *errno = 0;
        }
        let entry = unsafe { libc::readdir(stream.0) };
        if entry.is_null() {
            if unsafe { *errno } != 0 {
                return Err(io::Error::last_os_error());
            }
            break;
        }
        let name = unsafe { CStr::from_ptr((*entry).d_name.as_ptr()) };
        if name.to_bytes() == b"." || name.to_bytes() == b".." {
            continue;
        }
        if entries.len() >= limit {
            return Err(io::Error::other("directory exceeds entry bound"));
        }
        let mut metadata = std::mem::MaybeUninit::<libc::stat>::uninit();
        if unsafe {
            libc::fstatat(
                directory,
                name.as_ptr(),
                metadata.as_mut_ptr(),
                libc::AT_SYMLINK_NOFOLLOW,
            )
        } < 0
        {
            return Err(io::Error::last_os_error());
        }
        let name = name
            .to_str()
            .map_err(|_| io::Error::other("directory entry is not UTF-8"))?
            .to_owned();
        entries.push((name, unsafe { metadata.assume_init() }));
    }
    Ok(entries)
}

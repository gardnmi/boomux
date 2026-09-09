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

//! User-initiated updates between verified Developer ID bundles. Worker-only.
//! Versioned apps stay immutable: the old app is the rollback target and is never
//! removed, renamed, or replaced while any Desktop/daemon may be using it.
#![cfg_attr(not(target_os = "macos"), allow(dead_code))]
use semver::Version;
use serde::{Deserialize, Serialize};
use std::{
    collections::HashSet,
    fs,
    io::{Read, Write},
    os::fd::AsRawFd,
    os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt},
    os::unix::process::CommandExt,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::{Duration, Instant},
};

const BUNDLE_ID: &str = "com.boomux.desktop.preview";
const ASSET: &str = "boomux-desktop-aarch64-apple-darwin.zip";
const ARCHIVE_LIMIT: u64 = 256 * 1024 * 1024;
const PENDING: &str = ".boomux-update-pending.json";
const CURRENT: &str = ".boomux-current.json";

#[derive(Clone, Debug)]
pub struct Installation {
    root: PathBuf,
    running: PathBuf,
    version: String,
    team: String,
    identity: (u64, u64),
}
#[derive(Clone, Debug)]
pub struct Prepared {
    installation: Installation,
    release: PathBuf,
    pub version: String,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Current {
    candidate: PathBuf,
    version: String,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Pending {
    from: PathBuf,
    candidate: PathBuf,
    version: String,
}

fn stable(value: &str) -> Result<Version, String> {
    let version = Version::parse(value).map_err(|e| e.to_string())?;
    if value.len() > 64 || !version.pre.is_empty() || !version.build.is_empty() {
        return Err("Expected a stable release version".into());
    }
    Ok(version)
}
fn owned(path: &Path) -> Result<fs::Metadata, String> {
    let metadata = fs::symlink_metadata(path).map_err(|e| e.to_string())?;
    if metadata.file_type().is_symlink()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.permissions().mode() & 0o022 != 0
    {
        return Err(format!(
            "Update path must be owned by you and not writable by others: {}",
            path.display()
        ));
    }
    Ok(metadata)
}
fn private_read(path: &Path, limit: u64) -> Result<Vec<u8>, String> {
    owned(path)?;
    let file = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
        .map_err(|e| e.to_string())?;
    let meta = file.metadata().map_err(|e| e.to_string())?;
    if !meta.is_file() || meta.nlink() != 1 || meta.len() > limit {
        return Err("Invalid or oversized update file".into());
    }
    let mut bytes = Vec::new();
    file.take(limit + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| e.to_string())?;
    if bytes.len() as u64 > limit {
        return Err("Update file grew beyond its limit".into());
    }
    Ok(bytes)
}
struct BoundedCommand {
    command: Command,
    seconds: u32,
}
trait Pipe: Read + AsRawFd {}
impl<T: Read + AsRawFd> Pipe for T {}
fn run(mut specification: BoundedCommand) -> Result<String, String> {
    let command = &mut specification.command;
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0);
    let mut child = command.spawn().map_err(|e| e.to_string())?;
    let result = (|| {
        let mut streams: [Box<dyn Pipe>; 2] = [
            Box::new(child.stdout.take().ok_or("Missing stdout")?),
            Box::new(child.stderr.take().ok_or("Missing stderr")?),
        ];
        for stream in &streams {
            let fd = stream.as_raw_fd();
            // SAFETY: these are live, owned pipe descriptors.
            unsafe {
                let flags = libc::fcntl(fd, libc::F_GETFL);
                if flags < 0 || libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) < 0 {
                    return Err("Could not bound update pipes".into());
                }
            }
        }
        let deadline = Instant::now() + Duration::from_secs(u64::from(specification.seconds));
        let mut output = [Vec::new(), Vec::new()];
        let mut exited = false;
        loop {
            for (stream, bytes) in streams.iter_mut().zip(output.iter_mut()) {
                let mut buffer = [0u8; 4096];
                loop {
                    if Instant::now() >= deadline {
                        return Err("Update command timed out".into());
                    }
                    match stream.read(&mut buffer) {
                        Ok(0) => break,
                        Ok(count) => {
                            if bytes.len() + count > 65536 {
                                return Err("Update command output exceeded its limit".into());
                            }
                            bytes.extend_from_slice(&buffer[..count]);
                        }
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
                        Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                        Err(error) => return Err(error.to_string()),
                    }
                }
            }
            // Drain once after exit, without waiting for descendants to close
            // inherited pipes. WNOWAIT reserves the PID until group cleanup.
            if exited {
                return Ok(output);
            }
            let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
            let status = unsafe {
                libc::waitid(
                    libc::P_PID,
                    child.id(),
                    &mut info,
                    libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
                )
            };
            if status < 0 {
                let error = std::io::Error::last_os_error();
                if error.kind() == std::io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(error.to_string());
            }
            exited = unsafe { info.si_pid() } != 0;
            if !exited {
                std::thread::sleep(Duration::from_millis(10));
            }
        }
    })();
    // The unreaped leader anchors this group even after an early direct exit.
    unsafe {
        libc::kill(-(child.id() as i32), libc::SIGKILL);
    }
    let status = child.wait().map_err(|e| e.to_string())?;
    let [stdout, stderr] = result?;
    if !status.success() {
        return Err(format!(
            "Update command failed ({status}): {}",
            String::from_utf8_lossy(&stderr)
        ));
    }
    if stdout.is_empty() {
        Ok(String::from_utf8_lossy(&stderr).into_owned())
    } else {
        Ok(String::from_utf8_lossy(&stdout).into_owned())
    }
}
fn command(seconds: u32, program: &str, args: &[&std::ffi::OsStr]) -> BoundedCommand {
    let mut command = Command::new(program);
    command.args(args);
    BoundedCommand { command, seconds }
}
fn plist(app: &Path, key: &str) -> Result<String, String> {
    let operation = format!("Print :{key}");
    run(command(
        10,
        "/usr/libexec/PlistBuddy",
        &[
            "-c".as_ref(),
            operation.as_ref(),
            app.join("Contents/Info.plist").as_os_str(),
        ],
    ))
    .map(|s| s.trim().to_owned())
}
fn signed_identity(app: &Path) -> Result<(String, String), String> {
    owned(app)?;
    let mut remaining = vec![app.to_owned()];
    let mut count = 0;
    while let Some(path) = remaining.pop() {
        count += 1;
        if count > 20000 {
            return Err("App exceeds update file limit".into());
        }
        let metadata = owned(&path)?;
        if metadata.is_dir() {
            for entry in fs::read_dir(path).map_err(|e| e.to_string())? {
                remaining.push(entry.map_err(|e| e.to_string())?.path());
            }
        } else if !metadata.is_file() || metadata.nlink() != 1 {
            return Err("App contains a special or linked file".into());
        }
    }
    run(command(
        30,
        "/usr/bin/codesign",
        &[
            "--verify".as_ref(),
            "--deep".as_ref(),
            "--strict".as_ref(),
            app.as_os_str(),
        ],
    ))?;
    let details = run(command(
        10,
        "/usr/bin/codesign",
        &[
            "--display".as_ref(),
            "--verbose=4".as_ref(),
            app.as_os_str(),
        ],
    ))?;
    let team = details
        .lines()
        .find_map(|line| line.strip_prefix("TeamIdentifier="))
        .filter(|team| {
            team.len() == 10
                && team
                    .bytes()
                    .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit())
        })
        .ok_or("Ad-hoc apps cannot self-update; install a notarized Developer ID release first")?;
    let requirement = format!(
        "=anchor apple generic and certificate leaf[field.1.2.840.113635.100.6.1.13] exists and identifier \"{BUNDLE_ID}\" and certificate leaf[subject.OU] = \"{team}\""
    );
    run(command(
        30,
        "/usr/bin/codesign",
        &[
            "--verify".as_ref(),
            "--deep".as_ref(),
            "--strict".as_ref(),
            "-R".as_ref(),
            requirement.as_ref(),
            app.as_os_str(),
        ],
    ))?;
    run(command(
        30,
        "/usr/sbin/spctl",
        &[
            "--assess".as_ref(),
            "--type".as_ref(),
            "execute".as_ref(),
            app.as_os_str(),
        ],
    ))?;
    if plist(app, "CFBundleIdentifier")? != BUNDLE_ID
        || plist(app, "CFBundleExecutable")? != "boomux-launcher"
    {
        return Err("Unexpected app identity".into());
    }
    let version = plist(app, "CFBundleShortVersionString")?;
    stable(&version)?;
    if plist(app, "CFBundleVersion")? != version {
        return Err("Mismatched bundle versions".into());
    }
    Ok((team.to_owned(), version))
}

struct Directory(PathBuf);
impl Drop for Directory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
fn private_directory(path: PathBuf) -> Result<Directory, String> {
    fs::DirBuilder::new()
        .mode(0o700)
        .create(&path)
        .map_err(|e| {
            format!("Update directory unavailable (another installer may be active): {e}")
        })?;
    Ok(Directory(path))
}
// Persistent inode plus flock: kernel releases ownership after crash/quit.
// Never unlink this file, which would let two holders lock different inodes.
struct InstallLock {
    _file: fs::File,
}
impl InstallLock {
    fn acquire(root: &Path) -> Result<Self, String> {
        owned(root)?;
        if fs::symlink_metadata(root.join(".boomux-install.lock")).is_ok() {
            return Err(
                "The legacy installer is active or left a lock; inspect it before updating".into(),
            );
        }
        let file = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(root.join(".boomux-update.lock"))
            .map_err(|e| e.to_string())?;
        let metadata = file.metadata().map_err(|e| e.to_string())?;
        if !metadata.is_file()
            || metadata.uid() != unsafe { libc::geteuid() }
            || metadata.mode() & 0o077 != 0
            || metadata.nlink() != 1
        {
            return Err("Unsafe updater lock file".into());
        }
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            return Err("Another app instance is updating this installation".into());
        }
        Ok(Self { _file: file })
    }
}

fn rename_exclusive(source: &Path, destination: &Path) -> Result<(), String> {
    use std::os::unix::ffi::OsStrExt;
    let source =
        std::ffi::CString::new(source.as_os_str().as_bytes()).map_err(|e| e.to_string())?;
    let destination =
        std::ffi::CString::new(destination.as_os_str().as_bytes()).map_err(|e| e.to_string())?;
    // Same-filesystem staging plus the kernel's no-replace primitive prevents
    // both replacement and `mv`'s move-inside-existing-directory behavior.
    #[cfg(target_os = "macos")]
    let result =
        unsafe { libc::renamex_np(source.as_ptr(), destination.as_ptr(), libc::RENAME_EXCL) };
    #[cfg(target_os = "linux")]
    let result = unsafe {
        libc::renameat2(
            libc::AT_FDCWD,
            source.as_ptr(),
            libc::AT_FDCWD,
            destination.as_ptr(),
            libc::RENAME_NOREPLACE,
        )
    };
    if result == 0 {
        Ok(())
    } else {
        Err(format!(
            "Update destination appeared or cannot be installed: {}",
            std::io::Error::last_os_error()
        ))
    }
}

impl Installation {
    pub fn discover() -> Option<Self> {
        if !cfg!(target_arch = "aarch64") {
            return None;
        }
        Self::from_executable(&std::env::current_exe().ok()?).ok()
    }
    fn from_executable(executable: &Path) -> Result<Self, String> {
        let running = executable
            .parent()
            .and_then(Path::parent)
            .and_then(Path::parent)
            .ok_or("Not an app bundle")?
            .to_owned();
        if executable != running.join("Contents/MacOS/boomux-desktop")
            || running.extension().is_none_or(|v| v != "app")
        {
            return Err("Not an installed Mac app".into());
        }
        let root = running
            .parent()
            .ok_or("Missing installation directory")?
            .to_owned();
        owned(&root)?;
        if fs::canonicalize(&running).map_err(|e| e.to_string())? != running {
            return Err("App path contains a symlink".into());
        }
        let metadata = owned(&running)?;
        let (team, version) = signed_identity(&running)?;
        Ok(Self {
            root,
            running,
            version,
            team,
            identity: (metadata.dev(), metadata.ino()),
        })
    }
    fn require_current(&self) -> Result<(), String> {
        let metadata = owned(&self.running)?;
        if (metadata.dev(), metadata.ino()) != self.identity
            || signed_identity(&self.running)? != (self.team.clone(), self.version.clone())
        {
            return Err("The running installation changed; reopen before updating".into());
        }
        Ok(())
    }
    fn validate_candidate(&self, app: &Path, version: &str) -> Result<(), String> {
        if signed_identity(app)? != (self.team.clone(), version.into()) {
            return Err("Update Team ID or version differs from the installed app".into());
        }
        for name in [
            "boomux",
            "boomux-desktop",
            "webgpu_gateway",
            "boomux-launcher",
        ] {
            let binary = app.join("Contents/MacOS").join(name);
            let meta = owned(&binary)?;
            if !meta.is_file() || meta.mode() & 0o111 == 0 {
                return Err("Missing executable in update".into());
            }
            let mut header = [0u8; 8];
            fs::File::open(&binary)
                .and_then(|mut file| file.read_exact(&mut header))
                .map_err(|e| e.to_string())?;
            if u32::from_le_bytes(header[..4].try_into().unwrap()) != 0xfeedfacf
                || u32::from_le_bytes(header[4..].try_into().unwrap()) != 0x0100000c
            {
                return Err("Update is not an Apple Silicon Mach-O executable".into());
            }
            if matches!(name, "boomux" | "boomux-desktop") {
                let mut cmd = Command::new(&binary);
                cmd.arg("--version");
                if run(BoundedCommand {
                    command: cmd,
                    seconds: 10,
                })?
                .trim()
                    != format!("{name} {version}")
                {
                    return Err("Bundled executable version mismatch".into());
                }
            }
        }
        Ok(())
    }
    pub fn pending(&self) -> Result<Option<Prepared>, String> {
        let path = self.root.join(PENDING);
        if !path.try_exists().map_err(|e| e.to_string())? {
            return Ok(None);
        }
        let pending: Pending =
            serde_json::from_slice(&private_read(&path, 4096)?).map_err(|e| e.to_string())?;
        if pending.from != self.running {
            return Ok(None);
        }
        if stable(&pending.version)? <= stable(&self.version)?
            || pending.candidate != self.root.join(format!("Boomux-{}.app", pending.version))
        {
            return Err("Invalid pending update".into());
        }
        self.validate_candidate(&pending.candidate, &pending.version)?;
        Ok(Some(Prepared {
            installation: self.clone(),
            release: pending.candidate,
            version: pending.version,
        }))
    }
    pub fn finish_installation(&self) -> Result<Option<Prepared>, String> {
        // A manually installed first signed release needs explicit CLI migration;
        // there is no unverified/ad-hoc executable fallback for rollback.
        Ok(None)
    }
    pub fn prepare(&self, version: &str) -> Result<Prepared, String> {
        if stable(version)? <= stable(&self.version)? {
            return Err("Only newer stable releases can be installed".into());
        }
        let _lock = InstallLock::acquire(&self.root)?;
        self.require_current()?;
        let stage = private_directory(
            self.root
                .join(format!(".boomux-update-{}", uuid::Uuid::new_v4())),
        )?;
        let archive = stage.0.join(ASSET);
        let checksum = stage.0.join(format!("{ASSET}.sha256"));
        for (name, output, limit) in [
            (ASSET.to_owned(), &archive, ARCHIVE_LIMIT),
            (format!("{ASSET}.sha256"), &checksum, 1024),
        ] {
            let url =
                format!("https://github.com/gardnmi/boomux/releases/download/v{version}/{name}");
            run(command(
                300,
                "/usr/bin/curl",
                &[
                    "--disable".as_ref(),
                    "--fail".as_ref(),
                    "--silent".as_ref(),
                    "--show-error".as_ref(),
                    "--location".as_ref(),
                    "--proto".as_ref(),
                    "=https".as_ref(),
                    "--proto-redir".as_ref(),
                    "=https".as_ref(),
                    "--connect-timeout".as_ref(),
                    "10".as_ref(),
                    "--max-time".as_ref(),
                    "300".as_ref(),
                    "--max-filesize".as_ref(),
                    limit.to_string().as_ref(),
                    "--output".as_ref(),
                    output.as_os_str(),
                    url.as_ref(),
                ],
            ))?;
        }
        let expected = checksum_value(&private_read(&checksum, 1024)?)?;
        let actual = run(command(
            30,
            "/usr/bin/shasum",
            &["-a".as_ref(), "256".as_ref(), archive.as_os_str()],
        ))?;
        if actual.split_whitespace().next() != Some(expected.as_str()) {
            return Err("Release checksum mismatch".into());
        }
        let unpacked = stage.0.join("unpacked");
        extract_archive(&private_read(&archive, ARCHIVE_LIMIT)?, &unpacked)?;
        let candidate = unpacked.join("Boomux macOS Preview/Boomux.app");
        self.validate_candidate(&candidate, version)?;
        let release = self.root.join(format!("Boomux-{version}.app"));
        if release.try_exists().map_err(|e| e.to_string())? {
            self.validate_candidate(&release, version)?;
        } else {
            rename_exclusive(&candidate, &release)?;
        }
        self.require_current()?;
        self.validate_candidate(&release, version)?;
        let pending = Pending {
            from: self.running.clone(),
            candidate: release.clone(),
            version: version.into(),
        };
        let temporary = stage.0.join("pending.json");
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temporary)
            .map_err(|e| e.to_string())?;
        file.write_all(&serde_json::to_vec(&pending).map_err(|e| e.to_string())?)
            .map_err(|e| e.to_string())?;
        file.sync_all().map_err(|e| e.to_string())?;
        fs::rename(temporary, self.root.join(PENDING)).map_err(|e| e.to_string())?;
        fs::File::open(&self.root)
            .and_then(|f| f.sync_all())
            .map_err(|e| e.to_string())?;
        Ok(Prepared {
            installation: self.clone(),
            release,
            version: version.into(),
        })
    }
}

fn checksum_value(bytes: &[u8]) -> Result<String, String> {
    let text = std::str::from_utf8(bytes).map_err(|e| e.to_string())?;
    let parts: Vec<_> = text.split_whitespace().collect();
    if parts.len() != 2
        || parts[1] != ASSET
        || parts[0].len() != 64
        || !parts[0]
            .bytes()
            .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
    {
        return Err("Malformed release checksum".into());
    }
    Ok(parts[0].into())
}

/// Validate the ordinary ZIP subset produced by ditto before invoking it.
/// Reject links, traversal, duplicate paths, local/central name disagreement,
/// ZIP64, split/encrypted archives and unbounded decompression. Signatures are
/// checked separately after extraction; checksums alone are not authentication.
fn reject_zip64_extra(mut extra: &[u8]) -> Result<(), String> {
    while !extra.is_empty() {
        if extra.len() < 4 {
            return Err("Malformed ZIP extra field".into());
        }
        let kind = u16::from_le_bytes(extra[..2].try_into().unwrap());
        let size = usize::from(u16::from_le_bytes(extra[2..4].try_into().unwrap()));
        if kind == 1 {
            return Err("ZIP64 updates are not supported".into());
        }
        extra = extra.get(4 + size..).ok_or("Malformed ZIP extra field")?;
    }
    Ok(())
}

fn validate_zip(bytes: &[u8]) -> Result<usize, String> {
    let invalid = || "Unsupported or unsafe update ZIP".to_string();
    let u16_at = |offset: usize| {
        bytes
            .get(offset..offset + 2)
            .and_then(|v| v.try_into().ok())
            .map(u16::from_le_bytes)
            .ok_or_else(invalid)
    };
    let u32_at = |offset: usize| {
        bytes
            .get(offset..offset + 4)
            .and_then(|v| v.try_into().ok())
            .map(u32::from_le_bytes)
            .ok_or_else(invalid)
    };
    let start = bytes.len().saturating_sub(65557);
    let end = (start..bytes.len().saturating_sub(21))
        .rev()
        .find(|&i| bytes.get(i..i + 4) == Some(b"PK\x05\x06"))
        .ok_or_else(invalid)?;
    if end + 22 + usize::from(u16_at(end + 20)?) != bytes.len()
        || u16_at(end + 4)? != 0
        || u16_at(end + 6)? != 0
    {
        return Err(invalid());
    }
    let entries = usize::from(u16_at(end + 10)?);
    if entries == 0 || entries > 20000 || usize::from(u16_at(end + 8)?) != entries {
        return Err(invalid());
    }
    let central = u32_at(end + 16)? as usize;
    if central.checked_add(u32_at(end + 12)? as usize) != Some(end) {
        return Err(invalid());
    }
    let mut offset = central;
    let mut total = 0u64;
    let mut names = HashSet::new();
    let mut ranges = Vec::new();
    for _ in 0..entries {
        if bytes.get(offset..offset + 4) != Some(b"PK\x01\x02") {
            return Err(invalid());
        }
        let flags = u16_at(offset + 8)?;
        let method = u16_at(offset + 10)?;
        let compressed = u32_at(offset + 20)?;
        let size = u32_at(offset + 24)?;
        let name_len = usize::from(u16_at(offset + 28)?);
        let extra = usize::from(u16_at(offset + 30)?);
        let comment = usize::from(u16_at(offset + 32)?);
        let mode = u32_at(offset + 38)? >> 16;
        let local = u32_at(offset + 42)? as usize;
        reject_zip64_extra(
            bytes
                .get(offset + 46 + name_len..offset + 46 + name_len + extra)
                .ok_or_else(invalid)?,
        )?;
        let name = bytes
            .get(offset + 46..offset + 46 + name_len)
            .ok_or_else(invalid)?;
        let path = std::str::from_utf8(name).map_err(|_| invalid())?;
        let parts: Vec<_> = path.trim_end_matches('/').split('/').collect();
        if name_len > 4096
            || name.contains(&0)
            || path.contains('\\')
            || parts
                .iter()
                .any(|part| part.is_empty() || *part == "." || *part == "..")
            || !matches!(parts[0], "Boomux macOS Preview" | "__MACOSX")
            || !names.insert(path.to_owned())
            || flags & 1 != 0
            || !matches!(method, 0 | 8)
            || u16_at(offset + 34)? != 0
            || !matches!(mode & 0o170000, 0 | 0o100000 | 0o040000)
            || compressed == u32::MAX
            || size > 300 * 1024 * 1024
        {
            return Err(invalid());
        }
        total += u64::from(size);
        if total > 1024 * 1024 * 1024
            || bytes.get(local..local + 4) != Some(b"PK\x03\x04")
            || u16_at(local + 6)? != flags
            || u16_at(local + 8)? != method
            || usize::from(u16_at(local + 26)?) != name_len
            || bytes.get(local + 30..local + 30 + name_len) != Some(name)
        {
            return Err(invalid());
        }
        let local_extra = usize::from(u16_at(local + 28)?);
        reject_zip64_extra(
            bytes
                .get(local + 30 + name_len..local + 30 + name_len + local_extra)
                .ok_or_else(invalid)?,
        )?;
        let body = local
            .checked_add(30 + name_len + usize::from(u16_at(local + 28)?))
            .ok_or_else(invalid)?;
        let finish = body
            .checked_add(compressed as usize)
            .filter(|end| *end <= central)
            .ok_or_else(invalid)?;
        ranges.push((local, finish));
        offset = offset
            .checked_add(46 + name_len + extra + comment)
            .filter(|offset| *offset <= end)
            .ok_or_else(invalid)?;
    }
    ranges.sort_unstable();
    if offset != end || ranges.windows(2).any(|pair| pair[0].1 > pair[1].0) {
        return Err(invalid());
    }
    Ok(entries)
}

fn extract_archive(bytes: &[u8], destination: &Path) -> Result<(), String> {
    let expected_entries = validate_zip(bytes)?;
    let mut archive =
        zip::ZipArchive::new(std::io::Cursor::new(bytes)).map_err(|e| e.to_string())?;
    if archive.len() != expected_entries {
        return Err("ZIP entry count changed during decoding".into());
    }
    let mut total = 0u64;
    let mut names = HashSet::new();
    fs::DirBuilder::new()
        .mode(0o700)
        .create(destination)
        .map_err(|e| e.to_string())?;
    for index in 0..archive.len() {
        let mut entry = archive.by_index(index).map_err(|e| e.to_string())?;
        let relative = entry.enclosed_name().ok_or("Unsafe archive filename")?;
        let size = entry.size();
        total = total
            .checked_add(size)
            .ok_or("Decoded archive size overflow")?;
        if size > 300 * 1024 * 1024
            || total > 1024 * 1024 * 1024
            || entry.compressed_size() > ARCHIVE_LIMIT
            || !names.insert(relative.clone())
            || entry.name().len() > 4096
            || entry.name().contains('\\')
            || entry.name().contains('\0')
            || !matches!(relative.components().next(), Some(std::path::Component::Normal(name)) if name == "Boomux macOS Preview" || name == "__MACOSX")
        {
            return Err("Decoded ZIP metadata exceeds its contract".into());
        }
        if entry.is_symlink() {
            return Err("Archive links are not allowed".into());
        }
        let path = destination.join(relative);
        if entry.is_dir() {
            fs::create_dir_all(&path).map_err(|e| e.to_string())?;
        } else {
            fs::create_dir_all(path.parent().ok_or("Invalid archive path")?)
                .map_err(|e| e.to_string())?;
            // A private new staging directory, no archive-created links, and
            // create_new prevent overwrite or case-normalization collisions.
            let mode = if entry.unix_mode().unwrap_or(0) & 0o111 != 0 {
                0o755
            } else {
                0o644
            };
            let mut file = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(mode)
                .custom_flags(libc::O_NOFOLLOW)
                .open(&path)
                .map_err(|e| e.to_string())?;
            let size = entry.size();
            let copied = std::io::copy(&mut (&mut entry).take(size + 1), &mut file)
                .map_err(|e| e.to_string())?;
            if copied != size {
                return Err("Archive size mismatch".into());
            }
        }
    }
    Ok(())
}

fn daemon_status(cli: &Path) -> Result<serde_json::Value, String> {
    let mut command = Command::new(cli);
    command.args(["--json", "daemon", "status"]);
    let value: serde_json::Value = serde_json::from_str(&run(BoundedCommand {
        command,
        seconds: 10,
    })?)
    .map_err(|e| e.to_string())?;
    if value["schema"] != "boomux.cli/v1" || value["command"] != "daemon.status" {
        return Err("Unrecognized daemon status".into());
    }
    Ok(value["data"].clone())
}
fn restart_daemon(app: &Path) -> Result<(), String> {
    let cli = app.join("Contents/MacOS/boomux");
    let mut command = Command::new(&cli);
    command
        .args(["daemon", "restart", "--executable"])
        .arg(&cli);
    run(BoundedCommand {
        command,
        seconds: 45,
    })?;
    let status = daemon_status(&cli)?;
    if status["status"] != "running" || status["executable"].as_str() != cli.to_str() {
        return Err("Replacement daemon readiness was not confirmed".into());
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Step {
    Handoff,
    OpenWindow,
    Rollback,
}
fn transaction(mut operation: impl FnMut(Step) -> Result<(), String>) -> Result<(), String> {
    // A failed command can be ambiguous, so attempt the session-preserving
    // handoff back to the already-validated previous executable on either error.
    if let Err(error) = operation(Step::Handoff).and_then(|_| operation(Step::OpenWindow)) {
        let recovery = operation(Step::Rollback);
        return Err(format!(
            "{error}. Previous app retained. Daemon recovery: {}",
            recovery.err().unwrap_or_else(|| "complete".into())
        ));
    }
    Ok(())
}
impl Prepared {
    pub fn restart(&self) -> Result<(), String> {
        let executable = self
            .installation
            .running
            .join("Contents/MacOS/boomux-desktop");
        // The helper owns bounded stage deadlines and rollback. An outer timer
        // must never kill it halfway through a session-preserving handoff.
        let mut child = Command::new(executable)
            .arg("--apply-macos-update")
            .arg(&self.release)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .process_group(0)
            .spawn()
            .map_err(|e| e.to_string())?;
        let mut error = Vec::new();
        let read = child
            .stderr
            .take()
            .ok_or("Missing update helper diagnostics")?
            .take(65537)
            .read_to_end(&mut error);
        // Even if diagnostic collection fails, wait for the authoritative
        // helper to finish recovery; do not terminate it from the old GUI.
        let status = child.wait().map_err(|e| e.to_string())?;
        if !status.success() {
            return Err(format!(
                "Update helper failed ({status}): {}",
                String::from_utf8_lossy(&error[..error.len().min(65536)])
            ));
        }
        // Diagnostics cannot reverse an acknowledged committed replacement.
        if let Err(error) = read {
            eprintln!("Update completed; diagnostic read failed: {error}");
        }
        Ok(())
    }
    fn restart_inner(&self) -> Result<(), String> {
        let installation = &self.installation;
        let _lock = InstallLock::acquire(&installation.root)?;
        let session = private_directory(
            installation
                .root
                .join(format!(".boomux-update-session-{}", uuid::Uuid::new_v4())),
        )?;
        installation.require_current()?;
        installation.validate_candidate(&self.release, &self.version)?;
        if installation
            .pending()?
            .is_none_or(|pending| pending.release != self.release)
        {
            return Err("The prepared update changed; check again".into());
        }
        let old_cli = installation.running.join("Contents/MacOS/boomux");
        let status = daemon_status(&old_cli)?;
        if status["status"] == "running" && status["executable"].as_str() != old_cli.to_str() {
            return Err("Another installation owns this daemon. Finish its explicit daemon migration before updating; no processes were changed".into());
        }
        transaction(|step| match step {
            Step::Handoff => restart_daemon(&self.release),
            Step::Rollback => restart_daemon(&installation.running),
            Step::OpenWindow => self.open_window(&session.0),
        })?;
        // A ready replacement is already committed. Its different `from` path
        // makes a stale marker harmless; cleanup cannot keep the old GUI alive.
        if let Err(error) = fs::remove_file(installation.root.join(PENDING)) {
            eprintln!("Update opened; stale pending marker could not be removed: {error}");
        }
        Ok(())
    }
    fn select_current(&self, temporary_directory: &Path) -> Result<(), String> {
        let path = self.installation.root.join(CURRENT);
        if path.try_exists().map_err(|e| e.to_string())? {
            private_read(&path, 4096)?;
        }
        let temporary = temporary_directory.join("current.json");
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temporary)
            .map_err(|e| e.to_string())?;
        let current = Current {
            candidate: self.release.clone(),
            version: self.version.clone(),
        };
        file.write_all(&serde_json::to_vec(&current).map_err(|e| e.to_string())?)
            .map_err(|e| e.to_string())?;
        file.sync_all().map_err(|e| e.to_string())?;
        fs::rename(temporary, path).map_err(|e| e.to_string())?;
        // Rename committed the selection; don't roll back the daemon after this
        // point merely because a durability flush failed.
        if let Err(error) = fs::File::open(&self.installation.root).and_then(|file| file.sync_all())
        {
            eprintln!("Updated app selection saved; directory flush failed: {error}");
        }
        Ok(())
    }
    fn open_window(&self, directory: &Path) -> Result<(), String> {
        let ready = directory.join(format!("ready-{}", uuid::Uuid::new_v4()));
        let mut paths = vec![self.release.join("Contents/MacOS")];
        paths.extend(std::env::split_paths(
            &std::env::var_os("PATH").unwrap_or_default(),
        ));
        let mut child = Command::new(self.release.join("Contents/MacOS/boomux-desktop"))
            .arg("--update-ready")
            .arg(&ready)
            .env(
                "PATH",
                std::env::join_paths(paths).map_err(|e| e.to_string())?,
            )
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            // Survive the helper/old GUI's bounded process-group cleanup.
            .process_group(0)
            .spawn()
            .map_err(|e| e.to_string())?;
        let deadline = Instant::now() + Duration::from_secs(30);
        let result = loop {
            match child.try_wait() {
                Ok(Some(_)) => break Err("The updated window exited before readiness".into()),
                Err(error) => break Err(error.to_string()),
                Ok(None) => (),
            }
            if ready.exists() && private_read(&ready, 16).is_ok_and(|bytes| bytes == b"ready\n") {
                break self.select_current(directory);
            }
            if Instant::now() >= deadline {
                break Err("The updated window did not acknowledge readiness".into());
            }
            std::thread::sleep(Duration::from_millis(50));
        };
        if result.is_err() {
            let _ = child.kill();
            let _ = child.wait();
        }
        let _ = fs::remove_file(ready);
        result
    }
}
fn redirect_current(args: impl Iterator<Item = std::ffi::OsString>) -> Result<(), String> {
    // Normal first launch has no committed update selection. Avoid a bundle
    // walk/signature process until there is actually a pointer to follow.
    let root = std::env::current_exe().ok().and_then(|path| {
        path.parent()?
            .parent()?
            .parent()?
            .parent()
            .map(Path::to_owned)
    });
    if root.is_none_or(|root| !root.join(CURRENT).exists()) {
        return Ok(());
    }
    let Some(installation) = Installation::discover() else {
        return Ok(());
    };
    let pointer = installation.root.join(CURRENT);
    if !pointer.try_exists().map_err(|e| e.to_string())? {
        return Ok(());
    }
    let current: Current =
        serde_json::from_slice(&private_read(&pointer, 4096)?).map_err(|e| e.to_string())?;
    if current.candidate == installation.running
        || stable(&current.version)? <= stable(&installation.version)?
    {
        return Ok(());
    }
    if current.candidate
        != installation
            .root
            .join(format!("Boomux-{}.app", current.version))
    {
        return Err("Selected update is outside this installation".into());
    }
    installation.validate_candidate(&current.candidate, &current.version)?;
    let mut command = Command::new(current.candidate.join("Contents/MacOS/boomux-desktop"));
    command.arg("--macos-launch").args(args);
    Err(command.exec().to_string())
}

/// An authorized restart transaction survives the old GUI quitting/crashing.
/// This entry point runs before GPUI and accepts only its already-verified,
/// owner-private pending record, not an arbitrary executable argument.
pub fn dispatch() {
    let mut args = std::env::args_os().skip(1);
    let action = args.next();
    if action.as_deref() == Some(std::ffi::OsStr::new("--macos-launch")) {
        // Older versioned Dock/Finder entries can follow the committed update.
        // Never hand off a daemon or execute an unverified pointer on launch.
        if let Err(error) = redirect_current(args) {
            eprintln!("Could not open selected update; retaining this app: {error}");
        }
        return;
    }
    if action.as_deref() != Some(std::ffi::OsStr::new("--apply-macos-update")) {
        return;
    }
    let result = (|| {
        let requested = args
            .next()
            .map(PathBuf::from)
            .ok_or("Missing update target")?;
        if args.next().is_some() {
            return Err("Unexpected update arguments".into());
        }
        let installation = Installation::discover()
            .ok_or("The current signed installation could not be verified")?;
        let prepared = installation
            .pending()?
            .ok_or("No prepared update for this installation")?;
        if prepared.release != requested {
            return Err("The requested update differs from the pending record".into());
        }
        prepared.restart_inner()
    })();
    if let Err(error) = result {
        eprintln!("{}", error.chars().take(8192).collect::<String>());
        std::process::exit(1);
    }
    std::process::exit(0);
}

pub fn signal_ready(path: PathBuf) {
    // Called only after GPUI created a window; no synchronous UI-thread I/O.
    std::thread::spawn(move || {
        if let Ok(mut file) = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(path)
        {
            let _ = file.write_all(b"ready\n");
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    fn archive(name: &str, local_name: &str, mode: u32) -> Vec<u8> {
        let mut bytes = vec![0; 30];
        bytes[..4].copy_from_slice(b"PK\x03\x04");
        bytes[26..28].copy_from_slice(&(local_name.len() as u16).to_le_bytes());
        bytes.extend_from_slice(local_name.as_bytes());
        let start = bytes.len();
        let mut central = vec![0; 46];
        central[..4].copy_from_slice(b"PK\x01\x02");
        central[28..30].copy_from_slice(&(name.len() as u16).to_le_bytes());
        central[38..42].copy_from_slice(&(mode << 16).to_le_bytes());
        bytes.extend_from_slice(&central);
        bytes.extend_from_slice(name.as_bytes());
        let mut end = vec![0; 22];
        end[..4].copy_from_slice(b"PK\x05\x06");
        end[8..10].copy_from_slice(&1u16.to_le_bytes());
        end[10..12].copy_from_slice(&1u16.to_le_bytes());
        end[12..16].copy_from_slice(&((bytes.len() - start) as u32).to_le_bytes());
        end[16..20].copy_from_slice(&(start as u32).to_le_bytes());
        bytes.extend_from_slice(&end);
        bytes
    }
    #[test]
    fn zip_rejects_escape_links_and_local_header_disagreement() {
        let path = "Boomux macOS Preview/Boomux.app/Contents/Info.plist";
        assert!(validate_zip(&archive(path, path, 0o100644)).is_ok());
        for path in [
            "/escape",
            "../escape",
            "Boomux macOS Preview/../escape",
            "Boomux macOS Preview/./file",
            "Boomux macOS Preview//file",
            "other/file",
            "Boomux macOS Preview/a\\b",
        ] {
            assert!(
                validate_zip(&archive(path, path, 0o100644)).is_err(),
                "{path}"
            );
        }
        assert!(validate_zip(&archive(path, path, 0o120777)).is_err());
        assert!(validate_zip(&archive(path, "../elsewhere", 0o100644)).is_err());
        assert!(validate_zip(b"not a zip").is_err());
    }
    #[test]
    fn extraction_materializes_only_new_regular_files_with_bounded_contents() {
        use zip::write::SimpleFileOptions;
        let root = std::env::temp_dir().join(format!("boomux-zip-test-{}", uuid::Uuid::new_v4()));
        let directory = private_directory(root.clone()).unwrap();
        let mut writer = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        writer
            .start_file(
                "Boomux macOS Preview/Boomux.app/Contents/MacOS/helper",
                SimpleFileOptions::default().unix_permissions(0o755),
            )
            .unwrap();
        writer.write_all(b"fixture bytes").unwrap();
        let bytes = writer.finish().unwrap().into_inner();
        extract_archive(&bytes, &root.join("out")).unwrap();
        let path = root.join("out/Boomux macOS Preview/Boomux.app/Contents/MacOS/helper");
        assert_eq!(fs::read(&path).unwrap(), b"fixture bytes");
        assert_eq!(owned(&path).unwrap().mode() & 0o777, 0o755);
        assert!(extract_archive(&bytes, &root.join("out")).is_err());
        drop(directory);
    }

    #[test]
    fn zip64_metadata_cannot_override_validated_sizes_or_offsets() {
        let mut extra = vec![1, 0, 24, 0];
        extra.extend_from_slice(&u64::MAX.to_le_bytes());
        extra.extend_from_slice(&0u64.to_le_bytes());
        extra.extend_from_slice(&0u64.to_le_bytes());
        assert!(reject_zip64_extra(&extra).is_err());
        assert!(reject_zip64_extra(&[2, 0, 9, 0]).is_err());
    }
    #[test]
    fn advisory_lock_can_be_reacquired_without_deleting_a_stale_directory() {
        let root = std::env::temp_dir().join(format!("boomux-lock-test-{}", uuid::Uuid::new_v4()));
        let directory = private_directory(root.clone()).unwrap();
        let lock = InstallLock::acquire(&root).unwrap();
        assert!(InstallLock::acquire(&root).is_err());
        drop(lock);
        let lock = InstallLock::acquire(&root).unwrap();
        assert!(root.join(".boomux-update.lock").exists());
        drop(lock);
        drop(directory);
    }

    #[test]
    fn command_bounds_cover_output_floods_and_descendant_held_pipes() {
        let start = Instant::now();
        let output = run(command(
            2,
            "/bin/sh",
            &["-c".as_ref(), "(sleep 30) & printf ready".as_ref()],
        ))
        .unwrap();
        assert_eq!(output, "ready");
        assert!(start.elapsed() < Duration::from_secs(2));
        assert!(
            run(command(1, "/bin/sh", &["-c".as_ref(), "sleep 30".as_ref()]))
                .unwrap_err()
                .contains("timed out")
        );
        assert!(
            run(command(
                2,
                "/bin/sh",
                &[
                    "-c".as_ref(),
                    "while :; do printf 123456789012345678901234567890; done".as_ref()
                ]
            ))
            .unwrap_err()
            .contains("output exceeded")
        );
    }
    #[cfg(target_os = "macos")]
    #[test]
    fn developer_id_requirement_compiles_on_macos() {
        let root =
            std::env::temp_dir().join(format!("boomux-requirement-test-{}", uuid::Uuid::new_v4()));
        let directory = private_directory(root.clone()).unwrap();
        let requirement = format!(
            "=anchor apple generic and certificate leaf[field.1.2.840.113635.100.6.1.13] exists and identifier \"{BUNDLE_ID}\" and certificate leaf[subject.OU] = \"ABCDEFGHIJ\""
        );
        run(command(
            10,
            "/usr/bin/csreq",
            &[
                "-r".as_ref(),
                requirement.as_ref(),
                "-b".as_ref(),
                root.join("requirement").as_os_str(),
            ],
        ))
        .unwrap();
        assert!(root.join("requirement").metadata().unwrap().len() > 0);
        drop(directory);
    }

    #[test]
    fn no_replace_installation_keeps_an_existing_directory_immutable() {
        let root =
            std::env::temp_dir().join(format!("boomux-rename-test-{}", uuid::Uuid::new_v4()));
        let directory = private_directory(root.clone()).unwrap();
        fs::create_dir(root.join("candidate")).unwrap();
        fs::create_dir(root.join("existing.app")).unwrap();
        fs::write(root.join("existing.app/keep"), b"unchanged").unwrap();
        assert!(rename_exclusive(&root.join("candidate"), &root.join("existing.app")).is_err());
        assert!(root.join("candidate").is_dir());
        assert!(!root.join("existing.app/candidate").exists());
        assert_eq!(
            fs::read(root.join("existing.app/keep")).unwrap(),
            b"unchanged"
        );
        rename_exclusive(&root.join("candidate"), &root.join("new.app")).unwrap();
        assert!(!root.join("candidate").exists());
        drop(directory);
    }
    #[test]
    fn committed_pointer_is_private_atomic_and_does_not_follow_links() {
        let root =
            std::env::temp_dir().join(format!("boomux-pointer-test-{}", uuid::Uuid::new_v4()));
        let directory = private_directory(root.clone()).unwrap();
        let temporary = private_directory(root.join("temporary")).unwrap();
        let prepared = Prepared {
            installation: Installation {
                root: root.clone(),
                running: root.join("Boomux-1.0.0.app"),
                version: "1.0.0".into(),
                team: "ABCDEFGHIJ".into(),
                identity: (0, 0),
            },
            release: root.join("Boomux-1.1.0.app"),
            version: "1.1.0".into(),
        };
        prepared.select_current(&temporary.0).unwrap();
        let selected: Current =
            serde_json::from_slice(&private_read(&root.join(CURRENT), 4096).unwrap()).unwrap();
        assert_eq!(selected.candidate, prepared.release);
        assert_eq!(selected.version, "1.1.0");
        assert_eq!(
            fs::metadata(root.join(CURRENT)).unwrap().mode() & 0o777,
            0o600
        );
        fs::remove_file(root.join(CURRENT)).unwrap();
        fs::write(root.join("unrelated"), b"keep").unwrap();
        std::os::unix::fs::symlink(root.join("unrelated"), root.join(CURRENT)).unwrap();
        assert!(prepared.select_current(&temporary.0).is_err());
        assert_eq!(fs::read(root.join("unrelated")).unwrap(), b"keep");
        drop(temporary);
        drop(directory);
    }

    #[test]
    fn checksum_and_version_cannot_select_other_assets_or_prereleases() {
        assert!(checksum_value(format!("{}  {ASSET}\n", "a".repeat(64)).as_bytes()).is_ok());
        for checksum in [
            format!("{}  other.zip", "a".repeat(64)),
            format!("{} {ASSET}\nextra", "a".repeat(64)),
            format!("{} {ASSET}", "A".repeat(64)),
        ] {
            assert!(checksum_value(checksum.as_bytes()).is_err());
        }
        for version in ["../bad", "1.2.3-beta", "1.2.3+build"] {
            assert!(stable(version).is_err());
        }
    }
    #[test]
    fn successful_transaction_hands_off_before_ready_window_without_rollback() {
        let mut steps = Vec::new();
        transaction(|step| {
            steps.push(step);
            Ok(())
        })
        .unwrap();
        assert_eq!(steps, [Step::Handoff, Step::OpenWindow]);
    }
    #[test]
    fn failed_handoff_or_window_recovers_without_killing_sessions() {
        for failure in [Step::Handoff, Step::OpenWindow] {
            let mut steps = Vec::new();
            let error = transaction(|step| {
                steps.push(step);
                if step == failure {
                    Err("fixture failure".into())
                } else {
                    Ok(())
                }
            })
            .unwrap_err();
            assert!(error.contains("Daemon recovery: complete"));
            assert_eq!(steps.last(), Some(&Step::Rollback));
            if failure == Step::Handoff {
                assert!(!steps.contains(&Step::OpenWindow));
            }
        }
    }
    #[test]
    fn failed_recovery_is_never_reported_as_success() {
        let error = transaction(|step| Err(format!("{step:?} failed"))).unwrap_err();
        assert!(error.contains("Rollback failed"));
    }
    #[test]
    fn exclusive_lock_and_unsafe_owned_files_fail_closed() {
        let root =
            std::env::temp_dir().join(format!("boomux-mac-update-test-{}", uuid::Uuid::new_v4()));
        let directory = private_directory(root.clone()).unwrap();
        assert!(private_directory(root.clone()).is_err());
        let file = root.join("pending");
        fs::write(&file, b"data").unwrap();
        fs::set_permissions(&file, fs::Permissions::from_mode(0o666)).unwrap();
        assert!(private_read(&file, 10).is_err());
        fs::remove_file(&file).unwrap();
        std::os::unix::fs::symlink("/etc/passwd", &file).unwrap();
        assert!(private_read(&file, 10).is_err());
        drop(directory);
        assert!(!root.exists());
    }
}

#[cfg(test)]
mod zip64_regression {
    use super::*;
    #[test]
    fn zip64_extra_size_override_is_rejected_before_extraction() {
        let name = b"Boomux macOS Preview/empty";
        let mut bytes = vec![0; 30];
        bytes[..4].copy_from_slice(b"PK\x03\x04");
        bytes[26..28].copy_from_slice(&(name.len() as u16).to_le_bytes());
        bytes.extend_from_slice(name);
        let start = bytes.len();
        let mut central = vec![0; 46];
        central[..4].copy_from_slice(b"PK\x01\x02");
        central[28..30].copy_from_slice(&(name.len() as u16).to_le_bytes());
        central[30..32].copy_from_slice(&28u16.to_le_bytes());
        bytes.extend_from_slice(&central);
        bytes.extend_from_slice(name);
        bytes.extend_from_slice(&1u16.to_le_bytes());
        bytes.extend_from_slice(&24u16.to_le_bytes());
        bytes.extend_from_slice(&(5u64 * 1024 * 1024 * 1024).to_le_bytes());
        bytes.extend_from_slice(&0u64.to_le_bytes());
        bytes.extend_from_slice(&0u64.to_le_bytes());
        let mut end = vec![0; 22];
        end[..4].copy_from_slice(b"PK\x05\x06");
        end[8..10].copy_from_slice(&1u16.to_le_bytes());
        end[10..12].copy_from_slice(&1u16.to_le_bytes());
        end[12..16].copy_from_slice(&((bytes.len() - start) as u32).to_le_bytes());
        end[16..20].copy_from_slice(&(start as u32).to_le_bytes());
        bytes.extend_from_slice(&end);
        assert!(validate_zip(&bytes).is_err());
        let mut archive = zip::ZipArchive::new(std::io::Cursor::new(&bytes)).unwrap();
        assert_eq!(
            archive.by_index(0).unwrap().size(),
            5u64 * 1024 * 1024 * 1024
        );
    }
}

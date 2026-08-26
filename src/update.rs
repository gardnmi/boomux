use std::cmp::Ordering;
use std::env;
use std::fmt::Write as _;
use std::fs::{self, File, OpenOptions};
use std::io::{self, IsTerminal, Read, Write};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use semver::Version;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use boomux::{client, ssh_bootstrap};

const DISTRIBUTION: Option<&str> = option_env!("BOOMUX_DISTRIBUTION");
const MAX_EXECUTABLE_BYTES: u64 = 256 * 1024 * 1024;
const MAX_SMOKE_OUTPUT_BYTES: usize = 4096;
const SMOKE_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_PROC_CMDLINE_BYTES: u64 = 4096;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum InstallKind {
    GithubRelease,
    PackageManaged,
    RootOwned,
    SourceBuild,
    DevelopmentBuild,
    Custom,
    Unknown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum UpdateState {
    UpdateAvailable,
    Current,
    NewerThanLatest,
    Ineligible,
    UnsupportedTarget,
    CheckFailed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum RecommendedAction {
    RunUpdate,
    None,
    KeepCurrent,
    UsePackageManager,
    InstallGithubRelease,
    Retry,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct UpdateStatus {
    current: String,
    latest: Option<String>,
    state: UpdateState,
    install_kind: InstallKind,
    path: String,
    target: Option<String>,
    release_url: Option<String>,
    recommended_action: RecommendedAction,
}

#[derive(Debug, Deserialize)]
struct LatestRelease {
    tag_name: String,
    html_url: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct FileFingerprint {
    device: u64,
    inode: u64,
    length: u64,
    modified_seconds: i64,
    modified_nanoseconds: i64,
    changed_seconds: i64,
    changed_nanoseconds: i64,
    owner: u32,
    mode: u32,
    links: u64,
    digest: [u8; 32],
}

struct Inspection {
    status: UpdateStatus,
    path: PathBuf,
    baseline: Option<FileFingerprint>,
    latest: Option<Version>,
    tag: Option<String>,
}

pub(crate) struct UninstallTarget {
    path: PathBuf,
    baseline: FileFingerprint,
}

impl UninstallTarget {
    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    pub(crate) fn authorization_token(&self) -> String {
        fingerprint_token(&self.baseline)
    }
}

pub(crate) fn status() -> UpdateStatus {
    inspect().status
}

pub(crate) fn guided_update() -> Result<(), Box<dyn std::error::Error>> {
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Boomux update requires an interactive terminal",
        )
        .into());
    }
    let inspection = inspect();
    let status = &inspection.status;
    println!("Current version: {}", status.current);
    println!(
        "Latest version: {}",
        status.latest.as_deref().unwrap_or("unavailable")
    );
    println!("Install path: {}", status.path);
    if status.state != UpdateState::UpdateAvailable
        || status.install_kind != InstallKind::GithubRelease
    {
        return Err(
            io::Error::new(io::ErrorKind::PermissionDenied, refusal_message(status)).into(),
        );
    }
    print!("Download, verify, and install this release? [y/N] ");
    io::stdout().flush()?;
    let mut answer = String::new();
    io::stdin().read_line(&mut answer)?;
    if !matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes") {
        return Err(
            io::Error::new(io::ErrorKind::PermissionDenied, "update was not authorized").into(),
        );
    }

    let target = status
        .target
        .as_deref()
        .expect("available update has a target");
    let tag = inspection
        .tag
        .as_deref()
        .expect("available update has a tag");
    let latest = inspection
        .latest
        .as_ref()
        .expect("available update has a version");
    let parent = inspection.path.parent().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "install path has no parent directory",
        )
    })?;
    let bytes = ssh_bootstrap::download_release_binary_in(parent, target, tag)?;
    let candidate = write_candidate(&inspection.path, &bytes)?;
    let result: io::Result<()> = (|| {
        smoke_test(&candidate, latest)?;
        let candidate_fingerprint = fingerprint(&candidate)?;
        let daemon = daemon_disposition(&inspection.path)?;
        replace_with_rollback(
            &inspection.path,
            &candidate,
            inspection
                .baseline
                .as_ref()
                .expect("eligible update has a baseline"),
            &candidate_fingerprint,
            || finish_daemon_handoff(&daemon, &inspection.path, &candidate_fingerprint),
            || recover_daemon_after_rollback(&daemon),
        )?;
        println!("Updated Boomux to {latest}");
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&candidate);
    }
    result.map_err(Into::into)
}

pub(crate) fn uninstall_target() -> io::Result<UninstallTarget> {
    let path = env::current_exe()?;
    let home = env::var_os("HOME").map(PathBuf::from);
    let (kind, baseline) = classify_installation(&path, home.as_deref(), DISTRIBUTION);
    if kind != InstallKind::GithubRelease {
        let message = match kind {
            InstallKind::PackageManaged | InstallKind::RootOwned => {
                "this Boomux executable is package-managed; uninstall it with the package manager that installed it"
            }
            InstallKind::SourceBuild | InstallKind::DevelopmentBuild => {
                "this Boomux executable is a source or development build; remove it through its build or installation workflow"
            }
            InstallKind::Custom | InstallKind::Unknown | InstallKind::GithubRelease => {
                "this Boomux executable is not an ownership-proven official release installation"
            }
        };
        return Err(io::Error::new(io::ErrorKind::PermissionDenied, message));
    }
    Ok(UninstallTarget {
        path,
        baseline: baseline.expect("eligible uninstall has a baseline"),
    })
}

pub(crate) fn stop_daemon_for_uninstall(
    target: &UninstallTarget,
) -> io::Result<client::DaemonLockReservation> {
    match daemon_disposition(&target.path)? {
        DaemonDisposition::Absent { _reservation, .. } => Ok(_reservation),
        DaemonDisposition::SameExecutable { client, .. } => {
            client
                .shutdown()
                .map_err(|error| io::Error::other(error.to_string()))?;
            reserve_daemon_absence(&client::socket_path()?)
        }
    }
}

pub(crate) fn stop_daemon_for_remote_uninstall(
    target: &UninstallTarget,
    expected_node_id: &str,
) -> io::Result<client::DaemonLockReservation> {
    match daemon_disposition(&target.path)? {
        DaemonDisposition::Absent { .. } => Err(io::Error::new(
            io::ErrorKind::NotFound,
            "remote uninstall requires the identity-verified daemon to remain running",
        )),
        DaemonDisposition::SameExecutable { client, .. } => {
            client
                .shutdown_if_node_identity(expected_node_id)
                .map_err(|error| io::Error::other(error.to_string()))?;
            reserve_daemon_absence(&client::socket_path()?)
        }
    }
}

fn fingerprint_token(fingerprint: &FileFingerprint) -> String {
    let mut digest = String::with_capacity(64);
    for byte in fingerprint.digest {
        write!(digest, "{byte:02x}").expect("writing to a string cannot fail");
    }
    format!(
        "{}:{}:{}:{}:{}:{}:{}:{}:{}:{}:{}",
        fingerprint.device,
        fingerprint.inode,
        fingerprint.length,
        fingerprint.modified_seconds,
        fingerprint.modified_nanoseconds,
        fingerprint.changed_seconds,
        fingerprint.changed_nanoseconds,
        fingerprint.owner,
        fingerprint.mode,
        fingerprint.links,
        digest
    )
}

pub(crate) fn remove_uninstall_target(target: &UninstallTarget) -> io::Result<()> {
    let parent = target
        .path
        .parent()
        .ok_or_else(|| io::Error::other("install path has no parent directory"))?;
    let staged = parent.join(format!(".boomux.uninstall.{}", Uuid::new_v4()));
    rename_noreplace(&target.path, &staged)?;
    let moved = match fingerprint(&staged) {
        Ok(moved) => moved,
        Err(error) => {
            return match rename_noreplace(&staged, &target.path) {
                Ok(()) => Err(error),
                Err(restore) => Err(io::Error::other(format!(
                    "could not inspect the staged Boomux executable: {error}; restoring it failed: {restore}; preserved at {}",
                    staged.display()
                ))),
            };
        }
    };
    if !same_candidate(&moved, &target.baseline) {
        return match rename_noreplace(&staged, &target.path) {
            Ok(()) => Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "installed Boomux executable changed after uninstall authorization",
            )),
            Err(restore) => Err(io::Error::other(format!(
                "installed Boomux executable changed after uninstall authorization; restoring the moved file failed: {restore}; preserved at {}",
                staged.display()
            ))),
        };
    }
    fs::remove_file(&staged)?;
    sync_directory(parent)
}

#[cfg(target_os = "linux")]
fn rename_noreplace(from: &Path, to: &Path) -> io::Result<()> {
    let from = std::ffi::CString::new(from.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path contains a NUL byte"))?;
    let to = std::ffi::CString::new(to.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path contains a NUL byte"))?;
    if unsafe {
        libc::renameat2(
            libc::AT_FDCWD,
            from.as_ptr(),
            libc::AT_FDCWD,
            to.as_ptr(),
            libc::RENAME_NOREPLACE,
        )
    } == 0
    {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(not(target_os = "linux"))]
fn rename_noreplace(_from: &Path, _to: &Path) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "self-uninstall requires Linux atomic rename support",
    ))
}

pub(crate) fn revalidate_uninstall_target(target: &UninstallTarget) -> io::Result<()> {
    let current = fingerprint(&target.path)?;
    if current != target.baseline {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "installed Boomux executable changed after uninstall authorization",
        ));
    }
    Ok(())
}

fn inspect() -> Inspection {
    let current = Version::parse(env!("CARGO_PKG_VERSION"))
        .expect("Cargo package version must be strict semver");
    let path = env::current_exe().unwrap_or_default();
    let home = env::var_os("HOME").map(PathBuf::from);
    let (install_kind, baseline) = classify_installation(&path, home.as_deref(), DISTRIBUTION);
    let target = release_target().map(str::to_owned);
    let latest_result = discover_latest();
    let (latest, tag, release_url) = match latest_result {
        Ok((version, tag, url)) => (Some(version), Some(tag), Some(url)),
        Err(_) => (None, None, None),
    };
    let state = if target.is_none() {
        UpdateState::UnsupportedTarget
    } else if install_kind != InstallKind::GithubRelease {
        UpdateState::Ineligible
    } else if let Some(latest) = latest.as_ref() {
        version_state(&current, latest)
    } else {
        UpdateState::CheckFailed
    };
    let recommended_action = recommended_action(state, install_kind);
    Inspection {
        status: UpdateStatus {
            current: current.to_string(),
            latest: latest.as_ref().map(ToString::to_string),
            state,
            install_kind,
            path: path.to_string_lossy().into_owned(),
            target,
            release_url,
            recommended_action,
        },
        path,
        baseline,
        latest,
        tag,
    }
}

fn version_state(current: &Version, latest: &Version) -> UpdateState {
    match current.cmp_precedence(latest) {
        Ordering::Less => UpdateState::UpdateAvailable,
        Ordering::Equal => UpdateState::Current,
        Ordering::Greater => UpdateState::NewerThanLatest,
    }
}

fn discover_latest() -> io::Result<(Version, String, String)> {
    let bytes = ssh_bootstrap::download_latest_release_metadata(&env::temp_dir())?;
    parse_latest(&bytes)
}

fn parse_latest(bytes: &[u8]) -> io::Result<(Version, String, String)> {
    let release: LatestRelease = serde_json::from_slice(bytes).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "latest release metadata is invalid",
        )
    })?;
    if release.html_url
        != format!(
            "https://github.com/gardnmi/boomux/releases/tag/{}",
            release.tag_name
        )
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "latest release URL is not an official Boomux release",
        ));
    }
    let value = release.tag_name.strip_prefix('v').ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidData, "latest release tag is invalid")
    })?;
    let version = Version::parse(value)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "latest release tag is invalid"))?;
    if version.pre.is_empty() && version.build.is_empty() {
        Ok((version, release.tag_name, release.html_url))
    } else {
        Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "latest release tag must be a stable semantic version",
        ))
    }
}

fn release_target() -> Option<&'static str> {
    match (env::consts::OS, env::consts::ARCH) {
        ("linux", "x86_64") => Some("x86_64-unknown-linux-gnu"),
        ("linux", "aarch64") => Some("aarch64-unknown-linux-gnu"),
        _ => None,
    }
}

fn classify_installation(
    executable: &Path,
    home: Option<&Path>,
    distribution: Option<&str>,
) -> (InstallKind, Option<FileFingerprint>) {
    let Ok(metadata) = fs::symlink_metadata(executable) else {
        return (InstallKind::Unknown, None);
    };
    let uid = unsafe { libc::geteuid() };
    if is_package_path(executable) {
        return (InstallKind::PackageManaged, None);
    }
    if metadata.uid() == 0 {
        return (InstallKind::RootOwned, None);
    }
    if distribution != Some("github-release") {
        return (
            if cfg!(debug_assertions) {
                InstallKind::DevelopmentBuild
            } else {
                InstallKind::SourceBuild
            },
            None,
        );
    }
    let Some(home) = home.filter(|path| path.is_absolute()) else {
        return (InstallKind::Unknown, None);
    };
    let expected = home.join(".local/bin/boomux");
    if executable != expected {
        return (InstallKind::Custom, None);
    }
    if validate_install_path(home, &expected, uid).is_err() {
        return (InstallKind::Unknown, None);
    }
    match fingerprint(&expected) {
        Ok(baseline) => (InstallKind::GithubRelease, Some(baseline)),
        Err(_) => (InstallKind::Unknown, None),
    }
}

fn is_package_path(path: &Path) -> bool {
    [
        "/usr/bin/boomux",
        "/usr/local/bin/boomux",
        "/opt/homebrew/bin/boomux",
        "/home/linuxbrew/.linuxbrew/bin/boomux",
        "/run/current-system/sw/bin/boomux",
    ]
    .iter()
    .any(|candidate| path == Path::new(candidate))
        || path.to_string_lossy().contains("/.nix-profile/")
        || path.to_string_lossy().contains("/.local/share/mise/")
}

fn validate_install_path(home: &Path, target: &Path, uid: u32) -> io::Result<()> {
    for directory in [
        home.to_path_buf(),
        home.join(".local"),
        home.join(".local/bin"),
    ] {
        let metadata = fs::symlink_metadata(&directory)?;
        if !metadata.is_dir() || metadata.file_type().is_symlink() || metadata.uid() != uid {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "unsafe install path",
            ));
        }
        if metadata.mode() & 0o022 != 0 {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "install path is writable by another user",
            ));
        }
    }
    let metadata = fs::symlink_metadata(target)?;
    if !metadata.is_file()
        || metadata.file_type().is_symlink()
        || metadata.uid() != uid
        || metadata.mode() & 0o111 == 0
        || metadata.nlink() != 1
        || metadata.len() == 0
        || metadata.len() > MAX_EXECUTABLE_BYTES
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "install target is not a safe owner-controlled executable",
        ));
    }
    Ok(())
}

fn fingerprint(path: &Path) -> io::Result<FileFingerprint> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)?;
    fingerprint_file(file)
}

fn fingerprint_following(path: &Path) -> io::Result<FileFingerprint> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_CLOEXEC)
        .open(path)?;
    fingerprint_file(file)
}

fn fingerprint_file(mut file: File) -> io::Result<FileFingerprint> {
    let before = file.metadata()?;
    if !before.is_file() || before.len() == 0 || before.len() > MAX_EXECUTABLE_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "unsafe executable",
        ));
    }
    let mut digest = Sha256::new();
    io::copy(&mut file, &mut digest)?;
    let after = file.metadata()?;
    let value = FileFingerprint {
        device: before.dev(),
        inode: before.ino(),
        length: before.len(),
        modified_seconds: before.mtime(),
        modified_nanoseconds: before.mtime_nsec(),
        changed_seconds: before.ctime(),
        changed_nanoseconds: before.ctime_nsec(),
        owner: before.uid(),
        mode: before.mode(),
        links: before.nlink(),
        digest: digest.finalize().into(),
    };
    if value
        != (FileFingerprint {
            device: after.dev(),
            inode: after.ino(),
            length: after.len(),
            modified_seconds: after.mtime(),
            modified_nanoseconds: after.mtime_nsec(),
            changed_seconds: after.ctime(),
            changed_nanoseconds: after.ctime_nsec(),
            owner: after.uid(),
            mode: after.mode(),
            links: after.nlink(),
            digest: value.digest,
        })
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "executable changed while it was being pinned",
        ));
    }
    Ok(value)
}

fn write_candidate(target: &Path, bytes: &[u8]) -> io::Result<PathBuf> {
    if bytes.is_empty() || bytes.len() as u64 > MAX_EXECUTABLE_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "candidate is not bounded",
        ));
    }
    let parent = target
        .parent()
        .ok_or_else(|| io::Error::other("install path has no parent"))?;
    let candidate = parent.join(format!(".boomux-update-{}", Uuid::new_v4()));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o700)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(&candidate)?;
    file.write_all(bytes)?;
    let mode = fs::symlink_metadata(target)?.mode() & 0o777;
    fs::set_permissions(&candidate, fs::Permissions::from_mode(mode))?;
    file.sync_all()?;
    Ok(candidate)
}

fn smoke_test(candidate: &Path, expected: &Version) -> io::Result<()> {
    let mut command = Command::new(candidate);
    command
        .arg("--version")
        .stdin(Stdio::null())
        .stderr(Stdio::piped())
        .stdout(Stdio::piped());
    // Isolate the candidate so a failed smoke test can terminate descendants too.
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                Err(io::Error::last_os_error())
            } else {
                Ok(())
            }
        });
    }
    let mut child = command.spawn()?;
    let pid = i32::try_from(child.id()).map_err(|_| io::Error::other("invalid candidate PID"))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| io::Error::other("candidate stdout was not captured"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| io::Error::other("candidate stderr was not captured"))?;
    let (sender, receiver) = mpsc::channel();
    spawn_smoke_reader(stdout, 0, sender.clone());
    spawn_smoke_reader(stderr, 1, sender.clone());
    drop(sender);
    let deadline = Instant::now() + SMOKE_TIMEOUT;
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = unsafe { libc::kill(-pid, libc::SIGKILL) };
            let _ = child.wait();
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "downloaded Boomux candidate smoke test timed out",
            ));
        }
        thread::sleep(Duration::from_millis(10));
    };
    let _ = unsafe { libc::kill(-pid, libc::SIGKILL) };
    let mut output = [None, None];
    for _ in 0..2 {
        let (index, result) = receiver
            .recv_timeout(Duration::from_secs(1))
            .map_err(|_| io::Error::other("candidate output did not close"))?;
        output[index] = Some(result?);
    }
    let [Some(stdout), Some(stderr)] = output else {
        return Err(io::Error::other("candidate output was incomplete"));
    };
    let expected = format!("boomux {expected}\n");
    if !status.success()
        || stdout != expected.as_bytes()
        || stderr.len() > MAX_SMOKE_OUTPUT_BYTES
        || stdout.len() > MAX_SMOKE_OUTPUT_BYTES
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "downloaded Boomux candidate failed its version smoke test",
        ));
    }
    Ok(())
}

fn spawn_smoke_reader(
    mut stream: impl Read + Send + 'static,
    index: usize,
    sender: mpsc::Sender<(usize, io::Result<Vec<u8>>)>,
) {
    thread::spawn(move || {
        let mut bytes = Vec::new();
        let result = Read::by_ref(&mut stream)
            .take((MAX_SMOKE_OUTPUT_BYTES + 1) as u64)
            .read_to_end(&mut bytes)
            .map(|_| bytes);
        let _ = sender.send((index, result));
    });
}

fn replace_with_rollback(
    target: &Path,
    candidate: &Path,
    baseline: &FileFingerprint,
    candidate_fingerprint: &FileFingerprint,
    after_replace: impl FnOnce() -> io::Result<()>,
    after_restore: impl FnOnce() -> io::Result<()>,
) -> io::Result<()> {
    replace_with_rollback_using(
        &RealTransactionFs,
        ReplacementTransaction {
            target,
            candidate,
            baseline,
            candidate_fingerprint,
        },
        after_replace,
        after_restore,
        |warning| eprintln!("boomux: {warning}"),
    )
}

trait TransactionFs {
    fn hard_link(&self, source: &Path, destination: &Path) -> io::Result<()>;
    fn rename(&self, source: &Path, destination: &Path) -> io::Result<()>;
    fn remove_file(&self, path: &Path) -> io::Result<()>;
    fn sync_directory(&self, path: &Path) -> io::Result<()>;
}

struct RealTransactionFs;

#[derive(Clone, Copy)]
struct ReplacementTransaction<'a> {
    target: &'a Path,
    candidate: &'a Path,
    baseline: &'a FileFingerprint,
    candidate_fingerprint: &'a FileFingerprint,
}

impl TransactionFs for RealTransactionFs {
    fn hard_link(&self, source: &Path, destination: &Path) -> io::Result<()> {
        fs::hard_link(source, destination)
    }

    fn rename(&self, source: &Path, destination: &Path) -> io::Result<()> {
        fs::rename(source, destination)
    }

    fn remove_file(&self, path: &Path) -> io::Result<()> {
        fs::remove_file(path)
    }

    fn sync_directory(&self, path: &Path) -> io::Result<()> {
        sync_directory(path)
    }
}

fn replace_with_rollback_using(
    operations: &impl TransactionFs,
    transaction: ReplacementTransaction<'_>,
    after_replace: impl FnOnce() -> io::Result<()>,
    after_restore: impl FnOnce() -> io::Result<()>,
    mut warn: impl FnMut(&'static str),
) -> io::Result<()> {
    let ReplacementTransaction {
        target,
        candidate,
        baseline,
        candidate_fingerprint,
    } = transaction;
    if &fingerprint(target)? != baseline {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "installed Boomux changed while the update was pending",
        ));
    }
    let parent = target
        .parent()
        .ok_or_else(|| io::Error::other("install path has no parent"))?;
    if candidate.parent() != Some(parent) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "candidate is not in the install directory",
        ));
    }
    if &fingerprint(candidate)? != candidate_fingerprint {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "downloaded Boomux candidate changed before activation",
        ));
    }
    let backup = parent.join(format!(".boomux-backup-{}", Uuid::new_v4()));
    operations.hard_link(target, &backup)?;
    if let Err(error) = operations.sync_directory(parent) {
        let _ = operations.remove_file(&backup);
        return Err(error);
    }
    if !same_file_after_backup(&fingerprint(target)?, baseline) {
        let _ = operations.remove_file(&backup);
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "installed Boomux changed before replacement",
        ));
    }
    if &fingerprint(candidate)? != candidate_fingerprint {
        let _ = operations.remove_file(&backup);
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "downloaded Boomux candidate changed before replacement",
        ));
    }
    if let Err(error) = operations.rename(candidate, target) {
        let _ = operations.remove_file(&backup);
        let _ = operations.sync_directory(parent);
        return Err(error);
    }
    if let Err(error) = operations.sync_directory(parent) {
        return Err(restore_after_failure(
            operations, &backup, target, parent, error,
        ));
    }
    let installed = match fingerprint(target) {
        Ok(installed) => installed,
        Err(error) => {
            return Err(restore_after_failure(
                operations, &backup, target, parent, error,
            ));
        }
    };
    if !same_candidate(&installed, candidate_fingerprint) {
        return Err(restore_after_failure(
            operations,
            &backup,
            target,
            parent,
            io::Error::new(
                io::ErrorKind::InvalidData,
                "installed Boomux does not match the pinned candidate",
            ),
        ));
    }
    match after_replace() {
        Ok(()) => {
            match operations.remove_file(&backup) {
                Ok(()) => {
                    if operations.sync_directory(parent).is_err() {
                        warn(
                            "update committed and its rollback backup was removed, but backup cleanup could not be synchronized",
                        );
                    }
                }
                Err(_) => warn(
                    "update committed, but its rollback backup could not be removed; a hidden recovery artifact remains",
                ),
            }
            Ok(())
        }
        Err(error) => {
            if let Err(restore) = operations.rename(&backup, target) {
                return Err(io::Error::other(format!(
                    "update failed ({error}) and rollback failed ({restore})"
                )));
            }
            let sync = operations.sync_directory(parent);
            let recovery = after_restore();
            match (sync, recovery) {
                (Ok(()), Ok(())) => Err(error),
                (Err(sync), Ok(())) => Err(io::Error::other(format!(
                    "update failed ({error}); executable rollback was restored and daemon recovery succeeded, but rollback synchronization failed ({sync})"
                ))),
                (Ok(()), Err(recovery)) => Err(io::Error::other(format!(
                    "update failed ({error}); executable rollback was synchronized but daemon recovery failed ({recovery})"
                ))),
                (Err(sync), Err(recovery)) => Err(io::Error::other(format!(
                    "update failed ({error}); executable rollback was restored but synchronization failed ({sync}) and daemon recovery failed ({recovery})"
                ))),
            }
        }
    }
}

fn restore_after_failure(
    operations: &impl TransactionFs,
    backup: &Path,
    target: &Path,
    parent: &Path,
    failure: io::Error,
) -> io::Error {
    match operations
        .rename(backup, target)
        .and_then(|()| operations.sync_directory(parent))
    {
        Ok(()) => failure,
        Err(restore) => io::Error::other(format!(
            "update failed ({failure}) and rollback failed ({restore})"
        )),
    }
}

fn sync_directory(path: &Path) -> io::Result<()> {
    File::open(path)?.sync_all()
}

fn same_file_after_backup(current: &FileFingerprint, baseline: &FileFingerprint) -> bool {
    current.device == baseline.device
        && current.inode == baseline.inode
        && current.length == baseline.length
        && current.modified_seconds == baseline.modified_seconds
        && current.modified_nanoseconds == baseline.modified_nanoseconds
        && current.owner == baseline.owner
        && current.mode == baseline.mode
        && current.links == baseline.links + 1
        && current.digest == baseline.digest
}

fn same_candidate(current: &FileFingerprint, candidate: &FileFingerprint) -> bool {
    current.device == candidate.device
        && current.inode == candidate.inode
        && current.length == candidate.length
        && current.modified_seconds == candidate.modified_seconds
        && current.modified_nanoseconds == candidate.modified_nanoseconds
        && current.owner == candidate.owner
        && current.mode == candidate.mode
        && current.links == candidate.links
        && current.digest == candidate.digest
}

enum DaemonDisposition {
    Absent {
        socket: PathBuf,
        _reservation: client::DaemonLockReservation,
    },
    SameExecutable {
        client: client::Client,
        pid: u32,
    },
}

fn daemon_disposition(target: &Path) -> io::Result<DaemonDisposition> {
    let socket = client::socket_path()?;
    match fs::symlink_metadata(&socket) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            let parent = socket
                .parent()
                .ok_or_else(|| io::Error::other("daemon socket has no parent"))?;
            ssh_bootstrap::secure_runtime_directory(parent)?;
            let reservation = reserve_daemon_absence(&socket)?;
            return Ok(DaemonDisposition::Absent {
                socket,
                _reservation: reservation,
            });
        }
        Err(error) => return Err(error),
        Ok(_) => {}
    }
    let daemon = client::connect().map_err(|error| {
        io::Error::new(
            io::ErrorKind::ConnectionRefused,
            format!("running daemon could not be safely identified: {error}"),
        )
    })?;
    let executable = daemon_executable(&daemon).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::PermissionDenied,
            "running daemon executable is unknown",
        )
    })?;
    let installed = fingerprint(target)?;
    if executable.path != target || !same_candidate(&executable.fingerprint, &installed) {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "running daemon uses a different Boomux executable or executable inode",
        ));
    }
    let pid = executable.pid;
    Ok(DaemonDisposition::SameExecutable {
        client: daemon,
        pid,
    })
}

fn reserve_daemon_absence(socket: &Path) -> io::Result<client::DaemonLockReservation> {
    client::reserve_daemon_lock(socket)?.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::ResourceBusy,
            "daemon socket is absent but its ownership lock is still held",
        )
    })
}

#[cfg(target_os = "linux")]
struct DaemonExecutable {
    pid: u32,
    path: PathBuf,
    fingerprint: FileFingerprint,
}

#[cfg(target_os = "linux")]
fn daemon_executable(daemon: &client::Client) -> Option<DaemonExecutable> {
    let credentials = daemon.daemon_process_credentials().ok()?;
    if credentials.uid != unsafe { libc::geteuid() } {
        return None;
    }
    let process_directory = PathBuf::from(format!("/proc/{}", credentials.pid));
    let process_metadata = fs::metadata(&process_directory).ok()?;
    if !process_metadata.is_dir() || process_metadata.uid() != credentials.uid {
        return None;
    }
    let process_executable = process_directory.join("exe");
    let executable = fs::read_link(&process_executable).ok()?;
    let bytes = executable.as_os_str().as_encoded_bytes();
    let bytes = bytes.strip_suffix(b" (deleted)").unwrap_or(bytes);
    let path = PathBuf::from(std::ffi::OsString::from_vec(bytes.to_vec()));
    let fingerprint = fingerprint_following(&process_executable).ok()?;
    let confirmed = daemon.daemon_process_credentials().ok()?;
    if confirmed != credentials {
        return None;
    }
    Some(DaemonExecutable {
        pid: credentials.pid,
        path,
        fingerprint,
    })
}

#[cfg(not(target_os = "linux"))]
struct DaemonExecutable {
    pid: u32,
    path: PathBuf,
    fingerprint: FileFingerprint,
}

#[cfg(not(target_os = "linux"))]
fn daemon_executable(_daemon: &client::Client) -> Option<DaemonExecutable> {
    None
}

fn finish_daemon_handoff(
    disposition: &DaemonDisposition,
    target: &Path,
    candidate: &FileFingerprint,
) -> io::Result<()> {
    match disposition {
        DaemonDisposition::Absent { socket, .. } => {
            if fs::symlink_metadata(socket).is_ok() {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    "daemon appeared while the update was being installed",
                ));
            }
            verify_installed_candidate(target, candidate).map(drop)
        }
        DaemonDisposition::SameExecutable { client, pid } => {
            client
                .restart()
                .map_err(|error| io::Error::other(error.to_string()))?;
            let installed = verify_installed_candidate(target, candidate)?;
            let executable = replacement_daemon_executable(client, target, candidate, *pid)
                .ok_or_else(|| {
                    io::Error::other("replacement daemon executable could not be verified")
                })?;
            if executable.path != target
                || !same_candidate(&executable.fingerprint, candidate)
                || !same_candidate(&executable.fingerprint, &installed)
            {
                return Err(io::Error::other(
                    "replacement daemon did not use the exact pinned candidate",
                ));
            }
            Ok(())
        }
    }
}

fn verify_installed_candidate(
    target: &Path,
    candidate: &FileFingerprint,
) -> io::Result<FileFingerprint> {
    let installed = fingerprint(target)?;
    if !same_candidate(&installed, candidate) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "installed Boomux no longer matches the pinned candidate",
        ));
    }
    Ok(installed)
}

fn recover_daemon_after_rollback(disposition: &DaemonDisposition) -> io::Result<()> {
    match disposition {
        DaemonDisposition::Absent { .. } => Ok(()),
        DaemonDisposition::SameExecutable { .. } => client::connect()
            .and_then(|daemon| daemon.restart())
            .map_err(|error| io::Error::other(error.to_string())),
    }
}

#[cfg(target_os = "linux")]
fn replacement_daemon_executable(
    daemon: &client::Client,
    target: &Path,
    candidate: &FileFingerprint,
    old_pid: u32,
) -> Option<DaemonExecutable> {
    let deadline = Instant::now() + SMOKE_TIMEOUT;
    loop {
        if let Some(executable) = daemon_executable(daemon).filter(|executable| {
            executable.pid != old_pid
                && executable.path == target
                && same_candidate(&executable.fingerprint, candidate)
                && read_proc_cmdline(
                    &PathBuf::from(format!("/proc/{}", executable.pid)).join("cmdline"),
                )
                .is_ok_and(|cmdline| is_replacement_daemon_cmdline(&cmdline, target))
                && daemon.daemon_process_credentials().is_ok_and(|confirmed| {
                    confirmed.pid == executable.pid && confirmed.uid == unsafe { libc::geteuid() }
                })
        }) {
            return Some(executable);
        }
        if Instant::now() >= deadline {
            return None;
        }
        thread::sleep(Duration::from_millis(10));
    }
}

#[cfg(target_os = "linux")]
fn read_proc_cmdline(path: &Path) -> io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    File::open(path)?
        .take(MAX_PROC_CMDLINE_BYTES + 1)
        .read_to_end(&mut bytes)?;
    if bytes.is_empty() || bytes.len() as u64 > MAX_PROC_CMDLINE_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "daemon command line is invalid",
        ));
    }
    Ok(bytes)
}

#[cfg(target_os = "linux")]
fn is_replacement_daemon_cmdline(cmdline: &[u8], target: &Path) -> bool {
    let arguments = cmdline
        .split(|byte| *byte == 0)
        .filter(|argument| !argument.is_empty())
        .collect::<Vec<_>>();
    arguments.len() == 5
        && arguments[0] == target.as_os_str().as_encoded_bytes()
        && arguments[1] == b"daemon"
        && arguments[2] == b"receive-handoff"
        && arguments[3] == b"--channel"
        && arguments[4].iter().all(u8::is_ascii_digit)
}

#[cfg(not(target_os = "linux"))]
fn replacement_daemon_executable(
    _daemon: &client::Client,
    _target: &Path,
    _candidate: &FileFingerprint,
    _old_pid: u32,
) -> Option<DaemonExecutable> {
    None
}

const fn recommended_action(state: UpdateState, kind: InstallKind) -> RecommendedAction {
    match (state, kind) {
        (_, InstallKind::PackageManaged | InstallKind::RootOwned) => {
            RecommendedAction::UsePackageManager
        }
        (UpdateState::UpdateAvailable, InstallKind::GithubRelease) => RecommendedAction::RunUpdate,
        (UpdateState::Current, _) => RecommendedAction::None,
        (UpdateState::NewerThanLatest, _) => RecommendedAction::KeepCurrent,
        (UpdateState::CheckFailed, _) => RecommendedAction::Retry,
        (UpdateState::UnsupportedTarget, _) => RecommendedAction::None,
        _ => RecommendedAction::InstallGithubRelease,
    }
}

fn refusal_message(status: &UpdateStatus) -> &'static str {
    match status.state {
        UpdateState::Current => "Boomux is already current",
        UpdateState::NewerThanLatest => {
            "installed Boomux is newer than the latest release; refusing to downgrade"
        }
        UpdateState::UnsupportedTarget => {
            "this platform has no supported Boomux self-update target"
        }
        UpdateState::CheckFailed => "latest Boomux release could not be verified",
        UpdateState::Ineligible => match status.install_kind {
            InstallKind::PackageManaged | InstallKind::RootOwned => {
                "this installation must be updated by its package or system manager"
            }
            _ => {
                "only official GitHub release builds installed at ~/.local/bin/boomux can self-update"
            }
        },
        UpdateState::UpdateAvailable => "this Boomux installation is not eligible for self-update",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::{Cell, RefCell};

    fn temporary_directory() -> PathBuf {
        let path = env::temp_dir().join(format!("boomux-update-test-{}", Uuid::new_v4()));
        fs::create_dir(&path).unwrap();
        path
    }

    #[test]
    fn strict_semver_rejects_partial_leading_zero_and_prerelease_latest() {
        assert!(Version::parse("1.2").is_err());
        assert!(Version::parse("1.02.3").is_err());
        let prerelease = Version::parse("1.2.3-rc.1").unwrap();
        assert!(!prerelease.pre.is_empty());
        assert!(parse_latest(
            br#"{"tag_name":"v1.2.3-rc.1","html_url":"https://github.com/gardnmi/boomux/releases/tag/v1.2.3-rc.1"}"#
        )
        .is_err());
        assert!(parse_latest(
            br#"{"tag_name":"1.2.3","html_url":"https://github.com/gardnmi/boomux/releases/tag/1.2.3"}"#
        )
        .is_err());
    }

    #[test]
    fn official_canonical_owner_installation_is_eligible() {
        let home = temporary_directory();
        let bin = home.join(".local/bin");
        fs::create_dir_all(&bin).unwrap();
        let executable = bin.join("boomux");
        fs::write(&executable, b"binary").unwrap();
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(
            classify_installation(&executable, Some(&home), Some("github-release")).0,
            InstallKind::GithubRelease
        );
        fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn uninstall_removes_the_unchanged_pinned_executable_and_syncs_parent() {
        let home = temporary_directory();
        let bin = home.join(".local/bin");
        fs::create_dir_all(&bin).unwrap();
        let path = bin.join("boomux");
        fs::write(&path, b"binary").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        let target = UninstallTarget {
            baseline: fingerprint(&path).unwrap(),
            path: path.clone(),
        };

        remove_uninstall_target(&target).unwrap();
        assert!(!path.exists());
        fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn uninstall_refuses_an_executable_changed_after_authorization() {
        let home = temporary_directory();
        let bin = home.join(".local/bin");
        fs::create_dir_all(&bin).unwrap();
        let path = bin.join("boomux");
        fs::write(&path, b"binary").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        let target = UninstallTarget {
            baseline: fingerprint(&path).unwrap(),
            path: path.clone(),
        };
        fs::write(&path, b"changed").unwrap();

        assert_eq!(
            remove_uninstall_target(&target).unwrap_err().kind(),
            io::ErrorKind::PermissionDenied
        );
        assert_eq!(fs::read(&path).unwrap(), b"changed");
        fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn never_recommends_downgrade() {
        assert_eq!(
            version_state(
                &Version::parse("2.0.0").unwrap(),
                &Version::parse("1.9.9").unwrap()
            ),
            UpdateState::NewerThanLatest
        );
        assert_eq!(
            recommended_action(UpdateState::NewerThanLatest, InstallKind::GithubRelease),
            RecommendedAction::KeepCurrent
        );
    }

    #[test]
    fn package_and_custom_installations_are_classified_conservatively() {
        let directory = temporary_directory();
        let executable = directory.join("boomux");
        fs::write(&executable, b"binary").unwrap();
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(
            classify_installation(&executable, Some(&directory), Some("github-release")).0,
            InstallKind::Custom
        );
        assert!(is_package_path(Path::new("/usr/bin/boomux")));
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn missing_socket_is_not_absent_while_daemon_lock_is_held() {
        use std::os::fd::AsRawFd;

        let directory = temporary_directory();
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(directory.join("daemon.lock"))
            .unwrap();
        assert_eq!(
            unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
            0
        );
        let error = reserve_daemon_absence(&directory.join("daemon.sock")).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::ResourceBusy);
        let _ = unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_UN) };
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn package_manager_guidance_wins_on_unsupported_targets() {
        assert_eq!(
            recommended_action(UpdateState::UnsupportedTarget, InstallKind::PackageManaged),
            RecommendedAction::UsePackageManager
        );
        assert_eq!(
            recommended_action(UpdateState::UnsupportedTarget, InstallKind::SourceBuild),
            RecommendedAction::None
        );
    }

    #[test]
    fn unsafe_target_symlink_is_ineligible() {
        let directory = temporary_directory();
        let local = directory.join(".local");
        fs::create_dir(&local).unwrap();
        std::os::unix::fs::symlink(&directory, local.join("bin")).unwrap();
        assert!(
            validate_install_path(&directory, &local.join("bin/boomux"), unsafe {
                libc::geteuid()
            })
            .is_err()
        );
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn candidate_smoke_test_requires_exact_version_output() {
        let directory = temporary_directory();
        let candidate = directory.join("candidate");
        fs::write(&candidate, b"#!/bin/sh\nprintf 'boomux 1.2.3\\n'\n").unwrap();
        fs::set_permissions(&candidate, fs::Permissions::from_mode(0o700)).unwrap();
        smoke_test(&candidate, &Version::parse("1.2.3").unwrap()).unwrap();
        assert!(smoke_test(&candidate, &Version::parse("1.2.4").unwrap()).is_err());
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn candidate_identity_requires_the_pinned_inode_and_digest() {
        let directory = temporary_directory();
        let candidate = directory.join("candidate");
        let copy = directory.join("copy");
        fs::write(&candidate, b"candidate").unwrap();
        fs::write(&copy, b"candidate").unwrap();
        let pinned = fingerprint(&candidate).unwrap();
        assert!(!same_candidate(&fingerprint(&copy).unwrap(), &pinned));
        fs::write(&candidate, b"tampered!").unwrap();
        assert!(!same_candidate(&fingerprint(&candidate).unwrap(), &pinned));
        fs::remove_dir_all(directory).unwrap();
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn replacement_daemon_cmdline_is_exact_and_bounded() {
        let target = Path::new("/home/person/.local/bin/boomux");
        assert!(is_replacement_daemon_cmdline(
            b"/home/person/.local/bin/boomux\0daemon\0receive-handoff\0--channel\x00198\0",
            target
        ));
        assert!(!is_replacement_daemon_cmdline(
            b"/home/person/.local/bin/boomux\0daemon\0run\0",
            target
        ));
        assert!(!is_replacement_daemon_cmdline(
            b"/other/boomux\0daemon\0receive-handoff\0--channel\x00198\0",
            target
        ));
    }

    #[test]
    fn replacement_commits_and_removes_backup() {
        let directory = temporary_directory();
        let target = directory.join("boomux");
        let candidate = directory.join("candidate");
        fs::write(&target, b"old").unwrap();
        fs::write(&candidate, b"new").unwrap();
        let baseline = fingerprint(&target).unwrap();
        let candidate_fingerprint = fingerprint(&candidate).unwrap();
        replace_with_rollback(
            &target,
            &candidate,
            &baseline,
            &candidate_fingerprint,
            || {
                verify_installed_candidate(&target, &candidate_fingerprint)?;
                Ok(())
            },
            || Ok(()),
        )
        .unwrap();
        assert_eq!(fs::read(&target).unwrap(), b"new");
        assert!(same_candidate(
            &fingerprint(&target).unwrap(),
            &candidate_fingerprint
        ));
        assert_eq!(fs::read_dir(&directory).unwrap().count(), 1);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn replacement_rolls_back_after_post_install_failure() {
        let directory = temporary_directory();
        let target = directory.join("boomux");
        let candidate = directory.join("candidate");
        fs::write(&target, b"old").unwrap();
        fs::write(&candidate, b"new").unwrap();
        let baseline = fingerprint(&target).unwrap();
        let candidate_fingerprint = fingerprint(&candidate).unwrap();
        let recovered = Cell::new(false);
        let error = replace_with_rollback(
            &target,
            &candidate,
            &baseline,
            &candidate_fingerprint,
            || Err(io::Error::other("handoff failed")),
            || {
                recovered.set(true);
                Ok(())
            },
        )
        .unwrap_err();
        assert_eq!(error.to_string(), "handoff failed");
        assert_eq!(fs::read(&target).unwrap(), b"old");
        assert!(recovered.get());
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn replacement_rejects_concurrent_target_change() {
        let directory = temporary_directory();
        let target = directory.join("boomux");
        let candidate = directory.join("candidate");
        fs::write(&target, b"old").unwrap();
        let baseline = fingerprint(&target).unwrap();
        fs::write(&target, b"changed").unwrap();
        fs::write(&candidate, b"new").unwrap();
        let candidate_fingerprint = fingerprint(&candidate).unwrap();
        assert!(
            replace_with_rollback(
                &target,
                &candidate,
                &baseline,
                &candidate_fingerprint,
                || Ok(()),
                || Ok(())
            )
            .is_err()
        );
        assert_eq!(fs::read(&target).unwrap(), b"changed");
        fs::remove_dir_all(directory).unwrap();
    }

    struct FailSyncFs {
        fail_on: usize,
        fail_remove: bool,
        sync_calls: Cell<usize>,
        renames: RefCell<Vec<(PathBuf, PathBuf)>>,
    }

    impl TransactionFs for FailSyncFs {
        fn hard_link(&self, source: &Path, destination: &Path) -> io::Result<()> {
            fs::hard_link(source, destination)
        }

        fn rename(&self, source: &Path, destination: &Path) -> io::Result<()> {
            self.renames
                .borrow_mut()
                .push((source.to_path_buf(), destination.to_path_buf()));
            fs::rename(source, destination)
        }

        fn remove_file(&self, path: &Path) -> io::Result<()> {
            if self.fail_remove {
                Err(io::Error::other("injected backup removal failure"))
            } else {
                fs::remove_file(path)
            }
        }

        fn sync_directory(&self, path: &Path) -> io::Result<()> {
            let call = self.sync_calls.get() + 1;
            self.sync_calls.set(call);
            if call == self.fail_on {
                Err(io::Error::other("injected directory sync failure"))
            } else {
                sync_directory(path)
            }
        }
    }

    #[test]
    fn post_rename_sync_failure_restores_and_synchronizes_backup() {
        let directory = temporary_directory();
        let target = directory.join("boomux");
        let candidate = directory.join("candidate");
        fs::write(&target, b"old").unwrap();
        fs::write(&candidate, b"new").unwrap();
        let baseline = fingerprint(&target).unwrap();
        let candidate_fingerprint = fingerprint(&candidate).unwrap();
        let operations = FailSyncFs {
            fail_on: 2,
            fail_remove: false,
            sync_calls: Cell::new(0),
            renames: RefCell::new(Vec::new()),
        };
        let after_replace = Cell::new(false);

        let error = replace_with_rollback_using(
            &operations,
            ReplacementTransaction {
                target: &target,
                candidate: &candidate,
                baseline: &baseline,
                candidate_fingerprint: &candidate_fingerprint,
            },
            || {
                after_replace.set(true);
                Ok(())
            },
            || Ok(()),
            |_| {},
        )
        .unwrap_err();

        assert_eq!(error.to_string(), "injected directory sync failure");
        assert!(!after_replace.get());
        assert_eq!(operations.sync_calls.get(), 3);
        assert_eq!(operations.renames.borrow().len(), 2);
        assert_eq!(fs::read(&target).unwrap(), b"old");
        assert!(!candidate.exists());
        assert_eq!(fs::read_dir(&directory).unwrap().count(), 1);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn rollback_sync_and_daemon_recovery_failures_are_both_reported() {
        let directory = temporary_directory();
        let target = directory.join("boomux");
        let candidate = directory.join("candidate");
        fs::write(&target, b"old").unwrap();
        fs::write(&candidate, b"new").unwrap();
        let baseline = fingerprint(&target).unwrap();
        let candidate_fingerprint = fingerprint(&candidate).unwrap();
        let operations = FailSyncFs {
            fail_on: 3,
            fail_remove: false,
            sync_calls: Cell::new(0),
            renames: RefCell::new(Vec::new()),
        };
        let recovered = Cell::new(false);

        let error = replace_with_rollback_using(
            &operations,
            ReplacementTransaction {
                target: &target,
                candidate: &candidate,
                baseline: &baseline,
                candidate_fingerprint: &candidate_fingerprint,
            },
            || Err(io::Error::other("handoff verification failed")),
            || {
                recovered.set(true);
                Err(io::Error::other("daemon recovery failed"))
            },
            |_| {},
        )
        .unwrap_err();

        assert!(recovered.get());
        assert_eq!(operations.sync_calls.get(), 3);
        assert_eq!(fs::read(&target).unwrap(), b"old");
        assert!(error.to_string().contains("rollback was restored"));
        assert!(error.to_string().contains("synchronization failed"));
        assert!(error.to_string().contains("daemon recovery failed"));
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn committed_update_keeps_backup_and_warns_when_removal_fails() {
        let directory = temporary_directory();
        let target = directory.join("boomux");
        let candidate = directory.join("candidate");
        fs::write(&target, b"old").unwrap();
        fs::write(&candidate, b"new").unwrap();
        let baseline = fingerprint(&target).unwrap();
        let candidate_fingerprint = fingerprint(&candidate).unwrap();
        let operations = FailSyncFs {
            fail_on: usize::MAX,
            fail_remove: true,
            sync_calls: Cell::new(0),
            renames: RefCell::new(Vec::new()),
        };
        let warnings = RefCell::new(Vec::new());

        replace_with_rollback_using(
            &operations,
            ReplacementTransaction {
                target: &target,
                candidate: &candidate,
                baseline: &baseline,
                candidate_fingerprint: &candidate_fingerprint,
            },
            || Ok(()),
            || Ok(()),
            |warning| warnings.borrow_mut().push(warning),
        )
        .unwrap();

        assert_eq!(fs::read(&target).unwrap(), b"new");
        assert_eq!(fs::read_dir(&directory).unwrap().count(), 2);
        assert_eq!(warnings.borrow().len(), 1);
        assert!(warnings.borrow()[0].contains("recovery artifact remains"));
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn committed_update_warns_but_succeeds_when_cleanup_sync_fails() {
        let directory = temporary_directory();
        let target = directory.join("boomux");
        let candidate = directory.join("candidate");
        fs::write(&target, b"old").unwrap();
        fs::write(&candidate, b"new").unwrap();
        let baseline = fingerprint(&target).unwrap();
        let candidate_fingerprint = fingerprint(&candidate).unwrap();
        let operations = FailSyncFs {
            fail_on: 3,
            fail_remove: false,
            sync_calls: Cell::new(0),
            renames: RefCell::new(Vec::new()),
        };
        let warnings = RefCell::new(Vec::new());

        replace_with_rollback_using(
            &operations,
            ReplacementTransaction {
                target: &target,
                candidate: &candidate,
                baseline: &baseline,
                candidate_fingerprint: &candidate_fingerprint,
            },
            || Ok(()),
            || Ok(()),
            |warning| warnings.borrow_mut().push(warning),
        )
        .unwrap();

        assert_eq!(fs::read(&target).unwrap(), b"new");
        assert_eq!(fs::read_dir(&directory).unwrap().count(), 1);
        assert_eq!(operations.sync_calls.get(), 3);
        assert_eq!(warnings.borrow().len(), 1);
        assert!(warnings.borrow()[0].contains("could not be synchronized"));
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn replacement_rejects_a_candidate_changed_after_pinning() {
        let directory = temporary_directory();
        let target = directory.join("boomux");
        let candidate = directory.join("candidate");
        fs::write(&target, b"old").unwrap();
        fs::write(&candidate, b"new").unwrap();
        let baseline = fingerprint(&target).unwrap();
        let candidate_fingerprint = fingerprint(&candidate).unwrap();
        fs::write(&candidate, b"tampered").unwrap();

        assert!(
            replace_with_rollback(
                &target,
                &candidate,
                &baseline,
                &candidate_fingerprint,
                || Ok(()),
                || Ok(())
            )
            .is_err()
        );
        assert_eq!(fs::read(&target).unwrap(), b"old");
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn installed_candidate_change_before_commit_rolls_back_old_executable() {
        let directory = temporary_directory();
        let target = directory.join("boomux");
        let candidate = directory.join("candidate");
        fs::write(&target, b"old").unwrap();
        fs::write(&candidate, b"new").unwrap();
        let baseline = fingerprint(&target).unwrap();
        let candidate_fingerprint = fingerprint(&candidate).unwrap();

        let error = replace_with_rollback(
            &target,
            &candidate,
            &baseline,
            &candidate_fingerprint,
            || {
                fs::write(&target, b"changed after activation")?;
                verify_installed_candidate(&target, &candidate_fingerprint).map(drop)
            },
            || Ok(()),
        )
        .unwrap_err();

        assert!(error.to_string().contains("pinned candidate"));
        assert_eq!(fs::read(&target).unwrap(), b"old");
        assert_eq!(fs::read_dir(&directory).unwrap().count(), 1);
        fs::remove_dir_all(directory).unwrap();
    }
}

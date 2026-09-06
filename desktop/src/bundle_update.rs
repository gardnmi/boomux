//! User-initiated whole-bundle updates. All functions run on a worker.
use semver::Version;
use std::{
    fs,
    io::{Read, Write},
    os::unix::{
        fs::{MetadataExt, PermissionsExt, symlink},
        process::CommandExt,
    },
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::{Duration, Instant},
};

#[derive(Clone, Debug)]
pub struct Installation {
    root: PathBuf,
    running: PathBuf,
}

#[derive(Clone, Debug)]
pub struct Prepared {
    installation: Installation,
    release: PathBuf,
    pub version: String,
    requires_pending: bool,
}

fn owned(path: &Path) -> Result<(), String> {
    let metadata = fs::symlink_metadata(path).map_err(|e| e.to_string())?;
    if metadata.file_type().is_symlink()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.permissions().mode() & 0o022 != 0
    {
        return Err(format!("Unsafe installation path: {}", path.display()));
    }
    Ok(())
}

fn release_version(path: &Path) -> Result<String, String> {
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or("Invalid release directory")?;
    let (tag, digest) = name.rsplit_once('-').ok_or("Invalid release directory")?;
    let version = tag.strip_prefix('v').ok_or("Invalid release version")?;
    stable(version)?;
    if digest.len() != 64
        || !digest
            .bytes()
            .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
    {
        return Err("Invalid release checksum".into());
    }
    Ok(version.into())
}

fn stable(version: &str) -> Result<Version, String> {
    let parsed = Version::parse(version).map_err(|e| e.to_string())?;
    if version.len() > 64 || !parsed.pre.is_empty() || !parsed.build.is_empty() {
        return Err("Expected a stable release version".into());
    }
    Ok(parsed)
}

impl Installation {
    pub fn discover() -> Option<Self> {
        Self::from_executable(&std::env::current_exe().ok()?).ok()
    }

    fn from_executable(executable: &Path) -> Result<Self, String> {
        let running = executable
            .parent()
            .and_then(Path::parent)
            .ok_or("Not an installed bundle")?
            .to_owned();
        let releases = running.parent().ok_or("Missing release directory")?;
        if releases.file_name().is_none_or(|name| name != "releases")
            || executable != running.join("libexec/boomux-desktop")
        {
            return Err("Install the official Desktop bundle to enable updates".into());
        }
        let root = releases
            .parent()
            .ok_or("Missing installation directory")?
            .to_owned();
        let installation = Self { root, running };
        installation.validate_release(&installation.running)?;
        installation.require_current()?;
        Ok(installation)
    }

    fn validate_release(&self, release: &Path) -> Result<String, String> {
        if release.parent() != Some(self.root.join("releases").as_path()) {
            return Err("Release is outside this installation".into());
        }
        for path in [&self.root, &self.root.join("releases"), release] {
            owned(path)?;
        }
        for relative in [
            "bin",
            "libexec",
            "release.txt",
            "bin/boomux",
            "libexec/boomux-desktop",
        ] {
            owned(&release.join(relative))?;
        }
        release_version(release)
    }

    fn require_current(&self) -> Result<(), String> {
        if fs::read_link(self.root.join("current")).map_err(|e| e.to_string())? != self.running {
            return Err("The installation changed. Reopen Desktop before updating.".into());
        }
        Ok(())
    }

    pub fn pending(&self) -> Result<Option<Prepared>, String> {
        let release = match fs::read_link(self.root.join("pending")) {
            Ok(path) => path,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e.to_string()),
        };
        let version = self.validate_release(&release)?;
        if stable(&version)? <= stable(&release_version(&self.running)?)? {
            return Ok(None);
        }
        Ok(Some(Prepared {
            installation: self.clone(),
            release,
            version,
            requires_pending: true,
        }))
    }

    pub fn finish_installation(&self) -> Result<Option<Prepared>, String> {
        let mut command = Command::new("timeout");
        command
            .args(["--kill-after=1s", "10s"])
            .arg(self.running.join("bin/boomux"))
            .args(["--json", "daemon", "status"]);
        let output = run(command, None)?;
        let status: serde_json::Value = serde_json::from_str(&output).map_err(|e| e.to_string())?;
        if status["schema"] != "boomux.cli/v1" || status["command"] != "daemon.status" {
            return Err("Unrecognized daemon status response".into());
        }
        let executable = status["data"]["executable"].as_str();
        if status["data"]["status"] != "running"
            || executable == self.running.join("bin/boomux").to_str()
        {
            return Ok(None);
        }
        if executable.is_none() {
            return Err("Cannot verify the running Boomux executable".into());
        }
        Ok(Some(Prepared {
            installation: self.clone(),
            release: self.running.clone(),
            version: release_version(&self.running)?,
            requires_pending: false,
        }))
    }

    pub fn prepare(&self, version: &str) -> Result<Prepared, String> {
        self.require_current()?;
        if stable(version)? <= stable(&release_version(&self.running)?)? {
            return Err("Only newer stable releases can be installed".into());
        }
        let mut command = Command::new("timeout");
        command
            .args(["--kill-after=2s", "1800s", "sh", "-s", "--", "--prepare"])
            .env("BOOMUX_DESKTOP_VERSION", format!("v{version}"))
            .env("BOOMUX_DESKTOP_INSTALL_DIR", &self.root);
        run(command, Some(include_bytes!("../install.sh")))?;
        self.require_current()?;
        let pending = self
            .pending()?
            .ok_or("The installer did not prepare an update")?;
        if pending.version != version {
            return Err("A different update was prepared; check again".into());
        }
        Ok(pending)
    }
}

struct Lock(PathBuf);
impl Drop for Lock {
    fn drop(&mut self) {
        let _ = fs::remove_dir(&self.0);
    }
}

fn switch(root: &Path, release: &Path) -> Result<(), String> {
    let temporary = root.join(".install-lock/current");
    symlink(release, &temporary).map_err(|e| e.to_string())?;
    let result = fs::rename(&temporary, root.join("current")).map_err(|e| e.to_string());
    let _ = fs::remove_file(temporary);
    result
}

fn restart_daemon(release: &Path) -> Result<(), String> {
    let mut command = Command::new("timeout");
    command
        .args(["--kill-after=2s", "45s"])
        .arg(release.join("bin/boomux"))
        .args(["daemon", "restart"]);
    run(command, None).map(|_| ())
}

impl Prepared {
    pub fn restart(&self) -> Result<(), String> {
        let installation = &self.installation;
        let lock_path = installation.root.join(".install-lock");
        fs::create_dir(&lock_path).map_err(|e| format!("Another installer may be active: {e}"))?;
        let lock = Lock(lock_path);
        installation.require_current()?;
        installation.validate_release(&self.release)?;
        if self.requires_pending {
            let pending = installation
                .pending()?
                .ok_or("The prepared update is no longer available")?;
            if pending.release != self.release {
                return Err("The prepared update changed; check again".into());
            }
        }
        restart_daemon(&self.release)?;
        let result = (|| {
            switch(&installation.root, &self.release)?;
            let ready = lock.0.join("ready");
            let mut paths = vec![self.release.join("bin")];
            paths.extend(std::env::split_paths(
                &std::env::var_os("PATH").unwrap_or_default(),
            ));
            let mut child = Command::new(self.release.join("libexec/boomux-desktop"))
                .arg("--update-ready")
                .arg(&ready)
                .env(
                    "PATH",
                    std::env::join_paths(paths).map_err(|e| e.to_string())?,
                )
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .map_err(|e| e.to_string())?;
            let deadline = Instant::now() + Duration::from_secs(30);
            loop {
                if child.try_wait().map_err(|e| e.to_string())?.is_some() {
                    break;
                }
                if ready.is_file() {
                    let _ = fs::remove_file(&ready);
                    let _ = fs::remove_file(installation.root.join("pending"));
                    return Ok(());
                }
                if Instant::now() >= deadline {
                    break;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            let _ = child.kill();
            let _ = child.wait();
            let _ = fs::remove_file(ready);
            Err("The updated window could not open".to_string())
        })();
        if let Err(error) = result {
            let restore = switch(&installation.root, &installation.running);
            let handoff = restart_daemon(&installation.running);
            return Err(format!(
                "{error}. Previous release restored: {}. Daemon recovery: {}.",
                restore.err().unwrap_or_else(|| "yes".into()),
                handoff.err().unwrap_or_else(|| "complete".into())
            ));
        }
        Ok(())
    }
}

fn run(mut command: Command, input: Option<&[u8]>) -> Result<String, String> {
    command
        .stdin(if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0);
    let mut child = command.spawn().map_err(|e| e.to_string())?;
    let pid = child.id() as i32;
    let collect = move |reader: Box<dyn Read + Send>| {
        std::thread::spawn(move || {
            let mut bytes = Vec::new();
            let result = reader.take(65537).read_to_end(&mut bytes);
            if bytes.len() > 65536 {
                unsafe {
                    libc::kill(-pid, libc::SIGKILL);
                }
            }
            result.map_err(|e| e.to_string())?;
            if bytes.len() > 65536 {
                return Err("Update command exceeded output limit".to_string());
            }
            Ok(String::from_utf8_lossy(&bytes).into_owned())
        })
    };
    let stdout = collect(Box::new(child.stdout.take().ok_or("Missing stdout")?));
    let stderr = collect(Box::new(child.stderr.take().ok_or("Missing stderr")?));
    if let Some(input) = input {
        let written = child.stdin.take().ok_or("Missing stdin")?.write_all(input);
        if written.is_err() {
            unsafe {
                libc::kill(-pid, libc::SIGKILL);
            }
        }
    }
    let status = child.wait().map_err(|e| e.to_string())?;
    let stdout = stdout.join().map_err(|_| "Update reader failed")??;
    let stderr = stderr.join().map_err(|_| "Update reader failed")??;
    if !status.success() {
        return Err(format!("Update failed ({status}): {stderr}\n{stdout}"));
    }
    Ok(stdout)
}

pub fn signal_ready(path: PathBuf) {
    // Invoked after GPUI creates the replacement window; never block GPUI on I/O.
    std::thread::spawn(move || {
        if let Ok(mut file) = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)
        {
            let _ = file.write_all(b"ready\n");
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            let root = std::env::temp_dir().join(format!(
                "boomux-bundle-update-{}-{}",
                std::process::id(),
                fastrand::u64(..)
            ));
            fs::create_dir(&root).unwrap();
            fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
            fs::create_dir(root.join("releases")).unwrap();
            Self(root)
        }
        fn release(&self, version: &str, opens: bool) -> PathBuf {
            let path = self
                .0
                .join("releases")
                .join(format!("v{version}-{}", "a".repeat(64)));
            fs::create_dir_all(path.join("bin")).unwrap();
            fs::create_dir(path.join("libexec")).unwrap();
            fs::write(
                path.join("release.txt"),
                format!("boomux-desktop {version}\nboomux {version}\n"),
            )
            .unwrap();
            // A trace provides evidence of handoff/recovery ordering, without
            // touching the developer's real daemon or display.
            fs::write(
                path.join("bin/boomux"),
                format!(
                    "#!/bin/sh\nprintf '{version} %s\\n' \"$*\" >> '{}'/handoffs\n",
                    self.0.display()
                ),
            )
            .unwrap();
            fs::write(path.join("libexec/boomux-desktop"), if opens {
                "#!/bin/sh\n[ \"$1\" = --update-ready ] || exit 2\nprintf ready > \"$2\"\nsleep 1\n"
            } else { "#!/bin/sh\nexit 3\n" }).unwrap();
            for executable in ["bin/boomux", "libexec/boomux-desktop"] {
                fs::set_permissions(path.join(executable), fs::Permissions::from_mode(0o755))
                    .unwrap();
            }
            path
        }
        fn prepare(&self, opens: bool) -> (Installation, Prepared) {
            let current = self.release("1.0.0", true);
            let next = self.release("1.1.0", opens);
            symlink(&current, self.0.join("current")).unwrap();
            symlink(next, self.0.join("pending")).unwrap();
            let installation =
                Installation::from_executable(&current.join("libexec/boomux-desktop")).unwrap();
            let prepared = installation.pending().unwrap().unwrap();
            (installation, prepared)
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn prepared_update_survives_reopen_without_switching_or_restarting() {
        let fixture = Fixture::new();
        let (installation, prepared) = fixture.prepare(true);
        assert_eq!(prepared.version, "1.1.0");
        installation.require_current().unwrap();
        assert!(!fixture.0.join("handoffs").exists());
        assert_eq!(
            installation.pending().unwrap().unwrap().release,
            prepared.release
        );
    }

    #[test]
    fn restart_hands_off_before_switching_and_waits_for_the_window() {
        let fixture = Fixture::new();
        let (_, prepared) = fixture.prepare(true);
        prepared.restart().unwrap();
        assert_eq!(
            fs::read_link(fixture.0.join("current")).unwrap(),
            prepared.release
        );
        assert!(!fixture.0.join("pending").exists());
        assert!(!fixture.0.join(".install-lock").exists());
        assert_eq!(
            fs::read_to_string(fixture.0.join("handoffs")).unwrap(),
            "1.1.0 daemon restart\n"
        );
    }

    #[test]
    fn failed_window_restores_previous_bundle_and_daemon_and_keeps_retry() {
        let fixture = Fixture::new();
        let (installation, prepared) = fixture.prepare(false);
        let error = prepared.restart().unwrap_err();
        assert!(error.contains("could not open"), "{error}");
        installation.require_current().unwrap();
        assert!(installation.pending().unwrap().is_some());
        assert!(!fixture.0.join(".install-lock").exists());
        assert_eq!(
            fs::read_to_string(fixture.0.join("handoffs")).unwrap(),
            "1.1.0 daemon restart\n1.0.0 daemon restart\n"
        );
    }

    #[test]
    fn concurrent_installer_or_changed_current_cannot_restart_the_daemon() {
        let fixture = Fixture::new();
        let (_, prepared) = fixture.prepare(true);
        fs::create_dir(fixture.0.join(".install-lock")).unwrap();
        assert!(prepared.restart().unwrap_err().contains("active"));
        fs::remove_dir(fixture.0.join(".install-lock")).unwrap();
        fs::remove_file(fixture.0.join("current")).unwrap();
        symlink(&prepared.release, fixture.0.join("current")).unwrap();
        assert!(
            prepared
                .restart()
                .unwrap_err()
                .contains("installation changed")
        );
        assert!(!fixture.0.join("handoffs").exists());
    }

    #[test]
    fn malformed_versions_external_targets_and_writable_bundles_are_rejected() {
        let fixture = Fixture::new();
        let (installation, prepared) = fixture.prepare(true);
        for version in ["1.0.0", "0.9.0", "1.2.0-beta.1", "1.2.0+build", "../../bad"] {
            assert!(installation.prepare(version).is_err());
        }
        fs::remove_file(fixture.0.join("pending")).unwrap();
        symlink("/tmp", fixture.0.join("pending")).unwrap();
        assert!(installation.pending().is_err());
        fs::set_permissions(
            prepared.release.join("bin/boomux"),
            fs::Permissions::from_mode(0o777),
        )
        .unwrap();
        assert!(installation.validate_release(&prepared.release).is_err());
        assert!(!fixture.0.join("handoffs").exists());
    }

    #[test]
    fn failed_daemon_handoff_keeps_the_current_bundle_and_pending_update() {
        let fixture = Fixture::new();
        let (installation, prepared) = fixture.prepare(true);
        fs::write(
            prepared.release.join("bin/boomux"),
            "#!/bin/sh\necho 'handoff refused' >&2\nexit 1\n",
        )
        .unwrap();
        let error = prepared.restart().unwrap_err();
        assert!(error.contains("handoff refused"), "{error}");
        installation.require_current().unwrap();
        assert!(installation.pending().unwrap().is_some());
        assert!(!fixture.0.join(".install-lock").exists());
    }

    #[test]
    fn manual_installation_only_requests_restart_for_a_different_daemon_executable() {
        let fixture = Fixture::new();
        let (installation, _) = fixture.prepare(true);
        fs::remove_file(fixture.0.join("pending")).unwrap();
        for (executable, expected) in [
            (installation.running.join("bin/boomux"), false),
            (fixture.0.join("previous/boomux"), true),
        ] {
            let status = serde_json::json!({
                "schema": "boomux.cli/v1", "command": "daemon.status",
                "data": {"status":"running", "executable":executable}
            });
            fs::write(
                installation.running.join("bin/boomux"),
                format!("#!/bin/sh\ncat <<'STATUS'\n{status}\nSTATUS\n"),
            )
            .unwrap();
            let result = installation.finish_installation().unwrap();
            assert_eq!(result.is_some(), expected);
            if let Some(prepared) = result {
                assert_eq!(prepared.release, installation.running);
                assert!(!prepared.requires_pending);
            }
            installation.require_current().unwrap();
            assert!(!fixture.0.join("pending").exists());
        }
        assert!(!fixture.0.join("handoffs").exists());
    }
}

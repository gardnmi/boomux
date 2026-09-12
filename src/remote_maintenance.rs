//! Temporary owner-side recovery for missing or noncanonical user installations.
//! The runner is uploaded privately and never creates identity or deletes durable state.
use crate::{
    client, config,
    integration_management::{self, AssetState, Environment, IntegrationId},
    update,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    env, fs,
    io::{self, Read, Write},
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    path::{Path, PathBuf},
};

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;
const MAX_BINARY: u64 = 256 * 1024 * 1024;

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Probe {
    version: u32,
    node_id: String,
    destination: PathBuf,
    installed: Option<String>,
    daemon_pid: Option<u32>,
    daemon_executable: Option<PathBuf>,
    preserved: Vec<String>,
}

fn private_directory(path: &Path) -> io::Result<()> {
    let m = fs::symlink_metadata(path)?;
    if !m.is_dir()
        || m.file_type().is_symlink()
        || m.uid() != unsafe { libc::geteuid() }
        || m.mode() & 0o022 != 0
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "maintenance directory is not privately owned",
        ));
    }
    Ok(())
}

fn binary_token(path: &Path) -> io::Result<Option<String>> {
    let mut f = match fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
    {
        Ok(f) => f,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    let m = f.metadata()?;
    if !m.is_file()
        || m.uid() != unsafe { libc::geteuid() }
        || m.mode() & 0o022 != 0
        || m.nlink() != 1
        || m.len() == 0
        || m.len() > MAX_BINARY
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "maintenance executable is not an owned regular file",
        ));
    }
    let mut hash = Sha256::new();
    let mut bytes = [0; 64 * 1024];
    let mut total = 0;
    loop {
        let n = f.read(&mut bytes)?;
        if n == 0 {
            break;
        }
        total += n as u64;
        if total > MAX_BINARY {
            return Err(io::Error::other("executable exceeds inspection limit"));
        }
        hash.update(&bytes[..n]);
    }
    let after = f.metadata()?;
    if total != m.len()
        || m.len() != after.len()
        || m.mtime() != after.mtime()
        || m.mtime_nsec() != after.mtime_nsec()
        || m.ctime() != after.ctime()
        || m.ctime_nsec() != after.ctime_nsec()
        || m.mode() != after.mode()
    {
        return Err(io::Error::other("executable changed during inspection"));
    }
    Ok(Some(format!(
        "{}:{}:{}:{:x}",
        m.dev(),
        m.ino(),
        m.len(),
        hash.finalize()
    )))
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Installation {
    version: u32,
    node_id: String,
    path: PathBuf,
}

fn receipt_path() -> Result<PathBuf> {
    let home = PathBuf::from(env::var_os("HOME").ok_or("HOME missing")?);
    let root = env::var_os("BOOMUX_STATE_HOME")
        .or_else(|| env::var_os("XDG_STATE_HOME"))
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".local/state"));
    Ok(root.join("boomux/installation.json"))
}

fn owned_destination(home: &Path, destination: &Path, create: bool) -> Result<()> {
    if update::is_package_path(destination) {
        return Err("package-managed executable; use its package manager".into());
    }
    let relative = destination
        .strip_prefix(home)
        .map_err(|_| "package-managed executable; use its package manager")?;
    if relative
        .components()
        .any(|c| !matches!(c, std::path::Component::Normal(_)))
        || relative.components().count() < 2
    {
        return Err("invalid user installation path".into());
    }
    if relative.starts_with(".local/share/boomux-desktop") || relative.starts_with("Applications") {
        return Err(
            "this executable belongs to a Desktop bundle; use its Desktop installer".into(),
        );
    }
    private_directory(home)?;
    let mut directory = home.to_path_buf();
    let parent = relative.parent().ok_or("installation parent missing")?;
    for part in parent.components() {
        directory.push(part);
        if !directory.try_exists()? {
            if create {
                use std::os::unix::fs::DirBuilderExt;
                fs::DirBuilder::new().mode(0o700).create(&directory)?;
            } else {
                continue;
            }
        }
        private_directory(&directory)?;
    }
    Ok(())
}

fn read_receipt(expected: &str) -> Result<Option<PathBuf>> {
    let path = receipt_path()?;
    let file = match fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
    {
        Ok(file) => file,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    let metadata = file.metadata()?;
    if !metadata.is_file()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.mode() & 0o077 != 0
        || metadata.len() > 8192
    {
        return Err("unsafe installation record".into());
    }
    let mut bytes = Vec::new();
    file.take(8193).read_to_end(&mut bytes)?;
    let receipt: Installation = serde_json::from_slice(&bytes)?;
    if receipt.version != 1 || receipt.node_id != expected {
        return Err("installation record belongs to another Node or version".into());
    }
    Ok(Some(receipt.path))
}

fn record_installation(probe: &Probe) -> Result<()> {
    let path = receipt_path()?;
    private_directory(path.parent().ok_or("receipt parent missing")?)?;
    let temporary = path.with_file_name(".installation.next");
    update::remove_recovery_temporary(&temporary)?;
    let result = (|| -> Result<()> {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temporary)?;
        file.write_all(&serde_json::to_vec(&Installation {
            version: 1,
            node_id: probe.node_id.clone(),
            path: probe.destination.clone(),
        })?)?;
        file.sync_all()?;
        fs::rename(&temporary, &path)?;
        fs::File::open(path.parent().unwrap())?.sync_all()?;
        Ok(())
    })();
    let _ = fs::remove_file(temporary);
    result
}

fn probe(expected: &str) -> Result<Probe> {
    let node_id = boomux::federation::existing_node_identity()?;
    if node_id != expected {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "remote Node identity changed; refusing maintenance",
        )
        .into());
    }
    let home = PathBuf::from(env::var_os("HOME").ok_or("HOME is missing")?);
    if !home.is_absolute() {
        return Err("HOME must be absolute".into());
    }
    let mut destination = home.join(".local/bin/boomux");
    let (daemon_pid, daemon_executable) = if let Some(daemon) = client::connect_if_running()? {
        if daemon.node_identity()? != node_id {
            return Err("running daemon belongs to another Node".into());
        }
        let executable = update::daemon_executable(&daemon)
            .ok_or("running daemon executable cannot be verified")?;
        destination = executable.path.clone();
        (Some(executable.pid), Some(executable.path))
    } else {
        if let Some(recorded) = read_receipt(expected)? {
            destination = recorded;
        }
        (None, None)
    };
    owned_destination(&home, &destination, false)?;
    let installed = binary_token(&destination)?;
    let environment = Environment::from_process();
    let preserved = IntegrationId::all()
        .into_iter()
        .filter_map(|id| {
            let status = integration_management::inspect(id, &environment, None);
            matches!(
                status.asset.state,
                AssetState::Modified | AssetState::Unavailable
            )
            .then(|| id.spec().display_name.to_string())
        })
        .collect();
    Ok(Probe {
        version: 1,
        node_id,
        destination,
        installed,
        daemon_pid,
        daemon_executable,
        preserved,
    })
}

fn token(probe: &Probe) -> Result<String> {
    Ok(format!("{:x}", Sha256::digest(serde_json::to_vec(probe)?)))
}

struct RecoveryLock {
    path: PathBuf,
    file: fs::File,
}
impl RecoveryLock {
    fn acquire(path: &Path) -> Result<Self> {
        let mut file = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(path)
            .map_err(|_| "another update is finishing recovery; retry shortly")?;
        use std::os::fd::AsRawFd;
        let m = file.metadata()?;
        if !m.is_file()
            || m.uid() != unsafe { libc::geteuid() }
            || m.mode() & 0o077 != 0
            || m.nlink() != 1
            || m.len() > 64
        {
            return Err("unsafe maintenance lock".into());
        }
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            return Err("another recovery operation is active; retry shortly".into());
        }
        let mut contents = String::new();
        Read::by_ref(&mut file)
            .take(65)
            .read_to_string(&mut contents)?;
        if !contents.is_empty() && contents != "boomux-recovery-v1\n" {
            return Err("unrecognized maintenance lock".into());
        }
        if contents.is_empty() {
            file.write_all(b"boomux-recovery-v1\n")?;
            file.sync_all()?;
        }
        let current = fs::symlink_metadata(path)?;
        if current.dev() != m.dev() || current.ino() != m.ino() {
            return Err("maintenance lock changed; retry".into());
        }
        Ok(Self {
            path: path.into(),
            file,
        })
    }
}
impl Drop for RecoveryLock {
    fn drop(&mut self) {
        if let (Ok(current), Ok(held)) = (fs::symlink_metadata(&self.path), self.file.metadata())
            && current.dev() == held.dev()
            && current.ino() == held.ino()
        {
            let _ = fs::remove_file(&self.path);
        }
    }
}

pub(crate) fn run(action: &str, expected: &str, authorization: Option<&str>) -> Result<()> {
    let initial = probe(expected)?;
    if action == "probe" {
        println!(
            "{}",
            serde_json::json!({"probe": initial, "token":token(&initial)?})
        );
        return Ok(());
    }
    if !matches!(action, "repair" | "remove") {
        return Err("unknown maintenance action".into());
    }
    if authorization != Some(token(&initial)?.as_str()) {
        return Err("installation changed after maintenance preparation; retry".into());
    }
    boomux::ssh_bootstrap::secure_runtime_directory(
        client::socket_path()?
            .parent()
            .ok_or("runtime parent missing")?,
    )?;
    // A regular file blocks legacy installers' mkdir lock too. Kernel flock
    // lets a new recovery runner reclaim this file after an interrupted run.
    let home = PathBuf::from(env::var_os("HOME").ok_or("HOME missing")?);
    let canonical = home.join(".local/bin/boomux");
    owned_destination(&home, &canonical, true)?;
    let _lock = RecoveryLock::acquire(&canonical.with_file_name(".boomux.bootstrap.lock"))?;
    let current = probe(expected)?;
    if token(&current)? != token(&initial)? {
        return Err("installation changed during maintenance preparation; retry".into());
    }
    if action == "repair" {
        let notifications = config::load_notification_settings().map_err(
            |_| "remote configuration could not be read; repair it on the owner before updating",
        )?;
        let _absence = if initial.daemon_pid.is_none() {
            Some(update::reserve_daemon_absence(&client::socket_path()?)?)
        } else {
            None
        };
        if initial.daemon_pid.is_some()
            && !client::connect()?.supports(boomux::protocol::ProtocolFeature::RestartExecutable)?
        {
            return Err("running daemon predates safe executable handoff; repair requires protocol 52 or newer".into());
        }
        if initial.daemon_pid.is_none() {
            boomux::federation::validate_recovery_state()
                .map_err(|_| "saved remote state cannot be read by this Boomux version; use a compatible release")?;
        }
        let home = PathBuf::from(env::var_os("HOME").ok_or("HOME missing")?);
        owned_destination(&home, &initial.destination, true)?;
        record_installation(&initial)?;
        let source = env::current_exe()?;
        binary_token(&source)?.ok_or("recovery runner disappeared")?;
        let mut input = fs::File::open(source)?;
        let temporary = initial.destination.with_file_name(".boomux-repair-install");
        update::remove_recovery_temporary(&temporary)?;
        let result = (|| -> Result<()> {
            let mut output = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o700)
                .open(&temporary)?;
            if io::copy(&mut (&mut input).take(MAX_BINARY + 1), &mut output)? > MAX_BINARY {
                return Err("executable exceeds inspection limit".into());
            }
            output.sync_all()?;
            drop(output);
            // Revalidate the target and never overwrite a concurrent first install.
            if binary_token(&initial.destination)? != initial.installed {
                return Err("installation changed before replacement".into());
            }
            if initial.installed.is_some() {
                update::replace_for_recovery(
                    &initial.destination,
                    &temporary,
                    expected,
                    notifications.clone().into(),
                    initial.daemon_pid.is_some(),
                )?;
            } else {
                update::rename_noreplace(&temporary, &initial.destination)?;
            }
            Ok(())
        })();
        let _ = fs::remove_file(&temporary);
        result?;
        fs::File::open(initial.destination.parent().unwrap())?.sync_all()?;
        if initial.installed.is_none() && initial.daemon_pid.is_some() {
            let daemon = client::connect()?;
            if daemon.node_identity()? != expected {
                return Err("Node identity changed before handoff".into());
            }
            daemon.restart_with_executable(initial.destination.clone(), notifications.into())?;
            update::verify_recovery_executable(&daemon, &initial.destination)?;
        }
        println!("Repaired Boomux at {}", initial.destination.display());
    } else {
        let environment = Environment::from_process();
        crate::uninstall::stop_web_gateways()?;
        if initial.daemon_pid.is_some() {
            client::connect()?.shutdown_if_node_identity(expected)?;
        }
        let _reservation = update::reserve_daemon_absence(&client::socket_path()?)?;
        for id in IntegrationId::all() {
            let status = integration_management::inspect(id, &environment, None);
            if status.asset.state == AssetState::Current
                && let Err(error) = integration_management::uninstall(id, &environment, false)
            {
                eprintln!("Preserved {} integration: {error}", id.spec().display_name);
            }
        }
        if binary_token(&initial.destination)? != initial.installed {
            return Err("executable changed during removal; retry".into());
        }
        if initial.installed.is_some() {
            fs::remove_file(&initial.destination)?;
        }
        if initial.destination.parent().unwrap().exists() {
            fs::File::open(initial.destination.parent().unwrap())?.sync_all()?;
        }
        let _ = fs::remove_file(receipt_path()?);
        for name in [".boomux-repair-install", ".boomux-recovery-backup"] {
            if update::remove_recovery_temporary(&initial.destination.with_file_name(name)).is_err()
            {
                eprintln!("Preserved an unrecognized recovery artifact");
            }
        }
        println!("Removed Boomux; saved Workspace data preserved");
    }
    Ok(())
}

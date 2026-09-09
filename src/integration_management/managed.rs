//! Versioned, per-target ownership receipts. No daemon state or environment is persisted.
use super::*;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::os::{fd::AsRawFd, unix::fs::MetadataExt};

#[derive(PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Receipt {
    schema: u8,
    release: [u64; 3],
    enabled: bool,
    fingerprint: Option<String>,
}

fn release() -> [u64; 3] {
    [
        env!("CARGO_PKG_VERSION_MAJOR"),
        env!("CARGO_PKG_VERSION_MINOR"),
        env!("CARGO_PKG_VERSION_PATCH"),
    ]
    .map(|part| part.parse().expect("Cargo release component"))
}

fn receipt_path(target: &InstallTarget) -> PathBuf {
    target.path.with_file_name(format!(
        ".{}.boomux-managed.json",
        target
            .path
            .file_name()
            .expect("asset filename")
            .to_string_lossy()
    ))
}

fn read(path: &Path) -> io::Result<Option<Vec<u8>>> {
    let file = match OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
    {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    let metadata = file.metadata()?;
    if !metadata.is_file()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.len() > MAX_CODEX_HOOKS_BYTES
    {
        return Err(io::Error::other(
            "integration file must be owned, regular, and at most 1 MiB",
        ));
    }
    let mut bytes = Vec::new();
    file.take(MAX_CODEX_HOOKS_BYTES + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_CODEX_HOOKS_BYTES {
        return Err(io::Error::other("integration file exceeds 1 MiB"));
    }
    Ok(Some(bytes))
}

fn load(target: &InstallTarget) -> io::Result<Option<Receipt>> {
    read(&receipt_path(target))?
        .map(|bytes| {
            let receipt: Receipt = serde_json::from_slice(&bytes)?;
            if receipt.schema != 1
                || receipt.fingerprint.as_ref().is_some_and(|hash| {
                    hash.len() != 64 || !hash.bytes().all(|b| b.is_ascii_hexdigit())
                })
            {
                return Err(io::Error::other(
                    "unsupported integration ownership receipt",
                ));
            }
            Ok(receipt)
        })
        .transpose()
}

fn save(target: &InstallTarget, receipt: &Receipt) -> io::Result<()> {
    let path = receipt_path(target);
    let baseline = read(&path)?;
    write_checked(
        &target.directory,
        &path,
        &serde_json::to_vec(receipt)?,
        baseline.as_deref(),
        0o600,
    )
}

// Shared by explicit install/uninstall and reconciliation. Never wait for another
// process while holding up startup; a later startup or explicit sync can retry.
fn lock(target: &InstallTarget) -> Result<fs::File, Box<dyn Error>> {
    ensure_safe_directory(&target.directory)?;
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(target.directory.join(".boomux-integration.lock"))?;
    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata.uid() != unsafe { libc::geteuid() } {
        return Err(io::Error::other("integration lock is not an owned regular file").into());
    }
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        return Err(io::Error::last_os_error().into());
    }
    Ok(file)
}

// Hash only Boomux's handlers in shared Codex hooks.json. Other integrations and
// unrelated settings can change without forfeiting ownership of our own entries.
fn fingerprint(id: IntegrationId, bytes: &[u8]) -> io::Result<String> {
    let owned_handlers;
    let content = if id == IntegrationId::CODEX {
        let document: Value = serde_json::from_slice(bytes)?;
        let mut owned = Map::new();
        if let Some(hooks) = document.get("hooks").and_then(Value::as_object) {
            for (event, groups) in hooks {
                let mut owned_groups = Vec::new();
                for group in groups.as_array().into_iter().flatten() {
                    let handlers: Vec<_> = group
                        .get("hooks")
                        .and_then(Value::as_array)
                        .into_iter()
                        .flatten()
                        .filter(|handler| {
                            handler.get("command").and_then(Value::as_str)
                                == Some(CODEX_HOOK_COMMAND)
                        })
                        .cloned()
                        .collect();
                    if !handlers.is_empty() {
                        let mut group = group.clone();
                        group["hooks"] = Value::Array(handlers);
                        owned_groups.push(group);
                    }
                }
                if !owned_groups.is_empty() {
                    owned.insert(event.clone(), Value::Array(owned_groups));
                }
            }
        }
        owned_handlers = serde_json::to_vec(&owned)?;
        owned_handlers.as_slice()
    } else {
        bytes
    };
    Ok(format!("{:x}", Sha256::digest(content)))
}

fn write_checked(
    directory: &Path,
    path: &Path,
    content: &[u8],
    baseline: Option<&[u8]>,
    mode: u32,
) -> io::Result<()> {
    let temporary = directory.join(format!(".boomux-managed-{}.tmp", Uuid::new_v4()));
    let result = (|| {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(mode)
            .open(&temporary)?;
        file.write_all(content)?;
        file.sync_all()?;
        if read(path)?.as_deref() != baseline {
            return Err(io::Error::other("integration changed during update"));
        }
        fs::rename(&temporary, path)?;
        fs::File::open(directory)?.sync_all()
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

fn record_installed(id: IntegrationId, target: &InstallTarget) -> io::Result<()> {
    let receipt = Receipt {
        schema: 1,
        release: release(),
        enabled: true,
        fingerprint: Some(fingerprint(id, id.installation().content.as_bytes())?),
    };
    if load(target)?.as_ref() == Some(&receipt) {
        return Ok(());
    }
    save(target, &receipt)
}

pub(super) fn install(
    id: IntegrationId,
    environment: &Environment,
    force: bool,
) -> Result<InstallResult, Box<dyn Error>> {
    let target = install_target(id, environment)?;
    let _lock = lock(&target)?;
    // Manual installation is an explicit opt-in, including after uninstall.
    let owned = is_owned(id, &target)?;
    let asset_changed = owned && !force && reconcile_locked(id, &target)?;
    let mut result = install_unmanaged(id, environment, force)?;
    if asset_changed {
        result.result = InstallOutcome::Replaced;
        result.restart_required = true;
    }
    record_installed(id, &target)?;
    Ok(result)
}

pub(super) fn uninstall(
    id: IntegrationId,
    environment: &Environment,
    force: bool,
) -> Result<UninstallResult, Box<dyn Error>> {
    let target = install_target(id, environment)?;
    let _lock = lock(&target)?;
    let force = force || is_owned(id, &target)?;
    preflight_uninstall(id, environment, force)?;
    // Persist opt-out before deleting, so crashes cannot cause reinstallation.
    save(
        &target,
        &Receipt {
            schema: 1,
            release: release(),
            enabled: false,
            fingerprint: None,
        },
    )?;
    uninstall_unmanaged(id, environment, force)
}

fn reconcile(id: IntegrationId, target: &InstallTarget) -> Result<(), Box<dyn Error>> {
    let _lock = lock(target)?;
    reconcile_locked(id, target).map(|_| ())
}

pub(super) fn is_owned(id: IntegrationId, target: &InstallTarget) -> io::Result<bool> {
    let Some(receipt) = load(target)?.filter(|receipt| receipt.enabled) else {
        return Ok(false);
    };
    let Some(bytes) = read(&target.path)? else {
        return Ok(false);
    };
    Ok(receipt.fingerprint.as_deref() == Some(fingerprint(id, &bytes)?.as_str()))
}

// Returns whether the asset changed, independently of ownership-receipt updates.
fn reconcile_locked(id: IntegrationId, target: &InstallTarget) -> Result<bool, Box<dyn Error>> {
    let receipt = load(target)?;
    if receipt
        .as_ref()
        .is_some_and(|receipt| !receipt.enabled || receipt.release > release())
    {
        return Ok(false);
    }
    let baseline = read(&target.path)?;
    let existing = if id == IntegrationId::CODEX {
        inspect_codex_hooks(&target.path)?
    } else {
        inspect_existing_asset(&target.path, id.installation().content)?
    };
    if existing == ExistingAsset::Current {
        let expected = fingerprint(id, id.installation().content.as_bytes())?;
        if receipt
            .as_ref()
            .and_then(|receipt| receipt.fingerprint.as_ref())
            != Some(&expected)
        {
            record_installed(id, target)?;
        }
        return Ok(false);
    }
    if let Some(receipt) = &receipt {
        if existing == ExistingAsset::Missing {
            // Respect removal outside the CLI as well.
            save(
                target,
                &Receipt {
                    schema: 1,
                    release: release(),
                    enabled: false,
                    fingerprint: None,
                },
            )?;
            return Ok(false);
        }
        if baseline
            .as_deref()
            .map(|bytes| fingerprint(id, bytes))
            .transpose()?
            != receipt.fingerprint
        {
            return Err(io::Error::other("customized integration preserved; use integration install --force only to replace it intentionally").into());
        }
    } else if existing == ExistingAsset::Modified {
        return Err(io::Error::other("unrecognized existing integration preserved").into());
    }
    if id == IntegrationId::CODEX {
        let mut document = match baseline.as_deref() {
            Some(bytes) => serde_json::from_slice(bytes)?,
            None => serde_json::from_str(CODEX_ASSET_FALLBACK)?,
        };
        replace_codex_handlers(&mut document)?;
        let mode = fs::metadata(&target.path)
            .map(|metadata| metadata.permissions().mode() & 0o777)
            .unwrap_or(0o600);
        write_checked(
            &target.directory,
            &target.path,
            codex_hooks_content(&document)?.as_bytes(),
            baseline.as_deref(),
            mode,
        )?;
    } else {
        write_checked(
            &target.directory,
            &target.path,
            id.installation().content.as_bytes(),
            baseline.as_deref(),
            0o600,
        )?;
    }
    record_installed(id, target)?;
    Ok(true)
}

/// Prepare bundled assets regardless of the daemon's PATH. Remote services may
/// not see tools installed through interactive shell configuration. One bounded
/// pass, with no host execution, daemon connection, or host restart.
pub(crate) fn synchronize(environment: &Environment) -> Vec<String> {
    let mut errors = Vec::new();
    for id in IntegrationId::all() {
        let result = (|| -> Result<(), Box<dyn Error>> {
            let target = install_target(id, environment)?;
            validate_existing_directory_chain(&target.directory)?;
            reconcile(id, &target)
        })();
        if let Err(error) = result {
            errors.push(format!("{}: {error}", id.spec().key));
        }
    }
    errors
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            let path = env::temp_dir().join(format!("boomux-managed-test-{}", Uuid::new_v4()));
            fs::create_dir(&path).unwrap();
            Self(path)
        }
        fn environment(&self) -> Environment {
            Environment::for_test(
                Some(self.0.clone().into_os_string()),
                None,
                None,
                None,
                Some(OsString::new()),
            )
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn previous_release(id: IntegrationId, target: &InstallTarget, bytes: &[u8]) {
        ensure_safe_directory(&target.directory).unwrap();
        fs::write(&target.path, bytes).unwrap();
        save(
            target,
            &Receipt {
                schema: 1,
                release: release(),
                enabled: true,
                fingerprint: Some(fingerprint(id, bytes).unwrap()),
            },
        )
        .unwrap();
    }

    #[test]
    fn managed_integration_upgrade_is_automatic_and_idempotent() {
        let fixture = Fixture::new();
        let id = IntegrationId::PI;
        let target = install_target(id, &fixture.environment()).unwrap();
        previous_release(id, &target, b"// previous bundled version\n");
        reconcile(id, &target).unwrap();
        assert_eq!(
            read(&target.path).unwrap().unwrap(),
            id.installation().content.as_bytes()
        );
        let receipt = read(&receipt_path(&target)).unwrap();
        reconcile(id, &target).unwrap();
        assert_eq!(read(&receipt_path(&target)).unwrap(), receipt);
        assert!(is_owned(id, &target).unwrap());

        // Explicit installation uses the same upgrade path but must report a
        // replacement/reload, not the subsequent no-op installation result.
        previous_release(id, &target, b"// previous bundled version\n");
        let upgraded = install(id, &fixture.environment(), false).unwrap();
        assert_eq!(upgraded.result, InstallOutcome::Replaced);
        assert!(upgraded.restart_required);
        let receipt_inode = fs::metadata(receipt_path(&target)).unwrap().ino();
        let unchanged = install(id, &fixture.environment(), false).unwrap();
        assert_eq!(unchanged.result, InstallOutcome::Unchanged);
        assert!(!unchanged.restart_required);
        assert_eq!(
            fs::metadata(receipt_path(&target)).unwrap().ino(),
            receipt_inode
        );
    }

    #[test]
    fn managed_integration_preserves_customizations_unknown_assets_and_future_receipts() {
        let fixture = Fixture::new();
        let id = IntegrationId::PI;
        let target = install_target(id, &fixture.environment()).unwrap();
        previous_release(id, &target, b"// previous bundled version\n");
        fs::write(&target.path, "// user customization").unwrap();
        assert!(reconcile(id, &target).is_err());
        assert_eq!(
            read(&target.path).unwrap().unwrap(),
            b"// user customization"
        );
        fs::remove_file(receipt_path(&target)).unwrap();
        assert!(reconcile(id, &target).is_err());
        let future = Receipt {
            schema: 2,
            release: release(),
            enabled: true,
            fingerprint: None,
        };
        save(&target, &future).unwrap();
        assert!(
            reconcile(id, &target)
                .unwrap_err()
                .to_string()
                .contains("unsupported integration ownership receipt")
        );
        assert_eq!(
            read(&target.path).unwrap().unwrap(),
            b"// user customization"
        );
    }

    #[test]
    fn managed_integration_does_not_downgrade_a_newer_release() {
        let fixture = Fixture::new();
        let id = IntegrationId::PI;
        let target = install_target(id, &fixture.environment()).unwrap();
        previous_release(id, &target, b"// newer bundle");
        let mut receipt = load(&target).unwrap().unwrap();
        receipt.release[0] += 1;
        save(&target, &receipt).unwrap();
        reconcile(id, &target).unwrap();
        assert_eq!(read(&target.path).unwrap().unwrap(), b"// newer bundle");
    }

    #[test]
    fn managed_integration_uninstall_sticks_and_manual_install_reenables_updates() {
        let fixture = Fixture::new();
        let environment = fixture.environment();
        let id = IntegrationId::PI;
        let target = install_target(id, &environment).unwrap();
        reconcile(id, &target).unwrap();
        uninstall(id, &environment, false).unwrap();
        reconcile(id, &target).unwrap();
        assert!(!target.path.exists());
        install(id, &environment, false).unwrap();
        assert!(is_owned(id, &target).unwrap());
        fs::remove_file(&target.path).unwrap();
        reconcile(id, &target).unwrap();
        assert!(!target.path.exists());
        assert!(!load(&target).unwrap().unwrap().enabled);
    }

    #[test]
    fn managed_integration_old_owned_assets_can_be_removed_without_force() {
        let fixture = Fixture::new();
        let environment = fixture.environment();
        let id = IntegrationId::PI;
        let target = install_target(id, &environment).unwrap();
        previous_release(id, &target, b"// previous bundle");
        preflight_uninstall(id, &environment, false).unwrap();
        uninstall(id, &environment, false).unwrap();
        assert!(!target.path.exists());
    }

    #[test]
    fn managed_codex_updates_only_owned_handlers() {
        let fixture = Fixture::new();
        let id = IntegrationId::CODEX;
        let target = install_target(id, &fixture.environment()).unwrap();
        let mut old: Value = serde_json::from_str(id.installation().content).unwrap();
        old["hooks"]["SessionStart"][0]["hooks"][0]["timeout"] = Value::from(42);
        previous_release(id, &target, &serde_json::to_vec(&old).unwrap());
        old["hooks"]["SessionStart"].as_array_mut().unwrap().push(
            serde_json::json!({"hooks":[{"type":"command","command":"other-tool", "timeout":7}]}),
        );
        old["description"] = Value::from("user description");
        fs::write(&target.path, serde_json::to_vec(&old).unwrap()).unwrap();
        reconcile(id, &target).unwrap();
        let current: Value = serde_json::from_slice(&read(&target.path).unwrap().unwrap()).unwrap();
        assert_eq!(current["description"], "user description");
        assert!(
            current["hooks"]["SessionStart"]
                .as_array()
                .unwrap()
                .iter()
                .any(|group| group["hooks"][0]["command"] == "other-tool")
        );
        assert_eq!(codex_hooks_state(&current).unwrap(), ExistingAsset::Current);
        assert!(is_owned(id, &target).unwrap());
        let mut edited = current;
        edited["hooks"]["SessionStart"]
            .as_array_mut()
            .unwrap()
            .iter_mut()
            .find(|group| group["hooks"][0]["command"] == CODEX_HOOK_COMMAND)
            .unwrap()["matcher"] = Value::from("user customization");
        fs::write(&target.path, serde_json::to_vec(&edited).unwrap()).unwrap();
        assert!(reconcile(id, &target).is_err());
    }

    #[test]
    fn managed_integration_sync_preinstalls_assets_without_hosts_on_path() {
        let fixture = Fixture::new();
        let environment = fixture.environment();
        assert!(synchronize(&environment).is_empty());
        for id in IntegrationId::all() {
            let target = install_target(id, &environment).unwrap();
            assert!(is_owned(id, &target).unwrap(), "{}", id.spec().key);
            assert_eq!(
                inspect_without_host_probe(id, &environment, None)
                    .asset
                    .state,
                AssetState::Current
            );
        }
        // Repeated startup must not overwrite assets or ownership receipts.
        let id = IntegrationId::OPENCODE;
        let target = install_target(id, &environment).unwrap();
        let asset_inode = fs::metadata(&target.path).unwrap().ino();
        let receipt_inode = fs::metadata(receipt_path(&target)).unwrap().ino();
        assert!(synchronize(&environment).is_empty());
        assert_eq!(fs::metadata(&target.path).unwrap().ino(), asset_inode);
        assert_eq!(
            fs::metadata(receipt_path(&target)).unwrap().ino(),
            receipt_inode
        );
    }

    #[test]
    fn managed_integration_sync_preserves_opt_outs_and_customizations_without_hosts() {
        let fixture = Fixture::new();
        let environment = fixture.environment();
        assert!(synchronize(&environment).is_empty());
        let opted_out = IntegrationId::OPENCODE;
        uninstall(opted_out, &environment, false).unwrap();
        let customized = install_target(IntegrationId::PI, &environment).unwrap();
        fs::write(&customized.path, "// user customization").unwrap();
        let errors = synchronize(&environment);
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert!(errors[0].contains("customized integration preserved"));
        assert!(
            !install_target(opted_out, &environment)
                .unwrap()
                .path
                .exists()
        );
        assert_eq!(
            fs::read(&customized.path).unwrap(),
            b"// user customization"
        );
    }

    #[test]
    fn managed_integration_rejects_symlinks_and_concurrent_writers() {
        let fixture = Fixture::new();
        let id = IntegrationId::PI;
        let target = install_target(id, &fixture.environment()).unwrap();
        let guard = lock(&target).unwrap();
        assert!(reconcile(id, &target).is_err());
        drop(guard);
        let outside = fixture.0.join("other");
        fs::write(&outside, "untouched").unwrap();
        std::os::unix::fs::symlink(&outside, &target.path).unwrap();
        assert!(reconcile(id, &target).is_err());
        assert_eq!(fs::read_to_string(outside).unwrap(), "untouched");
    }
}

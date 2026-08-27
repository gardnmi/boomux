use std::env;
use std::fs;
use std::io::{self, IsTerminal, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Component, Path, PathBuf};

use crate::integration_management::{self, AssetState};
use crate::{BOOMUX_SKILL, client, mobile_web, setup, tailscale_serve, update};

const MAX_PURGE_ENTRIES: usize = 16_384;
const MAX_WEB_GATEWAYS: usize = 256;

pub(crate) fn guided_uninstall(purge: bool) -> Result<(), Box<dyn std::error::Error>> {
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Boomux uninstall requires an interactive terminal",
        )
        .into());
    }

    let target = update::uninstall_target()?;
    let environment = integration_management::Environment::from_process();
    let mut removable_integrations = Vec::new();
    let mut preserved_integrations = Vec::new();
    for integration in integration_management::IntegrationId::all() {
        let status = integration_management::inspect(integration, &environment, None);
        match status.asset.state {
            AssetState::Current => removable_integrations.push(integration),
            AssetState::Modified | AssetState::Unavailable => {
                preserved_integrations.push((integration, status.asset.path))
            }
            AssetState::Missing => {}
        }
    }
    for integration in &removable_integrations {
        integration_management::preflight_uninstall(*integration, &environment, false)?;
    }

    let home = required_home()?;
    let skill_path = home.join(".agents/skills/boomux/SKILL.md");
    let skill_state = integration_management::regular_file_matches(&skill_path, BOOMUX_SKILL)?;
    let (bindings_path, bindings_state, bindings_error) = match setup::managed_bindings_status() {
        Ok((path, state)) => (path, state, None),
        Err(error) => (
            home.join(".config/hypr/bindings.lua"),
            Some(false),
            Some(error.to_string()),
        ),
    };
    let purge_directories = purge
        .then(|| Ok::<_, io::Error>((state_directory(&home)?, config_directory(&home)?)))
        .transpose()?;
    if let Some((state_directory, config_directory)) = &purge_directories {
        validate_owned_tree_if_present(state_directory)?;
        validate_owned_tree_if_present(config_directory)?;
    }

    println!("Install path: {}", target.path().display());
    println!(
        "Process impact: every Boomux web gateway, managed Shell process, PTY, and shared runtime on this Node will be stopped"
    );
    println!(
        "Owned assets: current Boomux integration files and the unchanged Boomux Agent Skill will be removed"
    );
    for (integration, path) in &preserved_integrations {
        println!(
            "Preserving modified or uninspectable {} integration asset{}",
            integration.spec().display_name,
            path.as_deref()
                .map(|path| format!(" at {path}"))
                .unwrap_or_default()
        );
    }
    if skill_state == Some(false) {
        println!(
            "Preserving modified Boomux Agent Skill at {}",
            skill_path.display()
        );
    }
    match bindings_state {
        Some(true) => println!(
            "Owned desktop assets: Boomux managed keybindings at {} will be removed",
            bindings_path.display()
        ),
        Some(false) => println!(
            "Preserving modified or uninspectable Boomux managed keybindings at {}{}",
            bindings_path.display(),
            bindings_error
                .as_deref()
                .map(|error| format!(": {error}"))
                .unwrap_or_default()
        ),
        None => {}
    }
    println!(
        "The Omarchy plugin remains independently managed; remove it with: omarchy plugin remove io.github.gardnmi.boomux"
    );
    if purge {
        let (state_directory, config_directory) = purge_directories
            .as_ref()
            .expect("purge directories were resolved");
        println!(
            "Data impact: --purge removes {} and {}",
            state_directory.display(),
            config_directory.display()
        );
        println!(
            "The optional BOOMUX_CONFIG file is not removed when it is outside the standard Boomux config directory"
        );
    } else {
        println!(
            "Data impact: durable state and configuration are preserved for a later reinstall"
        );
    }
    print!("Uninstall Boomux? [y/N] ");
    io::stdout().flush()?;
    let mut answer = String::new();
    io::stdin().read_line(&mut answer)?;
    if !matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes") {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "uninstall was not authorized",
        )
        .into());
    }

    update::revalidate_uninstall_target(&target)?;
    for integration in &removable_integrations {
        integration_management::preflight_uninstall(*integration, &environment, false)?;
    }
    if let Some((state_directory, config_directory)) = &purge_directories {
        validate_owned_tree_if_present(state_directory)?;
        validate_owned_tree_if_present(config_directory)?;
    }

    stop_web_gateways()?;
    let _daemon_reservation = update::stop_daemon_for_uninstall(&target)?;
    for integration in removable_integrations {
        let result = integration_management::uninstall(integration, &environment, false)?;
        if result.result == integration_management::UninstallOutcome::Removed {
            println!(
                "Removed {} integration asset from {}",
                integration.spec().display_name,
                result.path
            );
        }
    }
    if skill_state == Some(true)
        && integration_management::regular_file_matches(&skill_path, BOOMUX_SKILL)? == Some(true)
    {
        fs::remove_file(&skill_path)?;
        println!("Removed Boomux Agent Skill from {}", skill_path.display());
    } else if skill_state == Some(true) {
        println!(
            "Preserving Boomux Agent Skill because it changed after authorization: {}",
            skill_path.display()
        );
    }
    if bindings_state == Some(true) {
        match setup::managed_bindings_status() {
            Ok((_, Some(true))) => match setup::remove_managed_bindings() {
                Ok(true) => println!(
                    "Removed Boomux managed keybindings from {}",
                    bindings_path.display()
                ),
                _ => println!(
                    "Preserving Boomux managed keybindings because they changed after authorization: {}",
                    bindings_path.display()
                ),
            },
            _ => println!(
                "Preserving Boomux managed keybindings because they changed after authorization: {}",
                bindings_path.display()
            ),
        }
    }
    if purge {
        let (state_directory, config_directory) = purge_directories
            .as_ref()
            .expect("purge directories were resolved");
        validate_owned_tree_if_present(state_directory)?;
        validate_owned_tree_if_present(config_directory)?;
        remove_owned_tree_if_present(&home, state_directory)?;
        remove_owned_tree_if_present(&home, config_directory)?;
    }
    update::remove_uninstall_target(&target)?;
    println!("Uninstalled Boomux from {}", target.path().display());
    Ok(())
}

pub(crate) fn remote_uninstall(
    expected_node_id: &str,
    expected_executable: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    uuid::Uuid::parse_str(expected_node_id)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "expected Node ID is invalid"))?;
    let target = update::uninstall_target()?;
    if target.authorization_token() != expected_executable {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "installed Boomux executable changed after remote uninstall authorization",
        )
        .into());
    }
    stop_web_gateways()?;
    update::revalidate_uninstall_target(&target)?;
    let _daemon_reservation = update::stop_daemon_for_remote_uninstall(&target, expected_node_id)?;
    update::remove_uninstall_target(&target)?;
    println!("Uninstalled Boomux from {}", target.path().display());
    Ok(())
}

fn stop_web_gateways() -> Result<(), Box<dyn std::error::Error>> {
    let runtime = client::socket_path()?
        .parent()
        .ok_or_else(|| io::Error::other("Boomux runtime socket has no parent"))?
        .to_path_buf();
    let entries = match fs::read_dir(&runtime) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    let mut ports = Vec::new();
    let mut entry_count = 0;
    for entry in entries {
        entry_count += 1;
        if entry_count > MAX_WEB_GATEWAYS {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Boomux runtime directory exceeds the uninstall entry bound",
            )
            .into());
        }
        let entry = entry?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        let Some(port) = name
            .strip_prefix("web-")
            .and_then(|name| name.strip_suffix(".sock"))
        else {
            continue;
        };
        if let Ok(port) = port.parse::<u16>() {
            ports.push(port);
        }
    }
    ports.sort_unstable();
    ports.dedup();
    for port in ports {
        mobile_web::stop(port)?;
        tailscale_serve::cleanup_stale(port)?;
    }
    Ok(())
}

fn required_home() -> io::Result<PathBuf> {
    env::var_os("HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "HOME must be an absolute path"))
}

fn state_directory(home: &Path) -> io::Result<PathBuf> {
    xdg_directory("XDG_STATE_HOME", home.join(".local/state"), home).map(|root| root.join("boomux"))
}

fn config_directory(home: &Path) -> io::Result<PathBuf> {
    xdg_directory("XDG_CONFIG_HOME", home.join(".config"), home).map(|root| root.join("boomux"))
}

fn xdg_directory(variable: &str, fallback: PathBuf, home: &Path) -> io::Result<PathBuf> {
    let root = env::var_os(variable)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .unwrap_or(fallback);
    if !root.is_absolute()
        || root
            .components()
            .any(|component| matches!(component, Component::CurDir | Component::ParentDir))
        || !root.starts_with(home)
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("{variable} must be an absolute path beneath HOME for purge"),
        ));
    }
    validate_owned_directory_chain(home, &root)?;
    Ok(root)
}

fn validate_owned_directory_chain(home: &Path, target: &Path) -> io::Result<()> {
    let uid = unsafe { libc::geteuid() };
    let relative = target.strip_prefix(home).map_err(|_| {
        io::Error::new(
            io::ErrorKind::PermissionDenied,
            "purge directory is outside HOME",
        )
    })?;
    let mut current = home.to_path_buf();
    for component in std::iter::once(Component::RootDir).chain(relative.components()) {
        if component == Component::RootDir {
            current = home.to_path_buf();
        } else if let Component::Normal(component) = component {
            current.push(component);
        } else {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "purge directory contains an unsafe path component",
            ));
        }
        match fs::symlink_metadata(&current) {
            Ok(metadata)
                if metadata.is_dir()
                    && !metadata.file_type().is_symlink()
                    && metadata.uid() == uid => {}
            Ok(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    format!("unsafe purge directory chain at {}", current.display()),
                ));
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => break,
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

fn remove_owned_tree_if_present(home: &Path, path: &Path) -> io::Result<()> {
    let Some((parent, name, directory)) = open_owned_tree(home, path)? else {
        return Ok(());
    };
    let staged = format!(".boomux.uninstall.{}", uuid::Uuid::new_v4());
    renameat_noreplace(parent.as_raw_fd(), &name, &staged)?;
    let mut count = 0;
    remove_directory_contents(&directory, &mut count)?;
    unlinkat(parent.as_raw_fd(), &staged, libc::AT_REMOVEDIR)
}

fn open_owned_tree(home: &Path, path: &Path) -> io::Result<Option<(OwnedFd, String, OwnedFd)>> {
    let relative = path.strip_prefix(home).map_err(|_| {
        io::Error::new(
            io::ErrorKind::PermissionDenied,
            "purge path is outside HOME",
        )
    })?;
    let mut components = relative.components().peekable();
    let home_fd = open_directory(home)?;
    validate_directory_fd(&home_fd)?;
    let mut parent = home_fd;
    let mut name = None;
    while let Some(component) = components.next() {
        let Component::Normal(component) = component else {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "purge path contains an unsafe component",
            ));
        };
        let component = component.to_str().ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "purge path is not UTF-8")
        })?;
        if components.peek().is_none() {
            name = Some(component.to_owned());
            break;
        }
        parent = match openat_directory(parent.as_raw_fd(), component) {
            Ok(directory) => directory,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error),
        };
        validate_directory_fd(&parent)?;
    }
    let name = name
        .ok_or_else(|| io::Error::new(io::ErrorKind::PermissionDenied, "refusing to purge HOME"))?;
    let directory = match openat_directory(parent.as_raw_fd(), &name) {
        Ok(directory) => directory,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    let mut count = 0;
    validate_owned_directory_contents(&directory, &mut count)?;
    Ok(Some((parent, name, directory)))
}

fn open_directory(path: &Path) -> io::Result<OwnedFd> {
    let path = std::ffi::CString::new(path.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path contains a NUL byte"))?;
    let fd = unsafe {
        libc::open(
            path.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(unsafe { OwnedFd::from_raw_fd(fd) })
    }
}

fn openat_directory(parent: i32, name: &str) -> io::Result<OwnedFd> {
    let name = std::ffi::CString::new(name)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path contains a NUL byte"))?;
    let fd = unsafe {
        libc::openat(
            parent,
            name.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(unsafe { OwnedFd::from_raw_fd(fd) })
    }
}

fn validate_directory_fd(directory: &OwnedFd) -> io::Result<()> {
    let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
    if unsafe { libc::fstat(directory.as_raw_fd(), stat.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let stat = unsafe { stat.assume_init() };
    if stat.st_uid != unsafe { libc::geteuid() }
        || stat.st_mode & libc::S_IFMT != libc::S_IFDIR
        || stat.st_mode & 0o022 != 0
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "purge directory is not owner-controlled",
        ));
    }
    Ok(())
}

fn validate_owned_directory_contents(directory: &OwnedFd, count: &mut usize) -> io::Result<()> {
    validate_directory_fd(directory)?;
    for entry in fs::read_dir(format!("/proc/self/fd/{}", directory.as_raw_fd()))? {
        *count += 1;
        if *count > MAX_PURGE_ENTRIES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Boomux data tree exceeds the purge entry bound",
            ));
        }
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_str().ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "purge entry is not UTF-8")
        })?;
        let metadata = fs::symlink_metadata(entry.path())?;
        if metadata.uid() != unsafe { libc::geteuid() }
            || metadata.file_type().is_symlink()
            || (!metadata.is_dir() && !metadata.is_file())
        {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "purge tree contains an unsafe entry",
            ));
        }
        if metadata.is_dir() {
            let child = openat_directory(directory.as_raw_fd(), name)?;
            validate_owned_directory_contents(&child, count)?;
        }
    }
    Ok(())
}

fn remove_directory_contents(directory: &OwnedFd, count: &mut usize) -> io::Result<()> {
    validate_directory_fd(directory)?;
    for entry in fs::read_dir(format!("/proc/self/fd/{}", directory.as_raw_fd()))? {
        *count += 1;
        if *count > MAX_PURGE_ENTRIES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Boomux data tree exceeded the purge entry bound during removal",
            ));
        }
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_str().ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "purge entry is not UTF-8")
        })?;
        let metadata = fs::symlink_metadata(entry.path())?;
        if metadata.uid() != unsafe { libc::geteuid() } {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "purge tree changed to an entry owned by another user",
            ));
        }
        if metadata.is_dir() && !metadata.file_type().is_symlink() {
            let child = openat_directory(directory.as_raw_fd(), name)?;
            remove_directory_contents(&child, count)?;
            unlinkat(directory.as_raw_fd(), name, libc::AT_REMOVEDIR)?;
        } else if metadata.is_file() && !metadata.file_type().is_symlink() {
            unlinkat(directory.as_raw_fd(), name, 0)?;
        } else {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "purge tree changed to an unsafe entry",
            ));
        }
    }
    Ok(())
}

fn renameat_noreplace(parent: i32, from: &str, to: &str) -> io::Result<()> {
    let from = std::ffi::CString::new(from).unwrap();
    let to = std::ffi::CString::new(to).unwrap();
    if unsafe {
        libc::renameat2(
            parent,
            from.as_ptr(),
            parent,
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

fn unlinkat(parent: i32, name: &str, flags: i32) -> io::Result<()> {
    let name = std::ffi::CString::new(name).unwrap();
    if unsafe { libc::unlinkat(parent, name.as_ptr(), flags) } == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

fn validate_owned_tree_if_present(path: &Path) -> io::Result<()> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    if !metadata.is_dir()
        || metadata.file_type().is_symlink()
        || metadata.uid() != unsafe { libc::geteuid() }
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("refusing to purge unsafe directory {}", path.display()),
        ));
    }
    let mut count = 0;
    validate_owned_tree(path, unsafe { libc::geteuid() }, &mut count)?;
    Ok(())
}

fn validate_owned_tree(path: &Path, uid: u32, count: &mut usize) -> io::Result<()> {
    for entry in fs::read_dir(path)? {
        *count += 1;
        if *count > MAX_PURGE_ENTRIES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Boomux data tree exceeds the purge entry bound",
            ));
        }
        let entry = entry?;
        let metadata = fs::symlink_metadata(entry.path())?;
        if metadata.uid() != uid
            || metadata.file_type().is_symlink()
            || (!metadata.is_dir() && !metadata.is_file())
        {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!("refusing to purge unsafe entry {}", entry.path().display()),
            ));
        }
        if metadata.is_dir() {
            validate_owned_tree(&entry.path(), uid, count)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn purge_validation_rejects_symlinks_before_removing_the_tree() {
        let root = env::temp_dir().join(format!("boomux-uninstall-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(root.join("nested")).unwrap();
        fs::write(root.join("state.json"), b"state").unwrap();
        std::os::unix::fs::symlink("state.json", root.join("nested/link")).unwrap();

        assert_eq!(
            validate_owned_tree_if_present(&root).unwrap_err().kind(),
            io::ErrorKind::PermissionDenied
        );
        assert!(root.join("state.json").exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn purge_removes_only_a_valid_owner_tree() {
        let home = env::temp_dir().join(format!("boomux-uninstall-{}", uuid::Uuid::new_v4()));
        let root = home.join("state/boomux");
        fs::create_dir_all(root.join("nested")).unwrap();
        fs::write(root.join("nested/state.json"), b"state").unwrap();

        remove_owned_tree_if_present(&home, &root).unwrap();
        assert!(!root.exists());
        remove_owned_tree_if_present(&home, &root).unwrap();
        fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn purge_directory_chain_rejects_symlinked_ancestors() {
        let home = env::temp_dir().join(format!("boomux-uninstall-{}", uuid::Uuid::new_v4()));
        let outside = env::temp_dir().join(format!("boomux-outside-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&home).unwrap();
        fs::create_dir_all(&outside).unwrap();
        std::os::unix::fs::symlink(&outside, home.join("state-link")).unwrap();

        assert_eq!(
            validate_owned_directory_chain(&home, &home.join("state-link"))
                .unwrap_err()
                .kind(),
            io::ErrorKind::PermissionDenied
        );
        fs::remove_dir_all(home).unwrap();
        fs::remove_dir_all(outside).unwrap();
    }
}

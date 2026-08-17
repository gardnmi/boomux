use std::env;
#[cfg(test)]
use std::ffi::{OsStr, OsString};
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;

use uuid::Uuid;

const MAX_SSH_TARGET_BYTES: usize = 1024;
const MAX_REMOTE_EXECUTABLE_BYTES: usize = 4096;
const MAX_CONTROL_PATH_BYTES: usize = 100;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SshAuthenticationMode {
    Interactive,
    Batch,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SshTarget(String);

impl SshTarget {
    pub fn parse(target: impl Into<String>) -> io::Result<Self> {
        let target = target.into();
        if target.is_empty()
            || target.len() > MAX_SSH_TARGET_BYTES
            || target.starts_with('-')
            || target.chars().any(char::is_whitespace)
            || target.chars().any(char::is_control)
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "SSH target must be a bounded non-option argument without whitespace",
            ));
        }
        Ok(Self(target))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteExecutable(String);

impl RemoteExecutable {
    pub fn parse(path: impl Into<String>) -> io::Result<Self> {
        let path = path.into();
        if path.is_empty()
            || path.len() > MAX_REMOTE_EXECUTABLE_BYTES
            || !Path::new(&path).is_absolute()
            || path.chars().any(char::is_control)
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "remote Boomux executable must be a bounded absolute path",
            ));
        }
        Ok(Self(path))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

pub struct SshInvocation {
    directory: PathBuf,
    config_path: PathBuf,
    control_path: PathBuf,
    target: SshTarget,
    remote_command: String,
    authentication: SshAuthenticationMode,
}

impl SshInvocation {
    pub fn prepare(
        target: SshTarget,
        executable: RemoteExecutable,
        authentication: SshAuthenticationMode,
    ) -> io::Result<Self> {
        let socket_path = crate::client::socket_path()?;
        let runtime_directory = socket_path
            .parent()
            .ok_or_else(|| io::Error::other("Boomux runtime socket has no parent"))?;
        let user_config = env::var_os("HOME")
            .filter(|home| !home.is_empty())
            .map(PathBuf::from)
            .map(|home| home.join(".ssh/config"));
        Self::prepare_at(
            runtime_directory,
            user_config.as_deref(),
            target,
            executable,
            authentication,
        )
    }

    pub fn command(&self) -> Command {
        let mut command = Command::new("ssh");
        command
            .arg("-F")
            .arg(&self.config_path)
            .arg("-T")
            .args(["-o", "ClearAllForwardings=yes"])
            .args(["-o", "ForwardAgent=no"])
            .args(["-o", "ForwardX11=no"])
            .args(["-o", "PermitLocalCommand=no"])
            .args(["-o", "RemoteCommand=none"])
            .args(["-o", "ForkAfterAuthentication=no"])
            .args(["-o", "StdinNull=no"])
            .args(["-o", "SessionType=default"])
            .args(["-o", "ControlMaster=auto"])
            .args(["-o", "ControlPersist=60"])
            .arg("-o")
            .arg(format!(
                "ControlPath={}",
                self.control_path
                    .to_str()
                    .expect("validated SSH control path")
            ))
            .args([
                "-o",
                match self.authentication {
                    SshAuthenticationMode::Interactive => "BatchMode=no",
                    SshAuthenticationMode::Batch => "BatchMode=yes",
                },
            ])
            .arg(self.target.as_str())
            .arg(&self.remote_command);
        command
    }

    pub fn config_path(&self) -> &Path {
        &self.config_path
    }

    pub fn control_path(&self) -> &Path {
        &self.control_path
    }

    fn prepare_at(
        runtime_directory: &Path,
        user_config: Option<&Path>,
        target: SshTarget,
        executable: RemoteExecutable,
        authentication: SshAuthenticationMode,
    ) -> io::Result<Self> {
        secure_runtime_directory(runtime_directory)?;
        let directory = runtime_directory.join(format!("ssh-{}", Uuid::new_v4()));
        fs::create_dir(&directory)?;
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o700))?;
        let config_path = directory.join("config");
        let control_path = directory.join("c");
        if control_path.as_os_str().as_bytes().len() > MAX_CONTROL_PATH_BYTES {
            let _ = fs::remove_dir_all(&directory);
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "SSH control socket path exceeds the safe Unix socket bound",
            ));
        }
        validate_option_path(&control_path, "SSH control socket")?;
        let result = (|| {
            let mut config = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&config_path)?;
            if let Some(user_config) = user_config {
                writeln!(config, "Include {}", quote_ssh_config_path(user_config)?)?;
            }
            // SendEnv is list-valued, so clear user entries after their config.
            writeln!(config, "SendEnv -*")?;
            writeln!(config, "Host *")?;
            writeln!(config, "    ServerAliveInterval 15")?;
            writeln!(config, "    ServerAliveCountMax 3")?;
            config.sync_all()?;
            Ok(())
        })();
        if let Err(error) = result {
            let _ = fs::remove_dir_all(&directory);
            return Err(error);
        }
        Ok(Self {
            directory,
            config_path,
            control_path,
            target,
            remote_command: format!(
                "{} __federation-stdio",
                quote_posix_shell(executable.as_str())
            ),
            authentication,
        })
    }
}

impl Drop for SshInvocation {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.directory);
    }
}

fn secure_runtime_directory(path: &Path) -> io::Result<()> {
    fs::create_dir_all(path)?;
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.is_dir() || metadata.uid() != effective_uid() {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "Boomux runtime path is not an owned directory",
        ));
    }
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
}

fn quote_posix_shell(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn quote_ssh_config_path(path: &Path) -> io::Result<String> {
    let value = path.to_str().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "SSH configuration path must be valid UTF-8",
        )
    })?;
    if value.chars().any(char::is_control) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "SSH configuration path contains control characters",
        ));
    }
    Ok(format!(
        "\"{}\"",
        value.replace('\\', "\\\\").replace('"', "\\\"")
    ))
}

fn validate_option_path(path: &Path, label: &str) -> io::Result<()> {
    let value = path.to_str().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{label} path must be valid UTF-8"),
        )
    })?;
    if value.chars().any(char::is_control)
        || value.chars().any(char::is_whitespace)
        || value.contains('%')
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{label} path contains unsafe option characters"),
        ));
    }
    Ok(())
}

fn effective_uid() -> u32 {
    // `geteuid` has no arguments, pointers, or caller safety requirements.
    unsafe { libc::geteuid() }
}

#[cfg(test)]
fn command_arguments(command: &Command) -> Vec<OsString> {
    command.get_args().map(OsStr::to_os_string).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn runtime_directory() -> PathBuf {
        env::temp_dir().join(format!("boomux-ssh-{}", Uuid::new_v4()))
    }

    #[test]
    fn rejects_option_like_and_unbounded_targets() {
        for target in ["", "-oProxyCommand=bad", "host name", "host\nname"] {
            assert_eq!(
                SshTarget::parse(target).unwrap_err().kind(),
                io::ErrorKind::InvalidInput
            );
        }
        assert_eq!(
            SshTarget::parse("x".repeat(MAX_SSH_TARGET_BYTES + 1))
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidInput
        );
        assert_eq!(
            SshTarget::parse("user@workbox").unwrap().as_str(),
            "user@workbox"
        );

        for executable in ["boomux", "relative/boomux", "/opt/boomux\nbin"] {
            assert_eq!(
                RemoteExecutable::parse(executable).unwrap_err().kind(),
                io::ErrorKind::InvalidInput
            );
        }
    }

    #[test]
    fn builds_exact_fixed_ssh_arguments_and_quotes_only_the_executable() {
        let runtime = runtime_directory();
        let invocation = SshInvocation::prepare_at(
            &runtime,
            Some(Path::new("/home/person/.ssh/config")),
            SshTarget::parse("workbox").unwrap(),
            RemoteExecutable::parse("/opt/boomux's bin/boomux").unwrap(),
            SshAuthenticationMode::Interactive,
        )
        .unwrap();
        let command = invocation.command();
        assert_eq!(command.get_program(), "ssh");
        let arguments = command_arguments(&command);
        assert_eq!(arguments[0], "-F");
        assert_eq!(arguments[1], invocation.config_path().as_os_str());
        assert_eq!(arguments[2], "-T");
        assert!(
            arguments
                .windows(2)
                .any(|pair| pair == ["-o", "ClearAllForwardings=yes"])
        );
        assert!(
            arguments
                .windows(2)
                .any(|pair| pair == ["-o", "BatchMode=no"])
        );
        assert_eq!(arguments[arguments.len() - 2], "workbox");
        assert_eq!(
            arguments.last().unwrap(),
            "'/opt/boomux'\\''s bin/boomux' __federation-stdio"
        );

        let config = fs::read_to_string(invocation.config_path()).unwrap();
        assert!(config.starts_with("Include \"/home/person/.ssh/config\"\nSendEnv -*\nHost *\n"));
        assert!(config.contains("ServerAliveInterval 15"));
        assert_eq!(
            fs::symlink_metadata(invocation.config_path())
                .unwrap()
                .mode()
                & 0o777,
            0o600
        );
        let directory = invocation.directory.clone();
        drop(invocation);
        assert!(!directory.exists());
        fs::remove_dir_all(runtime).unwrap();
    }

    #[test]
    fn batch_mode_changes_only_the_authentication_policy() {
        let runtime = runtime_directory();
        let invocation = SshInvocation::prepare_at(
            &runtime,
            None,
            SshTarget::parse("workbox").unwrap(),
            RemoteExecutable::parse("/usr/bin/boomux").unwrap(),
            SshAuthenticationMode::Batch,
        )
        .unwrap();
        let arguments = command_arguments(&invocation.command());
        assert!(
            arguments
                .windows(2)
                .any(|pair| pair == ["-o", "BatchMode=yes"])
        );
        drop(invocation);
        fs::remove_dir_all(runtime).unwrap();
    }
}

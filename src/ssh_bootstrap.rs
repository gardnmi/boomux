use std::collections::HashSet;
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
const MAX_PROBE_OUTPUT_BYTES: usize = 64 * 1024;
const MAX_DISCOVERED_EXECUTABLES: usize = 32;
const PLATFORM_PROBE_PREFIX: &[u8] = b"boomux-platform-v1";
const EXECUTABLE_PROBE_PREFIX: &[u8] = b"boomux-executables-v1";

pub const PLATFORM_PROBE_COMMAND: &str = "os=$(uname -s) || exit; arch=$(uname -m) || exit; printf 'boomux-platform-v1\\0%s\\0%s\\0' \"$os\" \"$arch\"";
pub const EXECUTABLE_PROBE_COMMAND: &str = "printf 'boomux-executables-v1\\0'; path=$(command -v boomux 2>/dev/null || true); for candidate in \"$path\" /usr/local/bin/boomux /usr/bin/boomux /opt/homebrew/bin/boomux /home/linuxbrew/.linuxbrew/bin/boomux \"$HOME/.local/bin/boomux\" \"$HOME/.local/share/mise/shims/boomux\" \"$HOME/.nix-profile/bin/boomux\" /run/current-system/sw/bin/boomux; do case \"$candidate\" in /*) [ -f \"$candidate\" ] && [ -x \"$candidate\" ] && printf '%s\\0' \"$candidate\" ;; esac; done";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SshAuthenticationMode {
    Interactive,
    Batch,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemoteProbe {
    Platform,
    Executables,
}

impl RemoteProbe {
    pub const fn command(self) -> &'static str {
        match self {
            Self::Platform => PLATFORM_PROBE_COMMAND,
            Self::Executables => EXECUTABLE_PROBE_COMMAND,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemoteOperatingSystem {
    Linux,
    MacOs,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemoteArchitecture {
    X86_64,
    Aarch64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RemotePlatform {
    pub operating_system: RemoteOperatingSystem,
    pub architecture: RemoteArchitecture,
}

impl RemotePlatform {
    pub fn parse_probe(output: &[u8]) -> io::Result<Self> {
        let fields = parse_nul_fields(output, PLATFORM_PROBE_PREFIX)?;
        if fields.len() != 2 {
            return Err(invalid_probe(
                "platform probe returned an invalid field count",
            ));
        }
        let operating_system = match fields[0] {
            b"Linux" => RemoteOperatingSystem::Linux,
            b"Darwin" => RemoteOperatingSystem::MacOs,
            _ => return Err(invalid_probe("remote operating system is unsupported")),
        };
        let architecture = match fields[1] {
            b"x86_64" | b"amd64" => RemoteArchitecture::X86_64,
            b"aarch64" | b"arm64" => RemoteArchitecture::Aarch64,
            _ => return Err(invalid_probe("remote architecture is unsupported")),
        };
        Ok(Self {
            operating_system,
            architecture,
        })
    }

    pub const fn release_target(self) -> Option<&'static str> {
        match (self.operating_system, self.architecture) {
            (RemoteOperatingSystem::Linux, RemoteArchitecture::X86_64) => {
                Some("x86_64-unknown-linux-gnu")
            }
            _ => None,
        }
    }
}

pub fn parse_executable_probe(output: &[u8]) -> io::Result<Vec<RemoteExecutable>> {
    let fields = parse_nul_fields(output, EXECUTABLE_PROBE_PREFIX)?;
    if fields.len() > MAX_DISCOVERED_EXECUTABLES {
        return Err(invalid_probe(
            "executable probe returned too many candidates",
        ));
    }
    let mut seen = HashSet::new();
    let mut executables = Vec::new();
    for field in fields {
        let value = std::str::from_utf8(field)
            .map_err(|_| invalid_probe("executable probe returned non-UTF-8 data"))?;
        let executable = RemoteExecutable::parse(value.to_owned())?;
        if seen.insert(executable.0.clone()) {
            executables.push(executable);
        }
    }
    Ok(executables)
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

    pub fn prepare_probe(
        target: SshTarget,
        probe: RemoteProbe,
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
        Self::prepare_command_at(
            runtime_directory,
            user_config.as_deref(),
            target,
            probe.command().to_owned(),
            authentication,
        )
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
        Self::prepare_command_at(
            runtime_directory,
            user_config,
            target,
            format!(
                "{} __federation-stdio",
                quote_posix_shell(executable.as_str())
            ),
            authentication,
        )
    }

    fn prepare_command_at(
        runtime_directory: &Path,
        user_config: Option<&Path>,
        target: SshTarget,
        remote_command: String,
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
            remote_command,
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

fn parse_nul_fields<'a>(output: &'a [u8], prefix: &[u8]) -> io::Result<Vec<&'a [u8]>> {
    if output.len() > MAX_PROBE_OUTPUT_BYTES {
        return Err(invalid_probe("remote probe output exceeds the size limit"));
    }
    let mut fields = output.split(|byte| *byte == 0);
    if fields.next() != Some(prefix) {
        return Err(invalid_probe("remote probe returned an invalid header"));
    }
    let mut values = fields.collect::<Vec<_>>();
    if values.last() != Some(&&b""[..]) {
        return Err(invalid_probe("remote probe output is not NUL terminated"));
    }
    values.pop();
    if values.iter().any(|value| value.is_empty()) {
        return Err(invalid_probe("remote probe returned an empty field"));
    }
    Ok(values)
}

fn invalid_probe(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
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

    #[test]
    fn parses_supported_platforms_and_current_release_matrix() {
        let linux = RemotePlatform::parse_probe(b"boomux-platform-v1\0Linux\0x86_64\0").unwrap();
        assert_eq!(linux.operating_system, RemoteOperatingSystem::Linux);
        assert_eq!(linux.architecture, RemoteArchitecture::X86_64);
        assert_eq!(linux.release_target(), Some("x86_64-unknown-linux-gnu"));

        let mac = RemotePlatform::parse_probe(b"boomux-platform-v1\0Darwin\0arm64\0").unwrap();
        assert_eq!(mac.operating_system, RemoteOperatingSystem::MacOs);
        assert_eq!(mac.architecture, RemoteArchitecture::Aarch64);
        assert_eq!(mac.release_target(), None);

        for invalid in [
            &b"wrong\0Linux\0x86_64\0"[..],
            &b"boomux-platform-v1\0Plan9\0x86_64\0"[..],
            &b"boomux-platform-v1\0Linux\0mips\0"[..],
            &b"boomux-platform-v1\0Linux\0x86_64"[..],
        ] {
            assert_eq!(
                RemotePlatform::parse_probe(invalid).unwrap_err().kind(),
                io::ErrorKind::InvalidData
            );
        }
    }

    #[test]
    fn parses_bounded_unique_absolute_executable_candidates() {
        let candidates = parse_executable_probe(
            b"boomux-executables-v1\0/usr/bin/boomux\0/opt/boomux/bin/boomux\0/usr/bin/boomux\0",
        )
        .unwrap();
        assert_eq!(
            candidates
                .iter()
                .map(RemoteExecutable::as_str)
                .collect::<Vec<_>>(),
            ["/usr/bin/boomux", "/opt/boomux/bin/boomux"]
        );
        assert_eq!(
            parse_executable_probe(b"boomux-executables-v1\0relative/boomux\0")
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidInput
        );
    }

    #[test]
    fn probe_invocations_use_only_fixed_remote_commands() {
        let runtime = runtime_directory();
        let invocation = SshInvocation::prepare_command_at(
            &runtime,
            None,
            SshTarget::parse("workbox").unwrap(),
            RemoteProbe::Platform.command().to_owned(),
            SshAuthenticationMode::Interactive,
        )
        .unwrap();
        let arguments = command_arguments(&invocation.command());
        assert_eq!(arguments.last().unwrap(), PLATFORM_PROBE_COMMAND);
        drop(invocation);
        fs::remove_dir_all(runtime).unwrap();
    }
}

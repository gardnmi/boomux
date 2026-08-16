use std::collections::HashSet;
use std::env;
use std::ffi::{OsStr, OsString};
use std::fs::{self, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::io::AsRawFd;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::federation::FederationHandshake;
use crate::protocol::{self, AttachFrame, Envelope, Request, Response};

const MAX_SSH_TARGET_BYTES: usize = 1024;
const MAX_REMOTE_EXECUTABLE_BYTES: usize = 4096;
const MAX_CONTROL_PATH_BYTES: usize = 100;
const MAX_PROBE_OUTPUT_BYTES: usize = 64 * 1024;
const MAX_DISCOVERED_EXECUTABLES: usize = 32;
const MAX_PROBE_STDERR_BYTES: usize = 16 * 1024;
const MAX_PROBE_TIMEOUT: Duration = Duration::from_secs(120);
const CHILD_POLL_INTERVAL: Duration = Duration::from_millis(10);
const PLATFORM_PROBE_PREFIX: &[u8] = b"boomux-platform-v1";
const EXECUTABLE_PROBE_PREFIX: &[u8] = b"boomux-executables-v1";
const INSTALL_DESTINATION_PROBE_PREFIX: &[u8] = b"boomux-install-destination-v1";
const MAX_RELEASE_BYTES: u64 = 128 * 1024 * 1024;
const REMOTE_INSTALL_COMMAND: &str = "set -eu; case \"$HOME\" in /*) ;; *) exit 1 ;; esac; umask 077; directory=$HOME/.local/bin; destination=$directory/boomux; mkdir -p \"$directory\"; temporary=$(mktemp \"$directory/.boomux.XXXXXXXX\"); trap 'rm -f \"$temporary\"' EXIT HUP INT TERM; cat > \"$temporary\"; chmod 755 \"$temporary\"; mv -f \"$temporary\" \"$destination\"; trap - EXIT HUP INT TERM";
const REMOTE_RESTART_COMMAND: &str =
    "case \"$HOME\" in /*) exec \"$HOME/.local/bin/boomux\" daemon restart ;; *) exit 1 ;; esac";

pub const PLATFORM_PROBE_COMMAND: &str = "os=$(uname -s) || exit; arch=$(uname -m) || exit; printf 'boomux-platform-v1\\0%s\\0%s\\0' \"$os\" \"$arch\"";
pub const EXECUTABLE_PROBE_COMMAND: &str = "printf 'boomux-executables-v1\\0'; path=$(command -v boomux 2>/dev/null || true); for candidate in \"$path\" /usr/local/bin/boomux /usr/bin/boomux /opt/homebrew/bin/boomux /home/linuxbrew/.linuxbrew/bin/boomux \"$HOME/.local/bin/boomux\" \"$HOME/.local/share/mise/shims/boomux\" \"$HOME/.nix-profile/bin/boomux\" /run/current-system/sw/bin/boomux; do case \"$candidate\" in /*) [ -f \"$candidate\" ] && [ -x \"$candidate\" ] && printf '%s\\0' \"$candidate\" ;; esac; done";
pub const INSTALL_DESTINATION_PROBE_COMMAND: &str = "case \"$HOME\" in /*) printf 'boomux-install-destination-v1\\0%s\\0' \"$HOME/.local/bin/boomux\" ;; *) exit 1 ;; esac";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SshAuthenticationMode {
    Interactive,
    Batch,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemoteProbe {
    Platform,
    Executables,
    InstallDestination,
}

impl RemoteProbe {
    pub const fn command(self) -> &'static str {
        match self {
            Self::Platform => PLATFORM_PROBE_COMMAND,
            Self::Executables => EXECUTABLE_PROBE_COMMAND,
            Self::InstallDestination => INSTALL_DESTINATION_PROBE_COMMAND,
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteDiscovery {
    pub platform: RemotePlatform,
    pub executables: Vec<RemoteExecutable>,
    pub install_destination: RemoteExecutable,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompatibleRemoteHelper {
    pub executable: RemoteExecutable,
    pub handshake: FederationHandshake,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RemoteInstallSource {
    CurrentBinary(PathBuf),
    Release { target: &'static str, tag: String },
}

impl RemoteInstallSource {
    pub fn description(&self) -> String {
        match self {
            Self::CurrentBinary(path) => format!("current binary {}", path.display()),
            Self::Release { target, tag } => {
                format!("checksum-verified GitHub release {tag} for {target}")
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteInstallPlan {
    pub target: SshTarget,
    pub destination: RemoteExecutable,
    pub source: RemoteInstallSource,
    pub may_restart_daemon: bool,
}

pub enum RemoteBootstrapPlan {
    Ready(CompatibleRemoteHelper),
    Install(RemoteInstallPlan),
}

pub struct RemoteConnection {
    child: Child,
    pid: i32,
    stdin: Option<ChildStdin>,
    stdout: ChildStdout,
    stderr_reader: Option<BoundedReader>,
    pub executable: RemoteExecutable,
    pub handshake: FederationHandshake,
}

pub(crate) struct RemoteAttachmentReader {
    child: Child,
    pid: i32,
    stdout: ChildStdout,
    stderr_reader: Option<BoundedReader>,
}

pub(crate) struct RemoteAttachmentWriter(Option<ChildStdin>);

impl RemoteConnection {
    pub(crate) fn open_attachment(
        mut self,
        request: Request,
        timeout: Duration,
    ) -> io::Result<(Response, RemoteAttachmentReader, RemoteAttachmentWriter)> {
        let response = self.request(request, timeout)?;
        let mut this = std::mem::ManuallyDrop::new(self);
        // Ownership moves to the attachment while suppressing RemoteConnection's
        // process-group cleanup for the still-live channel.
        let (reader, writer) = unsafe {
            (
                RemoteAttachmentReader {
                    child: std::ptr::read(&this.child),
                    pid: this.pid,
                    stdout: std::ptr::read(&this.stdout),
                    stderr_reader: this.stderr_reader.take(),
                },
                RemoteAttachmentWriter(this.stdin.take()),
            )
        };
        Ok((response, reader, writer))
    }
    pub(crate) fn request(&mut self, request: Request, timeout: Duration) -> io::Result<Response> {
        let version = self.handshake.core_protocol_version;
        protocol::write_message(
            self.stdin.as_mut().ok_or_else(|| {
                io::Error::new(io::ErrorKind::BrokenPipe, "remote channel closed")
            })?,
            &Envelope::with_version(version, request),
        )?;
        let response: Envelope<Response> = read_message_with_deadline(&mut self.stdout, timeout)?;
        if response.version != version {
            return Err(invalid_probe("remote response version mismatch"));
        }
        Ok(response.message)
    }

    pub fn ping(&mut self) -> io::Result<()> {
        let version = self.handshake.core_protocol_version;
        protocol::write_message(
            self.stdin.as_mut().ok_or_else(|| {
                io::Error::new(io::ErrorKind::BrokenPipe, "remote channel closed")
            })?,
            &Envelope::with_version(version, Request::Ping),
        )?;
        let response: Envelope<Response> = protocol::read_message(&mut self.stdout)?;
        if response.version == version && response.message == Response::Pong {
            Ok(())
        } else {
            Err(invalid_probe(
                "remote helper returned an invalid ping response",
            ))
        }
    }

    pub(crate) fn node_projection_sync(
        &mut self,
        after: Option<protocol::EventCursor>,
        timeout: Duration,
    ) -> io::Result<protocol::NodeProjectionSync> {
        let version = self.handshake.core_protocol_version;
        if !protocol::ProtocolFeature::NodeProjectionSync.is_supported_by(version) {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "remote Node does not support projection synchronization",
            ));
        }
        let wait_ms = if after.is_some() { 1_000 } else { 0 };
        protocol::write_message(
            self.stdin.as_mut().ok_or_else(|| {
                io::Error::new(io::ErrorKind::BrokenPipe, "remote channel closed")
            })?,
            &Envelope::with_version(version, Request::SyncNodeProjection { after, wait_ms }),
        )?;
        let response: Envelope<Response> = read_message_with_deadline(&mut self.stdout, timeout)?;
        if response.version != version {
            return Err(invalid_probe("remote projection response version mismatch"));
        }
        match response.message {
            Response::NodeProjectionSync { sync } => Ok(sync),
            Response::Error { message, .. } => Err(io::Error::other(message)),
            _ => Err(invalid_probe(
                "remote helper returned an invalid projection response",
            )),
        }
    }
}

impl RemoteAttachmentReader {
    pub(crate) fn read_frame(&mut self) -> io::Result<AttachFrame> {
        AttachFrame::read_from(&mut self.stdout)
    }

    pub(crate) fn close(&mut self) {
        let _ = kill_process_group(self.pid, &mut self.child);
    }
}

impl RemoteAttachmentWriter {
    pub(crate) fn write_frame(&mut self, frame: &AttachFrame, timeout: Duration) -> io::Result<()> {
        if timeout.is_zero() || timeout > MAX_PROBE_TIMEOUT {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "attachment write deadline is outside the supported bound",
            ));
        }
        let stdin = self
            .0
            .as_mut()
            .ok_or_else(|| io::Error::new(io::ErrorKind::BrokenPipe, "remote channel closed"))?;
        let mut bytes = Vec::new();
        frame.write_to(&mut bytes)?;
        write_all_with_deadline(stdin, &bytes, timeout)
    }
}

fn write_all_with_deadline(
    writer: &mut (impl Write + AsRawFd),
    mut bytes: &[u8],
    timeout: Duration,
) -> io::Result<()> {
    let fd = writer.as_raw_fd();
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags == -1 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } == -1 {
        return Err(io::Error::last_os_error());
    }
    let deadline = Instant::now() + timeout;
    let result = (|| {
        while !bytes.is_empty() {
            match writer.write(bytes) {
                Ok(0) => {
                    return Err(io::Error::new(
                        io::ErrorKind::WriteZero,
                        "remote channel closed",
                    ));
                }
                Ok(count) => bytes = &bytes[count..],
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    let remaining = deadline.saturating_duration_since(Instant::now());
                    if remaining.is_zero() {
                        return Err(io::Error::new(
                            io::ErrorKind::TimedOut,
                            "remote attachment write timed out",
                        ));
                    }
                    let mut descriptor = libc::pollfd {
                        fd,
                        events: libc::POLLOUT,
                        revents: 0,
                    };
                    let milliseconds = i32::try_from(remaining.as_millis().min(i32::MAX as u128))
                        .unwrap_or(i32::MAX);
                    if unsafe { libc::poll(&mut descriptor, 1, milliseconds) } == 0 {
                        return Err(io::Error::new(
                            io::ErrorKind::TimedOut,
                            "remote attachment write timed out",
                        ));
                    }
                }
                Err(error) => return Err(error),
            }
        }
        writer.flush()
    })();
    let _ = unsafe { libc::fcntl(fd, libc::F_SETFL, flags) };
    result
}

impl Drop for RemoteAttachmentReader {
    fn drop(&mut self) {
        self.close();
        if let Some(reader) = self.stderr_reader.take() {
            let _ = join_bounded_reader(reader);
        }
    }
}

fn read_message_with_deadline<T: serde::de::DeserializeOwned>(
    reader: &mut (impl Read + AsRawFd),
    timeout: Duration,
) -> io::Result<T> {
    if timeout.is_zero() || timeout > MAX_PROBE_TIMEOUT {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "response deadline is outside the supported bound",
        ));
    }
    let fd = reader.as_raw_fd();
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags == -1 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } == -1 {
        return Err(io::Error::last_os_error());
    }
    let deadline = Instant::now() + timeout;
    let result = (|| {
        let mut length = [0_u8; 4];
        read_exact_with_deadline(reader, fd, &mut length, deadline)?;
        let length = u32::from_be_bytes(length) as usize;
        if length > protocol::MAX_CONTROL_FRAME {
            return Err(invalid_probe("remote control frame exceeds the size limit"));
        }
        let mut bytes = vec![0; length];
        read_exact_with_deadline(reader, fd, &mut bytes, deadline)?;
        serde_json::from_slice(&bytes).map_err(io::Error::other)
    })();
    let _ = unsafe { libc::fcntl(fd, libc::F_SETFL, flags) };
    result
}

fn read_exact_with_deadline(
    reader: &mut impl Read,
    fd: i32,
    mut bytes: &mut [u8],
    deadline: Instant,
) -> io::Result<()> {
    while !bytes.is_empty() {
        match reader.read(bytes) {
            Ok(0) => {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "remote channel closed",
                ));
            }
            Ok(count) => bytes = &mut bytes[count..],
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "remote response timed out",
                    ));
                }
                let mut descriptor = libc::pollfd {
                    fd,
                    events: libc::POLLIN,
                    revents: 0,
                };
                let milliseconds =
                    i32::try_from(remaining.as_millis().min(i32::MAX as u128)).unwrap_or(i32::MAX);
                let status = unsafe { libc::poll(&mut descriptor, 1, milliseconds) };
                if status == 0 {
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "remote response timed out",
                    ));
                }
                if status == -1 {
                    let error = io::Error::last_os_error();
                    if error.kind() != io::ErrorKind::Interrupted {
                        return Err(error);
                    }
                }
            }
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

impl Drop for RemoteConnection {
    fn drop(&mut self) {
        self.stdin.take();
        let _ = kill_process_group(self.pid, &mut self.child);
        if let Some(reader) = self.stderr_reader.take() {
            let _ = join_bounded_reader(reader);
        }
    }
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

    fn matches_local(self) -> bool {
        matches!(
            (
                self.operating_system,
                self.architecture,
                env::consts::OS,
                env::consts::ARCH
            ),
            (
                RemoteOperatingSystem::Linux,
                RemoteArchitecture::X86_64,
                "linux",
                "x86_64"
            ) | (
                RemoteOperatingSystem::Linux,
                RemoteArchitecture::Aarch64,
                "linux",
                "aarch64"
            ) | (
                RemoteOperatingSystem::MacOs,
                RemoteArchitecture::X86_64,
                "macos",
                "x86_64"
            ) | (
                RemoteOperatingSystem::MacOs,
                RemoteArchitecture::Aarch64,
                "macos",
                "aarch64"
            )
        )
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

pub fn parse_install_destination_probe(output: &[u8]) -> io::Result<RemoteExecutable> {
    let fields = parse_nul_fields(output, INSTALL_DESTINATION_PROBE_PREFIX)?;
    if fields.len() != 1 {
        return Err(invalid_probe(
            "install destination probe returned an invalid field count",
        ));
    }
    let value = std::str::from_utf8(fields[0])
        .map_err(|_| invalid_probe("install destination probe returned non-UTF-8 data"))?;
    RemoteExecutable::parse(value.to_owned())
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
    program: OsString,
    directory: PathBuf,
    config_path: PathBuf,
    control_path: PathBuf,
    target: SshTarget,
    remote_command: String,
    authentication: SshAuthenticationMode,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SshProbeOutput {
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

type BoundedReader = thread::JoinHandle<io::Result<(Vec<u8>, bool)>>;

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
        let mut command = Command::new(&self.program);
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
            .args(["-o", "ControlPersist=no"])
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

    pub fn run_probe(&self, timeout: Duration) -> io::Result<SshProbeOutput> {
        run_bounded_command(self.command(), timeout)
    }

    pub fn verify_helper(&self, timeout: Duration) -> io::Result<FederationHandshake> {
        run_helper_probe_command(self.command(), timeout)
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
        Self::prepare_command_with_program_at(
            runtime_directory,
            user_config,
            target,
            remote_command,
            authentication,
            OsStr::new("ssh"),
        )
    }

    fn prepare_command_with_program_at(
        runtime_directory: &Path,
        user_config: Option<&Path>,
        target: SshTarget,
        remote_command: String,
        authentication: SshAuthenticationMode,
        program: &OsStr,
    ) -> io::Result<Self> {
        secure_runtime_directory(runtime_directory)?;
        let nonce = Uuid::new_v4().simple().to_string();
        let directory = runtime_directory.join(format!("ssh-{}", &nonce[..16]));
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
            program: program.to_owned(),
            directory,
            config_path,
            control_path,
            target,
            remote_command,
            authentication,
        })
    }
}

pub fn discover_remote(
    target: SshTarget,
    authentication: SshAuthenticationMode,
    timeout: Duration,
) -> io::Result<RemoteDiscovery> {
    let socket_path = crate::client::socket_path()?;
    let runtime_directory = socket_path
        .parent()
        .ok_or_else(|| io::Error::other("Boomux runtime socket has no parent"))?;
    let user_config = env::var_os("HOME")
        .filter(|home| !home.is_empty())
        .map(PathBuf::from)
        .map(|home| home.join(".ssh/config"));
    discover_remote_at(
        runtime_directory,
        user_config.as_deref(),
        target,
        authentication,
        timeout,
        OsStr::new("ssh"),
    )
}

pub fn plan_remote_bootstrap(
    target: SshTarget,
    authentication: SshAuthenticationMode,
    timeout: Duration,
) -> io::Result<RemoteBootstrapPlan> {
    let discovery = discover_remote(target.clone(), authentication, timeout)?;
    if let Ok(helper) = find_compatible_remote_helper(
        target.clone(),
        &discovery.executables,
        authentication,
        timeout,
    ) {
        return Ok(RemoteBootstrapPlan::Ready(helper));
    }
    let source = select_install_source(discovery.platform)?;
    Ok(RemoteBootstrapPlan::Install(RemoteInstallPlan {
        target,
        destination: discovery.install_destination,
        source,
        may_restart_daemon: !discovery.executables.is_empty(),
    }))
}

pub fn install_remote(
    plan: &RemoteInstallPlan,
    authentication: SshAuthenticationMode,
    timeout: Duration,
) -> io::Result<CompatibleRemoteHelper> {
    let binary = load_install_source(&plan.source)?;
    let invocation =
        prepare_fixed_invocation(plan.target.clone(), REMOTE_INSTALL_COMMAND, authentication)?;
    run_streaming_command(invocation.command(), binary, timeout)?;

    let candidates = [plan.destination.clone()];
    match find_compatible_remote_helper(plan.target.clone(), &candidates, authentication, timeout) {
        Ok(helper) => Ok(helper),
        Err(first_error) if plan.may_restart_daemon => {
            let restart = prepare_fixed_invocation(
                plan.target.clone(),
                REMOTE_RESTART_COMMAND,
                authentication,
            )?;
            run_bounded_command(restart.command(), timeout).map_err(|_| first_error)?;
            find_compatible_remote_helper(plan.target.clone(), &candidates, authentication, timeout)
        }
        Err(error) => Err(error),
    }
}

pub fn connect_remote(
    target: SshTarget,
    helper: CompatibleRemoteHelper,
    authentication: SshAuthenticationMode,
    timeout: Duration,
) -> io::Result<RemoteConnection> {
    if timeout.is_zero() || timeout > MAX_PROBE_TIMEOUT {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "SSH connection timeout is outside the supported bound",
        ));
    }
    let invocation = SshInvocation::prepare(target, helper.executable.clone(), authentication)?;
    let mut command = invocation.command();
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
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
    let pid = i32::try_from(child.id()).map_err(|error| {
        let _ = child.kill();
        let _ = child.wait();
        io::Error::other(format!("child PID overflow: {error}"))
    })?;
    let stdin = child
        .stdin
        .take()
        .ok_or_else(|| io::Error::other("SSH helper stdin was not captured"))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| io::Error::other("SSH helper stdout was not captured"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| io::Error::other("SSH helper stderr was not captured"))?;
    let stderr_reader = match spawn_bounded_reader(stderr, MAX_PROBE_STDERR_BYTES, "stderr") {
        Ok(reader) => reader,
        Err(error) => {
            let _ = kill_process_group(pid, &mut child);
            return Err(error);
        }
    };
    let (sender, receiver) = mpsc::sync_channel(1);
    let handshake_worker = match thread::Builder::new()
        .name("boomux-ssh-handshake".into())
        .spawn(move || {
            let mut stdout = stdout;
            let result = crate::federation::read_handshake(&mut stdout);
            let _ = sender.send((result, stdout));
        }) {
        Ok(worker) => worker,
        Err(error) => {
            let _ = kill_process_group(pid, &mut child);
            let _ = join_bounded_reader(stderr_reader);
            return Err(error);
        }
    };
    let received = receiver.recv_timeout(timeout);
    let (handshake, stdout) = match received {
        Ok((result, stdout)) => (result, stdout),
        Err(_) => {
            let _ = kill_process_group(pid, &mut child);
            handshake_worker
                .join()
                .map_err(|_| io::Error::other("SSH handshake worker panicked"))?;
            let _ = join_bounded_reader(stderr_reader);
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "SSH helper handshake timed out",
            ));
        }
    };
    handshake_worker
        .join()
        .map_err(|_| io::Error::other("SSH handshake worker panicked"))?;
    let handshake = match handshake {
        Ok(handshake) => handshake,
        Err(error) => {
            let _ = kill_process_group(pid, &mut child);
            let _ = join_bounded_reader(stderr_reader);
            return Err(error);
        }
    };
    if let Err(error) = validate_live_handshake(&helper.handshake, &handshake) {
        let _ = kill_process_group(pid, &mut child);
        let _ = join_bounded_reader(stderr_reader);
        return Err(error);
    }
    Ok(RemoteConnection {
        child,
        pid,
        stdin: Some(stdin),
        stdout,
        stderr_reader: Some(stderr_reader),
        executable: helper.executable,
        handshake,
    })
}

fn validate_live_handshake(
    expected: &FederationHandshake,
    actual: &FederationHandshake,
) -> io::Result<()> {
    if !(protocol::MIN_PROTOCOL_VERSION..=protocol::PROTOCOL_VERSION)
        .contains(&actual.core_protocol_version)
    {
        return Err(invalid_probe(
            "remote helper reported an incompatible core protocol",
        ));
    }
    if actual.node_id != expected.node_id {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "remote helper identity changed after bootstrap",
        ));
    }
    Ok(())
}

fn prepare_fixed_invocation(
    target: SshTarget,
    command: &'static str,
    authentication: SshAuthenticationMode,
) -> io::Result<SshInvocation> {
    let socket_path = crate::client::socket_path()?;
    let runtime_directory = socket_path
        .parent()
        .ok_or_else(|| io::Error::other("Boomux runtime socket has no parent"))?;
    let user_config = env::var_os("HOME")
        .filter(|home| !home.is_empty())
        .map(PathBuf::from)
        .map(|home| home.join(".ssh/config"));
    SshInvocation::prepare_command_at(
        runtime_directory,
        user_config.as_deref(),
        target,
        command.to_owned(),
        authentication,
    )
}

fn select_install_source(platform: RemotePlatform) -> io::Result<RemoteInstallSource> {
    if platform.matches_local() {
        return Ok(RemoteInstallSource::CurrentBinary(env::current_exe()?));
    }
    let target = platform.release_target().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::Unsupported,
            "no Boomux release asset supports the remote platform",
        )
    })?;
    Ok(RemoteInstallSource::Release {
        target,
        tag: format!("v{}", env!("CARGO_PKG_VERSION")),
    })
}

fn load_install_source(source: &RemoteInstallSource) -> io::Result<Vec<u8>> {
    match source {
        RemoteInstallSource::CurrentBinary(path) => read_bounded_file(path),
        RemoteInstallSource::Release { target, tag } => download_release_binary(target, tag),
    }
}

fn read_bounded_file(path: &Path) -> io::Result<Vec<u8>> {
    let metadata = fs::metadata(path)?;
    if !metadata.is_file() || metadata.len() == 0 || metadata.len() > MAX_RELEASE_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Boomux install source is not a bounded regular file",
        ));
    }
    fs::read(path)
}

fn download_release_binary(target: &str, tag: &str) -> io::Result<Vec<u8>> {
    let socket_path = crate::client::socket_path()?;
    let parent = socket_path
        .parent()
        .ok_or_else(|| io::Error::other("Boomux runtime socket has no parent"))?;
    secure_runtime_directory(parent)?;
    let directory = parent.join(format!("release-{}", Uuid::new_v4()));
    fs::create_dir(&directory)?;
    fs::set_permissions(&directory, fs::Permissions::from_mode(0o700))?;
    let result = (|| {
        let archive_name = format!("boomux-{tag}-{target}.tar.gz");
        let archive = directory.join(&archive_name);
        let checksum = directory.join(format!("{archive_name}.sha256"));
        let base = format!("https://github.com/gardnmi/boomux/releases/download/{tag}");
        for (url, destination, maximum_size) in [
            (
                format!("{base}/{archive_name}"),
                &archive,
                MAX_RELEASE_BYTES,
            ),
            (format!("{base}/{archive_name}.sha256"), &checksum, 1024),
        ] {
            let status = Command::new("curl")
                .args([
                    "--fail",
                    "--location",
                    "--silent",
                    "--show-error",
                    "--output",
                ])
                .arg(destination)
                .arg("--max-filesize")
                .arg(maximum_size.to_string())
                .arg(url)
                .status()?;
            if !status.success() {
                return Err(io::Error::other("could not download Boomux release asset"));
            }
        }
        if fs::metadata(&archive)?.len() > MAX_RELEASE_BYTES {
            return Err(invalid_probe(
                "Boomux release archive exceeds the size limit",
            ));
        }
        verify_release_checksum(&archive, &checksum, &archive_name)?;
        let member = format!("boomux-{tag}-{target}/boomux");
        let status = Command::new("tar")
            .args(["-xzf"])
            .arg(&archive)
            .args(["-C"])
            .arg(&directory)
            .arg(&member)
            .status()?;
        if !status.success() {
            return Err(io::Error::other("could not extract Boomux release asset"));
        }
        read_bounded_file(&directory.join(member))
    })();
    let _ = fs::remove_dir_all(&directory);
    result
}

fn verify_release_checksum(archive: &Path, checksum: &Path, archive_name: &str) -> io::Result<()> {
    let checksum = fs::read_to_string(checksum)?;
    if checksum.len() > 1024 {
        return Err(invalid_probe(
            "Boomux release checksum exceeds the size limit",
        ));
    }
    let mut fields = checksum.split_ascii_whitespace();
    let expected = fields
        .next()
        .filter(|value| value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit()))
        .ok_or_else(|| invalid_probe("Boomux release checksum is invalid"))?;
    let name = fields
        .next()
        .map(|value| value.trim_start_matches('*'))
        .ok_or_else(|| invalid_probe("Boomux release checksum has no filename"))?;
    if name != archive_name || fields.next().is_some() {
        return Err(invalid_probe(
            "Boomux release checksum names an unexpected asset",
        ));
    }
    let mut file = fs::File::open(archive)?;
    let mut digest = Sha256::new();
    io::copy(&mut file, &mut digest)?;
    let actual = format!("{:x}", digest.finalize());
    if actual != expected.to_ascii_lowercase() {
        return Err(invalid_probe("Boomux release checksum did not match"));
    }
    Ok(())
}

pub fn find_compatible_remote_helper(
    target: SshTarget,
    executables: &[RemoteExecutable],
    authentication: SshAuthenticationMode,
    timeout: Duration,
) -> io::Result<CompatibleRemoteHelper> {
    let socket_path = crate::client::socket_path()?;
    let runtime_directory = socket_path
        .parent()
        .ok_or_else(|| io::Error::other("Boomux runtime socket has no parent"))?;
    let user_config = env::var_os("HOME")
        .filter(|home| !home.is_empty())
        .map(PathBuf::from)
        .map(|home| home.join(".ssh/config"));
    find_compatible_remote_helper_at(
        runtime_directory,
        user_config.as_deref(),
        target,
        executables,
        authentication,
        timeout,
        OsStr::new("ssh"),
    )
}

fn find_compatible_remote_helper_at(
    runtime_directory: &Path,
    user_config: Option<&Path>,
    target: SshTarget,
    executables: &[RemoteExecutable],
    authentication: SshAuthenticationMode,
    timeout: Duration,
    program: &OsStr,
) -> io::Result<CompatibleRemoteHelper> {
    if timeout.is_zero() || timeout > MAX_PROBE_TIMEOUT {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "SSH helper selection timeout is outside the supported bound",
        ));
    }
    let deadline = Instant::now().checked_add(timeout).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "SSH helper selection timeout overflow",
        )
    })?;
    for executable in executables {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "SSH helper selection timed out",
            ));
        }
        let invocation = SshInvocation::prepare_command_with_program_at(
            runtime_directory,
            user_config,
            target.clone(),
            format!(
                "{} __federation-stdio",
                quote_posix_shell(executable.as_str())
            ),
            authentication,
            program,
        )?;
        if let Ok(handshake) = invocation.verify_helper(remaining) {
            return Ok(CompatibleRemoteHelper {
                executable: executable.clone(),
                handshake,
            });
        }
    }
    if Instant::now() >= deadline {
        return Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "SSH helper selection timed out",
        ));
    }
    Err(io::Error::new(
        io::ErrorKind::NotFound,
        "no discovered remote Boomux executable is federation-compatible",
    ))
}

fn discover_remote_at(
    runtime_directory: &Path,
    user_config: Option<&Path>,
    target: SshTarget,
    authentication: SshAuthenticationMode,
    timeout: Duration,
    program: &OsStr,
) -> io::Result<RemoteDiscovery> {
    let run = |probe: RemoteProbe| -> io::Result<SshProbeOutput> {
        SshInvocation::prepare_command_with_program_at(
            runtime_directory,
            user_config,
            target.clone(),
            probe.command().to_owned(),
            authentication,
            program,
        )?
        .run_probe(timeout)
    };
    let platform = RemotePlatform::parse_probe(&run(RemoteProbe::Platform)?.stdout)?;
    let executables = parse_executable_probe(&run(RemoteProbe::Executables)?.stdout)?;
    let install_destination =
        parse_install_destination_probe(&run(RemoteProbe::InstallDestination)?.stdout)?;
    Ok(RemoteDiscovery {
        platform,
        executables,
        install_destination,
    })
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

fn run_bounded_command(mut command: Command, timeout: Duration) -> io::Result<SshProbeOutput> {
    if timeout.is_zero() || timeout > MAX_PROBE_TIMEOUT {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "SSH probe timeout is outside the supported bound",
        ));
    }
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // The child has not executed remote or user code; `setsid` creates a process
    // group that can be terminated and reaped as one bounded probe.
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
    let pid = i32::try_from(child.id()).map_err(|_| io::Error::other("child PID overflow"))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| io::Error::other("SSH probe stdout was not captured"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| io::Error::other("SSH probe stderr was not captured"))?;
    let stdout_reader = spawn_bounded_reader(stdout, MAX_PROBE_OUTPUT_BYTES, "stdout")?;
    let stderr_reader = spawn_bounded_reader(stderr, MAX_PROBE_STDERR_BYTES, "stderr")?;
    let deadline = Instant::now()
        .checked_add(timeout)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "SSH probe timeout overflow"))?;

    let status = loop {
        if let Some(status) = child.try_wait()? {
            break Some(status);
        }
        if Instant::now() >= deadline {
            // Negative PID addresses the process group created by `setsid`.
            if unsafe { libc::kill(-pid, libc::SIGKILL) } == -1 {
                let error = io::Error::last_os_error();
                if error.raw_os_error() != Some(libc::ESRCH) {
                    return Err(error);
                }
            }
            let _ = child.wait();
            break None;
        }
        thread::sleep(CHILD_POLL_INTERVAL);
    };

    let (stdout, stdout_truncated) = join_bounded_reader(stdout_reader)?;
    let (stderr, stderr_truncated) = join_bounded_reader(stderr_reader)?;
    if status.is_none() {
        return Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "SSH probe timed out",
        ));
    }
    if stdout_truncated || stderr_truncated {
        return Err(invalid_probe("SSH probe output exceeds the size limit"));
    }
    if !status.expect("checked above").success() {
        return Err(io::Error::other(
            "SSH probe failed; verify target resolution, the SSH service, credentials, and remote shell support",
        ));
    }
    Ok(SshProbeOutput { stdout, stderr })
}

fn run_streaming_command(
    mut command: Command,
    input: Vec<u8>,
    timeout: Duration,
) -> io::Result<()> {
    if timeout.is_zero() || timeout > MAX_PROBE_TIMEOUT {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "SSH install timeout is outside the supported bound",
        ));
    }
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
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
    let pid = i32::try_from(child.id()).map_err(|_| io::Error::other("child PID overflow"))?;
    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| io::Error::other("SSH install stdin was not captured"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| io::Error::other("SSH install stderr was not captured"))?;
    let stderr_reader = spawn_bounded_reader(stderr, MAX_PROBE_STDERR_BYTES, "stderr")?;
    let writer = thread::Builder::new()
        .name("boomux-ssh-install-input".into())
        .spawn(move || stdin.write_all(&input))?;
    let deadline = Instant::now().checked_add(timeout).ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput, "SSH install timeout overflow")
    })?;
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break Some(status);
        }
        if Instant::now() >= deadline {
            kill_process_group(pid, &mut child)?;
            break None;
        }
        thread::sleep(CHILD_POLL_INTERVAL);
    };
    writer
        .join()
        .map_err(|_| io::Error::other("SSH install input worker panicked"))??;
    let (_, stderr_truncated) = join_bounded_reader(stderr_reader)?;
    if status.is_none() {
        return Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "SSH install timed out",
        ));
    }
    if stderr_truncated {
        return Err(invalid_probe("SSH install stderr exceeds the size limit"));
    }
    if !status.expect("checked above").success() {
        return Err(io::Error::other("remote Boomux install failed"));
    }
    Ok(())
}

fn run_helper_probe_command(
    mut command: Command,
    timeout: Duration,
) -> io::Result<FederationHandshake> {
    if timeout.is_zero() || timeout > MAX_PROBE_TIMEOUT {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "SSH helper probe timeout is outside the supported bound",
        ));
    }
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // The child has not executed remote or user code; `setsid` gives timeout
    // cleanup one process-group boundary.
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
    let pid = i32::try_from(child.id()).map_err(|_| io::Error::other("child PID overflow"))?;
    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| io::Error::other("SSH helper stdin was not captured"))?;
    let mut stdout = child
        .stdout
        .take()
        .ok_or_else(|| io::Error::other("SSH helper stdout was not captured"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| io::Error::other("SSH helper stderr was not captured"))?;
    let stderr_reader = spawn_bounded_reader(stderr, MAX_PROBE_STDERR_BYTES, "stderr")?;
    let (result_sender, result_receiver) = mpsc::sync_channel(1);
    let protocol_worker = thread::Builder::new()
        .name("boomux-ssh-helper-probe".into())
        .spawn(move || {
            let result = (|| {
                let handshake = crate::federation::read_handshake(&mut stdout)?;
                if !(protocol::MIN_PROTOCOL_VERSION..=protocol::PROTOCOL_VERSION)
                    .contains(&handshake.core_protocol_version)
                {
                    return Err(invalid_probe(
                        "remote helper reported an incompatible core protocol",
                    ));
                }
                protocol::write_message(
                    &mut stdin,
                    &Envelope::with_version(handshake.core_protocol_version, Request::Ping),
                )?;
                let response: Envelope<Response> = protocol::read_message(&mut stdout)?;
                if response.version != handshake.core_protocol_version
                    || response.message != Response::Pong
                {
                    return Err(invalid_probe(
                        "remote helper returned an invalid compatibility response",
                    ));
                }
                Ok(handshake)
            })();
            let _ = result_sender.send(result);
        })?;
    let deadline = Instant::now()
        .checked_add(timeout)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "SSH probe timeout overflow"))?;
    let result = match result_receiver.recv_timeout(timeout) {
        Ok(result) => result,
        Err(mpsc::RecvTimeoutError::Timeout) => Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "SSH helper handshake timed out",
        )),
        Err(mpsc::RecvTimeoutError::Disconnected) => {
            Err(io::Error::other("SSH helper probe worker stopped"))
        }
    };
    let status = if result.is_err() {
        kill_process_group(pid, &mut child)?;
        None
    } else {
        let status = loop {
            if let Some(status) = child.try_wait()? {
                break Some(status);
            }
            if Instant::now() >= deadline {
                kill_process_group(pid, &mut child)?;
                break None;
            }
            thread::sleep(CHILD_POLL_INTERVAL);
        };
        // The SSH leader can exit while a descendant still owns a pipe.
        kill_process_group(pid, &mut child)?;
        status
    };
    protocol_worker
        .join()
        .map_err(|_| io::Error::other("SSH helper probe worker panicked"))?;
    let (_, stderr_truncated) = join_bounded_reader(stderr_reader)?;
    if stderr_truncated {
        return Err(invalid_probe("SSH helper stderr exceeds the size limit"));
    }
    if result.is_ok() && status.is_none() {
        return Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "SSH helper did not exit before the compatibility deadline",
        ));
    }
    if status.is_some_and(|status| !status.success()) {
        return Err(io::Error::other("SSH helper exited unsuccessfully"));
    }
    result
}

fn kill_process_group(pid: i32, child: &mut std::process::Child) -> io::Result<()> {
    if unsafe { libc::kill(-pid, libc::SIGKILL) } == -1 {
        let error = io::Error::last_os_error();
        if error.raw_os_error() != Some(libc::ESRCH) {
            return Err(error);
        }
    }
    let _ = child.wait();
    Ok(())
}

fn spawn_bounded_reader(
    mut reader: impl Read + Send + 'static,
    limit: usize,
    stream: &'static str,
) -> io::Result<BoundedReader> {
    thread::Builder::new()
        .name(format!("boomux-ssh-{stream}"))
        .spawn(move || {
            let mut bytes = Vec::new();
            reader
                .by_ref()
                .take((limit + 1) as u64)
                .read_to_end(&mut bytes)?;
            let truncated = bytes.len() > limit;
            bytes.truncate(limit);
            Ok((bytes, truncated))
        })
}

fn join_bounded_reader(reader: BoundedReader) -> io::Result<(Vec<u8>, bool)> {
    reader
        .join()
        .map_err(|_| io::Error::other("SSH probe output reader panicked"))?
}

#[cfg(test)]
fn command_arguments(command: &Command) -> Vec<OsString> {
    command.get_args().map(OsStr::to_os_string).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::federation::{FEDERATION_VERSION, FederationConnectionMode};

    fn runtime_directory() -> PathBuf {
        env::temp_dir().join(format!("boomux-ssh-{}", Uuid::new_v4()))
    }

    fn shell_printf(bytes: &[u8]) -> String {
        let escaped = bytes
            .iter()
            .map(|byte| format!("\\{byte:03o}"))
            .collect::<String>();
        format!("printf '{escaped}'")
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

        assert_eq!(
            parse_install_destination_probe(
                b"boomux-install-destination-v1\0/home/person/.local/bin/boomux\0"
            )
            .unwrap()
            .as_str(),
            "/home/person/.local/bin/boomux"
        );
        assert_eq!(
            parse_install_destination_probe(b"boomux-install-destination-v1\0relative/boomux\0")
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

    #[test]
    fn bounded_runner_captures_output_and_classifies_exit() {
        let mut command = Command::new("sh");
        command.args(["-c", "printf stdout; printf stderr >&2"]);
        let output = run_bounded_command(command, Duration::from_secs(1)).unwrap();
        assert_eq!(output.stdout, b"stdout");
        assert_eq!(output.stderr, b"stderr");

        let mut command = Command::new("sh");
        command.args(["-c", "exit 7"]);
        let error = run_bounded_command(command, Duration::from_secs(1)).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::Other);
        assert_eq!(
            error.to_string(),
            "SSH probe failed; verify target resolution, the SSH service, credentials, and remote shell support"
        );
    }

    #[test]
    fn bounded_runner_kills_process_group_on_timeout() {
        let started = Instant::now();
        let mut command = Command::new("sh");
        command.args(["-c", "sleep 5"]);
        assert_eq!(
            run_bounded_command(command, Duration::from_millis(50))
                .unwrap_err()
                .kind(),
            io::ErrorKind::TimedOut
        );
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn bounded_runner_rejects_oversized_output_and_timeout() {
        let mut command = Command::new("sh");
        command.args(["-c", "head -c 70000 /dev/zero"]);
        assert_eq!(
            run_bounded_command(command, Duration::from_secs(1))
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidData
        );

        assert_eq!(
            run_bounded_command(Command::new("true"), Duration::ZERO)
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidInput
        );
    }

    #[test]
    fn helper_probe_verifies_handshake_and_ping_on_one_channel() {
        let handshake = FederationHandshake {
            version: FEDERATION_VERSION,
            node_id: Uuid::new_v4().to_string(),
            helper_version: "0.18.0".into(),
            core_protocol_version: protocol::PROTOCOL_VERSION,
            connection_mode: FederationConnectionMode::AdHoc,
        };
        let mut handshake_bytes = Vec::new();
        crate::federation::write_handshake(&mut handshake_bytes, &handshake).unwrap();
        let mut request_bytes = Vec::new();
        protocol::write_message(
            &mut request_bytes,
            &Envelope::with_version(protocol::PROTOCOL_VERSION, Request::Ping),
        )
        .unwrap();
        let mut response_bytes = Vec::new();
        protocol::write_message(
            &mut response_bytes,
            &Envelope::with_version(protocol::PROTOCOL_VERSION, Response::Pong),
        )
        .unwrap();
        let mut command = Command::new("sh");
        command.args([
            "-c",
            &format!(
                "{}; dd bs=1 count={} of=/dev/null 2>/dev/null; {}",
                shell_printf(&handshake_bytes),
                request_bytes.len(),
                shell_printf(&response_bytes),
            ),
        ]);

        assert_eq!(
            run_helper_probe_command(command, Duration::from_secs(1)).unwrap(),
            handshake
        );
    }

    #[test]
    fn helper_probe_rejects_invalid_handshakes_and_timeouts() {
        let mut invalid = Command::new("sh");
        invalid.args(["-c", "printf 'NOTMAGIC'"]);
        assert_eq!(
            run_helper_probe_command(invalid, Duration::from_secs(1))
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidData
        );

        let started = Instant::now();
        let mut timeout = Command::new("sh");
        timeout.args(["-c", "sleep 5"]);
        assert_eq!(
            run_helper_probe_command(timeout, Duration::from_millis(50))
                .unwrap_err()
                .kind(),
            io::ErrorKind::TimedOut
        );
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn live_handshake_pins_identity_but_allows_compatible_version_changes() {
        let expected = FederationHandshake {
            version: FEDERATION_VERSION,
            node_id: Uuid::new_v4().to_string(),
            helper_version: "0.17.0".into(),
            core_protocol_version: protocol::MIN_PROTOCOL_VERSION,
            connection_mode: FederationConnectionMode::AdHoc,
        };
        let mut changed = expected.clone();
        changed.helper_version = "0.18.0".into();
        changed.core_protocol_version = protocol::PROTOCOL_VERSION;
        validate_live_handshake(&expected, &changed).unwrap();

        changed.node_id = Uuid::new_v4().to_string();
        assert_eq!(
            validate_live_handshake(&expected, &changed)
                .unwrap_err()
                .kind(),
            io::ErrorKind::PermissionDenied
        );
    }

    #[test]
    fn helper_selection_skips_incompatible_discovered_executables() {
        let runtime = runtime_directory();
        fs::create_dir_all(&runtime).unwrap();
        let ssh = runtime.join("ssh");
        let handshake = FederationHandshake {
            version: FEDERATION_VERSION,
            node_id: Uuid::new_v4().to_string(),
            helper_version: "0.18.0".into(),
            core_protocol_version: protocol::PROTOCOL_VERSION,
            connection_mode: FederationConnectionMode::AdHoc,
        };
        let mut handshake_bytes = Vec::new();
        crate::federation::write_handshake(&mut handshake_bytes, &handshake).unwrap();
        let mut request_bytes = Vec::new();
        protocol::write_message(
            &mut request_bytes,
            &Envelope::with_version(protocol::PROTOCOL_VERSION, Request::Ping),
        )
        .unwrap();
        let mut response_bytes = Vec::new();
        protocol::write_message(
            &mut response_bytes,
            &Envelope::with_version(protocol::PROTOCOL_VERSION, Response::Pong),
        )
        .unwrap();
        fs::write(
            &ssh,
            format!(
                "#!/bin/sh\nlast=\nfor arg do last=$arg; done\ncase \"$last\" in\n  \"'/bad/boomux' __federation-stdio\") printf 'NOTMAGIC' ;;\n  \"'/good/boomux' __federation-stdio\") {}; dd bs=1 count={} of=/dev/null 2>/dev/null; {} ;;\n  *) exit 64 ;;\nesac\n",
                shell_printf(&handshake_bytes),
                request_bytes.len(),
                shell_printf(&response_bytes),
            ),
        )
        .unwrap();
        fs::set_permissions(&ssh, fs::Permissions::from_mode(0o700)).unwrap();
        let candidates = [
            RemoteExecutable::parse("/bad/boomux").unwrap(),
            RemoteExecutable::parse("/good/boomux").unwrap(),
        ];

        let compatible = find_compatible_remote_helper_at(
            &runtime,
            None,
            SshTarget::parse("workbox").unwrap(),
            &candidates,
            SshAuthenticationMode::Batch,
            Duration::from_secs(1),
            ssh.as_os_str(),
        )
        .unwrap();
        assert_eq!(compatible.executable, candidates[1]);
        assert_eq!(compatible.handshake, handshake);
        assert_eq!(
            fs::read_dir(&runtime)
                .unwrap()
                .filter_map(Result::ok)
                .filter(|entry| entry.file_name().to_string_lossy().starts_with("ssh-"))
                .count(),
            0
        );
        fs::remove_dir_all(runtime).unwrap();
    }

    #[test]
    fn discovery_runs_fixed_probes_through_the_real_ssh_argv_boundary() {
        let runtime = runtime_directory();
        fs::create_dir_all(&runtime).unwrap();
        let log = runtime.join("arguments");
        let ssh = runtime.join("ssh");
        let quoted_log = quote_posix_shell(log.to_str().unwrap());
        fs::write(
            &ssh,
            format!(
                "#!/bin/sh\nprintf 'call\\0' >> {quoted_log}\nfor arg do printf '%s\\0' \"$arg\" >> {quoted_log}; done\nprintf 'end\\0' >> {quoted_log}\nlast=\nfor arg do last=$arg; done\ncase \"$last\" in\n  *boomux-platform-v1*) printf 'boomux-platform-v1\\0Linux\\0x86_64\\0' ;;\n  *boomux-executables-v1*) printf 'boomux-executables-v1\\0/usr/bin/boomux\\0/opt/homebrew/bin/boomux\\0' ;;\n  *boomux-install-destination-v1*) printf 'boomux-install-destination-v1\\0/home/person/.local/bin/boomux\\0' ;;\n  *) exit 64 ;;\nesac\n"
            ),
        )
        .unwrap();
        fs::set_permissions(&ssh, fs::Permissions::from_mode(0o700)).unwrap();

        let discovery = discover_remote_at(
            &runtime,
            Some(Path::new("/home/person/.ssh/config")),
            SshTarget::parse("user@workbox").unwrap(),
            SshAuthenticationMode::Interactive,
            Duration::from_secs(1),
            ssh.as_os_str(),
        )
        .unwrap();
        assert_eq!(
            discovery.platform,
            RemotePlatform {
                operating_system: RemoteOperatingSystem::Linux,
                architecture: RemoteArchitecture::X86_64,
            }
        );
        assert_eq!(
            discovery
                .executables
                .iter()
                .map(RemoteExecutable::as_str)
                .collect::<Vec<_>>(),
            ["/usr/bin/boomux", "/opt/homebrew/bin/boomux"]
        );
        assert_eq!(
            discovery.install_destination.as_str(),
            "/home/person/.local/bin/boomux"
        );

        let arguments = fs::read(&log).unwrap();
        let fields = arguments
            .split(|byte| *byte == 0)
            .filter(|field| !field.is_empty())
            .map(|field| std::str::from_utf8(field).unwrap())
            .collect::<Vec<_>>();
        assert_eq!(fields.iter().filter(|field| **field == "call").count(), 3);
        assert_eq!(
            fields
                .iter()
                .filter(|field| **field == "user@workbox")
                .count(),
            3
        );
        assert!(fields.contains(&PLATFORM_PROBE_COMMAND));
        assert!(fields.contains(&EXECUTABLE_PROBE_COMMAND));
        assert!(fields.contains(&INSTALL_DESTINATION_PROBE_COMMAND));
        assert_eq!(
            fs::read_dir(&runtime)
                .unwrap()
                .filter_map(Result::ok)
                .filter(|entry| entry.file_name().to_string_lossy().starts_with("ssh-"))
                .count(),
            0
        );
        fs::remove_dir_all(runtime).unwrap();
    }

    #[test]
    fn fixed_install_command_streams_private_executable_and_replaces_atomically() {
        let directory = runtime_directory();
        fs::create_dir_all(directory.join(".local/bin")).unwrap();
        let destination = directory.join(".local/bin/boomux");
        fs::write(&destination, b"previous").unwrap();
        fs::set_permissions(&destination, fs::Permissions::from_mode(0o755)).unwrap();
        let mut command = Command::new("sh");
        command
            .args(["-c", REMOTE_INSTALL_COMMAND])
            .env("HOME", &directory);
        run_streaming_command(command, b"replacement".to_vec(), Duration::from_secs(1)).unwrap();
        assert_eq!(fs::read(&destination).unwrap(), b"replacement");
        assert_eq!(fs::metadata(&destination).unwrap().mode() & 0o777, 0o755);
        assert!(
            fs::read_dir(directory.join(".local/bin"))
                .unwrap()
                .filter_map(Result::ok)
                .all(|entry| !entry.file_name().to_string_lossy().starts_with(".boomux."))
        );
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn failed_streamed_install_leaves_previous_binary_usable() {
        let directory = runtime_directory();
        fs::create_dir_all(directory.join("bin")).unwrap();
        let destination = directory.join("bin/boomux");
        fs::write(&destination, b"previous").unwrap();
        let temporary = directory.join("bin/.boomux.test");
        let mut command = Command::new("sh");
        command.args([
            "-c",
            "set -eu; trap 'rm -f \"$TEMPORARY\"' EXIT; cat > \"$TEMPORARY\"; false; mv -f \"$TEMPORARY\" \"$DESTINATION\"",
        ]);
        command
            .env("TEMPORARY", &temporary)
            .env("DESTINATION", &destination);
        assert!(
            run_streaming_command(command, b"replacement".to_vec(), Duration::from_secs(1))
                .is_err()
        );
        assert_eq!(fs::read(&destination).unwrap(), b"previous");
        assert!(!temporary.exists());
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn release_checksum_requires_exact_asset_name_and_digest() {
        let directory = runtime_directory();
        fs::create_dir_all(&directory).unwrap();
        let archive = directory.join("asset.tar.gz");
        let checksum = directory.join("asset.tar.gz.sha256");
        fs::write(&archive, b"archive").unwrap();
        let digest = format!("{:x}", Sha256::digest(b"archive"));
        fs::write(&checksum, format!("{digest}  asset.tar.gz\n")).unwrap();
        verify_release_checksum(&archive, &checksum, "asset.tar.gz").unwrap();

        fs::write(&checksum, format!("{digest}  another.tar.gz\n")).unwrap();
        assert_eq!(
            verify_release_checksum(&archive, &checksum, "asset.tar.gz")
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidData
        );
        fs::write(&checksum, format!("{}  asset.tar.gz\n", "0".repeat(64))).unwrap();
        assert_eq!(
            verify_release_checksum(&archive, &checksum, "asset.tar.gz")
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidData
        );
        fs::remove_dir_all(directory).unwrap();
    }
}

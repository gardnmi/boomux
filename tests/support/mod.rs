use std::fs;
use std::io;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use boomux::client::{Attachment, Client, ClientError, RemoteError};
use boomux::protocol::{AttachFrame, ErrorCode, TerminalProfile};
use uuid::Uuid;

pub(crate) const TIMEOUT: Duration = Duration::from_secs(10);

pub(crate) fn assert_generated_name(name: &str) {
    let Some((adjective, noun)) = name.split_once('-') else {
        panic!("generated name is not adjective-noun: {name}");
    };
    assert!(!adjective.is_empty());
    assert!(!noun.is_empty());
    assert!(adjective.bytes().all(|byte| byte.is_ascii_lowercase()));
    assert!(noun.bytes().all(|byte| byte.is_ascii_lowercase()));
}

pub(crate) struct TestDaemon {
    pub(crate) executable: PathBuf,
    pub(crate) runtime_dir: PathBuf,
    pub(crate) child: Option<Child>,
    pub(crate) client: Client,
}

impl TestDaemon {
    pub(crate) fn start() -> Self {
        Self::start_with(|command, _| {
            command
                .env("TERM", "daemon-term")
                .env("COLORTERM", "daemon-color")
                .env("TERM_PROGRAM", "daemon-program")
                .env("TERM_PROGRAM_VERSION", "daemon-version")
                .env("BOOMUX_DAEMON_ONLY", "must-not-leak");
        })
    }

    pub(crate) fn start_with(configure: impl FnOnce(&mut Command, &Path)) -> Self {
        let executable = PathBuf::from(env!("CARGO_BIN_EXE_boomux"));
        let runtime_dir = std::env::temp_dir().join(format!(
            "boomux-integration-{}-{}",
            std::process::id(),
            Uuid::new_v4()
        ));
        fs::create_dir(&runtime_dir).unwrap();
        let mut command = Command::new(&executable);
        command
            .args(["daemon", "run"])
            .env("XDG_RUNTIME_DIR", &runtime_dir)
            .env("XDG_STATE_HOME", runtime_dir.join("state"))
            .env("SHELL", "/bin/sh")
            .env("BOOMUX_NATIVE_TEST_HOOKS", "1")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        configure(&mut command, &runtime_dir);
        let child = command.spawn().unwrap();
        let client = Client::from_socket_path(runtime_dir.join("boomux/daemon.sock"));
        wait_until(|| client.ping().is_ok(), "daemon did not accept requests");
        Self {
            executable,
            runtime_dir,
            child: Some(child),
            client,
        }
    }

    pub(crate) fn command(&self) -> Command {
        let mut command = Command::new(&self.executable);
        command.env("XDG_RUNTIME_DIR", &self.runtime_dir);
        command.env("XDG_STATE_HOME", self.runtime_dir.join("state"));
        command.env("BOOMUX_NATIVE_TEST_HOOKS", "1");
        command
    }

    pub(crate) fn restart(&mut self) {
        self.restart_with(|_| {});
    }

    pub(crate) fn restart_with(&mut self, configure: impl FnOnce(&mut Command)) {
        assert!(self.child.is_none());
        let mut command = self.command();
        command
            .args(["daemon", "run"])
            .env("SHELL", "/bin/sh")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        configure(&mut command);
        let child = command.spawn().unwrap();
        self.child = Some(child);
        wait_until(
            || self.client.ping().is_ok(),
            "restarted daemon did not accept requests",
        );
    }

    pub(crate) fn stop_with_cli(&mut self) {
        let output = self.command().args(["daemon", "stop"]).output().unwrap();
        assert!(
            output.status.success(),
            "daemon stop failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(String::from_utf8_lossy(&output.stdout).contains("Stopped Boomux daemon"));
        let mut child = self.child.take().unwrap();
        wait_until(
            || child.try_wait().unwrap().is_some(),
            "daemon did not exit after shutdown",
        );
        wait_until(
            || !self.client.socket_path().exists(),
            "daemon socket was not removed",
        );
    }

    pub(crate) fn crash(&mut self) {
        let mut child = self.child.take().unwrap();
        child.kill().unwrap();
        child.wait().unwrap();
    }
}

impl Drop for TestDaemon {
    fn drop(&mut self) {
        if self.client.socket_path().exists() {
            let _ = self.client.shutdown();
        }
        if let Some(child) = self.child.as_mut() {
            let _ = child.kill();
            let _ = child.wait();
        }
        let _ = fs::remove_dir_all(&self.runtime_dir);
    }
}

pub(crate) fn wait_for_attach_with_profile(
    client: &Client,
    shell_id: &str,
    profile: TerminalProfile,
) -> Attachment {
    let deadline = Instant::now() + TIMEOUT;
    loop {
        match client.attach(shell_id, false, profile.clone()) {
            Ok(attachment) => return attachment,
            Err(error) => {
                assert!(
                    Instant::now() < deadline,
                    "attachment did not reconnect: {error}"
                );
                thread::sleep(Duration::from_millis(20));
            }
        }
    }
}

pub(crate) fn acknowledge_reconnect(stream: &mut UnixStream) {
    stream.set_read_timeout(Some(TIMEOUT)).unwrap();
    loop {
        match AttachFrame::read_from(stream).unwrap() {
            AttachFrame::Reconnect => {
                AttachFrame::ReconnectAck.write_to(stream).unwrap();
                return;
            }
            AttachFrame::Output(_) => {}
            frame => panic!("expected reconnect frame, got {frame:?}"),
        }
    }
}

pub(crate) fn profile() -> TerminalProfile {
    TerminalProfile {
        term: Some("attachment-term".into()),
        colorterm: Some("truecolor".into()),
        term_program: Some("integration-terminal".into()),
        term_program_version: Some("1.0".into()),
        rows: 24,
        cols: 80,
        pixel_width: 800,
        pixel_height: 600,
    }
}

pub(crate) fn read_until(stream: &mut UnixStream, needle: &[u8]) -> Vec<u8> {
    stream
        .set_read_timeout(Some(Duration::from_millis(200)))
        .unwrap();
    let deadline = Instant::now() + TIMEOUT;
    let mut output = Vec::new();
    while Instant::now() < deadline {
        match AttachFrame::read_from(stream) {
            Ok(AttachFrame::Output(bytes)) => {
                output.extend(bytes);
                if contains(&output, needle) {
                    return output;
                }
            }
            Ok(AttachFrame::Detached) => break,
            Ok(frame) => panic!("unexpected daemon frame: {frame:?}"),
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                ) => {}
            Err(error) => panic!("attachment read failed: {error}"),
        }
    }
    panic!(
        "did not receive {:?}; output was {:?}",
        String::from_utf8_lossy(needle),
        String::from_utf8_lossy(&output)
    );
}

pub(crate) fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}

pub(crate) fn assert_remote_code(error: &ClientError, expected: ErrorCode) {
    assert!(matches!(
        error,
        ClientError::Remote(RemoteError {
            code: Some(actual),
            ..
        }) if *actual == expected
    ));
}

pub(crate) fn parse_pid(output: &[u8], label: &str) -> Option<libc::pid_t> {
    let output = String::from_utf8_lossy(output);
    let value = output.rsplit_once(label)?.1;
    let digits = value
        .trim_start_matches(|character: char| !character.is_ascii_digit())
        .chars()
        .take_while(char::is_ascii_digit)
        .collect::<String>();
    digits.parse().ok()
}

pub(crate) fn process_exists(pid: libc::pid_t) -> bool {
    // Signal zero performs existence and permission checks without changing the process.
    unsafe { libc::kill(pid, 0) == 0 }
}

pub(crate) fn wait_until(mut condition: impl FnMut() -> bool, message: &str) {
    let deadline = Instant::now() + TIMEOUT;
    while Instant::now() < deadline {
        if condition() {
            return;
        }
        thread::sleep(Duration::from_millis(20));
    }
    panic!("{message}");
}

use std::fs;
use std::io;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use boomux::client::{Attachment, Client};
use boomux::protocol::{AttachFrame, ShellSpec, ShellStatus, TerminalProfile};
use uuid::Uuid;

const TIMEOUT: Duration = Duration::from_secs(5);

struct TestDaemon {
    executable: PathBuf,
    runtime_dir: PathBuf,
    child: Option<Child>,
    client: Client,
}

impl TestDaemon {
    fn start() -> Self {
        let executable = PathBuf::from(env!("CARGO_BIN_EXE_boomux"));
        let runtime_dir = std::env::temp_dir().join(format!(
            "boomux-integration-{}-{}",
            std::process::id(),
            Uuid::new_v4()
        ));
        fs::create_dir(&runtime_dir).unwrap();
        let child = Command::new(&executable)
            .args(["daemon", "run"])
            .env("XDG_RUNTIME_DIR", &runtime_dir)
            .env("SHELL", "/bin/sh")
            .env("TERM", "daemon-term")
            .env("COLORTERM", "daemon-color")
            .env("TERM_PROGRAM", "daemon-program")
            .env("TERM_PROGRAM_VERSION", "daemon-version")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let client = Client::from_socket_path(runtime_dir.join("boomux/daemon.sock"));
        wait_until(|| client.ping().is_ok(), "daemon did not accept requests");
        Self {
            executable,
            runtime_dir,
            child: Some(child),
            client,
        }
    }

    fn command(&self) -> Command {
        let mut command = Command::new(&self.executable);
        command.env("XDG_RUNTIME_DIR", &self.runtime_dir);
        command
    }

    fn stop_with_cli(&mut self) {
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
}

impl Drop for TestDaemon {
    fn drop(&mut self) {
        if let Some(child) = self.child.as_mut() {
            let _ = child.kill();
            let _ = child.wait();
        }
        let _ = fs::remove_dir_all(&self.runtime_dir);
    }
}

#[test]
fn native_daemon_lifecycle() {
    let mut daemon = TestDaemon::start();

    let status = daemon
        .command()
        .args(["daemon", "status"])
        .output()
        .unwrap();
    assert!(status.status.success());
    assert!(String::from_utf8_lossy(&status.stdout).contains("running (protocol 3"));

    let mut duplicate = daemon
        .command()
        .args(["daemon", "run"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    wait_until(
        || duplicate.try_wait().unwrap().is_some(),
        "second daemon did not reject the held startup lock",
    );
    assert!(!duplicate.wait().unwrap().success());

    let output = daemon
        .command()
        .args(["workspace", "create", "cli-test"])
        .output()
        .unwrap();
    assert!(output.status.success());
    let output = daemon
        .command()
        .args(["workspace", "inspect", "cli-test"])
        .output()
        .unwrap();
    assert!(output.status.success());
    assert!(String::from_utf8_lossy(&output.stdout).contains("NAME\tcli-test"));
    let output = daemon
        .command()
        .args(["workspace", "rename", "cli-test", "cli-renamed"])
        .output()
        .unwrap();
    assert!(output.status.success());
    let output = daemon
        .command()
        .args([
            "shell",
            "create",
            "cli-renamed",
            "--name",
            "checks",
            "--cwd",
        ])
        .arg(std::env::temp_dir())
        .args(["--", "/bin/sh", "-c", "printf lifecycle"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "shell create failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let cli_workspace = daemon
        .client
        .snapshot()
        .unwrap()
        .workspaces
        .into_iter()
        .find(|workspace| workspace.name == "cli-renamed")
        .unwrap();
    assert_eq!(cli_workspace.shells[0].status, ShellStatus::Pending);
    let output = daemon
        .command()
        .args(["shell", "inspect", "checks", "--workspace", "cli-renamed"])
        .output()
        .unwrap();
    assert!(output.status.success());
    assert!(String::from_utf8_lossy(&output.stdout).contains("STATUS\tpending"));
    let output = daemon
        .command()
        .args([
            "shell",
            "rename",
            "checks",
            "tests",
            "--workspace",
            "cli-renamed",
        ])
        .output()
        .unwrap();
    assert!(output.status.success());
    let output = daemon
        .command()
        .args(["shell", "close", "tests", "--workspace", "cli-renamed"])
        .output()
        .unwrap();
    assert!(output.status.success());
    let output = daemon
        .command()
        .args(["workspace", "close", "cli-renamed"])
        .output()
        .unwrap();
    assert!(output.status.success());

    let generated_shell = daemon
        .client
        .create_shell_with_workspace(ShellSpec {
            name: "shell-1".into(),
            command: vec!["/bin/sh".into(), "-c".into(), "sleep 5".into()],
            cwd: std::env::temp_dir(),
        })
        .unwrap();
    let generated_workspace = daemon
        .client
        .get_workspace(&generated_shell.workspace_id)
        .unwrap();
    assert_eq!(generated_workspace.name, "workspace-1");
    assert_eq!(generated_shell.status, ShellStatus::Pending);
    assert!(
        daemon
            .client
            .read_shell(&generated_shell.id, 1024)
            .unwrap()
            .is_empty()
    );
    let failed_shell = daemon
        .client
        .create_shell(
            &generated_workspace.id,
            ShellSpec {
                name: "failed".into(),
                command: vec!["/definitely/missing/boomux-command".into()],
                cwd: std::env::temp_dir(),
            },
        )
        .unwrap();
    let error = daemon
        .client
        .attach(&failed_shell.id, false, profile())
        .unwrap_err();
    assert!(error.to_string().contains("could not start shell"));
    assert_eq!(
        daemon.client.get_shell(&failed_shell.id).unwrap().status,
        ShellStatus::Pending
    );
    daemon
        .client
        .close_workspace(&generated_workspace.id)
        .unwrap();

    let workspace = daemon
        .client
        .create_workspace(
            "integration",
            vec![ShellSpec::login("shell-1", std::env::temp_dir())],
        )
        .unwrap();
    let shell = workspace.shells.first().unwrap();
    let shell_id = shell.id.clone();

    assert_eq!(shell.status, ShellStatus::Pending);
    let attachment = daemon.client.attach(&shell_id, false, profile()).unwrap();
    assert!(attachment.replay.len() <= 1024 * 1024);
    assert!(attachment.warning.is_none());
    let mut first = attachment.stream;
    AttachFrame::Input(b"stty -echo\n".to_vec())
        .write_to(&mut first)
        .unwrap();
    thread::sleep(Duration::from_millis(100));
    AttachFrame::Input(b"stty size\n".to_vec())
        .write_to(&mut first)
        .unwrap();
    assert!(contains(&read_until(&mut first, b"24 80"), b"24 80"));
    AttachFrame::Input(
        b"printf 'env=%s|%s|%s|%s\n' \"$TERM\" \"$COLORTERM\" \"$TERM_PROGRAM\" \"$TERM_PROGRAM_VERSION\"\n"
            .to_vec(),
    )
    .write_to(&mut first)
    .unwrap();
    let expected = b"env=attachment-term|truecolor|integration-terminal|1.0";
    assert!(contains(&read_until(&mut first, expected), expected));
    AttachFrame::Input(b"printf 'transport-ok\\n'\n".to_vec())
        .write_to(&mut first)
        .unwrap();
    let output = read_until(&mut first, b"transport-ok");
    assert!(contains(&output, b"transport-ok"));

    drop(first);
    wait_until(
        || {
            matches!(
                daemon.client.get_shell(&shell_id).unwrap().status,
                ShellStatus::Running
            )
        },
        "shell stopped after its attachment disconnected",
    );

    let mut second = wait_for_attach(&daemon.client, &shell_id).stream;
    let error = daemon
        .client
        .attach(&shell_id, false, profile())
        .unwrap_err();
    assert!(error.to_string().contains("active controller"));
    let mut mismatched = profile();
    mismatched.term = Some("alacritty".into());
    let takeover = daemon.client.attach(&shell_id, true, mismatched).unwrap();
    assert!(takeover.warning.as_deref().is_some_and(|warning| {
        warning.contains("attachment-term") && warning.contains("alacritty")
    }));
    let mut takeover = takeover.stream;
    second
        .set_read_timeout(Some(Duration::from_secs(1)))
        .unwrap();
    assert!(AttachFrame::read_from(&mut second).is_err());

    AttachFrame::Resize {
        rows: 40,
        cols: 100,
        pixel_width: 1200,
        pixel_height: 800,
    }
    .write_to(&mut takeover)
    .unwrap();
    AttachFrame::Input(b"stty size\n".to_vec())
        .write_to(&mut takeover)
        .unwrap();
    let output = read_until(&mut takeover, b"40 100");
    assert!(
        contains(&output, b"40 100"),
        "{}",
        String::from_utf8_lossy(&output)
    );

    AttachFrame::Input(b"sleep 30 & printf 'child=%s\\n' \"$!\"\n".to_vec())
        .write_to(&mut takeover)
        .unwrap();
    let output = read_until(&mut takeover, b"child=");
    let child_pid = parse_child_pid(&output).expect("shell did not report background child PID");

    drop(takeover);
    daemon.client.close_workspace(&workspace.id).unwrap();
    wait_until(
        || !process_exists(child_pid),
        "workspace close left a background process running",
    );
    assert!(daemon.client.snapshot().unwrap().workspaces.is_empty());

    daemon.stop_with_cli();
}

fn wait_for_attach(client: &Client, shell_id: &str) -> Attachment {
    let deadline = Instant::now() + TIMEOUT;
    loop {
        match client.attach(shell_id, false, profile()) {
            Ok(attachment) => return attachment,
            Err(error) if error.to_string().contains("active controller") => {
                assert!(Instant::now() < deadline, "old attachment was not released");
                thread::sleep(Duration::from_millis(20));
            }
            Err(error) => panic!("could not attach: {error}"),
        }
    }
}

fn profile() -> TerminalProfile {
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

fn read_until(stream: &mut UnixStream, needle: &[u8]) -> Vec<u8> {
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

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}

fn parse_child_pid(output: &[u8]) -> Option<libc::pid_t> {
    let output = String::from_utf8_lossy(output);
    let value = output.rsplit_once("child=")?.1;
    let digits = value
        .trim_start_matches(|character: char| !character.is_ascii_digit())
        .chars()
        .take_while(char::is_ascii_digit)
        .collect::<String>();
    digits.parse().ok()
}

fn process_exists(pid: libc::pid_t) -> bool {
    // Signal zero performs existence and permission checks without changing the process.
    unsafe { libc::kill(pid, 0) == 0 }
}

fn wait_until(mut condition: impl FnMut() -> bool, message: &str) {
    let deadline = Instant::now() + TIMEOUT;
    while Instant::now() < deadline {
        if condition() {
            return;
        }
        thread::sleep(Duration::from_millis(20));
    }
    panic!("{message}");
}

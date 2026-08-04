use std::fs::{self, OpenOptions};
use std::io::{self, IoSlice, Read, Write};
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use boomux::client::{Attachment, Client};
use boomux::protocol::{self, AttachFrame, ShellSpec, ShellStatus, TerminalProfile};
use portable_pty::{CommandBuilder, PtySize, native_pty_system};
use uuid::Uuid;

const TIMEOUT: Duration = Duration::from_secs(5);
const HANDOFF_CHANNEL_FD: RawFd = 198;

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
            .env("XDG_STATE_HOME", runtime_dir.join("state"))
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
        command.env("XDG_STATE_HOME", self.runtime_dir.join("state"));
        command
    }

    fn restart(&mut self) {
        assert!(self.child.is_none());
        let child = self
            .command()
            .args(["daemon", "run"])
            .env("SHELL", "/bin/sh")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        self.child = Some(child);
        wait_until(
            || self.client.ping().is_ok(),
            "restarted daemon did not accept requests",
        );
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

#[test]
fn replacement_bootstrap_rejects_invalid_inherited_descriptor() {
    let output = Command::new(env!("CARGO_BIN_EXE_boomux"))
        .args(["daemon", "receive-handoff", "--channel", "999999"])
        .output()
        .unwrap();

    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("Bad file descriptor"));
}

#[test]
fn replacement_bootstrap_receives_listener_and_lock_ownership() {
    let executable = PathBuf::from(env!("CARGO_BIN_EXE_boomux"));
    let root = std::env::temp_dir().join(format!(
        "boomux-handoff-{}-{}",
        std::process::id(),
        Uuid::new_v4()
    ));
    let runtime_home = root.join("runtime");
    let state_home = root.join("state");
    let runtime_directory = runtime_home.join("boomux");
    let state_directory = state_home.join("boomux");
    fs::create_dir_all(&runtime_directory).unwrap();
    fs::create_dir_all(&state_directory).unwrap();
    let socket_path = runtime_directory.join("daemon.sock");
    let listener = std::os::unix::net::UnixListener::bind(&socket_path).unwrap();
    let runtime_lock = locked_file(&runtime_directory.join("daemon.lock"));
    let state_lock = locked_file(&state_directory.join("daemon.lock"));
    let (mut parent_channel, child_channel) = UnixStream::pair().unwrap();
    let child_channel_fd = child_channel.as_raw_fd();
    let mut command = Command::new(executable);
    command
        .args([
            "daemon",
            "receive-handoff",
            "--channel",
            &HANDOFF_CHANNEL_FD.to_string(),
        ])
        .env("XDG_RUNTIME_DIR", &runtime_home)
        .env("XDG_STATE_HOME", &state_home)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    // The child duplicates its socketpair endpoint to the explicit bootstrap fd
    // immediately before exec and clears close-on-exec on that duplicate.
    unsafe {
        command.pre_exec(move || {
            if libc::dup2(child_channel_fd, HANDOFF_CHANNEL_FD) == -1
                || libc::fcntl(HANDOFF_CHANNEL_FD, libc::F_SETFD, 0) == -1
            {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut replacement = command.spawn().unwrap();
    drop(child_channel);

    parent_channel.set_read_timeout(Some(TIMEOUT)).unwrap();
    parent_channel.set_write_timeout(Some(TIMEOUT)).unwrap();
    parent_channel.write_all(b"BOOMUXH2").unwrap();
    protocol::write_message(&mut parent_channel, &serde_json::json!({ "runtimes": [] })).unwrap();
    send_descriptor(&parent_channel, listener.as_raw_fd(), 1);
    send_descriptor(&parent_channel, runtime_lock.as_raw_fd(), 2);
    send_descriptor(&parent_channel, state_lock.as_raw_fd(), 3);
    let mut ready = [0];
    parent_channel.read_exact(&mut ready).unwrap();
    assert_eq!(ready, [4]);

    drop(listener);
    drop(runtime_lock);
    drop(state_lock);
    let runtime_contender = open_lock(&runtime_directory.join("daemon.lock"));
    let state_contender = open_lock(&state_directory.join("daemon.lock"));
    assert_lock_is_held(&runtime_contender);
    assert_lock_is_held(&state_contender);
    UnixStream::connect(&socket_path).unwrap();

    parent_channel.write_all(&[5]).unwrap();
    wait_until(
        || replacement.try_wait().unwrap().is_some(),
        "replacement did not abort",
    );
    assert!(replacement.wait().unwrap().success());
    assert_eq!(
        unsafe { libc::flock(runtime_contender.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
        0
    );
    assert_eq!(
        unsafe { libc::flock(state_contender.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
        0
    );
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn native_daemon_recovers_reproducible_metadata_after_restart() {
    let mut daemon = TestDaemon::start();
    let workspace = daemon
        .client
        .create_workspace(
            "persistent",
            vec![ShellSpec {
                name: "restored".into(),
                command: vec![
                    "/bin/sh".into(),
                    "-c".into(),
                    "printf 'restored-command\\n'; sleep 30".into(),
                ],
                cwd: std::env::temp_dir(),
            }],
        )
        .unwrap();
    let shell = workspace.shells.first().unwrap();
    let workspace_id = workspace.id.clone();
    let shell_id = shell.id.clone();
    let mut first = daemon
        .client
        .attach(&shell_id, false, profile())
        .unwrap()
        .stream;
    assert!(contains(
        &read_until(&mut first, b"restored-command"),
        b"restored-command"
    ));
    drop(first);
    daemon
        .client
        .rename_workspace(&workspace_id, "persistent-renamed")
        .unwrap();
    daemon
        .client
        .rename_shell(&shell_id, "restored-renamed")
        .unwrap();
    let persisted: serde_json::Value = serde_json::from_slice(
        &fs::read(daemon.runtime_dir.join("state/boomux/state.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(
        persisted["workspaces"][0]["shells"][0]["last_profile"]["term"],
        "attachment-term"
    );

    daemon.stop_with_cli();
    daemon.restart();

    let restored = daemon.client.get_workspace(&workspace_id).unwrap();
    assert_eq!(restored.name, "persistent-renamed");
    assert_eq!(restored.shells.len(), 1);
    assert_eq!(restored.shells[0].id, shell_id);
    assert_eq!(restored.shells[0].name, "restored-renamed");
    assert_eq!(restored.shells[0].status, ShellStatus::Pending);
    let mut second = daemon
        .client
        .attach(&shell_id, false, profile())
        .unwrap()
        .stream;
    assert!(contains(
        &read_until(&mut second, b"restored-command"),
        b"restored-command"
    ));
    drop(second);
    daemon.stop_with_cli();
}

#[test]
fn native_daemon_handoffs_multiple_detached_shells() {
    let mut daemon = TestDaemon::start();
    let workspace = daemon
        .client
        .create_workspace(
            "multiple-live",
            vec![
                ShellSpec::login("first", std::env::temp_dir()),
                ShellSpec::login("second", std::env::temp_dir()),
            ],
        )
        .unwrap();
    let mut pids = Vec::new();
    for (index, shell) in workspace.shells.iter().enumerate() {
        let mut attachment = daemon
            .client
            .attach(&shell.id, false, profile())
            .unwrap()
            .stream;
        AttachFrame::Input(b"stty -echo\n".to_vec())
            .write_to(&mut attachment)
            .unwrap();
        thread::sleep(Duration::from_millis(50));
        let command = format!("printf 'pid{index}=%s:end\\n' \"$$\"\n");
        AttachFrame::Input(command.into_bytes())
            .write_to(&mut attachment)
            .unwrap();
        let output = read_until(&mut attachment, b":end");
        pids.push(parse_pid(&output, &format!("pid{index}=")).unwrap());
    }

    let restart = daemon
        .command()
        .args(["daemon", "restart"])
        .output()
        .unwrap();
    assert!(
        restart.status.success(),
        "multi-runtime restart failed: {}",
        String::from_utf8_lossy(&restart.stderr)
    );

    for (index, shell) in workspace.shells.iter().enumerate() {
        let mut attachment = daemon
            .client
            .attach(&shell.id, false, profile())
            .unwrap()
            .stream;
        let command = format!("printf 'after{index}=%s:end\\n' \"$$\"\n");
        AttachFrame::Input(command.into_bytes())
            .write_to(&mut attachment)
            .unwrap();
        let output = read_until(&mut attachment, b":end");
        assert_eq!(
            parse_pid(&output, &format!("after{index}=")),
            Some(pids[index])
        );
    }
    daemon.client.close_workspace(&workspace.id).unwrap();
    daemon.stop_with_cli();
}

#[test]
fn attachment_client_reconnects_across_daemon_restart() {
    let mut daemon = TestDaemon::start();
    let workspace = daemon
        .client
        .create_workspace(
            "attached-live",
            vec![ShellSpec::login("attached", std::env::temp_dir())],
        )
        .unwrap();
    let shell_id = workspace.shells[0].id.clone();
    let pty = native_pty_system()
        .openpty(PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 800,
            pixel_height: 600,
        })
        .unwrap();
    let descriptor = pty.master.as_raw_fd().unwrap();
    let flags = unsafe { libc::fcntl(descriptor, libc::F_GETFL) };
    assert_ne!(flags, -1);
    assert_ne!(
        unsafe { libc::fcntl(descriptor, libc::F_SETFL, flags | libc::O_NONBLOCK) },
        -1
    );
    let mut command = CommandBuilder::new(&daemon.executable);
    command.args(["__attach", &shell_id]);
    command.env("XDG_RUNTIME_DIR", &daemon.runtime_dir);
    command.env("XDG_STATE_HOME", daemon.runtime_dir.join("state"));
    command.env("TERM", "attachment-term");
    command.env("COLORTERM", "truecolor");
    command.env("TERM_PROGRAM", "integration-terminal");
    command.env("TERM_PROGRAM_VERSION", "1.0");
    let mut attachment_process = pty.slave.spawn_command(command).unwrap();
    drop(pty.slave);
    let mut reader = pty.master.try_clone_reader().unwrap();
    let mut writer = pty.master.take_writer().unwrap();
    read_raw_until(reader.as_mut(), b"$ ");
    writer.write_all(b"stty -echo\n").unwrap();
    thread::sleep(Duration::from_millis(50));
    writer
        .write_all(b"printf 'before-live-restart\\n'\n")
        .unwrap();
    assert!(contains(
        &read_raw_until(reader.as_mut(), b"before-live-restart"),
        b"before-live-restart"
    ));

    let restart = daemon
        .command()
        .args(["daemon", "restart"])
        .output()
        .unwrap();
    assert!(
        restart.status.success(),
        "attached client restart failed: {}",
        String::from_utf8_lossy(&restart.stderr)
    );
    writer
        .write_all(b"printf 'after-live-restart\\n'\n")
        .unwrap();
    assert!(contains(
        &read_raw_until(reader.as_mut(), b"after-live-restart"),
        b"after-live-restart"
    ));

    daemon.client.close_workspace(&workspace.id).unwrap();
    wait_until(
        || attachment_process.try_wait().unwrap().is_some(),
        "attachment client did not exit after shell close",
    );
    daemon.stop_with_cli();
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
    assert!(String::from_utf8_lossy(&status.stdout).contains("running (protocol 6"));

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

    let second_runtime = daemon.runtime_dir.join("second-runtime");
    fs::create_dir(&second_runtime).unwrap();
    let mut duplicate_state = Command::new(&daemon.executable)
        .args(["daemon", "run"])
        .env("XDG_RUNTIME_DIR", &second_runtime)
        .env("XDG_STATE_HOME", daemon.runtime_dir.join("state"))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    wait_until(
        || duplicate_state.try_wait().unwrap().is_some(),
        "second daemon did not reject the held state lock",
    );
    assert!(!duplicate_state.wait().unwrap().success());

    let pending_workspace = daemon
        .client
        .create_workspace(
            "handoff-pending",
            vec![ShellSpec::login("pending", std::env::temp_dir())],
        )
        .unwrap();
    let state_path = daemon.runtime_dir.join("state/boomux/state.json");
    let valid_state = fs::read(&state_path).unwrap();
    fs::write(&state_path, b"invalid replacement state").unwrap();
    let failed_restart = daemon
        .command()
        .args(["daemon", "restart"])
        .output()
        .unwrap();
    assert!(!failed_restart.status.success());
    assert!(daemon.client.ping().is_ok());
    assert!(daemon.client.socket_path().exists());
    fs::write(&state_path, valid_state).unwrap();

    let old_daemon_pid = daemon.child.as_ref().unwrap().id();
    let restart = daemon
        .command()
        .args(["daemon", "restart"])
        .output()
        .unwrap();
    assert!(
        restart.status.success(),
        "daemon restart failed: {}",
        String::from_utf8_lossy(&restart.stderr)
    );
    assert!(String::from_utf8_lossy(&restart.stdout).contains("Restarted Boomux daemon"));
    wait_until(
        || daemon.child.as_mut().unwrap().try_wait().unwrap().is_some(),
        "old daemon did not exit after handoff",
    );
    assert!(!process_exists(old_daemon_pid as libc::pid_t));
    let restored_pending = daemon.client.get_workspace(&pending_workspace.id).unwrap();
    assert_eq!(
        restored_pending.shells[0].id,
        pending_workspace.shells[0].id
    );
    assert_eq!(restored_pending.shells[0].status, ShellStatus::Pending);
    daemon
        .client
        .close_workspace(&pending_workspace.id)
        .unwrap();

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
    assert!(attachment.reconstruction.len() <= 1024 * 1024);
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
    let state_path = daemon.runtime_dir.join("state/boomux/state.json");
    let valid_state = fs::read(&state_path).unwrap();
    fs::write(&state_path, b"invalid active replacement state").unwrap();
    let mut failed_restart_command = daemon.command();
    let failed_restart = failed_restart_command
        .args(["daemon", "restart"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    acknowledge_reconnect(&mut first);
    let failed_restart = failed_restart.wait_with_output().unwrap();
    assert!(!failed_restart.status.success());
    fs::write(&state_path, valid_state).unwrap();
    let mut first = wait_for_attach_with_profile(&daemon.client, &shell_id, profile()).stream;
    AttachFrame::Input(b"printf 'active-rollback-ok\\n'\n".to_vec())
        .write_to(&mut first)
        .unwrap();
    assert!(contains(
        &read_until(&mut first, b"active-rollback-ok"),
        b"active-rollback-ok"
    ));

    let mut restart_command = daemon.command();
    let restart = restart_command
        .args(["daemon", "restart"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    acknowledge_reconnect(&mut first);
    let reconnected = wait_for_attach_with_profile(&daemon.client, &shell_id, profile());
    let mut first = reconnected.stream;
    let restart = restart.wait_with_output().unwrap();
    assert!(
        restart.status.success(),
        "active-controller restart failed: {}",
        String::from_utf8_lossy(&restart.stderr)
    );
    AttachFrame::Input(b"printf 'still-running\\n'\n".to_vec())
        .write_to(&mut first)
        .unwrap();
    assert!(contains(
        &read_until(&mut first, b"still-running"),
        b"still-running"
    ));
    AttachFrame::Input(
        b"printf 'G101MjtjO1kyeHBjR0p2WVhKawc=' | base64 -d; printf 'progress\rsettled\n'\n"
            .to_vec(),
    )
    .write_to(&mut first)
    .unwrap();
    let output = read_until(&mut first, b"settled");
    assert!(
        contains(&output, b"\x1b]52;c;Y2xpcGJvYXJk\x07"),
        "{output:?}"
    );
    let rendered = daemon.client.read_shell(&shell_id, 1024).unwrap();
    assert!(contains(&rendered, b"settled"));
    assert!(!rendered.contains(&b'\x1b'));
    assert!(!contains(&rendered, b"Y2xpcGJvYXJk"));
    let mut resized_profile = profile();
    resized_profile.rows = 40;
    resized_profile.cols = 100;
    resized_profile.pixel_width = 1200;
    resized_profile.pixel_height = 800;
    AttachFrame::Resize {
        rows: resized_profile.rows,
        cols: resized_profile.cols,
        pixel_width: resized_profile.pixel_width,
        pixel_height: resized_profile.pixel_height,
    }
    .write_to(&mut first)
    .unwrap();
    AttachFrame::Input(b"printf 'shellpid=%s:end\\n' \"$$\"\n".to_vec())
        .write_to(&mut first)
        .unwrap();
    let output = read_until(&mut first, b":end");
    let shell_pid = parse_pid(&output, "shellpid=").expect("shell did not report its PID");

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
    let state_path = daemon.runtime_dir.join("state/boomux/state.json");
    let valid_state = fs::read(&state_path).unwrap();
    fs::write(&state_path, b"invalid live replacement state").unwrap();
    let failed_live_restart = daemon
        .command()
        .args(["daemon", "restart"])
        .output()
        .unwrap();
    assert!(!failed_live_restart.status.success());
    fs::write(&state_path, valid_state).unwrap();
    let mut rollback_attachment =
        wait_for_attach_with_profile(&daemon.client, &shell_id, resized_profile.clone()).stream;
    AttachFrame::Input(b"printf 'rollback-ok\\n'\n".to_vec())
        .write_to(&mut rollback_attachment)
        .unwrap();
    assert!(contains(
        &read_until(&mut rollback_attachment, b"rollback-ok"),
        b"rollback-ok"
    ));
    drop(rollback_attachment);

    let restart = daemon
        .command()
        .args(["daemon", "restart"])
        .output()
        .unwrap();
    assert!(
        restart.status.success(),
        "live daemon restart failed: {}",
        String::from_utf8_lossy(&restart.stderr)
    );
    assert_eq!(
        daemon.client.get_shell(&shell_id).unwrap().status,
        ShellStatus::Running
    );
    let repeated_restart = daemon
        .command()
        .args(["daemon", "restart"])
        .output()
        .unwrap();
    assert!(
        repeated_restart.status.success(),
        "repeated live daemon restart failed: {}",
        String::from_utf8_lossy(&repeated_restart.stderr)
    );

    let second_attachment =
        wait_for_attach_with_profile(&daemon.client, &shell_id, resized_profile);
    assert!(!contains(
        &second_attachment.reconstruction,
        b"\x1b]52;c;Y2xpcGJvYXJk\x07"
    ));
    assert!(!contains(
        &second_attachment.reconstruction,
        b"Y2xpcGJvYXJk"
    ));
    assert!(contains(&second_attachment.reconstruction, b"settled"));
    let mut second = second_attachment.stream;
    AttachFrame::Input(b"stty size\n".to_vec())
        .write_to(&mut second)
        .unwrap();
    assert!(contains(&read_until(&mut second, b"40 100"), b"40 100"));
    AttachFrame::Input(b"printf 'shellpid2=%s:end\\n' \"$$\"\n".to_vec())
        .write_to(&mut second)
        .unwrap();
    let output = read_until(&mut second, b":end");
    assert_eq!(parse_pid(&output, "shellpid2="), Some(shell_pid));
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

    AttachFrame::Input(
        b"/bin/sh -c 'trap \"\" HUP TERM; sleep 30' & printf 'child=%s\\n' \"$!\"; exit\n".to_vec(),
    )
    .write_to(&mut takeover)
    .unwrap();
    let output = read_until(&mut takeover, b"child=");
    let child_pid = parse_child_pid(&output).expect("shell did not report background child PID");

    drop(takeover);
    wait_until(
        || {
            matches!(
                daemon.client.get_shell(&shell_id).unwrap().status,
                ShellStatus::Exited { .. }
            )
        },
        "imported shell leader exit was not detected",
    );
    daemon.client.close_workspace(&workspace.id).unwrap();
    wait_until(
        || !process_exists(child_pid),
        "workspace close left a background process running",
    );
    assert!(daemon.client.snapshot().unwrap().workspaces.is_empty());

    daemon.stop_with_cli();
}

fn wait_for_attach_with_profile(
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

fn acknowledge_reconnect(stream: &mut UnixStream) {
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

fn open_lock(path: &std::path::Path) -> std::fs::File {
    OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(path)
        .unwrap()
}

fn locked_file(path: &std::path::Path) -> std::fs::File {
    let file = open_lock(path);
    assert_eq!(
        unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
        0
    );
    file
}

fn assert_lock_is_held(file: &std::fs::File) {
    assert_eq!(
        unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
        -1
    );
    assert_eq!(
        io::Error::last_os_error().raw_os_error(),
        Some(libc::EWOULDBLOCK)
    );
}

fn send_descriptor(stream: &UnixStream, descriptor: RawFd, marker: u8) {
    use nix::sys::socket::{ControlMessage, MsgFlags, sendmsg};

    let marker = [marker];
    let data = [IoSlice::new(&marker)];
    let descriptors = [descriptor];
    let control = [ControlMessage::ScmRights(&descriptors)];
    assert_eq!(
        sendmsg::<()>(
            stream.as_raw_fd(),
            &data,
            &control,
            MsgFlags::MSG_NOSIGNAL,
            None,
        )
        .unwrap(),
        1
    );
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

fn read_raw_until(reader: &mut dyn Read, needle: &[u8]) -> Vec<u8> {
    let deadline = Instant::now() + TIMEOUT;
    let mut output = Vec::new();
    let mut buffer = [0; 16 * 1024];
    while Instant::now() < deadline {
        match reader.read(&mut buffer) {
            Ok(0) => break,
            Ok(count) => {
                output.extend_from_slice(&buffer[..count]);
                if contains(&output, needle) {
                    return output;
                }
            }
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                ) =>
            {
                thread::sleep(Duration::from_millis(10));
            }
            Err(error) => panic!("PTY read failed: {error}"),
        }
    }
    panic!(
        "did not receive {:?}; PTY output was {:?}",
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
    parse_pid(output, "child=")
}

fn parse_pid(output: &[u8], label: &str) -> Option<libc::pid_t> {
    let output = String::from_utf8_lossy(output);
    let value = output.rsplit_once(label)?.1;
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

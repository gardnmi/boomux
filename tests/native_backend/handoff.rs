use std::fs::{self, OpenOptions};
use std::io::{self, IoSlice, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use boomux::protocol::{
    self, AgentAuthority, AgentRegistrationSpec, AgentReport, AgentState, AttachFrame, ErrorCode,
    ShellSpec, ShellStatus,
};
use portable_pty::{CommandBuilder, PtySize, native_pty_system};
use uuid::Uuid;

use crate::support::{
    TIMEOUT, TestDaemon, acknowledge_reconnect, assert_remote_code, contains,
    ensure_test_opencode_runtime, parse_pid, process_exists, profile, read_until,
    wait_for_attach_with_profile, wait_until,
};

const HANDOFF_CHANNEL_FD: RawFd = 198;

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
    parent_channel.write_all(b"BOOMUXH5").unwrap();
    protocol::write_message(
        &mut parent_channel,
        &serde_json::json!({
            "runtimes": [],
            "exited": [],
            "event_stream": {
                "stream_id": Uuid::new_v4().to_string(),
                "latest_id": 0,
                "events": []
            }
        }),
    )
    .unwrap();
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
fn focused_attachment_is_exposed_in_daemon_snapshots() {
    let mut daemon = TestDaemon::start();
    let workspace = daemon
        .client
        .create_workspace(
            "focused-terminal",
            vec![ShellSpec::login("shell", std::env::temp_dir())],
        )
        .unwrap();
    let shell_id = workspace.shells[0].id.clone();
    let mut attachment = daemon.client.attach(&shell_id, false, profile()).unwrap();
    assert_eq!(attachment.protocol_version, protocol::PROTOCOL_VERSION);
    let event_cursor = daemon.client.events(None, 256, 0).unwrap().cursor;

    AttachFrame::FocusGained
        .write_to(&mut attachment.stream)
        .unwrap();
    let focus_events = daemon
        .client
        .events(Some(event_cursor), 256, 1_000)
        .unwrap();
    assert!(focus_events.events.iter().any(|event| matches!(
        event.kind,
        protocol::DaemonEventKind::FocusedTerminalPresentationChanged
    )));
    wait_until(
        || {
            daemon
                .client
                .snapshot()
                .unwrap()
                .focused_terminal
                .is_some_and(|focused| {
                    focused.revision == 1
                        && focused.workspace_id == workspace.id
                        && focused.shell_id == shell_id
                })
        },
        "daemon did not expose the focused attachment",
    );

    AttachFrame::FocusGained
        .write_to(&mut attachment.stream)
        .unwrap();
    wait_until(
        || {
            daemon
                .client
                .snapshot()
                .unwrap()
                .focused_terminal
                .is_some_and(|focused| focused.revision == 2)
        },
        "repeated focus did not advance the revision",
    );
    let restart = daemon
        .command()
        .args(["daemon", "restart"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    acknowledge_reconnect(&mut attachment.stream);
    let restart = restart.wait_with_output().unwrap();
    assert!(
        restart.status.success(),
        "daemon restart failed: {}",
        String::from_utf8_lossy(&restart.stderr)
    );
    wait_until(
        || {
            daemon
                .client
                .snapshot()
                .unwrap()
                .focused_terminal
                .is_some_and(|focused| focused.revision == 2 && focused.shell_id == shell_id)
        },
        "graceful handoff did not retain focused terminal state",
    );
    let mut reattached = wait_for_attach_with_profile(&daemon.client, &shell_id, profile());
    AttachFrame::FocusGained
        .write_to(&mut reattached.stream)
        .unwrap();
    wait_until(
        || {
            daemon
                .client
                .snapshot()
                .unwrap()
                .focused_terminal
                .is_some_and(|focused| focused.revision == 3)
        },
        "focus revision did not continue after graceful handoff",
    );
    drop(reattached);
    drop(attachment);
    daemon.stop_with_cli();
}

#[test]
fn graceful_restart_preserves_exited_run_and_terminal_state() {
    let mut daemon = TestDaemon::start();
    let workspace = daemon
        .client
        .create_workspace(
            "exited-handoff",
            vec![
                ShellSpec::login("finished", std::env::temp_dir()),
                ShellSpec::login("live", std::env::temp_dir()),
            ],
        )
        .unwrap();
    let shell_id = workspace.shells[0].id.clone();
    let live_shell_id = workspace.shells[1].id.clone();
    let mut live = daemon
        .client
        .attach(&live_shell_id, false, profile())
        .unwrap()
        .stream;
    AttachFrame::Input(b"printf 'live-during-exited-handoff\\n'\n".to_vec())
        .write_to(&mut live)
        .unwrap();
    assert!(contains(
        &read_until(&mut live, b"live-during-exited-handoff"),
        b"live-during-exited-handoff"
    ));
    drop(live);
    let mut attachment = daemon
        .client
        .attach(&shell_id, false, profile())
        .unwrap()
        .stream;
    AttachFrame::Input(b"printf 'final-exited-output\\n'; exit 7\n".to_vec())
        .write_to(&mut attachment)
        .unwrap();
    assert!(contains(
        &read_until(&mut attachment, b"final-exited-output"),
        b"final-exited-output"
    ));
    drop(attachment);
    wait_until(
        || {
            matches!(
                daemon.client.get_shell(&shell_id).unwrap().status,
                ShellStatus::Exited { code: Some(7) }
            )
        },
        "shell did not record its final exit",
    );
    let before = daemon.client.get_shell(&shell_id).unwrap();
    let before_run = before.run.clone().unwrap();
    let before_output = daemon.client.read_shell(&shell_id, 1024 * 1024).unwrap();
    assert!(contains(&before_output, b"final-exited-output"));

    let restart = daemon
        .command()
        .args(["daemon", "restart"])
        .output()
        .unwrap();
    assert!(
        restart.status.success(),
        "exited-shell restart failed: {}",
        String::from_utf8_lossy(&restart.stderr)
    );

    let after = daemon.client.get_shell(&shell_id).unwrap();
    assert_eq!(after.status, ShellStatus::Exited { code: Some(7) });
    assert_eq!(after.run.as_ref(), Some(&before_run));
    assert_eq!(
        daemon.client.get_shell(&live_shell_id).unwrap().status,
        ShellStatus::Running
    );
    let after_output = daemon.client.read_shell(&shell_id, 1024 * 1024).unwrap();
    assert_eq!(after_output, before_output);
    let mut restored = daemon.client.attach(&shell_id, false, profile()).unwrap();
    assert!(contains(&restored.reconstruction, b"final-exited-output"));
    assert!(matches!(
        AttachFrame::read_from(&mut restored.stream).unwrap(),
        AttachFrame::Detached
    ));
    assert_eq!(
        daemon.client.get_shell(&shell_id).unwrap().run.unwrap().id,
        before_run.id
    );

    let state_directory = daemon.runtime_dir.join("state/boomux");
    let saved_directory = daemon.runtime_dir.join("saved-exited-handoff-state");
    fs::rename(&state_directory, &saved_directory).unwrap();
    fs::write(&state_directory, b"not a directory").unwrap();
    let error = daemon.client.close_shell(&shell_id).unwrap_err();
    assert_remote_code(&error, ErrorCode::PersistenceFailed);
    let rolled_back = daemon.client.get_shell(&shell_id).unwrap();
    assert_eq!(rolled_back.status, ShellStatus::Exited { code: Some(7) });
    assert_eq!(rolled_back.run.as_ref(), Some(&before_run));
    assert!(contains(
        &daemon.client.read_shell(&shell_id, 1024 * 1024).unwrap(),
        b"final-exited-output"
    ));
    fs::remove_file(&state_directory).unwrap();
    fs::rename(&saved_directory, &state_directory).unwrap();

    daemon.client.close_workspace(&workspace.id).unwrap();
    daemon.stop_with_cli();
}

#[test]
fn graceful_restart_preserves_opencode_shared_runtime_and_stop_cleans_it_up() {
    let mut daemon = TestDaemon::start_with(|command, runtime_dir| {
        let bin = runtime_dir.join("bin");
        fs::create_dir(&bin).unwrap();
        let opencode = bin.join("opencode");
        fs::write(
            &opencode,
            "#!/bin/sh\nexec python3 -c 'import socket,sys,time; assert sys.argv[1:4] == [\"serve\", \"--hostname\", \"127.0.0.1\"]; assert sys.argv[4] == \"--port\"; s=socket.socket(); s.bind((\"127.0.0.1\", int(sys.argv[5]))); s.listen(); time.sleep(60)' \"$@\"\n",
        )
        .unwrap();
        fs::set_permissions(&opencode, fs::Permissions::from_mode(0o700)).unwrap();
        command.env(
            "PATH",
            format!("{}:{}", bin.display(), std::env::var("PATH").unwrap()),
        );
    });
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    let before = ensure_test_opencode_runtime(&daemon, port).unwrap();
    let pid = before.pid.unwrap() as libc::pid_t;
    let workspace = daemon
        .client
        .create_workspace(
            "opencode-handoff",
            vec![ShellSpec::login("claim", std::env::temp_dir())],
        )
        .unwrap();
    let shell_id = workspace.shells[0].id.clone();
    let attachment = daemon.client.attach(&shell_id, false, profile()).unwrap();
    let run_id = daemon.client.get_shell(&shell_id).unwrap().run.unwrap().id;
    let root_session_id = "ses_handoff_claim";
    daemon
        .client
        .ensure_opencode_session_claim(
            &before.generation_id,
            Uuid::new_v4().to_string(),
            root_session_id,
            &shell_id,
            &run_id,
            AgentRegistrationSpec {
                name: "handoff-agent".into(),
                integration: "opencode".into(),
                external_session_id: Some(root_session_id.into()),
                report: AgentReport {
                    state: AgentState::Working,
                    authority: AgentAuthority::LifecycleIntegration,
                    evidence: "handoff claim".into(),
                    confidence: 100,
                },
            },
        )
        .unwrap();
    drop(attachment);

    let state_path = daemon.runtime_dir.join("state/boomux/state.json");
    let valid_state = fs::read(&state_path).unwrap();
    fs::write(&state_path, b"invalid OpenCode handoff state").unwrap();
    let failed = daemon
        .command()
        .args(["daemon", "restart"])
        .output()
        .unwrap();
    assert!(!failed.status.success());
    fs::write(&state_path, valid_state).unwrap();
    assert_eq!(
        daemon.client.get_opencode_shared_runtime().unwrap(),
        Some(before.clone())
    );
    daemon
        .client
        .resolve_opencode_session_claim(&before.generation_id, root_session_id)
        .unwrap();
    assert!(process_exists(pid));

    daemon.client.restart().unwrap();

    let after = daemon
        .client
        .get_opencode_shared_runtime()
        .unwrap()
        .expect("shared runtime was not transferred");
    assert_eq!(after, before);
    assert!(process_exists(pid));
    assert!(TcpStream::connect(("127.0.0.1", port)).is_ok());
    let cleared = daemon
        .client
        .resolve_opencode_session_claim(&before.generation_id, root_session_id)
        .unwrap_err();
    assert_remote_code(&cleared, ErrorCode::NotFound);
    daemon.stop_with_cli();
    wait_until(
        || !process_exists(pid),
        "transferred OpenCode runtime survived daemon stop",
    );
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
    let mut run_ids = Vec::new();
    let mut output_revisions = Vec::new();
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
        let run = daemon.client.get_shell(&shell.id).unwrap().run.unwrap();
        run_ids.push(run.id);
        output_revisions.push(run.output_revision);
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
        let transferred_run = daemon.client.get_shell(&shell.id).unwrap().run.unwrap();
        assert_eq!(transferred_run.id, run_ids[index]);
        assert_eq!(transferred_run.output_revision, output_revisions[index]);
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
    command.env("SHELL", "/bin/sh");
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

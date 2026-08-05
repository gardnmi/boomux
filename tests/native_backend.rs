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

use boomux::client::{Attachment, Client, RemoteError};
use boomux::protocol::{
    self, AgentAuthority, AgentRegistrationSpec, AgentReport, AgentState, AttachFrame, ErrorCode,
    ShellRunExitReason, ShellSpec, ShellStatus, TerminalProfile, WorkspaceLauncherSpec,
};
use portable_pty::{CommandBuilder, PtySize, native_pty_system};
use uuid::Uuid;

const TIMEOUT: Duration = Duration::from_secs(10);
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

    fn crash(&mut self) {
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
fn json_cli_parse_and_daemon_start_failures_use_typed_envelopes() {
    let parse_failure = Command::new(env!("CARGO_BIN_EXE_boomux"))
        .args(["--json", "read"])
        .output()
        .unwrap();
    assert!(!parse_failure.status.success());
    assert!(parse_failure.stdout.is_empty());
    let parse_failure: serde_json::Value = serde_json::from_slice(&parse_failure.stderr).unwrap();
    assert_eq!(parse_failure["schema"], "boomux.cli/v1");
    assert_eq!(parse_failure["command"], "cli");
    assert_eq!(parse_failure["error"]["code"], "invalid_argument");
    let cursor_failure = Command::new(env!("CARGO_BIN_EXE_boomux"))
        .args(["events", "--after", "invalid", "--json"])
        .output()
        .unwrap();
    assert!(!cursor_failure.status.success());
    let cursor_failure: serde_json::Value = serde_json::from_slice(&cursor_failure.stderr).unwrap();
    assert_eq!(cursor_failure["command"], "events");
    assert_eq!(cursor_failure["error"]["code"], "invalid_argument");

    let root = std::env::temp_dir().join(format!("boomux-json-start-{}", Uuid::new_v4()));
    fs::create_dir(&root).unwrap();
    let invalid_state_home = root.join("state-file");
    fs::write(&invalid_state_home, b"not a directory").unwrap();
    let unavailable = Command::new(env!("CARGO_BIN_EXE_boomux"))
        .args(["list", "--json"])
        .env("XDG_RUNTIME_DIR", &root)
        .env("XDG_STATE_HOME", &invalid_state_home)
        .output()
        .unwrap();
    assert!(!unavailable.status.success());
    assert!(unavailable.stdout.is_empty());
    let unavailable: serde_json::Value = serde_json::from_slice(&unavailable.stderr).unwrap();
    assert_eq!(unavailable["command"], "list");
    assert_eq!(unavailable["error"]["code"], "daemon_unavailable");
    fs::remove_dir_all(root).unwrap();
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
    parent_channel.write_all(b"BOOMUXH4").unwrap();
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
    let first_run = daemon
        .client
        .get_shell(&shell_id)
        .unwrap()
        .run
        .expect("started shell has no run identity");
    assert_eq!(first_run.generation, 1);
    assert!(first_run.environment_has_run_id);
    assert!(first_run.ended_at_ms.is_none());
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
        persisted["workspaces"][0]["shells"][0]["last_run"]["profile"]["term"],
        "attachment-term"
    );
    assert_eq!(
        persisted["workspaces"][0]["shells"][0]["last_run"]["id"],
        first_run.id
    );

    daemon.stop_with_cli();
    daemon.restart();

    let restored = daemon.client.get_workspace(&workspace_id).unwrap();
    assert_eq!(restored.name, "persistent-renamed");
    assert_eq!(restored.shells.len(), 1);
    assert_eq!(restored.shells[0].id, shell_id);
    assert_eq!(restored.shells[0].name, "restored-renamed");
    assert_eq!(restored.shells[0].status, ShellStatus::Pending);
    assert!(restored.shells[0].run.is_none());
    let mut second = daemon
        .client
        .attach(&shell_id, false, profile())
        .unwrap()
        .stream;
    assert!(contains(
        &read_until(&mut second, b"restored-command"),
        b"restored-command"
    ));
    let second_run = daemon
        .client
        .get_shell(&shell_id)
        .unwrap()
        .run
        .expect("restarted shell has no run identity");
    assert_ne!(second_run.id, first_run.id);
    assert_eq!(second_run.generation, 2);
    drop(second);
    daemon.stop_with_cli();
}

#[test]
fn native_daemon_marks_a_crashed_run_interrupted() {
    let mut daemon = TestDaemon::start();
    let workspace = daemon
        .client
        .create_workspace(
            "crash-run",
            vec![ShellSpec::login("agent", std::env::temp_dir())],
        )
        .unwrap();
    let shell_id = workspace.shells[0].id.clone();
    let attachment = daemon.client.attach(&shell_id, false, profile()).unwrap();
    let first_run = daemon.client.get_shell(&shell_id).unwrap().run.unwrap();
    drop(attachment.stream);

    daemon.crash();
    daemon.restart();

    let restored = daemon.client.get_shell(&shell_id).unwrap();
    assert_eq!(restored.status, ShellStatus::Pending);
    assert!(restored.run.is_none());
    let persisted: serde_json::Value = serde_json::from_slice(
        &fs::read(daemon.runtime_dir.join("state/boomux/state.json")).unwrap(),
    )
    .unwrap();
    let last_run = &persisted["workspaces"][0]["shells"][0]["last_run"];
    assert_eq!(last_run["id"], first_run.id);
    assert_eq!(last_run["exit_reason"]["reason"], "interrupted");
    assert!(last_run["ended_at_ms"].as_u64().is_some());

    let second = daemon.client.attach(&shell_id, false, profile()).unwrap();
    let second_run = daemon.client.get_shell(&shell_id).unwrap().run.unwrap();
    assert_ne!(second_run.id, first_run.id);
    assert_eq!(second_run.generation, 2);
    drop(second.stream);
    daemon.stop_with_cli();
}

#[test]
fn failed_start_persistence_advances_the_run_generation() {
    let mut daemon = TestDaemon::start();
    let workspace = daemon
        .client
        .create_workspace(
            "failed-run-persistence",
            vec![ShellSpec::login("shell", std::env::temp_dir())],
        )
        .unwrap();
    let shell_id = workspace.shells[0].id.clone();
    let cursor = daemon.client.events(None, 256, 0).unwrap().cursor;
    let state_directory = daemon.runtime_dir.join("state/boomux");
    let saved_directory = daemon.runtime_dir.join("saved-state");
    fs::rename(&state_directory, &saved_directory).unwrap();
    fs::write(&state_directory, b"not a directory").unwrap();

    let error = daemon
        .client
        .attach(&shell_id, false, profile())
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("could not persist started shell")
    );
    let events = daemon.client.events(Some(cursor), 256, 0).unwrap();
    assert!(!events.events.iter().any(|event| matches!(
        event.kind,
        protocol::DaemonEventKind::RunStarted { .. } | protocol::DaemonEventKind::RunExited { .. }
    )));

    fs::remove_file(&state_directory).unwrap();
    fs::rename(&saved_directory, &state_directory).unwrap();
    let attachment = daemon.client.attach(&shell_id, false, profile()).unwrap();
    let run = daemon.client.get_shell(&shell_id).unwrap().run.unwrap();
    assert_eq!(run.generation, 2);
    drop(attachment.stream);
    daemon.client.close_workspace(&workspace.id).unwrap();
    daemon.stop_with_cli();
}

#[test]
fn exited_run_persistence_retries_after_storage_recovers() {
    let mut daemon = TestDaemon::start();
    let workspace = daemon
        .client
        .create_workspace(
            "exit-persistence-retry",
            vec![ShellSpec::login("shell", std::env::temp_dir())],
        )
        .unwrap();
    let shell_id = workspace.shells[0].id.clone();
    let mut attachment = daemon
        .client
        .attach(&shell_id, false, profile())
        .unwrap()
        .stream;
    let run_id = daemon.client.get_shell(&shell_id).unwrap().run.unwrap().id;
    let cursor = daemon.client.events(None, 256, 0).unwrap().cursor;
    let state_directory = daemon.runtime_dir.join("state/boomux");
    let saved_directory = daemon.runtime_dir.join("saved-exit-state");
    fs::rename(&state_directory, &saved_directory).unwrap();
    fs::write(&state_directory, b"not a directory").unwrap();

    AttachFrame::Input(b"exit\n".to_vec())
        .write_to(&mut attachment)
        .unwrap();
    wait_until(
        || {
            matches!(
                daemon.client.get_shell(&shell_id).unwrap().status,
                ShellStatus::Exited { .. }
            )
        },
        "shell did not exit while persistence was unavailable",
    );
    drop(attachment);
    let while_broken = daemon.client.events(Some(cursor.clone()), 256, 0).unwrap();
    assert!(
        !while_broken
            .events
            .iter()
            .any(|event| matches!(event.kind, protocol::DaemonEventKind::RunExited { .. }))
    );
    let error = daemon.client.events(None, 256, 0).unwrap_err();
    assert_eq!(
        error
            .get_ref()
            .and_then(|error| error.downcast_ref::<RemoteError>())
            .and_then(|error| error.code),
        Some(ErrorCode::PersistenceFailed)
    );
    let restart = daemon
        .command()
        .args(["daemon", "restart"])
        .output()
        .unwrap();
    assert!(!restart.status.success());
    assert!(daemon.client.ping().is_ok());
    fs::remove_file(&state_directory).unwrap();
    fs::rename(&saved_directory, &state_directory).unwrap();
    let state_path = state_directory.join("state.json");
    wait_until(
        || {
            let state: serde_json::Value =
                serde_json::from_slice(&fs::read(&state_path).unwrap()).unwrap();
            let run = &state["workspaces"][0]["shells"][0]["last_run"];
            run["id"] == run_id && run["ended_at_ms"].as_u64().is_some()
        },
        "exited run was not persisted after storage recovered",
    );
    wait_until(
        || {
            daemon
                .client
                .events(Some(cursor.clone()), 256, 0)
                .unwrap()
                .events
                .iter()
                .any(|event| {
                    matches!(
                        &event.kind,
                        protocol::DaemonEventKind::RunExited { run, .. } if run.id == run_id
                    )
                })
        },
        "exited run event was not published after storage recovered",
    );
    let events = daemon.client.events(Some(cursor), 256, 0).unwrap();
    assert_eq!(
        events
            .events
            .iter()
            .filter(|event| matches!(
                &event.kind,
                protocol::DaemonEventKind::RunExited { run, .. } if run.id == run_id
            ))
            .count(),
        1
    );

    let saved_directory = daemon.runtime_dir.join("saved-natural-exit-state");
    fs::rename(&state_directory, &saved_directory).unwrap();
    fs::write(&state_directory, b"not a directory").unwrap();
    assert!(daemon.client.close_shell(&shell_id).is_err());
    let restored = daemon.client.get_shell(&shell_id).unwrap();
    assert!(matches!(restored.status, ShellStatus::Exited { .. }));
    assert_eq!(restored.run.unwrap().id, run_id);
    fs::remove_file(&state_directory).unwrap();
    fs::rename(&saved_directory, &state_directory).unwrap();

    daemon.client.close_workspace(&workspace.id).unwrap();
    daemon.stop_with_cli();
}

#[test]
fn failed_close_publishes_terminated_run_after_storage_recovers() {
    let mut daemon = TestDaemon::start();
    let workspace = daemon
        .client
        .create_workspace(
            "failed-close-persistence",
            vec![ShellSpec::login("shell", std::env::temp_dir())],
        )
        .unwrap();
    let shell_id = workspace.shells[0].id.clone();
    let attachment = daemon.client.attach(&shell_id, false, profile()).unwrap();
    let run_id = daemon.client.get_shell(&shell_id).unwrap().run.unwrap().id;
    let cursor = daemon.client.events(None, 256, 0).unwrap().cursor;
    let state_directory = daemon.runtime_dir.join("state/boomux");
    let saved_directory = daemon.runtime_dir.join("saved-close-state");
    fs::rename(&state_directory, &saved_directory).unwrap();
    fs::write(&state_directory, b"not a directory").unwrap();

    assert!(daemon.client.close_shell(&shell_id).is_err());
    drop(attachment);
    let restored = daemon.client.get_shell(&shell_id).unwrap();
    assert_eq!(restored.status, ShellStatus::Pending);
    assert!(restored.run.is_none());
    assert!(
        !daemon
            .client
            .events(Some(cursor.clone()), 256, 0)
            .unwrap()
            .events
            .iter()
            .any(|event| matches!(event.kind, protocol::DaemonEventKind::RunExited { .. }))
    );

    fs::remove_file(&state_directory).unwrap();
    fs::rename(&saved_directory, &state_directory).unwrap();
    wait_until(
        || {
            daemon
                .client
                .events(Some(cursor.clone()), 256, 0)
                .unwrap()
                .events
                .iter()
                .any(|event| {
                    matches!(
                        &event.kind,
                        protocol::DaemonEventKind::RunExited { run, .. }
                            if run.id == run_id
                                && run.exit_reason == Some(ShellRunExitReason::Terminated)
                    )
                })
        },
        "terminated run event was not published after close persistence recovered",
    );
    daemon.client.close_workspace(&workspace.id).unwrap();
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
    assert_eq!(
        error
            .get_ref()
            .and_then(|error| error.downcast_ref::<RemoteError>())
            .and_then(|error| error.code),
        Some(ErrorCode::PersistenceFailed)
    );
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
fn daemon_events_and_revision_reads_survive_handoff() {
    let mut daemon = TestDaemon::start();
    let baseline = daemon.client.events(None, 256, 0).unwrap();
    assert!(baseline.snapshot.is_some());
    assert!(baseline.events.is_empty());
    let stream_id = baseline.stream_id.clone();
    let mut cursor = baseline.cursor;

    let polling_client = daemon.client.clone();
    let polling_cursor = cursor.clone();
    let poll = thread::spawn(move || polling_client.events(Some(polling_cursor), 256, 2_000));
    thread::sleep(Duration::from_millis(50));
    let workspace = daemon
        .client
        .create_workspace(
            "events",
            vec![ShellSpec::login("agent", std::env::temp_dir())],
        )
        .unwrap();
    let shell_id = workspace.shells[0].id.clone();
    let created = poll.join().unwrap().unwrap();
    assert!(created.snapshot.is_none());
    assert!(
        created
            .events
            .windows(2)
            .all(|events| events[0].id < events[1].id)
    );
    assert!(created.events.iter().any(|event| matches!(
        event.kind,
        protocol::DaemonEventKind::WorkspaceCreated { .. }
    )));
    assert!(
        created
            .events
            .iter()
            .any(|event| matches!(event.kind, protocol::DaemonEventKind::ShellCreated { .. }))
    );
    cursor = created.cursor;

    let mut attachment = daemon
        .client
        .attach(&shell_id, false, profile())
        .unwrap()
        .stream;
    AttachFrame::Input(b"printf 'event-output-one\\n'\n".to_vec())
        .write_to(&mut attachment)
        .unwrap();
    assert!(contains(
        &read_until(&mut attachment, b"event-output-one"),
        b"event-output-one"
    ));
    drop(attachment);
    let changed = daemon.client.events(Some(cursor), 256, 1_000).unwrap();
    assert!(
        changed
            .events
            .iter()
            .any(|event| matches!(event.kind, protocol::DaemonEventKind::RunStarted { .. }))
    );
    assert!(
        changed
            .events
            .iter()
            .any(|event| matches!(event.kind, protocol::DaemonEventKind::OutputChanged { .. }))
    );
    cursor = changed.cursor;

    let observed = daemon
        .client
        .read_shell_at(&shell_id, 1024 * 1024, None, None, 0)
        .unwrap();
    assert!(contains(&observed.bytes, b"event-output-one"));
    let run_id = observed.run_id.clone().unwrap();
    let revision = observed.output_revision.unwrap();
    let unchanged = daemon
        .client
        .read_shell_at(
            &shell_id,
            1024 * 1024,
            Some(run_id.clone()),
            Some(revision),
            10,
        )
        .unwrap();
    assert!(!unchanged.changed);
    assert!(unchanged.bytes.is_empty());
    let waiting_client = daemon.client.clone();
    let waiting_shell_id = shell_id.clone();
    let waiting_run_id = run_id.clone();
    let wait = thread::spawn(move || {
        waiting_client.read_shell_at(
            waiting_shell_id,
            1024 * 1024,
            Some(waiting_run_id),
            Some(revision),
            2_000,
        )
    });
    thread::sleep(Duration::from_millis(50));
    let mut attachment = daemon
        .client
        .attach(&shell_id, false, profile())
        .unwrap()
        .stream;
    AttachFrame::Input(b"printf 'event-output-two\\n'\n".to_vec())
        .write_to(&mut attachment)
        .unwrap();
    assert!(contains(
        &read_until(&mut attachment, b"event-output-two"),
        b"event-output-two"
    ));
    drop(attachment);
    let advanced = wait.join().unwrap().unwrap();
    assert!(advanced.changed);
    let advanced_revision = advanced.output_revision.unwrap();
    assert!(advanced_revision > revision);
    assert!(contains(&advanced.bytes, b"event-output-two"));
    let error = daemon
        .client
        .read_shell_at(
            &shell_id,
            1024,
            Some(Uuid::new_v4().to_string()),
            Some(revision),
            0,
        )
        .unwrap_err();
    assert_eq!(
        error
            .get_ref()
            .and_then(|error| error.downcast_ref::<RemoteError>())
            .and_then(|error| error.code),
        Some(ErrorCode::RunChanged)
    );
    let error = daemon
        .client
        .read_shell_at(&shell_id, 1024, Some(run_id.clone()), Some(u64::MAX), 0)
        .unwrap_err();
    assert_eq!(
        error
            .get_ref()
            .and_then(|error| error.downcast_ref::<RemoteError>())
            .and_then(|error| error.code),
        Some(ErrorCode::RevisionAhead)
    );

    let waiting_client = daemon.client.clone();
    let waiting_shell_id = shell_id.clone();
    let wait = thread::spawn(move || {
        waiting_client.read_shell_at(
            waiting_shell_id,
            1024,
            Some(run_id),
            Some(advanced_revision),
            5_000,
        )
    });
    thread::sleep(Duration::from_millis(50));
    let restart = daemon
        .command()
        .args(["daemon", "restart"])
        .output()
        .unwrap();
    assert!(restart.status.success());
    let error = wait.join().unwrap().unwrap_err();
    assert_eq!(
        error
            .get_ref()
            .and_then(|error| error.downcast_ref::<RemoteError>())
            .and_then(|error| error.code),
        Some(ErrorCode::DaemonStopping)
    );
    let handed_off = daemon.client.events(Some(cursor), 256, 1_000).unwrap();
    assert_eq!(handed_off.stream_id, stream_id);
    assert!(
        handed_off
            .events
            .iter()
            .any(|event| matches!(event.kind, protocol::DaemonEventKind::HandoffCompleted))
    );
    cursor = handed_off.cursor;

    let events_cli = daemon
        .command()
        .args([
            "events",
            "--after",
            &format!("{}:{}", cursor.stream_id, cursor.event_id),
            "--json",
        ])
        .output()
        .unwrap();
    assert!(events_cli.status.success());
    let events_cli: serde_json::Value = serde_json::from_slice(&events_cli.stdout).unwrap();
    assert_eq!(events_cli["command"], "events");
    assert_eq!(events_cli["data"]["stream_id"], stream_id);

    daemon.stop_with_cli();
    daemon.restart();
    let error = daemon.client.events(Some(cursor), 256, 0).unwrap_err();
    assert_eq!(
        error
            .get_ref()
            .and_then(|error| error.downcast_ref::<RemoteError>())
            .and_then(|error| error.code),
        Some(ErrorCode::CursorExpired)
    );
    let baseline = daemon.client.events(None, 256, 0).unwrap();
    let waiting_client = daemon.client.clone();
    let wait = thread::spawn(move || waiting_client.events(Some(baseline.cursor), 256, 5_000));
    thread::sleep(Duration::from_millis(50));
    daemon.stop_with_cli();
    let error = wait.join().unwrap().unwrap_err();
    assert_eq!(
        error
            .get_ref()
            .and_then(|error| error.downcast_ref::<RemoteError>())
            .and_then(|error| error.code),
        Some(ErrorCode::DaemonStopping)
    );
}

#[test]
fn agent_runtime_is_revisioned_durable_and_version_compatible() {
    let mut daemon = TestDaemon::start();
    let workspace = daemon
        .client
        .create_workspace(
            "agent-runtime",
            vec![ShellSpec::login("runtime", std::env::temp_dir())],
        )
        .unwrap();
    let shell_id = workspace.shells[0].id.clone();
    let attachment = daemon.client.attach(&shell_id, false, profile()).unwrap();
    let run_id = daemon
        .client
        .get_shell(&shell_id)
        .unwrap()
        .run
        .expect("started agent shell has no run identity")
        .id;
    let baseline = daemon.client.events(None, 256, 0).unwrap().cursor;
    let registration = AgentRegistrationSpec {
        name: "runtime-agent".into(),
        integration: "native-test".into(),
        external_session_id: Some("session-1".into()),
        report: AgentReport {
            state: AgentState::Working,
            authority: AgentAuthority::LifecycleIntegration,
            evidence: "registered".into(),
            confidence: 90,
        },
    };

    let error = daemon
        .client
        .register_agent(&shell_id, Uuid::new_v4().to_string(), registration.clone())
        .unwrap_err();
    assert_remote_code(&error, ErrorCode::RunChanged);
    let registered = daemon
        .client
        .register_agent(&shell_id, &run_id, registration)
        .unwrap();
    assert_eq!(registered.workspace_id, workspace.id);
    assert_eq!(registered.shell_id, shell_id);
    assert_eq!(registered.run_id, run_id);
    assert_eq!(registered.observation.revision, 1);
    assert_eq!(registered.observation.state, AgentState::Working);
    assert!(registered.ended_at_ms.is_none());
    assert_eq!(daemon.client.get_agent(&registered.id).unwrap(), registered);
    let snapshot_agent = daemon
        .client
        .snapshot()
        .unwrap()
        .workspaces
        .into_iter()
        .find(|current| current.id == workspace.id)
        .unwrap()
        .agents
        .into_iter()
        .find(|agent| agent.id == registered.id)
        .unwrap();
    assert_eq!(snapshot_agent, registered);

    let error = daemon
        .client
        .report_agent(
            &registered.id,
            Uuid::new_v4().to_string(),
            AgentReport {
                state: AgentState::Blocked,
                authority: AgentAuthority::ProcessAdapter,
                evidence: "wrong run".into(),
                confidence: 50,
            },
        )
        .unwrap_err();
    assert_remote_code(&error, ErrorCode::RunChanged);
    let blocked = daemon
        .client
        .report_agent(
            &registered.id,
            &run_id,
            AgentReport {
                state: AgentState::Blocked,
                authority: AgentAuthority::ProcessAdapter,
                evidence: "waiting for input".into(),
                confidence: 80,
            },
        )
        .unwrap();
    assert_eq!(blocked.observation.revision, 2);
    assert_eq!(blocked.observation.state, AgentState::Blocked);
    assert!(blocked.ended_at_ms.is_none());
    assert!(blocked.observation.observed_at_ms >= registered.observation.observed_at_ms);
    let done = daemon
        .client
        .report_agent(
            &registered.id,
            &run_id,
            AgentReport {
                state: AgentState::Done,
                authority: AgentAuthority::LifecycleIntegration,
                evidence: "completed".into(),
                confidence: 100,
            },
        )
        .unwrap();
    assert_eq!(done.observation.revision, 3);
    assert_eq!(done.observation.state, AgentState::Done);
    assert_eq!(done.ended_at_ms, Some(done.observation.observed_at_ms));
    let error = daemon
        .client
        .report_agent(
            &registered.id,
            &run_id,
            AgentReport {
                state: AgentState::Idle,
                authority: AgentAuthority::TerminalHeuristic,
                evidence: "too late".into(),
                confidence: 10,
            },
        )
        .unwrap_err();
    assert_remote_code(&error, ErrorCode::InvalidArgument);

    let events = daemon
        .client
        .events(Some(baseline.clone()), 256, 0)
        .unwrap();
    assert!(events.events.iter().any(|event| matches!(
        &event.kind,
        protocol::DaemonEventKind::AgentRegistered { agent, .. }
            if agent.id == registered.id && agent.observation.revision == 1
    )));
    assert!(events.events.iter().any(|event| matches!(
        &event.kind,
        protocol::DaemonEventKind::AgentStateChanged { agent, .. }
            if agent.id == registered.id && agent.observation.revision == 2
    )));
    assert!(events.events.iter().any(|event| matches!(
        &event.kind,
        protocol::DaemonEventKind::AgentCompleted { agent, .. }
            if agent.id == registered.id && agent.observation.revision == 3
    )));

    let list = daemon.command().args(["agent", "list"]).output().unwrap();
    assert!(list.status.success());
    assert!(contains(&list.stdout, b"runtime-agent"));
    assert!(contains(&list.stdout, registered.id.as_bytes()));
    let list = daemon
        .command()
        .args(["agent", "list", "--json"])
        .output()
        .unwrap();
    assert!(list.status.success());
    let list: serde_json::Value = serde_json::from_slice(&list.stdout).unwrap();
    assert_eq!(list["command"], "agent.list");
    assert_eq!(list["data"]["agents"][0]["id"], registered.id);
    assert_eq!(list["data"]["agents"][0]["observation"]["revision"], 3);
    let inspect = daemon
        .command()
        .args(["agent", "inspect", &registered.id])
        .output()
        .unwrap();
    assert!(inspect.status.success());
    assert!(contains(&inspect.stdout, b"STATE\tdone"));
    assert!(contains(&inspect.stdout, b"REVISION\t3"));
    let inspect = daemon
        .command()
        .args(["agent", "inspect", &registered.id, "--json"])
        .output()
        .unwrap();
    assert!(inspect.status.success());
    let inspect: serde_json::Value = serde_json::from_slice(&inspect.stdout).unwrap();
    assert_eq!(inspect["command"], "agent.inspect");
    assert_eq!(inspect["data"]["agent"]["id"], registered.id);
    assert_eq!(inspect["data"]["agent"]["observation"]["state"], "done");

    let protocol_eight_snapshot = versioned_request(&daemon.client, 8, protocol::Request::Snapshot);
    let protocol::Response::Snapshot { snapshot } = protocol_eight_snapshot else {
        panic!("expected protocol-8 snapshot response");
    };
    assert!(
        snapshot
            .workspaces
            .iter()
            .all(|workspace| workspace.agents.is_empty())
    );
    let protocol_eight_events = versioned_request(
        &daemon.client,
        8,
        protocol::Request::Events {
            after: Some(baseline),
            limit: 256,
            wait_ms: 0,
        },
    );
    let protocol::Response::Events {
        cursor: filtered_cursor,
        events,
        ..
    } = protocol_eight_events
    else {
        panic!("expected protocol-8 events response");
    };
    assert!(events.is_empty());
    daemon
        .client
        .rename_shell(&shell_id, "runtime-renamed")
        .unwrap();
    let protocol_eight_events = versioned_request(
        &daemon.client,
        8,
        protocol::Request::Events {
            after: Some(filtered_cursor.clone()),
            limit: 256,
            wait_ms: 0,
        },
    );
    let protocol::Response::Events { cursor, events, .. } = protocol_eight_events else {
        panic!("expected protocol-8 events response after rename");
    };
    assert!(events.iter().any(|event| matches!(
        &event.kind,
        protocol::DaemonEventKind::ShellRenamed { shell_id: id, .. } if id == &shell_id
    )));
    assert!(cursor.event_id > filtered_cursor.event_id);

    drop(attachment.stream);
    daemon.stop_with_cli();
    daemon.restart();
    assert_eq!(daemon.client.get_agent(&registered.id).unwrap(), done);
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
fn workspace_launchers_persist_emit_events_and_open_without_shells() {
    let mut daemon = TestDaemon::start();
    let workspace = daemon
        .client
        .create_workspace("launcher-only", Vec::new())
        .unwrap();
    let cursor = daemon.client.events(None, 256, 0).unwrap().cursor;
    let output = daemon.runtime_dir.join("launcher-output");
    let first = daemon
        .client
        .create_launcher(
            &workspace.id,
            WorkspaceLauncherSpec {
                name: "editor".into(),
                cwd: daemon.runtime_dir.clone(),
                command: vec![
                    "/bin/sh".into(),
                    "-c".into(),
                    "printf '%s|%s|%s|%s|%s' \"$PWD\" \"$BOOMUX_WORKSPACE_ID\" \"$BOOMUX_WORKSPACE\" \"$BOOMUX_LAUNCHER_ID\" \"$BOOMUX_LAUNCHER_NAME\" > \"$1\"".into(),
                    "launcher".into(),
                    output.display().to_string(),
                ],
            },
        )
        .unwrap();
    let second = daemon
        .client
        .create_launcher(
            &workspace.id,
            WorkspaceLauncherSpec {
                name: "browser".into(),
                cwd: daemon.runtime_dir.clone(),
                command: vec!["/bin/true".into()],
            },
        )
        .unwrap();
    let snapshot = daemon.client.get_workspace(&workspace.id).unwrap();
    assert_eq!(
        snapshot
            .launchers
            .iter()
            .map(|launcher| launcher.name.as_str())
            .collect::<Vec<_>>(),
        ["editor", "browser"]
    );
    let events = daemon.client.events(Some(cursor.clone()), 256, 0).unwrap();
    assert_eq!(
        events
            .events
            .iter()
            .filter(|event| matches!(
                event.kind,
                protocol::DaemonEventKind::LauncherCreated { .. }
            ))
            .count(),
        2
    );

    let restart = daemon
        .command()
        .args(["daemon", "restart"])
        .output()
        .unwrap();
    assert!(restart.status.success());
    let restored = daemon.client.get_workspace(&workspace.id).unwrap();
    assert_eq!(restored.launchers[0].id, first.id);
    assert_eq!(restored.launchers[1].id, second.id);
    let listed = daemon
        .command()
        .args(["launcher", "list", "--workspace", &workspace.id, "--json"])
        .output()
        .unwrap();
    assert!(listed.status.success());
    let listed: serde_json::Value = serde_json::from_slice(&listed.stdout).unwrap();
    assert_eq!(listed["schema"], "boomux.cli/v1");
    assert_eq!(listed["command"], "launcher.list");
    assert_eq!(listed["data"]["launchers"][0]["id"], first.id);
    assert_eq!(
        listed["data"]["launchers"][0]["command"],
        serde_json::json!(first.command)
    );

    let opened = daemon
        .command()
        .args(["workspace", "open", &workspace.id])
        .output()
        .unwrap();
    assert!(
        opened.status.success(),
        "launcher-only workspace failed to open: {}",
        String::from_utf8_lossy(&opened.stderr)
    );
    wait_until(|| output.is_file(), "workspace launcher did not run");
    assert_eq!(
        fs::read_to_string(&output).unwrap(),
        format!(
            "{}|{}|{}|{}|{}",
            daemon.runtime_dir.display(),
            workspace.id,
            workspace.name,
            first.id,
            first.name
        )
    );

    daemon.client.rename_launcher(&first.id, "zed").unwrap();
    assert_eq!(daemon.client.get_launcher(&first.id).unwrap().name, "zed");
    daemon.client.remove_launcher(&second.id).unwrap();
    assert_eq!(
        daemon
            .client
            .get_workspace(&workspace.id)
            .unwrap()
            .launchers
            .len(),
        1
    );
    let events = daemon.client.events(Some(cursor), 256, 0).unwrap();
    assert!(events.events.iter().any(|event| matches!(
        event.kind,
        protocol::DaemonEventKind::LauncherRenamed { .. }
    )));
    assert!(events.events.iter().any(|event| matches!(
        event.kind,
        protocol::DaemonEventKind::LauncherRemoved { .. }
    )));
    let restart = daemon
        .command()
        .args(["daemon", "restart"])
        .output()
        .unwrap();
    assert!(restart.status.success());
    assert_eq!(
        daemon
            .client
            .get_workspace(&workspace.id)
            .unwrap()
            .launchers
            .iter()
            .map(|launcher| launcher.name.as_str())
            .collect::<Vec<_>>(),
        ["zed"]
    );
    daemon.client.close_workspace(&workspace.id).unwrap();
    let restart = daemon
        .command()
        .args(["daemon", "restart"])
        .output()
        .unwrap();
    assert!(restart.status.success());
    assert!(
        daemon
            .client
            .snapshot()
            .unwrap()
            .workspaces
            .iter()
            .all(|current| current.id != workspace.id)
    );
    daemon.stop_with_cli();
}

#[test]
fn native_daemon_lifecycle() {
    let mut daemon = TestDaemon::start();

    let capabilities = daemon
        .command()
        .args(["capabilities", "--json"])
        .output()
        .unwrap();
    assert!(capabilities.status.success());
    let capabilities: serde_json::Value = serde_json::from_slice(&capabilities.stdout).unwrap();
    assert_eq!(capabilities["schema"], "boomux.cli/v1");
    assert_eq!(capabilities["command"], "capabilities");
    assert_eq!(capabilities["data"]["daemon_protocol_version"], 9);
    assert!(
        capabilities["data"]["json_commands"]
            .as_array()
            .unwrap()
            .iter()
            .any(|command| command == "events")
    );
    assert!(
        capabilities["data"]["features"]
            .as_array()
            .unwrap()
            .iter()
            .any(|feature| feature == "revision_aware_reads")
    );

    let status = daemon
        .command()
        .args(["daemon", "status"])
        .output()
        .unwrap();
    assert!(status.status.success());
    assert!(String::from_utf8_lossy(&status.stdout).contains("running (protocol 9"));
    let status = daemon
        .command()
        .args(["daemon", "status", "--json"])
        .output()
        .unwrap();
    let status: serde_json::Value = serde_json::from_slice(&status.stdout).unwrap();
    assert_eq!(status["schema"], "boomux.cli/v1");
    assert_eq!(status["data"]["status"], "running");
    let mut protocol_six = UnixStream::connect(daemon.client.socket_path()).unwrap();
    protocol::write_message(
        &mut protocol_six,
        &protocol::Envelope::with_version(6, protocol::Request::Ping),
    )
    .unwrap();
    let response: protocol::Envelope<protocol::Response> =
        protocol::read_message(&mut protocol_six).unwrap();
    assert_eq!(response.version, 6);
    assert_eq!(response.message, protocol::Response::Pong);

    let missing_id = Uuid::new_v4().to_string();
    let error = daemon.client.get_shell(&missing_id).unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::NotFound);
    let remote = error
        .get_ref()
        .and_then(|error| error.downcast_ref::<RemoteError>())
        .unwrap();
    assert_eq!(remote.code, Some(ErrorCode::NotFound));
    let typed_failure = daemon
        .command()
        .args(["shells", "--json"])
        .env("BOOMUX_SHELL_ID", &missing_id)
        .output()
        .unwrap();
    assert!(!typed_failure.status.success());
    let typed_failure: serde_json::Value = serde_json::from_slice(&typed_failure.stderr).unwrap();
    assert_eq!(typed_failure["command"], "shells");
    assert_eq!(typed_failure["error"]["code"], "not_found");
    let unsupported = daemon
        .command()
        .args(["workspace", "create", "must-not-exist", "--json"])
        .output()
        .unwrap();
    assert!(!unsupported.status.success());
    assert!(unsupported.stdout.is_empty());
    let unsupported_error: serde_json::Value = serde_json::from_slice(&unsupported.stderr).unwrap();
    assert_eq!(unsupported_error["error"]["code"], "invalid_argument");
    assert!(
        daemon
            .client
            .snapshot()
            .unwrap()
            .workspaces
            .iter()
            .all(|workspace| workspace.name != "must-not-exist")
    );

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
        .args(["workspace", "inspect", "cli-test", "--json"])
        .output()
        .unwrap();
    let output: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(output["command"], "workspace.inspect");
    assert_eq!(output["data"]["workspace"]["name"], "cli-test");
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
            "inspect",
            "checks",
            "--workspace",
            "cli-renamed",
            "--json",
        ])
        .output()
        .unwrap();
    let output: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(output["command"], "shell.inspect");
    assert_eq!(output["data"]["shell"]["status"], "pending");
    assert!(output["data"]["shell"]["run"].is_null());
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
        error
            .get_ref()
            .and_then(|error| error.downcast_ref::<RemoteError>())
            .and_then(|error| error.code),
        Some(ErrorCode::ShellStartFailed)
    );
    assert_eq!(
        daemon.client.get_shell(&failed_shell.id).unwrap().status,
        ShellStatus::Pending
    );
    assert!(
        daemon
            .client
            .get_shell(&failed_shell.id)
            .unwrap()
            .run
            .is_none()
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
    let initial_run = daemon
        .client
        .get_shell(&shell_id)
        .unwrap()
        .run
        .expect("running shell has no run identity");
    let inspected = daemon
        .command()
        .args(["shell", "inspect", &shell_id])
        .output()
        .unwrap();
    assert!(inspected.status.success());
    assert!(contains(&inspected.stdout, b"RUN ID"));
    assert!(contains(&inspected.stdout, initial_run.id.as_bytes()));
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
    let run_command = "printf 'run=%s:end\\n' \"$BOOMUX_RUN_ID\"\n".to_owned();
    AttachFrame::Input(run_command.into_bytes())
        .write_to(&mut first)
        .unwrap();
    let output = read_until(&mut first, b":end");
    assert!(contains(
        &output,
        format!("run={}", initial_run.id).as_bytes()
    ));
    AttachFrame::Input(b"printf 'transport-ok\\n'\n".to_vec())
        .write_to(&mut first)
        .unwrap();
    let output = read_until(&mut first, b"transport-ok");
    assert!(contains(&output, b"transport-ok"));
    let output = daemon
        .command()
        .args(["read", &shell_id, "--lines", "20", "--json"])
        .output()
        .unwrap();
    let output: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(output["command"], "read");
    assert_eq!(output["data"]["run_id"], initial_run.id);
    assert!(
        output["data"]["output"]
            .as_str()
            .unwrap()
            .contains("transport-ok")
    );
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
    assert_eq!(
        daemon.client.get_shell(&shell_id).unwrap().run.unwrap().id,
        initial_run.id
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
    loop {
        match AttachFrame::read_from(&mut second) {
            Ok(AttachFrame::Output(_)) => continue,
            Err(_) => break,
            Ok(frame) => panic!("old controller received unexpected frame: {frame:?}"),
        }
    }

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
    let exited_run = daemon.client.get_shell(&shell_id).unwrap().run.unwrap();
    assert_eq!(exited_run.id, initial_run.id);
    assert!(exited_run.ended_at_ms.is_some());
    assert!(matches!(
        exited_run.exit_reason,
        Some(ShellRunExitReason::Exited { .. })
    ));
    assert!(exited_run.output_revision > initial_run.output_revision);
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

fn assert_remote_code(error: &io::Error, expected: ErrorCode) {
    assert_eq!(
        error
            .get_ref()
            .and_then(|error| error.downcast_ref::<RemoteError>())
            .and_then(|error| error.code),
        Some(expected)
    );
}

fn versioned_request(
    client: &Client,
    version: u32,
    request: protocol::Request,
) -> protocol::Response {
    let mut stream = UnixStream::connect(client.socket_path()).unwrap();
    protocol::write_message(
        &mut stream,
        &protocol::Envelope::with_version(version, request),
    )
    .unwrap();
    let response: protocol::Envelope<protocol::Response> =
        protocol::read_message(&mut stream).unwrap();
    assert_eq!(response.version, version);
    response.message
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

use std::fs;
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::process::Command;
use std::thread;
use std::time::{Duration, Instant};

use boomux::client;
use boomux::protocol::{self, AttachFrame, ErrorCode, ShellSpec};
use uuid::Uuid;

use crate::support::{TestDaemon, assert_remote_code, contains, profile, read_until, wait_until};

#[test]
fn daemon_bounds_stalled_connections_and_recovers_capacity() {
    let daemon = TestDaemon::start();
    let mut stalled = (0..64)
        .map(|_| UnixStream::connect(daemon.client.socket_path()).unwrap())
        .collect::<Vec<_>>();
    for stream in &mut stalled {
        stream.write_all(&[0]).unwrap();
    }
    thread::sleep(Duration::from_millis(200));

    let mut rejected = UnixStream::connect(daemon.client.socket_path()).unwrap();
    rejected
        .set_read_timeout(Some(Duration::from_secs(1)))
        .unwrap();
    let mut byte = [0];
    assert_eq!(rejected.read(&mut byte).unwrap(), 0);

    wait_until(
        || daemon.client.ping().is_ok(),
        "daemon did not recover connection capacity",
    );
}

#[test]
fn daemon_shutdown_does_not_wait_indefinitely_for_abandoned_handlers() {
    let mut daemon = TestDaemon::start();
    let mut stalled = (0..8)
        .map(|_| UnixStream::connect(daemon.client.socket_path()).unwrap())
        .collect::<Vec<_>>();
    for stream in &mut stalled {
        stream.write_all(&[0]).unwrap();
    }
    thread::sleep(Duration::from_millis(100));

    let started = Instant::now();
    daemon.stop_with_cli();

    assert!(started.elapsed() < Duration::from_secs(5));
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
    let deadline = Instant::now() + Duration::from_secs(1);
    let mut saw_run_started = false;
    let mut saw_output_changed = false;
    while !saw_run_started || !saw_output_changed {
        let changed = daemon.client.events(Some(cursor), 256, 1_000).unwrap();
        saw_run_started |= changed
            .events
            .iter()
            .any(|event| matches!(event.kind, protocol::DaemonEventKind::RunStarted { .. }));
        saw_output_changed |= changed
            .events
            .iter()
            .any(|event| matches!(event.kind, protocol::DaemonEventKind::OutputChanged { .. }));
        cursor = changed.cursor;
        assert!(
            Instant::now() < deadline,
            "run and output events were not both published"
        );
    }

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
    assert_remote_code(&error, ErrorCode::RunChanged);
    let error = daemon
        .client
        .read_shell_at(&shell_id, 1024, Some(run_id.clone()), Some(u64::MAX), 0)
        .unwrap_err();
    assert_remote_code(&error, ErrorCode::RevisionAhead);

    let settle_deadline = Instant::now() + Duration::from_secs(2);
    let mut wait_revision = advanced_revision;
    loop {
        let settled = daemon
            .client
            .read_shell_at(
                &shell_id,
                1024,
                Some(run_id.clone()),
                Some(wait_revision),
                100,
            )
            .unwrap();
        if !settled.changed {
            break;
        }
        wait_revision = settled.output_revision.unwrap();
        assert!(
            Instant::now() < settle_deadline,
            "terminal output did not settle before restart"
        );
    }
    let waiting_client = daemon.client.clone();
    let waiting_shell_id = shell_id.clone();
    let wait = thread::spawn(move || {
        waiting_client.read_shell_at(
            waiting_shell_id,
            1024,
            Some(run_id),
            Some(wait_revision),
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
    assert_remote_code(&error, ErrorCode::DaemonStopping);
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
    assert_remote_code(&error, ErrorCode::CursorExpired);
    let baseline = daemon.client.events(None, 256, 0).unwrap();
    let waiting_client = daemon.client.clone();
    let wait = thread::spawn(move || waiting_client.events(Some(baseline.cursor), 256, 5_000));
    thread::sleep(Duration::from_millis(50));
    daemon.stop_with_cli();
    let error = wait.join().unwrap().unwrap_err();
    assert_remote_code(&error, ErrorCode::DaemonStopping);
}

#[test]
fn snapshot_watch_refreshes_on_events_and_recovers_after_cold_restart() {
    let mut daemon = TestDaemon::start();
    let mut watch = client::SnapshotWatch::baseline(&daemon.client).unwrap();
    assert!(watch.snapshot().workspaces.is_empty());
    assert_eq!(watch.poll(&daemon.client).unwrap(), (false, false));

    daemon
        .client
        .create_workspace("watched", Vec::new())
        .unwrap();
    assert_eq!(watch.poll(&daemon.client).unwrap(), (true, false));
    assert_eq!(watch.snapshot().workspaces[0].name, "watched");
    assert_eq!(watch.poll(&daemon.client).unwrap(), (false, false));

    daemon.stop_with_cli();
    daemon.restart();
    assert_eq!(watch.poll(&daemon.client).unwrap(), (true, true));
    assert_eq!(watch.snapshot().workspaces[0].name, "watched");
}

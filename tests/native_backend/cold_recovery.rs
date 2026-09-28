use boomux::protocol::{self, ErrorCode, Request, Response, ShellRecoveryTarget, ShellSpec};
use std::os::unix::net::UnixStream;

use crate::support::{TestDaemon, profile, read_until_after};

fn interrupted_shells(daemon: &mut TestDaemon) -> Vec<ShellRecoveryTarget> {
    let workspace = daemon
        .client
        .create_workspace(
            "batch",
            (0..2)
                .map(|index| ShellSpec {
                    name: format!("shell-{index}"),
                    cwd: daemon.runtime_dir.clone(),
                    command: vec![
                        "/bin/sh".into(),
                        "-c".into(),
                        "printf 'ready\\n'; exec /bin/sleep 60".into(),
                    ],
                })
                .collect(),
        )
        .unwrap();
    let targets = workspace
        .shells
        .iter()
        .map(|shell| {
            let mut attachment = daemon.client.attach(&shell.id, false, profile()).unwrap();
            read_until_after(&mut attachment.stream, b"ready", attachment.reconstruction);
            ShellRecoveryTarget {
                shell_id: shell.id.clone(),
                expected_run_id: daemon.client.get_shell(&shell.id).unwrap().run.unwrap().id,
                profile: profile(),
            }
        })
        .collect();
    daemon.crash();
    daemon.restart();
    targets
}

#[test]
fn cold_recovery_old_wire_rejects_batch_but_retains_individual_attachment() {
    let mut daemon = TestDaemon::start();
    let targets = interrupted_shells(&mut daemon);
    let request = Request::RecoverShells {
        shells: targets.clone(),
        environment: None,
    };
    let mut stream = UnixStream::connect(daemon.client.socket_path()).unwrap();
    protocol::write_message(&mut stream, &protocol::Envelope::with_version(55, request)).unwrap();
    let response: protocol::Envelope<Response> = protocol::read_message(&mut stream).unwrap();
    assert!(matches!(
        response.message,
        Response::Error {
            code: Some(ErrorCode::UnsupportedVersion),
            ..
        }
    ));
    for target in &targets {
        assert_eq!(
            daemon
                .client
                .get_shell(&target.shell_id)
                .unwrap()
                .run
                .unwrap()
                .id,
            target.expected_run_id
        );
    }
    let mut stream = UnixStream::connect(daemon.client.socket_path()).unwrap();
    protocol::write_message(
        &mut stream,
        &protocol::Envelope::with_version(
            55,
            Request::Attach {
                shell_id: targets[0].shell_id.clone(),
                takeover: false,
                restart_exited: false,
                expected_run_id: None,
                profile: profile(),
                environment: None,
                owner_environment: false,
            },
        ),
    )
    .unwrap();
    let response: protocol::Envelope<Response> = protocol::read_message(&mut stream).unwrap();
    assert!(matches!(response.message, Response::Attached { .. }));
}

#[test]
#[cfg(debug_assertions)]
fn cold_recovery_crash_on_either_side_of_commit_preserves_exact_generations() {
    use crate::support::wait_until;
    use boomux::client::Client;
    use boomux::protocol::{DaemonEventKind, ShellRecoveryResult, ShellRunExitReason, ShellStatus};
    use std::fs;
    for phase in ["before-commit", "after-commit"] {
        let mut daemon = TestDaemon::start();
        let targets = interrupted_shells(&mut daemon);
        let cursor = daemon.client.events(None, 100, 0).unwrap().cursor;
        let state = daemon.runtime_dir.join("state/boomux/state.json");
        let barrier = daemon
            .runtime_dir
            .join(format!("state/boomux/.native-test-recovery-{phase}"));
        fs::create_dir(&barrier).unwrap();
        let client = Client::from_socket_path(daemon.client.socket_path().to_owned());
        let request_targets = targets.clone();
        let request = std::thread::spawn(move || client.recover_shells(request_targets));
        wait_until(
            || barrier.join("ready").exists(),
            "batch did not reach commit barrier",
        );
        assert!(
            daemon
                .client
                .events(Some(cursor), 100, 0)
                .unwrap()
                .events
                .iter()
                .all(|event| !matches!(event.kind, DaemonEventKind::RunStarted { .. }))
        );
        let saved: serde_json::Value = serde_json::from_slice(&fs::read(&state).unwrap()).unwrap();
        let saved_runs = targets
            .iter()
            .map(|target| {
                let shell = saved["workspaces"][0]["shells"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .find(|shell| shell["id"] == target.shell_id)
                    .unwrap();
                let id = shell["last_run"]["id"].as_str().unwrap().to_owned();
                assert_eq!(id == target.expected_run_id, phase == "before-commit");
                id
            })
            .collect::<Vec<_>>();
        daemon.crash();
        assert!(request.join().unwrap().is_err());
        fs::remove_dir_all(barrier).unwrap();
        daemon.restart();
        for (target, saved_run) in targets.iter().zip(saved_runs) {
            let shell = daemon.client.get_shell(&target.shell_id).unwrap();
            assert_eq!(shell.status, ShellStatus::Pending);
            let run = shell.run.unwrap();
            assert_eq!(run.id, saved_run);
            assert_eq!(run.generation, if phase == "before-commit" { 1 } else { 2 });
            assert_eq!(run.exit_reason, Some(ShellRunExitReason::Interrupted));
        }
        if phase == "after-commit" {
            let results = daemon.client.recover_shells(targets).unwrap().unwrap();
            assert!(results.iter().all(|result| matches!(
                result,
                ShellRecoveryResult::Unavailable {
                    code: ErrorCode::RunChanged,
                    ..
                }
            )));
        }
    }
}

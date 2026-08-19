use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;

use boomux::client::{Attachment, Client};
use boomux::protocol::{
    self, AttachFrame, ErrorCode, ShellRunExitReason, ShellSpec, ShellStatus, UnixEnvironment,
    UnixEnvironmentVariable,
};
use uuid::Uuid;

use crate::support::{
    TestDaemon, assert_remote_code, contains, profile, read_until, wait_for_attach_with_profile,
    wait_until,
};

#[test]
fn legacy_workspace_default_cwd_is_inherited_and_survives_handoff() {
    let mut daemon = TestDaemon::start();
    let project = daemon.runtime_dir.join("project");
    let other = daemon.runtime_dir.join("other");
    fs::create_dir(&project).unwrap();
    fs::create_dir(&other).unwrap();

    daemon
        .client
        .create_workspace_with_default_cwd("project", Some(project.clone()), Vec::new())
        .unwrap();
    let inspected = daemon
        .command()
        .args(["workspace", "inspect", "project", "--json"])
        .output()
        .unwrap();
    let inspected: serde_json::Value = serde_json::from_slice(&inspected.stdout).unwrap();
    assert_eq!(
        inspected["data"]["workspace"]["default_cwd"],
        project.display().to_string()
    );

    let first = daemon
        .command()
        .current_dir(&other)
        .args(["shell", "create", "project", "--name", "first"])
        .output()
        .unwrap();
    assert!(first.status.success());
    let workspace = daemon
        .client
        .snapshot()
        .unwrap()
        .workspaces
        .into_iter()
        .find(|workspace| workspace.name == "project")
        .unwrap();
    assert_eq!(workspace.shells[0].cwd, project);

    let restart = daemon
        .command()
        .args(["daemon", "restart"])
        .output()
        .unwrap();
    assert!(restart.status.success());
    wait_until(
        || daemon.child.as_mut().unwrap().try_wait().unwrap().is_some(),
        "old daemon did not exit after handoff",
    );

    let second = daemon
        .command()
        .current_dir(&other)
        .args(["shell", "create", "project", "--name", "second"])
        .output()
        .unwrap();
    assert!(second.status.success());
    let explicit = daemon
        .command()
        .args(["shell", "create", "project", "--name", "explicit", "--cwd"])
        .arg(&other)
        .output()
        .unwrap();
    assert!(explicit.status.success());
    let workspace = daemon
        .client
        .snapshot()
        .unwrap()
        .workspaces
        .into_iter()
        .find(|workspace| workspace.name == "project")
        .unwrap();
    assert_eq!(workspace.default_cwd.as_deref(), Some(project.as_path()));
    assert_eq!(workspace.shells[1].cwd, project);
    assert_eq!(workspace.shells[2].cwd, other);

    daemon.stop_with_cli();
    daemon.restart();
    let restored = daemon
        .client
        .snapshot()
        .unwrap()
        .workspaces
        .into_iter()
        .find(|workspace| workspace.name == "project")
        .unwrap();
    assert_eq!(restored.default_cwd.as_deref(), Some(project.as_path()));
    let after_restart = daemon
        .command()
        .current_dir(&other)
        .args(["shell", "create", "project", "--name", "after-restart"])
        .output()
        .unwrap();
    assert!(after_restart.status.success());
    let restored = daemon
        .client
        .snapshot()
        .unwrap()
        .workspaces
        .into_iter()
        .find(|workspace| workspace.name == "project")
        .unwrap();
    assert_eq!(restored.shells[3].cwd, project);

    fs::remove_dir_all(restored.default_cwd.unwrap()).unwrap();
    let missing = daemon
        .command()
        .args(["shell", "create", "project", "--name", "missing"])
        .output()
        .unwrap();
    assert!(!missing.status.success());
    assert!(
        String::from_utf8_lossy(&missing.stderr)
            .contains("workspace default working directory is unavailable")
    );
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
    let mut first = daemon.client.attach(&shell_id, false, profile()).unwrap();
    if !contains(&first.reconstruction, b"restored-command") {
        first
            .reconstruction
            .extend(read_until(&mut first.stream, b"restored-command"));
    }
    assert!(contains(&first.reconstruction, b"restored-command"));
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
    let mut second = daemon.client.attach(&shell_id, false, profile()).unwrap();
    if !contains(&second.reconstruction, b"restored-command") {
        second
            .reconstruction
            .extend(read_until(&mut second.stream, b"restored-command"));
    }
    assert!(contains(&second.reconstruction, b"restored-command"));
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
fn reattachment_reflows_retained_output_for_the_new_terminal_width() {
    let mut daemon = TestDaemon::start();
    let workspace = daemon
        .client
        .create_workspace(
            "reflow-on-attach",
            vec![ShellSpec::login("shell", std::env::temp_dir())],
        )
        .unwrap();
    let shell_id = workspace.shells[0].id.clone();
    let mut wide_profile = profile();
    wide_profile.rows = 6;
    wide_profile.cols = 60;
    let mut first = daemon
        .client
        .attach(&shell_id, false, wide_profile)
        .unwrap();
    AttachFrame::Input(
        b"printf 'first: a description that must remain complete\\nsecond: another description that must remain complete\\nreflow-finished\\n'\n"
            .to_vec(),
    )
    .write_to(&mut first.stream)
    .unwrap();
    assert!(contains(
        &read_until(&mut first.stream, b"reflow-finished"),
        b"reflow-finished"
    ));
    drop(first);

    let mut narrow_profile = profile();
    narrow_profile.rows = 12;
    narrow_profile.cols = 24;
    let second = wait_for_attach_with_profile(&daemon.client, &shell_id, narrow_profile);
    let contents = String::from_utf8(daemon.client.read_shell(&shell_id, 1_024).unwrap()).unwrap();
    assert!(
        contents.contains("first: a description that must remain complete"),
        "{contents:?}"
    );
    assert!(
        contents.contains("second: another description that must remain complete"),
        "{contents:?}"
    );

    drop(second);
    daemon.stop_with_cli();
}

#[test]
fn structured_shell_preview_preserves_color_while_plain_read_stays_plain() {
    let mut daemon = TestDaemon::start();
    let workspace = daemon
        .client
        .create_workspace(
            "styled-preview",
            vec![ShellSpec::login("shell", std::env::temp_dir())],
        )
        .unwrap();
    let shell_id = workspace.shells[0].id.clone();
    let mut attachment = daemon.client.attach(&shell_id, false, profile()).unwrap();
    AttachFrame::Input(b"printf '\\033[31mstyled-preview-marker\\033[0m\\n'\n".to_vec())
        .write_to(&mut attachment.stream)
        .unwrap();
    assert!(contains(
        &read_until(&mut attachment.stream, b"styled-preview-marker"),
        b"styled-preview-marker"
    ));

    let preview = daemon
        .client
        .read_shell_preview(&shell_id, 1024 * 1024, 500)
        .unwrap();
    let styled = preview
        .lines
        .iter()
        .flat_map(|line| &line.spans)
        .find(|span| {
            span.text.contains("styled-preview-marker")
                && span.style.foreground == boomux::protocol::TerminalColor::Indexed(1)
        });
    assert!(
        styled.is_some(),
        "styled preview did not retain red output: {preview:?}"
    );
    let plain = daemon.client.read_shell(&shell_id, 1024 * 1024).unwrap();
    assert!(contains(&plain, b"styled-preview-marker"));
    assert!(!plain.contains(&b'\x1b'));

    drop(attachment);
    daemon.stop_with_cli();
}

#[test]
fn attach_environment_is_ephemeral_and_authoritative_for_initial_and_restarted_runs() {
    let daemon = TestDaemon::start();
    let client_shell = daemon.runtime_dir.join("client-shell");
    fs::write(
        &client_shell,
        "#!/bin/sh\nbytes=$(printf '%s' \"$NON_UTF8\" | /usr/bin/od -An -tx1)\nprintf 'startup=%s|daemon=%s|term=%s|run=%s|bytes=%s\\n' \"$CLIENT_MARKER\" \"${BOOMUX_DAEMON_ONLY-unset}\" \"$TERM\" \"$BOOMUX_RUN_ID\" \"$bytes\"\nexec /bin/sh\n",
    )
    .unwrap();
    fs::set_permissions(&client_shell, fs::Permissions::from_mode(0o755)).unwrap();
    let workspace = daemon
        .client
        .create_workspace(
            "ephemeral-environment",
            vec![ShellSpec::login("shell", std::env::temp_dir())],
        )
        .unwrap();
    let shell_id = workspace.shells[0].id.clone();
    let first_secret = format!("first-secret-{}", Uuid::new_v4());
    let mut first = attach_with_environment(
        &daemon.client,
        &shell_id,
        false,
        environment_for_shell(&client_shell, &first_secret),
    );
    let first_output = read_until(&mut first.stream, b"|bytes= ff fe");
    assert!(contains(&first_output, first_secret.as_bytes()));
    assert!(contains(&first_output, b"daemon=unset"));
    assert!(contains(&first_output, b"term=attachment-term"));
    assert!(!contains(&first_output, b"attacker-run-id"));
    AttachFrame::Input(b"exit 0\n".to_vec())
        .write_to(&mut first.stream)
        .unwrap();
    drop(first);
    wait_until(
        || {
            matches!(
                daemon.client.get_shell(&shell_id).unwrap().status,
                ShellStatus::Exited { .. }
            )
        },
        "first environment shell did not exit",
    );

    let second_secret = format!("second-secret-{}", Uuid::new_v4());
    let mut second = attach_with_environment(
        &daemon.client,
        &shell_id,
        true,
        environment_for_shell(&client_shell, &second_secret),
    );
    let second_output = read_until(&mut second.stream, b"|bytes= ff fe");
    assert!(contains(&second_output, second_secret.as_bytes()));
    assert!(contains(&second_output, b"daemon=unset"));
    assert!(contains(&second_output, b"term=attachment-term"));
    assert!(!contains(&second_output, first_secret.as_bytes()));

    let state = fs::read(daemon.runtime_dir.join("state/boomux/state.json")).unwrap();
    let snapshot = serde_json::to_vec(&daemon.client.snapshot().unwrap()).unwrap();
    let events = daemon.client.events(None, 256, 0).unwrap();
    let events = serde_json::to_vec(&(events.snapshot, events.events)).unwrap();
    for bytes in [&state, &snapshot, &events] {
        assert!(!contains(bytes, first_secret.as_bytes()));
        assert!(!contains(bytes, second_secret.as_bytes()));
        assert!(!contains(bytes, b"attacker-run-id"));
    }
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
    assert_remote_code(&error, ErrorCode::PersistenceFailed);
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
fn restart_on_attach_reopens_an_exited_durable_shell() {
    let mut daemon = TestDaemon::start();
    let workspace = daemon
        .client
        .create_workspace(
            "restart-exited",
            vec![ShellSpec {
                name: "command".into(),
                cwd: std::env::temp_dir(),
                command: vec!["/bin/sh".into()],
            }],
        )
        .unwrap();
    let shell_id = workspace.shells[0].id.clone();
    let mut first = daemon
        .client
        .attach(&shell_id, false, profile())
        .unwrap()
        .stream;
    AttachFrame::Input(b"printf 'restartable-run\\n'; exit 0\n".to_vec())
        .write_to(&mut first)
        .unwrap();
    assert!(contains(
        &read_until(&mut first, b"restartable-run"),
        b"restartable-run"
    ));
    drop(first);
    wait_until(
        || {
            matches!(
                daemon.client.get_shell(&shell_id).unwrap().status,
                ShellStatus::Exited { .. }
            )
        },
        "first shell run did not exit",
    );
    let first_run = daemon.client.get_shell(&shell_id).unwrap().run.unwrap();

    let mut second = daemon
        .client
        .attach_restarting(&shell_id, false, profile())
        .unwrap()
        .stream;
    AttachFrame::Input(b"printf 'restartable-run\\n'\n".to_vec())
        .write_to(&mut second)
        .unwrap();
    assert!(contains(
        &read_until(&mut second, b"restartable-run"),
        b"restartable-run"
    ));
    let second_run = daemon.client.get_shell(&shell_id).unwrap().run.unwrap();
    assert_ne!(second_run.id, first_run.id);
    assert_eq!(second_run.generation, 2);

    drop(second);
    daemon.client.close_workspace(&workspace.id).unwrap();
    daemon.stop_with_cli();
}

#[test]
fn exact_run_attach_rejects_a_run_changed_after_validation_without_takeover() {
    let mut daemon = TestDaemon::start();
    let workspace = daemon
        .client
        .create_workspace(
            "exact-attach-race",
            vec![ShellSpec {
                name: "shell".into(),
                cwd: std::env::temp_dir(),
                command: vec!["/bin/sh".into()],
            }],
        )
        .unwrap();
    let shell_id = workspace.shells[0].id.clone();
    let mut first = daemon
        .client
        .attach(&shell_id, false, profile())
        .unwrap()
        .stream;
    let validated_run = daemon.client.get_shell(&shell_id).unwrap().run.unwrap().id;

    AttachFrame::Input(b"exit 0\n".to_vec())
        .write_to(&mut first)
        .unwrap();
    drop(first);
    wait_until(
        || {
            matches!(
                daemon.client.get_shell(&shell_id).unwrap().status,
                ShellStatus::Exited { .. }
            )
        },
        "validated run did not exit",
    );

    let mut later = daemon
        .client
        .attach_restarting(&shell_id, false, profile())
        .unwrap()
        .stream;
    let later_run = daemon.client.get_shell(&shell_id).unwrap().run.unwrap().id;
    assert_ne!(later_run, validated_run);

    let error = daemon
        .client
        .attach_exact_run_with_client_environment(&shell_id, &validated_run, true, profile())
        .unwrap_err();
    assert_remote_code(&error, ErrorCode::RunChanged);

    AttachFrame::Input(b"printf 'later-run-still-attached\\n'\n".to_vec())
        .write_to(&mut later)
        .unwrap();
    assert!(contains(
        &read_until(&mut later, b"later-run-still-attached"),
        b"later-run-still-attached"
    ));

    drop(later);
    daemon.client.close_workspace(&workspace.id).unwrap();
    daemon.stop_with_cli();
}

#[test]
fn failed_implicit_terminal_launch_rolls_back_created_state() {
    let daemon = TestDaemon::start();
    let project = daemon.runtime_dir.join("project");
    fs::create_dir(&project).unwrap();

    let generated = daemon
        .command()
        .arg(&project)
        .arg("--new")
        .env("PATH", "")
        .output()
        .unwrap();
    assert!(!generated.status.success());
    assert!(daemon.client.snapshot().unwrap().workspaces.is_empty());

    let workspace = daemon
        .client
        .create_workspace("existing", Vec::new())
        .unwrap();
    let existing = daemon
        .command()
        .arg(&project)
        .args(["--name", "existing", "--new"])
        .env("PATH", "")
        .output()
        .unwrap();
    assert!(!existing.status.success());
    assert!(
        daemon
            .client
            .get_workspace(&workspace.id)
            .unwrap()
            .shells
            .is_empty()
    );
}

fn environment_for_shell(shell: &std::path::Path, marker: &str) -> UnixEnvironment {
    UnixEnvironment {
        variables: vec![
            UnixEnvironmentVariable {
                name: b"SHELL".to_vec(),
                value: shell.as_os_str().as_encoded_bytes().to_vec(),
            },
            UnixEnvironmentVariable {
                name: b"CLIENT_MARKER".to_vec(),
                value: marker.as_bytes().to_vec(),
            },
            UnixEnvironmentVariable {
                name: b"TERM".to_vec(),
                value: b"client-supplied-term".to_vec(),
            },
            UnixEnvironmentVariable {
                name: b"BOOMUX_RUN_ID".to_vec(),
                value: b"attacker-run-id".to_vec(),
            },
            UnixEnvironmentVariable {
                name: b"NON_UTF8".to_vec(),
                value: vec![0xff, 0xfe],
            },
        ],
    }
}

fn attach_with_environment(
    client: &Client,
    shell_id: &str,
    restart_exited: bool,
    environment: UnixEnvironment,
) -> Attachment {
    let mut stream = UnixStream::connect(client.socket_path()).unwrap();
    protocol::write_message(
        &mut stream,
        &protocol::Envelope::with_version(
            16,
            protocol::Request::Attach {
                shell_id: shell_id.into(),
                takeover: false,
                restart_exited,
                expected_run_id: None,
                profile: profile(),
                environment: Some(environment),
                owner_environment: false,
                controller_kind: protocol::AttachmentControllerKind::Legacy,
            },
        ),
    )
    .unwrap();
    let response: protocol::Envelope<protocol::Response> =
        protocol::read_message(&mut stream).unwrap();
    assert_eq!(response.version, 16);
    match response.message {
        protocol::Response::Attached {
            token,
            reconstruction,
            warning,
        } => Attachment {
            stream,
            token,
            reconstruction,
            warning,
            protocol_version: 16,
        },
        response => panic!("unexpected attach response: {response:?}"),
    }
}

#[test]
fn owner_environment_attach_rejects_an_arbitrary_unix_environment() {
    let daemon = TestDaemon::start();
    let workspace = daemon
        .client
        .create_workspace("owner-environment-exclusive", Vec::new())
        .unwrap();
    let shell = daemon
        .client
        .create_shell(&workspace.id, ShellSpec::login("exclusive", "/tmp"))
        .unwrap();
    let mut stream = UnixStream::connect(daemon.client.socket_path()).unwrap();
    protocol::write_message(
        &mut stream,
        &protocol::Envelope::with_version(
            protocol::PROTOCOL_VERSION,
            protocol::Request::Attach {
                shell_id: shell.id,
                takeover: false,
                restart_exited: false,
                expected_run_id: None,
                profile: profile(),
                environment: Some(UnixEnvironment {
                    variables: Vec::new(),
                }),
                owner_environment: true,
                controller_kind: protocol::AttachmentControllerKind::Legacy,
            },
        ),
    )
    .unwrap();
    let response: protocol::Envelope<protocol::Response> =
        protocol::read_message(&mut stream).unwrap();
    assert!(matches!(
        response.message,
        protocol::Response::Error {
            code: Some(ErrorCode::InvalidArgument),
            ..
        }
    ));
}

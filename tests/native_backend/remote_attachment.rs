use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::process::Stdio;

use boomux::protocol::{AttachFrame, QualifiedIdentity, ShellSpec};
use uuid::Uuid;

use crate::support::{
    CONTROL_MASTER_PREFIX, TestDaemon, contains, parse_pid, profile, read_until, read_until_after,
    wait_until,
};

fn install_fake_ssh(
    directory: &std::path::Path,
    executable: &std::path::Path,
    remote: &TestDaemon,
) {
    let ssh = directory.join("ssh");
    fs::write(
        &ssh,
        format!(
            "#!/bin/sh\n{CONTROL_MASTER_PREFIX}\nlast=\nfor arg do last=$arg; done\ncase \"$last\" in\n  *boomux-platform-v1*) printf 'boomux-platform-v1\\0Linux\\0x86_64\\0' ;;\n  *boomux-executables-v1*) printf 'boomux-executables-v1\\0{}\\0' ;;\n  *boomux-install-destination-v1*) printf 'boomux-install-destination-v1\\0{}/boomux\\0' ;;\n  *'__federation-stdio'*) exec env -i PATH=/usr/bin:/bin HOME={} XDG_RUNTIME_DIR={} XDG_STATE_HOME={} {} __federation-stdio ;;\n  *) exit 64 ;;\nesac\n",
            executable.display(),
            directory.display(),
            directory.display(),
            remote.runtime_dir.display(),
            remote.runtime_dir.join("state").display(),
            executable.display(),
        ),
    )
    .unwrap();
    fs::set_permissions(&ssh, fs::Permissions::from_mode(0o700)).unwrap();
}

#[test]
fn two_daemon_workspace_coordination_adopts_links_places_and_retries_close() {
    let remote = TestDaemon::start();
    let adopted_owner = remote
        .client
        .create_workspace("remote-adopt", Vec::new())
        .unwrap();
    let linked_owner = remote
        .client
        .create_workspace("remote-link", Vec::new())
        .unwrap();
    let remote_node = remote.client.node_identity().unwrap();
    let executable = remote.executable.clone();
    let mut local = TestDaemon::start_with(|command, directory| {
        install_fake_ssh(directory, &executable, &remote);
        command.env("PATH", format!("{}:/usr/bin:/bin", directory.display()));
    });
    local
        .client
        .add_node_registration("work", "fake-target", &remote_node)
        .unwrap();
    wait_until(
        || {
            local
                .client
                .combined_node_snapshot(None)
                .ok()
                .and_then(|snapshot| {
                    snapshot
                        .nodes
                        .into_iter()
                        .find(|node| node.node_id == remote_node)
                })
                .is_some_and(|node| node.workspace_owner_eligible)
        },
        "remote Node did not become eligible for Workspace placement",
    );

    let colliding_local_owner = local
        .client
        .create_workspace("remote-adopt", Vec::new())
        .unwrap();

    for arguments in [
        vec!["shell", "create", "remote-adopt", "--node", "work"],
        vec![
            "launcher",
            "create",
            "unsafe",
            "--workspace",
            "remote-adopt",
            "--node",
            "work",
            "--",
            "true",
        ],
    ] {
        let output = local.command().args(arguments).output().unwrap();
        assert!(!output.status.success());
        assert!(contains(&output.stderr, b"coordinated Workspace"));
    }
    let unchanged_owner = remote.client.get_workspace(&adopted_owner.id).unwrap();
    assert!(unchanged_owner.shells.is_empty());
    assert!(unchanged_owner.launchers.is_empty());
    let unchanged_local_owner = local
        .client
        .get_workspace(&colliding_local_owner.id)
        .unwrap();
    assert!(unchanged_local_owner.shells.is_empty());
    assert!(unchanged_local_owner.launchers.is_empty());

    let adopt = local
        .command()
        .args(["workspace", "adopt", &adopted_owner.id, "--node", "work"])
        .output()
        .unwrap();
    assert!(
        adopt.status.success(),
        "CLI remote adopt failed: {}",
        String::from_utf8_lossy(&adopt.stderr)
    );
    let adopted = local
        .client
        .combined_node_snapshot(None)
        .unwrap()
        .workspaces
        .into_iter()
        .find(|workspace| workspace.name == "remote-adopt")
        .unwrap();
    assert_eq!(adopted.placements[0].workspace_id, adopted_owner.id);

    let linked = local
        .client
        .create_global_workspace("linked-global")
        .unwrap();
    let link = local
        .command()
        .args([
            "workspace",
            "link",
            &linked.id,
            &linked_owner.id,
            "--node",
            "work",
        ])
        .output()
        .unwrap();
    assert!(
        link.status.success(),
        "CLI remote link failed: {}",
        String::from_utf8_lossy(&link.stderr)
    );
    let linked = local
        .client
        .combined_node_snapshot(None)
        .unwrap()
        .workspaces
        .into_iter()
        .find(|workspace| workspace.id == linked.id)
        .unwrap();
    assert_eq!(linked.placements[0].workspace_id, linked_owner.id);

    let ssh = local.runtime_dir.join("ssh");
    let disabled_ssh = local.runtime_dir.join("ssh.disabled");
    fs::rename(&ssh, &disabled_ssh).unwrap();
    assert!(
        local
            .client
            .create_global_workspace_with_shell(
                Uuid::new_v4().to_string(),
                Uuid::new_v4().to_string(),
                "restart-project",
                &remote_node,
                Uuid::new_v4().to_string(),
                "/tmp".into(),
                Uuid::new_v4().to_string(),
                ShellSpec {
                    name: "restart-project".into(),
                    cwd: "/tmp".into(),
                    command: Vec::new(),
                },
            )
            .is_err()
    );
    assert!(
        local
            .client
            .combined_node_snapshot(None)
            .unwrap()
            .workspaces
            .iter()
            .all(|workspace| workspace.name != "restart-project"),
        "preflight route failure must not persist empty global metadata"
    );
    local.crash();
    fs::rename(&disabled_ssh, &ssh).unwrap();
    let local_path = format!("{}:/usr/bin:/bin", local.runtime_dir.display());
    local.restart_with(|command| {
        command.env("PATH", local_path);
    });
    wait_until(
        || {
            local
                .client
                .combined_node_snapshot(None)
                .ok()
                .and_then(|snapshot| {
                    snapshot
                        .nodes
                        .into_iter()
                        .find(|node| node.node_id == remote_node)
                })
                .is_some_and(|node| node.workspace_owner_eligible)
        },
        "remote Node did not recover placement eligibility",
    );
    let (recovered_project, recovered_shell) = local
        .client
        .create_global_workspace_with_shell(
            Uuid::new_v4().to_string(),
            Uuid::new_v4().to_string(),
            "restart-project",
            &remote_node,
            Uuid::new_v4().to_string(),
            "/tmp".into(),
            Uuid::new_v4().to_string(),
            ShellSpec {
                name: "restart-project".into(),
                cwd: "/tmp".into(),
                command: Vec::new(),
            },
        )
        .unwrap();
    assert_eq!(recovered_project.placements.len(), 1);
    assert_eq!(recovered_shell.name, "restart-project");

    let placed = local
        .client
        .create_global_workspace("remote-placed")
        .unwrap();
    let owner_workspace_id = Uuid::new_v4().to_string();
    let (placed, shell) = local
        .client
        .create_global_workspace_shell(
            Uuid::new_v4().to_string(),
            &placed.id,
            placed.revision,
            &remote_node,
            &owner_workspace_id,
            Some("/tmp".into()),
            Uuid::new_v4().to_string(),
            ShellSpec {
                name: "remote-first".into(),
                cwd: "/tmp".into(),
                command: vec!["/bin/sh".into(), "-c".into(), "true".into()],
            },
        )
        .unwrap();
    assert_eq!(shell.workspace_id, owner_workspace_id);
    let opened = local
        .client
        .open_global_workspace(&placed.id, placed.revision)
        .unwrap();
    assert_eq!(opened.placements[0].status, "available");

    let ssh = local.runtime_dir.join("ssh");
    let disabled_ssh = local.runtime_dir.join("ssh.disabled");
    fs::rename(&ssh, &disabled_ssh).unwrap();
    let partial = local
        .client
        .close_global_workspace(&placed.id, placed.revision)
        .unwrap();
    assert_eq!(partial.placements[0].status, "unresolved");
    assert!(partial.workspace.closing);
    fs::rename(&disabled_ssh, &ssh).unwrap();
    let completed = local
        .client
        .retry_global_workspace_close(&placed.id)
        .unwrap();
    assert_eq!(completed.placements[0].status, "closed");
    assert!(
        local
            .client
            .combined_node_snapshot(None)
            .unwrap()
            .workspaces
            .iter()
            .all(|workspace| workspace.id != placed.id)
    );
}

#[test]
fn two_daemon_remote_attachment_keeps_environment_and_runtime_on_owner() {
    let mut remote = TestDaemon::start_with(|command, _| {
        command
            .env("REMOTE_PRIVATE", "owner-only")
            .env("TERM", "remote-daemon-term");
    });
    let workspace = remote
        .client
        .create_workspace(
            "remote-workspace",
            vec![ShellSpec {
                name: "remote-shell".into(),
                cwd: "/tmp".into(),
                command: vec![
                    "/bin/sh".into(),
                    "-c".into(),
                    "printf 'pid=%s remote=%s local=%s term=%s\\n' \"$$\" \"$REMOTE_PRIVATE\" \"${LOCAL_PRIVATE-unset}\" \"$TERM\"; exec cat".into(),
                ],
            }],
        )
        .unwrap();
    let shell = &workspace.shells[0];
    let remote_node = remote.client.node_identity().unwrap();
    let executable = remote.executable.clone();
    let local = TestDaemon::start_with(|command, directory| {
        install_fake_ssh(directory, &executable, &remote);
        command
            .env("PATH", format!("{}:/usr/bin:/bin", directory.display()))
            .env("LOCAL_PRIVATE", "must-not-cross-node");
    });
    local
        .client
        .add_node_registration("work", "fake-target", &remote_node)
        .unwrap();

    let mut attachment = local
        .client
        .attach_node(
            QualifiedIdentity::new(&remote_node, &shell.id),
            true,
            true,
            None,
            profile(),
        )
        .unwrap();
    let reconstruction = std::mem::take(&mut attachment.reconstruction);
    let output = read_until_after(
        &mut attachment.stream,
        b"term=attachment-term",
        reconstruction,
    );
    assert!(contains(&output, b"remote=owner-only"));
    assert!(contains(&output, b"local=unset"));
    let pid = parse_pid(&output, "pid=").expect("remote shell PID");
    let run_id = remote.client.get_shell(&shell.id).unwrap().run.unwrap().id;
    let local_event_cursor = local.client.events(None, 256, 0).unwrap().cursor;

    AttachFrame::Resize {
        rows: 40,
        cols: 120,
        pixel_width: 1200,
        pixel_height: 800,
    }
    .write_to(&mut attachment.stream)
    .unwrap();
    AttachFrame::Input(b"remote-input\n".to_vec())
        .write_to(&mut attachment.stream)
        .unwrap();
    assert!(contains(
        &read_until(&mut attachment.stream, b"remote-input"),
        b"remote-input"
    ));
    AttachFrame::FocusGained
        .write_to(&mut attachment.stream)
        .unwrap();
    let focus_events = local
        .client
        .events(Some(local_event_cursor), 256, 1_000)
        .unwrap();
    assert!(focus_events.events.iter().any(|event| matches!(
        event.kind,
        boomux::protocol::DaemonEventKind::FocusedTerminalPresentationChanged
    )));
    wait_until(
        || {
            remote
                .client
                .focused_terminal()
                .ok()
                .flatten()
                .is_some_and(|focused| focused.shell_id == shell.id)
        },
        "remote owner did not accept the relayed focus frame",
    );
    wait_until(
        || {
            local
                .client
                .combined_node_snapshot(None)
                .ok()
                .and_then(|snapshot| snapshot.focused_terminal)
                .is_some_and(|focused| {
                    focused.shell == QualifiedIdentity::new(&remote_node, &shell.id)
                })
        },
        "remote attachment focus was not presented by the local daemon",
    );
    let focus_before_handoff = local
        .client
        .combined_node_snapshot(None)
        .unwrap()
        .focused_terminal
        .unwrap();
    drop(attachment);
    wait_until(
        || {
            remote.client.get_shell(&shell.id).unwrap().status
                == boomux::protocol::ShellStatus::Running
        },
        "SSH disconnect changed remote shell lifecycle",
    );

    let mut reattached = local
        .client
        .attach_node(
            QualifiedIdentity::new(&remote_node, &shell.id),
            true,
            false,
            None,
            profile(),
        )
        .unwrap();
    let restart = local
        .command()
        .env(
            "PATH",
            format!("{}:/usr/bin:/bin", local.runtime_dir.display()),
        )
        .args(["daemon", "restart"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    assert_eq!(
        AttachFrame::read_from(&mut reattached.stream).unwrap(),
        AttachFrame::Reconnect
    );
    AttachFrame::ReconnectAck
        .write_to(&mut reattached.stream)
        .unwrap();
    assert!(restart.wait_with_output().unwrap().status.success());
    assert_eq!(
        local
            .client
            .combined_node_snapshot(None)
            .unwrap()
            .focused_terminal,
        Some(focus_before_handoff.clone())
    );
    let mut after_handoff = local
        .client
        .attach_node(
            QualifiedIdentity::new(&remote_node, &shell.id),
            true,
            false,
            None,
            profile(),
        )
        .unwrap();
    AttachFrame::FocusGained
        .write_to(&mut after_handoff.stream)
        .unwrap();
    wait_until(
        || {
            local
                .client
                .combined_node_snapshot(None)
                .ok()
                .and_then(|snapshot| snapshot.focused_terminal)
                .is_some_and(|focused| focused.revision > focus_before_handoff.revision)
        },
        "remote focus revision did not advance after local handoff",
    );
    AttachFrame::Input(b"after-local-handoff\n".to_vec())
        .write_to(&mut after_handoff.stream)
        .unwrap();
    let output = read_until(&mut after_handoff.stream, b"after-local-handoff");
    assert!(contains(&output, b"after-local-handoff"));
    assert_eq!(
        remote.client.get_shell(&shell.id).unwrap().run.unwrap().id,
        run_id
    );
    assert!(crate::support::process_exists(pid));

    let remote_restart = remote
        .command()
        .args(["daemon", "restart"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    loop {
        match AttachFrame::read_from(&mut after_handoff.stream).unwrap() {
            AttachFrame::Reconnect => break,
            AttachFrame::Output(_) => {}
            frame => panic!("expected reconnect after queued output, got {frame:?}"),
        }
    }
    AttachFrame::ReconnectAck
        .write_to(&mut after_handoff.stream)
        .unwrap();
    let remote_restart = remote_restart.wait_with_output().unwrap();
    assert!(
        remote_restart.status.success(),
        "remote restart failed: {}",
        String::from_utf8_lossy(&remote_restart.stderr)
    );
    let mut after_remote_handoff = local
        .client
        .attach_node(
            QualifiedIdentity::new(&remote_node, &shell.id),
            true,
            false,
            Some(run_id.clone()),
            profile(),
        )
        .unwrap();
    AttachFrame::Input(b"after-remote-handoff\n".to_vec())
        .write_to(&mut after_remote_handoff.stream)
        .unwrap();
    assert!(contains(
        &read_until(&mut after_remote_handoff.stream, b"after-remote-handoff"),
        b"after-remote-handoff"
    ));
    assert_eq!(
        remote.client.get_shell(&shell.id).unwrap().run.unwrap().id,
        run_id
    );
    assert!(crate::support::process_exists(pid));

    AttachFrame::FocusGained
        .write_to(&mut after_remote_handoff.stream)
        .unwrap();
    wait_until(
        || {
            local
                .client
                .combined_node_snapshot(None)
                .ok()
                .and_then(|snapshot| snapshot.focused_terminal)
                .is_some_and(|focused| {
                    focused.shell == QualifiedIdentity::new(&remote_node, &shell.id)
                })
        },
        "remote Shell did not regain presented focus before close",
    );
    let focused_close = local
        .command()
        .env(
            "PATH",
            format!("{}:/usr/bin:/bin", local.runtime_dir.display()),
        )
        .env_remove("BOOMUX_SHELL_ID")
        .env_remove("BOOMUX_WORKSPACE_ID")
        .args(["close", "--focused"])
        .output()
        .unwrap();
    assert!(
        focused_close.status.success(),
        "focused remote close failed: {}",
        String::from_utf8_lossy(&focused_close.stderr)
    );
    assert!(
        String::from_utf8_lossy(&focused_close.stdout)
            .contains("Closed focused shell remote-shell from remote-workspace")
    );
    assert!(remote.client.get_shell(&shell.id).is_err());

    remote.stop_with_cli();
}

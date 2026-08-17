use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::process::Stdio;

use boomux::protocol::{AttachFrame, QualifiedIdentity, ShellSpec};

use crate::support::{TestDaemon, contains, parse_pid, profile, read_until, wait_until};

fn install_fake_ssh(
    directory: &std::path::Path,
    executable: &std::path::Path,
    remote: &TestDaemon,
) {
    let ssh = directory.join("ssh");
    fs::write(
        &ssh,
        format!(
            "#!/bin/sh\nlast=\nfor arg do last=$arg; done\ncase \"$last\" in\n  *boomux-platform-v1*) printf 'boomux-platform-v1\\0Linux\\0x86_64\\0' ;;\n  *boomux-executables-v1*) printf 'boomux-executables-v1\\0{}\\0' ;;\n  *boomux-install-destination-v1*) printf 'boomux-install-destination-v1\\0{}/boomux\\0' ;;\n  *'__federation-stdio'*) exec env -i PATH=/usr/bin:/bin HOME={} XDG_RUNTIME_DIR={} XDG_STATE_HOME={} {} __federation-stdio ;;\n  *) exit 64 ;;\nesac\n",
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
    let output = read_until(&mut attachment.stream, b"term=attachment-term");
    assert!(contains(&output, b"remote=owner-only"));
    assert!(contains(&output, b"local=unset"));
    let pid = parse_pid(&output, "pid=").expect("remote shell PID");
    let run_id = remote.client.get_shell(&shell.id).unwrap().run.unwrap().id;

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
    assert_eq!(
        AttachFrame::read_from(&mut after_handoff.stream).unwrap(),
        AttachFrame::Reconnect
    );
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

    remote.stop_with_cli();
}

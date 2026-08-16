use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};
use std::os::unix::net::UnixStream;
use std::process::{Command, Stdio};
use std::thread;
use std::time::Duration;

use boomux::protocol::{self, AttachFrame, ErrorCode, ShellRunExitReason, ShellSpec, ShellStatus};
use uuid::Uuid;

use crate::support::{
    TestDaemon, acknowledge_reconnect, assert_generated_name, assert_remote_code, contains,
    parse_pid, process_exists, profile, read_until, wait_for_attach_with_profile, wait_until,
};

fn assert_unsafe_native_clock_rejected(runtime_dir: &std::path::Path, clock: &std::path::Path) {
    let mut child = Command::new(env!("CARGO_BIN_EXE_boomux"))
        .args(["daemon", "run"])
        .env("XDG_RUNTIME_DIR", runtime_dir)
        .env("XDG_STATE_HOME", runtime_dir.join("state"))
        .env("SHELL", "/bin/sh")
        .env("BOOMUX_NATIVE_TEST_HOOKS", "1")
        .env("BOOMUX_NATIVE_TEST_CLOCK", clock)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    wait_until(
        || child.try_wait().unwrap().is_some(),
        "daemon accepted an unsafe native clock path",
    );
    assert!(!child.wait().unwrap().success());
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
    assert_eq!(capabilities["data"]["daemon_protocol_version"], 35);
    assert!(
        capabilities["data"]
            .get("session_transcript_integrations")
            .is_none()
    );
    assert_eq!(
        capabilities["data"]["integration_hosts"]["opencode"]["validated_version"],
        "1.18.15"
    );
    assert_eq!(
        capabilities["data"]["integration_hosts"]["opencode"]["package"],
        "opencode-ai"
    );
    assert_eq!(
        capabilities["data"]["integration_hosts"]["pi"]["validated_version"],
        "0.84.1"
    );
    assert_eq!(
        capabilities["data"]["integration_hosts"]["pi"]["package"],
        "@earendil-works/pi-coding-agent"
    );
    let json_commands = capabilities["data"]["json_commands"].as_array().unwrap();
    for command in [
        "events",
        "project.list",
        "node.snapshot",
        "agent.register",
        "agent.ensure",
        "agent.report",
        "agent.wait",
        "attention.list",
        "attention.acknowledge",
        "integration.list",
        "integration.status",
        "integration.install",
        "integration.verify",
        "schedule.create",
        "schedule.list",
        "schedule.inspect",
        "schedule.pause",
        "schedule.resume",
        "schedule.remove",
    ] {
        assert!(json_commands.iter().any(|current| current == command));
    }
    assert!(
        capabilities["data"]["integration_hosts"]["opencode"]
            .get("schedule_dispatch")
            .is_none()
    );
    let features = capabilities["data"]["features"].as_array().unwrap();
    for feature in [
        "revision_aware_reads",
        "protocol_10",
        "protocol_12",
        "protocol_13",
        "protocol_14",
        "protocol_15",
        "protocol_16",
        "protocol_17",
        "protocol_18",
        "protocol_19",
        "protocol_20",
        "protocol_21",
        "protocol_22",
        "protocol_23",
        "protocol_24",
        "protocol_25",
        "protocol_26",
        "protocol_27",
        "protocol_28",
        "protocol_29",
        "protocol_30",
        "protocol_31",
        "protocol_32",
        "protocol_33",
        "protocol_34",
        "protocol_35",
        "node_registration_management",
        "node_projection_sync",
        "bounded_remote_node_projections",
        "combined_node_snapshot",
        "node_qualified_dashboard",
        "typed_exact_node_routing",
        "guarded_remote_management",
        "remote_pty_attachment",
        "owner_environment_attachment",
        "exact_run_attachment",
        "stable_node_identity",
        "revision_aware_scheduled_execution_wait",
        "bounded_scheduled_execution_history",
        "scheduled_execution_notifications",
        "agent_schedule_management",
        "durable_agent_schedules",
        "timed_schedule_dispatch",
        "scheduler_health",
        "bounded_scheduled_execution_concurrency",
        "workspace_default_cwd",
        "structured_terminal_previews",
        "focused_terminal_read",
        "focused_terminal_following",
        "inactive_agent_state",
        "protocol_11",
        "restartable_exited_shells",
        "idempotent_agent_ensure",
        "agent_authority_precedence",
        "opencode_lifecycle_plugin",
        "process_adapters",
        "persistent_agent_attention",
        "integration_management",
    ] {
        assert!(features.iter().any(|current| current == feature));
    }

    let status = daemon
        .command()
        .args(["daemon", "status"])
        .output()
        .unwrap();
    assert!(status.status.success());
    let status_text = String::from_utf8_lossy(&status.stdout);
    assert!(status_text.contains("running (protocol 35"));
    assert!(status_text.contains("scheduler active (0/4 active executions)"));
    let status = daemon
        .command()
        .args(["daemon", "status", "--json"])
        .output()
        .unwrap();
    let status: serde_json::Value = serde_json::from_slice(&status.stdout).unwrap();
    assert_eq!(status["schema"], "boomux.cli/v1");
    assert_eq!(status["data"]["status"], "running");
    assert_eq!(status["data"]["scheduler"]["state"], "active");
    assert_eq!(status["data"]["scheduler"]["active_executions"], 0);
    assert_eq!(status["data"]["scheduler"]["max_concurrent"], 4);
    for max_concurrent in [0, 65, u16::MAX] {
        let mut stream = UnixStream::connect(daemon.client.socket_path()).unwrap();
        protocol::write_message(
            &mut stream,
            &protocol::Envelope::with_version(
                24,
                protocol::Request::RestartWithNotificationConfig {
                    notifications: protocol::NotificationDeliveryConfig {
                        desktop_enabled: false,
                        sound_enabled: false,
                        blocked: true,
                        completed: true,
                        blocked_sound: "blocked".into(),
                        completed_sound: "completed".into(),
                        resume_agents: true,
                        persist_terminal_history: false,
                        max_scheduled_execution_concurrency: max_concurrent,
                        ..Default::default()
                    },
                    environment: None,
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
    assert_remote_code(&error, ErrorCode::NotFound);
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
        .args(["shell", "create", "cli-renamed", "--cwd"])
        .arg(std::env::temp_dir())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "unnamed shell create failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let generated = daemon
        .client
        .get_workspace(&cli_workspace.id)
        .unwrap()
        .shells
        .into_iter()
        .find(|shell| shell.name != "checks")
        .expect("unnamed shell was not created");
    assert_generated_name(&generated.name);
    assert_eq!(generated.status, ShellStatus::Pending);
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
    assert_remote_code(&error, ErrorCode::ShellStartFailed);
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
    let child_pid =
        parse_pid(&output, "child=").expect("shell did not report background child PID");

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

#[test]
fn node_identity_is_owner_only_and_survives_cold_and_graceful_restart() {
    let mut daemon = TestDaemon::start();
    let node_id = daemon.client.node_identity().unwrap();
    assert_eq!(Uuid::parse_str(&node_id).unwrap().to_string(), node_id);

    let path = daemon.runtime_dir.join("state/boomux/node.json");
    let metadata = fs::symlink_metadata(&path).unwrap();
    assert!(metadata.is_file());
    assert_eq!(metadata.mode() & 0o777, 0o600);

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
    assert_eq!(daemon.client.node_identity().unwrap(), node_id);

    daemon.stop_with_cli();
    daemon.restart();
    assert_eq!(daemon.client.node_identity().unwrap(), node_id);
}

#[test]
fn malformed_node_identity_disables_federation_without_blocking_local_daemon() {
    let daemon = TestDaemon::start_with(|_, runtime_dir| {
        let directory = runtime_dir.join("state/boomux");
        fs::create_dir_all(&directory).unwrap();
        fs::write(directory.join("node.json"), b"not-json").unwrap();
        fs::set_permissions(
            directory.join("node.json"),
            fs::Permissions::from_mode(0o600),
        )
        .unwrap();
    });

    daemon.client.ping().unwrap();
    assert_remote_code(
        &daemon.client.node_identity().unwrap_err(),
        ErrorCode::NodeIdentityUnavailable,
    );
    assert_eq!(
        fs::read(daemon.runtime_dir.join("state/boomux/node.json")).unwrap(),
        b"not-json"
    );
}

#[test]
fn federation_helper_binds_handshake_and_inner_request_to_one_daemon_socket() {
    let daemon = TestDaemon::start();
    let node_id = daemon.client.node_identity().unwrap();

    let mut stream = UnixStream::connect(daemon.client.socket_path()).unwrap();
    protocol::write_message(
        &mut stream,
        &protocol::Envelope::with_version(29, protocol::Request::OpenFederationChannel),
    )
    .unwrap();
    let response: protocol::Envelope<protocol::Response> =
        protocol::read_message(&mut stream).unwrap();
    assert_eq!(
        response,
        protocol::Envelope::with_version(
            29,
            protocol::Response::FederationChannel {
                node_id: node_id.clone(),
            },
        )
    );
    protocol::write_message(
        &mut stream,
        &protocol::Envelope::with_version(29, protocol::Request::Ping),
    )
    .unwrap();
    let response: protocol::Envelope<protocol::Response> =
        protocol::read_message(&mut stream).unwrap();
    assert_eq!(
        response,
        protocol::Envelope::with_version(29, protocol::Response::Pong)
    );

    let mut child = daemon
        .command()
        .arg("__federation-stdio")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let mut stdout = child.stdout.take().unwrap();
    let handshake = boomux::federation::read_handshake(&mut stdout).unwrap();
    assert_eq!(handshake.version, boomux::federation::FEDERATION_VERSION);
    assert_eq!(handshake.node_id, node_id);
    assert_eq!(handshake.core_protocol_version, 35);
    assert_eq!(
        handshake.connection_mode,
        boomux::federation::FederationConnectionMode::AdHoc
    );

    protocol::write_message(
        &mut stdin,
        &protocol::Envelope::with_version(30, protocol::Request::Ping),
    )
    .unwrap();
    drop(stdin);
    let response: protocol::Envelope<protocol::Response> =
        protocol::read_message(&mut stdout).unwrap();
    assert_eq!(
        response,
        protocol::Envelope::with_version(30, protocol::Response::Pong)
    );
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "federation helper failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn federation_helper_emits_no_handshake_for_a_mismatched_state_root() {
    let daemon = TestDaemon::start();
    let mismatched_state = daemon.runtime_dir.join("mismatched-state");
    let output = daemon
        .command()
        .arg("__federation-stdio")
        .env("XDG_STATE_HOME", &mismatched_state)
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .contains("daemon Node identity does not match the helper state root")
    );
}

#[test]
fn node_rekey_is_expected_identity_conditional_and_drains_federation_channels() {
    let daemon = TestDaemon::start();
    let original = daemon.client.node_identity().unwrap();

    let noninteractive = daemon
        .command()
        .args(["node", "rekey"])
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(!noninteractive.status.success());
    assert!(
        String::from_utf8_lossy(&noninteractive.stderr)
            .contains("node rekey requires an interactive terminal")
    );
    assert_eq!(daemon.client.node_identity().unwrap(), original);

    let channel = daemon.client.open_federation_channel().unwrap();

    assert_remote_code(
        &daemon.client.rekey_node(&original).unwrap_err(),
        ErrorCode::Busy,
    );
    assert_eq!(daemon.client.node_identity().unwrap(), original);

    drop(channel);
    let replacement = daemon.client.rekey_node(&original).unwrap();
    assert_ne!(replacement, original);
    assert_eq!(daemon.client.node_identity().unwrap(), replacement);
    assert_remote_code(
        &daemon.client.rekey_node(&original).unwrap_err(),
        ErrorCode::InvalidArgument,
    );
    assert_eq!(daemon.client.node_identity().unwrap(), replacement);

    let identity: serde_json::Value = serde_json::from_slice(
        &fs::read(daemon.runtime_dir.join("state/boomux/node.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(identity["node_id"], replacement);
}

#[test]
fn scheduling_config_is_sampled_and_applied_by_graceful_restart() {
    let config = std::sync::Arc::new(std::sync::Mutex::new(None));
    let config_for_daemon = std::sync::Arc::clone(&config);
    let daemon = TestDaemon::start_with(move |command, runtime_dir| {
        let path = runtime_dir.join("config.toml");
        fs::write(&path, "[scheduling]\nmax_concurrent = 2\n").unwrap();
        *config_for_daemon.lock().unwrap() = Some(path.clone());
        command.env("BOOMUX_CONFIG", path);
    });
    let config = config.lock().unwrap().clone().unwrap();

    assert_eq!(
        daemon
            .client
            .snapshot()
            .unwrap()
            .scheduler
            .unwrap()
            .max_concurrent,
        2
    );
    fs::write(&config, "[scheduling]\nmax_concurrent = 3\n").unwrap();
    assert_eq!(
        daemon
            .client
            .snapshot()
            .unwrap()
            .scheduler
            .unwrap()
            .max_concurrent,
        2
    );

    let doctor = daemon
        .command()
        .env("BOOMUX_CONFIG", &config)
        .arg("doctor")
        .output()
        .unwrap();
    assert!(
        String::from_utf8_lossy(&doctor.stderr).contains(
            "daemon uses max_concurrent=2, config resolves 3; restart daemon after changes"
        )
    );

    let restart = daemon
        .command()
        .env("BOOMUX_CONFIG", &config)
        .args(["daemon", "restart"])
        .output()
        .unwrap();
    assert!(
        restart.status.success(),
        "daemon restart failed: {}",
        String::from_utf8_lossy(&restart.stderr)
    );
    assert_eq!(
        daemon
            .client
            .snapshot()
            .unwrap()
            .scheduler
            .unwrap()
            .max_concurrent,
        3
    );
}

#[test]
fn native_clock_hook_rejects_outside_symlinked_and_unsafe_paths() {
    for case in ["outside", "symlink", "marker-symlink", "unsafe-marker"] {
        let runtime_dir =
            std::env::temp_dir().join(format!("boomux-unsafe-clock-{}-{}", case, Uuid::new_v4()));
        let private_runtime = runtime_dir.join("boomux");
        fs::create_dir_all(&private_runtime).unwrap();
        fs::set_permissions(&private_runtime, fs::Permissions::from_mode(0o700)).unwrap();
        let outside = runtime_dir.join("outside-clock");
        fs::create_dir(&outside).unwrap();
        fs::set_permissions(&outside, fs::Permissions::from_mode(0o700)).unwrap();
        fs::write(outside.join("tick"), "1 1767225600000").unwrap();
        fs::set_permissions(outside.join("tick"), fs::Permissions::from_mode(0o600)).unwrap();
        let clock = match case {
            "outside" => outside.clone(),
            "symlink" => {
                let path = private_runtime.join("clock-link");
                symlink(&outside, &path).unwrap();
                path
            }
            "marker-symlink" => {
                let path = private_runtime.join("clock-marker-link");
                fs::create_dir(&path).unwrap();
                fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
                symlink(outside.join("tick"), path.join("tick")).unwrap();
                path
            }
            "unsafe-marker" => {
                let path = private_runtime.join("clock");
                fs::create_dir(&path).unwrap();
                fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
                fs::write(path.join("tick"), "1 1767225600000").unwrap();
                fs::set_permissions(path.join("tick"), fs::Permissions::from_mode(0o644)).unwrap();
                path
            }
            _ => unreachable!(),
        };
        assert_unsafe_native_clock_rejected(&runtime_dir, &clock);
        fs::remove_dir_all(runtime_dir).unwrap();
    }
}

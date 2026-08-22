use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread;
use std::time::Duration;

use boomux::protocol::{AgentAuthority, AgentRegistrationSpec, AgentReport, AgentState, ShellSpec};
use uuid::Uuid;

use crate::support::{CONTROL_MASTER_PREFIX, TestDaemon, process_exists, profile, wait_until};

struct RemoteSubscriber {
    daemon: TestDaemon,
    capture: PathBuf,
    sound_capture: PathBuf,
    sound_player: PathBuf,
    config: PathBuf,
    path: std::ffi::OsString,
}

#[test]
fn desktop_notifications_are_deduplicated_private_and_survive_handoff() {
    let (daemon, capture, notify_send, sound_capture) = start_with_notifications();
    let workspace = daemon
        .client
        .create_workspace(
            "notification-workspace",
            vec![ShellSpec::login("notification-shell", std::env::temp_dir())],
        )
        .unwrap();
    let shell_id = workspace.shells[0].id.clone();
    let attachment = daemon.client.attach(&shell_id, false, profile()).unwrap();
    let run_id = daemon.client.get_shell(&shell_id).unwrap().run.unwrap().id;
    let agent = daemon
        .client
        .register_agent(
            &shell_id,
            &run_id,
            AgentRegistrationSpec {
                name: "notification-agent".into(),
                integration: "native-test".into(),
                external_session_id: Some("PRIVATE-SESSION".into()),
                report: AgentReport {
                    state: AgentState::Blocked,
                    authority: AgentAuthority::LifecycleIntegration,
                    evidence: "PRIVATE-EVIDENCE".into(),
                    confidence: 90,
                },
            },
        )
        .unwrap();
    wait_until(
        || captured_notification_count(&capture) == 1,
        "blocked notification was not delivered",
    );
    wait_until(
        || captured_notification_count(&sound_capture) == 1,
        "blocked notification sound was not delivered",
    );
    let captured = fs::read(&capture).unwrap();
    assert!(
        !captured
            .windows(b"PRIVATE-EVIDENCE".len())
            .any(|value| value == b"PRIVATE-EVIDENCE")
    );
    assert!(
        !captured
            .windows(b"PRIVATE-SESSION".len())
            .any(|value| value == b"PRIVATE-SESSION")
    );

    daemon
        .client
        .report_agent(
            &agent.id,
            &run_id,
            AgentReport {
                state: AgentState::Blocked,
                authority: AgentAuthority::LifecycleIntegration,
                evidence: "changed blocker evidence".into(),
                confidence: 95,
            },
        )
        .unwrap();
    thread::sleep(Duration::from_millis(100));
    assert_eq!(captured_notification_count(&capture), 1);
    assert_eq!(captured_notification_count(&sound_capture), 1);

    for (state, evidence, expected) in [
        (AgentState::Working, "resumed", 1),
        (AgentState::Unknown, "ambiguous execution boundary", 1),
        (AgentState::Working, "resumed after boundary", 1),
        (AgentState::Idle, "turn completed", 2),
        (AgentState::Working, "next turn", 2),
        (AgentState::Blocked, "blocked again", 3),
        (AgentState::Done, "completed", 4),
    ] {
        daemon
            .client
            .report_agent(
                &agent.id,
                &run_id,
                AgentReport {
                    state,
                    authority: AgentAuthority::LifecycleIntegration,
                    evidence: evidence.into(),
                    confidence: 95,
                },
            )
            .unwrap();
        wait_until(
            || captured_notification_count(&capture) == expected,
            "notification transition was not delivered",
        );
        wait_until(
            || captured_notification_count(&sound_capture) == expected,
            "notification transition sound was not delivered",
        );
    }

    let hang = daemon.runtime_dir.join("notification-hang");
    let notification_pid = daemon.runtime_dir.join("notification-pid");
    fs::write(&hang, b"").unwrap();
    daemon
        .client
        .register_agent(
            shell_id.clone(),
            run_id.clone(),
            AgentRegistrationSpec {
                name: "handoff-hanging-agent".into(),
                integration: "native-test".into(),
                external_session_id: Some("handoff-hanging-session".into()),
                report: AgentReport {
                    state: AgentState::Blocked,
                    authority: AgentAuthority::LifecycleIntegration,
                    evidence: "exercise notifier shutdown".into(),
                    confidence: 90,
                },
            },
        )
        .unwrap();
    wait_until(
        || notification_pid.is_file(),
        "fake notifier did not enter its hanging state",
    );
    let notification_pid_value = fs::read_to_string(&notification_pid)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    let replacement_config = daemon.runtime_dir.join("replacement-config.toml");
    fs::write(
        &replacement_config,
        "[notifications]\nenabled = true\nblocked = true\ncompleted = true\n[notifications.sound]\nenabled = true\nblocked = \"dialog-warning\"\n",
    )
    .unwrap();
    drop(attachment.stream);
    let mut restart_paths = vec![notify_send.parent().unwrap().to_path_buf()];
    restart_paths.extend(std::env::split_paths(
        &std::env::var_os("PATH").unwrap_or_default(),
    ));
    let restart = daemon
        .command()
        .env("BOOMUX_CONFIG", replacement_config)
        .env("BOOMUX_NOTIFICATION_CAPTURE", &capture)
        .env("BOOMUX_SOUND_CAPTURE", &sound_capture)
        .env("BOOMUX_NOTIFICATION_HANG", &hang)
        .env("BOOMUX_NOTIFICATION_PID", &notification_pid)
        .env("DBUS_SESSION_BUS_ADDRESS", "unix:path=/nonexistent")
        .env("PATH", std::env::join_paths(restart_paths).unwrap())
        .args(["daemon", "restart"])
        .output()
        .unwrap();
    assert!(
        restart.status.success(),
        "{}",
        String::from_utf8_lossy(&restart.stderr)
    );
    wait_until(
        || !process_exists(notification_pid_value),
        "daemon handoff did not reap the active notifier",
    );
    fs::remove_file(hang).unwrap();
    thread::sleep(Duration::from_millis(100));
    assert_eq!(captured_notification_count(&capture), 4);
    daemon
        .client
        .register_agent(
            shell_id.clone(),
            run_id.clone(),
            AgentRegistrationSpec {
                name: "post-handoff-agent".into(),
                integration: "native-test".into(),
                external_session_id: Some("post-handoff-session".into()),
                report: AgentReport {
                    state: AgentState::Blocked,
                    authority: AgentAuthority::LifecycleIntegration,
                    evidence: "post-handoff blocker".into(),
                    confidence: 90,
                },
            },
        )
        .unwrap();
    wait_until(
        || captured_notification_count(&capture) == 5,
        "notifications stopped after daemon handoff",
    );
    wait_until(
        || captured_notification_count(&sound_capture) == 5,
        "notification sounds stopped after daemon handoff",
    );
    assert!(
        fs::read(&sound_capture)
            .unwrap()
            .windows(b"dialog-warning".len())
            .any(|value| value == b"dialog-warning"),
        "daemon handoff did not resample sound configuration"
    );

    fs::remove_file(notify_send).unwrap();
    let sound_count = captured_notification_count(&sound_capture);
    daemon
        .client
        .register_agent(
            shell_id,
            run_id,
            AgentRegistrationSpec {
                name: "missing-notify-send-agent".into(),
                integration: "native-test".into(),
                external_session_id: Some("missing-notify-send-session".into()),
                report: AgentReport {
                    state: AgentState::Blocked,
                    authority: AgentAuthority::LifecycleIntegration,
                    evidence: "delivery must fail open".into(),
                    confidence: 90,
                },
            },
        )
        .unwrap();
    thread::sleep(Duration::from_millis(100));
    assert_eq!(captured_notification_count(&capture), 5);
    wait_until(
        || captured_notification_count(&sound_capture) == sound_count + 1,
        "sound did not survive desktop notification failure",
    );
}

#[test]
fn notification_test_command_plays_the_configured_sound() {
    let executable = PathBuf::from(env!("CARGO_BIN_EXE_boomux"));
    let directory = std::env::temp_dir().join(format!(
        "boomux-sound-test-{}-{}",
        std::process::id(),
        Uuid::new_v4()
    ));
    let bin = directory.join("bin");
    fs::create_dir_all(&bin).unwrap();
    let player = bin.join("canberra-gtk-play");
    let capture = directory.join("sound-arguments");
    fs::write(
        &player,
        "#!/bin/sh\nprintf '%s\\0' \"$@\" >> \"$BOOMUX_SOUND_CAPTURE\"\n",
    )
    .unwrap();
    fs::set_permissions(&player, fs::Permissions::from_mode(0o755)).unwrap();
    let config = directory.join("config.toml");
    fs::write(
        &config,
        "[notifications]\nenabled = false\nblocked = true\ncompleted = true\n[notifications.sound]\nenabled = true\nblocked = \"dialog-warning\"\ncompleted = \"complete\"\n",
    )
    .unwrap();
    let mut paths = vec![bin];
    if let Some(path) = std::env::var_os("PATH") {
        paths.extend(std::env::split_paths(&path));
    }
    let path = std::env::join_paths(paths).unwrap();

    for reason in ["blocked", "completed"] {
        let output = Command::new(&executable)
            .args(["notification", "test", reason])
            .env("BOOMUX_CONFIG", &config)
            .env("BOOMUX_SOUND_CAPTURE", &capture)
            .env("PATH", &path)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(String::from_utf8_lossy(&output.stdout).contains(reason));
    }

    let arguments = fs::read(&capture)
        .unwrap()
        .split(|byte| *byte == 0)
        .filter(|argument| !argument.is_empty())
        .map(|argument| String::from_utf8(argument.to_vec()).unwrap())
        .collect::<Vec<_>>();
    assert_eq!(
        arguments,
        [
            "--id",
            "dialog-warning",
            "--description",
            "Boomux Agent notification",
            "--id",
            "complete",
            "--description",
            "Boomux Agent notification",
        ]
    );

    fs::write(&player, "#!/bin/sh\nexit 7\n").unwrap();
    let failed = Command::new(&executable)
        .args(["notification", "test", "blocked"])
        .env("BOOMUX_CONFIG", &config)
        .env("PATH", &path)
        .output()
        .unwrap();
    assert!(!failed.status.success());
    assert!(String::from_utf8_lossy(&failed.stderr).contains("canberra-gtk-play exited"));
    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn full_notification_queue_is_fail_open_for_agent_lifecycle() {
    let (daemon, _capture, _notify_send, _sound_capture) = start_with_notifications();
    let workspace = daemon
        .client
        .create_workspace(
            "queue-workspace",
            vec![ShellSpec::login("queue-shell", std::env::temp_dir())],
        )
        .unwrap();
    let shell_id = workspace.shells[0].id.clone();
    let attachment = daemon.client.attach(&shell_id, false, profile()).unwrap();
    let run_id = daemon.client.get_shell(&shell_id).unwrap().run.unwrap().id;
    fs::write(daemon.runtime_dir.join("notification-hang"), b"").unwrap();

    for index in 0..40 {
        let agent = daemon
            .client
            .register_agent(
                &shell_id,
                &run_id,
                AgentRegistrationSpec {
                    name: format!("queued-agent-{index}"),
                    integration: "native-test".into(),
                    external_session_id: Some(format!("queued-session-{index}")),
                    report: AgentReport {
                        state: AgentState::Blocked,
                        authority: AgentAuthority::LifecycleIntegration,
                        evidence: "queue saturation must fail open".into(),
                        confidence: 90,
                    },
                },
            )
            .unwrap();
        assert_eq!(
            daemon
                .client
                .get_agent(&agent.id)
                .unwrap()
                .observation
                .state,
            AgentState::Blocked
        );
    }
    drop(attachment.stream);
}

#[test]
fn remote_notifications_are_independent_digest_once_and_survive_restart() {
    let owner = TestDaemon::start();
    let owner_id = owner.client.node_identity().unwrap();
    let workspace = owner
        .client
        .create_workspace(
            "remote-notification-workspace",
            vec![ShellSpec::login("remote-shell", std::env::temp_dir())],
        )
        .unwrap();
    let shell_id = workspace.shells[0].id.clone();
    let attachment = owner.client.attach(&shell_id, false, profile()).unwrap();
    let run_id = owner.client.get_shell(&shell_id).unwrap().run.unwrap().id;
    let mut first = start_remote_subscriber(&owner);
    let second = start_remote_subscriber(&owner);
    for subscriber in [&first, &second] {
        subscriber
            .daemon
            .client
            .add_node_registration("owner", "fake-owner", &owner_id)
            .unwrap();
        wait_until(
            || {
                subscriber
                    .daemon
                    .client
                    .combined_node_snapshot(Some(owner_id.clone()))
                    .is_ok_and(|snapshot| snapshot.nodes[0].current)
            },
            "remote notification subscriber did not establish its baseline",
        );
        assert_eq!(captured_notification_count(&subscriber.capture), 0);
    }

    let agent = owner
        .client
        .register_agent(
            &shell_id,
            &run_id,
            AgentRegistrationSpec {
                name: "remote-agent".into(),
                integration: "native-test".into(),
                external_session_id: Some("PRIVATE-REMOTE-SESSION".into()),
                report: AgentReport {
                    state: AgentState::Blocked,
                    authority: AgentAuthority::LifecycleIntegration,
                    evidence: "PRIVATE-REMOTE-EVIDENCE".into(),
                    confidence: 90,
                },
            },
        )
        .unwrap();
    for subscriber in [&first, &second] {
        wait_until(
            || captured_notification_count(&subscriber.capture) == 1,
            "independent subscriber did not deliver live remote attention",
        );
        wait_until(
            || captured_notification_count(&subscriber.sound_capture) == 1,
            "independent subscriber did not deliver live remote sound",
        );
        let captured = fs::read(&subscriber.capture).unwrap();
        assert!(!String::from_utf8_lossy(&captured).contains("PRIVATE-REMOTE"));
        assert!(String::from_utf8_lossy(&captured).contains("owner"));
        assert!(String::from_utf8_lossy(&captured).contains(&owner_id));
    }

    owner
        .client
        .report_agent(
            &agent.id,
            &run_id,
            AgentReport {
                state: AgentState::Working,
                authority: AgentAuthority::LifecycleIntegration,
                evidence: "resumed".into(),
                confidence: 90,
            },
        )
        .unwrap();
    let failed_ssh = fs::read(first.daemon.runtime_dir.join("ssh")).unwrap();
    fs::write(first.daemon.runtime_dir.join("ssh"), "#!/bin/sh\nexit 64\n").unwrap();
    wait_until(
        || {
            first
                .daemon
                .client
                .combined_node_snapshot(Some(owner_id.clone()))
                .is_ok_and(|snapshot| !snapshot.nodes[0].current)
        },
        "subscriber did not become disconnected",
    );
    let offline_agent = owner
        .client
        .register_agent(
            &shell_id,
            &run_id,
            AgentRegistrationSpec {
                name: "offline-agent".into(),
                integration: "native-test".into(),
                external_session_id: Some("private-offline-session".into()),
                report: AgentReport {
                    state: AgentState::Blocked,
                    authority: AgentAuthority::LifecycleIntegration,
                    evidence: "private offline blocker".into(),
                    confidence: 90,
                },
            },
        )
        .unwrap();
    owner
        .client
        .report_agent(
            &agent.id,
            &run_id,
            AgentReport {
                state: AgentState::Done,
                authority: AgentAuthority::LifecycleIntegration,
                evidence: "private completion".into(),
                confidence: 90,
            },
        )
        .unwrap();
    fs::write(first.daemon.runtime_dir.join("ssh"), failed_ssh).unwrap();
    wait_until(
        || captured_notification_count(&first.capture) == 2,
        "valid-cursor reconnect did not deliver one digest",
    );
    thread::sleep(Duration::from_millis(1200));
    assert_eq!(captured_notification_count(&first.capture), 2);
    assert!(
        String::from_utf8_lossy(&fs::read(&first.capture).unwrap()).contains("remote activity")
    );
    wait_until(
        || captured_notification_count(&second.capture) == 3,
        "online subscriber did not receive individual remote transitions",
    );

    let before_restart = captured_notification_count(&first.capture);
    first.daemon.crash();
    configure_remote_subscriber_restart(&mut first);
    thread::sleep(Duration::from_millis(1200));
    assert_eq!(captured_notification_count(&first.capture), before_restart);

    let before_handoff = captured_notification_count(&second.capture);
    let restart = second
        .daemon
        .command()
        .env("BOOMUX_CONFIG", &second.config)
        .env("BOOMUX_NOTIFICATION_CAPTURE", &second.capture)
        .env("BOOMUX_SOUND_CAPTURE", &second.sound_capture)
        .env("PATH", &second.path)
        .args(["daemon", "restart"])
        .output()
        .unwrap();
    assert!(
        restart.status.success(),
        "{}",
        String::from_utf8_lossy(&restart.stderr)
    );
    thread::sleep(Duration::from_millis(1200));
    assert_eq!(captured_notification_count(&second.capture), before_handoff);

    fs::remove_file(&first.sound_player).unwrap();
    owner
        .client
        .report_agent(
            &offline_agent.id,
            &run_id,
            AgentReport {
                state: AgentState::Done,
                authority: AgentAuthority::LifecycleIntegration,
                evidence: "private done after sound failure".into(),
                confidence: 90,
            },
        )
        .unwrap();
    wait_until(
        || captured_notification_count(&first.capture) == before_restart + 1,
        "desktop notification did not survive remote sound failure",
    );
    assert_eq!(
        owner
            .client
            .get_agent(&offline_agent.id)
            .unwrap()
            .observation
            .state,
        AgentState::Done
    );
    let cache = fs::read(
        first
            .daemon
            .runtime_dir
            .join("state/boomux/node-cache.json"),
    )
    .unwrap();
    assert!(!String::from_utf8_lossy(&cache).contains("private"));
    drop(attachment.stream);
}

fn start_with_notifications() -> (TestDaemon, PathBuf, PathBuf, PathBuf) {
    let mut capture = None;
    let mut notify_send = None;
    let mut sound_capture = None;
    let daemon = TestDaemon::start_with(|command, runtime_dir| {
        let bin_dir = runtime_dir.join("bin");
        fs::create_dir(&bin_dir).unwrap();
        let notification_command = bin_dir.join("notify-send");
        let sound_player = bin_dir.join("canberra-gtk-play");
        let notification_capture = runtime_dir.join("notifications");
        let sounds = runtime_dir.join("sounds");
        fs::write(
            &notification_command,
            "#!/bin/sh\nif [ -f \"$BOOMUX_NOTIFICATION_HANG\" ]; then\n  printf '%s\\n' \"$$\" > \"$BOOMUX_NOTIFICATION_PID\"\n  exec sleep 10\nfi\nprintf '%s\\0' \"$@\" >> \"$BOOMUX_NOTIFICATION_CAPTURE\"\n",
        )
        .unwrap();
        fs::set_permissions(&notification_command, fs::Permissions::from_mode(0o755)).unwrap();
        fs::write(
            &sound_player,
            "#!/bin/sh\nprintf '%s\\0' \"$@\" >> \"$BOOMUX_SOUND_CAPTURE\"\n",
        )
        .unwrap();
        fs::set_permissions(&sound_player, fs::Permissions::from_mode(0o755)).unwrap();
        let config = runtime_dir.join("config.toml");
        fs::write(
            &config,
            "[notifications]\nenabled = true\nblocked = true\ncompleted = true\n[notifications.sound]\nenabled = true\n",
        )
        .unwrap();
        let mut paths = vec![bin_dir];
        if let Some(path) = std::env::var_os("PATH") {
            paths.extend(std::env::split_paths(&path));
        }
        command
            .env("BOOMUX_CONFIG", config)
            .env("BOOMUX_NOTIFICATION_CAPTURE", &notification_capture)
            .env("BOOMUX_SOUND_CAPTURE", &sounds)
            .env(
                "BOOMUX_NOTIFICATION_HANG",
                runtime_dir.join("notification-hang"),
            )
            .env(
                "BOOMUX_NOTIFICATION_PID",
                runtime_dir.join("notification-pid"),
            )
            .env("DBUS_SESSION_BUS_ADDRESS", "unix:path=/nonexistent")
            .env("PATH", std::env::join_paths(paths).unwrap());
        capture = Some(notification_capture);
        notify_send = Some(notification_command);
        sound_capture = Some(sounds);
    });
    (
        daemon,
        capture.unwrap(),
        notify_send.unwrap(),
        sound_capture.unwrap(),
    )
}

fn start_remote_subscriber(owner: &TestDaemon) -> RemoteSubscriber {
    let mut capture = None;
    let mut sound_capture = None;
    let mut sound_player = None;
    let mut config = None;
    let mut path = None;
    let daemon = TestDaemon::start_with(|command, runtime_dir| {
        let ssh = runtime_dir.join("ssh");
        fs::write(
            &ssh,
            format!(
                "#!/bin/sh\n{CONTROL_MASTER_PREFIX}\nlast=\nfor arg do last=$arg; done\ncase \"$last\" in\n  *boomux-platform-v1*) printf 'boomux-platform-v1\\0Linux\\0x86_64\\0' ;;\n  *boomux-executables-v1*) printf 'boomux-executables-v1\\0{}\\0' ;;\n  *boomux-install-destination-v1*) printf 'boomux-install-destination-v1\\0{}\\0' ;;\n  *__federation-stdio*) exec env XDG_RUNTIME_DIR='{}' XDG_STATE_HOME='{}' '{}' __federation-stdio ;;\n  *) exit 64 ;;\nesac\n",
                owner.executable.display(),
                owner.executable.display(),
                owner.runtime_dir.display(),
                owner.runtime_dir.join("state").display(),
                owner.executable.display(),
            ),
        )
        .unwrap();
        fs::set_permissions(&ssh, fs::Permissions::from_mode(0o700)).unwrap();
        let bin = runtime_dir.join("bin");
        fs::create_dir(&bin).unwrap();
        let notifier = bin.join("notify-send");
        let player = bin.join("canberra-gtk-play");
        let notifications = runtime_dir.join("remote-notifications");
        let sounds = runtime_dir.join("remote-sounds");
        fs::write(
            &notifier,
            "#!/bin/sh\nprintf '%s\\0' \"$@\" >> \"$BOOMUX_NOTIFICATION_CAPTURE\"\n",
        )
        .unwrap();
        fs::write(
            &player,
            "#!/bin/sh\nprintf '%s\\0' \"$@\" >> \"$BOOMUX_SOUND_CAPTURE\"\n",
        )
        .unwrap();
        fs::set_permissions(&notifier, fs::Permissions::from_mode(0o700)).unwrap();
        fs::set_permissions(&player, fs::Permissions::from_mode(0o700)).unwrap();
        let configuration = runtime_dir.join("remote-notifications.toml");
        fs::write(
            &configuration,
            "[notifications]\nenabled = true\nblocked = true\ncompleted = true\nscheduled_dispatch_failed = true\nscheduled_interrupted = true\n[notifications.sound]\nenabled = true\n",
        )
        .unwrap();
        let paths = std::env::join_paths([
            runtime_dir.to_path_buf(),
            bin,
            PathBuf::from("/usr/bin"),
            PathBuf::from("/bin"),
        ])
        .unwrap();
        command
            .env("BOOMUX_CONFIG", &configuration)
            .env("BOOMUX_NOTIFICATION_CAPTURE", &notifications)
            .env("BOOMUX_SOUND_CAPTURE", &sounds)
            .env("PATH", &paths);
        capture = Some(notifications);
        sound_capture = Some(sounds);
        sound_player = Some(player);
        config = Some(configuration);
        path = Some(paths);
    });
    RemoteSubscriber {
        daemon,
        capture: capture.unwrap(),
        sound_capture: sound_capture.unwrap(),
        sound_player: sound_player.unwrap(),
        config: config.unwrap(),
        path: path.unwrap(),
    }
}

fn configure_remote_subscriber_restart(subscriber: &mut RemoteSubscriber) {
    let config = subscriber.config.clone();
    let capture = subscriber.capture.clone();
    let sound_capture = subscriber.sound_capture.clone();
    let path = subscriber.path.clone();
    subscriber.daemon.restart_with(|command| {
        command
            .env("BOOMUX_CONFIG", config)
            .env("BOOMUX_NOTIFICATION_CAPTURE", capture)
            .env("BOOMUX_SOUND_CAPTURE", sound_capture)
            .env("PATH", path);
    });
}

fn captured_notification_count(path: &Path) -> usize {
    fs::read(path)
        .map(|contents| {
            contents
                .split(|byte| *byte == 0)
                .filter(|argument| !argument.is_empty())
                .count()
                / 4
        })
        .unwrap_or(0)
}

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;
use std::thread;
use std::time::{Duration, Instant};

use boomux::client::Client;
use boomux::protocol::{
    self, AgentAuthority, AgentRegistrationSpec, AgentReport, AgentScheduleOverlapPolicy,
    AgentScheduleSession, AgentScheduleSpec, AgentScheduleState, AgentScheduleTrigger,
    AgentScheduleUpdate, AgentState, ScheduledExecutionState, ShellSpec,
};
use uuid::Uuid;

use crate::support::{TestDaemon, assert_remote_code, process_exists, profile, wait_until};

fn schedule_spec(daemon: &TestDaemon, name: &str, prompt: &str) -> AgentScheduleSpec {
    AgentScheduleSpec {
        name: name.into(),
        cwd: daemon.runtime_dir.clone(),
        integration: "opencode".into(),
        prompt: prompt.into(),
        session: AgentScheduleSession::Fresh,
        trigger: AgentScheduleTrigger {
            cron: " 0  2 * * * ".into(),
            timezone: "UTC".into(),
        },
        state: AgentScheduleState::Paused,
        overlap_policy: AgentScheduleOverlapPolicy::Skip,
    }
}

fn write_scheduler_tick(clock: &std::path::Path, generation: u64, now_ms: u64) {
    let tick = clock.join("tick");
    fs::write(&tick, format!("{generation} {now_ms}")).unwrap();
    fs::set_permissions(tick, fs::Permissions::from_mode(0o600)).unwrap();
}

fn create_scheduler_clock(clock: &std::path::Path) {
    fs::create_dir_all(clock.parent().unwrap()).unwrap();
    fs::set_permissions(clock.parent().unwrap(), fs::Permissions::from_mode(0o700)).unwrap();
    fs::create_dir(clock).unwrap();
    fs::set_permissions(clock, fs::Permissions::from_mode(0o700)).unwrap();
}

fn wait_for_scheduler_tick(clock: &std::path::Path, generation: u64) {
    wait_until(
        || {
            fs::read_to_string(clock.join("ack"))
                .is_ok_and(|value| value.trim() == generation.to_string())
        },
        "scheduler did not acknowledge deterministic tick",
    );
}

fn wait_for_scheduler_seen(clock: &std::path::Path, generation: u64) {
    wait_until(
        || {
            fs::read_to_string(clock.join("seen"))
                .is_ok_and(|value| value.trim() == generation.to_string())
        },
        "scheduler did not observe deterministic tick",
    );
}

#[test]
fn schedule_management_is_durable_private_and_process_free() {
    let mut daemon = TestDaemon::start();
    let workspace = daemon
        .client
        .create_workspace("scheduled", Vec::new())
        .unwrap();
    let baseline = daemon.client.events(None, 256, 0).unwrap().cursor;
    let prompt = "private native schedule prompt";
    let schedule = daemon
        .client
        .create_agent_schedule(&workspace.id, schedule_spec(&daemon, "nightly", prompt))
        .unwrap();
    assert_eq!(schedule.trigger.cron, "0 2 * * *");
    assert_eq!(schedule.revision, 1);
    assert!(schedule.execution_shell_id.is_none());

    let snapshot = daemon.client.get_workspace(&workspace.id).unwrap();
    assert!(snapshot.shells.is_empty());
    assert_eq!(snapshot.schedules, std::slice::from_ref(&schedule));
    assert!(!serde_json::to_string(&snapshot).unwrap().contains(prompt));
    let inspection = daemon.client.get_agent_schedule(&schedule.id).unwrap();
    assert_eq!(inspection.prompt, prompt);

    let updated_prompt = "updated private native schedule prompt\n";
    let updated = daemon
        .client
        .update_agent_schedule(
            &schedule.id,
            schedule.revision,
            AgentScheduleUpdate {
                name: "nightly-edited".into(),
                prompt: updated_prompt.into(),
                trigger: AgentScheduleTrigger {
                    cron: "15 3 * * 1-5".into(),
                    timezone: "America/New_York".into(),
                },
            },
        )
        .unwrap();
    assert_eq!(updated.revision, 2);
    assert_eq!(updated.prompt_revision, 2);
    assert_eq!(updated.trigger_revision, 2);
    assert_eq!(updated.name, "nightly-edited");
    assert_eq!(updated.trigger.cron, "15 3 * * 1-5");
    assert_eq!(
        daemon
            .client
            .get_agent_schedule(&schedule.id)
            .unwrap()
            .prompt,
        updated_prompt
    );

    let paused = daemon.client.pause_agent_schedule(&schedule.id).unwrap();
    assert_eq!(paused.revision, 2);
    let resumed = daemon.client.resume_agent_schedule(&schedule.id).unwrap();
    assert_eq!(resumed.revision, 3);
    assert_eq!(
        daemon.client.resume_agent_schedule(&schedule.id).unwrap(),
        resumed
    );
    let events = daemon.client.events(Some(baseline), 256, 0).unwrap();
    assert_eq!(
        events
            .events
            .iter()
            .filter(|event| matches!(
                event.kind,
                protocol::DaemonEventKind::AgentScheduleCreated { .. }
                    | protocol::DaemonEventKind::AgentScheduleUpdated { .. }
                    | protocol::DaemonEventKind::AgentSchedulePaused { .. }
                    | protocol::DaemonEventKind::AgentScheduleResumed { .. }
            ))
            .count(),
        3
    );
    assert!(
        !serde_json::to_string(&events.events)
            .unwrap()
            .contains(updated_prompt)
    );

    let restart = daemon
        .command()
        .args(["daemon", "restart"])
        .output()
        .unwrap();
    assert!(restart.status.success());
    assert_eq!(
        daemon
            .client
            .get_agent_schedule(&schedule.id)
            .unwrap()
            .prompt,
        updated_prompt
    );

    daemon.crash();
    let restart_path = {
        let mut paths = vec![daemon.runtime_dir.join("bin")];
        paths.extend(std::env::split_paths(
            &std::env::var_os("PATH").unwrap_or_default(),
        ));
        std::env::join_paths(paths).unwrap()
    };
    let restart_config = daemon.runtime_dir.join("cold-notifications.toml");
    let notification_capture = daemon.runtime_dir.join("cold-notifications");
    daemon.restart_with(|command| {
        command
            .env("PATH", &restart_path)
            .env("BOOMUX_CONFIG", &restart_config)
            .env("BOOMUX_NOTIFICATION_CAPTURE", &notification_capture);
    });
    assert_eq!(
        daemon
            .client
            .get_agent_schedule(&schedule.id)
            .unwrap()
            .prompt,
        updated_prompt
    );
    assert!(
        daemon.client.snapshot().unwrap().workspaces[0]
            .shells
            .is_empty()
    );

    daemon.client.remove_agent_schedule(&schedule.id).unwrap();
    assert!(daemon.client.get_agent_schedule(&schedule.id).is_err());
    daemon.stop_with_cli();
}

#[test]
fn schedule_cli_is_prompt_private_except_for_safe_explicit_inspection() {
    let mut daemon = TestDaemon::start();
    let workspace = daemon
        .client
        .create_workspace("schedule-cli", Vec::new())
        .unwrap();
    let prompt = "private\nreview\u{1b}]52;c;payload\u{7}";
    let created = daemon
        .command()
        .args([
            "schedule",
            "create",
            "review",
            "--workspace",
            &workspace.id,
            "--cwd",
            daemon.runtime_dir.to_str().unwrap(),
            "--integration",
            "opencode",
            "--prompt",
            prompt,
            "--cron",
            "0 9 * * 1-5",
            "--timezone",
            "UTC",
            "--json",
        ])
        .output()
        .unwrap();
    assert!(
        created.status.success(),
        "{}",
        String::from_utf8_lossy(&created.stderr)
    );
    assert!(!String::from_utf8_lossy(&created.stdout).contains(prompt));
    let created: serde_json::Value = serde_json::from_slice(&created.stdout).unwrap();
    assert_eq!(created["command"], "schedule.create");
    assert_eq!(created["data"]["schedule"]["state"], "paused");
    let schedule_id = created["data"]["schedule"]["id"].as_str().unwrap();

    let listed = daemon
        .command()
        .args(["schedule", "list", "--json"])
        .output()
        .unwrap();
    assert!(listed.status.success());
    assert!(!String::from_utf8_lossy(&listed.stdout).contains(prompt));

    let inspected = daemon
        .command()
        .args(["schedule", "inspect", schedule_id, "--json"])
        .output()
        .unwrap();
    assert!(inspected.status.success());
    let inspected: serde_json::Value = serde_json::from_slice(&inspected.stdout).unwrap();
    assert_eq!(inspected["data"]["schedule"]["prompt"], prompt);

    let human = daemon
        .command()
        .args(["schedule", "inspect", schedule_id])
        .output()
        .unwrap();
    assert!(human.status.success());
    assert!(!human.stdout.contains(&0x1b));
    assert!(String::from_utf8_lossy(&human.stdout).contains("\\u{1b}"));

    let workspace_inspect = daemon
        .command()
        .args(["workspace", "inspect", &workspace.id, "--json"])
        .output()
        .unwrap();
    assert!(workspace_inspect.status.success());
    let workspace_inspect: serde_json::Value =
        serde_json::from_slice(&workspace_inspect.stdout).unwrap();
    assert_eq!(
        workspace_inspect["data"]["workspace"]["schedules"][0]["id"],
        schedule_id
    );
    assert!(
        workspace_inspect["data"]["workspace"]["schedules"][0]
            .get("prompt")
            .is_none()
    );

    daemon.stop_with_cli();
}

#[test]
fn workspace_close_removes_schedules_with_one_close_event() {
    let mut daemon = TestDaemon::start();
    let workspace = daemon
        .client
        .create_workspace("scheduled-close", Vec::new())
        .unwrap();
    let schedule = daemon
        .client
        .create_agent_schedule(
            &workspace.id,
            schedule_spec(&daemon, "nightly", "private close prompt"),
        )
        .unwrap();
    let cursor = daemon.client.events(None, 256, 0).unwrap().cursor;

    daemon.client.close_workspace(&workspace.id).unwrap();

    assert!(daemon.client.get_agent_schedule(&schedule.id).is_err());
    let events = daemon.client.events(Some(cursor), 256, 0).unwrap();
    assert_eq!(events.events.len(), 1);
    assert!(matches!(
        events.events[0].kind,
        protocol::DaemonEventKind::WorkspaceClosed { .. }
    ));
    daemon.stop_with_cli();
}

#[test]
fn old_protocol_filters_schedules_and_invalid_create_does_not_mutate() {
    let mut daemon = TestDaemon::start();
    let workspace = daemon
        .client
        .create_workspace("scheduled-compat", Vec::new())
        .unwrap();
    let first = daemon
        .client
        .create_agent_schedule(
            &workspace.id,
            schedule_spec(&daemon, "first", "private first prompt"),
        )
        .unwrap();
    let baseline = versioned_request(
        &daemon,
        21,
        protocol::Request::Events {
            after: None,
            limit: 256,
            wait_ms: 0,
        },
    );
    let protocol::Response::Events {
        cursor,
        snapshot: Some(snapshot),
        events,
        ..
    } = baseline
    else {
        panic!("expected old-protocol baseline");
    };
    assert!(events.is_empty());
    assert!(snapshot.workspaces[0].schedules.is_empty());

    daemon
        .client
        .create_agent_schedule(
            &workspace.id,
            schedule_spec(&daemon, "second", "private second prompt"),
        )
        .unwrap();
    let filtered = versioned_request(
        &daemon,
        21,
        protocol::Request::Events {
            after: Some(cursor.clone()),
            limit: 256,
            wait_ms: 0,
        },
    );
    let protocol::Response::Events {
        cursor: advanced,
        events,
        ..
    } = filtered
    else {
        panic!("expected old-protocol events");
    };
    assert!(events.is_empty());
    assert!(advanced.event_id > cursor.event_id);

    let mut invalid = schedule_spec(&daemon, "invalid", "private invalid prompt");
    invalid.trigger.cron = "invalid".into();
    let error = daemon
        .client
        .create_agent_schedule(&workspace.id, invalid)
        .unwrap_err();
    assert_remote_code(&error, protocol::ErrorCode::InvalidArgument);
    let current = daemon.client.get_workspace(&workspace.id).unwrap();
    assert_eq!(current.schedules.len(), 2);
    assert_eq!(current.schedules[0].id, first.id);
    daemon.stop_with_cli();
}

fn versioned_request(
    daemon: &TestDaemon,
    version: u32,
    request: protocol::Request,
) -> protocol::Response {
    let mut stream = UnixStream::connect(daemon.client.socket_path()).unwrap();
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

#[test]
fn manual_execution_succeeds_and_duplicate_key_never_spawns_twice() {
    let mut daemon = TestDaemon::start_with(|command, runtime_dir| {
        let bin = runtime_dir.join("bin");
        fs::create_dir(&bin).unwrap();
        let host = bin.join("opencode");
        fs::write(
            &host,
            "#!/bin/sh\nprintf '%s\\0' \"$@\" > \"$BOOMUX_ARGV_CAPTURE\"\nprintf '%s' \"$PWD\" > \"$BOOMUX_CWD_CAPTURE\"\nprintf '%s|%s|%s' \"${BOOMUX_SCHEDULE_RUNNER_TOKEN-unset}\" \"${BOOMUX_DISPATCH_ENV-unset}\" \"${BOOMUX_NATIVE_TEST_HOOKS-unset}\" > \"$BOOMUX_ENV_CAPTURE\"\nprintf 'spawn\\n' >> \"$BOOMUX_EXECUTION_CAPTURE\"\nexit 0\n",
        )
        .unwrap();
        fs::set_permissions(&host, fs::Permissions::from_mode(0o755)).unwrap();
        let mut paths = vec![bin];
        paths.extend(std::env::split_paths(
            &std::env::var_os("PATH").unwrap_or_default(),
        ));
        command
            .env("PATH", std::env::join_paths(paths).unwrap())
            .env("BOOMUX_EXECUTION_CAPTURE", runtime_dir.join("executions"))
            .env("BOOMUX_ARGV_CAPTURE", runtime_dir.join("argv"))
            .env("BOOMUX_CWD_CAPTURE", runtime_dir.join("cwd"))
            .env("BOOMUX_ENV_CAPTURE", runtime_dir.join("environment"))
            .env("BOOMUX_DISPATCH_ENV", "daemon-start");
    });
    let capture = daemon.runtime_dir.join("executions");
    let workspace = daemon
        .client
        .create_workspace("run-now", Vec::new())
        .unwrap();
    let schedule = daemon
        .client
        .create_agent_schedule(
            &workspace.id,
            schedule_spec(&daemon, "manual", "exact prompt"),
        )
        .unwrap();
    let protocol::Response::Events {
        cursor: protocol_22_cursor,
        ..
    } = versioned_request(
        &daemon,
        22,
        protocol::Request::Events {
            after: None,
            limit: 256,
            wait_ms: 0,
        },
    )
    else {
        panic!("expected protocol-22 event baseline");
    };
    let event_cursor = daemon.client.events(None, 256, 0).unwrap().cursor;
    let key = Uuid::new_v4().to_string();
    let first = daemon
        .client
        .run_agent_schedule(&schedule.id, &key)
        .unwrap();
    let duplicate = daemon
        .client
        .run_agent_schedule(&schedule.id, &key)
        .unwrap();
    assert_eq!(first.id, duplicate.id);
    wait_until(
        || {
            daemon
                .client
                .get_scheduled_execution(&first.id)
                .is_ok_and(|execution| execution.state == ScheduledExecutionState::Exited)
        },
        "scheduled execution did not exit",
    );
    wait_until(
        || {
            daemon
                .client
                .events(Some(event_cursor.clone()), 256, 0)
                .is_ok_and(|page| {
                    page.events.iter().any(|event| {
                        matches!(
                            &event.kind,
                            protocol::DaemonEventKind::ScheduledExecutionChanged {
                                execution,
                                ..
                            } if execution.id == first.id
                                && execution.state == ScheduledExecutionState::Exited
                        )
                    })
                })
        },
        "scheduled execution exit event was not published",
    );
    assert_eq!(fs::read_to_string(&capture).unwrap(), "spawn\n");
    assert_eq!(
        fs::read(daemon.runtime_dir.join("argv")).unwrap(),
        b"run\0--\0exact prompt\0"
    );
    assert_eq!(
        fs::read_to_string(daemon.runtime_dir.join("cwd")).unwrap(),
        daemon.runtime_dir.display().to_string()
    );
    assert_eq!(
        fs::read_to_string(daemon.runtime_dir.join("environment")).unwrap(),
        "unset|daemon-start|unset"
    );
    let event_page = daemon.client.events(Some(event_cursor), 256, 0).unwrap();
    assert!(
        !serde_json::to_string(&event_page.events)
            .unwrap()
            .contains("exact prompt")
    );
    let created = event_page
        .events
        .iter()
        .position(|event| {
            matches!(
                &event.kind,
                protocol::DaemonEventKind::ScheduledExecutionCreated { execution, .. }
                    if execution.id == first.id
            )
        })
        .unwrap();
    let run_started = event_page
        .events
        .iter()
        .position(|event| {
            matches!(
                &event.kind,
                protocol::DaemonEventKind::RunStarted { run, .. }
                    if Some(run.id.as_str()) == first.run_id.as_deref()
            )
        })
        .unwrap();
    let active = event_page
        .events
        .iter()
        .position(|event| {
            matches!(
                &event.kind,
                protocol::DaemonEventKind::ScheduledExecutionChanged { execution, .. }
                    if execution.id == first.id
                        && execution.state == ScheduledExecutionState::Active
            )
        })
        .unwrap();
    let run_exited = event_page
        .events
        .iter()
        .position(|event| {
            matches!(
                &event.kind,
                protocol::DaemonEventKind::RunExited { run, .. }
                    if Some(run.id.as_str()) == first.run_id.as_deref()
            )
        })
        .unwrap();
    let exited = event_page
        .events
        .iter()
        .position(|event| {
            matches!(
                &event.kind,
                protocol::DaemonEventKind::ScheduledExecutionChanged { execution, .. }
                    if execution.id == first.id
                        && execution.state == ScheduledExecutionState::Exited
            )
        })
        .unwrap();
    assert!(
        created < run_started && run_started < active && active < run_exited && run_exited < exited
    );
    let revisions = event_page
        .events
        .iter()
        .filter_map(|event| match &event.kind {
            protocol::DaemonEventKind::ScheduledExecutionCreated { execution, .. }
            | protocol::DaemonEventKind::ScheduledExecutionChanged { execution, .. }
                if execution.id == first.id =>
            {
                Some(execution.clone())
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        revisions
            .iter()
            .map(|execution| execution.revision)
            .collect::<Vec<_>>(),
        vec![1, 2, 3, 4, 5, 6]
    );
    assert_eq!(revisions[0].state, ScheduledExecutionState::Claimed);
    assert!(revisions[0].shell_id.is_none());
    assert_eq!(revisions[1].state, ScheduledExecutionState::Claimed);
    assert!(revisions[1].shell_id.is_some());
    assert!(revisions[1].run_id.is_none());
    assert_eq!(revisions[2].state, ScheduledExecutionState::Starting);
    assert!(revisions[2].run_id.is_some());
    assert_eq!(revisions[3].state, ScheduledExecutionState::Active);
    assert!(revisions[3].outcome.is_none());
    assert_eq!(revisions[4].state, ScheduledExecutionState::Active);
    assert!(revisions[4].outcome.is_some());
    assert_eq!(revisions[5].state, ScheduledExecutionState::Exited);
    let second = daemon
        .command()
        .args([
            "schedule",
            "run",
            &schedule.id,
            "--idempotency-key",
            &Uuid::new_v4().to_string(),
            "--json",
        ])
        .env("BOOMUX_DISPATCH_ENV", "run-client")
        .output()
        .unwrap();
    assert!(second.status.success());
    let second: serde_json::Value = serde_json::from_slice(&second.stdout).unwrap();
    let second_id = second["data"]["execution"]["id"].as_str().unwrap();
    wait_until(
        || {
            daemon
                .client
                .get_scheduled_execution(second_id)
                .is_ok_and(|execution| execution.state == ScheduledExecutionState::Exited)
        },
        "second environment execution did not exit",
    );
    assert_eq!(
        fs::read_to_string(daemon.runtime_dir.join("environment")).unwrap(),
        "unset|daemon-start|unset"
    );
    let shell = daemon
        .client
        .get_shell(first.shell_id.as_ref().unwrap())
        .unwrap();
    assert_eq!(shell.command.len(), 3);
    assert!(!shell.command.join(" ").contains("exact prompt"));
    assert!(matches!(
        shell.owner,
        protocol::ShellOwner::Schedule { ref schedule_id } if schedule_id == &schedule.id
    ));
    let protocol::Response::Snapshot { snapshot } =
        versioned_request(&daemon, 22, protocol::Request::Snapshot)
    else {
        panic!("expected protocol-22 snapshot");
    };
    assert!(snapshot.workspaces[0].shells.is_empty());
    assert!(
        snapshot.workspaces[0].schedules[0]
            .execution_shell_id
            .is_none()
    );
    let protocol::Response::Error { code, .. } = versioned_request(
        &daemon,
        22,
        protocol::Request::GetShell {
            shell_id: shell.id.clone(),
        },
    ) else {
        panic!("expected protocol-22 schedule shell to be hidden");
    };
    assert_eq!(code, Some(protocol::ErrorCode::NotFound));
    let protocol::Response::Events { cursor, events, .. } = versioned_request(
        &daemon,
        22,
        protocol::Request::Events {
            after: Some(protocol_22_cursor.clone()),
            limit: 256,
            wait_ms: 0,
        },
    ) else {
        panic!("expected protocol-22 event page");
    };
    assert!(cursor.event_id > protocol_22_cursor.event_id);
    assert!(!events.iter().any(|event| matches!(
        event.kind,
        protocol::DaemonEventKind::ShellCreated { .. }
            | protocol::DaemonEventKind::RunStarted { .. }
            | protocol::DaemonEventKind::OutputChanged { .. }
            | protocol::DaemonEventKind::RunExited { .. }
            | protocol::DaemonEventKind::ScheduledExecutionCreated { .. }
            | protocol::DaemonEventKind::ScheduledExecutionChanged { .. }
    )));
    assert_remote_code(
        &daemon
            .client
            .rename_shell(&shell.id, "renamed")
            .unwrap_err(),
        protocol::ErrorCode::Busy,
    );
    assert_remote_code(
        &daemon.client.close_shell(&shell.id).unwrap_err(),
        protocol::ErrorCode::Busy,
    );
    assert_remote_code(
        &daemon.client.restart_shell(&shell.id).unwrap_err(),
        protocol::ErrorCode::Busy,
    );
    daemon.client.remove_agent_schedule(&schedule.id).unwrap();
    assert!(daemon.client.get_shell(&shell.id).is_err());
    let protocol::Response::Events { events, .. } = versioned_request(
        &daemon,
        22,
        protocol::Request::Events {
            after: Some(protocol_22_cursor),
            limit: 256,
            wait_ms: 0,
        },
    ) else {
        panic!("expected historical protocol-22 event page");
    };
    assert!(!events.iter().any(|event| {
        matches!(
            event.kind,
            protocol::DaemonEventKind::ShellCreated { .. }
                | protocol::DaemonEventKind::RunStarted { .. }
                | protocol::DaemonEventKind::OutputChanged { .. }
                | protocol::DaemonEventKind::RunExited { .. }
        )
    }));
    daemon.stop_with_cli();
}

#[test]
fn pi_dispatch_preserves_exact_argv_stdin_eof_and_continuation_identity() {
    let mut daemon = TestDaemon::start_with(|command, runtime_dir| {
        let bin = runtime_dir.join("bin");
        fs::create_dir(&bin).unwrap();
        let host = bin.join("pi");
        fs::write(
            &host,
            "#!/bin/sh\nprintf '%s\\0' \"$@\" > \"$BOOMUX_PI_ARGV_DIR/$BOOMUX_PI_CASE\"\ncat > \"$BOOMUX_PI_STDIN_DIR/$BOOMUX_PI_CASE\"\nprintf '%s' \"${BOOMUX_SCHEDULE_RUNNER_TOKEN-unset}\" > \"$BOOMUX_PI_TOKEN_DIR/$BOOMUX_PI_CASE\"\n",
        )
        .unwrap();
        fs::set_permissions(&host, fs::Permissions::from_mode(0o755)).unwrap();
        for directory in ["pi-argv", "pi-stdin", "pi-token"] {
            fs::create_dir(runtime_dir.join(directory)).unwrap();
        }
        let mut paths = vec![bin];
        paths.extend(std::env::split_paths(
            &std::env::var_os("PATH").unwrap_or_default(),
        ));
        command
            .env("PATH", std::env::join_paths(paths).unwrap())
            .env("BOOMUX_PI_ARGV_DIR", runtime_dir.join("pi-argv"))
            .env("BOOMUX_PI_STDIN_DIR", runtime_dir.join("pi-stdin"))
            .env("BOOMUX_PI_TOKEN_DIR", runtime_dir.join("pi-token"))
            .env("BOOMUX_PI_CASE", "fresh");
    });
    let workspace = daemon.client.create_workspace("pi", Vec::new()).unwrap();
    let prompt = "-@leading\nsecond line\n";
    let mut fresh_spec = schedule_spec(&daemon, "pi-fresh", prompt);
    fresh_spec.integration = "pi".into();
    let fresh = daemon
        .client
        .create_agent_schedule(&workspace.id, fresh_spec)
        .unwrap();
    let execution = daemon
        .client
        .run_agent_schedule(&fresh.id, Uuid::new_v4().to_string())
        .unwrap();
    wait_until(
        || {
            daemon
                .client
                .get_scheduled_execution(&execution.id)
                .is_ok_and(|execution| execution.state == ScheduledExecutionState::Exited)
        },
        "fresh Pi execution did not exit",
    );
    assert_eq!(
        fs::read(daemon.runtime_dir.join("pi-argv/fresh")).unwrap(),
        b"--print\0"
    );
    assert_eq!(
        fs::read(daemon.runtime_dir.join("pi-stdin/fresh")).unwrap(),
        prompt.as_bytes()
    );
    assert_eq!(
        fs::read_to_string(daemon.runtime_dir.join("pi-token/fresh")).unwrap(),
        "unset"
    );

    let restart = daemon
        .command()
        .args(["daemon", "restart"])
        .env("PATH", {
            let mut paths = vec![daemon.runtime_dir.join("bin")];
            paths.extend(std::env::split_paths(
                &std::env::var_os("PATH").unwrap_or_default(),
            ));
            std::env::join_paths(paths).unwrap()
        })
        .env("BOOMUX_PI_ARGV_DIR", daemon.runtime_dir.join("pi-argv"))
        .env("BOOMUX_PI_STDIN_DIR", daemon.runtime_dir.join("pi-stdin"))
        .env("BOOMUX_PI_TOKEN_DIR", daemon.runtime_dir.join("pi-token"))
        .env("BOOMUX_PI_CASE", "continue")
        .output()
        .unwrap();
    assert!(restart.status.success());
    let external_id = "exact-pi-session";
    let mut continuation_spec = schedule_spec(&daemon, "pi-continue", "continued");
    continuation_spec.integration = "pi".into();
    continuation_spec.session = AgentScheduleSession::Continue {
        external_session_id: external_id.into(),
    };
    let continuation = daemon
        .client
        .create_agent_schedule(&workspace.id, continuation_spec)
        .unwrap();
    let execution = daemon
        .client
        .run_agent_schedule(&continuation.id, Uuid::new_v4().to_string())
        .unwrap();
    wait_until(
        || {
            daemon
                .client
                .get_scheduled_execution(&execution.id)
                .is_ok_and(|execution| execution.state == ScheduledExecutionState::Exited)
        },
        "continued Pi execution did not exit",
    );
    assert_eq!(
        fs::read(daemon.runtime_dir.join("pi-argv/continue")).unwrap(),
        b"--session\0exact-pi-session\0--print\0"
    );
    assert_eq!(
        fs::read(daemon.runtime_dir.join("pi-stdin/continue")).unwrap(),
        b"continued"
    );
    daemon.stop_with_cli();
}

#[test]
fn inactive_agent_does_not_block_exact_continuation_dispatch() {
    let mut daemon = TestDaemon::start_with(|command, runtime_dir| {
        let bin = runtime_dir.join("bin");
        fs::create_dir(&bin).unwrap();
        let host = bin.join("opencode");
        fs::write(&host, "#!/bin/sh\nexit 0\n").unwrap();
        fs::set_permissions(&host, fs::Permissions::from_mode(0o755)).unwrap();
        let mut paths = vec![bin];
        paths.extend(std::env::split_paths(
            &std::env::var_os("PATH").unwrap_or_default(),
        ));
        command.env("PATH", std::env::join_paths(paths).unwrap());
    });
    let workspace = daemon
        .client
        .create_workspace(
            "inactive-continuation",
            vec![ShellSpec {
                name: "user".into(),
                cwd: daemon.runtime_dir.clone(),
                command: vec!["/bin/sh".into(), "-c".into(), "sleep 30".into()],
            }],
        )
        .unwrap();
    let attachment = daemon
        .client
        .attach(&workspace.shells[0].id, false, profile())
        .unwrap();
    let run_id = daemon
        .client
        .get_shell(&workspace.shells[0].id)
        .unwrap()
        .run
        .unwrap()
        .id;
    daemon
        .client
        .register_agent(
            &workspace.shells[0].id,
            run_id,
            AgentRegistrationSpec {
                name: "inactive".into(),
                integration: "opencode".into(),
                external_session_id: Some("continued-session".into()),
                report: AgentReport {
                    state: AgentState::Inactive,
                    authority: AgentAuthority::LifecycleIntegration,
                    evidence: "inactive".into(),
                    confidence: 100,
                },
            },
        )
        .unwrap();
    let mut spec = schedule_spec(&daemon, "continue-inactive", "continue");
    spec.session = AgentScheduleSession::Continue {
        external_session_id: "continued-session".into(),
    };
    let schedule = daemon
        .client
        .create_agent_schedule(&workspace.id, spec)
        .unwrap();
    let execution = daemon
        .client
        .run_agent_schedule(&schedule.id, Uuid::new_v4().to_string())
        .unwrap();
    wait_until(
        || {
            daemon
                .client
                .get_scheduled_execution(&execution.id)
                .is_ok_and(|execution| execution.state == ScheduledExecutionState::Exited)
        },
        "inactive Agent incorrectly blocked continuation",
    );
    drop(attachment.stream);
    daemon.client.close_workspace(&workspace.id).unwrap();
    daemon.stop_with_cli();
}

#[test]
fn continuation_registration_wins_before_atomic_eligibility_and_creates_no_phantom() {
    let mut daemon = TestDaemon::start_with(|command, runtime_dir| {
        let barrier = runtime_dir.join("continuation-pre-dispatch-barrier");
        fs::create_dir(&barrier).unwrap();
        let bin = runtime_dir.join("bin");
        fs::create_dir(&bin).unwrap();
        let host = bin.join("opencode");
        fs::write(
            &host,
            "#!/bin/sh\n: > \"$BOOMUX_CONTINUATION_RACE_SPAWNED\"\n",
        )
        .unwrap();
        fs::set_permissions(&host, fs::Permissions::from_mode(0o755)).unwrap();
        let mut paths = vec![bin];
        paths.extend(std::env::split_paths(
            &std::env::var_os("PATH").unwrap_or_default(),
        ));
        command
            .env("PATH", std::env::join_paths(paths).unwrap())
            .env("BOOMUX_NATIVE_TEST_PRE_DISPATCH_BARRIER", &barrier)
            .env(
                "BOOMUX_CONTINUATION_RACE_SPAWNED",
                runtime_dir.join("continuation-race-spawned"),
            );
    });
    let workspace = daemon
        .client
        .create_workspace(
            "continuation-race",
            vec![ShellSpec {
                name: "user".into(),
                cwd: daemon.runtime_dir.clone(),
                command: vec!["/bin/sh".into(), "-c".into(), "sleep 30".into()],
            }],
        )
        .unwrap();
    let attachment = daemon
        .client
        .attach(&workspace.shells[0].id, false, profile())
        .unwrap();
    let run_id = daemon
        .client
        .get_shell(&workspace.shells[0].id)
        .unwrap()
        .run
        .unwrap()
        .id;
    let mut spec = schedule_spec(&daemon, "continuation-race", "must not run");
    spec.session = AgentScheduleSession::Continue {
        external_session_id: "exact-racing-session".into(),
    };
    let schedule = daemon
        .client
        .create_agent_schedule(&workspace.id, spec)
        .unwrap();
    let request_client = Client::from_socket_path(daemon.client.socket_path().to_path_buf());
    let schedule_id = schedule.id.clone();
    let request = thread::spawn(move || {
        request_client.run_agent_schedule(schedule_id, Uuid::new_v4().to_string())
    });
    let barrier = daemon.runtime_dir.join("continuation-pre-dispatch-barrier");
    wait_until(
        || barrier.join("waiting").is_file(),
        "continuation pre-dispatch barrier was not reached",
    );
    daemon
        .client
        .register_agent(
            &workspace.shells[0].id,
            run_id,
            AgentRegistrationSpec {
                name: "racing-active-agent".into(),
                integration: "opencode".into(),
                external_session_id: Some("exact-racing-session".into()),
                report: AgentReport {
                    state: AgentState::Working,
                    authority: AgentAuthority::LifecycleIntegration,
                    evidence: "became active after claim".into(),
                    confidence: 100,
                },
            },
        )
        .unwrap();
    let protocol::Response::Events {
        cursor: old_cursor, ..
    } = versioned_request(
        &daemon,
        23,
        protocol::Request::Events {
            after: None,
            limit: 256,
            wait_ms: 0,
        },
    )
    else {
        panic!("expected protocol-23 baseline");
    };
    fs::write(barrier.join("release"), b"release").unwrap();
    let execution = request.join().unwrap().unwrap();
    assert_eq!(execution.state, ScheduledExecutionState::Skipped);
    assert_eq!(
        execution.reason,
        Some(protocol::ScheduledExecutionReason::ActiveSession)
    );
    assert!(execution.shell_id.is_none());
    assert!(execution.run_id.is_none());
    assert!(
        daemon
            .client
            .get_agent_schedule(&schedule.id)
            .unwrap()
            .schedule
            .execution_shell_id
            .is_none()
    );
    assert!(
        !daemon
            .runtime_dir
            .join("continuation-race-spawned")
            .exists()
    );
    let old_events = versioned_request(
        &daemon,
        23,
        protocol::Request::Events {
            after: Some(old_cursor.clone()),
            limit: 256,
            wait_ms: 0,
        },
    );
    let protocol::Response::Events { cursor, events, .. } = &old_events else {
        panic!("expected protocol-23 event page");
    };
    assert!(cursor.event_id > old_cursor.event_id);
    assert!(events.is_empty());
    let frozen = serde_json::to_value(old_events).unwrap();
    assert_eq!(frozen["events"], serde_json::json!([]));
    assert!(!frozen.to_string().contains("claimed"));
    assert!(!frozen.to_string().contains("skipped"));
    drop(attachment.stream);
    daemon.client.close_workspace(&workspace.id).unwrap();
    daemon.stop_with_cli();
}

#[test]
fn linked_agents_remain_authoritative_after_exit_and_late_ensure_repairs_links() {
    let mut daemon = TestDaemon::start_with(|command, runtime_dir| {
        let bin = runtime_dir.join("bin");
        fs::create_dir(&bin).unwrap();
        let host = bin.join("opencode");
        fs::write(
            &host,
            "#!/bin/sh\nif [ ! -e \"$BOOMUX_FIRST_AGENT\" ]; then\n  : > \"$BOOMUX_FIRST_AGENT\"\n  \"$BOOMUX_TEST_EXECUTABLE\" agent ensure scheduled --integration opencode --external-session-id linked-session --state working --authority lifecycle-integration --evidence running --confidence 100 >/dev/null\nfi\n",
        )
        .unwrap();
        fs::set_permissions(&host, fs::Permissions::from_mode(0o755)).unwrap();
        let mut paths = vec![bin];
        paths.extend(std::env::split_paths(
            &std::env::var_os("PATH").unwrap_or_default(),
        ));
        command
            .env("PATH", std::env::join_paths(paths).unwrap())
            .env("BOOMUX_TEST_EXECUTABLE", env!("CARGO_BIN_EXE_boomux"))
            .env("BOOMUX_FIRST_AGENT", runtime_dir.join("first-agent"));
    });
    let workspace = daemon
        .client
        .create_workspace("agent-link", Vec::new())
        .unwrap();
    let schedule = daemon
        .client
        .create_agent_schedule(
            &workspace.id,
            schedule_spec(&daemon, "agent-link", "private"),
        )
        .unwrap();
    let cursor = daemon.client.events(None, 256, 0).unwrap().cursor;
    let first = daemon
        .client
        .run_agent_schedule(&schedule.id, Uuid::new_v4().to_string())
        .unwrap();
    wait_until(
        || {
            daemon
                .client
                .get_scheduled_execution(&first.id)
                .is_ok_and(|execution| {
                    execution.state == ScheduledExecutionState::Exited
                        && execution.agent_id.is_some()
                })
        },
        "first execution did not exit with an Agent link",
    );
    let first = daemon.client.get_scheduled_execution(&first.id).unwrap();
    let agent = daemon
        .client
        .get_agent(first.agent_id.as_ref().unwrap())
        .unwrap();
    assert_eq!(agent.observation.state, AgentState::Working);
    assert!(agent.ended_at_ms.is_none());

    let second = daemon
        .client
        .run_agent_schedule(&schedule.id, Uuid::new_v4().to_string())
        .unwrap();
    wait_until(
        || {
            daemon
                .client
                .get_scheduled_execution(&second.id)
                .is_ok_and(|execution| execution.state == ScheduledExecutionState::Exited)
        },
        "second execution did not exit",
    );
    let second = daemon.client.get_scheduled_execution(&second.id).unwrap();
    assert!(second.agent_id.is_none());
    let late = daemon
        .client
        .ensure_agent(
            second.shell_id.clone().unwrap(),
            second.run_id.clone().unwrap(),
            AgentRegistrationSpec {
                name: "late".into(),
                integration: "opencode".into(),
                external_session_id: Some("late-session".into()),
                report: AgentReport {
                    state: AgentState::Idle,
                    authority: AgentAuthority::LifecycleIntegration,
                    evidence: "late registration".into(),
                    confidence: 100,
                },
            },
        )
        .unwrap();
    let linked = daemon.client.get_scheduled_execution(&second.id).unwrap();
    assert_eq!(linked.agent_id.as_deref(), Some(late.id.as_str()));
    assert_eq!(linked.state, ScheduledExecutionState::Exited);
    let events = daemon.client.events(Some(cursor), 256, 0).unwrap();
    assert!(
        !events
            .events
            .iter()
            .any(|event| matches!(event.kind, protocol::DaemonEventKind::AgentCompleted { .. }))
    );
    daemon.stop_with_cli();
}

#[test]
fn host_spawn_failure_is_distinct_from_process_exit() {
    let mut daemon = TestDaemon::start_with(|command, runtime_dir| {
        let bin = runtime_dir.join("bin");
        fs::create_dir(&bin).unwrap();
        let notify = bin.join("notify-send");
        fs::write(
            &notify,
            "#!/bin/sh\nprintf '%s\\n' \"$*\" >> \"$BOOMUX_NOTIFICATION_CAPTURE\"\n",
        )
        .unwrap();
        fs::set_permissions(&notify, fs::Permissions::from_mode(0o755)).unwrap();
        let config = runtime_dir.join("scheduled-notifications.toml");
        fs::write(
            &config,
            "[notifications]\nenabled = true\nscheduled_dispatch_failed = true\n",
        )
        .unwrap();
        command.env("PATH", bin).env("BOOMUX_CONFIG", config).env(
            "BOOMUX_NOTIFICATION_CAPTURE",
            runtime_dir.join("scheduled-notifications"),
        );
    });
    let workspace = daemon
        .client
        .create_workspace("spawn-fail", Vec::new())
        .unwrap();
    let schedule = daemon
        .client
        .create_agent_schedule(&workspace.id, schedule_spec(&daemon, "fail", "private"))
        .unwrap();
    let execution = daemon
        .client
        .run_agent_schedule(&schedule.id, Uuid::new_v4().to_string())
        .unwrap();
    wait_until(
        || {
            daemon
                .client
                .get_scheduled_execution(&execution.id)
                .is_ok_and(|execution| execution.state == ScheduledExecutionState::DispatchFailed)
        },
        "host spawn failure was not recorded",
    );
    let notification_capture = daemon.runtime_dir.join("scheduled-notifications");
    wait_until(
        || notification_capture.is_file(),
        "dispatch failure notification was not delivered",
    );
    let notification = fs::read_to_string(notification_capture).unwrap();
    assert_eq!(notification.lines().count(), 1);
    assert!(notification.contains(&execution.id));
    assert!(!notification.contains("private"));
    let failed = daemon
        .client
        .get_scheduled_execution(&execution.id)
        .unwrap();
    let late = daemon
        .client
        .ensure_agent(
            failed.shell_id.clone().unwrap(),
            failed.run_id.clone().unwrap(),
            AgentRegistrationSpec {
                name: "late spawn-failure Agent".into(),
                integration: "opencode".into(),
                external_session_id: Some("late-spawn-failure-session".into()),
                report: AgentReport {
                    state: AgentState::Idle,
                    authority: AgentAuthority::LifecycleIntegration,
                    evidence: "late exact link".into(),
                    confidence: 100,
                },
            },
        )
        .unwrap();
    assert_eq!(
        daemon
            .client
            .get_scheduled_execution(&execution.id)
            .unwrap()
            .agent_id
            .as_deref(),
        Some(late.id.as_str())
    );
    assert_eq!(
        daemon
            .client
            .run_agent_schedule(&schedule.id, &execution.dispatch_key)
            .unwrap()
            .id,
        execution.id
    );
    thread::sleep(Duration::from_millis(100));
    assert_eq!(
        fs::read_to_string(daemon.runtime_dir.join("scheduled-notifications"))
            .unwrap()
            .lines()
            .count(),
        1
    );
    daemon.stop_with_cli();
}

#[test]
fn runner_exit_without_a_terminal_report_interrupts_the_execution() {
    let mut daemon = TestDaemon::start_with(|command, runtime_dir| {
        let bin = runtime_dir.join("bin");
        fs::create_dir(&bin).unwrap();
        let host = bin.join("opencode");
        fs::write(&host, "#!/bin/sh\nkill -KILL \"$PPID\"\n").unwrap();
        fs::set_permissions(&host, fs::Permissions::from_mode(0o755)).unwrap();
        let mut paths = vec![bin];
        paths.extend(std::env::split_paths(
            &std::env::var_os("PATH").unwrap_or_default(),
        ));
        command.env("PATH", std::env::join_paths(paths).unwrap());
    });
    let workspace = daemon
        .client
        .create_workspace("runner-exit", Vec::new())
        .unwrap();
    let schedule = daemon
        .client
        .create_agent_schedule(
            &workspace.id,
            schedule_spec(&daemon, "runner-exit", "private"),
        )
        .unwrap();
    let execution = daemon
        .client
        .run_agent_schedule(&schedule.id, Uuid::new_v4().to_string())
        .unwrap();

    wait_until(
        || {
            daemon
                .client
                .get_scheduled_execution(&execution.id)
                .is_ok_and(|execution| execution.state == ScheduledExecutionState::Interrupted)
        },
        "runner exit did not reconcile the scheduled execution",
    );
    daemon.stop_with_cli();
}

#[test]
fn execution_cli_json_shapes_are_stable_and_prompt_free() {
    let mut daemon = TestDaemon::start_with(|command, _| {
        command.env("PATH", "/nonexistent");
    });
    let workspace = daemon
        .client
        .create_workspace("execution-cli", Vec::new())
        .unwrap();
    let prompt = "PRIVATE CLI EXECUTION PROMPT";
    let schedule = daemon
        .client
        .create_agent_schedule(&workspace.id, schedule_spec(&daemon, "cli", prompt))
        .unwrap();
    let key = Uuid::new_v4().to_string();
    let run = daemon
        .command()
        .args([
            "schedule",
            "run",
            &schedule.id,
            "--idempotency-key",
            &key,
            "--json",
        ])
        .output()
        .unwrap();
    assert!(
        run.status.success(),
        "{}",
        String::from_utf8_lossy(&run.stderr)
    );
    assert!(!String::from_utf8_lossy(&run.stdout).contains(prompt));
    let run: serde_json::Value = serde_json::from_slice(&run.stdout).unwrap();
    assert_eq!(run["command"], "schedule.run");
    assert_eq!(run["data"]["execution"]["dispatch_key"], key);
    let execution_id = run["data"]["execution"]["id"].as_str().unwrap();
    wait_until(
        || {
            daemon
                .client
                .get_scheduled_execution(execution_id)
                .is_ok_and(|execution| execution.state == ScheduledExecutionState::DispatchFailed)
        },
        "CLI execution did not record spawn failure",
    );
    for (arguments, expected) in [
        (
            vec!["execution", "list", "--schedule", &schedule.id, "--json"],
            "execution.list",
        ),
        (
            vec!["execution", "inspect", execution_id, "--json"],
            "execution.inspect",
        ),
        (
            vec!["execution", "cancel", execution_id, "--json"],
            "execution.cancel",
        ),
    ] {
        let output = daemon.command().args(arguments).output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(!String::from_utf8_lossy(&output.stdout).contains(prompt));
        let output: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(output["command"], expected);
        if expected == "execution.inspect" {
            assert!(output["data"]["execution"]["cwd"].is_string());
            assert_eq!(output["data"]["execution"]["reason"], "host_spawn_failed");
            assert!(output["data"]["execution"]["outcome"].is_null());
            assert!(output["data"]["execution"]["agent_id"].is_null());
            assert!(output["data"]["execution"]["revision"].as_u64().unwrap() > 0);
        } else if expected == "execution.list" {
            assert_eq!(output["data"]["limit"], 100);
            assert_eq!(output["data"]["truncated"], false);
            assert_eq!(output["data"]["schedule_limit"], 100);
            assert_eq!(output["data"]["schedules_truncated"], false);
            assert_eq!(output["data"]["schedules"][0]["schedule_id"], schedule.id);
        }
    }
    let revision = daemon
        .client
        .get_scheduled_execution(execution_id)
        .unwrap()
        .revision
        .to_string();
    let waited = daemon
        .command()
        .args([
            "execution",
            "wait",
            execution_id,
            "--after-revision",
            &revision,
            "--wait-ms",
            "0",
            "--json",
        ])
        .output()
        .unwrap();
    assert!(waited.status.success());
    let waited: serde_json::Value = serde_json::from_slice(&waited.stdout).unwrap();
    assert_eq!(waited["command"], "execution.wait");
    assert_eq!(waited["data"]["changed"], false);
    daemon.stop_with_cli();
}

#[test]
fn active_execution_cancellation_terminates_the_host_process_tree() {
    let mut daemon = TestDaemon::start_with(|command, runtime_dir| {
        let bin = runtime_dir.join("bin");
        fs::create_dir(&bin).unwrap();
        let host = bin.join("opencode");
        fs::write(
            &host,
            "#!/bin/sh\nsleep 30 &\nprintf '%s %s' \"$$\" \"$!\" > \"$BOOMUX_PID_CAPTURE\"\nwait\n",
        )
        .unwrap();
        fs::set_permissions(&host, fs::Permissions::from_mode(0o755)).unwrap();
        let mut paths = vec![bin];
        paths.extend(std::env::split_paths(
            &std::env::var_os("PATH").unwrap_or_default(),
        ));
        command
            .env("PATH", std::env::join_paths(paths).unwrap())
            .env("BOOMUX_PID_CAPTURE", runtime_dir.join("pids"));
    });
    let workspace = daemon
        .client
        .create_workspace("cancel", Vec::new())
        .unwrap();
    let schedule = daemon
        .client
        .create_agent_schedule(&workspace.id, schedule_spec(&daemon, "cancel", "private"))
        .unwrap();
    let execution = daemon
        .client
        .run_agent_schedule(&schedule.id, Uuid::new_v4().to_string())
        .unwrap();
    let pids = daemon.runtime_dir.join("pids");
    wait_until(|| pids.is_file(), "scheduled host did not start");
    wait_until(
        || {
            daemon
                .client
                .get_scheduled_execution(&execution.id)
                .is_ok_and(|execution| execution.state == ScheduledExecutionState::Active)
        },
        "scheduled execution did not become active",
    );
    assert_remote_code(
        &daemon
            .client
            .remove_agent_schedule(&schedule.id)
            .unwrap_err(),
        protocol::ErrorCode::Busy,
    );
    let current = daemon
        .client
        .get_scheduled_execution(&execution.id)
        .unwrap();
    let unauthorized = versioned_request(
        &daemon,
        protocol::PROTOCOL_VERSION,
        protocol::Request::ResolveScheduledExecutionClaim {
            schedule_id: schedule.id.clone(),
            shell_id: current.shell_id.clone().unwrap(),
            run_id: current.run_id.clone().unwrap(),
            runner_token: protocol::ScheduledRunnerCapability::new(Uuid::new_v4().to_string()),
        },
    );
    assert!(matches!(
        &unauthorized,
        protocol::Response::Error {
            code: Some(protocol::ErrorCode::RunChanged),
            ..
        }
    ));
    assert!(
        !serde_json::to_string(&unauthorized)
            .unwrap()
            .contains("private")
    );
    let skipped = daemon
        .client
        .run_agent_schedule(&schedule.id, Uuid::new_v4().to_string())
        .unwrap();
    assert_eq!(skipped.state, ScheduledExecutionState::Skipped);
    assert_eq!(
        skipped.reason,
        Some(protocol::ScheduledExecutionReason::Overlap)
    );
    let old_response = versioned_request(
        &daemon,
        23,
        protocol::Request::RunAgentSchedule {
            schedule_id: schedule.id.clone(),
            dispatch_key: Uuid::new_v4().to_string(),
        },
    );
    assert_eq!(
        serde_json::to_value(old_response).unwrap(),
        serde_json::json!({
            "response": "error",
            "message": "scheduled execution was skipped by the current concurrency policy",
            "code": "busy"
        })
    );
    let pids = fs::read_to_string(pids)
        .unwrap()
        .split_whitespace()
        .map(|pid| pid.parse::<libc::pid_t>().unwrap())
        .collect::<Vec<_>>();
    let cursor = daemon.client.events(None, 256, 0).unwrap().cursor;
    let cancelled = daemon
        .client
        .cancel_scheduled_execution(&execution.id)
        .unwrap();
    assert_eq!(cancelled.state, ScheduledExecutionState::Cancelled);
    wait_until(
        || pids.iter().all(|pid| !process_exists(*pid)),
        "scheduled process tree survived cancellation",
    );
    let events = daemon.client.events(Some(cursor), 256, 0).unwrap().events;
    let run_exited = events
        .iter()
        .position(|event| matches!(event.kind, protocol::DaemonEventKind::RunExited { .. }))
        .unwrap();
    let execution_cancelled = events
        .iter()
        .position(|event| {
            matches!(
                &event.kind,
                protocol::DaemonEventKind::ScheduledExecutionChanged { execution, .. }
                    if execution.state == ScheduledExecutionState::Cancelled
            )
        })
        .unwrap();
    assert!(run_exited < execution_cancelled);
    daemon.stop_with_cli();
}

#[test]
fn cancelling_a_terminal_execution_does_not_stop_the_reused_shells_new_run() {
    let mut daemon = TestDaemon::start_with(|command, runtime_dir| {
        let bin = runtime_dir.join("bin");
        fs::create_dir(&bin).unwrap();
        let host = bin.join("opencode");
        fs::write(
            &host,
            "#!/bin/sh\nif [ ! -e \"$BOOMUX_EXECUTION_CAPTURE\" ]; then : > \"$BOOMUX_EXECUTION_CAPTURE\"; exit 0; fi\nprintf '%s' \"$$\" > \"$BOOMUX_PID_CAPTURE\"\nsleep 30\n",
        )
        .unwrap();
        fs::set_permissions(&host, fs::Permissions::from_mode(0o755)).unwrap();
        let mut paths = vec![bin];
        paths.extend(std::env::split_paths(
            &std::env::var_os("PATH").unwrap_or_default(),
        ));
        command
            .env("PATH", std::env::join_paths(paths).unwrap())
            .env("BOOMUX_EXECUTION_CAPTURE", runtime_dir.join("first"))
            .env("BOOMUX_PID_CAPTURE", runtime_dir.join("pid"));
    });
    let workspace = daemon
        .client
        .create_workspace("cancel-old", Vec::new())
        .unwrap();
    let schedule = daemon
        .client
        .create_agent_schedule(
            &workspace.id,
            schedule_spec(&daemon, "cancel-old", "private"),
        )
        .unwrap();
    let first = daemon
        .client
        .run_agent_schedule(&schedule.id, Uuid::new_v4().to_string())
        .unwrap();
    wait_until(
        || {
            daemon
                .client
                .get_scheduled_execution(&first.id)
                .is_ok_and(|execution| execution.state == ScheduledExecutionState::Exited)
        },
        "first execution did not exit",
    );
    let second = daemon
        .client
        .run_agent_schedule(&schedule.id, Uuid::new_v4().to_string())
        .unwrap();
    let pid_path = daemon.runtime_dir.join("pid");
    wait_until(|| pid_path.is_file(), "second execution did not start");
    let pid = fs::read_to_string(pid_path)
        .unwrap()
        .parse::<libc::pid_t>()
        .unwrap();

    let unchanged = daemon.client.cancel_scheduled_execution(&first.id).unwrap();
    assert_eq!(unchanged.state, ScheduledExecutionState::Exited);
    assert!(process_exists(pid));

    daemon
        .client
        .cancel_scheduled_execution(&second.id)
        .unwrap();
    wait_until(
        || !process_exists(pid),
        "second execution survived cancellation",
    );
    daemon.stop_with_cli();
}

#[test]
fn claimed_cancellation_wins_before_spawn() {
    let mut daemon = TestDaemon::start_with(|command, runtime_dir| {
        let barrier = runtime_dir.join("claim-barrier");
        fs::create_dir(&barrier).unwrap();
        let bin = runtime_dir.join("bin");
        fs::create_dir(&bin).unwrap();
        let host = bin.join("opencode");
        fs::write(&host, "#!/bin/sh\n: > \"$BOOMUX_SPAWN_CAPTURE\"\n").unwrap();
        fs::set_permissions(&host, fs::Permissions::from_mode(0o755)).unwrap();
        let mut paths = vec![bin];
        paths.extend(std::env::split_paths(
            &std::env::var_os("PATH").unwrap_or_default(),
        ));
        command
            .env("PATH", std::env::join_paths(paths).unwrap())
            .env("BOOMUX_NATIVE_TEST_CLAIM_BARRIER", &barrier)
            .env("BOOMUX_SPAWN_CAPTURE", runtime_dir.join("spawned"));
    });
    let workspace = daemon
        .client
        .create_workspace("claim-cancel", Vec::new())
        .unwrap();
    let schedule = daemon
        .client
        .create_agent_schedule(
            &workspace.id,
            schedule_spec(&daemon, "claim-cancel", "private"),
        )
        .unwrap();
    let key = Uuid::new_v4().to_string();
    let client = Client::from_socket_path(daemon.client.socket_path().to_path_buf());
    let schedule_id = schedule.id.clone();
    let key_for_thread = key.clone();
    let run = thread::spawn(move || client.run_agent_schedule(schedule_id, key_for_thread));
    let barrier = daemon.runtime_dir.join("claim-barrier");
    wait_until(
        || barrier.join("claimed").is_file(),
        "claim barrier was not reached",
    );
    let claimed = daemon
        .client
        .scheduled_executions(None, Some(schedule.id.clone()))
        .unwrap()
        .pop()
        .unwrap();
    assert_eq!(claimed.state, ScheduledExecutionState::Claimed);

    let cancelled = daemon
        .client
        .cancel_scheduled_execution(&claimed.id)
        .unwrap();
    assert_eq!(cancelled.state, ScheduledExecutionState::Cancelled);
    fs::write(barrier.join("release"), b"release").unwrap();
    assert_eq!(
        run.join().unwrap().unwrap().state,
        ScheduledExecutionState::Cancelled
    );
    assert!(!daemon.runtime_dir.join("spawned").exists());
    daemon.stop_with_cli();
}

#[test]
fn post_claim_persistence_failure_terminalizes_without_spawning() {
    let mut daemon = TestDaemon::start_with(|command, runtime_dir| {
        let barrier = runtime_dir.join("dispatch-failure-barrier");
        fs::create_dir(&barrier).unwrap();
        let bin = runtime_dir.join("bin");
        fs::create_dir(&bin).unwrap();
        let host = bin.join("opencode");
        fs::write(&host, "#!/bin/sh\n: > \"$BOOMUX_SPAWN_CAPTURE\"\n").unwrap();
        fs::set_permissions(&host, fs::Permissions::from_mode(0o755)).unwrap();
        let mut paths = vec![bin];
        paths.extend(std::env::split_paths(
            &std::env::var_os("PATH").unwrap_or_default(),
        ));
        command
            .env("PATH", std::env::join_paths(paths).unwrap())
            .env("BOOMUX_NATIVE_TEST_CLAIM_BARRIER", &barrier)
            .env(
                "BOOMUX_SPAWN_CAPTURE",
                runtime_dir.join("dispatch-failure-spawn"),
            );
    });
    let workspace = daemon
        .client
        .create_workspace("dispatch-failure", Vec::new())
        .unwrap();
    let schedule = daemon
        .client
        .create_agent_schedule(
            &workspace.id,
            schedule_spec(&daemon, "dispatch-failure", "private"),
        )
        .unwrap();
    let request_client = Client::from_socket_path(daemon.client.socket_path().to_path_buf());
    let schedule_id = schedule.id.clone();
    let request = thread::spawn(move || {
        request_client.run_agent_schedule(schedule_id, Uuid::new_v4().to_string())
    });
    let barrier = daemon.runtime_dir.join("dispatch-failure-barrier");
    wait_until(
        || barrier.join("claimed").is_file(),
        "claim barrier was not reached",
    );
    let execution_id = daemon
        .client
        .scheduled_executions(None, Some(schedule.id.clone()))
        .unwrap()[0]
        .id
        .clone();
    let state_directory = daemon.runtime_dir.join("state/boomux");
    let saved_directory = daemon.runtime_dir.join("saved-dispatch-failure-state");
    fs::rename(&state_directory, &saved_directory).unwrap();
    fs::write(&state_directory, b"not a directory").unwrap();
    fs::write(barrier.join("release"), b"release").unwrap();
    assert!(request.join().unwrap().is_err());
    assert_eq!(
        daemon
            .client
            .get_scheduled_execution(&execution_id)
            .unwrap()
            .state,
        ScheduledExecutionState::DispatchFailed
    );
    assert!(!daemon.runtime_dir.join("dispatch-failure-spawn").exists());

    fs::remove_file(&state_directory).unwrap();
    fs::rename(&saved_directory, &state_directory).unwrap();
    wait_until(
        || {
            daemon
                .client
                .get_scheduled_execution(&execution_id)
                .is_ok_and(|execution| execution.state == ScheduledExecutionState::DispatchFailed)
        },
        "dispatch failure was not retained after persistence recovery",
    );
    daemon.stop_with_cli();
}

#[test]
fn runner_start_persistence_failure_kills_runtime_and_terminalizes() {
    let mut daemon = TestDaemon::start_with(|command, runtime_dir| {
        let barrier = runtime_dir.join("start-barrier");
        fs::create_dir(&barrier).unwrap();
        let bin = runtime_dir.join("bin");
        fs::create_dir(&bin).unwrap();
        let host = bin.join("opencode");
        fs::write(&host, "#!/bin/sh\n: > \"$BOOMUX_HOST_CAPTURE\"\n").unwrap();
        fs::set_permissions(&host, fs::Permissions::from_mode(0o755)).unwrap();
        let mut paths = vec![bin];
        paths.extend(std::env::split_paths(
            &std::env::var_os("PATH").unwrap_or_default(),
        ));
        command
            .env("PATH", std::env::join_paths(paths).unwrap())
            .env("BOOMUX_NATIVE_TEST_START_BARRIER", &barrier)
            .env(
                "BOOMUX_HOST_CAPTURE",
                runtime_dir.join("start-failure-host"),
            );
    });
    let workspace = daemon
        .client
        .create_workspace("start-failure", Vec::new())
        .unwrap();
    let schedule = daemon
        .client
        .create_agent_schedule(
            &workspace.id,
            schedule_spec(&daemon, "start-failure", "private"),
        )
        .unwrap();
    let request_client = Client::from_socket_path(daemon.client.socket_path().to_path_buf());
    let schedule_id = schedule.id.clone();
    let request = thread::spawn(move || {
        request_client.run_agent_schedule(schedule_id, Uuid::new_v4().to_string())
    });
    let barrier = daemon.runtime_dir.join("start-barrier");
    wait_until(
        || barrier.join("started").is_file(),
        "runner start barrier was not reached",
    );
    let state_directory = daemon.runtime_dir.join("state/boomux");
    let saved_directory = daemon.runtime_dir.join("saved-start-failure-state");
    fs::rename(&state_directory, &saved_directory).unwrap();
    fs::write(&state_directory, b"not a directory").unwrap();
    fs::write(barrier.join("release"), b"release").unwrap();
    assert!(request.join().unwrap().is_err());
    let execution_id = daemon
        .client
        .scheduled_executions(None, Some(schedule.id.clone()))
        .unwrap()[0]
        .id
        .clone();
    let failed = daemon
        .client
        .get_scheduled_execution(&execution_id)
        .unwrap();
    assert_eq!(failed.state, ScheduledExecutionState::DispatchFailed);
    assert_eq!(
        daemon
            .client
            .get_shell(failed.shell_id.as_ref().unwrap())
            .unwrap()
            .status,
        protocol::ShellStatus::Pending
    );
    assert!(!daemon.runtime_dir.join("start-failure-host").exists());

    fs::remove_file(&state_directory).unwrap();
    fs::rename(&saved_directory, &state_directory).unwrap();
    daemon.stop_with_cli();
}

#[test]
fn cancellation_persistence_failure_keeps_dead_process_terminal_and_retries() {
    let mut daemon = long_running_schedule_daemon("cancel-persistence");
    let (workspace_id, _schedule_id, execution_id, pid) =
        start_long_running_execution(&daemon, "cancel-persistence");
    let cursor = daemon.client.events(None, 256, 0).unwrap().cursor;
    let state_directory = daemon.runtime_dir.join("state/boomux");
    let saved_directory = daemon.runtime_dir.join("saved-cancel-state");
    fs::rename(&state_directory, &saved_directory).unwrap();
    fs::write(&state_directory, b"not a directory").unwrap();

    assert!(
        daemon
            .client
            .cancel_scheduled_execution(&execution_id)
            .is_err()
    );
    wait_until(
        || !process_exists(pid),
        "cancelled host survived persistence failure",
    );
    let execution = daemon
        .client
        .get_scheduled_execution(&execution_id)
        .unwrap();
    assert_eq!(execution.state, ScheduledExecutionState::Cancelled);
    assert_eq!(
        execution.reason,
        Some(protocol::ScheduledExecutionReason::CancelledByUser)
    );
    assert_eq!(
        daemon.client.get_workspace(&workspace_id).unwrap().shells[0].status,
        protocol::ShellStatus::Pending
    );

    fs::remove_file(&state_directory).unwrap();
    fs::rename(&saved_directory, &state_directory).unwrap();
    wait_until(
        || {
            daemon
                .client
                .events(Some(cursor.clone()), 256, 0)
                .is_ok_and(|events| {
                    events.events.iter().any(|event| {
                        matches!(
                            &event.kind,
                            protocol::DaemonEventKind::ScheduledExecutionChanged { execution, .. }
                                if execution.id == execution_id
                                    && execution.state == ScheduledExecutionState::Cancelled
                        )
                    })
                })
        },
        "cancelled execution was not persisted and published after recovery",
    );
    daemon.stop_with_cli();
}

#[test]
fn cancellation_stop_failure_leaves_live_exact_run_active() {
    let mut daemon = TestDaemon::start_with(|command, runtime_dir| {
        let barrier = runtime_dir.join("cancel-failure-barrier");
        fs::create_dir(&barrier).unwrap();
        let bin = runtime_dir.join("bin");
        fs::create_dir(&bin).unwrap();
        let host = bin.join("opencode");
        fs::write(
            &host,
            "#!/bin/sh\nprintf '%s' \"$$\" > \"$BOOMUX_PID_CAPTURE\"\nwhile [ ! -e \"$BOOMUX_OUTPUT_MARKER\" ]; do sleep 0.01; done\nprintf 'during-cancel-1\\n'\nsleep 0.02\nprintf 'during-cancel-2\\n'\nsleep 30\n",
        )
        .unwrap();
        fs::set_permissions(&host, fs::Permissions::from_mode(0o755)).unwrap();
        let mut paths = vec![bin];
        paths.extend(std::env::split_paths(
            &std::env::var_os("PATH").unwrap_or_default(),
        ));
        command
            .env("PATH", std::env::join_paths(paths).unwrap())
            .env("BOOMUX_PID_CAPTURE", runtime_dir.join("stop-failure-pid"))
            .env("BOOMUX_OUTPUT_MARKER", runtime_dir.join("output-marker"))
            .env("BOOMUX_NATIVE_TEST_CANCEL_STOP_FAILURE", "1")
            .env("BOOMUX_NATIVE_TEST_CANCEL_FAILURE_BARRIER", barrier);
    });
    let (workspace_id, schedule_id, execution_id, pid) =
        start_long_running_execution(&daemon, "stop-failure");
    let active = daemon
        .client
        .get_scheduled_execution(&execution_id)
        .unwrap();
    let shell_id = active.shell_id.clone().unwrap();
    let cursor = daemon.client.events(None, 256, 0).unwrap().cursor;
    let cancel_socket = daemon.client.socket_path().to_path_buf();
    let cancel_execution_id = execution_id.clone();
    let cancel = thread::spawn(move || {
        Client::from_socket_path(cancel_socket).cancel_scheduled_execution(cancel_execution_id)
    });
    let barrier = daemon.runtime_dir.join("cancel-failure-barrier");
    wait_until(
        || barrier.join("reserved").is_file(),
        "cancellation did not reserve lifecycle publication",
    );
    fs::write(daemon.runtime_dir.join("output-marker"), b"output").unwrap();
    wait_until(
        || {
            String::from_utf8_lossy(&daemon.client.read_shell(&shell_id, 1024).unwrap())
                .contains("during-cancel-2")
        },
        "concurrent cancellation output was not read",
    );
    assert!(
        daemon
            .client
            .events(Some(cursor.clone()), 256, 0)
            .unwrap()
            .events
            .iter()
            .all(|event| !matches!(event.kind, protocol::DaemonEventKind::OutputChanged { .. }))
    );
    fs::write(barrier.join("release"), b"release").unwrap();
    assert!(cancel.join().unwrap().is_err());
    let output_revisions = daemon
        .client
        .events(Some(cursor), 256, 0)
        .unwrap()
        .events
        .into_iter()
        .filter_map(|event| match event.kind {
            protocol::DaemonEventKind::OutputChanged {
                shell_id: event_shell_id,
                output_revision,
                ..
            } if event_shell_id == shell_id => Some(output_revision),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(output_revisions.len(), 1);
    assert!(output_revisions.windows(2).all(|pair| pair[0] <= pair[1]));
    assert!(process_exists(pid));
    assert_eq!(
        daemon
            .client
            .get_scheduled_execution(&execution_id)
            .unwrap()
            .state,
        ScheduledExecutionState::Active
    );

    let restart = daemon
        .command()
        .args(["daemon", "restart"])
        .output()
        .unwrap();
    assert!(restart.status.success());
    daemon
        .client
        .cancel_scheduled_execution(&execution_id)
        .unwrap();
    wait_until(
        || !process_exists(pid),
        "host survived cancellation after stop recovery",
    );
    daemon.client.remove_agent_schedule(&schedule_id).unwrap();
    daemon.client.close_workspace(&workspace_id).unwrap();
    daemon.stop_with_cli();
}

#[test]
fn failed_active_workspace_close_reconciles_execution_and_pending_shell() {
    let mut daemon = long_running_schedule_daemon("workspace-close-persistence");
    let (workspace_id, _schedule_id, execution_id, pid) =
        start_long_running_execution(&daemon, "workspace-close-persistence");
    let state_directory = daemon.runtime_dir.join("state/boomux");
    let saved_directory = daemon.runtime_dir.join("saved-workspace-close-state");
    fs::rename(&state_directory, &saved_directory).unwrap();
    fs::write(&state_directory, b"not a directory").unwrap();

    assert!(daemon.client.close_workspace(&workspace_id).is_err());
    wait_until(
        || !process_exists(pid),
        "workspace close did not stop the host",
    );
    let workspace = daemon.client.get_workspace(&workspace_id).unwrap();
    assert_eq!(workspace.shells[0].status, protocol::ShellStatus::Pending);
    let execution = daemon
        .client
        .get_scheduled_execution(&execution_id)
        .unwrap();
    assert_eq!(execution.state, ScheduledExecutionState::Interrupted);
    assert_eq!(execution.outcome, None);

    fs::remove_file(&state_directory).unwrap();
    fs::rename(&saved_directory, &state_directory).unwrap();
    wait_until(
        || {
            daemon
                .client
                .get_scheduled_execution(&execution_id)
                .is_ok_and(|execution| execution.state == ScheduledExecutionState::Interrupted)
        },
        "workspace close compensation was not persisted",
    );
    daemon.client.close_workspace(&workspace_id).unwrap();
    daemon.stop_with_cli();
}

#[test]
fn active_workspace_close_kills_and_removes_scheduled_work() {
    let mut daemon = long_running_schedule_daemon("workspace-close-active");
    let (workspace_id, _schedule_id, execution_id, pid) =
        start_long_running_execution(&daemon, "workspace-close-active");

    daemon.client.close_workspace(&workspace_id).unwrap();
    wait_until(
        || !process_exists(pid),
        "active workspace close left host alive",
    );
    assert!(daemon.client.get_workspace(&workspace_id).is_err());
    assert!(
        daemon
            .client
            .get_scheduled_execution(&execution_id)
            .is_err()
    );
    daemon.stop_with_cli();
}

fn long_running_schedule_daemon(label: &str) -> TestDaemon {
    let label = label.to_owned();
    TestDaemon::start_with(move |command, runtime_dir| {
        let bin = runtime_dir.join("bin");
        fs::create_dir(&bin).unwrap();
        let host = bin.join("opencode");
        fs::write(
            &host,
            "#!/bin/sh\nprintf '%s' \"$$\" > \"$BOOMUX_PID_CAPTURE\"\nsleep 30\n",
        )
        .unwrap();
        fs::set_permissions(&host, fs::Permissions::from_mode(0o755)).unwrap();
        let mut paths = vec![bin];
        paths.extend(std::env::split_paths(
            &std::env::var_os("PATH").unwrap_or_default(),
        ));
        command
            .env("PATH", std::env::join_paths(paths).unwrap())
            .env(
                "BOOMUX_PID_CAPTURE",
                runtime_dir.join(format!("{label}-pid")),
            );
    })
}

fn start_long_running_execution(
    daemon: &TestDaemon,
    label: &str,
) -> (String, String, String, libc::pid_t) {
    let workspace = daemon.client.create_workspace(label, Vec::new()).unwrap();
    let schedule = daemon
        .client
        .create_agent_schedule(&workspace.id, schedule_spec(daemon, label, "private"))
        .unwrap();
    let execution = daemon
        .client
        .run_agent_schedule(&schedule.id, Uuid::new_v4().to_string())
        .unwrap();
    let pid_path = daemon.runtime_dir.join(format!("{label}-pid"));
    wait_until(|| pid_path.is_file(), "scheduled host did not start");
    wait_until(
        || {
            daemon
                .client
                .get_scheduled_execution(&execution.id)
                .is_ok_and(|execution| execution.state == ScheduledExecutionState::Active)
        },
        "scheduled execution did not become active",
    );
    let pid = fs::read_to_string(pid_path)
        .unwrap()
        .parse::<libc::pid_t>()
        .unwrap();
    (workspace.id, schedule.id, execution.id, pid)
}

#[test]
fn transferred_active_executions_block_lower_global_limit_until_release() {
    let mut daemon = TestDaemon::start_with(|command, runtime_dir| {
        let config = runtime_dir.join("max-two.toml");
        fs::write(&config, "[scheduling]\nmax_concurrent = 2\n").unwrap();
        let bin = runtime_dir.join("bin");
        fs::create_dir(&bin).unwrap();
        let host = bin.join("opencode");
        fs::write(
            &host,
            "#!/bin/sh\nprintf '%s\n' \"$$\" >> \"$BOOMUX_BOUND_PIDS\"\nsleep 30\n",
        )
        .unwrap();
        fs::set_permissions(&host, fs::Permissions::from_mode(0o755)).unwrap();
        let mut paths = vec![bin];
        paths.extend(std::env::split_paths(
            &std::env::var_os("PATH").unwrap_or_default(),
        ));
        command
            .env("BOOMUX_CONFIG", config)
            .env("PATH", std::env::join_paths(paths).unwrap())
            .env("BOOMUX_BOUND_PIDS", runtime_dir.join("bound-pids"));
    });
    let mut schedules = Vec::new();
    for index in 1..=3 {
        let workspace = daemon
            .client
            .create_workspace(format!("bound-{index}"), Vec::new())
            .unwrap();
        let schedule = daemon
            .client
            .create_agent_schedule(
                &workspace.id,
                schedule_spec(&daemon, &format!("bound-{index}"), "private"),
            )
            .unwrap();
        schedules.push((workspace, schedule));
    }
    let mut active = Vec::new();
    for (_, schedule) in &schedules[..2] {
        let execution = daemon
            .client
            .run_agent_schedule(&schedule.id, Uuid::new_v4().to_string())
            .unwrap();
        wait_until(
            || {
                daemon
                    .client
                    .get_scheduled_execution(&execution.id)
                    .is_ok_and(|current| current.state == ScheduledExecutionState::Active)
            },
            "bounded execution did not become active",
        );
        active.push(execution.id);
    }
    let mut same_workspace_spec = schedule_spec(&daemon, "same-workspace", "private");
    same_workspace_spec.name = "same-workspace".into();
    let same_workspace = daemon
        .client
        .create_agent_schedule(&schedules[0].0.id, same_workspace_spec)
        .unwrap();
    let skipped = daemon
        .client
        .run_agent_schedule(&same_workspace.id, Uuid::new_v4().to_string())
        .unwrap();
    assert_eq!(
        skipped.reason,
        Some(protocol::ScheduledExecutionReason::WorkspaceCapacity)
    );

    let config = daemon.runtime_dir.join("max-one.toml");
    fs::write(&config, "[scheduling]\nmax_concurrent = 1\n").unwrap();
    let mut paths = vec![daemon.runtime_dir.join("bin")];
    paths.extend(std::env::split_paths(
        &std::env::var_os("PATH").unwrap_or_default(),
    ));
    let restart = daemon
        .command()
        .env("BOOMUX_CONFIG", &config)
        .env("PATH", std::env::join_paths(paths).unwrap())
        .env("BOOMUX_BOUND_PIDS", daemon.runtime_dir.join("bound-pids"))
        .args(["daemon", "restart"])
        .output()
        .unwrap();
    assert!(restart.status.success());
    let health = daemon.client.snapshot().unwrap().scheduler.unwrap();
    assert_eq!(health.max_concurrent, 1);
    assert_eq!(health.active_executions, 2);
    let blocked = daemon
        .client
        .run_agent_schedule(&schedules[2].1.id, Uuid::new_v4().to_string())
        .unwrap();
    assert_eq!(
        blocked.reason,
        Some(protocol::ScheduledExecutionReason::GlobalCapacity)
    );

    for execution_id in &active {
        daemon
            .client
            .cancel_scheduled_execution(execution_id)
            .unwrap();
    }
    let admitted = daemon
        .client
        .run_agent_schedule(&schedules[2].1.id, Uuid::new_v4().to_string())
        .unwrap();
    assert_ne!(admitted.state, ScheduledExecutionState::Skipped);
    daemon
        .client
        .cancel_scheduled_execution(&admitted.id)
        .unwrap();
    for (workspace, _) in schedules {
        daemon.client.close_workspace(&workspace.id).unwrap();
    }
    daemon.stop_with_cli();
}

#[test]
fn cold_recovery_interrupts_active_execution_without_respawn() {
    let mut daemon = TestDaemon::start_with(|command, runtime_dir| {
        let config = runtime_dir.join("cold-notifications.toml");
        fs::write(
            &config,
            "[notifications]\nenabled = true\nscheduled_interrupted = true\n",
        )
        .unwrap();
        let bin = runtime_dir.join("bin");
        fs::create_dir(&bin).unwrap();
        let host = bin.join("opencode");
        fs::write(
            &host,
            "#!/bin/sh\nprintf 'spawn\\n' >> \"$BOOMUX_EXECUTION_CAPTURE\"\nprintf '%s\\n' \"$$\" >> \"$BOOMUX_PID_CAPTURE\"\nsleep 30\n",
        )
        .unwrap();
        fs::set_permissions(&host, fs::Permissions::from_mode(0o755)).unwrap();
        let notify = bin.join("notify-send");
        fs::write(
            &notify,
            "#!/bin/sh\nprintf '%s\\n' \"$*\" >> \"$BOOMUX_COLD_NOTIFICATION_CAPTURE\"\n",
        )
        .unwrap();
        fs::set_permissions(&notify, fs::Permissions::from_mode(0o755)).unwrap();
        let mut paths = vec![bin];
        paths.extend(std::env::split_paths(
            &std::env::var_os("PATH").unwrap_or_default(),
        ));
        command
            .env("PATH", std::env::join_paths(paths).unwrap())
            .env("BOOMUX_CONFIG", config)
            .env("BOOMUX_EXECUTION_CAPTURE", runtime_dir.join("executions"))
            .env("BOOMUX_PID_CAPTURE", runtime_dir.join("pids"))
            .env(
                "BOOMUX_COLD_NOTIFICATION_CAPTURE",
                runtime_dir.join("cold-notifications"),
            );
    });
    let workspace = daemon.client.create_workspace("cold", Vec::new()).unwrap();
    let schedule = daemon
        .client
        .create_agent_schedule(&workspace.id, schedule_spec(&daemon, "cold", "private"))
        .unwrap();
    let execution = daemon
        .client
        .run_agent_schedule(&schedule.id, Uuid::new_v4().to_string())
        .unwrap();
    let capture = daemon.runtime_dir.join("executions");
    wait_until(|| capture.is_file(), "scheduled host did not start");
    let active = daemon
        .client
        .get_scheduled_execution(&execution.id)
        .unwrap();
    let old_pid = fs::read_to_string(daemon.runtime_dir.join("pids"))
        .unwrap()
        .lines()
        .next()
        .unwrap()
        .parse::<libc::pid_t>()
        .unwrap();
    daemon.crash();
    wait_until(
        || !process_exists(old_pid),
        "cold-crashed host remained alive",
    );
    let mut restart_paths = vec![daemon.runtime_dir.join("bin")];
    restart_paths.extend(std::env::split_paths(
        &std::env::var_os("PATH").unwrap_or_default(),
    ));
    let restart_path = std::env::join_paths(restart_paths).unwrap();
    let restart_config = daemon.runtime_dir.join("cold-notifications.toml");
    let restart_notifications = daemon.runtime_dir.join("cold-notifications");
    daemon.restart_with(move |command| {
        command
            .env("PATH", restart_path)
            .env("BOOMUX_CONFIG", restart_config)
            .env("BOOMUX_COLD_NOTIFICATION_CAPTURE", restart_notifications);
    });
    let recovered = daemon
        .client
        .get_scheduled_execution(&execution.id)
        .unwrap();
    assert_eq!(recovered.state, ScheduledExecutionState::Interrupted);
    assert_eq!(
        recovered.reason,
        Some(protocol::ScheduledExecutionReason::ColdDaemonRecovery)
    );
    assert_eq!(recovered.outcome, None);
    assert_eq!(recovered.shell_id, active.shell_id);
    assert_eq!(recovered.run_id, active.run_id);
    let notification_capture = daemon.runtime_dir.join("cold-notifications");
    wait_until(
        || notification_capture.is_file(),
        "cold interruption notification was not delivered",
    );
    let notification = fs::read_to_string(&notification_capture).unwrap();
    assert_eq!(notification.lines().count(), 1);
    assert!(notification.contains(&execution.id));
    assert!(!notification.contains("private"));
    assert_eq!(
        daemon
            .client
            .run_agent_schedule(&schedule.id, &execution.dispatch_key)
            .unwrap()
            .id,
        execution.id
    );
    assert_eq!(fs::read_to_string(&capture).unwrap(), "spawn\n");
    let mut paths = vec![daemon.runtime_dir.join("bin")];
    paths.extend(std::env::split_paths(
        &std::env::var_os("PATH").unwrap_or_default(),
    ));
    let restart = daemon
        .command()
        .args(["daemon", "restart"])
        .env(
            "BOOMUX_CONFIG",
            daemon.runtime_dir.join("cold-notifications.toml"),
        )
        .env("PATH", std::env::join_paths(paths).unwrap())
        .env("BOOMUX_EXECUTION_CAPTURE", &capture)
        .env("BOOMUX_PID_CAPTURE", daemon.runtime_dir.join("pids"))
        .output()
        .unwrap();
    assert!(restart.status.success());
    thread::sleep(Duration::from_millis(100));
    assert_eq!(
        fs::read_to_string(&notification_capture)
            .unwrap()
            .lines()
            .count(),
        1,
        "cold interruption notification replayed on graceful restart"
    );
    let next = daemon
        .client
        .run_agent_schedule(&schedule.id, Uuid::new_v4().to_string())
        .unwrap();
    wait_until(
        || fs::read_to_string(&capture).is_ok_and(|capture| capture.lines().count() == 2),
        "new key did not start exactly one new run",
    );
    let next = daemon.client.get_scheduled_execution(&next.id).unwrap();
    assert_eq!(next.shell_id, recovered.shell_id);
    assert_ne!(next.run_id, recovered.run_id);
    daemon.client.cancel_scheduled_execution(&next.id).unwrap();
    daemon.stop_with_cli();
}

#[test]
fn execution_wait_reconnects_and_terminal_execution_accepts_canonical_blocked_agent_link() {
    let mut daemon = TestDaemon::start_with(|command, runtime_dir| {
        let bin = runtime_dir.join("bin");
        fs::create_dir(&bin).unwrap();
        let host = bin.join("opencode");
        fs::write(
            &host,
            "#!/bin/sh\nwhile [ ! -e \"$BOOMUX_RELEASE_EXECUTION\" ]; do sleep 0.02; done\n",
        )
        .unwrap();
        fs::set_permissions(&host, fs::Permissions::from_mode(0o755)).unwrap();
        let mut paths = vec![bin];
        paths.extend(std::env::split_paths(
            &std::env::var_os("PATH").unwrap_or_default(),
        ));
        command
            .env("PATH", std::env::join_paths(paths).unwrap())
            .env(
                "BOOMUX_RELEASE_EXECUTION",
                runtime_dir.join("release-execution"),
            );
    });
    let workspace = daemon
        .client
        .create_workspace("execution-wait", Vec::new())
        .unwrap();
    let schedule = daemon
        .client
        .create_agent_schedule(
            &workspace.id,
            schedule_spec(&daemon, "execution-wait", "private wait prompt"),
        )
        .unwrap();
    let execution = daemon
        .client
        .run_agent_schedule(&schedule.id, Uuid::new_v4().to_string())
        .unwrap();
    let active = loop {
        let current = daemon
            .client
            .get_scheduled_execution(&execution.id)
            .unwrap();
        if current.state == ScheduledExecutionState::Active {
            break current;
        }
        thread::sleep(Duration::from_millis(10));
    };
    assert!(
        !daemon
            .client
            .wait_scheduled_execution(&active.id, active.revision, 0)
            .unwrap()
            .changed
    );
    let waiting_socket = daemon.client.socket_path().to_path_buf();
    let execution_id = active.id.clone();
    let revision = active.revision;
    let waiter = thread::spawn(move || {
        Client::from_socket_path(waiting_socket).wait_scheduled_execution(
            execution_id,
            revision,
            10_000,
        )
    });
    thread::sleep(Duration::from_millis(50));

    let mut paths = vec![daemon.runtime_dir.join("bin")];
    paths.extend(std::env::split_paths(
        &std::env::var_os("PATH").unwrap_or_default(),
    ));
    let restart = daemon
        .command()
        .env("PATH", std::env::join_paths(paths).unwrap())
        .args(["daemon", "restart"])
        .output()
        .unwrap();
    assert!(restart.status.success());
    let error = waiter.join().unwrap().unwrap_err();
    assert_remote_code(&error, protocol::ErrorCode::DaemonStopping);

    let before_terminal = daemon
        .client
        .get_scheduled_execution(&execution.id)
        .unwrap();
    fs::write(daemon.runtime_dir.join("release-execution"), "release").unwrap();
    let mut after_revision = before_terminal.revision;
    let terminal = loop {
        let waited = daemon
            .client
            .wait_scheduled_execution(&execution.id, after_revision, 1_000)
            .unwrap();
        assert!(waited.changed);
        if waited.execution.state == ScheduledExecutionState::Exited {
            break waited.execution;
        }
        after_revision = waited.execution.revision;
    };

    let agent = daemon
        .client
        .register_agent(
            terminal.shell_id.clone().unwrap(),
            terminal.run_id.clone().unwrap(),
            AgentRegistrationSpec {
                name: "blocked scheduled agent".into(),
                integration: "opencode".into(),
                external_session_id: Some("exact-blocked-session".into()),
                report: AgentReport {
                    state: AgentState::Blocked,
                    authority: AgentAuthority::LifecycleIntegration,
                    evidence: "permission required".into(),
                    confidence: 100,
                },
            },
        )
        .unwrap();
    let attention = agent.attention.as_ref().expect("blocked attention");
    assert_eq!(attention.observation.revision, agent.observation.revision);
    let linked = daemon
        .client
        .wait_scheduled_execution(&execution.id, terminal.revision, 1_000)
        .unwrap();
    assert!(linked.changed);
    assert_eq!(
        linked.execution.agent_id.as_deref(),
        Some(agent.id.as_str())
    );
    assert_eq!(
        linked.execution.external_session_id.as_deref(),
        agent.external_session_id.as_deref()
    );
    daemon.stop_with_cli();
}

#[test]
fn cold_recovery_clears_staged_outcome_and_survives_second_restart() {
    let mut daemon = TestDaemon::start_with(|command, runtime_dir| {
        let barrier = runtime_dir.join("outcome-barrier");
        fs::create_dir(&barrier).unwrap();
        let bin = runtime_dir.join("bin");
        fs::create_dir(&bin).unwrap();
        let host = bin.join("opencode");
        fs::write(&host, "#!/bin/sh\nexit 17\n").unwrap();
        fs::set_permissions(&host, fs::Permissions::from_mode(0o755)).unwrap();
        let notify = bin.join("notify-send");
        fs::write(
            &notify,
            "#!/bin/sh\nprintf '%s\\n' \"$*\" >> \"$BOOMUX_NOTIFICATION_CAPTURE\"\n",
        )
        .unwrap();
        fs::set_permissions(&notify, fs::Permissions::from_mode(0o755)).unwrap();
        let config = runtime_dir.join("cold-notifications.toml");
        fs::write(
            &config,
            "[notifications]\nenabled = true\nscheduled_interrupted = true\n",
        )
        .unwrap();
        let mut paths = vec![bin];
        paths.extend(std::env::split_paths(
            &std::env::var_os("PATH").unwrap_or_default(),
        ));
        command
            .env("PATH", std::env::join_paths(paths).unwrap())
            .env("BOOMUX_NATIVE_TEST_OUTCOME_BARRIER", &barrier)
            .env("BOOMUX_CONFIG", config)
            .env(
                "BOOMUX_NOTIFICATION_CAPTURE",
                runtime_dir.join("cold-notifications"),
            );
    });
    let workspace = daemon
        .client
        .create_workspace("cold-staged-outcome", Vec::new())
        .unwrap();
    let schedule = daemon
        .client
        .create_agent_schedule(
            &workspace.id,
            schedule_spec(&daemon, "cold-staged-outcome", "private"),
        )
        .unwrap();
    let execution = daemon
        .client
        .run_agent_schedule(&schedule.id, Uuid::new_v4().to_string())
        .unwrap();
    let barrier = daemon.runtime_dir.join("outcome-barrier");
    wait_until(
        || barrier.join("outcome").is_file(),
        "runner outcome was not staged before EOF",
    );
    let staged = daemon
        .client
        .get_scheduled_execution(&execution.id)
        .unwrap();
    assert_eq!(staged.state, ScheduledExecutionState::Active);
    assert_eq!(
        staged.outcome,
        Some(protocol::ScheduledExecutionOutcome::ExitCode { code: 17 })
    );

    let restart_path = {
        let mut paths = vec![daemon.runtime_dir.join("bin")];
        paths.extend(std::env::split_paths(
            &std::env::var_os("PATH").unwrap_or_default(),
        ));
        std::env::join_paths(paths).unwrap()
    };
    let restart_config = daemon.runtime_dir.join("cold-notifications.toml");
    let notification_capture = daemon.runtime_dir.join("cold-notifications");
    daemon.crash();
    daemon.restart_with(|command| {
        command
            .env("PATH", &restart_path)
            .env("BOOMUX_CONFIG", &restart_config)
            .env("BOOMUX_NOTIFICATION_CAPTURE", &notification_capture);
    });
    let recovered = daemon
        .client
        .get_scheduled_execution(&execution.id)
        .unwrap();
    assert_eq!(recovered.state, ScheduledExecutionState::Interrupted);
    assert_eq!(
        recovered.reason,
        Some(protocol::ScheduledExecutionReason::ColdDaemonRecovery)
    );
    assert_eq!(recovered.outcome, None);
    wait_until(
        || notification_capture.is_file(),
        "cold interruption notification was not delivered",
    );
    assert_eq!(
        fs::read_to_string(&notification_capture)
            .unwrap()
            .lines()
            .count(),
        1
    );

    daemon.crash();
    daemon.restart_with(|command| {
        command
            .env("PATH", &restart_path)
            .env("BOOMUX_CONFIG", &restart_config)
            .env("BOOMUX_NOTIFICATION_CAPTURE", &notification_capture);
    });
    let restored_again = daemon
        .client
        .get_scheduled_execution(&execution.id)
        .unwrap();
    assert_eq!(restored_again, recovered);
    thread::sleep(Duration::from_millis(100));
    assert_eq!(
        fs::read_to_string(notification_capture)
            .unwrap()
            .lines()
            .count(),
        1
    );
    daemon.stop_with_cli();
}

#[test]
fn deterministic_clock_dispatches_due_work_and_protocol_23_hides_it() {
    const BASE_MS: u64 = 1_767_225_600_000;
    let mut daemon = TestDaemon::start_with(|command, runtime_dir| {
        let clock = runtime_dir.join("boomux/scheduler-clock");
        create_scheduler_clock(&clock);
        write_scheduler_tick(&clock, 1, BASE_MS);
        let bin = runtime_dir.join("bin");
        fs::create_dir(&bin).unwrap();
        let host = bin.join("opencode");
        fs::write(
            &host,
            "#!/bin/sh\nprintf 'spawn\\n' >> \"$BOOMUX_TIMED_CAPTURE\"\n",
        )
        .unwrap();
        fs::set_permissions(&host, fs::Permissions::from_mode(0o755)).unwrap();
        let mut paths = vec![bin];
        paths.extend(std::env::split_paths(
            &std::env::var_os("PATH").unwrap_or_default(),
        ));
        command
            .env("PATH", std::env::join_paths(paths).unwrap())
            .env("BOOMUX_NATIVE_TEST_CLOCK", &clock)
            .env("BOOMUX_TIMED_CAPTURE", runtime_dir.join("timed-spawns"));
    });
    let clock = daemon.runtime_dir.join("boomux/scheduler-clock");
    wait_for_scheduler_tick(&clock, 1);
    let workspace = daemon
        .client
        .create_workspace("timed-native", Vec::new())
        .unwrap();
    let mut spec = schedule_spec(&daemon, "timed-native", "private");
    spec.trigger.cron = "* * * * *".into();
    spec.state = AgentScheduleState::Enabled;
    let schedule = daemon
        .client
        .create_agent_schedule(&workspace.id, spec)
        .unwrap();
    assert_eq!(
        schedule
            .next_occurrence
            .as_ref()
            .map(|occurrence| occurrence.scheduled_at_ms),
        Some(BASE_MS + 60_000)
    );
    let protocol::Response::Events {
        cursor: old_cursor, ..
    } = versioned_request(
        &daemon,
        23,
        protocol::Request::Events {
            after: None,
            limit: 256,
            wait_ms: 0,
        },
    )
    else {
        panic!("expected protocol-23 baseline");
    };

    write_scheduler_tick(&clock, 2, BASE_MS + 60_000);
    wait_for_scheduler_tick(&clock, 2);
    wait_until(
        || {
            daemon
                .client
                .scheduled_executions(None, Some(schedule.id.clone()))
                .is_ok_and(|executions| {
                    executions.len() == 1 && executions[0].state == ScheduledExecutionState::Exited
                })
        },
        "timed execution did not complete",
    );
    assert_eq!(
        fs::read_to_string(daemon.runtime_dir.join("timed-spawns")).unwrap(),
        "spawn\n"
    );
    let execution = daemon
        .client
        .scheduled_executions(None, Some(schedule.id.clone()))
        .unwrap()
        .pop()
        .unwrap();
    assert_eq!(
        execution.dispatch_kind,
        protocol::ScheduledExecutionDispatchKind::Timed
    );
    assert_eq!(execution.scheduled_at_ms, Some(BASE_MS + 60_000));
    assert_eq!(
        daemon
            .client
            .get_agent_schedule(&schedule.id)
            .unwrap()
            .schedule
            .next_occurrence
            .unwrap()
            .scheduled_at_ms,
        BASE_MS + 120_000
    );
    assert_eq!(
        execution.id,
        Uuid::new_v5(
            &Uuid::NAMESPACE_OID,
            format!(
                "boomux:timed-execution:{}:{}:{}",
                schedule.id,
                schedule.trigger_revision,
                BASE_MS + 60_000
            )
            .as_bytes(),
        )
        .to_string()
    );

    let protocol::Response::ScheduledExecutions { executions, .. } = versioned_request(
        &daemon,
        23,
        protocol::Request::ListScheduledExecutions {
            workspace_id: None,
            schedule_id: Some(schedule.id.clone()),
            limit: None,
        },
    ) else {
        panic!("expected protocol-23 execution list");
    };
    assert!(executions.is_empty());
    assert!(matches!(
        versioned_request(
            &daemon,
            23,
            protocol::Request::GetScheduledExecution {
                execution_id: execution.id.clone(),
            }
        ),
        protocol::Response::Error {
            code: Some(protocol::ErrorCode::NotFound),
            ..
        }
    ));
    let protocol::Response::Events { cursor, events, .. } = versioned_request(
        &daemon,
        23,
        protocol::Request::Events {
            after: Some(old_cursor.clone()),
            limit: 256,
            wait_ms: 0,
        },
    ) else {
        panic!("expected protocol-23 event page");
    };
    assert!(cursor.event_id > old_cursor.event_id);
    assert!(!events.iter().any(|event| matches!(
        event.kind,
        protocol::DaemonEventKind::ScheduledExecutionCreated { .. }
            | protocol::DaemonEventKind::ScheduledExecutionChanged { .. }
    )));
    let protocol::Response::Snapshot { snapshot } =
        versioned_request(&daemon, 23, protocol::Request::Snapshot)
    else {
        panic!("expected protocol-23 snapshot");
    };
    assert!(snapshot.scheduler.is_none());
    assert!(
        snapshot.workspaces[0].schedules[0]
            .next_occurrence
            .is_none()
    );
    daemon.stop_with_cli();
}

#[test]
fn paused_time_is_not_caught_up_after_resume() {
    const BASE_MS: u64 = 1_767_225_600_000;
    let mut daemon = TestDaemon::start_with(|command, runtime_dir| {
        let clock = runtime_dir.join("boomux/paused-scheduler-clock");
        create_scheduler_clock(&clock);
        write_scheduler_tick(&clock, 1, BASE_MS);
        let bin = runtime_dir.join("bin");
        fs::create_dir(&bin).unwrap();
        let host = bin.join("opencode");
        fs::write(&host, "#!/bin/sh\nexit 0\n").unwrap();
        fs::set_permissions(&host, fs::Permissions::from_mode(0o755)).unwrap();
        let mut paths = vec![bin];
        paths.extend(std::env::split_paths(
            &std::env::var_os("PATH").unwrap_or_default(),
        ));
        command
            .env("PATH", std::env::join_paths(paths).unwrap())
            .env("BOOMUX_NATIVE_TEST_CLOCK", &clock);
    });
    let clock = daemon.runtime_dir.join("boomux/paused-scheduler-clock");
    wait_for_scheduler_tick(&clock, 1);
    let workspace = daemon
        .client
        .create_workspace("paused-timed", Vec::new())
        .unwrap();
    let mut spec = schedule_spec(&daemon, "paused-timed", "private");
    spec.trigger.cron = "* * * * *".into();
    let schedule = daemon
        .client
        .create_agent_schedule(&workspace.id, spec)
        .unwrap();

    write_scheduler_tick(&clock, 2, BASE_MS + 180_000);
    wait_for_scheduler_tick(&clock, 2);
    assert!(
        daemon
            .client
            .scheduled_executions(None, Some(schedule.id.clone()))
            .unwrap()
            .is_empty()
    );
    daemon.client.resume_agent_schedule(&schedule.id).unwrap();
    assert_eq!(
        daemon
            .client
            .get_agent_schedule(&schedule.id)
            .unwrap()
            .schedule
            .next_occurrence
            .unwrap()
            .scheduled_at_ms,
        BASE_MS + 240_000
    );
    write_scheduler_tick(&clock, 3, BASE_MS + 240_000);
    wait_for_scheduler_tick(&clock, 3);
    wait_until(
        || {
            daemon
                .client
                .scheduled_executions(None, Some(schedule.id.clone()))
                .is_ok_and(|executions| {
                    executions.len() == 1 && executions[0].state == ScheduledExecutionState::Exited
                })
        },
        "first post-resume occurrence did not finish",
    );
    let execution = daemon
        .client
        .scheduled_executions(None, Some(schedule.id))
        .unwrap()
        .pop()
        .unwrap();
    assert_eq!(execution.scheduled_at_ms, Some(BASE_MS + 240_000));
    assert_eq!(execution.coalesced_through_ms, None);
    assert_eq!(
        daemon
            .client
            .get_agent_schedule(&execution.schedule_id)
            .unwrap()
            .schedule
            .next_occurrence
            .unwrap()
            .scheduled_at_ms,
        BASE_MS + 300_000
    );
    daemon.stop_with_cli();
}

#[test]
fn cold_downtime_coalesces_missed_occurrences_without_spawning_and_is_stable() {
    const BASE_MS: u64 = 1_767_225_600_000;
    let mut daemon = TestDaemon::start_with(|command, runtime_dir| {
        let clock = runtime_dir.join("boomux/cold-scheduler-clock");
        create_scheduler_clock(&clock);
        write_scheduler_tick(&clock, 1, BASE_MS);
        command.env("BOOMUX_NATIVE_TEST_CLOCK", &clock).env(
            "BOOMUX_TIMED_CAPTURE",
            runtime_dir.join("cold-timed-spawns"),
        );
    });
    let clock = daemon.runtime_dir.join("boomux/cold-scheduler-clock");
    wait_for_scheduler_tick(&clock, 1);
    let workspace = daemon
        .client
        .create_workspace("cold-timed", Vec::new())
        .unwrap();
    let mut spec = schedule_spec(&daemon, "cold-timed", "private");
    spec.trigger.cron = "* * * * *".into();
    spec.state = AgentScheduleState::Enabled;
    let schedule = daemon
        .client
        .create_agent_schedule(&workspace.id, spec)
        .unwrap();
    daemon.crash();

    write_scheduler_tick(&clock, 2, BASE_MS + 180_000);
    let clock_for_restart = clock.clone();
    daemon.restart_with(|command| {
        command.env("BOOMUX_NATIVE_TEST_CLOCK", clock_for_restart);
    });
    wait_for_scheduler_tick(&clock, 2);
    let executions = daemon
        .client
        .scheduled_executions(None, Some(schedule.id.clone()))
        .unwrap();
    assert_eq!(executions.len(), 1);
    assert_eq!(executions[0].state, ScheduledExecutionState::Skipped);
    assert_eq!(
        executions[0].reason,
        Some(protocol::ScheduledExecutionReason::Missed)
    );
    assert_eq!(executions[0].scheduled_at_ms, Some(BASE_MS + 60_000));
    assert_eq!(executions[0].coalesced_through_ms, Some(BASE_MS + 180_000));
    assert_eq!(
        daemon
            .client
            .get_agent_schedule(&schedule.id)
            .unwrap()
            .schedule
            .next_occurrence
            .unwrap()
            .scheduled_at_ms,
        BASE_MS + 240_000
    );
    assert!(!daemon.runtime_dir.join("cold-timed-spawns").exists());

    daemon.crash();
    let clock_for_restart = clock.clone();
    daemon.restart_with(|command| {
        command.env("BOOMUX_NATIVE_TEST_CLOCK", clock_for_restart);
    });
    assert_eq!(
        daemon
            .client
            .scheduled_executions(None, Some(schedule.id))
            .unwrap(),
        executions
    );
    daemon.stop_with_cli();
}

#[test]
fn graceful_handoff_replacement_evaluates_due_boundary_only_after_finalize() {
    const BASE_MS: u64 = 1_767_225_600_000;
    let mut daemon = TestDaemon::start_with(|command, runtime_dir| {
        let clock = runtime_dir.join("boomux/handoff-scheduler-clock");
        create_scheduler_clock(&clock);
        write_scheduler_tick(&clock, 1, BASE_MS);
        let replacement_clock = runtime_dir.join("boomux/handoff-replacement-clock");
        create_scheduler_clock(&replacement_clock);
        write_scheduler_tick(&replacement_clock, 2, BASE_MS + 60_000);
        let bin = runtime_dir.join("bin");
        fs::create_dir(&bin).unwrap();
        let host = bin.join("opencode");
        fs::write(
            &host,
            "#!/bin/sh\nprintf 'spawn\\n' >> \"$BOOMUX_TIMED_CAPTURE\"\n",
        )
        .unwrap();
        fs::set_permissions(&host, fs::Permissions::from_mode(0o755)).unwrap();
        let mut paths = vec![bin];
        paths.extend(std::env::split_paths(
            &std::env::var_os("PATH").unwrap_or_default(),
        ));
        command
            .env("PATH", std::env::join_paths(paths).unwrap())
            .env("BOOMUX_NATIVE_TEST_CLOCK", &clock)
            .env(
                "BOOMUX_TIMED_CAPTURE",
                runtime_dir.join("handoff-timed-spawns"),
            );
    });
    let clock = daemon.runtime_dir.join("boomux/handoff-scheduler-clock");
    wait_for_scheduler_tick(&clock, 1);
    let workspace = daemon
        .client
        .create_workspace("handoff-timed", Vec::new())
        .unwrap();
    let mut spec = schedule_spec(&daemon, "handoff-timed", "private");
    spec.trigger.cron = "* * * * *".into();
    spec.state = AgentScheduleState::Enabled;
    let schedule = daemon
        .client
        .create_agent_schedule(&workspace.id, spec)
        .unwrap();

    let replacement_clock = daemon.runtime_dir.join("boomux/handoff-replacement-clock");
    let mut paths = vec![daemon.runtime_dir.join("bin")];
    paths.extend(std::env::split_paths(
        &std::env::var_os("PATH").unwrap_or_default(),
    ));
    let restart = daemon
        .command()
        .args(["daemon", "restart"])
        .env("PATH", std::env::join_paths(paths).unwrap())
        .env("BOOMUX_NATIVE_TEST_CLOCK", &replacement_clock)
        .env(
            "BOOMUX_TIMED_CAPTURE",
            daemon.runtime_dir.join("handoff-timed-spawns"),
        )
        .output()
        .unwrap();
    assert!(restart.status.success());
    wait_for_scheduler_tick(&replacement_clock, 2);
    wait_until(
        || {
            daemon
                .client
                .scheduled_executions(None, Some(schedule.id.clone()))
                .is_ok_and(|executions| {
                    executions.len() == 1 && executions[0].state == ScheduledExecutionState::Exited
                })
        },
        "handoff due occurrence did not finish",
    );
    assert_eq!(
        fs::read_to_string(daemon.runtime_dir.join("handoff-timed-spawns")).unwrap(),
        "spawn\n"
    );
    daemon.stop_with_cli();
}

#[test]
fn graceful_handoff_transfers_an_old_daemon_due_boundary_claim_without_duplicate() {
    const BASE_MS: u64 = 1_767_225_600_000;
    let mut daemon = TestDaemon::start_with(|command, runtime_dir| {
        let clock = runtime_dir.join("boomux/handoff-old-claim-clock");
        create_scheduler_clock(&clock);
        write_scheduler_tick(&clock, 1, BASE_MS);
        let barrier = runtime_dir.join("handoff-old-claim-barrier");
        fs::create_dir(&barrier).unwrap();
        let bin = runtime_dir.join("bin");
        fs::create_dir(&bin).unwrap();
        let host = bin.join("opencode");
        fs::write(
            &host,
            "#!/bin/sh\nprintf 'spawn\n' >> \"$BOOMUX_TIMED_CAPTURE\"\n",
        )
        .unwrap();
        fs::set_permissions(&host, fs::Permissions::from_mode(0o755)).unwrap();
        let mut paths = vec![bin];
        paths.extend(std::env::split_paths(
            &std::env::var_os("PATH").unwrap_or_default(),
        ));
        command
            .env("PATH", std::env::join_paths(paths).unwrap())
            .env("BOOMUX_NATIVE_TEST_CLOCK", &clock)
            .env("BOOMUX_NATIVE_TEST_CLAIM_BARRIER", &barrier)
            .env(
                "BOOMUX_TIMED_CAPTURE",
                runtime_dir.join("handoff-old-claim-spawns"),
            );
    });
    let clock = daemon.runtime_dir.join("boomux/handoff-old-claim-clock");
    wait_for_scheduler_tick(&clock, 1);
    let workspace = daemon
        .client
        .create_workspace("handoff-old-claim", Vec::new())
        .unwrap();
    let mut spec = schedule_spec(&daemon, "handoff-old-claim", "private");
    spec.trigger.cron = "* * * * *".into();
    spec.state = AgentScheduleState::Enabled;
    let schedule = daemon
        .client
        .create_agent_schedule(&workspace.id, spec)
        .unwrap();

    write_scheduler_tick(&clock, 2, BASE_MS + 60_000);
    let barrier = daemon.runtime_dir.join("handoff-old-claim-barrier");
    wait_until(
        || barrier.join("claimed").is_file(),
        "old daemon did not commit the due-boundary claim",
    );
    let mut paths = vec![daemon.runtime_dir.join("bin")];
    paths.extend(std::env::split_paths(
        &std::env::var_os("PATH").unwrap_or_default(),
    ));
    let mut restart = daemon.command();
    restart
        .args(["daemon", "restart"])
        .env("PATH", std::env::join_paths(paths).unwrap())
        .env("BOOMUX_NATIVE_TEST_CLOCK", &clock)
        .env("BOOMUX_NATIVE_TEST_CLAIM_BARRIER", &barrier)
        .env(
            "BOOMUX_TIMED_CAPTURE",
            daemon.runtime_dir.join("handoff-old-claim-spawns"),
        );
    let restart = thread::spawn(move || restart.output().unwrap());
    fs::write(barrier.join("release"), b"release").unwrap();
    let restart = restart.join().unwrap();
    assert!(restart.status.success());
    wait_until(
        || {
            daemon
                .client
                .scheduled_executions(None, Some(schedule.id.clone()))
                .is_ok_and(|executions| {
                    executions.len() == 1 && executions[0].state == ScheduledExecutionState::Exited
                })
        },
        "transferred due-boundary claim did not finish",
    );
    assert_eq!(
        fs::read_to_string(daemon.runtime_dir.join("handoff-old-claim-spawns")).unwrap(),
        "spawn\n"
    );
    daemon.stop_with_cli();
}

#[test]
fn timed_persistence_failure_retries_the_same_occurrence_once() {
    const BASE_MS: u64 = 1_767_225_600_000;
    let mut daemon = TestDaemon::start_with(|command, runtime_dir| {
        let clock = runtime_dir.join("boomux/retry-scheduler-clock");
        create_scheduler_clock(&clock);
        write_scheduler_tick(&clock, 1, BASE_MS);
        command.env("BOOMUX_NATIVE_TEST_CLOCK", &clock);
    });
    let clock = daemon.runtime_dir.join("boomux/retry-scheduler-clock");
    wait_for_scheduler_tick(&clock, 1);
    let workspace = daemon
        .client
        .create_workspace("retry-timed", Vec::new())
        .unwrap();
    let mut spec = schedule_spec(&daemon, "retry-timed", "private");
    spec.trigger.cron = "* * * * *".into();
    spec.state = AgentScheduleState::Enabled;
    let schedule = daemon
        .client
        .create_agent_schedule(&workspace.id, spec)
        .unwrap();
    let state_directory = daemon.runtime_dir.join("state/boomux");
    let saved_directory = daemon.runtime_dir.join("saved-timed-retry-state");
    fs::rename(&state_directory, &saved_directory).unwrap();
    fs::write(&state_directory, b"not a directory").unwrap();

    write_scheduler_tick(&clock, 2, BASE_MS + 180_000);
    wait_for_scheduler_seen(&clock, 2);
    assert!(!fs::read_to_string(clock.join("ack")).is_ok_and(|value| value.trim() == "2"));

    fs::remove_file(&state_directory).unwrap();
    fs::rename(&saved_directory, &state_directory).unwrap();
    wait_for_scheduler_tick(&clock, 2);
    let executions = daemon
        .client
        .scheduled_executions(None, Some(schedule.id))
        .unwrap();
    assert_eq!(executions.len(), 2);
    assert_eq!(
        executions
            .iter()
            .filter(|execution| execution.scheduled_at_ms == Some(BASE_MS + 60_000))
            .count(),
        1
    );
    let missed = executions
        .iter()
        .find(|execution| execution.scheduled_at_ms == Some(BASE_MS + 60_000))
        .unwrap();
    assert_eq!(missed.state, ScheduledExecutionState::Skipped);
    assert_eq!(missed.coalesced_through_ms, Some(BASE_MS + 120_000));
    assert_eq!(
        executions
            .iter()
            .filter(|execution| execution.scheduled_at_ms == Some(BASE_MS + 180_000))
            .count(),
        1
    );
    daemon.stop_with_cli();
}

#[test]
fn persistent_scheduler_failure_backs_off_and_stop_interrupts_the_wait() {
    const BASE_MS: u64 = 1_767_225_600_000;
    let mut daemon = TestDaemon::start_with(|command, runtime_dir| {
        let clock = runtime_dir.join("boomux/persistent-failure-clock");
        create_scheduler_clock(&clock);
        write_scheduler_tick(&clock, 1, BASE_MS);
        command.env("BOOMUX_NATIVE_TEST_CLOCK", &clock);
    });
    let clock = daemon.runtime_dir.join("boomux/persistent-failure-clock");
    wait_for_scheduler_tick(&clock, 1);
    let workspace = daemon
        .client
        .create_workspace("persistent-scheduler-failure", Vec::new())
        .unwrap();
    let mut spec = schedule_spec(&daemon, "persistent-scheduler-failure", "private");
    spec.trigger.cron = "* * * * *".into();
    spec.state = AgentScheduleState::Enabled;
    daemon
        .client
        .create_agent_schedule(&workspace.id, spec)
        .unwrap();
    let baseline_attempts = fs::read_to_string(clock.join("attempts"))
        .unwrap()
        .trim()
        .parse::<u64>()
        .unwrap();
    let state_directory = daemon.runtime_dir.join("state/boomux");
    let saved_directory = daemon.runtime_dir.join("saved-persistent-failure-state");
    fs::rename(&state_directory, &saved_directory).unwrap();
    fs::write(&state_directory, b"not a directory").unwrap();

    write_scheduler_tick(&clock, 2, BASE_MS + 60_000);
    wait_for_scheduler_seen(&clock, 2);
    for index in 0..20 {
        let output = if index % 5 == 0 {
            daemon.command().arg("doctor").output().unwrap()
        } else {
            daemon
                .command()
                .args(["daemon", "status"])
                .output()
                .unwrap()
        };
        if index % 5 != 0 {
            assert!(output.status.success());
        }
    }
    let attempts_after_polling = fs::read_to_string(clock.join("attempts"))
        .unwrap()
        .trim()
        .parse::<u64>()
        .unwrap();
    assert!(attempts_after_polling <= baseline_attempts + 7);
    wait_until(
        || {
            fs::read_to_string(clock.join("attempts"))
                .ok()
                .and_then(|value| value.trim().parse::<u64>().ok())
                .is_some_and(|attempts| attempts >= baseline_attempts + 3)
        },
        "scheduler did not perform bounded retries",
    );
    let attempts = fs::read_to_string(clock.join("attempts"))
        .unwrap()
        .trim()
        .parse::<u64>()
        .unwrap();
    assert!(
        attempts <= baseline_attempts + 7,
        "retry loop spun: {attempts}"
    );
    assert_eq!(
        fs::read_to_string(clock.join("diagnostics"))
            .unwrap()
            .trim(),
        "1"
    );
    assert!(!fs::read_to_string(clock.join("ack")).is_ok_and(|value| value.trim() == "2"));
    assert_eq!(
        daemon.client.snapshot().unwrap().scheduler.unwrap().state,
        protocol::SchedulerState::Offline
    );
    let status = daemon
        .command()
        .args(["daemon", "status"])
        .output()
        .unwrap();
    assert!(String::from_utf8_lossy(&status.stdout).contains("scheduler offline"));
    let doctor = daemon.command().arg("doctor").output().unwrap();
    assert!(
        String::from_utf8_lossy(&doctor.stderr)
            .contains("err scheduler: offline; timed schedules are not being evaluated")
    );

    fs::remove_file(&state_directory).unwrap();
    fs::rename(&saved_directory, &state_directory).unwrap();
    let started = Instant::now();
    daemon.stop_with_cli();
    assert!(started.elapsed() < Duration::from_secs(1));
}

#[test]
fn daemon_stop_persists_explicit_cancellation_for_active_execution() {
    let mut daemon = long_running_schedule_daemon("daemon-stop");
    let (_workspace_id, _schedule_id, execution_id, pid) =
        start_long_running_execution(&daemon, "daemon-stop");

    daemon.stop_with_cli();
    wait_until(
        || !process_exists(pid),
        "daemon stop left scheduled host running",
    );
    let state: serde_json::Value = serde_json::from_slice(
        &fs::read(daemon.runtime_dir.join("state/boomux/state.json")).unwrap(),
    )
    .unwrap();
    let execution = state["workspaces"][0]["schedules"][0]["executions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|execution| execution["id"] == execution_id)
        .unwrap();
    assert_eq!(execution["state"], "cancelled");
    assert_eq!(execution["reason"], "daemon_shutdown");
    assert!(execution["outcome"].is_null());
}

#[test]
fn daemon_stop_cancels_persisted_claim_before_it_can_spawn() {
    let mut daemon = TestDaemon::start_with(|command, runtime_dir| {
        let barrier = runtime_dir.join("stop-claim-barrier");
        fs::create_dir(&barrier).unwrap();
        let bin = runtime_dir.join("bin");
        fs::create_dir(&bin).unwrap();
        let host = bin.join("opencode");
        fs::write(&host, "#!/bin/sh\n: > \"$BOOMUX_STOP_SPAWN\"\n").unwrap();
        fs::set_permissions(&host, fs::Permissions::from_mode(0o755)).unwrap();
        let mut paths = vec![bin];
        paths.extend(std::env::split_paths(
            &std::env::var_os("PATH").unwrap_or_default(),
        ));
        command
            .env("PATH", std::env::join_paths(paths).unwrap())
            .env("BOOMUX_NATIVE_TEST_CLAIM_BARRIER", &barrier)
            .env("BOOMUX_STOP_SPAWN", runtime_dir.join("stop-claim-spawn"));
    });
    let workspace = daemon
        .client
        .create_workspace("stop-claim", Vec::new())
        .unwrap();
    let schedule = daemon
        .client
        .create_agent_schedule(
            &workspace.id,
            schedule_spec(&daemon, "stop-claim", "private"),
        )
        .unwrap();
    let request_client = Client::from_socket_path(daemon.client.socket_path().to_path_buf());
    let schedule_id = schedule.id.clone();
    let request = thread::spawn(move || {
        request_client.run_agent_schedule(schedule_id, Uuid::new_v4().to_string())
    });
    let barrier = daemon.runtime_dir.join("stop-claim-barrier");
    wait_until(
        || barrier.join("claimed").is_file(),
        "claim barrier was not reached",
    );
    let execution_id = daemon
        .client
        .scheduled_executions(None, Some(schedule.id))
        .unwrap()[0]
        .id
        .clone();

    daemon.stop_with_cli();
    let _ = request.join().unwrap();
    assert!(!daemon.runtime_dir.join("stop-claim-spawn").exists());
    let state: serde_json::Value = serde_json::from_slice(
        &fs::read(daemon.runtime_dir.join("state/boomux/state.json")).unwrap(),
    )
    .unwrap();
    let execution = state["workspaces"][0]["schedules"][0]["executions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|execution| execution["id"] == execution_id)
        .unwrap();
    assert_eq!(execution["state"], "cancelled");
    assert_eq!(execution["reason"], "daemon_shutdown");
}

#[test]
fn graceful_handoff_preserves_active_host_and_dispatches_at_most_once() {
    let mut daemon = TestDaemon::start_with(|command, runtime_dir| {
        let bin = runtime_dir.join("bin");
        fs::create_dir(&bin).unwrap();
        let host = bin.join("opencode");
        fs::write(
            &host,
            "#!/bin/sh\nprintf '%s\\n' \"$$\" >> \"$BOOMUX_PID_CAPTURE\"\nsleep 2\n",
        )
        .unwrap();
        fs::set_permissions(&host, fs::Permissions::from_mode(0o755)).unwrap();
        let mut paths = vec![bin];
        paths.extend(std::env::split_paths(
            &std::env::var_os("PATH").unwrap_or_default(),
        ));
        command
            .env("PATH", std::env::join_paths(paths).unwrap())
            .env("BOOMUX_PID_CAPTURE", runtime_dir.join("pids"));
    });
    let workspace = daemon
        .client
        .create_workspace("handoff", Vec::new())
        .unwrap();
    let schedule = daemon
        .client
        .create_agent_schedule(&workspace.id, schedule_spec(&daemon, "handoff", "private"))
        .unwrap();
    let key = Uuid::new_v4().to_string();
    let execution = daemon
        .client
        .run_agent_schedule(&schedule.id, &key)
        .unwrap();
    let capture = daemon.runtime_dir.join("pids");
    wait_until(|| capture.is_file(), "scheduled host did not start");
    wait_until(
        || {
            daemon
                .client
                .get_scheduled_execution(&execution.id)
                .is_ok_and(|execution| execution.state == ScheduledExecutionState::Active)
        },
        "scheduled execution did not become active before handoff",
    );
    let active = daemon
        .client
        .get_scheduled_execution(&execution.id)
        .unwrap();
    let pid = fs::read_to_string(&capture)
        .unwrap()
        .trim()
        .parse::<libc::pid_t>()
        .unwrap();
    let restart = daemon
        .command()
        .args(["daemon", "restart"])
        .output()
        .unwrap();
    assert!(
        restart.status.success(),
        "{}",
        String::from_utf8_lossy(&restart.stderr)
    );
    assert!(process_exists(pid));
    assert_eq!(
        daemon
            .client
            .run_agent_schedule(&schedule.id, &key)
            .unwrap()
            .id,
        execution.id
    );
    assert_eq!(fs::read_to_string(&capture).unwrap().lines().count(), 1);
    wait_until(
        || {
            daemon
                .client
                .get_scheduled_execution(&execution.id)
                .is_ok_and(|execution| execution.state == ScheduledExecutionState::Exited)
        },
        "handoff-preserved host did not report natural exit",
    );
    let exited = daemon
        .client
        .get_scheduled_execution(&execution.id)
        .unwrap();
    assert_eq!(exited.shell_id, active.shell_id);
    assert_eq!(exited.run_id, active.run_id);
    assert_eq!(
        exited.outcome,
        Some(protocol::ScheduledExecutionOutcome::ExitCode { code: 0 })
    );
    wait_until(
        || !process_exists(pid),
        "naturally exited host PID remained alive",
    );
    daemon.stop_with_cli();
}

#[test]
fn graceful_handoff_resumes_a_persisted_claim_exactly_once() {
    let mut daemon = TestDaemon::start_with(|command, runtime_dir| {
        let barrier = runtime_dir.join("handoff-claim-barrier");
        fs::create_dir(&barrier).unwrap();
        let bin = runtime_dir.join("bin");
        fs::create_dir(&bin).unwrap();
        let host = bin.join("opencode");
        fs::write(
            &host,
            "#!/bin/sh\nprintf 'spawn\\n' >> \"$BOOMUX_EXECUTION_CAPTURE\"\n",
        )
        .unwrap();
        fs::set_permissions(&host, fs::Permissions::from_mode(0o755)).unwrap();
        let mut paths = vec![bin];
        paths.extend(std::env::split_paths(
            &std::env::var_os("PATH").unwrap_or_default(),
        ));
        command
            .env("PATH", std::env::join_paths(paths).unwrap())
            .env("BOOMUX_NATIVE_TEST_CLAIM_BARRIER", &barrier)
            .env(
                "BOOMUX_EXECUTION_CAPTURE",
                runtime_dir.join("claim-handoff-spawns"),
            );
    });
    let workspace = daemon
        .client
        .create_workspace("claim-handoff", Vec::new())
        .unwrap();
    let schedule = daemon
        .client
        .create_agent_schedule(
            &workspace.id,
            schedule_spec(&daemon, "claim-handoff", "private"),
        )
        .unwrap();
    let key = Uuid::new_v4().to_string();
    let request_client = Client::from_socket_path(daemon.client.socket_path().to_path_buf());
    let schedule_id = schedule.id.clone();
    let request_key = key.clone();
    let request =
        thread::spawn(move || request_client.run_agent_schedule(schedule_id, request_key));
    let barrier = daemon.runtime_dir.join("handoff-claim-barrier");
    wait_until(
        || barrier.join("claimed").is_file(),
        "claim barrier was not reached",
    );
    let claimed = daemon
        .client
        .scheduled_executions(None, Some(schedule.id.clone()))
        .unwrap()
        .pop()
        .unwrap();
    assert_eq!(claimed.state, ScheduledExecutionState::Claimed);

    let mut paths = vec![daemon.runtime_dir.join("bin")];
    paths.extend(std::env::split_paths(
        &std::env::var_os("PATH").unwrap_or_default(),
    ));
    let restart = daemon
        .command()
        .args(["daemon", "restart"])
        .env("PATH", std::env::join_paths(paths).unwrap())
        .env(
            "BOOMUX_EXECUTION_CAPTURE",
            daemon.runtime_dir.join("claim-handoff-spawns"),
        )
        .output()
        .unwrap();
    assert!(
        restart.status.success(),
        "{}",
        String::from_utf8_lossy(&restart.stderr)
    );
    fs::write(barrier.join("release"), b"release").unwrap();
    let _ = request.join().unwrap();
    wait_until(
        || {
            daemon
                .client
                .get_scheduled_execution(&claimed.id)
                .is_ok_and(|execution| execution.state == ScheduledExecutionState::Exited)
        },
        "replacement did not resume the persisted claim",
    );
    assert_eq!(
        fs::read_to_string(daemon.runtime_dir.join("claim-handoff-spawns"))
            .unwrap()
            .lines()
            .count(),
        1
    );
    let same = daemon
        .client
        .run_agent_schedule(&schedule.id, &key)
        .unwrap();
    assert_eq!(same.id, claimed.id);
    daemon.stop_with_cli();
}

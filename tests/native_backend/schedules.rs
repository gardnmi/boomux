use std::os::unix::net::UnixStream;

use boomux::protocol::{
    self, AgentScheduleOverlapPolicy, AgentScheduleSession, AgentScheduleSpec, AgentScheduleState,
    AgentScheduleTrigger,
};

use crate::support::{TestDaemon, assert_remote_code};

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

    let paused = daemon.client.pause_agent_schedule(&schedule.id).unwrap();
    assert_eq!(paused.revision, 1);
    let resumed = daemon.client.resume_agent_schedule(&schedule.id).unwrap();
    assert_eq!(resumed.revision, 2);
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
                    | protocol::DaemonEventKind::AgentSchedulePaused { .. }
                    | protocol::DaemonEventKind::AgentScheduleResumed { .. }
            ))
            .count(),
        2
    );
    assert!(
        !serde_json::to_string(&events.events)
            .unwrap()
            .contains(prompt)
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
        prompt
    );

    daemon.crash();
    daemon.restart();
    assert_eq!(
        daemon
            .client
            .get_agent_schedule(&schedule.id)
            .unwrap()
            .prompt,
        prompt
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

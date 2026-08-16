use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use boomux::protocol::{
    AgentScheduleOverlapPolicy, AgentScheduleSession, AgentScheduleSpec, AgentScheduleState,
    AgentScheduleTrigger, QualifiedIdentity, RoutedOperation, RoutedOperationResult,
    ScheduledExecutionReason, ScheduledExecutionState,
};
use uuid::Uuid;

use crate::support::{TestDaemon, process_exists, profile, wait_until};

fn install_fake_ssh(directory: &Path, owner: &TestDaemon) {
    let ssh = directory.join("ssh");
    fs::write(
        &ssh,
        format!(
            "#!/bin/sh\nlast=\nfor arg do last=$arg; done\ncase \"$last\" in\n  *boomux-platform-v1*) printf 'boomux-platform-v1\\0Linux\\0x86_64\\0' ;;\n  *boomux-executables-v1*) printf 'boomux-executables-v1\\0{}\\0' ;;\n  *boomux-install-destination-v1*) printf 'boomux-install-destination-v1\\0{}\\0' ;;\n  *__federation-stdio*) exec env XDG_RUNTIME_DIR='{}' XDG_STATE_HOME='{}' '{}' __federation-stdio ;;\n  *) exit 64 ;;\nesac\n",
            owner.executable.display(),
            owner.executable.display(),
            owner.runtime_dir.display(),
            owner.runtime_dir.join("state").display(),
            owner.executable.display(),
        ),
    )
    .unwrap();
    fs::set_permissions(&ssh, fs::Permissions::from_mode(0o700)).unwrap();
}

fn schedule_spec(cwd: &Path, prompt: &str, state: AgentScheduleState) -> AgentScheduleSpec {
    AgentScheduleSpec {
        name: "remote-review".into(),
        cwd: cwd.into(),
        integration: "opencode".into(),
        prompt: prompt.into(),
        session: AgentScheduleSession::Fresh,
        trigger: AgentScheduleTrigger {
            cron: "* * * * *".into(),
            timezone: "UTC".into(),
        },
        state,
        overlap_policy: AgentScheduleOverlapPolicy::Skip,
    }
}

fn create_clock(clock: &Path, generation: u64, now_ms: u64) {
    fs::create_dir_all(clock.parent().unwrap()).unwrap();
    fs::set_permissions(clock.parent().unwrap(), fs::Permissions::from_mode(0o700)).unwrap();
    fs::create_dir(clock).unwrap();
    fs::set_permissions(clock, fs::Permissions::from_mode(0o700)).unwrap();
    write_tick(clock, generation, now_ms);
}

fn write_tick(clock: &Path, generation: u64, now_ms: u64) {
    let tick = clock.join("tick");
    fs::write(&tick, format!("{generation} {now_ms}")).unwrap();
    fs::set_permissions(tick, fs::Permissions::from_mode(0o600)).unwrap();
}

fn wait_for_tick(clock: &Path, generation: u64) {
    wait_until(
        || {
            fs::read_to_string(clock.join("ack"))
                .is_ok_and(|value| value.trim() == generation.to_string())
        },
        "owner scheduler did not acknowledge deterministic tick",
    );
}

fn assert_tree_omits(path: &Path, private: &str) {
    if !path.exists() {
        return;
    }
    for entry in fs::read_dir(path).unwrap() {
        let entry = entry.unwrap();
        let file_type = entry.file_type().unwrap();
        if file_type.is_dir() {
            assert_tree_omits(&entry.path(), private);
        } else if file_type.is_file()
            && let Ok(bytes) = fs::read(entry.path())
        {
            assert!(!String::from_utf8_lossy(&bytes).contains(private));
        }
    }
}

#[test]
fn remote_execution_stays_owner_bound_across_local_stop_and_both_handoffs() {
    let mut owner = TestDaemon::start_with(|command, runtime_dir| {
        let bin = runtime_dir.join("bin");
        fs::create_dir(&bin).unwrap();
        let host = bin.join("opencode");
        fs::write(
            &host,
            "#!/bin/sh\nprintf '%s' \"$$\" > \"$BOOMUX_REMOTE_SCHEDULE_PID\"\nexec sleep 30\n",
        )
        .unwrap();
        fs::set_permissions(&host, fs::Permissions::from_mode(0o700)).unwrap();
        command
            .env("PATH", format!("{}:/usr/bin:/bin", bin.display()))
            .env(
                "BOOMUX_REMOTE_SCHEDULE_PID",
                runtime_dir.join("schedule-pid"),
            );
    });
    let owner_id = owner.client.node_identity().unwrap();
    let mut local = TestDaemon::start_with(|command, runtime_dir| {
        install_fake_ssh(runtime_dir, &owner);
        command.env("PATH", format!("{}:/usr/bin:/bin", runtime_dir.display()));
    });
    local
        .client
        .add_node_registration("owner", "fake-owner", &owner_id)
        .unwrap();
    let workspace = owner
        .client
        .create_workspace("remote-scheduled", Vec::new())
        .unwrap();
    wait_until(
        || {
            local
                .client
                .combined_node_snapshot(Some(owner_id.clone()))
                .is_ok_and(|snapshot| {
                    snapshot.nodes[0]
                        .remote_projection
                        .as_ref()
                        .is_some_and(|projection| {
                            projection
                                .workspaces
                                .iter()
                                .any(|value| value.id == workspace.id)
                        })
                })
        },
        "local projection did not observe owner workspace",
    );
    let cli_private = "PRIVATE REMOTE CLI CREATE PROMPT";
    let cli_create = local
        .command()
        .args([
            "schedule",
            "create",
            "cli-created",
            "--workspace",
            &workspace.id,
            "--cwd",
            owner.runtime_dir.to_str().unwrap(),
            "--integration",
            "opencode",
            "--prompt",
            cli_private,
            "--cron",
            "0 2 * * *",
            "--timezone",
            "UTC",
            "--node",
            "owner",
            "--json",
        ])
        .output()
        .unwrap();
    assert!(
        cli_create.status.success(),
        "{}",
        String::from_utf8_lossy(&cli_create.stderr)
    );
    assert!(!String::from_utf8_lossy(&cli_create.stdout).contains(cli_private));
    let cli_create: serde_json::Value = serde_json::from_slice(&cli_create.stdout).unwrap();
    assert_eq!(cli_create["data"]["node_id"], owner_id);
    let cli_schedule_id = cli_create["data"]["schedule"]["id"].as_str().unwrap();
    assert_eq!(
        owner
            .client
            .get_agent_schedule(cli_schedule_id)
            .unwrap()
            .prompt,
        cli_private
    );
    let private = "PRIVATE REMOTE SCHEDULE PROMPT";
    let schedule = match local
        .client
        .route_node_operation(
            &owner_id,
            RoutedOperation::CreateAgentSchedule {
                workspace_id: workspace.id.clone(),
                spec: schedule_spec(&owner.runtime_dir, private, AgentScheduleState::Paused),
            },
        )
        .unwrap()
    {
        RoutedOperationResult::AgentSchedule { schedule } => schedule,
        result => panic!("unexpected create result: {result:?}"),
    };
    assert!(local.client.snapshot().unwrap().workspaces.is_empty());
    assert!(
        !serde_json::to_string(&local.client.combined_node_snapshot(None).unwrap())
            .unwrap()
            .contains(private)
    );
    assert_tree_omits(&local.runtime_dir, private);
    assert_tree_omits(&local.runtime_dir, cli_private);

    let dispatch_key = Uuid::new_v4().to_string();
    let execution = match local
        .client
        .route_node_operation(
            &owner_id,
            RoutedOperation::RunAgentSchedule {
                schedule_id: schedule.id.clone(),
                dispatch_key: dispatch_key.clone(),
            },
        )
        .unwrap()
    {
        RoutedOperationResult::ScheduledExecution { execution, .. } => execution,
        result => panic!("unexpected run result: {result:?}"),
    };
    let duplicate = local
        .client
        .route_node_operation(
            &owner_id,
            RoutedOperation::RunAgentSchedule {
                schedule_id: schedule.id.clone(),
                dispatch_key,
            },
        )
        .unwrap();
    assert!(matches!(
        duplicate,
        RoutedOperationResult::ScheduledExecution { execution: ref value, .. }
            if value.id == execution.id
    ));
    let cli_inspect = local
        .command()
        .args([
            "execution",
            "inspect",
            &execution.id,
            "--node",
            "owner",
            "--json",
        ])
        .output()
        .unwrap();
    assert!(cli_inspect.status.success());
    assert!(!String::from_utf8_lossy(&cli_inspect.stdout).contains(private));
    let cli_inspect: serde_json::Value = serde_json::from_slice(&cli_inspect.stdout).unwrap();
    assert_eq!(cli_inspect["data"]["node_id"], owner_id);
    assert_eq!(cli_inspect["data"]["execution"]["id"], execution.id);
    let pid_path = owner.runtime_dir.join("schedule-pid");
    wait_until(|| pid_path.is_file(), "owner did not start scheduled host");
    let pid = fs::read_to_string(&pid_path)
        .unwrap()
        .parse::<libc::pid_t>()
        .unwrap();
    wait_until(
        || {
            owner
                .client
                .get_scheduled_execution(&execution.id)
                .is_ok_and(|value| value.state == ScheduledExecutionState::Active)
        },
        "owner execution did not become active",
    );

    local.stop_with_cli();
    assert!(process_exists(pid));
    assert_eq!(
        owner
            .client
            .get_scheduled_execution(&execution.id)
            .unwrap()
            .state,
        ScheduledExecutionState::Active
    );
    let local_path = format!("{}:/usr/bin:/bin", local.runtime_dir.display());
    local.restart_with(|command| {
        command.env("PATH", local_path);
    });

    let local_restart = local
        .command()
        .env(
            "PATH",
            format!("{}:/usr/bin:/bin", local.runtime_dir.display()),
        )
        .args(["daemon", "restart"])
        .output()
        .unwrap();
    assert!(local_restart.status.success());
    assert!(process_exists(pid));
    let owner_restart = owner
        .command()
        .args(["daemon", "restart"])
        .output()
        .unwrap();
    assert!(owner_restart.status.success());
    assert!(process_exists(pid));
    let current = owner.client.get_scheduled_execution(&execution.id).unwrap();
    assert_eq!(current.state, ScheduledExecutionState::Active);
    assert_eq!(current.run_id, execution.run_id);

    let exact_run = current.run_id.clone().unwrap();
    let exact_shell = current.shell_id.clone().unwrap();
    let attachment = local
        .client
        .attach_node(
            QualifiedIdentity::new(&owner_id, &exact_shell),
            true,
            false,
            Some(exact_run.clone()),
            profile(),
        )
        .unwrap();
    drop(attachment);

    let page = local
        .client
        .route_node_operation(
            &owner_id,
            RoutedOperation::ListScheduledExecutions {
                workspace_id: Some(workspace.id),
                schedule_id: Some(schedule.id),
                limit: 10,
            },
        )
        .unwrap();
    assert!(matches!(
        page,
        RoutedOperationResult::ScheduledExecutions { ref executions, .. }
            if executions.iter().any(|value| value.id == execution.id)
    ));
    let waited = local
        .client
        .route_node_operation(
            &owner_id,
            RoutedOperation::WaitScheduledExecution {
                execution_id: execution.id.clone(),
                after_revision: current.revision,
                wait_ms: 0,
            },
        )
        .unwrap();
    assert!(matches!(
        waited,
        RoutedOperationResult::ScheduledExecutionWait {
            changed: false,
            execution: ref value,
        } if value.id == execution.id
    ));
    assert!(
        local
            .client
            .route_node_operation(
                &owner_id,
                RoutedOperation::CancelScheduledExecution {
                    execution_id: execution.id.clone(),
                    expected_revision: current.revision - 1,
                },
            )
            .is_err()
    );
    assert!(process_exists(pid));
    let cancelled = local
        .client
        .route_node_operation(
            &owner_id,
            RoutedOperation::CancelScheduledExecution {
                execution_id: execution.id.clone(),
                expected_revision: current.revision,
            },
        )
        .unwrap();
    assert!(matches!(
        cancelled,
        RoutedOperationResult::ScheduledExecution { execution: ref value, .. }
            if value.state == ScheduledExecutionState::Cancelled
    ));
    wait_until(
        || !process_exists(pid),
        "owner cancellation did not stop exact host PID",
    );
    assert!(
        local
            .client
            .attach_node(
                QualifiedIdentity::new(&owner_id, exact_shell),
                true,
                false,
                Some(exact_run),
                profile(),
            )
            .is_err()
    );
    local.stop_with_cli();
    owner.stop_with_cli();
}

#[test]
fn remote_cold_restart_alone_applies_missed_policy_without_local_spawn() {
    const BASE_MS: u64 = 1_767_225_600_000;
    let mut owner = TestDaemon::start_with(|command, runtime_dir| {
        let clock = runtime_dir.join("boomux/remote-scheduler-clock");
        create_clock(&clock, 1, BASE_MS);
        command
            .env("BOOMUX_NATIVE_TEST_CLOCK", &clock)
            .env("PATH", "/nonexistent");
    });
    let clock = owner.runtime_dir.join("boomux/remote-scheduler-clock");
    wait_for_tick(&clock, 1);
    let owner_id = owner.client.node_identity().unwrap();
    let mut local = TestDaemon::start_with(|command, runtime_dir| {
        install_fake_ssh(runtime_dir, &owner);
        command.env("PATH", format!("{}:/usr/bin:/bin", runtime_dir.display()));
    });
    local
        .client
        .add_node_registration("owner", "fake-owner", &owner_id)
        .unwrap();
    let workspace = owner
        .client
        .create_workspace("remote-missed", Vec::new())
        .unwrap();
    let schedule = match local
        .client
        .route_node_operation(
            &owner_id,
            RoutedOperation::CreateAgentSchedule {
                workspace_id: workspace.id,
                spec: schedule_spec(
                    &owner.runtime_dir,
                    "PRIVATE MISSED PROMPT",
                    AgentScheduleState::Enabled,
                ),
            },
        )
        .unwrap()
    {
        RoutedOperationResult::AgentSchedule { schedule } => schedule,
        result => panic!("unexpected create result: {result:?}"),
    };
    owner.crash();
    write_tick(&clock, 2, BASE_MS + 180_000);
    let restart_clock = clock.clone();
    owner.restart_with(|command| {
        command
            .env("BOOMUX_NATIVE_TEST_CLOCK", restart_clock)
            .env("PATH", "/nonexistent");
    });
    wait_for_tick(&clock, 2);

    let result = local
        .client
        .route_node_operation(
            &owner_id,
            RoutedOperation::ListScheduledExecutions {
                workspace_id: None,
                schedule_id: Some(schedule.id),
                limit: 10,
            },
        )
        .unwrap();
    let RoutedOperationResult::ScheduledExecutions { executions, .. } = result else {
        panic!("unexpected execution list response");
    };
    assert_eq!(executions.len(), 1);
    assert_eq!(executions[0].state, ScheduledExecutionState::Skipped);
    assert_eq!(executions[0].reason, Some(ScheduledExecutionReason::Missed));
    assert_eq!(executions[0].scheduled_at_ms, Some(BASE_MS + 60_000));
    assert_eq!(executions[0].coalesced_through_ms, Some(BASE_MS + 180_000));
    assert!(local.client.snapshot().unwrap().workspaces.is_empty());
    assert_tree_omits(&local.runtime_dir, "PRIVATE MISSED PROMPT");
    local.stop_with_cli();
    owner.stop_with_cli();
}

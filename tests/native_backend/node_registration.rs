use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Command;
use std::sync::{Arc, Barrier};
use std::thread;
use std::time::Duration;

use boomux::client::{ClientError, RemoteError};
use boomux::protocol::{
    AgentScheduleOverlapPolicy, AgentScheduleSession, AgentScheduleSpec, AgentScheduleState,
    AgentScheduleTrigger, ErrorCode, Request, Response, ShellSpec, WorkspaceLauncherSpec,
};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::support::TestDaemon;

fn node_id(value: u128) -> String {
    Uuid::from_u128(value).to_string()
}

#[test]
fn coordinator_workspace_migrates_once_and_closes_after_owner_revision_changes() {
    let mut daemon = TestDaemon::start();
    let owner = daemon
        .client
        .create_workspace("migrated", Vec::new())
        .unwrap();
    daemon.stop_with_cli();

    fs::remove_file(
        daemon
            .runtime_dir
            .join("state/boomux/global_workspaces.json"),
    )
    .unwrap();
    daemon.restart();

    let combined = daemon.client.combined_node_snapshot(None).unwrap();
    assert_eq!(combined.workspaces.len(), 1);
    assert_eq!(combined.workspaces[0].id, owner.id);
    assert_eq!(combined.workspaces[0].placements.len(), 1);
    assert!(combined.external_workspaces.is_empty());
    let global = combined.workspaces[0].clone();

    daemon
        .client
        .create_launcher(
            &owner.id,
            WorkspaceLauncherSpec {
                name: "open-check".into(),
                cwd: std::env::temp_dir(),
                command: vec!["/bin/true".into()],
            },
        )
        .unwrap();
    let opened = daemon
        .command()
        .args(["workspace", "open", "migrated"])
        .output()
        .unwrap();
    assert!(
        opened.status.success(),
        "global Workspace open failed: {}",
        String::from_utf8_lossy(&opened.stderr)
    );
    assert!(String::from_utf8_lossy(&opened.stdout).contains("Opened 1 launcher(s)"));

    daemon
        .client
        .create_shell(
            &owner.id,
            ShellSpec {
                name: "later".into(),
                command: vec!["/bin/sh".into()],
                cwd: std::env::temp_dir(),
            },
        )
        .unwrap();
    let owner_after_mutation = daemon.client.get_workspace(&owner.id).unwrap();
    assert!(owner_after_mutation.revision > global.placements[0].owner_revision);

    let closed = daemon
        .client
        .close_global_workspace(&global.id, global.revision)
        .unwrap();
    assert_eq!(closed.placements.len(), 1);
    assert_eq!(closed.placements[0].status, "closed");
    assert!(daemon.client.snapshot().unwrap().workspaces.is_empty());
    assert!(
        daemon
            .client
            .combined_node_snapshot(None)
            .unwrap()
            .workspaces
            .is_empty()
    );
}

#[test]
fn first_resources_atomically_establish_one_exact_owner_placement() {
    let mut daemon = TestDaemon::start();
    let global = daemon.client.create_global_workspace("placed").unwrap();
    let combined = daemon.client.combined_node_snapshot(None).unwrap();
    let owner = combined
        .nodes
        .iter()
        .find(|node| node.local && node.workspace_owner_eligible)
        .unwrap();
    let owner_workspace_id = Uuid::new_v4().to_string();
    let cwd = std::env::temp_dir();
    assert!(
        daemon
            .client
            .create_global_workspace_shell(
                Uuid::new_v4().to_string(),
                &global.id,
                global.revision,
                &owner.node_id,
                Uuid::new_v4().to_string(),
                Some(cwd.clone()),
                Uuid::new_v4().to_string(),
                ShellSpec {
                    name: String::new(),
                    command: Vec::new(),
                    cwd: cwd.clone(),
                },
            )
            .is_err()
    );
    let operation_id = Uuid::new_v4().to_string();
    let shell_id = Uuid::new_v4().to_string();
    let shell_spec = ShellSpec {
        name: "first".into(),
        command: vec!["/bin/sh".into(), "-lc".into(), "printf '%s' safe".into()],
        cwd: cwd.clone(),
    };
    let (global, shell) = daemon
        .client
        .create_global_workspace_shell(
            &operation_id,
            &global.id,
            global.revision,
            &owner.node_id,
            &owner_workspace_id,
            Some(cwd.clone()),
            &shell_id,
            shell_spec.clone(),
        )
        .unwrap();
    assert_eq!(global.placements.len(), 1);
    assert_eq!(global.placements[0].workspace_id, owner_workspace_id);
    assert_eq!(
        global.placements[0].owner_workspace_name.as_deref(),
        Some("placed")
    );
    assert_eq!(shell.workspace_id, owner_workspace_id);
    let (replayed_global, replayed_shell) = daemon
        .client
        .create_global_workspace_shell(
            &operation_id,
            &global.id,
            global.revision - 1,
            &owner.node_id,
            &owner_workspace_id,
            Some(cwd.clone()),
            &shell_id,
            shell_spec,
        )
        .unwrap();
    assert_eq!(replayed_global, global);
    assert_eq!(replayed_shell, shell);
    daemon.crash();
    daemon.restart();
    let (restart_replay_global, restart_replay_shell) = daemon
        .client
        .create_global_workspace_shell(
            &operation_id,
            &global.id,
            global.revision - 1,
            &owner.node_id,
            &owner_workspace_id,
            Some(cwd.clone()),
            &shell_id,
            ShellSpec {
                name: "first".into(),
                command: vec!["/bin/sh".into(), "-lc".into(), "printf '%s' safe".into()],
                cwd: cwd.clone(),
            },
        )
        .unwrap();
    assert_eq!(restart_replay_global, global);
    assert_eq!(restart_replay_shell, shell);

    let global = daemon
        .client
        .rename_global_workspace(&global.id, global.revision, "renamed globally")
        .unwrap();
    assert_eq!(
        daemon
            .client
            .get_workspace(&owner_workspace_id)
            .unwrap()
            .name,
        "placed"
    );

    let launcher_id = Uuid::new_v4().to_string();
    let (global, launcher) = daemon
        .client
        .create_global_workspace_launcher(
            Uuid::new_v4().to_string(),
            &global.id,
            global.revision,
            &owner.node_id,
            &owner_workspace_id,
            Some(cwd.clone()),
            &launcher_id,
            WorkspaceLauncherSpec {
                name: "exact argv".into(),
                cwd: cwd.clone(),
                command: vec!["printf".into(), "%s".into(), "a b;$(private)".into()],
            },
        )
        .unwrap();
    assert_eq!(launcher.id, launcher_id);
    assert_eq!(launcher.command[2], "a b;$(private)");

    let schedule_id = Uuid::new_v4().to_string();
    let (_, schedule) = daemon
        .client
        .create_global_workspace_agent_schedule(
            Uuid::new_v4().to_string(),
            &global.id,
            global.revision,
            &owner.node_id,
            &owner_workspace_id,
            Some(cwd.clone()),
            &schedule_id,
            AgentScheduleSpec {
                name: "private".into(),
                cwd: cwd.clone(),
                integration: "opencode".into(),
                prompt: "PRIVATE FIRST PLACEMENT PROMPT".into(),
                session: AgentScheduleSession::Fresh,
                trigger: AgentScheduleTrigger {
                    cron: "0 2 * * *".into(),
                    timezone: "UTC".into(),
                },
                state: AgentScheduleState::Paused,
                overlap_policy: AgentScheduleOverlapPolicy::Skip,
            },
        )
        .unwrap();
    assert_eq!(schedule.id, schedule_id);
    let replay = daemon
        .client
        .request(Request::CreateWorkspaceAgentSchedule {
            workspace_id: owner_workspace_id.clone(),
            workspace_name: "placed".into(),
            default_cwd: Some(cwd.clone()),
            schedule_id: schedule_id.clone(),
            spec: AgentScheduleSpec {
                name: "private".into(),
                cwd: cwd.clone(),
                integration: "opencode".into(),
                prompt: "PRIVATE FIRST PLACEMENT PROMPT".into(),
                session: AgentScheduleSession::Fresh,
                trigger: AgentScheduleTrigger {
                    cron: "  0\t2  * * *  ".into(),
                    timezone: "  UTC\n".into(),
                },
                state: AgentScheduleState::Paused,
                overlap_policy: AgentScheduleOverlapPolicy::Skip,
            },
        })
        .unwrap();
    let Response::AgentSchedule { schedule: replay } = replay else {
        panic!("expected exact Schedule replay");
    };
    assert_eq!(replay, schedule);
    let snapshot = daemon.client.snapshot().unwrap();
    assert_eq!(snapshot.workspaces.len(), 1);
    assert_eq!(snapshot.workspaces[0].shells.len(), 1);
    assert_eq!(snapshot.workspaces[0].launchers.len(), 1);
    assert_eq!(snapshot.workspaces[0].schedules.len(), 1);
    assert!(
        !String::from_utf8_lossy(
            &fs::read(
                daemon
                    .runtime_dir
                    .join("state/boomux/global_workspaces.json")
            )
            .unwrap()
        )
        .contains("PRIVATE FIRST PLACEMENT PROMPT")
    );
}

#[test]
fn concurrent_first_resources_with_distinct_requested_owners_replay_canonical_successes() {
    let daemon = TestDaemon::start();
    let global = daemon
        .client
        .create_global_workspace("concurrent-first")
        .unwrap();
    let node_id = daemon.client.node_identity().unwrap();
    let barrier = Arc::new(Barrier::new(3));
    let mut requests = Vec::new();
    let mut handles = Vec::new();
    for offset in 0..2_u128 {
        let operation_id = Uuid::from_u128(100 + offset).to_string();
        let requested_owner_id = Uuid::from_u128(200 + offset).to_string();
        let shell_id = Uuid::from_u128(300 + offset).to_string();
        let shell = ShellSpec {
            name: format!("shell-{offset}"),
            command: Vec::new(),
            cwd: std::env::temp_dir(),
        };
        requests.push((
            operation_id.clone(),
            requested_owner_id.clone(),
            shell_id.clone(),
            shell.clone(),
        ));
        let client = daemon.client.clone();
        let barrier = Arc::clone(&barrier);
        let global = global.clone();
        let node_id = node_id.clone();
        handles.push(thread::spawn(move || {
            barrier.wait();
            client
                .create_global_workspace_shell(
                    operation_id,
                    &global.id,
                    global.revision,
                    node_id,
                    requested_owner_id,
                    Some(std::env::temp_dir()),
                    shell_id,
                    shell,
                )
                .unwrap()
        }));
    }
    barrier.wait();
    let first = handles.remove(0).join().unwrap();
    let second = handles.remove(0).join().unwrap();
    assert_eq!(first.0.placements.len(), 1);
    assert_eq!(second.0.placements.len(), 1);
    assert_eq!(
        first.0.placements[0].workspace_id,
        second.0.placements[0].workspace_id
    );
    assert_eq!(first.1.workspace_id, second.1.workspace_id);
    assert_ne!(requests[0].1, requests[1].1);
    assert!(
        requests
            .iter()
            .any(|request| request.1 == first.1.workspace_id)
    );

    for (index, (operation_id, requested_owner_id, shell_id, shell)) in
        requests.into_iter().enumerate()
    {
        let replay = daemon
            .client
            .create_global_workspace_shell(
                operation_id,
                &global.id,
                global.revision,
                &node_id,
                requested_owner_id,
                Some(std::env::temp_dir()),
                shell_id,
                shell,
            )
            .unwrap();
        let original = if index == 0 { &first } else { &second };
        assert_eq!(&replay, original);
    }
}

#[test]
fn concurrent_identical_workspace_resource_handlers_return_the_same_success() {
    let daemon = TestDaemon::start();
    let global = daemon
        .client
        .create_global_workspace("same-operation")
        .unwrap();
    let node_id = daemon.client.node_identity().unwrap();
    let operation_id = Uuid::new_v4().to_string();
    let owner_id = Uuid::new_v4().to_string();
    let shell_id = Uuid::new_v4().to_string();
    let shell = ShellSpec {
        name: "same".into(),
        command: Vec::new(),
        cwd: std::env::temp_dir(),
    };
    let barrier = Arc::new(Barrier::new(3));
    let mut handles = Vec::new();
    for _ in 0..2 {
        let client = daemon.client.clone();
        let global = global.clone();
        let node_id = node_id.clone();
        let operation_id = operation_id.clone();
        let owner_id = owner_id.clone();
        let shell_id = shell_id.clone();
        let shell = shell.clone();
        let barrier = Arc::clone(&barrier);
        handles.push(thread::spawn(move || {
            barrier.wait();
            client
                .create_global_workspace_shell(
                    operation_id,
                    &global.id,
                    global.revision,
                    node_id,
                    owner_id,
                    Some(std::env::temp_dir()),
                    shell_id,
                    shell,
                )
                .unwrap()
        }));
    }
    barrier.wait();
    let first = handles.remove(0).join().unwrap();
    let second = handles.remove(0).join().unwrap();
    assert_eq!(first, second);
    assert_eq!(
        daemon.client.snapshot().unwrap().workspaces[0].shells.len(),
        1
    );
}

#[test]
fn project_workspace_preflight_failure_does_not_reserve_the_name() {
    let daemon = TestDaemon::start();
    let local = daemon
        .client
        .combined_node_snapshot(None)
        .unwrap()
        .nodes
        .into_iter()
        .find(|node| node.local)
        .unwrap();
    assert!(
        daemon
            .client
            .create_global_workspace_with_shell(
                Uuid::new_v4().to_string(),
                Uuid::new_v4().to_string(),
                "missing-owner-project",
                Uuid::new_v4().to_string(),
                Uuid::new_v4().to_string(),
                "/tmp".into(),
                Uuid::new_v4().to_string(),
                ShellSpec {
                    name: "missing".into(),
                    cwd: "/tmp".into(),
                    command: Vec::new(),
                },
            )
            .is_err()
    );
    assert!(
        daemon
            .client
            .combined_node_snapshot(None)
            .unwrap()
            .workspaces
            .iter()
            .all(|workspace| workspace.name != "missing-owner-project")
    );
    let (recovered, recovered_shell) = daemon
        .client
        .create_global_workspace_with_shell(
            Uuid::new_v4().to_string(),
            Uuid::new_v4().to_string(),
            "missing-owner-project",
            &local.node_id,
            Uuid::new_v4().to_string(),
            "/tmp".into(),
            Uuid::new_v4().to_string(),
            ShellSpec {
                name: "valid-different-semantics".into(),
                cwd: "/tmp".into(),
                command: Vec::new(),
            },
        )
        .unwrap();
    assert_eq!(recovered.placements.len(), 1);
    assert_eq!(recovered_shell.name, "valid-different-semantics");
}

#[test]
fn attempted_project_survives_missing_registration_and_blocks_different_semantics() {
    let mut daemon = TestDaemon::start();
    let local_node_id = daemon.client.node_identity().unwrap();
    daemon.crash();

    let operation_id = Uuid::new_v4().to_string();
    let global_workspace_id = Uuid::new_v4().to_string();
    let missing_node_id = Uuid::new_v4().to_string();
    let owner_workspace_id = Uuid::new_v4().to_string();
    let shell_id = Uuid::new_v4().to_string();
    let shell = ShellSpec {
        name: "ambiguous".into(),
        cwd: "/tmp".into(),
        command: Vec::new(),
    };
    let request = Request::CreateGlobalWorkspaceWithShell {
        operation_id: operation_id.clone(),
        global_workspace_id: global_workspace_id.clone(),
        name: "attempted-project".into(),
        node_id: missing_node_id.clone(),
        owner_workspace_id: owner_workspace_id.clone(),
        default_cwd: "/tmp".into(),
        shell_id: shell_id.clone(),
        shell: shell.clone(),
    };
    let request_fingerprint = format!(
        "{:x}",
        Sha256::digest(serde_json::to_vec(&request).unwrap())
    );
    let path = daemon
        .runtime_dir
        .join("state/boomux/global_workspaces.json");
    fs::write(
        &path,
        serde_json::to_vec_pretty(&serde_json::json!({
            "version": 6,
            "local_migration_complete": true,
            "workspaces": [{
                "id": global_workspace_id,
                "revision": 1,
                "name": "attempted-project",
                "closing": false,
                "placements": []
            }],
            "pending_resources": [{
                "operation_id": operation_id,
                "creates_workspace": true,
                "request_fingerprint": request_fingerprint,
                "semantic_fingerprint": "b".repeat(64),
                "completion_reservation": " ".repeat(70_000),
                "owner_attempted": true,
                "global_workspace_id": global_workspace_id,
                "expected_global_revision": 1,
                "node_id": missing_node_id,
                "requested_owner_workspace_id": owner_workspace_id,
                "owner_workspace_id": owner_workspace_id,
                "owner_workspace_name": "attempted-project",
                "default_cwd": "/tmp",
                "resource_id": shell_id,
                "kind": "shell"
            }],
            "completed_operations": []
        }))
        .unwrap(),
    )
    .unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
    daemon.restart();

    assert!(
        daemon
            .client
            .create_global_workspace_with_shell(
                operation_id,
                global_workspace_id.clone(),
                "attempted-project",
                missing_node_id,
                owner_workspace_id,
                "/tmp".into(),
                shell_id,
                shell,
            )
            .is_err()
    );
    let retained = daemon
        .client
        .combined_node_snapshot(None)
        .unwrap()
        .workspaces
        .into_iter()
        .find(|workspace| workspace.id == global_workspace_id)
        .expect("attempted project must remain pending after registration loss");
    assert!(retained.placements.is_empty());

    assert!(
        daemon
            .client
            .create_global_workspace_with_shell(
                Uuid::new_v4().to_string(),
                Uuid::new_v4().to_string(),
                "attempted-project",
                local_node_id,
                Uuid::new_v4().to_string(),
                "/tmp".into(),
                Uuid::new_v4().to_string(),
                ShellSpec {
                    name: "different".into(),
                    cwd: "/tmp".into(),
                    command: Vec::new(),
                },
            )
            .is_err()
    );
    assert!(
        daemon
            .client
            .combined_node_snapshot(None)
            .unwrap()
            .workspaces
            .iter()
            .any(|workspace| workspace.id == global_workspace_id)
    );
}

fn contains_json_key(value: &serde_json::Value, key: &str) -> bool {
    match value {
        serde_json::Value::Object(values) => {
            values.contains_key(key) || values.values().any(|value| contains_json_key(value, key))
        }
        serde_json::Value::Array(values) => {
            values.iter().any(|value| contains_json_key(value, key))
        }
        _ => false,
    }
}

#[test]
fn registrations_survive_cold_recovery_outside_authoritative_state() {
    let mut daemon = TestDaemon::start();
    let local = daemon.client.node_identity().unwrap();
    let registration = daemon
        .client
        .add_node_registration("work", "work.example", node_id(2))
        .unwrap();
    assert_ne!(registration.node_id, local);
    let state_path = daemon.runtime_dir.join("state/boomux/state.json");
    if state_path.exists() {
        assert!(
            !fs::read_to_string(&state_path)
                .unwrap()
                .contains("work.example")
        );
    }
    daemon.crash();
    daemon.restart();

    assert_eq!(
        daemon.client.node_registration("work").unwrap(),
        registration
    );
    assert_eq!(daemon.client.snapshot().unwrap().workspaces.len(), 0);
}

#[test]
fn guarded_resource_revisions_survive_cold_recovery() {
    let mut daemon = TestDaemon::start();
    let workspace = daemon
        .client
        .create_workspace("guarded", Vec::new())
        .unwrap();
    assert_eq!(workspace.revision, 1);
    daemon
        .client
        .rename_workspace(&workspace.id, "guarded-renamed")
        .unwrap();
    let renamed = daemon.client.get_workspace(&workspace.id).unwrap();
    assert_eq!(renamed.revision, 2);

    daemon.crash();
    daemon.restart();
    let recovered = daemon.client.get_workspace(&workspace.id).unwrap();
    assert_eq!(recovered.name, "guarded-renamed");
    assert_eq!(recovered.revision, 2);
}

#[test]
fn combined_snapshot_is_separate_and_local_alias_ambiguity_is_typed() {
    let daemon = TestDaemon::start();
    daemon
        .client
        .add_node_registration("local", "other.example", node_id(3))
        .unwrap();
    let combined = daemon.client.combined_node_snapshot(None).unwrap();
    assert_eq!(combined.nodes.len(), 2);
    assert!(combined.nodes.iter().any(|node| node.local));
    assert!(combined.nodes.iter().any(|node| !node.local));

    let error = daemon
        .client
        .combined_node_snapshot(Some("local".into()))
        .unwrap_err();
    assert!(matches!(
        error,
        ClientError::Remote(RemoteError {
            code: Some(ErrorCode::AmbiguousTarget),
            ..
        })
    ));
}

#[test]
fn malformed_registration_store_preserves_local_daemon_operation() {
    let mut daemon = TestDaemon::start();
    daemon.crash();
    let path = daemon
        .runtime_dir
        .join("state/boomux/node_registrations.json");
    fs::write(&path, b"not-json").unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
    daemon.restart();

    daemon.client.ping().unwrap();
    daemon.client.snapshot().unwrap();
    let error = daemon.client.node_registrations().unwrap_err();
    assert!(matches!(
        error,
        ClientError::Remote(RemoteError {
            code: Some(ErrorCode::NodeRegistrationUnavailable),
            ..
        })
    ));
    assert_eq!(fs::read(&path).unwrap(), b"not-json");
}

#[test]
fn malformed_global_workspace_store_suppresses_capability_and_eligibility() {
    let mut daemon = TestDaemon::start();
    daemon.crash();
    let path = daemon
        .runtime_dir
        .join("state/boomux/global_workspaces.json");
    fs::write(&path, b"not-json").unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
    daemon.restart();

    daemon.client.ping().unwrap();
    let combined = daemon.client.combined_node_snapshot(None).unwrap();
    let local = combined.nodes.iter().find(|node| node.local).unwrap();
    assert!(!local.workspace_owner_eligible);
    assert!(
        local
            .workspace_owner_unavailable_reason
            .as_deref()
            .unwrap()
            .contains("storage")
    );
    assert!(
        !local
            .observed_capabilities
            .iter()
            .any(|capability| capability == "global_workspaces")
    );
    assert!(daemon.client.create_global_workspace("disabled").is_err());
    assert_eq!(fs::read(&path).unwrap(), b"not-json");
}

#[test]
fn cli_global_close_treats_missing_local_owner_as_already_removed() {
    let daemon = TestDaemon::start();
    let global = daemon
        .client
        .create_global_workspace("stale-local")
        .unwrap();
    let local = daemon
        .client
        .combined_node_snapshot(None)
        .unwrap()
        .nodes
        .into_iter()
        .find(|node| node.local)
        .unwrap();
    let owner_workspace_id = Uuid::new_v4().to_string();
    let (global, _) = daemon
        .client
        .create_global_workspace_shell(
            Uuid::new_v4().to_string(),
            &global.id,
            global.revision,
            &local.node_id,
            &owner_workspace_id,
            Some("/tmp".into()),
            Uuid::new_v4().to_string(),
            ShellSpec {
                name: "stale".into(),
                cwd: "/tmp".into(),
                command: Vec::new(),
            },
        )
        .unwrap();
    daemon.client.close_workspace(&owner_workspace_id).unwrap();
    let close = daemon
        .command()
        .env("BOOMUX_SHELL_ID", Uuid::new_v4().to_string())
        .args(["workspace", "close", &global.id])
        .output()
        .unwrap();
    assert!(
        close.status.success(),
        "global close rejected missing local owner: {}",
        String::from_utf8_lossy(&close.stderr)
    );
}

fn write_fake_handshake(directory: &Path, core_protocol_version: u32) {
    use boomux::federation::{
        FEDERATION_VERSION, FederationConnectionMode, FederationHandshake, write_handshake,
    };
    let handshake = FederationHandshake {
        version: FEDERATION_VERSION,
        node_id: node_id(2),
        helper_version: env!("CARGO_PKG_VERSION").into(),
        core_protocol_version,
        connection_mode: FederationConnectionMode::AdHoc,
    };
    let mut handshake_bytes = Vec::new();
    write_handshake(&mut handshake_bytes, &handshake).unwrap();
    fs::write(directory.join("handshake.bin"), handshake_bytes).unwrap();
}

fn write_fake_combined_snapshot(directory: &Path, workspace_owner_eligible: bool) {
    use boomux::protocol::{
        self, CombinedNode, CombinedNodeSnapshot, Envelope, NodeProjectionHealthCode, Response,
        SchedulerHealth, SchedulerState, Snapshot, WorkspaceSnapshot,
    };

    let unavailable_reason =
        (!workspace_owner_eligible).then(|| "coordinated Workspace storage is unavailable".into());
    let capabilities = if workspace_owner_eligible {
        vec!["protocol_38".into(), "global_workspaces".into()]
    } else {
        vec!["protocol_38".into()]
    };
    let scheduler = SchedulerHealth {
        state: SchedulerState::Active,
        max_concurrent: 4,
        active_executions: 0,
    };
    let mut response = Vec::new();
    protocol::write_message(
        &mut response,
        &Envelope::with_version(
            protocol::PROTOCOL_VERSION,
            Response::CombinedNodeSnapshot {
                snapshot: CombinedNodeSnapshot {
                    nodes: vec![CombinedNode {
                        node_id: node_id(2),
                        alias: "local".into(),
                        local: true,
                        route: None,
                        registration_revision: None,
                        health: NodeProjectionHealthCode::Online,
                        current: true,
                        stale: false,
                        observed_at_ms: 1,
                        observed_protocol_version: Some(protocol::PROTOCOL_VERSION),
                        observed_capabilities: capabilities,
                        workspace_owner_eligible,
                        workspace_owner_unavailable_reason: unavailable_reason,
                        scheduler: scheduler.clone(),
                        local_snapshot: Some(Snapshot {
                            workspaces: vec![WorkspaceSnapshot {
                                id: "shared-workspace".into(),
                                revision: 7,
                                name: "shared-private".into(),
                                default_cwd: Some("/remote/private".into()),
                                shells: Vec::new(),
                                launchers: Vec::new(),
                                agents: Vec::new(),
                                schedules: Vec::new(),
                            }],
                            focused_terminal: None,
                            scheduler: Some(scheduler),
                        }),
                        remote_projection: None,
                    }],
                    workspaces: Vec::new(),
                    external_workspaces: Vec::new(),
                },
            },
        ),
    )
    .unwrap();
    fs::write(directory.join("combined.bin"), response).unwrap();
}

fn fake_ssh(directory: &Path) {
    use boomux::protocol::{
        self, Envelope, EventCursor, NodeProjectionShell, NodeProjectionSnapshot,
        NodeProjectionSync, NodeProjectionSyncMode, NodeProjectionWorkspace, Response,
        SchedulerHealth, SchedulerState, ShellOwner, ShellStatus,
    };

    write_fake_handshake(directory, protocol::PROTOCOL_VERSION);
    write_fake_combined_snapshot(directory, true);
    let mut ping = Vec::new();
    protocol::write_message(
        &mut ping,
        &Envelope::with_version(protocol::PROTOCOL_VERSION, Response::Pong),
    )
    .unwrap();
    let mut sync = Vec::new();
    protocol::write_message(
        &mut sync,
        &Envelope::with_version(
            protocol::PROTOCOL_VERSION,
            Response::NodeProjectionSync {
                sync: NodeProjectionSync {
                    mode: NodeProjectionSyncMode::Baseline,
                    cursor: EventCursor {
                        stream_id: Uuid::from_u128(20).to_string(),
                        event_id: 0,
                    },
                    projection: NodeProjectionSnapshot {
                        node_id: node_id(2),
                        workspaces: vec![NodeProjectionWorkspace {
                            id: "shared-workspace".into(),
                            name: "shared".into(),
                            item_count: 1,
                            attention_count: 0,
                        }],
                        shells: vec![NodeProjectionShell {
                            id: "shared-shell".into(),
                            workspace_id: "shared-workspace".into(),
                            name: "shared".into(),
                            owner: ShellOwner::User,
                            status: ShellStatus::Running,
                            run_id: Some("shared-run".into()),
                            generation: Some(1),
                            started_at_ms: Some(1),
                            ended_at_ms: None,
                        }],
                        launchers: Vec::new(),
                        agents: Vec::new(),
                        schedules: Vec::new(),
                        executions: Vec::new(),
                        executions_truncated: false,
                        scheduler: SchedulerHealth {
                            state: SchedulerState::Active,
                            max_concurrent: 4,
                            active_executions: 0,
                        },
                    },
                    transitions: Vec::new(),
                    capabilities: vec!["protocol_38".into(), "global_workspaces".into()],
                },
            },
        ),
    )
    .unwrap();
    let mut workspace = Vec::new();
    protocol::write_message(
        &mut workspace,
        &Envelope::with_version(
            protocol::PROTOCOL_VERSION,
            Response::Workspace {
                workspace: protocol::WorkspaceSnapshot {
                    id: "shared-workspace".into(),
                    revision: 7,
                    name: "shared-private".into(),
                    default_cwd: Some("/remote/private".into()),
                    shells: Vec::new(),
                    launchers: Vec::new(),
                    agents: Vec::new(),
                    schedules: Vec::new(),
                },
            },
        ),
    )
    .unwrap();
    fs::write(directory.join("pong.bin"), ping).unwrap();
    fs::write(directory.join("sync.bin"), sync).unwrap();
    fs::write(directory.join("workspace.bin"), workspace).unwrap();
    let ssh = directory.join("ssh");
    fs::write(
        &ssh,
        format!(
            "#!/bin/sh\nlast=\nfor arg do last=$arg; done\ncase \"$last\" in\n  *boomux-platform-v1*) printf 'boomux-platform-v1\\0Linux\\0x86_64\\0' ;;\n  *boomux-executables-v1*) printf 'boomux-executables-v1\\0/remote/boomux\\0' ;;\n  *boomux-install-destination-v1*) printf 'boomux-install-destination-v1\\0/home/remote/.local/bin/boomux\\0' ;;\n  \"'/remote/boomux' __federation-stdio\") cat \"{}\"; python3 -c 'import json,struct,sys; data=sys.stdin.buffer.read(struct.unpack(\">I\",sys.stdin.buffer.read(4))[0]); request=json.loads(data)[\"message\"][\"request\"]; paths={{\"ping\":sys.argv[1],\"sync_node_projection\":sys.argv[2],\"get_workspace\":sys.argv[3],\"get_combined_node_snapshot\":sys.argv[4]}}; sys.stdout.buffer.write(open(paths[request],\"rb\").read())' \"{}\" \"{}\" \"{}\" \"{}\" ;;\n  *) exit 64 ;;\nesac\n",
            directory.join("handshake.bin").display(),
            directory.join("pong.bin").display(),
            directory.join("sync.bin").display(),
            directory.join("workspace.bin").display(),
            directory.join("combined.bin").display(),
        ),
    )
    .unwrap();
    fs::set_permissions(&ssh, fs::Permissions::from_mode(0o700)).unwrap();
}

fn command(directory: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_boomux"));
    command
        .env("PATH", format!("{}:/usr/bin:/bin", directory.display()))
        .env("HOME", directory.join("home"))
        .env("XDG_RUNTIME_DIR", directory.join("runtime"))
        .env("XDG_STATE_HOME", directory.join("state"));
    command
}

#[test]
fn cli_add_and_retarget_pin_verified_identity_without_persisting_helper_path() {
    let id = Uuid::new_v4().simple().to_string();
    let directory = std::env::temp_dir().join(format!("bx-n-{}-{}", std::process::id(), &id[..8]));
    fs::create_dir_all(directory.join("home/.ssh")).unwrap();
    fs::create_dir_all(directory.join("runtime")).unwrap();
    fake_ssh(&directory);

    let add = command(&directory)
        .args(["node", "add", "work", "workbox"])
        .output()
        .unwrap();
    assert!(
        add.status.success(),
        "node add failed: {}",
        String::from_utf8_lossy(&add.stderr)
    );
    let list = command(&directory)
        .args(["node", "list", "--json"])
        .output()
        .unwrap();
    assert!(list.status.success());
    let list: serde_json::Value = serde_json::from_slice(&list.stdout).unwrap();
    assert_eq!(list["data"][0]["node_id"], node_id(2));
    let revision = list["data"][0]["revision"].as_u64().unwrap();

    let mut online = None;
    let mut last_inspect = None;
    for _ in 0..200 {
        let inspect = command(&directory)
            .args(["node", "inspect", "work", "--json"])
            .output()
            .unwrap();
        assert!(inspect.status.success());
        let inspect: serde_json::Value = serde_json::from_slice(&inspect.stdout).unwrap();
        if inspect["data"]["projection"]["code"] == "online" {
            online = Some(inspect);
            break;
        }
        last_inspect = Some(inspect);
        thread::sleep(Duration::from_millis(25));
    }
    let online = online
        .unwrap_or_else(|| panic!("background projection did not become online: {last_inspect:?}"));
    assert_eq!(online["data"]["projection"]["cursor"], 0);
    let routed =
        boomux::client::Client::from_socket_path(directory.join("runtime/boomux/daemon.sock"))
            .route_node_operation(
                node_id(2),
                boomux::protocol::RoutedOperation::GetWorkspace {
                    workspace_id: "shared-workspace".into(),
                },
            )
            .unwrap();
    let boomux::protocol::RoutedOperationResult::Workspace { workspace } = routed else {
        panic!("expected routed workspace");
    };
    assert_eq!(workspace.revision, 7);
    assert_eq!(
        workspace.default_cwd.as_deref(),
        Some(Path::new("/remote/private"))
    );
    let combined = command(&directory)
        .args(["node", "snapshot", "--json"])
        .output()
        .unwrap();
    assert!(
        combined.status.success(),
        "node snapshot failed: {}",
        String::from_utf8_lossy(&combined.stderr)
    );
    let combined: serde_json::Value = serde_json::from_slice(&combined.stdout).unwrap();
    assert_eq!(combined["command"], "node.snapshot");
    assert_eq!(combined["data"]["nodes"].as_array().unwrap().len(), 2);
    let remote = combined["data"]["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|node| node["alias"] == "work")
        .unwrap();
    assert_eq!(remote["health"], "online");
    assert_eq!(remote["current"], true);
    assert_eq!(
        remote["remote_projection"]["workspaces"][0]["id"]["node_id"],
        node_id(2)
    );
    assert_eq!(
        remote["remote_projection"]["workspaces"][0]["id"]["inner_id"],
        "shared-workspace"
    );
    assert_eq!(
        remote["remote_projection"]["shells"][0]["workspace_id"]["inner_id"],
        "shared-workspace"
    );
    assert_eq!(remote["workspace_owner_eligible"], true);
    write_fake_handshake(&directory, 37);
    let client =
        boomux::client::Client::from_socket_path(directory.join("runtime/boomux/daemon.sock"));
    let adopt = client
        .adopt_node_workspace(
            boomux::protocol::QualifiedIdentity::new(node_id(2), "shared-workspace"),
            7,
        )
        .unwrap_err();
    assert!(matches!(
        adopt,
        ClientError::Remote(RemoteError {
            code: Some(ErrorCode::UnsupportedVersion),
            ..
        })
    ));
    write_fake_handshake(&directory, boomux::protocol::PROTOCOL_VERSION);
    write_fake_combined_snapshot(&directory, false);
    let unavailable = client
        .adopt_node_workspace(
            boomux::protocol::QualifiedIdentity::new(node_id(2), "shared-workspace"),
            7,
        )
        .unwrap_err();
    assert!(matches!(
        unavailable,
        ClientError::Remote(RemoteError {
            code: Some(ErrorCode::UnsupportedVersion),
            ..
        })
    ));
    write_fake_combined_snapshot(&directory, true);
    let cache: serde_json::Value =
        serde_json::from_slice(&fs::read(directory.join("state/boomux/node-cache.json")).unwrap())
            .unwrap();
    for private in [
        "cwd",
        "command",
        "prompt",
        "evidence",
        "environment",
        "external_session_id",
        "runner_token",
    ] {
        assert!(!contains_json_key(&cache, private));
    }

    let retarget = command(&directory)
        .args([
            "node",
            "retarget",
            "work",
            "otherbox",
            "--revision",
            &revision.to_string(),
        ])
        .output()
        .unwrap();
    assert!(
        retarget.status.success(),
        "node retarget failed: {}",
        String::from_utf8_lossy(&retarget.stderr)
    );
    let persisted =
        fs::read_to_string(directory.join("state/boomux/node_registrations.json")).unwrap();
    assert!(persisted.contains("otherbox"));
    assert!(!persisted.contains("/remote/boomux"));

    let restart = command(&directory)
        .args(["daemon", "restart"])
        .output()
        .unwrap();
    assert!(restart.status.success());
    let inspect = command(&directory)
        .args(["node", "inspect", "work", "--json"])
        .output()
        .unwrap();
    assert!(inspect.status.success());
    let inspect: serde_json::Value = serde_json::from_slice(&inspect.stdout).unwrap();
    assert!(
        inspect["data"]["projection"]["cache_generation"]
            .as_u64()
            .is_some_and(|generation| generation > 0)
    );

    let stop = command(&directory)
        .args(["daemon", "stop"])
        .output()
        .unwrap();
    assert!(stop.status.success());
    fs::remove_dir_all(directory).unwrap();
}

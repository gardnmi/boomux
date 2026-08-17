use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Command;
use std::thread;
use std::time::Duration;

use boomux::client::{ClientError, RemoteError};
use boomux::protocol::ErrorCode;
use uuid::Uuid;

use crate::support::TestDaemon;

fn node_id(value: u128) -> String {
    Uuid::from_u128(value).to_string()
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

fn shell_printf(bytes: &[u8]) -> String {
    let escaped = bytes
        .iter()
        .map(|byte| format!("\\{byte:03o}"))
        .collect::<String>();
    format!("printf '{escaped}'")
}

fn fake_ssh(directory: &Path) {
    use boomux::federation::{
        FEDERATION_VERSION, FederationConnectionMode, FederationHandshake, write_handshake,
    };
    use boomux::protocol::{
        self, Envelope, EventCursor, NodeProjectionShell, NodeProjectionSnapshot,
        NodeProjectionSync, NodeProjectionSyncMode, NodeProjectionWorkspace, Response,
        SchedulerHealth, SchedulerState, ShellOwner, ShellStatus,
    };

    let handshake = FederationHandshake {
        version: FEDERATION_VERSION,
        node_id: node_id(2),
        helper_version: env!("CARGO_PKG_VERSION").into(),
        core_protocol_version: protocol::PROTOCOL_VERSION,
        connection_mode: FederationConnectionMode::AdHoc,
    };
    let mut handshake_bytes = Vec::new();
    write_handshake(&mut handshake_bytes, &handshake).unwrap();
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
                },
            },
        ),
    )
    .unwrap();
    fs::write(directory.join("pong.bin"), ping).unwrap();
    fs::write(directory.join("sync.bin"), sync).unwrap();
    let ssh = directory.join("ssh");
    fs::write(
        &ssh,
        format!(
            "#!/bin/sh\nlast=\nfor arg do last=$arg; done\ncase \"$last\" in\n  *boomux-platform-v1*) printf 'boomux-platform-v1\\0Linux\\0x86_64\\0' ;;\n  *boomux-executables-v1*) printf 'boomux-executables-v1\\0/remote/boomux\\0' ;;\n  *boomux-install-destination-v1*) printf 'boomux-install-destination-v1\\0/home/remote/.local/bin/boomux\\0' ;;\n  \"'/remote/boomux' __federation-stdio\") {}; python3 -c 'import json,struct,sys; data=sys.stdin.buffer.read(struct.unpack(\">I\",sys.stdin.buffer.read(4))[0]); request=json.loads(data)[\"message\"][\"request\"]; path=sys.argv[1] if request==\"ping\" else sys.argv[2]; sys.stdout.buffer.write(open(path,\"rb\").read())' \"{}\" \"{}\" ;;\n  *) exit 64 ;;\nesac\n",
            shell_printf(&handshake_bytes),
            directory.join("pong.bin").display(),
            directory.join("sync.bin").display(),
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

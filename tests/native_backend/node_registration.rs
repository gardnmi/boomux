use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Command;

use boomux::client::{ClientError, RemoteError};
use boomux::protocol::ErrorCode;
use uuid::Uuid;

use crate::support::TestDaemon;

fn node_id(value: u128) -> String {
    Uuid::from_u128(value).to_string()
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
    use boomux::protocol::{self, Envelope, Request, Response};

    let handshake = FederationHandshake {
        version: FEDERATION_VERSION,
        node_id: node_id(2),
        helper_version: env!("CARGO_PKG_VERSION").into(),
        core_protocol_version: protocol::PROTOCOL_VERSION,
        connection_mode: FederationConnectionMode::AdHoc,
    };
    let mut handshake_bytes = Vec::new();
    write_handshake(&mut handshake_bytes, &handshake).unwrap();
    let mut request = Vec::new();
    protocol::write_message(
        &mut request,
        &Envelope::with_version(protocol::PROTOCOL_VERSION, Request::Ping),
    )
    .unwrap();
    let mut response = Vec::new();
    protocol::write_message(
        &mut response,
        &Envelope::with_version(protocol::PROTOCOL_VERSION, Response::Pong),
    )
    .unwrap();
    let ssh = directory.join("ssh");
    fs::write(
        &ssh,
        format!(
            "#!/bin/sh\nlast=\nfor arg do last=$arg; done\ncase \"$last\" in\n  *boomux-platform-v1*) printf 'boomux-platform-v1\\0Linux\\0x86_64\\0' ;;\n  *boomux-executables-v1*) printf 'boomux-executables-v1\\0/remote/boomux\\0' ;;\n  *boomux-install-destination-v1*) printf 'boomux-install-destination-v1\\0/home/remote/.local/bin/boomux\\0' ;;\n  \"'/remote/boomux' __federation-stdio\") {}; dd bs=1 count={} of=/dev/null 2>/dev/null; {} ;;\n  *) exit 64 ;;\nesac\n",
            shell_printf(&handshake_bytes),
            request.len(),
            shell_printf(&response),
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

    let stop = command(&directory)
        .args(["daemon", "stop"])
        .output()
        .unwrap();
    assert!(stop.status.success());
    fs::remove_dir_all(directory).unwrap();
}

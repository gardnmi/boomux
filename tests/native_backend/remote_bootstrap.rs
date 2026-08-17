use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Command;

use boomux::federation::{
    FEDERATION_VERSION, FederationConnectionMode, FederationHandshake, write_handshake,
};
use boomux::protocol::{self, Envelope, Request, Response};
use uuid::Uuid;

fn shell_printf(bytes: &[u8]) -> String {
    let escaped = bytes
        .iter()
        .map(|byte| format!("\\{byte:03o}"))
        .collect::<String>();
    format!("printf '{escaped}'")
}

fn fake_ssh(
    directory: &Path,
    executables: &str,
    disconnect_second_helper: bool,
) -> std::path::PathBuf {
    let handshake = FederationHandshake {
        version: FEDERATION_VERSION,
        node_id: "550e8400-e29b-41d4-a716-446655440000".into(),
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
    let log = directory.join("ssh.log");
    let count = directory.join("helper.count");
    let ssh = directory.join("ssh");
    let helper = if disconnect_second_helper {
        format!(
            "count=0; [ ! -f '{}' ] || count=$(cat '{}'); count=$((count + 1)); printf '%s' \"$count\" > '{}'; {}; dd bs=1 count={} of=/dev/null 2>/dev/null; [ \"$count\" -lt 2 ] && {}",
            count.display(),
            count.display(),
            count.display(),
            shell_printf(&handshake_bytes),
            request.len(),
            shell_printf(&response),
        )
    } else {
        format!(
            "{}; dd bs=1 count={} of=/dev/null 2>/dev/null; {}",
            shell_printf(&handshake_bytes),
            request.len(),
            shell_printf(&response),
        )
    };
    fs::write(
        &ssh,
        format!(
            "#!/bin/sh\nlast=\nfor arg do last=$arg; done\nprintf '%s\\n' \"$last\" >> '{}'\ncase \"$last\" in\n  *boomux-platform-v1*) printf 'boomux-platform-v1\\0Linux\\0x86_64\\0' ;;\n  *boomux-executables-v1*) printf 'boomux-executables-v1\\0{}' ;;\n  *boomux-install-destination-v1*) printf 'boomux-install-destination-v1\\0/home/remote/.local/bin/boomux\\0' ;;\n  \"'/remote/boomux' __federation-stdio\") {} ;;\n  *) exit 64 ;;\nesac\n",
            log.display(),
            executables,
            helper,
        ),
    )
    .unwrap();
    fs::set_permissions(&ssh, fs::Permissions::from_mode(0o700)).unwrap();
    ssh
}

fn command(directory: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_boomux"));
    command
        .args(["--remote", "workbox"])
        .env("PATH", format!("{}:/usr/bin:/bin", directory.display()))
        .env("HOME", directory.join("home"))
        .env("XDG_RUNTIME_DIR", directory.join("runtime"))
        .env("XDG_STATE_HOME", directory.join("state"));
    command
}

fn test_directory() -> std::path::PathBuf {
    let id = Uuid::new_v4().simple().to_string();
    std::env::temp_dir().join(format!("bx-r-{}-{}", std::process::id(), &id[..8]))
}

#[test]
fn public_remote_uses_verified_stdio_protocol_channel() {
    let directory = test_directory();
    fs::create_dir_all(directory.join("home/.ssh")).unwrap();
    fs::create_dir_all(directory.join("runtime")).unwrap();
    fake_ssh(&directory, "/remote/boomux\\0", false);

    let output = command(&directory).output().unwrap();
    assert!(
        output.status.success(),
        "remote command failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("Connected to Boomux Node 550e8400-e29b-41d4-a716-446655440000"));
    let log = fs::read_to_string(directory.join("ssh.log")).unwrap();
    assert_eq!(log.matches("__federation-stdio").count(), 2);
    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn noninteractive_remote_refuses_install_without_modification() {
    let directory = test_directory();
    fs::create_dir_all(directory.join("home/.ssh")).unwrap();
    fs::create_dir_all(directory.join("runtime")).unwrap();
    fake_ssh(&directory, "", false);

    let output = command(&directory).output().unwrap();
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .contains("noninteractive --remote never modifies remote software")
    );
    let log = fs::read_to_string(directory.join("ssh.log")).unwrap();
    assert_eq!(log.lines().count(), 3);
    assert!(!log.contains("mktemp"));
    assert!(!log.contains("daemon restart"));
    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn remote_disconnect_after_handshake_fails_without_hanging() {
    let directory = test_directory();
    fs::create_dir_all(directory.join("home/.ssh")).unwrap();
    fs::create_dir_all(directory.join("runtime")).unwrap();
    fake_ssh(&directory, "/remote/boomux\\0", true);

    let output = command(&directory).output().unwrap();
    assert!(!output.status.success());
    assert!(!String::from_utf8_lossy(&output.stdout).contains("Connected to Boomux Node"));
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("failed to fill whole buffer"),
        "unexpected disconnect error: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    fs::remove_dir_all(directory).unwrap();
}

use std::fs;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use boomux::protocol::ShellSpec;

use crate::support::{TestDaemon, profile};

struct WebCleanup {
    executable: PathBuf,
    runtime_dir: PathBuf,
    port: String,
}

impl WebCleanup {
    fn new(daemon: &TestDaemon, port: String) -> Self {
        Self {
            executable: daemon.executable.clone(),
            runtime_dir: daemon.runtime_dir.clone(),
            port,
        }
    }
}

impl Drop for WebCleanup {
    fn drop(&mut self) {
        let _ = Command::new(&self.executable)
            .args(["web", "stop", "--port", &self.port, "--json"])
            .env("XDG_RUNTIME_DIR", &self.runtime_dir)
            .env("XDG_CONFIG_HOME", self.runtime_dir.join("config"))
            .env("XDG_STATE_HOME", self.runtime_dir.join("state"))
            .output();
    }
}

fn unused_port() -> u16 {
    TcpListener::bind(("127.0.0.1", 0))
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn run_web(daemon: &TestDaemon, arguments: &[&str], path: &Path) -> std::process::Output {
    let mut command = daemon.command();
    command.args(arguments).env("PATH", path);
    command.output().unwrap()
}

#[test]
fn web_starts_without_opencode_and_repeats_idempotently() {
    let mut daemon = TestDaemon::start();
    let empty_path = daemon.runtime_dir.join("empty-path");
    fs::create_dir(&empty_path).unwrap();
    let workspace = daemon
        .client
        .create_workspace(
            "claude-web-without-opencode",
            vec![ShellSpec {
                name: "claude".into(),
                command: vec!["/bin/sleep".into(), "30".into()],
                cwd: std::env::temp_dir(),
            }],
        )
        .unwrap();
    let shell_id = workspace.shells[0].id.clone();
    let _attachment = daemon.client.attach(&shell_id, false, profile()).unwrap();
    let run_id = daemon.client.get_shell(&shell_id).unwrap().run.unwrap().id;
    let mut hook = daemon.command();
    hook.args(["claude", "hook"])
        .env("BOOMUX_SHELL_ID", &shell_id)
        .env("BOOMUX_RUN_ID", &run_id)
        .env("CLAUDE_CODE_BRIDGE_SESSION_ID", "bridge-without-opencode")
        .stdin(Stdio::piped());
    let mut hook = hook.spawn().unwrap();
    write!(
        hook.stdin.take().unwrap(),
        "{{\"session_id\":\"claude-without-opencode\",\"hook_event_name\":\"SessionStart\"}}"
    )
    .unwrap();
    assert!(hook.wait().unwrap().success());

    let dashboard_port = unused_port();
    let opencode_port = unused_port();
    assert_ne!(dashboard_port, opencode_port);
    let dashboard_port_value = dashboard_port;
    let opencode_port_value = opencode_port;
    let dashboard_port = dashboard_port_value.to_string();
    let opencode_port = opencode_port_value.to_string();
    let arguments = [
        "web",
        "start",
        "--port",
        &dashboard_port,
        "--opencode-web-port",
        &opencode_port,
        "--json",
    ];
    let cleanup = WebCleanup::new(&daemon, dashboard_port.clone());

    let started = run_web(&daemon, &arguments, &empty_path);
    assert!(
        started.status.success(),
        "web start failed: {}",
        String::from_utf8_lossy(&started.stderr)
    );
    let started: serde_json::Value = serde_json::from_slice(&started.stdout).unwrap();
    assert_eq!(started["data"]["running"], true);
    assert_eq!(started["data"]["changed"], true);
    assert_eq!(
        started["data"]["opencode_requested_port"],
        opencode_port_value
    );
    assert_eq!(
        started["data"]["opencode_requested_url"],
        format!("http://127.0.0.1:{opencode_port}")
    );
    assert!(
        started["data"]["opencode_port"].is_null(),
        "unexpected OpenCode runtime: {started}"
    );
    assert!(started["data"]["opencode_url"].is_null());
    assert!(TcpListener::bind(("127.0.0.1", dashboard_port_value)).is_err());
    let mut stream = TcpStream::connect(("127.0.0.1", dashboard_port_value)).unwrap();
    stream
        .write_all(b"GET /api/snapshot HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n")
        .unwrap();
    let mut response = Vec::new();
    stream.read_to_end(&mut response).unwrap();
    let body = response
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .map(|index| &response[index + 4..])
        .unwrap();
    let snapshot: serde_json::Value = serde_json::from_slice(body).unwrap();
    let claude = snapshot["agents"]
        .as_array()
        .unwrap()
        .iter()
        .find(|agent| agent["integration"] == "claude")
        .unwrap();
    assert_eq!(claude["native_web"]["label"], "Open in Claude");
    assert_eq!(
        claude["native_web"]["url"],
        "https://claude.ai/code/bridge-without-opencode"
    );

    let repeated = run_web(&daemon, &arguments, &empty_path);
    assert!(repeated.status.success());
    let repeated: serde_json::Value = serde_json::from_slice(&repeated.stdout).unwrap();
    assert_eq!(repeated["data"]["changed"], false);

    let stopped = run_web(
        &daemon,
        &["web", "stop", "--port", &dashboard_port, "--json"],
        &empty_path,
    );
    assert!(stopped.status.success());
    drop(cleanup);
    daemon.stop_with_cli();
}

#[test]
fn web_keeps_an_opencode_port_conflict_fatal() {
    let mut daemon = TestDaemon::start();
    let empty_path = daemon.runtime_dir.join("empty-path");
    fs::create_dir(&empty_path).unwrap();
    let dashboard_port = unused_port();
    let occupied = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let dashboard_port = dashboard_port.to_string();
    let opencode_port = occupied.local_addr().unwrap().port().to_string();
    let cleanup = WebCleanup::new(&daemon, dashboard_port.clone());

    let output = run_web(
        &daemon,
        &[
            "web",
            "start",
            "--port",
            &dashboard_port,
            "--opencode-web-port",
            &opencode_port,
            "--json",
        ],
        &empty_path,
    );
    assert!(!output.status.success());
    let error: serde_json::Value = serde_json::from_slice(&output.stderr).unwrap();
    assert!(
        error["error"]["message"]
            .as_str()
            .unwrap()
            .contains("exited before becoming ready")
    );

    drop(cleanup);
    daemon.stop_with_cli();
}

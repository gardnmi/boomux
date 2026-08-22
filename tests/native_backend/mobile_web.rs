use std::fs;
use std::io::{self, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use boomux::protocol::{AgentAuthority, AgentRegistrationSpec, AgentReport, AgentState, ShellSpec};
use portable_pty::{CommandBuilder, PtySize, native_pty_system};
use tungstenite::client::IntoClientRequest;
use tungstenite::http::{HeaderValue, header};
use tungstenite::{Message, connect};

use crate::support::{TIMEOUT, TestDaemon, contains, profile, wait_until};

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

fn http_json(port: u16, request: &[u8]) -> serde_json::Value {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
    stream.write_all(request).unwrap();
    let mut response = Vec::new();
    stream.read_to_end(&mut response).unwrap();
    let header_end = response
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .unwrap();
    assert!(response[..header_end].starts_with(b"HTTP/1.1 200"));
    serde_json::from_slice(&response[header_end + 4..]).unwrap()
}

fn terminal_websocket_request(
    port: u16,
    token: &str,
    origin: &str,
) -> tungstenite::handshake::client::Request {
    let mut request = format!("ws://127.0.0.1:{port}/api/terminal")
        .into_client_request()
        .unwrap();
    request
        .headers_mut()
        .insert(header::ORIGIN, HeaderValue::from_str(origin).unwrap());
    request.headers_mut().insert(
        header::SEC_WEBSOCKET_PROTOCOL,
        HeaderValue::from_str(&format!("boomux.terminal.v1, boomux.token.{token}")).unwrap(),
    );
    request
}

fn read_terminal_until(reader: &mut dyn Read, needle: &[u8]) -> Vec<u8> {
    let deadline = Instant::now() + TIMEOUT;
    let mut output = Vec::new();
    let mut buffer = [0; 16 * 1024];
    while Instant::now() < deadline {
        match reader.read(&mut buffer) {
            Ok(0) => break,
            Ok(count) => {
                output.extend_from_slice(&buffer[..count]);
                if contains(&output, needle) {
                    return output;
                }
            }
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                ) =>
            {
                thread::sleep(Duration::from_millis(10));
            }
            Err(error) => panic!("terminal read failed: {error}"),
        }
    }
    panic!(
        "did not receive {:?}; terminal output was {:?}",
        String::from_utf8_lossy(needle),
        String::from_utf8_lossy(&output)
    );
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

    let codex_workspace = daemon
        .client
        .create_workspace(
            "codex-web-without-handoff",
            vec![ShellSpec {
                name: "codex".into(),
                command: vec!["/bin/sleep".into(), "30".into()],
                cwd: std::env::temp_dir(),
            }],
        )
        .unwrap();
    let codex_shell_id = codex_workspace.shells[0].id.clone();
    let _codex_attachment = daemon
        .client
        .attach(&codex_shell_id, false, profile())
        .unwrap();
    let codex_run_id = daemon
        .client
        .get_shell(&codex_shell_id)
        .unwrap()
        .run
        .unwrap()
        .id;
    let mut codex_hook = daemon.command();
    codex_hook
        .args(["codex", "hook"])
        .env("BOOMUX_SHELL_ID", &codex_shell_id)
        .env("BOOMUX_RUN_ID", &codex_run_id)
        .env("BOOMUX_CODEX_RUN_SCOPED", "1")
        .stdin(Stdio::piped());
    let mut codex_hook = codex_hook.spawn().unwrap();
    codex_hook
        .stdin
        .take()
        .unwrap()
        .write_all(
            b"{\"session_id\":\"codex-without-handoff\",\"hook_event_name\":\"SessionStart\"}",
        )
        .unwrap();
    assert!(codex_hook.wait().unwrap().success());

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
    let codex = snapshot["agents"]
        .as_array()
        .unwrap()
        .iter()
        .find(|agent| agent["integration"] == "codex")
        .unwrap();
    assert!(codex.get("native_web").is_none());

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

#[test]
fn current_local_agent_from_any_harness_can_collaborate_from_web_terminal() {
    let mut daemon = TestDaemon::start();
    let empty_path = daemon.runtime_dir.join("empty-path");
    fs::create_dir(&empty_path).unwrap();
    let workspace = daemon
        .client
        .create_workspace(
            "kiro-web-terminal",
            vec![ShellSpec {
                name: "kiro".into(),
                command: vec!["/bin/sh".into()],
                cwd: std::env::temp_dir(),
            }],
        )
        .unwrap();
    let shell_id = workspace.shells[0].id.clone();
    let attachment = daemon.client.attach(&shell_id, false, profile()).unwrap();
    let run_id = daemon.client.get_shell(&shell_id).unwrap().run.unwrap().id;
    daemon
        .client
        .register_agent(
            &shell_id,
            &run_id,
            AgentRegistrationSpec {
                name: "future-agent".into(),
                integration: "future-harness".into(),
                external_session_id: Some("future-session".into()),
                report: AgentReport {
                    state: AgentState::Working,
                    authority: AgentAuthority::LifecycleIntegration,
                    evidence: "future harness started".into(),
                    confidence: 100,
                },
            },
        )
        .unwrap();

    drop(attachment);
    let pty = native_pty_system()
        .openpty(PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 800,
            pixel_height: 480,
        })
        .unwrap();
    let descriptor = pty.master.as_raw_fd().unwrap();
    let flags = unsafe { libc::fcntl(descriptor, libc::F_GETFL) };
    assert_ne!(flags, -1);
    assert_ne!(
        unsafe { libc::fcntl(descriptor, libc::F_SETFL, flags | libc::O_NONBLOCK) },
        -1
    );
    let mut command = CommandBuilder::new(&daemon.executable);
    command.args([
        "__attach",
        &shell_id,
        "--takeover",
        "--expected-run-id",
        &run_id,
    ]);
    command.env("XDG_RUNTIME_DIR", &daemon.runtime_dir);
    command.env("XDG_STATE_HOME", daemon.runtime_dir.join("state"));
    command.env("TERM", "xterm-256color");
    command.env("COLORTERM", "truecolor");
    command.env("TERM_PROGRAM", "kiro-web-test");
    command.env("SHELL", "/bin/sh");
    let mut native_attachment = pty.slave.spawn_command(command).unwrap();
    drop(pty.slave);
    let mut native_reader = pty.master.try_clone_reader().unwrap();
    let mut native_writer = pty.master.take_writer().unwrap();
    native_writer
        .write_all(b"printf 'native-before-web\\n'\n")
        .unwrap();
    read_terminal_until(native_reader.as_mut(), b"native-before-web");

    let dashboard_port = unused_port();
    let opencode_port = unused_port();
    let dashboard_port_text = dashboard_port.to_string();
    let opencode_port_text = opencode_port.to_string();
    let cleanup = WebCleanup::new(&daemon, dashboard_port_text.clone());
    let started = run_web(
        &daemon,
        &[
            "web",
            "start",
            "--port",
            &dashboard_port_text,
            "--opencode-web-port",
            &opencode_port_text,
            "--json",
        ],
        &empty_path,
    );
    assert!(
        started.status.success(),
        "web start failed: {}",
        String::from_utf8_lossy(&started.stderr)
    );

    let snapshot_request = format!(
        "GET /api/snapshot HTTP/1.1\r\nHost: 127.0.0.1:{dashboard_port}\r\nConnection: close\r\n\r\n"
    );
    let snapshot = http_json(dashboard_port, snapshot_request.as_bytes());
    let agent = snapshot["agents"]
        .as_array()
        .unwrap()
        .iter()
        .find(|agent| agent["integration"] == "future-harness")
        .unwrap();
    let body = serde_json::json!({
        "node_id": agent["node_id"],
        "agent_id": agent["agent_id"],
        "shell_id": shell_id,
        "run_id": run_id,
        "rows": 40,
        "cols": 100,
        "pixel_width": 1000,
        "pixel_height": 800
    })
    .to_string();
    let authorization_request = format!(
        "POST /api/terminal/authorize HTTP/1.1\r\nHost: 127.0.0.1:{dashboard_port}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let authorization = http_json(dashboard_port, authorization_request.as_bytes());
    let token = authorization["token"].as_str().unwrap();
    let origin = format!("http://127.0.0.1:{dashboard_port}");
    let request = terminal_websocket_request(dashboard_port, token, &origin);
    let (mut socket, response) = connect(request).unwrap();
    assert_eq!(
        response
            .headers()
            .get(header::SEC_WEBSOCKET_PROTOCOL)
            .unwrap(),
        "boomux.terminal.v1"
    );
    if let tungstenite::stream::MaybeTlsStream::Plain(stream) = socket.get_mut() {
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
    }
    assert!(matches!(
        connect(terminal_websocket_request(dashboard_port, token, &origin)),
        Err(tungstenite::Error::Http(response)) if response.status() == tungstenite::http::StatusCode::FORBIDDEN
    ));

    let second_authorization = http_json(dashboard_port, authorization_request.as_bytes());
    let second_token = second_authorization["token"].as_str().unwrap();
    assert!(matches!(
        connect(terminal_websocket_request(
            dashboard_port,
            second_token,
            "https://foreign.example"
        )),
        Err(tungstenite::Error::Http(response)) if response.status() == tungstenite::http::StatusCode::FORBIDDEN
    ));

    let Message::Text(message) = socket.read().unwrap() else {
        panic!("terminal did not report its attachment profile");
    };
    let attached: serde_json::Value = serde_json::from_str(message.as_ref()).unwrap();
    assert_eq!(attached["type"], "attached");
    assert_eq!(attached["rows"], 24);
    assert_eq!(attached["cols"], 80);
    let _reconstruction = socket.read().unwrap();
    socket
        .send(Message::Binary(
            b"printf 'kiro-web-ok\\n'\n".to_vec().into(),
        ))
        .unwrap();
    let mut output = Vec::new();
    while !output
        .windows(b"kiro-web-ok".len())
        .any(|window| window == b"kiro-web-ok")
    {
        if let Message::Binary(bytes) = socket.read().unwrap() {
            output.extend_from_slice(&bytes);
        }
    }
    read_terminal_until(native_reader.as_mut(), b"kiro-web-ok");

    native_writer
        .write_all(b"printf 'native-while-web-open\\n'\n")
        .unwrap();
    read_terminal_until(native_reader.as_mut(), b"native-while-web-open");
    let mut output = Vec::new();
    while !output
        .windows(b"native-while-web-open".len())
        .any(|window| window == b"native-while-web-open")
    {
        if let Message::Binary(bytes) = socket.read().unwrap() {
            output.extend_from_slice(&bytes);
        }
    }

    socket
        .send(Message::Text(
            r#"{"type":"resize","rows":50,"cols":120,"pixel_width":1200,"pixel_height":1000}"#
                .into(),
        ))
        .unwrap();
    socket
        .send(Message::Binary(b"stty size\n".to_vec().into()))
        .unwrap();
    let mut output = Vec::new();
    while !output
        .windows(b"24 80".len())
        .any(|window| window == b"24 80")
    {
        if let Message::Binary(bytes) = socket.read().unwrap() {
            output.extend_from_slice(&bytes);
        }
    }

    pty.master
        .resize(PtySize {
            rows: 30,
            cols: 100,
            pixel_width: 1000,
            pixel_height: 600,
        })
        .unwrap();
    loop {
        match socket.read().unwrap() {
            Message::Text(message) => {
                let message: serde_json::Value = serde_json::from_str(message.as_ref()).unwrap();
                if message["type"] == "resize" {
                    assert_eq!(message["rows"], 30);
                    assert_eq!(message["cols"], 100);
                    break;
                }
            }
            Message::Binary(_) => {}
            message => panic!("unexpected terminal resize message: {message:?}"),
        }
    }

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
    wait_until(
        || daemon.client.ping().is_ok(),
        "replacement daemon did not accept requests",
    );
    let mut reconnecting = false;
    let mut reattached = false;
    while !reattached {
        match socket.read().unwrap() {
            Message::Text(message) if message.contains("\"type\":\"reconnecting\"") => {
                reconnecting = true;
            }
            Message::Text(message) if message.contains("\"type\":\"attached\"") => {
                reattached = true;
            }
            Message::Binary(_) => {}
            message => panic!("unexpected terminal reconnect message: {message:?}"),
        }
    }
    assert!(reconnecting);
    socket
        .send(Message::Binary(
            b"printf 'kiro-web-after-restart\\n'\n".to_vec().into(),
        ))
        .unwrap();
    let mut output = Vec::new();
    while !output
        .windows(b"kiro-web-after-restart".len())
        .any(|window| window == b"kiro-web-after-restart")
    {
        if let Message::Binary(bytes) = socket.read().unwrap() {
            output.extend_from_slice(&bytes);
        }
    }
    read_terminal_until(native_reader.as_mut(), b"kiro-web-after-restart");
    socket.close(None).unwrap();
    native_writer
        .write_all(b"printf 'native-after-web\\n'\n")
        .unwrap();
    read_terminal_until(native_reader.as_mut(), b"native-after-web");
    assert_eq!(
        daemon.client.get_shell(&shell_id).unwrap().run.unwrap().id,
        run_id
    );

    daemon.client.close_shell(&shell_id).unwrap();
    let _ = native_attachment.wait();
    drop(cleanup);
    daemon.stop_with_cli();
}

use std::io::{Read, Write};
use std::net::{Ipv4Addr, SocketAddrV4, TcpListener, TcpStream};
use std::process::{Child, Stdio};
use std::time::Duration;

use boomux::protocol::{
    AgentAuthority, AgentRegistrationSpec, AgentReport, AgentState, AttachFrame, ShellSpec,
    ShellStatus,
};
use serde_json::Value;
use tungstenite::client::IntoClientRequest;
use tungstenite::stream::MaybeTlsStream;
use tungstenite::{Message, connect};

use crate::support::{TestDaemon, contains, profile, read_until, wait_until};

struct WebProcess(Child);

impl Drop for WebProcess {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn local_web_terminal_requires_origin_and_controls_only_the_exact_run() {
    let mut daemon = TestDaemon::start();
    let workspace = daemon
        .client
        .create_workspace(
            "web-terminal",
            vec![ShellSpec {
                name: "shell".into(),
                cwd: std::env::temp_dir(),
                command: vec!["/bin/sh".into()],
            }],
        )
        .unwrap();
    let shell_id = workspace.shells[0].id.clone();
    let mut initial = daemon
        .client
        .attach(&shell_id, false, profile())
        .unwrap()
        .stream;
    let run_id = daemon.client.get_shell(&shell_id).unwrap().run.unwrap().id;
    AttachFrame::Input(b"printf 'reconstruction-row\\n'\n".to_vec())
        .write_to(&mut initial)
        .unwrap();
    assert!(contains(
        &read_until(&mut initial, b"reconstruction-row"),
        b"reconstruction-row"
    ));
    let (reconstruction, _, rows, cols) = daemon
        .client
        .read_terminal_reconstruction(&shell_id, &run_id)
        .unwrap();
    let mut reconstructed = vt100::Parser::new(rows, cols, 0);
    reconstructed.process(&reconstruction);
    assert!(
        reconstructed
            .screen()
            .contents()
            .contains("reconstruction-row")
    );
    let agent = daemon
        .client
        .register_agent(
            &shell_id,
            &run_id,
            AgentRegistrationSpec {
                name: "web-agent".into(),
                integration: "opencode".into(),
                external_session_id: Some("web-session".into()),
                report: AgentReport {
                    state: AgentState::Working,
                    authority: AgentAuthority::LifecycleIntegration,
                    evidence: "native web terminal test".into(),
                    confidence: 100,
                },
            },
        )
        .unwrap();
    AttachFrame::Detached.write_to(&mut initial).unwrap();
    drop(initial);
    let mut native = daemon
        .client
        .attach_native(&shell_id, &run_id, false, profile())
        .unwrap()
        .stream;

    let port = available_port();
    let child = daemon
        .command()
        .args(["web", "--port", &port.to_string(), "--no-opencode-web"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let _web = WebProcess(child);
    wait_until(
        || TcpStream::connect((Ipv4Addr::LOCALHOST, port)).is_ok(),
        "mobile web server did not accept requests",
    );

    let node_id = daemon.client.node_identity().unwrap();
    let grant_path = format!("/api/agents/{node_id}/{}/terminal-grant", agent.id);
    let detail = get(port, &format!("/api/agents/{node_id}/{}", agent.id));
    let detail: Value = serde_json::from_str(detail.split("\r\n\r\n").nth(1).unwrap()).unwrap();
    assert!(detail.get("terminal_output").is_none());
    assert!(detail.get("terminal_reconstruction").is_none());
    assert!(detail.get("terminal_rows").is_none());
    assert!(detail.get("terminal_cols").is_none());
    let rejected = post(port, &grant_path, None);
    assert!(rejected.starts_with("HTTP/1.1 403"));
    let rebound = post_with_authority(
        port,
        &grant_path,
        "attacker.example",
        Some("http://attacker.example"),
    );
    assert!(rebound.starts_with("HTTP/1.1 403"));

    let fitted_grant_path = format!("{grant_path}?rows=30&cols=100");
    let response = post(
        port,
        &fitted_grant_path,
        Some(&format!("http://127.0.0.1:{port}")),
    );
    assert!(response.starts_with("HTTP/1.1 200"), "{response}");
    let body = response.split("\r\n\r\n").nth(1).unwrap();
    let grant: Value = serde_json::from_str(body).unwrap();
    assert_eq!(grant["rows"], 30);
    assert_eq!(grant["cols"], 100);
    let websocket_path = grant["websocket_url"].as_str().unwrap();
    let websocket_url = format!("ws://127.0.0.1:{port}{websocket_path}");
    let mut request = websocket_url.clone().into_client_request().unwrap();
    request.headers_mut().insert(
        "Origin",
        format!("http://127.0.0.1:{port}").parse().unwrap(),
    );
    let (mut websocket, _) = connect(request).unwrap();
    if let MaybeTlsStream::Plain(stream) = websocket.get_mut() {
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
    }
    read_until_suspended(&mut native);
    drop(native);
    let (_, _, rows, cols) = daemon
        .client
        .read_terminal_reconstruction(&shell_id, &run_id)
        .unwrap();
    assert_eq!((rows, cols), (30, 100));
    assert!(matches!(websocket.read().unwrap(), Message::Binary(_)));
    websocket
        .send(Message::Text(
            r#"{"type":"resize","rows":40,"cols":120}"#.into(),
        ))
        .unwrap();
    websocket
        .send(Message::Binary(
            b"printf 'web-terminal-controlled\\n'\n".to_vec().into(),
        ))
        .unwrap();
    let mut output = Vec::new();
    while !contains(&output, b"web-terminal-controlled") {
        match websocket.read().unwrap() {
            Message::Binary(bytes) => output.extend_from_slice(&bytes),
            Message::Text(status) => {
                let status: Value = serde_json::from_str(&status).unwrap();
                assert_eq!(status["type"], "warning", "{status}");
            }
            other => panic!("unexpected WebSocket message: {other:?}"),
        }
    }
    let (_, _, rows, cols) = daemon
        .client
        .read_terminal_reconstruction(&shell_id, &run_id)
        .unwrap();
    assert_eq!((rows, cols), (40, 120));
    websocket.close(None).unwrap();

    let mut released = None;
    wait_until(
        || match daemon
            .client
            .attach_native(&shell_id, &run_id, false, profile())
        {
            Ok(attachment) => {
                released = Some(attachment);
                true
            }
            Err(_) => false,
        },
        "closing the WebSocket did not release its controller",
    );
    let mut released = released.unwrap().stream;
    let (_, _, rows, cols) = daemon
        .client
        .read_terminal_reconstruction(&shell_id, &run_id)
        .unwrap();
    assert_eq!((rows, cols), (24, 80));

    let takeover = post(port, &grant_path, Some(&format!("http://127.0.0.1:{port}")));
    let takeover: Value = serde_json::from_str(takeover.split("\r\n\r\n").nth(1).unwrap()).unwrap();
    let takeover_path = takeover["websocket_url"].as_str().unwrap();
    let mut takeover_request = format!("ws://127.0.0.1:{port}{takeover_path}")
        .into_client_request()
        .unwrap();
    takeover_request.headers_mut().insert(
        "Origin",
        format!("http://127.0.0.1:{port}").parse().unwrap(),
    );
    let (mut second_websocket, _) = connect(takeover_request).unwrap();
    read_until_suspended(&mut released);
    drop(released);
    assert!(matches!(
        second_websocket.read().unwrap(),
        Message::Binary(_)
    ));

    let mut reclaimed = daemon
        .client
        .attach_native(&shell_id, &run_id, true, profile())
        .unwrap()
        .stream;
    AttachFrame::Input(b"printf 'native-controller-reclaimed\n'\n".to_vec())
        .write_to(&mut reclaimed)
        .unwrap();
    assert!(contains(
        &read_until(&mut reclaimed, b"native-controller-reclaimed"),
        b"native-controller-reclaimed"
    ));
    AttachFrame::Detached.write_to(&mut reclaimed).unwrap();
    drop(reclaimed);

    let shell = daemon.client.get_shell(&shell_id).unwrap();
    assert_eq!(shell.status, ShellStatus::Running);
    let run = shell.run.unwrap();
    assert_eq!(run.id, run_id);

    let mut reused = websocket_url.into_client_request().unwrap();
    reused.headers_mut().insert(
        "Origin",
        format!("http://127.0.0.1:{port}").parse().unwrap(),
    );
    let error = connect(reused).unwrap_err();
    assert!(matches!(error, tungstenite::Error::Http(response) if response.status() == 404));

    daemon.client.close_workspace(&workspace.id).unwrap();
    daemon.stop_with_cli();
}

fn available_port() -> u16 {
    TcpListener::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0))
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn read_until_suspended(stream: &mut impl Read) {
    loop {
        match AttachFrame::read_from(stream).unwrap() {
            AttachFrame::Output(_) => {}
            AttachFrame::Suspended => return,
            frame => panic!("unexpected native attachment frame before suspension: {frame:?}"),
        }
    }
}

fn post(port: u16, path: &str, origin: Option<&str>) -> String {
    post_with_authority(port, path, &format!("127.0.0.1:{port}"), origin)
}

fn get(port: u16, path: &str) -> String {
    let mut stream = TcpStream::connect((Ipv4Addr::LOCALHOST, port)).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    write!(
        stream,
        "GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n"
    )
    .unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).unwrap();
    response
}

fn post_with_authority(port: u16, path: &str, host: &str, origin: Option<&str>) -> String {
    let mut stream = TcpStream::connect((Ipv4Addr::LOCALHOST, port)).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let origin = origin
        .map(|origin| format!("Origin: {origin}\r\n"))
        .unwrap_or_default();
    write!(
        stream,
        "POST {path} HTTP/1.1\r\nHost: {host}\r\n{origin}Content-Length: 0\r\nConnection: close\r\n\r\n"
    )
    .unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).unwrap();
    response
}

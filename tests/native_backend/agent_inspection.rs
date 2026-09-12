use crate::support::{TestDaemon, profile};
use boomux::protocol::{
    self, AgentAuthority, AgentRegistrationSpec, AgentReport, AgentState, Envelope, ErrorCode,
    HostServiceOperation, Request, Response, ShellSpec,
};
use std::{fs, os::unix::net::UnixStream, time::Duration};

#[test]
fn agent_inspection_is_read_only_exact_run_and_rejects_old_wire_requests() {
    let mut daemon = TestDaemon::start();
    let project = daemon.runtime_dir.join("repo");
    fs::create_dir_all(project.join(".git")).unwrap();
    fs::create_dir_all(project.join(".codex")).unwrap();
    fs::write(
        project.join(".codex/config.toml"),
        "[mcp_servers.fixture]\ncommand='unused'\nargs=['SECRET-ARGV']\n",
    )
    .unwrap();
    let workspace = daemon
        .client
        .create_workspace(
            "inspection",
            vec![ShellSpec {
                name: "shell".into(),
                cwd: project,
                command: vec!["/bin/sleep".into(), "60".into()],
            }],
        )
        .unwrap();
    let shell = &workspace.shells[0];
    let attachment = daemon.client.attach(&shell.id, false, profile()).unwrap();
    let run = daemon.client.get_shell(&shell.id).unwrap().run.unwrap();
    let agent = daemon
        .client
        .register_agent(
            &shell.id,
            &run.id,
            AgentRegistrationSpec {
                name: "fixture".into(),
                integration: "codex".into(),
                external_session_id: None,
                report: AgentReport {
                    state: AgentState::Idle,
                    authority: AgentAuthority::LifecycleIntegration,
                    evidence: "fixture".into(),
                    confidence: 100,
                },
            },
        )
        .unwrap();
    let state = fs::read(daemon.runtime_dir.join("state/boomux/state.json")).unwrap();
    let inspection = daemon
        .client
        .inspect_agent(None, &agent.id, &run.id, Duration::from_secs(5))
        .unwrap();
    assert_eq!(inspection.agent.run_id, run.id);
    assert!(inspection.mcp_servers.iter().any(|s| s.name == "fixture"));
    let response = Response::HostService {
        result: protocol::HostServiceResult::AgentInspection { inspection },
    };
    let json = serde_json::to_string(&response).unwrap();
    assert!(!json.contains("SECRET-ARGV"));
    assert_eq!(serde_json::from_str::<Response>(&json).unwrap(), response);
    assert_eq!(
        state,
        fs::read(daemon.runtime_dir.join("state/boomux/state.json")).unwrap()
    );
    for request in [
        Request::HostService {
            operation: HostServiceOperation::InspectAgent {
                agent_id: agent.id.clone(),
                expected_run_id: run.id.clone(),
            },
        },
        Request::RouteNodeHostService {
            node_id: "missing-owner".into(),
            operation: HostServiceOperation::InspectAgent {
                agent_id: agent.id.clone(),
                expected_run_id: run.id.clone(),
            },
        },
    ] {
        let mut stream = UnixStream::connect(daemon.client.socket_path()).unwrap();
        protocol::write_message(&mut stream, &Envelope::with_version(54, request)).unwrap();
        let response: Envelope<Response> = protocol::read_message(&mut stream).unwrap();
        assert!(matches!(
            response.message,
            Response::Error {
                code: Some(ErrorCode::UnsupportedVersion),
                ..
            }
        ));
    }
    assert!(
        daemon
            .client
            .inspect_agent(None, &agent.id, "wrong-run", Duration::from_secs(5))
            .is_err()
    );
    drop(attachment);
    daemon.stop_with_cli();
}

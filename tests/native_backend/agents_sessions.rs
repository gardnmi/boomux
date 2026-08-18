use std::fs;
use std::os::unix::net::UnixStream;
use std::thread;
use std::time::Duration;

use boomux::client::Client;
use boomux::protocol::{
    self, AgentAuthority, AgentRegistrationSpec, AgentReport, AgentState, ErrorCode, ShellSpec,
};

use crate::support::{TestDaemon, assert_generated_name, assert_remote_code, contains, profile};

#[test]
fn agent_runtime_is_revisioned_durable_and_version_compatible() {
    let mut daemon = TestDaemon::start();
    let workspace = daemon
        .client
        .create_workspace(
            "agent-runtime",
            vec![ShellSpec::login("runtime", std::env::temp_dir())],
        )
        .unwrap();
    let shell_id = workspace.shells[0].id.clone();
    let attachment = daemon.client.attach(&shell_id, false, profile()).unwrap();
    let run_id = daemon
        .client
        .get_shell(&shell_id)
        .unwrap()
        .run
        .expect("started agent shell has no run identity")
        .id;
    let baseline = daemon.client.events(None, 256, 0).unwrap().cursor;
    let registration = AgentRegistrationSpec {
        name: "runtime-agent".into(),
        integration: "native-test".into(),
        external_session_id: Some("session-1".into()),
        report: AgentReport {
            state: AgentState::Working,
            authority: AgentAuthority::LifecycleIntegration,
            evidence: "registered".into(),
            confidence: 90,
        },
    };

    let ensure_agent = |daemon: &TestDaemon| {
        let output = daemon
            .command()
            .args([
                "agent",
                "ensure",
                "--integration",
                "native-test",
                "--external-session-id",
                "session-1",
                "--shell-id",
                &shell_id,
                "--run-id",
                &run_id,
                "--state",
                "working",
                "--authority",
                "lifecycle-integration",
                "--evidence",
                "registered",
                "--confidence",
                "90",
                "--json",
            ])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice::<serde_json::Value>(&output.stdout).unwrap()
    };
    let ensure = ensure_agent(&daemon);
    assert_eq!(ensure["schema"], "boomux.cli/v1");
    assert_eq!(ensure["command"], "agent.ensure");
    let agent_id = ensure["data"]["agent"]["id"].as_str().unwrap().to_owned();
    assert_eq!(ensure["data"]["agent"]["shell_id"], shell_id);
    assert_eq!(ensure["data"]["agent"]["run_id"], run_id);
    assert_eq!(ensure["data"]["agent"]["external_session_id"], "session-1");
    assert_eq!(ensure["data"]["agent"]["workspace_name"], "agent-runtime");
    assert_eq!(ensure["data"]["agent"]["observation"]["revision"], 1);
    let generated_name = ensure["data"]["agent"]["name"].as_str().unwrap();
    assert_generated_name(generated_name);

    let repeated = ensure_agent(&daemon);
    assert_eq!(repeated["data"]["agent"]["id"], agent_id);
    assert_eq!(repeated["data"]["agent"]["name"], generated_name);
    assert_eq!(repeated["data"]["agent"]["observation"]["revision"], 1);
    let ensured_events = daemon
        .client
        .events(Some(baseline.clone()), 256, 0)
        .unwrap();
    assert_eq!(
        ensured_events
            .events
            .iter()
            .filter(|event| matches!(
                &event.kind,
                protocol::DaemonEventKind::AgentRegistered { agent, .. }
                    if agent.id == agent_id
            ))
            .count(),
        1
    );
    let workspace_agents = daemon.client.get_workspace(&workspace.id).unwrap().agents;
    assert_eq!(workspace_agents.len(), 1);
    assert_eq!(workspace_agents[0].id, agent_id);

    drop(attachment.stream);
    let restart = daemon
        .command()
        .args(["daemon", "restart"])
        .output()
        .unwrap();
    assert!(
        restart.status.success(),
        "{}",
        String::from_utf8_lossy(&restart.stderr)
    );
    let recovered = ensure_agent(&daemon);
    assert_eq!(recovered["data"]["agent"]["id"], agent_id);
    assert_eq!(recovered["data"]["agent"]["observation"]["revision"], 1);
    let recovered = daemon.client.get_agent(&agent_id).unwrap();

    let weak_report_cursor = daemon.client.events(None, 256, 0).unwrap().cursor;
    for report in [
        AgentReport {
            state: AgentState::Blocked,
            authority: AgentAuthority::ProcessAdapter,
            evidence: "process thinks blocked".into(),
            confidence: 80,
        },
        AgentReport {
            state: AgentState::Done,
            authority: AgentAuthority::TerminalHeuristic,
            evidence: "prompt disappeared".into(),
            confidence: 40,
        },
    ] {
        assert_eq!(
            daemon
                .client
                .report_agent(&agent_id, &run_id, report)
                .unwrap(),
            recovered
        );
    }
    assert!(
        daemon
            .client
            .events(Some(weak_report_cursor), 256, 0)
            .unwrap()
            .events
            .is_empty()
    );

    let register = daemon
        .command()
        .args([
            "agent",
            "register",
            "adapter-agent",
            "--integration",
            "native-test",
            "--shell-id",
            &shell_id,
            "--run-id",
            &run_id,
            "--state",
            "idle",
            "--authority",
            "terminal-heuristic",
            "--evidence",
            "prompt visible",
            "--confidence",
            "30",
            "--json",
        ])
        .output()
        .unwrap();
    assert!(
        register.status.success(),
        "{}",
        String::from_utf8_lossy(&register.stderr)
    );
    let register: serde_json::Value = serde_json::from_slice(&register.stdout).unwrap();
    assert_eq!(register["schema"], "boomux.cli/v1");
    assert_eq!(register["command"], "agent.register");
    assert_eq!(register["data"]["agent"]["workspace_name"], "agent-runtime");
    assert_eq!(register["data"]["agent"]["observation"]["revision"], 1);
    let adapter_id = register["data"]["agent"]["id"].as_str().unwrap().to_owned();

    let mut wait_command = daemon.command();
    wait_command.args([
        "agent",
        "wait",
        &adapter_id,
        "--after-revision",
        "1",
        "--wait-ms",
        "5000",
        "--json",
    ]);
    let wait = thread::spawn(move || wait_command.output().unwrap());
    thread::sleep(Duration::from_millis(50));

    let report_cursor = daemon.client.events(None, 256, 0).unwrap().cursor;
    let report = daemon
        .command()
        .args([
            "agent",
            "report",
            &adapter_id,
            "--shell-id",
            &shell_id,
            "--run-id",
            &run_id,
            "--state",
            "blocked",
            "--authority",
            "process-adapter",
            "--evidence",
            "waiting for input",
            "--confidence",
            "80",
            "--json",
        ])
        .output()
        .unwrap();
    assert!(
        report.status.success(),
        "{}",
        String::from_utf8_lossy(&report.stderr)
    );
    let report: serde_json::Value = serde_json::from_slice(&report.stdout).unwrap();
    assert_eq!(report["schema"], "boomux.cli/v1");
    assert_eq!(report["command"], "agent.report");
    assert_eq!(report["data"]["agent"]["workspace_name"], "agent-runtime");
    assert_eq!(report["data"]["agent"]["observation"]["revision"], 2);
    assert_eq!(
        report["data"]["agent"]["observation"]["authority"],
        "process_adapter"
    );
    let waited = wait.join().unwrap();
    assert!(
        waited.status.success(),
        "{}",
        String::from_utf8_lossy(&waited.stderr)
    );
    let waited: serde_json::Value = serde_json::from_slice(&waited.stdout).unwrap();
    assert_eq!(waited["command"], "agent.wait");
    assert_eq!(waited["data"]["changed"], true);
    assert_eq!(waited["data"]["agent"]["id"], adapter_id);
    assert_eq!(waited["data"]["agent"]["workspace_name"], "agent-runtime");
    assert_eq!(waited["data"]["agent"]["observation"]["revision"], 2);
    let duplicate = daemon
        .client
        .report_agent(
            &adapter_id,
            &run_id,
            AgentReport {
                state: AgentState::Blocked,
                authority: AgentAuthority::ProcessAdapter,
                evidence: "waiting for input".into(),
                confidence: 80,
            },
        )
        .unwrap();
    assert_eq!(duplicate.observation.revision, 2);
    let unchanged = daemon
        .command()
        .args([
            "agent",
            "wait",
            &adapter_id,
            "--after-revision",
            "2",
            "--wait-ms",
            "10",
            "--json",
        ])
        .output()
        .unwrap();
    assert!(unchanged.status.success());
    let unchanged: serde_json::Value = serde_json::from_slice(&unchanged.stdout).unwrap();
    assert_eq!(unchanged["data"]["changed"], false);
    let report_events = daemon.client.events(Some(report_cursor), 256, 0).unwrap();
    assert_eq!(
        report_events
            .events
            .iter()
            .filter(|event| matches!(
                &event.kind,
                protocol::DaemonEventKind::AgentStateChanged { agent, .. }
                    if agent.id == adapter_id
            ))
            .count(),
        1
    );

    let completion = AgentReport {
        state: AgentState::Done,
        authority: AgentAuthority::LifecycleIntegration,
        evidence: "completed".into(),
        confidence: 100,
    };
    let done_cursor = report_events.cursor;
    let done = daemon
        .client
        .report_agent(&adapter_id, &run_id, completion.clone())
        .unwrap();
    assert_eq!(done.observation.revision, 3);
    assert_eq!(done.observation.state, AgentState::Done);
    assert_eq!(done.ended_at_ms, Some(done.observation.observed_at_ms));
    assert_eq!(
        done.attention.as_ref().map(|attention| attention.reason),
        Some(protocol::AgentAttentionReason::Completed)
    );
    assert_eq!(
        daemon
            .client
            .report_agent(&adapter_id, &run_id, completion)
            .unwrap(),
        done
    );
    let done_events = daemon.client.events(Some(done_cursor), 256, 0).unwrap();
    assert_eq!(
        done_events
            .events
            .iter()
            .filter(|event| matches!(
                &event.kind,
                protocol::DaemonEventKind::AgentCompleted { agent, .. }
                    if agent.id == adapter_id
            ))
            .count(),
        1
    );
    let attention = daemon
        .command()
        .args(["attention", "list", "--workspace", &workspace.id, "--json"])
        .output()
        .unwrap();
    assert!(
        attention.status.success(),
        "{}",
        String::from_utf8_lossy(&attention.stderr)
    );
    let attention: serde_json::Value = serde_json::from_slice(&attention.stdout).unwrap();
    assert_eq!(attention["command"], "attention.list");
    assert_eq!(attention["data"]["attention"][0]["agent"]["id"], adapter_id);
    assert_eq!(attention["data"]["attention"][0]["reason"], "completed");
    assert_eq!(
        attention["data"]["attention"][0]["observation"]["revision"],
        3
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
            .get_agent(&adapter_id)
            .unwrap()
            .attention
            .as_ref()
            .map(|attention| attention.observation.revision),
        Some(3)
    );
    let acknowledgment_cursor = daemon.client.events(None, 256, 0).unwrap().cursor;
    let acknowledgment = daemon
        .command()
        .args([
            "attention",
            "acknowledge",
            &adapter_id,
            "--observation-revision",
            "3",
            "--json",
        ])
        .output()
        .unwrap();
    assert!(acknowledgment.status.success());
    let acknowledgment: serde_json::Value = serde_json::from_slice(&acknowledgment.stdout).unwrap();
    assert_eq!(acknowledgment["command"], "attention.acknowledge");
    assert_eq!(acknowledgment["data"]["changed"], true);
    assert_eq!(
        acknowledgment["data"]["agent"]["workspace_name"],
        "agent-runtime"
    );
    assert!(acknowledgment["data"]["agent"]["attention"].is_null());
    assert!(
        daemon
            .client
            .events(Some(acknowledgment_cursor), 256, 0)
            .unwrap()
            .events
            .iter()
            .any(|event| matches!(
                &event.kind,
                protocol::DaemonEventKind::AgentAttentionAcknowledged { agent, .. }
                    if agent.id == adapter_id && agent.attention.is_none()
            ))
    );
    let repeated = daemon
        .client
        .acknowledge_agent_attention(&adapter_id, 3)
        .unwrap();
    assert!(!repeated.changed);
    let error = daemon
        .client
        .report_agent(
            &adapter_id,
            &run_id,
            AgentReport {
                state: AgentState::Done,
                authority: AgentAuthority::LifecycleIntegration,
                evidence: "conflicting completion".into(),
                confidence: 100,
            },
        )
        .unwrap_err();
    assert_remote_code(&error, ErrorCode::InvalidArgument);

    for request in [
        protocol::Request::RegisterAgent {
            shell_id: shell_id.clone(),
            run_id: run_id.clone(),
            spec: AgentRegistrationSpec {
                report: AgentReport {
                    authority: AgentAuthority::DaemonLifecycle,
                    ..registration.report.clone()
                },
                ..registration.clone()
            },
        },
        protocol::Request::EnsureAgent {
            shell_id: shell_id.clone(),
            run_id: run_id.clone(),
            spec: AgentRegistrationSpec {
                report: AgentReport {
                    authority: AgentAuthority::DaemonLifecycle,
                    ..registration.report.clone()
                },
                ..registration.clone()
            },
        },
        protocol::Request::ReportAgent {
            agent_id: agent_id.clone(),
            run_id: run_id.clone(),
            report: AgentReport {
                authority: AgentAuthority::DaemonLifecycle,
                ..registration.report.clone()
            },
        },
    ] {
        assert!(matches!(
            versioned_request(&daemon.client, 10, request),
            protocol::Response::Error {
                code: Some(ErrorCode::InvalidArgument),
                ..
            }
        ));
    }
    let reserved_cli = daemon
        .command()
        .args([
            "agent",
            "report",
            &agent_id,
            "--state",
            "done",
            "--authority",
            "daemon-lifecycle",
            "--evidence",
            "reserved",
            "--confidence",
            "100",
            "--json",
        ])
        .output()
        .unwrap();
    assert!(!reserved_cli.status.success());
    let reserved_cli: serde_json::Value = serde_json::from_slice(&reserved_cli.stderr).unwrap();
    assert_eq!(reserved_cli["error"]["code"], "invalid_argument");

    let list = daemon.command().args(["agent", "list"]).output().unwrap();
    assert!(list.status.success());
    assert!(contains(&list.stdout, generated_name.as_bytes()));
    assert!(contains(&list.stdout, agent_id.as_bytes()));
    let list = daemon
        .command()
        .args(["agent", "list", "--json"])
        .output()
        .unwrap();
    assert!(list.status.success());
    let list: serde_json::Value = serde_json::from_slice(&list.stdout).unwrap();
    assert_eq!(list["command"], "agent.list");
    assert_eq!(list["data"]["agents"][0]["id"], agent_id);
    assert_eq!(list["data"]["agents"][0]["observation"]["revision"], 1);
    let host_bin = daemon.runtime_dir.join("host-bin");
    fs::create_dir(&host_bin).unwrap();
    let session_list = daemon
        .command()
        .args(["session", "list", "--workspace", &workspace.id, "--json"])
        .env("PATH", &host_bin)
        .output()
        .unwrap();
    assert!(
        session_list.status.success(),
        "{}",
        String::from_utf8_lossy(&session_list.stderr)
    );
    let session_list: serde_json::Value = serde_json::from_slice(&session_list.stdout).unwrap();
    assert_eq!(session_list["command"], "session.list");
    let sessions = session_list["data"]["sessions"].as_array().unwrap();
    assert_eq!(sessions.len(), 2);
    assert!(
        sessions
            .iter()
            .all(|session| session["workspace_id"] == workspace.id)
    );
    let projected = sessions
        .iter()
        .find(|session| session["external_session_id"] == "session-1")
        .unwrap();
    let session_id = projected["id"].as_str().unwrap();
    assert_eq!(projected["description"], generated_name);
    assert_eq!(projected["occurrence_count"], 1);

    let session_inspect = daemon
        .command()
        .args(["session", "inspect", session_id, "--json"])
        .env("PATH", &host_bin)
        .output()
        .unwrap();
    assert!(session_inspect.status.success());
    let session_inspect: serde_json::Value =
        serde_json::from_slice(&session_inspect.stdout).unwrap();
    assert_eq!(session_inspect["command"], "session.inspect");
    assert_eq!(session_inspect["data"]["session"]["id"], session_id);
    assert_eq!(
        session_inspect["data"]["session"]["occurrences"][0]["agent_id"],
        agent_id
    );
    assert_eq!(
        session_inspect["data"]["session"]["occurrences"][0]["shell_id"],
        shell_id
    );

    let missing_session = daemon
        .command()
        .args(["session", "inspect", "session-1", "--json"])
        .output()
        .unwrap();
    assert!(!missing_session.status.success());
    let missing_session: serde_json::Value =
        serde_json::from_slice(&missing_session.stderr).unwrap();
    assert_eq!(missing_session["command"], "session.inspect");
    assert_eq!(missing_session["error"]["code"], "not_found");
    let inspect = daemon
        .command()
        .args(["agent", "inspect", &adapter_id])
        .output()
        .unwrap();
    assert!(inspect.status.success());
    assert!(contains(&inspect.stdout, b"STATE\tdone"));
    assert!(contains(&inspect.stdout, b"REVISION\t3"));
    let inspect = daemon
        .command()
        .args(["agent", "inspect", &adapter_id, "--json"])
        .output()
        .unwrap();
    assert!(inspect.status.success());
    let inspect: serde_json::Value = serde_json::from_slice(&inspect.stdout).unwrap();
    assert_eq!(inspect["command"], "agent.inspect");
    assert_eq!(inspect["data"]["agent"]["id"], adapter_id);
    assert_eq!(inspect["data"]["agent"]["observation"]["state"], "done");

    assert!(matches!(
        versioned_request(
            &daemon.client,
            9,
            protocol::Request::EnsureAgent {
                shell_id: shell_id.clone(),
                run_id: run_id.clone(),
                spec: registration.clone(),
            },
        ),
        protocol::Response::Error {
            code: Some(ErrorCode::UnsupportedVersion),
            ..
        }
    ));
    assert!(matches!(
        versioned_request(
            &daemon.client,
            13,
            protocol::Request::WaitAgent {
                agent_id: agent_id.clone(),
                after_revision: 1,
                wait_ms: 0,
            },
        ),
        protocol::Response::Error {
            code: Some(ErrorCode::UnsupportedVersion),
            ..
        }
    ));
    assert!(matches!(
        versioned_request(
            &daemon.client,
            9,
            protocol::Request::GetAgent {
                agent_id: agent_id.clone(),
            },
        ),
        protocol::Response::Agent { agent } if agent.id == agent_id
    ));
    assert!(matches!(
        versioned_request(
            &daemon.client,
            9,
            protocol::Request::ReportAgent {
                agent_id: agent_id.clone(),
                run_id: run_id.clone(),
                report: registration.report.clone(),
            },
        ),
        protocol::Response::Agent { agent }
            if agent.id == agent_id && agent.observation.revision == 1
    ));

    let legacy_baseline = daemon.client.events(None, 256, 0).unwrap().cursor;
    let protocol_eight_snapshot = versioned_request(&daemon.client, 8, protocol::Request::Snapshot);
    let protocol::Response::Snapshot { snapshot } = protocol_eight_snapshot else {
        panic!("expected protocol-8 snapshot response");
    };
    assert!(
        snapshot
            .workspaces
            .iter()
            .all(|workspace| workspace.agents.is_empty())
    );
    let protocol_eight_events = versioned_request(
        &daemon.client,
        8,
        protocol::Request::Events {
            after: Some(legacy_baseline),
            limit: 256,
            wait_ms: 0,
        },
    );
    let protocol::Response::Events {
        cursor: filtered_cursor,
        events,
        ..
    } = protocol_eight_events
    else {
        panic!("expected protocol-8 events response");
    };
    assert!(events.is_empty());
    daemon
        .client
        .rename_shell(&shell_id, "runtime-renamed")
        .unwrap();
    let protocol_eight_events = versioned_request(
        &daemon.client,
        8,
        protocol::Request::Events {
            after: Some(filtered_cursor.clone()),
            limit: 256,
            wait_ms: 0,
        },
    );
    let protocol::Response::Events { cursor, events, .. } = protocol_eight_events else {
        panic!("expected protocol-8 events response after rename");
    };
    assert!(events.iter().any(|event| matches!(
        &event.kind,
        protocol::DaemonEventKind::ShellRenamed { shell_id: id, .. } if id == &shell_id
    )));
    assert!(cursor.event_id > filtered_cursor.event_id);

    daemon.stop_with_cli();
}

#[test]
fn unnamed_agent_registration_and_supervision_generate_names() {
    let daemon = TestDaemon::start();
    let workspace = daemon
        .client
        .create_workspace(
            "generated-agent-names",
            vec![ShellSpec::login("runtime", std::env::temp_dir())],
        )
        .unwrap();
    let shell_id = workspace.shells[0].id.clone();
    let attachment = daemon.client.attach(&shell_id, false, profile()).unwrap();
    let run_id = daemon.client.get_shell(&shell_id).unwrap().run.unwrap().id;

    let registered = daemon
        .command()
        .args([
            "agent",
            "register",
            "--integration",
            "native-test",
            "--external-session-id",
            "registered-session",
            "--shell-id",
            &shell_id,
            "--run-id",
            &run_id,
            "--state",
            "working",
            "--authority",
            "lifecycle-integration",
            "--evidence",
            "registered",
            "--confidence",
            "100",
            "--json",
        ])
        .output()
        .unwrap();
    assert!(registered.status.success());
    let registered: serde_json::Value = serde_json::from_slice(&registered.stdout).unwrap();
    assert_generated_name(registered["data"]["agent"]["name"].as_str().unwrap());

    let supervised = daemon
        .command()
        .args([
            "agent",
            "supervise",
            "--integration",
            "native-test",
            "--external-session-id",
            "supervised-session",
            "--shell-id",
            &shell_id,
            "--run-id",
            &run_id,
            "--",
            "/bin/true",
        ])
        .output()
        .unwrap();
    assert!(
        supervised.status.success(),
        "{}",
        String::from_utf8_lossy(&supervised.stderr)
    );
    let agents = daemon.client.get_workspace(&workspace.id).unwrap().agents;
    let supervised = agents
        .iter()
        .find(|agent| agent.external_session_id.as_deref() == Some("supervised-session"))
        .expect("supervisor did not register its agent");
    assert_generated_name(&supervised.name);

    drop(attachment.stream);
}

#[test]
fn session_context_survives_shell_removal_and_cold_restart() {
    let mut daemon = TestDaemon::start();
    let project = daemon.runtime_dir.join("durable-source-project");
    fs::create_dir(&project).unwrap();
    let workspace = daemon
        .client
        .create_workspace(
            "durable-source",
            vec![ShellSpec::login("agent", project.clone())],
        )
        .unwrap();
    let shell_id = workspace.shells[0].id.clone();
    let attachment = daemon.client.attach(&shell_id, false, profile()).unwrap();
    let run_id = daemon.client.get_shell(&shell_id).unwrap().run.unwrap().id;
    let agent = daemon
        .client
        .ensure_agent(
            &shell_id,
            &run_id,
            AgentRegistrationSpec {
                name: "pi".into(),
                integration: "pi".into(),
                external_session_id: Some("durable-pi-session".into()),
                report: AgentReport {
                    state: AgentState::Idle,
                    authority: AgentAuthority::LifecycleIntegration,
                    evidence: "fixture idle".into(),
                    confidence: 100,
                },
            },
        )
        .unwrap();
    assert_eq!(agent.cwd.as_deref(), Some(project.as_path()));

    daemon.client.close_shell(&shell_id).unwrap();
    drop(attachment);
    assert!(daemon.client.get_shell(&shell_id).is_err());
    daemon.stop_with_cli();
    daemon.restart();

    let sessions = daemon
        .command()
        .args(["session", "list", "--workspace", &workspace.id, "--json"])
        .output()
        .unwrap();
    assert!(sessions.status.success());
    let sessions: serde_json::Value = serde_json::from_slice(&sessions.stdout).unwrap();
    let session_id = sessions["data"]["sessions"][0]["id"].as_str().unwrap();
    let inspect = daemon
        .command()
        .args(["session", "inspect", session_id, "--json"])
        .output()
        .unwrap();
    assert!(inspect.status.success());
    let inspect: serde_json::Value = serde_json::from_slice(&inspect.stdout).unwrap();
    let occurrence = &inspect["data"]["session"]["occurrences"][0];
    assert!(occurrence["retained_shell_name"].is_null());
    assert!(occurrence["retained_shell_cwd"].is_null());
    assert_eq!(occurrence["source_cwd"], project.display().to_string());
}

#[test]
fn explicit_process_supervisor_preserves_child_io_exit_and_agent_authority() {
    let daemon = TestDaemon::start();
    let workspace = daemon
        .client
        .create_workspace(
            "process-supervisor",
            vec![ShellSpec {
                name: "supervised".into(),
                command: vec!["/bin/sh".into(), "-c".into(), "sleep 30".into()],
                cwd: std::env::temp_dir(),
            }],
        )
        .unwrap();
    let shell_id = workspace.shells[0].id.clone();
    let _attachment = daemon.client.attach(&shell_id, false, profile()).unwrap();
    let run_id = daemon
        .client
        .get_shell(&shell_id)
        .unwrap()
        .run
        .expect("started supervisor shell has no run identity")
        .id;
    let external_session_id = "native-process-supervisor-session";
    let baseline = daemon.client.events(None, 256, 0).unwrap().cursor;

    let output = daemon
        .command()
        .args([
            "agent",
            "supervise",
            "native-supervisor",
            "--integration",
            "native-test",
            "--external-session-id",
            external_session_id,
            "--shell-id",
            &shell_id,
            "--run-id",
            &run_id,
            "--",
            "/bin/sh",
            "-c",
            "printf 'supervisor stdout\\n'; printf 'supervisor stderr\\n' >&2; exit 23",
        ])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(23));
    assert_eq!(output.stdout, b"supervisor stdout\n");
    assert_eq!(output.stderr, b"supervisor stderr\n");

    let agents = daemon.client.get_workspace(&workspace.id).unwrap().agents;
    assert_eq!(agents.len(), 1);
    let supervised = &agents[0];
    assert_eq!(supervised.shell_id, shell_id);
    assert_eq!(supervised.run_id, run_id);
    assert_eq!(
        supervised.external_session_id.as_deref(),
        Some(external_session_id)
    );
    assert_eq!(supervised.observation.revision, 2);
    assert_eq!(supervised.observation.state, AgentState::Unknown);
    assert_eq!(
        supervised.observation.authority,
        AgentAuthority::ProcessAdapter
    );
    assert!(
        supervised
            .observation
            .evidence
            .contains("exited with code 23")
    );
    assert_eq!(supervised.ended_at_ms, None);
    let agent_id = supervised.id.clone();
    let supervisor_events = daemon.client.events(Some(baseline), 256, 0).unwrap();
    assert!(supervisor_events.events.iter().any(|event| matches!(
        &event.kind,
        protocol::DaemonEventKind::AgentRegistered { agent, .. } if agent.id == agent_id
    )));
    assert!(supervisor_events.events.iter().any(|event| matches!(
        &event.kind,
        protocol::DaemonEventKind::AgentStateChanged { agent, .. } if agent.id == agent_id
    )));
    assert!(!supervisor_events.events.iter().any(|event| matches!(
        &event.kind,
        protocol::DaemonEventKind::AgentCompleted { agent, .. } if agent.id == agent_id
    )));

    let lifecycle_report = AgentReport {
        state: AgentState::Working,
        authority: AgentAuthority::LifecycleIntegration,
        evidence: "lifecycle integration owns session".into(),
        confidence: 100,
    };
    let ensured = daemon
        .client
        .ensure_agent(
            &shell_id,
            &run_id,
            AgentRegistrationSpec {
                name: "native-supervisor".into(),
                integration: "native-test".into(),
                external_session_id: Some(external_session_id.into()),
                report: lifecycle_report.clone(),
            },
        )
        .unwrap();
    assert_eq!(ensured.id, agent_id);
    assert_eq!(ensured.observation.revision, 2);
    let lifecycle = daemon
        .client
        .report_agent(&agent_id, &run_id, lifecycle_report)
        .unwrap();
    assert_eq!(lifecycle.id, agent_id);
    assert_eq!(lifecycle.observation.revision, 3);
    assert_eq!(lifecycle.observation.state, AgentState::Working);
    assert_eq!(
        lifecycle.observation.authority,
        AgentAuthority::LifecycleIntegration
    );

    let lower_authority_cursor = daemon.client.events(None, 256, 0).unwrap().cursor;
    let repeated = daemon
        .command()
        .args([
            "agent",
            "supervise",
            "native-supervisor",
            "--integration",
            "native-test",
            "--external-session-id",
            external_session_id,
            "--shell-id",
            &shell_id,
            "--run-id",
            &run_id,
            "--",
            "/bin/sh",
            "-c",
            "exit 0",
        ])
        .output()
        .unwrap();
    assert!(repeated.status.success());
    assert!(repeated.stdout.is_empty());
    assert!(repeated.stderr.is_empty());
    assert_eq!(daemon.client.get_agent(&agent_id).unwrap(), lifecycle);
    let repeated_events = daemon
        .client
        .events(Some(lower_authority_cursor), 256, 0)
        .unwrap();
    assert!(!repeated_events.events.iter().any(|event| matches!(
        &event.kind,
        protocol::DaemonEventKind::AgentRegistered { agent, .. }
            | protocol::DaemonEventKind::AgentStateChanged { agent, .. }
            | protocol::DaemonEventKind::AgentCompleted { agent, .. }
            if agent.id == agent_id
    )));
}

#[test]
fn cold_recovery_presents_resumable_agent_only_to_protocol_forty() {
    let mut daemon = TestDaemon::start();
    let workspace = daemon
        .client
        .create_workspace(
            "recover-agent-presentation",
            vec![ShellSpec::login("agent", std::env::temp_dir())],
        )
        .unwrap();
    let workspace_id = workspace.id.clone();
    let shell_id = workspace.shells[0].id.clone();
    let attachment = daemon.client.attach(&shell_id, false, profile()).unwrap();
    let first_run = daemon.client.get_shell(&shell_id).unwrap().run.unwrap();
    let agent = daemon
        .client
        .register_agent(
            &shell_id,
            &first_run.id,
            AgentRegistrationSpec {
                name: "recovered-agent".into(),
                integration: "opencode".into(),
                external_session_id: Some("session-1".into()),
                report: AgentReport {
                    state: AgentState::Blocked,
                    authority: AgentAuthority::LifecycleIntegration,
                    evidence: "waiting before crash".into(),
                    confidence: 100,
                },
            },
        )
        .unwrap();

    daemon.crash();
    drop(attachment.stream);
    daemon.restart();

    let recovered = daemon.client.get_shell(&shell_id).unwrap();
    assert_eq!(recovered.status, protocol::ShellStatus::Pending);
    let recovered_run = recovered.run.expect("protocol 40 omitted recovery run");
    assert_eq!(
        recovered.recovered_agent_id.as_deref(),
        Some(agent.id.as_str())
    );
    assert_eq!(recovered_run.id, first_run.id);
    assert_eq!(
        recovered_run.exit_reason,
        Some(protocol::ShellRunExitReason::Interrupted)
    );
    assert!(recovered_run.ended_at_ms.is_some());
    assert_eq!(daemon.client.get_agent(&agent.id).unwrap(), agent);

    let protocol::Response::Snapshot { snapshot } =
        versioned_request(&daemon.client, 39, protocol::Request::Snapshot)
    else {
        panic!("expected protocol-39 snapshot");
    };
    let old_shell = snapshot
        .workspaces
        .iter()
        .find(|workspace| workspace.id == workspace_id)
        .and_then(|workspace| workspace.shells.iter().find(|shell| shell.id == shell_id))
        .expect("protocol-39 snapshot omitted recovered shell");
    assert!(old_shell.run.is_none());
    assert!(old_shell.recovered_agent_id.is_none());

    daemon.stop_with_cli();
}

fn versioned_request(
    client: &Client,
    version: u32,
    request: protocol::Request,
) -> protocol::Response {
    let mut stream = UnixStream::connect(client.socket_path()).unwrap();
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

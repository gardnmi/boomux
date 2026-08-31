use std::fs;
use std::io::Write;
use std::net::TcpListener;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::process::Stdio;
use std::thread;
use std::time::{Duration, Instant};

use boomux::client::Client;
use boomux::protocol::{
    self, AgentAuthority, AgentRegistrationSpec, AgentReport, AgentState, ErrorCode, Request,
    Response, ShellSpec, ShellStatus, UnixEnvironment, UnixEnvironmentVariable,
};
use uuid::Uuid;

use crate::support::{
    TestDaemon, assert_generated_name, assert_remote_code, contains, ensure_test_opencode_runtime,
    process_exists, profile, wait_until,
};

#[test]
fn claude_hook_reports_lifecycle_and_synchronizes_ephemeral_bridge_binding() {
    let mut daemon = TestDaemon::start();
    let workspace = daemon
        .client
        .create_workspace(
            "claude-hook",
            vec![ShellSpec {
                name: "claude".into(),
                command: vec!["/bin/sleep".into(), "300".into()],
                cwd: std::env::temp_dir(),
            }],
        )
        .unwrap();
    let shell_id = workspace.shells[0].id.clone();
    let attachment = daemon.client.attach(&shell_id, false, profile()).unwrap();
    let run_id = daemon.client.get_shell(&shell_id).unwrap().run.unwrap().id;

    let run_hook = |event: &str, bridge: Option<&str>| {
        let mut command = daemon.command();
        command
            .args(["claude", "hook"])
            .env("BOOMUX_SHELL_ID", &shell_id)
            .env("BOOMUX_RUN_ID", &run_id)
            .stdin(Stdio::piped());
        if let Some(bridge) = bridge {
            command.env("CLAUDE_CODE_BRIDGE_SESSION_ID", bridge);
        } else {
            command.env_remove("CLAUDE_CODE_BRIDGE_SESSION_ID");
        }
        let mut child = command.spawn().unwrap();
        write!(
            child.stdin.take().unwrap(),
            "{{\"session_id\":\"claude-session\",\"hook_event_name\":\"{event}\"}}"
        )
        .unwrap();
        assert!(child.wait().unwrap().success());
    };

    let event_cursor = daemon.client.events(None, 256, 0).unwrap().cursor;
    run_hook("SessionStart", Some("bridge-exact"));
    let snapshot = daemon.client.snapshot().unwrap();
    let agent = snapshot.workspaces[0].agents[0].clone();
    assert_eq!(agent.integration, "claude");
    assert_eq!(agent.external_session_id.as_deref(), Some("claude-session"));
    assert_eq!(agent.observation.state, AgentState::Idle);
    assert_eq!(
        daemon
            .client
            .get_claude_remote_control_binding(&agent.id, &shell_id, &run_id)
            .unwrap()
            .unwrap()
            .bridge_session_id,
        "bridge-exact"
    );
    let persisted = fs::read(daemon.runtime_dir.join("state/boomux/state.json")).unwrap();
    let events = daemon.client.events(Some(event_cursor), 256, 0).unwrap();
    let events = serde_json::to_vec(&events.events).unwrap();
    for public_or_durable in [serde_json::to_vec(&snapshot).unwrap(), events, persisted] {
        assert!(
            !public_or_durable
                .windows(b"bridge-exact".len())
                .any(|window| window == b"bridge-exact")
        );
    }

    drop(attachment);
    daemon.client.restart().unwrap();
    assert_eq!(
        daemon
            .client
            .get_claude_remote_control_binding(&agent.id, &shell_id, &run_id)
            .unwrap()
            .unwrap()
            .bridge_session_id,
        "bridge-exact"
    );

    run_hook("SessionEnd", Some("ignored-on-session-end"));
    assert_eq!(
        daemon
            .client
            .get_agent(&agent.id)
            .unwrap()
            .observation
            .state,
        AgentState::Inactive
    );

    run_hook("UserPromptSubmit", None);
    assert_eq!(
        daemon
            .client
            .get_agent(&agent.id)
            .unwrap()
            .observation
            .state,
        AgentState::Working
    );
    assert!(
        daemon
            .client
            .get_claude_remote_control_binding(&agent.id, &shell_id, &run_id)
            .unwrap()
            .is_none()
    );

    daemon.stop_with_cli();
}

#[test]
fn codex_hook_requires_run_scoped_launch_and_reuses_exact_thread_agent() {
    let mut daemon = TestDaemon::start();
    let workspace = daemon
        .client
        .create_workspace(
            "codex-hook",
            vec![ShellSpec {
                name: "codex".into(),
                command: vec!["/bin/sleep".into(), "30".into()],
                cwd: std::env::temp_dir(),
            }],
        )
        .unwrap();
    let shell_id = workspace.shells[0].id.clone();
    let _attachment = daemon.client.attach(&shell_id, false, profile()).unwrap();
    let run_id = daemon.client.get_shell(&shell_id).unwrap().run.unwrap().id;

    let run_hook = |event: &str, run_scoped: bool| {
        let mut command = daemon.command();
        command
            .args(["codex", "hook"])
            .env("BOOMUX_SHELL_ID", &shell_id)
            .env("BOOMUX_RUN_ID", &run_id)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped());
        if run_scoped {
            command.env("BOOMUX_CODEX_RUN_SCOPED", "1");
        } else {
            command.env_remove("BOOMUX_CODEX_RUN_SCOPED");
        }
        let mut child = command.spawn().unwrap();
        write!(
            child.stdin.take().unwrap(),
            "{{\"session_id\":\"codex-thread\",\"hook_event_name\":\"{event}\"}}"
        )
        .unwrap();
        let output = child.wait_with_output().unwrap();
        assert!(output.status.success());
        assert!(output.stdout.is_empty());
    };

    run_hook("SessionStart", false);
    assert!(
        daemon.client.snapshot().unwrap().workspaces[0]
            .agents
            .is_empty()
    );

    for (event, expected) in [
        ("SessionStart", AgentState::Idle),
        ("UserPromptSubmit", AgentState::Working),
        ("PermissionRequest", AgentState::Blocked),
        ("PostToolUse", AgentState::Working),
        ("Stop", AgentState::Idle),
        ("SessionEnd", AgentState::Inactive),
    ] {
        run_hook(event, true);
        let agents = &daemon.client.snapshot().unwrap().workspaces[0].agents;
        assert_eq!(agents.len(), 1);
        assert_eq!(agents[0].integration, "codex");
        assert_eq!(
            agents[0].external_session_id.as_deref(),
            Some("codex-thread")
        );
        assert_eq!(agents[0].observation.state, expected, "{event}");
        assert_ne!(agents[0].observation.state, AgentState::Done);
    }

    daemon.stop_with_cli();
}

#[test]
fn kiro_agent_without_live_holder_becomes_inactive() {
    let mut daemon = TestDaemon::start();
    let workspace = daemon
        .client
        .create_workspace(
            "holderless-kiro",
            vec![ShellSpec {
                name: "kiro".into(),
                command: vec!["/bin/sleep".into(), "300".into()],
                cwd: std::env::temp_dir(),
            }],
        )
        .unwrap();
    let shell_id = workspace.shells[0].id.clone();
    let attachment = daemon.client.attach(&shell_id, false, profile()).unwrap();
    let run_id = daemon.client.get_shell(&shell_id).unwrap().run.unwrap().id;
    let agent = daemon
        .client
        .register_agent(
            &shell_id,
            &run_id,
            AgentRegistrationSpec {
                name: "Kiro CLI".into(),
                integration: "kiro".into(),
                external_session_id: Some("pre-holder-session".into()),
                report: AgentReport {
                    state: AgentState::Idle,
                    authority: AgentAuthority::LifecycleIntegration,
                    evidence: "Kiro session idle".into(),
                    confidence: 100,
                },
            },
        )
        .unwrap();

    wait_until(
        || {
            daemon.client.get_agent(&agent.id).is_ok_and(|agent| {
                agent.observation.state == AgentState::Inactive
                    && agent.observation.evidence == "Kiro launch authority unavailable"
            })
        },
        "holderless Kiro Agent remained active",
    );

    drop(attachment);
    daemon.stop_with_cli();
}

#[test]
fn sequential_kiro_process_holders_inactivate_only_the_exited_session() {
    let mut daemon = TestDaemon::start();
    let workspace = daemon
        .client
        .create_workspace(
            "kiro-hook",
            vec![ShellSpec {
                name: "kiro".into(),
                command: vec!["/bin/sleep".into(), "300".into()],
                cwd: std::env::temp_dir(),
            }],
        )
        .unwrap();
    let shell_id = workspace.shells[0].id.clone();
    let _attachment = daemon.client.attach(&shell_id, false, profile()).unwrap();
    let run_id = daemon.client.get_shell(&shell_id).unwrap().run.unwrap().id;

    let kiro_home = daemon.runtime_dir.join("kiro-holder-home");
    fs::create_dir_all(kiro_home.join("hooks")).unwrap();
    fs::write(
        kiro_home.join("hooks/boomux.json"),
        include_str!("../../integrations/kiro/boomux.json"),
    )
    .unwrap();
    let kiro = daemon.runtime_dir.join("kiro-holder-cli");
    fs::write(
        &kiro,
        "#!/bin/sh\nprintf '%s' \"$$\" > \"$KIRO_TEST_PID\"\nprintf '{\"session_id\":\"%s\",\"hook_event_name\":\"UserPromptSubmit\"}' \"$KIRO_TEST_SESSION\" | \"$KIRO_TEST_BOOMUX\" kiro hook\nwhile [ ! -e \"$KIRO_TEST_STOP\" ]; do sleep 0.01; done\nprintf '{\"session_id\":\"%s\",\"hook_event_name\":\"Stop\"}' \"$KIRO_TEST_SESSION\" | \"$KIRO_TEST_BOOMUX\" kiro hook\n/bin/sleep 300 &\nprintf '%s' \"$!\" > \"$KIRO_TEST_DESCENDANT_PID\"\nwait\n",
    )
    .unwrap();
    fs::set_permissions(&kiro, fs::Permissions::from_mode(0o755)).unwrap();

    let run_unclaimed_hook = || {
        let mut command = daemon.command();
        command
            .args(["kiro", "hook"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped());
        let mut child = command.spawn().unwrap();
        write!(
            child.stdin.take().unwrap(),
            "{{\"session_id\":\"unclaimed\",\"hook_event_name\":\"UserPromptSubmit\"}}"
        )
        .unwrap();
        let output = child.wait_with_output().unwrap();
        assert!(output.status.success());
        assert!(output.stdout.is_empty());
    };
    run_unclaimed_hook();
    assert!(
        daemon.client.snapshot().unwrap().workspaces[0]
            .agents
            .is_empty()
    );

    let run = |session: &str, case: &str, kill_holder: bool| {
        let pid_file = daemon.runtime_dir.join(format!("kiro-{case}-pid"));
        let descendant_pid_file = daemon
            .runtime_dir
            .join(format!("kiro-{case}-descendant-pid"));
        let stop_file = daemon.runtime_dir.join(format!("kiro-{case}-stop"));
        let mut command = daemon.command();
        command
            .args(["kiro", "launch", "--"])
            .env("KIRO_HOME", &kiro_home)
            .env("BOOMUX_REAL_KIRO", &kiro)
            .env("BOOMUX_SHELL_ID", &shell_id)
            .env("BOOMUX_RUN_ID", &run_id)
            .env("KIRO_TEST_SESSION", session)
            .env("KIRO_TEST_PID", &pid_file)
            .env("KIRO_TEST_DESCENDANT_PID", &descendant_pid_file)
            .env("KIRO_TEST_STOP", &stop_file)
            .env("KIRO_TEST_BOOMUX", env!("CARGO_BIN_EXE_boomux"));
        command.process_group(0);
        let mut child = command.spawn().unwrap();
        wait_until(
            || {
                daemon.client.snapshot().is_ok_and(|snapshot| {
                    snapshot.workspaces[0].agents.iter().any(|agent| {
                        agent.external_session_id.as_deref() == Some(session)
                            && agent.observation.state == AgentState::Working
                    })
                })
            },
            "Kiro holder hook did not report Working",
        );
        fs::write(&stop_file, "").unwrap();
        wait_until(
            || {
                daemon.client.snapshot().is_ok_and(|snapshot| {
                    snapshot.workspaces[0].agents.iter().any(|agent| {
                        agent.external_session_id.as_deref() == Some(session)
                            && agent.observation.state == AgentState::Idle
                    })
                })
            },
            "Kiro Stop hook did not report Idle",
        );
        let pid = fs::read_to_string(pid_file)
            .unwrap()
            .parse::<libc::pid_t>()
            .unwrap();
        wait_until(
            || descendant_pid_file.exists(),
            "Kiro launcher shim did not start its descendant",
        );
        let descendant_pid = fs::read_to_string(descendant_pid_file)
            .unwrap()
            .parse::<libc::pid_t>()
            .unwrap();
        let event_waiter = if kill_holder {
            let baseline = daemon.client.events(None, 256, 0).unwrap().cursor;
            let client = daemon.client.clone();
            Some(thread::spawn(move || {
                client.events(Some(baseline), 256, 2_000).unwrap()
            }))
        } else {
            None
        };
        let status = if kill_holder {
            assert_eq!(
                unsafe { libc::kill(child.id() as libc::pid_t, libc::SIGTERM) },
                0
            );
            child.wait().unwrap()
        } else {
            assert_eq!(
                unsafe { libc::kill(-(child.id() as libc::pid_t), libc::SIGINT) },
                0
            );
            child.wait().unwrap()
        };
        if kill_holder {
            assert_eq!(status.signal(), Some(libc::SIGTERM));
            wait_until(
                || !process_exists(pid),
                "directly terminated Kiro holder left its child orphaned",
            );
            wait_until(
                || !process_exists(descendant_pid),
                "directly terminated Kiro holder left a descendant orphaned",
            );
            let events = event_waiter.unwrap().join().unwrap().events;
            assert!(events.iter().any(|event| {
                matches!(
                    &event.kind,
                    protocol::DaemonEventKind::AgentStateChanged { agent, .. }
                        if agent.external_session_id.as_deref() == Some(session)
                            && agent.observation.state == AgentState::Inactive
                )
            }));
        } else {
            assert_eq!(status.signal(), Some(libc::SIGINT));
            wait_until(
                || !process_exists(pid),
                "foreground Ctrl+C left the managed Kiro child alive",
            );
            wait_until(
                || !process_exists(descendant_pid),
                "foreground Ctrl+C left a Kiro descendant alive",
            );
        }
        wait_until(
            || {
                daemon.client.snapshot().is_ok_and(|snapshot| {
                    snapshot.workspaces[0].agents.iter().any(|agent| {
                        agent.external_session_id.as_deref() == Some(session)
                            && agent.observation.state == AgentState::Inactive
                            && agent.attention.is_none()
                    })
                })
            },
            "Kiro final holder exit did not report Inactive",
        );
    };

    run("session-a", "a", false);
    run("session-b", "b", true);
    let agents = &daemon.client.snapshot().unwrap().workspaces[0].agents;
    assert_eq!(agents.len(), 2);
    assert!(agents.iter().all(|agent| {
        agent.observation.state == AgentState::Inactive
            && agent.observation.state != AgentState::Done
            && agent.attention.is_none()
    }));

    daemon.stop_with_cli();
}

#[test]
fn one_codex_app_server_catalogs_multiple_workspace_directories() {
    let mut daemon = TestDaemon::start_with(|command, runtime_dir| {
        let bin = runtime_dir.join("codex-catalog-bin");
        fs::create_dir(&bin).unwrap();
        fs::create_dir(runtime_dir.join("other")).unwrap();
        let codex = bin.join("codex");
        fs::write(
            &codex,
            "#!/bin/sh\nprintf x >> \"$CODEX_CATALOG_COUNT\"\n/bin/sleep 1\n[ \"$1\" = app-server ] || exit 64\nIFS= read -r line || exit 65\nprintf '%s\\n' '{\"id\":1,\"result\":{}}'\nIFS= read -r line || exit 66\nfor id in 2 3; do\n  IFS= read -r line || exit 67\n  printf '{\"id\":%s,\"result\":{\"data\":[{\"id\":\"codex-history-%s\",\"name\":\"Historical Codex thread %s\",\"preview\":\"fallback\",\"ephemeral\":false,\"createdAt\":10,\"updatedAt\":20}],\"nextCursor\":null}}\\n' \"$id\" \"$id\" \"$id\"\ndone\n",
        )
        .unwrap();
        fs::set_permissions(&codex, fs::Permissions::from_mode(0o755)).unwrap();
        command
            .env("PATH", &bin)
            .env("CODEX_CATALOG_COUNT", runtime_dir.join("catalog-count"));
    });
    let workspace = daemon
        .client
        .create_workspace(
            "codex-catalog",
            vec![
                ShellSpec::login("shell", &daemon.runtime_dir),
                ShellSpec::login("other", daemon.runtime_dir.join("other")),
            ],
        )
        .unwrap();

    let list_started = Instant::now();
    let output = daemon
        .command()
        .args(["session", "list", "--workspace", &workspace.id, "--json"])
        .output()
        .unwrap();
    let list_elapsed = list_started.elapsed();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let output: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let sessions = output["data"]["sessions"].as_array().unwrap();
    assert_eq!(sessions.len(), 2, "{sessions:#?}");
    assert!(sessions.iter().all(|session| {
        session["integration"] == "codex"
            && session["state"] == "unknown"
            && session["occurrence_count"] == 0
    }));
    let session_id = sessions[0]["id"].as_str().unwrap();
    let inspect_started = Instant::now();
    assert!(matches!(
        daemon
            .client
            .request(Request::HostService {
                operation: protocol::HostServiceOperation::InspectAgentSession {
                    session_id: session_id.into(),
                },
            })
            .unwrap(),
        Response::HostService {
            result: protocol::HostServiceResult::AgentSession { .. }
        }
    ));
    let inspect_elapsed = inspect_started.elapsed();
    assert_eq!(
        fs::read(daemon.runtime_dir.join("catalog-count")).unwrap(),
        b"x"
    );
    assert!(list_elapsed >= Duration::from_millis(900));
    assert!(
        inspect_elapsed < Duration::from_millis(250),
        "cached Session inspection took {inspect_elapsed:?}"
    );
    eprintln!(
        "cold Session catalog took {list_elapsed:?}; cached inspection took {inspect_elapsed:?}"
    );

    daemon.stop_with_cli();
}

#[test]
fn kiro_catalog_titles_exact_observed_session_without_projecting_history() {
    let mut daemon = TestDaemon::start_with(|command, runtime_dir| {
        let bin = runtime_dir.join("kiro-catalog-bin");
        fs::create_dir(&bin).unwrap();
        let kiro = bin.join("kiro-cli");
        fs::write(
            &kiro,
            "#!/bin/sh\nprintf '%s\\0' \"$@\" > \"$KIRO_CATALOG_ARGS\"\n[ \"$1\" = chat ] || exit 64\n[ \"$2\" = --list-sessions ] || exit 65\n[ \"$3\" = --format ] || exit 66\n[ \"$4\" = json ] || exit 67\nprintf '[{\"cwd\":\"%s\",\"sessions\":[{\"sessionId\":\"kiro-observed\",\"source\":\"v3\",\"title\":\"Triage Slack issue thread\",\"executionTarget\":\"local\"},{\"sessionId\":\"kiro-history\",\"source\":\"v2\",\"title\":\"Unobserved history\"}],\"complete\":true}]\\n' \"$KIRO_CATALOG_CWD\"\n",
        )
        .unwrap();
        fs::set_permissions(&kiro, fs::Permissions::from_mode(0o755)).unwrap();
        command
            .env("PATH", &bin)
            .env("KIRO_CATALOG_CWD", runtime_dir)
            .env("KIRO_CATALOG_ARGS", runtime_dir.join("kiro-catalog-args"));
    });
    let workspace = daemon
        .client
        .create_workspace(
            "kiro-catalog",
            vec![ShellSpec {
                name: "kiro".into(),
                command: vec!["/bin/sleep".into(), "300".into()],
                cwd: daemon.runtime_dir.clone(),
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
                name: "Kiro CLI".into(),
                integration: "kiro".into(),
                external_session_id: Some("kiro-observed".into()),
                report: AgentReport {
                    state: AgentState::Inactive,
                    authority: AgentAuthority::LifecycleIntegration,
                    evidence: "historical Session".into(),
                    confidence: 100,
                },
            },
        )
        .unwrap();

    let output = daemon
        .command()
        .args(["session", "list", "--workspace", &workspace.id, "--json"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let output: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let sessions = output["data"]["sessions"].as_array().unwrap();
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0]["integration"], "kiro");
    assert_eq!(sessions[0]["external_session_id"], "kiro-observed");
    assert_eq!(sessions[0]["description"], "Triage Slack issue thread");
    assert_eq!(sessions[0]["occurrence_count"], 1);
    assert_eq!(
        fs::read(daemon.runtime_dir.join("kiro-catalog-args")).unwrap(),
        b"chat\0--list-sessions\0--format\0json\0"
    );

    drop(attachment);
    daemon.stop_with_cli();
}

#[test]
fn pi_and_claude_titles_enrich_exact_observed_sessions_without_projecting_history() {
    let mut daemon = TestDaemon::start_with(|command, runtime_dir| {
        let pi_sessions = runtime_dir.join("pi-title-sessions");
        fs::create_dir(&pi_sessions).unwrap();
        for (id, title) in [
            ("pi-observed", "Named Pi session"),
            ("pi-history", "Unobserved Pi history"),
        ] {
            fs::write(
                pi_sessions.join(format!("{id}.jsonl")),
                format!(
                    "{{\"type\":\"session\",\"version\":3,\"id\":\"{id}\",\"cwd\":\"{}\"}}\n{{\"type\":\"session_info\",\"name\":\"{title}\"}}\n",
                    runtime_dir.display()
                ),
            )
            .unwrap();
        }

        let claude_config = runtime_dir.join("claude-title-config");
        let encoded_directory = runtime_dir
            .to_string_lossy()
            .chars()
            .map(|character| {
                if character.is_ascii_alphanumeric() {
                    character
                } else {
                    '-'
                }
            })
            .collect::<String>();
        let claude_sessions = claude_config.join("projects").join(encoded_directory);
        fs::create_dir_all(&claude_sessions).unwrap();
        for (id, title) in [
            ("claude-observed", "Claude exact title"),
            ("claude-history", "Unobserved Claude history"),
        ] {
            fs::write(
                claude_sessions.join(format!("{id}.jsonl")),
                format!(
                    "{{\"type\":\"user\",\"sessionId\":\"{id}\",\"cwd\":\"{}\"}}\n{{\"type\":\"ai-title\",\"sessionId\":\"{id}\",\"aiTitle\":\"{title}\"}}\n",
                    runtime_dir.display()
                ),
            )
            .unwrap();
        }
        command
            .env("PI_CODING_AGENT_SESSION_DIR", pi_sessions)
            .env("CLAUDE_CONFIG_DIR", claude_config);
    });
    let workspace = daemon
        .client
        .create_workspace(
            "title-only-hosts",
            vec![ShellSpec {
                name: "agents".into(),
                command: vec!["/bin/sleep".into(), "300".into()],
                cwd: daemon.runtime_dir.clone(),
            }],
        )
        .unwrap();
    let shell_id = workspace.shells[0].id.clone();
    let attachment = daemon.client.attach(&shell_id, false, profile()).unwrap();
    let run_id = daemon.client.get_shell(&shell_id).unwrap().run.unwrap().id;
    for (integration, external_session_id, name) in [
        ("pi", "pi-observed", "Pi"),
        ("claude", "claude-observed", "Claude Code"),
    ] {
        daemon
            .client
            .register_agent(
                &shell_id,
                &run_id,
                AgentRegistrationSpec {
                    name: name.into(),
                    integration: integration.into(),
                    external_session_id: Some(external_session_id.into()),
                    report: AgentReport {
                        state: AgentState::Inactive,
                        authority: AgentAuthority::LifecycleIntegration,
                        evidence: "historical Session".into(),
                        confidence: 100,
                    },
                },
            )
            .unwrap();
    }

    let output = daemon
        .command()
        .args(["session", "list", "--workspace", &workspace.id, "--json"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let output: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let sessions = output["data"]["sessions"].as_array().unwrap();
    assert_eq!(sessions.len(), 2);
    for (integration, external_session_id, title) in [
        ("pi", "pi-observed", "Named Pi session"),
        ("claude", "claude-observed", "Claude exact title"),
    ] {
        let session = sessions
            .iter()
            .find(|session| session["integration"] == integration)
            .unwrap();
        assert_eq!(session["external_session_id"], external_session_id);
        assert_eq!(session["description"], title);
        assert_eq!(session["occurrence_count"], 1);
        assert_eq!(session["state"], "inactive");
    }

    drop(attachment);
    daemon.stop_with_cli();
}

#[test]
fn exact_durable_session_inspection_bypasses_host_catalogs() {
    let mut daemon = TestDaemon::start_with(|command, runtime_dir| {
        let bin = runtime_dir.join("slow-catalog-bin");
        fs::create_dir(&bin).unwrap();
        for executable in ["opencode", "codex", "kiro-cli"] {
            let path = bin.join(executable);
            fs::write(
                &path,
                format!(
                    "#!/bin/sh\nprintf x >> \"{}\"\n/bin/sleep 5\nexit 1\n",
                    runtime_dir.join("catalog-invoked").display()
                ),
            )
            .unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        }
        command.env("PATH", bin);
    });
    let workspace = daemon
        .client
        .create_workspace(
            "fast-session-inspect",
            vec![ShellSpec {
                name: "agent".into(),
                command: vec!["/bin/sleep".into(), "300".into()],
                cwd: daemon.runtime_dir.clone(),
            }],
        )
        .unwrap();
    let shell_id = workspace.shells[0].id.clone();
    let attachment = daemon.client.attach(&shell_id, false, profile()).unwrap();
    let run_id = daemon.client.get_shell(&shell_id).unwrap().run.unwrap().id;
    let agent = daemon
        .client
        .register_agent(
            &shell_id,
            &run_id,
            AgentRegistrationSpec {
                name: "Kiro CLI".into(),
                integration: "kiro".into(),
                external_session_id: Some("fast-session".into()),
                report: AgentReport {
                    state: AgentState::Inactive,
                    authority: AgentAuthority::LifecycleIntegration,
                    evidence: "historical Session".into(),
                    confidence: 100,
                },
            },
        )
        .unwrap();
    let session_id = match daemon
        .client
        .request(Request::HostService {
            operation: protocol::HostServiceOperation::ResolveAgentSession {
                workspace_id: workspace.id,
                agent_id: agent.id,
            },
        })
        .unwrap()
    {
        Response::HostService {
            result: protocol::HostServiceResult::ResolvedAgentSession { session },
        } => session.id,
        response => panic!("unexpected Session resolution: {response:?}"),
    };

    let started = Instant::now();
    assert!(matches!(
        daemon
            .client
            .request(Request::HostService {
                operation: protocol::HostServiceOperation::InspectAgentSession { session_id },
            })
            .unwrap(),
        Response::HostService {
            result: protocol::HostServiceResult::AgentSession { .. }
        }
    ));
    let elapsed = started.elapsed();
    assert!(
        elapsed < Duration::from_secs(1),
        "exact durable Session inspection took {elapsed:?}"
    );
    assert!(!daemon.runtime_dir.join("catalog-invoked").exists());

    drop(attachment);
    daemon.stop_with_cli();
}

#[test]
fn cold_recovery_resumes_exact_codex_thread_with_run_scoped_hooks() {
    let mut daemon = TestDaemon::start_with(|command, runtime_dir| {
        let bin = runtime_dir.join("codex-bin");
        let codex_home = runtime_dir.join("codex-home");
        fs::create_dir(&bin).unwrap();
        fs::create_dir(&codex_home).unwrap();
        fs::write(
            codex_home.join("hooks.json"),
            include_str!("../../integrations/codex/hooks.json"),
        )
        .unwrap();
        let codex = bin.join("codex");
        fs::write(
            &codex,
            "#!/bin/sh\n: > \"$CODEX_RECOVERY_CAPTURE\"\nfor arg do printf '%s\\0' \"$arg\" >> \"$CODEX_RECOVERY_CAPTURE\"; done\n[ \"${3-unset}\" = resume ] || /bin/sleep 30\n",
        )
        .unwrap();
        fs::set_permissions(&codex, fs::Permissions::from_mode(0o700)).unwrap();
        command
            .env("PATH", &bin)
            .env("CODEX_HOME", &codex_home)
            .env(
                "CODEX_RECOVERY_CAPTURE",
                runtime_dir.join("codex-recovery-argv"),
            );
    });
    let codex = daemon.runtime_dir.join("codex-bin/codex");
    let workspace = daemon
        .client
        .create_workspace(
            "codex-recovery",
            vec![ShellSpec {
                name: "codex".into(),
                command: vec![codex.display().to_string()],
                cwd: daemon.runtime_dir.clone(),
            }],
        )
        .unwrap();
    let shell_id = workspace.shells[0].id.clone();
    let attachment = daemon.client.attach(&shell_id, false, profile()).unwrap();
    wait_until(
        || fs::read(daemon.runtime_dir.join("codex-recovery-argv")).is_ok(),
        "initial Codex run did not launch",
    );
    let first_run = daemon.client.get_shell(&shell_id).unwrap().run.unwrap();
    let agent = daemon
        .client
        .register_agent(
            &shell_id,
            &first_run.id,
            AgentRegistrationSpec {
                name: "Codex".into(),
                integration: "codex".into(),
                external_session_id: Some("exact-recovery-thread".into()),
                report: AgentReport {
                    state: AgentState::Idle,
                    authority: AgentAuthority::LifecycleIntegration,
                    evidence: "Codex session idle".into(),
                    confidence: 100,
                },
            },
        )
        .unwrap();

    daemon.crash();
    drop(attachment.stream);
    let bin = daemon.runtime_dir.join("codex-bin");
    let codex_home = daemon.runtime_dir.join("codex-home");
    let capture = daemon.runtime_dir.join("codex-recovery-argv");
    daemon.restart_with(move |command| {
        command
            .env("PATH", bin)
            .env("CODEX_HOME", codex_home)
            .env("CODEX_RECOVERY_CAPTURE", capture);
    });
    let recovered = daemon.client.get_shell(&shell_id).unwrap();
    assert_eq!(recovered.status, ShellStatus::Pending);
    assert_eq!(
        recovered.recovered_agent_id.as_deref(),
        Some(agent.id.as_str())
    );
    let retained = daemon.client.snapshot().unwrap().workspaces[0]
        .agents
        .iter()
        .find(|candidate| candidate.id == agent.id)
        .unwrap()
        .clone();
    assert_eq!(retained.run_id, first_run.id);

    let recovered_attachment = daemon.client.attach(&shell_id, false, profile()).unwrap();
    wait_until(
        || {
            fs::read(daemon.runtime_dir.join("codex-recovery-argv"))
                .is_ok_and(|argv| argv == b"--enable\0hooks\0resume\0exact-recovery-thread\0")
        },
        "recovered Codex run did not resume the exact thread with hooks",
    );
    let second_run = daemon.client.get_shell(&shell_id).unwrap().run.unwrap();
    assert_ne!(second_run.id, first_run.id);

    drop(recovered_attachment);
    daemon.stop_with_cli();
}

#[test]
fn cold_recovery_resumes_exact_kiro_v3_session_with_run_scoped_hooks() {
    let mut daemon = TestDaemon::start_with(|command, runtime_dir| {
        let bin = runtime_dir.join("kiro-bin");
        let kiro_home = runtime_dir.join("kiro-home");
        fs::create_dir(&bin).unwrap();
        fs::create_dir_all(kiro_home.join("hooks")).unwrap();
        fs::write(
            kiro_home.join("hooks/boomux.json"),
            include_str!("../../integrations/kiro/boomux.json"),
        )
        .unwrap();
        let kiro = bin.join("kiro-cli");
        fs::write(
            &kiro,
            "#!/bin/sh\n: > \"$KIRO_RECOVERY_CAPTURE\"\nfor arg do printf '%s\\0' \"$arg\" >> \"$KIRO_RECOVERY_CAPTURE\"; done\nprintf '%s' \"${BOOMUX_KIRO_LAUNCH_HOLDER-unset}\" > \"$KIRO_RECOVERY_MARKER\"\ncase \" $* \" in *' --resume-id '*) exit 0 ;; esac\n/bin/sleep 30\n",
        )
        .unwrap();
        fs::set_permissions(&kiro, fs::Permissions::from_mode(0o700)).unwrap();
        command
            .env("PATH", &bin)
            .env("KIRO_HOME", &kiro_home)
            .env(
                "KIRO_RECOVERY_CAPTURE",
                runtime_dir.join("kiro-recovery-argv"),
            )
            .env(
                "KIRO_RECOVERY_MARKER",
                runtime_dir.join("kiro-recovery-marker"),
            );
    });
    let kiro = daemon.runtime_dir.join("kiro-bin/kiro-cli");
    let workspace = daemon
        .client
        .create_workspace(
            "kiro-recovery",
            vec![ShellSpec {
                name: "kiro".into(),
                command: vec![kiro.display().to_string()],
                cwd: daemon.runtime_dir.clone(),
            }],
        )
        .unwrap();
    let shell_id = workspace.shells[0].id.clone();
    let attachment = daemon.client.attach(&shell_id, false, profile()).unwrap();
    wait_until(
        || {
            fs::read(daemon.runtime_dir.join("kiro-recovery-argv"))
                .is_ok_and(|argv| argv == b"--v3\0")
        },
        "initial Kiro run did not launch v3",
    );
    Uuid::parse_str(&fs::read_to_string(daemon.runtime_dir.join("kiro-recovery-marker")).unwrap())
        .unwrap();
    let first_run = daemon.client.get_shell(&shell_id).unwrap().run.unwrap();
    let agent = daemon
        .client
        .register_agent(
            &shell_id,
            &first_run.id,
            AgentRegistrationSpec {
                name: "Kiro CLI".into(),
                integration: "kiro".into(),
                external_session_id: Some("exact-kiro-session".into()),
                report: AgentReport {
                    state: AgentState::Idle,
                    authority: AgentAuthority::LifecycleIntegration,
                    evidence: "Kiro session idle".into(),
                    confidence: 100,
                },
            },
        )
        .unwrap();

    daemon.crash();
    drop(attachment.stream);
    let bin = daemon.runtime_dir.join("kiro-bin");
    let kiro_home = daemon.runtime_dir.join("kiro-home");
    let capture = daemon.runtime_dir.join("kiro-recovery-argv");
    let marker = daemon.runtime_dir.join("kiro-recovery-marker");
    daemon.restart_with(move |command| {
        command
            .env("PATH", bin)
            .env("KIRO_HOME", kiro_home)
            .env("KIRO_RECOVERY_CAPTURE", capture)
            .env("KIRO_RECOVERY_MARKER", marker);
    });
    let recovered = daemon.client.get_shell(&shell_id).unwrap();
    assert_eq!(
        recovered.recovered_agent_id.as_deref(),
        Some(agent.id.as_str())
    );

    let recovered_attachment = daemon.client.attach(&shell_id, false, profile()).unwrap();
    wait_until(
        || {
            fs::read(daemon.runtime_dir.join("kiro-recovery-argv"))
                .is_ok_and(|argv| argv == b"--v3\0chat\0--resume-id\0exact-kiro-session\0")
        },
        "recovered Kiro run did not resume the exact v3 session",
    );
    Uuid::parse_str(&fs::read_to_string(daemon.runtime_dir.join("kiro-recovery-marker")).unwrap())
        .unwrap();
    let second_run = daemon.client.get_shell(&shell_id).unwrap().run.unwrap();
    assert_ne!(second_run.id, first_run.id);

    drop(recovered_attachment);
    daemon.stop_with_cli();
}

#[test]
fn opencode_shared_runtime_and_claims_are_node_wide_and_ephemeral() {
    let mut daemon = TestDaemon::start_with(|command, runtime_dir| {
        let bin = runtime_dir.join("bin");
        fs::create_dir(&bin).unwrap();
        let opencode = bin.join("opencode");
        fs::write(
            &opencode,
            "#!/bin/sh\nexec python3 -c 'import socket,sys,time; assert sys.argv[1:4] == [\"serve\", \"--hostname\", \"127.0.0.1\"]; assert sys.argv[4] == \"--port\"; s=socket.socket(); s.bind((\"127.0.0.1\", int(sys.argv[5]))); s.listen(); time.sleep(60)' \"$@\"\n",
        )
        .unwrap();
        fs::set_permissions(&opencode, fs::Permissions::from_mode(0o700)).unwrap();
        command.env(
            "PATH",
            format!("{}:{}", bin.display(), std::env::var("PATH").unwrap()),
        );
    });
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    let occupied = ensure_test_opencode_runtime(&daemon, port).unwrap_err();
    assert_remote_code(&occupied, ErrorCode::Busy);
    drop(listener);
    let runtime = ensure_test_opencode_runtime(&daemon, port).unwrap();
    assert_eq!(
        ensure_test_opencode_runtime(&daemon, port).unwrap(),
        runtime
    );
    assert_eq!(
        daemon.client.get_opencode_shared_runtime().unwrap(),
        Some(runtime.clone())
    );
    let conflict = ensure_test_opencode_runtime(&daemon, port.saturating_add(1)).unwrap_err();
    assert_remote_code(&conflict, ErrorCode::Busy);

    let workspace = daemon
        .client
        .create_workspace(
            "shared-claim",
            vec![ShellSpec::login("opencode", std::env::temp_dir())],
        )
        .unwrap();
    let shell_id = workspace.shells[0].id.clone();
    let attachment = daemon.client.attach(&shell_id, false, profile()).unwrap();
    let run_id = daemon.client.get_shell(&shell_id).unwrap().run.unwrap().id;
    let holder_one = uuid::Uuid::new_v4().to_string();
    let holder_two = uuid::Uuid::new_v4().to_string();
    let spec = |root: &str| AgentRegistrationSpec {
        name: "opencode-agent".into(),
        integration: "opencode".into(),
        external_session_id: Some(root.into()),
        report: AgentReport {
            state: AgentState::Working,
            authority: AgentAuthority::LifecycleIntegration,
            evidence: "native shared claim".into(),
            confidence: 100,
        },
    };
    let (first, agent) = daemon
        .client
        .ensure_opencode_session_claim(
            &runtime.generation_id,
            &holder_one,
            "ses_native_shared",
            &shell_id,
            &run_id,
            spec("ses_native_shared"),
        )
        .unwrap();
    let baseline = daemon.client.events(None, 256, 0).unwrap().cursor;
    let (second, second_agent) = daemon
        .client
        .ensure_opencode_session_claim(
            &runtime.generation_id,
            &holder_two,
            "ses_native_shared",
            &shell_id,
            &run_id,
            spec("ses_native_shared"),
        )
        .unwrap();
    assert_eq!(second.claim_id, first.claim_id);
    assert_eq!(second.holder_count, 2);
    assert_eq!(second_agent.id, agent.id);
    assert!(
        daemon
            .client
            .events(Some(baseline), 256, 0)
            .unwrap()
            .events
            .is_empty()
    );
    let wrong_generation = daemon
        .client
        .report_claimed_opencode_agent(
            uuid::Uuid::new_v4().to_string(),
            "ses_native_shared",
            AgentReport {
                state: AgentState::Blocked,
                authority: AgentAuthority::LifecycleIntegration,
                evidence: "unauthorized".into(),
                confidence: 100,
            },
        )
        .unwrap_err();
    assert_remote_code(&wrong_generation, ErrorCode::NotFound);
    let runtime_pid = runtime.pid.unwrap() as libc::pid_t;
    drop(attachment);
    daemon.stop_with_cli();
    wait_until(
        || !process_exists(runtime_pid),
        "OpenCode shared runtime survived daemon stop",
    );
}

#[test]
fn opencode_runtime_receives_exact_ephemeral_environment_and_cold_adopts() {
    let mut daemon = TestDaemon::start_with(|command, runtime_dir| {
        let bin = runtime_dir.join("bin");
        fs::create_dir(&bin).unwrap();
        let opencode = bin.join("opencode");
        fs::write(
            &opencode,
            "#!/bin/sh\nprintf '%s\\n%s\\n%s\\n' \"$OPENCODE_SERVER_USERNAME\" \"$OPENCODE_SERVER_PASSWORD\" \"${BOOMUX_SHELL_ID-unset}\" > \"$CAPTURE\"\nexec python3 -c 'import socket,sys,time; s=socket.socket(); s.bind((\"127.0.0.1\", int(sys.argv[5]))); s.listen(); time.sleep(60)' \"$@\"\n",
        )
        .unwrap();
        fs::set_permissions(&opencode, fs::Permissions::from_mode(0o700)).unwrap();
        command.env(
            "PATH",
            format!("{}:{}", bin.display(), std::env::var("PATH").unwrap()),
        );
    });
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    let capture = daemon.runtime_dir.join("opencode-environment");
    let path = format!(
        "{}:{}",
        daemon.runtime_dir.join("bin").display(),
        std::env::var("PATH").unwrap()
    );
    let response = daemon
        .client
        .request(Request::EnsureOpenCodeSharedRuntime {
            port,
            environment: Some(UnixEnvironment {
                variables: vec![
                    UnixEnvironmentVariable {
                        name: b"PATH".to_vec(),
                        value: path.into_bytes(),
                    },
                    UnixEnvironmentVariable {
                        name: b"CAPTURE".to_vec(),
                        value: capture.as_os_str().as_encoded_bytes().to_vec(),
                    },
                    UnixEnvironmentVariable {
                        name: b"OPENCODE_SERVER_USERNAME".to_vec(),
                        value: b"boomux-user".to_vec(),
                    },
                    UnixEnvironmentVariable {
                        name: b"OPENCODE_SERVER_PASSWORD".to_vec(),
                        value: b"boomux-password".to_vec(),
                    },
                    UnixEnvironmentVariable {
                        name: b"BOOMUX_SHELL_ID".to_vec(),
                        value: b"must-not-leak".to_vec(),
                    },
                ],
            }),
        })
        .unwrap();
    let Response::OpenCodeSharedRuntime {
        runtime: Some(before),
    } = response
    else {
        panic!("unexpected shared runtime response: {response:?}");
    };
    assert_eq!(
        fs::read_to_string(&capture).unwrap(),
        "boomux-user\nboomux-password\nunset\n"
    );
    let pid = before.pid.unwrap() as libc::pid_t;

    daemon.crash();
    assert!(process_exists(pid));
    daemon.restart();
    let after = ensure_test_opencode_runtime(&daemon, port).unwrap();
    assert_eq!(after, before);
    daemon.stop_with_cli();
    wait_until(
        || !process_exists(pid),
        "cold-adopted OpenCode runtime survived daemon stop",
    );
    assert!(TcpListener::bind(("127.0.0.1", port)).is_ok());
}

#[test]
fn cold_recovery_attaches_opencode_session_to_shared_runtime_and_new_run() {
    let mut daemon = TestDaemon::start_with(|command, runtime_dir| {
        let runtime_bin = runtime_dir.join("bin");
        let shell_bin = runtime_dir.join("shell-bin");
        fs::create_dir(&runtime_bin).unwrap();
        fs::create_dir(&shell_bin).unwrap();
        let runtime_opencode = runtime_bin.join("opencode");
        fs::write(
            &runtime_opencode,
            "#!/bin/sh\ncase \"${1-}\" in\n  serve) exec python3 -c 'import socket,sys,time; s=socket.socket(); s.bind((\"127.0.0.1\", int(sys.argv[5]))); s.listen(); time.sleep(60)' \"$@\" ;;\n  attach) printf 'path-decoy\\n' > \"$CAPTURE\"; exit 1 ;;\n  *) while :; do sleep 60; done ;;\nesac\n",
        )
        .unwrap();
        fs::set_permissions(&runtime_opencode, fs::Permissions::from_mode(0o700)).unwrap();
        let shell_opencode = shell_bin.join("opencode");
        fs::write(
            &shell_opencode,
            "#!/bin/sh\ncase \"${1-}\" in\n  attach) { for arg do printf 'arg:%s\\n' \"$arg\"; done; printf 'generation:%s\\nholder:%s\\nshell:%s\\nrun:%s\\n' \"$BOOMUX_OPENCODE_SHARED_GENERATION\" \"$BOOMUX_OPENCODE_CLAIM_HOLDER\" \"$BOOMUX_SHELL_ID\" \"$BOOMUX_RUN_ID\"; } > \"$CAPTURE\"; while :; do sleep 60; done ;;\n  *) while :; do sleep 60; done ;;\nesac\n",
        )
        .unwrap();
        fs::set_permissions(&shell_opencode, fs::Permissions::from_mode(0o700)).unwrap();
        command
            .env(
                "PATH",
                format!(
                    "{}:{}",
                    runtime_bin.display(),
                    std::env::var("PATH").unwrap()
                ),
            )
            .env("CAPTURE", runtime_dir.join("recovered-opencode"));
    });
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    let runtime = ensure_test_opencode_runtime(&daemon, port).unwrap();
    let opencode_directory = daemon.runtime_dir.join("shell-bin");
    let workspace = daemon
        .client
        .create_workspace(
            "recover-shared-opencode",
            vec![ShellSpec {
                name: "opencode".into(),
                command: vec!["./opencode".into()],
                cwd: opencode_directory,
            }],
        )
        .unwrap();
    let shell_id = workspace.shells[0].id.clone();
    let attachment = daemon.client.attach(&shell_id, false, profile()).unwrap();
    let first_run = daemon.client.get_shell(&shell_id).unwrap().run.unwrap();
    daemon
        .client
        .register_agent(
            &shell_id,
            &first_run.id,
            AgentRegistrationSpec {
                name: "OpenCode".into(),
                integration: "opencode".into(),
                external_session_id: Some("ses_recovered_exact".into()),
                report: AgentReport {
                    state: AgentState::Idle,
                    authority: AgentAuthority::LifecycleIntegration,
                    evidence: "idle before interruption".into(),
                    confidence: 100,
                },
            },
        )
        .unwrap();

    daemon.crash();
    drop(attachment.stream);
    let path = format!(
        "{}:{}",
        daemon.runtime_dir.join("bin").display(),
        std::env::var("PATH").unwrap()
    );
    let capture = daemon.runtime_dir.join("recovered-opencode");
    let restart_capture = capture.clone();
    daemon.restart_with(|command| {
        command.env("PATH", path).env("CAPTURE", restart_capture);
    });
    let recovered_attachment = daemon.client.attach(&shell_id, false, profile()).unwrap();
    wait_until(
        || capture.is_file(),
        "recovered OpenCode did not attach to the shared runtime",
    );
    let second_run = daemon.client.get_shell(&shell_id).unwrap().run.unwrap();
    assert_eq!(second_run.generation, first_run.generation + 1);
    assert_ne!(second_run.id, first_run.id);
    let captured = fs::read_to_string(&capture).unwrap();
    assert!(captured.contains("arg:attach\n"));
    assert!(captured.contains(&format!("arg:{}\n", runtime.url)));
    assert!(captured.contains("arg:--session\narg:ses_recovered_exact\n"));
    assert!(captured.contains(&format!("generation:{}\n", runtime.generation_id)));
    assert!(captured.contains(&format!("shell:{shell_id}\n")));
    assert!(captured.contains(&format!("run:{}\n", second_run.id)));
    let holder = captured
        .lines()
        .find_map(|line| line.strip_prefix("holder:"))
        .filter(|holder| !holder.is_empty())
        .unwrap();
    let registration = AgentRegistrationSpec {
        name: "OpenCode".into(),
        integration: "opencode".into(),
        external_session_id: Some("ses_recovered_exact".into()),
        report: AgentReport {
            state: AgentState::Unknown,
            authority: AgentAuthority::LifecycleIntegration,
            evidence: "direct recovered TUI selection".into(),
            confidence: 100,
        },
    };
    let (claim, agent) = daemon
        .client
        .ensure_opencode_session_claim(
            &runtime.generation_id,
            holder,
            "ses_recovered_exact",
            &shell_id,
            &second_run.id,
            registration,
        )
        .unwrap();
    assert_eq!(claim.run_id, second_run.id);
    assert_eq!(agent.run_id, second_run.id);
    assert_eq!(
        daemon
            .client
            .resolve_opencode_session_claim(&runtime.generation_id, "ses_recovered_exact")
            .unwrap()
            .0,
        claim
    );

    drop(recovered_attachment);
    daemon.stop_with_cli();
}

#[test]
fn supervised_opencode_resume_attaches_to_warm_shared_runtime_without_cold_start() {
    let mut daemon = TestDaemon::start_with(|command, runtime_dir| {
        let runtime_bin = runtime_dir.join("bin");
        fs::create_dir(&runtime_bin).unwrap();
        let opencode = runtime_bin.join("opencode");
        fs::write(
            &opencode,
            "#!/bin/sh\ncase \"${1-}\" in\n  serve) exec python3 -c 'import socket,sys,time; s=socket.socket(); s.bind((\"127.0.0.1\", int(sys.argv[5]))); s.listen(); time.sleep(60)' \"$@\" ;;\n  attach) { for arg do printf 'arg:%s\\n' \"$arg\"; done; printf 'generation:%s\\nshell:%s\\nrun:%s\\n' \"$BOOMUX_OPENCODE_SHARED_GENERATION\" \"$BOOMUX_SHELL_ID\" \"$BOOMUX_RUN_ID\"; } > \"$CAPTURE\"; while :; do sleep 60; done ;;\n  *) printf 'standalone:%s\\n' \"$*\" > \"$STANDALONE_CAPTURE\"; while :; do sleep 60; done ;;\nesac\n",
        )
        .unwrap();
        fs::set_permissions(&opencode, fs::Permissions::from_mode(0o700)).unwrap();
        command
            .env(
                "PATH",
                format!(
                    "{}:{}",
                    runtime_bin.display(),
                    std::env::var("PATH").unwrap()
                ),
            )
            .env("CAPTURE", runtime_dir.join("shared-resume"))
            .env("STANDALONE_CAPTURE", runtime_dir.join("standalone-resume"));
    });
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    let runtime = ensure_test_opencode_runtime(&daemon, port).unwrap();
    let session_id = "ses_managed_resume_exact";
    let workspace = daemon
        .client
        .create_workspace(
            "managed-shared-opencode",
            vec![ShellSpec {
                name: "opencode-session".into(),
                command: vec![
                    daemon.executable.display().to_string(),
                    "agent".into(),
                    "supervise".into(),
                    "OpenCode".into(),
                    "--integration".into(),
                    "opencode".into(),
                    "--external-session-id".into(),
                    session_id.into(),
                    "--".into(),
                    "opencode".into(),
                    "--session".into(),
                    session_id.into(),
                ],
                cwd: daemon.runtime_dir.clone(),
            }],
        )
        .unwrap();
    let shell_id = workspace.shells[0].id.clone();
    let capture = daemon.runtime_dir.join("shared-resume");
    let standalone_capture = daemon.runtime_dir.join("standalone-resume");

    let started = Instant::now();
    let attachment = daemon.client.attach(&shell_id, false, profile()).unwrap();
    wait_until(
        || capture.is_file(),
        "supervised OpenCode resume did not attach to the shared runtime",
    );
    let elapsed = started.elapsed();
    let shell = daemon.client.get_shell(&shell_id).unwrap();
    let run = shell.run.unwrap();
    let captured = fs::read_to_string(capture).unwrap();
    assert!(captured.contains("arg:attach\n"));
    assert!(captured.contains(&format!("arg:{}\n", runtime.url)));
    assert!(captured.contains(&format!("arg:--session\narg:{session_id}\n")));
    assert!(captured.contains(&format!("generation:{}\n", runtime.generation_id)));
    assert!(captured.contains(&format!("shell:{shell_id}\n")));
    assert!(captured.contains(&format!("run:{}\n", run.id)));
    assert!(!standalone_capture.exists());
    assert!(
        elapsed < Duration::from_secs(2),
        "warm shared resume took {elapsed:?}"
    );
    eprintln!("warm shared OpenCode resume attached in {elapsed:?}");

    drop(attachment);
    daemon.stop_with_cli();
}

#[test]
fn recovered_opencode_fallback_preserves_user_environment_and_exact_session() {
    let mut daemon = TestDaemon::start();
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    let empty_bin = daemon.runtime_dir.join("empty-bin");
    fs::create_dir(&empty_bin).unwrap();
    let capture = daemon.runtime_dir.join("standalone-recovery");
    let fallback = daemon.runtime_dir.join("fallback-opencode");
    fs::write(
        &fallback,
        "#!/bin/sh\n{ for arg do printf 'arg:%s\\n' \"$arg\"; done; printf 'path:%s\\ntui:%s\\nreal:%s\\noriginal:%s\\nshim:%s\\nprivate-tui:%s\\nclaude:%s\\nclaude-policy:%s\\nzdot:%s\\n' \"$PATH\" \"${OPENCODE_TUI_CONFIG-unset}\" \"${BOOMUX_REAL_OPENCODE-unset}\" \"${BOOMUX_ORIGINAL_PATH-unset}\" \"${BOOMUX_SHIM_EXECUTABLE-unset}\" \"${BOOMUX_OPENCODE_TUI_CONFIG-unset}\" \"${BOOMUX_REAL_CLAUDE-unset}\" \"${BOOMUX_CLAUDE_REMOTE_CONTROL-unset}\" \"${BOOMUX_USER_ZDOTDIR-unset}\"; } > \"$CAPTURE\"\n",
    )
    .unwrap();
    fs::set_permissions(&fallback, fs::Permissions::from_mode(0o700)).unwrap();
    let output = daemon
        .command()
        .args([
            "opencode",
            "shared",
            "--session",
            "ses_fallback_exact",
            "--port",
            &port.to_string(),
        ])
        .env("PATH", &empty_bin)
        .env("CAPTURE", &capture)
        .env("BOOMUX_SHELL_ID", "shell-exact")
        .env("BOOMUX_RUN_ID", "run-exact")
        .env("BOOMUX_REAL_OPENCODE", &fallback)
        .env("BOOMUX_ORIGINAL_PATH", "/user/original-bin")
        .env("BOOMUX_SHIM_EXECUTABLE", &daemon.executable)
        .env("BOOMUX_OPENCODE_TUI_CONFIG", "/private/tui.json")
        .env("OPENCODE_TUI_CONFIG", "/user/tui.json")
        .env("BOOMUX_REAL_CLAUDE", "/private/claude")
        .env("BOOMUX_CLAUDE_REMOTE_CONTROL", "1")
        .env("BOOMUX_USER_ZDOTDIR", "/user/zdotdir")
        .output()
        .unwrap();
    assert!(output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("continuing standalone"));
    assert_eq!(
        fs::read_to_string(capture).unwrap(),
        "arg:--session\narg:ses_fallback_exact\npath:/user/original-bin\ntui:/user/tui.json\nreal:unset\noriginal:unset\nshim:unset\nprivate-tui:unset\nclaude:unset\nclaude-policy:unset\nzdot:unset\n"
    );
    daemon.stop_with_cli();
}

#[test]
fn agent_runtime_is_revisioned_and_durable() {
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

    let host_bin = daemon.runtime_dir.join("host-bin");
    fs::create_dir(&host_bin).unwrap();
    let terminal_resolver = host_bin.join("xdg-terminal-exec");
    fs::write(
        &terminal_resolver,
        "#!/bin/sh\nif [ \"${1-}\" = --print-id ]; then printf 'test.desktop\\n'; else printf '/bin/true\\0'; fi\n",
    )
    .unwrap();
    fs::set_permissions(&terminal_resolver, fs::Permissions::from_mode(0o755)).unwrap();
    let blocked_sessions = daemon
        .command()
        .args(["session", "list", "--workspace", &workspace.id, "--json"])
        .env("PATH", &host_bin)
        .output()
        .unwrap();
    assert!(blocked_sessions.status.success());
    let blocked_sessions: serde_json::Value =
        serde_json::from_slice(&blocked_sessions.stdout).unwrap();
    let blocked_session_id = blocked_sessions["data"]["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|session| session["attentions"][0]["agent_id"] == adapter_id)
        .and_then(|session| session["id"].as_str())
        .expect("blocked Agent attention was not projected onto its Session");
    let opened = daemon
        .command()
        .args([
            "--terminal",
            "test.desktop",
            "session",
            "open",
            blocked_session_id,
        ])
        .env("PATH", &host_bin)
        .output()
        .unwrap();
    assert!(
        opened.status.success(),
        "{}",
        String::from_utf8_lossy(&opened.stderr)
    );
    assert!(
        daemon
            .client
            .get_agent(&adapter_id)
            .unwrap()
            .attention
            .is_none()
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
    let session_attention = daemon
        .command()
        .args(["session", "list", "--workspace", &workspace.id, "--json"])
        .env("PATH", &host_bin)
        .output()
        .unwrap();
    assert!(session_attention.status.success());
    let session_attention: serde_json::Value =
        serde_json::from_slice(&session_attention.stdout).unwrap();
    let adapter_session = session_attention["data"]["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|session| session["attentions"][0]["agent_id"] == adapter_id)
        .expect("completed Agent attention was not projected onto its Session");
    assert_eq!(adapter_session["attentions"][0]["reason"], "completed");
    assert_eq!(adapter_session["attentions"][0]["observation_revision"], 3);
    assert!(adapter_session["git_branch"].is_null());

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
            versioned_request(&daemon.client, protocol::PROTOCOL_VERSION, request),
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
    let workspace_revision = projected["workspace_revision"].as_u64().unwrap();

    let renamed = daemon
        .command()
        .args([
            "session",
            "rename",
            session_id,
            "Checkout retry investigation",
            "--revision",
            &workspace_revision.to_string(),
            "--json",
        ])
        .env("PATH", &host_bin)
        .output()
        .unwrap();
    assert!(
        renamed.status.success(),
        "{}",
        String::from_utf8_lossy(&renamed.stderr)
    );
    let renamed: serde_json::Value = serde_json::from_slice(&renamed.stdout).unwrap();
    assert_eq!(renamed["command"], "session.rename");
    assert_eq!(
        renamed["data"]["result"]["user_display_name"],
        "Checkout retry investigation"
    );
    assert_eq!(renamed["data"]["result"]["session_id"], session_id);
    assert_eq!(renamed["data"]["result"]["workspace_id"], workspace.id);
    assert_eq!(renamed["data"]["result"]["changed"], true);
    let renamed_revision = renamed["data"]["result"]["workspace_revision"]
        .as_u64()
        .unwrap();
    assert_eq!(renamed_revision, workspace_revision + 1);

    let stale = daemon
        .command()
        .args([
            "session",
            "reset-name",
            session_id,
            "--revision",
            &workspace_revision.to_string(),
            "--json",
        ])
        .env("PATH", &host_bin)
        .output()
        .unwrap();
    assert!(!stale.status.success());
    let stale: serde_json::Value = serde_json::from_slice(&stale.stderr).unwrap();
    assert_eq!(stale["command"], "session.reset-name");
    assert_eq!(stale["error"]["code"], "revision_ahead");

    let reset = daemon
        .command()
        .args([
            "session",
            "reset-name",
            session_id,
            "--revision",
            &renamed_revision.to_string(),
            "--json",
        ])
        .env("PATH", &host_bin)
        .output()
        .unwrap();
    assert!(
        reset.status.success(),
        "{}",
        String::from_utf8_lossy(&reset.stderr)
    );
    let reset: serde_json::Value = serde_json::from_slice(&reset.stdout).unwrap();
    assert_eq!(reset["command"], "session.reset-name");
    assert!(reset["data"]["result"]["user_display_name"].is_null());
    assert_eq!(reset["data"]["result"]["session_id"], session_id);
    assert_eq!(reset["data"]["result"]["changed"], true);

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
    assert!(
        std::process::Command::new("git")
            .args(["init", "-q", "-b", "feat/durable-context"])
            .arg(&project)
            .status()
            .unwrap()
            .success()
    );
    let observed_project = daemon.runtime_dir.join("durable-observed-project");
    fs::create_dir(&observed_project).unwrap();
    assert!(
        std::process::Command::new("git")
            .args(["init", "-q", "-b", "feat/observed-context"])
            .arg(&observed_project)
            .status()
            .unwrap()
            .success()
    );
    fs::write(observed_project.join("staged.txt"), "staged\n").unwrap();
    assert!(
        std::process::Command::new("git")
            .args([
                "-C",
                observed_project.to_str().unwrap(),
                "add",
                "staged.txt",
            ])
            .status()
            .unwrap()
            .success()
    );
    fs::write(observed_project.join("untracked.txt"), "untracked\n").unwrap();
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
    let context_cursor = daemon.client.events(None, 256, 0).unwrap().cursor;
    let (launch_observed, changed) = daemon
        .client
        .observe_agent_working_context(&agent.id, &shell_id, &run_id, project.clone())
        .unwrap();
    assert!(changed);
    assert_eq!(launch_observed.working_contexts.len(), 1);
    assert_eq!(
        launch_observed.working_contexts[0].repository,
        "durable-source-project"
    );
    assert_eq!(
        launch_observed.working_contexts[0].branch,
        "feat/durable-context"
    );
    let (observed, changed) = daemon
        .client
        .observe_agent_working_context(&agent.id, &shell_id, &run_id, observed_project.clone())
        .unwrap();
    assert!(changed);
    assert_eq!(observed.working_contexts.len(), 2);
    assert_eq!(
        observed.working_contexts[0].repository,
        "durable-observed-project"
    );
    assert_eq!(observed.working_contexts[0].branch, "feat/observed-context");
    assert!(
        daemon
            .client
            .events(Some(context_cursor), 256, 0)
            .unwrap()
            .events
            .iter()
            .any(|event| matches!(
                &event.kind,
                protocol::DaemonEventKind::AgentWorkingContextObserved { agent, .. }
                    if agent.id == observed.id
            ))
    );

    let result = daemon
        .client
        .host_service(protocol::HostServiceOperation::ListAgentSessions {
            workspace_id: Some(workspace.id.clone()),
        })
        .unwrap();
    let protocol::HostServiceResult::AgentSessions { sessions } = result else {
        panic!("unexpected Session list response");
    };
    let projected = &sessions[0];
    assert_eq!(
        projected.git_branch.as_deref(),
        Some("feat/durable-context")
    );
    assert_eq!(projected.working_context_count, 1);
    assert_eq!(
        projected.working_contexts[0].repository,
        "durable-observed-project"
    );
    assert_eq!(
        projected.working_contexts[0].branch,
        "feat/observed-context"
    );
    assert_eq!(projected.working_contexts[0].push_status, None);
    assert_eq!(
        projected.working_contexts[0].worktree_status,
        Some(protocol::HostGitWorktreeStatus {
            staged: true,
            unstaged_or_untracked: true,
        })
    );
    let operation_id = Uuid::new_v4().to_string();
    let session_id = projected.id.clone();
    let expected_revision = projected.workspace_revision;
    let accepted = daemon
        .client
        .set_agent_session_display_name(
            operation_id.clone(),
            session_id.clone(),
            expected_revision,
            Some("Durable investigation".into()),
        )
        .unwrap();
    daemon
        .client
        .rename_workspace(&workspace.id, "renamed-durable-source")
        .unwrap();

    daemon.client.close_shell(&shell_id).unwrap();
    drop(attachment);
    assert!(daemon.client.get_shell(&shell_id).is_err());
    daemon.stop_with_cli();
    daemon.restart();

    let restored_agent = daemon.client.get_agent(&agent.id).unwrap();
    assert_eq!(restored_agent.working_contexts, observed.working_contexts);

    let replayed = daemon
        .client
        .set_agent_session_display_name(
            operation_id,
            session_id,
            expected_revision,
            Some("Durable investigation".into()),
        )
        .unwrap();
    assert_eq!(replayed, accepted);

    let sessions = daemon
        .command()
        .args(["session", "list", "--workspace", &workspace.id, "--json"])
        .output()
        .unwrap();
    assert!(sessions.status.success());
    let sessions: serde_json::Value = serde_json::from_slice(&sessions.stdout).unwrap();
    assert_eq!(
        sessions["data"]["sessions"][0]["description"],
        "Durable investigation"
    );
    assert_eq!(
        sessions["data"]["sessions"][0]["user_display_name"],
        "Durable investigation"
    );
    assert_eq!(
        sessions["data"]["sessions"][0]["workspace_name"],
        "renamed-durable-source"
    );
    assert_eq!(sessions["data"]["sessions"][0]["working_context_count"], 1);
    assert_eq!(
        sessions["data"]["sessions"][0]["working_contexts"][0]["repository"],
        "durable-observed-project"
    );
    assert_eq!(
        sessions["data"]["sessions"][0]["working_contexts"][0]["branch"],
        "feat/observed-context"
    );
    assert_eq!(
        sessions["data"]["sessions"][0]["working_contexts"][0]["worktree_status"]["staged"],
        true
    );
    assert_eq!(
        sessions["data"]["sessions"][0]["working_contexts"][0]["worktree_status"]["unstaged_or_untracked"],
        true
    );
    let protocol_fifty = versioned_raw_request(
        &daemon.client,
        50,
        Request::HostService {
            operation: protocol::HostServiceOperation::ListAgentSessions {
                workspace_id: Some(workspace.id.clone()),
            },
        },
    );
    let old_context = &protocol_fifty.message["result"]["sessions"][0]["working_contexts"][0];
    assert_eq!(protocol_fifty.version, 50);
    assert!(old_context.get("push_status").is_none());
    assert!(old_context.get("worktree_status").is_none());
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
fn cold_recovery_presents_resumable_agent() {
    let mut daemon = TestDaemon::start();
    let workspace = daemon
        .client
        .create_workspace(
            "recover-agent-presentation",
            vec![ShellSpec::login("agent", std::env::temp_dir())],
        )
        .unwrap();
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

    daemon.stop_with_cli();
}

#[test]
fn session_hide_is_durable_non_destructive_and_protocol_scoped() {
    let mut daemon = TestDaemon::start();
    let workspace = daemon
        .client
        .create_workspace(
            "hidden-session",
            vec![ShellSpec {
                name: "agent".into(),
                command: vec!["/bin/sleep".into(), "300".into()],
                cwd: std::env::temp_dir(),
            }],
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
                name: "hidden agent".into(),
                integration: "opencode".into(),
                external_session_id: Some("hidden-external".into()),
                report: AgentReport {
                    state: AgentState::Idle,
                    authority: AgentAuthority::LifecycleIntegration,
                    evidence: "ready".into(),
                    confidence: 100,
                },
            },
        )
        .unwrap();
    let protocol::HostServiceResult::AgentSessions { sessions } = daemon
        .client
        .host_service(protocol::HostServiceOperation::ListAgentSessions {
            workspace_id: Some(workspace.id.clone()),
        })
        .unwrap()
    else {
        panic!("unexpected Session list response");
    };
    let session_id = sessions[0].id.clone();
    assert!(matches!(
        versioned_request(
            &daemon.client,
            50,
            Request::HideAgentSession {
                operation_id: Uuid::new_v4().to_string(),
                session_id: session_id.clone(),
                workspace_id: workspace.id.clone(),
                expected_workspace_revision: sessions[0].workspace_revision,
            },
        ),
        Response::Error {
            code: Some(ErrorCode::UnsupportedVersion),
            ..
        }
    ));
    let cursor = daemon.client.events(None, 256, 0).unwrap().cursor;

    let hidden = daemon
        .command()
        .args([
            "session",
            "hide",
            &session_id,
            "--workspace",
            &workspace.id,
            "--json",
        ])
        .output()
        .unwrap();
    assert!(
        hidden.status.success(),
        "{}",
        String::from_utf8_lossy(&hidden.stderr)
    );
    let hidden: serde_json::Value = serde_json::from_slice(&hidden.stdout).unwrap();
    assert_eq!(hidden["command"], "session.hide");
    assert_eq!(hidden["data"]["result"]["session_id"], session_id);
    assert_eq!(hidden["data"]["result"]["workspace_id"], workspace.id);
    assert_eq!(hidden["data"]["result"]["changed"], true);
    let hidden_revision = hidden["data"]["result"]["workspace_revision"]
        .as_u64()
        .unwrap();

    let protocol::HostServiceResult::AgentSessions { sessions } = daemon
        .client
        .host_service(protocol::HostServiceOperation::ListAgentSessions {
            workspace_id: Some(workspace.id.clone()),
        })
        .unwrap()
    else {
        panic!("unexpected Session list response");
    };
    assert!(sessions.is_empty());
    for request in [
        Request::HostService {
            operation: protocol::HostServiceOperation::InspectAgentSession {
                session_id: session_id.clone(),
            },
        },
        Request::HostService {
            operation: protocol::HostServiceOperation::ResolveAgentSession {
                workspace_id: workspace.id.clone(),
                agent_id: agent.id.clone(),
            },
        },
        Request::ResumeAgentSession {
            session_id: session_id.clone(),
            profile: profile(),
        },
    ] {
        assert!(matches!(
            versioned_request(&daemon.client, 51, request),
            Response::Error {
                code: Some(ErrorCode::NotFound),
                ..
            }
        ));
    }

    let Response::HostService {
        result: protocol::HostServiceResult::AgentSessions { sessions },
    } = versioned_request(
        &daemon.client,
        50,
        Request::HostService {
            operation: protocol::HostServiceOperation::ListAgentSessions {
                workspace_id: Some(workspace.id.clone()),
            },
        },
    )
    else {
        panic!("unexpected protocol-50 Session list response");
    };
    assert_eq!(sessions.len(), 1);
    assert!(matches!(
        versioned_request(
            &daemon.client,
            50,
            Request::HostService {
                operation: protocol::HostServiceOperation::InspectAgentSession {
                    session_id: session_id.clone(),
                },
            },
        ),
        Response::HostService {
            result: protocol::HostServiceResult::AgentSession { .. }
        }
    ));

    let current_events = daemon.client.events(Some(cursor.clone()), 256, 0).unwrap();
    assert!(current_events.events.iter().any(|event| matches!(
        event.kind,
        protocol::DaemonEventKind::AgentSessionHidden { .. }
    )));
    let Response::Events {
        cursor: old_cursor,
        events: old_events,
        ..
    } = versioned_request(
        &daemon.client,
        50,
        Request::Events {
            after: Some(cursor),
            limit: 256,
            wait_ms: 0,
        },
    )
    else {
        panic!("unexpected protocol-50 events response");
    };
    assert_eq!(old_cursor, current_events.cursor);
    assert!(old_events.is_empty());
    assert_eq!(daemon.client.get_agent(&agent.id).unwrap(), agent);
    let shell = daemon.client.get_shell(&shell_id).unwrap();
    assert_eq!(
        shell.run.as_ref().map(|run| run.id.as_str()),
        Some(run_id.as_str())
    );

    let repeated = daemon
        .command()
        .args([
            "session",
            "hide",
            &session_id,
            "--workspace",
            &workspace.id,
            "--json",
        ])
        .output()
        .unwrap();
    assert!(repeated.status.success());
    let repeated: serde_json::Value = serde_json::from_slice(&repeated.stdout).unwrap();
    assert_eq!(repeated["data"]["result"]["changed"], false);
    assert_eq!(
        repeated["data"]["result"]["workspace_revision"],
        hidden_revision
    );

    let persisted = fs::read_to_string(daemon.runtime_dir.join("state/boomux/state.json")).unwrap();
    assert!(persisted.contains("\"version\": 17"));
    assert!(persisted.contains("\"hidden_sessions\""));
    drop(attachment);
    daemon.crash();
    daemon.restart();
    let protocol::HostServiceResult::AgentSessions { sessions } = daemon
        .client
        .host_service(protocol::HostServiceOperation::ListAgentSessions {
            workspace_id: Some(workspace.id.clone()),
        })
        .unwrap()
    else {
        panic!("unexpected restored Session list response");
    };
    assert!(sessions.is_empty());
    assert_eq!(daemon.client.get_agent(&agent.id).unwrap().id, agent.id);

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

fn versioned_raw_request(
    client: &Client,
    version: u32,
    request: protocol::Request,
) -> protocol::Envelope<serde_json::Value> {
    let mut stream = UnixStream::connect(client.socket_path()).unwrap();
    protocol::write_message(
        &mut stream,
        &protocol::Envelope::with_version(version, request),
    )
    .unwrap();
    protocol::read_message(&mut stream).unwrap()
}

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::time::Duration;

use boomux::protocol::{
    AgentAuthority, AgentRegistrationSpec, AgentReport, AgentState, AttachFrame,
    HostServiceIntegrationAction, HostServiceOperation, HostServiceResult, ShellSpec,
    WorkspaceLauncherSpec,
};

use crate::support::{CONTROL_MASTER_PREFIX, TestDaemon, profile, wait_until};

#[test]
fn registered_node_host_services_use_only_owner_path_config_cwd_and_stored_argv() {
    let root = std::env::temp_dir().join(format!(
        "boomux-host-services-{}-{}",
        std::process::id(),
        uuid::Uuid::new_v4()
    ));
    let ssh_bin = root.join("ssh-bin");
    let owner_bin = root.join("owner-bin");
    let owner_projects = root.join("owner-projects");
    let owner_config = root.join("owner-config.toml");
    fs::create_dir_all(&ssh_bin).unwrap();
    fs::create_dir_all(&owner_bin).unwrap();
    fs::create_dir_all(owner_projects.join("remote-only/.git")).unwrap();
    fs::write(
        &owner_config,
        format!(
            "[projects]\nroots = [{}]\nmax_depth = 1\n",
            toml::Value::String(owner_projects.display().to_string())
        ),
    )
    .unwrap();
    let output = root.join("owner-launcher-output");
    let launcher = owner_bin.join("capture-launcher");
    fs::write(
        &launcher,
        "#!/bin/sh\nprintf '%s\\n%s\\n%s\\n' \"$PWD\" \"$1\" \"$2\" > \"$3\"\n",
    )
    .unwrap();
    fs::set_permissions(&launcher, fs::Permissions::from_mode(0o700)).unwrap();
    let resume_output = root.join("owner-resume-output");
    let pi = owner_bin.join("pi");
    fs::write(
        &pi,
        format!(
            "#!/bin/sh\nprintf '%s\\n%s\\n%s\\n' \"$1\" \"$2\" \"$PWD\" > '{}'\nprintf 'owner resume\\n'\n",
            resume_output.display()
        ),
    )
    .unwrap();
    fs::set_permissions(&pi, fs::Permissions::from_mode(0o700)).unwrap();

    let owner_path = format!("{}:/usr/bin:/bin", owner_bin.display());
    let mut owner = TestDaemon::start_with(|command, _| {
        command
            .env("PATH", &owner_path)
            .env("BOOMUX_CONFIG", &owner_config)
            .env("HOME", root.join("owner-home"));
    });
    let owner_id = owner.client.node_identity().unwrap();

    let ssh = ssh_bin.join("ssh");
    fs::write(
        &ssh,
        format!(
            "#!/bin/sh\n{CONTROL_MASTER_PREFIX}\nlast=\nfor arg do last=$arg; done\ncase \"$last\" in\n  *boomux-platform-v1*) printf 'boomux-platform-v1\\0Linux\\0x86_64\\0' ;;\n  *boomux-executables-v1*) printf 'boomux-executables-v1\\0{}\\0' ;;\n  *boomux-install-destination-v1*) printf 'boomux-install-destination-v1\\0{}\\0' ;;\n  *__federation-stdio*) exec env XDG_RUNTIME_DIR='{}' XDG_STATE_HOME='{}' '{}' __federation-stdio ;;\n  *) exit 64 ;;\nesac\n",
            owner.executable.display(),
            owner.executable.display(),
            owner.runtime_dir.display(),
            owner.runtime_dir.join("state").display(),
            owner.executable.display(),
        ),
    )
    .unwrap();
    fs::set_permissions(&ssh, fs::Permissions::from_mode(0o700)).unwrap();

    let local_path = format!("{}:/usr/bin:/bin", ssh_bin.display());
    let mut local = TestDaemon::start_with(|command, _| {
        command
            .env("PATH", &local_path)
            .env("HOME", root.join("local-home"));
    });
    local
        .client
        .add_node_registration("owner", "fake-owner", &owner_id)
        .unwrap();

    let workspace = owner
        .client
        .create_workspace("remote-host", Vec::new())
        .unwrap();
    let exact_argument = "$(touch local-pwned); semi; colon";
    let stored_launcher = owner
        .client
        .create_launcher(
            &workspace.id,
            WorkspaceLauncherSpec {
                name: "capture".into(),
                cwd: owner_projects.join("remote-only"),
                command: vec![
                    launcher.display().to_string(),
                    exact_argument.into(),
                    "two words".into(),
                    output.display().to_string(),
                ],
            },
        )
        .unwrap();

    let projects = local
        .client
        .route_node_host_service(&owner_id, HostServiceOperation::DiscoverProjects)
        .unwrap();
    let HostServiceResult::Projects { discovery } = projects else {
        panic!("unexpected project response");
    };
    assert_eq!(discovery.projects.len(), 1);
    assert_eq!(discovery.projects[0].name, "remote-only");
    assert!(discovery.projects[0].path.starts_with(&owner_projects));

    let invoked = local
        .client
        .route_node_host_service(
            &owner_id,
            HostServiceOperation::InvokeLauncher {
                workspace_id: workspace.id.clone(),
                launcher_id: stored_launcher.id.clone(),
            },
        )
        .unwrap();
    assert!(matches!(invoked, HostServiceResult::LauncherInvoked { .. }));
    let expected_output = format!(
        "{}\n{}\ntwo words\n",
        owner_projects.join("remote-only").display(),
        exact_argument,
    );
    wait_until(
        || fs::read_to_string(&output).is_ok_and(|value| value == expected_output),
        "owner-side launcher did not produce its output",
    );
    assert_eq!(fs::read_to_string(&output).unwrap(), expected_output);
    assert!(!root.join("local-pwned").exists());

    let session_workspace = owner
        .client
        .create_workspace(
            "remote-session",
            vec![ShellSpec {
                name: "pi".into(),
                command: vec!["/bin/sh".into(), "-c".into(), "sleep 30".into()],
                cwd: owner_projects.join("remote-only"),
            }],
        )
        .unwrap();
    let shell_id = session_workspace.shells[0].id.clone();
    let owner_attachment = owner.client.attach(&shell_id, false, profile()).unwrap();
    let run_id = owner.client.get_shell(&shell_id).unwrap().run.unwrap().id;
    let hostile_session_id = "pi-exact; touch local-session-pwned";
    let agent = owner
        .client
        .ensure_agent(
            &shell_id,
            &run_id,
            AgentRegistrationSpec {
                name: "owner pi".into(),
                integration: "pi".into(),
                external_session_id: Some(hostile_session_id.into()),
                report: AgentReport {
                    state: AgentState::Idle,
                    authority: AgentAuthority::LifecycleIntegration,
                    evidence: "owner catalog".into(),
                    confidence: 100,
                },
            },
        )
        .unwrap();
    let current_sessions = local
        .client
        .route_node_host_service(
            &owner_id,
            HostServiceOperation::ListAgentSessions {
                workspace_id: Some(session_workspace.id.clone()),
            },
        )
        .unwrap();
    let HostServiceResult::AgentSessions {
        sessions: current_sessions,
    } = current_sessions
    else {
        panic!("unexpected current Agent Session response");
    };
    assert_eq!(current_sessions.len(), 1);
    assert!(current_sessions[0].state_is_current);
    let current_inspection = local
        .client
        .route_node_host_service(
            &owner_id,
            HostServiceOperation::InspectAgentSession {
                session_id: current_sessions[0].id.clone(),
            },
        )
        .unwrap();
    let HostServiceResult::AgentSession {
        session: current_inspection,
    } = current_inspection
    else {
        panic!("unexpected current Agent Session inspection response");
    };
    assert_eq!(current_inspection.occurrences.len(), 1);
    assert_eq!(current_inspection.occurrences[0].shell_id, shell_id);
    assert_eq!(current_inspection.occurrences[0].run_id, run_id);
    assert!(local.client.snapshot().unwrap().workspaces.is_empty());
    owner
        .client
        .report_agent(
            &agent.id,
            &run_id,
            AgentReport {
                state: AgentState::Inactive,
                authority: AgentAuthority::LifecycleIntegration,
                evidence: "owner shutdown".into(),
                confidence: 100,
            },
        )
        .unwrap();
    let sessions = local
        .client
        .route_node_host_service(
            &owner_id,
            HostServiceOperation::ListAgentSessions {
                workspace_id: Some(session_workspace.id.clone()),
            },
        )
        .unwrap();
    let HostServiceResult::AgentSessions { sessions } = sessions else {
        panic!("unexpected Agent Session response");
    };
    assert_eq!(sessions.len(), 1);
    let mut resumed = local
        .client
        .resume_agent_session(Some(&owner_id), &sessions[0].id, profile())
        .unwrap();
    let mut terminal_output = Vec::new();
    loop {
        match AttachFrame::read_from(&mut resumed.stream).unwrap() {
            AttachFrame::Output(bytes) => terminal_output.extend(bytes),
            AttachFrame::Detached => break,
            _ => {}
        }
    }
    assert!(String::from_utf8_lossy(&terminal_output).contains("owner resume"));
    assert_eq!(
        fs::read_to_string(&resume_output).unwrap(),
        format!(
            "--session\n{}\n{}\n",
            hostile_session_id,
            owner_projects.join("remote-only").display(),
        )
    );
    assert!(!root.join("local-session-pwned").exists());
    assert!(local.client.snapshot().unwrap().workspaces.is_empty());
    drop(owner_attachment);

    let preview = local
        .client
        .route_node_host_service(
            &owner_id,
            HostServiceOperation::PreviewIntegrationMutation {
                action: HostServiceIntegrationAction::Install,
                integrations: vec!["pi".into()],
                force: false,
            },
        )
        .unwrap();
    let HostServiceResult::IntegrationMutationPreview { preview } = preview else {
        panic!("unexpected integration preview response");
    };
    let pi_asset = root.join("owner-home/.pi/agent/extensions/boomux.js");
    fs::create_dir_all(pi_asset.parent().unwrap()).unwrap();
    fs::write(&pi_asset, "owner customization").unwrap();
    assert!(
        local
            .client
            .route_node_host_service(
                &owner_id,
                HostServiceOperation::CommitIntegrationMutation {
                    preview_token: preview.token,
                },
            )
            .is_err()
    );
    assert_eq!(fs::read_to_string(pi_asset).unwrap(), "owner customization");

    let unsupported = boomux::protocol::Envelope::with_version(
        35,
        boomux::protocol::Request::HostService {
            operation: HostServiceOperation::DiscoverProjects,
        },
    );
    assert_eq!(
        unsupported
            .message
            .required_feature()
            .unwrap()
            .minimum_version(),
        36
    );

    local.stop_with_cli();
    owner.stop_with_cli();
    std::thread::sleep(Duration::from_millis(10));
    fs::remove_dir_all(root).unwrap();
}

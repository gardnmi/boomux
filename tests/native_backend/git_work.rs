use crate::support::{TestDaemon, profile, wait_until};
use boomux::{
    git_work::Overview,
    protocol::{self, ShellSpec},
};
use std::{fs, process::Command, time::Duration};

#[test]
fn git_work_overview_tracks_live_cwd_deduplicates_shells_and_preserves_launch_directory() {
    let mut daemon = TestDaemon::start();
    let repo = daemon.runtime_dir.join("repo");
    assert!(
        Command::new("git")
            .args(["init", "-q", "-b", "main"])
            .arg(&repo)
            .status()
            .unwrap()
            .success()
    );
    assert!(
        Command::new("git")
            .arg("-C")
            .arg(&repo)
            .args([
                "-c",
                "user.name=Test",
                "-c",
                "user.email=test@example.com",
                "commit",
                "--allow-empty",
                "-qm",
                "initial"
            ])
            .status()
            .unwrap()
            .success()
    );
    let linked = daemon.runtime_dir.join("linked");
    assert!(
        Command::new("git")
            .arg("-C")
            .arg(&repo)
            .args(["worktree", "add", "-qb", "feature"])
            .arg(&linked)
            .status()
            .unwrap()
            .success()
    );
    fs::write(linked.join("unfinished"), "work").unwrap();
    let workspace = daemon
        .client
        .create_workspace(
            "Git work",
            vec![
                ShellSpec {
                    name: "live".into(),
                    cwd: repo.clone(),
                    command: vec![
                        "/bin/sh".into(),
                        "-c".into(),
                        "cd \"$1\"; printf ready; read answer".into(),
                        "git-fixture".into(),
                        linked.display().to_string(),
                    ],
                },
                ShellSpec {
                    name: "second".into(),
                    cwd: linked.clone(),
                    command: vec![],
                },
            ],
        )
        .unwrap();
    let attached = daemon
        .client
        .attach(&workspace.shells[0].id, false, profile())
        .unwrap();
    let run_id = daemon
        .client
        .get_shell(&workspace.shells[0].id)
        .unwrap()
        .run
        .unwrap()
        .id;
    let agent = daemon
        .client
        .ensure_agent(
            &workspace.shells[0].id,
            &run_id,
            protocol::AgentRegistrationSpec {
                name: "fixture agent".into(),
                integration: "codex".into(),
                external_session_id: Some("git-panel-fixture".into()),
                report: protocol::AgentReport {
                    state: protocol::AgentState::Working,
                    authority: protocol::AgentAuthority::LifecycleIntegration,
                    evidence: "fixture".into(),
                    confidence: 100,
                },
            },
        )
        .unwrap();
    daemon
        .client
        .observe_agent_working_context(&agent.id, &workspace.shells[0].id, &run_id, repo.clone())
        .unwrap();
    let mut observed = Overview::default();
    wait_until(
        || {
            observed = daemon
                .client
                .git_overview(None, true, Duration::from_secs(2))
                .unwrap();
            observed.worktrees.iter().any(|row| {
                row.root == linked && row.shells.len() == 2 && row.shells.iter().any(|s| s.live_cwd)
            })
        },
        "Git overview did not observe the Shell's live worktree",
    );
    assert_eq!(observed.worktrees.len(), 2);
    let row = observed
        .worktrees
        .iter()
        .find(|r| r.root == linked)
        .unwrap();
    assert_eq!(row.branch, "feature");
    assert!(
        row.agents
            .iter()
            .any(|a| a.id == agent.id && !a.observed_context)
    );
    assert!(
        observed
            .worktrees
            .iter()
            .find(|r| r.root == repo)
            .unwrap()
            .agents
            .iter()
            .any(|a| a.id == agent.id && a.observed_context)
    );

    assert_eq!(row.status.as_ref().unwrap().untracked, 1);
    assert_eq!(
        daemon
            .client
            .get_shell(&workspace.shells[0].id)
            .unwrap()
            .cwd,
        repo
    );
    // No implicit start or lifecycle mutation for the second Shell.
    assert_eq!(
        daemon
            .client
            .get_shell(&workspace.shells[1].id)
            .unwrap()
            .status,
        protocol::ShellStatus::Pending
    );
    drop(attached);
    daemon.stop_with_cli();
}

#[test]
fn git_work_overview_rejects_old_wire_version_before_inspection() {
    use protocol::{Envelope, ErrorCode, HostServiceOperation, Request, Response};
    use std::os::unix::net::UnixStream;
    let mut daemon = TestDaemon::start();
    let mut stream = UnixStream::connect(daemon.client.socket_path()).unwrap();
    protocol::write_message(
        &mut stream,
        &Envelope::with_version(
            52,
            Request::HostService {
                operation: HostServiceOperation::GitOverview { refresh: true },
            },
        ),
    )
    .unwrap();
    let response: Envelope<Response> = protocol::read_message(&mut stream).unwrap();
    assert!(matches!(
        response.message,
        Response::Error {
            code: Some(ErrorCode::UnsupportedVersion),
            ..
        }
    ));
    daemon.stop_with_cli();
}

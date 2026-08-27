use std::collections::BTreeSet;
use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};

use crate::support::{TestDaemon, assert_generated_name, wait_until};
use uuid::Uuid;

#[test]
fn atomic_workspace_create_autostarts_and_returns_exact_local_identity() {
    let mut daemon = TestDaemon::start();
    let node_id = daemon.client.node_identity().unwrap();
    let project = daemon.runtime_dir.join("project");
    let project_alias = daemon.runtime_dir.join("project-alias");
    fs::create_dir(&project).unwrap();
    symlink(&project, &project_alias).unwrap();
    daemon.stop_with_cli();

    let output = daemon
        .command()
        .args(["workspace", "create", "--node", &node_id, "--cwd"])
        .arg(&project_alias)
        .arg("--json")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "atomic create failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let output: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(output["schema"], "boomux.cli/v1");
    assert_eq!(output["command"], "workspace.create");
    let data = output["data"].as_object().unwrap();
    assert_eq!(
        data.keys().map(String::as_str).collect::<BTreeSet<_>>(),
        BTreeSet::from(["placement", "presentation_warning", "shell", "workspace"])
    );
    assert!(output["data"]["presentation_warning"].is_null());
    for (field, expected) in [
        ("workspace", ["id", "name", "revision"].as_slice()),
        (
            "placement",
            ["default_cwd", "node_id", "owner_workspace_id"].as_slice(),
        ),
        ("shell", ["cwd", "id", "name", "node_id"].as_slice()),
    ] {
        let keys = data[field]
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect::<BTreeSet<_>>();
        assert_eq!(keys, expected.iter().copied().collect());
    }
    assert_generated_name(output["data"]["workspace"]["name"].as_str().unwrap());
    assert_generated_name(output["data"]["shell"]["name"].as_str().unwrap());
    assert_eq!(output["data"]["placement"]["node_id"], node_id);
    assert_eq!(output["data"]["shell"]["node_id"], node_id);
    assert_eq!(
        output["data"]["placement"]["default_cwd"],
        project.display().to_string()
    );
    assert_eq!(
        output["data"]["shell"]["cwd"],
        project.display().to_string()
    );

    let global_id = output["data"]["workspace"]["id"].as_str().unwrap();
    let owner_id = output["data"]["placement"]["owner_workspace_id"]
        .as_str()
        .unwrap();
    let shell_id = output["data"]["shell"]["id"].as_str().unwrap();
    let combined = daemon.client.combined_node_snapshot(None).unwrap();
    let workspace = combined
        .workspaces
        .iter()
        .find(|workspace| workspace.id == global_id)
        .unwrap();
    assert_eq!(workspace.name, output["data"]["workspace"]["name"]);
    assert_eq!(workspace.revision, output["data"]["workspace"]["revision"]);
    assert_eq!(workspace.placements.len(), 1);
    assert_eq!(workspace.placements[0].node_id, node_id);
    assert_eq!(workspace.placements[0].workspace_id, owner_id);
    assert_eq!(
        workspace.placements[0].default_cwd.as_deref(),
        Some(project.as_path())
    );
    let owner = daemon.client.get_workspace(owner_id).unwrap();
    assert_eq!(owner.default_cwd.as_deref(), Some(project.as_path()));
    assert_eq!(owner.shells.len(), 1);
    assert_eq!(owner.shells[0].id, shell_id);
    assert_eq!(owner.shells[0].cwd, project);

    let empty = daemon
        .command()
        .args(["workspace", "create", "still-empty"])
        .output()
        .unwrap();
    assert!(empty.status.success());
    let combined = daemon.client.combined_node_snapshot(None).unwrap();
    let empty = combined
        .workspaces
        .iter()
        .find(|workspace| workspace.name == "still-empty")
        .unwrap();
    assert!(empty.placements.is_empty());
}

#[test]
fn atomic_workspace_create_open_releases_the_exact_shell_after_commit() {
    let daemon = TestDaemon::start();
    let node_id = daemon.client.node_identity().unwrap();
    let bin = daemon.runtime_dir.join("bin");
    fs::create_dir(&bin).unwrap();
    let resolver = bin.join("xdg-terminal-exec");
    fs::write(
        &resolver,
        "#!/bin/sh\nwhile [ \"$#\" -gt 0 ] && [ \"$1\" != -- ]; do shift; done\nshift\ngate=\nfor arg do if [ \"$previous\" = --gate ]; then gate=$arg; break; fi; previous=$arg; done\nprintf '%s\\0' python3 -c 'import socket,sys; s=socket.socket(socket.AF_UNIX); s.connect(sys.argv[1]); status=s.recv(1); open(sys.argv[2], \"w\").write(sys.argv[3]+\"\\n\"+str(status[0]))' \"$gate\" \"$BOOMUX_GATE_MARKER\" \"$3\"\n",
    )
    .unwrap();
    fs::set_permissions(&resolver, fs::Permissions::from_mode(0o700)).unwrap();
    let marker = daemon.runtime_dir.join("atomic-gate");
    let output = daemon
        .command()
        .env("PATH", format!("{}:/usr/bin:/bin", bin.display()))
        .env("BOOMUX_GATE_MARKER", &marker)
        .args([
            "workspace",
            "create",
            "gated-atomic",
            "--node",
            &node_id,
            "--cwd",
            "/tmp",
            "--open",
            "--json",
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "atomic create and open failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let output: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    wait_until(
        || fs::read_to_string(&marker).is_ok_and(|value| value.ends_with("\n1")),
        "terminal did not observe the committed creation gate",
    );
    let marker = fs::read_to_string(marker).unwrap();
    let (opened_shell, status) = marker.split_once('\n').unwrap();
    assert_eq!(status, "1");
    assert_eq!(opened_shell, output["data"]["shell"]["id"]);
    assert!(output["data"]["presentation_warning"].is_null());
    let owner_id = output["data"]["placement"]["owner_workspace_id"]
        .as_str()
        .unwrap();
    assert!(
        daemon
            .client
            .get_workspace(owner_id)
            .unwrap()
            .shells
            .iter()
            .any(|shell| shell.id == opened_shell),
        "the gate opened before the exact Shell was committed"
    );
}

#[test]
fn atomic_workspace_create_reports_post_commit_gate_failure_as_a_warning() {
    let daemon = TestDaemon::start();
    let node_id = daemon.client.node_identity().unwrap();
    let bin = daemon.runtime_dir.join("warning-bin");
    fs::create_dir(&bin).unwrap();
    let resolver = bin.join("xdg-terminal-exec");
    fs::write(
        &resolver,
        "#!/bin/sh\nwhile [ \"$#\" -gt 0 ] && [ \"$1\" != -- ]; do shift; done\nshift\ngate=\nfor arg do if [ \"$previous\" = --gate ]; then gate=$arg; break; fi; previous=$arg; done\nprintf '%s\\0' python3 -c 'import socket,struct,sys; s=socket.socket(socket.AF_UNIX); s.connect(sys.argv[1]); s.setsockopt(socket.SOL_SOCKET, socket.SO_LINGER, struct.pack(\"ii\", 1, 0)); s.close()' \"$gate\"\n",
    )
    .unwrap();
    fs::set_permissions(&resolver, fs::Permissions::from_mode(0o700)).unwrap();
    let output = daemon
        .command()
        .env("PATH", format!("{}:/usr/bin:/bin", bin.display()))
        .args([
            "workspace",
            "create",
            "warning-atomic",
            "--node",
            &node_id,
            "--cwd",
            "/tmp",
            "--open",
            "--json",
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "post-commit gate failure was reported as mutation failure: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let output: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(output["command"], "workspace.create");
    assert!(
        output["data"]["presentation_warning"].as_str().is_some_and(
            |warning| warning.contains("was created but its terminal could not attach")
        )
    );
    let owner_id = output["data"]["placement"]["owner_workspace_id"]
        .as_str()
        .unwrap();
    let shell_id = output["data"]["shell"]["id"].as_str().unwrap();
    assert!(
        daemon
            .client
            .get_workspace(owner_id)
            .unwrap()
            .shells
            .iter()
            .any(|shell| shell.id == shell_id)
    );
}

#[test]
fn workspace_default_cwd_mutation_changes_only_future_omitted_shells() {
    let mut daemon = TestDaemon::start();
    let node_id = daemon.client.node_identity().unwrap();
    let initial = daemon.runtime_dir.join("initial");
    let updated = daemon.runtime_dir.join("updated");
    let updated_alias = daemon.runtime_dir.join("updated-alias");
    let explicit = daemon.runtime_dir.join("explicit");
    fs::create_dir(&initial).unwrap();
    fs::create_dir(&updated).unwrap();
    fs::create_dir(&explicit).unwrap();
    symlink(&updated, &updated_alias).unwrap();
    let created = daemon
        .command()
        .args([
            "workspace",
            "create",
            "cwd-work",
            "--node",
            &node_id,
            "--cwd",
        ])
        .arg(&initial)
        .arg("--json")
        .output()
        .unwrap();
    assert!(
        created.status.success(),
        "{}",
        String::from_utf8_lossy(&created.stderr)
    );
    let created: serde_json::Value = serde_json::from_slice(&created.stdout).unwrap();
    let global_id = created["data"]["workspace"]["id"].as_str().unwrap();
    let owner_id = created["data"]["placement"]["owner_workspace_id"]
        .as_str()
        .unwrap();
    let existing_shell_id = created["data"]["shell"]["id"].as_str().unwrap();
    let before = daemon.client.combined_node_snapshot(None).unwrap();
    let before = before
        .workspaces
        .iter()
        .find(|workspace| workspace.id == global_id)
        .unwrap();
    let before_global_revision = before.revision;
    let before_owner_revision = before.placements[0].owner_revision;

    let changed = daemon
        .command()
        .args([
            "workspace",
            "set-default-cwd",
            global_id,
            "--node",
            &node_id,
            "--cwd",
        ])
        .arg(&updated_alias)
        .arg("--json")
        .output()
        .unwrap();
    assert!(
        changed.status.success(),
        "{}",
        String::from_utf8_lossy(&changed.stderr)
    );
    let changed: serde_json::Value = serde_json::from_slice(&changed.stdout).unwrap();
    assert_eq!(changed["schema"], "boomux.cli/v1");
    assert_eq!(changed["command"], "workspace.set-default-cwd");
    assert_eq!(
        changed["data"]
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect::<BTreeSet<_>>(),
        BTreeSet::from([
            "default_cwd",
            "global_revision",
            "node_id",
            "owner_revision",
            "owner_workspace_id",
            "result",
            "workspace_id",
        ])
    );
    assert_eq!(changed["data"]["workspace_id"], global_id);
    assert_eq!(changed["data"]["node_id"], node_id);
    assert_eq!(changed["data"]["owner_workspace_id"], owner_id);
    assert_eq!(
        changed["data"]["default_cwd"],
        updated.display().to_string()
    );
    assert_eq!(
        changed["data"]["global_revision"],
        before_global_revision + 1
    );
    assert_eq!(changed["data"]["owner_revision"], before_owner_revision + 1);
    assert_eq!(changed["data"]["result"], "updated");
    let owner = daemon.client.get_workspace(owner_id).unwrap();
    assert_eq!(owner.default_cwd.as_deref(), Some(updated.as_path()));
    assert_eq!(
        owner
            .shells
            .iter()
            .find(|shell| shell.id == existing_shell_id)
            .unwrap()
            .cwd,
        initial
    );

    let unchanged = daemon
        .command()
        .args([
            "workspace",
            "set-default-cwd",
            global_id,
            "--node",
            &node_id,
            "--cwd",
        ])
        .arg(&updated)
        .arg("--json")
        .output()
        .unwrap();
    assert!(unchanged.status.success());
    let unchanged: serde_json::Value = serde_json::from_slice(&unchanged.stdout).unwrap();
    assert_eq!(unchanged["data"]["result"], "unchanged");
    assert_eq!(
        unchanged["data"]["global_revision"],
        before_global_revision + 1
    );
    assert_eq!(
        unchanged["data"]["owner_revision"],
        before_owner_revision + 1
    );

    let inherited = daemon
        .command()
        .args([
            "shell",
            "create",
            global_id,
            "--node",
            &node_id,
            "--name",
            "inherited",
        ])
        .output()
        .unwrap();
    assert!(
        inherited.status.success(),
        "{}",
        String::from_utf8_lossy(&inherited.stderr)
    );
    let explicit_shell = daemon
        .command()
        .args([
            "shell", "create", global_id, "--node", &node_id, "--name", "explicit", "--cwd",
        ])
        .arg(&explicit)
        .output()
        .unwrap();
    assert!(
        explicit_shell.status.success(),
        "{}",
        String::from_utf8_lossy(&explicit_shell.stderr)
    );
    let owner = daemon.client.get_workspace(owner_id).unwrap();
    assert_eq!(
        owner
            .shells
            .iter()
            .find(|shell| shell.name == "inherited")
            .unwrap()
            .cwd,
        updated
    );
    assert_eq!(
        owner
            .shells
            .iter()
            .find(|shell| shell.name == "explicit")
            .unwrap()
            .cwd,
        explicit
    );

    daemon.stop_with_cli();
    daemon.restart();
    let owner = daemon.client.get_workspace(owner_id).unwrap();
    assert_eq!(owner.default_cwd.as_deref(), Some(updated.as_path()));
}

#[test]
fn prepared_default_cwd_operation_completes_from_owner_state_after_cold_restart() {
    let mut daemon = TestDaemon::start();
    let node_id = daemon.client.node_identity().unwrap();
    let initial = daemon.runtime_dir.join("cold-initial");
    let updated = daemon.runtime_dir.join("cold-updated");
    fs::create_dir(&initial).unwrap();
    fs::create_dir(&updated).unwrap();
    let created = daemon
        .command()
        .args([
            "workspace",
            "create",
            "cold-cwd",
            "--node",
            &node_id,
            "--cwd",
        ])
        .arg(&initial)
        .arg("--json")
        .output()
        .unwrap();
    assert!(created.status.success());
    let created: serde_json::Value = serde_json::from_slice(&created.stdout).unwrap();
    let global_id = created["data"]["workspace"]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let owner_id = created["data"]["placement"]["owner_workspace_id"]
        .as_str()
        .unwrap()
        .to_owned();
    let combined = daemon.client.combined_node_snapshot(None).unwrap();
    let workspace = combined
        .workspaces
        .iter()
        .find(|workspace| workspace.id == global_id)
        .unwrap();
    let global_revision = workspace.revision;
    let owner_revision = workspace.placements[0].owner_revision;
    daemon.stop_with_cli();

    let owner_path = daemon.runtime_dir.join("state/boomux/state.json");
    let mut owner_state: serde_json::Value =
        serde_json::from_slice(&fs::read(&owner_path).unwrap()).unwrap();
    let owner = owner_state["workspaces"]
        .as_array_mut()
        .unwrap()
        .iter_mut()
        .find(|workspace| workspace["id"] == owner_id)
        .unwrap();
    owner["revision"] = (owner_revision + 1).into();
    owner["default_cwd"] = updated.display().to_string().into();
    fs::write(
        &owner_path,
        serde_json::to_vec_pretty(&owner_state).unwrap(),
    )
    .unwrap();

    let coordinator_path = daemon
        .runtime_dir
        .join("state/boomux/global_workspaces.json");
    let mut coordinator: serde_json::Value =
        serde_json::from_slice(&fs::read(&coordinator_path).unwrap()).unwrap();
    coordinator["pending_default_cwd_operations"] = serde_json::json!([{
        "operation_id": Uuid::new_v4().to_string(),
        "global_workspace_id": global_id,
        "expected_global_revision": global_revision,
        "node_id": node_id,
        "owner_workspace_id": owner_id,
        "expected_owner_revision": owner_revision,
        "requested_default_cwd": updated,
        "default_cwd": updated,
        "owner_attempted": true,
        "completion_reservation": " ".repeat(8 * 1024),
    }]);
    fs::write(
        &coordinator_path,
        serde_json::to_vec_pretty(&coordinator).unwrap(),
    )
    .unwrap();

    daemon.restart();
    let combined = daemon.client.combined_node_snapshot(None).unwrap();
    let workspace = combined
        .workspaces
        .iter()
        .find(|workspace| workspace.id == global_id)
        .unwrap();
    assert_eq!(workspace.revision, global_revision + 1);
    assert_eq!(workspace.placements[0].owner_revision, owner_revision + 1);
    assert_eq!(
        workspace.placements[0].default_cwd.as_deref(),
        Some(updated.as_path())
    );
    let persisted: serde_json::Value =
        serde_json::from_slice(&fs::read(coordinator_path).unwrap()).unwrap();
    assert!(
        persisted["pending_default_cwd_operations"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        persisted["completed_default_cwd_operations"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
}

#[test]
fn never_attempted_default_cwd_operation_is_cancelled_after_cold_restart() {
    let mut daemon = TestDaemon::start();
    let node_id = daemon.client.node_identity().unwrap();
    let created = daemon
        .command()
        .args([
            "workspace",
            "create",
            "cold-unattempted-cwd",
            "--node",
            &node_id,
            "--cwd",
            "/tmp",
            "--json",
        ])
        .output()
        .unwrap();
    assert!(created.status.success());
    let created: serde_json::Value = serde_json::from_slice(&created.stdout).unwrap();
    let global_id = created["data"]["workspace"]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let owner_id = created["data"]["placement"]["owner_workspace_id"]
        .as_str()
        .unwrap()
        .to_owned();
    let combined = daemon.client.combined_node_snapshot(None).unwrap();
    let workspace = combined
        .workspaces
        .iter()
        .find(|workspace| workspace.id == global_id)
        .unwrap();
    let global_revision = workspace.revision;
    let owner_revision = workspace.placements[0].owner_revision;
    daemon.stop_with_cli();

    let coordinator_path = daemon
        .runtime_dir
        .join("state/boomux/global_workspaces.json");
    let mut coordinator: serde_json::Value =
        serde_json::from_slice(&fs::read(&coordinator_path).unwrap()).unwrap();
    coordinator["pending_default_cwd_operations"] = serde_json::json!([{
        "operation_id": Uuid::new_v4().to_string(),
        "global_workspace_id": global_id,
        "expected_global_revision": global_revision,
        "node_id": node_id,
        "owner_workspace_id": owner_id,
        "expected_owner_revision": owner_revision,
        "requested_default_cwd": "/tmp",
        "default_cwd": "/tmp",
        "owner_attempted": false,
        "completion_reservation": " ".repeat(8 * 1024),
    }]);
    fs::write(
        &coordinator_path,
        serde_json::to_vec_pretty(&coordinator).unwrap(),
    )
    .unwrap();

    daemon.restart();
    daemon.client.combined_node_snapshot(None).unwrap();
    let persisted: serde_json::Value =
        serde_json::from_slice(&fs::read(coordinator_path).unwrap()).unwrap();
    assert!(
        persisted["pending_default_cwd_operations"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    let renamed = daemon
        .command()
        .args(["workspace", "rename", &global_id, "after-recovery"])
        .output()
        .unwrap();
    assert!(
        renamed.status.success(),
        "cancelled prepare blocked later Workspace mutation: {}",
        String::from_utf8_lossy(&renamed.stderr)
    );
}

#[test]
fn definitive_default_cwd_preflight_failure_leaves_no_pending_operation() {
    let daemon = TestDaemon::start();
    let node_id = daemon.client.node_identity().unwrap();
    let created = daemon
        .command()
        .args([
            "workspace",
            "create",
            "preflight-cwd",
            "--node",
            &node_id,
            "--cwd",
            "/tmp",
            "--json",
        ])
        .output()
        .unwrap();
    assert!(created.status.success());
    let created: serde_json::Value = serde_json::from_slice(&created.stdout).unwrap();
    let global_id = created["data"]["workspace"]["id"].as_str().unwrap();
    let missing = daemon.runtime_dir.join("does-not-exist");
    let failed = daemon
        .command()
        .args([
            "workspace",
            "set-default-cwd",
            global_id,
            "--node",
            &node_id,
            "--cwd",
        ])
        .arg(missing)
        .arg("--json")
        .output()
        .unwrap();
    assert!(!failed.status.success());

    let coordinator_path = daemon
        .runtime_dir
        .join("state/boomux/global_workspaces.json");
    let persisted: serde_json::Value =
        serde_json::from_slice(&fs::read(coordinator_path).unwrap()).unwrap();
    assert!(
        persisted["pending_default_cwd_operations"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    let renamed = daemon
        .command()
        .args(["workspace", "rename", global_id, "after-preflight"])
        .output()
        .unwrap();
    assert!(
        renamed.status.success(),
        "definitive preflight blocked later Workspace mutation: {}",
        String::from_utf8_lossy(&renamed.stderr)
    );
}

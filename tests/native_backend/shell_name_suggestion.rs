use std::collections::BTreeSet;
use std::fs;
use std::os::unix::fs::PermissionsExt;

use boomux::protocol::ShellSpec;

use crate::support::{TestDaemon, assert_generated_name, wait_until};

#[test]
fn create_and_open_releases_terminal_only_after_durable_creation() {
    let daemon = TestDaemon::start();
    daemon.client.create_global_workspace("gated-open").unwrap();
    let bin = daemon.runtime_dir.join("bin");
    fs::create_dir(&bin).unwrap();
    let resolver = bin.join("xdg-terminal-exec");
    fs::write(
        &resolver,
        "#!/bin/sh\nwhile [ \"$#\" -gt 0 ] && [ \"$1\" != -- ]; do shift; done\nshift\ngate=\nwhile [ \"$#\" -gt 0 ]; do\n  if [ \"$1\" = --gate ]; then shift; gate=$1; break; fi\n  shift\ndone\nprintf '%s\\0' python3 -c 'import socket,sys; s=socket.socket(socket.AF_UNIX); s.connect(sys.argv[1]); status=s.recv(1); open(sys.argv[2], \"wb\").write(status)' \"$gate\" \"$BOOMUX_GATE_MARKER\"\n",
    )
    .unwrap();
    fs::set_permissions(&resolver, fs::Permissions::from_mode(0o700)).unwrap();
    let marker = daemon.runtime_dir.join("gate-status");
    let output = daemon
        .command()
        .env("PATH", format!("{}:/usr/bin:/bin", bin.display()))
        .env("BOOMUX_GATE_MARKER", &marker)
        .args([
            "shell",
            "create",
            "gated-open",
            "--name",
            "gated",
            "--cwd",
            "/tmp",
            "--open",
            "--",
            "/bin/sh",
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "create and open failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    wait_until(
        || fs::read(&marker).is_ok_and(|status| status == [1]),
        "terminal did not observe the successful creation gate",
    );
    let snapshot = daemon.client.snapshot().unwrap();
    let shell = snapshot
        .workspaces
        .iter()
        .find(|workspace| workspace.name == "gated-open")
        .unwrap()
        .shells
        .iter()
        .find(|shell| shell.name == "gated")
        .unwrap();
    assert!(shell.run.is_none(), "the gate observer must not attach");

    fs::write(&resolver, "#!/bin/sh\nexit 64\n").unwrap();
    let output = daemon
        .command()
        .env("PATH", format!("{}:/usr/bin:/bin", bin.display()))
        .args([
            "shell",
            "create",
            "gated-open",
            "--name",
            "terminal-failed",
            "--open",
        ])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(
        daemon
            .client
            .snapshot()
            .unwrap()
            .workspaces
            .iter()
            .flat_map(|workspace| &workspace.shells)
            .all(|shell| shell.name != "terminal-failed"),
        "terminal preparation failure must happen before Shell creation"
    );

    let output = daemon
        .command()
        .args(["workspace", "close", "gated-open"])
        .output()
        .unwrap();
    assert!(output.status.success());
}

#[test]
fn shell_name_suggestion_is_stable_non_mutating_cli_data() {
    let daemon = TestDaemon::start();
    let workspace = daemon
        .client
        .create_workspace(
            "suggestions",
            vec![
                ShellSpec::login("agile-badger", std::env::temp_dir()),
                ShellSpec::login("quiet-otter", std::env::temp_dir()),
            ],
        )
        .unwrap();
    let before = workspace
        .shells
        .iter()
        .map(|shell| (shell.id.clone(), shell.name.clone()))
        .collect::<BTreeSet<_>>();

    let output = daemon
        .command()
        .args(["shell", "suggest-name", "suggestions", "--json"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "shell name suggestion failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["schema"], "boomux.cli/v1");
    assert_eq!(value["command"], "shell.suggest-name");
    assert_eq!(
        value["data"]["node_id"],
        daemon.client.node_identity().unwrap()
    );
    assert_eq!(value["data"]["workspace_id"], workspace.id);
    assert_eq!(value["data"].as_object().unwrap().len(), 3);
    let name = value["data"]["name"].as_str().unwrap();
    assert!(!name.is_empty());
    assert_generated_name(name);
    assert!(!before.iter().any(|(_, current)| current == name));

    let after = daemon
        .client
        .get_workspace(&workspace.id)
        .unwrap()
        .shells
        .into_iter()
        .map(|shell| (shell.id, shell.name))
        .collect::<BTreeSet<_>>();
    assert_eq!(after, before);

    let human = daemon
        .command()
        .args(["shell", "suggest-name", &workspace.id])
        .output()
        .unwrap();
    assert!(human.status.success());
    let human = String::from_utf8(human.stdout).unwrap();
    assert!(human.contains("Suggested shell name"));
    assert!(human.contains(&workspace.id));
    assert!(human.contains("not reserved"));
}

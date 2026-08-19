use std::fs;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::process::{Command, Stdio};

use boomux::protocol::{self, WorkspaceLauncherSpec};
use uuid::Uuid;

use crate::support::{TestDaemon, wait_until};

#[test]
fn workspace_launchers_persist_emit_events_and_open_without_shells() {
    let mut daemon = TestDaemon::start();
    let workspace = daemon
        .client
        .create_workspace("launcher-only", Vec::new())
        .unwrap();
    let cursor = daemon.client.events(None, 256, 0).unwrap().cursor;
    let output = daemon.runtime_dir.join("launcher-output");
    let first = daemon
        .client
        .create_launcher(
            &workspace.id,
            WorkspaceLauncherSpec {
                name: "editor".into(),
                cwd: daemon.runtime_dir.clone(),
                command: vec![
                    "/bin/sh".into(),
                    "-c".into(),
                    "printf '%s|%s|%s|%s|%s' \"$PWD\" \"$BOOMUX_WORKSPACE_ID\" \"$BOOMUX_WORKSPACE\" \"$BOOMUX_LAUNCHER_ID\" \"$BOOMUX_LAUNCHER_NAME\" > \"$1\"".into(),
                    "launcher".into(),
                    output.display().to_string(),
                ],
            },
        )
        .unwrap();
    let second = daemon
        .client
        .create_launcher(
            &workspace.id,
            WorkspaceLauncherSpec {
                name: "browser".into(),
                cwd: daemon.runtime_dir.clone(),
                command: vec!["/bin/true".into()],
            },
        )
        .unwrap();
    let snapshot = daemon.client.get_workspace(&workspace.id).unwrap();
    assert_eq!(
        snapshot
            .launchers
            .iter()
            .map(|launcher| launcher.name.as_str())
            .collect::<Vec<_>>(),
        ["editor", "browser"]
    );
    let events = daemon.client.events(Some(cursor.clone()), 256, 0).unwrap();
    assert_eq!(
        events
            .events
            .iter()
            .filter(|event| matches!(
                event.kind,
                protocol::DaemonEventKind::LauncherCreated { .. }
            ))
            .count(),
        2
    );

    let restart = daemon
        .command()
        .args(["daemon", "restart"])
        .output()
        .unwrap();
    assert!(restart.status.success());
    let restored = daemon.client.get_workspace(&workspace.id).unwrap();
    assert_eq!(restored.launchers[0].id, first.id);
    assert_eq!(restored.launchers[1].id, second.id);
    let listed = daemon
        .command()
        .args(["launcher", "list", "--workspace", &workspace.id, "--json"])
        .output()
        .unwrap();
    assert!(listed.status.success());
    let listed: serde_json::Value = serde_json::from_slice(&listed.stdout).unwrap();
    assert_eq!(listed["schema"], "boomux.cli/v1");
    assert_eq!(listed["command"], "launcher.list");
    assert_eq!(listed["data"]["launchers"][0]["id"], first.id);
    assert_eq!(
        listed["data"]["launchers"][0]["command"],
        serde_json::json!(first.command)
    );

    let opened = daemon
        .command()
        .args(["workspace", "open", &workspace.id])
        .output()
        .unwrap();
    assert!(
        opened.status.success(),
        "launcher-only workspace failed to open: {}",
        String::from_utf8_lossy(&opened.stderr)
    );
    wait_until(
        || fs::metadata(&output).is_ok_and(|metadata| metadata.len() > 0),
        "workspace launcher did not run",
    );
    assert_eq!(
        fs::read_to_string(&output).unwrap(),
        format!(
            "{}|{}|{}|{}|{}",
            daemon.runtime_dir.display(),
            workspace.id,
            workspace.name,
            first.id,
            first.name
        )
    );

    daemon.client.rename_launcher(&first.id, "zed").unwrap();
    assert_eq!(daemon.client.get_launcher(&first.id).unwrap().name, "zed");
    daemon.client.remove_launcher(&second.id).unwrap();
    assert_eq!(
        daemon
            .client
            .get_workspace(&workspace.id)
            .unwrap()
            .launchers
            .len(),
        1
    );
    let events = daemon.client.events(Some(cursor), 256, 0).unwrap();
    assert!(events.events.iter().any(|event| matches!(
        event.kind,
        protocol::DaemonEventKind::LauncherRenamed { .. }
    )));
    assert!(events.events.iter().any(|event| matches!(
        event.kind,
        protocol::DaemonEventKind::LauncherRemoved { .. }
    )));
    let restart = daemon
        .command()
        .args(["daemon", "restart"])
        .output()
        .unwrap();
    assert!(restart.status.success());
    assert_eq!(
        daemon
            .client
            .get_workspace(&workspace.id)
            .unwrap()
            .launchers
            .iter()
            .map(|launcher| launcher.name.as_str())
            .collect::<Vec<_>>(),
        ["zed"]
    );
    daemon.client.close_workspace(&workspace.id).unwrap();
    let restart = daemon
        .command()
        .args(["daemon", "restart"])
        .output()
        .unwrap();
    assert!(restart.status.success());
    assert!(
        daemon
            .client
            .snapshot()
            .unwrap()
            .workspaces
            .iter()
            .all(|current| current.id != workspace.id)
    );
    daemon.stop_with_cli();
}

#[test]
fn launcher_invoke_uses_tui_detached_process_semantics() {
    let mut daemon = TestDaemon::start();
    let workspace = daemon
        .client
        .create_workspace("launcher-cli", Vec::new())
        .unwrap();
    let output = daemon.runtime_dir.join("launcher-invoke-output");
    let launcher = daemon
        .client
        .create_launcher(
            &workspace.id,
            WorkspaceLauncherSpec {
                name: "capture".into(),
                cwd: daemon.runtime_dir.clone(),
                command: vec![
                    "/bin/sh".into(),
                    "-c".into(),
                    "printf '%s|%s|%s|%s|%s|%s|%s|%s' \"$PWD\" \"$BOOMUX_WORKSPACE_ID\" \"$BOOMUX_WORKSPACE\" \"$BOOMUX_LAUNCHER_ID\" \"$BOOMUX_LAUNCHER_NAME\" \"$BOOMUX_INVOKE_TEST\" \"${BOOMUX_SHELL_ID-unset}\" \"$2\" > \"$1\"".into(),
                    "launcher".into(),
                    output.display().to_string(),
                    "exact argument".into(),
                ],
            },
        )
        .unwrap();

    let invoked = daemon
        .command()
        .args(["launcher", "invoke", &launcher.id])
        .env("BOOMUX_INVOKE_TEST", "inherited")
        .env("BOOMUX_SHELL_ID", "stale-shell")
        .output()
        .unwrap();
    assert!(
        invoked.status.success(),
        "launcher invoke failed: {}",
        String::from_utf8_lossy(&invoked.stderr)
    );
    assert_eq!(
        String::from_utf8(invoked.stdout).unwrap(),
        "Launched capture from launcher-cli\n"
    );
    let expected = format!(
        "{}|{}|{}|{}|{}|inherited|unset|exact argument",
        daemon.runtime_dir.display(),
        workspace.id,
        workspace.name,
        launcher.id,
        launcher.name,
    );
    wait_until(
        || fs::read_to_string(&output).is_ok_and(|actual| actual == expected),
        "invoked launcher did not finish writing",
    );

    daemon.stop_with_cli();
}

#[test]
fn integration_management_reports_and_installs_bundled_hosts() {
    let root = std::env::temp_dir().join(format!(
        "boomux-integration-management-{}-{}",
        std::process::id(),
        Uuid::new_v4()
    ));
    let bin = root.join("bin");
    let config = root.join("config");
    let pi = root.join("pi");
    let runtime = root.join("runtime");
    fs::create_dir_all(&bin).unwrap();
    fs::create_dir_all(&runtime).unwrap();
    for (name, version) in [("opencode", "1.18.18"), ("pi", "0.84.1")] {
        let executable = bin.join(name);
        fs::write(
            &executable,
            format!("#!/bin/sh\nprintf '%s\\n' '{version}'\n"),
        )
        .unwrap();
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o755)).unwrap();
    }
    let command = || {
        let mut command = Command::new(env!("CARGO_BIN_EXE_boomux"));
        command
            .env("HOME", &root)
            .env("XDG_CONFIG_HOME", &config)
            .env("PI_CODING_AGENT_DIR", &pi)
            .env("XDG_RUNTIME_DIR", &runtime)
            .env("PATH", &bin);
        command
    };

    let listed = command()
        .args(["integration", "list", "--json"])
        .output()
        .unwrap();
    assert!(listed.status.success());
    let listed: serde_json::Value = serde_json::from_slice(&listed.stdout).unwrap();
    assert_eq!(listed["schema"], "boomux.cli/v1");
    assert_eq!(listed["command"], "integration.list");
    assert_eq!(listed["data"]["integrations"].as_array().unwrap().len(), 2);

    let missing = command()
        .args(["integration", "status", "--json"])
        .output()
        .unwrap();
    assert!(missing.status.success());
    let missing: serde_json::Value = serde_json::from_slice(&missing.stdout).unwrap();
    assert_eq!(missing["command"], "integration.status");
    for integration in missing["data"]["integrations"].as_array().unwrap() {
        assert_eq!(integration["host"]["compatibility"], "validated");
        assert_eq!(integration["asset"]["state"], "missing");
        assert_eq!(integration["runtime"]["state"], "not_observable");
        assert_eq!(integration["recommended_action"], "install");
    }
    assert!(fs::read_dir(&runtime).unwrap().next().is_none());

    fs::create_dir_all(pi.join("extensions")).unwrap();
    fs::write(pi.join("extensions/boomux.js"), "custom extension").unwrap();
    let preflight_refused = command()
        .args(["integration", "install", "--all", "--json"])
        .output()
        .unwrap();
    assert!(!preflight_refused.status.success());
    assert!(!config.join("opencode/plugins/boomux.js").exists());
    fs::remove_file(pi.join("extensions/boomux.js")).unwrap();

    let installed = command()
        .args(["integration", "install", "--all", "--json"])
        .output()
        .unwrap();
    assert!(
        installed.status.success(),
        "integration install failed: {}",
        String::from_utf8_lossy(&installed.stderr)
    );
    let installed: serde_json::Value = serde_json::from_slice(&installed.stdout).unwrap();
    assert_eq!(installed["command"], "integration.install");
    for integration in installed["data"]["integrations"].as_array().unwrap() {
        assert_eq!(integration["result"], "installed");
        assert_eq!(integration["restart_required"], true);
    }
    assert!(config.join("opencode/plugins/boomux.js").is_file());
    assert!(pi.join("extensions/boomux.js").is_file());

    for arguments in [["opencode", "install"], ["pi", "install"]] {
        let shortcut = command().args(arguments).output().unwrap();
        assert!(shortcut.status.success());
        assert!(String::from_utf8_lossy(&shortcut.stdout).contains("already installed"));
    }

    let current = command()
        .args(["integration", "status", "pi", "--json"])
        .output()
        .unwrap();
    let current: serde_json::Value = serde_json::from_slice(&current.stdout).unwrap();
    assert_eq!(
        current["data"]["integrations"][0]["asset"]["state"],
        "current"
    );

    fs::write(pi.join("extensions/boomux.js"), "custom extension").unwrap();
    let refused = command()
        .args(["integration", "install", "pi", "--json"])
        .output()
        .unwrap();
    assert!(!refused.status.success());
    let refused: serde_json::Value = serde_json::from_slice(&refused.stderr).unwrap();
    assert_eq!(refused["command"], "integration.install");
    assert_eq!(refused["error"]["code"], "already_exists");

    let uninstall_refused = command()
        .args(["integration", "uninstall", "--all", "--json"])
        .output()
        .unwrap();
    assert!(!uninstall_refused.status.success());
    assert!(config.join("opencode/plugins/boomux.js").is_file());
    assert_eq!(
        fs::read_to_string(pi.join("extensions/boomux.js")).unwrap(),
        "custom extension"
    );

    let uninstalled = command()
        .args(["integration", "uninstall", "--all", "--force", "--json"])
        .output()
        .unwrap();
    assert!(uninstalled.status.success());
    let uninstalled: serde_json::Value = serde_json::from_slice(&uninstalled.stdout).unwrap();
    assert_eq!(uninstalled["command"], "integration.uninstall");
    for integration in uninstalled["data"]["integrations"].as_array().unwrap() {
        assert_eq!(integration["result"], "removed");
        assert_eq!(integration["restart_required"], true);
    }
    assert!(!config.join("opencode/plugins/boomux.js").exists());
    assert!(!pi.join("extensions/boomux.js").exists());
    assert!(config.join("opencode/plugins").is_dir());
    assert!(pi.join("extensions").is_dir());

    let absent = command()
        .args(["integration", "uninstall", "pi", "--json"])
        .output()
        .unwrap();
    assert!(absent.status.success());
    let absent: serde_json::Value = serde_json::from_slice(&absent.stdout).unwrap();
    assert_eq!(absent["data"]["integrations"][0]["result"], "not_installed");
    assert_eq!(absent["data"]["integrations"][0]["restart_required"], false);

    let mut declined = command();
    declined
        .args(["integration", "setup", "pi"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped());
    let mut declined = declined.spawn().unwrap();
    declined.stdin.take().unwrap().write_all(b"n\n").unwrap();
    let declined = declined.wait_with_output().unwrap();
    assert!(declined.status.success());
    assert!(String::from_utf8_lossy(&declined.stdout).contains("No changes made."));
    assert!(!pi.join("extensions/boomux.js").exists());

    let setup = command()
        .args(["integration", "setup", "pi", "--yes"])
        .output()
        .unwrap();
    assert!(setup.status.success());
    let setup_output = String::from_utf8_lossy(&setup.stdout);
    assert!(setup_output.contains("Plan: install"));
    assert!(setup_output.contains("boomux integration verify pi"));
    assert!(pi.join("extensions/boomux.js").is_file());

    fs::write(pi.join("extensions/boomux.js"), "custom extension").unwrap();
    let setup_refused = command()
        .args(["integration", "setup", "pi", "--yes"])
        .output()
        .unwrap();
    assert!(!setup_refused.status.success());
    assert_eq!(
        fs::read_to_string(pi.join("extensions/boomux.js")).unwrap(),
        "custom extension"
    );

    let setup_replaced = command()
        .args(["integration", "setup", "pi", "--yes", "--force"])
        .output()
        .unwrap();
    assert!(setup_replaced.status.success());
    assert!(String::from_utf8_lossy(&setup_replaced.stdout).contains("Plan: replace"));
    assert_ne!(
        fs::read_to_string(pi.join("extensions/boomux.js")).unwrap(),
        "custom extension"
    );

    let invalid_environment = Command::new(env!("CARGO_BIN_EXE_boomux"))
        .args(["integration", "install", "opencode", "--json"])
        .env_remove("HOME")
        .env_remove("XDG_CONFIG_HOME")
        .output()
        .unwrap();
    assert!(!invalid_environment.status.success());
    let invalid_environment: serde_json::Value =
        serde_json::from_slice(&invalid_environment.stderr).unwrap();
    assert_eq!(invalid_environment["error"]["code"], "invalid_argument");

    fs::remove_dir_all(root).unwrap();
}

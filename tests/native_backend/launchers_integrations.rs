use std::fs;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::process::{Command, Stdio};

use boomux::protocol::{self, WorkspaceLauncherSpec};
use portable_pty::{CommandBuilder, PtySize, native_pty_system};
use uuid::Uuid;

use crate::support::{TestDaemon, wait_until};

#[test]
fn guided_setup_requires_an_interactive_terminal() {
    let output = Command::new(env!("CARGO_BIN_EXE_boomux"))
        .arg("setup")
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .contains("Boomux setup requires an interactive terminal")
    );
}

#[test]
fn guided_setup_discovers_and_installs_one_selected_harness() {
    let root = std::env::temp_dir().join(format!(
        "boomux-guided-setup-{}-{}",
        std::process::id(),
        Uuid::new_v4()
    ));
    let bin = root.join("bin");
    let config = root.join("config");
    fs::create_dir_all(&bin).unwrap();
    for (name, content) in [
        ("opencode", "#!/bin/sh\nprintf '1.18.18\\n'\n"),
        ("xdg-terminal-exec", "#!/bin/sh\nexit 0\n"),
    ] {
        let path = bin.join(name);
        fs::write(&path, content).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    }

    let pty = native_pty_system()
        .openpty(PtySize {
            rows: 24,
            cols: 120,
            pixel_width: 1200,
            pixel_height: 800,
        })
        .unwrap();
    let mut command = CommandBuilder::new(env!("CARGO_BIN_EXE_boomux"));
    command.arg("setup");
    command.env("HOME", &root);
    command.env("XDG_CONFIG_HOME", &config);
    command.env("PATH", &bin);
    let mut child = pty.slave.spawn_command(command).unwrap();
    drop(pty.slave);
    let mut reader = pty.master.try_clone_reader().unwrap();
    let mut writer = pty.master.take_writer().unwrap();
    writer.write_all(b"yes\nno\n").unwrap();
    drop(writer);
    assert!(child.wait().unwrap().success());
    let mut output = String::new();
    std::io::Read::read_to_string(reader.as_mut(), &mut output).unwrap();
    assert!(output.contains("OpenCode"));
    assert!(output.contains("host available | integration missing"));
    assert!(output.contains("integration installed"));
    assert!(output.contains("Omarchy"));
    assert!(output.contains("not detected"));
    assert!(
        config.join("opencode/plugins/boomux.js").is_file(),
        "{output}"
    );
    assert!(!root.join(".agents/skills/boomux/SKILL.md").exists());
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn guided_setup_installs_omarchy_plugin_and_managed_bindings() {
    let root = std::env::temp_dir().join(format!(
        "boomux-guided-desktop-{}-{}",
        std::process::id(),
        Uuid::new_v4()
    ));
    let bin = root.join("bin");
    let log = root.join("commands.log");
    fs::create_dir_all(&bin).unwrap();
    let omarchy = bin.join("omarchy");
    fs::write(
        &omarchy,
        "#!/bin/sh\nprintf 'omarchy %s\\n' \"$*\" >> \"$LOG\"\ncase \"$*\" in\n  version) printf 'Omarchy 4.0\\n' ;;\n  'plugin list --json') printf '[]\\n' ;;\n  'plugin add https://github.com/gardnmi/omarchy-boomux.git --enable --yes') ;;\n  'menu keybindings --print') printf 'SUPER + B  → Browser\\nSUPER CTRL + W  → Close\\n' ;;\n  *) exit 97 ;;\nesac\n",
    )
    .unwrap();
    fs::set_permissions(&omarchy, fs::Permissions::from_mode(0o755)).unwrap();
    let hyprctl = bin.join("hyprctl");
    fs::write(
        &hyprctl,
        "#!/bin/sh\nprintf 'hyprctl %s\\n' \"$*\" >> \"$LOG\"\ncase \"$1\" in reload|configerrors) ;; *) exit 98 ;; esac\n",
    )
    .unwrap();
    fs::set_permissions(&hyprctl, fs::Permissions::from_mode(0o755)).unwrap();
    let terminal = bin.join("xdg-terminal-exec");
    fs::write(&terminal, "#!/bin/sh\nexit 0\n").unwrap();
    fs::set_permissions(&terminal, fs::Permissions::from_mode(0o755)).unwrap();

    let pty = native_pty_system()
        .openpty(PtySize {
            rows: 24,
            cols: 120,
            pixel_width: 1200,
            pixel_height: 800,
        })
        .unwrap();
    let mut command = CommandBuilder::new(env!("CARGO_BIN_EXE_boomux"));
    command.arg("setup");
    command.env("HOME", &root);
    command.env("PATH", &bin);
    command.env("LOG", &log);
    command.env("HYPRLAND_INSTANCE_SIGNATURE", "test-instance");
    let mut child = pty.slave.spawn_command(command).unwrap();
    drop(pty.slave);
    let mut reader = pty.master.try_clone_reader().unwrap();
    let mut writer = pty.master.take_writer().unwrap();
    writer.write_all(b"yes\nyes\n").unwrap();
    drop(writer);
    assert!(child.wait().unwrap().success());
    let mut output = String::new();
    std::io::Read::read_to_string(reader.as_mut(), &mut output).unwrap();
    assert!(output.contains("conflicts require replacement"));
    assert!(output.contains("reloaded without errors"));
    let commands = fs::read_to_string(&log).unwrap();
    assert!(commands.contains(
        "omarchy plugin add https://github.com/gardnmi/omarchy-boomux.git --enable --yes\n"
    ));
    assert!(commands.contains("hyprctl reload\n"));
    assert!(commands.contains("hyprctl configerrors\n"));
    let bindings = fs::read_to_string(root.join(".config/hypr/bindings.lua")).unwrap();
    assert!(bindings.contains("BEGIN BOOMUX MANAGED KEYBINDINGS"));
    assert!(bindings.contains("boomux desktop gather"));
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn guided_setup_declined_bindings_do_not_create_hyprland_config() {
    let root = std::env::temp_dir().join(format!(
        "boomux-guided-desktop-decline-{}-{}",
        std::process::id(),
        Uuid::new_v4()
    ));
    let bin = root.join("bin");
    fs::create_dir_all(&bin).unwrap();
    let omarchy = bin.join("omarchy");
    fs::write(
        &omarchy,
        "#!/bin/sh\ncase \"$*\" in\n  version) printf 'Omarchy 4.0\\n' ;;\n  'plugin list --json') printf '[{\"id\":\"io.github.gardnmi.boomux\",\"enabled\":true}]\\n' ;;\n  'menu keybindings --print') printf '' ;;\n  *) exit 97 ;;\nesac\n",
    )
    .unwrap();
    fs::set_permissions(&omarchy, fs::Permissions::from_mode(0o755)).unwrap();
    let terminal = bin.join("xdg-terminal-exec");
    fs::write(&terminal, "#!/bin/sh\nexit 0\n").unwrap();
    fs::set_permissions(&terminal, fs::Permissions::from_mode(0o755)).unwrap();

    let pty = native_pty_system()
        .openpty(PtySize {
            rows: 24,
            cols: 120,
            pixel_width: 1200,
            pixel_height: 800,
        })
        .unwrap();
    let mut command = CommandBuilder::new(env!("CARGO_BIN_EXE_boomux"));
    command.arg("setup");
    command.env("HOME", &root);
    command.env("PATH", &bin);
    let mut child = pty.slave.spawn_command(command).unwrap();
    drop(pty.slave);
    let mut writer = pty.master.take_writer().unwrap();
    writer.write_all(b"no\n").unwrap();
    drop(writer);
    assert!(child.wait().unwrap().success());
    assert!(!root.join(".config/hypr").exists());
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn guided_setup_recognizes_compatible_user_managed_bindings() {
    let root = std::env::temp_dir().join(format!(
        "boomux-guided-desktop-existing-{}-{}",
        std::process::id(),
        Uuid::new_v4()
    ));
    let bin = root.join("bin");
    let hypr = root.join(".config/hypr");
    fs::create_dir_all(&bin).unwrap();
    fs::create_dir_all(&hypr).unwrap();
    let bindings = b"o.bind(\"SUPER + B\", \"Toggle Boomux panel\", \"omarchy-shell io.github.gardnmi.boomux toggle\")\n\
o.bind(\"SUPER + A\", \"Focus Boomux panel\", \"omarchy-shell io.github.gardnmi.boomux focus\")\n\
hl.exec_cmd(\"omarchy-shell io.github.gardnmi.boomux releaseFocus\")\n\
o.bind(\"SUPER + LEFT\", \"Focus on left window\", focus(\"l\"))\n\
o.bind(\"SUPER + RIGHT\", \"Focus on right window\", focus(\"r\"))\n\
o.bind(\"SUPER + UP\", \"Focus on above window\", focus(\"u\"))\n\
o.bind(\"SUPER + DOWN\", \"Focus on below window\", focus(\"d\"))\n\
o.bind(\"SUPER + TAB\", \"Next workspace\", \"boomux desktop next\")\n\
o.bind(\"SUPER + SHIFT + TAB\", \"Previous workspace\", \"boomux desktop previous\")\n\
o.bind(\"SUPER + RETURN\", \"Contextual terminal\", \"boomux desktop terminal\")\n\
o.bind(\"SUPER + O\", \"Pop window contextually\", \"boomux desktop pop\")\n\
o.bind(\"SUPER + ALT + B\", \"Return terminal\", \"boomux desktop return\")\n\
o.bind(\"SUPER + ALT + R\", \"Gather terminals\", \"boomux desktop gather\")\n\
o.bind(\"SUPER + CTRL + RETURN\", \"New Shell\", \"boomux shell create --open\")\n\
o.bind(\"SUPER + CTRL + W\", \"Close Shell\", \"boomux close --focused\")\n";
    fs::write(hypr.join("bindings.lua"), bindings).unwrap();
    let omarchy = bin.join("omarchy");
    fs::write(
        &omarchy,
        "#!/bin/sh\ncase \"$*\" in\n  version) printf 'Omarchy 4.0\\n' ;;\n  'plugin list --json') printf '[{\"id\":\"io.github.gardnmi.boomux\",\"enabled\":true}]\\n' ;;\n  'menu keybindings --print') printf 'SUPER + B  → Toggle Boomux panel\\nSUPER + RETURN  → Contextual terminal\\n' ;;\n  *) exit 97 ;;\nesac\n",
    )
    .unwrap();
    fs::set_permissions(&omarchy, fs::Permissions::from_mode(0o755)).unwrap();
    let terminal = bin.join("xdg-terminal-exec");
    fs::write(&terminal, "#!/bin/sh\nexit 0\n").unwrap();
    fs::set_permissions(&terminal, fs::Permissions::from_mode(0o755)).unwrap();

    let pty = native_pty_system()
        .openpty(PtySize {
            rows: 24,
            cols: 120,
            pixel_width: 1200,
            pixel_height: 800,
        })
        .unwrap();
    let mut command = CommandBuilder::new(env!("CARGO_BIN_EXE_boomux"));
    command.arg("setup");
    command.env("HOME", &root);
    command.env("PATH", &bin);
    command.env("NO_COLOR", "1");
    let mut child = pty.slave.spawn_command(command).unwrap();
    drop(pty.slave);
    let mut reader = pty.master.try_clone_reader().unwrap();
    let mut writer = pty.master.take_writer().unwrap();
    writer.write_all(b"no\n").unwrap();
    drop(writer);
    assert!(child.wait().unwrap().success());
    let mut output = String::new();
    std::io::Read::read_to_string(reader.as_mut(), &mut output).unwrap();
    assert!(
        output.contains("compatible user-managed profile"),
        "{output}"
    );
    assert!(output.contains("Reinstall impact"), "{output}");
    assert!(
        output.contains("Reinstall the standard Boomux keybinding profile?"),
        "{output}"
    );
    assert_eq!(fs::read(hypr.join("bindings.lua")).unwrap(), bindings);
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn guided_setup_restores_bindings_after_hyprland_validation_failure() {
    let root = std::env::temp_dir().join(format!(
        "boomux-guided-desktop-rollback-{}-{}",
        std::process::id(),
        Uuid::new_v4()
    ));
    let bin = root.join("bin");
    let hypr = root.join(".config/hypr");
    let validation_count = root.join("validation-count");
    fs::create_dir_all(&bin).unwrap();
    fs::create_dir_all(&hypr).unwrap();
    let baseline = b"-- exact user bindings\n\xff\n";
    fs::write(hypr.join("bindings.lua"), baseline).unwrap();
    let omarchy = bin.join("omarchy");
    fs::write(
        &omarchy,
        "#!/bin/sh\ncase \"$*\" in\n  version) printf 'Omarchy 4.0\\n' ;;\n  'plugin list --json') printf '[{\"id\":\"io.github.gardnmi.boomux\",\"enabled\":true}]\\n' ;;\n  'menu keybindings --print') printf '' ;;\n  *) exit 97 ;;\nesac\n",
    )
    .unwrap();
    fs::set_permissions(&omarchy, fs::Permissions::from_mode(0o755)).unwrap();
    let hyprctl = bin.join("hyprctl");
    fs::write(
        &hyprctl,
        "#!/bin/sh\ncase \"$1\" in\n  reload) ;;\n  configerrors) if [ ! -e \"$VALIDATION_COUNT\" ]; then : > \"$VALIDATION_COUNT\"; printf 'bad binding\\n'; fi ;;\n  *) exit 98 ;;\nesac\n",
    )
    .unwrap();
    fs::set_permissions(&hyprctl, fs::Permissions::from_mode(0o755)).unwrap();
    let terminal = bin.join("xdg-terminal-exec");
    fs::write(&terminal, "#!/bin/sh\nexit 0\n").unwrap();
    fs::set_permissions(&terminal, fs::Permissions::from_mode(0o755)).unwrap();

    let pty = native_pty_system()
        .openpty(PtySize {
            rows: 24,
            cols: 120,
            pixel_width: 1200,
            pixel_height: 800,
        })
        .unwrap();
    let mut command = CommandBuilder::new(env!("CARGO_BIN_EXE_boomux"));
    command.arg("setup");
    command.env("HOME", &root);
    command.env("PATH", &bin);
    command.env("VALIDATION_COUNT", &validation_count);
    command.env("HYPRLAND_INSTANCE_SIGNATURE", "test-instance");
    let mut child = pty.slave.spawn_command(command).unwrap();
    drop(pty.slave);
    let mut writer = pty.master.take_writer().unwrap();
    writer.write_all(b"yes\n").unwrap();
    drop(writer);
    assert!(!child.wait().unwrap().success());
    assert_eq!(fs::read(hypr.join("bindings.lua")).unwrap(), baseline);
    fs::remove_dir_all(root).unwrap();
}

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
    let claude = root.join("claude");
    let claude_manifest = claude.join("skills/boomux/.claude-plugin/plugin.json");
    let codex_hooks = root.join(".codex/hooks.json");
    let kiro_hooks = root.join(".kiro/hooks/boomux.json");
    let runtime = root.join("runtime");
    fs::create_dir_all(&bin).unwrap();
    fs::create_dir_all(&runtime).unwrap();
    for (name, version) in [
        ("opencode", "1.18.18"),
        ("pi", "0.84.1"),
        ("claude", "2.1.236"),
        ("codex", "0.147.0"),
        ("kiro-cli", "2.18.0"),
    ] {
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
            .env("CLAUDE_CONFIG_DIR", &claude)
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
    assert_eq!(
        listed["data"]["integrations"]
            .as_array()
            .unwrap()
            .iter()
            .map(|integration| integration["name"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["opencode", "pi", "claude", "codex", "kiro"]
    );

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
    assert!(!claude_manifest.exists());
    assert!(!codex_hooks.exists());
    assert!(!kiro_hooks.exists());
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
    assert!(claude_manifest.is_file());
    assert!(codex_hooks.is_file());
    assert!(kiro_hooks.is_file());
    let manifest: serde_json::Value =
        serde_json::from_slice(&fs::read(&claude_manifest).unwrap()).unwrap();
    assert_eq!(manifest["name"], "boomux");
    let hooks: serde_json::Value =
        serde_json::from_slice(&fs::read(&codex_hooks).unwrap()).unwrap();
    assert_eq!(
        hooks["hooks"]["SessionStart"][0]["hooks"][0]["command"],
        "boomux codex hook"
    );
    let kiro: serde_json::Value = serde_json::from_slice(&fs::read(&kiro_hooks).unwrap()).unwrap();
    assert_eq!(kiro["version"], "v1");
    assert_eq!(kiro["hooks"][0]["action"]["command"], "boomux kiro hook");

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
    let claude_current = command()
        .args(["integration", "status", "claude", "--json"])
        .output()
        .unwrap();
    assert!(claude_current.status.success());
    let claude_current: serde_json::Value = serde_json::from_slice(&claude_current.stdout).unwrap();
    assert_eq!(
        claude_current["data"]["integrations"][0]["asset"]["state"],
        "current"
    );
    assert_eq!(
        claude_current["data"]["integrations"][0]["asset"]["path"],
        claude_manifest.to_str().unwrap()
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
    assert!(claude_manifest.is_file());
    assert!(codex_hooks.is_file());
    assert!(kiro_hooks.is_file());
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
    assert!(!claude_manifest.exists());
    assert!(!codex_hooks.exists());
    assert!(!kiro_hooks.exists());
    assert!(config.join("opencode/plugins").is_dir());
    assert!(pi.join("extensions").is_dir());
    assert!(claude.join("skills/boomux/.claude-plugin").is_dir());
    assert!(root.join(".codex").is_dir());
    assert!(root.join(".kiro/hooks").is_dir());

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

#[test]
fn codex_hidden_launcher_scopes_chat_hooks_and_passes_service_commands_through() {
    let root = std::env::temp_dir().join(format!(
        "boomux-codex-launcher-{}-{}",
        std::process::id(),
        Uuid::new_v4()
    ));
    let bin = root.join("bin");
    let codex_home = root.join("codex-home");
    fs::create_dir_all(&bin).unwrap();
    fs::create_dir(&codex_home).unwrap();
    fs::write(
        codex_home.join("hooks.json"),
        include_str!("../../integrations/codex/hooks.json"),
    )
    .unwrap();
    let codex = bin.join("codex");
    fs::write(
        &codex,
        "#!/bin/sh\n: > \"$BOOMUX_CODEX_CAPTURE\"\nfor arg do printf '%s\\0' \"$arg\" >> \"$BOOMUX_CODEX_CAPTURE\"; done\nprintf '%s' \"${BOOMUX_CODEX_RUN_SCOPED-unset}\" > \"$BOOMUX_CODEX_MARKER\"\n",
    )
    .unwrap();
    fs::set_permissions(&codex, fs::Permissions::from_mode(0o755)).unwrap();

    let run = |arguments: &[&str], case: &str| {
        let capture = root.join(format!("{case}-argv"));
        let marker = root.join(format!("{case}-marker"));
        let mut command = Command::new(env!("CARGO_BIN_EXE_boomux"));
        command
            .args(["codex", "launch", "--"])
            .args(arguments)
            .env("PATH", &bin)
            .env("CODEX_HOME", &codex_home)
            .env("BOOMUX_CODEX_CAPTURE", &capture)
            .env("BOOMUX_CODEX_MARKER", &marker)
            .env("BOOMUX_SHELL_ID", "shell-1")
            .env("BOOMUX_RUN_ID", "run-1");
        let output = command.output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        (
            fs::read(capture).unwrap(),
            fs::read_to_string(marker).unwrap(),
        )
    };

    let (argv, marker) = run(&["resume", "thread; literal"], "resume");
    assert_eq!(argv, b"--enable\0hooks\0resume\0thread; literal\0");
    assert_eq!(marker, "1");

    let (argv, marker) = run(&["remote-control", "start", "--json"], "service");
    assert_eq!(argv, b"remote-control\0start\0--json\0");
    assert_eq!(marker, "unset");

    let (argv, marker) = run(&["--remote", "unix:///tmp/codex.sock"], "remote");
    assert_eq!(argv, b"--remote\0unix:///tmp/codex.sock\0");
    assert_eq!(marker, "unset");

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn kiro_hidden_launcher_selects_v3_and_preserves_explicit_arguments() {
    let root = std::env::temp_dir().join(format!(
        "boomux-kiro-launcher-{}-{}",
        std::process::id(),
        Uuid::new_v4()
    ));
    let bin = root.join("bin");
    let kiro_home = root.join("kiro-home");
    fs::create_dir_all(&bin).unwrap();
    fs::create_dir_all(kiro_home.join("hooks")).unwrap();
    fs::write(
        kiro_home.join("hooks/boomux.json"),
        include_str!("../../integrations/kiro/boomux.json"),
    )
    .unwrap();
    let kiro = bin.join("kiro-cli");
    fs::write(
        &kiro,
        "#!/bin/sh\n: > \"$BOOMUX_KIRO_CAPTURE\"\nfor arg do printf '%s\\0' \"$arg\" >> \"$BOOMUX_KIRO_CAPTURE\"; done\nprintf '%s' \"${BOOMUX_KIRO_LAUNCH_HOLDER-unset}\" > \"$BOOMUX_KIRO_MARKER\"\n",
    )
    .unwrap();
    fs::set_permissions(&kiro, fs::Permissions::from_mode(0o755)).unwrap();

    let run = |arguments: &[&str], case: &str| {
        let capture = root.join(format!("{case}-argv"));
        let marker = root.join(format!("{case}-marker"));
        let mut command = Command::new(env!("CARGO_BIN_EXE_boomux"));
        command
            .args(["kiro", "launch", "--"])
            .args(arguments)
            .env("PATH", &bin)
            .env("KIRO_HOME", &kiro_home)
            .env("BOOMUX_KIRO_CAPTURE", &capture)
            .env("BOOMUX_KIRO_MARKER", &marker)
            .env("BOOMUX_SHELL_ID", "shell-1")
            .env("BOOMUX_RUN_ID", "run-1");
        let output = command.output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        (
            fs::read(capture).unwrap(),
            fs::read_to_string(marker).unwrap(),
        )
    };

    let (argv, marker) = run(&[], "bare");
    assert_eq!(argv, b"--v3\0");
    assert_eq!(marker, "unset");

    let (argv, marker) = run(&["--v3", "chat", "two words", "semi;colon"], "v3");
    assert_eq!(argv, b"--v3\0chat\0two words\0semi;colon\0");
    assert_eq!(marker, "unset");

    let (argv, marker) = run(&["chat", "legacy"], "v2");
    assert_eq!(argv, b"chat\0legacy\0");
    assert_eq!(marker, "unset");

    let (argv, marker) = run(&["--v3", "chat", "--cloud"], "cloud");
    assert_eq!(argv, b"--v3\0chat\0--cloud\0");
    assert_eq!(marker, "unset");

    fs::write(kiro_home.join("hooks/boomux.json"), "custom").unwrap();
    let (argv, marker) = run(&[], "modified");
    assert!(argv.is_empty());
    assert_eq!(marker, "unset");

    fs::remove_dir_all(root).unwrap();
}

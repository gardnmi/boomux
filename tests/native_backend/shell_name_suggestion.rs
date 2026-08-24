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
fn hyprland_workspace_layer_places_toggles_and_reuses_exact_shell_windows() {
    let daemon = TestDaemon::start();
    let workspace = daemon
        .client
        .create_global_workspace("desktop-layer")
        .unwrap();
    let node_id = daemon.client.node_identity().unwrap();
    let bin = daemon.runtime_dir.join("hypr-bin");
    let config_dir = daemon.runtime_dir.join("config/boomux");
    fs::create_dir(&bin).unwrap();
    fs::create_dir_all(&config_dir).unwrap();
    fs::write(
        config_dir.join("config.toml"),
        "[desktop]\nworkspace_layer = \"hyprland-special\"\n",
    )
    .unwrap();
    let clients = daemon.runtime_dir.join("hypr-clients.json");
    let monitors = daemon.runtime_dir.join("hypr-monitors.json");
    let dispatches = daemon.runtime_dir.join("hypr-dispatches");
    let evaluations = daemon.runtime_dir.join("hypr-evaluations");
    let gate_marker = daemon.runtime_dir.join("hypr-gate-status");
    let launch_count = daemon.runtime_dir.join("hypr-launch-count");
    fs::write(&clients, "[]").unwrap();
    fs::write(
        &monitors,
        r#"[{"focused":true,"specialWorkspace":{"id":0,"name":""}}]"#,
    )
    .unwrap();

    let hyprctl = bin.join("hyprctl");
    fs::write(
        &hyprctl,
        "#!/bin/sh\ncase \"$1 $2\" in\n  '-j clients') cat \"$BOOMUX_HYPR_CLIENTS\" ;;\n  '-j monitors') cat \"$BOOMUX_HYPR_MONITORS\" ;;\n  'dispatch '*) printf '%s\\n' \"$2\" >> \"$BOOMUX_HYPR_DISPATCHES\"; printf ok ;;\n  'eval '*) printf '%s\\n' \"$2\" >> \"$BOOMUX_HYPR_EVALUATIONS\"; printf ok ;;\n  *) exit 64 ;;\nesac\n",
    )
    .unwrap();
    fs::set_permissions(&hyprctl, fs::Permissions::from_mode(0o700)).unwrap();

    let resolver = bin.join("xdg-terminal-exec");
    let resolver_script = r#"#!/bin/sh
title=
while [ "$#" -gt 0 ] && [ "$1" != -- ]; do
  case "$1" in --title=*) title=${1#--title=} ;; esac
  shift
done
shift
gate=
while [ "$#" -gt 0 ]; do
  if [ "$1" = --gate ]; then shift; gate=$1; break; fi
  shift
done
python3 -c 'import json,os,sys; cp=os.environ["BOOMUX_HYPR_LAUNCH_COUNT"]; n=int(open(cp).read())+1 if os.path.exists(cp) else 1; open(cp,"w").write(str(n)); p=os.environ["BOOMUX_HYPR_CLIENTS"]; clients=json.load(open(p)); clients.append({"address":f"0x{n:08x}","workspace":{"id":-99,"name":"special:boomux-"+os.environ["BOOMUX_HYPR_WORKSPACE"]},"initialTitle":sys.argv[1]}); json.dump(clients,open(p,"w"))' "$title"
printf '%s\0' python3 -c 'import socket,sys; s=socket.socket(socket.AF_UNIX); s.connect(sys.argv[1]); open(sys.argv[2], "wb").write(s.recv(1))' "$gate" "$BOOMUX_GATE_MARKER"
"#;
    fs::write(&resolver, resolver_script).unwrap();
    fs::set_permissions(&resolver, fs::Permissions::from_mode(0o700)).unwrap();

    let path = format!("{}:/usr/bin:/bin", bin.display());
    let configure = |command: &mut std::process::Command| {
        command
            .env("PATH", &path)
            .env("HYPRLAND_INSTANCE_SIGNATURE", "test")
            .env("BOOMUX_HYPR_CLIENTS", &clients)
            .env("BOOMUX_HYPR_MONITORS", &monitors)
            .env("BOOMUX_HYPR_DISPATCHES", &dispatches)
            .env("BOOMUX_HYPR_EVALUATIONS", &evaluations)
            .env("BOOMUX_HYPR_WORKSPACE", &workspace.id)
            .env("BOOMUX_GATE_MARKER", &gate_marker)
            .env("BOOMUX_HYPR_LAUNCH_COUNT", &launch_count);
    };
    let mut create = daemon.command();
    configure(&mut create);
    let output = create
        .args([
            "shell",
            "create",
            &workspace.id,
            "--node",
            &node_id,
            "--name",
            "desktop",
            "--cwd",
            "/tmp",
            "--open",
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "desktop shell creation failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    wait_until(
        || fs::read(&gate_marker).is_ok_and(|status| status == [1]),
        "desktop terminal did not observe the successful creation gate",
    );
    let shell = daemon
        .client
        .snapshot()
        .unwrap()
        .workspaces
        .into_iter()
        .flat_map(|workspace| workspace.shells)
        .find(|shell| shell.name == "desktop")
        .unwrap();
    let clients_value: serde_json::Value =
        serde_json::from_slice(&fs::read(&clients).unwrap()).unwrap();
    assert_eq!(
        clients_value[0]["initialTitle"],
        format!(
            "boomux:shell:{node_id}:{} | desktop-layer - desktop",
            shell.id
        )
    );

    let mut pending = daemon.command();
    configure(&mut pending);
    let output = pending
        .args([
            "shell",
            "create",
            &workspace.id,
            "--node",
            &node_id,
            "--name",
            "concurrent",
            "--cwd",
            "/tmp",
        ])
        .output()
        .unwrap();
    assert!(output.status.success());
    fs::write(&launch_count, "0").unwrap();
    fs::write(
        &resolver,
        r#"#!/bin/sh
title=
while [ "$#" -gt 0 ] && [ "$1" != -- ]; do
  case "$1" in --title=*) title=${1#--title=} ;; esac
  shift
done
n=$(cat "$BOOMUX_HYPR_LAUNCH_COUNT")
n=$((n + 1))
printf '%s' "$n" > "$BOOMUX_HYPR_LAUNCH_COUNT"
printf '%s\0' python3 -c 'import json,sys,time; time.sleep(2.5); p=sys.argv[1]; clients=json.load(open(p)); clients.append({"address":"0xfeedbeef","workspace":{"id":-99,"name":"special:boomux-"+sys.argv[2]},"initialTitle":sys.argv[3]}); json.dump(clients,open(p,"w"))' "$BOOMUX_HYPR_CLIENTS" "$BOOMUX_HYPR_WORKSPACE" "$title"
"#,
    )
    .unwrap();
    let mut first = daemon.command();
    configure(&mut first);
    first.args(["workspace", "open", &workspace.id]);
    let mut second = daemon.command();
    configure(&mut second);
    second.args(["workspace", "open", &workspace.id]);
    let first = first.spawn().unwrap();
    let second = second.spawn().unwrap();
    let first = first.wait_with_output().unwrap();
    let second = second.wait_with_output().unwrap();
    assert!(
        first.status.success(),
        "first concurrent open failed: {}",
        String::from_utf8_lossy(&first.stderr)
    );
    assert!(
        second.status.success(),
        "second concurrent open failed: {}",
        String::from_utf8_lossy(&second.stderr)
    );
    assert_eq!(fs::read_to_string(&launch_count).unwrap(), "1");
    wait_until(
        || {
            serde_json::from_slice::<serde_json::Value>(&fs::read(&clients).unwrap())
                .unwrap()
                .as_array()
                .is_some_and(|clients| clients.len() == 2)
        },
        "delayed terminal did not register its Hyprland window",
    );
    let clients_value: serde_json::Value =
        serde_json::from_slice(&fs::read(&clients).unwrap()).unwrap();
    assert_eq!(clients_value.as_array().unwrap().len(), 2);

    fs::write(&resolver, "#!/bin/sh\nexit 64\n").unwrap();
    let mut reopen = daemon.command();
    configure(&mut reopen);
    let output = reopen
        .args(["workspace", "open", &workspace.id])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "existing window was not reused: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let mut toggle = daemon.command();
    configure(&mut toggle);
    let output = toggle.args(["desktop", "toggle"]).output().unwrap();
    assert!(
        output.status.success(),
        "desktop toggle failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        fs::read_to_string(&dispatches).unwrap(),
        format!(
            "hl.dsp.workspace.toggle_special(\"boomux-{}\")\n",
            workspace.id
        )
    );
    assert_eq!(
        fs::read_to_string(&evaluations).unwrap(),
        format!(
            "hl.workspace_rule({{ workspace = \"special:boomux-{}\", layout = \"dwindle\" }})\n",
            workspace.id
        )
    );

    let mut close = daemon.command();
    configure(&mut close);
    let output = close.args(["desktop", "close"]).output().unwrap();
    assert!(output.status.success());
    assert_eq!(
        fs::read_to_string(&dispatches).unwrap(),
        format!(
            "hl.dsp.workspace.toggle_special(\"boomux-{}\")\nhl.dsp.window.close({{ window = \"activewindow\" }})\n",
            workspace.id
        )
    );

    fs::write(
        &monitors,
        format!(
            r#"[{{"focused":true,"specialWorkspace":{{"id":-99,"name":"special:boomux-{}"}}}}]"#,
            workspace.id
        ),
    )
    .unwrap();
    fs::write(&resolver, resolver_script).unwrap();
    let mut contextual_terminal = daemon.command();
    configure(&mut contextual_terminal);
    let output = contextual_terminal
        .args(["desktop", "terminal"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "contextual terminal failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        daemon
            .client
            .snapshot()
            .unwrap()
            .workspaces
            .into_iter()
            .flat_map(|workspace| workspace.shells)
            .count(),
        3
    );
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

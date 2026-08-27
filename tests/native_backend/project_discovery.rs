use std::fs;
use std::process::Command;

use uuid::Uuid;

#[test]
fn bare_cli_prints_command_help_without_starting_the_daemon() {
    let root = std::env::temp_dir().join(format!(
        "boomux-bare-help-{}-{}",
        std::process::id(),
        Uuid::new_v4()
    ));
    let runtime = root.join("runtime");
    fs::create_dir_all(&runtime).unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_boomux"))
        .env("HOME", &root)
        .env("XDG_RUNTIME_DIR", &runtime)
        .env("XDG_STATE_HOME", root.join("state"))
        .output()
        .unwrap();

    assert!(output.status.success());
    assert!(output.stderr.is_empty());
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("Usage:"), "{stdout}");
    assert!(stdout.contains("Commands:"), "{stdout}");
    assert!(stdout.contains("ui"), "{stdout}");
    assert!(!runtime.join("boomux/daemon.sock").exists());

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn project_list_is_local_and_has_a_stable_json_envelope() {
    let root = std::env::temp_dir().join(format!(
        "boomux-project-discovery-{}-{}",
        std::process::id(),
        Uuid::new_v4()
    ));
    let projects_root = root.join("Projects");
    let project = projects_root.join("alpha");
    let runtime = root.join("runtime");
    let config_home = root.join("config-home");
    fs::create_dir_all(project.join(".git")).unwrap();
    fs::create_dir_all(&runtime).unwrap();
    fs::create_dir_all(&config_home).unwrap();
    let config = root.join("config.toml");
    fs::write(
        &config,
        format!(
            "[projects]\nroots = [{:?}, {:?}]\nmax_depth = 2\n",
            projects_root.display().to_string(),
            root.join("missing").display().to_string()
        ),
    )
    .unwrap();

    let command = || {
        let mut command = Command::new(env!("CARGO_BIN_EXE_boomux"));
        command
            .env("BOOMUX_CONFIG", &config)
            .env("HOME", &root)
            .env("XDG_CONFIG_HOME", &config_home)
            .env("XDG_RUNTIME_DIR", &runtime)
            .env("XDG_STATE_HOME", root.join("state"));
        command
    };

    let output = command()
        .args(["project", "list", "--json"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "project list failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["schema"], "boomux.cli/v1");
    assert_eq!(value["command"], "project.list");
    assert_eq!(value["data"]["roots_configured"], true);
    assert_eq!(value["data"]["projects"].as_array().unwrap().len(), 1);
    assert_eq!(value["data"]["projects"][0]["name"], "alpha");
    assert_eq!(
        value["data"]["projects"][0]["path"],
        project.canonicalize().unwrap().display().to_string()
    );
    assert_eq!(value["data"]["projects"][0]["group"], "Projects");
    assert_eq!(value["data"]["projects"][0]["group_order"], 0);
    assert_eq!(value["data"]["warnings"].as_array().unwrap().len(), 1);
    assert!(
        value["data"]["warnings"][0]
            .as_str()
            .unwrap()
            .contains("project root is not a directory")
    );

    let human = command().args(["project", "list"]).output().unwrap();
    assert!(human.status.success());
    let stdout = String::from_utf8(human.stdout).unwrap();
    let stderr = String::from_utf8(human.stderr).unwrap();
    assert!(stdout.contains("GROUP\tNAME\tPATH"));
    assert!(stdout.contains("Projects\talpha\t"));
    assert!(stderr.contains("warning: project root is not a directory"));

    fs::write(&config, "").unwrap();
    let empty = command()
        .args(["project", "list", "--json"])
        .output()
        .unwrap();
    assert!(empty.status.success());
    let empty: serde_json::Value = serde_json::from_slice(&empty.stdout).unwrap();
    assert_eq!(empty["data"]["roots_configured"], false);
    assert_eq!(empty["data"]["projects"], serde_json::json!([]));
    assert_eq!(empty["data"]["warnings"], serde_json::json!([]));

    let capabilities = command().args(["capabilities", "--json"]).output().unwrap();
    assert!(capabilities.status.success());
    let capabilities: serde_json::Value = serde_json::from_slice(&capabilities.stdout).unwrap();
    assert!(
        capabilities["data"]["json_commands"]
            .as_array()
            .unwrap()
            .iter()
            .any(|command| command == "project.list")
    );
    assert!(!runtime.join("boomux/daemon.sock").exists());

    fs::remove_dir_all(root).unwrap();
}

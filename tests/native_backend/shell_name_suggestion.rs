use std::collections::BTreeSet;

use boomux::protocol::ShellSpec;

use crate::support::{TestDaemon, assert_generated_name};

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

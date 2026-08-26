use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use uuid::Uuid;

struct TestDirectory(PathBuf);

impl TestDirectory {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "boomux-config-cli-{}-{}",
            std::process::id(),
            Uuid::new_v4()
        ));
        fs::create_dir(&path).unwrap();
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }

    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_boomux"));
        command
            .env("HOME", self.path())
            .env("XDG_CONFIG_HOME", self.path().join("xdg"))
            .env("XDG_RUNTIME_DIR", self.path().join("runtime"))
            .env_remove("BOOMUX_CONFIG")
            .env_remove("VISUAL")
            .env_remove("EDITOR");
        command
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn assert_success(output: &Output) {
    assert!(
        output.status.success(),
        "command failed: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn write_executable(path: &Path, contents: &str) {
    fs::write(path, contents).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
}

#[test]
fn path_uses_environment_override_then_canonical_global_path() {
    let test = TestDirectory::new();
    let global = test.path().join("xdg/boomux/config.toml");
    let output = test.command().args(["config", "path"]).output().unwrap();
    assert_success(&output);
    assert_eq!(
        String::from_utf8(output.stdout).unwrap().trim(),
        global.display().to_string()
    );

    let override_path = test.path().join("override.toml");
    let output = test
        .command()
        .env("BOOMUX_CONFIG", &override_path)
        .args(["config", "path"])
        .output()
        .unwrap();
    assert_success(&output);
    assert_eq!(
        String::from_utf8(output.stdout).unwrap().trim(),
        override_path.display().to_string()
    );
}

#[test]
fn validate_resolves_semantics_after_layer_merge_without_starting_a_daemon() {
    let test = TestDirectory::new();
    let global = test.path().join("xdg/boomux/config.toml");
    let override_path = test.path().join("override.toml");
    fs::create_dir_all(global.parent().unwrap()).unwrap();
    fs::write(&global, "[projects]\nmax_depth = 99\n").unwrap();
    fs::write(&override_path, "[projects]\nmax_depth = 2\n").unwrap();

    let valid_override = test
        .command()
        .env("BOOMUX_CONFIG", &override_path)
        .args(["config", "validate"])
        .output()
        .unwrap();
    assert_success(&valid_override);
    assert!(String::from_utf8_lossy(&valid_override.stdout).contains("2 loaded layers"));
    assert!(!test.path().join("runtime/boomux/daemon.sock").exists());

    fs::write(
        &override_path,
        "[dashboard]\nfollow_focused_terminal = false\n",
    )
    .unwrap();
    let invalid_merged = test
        .command()
        .env("BOOMUX_CONFIG", &override_path)
        .args(["config", "validate"])
        .output()
        .unwrap();
    assert!(!invalid_merged.status.success());
    assert!(
        String::from_utf8_lossy(&invalid_merged.stderr)
            .contains("projects.max_depth must be between 1 and 10")
    );

    let json = test
        .command()
        .args(["config", "validate", "--json"])
        .output()
        .unwrap();
    assert!(!json.status.success());
    assert!(
        String::from_utf8_lossy(&json.stdout).contains("--json is not supported")
            || String::from_utf8_lossy(&json.stderr).contains("--json is not supported")
    );
}

#[test]
fn canonical_global_environment_override_is_one_loaded_layer() {
    let test = TestDirectory::new();
    let global = test.path().join("xdg/boomux/config.toml");
    fs::create_dir_all(global.parent().unwrap()).unwrap();
    fs::write(&global, "[projects]\nmax_depth = 2\n").unwrap();

    let output = test
        .command()
        .env("BOOMUX_CONFIG", &global)
        .args(["config", "validate"])
        .output()
        .unwrap();
    assert_success(&output);
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("1 loaded layer"), "{stdout}");
    assert!(stdout.contains("active BOOMUX_CONFIG"), "{stdout}");
}

#[test]
fn edit_creates_comprehensive_private_template() {
    let test = TestDirectory::new();
    let target = test.path().join("nested/config.toml");
    let editor = test.path().join("editor");
    write_executable(&editor, "#!/bin/sh\nexit 0\n");

    let output = test
        .command()
        .env("BOOMUX_CONFIG", &target)
        .env("VISUAL", &editor)
        .args(["config", "edit"])
        .output()
        .unwrap();
    assert_success(&output);
    let contents = fs::read_to_string(&target).unwrap();
    for section in [
        "[projects]",
        "[dashboard]",
        "[notifications]",
        "[notifications.sound]",
        "[recovery]",
    ] {
        assert!(
            contents.contains(section),
            "missing template section {section}"
        );
    }
    assert!(contents.contains("# terminal ="));
    assert_eq!(
        fs::metadata(&target).unwrap().permissions().mode() & 0o777,
        0o600
    );
}

#[test]
fn editor_arguments_are_exact_and_shell_metacharacters_are_not_interpreted() {
    let test = TestDirectory::new();
    let target = test.path().join("config.toml");
    let editor = test.path().join("editor");
    let capture = test.path().join("arguments");
    let injected = test.path().join("injected");
    fs::write(&target, "# original comment\n[projects]\nmax_depth = 2\n").unwrap();
    fs::set_permissions(&target, fs::Permissions::from_mode(0o640)).unwrap();
    write_executable(
        &editor,
        "#!/bin/sh\nprintf '%s\\n%s\\n' \"$1\" \"$2\" > \"$CAPTURE\"\nprintf '\\n# editor comment\\n' >> \"$3\"\n",
    );
    let visual = format!(
        "'{}' 'argument with spaces' '$(touch {})'",
        editor.display(),
        injected.display()
    );

    let output = test
        .command()
        .env("BOOMUX_CONFIG", &target)
        .env("VISUAL", visual)
        .env("CAPTURE", &capture)
        .args(["config", "edit"])
        .output()
        .unwrap();
    assert_success(&output);
    assert_eq!(
        fs::read_to_string(capture).unwrap(),
        format!("argument with spaces\n$(touch {})\n", injected.display())
    );
    assert!(!injected.exists());
    let contents = fs::read_to_string(&target).unwrap();
    assert!(contents.contains("# original comment"));
    assert!(contents.contains("# editor comment"));
    assert_eq!(
        fs::metadata(&target).unwrap().permissions().mode() & 0o777,
        0o640
    );
}

#[test]
fn invalid_candidate_and_concurrent_target_change_leave_target_unreplaced() {
    let test = TestDirectory::new();
    let target = test.path().join("config.toml");
    let editor = test.path().join("editor");
    let original = "# original\n[projects]\nmax_depth = 2\n";
    fs::write(&target, original).unwrap();
    write_executable(
        &editor,
        "#!/bin/sh\nprintf '[projects]\\nmax_depth = 99\\n' > \"$1\"\n",
    );
    let invalid = test
        .command()
        .env("BOOMUX_CONFIG", &target)
        .env("VISUAL", &editor)
        .args(["config", "edit"])
        .output()
        .unwrap();
    assert!(!invalid.status.success());
    assert_eq!(fs::read_to_string(&target).unwrap(), original);

    write_executable(
        &editor,
        "#!/bin/sh\nprintf '# candidate\\n[projects]\\nmax_depth = 3\\n' > \"$1\"\nprintf '# concurrent\\n[projects]\\nmax_depth = 4\\n' > \"$TARGET\"\n",
    );
    let concurrent = test
        .command()
        .env("BOOMUX_CONFIG", &target)
        .env("VISUAL", &editor)
        .env("TARGET", &target)
        .args(["config", "edit"])
        .output()
        .unwrap();
    assert!(!concurrent.status.success());
    assert!(
        String::from_utf8_lossy(&concurrent.stderr).contains("changed while it was being edited")
    );
    assert_eq!(
        fs::read_to_string(&target).unwrap(),
        "# concurrent\n[projects]\nmax_depth = 4\n"
    );
}

#[test]
fn bounded_reads_and_unsafe_targets_are_rejected() {
    let test = TestDirectory::new();
    let oversized = test.path().join("oversized.toml");
    fs::write(&oversized, vec![b'#'; 1024 * 1024 + 1]).unwrap();
    let output = test
        .command()
        .env("BOOMUX_CONFIG", &oversized)
        .args(["config", "validate"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("exceeds the 1048576 byte limit"));

    let real = test.path().join("real.toml");
    let linked = test.path().join("linked.toml");
    let editor = test.path().join("editor");
    fs::write(&real, "# unchanged\n").unwrap();
    symlink(&real, &linked).unwrap();
    write_executable(&editor, "#!/bin/sh\nexit 0\n");
    let output = test
        .command()
        .env("BOOMUX_CONFIG", &linked)
        .env("VISUAL", &editor)
        .args(["config", "edit"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("not a regular file"));
    assert_eq!(fs::read_to_string(real).unwrap(), "# unchanged\n");
}

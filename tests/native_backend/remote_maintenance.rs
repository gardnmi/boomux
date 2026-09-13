use boomux::client::Client;
use serde_json::{Value, json};
#[cfg(target_os = "linux")]
use std::process::Stdio;
use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::PathBuf,
    process::{Command, Output},
};
use uuid::Uuid;

struct Fixture {
    root: PathBuf,
    node: String,
    runner: PathBuf,
}
impl Fixture {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!("boomux-recovery-{}", Uuid::new_v4()));
        for directory in ["", "home", "state/boomux", "runtime/boomux", "config"] {
            fs::create_dir_all(root.join(directory)).unwrap();
            fs::set_permissions(root.join(directory), fs::Permissions::from_mode(0o700)).unwrap();
        }
        let node = Uuid::new_v4().to_string();
        let identity = root.join("state/boomux/node.json");
        fs::write(&identity, json!({"version":1,"node_id":node}).to_string()).unwrap();
        fs::set_permissions(identity, fs::Permissions::from_mode(0o600)).unwrap();
        let runner = root.join("runner");
        fs::copy(env!("CARGO_BIN_EXE_boomux"), &runner).unwrap();
        Self { root, node, runner }
    }
    fn command(&self) -> Command {
        self.command_at(&self.runner)
    }
    fn command_at(&self, executable: &std::path::Path) -> Command {
        let mut cmd = Command::new(executable);
        cmd.env("HOME", self.root.join("home"))
            .env("XDG_RUNTIME_DIR", self.root.join("runtime"))
            .env("XDG_STATE_HOME", self.root.join("state"))
            .env("BOOMUX_STATE_HOME", self.root.join("state"))
            .env("XDG_CONFIG_HOME", self.root.join("config"))
            .env("SHELL", "/bin/sh");
        cmd
    }
    fn installed(&self) -> PathBuf {
        self.root.join("home/.local/bin/boomux")
    }
    fn probe(&self) -> Value {
        let out = self
            .command()
            .args(["__remote-maintenance", "probe", &self.node])
            .output()
            .unwrap();
        success(&out);
        serde_json::from_slice(&out.stdout).unwrap()
    }
    fn action(&self, action: &str, probe: &Value) -> Output {
        self.command()
            .args([
                "__remote-maintenance",
                action,
                &self.node,
                probe["token"].as_str().unwrap(),
            ])
            .output()
            .unwrap()
    }
    fn client(&self) -> Client {
        Client::from_socket_path(self.root.join("runtime/boomux/daemon.sock"))
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = self.client().shutdown_if_node_identity(&self.node);
        let _ = fs::remove_dir_all(&self.root);
    }
}
fn success(out: &Output) {
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn missing_executable_can_be_repaired_and_removal_is_repeatable() {
    let fixture = Fixture::new();
    fs::remove_dir(fixture.root.join("runtime/boomux")).unwrap();
    let saved = fixture.root.join("state/boomux/user-data");
    fs::write(&saved, b"preserve").unwrap();
    let probe = fixture.probe();
    assert!(probe["probe"]["installed"].is_null());
    assert!(!fixture.root.join("runtime/boomux/daemon.sock").exists());
    success(&fixture.action("repair", &probe));
    assert!(fixture.installed().is_file());
    assert_eq!(fixture.probe()["probe"]["node_id"], fixture.node);
    success(&fixture.action("remove", &fixture.probe()));
    assert!(!fixture.installed().exists());
    success(&fixture.action("remove", &fixture.probe()));
    assert_eq!(fs::read(saved).unwrap(), b"preserve");
    assert!(
        !fixture
            .root
            .join("home/.local/bin/.boomux.bootstrap.lock")
            .exists()
    );
}

#[test]
fn changed_identity_and_changed_executable_refuse_mutation() {
    let fixture = Fixture::new();
    let out = fixture
        .command()
        .args(["__remote-maintenance", "probe", &Uuid::new_v4().to_string()])
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(!fixture.installed().exists());
    let probe = fixture.probe();
    fs::create_dir_all(fixture.installed().parent().unwrap()).unwrap();
    fs::write(fixture.installed(), b"concurrent install").unwrap();
    let out = fixture.action("remove", &probe);
    assert!(!out.status.success());
    assert_eq!(
        fs::read(fixture.installed()).unwrap(),
        b"concurrent install"
    );
}

#[test]
fn abandoned_recovery_lock_is_reclaimed_but_legacy_transaction_is_preserved() {
    let fixture = Fixture::new();
    fs::create_dir_all(fixture.installed().parent().unwrap()).unwrap();
    let lock = fixture.installed().with_file_name(".boomux.bootstrap.lock");
    fs::create_dir(&lock).unwrap();
    fs::write(lock.join("id"), b"legacy transaction").unwrap();
    assert!(!fixture.action("remove", &fixture.probe()).status.success());
    assert_eq!(fs::read(lock.join("id")).unwrap(), b"legacy transaction");
    fs::remove_dir_all(&lock).unwrap();
    fs::write(&lock, b"boomux-recovery-v1\n").unwrap();
    fs::set_permissions(&lock, fs::Permissions::from_mode(0o600)).unwrap();
    use std::os::fd::AsRawFd;
    let held = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&lock)
        .unwrap();
    assert_eq!(
        unsafe { libc::flock(held.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
        0
    );
    assert!(!fixture.action("remove", &fixture.probe()).status.success());
    assert!(lock.exists());
    drop(held);
    success(&fixture.action("remove", &fixture.probe()));
    assert!(!lock.exists());
}

#[test]
fn stale_daemon_socket_does_not_block_missing_executable_removal() {
    let fixture = Fixture::new();
    let socket = fixture.root.join("runtime/boomux/daemon.sock");
    let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
    drop(listener);
    assert!(fixture.probe()["probe"]["daemon_pid"].is_null());
    success(&fixture.action("remove", &fixture.probe()));
}

#[test]
fn recovery_refuses_symlink_installation_parent() {
    let fixture = Fixture::new();
    fs::create_dir_all(fixture.root.join("home/.local")).unwrap();
    std::os::unix::fs::symlink(
        fixture.root.join("config"),
        fixture.root.join("home/.local/bin"),
    )
    .unwrap();
    let out = fixture
        .command()
        .args(["__remote-maintenance", "probe", &fixture.node])
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(!fixture.root.join("config/boomux").exists());
}

#[test]
fn unreadable_saved_state_prevents_repair_but_does_not_prevent_removal() {
    let fixture = Fixture::new();
    let state = fixture.root.join("state/boomux/state.json");
    fs::write(&state, b"{\"version\":999}").unwrap();
    fs::set_permissions(&state, fs::Permissions::from_mode(0o600)).unwrap();
    let out = fixture.action("repair", &fixture.probe());
    assert!(!out.status.success());
    assert!(!fixture.installed().exists());
    assert!(String::from_utf8_lossy(&out.stderr).contains("saved remote state cannot be read"));
    success(&fixture.action("remove", &fixture.probe()));
    assert_eq!(fs::read(&state).unwrap(), b"{\"version\":999}");
}

#[cfg(target_os = "linux")]
#[test]
fn recovery_follows_running_user_installation_and_preserves_shell_run() {
    use crate::support::{profile, wait_until};
    use boomux::protocol::ShellSpec;
    let fixture = Fixture::new();
    let installed = fixture.root.join("home/custom/bin/boomux");
    fs::create_dir_all(installed.parent().unwrap()).unwrap();
    fs::copy(&fixture.runner, &installed).unwrap();
    let mut child = fixture
        .command_at(&installed)
        .args(["daemon", "run"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let client = fixture.client();
    wait_until(
        || client.ping().is_ok(),
        "recovery fixture daemon did not start",
    );
    let workspace = client
        .create_workspace(
            "keep",
            vec![ShellSpec {
                name: "keep".into(),
                command: vec![
                    "/bin/sh".into(),
                    "-c".into(),
                    "echo $$ > shell.pid; exec /bin/sleep 300".into(),
                ],
                cwd: fixture.root.clone(),
            }],
        )
        .unwrap();
    let shell = &workspace.shells[0].id;
    let attachment = client.attach(shell, false, profile()).unwrap();
    let run = client.get_shell(shell).unwrap().run.unwrap();
    drop(attachment);
    wait_until(
        || fixture.root.join("shell.pid").exists(),
        "Shell PID was not recorded",
    );
    let shell_pid: i32 = fs::read_to_string(fixture.root.join("shell.pid"))
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    let old_pid = client.daemon_process_credentials().unwrap().pid;
    // First replace an existing executable at a noncanonical user path.
    let probe = fixture.probe();
    assert_eq!(probe["probe"]["destination"], installed.to_str().unwrap());
    success(&fixture.action("repair", &probe));
    assert_ne!(client.daemon_process_credentials().unwrap().pid, old_pid);
    assert_eq!(client.get_shell(shell).unwrap().run.unwrap().id, run.id);
    // Then repair the same running owner after its on-disk executable disappears.
    fs::remove_file(&installed).unwrap();
    success(&fixture.action("repair", &fixture.probe()));
    assert_eq!(client.get_shell(shell).unwrap().run.unwrap().id, run.id);
    assert_eq!(client.node_identity().unwrap(), fixture.node);
    assert_eq!(unsafe { libc::kill(shell_pid, 0) }, 0);
    assert!(
        client
            .get_shell(shell)
            .unwrap()
            .run
            .unwrap()
            .ended_at_ms
            .is_none()
    );
    success(&fixture.action("remove", &fixture.probe()));
    assert!(!installed.exists());
    assert!(client.ping().is_err());
    let _ = child.wait();
}

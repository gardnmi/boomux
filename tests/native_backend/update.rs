use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use boomux::client::Client;
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::support::wait_until;

fn fixture() -> PathBuf {
    let root = std::env::temp_dir().join(format!("boomux-update-native-{}", Uuid::new_v4()));
    fs::create_dir(&root).unwrap();
    let curl = root.join("curl");
    fs::write(
        &curl,
        "#!/bin/sh\noutput=\nprevious=\nfor argument do\n  if [ \"$previous\" = --output ]; then output=$argument; fi\n  previous=$argument\ndone\n[ -n \"$output\" ] || exit 64\nprintf '%s' '{\"tag_name\":\"v99.0.0\",\"html_url\":\"https://github.com/gardnmi/boomux/releases/tag/v99.0.0\"}' > \"$output\"\n",
    )
    .unwrap();
    fs::set_permissions(&curl, fs::Permissions::from_mode(0o755)).unwrap();
    root
}

fn command(root: &Path) -> Command {
    command_with_executable(root, Path::new(env!("CARGO_BIN_EXE_boomux")))
}

fn command_with_executable(root: &Path, executable: &Path) -> Command {
    let mut command = Command::new(executable);
    command
        .env("PATH", root)
        .env("HOME", root.join("home"))
        .env("XDG_RUNTIME_DIR", root.join("runtime"))
        .env("XDG_CONFIG_HOME", root.join("config"))
        .env("XDG_STATE_HOME", root.join("state"));
    command
}

fn executable_digest(path: &Path) -> [u8; 32] {
    let mut file = File::open(path).unwrap();
    let mut digest = Sha256::new();
    let mut buffer = [0; 8192];
    loop {
        let count = file.read(&mut buffer).unwrap();
        if count == 0 {
            return digest.finalize().into();
        }
        digest.update(&buffer[..count]);
    }
}

#[cfg(target_os = "linux")]
fn wait_for_replacement_pid(executable: &Path, excluded: u32) -> u32 {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(pid) = fs::read_dir("/proc")
            .unwrap()
            .filter_map(Result::ok)
            .find_map(|entry| {
                let pid = entry.file_name().to_str()?.parse::<u32>().ok()?;
                (pid != excluded
                    && fs::read_link(entry.path().join("exe")).is_ok_and(|path| path == executable))
                .then_some(pid)
            })
        {
            return pid;
        }
        assert!(
            Instant::now() < deadline,
            "replacement daemon did not execute pinned path"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn package_managed_executable_reports_package_manager_guidance() {
    let root = fixture();
    let executable = root.join("home/.local/share/mise/installs/boomux/1.0.0/bin/boomux");
    fs::create_dir_all(executable.parent().unwrap()).unwrap();
    fs::copy(env!("CARGO_BIN_EXE_boomux"), &executable).unwrap();

    let output = command_with_executable(&root, &executable)
        .args(["--json", "update", "status"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "package status failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["data"]["install_kind"], "package_managed");
    assert_eq!(value["data"]["state"], "ineligible");
    assert_eq!(value["data"]["recommended_action"], "use_package_manager");
    assert_eq!(value["data"]["path"], executable.to_string_lossy().as_ref());
    assert!(!root.join("runtime/boomux/daemon.sock").exists());
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn update_status_has_stable_json_and_does_not_start_daemon() {
    let root = fixture();
    let output = command(&root)
        .args(["--json", "update", "status"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "update status failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["schema"], "boomux.cli/v1");
    assert_eq!(value["command"], "update.status");
    assert_eq!(value["data"]["current"], env!("CARGO_PKG_VERSION"));
    assert_eq!(value["data"]["latest"], "99.0.0");
    assert_eq!(value["data"]["state"], "ineligible");
    assert_eq!(value["data"]["install_kind"], "development_build");
    assert_eq!(
        value["data"]["target"],
        if cfg!(target_arch = "x86_64") {
            "x86_64-unknown-linux-gnu"
        } else {
            "aarch64-unknown-linux-gnu"
        }
    );
    assert_eq!(
        value["data"]["release_url"],
        "https://github.com/gardnmi/boomux/releases/tag/v99.0.0"
    );
    assert_eq!(
        value["data"]["recommended_action"],
        "install_github_release"
    );
    assert!(
        value["data"]["path"]
            .as_str()
            .is_some_and(|path| !path.is_empty())
    );
    assert!(!root.join("runtime/boomux/daemon.sock").exists());
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn capabilities_advertise_update_surfaces() {
    let root = fixture();
    let output = command(&root)
        .args(["--json", "capabilities"])
        .output()
        .unwrap();
    assert!(output.status.success());
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let commands = value["data"]["json_commands"].as_array().unwrap();
    assert!(commands.iter().any(|value| value == "update.status"));
    let features = value["data"]["features"].as_array().unwrap();
    assert!(features.iter().any(|value| value == "local_update_status"));
    assert!(features.iter().any(|value| value == "guided_local_update"));
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn json_guided_update_is_rejected_before_any_mutation() {
    let root = fixture();
    let output = command(&root).args(["--json", "update"]).output().unwrap();
    assert!(!output.status.success());
    let value: serde_json::Value = serde_json::from_slice(&output.stderr).unwrap();
    assert_eq!(value["command"], "update");
    assert_eq!(value["error"]["code"], "invalid_argument");
    assert!(!root.join("runtime").exists());
    fs::remove_dir_all(root).unwrap();
}

#[cfg(target_os = "linux")]
#[test]
fn graceful_restart_executes_an_atomically_replaced_installed_inode() {
    let root = fixture();
    let bin = root.join("home/.local/bin");
    fs::create_dir_all(&bin).unwrap();
    fs::create_dir_all(root.join("runtime")).unwrap();
    let installed = bin.join("boomux");
    let candidate = bin.join("candidate");
    let backup = bin.join("backup");
    fs::copy(env!("CARGO_BIN_EXE_boomux"), &installed).unwrap();
    fs::copy(env!("CARGO_BIN_EXE_boomux"), &candidate).unwrap();
    OpenOptions::new()
        .append(true)
        .open(&candidate)
        .unwrap()
        .write_all(b"boomux-native-update-candidate")
        .unwrap();
    fs::set_permissions(&installed, fs::Permissions::from_mode(0o755)).unwrap();
    fs::set_permissions(&candidate, fs::Permissions::from_mode(0o755)).unwrap();
    let old_digest = executable_digest(&installed);
    let candidate_digest = executable_digest(&candidate);
    assert_ne!(candidate_digest, old_digest);

    let mut daemon = command_with_executable(&root, &installed);
    daemon
        .args(["daemon", "run"])
        .env("SHELL", "/bin/sh")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let mut old_process = daemon.spawn().unwrap();
    let client = Client::from_socket_path(root.join("runtime/boomux/daemon.sock"));
    wait_until(|| client.ping().is_ok(), "temporary daemon did not start");
    let old_pid = client.daemon_peer_credentials().unwrap().pid;

    fs::hard_link(&installed, &backup).unwrap();
    File::open(&bin).unwrap().sync_all().unwrap();
    fs::rename(&candidate, &installed).unwrap();
    File::open(&bin).unwrap().sync_all().unwrap();
    client.restart().unwrap();

    wait_until(
        || old_process.try_wait().unwrap().is_some(),
        "old daemon process did not exit after handoff",
    );
    let new_pid = wait_for_replacement_pid(&installed, old_pid);
    assert_ne!(new_pid, old_pid);
    let persistent_connection =
        UnixStream::connect(root.join("runtime/boomux/daemon.sock")).unwrap();
    assert_eq!(client.daemon_peer_credentials().unwrap().pid, old_pid);
    assert_eq!(client.daemon_process_credentials().unwrap().pid, new_pid);
    let status = command_with_executable(&root, &installed)
        .args(["daemon", "status", "--json"])
        .output()
        .unwrap();
    assert!(status.status.success());
    let status: serde_json::Value = serde_json::from_slice(&status.stdout).unwrap();
    assert_eq!(status["data"]["pid"], new_pid);
    assert_eq!(
        status["data"]["executable"],
        installed.to_string_lossy().as_ref()
    );
    drop(persistent_connection);
    let installed_metadata = fs::metadata(&installed).unwrap();
    let daemon_metadata = fs::metadata(format!("/proc/{new_pid}/exe")).unwrap();
    assert_eq!(daemon_metadata.dev(), installed_metadata.dev());
    assert_eq!(daemon_metadata.ino(), installed_metadata.ino());
    assert_eq!(daemon_metadata.len(), installed_metadata.len());
    assert_eq!(executable_digest(&installed), candidate_digest);
    assert_eq!(
        executable_digest(Path::new(&format!("/proc/{new_pid}/exe"))),
        candidate_digest
    );
    let cmdline = fs::read(format!("/proc/{new_pid}/cmdline")).unwrap();
    let arguments = cmdline
        .split(|byte| *byte == 0)
        .filter(|argument| !argument.is_empty())
        .collect::<Vec<_>>();
    assert_eq!(arguments.len(), 5);
    assert_eq!(arguments[0], installed.as_os_str().as_encoded_bytes());
    assert_eq!(arguments[1], b"daemon");
    assert_eq!(arguments[2], b"receive-handoff");
    assert_eq!(arguments[3], b"--channel");

    fs::remove_file(&backup).unwrap();
    File::open(&bin).unwrap().sync_all().unwrap();
    client.shutdown().unwrap();
    fs::remove_dir_all(root).unwrap();
}

use std::fs;
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use boomux::client::Client;
use boomux::federation::{
    FEDERATION_VERSION, FederationConnectionMode, FederationHandshake, write_handshake,
};
use boomux::protocol::{self, Envelope, Request, Response};
use boomux::ssh_bootstrap::{
    REMOTE_INSTALL_ACTIVATE_COMMAND, REMOTE_INSTALL_COMMAND, REMOTE_INSTALL_ROLLBACK_COMMAND,
};
use sha2::{Digest, Sha256};
use uuid::Uuid;

const CONTROL_MASTER_PREFIX: &str = "control=; previous=; master=false; check=false; last=; for arg do last=$arg; case \"$previous\" in -S) control=$arg ;; -O) [ \"$arg\" = check ] && check=true ;; esac; case \"$arg\" in ControlPath=*) control=${arg#ControlPath=} ;; -N) master=true ;; esac; previous=$arg; done; if $master; then : > \"$control.ready\"; trap 'rm -f \"$control.ready\"' EXIT HUP INT TERM; while :; do sleep 60; done; fi; if $check; then [ -e \"$control.ready\" ]; exit; fi; case \"$last\" in *'boomux-install-activation-v1'*) cat >/dev/null; printf 'boomux-install-activation-v1\\0activated\\0'; exit ;; *'boomux-install-transaction-v1'*) IFS= read -r boomux_test_txn; printf() { case \"$1\" in boomux-install-transaction-v1*) command printf 'boomux-install-transaction-v1\\0%s\\0' \"$boomux_test_txn\" ;; *) command printf \"$@\" ;; esac; } ;; *'prior_daemon.next'*|*': > \"$transaction/daemon_contacted\"'*|*'lease.next'*) cat >/dev/null; exit ;; esac";

fn shell_printf(bytes: &[u8]) -> String {
    let escaped = bytes
        .iter()
        .map(|byte| format!("\\{byte:03o}"))
        .collect::<String>();
    format!("printf '{escaped}'")
}

fn fake_ssh(
    directory: &Path,
    executables: &str,
    disconnect_second_helper: bool,
) -> std::path::PathBuf {
    let handshake = FederationHandshake {
        version: FEDERATION_VERSION,
        node_id: "550e8400-e29b-41d4-a716-446655440000".into(),
        helper_version: env!("CARGO_PKG_VERSION").into(),
        core_protocol_version: protocol::PROTOCOL_VERSION,
        connection_mode: FederationConnectionMode::AdHoc,
    };
    let mut handshake_bytes = Vec::new();
    write_handshake(&mut handshake_bytes, &handshake).unwrap();
    let mut request = Vec::new();
    protocol::write_message(
        &mut request,
        &Envelope::with_version(protocol::PROTOCOL_VERSION, Request::Ping),
    )
    .unwrap();
    let mut response = Vec::new();
    protocol::write_message(
        &mut response,
        &Envelope::with_version(protocol::PROTOCOL_VERSION, Response::Pong),
    )
    .unwrap();
    let log = directory.join("ssh.log");
    let count = directory.join("helper.count");
    let ssh = directory.join("ssh");
    let helper = if disconnect_second_helper {
        format!(
            "count=0; [ ! -f '{}' ] || count=$(cat '{}'); count=$((count + 1)); printf '%s' \"$count\" > '{}'; {}; dd bs=1 count={} of=/dev/null 2>/dev/null; [ \"$count\" -lt 2 ] && {}",
            count.display(),
            count.display(),
            count.display(),
            shell_printf(&handshake_bytes),
            request.len(),
            shell_printf(&response),
        )
    } else {
        format!(
            "{}; dd bs=1 count={} of=/dev/null 2>/dev/null; {}",
            shell_printf(&handshake_bytes),
            request.len(),
            shell_printf(&response),
        )
    };
    fs::write(
        &ssh,
        format!(
            "#!/bin/sh\n{CONTROL_MASTER_PREFIX}\nlast=\nfor arg do last=$arg; done\nprintf '%s|%s\\n' \"$control\" \"$last\" >> '{}'\ncase \"$last\" in *'exec '*) last=${{last##*exec }} ;; esac\ncase \"$last\" in\n  *boomux-platform-v1*) printf 'boomux-platform-v1\\0Linux\\0x86_64\\0' ;;\n  *boomux-executables-v1*) printf 'boomux-executables-v1\\0{}' ;;\n  *boomux-install-destination-v1*) printf 'boomux-install-destination-v1\\0/home/remote/.local/bin/boomux\\0' ;;\n  \"'/remote/boomux' __federation-stdio\") {} ;;\n  *) exit 64 ;;\nesac\n",
            log.display(),
            executables,
            helper,
        ),
    )
    .unwrap();
    fs::set_permissions(&ssh, fs::Permissions::from_mode(0o700)).unwrap();
    ssh
}

fn fake_old_ssh(directory: &Path) {
    let log = directory.join("ssh.log");
    let ssh = directory.join("ssh");
    fs::write(
        &ssh,
        format!(
            "#!/bin/sh\n{CONTROL_MASTER_PREFIX}\nlast=\nfor arg do last=$arg; done\nprintf '%s|%s\\n' \"$control\" \"$last\" >> '{}'\ncase \"$last\" in *'exec '*) last=${{last##*exec }} ;; esac\ncase \"$last\" in\n  *boomux-platform-v1*) printf 'boomux-platform-v1\\0Linux\\0x86_64\\0' ;;\n  *boomux-executables-v1*) printf 'boomux-executables-v1\\0/remote/boomux\\0' ;;\n  *boomux-install-destination-v1*) printf 'boomux-install-destination-v1\\0/home/remote/.local/bin/boomux\\0' ;;\n  \"'/remote/boomux' __federation-stdio\") exit 2 ;;\n  \"'/remote/boomux' --version\") printf 'boomux 0.14.2\\n' ;;\n  *) exit 64 ;;\nesac\n",
            log.display(),
        ),
    )
    .unwrap();
    fs::set_permissions(&ssh, fs::Permissions::from_mode(0o700)).unwrap();
}

fn fake_auth_eof_ssh(directory: &Path) {
    let log = directory.join("ssh.log");
    let ssh = directory.join("ssh");
    fs::write(
        &ssh,
        format!(
            "#!/bin/sh\n{CONTROL_MASTER_PREFIX}\nlast=\nfor arg do last=$arg; done\nprintf '%s|%s\\n' \"$control\" \"$last\" >> '{}'\ncase \"$last\" in *'exec '*) last=${{last##*exec }} ;; esac\ncase \"$last\" in\n  *boomux-platform-v1*) printf 'boomux-platform-v1\\0Linux\\0x86_64\\0' ;;\n  *boomux-executables-v1*) printf 'boomux-executables-v1\\0/remote/boomux\\0' ;;\n  *boomux-install-destination-v1*) printf 'boomux-install-destination-v1\\0/home/remote/.local/bin/boomux\\0' ;;\n  \"'/remote/boomux' __federation-stdio\" | \"'/remote/boomux' --version\") printf 'Permission denied (publickey).\\n' >&2; exit 255 ;;\n  *) exit 64 ;;\nesac\n",
            log.display(),
        ),
    )
    .unwrap();
    fs::set_permissions(&ssh, fs::Permissions::from_mode(0o700)).unwrap();
}

fn fake_malformed_helper_ssh(directory: &Path) {
    let ssh = directory.join("ssh");
    fs::write(
        &ssh,
        format!(
            "#!/bin/sh\n{CONTROL_MASTER_PREFIX}\nlast=\nfor arg do last=$arg; done\ncase \"$last\" in *'exec '*) last=${{last##*exec }} ;; esac\ncase \"$last\" in\n  *boomux-platform-v1*) printf 'boomux-platform-v1\\0Linux\\0x86_64\\0' ;;\n  *boomux-executables-v1*) printf 'boomux-executables-v1\\0/remote/boomux\\0' ;;\n  *boomux-install-destination-v1*) printf 'boomux-install-destination-v1\\0/home/remote/.local/bin/boomux\\0' ;;\n  \"'/remote/boomux' __federation-stdio\") printf 'NOTMAGIC' ;;\n  \"'/remote/boomux' --version\") printf 'boomux 0.14.2\\n' ;;\n  *) exit 64 ;;\nesac\n"
        ),
    )
    .unwrap();
    fs::set_permissions(&ssh, fs::Permissions::from_mode(0o700)).unwrap();
}

fn fake_runtime_upgrade_ssh(directory: &Path) {
    let handshake = FederationHandshake {
        version: FEDERATION_VERSION,
        node_id: "550e8400-e29b-41d4-a716-446655440000".into(),
        helper_version: env!("CARGO_PKG_VERSION").into(),
        core_protocol_version: protocol::PROTOCOL_VERSION,
        connection_mode: FederationConnectionMode::AdHoc,
    };
    let mut handshake_bytes = Vec::new();
    write_handshake(&mut handshake_bytes, &handshake).unwrap();
    let mut request = Vec::new();
    protocol::write_message(
        &mut request,
        &Envelope::with_version(protocol::PROTOCOL_VERSION, Request::Ping),
    )
    .unwrap();
    let mut response = Vec::new();
    protocol::write_message(
        &mut response,
        &Envelope::with_version(protocol::PROTOCOL_VERSION, Response::Pong),
    )
    .unwrap();
    let installed = directory.join("installed");
    let helper_calls = directory.join("helper-calls");
    let restarted = directory.join("restarted");
    let committed = directory.join("committed");
    let socket = directory.join("remote-runtime/boomux/daemon.sock");
    fs::create_dir_all(socket.parent().unwrap()).unwrap();
    fs::write(&socket, b"socket").unwrap();
    let ssh = directory.join("ssh");
    let script = format!(
        "#!/bin/sh\n{CONTROL_MASTER_PREFIX}\nunset XDG_RUNTIME_DIR\nlast=\nfor arg do last=$arg; done\nrequire_runtime() {{ case \"$last\" in *boomux-runtime-v1*'/run/user/$boomux_uid'*) [ -e {socket} ] ;; *) return 1 ;; esac; }}\ncase \"$last\" in\n  *boomux-platform-v1*) printf 'boomux-platform-v1\\0Linux\\0x86_64\\0' ;;\n  *boomux-executables-v1*) printf 'boomux-executables-v1\\0/remote/boomux\\0' ;;\n  *boomux-install-destination-v1*) printf 'boomux-install-destination-v1\\0/home/remote/.local/bin/boomux\\0' ;;\n  *'boomux-install-transaction-v1'*) cat >/dev/null; : > {installed}; printf 'boomux-install-transaction-v1\\0.boomux.bootstrap.ABC12345\\0' ;;\n  *': > \"$transaction/restarted\"'*) cat >/dev/null; : > {restarted} ;;\n  *'committed=$lock/committed'*) cat >/dev/null; [ -e {restarted} ]; : > {committed}; printf 'boomux-install-commit-v1\\0committed\\0' ;;\n  *'daemon status --json'*) require_runtime || exit 97; printf '%s' '{{\"schema\":\"boomux.cli/v1\",\"command\":\"daemon.status\",\"data\":{{\"protocol_version\":21}}}}' ;;\n  *'daemon restart'*) require_runtime || exit 97; printf 'restart\\n' >> {restarted} ;;\n  *\"'/home/remote/.local/bin/boomux' __federation-stdio\" | *\"'/remote/boomux' __federation-stdio\") n=0; [ ! -e {helper_calls} ] || n=$(cat {helper_calls}); n=$((n + 1)); printf '%s' \"$n\" > {helper_calls}; if [ ! -e {installed} ]; then exit 2; fi; require_runtime || exit 97; {handshake}; limit=1; [ \"$n\" -ge 4 ] && limit=2; i=0; while [ \"$i\" -lt \"$limit\" ]; do dd bs=1 count={request_len} of=/dev/null 2>/dev/null || exit; {pong}; i=$((i + 1)); done ;;\n  \"'/remote/boomux' --version\") printf 'boomux 0.14.2\\n' ;;\n  *) exit 64 ;;\nesac\n",
        socket = socket.display(),
        installed = installed.display(),
        helper_calls = helper_calls.display(),
        restarted = restarted.display(),
        committed = committed.display(),
        handshake = shell_printf(&handshake_bytes),
        request_len = request.len(),
        pong = shell_printf(&response),
    );
    let script = script.replace("/remote/boomux", "/home/remote/.local/bin/boomux");
    let script = script.replace(
        "*'daemon status --json'*) require_runtime || exit 97; printf '%s' '{\"schema\":\"boomux.cli/v1\",\"command\":\"daemon.status\",\"data\":{\"protocol_version\":21}}' ;;",
        "*'.boomux.bootstrap.'*'daemon status --json'*) require_runtime || exit 97; printf '%s' '{\"schema\":\"boomux.cli/v1\",\"command\":\"daemon.status\",\"data\":{\"protocol_version\":21,\"pid\":123,\"executable\":\"/home/remote/.local/bin/boomux\",\"socket_device\":1,\"socket_inode\":1}}' ;;\n  *'daemon status --json'*) require_runtime || exit 97; printf '%s' '{\"schema\":\"boomux.cli/v1\",\"command\":\"daemon.status\",\"data\":{\"protocol_version\":21}}' ;;",
    );
    assert!(script.contains("\"pid\":123"));
    let script = script.replace("limit=1; [ \"$n\" -ge 4 ] && limit=2;", "limit=1;");
    let script = script.replace(
        &format!(": > {}", committed.display()),
        &format!(
            "/bin/cat {} > {}",
            helper_calls.display(),
            committed.display()
        ),
    );
    fs::write(&ssh, script).unwrap();
    fs::set_permissions(&ssh, fs::Permissions::from_mode(0o700)).unwrap();
}

fn fake_challenge_ssh(directory: &Path) {
    let ssh = directory.join("ssh");
    fs::write(
        &ssh,
        "#!/bin/sh\ncontrol=; previous=; master=false; check=false\nfor arg do\n  case \"$previous\" in -S) control=$arg ;; -O) [ \"$arg\" = check ] && check=true ;; esac\n  case \"$arg\" in ControlPath=*) control=${arg#ControlPath=} ;; -N) master=true ;; esac\n  previous=$arg\ndone\nif $master; then\n  printf 'To auth' >&2\n  printf 'enticate, visit:\nhttps://login.tailscale.test/challenge\n' >&2\n  : > \"$control.ready\"\n  trap 'rm -f \"$control.ready\"' EXIT HUP INT TERM\n  while :; do sleep 60; done\nfi\nif $check; then [ -e \"$control.ready\" ]; exit; fi\nexit 64\n",
    )
    .unwrap();
    fs::set_permissions(&ssh, fs::Permissions::from_mode(0o700)).unwrap();
}

fn command(directory: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_boomux"));
    command
        .args(["--remote", "workbox"])
        .env("PATH", format!("{}:/usr/bin:/bin", directory.display()))
        .env("HOME", directory.join("home"))
        .env("XDG_RUNTIME_DIR", directory.join("runtime"))
        .env("XDG_STATE_HOME", directory.join("state"));
    command
}

fn test_directory() -> std::path::PathBuf {
    let id = Uuid::new_v4().simple().to_string();
    std::env::temp_dir().join(format!("bx-r-{}-{}", std::process::id(), &id[..8]))
}

fn run_interactive(
    directory: &Path,
    arguments: &[&str],
    input: &[u8],
) -> (std::process::ExitStatus, Vec<u8>) {
    let mut master = 0;
    let mut slave = 0;
    assert_eq!(
        unsafe {
            libc::openpty(
                &mut master,
                &mut slave,
                std::ptr::null_mut(),
                std::ptr::null(),
                std::ptr::null(),
            )
        },
        0
    );
    let master = unsafe { OwnedFd::from_raw_fd(master) };
    let slave = unsafe { OwnedFd::from_raw_fd(slave) };
    let mut command = Command::new(env!("CARGO_BIN_EXE_boomux"));
    command
        .args(arguments)
        .env("PATH", format!("{}:/usr/bin:/bin", directory.display()))
        .env("HOME", directory.join("home"))
        .env("XDG_RUNTIME_DIR", directory.join("runtime"))
        .env("XDG_STATE_HOME", directory.join("state"))
        .stdin(Stdio::from(slave.try_clone().unwrap()))
        .stdout(Stdio::from(slave.try_clone().unwrap()))
        .stderr(Stdio::from(slave));
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 || libc::ioctl(0, libc::TIOCSCTTY, 0) == -1 {
                Err(std::io::Error::last_os_error())
            } else {
                Ok(())
            }
        });
    }
    let mut child = command.spawn().unwrap();
    let mut master = fs::File::from(master);
    master.write_all(input).unwrap();
    let flags = unsafe { libc::fcntl(master.as_raw_fd(), libc::F_GETFL) };
    assert_ne!(flags, -1);
    assert_ne!(
        unsafe { libc::fcntl(master.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) },
        -1
    );
    let mut output = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let mut bytes = [0_u8; 4096];
        match master.read(&mut bytes) {
            Ok(count) => output.extend_from_slice(&bytes[..count]),
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(error) if error.raw_os_error() == Some(libc::EIO) => {}
            Err(error) => panic!("PTY read failed: {error}"),
        }
        if let Some(status) = child.try_wait().unwrap() {
            return (status, output);
        }
        assert!(
            Instant::now() < deadline,
            "interactive command timed out: {}",
            String::from_utf8_lossy(&output)
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn run_transaction_script(
    script: &str,
    home: &Path,
    runtime: &Path,
    input: &[u8],
) -> std::process::Output {
    let mut child = Command::new("sh")
        .args(["-c", script])
        .env("HOME", home)
        .env("XDG_RUNTIME_DIR", runtime)
        .env("XDG_STATE_HOME", home.join("state"))
        .env("PATH", "/usr/bin:/bin")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(input).unwrap();
    child.wait_with_output().unwrap()
}

#[cfg(target_os = "linux")]
fn executable_digest(path: &Path) -> Vec<u8> {
    let mut file = fs::File::open(path).unwrap();
    let mut digest = Sha256::new();
    let mut bytes = [0_u8; 64 * 1024];
    loop {
        let count = file.read(&mut bytes).unwrap();
        if count == 0 {
            return digest.finalize().to_vec();
        }
        digest.update(&bytes[..count]);
    }
}

#[cfg(target_os = "linux")]
fn daemon_pid_for_executable(executable: &Path, excluded: &[u32]) -> Option<u32> {
    fs::read_dir("/proc")
        .ok()?
        .filter_map(Result::ok)
        .filter_map(|entry| entry.file_name().to_str()?.parse::<u32>().ok())
        .filter(|pid| !excluded.contains(pid))
        .find(|pid| fs::read_link(format!("/proc/{pid}/exe")).is_ok_and(|path| path == executable))
}

#[cfg(target_os = "linux")]
fn wait_for_daemon_executable(executable: &Path, excluded: &[u32]) -> u32 {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(pid) = daemon_pid_for_executable(executable, excluded) {
            return pid;
        }
        assert!(
            Instant::now() < deadline,
            "daemon did not execute {}",
            executable.display()
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn staged_missing_install_refuses_socket_that_appears_before_activation() {
    let directory = test_directory();
    let runtime = directory.join("runtime");
    fs::create_dir_all(&runtime).unwrap();
    fs::set_permissions(&runtime, fs::Permissions::from_mode(0o700)).unwrap();
    fs::create_dir_all(directory.join(".local/bin")).unwrap();
    let transaction = ".boomux.bootstrap.MISS1234";
    let mut upload = format!("{transaction}\n").into_bytes();
    upload.extend_from_slice(&fs::read(env!("CARGO_BIN_EXE_boomux")).unwrap());
    let output = run_transaction_script(REMOTE_INSTALL_COMMAND, &directory, &runtime, &upload);
    assert!(output.status.success());
    assert!(!directory.join(".local/bin/boomux").exists());

    let socket = runtime.join("boomux/daemon.sock");
    fs::create_dir_all(socket.parent().unwrap()).unwrap();
    fs::write(&socket, b"stale-or-racing-daemon").unwrap();
    let activation = run_transaction_script(
        REMOTE_INSTALL_ACTIVATE_COMMAND,
        &directory,
        &runtime,
        format!("{transaction}\nmissing\nabsent\n\n\n\n\n").as_bytes(),
    );
    assert!(activation.status.success());
    assert_eq!(
        activation.stdout,
        b"boomux-install-activation-v1\0daemon_present\0"
    );
    assert!(!directory.join(".local/bin/boomux").exists());
    let rollback = run_transaction_script(
        REMOTE_INSTALL_ROLLBACK_COMMAND,
        &directory,
        &runtime,
        format!("{transaction}\n").as_bytes(),
    );
    assert!(rollback.status.success());
    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn staged_upload_and_activation_recover_lost_acknowledgments() {
    let directory = test_directory();
    let runtime = directory.join("runtime");
    fs::create_dir_all(&runtime).unwrap();
    fs::set_permissions(&runtime, fs::Permissions::from_mode(0o700)).unwrap();
    fs::create_dir_all(directory.join(".local/bin")).unwrap();
    let destination = directory.join(".local/bin/boomux");
    fs::write(&destination, b"previous").unwrap();
    fs::set_permissions(&destination, fs::Permissions::from_mode(0o755)).unwrap();
    let transaction = ".boomux.bootstrap.ACKS1234";
    let replacement = fs::read(env!("CARGO_BIN_EXE_boomux")).unwrap();
    let mut upload = format!("{transaction}\n").into_bytes();
    upload.extend_from_slice(&replacement);

    let lost_upload_ack = format!("{REMOTE_INSTALL_COMMAND}; exit 255");
    let first = run_transaction_script(&lost_upload_ack, &directory, &runtime, &upload);
    assert!(!first.status.success());
    assert_eq!(fs::read(&destination).unwrap(), b"previous");
    let retry = run_transaction_script(REMOTE_INSTALL_COMMAND, &directory, &runtime, &upload);
    assert!(retry.status.success());

    let activation_input = format!("{transaction}\nmissing\nabsent\n\n\n\n\n");
    let lost_activation_ack = format!("{REMOTE_INSTALL_ACTIVATE_COMMAND}; exit 255");
    let first = run_transaction_script(
        &lost_activation_ack,
        &directory,
        &runtime,
        activation_input.as_bytes(),
    );
    assert!(!first.status.success());
    assert_eq!(fs::read(&destination).unwrap(), replacement);
    let retry = run_transaction_script(
        REMOTE_INSTALL_ACTIVATE_COMMAND,
        &directory,
        &runtime,
        activation_input.as_bytes(),
    );
    assert!(retry.status.success());
    assert_eq!(retry.stdout, b"boomux-install-activation-v1\0activated\0");
    let rollback = run_transaction_script(
        REMOTE_INSTALL_ROLLBACK_COMMAND,
        &directory,
        &runtime,
        format!("{transaction}\n").as_bytes(),
    );
    assert!(rollback.status.success());
    assert_eq!(fs::read(&destination).unwrap(), b"previous");
    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn uploaded_only_watchdog_cleans_transaction_without_touching_destination() {
    let directory = test_directory();
    let runtime = directory.join("runtime");
    fs::create_dir_all(&runtime).unwrap();
    fs::set_permissions(&runtime, fs::Permissions::from_mode(0o700)).unwrap();
    fs::create_dir_all(directory.join(".local/bin")).unwrap();
    let destination = directory.join(".local/bin/boomux");
    fs::write(&destination, b"previous").unwrap();
    fs::set_permissions(&destination, fs::Permissions::from_mode(0o755)).unwrap();
    let transaction = ".boomux.bootstrap.WDOG1234";
    let mut upload = format!("{transaction}\n").into_bytes();
    upload.extend_from_slice(&fs::read(env!("CARGO_BIN_EXE_boomux")).unwrap());
    let command = REMOTE_INSTALL_COMMAND.replace("lease_limit=180", "lease_limit=1");
    let output = run_transaction_script(&command, &directory, &runtime, &upload);
    assert!(output.status.success());
    let lock = directory.join(".local/bin/.boomux.bootstrap.lock");
    let deadline = Instant::now() + Duration::from_secs(3);
    while lock.exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(!lock.exists());
    assert_eq!(fs::read(&destination).unwrap(), b"previous");
    assert!(!directory.join(".local/bin").join(transaction).exists());
    fs::remove_dir_all(directory).unwrap();
}

#[cfg(target_os = "linux")]
#[test]
fn live_destination_replacement_and_rollback_select_the_correct_daemon_binary() {
    let directory = test_directory();
    let runtime = directory.join("runtime");
    let state = directory.join("state");
    let bin = directory.join(".local/bin");
    let destination = bin.join("boomux");
    fs::create_dir_all(&runtime).unwrap();
    fs::set_permissions(&runtime, fs::Permissions::from_mode(0o700)).unwrap();
    fs::create_dir_all(&state).unwrap();
    fs::create_dir_all(&bin).unwrap();

    let old_bytes = fs::read(env!("CARGO_BIN_EXE_boomux")).unwrap();
    fs::write(&destination, &old_bytes).unwrap();
    fs::set_permissions(&destination, fs::Permissions::from_mode(0o755)).unwrap();
    let old_digest = executable_digest(&destination);
    let mut replacement_bytes = old_bytes;
    replacement_bytes.extend_from_slice(b"\nboomux-native-replacement-fixture\n");
    let replacement_digest = Sha256::digest(&replacement_bytes).to_vec();
    assert_ne!(old_digest, replacement_digest);

    let mut old_daemon = Command::new(&destination)
        .args(["daemon", "run"])
        .env("HOME", &directory)
        .env("XDG_RUNTIME_DIR", &runtime)
        .env("XDG_STATE_HOME", &state)
        .env("SHELL", "/bin/sh")
        .env("BOOMUX_NATIVE_TEST_HOOKS", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let old_pid = old_daemon.id();
    let client = Client::from_socket_path(runtime.join("boomux/daemon.sock"));
    let deadline = Instant::now() + Duration::from_secs(10);
    while client.ping().is_err() {
        assert!(
            Instant::now() < deadline,
            "old fixture daemon did not start"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(
        fs::read_link(format!("/proc/{old_pid}/exe")).unwrap(),
        destination
    );

    let transaction = ".boomux.bootstrap.ABC12345";
    let mut upload_input = format!("{transaction}\n").into_bytes();
    upload_input.extend_from_slice(&replacement_bytes);
    let install =
        run_transaction_script(REMOTE_INSTALL_COMMAND, &directory, &runtime, &upload_input);
    assert!(
        install.status.success(),
        "install failed: {}",
        String::from_utf8_lossy(&install.stderr)
    );
    let fields = install.stdout.split(|byte| *byte == 0).collect::<Vec<_>>();
    assert_eq!(
        fields.first().copied(),
        Some(b"boomux-install-transaction-v1".as_slice())
    );
    assert_eq!(fields.get(1).copied(), Some(transaction.as_bytes()));
    let transaction_input = format!("{transaction}\n");
    let backup = bin.join(transaction).join("backup");
    assert_eq!(executable_digest(&destination), old_digest);
    assert!(!backup.exists());
    let socket_metadata = fs::metadata(runtime.join("boomux/daemon.sock")).unwrap();
    let activation_input = format!(
        "{transaction}\nupgrade\n{}\n{old_pid}\n{}\n{}\n{}\n",
        protocol::PROTOCOL_VERSION,
        destination.display(),
        socket_metadata.dev(),
        socket_metadata.ino()
    );
    let activation = run_transaction_script(
        REMOTE_INSTALL_ACTIVATE_COMMAND,
        &directory,
        &runtime,
        activation_input.as_bytes(),
    );
    assert!(
        activation.status.success(),
        "activation failed: {}",
        String::from_utf8_lossy(&activation.stderr)
    );
    assert_eq!(executable_digest(&backup), old_digest);
    assert_eq!(executable_digest(&destination), replacement_digest);
    assert_eq!(
        fs::read_link(format!("/proc/{old_pid}/exe")).unwrap(),
        std::path::PathBuf::from(format!("{} (deleted)", destination.display()))
    );

    fs::write(
        bin.join(transaction).join("prior_daemon"),
        protocol::PROTOCOL_VERSION.to_string(),
    )
    .unwrap();
    fs::write(bin.join(transaction).join("daemon_contacted"), b"").unwrap();
    let restart = Command::new(&destination)
        .args(["daemon", "restart"])
        .env("HOME", &directory)
        .env("XDG_RUNTIME_DIR", &runtime)
        .env("XDG_STATE_HOME", &state)
        .env("BOOMUX_NATIVE_TEST_HOOKS", "1")
        .output()
        .unwrap();
    assert!(
        restart.status.success(),
        "replacement restart failed: {}",
        String::from_utf8_lossy(&restart.stderr)
    );
    assert!(old_daemon.wait().unwrap().success());
    let replacement_pid = wait_for_daemon_executable(&destination, &[old_pid]);
    assert_eq!(
        executable_digest(Path::new(&format!("/proc/{replacement_pid}/exe"))),
        replacement_digest
    );
    let deadline = Instant::now() + Duration::from_secs(10);
    while client.ping().is_err() {
        assert!(
            Instant::now() < deadline,
            "replacement daemon did not accept requests"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    std::thread::sleep(Duration::from_millis(200));

    let rollback = run_transaction_script(
        REMOTE_INSTALL_ROLLBACK_COMMAND,
        &directory,
        &runtime,
        transaction_input.as_bytes(),
    );
    assert!(
        rollback.status.success(),
        "rollback failed: {}",
        String::from_utf8_lossy(&rollback.stderr)
    );
    assert_eq!(executable_digest(&destination), old_digest);
    let restored_pid = wait_for_daemon_executable(&destination, &[old_pid, replacement_pid]);
    assert_eq!(
        executable_digest(Path::new(&format!("/proc/{restored_pid}/exe"))),
        old_digest
    );

    let stop = Command::new(&destination)
        .args(["daemon", "stop"])
        .env("HOME", &directory)
        .env("XDG_RUNTIME_DIR", &runtime)
        .env("XDG_STATE_HOME", &state)
        .output()
        .unwrap();
    assert!(stop.status.success());
    let deadline = Instant::now() + Duration::from_secs(10);
    while Path::new(&format!("/proc/{restored_pid}")).exists() {
        assert!(Instant::now() < deadline, "restored daemon did not stop");
        std::thread::sleep(Duration::from_millis(20));
    }
    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn public_remote_uses_verified_stdio_protocol_channel() {
    let directory = test_directory();
    fs::create_dir_all(directory.join("home/.ssh")).unwrap();
    fs::create_dir_all(directory.join("runtime")).unwrap();
    fake_ssh(&directory, "/remote/boomux\\0", false);

    let output = command(&directory).output().unwrap();
    assert!(
        output.status.success(),
        "remote command failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("Connected to Boomux Node 550e8400-e29b-41d4-a716-446655440000"));
    let log = fs::read_to_string(directory.join("ssh.log")).unwrap();
    assert_eq!(log.matches("__federation-stdio").count(), 2);
    let control_paths = log
        .lines()
        .map(|line| line.split_once('|').unwrap().0)
        .collect::<std::collections::HashSet<_>>();
    assert_eq!(control_paths.len(), 1);
    assert!(
        fs::read_dir(directory.join("runtime/boomux"))
            .unwrap()
            .filter_map(Result::ok)
            .all(|entry| !entry.file_name().to_string_lossy().starts_with("ssh-"))
    );
    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn noninteractive_remote_refuses_install_without_modification() {
    let directory = test_directory();
    fs::create_dir_all(directory.join("home/.ssh")).unwrap();
    fs::create_dir_all(directory.join("runtime")).unwrap();
    fake_ssh(&directory, "", false);

    let output = command(&directory).output().unwrap();
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("remote Boomux installation is required")
    );
    let log = fs::read_to_string(directory.join("ssh.log")).unwrap();
    assert_eq!(log.lines().count(), 3);
    assert!(!log.contains("mktemp"));
    assert!(!log.contains("daemon restart"));
    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn json_node_add_reports_install_required_without_remote_mutation() {
    let directory = test_directory();
    fs::create_dir_all(directory.join("home/.ssh")).unwrap();
    fs::create_dir_all(directory.join("runtime")).unwrap();
    fake_ssh(&directory, "", false);

    let output = Command::new(env!("CARGO_BIN_EXE_boomux"))
        .args(["node", "add", "work", "workbox", "--json"])
        .env("PATH", format!("{}:/usr/bin:/bin", directory.display()))
        .env("HOME", directory.join("home"))
        .env("XDG_RUNTIME_DIR", directory.join("runtime"))
        .env("XDG_STATE_HOME", directory.join("state"))
        .output()
        .unwrap();
    assert!(!output.status.success());
    let error: serde_json::Value = serde_json::from_slice(&output.stderr).unwrap();
    assert_eq!(error["command"], "node.add");
    assert_eq!(error["error"]["code"], "install_required");
    assert!(
        error["error"]["message"]
            .as_str()
            .unwrap()
            .contains("interactive terminal")
    );
    let log = fs::read_to_string(directory.join("ssh.log")).unwrap();
    assert_eq!(log.lines().count(), 3);
    assert!(!log.contains("mktemp"));
    assert!(!log.contains("daemon restart"));
    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn json_node_add_reports_upgrade_required_without_remote_mutation() {
    let directory = test_directory();
    fs::create_dir_all(directory.join("home/.ssh")).unwrap();
    fs::create_dir_all(directory.join("runtime")).unwrap();
    fake_old_ssh(&directory);

    let output = Command::new(env!("CARGO_BIN_EXE_boomux"))
        .args(["node", "add", "work", "workbox", "--json"])
        .env("PATH", format!("{}:/usr/bin:/bin", directory.display()))
        .env("HOME", directory.join("home"))
        .env("XDG_RUNTIME_DIR", directory.join("runtime"))
        .env("XDG_STATE_HOME", directory.join("state"))
        .output()
        .unwrap();
    assert!(!output.status.success());
    let error: serde_json::Value = serde_json::from_slice(&output.stderr).unwrap();
    assert_eq!(error["error"]["code"], "upgrade_required");
    let log = fs::read_to_string(directory.join("ssh.log")).unwrap();
    assert_eq!(log.lines().count(), 5);
    assert!(!log.contains("mktemp"));
    assert!(!log.contains("daemon restart"));
    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn json_node_add_classifies_authenticated_route_eof_without_mutation() {
    let directory = test_directory();
    fs::create_dir_all(directory.join("home/.ssh")).unwrap();
    fs::create_dir_all(directory.join("runtime")).unwrap();
    fake_auth_eof_ssh(&directory);

    let output = Command::new(env!("CARGO_BIN_EXE_boomux"))
        .args(["node", "add", "work", "workbox", "--json"])
        .env("PATH", format!("{}:/usr/bin:/bin", directory.display()))
        .env("HOME", directory.join("home"))
        .env("XDG_RUNTIME_DIR", directory.join("runtime"))
        .env("XDG_STATE_HOME", directory.join("state"))
        .output()
        .unwrap();
    assert!(!output.status.success());
    let error: serde_json::Value = serde_json::from_slice(&output.stderr).unwrap();
    assert_eq!(error["error"]["code"], "bootstrap_authentication_failed");
    let log = fs::read_to_string(directory.join("ssh.log")).unwrap();
    assert!(!log.contains("mktemp"));
    assert!(!log.contains("daemon restart"));
    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn json_node_add_classifies_malformed_helper_without_mutation() {
    let directory = test_directory();
    fs::create_dir_all(directory.join("home/.ssh")).unwrap();
    fs::create_dir_all(directory.join("runtime")).unwrap();
    fake_malformed_helper_ssh(&directory);

    let output = Command::new(env!("CARGO_BIN_EXE_boomux"))
        .args(["node", "add", "work", "workbox", "--json"])
        .env("PATH", format!("{}:/usr/bin:/bin", directory.display()))
        .env("HOME", directory.join("home"))
        .env("XDG_RUNTIME_DIR", directory.join("runtime"))
        .env("XDG_STATE_HOME", directory.join("state"))
        .output()
        .unwrap();
    assert!(!output.status.success());
    let error: serde_json::Value = serde_json::from_slice(&output.stderr).unwrap();
    assert_eq!(error["error"]["code"], "bootstrap_malformed_helper");
    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn json_node_add_classifies_invalid_framed_executable_paths_as_malformed_helper() {
    for candidate in [
        "relative/boomux".to_owned(),
        "/opt/boomux\nbin".to_owned(),
        format!("/{}", "x".repeat(4096)),
    ] {
        let directory = test_directory();
        fs::create_dir_all(directory.join("home/.ssh")).unwrap();
        fs::create_dir_all(directory.join("runtime")).unwrap();
        fake_ssh(&directory, &format!("{candidate}\\0"), false);

        let output = Command::new(env!("CARGO_BIN_EXE_boomux"))
            .args(["node", "add", "work", "workbox", "--json"])
            .env("PATH", format!("{}:/usr/bin:/bin", directory.display()))
            .env("HOME", directory.join("home"))
            .env("XDG_RUNTIME_DIR", directory.join("runtime"))
            .env("XDG_STATE_HOME", directory.join("state"))
            .output()
            .unwrap();
        assert!(!output.status.success());
        let error: serde_json::Value = serde_json::from_slice(&output.stderr).unwrap();
        assert_eq!(error["error"]["code"], "bootstrap_malformed_helper");
        fs::remove_dir_all(directory).unwrap();
    }
}

#[test]
fn interactive_upgrade_restarts_old_running_daemon_after_compatible_provisional_helper() {
    let directory = test_directory();
    fs::create_dir_all(directory.join("home/.ssh")).unwrap();
    fs::create_dir_all(directory.join("runtime")).unwrap();
    fake_runtime_upgrade_ssh(&directory);

    let mut master = 0;
    let mut slave = 0;
    assert_eq!(
        unsafe {
            libc::openpty(
                &mut master,
                &mut slave,
                std::ptr::null_mut(),
                std::ptr::null(),
                std::ptr::null(),
            )
        },
        0
    );
    let master = unsafe { OwnedFd::from_raw_fd(master) };
    let slave = unsafe { OwnedFd::from_raw_fd(slave) };
    let mut command = Command::new(env!("CARGO_BIN_EXE_boomux"));
    command
        .args(["--remote", "workbox"])
        .env("PATH", format!("{}:/usr/bin:/bin", directory.display()))
        .env("HOME", directory.join("home"))
        .env("XDG_RUNTIME_DIR", directory.join("runtime"))
        .env("XDG_STATE_HOME", directory.join("state"))
        .stdin(Stdio::from(slave.try_clone().unwrap()))
        .stdout(Stdio::from(slave.try_clone().unwrap()))
        .stderr(Stdio::from(slave));
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 || libc::ioctl(0, libc::TIOCSCTTY, 0) == -1 {
                Err(std::io::Error::last_os_error())
            } else {
                Ok(())
            }
        });
    }
    let mut child = command.spawn().unwrap();
    let mut master = fs::File::from(master);
    master.write_all(b"y\n").unwrap();
    let flags = unsafe { libc::fcntl(master.as_raw_fd(), libc::F_GETFL) };
    assert_ne!(flags, -1);
    assert_ne!(
        unsafe { libc::fcntl(master.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) },
        -1
    );
    let mut output = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(30);
    let status = loop {
        let mut bytes = [0_u8; 4096];
        match master.read(&mut bytes) {
            Ok(count) => output.extend_from_slice(&bytes[..count]),
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(error) if error.raw_os_error() == Some(libc::EIO) => {}
            Err(error) => panic!("PTY read failed: {error}"),
        }
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        assert!(Instant::now() < deadline, "interactive upgrade timed out");
        std::thread::sleep(Duration::from_millis(20));
    };
    assert!(
        status.success(),
        "interactive upgrade failed with {status}: {}",
        String::from_utf8_lossy(&output)
    );
    assert_eq!(
        fs::read_to_string(directory.join("restarted")).unwrap(),
        "restart\n"
    );
    assert!(directory.join("committed").exists());

    let _ = Command::new(env!("CARGO_BIN_EXE_boomux"))
        .args(["daemon", "stop"])
        .env("HOME", directory.join("home"))
        .env("XDG_RUNTIME_DIR", directory.join("runtime"))
        .env("XDG_STATE_HOME", directory.join("state"))
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn upgraded_node_add_registers_after_the_precommit_ping_channel_closes() {
    let directory = test_directory();
    fs::create_dir_all(directory.join("home/.ssh")).unwrap();
    fs::create_dir_all(directory.join("runtime")).unwrap();
    fake_runtime_upgrade_ssh(&directory);

    let (status, output) = run_interactive(&directory, &["node", "add", "work", "workbox"], b"y\n");
    assert!(
        status.success(),
        "upgraded node add failed with {status}: {}",
        String::from_utf8_lossy(&output)
    );
    assert_eq!(
        fs::read_to_string(directory.join("committed")).unwrap(),
        "4"
    );
    let persisted: serde_json::Value = serde_json::from_slice(
        &fs::read(directory.join("state/boomux/node_registrations.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(persisted["registrations"][0]["alias"], "work");
    assert_eq!(persisted["registrations"][0]["target"], "workbox");
    assert_eq!(
        persisted["registrations"][0]["node_id"],
        "550e8400-e29b-41d4-a716-446655440000"
    );

    let stop = Command::new(env!("CARGO_BIN_EXE_boomux"))
        .args(["daemon", "stop"])
        .env("HOME", directory.join("home"))
        .env("XDG_RUNTIME_DIR", directory.join("runtime"))
        .env("XDG_STATE_HOME", directory.join("state"))
        .output()
        .unwrap();
    assert!(stop.status.success());
    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn guided_node_add_mirrors_master_challenge_and_waits_after_failure() {
    let directory = test_directory();
    fs::create_dir_all(directory.join("home/.ssh")).unwrap();
    fs::create_dir_all(directory.join("runtime")).unwrap();
    fake_challenge_ssh(&directory);

    let mut master = 0;
    let mut slave = 0;
    assert_eq!(
        unsafe {
            libc::openpty(
                &mut master,
                &mut slave,
                std::ptr::null_mut(),
                std::ptr::null(),
                std::ptr::null(),
            )
        },
        0
    );
    let master = unsafe { OwnedFd::from_raw_fd(master) };
    let slave = unsafe { OwnedFd::from_raw_fd(slave) };
    let mut command = Command::new(env!("CARGO_BIN_EXE_boomux"));
    command
        .arg("__guided-node-add")
        .env("PATH", format!("{}:/usr/bin:/bin", directory.display()))
        .env("HOME", directory.join("home"))
        .env("XDG_RUNTIME_DIR", directory.join("runtime"))
        .env("XDG_STATE_HOME", directory.join("state"))
        .stdin(Stdio::from(slave.try_clone().unwrap()))
        .stdout(Stdio::from(slave.try_clone().unwrap()))
        .stderr(Stdio::from(slave));
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 || libc::ioctl(0, libc::TIOCSCTTY, 0) == -1 {
                Err(std::io::Error::last_os_error())
            } else {
                Ok(())
            }
        });
    }
    let mut child = command.spawn().unwrap();
    let mut master = fs::File::from(master);
    master.write_all(b"work\nworkbox\n").unwrap();
    let flags = unsafe { libc::fcntl(master.as_raw_fd(), libc::F_GETFL) };
    assert_ne!(flags, -1);
    assert_ne!(
        unsafe { libc::fcntl(master.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) },
        -1
    );
    let mut output = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(10);
    while !output
        .windows(b"Press Enter to close.".len())
        .any(|window| window == b"Press Enter to close.")
    {
        let mut bytes = [0_u8; 1024];
        match master.read(&mut bytes) {
            Ok(0) => panic!("guided terminal closed before the outcome hold"),
            Ok(count) => output.extend_from_slice(&bytes[..count]),
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(error) if error.raw_os_error() == Some(libc::EIO) => {}
            Err(error) => panic!("PTY read failed: {error}"),
        }
        assert!(
            Instant::now() < deadline,
            "guided challenge timed out: {}",
            String::from_utf8_lossy(&output)
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    let output = String::from_utf8_lossy(&output);
    assert!(output.contains("To authenticate, visit:"), "{output}");
    assert!(
        output.contains("https://login.tailscale.test/challenge"),
        "{output}"
    );
    assert!(output.contains("Node setup failed (exit 1)."), "{output}");
    assert!(child.try_wait().unwrap().is_none());
    master.write_all(b"\n").unwrap();
    assert_eq!(child.wait().unwrap().code(), Some(1));
    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn ready_helper_ping_failure_aborts_without_hanging() {
    let directory = test_directory();
    fs::create_dir_all(directory.join("home/.ssh")).unwrap();
    fs::create_dir_all(directory.join("runtime")).unwrap();
    fake_ssh(&directory, "/remote/boomux\\0", true);

    let output = command(&directory).output().unwrap();
    assert!(!output.status.success());
    assert!(!String::from_utf8_lossy(&output.stdout).contains("Connected to Boomux Node"));
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("failed to fill whole buffer"),
        "unexpected disconnect error: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    fs::remove_dir_all(directory).unwrap();
}

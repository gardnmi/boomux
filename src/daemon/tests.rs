use super::*;

#[test]
fn active_remote_upgrade_projects_as_reconnecting() {
    assert_eq!(
        classify_node_sync_error(&io::Error::from(io::ErrorKind::WouldBlock)),
        crate::protocol::NodeProjectionHealthCode::Reconnecting
    );
    assert_eq!(
        classify_node_sync_error(&io::Error::from(io::ErrorKind::Unsupported)),
        crate::protocol::NodeProjectionHealthCode::Unsupported
    );
    assert_eq!(
        classify_node_sync_error(&crate::ssh_bootstrap::stale_upgrade_recovery()),
        crate::protocol::NodeProjectionHealthCode::Stale
    );
}

use std::sync::Barrier;

use crate::protocol::{AgentAuthority, AgentState};

trait ShellTestControl {
    fn kill(&self) -> io::Result<()>;
}

impl ShellTestControl for Shell {
    fn kill(&self) -> io::Result<()> {
        ShellRuntimeManager::default().kill(self)
    }
}

trait ReaderTestControl {
    fn pause_reader(&self) -> io::Result<()>;
    fn resume_reader(&self) -> io::Result<()>;
}

impl ReaderTestControl for ShellRuntime {
    fn pause_reader(&self) -> io::Result<()> {
        ShellRuntimeManager::default().pause_reader(self)
    }

    fn resume_reader(&self) -> io::Result<()> {
        ShellRuntimeManager::default().resume_reader(self)
    }
}

fn spawn_runtime(
    shell: &Arc<Shell>,
    run: &ShellRun,
    workspace_name: &str,
    shell_name: &str,
    profile: &TerminalProfile,
    environment: Option<&UnixEnvironment>,
    recovery: RuntimeRecovery<'_>,
) -> io::Result<(Arc<ShellRuntime>, PtyReader)> {
    ShellRuntimeManager::default().spawn_runtime(
        shell,
        run,
        RuntimeStart {
            workspace_name,
            shell_name,
            profile,
            environment,
            recovery,
            claude_remote_control: true,
        },
    )
}

fn start_pty_reader(
    registry: Weak<DaemonService>,
    shell: Arc<Shell>,
    run: Arc<ShellRun>,
    runtime: Arc<ShellRuntime>,
    reader: PtyReader,
    start_paused: bool,
) -> io::Result<()> {
    ShellRuntimeManager::default().start_pty_reader(
        registry,
        shell,
        run,
        runtime,
        reader,
        start_paused,
    )
}

#[derive(Default)]
struct RecordingNotificationSink {
    requests: Mutex<Vec<NotificationRequest>>,
}

impl NotificationSink for RecordingNotificationSink {
    fn notify(&self, request: NotificationRequest) {
        self.requests.lock().unwrap().push(request);
    }
}

fn notification_registry(
    settings: NotificationSettings,
) -> (DaemonService, Arc<RecordingNotificationSink>) {
    let sink = Arc::new(RecordingNotificationSink::default());
    let registry = DaemonService {
        notification_settings: NotificationDeliverySettings {
            desktop: settings,
            ..Default::default()
        },
        notification_sink: sink.clone(),
        ..DaemonService::default()
    };
    (registry, sink)
}

fn profile() -> TerminalProfile {
    TerminalProfile {
        term: Some("xterm-256color".into()),
        colorterm: Some("truecolor".into()),
        term_program: Some("test".into()),
        term_program_version: Some("1".into()),
        rows: 24,
        cols: 80,
        pixel_width: 0,
        pixel_height: 0,
    }
}

#[test]
fn create_started_shell_rolls_back_process_and_events_on_failure() {
    for failure in ["spawn", "mutation", "persistence"] {
        let directory = env::temp_dir().join(format!("boomux-start-{}", Uuid::new_v4()));
        fs::create_dir_all(&directory).unwrap();
        let registry = Arc::new(
            DaemonService::restore(StateStore::at(directory.join("state.json")), false, None)
                .unwrap(),
        );
        let workspace = registry.create_workspace("test".into(), vec![]).unwrap();
        let events_before = registry.events.manifest().unwrap().events.len();
        let pid_file = directory.join("pid");
        if failure == "mutation" {
            registry.fail_after_mutation.store(true, Ordering::Release);
        }
        if failure == "persistence" {
            registry.fail_next_persistence();
        }
        let result = registry.dispatch_arc(
            Request::CreateStartedShell {
                workspace_id: workspace.id.clone(),
                shell: ShellSpec {
                    name: "test".into(),
                    cwd: directory.clone(),
                    command: if failure == "spawn" {
                        vec![directory.join("missing").to_string_lossy().into_owned()]
                    } else {
                        vec![
                            "/bin/sh".into(),
                            "-c".into(),
                            "echo $$ > pid; exec sleep 60".into(),
                        ]
                    },
                },
                profile: profile(),
                environment: None,
            },
            protocol::PROTOCOL_VERSION,
        );
        assert!(result.is_err(), "{failure}");
        assert!(
            lock(&registry.workspace(&workspace.id).unwrap().shell_ids)
                .unwrap()
                .is_empty()
        );
        assert!(lock(&registry.durable.state).unwrap().shells.is_empty());
        assert_eq!(
            registry.events.manifest().unwrap().events.len(),
            events_before
        );
        if let Ok(pid) = fs::read_to_string(pid_file) {
            let pid: i32 = pid.trim().parse().unwrap();
            assert_eq!(
                unsafe { libc::kill(pid, 0) },
                -1,
                "failed start leaked process {pid}"
            );
        }
        drop(registry);
        fs::remove_dir_all(directory).unwrap();
    }
}

#[test]
fn listener_wait_is_bounded_and_wakes_for_connections() {
    let directory = env::temp_dir().join(format!("boomux-listener-{}", Uuid::new_v4()));
    fs::create_dir(&directory).unwrap();
    let path = directory.join("socket");
    let listener = UnixListener::bind(&path).unwrap();
    listener.set_nonblocking(true).unwrap();
    assert!(!wait_for_listener(&listener, Duration::ZERO).unwrap());
    let connecting = thread::spawn(move || UnixStream::connect(path).unwrap());
    assert!(wait_for_listener(&listener, Duration::from_secs(1)).unwrap());
    let (accepted, _) = listener.accept().unwrap();
    let client = connecting.join().unwrap();
    assert!(!wait_for_listener(&listener, Duration::ZERO).unwrap());
    drop((accepted, client, listener));
    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn new_shell_terminal_starts_without_injected_output() {
    let terminal = initial_terminal_state(24, 80, None);

    assert!(terminal.plain_text().is_empty());
}

#[test]
fn cold_terminal_history_is_presented_with_a_recovery_notice() {
    let terminal = initial_terminal_state(24, 80, Some("old output\n"));
    let text = terminal.plain_text();

    assert!(text.contains("restored bounded history from previous run\nold output"));
}

fn test_environment(values: &[(&str, &Path)]) -> UnixEnvironment {
    UnixEnvironment {
        variables: values
            .iter()
            .map(|(name, value)| UnixEnvironmentVariable {
                name: name.as_bytes().to_vec(),
                value: value.as_os_str().as_bytes().to_vec(),
            })
            .collect(),
    }
}

#[test]
fn opencode_shim_eligibility_is_limited_to_login_shells() {
    let login =
        create_pending_shell("workspace", ShellSpec::login("login", env::temp_dir())).unwrap();
    assert!(opencode_shim_eligible(&login, &[]));
    assert!(!opencode_shim_eligible(
        &login,
        &["opencode".into(), "--continue".into()]
    ));

    let command = create_pending_shell(
        "workspace",
        ShellSpec {
            name: "command".into(),
            cwd: env::temp_dir(),
            command: vec!["opencode".into()],
        },
    )
    .unwrap();
    assert!(!opencode_shim_eligible(&command, &command.command));
}

#[test]
fn supervised_opencode_resume_is_exactly_recognized_for_shared_launch() {
    let command = vec![
        "/opt/boomux".into(),
        "agent".into(),
        "supervise".into(),
        "OpenCode".into(),
        "--integration".into(),
        "opencode".into(),
        "--external-session-id".into(),
        "session-exact".into(),
        "--".into(),
        "/opt/opencode".into(),
        "--session".into(),
        "session-exact".into(),
    ];
    assert_eq!(supervised_opencode_session(&command), Some("session-exact"));
    assert_eq!(
        supervised_shared_opencode_command(&command, "/new/boomux").unwrap(),
        [
            "/opt/boomux",
            "agent",
            "supervise",
            "OpenCode",
            "--integration",
            "opencode",
            "--external-session-id",
            "session-exact",
            "--",
            "/new/boomux",
            "opencode",
            "shared",
            "--session",
            "session-exact",
        ]
    );

    let mut mismatched = command.clone();
    mismatched[11] = "different-session".into();
    assert_eq!(supervised_opencode_session(&mismatched), None);

    let mut argument_bearing = command;
    argument_bearing.push("--fork".into());
    assert_eq!(supervised_opencode_session(&argument_bearing), None);
}

#[test]
fn claude_remote_control_rewrites_only_bare_commands() {
    let command = create_pending_shell(
        "workspace",
        ShellSpec {
            name: "claude".into(),
            cwd: env::temp_dir(),
            command: vec!["/opt/anthropic/bin/claude".into()],
        },
    )
    .unwrap();
    assert_eq!(
        claude_remote_control_command(&command, &command.command, false, true),
        Some(vec![
            "/opt/anthropic/bin/claude".into(),
            "--remote-control".into(),
        ])
    );
    for (argv, recovery, enabled) in [
        (
            vec!["claude".into(), "--resume".into(), "id".into()],
            false,
            true,
        ),
        (vec!["claude".into()], true, true),
        (vec!["claude".into()], false, false),
        (vec!["claude-wrapper".into()], false, true),
    ] {
        assert!(
            claude_remote_control_command(&command, &argv, recovery, enabled).is_none(),
            "rewrote {argv:?}"
        );
    }
}

#[test]
fn codex_launcher_accepts_only_managed_chat_shapes() {
    let shell = create_pending_shell(
        "workspace",
        ShellSpec {
            name: "codex".into(),
            cwd: env::temp_dir(),
            command: vec!["/opt/openai/codex".into()],
        },
    )
    .unwrap();
    for argv in [
        vec!["/opt/openai/codex".into()],
        vec!["/opt/openai/codex".into(), "resume".into(), "exact".into()],
        vec!["/opt/openai/codex".into(), "exec".into(), "-".into()],
    ] {
        assert!(codex_launch_eligible(&shell, &argv));
    }
    for argv in [
        vec!["codex".into(), "remote-control".into()],
        vec!["codex".into(), "--remote".into(), "unix://".into()],
        vec!["codex-wrapper".into()],
    ] {
        assert!(!codex_launch_eligible(&shell, &argv));
    }
}

#[test]
fn kiro_launcher_accepts_chat_without_selecting_an_engine() {
    let shell = create_pending_shell(
        "workspace",
        ShellSpec {
            name: "kiro".into(),
            cwd: env::temp_dir(),
            command: vec!["/opt/kiro/bin/kiro-cli".into()],
        },
    )
    .unwrap();
    for argv in [
        vec!["/opt/kiro/bin/kiro-cli".into()],
        vec!["kiro-cli".into(), "chat".into()],
        vec!["kiro-cli".into(), "--agent".into(), "reviewer".into()],
        vec!["kiro-cli".into(), "--version".into()],
        vec![
            "/opt/kiro/bin/kiro-cli".into(),
            "--v3".into(),
            "chat".into(),
        ],
    ] {
        assert!(kiro_launch_eligible(&shell, &argv));
    }
    assert!(!kiro_launch_eligible(&shell, &["kiro".into()]));
}

#[test]
fn inherited_opencode_shim_provenance_is_stripped_without_losing_identity() {
    let mut environment = test_environment(&[
        ("PATH", Path::new("/runtime/boomux/shims:/usr/bin")),
        ("BOOMUX_ORIGINAL_PATH", Path::new("/usr/local/bin:/usr/bin")),
        (
            "BOOMUX_OPENCODE_SHIM_DIR",
            Path::new("/runtime/boomux/shims"),
        ),
        ("BOOMUX_REAL_OPENCODE", Path::new("/usr/bin/opencode")),
        ("BOOMUX_REAL_CLAUDE", Path::new("/usr/bin/claude")),
        ("BOOMUX_REAL_CODEX", Path::new("/usr/bin/codex")),
        ("BOOMUX_REAL_KIRO", Path::new("/usr/bin/kiro-cli")),
        ("BOOMUX_CODEX_RUN_SCOPED", Path::new("1")),
        ("BOOMUX_KIRO_RUN_SCOPED", Path::new("1")),
        ("BOOMUX_CLAUDE_REMOTE_CONTROL", Path::new("1")),
        (
            "BOOMUX_OPENCODE_TUI_CONFIG",
            Path::new("/runtime/boomux/shims/tui.json"),
        ),
        (
            "OPENCODE_TUI_CONFIG",
            Path::new("/runtime/boomux/shims/tui.json"),
        ),
        ("BOOMUX_SHIM_EXECUTABLE", Path::new("/usr/bin/boomux")),
        ("BOOMUX_OPENCODE_SHARED_GENERATION", Path::new("generation")),
        ("BOOMUX_OPENCODE_CLAIM_HOLDER", Path::new("holder")),
        ("BOOMUX_USER_ZDOTDIR", Path::new("/home/user")),
        ("ZDOTDIR", Path::new("/runtime/boomux/shims")),
        ("BOOMUX_SHELL_ID", Path::new("shell-1")),
        ("BOOMUX_RUN_ID", Path::new("run-1")),
    ]);
    environment.variables.push(UnixEnvironmentVariable {
        name: b"KEEP".to_vec(),
        value: b"value".to_vec(),
    });

    let sanitized = sanitize_opencode_shim_environment(&environment);
    assert_eq!(
        environment_value(&sanitized, b"PATH").as_deref(),
        Some(std::ffi::OsStr::new("/usr/local/bin:/usr/bin"))
    );
    for name in [
        b"BOOMUX_ORIGINAL_PATH".as_slice(),
        b"BOOMUX_OPENCODE_SHIM_DIR",
        b"BOOMUX_REAL_OPENCODE",
        b"BOOMUX_REAL_CLAUDE",
        b"BOOMUX_REAL_CODEX",
        b"BOOMUX_REAL_KIRO",
        b"BOOMUX_CODEX_RUN_SCOPED",
        b"BOOMUX_KIRO_RUN_SCOPED",
        b"BOOMUX_CLAUDE_REMOTE_CONTROL",
        b"BOOMUX_OPENCODE_TUI_CONFIG",
        b"BOOMUX_SHIM_EXECUTABLE",
        b"BOOMUX_OPENCODE_SHARED_GENERATION",
        b"BOOMUX_OPENCODE_CLAIM_HOLDER",
        b"BOOMUX_USER_ZDOTDIR",
        b"OPENCODE_TUI_CONFIG",
    ] {
        assert!(
            environment_value(&sanitized, name).is_none(),
            "retained {name:?}"
        );
    }
    assert_eq!(
        environment_value(&sanitized, b"BOOMUX_SHELL_ID").unwrap(),
        "shell-1"
    );
    assert_eq!(
        environment_value(&sanitized, b"BOOMUX_RUN_ID").unwrap(),
        "run-1"
    );
    assert_eq!(environment_value(&sanitized, b"KEEP").unwrap(), "value");
    assert_eq!(
        environment_value(&sanitized, b"ZDOTDIR").unwrap(),
        "/home/user"
    );
}

#[test]
fn common_shell_startup_adapters_reassert_the_scoped_shim_after_user_config() {
    let mut environment = test_environment(&[
        (
            "BOOMUX_OPENCODE_SHIM_DIR",
            Path::new("/runtime/boomux/shims"),
        ),
        ("HOME", Path::new("/home/user")),
        ("ZDOTDIR", Path::new("/home/user/custom-zsh")),
    ]);

    assert_eq!(
        configure_opencode_shell_startup(Path::new("/bin/bash").as_os_str(), &mut environment),
        vec![
            std::ffi::OsString::from("--rcfile"),
            std::ffi::OsString::from("/runtime/boomux/shims/boomux.bashrc"),
        ]
    );

    assert!(
        configure_opencode_shell_startup(Path::new("/usr/bin/zsh").as_os_str(), &mut environment)
            .is_empty()
    );
    assert_eq!(
        environment_value(&environment, b"BOOMUX_USER_ZDOTDIR").unwrap(),
        "/home/user/custom-zsh"
    );
    assert_eq!(
        environment_value(&environment, b"ZDOTDIR").unwrap(),
        "/runtime/boomux/shims"
    );

    let fish =
        configure_opencode_shell_startup(Path::new("/usr/bin/fish").as_os_str(), &mut environment);
    assert_eq!(fish[0], "--init-command");
    assert!(fish[1].to_string_lossy().contains("BOOMUX_ORIGINAL_PATH"));
    assert!(
        configure_opencode_shell_startup(
            Path::new("/usr/bin/unknown-shell").as_os_str(),
            &mut environment
        )
        .is_empty()
    );
    for source in [OPENCODE_BASH_RC, OPENCODE_ZSH_ENV, OPENCODE_ZSH_RC] {
        assert!(
            std::str::from_utf8(source)
                .unwrap()
                .contains("BOOMUX_OPENCODE_SHIM_DIR")
        );
    }
    assert!(
        std::str::from_utf8(OPENCODE_BASH_RC)
            .unwrap()
            .contains("builtin hash -r 2>/dev/null || :")
    );
}

#[test]
fn zsh_startup_preserves_user_directory_and_sources_each_file_once() {
    let Some(zsh) = ["/bin/zsh", "/usr/bin/zsh"]
        .into_iter()
        .find(|path| Path::new(path).is_file())
    else {
        assert!(!cfg!(target_os = "macos"), "macOS must provide zsh");
        eprintln!("skipping zsh startup fixture: zsh is not installed");
        return;
    };
    for case in [
        "missing-env",
        "ordinary-env",
        "custom-zdotdir",
        "env-redirect",
        "env-unset",
        "rc-redirect",
    ] {
        let directory = env::temp_dir().join(format!("boomux-zsh {case}-{}", Uuid::new_v4()));
        let home = directory.join("home");
        let custom = directory.join("custom");
        let relocated = directory.join("relocated");
        let shims = directory.join("shims");
        for path in [&home, &custom, &relocated, &shims] {
            fs::create_dir_all(path).unwrap();
        }
        let original = if matches!(case, "custom-zdotdir" | "env-unset") {
            &custom
        } else {
            &home
        };
        let rc_directory = match case {
            "env-redirect" => &relocated,
            "env-unset" => &home,
            _ => original,
        };
        let final_directory = if case == "rc-redirect" {
            &relocated
        } else {
            rc_directory
        };
        if case != "missing-env" {
            let redirect = match case {
                "env-redirect" => "ZDOTDIR=$BOOMUX_TEST_RELOCATED\n",
                "env-unset" => "unset ZDOTDIR\n",
                _ => "",
            };
            fs::write(
                original.join(".zshenv"),
                format!(
                    "[[ $ZDOTDIR = $BOOMUX_TEST_ORIGINAL ]] || exit 10\n\
                         (( BOOMUX_TEST_ENV_COUNT += 1 ))\n{redirect}"
                ),
            )
            .unwrap();
        }
        fs::write(
            rc_directory.join(".zshrc"),
            format!(
                "[[ $ZDOTDIR = $BOOMUX_TEST_RC_DIRECTORY ]] || exit 11\n\
                     (( BOOMUX_TEST_RC_COUNT += 1 ))\n\
                     PATH=/selected/bin:/usr/bin:/bin\n{}",
                if case == "rc-redirect" {
                    "ZDOTDIR=$BOOMUX_TEST_RELOCATED\n"
                } else {
                    ""
                }
            ),
        )
        .unwrap();
        fs::write(shims.join(".zshenv"), OPENCODE_ZSH_ENV).unwrap();
        fs::write(shims.join(".zshrc"), OPENCODE_ZSH_RC).unwrap();
        let mut environment = test_environment(&[
            ("HOME", &home),
            ("PATH", Path::new("/usr/bin:/bin")),
            ("BOOMUX_OPENCODE_SHIM_DIR", &shims),
        ]);
        if original == &custom {
            set_environment_value(&mut environment, b"ZDOTDIR", original.as_os_str());
        }
        configure_opencode_shell_startup(std::ffi::OsStr::new(zsh), &mut environment);
        let mut command = Command::new(zsh);
        command.env_clear();
        for variable in environment.variables {
            command.env(
                std::ffi::OsString::from_vec(variable.name),
                std::ffi::OsString::from_vec(variable.value),
            );
        }
        let output = command
            .args([
                "-d",
                "-i",
                "-c",
                r#"[[ $ZDOTDIR = $BOOMUX_TEST_FINAL_DIRECTORY ]] || exit 12
[[ $BOOMUX_TEST_RC_COUNT = 1 ]] || exit 13
[[ ${BOOMUX_TEST_ENV_COUNT:-0} = $BOOMUX_TEST_EXPECTED_ENV_COUNT ]] || exit 14
[[ $PATH = "$BOOMUX_OPENCODE_SHIM_DIR:/selected/bin:/usr/bin:/bin" ]] || exit 15
[[ $BOOMUX_ORIGINAL_PATH = /selected/bin:/usr/bin:/bin ]] || exit 16
[[ -z ${BOOMUX_USER_ZDOTDIR+x} ]] || exit 17
"#,
            ])
            .env("BOOMUX_TEST_ORIGINAL", original)
            .env("BOOMUX_TEST_RELOCATED", &relocated)
            .env("BOOMUX_TEST_RC_DIRECTORY", rc_directory)
            .env("BOOMUX_TEST_FINAL_DIRECTORY", final_directory)
            .env(
                "BOOMUX_TEST_EXPECTED_ENV_COUNT",
                if case == "missing-env" { "0" } else { "1" },
            )
            .output()
            .unwrap();
        fs::remove_dir_all(directory).unwrap();
        assert!(output.status.success(), "{case}: {output:?}");
        assert!(output.stderr.is_empty(), "{case}: {output:?}");
    }
}

#[test]
fn bash_startup_silences_cache_reset_when_hashing_is_disabled() {
    let directory = env::temp_dir().join(format!("boomux-bashrc-{}", Uuid::new_v4()));
    let startup = directory.join("boomux.bashrc");
    fs::create_dir(&directory).unwrap();
    fs::write(directory.join(".bashrc"), b"set +h\n").unwrap();
    fs::write(&startup, OPENCODE_BASH_RC).unwrap();

    let output = Command::new("/bin/bash")
        .args(["--noprofile", "--norc", "-c", ". \"$BOOMUX_TEST_RC\""])
        .env("HOME", &directory)
        .env("PATH", "/usr/bin:/bin")
        .env("BOOMUX_OPENCODE_SHIM_DIR", directory.join("shims"))
        .env("BOOMUX_TEST_RC", startup)
        .output()
        .unwrap();

    assert!(output.status.success());
    assert!(
        output.stderr.is_empty(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn bash_prompt_refresh_preserves_callbacks_status_and_current_tool_path() {
    let directory = env::temp_dir().join(format!("boomux-prompt-{}", Uuid::new_v4()));
    fs::create_dir(&directory).unwrap();
    let startup = directory.join("boomux.bashrc");
    fs::write(&startup, OPENCODE_BASH_RC).unwrap();
    for callbacks in [
        "PROMPT_COMMAND='PATH=/selected/bin:$PATH; false'",
        "PROMPT_COMMAND=('PATH=/selected/bin:$PATH' 'false')",
    ] {
        fs::write(directory.join(".bashrc"), callbacks).unwrap();
        let output = Command::new("/bin/bash")
            .args([
                "--noprofile",
                "--norc",
                "-c",
                r#". "$BOOMUX_TEST_RC"
for callback in "${PROMPT_COMMAND[@]}"; do eval "$callback"; done
status=$?
[ "$status" = 1 ] || exit 10
[ "$PATH" = /boomux/shims:/selected/bin:/usr/bin:/bin ] || exit 11
[ "$BOOMUX_ORIGINAL_PATH" = /selected/bin:/usr/bin:/bin ] || exit 12
"#,
            ])
            .env("HOME", &directory)
            .env("PATH", "/usr/bin:/bin")
            .env("BOOMUX_OPENCODE_SHIM_DIR", "/boomux/shims")
            .env("BOOMUX_TEST_RC", &startup)
            .output()
            .unwrap();
        assert!(output.status.success(), "{callbacks}: {output:?}");
    }
    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn runtime_assets_reuse_matching_files_and_repair_changed_content_or_mode() {
    let directory = env::temp_dir().join(format!("boomux-runtime-asset-{}", Uuid::new_v4()));
    fs::create_dir(&directory).unwrap();
    let path = directory.join("shim");
    let content = vec![b'x'; 20_000];
    atomic_runtime_asset(&path, &content, 0o700).unwrap();
    let before = fs::metadata(&path).unwrap();
    for _ in 0..12 {
        atomic_runtime_asset(&path, &content, 0o700).unwrap();
    }
    let after = fs::metadata(&path).unwrap();
    assert_eq!(before.ino(), after.ino());
    assert_eq!(
        (before.ctime(), before.ctime_nsec()),
        (after.ctime(), after.ctime_nsec())
    );
    let mut changed = content.clone();
    changed[19_999] = b'y';
    atomic_runtime_asset(&path, &changed, 0o700).unwrap();
    assert_eq!(fs::read(&path).unwrap(), changed);
    fs::set_permissions(&path, fs::Permissions::from_mode(0o777)).unwrap();
    atomic_runtime_asset(&path, &changed, 0o700).unwrap();
    assert_eq!(fs::metadata(&path).unwrap().mode() & 0o777, 0o700);
    let link = directory.join("link");
    std::os::unix::fs::symlink(&path, &link).unwrap();
    assert!(atomic_runtime_asset(&link, &changed, 0o700).is_err());
    assert_eq!(fs::read(&path).unwrap(), changed);
    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn shim_assets_are_private_and_forward_exact_arguments() {
    let directory = env::temp_dir().join(format!("boomux-opencode-shim-{}", Uuid::new_v4()));
    let runtime = directory.join("runtime");
    let bin = directory.join("bin");
    fs::create_dir_all(&runtime).unwrap();
    fs::create_dir_all(&bin).unwrap();
    let host = bin.join("opencode");
    fs::write(&host, b"#!/bin/sh\nprintf '<%s>\\n' \"$@\"\n").unwrap();
    fs::set_permissions(&host, fs::Permissions::from_mode(0o700)).unwrap();
    let claude = bin.join("claude");
    fs::write(&claude, b"#!/bin/sh\nprintf '[%s]\\n' \"$@\"\n").unwrap();
    fs::set_permissions(&claude, fs::Permissions::from_mode(0o700)).unwrap();
    let codex = bin.join("codex");
    fs::write(&codex, b"#!/bin/sh\nprintf '{%s}\\n' \"$@\"\n").unwrap();
    fs::set_permissions(&codex, fs::Permissions::from_mode(0o700)).unwrap();
    let kiro = bin.join("kiro-cli");
    fs::write(&kiro, b"#!/bin/sh\nprintf '(%s)\\n' \"$@\"\n").unwrap();
    fs::set_permissions(&kiro, fs::Permissions::from_mode(0o700)).unwrap();
    let environment = test_environment(&[("XDG_RUNTIME_DIR", &runtime), ("PATH", &bin)]);

    let injected = inject_opencode_shim_environment(&environment, true).unwrap();
    let shim = PathBuf::from(environment_value(&injected, b"BOOMUX_OPENCODE_SHIM_DIR").unwrap())
        .join("opencode");
    assert_eq!(
        fs::metadata(&shim).unwrap().permissions().mode() & 0o777,
        0o700
    );
    assert_eq!(
        fs::metadata(shim.with_file_name("tui.json"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
    let output = Command::new(&shim)
        .args(["", "$(touch should-not-exist)", "semi;colon", "two words"])
        .env("BOOMUX_REAL_OPENCODE", &host)
        .output()
        .unwrap();
    assert!(output.status.success());
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        "<>\n<$(touch should-not-exist)>\n<semi;colon>\n<two words>\n"
    );
    let noninteractive = Command::new(&shim)
        .env("BOOMUX_REAL_OPENCODE", &host)
        .env("BOOMUX_SHELL_ID", "shell-1")
        .env("BOOMUX_RUN_ID", "run-1")
        .output()
        .unwrap();
    assert!(noninteractive.status.success());
    assert_eq!(noninteractive.stdout, b"<>\n");
    assert!(
        std::str::from_utf8(OPENCODE_SHIM)
            .unwrap()
            .contains("exec \"$BOOMUX_SHIM_EXECUTABLE\" opencode shared")
    );
    let claude_shim = shim.with_file_name("claude");
    let explicit = Command::new(&claude_shim)
        .args(["--resume", "exact; id"])
        .env("BOOMUX_REAL_CLAUDE", &claude)
        .env("BOOMUX_CLAUDE_REMOTE_CONTROL", "1")
        .output()
        .unwrap();
    assert!(explicit.status.success());
    assert_eq!(explicit.stdout, b"[--resume]\n[exact; id]\n");
    assert!(
        std::str::from_utf8(CLAUDE_SHIM)
            .unwrap()
            .contains("exec \"$BOOMUX_REAL_CLAUDE\" --remote-control")
    );
    let codex_shim = shim.with_file_name("codex");
    assert_eq!(
        fs::metadata(&codex_shim).unwrap().permissions().mode() & 0o777,
        0o700
    );
    assert_eq!(
        environment_value(&injected, b"BOOMUX_REAL_CODEX").as_deref(),
        Some(codex.as_os_str())
    );
    assert!(
        std::str::from_utf8(CODEX_SHIM)
            .unwrap()
            .contains("codex launch -- \"$@\"")
    );
    let kiro_shim = shim.with_file_name("kiro-cli");
    assert_eq!(
        fs::metadata(&kiro_shim).unwrap().permissions().mode() & 0o777,
        0o700
    );
    assert_eq!(
        environment_value(&injected, b"BOOMUX_REAL_KIRO").as_deref(),
        Some(kiro.as_os_str())
    );
    assert!(
        std::str::from_utf8(KIRO_SHIM)
            .unwrap()
            .contains("kiro launch -- \"$@\"")
    );
    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn missing_agent_hosts_still_prepare_the_late_install_kiro_shim() {
    let directory = env::temp_dir().join(format!("boomux-opencode-missing-{}", Uuid::new_v4()));
    let runtime = directory.join("runtime");
    let empty_bin = directory.join("bin");
    fs::create_dir_all(&runtime).unwrap();
    fs::create_dir_all(&empty_bin).unwrap();
    let environment = test_environment(&[("XDG_RUNTIME_DIR", &runtime), ("PATH", &empty_bin)]);
    let sanitized = sanitize_opencode_shim_environment(&environment);
    let injected = inject_opencode_shim_environment(&sanitized, true).unwrap();
    let shim_dir =
        PathBuf::from(environment_value(&injected, b"BOOMUX_OPENCODE_SHIM_DIR").unwrap());
    assert!(shim_dir.join("kiro-cli").is_file());
    assert_eq!(environment_value(&injected, b"BOOMUX_REAL_KIRO"), None);
    assert_eq!(
        env::split_paths(&environment_value(&injected, b"PATH").unwrap())
            .next()
            .as_deref(),
        Some(shim_dir.as_path())
    );
    fs::remove_dir_all(directory).unwrap();
}

fn add_recovery_agent(
    registry: &DaemonService,
    shell: &Shell,
    run_id: &str,
    integration: &str,
    external_session_id: &str,
) -> String {
    let agent = Arc::new(AgentInstance {
        id: Uuid::new_v4().to_string(),
        workspace_id: shell.workspace_id.clone(),
        shell_id: shell.id.clone(),
        run_id: run_id.into(),
        name: integration.into(),
        integration: integration.into(),
        external_session_id: Some(external_session_id.into()),
        cwd: Some(shell.cwd.clone()),
        started_at_ms: 1,
        state: Mutex::new(AgentInstanceState {
            ended_at_ms: None,
            observation: AgentObservationSnapshot {
                revision: 1,
                state: AgentState::Working,
                authority: AgentAuthority::LifecycleIntegration,
                evidence: "active before interruption".into(),
                confidence: 100,
                observed_at_ms: 1,
            },
            attention: None,
            working_contexts: Vec::new(),
        }),
    });
    let agent_id = agent.id.clone();
    lock(&registry.durable.state)
        .unwrap()
        .agents
        .insert(agent.id.clone(), agent);
    agent_id
}

fn recovery_shell(
    registry: &DaemonService,
    command: Vec<String>,
) -> (Arc<Shell>, PersistedShellRun) {
    let workspace = registry
        .create_workspace(
            "recovery".into(),
            vec![ShellSpec {
                name: "agent".into(),
                command,
                cwd: env::temp_dir(),
            }],
        )
        .unwrap();
    let shell = registry.shell(&workspace.shells[0].id).unwrap();
    let run = PersistedShellRun {
        id: Uuid::new_v4().to_string(),
        generation: 1,
        started_at_ms: 1,
        ended_at_ms: Some(2),
        exit_reason: Some(ShellRunExitReason::Interrupted),
        output_revision: 1,
        environment_has_run_id: true,
        profile: profile(),
        terminal_history: None,
    };
    *lock(&shell.last_run).unwrap() = Some(run.clone());
    (shell, run)
}

#[test]
fn interrupted_authoritative_agent_builds_native_resume_command() {
    let registry = DaemonService::default();
    let (shell, run) = recovery_shell(&registry, vec!["/opt/bin/opencode".into()]);
    let agent_id = add_recovery_agent(&registry, &shell, &run.id, "opencode", "session-1");
    add_recovery_agent(&registry, &shell, &run.id, "native-test", "other");

    let snapshot = registry.snapshot().unwrap();
    let snapshot = &snapshot.workspaces[0].shells[0];
    assert_eq!(snapshot.status, ShellStatus::Pending);
    assert_eq!(
        snapshot.run.as_ref().map(|run| run.id.as_str()),
        Some(run.id.as_str())
    );
    assert_eq!(
        snapshot.recovered_agent_id.as_deref(),
        Some(agent_id.as_str())
    );
    let mut projection = shell.node_projection().unwrap();
    registry.add_recovery_projection(&mut projection).unwrap();
    assert_eq!(projection.run_id.as_deref(), Some(run.id.as_str()));
    assert_eq!(
        projection.recovered_agent_id.as_deref(),
        Some(agent_id.as_str())
    );

    let resumable = registry
        .resumable_agent(&shell, Some(&run))
        .unwrap()
        .unwrap();
    assert_eq!(resumable.agent_id, agent_id);
    assert_eq!(resumable.integration, "opencode");
    assert_eq!(resumable.external_session_id, "session-1");
    assert_eq!(
        resumable.command,
        ["/opt/bin/opencode", "--session", "session-1"]
    );

    let agent = lock(&registry.durable.state)
        .unwrap()
        .agents
        .get(&agent_id)
        .unwrap()
        .clone();
    lock(&agent.state).unwrap().observation.authority = AgentAuthority::TerminalHeuristic;
    assert!(
        registry
            .resumable_agent(&shell, Some(&run))
            .unwrap()
            .is_none()
    );
    assert!(registry.recovery_presentation(&shell.id).unwrap().is_none());
}

#[test]
fn session_display_name_mutation_replays_and_reset_restores_derived_name() {
    let registry = DaemonService::default();
    let (shell, run) = recovery_shell(&registry, vec!["agent".into()]);
    let agent_id = add_recovery_agent(
        &registry,
        &shell,
        &run.id,
        "native-test",
        "external-session",
    );
    let workspace = registry.workspace(&shell.workspace_id).unwrap();
    lock(&workspace.agent_ids).unwrap().push(agent_id);
    let snapshot = registry.snapshot().unwrap();
    let projected = host_services::sessions_with_catalog(&snapshot, &[]);
    let session_id = projected[0].id.clone();
    let revision = projected[0].workspace_revision;
    let operation_id = Uuid::new_v4().to_string();
    assert!(
        registry
            .set_agent_session_display_name(
                "not-a-uuid".into(),
                session_id.clone(),
                revision,
                Some("invalid operation".into()),
            )
            .unwrap_err()
            .to_string()
            .contains("invalid Session display-name operation ID")
    );
    let response = registry.route_node_operation(
        "unregistered-node",
        RoutedOperation::SetAgentSessionDisplayName {
            operation_id: "not-a-uuid".into(),
            session_id: session_id.clone(),
            expected_workspace_revision: revision,
            display_name: Some("invalid routed operation".into()),
        },
    );
    assert!(matches!(
        response,
        Response::Error {
            code: Some(ErrorCode::UnsupportedVersion),
            ref message,
        } if message.contains("Agent Session mutation has been removed")
    ));

    let response = registry
        .set_agent_session_display_name(
            operation_id.clone(),
            session_id.clone(),
            revision,
            Some("  Checkout   retry investigation  ".into()),
        )
        .unwrap();
    let Response::AgentSessionDisplayName { outcome: result } = response else {
        panic!("unexpected Session display-name response");
    };
    assert_eq!(
        result.user_display_name.as_deref(),
        Some("Checkout retry investigation")
    );
    assert_eq!(result.session_id, session_id);
    assert_eq!(result.workspace_id, workspace.id);
    assert_eq!(result.workspace_revision, revision + 1);
    assert!(result.changed);
    let accepted = result.clone();

    let replay = registry
        .set_agent_session_display_name(
            operation_id.clone(),
            session_id.clone(),
            revision,
            Some("Checkout retry investigation".into()),
        )
        .unwrap();
    assert_eq!(
        replay,
        Response::AgentSessionDisplayName { outcome: result }
    );
    assert_eq!(
        registry
            .set_agent_session_display_name(
                operation_id.clone(),
                session_id.clone(),
                revision,
                Some("different request".into()),
            )
            .unwrap_err()
            .wire_code(),
        ErrorCode::IdempotencyExpired
    );

    let response = registry
        .set_agent_session_display_name(
            Uuid::new_v4().to_string(),
            session_id.clone(),
            revision + 1,
            None,
        )
        .unwrap();
    let Response::AgentSessionDisplayName { outcome: result } = response else {
        panic!("unexpected Session reset response");
    };
    assert!(result.user_display_name.is_none());
    assert_eq!(result.workspace_revision, revision + 2);
    assert!(result.changed);
    let snapshot = registry.snapshot().unwrap();
    let sessions = registry
        .host_sessions(&snapshot, Some(&workspace.id))
        .unwrap();
    let session = sessions
        .iter()
        .find(|session| session.id == session_id)
        .unwrap();
    assert_eq!(session.description, "native-test");

    let replay = registry
        .set_agent_session_display_name(
            operation_id.clone(),
            session_id.clone(),
            revision,
            Some("Checkout retry investigation".into()),
        )
        .unwrap();
    assert_eq!(
        replay,
        Response::AgentSessionDisplayName { outcome: accepted }
    );

    lock(&workspace.agent_ids).unwrap().clear();
    let replay_without_projection = registry
        .set_agent_session_display_name(
            operation_id,
            session_id,
            revision,
            Some("Checkout retry investigation".into()),
        )
        .unwrap();
    assert_eq!(replay_without_projection, replay);

    registry.close_workspace(&workspace.id).unwrap();
    assert!(
        registry
            .capture_persisted_state()
            .unwrap()
            .state
            .workspaces
            .is_empty()
    );
}

#[test]
fn session_hide_is_workspace_scoped_revision_safe_and_semantically_idempotent() {
    let registry = DaemonService::default();
    let (shell, run) = recovery_shell(&registry, vec!["agent".into()]);
    let agent_id = add_recovery_agent(
        &registry,
        &shell,
        &run.id,
        "native-test",
        "external-session",
    );
    let workspace = registry.workspace(&shell.workspace_id).unwrap();
    lock(&workspace.agent_ids).unwrap().push(agent_id.clone());
    let projected = registry
        .host_sessions(&registry.snapshot().unwrap(), Some(&workspace.id))
        .unwrap();
    // The local host catalog may also contain sessions for the fixture cwd.
    let session = projected
        .iter()
        .find(|session| {
            session
                .occurrences
                .iter()
                .any(|occurrence| occurrence.agent_id == agent_id)
        })
        .unwrap();
    let session_id = session.id.clone();
    let revision = session.workspace_revision;
    let operation_id = Uuid::new_v4().to_string();

    let response = registry
        .hide_agent_session(
            operation_id.clone(),
            session_id.clone(),
            workspace.id.clone(),
            revision,
        )
        .unwrap();
    let Response::AgentSessionHidden { outcome: hidden } = response else {
        panic!("unexpected Session hide response");
    };
    assert!(hidden.changed);
    assert_eq!(hidden.workspace_revision, revision + 1);
    assert_eq!(
        registry.agent(&agent_id).unwrap().snapshot().unwrap().id,
        agent_id
    );

    for version in [50, 51] {
        for operation in [
            HostServiceOperation::ListAgentSessions {
                workspace_id: Some(workspace.id.clone()),
            },
            HostServiceOperation::InspectAgentSession {
                session_id: session_id.clone(),
            },
        ] {
            assert_eq!(
                registry
                    .host_service_for_version(operation, version)
                    .unwrap_err()
                    .wire_code(),
                ErrorCode::UnsupportedVersion
            );
        }
    }

    let replay = registry
        .hide_agent_session(
            operation_id,
            session_id.clone(),
            workspace.id.clone(),
            revision,
        )
        .unwrap();
    assert_eq!(replay, Response::AgentSessionHidden { outcome: hidden });

    assert_eq!(
        registry
            .hide_agent_session(
                Uuid::new_v4().to_string(),
                session_id.clone(),
                workspace.id.clone(),
                revision,
            )
            .unwrap_err()
            .wire_code(),
        ErrorCode::RevisionAhead
    );

    let fresh = registry
        .hide_agent_session(
            Uuid::new_v4().to_string(),
            session_id.clone(),
            workspace.id.clone(),
            revision + 1,
        )
        .unwrap();
    let Response::AgentSessionHidden { outcome: fresh } = fresh else {
        panic!("unexpected repeated Session hide response");
    };
    assert!(!fresh.changed);
    assert_eq!(fresh.workspace_revision, revision + 1);
    assert_eq!(lock(&workspace.hidden_sessions).unwrap().len(), 1);
    assert_eq!(lock(&workspace.session_hide_operations).unwrap().len(), 2);
    assert!(matches!(
        &lock(&workspace.hidden_sessions).unwrap()[0].session,
        PersistedSessionIdentity::External { external_session_id }
            if external_session_id == "external-session"
    ));
}

#[test]
fn session_display_name_persistence_failure_rolls_back_without_event() {
    let directory = env::temp_dir().join(format!("boomux-session-name-{}", Uuid::new_v4()));
    let registry =
        DaemonService::restore(StateStore::at(directory.join("state.json")), false, None).unwrap();
    let (shell, run) = recovery_shell(&registry, vec!["agent".into()]);
    let agent_id = add_recovery_agent(
        &registry,
        &shell,
        &run.id,
        "native-test",
        "external-session",
    );
    let workspace = registry.workspace(&shell.workspace_id).unwrap();
    lock(&workspace.agent_ids).unwrap().push(agent_id);
    let projected = registry
        .host_sessions(&registry.snapshot().unwrap(), Some(&workspace.id))
        .unwrap();
    let session_id = projected[0].id.clone();
    let revision = projected[0].workspace_revision;
    let events_before = registry.events.manifest().unwrap().events.len();

    registry.fail_next_persistence();
    assert_eq!(
        registry
            .set_agent_session_display_name(
                Uuid::new_v4().to_string(),
                session_id.clone(),
                revision,
                Some("not committed".into()),
            )
            .unwrap_err()
            .wire_code(),
        ErrorCode::PersistenceFailed
    );

    assert_eq!(*lock(&workspace.revision).unwrap(), revision);
    assert!(lock(&workspace.session_display_names).unwrap().is_empty());
    assert!(
        lock(&workspace.session_display_name_operations)
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        registry.events.manifest().unwrap().events.len(),
        events_before
    );

    registry
        .set_agent_session_display_name(
            Uuid::new_v4().to_string(),
            session_id,
            revision,
            Some("committed".into()),
        )
        .unwrap();
    assert_eq!(
        registry.events.manifest().unwrap().events.len(),
        events_before + 1
    );
    let persisted = registry.capture_persisted_state().unwrap();
    let receipt =
        serde_json::to_value(&persisted.state.workspaces[0].session_display_name_operations[0])
            .unwrap();
    assert_eq!(
        receipt["result"],
        serde_json::json!({
            "session_id": projected[0].id,
            "workspace_id": workspace.id,
            "user_display_name": "committed",
            "workspace_revision": revision + 1,
            "changed": true
        })
    );
    assert!(receipt.to_string().find("description").is_none());
    drop(registry);
    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn session_hide_persistence_failure_rolls_back_without_event() {
    let directory = env::temp_dir().join(format!("boomux-session-hide-{}", Uuid::new_v4()));
    let registry =
        DaemonService::restore(StateStore::at(directory.join("state.json")), false, None).unwrap();
    let (shell, run) = recovery_shell(&registry, vec!["agent".into()]);
    let agent_id = add_recovery_agent(
        &registry,
        &shell,
        &run.id,
        "native-test",
        "external-session",
    );
    let workspace = registry.workspace(&shell.workspace_id).unwrap();
    lock(&workspace.agent_ids).unwrap().push(agent_id);
    let projected = registry
        .host_sessions(&registry.snapshot().unwrap(), Some(&workspace.id))
        .unwrap();
    let session_id = projected[0].id.clone();
    let revision = projected[0].workspace_revision;
    let events_before = registry.events.manifest().unwrap().events.len();

    registry.fail_next_persistence();
    assert_eq!(
        registry
            .hide_agent_session(
                Uuid::new_v4().to_string(),
                session_id.clone(),
                workspace.id.clone(),
                revision,
            )
            .unwrap_err()
            .wire_code(),
        ErrorCode::PersistenceFailed
    );
    assert_eq!(*lock(&workspace.revision).unwrap(), revision);
    assert!(lock(&workspace.hidden_sessions).unwrap().is_empty());
    assert!(lock(&workspace.session_hide_operations).unwrap().is_empty());
    assert_eq!(
        registry.events.manifest().unwrap().events.len(),
        events_before
    );

    registry
        .hide_agent_session(
            Uuid::new_v4().to_string(),
            session_id,
            workspace.id.clone(),
            revision,
        )
        .unwrap();
    assert_eq!(
        registry.events.manifest().unwrap().events.len(),
        events_before + 1
    );
    assert_eq!(
        registry.capture_persisted_state().unwrap().state.workspaces[0]
            .hidden_sessions
            .len(),
        1
    );
    drop(registry);
    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn workspace_scoped_session_catalog_excludes_unrelated_directories() {
    let registry = DaemonService::default();
    let selected_cwd = env::temp_dir().join(format!("selected-{}", Uuid::new_v4()));
    let unrelated_cwd = env::temp_dir().join(format!("unrelated-{}", Uuid::new_v4()));
    fs::create_dir(&selected_cwd).unwrap();
    fs::create_dir(&unrelated_cwd).unwrap();
    let selected = registry
        .create_workspace(
            "selected".into(),
            vec![ShellSpec::login("selected", selected_cwd.clone())],
        )
        .unwrap();
    registry
        .create_workspace(
            "unrelated".into(),
            vec![ShellSpec::login("unrelated", unrelated_cwd.clone())],
        )
        .unwrap();

    registry
        .host_sessions(&registry.snapshot().unwrap(), Some(&selected.id))
        .unwrap();

    let catalog = lock(&registry.host_session_catalog.state).unwrap();
    assert!(
        catalog
            .entries
            .keys()
            .any(|request| request.directory == selected_cwd)
    );
    assert!(
        !catalog
            .entries
            .keys()
            .any(|request| request.directory == unrelated_cwd)
    );
    drop(catalog);
    fs::remove_dir_all(selected_cwd).unwrap();
    fs::remove_dir_all(unrelated_cwd).unwrap();
}

#[test]
fn provisional_opencode_titles_refresh_quickly_without_permanent_fast_polling() {
    let cache = HostSessionCatalogCache::default();
    let request = crate::host_session_titles::ProjectionRequest {
        integration: "opencode".into(),
        directory: "/repo".into(),
    };
    let mut session = crate::host_session_titles::HostSession {
        integration: "opencode".into(),
        root_id: "session-1".into(),
        title: "New session - 2026-09-12T05:08:10.407Z".into(),
        directory: "/repo".into(),
        created_at_ms: unix_time_ms(),
        updated_at_ms: unix_time_ms(),
    };
    let requests = [request.clone()];
    cache
        .records_with(&requests, &|_| vec![Some(vec![session.clone()])])
        .unwrap();
    lock(&cache.state)
        .unwrap()
        .entries
        .get_mut(&request)
        .unwrap()
        .inspected_at = Instant::now() - Duration::from_secs(4);
    session.title = "Casual greeting".into();
    let refreshed = cache
        .records_with(&requests, &|_| vec![Some(vec![session.clone()])])
        .unwrap();
    assert_eq!(refreshed[0].title, "Casual greeting");
    assert_eq!(
        host_session_catalog_ttl(Some(&refreshed), unix_time_ms()),
        HOST_SESSION_CATALOG_TTL
    );
    session.title = "New session - old untitled conversation".into();
    assert_eq!(
        host_session_catalog_ttl(Some(&[session.clone()]), session.created_at_ms + 30_000),
        HOST_SESSION_CATALOG_TTL
    );
    assert_eq!(
        host_session_catalog_ttl(None, unix_time_ms()),
        HOST_SESSION_CATALOG_FAILURE_TTL
    );
}

#[test]
fn session_catalog_cache_is_single_flight_without_holding_its_state_lock() {
    use std::sync::Barrier;

    let cache = Arc::new(HostSessionCatalogCache::default());
    let requests = vec![crate::host_session_titles::ProjectionRequest {
        integration: "opencode".into(),
        directory: "/repo".into(),
    }];
    let calls = Arc::new(AtomicU64::new(0));
    let barrier = Arc::new(Barrier::new(2));
    let (started_tx, started_rx) = mpsc::channel();

    let first_cache = Arc::clone(&cache);
    let first_requests = requests.clone();
    let first_calls = Arc::clone(&calls);
    let first_barrier = Arc::clone(&barrier);
    let first = thread::spawn(move || {
        first_cache
            .records_with(&first_requests, &|requests| {
                first_calls.fetch_add(1, Ordering::SeqCst);
                started_tx.send(()).unwrap();
                first_barrier.wait();
                vec![None; requests.len()]
            })
            .unwrap()
    });
    started_rx.recv_timeout(Duration::from_secs(1)).unwrap();

    let second_cache = Arc::clone(&cache);
    let second_requests = requests.clone();
    let second_calls = Arc::clone(&calls);
    let second = thread::spawn(move || {
        second_cache
            .records_with(&second_requests, &|requests| {
                second_calls.fetch_add(1, Ordering::SeqCst);
                vec![None; requests.len()]
            })
            .unwrap()
    });

    assert!(cache.state.try_lock().is_ok());
    barrier.wait();
    assert!(first.join().unwrap().is_empty());
    assert!(second.join().unwrap().is_empty());
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

#[test]
fn interrupted_claude_agent_builds_exact_resume_command() {
    let registry = DaemonService::default();
    let (shell, run) = recovery_shell(&registry, vec!["/opt/bin/claude".into()]);
    let agent_id = add_recovery_agent(&registry, &shell, &run.id, "claude", "claude-exact");

    assert_eq!(
        registry.snapshot().unwrap().workspaces[0].shells[0]
            .recovered_agent_id
            .as_deref(),
        Some(agent_id.as_str())
    );
    let resumable = registry
        .resumable_agent(&shell, Some(&run))
        .unwrap()
        .unwrap();
    assert_eq!(resumable.agent_id, agent_id);
    assert_eq!(resumable.integration, "claude");
    assert_eq!(resumable.external_session_id, "claude-exact");
    assert_eq!(
        resumable.command,
        ["/opt/bin/claude", "--resume", "claude-exact"]
    );
}

#[test]
fn interrupted_codex_agent_builds_exact_resume_subcommand() {
    let registry = DaemonService::default();
    let (shell, run) = recovery_shell(&registry, vec!["/opt/bin/codex".into()]);
    let agent_id = add_recovery_agent(&registry, &shell, &run.id, "codex", "codex-exact");

    let resumable = registry
        .resumable_agent(&shell, Some(&run))
        .unwrap()
        .unwrap();
    assert_eq!(resumable.agent_id, agent_id);
    assert_eq!(resumable.integration, "codex");
    assert_eq!(resumable.external_session_id, "codex-exact");
    assert_eq!(
        resumable.command,
        ["/opt/bin/codex", "resume", "codex-exact"]
    );
}

#[test]
fn pending_snapshot_preserves_last_outcome_without_an_agent() {
    let registry = DaemonService::default();
    let (shell, run) = recovery_shell(&registry, Vec::new());
    for reason in [
        ShellRunExitReason::Interrupted,
        ShellRunExitReason::Terminated,
        ShellRunExitReason::Exited { code: Some(99) },
    ] {
        lock(&shell.last_run).unwrap().as_mut().unwrap().exit_reason = Some(reason.clone());
        let snapshot = registry.snapshot().unwrap();
        let pending = &snapshot.workspaces[0].shells[0];
        assert_eq!(pending.status, ShellStatus::Pending);
        assert!(pending.recovered_agent_id.is_none());
        let previous = pending.run.as_ref().unwrap();
        assert_eq!(previous.id, run.id);
        assert_eq!(previous.exit_reason, Some(reason));
    }
}

#[test]
fn recovery_prefers_unique_active_session_over_inactive_history() {
    let registry = DaemonService::default();
    let (shell, run) = recovery_shell(&registry, vec!["/opt/bin/codex".into()]);
    let active = add_recovery_agent(&registry, &shell, &run.id, "codex", "active-thread");
    let old = add_recovery_agent(&registry, &shell, &run.id, "codex", "old-thread");
    let set_state = |id: &str, state| {
        let durable = lock(&registry.durable.state).unwrap();
        lock(&durable.agents[id].state).unwrap().observation.state = state;
    };
    set_state(&old, AgentState::Inactive);
    for state in [AgentState::Working, AgentState::Idle, AgentState::Blocked] {
        set_state(&active, state);
        let recovered = registry
            .resumable_agent(&shell, Some(&run))
            .unwrap()
            .unwrap();
        assert_eq!(recovered.agent_id, active);
        assert_eq!(
            recovered.command,
            ["/opt/bin/codex", "resume", "active-thread"]
        );
    }
    set_state(&old, AgentState::Idle);
    assert!(
        registry
            .resumable_agent(&shell, Some(&run))
            .unwrap()
            .is_none()
    );
    set_state(&active, AgentState::Inactive);
    set_state(&old, AgentState::Inactive);
    assert!(
        registry
            .resumable_agent(&shell, Some(&run))
            .unwrap()
            .is_none()
    );
    set_state(&old, AgentState::Done);
    assert_eq!(
        registry
            .resumable_agent(&shell, Some(&run))
            .unwrap()
            .unwrap()
            .agent_id,
        active
    );
}

#[test]
fn interrupted_kiro_agent_builds_exact_v3_resume_command() {
    let registry = DaemonService::default();
    let (shell, run) = recovery_shell(&registry, vec!["/opt/kiro/kiro-cli".into()]);
    let agent_id = add_recovery_agent(&registry, &shell, &run.id, "kiro", "kiro-exact");

    let resumable = registry
        .resumable_agent(&shell, Some(&run))
        .unwrap()
        .unwrap();
    assert_eq!(resumable.agent_id, agent_id);
    assert_eq!(resumable.integration, "kiro");
    assert_eq!(resumable.external_session_id, "kiro-exact");
    assert_eq!(
        resumable.command,
        [
            "/opt/kiro/kiro-cli",
            "--v3",
            "chat",
            "--resume-id",
            "kiro-exact",
        ]
    );
}

#[test]
fn recovery_falls_back_when_agent_identity_is_ambiguous_or_disabled() {
    let mut registry = DaemonService::default();
    let (shell, run) = recovery_shell(&registry, Vec::new());
    add_recovery_agent(&registry, &shell, &run.id, "pi", "session-1");
    add_recovery_agent(&registry, &shell, &run.id, "pi", "session-2");

    assert!(
        registry
            .resumable_agent(&shell, Some(&run))
            .unwrap()
            .is_none()
    );
    assert!(registry.recovery_presentation(&shell.id).unwrap().is_none());

    lock(&registry.durable.state)
        .unwrap()
        .agents
        .retain(|_, agent| agent.external_session_id.as_deref() == Some("session-1"));
    registry.notification_settings.resume_agents = false;
    assert!(
        registry
            .resumable_agent(&shell, Some(&run))
            .unwrap()
            .is_none()
    );
    assert!(registry.recovery_presentation(&shell.id).unwrap().is_none());
}

#[test]
fn disabling_history_persistence_clears_retained_history() {
    let registry = DaemonService::default();
    let (shell, _) = recovery_shell(&registry, Vec::new());
    lock(&shell.last_run)
        .unwrap()
        .as_mut()
        .unwrap()
        .terminal_history = Some("secret output".into());

    registry.clear_terminal_histories().unwrap();

    assert!(
        lock(&shell.last_run)
            .unwrap()
            .as_ref()
            .unwrap()
            .terminal_history
            .is_none()
    );
}

fn agent_spec(state: AgentState) -> AgentRegistrationSpec {
    AgentRegistrationSpec {
        name: "test-agent".into(),
        integration: "daemon-test".into(),
        external_session_id: Some("external-1".into()),
        report: AgentReport {
            state,
            authority: AgentAuthority::LifecycleIntegration,
            evidence: "test observation".into(),
            confidence: 90,
        },
    }
}

fn opencode_agent_spec(external_session_id: &str, state: AgentState) -> AgentRegistrationSpec {
    AgentRegistrationSpec {
        name: "opencode-agent".into(),
        integration: "opencode".into(),
        external_session_id: Some(external_session_id.into()),
        report: AgentReport {
            state,
            authority: AgentAuthority::LifecycleIntegration,
            evidence: "OpenCode attachment test".into(),
            confidence: 100,
        },
    }
}

fn agent_report(state: AgentState, authority: AgentAuthority, evidence: &str) -> AgentReport {
    AgentReport {
        state,
        authority,
        evidence: evidence.into(),
        confidence: 90,
    }
}

fn kiro_report(state: AgentState) -> AgentReport {
    AgentReport {
        state,
        authority: AgentAuthority::LifecycleIntegration,
        evidence: "Kiro test hook".into(),
        confidence: 100,
    }
}

fn test_kiro_holder(registry: &DaemonService, shell_id: &str, run_id: &str) -> (String, StdChild) {
    let child = Command::new("/bin/sleep")
        .arg("30")
        .env("BOOMUX_SHELL_ID", shell_id)
        .env("BOOMUX_RUN_ID", run_id)
        .spawn()
        .unwrap();
    let pid = child.id();
    let deadline = Instant::now() + Duration::from_secs(1);
    while Instant::now() < deadline
        && !process_has_environment(pid, b"BOOMUX_RUN_ID", run_id.as_bytes()).unwrap_or(false)
    {
        thread::sleep(Duration::from_millis(1));
    }
    let stat = platform::process_snapshot(pid).unwrap();
    let holder_id = Uuid::new_v4().to_string();
    lock(&registry.kiro.state).unwrap().insert(
        holder_id.clone(),
        KiroLaunchHolder {
            pid,
            start_time: stat.start_time,
            process_group_leader: Some(stat.group) == Some(pid as libc::pid_t),
            shell_id: shell_id.into(),
            run_id: run_id.into(),
            sessions: HashMap::new(),
        },
    );
    (holder_id, child)
}

fn report_test_kiro(
    registry: &DaemonService,
    holder_id: &str,
    session_id: &str,
) -> AgentInstanceSnapshot {
    let Response::Agent { agent } = registry
        .dispatch(Request::ReportKiroHook {
            holder_id: holder_id.into(),
            session_id: session_id.into(),
            report: kiro_report(AgentState::Working),
        })
        .unwrap()
    else {
        panic!("expected Kiro Agent");
    };
    agent
}

#[test]
fn kiro_hooks_accept_only_documented_lifecycle_states() {
    let registry = DaemonService::default();
    let (_, shell, _) = running_shell(&registry);
    let run_id = shell.snapshot().unwrap().run.unwrap().id;
    let (holder_id, mut process) = test_kiro_holder(&registry, &shell.id, &run_id);

    for state in [AgentState::Unknown, AgentState::Working, AgentState::Idle] {
        let Response::Agent { agent } = registry
            .dispatch(Request::ReportKiroHook {
                holder_id: holder_id.clone(),
                session_id: "session-a".into(),
                report: kiro_report(state),
            })
            .unwrap()
        else {
            panic!("expected Kiro Agent");
        };
        assert_eq!(agent.observation.state, state);
    }
    for state in [AgentState::Blocked, AgentState::Inactive, AgentState::Done] {
        assert!(
            registry
                .dispatch(Request::ReportKiroHook {
                    holder_id: holder_id.clone(),
                    session_id: "session-a".into(),
                    report: kiro_report(state),
                })
                .is_err(),
            "accepted unsupported Kiro state {state:?}"
        );
    }

    process.kill().unwrap();
    process.wait().unwrap();
}

#[test]
fn kiro_reconciliation_inactivates_agents_without_live_holder_authority() {
    let registry = DaemonService::default();
    let (_, shell, _) = running_shell(&registry);
    let run_id = shell.snapshot().unwrap().run.unwrap().id;
    let Response::Agent { agent: orphan } = registry
        .dispatch(Request::RegisterAgent {
            shell_id: shell.id.clone(),
            run_id: run_id.clone(),
            spec: AgentRegistrationSpec {
                name: "legacy Kiro".into(),
                integration: "kiro".into(),
                external_session_id: Some("legacy-session".into()),
                report: kiro_report(AgentState::Idle),
            },
        })
        .unwrap()
    else {
        panic!("expected registered Kiro Agent");
    };
    let (holder_id, mut process) = test_kiro_holder(&registry, &shell.id, &run_id);
    let owned = report_test_kiro(&registry, &holder_id, "owned-session");

    registry.fail_after_mutation.store(true, Ordering::Release);
    assert!(registry.reconcile_dead_kiro_holders().is_err());
    assert_eq!(
        registry
            .durable
            .agent(&orphan.id)
            .unwrap()
            .snapshot()
            .unwrap()
            .observation
            .state,
        AgentState::Idle
    );

    registry.reconcile_dead_kiro_holders().unwrap();
    let orphan = registry
        .durable
        .agent(&orphan.id)
        .unwrap()
        .snapshot()
        .unwrap();
    assert_eq!(orphan.observation.state, AgentState::Inactive);
    assert_eq!(
        orphan.observation.evidence,
        "Kiro launch authority unavailable"
    );
    assert_eq!(
        registry
            .durable
            .agent(&owned.id)
            .unwrap()
            .snapshot()
            .unwrap()
            .observation
            .state,
        AgentState::Working
    );

    process.kill().unwrap();
    process.wait().unwrap();
}

#[test]
fn kiro_sequential_processes_inactivate_exact_exited_holder_sessions() {
    let registry = DaemonService::default();
    let (_, shell, _) = running_shell(&registry);
    let run_id = shell.snapshot().unwrap().run.unwrap().id;
    let (holder_a, mut process_a) = test_kiro_holder(&registry, &shell.id, &run_id);
    let agent_a = report_test_kiro(&registry, &holder_a, "session-a");
    assert_eq!(agent_a.observation.state, AgentState::Working);

    process_a.kill().unwrap();
    process_a.wait().unwrap();
    let Response::Snapshot { snapshot } = registry.dispatch(Request::Snapshot).unwrap() else {
        panic!("expected snapshot");
    };
    assert_eq!(
        snapshot.workspaces[0].agents[0].observation.state,
        AgentState::Working
    );
    registry.fail_after_mutation.store(true, Ordering::Release);
    assert!(registry.reconcile_dead_kiro_holders().is_err());
    assert_eq!(
        registry
            .durable
            .agent(&agent_a.id)
            .unwrap()
            .snapshot()
            .unwrap()
            .observation
            .state,
        AgentState::Working
    );
    assert!(lock(&registry.kiro.state).unwrap().contains_key(&holder_a));
    registry.reconcile_dead_kiro_holders().unwrap();
    let Response::Snapshot { snapshot } = registry.dispatch(Request::Snapshot).unwrap() else {
        panic!("expected snapshot");
    };
    let inactive_a = snapshot.workspaces[0]
        .agents
        .iter()
        .find(|agent| agent.id == agent_a.id)
        .unwrap();
    assert_eq!(inactive_a.observation.state, AgentState::Inactive);
    assert!(inactive_a.attention.is_none());
    assert!(
        lock(&registry.events.state)
            .unwrap()
            .events
            .iter()
            .any(|event| {
                matches!(
                    &event.kind,
                    DaemonEventKind::AgentStateChanged { agent, .. }
                        if agent.id == agent_a.id
                            && agent.observation.state == AgentState::Inactive
                )
            })
    );
    assert!(
        registry
            .dispatch(Request::ReportKiroHook {
                holder_id: holder_a.clone(),
                session_id: "session-a".into(),
                report: kiro_report(AgentState::Working),
            })
            .is_err()
    );

    let (holder_b, mut process_b) = test_kiro_holder(&registry, &shell.id, &run_id);
    let agent_b = report_test_kiro(&registry, &holder_b, "session-b");
    let agents = lock(&registry.durable.state)
        .unwrap()
        .agents
        .values()
        .map(|agent| agent.snapshot().unwrap())
        .collect::<Vec<_>>();
    let current = agents
        .iter()
        .filter(|agent| {
            agent.run_id == run_id
                && !matches!(
                    agent.observation.state,
                    AgentState::Inactive | AgentState::Done
                )
        })
        .collect::<Vec<_>>();
    assert_eq!(current.len(), 1);
    assert_eq!(current[0].id, agent_b.id);
    assert!(
        agents
            .iter()
            .all(|agent| agent.observation.state != AgentState::Done)
    );
    process_b.kill().unwrap();
    process_b.wait().unwrap();
}

#[test]
fn kiro_holder_release_survives_its_workspace_removing_the_agent_first() {
    let registry = DaemonService::default();
    let (workspace, shell, _) = running_shell(&registry);
    let run_id = shell.snapshot().unwrap().run.unwrap().id;
    let (holder_id, mut process) = test_kiro_holder(&registry, &shell.id, &run_id);
    let agent = report_test_kiro(&registry, &holder_id, "removed-session");

    registry.close_workspace(&workspace.id).unwrap();
    assert!(registry.durable.agent(&agent.id).is_err());
    assert!(matches!(
        registry.release_kiro_launch_holder(&holder_id).unwrap(),
        Response::KiroLaunchHolderReleased { released: true }
    ));

    assert!(!lock(&registry.kiro.state).unwrap().contains_key(&holder_id));
    process.kill().unwrap();
    process.wait().unwrap();
}

#[test]
fn kiro_sessions_follow_all_and_only_their_live_holders() {
    let registry = DaemonService::default();
    let (_, shell, _) = running_shell(&registry);
    let run_id = shell.snapshot().unwrap().run.unwrap().id;
    let (holder_a, mut process_a) = test_kiro_holder(&registry, &shell.id, &run_id);
    let (holder_b, mut process_b) = test_kiro_holder(&registry, &shell.id, &run_id);
    let shared = report_test_kiro(&registry, &holder_a, "shared-session");
    let shared_again = report_test_kiro(&registry, &holder_b, "shared-session");
    assert_eq!(shared.id, shared_again.id);
    let separate = report_test_kiro(&registry, &holder_b, "separate-session");

    registry
        .dispatch(Request::ReleaseKiroLaunchHolder {
            holder_id: holder_a.clone(),
        })
        .unwrap();
    assert_eq!(
        registry
            .durable
            .agent(&shared.id)
            .unwrap()
            .snapshot()
            .unwrap()
            .observation
            .state,
        AgentState::Working
    );
    registry
        .dispatch(Request::ReleaseKiroLaunchHolder {
            holder_id: holder_b.clone(),
        })
        .unwrap();
    assert_eq!(
        registry
            .durable
            .agent(&shared.id)
            .unwrap()
            .snapshot()
            .unwrap()
            .observation
            .state,
        AgentState::Inactive
    );
    assert_eq!(
        registry
            .durable
            .agent(&separate.id)
            .unwrap()
            .snapshot()
            .unwrap()
            .observation
            .state,
        AgentState::Inactive
    );

    let (holder_c, mut process_c) = test_kiro_holder(&registry, &shell.id, &run_id);
    let reactivated = report_test_kiro(&registry, &holder_c, "shared-session");
    assert_eq!(reactivated.id, shared.id);
    assert_eq!(reactivated.observation.state, AgentState::Working);
    registry
        .dispatch(Request::ReleaseKiroLaunchHolder {
            holder_id: holder_c,
        })
        .unwrap();
    assert_eq!(
        registry
            .durable
            .agent(&shared.id)
            .unwrap()
            .snapshot()
            .unwrap()
            .observation
            .state,
        AgentState::Inactive
    );
    for process in [&mut process_a, &mut process_b, &mut process_c] {
        process.kill().unwrap();
        process.wait().unwrap();
    }
}

fn running_shell(registry: &DaemonService) -> (WorkspaceSnapshot, Arc<Shell>, Arc<ShellRuntime>) {
    let workspace = registry
        .create_workspace(
            "agents".into(),
            vec![ShellSpec {
                name: "agent-shell".into(),
                command: vec!["/bin/sleep".into(), "30".into()],
                cwd: env::temp_dir(),
            }],
        )
        .unwrap();
    let shell = registry.shell(&workspace.shells[0].id).unwrap();
    let run = Arc::new(ShellRun::new(1));
    let (runtime, _reader) = spawn_runtime(
        &shell,
        &run,
        "agents",
        "agent-shell",
        &profile(),
        None,
        RuntimeRecovery::default(),
    )
    .unwrap();
    *lock(&shell.last_run).unwrap() = Some(run.persisted(profile()).unwrap());
    *lock(&shell.lifecycle).unwrap() = ShellLifecycle::Running {
        profile: profile(),
        run,
        runtime: Arc::clone(&runtime),
    };
    (workspace, shell, runtime)
}

fn install_test_opencode_runtime(registry: &DaemonService) -> String {
    let mut command = Command::new("/bin/sleep");
    command.arg("30");
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                Err(io::Error::last_os_error())
            } else {
                Ok(())
            }
        });
    }
    let child = command.spawn().unwrap();
    let generation_id = Uuid::new_v4().to_string();
    *lock(&registry.opencode.state).unwrap() = OpenCodeCoordinatorState {
        runtime: Some(OpenCodeRuntime {
            generation_id: generation_id.clone(),
            port: 4096,
            pid: child.id(),
            process: OpenCodeRuntimeProcess::Owned(child),
        }),
        claims: HashMap::new(),
    };
    generation_id
}

#[test]
fn claude_remote_control_binding_requires_exact_active_claude_agent() {
    let registry = DaemonService::default();
    let (_, shell, _runtime) = running_shell(&registry);
    let run_id = match &*lock(&shell.lifecycle).unwrap() {
        ShellLifecycle::Running { run, .. } => run.id.clone(),
        _ => unreachable!(),
    };
    let Response::Agent { agent } = registry
        .dispatch(Request::EnsureAgent {
            shell_id: shell.id.clone(),
            run_id: run_id.clone(),
            spec: AgentRegistrationSpec {
                name: "Claude Code".into(),
                integration: "claude".into(),
                external_session_id: Some("claude-session".into()),
                report: agent_report(
                    AgentState::Idle,
                    AgentAuthority::LifecycleIntegration,
                    "Claude session idle",
                ),
            },
        })
        .unwrap()
    else {
        panic!("unexpected Agent ensure response");
    };
    let binding = registry
        .set_claude_remote_control_binding(
            &agent.id,
            &shell.id,
            &run_id,
            Some("bridge/exact".into()),
        )
        .unwrap()
        .unwrap();
    assert_eq!(binding.bridge_session_id, "bridge/exact");
    assert_eq!(
        registry
            .get_claude_remote_control_binding(&agent.id, &shell.id, &run_id)
            .unwrap(),
        Some(binding)
    );
    assert!(
        registry
            .set_claude_remote_control_binding(
                &agent.id,
                &shell.id,
                &Uuid::new_v4().to_string(),
                Some("other".into()),
            )
            .is_err()
    );
    assert!(
        registry
            .set_claude_remote_control_binding(
                &agent.id,
                &shell.id,
                &run_id,
                Some("bad\nbridge".into()),
            )
            .is_err()
    );
    registry
        .dispatch(Request::ReportAgent {
            agent_id: agent.id.clone(),
            run_id: run_id.clone(),
            report: agent_report(
                AgentState::Inactive,
                AgentAuthority::LifecycleIntegration,
                "Claude session inactive",
            ),
        })
        .unwrap();
    assert!(
        registry
            .set_claude_remote_control_binding(
                &agent.id,
                &shell.id,
                &run_id,
                Some("inactive".into()),
            )
            .is_err()
    );
    assert_eq!(
        registry
            .set_claude_remote_control_binding(&agent.id, &shell.id, &run_id, None)
            .unwrap(),
        None
    );
    assert!(
        lock(&registry.claude_remote_control.state)
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        registry
            .get_claude_remote_control_binding(&agent.id, &shell.id, &run_id)
            .unwrap_err()
            .wire_code(),
        ErrorCode::RunChanged
    );
}

#[test]
fn opencode_claims_support_renewal_multiple_holders_switch_and_safe_release() {
    let registry = DaemonService::default();
    let (_, shell, _runtime) = running_shell(&registry);
    let generation_id = install_test_opencode_runtime(&registry);
    let run_id = match &*lock(&shell.lifecycle).unwrap() {
        ShellLifecycle::Running { run, .. } => run.id.clone(),
        _ => unreachable!(),
    };
    let holder_one = Uuid::new_v4().to_string();
    let holder_two = Uuid::new_v4().to_string();
    let ensure = |holder_id: &str, root_session_id: &str| match registry
        .dispatch(Request::EnsureOpenCodeSessionClaim {
            generation_id: generation_id.clone(),
            holder_id: holder_id.into(),
            root_session_id: root_session_id.into(),
            shell_id: shell.id.clone(),
            run_id: run_id.clone(),
            spec: opencode_agent_spec(root_session_id, AgentState::Working),
        })
        .unwrap()
    {
        Response::OpenCodeSessionClaim { claim, agent } => (claim, agent),
        response => panic!("unexpected response: {response:?}"),
    };

    let (first, first_agent) = ensure(&holder_one, "ses_shared");
    let (renewed, _) = ensure(&holder_one, "ses_shared");
    assert_eq!(renewed.claim_id, first.claim_id);
    let (shared, _) = ensure(&holder_two, "ses_shared");
    assert_eq!(shared.claim_id, first.claim_id);
    assert_eq!(shared.holder_count, 2);

    let (switched, _) = ensure(&holder_one, "ses_new");
    assert_ne!(switched.claim_id, first.claim_id);
    let stale_release = registry
        .dispatch(Request::ReleaseOpenCodeSessionClaim {
            generation_id: generation_id.clone(),
            holder_id: holder_one.clone(),
            claim_id: first.claim_id,
        })
        .unwrap();
    assert!(matches!(
        stale_release,
        Response::OpenCodeSessionClaimReleased {
            released: false,
            ..
        }
    ));
    let resolved = registry
        .dispatch(Request::ResolveOpenCodeSessionClaim {
            generation_id: generation_id.clone(),
            root_session_id: "ses_shared".into(),
        })
        .unwrap();
    assert!(matches!(
        resolved,
        Response::OpenCodeSessionClaim { agent, .. } if agent.id == first_agent.id
    ));

    registry
        .dispatch(Request::ReportClaimedOpenCodeAgent {
            generation_id: generation_id.clone(),
            root_session_id: "ses_shared".into(),
            report: agent_report(
                AgentState::Blocked,
                AgentAuthority::LifecycleIntegration,
                "claimed report",
            ),
        })
        .unwrap();
    registry
        .dispatch(Request::ReportClaimedOpenCodeAgent {
            generation_id: generation_id.clone(),
            root_session_id: "ses_shared".into(),
            report: agent_report(
                AgentState::Done,
                AgentAuthority::LifecycleIntegration,
                "claimed completion",
            ),
        })
        .unwrap();
    let completed = registry
        .dispatch(Request::ResolveOpenCodeSessionClaim {
            generation_id: generation_id.clone(),
            root_session_id: "ses_shared".into(),
        })
        .unwrap_err();
    assert_eq!(completed.wire_code(), ErrorCode::NotFound);
    registry.opencode.shutdown().unwrap();
}

#[test]
fn opencode_claim_expiry_reclaims_roots_and_holders() {
    let mut state = OpenCodeCoordinatorState::default();
    state.claims.insert(
        "ses_expired".into(),
        OpenCodeRootClaim {
            claim_id: Uuid::new_v4().to_string(),
            workspace_id: Uuid::new_v4().to_string(),
            shell_id: Uuid::new_v4().to_string(),
            run_id: Uuid::new_v4().to_string(),
            agent_id: Uuid::new_v4().to_string(),
            selected_holder_id: "holder".into(),
            holders: HashMap::from([(
                "holder".into(),
                OpenCodeClaimHolder {
                    expires_at: Instant::now(),
                    expires_at_ms: 0,
                },
            )]),
        },
    );

    state.prune_claims(Instant::now());

    assert!(state.claims.is_empty());
    assert_eq!(state.holder_count(), 0);
}

#[test]
fn opencode_final_claim_release_inactivates_agent_transactionally() {
    let registry = DaemonService::default();
    let (_, shell, _runtime) = running_shell(&registry);
    let generation_id = install_test_opencode_runtime(&registry);
    let run_id = shell.snapshot().unwrap().run.unwrap().id;
    let holder_id = Uuid::new_v4().to_string();
    let final_holder_id = Uuid::new_v4().to_string();
    let (claim, agent) = match registry
        .dispatch(Request::EnsureOpenCodeSessionClaim {
            generation_id: generation_id.clone(),
            holder_id: holder_id.clone(),
            root_session_id: "ses_release".into(),
            shell_id: shell.id.clone(),
            run_id: run_id.clone(),
            spec: opencode_agent_spec("ses_release", AgentState::Working),
        })
        .unwrap()
    {
        Response::OpenCodeSessionClaim { claim, agent } => (claim, agent),
        response => panic!("unexpected response: {response:?}"),
    };
    registry
        .dispatch(Request::EnsureOpenCodeSessionClaim {
            generation_id: generation_id.clone(),
            holder_id: final_holder_id.clone(),
            root_session_id: "ses_release".into(),
            shell_id: shell.id.clone(),
            run_id,
            spec: opencode_agent_spec("ses_release", AgentState::Working),
        })
        .unwrap();

    let event_id = lock(&registry.events.state).unwrap().latest_id;
    assert!(matches!(
        registry
            .dispatch(Request::ReleaseOpenCodeSessionClaim {
                generation_id: generation_id.clone(),
                holder_id,
                claim_id: claim.claim_id.clone(),
            })
            .unwrap(),
        Response::OpenCodeSessionClaimReleased { released: true }
    ));
    assert_eq!(
        registry
            .durable
            .agent(&agent.id)
            .unwrap()
            .snapshot()
            .unwrap()
            .observation
            .state,
        AgentState::Working
    );
    assert_eq!(lock(&registry.events.state).unwrap().latest_id, event_id);

    registry.fail_after_mutation.store(true, Ordering::Release);
    registry
        .dispatch(Request::ReleaseOpenCodeSessionClaim {
            generation_id: generation_id.clone(),
            holder_id: final_holder_id.clone(),
            claim_id: claim.claim_id.clone(),
        })
        .unwrap_err();
    assert_eq!(
        registry
            .durable
            .agent(&agent.id)
            .unwrap()
            .snapshot()
            .unwrap()
            .observation
            .state,
        AgentState::Working
    );
    assert!(
        lock(&registry.opencode.state)
            .unwrap()
            .claims
            .contains_key("ses_release")
    );

    assert!(matches!(
        registry
            .dispatch(Request::ReleaseOpenCodeSessionClaim {
                generation_id: generation_id.clone(),
                holder_id: final_holder_id.clone(),
                claim_id: claim.claim_id.clone(),
            })
            .unwrap(),
        Response::OpenCodeSessionClaimReleased { released: true }
    ));
    let inactive = registry
        .durable
        .agent(&agent.id)
        .unwrap()
        .snapshot()
        .unwrap();
    assert_eq!(inactive.observation.state, AgentState::Inactive);
    assert_eq!(
        inactive.observation.evidence,
        "OpenCode session claim released"
    );
    assert!(inactive.ended_at_ms.is_none());
    assert!(inactive.attention.is_none());
    let events = lock(&registry.events.state).unwrap();
    assert_eq!(events.latest_id, event_id + 1);
    assert!(matches!(
        events.events.back().map(|event| &event.kind),
        Some(DaemonEventKind::AgentStateChanged { agent, .. })
            if agent.id == inactive.id && agent.observation.state == AgentState::Inactive
    ));
    drop(events);

    assert!(matches!(
        registry
            .dispatch(Request::ReleaseOpenCodeSessionClaim {
                generation_id,
                holder_id: final_holder_id,
                claim_id: claim.claim_id,
            })
            .unwrap(),
        Response::OpenCodeSessionClaimReleased { released: false }
    ));
    assert_eq!(
        lock(&registry.events.state).unwrap().latest_id,
        event_id + 1
    );
    registry.opencode.shutdown().unwrap();
}

#[test]
fn opencode_switching_a_sole_holder_inactivates_the_previous_agent() {
    let registry = DaemonService::default();
    let (_, shell, _runtime) = running_shell(&registry);
    let generation_id = install_test_opencode_runtime(&registry);
    let run_id = shell.snapshot().unwrap().run.unwrap().id;
    let holder_id = Uuid::new_v4().to_string();
    let ensure = |root_session_id: &str| match registry
        .dispatch(Request::EnsureOpenCodeSessionClaim {
            generation_id: generation_id.clone(),
            holder_id: holder_id.clone(),
            root_session_id: root_session_id.into(),
            shell_id: shell.id.clone(),
            run_id: run_id.clone(),
            spec: opencode_agent_spec(root_session_id, AgentState::Working),
        })
        .unwrap()
    {
        Response::OpenCodeSessionClaim { agent, .. } => agent,
        response => panic!("unexpected response: {response:?}"),
    };

    let previous = ensure("ses_previous");
    let current = ensure("ses_current");

    assert_eq!(
        registry
            .durable
            .agent(&previous.id)
            .unwrap()
            .snapshot()
            .unwrap()
            .observation
            .state,
        AgentState::Inactive
    );
    assert_eq!(current.observation.state, AgentState::Working);
    registry.opencode.shutdown().unwrap();
}

#[test]
fn failed_claim_ensure_leaves_ephemeral_selection_unchanged() {
    let registry = DaemonService::default();
    let (_, shell, _runtime) = running_shell(&registry);
    let generation_id = install_test_opencode_runtime(&registry);
    let run_id = match &*lock(&shell.lifecycle).unwrap() {
        ShellLifecycle::Running { run, .. } => run.id.clone(),
        _ => unreachable!(),
    };
    registry.fail_after_mutation.store(true, Ordering::Release);

    let error = registry
        .dispatch(Request::EnsureOpenCodeSessionClaim {
            generation_id,
            holder_id: Uuid::new_v4().to_string(),
            root_session_id: "ses_rollback".into(),
            shell_id: shell.id.clone(),
            run_id,
            spec: opencode_agent_spec("ses_rollback", AgentState::Working),
        })
        .unwrap_err();

    assert_eq!(error.wire_code(), ErrorCode::Internal);
    assert!(lock(&registry.opencode.state).unwrap().claims.is_empty());
    registry.opencode.shutdown().unwrap();
}

#[test]
fn invalid_or_failed_claim_ensure_rolls_back_all_durable_effects() {
    let registry = DaemonService::default();
    let (workspace, shell, _runtime) = running_shell(&registry);
    let generation_id = install_test_opencode_runtime(&registry);
    let run_id = shell.snapshot().unwrap().run.unwrap().id;
    let baseline = registry.capture_persisted_state().unwrap();
    let baseline_json = serde_json::to_value(&baseline.state).unwrap();
    let baseline_revision = registry
        .workspace(&workspace.id)
        .unwrap()
        .snapshot(&registry.durable)
        .unwrap()
        .revision;
    let baseline_event = lock(&registry.events.state).unwrap().latest_id;

    for (state, inject_failure) in [(AgentState::Done, false), (AgentState::Working, true)] {
        registry
            .fail_after_mutation
            .store(inject_failure, Ordering::Release);
        let result = registry.dispatch(Request::EnsureOpenCodeSessionClaim {
            generation_id: generation_id.clone(),
            holder_id: Uuid::new_v4().to_string(),
            root_session_id: format!("ses_rollback_{state:?}"),
            shell_id: shell.id.clone(),
            run_id: run_id.clone(),
            spec: opencode_agent_spec(&format!("ses_rollback_{state:?}"), state),
        });
        assert!(result.is_err());
        assert_eq!(registry.snapshot().unwrap().workspaces[0].agents.len(), 0);
        assert_eq!(
            registry
                .workspace(&workspace.id)
                .unwrap()
                .snapshot(&registry.durable)
                .unwrap()
                .revision,
            baseline_revision
        );
        assert_eq!(
            lock(&registry.events.state).unwrap().latest_id,
            baseline_event
        );
        assert_eq!(
            serde_json::to_value(&registry.capture_persisted_state().unwrap().state).unwrap(),
            baseline_json
        );
        assert!(lock(&registry.opencode.state).unwrap().claims.is_empty());
    }
    registry.opencode.shutdown().unwrap();
}

#[test]
fn claimed_report_revalidates_run_after_waiting_for_mutation_gate() {
    let registry = Arc::new(DaemonService::default());
    let (workspace, shell, runtime) = running_shell(&registry);
    let generation_id = install_test_opencode_runtime(&registry);
    let run_id = shell.snapshot().unwrap().run.unwrap().id;
    registry
        .dispatch(Request::EnsureOpenCodeSessionClaim {
            generation_id: generation_id.clone(),
            holder_id: Uuid::new_v4().to_string(),
            root_session_id: "ses_stale_report".into(),
            shell_id: shell.id.clone(),
            run_id: run_id.clone(),
            spec: opencode_agent_spec("ses_stale_report", AgentState::Working),
        })
        .unwrap();

    let mutation = lock(&registry.mutation_lock).unwrap();
    let reporting_registry = Arc::clone(&registry);
    let report = thread::spawn(move || {
        reporting_registry.dispatch(Request::ReportClaimedOpenCodeAgent {
            generation_id,
            root_session_id: "ses_stale_report".into(),
            report: agent_report(
                AgentState::Blocked,
                AgentAuthority::LifecycleIntegration,
                "must be rejected",
            ),
        })
    });
    thread::sleep(Duration::from_millis(20));
    assert!(registry.opencode.state.try_lock().is_ok());
    let replacement = Arc::new(ShellRun::new(2));
    *lock(&shell.lifecycle).unwrap() = ShellLifecycle::Running {
        profile: profile(),
        run: replacement,
        runtime,
    };
    drop(mutation);

    assert_eq!(
        report.join().unwrap().unwrap_err().wire_code(),
        ErrorCode::RunChanged
    );
    assert!(lock(&registry.opencode.state).unwrap().claims.is_empty());
    registry.opencode.shutdown().unwrap();
    shell.kill().unwrap();
    registry.close_workspace(&workspace.id).unwrap();
}

fn test_kiro_launcher_process(shell_id: &str, run_id: &str) -> StdChild {
    let child = Command::new("python3")
        .args(["-c", "import time; time.sleep(30)", "kiro", "launch"])
        .env("BOOMUX_SHELL_ID", shell_id)
        .env("BOOMUX_RUN_ID", run_id)
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(1);
    while Instant::now() < deadline
        && !process_has_environment(child.id(), b"BOOMUX_RUN_ID", run_id.as_bytes())
            .unwrap_or(false)
    {
        thread::sleep(Duration::from_millis(1));
    }
    child
}

#[test]
fn kiro_holder_acquire_revalidates_run_inside_the_mutation_gate() {
    let registry = Arc::new(DaemonService::default());
    let (workspace, shell, runtime) = running_shell(&registry);
    let run_id = shell.snapshot().unwrap().run.unwrap().id;
    let mut holder_process = test_kiro_launcher_process(&shell.id, &run_id);
    let mutation = lock(&registry.mutation_lock).unwrap();
    let acquiring = Arc::clone(&registry);
    let shell_id = shell.id.clone();
    let process_id = holder_process.id();
    let acquire = thread::spawn(move || {
        acquiring.dispatch(Request::AcquireKiroLaunchHolder {
            pid: process_id,
            shell_id,
            run_id,
        })
    });
    thread::sleep(Duration::from_millis(20));
    *lock(&shell.lifecycle).unwrap() = ShellLifecycle::Running {
        profile: profile(),
        run: Arc::new(ShellRun::new(2)),
        runtime,
    };
    drop(mutation);

    assert_eq!(
        acquire.join().unwrap().unwrap_err().wire_code(),
        ErrorCode::RunChanged
    );
    assert!(lock(&registry.kiro.state).unwrap().is_empty());
    holder_process.kill().unwrap();
    holder_process.wait().unwrap();
    shell.kill().unwrap();
    registry.close_workspace(&workspace.id).unwrap();
}

#[test]
fn kiro_handoff_import_rejects_a_noncurrent_shell_run() {
    let registry = DaemonService::default();
    let (workspace, shell, runtime) = running_shell(&registry);
    let run_id = shell.snapshot().unwrap().run.unwrap().id;
    let (holder_id, mut process) = test_kiro_holder(&registry, &shell.id, &run_id);
    report_test_kiro(&registry, &holder_id, "handoff-session");
    let transferred = registry.export_kiro_launch_holders().unwrap();
    lock(&registry.kiro.state).unwrap().clear();
    *lock(&shell.lifecycle).unwrap() = ShellLifecycle::Running {
        profile: profile(),
        run: Arc::new(ShellRun::new(2)),
        runtime,
    };

    assert!(registry.import_kiro_launch_holders(transferred).is_err());
    assert!(lock(&registry.kiro.state).unwrap().is_empty());
    process.kill().unwrap();
    process.wait().unwrap();
    shell.kill().unwrap();
    registry.close_workspace(&workspace.id).unwrap();
}

fn install_test_controller(
    runtime: &ShellRuntime,
    token: &str,
) -> mpsc::Receiver<ControllerOutput> {
    let (connection, _peer) = UnixStream::pair().unwrap();
    let (output, receiver) = mpsc::sync_channel(1);
    *lock(&runtime.controller).unwrap() = Some(Controller {
        token: token.into(),
        output,
        connection,
        reconnect_ack: None,
    });
    receiver
}

fn install_test_collaborator(
    runtime: &ShellRuntime,
    token: &str,
    capacity: usize,
) -> mpsc::Receiver<ControllerOutput> {
    let (connection, _peer) = UnixStream::pair().unwrap();
    let (output, receiver) = mpsc::sync_channel(capacity);
    lock(&runtime.collaborators).unwrap().insert(
        token.into(),
        Controller {
            token: token.into(),
            output,
            connection,
            reconnect_ack: None,
        },
    );
    receiver
}

#[test]
fn collaborative_participants_fan_out_release_and_preserve_primary_authority() {
    let registry = DaemonService::default();
    let (workspace, shell, runtime) = running_shell(&registry);
    let _primary = install_test_controller(&runtime, "primary");
    let collaborator = install_test_collaborator(&runtime, "collaborator", 2);

    assert!(ShellRuntimeManager::participant_is_authorized(&runtime, "primary").unwrap());
    assert!(ShellRuntimeManager::participant_is_authorized(&runtime, "collaborator").unwrap());
    assert!(ShellRuntimeManager::participant_is_primary(&runtime, "primary").unwrap());
    assert!(!ShellRuntimeManager::participant_is_primary(&runtime, "collaborator").unwrap());

    ShellRuntimeManager::fanout_output(&runtime, b"shared-output");
    assert!(matches!(
        collaborator.recv().unwrap(),
        ControllerOutput::Data(bytes) if bytes == b"shared-output"
    ));
    ShellRuntimeManager::release_controller(&runtime, "collaborator").unwrap();
    assert!(lock(&runtime.collaborators).unwrap().is_empty());
    assert!(lock(&runtime.controller).unwrap().is_some());

    shell.kill().unwrap();
    registry.close_workspace(&workspace.id).unwrap();
}

#[test]
fn primary_resize_fans_out_to_collaborators_only() {
    let registry = DaemonService::default();
    let (workspace, shell, runtime) = running_shell(&registry);
    let primary = install_test_controller(&runtime, "primary");
    let collaborator = install_test_collaborator(&runtime, "collaborator", 1);
    let size = PtySize {
        rows: 30,
        cols: 100,
        pixel_width: 1_000,
        pixel_height: 600,
    };

    ShellRuntimeManager::fanout_collaborator_resize(&runtime, size);

    assert!(matches!(
        collaborator.recv().unwrap(),
        ControllerOutput::Resize {
            rows: 30,
            cols: 100,
            pixel_width: 1_000,
            pixel_height: 600,
        }
    ));
    assert!(matches!(primary.try_recv(), Err(mpsc::TryRecvError::Empty)));
    shell.kill().unwrap();
    registry.close_workspace(&workspace.id).unwrap();
}

#[test]
fn slow_collaborator_is_removed_without_displacing_primary() {
    let registry = DaemonService::default();
    let (workspace, shell, runtime) = running_shell(&registry);
    let _primary = install_test_controller(&runtime, "primary");
    let collaborator = install_test_collaborator(&runtime, "slow", 1);
    lock(&runtime.collaborators)
        .unwrap()
        .get("slow")
        .unwrap()
        .output
        .try_send(ControllerOutput::Data(b"queued".to_vec()))
        .unwrap();

    ShellRuntimeManager::fanout_output(&runtime, b"new-output");

    assert!(lock(&runtime.collaborators).unwrap().is_empty());
    assert!(lock(&runtime.controller).unwrap().is_some());
    assert!(matches!(
        collaborator.recv().unwrap(),
        ControllerOutput::Data(bytes) if bytes == b"queued"
    ));
    shell.kill().unwrap();
    registry.close_workspace(&workspace.id).unwrap();
}

#[test]
fn primary_output_applies_backpressure_instead_of_disconnect() {
    let registry = DaemonService::default();
    let (workspace, shell, runtime) = running_shell(&registry);
    let primary = install_test_controller(&runtime, "primary");
    lock(&runtime.controller)
        .unwrap()
        .as_ref()
        .unwrap()
        .output
        .send(ControllerOutput::Data(b"queued".to_vec()))
        .unwrap();
    let (started_sender, started_receiver) = mpsc::sync_channel(0);
    let (completed_sender, completed_receiver) = mpsc::sync_channel(0);
    let runtime_for_output = Arc::clone(&runtime);
    let output = thread::spawn(move || {
        started_sender.send(()).unwrap();
        ShellRuntimeManager::fanout_output(&runtime_for_output, b"next");
        completed_sender.send(()).unwrap();
    });

    started_receiver.recv().unwrap();
    assert!(matches!(
        completed_receiver.recv_timeout(Duration::from_millis(100)),
        Err(mpsc::RecvTimeoutError::Timeout)
    ));
    assert!(matches!(
        primary.recv().unwrap(),
        ControllerOutput::Data(bytes) if bytes == b"queued"
    ));
    completed_receiver
        .recv_timeout(Duration::from_secs(1))
        .unwrap();
    output.join().unwrap();
    assert!(matches!(
        primary.recv().unwrap(),
        ControllerOutput::Data(bytes) if bytes == b"next"
    ));
    assert!(lock(&runtime.controller).unwrap().is_some());

    shell.kill().unwrap();
    registry.close_workspace(&workspace.id).unwrap();
}

#[test]
fn disconnected_primary_unblocks_backpressured_output() {
    let registry = DaemonService::default();
    let (workspace, shell, runtime) = running_shell(&registry);
    let primary = install_test_controller(&runtime, "primary");
    lock(&runtime.controller)
        .unwrap()
        .as_ref()
        .unwrap()
        .output
        .send(ControllerOutput::Data(b"queued".to_vec()))
        .unwrap();
    let runtime_for_output = Arc::clone(&runtime);
    let output =
        thread::spawn(move || ShellRuntimeManager::fanout_output(&runtime_for_output, b"next"));

    drop(primary);
    output.join().unwrap();
    assert!(lock(&runtime.controller).unwrap().is_none());

    shell.kill().unwrap();
    registry.close_workspace(&workspace.id).unwrap();
}

#[test]
fn exclusive_takeover_detaches_all_collaborators() {
    let registry = DaemonService::default();
    let (workspace, shell, runtime) = running_shell(&registry);
    let _primary = install_test_controller(&runtime, "primary");
    let first = install_test_collaborator(&runtime, "first", 1);
    let second = install_test_collaborator(&runtime, "second", 1);

    ShellRuntimeManager::displace_collaborators(&runtime).unwrap();

    assert!(first.recv().is_err());
    assert!(second.recv().is_err());
    assert!(lock(&runtime.collaborators).unwrap().is_empty());
    assert!(lock(&runtime.controller).unwrap().is_some());
    shell.kill().unwrap();
    registry.close_workspace(&workspace.id).unwrap();
}

#[test]
fn empty_registry_has_empty_snapshot() {
    assert!(
        DaemonService::default()
            .snapshot()
            .unwrap()
            .workspaces
            .is_empty()
    );
}

#[test]
fn exact_workspace_shell_creation_is_atomic_idempotent_and_collision_safe() {
    let registry = DaemonService::default();
    let workspace_id = Uuid::from_u128(1).to_string();
    let shell_id = Uuid::from_u128(2).to_string();
    let cwd = env::temp_dir();
    let request = Request::CreateWorkspaceShell {
        workspace_id: workspace_id.clone(),
        workspace_name: "coordinated".into(),
        default_cwd: Some(cwd.clone()),
        shell_id: shell_id.clone(),
        shell: ShellSpec {
            name: "shell".into(),
            command: vec!["bash".into(), "-lc".into(), "printf %s safe".into()],
            cwd: cwd.clone(),
        },
    };
    assert!(matches!(
        registry.dispatch(request.clone()).unwrap(),
        Response::Shell { .. }
    ));
    assert!(matches!(
        registry.dispatch(request).unwrap(),
        Response::Shell { .. }
    ));
    let snapshot = registry.snapshot().unwrap();
    assert_eq!(snapshot.workspaces.len(), 1);
    assert_eq!(snapshot.workspaces[0].id, workspace_id);
    assert_eq!(snapshot.workspaces[0].shells.len(), 1);
    assert_eq!(snapshot.workspaces[0].shells[0].id, shell_id);

    let conflict = registry.dispatch(Request::CreateWorkspaceShell {
        workspace_id: snapshot.workspaces[0].id.clone(),
        workspace_name: "coordinated".into(),
        default_cwd: Some(cwd.clone()),
        shell_id: snapshot.workspaces[0].shells[0].id.clone(),
        shell: ShellSpec {
            name: "different".into(),
            command: vec!["bash".into()],
            cwd,
        },
    });
    assert!(conflict.is_err());
    assert_eq!(registry.snapshot().unwrap().workspaces[0].shells.len(), 1);
}

#[test]
fn focus_reports_require_protocol_eighteen_and_increment_revision() {
    let registry = DaemonService::default();
    let (_workspace, shell, runtime) = running_shell(&registry);
    let run = match &*lock(&shell.lifecycle).unwrap() {
        ShellLifecycle::Running { run, .. } => Arc::clone(run),
        _ => panic!("expected running shell"),
    };

    assert!(
        registry
            .record_focus_gained(17, &shell, &run, &runtime, "controller")
            .is_err()
    );
    assert!(registry.snapshot().unwrap().focused_terminal.is_none());

    let _ = install_test_controller(&runtime, "controller");
    assert!(
        registry
            .record_focus_gained(18, &shell, &run, &runtime, "controller")
            .unwrap()
    );
    assert!(
        registry
            .record_focus_gained(18, &shell, &run, &runtime, "controller")
            .unwrap()
    );

    let focused = registry.snapshot().unwrap().focused_terminal.unwrap();
    assert_eq!(focused.revision, 2);
    assert_eq!(focused.workspace_id, shell.workspace_id);
    assert_eq!(focused.shell_id, shell.id);
    let lifecycle = lock(&shell.lifecycle).unwrap();
    let ShellLifecycle::Running { run, .. } = &*lifecycle else {
        panic!("expected running shell");
    };
    assert_eq!(focused.run_id, run.id);
    drop(lifecycle);

    *lock(&shell.lifecycle).unwrap() = ShellLifecycle::Running {
        profile: profile(),
        run: Arc::new(ShellRun::new(2)),
        runtime: Arc::clone(&runtime),
    };
    assert!(registry.snapshot().unwrap().focused_terminal.is_none());

    lock(&registry.durable.state)
        .unwrap()
        .shells
        .remove(&shell.id);
    assert!(registry.snapshot().unwrap().focused_terminal.is_none());
}

#[test]
fn presented_focus_revisions_order_local_and_remote_nodes() {
    let runtimes = ShellRuntimeManager::default();

    runtimes
        .record_presented_focus("local-node".into(), "local-shell".into())
        .unwrap();
    let local_revision = runtimes
        .presented_focused_terminal()
        .unwrap()
        .unwrap()
        .revision;
    runtimes
        .record_presented_focus("remote-node".into(), "remote-shell".into())
        .unwrap();

    let remote = runtimes.presented_focused_terminal().unwrap().unwrap();
    assert!(remote.revision > local_revision);
    assert_eq!(
        remote.shell,
        QualifiedIdentity::new("remote-node", "remote-shell")
    );
}

#[test]
fn focus_reports_from_a_replaced_controller_are_ignored() {
    let registry = DaemonService::default();
    let (_workspace, shell, runtime) = running_shell(&registry);
    let run = match &*lock(&shell.lifecycle).unwrap() {
        ShellLifecycle::Running { run, .. } => Arc::clone(run),
        _ => panic!("expected running shell"),
    };

    let _ = install_test_controller(&runtime, "old-controller");
    assert!(
        registry
            .record_focus_gained(18, &shell, &run, &runtime, "old-controller")
            .unwrap()
    );
    let _ = install_test_controller(&runtime, "current-controller");
    assert!(
        !registry
            .record_focus_gained(18, &shell, &run, &runtime, "old-controller")
            .unwrap()
    );
    assert_eq!(
        registry
            .snapshot()
            .unwrap()
            .focused_terminal
            .unwrap()
            .revision,
        1
    );

    assert!(
        registry
            .record_focus_gained(18, &shell, &run, &runtime, "current-controller")
            .unwrap()
    );
    assert_eq!(
        registry
            .snapshot()
            .unwrap()
            .focused_terminal
            .unwrap()
            .revision,
        2
    );
}

#[test]
fn focus_reports_accept_current_collaborators() {
    let registry = DaemonService::default();
    let (workspace, shell, runtime) = running_shell(&registry);
    let run = match &*lock(&shell.lifecycle).unwrap() {
        ShellLifecycle::Running { run, .. } => Arc::clone(run),
        _ => panic!("expected running shell"),
    };
    let _receiver = install_test_collaborator(&runtime, "collaborator", 1);

    assert!(
        registry
            .record_focus_gained(44, &shell, &run, &runtime, "collaborator")
            .unwrap()
    );
    assert_eq!(
        registry
            .snapshot()
            .unwrap()
            .focused_terminal
            .unwrap()
            .shell_id,
        shell.id
    );

    shell.kill().unwrap();
    registry.close_workspace(&workspace.id).unwrap();
}

#[test]
fn stale_handoff_focus_target_is_not_imported() {
    let registry = DaemonService::default();

    registry
        .import_focused_terminal(Some(FocusedTerminalSnapshot {
            revision: 9,
            workspace_id: "missing-workspace".into(),
            shell_id: "missing-shell".into(),
            run_id: "missing-run".into(),
        }))
        .unwrap();

    assert!(registry.snapshot().unwrap().focused_terminal.is_none());
    assert_eq!(lock(&registry.runtimes.focus).unwrap().revision, 9);
}

#[test]
fn explicit_replacement_pins_the_inspected_inode_and_rejects_untrusted_paths() {
    let directory = env::temp_dir().join(format!("boomux-pinned-{}", Uuid::new_v4()));
    fs::create_dir(&directory).unwrap();
    let candidate = directory.join("boomux");
    fs::write(&candidate, b"\x7fELForiginal").unwrap();
    fs::set_permissions(&candidate, fs::Permissions::from_mode(0o755)).unwrap();
    let pinned = pin_replacement_executable(&candidate).unwrap();
    assert!(pinned.as_raw_fd() > handoff::CHANNEL_FD);
    assert_ne!(
        unsafe { libc::fcntl(pinned.as_raw_fd(), libc::F_GETFD) } & libc::FD_CLOEXEC,
        0
    );
    let alternate = directory.join("alternate");
    fs::write(&alternate, b"\x7fELFreplaced").unwrap();
    fs::rename(&alternate, &candidate).unwrap();
    let bytes = fs::read(format!("/proc/self/fd/{}", pinned.as_raw_fd())).unwrap();
    assert_eq!(bytes, b"\x7fELForiginal");
    for mode in [0o644, 0o777] {
        fs::set_permissions(&candidate, fs::Permissions::from_mode(mode)).unwrap();
        assert!(pin_replacement_executable(&candidate).is_err());
    }
    fs::set_permissions(&candidate, fs::Permissions::from_mode(0o755)).unwrap();
    let link = directory.join("link");
    std::os::unix::fs::symlink(&candidate, &link).unwrap();
    assert!(pin_replacement_executable(&link).is_err());
    assert!(pin_replacement_executable(Path::new("relative/boomux")).is_err());
    fs::write(&candidate, b"#!/bin/sh\nexit 0\n").unwrap();
    assert!(pin_replacement_executable(&candidate).is_err());
    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn replacement_finds_new_binary_after_binary_replacement() {
    let directory = env::temp_dir().join(format!("boomux-replacement-{}", Uuid::new_v4()));
    let installed = directory.join("boomux");
    fs::create_dir_all(&directory).unwrap();
    fs::write(&installed, b"replacement").unwrap();

    assert_eq!(
        select_replacement_executable(
            directory.join("boomux (deleted)"),
            Some(PathBuf::from("boomux"))
        ),
        installed
    );

    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn event_batches_are_monotonic_and_paginated() {
    let registry = DaemonService::default();
    let Response::Events {
        cursor, snapshot, ..
    } = registry.read_events(None, 256, 0).unwrap()
    else {
        panic!("expected event baseline");
    };
    assert!(snapshot.is_some());
    registry
        .events
        .publish(DaemonEventKind::WorkspaceClosed {
            workspace_id: "w1".into(),
        })
        .unwrap();
    registry
        .events
        .publish(DaemonEventKind::WorkspaceClosed {
            workspace_id: "w2".into(),
        })
        .unwrap();

    let Response::Events {
        cursor: first_cursor,
        events,
        ..
    } = registry.read_events(Some(&cursor), 1, 0).unwrap()
    else {
        panic!("expected event page");
    };
    assert_eq!(events.len(), 1);
    let Response::Events { events, .. } =
        registry.read_events(Some(&first_cursor), 256, 0).unwrap()
    else {
        panic!("expected second event page");
    };
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].id, first_cursor.event_id + 1);
}

#[test]
fn event_cursor_expires_after_retention() {
    let registry = DaemonService::default();
    let Response::Events { cursor, .. } = registry.read_events(None, 256, 0).unwrap() else {
        panic!("expected event baseline");
    };
    for index in 0..=MAX_RETAINED_EVENTS {
        registry
            .events
            .publish(DaemonEventKind::WorkspaceClosed {
                workspace_id: format!("w{index}"),
            })
            .unwrap();
    }

    let error = registry.read_events(Some(&cursor), 256, 0).unwrap_err();
    assert_eq!(error.wire_code(), ErrorCode::CursorExpired);
}

#[test]
fn blocked_publication_coalesces_output_by_shell_run() {
    let mut transition = TransitionState {
        persistence_in_flight: true,
        ..TransitionState::default()
    };
    for revision in 1..=10_000 {
        transition.queue_runtime_event(DaemonEventKind::OutputChanged {
            workspace_id: "w1".into(),
            shell_id: "s1".into(),
            run_id: "r1".into(),
            output_revision: revision,
        });
    }

    assert_eq!(transition.pending_runtime_events.len(), 1);
    assert!(matches!(
        transition.pending_runtime_events.front(),
        Some(DaemonEventKind::OutputChanged {
            output_revision: 10_000,
            ..
        })
    ));
}

#[test]
fn blocked_publication_coalesces_snapshot_invalidations() {
    let mut transition = TransitionState {
        persistence_in_flight: true,
        ..TransitionState::default()
    };
    for generation in 1..=10_000 {
        transition.queue_runtime_event(DaemonEventKind::NodeProjectionChanged {
            node_id: "node-1".into(),
            cache_generation: generation,
        });
        transition.queue_runtime_event(DaemonEventKind::FocusedTerminalPresentationChanged);
    }
    transition.queue_runtime_event(DaemonEventKind::NodeProjectionChanged {
        node_id: "node-2".into(),
        cache_generation: 4,
    });

    assert_eq!(transition.pending_runtime_events.len(), 3);
    assert!(
        transition
            .pending_runtime_events
            .iter()
            .any(|event| matches!(
                event,
                DaemonEventKind::NodeProjectionChanged {
                    node_id,
                    cache_generation: 10_000,
                } if node_id == "node-1"
            ))
    );
    assert_eq!(
        transition
            .pending_runtime_events
            .iter()
            .filter(|event| matches!(event, DaemonEventKind::FocusedTerminalPresentationChanged))
            .count(),
        1
    );
}

#[test]
fn process_name_is_trimmed_sanitized_and_bounded() {
    assert_eq!(parse_process_name(b"  sleep\n"), Some("sleep".into()));
    assert_eq!(parse_process_name(b" \n\t "), None);
    assert_eq!(parse_process_name(b"bad\0name\n"), Some("bad?name".into()));
    assert_eq!(
        parse_process_name(&[b'x'; MAX_FOREGROUND_PROCESS_BYTES + 1]),
        Some("x".repeat(MAX_FOREGROUND_PROCESS_BYTES))
    );
    assert_eq!(
        proc_foreground_process_group("123 (shell name) S 1 123 123 34826 456 0"),
        Some(456)
    );
    assert_eq!(
        proc_process_group("123 (shell name) S 1 123 123 34826 456 0"),
        Some(123)
    );
}

#[test]
fn pending_and_exited_shell_snapshots_have_no_foreground_process() {
    let shell = create_pending_shell(
        "workspace-id",
        ShellSpec::login("snapshot-test", env::temp_dir()),
    )
    .unwrap();
    assert!(shell.snapshot().unwrap().foreground_process.is_none());

    let run = Arc::new(ShellRun::new(1));
    run.finish(ShellRunExitReason::Exited { code: Some(0) })
        .unwrap();
    *lock(&shell.lifecycle).unwrap() = ShellLifecycle::Exited {
        code: Some(0),
        profile: profile(),
        run,
        runtime: None,
        terminal: Arc::new(Mutex::new(TerminalState::new(24, 80))),
    };
    assert!(shell.snapshot().unwrap().foreground_process.is_none());
}

#[test]
fn running_shell_snapshot_reports_real_pty_foreground_process() {
    let registry = DaemonService::default();
    let (_workspace, shell, _runtime) = running_shell(&registry);

    let deadline = Instant::now() + FOREGROUND_PROCESS_CACHE_INTERVAL + Duration::from_secs(1);
    loop {
        let foreground_process = shell.snapshot().unwrap().foreground_process;
        if foreground_process.as_deref() == Some("sleep") {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "sleep did not become the foreground process; last observed {foreground_process:?}"
        );
        thread::sleep(IO_RETRY_DELAY);
    }
    shell.kill().unwrap();
}

#[test]
fn rejects_duplicate_shell_names_before_spawning() {
    let cwd = env::temp_dir();
    let specs = vec![
        ShellSpec::login("shell", &cwd),
        ShellSpec::login("shell", &cwd),
    ];

    let error = validate_shell_specs(&specs).unwrap_err();

    assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
}

#[test]
fn validates_terminal_profile_bounds() {
    assert!(validate_terminal_profile(&profile()).is_ok());

    let mut invalid = profile();
    invalid.rows = 0;
    assert!(validate_terminal_profile(&invalid).is_err());

    let mut invalid = profile();
    invalid.cols = MAX_TERMINAL_COLS + 1;
    assert!(validate_terminal_profile(&invalid).is_err());

    let mut invalid = profile();
    invalid.term = Some("bad\nterm".into());
    assert!(validate_terminal_profile(&invalid).is_err());

    let mut invalid = profile();
    invalid.term = Some("x".repeat(MAX_TERMINAL_ENV_VALUE + 1));
    assert!(validate_terminal_profile(&invalid).is_err());
}

#[test]
fn validates_unix_environment_without_echoing_payload() {
    let invalid_name = UnixEnvironment {
        variables: vec![protocol::UnixEnvironmentVariable {
            name: b"SECRET=NAME".to_vec(),
            value: b"secret-value".to_vec(),
        }],
    };
    let error = validate_unix_environment(&invalid_name).unwrap_err();
    assert!(!error.to_string().contains("SECRET"));
    assert!(!error.to_string().contains("secret-value"));

    let invalid_value = UnixEnvironment {
        variables: vec![protocol::UnixEnvironmentVariable {
            name: b"VALID_NAME".to_vec(),
            value: b"secret\0value".to_vec(),
        }],
    };
    assert!(validate_unix_environment(&invalid_value).is_err());

    let bytes = UnixEnvironment {
        variables: vec![protocol::UnixEnvironmentVariable {
            name: b"NON_UTF8".to_vec(),
            value: vec![0xff, 0xfe],
        }],
    };
    assert!(validate_unix_environment(&bytes).is_ok());
}

#[test]
fn listener_ownership_requires_the_exact_process_session() {
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    let session = unsafe { libc::getsid(0) } as u32;

    assert!(opencode_listener_belongs_to_session(port, session));
    assert!(!opencode_listener_belongs_to_session(port, u32::MAX));
}

#[test]
fn bounds_new_names_without_rejecting_legacy_persisted_names() {
    let long_name = "x".repeat(MAX_NAME_BYTES + 1);
    assert_eq!(
        validate_name(&long_name).unwrap_err().kind(),
        io::ErrorKind::InvalidInput
    );
    assert!(validate_persisted_name(&long_name).is_ok());
    assert_eq!(
        validate_name("real\nforged\trow").unwrap_err().kind(),
        io::ErrorKind::InvalidInput
    );
    assert!(validate_persisted_name("real\nlegacy row").is_ok());
}

#[test]
fn normalizes_and_bounds_session_display_name_metadata() {
    assert_eq!(
        normalize_session_display_name("  Checkout   retry  ").unwrap(),
        "Checkout retry"
    );
    assert!(
        normalize_session_display_name(&"x".repeat(MAX_SESSION_DISPLAY_NAME_CHARS + 1)).is_err()
    );
    assert!(normalize_session_display_name("forged\nrow").is_err());

    let record = PersistedSessionDisplayName {
        integration: "opencode".into(),
        session: PersistedSessionIdentity::External {
            external_session_id: "external".into(),
        },
        display_name: "Valid name".into(),
    };
    let mut workspace = PersistedWorkspace {
        id: Uuid::new_v4().to_string(),
        revision: 1,
        name: "work".into(),
        default_cwd: None,
        shells: Vec::new(),
        launchers: Vec::new(),
        agents: Vec::new(),
        session_display_names: vec![record.clone()],
        session_display_name_operations: Vec::new(),
        hidden_sessions: Vec::new(),
        session_hide_operations: Vec::new(),
    };
    assert!(validate_persisted_session_display_names(&workspace).is_ok());
    workspace.session_display_names[0].display_name = "not  normalized".into();
    assert_eq!(
        validate_persisted_session_display_names(&workspace)
            .unwrap_err()
            .kind(),
        io::ErrorKind::InvalidData
    );
    workspace.session_display_names = vec![record; MAX_SESSION_DISPLAY_NAMES_PER_WORKSPACE + 1];
    assert_eq!(
        validate_persisted_session_display_names(&workspace)
            .unwrap_err()
            .kind(),
        io::ErrorKind::InvalidData
    );
    workspace.session_display_names.clear();
    let hidden = PersistedHiddenSession {
        session_id: Uuid::new_v4().to_string(),
        integration: "opencode".into(),
        session: PersistedSessionIdentity::External {
            external_session_id: "external".into(),
        },
    };
    workspace.hidden_sessions = vec![hidden; MAX_HIDDEN_SESSIONS_PER_WORKSPACE + 1];
    assert_eq!(
        validate_persisted_session_display_names(&workspace)
            .unwrap_err()
            .kind(),
        io::ErrorKind::InvalidData
    );
}

#[test]
fn hidden_sessions_are_filtered_before_the_list_limit() {
    let sessions = (0..=MAX_HOST_SERVICE_SESSIONS)
        .map(|index| crate::session_projection::SessionProjection {
            id: format!("session-{index}"),
            workspace_id: "workspace".into(),
            workspace_name: "work".into(),
            integration: "opencode".into(),
            external_session_id: Some(format!("external-{index}")),
            description: format!("Session {index}"),
            user_display_name: None,
            workspace_revision: 1,
            state: AgentState::Inactive,
            state_is_current: false,
            started_at_ms: 1,
            last_at_ms: 1,
            source_cwd: None,
            occurrences: Vec::new(),
        })
        .collect::<Vec<_>>();
    let hidden = [crate::session_projection::HiddenSessionMetadata {
        workspace_id: "workspace".into(),
        integration: "opencode".into(),
        external_session_id: Some("external-0".into()),
        agent_id: None,
    }];

    let mut current = sessions.clone();
    apply_session_visibility_limit(&mut current, &hidden, 51);
    assert_eq!(current.len(), MAX_HOST_SERVICE_SESSIONS);
    assert_eq!(current[0].id, "session-1");
    assert_eq!(current.last().unwrap().id, "session-1000");

    let mut old = sessions;
    apply_session_visibility_limit(&mut old, &hidden, 50);
    assert_eq!(old.len(), MAX_HOST_SERVICE_SESSIONS);
    assert_eq!(old[0].id, "session-0");
    assert_eq!(old.last().unwrap().id, "session-999");
}

#[test]
fn protocol_thirty_one_filters_projection_invalidation_without_rewinding_cursor() {
    let cursor = EventCursor {
        stream_id: Uuid::new_v4().to_string(),
        event_id: 4,
    };
    let response = response_for_version(
        Response::Events {
            stream_id: cursor.stream_id.clone(),
            cursor: cursor.clone(),
            snapshot: None,
            events: vec![DaemonEvent {
                id: 4,
                at_ms: 10,
                kind: DaemonEventKind::NodeProjectionChanged {
                    node_id: Uuid::from_u128(2).to_string(),
                    cache_generation: 3,
                },
            }],
        },
        31,
    );
    let Response::Events {
        cursor: filtered_cursor,
        events,
        ..
    } = response
    else {
        panic!("expected events");
    };
    assert_eq!(filtered_cursor, cursor);
    assert!(events.is_empty());
}

#[test]
fn protocol_forty_nine_filters_session_display_name_fields_and_events() {
    let summary: crate::protocol::HostAgentSessionSummary =
        serde_json::from_value(serde_json::json!({
            "id": "session", "workspace_id": "workspace",
            "workspace_name": "work", "description": "User name",
            "user_display_name": "User name", "workspace_revision": 3,
            "integration": "opencode", "external_session_id": "external",
            "state": "inactive", "state_is_current": false,
            "started_at_ms": 1, "last_at_ms": 2, "occurrence_count": 1,
            "attentions": [{
                "agent_id": "agent", "reason": "completed",
                "observation_revision": 3, "observed_at_ms": 2
            }],
            "git_branch": "feat/session-radar",
            "working_contexts": [{
                "repository": "boomux", "branch": "feat/session-radar",
                "observed_at_ms": 3
            }],
            "working_context_count": 1
        }))
        .unwrap();
    let cursor = EventCursor {
        stream_id: Uuid::new_v4().to_string(),
        event_id: 9,
    };
    let response = response_for_version(
        Response::Events {
            stream_id: cursor.stream_id.clone(),
            cursor: cursor.clone(),
            snapshot: None,
            events: vec![DaemonEvent {
                id: 9,
                at_ms: 1,
                kind: DaemonEventKind::AgentSessionDisplayNameChanged {
                    workspace_id: "workspace".into(),
                    session_id: "session".into(),
                    user_display_name: Some("User name".into()),
                    workspace_revision: 3,
                },
            }],
        },
        49,
    );
    let Response::Events {
        cursor: filtered,
        events,
        ..
    } = response
    else {
        panic!("expected events response");
    };
    assert_eq!(filtered, cursor);
    assert!(events.is_empty());

    let response = response_for_version(
        Response::HostService {
            result: HostServiceResult::AgentSessions {
                sessions: vec![summary.clone()],
            },
        },
        49,
    );
    let Response::HostService {
        result: HostServiceResult::AgentSessions { sessions },
    } = response
    else {
        panic!("expected Session list response");
    };
    assert!(sessions[0].user_display_name.is_none());
    assert_eq!(sessions[0].workspace_revision, 0);
    assert_eq!(sessions[0].description, "User name");
    assert!(sessions[0].attentions.is_empty());
    assert!(sessions[0].git_branch.is_none());
    assert!(sessions[0].working_contexts.is_empty());
    assert_eq!(sessions[0].working_context_count, 0);

    let response = response_for_version(
        Response::HostService {
            result: HostServiceResult::AgentSession {
                session: crate::protocol::HostAgentSessionInspection {
                    summary,
                    source_cwd: None,
                    occurrences: Vec::new(),
                    projected_occurrences: vec![crate::protocol::HostAgentSessionOccurrence {
                        agent_id: Uuid::new_v4().to_string(),
                        shell_id: Uuid::new_v4().to_string(),
                        retained_shell_name: None,
                        retained_shell_cwd: None,
                        source_cwd: Some("/tmp/project".into()),
                        run_id: Uuid::new_v4().to_string(),
                        started_at_ms: 1,
                        ended_at_ms: None,
                        is_current: false,
                        observation: AgentObservationSnapshot {
                            revision: 1,
                            state: AgentState::Inactive,
                            authority: AgentAuthority::LifecycleIntegration,
                            evidence: "inactive".into(),
                            confidence: 100,
                            observed_at_ms: 2,
                        },
                    }],
                },
            },
        },
        49,
    );
    let Response::HostService {
        result: HostServiceResult::AgentSession { session },
    } = response
    else {
        panic!("expected Session inspection response");
    };
    assert!(session.projected_occurrences.is_empty());
    assert!(session.summary.user_display_name.is_none());
    assert_eq!(session.summary.workspace_revision, 0);
    assert!(session.summary.attentions.is_empty());
    assert!(session.summary.git_branch.is_none());
    assert!(session.summary.working_contexts.is_empty());
    assert_eq!(session.summary.working_context_count, 0);
}

#[test]
fn protocol_fifty_strips_session_response_time_git_status_from_all_host_shapes() {
    let summary: crate::protocol::HostAgentSessionSummary =
        serde_json::from_value(serde_json::json!({
            "id": "session", "workspace_id": "workspace",
            "workspace_name": "work", "description": "Session",
            "integration": "opencode", "external_session_id": "external",
            "state": "inactive", "state_is_current": false,
            "started_at_ms": 1, "last_at_ms": 2, "occurrence_count": 1,
            "working_contexts": [{
                "repository": "boomux", "branch": "feat/session-radar",
                "observed_at_ms": 3,
                "push_status": { "status": "ahead", "commit_count": 2 },
                "worktree_status": {
                    "staged": true,
                    "unstaged_or_untracked": true
                }
            }],
            "working_context_count": 1
        }))
        .unwrap();
    let responses = [
        Response::HostService {
            result: HostServiceResult::AgentSessions {
                sessions: vec![summary.clone()],
            },
        },
        Response::HostService {
            result: HostServiceResult::AgentSession {
                session: crate::protocol::HostAgentSessionInspection {
                    summary: summary.clone(),
                    source_cwd: None,
                    occurrences: Vec::new(),
                    projected_occurrences: Vec::new(),
                },
            },
        },
        Response::HostService {
            result: HostServiceResult::ResolvedAgentSession { session: summary },
        },
    ];

    for response in responses {
        for (version, retained) in [(51, true), (50, false)] {
            let response = response_for_version(response.clone(), version);
            let context = match &response {
                Response::HostService {
                    result: HostServiceResult::AgentSessions { sessions },
                } => &sessions[0].working_contexts[0],
                Response::HostService {
                    result: HostServiceResult::AgentSession { session },
                } => &session.summary.working_contexts[0],
                Response::HostService {
                    result: HostServiceResult::ResolvedAgentSession { session },
                } => &session.working_contexts[0],
                _ => panic!("expected Agent Session host-service response"),
            };
            assert_eq!(context.push_status.is_some(), retained);
            assert_eq!(context.worktree_status.is_some(), retained);
        }
    }
}

#[test]
fn protocol_fifty_filters_session_hide_events_without_rewinding_cursor() {
    let cursor = EventCursor {
        stream_id: Uuid::new_v4().to_string(),
        event_id: 9,
    };
    let response = Response::Events {
        stream_id: cursor.stream_id.clone(),
        cursor: cursor.clone(),
        snapshot: None,
        events: vec![DaemonEvent {
            id: 9,
            at_ms: 1,
            kind: DaemonEventKind::AgentSessionHidden {
                workspace_id: Uuid::from_u128(1).to_string(),
                session_id: Uuid::from_u128(2).to_string(),
                workspace_revision: 3,
            },
        }],
    };

    let Response::Events {
        cursor: current_cursor,
        events: current_events,
        ..
    } = response_for_version(response.clone(), 51)
    else {
        panic!("expected current events response");
    };
    assert_eq!(current_cursor, cursor);
    assert_eq!(current_events.len(), 1);

    let Response::Events {
        cursor: old_cursor,
        events: old_events,
        ..
    } = response_for_version(response, 50)
    else {
        panic!("expected old events response");
    };
    assert_eq!(old_cursor, cursor);
    assert!(old_events.is_empty());
}

#[test]
fn protocol_thirty_eight_filters_focus_invalidation_without_rewinding_cursor() {
    let cursor = EventCursor {
        stream_id: Uuid::new_v4().to_string(),
        event_id: 5,
    };
    let response = Response::Events {
        stream_id: cursor.stream_id.clone(),
        cursor: cursor.clone(),
        snapshot: None,
        events: vec![DaemonEvent {
            id: 5,
            at_ms: 10,
            kind: DaemonEventKind::FocusedTerminalPresentationChanged,
        }],
    };

    let Response::Events {
        cursor: current_cursor,
        events: current_events,
        ..
    } = response_for_version(response.clone(), 39)
    else {
        panic!("expected current events");
    };
    assert_eq!(current_cursor, cursor);
    assert!(matches!(
        current_events.as_slice(),
        [DaemonEvent {
            kind: DaemonEventKind::FocusedTerminalPresentationChanged,
            ..
        }]
    ));

    let Response::Events {
        cursor: filtered_cursor,
        events,
        ..
    } = response_for_version(response, 38)
    else {
        panic!("expected filtered events");
    };
    assert_eq!(filtered_cursor, cursor);
    assert!(events.is_empty());
}

#[test]
fn projection_cut_resumes_exactly_or_reseeds_on_stream_expiry() {
    let events = EventStream::new();
    let baseline = events.transaction().unwrap().cursor();
    events
        .publish(DaemonEventKind::WorkspaceCreated {
            workspace_id: "workspace-1".into(),
            name: "private-name-is-not-copied-into-transition".into(),
        })
        .unwrap();
    let transaction = events.transaction().unwrap();
    let through = transaction.cursor();
    let (mode, transitions) =
        projection_transitions(&transaction.events, Some(&baseline), &through);
    assert_eq!(mode, NodeProjectionSyncMode::Resumed);
    assert!(matches!(
        transitions.as_slice(),
        [NodeProjectionTransition {
            kind: NodeProjectionTransitionKind::Workspace { workspace_id },
            ..
        }] if workspace_id == "workspace-1"
    ));
    let expired = EventCursor {
        stream_id: Uuid::new_v4().to_string(),
        event_id: baseline.event_id,
    };
    let (mode, transitions) = projection_transitions(&transaction.events, Some(&expired), &through);
    assert_eq!(mode, NodeProjectionSyncMode::Baseline);
    assert!(transitions.is_empty());
}

#[test]
fn projection_cut_reseeds_only_beyond_transition_limit() {
    let events = EventStream::new();
    let baseline = events.transaction().unwrap().cursor();
    for index in 0..=protocol::MAX_NODE_PROJECTION_TRANSITIONS {
        events
            .publish(DaemonEventKind::WorkspaceClosed {
                workspace_id: format!("workspace-{index}"),
            })
            .unwrap();
    }
    let transaction = events.transaction().unwrap();
    let through = transaction.cursor();

    let (mode, transitions) =
        projection_transitions(&transaction.events, Some(&baseline), &through);
    assert_eq!(mode, NodeProjectionSyncMode::Baseline);
    assert!(transitions.is_empty());

    let after_first = EventCursor {
        stream_id: baseline.stream_id,
        event_id: baseline.event_id + 1,
    };
    let (mode, transitions) =
        projection_transitions(&transaction.events, Some(&after_first), &through);
    assert_eq!(mode, NodeProjectionSyncMode::Resumed);
    assert_eq!(
        transitions.len(),
        usize::from(protocol::MAX_NODE_PROJECTION_TRANSITIONS)
    );
}

fn remote_notification_test_settings() -> NotificationDeliverySettings {
    NotificationDeliverySettings {
        desktop: NotificationSettings {
            enabled: true,
            blocked: true,
            completed: true,
        },
        ..Default::default()
    }
}

fn remote_notification_projection(node_id: &str) -> NodeProjectionSnapshot {
    NodeProjectionSnapshot {
        node_id: node_id.into(),
        workspaces: vec![NodeProjectionWorkspace {
            id: "workspace-1".into(),
            name: "project".into(),
            item_count: 4,
            attention_count: 2,
        }],
        shells: vec![NodeProjectionShell {
            id: "shell-1".into(),
            workspace_id: "workspace-1".into(),
            name: "agent-shell".into(),
            status: ShellStatus::Running,
            run_id: Some("run-1".into()),
            generation: Some(1),
            started_at_ms: Some(1),
            ended_at_ms: None,
            recovered_agent_id: None,
        }],
        launchers: Vec::new(),
        agents: vec![
            NodeProjectionAgent {
                id: "agent-blocked".into(),
                workspace_id: "workspace-1".into(),
                shell_id: "shell-1".into(),
                run_id: "run-1".into(),
                name: "blocked-agent".into(),
                integration: "test".into(),
                state: AgentState::Blocked,
                observation_revision: 2,
                observed_at_ms: 2,
                started_at_ms: 1,
                ended_at_ms: None,
                attention: Some(NodeProjectionAttention {
                    reason: AgentAttentionReason::Blocked,
                    observation_revision: 2,
                    observed_at_ms: 2,
                }),
            },
            NodeProjectionAgent {
                id: "agent-done".into(),
                workspace_id: "workspace-1".into(),
                shell_id: "shell-1".into(),
                run_id: "run-1".into(),
                name: "done-agent".into(),
                integration: "test".into(),
                state: AgentState::Done,
                observation_revision: 4,
                observed_at_ms: 4,
                started_at_ms: 1,
                ended_at_ms: Some(4),
                attention: Some(NodeProjectionAttention {
                    reason: AgentAttentionReason::Completed,
                    observation_revision: 4,
                    observed_at_ms: 4,
                }),
            },
        ],
    }
}

#[test]
fn reduced_remote_transitions_classify_live_attention_and_one_reconnect_digest() {
    let node_id = Uuid::from_u128(2).to_string();
    let stream_id = Uuid::from_u128(3).to_string();
    let registration = crate::protocol::NodeRegistrationSnapshot {
        alias: "work".into(),
        target: "work.example".into(),
        node_id: node_id.clone(),
        revision: 1,
        tombstone_epoch: 0,
    };
    let sync = NodeProjectionSync {
        mode: NodeProjectionSyncMode::Resumed,
        cursor: EventCursor {
            stream_id: stream_id.clone(),
            event_id: 13,
        },
        projection: remote_notification_projection(&node_id),
        transitions: vec![
            NodeProjectionTransition {
                event_id: 11,
                at_ms: 11,
                kind: NodeProjectionTransitionKind::Agent {
                    workspace_id: "workspace-1".into(),
                    agent_id: "agent-blocked".into(),
                    revision: 2,
                },
            },
            NodeProjectionTransition {
                event_id: 12,
                at_ms: 12,
                kind: NodeProjectionTransitionKind::Agent {
                    workspace_id: "workspace-1".into(),
                    agent_id: "agent-done".into(),
                    revision: 4,
                },
            },
        ],
        capabilities: Vec::new(),
    };
    let live = ProjectionCommit {
        generation: 2,
        previous_health: Some(crate::protocol::NodeProjectionHealthCode::Online),
        previous_cursor: Some(EventCursor {
            stream_id: stream_id.clone(),
            event_id: 10,
        }),
    };
    let (requests, digest) = remote_notification_candidates(
        &registration,
        &sync,
        &live,
        &remote_notification_test_settings(),
    );
    assert_eq!(requests.len(), 2);
    assert!(digest.is_none());
    assert_eq!(requests[0].request.reason, NotificationReason::Blocked);
    assert_eq!(requests[1].request.reason, NotificationReason::Completed);
    assert_eq!(requests[0].request.node.as_ref().unwrap().alias, "work");

    let reconnect = ProjectionCommit {
        previous_health: Some(crate::protocol::NodeProjectionHealthCode::Stale),
        ..live
    };
    let (requests, digest) = remote_notification_candidates(
        &registration,
        &sync,
        &reconnect,
        &remote_notification_test_settings(),
    );
    assert!(requests.is_empty());
    let digest = digest.unwrap();
    assert_eq!(digest.claim.prior_cursor, 10);
    assert_eq!(digest.claim.through_cursor, 13);
    assert_eq!(digest.request.digest.as_ref().unwrap().blocked, 1);
    assert_eq!(digest.request.digest.as_ref().unwrap().completed, 1);
}

#[test]
fn baseline_and_stale_reduced_revisions_do_not_notify() {
    let node_id = Uuid::from_u128(2).to_string();
    let stream_id = Uuid::from_u128(3).to_string();
    let registration = crate::protocol::NodeRegistrationSnapshot {
        alias: "work".into(),
        target: "work.example".into(),
        node_id: node_id.clone(),
        revision: 1,
        tombstone_epoch: 0,
    };
    let mut sync = NodeProjectionSync {
        mode: NodeProjectionSyncMode::Baseline,
        cursor: EventCursor {
            stream_id: stream_id.clone(),
            event_id: 8,
        },
        projection: remote_notification_projection(&node_id),
        transitions: Vec::new(),
        capabilities: Vec::new(),
    };
    let commit = ProjectionCommit {
        generation: 2,
        previous_health: Some(crate::protocol::NodeProjectionHealthCode::Stale),
        previous_cursor: Some(EventCursor {
            stream_id,
            event_id: 7,
        }),
    };
    let result = remote_notification_candidates(
        &registration,
        &sync,
        &commit,
        &remote_notification_test_settings(),
    );
    assert!(result.0.is_empty() && result.1.is_none());

    sync.mode = NodeProjectionSyncMode::Resumed;
    sync.transitions.push(NodeProjectionTransition {
        event_id: 8,
        at_ms: 8,
        kind: NodeProjectionTransitionKind::Agent {
            workspace_id: "workspace-1".into(),
            agent_id: "agent-blocked".into(),
            revision: 1,
        },
    });
    let result = remote_notification_candidates(
        &registration,
        &sync,
        &commit,
        &remote_notification_test_settings(),
    );
    assert!(result.0.is_empty() && result.1.is_none());
}

#[test]
fn response_writes_time_out_when_the_client_does_not_read() {
    let (mut server, _client) = UnixStream::pair().unwrap();
    let response = Response::Error {
        message: "x".repeat(4 * 1024 * 1024),
        code: Some(ErrorCode::Busy),
    };
    let started = Instant::now();

    let error = send_response(&mut server, protocol::PROTOCOL_VERSION, response).unwrap_err();

    assert!(matches!(
        error.kind(),
        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
    ));
    assert!(started.elapsed() < Duration::from_secs(3));
}

#[test]
fn daemon_errors_convert_to_stable_codes_at_the_wire_boundary() {
    let cases = [
        (
            io::Error::new(io::ErrorKind::InvalidInput, "invalid request").into(),
            ErrorCode::InvalidArgument,
            "invalid request",
        ),
        (
            DaemonError::lifecycle(ErrorCode::RunChanged, "run changed"),
            ErrorCode::RunChanged,
            "run changed",
        ),
        (
            DaemonError::persistence(io::Error::other("state write failed")),
            ErrorCode::PersistenceFailed,
            "state write failed",
        ),
        (
            DaemonError::protocol("unsupported request"),
            ErrorCode::UnsupportedVersion,
            "unsupported request",
        ),
        (
            io::Error::other("unexpected failure").into(),
            ErrorCode::Internal,
            "unexpected failure",
        ),
    ];

    for (error, expected_code, expected_message) in cases {
        let (mut server, mut client) = UnixStream::pair().unwrap();
        send_daemon_error(&mut server, protocol::PROTOCOL_VERSION, error).unwrap();
        let response: Envelope<Response> = protocol::read_message(&mut client).unwrap();
        assert_eq!(response.version, protocol::PROTOCOL_VERSION);
        assert_eq!(
            response.message,
            Response::Error {
                message: expected_message.into(),
                code: Some(expected_code),
            }
        );
    }
}

#[test]
fn failed_persistence_rolls_back_registry_mutation() {
    let directory = env::temp_dir().join(format!("boomux-rollback-{}", Uuid::new_v4()));
    let registry = DaemonService::restore(
        StateStore::at(directory.join("state/state.json")),
        false,
        None,
    )
    .unwrap();
    registry.fail_next_persistence();

    let result = registry.dispatch(Request::CreateWorkspace {
        name: "rolled-back".into(),
        default_cwd: None,
        shells: Vec::new(),
    });

    assert!(result.is_err());
    assert!(registry.snapshot().unwrap().workspaces.is_empty());
    assert!(lock(&registry.events.state).unwrap().events.is_empty());
    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn same_name_rename_is_a_noop_without_preparing_persistence() {
    let directory = env::temp_dir().join(format!("boomux-noop-{}", Uuid::new_v4()));
    let registry = DaemonService::restore(
        StateStore::at(directory.join("state/state.json")),
        false,
        None,
    )
    .unwrap();
    let Response::Workspace { workspace } = registry
        .dispatch(Request::CreateWorkspace {
            name: "unchanged".into(),
            default_cwd: None,
            shells: vec![ShellSpec::login("shell", env::temp_dir())],
        })
        .unwrap()
    else {
        panic!("expected workspace");
    };
    let shell = workspace.shells[0].clone();
    let Response::Launcher { launcher } = registry
        .dispatch(Request::CreateLauncher {
            workspace_id: workspace.id.clone(),
            spec: WorkspaceLauncherSpec {
                name: "launcher".into(),
                cwd: env::temp_dir(),
                command: vec!["true".into()],
            },
        })
        .unwrap()
    else {
        panic!("expected launcher");
    };
    let event_id = lock(&registry.events.state).unwrap().latest_id;
    registry.fail_next_persistence();

    registry
        .dispatch(Request::RenameWorkspace {
            workspace_id: workspace.id.clone(),
            name: workspace.name.clone(),
        })
        .unwrap();

    assert_eq!(lock(&registry.events.state).unwrap().latest_id, event_id);
    assert!(
        registry
            .dispatch(Request::RenameWorkspace {
                workspace_id: workspace.id.clone(),
                name: "changed".into(),
            })
            .is_err(),
        "the no-op must not consume the injected persistence failure"
    );
    assert_eq!(registry.snapshot().unwrap().workspaces[0].name, "unchanged");
    registry.flush_pending().unwrap();

    registry.fail_next_persistence();
    registry
        .dispatch(Request::RenameShell {
            shell_id: shell.id.clone(),
            name: shell.name.clone(),
        })
        .unwrap();
    assert!(
        registry
            .dispatch(Request::RenameShell {
                shell_id: shell.id.clone(),
                name: "changed-shell".into(),
            })
            .is_err(),
        "the shell no-op must not consume the injected persistence failure"
    );
    assert_eq!(
        registry.shell(&shell.id).unwrap().snapshot().unwrap().name,
        "shell"
    );
    registry.flush_pending().unwrap();

    registry.fail_next_persistence();
    registry
        .dispatch(Request::RenameLauncher {
            launcher_id: launcher.id.clone(),
            name: launcher.name.clone(),
        })
        .unwrap();
    assert!(
        registry
            .dispatch(Request::RenameLauncher {
                launcher_id: launcher.id.clone(),
                name: "changed-launcher".into(),
            })
            .is_err(),
        "the launcher no-op must not consume the injected persistence failure"
    );
    assert_eq!(
        registry
            .launcher(&launcher.id)
            .unwrap()
            .snapshot()
            .unwrap()
            .name,
        "launcher"
    );
    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn post_mutation_errors_rollback_create_rename_and_remove_shapes() {
    let registry = DaemonService::default();
    let assert_rollback = |request| {
        let before = registry.snapshot().unwrap();
        let event_id = lock(&registry.events.state).unwrap().latest_id;
        registry.fail_after_next_mutation();
        assert!(registry.dispatch(request).is_err());
        assert_eq!(registry.snapshot().unwrap(), before);
        assert_eq!(lock(&registry.events.state).unwrap().latest_id, event_id);
    };

    assert_rollback(Request::CreateWorkspace {
        name: "explicit".into(),
        default_cwd: None,
        shells: vec![ShellSpec::login("first", env::temp_dir())],
    });
    assert_rollback(Request::CreateShell {
        workspace_id: None,
        shell: ShellSpec::login("implicit", env::temp_dir()),
    });

    let Response::Workspace { workspace } = registry
        .dispatch(Request::CreateWorkspace {
            name: "retained".into(),
            default_cwd: None,
            shells: vec![ShellSpec::login("shell", env::temp_dir())],
        })
        .unwrap()
    else {
        panic!("expected workspace");
    };
    assert_rollback(Request::CreateShell {
        workspace_id: Some(workspace.id.clone()),
        shell: ShellSpec::login("second", env::temp_dir()),
    });
    assert_rollback(Request::CreateLauncher {
        workspace_id: workspace.id.clone(),
        spec: WorkspaceLauncherSpec {
            name: "temporary".into(),
            cwd: env::temp_dir(),
            command: vec!["true".into()],
        },
    });

    let Response::Launcher { launcher } = registry
        .dispatch(Request::CreateLauncher {
            workspace_id: workspace.id.clone(),
            spec: WorkspaceLauncherSpec {
                name: "retained-launcher".into(),
                cwd: env::temp_dir(),
                command: vec!["true".into()],
            },
        })
        .unwrap()
    else {
        panic!("expected launcher");
    };
    assert_rollback(Request::RenameWorkspace {
        workspace_id: workspace.id.clone(),
        name: "renamed".into(),
    });
    assert_rollback(Request::RenameShell {
        shell_id: workspace.shells[0].id.clone(),
        name: "renamed-shell".into(),
    });
    assert_rollback(Request::RenameLauncher {
        launcher_id: launcher.id.clone(),
        name: "renamed-launcher".into(),
    });
    assert_rollback(Request::RemoveLauncher {
        launcher_id: launcher.id,
    });
}

#[test]
fn post_mutation_errors_rollback_every_agent_mutation_shape() {
    let registry = DaemonService::default();
    let (workspace, shell, _runtime) = running_shell(&registry);
    let run_id = shell.snapshot().unwrap().run.unwrap().id;
    let assert_rollback = |request| {
        let before = registry.snapshot().unwrap();
        let event_id = lock(&registry.events.state).unwrap().latest_id;
        registry.fail_after_next_mutation();
        assert!(registry.dispatch(request).is_err());
        assert_eq!(registry.snapshot().unwrap(), before);
        assert_eq!(lock(&registry.events.state).unwrap().latest_id, event_id);
    };

    assert_rollback(Request::RegisterAgent {
        shell_id: shell.id.clone(),
        run_id: run_id.clone(),
        spec: agent_spec(AgentState::Working),
    });
    let Response::Agent { agent } = registry
        .dispatch(Request::RegisterAgent {
            shell_id: shell.id.clone(),
            run_id: run_id.clone(),
            spec: agent_spec(AgentState::Working),
        })
        .unwrap()
    else {
        panic!("expected agent");
    };
    let mut ensured = agent_spec(AgentState::Working);
    ensured.external_session_id = Some("external-2".into());
    assert_rollback(Request::EnsureAgent {
        shell_id: shell.id.clone(),
        run_id: run_id.clone(),
        spec: ensured,
    });
    assert_rollback(Request::ReportAgent {
        agent_id: agent.id.clone(),
        run_id: run_id.clone(),
        report: agent_spec(AgentState::Blocked).report,
    });
    let Response::Agent { agent } = registry
        .dispatch(Request::ReportAgent {
            agent_id: agent.id,
            run_id,
            report: agent_spec(AgentState::Blocked).report,
        })
        .unwrap()
    else {
        panic!("expected blocked agent");
    };
    assert_rollback(Request::AcknowledgeAgentAttention {
        agent_id: agent.id,
        observation_revision: agent.observation.revision,
    });

    shell.kill().unwrap();
    registry.close_workspace(&workspace.id).unwrap();
}

#[test]
fn shutdown_reserves_pending_runtime_and_per_shell_compensation_capacity() {
    let registry = DaemonService::default();
    let workspace = registry
        .create_workspace(
            "capacity".into(),
            vec![
                ShellSpec::login("first", env::temp_dir()),
                ShellSpec::login("second", env::temp_dir()),
            ],
        )
        .unwrap();
    lock(&registry.events.transitions)
        .unwrap()
        .pending_runtime_events
        .push_back(DaemonEventKind::OutputChanged {
            workspace_id: workspace.id.clone(),
            shell_id: workspace.shells[0].id.clone(),
            run_id: "pending-run".into(),
            output_revision: 1,
        });
    lock(&registry.events.state).unwrap().latest_id = u64::MAX - 2;

    assert!(registry.shutdown().is_err());

    assert!(!registry.runtimes.is_stopping());
    assert_eq!(registry.snapshot().unwrap().workspaces.len(), 1);
    assert_eq!(
        lock(&registry.events.transitions)
            .unwrap()
            .lifecycle_event_reservation,
        0
    );
    lock(&registry.events.state).unwrap().latest_id = 0;
    lock(&registry.events.transitions)
        .unwrap()
        .pending_runtime_events
        .clear();
    registry.close_workspace(&workspace.id).unwrap();
}

#[test]
fn deferred_natural_exit_retries_after_unrelated_lifecycle_reservation() {
    let registry = Arc::new(DaemonService::default());
    let (workspace, target, _target_runtime) = running_shell(&registry);
    let unrelated_snapshot = registry
        .create_shell(
            &workspace.id,
            ShellSpec {
                name: "unrelated".into(),
                command: vec!["/bin/sleep".into(), "30".into()],
                cwd: env::temp_dir(),
            },
        )
        .unwrap();
    let unrelated = registry.shell(&unrelated_snapshot.id).unwrap();
    let unrelated_run = Arc::new(ShellRun::new(1));
    let (unrelated_runtime, _reader) = spawn_runtime(
        &unrelated,
        &unrelated_run,
        "agents",
        "unrelated",
        &profile(),
        None,
        RuntimeRecovery::default(),
    )
    .unwrap();
    *lock(&unrelated.last_run).unwrap() = Some(unrelated_run.persisted(profile()).unwrap());
    *lock(&unrelated.lifecycle).unwrap() = ShellLifecycle::Running {
        profile: profile(),
        run: Arc::clone(&unrelated_run),
        runtime: Arc::clone(&unrelated_runtime),
    };
    registry
        .runtimes
        .stop_runtime(&unrelated)
        .map_err(|error| error.source)
        .unwrap();
    lock(&registry.events.state).unwrap().latest_id = u64::MAX - 1;
    let mut transaction = registry.events.transaction().unwrap();
    transaction.reserve_with_pending(1).unwrap();
    transaction.begin_lifecycle_reservation(1);
    drop(transaction);

    let started = Instant::now();
    assert_eq!(
        registry
            .try_record_run_exit(&unrelated, &unrelated_run, &unrelated_runtime, Some(0))
            .unwrap(),
        RunExitRecord::Deferred
    );
    assert!(started.elapsed() < Duration::from_millis(100));
    DaemonService::defer_run_exit(
        Arc::downgrade(&registry),
        Arc::clone(&unrelated),
        Arc::clone(&unrelated_run),
        Arc::clone(&unrelated_runtime),
        Some(0),
    )
    .unwrap();

    let mut transaction = registry.events.transaction().unwrap();
    transaction.release_lifecycle_reservation();
    drop(transaction);
    registry.events.notify();

    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        let exited = matches!(
            unrelated.snapshot().unwrap().status,
            ShellStatus::Exited { code: Some(0) }
        );
        let exit_events = lock(&registry.events.state)
            .unwrap()
            .events
            .iter()
            .filter(|event| {
                matches!(
                    &event.kind,
                    DaemonEventKind::RunExited { shell_id, .. }
                        if shell_id == &unrelated.id
                )
            })
            .count();
        if exited && exit_events == 1 {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "deferred run exit did not commit"
        );
        thread::sleep(IO_RETRY_DELAY);
    }
    thread::sleep(Duration::from_millis(20));
    assert_eq!(
        lock(&registry.events.state)
            .unwrap()
            .events
            .iter()
            .filter(|event| {
                matches!(
                    &event.kind,
                    DaemonEventKind::RunExited { shell_id, .. }
                        if shell_id == &unrelated.id
                )
            })
            .count(),
        1
    );

    let mut events = lock(&registry.events.state).unwrap();
    events.events.clear();
    events.latest_id = u64::MAX - 1;
    drop(events);
    let (target_run, target_runtime) = match &*lock(&target.lifecycle).unwrap() {
        ShellLifecycle::Running { run, runtime, .. } => (Arc::clone(run), Arc::clone(runtime)),
        _ => panic!("expected running target shell"),
    };
    registry
        .runtimes
        .stop_runtime(&target)
        .map_err(|error| error.source)
        .unwrap();
    let mut transaction = registry.events.transaction().unwrap();
    transaction.reserve_with_pending(1).unwrap();
    transaction.begin_lifecycle_reservation(1);
    drop(transaction);
    assert_eq!(
        registry
            .try_record_run_exit(&target, &target_run, &target_runtime, None)
            .unwrap(),
        RunExitRecord::Deferred
    );
    DaemonService::defer_run_exit(
        Arc::downgrade(&registry),
        Arc::clone(&target),
        target_run,
        target_runtime,
        None,
    )
    .unwrap();
    let _rollback = registry.runtimes.finalize_stop(&target).unwrap();
    let mut transaction = registry.events.transaction().unwrap();
    transaction.release_lifecycle_reservation();
    drop(transaction);
    registry.events.notify();
    thread::sleep(Duration::from_millis(20));
    assert!(
        lock(&registry.events.state)
            .unwrap()
            .events
            .iter()
            .all(|event| !matches!(
                &event.kind,
                DaemonEventKind::RunExited { shell_id, .. } if shell_id == &target.id
            ))
    );

    let mut events = lock(&registry.events.state).unwrap();
    events.latest_id = 0;
    events.events.clear();
    drop(events);
    registry.close_workspace(&workspace.id).unwrap();
}

#[test]
fn failed_workspace_close_compensates_every_stopped_shell() {
    let directory = env::temp_dir().join(format!("boomux-close-compensation-{}", Uuid::new_v4()));
    let registry = DaemonService::restore(
        StateStore::at(directory.join("state/state.json")),
        false,
        None,
    )
    .unwrap();
    let (workspace, first, _runtime) = running_shell(&registry);
    let second_snapshot = registry
        .create_shell(
            &workspace.id,
            ShellSpec {
                name: "second".into(),
                command: vec!["/bin/sleep".into(), "30".into()],
                cwd: env::temp_dir(),
            },
        )
        .unwrap();
    let second = registry.shell(&second_snapshot.id).unwrap();
    let second_run = Arc::new(ShellRun::new(1));
    let (second_runtime, _reader) = spawn_runtime(
        &second,
        &second_run,
        "agents",
        "second",
        &profile(),
        None,
        RuntimeRecovery::default(),
    )
    .unwrap();
    *lock(&second.last_run).unwrap() = Some(second_run.persisted(profile()).unwrap());
    *lock(&second.lifecycle).unwrap() = ShellLifecycle::Running {
        profile: profile(),
        run: second_run,
        runtime: second_runtime,
    };
    registry.fail_next_persistence();

    assert!(registry.close_workspace(&workspace.id).is_err());

    let restored = registry
        .workspace(&workspace.id)
        .unwrap()
        .snapshot(&registry.durable)
        .unwrap();
    assert_eq!(restored.shells.len(), 2);
    assert!(
        restored
            .shells
            .iter()
            .all(|shell| shell.status == ShellStatus::Pending)
    );
    for shell in [&first, &second] {
        assert_eq!(
            shell.snapshot().unwrap().run.unwrap().exit_reason,
            Some(ShellRunExitReason::Terminated)
        );
    }
    let transitions = lock(&registry.events.transitions).unwrap();
    assert_eq!(transitions.pending_durable_events.len(), 1);
    assert_eq!(transitions.pending_durable_events[0].len(), 2);
    assert!(
        transitions.pending_durable_events[0]
            .iter()
            .all(|event| matches!(event, DaemonEventKind::RunExited { .. }))
    );
    drop(transitions);
    assert!(lock(&registry.events.state).unwrap().events.is_empty());

    registry.close_workspace(&workspace.id).unwrap();
    let events = lock(&registry.events.state).unwrap();
    assert_eq!(events.events.len(), 3);
    assert!(matches!(
        events.events.back().map(|event| &event.kind),
        Some(DaemonEventKind::WorkspaceClosed { .. })
    ));
    drop(events);
    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn slow_persistence_does_not_block_pty_draining() {
    let directory = env::temp_dir().join(format!("boomux-slow-store-{}", Uuid::new_v4()));
    let gate = Arc::new((Mutex::new((false, false, false)), Condvar::new()));
    let hook_gate = Arc::clone(&gate);
    let store = StateStore::at_with_save_hook(
        directory.join("state/state.json"),
        Arc::new(move || {
            let (state, changed) = &*hook_gate;
            let mut state = state.lock().unwrap();
            if !state.0 {
                return;
            }
            state.1 = true;
            changed.notify_all();
            while !state.2 {
                state = changed.wait(state).unwrap();
            }
        }),
    );
    let registry = Arc::new(DaemonService::restore(store, false, None).unwrap());
    let workspace = registry
            .create_workspace(
                "slow-store".into(),
                vec![ShellSpec {
                    name: "draining".into(),
                    command: vec![
                        "/bin/sh".into(),
                        "-c".into(),
                        "stty -echo; while IFS= read -r line; do printf 'observed:%s\\n' \"$line\"; done"
                            .into(),
                    ],
                    cwd: env::temp_dir(),
                }],
            )
            .unwrap();
    let shell = registry.shell(&workspace.shells[0].id).unwrap();
    let terminal_profile = profile();
    let run = Arc::new(ShellRun::new(1));
    let (runtime, reader) = spawn_runtime(
        &shell,
        &run,
        "slow-store",
        "draining",
        &terminal_profile,
        None,
        RuntimeRecovery::default(),
    )
    .unwrap();
    *lock(&shell.last_run).unwrap() = Some(run.persisted(terminal_profile.clone()).unwrap());
    *lock(&shell.lifecycle).unwrap() = ShellLifecycle::Running {
        profile: terminal_profile,
        run: Arc::clone(&run),
        runtime: Arc::clone(&runtime),
    };
    start_pty_reader(
        Arc::downgrade(&registry),
        Arc::clone(&shell),
        Arc::clone(&run),
        Arc::clone(&runtime),
        reader,
        false,
    )
    .unwrap();
    lock(&gate.0).unwrap().0 = true;

    let mutation_registry = Arc::clone(&registry);
    let workspace_id = workspace.id.clone();
    let mutation = thread::spawn(move || {
        mutation_registry.dispatch(Request::RenameWorkspace {
            workspace_id,
            name: "renamed".into(),
        })
    });
    {
        let (state, changed) = &*gate;
        let state = state.lock().unwrap();
        let (state, timeout) = changed
            .wait_timeout_while(state, Duration::from_secs(2), |state| !state.1)
            .unwrap();
        assert!(!timeout.timed_out(), "persistence write did not start");
        drop(state);
    }

    let previous_revision = run.output_revision.load(Ordering::Acquire);
    lock(&runtime.master)
        .unwrap()
        .write(b"during-save\n")
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline
        && !lock(&runtime.terminal)
            .unwrap()
            .plain_text()
            .contains("observed:during-save")
    {
        thread::sleep(IO_RETRY_DELAY);
    }
    assert!(run.output_revision.load(Ordering::Acquire) > previous_revision);
    assert!(
        lock(&runtime.terminal)
            .unwrap()
            .plain_text()
            .contains("observed:during-save")
    );
    let response = registry
        .read_shell_at(
            &shell.id,
            MAX_SHELL_READ_BYTES,
            Some(&run.id),
            Some(previous_revision),
            500,
        )
        .unwrap();
    assert!(matches!(
        response,
        Response::OutputState { changed: true, .. }
    ));

    {
        let (state, changed) = &*gate;
        let mut state = state.lock().unwrap();
        state.2 = true;
        changed.notify_all();
    }
    assert!(mutation.join().unwrap().is_ok());
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline
        && !lock(&registry.events.state)
            .unwrap()
            .events
            .iter()
            .any(|event| matches!(event.kind, DaemonEventKind::OutputChanged { .. }))
    {
        thread::sleep(IO_RETRY_DELAY);
    }
    let events = lock(&registry.events.state).unwrap();
    let rename = events
        .events
        .iter()
        .position(|event| matches!(event.kind, DaemonEventKind::WorkspaceRenamed { .. }))
        .unwrap();
    let output = events
        .events
        .iter()
        .position(|event| matches!(event.kind, DaemonEventKind::OutputChanged { .. }))
        .unwrap();
    assert!(rename < output);
    drop(events);
    shell.kill().unwrap();
    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn independent_pty_readers_process_output_concurrently() {
    let registry = Arc::new(DaemonService::default());
    let mut shells = Vec::new();
    for index in 0..2 {
        let shell = create_pending_shell(
            "workspace-id",
            ShellSpec {
                name: format!("concurrent-{index}"),
                command: vec![
                    "/bin/sh".into(),
                    "-c".into(),
                    "stty -echo; while IFS= read -r line; do printf '%s\\n' \"$line\"; done".into(),
                ],
                cwd: env::temp_dir(),
            },
        )
        .unwrap();
        let run = Arc::new(ShellRun::new(1));
        let terminal_profile = profile();
        let (runtime, reader) = spawn_runtime(
            &shell,
            &run,
            "workspace",
            &format!("concurrent-{index}"),
            &terminal_profile,
            None,
            RuntimeRecovery::default(),
        )
        .unwrap();
        *lock(&shell.last_run).unwrap() = Some(run.persisted(terminal_profile.clone()).unwrap());
        *lock(&shell.lifecycle).unwrap() = ShellLifecycle::Running {
            profile: terminal_profile,
            run: Arc::clone(&run),
            runtime: Arc::clone(&runtime),
        };
        start_pty_reader(
            Arc::downgrade(&registry),
            Arc::clone(&shell),
            Arc::clone(&run),
            Arc::clone(&runtime),
            reader,
            false,
        )
        .unwrap();
        shells.push((shell, run, runtime));
    }

    for (index, (_, _, runtime)) in shells.iter().enumerate() {
        let input = (0..500)
            .map(|line| format!("shell-{index}-{line}\n"))
            .collect::<String>();
        lock(&runtime.master)
            .unwrap()
            .write(input.as_bytes())
            .unwrap();
    }
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline
        && shells.iter().enumerate().any(|(index, (_, _, runtime))| {
            !lock(&runtime.terminal)
                .unwrap()
                .plain_text()
                .contains(&format!("shell-{index}-499"))
        })
    {
        thread::sleep(IO_RETRY_DELAY);
    }
    for (index, (shell, run, runtime)) in shells.into_iter().enumerate() {
        assert!(run.output_revision.load(Ordering::Acquire) >= 2);
        assert!(
            lock(&runtime.terminal)
                .unwrap()
                .plain_text()
                .contains(&format!("shell-{index}-499"))
        );
        shell.kill().unwrap();
    }
}

#[test]
fn coordinated_workspace_batch_is_included_in_baseline_cursor() {
    let registry = DaemonService::default();
    let response = registry
        .dispatch(Request::CreateWorkspace {
            name: "coordinated".into(),
            default_cwd: None,
            shells: vec![
                ShellSpec::login("one", env::temp_dir()),
                ShellSpec::login("two", env::temp_dir()),
            ],
        })
        .unwrap();
    assert!(matches!(response, Response::Workspace { .. }));

    let Response::Events {
        cursor,
        snapshot: Some(snapshot),
        ..
    } = registry.read_events(None, 256, 0).unwrap()
    else {
        panic!("expected coordinated baseline");
    };
    assert_eq!(snapshot.workspaces.len(), 1);
    assert_eq!(snapshot.workspaces[0].shells.len(), 2);
    assert_eq!(cursor.event_id, 3);
    let Response::Events { events, .. } = registry.read_events(Some(&cursor), 256, 0).unwrap()
    else {
        panic!("expected event page");
    };
    assert!(events.is_empty());
    let event_state = lock(&registry.events.state).unwrap();
    let events = &event_state.events;
    assert!(matches!(
        events[0].kind,
        DaemonEventKind::WorkspaceCreated { .. }
    ));
    assert!(
        events
            .iter()
            .skip(1)
            .all(|event| matches!(event.kind, DaemonEventKind::ShellCreated { .. }))
    );
}

#[test]
fn launcher_mutations_are_coordinated_and_names_are_unique_per_workspace() {
    let registry = DaemonService::default();
    let Response::Workspace { workspace } = registry
        .dispatch(Request::CreateWorkspace {
            name: "launchers".into(),
            default_cwd: None,
            shells: Vec::new(),
        })
        .unwrap()
    else {
        panic!("expected workspace");
    };
    let spec = WorkspaceLauncherSpec {
        name: "editor".into(),
        command: vec!["zeditor".into(), ".".into()],
        cwd: env::temp_dir(),
    };
    let Response::Launcher { launcher } = registry
        .dispatch(Request::CreateLauncher {
            workspace_id: workspace.id.clone(),
            spec: spec.clone(),
        })
        .unwrap()
    else {
        panic!("expected launcher");
    };
    assert!(
        registry
            .dispatch(Request::CreateLauncher {
                workspace_id: workspace.id.clone(),
                spec,
            })
            .is_err()
    );
    let snapshot = registry
        .workspace(&workspace.id)
        .unwrap()
        .snapshot(&registry.durable)
        .unwrap();
    assert_eq!(snapshot.launchers, vec![launcher]);
    assert_eq!(
        lock(&registry.events.state)
            .unwrap()
            .events
            .iter()
            .filter(|event| matches!(event.kind, DaemonEventKind::LauncherCreated { .. }))
            .count(),
        1
    );
}

#[test]
fn agent_registration_and_reports_enforce_run_binding_and_complete_monotonically() {
    let registry = DaemonService::default();
    let (workspace, shell, _runtime) = running_shell(&registry);
    let run_id = shell.snapshot().unwrap().run.unwrap().id;

    let wrong_run = registry.dispatch(Request::RegisterAgent {
        shell_id: shell.id.clone(),
        run_id: Uuid::new_v4().to_string(),
        spec: agent_spec(AgentState::Working),
    });
    assert_eq!(wrong_run.unwrap_err().wire_code(), ErrorCode::RunChanged);

    let Response::Agent { agent } = registry
        .dispatch(Request::RegisterAgent {
            shell_id: shell.id.clone(),
            run_id: run_id.clone(),
            spec: agent_spec(AgentState::Working),
        })
        .unwrap()
    else {
        panic!("expected registered agent");
    };
    assert_eq!(agent.cwd.as_deref(), Some(shell.cwd.as_path()));
    assert_eq!(agent.observation.revision, 1);
    assert!(agent.ended_at_ms.is_none());
    assert_eq!(
        registry
            .dispatch(Request::ReportAgent {
                agent_id: agent.id.clone(),
                run_id: Uuid::new_v4().to_string(),
                report: agent_spec(AgentState::Idle).report,
            })
            .unwrap_err()
            .wire_code(),
        ErrorCode::RunChanged
    );

    let Response::Agent { agent } = registry
        .dispatch(Request::ReportAgent {
            agent_id: agent.id.clone(),
            run_id,
            report: agent_spec(AgentState::Done).report,
        })
        .unwrap()
    else {
        panic!("expected completed agent");
    };
    assert_eq!(agent.observation.revision, 2);
    assert_eq!(agent.observation.state, AgentState::Done);
    assert_eq!(
        agent.attention.as_ref().map(|attention| attention.reason),
        Some(AgentAttentionReason::Completed)
    );
    assert_eq!(
        agent.attention.as_ref().unwrap().observation,
        agent.observation
    );
    assert_eq!(agent.ended_at_ms, Some(agent.observation.observed_at_ms));
    assert_eq!(
        registry.snapshot().unwrap().workspaces[0].agents,
        vec![agent.clone()]
    );
    {
        let event_state = lock(&registry.events.state).unwrap();
        assert!(matches!(
            event_state.events[0].kind,
            DaemonEventKind::AgentRegistered { .. }
        ));
        assert!(matches!(
            event_state.events[1].kind,
            DaemonEventKind::AgentCompleted { .. }
        ));
    }
    assert!(
        registry
            .dispatch(Request::ReportAgent {
                agent_id: agent.id,
                run_id: agent.run_id,
                report: agent_spec(AgentState::Idle).report,
            })
            .is_err()
    );

    shell.kill().unwrap();
    registry.close_workspace(&workspace.id).unwrap();
}

#[test]
fn agent_working_contexts_are_exact_deduplicated_and_bounded() {
    let directory = env::temp_dir().join(format!("boomux-agent-context-{}", Uuid::new_v4()));
    let repository = directory.join("boomux");
    fs::create_dir_all(&repository).unwrap();
    assert!(
        Command::new("git")
            .args(["init", "-q", "-b", "feat/working-contexts"])
            .arg(&repository)
            .status()
            .unwrap()
            .success()
    );
    let registry = DaemonService::default();
    let (workspace, shell, _runtime) = running_shell(&registry);
    let run_id = shell.snapshot().unwrap().run.unwrap().id;
    let Response::Agent { agent } = registry
        .dispatch(Request::RegisterAgent {
            shell_id: shell.id.clone(),
            run_id: run_id.clone(),
            spec: agent_spec(AgentState::Working),
        })
        .unwrap()
    else {
        panic!("expected registered Agent");
    };

    assert_eq!(
        registry
            .dispatch(Request::ObserveAgentWorkingContext {
                agent_id: agent.id.clone(),
                shell_id: shell.id.clone(),
                run_id: Uuid::new_v4().to_string(),
                path: repository.clone(),
            })
            .unwrap_err()
            .wire_code(),
        ErrorCode::RunChanged
    );
    let event_id = lock(&registry.events.state).unwrap().latest_id;
    let Response::AgentWorkingContext {
        agent: observed,
        changed,
    } = registry
        .dispatch(Request::ObserveAgentWorkingContext {
            agent_id: agent.id.clone(),
            shell_id: shell.id.clone(),
            run_id: run_id.clone(),
            path: repository.clone(),
        })
        .unwrap()
    else {
        panic!("expected working context");
    };
    assert!(changed);
    assert_eq!(observed.working_contexts.len(), 1);
    assert_eq!(observed.working_contexts[0].repository, "boomux");
    assert_eq!(observed.working_contexts[0].branch, "feat/working-contexts");
    assert!(matches!(
        lock(&registry.events.state)
            .unwrap()
            .events
            .back()
            .unwrap()
            .kind,
        DaemonEventKind::AgentWorkingContextObserved { .. }
    ));

    let Response::AgentWorkingContext { changed, .. } = registry
        .dispatch(Request::ObserveAgentWorkingContext {
            agent_id: agent.id.clone(),
            shell_id: shell.id.clone(),
            run_id: run_id.clone(),
            path: repository,
        })
        .unwrap()
    else {
        panic!("expected duplicate working context");
    };
    assert!(!changed);
    assert_eq!(
        lock(&registry.events.state).unwrap().latest_id,
        event_id + 1
    );

    for index in 0..=MAX_AGENT_WORKING_CONTEXTS {
        registry
            .durable
            .observe_agent_working_context(
                &agent.id,
                &shell.id,
                &run_id,
                AgentWorkingContextSnapshot {
                    worktree_root: format!("/worktrees/repository-{index}").into(),
                    repository: format!("repository-{index}"),
                    branch: "main".into(),
                    observed_at_ms: 0,
                },
            )
            .unwrap();
    }
    let bounded = registry.agent(&agent.id).unwrap().snapshot().unwrap();
    assert_eq!(bounded.working_contexts.len(), MAX_AGENT_WORKING_CONTEXTS);
    assert_eq!(bounded.working_contexts[0].repository, "repository-8");
    assert!(
        bounded
            .working_contexts
            .iter()
            .all(|context| context.repository != "repository-0")
    );

    shell.kill().unwrap();
    registry.close_workspace(&workspace.id).unwrap();
    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn failed_working_context_persistence_rolls_back_without_event() {
    let directory = env::temp_dir().join(format!("boomux-agent-context-undo-{}", Uuid::new_v4()));
    let repository = directory.join("boomux");
    fs::create_dir_all(&repository).unwrap();
    assert!(
        Command::new("git")
            .args(["init", "-q", "-b", "feat/working-contexts"])
            .arg(&repository)
            .status()
            .unwrap()
            .success()
    );
    let registry = DaemonService::restore(
        StateStore::at(directory.join("state/state.json")),
        false,
        None,
    )
    .unwrap();
    let (workspace, shell, _runtime) = running_shell(&registry);
    let run_id = shell.snapshot().unwrap().run.unwrap().id;
    let Response::Agent { agent } = registry
        .dispatch(Request::RegisterAgent {
            shell_id: shell.id.clone(),
            run_id: run_id.clone(),
            spec: agent_spec(AgentState::Working),
        })
        .unwrap()
    else {
        panic!("expected registered Agent");
    };
    let event_id = lock(&registry.events.state).unwrap().latest_id;
    registry.fail_next_persistence();

    let error = registry
        .dispatch(Request::ObserveAgentWorkingContext {
            agent_id: agent.id.clone(),
            shell_id: shell.id.clone(),
            run_id: run_id.clone(),
            path: repository.clone(),
        })
        .unwrap_err();

    assert_eq!(error.wire_code(), ErrorCode::PersistenceFailed);
    assert!(
        registry
            .agent(&agent.id)
            .unwrap()
            .snapshot()
            .unwrap()
            .working_contexts
            .is_empty()
    );
    assert_eq!(lock(&registry.events.state).unwrap().latest_id, event_id);
    let Response::AgentWorkingContext { agent, changed } = registry
        .dispatch(Request::ObserveAgentWorkingContext {
            agent_id: agent.id,
            shell_id: shell.id.clone(),
            run_id,
            path: repository,
        })
        .unwrap()
    else {
        panic!("expected working context");
    };
    assert!(changed);
    assert_eq!(agent.working_contexts.len(), 1);

    shell.kill().unwrap();
    registry.close_workspace(&workspace.id).unwrap();
    drop(registry);
    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn ensure_agent_reuses_identity_without_events_or_revision_changes() {
    let registry = DaemonService::default();
    let (workspace, shell, _runtime) = running_shell(&registry);
    let run_id = shell.snapshot().unwrap().run.unwrap().id;
    let spec = agent_spec(AgentState::Working);

    let Response::Agent { agent: created } = registry
        .dispatch(Request::EnsureAgent {
            shell_id: shell.id.clone(),
            run_id: run_id.clone(),
            spec: spec.clone(),
        })
        .unwrap()
    else {
        panic!("expected ensured agent");
    };
    let event_id = lock(&registry.events.state).unwrap().latest_id;
    let Response::Agent { agent: reused } = registry
        .dispatch(Request::EnsureAgent {
            shell_id: shell.id.clone(),
            run_id,
            spec,
        })
        .unwrap()
    else {
        panic!("expected reused agent");
    };

    assert_eq!(reused, created);
    assert_eq!(lock(&registry.events.state).unwrap().latest_id, event_id);
    assert_eq!(registry.snapshot().unwrap().workspaces[0].agents.len(), 1);
    shell.kill().unwrap();
    registry.close_workspace(&workspace.id).unwrap();
}

#[test]
fn concurrent_ensure_agent_creates_one_identity() {
    let registry = Arc::new(DaemonService::default());
    let (workspace, shell, _runtime) = running_shell(&registry);
    let run_id = shell.snapshot().unwrap().run.unwrap().id;
    let barrier = Arc::new(Barrier::new(3));
    let mut threads = Vec::new();
    for _ in 0..2 {
        let registry = Arc::clone(&registry);
        let shell_id = shell.id.clone();
        let run_id = run_id.clone();
        let barrier = Arc::clone(&barrier);
        threads.push(thread::spawn(move || {
            barrier.wait();
            let Response::Agent { agent } = registry
                .dispatch(Request::EnsureAgent {
                    shell_id,
                    run_id,
                    spec: agent_spec(AgentState::Working),
                })
                .unwrap()
            else {
                panic!("expected ensured agent");
            };
            agent.id
        }));
    }
    barrier.wait();
    let ids = threads
        .into_iter()
        .map(|thread| thread.join().unwrap())
        .collect::<Vec<_>>();

    assert_eq!(ids[0], ids[1]);
    assert_eq!(registry.snapshot().unwrap().workspaces[0].agents.len(), 1);
    shell.kill().unwrap();
    registry.close_workspace(&workspace.id).unwrap();
}

#[test]
fn ensure_agent_resolves_only_a_unique_active_legacy_match() {
    let registry = DaemonService::default();
    let (workspace, shell, _runtime) = running_shell(&registry);
    let run_id = shell.snapshot().unwrap().run.unwrap().id;
    let completed = registry
        .register_agent(&shell.id, &run_id, agent_spec(AgentState::Done))
        .unwrap();
    let active = registry
        .register_agent(&shell.id, &run_id, agent_spec(AgentState::Working))
        .unwrap();

    let (ensured, created) = registry
        .ensure_agent(&shell.id, &run_id, agent_spec(AgentState::Working))
        .unwrap();
    assert!(!created);
    assert_eq!(ensured.id, active.id);
    assert_ne!(ensured.id, completed.id);

    registry
        .register_agent(&shell.id, &run_id, agent_spec(AgentState::Working))
        .unwrap();
    assert_eq!(
        registry
            .ensure_agent(&shell.id, &run_id, agent_spec(AgentState::Working))
            .unwrap_err()
            .wire_code(),
        ErrorCode::AlreadyExists
    );
    shell.kill().unwrap();
    registry.close_workspace(&workspace.id).unwrap();
}

#[test]
fn ensure_agent_requires_external_id_and_distinguishes_runs() {
    let registry = DaemonService::default();
    let (workspace, shell, runtime) = running_shell(&registry);
    let first_run_id = shell.snapshot().unwrap().run.unwrap().id;
    let mut missing_id = agent_spec(AgentState::Working);
    missing_id.external_session_id = None;
    assert_eq!(
        registry
            .dispatch(Request::EnsureAgent {
                shell_id: shell.id.clone(),
                run_id: first_run_id.clone(),
                spec: missing_id,
            })
            .unwrap_err()
            .wire_code(),
        ErrorCode::InvalidArgument
    );
    let Response::Agent { agent: first } = registry
        .dispatch(Request::EnsureAgent {
            shell_id: shell.id.clone(),
            run_id: first_run_id.clone(),
            spec: agent_spec(AgentState::Working),
        })
        .unwrap()
    else {
        panic!("expected first agent");
    };

    let second_run = Arc::new(ShellRun::new(2));
    *lock(&shell.lifecycle).unwrap() = ShellLifecycle::Running {
        profile: profile(),
        run: Arc::clone(&second_run),
        runtime: Arc::clone(&runtime),
    };
    let Response::Agent { agent: recovered } = registry
        .dispatch(Request::EnsureAgent {
            shell_id: shell.id.clone(),
            run_id: first_run_id,
            spec: agent_spec(AgentState::Working),
        })
        .unwrap()
    else {
        panic!("expected recovered first agent");
    };
    let Response::Agent { agent: second } = registry
        .dispatch(Request::EnsureAgent {
            shell_id: shell.id.clone(),
            run_id: second_run.id.clone(),
            spec: agent_spec(AgentState::Working),
        })
        .unwrap()
    else {
        panic!("expected second agent");
    };

    assert_eq!(recovered.id, first.id);
    assert_ne!(second.id, first.id);
    shell.kill().unwrap();
    registry.close_workspace(&workspace.id).unwrap();
}

#[test]
fn reports_obey_authority_and_idempotent_completion_rules() {
    let registry = DaemonService::default();
    let (workspace, shell, _runtime) = running_shell(&registry);
    let run_id = shell.snapshot().unwrap().run.unwrap().id;
    let mut spec = agent_spec(AgentState::Working);
    spec.report = agent_report(
        AgentState::Working,
        AgentAuthority::ProcessAdapter,
        "process working",
    );
    let Response::Agent { agent } = registry
        .dispatch(Request::RegisterAgent {
            shell_id: shell.id.clone(),
            run_id: run_id.clone(),
            spec,
        })
        .unwrap()
    else {
        panic!("expected agent");
    };
    let registered_event_id = lock(&registry.events.state).unwrap().latest_id;

    for report in [
        agent_report(
            AgentState::Done,
            AgentAuthority::TerminalHeuristic,
            "weak done",
        ),
        agent_report(
            AgentState::Working,
            AgentAuthority::ProcessAdapter,
            "process working",
        ),
        agent_report(
            AgentState::Working,
            AgentAuthority::ProcessAdapter,
            "updated working evidence",
        ),
    ] {
        let Response::Agent { agent: unchanged } = registry
            .dispatch(Request::ReportAgent {
                agent_id: agent.id.clone(),
                run_id: run_id.clone(),
                report,
            })
            .unwrap()
        else {
            panic!("expected unchanged agent");
        };
        assert_eq!(unchanged, agent);
    }
    assert_eq!(
        lock(&registry.events.state).unwrap().latest_id,
        registered_event_id
    );

    let completion = agent_report(
        AgentState::Done,
        AgentAuthority::LifecycleIntegration,
        "lifecycle done",
    );
    let Response::Agent { agent: completed } = registry
        .dispatch(Request::ReportAgent {
            agent_id: agent.id.clone(),
            run_id: run_id.clone(),
            report: completion.clone(),
        })
        .unwrap()
    else {
        panic!("expected completed agent");
    };
    let completion_event_id = lock(&registry.events.state).unwrap().latest_id;
    let Response::Agent { agent: retried } = registry
        .dispatch(Request::ReportAgent {
            agent_id: agent.id.clone(),
            run_id: run_id.clone(),
            report: completion,
        })
        .unwrap()
    else {
        panic!("expected retried completion");
    };
    assert_eq!(retried, completed);
    assert_eq!(
        lock(&registry.events.state).unwrap().latest_id,
        completion_event_id
    );
    assert!(
        registry
            .dispatch(Request::ReportAgent {
                agent_id: agent.id.clone(),
                run_id: run_id.clone(),
                report: agent_report(
                    AgentState::Done,
                    AgentAuthority::LifecycleIntegration,
                    "conflicting done",
                ),
            })
            .is_err()
    );
    assert!(
        registry
            .dispatch(Request::ReportAgent {
                agent_id: agent.id,
                run_id,
                report: agent_report(
                    AgentState::Done,
                    AgentAuthority::DaemonLifecycle,
                    "external daemon claim",
                ),
            })
            .is_err()
    );
    shell.kill().unwrap();
    registry.close_workspace(&workspace.id).unwrap();
}

#[test]
fn failed_agent_mutation_restores_observation_revision() {
    let directory = env::temp_dir().join(format!("boomux-agent-undo-{}", Uuid::new_v4()));
    let registry = DaemonService::restore(
        StateStore::at(directory.join("state/state.json")),
        false,
        None,
    )
    .unwrap();
    let (workspace, shell, _runtime) = running_shell(&registry);
    let run_id = shell.snapshot().unwrap().run.unwrap().id;
    let Response::Agent { agent } = registry
        .dispatch(Request::RegisterAgent {
            shell_id: shell.id.clone(),
            run_id: run_id.clone(),
            spec: agent_spec(AgentState::Working),
        })
        .unwrap()
    else {
        panic!("expected agent");
    };
    registry.fail_next_persistence();

    let result = registry.dispatch(Request::ReportAgent {
        agent_id: agent.id.clone(),
        run_id: run_id.clone(),
        report: agent_spec(AgentState::Blocked).report,
    });

    assert!(result.is_err());
    let restored = registry.agent(&agent.id).unwrap().snapshot().unwrap();
    assert_eq!(restored.observation.revision, 1);
    assert_eq!(restored.observation.state, AgentState::Working);
    assert!(restored.attention.is_none());
    shell.kill().unwrap();
    registry.close_workspace(&workspace.id).unwrap();
    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn agent_attention_is_raised_preserved_and_superseded_by_completion() {
    let registry = DaemonService::default();
    let (workspace, shell, _runtime) = running_shell(&registry);
    let run_id = shell.snapshot().unwrap().run.unwrap().id;
    let registered = registry
        .register_agent(&shell.id, &run_id, agent_spec(AgentState::Blocked))
        .unwrap();
    let registered_id = registered.id.clone();
    let blocked = registered.attention.clone().unwrap();
    assert_eq!(blocked.reason, AgentAttentionReason::Blocked);
    assert_eq!(blocked.observation, registered.observation);

    let (unchanged, changed, _) = registry
        .report_agent(
            &registered_id,
            &run_id,
            agent_report(
                AgentState::Working,
                AgentAuthority::TerminalHeuristic,
                "weak working",
            ),
        )
        .unwrap();
    assert!(!changed);
    assert_eq!(unchanged.attention.as_ref(), Some(&blocked));

    let (idle, changed, _) = registry
        .report_agent(&registered_id, &run_id, agent_spec(AgentState::Idle).report)
        .unwrap();
    assert!(changed);
    assert_eq!(idle.attention.as_ref(), Some(&blocked));

    let duplicate = agent_spec(AgentState::Idle).report;
    let (idle_again, changed, _) = registry.report_agent(&idle.id, &run_id, duplicate).unwrap();
    assert!(!changed);
    assert_eq!(idle_again.attention.as_ref(), Some(&blocked));

    let (done, changed, completed) = registry
        .report_agent(&idle.id, &run_id, agent_spec(AgentState::Done).report)
        .unwrap();
    assert!(changed && completed);
    let attention = done.attention.unwrap();
    assert_eq!(attention.reason, AgentAttentionReason::Completed);
    assert_eq!(attention.observation, done.observation);
    shell.kill().unwrap();
    registry.close_workspace(&workspace.id).unwrap();
}

#[test]
fn notifications_are_deduplicated_and_follow_current_attention() {
    let (registry, sink) = notification_registry(NotificationSettings {
        enabled: true,
        blocked: true,
        completed: true,
    });
    let (workspace, shell, _runtime) = running_shell(&registry);
    let run_id = shell.snapshot().unwrap().run.unwrap().id;

    let Response::Agent { agent: blocked } = registry
        .dispatch(Request::RegisterAgent {
            shell_id: shell.id.clone(),
            run_id: run_id.clone(),
            spec: agent_spec(AgentState::Blocked),
        })
        .unwrap()
    else {
        panic!("expected blocked agent");
    };
    assert_eq!(sink.requests.lock().unwrap().len(), 1);

    registry
        .dispatch(Request::ReportAgent {
            agent_id: blocked.id.clone(),
            run_id: run_id.clone(),
            report: agent_spec(AgentState::Blocked).report,
        })
        .unwrap();
    registry
        .dispatch(Request::ReportAgent {
            agent_id: blocked.id.clone(),
            run_id: run_id.clone(),
            report: agent_report(
                AgentState::Blocked,
                AgentAuthority::LifecycleIntegration,
                "same blocker with updated evidence",
            ),
        })
        .unwrap();
    registry
        .dispatch(Request::ReportAgent {
            agent_id: blocked.id.clone(),
            run_id: run_id.clone(),
            report: agent_report(
                AgentState::Working,
                AgentAuthority::TerminalHeuristic,
                "lower authority",
            ),
        })
        .unwrap();
    assert_eq!(sink.requests.lock().unwrap().len(), 1);

    let Response::Agent { agent: working } = registry
        .dispatch(Request::ReportAgent {
            agent_id: blocked.id.clone(),
            run_id: run_id.clone(),
            report: agent_spec(AgentState::Working).report,
        })
        .unwrap()
    else {
        panic!("expected working agent");
    };
    assert!(working.attention.is_some());
    assert_eq!(sink.requests.lock().unwrap().len(), 1);

    let Response::Agent { agent: reblocked } = registry
        .dispatch(Request::ReportAgent {
            agent_id: blocked.id.clone(),
            run_id: run_id.clone(),
            report: agent_spec(AgentState::Blocked).report,
        })
        .unwrap()
    else {
        panic!("expected reblocked agent");
    };
    assert_eq!(sink.requests.lock().unwrap().len(), 2);

    registry
        .dispatch(Request::AcknowledgeAgentAttention {
            agent_id: reblocked.id.clone(),
            observation_revision: reblocked.observation.revision,
        })
        .unwrap();
    assert_eq!(sink.requests.lock().unwrap().len(), 2);

    registry
        .dispatch(Request::ReportAgent {
            agent_id: reblocked.id,
            run_id: run_id.clone(),
            report: agent_spec(AgentState::Done).report,
        })
        .unwrap();
    assert_eq!(sink.requests.lock().unwrap().len(), 3);

    registry
        .dispatch(Request::RegisterAgent {
            shell_id: shell.id.clone(),
            run_id,
            spec: agent_spec(AgentState::Done),
        })
        .unwrap();
    let requests = sink.requests.lock().unwrap();
    assert_eq!(requests.len(), 4);
    assert_eq!(requests[0].reason, NotificationReason::Blocked);
    assert_eq!(requests[1].reason, NotificationReason::Blocked);
    assert_eq!(requests[2].reason, NotificationReason::Completed);
    assert_eq!(requests[3].reason, NotificationReason::Completed);
    assert_eq!(requests[0].workspace, "agents");
    assert_eq!(requests[0].shell, "agent-shell");
    drop(requests);

    shell.kill().unwrap();
    registry.close_workspace(&workspace.id).unwrap();
}

#[test]
fn working_to_idle_notifies_without_completed_attention() {
    let (registry, sink) = notification_registry(NotificationSettings {
        enabled: true,
        blocked: true,
        completed: true,
    });
    let (workspace, shell, _runtime) = running_shell(&registry);
    let run_id = shell.snapshot().unwrap().run.unwrap().id;
    let Response::Agent { agent } = registry
        .dispatch(Request::RegisterAgent {
            shell_id: shell.id.clone(),
            run_id: run_id.clone(),
            spec: agent_spec(AgentState::Working),
        })
        .unwrap()
    else {
        panic!("expected working agent");
    };

    let Response::Agent { agent } = registry
        .dispatch(Request::ReportAgent {
            agent_id: agent.id,
            run_id,
            report: agent_spec(AgentState::Idle).report,
        })
        .unwrap()
    else {
        panic!("expected idle agent");
    };

    assert!(agent.attention.is_none());
    let requests = sink.requests.lock().unwrap();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].reason, NotificationReason::Completed);
    drop(requests);
    shell.kill().unwrap();
    registry.close_workspace(&workspace.id).unwrap();
}

#[test]
fn disabled_notifications_do_not_reach_sink() {
    let (registry, sink) = notification_registry(NotificationSettings::default());
    let (workspace, shell, _runtime) = running_shell(&registry);
    let run_id = shell.snapshot().unwrap().run.unwrap().id;
    registry
        .dispatch(Request::RegisterAgent {
            shell_id: shell.id.clone(),
            run_id,
            spec: agent_spec(AgentState::Blocked),
        })
        .unwrap();
    assert!(sink.requests.lock().unwrap().is_empty());
    shell.kill().unwrap();
    registry.close_workspace(&workspace.id).unwrap();
}

#[test]
fn retained_agent_notifications_explain_a_removed_shell() {
    let (registry, sink) = notification_registry(NotificationSettings {
        enabled: true,
        blocked: true,
        completed: true,
    });
    let (workspace, shell, _runtime) = running_shell(&registry);
    let run_id = shell.snapshot().unwrap().run.unwrap().id;
    let Response::Agent { agent } = registry
        .dispatch(Request::RegisterAgent {
            shell_id: shell.id.clone(),
            run_id: run_id.clone(),
            spec: agent_spec(AgentState::Working),
        })
        .unwrap()
    else {
        panic!("expected working agent");
    };
    registry
        .dispatch(Request::CloseShell {
            shell_id: shell.id.clone(),
        })
        .unwrap();
    registry
        .dispatch(Request::ReportAgent {
            agent_id: agent.id,
            run_id,
            report: agent_spec(AgentState::Blocked).report,
        })
        .unwrap();

    let requests = sink.requests.lock().unwrap();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].workspace, "agents");
    assert_eq!(requests[0].shell, "removed");
    drop(requests);
    registry.close_workspace(&workspace.id).unwrap();
}

#[test]
fn failed_persistence_does_not_notify() {
    let directory = env::temp_dir().join(format!("boomux-notification-{}", Uuid::new_v4()));
    let state_directory = directory.join("state");
    let mut registry = DaemonService::restore(
        StateStore::at(state_directory.join("state.json")),
        false,
        None,
    )
    .unwrap();
    let sink = Arc::new(RecordingNotificationSink::default());
    registry.notification_settings = NotificationDeliverySettings {
        desktop: NotificationSettings {
            enabled: true,
            blocked: true,
            completed: true,
        },
        ..Default::default()
    };
    registry.notification_sink = sink.clone();
    let (workspace, shell, _runtime) = running_shell(&registry);
    let run_id = shell.snapshot().unwrap().run.unwrap().id;
    let Response::Agent { agent } = registry
        .dispatch(Request::RegisterAgent {
            shell_id: shell.id.clone(),
            run_id: run_id.clone(),
            spec: agent_spec(AgentState::Working),
        })
        .unwrap()
    else {
        panic!("expected working agent");
    };
    fs::remove_dir_all(&state_directory).unwrap();
    fs::write(&state_directory, b"not a directory").unwrap();

    assert!(
        registry
            .dispatch(Request::ReportAgent {
                agent_id: agent.id,
                run_id,
                report: agent_spec(AgentState::Blocked).report,
            })
            .is_err()
    );
    assert!(sink.requests.lock().unwrap().is_empty());
    shell.kill().unwrap();
    let _ = workspace;
    drop(registry);
    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn failed_attention_acknowledgment_persistence_restores_attention() {
    let directory = env::temp_dir().join(format!("boomux-attention-{}", Uuid::new_v4()));
    let state_directory = directory.join("state");
    let registry = DaemonService::restore(
        StateStore::at(state_directory.join("state.json")),
        false,
        None,
    )
    .unwrap();
    let (_workspace, shell, _runtime) = running_shell(&registry);
    let run_id = shell.snapshot().unwrap().run.unwrap().id;
    let Response::Agent { agent } = registry
        .dispatch(Request::RegisterAgent {
            shell_id: shell.id.clone(),
            run_id,
            spec: agent_spec(AgentState::Blocked),
        })
        .unwrap()
    else {
        panic!("expected registered agent");
    };
    let event_id = lock(&registry.events.state).unwrap().latest_id;
    fs::remove_dir_all(&state_directory).unwrap();
    fs::write(&state_directory, b"not a directory").unwrap();

    let error = registry
        .dispatch(Request::AcknowledgeAgentAttention {
            agent_id: agent.id.clone(),
            observation_revision: agent.observation.revision,
        })
        .unwrap_err();

    assert_eq!(error.wire_code(), ErrorCode::PersistenceFailed);
    assert!(
        registry
            .agent(&agent.id)
            .unwrap()
            .snapshot()
            .unwrap()
            .attention
            .is_some()
    );
    assert_eq!(lock(&registry.events.state).unwrap().latest_id, event_id);
    shell.kill().unwrap();
    drop(registry);
    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn agent_attention_acknowledgment_is_conditional_idempotent_and_rollback_safe() {
    let registry = DaemonService::default();
    let (workspace, shell, _runtime) = running_shell(&registry);
    let run_id = shell.snapshot().unwrap().run.unwrap().id;
    let agent = registry
        .register_agent(&shell.id, &run_id, agent_spec(AgentState::Blocked))
        .unwrap();
    let revision = agent.observation.revision;

    let mismatch = registry.acknowledge_agent_attention(&agent.id, revision + 1);
    assert_eq!(mismatch.unwrap_err().wire_code(), ErrorCode::RevisionAhead);
    assert!(
        registry
            .agent(&agent.id)
            .unwrap()
            .snapshot()
            .unwrap()
            .attention
            .is_some()
    );

    registry.fail_after_next_mutation();
    let result = registry.dispatch(Request::AcknowledgeAgentAttention {
        agent_id: agent.id.clone(),
        observation_revision: revision,
    });
    assert!(result.is_err());
    assert!(
        registry
            .agent(&agent.id)
            .unwrap()
            .snapshot()
            .unwrap()
            .attention
            .is_some()
    );

    let event_id = lock(&registry.events.state).unwrap().latest_id;
    let Response::AgentAttentionAcknowledged { agent, changed } = registry
        .dispatch(Request::AcknowledgeAgentAttention {
            agent_id: agent.id.clone(),
            observation_revision: revision,
        })
        .unwrap()
    else {
        panic!("expected attention acknowledgment");
    };
    assert!(changed);
    assert!(agent.attention.is_none());
    assert_eq!(agent.observation.revision, revision);
    assert_eq!(
        lock(&registry.events.state).unwrap().latest_id,
        event_id + 1
    );

    let Response::AgentAttentionAcknowledged { agent, changed } = registry
        .dispatch(Request::AcknowledgeAgentAttention {
            agent_id: agent.id,
            observation_revision: revision + 100,
        })
        .unwrap()
    else {
        panic!("expected idempotent acknowledgment");
    };
    assert!(!changed);
    assert_eq!(agent.observation.revision, revision);
    assert_eq!(
        lock(&registry.events.state).unwrap().latest_id,
        event_id + 1
    );
    shell.kill().unwrap();
    registry.close_workspace(&workspace.id).unwrap();
}

#[test]
fn agent_wait_is_revision_conditional_and_wakes_after_durable_change() {
    let registry = Arc::new(DaemonService::default());
    let (workspace, shell, _runtime) = running_shell(&registry);
    let run_id = shell.snapshot().unwrap().run.unwrap().id;
    let Response::Agent { agent } = registry
        .dispatch(Request::RegisterAgent {
            shell_id: shell.id.clone(),
            run_id: run_id.clone(),
            spec: agent_spec(AgentState::Working),
        })
        .unwrap()
    else {
        panic!("expected registered agent");
    };

    let Response::AgentWait { changed, .. } = registry.wait_agent(&agent.id, 0, 0).unwrap() else {
        panic!("expected immediate Agent wait");
    };
    assert!(changed);
    let Response::AgentWait { changed, .. } = registry.wait_agent(&agent.id, 1, 0).unwrap() else {
        panic!("expected unchanged Agent wait");
    };
    assert!(!changed);
    assert_eq!(
        registry
            .wait_agent(&agent.id, 2, 0)
            .unwrap_err()
            .wire_code(),
        ErrorCode::RevisionAhead
    );

    let waiting_registry = Arc::clone(&registry);
    let waiting_agent_id = agent.id.clone();
    let waiter = thread::spawn(move || waiting_registry.wait_agent(&waiting_agent_id, 1, 2_000));
    thread::sleep(Duration::from_millis(20));
    let Response::Agent { agent: blocked } = registry
        .dispatch(Request::ReportAgent {
            agent_id: agent.id.clone(),
            run_id: run_id.clone(),
            report: agent_spec(AgentState::Blocked).report,
        })
        .unwrap()
    else {
        panic!("expected changed Agent");
    };
    let Response::AgentWait {
        agent: waited,
        changed,
    } = waiter.join().unwrap().unwrap()
    else {
        panic!("expected changed Agent wait");
    };
    assert!(changed);
    assert_eq!(waited, blocked);
    assert_eq!(waited.observation.revision, 2);

    let Response::Agent { agent: done } = registry
        .dispatch(Request::ReportAgent {
            agent_id: agent.id.clone(),
            run_id,
            report: agent_spec(AgentState::Done).report,
        })
        .unwrap()
    else {
        panic!("expected completed Agent");
    };
    let start = Instant::now();
    let Response::AgentWait { changed, .. } = registry
        .wait_agent(&done.id, done.observation.revision, 2_000)
        .unwrap()
    else {
        panic!("expected terminal Agent wait");
    };
    assert!(!changed);
    assert!(start.elapsed() < Duration::from_secs(1));

    shell.kill().unwrap();
    registry.close_workspace(&workspace.id).unwrap();
}

#[test]
fn agent_wait_wakes_with_daemon_stopping_before_registry_cleanup() {
    let registry = Arc::new(DaemonService::default());
    let (workspace, shell, _runtime) = running_shell(&registry);
    let run_id = shell.snapshot().unwrap().run.unwrap().id;
    let Response::Agent { agent } = registry
        .dispatch(Request::RegisterAgent {
            shell_id: shell.id.clone(),
            run_id,
            spec: agent_spec(AgentState::Working),
        })
        .unwrap()
    else {
        panic!("expected registered agent");
    };
    let waiting_registry = Arc::clone(&registry);
    let waiter = thread::spawn(move || waiting_registry.wait_agent(&agent.id, 1, 2_000));
    thread::sleep(Duration::from_millis(20));

    registry.runtimes.begin_stopping();
    registry.events.notify();

    assert_eq!(
        waiter.join().unwrap().unwrap_err().wire_code(),
        ErrorCode::DaemonStopping
    );
    registry.runtimes.cancel_stopping();
    shell.kill().unwrap();
    registry.close_workspace(&workspace.id).unwrap();
}

#[test]
fn agent_instances_restore_from_daemon_persistence() {
    let directory = env::temp_dir().join(format!("boomux-agent-restore-{}", Uuid::new_v4()));
    let path = directory.join("state/state.json");
    let registry = DaemonService::restore(StateStore::at(path.clone()), false, None).unwrap();
    let (_workspace, shell, runtime) = running_shell(&registry);
    let shell_id = shell.id.clone();
    let run_id = shell.snapshot().unwrap().run.unwrap().id;
    let spec = agent_spec(AgentState::Working);
    let Response::Agent { agent: registered } = registry
        .dispatch(Request::EnsureAgent {
            shell_id: shell_id.clone(),
            run_id: run_id.clone(),
            spec: spec.clone(),
        })
        .unwrap()
    else {
        panic!("expected registered agent");
    };
    let Response::Agent { agent } = registry
        .dispatch(Request::ReportAgent {
            agent_id: registered.id,
            run_id: run_id.clone(),
            report: agent_spec(AgentState::Inactive).report,
        })
        .unwrap()
    else {
        panic!("expected inactive agent");
    };
    assert_eq!(agent.observation.state, AgentState::Inactive);
    assert_eq!(agent.ended_at_ms, None);

    shell.kill().unwrap();
    drop(runtime);
    drop(shell);
    drop(registry);

    let restored = DaemonService::restore(StateStore::at(path), false, None).unwrap();
    assert_eq!(
        restored.agent(&agent.id).unwrap().snapshot().unwrap(),
        agent
    );
    assert_eq!(
        restored.snapshot().unwrap().workspaces[0].agents,
        vec![agent.clone()]
    );
    let Response::Agent { agent: ensured } = restored
        .dispatch(Request::EnsureAgent {
            shell_id,
            run_id,
            spec,
        })
        .unwrap()
    else {
        panic!("expected restored ensured agent");
    };
    assert_eq!(ensured, agent);
    let Response::Agent { agent: reactivated } = restored
        .dispatch(Request::ReportAgent {
            agent_id: ensured.id,
            run_id: ensured.run_id,
            report: agent_spec(AgentState::Idle).report,
        })
        .unwrap()
    else {
        panic!("expected reactivated agent");
    };
    assert_eq!(reactivated.id, agent.id);
    assert_eq!(reactivated.observation.state, AgentState::Idle);
    assert_eq!(reactivated.ended_at_ms, None);
    assert_eq!(restored.snapshot().unwrap().workspaces[0].agents.len(), 1);
    drop(restored);
    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn protocol_eight_responses_hide_agent_snapshots_and_events() {
    let agent = AgentInstanceSnapshot {
        id: "a1".into(),
        workspace_id: "w1".into(),
        shell_id: "s1".into(),
        run_id: "r1".into(),
        name: "agent".into(),
        integration: "test".into(),
        external_session_id: None,
        cwd: Some("/tmp/project".into()),
        started_at_ms: 1,
        ended_at_ms: None,
        observation: AgentObservationSnapshot {
            revision: 1,
            state: AgentState::Working,
            authority: AgentAuthority::LifecycleIntegration,
            evidence: "working".into(),
            confidence: 90,
            observed_at_ms: 1,
        },
        attention: None,
        working_contexts: Vec::new(),
    };
    let workspace = WorkspaceSnapshot {
        id: "w1".into(),
        revision: 1,
        name: "workspace".into(),
        default_cwd: None,
        shells: Vec::new(),
        launchers: Vec::new(),
        agents: vec![agent.clone()],
    };
    let response = Response::Events {
        stream_id: "stream".into(),
        cursor: EventCursor {
            stream_id: "stream".into(),
            event_id: 2,
        },
        snapshot: Some(Snapshot {
            workspaces: vec![workspace],
            focused_terminal: None,
        }),
        events: vec![
            DaemonEvent {
                id: 1,
                at_ms: 1,
                kind: DaemonEventKind::AgentRegistered {
                    workspace_id: "w1".into(),
                    shell_id: "s1".into(),
                    agent,
                },
            },
            DaemonEvent {
                id: 2,
                at_ms: 2,
                kind: DaemonEventKind::WorkspaceRenamed {
                    workspace_id: "w1".into(),
                    name: "renamed".into(),
                },
            },
        ],
    };

    let Response::Events {
        snapshot: Some(snapshot),
        events,
        cursor,
        ..
    } = response_for_version(response, 8)
    else {
        panic!("expected filtered events");
    };
    assert!(snapshot.workspaces[0].agents.is_empty());
    assert_eq!(events.len(), 1);
    assert_eq!(cursor.event_id, 2);
    assert!(matches!(
        events[0].kind,
        DaemonEventKind::WorkspaceRenamed { .. }
    ));
    let encoded =
        serde_json::to_value(response_for_version(Response::Snapshot { snapshot }, 8)).unwrap();
    assert!(encoded["snapshot"]["workspaces"][0].get("agents").is_none());
}

#[test]
fn protocol_thirty_seven_combined_snapshots_hide_coordinator_workspaces() {
    let workspace_id = Uuid::from_u128(1).to_string();
    let node_id = Uuid::from_u128(2).to_string();
    let current_node = protocol::CombinedNode {
        node_id: node_id.clone(),
        alias: "remote".into(),
        local: false,
        route: Some("remote.example".into()),
        registration_revision: Some(7),
        health: protocol::NodeProjectionHealthCode::Online,
        current: true,
        stale: false,
        observed_at_ms: 10,
        observed_protocol_version: Some(37),
        observed_capabilities: vec!["protocol_37".into()],
        observed_helper_version: Some("0.41.0".into()),
        workspace_owner_eligible: true,
        workspace_owner_unavailable_reason: Some("new field".into()),
        local_snapshot: None,
        remote_projection: None,
    };
    let response = Response::CombinedNodeSnapshot {
        snapshot: protocol::CombinedNodeSnapshot {
            nodes: vec![current_node.clone()],
            workspaces: vec![protocol::GlobalWorkspaceSnapshot {
                id: workspace_id.clone(),
                revision: 1,
                name: "work".into(),
                closing: false,
                placements: Vec::new(),
            }],
            external_workspaces: vec![protocol::ExternalWorkspaceSnapshot {
                identity: protocol::QualifiedIdentity::new(node_id.clone(), workspace_id),
                revision: 1,
                name: "external".into(),
                default_cwd: Some("/owner/work".into()),
                available: true,
            }],
            focused_terminal: Some(protocol::QualifiedFocusedTerminalSnapshot {
                revision: 9,
                shell: protocol::QualifiedIdentity::new(node_id.clone(), "shell"),
            }),
        },
    };
    let Response::CombinedNodeSnapshot {
        snapshot: protocol_thirty_eight,
    } = response_for_version(response.clone(), 38)
    else {
        panic!("expected combined Node snapshot");
    };
    assert_eq!(protocol_thirty_eight.workspaces.len(), 1);
    assert_eq!(protocol_thirty_eight.external_workspaces.len(), 1);
    assert!(protocol_thirty_eight.focused_terminal.is_none());
    let Response::CombinedNodeSnapshot { snapshot } = response_for_version(response, 37) else {
        panic!("expected combined Node snapshot");
    };
    assert!(snapshot.workspaces.is_empty());
    assert!(snapshot.external_workspaces.is_empty());
    assert!(snapshot.focused_terminal.is_none());
    let encoded_node = serde_json::to_value(&snapshot.nodes[0]).unwrap();
    assert!(encoded_node.get("route").is_none());
    assert!(encoded_node.get("registration_revision").is_none());
    assert!(encoded_node.get("workspace_owner_eligible").is_none());
    assert!(
        encoded_node
            .get("workspace_owner_unavailable_reason")
            .is_none()
    );
    #[derive(serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    struct ProtocolThirtySevenCombinedNode {
        node_id: String,
        alias: String,
        local: bool,
        health: protocol::NodeProjectionHealthCode,
        current: bool,
        stale: bool,
        observed_at_ms: u64,
        observed_protocol_version: Option<u32>,
        observed_capabilities: Vec<String>,
        local_snapshot: Option<Snapshot>,
        remote_projection: Option<NodeProjectionSnapshot>,
    }
    for version in 33..=37 {
        let Response::CombinedNodeSnapshot { snapshot } = response_for_version(
            Response::CombinedNodeSnapshot {
                snapshot: protocol::CombinedNodeSnapshot {
                    nodes: vec![current_node.clone()],
                    workspaces: Vec::new(),
                    external_workspaces: Vec::new(),
                    focused_terminal: None,
                },
            },
            version,
        ) else {
            unreachable!();
        };
        let old: ProtocolThirtySevenCombinedNode =
            serde_json::from_value(serde_json::to_value(&snapshot.nodes[0]).unwrap()).unwrap();
        assert_eq!(old.node_id, node_id);
        assert_eq!(old.alias, "remote");
        assert!(!old.local);
        assert_eq!(old.health, protocol::NodeProjectionHealthCode::Online);
        assert!(old.current);
        assert!(!old.stale);
        assert_eq!(old.observed_at_ms, 10);
        assert_eq!(old.observed_protocol_version, Some(37));
        assert_eq!(old.observed_capabilities, ["protocol_37"]);
        assert!(old.local_snapshot.is_none());
        assert!(old.remote_projection.is_none());
    }
}

#[test]
fn protocol_forty_combined_snapshots_omit_observed_helper_version() {
    let node = protocol::CombinedNode {
        node_id: Uuid::from_u128(2).to_string(),
        alias: "remote".into(),
        local: false,
        route: None,
        registration_revision: None,
        health: protocol::NodeProjectionHealthCode::Online,
        current: true,
        stale: false,
        observed_at_ms: 10,
        observed_protocol_version: Some(41),
        observed_capabilities: vec!["protocol_41".into()],
        observed_helper_version: Some("0.41.0".into()),
        workspace_owner_eligible: false,
        workspace_owner_unavailable_reason: None,
        local_snapshot: None,
        remote_projection: None,
    };
    let response = |version| {
        response_for_version(
            Response::CombinedNodeSnapshot {
                snapshot: protocol::CombinedNodeSnapshot {
                    nodes: vec![node.clone()],
                    workspaces: Vec::new(),
                    external_workspaces: Vec::new(),
                    focused_terminal: None,
                },
            },
            version,
        )
    };

    let Response::CombinedNodeSnapshot { snapshot } = response(40) else {
        unreachable!();
    };
    let old = serde_json::to_value(&snapshot.nodes[0]).unwrap();
    assert!(old.get("observed_helper_version").is_none());

    let Response::CombinedNodeSnapshot { snapshot } = response(41) else {
        unreachable!();
    };
    assert_eq!(
        serde_json::to_value(&snapshot.nodes[0]).unwrap()["observed_helper_version"],
        "0.41.0"
    );

    let health = protocol::NodeProjectionHealth {
        code: protocol::NodeProjectionHealthCode::Online,
        stale: false,
        cache_generation: 1,
        stream_id: None,
        cursor: None,
        last_attempt_at_ms: None,
        last_success_at_ms: None,
        retry_at_ms: None,
        capabilities: vec!["protocol_41".into()],
        observed_helper_version: Some("0.41.0".into()),
    };
    let Response::NodeProjectionHealth { health } =
        response_for_version(Response::NodeProjectionHealth { health }, 40)
    else {
        unreachable!();
    };
    assert!(serde_json::to_value(health).unwrap()["observed_helper_version"].is_null());
}

#[test]
fn protocol_forty_preserves_only_negotiated_recovery_presentations() {
    let shell = ShellSnapshot {
        id: "shell-1".into(),
        revision: 1,
        workspace_id: "workspace-1".into(),
        name: "agent".into(),
        cwd: "/tmp/project".into(),
        command: Vec::new(),
        status: ShellStatus::Pending,
        run: Some(ShellRunSnapshot {
            id: "run-1".into(),
            generation: 1,
            started_at_ms: 1,
            ended_at_ms: Some(2),
            exit_reason: Some(ShellRunExitReason::Interrupted),
            output_revision: 3,
            environment_has_run_id: true,
        }),
        recovered_agent_id: Some("agent-1".into()),
        foreground_process: None,
    };
    let workspace = WorkspaceSnapshot {
        id: "workspace-1".into(),
        revision: 1,
        name: "project".into(),
        default_cwd: None,
        shells: vec![shell.clone()],
        launchers: Vec::new(),
        agents: Vec::new(),
    };
    let response = Response::Snapshot {
        snapshot: Snapshot {
            workspaces: vec![workspace.clone()],
            focused_terminal: None,
        },
    };
    let Response::Snapshot { snapshot } = response_for_version(response.clone(), 39) else {
        panic!("expected snapshot");
    };
    assert!(snapshot.workspaces[0].shells[0].run.is_none());
    assert!(
        snapshot.workspaces[0].shells[0]
            .recovered_agent_id
            .is_none()
    );
    let Response::Snapshot { snapshot } = response_for_version(response, 40) else {
        panic!("expected snapshot");
    };
    assert_eq!(
        snapshot.workspaces[0].shells[0].run.as_ref().unwrap().id,
        "run-1"
    );
    assert_eq!(
        snapshot.workspaces[0].shells[0]
            .recovered_agent_id
            .as_deref(),
        Some("agent-1")
    );

    let response = Response::RoutedNodeOperation {
        result: RoutedOperationResult::Workspace {
            workspace: workspace.clone(),
        },
    };
    let Response::RoutedNodeOperation {
        result: RoutedOperationResult::Workspace { workspace },
    } = response_for_version(response, 39)
    else {
        panic!("expected routed workspace");
    };
    assert!(workspace.shells[0].run.is_none());
    assert!(workspace.shells[0].recovered_agent_id.is_none());

    let response = Response::GlobalWorkspaceResource {
        workspace: protocol::GlobalWorkspaceSnapshot {
            id: "global-1".into(),
            revision: 1,
            name: "project".into(),
            closing: false,
            placements: Vec::new(),
        },
        resource: RoutedOperationResult::Shell {
            shell: shell.clone(),
        },
    };
    let Response::GlobalWorkspaceResource {
        resource: RoutedOperationResult::Shell { shell },
        ..
    } = response_for_version(response, 39)
    else {
        panic!("expected global workspace shell");
    };
    assert!(shell.run.is_none());
    assert!(shell.recovered_agent_id.is_none());

    let mut projection = remote_notification_projection("node-1");
    projection.shells[0].status = ShellStatus::Pending;
    projection.shells[0].recovered_agent_id = Some("agent-blocked".into());
    let response = Response::NodeProjectionSync {
        sync: NodeProjectionSync {
            mode: NodeProjectionSyncMode::Baseline,
            cursor: EventCursor {
                stream_id: "stream".into(),
                event_id: 1,
            },
            projection,
            transitions: Vec::new(),
            capabilities: vec!["recovered_agent_presentation".into()],
        },
    };
    let Response::NodeProjectionSync { sync } = response_for_version(response.clone(), 39) else {
        panic!("expected projection sync");
    };
    assert!(sync.projection.shells[0].run_id.is_none());
    assert!(sync.projection.shells[0].recovered_agent_id.is_none());
    let Response::NodeProjectionSync { sync } = response_for_version(response, 40) else {
        panic!("expected projection sync");
    };
    assert_eq!(sync.projection.shells[0].run_id.as_deref(), Some("run-1"));
    assert_eq!(
        sync.projection.shells[0].recovered_agent_id.as_deref(),
        Some("agent-blocked")
    );
}

#[test]
fn protocol_seventeen_responses_hide_focused_terminal() {
    let response = Response::Snapshot {
        snapshot: Snapshot {
            workspaces: Vec::new(),
            focused_terminal: Some(FocusedTerminalSnapshot {
                revision: 1,
                workspace_id: "w1".into(),
                shell_id: "s1".into(),
                run_id: "r1".into(),
            }),
        },
    };

    let Response::Snapshot { snapshot } = response_for_version(response, 17) else {
        panic!("expected snapshot response");
    };
    assert!(snapshot.focused_terminal.is_none());
}

#[test]
fn older_protocol_responses_downgrade_agent_fields() {
    let agent = AgentInstanceSnapshot {
        id: "a1".into(),
        workspace_id: "w1".into(),
        shell_id: "s1".into(),
        run_id: "r1".into(),
        name: "pi".into(),
        integration: "pi".into(),
        external_session_id: Some("session-1".into()),
        cwd: Some("/tmp/project".into()),
        started_at_ms: 1,
        ended_at_ms: None,
        observation: AgentObservationSnapshot {
            revision: 2,
            state: AgentState::Inactive,
            authority: AgentAuthority::LifecycleIntegration,
            evidence: "Pi session inactive".into(),
            confidence: 100,
            observed_at_ms: 2,
        },
        attention: None,
        working_contexts: Vec::new(),
    };

    let Response::Agent { agent: downgraded } = response_for_version(
        Response::Agent {
            agent: agent.clone(),
        },
        11,
    ) else {
        panic!("expected agent response");
    };
    assert_eq!(downgraded.observation.state, AgentState::Unknown);
    assert!(downgraded.cwd.is_none());

    let Response::Agent { agent: current } = response_for_version(
        Response::Agent {
            agent: agent.clone(),
        },
        12,
    ) else {
        panic!("expected agent response");
    };
    assert_eq!(current.observation.state, AgentState::Inactive);
    assert!(current.cwd.is_none());

    let Response::Agent { agent: current } = response_for_version(
        Response::Agent {
            agent: agent.clone(),
        },
        13,
    ) else {
        panic!("expected agent response");
    };
    assert_eq!(current.cwd.as_deref(), Some(Path::new("/tmp/project")));

    let workspace = WorkspaceSnapshot {
        id: "w1".into(),
        revision: 1,
        name: "workspace".into(),
        default_cwd: None,
        shells: Vec::new(),
        launchers: Vec::new(),
        agents: vec![agent.clone()],
    };
    let Response::Workspace {
        workspace: downgraded_workspace,
    } = response_for_version(
        Response::Workspace {
            workspace: workspace.clone(),
        },
        11,
    )
    else {
        panic!("expected workspace response");
    };
    assert_eq!(
        downgraded_workspace.agents[0].observation.state,
        AgentState::Unknown
    );

    let Response::Events {
        snapshot: Some(snapshot),
        events,
        ..
    } = response_for_version(
        Response::Events {
            stream_id: "stream".into(),
            cursor: EventCursor {
                stream_id: "stream".into(),
                event_id: 1,
            },
            snapshot: Some(Snapshot {
                workspaces: vec![workspace],
                focused_terminal: None,
            }),
            events: vec![DaemonEvent {
                id: 1,
                at_ms: 1,
                kind: DaemonEventKind::AgentStateChanged {
                    workspace_id: "w1".into(),
                    shell_id: "s1".into(),
                    agent,
                },
            }],
        },
        11,
    )
    else {
        panic!("expected events response");
    };
    assert_eq!(
        snapshot.workspaces[0].agents[0].observation.state,
        AgentState::Unknown
    );
    let DaemonEventKind::AgentStateChanged { agent, .. } = &events[0].kind else {
        panic!("expected agent state event");
    };
    assert_eq!(agent.observation.state, AgentState::Unknown);
}

#[test]
fn protocol_fourteen_omits_attention_and_filters_acknowledgment_events() {
    let observation = AgentObservationSnapshot {
        revision: 2,
        state: AgentState::Blocked,
        authority: AgentAuthority::LifecycleIntegration,
        evidence: "blocked".into(),
        confidence: 100,
        observed_at_ms: 2,
    };
    let agent = AgentInstanceSnapshot {
        id: "a1".into(),
        workspace_id: "w1".into(),
        shell_id: "s1".into(),
        run_id: "r1".into(),
        name: "agent".into(),
        integration: "test".into(),
        external_session_id: None,
        cwd: None,
        started_at_ms: 1,
        ended_at_ms: None,
        observation: observation.clone(),
        attention: Some(AgentAttentionSnapshot {
            reason: AgentAttentionReason::Blocked,
            observation,
        }),
        working_contexts: Vec::new(),
    };
    let cursor = EventCursor {
        stream_id: "stream".into(),
        event_id: 2,
    };
    let response = Response::Events {
        stream_id: "stream".into(),
        cursor: cursor.clone(),
        snapshot: Some(Snapshot {
            workspaces: vec![WorkspaceSnapshot {
                id: "w1".into(),
                revision: 1,
                name: "workspace".into(),
                default_cwd: None,
                shells: Vec::new(),
                launchers: Vec::new(),
                agents: vec![agent.clone()],
            }],
            focused_terminal: None,
        }),
        events: vec![DaemonEvent {
            id: 2,
            at_ms: 3,
            kind: DaemonEventKind::AgentAttentionAcknowledged {
                workspace_id: "w1".into(),
                shell_id: "s1".into(),
                agent,
            },
        }],
    };

    let Response::Events {
        cursor: filtered_cursor,
        snapshot: Some(snapshot),
        events,
        ..
    } = response_for_version(response, 14)
    else {
        panic!("expected events response");
    };
    assert_eq!(filtered_cursor, cursor);
    assert!(events.is_empty());
    assert!(snapshot.workspaces[0].agents[0].attention.is_none());
}

#[test]
fn protocol_forty_eight_filters_default_cwd_events_but_advances_cursor() {
    let cursor = EventCursor {
        stream_id: "stream".into(),
        event_id: 4,
    };
    let response = Response::Events {
        stream_id: "stream".into(),
        cursor: cursor.clone(),
        snapshot: None,
        events: vec![DaemonEvent {
            id: 4,
            at_ms: 1,
            kind: DaemonEventKind::WorkspaceDefaultCwdChanged {
                workspace_id: "workspace".into(),
                default_cwd: "/work".into(),
            },
        }],
    };
    let Response::Events {
        cursor: filtered_cursor,
        events,
        ..
    } = response_for_version(response, 48)
    else {
        panic!("expected events response");
    };
    assert_eq!(filtered_cursor, cursor);
    assert!(events.is_empty());
}

#[test]
fn protocol_forty_nine_filters_working_contexts_without_rewinding_cursors() {
    let context = AgentWorkingContextSnapshot {
        worktree_root: "/worktrees/boomux".into(),
        repository: "boomux".into(),
        branch: "feat/working-contexts".into(),
        observed_at_ms: 5,
    };
    let agent = AgentInstanceSnapshot {
        id: "a1".into(),
        workspace_id: "w1".into(),
        shell_id: "s1".into(),
        run_id: "r1".into(),
        name: "agent".into(),
        integration: "test".into(),
        external_session_id: Some("external".into()),
        cwd: Some("/worktrees/boomux".into()),
        started_at_ms: 1,
        ended_at_ms: None,
        observation: AgentObservationSnapshot {
            revision: 1,
            state: AgentState::Working,
            authority: AgentAuthority::LifecycleIntegration,
            evidence: "working".into(),
            confidence: 100,
            observed_at_ms: 1,
        },
        attention: None,
        working_contexts: vec![context],
    };
    let cursor = EventCursor {
        stream_id: "stream".into(),
        event_id: 5,
    };
    let response = Response::Events {
        stream_id: "stream".into(),
        cursor: cursor.clone(),
        snapshot: Some(Snapshot {
            workspaces: vec![WorkspaceSnapshot {
                id: "w1".into(),
                revision: 1,
                name: "workspace".into(),
                default_cwd: None,
                shells: Vec::new(),
                launchers: Vec::new(),
                agents: vec![agent.clone()],
            }],
            focused_terminal: None,
        }),
        events: vec![DaemonEvent {
            id: 5,
            at_ms: 5,
            kind: DaemonEventKind::AgentWorkingContextObserved {
                workspace_id: "w1".into(),
                shell_id: "s1".into(),
                agent,
            },
        }],
    };

    let Response::Events {
        cursor: filtered_cursor,
        snapshot: Some(snapshot),
        events,
        ..
    } = response_for_version(response, 49)
    else {
        panic!("expected events response");
    };
    assert_eq!(filtered_cursor, cursor);
    assert!(events.is_empty());
    assert!(snapshot.workspaces[0].agents[0].working_contexts.is_empty());

    let sync = Response::NodeProjectionSync {
        sync: NodeProjectionSync {
            mode: NodeProjectionSyncMode::Resumed,
            cursor: cursor.clone(),
            projection: remote_notification_projection("node-1"),
            transitions: vec![
                NodeProjectionTransition {
                    event_id: 4,
                    at_ms: 4,
                    kind: NodeProjectionTransitionKind::HandoffCompleted,
                },
                NodeProjectionTransition {
                    event_id: 5,
                    at_ms: 5,
                    kind: NodeProjectionTransitionKind::SessionContext {
                        workspace_id: "w1".into(),
                        agent_id: "a1".into(),
                    },
                },
            ],
            capabilities: vec!["session_presentation_context".into()],
        },
    };
    let Response::NodeProjectionSync { sync: filtered } = response_for_version(sync.clone(), 49)
    else {
        panic!("expected projection sync");
    };
    assert_eq!(filtered.cursor, cursor);
    assert_eq!(filtered.transitions.len(), 1);
    assert!(matches!(
        filtered.transitions[0].kind,
        NodeProjectionTransitionKind::HandoffCompleted
    ));
    let Response::NodeProjectionSync { sync } = response_for_version(sync, 50) else {
        panic!("expected projection sync");
    };
    assert_eq!(sync.transitions.len(), 2);
}

#[test]
fn protocol_forty_eight_owner_fails_default_cwd_feature_preflight() {
    let protocol_forty_eight = protocol::ProtocolFeature::ALL
        .iter()
        .copied()
        .filter(|feature| feature.minimum_version() <= 48)
        .flat_map(protocol::ProtocolFeature::capability_names)
        .map(|capability| (*capability).to_owned())
        .collect::<Vec<_>>();
    assert!(matches!(
        require_capabilities_support_feature(
            &protocol_forty_eight,
            protocol::ProtocolFeature::WorkspacePlacementDefaultCwd,
        ),
        Err(DaemonError::Lifecycle {
            code: ErrorCode::UnsupportedVersion,
            ..
        })
    ));
    let mut protocol_forty_nine = protocol_forty_eight;
    protocol_forty_nine.extend(
        protocol::ProtocolFeature::WorkspacePlacementDefaultCwd
            .capability_names()
            .iter()
            .map(|capability| (*capability).to_owned()),
    );
    assert!(capabilities_support_feature(
        &protocol_forty_nine,
        protocol::ProtocolFeature::WorkspacePlacementDefaultCwd,
    ));
}

#[test]
fn remote_default_cwd_attempt_is_marked_only_after_supported_handshake() {
    let mut attempted = false;
    let unsupported = prepare_supported_owner_request(
        48,
        Some(protocol::ProtocolFeature::WorkspacePlacementDefaultCwd),
        &mut || {
            attempted = true;
            Ok(())
        },
    );
    assert_eq!(unsupported.unwrap_err().kind(), io::ErrorKind::Unsupported);
    assert!(!attempted);

    prepare_supported_owner_request(
        49,
        Some(protocol::ProtocolFeature::WorkspacePlacementDefaultCwd),
        &mut || {
            attempted = true;
            Ok(())
        },
    )
    .unwrap();
    assert!(attempted);
}

#[test]
fn session_display_name_remote_request_requires_protocol_fifty_before_dispatch() {
    let mut dispatched = false;
    let unsupported = prepare_supported_owner_request(
        49,
        Some(protocol::ProtocolFeature::SessionDisplayNames),
        &mut || {
            dispatched = true;
            Ok(())
        },
    );
    assert_eq!(unsupported.unwrap_err().kind(), io::ErrorKind::Unsupported);
    assert!(!dispatched);

    prepare_supported_owner_request(
        50,
        Some(protocol::ProtocolFeature::SessionDisplayNames),
        &mut || {
            dispatched = true;
            Ok(())
        },
    )
    .unwrap();
    assert!(dispatched);
}

#[test]
fn default_cwd_retains_only_ambiguous_owner_errors() {
    for code in [
        ErrorCode::OutcomeUnknown,
        ErrorCode::PersistenceFailed,
        ErrorCode::Timeout,
    ] {
        assert!(default_cwd_owner_error_is_ambiguous(Some(code)));
    }
    for code in [
        ErrorCode::RevisionAhead,
        ErrorCode::UnsupportedVersion,
        ErrorCode::NotFound,
        ErrorCode::Internal,
    ] {
        assert!(!default_cwd_owner_error_is_ambiguous(Some(code)));
    }
    assert!(!default_cwd_owner_error_is_ambiguous(None));
}

#[test]
fn guarded_workspace_default_cwd_changes_only_future_defaults() {
    let root = env::temp_dir().join(format!("boomux-default-cwd-{}", Uuid::new_v4()));
    let old = root.join("old");
    let new = root.join("new");
    fs::create_dir_all(&old).unwrap();
    fs::create_dir_all(&new).unwrap();
    let registry = DaemonService::default();
    let workspace = registry
        .create_workspace_with_default_cwd(
            "work".into(),
            Some(old.clone()),
            vec![ShellSpec::login("existing", old.clone())],
        )
        .unwrap();
    let response = registry
        .dispatch(Request::GuardedSetWorkspaceDefaultCwd {
            workspace_id: workspace.id.clone(),
            expected_revision: workspace.revision,
            default_cwd: new.clone(),
        })
        .unwrap();
    let Response::Workspace { workspace: updated } = response else {
        panic!("expected Workspace response");
    };
    assert_eq!(updated.revision, workspace.revision + 1);
    assert_eq!(updated.default_cwd.as_deref(), Some(new.as_path()));
    assert_eq!(updated.shells[0].cwd, old);

    let Response::Events { cursor, .. } = registry
        .dispatch(Request::Events {
            after: None,
            limit: 256,
            wait_ms: 0,
        })
        .unwrap()
    else {
        panic!("expected event baseline");
    };
    let unchanged = registry
        .dispatch(Request::GuardedSetWorkspaceDefaultCwd {
            workspace_id: workspace.id.clone(),
            expected_revision: updated.revision,
            default_cwd: new,
        })
        .unwrap();
    let Response::Workspace {
        workspace: unchanged,
    } = unchanged
    else {
        panic!("expected unchanged Workspace response");
    };
    assert_eq!(unchanged.revision, updated.revision);
    let Response::Events { events, .. } = registry
        .dispatch(Request::Events {
            after: Some(cursor),
            limit: 256,
            wait_ms: 0,
        })
        .unwrap()
    else {
        panic!("expected event page");
    };
    assert!(events.is_empty());
    let stale = registry.dispatch(Request::GuardedSetWorkspaceDefaultCwd {
        workspace_id: workspace.id,
        expected_revision: workspace.revision,
        default_cwd: root.clone(),
    });
    assert!(matches!(
        stale,
        Err(DaemonError::Lifecycle {
            code: ErrorCode::RevisionAhead,
            ..
        })
    ));
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn workspace_snapshot_never_tears_revision_from_default_cwd() {
    let root = env::temp_dir().join(format!("boomux-cwd-snapshot-{}", Uuid::new_v4()));
    let old = root.join("old");
    let new = root.join("new");
    fs::create_dir_all(&old).unwrap();
    fs::create_dir_all(&new).unwrap();
    let registry = Arc::new(DaemonService::default());
    let workspace = registry
        .create_workspace_with_default_cwd("work".into(), Some(old.clone()), Vec::new())
        .unwrap();
    let finished = Arc::new(AtomicBool::new(false));
    let writer_registry = Arc::clone(&registry);
    let writer_finished = Arc::clone(&finished);
    let workspace_id = workspace.id.clone();
    let writer_old = old.clone();
    let writer_new = new.clone();
    let writer = thread::spawn(move || {
        for index in 0..2_000 {
            let cwd = if index % 2 == 0 {
                writer_new.clone()
            } else {
                writer_old.clone()
            };
            assert!(
                writer_registry
                    .durable
                    .set_workspace_default_cwd(&workspace_id, cwd)
                    .unwrap()
                    .is_some()
            );
        }
        writer_finished.store(true, Ordering::Release);
    });
    while !finished.load(Ordering::Acquire) {
        let snapshot = registry
            .workspace(&workspace.id)
            .unwrap()
            .snapshot(&registry.durable)
            .unwrap();
        let expected = if snapshot.revision % 2 == 0 {
            new.as_path()
        } else {
            old.as_path()
        };
        assert_eq!(snapshot.default_cwd.as_deref(), Some(expected));
    }
    writer.join().unwrap();
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn protocol_seven_event_pages_hide_launcher_events() {
    let cursor = EventCursor {
        stream_id: "stream".into(),
        event_id: 2,
    };
    let response = Response::Events {
        stream_id: "stream".into(),
        cursor: cursor.clone(),
        snapshot: None,
        events: vec![
            DaemonEvent {
                id: 1,
                at_ms: 1,
                kind: DaemonEventKind::LauncherCreated {
                    workspace_id: "workspace".into(),
                    launcher_id: "launcher".into(),
                    name: "editor".into(),
                },
            },
            DaemonEvent {
                id: 2,
                at_ms: 2,
                kind: DaemonEventKind::WorkspaceRenamed {
                    workspace_id: "workspace".into(),
                    name: "renamed".into(),
                },
            },
        ],
    };
    let Response::Events {
        cursor: filtered_cursor,
        events,
        ..
    } = response_for_version(response, 7)
    else {
        panic!("expected events");
    };
    assert_eq!(filtered_cursor, cursor);
    assert_eq!(events.len(), 1);
    assert!(matches!(
        events[0].kind,
        DaemonEventKind::WorkspaceRenamed { .. }
    ));
}

#[test]
fn pty_reader_pause_forms_an_acknowledged_output_barrier() {
    let shell = create_pending_shell(
        "workspace-id",
        ShellSpec {
            name: "pause-test".into(),
            command: vec![
                "/bin/sh".into(),
                "-c".into(),
                "stty -echo; while IFS= read -r line; do printf 'observed:%s\\n' \"$line\"; done"
                    .into(),
            ],
            cwd: env::temp_dir(),
        },
    )
    .unwrap();
    let terminal_profile = profile();
    let run = Arc::new(ShellRun::new(1));
    let (runtime, reader) = spawn_runtime(
        &shell,
        &run,
        "workspace",
        "pause-test",
        &terminal_profile,
        None,
        RuntimeRecovery::default(),
    )
    .unwrap();
    *lock(&shell.last_run).unwrap() = Some(run.persisted(terminal_profile.clone()).unwrap());
    *lock(&shell.lifecycle).unwrap() = ShellLifecycle::Running {
        profile: terminal_profile,
        run: Arc::clone(&run),
        runtime: Arc::clone(&runtime),
    };
    let registry = Arc::new(DaemonService::default());
    start_pty_reader(
        Arc::downgrade(&registry),
        Arc::clone(&shell),
        run,
        Arc::clone(&runtime),
        reader,
        false,
    )
    .unwrap();

    runtime.pause_reader().unwrap();
    lock(&runtime.master).unwrap().write(b"paused\n").unwrap();
    thread::sleep(Duration::from_millis(50));
    assert!(
        !lock(&runtime.terminal)
            .unwrap()
            .plain_text()
            .contains("observed:paused")
    );

    runtime.resume_reader().unwrap();
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline
        && !lock(&runtime.terminal)
            .unwrap()
            .plain_text()
            .contains("observed:paused")
    {
        thread::sleep(IO_RETRY_DELAY);
    }
    assert!(
        lock(&runtime.terminal)
            .unwrap()
            .plain_text()
            .contains("observed:paused")
    );
    shell.kill().unwrap();
}

#[test]
fn close_does_not_deadlock_with_a_naturally_exiting_reader() {
    let registry = Arc::new(DaemonService::default());
    let workspace = registry
        .create_workspace(
            "exit-race".into(),
            vec![ShellSpec {
                name: "short-lived".into(),
                command: vec!["/bin/sh".into(), "-c".into(), "sleep 0.01".into()],
                cwd: env::temp_dir(),
            }],
        )
        .unwrap();
    let shell = registry.shell(&workspace.shells[0].id).unwrap();
    let terminal_profile = profile();
    let run = Arc::new(ShellRun::new(1));
    let (runtime, reader) = spawn_runtime(
        &shell,
        &run,
        "exit-race",
        "short-lived",
        &terminal_profile,
        None,
        RuntimeRecovery::default(),
    )
    .unwrap();
    *lock(&shell.last_run).unwrap() = Some(run.persisted(terminal_profile.clone()).unwrap());
    *lock(&shell.lifecycle).unwrap() = ShellLifecycle::Running {
        profile: terminal_profile,
        run: Arc::clone(&run),
        runtime: Arc::clone(&runtime),
    };
    start_pty_reader(
        Arc::downgrade(&registry),
        Arc::clone(&shell),
        run,
        runtime,
        reader,
        false,
    )
    .unwrap();
    let shell_id = shell.id.clone();
    let (completed, completion) = mpsc::sync_channel(1);
    thread::spawn(move || {
        let _ = completed.send(registry.close_shell(&shell_id));
    });

    completion
        .recv_timeout(Duration::from_secs(3))
        .expect("shell close deadlocked with the PTY reader")
        .unwrap();
}

#[test]
fn attachment_admission_preserves_management_capacity_and_releases_permits() {
    let admission = Arc::new(ConnectionAdmission::default());
    let mut attachments = Vec::new();
    for _ in 0..MAX_ATTACHMENT_HANDLERS {
        let mut permit = admission.admit().unwrap();
        assert!(permit.attach());
        attachments.push(permit);
    }
    let mut management = Vec::new();
    for _ in 0..MAX_CONNECTION_HANDLERS {
        let mut permit = admission.admit().unwrap();
        assert!(!permit.attach());
        management.push(permit);
    }
    assert!(admission.admit().is_none());
    attachments.pop();
    assert!(management[0].attach());
    assert!(admission.admit().is_some());
    drop(attachments);
    drop(management);
    assert_eq!(admission.management.load(Ordering::Acquire), 0);
    assert_eq!(admission.attachments.load(Ordering::Acquire), 0);
}

#[test]
fn reader_wait_wakes_for_commands_and_disconnection() {
    let (commands, receiver, wake) = ReaderCommands::channel().unwrap();
    let (observed, observation) = mpsc::sync_channel(0);
    let waiter = thread::spawn(move || {
        wake.wait(None, None, None).unwrap();
        wake.clear();
        assert!(matches!(receiver.try_recv(), Ok(ReaderCommand::Resume)));
        observed.send(()).unwrap();
        wake.wait(None, None, None).unwrap();
        assert!(matches!(
            receiver.try_recv(),
            Err(mpsc::TryRecvError::Disconnected)
        ));
    });
    commands.send(ReaderCommand::Resume).unwrap();
    observation.recv_timeout(Duration::from_secs(2)).unwrap();
    drop(commands);
    waiter.join().unwrap();
}

#[test]
fn timed_out_reader_pause_cancels_queued_command() {
    let (commands, receiver, _wake) = ReaderCommands::channel().unwrap();
    let (observed, observation) = mpsc::sync_channel(1);
    let handle = thread::spawn(move || {
        thread::sleep(Duration::from_millis(30));
        if let ReaderCommand::Pause { cancelled, .. } = receiver.recv().unwrap() {
            observed.send(cancelled.load(Ordering::Acquire)).unwrap();
        }
        Ok(())
    });
    let task = ReaderTask {
        commands,
        handle: Mutex::new(Some(handle)),
    };

    let error = task
        .pause_with_timeout(Duration::from_millis(5))
        .unwrap_err();

    assert_eq!(error.kind(), io::ErrorKind::TimedOut);
    assert!(observation.recv().unwrap());
    task.stop().unwrap();
}

#[test]
fn reports_only_term_mismatches() {
    assert_eq!(term_mismatch_warning(Some("xterm"), Some("xterm")), None);
    assert!(term_mismatch_warning(Some("xterm"), Some("alacritty")).is_some());
    assert!(term_mismatch_warning(None, Some("xterm")).is_some());
}

#[test]
fn run_completion_never_precedes_its_start_timestamp() {
    let run = ShellRun {
        id: Uuid::new_v4().to_string(),
        generation: 1,
        started_at_ms: u64::MAX,
        ended: Mutex::new(None),
        output_revision: AtomicU64::new(0),
        environment_has_run_id: true,
    };

    run.finish(ShellRunExitReason::Interrupted).unwrap();

    assert_eq!(run.snapshot().unwrap().ended_at_ms, Some(u64::MAX));
}

#[test]
fn pathless_workspace_snapshot_has_no_default_cwd_and_no_shells() {
    let registry = DaemonService::default();

    let workspace = registry
        .create_workspace("empty".into(), Vec::new())
        .unwrap();
    let value = serde_json::to_value(&workspace).unwrap();

    assert!(workspace.shells.is_empty());
    assert!(workspace.default_cwd.is_none());
    assert!(value.get("default_cwd").is_none());
}

#[test]
fn unavailable_global_store_suppresses_runtime_coordination_capabilities() {
    let registry = DaemonService::default();
    let capabilities = registry.runtime_protocol_capabilities();
    assert!(!capabilities.iter().any(|capability| {
        protocol::ProtocolFeature::GlobalWorkspaces
            .capability_names()
            .contains(&capability.as_str())
    }));
}

#[test]
fn forced_node_projection_refresh_interrupts_only_the_existing_worker_sleep() {
    let registry = DaemonService::default();
    let node_id = Uuid::from_u128(42).to_string();
    lock(&registry.node_projection_workers.wake)
        .unwrap()
        .insert(node_id.clone());
    let started = Instant::now();
    assert!(!interruptible_node_sleep(
        &registry,
        &node_id,
        Duration::from_secs(1)
    ));
    assert!(started.elapsed() < Duration::from_millis(200));
    assert!(
        lock(&registry.node_projection_workers.wake)
            .unwrap()
            .is_empty()
    );
}

#[test]
fn guarded_resource_mutations_reject_stale_revisions_without_changing_legacy_semantics() {
    let registry = DaemonService::default();
    let workspace = registry
        .create_workspace("guarded".into(), Vec::new())
        .unwrap();
    let stale = registry
        .dispatch(Request::GuardedRenameWorkspace {
            workspace_id: workspace.id.clone(),
            name: "stale".into(),
            expected_revision: workspace.revision + 1,
        })
        .unwrap_err();
    assert_eq!(stale.wire_code(), ErrorCode::RevisionAhead);
    assert_eq!(
        registry
            .workspace(&workspace.id)
            .unwrap()
            .snapshot(&registry.durable)
            .unwrap()
            .name,
        "guarded"
    );

    registry
        .dispatch(Request::RenameWorkspace {
            workspace_id: workspace.id.clone(),
            name: "legacy".into(),
        })
        .unwrap();
    let current = registry
        .workspace(&workspace.id)
        .unwrap()
        .snapshot(&registry.durable)
        .unwrap();
    assert_eq!(current.name, "legacy");
    assert_eq!(current.revision, workspace.revision + 1);
}

#[test]
fn workspace_snapshot_retains_default_cwd() {
    let registry = DaemonService::default();
    let cwd = env::temp_dir();

    let workspace = registry
        .create_workspace_with_default_cwd("project".into(), Some(cwd.clone()), Vec::new())
        .unwrap();

    assert_eq!(workspace.default_cwd.as_deref(), Some(cwd.as_path()));
}

#[test]
fn protocol_eighteen_responses_hide_workspace_default_cwd() {
    let source_workspace = WorkspaceSnapshot {
        id: "w1".into(),
        revision: 1,
        name: "project".into(),
        default_cwd: Some("/tmp/project".into()),
        shells: Vec::new(),
        launchers: Vec::new(),
        agents: Vec::new(),
    };

    let Response::Workspace { workspace } = response_for_version(
        Response::Workspace {
            workspace: source_workspace.clone(),
        },
        18,
    ) else {
        panic!("expected workspace response");
    };

    assert!(workspace.default_cwd.is_none());

    let Response::Snapshot { snapshot } = response_for_version(
        Response::Snapshot {
            snapshot: Snapshot {
                workspaces: vec![source_workspace.clone()],
                focused_terminal: None,
            },
        },
        18,
    ) else {
        panic!("expected snapshot response");
    };
    assert!(snapshot.workspaces[0].default_cwd.is_none());

    let Response::Events {
        snapshot: Some(snapshot),
        ..
    } = response_for_version(
        Response::Events {
            stream_id: "stream".into(),
            cursor: EventCursor {
                stream_id: "stream".into(),
                event_id: 0,
            },
            snapshot: Some(Snapshot {
                workspaces: vec![source_workspace],
                focused_terminal: None,
            }),
            events: Vec::new(),
        },
        18,
    )
    else {
        panic!("expected events response");
    };
    assert!(snapshot.workspaces[0].default_cwd.is_none());
}

#[test]
fn concurrent_duplicate_workspace_names_publish_only_once() {
    let registry = Arc::new(DaemonService::default());
    let barrier = Arc::new(Barrier::new(3));
    let threads = (0..2)
        .map(|_| {
            let registry = Arc::clone(&registry);
            let barrier = Arc::clone(&barrier);
            thread::spawn(move || {
                barrier.wait();
                registry.create_workspace("same".into(), Vec::new())
            })
        })
        .collect::<Vec<_>>();
    barrier.wait();
    let results = threads
        .into_iter()
        .map(|thread| thread.join().unwrap())
        .collect::<Vec<_>>();

    assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
    assert!(results.iter().any(|result| {
        result
            .as_ref()
            .is_err_and(|error| error.kind() == io::ErrorKind::AlreadyExists)
    }));
    assert_eq!(registry.snapshot().unwrap().workspaces.len(), 1);
}

#[test]
fn shell_without_workspace_gets_the_next_generated_workspace_name() {
    let registry = DaemonService::default();
    registry
        .create_workspace("workspace-1".into(), Vec::new())
        .unwrap();

    let shell = registry
        .create_shell_with_workspace(ShellSpec {
            name: "shell-1".into(),
            command: vec!["/bin/sh".into(), "-c".into(), "sleep 5".into()],
            cwd: env::temp_dir(),
        })
        .unwrap();
    let workspace = registry.workspace(&shell.workspace_id).unwrap();

    assert_eq!(&*lock(&workspace.name).unwrap(), "workspace-2");
    assert_eq!(
        lock(&workspace.default_cwd).unwrap().as_deref(),
        Some(env::temp_dir().as_path())
    );
    registry.shutdown().unwrap();
}

#[test]
fn daemon_lock_allows_only_one_owner() {
    let directory = env::temp_dir().join(format!("boomux-lock-test-{}", Uuid::new_v4()));
    fs::create_dir(&directory).unwrap();
    let first = acquire_daemon_lock(&directory).unwrap();

    let error = acquire_daemon_lock(&directory).unwrap_err();

    assert_eq!(error.kind(), io::ErrorKind::AddrInUse);
    drop(first);
    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn registry_closes_shell_without_removing_workspace() {
    let registry = DaemonService::default();
    let workspace = registry
        .create_workspace(
            "test".into(),
            vec![ShellSpec {
                name: "one".into(),
                command: vec!["/bin/sh".into(), "-c".into(), "sleep 5".into()],
                cwd: env::temp_dir(),
            }],
        )
        .unwrap();
    assert_eq!(workspace.shells.len(), 1);
    assert_eq!(registry.snapshot().unwrap().workspaces.len(), 1);

    registry.close_shell(&workspace.shells[0].id).unwrap();
    let snapshot = registry.snapshot().unwrap();
    assert_eq!(snapshot.workspaces.len(), 1);
    assert!(snapshot.workspaces[0].shells.is_empty());

    registry.close_workspace(&workspace.id).unwrap();
    assert!(registry.snapshot().unwrap().workspaces.is_empty());
}

#[test]
fn persisted_attention_rejects_impossible_observation_history() {
    let persisted = |attention: AgentAttentionSnapshot| PersistedAgentInstance {
        id: Uuid::new_v4().to_string(),
        shell_id: Uuid::new_v4().to_string(),
        run_id: Uuid::new_v4().to_string(),
        name: "agent".into(),
        integration: "test".into(),
        external_session_id: Some("session".into()),
        cwd: Some(env::temp_dir()),
        started_at_ms: 10,
        ended_at_ms: None,
        observation: AgentObservationSnapshot {
            revision: 2,
            state: AgentState::Working,
            authority: AgentAuthority::LifecycleIntegration,
            evidence: "working".into(),
            confidence: 100,
            observed_at_ms: 20,
        },
        attention: Some(attention),
        working_contexts: Vec::new(),
    };
    let completed = AgentAttentionSnapshot {
        reason: AgentAttentionReason::Completed,
        observation: AgentObservationSnapshot {
            revision: 1,
            state: AgentState::Done,
            authority: AgentAuthority::LifecycleIntegration,
            evidence: "done".into(),
            confidence: 100,
            observed_at_ms: 15,
        },
    };
    assert_eq!(
        validate_persisted_agent(&persisted(completed))
            .unwrap_err()
            .kind(),
        io::ErrorKind::InvalidData
    );

    let mismatched_current = AgentAttentionSnapshot {
        reason: AgentAttentionReason::Blocked,
        observation: AgentObservationSnapshot {
            revision: 2,
            state: AgentState::Blocked,
            authority: AgentAuthority::LifecycleIntegration,
            evidence: "different revision contents".into(),
            confidence: 100,
            observed_at_ms: 20,
        },
    };
    assert_eq!(
        validate_persisted_agent(&persisted(mismatched_current))
            .unwrap_err()
            .kind(),
        io::ErrorKind::InvalidData
    );
}

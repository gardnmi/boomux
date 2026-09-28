//! Opt-in cold recovery measurements; all processes and harness files are isolated.
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use boomux::protocol::{
    AgentAuthority, AgentRegistrationSpec, AgentReport, AgentState, AttachFrame,
    ShellRecoveryResult, ShellRecoveryTarget, ShellRunExitReason, ShellSpec, ShellStatus,
};
use uuid::Uuid;

use crate::support::{TestDaemon, profile, read_until, read_until_after};

struct TimingState(Option<PathBuf>);

impl Drop for TimingState {
    fn drop(&mut self) {
        if let Some(path) = &self.0 {
            let _ = fs::remove_dir_all(path);
        }
    }
}

fn count(name: &str, default: usize, max: usize) -> usize {
    let value = std::env::var(name)
        .map(|value| value.parse::<usize>().expect("expected a positive integer"))
        .unwrap_or(default);
    assert!((1..=max).contains(&value), "{name} must be in 1..={max}");
    value
}

fn process_metrics(_pid: u32) -> serde_json::Value {
    #[cfg(target_os = "linux")]
    {
        let root = PathBuf::from(format!("/proc/{_pid}"));
        let status = fs::read_to_string(root.join("status")).unwrap();
        let smaps = fs::read_to_string(root.join("smaps_rollup")).unwrap();
        let field = |text: &str, key: &str| -> u64 {
            text.lines()
                .find_map(|line| line.strip_prefix(key))
                .unwrap()
                .split_whitespace()
                .next()
                .unwrap()
                .parse()
                .unwrap()
        };
        let stat = fs::read_to_string(root.join("stat")).unwrap();
        let fields = stat
            .rsplit_once(") ")
            .unwrap()
            .1
            .split_whitespace()
            .collect::<Vec<_>>();
        let ticks: u64 = fields[11].parse::<u64>().unwrap() + fields[12].parse::<u64>().unwrap();
        let hz = unsafe { libc::sysconf(libc::_SC_CLK_TCK) } as f64;
        serde_json::json!({
            "rss_kib": field(&status, "VmRSS:"), "pss_kib": field(&smaps, "Pss:"),
            "peak_rss_kib": field(&status, "VmHWM:"), "cpu_ms": ticks as f64 * 1000.0 / hz,
            "threads": field(&status, "Threads:"), "fds": fs::read_dir(root.join("fd")).unwrap().count(),
        })
    }
    #[cfg(not(target_os = "linux"))]
    serde_json::Value::Null
}

#[test]
#[ignore = "explicit cold recovery timing diagnostic; see docs/desktop/performance.md"]
fn cold_recovery_phase_timings() {
    let shell_count = count("BOOMUX_TIMING_SHELLS", 4, 32);
    let samples = count("BOOMUX_TIMING_SAMPLES", 3, 10);
    let poll_refill = std::env::var("BOOMUX_TIMING_POLL_REFILL").is_ok_and(|value| value == "1");
    let state =
        TimingState(std::env::var_os("BOOMUX_TIMING_STATE_ROOT").map(|root| {
            PathBuf::from(root).join(format!("boomux-cold-timing-{}", Uuid::new_v4()))
        }));
    let batch = std::env::var("BOOMUX_TIMING_BATCH_RECOVERY").is_ok_and(|value| value == "1");
    let compare = std::env::var("BOOMUX_TIMING_COMPARE_RECOVERY").is_ok_and(|value| value == "1");
    exercise_cold_recovery(shell_count, samples, poll_refill, batch, compare, state);
}

#[test]
fn cold_recovery_batch_resumes_exact_conversations() {
    exercise_cold_recovery(4, 1, false, true, false, TimingState(None));
}

fn exercise_cold_recovery(
    shell_count: usize,
    samples: usize,
    poll_refill: bool,
    batch: bool,
    compare: bool,
    state: TimingState,
) {
    assert!(
        !(batch || compare) || shell_count <= 4,
        "batch timing compares one group of at most four Shells"
    );
    let mut daemon = TestDaemon::start_with(|command, runtime| {
        let bin = runtime.join("bin");
        let codex_home = runtime.join("codex-home");
        fs::create_dir(&bin).unwrap();
        fs::create_dir(&codex_home).unwrap();
        fs::write(
            codex_home.join("hooks.json"),
            include_str!("../../integrations/codex/hooks.json"),
        )
        .unwrap();
        let executable = bin.join("codex");
        // Exercise the real Codex launch wrapper and exact resume argv. Readiness
        // and input replies are deterministic; no real harness or network is used.
        fs::write(
            &executable,
            "#!/bin/sh\n[ \"$1\" = --enable ] && [ \"$2\" = hooks ] || exit 90\nshift 2\nif [ \"${1-}\" = resume ]; then session=$2; else session=initial; fi\nprintf 'cold-ready:%s:%s\\n' \"$session\" \"$BOOMUX_RUN_ID\"\nwhile IFS= read -r line; do printf 'cold-reply:%s\\n' \"$line\"; done\n",
        )
        .unwrap();
        fs::set_permissions(executable, fs::Permissions::from_mode(0o700)).unwrap();
        command.env("PATH", bin).env("CODEX_HOME", codex_home);
        if let Some(state) = &state.0 {
            command.env("BOOMUX_STATE_HOME", state);
        }
    });
    let bin = daemon.runtime_dir.join("bin");
    eprintln!(
        "{}",
        serde_json::json!({"empty_daemon": process_metrics(daemon.child.as_ref().unwrap().id())})
    );
    let codex_home = daemon.runtime_dir.join("codex-home");
    let workspace = daemon
        .client
        .create_workspace(
            "cold-timing",
            (0..shell_count)
                .map(|index| ShellSpec {
                    name: format!("agent-{index}"),
                    command: vec![bin.join("codex").display().to_string()],
                    cwd: daemon.runtime_dir.clone(),
                })
                .collect(),
        )
        .unwrap();
    let mut runs = Vec::new();
    for shell in &workspace.shells {
        let mut attachment = daemon.client.attach(&shell.id, false, profile()).unwrap();
        read_until_after(
            &mut attachment.stream,
            b"cold-ready:initial:",
            attachment.reconstruction,
        );
        runs.push(daemon.client.get_shell(&shell.id).unwrap().run.unwrap().id);
    }
    eprintln!(
        "cold recovery: profile={}, shells={shell_count}, concurrency=4, terminal=24x80, samples={samples}, poll_refill={poll_refill}, batch={batch}, external_state={}",
        if cfg!(debug_assertions) {
            "debug"
        } else {
            "release"
        },
        state.0.is_some()
    );
    for sample in 0..samples {
        let batch = if compare { sample % 2 == 1 } else { batch };
        // A real harness registers on each new run; do that outside the timed
        // interval so this fixture measures admission rather than hook writes.
        for (index, (shell, run)) in workspace.shells.iter().zip(&runs).enumerate() {
            daemon
                .client
                .register_agent(
                    &shell.id,
                    run,
                    AgentRegistrationSpec {
                        name: "Timing agent".into(),
                        integration: "codex".into(),
                        external_session_id: Some(format!("cold-session-{index}")),
                        report: AgentReport {
                            state: AgentState::Idle,
                            authority: AgentAuthority::LifecycleIntegration,
                            evidence: "timing fixture ready".into(),
                            confidence: 100,
                        },
                    },
                )
                .unwrap();
        }
        daemon.crash();
        let started = Instant::now();
        daemon.restart_with(|command| {
            command.env("PATH", &bin).env("CODEX_HOME", &codex_home);
            if let Some(state) = &state.0 {
                command.env("BOOMUX_STATE_HOME", state);
            }
        });
        let daemon_ready = started.elapsed();
        let snapshot = daemon.client.snapshot().unwrap();
        let snapshot_ready = started.elapsed();
        assert_eq!(snapshot.workspaces[0].shells.len(), shell_count);
        assert!(snapshot.workspaces[0].shells.iter().all(|shell| {
            shell.status == ShellStatus::Pending && shell.recovered_agent_id.is_some()
        }));
        let batch_started = Instant::now();
        let recovered = batch.then(|| {
            daemon
                .client
                .recover_shells(
                    workspace
                        .shells
                        .iter()
                        .zip(&runs)
                        .map(|(shell, run)| ShellRecoveryTarget {
                            shell_id: shell.id.clone(),
                            expected_run_id: run.clone(),
                            profile: profile(),
                        })
                        .collect(),
                )
                .unwrap()
                .unwrap()
                .into_iter()
                .map(|result| {
                    let ShellRecoveryResult::Started { shell } = result else {
                        panic!("recovery skipped a fixture Shell")
                    };
                    *shell
                })
                .collect::<Vec<_>>()
        });
        let batch_duration = batch_started.elapsed();
        let next = AtomicUsize::new(0);
        let refill_epoch = Instant::now();
        let results = std::thread::scope(|scope| {
            let handles = (0..shell_count.min(4))
                .map(|_| {
                    scope.spawn(|| {
                        let mut results = Vec::new();
                        loop {
                            let index = next.fetch_add(1, Ordering::Relaxed);
                            let Some(shell) = workspace.shells.get(index) else {
                                return results;
                            };
                            if poll_refill && index >= 4 {
                                // Model the old Desktop policy: freed slots wait
                                // for the next one-second overview tick. Omit
                                // overview I/O to measure the scheduling floor.
                                let next_tick = Duration::from_secs(refill_epoch.elapsed().as_secs() + 1);
                                std::thread::sleep(next_tick.saturating_sub(refill_epoch.elapsed()));
                            }
                            let begin = Instant::now();
                            let pending = if let Some(recovered) = &recovered {
                                recovered[index].clone()
                            } else {
                                daemon.client.get_shell(&shell.id).unwrap()
                            };
                            let inspected = begin.elapsed();
                            let previous = pending.run.unwrap();
                            if !batch {
                                assert_eq!(previous.id, runs[index]);
                                assert_eq!(previous.exit_reason, Some(ShellRunExitReason::Interrupted));
                            }
                            let mut attachment = if batch {
                                daemon.client.attach_exact_run(&shell.id, &previous.id, false, profile()).unwrap()
                            } else {
                                daemon.client.attach(&shell.id, false, profile()).unwrap()
                            };
                            let attached = begin.elapsed();
                            let current = if batch { previous.clone() } else { daemon.client.get_shell(&shell.id).unwrap().run.unwrap() };
                            let run_known = begin.elapsed();
                            assert_ne!(current.id, runs[index]);
                            assert_eq!(current.generation, sample as u64 + 2);
                            read_until_after(
                                &mut attachment.stream,
                                format!("cold-ready:cold-session-{index}:{}", current.id).as_bytes(),
                                attachment.reconstruction,
                            );
                            let output_ready = begin.elapsed();
                            AttachFrame::Input(b"probe\n".to_vec())
                                .write_to(&mut attachment.stream)
                                .unwrap();
                            read_until(&mut attachment.stream, b"cold-reply:probe");
                            let input_ready = begin.elapsed();
                            results.push((index, current.id, serde_json::json!({
                                "sample": sample, "shell": index,
                                "inspect_ms": inspected.as_secs_f64() * 1000.0,
                                "attach_ms": (attached - inspected).as_secs_f64() * 1000.0,
                                "run_lookup_ms": (run_known - attached).as_secs_f64() * 1000.0,
                                "first_output_ms": (output_ready - run_known).as_secs_f64() * 1000.0,
                                "input_ms": (input_ready - output_ready).as_secs_f64() * 1000.0,
                                "ready_since_restart_ms": started.elapsed().as_secs_f64() * 1000.0,
                            })));
                        }
                    })
                })
                .collect::<Vec<_>>();
            handles
                .into_iter()
                .flat_map(|handle| handle.join().unwrap())
                .collect::<Vec<_>>()
        });
        let all_ready_ms = started.elapsed().as_secs_f64() * 1000.0;
        eprintln!(
            "{}",
            serde_json::json!({
                "sample": sample,
                "batch": batch,
                "batch_start_ms": if batch { batch_duration.as_secs_f64() * 1000.0 } else { 0.0 },
                "daemon_ready_ms": daemon_ready.as_secs_f64() * 1000.0,
                "snapshot_ms": (snapshot_ready - daemon_ready).as_secs_f64() * 1000.0,
                "all_ready_ms": all_ready_ms,
                "daemon": process_metrics(daemon.child.as_ref().unwrap().id()),
            })
        );
        for (index, run, timing) in results {
            runs[index] = run;
            eprintln!("{timing}");
        }
    }
    daemon.client.close_workspace(&workspace.id).unwrap();
    eprintln!(
        "{}",
        serde_json::json!({"after_cleanup_daemon": process_metrics(daemon.child.as_ref().unwrap().id())})
    );
}

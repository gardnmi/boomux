//! Bounded owner-authoritative cold starts sharing one durable commit.
use super::*;
use crate::protocol::{MAX_RECOVERY_SHELLS, ShellRecoveryResult, ShellRecoveryTarget};

struct StartedRecovery {
    shell: Arc<Shell>,
    previous: PersistedShellRun,
    runtime: Arc<ShellRuntime>,
}

impl DaemonService {
    pub(super) fn recover_shells(
        self: &Arc<Self>,
        targets: Vec<ShellRecoveryTarget>,
        environment: Option<UnixEnvironment>,
    ) -> DaemonResult<Response> {
        if targets.is_empty() || targets.len() > MAX_RECOVERY_SHELLS {
            return Err(DaemonError::validation(
                "recovery requires one to four Shells",
            ));
        }
        let mut identities = HashSet::new();
        for target in &targets {
            validate_id("shell", &target.shell_id)?;
            validate_id("run", &target.expected_run_id)?;
            validate_terminal_profile(&target.profile)?;
            if !identities.insert(&target.shell_id) {
                return Err(DaemonError::validation("duplicate recovery Shell"));
            }
        }
        if let Some(environment) = &environment {
            validate_unix_environment(environment)?;
        }
        let mutation = lock(&self.mutation_lock)?;
        self.checkpoint_local_shell_transactions_with_mutation()
            .map_err(DaemonError::persistence)?;
        self.ensure_running()?;
        self.flush_pending()?;
        let persistence = lock(&self.durable.persist_lock)?;
        let mut transaction = self.events.transaction()?;
        transaction.reserve_with_pending(targets.len())?;
        let mut started = Vec::with_capacity(targets.len());
        let staged = (|| -> DaemonResult<_> {
            let mut results = Vec::with_capacity(targets.len());
            let mut events = Vec::with_capacity(targets.len());
            for target in targets {
                let selected = self.recovery_target(&target);
                let (shell, previous) = match selected {
                    Ok(selected) => selected,
                    Err(error) => {
                        results.push(ShellRecoveryResult::Unavailable {
                            shell_id: target.shell_id,
                            code: error.wire_code(),
                            message: error.to_string(),
                        });
                        continue;
                    }
                };
                let resumable = self.resumable_agent(&shell, Some(&previous))?;
                let workspace = self.workspace(&shell.workspace_id)?;
                let workspace_name = lock(&workspace.name)?.clone();
                let shell_name = lock(&shell.name)?.clone();
                let generation = previous
                    .generation
                    .checked_add(1)
                    .ok_or_else(|| io::Error::other("shell run generation exhausted"))?;
                let run = Arc::new(ShellRun::new(generation));
                let saved_run = run.persisted(target.profile.clone())?;
                let mut lifecycle = lock(&shell.lifecycle)?;
                let spawned = self.runtimes.spawn_runtime(
                    &shell,
                    &run,
                    RuntimeStart {
                        workspace_name: &workspace_name,
                        shell_name: &shell_name,
                        profile: &target.profile,
                        environment: environment.as_ref(),
                        recovery: RuntimeRecovery {
                            effective_command: resumable
                                .as_ref()
                                .map(|agent| agent.command.as_slice()),
                            opencode_session_id: resumable
                                .as_ref()
                                .filter(|agent| agent.integration == "opencode")
                                .map(|agent| agent.external_session_id.as_str()),
                            history: self
                                .notification_settings
                                .persist_terminal_history
                                .then_some(previous.terminal_history.as_deref())
                                .flatten(),
                        },
                        claude_remote_control: self.notification_settings.claude_remote_control,
                    },
                );
                let (runtime, reader) = match spawned {
                    Ok(spawned) => spawned,
                    Err(error) => {
                        results.push(ShellRecoveryResult::Unavailable {
                            shell_id: target.shell_id,
                            code: ErrorCode::ShellStartFailed,
                            message: format!("could not start recovered Shell: {error}"),
                        });
                        continue;
                    }
                };
                *lifecycle = ShellLifecycle::Running {
                    profile: target.profile,
                    run: Arc::clone(&run),
                    runtime: Arc::clone(&runtime),
                };
                started.push(StartedRecovery {
                    shell: Arc::clone(&shell),
                    previous,
                    runtime: Arc::clone(&runtime),
                });
                drop(lifecycle);
                *lock(&shell.last_run)? = Some(saved_run);
                self.runtimes.start_pty_reader(
                    Arc::downgrade(self),
                    Arc::clone(&shell),
                    Arc::clone(&run),
                    runtime,
                    reader,
                    true,
                )?;
                events.push(DaemonEventKind::RunStarted {
                    workspace_id: shell.workspace_id.clone(),
                    shell_id: shell.id.clone(),
                    run: run.snapshot()?,
                });
                results.push(ShellRecoveryResult::Started {
                    shell: Box::new(shell.snapshot()?),
                });
            }
            let saved = if started.is_empty() {
                None
            } else {
                Some(self.capture_persisted_state()?)
            };
            Ok((results, events, saved))
        })();
        let (results, events, saved) = match staged {
            Ok(staged) => staged,
            Err(error) => return Err(self.rollback_recovery(error, &started)),
        };
        let Some(saved) = saved else {
            return Ok(Response::RecoveredShells { results });
        };
        transaction.begin_persistence(events.len());
        drop(transaction);
        self.recovery_test_barrier("before-commit");
        if let Err(error) = self.write_persisted_state(saved) {
            // Roll back under the mutation gate before any caller can attach
            // to a failed start. All staged readers are still paused.
            let error = self.rollback_recovery(DaemonError::persistence(error), &started);
            let mut transaction = self.events.transaction()?;
            transaction.finish_persistence();
            drop(transaction);
            self.events.notify();
            return Err(error);
        }
        self.recovery_test_barrier("after-commit");
        let mut transaction = self.events.transaction()?;
        transaction.append_batch(events);
        transaction.finish_persistence();
        drop(transaction);
        drop(persistence);
        self.events.notify();
        // Attempt every resume even if one reader has unexpectedly stopped.
        let mut failure = None;
        for entry in &started {
            if let Err(error) = self.runtimes.resume_reader(&entry.runtime) {
                failure.get_or_insert(error);
            }
        }
        drop(mutation);
        if let Some(error) = failure {
            return Err(DaemonError::lifecycle(
                ErrorCode::OutcomeUnknown,
                format!("recovery committed but reader resume failed: {error}"),
            ));
        }
        Ok(Response::RecoveredShells { results })
    }

    fn recovery_target(
        &self,
        target: &ShellRecoveryTarget,
    ) -> DaemonResult<(Arc<Shell>, PersistedShellRun)> {
        let shell = self.shell(&target.shell_id)?;
        if !matches!(*lock(&shell.lifecycle)?, ShellLifecycle::Pending) {
            return Err(DaemonError::lifecycle(
                ErrorCode::RunChanged,
                "Shell is no longer pending",
            ));
        }
        let previous = lock(&shell.last_run)?
            .clone()
            .filter(|run| run.id == target.expected_run_id)
            .ok_or_else(|| {
                DaemonError::lifecycle(ErrorCode::RunChanged, "Previous ShellRun changed")
            })?;
        if previous.exit_reason != Some(ShellRunExitReason::Interrupted) {
            return Err(DaemonError::validation(
                "Shell ended normally; start it explicitly",
            ));
        }
        Ok((shell, previous))
    }

    fn rollback_recovery(
        &self,
        mut error: DaemonError,
        started: &[StartedRecovery],
    ) -> DaemonError {
        for entry in started.iter().rev() {
            let restored = (|| -> io::Result<()> {
                self.runtimes.kill(&entry.shell)?;
                *lock(&entry.shell.last_run)? = Some(entry.previous.clone());
                self.runtimes.reset_pending(&entry.shell)
            })();
            if let Err(cleanup) = restored {
                error = Self::append_error_context(
                    error,
                    format!("recovery rollback failed: {cleanup}"),
                );
            }
        }
        error
    }

    fn recovery_test_barrier(&self, _phase: &str) {
        #[cfg(debug_assertions)]
        if self.native_test_hooks_enabled()
            && let Ok(directory) = state_directory_from_environment()
        {
            let barrier = directory.join(format!(".native-test-recovery-{_phase}"));
            if barrier.is_dir() && fs::write(barrier.join("ready"), b"").is_ok() {
                let deadline = Instant::now() + Duration::from_secs(10);
                while !barrier.join("release").exists() && Instant::now() < deadline {
                    thread::sleep(Duration::from_millis(10));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fixture {
        root: PathBuf,
        registry: Arc<DaemonService>,
        targets: Vec<ShellRecoveryTarget>,
        writes: Arc<AtomicUsize>,
    }

    impl Fixture {
        fn new(missing_last: bool) -> Self {
            let root = env::temp_dir().join(format!("boomux-recovery-{}", Uuid::new_v4()));
            fs::create_dir(&root).unwrap();
            let writes = Arc::new(AtomicUsize::new(0));
            let counter = Arc::clone(&writes);
            let registry = Arc::new(
                DaemonService::restore(
                    StateStore::at_with_save_hook(
                        root.join("state.json"),
                        Arc::new(move || {
                            counter.fetch_add(1, Ordering::Relaxed);
                        }),
                    ),
                    false,
                    None,
                )
                .unwrap(),
            );
            let workspace = registry
                .create_workspace(
                    "recovery".into(),
                    (0..4)
                        .map(|i| ShellSpec {
                            name: format!("shell-{i}"),
                            cwd: root.clone(),
                            command: if missing_last && i == 3 {
                                vec![root.join("missing").display().to_string()]
                            } else {
                                vec![
                                    "/bin/sh".into(),
                                    "-c".into(),
                                    format!("echo $$ > child-{i}; exec /bin/sleep 60"),
                                ]
                            },
                        })
                        .collect(),
                )
                .unwrap();
            let targets = workspace
                .shells
                .iter()
                .map(|snapshot| {
                    let shell = registry.shell(&snapshot.id).unwrap();
                    let run = ShellRun::new(1);
                    run.finish(ShellRunExitReason::Interrupted).unwrap();
                    let profile = TerminalProfile {
                        term: None,
                        colorterm: None,
                        term_program: None,
                        term_program_version: None,
                        rows: 24,
                        cols: 80,
                        pixel_width: 0,
                        pixel_height: 0,
                    };
                    *lock(&shell.last_run).unwrap() = Some(run.persisted(profile.clone()).unwrap());
                    ShellRecoveryTarget {
                        shell_id: shell.id.clone(),
                        expected_run_id: run.id,
                        profile,
                    }
                })
                .collect();
            registry.persist().unwrap();
            writes.store(0, Ordering::Relaxed);
            Self {
                root,
                registry,
                targets,
                writes,
            }
        }

        fn assert_rolled_back(&self) {
            for target in &self.targets {
                let shell = self.registry.shell(&target.shell_id).unwrap();
                assert!(matches!(
                    *lock(&shell.lifecycle).unwrap(),
                    ShellLifecycle::Pending
                ));
                assert_eq!(
                    lock(&shell.last_run).unwrap().as_ref().unwrap().id,
                    target.expected_run_id
                );
            }
            assert!(self.registry.events.manifest().unwrap().events.is_empty());
            for i in 0..4 {
                if let Ok(pid) = fs::read_to_string(self.root.join(format!("child-{i}"))) {
                    let pid: i32 = pid.trim().parse().unwrap();
                    assert_eq!(
                        unsafe { libc::kill(pid, 0) },
                        -1,
                        "leaked recovered child {pid}"
                    );
                }
            }
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = self.registry.shutdown();
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    #[test]
    fn cold_recovery_commits_four_runs_once_and_rejects_replay() {
        let fixture = Fixture::new(false);
        let Response::RecoveredShells { results } = fixture
            .registry
            .recover_shells(fixture.targets.clone(), None)
            .unwrap()
        else {
            panic!()
        };
        assert_eq!(fixture.writes.load(Ordering::Relaxed), 1);
        let saved = fs::read_to_string(fixture.root.join("state.json")).unwrap();
        for (result, target) in results.iter().zip(&fixture.targets) {
            let ShellRecoveryResult::Started { shell } = result else {
                panic!()
            };
            let run = shell.run.as_ref().unwrap();
            assert_ne!(run.id, target.expected_run_id);
            assert_eq!(run.generation, 2);
            assert!(saved.contains(&run.id));
        }
        assert_eq!(
            fixture
                .registry
                .events
                .manifest()
                .unwrap()
                .events
                .iter()
                .filter(|event| matches!(event.kind, DaemonEventKind::RunStarted { .. }))
                .count(),
            4
        );
        let Response::RecoveredShells { results } = fixture
            .registry
            .recover_shells(fixture.targets.clone(), None)
            .unwrap()
        else {
            panic!()
        };
        assert!(results.iter().all(|result| matches!(
            result,
            ShellRecoveryResult::Unavailable {
                code: ErrorCode::RunChanged,
                ..
            }
        )));
        assert_eq!(fixture.writes.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn cold_recovery_rolls_back_all_staged_runs_on_stage_or_commit_failure() {
        for stage_failure in [true, false] {
            let fixture = Fixture::new(false);
            if stage_failure {
                let shell = fixture
                    .registry
                    .shell(&fixture.targets[3].shell_id)
                    .unwrap();
                lock(&shell.last_run).unwrap().as_mut().unwrap().generation = u64::MAX;
            } else {
                fixture.registry.fail_next_persistence();
            }
            assert!(
                fixture
                    .registry
                    .recover_shells(fixture.targets.clone(), None)
                    .is_err()
            );
            fixture.assert_rolled_back();
            assert_eq!(fixture.writes.load(Ordering::Relaxed), 0);
            if !stage_failure {
                // The failed transaction leaves the previous exact identities retryable.
                assert!(
                    fixture
                        .registry
                        .recover_shells(fixture.targets.clone(), None)
                        .is_ok()
                );
            }
        }
    }

    #[test]
    fn cold_recovery_missing_executable_does_not_block_other_shells() {
        let fixture = Fixture::new(true);
        let Response::RecoveredShells { results } = fixture
            .registry
            .recover_shells(fixture.targets.clone(), None)
            .unwrap()
        else {
            panic!()
        };
        assert!(
            results[..3]
                .iter()
                .all(|result| matches!(result, ShellRecoveryResult::Started { .. }))
        );
        assert!(matches!(
            results[3],
            ShellRecoveryResult::Unavailable {
                code: ErrorCode::ShellStartFailed,
                ..
            }
        ));
        assert_eq!(fixture.writes.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn cold_recovery_skips_ineligible_targets_and_bounds_requests() {
        let fixture = Fixture::new(false);
        for targets in [
            vec![],
            vec![fixture.targets[0].clone(); 2],
            vec![fixture.targets[0].clone(); 5],
        ] {
            assert!(fixture.registry.recover_shells(targets, None).is_err());
        }
        let mut targets = fixture.targets.clone();
        targets[0].expected_run_id = Uuid::new_v4().to_string();
        let shell = fixture.registry.shell(&targets[1].shell_id).unwrap();
        lock(&shell.last_run).unwrap().as_mut().unwrap().exit_reason =
            Some(ShellRunExitReason::Exited { code: Some(0) });
        targets[2].shell_id = Uuid::new_v4().to_string();
        let Response::RecoveredShells { results } =
            fixture.registry.recover_shells(targets, None).unwrap()
        else {
            panic!()
        };
        for (result, code) in results[..3].iter().zip([
            ErrorCode::RunChanged,
            ErrorCode::InvalidArgument,
            ErrorCode::NotFound,
        ]) {
            assert!(
                matches!(result, ShellRecoveryResult::Unavailable { code: actual, .. } if *actual == code)
            );
        }
        assert!(matches!(&results[3], ShellRecoveryResult::Started { .. }));
        assert_eq!(fixture.writes.load(Ordering::Relaxed), 1);
    }
}

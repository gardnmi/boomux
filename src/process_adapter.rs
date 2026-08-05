use std::error::Error;
use std::fmt;
use std::io;
use std::os::unix::process::ExitStatusExt;
use std::process::{Child, Command, Stdio};

use boomux::client;
use boomux::protocol::{
    AgentAuthority, AgentInstanceSnapshot, AgentRegistrationSpec, AgentReport, AgentState,
};

const CONFIDENCE: u8 = 100;

pub struct SuperviseSpec {
    pub name: String,
    pub integration: String,
    pub external_session_id: String,
    pub shell_id: String,
    pub run_id: String,
    pub command: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProcessExit {
    Code(i32),
    Signal(i32),
}

#[derive(Debug)]
pub enum SuperviseError {
    Invalid(&'static str),
    Spawn(io::Error),
    Wait(io::Error),
}

impl fmt::Display for SuperviseError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Invalid(message) => formatter.write_str(message),
            Self::Spawn(error) => write!(formatter, "cannot start supervised process: {error}"),
            Self::Wait(error) => write!(formatter, "cannot wait for supervised process: {error}"),
        }
    }
}

impl Error for SuperviseError {}

trait SupervisedChild {
    fn id(&self) -> u32;
    fn wait(&mut self) -> io::Result<ProcessExit>;
}

impl SupervisedChild for Child {
    fn id(&self) -> u32 {
        Child::id(self)
    }

    fn wait(&mut self) -> io::Result<ProcessExit> {
        let status = Child::wait(self)?;
        if let Some(code) = status.code() {
            Ok(ProcessExit::Code(code))
        } else if let Some(signal) = status.signal() {
            Ok(ProcessExit::Signal(signal))
        } else {
            Err(io::Error::other("child exited without a code or signal"))
        }
    }
}

trait Runner {
    fn spawn(&mut self, argv: &[String]) -> io::Result<Box<dyn SupervisedChild>>;
}

struct CommandRunner;

impl Runner for CommandRunner {
    fn spawn(&mut self, argv: &[String]) -> io::Result<Box<dyn SupervisedChild>> {
        Command::new(&argv[0])
            .args(&argv[1..])
            .stdin(Stdio::inherit())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .spawn()
            .map(|child| Box::new(child) as Box<dyn SupervisedChild>)
    }
}

trait Reporter {
    fn ensure(
        &mut self,
        shell_id: &str,
        run_id: &str,
        spec: AgentRegistrationSpec,
    ) -> io::Result<AgentInstanceSnapshot>;
    fn report(
        &mut self,
        agent_id: &str,
        run_id: &str,
        report: AgentReport,
    ) -> io::Result<AgentInstanceSnapshot>;
}

#[derive(Default)]
struct BoomuxReporter {
    client: Option<client::Client>,
}

impl BoomuxReporter {
    fn client(&mut self) -> io::Result<&client::Client> {
        if self.client.is_none() {
            self.client = Some(client::connect_or_start()?);
        }
        Ok(self.client.as_ref().expect("client was initialized"))
    }
}

impl Reporter for BoomuxReporter {
    fn ensure(
        &mut self,
        shell_id: &str,
        run_id: &str,
        spec: AgentRegistrationSpec,
    ) -> io::Result<AgentInstanceSnapshot> {
        self.client()?.ensure_agent(shell_id, run_id, spec)
    }

    fn report(
        &mut self,
        agent_id: &str,
        run_id: &str,
        report: AgentReport,
    ) -> io::Result<AgentInstanceSnapshot> {
        self.client()?.report_agent(agent_id, run_id, report)
    }
}

trait Warnings {
    fn warn(&mut self, message: &str);
}

#[derive(Default)]
struct StderrWarnings {
    emitted: bool,
}

impl Warnings for StderrWarnings {
    fn warn(&mut self, message: &str) {
        if !self.emitted {
            eprintln!("boomux: warning: {message}");
            self.emitted = true;
        }
    }
}

pub fn supervise(spec: SuperviseSpec) -> Result<ProcessExit, SuperviseError> {
    supervise_with(
        spec,
        &mut CommandRunner,
        &mut BoomuxReporter::default(),
        &mut StderrWarnings::default(),
    )
}

fn supervise_with(
    spec: SuperviseSpec,
    runner: &mut dyn Runner,
    reporter: &mut dyn Reporter,
    warnings: &mut dyn Warnings,
) -> Result<ProcessExit, SuperviseError> {
    validate(&spec)?;
    let mut child = runner.spawn(&spec.command).map_err(SuperviseError::Spawn)?;
    let pid = child.id();
    let start_report = observation(format!("supervised process {pid} started"));
    let registration = AgentRegistrationSpec {
        name: spec.name,
        integration: spec.integration,
        external_session_id: Some(spec.external_session_id),
        report: start_report.clone(),
    };

    let agent = match reporter.ensure(&spec.shell_id, &spec.run_id, registration) {
        Ok(agent) => Some(agent),
        Err(error) => {
            warnings.warn(&format!("agent ensure failed: {error}"));
            None
        }
    };
    let agent_id = agent.as_ref().and_then(|agent| {
        if agent.ended_at_ms.is_some() {
            None
        } else {
            if let Err(error) = reporter.report(&agent.id, &spec.run_id, start_report) {
                warnings.warn(&format!("agent start report failed: {error}"));
            }
            Some(agent.id.clone())
        }
    });

    let exit = child.wait().map_err(SuperviseError::Wait)?;
    if let Some(agent_id) = agent_id {
        let evidence = match exit {
            ProcessExit::Code(code) => format!("supervised process {pid} exited with code {code}"),
            ProcessExit::Signal(signal) => {
                format!("supervised process {pid} exited from signal {signal}")
            }
        };
        if let Err(error) = reporter.report(&agent_id, &spec.run_id, observation(evidence)) {
            warnings.warn(&format!("agent exit report failed: {error}"));
        }
    }
    Ok(exit)
}

fn validate(spec: &SuperviseSpec) -> Result<(), SuperviseError> {
    for (value, message) in [
        (&spec.name, "agent name cannot be empty"),
        (&spec.integration, "agent integration cannot be empty"),
        (
            &spec.external_session_id,
            "agent external session ID cannot be empty",
        ),
        (&spec.shell_id, "agent shell ID cannot be empty"),
        (&spec.run_id, "agent run ID cannot be empty"),
    ] {
        if value.trim().is_empty() {
            return Err(SuperviseError::Invalid(message));
        }
    }
    if spec.command.is_empty() || spec.command[0].is_empty() {
        return Err(SuperviseError::Invalid(
            "supervised command cannot be empty",
        ));
    }
    Ok(())
}

fn observation(evidence: String) -> AgentReport {
    AgentReport {
        state: AgentState::Unknown,
        authority: AgentAuthority::ProcessAdapter,
        evidence,
        confidence: CONFIDENCE,
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::rc::Rc;

    use boomux::protocol::AgentObservationSnapshot;

    use super::*;

    struct FakeChild {
        exit: ProcessExit,
    }

    impl SupervisedChild for FakeChild {
        fn id(&self) -> u32 {
            42
        }

        fn wait(&mut self) -> io::Result<ProcessExit> {
            Ok(self.exit)
        }
    }

    #[derive(Default)]
    struct FakeRunner {
        argv: Rc<RefCell<Vec<String>>>,
    }

    impl Runner for FakeRunner {
        fn spawn(&mut self, argv: &[String]) -> io::Result<Box<dyn SupervisedChild>> {
            *self.argv.borrow_mut() = argv.to_vec();
            Ok(Box::new(FakeChild {
                exit: ProcessExit::Code(23),
            }))
        }
    }

    struct FakeReporter {
        ensured: AgentInstanceSnapshot,
        reports: Vec<AgentReport>,
        fail_ensure: bool,
        fail_reports: bool,
    }

    impl Reporter for FakeReporter {
        fn ensure(
            &mut self,
            _shell_id: &str,
            _run_id: &str,
            spec: AgentRegistrationSpec,
        ) -> io::Result<AgentInstanceSnapshot> {
            assert_eq!(spec.external_session_id.as_deref(), Some("session-1"));
            assert_report(&spec.report);
            if self.fail_ensure {
                Err(io::Error::other("offline"))
            } else {
                Ok(self.ensured.clone())
            }
        }

        fn report(
            &mut self,
            _agent_id: &str,
            _run_id: &str,
            report: AgentReport,
        ) -> io::Result<AgentInstanceSnapshot> {
            self.reports.push(report);
            if self.fail_reports {
                Err(io::Error::other("offline"))
            } else {
                Ok(self.ensured.clone())
            }
        }
    }

    #[derive(Default)]
    struct FakeWarnings(Vec<String>);

    impl Warnings for FakeWarnings {
        fn warn(&mut self, message: &str) {
            self.0.push(message.into());
        }
    }

    fn spec(command: &[&str]) -> SuperviseSpec {
        SuperviseSpec {
            name: "agent".into(),
            integration: "test".into(),
            external_session_id: "session-1".into(),
            shell_id: "s1".into(),
            run_id: "r1".into(),
            command: command.iter().map(|value| (*value).into()).collect(),
        }
    }

    fn snapshot(authority: AgentAuthority, completed: bool) -> AgentInstanceSnapshot {
        AgentInstanceSnapshot {
            id: "a1".into(),
            workspace_id: "w1".into(),
            shell_id: "s1".into(),
            run_id: "r1".into(),
            name: "agent".into(),
            integration: "test".into(),
            external_session_id: Some("session-1".into()),
            started_at_ms: 1,
            ended_at_ms: completed.then_some(2),
            observation: AgentObservationSnapshot {
                revision: 1,
                state: AgentState::Working,
                authority,
                evidence: "existing observation".into(),
                confidence: 100,
                observed_at_ms: 1,
            },
        }
    }

    fn assert_report(report: &AgentReport) {
        assert_eq!(report.state, AgentState::Unknown);
        assert_eq!(report.authority, AgentAuthority::ProcessAdapter);
        assert_eq!(report.confidence, 100);
        assert!(report.evidence.len() < 128);
    }

    #[test]
    fn preserves_exact_argv_and_propagates_child_exit() {
        let mut runner = FakeRunner::default();
        let argv = Rc::clone(&runner.argv);
        let mut reporter = FakeReporter {
            ensured: snapshot(AgentAuthority::ProcessAdapter, false),
            reports: Vec::new(),
            fail_ensure: false,
            fail_reports: false,
        };

        let exit = supervise_with(
            spec(&["printf", "%s; exit 9", "literal value"]),
            &mut runner,
            &mut reporter,
            &mut FakeWarnings::default(),
        )
        .unwrap();

        assert_eq!(exit, ProcessExit::Code(23));
        assert_eq!(&*argv.borrow(), &["printf", "%s; exit 9", "literal value"]);
    }

    #[test]
    fn sends_start_and_exit_unknown_observations() {
        let mut reporter = FakeReporter {
            ensured: snapshot(AgentAuthority::ProcessAdapter, false),
            reports: Vec::new(),
            fail_ensure: false,
            fail_reports: false,
        };
        supervise_with(
            spec(&["agent"]),
            &mut FakeRunner::default(),
            &mut reporter,
            &mut FakeWarnings::default(),
        )
        .unwrap();

        assert_eq!(reporter.reports.len(), 2);
        reporter.reports.iter().for_each(assert_report);
        assert!(reporter.reports[0].evidence.contains("started"));
        assert!(reporter.reports[1].evidence.contains("code 23"));
    }

    #[test]
    fn unchanged_lifecycle_snapshot_does_not_stop_exit_reporting() {
        let mut reporter = FakeReporter {
            ensured: snapshot(AgentAuthority::LifecycleIntegration, false),
            reports: Vec::new(),
            fail_ensure: false,
            fail_reports: false,
        };
        supervise_with(
            spec(&["agent"]),
            &mut FakeRunner::default(),
            &mut reporter,
            &mut FakeWarnings::default(),
        )
        .unwrap();

        assert_eq!(reporter.reports.len(), 2);
        assert_eq!(
            reporter.ensured.observation.evidence,
            "existing observation"
        );
    }

    #[test]
    fn completed_agent_skips_start_and_exit_reports() {
        let mut reporter = FakeReporter {
            ensured: snapshot(AgentAuthority::DaemonLifecycle, true),
            reports: Vec::new(),
            fail_ensure: false,
            fail_reports: false,
        };
        supervise_with(
            spec(&["agent"]),
            &mut FakeRunner::default(),
            &mut reporter,
            &mut FakeWarnings::default(),
        )
        .unwrap();

        assert!(reporter.reports.is_empty());
    }

    #[test]
    fn reporting_failure_is_fail_open() {
        let mut reporter = FakeReporter {
            ensured: snapshot(AgentAuthority::ProcessAdapter, false),
            reports: Vec::new(),
            fail_ensure: false,
            fail_reports: true,
        };
        let mut warnings = FakeWarnings::default();

        let exit = supervise_with(
            spec(&["agent"]),
            &mut FakeRunner::default(),
            &mut reporter,
            &mut warnings,
        )
        .unwrap();

        assert_eq!(exit, ProcessExit::Code(23));
        assert_eq!(reporter.reports.len(), 2);
        assert_eq!(warnings.0.len(), 2);
    }

    #[test]
    fn ensure_failure_is_fail_open() {
        let mut reporter = FakeReporter {
            ensured: snapshot(AgentAuthority::ProcessAdapter, false),
            reports: Vec::new(),
            fail_ensure: true,
            fail_reports: false,
        };
        let mut warnings = FakeWarnings::default();

        let exit = supervise_with(
            spec(&["agent"]),
            &mut FakeRunner::default(),
            &mut reporter,
            &mut warnings,
        )
        .unwrap();

        assert_eq!(exit, ProcessExit::Code(23));
        assert!(reporter.reports.is_empty());
        assert_eq!(warnings.0.len(), 1);
    }

    #[test]
    fn command_runner_does_not_interpret_shell_metacharacters() {
        let mut child = CommandRunner
            .spawn(&["/usr/bin/test".into(), "literal;false".into()])
            .unwrap();

        assert_eq!(child.wait().unwrap(), ProcessExit::Code(0));
    }

    #[test]
    fn validates_identity_and_argv_before_spawn() {
        let mut invalid = spec(&[]);
        invalid.integration = " ".into();
        let runner = &mut FakeRunner::default();
        let argv = Rc::clone(&runner.argv);
        let result = supervise_with(
            invalid,
            runner,
            &mut FakeReporter {
                ensured: snapshot(AgentAuthority::ProcessAdapter, false),
                reports: Vec::new(),
                fail_ensure: false,
                fail_reports: false,
            },
            &mut FakeWarnings::default(),
        );
        assert!(matches!(result, Err(SuperviseError::Invalid(_))));
        assert!(argv.borrow().is_empty());
    }
}

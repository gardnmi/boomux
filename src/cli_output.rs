use std::error::Error;
use std::io;

use serde::Serialize;

use boomux::client::RemoteError;
use boomux::protocol::{
    ErrorCode, ShellRunExitReason, ShellSnapshot, ShellStatus, WorkspaceLauncherSnapshot,
};

pub(crate) const SCHEMA: &str = "boomux.cli/v1";

#[derive(Serialize)]
struct Envelope<'a, T> {
    schema: &'static str,
    command: &'a str,
    data: T,
}

#[derive(Serialize)]
struct ErrorEnvelope<'a> {
    schema: &'static str,
    command: &'a str,
    error: ErrorData<'a>,
}

#[derive(Serialize)]
struct ErrorData<'a> {
    code: &'a str,
    message: String,
}

#[derive(Debug)]
pub(crate) struct CliError {
    code: &'static str,
    message: String,
}

impl std::fmt::Display for CliError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl Error for CliError {}

#[derive(Serialize)]
pub(crate) struct ShellData {
    pub(crate) id: String,
    pub(crate) workspace_id: String,
    pub(crate) workspace_name: Option<String>,
    pub(crate) name: String,
    pub(crate) cwd: String,
    pub(crate) status: &'static str,
    pub(crate) exit_code: Option<u32>,
    pub(crate) run: Option<RunData>,
}

#[derive(Serialize)]
pub(crate) struct RunData {
    id: String,
    generation: u64,
    started_at_ms: u64,
    ended_at_ms: Option<u64>,
    exit_reason: Option<&'static str>,
    exit_code: Option<u32>,
    output_revision: u64,
    environment_has_run_id: bool,
}

#[derive(Serialize)]
pub(crate) struct WorkspaceSummary {
    pub(crate) id: String,
    pub(crate) name: String,
    pub(crate) shell_count: usize,
    pub(crate) launcher_count: usize,
}

#[derive(Serialize)]
pub(crate) struct LauncherData {
    pub(crate) id: String,
    pub(crate) workspace_id: String,
    pub(crate) workspace_name: Option<String>,
    pub(crate) name: String,
    pub(crate) cwd: String,
    pub(crate) command: Vec<String>,
}

pub(crate) fn print<T: Serialize>(command: &str, data: T) -> Result<(), Box<dyn Error>> {
    println!(
        "{}",
        serde_json::to_string_pretty(&Envelope {
            schema: SCHEMA,
            command,
            data,
        })?
    );
    Ok(())
}

pub(crate) fn print_error(command: &str, error: &(dyn Error + 'static)) {
    print_error_message(command, classify_error(command, error), error.to_string());
}

pub(crate) fn print_error_message(command: &str, code: &str, message: impl Into<String>) {
    let envelope = ErrorEnvelope {
        schema: SCHEMA,
        command,
        error: ErrorData {
            code,
            message: message.into(),
        },
    };
    match serde_json::to_string_pretty(&envelope) {
        Ok(json) => eprintln!("{json}"),
        Err(_) => eprintln!(
            "{{\"schema\":\"boomux.cli/v1\",\"command\":\"unknown\",\"error\":{{\"code\":\"internal\",\"message\":\"could not serialize error\"}}}}"
        ),
    }
}

pub(crate) fn failure(code: &'static str, message: impl Into<String>) -> Box<dyn Error> {
    Box::new(CliError {
        code,
        message: message.into(),
    })
}

pub(crate) fn shell(shell: &ShellSnapshot, workspace_name: Option<&str>) -> ShellData {
    let (status, exit_code) = match shell.status {
        ShellStatus::Pending => ("pending", None),
        ShellStatus::Running => ("running", None),
        ShellStatus::Exited { code } => ("exited", code),
    };
    ShellData {
        id: shell.id.clone(),
        workspace_id: shell.workspace_id.clone(),
        workspace_name: workspace_name.map(str::to_owned),
        name: shell.name.clone(),
        cwd: shell.cwd.display().to_string(),
        status,
        exit_code,
        run: shell.run.as_ref().map(|run| {
            let (exit_reason, exit_code) = match run.exit_reason.as_ref() {
                Some(ShellRunExitReason::Exited { code }) => (Some("exited"), *code),
                Some(ShellRunExitReason::Terminated) => (Some("terminated"), None),
                Some(ShellRunExitReason::Interrupted) => (Some("interrupted"), None),
                None => (None, None),
            };
            RunData {
                id: run.id.clone(),
                generation: run.generation,
                started_at_ms: run.started_at_ms,
                ended_at_ms: run.ended_at_ms,
                exit_reason,
                exit_code,
                output_revision: run.output_revision,
                environment_has_run_id: run.environment_has_run_id,
            }
        }),
    }
}

pub(crate) fn launcher(
    launcher: &WorkspaceLauncherSnapshot,
    workspace_name: Option<&str>,
) -> LauncherData {
    LauncherData {
        id: launcher.id.clone(),
        workspace_id: launcher.workspace_id.clone(),
        workspace_name: workspace_name.map(str::to_owned),
        name: launcher.name.clone(),
        cwd: launcher.cwd.display().to_string(),
        command: launcher.command.clone(),
    }
}

fn classify_error(command: &str, error: &(dyn Error + 'static)) -> &'static str {
    let mut current = Some(error);
    while let Some(candidate) = current {
        if let Some(cli) = candidate.downcast_ref::<CliError>() {
            return cli.code;
        }
        if let Some(remote) = candidate.downcast_ref::<RemoteError>() {
            return remote.code.map_or("unknown", protocol_error_code);
        }
        if let Some(io_error) = candidate.downcast_ref::<io::Error>() {
            if let Some(remote) = io_error
                .get_ref()
                .and_then(|inner| inner.downcast_ref::<RemoteError>())
            {
                return remote.code.map_or("unknown", protocol_error_code);
            }
            if command == "daemon.status"
                && matches!(
                    io_error.kind(),
                    io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
                )
            {
                return "daemon_unavailable";
            }
            return match io_error.kind() {
                io::ErrorKind::InvalidInput | io::ErrorKind::InvalidData => "invalid_argument",
                io::ErrorKind::NotFound => "not_found",
                io::ErrorKind::AlreadyExists => "already_exists",
                io::ErrorKind::WouldBlock | io::ErrorKind::AddrInUse => "busy",
                io::ErrorKind::ConnectionAborted => "daemon_stopping",
                io::ErrorKind::ConnectionRefused => "daemon_unavailable",
                io::ErrorKind::TimedOut => "timeout",
                _ => "internal",
            };
        }
        current = candidate.source();
    }
    "internal"
}

fn protocol_error_code(code: ErrorCode) -> &'static str {
    match code {
        ErrorCode::InvalidArgument => "invalid_argument",
        ErrorCode::NotFound => "not_found",
        ErrorCode::AlreadyExists => "already_exists",
        ErrorCode::Busy => "busy",
        ErrorCode::DaemonStopping => "daemon_stopping",
        ErrorCode::ShellStartFailed => "shell_start_failed",
        ErrorCode::PersistenceFailed => "persistence_failed",
        ErrorCode::Timeout => "timeout",
        ErrorCode::UnsupportedVersion => "unsupported_version",
        ErrorCode::CursorExpired => "cursor_expired",
        ErrorCode::RunChanged => "run_changed",
        ErrorCode::RevisionAhead => "revision_ahead",
        ErrorCode::Internal => "internal",
        ErrorCode::Unknown => "unknown",
    }
}

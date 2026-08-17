use std::env;
use std::io::{self, Read, Write};
use std::os::fd::AsRawFd;
use std::thread;
use std::time::{Duration, Instant};

use crossterm::terminal;

use crate::client;
use crate::protocol::{AttachFrame, ProtocolFeature, TerminalProfile};
use crate::terminal_focus::FocusMode;

const POLL_INTERVAL_MS: i32 = 100;
const ESCAPE_DISAMBIGUATION_MS: i32 = 10;
const RECONNECT_ATTEMPTS: usize = 600;
const RECONNECT_DELAY: Duration = Duration::from_millis(25);
const ENABLE_FOCUS_REPORTING: &[u8] = b"\x1b[?1004h";
const DISABLE_FOCUS_REPORTING: &[u8] = b"\x1b[?1004l";

struct RawMode;

const FOCUS_GAINED: &[u8] = b"\x1b[I";
const FOCUS_LOST: &[u8] = b"\x1b[O";

#[derive(Default)]
struct FocusTracking {
    enabled: bool,
    child_mode: FocusMode,
    input: FocusInput,
}

#[derive(Default)]
struct FocusInput {
    pending: Vec<u8>,
    pending_since: Option<Instant>,
}

impl FocusInput {
    fn process(&mut self, bytes: &[u8], forward_focus: bool) -> (Vec<u8>, usize) {
        let mut combined = std::mem::take(&mut self.pending);
        self.pending_since = None;
        combined.extend_from_slice(bytes);
        let mut forwarded = Vec::with_capacity(combined.len());
        let mut gained = 0;
        let mut offset = 0;
        while offset < combined.len() {
            let remaining = &combined[offset..];
            if remaining.starts_with(FOCUS_GAINED) {
                gained += 1;
                if forward_focus {
                    forwarded.extend_from_slice(FOCUS_GAINED);
                }
                offset += FOCUS_GAINED.len();
            } else if remaining.starts_with(FOCUS_LOST) {
                if forward_focus {
                    forwarded.extend_from_slice(FOCUS_LOST);
                }
                offset += FOCUS_LOST.len();
            } else if remaining.len() < FOCUS_GAINED.len()
                && (FOCUS_GAINED.starts_with(remaining) || FOCUS_LOST.starts_with(remaining))
            {
                self.pending.extend_from_slice(remaining);
                self.pending_since = Some(Instant::now());
                break;
            } else {
                forwarded.push(combined[offset]);
                offset += 1;
            }
        }
        (forwarded, gained)
    }

    fn flush_pending(&mut self) -> Vec<u8> {
        self.pending_since = None;
        std::mem::take(&mut self.pending)
    }

    fn poll_timeout(&self) -> i32 {
        let Some(since) = self.pending_since else {
            return POLL_INTERVAL_MS;
        };
        let remaining =
            Duration::from_millis(ESCAPE_DISAMBIGUATION_MS as u64).saturating_sub(since.elapsed());
        remaining.as_millis().min(POLL_INTERVAL_MS as u128) as i32
    }

    fn pending_expired(&self) -> bool {
        self.pending_since.is_some_and(|since| {
            since.elapsed() >= Duration::from_millis(ESCAPE_DISAMBIGUATION_MS as u64)
        })
    }
}

impl FocusTracking {
    fn reset(&mut self, enabled: bool) {
        self.enabled = enabled;
        self.child_mode = FocusMode::default();
        self.input.flush_pending();
    }
}

impl RawMode {
    fn enter() -> io::Result<Self> {
        terminal::enable_raw_mode()?;
        Ok(Self)
    }
}

impl Drop for RawMode {
    fn drop(&mut self) {
        let _ = terminal::disable_raw_mode();
    }
}

pub fn run(
    shell_id: &str,
    node_id: Option<&str>,
    takeover: bool,
    restart_exited: bool,
    expected_run_id: Option<&str>,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut profile = terminal_profile()?;
    let mut size = (
        profile.rows,
        profile.cols,
        profile.pixel_width,
        profile.pixel_height,
    );
    let client = client::connect_or_start()?;
    let mut attachment = attach_once(
        &client,
        shell_id,
        node_id,
        takeover,
        restart_exited,
        expected_run_id,
        &profile,
    )?;
    let _raw_mode = RawMode::enter()?;
    let mut stdin = io::stdin().lock();
    let mut stdout = io::stdout().lock();
    let mut input = [0; 16 * 1024];
    let mut focus_reporting = false;
    let mut focus = FocusTracking::default();

    let result: Result<(), Box<dyn std::error::Error>> = (|| {
        loop {
            let report_focus =
                ProtocolFeature::FocusedTerminal.is_supported_by(attachment.protocol_version);
            if report_focus != focus_reporting {
                stdout.write_all(if report_focus {
                    ENABLE_FOCUS_REPORTING
                } else {
                    DISABLE_FOCUS_REPORTING
                })?;
                stdout.flush()?;
                focus_reporting = report_focus;
            }
            if let Some(warning) = attachment.warning.take() {
                eprintln!("boomux: warning: {warning}");
            }
            focus.reset(report_focus);
            stdout.write_all(&attachment.reconstruction)?;
            if focus.enabled && focus.child_mode.process(&attachment.reconstruction) {
                stdout.write_all(ENABLE_FOCUS_REPORTING)?;
            }
            stdout.flush()?;
            let mut stream = attachment.stream;

            if !pump_attachment(
                &mut stream,
                &mut stdin,
                &mut stdout,
                &mut input,
                &mut size,
                &mut focus,
            )? {
                return Ok(());
            }
            if let Ok((rows, cols, pixel_width, pixel_height)) = dimensions() {
                size = (rows, cols, pixel_width, pixel_height);
                profile.rows = rows;
                profile.cols = cols;
                profile.pixel_width = pixel_width;
                profile.pixel_height = pixel_height;
            }
            attachment = reconnect(
                &client,
                shell_id,
                node_id,
                takeover,
                expected_run_id,
                &profile,
            )?;
        }
    })();
    if focus_reporting {
        let disable_result = stdout
            .write_all(DISABLE_FOCUS_REPORTING)
            .and_then(|()| stdout.flush());
        if result.is_ok() {
            disable_result?;
        }
    }
    result
}

pub fn run_agent_session(
    session_id: &str,
    node_id: Option<&str>,
) -> Result<(), Box<dyn std::error::Error>> {
    let profile = terminal_profile()?;
    let mut size = (
        profile.rows,
        profile.cols,
        profile.pixel_width,
        profile.pixel_height,
    );
    let client = client::connect_or_start()?;
    let attachment = client.resume_agent_session(node_id, session_id, profile)?;
    let _raw_mode = RawMode::enter()?;
    let mut stdin = io::stdin().lock();
    let mut stdout = io::stdout().lock();
    let mut input = [0; 16 * 1024];
    let mut focus = FocusTracking::default();
    stdout.write_all(&attachment.reconstruction)?;
    stdout.flush()?;
    let mut stream = attachment.stream;
    let _ = pump_attachment(
        &mut stream,
        &mut stdin,
        &mut stdout,
        &mut input,
        &mut size,
        &mut focus,
    )?;
    Ok(())
}

fn reconnect(
    client: &client::Client,
    shell_id: &str,
    node_id: Option<&str>,
    takeover: bool,
    expected_run_id: Option<&str>,
    profile: &TerminalProfile,
) -> client::Result<client::Attachment> {
    let mut last_error = None;
    for _ in 0..RECONNECT_ATTEMPTS {
        match attach_once(
            client,
            shell_id,
            node_id,
            takeover,
            false,
            expected_run_id,
            profile,
        ) {
            Ok(attachment) => return Ok(attachment),
            Err(error) if exact_reconnect_error_is_permanent(expected_run_id, &error) => {
                return Err(error);
            }
            Err(error) => last_error = Some(error),
        }
        thread::sleep(RECONNECT_DELAY);
    }
    Err(client::ClientError::Lifecycle(
        client::LifecycleError::AttachmentReconnectTimeout(last_error.map(Box::new)),
    ))
}

fn exact_reconnect_error_is_permanent(
    expected_run_id: Option<&str>,
    error: &client::ClientError,
) -> bool {
    expected_run_id.is_some()
        && matches!(
            error,
            client::ClientError::Remote(client::RemoteError {
                code: Some(crate::protocol::ErrorCode::RunChanged),
                ..
            })
        )
}

fn attach_once(
    client: &client::Client,
    shell_id: &str,
    node_id: Option<&str>,
    takeover: bool,
    restart_exited: bool,
    expected_run_id: Option<&str>,
    profile: &TerminalProfile,
) -> client::Result<client::Attachment> {
    if let Some(node_id) = node_id {
        return client.attach_node(
            crate::protocol::QualifiedIdentity::new(node_id, shell_id),
            takeover,
            restart_exited,
            expected_run_id.map(str::to_owned),
            profile.clone(),
        );
    }
    if let Some(expected_run_id) = expected_run_id {
        return client.attach_exact_run_with_client_environment(
            shell_id,
            expected_run_id,
            takeover,
            profile.clone(),
        );
    }
    let transfers_environment = client.supports(ProtocolFeature::ClientEnvironment)?;
    match (restart_exited, transfers_environment) {
        (true, true) => {
            client.attach_restarting_with_client_environment(shell_id, takeover, profile.clone())
        }
        (true, false) => client.attach_restarting(shell_id, takeover, profile.clone()),
        (false, true) => client.attach_with_client_environment(shell_id, takeover, profile.clone()),
        (false, false) => client.attach(shell_id, takeover, profile.clone()),
    }
}

fn pump_attachment(
    stream: &mut std::os::unix::net::UnixStream,
    stdin: &mut (impl Read + AsRawFd),
    stdout: &mut impl Write,
    input: &mut [u8],
    size: &mut (u16, u16, u16, u16),
    focus: &mut FocusTracking,
) -> io::Result<bool> {
    loop {
        let mut descriptors = [
            libc::pollfd {
                fd: stdin.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: stream.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        // Both descriptors remain valid for the duration of this call.
        let timeout = if focus.enabled {
            focus.input.poll_timeout()
        } else {
            POLL_INTERVAL_MS
        };
        let ready = unsafe {
            libc::poll(
                descriptors.as_mut_ptr(),
                descriptors.len() as libc::nfds_t,
                timeout,
            )
        };
        if ready < 0 {
            let error = io::Error::last_os_error();
            if error.kind() != io::ErrorKind::Interrupted {
                return Err(error);
            }
        }

        if descriptors[1].revents & (libc::POLLIN | libc::POLLHUP | libc::POLLERR) != 0 {
            match AttachFrame::read_from(stream) {
                Ok(AttachFrame::Output(bytes)) => {
                    stdout.write_all(&bytes)?;
                    if focus.enabled && focus.child_mode.process(&bytes) {
                        stdout.write_all(ENABLE_FOCUS_REPORTING)?;
                    }
                    stdout.flush()?;
                }
                Ok(AttachFrame::Detached) => return Ok(false),
                Ok(AttachFrame::Reconnect) => {
                    let pending = focus.input.flush_pending();
                    if !pending.is_empty() {
                        AttachFrame::Input(pending).write_to(stream)?;
                    }
                    let _ = AttachFrame::ReconnectAck.write_to(stream);
                    return Ok(true);
                }
                Ok(_) => {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "daemon sent an invalid attach frame",
                    ));
                }
                Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => return Ok(false),
                Err(error) => return Err(error),
            }
        }
        if descriptors[0].revents & libc::POLLIN != 0 {
            let count = stdin.read(input)?;
            if count == 0 {
                let pending = focus.input.flush_pending();
                if !pending.is_empty() {
                    AttachFrame::Input(pending).write_to(stream)?;
                }
                AttachFrame::Detached.write_to(stream)?;
                return Ok(false);
            }
            if focus.enabled {
                let (forwarded, gained) = focus
                    .input
                    .process(&input[..count], focus.child_mode.enabled());
                if !forwarded.is_empty() {
                    AttachFrame::Input(forwarded).write_to(stream)?;
                }
                for _ in 0..gained {
                    AttachFrame::FocusGained.write_to(stream)?;
                }
            } else {
                AttachFrame::Input(input[..count].to_vec()).write_to(stream)?;
            }
        }
        if focus.input.pending_expired() {
            let pending = focus.input.flush_pending();
            if !pending.is_empty() {
                AttachFrame::Input(pending).write_to(stream)?;
            }
        }

        if let Ok(new_size) = dimensions()
            && new_size != *size
        {
            *size = new_size;
            AttachFrame::Resize {
                rows: size.0,
                cols: size.1,
                pixel_width: size.2,
                pixel_height: size.3,
            }
            .write_to(stream)?;
        }
    }
}

fn terminal_profile() -> io::Result<TerminalProfile> {
    let (rows, cols, pixel_width, pixel_height) = dimensions()?;
    Ok(TerminalProfile {
        term: terminal_variable("TERM"),
        colorterm: terminal_variable("COLORTERM"),
        term_program: terminal_variable("TERM_PROGRAM"),
        term_program_version: terminal_variable("TERM_PROGRAM_VERSION"),
        rows,
        cols,
        pixel_width,
        pixel_height,
    })
}

fn terminal_variable(name: &str) -> Option<String> {
    env::var(name).ok().filter(|value| !value.is_empty())
}

fn dimensions() -> io::Result<(u16, u16, u16, u16)> {
    let mut size = libc::winsize {
        ws_row: 0,
        ws_col: 0,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    // stdout remains open while the attachment is active and ioctl only writes `size`.
    if unsafe { libc::ioctl(io::stdout().as_raw_fd(), libc::TIOCGWINSZ, &mut size) } == -1 {
        return Err(io::Error::last_os_error());
    }
    if size.ws_row == 0 || size.ws_col == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "terminal reported zero rows or columns",
        ));
    }
    Ok((size.ws_row, size.ws_col, size.ws_xpixel, size.ws_ypixel))
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::net::{UnixListener, UnixStream};

    use uuid::Uuid;

    use super::*;

    #[test]
    fn exact_reconnect_returns_run_changed_without_retrying() {
        let directory = std::env::temp_dir().join(format!("boomux-reconnect-{}", Uuid::new_v4()));
        fs::create_dir_all(&directory).unwrap();
        let socket = directory.join("daemon.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let request: crate::protocol::Envelope<crate::protocol::Request> =
                crate::protocol::read_message(&mut stream).unwrap();
            assert!(matches!(
                request.message,
                crate::protocol::Request::Attach {
                    expected_run_id: Some(ref run_id),
                    ..
                } if run_id == "run-1"
            ));
            crate::protocol::write_message(
                &mut stream,
                &crate::protocol::Envelope::with_version(
                    request.version,
                    crate::protocol::Response::Error {
                        message: "run changed".into(),
                        code: Some(crate::protocol::ErrorCode::RunChanged),
                    },
                ),
            )
            .unwrap();
        });
        let client = client::Client::from_socket_path(socket);
        let error = reconnect(
            &client,
            "shell-1",
            None,
            true,
            Some("run-1"),
            &TerminalProfile {
                term: Some("xterm-256color".into()),
                colorterm: None,
                term_program: None,
                term_program_version: None,
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            },
        )
        .unwrap_err();
        assert!(matches!(
            error,
            client::ClientError::Remote(client::RemoteError {
                code: Some(crate::protocol::ErrorCode::RunChanged),
                ..
            })
        ));
        server.join().unwrap();
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn reconnect_frame_finishes_current_pump_after_flushing_output() {
        let (mut daemon, mut attachment) = UnixStream::pair().unwrap();
        let (mut fake_stdin, mut stdin_writer) = UnixStream::pair().unwrap();
        let sender = thread::spawn(move || {
            AttachFrame::Output(b"before-reconnect".to_vec())
                .write_to(&mut daemon)
                .unwrap();
            AttachFrame::Reconnect.write_to(&mut daemon).unwrap();
            assert_eq!(
                AttachFrame::read_from(&mut daemon).unwrap(),
                AttachFrame::Input(b"queued-input".to_vec())
            );
            assert_eq!(
                AttachFrame::read_from(&mut daemon).unwrap(),
                AttachFrame::ReconnectAck
            );
        });
        stdin_writer.write_all(b"queued-input").unwrap();
        let mut stdout = Vec::new();
        let mut input = [0; 128];
        let mut size = (24, 80, 0, 0);

        let reconnect = pump_attachment(
            &mut attachment,
            &mut fake_stdin,
            &mut stdout,
            &mut input,
            &mut size,
            &mut FocusTracking::default(),
        )
        .unwrap();

        sender.join().unwrap();
        assert!(reconnect);
        assert_eq!(stdout, b"before-reconnect");
    }

    #[test]
    fn focus_input_detects_split_reports_without_forwarding_to_an_unsubscribed_child() {
        let mut input = FocusInput::default();

        assert_eq!(input.process(b"text\x1b[", false), (b"text".to_vec(), 0));
        assert_eq!(input.process(b"I\x1b[I", false), (Vec::new(), 2));
        assert_eq!(input.process(b"\x1b[x", false), (b"\x1b[x".to_vec(), 0));
    }

    #[test]
    fn protocol_eighteen_forwards_input_before_focus_frames() {
        let (mut daemon, mut attachment) = UnixStream::pair().unwrap();
        let (mut fake_stdin, mut stdin_writer) = UnixStream::pair().unwrap();
        let sender = thread::spawn(move || {
            assert_eq!(
                AttachFrame::read_from(&mut daemon).unwrap(),
                AttachFrame::Input(b"a".to_vec())
            );
            assert_eq!(
                AttachFrame::read_from(&mut daemon).unwrap(),
                AttachFrame::FocusGained
            );
            assert_eq!(
                AttachFrame::read_from(&mut daemon).unwrap(),
                AttachFrame::FocusGained
            );
            AttachFrame::Detached.write_to(&mut daemon).unwrap();
        });
        stdin_writer.write_all(b"a\x1b[I\x1b[I").unwrap();
        let mut stdout = Vec::new();
        let mut input = [0; 128];
        let mut size = (24, 80, 0, 0);

        let reconnect = pump_attachment(
            &mut attachment,
            &mut fake_stdin,
            &mut stdout,
            &mut input,
            &mut size,
            &mut FocusTracking {
                enabled: true,
                ..Default::default()
            },
        )
        .unwrap();

        sender.join().unwrap();
        assert!(!reconnect);
    }

    #[test]
    fn focus_input_is_forwarded_when_the_child_subscribed() {
        let mut input = FocusInput::default();

        assert_eq!(
            input.process(b"before\x1b[I\x1b[Oafter", true),
            (b"before\x1b[I\x1b[Oafter".to_vec(), 1)
        );
    }

    #[test]
    fn attachment_reenables_focus_after_child_disables_it() {
        let (mut daemon, mut attachment) = UnixStream::pair().unwrap();
        let (mut fake_stdin, _stdin_writer) = UnixStream::pair().unwrap();
        let sender = thread::spawn(move || {
            AttachFrame::Output(b"before\x1b[?1004;2004lafter".to_vec())
                .write_to(&mut daemon)
                .unwrap();
            AttachFrame::Detached.write_to(&mut daemon).unwrap();
        });
        let mut stdout = Vec::new();
        let mut input = [0; 128];
        let mut size = (24, 80, 0, 0);

        let reconnect = pump_attachment(
            &mut attachment,
            &mut fake_stdin,
            &mut stdout,
            &mut input,
            &mut size,
            &mut FocusTracking {
                enabled: true,
                ..Default::default()
            },
        )
        .unwrap();

        sender.join().unwrap();
        assert!(!reconnect);
        assert_eq!(stdout, b"before\x1b[?1004;2004lafter\x1b[?1004h");
    }
}

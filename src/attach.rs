use std::env;
use std::io::{self, Read, Write};
use std::os::fd::AsRawFd;
use std::thread;
use std::time::Duration;

use crossterm::terminal;

use crate::client;
use crate::protocol::{AttachFrame, TerminalProfile};

const POLL_INTERVAL_MS: i32 = 100;
const RECONNECT_ATTEMPTS: usize = 600;
const RECONNECT_DELAY: Duration = Duration::from_millis(25);

struct RawMode;

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

pub fn run(shell_id: &str, takeover: bool, restart_exited: bool) -> io::Result<()> {
    let mut profile = terminal_profile()?;
    let mut size = (
        profile.rows,
        profile.cols,
        profile.pixel_width,
        profile.pixel_height,
    );
    let client = client::connect_or_start()?;
    let mut attachment = if restart_exited {
        client.attach_restarting(shell_id, takeover, profile.clone())?
    } else {
        client.attach(shell_id, takeover, profile.clone())?
    };
    let _raw_mode = RawMode::enter()?;
    let mut stdin = io::stdin().lock();
    let mut stdout = io::stdout().lock();
    let mut input = [0; 16 * 1024];

    loop {
        if let Some(warning) = attachment.warning.take() {
            eprintln!("boomux: warning: {warning}");
        }
        stdout.write_all(&attachment.reconstruction)?;
        stdout.flush()?;
        let mut stream = attachment.stream;

        if !pump_attachment(&mut stream, &mut stdin, &mut stdout, &mut input, &mut size)? {
            return Ok(());
        }
        if let Ok((rows, cols, pixel_width, pixel_height)) = dimensions() {
            size = (rows, cols, pixel_width, pixel_height);
            profile.rows = rows;
            profile.cols = cols;
            profile.pixel_width = pixel_width;
            profile.pixel_height = pixel_height;
        }
        attachment = reconnect(&client, shell_id, takeover, &profile)?;
    }
}

fn reconnect(
    client: &client::Client,
    shell_id: &str,
    takeover: bool,
    profile: &TerminalProfile,
) -> io::Result<client::Attachment> {
    let mut last_error = None;
    for _ in 0..RECONNECT_ATTEMPTS {
        match client.attach(shell_id, takeover, profile.clone()) {
            Ok(attachment) => return Ok(attachment),
            Err(error) => last_error = Some(error),
        }
        thread::sleep(RECONNECT_DELAY);
    }
    Err(last_error.unwrap_or_else(|| io::Error::other("daemon attachment did not reconnect")))
}

fn pump_attachment(
    stream: &mut std::os::unix::net::UnixStream,
    stdin: &mut (impl Read + AsRawFd),
    stdout: &mut impl Write,
    input: &mut [u8],
    size: &mut (u16, u16, u16, u16),
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
        let ready = unsafe {
            libc::poll(
                descriptors.as_mut_ptr(),
                descriptors.len() as libc::nfds_t,
                POLL_INTERVAL_MS,
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
                    stdout.flush()?;
                }
                Ok(AttachFrame::Detached) => return Ok(false),
                Ok(AttachFrame::Reconnect) => {
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
                AttachFrame::Detached.write_to(stream)?;
                return Ok(false);
            }
            AttachFrame::Input(input[..count].to_vec()).write_to(stream)?;
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
    use std::os::unix::net::UnixStream;

    use super::*;

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
        )
        .unwrap();

        sender.join().unwrap();
        assert!(reconnect);
        assert_eq!(stdout, b"before-reconnect");
    }
}

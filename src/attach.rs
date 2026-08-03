use std::env;
use std::io::{self, Read, Write};
use std::os::fd::AsRawFd;

use crossterm::terminal;

use crate::client;
use crate::protocol::{AttachFrame, TerminalProfile};

const POLL_INTERVAL_MS: i32 = 100;

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

pub fn run(shell_id: &str, takeover: bool) -> io::Result<()> {
    let profile = terminal_profile()?;
    let mut size = (
        profile.rows,
        profile.cols,
        profile.pixel_width,
        profile.pixel_height,
    );
    let client = client::connect_or_start()?;
    let attachment = client.attach(shell_id, takeover, profile)?;
    if let Some(warning) = attachment.warning {
        eprintln!("boomux: warning: {warning}");
    }
    let mut stream = attachment.stream;
    let _raw_mode = RawMode::enter()?;
    let mut stdin = io::stdin().lock();
    let mut stdout = io::stdout().lock();
    stdout.write_all(&attachment.replay)?;
    stdout.flush()?;

    let mut input = [0; 16 * 1024];

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

        if descriptors[0].revents & libc::POLLIN != 0 {
            let count = stdin.read(&mut input)?;
            if count == 0 {
                AttachFrame::Detached.write_to(&mut stream)?;
                return Ok(());
            }
            AttachFrame::Input(input[..count].to_vec()).write_to(&mut stream)?;
        }
        if descriptors[1].revents & (libc::POLLIN | libc::POLLHUP | libc::POLLERR) != 0 {
            match AttachFrame::read_from(&mut stream) {
                Ok(AttachFrame::Output(bytes)) => {
                    stdout.write_all(&bytes)?;
                    stdout.flush()?;
                }
                Ok(AttachFrame::Detached) => return Ok(()),
                Ok(_) => {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "daemon sent an invalid attach frame",
                    ));
                }
                Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => return Ok(()),
                Err(error) => return Err(error),
            }
        }

        if let Ok(new_size) = dimensions()
            && new_size != size
        {
            size = new_size;
            AttachFrame::Resize {
                rows: size.0,
                cols: size.1,
                pixel_width: size.2,
                pixel_height: size.3,
            }
            .write_to(&mut stream)?;
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

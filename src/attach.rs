use std::io::{self, Read, Write};
use std::os::fd::AsRawFd;

use crossterm::terminal;

use crate::client;
use crate::protocol::AttachFrame;

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
    let client = client::connect_or_start()?;
    let (mut stream, _token, replay) = client.attach(shell_id, takeover)?;
    let _raw_mode = RawMode::enter()?;
    let mut stdin = io::stdin().lock();
    let mut stdout = io::stdout().lock();
    stdout.write_all(&replay)?;
    stdout.flush()?;

    let (mut cols, mut rows) = terminal::size()?;
    AttachFrame::Resize { rows, cols }.write_to(&mut stream)?;
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

        if let Ok((new_cols, new_rows)) = terminal::size()
            && (new_cols, new_rows) != (cols, rows)
        {
            cols = new_cols;
            rows = new_rows;
            AttachFrame::Resize { rows, cols }.write_to(&mut stream)?;
        }
    }
}

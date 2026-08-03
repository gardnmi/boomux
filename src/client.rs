use std::env;
use std::io;
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::Duration;

use crate::protocol::{
    self, Envelope, Request, Response, ShellSnapshot, ShellSpec, Snapshot, WorkspaceSnapshot,
};

const CONNECT_ATTEMPTS: usize = 40;
const CONNECT_DELAY: Duration = Duration::from_millis(25);

#[derive(Debug, Clone)]
pub struct Client {
    socket_path: PathBuf,
}

pub fn socket_path() -> io::Result<PathBuf> {
    let runtime = env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "XDG_RUNTIME_DIR is not set"))?;
    if !runtime.is_absolute() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "XDG_RUNTIME_DIR must be absolute",
        ));
    }
    Ok(runtime.join("boomux").join("daemon.sock"))
}

pub fn connect_or_start() -> io::Result<Client> {
    let client = Client {
        socket_path: socket_path()?,
    };
    if client.ping().is_ok() {
        return Ok(client);
    }

    let mut command = Command::new(env::current_exe()?);
    command
        .arg("daemon")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    // The child has not executed user code yet; `setsid` only detaches it
    // from the launching terminal before exec.
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                Err(io::Error::last_os_error())
            } else {
                Ok(())
            }
        });
    }
    command.spawn()?;

    let mut last_error = None;
    for _ in 0..CONNECT_ATTEMPTS {
        match client.ping() {
            Ok(()) => return Ok(client),
            Err(error) => last_error = Some(error),
        }
        thread::sleep(CONNECT_DELAY);
    }
    Err(last_error.unwrap_or_else(|| io::Error::other("daemon did not start")))
}

impl Client {
    pub fn socket_path(&self) -> &Path {
        &self.socket_path
    }

    pub fn request(&self, request: Request) -> io::Result<Response> {
        let mut stream = UnixStream::connect(&self.socket_path)?;
        protocol::write_message(&mut stream, &Envelope::new(request))?;
        let response: Envelope<Response> = protocol::read_message(&mut stream)?;
        if response.version != protocol::PROTOCOL_VERSION {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "protocol version mismatch",
            ));
        }
        match response.message {
            Response::Error { message } => Err(io::Error::other(message)),
            response => Ok(response),
        }
    }

    pub fn ping(&self) -> io::Result<()> {
        expect_ok(self.request(Request::Ping)?, Response::Pong)
    }

    pub fn snapshot(&self) -> io::Result<Snapshot> {
        match self.request(Request::Snapshot)? {
            Response::Snapshot { snapshot } => Ok(snapshot),
            other => unexpected(other),
        }
    }

    pub fn get_workspace(&self, workspace_id: impl Into<String>) -> io::Result<WorkspaceSnapshot> {
        match self.request(Request::GetWorkspace {
            workspace_id: workspace_id.into(),
        })? {
            Response::Workspace { workspace } => Ok(workspace),
            other => unexpected(other),
        }
    }

    pub fn get_shell(&self, shell_id: impl Into<String>) -> io::Result<ShellSnapshot> {
        match self.request(Request::GetShell {
            shell_id: shell_id.into(),
        })? {
            Response::Shell { shell } => Ok(shell),
            other => unexpected(other),
        }
    }

    pub fn create_workspace(
        &self,
        name: impl Into<String>,
        cwd: PathBuf,
        shells: Vec<ShellSpec>,
    ) -> io::Result<WorkspaceSnapshot> {
        match self.request(Request::CreateWorkspace {
            name: name.into(),
            cwd,
            shells,
        })? {
            Response::Workspace { workspace } => Ok(workspace),
            other => unexpected(other),
        }
    }

    pub fn create_shell(
        &self,
        workspace_id: impl Into<String>,
        shell: ShellSpec,
    ) -> io::Result<ShellSnapshot> {
        match self.request(Request::CreateShell {
            workspace_id: workspace_id.into(),
            shell,
        })? {
            Response::Shell { shell } => Ok(shell),
            other => unexpected(other),
        }
    }

    pub fn read_shell(&self, shell_id: impl Into<String>, max_bytes: usize) -> io::Result<Vec<u8>> {
        match self.request(Request::ReadShell {
            shell_id: shell_id.into(),
            max_bytes,
        })? {
            Response::Output { bytes } => Ok(bytes),
            other => unexpected(other),
        }
    }

    #[allow(dead_code)]
    pub fn rename_workspace(
        &self,
        workspace_id: impl Into<String>,
        name: impl Into<String>,
    ) -> io::Result<()> {
        expect_ok(
            self.request(Request::RenameWorkspace {
                workspace_id: workspace_id.into(),
                name: name.into(),
            })?,
            Response::Ok,
        )
    }

    pub fn rename_shell(
        &self,
        shell_id: impl Into<String>,
        name: impl Into<String>,
    ) -> io::Result<()> {
        expect_ok(
            self.request(Request::RenameShell {
                shell_id: shell_id.into(),
                name: name.into(),
            })?,
            Response::Ok,
        )
    }

    pub fn close_workspace(&self, workspace_id: impl Into<String>) -> io::Result<()> {
        expect_ok(
            self.request(Request::CloseWorkspace {
                workspace_id: workspace_id.into(),
            })?,
            Response::Ok,
        )
    }

    #[allow(dead_code)]
    pub fn close_shell(&self, shell_id: impl Into<String>) -> io::Result<()> {
        expect_ok(
            self.request(Request::CloseShell {
                shell_id: shell_id.into(),
            })?,
            Response::Ok,
        )
    }

    pub fn attach(
        &self,
        shell_id: impl Into<String>,
        takeover: bool,
    ) -> io::Result<(UnixStream, String, Vec<u8>)> {
        let mut stream = UnixStream::connect(&self.socket_path)?;
        protocol::write_message(
            &mut stream,
            &Envelope::new(Request::Attach {
                shell_id: shell_id.into(),
                takeover,
            }),
        )?;
        let response: Envelope<Response> = protocol::read_message(&mut stream)?;
        if response.version != protocol::PROTOCOL_VERSION {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "protocol version mismatch",
            ));
        }
        match response.message {
            Response::Attached { token, replay } => Ok((stream, token, replay)),
            Response::Error { message } => Err(io::Error::other(message)),
            other => unexpected(other),
        }
    }
}

fn expect_ok(actual: Response, expected: Response) -> io::Result<()> {
    if actual == expected {
        Ok(())
    } else {
        unexpected(actual)
    }
}

fn unexpected<T>(response: Response) -> io::Result<T> {
    Err(io::Error::new(
        io::ErrorKind::InvalidData,
        format!("unexpected daemon response: {response:?}"),
    ))
}

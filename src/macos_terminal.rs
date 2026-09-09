//! Terminal.app bridge. Only the private rendezvous path enters the launcher
//! script; commands and their exact Unix argument vectors travel over a socket.
use serde::{Deserialize, Serialize};
use std::env;
use std::ffi::{OsStr, OsString};
use std::fs::{self, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};
use std::time::{Duration, Instant};

const LIMIT: u64 = 1024 * 1024;
#[derive(Serialize, Deserialize)]
struct Launch {
    program: Vec<u8>,
    args: Vec<Vec<u8>>,
    cwd: Option<Vec<u8>>,
    environment: Vec<(Vec<u8>, Vec<u8>)>,
}
struct Files {
    socket: PathBuf,
    script: PathBuf,
}
impl Drop for Files {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.socket);
        let _ = fs::remove_file(&self.script);
    }
}

pub fn launch(program: &OsStr, args: &[OsString], cwd: Option<&Path>) -> io::Result<()> {
    let root = boomux::client::socket_path()?
        .parent()
        .unwrap()
        .to_path_buf();
    let id = uuid::Uuid::new_v4().simple().to_string();
    let files = Files {
        socket: root.join(format!("t-{}.sock", &id[..12])),
        script: root.join(format!("t-{}.command", &id[..12])),
    };
    let listener = UnixListener::bind(&files.socket)?;
    fs::set_permissions(&files.socket, fs::Permissions::from_mode(0o600))?;
    listener.set_nonblocking(true)?;
    let executable = env::current_exe()?;
    let quote = |path: &Path| -> io::Result<String> {
        let text = path
            .to_str()
            .ok_or_else(|| io::Error::other("terminal bridge path is not UTF-8"))?;
        Ok(format!("'{}'", text.replace('\'', "'\\''")))
    };
    let script = format!(
        "#!/bin/sh\nexec {} __macos-terminal {}\n",
        quote(&executable)?,
        quote(&files.socket)?
    );
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o700)
        .open(&files.script)?
        .write_all(script.as_bytes())?;
    let request = Launch {
        program: program.as_bytes().to_vec(),
        args: args.iter().map(|s| s.as_bytes().to_vec()).collect(),
        cwd: cwd.map(|p| p.as_os_str().as_bytes().to_vec()),
        environment: env::vars_os()
            .map(|(k, v)| (k.as_bytes().to_vec(), v.as_bytes().to_vec()))
            .collect(),
    };
    let bytes = serde_json::to_vec(&request)?;
    if bytes.len() as u64 > LIMIT {
        return Err(io::Error::other(
            "terminal startup environment exceeds bound",
        ));
    }
    let mut opener = Command::new("/usr/bin/open")
        .args(["-a", "Terminal"])
        .arg(&files.script)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(status) = opener.try_wait()?
            && !status.success()
        {
            return Err(io::Error::other("Terminal.app could not be opened"));
        }
        match listener.accept() {
            Ok((mut stream, _)) => {
                let (_, uid) = boomux::platform::peer_credentials(&stream)?;
                if uid != unsafe { libc::geteuid() } {
                    continue;
                }
                stream.set_nonblocking(false)?;
                stream.set_write_timeout(Some(Duration::from_secs(2)))?;
                stream.write_all(&bytes)?;
                return Ok(());
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => (),
            Err(e) => return Err(e),
        }
        if Instant::now() >= deadline {
            let _ = opener.kill();
            let _ = opener.wait();
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "Terminal.app did not connect",
            ));
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

pub fn dispatch() -> Option<ExitCode> {
    if env::args_os().nth(1).as_deref() != Some(OsStr::new("__macos-terminal")) {
        return None;
    }
    let result = (|| -> io::Result<()> {
        let path = env::args_os()
            .nth(2)
            .ok_or_else(|| io::Error::other("missing terminal rendezvous"))?;
        let stream = UnixStream::connect(path)?;
        let (_, uid) = boomux::platform::peer_credentials(&stream)?;
        if uid != unsafe { libc::geteuid() } {
            return Err(io::Error::other("terminal rendezvous owner mismatch"));
        }
        stream.set_read_timeout(Some(Duration::from_secs(2)))?;
        let mut bytes = Vec::new();
        stream.take(LIMIT + 1).read_to_end(&mut bytes)?;
        if bytes.len() as u64 > LIMIT {
            return Err(io::Error::other("terminal request exceeds bound"));
        }
        let request: Launch = serde_json::from_slice(&bytes)?;
        let mut command = Command::new(OsString::from_vec(request.program));
        command.args(request.args.into_iter().map(OsString::from_vec));
        command.env_clear().envs(
            request
                .environment
                .into_iter()
                .map(|(k, v)| (OsString::from_vec(k), OsString::from_vec(v))),
        );
        if let Some(cwd) = request.cwd {
            command.current_dir(OsString::from_vec(cwd));
        }
        Err(command.exec())
    })();
    if let Err(e) = result {
        eprintln!("boomux: {e}");
    }
    Some(ExitCode::FAILURE)
}

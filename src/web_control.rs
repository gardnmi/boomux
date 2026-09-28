//! Bounded control protocol shared by the legacy dashboard and its migration.
use boomux::client;
use serde::{Deserialize, Serialize};
use std::{
    error::Error,
    fs,
    io::{self, Read, Write},
    os::unix::net::UnixStream,
    path::{Path, PathBuf},
    thread,
    time::Duration,
};

const WEB_STOP_TIMEOUT: Duration = Duration::from_secs(5);
pub(crate) const WEB_STOP_REQUEST: &[u8] = b"boomux-web-stop-v1\n";
pub(crate) const WEB_STOP_ACK: &[u8] = b"boomux-web-stopping-v1\n";
pub(crate) const WEB_STATUS_REQUEST: &[u8] = b"boomux-web-status-v1\n";
const MAX_WEB_CONTROL_RESPONSE: u64 = 16 * 1024;

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct WebStatus {
    pub(crate) running: bool,
    pub(crate) port: u16,
    pub(crate) tailscale: bool,
    pub(crate) dashboard_url: String,
    #[serde(default)]
    pub(crate) opencode_requested_port: Option<u16>,
    #[serde(default)]
    pub(crate) opencode_requested_url: Option<String>,
    pub(crate) opencode_port: Option<u16>,
    pub(crate) opencode_url: Option<String>,
}

pub(crate) fn stop(port: u16) -> Result<bool, Box<dyn Error>> {
    stop_control_socket(&web_control_socket_path(port)?).map_err(Into::into)
}

pub(crate) fn status(port: u16) -> Result<Option<WebStatus>, Box<dyn Error>> {
    status_control_socket(&web_control_socket_path(port)?).map_err(Into::into)
}

pub(crate) fn status_control_socket(path: &Path) -> io::Result<Option<WebStatus>> {
    let mut stream = match UnixStream::connect(path) {
        Ok(stream) => stream,
        Err(error)
            if matches!(
                error.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
            ) =>
        {
            return Ok(None);
        }
        Err(error) => return Err(error),
    };
    stream.set_read_timeout(Some(WEB_STOP_TIMEOUT))?;
    stream.set_write_timeout(Some(WEB_STOP_TIMEOUT))?;
    stream.write_all(WEB_STATUS_REQUEST)?;
    let mut response = Vec::new();
    stream
        .take(MAX_WEB_CONTROL_RESPONSE)
        .read_to_end(&mut response)?;
    if response.len() as u64 == MAX_WEB_CONTROL_RESPONSE {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Boomux web status response exceeded its bound",
        ));
    }
    serde_json::from_slice(&response)
        .map(Some)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

pub(crate) fn web_control_socket_path(port: u16) -> io::Result<PathBuf> {
    client::socket_path()?
        .parent()
        .map(|directory| directory.join(format!("web-{port}.sock")))
        .ok_or_else(|| io::Error::other("Boomux daemon socket has no runtime directory"))
}

pub(crate) fn stop_control_socket(path: &Path) -> io::Result<bool> {
    let mut stream = match UnixStream::connect(path) {
        Ok(stream) => stream,
        Err(error)
            if matches!(
                error.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
            ) =>
        {
            return Ok(false);
        }
        Err(error) => return Err(error),
    };
    stream.set_read_timeout(Some(WEB_STOP_TIMEOUT))?;
    stream.set_write_timeout(Some(WEB_STOP_TIMEOUT))?;
    stream.write_all(WEB_STOP_REQUEST)?;
    let mut acknowledgement = vec![0; WEB_STOP_ACK.len()];
    stream.read_exact(&mut acknowledgement)?;
    if acknowledgement != WEB_STOP_ACK {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Boomux web returned an invalid stop acknowledgement",
        ));
    }
    let started = std::time::Instant::now();
    while started.elapsed() < WEB_STOP_TIMEOUT {
        match fs::symlink_metadata(path) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(true),
            Err(error) => return Err(error),
            Ok(_) => thread::sleep(Duration::from_millis(100)),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::TimedOut,
        "Boomux web did not stop before the timeout",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn stop_control_socket_requests_and_waits_for_shutdown() {
        use std::os::unix::net::UnixListener as StdUnixListener;

        let directory =
            std::env::temp_dir().join(format!("boomux-web-stop-test-{}", uuid::Uuid::new_v4()));
        fs::create_dir(&directory).unwrap();
        let path = directory.join("web.sock");
        let listener = StdUnixListener::bind(&path).unwrap();
        let server_path = path.clone();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = vec![0; WEB_STOP_REQUEST.len()];
            stream.read_exact(&mut request).unwrap();
            assert_eq!(request, WEB_STOP_REQUEST);
            stream.write_all(WEB_STOP_ACK).unwrap();
            drop(stream);
            drop(listener);
            fs::remove_file(server_path).unwrap();
        });

        assert!(stop_control_socket(&path).unwrap());
        server.join().unwrap();
        assert!(!stop_control_socket(&path).unwrap());
        fs::remove_dir(directory).unwrap();
    }

    #[test]
    fn status_control_socket_returns_bounded_runtime_state() {
        use std::os::unix::net::UnixListener as StdUnixListener;

        let directory =
            std::env::temp_dir().join(format!("boomux-web-status-test-{}", uuid::Uuid::new_v4()));
        fs::create_dir(&directory).unwrap();
        let path = directory.join("web.sock");
        let listener = StdUnixListener::bind(&path).unwrap();
        let expected = WebStatus {
            running: true,
            port: 3737,
            tailscale: true,
            dashboard_url: "https://host.example.ts.net".into(),
            opencode_requested_port: Some(4097),
            opencode_requested_url: None,
            opencode_port: Some(4097),
            opencode_url: Some("https://host.example.ts.net:4097".into()),
        };
        let response = serde_json::to_vec(&expected).unwrap();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = vec![0; WEB_STATUS_REQUEST.len()];
            stream.read_exact(&mut request).unwrap();
            assert_eq!(request, WEB_STATUS_REQUEST);
            stream.write_all(&response).unwrap();
        });

        let status = status_control_socket(&path).unwrap().unwrap();
        assert_eq!(status.dashboard_url, expected.dashboard_url);
        assert_eq!(status.opencode_requested_port, Some(4097));
        assert_eq!(status.opencode_port, Some(4097));
        server.join().unwrap();
        fs::remove_file(path).unwrap();
        fs::remove_dir(directory).unwrap();
    }

    #[test]
    fn old_web_status_defaults_requested_opencode_configuration() {
        let status: WebStatus = serde_json::from_str(
            r#"{"running":true,"port":3737,"tailscale":false,"dashboard_url":"http://127.0.0.1:3737","opencode_port":4097,"opencode_url":"http://127.0.0.1:4097"}"#,
        )
        .unwrap();
        assert_eq!(status.opencode_requested_port, None);
        assert_eq!(status.opencode_requested_url, None);
        assert_eq!(status.opencode_port, Some(4097));
    }
}

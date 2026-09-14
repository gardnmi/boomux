//! Desktop-owned tiling gateway. Closing stdin revokes sharing without touching Shells.
use std::io::Read;
use std::os::fd::AsRawFd;
use std::os::unix::fs::PermissionsExt;

pub const INSTALL_URL: &str = "https://tailscale.com/download";
pub const SETUP_URL: &str = "https://tailscale.com/docs/features/tailscale-serve";
use std::path::PathBuf;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::time::{Duration, Instant};

pub struct Publisher {
    pub url: String,
    input: Option<ChildStdin>,
    child: Option<Child>,
}
impl Publisher {
    pub fn start() -> Result<Self, String> {
        let directory = std::env::current_exe()
            .map_err(|e| e.to_string())?
            .parent()
            .ok_or("Desktop executable has no directory")?
            .to_path_buf();
        let executable = gateway_path(&directory).ok_or(
            "Build the tiling web gateway beside Desktop: cargo build --locked --example webgpu_gateway",
        )?;
        let tailscale_available = std::env::var_os("PATH").is_some_and(|paths| {
            std::env::split_paths(&paths).any(|directory| {
                std::fs::metadata(directory.join("tailscale")).is_ok_and(|metadata| {
                    metadata.is_file() && metadata.permissions().mode() & 0o111 != 0
                })
            })
        });
        if !tailscale_available {
            return Err("WebUI sharing needs Tailscale. Install it and connect this computer, then try Open WebUI again.".into());
        }
        let mut child = Command::new(executable)
            .args(["--desktop", "--tailscale"])
            .env("POC_PORT", "4391")
            .env_remove("POC_WORKSPACE_ID")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .map_err(|e| format!("Could not start web sharing: {e}"))?;
        let input = child.stdin.take();
        let mut output = child
            .stdout
            .take()
            .ok_or("Gateway readiness pipe missing")?;
        let mut publisher = Self {
            url: String::new(),
            input,
            child: Some(child),
        };
        // Read readiness with a deadline and a strict size bound; no orphan reader thread.
        let fd = output.as_raw_fd();
        unsafe {
            let flags = libc::fcntl(fd, libc::F_GETFL);
            if flags < 0 || libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) < 0 {
                return Err(std::io::Error::last_os_error().to_string());
            }
        }
        let deadline = Instant::now() + Duration::from_secs(60);
        let mut bytes = Vec::new();
        loop {
            let mut buffer = [0; 1024];
            match output.read(&mut buffer) {
                Ok(0) => return Err("Web gateway exited before sharing was ready".into()),
                Ok(count) => {
                    bytes.extend_from_slice(&buffer[..count]);
                    if bytes.len() > 8192 {
                        return Err("Invalid gateway readiness response".into());
                    }
                    if let Some(end) = bytes.iter().position(|byte| *byte == b'\n') {
                        let value: serde_json::Value =
                            serde_json::from_slice(&bytes[..end]).map_err(|e| e.to_string())?;
                        if let Some(error) = value["error"].as_str() {
                            return Err(error.into());
                        }
                        let url = value["url"]
                            .as_str()
                            .filter(|url| {
                                url.starts_with("https://") && !url.contains(char::is_whitespace)
                            })
                            .ok_or("Gateway did not return a private HTTPS URL")?;
                        publisher.url = url.into();
                        return Ok(publisher);
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(error) => return Err(error.to_string()),
            }
            if Instant::now() >= deadline {
                return Err(
                    "Web sharing timed out. Check Tailscale sign-in and HTTPS permissions.".into(),
                );
            }
            std::thread::sleep(Duration::from_millis(25));
        }
    }

    pub fn stop(mut self) -> Result<(), String> {
        self.input.take();
        if let Some(mut child) = self.child.take() {
            let status = child.wait().map_err(|e| e.to_string())?;
            if !status.success() {
                return Err(format!(
                    "Web gateway stopped with {status}; check tailscale serve status"
                ));
            }
        }
        Ok(())
    }
}
impl Drop for Publisher {
    fn drop(&mut self) {
        self.input.take();
        if let Some(mut child) = self.child.take() {
            // Reap off the UI thread. EOF also handles unexpected Desktop termination.
            std::thread::spawn(move || {
                let _ = child.wait();
            });
        }
    }
}
fn gateway_path(directory: &std::path::Path) -> Option<PathBuf> {
    [
        directory.join("webgpu_gateway"),
        directory.join("examples/webgpu_gateway"),
    ]
    .into_iter()
    .find(|path| path.is_file())
}

use std::error::Error;
use std::ffi::{OsStr, OsString};
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

const OWNERSHIP_VERSION: u32 = 1;
const MAX_STATUS_BYTES: usize = 1024 * 1024;
const MAX_ERROR_BYTES: usize = 4096;

#[derive(Clone, Debug, Deserialize, Serialize)]
struct OwnedRoute {
    https_port: u16,
    target: String,
}

#[derive(Debug, Deserialize, Serialize)]
struct OwnershipRecord {
    version: u32,
    dns_name: String,
    routes: Vec<OwnedRoute>,
}

#[derive(Debug)]
struct TailnetStatus {
    dns_name: String,
}

pub(crate) struct Exposure {
    executable: OsString,
    record_path: PathBuf,
    record: OwnershipRecord,
    active: bool,
}

impl Exposure {
    pub(crate) fn enable(
        dashboard_port: u16,
        opencode_port: Option<u16>,
    ) -> Result<Self, Box<dyn Error>> {
        let record_path = ownership_path(dashboard_port)?;
        cleanup_record_if_present(OsStr::new("tailscale"), &record_path)?;
        Self::enable_with(
            OsStr::new("tailscale"),
            record_path,
            dashboard_port,
            opencode_port,
        )
    }

    fn enable_with(
        executable: &OsStr,
        record_path: PathBuf,
        dashboard_port: u16,
        opencode_port: Option<u16>,
    ) -> Result<Self, Box<dyn Error>> {
        let tailnet = tailnet_status(executable)?;
        let serve_status = serve_status(executable)?;
        let mut desired = vec![OwnedRoute {
            https_port: 443,
            target: format!("http://127.0.0.1:{dashboard_port}"),
        }];
        if let Some(port) = opencode_port {
            desired.push(OwnedRoute {
                https_port: port,
                target: format!("http://127.0.0.1:{port}"),
            });
        }

        let mut missing = Vec::new();
        for route in desired {
            match route_state(&serve_status, &tailnet.dns_name, &route) {
                RouteState::Compatible => {}
                RouteState::Missing => missing.push(route),
                RouteState::Conflict => {
                    return Err(io::Error::new(
                        io::ErrorKind::AddrInUse,
                        format!(
                            "Tailscale Serve HTTPS port {} already has a conflicting handler",
                            route.https_port
                        ),
                    )
                    .into());
                }
            }
        }

        let mut exposure = Self {
            executable: executable.to_owned(),
            record_path,
            record: OwnershipRecord {
                version: OWNERSHIP_VERSION,
                dns_name: tailnet.dns_name,
                routes: Vec::new(),
            },
            active: true,
        };
        for route in missing {
            exposure.record.routes.push(route.clone());
            exposure.persist()?;
            if let Err(error) = configure_route(executable, &route) {
                let _ = exposure.cleanup();
                return Err(error);
            }
        }
        if exposure.record.routes.is_empty() {
            exposure.active = false;
        }
        Ok(exposure)
    }

    pub(crate) fn dashboard_url(&self) -> String {
        format!("https://{}", self.record.dns_name)
    }

    pub(crate) fn opencode_url(&self, port: u16) -> String {
        format!("https://{}:{port}", self.record.dns_name)
    }

    fn persist(&self) -> io::Result<()> {
        write_record(&self.record_path, &self.record)
    }

    fn cleanup(&mut self) -> Result<(), Box<dyn Error>> {
        if !self.active {
            return Ok(());
        }
        cleanup_record(&self.executable, &self.record_path, &self.record)?;
        self.active = false;
        Ok(())
    }
}

impl Drop for Exposure {
    fn drop(&mut self) {
        if let Err(error) = self.cleanup() {
            eprintln!("boomux: failed to remove Tailscale Serve exposure: {error}");
        }
    }
}

pub(crate) fn cleanup_stale(dashboard_port: u16) -> Result<(), Box<dyn Error>> {
    cleanup_record_if_present(OsStr::new("tailscale"), &ownership_path(dashboard_port)?)
}

fn ownership_path(dashboard_port: u16) -> io::Result<PathBuf> {
    boomux::client::socket_path()?
        .parent()
        .map(|directory| directory.join(format!("web-{dashboard_port}-tailscale.json")))
        .ok_or_else(|| io::Error::other("Boomux daemon socket has no runtime directory"))
}

fn tailnet_status(executable: &OsStr) -> Result<TailnetStatus, Box<dyn Error>> {
    let output = run(executable, &["status", "--json"])?;
    let status = successful_json("tailscale status", output)?;
    if status.get("BackendState").and_then(Value::as_str) != Some("Running")
        || status.pointer("/Self/Online").and_then(Value::as_bool) != Some(true)
    {
        return Err(
            io::Error::new(io::ErrorKind::NotConnected, "Tailscale is not connected").into(),
        );
    }
    let dns_name = status
        .pointer("/Self/DNSName")
        .and_then(Value::as_str)
        .map(|name| name.trim_end_matches('.'))
        .filter(|name| !name.is_empty())
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                "Tailscale MagicDNS name is unavailable",
            )
        })?;
    Ok(TailnetStatus {
        dns_name: dns_name.to_owned(),
    })
}

fn serve_status(executable: &OsStr) -> Result<Value, Box<dyn Error>> {
    successful_json(
        "tailscale serve status",
        run(executable, &["serve", "status", "--json"])?,
    )
}

fn successful_json(context: &str, output: Output) -> Result<Value, Box<dyn Error>> {
    if !output.status.success() {
        return Err(command_error(context, &output).into());
    }
    if output.stdout.len() > MAX_STATUS_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{context} output exceeded {MAX_STATUS_BYTES} bytes"),
        )
        .into());
    }
    serde_json::from_slice(&output.stdout).map_err(Into::into)
}

fn run(executable: &OsStr, arguments: &[&str]) -> io::Result<Output> {
    Command::new(executable).args(arguments).output()
}

fn configure_route(executable: &OsStr, route: &OwnedRoute) -> Result<(), Box<dyn Error>> {
    let https = format!("--https={}", route.https_port);
    let output = Command::new(executable)
        .args(["serve", "--bg", "--yes", &https, &route.target])
        .output()?;
    if output.status.success() {
        Ok(())
    } else {
        Err(command_error("tailscale serve", &output).into())
    }
}

fn remove_route(executable: &OsStr, route: &OwnedRoute) -> Result<(), Box<dyn Error>> {
    let https = format!("--https={}", route.https_port);
    let output = Command::new(executable)
        .args(["serve", &https, "--set-path=/", "off"])
        .output()?;
    if output.status.success() {
        Ok(())
    } else {
        Err(command_error("tailscale serve off", &output).into())
    }
}

fn command_error(context: &str, output: &Output) -> io::Error {
    let stderr = String::from_utf8_lossy(&output.stderr);
    let detail = stderr
        .trim()
        .chars()
        .take(MAX_ERROR_BYTES)
        .collect::<String>();
    io::Error::other(if detail.is_empty() {
        format!("{context} failed with {}", output.status)
    } else {
        format!("{context} failed: {detail}")
    })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RouteState {
    Compatible,
    Missing,
    Conflict,
}

fn route_state(status: &Value, dns_name: &str, route: &OwnedRoute) -> RouteState {
    let web_key = format!("{dns_name}:{}", route.https_port);
    let proxy = status
        .get("Web")
        .and_then(|web| web.get(&web_key))
        .and_then(|web| web.pointer("/Handlers/~1/Proxy"))
        .and_then(Value::as_str);
    if proxy == Some(route.target.as_str()) {
        return RouteState::Compatible;
    }
    if proxy.is_some() {
        return RouteState::Conflict;
    }
    let port = route.https_port.to_string();
    if let Some(tcp) = status.get("TCP").and_then(|tcp| tcp.get(&port))
        && tcp.get("HTTPS").and_then(Value::as_bool) != Some(true)
    {
        return RouteState::Conflict;
    }
    RouteState::Missing
}

fn cleanup_record_if_present(executable: &OsStr, record_path: &Path) -> Result<(), Box<dyn Error>> {
    let Some(record) = read_record(record_path)? else {
        return Ok(());
    };
    cleanup_record(executable, record_path, &record)
}

fn cleanup_record(
    executable: &OsStr,
    record_path: &Path,
    record: &OwnershipRecord,
) -> Result<(), Box<dyn Error>> {
    let status = serve_status(executable)?;
    for route in &record.routes {
        if route_state(&status, &record.dns_name, route) == RouteState::Compatible {
            remove_route(executable, route)?;
        }
    }
    match fs::remove_file(record_path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

fn read_record(path: &Path) -> io::Result<Option<OwnershipRecord>> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    if !metadata.file_type().is_file()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.len() > 64 * 1024
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid Boomux Tailscale ownership record",
        ));
    }
    let record: OwnershipRecord = serde_json::from_slice(&fs::read(path)?)?;
    if record.version != OWNERSHIP_VERSION {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "unsupported Boomux Tailscale ownership record version",
        ));
    }
    Ok(Some(record))
}

fn write_record(path: &Path, record: &OwnershipRecord) -> io::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::other("Tailscale ownership record has no parent"))?;
    let temporary = parent.join(format!(".tailscale-{}.tmp", Uuid::new_v4()));
    let result = (|| {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temporary)?;
        serde_json::to_writer(&mut file, record)?;
        file.write_all(b"\n")?;
        file.sync_all()?;
        fs::rename(&temporary, path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn status(proxy: Option<&str>) -> Value {
        let mut value = serde_json::json!({
            "TCP": {},
            "Web": {},
        });
        if let Some(proxy) = proxy {
            value["TCP"]["443"] = serde_json::json!({ "HTTPS": true });
            value["Web"]["host.example.ts.net:443"] = serde_json::json!({
                "Handlers": { "/": { "Proxy": proxy } }
            });
        }
        value
    }

    #[test]
    fn route_planning_preserves_compatible_and_rejects_conflicting_handlers() {
        let route = OwnedRoute {
            https_port: 443,
            target: "http://127.0.0.1:3737".into(),
        };
        assert_eq!(
            route_state(&status(None), "host.example.ts.net", &route),
            RouteState::Missing
        );
        assert_eq!(
            route_state(
                &status(Some("http://127.0.0.1:3737")),
                "host.example.ts.net",
                &route
            ),
            RouteState::Compatible
        );
        assert_eq!(
            route_state(
                &status(Some("http://127.0.0.1:9000")),
                "host.example.ts.net",
                &route
            ),
            RouteState::Conflict
        );
    }

    #[test]
    fn ownership_record_is_owner_only_and_versioned() {
        let directory =
            std::env::temp_dir().join(format!("boomux-tailscale-record-test-{}", Uuid::new_v4()));
        fs::create_dir(&directory).unwrap();
        let path = directory.join("record.json");
        let record = OwnershipRecord {
            version: OWNERSHIP_VERSION,
            dns_name: "host.example.ts.net".into(),
            routes: vec![OwnedRoute {
                https_port: 4097,
                target: "http://127.0.0.1:4097".into(),
            }],
        };

        write_record(&path, &record).unwrap();
        assert_eq!(fs::metadata(&path).unwrap().mode() & 0o777, 0o600);
        let loaded = read_record(&path).unwrap().unwrap();
        assert_eq!(loaded.version, OWNERSHIP_VERSION);
        assert_eq!(loaded.routes[0].https_port, 4097);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn exposure_preserves_compatible_routes_and_removes_only_created_routes() {
        let directory =
            std::env::temp_dir().join(format!("boomux-tailscale-command-test-{}", Uuid::new_v4()));
        fs::create_dir(&directory).unwrap();
        let executable = directory.join("tailscale");
        let marker = directory.join("opencode-enabled");
        let log = directory.join("commands.log");
        let script = format!(
            "#!/bin/sh\n\
             printf '%s\\n' \"$*\" >> '{log}'\n\
             case \"$*\" in\n\
               'status --json') printf '%s' '{{\"BackendState\":\"Running\",\"Self\":{{\"Online\":true,\"DNSName\":\"host.example.ts.net.\"}}}}' ;;\n\
               'serve status --json')\n\
                 if [ -e '{marker}' ]; then\n\
                   printf '%s' '{{\"TCP\":{{\"443\":{{\"HTTPS\":true}},\"4097\":{{\"HTTPS\":true}}}},\"Web\":{{\"host.example.ts.net:443\":{{\"Handlers\":{{\"/\":{{\"Proxy\":\"http://127.0.0.1:3737\"}}}}}},\"host.example.ts.net:4097\":{{\"Handlers\":{{\"/\":{{\"Proxy\":\"http://127.0.0.1:4097\"}}}}}}}}}}'\n\
                 else\n\
                   printf '%s' '{{\"TCP\":{{\"443\":{{\"HTTPS\":true}}}},\"Web\":{{\"host.example.ts.net:443\":{{\"Handlers\":{{\"/\":{{\"Proxy\":\"http://127.0.0.1:3737\"}}}}}}}}}}'\n\
                 fi ;;\n\
               'serve --bg --yes --https=4097 http://127.0.0.1:4097') : > '{marker}' ;;\n\
               'serve --https=4097 --set-path=/ off') rm -f '{marker}' ;;\n\
               *) exit 64 ;;\n\
             esac\n",
            log = log.display(),
            marker = marker.display(),
        );
        fs::write(&executable, script).unwrap();
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o755)).unwrap();
        let record = directory.join("ownership.json");

        let exposure =
            Exposure::enable_with(executable.as_os_str(), record.clone(), 3737, Some(4097))
                .unwrap();
        assert_eq!(exposure.dashboard_url(), "https://host.example.ts.net");
        assert!(marker.exists());
        assert!(record.exists());
        drop(exposure);

        assert!(!marker.exists());
        assert!(!record.exists());
        let commands = fs::read_to_string(log).unwrap();
        assert!(commands.contains("serve --bg --yes --https=4097 http://127.0.0.1:4097"));
        assert!(commands.contains("serve --https=4097 --set-path=/ off"));
        assert!(!commands.contains("--https=443 http://127.0.0.1:3737"));
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn exposure_without_opencode_publishes_only_the_dashboard() {
        let directory = std::env::temp_dir().join(format!(
            "boomux-tailscale-dashboard-only-test-{}",
            Uuid::new_v4()
        ));
        fs::create_dir(&directory).unwrap();
        let executable = directory.join("tailscale");
        let log = directory.join("commands.log");
        let marker = directory.join("dashboard-enabled");
        let script = format!(
            "#!/bin/sh\n\
             printf '%s\\n' \"$*\" >> '{log}'\n\
             case \"$*\" in\n\
               'status --json') printf '%s' '{{\"BackendState\":\"Running\",\"Self\":{{\"Online\":true,\"DNSName\":\"host.example.ts.net.\"}}}}' ;;\n\
               'serve status --json')\n\
                 if [ -e '{marker}' ]; then\n\
                   printf '%s' '{{\"TCP\":{{\"443\":{{\"HTTPS\":true}}}},\"Web\":{{\"host.example.ts.net:443\":{{\"Handlers\":{{\"/\":{{\"Proxy\":\"http://127.0.0.1:3737\"}}}}}}}}}}'\n\
                 else\n\
                   printf '%s' '{{\"TCP\":{{}},\"Web\":{{}}}}'\n\
                 fi ;;\n\
               'serve --bg --yes --https=443 http://127.0.0.1:3737') : > '{marker}' ;;\n\
               'serve --https=443 --set-path=/ off') rm -f '{marker}' ;;\n\
               *) exit 64 ;;\n\
             esac\n",
            log = log.display(),
            marker = marker.display(),
        );
        fs::write(&executable, script).unwrap();
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o755)).unwrap();
        let record = directory.join("ownership.json");

        let exposure = Exposure::enable_with(executable.as_os_str(), record, 3737, None).unwrap();
        assert_eq!(exposure.dashboard_url(), "https://host.example.ts.net");
        drop(exposure);

        let commands = fs::read_to_string(log).unwrap();
        assert!(commands.contains("serve --bg --yes --https=443 http://127.0.0.1:3737"));
        assert!(commands.contains("serve --https=443 --set-path=/ off"));
        assert!(!commands.contains("4097"));
        fs::remove_dir_all(directory).unwrap();
    }
}

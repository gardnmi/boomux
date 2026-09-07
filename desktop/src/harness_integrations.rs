//! Read-only local discovery and explicitly requested integration installation.
//! The matching CLI owns host probing, configuration validation, and writes.
use boomux::integrations::{self, IntegrationDescriptor};
use serde_json::Value;
use std::{
    collections::HashSet,
    io::Read,
    os::unix::process::CommandExt,
    process::{Command, Stdio},
};

const OUTPUT_LIMIT: u64 = 128 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Need {
    Install,
    Review,
}

#[derive(Clone, Copy, Debug)]
pub struct Suggestion {
    pub descriptor: &'static IntegrationDescriptor,
    pub need: Need,
}

#[derive(Default)]
pub struct Report {
    pub detected: usize,
    pub unavailable: usize,
    pub suggestions: Vec<Suggestion>,
}

#[derive(Default)]
pub struct Model {
    pub report: Report,
    pub checked: bool,
    pub busy: bool,
    pub installing: Option<&'static str>,
    pub dismissed: HashSet<&'static str>,
    pub confirm_replace: Option<&'static str>,
    pub message: Option<String>,
    pub error: Option<String>,
}

fn data(raw: &[u8], command: &str) -> Result<Value, String> {
    let envelope: Value = serde_json::from_slice(raw)
        .map_err(|_| "Boomux returned an invalid integration response".to_string())?;
    if envelope["schema"] != "boomux.cli/v1" || envelope["command"] != command {
        return Err("Boomux returned an unexpected integration response".into());
    }
    if let Some(message) = envelope["error"]["message"].as_str() {
        return Err(message.chars().take(2048).collect());
    }
    envelope
        .get("data")
        .cloned()
        .ok_or_else(|| "Missing integration response data".into())
}

fn parse_report(raw: &[u8]) -> Result<Report, String> {
    let data = data(raw, "integration.status")?;
    let rows = data["integrations"]
        .as_array()
        .filter(|rows| rows.len() <= 256)
        .ok_or("Invalid integration status list")?;
    let mut report = Report::default();
    let mut seen = HashSet::new();
    for row in rows {
        let name = row["name"].as_str().ok_or("Missing integration name")?;
        let Some(descriptor) =
            integrations::by_key(name).filter(|item| item.installation.is_some())
        else {
            continue;
        };
        if !seen.insert(descriptor.key) {
            return Err("Duplicate integration status".into());
        }
        match row["host"]["state"].as_str() {
            Some("missing") => continue,
            Some("available") => report.detected += 1,
            Some("probe_failed" | "not_checked") => {
                report.unavailable += 1;
                continue;
            }
            _ => return Err("Invalid harness detection status".into()),
        }
        let need = match row["asset"]["state"].as_str() {
            Some("missing") => Need::Install,
            // The CLI deliberately does not distinguish old releases from user edits.
            Some("modified") => Need::Review,
            Some("current") => continue,
            Some("unavailable") => {
                report.unavailable += 1;
                continue;
            }
            _ => return Err("Invalid integration asset status".into()),
        };
        report.suggestions.push(Suggestion { descriptor, need });
    }
    Ok(report)
}

// One background operation per window. timeout owns the process group; output
// and errors are bounded, stdin is closed, and no shell interpolation is used.
fn output(args: &[&str]) -> Result<Vec<u8>, String> {
    let mut child = Command::new("timeout")
        .args(["--kill-after=1s", "35s", "boomux"])
        .args(args)
        .process_group(0)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|error| error.to_string())?;
    let mut bytes = Vec::new();
    let read = child
        .stdout
        .take()
        .expect("piped stdout")
        .take(OUTPUT_LIMIT + 1)
        .read_to_end(&mut bytes);
    if read.is_err() || bytes.len() as u64 > OUTPUT_LIMIT {
        // This is only the process group created for the child above.
        unsafe {
            libc::kill(-(child.id() as i32), libc::SIGKILL);
        }
        let _ = child.wait();
        return Err("Integration command output could not be read within its limit".into());
    }
    let status = child.wait().map_err(|error| error.to_string())?;
    if !status.success() {
        let message = serde_json::from_slice::<Value>(&bytes)
            .ok()
            .and_then(|value| value["error"]["message"].as_str().map(str::to_owned))
            .map(|message| message.chars().take(2048).collect())
            .unwrap_or_else(|| {
                "Integration command failed or timed out. Check again in Settings.".into()
            });
        return Err(message);
    }
    Ok(bytes)
}

pub fn check() -> Result<Report, String> {
    parse_report(&output(&["integration", "status", "--json"])?)
}

pub fn install(suggestion: Suggestion, replace_confirmed: bool) -> Result<(), String> {
    if suggestion.need == Need::Review && !replace_confirmed {
        return Err("Confirm replacement of the existing Boomux integration first".into());
    }
    let mut args = vec![
        "integration",
        "install",
        suggestion.descriptor.key,
        "--json",
    ];
    if suggestion.need == Need::Review {
        args.push("--force");
    }
    parse_install_result(&output(&args)?, suggestion.descriptor.key)
}

fn parse_install_result(raw: &[u8], name: &str) -> Result<(), String> {
    let response = data(raw, "integration.install")?;
    let results = response["integrations"]
        .as_array()
        .ok_or("Missing installation result")?;
    if results.len() != 1
        || results[0]["name"].as_str() != Some(name)
        || !matches!(
            results[0]["result"].as_str(),
            Some("installed" | "replaced" | "unchanged")
        )
    {
        return Err(
            "Unexpected installation result; check integration status before retrying".into(),
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    fn status(rows: Value) -> Vec<u8> {
        serde_json::to_vec(&json!({"schema":"boomux.cli/v1", "command":"integration.status", "data":{"integrations":rows}})).unwrap()
    }
    #[test]
    fn only_detected_hosts_with_missing_or_different_assets_are_suggested() {
        let report = parse_report(&status(json!([
            {"name":"claude","host":{"state":"available"},"asset":{"state":"missing"}},
            {"name":"pi","host":{"state":"available"},"asset":{"state":"modified"}},
            {"name":"opencode","host":{"state":"missing"},"asset":{"state":"missing"}},
            {"name":"codex","host":{"state":"available"},"asset":{"state":"current"}},
            {"name":"kiro","host":{"state":"probe_failed"},"asset":{"state":"missing"}}
        ])))
        .unwrap();
        assert_eq!(report.detected, 3);
        assert_eq!(report.unavailable, 1);
        assert_eq!(report.suggestions.len(), 2);
        assert_eq!(report.suggestions[0].descriptor.key, "claude");
        assert_eq!(report.suggestions[0].need, Need::Install);
        assert_eq!(report.suggestions[1].need, Need::Review);
    }
    #[test]
    fn malformed_and_duplicate_statuses_do_not_report_success() {
        assert!(parse_report(b"{}").is_err());
        assert!(parse_report(&status(json!([{"name":"claude"}]))).is_err());
        let row = json!({"name":"pi","host":{"state":"available"},"asset":{"state":"current"}});
        assert!(parse_report(&status(json!([row.clone(), row]))).is_err());
    }
    #[test]
    fn installation_results_must_match_the_requested_integration() {
        for outcome in ["installed", "replaced", "unchanged"] {
            let raw = serde_json::to_vec(&json!({"schema":"boomux.cli/v1", "command":"integration.install", "data":{"integrations":[{"name":"pi", "result":outcome}]}})).unwrap();
            assert!(parse_install_result(&raw, "pi").is_ok());
            assert!(parse_install_result(&raw, "claude").is_err());
        }
        assert!(parse_install_result(b"{}", "pi").is_err());
    }

    #[test]
    fn replacement_requires_confirmation_before_running_a_command() {
        let suggestion = Suggestion {
            descriptor: &integrations::PI,
            need: Need::Review,
        };
        assert!(
            install(suggestion, false)
                .unwrap_err()
                .contains("Confirm replacement")
        );
    }
}

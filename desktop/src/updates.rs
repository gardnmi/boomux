//! Read-only release discovery. No installs or daemon lifecycle operations.
use semver::Version;
use serde_json::Value;
use std::{io::Read, process::Stdio};

const LIMIT: u64 = 128 * 1024;
#[cfg(target_os = "linux")]
const DESKTOP_ASSET: &str = "boomux-desktop-x86_64-unknown-linux-gnu.tar.gz";
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
const DESKTOP_ASSET: &str = "boomux-desktop-aarch64-apple-darwin.zip";
#[cfg(all(target_os = "macos", target_arch = "x86_64"))]
const DESKTOP_ASSET: &str = "boomux-desktop-x86_64-apple-darwin.zip";

#[derive(Clone, Debug, PartialEq)]
pub struct Notice {
    pub current: String,
    pub latest: String,
    pub url: String,
}

impl Notice {
    pub fn visible(&self, dismissed: &str) -> bool {
        self.latest != dismissed
    }
}

#[derive(Default)]
pub struct Check {
    pub desktop: Option<Notice>,
    pub boomux: Option<Notice>,
    pub unavailable: bool,
    pub installable: bool,
    pub prepared: Option<crate::bundle_update::Prepared>,
}

impl Check {
    /// One release notice, even when a development install checks both binaries.
    pub fn release_notice(
        &self,
        desktop_dismissed: &str,
        boomux_dismissed: &str,
    ) -> Option<&Notice> {
        if self.prepared.is_some() {
            return None;
        }
        self.boomux
            .iter()
            .chain(self.desktop.iter())
            .max_by_key(|notice| Version::parse(&notice.latest).ok())
            .filter(|notice| notice.visible(desktop_dismissed) && notice.visible(boomux_dismissed))
    }

    pub fn version_summary(&self, notice: &Notice) -> String {
        if let (Some(desktop), Some(boomux)) = (&self.desktop, &self.boomux)
            && desktop.current != boomux.current
        {
            return format!(
                "Desktop {} · CLI {} → {}",
                desktop.current, boomux.current, notice.latest
            );
        }
        format!("{} → {}", notice.current, notice.latest)
    }
}

pub fn valid_dismissal(text: &str) -> bool {
    text.is_empty() || (text.len() <= 64 && Version::parse(text).is_ok())
}

fn notice(current: &str, tag: &str, repository: &str) -> Option<Notice> {
    if current.len() > 64 || tag.len() > 64 {
        return None;
    }
    let current = Version::parse(current).ok()?;
    let latest = Version::parse(tag.strip_prefix('v').unwrap_or(tag)).ok()?;
    if !latest.pre.is_empty() || !latest.cmp_precedence(&current).is_gt() {
        return None;
    }
    Some(Notice {
        current: current.to_string(),
        latest: latest.to_string(),
        url: format!("https://github.com/gardnmi/{repository}/releases/tag/{tag}"),
    })
}

fn desktop_release(raw: &[u8], current: &str) -> Option<Notice> {
    let release: Value = serde_json::from_slice(raw).ok()?;
    if release.get("draft")?.as_bool()? || release.get("prerelease")?.as_bool()? {
        return None;
    }
    let assets = release.get("assets")?.as_array()?;
    for name in [DESKTOP_ASSET.to_owned(), format!("{DESKTOP_ASSET}.sha256")] {
        if !assets
            .iter()
            .any(|asset| asset["name"].as_str() == Some(&name))
        {
            return None;
        }
    }
    notice(current, release.get("tag_name")?.as_str()?, "boomux")
}

fn boomux_status(raw: &[u8]) -> Option<Notice> {
    let envelope: Value = serde_json::from_slice(raw).ok()?;
    if envelope.get("schema")?.as_str()? != "boomux.cli/v1"
        || envelope.get("command")?.as_str()? != "update.status"
    {
        return None;
    }
    let data = envelope.get("data")?;
    let current = data.get("current")?.as_str()?;
    let latest = data.get("latest")?.as_str()?;
    notice(current, &format!("v{latest}"), "boomux")
}

// Both process lifetime and retained output are bounded. Call only on a worker.
fn output(program: &str, args: &[&str]) -> Option<Vec<u8>> {
    let mut child = crate::subprocess::command(20, program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let mut bytes = Vec::new();
    let read = child.stdout.take()?.take(LIMIT + 1).read_to_end(&mut bytes);
    let status = child.wait().ok()?;
    (read.is_ok() && status.success() && bytes.len() as u64 <= LIMIT).then_some(bytes)
}

pub fn check() -> Check {
    let bundled = std::env::current_exe()
        .ok()
        .and_then(|path| path.parent()?.parent().map(|p| p.join("release.txt")))
        .is_some_and(|path| path.is_file());
    let desktop_raw = output(
        "curl",
        &[
            "--disable",
            "--fail",
            "--silent",
            "--show-error",
            "--location",
            "--proto",
            "=https",
            "--proto-redir",
            "=https",
            "--connect-timeout",
            "5",
            "--max-time",
            "15",
            "--max-filesize",
            "131072",
            "--header",
            "Accept: application/vnd.github+json",
            "--user-agent",
            "boomux-desktop-update-check",
            "https://api.github.com/repos/gardnmi/boomux/releases/latest",
        ],
    );
    // Official bundles have one release and must never offer component updates.
    let boomux_raw = (!bundled)
        .then(|| output("boomux", &["--json", "update", "status"]))
        .flatten();
    let desktop = desktop_raw
        .as_deref()
        .and_then(|raw| desktop_release(raw, env!("CARGO_PKG_VERSION")));
    let boomux = boomux_raw.as_deref().and_then(boomux_status);
    let unavailable = desktop_raw
        .as_deref()
        .and_then(|raw| serde_json::from_slice::<Value>(raw).ok())
        .is_none_or(|value| !value["tag_name"].is_string())
        || (!bundled
            && boomux_raw
                .as_deref()
                .and_then(|raw| serde_json::from_slice::<Value>(raw).ok())
                .is_none_or(|value| !value["data"]["latest"].is_string()));
    let installation = crate::bundle_update::Installation::discover();
    let prepared = installation.as_ref().and_then(|installation| {
        installation
            .pending()
            .ok()
            .flatten()
            .or_else(|| installation.finish_installation().ok().flatten())
    });
    Check {
        installable: installation.is_some(),
        prepared,
        desktop,
        boomux,
        unavailable,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn release_notice_unifies_components_and_honors_either_saved_dismissal() {
        let check = Check {
            desktop: notice("1.10.0", "v1.11.1", "boomux"),
            boomux: notice("1.10.0", "v1.11.1", "boomux"),
            ..Check::default()
        };
        let release = check.release_notice("", "").unwrap();
        assert_eq!(check.version_summary(release), "1.10.0 → 1.11.1");
        assert!(check.release_notice("1.11.1", "").is_none());
        assert!(check.release_notice("", "1.11.1").is_none());
        assert!(check.release_notice("1.11.0", "1.11.0").is_some());
    }

    #[test]
    fn release_notice_retains_partial_checks_and_different_component_versions() {
        let mut check = Check {
            boomux: notice("1.9.0", "v1.12.0", "boomux"),
            ..Check::default()
        };
        assert_eq!(check.release_notice("", "").unwrap().latest, "1.12.0");
        check.desktop = notice("1.10.0", "v1.11.1", "boomux");
        let release = check.release_notice("1.11.1", "").unwrap();
        assert_eq!(release.latest, "1.12.0");
        assert_eq!(
            check.version_summary(release),
            "Desktop 1.10.0 · CLI 1.9.0 → 1.12.0"
        );
        assert!(check.release_notice("", "1.12.0").is_none());
        check.boomux = None;
        assert_eq!(check.release_notice("", "").unwrap().latest, "1.11.1");
    }
    #[test]
    fn only_newer_stable_releases_are_offered() {
        for tag in ["v0.1.0", "v0.0.9", "v0.2.0-beta.1", "bad", "v0.1.0+rebuild"] {
            assert!(notice("0.1.0", tag, "boomux-desktop").is_none());
        }
        assert!(notice("0.9.0", "v0.10.0", "boomux-desktop").is_some());
        assert!(
            desktop_release(
                br#"{"tag_name":"v0.2.0","draft":true,"prerelease":false}"#,
                "0.1.0"
            )
            .is_none()
        );
        assert!(desktop_release(b"invalid", "0.1.0").is_none());
    }
    #[test]
    fn dismissal_is_version_specific_and_urls_are_fixed_to_the_owner() {
        let raw = serde_json::to_vec(&serde_json::json!({
            "tag_name": "v0.2.0", "draft": false, "prerelease": false,
            "html_url": "https://untrusted.example",
            "assets": [{"name": DESKTOP_ASSET}, {"name": format!("{DESKTOP_ASSET}.sha256")}]
        }))
        .unwrap();
        let update = desktop_release(&raw, "0.1.0").unwrap();
        assert!(!update.visible("0.2.0"));
        assert!(update.visible("0.1.1"));
        assert_eq!(
            update.url,
            "https://github.com/gardnmi/boomux/releases/tag/v0.2.0"
        );
        assert!(!valid_dismissal("unbounded or malformed text"));
    }
    #[test]
    fn releases_without_the_complete_desktop_bundle_are_not_offered() {
        for assets in [
            serde_json::json!([]),
            serde_json::json!([{"name": DESKTOP_ASSET}]),
        ] {
            let raw = serde_json::to_vec(&serde_json::json!({
                "tag_name": "v2.0.0", "draft": false, "prerelease": false, "assets": assets
            }))
            .unwrap();
            assert!(desktop_release(&raw, "1.9.7").is_none());
        }
    }
    #[test]
    fn boomux_discovery_requires_the_supported_envelope() {
        let raw = br#"{"schema":"boomux.cli/v1","command":"update.status","data":{"current":"1.9.5","latest":"1.10.0","install_kind":"source_build"}}"#;
        assert_eq!(boomux_status(raw).unwrap().latest, "1.10.0");
        assert!(boomux_status(br#"{"schema":"unknown","command":"update.status","data":{"current":"1.9.5","latest":"1.10.0"}}"#).is_none());
        assert!(boomux_status(br#"{"schema":"boomux.cli/v1","command":"update.status","data":{"current":"1.9.5","latest":null}}"#).is_none());
    }
}

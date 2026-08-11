use std::env;
use std::error::Error;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

use serde::Deserialize;

const DEFAULT_PROJECT_SEARCH_DEPTH: usize = 3;
const MAX_PROJECT_SEARCH_DEPTH: usize = 10;

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct RawConfig {
    terminal: Option<String>,
    projects: Option<RawProjectsConfig>,
    notifications: Option<RawNotificationsConfig>,
    dashboard: Option<RawDashboardConfig>,
    recovery: Option<RawRecoveryConfig>,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct RawProjectsConfig {
    roots: Option<Vec<String>>,
    max_depth: Option<usize>,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct RawNotificationsConfig {
    enabled: Option<bool>,
    blocked: Option<bool>,
    completed: Option<bool>,
    sound: Option<RawNotificationSoundConfig>,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct RawNotificationSoundConfig {
    enabled: Option<bool>,
    blocked: Option<String>,
    completed: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct RawDashboardConfig {
    follow_focused_terminal: Option<bool>,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct RawRecoveryConfig {
    resume_agents: Option<bool>,
    persist_terminal_history: Option<bool>,
}

#[derive(Debug)]
pub(crate) struct Config {
    pub(crate) terminal: Option<String>,
    pub(crate) projects: ProjectsConfig,
    pub(crate) path: Option<PathBuf>,
    pub(crate) notifications: boomux::daemon::NotificationDeliverySettings,
    pub(crate) dashboard: DashboardConfig,
}

#[derive(Debug)]
pub(crate) struct ProjectsConfig {
    pub(crate) roots: Vec<PathBuf>,
    pub(crate) max_depth: usize,
}

#[derive(Debug)]
pub(crate) struct DashboardConfig {
    pub(crate) follow_focused_terminal: bool,
}

#[derive(Debug)]
struct ConfigError(String);

impl fmt::Display for ConfigError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl Error for ConfigError {}

pub(crate) fn load() -> Result<Config, Box<dyn Error>> {
    let (raw, loaded_path) = load_raw()?;
    resolve(raw, loaded_path)
}

pub(crate) fn load_notification_settings()
-> Result<boomux::daemon::NotificationDeliverySettings, Box<dyn Error>> {
    let (raw, _) = load_raw()?;
    Ok(resolve_daemon_settings(raw.notifications, raw.recovery))
}

fn load_raw() -> Result<(RawConfig, Option<PathBuf>), Box<dyn Error>> {
    let global_path = global_config_path();
    let mut raw = RawConfig::default();
    let mut loaded_path = None;

    if let Some(path) = global_path.as_deref()
        && path.is_file()
    {
        merge(&mut raw, read(path)?);
        loaded_path = Some(path.to_owned());
    }

    if let Some(path) = env::var_os("BOOMUX_CONFIG") {
        let path = PathBuf::from(path);
        if path.as_os_str().is_empty() {
            return Err(ConfigError("BOOMUX_CONFIG cannot be empty".into()).into());
        }
        merge(&mut raw, read(&path)?);
        loaded_path = Some(path);
    }

    Ok((raw, loaded_path))
}

pub(crate) fn global_config_path() -> Option<PathBuf> {
    env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .or_else(|| {
            env::var_os("HOME")
                .map(PathBuf::from)
                .filter(|path| path.is_absolute())
                .map(|home| home.join(".config"))
        })
        .map(|directory| directory.join("boomux/config.toml"))
}

fn read(path: &Path) -> Result<RawConfig, Box<dyn Error>> {
    let contents = fs::read_to_string(path)
        .map_err(|error| ConfigError(format!("could not read {}: {error}", path.display())))?;
    toml::from_str(&contents)
        .map_err(|error| ConfigError(format!("invalid config {}: {error}", path.display())).into())
}

fn merge(base: &mut RawConfig, next: RawConfig) {
    if next.terminal.is_some() {
        base.terminal = next.terminal;
    }
    if let Some(next_projects) = next.projects {
        let projects = base.projects.get_or_insert_default();
        if next_projects.roots.is_some() {
            projects.roots = next_projects.roots;
        }
        if next_projects.max_depth.is_some() {
            projects.max_depth = next_projects.max_depth;
        }
    }
    if let Some(next_notifications) = next.notifications {
        let notifications = base.notifications.get_or_insert_default();
        if next_notifications.enabled.is_some() {
            notifications.enabled = next_notifications.enabled;
        }
        if next_notifications.blocked.is_some() {
            notifications.blocked = next_notifications.blocked;
        }
        if next_notifications.completed.is_some() {
            notifications.completed = next_notifications.completed;
        }
        if let Some(next_sound) = next_notifications.sound {
            let sound = notifications.sound.get_or_insert_default();
            if next_sound.enabled.is_some() {
                sound.enabled = next_sound.enabled;
            }
            if next_sound.blocked.is_some() {
                sound.blocked = next_sound.blocked;
            }
            if next_sound.completed.is_some() {
                sound.completed = next_sound.completed;
            }
        }
    }
    if let Some(next_dashboard) = next.dashboard {
        let dashboard = base.dashboard.get_or_insert_default();
        if next_dashboard.follow_focused_terminal.is_some() {
            dashboard.follow_focused_terminal = next_dashboard.follow_focused_terminal;
        }
    }
    if let Some(next_recovery) = next.recovery {
        let recovery = base.recovery.get_or_insert_default();
        if next_recovery.resume_agents.is_some() {
            recovery.resume_agents = next_recovery.resume_agents;
        }
        if next_recovery.persist_terminal_history.is_some() {
            recovery.persist_terminal_history = next_recovery.persist_terminal_history;
        }
    }
}

fn resolve(raw: RawConfig, path: Option<PathBuf>) -> Result<Config, Box<dyn Error>> {
    let terminal = raw
        .terminal
        .map(|terminal| terminal.trim().to_owned())
        .map(|terminal| {
            crate::terminal::validate_desktop_entry(&terminal)?;
            Ok::<_, Box<dyn Error>>(terminal)
        })
        .transpose()?;
    let projects = raw.projects.unwrap_or_default();
    let max_depth = projects.max_depth.unwrap_or(DEFAULT_PROJECT_SEARCH_DEPTH);
    if !(1..=MAX_PROJECT_SEARCH_DEPTH).contains(&max_depth) {
        return Err(ConfigError(format!(
            "projects.max_depth must be between 1 and {MAX_PROJECT_SEARCH_DEPTH}"
        ))
        .into());
    }

    let roots = projects
        .roots
        .unwrap_or_default()
        .into_iter()
        .map(|root| expand_root(&root))
        .collect::<Result<_, _>>()?;
    Ok(Config {
        terminal,
        projects: ProjectsConfig { roots, max_depth },
        path,
        notifications: resolve_daemon_settings(raw.notifications, raw.recovery),
        dashboard: DashboardConfig {
            follow_focused_terminal: raw
                .dashboard
                .unwrap_or_default()
                .follow_focused_terminal
                .unwrap_or(true),
        },
    })
}

#[cfg(test)]
fn resolve_notifications(
    raw: Option<RawNotificationsConfig>,
) -> boomux::daemon::NotificationDeliverySettings {
    resolve_daemon_settings(raw, None)
}

fn resolve_daemon_settings(
    notifications: Option<RawNotificationsConfig>,
    recovery: Option<RawRecoveryConfig>,
) -> boomux::daemon::NotificationDeliverySettings {
    let raw = notifications.unwrap_or_default();
    let recovery = recovery.unwrap_or_default();
    boomux::daemon::NotificationDeliverySettings {
        desktop: boomux::daemon::NotificationSettings {
            enabled: raw.enabled.unwrap_or(false),
            blocked: raw.blocked.unwrap_or(true),
            completed: raw.completed.unwrap_or(true),
        },
        sound: raw.sound.map_or_else(Default::default, |sound| {
            boomux::daemon::NotificationSoundSettings {
                enabled: sound.enabled.unwrap_or(false),
                blocked: sound
                    .blocked
                    .unwrap_or_else(|| "message-new-instant".into()),
                completed: sound.completed.unwrap_or_else(|| "complete".into()),
            }
        }),
        resume_agents: recovery.resume_agents.unwrap_or(true),
        persist_terminal_history: recovery.persist_terminal_history.unwrap_or(false),
    }
}

fn expand_root(root: &str) -> Result<PathBuf, Box<dyn Error>> {
    let path = if root == "~" {
        home_directory()?
    } else if let Some(relative) = root.strip_prefix("~/") {
        home_directory()?.join(relative)
    } else {
        PathBuf::from(root)
    };
    if !path.is_absolute() {
        return Err(ConfigError(format!(
            "project root must be absolute or start with ~: {root}"
        ))
        .into());
    }
    Ok(path)
}

fn home_directory() -> Result<PathBuf, Box<dyn Error>> {
    env::var_os("HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .ok_or_else(|| ConfigError("HOME must be an absolute path to expand ~".into()).into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_project_settings() {
        let raw: RawConfig = toml::from_str(
            r#"
                [projects]
                roots = ["/tmp/projects"]
                max_depth = 4
            "#,
        )
        .expect("valid config");
        let config = resolve(raw, None).expect("resolved config");

        assert_eq!(config.projects.roots, [PathBuf::from("/tmp/projects")]);
        assert_eq!(config.projects.max_depth, 4);
    }

    #[test]
    fn parses_terminal_preference() {
        let raw: RawConfig =
            toml::from_str(r#"terminal = "Alacritty.desktop""#).expect("valid config");

        let config = resolve(raw, None).expect("resolved config");

        assert_eq!(config.terminal.as_deref(), Some("Alacritty.desktop"));
    }

    #[test]
    fn dashboard_follows_focused_terminals_by_default_and_can_be_disabled() {
        let default = resolve(RawConfig::default(), None).expect("resolved default config");
        assert!(default.dashboard.follow_focused_terminal);

        let raw: RawConfig =
            toml::from_str("[dashboard]\nfollow_focused_terminal = false").expect("valid config");
        let config = resolve(raw, None).expect("resolved config");
        assert!(!config.dashboard.follow_focused_terminal);
    }

    #[test]
    fn dashboard_setting_merges_independently() {
        let mut base: RawConfig = toml::from_str(
            "[dashboard]\nfollow_focused_terminal = false\n[projects]\nmax_depth = 2",
        )
        .expect("valid base");
        let next: RawConfig =
            toml::from_str("[dashboard]\nfollow_focused_terminal = true").expect("valid override");

        merge(&mut base, next);
        let config = resolve(base, None).expect("resolved config");
        assert!(config.dashboard.follow_focused_terminal);
        assert_eq!(config.projects.max_depth, 2);
        assert!(toml::from_str::<RawConfig>("[dashboard]\nunknown = true").is_err());
    }

    #[test]
    fn terminal_preference_can_be_overridden() {
        let mut base: RawConfig =
            toml::from_str(r#"terminal = "Alacritty.desktop""#).expect("valid base");
        let next: RawConfig = toml::from_str(r#"terminal = "com.mitchellh.ghostty.desktop""#)
            .expect("valid override");

        merge(&mut base, next);
        let config = resolve(base, None).expect("resolved config");

        assert_eq!(
            config.terminal.as_deref(),
            Some("com.mitchellh.ghostty.desktop")
        );
    }

    #[test]
    fn rejects_invalid_terminal_desktop_entries() {
        for terminal in ["", "Alacritty", "-Alacritty.desktop", "bad\n.desktop"] {
            let raw: RawConfig =
                toml::from_str(&format!("terminal = {terminal:?}")).expect("valid TOML");

            assert!(resolve(raw, None).is_err());
        }
    }

    #[test]
    fn override_merges_only_specified_project_fields() {
        let mut base: RawConfig = toml::from_str(
            r#"
                [projects]
                roots = ["/tmp/projects"]
                max_depth = 2
            "#,
        )
        .expect("valid base");
        let next: RawConfig = toml::from_str(
            r#"
                [projects]
                max_depth = 5
            "#,
        )
        .expect("valid override");

        merge(&mut base, next);
        let config = resolve(base, None).expect("resolved config");

        assert_eq!(config.projects.roots, [PathBuf::from("/tmp/projects")]);
        assert_eq!(config.projects.max_depth, 5);
    }

    #[test]
    fn rejects_unknown_settings_and_relative_roots() {
        assert!(toml::from_str::<RawConfig>("unknown = true").is_err());

        let raw: RawConfig = toml::from_str(
            r#"
                [projects]
                roots = ["Projects"]
            "#,
        )
        .expect("valid TOML");
        assert!(resolve(raw, None).is_err());
    }

    #[test]
    fn rejects_unbounded_project_depth() {
        let raw: RawConfig = toml::from_str(
            r#"
                [projects]
                max_depth = 11
            "#,
        )
        .expect("valid TOML");

        assert!(resolve(raw, None).is_err());
    }

    #[test]
    fn notification_settings_default_and_parse() {
        assert_eq!(
            resolve_notifications(None),
            boomux::daemon::NotificationDeliverySettings::default()
        );
        let raw: RawConfig = toml::from_str(
            "[notifications]\nenabled = true\nblocked = false\ncompleted = false\n[notifications.sound]\nenabled = true\nblocked = \"dialog-warning\"",
        )
        .unwrap();
        let settings = resolve_notifications(raw.notifications);
        assert!(settings.desktop.enabled);
        assert!(!settings.desktop.blocked);
        assert!(!settings.desktop.completed);
        assert!(settings.sound.enabled);
        assert_eq!(settings.sound.blocked, "dialog-warning");
        assert_eq!(settings.sound.completed, "complete");
    }

    #[test]
    fn recovery_settings_default_and_merge_per_field() {
        let defaults = resolve_daemon_settings(None, None);
        assert!(defaults.resume_agents);
        assert!(!defaults.persist_terminal_history);

        let mut base: RawConfig =
            toml::from_str("[recovery]\nresume_agents = false\npersist_terminal_history = true")
                .unwrap();
        let next: RawConfig = toml::from_str("[recovery]\nresume_agents = true").unwrap();
        merge(&mut base, next);

        let settings = resolve_daemon_settings(base.notifications, base.recovery);
        assert!(settings.resume_agents);
        assert!(settings.persist_terminal_history);
        assert!(toml::from_str::<RawConfig>("[recovery]\nunknown = true").is_err());
    }

    #[test]
    fn notification_overrides_merge_per_field() {
        let mut base: RawConfig =
            toml::from_str("[notifications]\nenabled = true\nblocked = false\ncompleted = false")
                .unwrap();
        let next = toml::from_str(
            "[notifications]\ncompleted = true\n[notifications.sound]\nenabled = true\ncompleted = \"service-login\"",
        )
        .unwrap();
        merge(&mut base, next);

        let settings = resolve_notifications(base.notifications);
        assert_eq!(
            settings,
            boomux::daemon::NotificationDeliverySettings {
                desktop: boomux::daemon::NotificationSettings {
                    enabled: true,
                    blocked: false,
                    completed: true,
                },
                sound: boomux::daemon::NotificationSoundSettings {
                    enabled: true,
                    blocked: "message-new-instant".into(),
                    completed: "service-login".into(),
                },
                resume_agents: true,
                persist_terminal_history: false,
            }
        );
    }

    #[test]
    fn rejects_unknown_notification_settings() {
        assert!(toml::from_str::<RawConfig>("[notifications]\nunknown = true").is_err());
        assert!(toml::from_str::<RawConfig>("[notifications.sound]\nunknown = true").is_err());
    }

    #[test]
    fn notification_resolution_ignores_unrelated_semantic_errors() {
        let raw: RawConfig = toml::from_str(
            "terminal = \"invalid\"\n[projects]\nroots = [\"relative\"]\n[notifications]\nenabled = true",
        )
        .unwrap();
        assert!(resolve(raw.clone(), None).is_err());
        assert!(resolve_notifications(raw.notifications).desktop.enabled);
    }
}

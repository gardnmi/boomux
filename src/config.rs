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
}

#[derive(Debug)]
pub(crate) struct Config {
    pub(crate) terminal: Option<String>,
    pub(crate) projects: ProjectsConfig,
    pub(crate) path: Option<PathBuf>,
    pub(crate) notifications: boomux::daemon::NotificationSettings,
}

#[derive(Debug)]
pub(crate) struct ProjectsConfig {
    pub(crate) roots: Vec<PathBuf>,
    pub(crate) max_depth: usize,
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
-> Result<boomux::daemon::NotificationSettings, Box<dyn Error>> {
    let (raw, _) = load_raw()?;
    Ok(resolve_notifications(raw.notifications))
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
        notifications: resolve_notifications(raw.notifications),
    })
}

fn resolve_notifications(
    raw: Option<RawNotificationsConfig>,
) -> boomux::daemon::NotificationSettings {
    let raw = raw.unwrap_or_default();
    boomux::daemon::NotificationSettings {
        enabled: raw.enabled.unwrap_or(false),
        blocked: raw.blocked.unwrap_or(true),
        completed: raw.completed.unwrap_or(true),
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
            boomux::daemon::NotificationSettings::default()
        );
        let raw: RawConfig =
            toml::from_str("[notifications]\nenabled = true\nblocked = false\ncompleted = false")
                .unwrap();
        let settings = resolve_notifications(raw.notifications);
        assert!(settings.enabled);
        assert!(!settings.blocked);
        assert!(!settings.completed);
    }

    #[test]
    fn notification_overrides_merge_per_field() {
        let mut base: RawConfig =
            toml::from_str("[notifications]\nenabled = true\nblocked = false\ncompleted = false")
                .unwrap();
        let next = toml::from_str("[notifications]\ncompleted = true").unwrap();
        merge(&mut base, next);

        let settings = resolve_notifications(base.notifications);
        assert_eq!(
            settings,
            boomux::daemon::NotificationSettings {
                enabled: true,
                blocked: false,
                completed: true,
            }
        );
    }

    #[test]
    fn rejects_unknown_notification_settings() {
        assert!(toml::from_str::<RawConfig>("[notifications]\nunknown = true").is_err());
    }

    #[test]
    fn notification_resolution_ignores_unrelated_semantic_errors() {
        let raw: RawConfig = toml::from_str(
            "terminal = \"invalid\"\n[projects]\nroots = [\"relative\"]\n[notifications]\nenabled = true",
        )
        .unwrap();
        assert!(resolve(raw.clone(), None).is_err());
        assert!(resolve_notifications(raw.notifications).enabled);
    }
}

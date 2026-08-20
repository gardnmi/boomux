use std::env;
use std::error::Error;
use std::ffi::{CString, OsStr, OsString};
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};

use serde::Deserialize;
use toml_edit::{Array, DocumentMut, Item, Table, TableLike, Value};

const DEFAULT_PROJECT_SEARCH_DEPTH: usize = 3;
const MAX_PROJECT_SEARCH_DEPTH: usize = 10;
const DEFAULT_MAX_SCHEDULED_EXECUTION_CONCURRENCY: u16 = 4;
const MAX_SCHEDULED_EXECUTION_CONCURRENCY: i64 = 64;
const MAX_CONFIG_BYTES: u64 = 1024 * 1024;
const TEMPORARY_CREATE_ATTEMPTS: u8 = 16;

pub(crate) const CONFIG_TEMPLATE: &str = r#"# Boomux configuration
# Uncomment settings to override their defaults.

# XDG desktop entry used when Boomux opens terminal windows.
# terminal = "Alacritty.desktop"

[projects]
# roots = ["~/Projects"]
# max_depth = 3

[dashboard]
# follow_focused_terminal = true

[notifications]
# enabled = false
# blocked = true
# completed = true
# scheduled_dispatch_failed = false
# scheduled_interrupted = false

[notifications.sound]
# enabled = false
# blocked = "message-new-instant"
# completed = "complete"
# scheduled_dispatch_failed = "dialog-warning"
# scheduled_interrupted = "dialog-warning"

[recovery]
# resume_agents = true
# persist_terminal_history = false

[scheduling]
# max_concurrent = 4
"#;

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct RawConfig {
    terminal: Option<String>,
    projects: Option<RawProjectsConfig>,
    notifications: Option<RawNotificationsConfig>,
    dashboard: Option<RawDashboardConfig>,
    recovery: Option<RawRecoveryConfig>,
    scheduling: Option<RawSchedulingConfig>,
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
    scheduled_dispatch_failed: Option<bool>,
    scheduled_interrupted: Option<bool>,
    sound: Option<RawNotificationSoundConfig>,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct RawNotificationSoundConfig {
    enabled: Option<bool>,
    blocked: Option<String>,
    completed: Option<String>,
    scheduled_dispatch_failed: Option<String>,
    scheduled_interrupted: Option<String>,
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

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct RawSchedulingConfig {
    max_concurrent: Option<i64>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Config {
    pub(crate) terminal: Option<String>,
    pub(crate) projects: ProjectsConfig,
    pub(crate) path: Option<PathBuf>,
    pub(crate) notifications: boomux::daemon::NotificationDeliverySettings,
    pub(crate) dashboard: DashboardConfig,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ProjectsConfig {
    pub(crate) roots: Vec<PathBuf>,
    pub(crate) max_depth: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct DashboardConfig {
    pub(crate) follow_focused_terminal: bool,
}

#[derive(Debug)]
struct ConfigError(String);

#[derive(Debug)]
struct ConfigCommittedError(String);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ConfigSource {
    Default,
    Global,
    Environment,
}

impl ConfigSource {
    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::Global => "global",
            Self::Environment => "BOOMUX_CONFIG",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ConfigLayer {
    pub(crate) source: ConfigSource,
    pub(crate) path: PathBuf,
    pub(crate) loaded: bool,
}

#[derive(Clone, Debug)]
pub(crate) struct ConfigSnapshot {
    pub(crate) effective: Config,
    pub(crate) active_path: PathBuf,
    pub(crate) active_source: ConfigSource,
    pub(crate) layers: Vec<ConfigLayer>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ConfigValues {
    pub(crate) terminal: Option<String>,
    pub(crate) project_roots: Vec<String>,
    pub(crate) project_max_depth: usize,
    pub(crate) follow_focused_terminal: bool,
    pub(crate) resume_agents: bool,
    pub(crate) persist_terminal_history: bool,
    pub(crate) scheduling_max_concurrent: u16,
    pub(crate) notifications_enabled: bool,
    pub(crate) notifications_blocked: bool,
    pub(crate) notifications_completed: bool,
    pub(crate) notifications_scheduled_dispatch_failed: bool,
    pub(crate) notifications_scheduled_interrupted: bool,
    pub(crate) sound_enabled: bool,
    pub(crate) sound_blocked: String,
    pub(crate) sound_completed: String,
    pub(crate) sound_scheduled_dispatch_failed: String,
    pub(crate) sound_scheduled_interrupted: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct ConfigOverrides {
    pub(crate) terminal: Option<String>,
    pub(crate) project_roots: Option<Vec<String>>,
    pub(crate) project_max_depth: Option<usize>,
    pub(crate) follow_focused_terminal: Option<bool>,
    pub(crate) resume_agents: Option<bool>,
    pub(crate) persist_terminal_history: Option<bool>,
    pub(crate) scheduling_max_concurrent: Option<u16>,
    pub(crate) notifications_enabled: Option<bool>,
    pub(crate) notifications_blocked: Option<bool>,
    pub(crate) notifications_completed: Option<bool>,
    pub(crate) notifications_scheduled_dispatch_failed: Option<bool>,
    pub(crate) notifications_scheduled_interrupted: Option<bool>,
    pub(crate) sound_enabled: Option<bool>,
    pub(crate) sound_blocked: Option<String>,
    pub(crate) sound_completed: Option<String>,
    pub(crate) sound_scheduled_dispatch_failed: Option<String>,
    pub(crate) sound_scheduled_interrupted: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ConfigSettings {
    pub(crate) active_path: PathBuf,
    pub(crate) active_source: ConfigSource,
    pub(crate) inherited_source: ConfigSource,
    pub(crate) inherited: ConfigValues,
    pub(crate) inherited_overrides: ConfigOverrides,
    pub(crate) effective: ConfigValues,
    pub(crate) overrides: ConfigOverrides,
    baseline: Option<Vec<u8>>,
    inherited_baseline: Option<Vec<u8>>,
}

impl ConfigValues {
    pub(crate) fn daemon_settings(&self) -> boomux::daemon::NotificationDeliverySettings {
        boomux::daemon::NotificationDeliverySettings {
            desktop: boomux::daemon::NotificationSettings {
                enabled: self.notifications_enabled,
                blocked: self.notifications_blocked,
                completed: self.notifications_completed,
                scheduled_dispatch_failed: self.notifications_scheduled_dispatch_failed,
                scheduled_interrupted: self.notifications_scheduled_interrupted,
            },
            sound: boomux::daemon::NotificationSoundSettings {
                enabled: self.sound_enabled,
                blocked: self.sound_blocked.clone(),
                completed: self.sound_completed.clone(),
                scheduled_dispatch_failed: self.sound_scheduled_dispatch_failed.clone(),
                scheduled_interrupted: self.sound_scheduled_interrupted.clone(),
            },
            resume_agents: self.resume_agents,
            persist_terminal_history: self.persist_terminal_history,
            max_scheduled_execution_concurrency: self.scheduling_max_concurrent,
        }
    }
}

impl ConfigSettings {
    pub(crate) fn effective_config(&self) -> Result<Config, Box<dyn Error>> {
        Ok(Config {
            terminal: self.effective.terminal.clone(),
            projects: ProjectsConfig {
                roots: self
                    .effective
                    .project_roots
                    .iter()
                    .map(|root| expand_root(root))
                    .collect::<Result<_, _>>()?,
                max_depth: self.effective.project_max_depth,
            },
            path: Some(self.active_path.clone()),
            notifications: self.effective.daemon_settings(),
            dashboard: DashboardConfig {
                follow_focused_terminal: self.effective.follow_focused_terminal,
            },
        })
    }
}

impl fmt::Display for ConfigError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl Error for ConfigError {}

impl fmt::Display for ConfigCommittedError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl Error for ConfigCommittedError {}

pub(crate) fn load() -> Result<Config, Box<dyn Error>> {
    let (raw, loaded_path) = load_raw()?;
    resolve(raw, loaded_path)
}

pub(crate) fn load_snapshot() -> Result<ConfigSnapshot, Box<dyn Error>> {
    let paths = ConfigPaths::from_environment()?;
    let (raw, loaded_path, layers) = load_raw_from_paths(&paths, None)?;
    Ok(ConfigSnapshot {
        effective: resolve(raw, loaded_path)?,
        active_path: paths.active()?.to_owned(),
        active_source: paths.active_source(),
        layers,
    })
}

pub(crate) fn load_settings() -> Result<ConfigSettings, Box<dyn Error>> {
    let paths = ConfigPaths::from_environment()?;
    let active_path = paths.active()?.to_owned();
    let baseline = match fs::metadata(&active_path) {
        Ok(_) => Some(read_bounded(&active_path)?),
        Err(error) if error.kind() == io::ErrorKind::NotFound && paths.environment.is_none() => {
            None
        }
        Err(error) => return Err(error.into()),
    };
    settings_from_paths(&paths, active_path, baseline)
}

fn settings_from_paths(
    paths: &ConfigPaths,
    active_path: PathBuf,
    baseline: Option<Vec<u8>>,
) -> Result<ConfigSettings, Box<dyn Error>> {
    let active_raw = baseline
        .as_deref()
        .map(|bytes| parse(&active_path, bytes))
        .transpose()?
        .unwrap_or_default();
    let (effective_raw, loaded_path, _) = load_raw_from_paths(paths, baseline.as_deref())?;
    let (inherited_raw, inherited_baseline) = load_inherited_raw(paths)?;
    let effective = resolve(effective_raw.clone(), loaded_path)?;
    let inherited = preview_values(&inherited_raw);
    Ok(ConfigSettings {
        active_path,
        active_source: paths.active_source(),
        inherited_source: if paths.has_distinct_environment_layer()
            && paths.global.as_deref().is_some_and(Path::is_file)
        {
            ConfigSource::Global
        } else {
            ConfigSource::Default
        },
        inherited,
        inherited_overrides: preview_overrides(inherited_raw),
        effective: values_from_raw_and_config(effective_raw, &effective),
        overrides: overrides_from_raw(active_raw)?,
        baseline,
        inherited_baseline,
    })
}

pub(crate) fn load_notification_settings()
-> Result<boomux::daemon::NotificationDeliverySettings, Box<dyn Error>> {
    let (raw, _) = load_raw()?;
    resolve_daemon_settings(raw.notifications, raw.recovery, raw.scheduling)
}

fn load_raw() -> Result<(RawConfig, Option<PathBuf>), Box<dyn Error>> {
    let paths = ConfigPaths::from_environment()?;
    let (raw, loaded_path, _) = load_raw_from_paths(&paths, None)?;
    Ok((raw, loaded_path))
}

#[derive(Clone, Debug)]
struct ConfigPaths {
    global: Option<PathBuf>,
    environment: Option<PathBuf>,
}

type RawLoad = (RawConfig, Option<PathBuf>, Vec<ConfigLayer>);
type InheritedLoad = (RawConfig, Option<Vec<u8>>);

impl ConfigPaths {
    fn from_environment() -> Result<Self, Box<dyn Error>> {
        let environment = env::var_os("BOOMUX_CONFIG")
            .map(|path| {
                if path.is_empty() {
                    Err(ConfigError("BOOMUX_CONFIG cannot be empty".into()))
                } else {
                    Ok(PathBuf::from(path))
                }
            })
            .transpose()?;
        Ok(Self {
            global: global_config_path(),
            environment,
        })
    }

    fn active(&self) -> Result<&Path, Box<dyn Error>> {
        self.environment
            .as_deref()
            .or(self.global.as_deref())
            .ok_or_else(|| {
                ConfigError(
                    "cannot resolve config path: XDG_CONFIG_HOME or HOME must be absolute".into(),
                )
                .into()
            })
    }

    fn active_source(&self) -> ConfigSource {
        if self.environment.is_some() {
            ConfigSource::Environment
        } else {
            ConfigSource::Global
        }
    }

    fn has_distinct_environment_layer(&self) -> bool {
        let (Some(global), Some(environment)) = (&self.global, &self.environment) else {
            return false;
        };
        if global == environment {
            return false;
        }
        match (fs::metadata(global), fs::metadata(environment)) {
            (Ok(global), Ok(environment)) => {
                global.dev() != environment.dev() || global.ino() != environment.ino()
            }
            _ => true,
        }
    }
}

fn load_raw_from_paths(
    paths: &ConfigPaths,
    active_candidate: Option<&[u8]>,
) -> Result<RawLoad, Box<dyn Error>> {
    let mut raw = RawConfig::default();
    let mut loaded_path = None;
    let mut layers = Vec::new();

    if let Some(path) = paths
        .global
        .as_deref()
        .filter(|_| !paths.environment.is_some() || paths.has_distinct_environment_layer())
    {
        let loaded = active_candidate.is_some() && paths.environment.is_none() || path.is_file();
        if loaded {
            let next = if paths.environment.is_none() {
                active_candidate.map_or_else(|| read(path), |bytes| parse(path, bytes))?
            } else {
                read(path)?
            };
            merge(&mut raw, next);
            loaded_path = Some(path.to_owned());
        }
        layers.push(ConfigLayer {
            source: ConfigSource::Global,
            path: path.to_owned(),
            loaded,
        });
    }

    if let Some(path) = paths.environment.as_deref() {
        let next = active_candidate.map_or_else(|| read(path), |bytes| parse(path, bytes))?;
        merge(&mut raw, next);
        loaded_path = Some(path.to_owned());
        layers.push(ConfigLayer {
            source: ConfigSource::Environment,
            path: path.to_owned(),
            loaded: true,
        });
    }

    Ok((raw, loaded_path, layers))
}

fn load_inherited_raw(paths: &ConfigPaths) -> Result<InheritedLoad, Box<dyn Error>> {
    if paths.has_distinct_environment_layer()
        && let Some(global) = paths.global.as_deref()
        && global.is_file()
    {
        let bytes = read_bounded(global)?;
        return Ok((parse(global, &bytes)?, Some(bytes)));
    }
    Ok((RawConfig::default(), None))
}

fn verify_inherited_baseline(
    paths: &ConfigPaths,
    baseline: Option<&[u8]>,
) -> Result<(), Box<dyn Error>> {
    if !paths.has_distinct_environment_layer() {
        return Ok(());
    }
    let Some(global) = paths.global.as_deref() else {
        return Ok(());
    };
    let current = if global.is_file() {
        Some(read_bounded(global)?)
    } else {
        None
    };
    if current.as_deref() != baseline {
        return Err(ConfigError(format!(
            "inherited config changed while settings were being edited: {}",
            global.display()
        ))
        .into());
    }
    Ok(())
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
    let bytes = read_bounded(path)?;
    parse(path, &bytes)
}

fn parse(path: &Path, bytes: &[u8]) -> Result<RawConfig, Box<dyn Error>> {
    let contents = std::str::from_utf8(bytes)
        .map_err(|error| ConfigError(format!("invalid config {}: {error}", path.display())))?;
    toml::from_str(contents)
        .map_err(|error| ConfigError(format!("invalid config {}: {error}", path.display())).into())
}

fn read_bounded(path: &Path) -> Result<Vec<u8>, Box<dyn Error>> {
    let file = File::open(path)
        .map_err(|error| ConfigError(format!("could not read {}: {error}", path.display())))?;
    if file.metadata()?.len() > MAX_CONFIG_BYTES {
        return Err(ConfigError(format!(
            "config {} exceeds the {} byte limit",
            path.display(),
            MAX_CONFIG_BYTES
        ))
        .into());
    }
    let mut bytes = Vec::new();
    file.take(MAX_CONFIG_BYTES + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_CONFIG_BYTES {
        return Err(ConfigError(format!(
            "config {} exceeds the {} byte limit",
            path.display(),
            MAX_CONFIG_BYTES
        ))
        .into());
    }
    Ok(bytes)
}

pub(crate) fn active_path() -> Result<PathBuf, Box<dyn Error>> {
    Ok(ConfigPaths::from_environment()?.active()?.to_owned())
}

pub(crate) fn validate() -> Result<ConfigSnapshot, Box<dyn Error>> {
    let paths = ConfigPaths::from_environment()?;
    validate_layers(&paths, None)?;
    load_snapshot()
}

pub(crate) fn save_settings(
    expected: &ConfigSettings,
    overrides: &ConfigOverrides,
) -> Result<ConfigSettings, Box<dyn Error>> {
    let paths = ConfigPaths::from_environment()?;
    save_settings_with_paths(&paths, expected, overrides)
}

fn save_settings_with_paths(
    paths: &ConfigPaths,
    expected: &ConfigSettings,
    overrides: &ConfigOverrides,
) -> Result<ConfigSettings, Box<dyn Error>> {
    if paths.active()? != expected.active_path || paths.active_source() != expected.active_source {
        return Err(
            ConfigError("active config source changed; reload settings and retry".into()).into(),
        );
    }
    let original = expected
        .baseline
        .as_deref()
        .unwrap_or(CONFIG_TEMPLATE.as_bytes());
    let text = std::str::from_utf8(original).map_err(|error| {
        ConfigError(format!(
            "invalid config {}: {error}",
            expected.active_path.display()
        ))
    })?;
    let mut document = text.parse::<DocumentMut>().map_err(|error| {
        ConfigError(format!(
            "invalid config {}: {error}",
            expected.active_path.display()
        ))
    })?;
    apply_overrides(&mut document, overrides);
    let candidate = document.to_string().into_bytes();
    if candidate.len() as u64 > MAX_CONFIG_BYTES {
        return Err(ConfigError(format!(
            "config {} exceeds the {} byte limit",
            expected.active_path.display(),
            MAX_CONFIG_BYTES
        ))
        .into());
    }
    let saved = settings_from_paths(paths, expected.active_path.clone(), Some(candidate.clone()))?;
    verify_inherited_baseline(paths, expected.inherited_baseline.as_deref())?;
    commit_candidate(
        &expected.active_path,
        expected.baseline.as_deref(),
        &candidate,
    )?;
    Ok(saved)
}

pub(crate) fn edit() -> Result<(), Box<dyn Error>> {
    let paths = ConfigPaths::from_environment()?;
    let editor = editor_argv()?;
    edit_with(&paths, |working| run_editor(&editor, working), || {})
}

fn editor_argv() -> Result<Vec<OsString>, Box<dyn Error>> {
    for name in ["VISUAL", "EDITOR"] {
        if let Some(value) = env::var_os(name) {
            let value = value.into_string().map_err(|_| {
                ConfigError(format!("{name} must contain valid UTF-8 command arguments"))
            })?;
            let words = shell_words::split(&value)
                .map_err(|error| ConfigError(format!("invalid {name}: {error}")))?;
            if words.is_empty() {
                return Err(ConfigError(format!("{name} cannot be empty")).into());
            }
            return Ok(words.into_iter().map(OsString::from).collect());
        }
    }
    Ok(vec![OsString::from("sensible-editor")])
}

fn run_editor(editor: &[OsString], working: &Path) -> Result<ExitStatus, Box<dyn Error>> {
    let mut command = Command::new(&editor[0]);
    command
        .args(&editor[1..])
        .arg(working)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());
    match command.status() {
        Err(error)
            if error.kind() == io::ErrorKind::NotFound
                && editor.first().is_some_and(|word| word == "sensible-editor") =>
        {
            Ok(Command::new("vi")
                .arg(working)
                .stdin(Stdio::inherit())
                .stdout(Stdio::inherit())
                .stderr(Stdio::inherit())
                .status()?)
        }
        result => Ok(result?),
    }
}

fn edit_with(
    paths: &ConfigPaths,
    run: impl FnOnce(&Path) -> Result<ExitStatus, Box<dyn Error>>,
    before_commit: impl FnOnce(),
) -> Result<(), Box<dyn Error>> {
    let target = paths.active()?;
    let parent = target
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let baseline = match fs::symlink_metadata(target) {
        Ok(metadata) => {
            validate_regular_owner(target, &metadata, "config target")?;
            Some(read_bounded(target)?)
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => None,
        Err(error) => return Err(error.into()),
    };
    let (_, inherited_baseline) = load_inherited_raw(paths)?;

    let mut builder = fs::DirBuilder::new();
    builder.recursive(true).mode(0o700);
    builder.create(parent)?;
    let (mut working_file, working_path) = create_working_file(parent, target.file_name())?;
    let working = TemporaryPath(Some(working_path));
    working_file.write_all(baseline.as_deref().unwrap_or(CONFIG_TEMPLATE.as_bytes()))?;
    working_file.sync_all()?;
    drop(working_file);

    let status = run(working.path())?;
    if !status.success() {
        return Err(ConfigError(format!("editor exited with {status}")).into());
    }
    let candidate_metadata = fs::symlink_metadata(working.path())?;
    validate_regular_owner(working.path(), &candidate_metadata, "edited config")?;
    let candidate = read_bounded(working.path())?;
    validate_layers(paths, Some(&candidate))?;

    before_commit();
    verify_inherited_baseline(paths, inherited_baseline.as_deref())?;
    match commit_candidate(target, baseline.as_deref(), &candidate) {
        Ok(()) => Ok(()),
        Err(error) if error.downcast_ref::<ConfigCommittedError>().is_some() => {
            if read_bounded(target).ok().as_deref() == Some(candidate.as_slice()) {
                Ok(())
            } else {
                Err(error)
            }
        }
        Err(error) => Err(error),
    }
}

fn commit_candidate(
    target: &Path,
    baseline: Option<&[u8]>,
    candidate: &[u8],
) -> Result<(), Box<dyn Error>> {
    let parent = target
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true).mode(0o700);
    builder.create(parent)?;
    let (mut working_file, working_path) = create_working_file(parent, target.file_name())?;
    let mut working = TemporaryPath(Some(working_path));
    working_file.write_all(candidate)?;
    working_file.sync_all()?;
    drop(working_file);

    match baseline {
        None => {
            fs::set_permissions(working.path(), fs::Permissions::from_mode(0o600))?;
            rename_noreplace(working.path(), target).map_err(|error| {
                if error.raw_os_error() == Some(libc::EEXIST) {
                    ConfigError(format!(
                        "config target changed while it was being edited: {}",
                        target.display()
                    ))
                    .into()
                } else {
                    Box::<dyn Error>::from(error)
                }
            })?;
            working.disarm();
        }
        Some(baseline) => {
            let current = fs::symlink_metadata(target)?;
            validate_regular_owner(target, &current, "config target")?;
            if read_bounded(target)? != baseline {
                return Err(ConfigError(format!(
                    "config target changed while it was being edited: {}",
                    target.display()
                ))
                .into());
            }
            fs::set_permissions(
                working.path(),
                fs::Permissions::from_mode(current.mode() & 0o7777),
            )?;
            fs::rename(working.path(), target)?;
            working.disarm();
        }
    }
    File::open(target)
        .and_then(|file| file.sync_all())
        .and_then(|()| File::open(parent)?.sync_all())
        .map_err(|error| {
            ConfigCommittedError(format!(
                "config was committed but could not be synchronized: {error}"
            ))
        })?;
    Ok(())
}

fn validate_layers(
    paths: &ConfigPaths,
    active_candidate: Option<&[u8]>,
) -> Result<(), Box<dyn Error>> {
    let (raw, loaded_path, _) = load_raw_from_paths(paths, active_candidate)?;
    resolve(raw, loaded_path)?;
    Ok(())
}

fn values_from_raw_and_config(raw: RawConfig, config: &Config) -> ConfigValues {
    let roots = raw.projects.unwrap_or_default().roots.unwrap_or_default();
    let notifications = &config.notifications;
    ConfigValues {
        terminal: config.terminal.clone(),
        project_roots: roots,
        project_max_depth: config.projects.max_depth,
        follow_focused_terminal: config.dashboard.follow_focused_terminal,
        resume_agents: notifications.resume_agents,
        persist_terminal_history: notifications.persist_terminal_history,
        scheduling_max_concurrent: notifications.max_scheduled_execution_concurrency,
        notifications_enabled: notifications.desktop.enabled,
        notifications_blocked: notifications.desktop.blocked,
        notifications_completed: notifications.desktop.completed,
        notifications_scheduled_dispatch_failed: notifications.desktop.scheduled_dispatch_failed,
        notifications_scheduled_interrupted: notifications.desktop.scheduled_interrupted,
        sound_enabled: notifications.sound.enabled,
        sound_blocked: notifications.sound.blocked.clone(),
        sound_completed: notifications.sound.completed.clone(),
        sound_scheduled_dispatch_failed: notifications.sound.scheduled_dispatch_failed.clone(),
        sound_scheduled_interrupted: notifications.sound.scheduled_interrupted.clone(),
    }
}

fn preview_values(raw: &RawConfig) -> ConfigValues {
    let defaults = resolve(RawConfig::default(), None).expect("built-in config is valid");
    let mut values = values_from_raw_and_config(RawConfig::default(), &defaults);
    if let Some(terminal) = &raw.terminal {
        values.terminal = Some(terminal.trim().to_owned());
    }
    if let Some(projects) = &raw.projects {
        if let Some(roots) = &projects.roots {
            values.project_roots.clone_from(roots);
        }
        if let Some(max_depth) = projects.max_depth {
            values.project_max_depth = max_depth;
        }
    }
    if let Some(dashboard) = &raw.dashboard
        && let Some(value) = dashboard.follow_focused_terminal
    {
        values.follow_focused_terminal = value;
    }
    if let Some(recovery) = &raw.recovery {
        values.resume_agents = recovery.resume_agents.unwrap_or(values.resume_agents);
        values.persist_terminal_history = recovery
            .persist_terminal_history
            .unwrap_or(values.persist_terminal_history);
    }
    if let Some(max_concurrent) = raw
        .scheduling
        .as_ref()
        .and_then(|scheduling| scheduling.max_concurrent)
    {
        values.scheduling_max_concurrent = u16::try_from(max_concurrent).unwrap_or_else(|_| {
            if max_concurrent.is_negative() {
                0
            } else {
                u16::MAX
            }
        });
    }
    if let Some(notifications) = &raw.notifications {
        values.notifications_enabled = notifications
            .enabled
            .unwrap_or(values.notifications_enabled);
        values.notifications_blocked = notifications
            .blocked
            .unwrap_or(values.notifications_blocked);
        values.notifications_completed = notifications
            .completed
            .unwrap_or(values.notifications_completed);
        values.notifications_scheduled_dispatch_failed = notifications
            .scheduled_dispatch_failed
            .unwrap_or(values.notifications_scheduled_dispatch_failed);
        values.notifications_scheduled_interrupted = notifications
            .scheduled_interrupted
            .unwrap_or(values.notifications_scheduled_interrupted);
        if let Some(sound) = &notifications.sound {
            values.sound_enabled = sound.enabled.unwrap_or(values.sound_enabled);
            if let Some(value) = &sound.blocked {
                values.sound_blocked.clone_from(value);
            }
            if let Some(value) = &sound.completed {
                values.sound_completed.clone_from(value);
            }
            if let Some(value) = &sound.scheduled_dispatch_failed {
                values.sound_scheduled_dispatch_failed.clone_from(value);
            }
            if let Some(value) = &sound.scheduled_interrupted {
                values.sound_scheduled_interrupted.clone_from(value);
            }
        }
    }
    values
}

fn preview_overrides(raw: RawConfig) -> ConfigOverrides {
    let projects = raw.projects.unwrap_or_default();
    let dashboard = raw.dashboard.unwrap_or_default();
    let recovery = raw.recovery.unwrap_or_default();
    let scheduling = raw.scheduling.unwrap_or_default();
    let notifications = raw.notifications.unwrap_or_default();
    let sound = notifications.sound.unwrap_or_default();
    ConfigOverrides {
        terminal: raw.terminal,
        project_roots: projects.roots,
        project_max_depth: projects.max_depth,
        follow_focused_terminal: dashboard.follow_focused_terminal,
        resume_agents: recovery.resume_agents,
        persist_terminal_history: recovery.persist_terminal_history,
        scheduling_max_concurrent: scheduling.max_concurrent.map(|value| {
            u16::try_from(value).unwrap_or_else(|_| if value.is_negative() { 0 } else { u16::MAX })
        }),
        notifications_enabled: notifications.enabled,
        notifications_blocked: notifications.blocked,
        notifications_completed: notifications.completed,
        notifications_scheduled_dispatch_failed: notifications.scheduled_dispatch_failed,
        notifications_scheduled_interrupted: notifications.scheduled_interrupted,
        sound_enabled: sound.enabled,
        sound_blocked: sound.blocked,
        sound_completed: sound.completed,
        sound_scheduled_dispatch_failed: sound.scheduled_dispatch_failed,
        sound_scheduled_interrupted: sound.scheduled_interrupted,
    }
}

fn overrides_from_raw(raw: RawConfig) -> Result<ConfigOverrides, Box<dyn Error>> {
    let projects = raw.projects.unwrap_or_default();
    let dashboard = raw.dashboard.unwrap_or_default();
    let recovery = raw.recovery.unwrap_or_default();
    let scheduling = raw.scheduling.unwrap_or_default();
    let notifications = raw.notifications.unwrap_or_default();
    let sound = notifications.sound.unwrap_or_default();
    Ok(ConfigOverrides {
        terminal: raw.terminal,
        project_roots: projects.roots,
        project_max_depth: projects.max_depth,
        follow_focused_terminal: dashboard.follow_focused_terminal,
        resume_agents: recovery.resume_agents,
        persist_terminal_history: recovery.persist_terminal_history,
        scheduling_max_concurrent: scheduling
            .max_concurrent
            .map(u16::try_from)
            .transpose()
            .map_err(|_| ConfigError("scheduling.max_concurrent must fit in u16".into()))?,
        notifications_enabled: notifications.enabled,
        notifications_blocked: notifications.blocked,
        notifications_completed: notifications.completed,
        notifications_scheduled_dispatch_failed: notifications.scheduled_dispatch_failed,
        notifications_scheduled_interrupted: notifications.scheduled_interrupted,
        sound_enabled: sound.enabled,
        sound_blocked: sound.blocked,
        sound_completed: sound.completed,
        sound_scheduled_dispatch_failed: sound.scheduled_dispatch_failed,
        sound_scheduled_interrupted: sound.scheduled_interrupted,
    })
}

fn apply_overrides(document: &mut DocumentMut, overrides: &ConfigOverrides) {
    set_string(document, &["terminal"], overrides.terminal.as_deref());
    set_strings(
        document,
        &["projects", "roots"],
        overrides.project_roots.as_deref(),
    );
    set_integer(
        document,
        &["projects", "max_depth"],
        overrides.project_max_depth.map(|value| value as i64),
    );
    set_bool(
        document,
        &["dashboard", "follow_focused_terminal"],
        overrides.follow_focused_terminal,
    );
    set_bool(
        document,
        &["recovery", "resume_agents"],
        overrides.resume_agents,
    );
    set_bool(
        document,
        &["recovery", "persist_terminal_history"],
        overrides.persist_terminal_history,
    );
    set_integer(
        document,
        &["scheduling", "max_concurrent"],
        overrides.scheduling_max_concurrent.map(i64::from),
    );
    for (key, setting) in [
        ("enabled", overrides.notifications_enabled),
        ("blocked", overrides.notifications_blocked),
        ("completed", overrides.notifications_completed),
        (
            "scheduled_dispatch_failed",
            overrides.notifications_scheduled_dispatch_failed,
        ),
        (
            "scheduled_interrupted",
            overrides.notifications_scheduled_interrupted,
        ),
    ] {
        set_bool(document, &["notifications", key], setting);
    }
    set_bool(
        document,
        &["notifications", "sound", "enabled"],
        overrides.sound_enabled,
    );
    for (key, setting) in [
        ("blocked", overrides.sound_blocked.as_deref()),
        ("completed", overrides.sound_completed.as_deref()),
        (
            "scheduled_dispatch_failed",
            overrides.sound_scheduled_dispatch_failed.as_deref(),
        ),
        (
            "scheduled_interrupted",
            overrides.sound_scheduled_interrupted.as_deref(),
        ),
    ] {
        set_string(document, &["notifications", "sound", key], setting);
    }
}

fn table_for_path<'a>(document: &'a mut DocumentMut, path: &[&str]) -> &'a mut dyn TableLike {
    let mut table: &mut dyn TableLike = document.as_table_mut();
    for key in path {
        let item = table
            .entry(key)
            .or_insert_with(|| Item::Table(Table::new()));
        if !item.is_table_like() {
            *item = Item::Table(Table::new());
        }
        table = item.as_table_like_mut().expect("created table-like item");
    }
    table
}

fn remove_key(document: &mut DocumentMut, path: &[&str]) {
    let Some((key, parents)) = path.split_last() else {
        return;
    };
    let mut table: &mut dyn TableLike = document.as_table_mut();
    for parent in parents {
        let Some(next) = table.get_mut(parent).and_then(Item::as_table_like_mut) else {
            return;
        };
        table = next;
    }
    table.remove(key);
}

fn set_value(document: &mut DocumentMut, path: &[&str], setting: Option<Value>) {
    let Some((key, parents)) = path.split_last() else {
        return;
    };
    match setting {
        Some(setting) => {
            let table = table_for_path(document, parents);
            let mut setting = setting;
            if let Some(existing) = table.get_mut(key).and_then(Item::as_value_mut) {
                setting.decor_mut().clone_from(existing.decor());
                *existing = setting;
                return;
            }
            table.insert(key, Item::Value(setting));
        }
        None => remove_key(document, path),
    }
}

fn set_string(document: &mut DocumentMut, path: &[&str], setting: Option<&str>) {
    set_value(document, path, setting.map(Value::from));
}

fn set_bool(document: &mut DocumentMut, path: &[&str], setting: Option<bool>) {
    set_value(document, path, setting.map(Value::from));
}

fn set_integer(document: &mut DocumentMut, path: &[&str], setting: Option<i64>) {
    set_value(document, path, setting.map(Value::from));
}

fn set_strings(document: &mut DocumentMut, path: &[&str], setting: Option<&[String]>) {
    set_value(
        document,
        path,
        setting.map(|strings| {
            let mut array = Array::new();
            for string in strings {
                array.push(string.as_str());
            }
            Value::Array(array)
        }),
    );
}

fn validate_regular_owner(
    path: &Path,
    metadata: &fs::Metadata,
    description: &str,
) -> Result<(), Box<dyn Error>> {
    if !metadata.file_type().is_file() {
        return Err(ConfigError(format!(
            "{description} is not a regular file: {}",
            path.display()
        ))
        .into());
    }
    if metadata.uid() != unsafe { libc::geteuid() } {
        return Err(ConfigError(format!(
            "{description} is not owned by the current user: {}",
            path.display()
        ))
        .into());
    }
    Ok(())
}

fn create_working_file(
    parent: &Path,
    target_name: Option<&OsStr>,
) -> Result<(File, PathBuf), Box<dyn Error>> {
    let target_name = target_name.unwrap_or_else(|| OsStr::new("config.toml"));
    for _ in 0..TEMPORARY_CREATE_ATTEMPTS {
        let name = format!(
            ".{}.edit-{}",
            target_name.to_string_lossy(),
            uuid::Uuid::new_v4()
        );
        let path = parent.join(name);
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
        {
            Ok(file) => return Ok((file, path)),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error.into()),
        }
    }
    Err(ConfigError("could not allocate a temporary config working copy".into()).into())
}

struct TemporaryPath(Option<PathBuf>);

impl TemporaryPath {
    fn path(&self) -> &Path {
        self.0.as_deref().expect("temporary path is armed")
    }

    fn disarm(&mut self) {
        self.0 = None;
    }
}

impl Drop for TemporaryPath {
    fn drop(&mut self) {
        if let Some(path) = self.0.take() {
            let _ = fs::remove_file(path);
        }
    }
}

fn rename_noreplace(from: &Path, to: &Path) -> io::Result<()> {
    renameat2(from, to, libc::RENAME_NOREPLACE)
}

fn renameat2(from: &Path, to: &Path, flags: u32) -> io::Result<()> {
    let from = CString::new(from.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path contains a NUL byte"))?;
    let to = CString::new(to.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path contains a NUL byte"))?;
    let result = unsafe {
        libc::renameat2(
            libc::AT_FDCWD,
            from.as_ptr(),
            libc::AT_FDCWD,
            to.as_ptr(),
            flags,
        )
    };
    if result == -1 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
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
        if next_notifications.scheduled_dispatch_failed.is_some() {
            notifications.scheduled_dispatch_failed = next_notifications.scheduled_dispatch_failed;
        }
        if next_notifications.scheduled_interrupted.is_some() {
            notifications.scheduled_interrupted = next_notifications.scheduled_interrupted;
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
            if next_sound.scheduled_dispatch_failed.is_some() {
                sound.scheduled_dispatch_failed = next_sound.scheduled_dispatch_failed;
            }
            if next_sound.scheduled_interrupted.is_some() {
                sound.scheduled_interrupted = next_sound.scheduled_interrupted;
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
    if let Some(next_scheduling) = next.scheduling {
        let scheduling = base.scheduling.get_or_insert_default();
        if next_scheduling.max_concurrent.is_some() {
            scheduling.max_concurrent = next_scheduling.max_concurrent;
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
        notifications: resolve_daemon_settings(raw.notifications, raw.recovery, raw.scheduling)?,
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
    resolve_daemon_settings(raw, None, None).expect("default scheduling config is valid")
}

fn resolve_daemon_settings(
    notifications: Option<RawNotificationsConfig>,
    recovery: Option<RawRecoveryConfig>,
    scheduling: Option<RawSchedulingConfig>,
) -> Result<boomux::daemon::NotificationDeliverySettings, Box<dyn Error>> {
    let raw = notifications.unwrap_or_default();
    let recovery = recovery.unwrap_or_default();
    let max_concurrent = scheduling
        .unwrap_or_default()
        .max_concurrent
        .unwrap_or(i64::from(DEFAULT_MAX_SCHEDULED_EXECUTION_CONCURRENCY));
    if !(1..=MAX_SCHEDULED_EXECUTION_CONCURRENCY).contains(&max_concurrent) {
        return Err(ConfigError(format!(
            "scheduling.max_concurrent must be between 1 and {MAX_SCHEDULED_EXECUTION_CONCURRENCY}"
        ))
        .into());
    }
    Ok(boomux::daemon::NotificationDeliverySettings {
        desktop: boomux::daemon::NotificationSettings {
            enabled: raw.enabled.unwrap_or(false),
            blocked: raw.blocked.unwrap_or(true),
            completed: raw.completed.unwrap_or(true),
            scheduled_dispatch_failed: raw.scheduled_dispatch_failed.unwrap_or(false),
            scheduled_interrupted: raw.scheduled_interrupted.unwrap_or(false),
        },
        sound: raw.sound.map_or_else(Default::default, |sound| {
            boomux::daemon::NotificationSoundSettings {
                enabled: sound.enabled.unwrap_or(false),
                blocked: sound
                    .blocked
                    .unwrap_or_else(|| "message-new-instant".into()),
                completed: sound.completed.unwrap_or_else(|| "complete".into()),
                scheduled_dispatch_failed: sound
                    .scheduled_dispatch_failed
                    .unwrap_or_else(|| "dialog-warning".into()),
                scheduled_interrupted: sound
                    .scheduled_interrupted
                    .unwrap_or_else(|| "dialog-warning".into()),
            }
        }),
        resume_agents: recovery.resume_agents.unwrap_or(true),
        persist_terminal_history: recovery.persist_terminal_history.unwrap_or(false),
        max_scheduled_execution_concurrency: max_concurrent as u16,
    })
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
pub(crate) fn test_settings() -> ConfigSettings {
    let effective = resolve(RawConfig::default(), None).expect("default config");
    let values = values_from_raw_and_config(RawConfig::default(), &effective);
    ConfigSettings {
        active_path: PathBuf::from("/tmp/boomux-test-config.toml"),
        active_source: ConfigSource::Global,
        inherited_source: ConfigSource::Default,
        inherited: values.clone(),
        inherited_overrides: ConfigOverrides::default(),
        effective: values,
        overrides: ConfigOverrides::default(),
        baseline: None,
        inherited_baseline: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TestDirectory(PathBuf);

    impl TestDirectory {
        fn new() -> Self {
            let path = env::temp_dir().join(format!(
                "boomux-config-test-{}-{}",
                std::process::id(),
                uuid::Uuid::new_v4()
            ));
            fs::create_dir(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

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
        assert!(!settings.desktop.scheduled_dispatch_failed);
        assert!(!settings.desktop.scheduled_interrupted);
    }

    #[test]
    fn recovery_settings_default_and_merge_per_field() {
        let defaults = resolve_daemon_settings(None, None, None).unwrap();
        assert!(defaults.resume_agents);
        assert!(!defaults.persist_terminal_history);

        let mut base: RawConfig =
            toml::from_str("[recovery]\nresume_agents = false\npersist_terminal_history = true")
                .unwrap();
        let next: RawConfig = toml::from_str("[recovery]\nresume_agents = true").unwrap();
        merge(&mut base, next);

        let settings =
            resolve_daemon_settings(base.notifications, base.recovery, base.scheduling).unwrap();
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
                    ..Default::default()
                },
                sound: boomux::daemon::NotificationSoundSettings {
                    enabled: true,
                    blocked: "message-new-instant".into(),
                    completed: "service-login".into(),
                    ..Default::default()
                },
                resume_agents: true,
                persist_terminal_history: false,
                max_scheduled_execution_concurrency: 4,
            }
        );
    }

    #[test]
    fn scheduled_notification_categories_are_independent_and_default_disabled() {
        let raw: RawConfig = toml::from_str(
            "[notifications]\nenabled = true\nscheduled_dispatch_failed = true\nscheduled_interrupted = false\n[notifications.sound]\nscheduled_interrupted = \"service-logout\"",
        )
        .unwrap();
        let settings = resolve_notifications(raw.notifications);
        assert!(settings.desktop.scheduled_dispatch_failed);
        assert!(!settings.desktop.scheduled_interrupted);
        assert!(settings.desktop.blocked);
        assert!(settings.desktop.completed);
        assert_eq!(settings.sound.scheduled_interrupted, "service-logout");
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

    #[test]
    fn scheduling_concurrency_defaults_layers_and_rejects_invalid_values() {
        assert_eq!(
            resolve(RawConfig::default(), None)
                .unwrap()
                .notifications
                .max_scheduled_execution_concurrency,
            4
        );
        let mut base: RawConfig = toml::from_str("[scheduling]\nmax_concurrent = 2").unwrap();
        let next: RawConfig = toml::from_str("[scheduling]\nmax_concurrent = 9").unwrap();
        merge(&mut base, next);
        assert_eq!(
            resolve(base, None)
                .unwrap()
                .notifications
                .max_scheduled_execution_concurrency,
            9
        );
        for value in [0, -1, 65] {
            let raw: RawConfig =
                toml::from_str(&format!("[scheduling]\nmax_concurrent = {value}")).unwrap();
            assert!(resolve(raw, None).is_err());
        }
        assert!(toml::from_str::<RawConfig>("[scheduling]\nunknown = 1").is_err());
    }

    #[test]
    fn structured_edits_preserve_comments_and_unrelated_formatting() {
        let original = r#"# owner note
terminal = "Alacritty.desktop" # keep inline

[projects] # project note
roots = [ "/old" ]
max_depth = 2

[dashboard]
follow_focused_terminal = true
"#;
        let mut document = original.parse::<DocumentMut>().unwrap();
        let overrides = ConfigOverrides {
            terminal: Some("com.mitchellh.ghostty.desktop".into()),
            project_roots: Some(vec!["~/Code".into(), "/srv/work".into()]),
            project_max_depth: Some(2),
            follow_focused_terminal: Some(false),
            ..Default::default()
        };

        apply_overrides(&mut document, &overrides);
        let edited = document.to_string();

        assert!(edited.contains("# owner note"));
        assert!(edited.contains("# keep inline"));
        assert!(edited.contains("# project note"));
        assert!(edited.contains("max_depth = 2"));
        assert!(!edited.contains("follow_focused_terminal = true"));
        assert!(edited.contains("follow_focused_terminal = false"));
        assert!(edited.contains("~/Code"));
        assert!(edited.contains("/srv/work"));
    }

    #[test]
    fn structured_edits_preserve_unrelated_inline_table_values() {
        let original = r#"notifications = { blocked = false, sound = { blocked = "old", completed = "keep" } }
"#;
        let mut document = original.parse::<DocumentMut>().unwrap();
        let overrides = ConfigOverrides {
            notifications_enabled: Some(true),
            notifications_blocked: Some(false),
            sound_blocked: Some("new".into()),
            sound_completed: Some("keep".into()),
            ..Default::default()
        };

        apply_overrides(&mut document, &overrides);
        let raw: RawConfig = toml::from_str(&document.to_string()).unwrap();
        let notifications = raw.notifications.unwrap();
        assert_eq!(notifications.enabled, Some(true));
        assert_eq!(notifications.blocked, Some(false));
        let sound = notifications.sound.unwrap();
        assert_eq!(sound.blocked.as_deref(), Some("new"));
        assert_eq!(sound.completed.as_deref(), Some("keep"));
    }

    #[test]
    fn layered_semantics_are_resolved_only_after_field_merge() {
        let mut base: RawConfig =
            toml::from_str("[projects]\nmax_depth = 99\n[scheduling]\nmax_concurrent = 100")
                .unwrap();
        let override_layer: RawConfig =
            toml::from_str("[projects]\nmax_depth = 4\n[scheduling]\nmax_concurrent = 8").unwrap();

        assert!(resolve(base.clone(), None).is_err());
        merge(&mut base, override_layer);
        let resolved = resolve(base, None).unwrap();
        assert_eq!(resolved.projects.max_depth, 4);
        assert_eq!(
            resolved.notifications.max_scheduled_execution_concurrency,
            8
        );
    }

    #[test]
    fn source_labels_distinguish_default_global_and_environment() {
        assert_eq!(ConfigSource::Default.label(), "default");
        assert_eq!(ConfigSource::Global.label(), "global");
        assert_eq!(ConfigSource::Environment.label(), "BOOMUX_CONFIG");

        let global = ConfigPaths {
            global: Some(PathBuf::from("/tmp/global.toml")),
            environment: None,
        };
        assert_eq!(global.active_source(), ConfigSource::Global);
        let environment = ConfigPaths {
            global: global.global,
            environment: Some(PathBuf::from("/tmp/override.toml")),
        };
        assert_eq!(environment.active_source(), ConfigSource::Environment);
        assert_eq!(
            environment.active().unwrap(),
            Path::new("/tmp/override.toml")
        );
    }

    #[test]
    fn settings_load_and_save_allow_active_layer_to_correct_invalid_lower_semantics() {
        let test = TestDirectory::new();
        let global = test.0.join("global.toml");
        let environment = test.0.join("environment.toml");
        fs::write(
            &global,
            "[projects]\nmax_depth = 99\n[scheduling]\nmax_concurrent = 100\n",
        )
        .unwrap();
        fs::write(
            &environment,
            "[projects]\nmax_depth = 2\n[scheduling]\nmax_concurrent = 8\n",
        )
        .unwrap();
        fs::set_permissions(&environment, fs::Permissions::from_mode(0o640)).unwrap();
        let paths = ConfigPaths {
            global: Some(global.clone()),
            environment: Some(environment.clone()),
        };

        let settings = settings_from_paths(
            &paths,
            environment.clone(),
            Some(read_bounded(&environment).unwrap()),
        )
        .unwrap();
        assert_eq!(settings.inherited.project_max_depth, 99);
        assert_eq!(settings.inherited.scheduling_max_concurrent, 100);
        assert_eq!(settings.effective.project_max_depth, 2);
        assert_eq!(settings.effective.scheduling_max_concurrent, 8);

        let mut overrides = settings.overrides.clone();
        overrides.project_max_depth = Some(3);
        let saved = save_settings_with_paths(&paths, &settings, &overrides).unwrap();
        assert_eq!(saved.effective.project_max_depth, 3);
        assert_eq!(saved.effective.scheduling_max_concurrent, 8);
        assert_eq!(
            fs::metadata(&environment).unwrap().permissions().mode() & 0o777,
            0o640
        );
        assert!(
            fs::read_to_string(global)
                .unwrap()
                .contains("max_depth = 99")
        );
    }

    #[test]
    fn settings_save_rejects_a_changed_inherited_layer() {
        let test = TestDirectory::new();
        let global = test.0.join("global.toml");
        let environment = test.0.join("environment.toml");
        fs::write(&global, "[projects]\nmax_depth = 2\n").unwrap();
        fs::write(&environment, "[notifications]\nenabled = false\n").unwrap();
        let paths = ConfigPaths {
            global: Some(global.clone()),
            environment: Some(environment.clone()),
        };
        let settings = settings_from_paths(
            &paths,
            environment.clone(),
            Some(read_bounded(&environment).unwrap()),
        )
        .unwrap();
        fs::write(&global, "[projects]\nmax_depth = 3\n").unwrap();

        let mut overrides = settings.overrides.clone();
        overrides.notifications_enabled = Some(true);
        let error = save_settings_with_paths(&paths, &settings, &overrides)
            .unwrap_err()
            .to_string();

        assert!(error.contains("inherited config changed"), "{error}");
        assert!(fs::read_to_string(environment).unwrap().contains("false"));
    }

    #[test]
    fn settings_read_symlink_but_refuse_to_mutate_it() {
        let test = TestDirectory::new();
        let real = test.0.join("real.toml");
        let linked = test.0.join("linked.toml");
        fs::write(&real, "[projects]\nmax_depth = 2\n").unwrap();
        std::os::unix::fs::symlink(&real, &linked).unwrap();
        let paths = ConfigPaths {
            global: None,
            environment: Some(linked.clone()),
        };
        let settings =
            settings_from_paths(&paths, linked, Some(read_bounded(&real).unwrap())).unwrap();
        assert_eq!(settings.effective.project_max_depth, 2);

        let mut overrides = settings.overrides.clone();
        overrides.project_max_depth = Some(3);
        let error = save_settings_with_paths(&paths, &settings, &overrides)
            .unwrap_err()
            .to_string();
        assert!(error.contains("not a regular file"), "{error}");
        assert!(fs::read_to_string(real).unwrap().contains("max_depth = 2"));
    }

    #[test]
    fn readable_non_owned_files_are_readable_but_not_mutable() {
        let path = Path::new("/etc/passwd");
        let metadata = fs::metadata(path).unwrap();
        if metadata.uid() == unsafe { libc::geteuid() } {
            return;
        }

        assert!(!read_bounded(path).unwrap().is_empty());
        let error = validate_regular_owner(path, &metadata, "config target")
            .unwrap_err()
            .to_string();
        assert!(error.contains("not owned by the current user"), "{error}");
    }

    #[test]
    fn canonical_global_environment_path_is_loaded_once_as_environment() {
        let test = TestDirectory::new();
        let path = test.0.join("config.toml");
        fs::write(&path, "[projects]\nmax_depth = 2\n").unwrap();
        let alias = path.parent().unwrap().join(".").join("config.toml");
        let paths = ConfigPaths {
            global: Some(path.clone()),
            environment: Some(alias.clone()),
        };

        let (raw, loaded_path, layers) = load_raw_from_paths(&paths, None).unwrap();
        assert_eq!(resolve(raw, loaded_path).unwrap().projects.max_depth, 2);
        assert_eq!(layers.len(), 1);
        assert_eq!(layers[0].source, ConfigSource::Environment);

        let settings =
            settings_from_paths(&paths, alias, Some(read_bounded(&path).unwrap())).unwrap();
        assert_eq!(settings.active_source, ConfigSource::Environment);
        assert_eq!(settings.inherited_source, ConfigSource::Default);
        assert_eq!(settings.inherited_overrides, ConfigOverrides::default());
    }
}

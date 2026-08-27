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

const DEFAULT_PROJECT_SEARCH_DEPTH: usize = 3;
const MAX_PROJECT_SEARCH_DEPTH: usize = 10;
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

[desktop]
# workspace_layer = "hyprland-special"

[notifications]
# enabled = false
# blocked = true
# completed = true

[notifications.sound]
# enabled = false
# blocked = "message-new-instant"
# completed = "complete"

[recovery]
# resume_agents = true
# persist_terminal_history = false

[claude]
# remote_control = true
"#;

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct RawConfig {
    terminal: Option<String>,
    projects: Option<RawProjectsConfig>,
    notifications: Option<RawNotificationsConfig>,
    dashboard: Option<RawDashboardConfig>,
    desktop: Option<RawDesktopConfig>,
    recovery: Option<RawRecoveryConfig>,
    claude: Option<RawClaudeConfig>,
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
struct RawDesktopConfig {
    workspace_layer: Option<DesktopWorkspaceLayer>,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct RawRecoveryConfig {
    resume_agents: Option<bool>,
    persist_terminal_history: Option<bool>,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct RawClaudeConfig {
    remote_control: Option<bool>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Config {
    pub(crate) terminal: Option<String>,
    pub(crate) projects: ProjectsConfig,
    pub(crate) path: Option<PathBuf>,
    pub(crate) notifications: boomux::daemon::NotificationDeliverySettings,
    pub(crate) dashboard: DashboardConfig,
    pub(crate) desktop: DesktopConfig,
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

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct DesktopConfig {
    pub(crate) workspace_layer: DesktopWorkspaceLayer,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum DesktopWorkspaceLayer {
    Disabled,
    HyprlandSpecial,
}

#[derive(Debug)]
struct ConfigError(String);

#[derive(Debug)]
struct ConfigCommittedError(String);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ConfigSource {
    Global,
    Environment,
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

pub(crate) fn load_notification_settings()
-> Result<boomux::daemon::NotificationDeliverySettings, Box<dyn Error>> {
    let (raw, _) = load_raw()?;
    resolve_daemon_settings(raw.notifications, raw.recovery, raw.claude)
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

fn load_inherited_baseline(paths: &ConfigPaths) -> Result<Option<Vec<u8>>, Box<dyn Error>> {
    if paths.has_distinct_environment_layer()
        && let Some(global) = paths.global.as_deref()
        && global.is_file()
    {
        return read_bounded(global).map(Some);
    }
    Ok(None)
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

pub(crate) fn edit() -> Result<(), Box<dyn Error>> {
    let paths = ConfigPaths::from_environment()?;
    let editor = editor_argv()?;
    edit_with(
        &paths,
        |working| {
            let status = run_editor(&editor, working)?;
            if status.success() {
                Ok(())
            } else {
                Err(ConfigError(format!("editor exited with {status}")).into())
            }
        },
        || {},
    )
}

pub(crate) fn enable_hyprland_workspace_layer() -> Result<PathBuf, Box<dyn Error>> {
    let paths = ConfigPaths::from_environment()?;
    enable_hyprland_workspace_layer_with(&paths)
}

fn enable_hyprland_workspace_layer_with(paths: &ConfigPaths) -> Result<PathBuf, Box<dyn Error>> {
    let target = paths.active()?.to_owned();
    edit_with(
        paths,
        |working| {
            let source = read_bounded(working)?;
            let source = std::str::from_utf8(&source).map_err(|error| {
                ConfigError(format!("invalid config {}: {error}", working.display()))
            })?;
            let mut document = source.parse::<toml_edit::DocumentMut>().map_err(|error| {
                ConfigError(format!("invalid config {}: {error}", working.display()))
            })?;
            document["desktop"]["workspace_layer"] = toml_edit::value("hyprland-special");
            let candidate = document.to_string();
            let mut file = OpenOptions::new()
                .write(true)
                .truncate(true)
                .open(working)?;
            file.write_all(candidate.as_bytes())?;
            file.sync_all()?;
            Ok(())
        },
        || {},
    )?;
    Ok(target)
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
    run: impl FnOnce(&Path) -> Result<(), Box<dyn Error>>,
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
    let inherited_baseline = load_inherited_baseline(paths)?;

    let mut builder = fs::DirBuilder::new();
    builder.recursive(true).mode(0o700);
    builder.create(parent)?;
    let (mut working_file, working_path) = create_working_file(parent, target.file_name())?;
    let working = TemporaryPath(Some(working_path));
    working_file.write_all(baseline.as_deref().unwrap_or(CONFIG_TEMPLATE.as_bytes()))?;
    working_file.sync_all()?;
    drop(working_file);

    run(working.path())?;
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
    if let Some(next_desktop) = next.desktop {
        let desktop = base.desktop.get_or_insert_default();
        if next_desktop.workspace_layer.is_some() {
            desktop.workspace_layer = next_desktop.workspace_layer;
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
    if let Some(next_claude) = next.claude {
        let claude = base.claude.get_or_insert_default();
        if next_claude.remote_control.is_some() {
            claude.remote_control = next_claude.remote_control;
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
        notifications: resolve_daemon_settings(raw.notifications, raw.recovery, raw.claude)?,
        dashboard: DashboardConfig {
            follow_focused_terminal: raw
                .dashboard
                .unwrap_or_default()
                .follow_focused_terminal
                .unwrap_or(true),
        },
        desktop: DesktopConfig {
            workspace_layer: raw
                .desktop
                .unwrap_or_default()
                .workspace_layer
                .unwrap_or(DesktopWorkspaceLayer::Disabled),
        },
    })
}

#[cfg(test)]
fn resolve_notifications(
    raw: Option<RawNotificationsConfig>,
) -> boomux::daemon::NotificationDeliverySettings {
    resolve_daemon_settings(raw, None, None).expect("default notification config is valid")
}

fn resolve_daemon_settings(
    notifications: Option<RawNotificationsConfig>,
    recovery: Option<RawRecoveryConfig>,
    claude: Option<RawClaudeConfig>,
) -> Result<boomux::daemon::NotificationDeliverySettings, Box<dyn Error>> {
    let raw = notifications.unwrap_or_default();
    let recovery = recovery.unwrap_or_default();
    Ok(boomux::daemon::NotificationDeliverySettings {
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
        claude_remote_control: claude.unwrap_or_default().remote_control.unwrap_or(true),
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
    fn desktop_workspace_layer_defaults_off_and_can_be_enabled_independently() {
        let default = resolve(RawConfig::default(), None).expect("resolved default config");
        assert_eq!(
            default.desktop.workspace_layer,
            DesktopWorkspaceLayer::Disabled
        );

        let mut base: RawConfig = toml::from_str("[projects]\nmax_depth = 2").expect("valid base");
        let next: RawConfig = toml::from_str("[desktop]\nworkspace_layer = \"hyprland-special\"")
            .expect("valid override");
        merge(&mut base, next);
        let config = resolve(base, None).expect("resolved config");
        assert_eq!(
            config.desktop.workspace_layer,
            DesktopWorkspaceLayer::HyprlandSpecial
        );
        assert_eq!(config.projects.max_depth, 2);
        assert!(toml::from_str::<RawConfig>("[desktop]\nunknown = true").is_err());
        assert!(toml::from_str::<RawConfig>("[desktop]\nworkspace_layer = \"sway\"").is_err());
    }

    #[test]
    fn setup_enablement_preserves_config_formatting_and_unrelated_settings() {
        let directory = TestDirectory::new();
        let target = directory.0.join("config.toml");
        fs::write(
            &target,
            "# keep this comment\n[projects]\nmax_depth = 4\n\n[desktop]\nworkspace_layer = \"disabled\" # setup may replace this\n",
        )
        .unwrap();
        fs::set_permissions(&target, fs::Permissions::from_mode(0o640)).unwrap();
        let paths = ConfigPaths {
            global: Some(target.clone()),
            environment: None,
        };

        assert_eq!(
            enable_hyprland_workspace_layer_with(&paths).unwrap(),
            target
        );
        let content = fs::read_to_string(&target).unwrap();
        assert!(content.contains("# keep this comment"));
        assert!(content.contains("max_depth = 4"));
        assert!(content.contains("workspace_layer = \"hyprland-special\""));
        assert_eq!(
            fs::metadata(&target).unwrap().permissions().mode() & 0o777,
            0o640
        );
        let (raw, _, _) = load_raw_from_paths(&paths, None).unwrap();
        assert_eq!(
            resolve(raw, Some(target)).unwrap().desktop.workspace_layer,
            DesktopWorkspaceLayer::HyprlandSpecial
        );
    }

    #[test]
    fn setup_enablement_updates_only_the_active_environment_layer() {
        let directory = TestDirectory::new();
        let global = directory.0.join("global.toml");
        let environment = directory.0.join("environment.toml");
        let global_content = "# inherited\n[projects]\nmax_depth = 4\n";
        fs::write(&global, global_content).unwrap();
        fs::write(&environment, "# active override\n").unwrap();
        let paths = ConfigPaths {
            global: Some(global.clone()),
            environment: Some(environment.clone()),
        };

        assert_eq!(
            enable_hyprland_workspace_layer_with(&paths).unwrap(),
            environment
        );
        assert_eq!(fs::read_to_string(global).unwrap(), global_content);
        let environment_content = fs::read_to_string(&environment).unwrap();
        assert!(environment_content.contains("# active override"));
        assert!(environment_content.contains("workspace_layer = \"hyprland-special\""));
        let (raw, _, _) = load_raw_from_paths(&paths, None).unwrap();
        let config = resolve(raw, Some(environment)).unwrap();
        assert_eq!(config.projects.max_depth, 4);
        assert_eq!(
            config.desktop.workspace_layer,
            DesktopWorkspaceLayer::HyprlandSpecial
        );
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
        let defaults = resolve_daemon_settings(None, None, None).unwrap();
        assert!(defaults.resume_agents);
        assert!(!defaults.persist_terminal_history);

        let mut base: RawConfig =
            toml::from_str("[recovery]\nresume_agents = false\npersist_terminal_history = true")
                .unwrap();
        let next: RawConfig = toml::from_str("[recovery]\nresume_agents = true").unwrap();
        merge(&mut base, next);

        let settings =
            resolve_daemon_settings(base.notifications, base.recovery, base.claude).unwrap();
        assert!(settings.resume_agents);
        assert!(settings.persist_terminal_history);
        assert!(toml::from_str::<RawConfig>("[recovery]\nunknown = true").is_err());
    }

    #[test]
    fn claude_remote_control_defaults_on_and_can_be_disabled_per_layer() {
        let defaults = resolve_daemon_settings(None, None, None).unwrap();
        assert!(defaults.claude_remote_control);

        let mut base: RawConfig = toml::from_str("[claude]\nremote_control = false").unwrap();
        let next: RawConfig =
            toml::from_str("[dashboard]\nfollow_focused_terminal = false").unwrap();
        merge(&mut base, next);
        let settings =
            resolve_daemon_settings(base.notifications, base.recovery, base.claude).unwrap();
        assert!(!settings.claude_remote_control);
        assert!(toml::from_str::<RawConfig>("[claude]\nunknown = true").is_err());
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
                claude_remote_control: true,
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

    #[test]
    fn layered_semantics_are_resolved_only_after_field_merge() {
        let mut base: RawConfig = toml::from_str("[projects]\nmax_depth = 99").unwrap();
        let override_layer: RawConfig = toml::from_str("[projects]\nmax_depth = 4").unwrap();

        assert!(resolve(base.clone(), None).is_err());
        merge(&mut base, override_layer);
        let resolved = resolve(base, None).unwrap();
        assert_eq!(resolved.projects.max_depth, 4);
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
    }
}

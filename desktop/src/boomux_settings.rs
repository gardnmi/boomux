//! Desktop edits the temporary document supplied by `boomux config edit`.
//! Boomux owns validation, layered configuration, conflict checks, and commit.
use std::os::unix::fs::DirBuilderExt;
use std::{
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
};
use toml_edit::{Array, DocumentMut, Item, Value};

const LIMIT: u64 = 1024 * 1024;
pub const EDITOR_FLAG: &str = "--boomux-settings-editor";

#[derive(Clone, Copy)]
pub enum Kind {
    Bool,
    Text,
    Roots,
    Number,
    Choice(&'static [&'static str]),
}
pub struct Field {
    pub key: &'static str,
    pub label: &'static str,
    pub kind: Kind,
}
impl Field {
    pub fn requires_restart(&self) -> bool {
        self.key.starts_with("notifications.")
            || self.key.starts_with("recovery.")
            || self.key == "claude.remote_control"
    }
}
pub const FIELDS: &[Field] = &[
    Field {
        key: "notifications.enabled",
        label: "Notifications",
        kind: Kind::Bool,
    },
    Field {
        key: "notifications.blocked",
        label: "Notify when blocked",
        kind: Kind::Bool,
    },
    Field {
        key: "notifications.completed",
        label: "Notify on completion",
        kind: Kind::Bool,
    },
    Field {
        key: "notifications.sound.enabled",
        label: "Notification sounds",
        kind: Kind::Bool,
    },
    Field {
        key: "notifications.sound.blocked",
        label: "Blocked sound",
        kind: Kind::Text,
    },
    Field {
        key: "notifications.sound.completed",
        label: "Completion sound",
        kind: Kind::Text,
    },
    Field {
        key: "recovery.resume_agents",
        label: "Resume agents after recovery",
        kind: Kind::Bool,
    },
    Field {
        key: "recovery.persist_terminal_history",
        label: "Persist terminal history",
        kind: Kind::Bool,
    },
    Field {
        key: "dashboard.follow_focused_terminal",
        label: "Dashboard follows terminal focus",
        kind: Kind::Bool,
    },
    Field {
        key: "terminal",
        label: "Terminal desktop entry",
        kind: Kind::Text,
    },
    Field {
        key: "projects.roots",
        label: "Project folders (one per line)",
        kind: Kind::Roots,
    },
    Field {
        key: "projects.max_depth",
        label: "Project search depth",
        kind: Kind::Number,
    },
    Field {
        key: "desktop.workspace_layer",
        label: "Workspace layer",
        kind: Kind::Choice(&["disabled", "hyprland-special"]),
    },
    Field {
        key: "claude.remote_control",
        label: "Claude remote control",
        kind: Kind::Bool,
    },
];

#[derive(Clone)]
pub struct Snapshot {
    pub path: PathBuf,
    pub original: Option<String>,
    pub document: DocumentMut,
    inherited: DocumentMut,
    defaults: DocumentMut,
}
impl Snapshot {
    /// Append picker selections without rewriting existing paths or splitting
    /// legitimate whitespace in directory names through the manual text editor.
    pub fn add_project_folders(&mut self, paths: &[PathBuf]) -> Result<bool, String> {
        let mut roots = self
            .value("projects.roots")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let original_len = roots.len();
        for path in paths {
            if !path.is_absolute() {
                return Err("Select an absolute project folder path".into());
            }
            let text = path
                .to_str()
                .ok_or("Project folder paths must be valid UTF-8")?;
            let exists = roots.iter().filter_map(Value::as_str).any(|root| {
                if root == text {
                    return true;
                }
                let relative = if root == "~" {
                    Some("")
                } else {
                    root.strip_prefix("~/")
                };
                relative
                    .and_then(|relative| {
                        std::env::var_os("HOME").map(|home| PathBuf::from(home).join(relative))
                    })
                    .as_deref()
                    == Some(path.as_path())
            });
            if !exists {
                roots.push(text);
            }
        }
        if roots.len() == original_len {
            return Ok(false);
        }
        if self.document.get("projects").is_none() {
            self.document["projects"] = Item::Table(toml_edit::Table::new());
        }
        self.document["projects"]["roots"] = Item::Value(Value::Array(roots));
        Ok(true)
    }

    pub fn notifications_enabled(&self) -> bool {
        self.value("notifications.enabled").and_then(Value::as_bool) == Some(true)
            || self
                .value("notifications.sound.enabled")
                .and_then(Value::as_bool)
                == Some(true)
    }
    pub fn control_enabled(&self, index: usize) -> bool {
        match index {
            1..=3 => self.notifications_enabled(),
            4..=5 => {
                self.notifications_enabled()
                    && self
                        .value("notifications.sound.enabled")
                        .and_then(Value::as_bool)
                        == Some(true)
            }
            _ => true,
        }
    }
    pub fn control_text(&self, index: usize) -> String {
        if index == 0 {
            self.notifications_enabled().to_string()
        } else {
            self.text(index)
        }
    }
    pub fn set_control(&mut self, index: usize, text: &str) -> Result<(), String> {
        if !self.control_enabled(index) {
            return Err("Enable notifications before changing this setting".into());
        }
        self.set(index, Some(text))?;
        if index == 0 && text == "false" {
            self.set(3, Some("false"))?;
        }
        Ok(())
    }

    pub fn value(&self, key: &str) -> Option<&Value> {
        lookup(&self.document, key)
            .or_else(|| lookup(&self.inherited, key))
            .or_else(|| lookup(&self.defaults, key))
    }
    pub fn text(&self, index: usize) -> String {
        match self.value(FIELDS[index].key) {
            Some(Value::Array(values)) => values
                .iter()
                .filter_map(Value::as_str)
                .collect::<Vec<_>>()
                .join("\n"),
            Some(Value::Boolean(value)) => value.value().to_string(),
            Some(Value::Integer(value)) => value.value().to_string(),
            Some(value) => value
                .as_str()
                .map(str::to_owned)
                .unwrap_or_else(|| value.to_string()),
            None => String::new(),
        }
    }
    pub fn set(&mut self, index: usize, text: Option<&str>) -> Result<(), String> {
        let field = &FIELDS[index];
        let value = text
            .map(|text| -> Result<Value, String> {
                Ok(match field.kind {
                    Kind::Bool => Value::from(text.parse::<bool>().map_err(|e| e.to_string())?),
                    Kind::Number => {
                        Value::from(text.parse::<i64>().map_err(|_| "Enter a whole number")?)
                    }
                    Kind::Roots => {
                        let mut array = Array::new();
                        for line in text.lines().filter(|s| !s.trim().is_empty()) {
                            array.push(line.trim());
                        }
                        Value::Array(array)
                    }
                    Kind::Text | Kind::Choice(_) => Value::from(text),
                })
            })
            .transpose()?;
        let mut parts = field.key.split('.').peekable();
        let mut item = self.document.as_item_mut();
        while let Some(part) = parts.next() {
            if parts.peek().is_none() {
                let table = item
                    .as_table_like_mut()
                    .ok_or("Expected a configuration table")?;
                if let Some(mut value) = value {
                    if let Some(existing) = table.get(part).and_then(Item::as_value) {
                        *value.decor_mut() = existing.decor().clone();
                    }
                    table.insert(part, Item::Value(value));
                } else {
                    table.remove(part);
                }
                break;
            }
            if item.get(part).is_none() {
                if value.is_none() {
                    return Ok(());
                }
                item[part] = Item::Table(toml_edit::Table::new());
            }
            item = item.get_mut(part).ok_or("Expected a configuration table")?;
        }
        Ok(())
    }
    pub fn restart_changed(&self) -> bool {
        let original = self
            .original
            .as_deref()
            .unwrap_or("")
            .parse::<DocumentMut>()
            .expect("validated snapshot");
        FIELDS
            .iter()
            .filter(|field| field.requires_restart())
            .any(|field| {
                let before = lookup(&original, field.key)
                    .or_else(|| lookup(&self.inherited, field.key))
                    .or_else(|| lookup(&self.defaults, field.key));
                before.map(display_value) != self.value(field.key).map(display_value)
            })
    }
    pub fn dirty(&self) -> bool {
        self.document.to_string() != self.original.as_deref().unwrap_or("")
    }
}
fn display_value(value: &Value) -> String {
    if let Some(text) = value.as_str() {
        text.to_owned()
    } else if let Some(boolean) = value.as_bool() {
        boolean.to_string()
    } else {
        value.to_string()
    }
}

fn lookup<'a>(document: &'a DocumentMut, key: &str) -> Option<&'a Value> {
    let mut item = document.as_item();
    for part in key.split('.') {
        item = item.get(part)?;
    }
    item.as_value()
}

fn defaults() -> DocumentMut {
    // Use the workspace Boomux library's public defaults for daemon-owned values.
    // The remaining CLI-only presentation defaults mirror the root CLI configuration contract.
    let daemon = boomux::daemon::NotificationDeliverySettings::default();
    let values = [
        daemon.desktop.enabled.to_string(),
        daemon.desktop.blocked.to_string(),
        daemon.desktop.completed.to_string(),
        daemon.sound.enabled.to_string(),
        daemon.sound.blocked,
        daemon.sound.completed,
        daemon.resume_agents.to_string(),
        daemon.persist_terminal_history.to_string(),
        "true".into(),
        String::new(),
        String::new(),
        "3".into(),
        "disabled".into(),
        daemon.claude_remote_control.to_string(),
    ];
    let mut snapshot = Snapshot {
        path: PathBuf::new(),
        original: None,
        document: DocumentMut::new(),
        inherited: DocumentMut::new(),
        defaults: DocumentMut::new(),
    };
    for (index, value) in values.iter().enumerate() {
        snapshot
            .set(index, Some(value))
            .expect("valid Boomux default");
    }
    snapshot.document
}

fn read(path: &Path) -> Result<String, String> {
    let file = fs::File::open(path).map_err(|e| e.to_string())?;
    let mut text = String::new();
    file.take(LIMIT + 1)
        .read_to_string(&mut text)
        .map_err(|e| e.to_string())?;
    if text.len() as u64 > LIMIT {
        return Err("Boomux configuration exceeds 1 MiB".into());
    }
    Ok(text)
}
struct Temporary(PathBuf);
impl Temporary {
    fn new() -> Result<Self, String> {
        let path = std::env::temp_dir().join(format!(
            "boomux-desktop-config-{}-{}",
            std::process::id(),
            fastrand::u64(..)
        ));
        fs::DirBuilder::new()
            .mode(0o700)
            .create(&path)
            .map_err(|e| e.to_string())?;
        Ok(Self(path))
    }
}
impl Drop for Temporary {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

// Coreutils timeout owns the entire process group, including our editor helper.
// Pipe readers retain at most 64 KiB (1 MiB for project discovery).
// All waits happen on a worker thread.
fn run(args: &[&str], editor: Option<String>) -> Result<String, String> {
    run_layer(args, editor, false)
}

pub fn discover_projects() -> Result<boomux::protocol::HostProjectDiscovery, String> {
    let output = run(&["project", "list", "--json"], None)?;
    let envelope: serde_json::Value = serde_json::from_str(&output).map_err(|e| e.to_string())?;
    serde_json::from_value(
        envelope
            .get("data")
            .cloned()
            .ok_or("Missing project discovery result")?,
    )
    .map_err(|e| format!("Invalid project discovery result: {e}"))
}
fn run_layer(args: &[&str], editor: Option<String>, global: bool) -> Result<String, String> {
    let mut command = Command::new("timeout");
    command
        .args([
            "--kill-after=1s",
            if args == ["daemon", "restart"] {
                "30s"
            } else {
                "10s"
            },
            "boomux",
        ])
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if global {
        command.env_remove("BOOMUX_CONFIG");
    }
    if let Some(editor) = editor {
        command.env("VISUAL", &editor).env("EDITOR", editor);
    }
    let mut child = command
        .spawn()
        .map_err(|e| format!("Could not run Boomux: {e}"))?;
    let output = child.stdout.take().ok_or("Missing Boomux output pipe")?;
    let errors = child.stderr.take().ok_or("Missing Boomux error pipe")?;
    let output_limit = if args == ["project", "list", "--json"] {
        LIMIT
    } else {
        65536
    };
    let collect = |reader: Box<dyn Read + Send>| {
        std::thread::spawn(move || {
            let mut text = String::new();
            reader
                .take(output_limit)
                .read_to_string(&mut text)
                .map(|_| text)
                .map_err(|e| e.to_string())
        })
    };
    let output = collect(Box::new(output));
    let errors = collect(Box::new(errors));
    let status = child.wait().map_err(|e| e.to_string())?;
    let stdout = output.join().map_err(|_| "Boomux output reader failed")??;
    let stderr = errors.join().map_err(|_| "Boomux error reader failed")??;
    if !status.success() {
        return Err(format!("Boomux {status}: {}", stderr.trim()));
    }
    Ok(stdout)
}
pub fn restart() -> Result<(), String> {
    run(&["daemon", "restart"], None).map(|_| ())
}
pub fn load() -> Result<Snapshot, String> {
    let path = PathBuf::from(run(&["config", "path"], None)?.trim_end_matches('\n'));
    run(&["config", "validate"], None)?;
    let original = match fs::symlink_metadata(&path) {
        Ok(metadata) if metadata.file_type().is_file() => Some(read(&path)?),
        Ok(_) => return Err("Boomux configuration must be a regular file".into()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(e.to_string()),
    };
    let document = original
        .as_deref()
        .unwrap_or("")
        .parse::<DocumentMut>()
        .map_err(|e| e.to_string())?;
    let mut inherited = DocumentMut::new();
    if std::env::var_os("BOOMUX_CONFIG").is_some() {
        let global =
            PathBuf::from(run_layer(&["config", "path"], None, true)?.trim_end_matches('\n'));
        if global.is_file() {
            inherited = read(&global)?
                .parse::<DocumentMut>()
                .map_err(|e| e.to_string())?;
        }
    }
    Ok(Snapshot {
        path,
        original,
        document,
        inherited,
        defaults: defaults(),
    })
}
fn quote(path: &Path) -> String {
    format!("'{}'", path.to_string_lossy().replace('\'', "'\\''"))
}
pub fn save(snapshot: &Snapshot) -> Result<Snapshot, String> {
    let request = Temporary::new()?;
    fs::write(request.0.join("candidate"), snapshot.document.to_string())
        .map_err(|e| e.to_string())?;
    if let Some(original) = &snapshot.original {
        fs::write(request.0.join("baseline"), original).map_err(|e| e.to_string())?;
    }
    // The owner chooses the target. Refuse to save if BOOMUX_CONFIG changed since loading.
    let current = PathBuf::from(run(&["config", "path"], None)?.trim_end_matches('\n'));
    if current != snapshot.path {
        return Err("Active Boomux configuration changed; reload before saving".into());
    }
    fs::write(
        request.0.join("target"),
        snapshot.path.as_os_str().as_encoded_bytes(),
    )
    .map_err(|e| e.to_string())?;
    let executable = std::env::current_exe().map_err(|e| e.to_string())?;
    run(
        &["config", "edit"],
        Some(format!(
            "{} {EDITOR_FLAG} {}",
            quote(&executable),
            quote(&request.0)
        )),
    )?;
    load()
}
pub fn editor(request: &Path, working: &Path) -> Result<(), String> {
    let baseline_path = request.join("baseline");
    if baseline_path.exists() {
        if read(&baseline_path)? != read(working)? {
            return Err("Configuration changed since it was loaded; reload before saving".into());
        }
    } else {
        let target = read(&request.join("target"))?;
        if fs::symlink_metadata(target).is_ok() {
            return Err(
                "Configuration was created since it was loaded; reload before saving".into(),
            );
        }
    }
    let candidate = read(&request.join("candidate"))?;
    let mut file = fs::OpenOptions::new()
        .write(true)
        .truncate(true)
        .open(working)
        .map_err(|e| e.to_string())?;
    file.write_all(candidate.as_bytes())
        .and_then(|()| file.sync_all())
        .map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn notification_master_disables_both_channels_and_gates_dependent_controls() {
        let mut snapshot = Snapshot {
            path: PathBuf::new(),
            original: None,
            document: DocumentMut::new(),
            inherited: DocumentMut::new(),
            defaults: defaults(),
        };
        assert!(!snapshot.notifications_enabled());
        assert!((1..=5).all(|index| !snapshot.control_enabled(index)));
        snapshot.set_control(0, "true").unwrap();
        assert!((1..=3).all(|index| snapshot.control_enabled(index)));
        assert!(!snapshot.control_enabled(4));
        snapshot.set_control(3, "true").unwrap();
        assert!(snapshot.control_enabled(4));
        let blocked = snapshot.text(1);
        let sound = snapshot.text(4);
        snapshot.set_control(0, "false").unwrap();
        assert_eq!(snapshot.text(0), "false");
        assert_eq!(snapshot.text(3), "false");
        assert_eq!(snapshot.text(1), blocked);
        assert_eq!(snapshot.text(4), sound);
        assert!(snapshot.set_control(3, "true").is_err());
        assert!(snapshot.control_enabled(6));
        // A configuration enabling only sound still appears enabled in the master switch.
        snapshot.set(3, Some("true")).unwrap();
        assert_eq!(snapshot.control_text(0), "true");
        snapshot.set_control(0, "false").unwrap();
        assert!(!snapshot.notifications_enabled());
    }

    #[test]
    fn folder_picker_appends_inherited_roots_and_preserves_path_text() {
        let mut snapshot = Snapshot {
            path: PathBuf::new(),
            original: None,
            document: "# user comment\n[projects]\nmax_depth = 4\n"
                .parse()
                .unwrap(),
            inherited: "[projects]\nroots = ['/existing']\n".parse().unwrap(),
            defaults: defaults(),
        };
        assert!(
            snapshot
                .add_project_folders(&[
                    PathBuf::from("/existing"),
                    PathBuf::from("/Side projects"),
                    PathBuf::from("/Side projects"),
                    PathBuf::from("/ whitespace \nfolder "),
                ])
                .unwrap()
        );
        let roots = snapshot
            .value("projects.roots")
            .unwrap()
            .as_array()
            .unwrap();
        assert_eq!(
            roots.iter().filter_map(Value::as_str).collect::<Vec<_>>(),
            ["/existing", "/Side projects", "/ whitespace \nfolder "]
        );
        assert_eq!(snapshot.text(11), "4");
        assert!(snapshot.document.to_string().contains("# user comment"));
        assert!(!snapshot.restart_changed());
    }

    #[test]
    fn folder_picker_cancel_duplicates_and_invalid_paths_do_not_change_settings() {
        let mut snapshot = Snapshot {
            path: PathBuf::new(),
            original: None,
            document: "[projects]\nroots = ['/existing']\n".parse().unwrap(),
            inherited: DocumentMut::new(),
            defaults: defaults(),
        };
        let original = snapshot.document.to_string();
        assert!(!snapshot.add_project_folders(&[]).unwrap());
        assert!(
            !snapshot
                .add_project_folders(&[PathBuf::from("/existing")])
                .unwrap()
        );
        assert!(
            snapshot
                .add_project_folders(&[PathBuf::from("/new"), PathBuf::from("relative")])
                .is_err()
        );
        assert_eq!(snapshot.document.to_string(), original);
    }

    #[test]
    fn project_roots_preserve_spaces_and_do_not_require_restart() {
        let mut snapshot = Snapshot {
            path: PathBuf::new(),
            original: None,
            document: DocumentMut::new(),
            inherited: DocumentMut::new(),
            defaults: defaults(),
        };
        snapshot
            .set_control(10, "~/Work\n/home/person/Side projects")
            .unwrap();
        let roots = snapshot
            .value("projects.roots")
            .unwrap()
            .as_array()
            .unwrap();
        assert_eq!(roots.len(), 2);
        assert_eq!(
            roots.get(1).unwrap().as_str(),
            Some("/home/person/Side projects")
        );
        assert!(!snapshot.restart_changed());
        snapshot.set_control(10, "").unwrap();
        assert!(
            snapshot
                .value("projects.roots")
                .unwrap()
                .as_array()
                .unwrap()
                .is_empty()
        );
        assert!(!snapshot.restart_changed());
    }

    #[test]
    fn only_daemon_settings_require_restart_and_only_when_values_change() {
        let mut snapshot = Snapshot {
            path: PathBuf::new(),
            original: None,
            document: DocumentMut::new(),
            inherited: DocumentMut::new(),
            defaults: defaults(),
        };
        assert!(!snapshot.restart_changed());
        snapshot.set(11, Some("4")).unwrap();
        assert!(!snapshot.restart_changed());
        snapshot.set(4, Some(&snapshot.text(4))).unwrap();
        assert!(!snapshot.restart_changed());
        snapshot.set(0, Some("true")).unwrap();
        assert!(snapshot.restart_changed());
        snapshot.set(0, Some("false")).unwrap();
        assert!(!snapshot.restart_changed());
        for (index, field) in FIELDS.iter().enumerate() {
            assert_eq!(field.requires_restart(), matches!(index, 0..=7 | 13));
        }
    }

    #[test]
    fn displayed_values_resolve_layers_without_copying_them_into_the_draft() {
        let source = "[notifications]\nenabled = false\n";
        let mut snapshot = Snapshot {
            path: PathBuf::new(),
            original: Some(source.into()),
            document: source.parse().unwrap(),
            inherited:
                "[notifications]\nenabled = true\n[notifications.sound]\nblocked = 'custom-sound'\n"
                    .parse()
                    .unwrap(),
            defaults: defaults(),
        };
        assert_eq!(snapshot.text(0), "false");
        assert_eq!(snapshot.text(4), "custom-sound");
        assert_eq!(snapshot.text(11), "3");
        assert_eq!(
            snapshot.text(2),
            boomux::daemon::NotificationDeliverySettings::default()
                .desktop
                .completed
                .to_string()
        );
        assert!(
            FIELDS
                .iter()
                .all(|field| snapshot.value(field.key).is_some())
        );
        assert!(!snapshot.dirty());
        snapshot.set(0, Some("true")).unwrap();
        assert!(snapshot.dirty());
        assert!(lookup(&snapshot.document, "notifications.sound.blocked").is_none());
        assert!(lookup(&snapshot.document, "projects.max_depth").is_none());
    }

    #[test]
    fn changing_one_setting_preserves_comments_and_other_fields() {
        let source = "# preferences\n[notifications]\nenabled = false # keep this\ncompleted = true\n[projects]\nroots = ['~/Work']\n";
        let mut snapshot = Snapshot {
            path: PathBuf::new(),
            original: Some(source.into()),
            document: source.parse().unwrap(),
            inherited: DocumentMut::new(),
            defaults: defaults(),
        };
        snapshot.set(0, Some("true")).unwrap();
        let updated = snapshot.document.to_string();
        assert!(updated.contains("# preferences"));
        assert!(updated.contains("enabled = true # keep this"));
        assert!(updated.contains("roots = ['~/Work']"));
        snapshot.set(0, None).unwrap();
        assert!(lookup(&snapshot.document, "notifications.enabled").is_none());
        assert_eq!(
            snapshot
                .value("notifications.completed")
                .and_then(Value::as_bool),
            Some(true)
        );
    }
    #[test]
    fn stale_editor_does_not_replace_working_document() {
        let request = Temporary::new().unwrap();
        let working = request.0.join("working");
        fs::write(request.0.join("baseline"), "old").unwrap();
        fs::write(&working, "new").unwrap();
        fs::write(request.0.join("candidate"), "candidate").unwrap();
        assert!(editor(&request.0, &working).is_err());
        assert_eq!(read(&working).unwrap(), "new");
    }
}

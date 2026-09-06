use crate::{MotionSpeed, PaneCornerStyle, PaneLayoutMode, WorkspacePaneMode};
use std::{
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
};

#[derive(Clone, Debug, PartialEq)]
pub struct Settings {
    pub dismissed_desktop_update: String,
    pub dismissed_boomux_update: String,
    pub settings_restart_pending: bool,
    pub sidebar_visible: bool,
    pub pane_headings_visible: bool,
    pub pane_corner_style: PaneCornerStyle,
    pub pane_gap: f32,
    pub focus_highlight_strength: u8,
    pub motion_speed: MotionSpeed,
    pub workspace_pane_mode: WorkspacePaneMode,
    pub pane_layout_mode: PaneLayoutMode,
    pub confirm_destructive_actions: bool,
}
impl Default for Settings {
    fn default() -> Self {
        Self {
            dismissed_desktop_update: String::new(),
            dismissed_boomux_update: String::new(),
            settings_restart_pending: false,
            sidebar_visible: true,
            pane_headings_visible: true,
            pane_corner_style: PaneCornerStyle::Rounded,
            pane_gap: 8.0,
            focus_highlight_strength: 100,
            motion_speed: MotionSpeed::Smooth,
            workspace_pane_mode: WorkspacePaneMode::Workspace,
            pane_layout_mode: PaneLayoutMode::Tiled,
            confirm_destructive_actions: true,
        }
    }
}
pub fn path() -> Option<PathBuf> {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| {
            std::env::var_os("HOME")
                .map(PathBuf::from)
                .filter(|p| p.is_absolute())
                .map(|p| p.join(".config"))
        })
        .map(|p| p.join("boomux-desktop/settings.toml"))
}
impl Settings {
    pub fn load(path: &Path) -> Result<Self, String> {
        let file = match fs::File::open(path) {
            Ok(file) => file,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(e) => return Err(e.to_string()),
        };
        let mut text = String::new();
        file.take(65537)
            .read_to_string(&mut text)
            .map_err(|e| e.to_string())?;
        if text.len() > 65536 {
            return Err("settings exceed 64 KiB".into());
        }
        Self::parse(&text)
    }
    fn parse(text: &str) -> Result<Self, String> {
        let values: toml::Table = text.parse().map_err(|e: toml::de::Error| e.to_string())?;
        let mut s = Self::default();
        for (key, value) in values {
            let invalid = || format!("invalid Desktop setting: {key}");
            match key.as_str() {
                "dismissed_desktop_update" | "dismissed_boomux_update" => {
                    let text = value
                        .as_str()
                        .filter(|text| crate::updates::valid_dismissal(text))
                        .ok_or_else(invalid)?;
                    if key == "dismissed_desktop_update" {
                        s.dismissed_desktop_update = text.into();
                    } else {
                        s.dismissed_boomux_update = text.into();
                    }
                }
                "settings_restart_pending" => {
                    s.settings_restart_pending = value.as_bool().ok_or_else(invalid)?
                }
                "sidebar_visible" => s.sidebar_visible = value.as_bool().ok_or_else(invalid)?,
                "pane_headings_visible" => {
                    s.pane_headings_visible = value.as_bool().ok_or_else(invalid)?
                }
                "confirm_destructive_actions" => {
                    s.confirm_destructive_actions = value.as_bool().ok_or_else(invalid)?
                }
                "pane_gap" => {
                    let n = value
                        .as_float()
                        .or_else(|| value.as_integer().map(|n| n as f64))
                        .ok_or_else(invalid)?;
                    if !n.is_finite() || !(0.0..=32.0).contains(&n) {
                        return Err(invalid());
                    }
                    s.pane_gap = n as f32;
                }
                "focus_highlight_strength" => {
                    let n = value
                        .as_integer()
                        .filter(|n| (0..=100).contains(n))
                        .ok_or_else(invalid)?;
                    s.focus_highlight_strength = n as u8;
                }
                "pane_corner_style" => {
                    s.pane_corner_style = match value.as_str() {
                        Some("rounded") => PaneCornerStyle::Rounded,
                        Some("square") => PaneCornerStyle::Square,
                        Some("mixed") => PaneCornerStyle::Mixed,
                        _ => return Err(invalid()),
                    }
                }
                "motion_speed" => {
                    s.motion_speed = match value.as_str() {
                        Some("instant") => MotionSpeed::Instant,
                        Some("fast") => MotionSpeed::Fast,
                        Some("smooth") => MotionSpeed::Smooth,
                        _ => return Err(invalid()),
                    }
                }
                "workspace_pane_mode" => {
                    s.workspace_pane_mode = match value.as_str() {
                        Some("workspace") => WorkspacePaneMode::Workspace,
                        Some("mixed") => WorkspacePaneMode::Mixed,
                        _ => return Err(invalid()),
                    }
                }
                "pane_layout_mode" => {
                    s.pane_layout_mode = match value.as_str() {
                        Some("tiled") => PaneLayoutMode::Tiled,
                        Some("tabbed") => PaneLayoutMode::Tabbed,
                        _ => return Err(invalid()),
                    }
                }
                _ => return Err(format!("unknown Desktop setting: {key}")),
            }
        }
        if s.pane_layout_mode == PaneLayoutMode::Tabbed {
            s.workspace_pane_mode = WorkspacePaneMode::Workspace;
        }
        Ok(s)
    }
    fn encode(&self) -> String {
        format!(
            "# Boomux Desktop preferences; shared Boomux configuration is separate.\nsidebar_visible = {}\npane_headings_visible = {}\npane_corner_style = \"{}\"\npane_gap = {}\nfocus_highlight_strength = {}\nmotion_speed = \"{}\"\nworkspace_pane_mode = \"{}\"\npane_layout_mode = \"{}\"\nconfirm_destructive_actions = {}\nsettings_restart_pending = {}\ndismissed_desktop_update = \"{}\"\ndismissed_boomux_update = \"{}\"\n",
            self.sidebar_visible,
            self.pane_headings_visible,
            match self.pane_corner_style {
                PaneCornerStyle::Rounded => "rounded",
                PaneCornerStyle::Square => "square",
                PaneCornerStyle::Mixed => "mixed",
            },
            self.pane_gap,
            self.focus_highlight_strength,
            match self.motion_speed {
                MotionSpeed::Instant => "instant",
                MotionSpeed::Fast => "fast",
                MotionSpeed::Smooth => "smooth",
            },
            match self.workspace_pane_mode {
                WorkspacePaneMode::Workspace => "workspace",
                WorkspacePaneMode::Mixed => "mixed",
            },
            match self.pane_layout_mode {
                PaneLayoutMode::Tiled => "tiled",
                PaneLayoutMode::Tabbed => "tabbed",
            },
            self.confirm_destructive_actions,
            self.settings_restart_pending,
            self.dismissed_desktop_update,
            self.dismissed_boomux_update
        )
    }
    fn save(&self, path: &Path) -> Result<(), String> {
        let parent = path.parent().ok_or("invalid settings path")?;
        fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        let temporary = parent.join(format!(
            ".settings-{}-{}.tmp",
            std::process::id(),
            fastrand::u64(..)
        ));
        let result = (|| -> std::io::Result<()> {
            let mut file = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&temporary)?;
            file.write_all(self.encode().as_bytes())?;
            file.sync_all()?;
            fs::rename(&temporary, path)
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temporary);
        }
        result.map_err(|e| e.to_string())
    }
}

// One background writer, with only the latest pending snapshot retained. Closing
// the sender lets the worker drain the final snapshot and exit without blocking GPUI.
pub fn writer(
    path: PathBuf,
) -> (
    async_channel::Sender<Settings>,
    async_channel::Receiver<Result<(), String>>,
    async_channel::Receiver<()>,
) {
    let (send, receive) = async_channel::bounded::<Settings>(1);
    let (status, updates) = async_channel::bounded(1);
    let (finished, completion) = async_channel::bounded::<()>(1);
    std::thread::spawn(move || {
        while let Ok(settings) = receive.recv_blocking() {
            let _ = status.force_send(settings.save(&path));
        }
        drop(finished);
    });
    (send, updates, completion)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn update_dismissals_round_trip_independently() {
        let settings = Settings {
            dismissed_desktop_update: "0.2.0".into(),
            dismissed_boomux_update: "1.10.0".into(),
            ..Settings::default()
        };
        assert_eq!(Settings::parse(&settings.encode()).unwrap(), settings);
        assert!(Settings::parse("dismissed_desktop_update = 'not a version'").is_err());
        assert!(
            Settings::parse("")
                .unwrap()
                .dismissed_desktop_update
                .is_empty()
        );
    }

    #[test]
    fn restart_reminder_survives_reopening_desktop() {
        let settings = Settings {
            settings_restart_pending: true,
            ..Settings::default()
        };
        assert!(
            Settings::parse(&settings.encode())
                .unwrap()
                .settings_restart_pending
        );
        assert!(!Settings::parse("").unwrap().settings_restart_pending);
    }
    #[test]
    fn preferences_round_trip_and_missing_fields_use_defaults() {
        let settings = Settings {
            pane_gap: 0.0,
            motion_speed: MotionSpeed::Instant,
            pane_headings_visible: false,
            ..Settings::default()
        };
        assert_eq!(Settings::parse(&settings.encode()).unwrap(), settings);
        assert_eq!(Settings::parse("").unwrap(), Settings::default());
        assert_eq!(
            Settings::parse("pane_layout_mode = 'tabbed'\nworkspace_pane_mode = 'mixed'")
                .unwrap()
                .workspace_pane_mode,
            WorkspacePaneMode::Workspace
        );
    }
    #[test]
    fn malformed_or_out_of_range_preferences_are_rejected() {
        for text in [
            "pane_gap = nan",
            "pane_gap = 33",
            "focus_highlight_strength = -1",
            "motion_speed = 'slow'",
            "sidebar_visible = 'true'",
            "unknown = 3",
            "broken[",
        ] {
            assert!(Settings::parse(text).is_err(), "{text}");
        }
    }
    #[test]
    fn worker_drains_final_snapshot_and_saves_atomically() {
        let dir = std::env::temp_dir().join(format!("boomux-settings-{}", fastrand::u64(..)));
        let path = dir.join("settings.toml");
        let (send, updates, _) = writer(path.clone());
        for pane_gap in 0..=32 {
            send.force_send(Settings {
                pane_gap: pane_gap as f32,
                ..Settings::default()
            })
            .unwrap();
        }
        drop(send);
        while let Ok(result) = updates.recv_blocking() {
            result.unwrap();
        }
        assert_eq!(Settings::load(&path).unwrap().pane_gap, 32.0);
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 1);
        fs::remove_dir_all(dir).unwrap();
    }
}

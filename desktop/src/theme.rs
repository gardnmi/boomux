use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{OnceLock, RwLock};

use notify::{RecommendedWatcher, RecursiveMode, Watcher};

const MAX_THEME_BYTES: u64 = 64 * 1024;

static CANVAS: AtomicU32 = AtomicU32::new(0x11111b);
static PANEL: AtomicU32 = AtomicU32::new(0x181825);
static SURFACE: AtomicU32 = AtomicU32::new(0x1e1e2e);
static RAISED: AtomicU32 = AtomicU32::new(0x29293d);
static HOVER: AtomicU32 = AtomicU32::new(0x313244);
static BORDER: AtomicU32 = AtomicU32::new(0x45475a);
static TEXT: AtomicU32 = AtomicU32::new(0xcdd6f4);
static TEXT_SECONDARY: AtomicU32 = AtomicU32::new(0xa6adc8);
static TEXT_MUTED: AtomicU32 = AtomicU32::new(0x7f849c);
static TEXT_SUBTLE: AtomicU32 = AtomicU32::new(0x6c7086);
static ACCENT: AtomicU32 = AtomicU32::new(0xcba6f7);
static SELECTION: AtomicU32 = AtomicU32::new(0xf5e0dc);
static DANGER: AtomicU32 = AtomicU32::new(0xf38ba8);
static SUCCESS: AtomicU32 = AtomicU32::new(0xa6e3a1);
static WARNING: AtomicU32 = AtomicU32::new(0xf9e2af);
static TERMINAL: OnceLock<RwLock<TerminalTheme>> = OnceLock::new();

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TerminalTheme {
    pub foreground: u32,
    pub background: u32,
    pub cursor: u32,
    pub ansi: [u32; 16],
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AppTheme {
    pub canvas: u32,
    pub panel: u32,
    pub surface: u32,
    pub raised: u32,
    pub hover: u32,
    pub border: u32,
    pub text: u32,
    pub text_secondary: u32,
    pub text_muted: u32,
    pub text_subtle: u32,
    pub accent: u32,
    pub selection: u32,
    pub danger: u32,
    pub success: u32,
    pub warning: u32,
    pub terminal: TerminalTheme,
}

impl Default for AppTheme {
    fn default() -> Self {
        Self {
            canvas: 0x11111b,
            panel: 0x181825,
            surface: 0x1e1e2e,
            raised: 0x29293d,
            hover: 0x313244,
            border: 0x45475a,
            text: 0xcdd6f4,
            text_secondary: 0xa6adc8,
            text_muted: 0x7f849c,
            text_subtle: 0x6c7086,
            accent: 0xcba6f7,
            selection: 0xf5e0dc,
            danger: 0xf38ba8,
            success: 0xa6e3a1,
            warning: 0xf9e2af,
            terminal: TerminalTheme {
                foreground: 0xcdd6f4,
                background: 0x11111b,
                cursor: 0xcdd6f4,
                ansi: [
                    0x1e1e2e, 0xf38ba8, 0xa6e3a1, 0xf9e2af, 0x89b4fa, 0xf5c2e7, 0x94e2d5, 0xcdd6f4,
                    0x45475a, 0xf38ba8, 0xa6e3a1, 0xf9e2af, 0x89b4fa, 0xf5c2e7, 0x94e2d5, 0xffffff,
                ],
            },
        }
    }
}

impl AppTheme {
    pub fn load_omarchy(path: &Path) -> Result<Self, String> {
        let metadata = fs::metadata(path)
            .map_err(|error| format!("could not inspect Omarchy theme: {error}"))?;
        if metadata.len() > MAX_THEME_BYTES {
            return Err("Omarchy colors.toml exceeds the 64 KiB limit".into());
        }
        let source = fs::read_to_string(path)
            .map_err(|error| format!("could not read Omarchy theme: {error}"))?;
        let values = source
            .parse::<toml::Table>()
            .map_err(|error| format!("could not parse Omarchy theme: {error}"))?;
        Self::from_colors(&values)
    }

    fn from_colors(values: &toml::Table) -> Result<Self, String> {
        let fallback = Self::default();
        let value = |keys: &[&str], default: u32| {
            keys.iter()
                .find_map(|key| values.get(*key).and_then(toml::Value::as_str))
                .and_then(parse_hex_color)
                .unwrap_or(default)
        };
        let background = value(&["background", "bg", "color0"], fallback.surface);
        let foreground = value(&["foreground", "fg", "color7"], fallback.text);
        let muted = value(
            &["muted", "dark_foreground", "dark_fg", "color8"],
            fallback.border,
        );
        let dark_foreground = value(&["dark_foreground", "dark_fg", "muted", "color8"], muted);
        let light_foreground = value(
            &["light_foreground", "light_fg", "foreground", "fg", "color7"],
            foreground,
        );
        let accent = value(&["accent", "blue", "color4"], fallback.accent);
        let red = value(&["red", "color1"], fallback.danger);
        let green = value(&["green", "color2"], fallback.success);
        let yellow = value(&["yellow", "color3"], fallback.warning);
        let magenta = value(&["magenta", "purple", "color5"], fallback.terminal.ansi[5]);
        let cyan = value(&["cyan", "color6"], fallback.terminal.ansi[6]);
        let bright_foreground = value(&["bright_foreground", "bright_fg", "color15"], foreground);
        let semantic = [
            background,
            red,
            green,
            yellow,
            value(&["blue", "color4"], accent),
            magenta,
            cyan,
            foreground,
            muted,
            value(&["bright_red", "color9"], red),
            value(&["bright_green", "color10"], green),
            value(&["bright_yellow", "color11"], yellow),
            value(&["bright_blue", "color12"], accent),
            value(&["bright_magenta", "bright_purple", "color13"], magenta),
            value(&["bright_cyan", "color14"], cyan),
            bright_foreground,
        ];
        let ansi = std::array::from_fn(|index| value(&[&format!("color{index}")], semantic[index]));
        let panel = value(
            &["dark_background", "dark_bg"],
            mix_rgb(background, 0x000000, 0.25),
        );
        let canvas = value(
            &["darker_background", "darker_bg"],
            mix_rgb(background, 0x000000, 0.5),
        );
        let hover = value(
            &["lighter_background", "lighter_bg"],
            mix_rgb(background, foreground, 0.14),
        );
        let selection = value(&["selection", "selection_background"], muted);

        Ok(Self {
            canvas,
            panel,
            surface: background,
            raised: mix_rgb(background, hover, 0.5),
            hover,
            border: muted,
            text: foreground,
            text_secondary: light_foreground,
            text_muted: mix_rgb(light_foreground, dark_foreground, 0.5),
            text_subtle: dark_foreground,
            accent,
            selection,
            danger: red,
            success: green,
            warning: yellow,
            terminal: TerminalTheme {
                foreground,
                background,
                cursor: value(&["cursor", "bright_foreground", "color15"], foreground),
                ansi,
            },
        })
    }
}

pub fn install(theme: AppTheme) {
    for (slot, value) in [
        (&CANVAS, theme.canvas),
        (&PANEL, theme.panel),
        (&SURFACE, theme.surface),
        (&RAISED, theme.raised),
        (&HOVER, theme.hover),
        (&BORDER, theme.border),
        (&TEXT, theme.text),
        (&TEXT_SECONDARY, theme.text_secondary),
        (&TEXT_MUTED, theme.text_muted),
        (&TEXT_SUBTLE, theme.text_subtle),
        (&ACCENT, theme.accent),
        (&SELECTION, theme.selection),
        (&DANGER, theme.danger),
        (&SUCCESS, theme.success),
        (&WARNING, theme.warning),
    ] {
        slot.store(value, Ordering::Release);
    }
    *TERMINAL
        .get_or_init(|| RwLock::new(AppTheme::default().terminal))
        .write()
        .unwrap() = theme.terminal;
}

pub fn current_terminal() -> TerminalTheme {
    *TERMINAL
        .get_or_init(|| RwLock::new(AppTheme::default().terminal))
        .read()
        .unwrap()
}

pub fn resolve_legacy(color: u32) -> u32 {
    let load = |slot: &AtomicU32| slot.load(Ordering::Acquire);
    match color {
        0x11111b => load(&CANVAS),
        0x181825 => load(&PANEL),
        0x1e1e2e => load(&SURFACE),
        0x29293d => load(&RAISED),
        0x313244 => load(&HOVER),
        0x45475a => load(&BORDER),
        0xcdd6f4 => load(&TEXT),
        0xa6adc8 => load(&TEXT_SECONDARY),
        0x7f849c => load(&TEXT_MUTED),
        0x6c7086 => load(&TEXT_SUBTLE),
        0xcba6f7 | 0x89b4fa => load(&ACCENT),
        0xf5e0dc | 0xf5e0e6 => load(&SELECTION),
        0xf38ba8 => load(&DANGER),
        0xa6e3a1 => load(&SUCCESS),
        0xf9e2af => load(&WARNING),
        0x252536 => mix_rgb(load(&SURFACE), load(&TEXT), 0.05),
        0x25283c | 0x202235 => mix_rgb(load(&CANVAS), load(&ACCENT), 0.12),
        0x585b70 => mix_rgb(load(&BORDER), load(&TEXT), 0.2),
        0x89384c => mix_rgb(load(&CANVAS), load(&DANGER), 0.45),
        0x365a8c => mix_rgb(load(&SURFACE), load(&ACCENT), 0.35),
        _ => color,
    }
}

pub struct ThemeWatcher {
    _watcher: RecommendedWatcher,
    pub updates: async_channel::Receiver<()>,
}

impl ThemeWatcher {
    pub fn new(state_directory: &Path) -> Result<Self, String> {
        let (sender, updates) = async_channel::bounded(1);
        let mut watcher =
            notify::recommended_watcher(move |event: notify::Result<notify::Event>| {
                if event.is_ok() {
                    let _ = sender.try_send(());
                }
            })
            .map_err(|error| format!("could not start Omarchy theme watcher: {error}"))?;
        watcher
            .watch(state_directory, RecursiveMode::NonRecursive)
            .map_err(|error| format!("could not watch Omarchy theme: {error}"))?;
        Ok(Self {
            _watcher: watcher,
            updates,
        })
    }
}

pub fn omarchy_state_directory() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .filter(|home| !home.is_empty())
        .map(PathBuf::from)
        .map(|home| home.join(".local/state/omarchy/current"))
        .filter(|path| path.is_dir())
}

pub fn omarchy_colors_path(state_directory: &Path) -> PathBuf {
    state_directory.join("theme/colors.toml")
}

fn parse_hex_color(value: &str) -> Option<u32> {
    let hex = value.strip_prefix('#')?;
    (hex.len() == 6)
        .then(|| u32::from_str_radix(hex, 16).ok())
        .flatten()
}

fn mix_rgb(from: u32, to: u32, amount: f32) -> u32 {
    let mix = |shift: u32| {
        let start = ((from >> shift) & 0xff_u32) as f32;
        let end = ((to >> shift) & 0xff_u32) as f32;
        (start * (1.0 - amount) + end * amount).round() as u32
    };
    (mix(16) << 16) | (mix(8) << 8) | mix(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_semantic_and_ansi_colors_with_omarchy_fallbacks() {
        let values = r##"
            accent = "#112233"
            background = "#202020"
            foreground = "#eeeeee"
            color1 = "#ff0000"
            color2 = "#00ff00"
            color3 = "#ffff00"
            color5 = "#ff00ff"
            color6 = "#00ffff"
            color8 = "#777777"
        "##
        .parse::<toml::Table>()
        .unwrap();
        let theme = AppTheme::from_colors(&values).unwrap();
        assert_eq!(theme.accent, 0x112233);
        assert_eq!(theme.terminal.ansi[1], 0xff0000);
        assert_eq!(theme.terminal.ansi[9], 0xff0000);
        assert_eq!(theme.panel, 0x181818);
        assert_eq!(theme.canvas, 0x101010);
        assert_eq!(theme.terminal.background, 0x202020);
    }

    #[test]
    fn uses_omarchy_surface_and_foreground_roles_for_light_themes() {
        let values = r##"
            mode = "light"
            background = "#eff1f5"
            dark_background = "#e3e4e8"
            darker_background = "#d7d8dc"
            lighter_background = "#dce0e8"
            foreground = "#4c4f69"
            light_foreground = "#5c5f77"
            dark_foreground = "#9ca0b0"
            muted = "#acb0be"
            selection = "#ccd0da"
        "##
        .parse::<toml::Table>()
        .unwrap();

        let theme = AppTheme::from_colors(&values).unwrap();
        assert_eq!(theme.surface, 0xeff1f5);
        assert_eq!(theme.panel, 0xe3e4e8);
        assert_eq!(theme.canvas, 0xd7d8dc);
        assert_eq!(theme.hover, 0xdce0e8);
        assert_eq!(theme.text_secondary, 0x5c5f77);
        assert_eq!(theme.text_subtle, 0x9ca0b0);
        assert_eq!(theme.selection, 0xccd0da);
        assert_eq!(theme.terminal.background, 0xeff1f5);
    }

    #[test]
    fn invalid_colors_fall_back_without_panicking() {
        assert_eq!(parse_hex_color("#abcdef"), Some(0xabcdef));
        assert_eq!(parse_hex_color("abcdef"), None);
        assert_eq!(parse_hex_color("#nothex"), None);
    }
}

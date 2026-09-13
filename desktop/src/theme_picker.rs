//! Local palette selection; previews never reconfigure live terminals.
use crate::*;
use std::sync::OnceLock;

pub(crate) struct Preset {
    pub id: String,
    pub name: String,
    pub theme: AppTheme,
}

pub(crate) fn presets() -> &'static [Preset] {
    static PRESETS: OnceLock<Vec<Preset>> = OnceLock::new();
    PRESETS.get_or_init(|| {
        let values: Vec<serde_json::Value> =
            serde_json::from_str(include_str!("../assets/themes.json")).expect("bundled palettes");
        values
            .into_iter()
            .map(|v| {
                let color = |value: &serde_json::Value| {
                    u32::from_str_radix(
                        value
                            .as_str()
                            .expect("palette color")
                            .trim_start_matches('#'),
                        16,
                    )
                    .expect("hex palette color")
                };
                let c = |key: &str| color(&v[key]);
                let terminal = &v["terminal"];
                let t = |key: &str| color(&terminal[key]);
                let ansi = [
                    "black",
                    "red",
                    "green",
                    "yellow",
                    "blue",
                    "magenta",
                    "cyan",
                    "white",
                    "brightBlack",
                    "brightRed",
                    "brightGreen",
                    "brightYellow",
                    "brightBlue",
                    "brightMagenta",
                    "brightCyan",
                    "brightWhite",
                ]
                .map(t);
                Preset {
                    id: v["id"].as_str().unwrap().into(),
                    name: v["name"].as_str().unwrap().into(),
                    theme: AppTheme {
                        canvas: c("deep"),
                        panel: c("surface"),
                        surface: c("bg"),
                        raised: c("raised"),
                        hover: c("raised"),
                        border: c("border"),
                        text: c("fg"),
                        text_secondary: c("fg"),
                        text_muted: c("muted"),
                        text_subtle: c("muted"),
                        accent: c("accent"),
                        selection: c("selection"),
                        danger: ansi[1],
                        success: ansi[2],
                        warning: ansi[3],
                        terminal: theme::TerminalTheme {
                            background: t("background"),
                            foreground: t("foreground"),
                            cursor: t("cursor"),
                            ansi,
                        },
                    },
                }
            })
            .collect()
    })
}

pub(crate) fn valid_id(id: &str) -> bool {
    id == "system" || presets().iter().any(|p| p.id == id)
}
pub(crate) fn selection_index(id: &str) -> usize {
    presets()
        .iter()
        .position(|p| p.id == id)
        .map_or(0, |i| i + 1)
}
fn system_fallback(light: bool) -> AppTheme {
    let id = if light { "catppuccin-latte" } else { "boomux" };
    presets().iter().find(|p| p.id == id).unwrap().theme
}

fn resolve_selection(id: &str, system: Option<AppTheme>, light: bool) -> AppTheme {
    presets().iter().find(|preset| preset.id == id).map_or_else(
        || system.unwrap_or_else(|| system_fallback(light)),
        |preset| preset.theme,
    )
}

impl Workspace {
    pub(super) fn selected_theme_label(&self) -> String {
        if self.color_theme == "system" {
            format!(
                "System · {}",
                if self.system_theme.is_some() {
                    "Omarchy"
                } else if self.system_light {
                    "Light"
                } else {
                    "Dark"
                }
            )
        } else {
            presets()
                .iter()
                .find(|p| p.id == self.color_theme)
                .map_or("System", |p| p.name.as_str())
                .into()
        }
    }

    fn candidate_theme(&self, index: usize) -> AppTheme {
        if index == 0 {
            self.system_theme
                .unwrap_or_else(|| system_fallback(self.system_light))
        } else {
            presets()[index - 1].theme
        }
    }

    pub(super) fn apply_selected_theme(&mut self, cx: &mut Context<Self>) {
        let theme = resolve_selection(&self.color_theme, self.system_theme, self.system_light);
        self.theme = theme;
        theme::install(theme);
        self.theme_error = None;
        for session in self
            .terminals
            .values()
            .filter_map(|pane| pane.session.as_ref())
        {
            if let Err(error) = session.set_theme(theme.terminal) {
                self.theme_error.get_or_insert(error);
            }
        }
        cx.notify();
    }

    fn apply_theme_candidate(&mut self, cx: &mut Context<Self>) {
        if let Some(index) = self.theme_candidate.take() {
            self.color_theme = if index == 0 {
                "system".into()
            } else {
                presets()[index - 1].id.clone()
            };
            self.apply_selected_theme(cx);
            self.save_settings();
        }
    }

    pub(super) fn theme_picker_key_down(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(index) = self.theme_candidate else {
            return;
        };
        match event.keystroke.key.as_str() {
            "escape" => self.theme_candidate = None,
            "enter" => self.apply_theme_candidate(cx),
            "up" | "left" => self.theme_candidate = Some(index.saturating_sub(1)),
            "down" | "right" => self.theme_candidate = Some((index + 1).min(presets().len())),
            "home" => self.theme_candidate = Some(0),
            "end" => self.theme_candidate = Some(presets().len()),
            _ => {}
        }
        self.theme_scroll_anchor.scroll_to(window, cx);
        cx.notify();
    }

    pub(super) fn theme_picker_overlay(&self, cx: &mut Context<Self>) -> Option<gpui::AnyElement> {
        let selected = self.theme_candidate?;
        let preview = self.candidate_theme(selected);
        let name = if selected == 0 {
            "System"
        } else {
            presets()[selected - 1].name.as_str()
        };
        let rows = std::iter::once("System")
            .chain(presets().iter().map(|p| p.name.as_str()))
            .enumerate()
            .map(|(index, name)| {
                let palette = self.candidate_theme(index);
                div()
                    .id(SharedString::from(format!("theme-choice-{index}")))
                    .anchor_scroll((selected == index).then(|| self.theme_scroll_anchor.clone()))
                    .role(gpui::Role::Button)
                    .aria_label(format!("Preview {name}"))
                    .flex_none()
                    .p_2()
                    .flex()
                    .items_center()
                    .gap_2()
                    .cursor_pointer()
                    .bg(rgb(if selected == index {
                        0x313244
                    } else {
                        0x181825
                    }))
                    .hover(|row| row.bg(rgb(0x45475a)))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        cx.stop_propagation();
                        this.theme_candidate = Some(index);
                        cx.notify();
                    }))
                    .child(
                        div()
                            .w(px(14.0))
                            .child(if selected == index { "✓" } else { "" }),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .text_sm()
                            .truncate()
                            .child(name.to_owned()),
                    )
                    .children(
                        [palette.surface, palette.text, palette.accent].map(|color| {
                            div()
                                .size(px(12.0))
                                .flex_none()
                                .rounded(px(2.0))
                                .bg(gpui::rgb(color))
                        }),
                    )
            });
        let sample = div()
            .flex_1()
            .min_w_0()
            .p_3()
            .bg(gpui::rgb(preview.canvas))
            .text_color(gpui::rgb(preview.text))
            .child(div().text_lg().child(name.to_owned()))
            .child(
                div()
                    .mt_2()
                    .text_sm()
                    .text_color(gpui::rgb(preview.text_muted))
                    .child("Preview · Apply to update your windows"),
            )
            .child(
                div()
                    .mt_4()
                    .border_1()
                    .border_color(gpui::rgb(preview.border))
                    .flex()
                    .h(px(190.0))
                    .child(
                        div()
                            .w(px(95.0))
                            .p_2()
                            .bg(gpui::rgb(preview.panel))
                            .text_sm()
                            .child("Workspaces")
                            .child(
                                div()
                                    .mt_2()
                                    .p_1()
                                    .bg(gpui::rgb(preview.raised))
                                    .child("Project"),
                            )
                            .child(
                                div()
                                    .mt_2()
                                    .text_color(gpui::rgb(preview.text_muted))
                                    .child("Shell"),
                            ),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .p_2()
                            .bg(gpui::rgb(preview.terminal.background))
                            .text_color(gpui::rgb(preview.terminal.foreground))
                            .text_sm()
                            .child(
                                div()
                                    .text_color(gpui::rgb(preview.accent))
                                    .child("● Terminal"),
                            )
                            .child(div().mt_3().child("$ git status"))
                            .child(
                                div()
                                    .mt_2()
                                    .text_color(gpui::rgb(preview.success))
                                    .child("Working tree clean"),
                            )
                            .child(
                                div()
                                    .mt_2()
                                    .text_color(gpui::rgb(preview.warning))
                                    .child("Waiting for input"),
                            ),
                    ),
            )
            .child(div().mt_3().flex().gap_2().children(
                [("Accent", preview.accent), ("Error", preview.danger)].map(|(label, color)| {
                    div()
                        .p_2()
                        .border_1()
                        .border_color(gpui::rgb(preview.border))
                        .text_color(gpui::rgb(color))
                        .child(label)
                }),
            ));
        Some(
            div()
                .absolute()
                .inset_0()
                .occlude()
                .flex()
                .items_center()
                .justify_center()
                .bg(gpui::rgba(0x00000099))
                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                .child(
                    div()
                        .w(px(680.0))
                        .max_w_full()
                        .h(px(490.0))
                        .max_h_full()
                        .m_3()
                        .p_4()
                        .flex()
                        .flex_col()
                        .gap_3()
                        .bg(rgb(0x1e1e2e))
                        .border_1()
                        .border_color(rgb(0x45475a))
                        .rounded(px(3.0))
                        .child(div().flex_none().text_lg().child("Color theme"))
                        .child(
                            div()
                                .flex_1()
                                .min_h_0()
                                .flex()
                                .gap_3()
                                .child(
                                    div()
                                        .id("theme-palette-list")
                                        .w(px(235.0))
                                        .flex_none()
                                        .overflow_y_scroll()
                                        .track_scroll(&self.theme_scroll_handle)
                                        .children(rows),
                                )
                                .child(sample),
                        )
                        .child(
                            div()
                                .flex_none()
                                .text_xs()
                                .text_color(rgb(0x7f849c))
                                .child("↑ ↓ Preview · Enter Apply · Esc Cancel"),
                        )
                        .child(
                            div()
                                .flex_none()
                                .flex()
                                .justify_end()
                                .gap_2()
                                .child(
                                    Self::settings_control("cancel-theme", "Cancel", false, true)
                                        .button_chrome()
                                        .on_click(cx.listener(|this, _, _, cx| {
                                            cx.stop_propagation();
                                            this.theme_candidate = None;
                                            cx.notify();
                                        })),
                                )
                                .child(
                                    Self::settings_control("apply-theme", "Apply", true, true)
                                        .button_chrome()
                                        .on_click(cx.listener(|this, _, _, cx| {
                                            cx.stop_propagation();
                                            this.apply_theme_candidate(cx);
                                        })),
                                ),
                        ),
                )
                .into_any_element(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn system_tracks_source_while_presets_remain_fixed() {
        let omarchy = presets()[3].theme;
        let changed = presets()[4].theme;
        assert_eq!(resolve_selection("system", Some(omarchy), false), omarchy);
        assert_eq!(resolve_selection("system", Some(changed), true), changed);
        assert_eq!(
            resolve_selection("system", None, true),
            system_fallback(true)
        );
        assert_eq!(
            resolve_selection("system", None, false),
            system_fallback(false)
        );
        let preset = &presets()[1];
        assert_eq!(
            resolve_selection(&preset.id, Some(omarchy), true),
            preset.theme
        );
        assert_eq!(
            resolve_selection(&preset.id, Some(changed), false),
            preset.theme
        );
        assert_eq!(resolve_selection("unknown", Some(omarchy), false), omarchy);
    }

    #[test]
    fn bundled_palette_ids_and_colors_are_valid() {
        assert_eq!(presets().len(), 23);
        let mut ids = HashSet::new();
        for preset in presets() {
            assert!(ids.insert(&preset.id));
            assert_ne!(preset.theme.text, preset.theme.surface);
            assert!(preset.theme.terminal.ansi.iter().all(|c| *c <= 0xffffff));
            assert!(selection_index(&preset.id) > 0);
        }
        assert_eq!(selection_index("missing"), 0);
        assert!(!valid_id("missing"));
        assert!(valid_id("system"));
        assert_ne!(system_fallback(true), system_fallback(false));
    }
}

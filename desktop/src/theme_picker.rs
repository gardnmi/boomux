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

pub(super) struct Reveal {
    generation: u64,
    duration: Duration,
    previous: AppTheme,
    caches: HashMap<usize, Arc<TerminalPaintCache>>,
}

fn carousel_index(index: usize, delta: isize) -> usize {
    (index as isize + delta).rem_euclid((presets().len() + 1) as isize) as usize
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

    fn commit_theme(&mut self, index: usize, cx: &mut Context<Self>) {
        self.color_theme = if index == 0 {
            "system".into()
        } else {
            presets()[index - 1].id.clone()
        };
        self.apply_selected_theme(cx);
        self.save_settings();
    }

    fn apply_theme_candidate(&mut self, cx: &mut Context<Self>) {
        let Some(index) = self.theme_candidate.take() else {
            return;
        };
        self.close_settings(cx);
        let target = self.candidate_theme(index);
        self.theme_reveal = None;
        if let Some(duration) = self
            .motion_speed
            .duration()
            .filter(|_| target != self.theme)
        {
            self.theme_carousel_generation = self.theme_carousel_generation.wrapping_add(1);
            let generation = self.theme_carousel_generation;
            self.theme_reveal = Some(Reveal {
                generation,
                duration,
                previous: self.theme,
                caches: self
                    .terminals
                    .iter()
                    .filter_map(|(id, pane)| {
                        pane.paint_cache
                            .as_ref()
                            .map(|cache| (*id, Arc::clone(cache)))
                    })
                    .collect(),
            });
            cx.spawn(async move |this, cx| {
                cx.background_executor().timer(duration).await;
                this.update(cx, |this, cx| {
                    if this
                        .theme_reveal
                        .as_ref()
                        .is_some_and(|r| r.generation == generation)
                    {
                        this.theme_reveal = None;
                        cx.notify();
                    }
                })
                .ok();
            })
            .detach();
        }
        // Commit once, before the first animation frame. Only the paint mask moves.
        self.commit_theme(index, cx);
    }

    pub(super) fn render_theme_transition(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let current = self.render_frame(window, cx, false);
        let Some(reveal) = self.theme_reveal.take() else {
            return current;
        };
        let new_theme = self.theme;
        self.theme = reveal.previous;
        theme::install(self.theme);
        // Arc swaps preserve the old terminal view without copying transcripts or
        // asking workers to oscillate between palettes on animation frames.
        let mut restored = Vec::with_capacity(reveal.caches.len());
        for (id, cache) in &reveal.caches {
            if let Some(pane) = self.terminals.get_mut(id) {
                restored.push((
                    *id,
                    pane.paint_cache.replace(Arc::clone(cache)),
                    pane.screen.replace(Arc::clone(&cache.screen)),
                ));
            }
        }
        let previous = self.render_frame(window, cx, true);
        for (id, cache, screen) in restored {
            if let Some(pane) = self.terminals.get_mut(&id) {
                pane.paint_cache = cache;
                pane.screen = screen;
            }
        }
        self.theme = new_theme;
        theme::install(new_theme);
        let generation = reveal.generation;
        let duration = reveal.duration;
        self.theme_reveal = Some(reveal);
        div()
            .relative()
            .size_full()
            .overflow_hidden()
            .child(
                div()
                    .id("theme-previous-frame")
                    .absolute()
                    .inset_0()
                    .child(previous),
            )
            .child(
                div().absolute().inset_0().child(
                    RevealClip {
                        child: current,
                        progress: 0.0,
                    }
                    .with_animation(
                        SharedString::from(format!("theme-reveal-{generation}")),
                        Animation::new(duration).with_easing(web_reveal_easing),
                        |mut element, progress| {
                            element.progress = progress;
                            element
                        },
                    ),
                ),
            )
            .into_any_element()
    }

    fn step_theme(&mut self, delta: isize, cx: &mut Context<Self>) {
        if let Some(index) = self.theme_candidate {
            self.theme_candidate = Some(carousel_index(index, delta));
            self.theme_carousel_direction = delta.signum() as f32;
            self.theme_carousel_generation = self.theme_carousel_generation.wrapping_add(1);
            cx.notify();
        }
    }

    pub(super) fn theme_picker_key_down(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.theme_candidate.is_none() {
            return;
        }
        match event.keystroke.key.as_str() {
            "escape" => self.theme_candidate = None,
            "enter" => self.apply_theme_candidate(cx),
            "up" | "left" => self.step_theme(-1, cx),
            "down" | "right" => self.step_theme(1, cx),
            "home" => self.theme_candidate = Some(0),
            "end" => self.theme_candidate = Some(presets().len()),
            _ => {}
        }
        self.theme_scroll_anchor.scroll_to(window, cx);
        cx.notify();
    }

    fn theme_card(&self, index: usize) -> Div {
        let palette = self.candidate_theme(index);
        let name = if index == 0 {
            "System"
        } else {
            &presets()[index - 1].name
        };
        div()
            .size_full()
            .overflow_hidden()
            .flex()
            .flex_col()
            .bg(gpui::rgb(palette.canvas))
            .text_color(gpui::rgb(palette.text))
            .child(
                div()
                    .flex_none()
                    .p_3()
                    .bg(gpui::rgb(palette.panel))
                    .border_b_1()
                    .border_color(gpui::rgb(palette.border))
                    .child(format!("▣  Boomux  ·  {name}")),
            )
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .flex()
                    .child(
                        div()
                            .w(relative(0.28))
                            .flex_none()
                            .p_3()
                            .bg(gpui::rgb(palette.panel))
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(gpui::rgb(palette.text_muted))
                                    .child("WORKSPACES"),
                            )
                            .child(
                                div()
                                    .mt_3()
                                    .p_2()
                                    .bg(gpui::rgb(palette.selection))
                                    .child("Project"),
                            )
                            .child(div().mt_2().text_sm().child("●  Shell")),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .p_3()
                            .bg(gpui::rgb(palette.terminal.background))
                            .text_color(gpui::rgb(palette.terminal.foreground))
                            .text_sm()
                            .child(
                                div()
                                    .text_color(gpui::rgb(palette.accent))
                                    .child("●  Terminal"),
                            )
                            .child(div().mt_4().child("$ git status"))
                            .child(
                                div()
                                    .mt_2()
                                    .text_color(gpui::rgb(palette.success))
                                    .child("Working tree clean"),
                            )
                            .child(div().mt_3().child("$ boomux"))
                            .child(
                                div()
                                    .mt_2()
                                    .text_color(gpui::rgb(palette.warning))
                                    .child("Waiting for input ▌"),
                            ),
                    ),
            )
            .child(
                div().flex_none().p_2().flex().gap_1().children(
                    palette.terminal.ansi[..8]
                        .iter()
                        .map(|color| div().h(px(5.0)).flex_1().bg(gpui::rgb(*color))),
                ),
            )
    }

    fn theme_carousel_preview(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let selected = self.theme_candidate.unwrap_or_default();
        // Paint the center last. Only three synthetic cards exist, regardless of catalog size.
        let cards = [-1_isize, 1, 0].map(|offset| {
            let index = carousel_index(selected, offset);
            let center = offset == 0;
            let palette = self.candidate_theme(index);
            let name = if index == 0 {
                "System"
            } else {
                &presets()[index - 1].name
            };
            div()
                .id(SharedString::from(format!("theme-card-{offset}")))
                .absolute()
                .left(relative(match offset {
                    -1 => 0.0,
                    1 => 0.46,
                    _ => 0.23,
                }))
                .top(relative(if center { 0.02 } else { 0.14 }))
                .w(relative(0.54))
                .h(relative(if center { 0.96 } else { 0.72 }))
                .occlude()
                .overflow_hidden()
                .rounded(px(4.0))
                .border_1()
                .border_color(gpui::rgb(palette.accent))
                .shadow_lg()
                .opacity(if center { 1.0 } else { 0.48 })
                .hover(|card| card.opacity(1.0))
                .cursor_pointer()
                .role(gpui::Role::Button)
                .aria_label(format!(
                    "{} {name}",
                    if center { "Apply" } else { "Preview" }
                ))
                .on_click(cx.listener(move |this, _, _, cx| {
                    cx.stop_propagation();
                    if center {
                        this.apply_theme_candidate(cx);
                    } else {
                        this.step_theme(offset, cx);
                    }
                }))
                .child(self.theme_card(index))
        });
        let deck = div().relative().size_full().children(cards);
        let deck = if let Some(duration) = self.motion_speed.duration() {
            let direction = self.theme_carousel_direction;
            deck.with_animation(
                SharedString::from(format!("theme-carousel-{}", self.theme_carousel_generation)),
                Animation::new(duration).with_easing(ease_out_quint()),
                move |element, progress| {
                    element
                        .left(px(direction * 55.0 * (1.0 - progress)))
                        .opacity(0.45 + 0.55 * progress)
                },
            )
            .into_any_element()
        } else {
            deck.into_any_element()
        };
        let name = if selected == 0 {
            "System"
        } else {
            &presets()[selected - 1].name
        };
        div()
            .flex_1()
            .min_h_0()
            .flex()
            .flex_col()
            .gap_3()
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(
                        Self::settings_control("theme-previous", "‹", false, true)
                            .button_chrome()
                            .on_click(cx.listener(|this, _, _, cx| this.step_theme(-1, cx))),
                    )
                    .child(
                        div()
                            .id("theme-carousel")
                            .relative()
                            .flex_1()
                            .min_w_0()
                            .h_full()
                            .overflow_hidden()
                            .on_scroll_wheel(cx.listener(
                                |this, event: &ScrollWheelEvent, _, cx| {
                                    cx.stop_propagation();
                                    let delta = event.delta.pixel_delta(px(24.0));
                                    let delta = if delta.y.abs() > delta.x.abs() {
                                        f32::from(delta.y)
                                    } else {
                                        f32::from(delta.x)
                                    };
                                    if delta.abs() < 1.0
                                        || this.theme_wheel_at.is_some_and(|time| {
                                            time.elapsed() < Duration::from_millis(140)
                                        })
                                    {
                                        return;
                                    }
                                    this.theme_wheel_at = Some(Instant::now());
                                    this.step_theme(if delta < 0.0 { 1 } else { -1 }, cx);
                                },
                            ))
                            .child(deck),
                    )
                    .child(
                        Self::settings_control("theme-next", "›", false, true)
                            .button_chrome()
                            .on_click(cx.listener(|this, _, _, cx| this.step_theme(1, cx))),
                    ),
            )
            .child(
                div()
                    .flex_none()
                    .text_center()
                    .child(div().text_xs().text_color(rgb(0x7f849c)).child(format!(
                        "{:02} / {}",
                        selected + 1,
                        presets().len() + 1
                    )))
                    .child(div().mt_2().text_2xl().child(name.to_owned()))
                    .child(
                        div()
                            .mt_2()
                            .text_sm()
                            .text_color(rgb(0x7f849c))
                            .child("Preview a palette · Click the center card to apply"),
                    ),
            )
            .into_any_element()
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
                        .w(px(if self.theme_carousel { 1040.0 } else { 680.0 }))
                        .max_w_full()
                        .h(px(if self.theme_carousel { 580.0 } else { 490.0 }))
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
                        .child(
                            div()
                                .flex_none()
                                .flex()
                                .items_center()
                                .justify_between()
                                .child(div().text_lg().child("Color theme"))
                                .child(
                                    Self::settings_control(
                                        "theme-view",
                                        if self.theme_carousel {
                                            "List view"
                                        } else {
                                            "Carousel"
                                        },
                                        false,
                                        true,
                                    )
                                    .button_chrome()
                                    .on_click(cx.listener(
                                        |this, _, _, cx| {
                                            this.theme_carousel = !this.theme_carousel;
                                            cx.notify();
                                        },
                                    )),
                                ),
                        )
                        .child(if self.theme_carousel {
                            self.theme_carousel_preview(cx)
                        } else {
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
                                .child(sample)
                                .into_any_element()
                        })
                        .child(
                            div()
                                .flex_none()
                                .text_xs()
                                .text_color(rgb(0x7f849c))
                                .child("Scroll or ← → Preview · Enter Apply · Esc Cancel"),
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

// CSS cubic-bezier(.35, 0, .2, 1), matching the web UI's timing curve.
fn web_reveal_easing(progress: f32) -> f32 {
    if progress <= 0.0 {
        return 0.0;
    }
    if progress >= 1.0 {
        return 1.0;
    }
    let (mut low, mut high) = (0.0, 1.0);
    for _ in 0..16 {
        let t = (low + high) * 0.5;
        let x = 3.0 * (1.0 - t) * (1.0 - t) * t * 0.35 + 3.0 * (1.0 - t) * t * t * 0.2 + t * t * t;
        if x < progress {
            low = t;
        } else {
            high = t;
        }
    }
    let t = (low + high) * 0.5;
    3.0 * (1.0 - t) * t * t + t * t * t
}

fn reveal_edges(progress: f32) -> (f32, f32) {
    let half = progress.clamp(0.0, 1.0) * 0.5;
    (0.5 - half, 0.5 + half)
}

struct RevealClip {
    child: gpui::AnyElement,
    progress: f32,
}

impl IntoElement for RevealClip {
    type Element = Self;
    fn into_element(self) -> Self {
        self
    }
}

impl gpui::Element for RevealClip {
    type RequestLayoutState = ();
    type PrepaintState = ();
    fn id(&self) -> Option<gpui::ElementId> {
        None
    }
    fn source_location(&self) -> Option<&'static std::panic::Location<'static>> {
        None
    }
    fn request_layout(
        &mut self,
        _: Option<&gpui::GlobalElementId>,
        _: Option<&gpui::InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (gpui::LayoutId, ()) {
        (self.child.request_layout(window, cx), ())
    }
    fn prepaint(
        &mut self,
        _: Option<&gpui::GlobalElementId>,
        _: Option<&gpui::InspectorElementId>,
        _: Bounds<gpui::Pixels>,
        _: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) {
        self.child.prepaint(window, cx);
    }
    fn paint(
        &mut self,
        _: Option<&gpui::GlobalElementId>,
        _: Option<&gpui::InspectorElementId>,
        bounds: Bounds<gpui::Pixels>,
        _: &mut (),
        _: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) {
        let (left, right) = reveal_edges(self.progress);
        let mask = gpui::ContentMask {
            bounds: Bounds::new(
                point(bounds.left() + bounds.size.width * left, bounds.top()),
                size(bounds.size.width * (right - left), bounds.size.height),
            ),
        };
        window.with_content_mask(Some(mask), |window| self.child.paint(window, cx));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn web_timing_curve_is_continuous_monotonic_and_finishes_exactly() {
        assert_eq!(web_reveal_easing(0.0), 0.0);
        assert_eq!(web_reveal_easing(1.0), 1.0);
        let values = (0..=100)
            .map(|i| web_reveal_easing(i as f32 / 100.0))
            .collect::<Vec<_>>();
        assert!(values.windows(2).all(|pair| pair[0] <= pair[1]));
        assert!(web_reveal_easing(0.5) > 0.7);
    }

    #[test]
    fn reveal_mask_expands_continuously_without_moving_content() {
        assert_eq!(reveal_edges(0.0), (0.5, 0.5));
        assert_eq!(reveal_edges(0.5), (0.25, 0.75));
        assert_eq!(reveal_edges(1.0), (0.0, 1.0));
    }

    #[test]
    fn carousel_wraps_in_both_directions_including_system() {
        assert_eq!(carousel_index(0, -1), presets().len());
        assert_eq!(carousel_index(presets().len(), 1), 0);
        assert_eq!(carousel_index(0, 1), 1);
        assert_eq!(carousel_index(1, -1), 0);
    }

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

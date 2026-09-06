use std::time::Duration;

use gpui::{
    Animation, AnimationExt, AnyElement, IntoElement, SharedString, div, prelude::*, px, relative,
};

use crate::theme::AppTheme;

// A slightly overshooting assembly, contained inside the icon's 24px box.
fn tile_position(index: usize, progress: f32, exiting: bool) -> (f32, f32) {
    let spread = if exiting {
        (1.0 - progress).powi(3)
    } else {
        let t = progress - 1.0;
        1.0 + 2.7 * t.powi(3) + 1.7 * t.powi(2)
    };
    let x = if index.is_multiple_of(2) { -4.5 } else { 4.5 };
    let y = if index < 2 { -4.5 } else { 4.5 };
    (8.5 + x * spread, 8.5 + y * spread)
}

fn icon(
    theme: AppTheme,
    duration: Option<Duration>,
    exiting: bool,
    generation: u64,
    activity: (usize, u64),
    scale: f32,
) -> AnyElement {
    let mut icon = div()
        .relative()
        .w(px(24.0 * scale))
        .h(px(24.0 * scale))
        .flex_shrink_0();
    for index in 0..4 {
        let (x, y) = tile_position(index, 1.0, exiting);
        let tile = div()
            .absolute()
            .left(px(x * scale))
            .top(px(y * scale))
            .w(px(7.0 * scale))
            .h(px(7.0 * scale))
            .rounded(px(1.5 * scale))
            .bg(gpui::rgb(theme.accent));
        let tile = if let Some(duration) = duration {
            let animations = if exiting {
                vec![Animation::new(duration)]
            } else {
                vec![
                    Animation::new(duration),
                    Animation::new(Duration::from_secs(3)).repeat(),
                ]
            };
            tile.with_animations(
                SharedString::from(format!(
                    "layout-tile-{scale}-{generation}-{activity:?}-{index}-{exiting}"
                )),
                animations,
                move |tile, stage, progress| {
                    if stage == 0 {
                        let (x, y) = tile_position(index, progress, exiting);
                        tile.left(px(x * scale)).top(px(y * scale))
                    } else {
                        let wave = (progress * std::f32::consts::TAU + index as f32 * 0.7).sin();
                        tile.opacity(0.85 + 0.15 * wave)
                    }
                },
            )
            .into_any_element()
        } else {
            tile.into_any_element()
        };
        icon = icon.child(tile);
    }

    icon.into_any_element()
}

pub fn render_pane_overlay(
    theme: AppTheme,
    duration: Option<Duration>,
    exiting: bool,
    generation: u64,
    activity: (usize, u64),
) -> AnyElement {
    let mut veil = gpui::rgb(theme.canvas);
    veil.alpha = 0.75;
    let overlay = div()
        .id(("layout-pane-overlay", activity.0))
        .absolute()
        .top(px(0.0))
        .left(px(0.0))
        .size_full()
        .flex()
        .items_center()
        .justify_center()
        .bg(veil)
        .child(icon(theme, duration, exiting, generation, activity, 2.5));
    if let Some(duration) = duration {
        overlay
            .with_animation(
                SharedString::from(format!("layout-pane-fade-{generation}-{exiting}")),
                Animation::new(duration),
                move |overlay, progress| overlay.opacity(visibility(progress, exiting)),
            )
            .into_any_element()
    } else {
        overlay.into_any_element()
    }
}

fn visibility(progress: f32, exiting: bool) -> f32 {
    if exiting {
        1.0 - progress
    } else {
        1.0 - (1.0 - progress).powi(3)
    }
}

pub fn render(
    theme: AppTheme,
    duration: Option<Duration>,
    exiting: bool,
    generation: u64,
    activity: (usize, u64),
) -> AnyElement {
    let badge = div()
        .absolute()
        .top(px(8.0))
        .left(relative(0.5))
        .ml(px(-79.0))
        .w(px(158.0))
        .h(px(34.0))
        .flex()
        .items_center()
        .justify_center()
        .gap(px(8.0))
        .rounded(px(6.0))
        .border_1()
        .border_color(gpui::rgb(theme.accent))
        .bg(gpui::rgb(theme.raised))
        .text_color(gpui::rgb(theme.text))
        .text_xs()
        .font_weight(gpui::FontWeight::SEMIBOLD)
        .child(icon(theme, duration, exiting, generation, activity, 1.0))
        .child("LAYOUT")
        .child(
            div()
                .px(px(5.0))
                .py(px(1.0))
                .rounded(px(4.0))
                .bg(gpui::rgb(theme.accent))
                .text_color(gpui::rgb(crate::contrast_foreground(theme.accent)))
                .text_size(px(10.0))
                .child("ESC"),
        );

    if let Some(duration) = duration {
        badge
            .with_animation(
                SharedString::from(format!("layout-badge-{generation}-{exiting}")),
                Animation::new(duration),
                move |badge, progress| {
                    let visible = visibility(progress, exiting);
                    badge.top(px(8.0 - 10.0 * (1.0 - visible))).opacity(visible)
                },
            )
            .into_any_element()
    } else {
        badge.into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn overlay_fades_monotonically_to_the_mode_visibility() {
        assert_eq!(visibility(0.0, false), 0.0);
        assert_eq!(visibility(1.0, false), 1.0);
        assert_eq!(visibility(0.0, true), 1.0);
        assert_eq!(visibility(1.0, true), 0.0);
        for step in 1..=100 {
            let before = (step - 1) as f32 / 100.0;
            let after = step as f32 / 100.0;
            assert!(visibility(before, false) <= visibility(after, false));
            assert!(visibility(before, true) >= visibility(after, true));
        }
    }

    #[test]
    fn tiles_assemble_into_a_grid_and_fold_to_one_square() {
        for (index, expected) in [(4.0, 4.0), (13.0, 4.0), (4.0, 13.0), (13.0, 13.0)]
            .into_iter()
            .enumerate()
        {
            assert_eq!(tile_position(index, 0.0, false), (8.5, 8.5));
            assert_eq!(tile_position(index, 1.0, false), expected);
            assert_eq!(tile_position(index, 0.0, true), expected);
            assert_eq!(tile_position(index, 1.0, true), (8.5, 8.5));
        }
    }

    #[test]
    fn spring_overshoot_stays_inside_icon_bounds() {
        for exiting in [false, true] {
            for index in 0..4 {
                for step in 0..=100 {
                    let (x, y) = tile_position(index, step as f32 / 100.0, exiting);
                    assert!((0.0..=17.0).contains(&x));
                    assert!((0.0..=17.0).contains(&y));
                }
            }
        }
        assert!(tile_position(0, 0.6, false).0 < 4.0);
    }
}

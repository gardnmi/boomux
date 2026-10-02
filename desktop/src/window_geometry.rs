//! Window coordinates are logical GPUI pixels, independent of terminal PTY units.

pub fn valid(rect: [f32; 4]) -> bool {
    rect.iter().all(|n| n.is_finite() && n.abs() <= 1_000_000.0) && rect[2] > 0.0 && rect[3] > 0.0
}

/// Fit a normal window inside usable display bounds, including negative origins.
/// If its display disappeared, retain the size but center on the fallback display.
#[cfg(any(target_os = "macos", test))]
fn fit(usable: [f32; 4], saved: Option<[f32; 4]>, same_display: bool) -> [f32; 4] {
    let rect = saved
        .filter(|rect| valid(*rect))
        .unwrap_or([0.0, 0.0, 1180.0, 760.0]);
    let width = rect[2].min(usable[2]);
    let height = rect[3].min(usable[3]);
    let x = if same_display && saved.is_some() {
        rect[0]
    } else {
        usable[0] + (usable[2] - width) / 2.0
    };
    let y = if same_display && saved.is_some() {
        rect[1]
    } else {
        usable[1] + (usable[3] - height) / 2.0
    };
    [
        x.clamp(usable[0], usable[0] + usable[2] - width),
        y.clamp(usable[1], usable[1] + usable[3] - height),
        width,
        height,
    ]
}

#[cfg(target_os = "macos")]
pub fn restore(
    saved: Option<&crate::layout_state::MacWindow>,
    cx: &gpui::App,
) -> gpui::Bounds<gpui::Pixels> {
    use gpui::{Bounds, point, px, size};
    let preferred = saved.and_then(|saved| {
        cx.displays().into_iter().find(|display| {
            display
                .uuid()
                .ok()
                .is_some_and(|uuid| uuid.to_string() == saved.display)
        })
    });
    let same_display = preferred.is_some();
    let Some(display) = preferred.or_else(|| cx.primary_display()) else {
        return Bounds::centered(None, size(px(1180.0), px(760.0)), cx);
    };
    let bounds = display.visible_bounds();
    let usable = [
        bounds.origin.x.into(),
        bounds.origin.y.into(),
        bounds.size.width.into(),
        bounds.size.height.into(),
    ];
    if !valid(usable) {
        return Bounds::centered(None, size(px(1180.0), px(760.0)), cx);
    }
    let [x, y, width, height] = fit(usable, saved.map(|saved| saved.rect), same_display);
    Bounds::new(point(px(x), px(y)), size(px(width), px(height)))
}

#[cfg(target_os = "macos")]
pub fn capture(window: &gpui::Window, cx: &gpui::App) -> Option<crate::layout_state::MacWindow> {
    // Do not replace the last normal bounds with a fullscreen/maximized frame.
    let gpui::WindowBounds::Windowed(bounds) = window.window_bounds() else {
        return None;
    };
    let rect = [
        bounds.origin.x.into(),
        bounds.origin.y.into(),
        bounds.size.width.into(),
        bounds.size.height.into(),
    ];
    valid(rect).then(|| crate::layout_state::MacWindow {
        rect,
        display: window
            .display(cx)
            .and_then(|display| display.uuid().ok())
            .map(|uuid| uuid.to_string())
            .unwrap_or_default(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn initial_window_fits_small_usable_display() {
        assert_eq!(
            fit([0.0, 25.0, 800.0, 550.0], None, false),
            [0.0, 25.0, 800.0, 550.0]
        );
    }

    #[test]
    fn restored_window_is_clamped_on_negative_origin_display() {
        assert_eq!(
            fit(
                [-1920.0, 25.0, 1920.0, 1000.0],
                Some([-2100.0, -50.0, 800.0, 600.0]),
                true
            ),
            [-1920.0, 25.0, 800.0, 600.0]
        );
        assert_eq!(
            fit(
                [-1920.0, 25.0, 1920.0, 1000.0],
                Some([-100.0, 900.0, 800.0, 600.0]),
                true
            ),
            [-800.0, 425.0, 800.0, 600.0]
        );
    }

    #[test]
    fn missing_display_recenters_and_shrinks_without_losing_visible_controls() {
        assert_eq!(
            fit(
                [0.0, 30.0, 1000.0, 700.0],
                Some([2000.0, 1000.0, 1600.0, 1200.0]),
                false
            ),
            [0.0, 30.0, 1000.0, 700.0]
        );
        assert_eq!(
            fit(
                [0.0, 30.0, 1000.0, 700.0],
                Some([2000.0, 1000.0, 600.0, 400.0]),
                false
            ),
            [200.0, 180.0, 600.0, 400.0]
        );
    }

    #[test]
    fn invalid_or_nonfinite_persisted_geometry_is_rejected() {
        for rect in [
            [0.0, 0.0, 0.0, 10.0],
            [f32::NAN, 0.0, 10.0, 10.0],
            [0.0, f32::INFINITY, 10.0, 10.0],
            [0.0, 0.0, 1_000_001.0, 10.0],
        ] {
            assert!(!valid(rect));
        }
    }
}

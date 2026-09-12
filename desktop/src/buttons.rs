//! Shared compact button corners and transient hover feedback.
use crate::*;

pub struct Motion(pub Option<Duration>);
impl gpui::Global for Motion {}

#[derive(Clone)]
struct Transition {
    from: f32,
    target: f32,
    started: Instant,
}
impl Transition {
    fn value(&self, now: Instant, duration: Option<Duration>) -> f32 {
        let Some(duration) = duration.filter(|d| !d.is_zero()) else {
            return self.target;
        };
        let t = (now.saturating_duration_since(self.started).as_secs_f32()
            / duration.as_secs_f32())
        .clamp(0.0, 1.0);
        self.from + (self.target - self.from) * (1.0 - (1.0 - t).powi(3))
    }
}

#[derive(IntoElement)]
struct HoverFill;
impl gpui::RenderOnce for HoverFill {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let duration = cx
            .try_global::<Motion>()
            .map_or(Some(Duration::from_millis(160)), |motion| {
                motion.0.map(|d| d.min(Duration::from_millis(200)))
            });
        let duration = if cx.reduce_motion() { None } else { duration };
        let state = window.use_keyed_state("button-hover-transition", cx, |_, _| Transition {
            from: 0.0,
            target: 0.0,
            started: Instant::now(),
        });
        let transition = state.read(cx).clone();
        div()
            .id("button-hover-fill")
            .absolute()
            .top_0()
            .left_0()
            .size_full()
            .on_hover(move |hovered, _, cx| {
                state.update(cx, |state, cx| {
                    let target = if *hovered { 1.0 } else { 0.0 };
                    if state.target != target {
                        let now = Instant::now();
                        state.from = state.value(now, duration);
                        state.target = target;
                        state.started = now;
                        cx.notify();
                    }
                });
            })
            .child(
                canvas(
                    |_, _, _| (),
                    move |bounds, (), window, _| {
                        let value = transition.value(Instant::now(), duration);
                        if (value - transition.target).abs() > 0.001 {
                            window.request_animation_frame();
                        }
                        if value <= 0.001 {
                            return;
                        }
                        // A slanted leading edge sweeps down over the existing semantic color.
                        let height = f32::from(bounds.size.height);
                        let width = f32::from(bounds.size.width);
                        let y = value * (height + width * 0.12);
                        let mut path = gpui::PathBuilder::fill();
                        path.move_to(bounds.origin);
                        path.line_to(point(bounds.right(), bounds.top()));
                        path.line_to(point(bounds.right(), bounds.top() + px(y - width * 0.12)));
                        path.line_to(point(bounds.left(), bounds.top() + px(y)));
                        path.close();
                        if let Ok(path) = path.build() {
                            let mut color = rgb(0x89b4fa);
                            color.alpha = 0.20;
                            window.paint_path(path, color);
                        }
                    },
                )
                .size_full(),
            )
    }
}

pub trait ButtonChrome {
    fn button_chrome(self) -> Self;
}
impl ButtonChrome for Stateful<Div> {
    fn button_chrome(mut self) -> Self {
        if matches!(
            gpui::Element::a11y_role(&self),
            Some(gpui::Role::Switch | gpui::Role::SearchInput | gpui::Role::TextInput)
        ) {
            return self;
        }
        if self.style().mouse_cursor == Some(CursorStyle::Arrow) {
            return self.rounded(px(3.0));
        }
        self.rounded(px(3.0)).overflow_hidden().child(HoverFill)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn hover_transition_is_bounded_reversible_and_respects_disabled_motion() {
        let start = Instant::now();
        let duration = Some(Duration::from_millis(160));
        let mut transition = Transition {
            from: 0.0,
            target: 1.0,
            started: start,
        };
        assert_eq!(transition.value(start, duration), 0.0);
        let middle = transition.value(start + Duration::from_millis(80), duration);
        assert!(middle > 0.0 && middle < 1.0);
        transition = Transition {
            from: middle,
            target: 0.0,
            started: start + Duration::from_millis(80),
        };
        assert_eq!(transition.value(transition.started, duration), middle);
        assert_eq!(
            transition.value(start + Duration::from_secs(1), duration),
            0.0
        );
        transition.target = 1.0;
        assert_eq!(transition.value(start, None), 1.0);
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Axis {
    Horizontal,
    Vertical,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Direction {
    Left,
    Right,
    Up,
    Down,
}

impl Direction {
    fn axis(self) -> Axis {
        match self {
            Self::Left | Self::Right => Axis::Horizontal,
            Self::Up | Self::Down => Axis::Vertical,
        }
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

impl Rect {
    fn center(self) -> (f32, f32) {
        (self.x + self.width / 2.0, self.y + self.height / 2.0)
    }

    fn right(self) -> f32 {
        self.x + self.width
    }

    fn bottom(self) -> f32 {
        self.y + self.height
    }
}

fn interval_overlap(a_start: f32, a_end: f32, b_start: f32, b_end: f32) -> f32 {
    a_end.min(b_end) - a_start.max(b_start)
}

#[derive(Clone, Debug, PartialEq)]
pub enum Node {
    Pane(usize),
    Split {
        axis: Axis,
        ratio: f32,
        first: Box<Node>,
        second: Box<Node>,
    },
}

impl Node {
    pub fn pane(id: usize) -> Self {
        Self::Pane(id)
    }

    pub fn split(&mut self, target: usize, new_id: usize, axis: Axis) -> bool {
        self.split_at(target, new_id, axis, false)
    }

    pub fn split_at(&mut self, target: usize, new_id: usize, axis: Axis, before: bool) -> bool {
        match self {
            Self::Pane(id) if *id == target => {
                let target = Self::Pane(target);
                let new = Self::Pane(new_id);
                let (first, second) = if before { (new, target) } else { (target, new) };
                *self = Self::Split {
                    axis,
                    ratio: 0.5,
                    first: Box::new(first),
                    second: Box::new(second),
                };
                true
            }
            Self::Pane(_) => false,
            Self::Split { first, second, .. } => {
                first.split_at(target, new_id, axis, before)
                    || second.split_at(target, new_id, axis, before)
            }
        }
    }

    pub fn contains(&self, target: usize) -> bool {
        match self {
            Self::Pane(id) => *id == target,
            Self::Split { first, second, .. } => first.contains(target) || second.contains(target),
        }
    }

    pub fn remove(self, target: usize) -> Option<Self> {
        match self {
            Self::Pane(id) => (id != target).then_some(Self::Pane(id)),
            Self::Split {
                axis,
                ratio,
                first,
                second,
            } => match (first.remove(target), second.remove(target)) {
                (Some(first), Some(second)) => Some(Self::Split {
                    axis,
                    ratio,
                    first: Box::new(first),
                    second: Box::new(second),
                }),
                (Some(node), None) | (None, Some(node)) => Some(node),
                (None, None) => None,
            },
        }
    }

    pub fn pane_ids(&self) -> Vec<usize> {
        let mut ids = Vec::new();
        self.collect_ids(&mut ids);
        ids
    }

    fn collect_ids(&self, ids: &mut Vec<usize>) {
        match self {
            Self::Pane(id) => ids.push(*id),
            Self::Split { first, second, .. } => {
                first.collect_ids(ids);
                second.collect_ids(ids);
            }
        }
    }

    pub fn rects(&self) -> Vec<(usize, Rect)> {
        let mut rects = Vec::new();
        self.collect_rects(
            Rect {
                x: 0.0,
                y: 0.0,
                width: 1.0,
                height: 1.0,
            },
            &mut rects,
        );
        rects
    }

    fn collect_rects(&self, rect: Rect, out: &mut Vec<(usize, Rect)>) {
        match self {
            Self::Pane(id) => out.push((*id, rect)),
            Self::Split {
                axis,
                ratio,
                first,
                second,
            } => {
                let (a, b) = match axis {
                    Axis::Horizontal => (
                        Rect {
                            width: rect.width * ratio,
                            ..rect
                        },
                        Rect {
                            x: rect.x + rect.width * ratio,
                            width: rect.width * (1.0 - ratio),
                            ..rect
                        },
                    ),
                    Axis::Vertical => (
                        Rect {
                            height: rect.height * ratio,
                            ..rect
                        },
                        Rect {
                            y: rect.y + rect.height * ratio,
                            height: rect.height * (1.0 - ratio),
                            ..rect
                        },
                    ),
                };
                first.collect_rects(a, out);
                second.collect_rects(b, out);
            }
        }
    }

    pub fn neighbor(&self, focused: usize, direction: Direction) -> Option<usize> {
        let rects = self.rects();
        let current = rects.iter().find(|(id, _)| *id == focused)?.1;
        let (cx, cy) = current.center();

        rects
            .into_iter()
            .filter(|(id, _)| *id != focused)
            .filter_map(|(id, rect)| {
                let (x, y) = rect.center();
                let (primary, secondary, overlap) = match direction {
                    Direction::Left if rect.right() <= current.x + f32::EPSILON => (
                        current.x - rect.right(),
                        (cy - y).abs(),
                        interval_overlap(current.y, current.bottom(), rect.y, rect.bottom()),
                    ),
                    Direction::Right if rect.x + f32::EPSILON >= current.right() => (
                        rect.x - current.right(),
                        (cy - y).abs(),
                        interval_overlap(current.y, current.bottom(), rect.y, rect.bottom()),
                    ),
                    Direction::Up if rect.bottom() <= current.y + f32::EPSILON => (
                        current.y - rect.bottom(),
                        (cx - x).abs(),
                        interval_overlap(current.x, current.right(), rect.x, rect.right()),
                    ),
                    Direction::Down if rect.y + f32::EPSILON >= current.bottom() => (
                        rect.y - current.bottom(),
                        (cx - x).abs(),
                        interval_overlap(current.x, current.right(), rect.x, rect.right()),
                    ),
                    _ => return None,
                };
                Some((id, overlap <= f32::EPSILON, primary, secondary))
            })
            .min_by(|a, b| {
                a.1.cmp(&b.1)
                    .then_with(|| a.2.total_cmp(&b.2))
                    .then_with(|| a.3.total_cmp(&b.3))
            })
            .map(|(id, _, _, _)| id)
    }

    pub fn toggle_split(&mut self, focused: usize) -> bool {
        self.update_nearest_split(focused, |axis, _, _, _| {
            *axis = match *axis {
                Axis::Horizontal => Axis::Vertical,
                Axis::Vertical => Axis::Horizontal,
            };
        })
    }

    pub fn equalize_split(&mut self, focused: usize) -> bool {
        self.update_nearest_split(focused, |_, ratio, _, _| *ratio = 0.5)
    }

    pub fn swap_split(&mut self, focused: usize) -> bool {
        self.update_nearest_split(focused, |_, ratio, first, second| {
            std::mem::swap(first, second);
            *ratio = 1.0 - *ratio;
        })
    }

    fn update_nearest_split(
        &mut self,
        focused: usize,
        mut update: impl FnMut(&mut Axis, &mut f32, &mut Box<Node>, &mut Box<Node>),
    ) -> bool {
        self.update_nearest_split_with(focused, &mut update)
    }

    fn update_nearest_split_with(
        &mut self,
        focused: usize,
        update: &mut impl FnMut(&mut Axis, &mut f32, &mut Box<Node>, &mut Box<Node>),
    ) -> bool {
        let Self::Split {
            axis,
            ratio,
            first,
            second,
        } = self
        else {
            return false;
        };
        let focused_first = first.contains(focused);
        let focused_second = second.contains(focused);
        if !focused_first && !focused_second {
            return false;
        }
        let child_updated = if focused_first {
            first.update_nearest_split_with(focused, update)
        } else {
            second.update_nearest_split_with(focused, update)
        };
        if child_updated {
            return true;
        }
        update(axis, ratio, first, second);
        true
    }

    pub fn swap_panes(&mut self, a: usize, b: usize) {
        self.replace_id(a, usize::MAX);
        self.replace_id(b, a);
        self.replace_id(usize::MAX, b);
    }

    fn replace_id(&mut self, old: usize, new: usize) -> bool {
        match self {
            Self::Pane(id) if *id == old => {
                *id = new;
                true
            }
            Self::Pane(_) => false,
            Self::Split { first, second, .. } => {
                first.replace_id(old, new) || second.replace_id(old, new)
            }
        }
    }

    pub fn resize(&mut self, focused: usize, direction: Direction, amount: f32) -> bool {
        let Self::Split {
            axis,
            ratio,
            first,
            second,
        } = self
        else {
            return false;
        };
        let focused_first = first.contains(focused);
        let focused_second = second.contains(focused);
        if !focused_first && !focused_second {
            return false;
        }

        // Prefer the closest matching divider inside the focused subtree.
        let child_resized = if focused_first {
            first.resize(focused, direction, amount)
        } else {
            second.resize(focused, direction, amount)
        };
        if child_resized {
            return true;
        }

        if *axis != direction.axis() {
            return false;
        }

        let delta = match direction {
            Direction::Left | Direction::Up => -amount,
            Direction::Right | Direction::Down => amount,
        };
        *ratio = (*ratio + delta).clamp(0.2, 0.8);
        true
    }

    /// Moves the nearest matching split divider by a delta expressed as a
    /// fraction of the root layout. The divider follows the pointer regardless
    /// of which side contains the focused pane.
    pub fn resize_from_pointer(&mut self, focused: usize, axis: Axis, delta: f32) -> bool {
        self.resize_from_pointer_in_span(focused, axis, delta, 1.0)
    }

    fn resize_from_pointer_in_span(
        &mut self,
        focused: usize,
        target_axis: Axis,
        delta: f32,
        span: f32,
    ) -> bool {
        match self {
            Self::Pane(_) => false,
            Self::Split {
                axis,
                ratio,
                first,
                second,
            } => {
                let focused_first = first.contains(focused);
                let focused_second = second.contains(focused);
                if !focused_first && !focused_second {
                    return false;
                }

                let child_span = if *axis == target_axis {
                    span * if focused_first { *ratio } else { 1.0 - *ratio }
                } else {
                    span
                };
                let child_resized = if focused_first {
                    first.resize_from_pointer_in_span(focused, target_axis, delta, child_span)
                } else {
                    second.resize_from_pointer_in_span(focused, target_axis, delta, child_span)
                };

                if child_resized {
                    true
                } else if *axis == target_axis {
                    *ratio = (*ratio + delta / span.max(f32::EPSILON)).clamp(0.2, 0.8);
                    true
                } else {
                    false
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Node {
        Node::Split {
            axis: Axis::Horizontal,
            ratio: 0.5,
            first: Box::new(Node::pane(1)),
            second: Box::new(Node::Split {
                axis: Axis::Vertical,
                ratio: 0.5,
                first: Box::new(Node::pane(2)),
                second: Box::new(Node::pane(3)),
            }),
        }
    }

    #[test]
    fn splits_and_collapses_tree() {
        let mut root = Node::pane(1);
        assert!(root.split(1, 2, Axis::Horizontal));
        assert!(root.split(2, 3, Axis::Vertical));
        assert_eq!(root.pane_ids(), vec![1, 2, 3]);

        root = root.remove(2).unwrap();
        assert_eq!(root.pane_ids(), vec![1, 3]);
    }

    #[test]
    fn inserts_before_or_after_drop_target() {
        let mut before = Node::pane(1);
        assert!(before.split_at(1, 2, Axis::Horizontal, true));
        assert_eq!(before.pane_ids(), vec![2, 1]);

        let mut after = Node::pane(1);
        assert!(after.split_at(1, 2, Axis::Vertical, false));
        assert_eq!(after.pane_ids(), vec![1, 2]);
    }

    #[test]
    fn finds_spatial_neighbors() {
        let root = sample();
        assert_eq!(root.neighbor(1, Direction::Right), Some(2));
        assert_eq!(root.neighbor(2, Direction::Down), Some(3));
        assert_eq!(root.neighbor(3, Direction::Left), Some(1));
        assert_eq!(root.neighbor(1, Direction::Left), None);
    }

    #[test]
    fn directional_swap_moves_both_panes_without_changing_identity() {
        let mut root = sample();
        let neighbor = root.neighbor(1, Direction::Right).unwrap();

        root.swap_panes(1, neighbor);

        let rects = root.rects();
        let focused = rects.iter().find(|(id, _)| *id == 1).unwrap().1;
        let swapped = rects.iter().find(|(id, _)| *id == neighbor).unwrap().1;
        assert_eq!(focused.x, 0.5);
        assert_eq!(focused.y, 0.0);
        assert_eq!(swapped.x, 0.0);
        assert_eq!(swapped.y, 0.0);
        assert_eq!(root.pane_ids().len(), 3);
    }

    #[test]
    fn nearest_split_can_be_rotated_equalized_and_swapped() {
        let mut root = sample();
        assert!(root.toggle_split(2));
        match &root {
            Node::Split { second, .. } => match second.as_ref() {
                Node::Split { axis, .. } => assert_eq!(*axis, Axis::Horizontal),
                _ => panic!("expected nested split"),
            },
            _ => panic!("expected root split"),
        }

        if let Node::Split { second, .. } = &mut root
            && let Node::Split { ratio, .. } = second.as_mut()
        {
            *ratio = 0.7;
        }
        assert!(root.equalize_split(2));
        assert_eq!(
            root.rects()
                .iter()
                .find(|(id, _)| *id == 2)
                .unwrap()
                .1
                .width,
            0.25
        );

        let before = root.rects();
        assert!(root.swap_split(2));
        let after = root.rects();
        assert_eq!(
            before.iter().find(|(id, _)| *id == 2).unwrap().1.width,
            0.25
        );
        assert_eq!(after.iter().find(|(id, _)| *id == 2).unwrap().1.width, 0.25);
        assert!(after.iter().find(|(id, _)| *id == 2).unwrap().1.x > 0.5);
    }

    #[test]
    fn directional_neighbors_must_be_beyond_the_relevant_edge() {
        let root = Node::Split {
            axis: Axis::Vertical,
            ratio: 0.5,
            first: Box::new(Node::Split {
                axis: Axis::Horizontal,
                ratio: 0.5,
                first: Box::new(Node::pane(1)),
                second: Box::new(Node::pane(2)),
            }),
            second: Box::new(Node::pane(3)),
        };

        assert_eq!(root.neighbor(1, Direction::Right), Some(2));
        assert_eq!(root.neighbor(1, Direction::Down), Some(3));
    }

    #[test]
    fn resize_is_bounded() {
        let mut root = sample();
        for _ in 0..20 {
            root.resize(1, Direction::Right, 0.05);
        }
        match root {
            Node::Split { ratio, .. } => assert_eq!(ratio, 0.8),
            _ => panic!("expected split"),
        }
    }

    #[test]
    fn keyboard_resize_moves_the_nearest_axis_divider_in_both_directions() {
        let pane_rect = |layout: &Node, pane_id| {
            layout
                .rects()
                .into_iter()
                .find(|(id, _)| *id == pane_id)
                .unwrap()
                .1
        };

        let mut left_pane = sample();
        assert!(left_pane.resize(1, Direction::Left, 0.1));
        assert!((pane_rect(&left_pane, 1).width - 0.4).abs() < f32::EPSILON);
        assert!(left_pane.resize(1, Direction::Right, 0.1));
        assert!((pane_rect(&left_pane, 1).width - 0.5).abs() < f32::EPSILON);

        let mut right_pane = sample();
        assert!(right_pane.resize(2, Direction::Left, 0.1));
        assert!((pane_rect(&right_pane, 2).width - 0.6).abs() < f32::EPSILON);
        assert!(right_pane.resize(2, Direction::Right, 0.1));
        assert!((pane_rect(&right_pane, 2).width - 0.5).abs() < f32::EPSILON);

        assert!(right_pane.resize(2, Direction::Up, 0.1));
        assert!((pane_rect(&right_pane, 2).height - 0.4).abs() < f32::EPSILON);
        assert!(right_pane.resize(2, Direction::Down, 0.1));
        assert!((pane_rect(&right_pane, 2).height - 0.5).abs() < f32::EPSILON);
    }

    #[test]
    fn pointer_resize_follows_divider_for_every_tile_side() {
        let mut lower = sample();
        assert!(lower.resize_from_pointer(3, Axis::Vertical, -0.1));
        let lower_height = lower
            .rects()
            .into_iter()
            .find(|(id, _)| *id == 3)
            .unwrap()
            .1
            .height;
        assert!((lower_height - 0.6).abs() < f32::EPSILON);

        let mut right = sample();
        assert!(right.resize_from_pointer(2, Axis::Horizontal, -0.1));
        let right_width = right
            .rects()
            .into_iter()
            .find(|(id, _)| *id == 2)
            .unwrap()
            .1
            .width;
        assert!((right_width - 0.6).abs() < f32::EPSILON);

        let mut upper = sample();
        assert!(upper.resize_from_pointer(2, Axis::Vertical, 0.1));
        let upper_height = upper
            .rects()
            .into_iter()
            .find(|(id, _)| *id == 2)
            .unwrap()
            .1
            .height;
        assert!((upper_height - 0.6).abs() < f32::EPSILON);
    }
}

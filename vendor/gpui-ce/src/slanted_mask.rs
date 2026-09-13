//! Bounded paint-only parallelogram clipping. No GPU ABI changes.

pub(crate) fn clip_bands(rect: [f32; 4], mask: [f32; 5]) -> impl Iterator<Item = [f32; 4]> {
    let [x0, y0, x1, y1] = rect;
    let [origin, height, left, right, slant] = mask;
    let edges = move |y: f32| {
        let shift = slant * ((y - origin) / height - 0.5);
        (left + shift, right + shift)
    };
    let (lt, rt) = edges(y0);
    let (lb, rb) = edges(y1);
    let outside = height <= 0.0
        || right <= left
        || x1 <= x0
        || y1 <= y0
        || x1 <= lt.min(lb)
        || x0 >= rt.max(rb);
    let inside = x0 >= lt.max(lb) && x1 <= rt.min(rb);
    let count = slant.abs().ceil().clamp(1.0, 512.0);
    let band_height = height / count;
    let (first, last) = if outside {
        (0, 0)
    } else if inside {
        (0, 1)
    } else {
        (
            ((y0 - origin) / band_height).floor().max(0.0) as usize,
            ((y1 - origin) / band_height).ceil().min(count) as usize,
        )
    };
    (first..last).filter_map(move |band| {
        if inside {
            return Some(rect);
        }
        let top = (origin + band as f32 * band_height).max(y0);
        let bottom = (origin + (band + 1) as f32 * band_height).min(y1);
        let (left, right) = edges((top + bottom) * 0.5);
        let left = left.max(x0);
        let right = right.min(x1);
        (right > left && bottom > top).then_some([left, top, right, bottom])
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    const MASK: [f32; 5] = [0.0, 100.0, 30.0, 70.0, 20.0];
    #[test]
    fn interior_is_one_primitive_and_exterior_is_empty() {
        let rect = [45.0, 30.0, 50.0, 35.0];
        assert_eq!(clip_bands(rect, MASK).collect::<Vec<_>>(), vec![rect]);
        assert_eq!(clip_bands([0.0, 30.0, 5.0, 35.0], MASK).count(), 0);
    }
    #[test]
    fn bands_cover_once_and_slant_right_from_top_to_bottom() {
        let bands = clip_bands([0.0, 0.0, 100.0, 100.0], MASK).collect::<Vec<_>>();
        assert_eq!(bands.len(), 20);
        let mut end = 0.0;
        for [left, top, right, bottom] in bands {
            assert_eq!(top, end);
            end = bottom;
            assert!((left - (20.0 + (top + bottom) * 0.1)).abs() < 0.001);
            assert!((right - left - 40.0).abs() < 0.001);
        }
        assert_eq!(end, 100.0);
    }
    #[test]
    fn mask_expansion_is_bounded_and_closed_mask_is_empty() {
        assert!(
            clip_bands(
                [0.0, 0.0, 100.0, 100.0],
                [0.0, 100.0, 30.0, 70.0, 100_000.0]
            )
            .count()
                <= 512
        );
        assert_eq!(
            clip_bands([0.0, 0.0, 100.0, 100.0], [0.0, 100.0, 50.0, 50.0, 20.0]).count(),
            0
        );
    }
    #[test]
    fn fully_open_reveal_covers_window_with_one_primitive() {
        let rect = [0.0, 0.0, 100.0, 100.0];
        assert_eq!(
            clip_bands(rect, [0.0, 100.0, -8.001, 108.001, 16.0]).collect::<Vec<_>>(),
            vec![rect]
        );
    }
}

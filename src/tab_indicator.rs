//! Pure geometry for the tabbed-display click indicator (one segment per tab).
//! No `objc2`/`AppKit` here so it stays unit-testable; the drawing and click
//! glue lives in `src/overlay.rs`.

use bevy::math::IRect;
use serde::Deserialize;

use crate::manager::Origin;

/// Which edge of the column the indicator sits against. `Left`/`Right` give
/// a vertical strip, `Top`/`Bottom` a horizontal one.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, Deserialize)]
#[cfg_attr(test, derive(serde::Serialize))]
#[serde(rename_all = "lowercase")]
pub enum TabIndicatorPosition {
    #[default]
    Left,
    Right,
    Top,
    Bottom,
}

impl TabIndicatorPosition {
    fn is_vertical_strip(self) -> bool {
        matches!(self, Self::Left | Self::Right)
    }
}

#[derive(Clone, Copy, Debug)]
pub struct TabIndicatorLayout {
    pub position: TabIndicatorPosition,
    /// Thickness of the indicator strip, in pixels.
    pub width: i32,
    /// Gap between the indicator and the column it belongs to.
    pub gap: i32,
    /// Gap between adjacent tab segments within the indicator.
    pub segment_gap: i32,
}

impl Default for TabIndicatorLayout {
    fn default() -> Self {
        Self {
            position: TabIndicatorPosition::default(),
            width: 6,
            gap: 4,
            segment_gap: 2,
        }
    }
}

/// The indicator's rect, floating just outside `column_rect` on the
/// configured edge. The column's own size is untouched.
pub fn indicator_rect(column_rect: IRect, layout: &TabIndicatorLayout) -> IRect {
    use TabIndicatorPosition::{Bottom, Left, Right, Top};
    match layout.position {
        Left => IRect::from_corners(
            Origin::new(
                column_rect.min.x - layout.gap - layout.width,
                column_rect.min.y,
            ),
            Origin::new(column_rect.min.x - layout.gap, column_rect.max.y),
        ),
        Right => IRect::from_corners(
            Origin::new(column_rect.max.x + layout.gap, column_rect.min.y),
            Origin::new(
                column_rect.max.x + layout.gap + layout.width,
                column_rect.max.y,
            ),
        ),
        Top => IRect::from_corners(
            Origin::new(
                column_rect.min.x,
                column_rect.min.y - layout.gap - layout.width,
            ),
            Origin::new(column_rect.max.x, column_rect.min.y - layout.gap),
        ),
        Bottom => IRect::from_corners(
            Origin::new(column_rect.min.x, column_rect.max.y + layout.gap),
            Origin::new(
                column_rect.max.x,
                column_rect.max.y + layout.gap + layout.width,
            ),
        ),
    }
}

/// Per-tab segment rects within the indicator, in tab order. The last
/// segment absorbs integer-division leftovers so the segments tile exactly.
pub fn tab_segments(indicator: IRect, tab_count: usize, layout: &TabIndicatorLayout) -> Vec<IRect> {
    if tab_count == 0 {
        return Vec::new();
    }
    let tab_count_i32 = i32::try_from(tab_count).unwrap_or(i32::MAX);
    let total_gap = layout.segment_gap * (tab_count_i32 - 1).max(0);

    if layout.position.is_vertical_strip() {
        let available = (indicator.height() - total_gap).max(0);
        let seg_len = available / tab_count_i32;
        let mut segments = Vec::with_capacity(tab_count);
        let mut y = indicator.min.y;
        for i in 0..tab_count {
            let h = if i + 1 == tab_count {
                indicator.max.y - y
            } else {
                seg_len
            };
            segments.push(IRect::from_corners(
                Origin::new(indicator.min.x, y),
                Origin::new(indicator.max.x, y + h),
            ));
            y += h + layout.segment_gap;
        }
        segments
    } else {
        let available = (indicator.width() - total_gap).max(0);
        let seg_len = available / tab_count_i32;
        let mut segments = Vec::with_capacity(tab_count);
        let mut x = indicator.min.x;
        for i in 0..tab_count {
            let w = if i + 1 == tab_count {
                indicator.max.x - x
            } else {
                seg_len
            };
            segments.push(IRect::from_corners(
                Origin::new(x, indicator.min.y),
                Origin::new(x + w, indicator.max.y),
            ));
            x += w + layout.segment_gap;
        }
        segments
    }
}

/// Which segment (if any) a click at `point` lands on. `point` and
/// `segments` must be in the same coordinate space.
pub fn hit_test(point: Origin, segments: &[IRect]) -> Option<usize> {
    segments.iter().position(|segment| segment.contains(point))
}

/// Whether `column_rect` is visibly inside `viewport`, so the indicator is
/// hidden for columns scrolled off-screen. Off-screen columns keep a
/// `sliver_width` overlap (see `position_layout_windows`), so this requires
/// more than that.
pub fn column_is_visible(column_rect: IRect, viewport: IRect, sliver_width: i32) -> bool {
    let overlap = column_rect.intersect(viewport);
    overlap.width() > sliver_width && overlap.height() > 0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn layout(position: TabIndicatorPosition) -> TabIndicatorLayout {
        TabIndicatorLayout {
            position,
            width: 6,
            gap: 4,
            segment_gap: 2,
        }
    }

    #[test]
    fn indicator_rect_floats_on_the_configured_edge() {
        let column = IRect::new(100, 20, 500, 768);

        let left = indicator_rect(column, &layout(TabIndicatorPosition::Left));
        assert_eq!(
            left.max.x,
            column.min.x - 4,
            "left indicator ends at the gap before the column"
        );
        assert_eq!(left.width(), 6);
        assert_eq!((left.min.y, left.max.y), (column.min.y, column.max.y));

        let right = indicator_rect(column, &layout(TabIndicatorPosition::Right));
        assert_eq!(right.min.x, column.max.x + 4);
        assert_eq!(right.width(), 6);

        let top = indicator_rect(column, &layout(TabIndicatorPosition::Top));
        assert_eq!(top.max.y, column.min.y - 4);
        assert_eq!(top.height(), 6);
        assert_eq!((top.min.x, top.max.x), (column.min.x, column.max.x));

        let bottom = indicator_rect(column, &layout(TabIndicatorPosition::Bottom));
        assert_eq!(bottom.min.y, column.max.y + 4);
        assert_eq!(bottom.height(), 6);
    }

    #[test]
    fn tab_segments_tile_a_vertical_strip_top_to_bottom_with_no_gap_at_the_end() {
        let indicator = IRect::new(0, 0, 6, 100);
        let segments = tab_segments(indicator, 3, &layout(TabIndicatorPosition::Left));

        assert_eq!(segments.len(), 3);
        // Contiguous: each segment starts after the previous one's gap.
        assert_eq!(segments[0].min.y, 0);
        assert_eq!(segments[1].min.y, segments[0].max.y + 2);
        assert_eq!(segments[2].min.y, segments[1].max.y + 2);
        // The last segment reaches the indicator's bottom edge exactly.
        assert_eq!(segments[2].max.y, indicator.max.y);
        // Every segment spans the indicator's full width.
        for segment in &segments {
            assert_eq!(
                (segment.min.x, segment.max.x),
                (indicator.min.x, indicator.max.x)
            );
        }
    }

    #[test]
    fn tab_segments_tile_a_horizontal_strip_left_to_right() {
        let indicator = IRect::new(0, 0, 100, 6);
        let segments = tab_segments(indicator, 4, &layout(TabIndicatorPosition::Bottom));

        assert_eq!(segments.len(), 4);
        assert_eq!(segments[0].min.x, 0);
        assert_eq!(segments[3].max.x, indicator.max.x);
        for i in 1..4 {
            assert_eq!(segments[i].min.x, segments[i - 1].max.x + 2);
        }
    }

    #[test]
    fn tab_segments_handles_a_single_tab() {
        let indicator = IRect::new(0, 0, 6, 100);
        let segments = tab_segments(indicator, 1, &layout(TabIndicatorPosition::Left));
        assert_eq!(segments.len(), 1);
        assert_eq!(segments[0], indicator);
    }

    #[test]
    fn tab_segments_handles_zero_tabs() {
        let indicator = IRect::new(0, 0, 6, 100);
        assert_eq!(
            tab_segments(indicator, 0, &layout(TabIndicatorPosition::Left)),
            Vec::<IRect>::new()
        );
    }

    #[test]
    fn hit_test_finds_the_containing_segment() {
        let indicator = IRect::new(0, 0, 6, 90);
        let segments = tab_segments(indicator, 3, &layout(TabIndicatorPosition::Left));

        assert_eq!(hit_test(Origin::new(3, 10), &segments), Some(0));
        assert_eq!(hit_test(Origin::new(3, 45), &segments), Some(1));
        assert_eq!(hit_test(Origin::new(3, 85), &segments), Some(2));
    }

    #[test]
    fn hit_test_misses_outside_every_segment() {
        let indicator = IRect::new(0, 0, 6, 90);
        let segments = tab_segments(indicator, 3, &layout(TabIndicatorPosition::Left));

        // Well outside the indicator entirely.
        assert_eq!(hit_test(Origin::new(3, 500), &segments), None);
        assert_eq!(hit_test(Origin::new(-50, 10), &segments), None);
    }

    #[test]
    fn hit_test_boundary_lands_on_exactly_one_segment() {
        // `IRect::contains` is max-exclusive: a shared edge hits exactly one
        // segment.
        let indicator = IRect::new(0, 0, 6, 90);
        let segments = tab_segments(indicator, 3, &layout(TabIndicatorPosition::Left));
        let boundary_y = segments[0].max.y;

        let hits: Vec<_> = segments
            .iter()
            .enumerate()
            .filter(|(_, s)| s.contains(Origin::new(3, boundary_y)))
            .map(|(i, _)| i)
            .collect();
        assert!(
            hits.len() <= 1,
            "a boundary point must land on at most one segment"
        );
    }

    #[test]
    fn column_is_visible_true_when_the_column_meaningfully_overlaps_the_viewport() {
        let viewport = IRect::new(0, 0, 1920, 1080);
        let column = IRect::new(100, 20, 500, 768);
        assert!(column_is_visible(column, viewport, 5));
    }

    #[test]
    fn column_is_visible_false_for_a_column_parked_at_the_deliberate_off_screen_sliver() {
        // The off-screen sliver must not count as visible, on either side.
        let viewport = IRect::new(0, 0, 1920, 1080);
        let sliver_width = 5;

        // Scrolled off to the right: only the sliver's width overlaps.
        let off_right_sliver = IRect::new(1915, 20, 2800, 768);
        assert!(!column_is_visible(off_right_sliver, viewport, sliver_width));

        // Scrolled off to the left: only the sliver's width overlaps.
        let off_left_sliver = IRect::new(-800, 20, viewport.min.x + sliver_width, 768);
        assert!(!column_is_visible(off_left_sliver, viewport, sliver_width));
    }

    #[test]
    fn column_is_visible_false_when_fully_scrolled_off_the_viewport() {
        let viewport = IRect::new(0, 0, 1920, 1080);

        // Scrolled off to the right, no overlap at all.
        let off_right = IRect::new(1920, 20, 2300, 768);
        assert!(!column_is_visible(off_right, viewport, 5));

        // Scrolled off to the left.
        let off_left = IRect::new(-500, 20, -100, 768);
        assert!(!column_is_visible(off_left, viewport, 5));

        // Parked below the viewport's bottom edge.
        let off_below = IRect::new(100, 1080, 500, 1400);
        assert!(!column_is_visible(off_below, viewport, 5));
    }
}

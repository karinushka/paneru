//! Pure one-hop paging math for horizontal strip gestures.

use bevy::math::IRect;

use crate::ecs::{PagingGesture, Scrolling};

use super::STICKY_EDGE_THRESHOLD_POINTS;

pub(super) const FLING_VELOCITY_THRESHOLD: f64 = 0.5;
const ADVANCE_RATIO: f64 = 0.25;

pub(super) fn capture_gesture(
    current_position: f64,
    viewport: &IRect,
    columns: impl IntoIterator<Item = (i32, i32)>,
) -> Option<PagingGesture> {
    let stops = snap_stops(viewport, columns);
    let start_index = stops
        .iter()
        .enumerate()
        .min_by(|(_, left), (_, right)| {
            (current_position - **left)
                .abs()
                .total_cmp(&(current_position - **right).abs())
        })?
        .0;

    Some(PagingGesture {
        start_stop: stops[start_index],
        previous_stop: start_index.checked_sub(1).map(|index| stops[index]),
        next_stop: stops.get(start_index + 1).copied(),
        release_velocity: 0.0,
    })
}

pub(super) fn constrain_motion(scrolling: &mut Scrolling, direction_modifier: f64) {
    let Some(paging) = scrolling.paging_gesture else {
        return;
    };
    let lower = paging.next_stop.unwrap_or(paging.start_stop);
    let upper = paging.previous_stop.unwrap_or(paging.start_stop);
    let previous_position = scrolling.position;
    scrolling.position = scrolling.position.clamp(lower, upper);
    if let Some(target) = scrolling.target_position.as_mut() {
        *target = target.clamp(lower, upper);
    }

    let coordinate_velocity = scrolling.velocity * direction_modifier;
    if (previous_position < lower && coordinate_velocity < 0.0)
        || (previous_position > upper && coordinate_velocity > 0.0)
    {
        scrolling.velocity = 0.0;
    }
}

/// Return reading-order paging stops. Numeric offsets decrease as the strip
/// advances to the right, so the result is sorted from greatest to smallest.
fn snap_stops(viewport: &IRect, columns: impl IntoIterator<Item = (i32, i32)>) -> Vec<f64> {
    let columns = columns
        .into_iter()
        .filter(|(_, width)| *width > 0)
        .collect::<Vec<_>>();
    let Some(content_min) = columns.iter().map(|(position, _)| *position).min() else {
        return Vec::new();
    };
    let content_max = columns
        .iter()
        .map(|(position, width)| position.saturating_add(*width))
        .max()
        .unwrap_or(content_min);
    let first_bound = viewport.min.x - content_min;
    let last_bound = viewport.max.x - content_max;
    let lower_bound = f64::from(first_bound.min(last_bound));
    let upper_bound = f64::from(first_bound.max(last_bound));

    let mut stops = columns
        .into_iter()
        .flat_map(|(position, width)| {
            let left_aligned = f64::from(viewport.min.x - position).clamp(lower_bound, upper_bound);
            let right_aligned =
                f64::from(viewport.max.x - position - width).clamp(lower_bound, upper_bound);
            if width > viewport.width() {
                [Some(left_aligned), Some(right_aligned)]
            } else {
                [Some(left_aligned), None]
            }
        })
        .flatten()
        .collect::<Vec<_>>();
    stops.sort_by(|left, right| right.total_cmp(left));
    stops.dedup();
    stops
}

pub(super) fn snap_target(
    current_position: f64,
    viewport_width: f64,
    paging: PagingGesture,
) -> f64 {
    let edge_target = [
        Some(paging.start_stop),
        paging.previous_stop,
        paging.next_stop,
    ]
    .into_iter()
    .flatten()
    .filter(|stop| (current_position - *stop).abs() <= f64::from(STICKY_EDGE_THRESHOLD_POINTS))
    .min_by(|left, right| {
        (current_position - *left)
            .abs()
            .total_cmp(&(current_position - *right).abs())
    });
    if let Some(target) = edge_target {
        return target;
    }

    let displacement = current_position - paging.start_stop;
    let displacement_neighbor = if displacement > 0.0 {
        paging.previous_stop
    } else if displacement < 0.0 {
        paging.next_stop
    } else {
        None
    };
    if let Some(neighbor) = displacement_neighbor {
        let threshold = ((paging.start_stop - neighbor).abs() * ADVANCE_RATIO)
            .min(viewport_width * ADVANCE_RATIO);
        if displacement.abs() >= threshold {
            return neighbor;
        }
    }

    let fling_neighbor = if paging.release_velocity > 0.0 {
        paging.previous_stop
    } else if paging.release_velocity < 0.0 {
        paging.next_stop
    } else {
        None
    };
    if paging.release_velocity.abs() >= FLING_VELOCITY_THRESHOLD
        && let Some(neighbor) = fling_neighbor
    {
        return neighbor;
    }

    paging.start_stop
}

pub(super) fn ready_to_snap(scrolling: &Scrolling) -> bool {
    !scrolling.gesture_active
        && !scrolling.is_user_swiping
        && scrolling.velocity.abs() <= FLING_VELOCITY_THRESHOLD
        && scrolling.target_position.is_none()
}

#[cfg(test)]
mod tests {
    use bevy::math::IRect;

    use super::{capture_gesture, constrain_motion, ready_to_snap, snap_stops, snap_target};
    use crate::ecs::{PagingGesture, Scrolling};

    #[test]
    fn normal_has_one_stop_and_oversized_has_two() {
        let viewport = IRect::new(0, 0, 1000, 800);
        assert_eq!(
            snap_stops(&viewport, [(0, 600), (600, 1500), (2100, 600)]),
            vec![0.0, -600.0, -1100.0, -1700.0]
        );
    }

    #[test]
    fn arbitrarily_wide_column_still_has_exactly_two_stops() {
        let viewport = IRect::new(0, 0, 1000, 800);
        let stops = snap_stops(&viewport, [(0, 600), (600, 3500), (4100, 600)]);
        assert_eq!(stops, vec![0.0, -600.0, -3100.0, -3700.0]);
        assert_eq!(
            stops
                .iter()
                .filter(|stop| **stop <= -600.0 && **stop >= -3100.0)
                .count(),
            2
        );
    }

    #[test]
    fn neighborhood_is_ordered_and_reverse_symmetric() {
        let viewport = IRect::new(0, 0, 1000, 800);
        let columns = [(0, 600), (600, 1500), (2100, 600)];
        let left = capture_gesture(-600.0, &viewport, columns).unwrap();
        assert_eq!(
            (left.previous_stop, left.next_stop),
            (Some(0.0), Some(-1100.0))
        );
        let right = capture_gesture(-1100.0, &viewport, columns).unwrap();
        assert_eq!(
            (right.previous_stop, right.next_stop),
            (Some(-600.0), Some(-1700.0))
        );
    }

    #[test]
    fn motion_is_capped_at_adjacent_stops() {
        let mut scrolling = Scrolling {
            position: -5000.0,
            target_position: Some(-4000.0),
            velocity: -2.0,
            paging_gesture: Some(gesture()),
            ..Default::default()
        };
        constrain_motion(&mut scrolling, 1.0);
        assert_eq!(
            (
                scrolling.position,
                scrolling.target_position,
                scrolling.velocity
            ),
            (-1100.0, Some(-1100.0), 0.0)
        );

        scrolling.position = 5000.0;
        scrolling.target_position = Some(4000.0);
        scrolling.velocity = 2.0;
        constrain_motion(&mut scrolling, 1.0);
        assert_eq!(
            (
                scrolling.position,
                scrolling.target_position,
                scrolling.velocity
            ),
            (0.0, Some(0.0), 0.0)
        );
    }

    #[test]
    fn release_returns_or_advances_exactly_one_stop() {
        let paging = gesture();
        assert_eq!(snap_target(-700.0, 1000.0, paging), -600.0);
        assert_eq!(snap_target(-730.0, 1000.0, paging), -1100.0);
        assert_eq!(snap_target(-1080.0, 1000.0, paging), -1100.0);
        assert_eq!(
            snap_target(
                -650.0,
                1000.0,
                PagingGesture {
                    release_velocity: -0.5,
                    ..paging
                }
            ),
            -1100.0
        );
        assert_eq!(
            snap_target(
                -970.0,
                1000.0,
                PagingGesture {
                    start_stop: -1100.0,
                    previous_stop: Some(-600.0),
                    next_stop: Some(-1700.0),
                    release_velocity: 0.0
                }
            ),
            -600.0
        );
    }

    #[test]
    fn snap_waits_for_end_and_momentum_settlement() {
        let mut scrolling = Scrolling {
            gesture_active: true,
            is_user_swiping: true,
            ..Default::default()
        };
        assert!(!ready_to_snap(&scrolling));
        scrolling.gesture_active = false;
        assert!(!ready_to_snap(&scrolling));
        scrolling.is_user_swiping = false;
        scrolling.target_position = Some(-600.0);
        assert!(!ready_to_snap(&scrolling));
        scrolling.target_position = None;
        scrolling.velocity = 0.51;
        assert!(!ready_to_snap(&scrolling));
        scrolling.velocity = 0.5;
        assert!(ready_to_snap(&scrolling));
    }

    fn gesture() -> PagingGesture {
        PagingGesture {
            start_stop: -600.0,
            previous_stop: Some(0.0),
            next_stop: Some(-1100.0),
            release_velocity: 0.0,
        }
    }
}

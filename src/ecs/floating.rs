use bevy::math::IRect;

use crate::manager::{Origin, Size};

pub(super) fn clamp_origin_to_bounds(origin: IRect, size: Size, bounds: IRect) -> IRect {
    let max = (bounds.max - size).max(bounds.min);
    let min = origin.min.clamp(bounds.min, max);
    IRect::from_corners(min, min + size)
}

pub(super) fn offset_frame_within_bounds(frame: IRect, bounds: IRect, offset: i32) -> IRect {
    let candidates = [
        (offset, offset),
        (offset, -offset),
        (-offset, offset),
        (-offset, -offset),
        (offset, 0),
        (-offset, 0),
        (0, offset),
        (0, -offset),
    ];

    for (dx, dy) in candidates {
        let moved = IRect::from_corners(
            Origin::new(frame.min.x + dx, frame.min.y + dy),
            Origin::new(frame.max.x + dx, frame.max.y + dy),
        );
        if moved.min.x >= bounds.min.x
            && moved.max.x <= bounds.max.x
            && moved.min.y >= bounds.min.y
            && moved.max.y <= bounds.max.y
        {
            return moved;
        }
    }

    frame
}

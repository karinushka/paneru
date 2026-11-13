use std::sync::{Arc, Mutex};

use bevy::prelude::*;

use crate::commands::Command;
use crate::config::{Config, MainOptions};
use crate::events::Event;

use super::*;

const RIGHT_DISPLAY_ID: u32 = 3;
const RIGHT_WORKSPACE_ID: u64 = 30;

fn left_monitor() -> IRect {
    IRect::new(-1408, -EXT_DISPLAY_HEIGHT, 512, 0)
}

fn right_monitor() -> IRect {
    IRect::new(-1408, -2 * EXT_DISPLAY_HEIGHT, 512, -EXT_DISPLAY_HEIGHT)
}

fn laptop() -> IRect {
    IRect::new(0, 0, TEST_DISPLAY_WIDTH, TEST_DISPLAY_HEIGHT)
}

fn desk(lowest_below: bool) -> TestHarness {
    let config: Config = (
        MainOptions {
            horizontal_mouse_warp: Some(1),
            horizontal_mouse_warp_lowest_below: Some(lowest_below),
            ..Default::default()
        },
        vec![],
    )
        .into();
    TestHarness::new()
        .with_config(config)
        .with_windows(1)
        .with_display(EXT_DISPLAY_ID, left_monitor(), vec![EXT_WORKSPACE_ID])
        .with_display(RIGHT_DISPLAY_ID, right_monitor(), vec![RIGHT_WORKSPACE_ID])
}

fn moves(points: &[(i32, i32)]) -> Vec<Event> {
    points
        .iter()
        .map(|&(x, y)| Event::MouseMoved {
            point: objc2_core_foundation::CGPoint {
                x: f64::from(x),
                y: f64::from(y),
            },
            modifiers: crate::platform::Modifiers::empty(),
        })
        .collect()
}

fn pointer_moves(harness: TestHarness, points: &[(i32, i32)]) -> (IVec2, IVec2) {
    let events = moves(points);
    let seen = Arc::new(Mutex::new((IVec2::ZERO, IVec2::ZERO)));
    let before = Arc::clone(&seen);
    let after = Arc::clone(&seen);
    harness
        .on_iteration(1, move |world, state| {
            before.lock().expect("cursor record").0 = state.cursor_position();
            world.write_message_batch(events.iter().cloned());
        })
        .on_iteration(2, move |_world, state| {
            after.lock().expect("cursor record").1 = state.cursor_position();
        })
        .run(vec![
            Event::MenuOpened { window_id: 0 },
            Event::Command {
                command: Command::PrintState,
            },
            Event::Command {
                command: Command::PrintState,
            },
        ]);
    *seen.lock().expect("cursor record")
}

#[test]
fn moving_down_from_the_upper_monitor_lands_on_the_laptop_below_it() {
    let (_, cursor) = pointer_moves(desk(true), &[(-1308, -1210), (-1308, -1190)]);
    assert_eq!(
        cursor,
        IVec2::new(612, 6),
        "100 px into the right monitor is 100 px right of the laptop's centre"
    );
}

#[test]
fn moving_down_from_the_upper_monitor_past_the_laptop_stays_on_it() {
    let (_, cursor) = pointer_moves(desk(true), &[(100, -1210), (100, -1190)]);
    assert_eq!(
        cursor,
        IVec2::new(100, -EXT_DISPLAY_HEIGHT - 1),
        "no display is below that part of the right monitor"
    );
}

#[test]
fn moving_up_from_the_lower_monitor_stays_on_it() {
    let (_, cursor) = pointer_moves(desk(true), &[(-1308, -1190), (-1308, -1210)]);
    assert_eq!(
        cursor,
        IVec2::new(-1308, -EXT_DISPLAY_HEIGHT),
        "no display is above the left monitor"
    );
}

#[test]
fn a_fast_flick_down_from_the_upper_monitor_still_reaches_the_laptop() {
    let (_, cursor) = pointer_moves(desk(true), &[(-1308, -1500), (-1308, -900)]);
    assert_eq!(cursor, IVec2::new(612, 6));
}

#[test]
fn moving_up_from_the_laptop_where_macos_has_no_display_above_reaches_the_upper_monitor() {
    let (_, cursor) = pointer_moves(desk(true), &[(800, 40), (800, 1)]);
    assert_eq!(cursor, IVec2::new(800 - 1920, -EXT_DISPLAY_HEIGHT - 7));
}

#[test]
fn the_laptop_leaves_the_sideways_warp_but_the_monitors_keep_it() {
    let (before, cursor) = pointer_moves(desk(true), &[(-1380, -600), (-1407, -600)]);
    assert_eq!(
        cursor, before,
        "the left edge of the left monitor has no display to its left"
    );

    let (_, cursor) = pointer_moves(desk(true), &[(480, -600), (511, -600)]);
    assert!(
        right_monitor().contains(cursor),
        "the right edge of the left monitor still reaches the right monitor: {cursor:?}"
    );

    let (before, cursor) = pointer_moves(desk(true), &[(990, 400), (1023, 400)]);
    assert_eq!(cursor, before, "the laptop's right edge does not warp");
}

#[test]
fn without_the_option_moving_down_between_monitors_stays_native() {
    let (before, cursor) = pointer_moves(desk(false), &[(-1308, -1210), (-1308, -1190)]);
    assert_eq!(cursor, before);
}

#[test]
fn with_two_displays_the_option_keeps_the_laptop_in_the_row() {
    let config: Config = (
        MainOptions {
            horizontal_mouse_warp: Some(1),
            horizontal_mouse_warp_lowest_below: Some(true),
            ..Default::default()
        },
        vec![],
    )
        .into();
    let harness = TestHarness::new()
        .with_config(config)
        .with_windows(1)
        .with_display(EXT_DISPLAY_ID, left_monitor(), vec![EXT_WORKSPACE_ID]);
    let (_, cursor) = pointer_moves(harness, &[(990, 400), (1023, 400)]);
    assert!(
        left_monitor().contains(cursor),
        "the laptop's right edge still reaches the display above: {cursor:?}"
    );
    assert!(!laptop().contains(cursor));
}

#[test]
fn a_stale_event_after_a_sideways_warp_is_not_a_crossing() {
    let (_, cursor) = pointer_moves(desk(true), &[(480, -600), (511, -600), (500, -600)]);
    assert!(
        right_monitor().contains(cursor),
        "the pointer stays where the sideways warp put it: {cursor:?}"
    );
}

#[test]
fn top_pixel_row_of_laptop_belongs_to_laptop_not_monitor_above() {
    let (before, cursor) = pointer_moves(desk(true), &[(-200, -1500), (200, 0)]);
    assert_eq!(
        cursor, before,
        "y = 0 is the top pixel row of the laptop, not the bottom edge of the left monitor"
    );
}

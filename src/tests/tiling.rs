use crate::commands::{Command, Direction, Operation, ResizeDirection};
use crate::config::{Config, MainOptions, WindowParams};
use crate::events::Event;
use crate::{assert_window_at, assert_window_size};
use bevy::prelude::*;

use super::*;

#[test]
#[allow(clippy::too_many_lines)]
fn test_window_shuffle() {
    const PADDING_LEFT: u16 = 3;
    const PADDING_RIGHT: u16 = 5;
    const PADDING_TOP: u16 = 7;
    const PADDING_BOTTOM: u16 = 9;
    const SLIVER_WIDTH: u16 = 5;

    let commands = vec![
        Event::MenuOpened { window_id: 0 }, // 0
        Event::Command {
            command: Command::PrintState,
        },
        Event::Command {
            command: Command::Window(Operation::Focus(Direction::Last)),
        }, // 2
        Event::Command {
            command: Command::Window(Operation::Focus(Direction::First)),
        }, // 3
        Event::Command {
            command: Command::Window(Operation::Focus(Direction::East)),
        }, // 4
        Event::Command {
            command: Command::Window(Operation::Stack(true)),
        }, // 5
        Event::Command {
            command: Command::Window(Operation::Center),
        }, // 6
        Event::Command {
            command: Command::Window(Operation::Focus(Direction::East)),
        }, // 7
        Event::Command {
            command: Command::Window(Operation::Stack(true)),
        }, // 8
        Event::Command {
            command: Command::Window(Operation::Center),
        }, // 9
        Event::Command {
            command: Command::PrintState,
        }, // 10
    ];

    // Logical width includes padding expansion on each side.
    let logical_width = TEST_WINDOW_WIDTH;
    let top_edge = TEST_MENUBAR_HEIGHT + i32::from(PADDING_TOP);
    let left_edge = i32::from(PADDING_LEFT);
    let right_edge = TEST_DISPLAY_WIDTH - i32::from(PADDING_RIGHT);
    let offscreen_right = right_edge - i32::from(SLIVER_WIDTH) + i32::from(PADDING_RIGHT);
    let offscreen_left =
        left_edge - logical_width + i32::from(SLIVER_WIDTH) - i32::from(PADDING_LEFT);
    let centered = (TEST_DISPLAY_WIDTH - logical_width) / 2;

    let mut params = WindowParams::new(".*", None);
    params.vertical_padding = Some(3);
    params.horizontal_padding = Some(2);
    let config: Config = (
        MainOptions {
            padding_left: Some(PADDING_LEFT),
            padding_right: Some(PADDING_RIGHT),
            padding_top: Some(PADDING_TOP),
            padding_bottom: Some(PADDING_BOTTOM),
            ..Default::default()
        },
        vec![params],
    )
        .into();

    TestHarness::new()
        .with_config(config)
        .with_windows(5)
        .on_iteration(2, move |world, _state| {
            assert_window_at!(world, 0, offscreen_left, top_edge);
            assert_window_at!(world, 1, offscreen_left, top_edge);
            assert_window_at!(world, 2, right_edge - 3 * logical_width, top_edge);
            assert_window_at!(world, 3, right_edge - 2 * logical_width, top_edge);
            assert_window_at!(world, 4, right_edge - logical_width, top_edge);
        })
        .on_iteration(3, move |world, _state| {
            assert_window_at!(world, 0, left_edge, top_edge);
            assert_window_at!(world, 1, left_edge + logical_width, top_edge);
            assert_window_at!(world, 2, left_edge + 2 * logical_width, top_edge);
            assert_window_at!(world, 3, offscreen_right, top_edge);
            assert_window_at!(world, 4, offscreen_right, top_edge);
        })
        .on_iteration(6, move |world, _state| {
            assert_window_at!(world, 0, centered, top_edge);
            assert_window_at!(world, 1, centered, 393);
            assert_window_at!(world, 2, centered + logical_width, top_edge);
            assert_window_at!(world, 3, offscreen_right, top_edge);
            assert_window_at!(world, 4, offscreen_right, top_edge);
        })
        .on_iteration(10, move |world, _state| {
            assert_window_at!(world, 0, centered, top_edge);
            assert_window_at!(world, 1, centered, 271);
            assert_window_at!(world, 2, centered, 515);
            assert_window_at!(world, 3, centered + logical_width, top_edge);
            assert_window_at!(world, 4, offscreen_right, top_edge);
        })
        .run(commands);
}

#[test]
fn test_window_balance() {
    let commands = vec![
        Event::MenuOpened { window_id: 0 },
        Event::Command {
            command: Command::Window(Operation::Resize(ResizeDirection::Grow)),
        },
        Event::Command {
            command: Command::Window(Operation::Balance),
        },
    ];

    TestHarness::new()
        .with_windows(3)
        .on_iteration(1, |world, _state| {
            // After grow, window 0 should be 512 (50% of 1024).
            assert_window_size!(world, 0, 512, 748);
        })
        .on_iteration(2, |world, _state| {
            // After balance, all windows should match window 0's width.
            assert_window_size!(world, 0, 512, 748);
            assert_window_size!(world, 1, 512, 748);
            assert_window_size!(world, 2, 512, 748);
        })
        .run(commands);
}

#[test]
fn test_startup_windows() {
    let commands = vec![
        Event::MenuOpened { window_id: 0 },
        Event::Command {
            command: Command::Window(Operation::Focus(Direction::East)),
        },
        Event::Command {
            command: Command::Window(Operation::Focus(Direction::East)),
        },
        Event::Command {
            command: Command::Window(Operation::Focus(Direction::First)),
        },
        Event::Command {
            command: Command::PrintState,
        },
    ];

    TestHarness::new()
        .with_windows(5)
        .on_iteration(4, |world, _state| {
            assert_window_at!(world, 0, 0, TEST_MENUBAR_HEIGHT);
            assert_window_at!(world, 1, 400, TEST_MENUBAR_HEIGHT);
            assert_window_at!(world, 2, 800, TEST_MENUBAR_HEIGHT);
        })
        .run(commands);
}

#[test]
fn test_window_resize_grow_and_shrink_cycle() {
    let commands = vec![
        Event::MenuOpened { window_id: 0 },
        Event::Command {
            command: Command::Window(Operation::Resize(ResizeDirection::Grow)),
        },
        Event::Command {
            command: Command::Window(Operation::Resize(ResizeDirection::Grow)),
        },
        Event::Command {
            command: Command::Window(Operation::Resize(ResizeDirection::Grow)),
        },
        Event::Command {
            command: Command::Window(Operation::Resize(ResizeDirection::Shrink)),
        },
    ];

    let config: Config = (
        MainOptions {
            preset_column_widths: vec![0.25, 0.5, 0.75],
            ..Default::default()
        },
        vec![],
    )
        .into();

    TestHarness::new()
        .with_config(config)
        .with_windows(1)
        .on_iteration(1, |world, _state| {
            assert_window_size!(world, 0, 512, 748);
        })
        .on_iteration(2, |world, _state| {
            assert_window_size!(world, 0, 768, 748);
        })
        .on_iteration(3, |world, _state| {
            assert_window_size!(world, 0, 256, 748);
        })
        .on_iteration(4, |world, _state| {
            assert_window_size!(world, 0, 768, 748);
        })
        .run(commands);
}

#[test]
fn test_window_can_resize_to_two_display_widths_and_scroll() {
    let commands = vec![
        Event::MenuOpened { window_id: 0 },
        Event::Command {
            command: Command::Window(Operation::SetWidth(2.0)),
        },
        Event::Swipe {
            delta: 0.3,
            fingers: 3,
        },
        Event::Command {
            command: Command::Window(Operation::Snap),
        },
    ];

    let config: Config = (
        MainOptions {
            swipe_gesture_fingers: Some(3),
            animation_speed: Some(10000.0),
            ..Default::default()
        },
        vec![],
    )
        .into();

    TestHarness::new()
        .with_config(config)
        .with_windows(1)
        .on_iteration(1, |world, _state| {
            assert_window_size!(world, 0, 2048, 748);
            assert_oversized_window_is_pannable(world, 0);
        })
        .on_iteration(2, |world, _state| {
            assert_window_size!(world, 0, 2048, 748);
            assert_oversized_window_is_pannable(world, 0);
        })
        .on_iteration(3, |world, _state| {
            assert_window_size!(world, 0, 2048, 748);
            assert_oversized_window_is_pannable(world, 0);
        })
        .run(commands);
}

fn assert_oversized_window_is_pannable(world: &mut World, id: i32) {
    let mut query = world.query::<&crate::manager::Window>();
    let window = query
        .iter(world)
        .find(|window| window.id() == id)
        .expect("window not found");
    let x = window.frame().min.x;
    assert!(
        (-TEST_DISPLAY_WIDTH..=0).contains(&x),
        "oversized window must stay within its pannable range, got x={x}"
    );
}

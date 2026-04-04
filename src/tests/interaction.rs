use bevy::prelude::*;

use crate::commands::{Command, Direction, Operation};
use crate::config::{Config, MainOptions, WindowParams};
use crate::ecs::SpawnWindowTrigger;
use crate::events::Event;
use crate::manager::{Origin, Size, Window};
use crate::{assert_focused, assert_window_at, assert_window_size};

use super::*;

#[test]
fn test_dont_focus() {
    let commands = vec![
        Event::MenuOpened { window_id: 0 }, // 0
        Event::Command {
            command: Command::Window(Operation::Focus(Direction::Last)),
        }, // 1
        Event::Command {
            command: Command::Window(Operation::Focus(Direction::First)),
        }, // 2
        Event::Command {
            command: Command::PrintState,
        }, // 3
    ];

    let offscreen_right = TEST_DISPLAY_WIDTH - 5;

    let mut params = WindowParams::new(".*", None);
    params.dont_focus = Some(true);
    params.index = Some(100);
    let config: Config = (MainOptions::default(), vec![params]).into();

    let mut harness = TestHarness::new().with_config(config).with_windows(3);

    let app = setup_process(harness.app.world_mut());
    let internal_queue = harness.internal_queue.clone();

    harness
        .on_iteration(1, move |world| {
            let origin = Origin::new(0, 0);
            let size = Size::new(TEST_WINDOW_WIDTH, TEST_WINDOW_HEIGHT);
            let window = MockWindow::new(
                3,
                IRect {
                    min: origin,
                    max: origin + size,
                },
                internal_queue.clone(),
                app.clone(),
            );
            let window = Window::new(Box::new(window));
            world.trigger(SpawnWindowTrigger(vec![window]));
        })
        .on_iteration(3, move |world| {
            assert_window_at!(world, 2, 0, TEST_MENUBAR_HEIGHT);
            assert_window_at!(world, 1, 400, TEST_MENUBAR_HEIGHT);
            assert_window_at!(world, 0, 800, TEST_MENUBAR_HEIGHT);
            assert_window_at!(world, 3, offscreen_right, TEST_MENUBAR_HEIGHT);
            assert_focused!(world, 2);
        })
        .run(commands);
}

#[test]
fn test_offscreen_windows_preserve_height() {
    let expected_height = TEST_DISPLAY_HEIGHT - TEST_MENUBAR_HEIGHT;

    let commands = vec![
        Event::MenuOpened { window_id: 0 },
        Event::Command {
            command: Command::Window(Operation::Focus(Direction::First)),
        },
    ];

    TestHarness::new()
        .with_windows(5)
        .on_iteration(1, move |world| {
            assert_window_size!(world, 4, TEST_WINDOW_WIDTH, expected_height);
            assert_window_size!(world, 3, TEST_WINDOW_WIDTH, expected_height);
            assert_window_size!(world, 2, TEST_WINDOW_WIDTH, expected_height);
            assert_window_size!(world, 1, TEST_WINDOW_WIDTH, expected_height);
            assert_window_size!(world, 0, TEST_WINDOW_WIDTH, expected_height);
        })
        .run(commands);
}

#[test]
fn test_sliver_smaller_than_edge_padding() {
    const PADDING: u16 = 8;
    const SLIVER: u16 = 1;

    let commands = vec![
        Event::MenuOpened { window_id: 0 },
        Event::Command {
            command: Command::Window(Operation::Focus(Direction::Last)),
        },
        Event::Command {
            command: Command::Window(Operation::Focus(Direction::First)),
        },
        Event::Command {
            command: Command::Window(Operation::Focus(Direction::Last)),
        },
    ];

    let top_edge = TEST_MENUBAR_HEIGHT + i32::from(PADDING);
    let right_edge = TEST_DISPLAY_WIDTH - i32::from(PADDING);
    let offscreen_right = TEST_DISPLAY_WIDTH - i32::from(SLIVER);
    let offscreen_left = i32::from(SLIVER) - TEST_WINDOW_WIDTH;
    let left_edge = i32::from(PADDING);

    let config: Config = (
        MainOptions {
            sliver_width: Some(SLIVER),
            padding_top: Some(PADDING),
            padding_bottom: Some(PADDING),
            padding_left: Some(PADDING),
            padding_right: Some(PADDING),
            ..Default::default()
        },
        vec![],
    )
        .into();

    TestHarness::new()
        .with_config(config)
        .with_windows(5)
        .on_iteration(2, move |world| {
            assert_window_at!(world, 4, left_edge, top_edge);
            assert_window_at!(world, 3, left_edge + TEST_WINDOW_WIDTH, top_edge);
            assert_window_at!(world, 2, left_edge + 2 * TEST_WINDOW_WIDTH, top_edge);
            assert_window_at!(world, 1, offscreen_right, top_edge);
            assert_window_at!(world, 0, offscreen_right, top_edge);
        })
        .on_iteration(3, move |world| {
            assert_window_at!(world, 4, offscreen_left, top_edge);
            assert_window_at!(world, 3, offscreen_left, top_edge);
            assert_window_at!(world, 2, right_edge - 3 * TEST_WINDOW_WIDTH, top_edge);
            assert_window_at!(world, 1, right_edge - 2 * TEST_WINDOW_WIDTH, top_edge);
            assert_window_at!(world, 0, right_edge - TEST_WINDOW_WIDTH, top_edge);
        })
        .run(commands);
}

#[test]
fn test_scrolling() {
    let commands = vec![
        Event::MenuOpened { window_id: 0 },
        Event::Command {
            command: Command::Window(Operation::Focus(Direction::Last)),
        },
        Event::Command {
            command: Command::Window(Operation::Focus(Direction::First)),
        },
        Event::Command {
            command: Command::PrintState,
        },
        Event::Swipe {
            deltas: vec![0.1, 0.1, 0.1],
        },
        Event::Command {
            command: Command::PrintState,
        },
    ];

    let config: Config = (
        MainOptions {
            swipe_gesture_fingers: Some(3),
            ..Default::default()
        },
        vec![],
    )
        .into();

    TestHarness::new()
        .with_config(config)
        .with_windows(3)
        .on_iteration(3, move |world| {
            assert_window_at!(world, 2, 0, TEST_MENUBAR_HEIGHT);
            assert_window_at!(world, 1, 400, TEST_MENUBAR_HEIGHT);
            assert_window_at!(world, 0, 800, TEST_MENUBAR_HEIGHT);
        })
        .on_iteration(5, move |world| {
            assert_window_at!(world, 2, -395, TEST_MENUBAR_HEIGHT);
            assert_window_at!(world, 1, -395, TEST_MENUBAR_HEIGHT);
            assert_window_at!(world, 0, 0, TEST_MENUBAR_HEIGHT);
        })
        .run(commands);
}

#[test]
fn test_window_hidden_ratio() {
    let commands = vec![
        Event::MenuOpened { window_id: 0 },
        Event::Swipe {
            deltas: vec![0.1, 0.1, 0.1],
        },
        Event::Command {
            command: Command::Window(Operation::Focus(Direction::First)),
        },
    ];

    let config: Config = (
        MainOptions {
            window_hidden_ratio: Some(0.5),
            animation_speed: Some(10000.0),
            swipe_gesture_fingers: Some(3),
            ..Default::default()
        },
        vec![],
    )
        .into();

    TestHarness::new()
        .with_config(config)
        .with_windows(2)
        .on_iteration(2, |world| {
            let mut query = world.query::<&Window>();
            let window = query.iter(world).find(|w| w.id() == 1).unwrap();
            assert!(window.frame().min.x < 0);
        })
        .run(commands);
}

#[test]
fn test_window_hidden_ratio_swap() {
    let commands = vec![
        Event::MenuOpened { window_id: 0 },
        Event::Command {
            command: Command::Window(Operation::Center),
        },
        Event::Command {
            command: Command::Window(Operation::Swap(Direction::Last)),
        },
    ];

    let config: Config = (
        MainOptions {
            window_hidden_ratio: Some(1.0),
            animation_speed: Some(10000.0),
            ..Default::default()
        },
        vec![],
    )
        .into();

    let centered = (TEST_DISPLAY_WIDTH - TEST_WINDOW_WIDTH) / 2;

    TestHarness::new()
        .with_config(config)
        .with_windows(5)
        .on_iteration(1, move |world| {
            assert_window_at!(world, 4, centered, TEST_MENUBAR_HEIGHT);
        })
        .on_iteration(2, move |world| {
            assert_window_at!(world, 4, centered, TEST_MENUBAR_HEIGHT);
            assert_window_at!(world, 0, centered - TEST_WINDOW_WIDTH, TEST_MENUBAR_HEIGHT);
        })
        .run(commands);
}

#[test]
fn test_rapid_focus_not_swallowed() {
    let mut harness = TestHarness::new().with_windows(5);

    harness.run(vec![
        Event::MenuOpened { window_id: 0 },
        Event::Command {
            command: Command::Window(Operation::Focus(Direction::Last)),
        },
    ]);

    verify_focused_window(0, harness.app.world_mut());

    let focus_west = Event::Command {
        command: Command::Window(Operation::Focus(Direction::West)),
    };
    for _ in 0..3 {
        harness
            .app
            .world_mut()
            .write_message::<Event>(focus_west.clone());
        harness.app.update();
    }

    verify_focused_window(3, harness.app.world_mut());
}

#[test]
fn test_stale_focus_event_ignored() {
    let commands = vec![
        Event::MenuOpened { window_id: 0 },
        Event::Command {
            command: Command::Window(Operation::Focus(Direction::East)),
        },
        Event::WindowFocused { window_id: 4 },
        Event::Command {
            command: Command::PrintState,
        },
    ];

    TestHarness::new()
        .with_windows(5)
        .on_iteration(1, |world| {
            assert_focused!(world, 3);
        })
        .on_iteration(3, |world| {
            assert_focused!(world, 3);
        })
        .run(commands);
}

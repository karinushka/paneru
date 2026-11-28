use std::time::Duration;

use crate::commands::{Command, Direction, Operation, ResizeDirection};
use crate::config::{
    Config, InitialWindowWidth, InitialWindowWidthMode, MainOptions, WindowParams, parse_command,
};
use crate::ecs::layout::LayoutStrip;
use crate::ecs::{ActiveWorkspaceMarker, SpawnWindowTrigger};
use crate::events::{DestroySource, Event};
use crate::{assert_focused, assert_window_at, assert_window_size};
use bevy::math::IRect;
use bevy::prelude::*;

use super::*;

#[test]
fn new_window_starts_at_complement_of_focused_width() {
    let mut params = WindowParams::new(".*", None);
    params.width = Some(InitialWindowWidth::Mode(
        InitialWindowWidthMode::ComplementFocused,
    ));
    let config: Config = (MainOptions::default(), vec![params]).into();

    TestHarness::new()
        .with_windows(1)
        .with_focused_window(0)
        .on_iteration(0, move |world, state| {
            world.insert_resource(config.clone());
            let frame = IRect::new(0, 0, TEST_WINDOW_WIDTH, TEST_WINDOW_HEIGHT);
            let window = state.spawn_window(TEST_PROCESS_ID, TEST_WORKSPACE_ID, 1, frame);
            world.trigger(SpawnWindowTrigger(vec![window]));
        })
        .on_iteration(1, |world, _state| {
            assert_window_size!(world, 0, TEST_WINDOW_WIDTH, 748);
            assert_window_size!(world, 1, TEST_DISPLAY_WIDTH - TEST_WINDOW_WIDTH, 748);
        })
        .run(vec![
            Event::MenuOpened { window_id: 0 },
            Event::Command {
                command: Command::PrintState,
            },
        ]);
}

#[test]
fn new_window_starts_full_width_after_full_width_focus() {
    let mut fixed = WindowParams::new(".*", None);
    fixed.width = Some(InitialWindowWidth::Ratio(1.0));
    let fixed_config: Config = (MainOptions::default(), vec![fixed]).into();

    let mut dynamic = WindowParams::new(".*", None);
    dynamic.width = Some(InitialWindowWidth::Mode(
        InitialWindowWidthMode::ComplementFocused,
    ));
    let dynamic_config: Config = (MainOptions::default(), vec![dynamic]).into();

    TestHarness::new()
        .with_config(fixed_config)
        .with_windows(1)
        .with_focused_window(0)
        .on_iteration(0, move |world, state| {
            assert_window_size!(world, 0, TEST_DISPLAY_WIDTH, 748);
            world.insert_resource(dynamic_config.clone());
            let frame = IRect::new(0, 0, TEST_WINDOW_WIDTH, TEST_WINDOW_HEIGHT);
            let window = state.spawn_window(TEST_PROCESS_ID, TEST_WORKSPACE_ID, 1, frame);
            world.trigger(SpawnWindowTrigger(vec![window]));
        })
        .on_iteration(1, |world, _state| {
            assert_window_size!(world, 1, TEST_DISPLAY_WIDTH, 748);
        })
        .run(vec![
            Event::MenuOpened { window_id: 0 },
            Event::Command {
                command: Command::PrintState,
            },
        ]);
}

#[test]
fn configured_width_uses_raw_display_bounds_with_edge_padding() {
    let options = MainOptions {
        padding_left: Some(20),
        padding_right: Some(30),
        ..MainOptions::default()
    };
    let mut params = WindowParams::new(".*", None);
    params.width = Some(InitialWindowWidth::Ratio(0.5));
    let config: Config = (options, vec![params]).into();

    TestHarness::new()
        .with_config(config)
        .with_windows(1)
        .on_iteration(0, |world, state| {
            let frame = IRect::new(0, 0, TEST_WINDOW_WIDTH, TEST_WINDOW_HEIGHT);
            let window = state.spawn_window(TEST_PROCESS_ID, TEST_WORKSPACE_ID, 1, frame);
            world.trigger(SpawnWindowTrigger(vec![window]));
        })
        .on_iteration(1, |world, _state| {
            assert_window_size!(world, 1, TEST_DISPLAY_WIDTH / 2, 748);
            let mut query = world.query::<(&crate::manager::Window, &crate::ecs::WidthRatio)>();
            let ratio = query
                .iter(world)
                .find_map(|(window, ratio)| (window.id() == 1).then_some(ratio.0));
            assert_eq!(ratio, Some(0.5));
        })
        .run(vec![
            Event::MenuOpened { window_id: 0 },
            Event::Command {
                command: Command::PrintState,
            },
        ]);
}

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
            continuous_swipe: Some(false),
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
fn test_window_resize_pressed_during_the_animation_advances_to_the_next_preset() {
    let mut params = WindowParams::new(".*", None);
    params.width = Some(InitialWindowWidth::Ratio(0.45));
    let config: Config = (
        MainOptions {
            preset_column_widths: vec![0.25, 0.5, 0.75],
            animation_speed: Some(8.0),
            ..Default::default()
        },
        vec![params],
    )
        .into();
    let mut harness = TestHarness::new().with_config(config).with_windows(1);
    harness.run(vec![Event::MenuOpened { window_id: 0 }]);
    harness.advance(Duration::from_secs(2));
    assert_window_size!(harness.world(), 0, 461, 748);

    let grow = || Event::Command {
        command: Command::Window(Operation::Resize(ResizeDirection::Grow)),
    };
    harness.world().write_message::<Event>(grow());
    harness.advance(Duration::from_millis(100));
    harness.world().write_message::<Event>(grow());
    harness.advance(Duration::from_secs(3));

    assert_window_size!(harness.world(), 0, 256, 748);
}

/// Stacks two windows, then cycles the focused one's height through the
/// presets. The window above it — the only neighbour, since the focused one is
/// last in the stack — absorbs the whole difference, and the column keeps
/// filling the viewport.
#[test]
fn test_window_vertical_resize_grow_and_shrink_cycle() {
    // The viewport is `TEST_DISPLAY_HEIGHT - TEST_MENUBAR_HEIGHT` tall and the
    // two stacked windows split it evenly to start with.
    const VIEWPORT_HEIGHT: i32 = TEST_DISPLAY_HEIGHT - TEST_MENUBAR_HEIGHT;

    let commands = vec![
        Event::MenuOpened { window_id: 0 }, // 0
        Event::Command {
            command: Command::Window(Operation::Focus(Direction::First)),
        }, // 1
        Event::Command {
            command: Command::Window(Operation::Focus(Direction::East)),
        }, // 2
        Event::Command {
            command: Command::Window(Operation::Stack(true)),
        }, // 3
        Event::Command {
            command: Command::Window(Operation::ResizeVertical(ResizeDirection::Grow)),
        }, // 4
        Event::Command {
            command: Command::Window(Operation::ResizeVertical(ResizeDirection::Grow)),
        }, // 5
        Event::Command {
            command: Command::Window(Operation::ResizeVertical(ResizeDirection::Shrink)),
        }, // 6
    ];

    let config: Config = (
        MainOptions {
            preset_stack_heights: vec![0.3, 0.5, 0.7],
            ..Default::default()
        },
        vec![],
    )
        .into();

    TestHarness::new()
        .with_config(config)
        .with_windows(2)
        .on_iteration(3, |world, _state| {
            assert_window_size!(world, 0, TEST_WINDOW_WIDTH, VIEWPORT_HEIGHT / 2);
            assert_window_size!(world, 1, TEST_WINDOW_WIDTH, VIEWPORT_HEIGHT / 2);
        })
        .on_iteration(4, |world, _state| {
            // 0.7 of the viewport, with window 0 giving up exactly the delta.
            assert_window_size!(world, 1, TEST_WINDOW_WIDTH, 524);
            assert_window_size!(world, 0, TEST_WINDOW_WIDTH, VIEWPORT_HEIGHT - 524);
        })
        .on_iteration(5, |world, _state| {
            // Past the last preset, so it cycles back to the smallest.
            assert_window_size!(world, 1, TEST_WINDOW_WIDTH, 224);
            assert_window_size!(world, 0, TEST_WINDOW_WIDTH, VIEWPORT_HEIGHT - 224);
        })
        .on_iteration(6, |world, _state| {
            // Below the smallest preset, so shrinking cycles to the largest.
            assert_window_size!(world, 1, TEST_WINDOW_WIDTH, 524);
            assert_window_size!(world, 0, TEST_WINDOW_WIDTH, VIEWPORT_HEIGHT - 524);
        })
        .run(commands);
}

/// A preset that would starve the neighbour is clamped to leave it the minimum
/// window height, rather than letting the binpacker average the column.
#[test]
fn test_window_vertical_resize_leaves_the_neighbour_its_minimum() {
    const VIEWPORT_HEIGHT: i32 = TEST_DISPLAY_HEIGHT - TEST_MENUBAR_HEIGHT;
    const MIN_WINDOW_HEIGHT: i32 = 200;

    let commands = vec![
        Event::MenuOpened { window_id: 0 },
        Event::Command {
            command: Command::Window(Operation::Focus(Direction::First)),
        },
        Event::Command {
            command: Command::Window(Operation::Focus(Direction::East)),
        },
        Event::Command {
            command: Command::Window(Operation::Stack(true)),
        },
        Event::Command {
            command: Command::Window(Operation::ResizeVertical(ResizeDirection::Grow)),
        },
    ];

    let config: Config = (
        MainOptions {
            // 0.9 of the viewport would leave the neighbour 75px.
            preset_stack_heights: vec![0.5, 0.9],
            ..Default::default()
        },
        vec![],
    )
        .into();

    TestHarness::new()
        .with_config(config)
        .with_windows(2)
        .on_iteration(4, |world, _state| {
            assert_window_size!(
                world,
                1,
                TEST_WINDOW_WIDTH,
                VIEWPORT_HEIGHT - MIN_WINDOW_HEIGHT
            );
            assert_window_size!(world, 0, TEST_WINDOW_WIDTH, MIN_WINDOW_HEIGHT);
        })
        .run(commands);
}

/// Outside a stack the binpacker always stretches a column's lone member to the
/// full viewport, so the command is a no-op there.
#[test]
fn test_window_vertical_resize_is_a_noop_outside_a_stack() {
    const VIEWPORT_HEIGHT: i32 = TEST_DISPLAY_HEIGHT - TEST_MENUBAR_HEIGHT;

    let commands = vec![
        Event::MenuOpened { window_id: 0 },
        Event::Command {
            command: Command::Window(Operation::ResizeVertical(ResizeDirection::Shrink)),
        },
    ];

    let config: Config = (
        MainOptions {
            preset_stack_heights: vec![0.3, 0.5, 0.7],
            ..Default::default()
        },
        vec![],
    )
        .into();

    TestHarness::new()
        .with_config(config)
        .with_windows(2)
        .on_iteration(1, |world, _state| {
            assert_window_size!(world, 0, TEST_WINDOW_WIDTH, VIEWPORT_HEIGHT);
            assert_window_size!(world, 1, TEST_WINDOW_WIDTH, VIEWPORT_HEIGHT);
        })
        .run(commands);
}

/// Four windows need 800px at the 200px minimum window height, more than the
/// 748px viewport, so the column is split evenly below that minimum.
fn assert_four_window_stack_split_evenly(world: &mut World) {
    const ROW_HEIGHT: i32 = (TEST_DISPLAY_HEIGHT - TEST_MENUBAR_HEIGHT) / 4;
    for id in 0..4 {
        assert_window_at!(world, id, 0, TEST_MENUBAR_HEIGHT + ROW_HEIGHT * id);
        assert_window_size!(world, id, TEST_WINDOW_WIDTH, ROW_HEIGHT);
    }
}

#[test]
fn test_stack_too_tall_for_the_minimum_height_is_split_evenly() {
    let commands = vec![
        Event::MenuOpened { window_id: 0 },
        Event::Command {
            command: Command::Window(Operation::Focus(Direction::First)),
        },
        Event::Command {
            command: Command::Window(Operation::Focus(Direction::East)),
        },
        Event::Command {
            command: Command::Window(Operation::Stack(true)),
        },
        Event::Command {
            command: Command::Window(Operation::Focus(Direction::East)),
        },
        Event::Command {
            command: Command::Window(Operation::Stack(true)),
        },
        Event::Command {
            command: Command::Window(Operation::Focus(Direction::East)),
        },
        Event::Command {
            command: Command::Window(Operation::Stack(true)),
        },
    ];

    TestHarness::new()
        .with_windows(4)
        .on_iteration(7, |world, _state| {
            assert_four_window_stack_split_evenly(world);
        })
        .run(commands);
}

/// A whole stack joining a full-height window: not even the full-height window
/// keeps its height, which takes a different give-up path in the binpacker.
#[test]
fn test_stack_joining_a_full_height_window_is_split_evenly() {
    let commands = vec![
        Event::MenuOpened { window_id: 0 },
        Event::Command {
            command: Command::Window(Operation::Focus(Direction::First)),
        },
        Event::Command {
            command: Command::Window(Operation::Focus(Direction::East)),
        },
        Event::Command {
            command: Command::Window(Operation::Focus(Direction::East)),
        },
        Event::Command {
            command: Command::Window(Operation::Stack(true)),
        },
        Event::Command {
            command: Command::Window(Operation::Focus(Direction::East)),
        },
        Event::Command {
            command: Command::Window(Operation::Stack(true)),
        },
        Event::Command {
            command: Command::Window(Operation::Stack(true)),
        },
    ];

    TestHarness::new()
        .with_windows(4)
        .on_iteration(6, |world, _state| {
            let strip = world.query::<&LayoutStrip>().single(world).unwrap();
            assert_eq!(strip.len(), 2, "windows 1-3 share the second column");
        })
        .on_iteration(7, |world, _state| {
            assert_four_window_stack_split_evenly(world);
        })
        .run(commands);
}

/// A command as its keybinding names it, e.g. `window_step_grow`.
fn binding(name: &str) -> Event {
    let argv = name.split('_').collect::<Vec<_>>();
    Event::Command {
        command: parse_command(&argv).unwrap_or_else(|err| panic!("{name}: {err}")),
    }
}

fn resize_step_config(step: f64) -> Config {
    Config::try_from(format!("[options]\nwindow_resize_step = {step}\n[bindings]\n").as_str())
        .expect("config should parse")
}

/// The width steps by `window_resize_step` of the display, stops at the full
/// display width and at one step, and never wraps around.
#[test]
fn test_window_step_resize_changes_width_by_the_step_without_wrapping() {
    let mut commands = vec![Event::MenuOpened { window_id: 0 }];
    commands.extend((0..4).map(|_| binding("window_step_grow")));
    commands.extend((0..4).map(|_| binding("window_step_shrink")));

    // A quarter of the 1024px display is 256px.
    let widths = [656, 912, 1024, 1024, 768, 512, 256, 256];
    let mut harness = TestHarness::new()
        .with_config(resize_step_config(0.25))
        .with_windows(1);
    for (index, width) in widths.into_iter().enumerate() {
        harness = harness.on_iteration(index + 1, move |world, _state| {
            assert_window_size!(world, 0, width, TEST_DISPLAY_HEIGHT - TEST_MENUBAR_HEIGHT);
        });
    }
    harness.run(commands);
}

/// The default step is five percentage points of the display width.
#[test]
fn test_window_step_resize_defaults_to_five_points() {
    const VIEWPORT_HEIGHT: i32 = TEST_DISPLAY_HEIGHT - TEST_MENUBAR_HEIGHT;

    TestHarness::new()
        .with_windows(1)
        .on_iteration(1, |world, _state| {
            // 400px is 39.0625% of 1024px, and 44.0625% is 451.2px.
            assert_window_size!(world, 0, 451, VIEWPORT_HEIGHT);
        })
        .on_iteration(2, |world, _state| {
            assert_window_size!(world, 0, TEST_WINDOW_WIDTH, VIEWPORT_HEIGHT);
        })
        .run(vec![
            Event::MenuOpened { window_id: 0 },
            binding("window_step_grow"),
            binding("window_step_shrink"),
        ]);
}

/// In a stack, the height steps by `window_resize_step` of the viewport, the
/// neighbour absorbs the difference, and the neighbour keeps its minimum.
#[test]
fn test_window_vertical_step_resize_changes_height_by_the_step() {
    const VIEWPORT_HEIGHT: i32 = TEST_DISPLAY_HEIGHT - TEST_MENUBAR_HEIGHT;

    let mut commands = vec![
        Event::MenuOpened { window_id: 0 },
        Event::Command {
            command: Command::Window(Operation::Focus(Direction::First)),
        },
        Event::Command {
            command: Command::Window(Operation::Focus(Direction::East)),
        },
        Event::Command {
            command: Command::Window(Operation::Stack(true)),
        },
        binding("window_vertical_step_grow"),
        binding("window_vertical_step_shrink"),
    ];
    commands.extend((0..4).map(|_| binding("window_vertical_step_grow")));

    // A tenth of the 748px viewport is 74.8px, and 548px leaves the neighbour
    // its 200px minimum.
    let heights = [449, 374, 449, 524, 548, 548];
    let mut harness = TestHarness::new()
        .with_config(resize_step_config(0.1))
        .with_windows(2)
        .on_iteration(3, |world, _state| {
            assert_window_size!(world, 1, TEST_WINDOW_WIDTH, VIEWPORT_HEIGHT / 2);
        });
    for (index, height) in heights.into_iter().enumerate() {
        harness = harness.on_iteration(index + 4, move |world, _state| {
            assert_window_size!(world, 1, TEST_WINDOW_WIDTH, height);
            assert_window_size!(world, 0, TEST_WINDOW_WIDTH, VIEWPORT_HEIGHT - height);
        });
    }
    harness.run(commands);
}

/// Even with a large `window_resize_step`, shrinking a stacked window still
/// reaches `MIN_WINDOW_HEIGHT` rather than getting stranded at one step.
#[test]
fn test_window_vertical_step_shrink_reaches_minimum_height_with_large_step() {
    const VIEWPORT_HEIGHT: i32 = TEST_DISPLAY_HEIGHT - TEST_MENUBAR_HEIGHT;
    const MIN_HEIGHT: i32 = 200;

    let commands = vec![
        Event::MenuOpened { window_id: 0 },
        Event::Command {
            command: Command::Window(Operation::Focus(Direction::First)),
        },
        Event::Command {
            command: Command::Window(Operation::Focus(Direction::East)),
        },
        Event::Command {
            command: Command::Window(Operation::Stack(true)),
        },
        binding("window_vertical_step_shrink"),
    ];

    TestHarness::new()
        .with_config(resize_step_config(0.5))
        .with_windows(2)
        .on_iteration(3, |world, _state| {
            assert_window_size!(world, 1, TEST_WINDOW_WIDTH, VIEWPORT_HEIGHT / 2);
        })
        .on_iteration(4, |world, _state| {
            assert_window_size!(world, 1, TEST_WINDOW_WIDTH, MIN_HEIGHT);
            assert_window_size!(world, 0, TEST_WINDOW_WIDTH, VIEWPORT_HEIGHT - MIN_HEIGHT);
        })
        .run(commands);
}

/// Outside a stack a column's lone window always fills the viewport height.
#[test]
fn test_window_vertical_step_resize_is_a_noop_outside_a_stack() {
    const VIEWPORT_HEIGHT: i32 = TEST_DISPLAY_HEIGHT - TEST_MENUBAR_HEIGHT;

    TestHarness::new()
        .with_windows(1)
        .on_iteration(1, |world, _state| {
            assert_window_size!(world, 0, TEST_WINDOW_WIDTH, VIEWPORT_HEIGHT);
        })
        .run(vec![
            Event::MenuOpened { window_id: 0 },
            binding("window_vertical_step_shrink"),
        ]);
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
            distance_cm: 0.0,
            elapsed_secs: None,
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

/// A `LayoutOp::SetWidth` handed back by a Lua handler must resize the focused
/// window just like the interactive `Operation::SetWidth` path, instead of only
/// storing a `WidthRatio` that nothing consumes and the next bounds sync
/// overwrites from the live frame.
#[test]
fn test_lua_set_width_layout_op_resizes_the_focused_window() {
    use crate::types::windowset::LayoutOp;

    const VIEWPORT_HEIGHT: i32 = TEST_DISPLAY_HEIGHT - TEST_MENUBAR_HEIGHT;

    let commands = vec![
        Event::Command {
            command: Command::PrintState,
        },
        Event::Command {
            command: Command::Layout(vec![LayoutOp::SetWidth {
                window: 0,
                ratio: 0.5,
            }]),
        },
    ];

    TestHarness::new()
        .with_windows(1)
        .on_iteration(1, |world, _state| {
            assert_window_size!(world, 0, TEST_DISPLAY_WIDTH / 2, VIEWPORT_HEIGHT);
        })
        .run(commands);
}

/// A `LayoutOp::SetWidth` names its window, so it must resize that window even
/// when another one holds the focus, and leave the focused one alone.
#[test]
fn test_lua_set_width_layout_op_resizes_an_unfocused_window() {
    use crate::types::windowset::LayoutOp;

    const VIEWPORT_HEIGHT: i32 = TEST_DISPLAY_HEIGHT - TEST_MENUBAR_HEIGHT;

    let commands = vec![
        Event::Command {
            command: Command::PrintState,
        },
        Event::Command {
            command: Command::Layout(vec![LayoutOp::SetWidth {
                window: 1,
                ratio: 0.5,
            }]),
        },
    ];

    TestHarness::new()
        .with_windows(2)
        .with_focused_window(0)
        .on_iteration(1, |world, _state| {
            assert_focused!(world, 0);
            assert_window_size!(world, 0, TEST_WINDOW_WIDTH, VIEWPORT_HEIGHT);
            assert_window_size!(world, 1, TEST_DISPLAY_WIDTH / 2, VIEWPORT_HEIGHT);
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

/// A floating window is out of the tiling layout, so it must not keep a slot in
/// the strip: the tiler lays columns out left to right by accumulated width, so
/// a floating member reserves space no tiled window occupies — a gap.
#[test]
fn test_floating_window_does_not_hold_a_slot_in_the_strip() {
    let commands = vec![
        Event::Command {
            command: Command::PrintState,
        }, // 0
        Event::Command {
            command: Command::Window(Operation::Manage),
        }, // 1 — float the focused window
        Event::Command {
            command: Command::PrintState,
        }, // 2
    ];

    TestHarness::new()
        .with_windows(3)
        .on_iteration(0, |world, _state| {
            // Window 0 holds the focus, so it is the one about to float.
            assert_eq!(window_x(world, 0), 0);
            assert_eq!(window_x(world, 1), TEST_WINDOW_WIDTH);
            assert_eq!(window_x(world, 2), 2 * TEST_WINDOW_WIDTH);
        })
        .on_iteration(2, |world, _state| {
            let entity = find_window_entity(0, world);
            let mut query = world.query::<&LayoutStrip>();
            assert!(
                !query.iter(world).any(|strip| strip.contains(entity)),
                "a floating window must not stay in any layout strip"
            );
            // The slot it vacated has to close up: window 1 slides to the left
            // edge rather than leaving an empty column where window 0 was.
            assert_eq!(
                window_x(world, 1),
                0,
                "the tiled windows must close the gap the floating one left"
            );
            assert_eq!(window_x(world, 2), TEST_WINDOW_WIDTH);
        })
        .run(commands);
}

fn active_strip_window_ids(world: &mut World) -> Vec<i32> {
    let columns = world
        .query_filtered::<&LayoutStrip, With<ActiveWorkspaceMarker>>()
        .single(world)
        .expect("an active workspace")
        .all_columns();
    let mut windows = world.query::<&crate::manager::Window>();
    columns
        .into_iter()
        .map(|entity| windows.get(world, entity).expect("a window").id())
        .collect()
}

fn float_and_retile_middle_window(config: Config) {
    let commands = vec![
        Event::Command {
            command: Command::PrintState,
        }, // 0
        Event::Command {
            command: Command::Window(Operation::Focus(Direction::East)),
        }, // 1
        Event::Command {
            command: Command::Window(Operation::Manage),
        }, // 2: float window 1
        Event::Command {
            command: Command::Window(Operation::Manage),
        }, // 3: tile it again
    ];

    let order = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
    let before = order.clone();
    let after = order;
    TestHarness::new()
        .with_config(config)
        .with_windows(3)
        .on_iteration(1, move |world, _state| {
            assert_focused!(world, 1);
            *before.borrow_mut() = active_strip_window_ids(world);
        })
        .on_iteration(3, move |world, _state| {
            assert_eq!(
                active_strip_window_ids(world),
                *after.borrow(),
                "the re-tiled window must return to the slot it floated from"
            );
        })
        .run(commands);
}

#[test]
fn test_retiled_window_returns_to_its_slot() {
    float_and_retile_middle_window(Config::default());
}

/// A rule's `index` places a new window, not one that left its slot to float.
#[test]
fn test_retiled_window_returns_to_its_slot_despite_an_insertion_rule() {
    let mut params = WindowParams::new(".*", None);
    params.index = Some(0);
    float_and_retile_middle_window((MainOptions::default(), vec![params]).into());
}

#[test]
fn test_unordered_window_releases_its_layout_space() {
    let mut harness = TestHarness::new().with_windows(3);

    // Allow initial layout to settle.
    harness.advance(Duration::from_millis(100));
    assert_eq!(window_x(harness.app.world_mut(), 0), 0);
    assert_eq!(window_x(harness.app.world_mut(), 1), TEST_WINDOW_WIDTH);
    assert_eq!(window_x(harness.app.world_mut(), 2), TEST_WINDOW_WIDTH * 2);

    // Window 1 disappears silently from the OS (unordered on WindowServer and dead in AX).
    harness.mock_state.os_vanish_window(1);

    // Step past the 5-second periodic UNORDERED_WINDOW_SWEEP_FREQ timer.
    harness.advance(Duration::from_millis(5200));

    let mut query = harness.app.world_mut().query::<&crate::manager::Window>();
    assert!(
        query
            .iter(harness.app.world())
            .all(|window| window.id() != 1),
        "unordered window 1 should be removed within periodic check"
    );
    assert_eq!(window_x(harness.app.world_mut(), 0), 0);
    assert_eq!(window_x(harness.app.world_mut(), 2), TEST_WINDOW_WIDTH);
}

/// The same invariant for a window floated by a config rule rather than by the
/// toggle. This is the path that runs while the window is being spawned, so it
/// races the strip insertion the tiling path is doing at the same time.
#[test]
fn test_rule_floated_window_does_not_hold_a_slot_in_the_strip() {
    let commands = vec![
        Event::Command {
            command: Command::PrintState,
        },
        Event::Command {
            command: Command::PrintState,
        },
    ];

    // Only window 1 floats; 0 and 2 tile as usual.
    let mut params = WindowParams::new("^Window 1$", None);
    params.floating = Some(true);
    let config: Config = (MainOptions::default(), vec![params]).into();

    TestHarness::new()
        .with_config(config)
        .with_windows(3)
        .on_iteration(1, |world, _state| {
            let entity = find_window_entity(1, world);
            let mut query = world.query::<&LayoutStrip>();
            assert!(
                !query.iter(world).any(|strip| strip.contains(entity)),
                "a rule-floated window must not stay in any layout strip"
            );
            assert_eq!(
                window_x(world, 2) - window_x(world, 0),
                TEST_WINDOW_WIDTH,
                "the tiled windows must close the gap the floating one left"
            );
        })
        .run(commands);
}

/// Closing a window while its application stays alive must free its slot in
/// the strip. The AX element of such a window often keeps answering queries
/// after the window is gone, which used to make `window_destroyed_trigger`
/// mistake the destroy event for a space change and leave a gap where the
/// window had been.
#[test]
fn test_closing_window_of_live_app_closes_the_gap() {
    let commands = vec![
        Event::Command {
            command: Command::PrintState,
        }, // 0
        Event::Command {
            command: Command::PrintState,
        }, // 1
        Event::Command {
            command: Command::PrintState,
        }, // 2
    ];

    TestHarness::new()
        .with_windows(3)
        .on_iteration(0, |world, state| {
            let left = window_x(world, 0);
            assert_eq!(
                window_x(world, 2) - left,
                2 * TEST_WINDOW_WIDTH,
                "three windows should tile side by side before the close"
            );
            state.os_close_window(1);
        })
        .on_iteration(2, |world, _state| {
            assert!(
                !window_exists(world, 1),
                "closed window must be dropped from the world"
            );
            assert_eq!(
                window_x(world, 2) - window_x(world, 0),
                TEST_WINDOW_WIDTH,
                "surviving windows must close the gap left by window 1"
            );
        })
        .run(commands);
}

/// Closing the focused window focuses its right neighbour, wherever the strip
/// is scrolled; at the right edge, its left one.
#[test]
fn test_closing_focused_window_focuses_its_right_neighbour() {
    let commands = vec![
        Event::MenuOpened { window_id: 0 },
        Event::Command {
            command: Command::Window(Operation::Focus(Direction::Last)),
        },
        Event::Command {
            command: Command::Window(Operation::Focus(Direction::West)),
        }, // 2
        Event::Command {
            command: Command::PrintState,
        },
        Event::Command {
            command: Command::PrintState,
        }, // 4
        Event::Command {
            command: Command::PrintState,
        },
        Event::Command {
            command: Command::PrintState,
        }, // 6
    ];

    TestHarness::new()
        .with_windows(5)
        .on_iteration(2, |world, state| {
            assert_focused!(world, 3);
            state.os_close_window(3);
        })
        .on_iteration(4, |world, state| {
            assert!(!window_exists(world, 3), "closed window must be dropped");
            assert_focused!(world, 4);
            state.os_close_window(4);
        })
        .on_iteration(6, |world, _state| {
            assert!(!window_exists(world, 4), "closed window must be dropped");
            assert_focused!(world, 2);
        })
        .run(commands);
}

/// Closing the focused window of a stack focuses the one below it, and the
/// stack's bottom one the one above, so focus stays in the column.
#[test]
fn test_closing_focused_window_in_a_stack_stays_in_the_stack() {
    let commands = vec![
        Event::MenuOpened { window_id: 0 },
        Event::Command {
            command: Command::Window(Operation::Focus(Direction::East)),
        },
        Event::Command {
            command: Command::Window(Operation::Stack(true)),
        },
        Event::Command {
            command: Command::Window(Operation::Focus(Direction::East)),
        },
        Event::Command {
            command: Command::Window(Operation::Stack(true)),
        }, // 4: 0, 1 and 2 stacked, 2 focused
        Event::Command {
            command: Command::Window(Operation::Focus(Direction::North)),
        }, // 5
        Event::Command {
            command: Command::PrintState,
        },
        Event::Command {
            command: Command::PrintState,
        }, // 7
        Event::Command {
            command: Command::PrintState,
        },
        Event::Command {
            command: Command::PrintState,
        }, // 9
    ];

    TestHarness::new()
        .with_windows(4)
        .on_iteration(5, |world, state| {
            assert_focused!(world, 1);
            state.os_close_window(1);
        })
        .on_iteration(7, |world, state| {
            assert!(!window_exists(world, 1), "closed window must be dropped");
            assert_focused!(world, 2);
            state.os_close_window(2);
        })
        .on_iteration(9, |world, _state| {
            assert!(!window_exists(world, 2), "closed window must be dropped");
            assert_focused!(world, 0);
        })
        .run(commands);
}

/// A window opened beside the focused one hands focus back to it on closing,
/// unless focus has been to another column since.
#[test]
fn test_closing_a_new_window_returns_focus_to_its_opener() {
    let open = |world: &mut World, state: &MockState, id| {
        let frame = IRect::new(0, 0, TEST_WINDOW_WIDTH, TEST_WINDOW_HEIGHT);
        let window = state.spawn_window(TEST_PROCESS_ID, TEST_WORKSPACE_ID, id, frame);
        world.trigger(SpawnWindowTrigger(vec![window]));
        // macOS focuses a window as it opens.
        state.focus_window(id);
    };
    let commands = vec![
        Event::MenuOpened { window_id: 0 },
        Event::Command {
            command: Command::PrintState,
        },
        Event::Command {
            command: Command::PrintState,
        }, // 2
        Event::Command {
            command: Command::PrintState,
        },
        Event::Command {
            command: Command::PrintState,
        }, // 4
        Event::Command {
            command: Command::PrintState,
        }, // 5
        Event::Command {
            command: Command::Window(Operation::Focus(Direction::East)),
        },
        Event::Command {
            command: Command::Window(Operation::Focus(Direction::West)),
        }, // 7
        Event::Command {
            command: Command::PrintState,
        },
        Event::Command {
            command: Command::PrintState,
        }, // 9
    ];

    TestHarness::new()
        .with_windows(3)
        .with_focused_window(0)
        .on_iteration(0, move |world, state| open(world, &state, 3))
        .on_iteration(2, |world, state| {
            assert_eq!(strip_column(world, 3), Some(1), "opened beside window 0");
            assert_focused!(world, 3);
            state.os_close_window(3);
        })
        .on_iteration(4, move |world, state| {
            assert!(!window_exists(world, 3), "closed window must be dropped");
            assert_focused!(world, 0);
            open(world, &state, 4);
        })
        .on_iteration(5, |world, _state| assert_focused!(world, 4))
        .on_iteration(7, |world, state| {
            assert_focused!(world, 4);
            state.os_close_window(4);
        })
        .on_iteration(9, |world, _state| {
            assert!(!window_exists(world, 4), "closed window must be dropped");
            assert_focused!(world, 1);
        })
        .run(commands);
}

/// A space notification for a window whose AX element is gone is a real close,
/// not a space change. It must be recognised on its own, without waiting for
/// the periodic unordered-window sweep, even though `role()` is cached.
#[test]
fn test_space_notification_for_dead_window_removes_it() {
    let mut harness = TestHarness::new().with_windows(3);
    harness.advance(Duration::from_millis(100));

    harness.mock_state.os_vanish_window(1);
    harness
        .app
        .world_mut()
        .write_message::<Event>(Event::WindowDestroyed {
            window_id: 1,
            source: DestroySource::SpaceNotification,
        });
    // Well short of the 1-second CLOSED_WINDOW_CHECK_FREQ sweep, so only the
    // destroy event can remove the window.
    harness.advance(Duration::from_millis(60));

    assert!(
        !window_exists(harness.app.world_mut(), 1),
        "a space notification for a dead window must remove it"
    );
}

/// Closing a floating window despawns it, which removes its `Unmanaged` marker.
/// That removal must not put the dying window back into the layout strip.
#[test]
fn test_closing_floating_window_does_not_reinsert_it_into_the_strip() {
    let commands = vec![
        Event::Command {
            command: Command::PrintState,
        },
        Event::Command {
            command: Command::PrintState,
        },
    ];

    let mut params = WindowParams::new("^Window 1$", None);
    params.floating = Some(true);
    let config: Config = (MainOptions::default(), vec![params]).into();

    TestHarness::new()
        .with_config(config)
        .with_windows(3)
        .on_iteration(0, |_world, state| state.os_close_window(1))
        .on_iteration(1, |world, _state| {
            assert!(!window_exists(world, 1), "closed window must be despawned");
            let mut strips = world.query::<&LayoutStrip>();
            let entities = strips
                .iter(world)
                .flat_map(LayoutStrip::all_windows)
                .collect::<Vec<_>>();
            for entity in entities {
                assert!(
                    world.get::<crate::manager::Window>(entity).is_some(),
                    "strip holds {entity}, which is no longer a window"
                );
            }
        })
        .run(commands);
}

/// Floats window `id` with the `float` event after a warm-up step, minimizes
/// it, restores it, and checks that it floats outside the strip again.
fn assert_floats_after_minimize_and_restore(harness: TestHarness, float: Event, id: i32) {
    let print_state = || Event::Command {
        command: Command::PrintState,
    };
    let commands = vec![
        print_state(),
        float,
        print_state(),
        print_state(),
        print_state(),
    ];

    let in_strip = move |world: &mut World| {
        let entity = find_window_entity(id, world);
        let mut strips = world.query::<&LayoutStrip>();
        strips.iter(world).any(|strip| strip.contains(entity))
    };
    let floating = move |world: &mut World| {
        let entity = find_window_entity(id, world);
        matches!(
            world.entity(entity).get::<crate::ecs::Unmanaged>(),
            Some(crate::ecs::Unmanaged::Floating)
        )
    };

    harness
        .on_iteration(2, move |world, state| {
            assert!(floating(world) && !in_strip(world), "window {id} floats");
            state.os_minimize_window(id, true);
        })
        .on_iteration(3, move |world, state| {
            let entity = find_window_entity(id, world);
            assert!(matches!(
                world.entity(entity).get::<crate::ecs::Unmanaged>(),
                Some(crate::ecs::Unmanaged::Minimized)
            ));
            state.os_minimize_window(id, false);
        })
        .on_iteration(4, move |world, _state| {
            assert!(
                !in_strip(world),
                "the restored window must not take a column"
            );
            assert!(floating(world), "the restored window must float");
        })
        .run(commands);
}

/// A window a rule floats stays floating after it is minimized and restored.
#[test]
fn test_rule_floated_window_stays_floating_after_minimize_and_restore() {
    let mut params = WindowParams::new("^Window 1$", None);
    params.floating = Some(true);
    let config: Config = (MainOptions::default(), vec![params]).into();
    let harness = TestHarness::new().with_config(config).with_windows(2);
    let print_state = Event::Command {
        command: Command::PrintState,
    };
    assert_floats_after_minimize_and_restore(harness, print_state, 1);
}

/// A window the user floated with `window_manage` stays floating after it is
/// minimized and restored.
#[test]
fn test_user_floated_window_stays_floating_after_minimize_and_restore() {
    let float = Event::Command {
        command: Command::Window(Operation::Manage),
    };
    assert_floats_after_minimize_and_restore(TestHarness::new().with_windows(2), float, 0);
}

/// A tiled window that is minimized and restored takes a column again.
#[test]
fn test_tiled_window_is_tiled_again_after_minimize_and_restore() {
    let print_state = || Event::Command {
        command: Command::PrintState,
    };
    let in_strip = |world: &mut World| {
        let entity = find_window_entity(1, world);
        let mut strips = world.query::<&LayoutStrip>();
        strips.iter(world).any(|strip| strip.contains(entity))
    };

    TestHarness::new()
        .with_windows(2)
        .on_iteration(1, move |world, state| {
            assert!(in_strip(world));
            state.os_minimize_window(1, true);
        })
        .on_iteration(2, move |world, state| {
            assert!(!in_strip(world), "a minimized window leaves the strip");
            state.os_minimize_window(1, false);
        })
        .on_iteration(3, move |world, _state| {
            assert!(in_strip(world), "the restored window takes a column again");
            let entity = find_window_entity(1, world);
            assert!(
                world
                    .entity(entity)
                    .get::<crate::ecs::Unmanaged>()
                    .is_none()
            );
        })
        .run(vec![
            print_state(),
            print_state(),
            print_state(),
            print_state(),
        ]);
}

/// A minimized floating window that another path, such as Lua or session
/// restore, tiles must not keep the marker that floats it on restore.
#[test]
fn test_tiling_a_minimized_floating_window_clears_floats_when_restored() {
    let print_state = || Event::Command {
        command: Command::PrintState,
    };
    let float = Event::Command {
        command: Command::Window(Operation::Manage),
    };

    TestHarness::new()
        .with_windows(2)
        .on_iteration(2, |_world, state| {
            state.os_minimize_window(0, true);
        })
        .on_iteration(3, |world, _state| {
            let entity = find_window_entity(0, world);
            assert!(
                world
                    .entity(entity)
                    .contains::<crate::ecs::FloatsWhenRestored>(),
                "minimizing a floating window marks it to float on restore"
            );
            world.entity_mut(entity).remove::<crate::ecs::Unmanaged>();
        })
        .on_iteration(4, |world, _state| {
            let entity = find_window_entity(0, world);
            assert!(
                !world
                    .entity(entity)
                    .contains::<crate::ecs::FloatsWhenRestored>(),
                "a tiled window must not keep FloatsWhenRestored"
            );
        })
        .run(vec![
            print_state(),
            float,
            print_state(),
            print_state(),
            print_state(),
        ]);
}

// 2026-10-26: A captured Emacs popup disappeared just before focus jumped to
// the other half-screen tile. Closing an unfocused popup must preserve focus.
#[test]
fn test_closing_unfocused_popup_preserves_editor_focus() {
    let mut tiled = WindowParams::new(".*", None);
    tiled.width = Some(InitialWindowWidth::Ratio(0.5));
    let mut popup = WindowParams::new("^Window 2$", None);
    popup.floating = Some(true);
    popup.dont_focus = Some(true);
    let config: Config = (MainOptions::default(), vec![tiled, popup]).into();

    TestHarness::new()
        .with_config(config)
        .with_windows(3)
        .on_iteration(0, |_world, state| state.focus_window(1))
        .on_iteration(1, |world, state| {
            assert_focused!(world, 1);
            state.os_close_window(2);
        })
        .on_iteration(2, |world, _state| {
            assert!(!window_exists(world, 2), "closed popup must be removed");
            assert_focused!(world, 1);
        })
        .run(vec![
            Event::Command {
                command: Command::PrintState,
            },
            Event::Command {
                command: Command::PrintState,
            },
            Event::Command {
                command: Command::PrintState,
            },
        ]);
}

fn assert_shown_whole(world: &mut World, id: i32) {
    let mut query = world.query::<&crate::manager::Window>();
    let frame = query
        .iter(world)
        .find(|window| window.id() == id)
        .unwrap_or_else(|| panic!("window {id} not found"))
        .frame();
    assert!(
        frame.min.x >= 0 && frame.max.x <= TEST_DISPLAY_WIDTH,
        "window {id} must be shown whole on the display, got {frame:?}"
    );
}

/// Windows left of the focused one close while it keeps focus. Each close
/// pulls the columns after it left while the strip's origin stays put, so the
/// focused window has to be scrolled back into view.
#[test]
fn test_focused_survivor_stays_shown_when_windows_left_of_it_close() {
    let commands = vec![
        Event::MenuOpened { window_id: 0 },
        Event::Command {
            command: Command::Window(Operation::Focus(Direction::Last)),
        },
        Event::Command {
            command: Command::PrintState,
        },
        Event::Command {
            command: Command::PrintState,
        },
        Event::Command {
            command: Command::PrintState,
        },
    ];

    TestHarness::new()
        .with_windows(4)
        .on_iteration(1, |world, state| {
            assert_focused!(world, 3);
            assert_shown_whole(world, 3);
            state.os_close_window(0);
        })
        .on_iteration(2, |world, state| {
            assert!(!window_exists(world, 0));
            assert_shown_whole(world, 3);
            state.os_close_window(1);
        })
        .on_iteration(3, |world, state| {
            assert!(!window_exists(world, 1));
            assert_shown_whole(world, 3);
            state.os_close_window(2);
        })
        .on_iteration(4, |world, _state| {
            assert!(!window_exists(world, 2));
            assert_focused!(world, 3);
            assert_shown_whole(world, 3);
        })
        .run(commands);
}

/// Closing a column left of focus shifts the strip origin right so the focused
/// window stays at the same screen x while off-screen left columns slide in;
/// once nothing remains left to fill the gap in standard edge-invariant mode,
/// the origin clamps at the left edge so no blank gap opens.
#[test]
fn test_closing_window_left_of_focus_keeps_focused_window_stationary() {
    let mut half = WindowParams::new(".*", None);
    half.width = Some(InitialWindowWidth::Ratio(0.5));
    let config: Config = (MainOptions::default(), vec![half]).into();

    let commands = vec![
        Event::MenuOpened { window_id: 0 },
        Event::Command {
            command: Command::Window(Operation::Focus(Direction::Nth(2))),
        }, // 1: focus window 2 (1 at 0..512, 2 at 512..1024, 0 off-screen left)
        Event::Command {
            command: Command::PrintState,
        }, // 2: after closing window 1
        Event::Command {
            command: Command::PrintState,
        }, // 3: after closing leftmost window 0
    ];

    TestHarness::new()
        .with_config(config)
        .with_windows(4)
        .on_iteration(1, |world, state| {
            assert_focused!(world, 2);
            assert_eq!(window_x(world, 1), 0);
            assert_eq!(window_x(world, 2), TEST_DISPLAY_WIDTH / 2);
            state.os_close_window(1);
        })
        .on_iteration(2, |world, state| {
            assert!(!window_exists(world, 1));
            assert_focused!(world, 2);
            assert_eq!(
                window_x(world, 2),
                TEST_DISPLAY_WIDTH / 2,
                "focused window must stay stationary while off-screen left column fills the gap"
            );
            assert_eq!(
                window_x(world, 0),
                0,
                "off-screen column left of the closed window must slide in to fill the gap"
            );
            state.os_close_window(0);
        })
        .on_iteration(3, |world, _state| {
            assert!(!window_exists(world, 0));
            assert_focused!(world, 2);
            assert_eq!(
                window_x(world, 2),
                0,
                "closing the leftmost column in edge-invariant mode must clamp at the left edge"
            );
        })
        .run(commands);
}

/// With `auto_center = true`, closing the column to the left of a centered
/// focused window must not clamp the strip origin to `viewport.min.x` and pull
/// the focused window off-center.
#[test]
fn test_closing_window_left_of_focus_preserves_auto_center() {
    let mut half = WindowParams::new(".*", None);
    half.width = Some(InitialWindowWidth::Ratio(0.5));
    let options = MainOptions {
        auto_center: Some(true),
        ..MainOptions::default()
    };
    let config: Config = (options, vec![half]).into();
    let centered_x = (TEST_DISPLAY_WIDTH - TEST_DISPLAY_WIDTH / 2) / 2;

    let commands = vec![
        Event::MenuOpened { window_id: 0 },
        Event::Command {
            command: Command::Window(Operation::Focus(Direction::Last)),
        }, // 1: focus window 1 (centered)
        Event::Command {
            command: Command::PrintState,
        }, // 2: after closing window 0
    ];

    TestHarness::new()
        .with_config(config)
        .with_windows(2)
        .on_iteration(1, move |world, state| {
            assert_focused!(world, 1);
            assert_eq!(window_x(world, 1), centered_x);
            state.os_close_window(0);
        })
        .on_iteration(2, move |world, _state| {
            assert!(!window_exists(world, 0));
            assert_focused!(world, 1);
            assert_eq!(
                window_x(world, 1),
                centered_x,
                "auto_center focused window must remain centered when its left neighbour closes"
            );
        })
        .run(commands);
}

/// When the user explicitly centered the focused window (`ManualStripOffset`),
/// closing the column to its left must leave it centered instead of clamping to
/// the left viewport edge.
#[test]
fn test_closing_window_left_of_focus_preserves_manual_center() {
    let mut half = WindowParams::new(".*", None);
    half.width = Some(InitialWindowWidth::Ratio(0.5));
    let config: Config = (MainOptions::default(), vec![half]).into();
    let centered_x = (TEST_DISPLAY_WIDTH - TEST_DISPLAY_WIDTH / 2) / 2;

    let commands = vec![
        Event::MenuOpened { window_id: 0 },
        Event::Command {
            command: Command::Window(Operation::Focus(Direction::Last)),
        }, // 1: focus window 1
        Event::Command {
            command: Command::Window(Operation::Center),
        }, // 2: manually center window 1
        Event::Command {
            command: Command::PrintState,
        }, // 3: after closing window 0
    ];

    TestHarness::new()
        .with_config(config)
        .with_windows(2)
        .on_iteration(2, move |world, state| {
            assert_focused!(world, 1);
            assert_eq!(window_x(world, 1), centered_x);
            state.os_close_window(0);
        })
        .on_iteration(3, move |world, _state| {
            assert!(!window_exists(world, 0));
            assert_focused!(world, 1);
            assert_eq!(
                window_x(world, 1),
                centered_x,
                "manually centered window must stay centered when its left neighbour closes"
            );
        })
        .run(commands);
}

/// Closing one member of a multi-window stack left of focus leaves the column
/// intact, so the strip origin must not shift rightward.
#[test]
fn test_closing_stack_member_left_of_focus_does_not_shift_strip() {
    let mut half = WindowParams::new(".*", None);
    half.width = Some(InitialWindowWidth::Ratio(0.5));
    let config: Config = (MainOptions::default(), vec![half]).into();

    let commands = vec![
        Event::MenuOpened { window_id: 0 },
        Event::Command {
            command: Command::Window(Operation::Focus(Direction::Nth(2))),
        }, // 1: focus window 2
        Event::Command {
            command: Command::Window(Operation::Stack(true)),
        }, // 2: stack window 2 onto window 1 -> [0, Stack(1, 2), 3]
        Event::Command {
            command: Command::Window(Operation::Focus(Direction::Last)),
        }, // 3: focus window 3 -> Stack(1, 2) at 0..512, 3 at 512..1024, 0 off-screen left
        Event::Command {
            command: Command::PrintState,
        }, // 4: after closing window 2 (stack still survives with window 1)
    ];

    TestHarness::new()
        .with_config(config)
        .with_windows(4)
        .on_iteration(3, |world, state| {
            assert_focused!(world, 3);
            assert_eq!(window_x(world, 1), 0);
            assert_eq!(window_x(world, 3), TEST_DISPLAY_WIDTH / 2);
            state.os_close_window(2);
        })
        .on_iteration(4, |world, _state| {
            assert!(!window_exists(world, 2));
            assert_focused!(world, 3);
            assert_eq!(
                window_x(world, 1),
                0,
                "surviving stack member must stay in place when its sibling closes"
            );
            assert_eq!(
                window_x(world, 3),
                TEST_DISPLAY_WIDTH / 2,
                "focused window must not shift when a stack left of it survives"
            );
        })
        .run(commands);
}

/// Floating, minimizing, or moving an unfocused column left of focus to another
/// virtual workspace must all keep the focused window at the same screen x.
#[test]
fn test_float_minimize_and_virtual_move_left_of_focus_keep_focused_stationary() {
    use crate::types::windowset::LayoutOp;

    let mut half = WindowParams::new(".*", None);
    half.width = Some(InitialWindowWidth::Ratio(0.5));
    let config: Config = (MainOptions::default(), vec![half]).into();

    let commands = vec![
        Event::MenuOpened { window_id: 0 },
        Event::Command {
            command: Command::Window(Operation::Focus(Direction::Last)),
        }, // 1: focus window 4 (3 at 0..512, 4 at 512..1024, 0..=2 off-screen left)
        Event::Command {
            command: Command::Layout(vec![LayoutOp::SetFloating {
                window: 3,
                floating: true,
            }]),
        }, // 2: float unfocused window 3
        Event::Command {
            command: Command::PrintState,
        }, // 3: after minimizing unfocused window 2
        Event::Command {
            command: Command::Layout(vec![LayoutOp::MoveToWorkspace {
                window: 1,
                workspace: 2,
                follow: false,
            }]),
        }, // 4: shift unfocused window 1 to virtual workspace 2 without following
    ];

    TestHarness::new()
        .with_config(config)
        .with_windows(5)
        .on_iteration(1, |world, _state| {
            assert_focused!(world, 4);
            assert_eq!(window_x(world, 3), 0);
            assert_eq!(window_x(world, 4), TEST_DISPLAY_WIDTH / 2);
        })
        .on_iteration(2, |world, state| {
            assert_focused!(world, 4);
            assert_eq!(
                window_x(world, 4),
                TEST_DISPLAY_WIDTH / 2,
                "floating a column left of focus must keep focused window stationary"
            );
            assert_eq!(window_x(world, 2), 0);
            state.os_minimize_window(2, true);
        })
        .on_iteration(3, |world, _state| {
            assert_focused!(world, 4);
            assert_eq!(
                window_x(world, 4),
                TEST_DISPLAY_WIDTH / 2,
                "minimizing a column left of focus must keep focused window stationary"
            );
            assert_eq!(window_x(world, 1), 0);
        })
        .on_iteration(4, |world, _state| {
            assert_focused!(world, 4);
            assert_eq!(
                window_x(world, 4),
                TEST_DISPLAY_WIDTH / 2,
                "shifting an unfocused column left of focus to another workspace must keep focused window stationary"
            );
            assert_eq!(window_x(world, 0), 0);
        })
        .run(commands);
}

/// If the user scrolled away from the focused window so it is currently off-screen,
/// closing another window must not yank the viewport back to the focused window.
#[test]
fn test_closing_window_while_scrolled_away_does_not_yank_viewport_to_focused() {
    use crate::ecs::Position;

    let mut half = WindowParams::new(".*", None);
    half.width = Some(InitialWindowWidth::Ratio(0.5));
    let config: Config = (MainOptions::default(), vec![half]).into();

    let commands = vec![
        Event::MenuOpened { window_id: 0 },
        Event::Command {
            command: Command::Window(Operation::Focus(Direction::Last)),
        }, // 1: focus window 3
        Event::Command {
            command: Command::PrintState,
        }, // 2: settle after scrolling strip back to x=0
        Event::Command {
            command: Command::PrintState,
        }, // 3: after closing window 1 while scrolled at x=0
    ];

    TestHarness::new()
        .with_config(config)
        .with_windows(4)
        .on_iteration(1, |world, _state| {
            assert_focused!(world, 3);
            let mut query = world
                .query_filtered::<&mut Position, (With<LayoutStrip>, With<ActiveWorkspaceMarker>)>(
                );
            let mut strip_pos = query.single_mut(world).expect("active strip position");
            strip_pos.0.x = 0;
        })
        .on_iteration(2, |world, state| {
            assert_focused!(world, 3);
            assert_eq!(window_x(world, 0), 0);
            assert_eq!(window_x(world, 1), TEST_DISPLAY_WIDTH / 2);
            state.os_close_window(1);
        })
        .on_iteration(3, |world, _state| {
            assert!(!window_exists(world, 1));
            assert_focused!(world, 3);
            assert_eq!(
                window_x(world, 0),
                0,
                "closing a window while scrolled away must not yank the viewport back to focused"
            );
        })
        .run(commands);
}

/// On a secondary display whose viewport origin is non-zero, clamping after
/// closing the leftmost column must clamp to that display's `min.x`, not `0`.
#[test]
fn test_closing_window_left_of_focus_clamps_to_secondary_display_origin() {
    const EXT_DISPLAY_ID: u32 = 2;
    const EXT_WORKSPACE_ID: u64 = 200;
    const EXT_MIN_X: i32 = 1024;
    const EXT_WIDTH: i32 = 1024;

    let mut half = WindowParams::new(".*", None);
    half.width = Some(InitialWindowWidth::Ratio(0.5));
    let config: Config = (MainOptions::default(), vec![half]).into();

    let commands = vec![
        Event::MenuOpened { window_id: 0 },
        Event::Command {
            command: Command::PrintState,
        }, // 1: after focusing window 201 on secondary display
        Event::Command {
            command: Command::PrintState,
        }, // 2: after closing window 200 on secondary display
    ];

    let ext_bounds = IRect::new(EXT_MIN_X, 0, EXT_MIN_X + EXT_WIDTH, TEST_DISPLAY_HEIGHT);
    TestHarness::new()
        .with_config(config)
        .with_display(EXT_DISPLAY_ID, ext_bounds, vec![EXT_WORKSPACE_ID])
        .with_windows(1)
        .with_workspace_window(200, EXT_WORKSPACE_ID, |w| {
            w.frame = IRect::new(EXT_MIN_X, 0, EXT_MIN_X + EXT_WIDTH / 2, TEST_DISPLAY_HEIGHT);
        })
        .with_workspace_window(201, EXT_WORKSPACE_ID, |w| {
            w.frame = IRect::new(
                EXT_MIN_X + EXT_WIDTH / 2,
                0,
                EXT_MIN_X + EXT_WIDTH,
                TEST_DISPLAY_HEIGHT,
            );
        })
        .on_iteration(0, |_world, state| {
            state.focus_window(201);
        })
        .on_iteration(1, |world, state| {
            assert_focused!(world, 201);
            assert_eq!(window_x(world, 200), EXT_MIN_X);
            assert_eq!(window_x(world, 201), EXT_MIN_X + EXT_WIDTH / 2);
            state.os_close_window(200);
        })
        .on_iteration(2, |world, _state| {
            assert!(!window_exists(world, 200));
            assert_focused!(world, 201);
            assert_eq!(
                window_x(world, 201),
                EXT_MIN_X,
                "closing leftmost column on secondary display must clamp to its viewport min.x"
            );
        })
        .run(commands);
}

fn window_x(world: &mut World, id: i32) -> i32 {
    let mut query = world.query::<&crate::manager::Window>();
    query
        .iter(world)
        .find(|window| window.id() == id)
        .unwrap_or_else(|| panic!("window {id} not found"))
        .frame()
        .min
        .x
}

fn strip_column(world: &mut World, id: i32) -> Option<usize> {
    let entity = world
        .query::<(Entity, &crate::manager::Window)>()
        .iter(world)
        .find_map(|(entity, window)| (window.id() == id).then_some(entity))?;
    world
        .query_filtered::<&LayoutStrip, With<ActiveWorkspaceMarker>>()
        .iter(world)
        .find_map(|strip| strip.index_of(entity).ok())
}

fn padding_set(state: &MockState, id: i32) -> (Option<i32>, Option<i32>) {
    let mut set = (None, None);
    state.update_window(id, |window| set = window.padding_set);
    set
}

/// `apply_window_defaults` pads only a window that opens tiled, so a window
/// that opens floating has to get its padding when it is tiled.
#[test]
fn test_window_tiled_from_floating_gets_its_padding() {
    let mut floating = WindowParams::new("^Window 1$", Some("test".to_string()));
    floating.floating = Some(true);
    let mut padded = WindowParams::new(".*", None);
    padded.horizontal_padding = Some(4);
    padded.vertical_padding = Some(4);
    let config: Config = (MainOptions::default(), vec![floating, padded]).into();

    let commands = vec![
        Event::Command {
            command: Command::PrintState,
        }, // 0
        Event::Command {
            command: Command::PrintState,
        }, // 1
        Event::Command {
            command: Command::Window(Operation::Manage),
        }, // 2: tile window 1
    ];

    TestHarness::new()
        .with_config(config)
        .with_windows(2)
        .on_iteration(0, |_world, state| state.focus_window(1))
        .on_iteration(1, |world, state| {
            assert_focused!(world, 1);
            let entity = find_window_entity(1, world);
            assert!(world.entity(entity).contains::<crate::ecs::Unmanaged>());
            assert_eq!(padding_set(&state, 0), (Some(4), Some(4)));
            assert_eq!(padding_set(&state, 1), (None, None));
        })
        .on_iteration(2, |world, state| {
            let entity = find_window_entity(1, world);
            assert!(!world.entity(entity).contains::<crate::ecs::Unmanaged>());
            assert_eq!(
                padding_set(&state, 1),
                (Some(4), Some(4)),
                "a window tiled from floating must get its padding"
            );
        })
        .run(commands);
}

fn geometry_changed_since(
    harness: &mut TestHarness,
    id: i32,
    before: bevy::ecs::change_detection::Tick,
    after: bevy::ecs::change_detection::Tick,
) -> (bool, bool) {
    use crate::ecs::{Bounds, Position};
    use bevy::ecs::change_detection::DetectChanges as _;

    let entity = find_window_entity(id, harness.world());
    let entity_ref = harness.app.world().entity(entity);
    let pos = entity_ref
        .get_ref::<Position>()
        .unwrap()
        .last_changed()
        .is_newer_than(before, after);
    let bounds = entity_ref
        .get_ref::<Bounds>()
        .unwrap()
        .last_changed()
        .is_newer_than(before, after);
    (pos, bounds)
}

/// Reloading config with new per-window `horizontal_padding` / `vertical_padding`
/// updates already-open tiled windows and marks their geometry dirty so the new
/// AX inset is committed immediately, while leaving floating and minimized
/// windows untouched.
#[test]
fn test_config_reload_updates_padding_on_open_tiled_windows() {
    fn config_with_padding(h: i32, v: i32) -> Config {
        let mut floating = WindowParams::new("^Window 2$", Some("test".to_string()));
        floating.floating = Some(true);
        let mut padded = WindowParams::new(".*", None);
        padded.horizontal_padding = Some(h);
        padded.vertical_padding = Some(v);
        (MainOptions::default(), vec![floating, padded]).into()
    }

    let mut harness = TestHarness::new()
        .with_config(config_with_padding(4, 4))
        .with_windows(4);
    harness.run(vec![Event::Command {
        command: Command::PrintState,
    }]);

    // Minimize window 3 so we cover tiled (0, 1), floating (2), and minimized (3).
    harness.mock_state.os_minimize_window(3, true);
    harness.advance(std::time::Duration::from_millis(100));

    // Sync mock readback fields to the initial padding so change detection reflects reality.
    for id in [0, 1] {
        harness.mock_state.update_window(id, |w| {
            w.horizontal_padding = 4;
            w.vertical_padding = 4;
        });
    }
    assert_eq!(padding_set(&harness.mock_state, 0), (Some(4), Some(4)));
    assert_eq!(padding_set(&harness.mock_state, 1), (Some(4), Some(4)));
    assert_eq!(padding_set(&harness.mock_state, 2), (None, None));
    assert_eq!(padding_set(&harness.mock_state, 3), (Some(4), Some(4)));

    // 1. Reload with larger padding (4 -> 12): tiled windows update and mark Position/Bounds changed.
    harness
        .app
        .world_mut()
        .insert_resource(config_with_padding(12, 12));
    let before_tick = harness.app.world().read_change_tick();
    harness.app.update();
    let after_tick = harness.app.world().read_change_tick();

    assert_eq!(padding_set(&harness.mock_state, 0), (Some(12), Some(12)));
    assert_eq!(padding_set(&harness.mock_state, 1), (Some(12), Some(12)));
    assert_eq!(
        padding_set(&harness.mock_state, 2),
        (None, None),
        "floating window must stay unpadded on config reload"
    );
    assert_eq!(
        padding_set(&harness.mock_state, 3),
        (Some(4), Some(4)),
        "minimized window must not be touched while minimized"
    );
    for id in [0, 1] {
        assert_eq!(
            geometry_changed_since(&mut harness, id, before_tick, after_tick),
            (true, true),
            "window {id} must mark Position and Bounds changed so AX commits the new inset"
        );
    }

    // 2. Reload with unchanged padding: Position/Bounds must not be spuriously marked changed.
    for id in [0, 1] {
        harness.mock_state.update_window(id, |w| {
            w.horizontal_padding = 12;
            w.vertical_padding = 12;
        });
    }
    harness.app.update();
    harness
        .app
        .world_mut()
        .insert_resource(config_with_padding(12, 12));
    let before_noop = harness.app.world().read_change_tick();
    harness.app.update();
    let after_noop = harness.app.world().read_change_tick();
    for id in [0, 1] {
        assert_eq!(
            geometry_changed_since(&mut harness, id, before_noop, after_noop),
            (false, false),
            "unchanged padding must not mark Position/Bounds changed on window {id}"
        );
    }

    // 3. Removing the padding rule resets existing tiled windows back to 0.
    let no_padding: Config = (MainOptions::default(), vec![]).into();
    harness.app.world_mut().insert_resource(no_padding);
    harness.app.update();
    assert_eq!(padding_set(&harness.mock_state, 0), (Some(0), Some(0)));
    assert_eq!(padding_set(&harness.mock_state, 1), (Some(0), Some(0)));
}

/// A native fullscreen window updates its stored padding on config reload so it
/// has the right inset if it leaves fullscreen later, without marking `Position`
/// or `Bounds` changed while fullscreen.
#[test]
fn test_config_reload_updates_fullscreen_padding_without_moving_it() {
    let mut initial = WindowParams::new(".*", None);
    initial.horizontal_padding = Some(4);
    initial.vertical_padding = Some(4);
    let mut updated = WindowParams::new(".*", None);
    updated.horizontal_padding = Some(12);
    updated.vertical_padding = Some(12);

    let mut harness = TestHarness::new()
        .with_config((MainOptions::default(), vec![initial]).into())
        .with_window(0, |w| {
            w.is_full_screen = true;
            w.horizontal_padding = 4;
            w.vertical_padding = 4;
        });
    harness.run(vec![Event::Command {
        command: Command::PrintState,
    }]);

    harness
        .app
        .world_mut()
        .insert_resource::<Config>((MainOptions::default(), vec![updated]).into());
    let before_tick = harness.app.world().read_change_tick();
    harness.app.update();
    let after_tick = harness.app.world().read_change_tick();

    assert_eq!(
        padding_set(&harness.mock_state, 0),
        (Some(12), Some(12)),
        "stored padding must still update for fullscreen windows"
    );
    assert_eq!(
        geometry_changed_since(&mut harness, 0, before_tick, after_tick),
        (false, false),
        "fullscreen window must not mark Position/Bounds changed on padding reload"
    );
}

fn window_exists(world: &mut World, id: i32) -> bool {
    let mut query = world.query::<&crate::manager::Window>();
    query.iter(world).any(|window| window.id() == id)
}

/// A `subrole` window rule applies its effects only to windows reporting that
/// subrole: an app's dialog floats while its standard window stays tiled.
#[test]
fn test_subrole_window_rule_floats_only_matching_windows() {
    let config = Config::try_from(
        r#"
[options]

[bindings]

[windows.dialogs]
title = ".*"
subrole = "AXDialog"
floating = true
"#,
    )
    .expect("config should parse");

    let commands = vec![Event::Command {
        command: Command::PrintState,
    }];

    TestHarness::new()
        .with_config(config)
        .with_windows(1)
        .with_window(1, |window| window.subrole = "AXDialog".to_string())
        // Iterations are 0-indexed: this is the only command's iteration.
        .on_iteration(0, |world, _state| {
            let is_floating = |world: &mut World, id| {
                let entity = find_window_entity(id, world);
                matches!(
                    world.entity(entity).get::<crate::ecs::Unmanaged>(),
                    Some(crate::ecs::Unmanaged::Floating)
                )
            };
            assert!(!is_floating(world, 0), "standard window must stay tiled");
            assert!(is_floating(world, 1), "dialog must float");
        })
        .run(commands);
}

fn spawn_fixed_size_window(world: &mut World, state: &MockState, frame: IRect) {
    let window = state.spawn_window(TEST_PROCESS_ID, TEST_WORKSPACE_ID, 1, frame);
    state.update_window(1, |window| window.fixed_size = true);
    world.trigger(SpawnWindowTrigger(vec![window]));
}

fn in_any_strip(world: &mut World, id: i32) -> bool {
    let entity = find_window_entity(id, world);
    let mut strips = world.query::<&LayoutStrip>();
    strips.iter(world).any(|strip| strip.contains(entity))
}

/// A new window whose size macOS reports as not settable, such as a fixed-size
/// dialog, floats at its own frame instead of being stretched into a column.
#[test]
fn test_new_non_resizable_window_floats() {
    let frame = IRect::new(100, 100, 500, 400);
    TestHarness::new()
        .with_windows(1)
        .with_focused_window(0)
        .on_iteration(0, move |world, state| {
            spawn_fixed_size_window(world, &state, frame);
        })
        .on_iteration(1, move |world, _state| {
            assert!(
                !in_any_strip(world, 1),
                "a non-resizable window must not take a column"
            );
            let entity = find_window_entity(1, world);
            assert!(
                matches!(
                    world.entity(entity).get::<crate::ecs::Unmanaged>(),
                    Some(crate::ecs::Unmanaged::Floating)
                ),
                "a non-resizable window must float"
            );
            assert_window_at!(world, 1, frame.min.x, frame.min.y);
            assert_window_size!(world, 1, frame.width(), frame.height());
            assert!(in_any_strip(world, 0), "the resizable window stays tiled");
        })
        .run(vec![
            Event::MenuOpened { window_id: 0 },
            Event::Command {
                command: Command::PrintState,
            },
        ]);
}

/// A rule with `floating = false` tiles a non-resizable window.
#[test]
fn test_rule_tiles_a_non_resizable_window() {
    let mut params = WindowParams::new("^Window 1$", None);
    params.floating = Some(false);
    let config: Config = (MainOptions::default(), vec![params]).into();

    TestHarness::new()
        .with_config(config)
        .with_windows(1)
        .with_focused_window(0)
        .on_iteration(0, |world, state| {
            spawn_fixed_size_window(world, &state, IRect::new(100, 100, 500, 400));
        })
        .on_iteration(1, |world, _state| {
            let entity = find_window_entity(1, world);
            assert!(
                world
                    .entity(entity)
                    .get::<crate::ecs::Unmanaged>()
                    .is_none()
            );
            assert!(in_any_strip(world, 1), "the rule keeps it tiled");
        })
        .run(vec![
            Event::MenuOpened { window_id: 0 },
            Event::Command {
                command: Command::PrintState,
            },
        ]);
}

/// Native fullscreen temporarily makes `AXSize` non-settable on macOS;
/// discovering a fullscreen window must not classify it as `Unmanaged::Floating`.
#[test]
fn test_native_fullscreen_window_reporting_fixed_size_does_not_float() {
    TestHarness::new()
        .with_windows(1)
        .with_focused_window(0)
        .on_iteration(0, |world, state| {
            let window = state.spawn_window(
                TEST_PROCESS_ID,
                TEST_WORKSPACE_ID,
                1,
                IRect::new(0, 0, TEST_DISPLAY_WIDTH, TEST_DISPLAY_HEIGHT),
            );
            state.update_window(1, |window| {
                window.fixed_size = true;
                window.is_full_screen = true;
            });
            world.trigger(SpawnWindowTrigger(vec![window]));
        })
        .on_iteration(1, |world, _state| {
            let entity = find_window_entity(1, world);
            assert!(
                world
                    .entity(entity)
                    .get::<crate::ecs::Unmanaged>()
                    .is_none(),
                "a native fullscreen window must not be floated when AXSize is unsettable"
            );
        })
        .run(vec![
            Event::MenuOpened { window_id: 0 },
            Event::Command {
                command: Command::PrintState,
            },
        ]);
}

/// Regression test: during startup (`Initializing`), `apply_window_defaults` and
/// `apply_window_positions` must classify floating windows as `Unmanaged::Floating`
/// *before* `finish_setup` populates the workspace strip and selects the initial
/// focused window. Otherwise, a floating window that appears first in the
/// workspace window list is treated as managed by `finish_setup` and steals
/// `FocusedMarker` from the first tiled window, or `finish_setup` removes
/// `Initializing` before `apply_window_positions` runs and causes startup
/// windows to be treated as runtime spawns.
#[test]
fn test_startup_floating_window_does_not_steal_initial_focus_or_enter_strip() {
    let mut params = WindowParams::new("^Window 0$", None);
    params.floating = Some(true);
    let config: Config = (MainOptions::default(), vec![params]).into();

    let commands = vec![Event::Command {
        command: Command::PrintState,
    }];

    TestHarness::new()
        .with_config(config)
        .with_windows(3)
        .on_iteration(0, |world, _state| {
            let win0 = find_window_entity(0, world);
            let win1 = find_window_entity(1, world);
            let win2 = find_window_entity(2, world);

            assert!(
                matches!(
                    world.entity(win0).get::<crate::ecs::Unmanaged>(),
                    Some(crate::ecs::Unmanaged::Floating)
                ),
                "window 0 must be marked Unmanaged::Floating during startup"
            );
            assert!(
                world
                    .entity(win0)
                    .get::<crate::ecs::FocusedMarker>()
                    .is_none(),
                "floating window 0 must not receive initial FocusedMarker"
            );
            assert!(
                world
                    .entity(win1)
                    .get::<crate::ecs::FocusedMarker>()
                    .is_some(),
                "first tiled window (1) must receive initial FocusedMarker"
            );

            let mut strips = world.query::<&LayoutStrip>();
            let strip = strips.single(world).expect("single active strip");
            assert_eq!(
                strip.all_windows(),
                vec![win1, win2],
                "strip must contain only tiled windows in workspace order without holes"
            );
            assert_window_at!(world, 1, 0, TEST_MENUBAR_HEIGHT);
            assert_window_at!(world, 2, TEST_WINDOW_WIDTH, TEST_MENUBAR_HEIGHT);
        })
        .run(commands);
}

/// A new window that opens floating and focused takes focus in Paneru, so
/// `window_manage` acts on it, not on the window focused before it. macOS does
/// not report the focus change here, as when the app was not tracked yet.
#[test]
fn test_new_floating_window_takes_focus() {
    let mut params = WindowParams::new("^Window 1$", None);
    params.floating = Some(true);
    let config: Config = (MainOptions::default(), vec![params]).into();

    let floating = |world: &mut World, id| {
        let entity = find_window_entity(id, world);
        matches!(
            world.entity(entity).get::<crate::ecs::Unmanaged>(),
            Some(crate::ecs::Unmanaged::Floating)
        )
    };

    TestHarness::new()
        .with_config(config)
        .with_windows(1)
        .with_focused_window(0)
        .on_iteration(0, |world, state| {
            let frame = IRect::new(100, 100, 500, 400);
            let window = state.spawn_window(TEST_PROCESS_ID, TEST_WORKSPACE_ID, 1, frame);
            state.set_focused_window(1);
            world.trigger(SpawnWindowTrigger(vec![window]));
        })
        .on_iteration(2, move |world, _state| {
            assert!(floating(world, 1), "the rule floats window 1");
        })
        .on_iteration(4, move |world, _state| {
            assert!(
                !floating(world, 0),
                "window_manage must not float the previously focused window"
            );
            assert!(
                !floating(world, 1),
                "window_manage must tile the new window"
            );
            assert_focused!(world, 1);
        })
        .run(vec![
            Event::MenuOpened { window_id: 0 },
            Event::Command {
                command: Command::PrintState,
            },
            Event::Command {
                command: Command::PrintState,
            },
            Event::Command {
                command: Command::Window(Operation::Manage),
            },
            Event::Command {
                command: Command::PrintState,
            },
        ]);
}

/// A new floating window with a `dont_focus` rule leaves focus where it was.
#[test]
fn test_new_floating_window_with_dont_focus_leaves_focus() {
    let mut params = WindowParams::new("^Window 1$", None);
    params.floating = Some(true);
    params.dont_focus = Some(true);
    let config: Config = (MainOptions::default(), vec![params]).into();

    TestHarness::new()
        .with_config(config)
        .with_windows(1)
        .with_focused_window(0)
        .on_iteration(0, |world, state| {
            let frame = IRect::new(100, 100, 500, 400);
            let window = state.spawn_window(TEST_PROCESS_ID, TEST_WORKSPACE_ID, 1, frame);
            state.set_focused_window(1);
            world.trigger(SpawnWindowTrigger(vec![window]));
        })
        .on_iteration(2, |world, _state| {
            assert_focused!(world, 0);
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
}

/// Regression test: stacking an already-focused window (`Operation::Stack(true)`)
/// when `auto_center = false` and `continuous_swipe = false` must compute the
/// strip offset from the window's new layout slot (`layout_position.0 + strip_target`)
/// and clamp the strip to `display_bounds.min.x` (`0`) when the stacked strip fits
/// inside the viewport, rather than anchoring the strip to the window's pre-stack
/// `moving_frame` X coordinate (`400`).
#[test]
fn test_stacking_focused_window_pins_strip_to_left_edge_when_strip_fits_viewport() {
    let commands = vec![
        Event::MenuOpened { window_id: 0 },
        Event::Command {
            command: Command::Window(Operation::Focus(Direction::Last)),
        },
        Event::Command {
            command: Command::Window(Operation::Stack(true)),
        },
    ];

    TestHarness::new()
        .with_windows(2)
        .on_iteration(1, |world, _state| {
            assert_window_at!(world, 0, 0, TEST_MENUBAR_HEIGHT);
            assert_window_at!(world, 1, TEST_WINDOW_WIDTH, TEST_MENUBAR_HEIGHT);
        })
        .on_iteration(2, |world, _state| {
            let mut query = world
                .query_filtered::<&crate::ecs::Position, With<crate::ecs::ActiveWorkspaceMarker>>();
            let strip_pos = query.single(world).expect("active strip").0;
            assert_eq!(
                strip_pos.x, 0,
                "single-column stacked strip that fits the display must stay pinned at x = 0"
            );
            assert_window_at!(world, 0, 0, TEST_MENUBAR_HEIGHT);
            assert_window_at!(world, 1, 0, 394);
        })
        .run(commands);
}

#[test]
fn test_window_resize_moves_subsequent_windows_in_strip() {
    let commands = vec![
        Event::MenuOpened { window_id: 0 },
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
            continuous_swipe: Some(false),
            ..Default::default()
        },
        vec![],
    )
        .into();

    TestHarness::new()
        .with_config(config)
        .with_windows(3)
        .on_iteration(0, |world, _state| {
            assert_window_at!(world, 0, 0, TEST_MENUBAR_HEIGHT);
            assert_window_at!(world, 1, 400, TEST_MENUBAR_HEIGHT);
            assert_window_at!(world, 2, 800, TEST_MENUBAR_HEIGHT);
        })
        .on_iteration(1, |world, _state| {
            // Window 0 grows from 400 -> 512; windows 1 and 2 must shift right.
            assert_window_size!(world, 0, 512, 748);
            assert_window_at!(world, 0, 0, TEST_MENUBAR_HEIGHT);
            assert_window_at!(world, 1, 512, TEST_MENUBAR_HEIGHT);
            assert_window_at!(world, 2, 912, TEST_MENUBAR_HEIGHT);
        })
        .on_iteration(2, |world, _state| {
            // Window 0 shrinks from 512 -> 256; windows 1 and 2 must shift left.
            assert_window_size!(world, 0, 256, 748);
            assert_window_at!(world, 0, 0, TEST_MENUBAR_HEIGHT);
            assert_window_at!(world, 1, 256, TEST_MENUBAR_HEIGHT);
            assert_window_at!(world, 2, 656, TEST_MENUBAR_HEIGHT);
        })
        .run(commands);
}

#[test]
fn test_window_resize_middle_column_centers_around_window_and_shifts_strip() {
    let commands = vec![
        Event::MenuOpened { window_id: 0 },
        Event::Command {
            command: Command::Window(Operation::Focus(Direction::East)),
        },
        Event::Command {
            command: Command::Window(Operation::Resize(ResizeDirection::Shrink)),
        },
    ];

    let config: Config = (
        MainOptions {
            preset_column_widths: vec![0.25, 0.5, 0.75],
            continuous_swipe: Some(true),
            ..Default::default()
        },
        vec![],
    )
        .into();

    TestHarness::new()
        .with_config(config)
        .with_windows(3)
        .on_iteration(1, |world, _state| {
            assert_window_at!(world, 0, 0, TEST_MENUBAR_HEIGHT);
            assert_window_at!(world, 1, 400, TEST_MENUBAR_HEIGHT);
            assert_window_at!(world, 2, 800, TEST_MENUBAR_HEIGHT);
        })
        .on_iteration(2, |world, _state| {
            // Window 1 shrinks from 400 -> 256 around its center (600):
            // its new screen x is 600 - 128 = 472, so the strip shifts right
            // by +72: window 0 moves to 72, window 1 to 472, and window 2 to 728.
            assert_window_size!(world, 1, 256, 748);
            assert_window_at!(world, 0, 72, TEST_MENUBAR_HEIGHT);
            assert_window_at!(world, 1, 472, TEST_MENUBAR_HEIGHT);
            assert_window_at!(world, 2, 728, TEST_MENUBAR_HEIGHT);
        })
        .run(commands);
}

fn focused_middle_window_with_min_width(min_width: i32) -> TestHarness {
    focused_middle_window_with(min_width, true)
}

fn focused_middle_window_with(min_width: i32, continuous_swipe: bool) -> TestHarness {
    let config: Config = (
        MainOptions {
            preset_column_widths: vec![0.25, 0.5, 0.75],
            continuous_swipe: Some(continuous_swipe),
            ..Default::default()
        },
        vec![],
    )
        .into();
    let mut harness = TestHarness::new()
        .with_config(config)
        .with_window(0, |_| {})
        .with_window(1, |window| window.min_width = min_width)
        .with_window(2, |_| {});
    harness.run(vec![
        Event::MenuOpened { window_id: 0 },
        Event::Command {
            command: Command::Window(Operation::Focus(Direction::East)),
        },
    ]);
    harness
}

fn shrink() -> Event {
    Event::Command {
        command: Command::Window(Operation::Resize(ResizeDirection::Shrink)),
    }
}

#[test]
fn test_window_resize_refused_by_the_app_leaves_the_strip_in_place() {
    let mut harness = focused_middle_window_with_min_width(400);

    for _ in 0..2 {
        harness.run(vec![shrink()]);
        harness.advance(Duration::from_secs(2));
        let world = harness.world();
        assert_window_size!(world, 1, 400, 748);
        assert_window_at!(world, 0, 0, TEST_MENUBAR_HEIGHT);
        assert_window_at!(world, 1, 400, TEST_MENUBAR_HEIGHT);
        assert_window_at!(world, 2, 800, TEST_MENUBAR_HEIGHT);
    }
}

#[test]
fn test_window_resize_refused_with_edge_snapping_leaves_the_strip_in_place() {
    let config: Config = (
        MainOptions {
            preset_column_widths: vec![0.25, 0.5, 0.75],
            continuous_swipe: Some(false),
            ..Default::default()
        },
        vec![],
    )
        .into();
    let mut harness = TestHarness::new()
        .with_config(config)
        .with_window(0, |_| {})
        .with_window(1, |_| {})
        .with_window(2, |window| window.min_width = 400);
    let focus_east = || Event::Command {
        command: Command::Window(Operation::Focus(Direction::East)),
    };
    harness.run(vec![
        Event::MenuOpened { window_id: 0 },
        focus_east(),
        focus_east(),
        shrink(),
    ]);
    harness.advance(Duration::from_secs(2));

    let world = harness.world();
    assert_window_size!(world, 2, 400, 748);
    assert_window_at!(world, 0, -176, TEST_MENUBAR_HEIGHT);
    assert_window_at!(world, 1, 224, TEST_MENUBAR_HEIGHT);
    assert_window_at!(world, 2, 624, TEST_MENUBAR_HEIGHT);
}

#[test]
fn test_window_resize_pressed_again_while_refused_starts_from_the_kept_width() {
    let mut harness = focused_middle_window_with_min_width(400);

    harness.run(vec![shrink()]);
    harness.advance(Duration::from_millis(300));
    harness.run(vec![shrink()]);
    harness.advance(Duration::from_secs(2));

    let world = harness.world();
    assert_window_size!(world, 1, 400, 748);
    assert_window_at!(world, 0, 0, TEST_MENUBAR_HEIGHT);
    assert_window_at!(world, 1, 400, TEST_MENUBAR_HEIGHT);
    assert_window_at!(world, 2, 800, TEST_MENUBAR_HEIGHT);
}

#[test]
fn test_window_step_resize_pressed_again_while_refused_leaves_the_strip_in_place() {
    let mut harness = focused_middle_window_with_min_width(400);

    harness.run(vec![binding("window_step_shrink")]);
    harness.advance(Duration::from_millis(300));
    harness.run(vec![binding("window_step_shrink")]);
    harness.advance(Duration::from_secs(2));

    let world = harness.world();
    assert_window_size!(world, 1, 400, 748);
    assert_window_at!(world, 0, 0, TEST_MENUBAR_HEIGHT);
    assert_window_at!(world, 1, 400, TEST_MENUBAR_HEIGHT);
    assert_window_at!(world, 2, 800, TEST_MENUBAR_HEIGHT);
}

#[test]
fn test_window_resize_refused_with_a_late_resize_notification_leaves_the_strip_in_place() {
    let mut harness = focused_middle_window_with_min_width(400);

    harness.run(vec![shrink()]);
    harness
        .mock_state
        .os_resize_window(1, crate::manager::Size::new(400, 748));
    harness.advance(Duration::from_secs(2));

    let world = harness.world();
    assert_window_size!(world, 1, 400, 748);
    assert_window_at!(world, 0, 0, TEST_MENUBAR_HEIGHT);
    assert_window_at!(world, 1, 400, TEST_MENUBAR_HEIGHT);
    assert_window_at!(world, 2, 800, TEST_MENUBAR_HEIGHT);
}

#[test]
fn test_window_resize_refused_then_centered_stays_centered() {
    let mut harness = focused_middle_window_with_min_width(400);

    harness.run(vec![
        shrink(),
        Event::Command {
            command: Command::Window(Operation::Center),
        },
    ]);
    harness.advance(Duration::from_secs(2));

    let world = harness.world();
    assert_window_size!(world, 1, 400, 748);
    assert_window_at!(world, 1, 312, TEST_MENUBAR_HEIGHT);
}

#[test]
fn test_window_resize_refused_then_centered_during_animation_keeps_the_new_placement() {
    for rate in [8.0, 12.0, 20.0] {
        let mut harness = focused_middle_window_with_min_width(400);
        let config: Config = (
            MainOptions {
                preset_column_widths: vec![0.25, 0.5, 0.75],
                continuous_swipe: Some(true),
                animation_speed: Some(rate),
                ..Default::default()
            },
            vec![],
        )
            .into();
        harness.world().insert_resource(config);
        harness.world().write_message::<Event>(shrink());
        harness.advance(Duration::from_millis(100));
        harness.run(vec![Event::Command {
            command: Command::Window(Operation::Center),
        }]);
        harness.advance(Duration::from_millis(50));
        let centered_x = {
            let world = harness.world();
            let entity = find_window_entity(1, world);
            let position = world.get::<crate::ecs::Position>(entity).unwrap();
            world
                .get::<crate::ecs::RepositionMarker>(entity)
                .map_or(position.0.x, |target| target.0.x)
        };
        harness.advance(Duration::from_secs(4));

        let world = harness.world();
        assert_window_size!(world, 1, 400, 748);
        assert_window_at!(world, 1, centered_x, TEST_MENUBAR_HEIGHT);
    }
}

#[test]
fn test_window_resize_after_center_during_animation_uses_a_fresh_center() {
    let mut harness = focused_middle_window_with_min_width(400);
    let config: Config = (
        MainOptions {
            preset_column_widths: vec![0.25, 0.5, 0.75],
            continuous_swipe: Some(true),
            animation_speed: Some(8.0),
            ..Default::default()
        },
        vec![],
    )
        .into();
    harness.world().insert_resource(config);
    harness.world().write_message::<Event>(shrink());
    harness.advance(Duration::from_millis(100));
    harness.world().write_message::<Event>(Event::Command {
        command: Command::Window(Operation::Center),
    });
    harness.advance(Duration::from_millis(100));
    let fresh_center = {
        let world = harness.world();
        let entity = find_window_entity(1, world);
        let position = world.get::<crate::ecs::Position>(entity).unwrap().0;
        let size = world.get::<crate::ecs::Bounds>(entity).unwrap().0;
        IRect::from_corners(position, position + size).center().x
    };
    assert_ne!(fresh_center, 600);
    harness.world().write_message::<Event>(Event::Command {
        command: Command::Window(Operation::SetWidth(0.25)),
    });
    harness.advance(Duration::from_secs(4));

    let world = harness.world();
    assert_window_size!(world, 1, 400, 748);
    assert_window_at!(world, 1, fresh_center - 200, TEST_MENUBAR_HEIGHT);
}

#[test]
fn test_window_resize_started_on_the_next_frame_keeps_the_original_center() {
    let mut harness = focused_middle_window_with_min_width(400);
    harness.world().write_message::<Event>(shrink());
    harness.advance(Duration::from_millis(20));
    harness.world().write_message::<Event>(Event::Command {
        command: Command::Window(Operation::SetWidth(0.75)),
    });
    harness.advance(Duration::from_secs(2));

    let world = harness.world();
    assert_window_size!(world, 1, 768, 748);
    assert_window_at!(world, 1, 216, TEST_MENUBAR_HEIGHT);
}

#[test]
fn test_window_resize_mixed_with_center_in_one_frame_does_not_correct_later() {
    for center_first in [false, true] {
        let mut harness = focused_middle_window_with_min_width(400);
        let center = Event::Command {
            command: Command::Window(Operation::Center),
        };
        let resize = Event::Command {
            command: Command::Window(Operation::SetWidth(0.25)),
        };
        let events = if center_first {
            [center, resize]
        } else {
            [resize, center]
        };
        for event in events {
            harness.world().write_message::<Event>(event);
        }
        harness.advance(Duration::from_millis(100));
        let placed_x = {
            let world = harness.world();
            let entity = find_window_entity(1, world);
            world.get::<crate::ecs::Position>(entity).unwrap().0.x
        };
        harness.advance(Duration::from_secs(2));

        let world = harness.world();
        assert_window_size!(world, 1, 400, 748);
        assert_window_at!(world, 1, placed_x, TEST_MENUBAR_HEIGHT);
    }
}

#[test]
fn test_window_resize_lua_width_continuations_preserve_the_original_center() {
    use crate::types::windowset::LayoutOp;

    for start_with_cli in [false, true] {
        let mut harness = focused_middle_window_with_min_width(400);
        if start_with_cli {
            harness.run(vec![shrink()]);
        }
        let resize = || Event::Command {
            command: Command::Layout(vec![LayoutOp::SetWidth {
                window: 1,
                ratio: 0.25,
            }]),
        };
        harness.run(vec![resize(), resize(), resize()]);
        harness.advance(Duration::from_secs(2));

        let world = harness.world();
        assert_window_size!(world, 1, 400, 748);
        assert_window_at!(world, 1, 400, TEST_MENUBAR_HEIGHT);
    }
}

#[test]
fn test_window_resize_lua_width_on_unfocused_window_cancels_pending_recovery() {
    use crate::types::windowset::LayoutOp;

    let mut harness = focused_middle_window_with_min_width(400);
    harness.run(vec![
        shrink(),
        Event::Command {
            command: Command::Layout(vec![LayoutOp::SetWidth {
                window: 0,
                ratio: 0.5,
            }]),
        },
    ]);
    let placed_x = {
        let world = harness.world();
        let entity = find_window_entity(1, world);
        assert!(world.get::<crate::ecs::AwaitResize>(entity).is_none());
        world.get::<crate::ecs::Position>(entity).unwrap().0.x
    };
    harness.advance(Duration::from_secs(2));

    let world = harness.world();
    assert_window_size!(world, 1, 400, 748);
    assert_window_at!(world, 1, placed_x, TEST_MENUBAR_HEIGHT);
}

#[test]
#[cfg(feature = "lua")]
fn test_window_resize_lua_callback_invocation_preserves_pending_recovery() {
    let mut harness = focused_middle_window_with_min_width(400);
    harness.run(vec![shrink()]);
    harness.world().write_message::<Event>(Event::Command {
        command: Command::Lua(0),
    });
    harness.advance(Duration::from_millis(20));
    harness.run(vec![shrink()]);
    harness.advance(Duration::from_secs(2));

    let world = harness.world();
    assert_window_size!(world, 1, 400, 748);
    assert_window_at!(world, 1, 400, TEST_MENUBAR_HEIGHT);
}

#[test]
fn test_window_resize_refused_then_focus_moved_away_and_back_keeps_the_new_placement() {
    let mut harness = focused_middle_window_with_min_width(400);
    let focus = |direction| Event::Command {
        command: Command::Window(Operation::Focus(direction)),
    };

    harness.run(vec![
        shrink(),
        focus(Direction::East),
        focus(Direction::West),
    ]);
    harness.advance(Duration::from_secs(2));

    let world = harness.world();
    assert_window_size!(world, 1, 400, 748);
    assert_window_at!(world, 1, 224, TEST_MENUBAR_HEIGHT);
}

#[test]
fn test_window_resize_refused_at_the_left_edge_keeps_the_window_flush() {
    let config: Config = (
        MainOptions {
            preset_column_widths: vec![0.25, 0.5, 0.75],
            continuous_swipe: Some(true),
            ..Default::default()
        },
        vec![],
    )
        .into();
    let mut harness = TestHarness::new()
        .with_config(config)
        .with_window(0, |window| window.max_width = 400)
        .with_window(1, |_| {})
        .with_window(2, |_| {});

    harness.run(vec![
        Event::MenuOpened { window_id: 0 },
        Event::Command {
            command: Command::Window(Operation::Resize(ResizeDirection::Grow)),
        },
    ]);
    harness.advance(Duration::from_secs(2));

    let world = harness.world();
    assert_window_size!(world, 0, 400, 748);
    assert_window_at!(world, 0, 0, TEST_MENUBAR_HEIGHT);
    assert_window_at!(world, 1, 400, TEST_MENUBAR_HEIGHT);
    assert_window_at!(world, 2, 800, TEST_MENUBAR_HEIGHT);
}

#[test]
fn test_window_resize_applied_late_by_the_app_keeps_its_placement() {
    let mut harness = focused_middle_window_with_min_width(400);

    harness.run(vec![shrink()]);
    harness
        .mock_state
        .update_window(1, |window| window.min_width = 0);
    harness
        .mock_state
        .os_resize_window(1, crate::manager::Size::new(256, 748));
    harness.advance(Duration::from_secs(2));

    let world = harness.world();
    assert_window_size!(world, 1, 256, 748);
    assert_window_at!(world, 0, 72, TEST_MENUBAR_HEIGHT);
    assert_window_at!(world, 1, 472, TEST_MENUBAR_HEIGHT);
    assert_window_at!(world, 2, 728, TEST_MENUBAR_HEIGHT);
}

#[test]
fn test_window_resize_applied_late_without_notification_updates_the_strip() {
    let mut harness = focused_middle_window_with_min_width(400);
    harness.run(vec![shrink()]);
    harness.mock_state.update_window(1, |window| {
        window.min_width = 0;
        window.frame.max = window.frame.min + crate::manager::Size::new(256, 748);
    });
    harness.advance(Duration::from_secs(2));

    let world = harness.world();
    let entity = find_window_entity(1, world);
    assert_eq!(world.get::<crate::ecs::Bounds>(entity).unwrap().0.x, 256);
    assert_window_size!(world, 1, 256, 748);
    assert_window_at!(world, 0, 72, TEST_MENUBAR_HEIGHT);
    assert_window_at!(world, 1, 472, TEST_MENUBAR_HEIGHT);
    assert_window_at!(world, 2, 728, TEST_MENUBAR_HEIGHT);
}

#[test]
fn test_window_resize_refused_by_unfocused_stack_sibling_restores_the_strip() {
    for refusing_sibling in [1, 2] {
        let config: Config = (
            MainOptions {
                preset_column_widths: vec![0.25, 0.5, 0.75],
                continuous_swipe: Some(true),
                ..Default::default()
            },
            vec![],
        )
            .into();
        let mut harness = TestHarness::new().with_config(config).with_windows(4);
        let focus = || Event::Command {
            command: Command::Window(Operation::Focus(Direction::East)),
        };
        harness.run(vec![
            Event::MenuOpened { window_id: 0 },
            focus(),
            focus(),
            Event::Command {
                command: Command::Window(Operation::Stack(true)),
            },
        ]);
        if refusing_sibling == 2 {
            harness.run(vec![Event::Command {
                command: Command::Window(Operation::Focus(Direction::North)),
            }]);
        }
        harness
            .mock_state
            .update_window(refusing_sibling, |window| window.min_width = 400);
        let initial_x = {
            let world = harness.world();
            let entity = find_window_entity(1, world);
            world.get::<crate::ecs::Position>(entity).unwrap().0.x
        };
        harness.run(vec![shrink()]);
        harness.advance(Duration::from_secs(2));

        let world = harness.world();
        assert_window_size!(world, 1, 400, 374);
        assert_window_size!(world, 2, 400, 374);
        assert_window_at!(world, 0, initial_x - 400, TEST_MENUBAR_HEIGHT);
        assert_window_at!(world, 1, initial_x, TEST_MENUBAR_HEIGHT);
        assert_window_at!(world, 2, initial_x, TEST_MENUBAR_HEIGHT + 374);
        assert_window_at!(world, 3, initial_x + 400, TEST_MENUBAR_HEIGHT);
    }
}

#[test]
fn test_full_width_centers_and_fills_viewport_when_window_was_partially_offscreen() {
    let commands = vec![
        Event::MenuOpened { window_id: 0 },
        Event::Command {
            command: Command::Window(Operation::Center),
        },
        Event::Command {
            command: Command::Window(Operation::FullWidth),
        },
        Event::Command {
            command: Command::Window(Operation::FullWidth),
        },
    ];

    let config: Config = (
        MainOptions {
            window_hidden_ratio: Some(0.5),
            continuous_swipe: Some(true),
            ..Default::default()
        },
        vec![],
    )
        .into();

    TestHarness::new()
        .with_config(config)
        .with_windows(2)
        .on_iteration(1, |world, _state| {
            // Window 0 (width 400) is centered at x = 312.
            assert_window_at!(world, 0, 312, TEST_MENUBAR_HEIGHT);
        })
        .on_iteration(2, |world, _state| {
            // Even when `window_hidden_ratio` is set (312 / 1024 = 0.30 < 0.5)
            // and window 0 was off-origin, `FullWidth` must bring it fully on
            // screen (`x = 0`, `width = 1024`).
            assert_window_size!(world, 0, 1024, 748);
            assert_window_at!(world, 0, 0, TEST_MENUBAR_HEIGHT);
        })
        .on_iteration(3, |world, _state| {
            // Toggling `FullWidth` off restores the original width (400) and
            // centers it in place (`512 - 200 = 312`).
            assert_window_size!(world, 0, 400, 748);
            assert_window_at!(world, 0, 312, TEST_MENUBAR_HEIGHT);
        })
        .run(commands);
}

#[test]
fn new_window_on_inactive_space_joins_that_space_strip() {
    const OTHER_WORKSPACE_ID: WorkspaceId = 3;

    TestHarness::new()
        .with_display(
            TEST_DISPLAY_ID,
            IRect::new(0, 0, TEST_DISPLAY_WIDTH, TEST_DISPLAY_HEIGHT),
            vec![TEST_WORKSPACE_ID, OTHER_WORKSPACE_ID],
        )
        .with_windows(1)
        .on_iteration(0, |world, state| {
            let frame = IRect::new(0, 0, TEST_WINDOW_WIDTH, TEST_WINDOW_HEIGHT);
            let window = state.spawn_window(TEST_PROCESS_ID, OTHER_WORKSPACE_ID, 1, frame);
            world.trigger(SpawnWindowTrigger(vec![window]));
        })
        .on_iteration(1, |world, _state| {
            let mut strips = world.query::<(&LayoutStrip, Has<ActiveWorkspaceMarker>)>();
            let new_window = find_window_entity(1, world);
            let mut active_ids = vec![];
            for (strip, active) in strips.iter(world) {
                if active {
                    active_ids.push(strip.id());
                }
                assert_eq!(
                    strip.contains(new_window),
                    strip.id() == OTHER_WORKSPACE_ID,
                    "window 1 must only be in the strip of its own Space (strip {})",
                    strip.id()
                );
            }
            assert_eq!(active_ids, vec![TEST_WORKSPACE_ID]);
        })
        .run(vec![
            Event::MenuOpened { window_id: 0 },
            Event::Command {
                command: Command::PrintState,
            },
        ]);
}

#[test]
fn new_window_on_active_space_keeps_after_focus_placement() {
    TestHarness::new()
        .with_windows(2)
        .with_focused_window(0)
        .on_iteration(0, |world, state| {
            let frame = IRect::new(0, 0, TEST_WINDOW_WIDTH, TEST_WINDOW_HEIGHT);
            let window = state.spawn_window(TEST_PROCESS_ID, TEST_WORKSPACE_ID, 2, frame);
            world.trigger(SpawnWindowTrigger(vec![window]));
        })
        .on_iteration(1, |world, _state| {
            let new_window = find_window_entity(2, world);
            let mut strips = world.query::<(&LayoutStrip, Has<ActiveWorkspaceMarker>)>();
            let (strip, _) = strips
                .iter(world)
                .find(|(_, active)| *active)
                .expect("active strip");
            assert_eq!(strip.index_of(new_window).ok(), Some(1));
        })
        .run(vec![
            Event::MenuOpened { window_id: 0 },
            Event::Command {
                command: Command::PrintState,
            },
        ]);
}

/// A harness with `count` windows and `auto_center` as given, settled.
fn balance_harness(count: i32, auto_center: bool) -> TestHarness {
    balance_harness_with(
        count,
        MainOptions {
            auto_center: Some(auto_center),
            ..Default::default()
        },
    )
}

fn balance_harness_with(count: i32, options: MainOptions) -> TestHarness {
    let config: Config = (options, vec![]).into();
    let mut h = TestHarness::new().with_config(config).with_windows(count);
    balance_pump(&mut h, Command::PrintState);
    h
}

/// A second of simulated time: long enough for any resize or slide to land.
fn balance_settle(h: &mut TestHarness) {
    h.advance(Duration::from_secs(1));
}

fn balance_pump(h: &mut TestHarness, command: Command) {
    h.app
        .world_mut()
        .write_message::<Event>(Event::Command { command });
    balance_settle(h);
}

fn balance_focus(h: &mut TestHarness, id: crate::platform::WinID) {
    h.mock_state.focus_window(id);
    balance_settle(h);
}

/// Window `id`'s frame as it was last placed.
fn balance_frame(h: &mut TestHarness, id: crate::platform::WinID) -> IRect {
    let world = h.app.world_mut();
    let mut query = world.query::<&crate::manager::Window>();
    query
        .iter(world)
        .find(|w| w.id() == id)
        .expect("window not found")
        .frame()
}

/// Gives each window its width, focuses `focused`, balances, and checks the
/// focused window kept its place on screen while the others took its width.
fn assert_balance_keeps_focused_in_place(
    h: &mut TestHarness,
    widths: &[(crate::platform::WinID, f64)],
    focused: crate::platform::WinID,
    ids: &[crate::platform::WinID],
) {
    for &(id, ratio) in widths {
        balance_focus(h, id);
        balance_pump(h, Command::Window(Operation::SetWidth(ratio)));
    }
    balance_focus(h, focused);
    let before = balance_frame(h, focused);

    balance_pump(h, Command::Window(Operation::Balance));

    let after = balance_frame(h, focused);
    assert_eq!(
        after.min.x, before.min.x,
        "focused window moved: {before:?} -> {after:?}"
    );
    for &id in ids {
        assert_eq!(
            balance_frame(h, id).width(),
            before.width(),
            "window {id} width"
        );
    }
}

#[test]
fn balance_keeps_focused_in_place_when_others_were_wider() {
    for auto_center in [true, false] {
        let mut h = balance_harness(4, auto_center);
        assert_balance_keeps_focused_in_place(&mut h, &[(2, 0.25)], 2, &[0, 1, 2, 3]);
    }
}

#[test]
fn balance_keeps_focused_in_place_when_others_were_narrower() {
    for auto_center in [true, false] {
        let mut h = balance_harness(4, auto_center);
        assert_balance_keeps_focused_in_place(
            &mut h,
            &[(0, 0.25), (1, 0.25), (3, 0.25), (2, 0.5)],
            2,
            &[0, 1, 2, 3],
        );
    }
}

#[test]
fn balance_of_first_column_leaves_it_in_place() {
    for auto_center in [true, false] {
        let mut h = balance_harness(4, auto_center);
        assert_balance_keeps_focused_in_place(&mut h, &[(0, 0.25)], 0, &[0, 1, 2, 3]);
    }
}

#[test]
fn balance_keeps_focused_in_place_after_a_stack() {
    for auto_center in [true, false] {
        let mut h = balance_harness(5, auto_center);
        // Window 1 onto window 0's column: a stack left of the focused window.
        balance_focus(&mut h, 1);
        balance_pump(&mut h, Command::Window(Operation::Stack(true)));
        assert_balance_keeps_focused_in_place(&mut h, &[(3, 0.25)], 3, &[0, 2, 3, 4]);
    }
}

/// With `auto_center` and continuous swipes off, a strip that fits after
/// balancing is pinned to the left edge, measured by its new width rather
/// than the old.
#[test]
fn balance_pins_a_strip_that_now_fits_to_the_left_edge() {
    let mut h = balance_harness_with(
        3,
        MainOptions {
            auto_center: Some(false),
            continuous_swipe: Some(false),
            ..Default::default()
        },
    );
    balance_focus(&mut h, 2);
    balance_pump(&mut h, Command::Window(Operation::SetWidth(0.25)));
    balance_pump(&mut h, Command::Window(Operation::Balance));

    let first = balance_frame(&mut h, 0);
    let last = balance_frame(&mut h, 2);
    assert_eq!(first.width(), last.width());
    assert_eq!(first.min.x, 0, "first window off the left edge: {first:?}");
    assert!(
        last.max.x <= TEST_DISPLAY_WIDTH,
        "strip overflows: {last:?}"
    );
}

/// The float key acts on the window the engine marks focused. When the user
/// has switched to a window the engine does not manage, such as one from an
/// accessory app, that mark still sits on the last managed window. Floating it
/// would change a window the user is not looking at.
#[test]
fn test_float_ignores_window_macos_no_longer_focuses() {
    const ACCESSORY: i32 = TEST_PROCESS_ID + 1;
    let manage = || Event::Command {
        command: Command::Window(Operation::Manage),
    };
    let commands = vec![
        Event::Command {
            command: Command::PrintState,
        }, // 0
        Event::Command {
            command: Command::PrintState,
        }, // 1
        manage(), // 2
        Event::Command {
            command: Command::PrintState,
        }, // 3
        manage(), // 4
    ];
    let floating = |world: &mut World, id| {
        let entity = find_window_entity(id, world);
        world.get::<crate::ecs::Unmanaged>(entity).is_some()
    };

    TestHarness::new()
        .with_windows(2)
        .with_focused_window(0)
        .on_iteration(0, |_world, state| {
            state.spawn_app(ACCESSORY, "com.example.accessory", "Accessory");
            let frame = IRect::new(0, 0, TEST_WINDOW_WIDTH, TEST_WINDOW_HEIGHT);
            state.spawn_window(ACCESSORY, TEST_WORKSPACE_ID, 9, frame);
            state.update_app(TEST_PROCESS_ID, |app| app.is_frontmost = false);
            state.focus_window(9);
        })
        .on_iteration(1, |world, _state| {
            assert_focused!(world, 0);
        })
        .on_iteration(2, move |world, state| {
            assert!(
                !floating(world, 0),
                "window 0 must stay tiled while macOS focuses another app"
            );
            state.update_app(ACCESSORY, |app| app.is_frontmost = false);
            state.update_app(TEST_PROCESS_ID, |app| app.is_frontmost = true);
            state.focus_window(0);
        })
        .on_iteration(4, move |world, _state| {
            assert!(
                floating(world, 0),
                "window 0 must float once macOS focuses it again"
            );
        })
        .run(commands);
}

/// An app too busy to answer the focused-window query must not block the
/// float key, so only a different answer from macOS skips the toggle.
#[test]
fn test_float_proceeds_when_the_app_cannot_report_focus() {
    let commands = vec![
        Event::Command {
            command: Command::PrintState,
        }, // 0
        Event::Command {
            command: Command::Window(Operation::Manage),
        }, // 1
    ];

    TestHarness::new()
        .with_windows(2)
        .with_focused_window(0)
        .on_iteration(0, |_world, state| {
            state.update_app(TEST_PROCESS_ID, |app| app.focused_window_id = None);
        })
        .on_iteration(1, |world, _state| {
            let entity = find_window_entity(0, world);
            assert!(
                world.get::<crate::ecs::Unmanaged>(entity).is_some(),
                "window 0 must float when its app cannot report focus"
            );
        })
        .run(commands);
}

/// macOS can focus another window of the same app before its focus event
/// reaches the engine. The float key must not float the window the user left.
#[test]
fn test_float_ignores_window_before_its_app_reports_new_focus() {
    let commands = vec![
        Event::Command {
            command: Command::PrintState,
        }, // 0
        Event::Command {
            command: Command::Window(Operation::Manage),
        }, // 1
    ];

    TestHarness::new()
        .with_windows(2)
        .with_focused_window(0)
        .on_iteration(0, |_world, state| {
            state.set_focused_window(1);
        })
        .on_iteration(1, |world, _state| {
            assert_focused!(world, 0);
            for id in [0, 1] {
                let entity = find_window_entity(id, world);
                assert!(
                    world.get::<crate::ecs::Unmanaged>(entity).is_none(),
                    "window {id} must stay tiled"
                );
            }
        })
        .run(commands);
}

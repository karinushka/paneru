//! Tests for the paneru-driven "tabbed display" toggle on `Column::Stack`
//! (one window visible at a time, sharing the full column rect, cycled with
//! the existing `Focus` North/South directions). Deliberately kept separate
//! from `src/tests/tabs.rs`, which covers the unrelated macOS-native
//! "Merge All Windows" tab detection.

use bevy::prelude::*;

use crate::assert_focused;
use crate::assert_window_size;
use crate::commands::{Command, Direction, Operation};
use crate::ecs::layout::LayoutStrip;
use crate::events::Event;

use super::*;

fn is_tabbed_display(world: &mut World, window_id: i32) -> bool {
    let entity = find_window_entity(window_id, world);
    let mut query = world.query::<&LayoutStrip>();
    query
        .single(world)
        .expect("a single active strip")
        .is_tabbed_display(entity)
}

/// Builds a 3-window `Column::Stack` (windows 0, 1, 2) followed by a fourth,
/// separate single-window column (window 3), via the same `Operation::Stack`
/// path a real keybinding would use.
fn stack_first_three(commands: &mut Vec<Event>) {
    commands.extend([
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
    ]);
}

#[test]
fn test_toggle_tabbed_display_gives_every_member_the_full_column_size() {
    let mut commands = vec![Event::MenuOpened { window_id: 0 }];
    stack_first_three(&mut commands);
    commands.push(Event::Command {
        command: Command::Window(Operation::ToggleTabbedDisplay),
    });

    TestHarness::new()
        .with_windows(4)
        .on_iteration(commands.len() - 1, |world, _state| {
            let expected_height = TEST_DISPLAY_HEIGHT - TEST_MENUBAR_HEIGHT;
            assert_window_size!(world, 0, TEST_WINDOW_WIDTH, expected_height);
            assert_window_size!(world, 1, TEST_WINDOW_WIDTH, expected_height);
            assert_window_size!(world, 2, TEST_WINDOW_WIDTH, expected_height);
        })
        .run(commands);
}

fn window_height(world: &mut World, id: i32) -> i32 {
    let mut query = world.query::<&crate::manager::Window>();
    query
        .iter(world)
        .find(|w| w.id() == id)
        .expect("window not found")
        .frame()
        .height()
}

#[test]
fn test_toggle_tabbed_display_off_restores_split_heights() {
    let mut commands = vec![Event::MenuOpened { window_id: 0 }];
    stack_first_three(&mut commands);
    commands.push(Event::Command {
        command: Command::Window(Operation::ToggleTabbedDisplay),
    });
    commands.push(Event::Command {
        command: Command::Window(Operation::ToggleTabbedDisplay),
    });

    TestHarness::new()
        .with_windows(4)
        .on_iteration(commands.len() - 1, |world, _state| {
            let heights = [
                window_height(world, 0),
                window_height(world, 1),
                window_height(world, 2),
            ];
            assert!(
                heights
                    .iter()
                    .any(|&h| h != TEST_DISPLAY_HEIGHT - TEST_MENUBAR_HEIGHT),
                "toggling off must restore a split (non-full-height) layout, got {heights:?}"
            );
        })
        .run(commands);
}

#[test]
fn test_focus_south_cycles_tabbed_stack_without_leaving_the_column() {
    let mut commands = vec![Event::MenuOpened { window_id: 0 }];
    stack_first_three(&mut commands);
    commands.push(Event::Command {
        command: Command::Window(Operation::ToggleTabbedDisplay),
    });
    // Start from a known tab: `Direction::First` focuses the leftmost
    // column's `top()`, which is window 0 regardless of tabbed state.
    commands.push(Event::Command {
        command: Command::Window(Operation::Focus(Direction::First)),
    });
    let after_first = commands.len() - 1;
    commands.push(Event::Command {
        command: Command::Window(Operation::Focus(Direction::South)),
    });
    let after_second = commands.len() - 1;
    commands.push(Event::Command {
        command: Command::Window(Operation::Focus(Direction::South)),
    });
    let after_third = commands.len() - 1;
    // One more South past the last tab must not jump to window 3's column.
    commands.push(Event::Command {
        command: Command::Window(Operation::Focus(Direction::South)),
    });
    let after_fourth = commands.len() - 1;

    TestHarness::new()
        .with_windows(4)
        .on_iteration(after_first, |world, _state| assert_focused!(world, 0))
        .on_iteration(after_second, |world, _state| assert_focused!(world, 1))
        .on_iteration(after_third, |world, _state| assert_focused!(world, 2))
        .on_iteration(after_fourth, |world, _state| {
            assert_focused!(world, 2);
        })
        .run(commands);
}

#[test]
fn test_close_active_tab_focuses_sibling_not_other_column() {
    let mut commands = vec![Event::MenuOpened { window_id: 0 }];
    // Stack only windows 0 and 1 (leave 2 as its own, separate column).
    commands.push(Event::Command {
        command: Command::Window(Operation::Focus(Direction::East)),
    });
    commands.push(Event::Command {
        command: Command::Window(Operation::Stack(true)),
    });
    commands.push(Event::Command {
        command: Command::Window(Operation::ToggleTabbedDisplay),
    });
    let toggle_iter = commands.len() - 1;
    // `os_close_window`'s destroy events are only drained on the *next*
    // iteration's update loop and observed the one after that — mirrors
    // `test_closing_window_of_live_app_closes_the_gap` in tiling.rs.
    commands.push(Event::Command {
        command: Command::PrintState,
    });
    commands.push(Event::Command {
        command: Command::PrintState,
    });

    TestHarness::new()
        .with_windows(3)
        .on_iteration(toggle_iter, |world, state| {
            // Window 1 is the active tab (it was focused when toggling).
            assert_focused!(world, 1);
            state.os_close_window(1);
        })
        .on_iteration(toggle_iter + 2, |world, _state| {
            assert!(
                !window_exists(world, 1),
                "closed window must be dropped from the world"
            );
            assert_focused!(world, 0);
        })
        .run(commands);
}

#[test]
fn test_toggle_tabbed_display_noop_on_single_window_column() {
    let commands = vec![
        Event::MenuOpened { window_id: 0 },
        Event::Command {
            command: Command::Window(Operation::ToggleTabbedDisplay),
        },
    ];

    TestHarness::new()
        .with_windows(1)
        .on_iteration(1, |world, _state| {
            assert!(
                !is_tabbed_display(world, 0),
                "toggling a lone Single column must be a no-op"
            );
        })
        .run(commands);
}

/// Regression: `toggle_tabbed_display` used to return a plain `bool`, so
/// "not applicable" (nothing to toggle) and "successfully turned tabs off"
/// were indistinguishable — both reported `false`. That made repeatedly
/// pressing the hotkey on a window that isn't actually stacked flash "Tabs
/// off" every single time, looking like a stuck toggle. The handler must
/// stay completely silent (no flash message) when the toggle isn't
/// applicable.
#[test]
fn test_toggle_tabbed_display_noop_does_not_flash_a_message() {
    let commands = vec![
        Event::MenuOpened { window_id: 0 },
        Event::Command {
            command: Command::Window(Operation::ToggleTabbedDisplay),
        },
        Event::Command {
            command: Command::Window(Operation::ToggleTabbedDisplay),
        },
        Event::Command {
            command: Command::Window(Operation::ToggleTabbedDisplay),
        },
    ];

    TestHarness::new()
        .with_windows(1)
        .on_iteration(3, |world, _state| {
            let mut query = world.query::<&crate::ecs::FlashMessage>();
            assert!(
                query.iter(world).next().is_none(),
                "toggling a non-applicable column must not flash any message, \
                 repeatedly or otherwise"
            );
        })
        .run(commands);
}

fn window_exists(world: &mut World, id: i32) -> bool {
    let mut query = world.query::<&crate::manager::Window>();
    query.iter(world).any(|window| window.id() == id)
}

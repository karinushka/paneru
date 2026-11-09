//! Tests for the `window_consume`/`window_expel` commands — the
//! complementary pair to the existing `window_stack`/`window_unstack`.
//! `Consume` pulls the column to the *right* into the current column
//! (mirror of `Stack(true)`, which merges leftward); `Expel` is
//! mechanically identical to `Stack(false)` but leaves focus on the
//! remaining stack instead of following the ejected window out.

use bevy::prelude::*;

use crate::assert_focused;
use crate::commands::{Command, Direction, Operation};
use crate::ecs::layout::LayoutStrip;
use crate::events::Event;

use super::*;

/// Builds a 3-window `Column::Stack` (windows 0, 1, 2) followed by a fourth,
/// separate single-window column (window 3) — same helper `tabbed_display`
/// tests use, duplicated here since it's specific to test setup, not shared
/// production code.
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

fn same_column(world: &mut World, id_a: i32, id_b: i32) -> bool {
    let entity_a = find_window_entity(id_a, world);
    let entity_b = find_window_entity(id_b, world);
    let mut query = world.query::<&LayoutStrip>();
    let strip = query.single(world).expect("a single active strip");
    strip.index_of(entity_a).ok() == strip.index_of(entity_b).ok()
}

#[test]
fn test_consume_pulls_the_right_neighbor_into_the_current_column() {
    let commands = vec![
        Event::MenuOpened { window_id: 0 },
        Event::Command {
            command: Command::Window(Operation::Consume),
        },
    ];

    TestHarness::new()
        .with_windows(2)
        .on_iteration(commands.len() - 1, |world, _state| {
            assert!(
                same_column(world, 0, 1),
                "window 1 must have been consumed into window 0's column"
            );
            assert_focused!(world, 0);
        })
        .run(commands);
}

#[test]
fn test_consume_is_a_noop_with_no_right_neighbor() {
    let commands = vec![
        Event::MenuOpened { window_id: 0 },
        Event::Command {
            command: Command::Window(Operation::Focus(Direction::East)),
        },
        // Window 1 is now the last column — nothing to its right to consume.
        Event::Command {
            command: Command::Window(Operation::Consume),
        },
    ];

    TestHarness::new()
        .with_windows(2)
        .on_iteration(commands.len() - 1, |world, _state| {
            assert!(
                !same_column(world, 0, 1),
                "there is nothing to the right of window 1 — consume must be a no-op"
            );
            assert_focused!(world, 1);
        })
        .run(commands);
}

/// Expelling the *last* item in a stack has no next (South) sibling, so it
/// must fall back to the previous (North) one.
#[test]
fn test_expel_falls_back_to_the_previous_item_when_expelling_the_last_one() {
    let mut commands = vec![Event::MenuOpened { window_id: 0 }];
    stack_first_three(&mut commands);
    // `stack_first_three` leaves window 2 focused — the last item in the
    // Stack(0, 1, 2).
    commands.push(Event::Command {
        command: Command::Window(Operation::Expel),
    });
    let after_expel = commands.len() - 1;

    TestHarness::new()
        .with_windows(4)
        .on_iteration(after_expel, |world, _state| {
            assert!(
                !same_column(world, 1, 2),
                "window 2 must have been ejected into its own column"
            );
            assert!(
                same_column(world, 0, 1),
                "windows 0 and 1 must still share the remaining stack"
            );
            assert_focused!(world, 1);
        })
        .run(commands);
}

/// Expelling a non-last item must land focus on the *next* (South) item.
#[test]
fn test_expel_focuses_the_next_item_when_expelling_a_middle_one() {
    let mut commands = vec![Event::MenuOpened { window_id: 0 }];
    stack_first_three(&mut commands);
    // Move focus from window 2 (last) to window 1 (middle).
    commands.push(Event::Command {
        command: Command::Window(Operation::Focus(Direction::North)),
    });
    commands.push(Event::Command {
        command: Command::Window(Operation::Expel),
    });
    let after_expel = commands.len() - 1;

    TestHarness::new()
        .with_windows(4)
        .on_iteration(after_expel, |world, _state| {
            assert!(
                !same_column(world, 0, 1),
                "window 1 must have been ejected into its own column"
            );
            assert!(
                same_column(world, 0, 2),
                "windows 0 and 2 must still share the remaining stack"
            );
            assert_focused!(world, 2);
        })
        .run(commands);
}

/// Expelling a window that isn't in a stack at all must be a complete
/// no-op — same as `window_unstack`'s existing no-op behavior.
#[test]
fn test_expel_is_a_noop_outside_a_stack() {
    let commands = vec![
        Event::MenuOpened { window_id: 0 },
        Event::Command {
            command: Command::Window(Operation::FullWidth),
        },
        Event::Command {
            command: Command::Window(Operation::Expel),
        },
    ];

    TestHarness::new()
        .with_windows(2)
        .on_iteration(commands.len() - 1, |world, _state| {
            assert!(!same_column(world, 0, 1));
            assert_focused!(world, 0);
            let entity0 = find_window_entity(0, world);
            assert!(
                world.get::<crate::ecs::FullWidthMarker>(entity0).is_some(),
                "a no-op expel outside a stack must not strip FullWidthMarker"
            );
        })
        .run(commands);
}

#[test]
fn test_consume_into_tabbed_stack_joins_tabs_and_keeps_active_tab() {
    let mut commands = vec![Event::MenuOpened { window_id: 0 }];
    stack_first_three(&mut commands);
    commands.extend([
        Event::Command {
            command: Command::Window(Operation::ToggleTabbedDisplay),
        },
        Event::Command {
            command: Command::Window(Operation::Consume),
        },
    ]);

    TestHarness::new()
        .with_windows(4)
        .on_iteration(commands.len() - 1, |world, _state| {
            let entity2 = find_window_entity(2, world);
            let entity3 = find_window_entity(3, world);
            let mut query = world.query::<&LayoutStrip>();
            let strip = query.single(world).expect("a single active strip");
            assert!(
                strip.is_tabbed_display(entity3),
                "consumed window 3 must join the tabbed stack"
            );
            assert_eq!(
                strip.active_tab(entity3),
                Some(entity2),
                "the active tab must remain window 2 after consuming window 3"
            );
            assert_focused!(world, 2);
        })
        .run(commands);
}

#[test]
fn test_consume_tabbed_neighbor_into_split_column_clears_tabbed_state() {
    let commands = vec![
        Event::MenuOpened { window_id: 0 },
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
            command: Command::Window(Operation::ToggleTabbedDisplay),
        },
        Event::Command {
            command: Command::Window(Operation::Focus(Direction::First)),
        },
        Event::Command {
            command: Command::Window(Operation::Consume),
        },
    ];

    TestHarness::new()
        .with_windows(3)
        .on_iteration(commands.len() - 1, |world, _state| {
            let entities = [0, 1, 2].map(|id| (id, find_window_entity(id, world)));
            let mut query = world.query::<&LayoutStrip>();
            let strip = query.single(world).expect("a single active strip");
            for (id, entity) in entities {
                assert!(
                    !strip.is_tabbed_display(entity),
                    "consuming a tabbed stack into a non-tabbed column must clear tabbed state on window {id}"
                );
            }
        })
        .run(commands);
}

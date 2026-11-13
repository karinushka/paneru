//! Tests for the paneru-driven "tabbed display" toggle on `Column::Stack`
//! (one window visible at a time, sharing the full column rect, cycled with
//! the existing `Focus` North/South directions). Deliberately kept separate
//! from `src/tests/tabs.rs`, which covers the unrelated macOS-native
//! "Merge All Windows" tab detection.

use bevy::ecs::system::RunSystemOnce;
use bevy::prelude::*;

use crate::assert_focused;
use crate::assert_window_size;
use crate::commands::{Command, Direction, Operation};
use crate::config::{Config, MainOptions};
use crate::ecs::RepositionMarker;
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

fn window_y(world: &mut World, id: i32) -> i32 {
    let mut query = world.query::<&crate::manager::Window>();
    query
        .iter(world)
        .find(|w| w.id() == id)
        .expect("window not found")
        .frame()
        .min
        .y
}

fn window_x(world: &mut World, id: i32) -> i32 {
    let mut query = world.query::<&crate::manager::Window>();
    query
        .iter(world)
        .find(|w| w.id() == id)
        .expect("window not found")
        .frame()
        .min
        .x
}

/// Inactive tabs are moved off-screen horizontally (same y), not merely
/// occluded by z-order.
#[test]
fn test_toggle_tabbed_display_parks_inactive_tabs_away_from_the_active_one() {
    let mut commands = vec![Event::MenuOpened { window_id: 0 }];
    stack_first_three(&mut commands);
    commands.push(Event::Command {
        command: Command::Window(Operation::ToggleTabbedDisplay),
    });

    TestHarness::new()
        .with_windows(4)
        .on_iteration(commands.len() - 1, |world, _state| {
            // `stack_first_three` leaves window 2 focused, so it's active.
            let parked_x0 = window_x(world, 0);
            let parked_x1 = window_x(world, 1);

            assert_eq!(
                window_y(world, 2),
                TEST_MENUBAR_HEIGHT,
                "the active tab must be on-screen at the top of the viewport"
            );
            // Parked members sit past the display's right edge.
            assert!(
                parked_x0 + TEST_WINDOW_WIDTH > TEST_DISPLAY_WIDTH,
                "inactive tab 0 must be parked off past the display's right \
                 edge, not sitting on-screen — got={parked_x0}"
            );
            assert!(
                parked_x1 + TEST_WINDOW_WIDTH > TEST_DISPLAY_WIDTH,
                "inactive tab 1 must be parked off past the display's right \
                 edge, not sitting on-screen — got={parked_x1}"
            );
            assert_eq!(
                window_y(world, 0),
                window_y(world, 2),
                "parked tabs share the active tab's y — they're hidden \
                 horizontally, not stacked below it"
            );
        })
        .run(commands);
}

/// Regression: a parked tab must not extend onto a display below, which
/// macOS would then adopt it onto.
#[test]
fn test_parked_tab_never_leaks_onto_the_display_below() {
    let mut commands = vec![Event::MenuOpened { window_id: 0 }];
    stack_first_three(&mut commands);
    commands.push(Event::Command {
        command: Command::Window(Operation::ToggleTabbedDisplay),
    });

    let mut harness = TestHarness::new();
    // A second display physically south of TEST_DISPLAY: y in
    // [TEST_DISPLAY_HEIGHT, TEST_DISPLAY_HEIGHT + EXT_DISPLAY_HEIGHT).
    harness.mock_state.add_display(
        EXT_DISPLAY_ID,
        IRect::new(
            0,
            TEST_DISPLAY_HEIGHT,
            EXT_DISPLAY_WIDTH,
            TEST_DISPLAY_HEIGHT + EXT_DISPLAY_HEIGHT,
        ),
        vec![EXT_WORKSPACE_ID],
    );

    harness
        .with_windows(4)
        .on_iteration(commands.len() - 1, |world, _state| {
            for id in [0, 1] {
                let bottom = window_y(world, id) + window_height(world, id);
                assert!(
                    bottom <= TEST_DISPLAY_HEIGHT,
                    "parked window {id}'s frame must stay within TEST_DISPLAY's \
                     own bounds (bottom <= {TEST_DISPLAY_HEIGHT}), not leak onto \
                     the display below — got bottom={bottom}"
                );
            }
        })
        .run(commands);
}

/// Regression: `detect_moved_windows` must ignore a spurious workspace move
/// reported for a parked tab instead of splitting it from its group.
#[test]
fn test_detect_moved_windows_ignores_a_spurious_report_for_a_parked_tab() {
    let mut commands = vec![Event::MenuOpened { window_id: 0 }];
    stack_first_three(&mut commands);
    commands.push(Event::Command {
        command: Command::Window(Operation::ToggleTabbedDisplay),
    });
    // Window 2 is active; North switches to window 1, parking window 2.
    commands.push(Event::Command {
        command: Command::Window(Operation::Focus(Direction::North)),
    });

    let mut harness = TestHarness::new().with_windows(4);
    harness.run(commands);

    // The OS misattributes the parked tab to another workspace.
    harness
        .mock_state
        .update_window(2, |w| w.workspace_id = EXT_WORKSPACE_ID);

    let world = harness.world();
    activate_new_workspace(world, EXT_WORKSPACE_ID);
    world
        .run_system_once(crate::ecs::workspace::detect_moved_windows)
        .expect("the system should run");

    let entity2 = find_window_entity(2, world);
    let mut strips = world.query::<&LayoutStrip>();
    let original_strip = strips
        .iter(world)
        .find(|strip| strip.id() == TEST_WORKSPACE_ID)
        .expect("the original strip should still exist");
    assert!(
        original_strip.contains(entity2),
        "a spurious move report for a parked tab must not pull it out of its stack"
    );
    assert!(
        original_strip.is_tabbed_display(entity2),
        "the stack must still be in tabbed display"
    );
    let other_strip = strips
        .iter(world)
        .find(|strip| strip.id() == EXT_WORKSPACE_ID)
        .expect("the destination strip should still exist");
    assert!(
        !other_strip.contains(entity2),
        "the spuriously-reported workspace must not have adopted the parked tab"
    );
}

/// Spawns an empty strip for `workspace_id` and moves `ActiveWorkspaceMarker`
/// to it, as a physical workspace switch would (`detect_moved_windows` needs
/// exactly one newly-added marker).
fn activate_new_workspace(world: &mut World, workspace_id: crate::platform::WorkspaceId) {
    let previous = world
        .query_filtered::<Entity, With<crate::ecs::ActiveWorkspaceMarker>>()
        .single(world)
        .expect("one active workspace before the switch");
    world
        .entity_mut(previous)
        .remove::<crate::ecs::ActiveWorkspaceMarker>();
    world.spawn((
        LayoutStrip::new(workspace_id, 0),
        crate::ecs::ActiveWorkspaceMarker,
    ));
}

/// Switching virtual workspace and back leaves a tabbed group intact,
/// including its active tab.
#[test]
fn test_switching_virtual_workspace_and_back_leaves_the_tabbed_group_intact() {
    let mut commands = vec![Event::MenuOpened { window_id: 0 }];
    stack_first_three(&mut commands);
    commands.push(Event::Command {
        command: Command::Window(Operation::ToggleTabbedDisplay),
    });
    commands.push(Event::Command {
        command: Command::Window(Operation::VirtualAdd),
    });
    commands.push(Event::Command {
        command: Command::Window(Operation::Virtual(Direction::First)),
    });

    TestHarness::new()
        .with_windows(4)
        .on_iteration(commands.len() - 1, |world, _state| {
            let entity0 = find_window_entity(0, world);
            let entity1 = find_window_entity(1, world);
            let entity2 = find_window_entity(2, world);
            let mut strips = world.query::<&LayoutStrip>();
            let strip = strips
                .iter(world)
                .find(|strip| strip.id() == TEST_WORKSPACE_ID && strip.contains(entity2))
                .expect("the tabbed stack should still exist on its workspace");
            assert!(
                strip.is_tabbed_display(entity2),
                "switching virtual workspace and back must not un-tab the stack"
            );
            assert_eq!(
                strip.active_tab(entity2),
                Some(entity2),
                "the active tab must survive an unrelated virtual workspace switch"
            );
            assert!(
                strip.contains(entity0) && strip.contains(entity1),
                "no group member should be left behind by an unrelated workspace switch"
            );
        })
        .run(commands);
}

/// Cycling tabs with `Focus` North/South parks the previously active tab.
#[test]
fn test_focus_north_parks_the_previously_active_tab() {
    let mut commands = vec![Event::MenuOpened { window_id: 0 }];
    stack_first_three(&mut commands);
    commands.push(Event::Command {
        command: Command::Window(Operation::ToggleTabbedDisplay),
    });
    // Window 2 is active; North switches to window 1.
    commands.push(Event::Command {
        command: Command::Window(Operation::Focus(Direction::North)),
    });
    let after_north = commands.len() - 1;

    TestHarness::new()
        .with_windows(4)
        .on_iteration(after_north, |world, _state| {
            let parked_x0 = window_x(world, 0);
            let parked_x2 = window_x(world, 2);

            assert_eq!(
                window_y(world, 1),
                TEST_MENUBAR_HEIGHT,
                "window 1 must now be the on-screen active tab"
            );
            assert!(
                parked_x0 + TEST_WINDOW_WIDTH > TEST_DISPLAY_WIDTH,
                "window 0 must stay parked off past the display's right edge"
            );
            assert!(
                parked_x2 + TEST_WINDOW_WIDTH > TEST_DISPLAY_WIDTH,
                "the previously-active window 2 must be parked off past the \
                 display's right edge, not left on-screen"
            );
        })
        .run(commands);
}

/// Regression: tab switches must still swap windows while a
/// `ManualStripOffset` (from `Operation::Center`) pins the strip.
#[test]
fn test_switching_tabs_after_centering_the_column_still_updates_the_window() {
    let commands = vec![
        Event::MenuOpened { window_id: 0 },
        Event::Command {
            command: Command::Window(Operation::Focus(Direction::East)),
        },
        Event::Command {
            command: Command::Window(Operation::Stack(true)),
        },
        Event::Command {
            command: Command::Window(Operation::ToggleTabbedDisplay),
        },
        // Window 1 is active. Centering pins a `ManualStripOffset`.
        Event::Command {
            command: Command::Window(Operation::Center),
        },
        // Switch to window 0 while pinned.
        Event::Command {
            command: Command::Window(Operation::Focus(Direction::North)),
        },
    ];
    let after_switch = commands.len() - 1;

    TestHarness::new()
        .with_windows(2)
        .on_iteration(after_switch, |world, _state| {
            assert_eq!(
                window_y(world, 0),
                TEST_MENUBAR_HEIGHT,
                "window 0 must now be the on-screen active tab, even though \
                 centering pinned the strip's position"
            );
            assert!(
                window_x(world, 1) + TEST_WINDOW_WIDTH > TEST_DISPLAY_WIDTH,
                "window 1 must be parked off past the display's right edge, \
                 not still showing on screen"
            );
        })
        .run(commands);
}

/// Regression: a tab switch must not reshuffle the strip away and back
/// (from reading the new tab's stale, parked `Position`).
#[test]
fn test_switching_tabs_never_reshuffles_the_strip_to_a_bogus_position() {
    // Needs auto-center on: with it off, an unrelated edge-invariant clamp
    // masks the bug.
    let config: Config = (
        MainOptions {
            auto_center: Some(true),
            ..Default::default()
        },
        vec![],
    )
        .into();

    let mut commands = vec![Event::MenuOpened { window_id: 0 }];
    stack_first_three(&mut commands);
    commands.push(Event::Command {
        command: Command::Window(Operation::ToggleTabbedDisplay),
    });

    let mut h = TestHarness::new().with_config(config).with_windows(4);
    h.run(commands);

    let strip_entity = {
        let mut query = h.world().query::<(Entity, &LayoutStrip)>();
        query
            .iter(h.world())
            .map(|(entity, _)| entity)
            .next()
            .expect("a strip")
    };
    let correct_position = h
        .world()
        .get::<crate::ecs::Position>(strip_entity)
        .expect("strip position")
        .0;

    // Window 2 is active; North switches to window 1.
    h.app.world_mut().write_message::<Event>(Event::Command {
        command: Command::Window(Operation::Focus(Direction::North)),
    });
    for _ in 0..8 {
        h.app.update();
        for e in h.mock_state.drain_events() {
            h.app.world_mut().write_message::<Event>(e);
        }
        // Checked every tick: the jiggle is transient.
        if let Some(target) = h.world().get::<RepositionMarker>(strip_entity).map(|m| m.0) {
            assert_eq!(
                target, correct_position,
                "the strip must never be commanded to reshuffle away from its \
                 already-correct position while a tab switch is in progress"
            );
        }
        let actual_position = h
            .world()
            .get::<crate::ecs::Position>(strip_entity)
            .expect("strip position")
            .0;
        assert_eq!(
            actual_position, correct_position,
            "the strip must never actually move away from its already-correct \
             position while a tab switch is in progress"
        );
    }
}

/// Regression: `Focus(First/Last/Nth)` into a tabbed column lands on the
/// active tab, not the column's `top()`.
#[test]
fn test_jumping_to_a_tabbed_column_lands_on_the_active_tab_not_the_first_item() {
    let mut commands = vec![Event::MenuOpened { window_id: 0 }];
    stack_first_three(&mut commands);
    commands.push(Event::Command {
        command: Command::Window(Operation::ToggleTabbedDisplay),
    });
    // Make window 1 active, then leave so `Focus(First)` jumps back in.
    commands.push(Event::Command {
        command: Command::Window(Operation::Focus(Direction::North)),
    });
    commands.push(Event::Command {
        command: Command::Window(Operation::Focus(Direction::East)),
    });
    commands.push(Event::Command {
        command: Command::Window(Operation::Focus(Direction::First)),
    });
    let after_first = commands.len() - 1;

    TestHarness::new()
        .with_windows(4)
        .on_iteration(after_first, |world, _state| {
            assert_focused!(world, 1);
            assert!(
                is_tabbed_display(world, 1),
                "the stack must still be in tabbed display"
            );
        })
        .run(commands);
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

/// Regression: re-entering a tabbed column from the side lands on its
/// active tab, not the row-index match.
#[test]
fn test_reentering_a_tabbed_column_from_the_side_preserves_the_active_tab() {
    let mut commands = vec![Event::MenuOpened { window_id: 0 }];
    stack_first_three(&mut commands);
    commands.push(Event::Command {
        command: Command::Window(Operation::ToggleTabbedDisplay),
    });
    // Window 2 is active; switch to window 1 before leaving.
    commands.push(Event::Command {
        command: Command::Window(Operation::Focus(Direction::North)),
    });
    // Leave to window 3's column...
    commands.push(Event::Command {
        command: Command::Window(Operation::Focus(Direction::East)),
    });
    let after_leaving = commands.len() - 1;
    // ...then come straight back from the side.
    commands.push(Event::Command {
        command: Command::Window(Operation::Focus(Direction::West)),
    });
    let after_returning = commands.len() - 1;

    TestHarness::new()
        .with_windows(4)
        .on_iteration(after_leaving, |world, _state| {
            assert_focused!(world, 3);
        })
        .on_iteration(after_returning, |world, _state| {
            assert_focused!(world, 1);
            assert!(
                is_tabbed_display(world, 1),
                "the stack must still be in tabbed display"
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
    // Window 2 is active; North twice reaches window 0. (`Direction::First`
    // would land on the active tab, not window 0.)
    commands.push(Event::Command {
        command: Command::Window(Operation::Focus(Direction::North)),
    });
    commands.push(Event::Command {
        command: Command::Window(Operation::Focus(Direction::North)),
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

/// Tab switches never animate; a slow `animation_speed` makes any
/// animation detectable.
#[test]
fn test_tab_switch_never_animates_even_with_slow_animation_speed() {
    let config: Config = (
        MainOptions {
            animation_speed: Some(3.0),
            ..Default::default()
        },
        vec![],
    )
        .into();

    let mut h = TestHarness::new().with_config(config).with_windows(4);
    let pump = |h: &mut TestHarness, c: Command| {
        h.app
            .world_mut()
            .write_message::<Event>(Event::Command { command: c });
        for _ in 0..8 {
            h.app.update();
            for e in h.mock_state.drain_events() {
                h.app.world_mut().write_message::<Event>(e);
            }
        }
    };
    // Only tab members (0-2): the strip and window 3 may still be settling
    // from the stacking commands.
    let tab_members_animating = |h: &mut TestHarness| -> Vec<i32> {
        let world = h.app.world_mut();
        let mut query = world.query_filtered::<&crate::manager::Window, With<RepositionMarker>>();
        query
            .iter(world)
            .map(|w| w.id())
            .filter(|id| (0..=2).contains(id))
            .collect()
    };

    pump(&mut h, Command::PrintState);
    pump(&mut h, Command::Window(Operation::Focus(Direction::East)));
    pump(&mut h, Command::Window(Operation::Stack(true)));
    pump(&mut h, Command::Window(Operation::Focus(Direction::East)));
    pump(&mut h, Command::Window(Operation::Stack(true)));

    pump(&mut h, Command::Window(Operation::ToggleTabbedDisplay));
    assert_eq!(
        tab_members_animating(&mut h),
        Vec::<i32>::new(),
        "toggling tabbed display must never leave a RepositionMarker on a \
         tab member, even with a slow animation_speed"
    );

    pump(&mut h, Command::Window(Operation::Focus(Direction::First)));
    assert_eq!(
        tab_members_animating(&mut h),
        Vec::<i32>::new(),
        "switching the active tab must never leave a RepositionMarker on a \
         tab member, even with a slow animation_speed"
    );
}

#[test]
fn test_toggling_tabs_on_and_off_while_centered_updates_top_item_position() {
    let commands = vec![
        Event::MenuOpened { window_id: 0 },
        Event::Command {
            command: Command::Window(Operation::Focus(Direction::East)),
        },
        Event::Command {
            command: Command::Window(Operation::Stack(true)),
        },
        // Centering pins a `ManualStripOffset` while the stack is still split.
        Event::Command {
            command: Command::Window(Operation::Center),
        },
        // Toggle tabs on with window 1 focused; window 0 (at y = 0) must be parked.
        Event::Command {
            command: Command::Window(Operation::ToggleTabbedDisplay),
        },
        // Toggle tabs back off while still centered; window 0 must return on-screen.
        Event::Command {
            command: Command::Window(Operation::ToggleTabbedDisplay),
        },
    ];

    TestHarness::new()
        .with_windows(2)
        .on_iteration(4, |world, _state| {
            assert!(
                window_x(world, 0) + TEST_WINDOW_WIDTH > TEST_DISPLAY_WIDTH,
                "window 0 must be parked off-screen when toggling tabs on while centered"
            );
        })
        .on_iteration(5, |world, _state| {
            assert_eq!(
                window_x(world, 0),
                window_x(world, 1),
                "window 0 must return to the column x when toggling tabs off while centered"
            );
        })
        .run(commands);
}

#[test]
fn test_demoting_tabbed_stack_to_native_tabs_clears_tabbed_display() {
    let mut world = World::new();
    let entities = world.spawn_batch(vec![(), (), ()]).collect::<Vec<Entity>>();

    let mut strip = LayoutStrip::default();
    for &entity in &entities {
        strip.append(entity);
    }
    strip.convert_to_tabs(entities[0], entities[1]).unwrap();
    strip.stack(entities[2]).unwrap();
    assert_eq!(strip.toggle_tabbed_display(entities[2]), Some(true));

    strip.remove(entities[2]);
    assert!(
        !strip.is_tabbed_display(entities[0]) && !strip.is_tabbed_display(entities[1]),
        "a single surviving native-tab item demoted to Column::Tabs must not stay in tabbed_stacks"
    );
    assert!(
        !strip.is_parked_tab(entities[0]) && !strip.is_parked_tab(entities[1]),
        "surviving native tabs must not be marked as parked tabbed-display members"
    );
}

/// `Event::TabIndicatorClicked` focuses the clicked window, makes it the
/// group's active tab, and leaves the mouse cursor at the indicator instead of
/// warping it to the window center.
#[test]
fn test_tab_indicator_click_focuses_and_activates_the_clicked_tab() {
    let mut commands = vec![Event::MenuOpened { window_id: 0 }];
    stack_first_three(&mut commands);
    commands.push(Event::Command {
        command: Command::Window(Operation::ToggleTabbedDisplay),
    });
    let before_click = commands.len() - 1;
    // Window 2 is active; click window 0 so ordering can't explain the result.
    commands.push(Event::TabIndicatorClicked { window_id: 0 });
    let after_click = commands.len() - 1;

    let indicator_cursor = crate::manager::Origin::new(5, 100);
    TestHarness::new()
        .with_windows(4)
        .on_iteration(before_click, move |world, _state| {
            world
                .resource::<crate::manager::WindowManager>()
                .warp_mouse(indicator_cursor);
        })
        .on_iteration(after_click, move |world, state| {
            assert_focused!(world, 0);
            assert!(
                is_tabbed_display(world, 0),
                "the stack should still be in tabbed display after the click"
            );
            assert_eq!(
                window_y(world, 0),
                TEST_MENUBAR_HEIGHT,
                "the clicked tab must be the one on-screen"
            );
            assert!(
                window_x(world, 2) + TEST_WINDOW_WIDTH > TEST_DISPLAY_WIDTH,
                "the previously-active tab must be parked off past the \
                 display's right edge, not left on-screen"
            );
            assert_eq!(
                state.cursor_position(),
                indicator_cursor,
                "clicking the tab indicator must not warp the pointer to the window center"
            );
        })
        .run(commands);
}

/// Centering (or otherwise scrolling) a strip with a tabbed stack animates the
/// active on-screen tab smoothly into place instead of teleporting it on the
/// first frame, while inactive parked tabs stay snapped off-screen.
#[test]
fn test_centering_a_tabbed_stack_animates_the_active_tab_into_place() {
    let config: Config = (
        MainOptions {
            animation_speed: Some(3.0),
            ..Default::default()
        },
        vec![],
    )
        .into();

    let mut commands = vec![Event::MenuOpened { window_id: 0 }];
    stack_first_three(&mut commands);
    commands.push(Event::Command {
        command: Command::Window(Operation::ToggleTabbedDisplay),
    });

    let mut h = TestHarness::new().with_config(config).with_windows(4);
    h.run(commands);

    // Issue `Center` and advance two frames so `position_layout_windows` reacts
    // to the strip's new `RepositionMarker`.
    h.app.world_mut().write_message::<Event>(Event::Command {
        command: Command::Window(Operation::Center),
    });
    h.app.update();
    h.app.update();

    let active_entity = find_window_entity(2, h.world());
    let parked_0 = find_window_entity(0, h.world());
    let parked_1 = find_window_entity(1, h.world());
    assert!(
        h.world().get::<RepositionMarker>(active_entity).is_some(),
        "the active tab must animate into its centered position with the strip, not jump immediately"
    );
    assert!(
        h.world().get::<RepositionMarker>(parked_0).is_none()
            && h.world().get::<RepositionMarker>(parked_1).is_none(),
        "inactive parked tabs must remain snapped off-screen without animating"
    );
}

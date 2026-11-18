use std::time::Duration;

use bevy::ecs::hierarchy::ChildOf;
use bevy::ecs::query::Has;
use bevy::math::IRect;

use super::{
    ConfirmCondition, DESKTOP_CONFIRM_BUDGET, DesktopCensus, PendingDesktopOp, PendingDesktopOps,
    PinnedDesktopWindow, PostConfirmAction, StickyWindowMarker,
};
use crate::commands::{
    Command, DesktopAssignTarget, DesktopOperation, DesktopPinTarget, DesktopTarget, Direction,
    MoveFocus,
};
use crate::config::Config;
use crate::ecs::layout::LayoutStrip;
use crate::ecs::{ActiveWorkspaceMarker, SelectedVirtualMarker, SpawnWindowTrigger, Unmanaged};
use crate::events::Event;
use crate::manager::spaces::{DesktopBridge, SpaceId};
use crate::manager::{Display, Window};
use crate::tests::{
    EXT_DISPLAY_HEIGHT, EXT_DISPLAY_ID, EXT_DISPLAY_WIDTH, EXT_WORKSPACE_ID, TEST_DISPLAY_WIDTH,
    TEST_WORKSPACE_ID, TestHarness, find_window_entity,
};

fn tick() -> Event {
    Event::Command {
        command: Command::PrintState,
    }
}

#[test]
fn space_current_changed_switches_active_workspace_immediately() {
    let second_space = 3u64;
    let harness = TestHarness::new()
        .with_workspace_window(10, TEST_WORKSPACE_ID, |_| {})
        .with_workspace_window(20, second_space, |_| {});
    harness
        .mock_state
        .add_space_on_display(1, second_space, false);

    let commands = vec![
        tick(),
        tick(),
        Event::SpaceCurrentChanged {
            space_id: second_space,
        },
    ];

    harness
        .on_iteration(1, move |world, state| {
            // Simulate CGS `managed_display_set_current_space` updating the display's
            // current space before `Event::SpaceCurrentChanged` lands on iteration 2.
            state.activate_workspace(1, second_space, false);
            let active_ws: Vec<u64> = world
                .query::<(&LayoutStrip, Has<ActiveWorkspaceMarker>)>()
                .iter(world)
                .filter_map(|(s, active)| active.then_some(s.id()))
                .collect();
            assert_eq!(active_ws, [TEST_WORKSPACE_ID]);
        })
        .on_iteration(2, move |world, _| {
            let active_ws: Vec<u64> = world
                .query::<(&LayoutStrip, Has<ActiveWorkspaceMarker>)>()
                .iter(world)
                .filter_map(|(s, active)| active.then_some(s.id()))
                .collect();
            assert_eq!(active_ws, [second_space]);
            assert_eq!(
                world.resource::<DesktopCensus>().active_space(1),
                Some(second_space)
            );
        })
        .run(commands);
}

#[test]
fn space_created_on_secondary_display_parents_strip_to_owning_display() {
    let new_ext_space = 21u64;
    let harness = TestHarness::new()
        .with_display(
            EXT_DISPLAY_ID,
            IRect::new(
                TEST_DISPLAY_WIDTH,
                0,
                TEST_DISPLAY_WIDTH + EXT_DISPLAY_WIDTH,
                EXT_DISPLAY_HEIGHT,
            ),
            vec![EXT_WORKSPACE_ID],
        )
        .with_workspace_window(10, TEST_WORKSPACE_ID, |_| {})
        .with_workspace_window(20, EXT_WORKSPACE_ID, |_| {});

    let commands = vec![tick(), tick()];

    harness
        .on_iteration(0, move |_world, state| {
            // Space 21 is created on secondary display 2 while display 1 holds focus.
            state.add_space_on_display(EXT_DISPLAY_ID, new_ext_space, true);
        })
        .on_iteration(1, move |world, _| {
            let mut strips = world.query::<(&LayoutStrip, &ChildOf, Has<SelectedVirtualMarker>)>();
            let (parent_entity, selected) = strips
                .iter(world)
                .find_map(|(strip, parent, sel)| {
                    (strip.id() == new_ext_space).then_some((parent.parent(), sel))
                })
                .expect("strip for space 21 spawned");
            let owning_display = world
                .get::<Display>(parent_entity)
                .expect("strip parent is a Display")
                .id();
            assert_eq!(
                owning_display, EXT_DISPLAY_ID,
                "space created on secondary monitor must attach to secondary Display"
            );
            assert!(
                selected,
                "row 0 of new space must carry SelectedVirtualMarker"
            );
        })
        .run(commands);
}

#[test]
fn sticky_window_auto_floats_and_is_not_relocated_across_space_switches() {
    let second_space = 3u64;
    let harness = TestHarness::new()
        .with_workspace_window(10, TEST_WORKSPACE_ID, |_| {})
        .with_workspace_window(20, second_space, |_| {})
        .with_workspace_window(99, TEST_WORKSPACE_ID, move |data| {
            data.sticky = true;
            data.extra_workspaces.insert(second_space);
        });
    harness
        .mock_state
        .add_space_on_display(1, second_space, false);

    let commands = vec![
        tick(),
        tick(),
        Event::SpaceCurrentChanged {
            space_id: second_space,
        },
    ];

    harness
        .on_iteration(1, move |world, state| {
            let (is_sticky, is_floating) = world
                .query::<(&Window, Has<StickyWindowMarker>, Option<&Unmanaged>)>()
                .iter(world)
                .find_map(|(w, sticky, unmanaged)| {
                    (w.id() == 99)
                        .then_some((sticky, matches!(unmanaged, Some(Unmanaged::Floating))))
                })
                .expect("window 99 exists");
            assert!(is_sticky, "0x800 window must carry StickyWindowMarker");
            assert!(is_floating, "0x800 window must auto-transition to Floating");
            crate::assert_not_on_workspace!(world, 99, TEST_WORKSPACE_ID);

            state.activate_workspace(1, second_space, false);
        })
        .on_iteration(2, move |world, _| {
            crate::assert_not_on_workspace!(world, 99, second_space);
            crate::assert_on_workspace!(world, 10, TEST_WORKSPACE_ID);
            crate::assert_on_workspace!(world, 20, second_space);
        })
        .run(commands);
}

#[test]
fn pending_desktop_op_confirms_deferred_mutation_and_expires_on_timeout() {
    let second_space = 3u64;
    let mut harness = TestHarness::new()
        .with_workspace_window(10, TEST_WORKSPACE_ID, |_| {})
        .with_workspace_window(20, second_space, |_| {});
    harness
        .mock_state
        .add_space_on_display(1, second_space, false);
    harness.run(vec![tick()]);

    // 1. Enqueue a deferred switch to `second_space`: stays pending until flushed.
    let target_sid = SpaceId::new(second_space).unwrap();
    harness.mock_state.set_defer_desktop_ops(true);
    {
        let world = harness.world();
        world
            .non_send_mut::<crate::manager::spaces::DesktopBridge>()
            .activate_space(
                "Main",
                target_sid,
                &[SpaceId::new(TEST_WORKSPACE_ID).unwrap()],
            )
            .unwrap();
        world
            .resource_mut::<PendingDesktopOps>()
            .push(PendingDesktopOp::new(
                "activate space 3",
                ConfirmCondition::SpaceActive(target_sid),
                Some(PostConfirmAction::ActivateAndFocusSpace {
                    space_id: target_sid,
                    preferred_window: None,
                }),
            ));
    }

    harness.advance(Duration::from_millis(120));
    assert!(
        !harness.world().resource::<PendingDesktopOps>().is_empty(),
        "op stays pending before WindowServer applies it"
    );

    harness.mock_state.flush_deferred_desktop_ops();
    harness.advance(Duration::from_millis(100));
    assert!(
        harness.world().resource::<PendingDesktopOps>().is_empty(),
        "op clears once census confirms target space active"
    );

    // 2. An impossible condition expires cleanly after `DESKTOP_CONFIRM_BUDGET`.
    let ghost = SpaceId::new(999).unwrap();
    harness
        .world()
        .resource_mut::<PendingDesktopOps>()
        .push(PendingDesktopOp::new(
            "impossible space 999",
            ConfirmCondition::SpaceActive(ghost),
            None,
        ));
    harness.advance(DESKTOP_CONFIRM_BUDGET + Duration::from_millis(100));
    assert!(
        harness.world().resource::<PendingDesktopOps>().is_empty(),
        "timed-out op cleans itself up after budget"
    );
}

fn desktop_cmd(op: DesktopOperation) -> Event {
    Event::Command {
        command: Command::Desktop(op),
    }
}

fn active_space_id(world: &mut bevy::ecs::world::World) -> u64 {
    world
        .query::<(&LayoutStrip, Has<ActiveWorkspaceMarker>)>()
        .iter(world)
        .find_map(|(s, active)| active.then_some(s.id()))
        .expect("one active workspace strip")
}

#[test]
fn desktop_focus_steps_across_desktops_and_fullscreen_and_clamps_edges() {
    let fs_space = 5u64;
    let desktop_2 = 6u64;
    let harness = TestHarness::new()
        .with_workspace_window(10, TEST_WORKSPACE_ID, |_| {})
        .with_workspace_window(11, TEST_WORKSPACE_ID, |_| {})
        .with_workspace_window(50, fs_space, |data| data.is_full_screen = true)
        .with_workspace_window(20, desktop_2, |_| {});
    harness.mock_state.add_space_on_display(1, fs_space, false);
    harness.mock_state.set_fullscreen_owner(fs_space, 50);
    harness.mock_state.add_space_on_display(1, desktop_2, false);

    let commands = vec![
        tick(), // 0: settle + focus window 11 on Desktop 1
        desktop_cmd(DesktopOperation::Focus(DesktopTarget::Direction {
            direction: Direction::West,
            display: None,
        })), // 1: left edge clamp -> stays on Desktop 1
        desktop_cmd(DesktopOperation::Focus(DesktopTarget::Direction {
            direction: Direction::East,
            display: None,
        })), // 2: steps into Fullscreen space 5
        desktop_cmd(DesktopOperation::Focus(DesktopTarget::Direction {
            direction: Direction::East,
            display: None,
        })), // 3: steps onto Desktop 2 (space 6)
        desktop_cmd(DesktopOperation::Focus(DesktopTarget::Direction {
            direction: Direction::East,
            display: None,
        })), // 4: right edge clamp -> stays on Desktop 2
        desktop_cmd(DesktopOperation::Focus(DesktopTarget::Number {
            number: 1,
            display: None,
        })), // 5: 1-based Desktop 1 restores remembered focus (11)
        desktop_cmd(DesktopOperation::Focus(DesktopTarget::Number {
            number: 2,
            display: None,
        })), // 6: 1-based Desktop 2 skips Fullscreen space 5 -> lands on space 6
    ];

    harness
        .on_iteration(0, |world, state| {
            state.focus_window(11);
            assert_eq!(active_space_id(world), TEST_WORKSPACE_ID);
        })
        .on_iteration(1, |world, _| {
            assert_eq!(active_space_id(world), TEST_WORKSPACE_ID);
            crate::assert_focused!(world, 11);
        })
        .on_iteration(2, move |world, _| {
            assert_eq!(
                world.resource::<DesktopCensus>().active_space(1),
                Some(fs_space)
            );
        })
        .on_iteration(3, move |world, _| {
            assert_eq!(active_space_id(world), desktop_2);
            crate::assert_focused!(world, 20);
        })
        .on_iteration(4, move |world, _| {
            assert_eq!(active_space_id(world), desktop_2);
        })
        .on_iteration(5, |world, _| {
            assert_eq!(active_space_id(world), TEST_WORKSPACE_ID);
            crate::assert_focused!(world, 11);
        })
        .on_iteration(6, move |world, _| {
            assert_eq!(active_space_id(world), desktop_2);
            crate::assert_focused!(world, 20);
        })
        .run(commands);
}

#[test]
fn desktop_send_and_move_transfer_windows_and_tab_groups() {
    let desktop_2 = 6u64;
    let harness = TestHarness::new()
        .with_workspace_window(10, TEST_WORKSPACE_ID, |_| {})
        .with_workspace_window(11, TEST_WORKSPACE_ID, |_| {})
        .with_workspace_window(12, TEST_WORKSPACE_ID, |_| {});
    harness.mock_state.add_space_on_display(1, desktop_2, false);

    let commands = vec![
        tick(), // 0: group 10+11 into a tabbed stack and focus 10
        tick(), // 1: settle focus on 10
        desktop_cmd(DesktopOperation::MoveWindow {
            target: DesktopTarget::Number {
                number: 2,
                display: None,
            },
            move_focus: MoveFocus::Stay,
        }), // 2: send tab group (10, 11) to Desktop 2 while staying on Desktop 1
        desktop_cmd(DesktopOperation::MoveWindow {
            target: DesktopTarget::Direction {
                direction: Direction::East,
                display: None,
            },
            move_focus: MoveFocus::Follow,
        }), // 3: move remaining window 12 to Desktop 2 and follow
    ];

    harness
        .on_iteration(0, |world, state| {
            let e10 = find_window_entity(10, world);
            let e11 = find_window_entity(11, world);
            for mut strip in world.query::<&mut LayoutStrip>().iter_mut(world) {
                if strip.id() == TEST_WORKSPACE_ID {
                    strip.remove(e11);
                    strip.convert_to_tabs(e10, e11).unwrap();
                }
            }
            state.focus_window(10);
        })
        .on_iteration(1, |world, _| {
            crate::assert_focused!(world, 10);
        })
        .on_iteration(2, move |world, _| {
            assert_eq!(active_space_id(world), TEST_WORKSPACE_ID);
            crate::assert_not_on_workspace!(world, 10, TEST_WORKSPACE_ID);
            crate::assert_not_on_workspace!(world, 11, TEST_WORKSPACE_ID);
            crate::assert_on_workspace!(world, 10, desktop_2);
            crate::assert_on_workspace!(world, 11, desktop_2);
            crate::assert_on_workspace!(world, 12, TEST_WORKSPACE_ID);
            crate::assert_focused!(world, 12);

            let bridge = world.non_send::<DesktopBridge>();
            let sid_2 = SpaceId::new(desktop_2).unwrap();
            assert_eq!(bridge.spaces_for_window(10).unwrap(), [sid_2]);
            assert_eq!(bridge.spaces_for_window(11).unwrap(), [sid_2]);
        })
        .on_iteration(3, move |world, _| {
            assert_eq!(active_space_id(world), desktop_2);
            crate::assert_on_workspace!(world, 12, desktop_2);
            crate::assert_focused!(world, 12);
        })
        .run(commands);
}

#[test]
fn fullscreen_guest_rescue_moves_unassociated_guest_and_refuses_owner_and_sheet() {
    let fs_space = 5u64;
    let harness = TestHarness::new()
        .with_workspace_window(10, TEST_WORKSPACE_ID, |_| {})
        .with_workspace_window(50, fs_space, |data| data.is_full_screen = true)
        .with_workspace_window(51, fs_space, |_| {})
        .with_workspace_window(52, fs_space, |_| {});
    harness.mock_state.add_space_on_display(1, fs_space, false);
    harness.mock_state.set_fullscreen_owner(fs_space, 50);
    harness.mock_state.set_associated_windows(52, vec![50, 52]);

    let to_desktop_1 = || {
        desktop_cmd(DesktopOperation::MoveWindow {
            target: DesktopTarget::Number {
                number: 1,
                display: None,
            },
            move_focus: MoveFocus::Follow,
        })
    };

    let commands = vec![
        tick(), // 0: activate fullscreen space 5 & focus owner 50
        Event::SpaceCurrentChanged { space_id: fs_space }, // 1: enter fs_space, focus 50
        to_desktop_1(), // 2: owner 50 refused -> focus associated sheet 52
        tick(), // 3: settle focus on 52
        to_desktop_1(), // 4: associated sheet 52 refused -> focus guest 51
        tick(), // 5: settle focus on 51
        to_desktop_1(), // 6: unassociated guest 51 rescued onto Desktop 1!
    ];

    harness
        .on_iteration(0, move |_world, state| {
            state.activate_workspace(1, fs_space, true);
            state.focus_window(50);
        })
        .on_iteration(2, move |world, state| {
            let sid_fs = SpaceId::new(fs_space).unwrap();
            assert_eq!(
                world
                    .non_send::<DesktopBridge>()
                    .spaces_for_window(50)
                    .unwrap(),
                [sid_fs],
                "fullscreen owner must never be moved"
            );
            state.focus_window(52);
        })
        .on_iteration(4, move |world, state| {
            let sid_fs = SpaceId::new(fs_space).unwrap();
            assert_eq!(
                world
                    .non_send::<DesktopBridge>()
                    .spaces_for_window(52)
                    .unwrap(),
                [sid_fs],
                "sheet associated with fullscreen owner must be refused"
            );
            state.focus_window(51);
        })
        .on_iteration(6, |world, _| {
            let sid_1 = SpaceId::new(TEST_WORKSPACE_ID).unwrap();
            assert_eq!(
                world
                    .non_send::<DesktopBridge>()
                    .spaces_for_window(51)
                    .unwrap(),
                [sid_1],
                "unassociated guest window must be rescued onto Desktop 1"
            );
            assert_eq!(active_space_id(world), TEST_WORKSPACE_ID);
            crate::assert_on_workspace!(world, 51, TEST_WORKSPACE_ID);
            crate::assert_focused!(world, 51);
        })
        .run(commands);
}

#[test]
fn desktop_create_without_and_with_focus_spawns_desktops() {
    let harness = TestHarness::new().with_workspace_window(10, TEST_WORKSPACE_ID, |_| {});

    let commands = vec![
        tick(),                                                 // 0: initial 1 desktop
        desktop_cmd(DesktopOperation::Create { focus: false }), // 1: create background desktop
        desktop_cmd(DesktopOperation::Create { focus: true }),  // 2: create & focus new desktop
    ];

    harness
        .on_iteration(0, |world, _| {
            assert_eq!(world.resource::<DesktopCensus>().desktop_count(1), 1);
            assert_eq!(active_space_id(world), TEST_WORKSPACE_ID);
        })
        .on_iteration(1, |world, _| {
            assert_eq!(world.resource::<DesktopCensus>().desktop_count(1), 2);
            assert_eq!(
                active_space_id(world),
                TEST_WORKSPACE_ID,
                "create without focus keeps current desktop active"
            );
        })
        .on_iteration(2, |world, _| {
            let census = world.resource::<DesktopCensus>();
            assert_eq!(census.desktop_count(1), 3);
            let third_id = census
                .0
                .display(1)
                .unwrap()
                .desktop_by_number(3)
                .unwrap()
                .id
                .get();
            assert_eq!(
                active_space_id(world),
                third_id,
                "create with focus activates the newly created desktop"
            );
        })
        .run(commands);
}

#[test]
fn desktop_destroy_handles_empty_only_active_close_migration_and_last_desktop_guard() {
    let desktop_2 = 6u64;
    let harness = TestHarness::new()
        .with_workspace_window(10, TEST_WORKSPACE_ID, |_| {})
        .with_workspace_window(20, desktop_2, |_| {});
    harness.mock_state.add_space_on_display(1, desktop_2, false);

    let commands = vec![
        tick(), // 0: settle
        desktop_cmd(DesktopOperation::Destroy {
            target: Some(DesktopTarget::Number {
                number: 2,
                display: None,
            }),
            empty_only: true,
        }), // 1: refused because window 20 is on Desktop 2
        desktop_cmd(DesktopOperation::Focus(DesktopTarget::Number {
            number: 2,
            display: None,
        })), // 2: switch to Desktop 2
        desktop_cmd(DesktopOperation::Destroy {
            target: None,
            empty_only: false,
        }), // 3: close active Desktop 2 -> switches to Desktop 1 & migrates window 20
        tick(), // 4: allow SpaceDestroyed Bookkeeping to despawn vacated strip
        desktop_cmd(DesktopOperation::Destroy {
            target: None,
            empty_only: false,
        }), // 5: refused by LastDesktop guard
    ];

    harness
        .on_iteration(1, |world, _| {
            assert_eq!(
                world.resource::<DesktopCensus>().desktop_count(1),
                2,
                "populated desktop must not be destroyed when empty_only = true"
            );
        })
        .on_iteration(2, move |world, _| {
            assert_eq!(active_space_id(world), desktop_2);
        })
        .on_iteration(4, move |world, _| {
            assert_eq!(world.resource::<DesktopCensus>().desktop_count(1), 1);
            assert_eq!(active_space_id(world), TEST_WORKSPACE_ID);
            crate::assert_on_workspace!(world, 10, TEST_WORKSPACE_ID);
            crate::assert_on_workspace!(world, 20, TEST_WORKSPACE_ID);
            let sid_1 = SpaceId::new(TEST_WORKSPACE_ID).unwrap();
            assert_eq!(
                world
                    .non_send::<DesktopBridge>()
                    .spaces_for_window(20)
                    .unwrap(),
                [sid_1]
            );
            let lingering = world
                .query::<&LayoutStrip>()
                .iter(world)
                .any(|s| s.id() == desktop_2);
            assert!(!lingering, "destroyed desktop strip must be despawned");
        })
        .on_iteration(5, |world, _| {
            assert_eq!(
                world.resource::<DesktopCensus>().desktop_count(1),
                1,
                "last desktop on display must never be destroyed"
            );
            assert_eq!(active_space_id(world), TEST_WORKSPACE_ID);
        })
        .run(commands);
}

#[test]
fn desktop_reorder_and_swap_permute_display_spaces_including_non_adjacent_two_step() {
    let harness = TestHarness::new().with_workspace_window(10, TEST_WORKSPACE_ID, |_| {});
    for ws in [6u64, 7, 8] {
        harness.mock_state.add_space_on_display(1, ws, false);
    }

    let space_order = |world: &bevy::ecs::world::World| -> Vec<u64> {
        world
            .resource::<DesktopCensus>()
            .0
            .display(1)
            .unwrap()
            .spaces
            .iter()
            .map(|s| s.id.get())
            .collect()
    };

    let commands = vec![
        tick(), // 0: initial [2, 6, 7, 8]
        desktop_cmd(DesktopOperation::Reorder(DesktopTarget::Number {
            number: 3,
            display: None,
        })), // 1: move active (2) to index 2 -> [6, 7, 2, 8]
        desktop_cmd(DesktopOperation::Swap(DesktopTarget::Number {
            number: 2,
            display: None,
        })), // 2: adjacent swap (index 2 <-> index 1) -> [6, 2, 7, 8]
        desktop_cmd(DesktopOperation::Swap(DesktopTarget::Number {
            number: 4,
            display: None,
        })), // 3: non-adjacent two-step swap (index 1 <-> index 3) -> [6, 8, 7, 2]
    ];

    harness
        .on_iteration(0, move |world, _| {
            assert_eq!(space_order(world), vec![TEST_WORKSPACE_ID, 6, 7, 8]);
        })
        .on_iteration(1, move |world, _| {
            assert_eq!(space_order(world), vec![6, 7, TEST_WORKSPACE_ID, 8]);
            assert_eq!(active_space_id(world), TEST_WORKSPACE_ID);
        })
        .on_iteration(2, move |world, _| {
            assert_eq!(space_order(world), vec![6, TEST_WORKSPACE_ID, 7, 8]);
            assert_eq!(active_space_id(world), TEST_WORKSPACE_ID);
        })
        .on_iteration(3, move |world, _| {
            assert_eq!(space_order(world), vec![6, 8, 7, TEST_WORKSPACE_ID]);
            assert_eq!(active_space_id(world), TEST_WORKSPACE_ID);
        })
        .run(commands);
}

#[test]
fn desktop_pin_all_auto_joins_future_desktops_and_unpin_collapses() {
    let desktop_2 = 6u64;
    let harness = TestHarness::new()
        .with_workspace_window(10, TEST_WORKSPACE_ID, |_| {})
        .with_workspace_window(11, TEST_WORKSPACE_ID, |_| {})
        .with_workspace_window(20, desktop_2, |_| {});
    harness.mock_state.add_space_on_display(1, desktop_2, false);

    let commands = vec![
        tick(),                                                    // 0: focus window 10
        tick(),                                                    // 1: settle focus on 10
        desktop_cmd(DesktopOperation::Pin(DesktopPinTarget::All)), // 2: pin 10 to all Desktops
        desktop_cmd(DesktopOperation::Create { focus: false }),    // 3: create Desktop 3
        tick(), // 4: auto-join pinned window 10 onto Desktop 3
        desktop_cmd(DesktopOperation::Focus(DesktopTarget::Number {
            number: 2,
            display: None,
        })), // 5: switch to Desktop 2
        tick(), // 6: focus floating pinned window 10 on Desktop 2
        desktop_cmd(DesktopOperation::Unpin), // 7: collapse 10 onto Desktop 2
    ];

    harness
        .on_iteration(0, |_world, state| {
            state.focus_window(10);
        })
        .on_iteration(2, move |world, _| {
            let e10 = find_window_entity(10, world);
            assert_eq!(
                world.get::<PinnedDesktopWindow>(e10).copied(),
                Some(PinnedDesktopWindow::AllDesktops { display_id: 1 })
            );
            assert!(world.get::<StickyWindowMarker>(e10).is_some());
            assert!(matches!(
                world.get::<Unmanaged>(e10),
                Some(Unmanaged::Floating)
            ));
            crate::assert_not_on_workspace!(world, 10, TEST_WORKSPACE_ID);
            let memberships = world
                .non_send::<DesktopBridge>()
                .spaces_for_window(10)
                .unwrap();
            assert_eq!(
                memberships,
                vec![
                    SpaceId::new(TEST_WORKSPACE_ID).unwrap(),
                    SpaceId::new(desktop_2).unwrap(),
                ]
            );
        })
        .on_iteration(4, move |world, _| {
            let third_sid = world
                .resource::<DesktopCensus>()
                .0
                .display(1)
                .unwrap()
                .desktop_by_number(3)
                .unwrap()
                .id;
            let memberships = world
                .non_send::<DesktopBridge>()
                .spaces_for_window(10)
                .unwrap();
            assert!(
                memberships.contains(&third_sid),
                "AllDesktops pinned window must automatically join newly created Desktop"
            );
        })
        .on_iteration(5, |_world, state| {
            state.focus_window(10);
        })
        .on_iteration(7, move |world, _| {
            let e10 = find_window_entity(10, world);
            assert!(world.get::<PinnedDesktopWindow>(e10).is_none());
            assert!(world.get::<StickyWindowMarker>(e10).is_none());
            assert_eq!(
                world
                    .non_send::<DesktopBridge>()
                    .spaces_for_window(10)
                    .unwrap(),
                vec![SpaceId::new(desktop_2).unwrap()],
                "unpin collapses memberships to the active Desktop"
            );
        })
        .run(commands);
}

#[test]
fn desktop_assign_all_and_clear_toggle_process_wide_sticky_assignment() {
    let desktop_2 = 6u64;
    let harness = TestHarness::new().with_workspace_window(10, TEST_WORKSPACE_ID, |_| {});
    harness.mock_state.add_space_on_display(1, desktop_2, false);

    let commands = vec![
        tick(), // 0: focus 10
        tick(), // 1: settle focus
        desktop_cmd(DesktopOperation::Assign {
            target: DesktopAssignTarget::All,
            pid: None,
        }), // 2: assign pid to All Desktops
        desktop_cmd(DesktopOperation::Focus(DesktopTarget::Number {
            number: 2,
            display: None,
        })), // 3: switch to Desktop 2
        tick(), // 4: focus 10 on Desktop 2
        desktop_cmd(DesktopOperation::Assign {
            target: DesktopAssignTarget::None,
            pid: None,
        }), // 5: clear assignment -> collapses onto active Desktop 2
    ];

    harness
        .on_iteration(0, |_world, state| {
            state.focus_window(10);
        })
        .on_iteration(2, move |world, _| {
            let e10 = find_window_entity(10, world);
            assert!(world.get::<StickyWindowMarker>(e10).is_some());
            assert!(matches!(
                world.get::<Unmanaged>(e10),
                Some(Unmanaged::Floating)
            ));
            let bridge = world.non_send::<DesktopBridge>();
            assert_eq!(bridge.window_is_sticky(10), Some(true));
            assert_eq!(
                bridge.spaces_for_window(10).unwrap(),
                vec![
                    SpaceId::new(TEST_WORKSPACE_ID).unwrap(),
                    SpaceId::new(desktop_2).unwrap(),
                ]
            );
        })
        .on_iteration(3, |_world, state| {
            state.focus_window(10);
        })
        .on_iteration(5, move |world, _| {
            let e10 = find_window_entity(10, world);
            assert!(
                world.get::<StickyWindowMarker>(e10).is_none(),
                "clearing process assignment clears StickyWindowMarker"
            );
            let bridge = world.non_send::<DesktopBridge>();
            assert_eq!(bridge.window_is_sticky(10), Some(false));
            assert_eq!(
                bridge.spaces_for_window(10).unwrap(),
                vec![SpaceId::new(desktop_2).unwrap()]
            );
        })
        .run(commands);
}

#[test]
fn window_desktop_rules_send_or_pin_newly_spawned_windows() {
    let desktop_2 = 6u64;
    let cfg = Config::try_from(
        r#"
[windows.slack]
title = "Slack"
desktop = 2

[windows.pip]
title = "PiP"
desktop = "all"
"#,
    )
    .unwrap();
    let harness =
        TestHarness::new()
            .with_config(cfg)
            .with_workspace_window(10, TEST_WORKSPACE_ID, |_| {});
    harness.mock_state.add_space_on_display(1, desktop_2, false);

    let commands = vec![
        tick(), // 0: spawn Slack (80) and PiP (81) at runtime
        tick(), // 1: verify placement
    ];

    harness
        .on_iteration(0, |world, state| {
            let w_slack = state.spawn_window(
                crate::tests::TEST_PROCESS_ID,
                TEST_WORKSPACE_ID,
                80,
                IRect::new(0, 0, 400, 400),
            );
            state.update_window(80, |d| d.title = "Slack".to_string());
            let w_pip = state.spawn_window(
                crate::tests::TEST_PROCESS_ID,
                TEST_WORKSPACE_ID,
                81,
                IRect::new(50, 50, 350, 250),
            );
            state.update_window(81, |d| d.title = "PiP".to_string());
            world.trigger(SpawnWindowTrigger(vec![w_slack, w_pip]));
        })
        .on_iteration(1, move |world, _| {
            assert_eq!(active_space_id(world), TEST_WORKSPACE_ID);
            crate::assert_not_on_workspace!(world, 80, TEST_WORKSPACE_ID);
            crate::assert_on_workspace!(world, 80, desktop_2);
            crate::assert_not_on_workspace!(world, 81, TEST_WORKSPACE_ID);
            crate::assert_not_on_workspace!(world, 81, desktop_2);

            let e_pip = find_window_entity(81, world);
            assert_eq!(
                world.get::<PinnedDesktopWindow>(e_pip).copied(),
                Some(PinnedDesktopWindow::AllDesktops { display_id: 1 })
            );
            assert!(matches!(
                world.get::<Unmanaged>(e_pip),
                Some(Unmanaged::Floating)
            ));

            let bridge = world.non_send::<DesktopBridge>();
            assert_eq!(
                bridge.spaces_for_window(80).unwrap(),
                vec![SpaceId::new(desktop_2).unwrap()]
            );
            assert_eq!(
                bridge.spaces_for_window(81).unwrap(),
                vec![
                    SpaceId::new(TEST_WORKSPACE_ID).unwrap(),
                    SpaceId::new(desktop_2).unwrap(),
                ]
            );
        })
        .run(commands);
}

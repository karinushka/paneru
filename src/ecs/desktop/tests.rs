use std::time::Duration;

use bevy::ecs::hierarchy::ChildOf;
use bevy::ecs::query::Has;
use bevy::math::IRect;

use super::{
    ConfirmCondition, DESKTOP_CONFIRM_BUDGET, DesktopCensus, PendingDesktopOp, PendingDesktopOps,
    PostConfirmAction, StickyWindowMarker,
};
use crate::commands::Command;
use crate::ecs::layout::LayoutStrip;
use crate::ecs::{ActiveWorkspaceMarker, SelectedVirtualMarker, Unmanaged};
use crate::events::Event;
use crate::manager::spaces::SpaceId;
use crate::manager::{Display, Window};
use crate::tests::{
    EXT_DISPLAY_HEIGHT, EXT_DISPLAY_ID, EXT_DISPLAY_WIDTH, EXT_WORKSPACE_ID, TEST_DISPLAY_WIDTH,
    TEST_WORKSPACE_ID, TestHarness,
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

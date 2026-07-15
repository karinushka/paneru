use std::time::{Duration, Instant};

use bevy::ecs::query::With;
use bevy::math::IRect;
use bevy::prelude::Entity;
use bevy::time::TimeUpdateStrategy;

use super::{Scrolling, smooth_native_scroll, sticky_edge_snap_target};
use crate::commands::Command;
use crate::ecs::{ActiveWorkspaceMarker, Position};
use crate::events::Event;
use crate::tests::TestHarness;

#[test]
fn native_scroll_smoothing_converges_without_overshoot() {
    let mut position = 0.0;
    let mut settled = false;
    for _ in 0..120 {
        let previous = position;
        (position, settled) = smooth_native_scroll(position, 100.0, 1.0 / 60.0);
        assert!(position >= previous);
        assert!(position <= 100.0);
        if settled {
            break;
        }
    }
    assert!((position - 100.0).abs() < f64::EPSILON);
    assert!(settled);
}

#[test]
fn sticky_scroll_snaps_only_inside_the_edge_hit_zone() {
    let viewport = IRect::new(0, 0, 1000, 800);
    let columns = [(0, 600), (600, 600)];
    assert_eq!(
        sticky_edge_snap_target(-631, &viewport, columns),
        Some(-600)
    );
    assert_eq!(
        sticky_edge_snap_target(-169, &viewport, columns),
        Some(-200)
    );
    assert_eq!(sticky_edge_snap_target(-591, &viewport, columns), None);
    assert_eq!(sticky_edge_snap_target(-167, &viewport, columns), None);
}

#[test]
fn sticky_scroll_exposes_both_edges_of_an_oversized_column() {
    let viewport = IRect::new(0, 0, 1000, 800);
    let column = [(0, 1500)];
    assert_eq!(sticky_edge_snap_target(-9, &viewport, column), Some(0));
    assert_eq!(sticky_edge_snap_target(-491, &viewport, column), Some(-500));
    assert_eq!(sticky_edge_snap_target(-250, &viewport, column), None);
}

#[test]
fn scrolling_component_is_removed_after_integer_effective_dead_zone() {
    let commands = vec![
        Event::MenuOpened { window_id: 0 },
        Event::Command {
            command: Command::PrintState,
        },
    ];
    TestHarness::new()
        .with_windows(3)
        .on_iteration(0, |world, _state| {
            world.insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_millis(
                16,
            )));
            let entity = {
                let mut query = world.query_filtered::<Entity, With<ActiveWorkspaceMarker>>();
                query.single(world).expect("one active workspace")
            };
            world.entity_mut(entity).insert(Scrolling {
                position: 0.0,
                target_position: Some(-1.0),
                last_event: Instant::now()
                    .checked_sub(Duration::from_millis(100))
                    .expect("100ms must fit before now"),
                ..Default::default()
            });
        })
        .on_iteration(1, |world, _state| {
            let mut scrolling = world.query_filtered::<&Scrolling, With<ActiveWorkspaceMarker>>();
            assert!(scrolling.single(world).is_err());
            let mut positions = world.query_filtered::<&Position, With<ActiveWorkspaceMarker>>();
            assert_eq!(positions.single(world).expect("one active workspace").x, -1);
        })
        .run(commands);
}

#[test]
fn explicit_touchpad_contact_is_not_ended_by_inactivity_fallback() {
    let commands = vec![
        Event::MenuOpened { window_id: 0 },
        Event::Command {
            command: Command::PrintState,
        },
        Event::TouchpadUp,
    ];
    TestHarness::new()
        .with_windows(1)
        .on_iteration(0, |world, _state| {
            let entity = {
                let mut query = world.query_filtered::<Entity, With<ActiveWorkspaceMarker>>();
                query.single(world).expect("one active workspace")
            };
            world.entity_mut(entity).insert(Scrolling {
                is_user_swiping: true,
                gesture_active: true,
                last_event: Instant::now()
                    .checked_sub(Duration::from_millis(100))
                    .expect("100ms must fit before now"),
                ..Default::default()
            });
        })
        .on_iteration(1, |world, _state| {
            let mut query = world.query_filtered::<&Scrolling, With<ActiveWorkspaceMarker>>();
            let scrolling = query
                .single(world)
                .expect("active contact keeps scrolling alive");
            assert!(scrolling.is_user_swiping);
            assert!(scrolling.gesture_active);
        })
        .on_iteration(2, |world, _state| {
            let mut query = world.query_filtered::<&Scrolling, With<ActiveWorkspaceMarker>>();
            assert!(query.single(world).is_err());
        })
        .run(commands);
}

#[test]
fn explicit_touchpad_begin_creates_lifecycle_state_before_first_delta() {
    let commands = vec![
        Event::MenuOpened { window_id: 0 },
        Event::TouchpadDown,
        Event::Command {
            command: Command::PrintState,
        },
        Event::TouchpadUp,
    ];
    TestHarness::new()
        .with_windows(1)
        .on_iteration(2, |world, _state| {
            let mut query = world.query_filtered::<&Scrolling, With<ActiveWorkspaceMarker>>();
            let scrolling = query
                .single(world)
                .expect("touch begin must create scrolling lifecycle state");
            assert!(scrolling.is_user_swiping);
            assert!(scrolling.gesture_active);
            assert!(scrolling.paging_gesture.is_some());
        })
        .run(commands);
}

#[test]
fn later_native_momentum_keeps_original_one_hop_paging_session() {
    let commands = vec![
        Event::MenuOpened { window_id: 0 },
        Event::Command {
            command: Command::Window(crate::commands::Operation::SetWidth(2.0)),
        },
        Event::TouchpadDown,
        Event::Scroll { delta: -100.0 },
        Event::TouchpadPhysicalUp,
        Event::TouchpadMomentumStart,
        Event::Scroll { delta: -100.0 },
        Event::TouchpadUp,
        Event::Command {
            command: Command::PrintState,
        },
    ];
    TestHarness::new()
        .with_windows(1)
        .on_iteration(2, |world, _state| {
            assert_original_oversized_paging_session(world);
        })
        .on_iteration(4, |world, _state| {
            let (_, _, _, gesture_active, is_user_swiping) = paging_snapshot(world);
            assert_original_oversized_paging_session(world);
            assert!(!gesture_active);
            assert!(is_user_swiping);
        })
        .on_iteration(5, |world, _state| {
            let (_, _, _, gesture_active, _) = paging_snapshot(world);
            assert_original_oversized_paging_session(world);
            assert!(gesture_active);
        })
        .on_iteration(6, |world, _state| {
            let (_, position, target_position, _, _) = paging_snapshot(world);
            assert_original_oversized_paging_session(world);
            assert!((-1024.0..=0.0).contains(&position));
            assert!(target_position.is_none_or(|target| (-1024.0..=0.0).contains(&target)));
        })
        .on_iteration(8, |world, _state| {
            let mut query = world.query_filtered::<&Position, With<ActiveWorkspaceMarker>>();
            let position = query.single(world).expect("active workspace").x;
            assert!(
                (-1024..=0).contains(&position),
                "native momentum must settle within the original one-hop bounds"
            );
        })
        .run(commands);
}

#[test]
fn touchpad_down_during_pending_settlement_does_not_recapture_stop() {
    let commands = vec![
        Event::MenuOpened { window_id: 0 },
        Event::Command {
            command: Command::Window(crate::commands::Operation::SetWidth(2.0)),
        },
        Event::TouchpadDown,
        Event::Scroll { delta: -100.0 },
        Event::TouchpadPhysicalUp,
        Event::TouchpadDown,
    ];
    TestHarness::new()
        .with_windows(1)
        .on_iteration(2, |world, _state| {
            assert_original_oversized_paging_session(world);
        })
        .on_iteration(5, |world, _state| {
            assert_original_oversized_paging_session(world);
            let (_, _, _, gesture_active, is_user_swiping) = paging_snapshot(world);
            assert!(gesture_active);
            assert!(is_user_swiping);
        })
        .run(commands);
}

#[test]
fn physical_up_and_momentum_start_in_same_update_leave_momentum_active() {
    let commands = vec![
        Event::MenuOpened { window_id: 0 },
        Event::Command {
            command: Command::Window(crate::commands::Operation::SetWidth(2.0)),
        },
        Event::TouchpadDown,
        Event::Scroll { delta: -100.0 },
        Event::Command {
            command: Command::PrintState,
        },
    ];
    TestHarness::new()
        .with_windows(1)
        .on_iteration(3, |world, _state| {
            assert_original_oversized_paging_session(world);
            world.write_message(Event::TouchpadPhysicalUp);
            world.write_message(Event::TouchpadMomentumStart);
        })
        .on_iteration(4, |world, _state| {
            assert_original_oversized_paging_session(world);
            let (_, _, _, gesture_active, is_user_swiping) = paging_snapshot(world);
            assert!(gesture_active, "momentum start must win over physical end");
            assert!(is_user_swiping);
        })
        .run(commands);
}

fn assert_original_oversized_paging_session(world: &mut bevy::prelude::World) {
    let (paging, _, _, _, _) = paging_snapshot(world);
    assert_eq!(paging.start_stop, -1024.0);
    assert_eq!(paging.previous_stop, Some(0.0));
    assert_eq!(paging.next_stop, None);
}

fn paging_snapshot(
    world: &mut bevy::prelude::World,
) -> (crate::ecs::PagingGesture, f64, Option<f64>, bool, bool) {
    let mut query = world.query_filtered::<&Scrolling, With<ActiveWorkspaceMarker>>();
    let scrolling = query
        .single(world)
        .expect("active workspace should be scrolling");
    (
        scrolling
            .paging_gesture
            .expect("paging session should remain captured"),
        scrolling.position,
        scrolling.target_position,
        scrolling.gesture_active,
        scrolling.is_user_swiping,
    )
}

//! Window pinning (spike). The mock harness has no `PinMirrorManager`, so
//! these cover the ECS side only: the toggle and the mirror lifecycle events.

use bevy::prelude::*;

use crate::commands::{Command, Operation};
use crate::ecs::pin::Pinned;
use crate::ecs::{FocusedMarker, Unmanaged};
use crate::events::Event;
use crate::manager::Window;

use super::*;

fn pin_command() -> Event {
    Event::Command {
        command: Command::Window(Operation::Pin),
    }
}

fn focused_id(world: &mut World) -> i32 {
    let mut query = world.query_filtered::<&Window, With<FocusedMarker>>();
    query.single(world).expect("one focused window").id()
}

fn is_pinned(world: &mut World, window_id: i32) -> bool {
    let entity = find_window_entity(window_id, world);
    world.get::<Pinned>(entity).is_some()
}

fn is_floating(world: &mut World, window_id: i32) -> bool {
    let entity = find_window_entity(window_id, world);
    matches!(world.get::<Unmanaged>(entity), Some(Unmanaged::Floating))
}

/// `Operation::Pin` pins the focused window, floating it if it was tiled, and
/// a second `Pin` unpins it while leaving it floating.
#[test]
fn test_pin_toggles_and_floats_focused_window() {
    let commands = vec![
        Event::MenuOpened { window_id: 0 }, // 0: warm-up
        pin_command(),                      // 1: pin
        pin_command(),                      // 2: unpin
    ];

    TestHarness::new()
        .with_windows(2)
        .on_iteration(1, |world, _state| {
            let id = focused_id(world);
            assert!(is_pinned(world, id), "focused window should be pinned");
            assert!(is_floating(world, id), "pinning a tiled window floats it");
        })
        .on_iteration(2, |world, _state| {
            let id = focused_id(world);
            assert!(!is_pinned(world, id), "second pin should unpin");
            assert!(
                is_floating(world, id),
                "unpinning leaves the window floating"
            );
        })
        .run(commands);
}

/// A stopped mirror (window closed, permission revoked) drops the pin instead
/// of leaving a pin with no mirror behind.
#[test]
fn test_pin_mirror_stopped_unpins() {
    let commands = vec![
        Event::MenuOpened { window_id: 0 }, // 0: warm-up
        pin_command(),                      // 1: pin the focused window
        Event::PinMirrorStopped { window_id: 0 },
        Event::PinMirrorStopped { window_id: 1 }, // 3: whichever was pinned is unpinned
    ];

    TestHarness::new()
        .with_windows(2)
        .on_iteration(1, |world, _state| {
            let id = focused_id(world);
            assert!(is_pinned(world, id), "precondition: focused window pinned");
        })
        .on_iteration(3, |world, _state| {
            assert!(!is_pinned(world, 0));
            assert!(!is_pinned(world, 1));
        })
        .run(commands);
}

/// Clicking a pinned window's mirror focuses the real window.
#[test]
fn test_pin_mirror_click_focuses_window() {
    let commands = vec![
        Event::MenuOpened { window_id: 0 },       // 0: warm-up
        Event::PinMirrorClicked { window_id: 1 }, // 1
        Event::PinMirrorClicked { window_id: 0 }, // 2
    ];

    TestHarness::new()
        .with_windows(2)
        .on_iteration(1, |world, _state| {
            assert_eq!(focused_id(world), 1);
        })
        .on_iteration(2, |world, _state| {
            assert_eq!(focused_id(world), 0);
        })
        .run(commands);
}

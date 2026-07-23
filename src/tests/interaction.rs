use std::sync::Arc;

use bevy::prelude::*;
use objc2_core_foundation::CGPoint;

use crate::commands::{Command, Direction, MoveFocus, Operation};
use crate::config::{Config, MainOptions, WindowParams};
use crate::ecs::display::FloatingLayer;
use crate::ecs::{ActiveWorkspaceMarker, Position, Unmanaged, layout::LayoutStrip};
use crate::ecs::{RepositionMarker, SpawnWindowTrigger};
use crate::events::Event;
use crate::manager::{Origin, Size, Window};
use crate::platform::Modifiers;
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

    let harness = TestHarness::new().with_config(config).with_windows(3);

    harness
        .on_iteration(1, move |world, state| {
            let origin = Origin::new(0, 0);
            let size = Size::new(TEST_WINDOW_WIDTH, TEST_WINDOW_HEIGHT);
            let frame = IRect::from_corners(origin, origin + size);
            let window = state.spawn_window(TEST_PROCESS_ID, TEST_WORKSPACE_ID, 3, frame);
            world.trigger(SpawnWindowTrigger(vec![window]));
        })
        .on_iteration(3, move |world, _| {
            assert_window_at!(world, 0, 0, TEST_MENUBAR_HEIGHT);
            assert_window_at!(world, 1, 400, TEST_MENUBAR_HEIGHT);
            assert_window_at!(world, 2, 800, TEST_MENUBAR_HEIGHT);
            assert_window_at!(world, 3, offscreen_right, TEST_MENUBAR_HEIGHT);
            assert_focused!(world, 0);
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
        .on_iteration(1, move |world, _state| {
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
        .on_iteration(2, move |world, _state| {
            assert_window_at!(world, 0, left_edge, top_edge);
            assert_window_at!(world, 1, left_edge + TEST_WINDOW_WIDTH, top_edge);
            assert_window_at!(world, 2, left_edge + 2 * TEST_WINDOW_WIDTH, top_edge);
            assert_window_at!(world, 3, offscreen_right, top_edge);
            assert_window_at!(world, 4, offscreen_right, top_edge);
        })
        .on_iteration(3, move |world, _state| {
            assert_window_at!(world, 0, offscreen_left, top_edge);
            assert_window_at!(world, 1, offscreen_left, top_edge);
            assert_window_at!(world, 2, right_edge - 3 * TEST_WINDOW_WIDTH, top_edge);
            assert_window_at!(world, 3, right_edge - 2 * TEST_WINDOW_WIDTH, top_edge);
            assert_window_at!(world, 4, right_edge - TEST_WINDOW_WIDTH, top_edge);
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
            delta: 0.4,
            fingers: 3,
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
        .on_iteration(3, move |world, _state| {
            assert_window_at!(world, 0, 0, TEST_MENUBAR_HEIGHT);
            assert_window_at!(world, 1, 400, TEST_MENUBAR_HEIGHT);
            assert_window_at!(world, 2, 800, TEST_MENUBAR_HEIGHT);
        })
        .on_iteration(5, move |world, _state| {
            assert_window_at!(world, 0, -395, TEST_MENUBAR_HEIGHT);
            assert_window_at!(world, 1, -24, TEST_MENUBAR_HEIGHT);
            assert_window_at!(world, 2, 376, TEST_MENUBAR_HEIGHT);
        })
        .run(commands);
}

#[test]
#[allow(clippy::float_cmp)]
fn test_scrolling_stop() {
    let commands = vec![
        Event::MenuOpened { window_id: 0 },
        Event::Swipe {
            delta: 0.3,
            fingers: 3,
        },
        Event::TouchpadDown,
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
        .on_iteration(3, |world, _state| {
            use crate::ecs::Scrolling;
            let mut query = world.query::<&Scrolling>();
            let scroll = query.single(world).unwrap();
            assert_eq!(scroll.velocity, 0.0);
            assert!(scroll.is_user_swiping);
        })
        .run(commands);
}

#[test]
fn test_window_hidden_ratio() {
    let commands = vec![
        Event::MenuOpened { window_id: 0 },
        Event::Swipe {
            delta: 0.3,
            fingers: 3,
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
        .on_iteration(2, |world, _state| {
            let entity = find_window_entity(0, world);
            let window = world.get::<Window>(entity).expect("finding window");
            assert!(window.frame().min.x < 0);
        })
        .run(commands);
}

#[test]
fn test_window_swap_brings_focused_into_view() {
    // After Center, id=4 is at the centered position. Swap(Last) bubbles
    // id=4 to column 4 (layout x=1600); with the strip at +312 that would
    // put id=4 off-screen to the right (1912). ensure_visible_in_strip
    // scrolls the strip by exactly the shortfall so id=4 sits at the right
    // edge of the viewport (max.x - width = 624). The strip does NOT
    // re-anchor id=4 to its old centered position — there was room to the
    // right, so it slides there. id=0 takes the slot immediately to the
    // left.
    let commands = vec![
        Event::MenuOpened { window_id: 0 },
        Event::Command {
            command: Command::PrintState,
        },
        Event::Command {
            command: Command::Window(Operation::Center),
        },
        Event::Command {
            command: Command::Window(Operation::Swap(Direction::Last)),
        },
        Event::Command {
            command: Command::PrintState,
        },
    ];

    let config: Config = (
        MainOptions {
            animation_speed: Some(10000.0),
            ..Default::default()
        },
        vec![],
    )
        .into();

    let centered = (TEST_DISPLAY_WIDTH - TEST_WINDOW_WIDTH) / 2;
    let right_edge = TEST_DISPLAY_WIDTH - TEST_WINDOW_WIDTH;

    TestHarness::new()
        .with_config(config)
        .with_windows(5)
        .on_iteration(2, move |world, _state| {
            assert_window_at!(world, 0, centered, TEST_MENUBAR_HEIGHT);
        })
        .on_iteration(4, move |world, _state| {
            assert_window_at!(world, 0, right_edge, TEST_MENUBAR_HEIGHT);
            assert_window_at!(
                world,
                4,
                right_edge - TEST_WINDOW_WIDTH,
                TEST_MENUBAR_HEIGHT
            );
            assert_focused!(world, 0);
        })
        .run(commands);
}

#[test]
fn test_window_swap_keeps_strip_when_in_view() {
    // Two windows fit the viewport. Swap(West) on the focused (right)
    // window swaps the columns: both new layout slots are still inside the
    // viewport with the strip where it is, so ensure_visible_in_strip does
    // nothing. The per-window animation slides each window into the other's
    // old position.
    let commands = vec![
        Event::MenuOpened { window_id: 0 },
        Event::Command {
            command: Command::Window(Operation::Focus(Direction::Last)),
        },
        Event::Command {
            command: Command::Window(Operation::Swap(Direction::West)),
        },
    ];

    let config: Config = (
        MainOptions {
            animation_speed: Some(10000.0),
            ..Default::default()
        },
        vec![],
    )
        .into();

    TestHarness::new()
        .with_config(config)
        .with_windows(2)
        .on_iteration(2, |world, _state| {
            assert_window_at!(world, 1, 0, TEST_MENUBAR_HEIGHT);
            assert_window_at!(world, 0, TEST_WINDOW_WIDTH, TEST_MENUBAR_HEIGHT);
            assert_focused!(world, 1);
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
        Event::Command {
            command: Command::PrintState,
        },
    ]);

    assert_focused!(harness.world(), 4);

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

    assert_focused!(harness.world(), 1);
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
        .on_iteration(1, |world, _state| {
            assert_focused!(world, 1);
        })
        .on_iteration(2, |world, _state| {
            assert_focused!(world, 1);
        })
        .run(commands);
}

#[test]
fn test_repeated_external_focus_reshuffles_already_focused_window() {
    let commands = vec![
        Event::MenuOpened { window_id: 0 },
        Event::Command {
            command: Command::PrintState,
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
        .with_windows(5)
        .on_iteration(1, |world, _state| {
            assert_focused!(world, 0);

            let mut query = world.query::<(Entity, &LayoutStrip, Has<ActiveWorkspaceMarker>)>();
            let (entity, _, _) = query
                .iter(world)
                .find(|(_, _, active)| *active)
                .expect("active strip");
            world.commands().entity(entity).insert((
                Position(Origin::new(0, 0)),
                RepositionMarker(Origin::new(-TEST_DISPLAY_WIDTH, 0)),
            ));
        })
        .on_iteration(2, |_world, state| {
            state.focus_window(0);
        })
        .on_iteration(4, |world, _state| {
            assert_focused!(world, 0);
            assert_window_at!(world, 0, 0, TEST_MENUBAR_HEIGHT);
        })
        .run(commands);
}

#[test]
fn test_external_focus_reactivates_hidden_virtual_strip_when_marker_is_stale() {
    let commands = vec![
        Event::Command {
            command: Command::PrintState,
        },
        Event::Command {
            command: Command::Window(Operation::VirtualNumber(1)),
        },
        Event::WindowFocused { window_id: 0 },
        Event::Command {
            command: Command::PrintState,
        },
    ];

    TestHarness::new()
        .with_windows(1)
        .on_iteration(1, |world, _state| {
            let mut query = world.query::<(&LayoutStrip, Has<ActiveWorkspaceMarker>)>();
            let active = query
                .iter(world)
                .find_map(|(strip, active)| active.then_some(strip.virtual_index))
                .expect("an active virtual strip");
            assert_eq!(active, 1);
            assert_focused!(world, 0);
        })
        .on_iteration(3, |world, _state| {
            let mut query = world.query::<(&LayoutStrip, Has<ActiveWorkspaceMarker>)>();
            let active = query
                .iter(world)
                .find_map(|(strip, active)| active.then_some(strip.virtual_index))
                .expect("an active virtual strip");
            assert_eq!(active, 0);
            assert_window_at!(world, 0, 0, TEST_MENUBAR_HEIGHT);
            assert_focused!(world, 0);
        })
        .run(commands);
}

// When the focused window leaves the active strip (e.g. it just became
// floating, or the OS handed focus to an off-strip window), window_focus
// east/west must enter the strip from the appropriate side rather than
// silently doing nothing.
fn focused_window_id(world: &mut World) -> i32 {
    let mut q = world.query::<(&Window, Has<crate::ecs::FocusedMarker>)>();
    q.iter(world)
        .find_map(|(w, f)| f.then_some(w.id()))
        .expect("a focused window")
}

fn entity_to_window_id(world: &mut World, entity: Entity) -> i32 {
    let mut q = world.query::<(&Window, Entity)>();
    q.iter(world)
        .find_map(|(w, e)| (e == entity).then_some(w.id()))
        .expect("entity must be a Window")
}

fn active_strip_first_id(world: &mut World) -> i32 {
    let entity = {
        let mut q = world.query_filtered::<&LayoutStrip, With<ActiveWorkspaceMarker>>();
        let strip = q.single(world).expect("a single active strip");
        strip
            .first()
            .expect("strip should have a column")
            .top()
            .expect("column should have a top entity")
    };
    entity_to_window_id(world, entity)
}

fn active_strip_last_id(world: &mut World) -> i32 {
    let entity = {
        let mut q = world.query_filtered::<&LayoutStrip, With<ActiveWorkspaceMarker>>();
        let strip = q.single(world).expect("a single active strip");
        strip
            .last()
            .expect("strip should have a column")
            .top()
            .expect("column should have a top entity")
    };
    entity_to_window_id(world, entity)
}

// Strip the currently focused entity out of every LayoutStrip so the
// "focused window not in active strip" condition is reproduced regardless
// of how the harness happened to populate the strip. Without this, the
// init-time duplicate-insertion in the test scheduler keeps the entity in
// the strip and the bug is masked.
fn remove_focused_from_all_strips(world: &mut World) {
    let entity = {
        let mut q = world.query_filtered::<Entity, With<crate::ecs::FocusedMarker>>();
        q.single(world).expect("a single focused entity")
    };
    let mut q = world.query::<&mut LayoutStrip>();
    for mut strip in q.iter_mut(world) {
        while strip.contains(entity) {
            strip.remove(entity);
        }
    }
}

#[test]
fn test_focus_recovers_when_focused_window_is_outside_strip() {
    let commands = vec![
        Event::MenuOpened { window_id: 0 },
        Event::Command {
            command: Command::Window(Operation::Focus(Direction::East)),
        },
    ];

    TestHarness::new()
        .with_windows(3)
        .on_iteration(0, |world, _state| {
            // Make the focused entity genuinely live outside any strip,
            // mirroring the state the user reported: the OS handed focus
            // to a window Paneru doesn't track on its active strip.
            remove_focused_from_all_strips(world);
        })
        .on_iteration(1, |world, _state| {
            // Before the fix: get_window_in_direction returns None because
            // active_strip.index_of(focused) fails for a window that's not
            // in the strip, so East is a silent no-op and focus stays on 0.
            let focused = focused_window_id(world);
            assert_ne!(
                focused, 0,
                "focus must leave the off-strip window 0 when pressing East",
            );
            let expected = active_strip_first_id(world);
            assert_eq!(
                focused, expected,
                "East from outside the strip enters at the first (leftmost) column",
            );
        })
        .run(commands);
}

#[test]
fn test_focus_west_from_outside_strip_enters_at_last_column() {
    let commands = vec![
        Event::MenuOpened { window_id: 0 },
        Event::Command {
            command: Command::Window(Operation::Focus(Direction::West)),
        },
    ];

    TestHarness::new()
        .with_windows(3)
        .on_iteration(0, |world, _state| {
            remove_focused_from_all_strips(world);
        })
        .on_iteration(1, |world, _state| {
            let focused = focused_window_id(world);
            let expected = active_strip_last_id(world);
            assert_ne!(focused, 0);
            assert_eq!(
                focused, expected,
                "West from outside the strip enters at the last (rightmost) column",
            );
        })
        .run(commands);
}

#[test]
fn test_external_focus_restores_app_hidden_window_to_original_virtual_strip() {
    let commands = vec![
        Event::Command {
            command: Command::PrintState,
        },
        Event::ApplicationHidden {
            pid: TEST_PROCESS_ID,
        },
        Event::Command {
            command: Command::Window(Operation::VirtualNumber(1)),
        },
        Event::ApplicationVisible {
            pid: TEST_PROCESS_ID,
        },
        Event::WindowFocused { window_id: 0 },
        Event::Command {
            command: Command::PrintState,
        },
    ];

    TestHarness::new()
        .with_windows(1)
        .on_iteration(2, |world, _state| {
            let mut query = world.query::<(&LayoutStrip, Has<ActiveWorkspaceMarker>)>();
            let active = query
                .iter(world)
                .find_map(|(strip, active)| active.then_some(strip.virtual_index))
                .expect("an active virtual strip");
            assert_eq!(active, 1);
        })
        .on_iteration(5, |world, _state| {
            let mut query = world.query::<(&LayoutStrip, Has<ActiveWorkspaceMarker>)>();
            let active = query
                .iter(world)
                .find_map(|(strip, active)| active.then_some(strip.virtual_index))
                .expect("an active virtual strip");
            assert_eq!(active, 0);
            assert_window_at!(world, 0, 0, TEST_MENUBAR_HEIGHT);
            assert_focused!(world, 0);
        })
        .run(commands);
}

#[test]
fn test_external_focus_restores_hidden_window_without_visible_event() {
    let ignored_repositions = Arc::new(std::sync::atomic::AtomicUsize::new(0));

    let commands = vec![
        Event::Command {
            command: Command::PrintState,
        },
        Event::ApplicationHidden {
            pid: TEST_PROCESS_ID,
        },
        Event::Command {
            command: Command::Window(Operation::VirtualNumber(1)),
        },
        Event::WindowFocused { window_id: 0 },
        Event::Command {
            command: Command::PrintState,
        },
    ];

    TestHarness::new()
        .with_windows(1)
        .on_iteration(1, move |world, _state| {
            let mut query = world.query::<&mut Window>();
            let mut window = query
                .iter_mut(world)
                .find(|window| window.id() == 0)
                .expect("window 0");
            window.reposition(Origin::new(0, TEST_DISPLAY_HEIGHT));
            ignored_repositions.store(1, std::sync::atomic::Ordering::SeqCst);
        })
        .on_iteration(4, |world, _state| {
            let mut query = world.query::<(&LayoutStrip, Has<ActiveWorkspaceMarker>)>();
            let active = query
                .iter(world)
                .find_map(|(strip, active)| active.then_some(strip.virtual_index))
                .expect("an active virtual strip");
            assert_eq!(active, 0);
            assert_window_at!(world, 0, 0, TEST_MENUBAR_HEIGHT);
            assert_focused!(world, 0);
        })
        .run(commands);
}

#[test]
fn mouse_in_bottom_right_corner_does_not_change_focus() {
    // Focus window 2 explicitly, then move cursor into the bottom-right 30x30
    // dead zone. The corner gate should suppress the focus-follow-mouse event,
    // so focus stays on window 2.
    //
    // Test display is 1024x768 with no Dock, so the dead zone is
    // x >= 994, y >= 738. Cursor at (1010, 750) is inside it. The mock's
    // find_window_at_point always returns window 0, so without the gate the
    // FFM event would shift focus to window 0; with the gate it should not.
    let commands = vec![
        Event::MenuOpened { window_id: 0 },
        Event::Command {
            command: Command::Window(Operation::Focus(Direction::West)),
        },
        Event::MouseMoved {
            point: CGPoint {
                x: 1010.0,
                y: 750.0,
            },
            modifiers: Modifiers::empty(),
        },
    ];

    TestHarness::new()
        .with_windows(3)
        .on_iteration(2, |world, _state| {
            // After MouseMoved into corner dead zone: focus should remain on window 2
            // because the corner gate suppressed the focus-follow-mouse event.
            assert_focused!(world, 0);
        })
        .run(commands);
}

#[test]
fn mouse_outside_corner_still_changes_focus() {
    use crate::events::Event;
    use crate::platform::Modifiers;
    use objc2_core_foundation::CGPoint;

    // Cursor at (500, 400), middle of the display, outside the dead zone.
    // FFM should fire normally and switch focus.
    //
    // Focus window 2 first, then move cursor away from the corner. The mock's
    // find_window_at_point always returns window 0, so FFM lands focus on
    // window 0.
    let commands = vec![
        Event::MenuOpened { window_id: 0 },
        Event::Command {
            command: Command::Window(Operation::Focus(Direction::West)),
        },
        Event::MouseMoved {
            point: CGPoint { x: 500.0, y: 400.0 },
            modifiers: Modifiers::empty(),
        },
    ];

    TestHarness::new()
        .with_windows(3)
        .on_iteration(2, |world, _state| {
            // After MouseMoved outside corner: FFM should have fired and changed focus.
            // In the mock, find_window_at_point always returns window 0, so window 0
            // should now be focused (changed from window 2).
            assert_focused!(world, 0);
        })
        .run(commands);
}

#[test]
fn toggle_floating_layer_flips_state() {
    fn current_layer(world: &mut World) -> FloatingLayer {
        let mut query = world.query_filtered::<&FloatingLayer, With<ActiveWorkspaceMarker>>();
        *query
            .single(world)
            .expect("active workspace has FloatingLayer")
    }

    let commands = vec![
        Event::Command {
            command: Command::PrintState,
        },
        Event::Command {
            command: Command::Window(Operation::ToggleFloatingLayer),
        },
        Event::Command {
            command: Command::Window(Operation::ToggleFloatingLayer),
        },
    ];

    TestHarness::new()
        .with_config(Config::default())
        .with_windows(3)
        .on_iteration(0, |world, _state| {
            assert_eq!(current_layer(world), FloatingLayer::Front);
        })
        .on_iteration(1, |world, _state| {
            assert_eq!(current_layer(world), FloatingLayer::Behind);
        })
        .on_iteration(2, |world, _state| {
            assert_eq!(current_layer(world), FloatingLayer::Front);
        })
        .run(commands);
}

#[test]
fn test_unfloat_after_virtual_switch_uses_active_workspace() {
    let commands = vec![
        Event::Command {
            command: Command::PrintState,
        },
        Event::Command {
            command: Command::Window(Operation::Manage),
        },
        Event::Command {
            command: Command::Window(Operation::VirtualNumber(1)),
        },
        Event::Command {
            command: Command::Window(Operation::Manage),
        },
    ];

    TestHarness::new()
        .with_windows(2)
        .on_iteration(3, |world, _state| {
            let entity = find_window_entity(0, world);
            let mut query = world.query_filtered::<&LayoutStrip, With<ActiveWorkspaceMarker>>();
            let strip = query.single(world).expect("an active virtual workspace");

            assert_eq!(strip.virtual_index, 1);
            assert!(strip.contains(entity));
        })
        .run(commands);
}

#[test]
fn focus_unmanaged_ignores_floats_from_other_workspaces() {
    let workspaces = vec![TEST_WORKSPACE_ID, TEST_WORKSPACE_ID + 1];
    let harness = TestHarness::new()
        .with_display(
            TEST_DISPLAY_ID,
            IRect::new(0, 0, TEST_DISPLAY_WIDTH, TEST_DISPLAY_HEIGHT),
            workspaces,
        )
        .with_workspace_window(0, TEST_WORKSPACE_ID, |_| {})
        .with_workspace_window(99, TEST_WORKSPACE_ID + 1, |w| {
            w.frame = IRect::new(600, 0, 600 + TEST_WINDOW_WIDTH, TEST_WINDOW_HEIGHT);
        });

    let commands = vec![
        Event::MenuOpened { window_id: 0 },
        Event::Command {
            command: Command::Window(Operation::FocusUnmanaged),
        },
        Event::Command {
            command: Command::PrintState,
        },
        Event::Command {
            command: Command::Window(Operation::Focus(Direction::East)),
        },
    ];

    harness
        .on_iteration(2, |world, _state| {
            let off_workspace_float = find_window_entity(99, world);
            world
                .entity_mut(off_workspace_float)
                .insert(Unmanaged::Floating);
            assert_focused!(world, 0);
        })
        .on_iteration(3, |world, _state| {
            let active_float = find_window_entity(0, world);
            world.entity_mut(active_float).insert(Unmanaged::Floating);
            assert_focused!(world, 0);
        })
        .on_iteration(4, |world, _state| {
            assert_focused!(world, 0);
        })
        .run(commands);
}

/// With `insert_windows_mid_strip` enabled, following a window into another
/// virtual workspace keeps it at its exact on-screen x — even when the
/// destination strip is scrolled and not grid-aligned. The rest of the strip
/// shifts to make room.
#[test]
fn test_mid_strip_insertion_preserves_window_x() {
    let config: Config = (
        MainOptions {
            insert_windows_mid_strip: Some(true),
            swipe_gesture_fingers: Some(3),
            ..Default::default()
        },
        vec![],
    )
        .into();

    let mut h = TestHarness::new().with_config(config).with_windows(8);

    let pump_event = |h: &mut TestHarness, ev: Event| {
        h.app.world_mut().write_message::<Event>(ev);
        for _ in 0..6 {
            h.app.update();
            for event in h.mock_state.drain_events() {
                h.app.world_mut().write_message::<Event>(event);
            }
        }
    };
    let cmd = |h: &mut TestHarness, c: Command| pump_event(h, Event::Command { command: c });
    let win_x = |h: &mut TestHarness, id: i32| -> i32 {
        let world = h.app.world_mut();
        let mut q = world.query::<(&Window, &Position)>();
        q.iter(world)
            .find_map(|(w, p)| (w.id() == id).then_some(p.0.x))
            .expect("window position")
    };

    // Build VW1 with four windows (scrollable), leaving four on VW0.
    for _ in 0..4 {
        cmd(
            &mut h,
            Command::Window(Operation::VirtualMoveNumber(1, MoveFocus::Stay)),
        );
    }
    // Scroll VW1 off the grid, return, then scroll VW0 off the grid too, so both
    // the moved window and the destination columns sit between column boundaries.
    cmd(&mut h, Command::Window(Operation::VirtualNumber(1)));
    pump_event(
        &mut h,
        Event::Swipe {
            delta: 0.3,
            fingers: 3,
        },
    );
    cmd(&mut h, Command::Window(Operation::VirtualNumber(0)));
    pump_event(
        &mut h,
        Event::Swipe {
            delta: 0.3,
            fingers: 3,
        },
    );

    let mover = {
        let world = h.app.world_mut();
        let mut q = world.query_filtered::<&Window, With<crate::ecs::FocusedMarker>>();
        q.single(world).expect("a focused window").id()
    };
    let before = win_x(&mut h, mover);
    assert_ne!(
        before % TEST_WINDOW_WIDTH,
        0,
        "test setup should leave the window off the column grid, got x={before}",
    );

    cmd(
        &mut h,
        Command::Window(Operation::VirtualMoveNumber(1, MoveFocus::Follow)),
    );
    assert_eq!(
        win_x(&mut h, mover),
        before,
        "moved window must keep its exact on-screen x",
    );
}

/// Without the flag (the default), a moved window is appended to the end of the
/// destination strip, preserving arrival order.
#[test]
fn test_move_appends_to_end_by_default() {
    let mut h = TestHarness::new().with_windows(3);

    let pump = |h: &mut TestHarness, cmd: Command| {
        h.app
            .world_mut()
            .write_message::<Event>(Event::Command { command: cmd });
        for _ in 0..6 {
            h.app.update();
            for event in h.mock_state.drain_events() {
                h.app.world_mut().write_message::<Event>(event);
            }
        }
    };

    // Seed VW1 with one window, keeping us on VW0.
    pump(
        &mut h,
        Command::Window(Operation::VirtualMoveNumber(1, MoveFocus::Stay)),
    );

    // Whatever window is focused now is the one the follow-move will carry.
    let mover = focused_window_id(h.app.world_mut());

    pump(
        &mut h,
        Command::Window(Operation::VirtualMoveNumber(1, MoveFocus::Follow)),
    );

    // Default behaviour: the moved window is appended, i.e. it is the last
    // column of the (now active) destination strip.
    let last = {
        let world = h.app.world_mut();
        let mut q = world.query::<(&LayoutStrip, Has<ActiveWorkspaceMarker>)>();
        let entity = q
            .iter(world)
            .find_map(|(s, a)| a.then(|| s.all_windows()))
            .and_then(|windows| windows.last().copied())
            .expect("active strip with windows");
        let mut wq = world.query::<(Entity, &Window)>();
        wq.iter(world)
            .find_map(|(ent, w)| (ent == entity).then_some(w.id()))
            .expect("window id")
    };
    assert_eq!(
        last, mover,
        "default move should append to the end of the strip"
    );
}

/// With `insert_windows_mid_strip` enabled and a smooth `animation_speed`, moving
/// a window to another virtual workspace must not animate: every window snaps to
/// its final spot. Checked per-update, since markers created and consumed
/// mid-move would be invisible to a settle-then-check.
#[test]
fn test_mid_strip_move_does_not_animate() {
    let config: Config = (
        MainOptions {
            insert_windows_mid_strip: Some(true),
            animation_speed: Some(12.0),
            virtual_workspace_animations: Some(false),
            swipe_gesture_fingers: Some(3),
            ..Default::default()
        },
        vec![],
    )
        .into();

    let mut h = TestHarness::new().with_config(config).with_windows(8);
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

    // Build a scrolled VW1 and scroll VW0 too, so the move is off the grid.
    for _ in 0..4 {
        pump(
            &mut h,
            Command::Window(Operation::VirtualMoveNumber(1, MoveFocus::Stay)),
        );
    }
    pump(&mut h, Command::Window(Operation::VirtualNumber(1)));
    h.app.world_mut().write_message::<Event>(Event::Swipe {
        delta: 0.3,
        fingers: 3,
    });
    for _ in 0..6 {
        h.app.update();
        for e in h.mock_state.drain_events() {
            h.app.world_mut().write_message::<Event>(e);
        }
    }
    pump(&mut h, Command::Window(Operation::VirtualNumber(0)));
    h.app.world_mut().write_message::<Event>(Event::Swipe {
        delta: 0.3,
        fingers: 3,
    });
    for _ in 0..6 {
        h.app.update();
        for e in h.mock_state.drain_events() {
            h.app.world_mut().write_message::<Event>(e);
        }
    }

    // Follow-move into the existing VW1, checking every update for animation.
    h.app.world_mut().write_message::<Event>(Event::Command {
        command: Command::Window(Operation::VirtualMoveNumber(1, MoveFocus::Follow)),
    });
    for step in 0..10 {
        h.app.update();
        for e in h.mock_state.drain_events() {
            h.app.world_mut().write_message::<Event>(e);
        }
        let world = h.app.world_mut();
        let mut q = world.query_filtered::<&Window, With<RepositionMarker>>();
        let animating: Vec<i32> = q.iter(world).map(|w| w.id()).collect();
        assert!(
            animating.is_empty(),
            "step {step}: no window should animate during a mid-strip move, got {animating:?}",
        );
    }
}

/// Switching virtual workspaces with `virtual_workspace_animations = false` must
/// never cause the active strip to slide left or right. The strip position must
/// snap directly to its saved scroll position without any `RepositionMarker`
/// animating it further. Regression test: after VW2 → VW1, a stale
/// `reshuffle_layout_strip` was computing an incorrect strip target from
/// un-updated window positions, inserting a `RepositionMarker` that animated
/// the strip sideways.
///
/// Setup: VW0 has 5 windows (scrollable), scrolled so the focused window sits
/// at a non-zero strip offset. We then switch to VW1 and back to VW0, and
/// verify that no `RepositionMarker` is ever placed on the strip (which would
/// cause horizontal sliding).
#[test]
fn test_virtual_workspace_switch_no_horizontal_slide_no_animations() {
    let config: Config = (
        MainOptions {
            virtual_workspace_animations: Some(false),
            animation_speed: Some(12.0),
            swipe_gesture_fingers: Some(3),
            ..Default::default()
        },
        vec![],
    )
        .into();

    // 5 windows: strip width = 5 * 400 = 2000, display = 1024 → scrollable.
    let mut h = TestHarness::new().with_config(config).with_windows(5);

    let pump_event = |h: &mut TestHarness, ev: Event| {
        h.app.world_mut().write_message::<Event>(ev);
        for _ in 0..8 {
            h.app.update();
            for e in h.mock_state.drain_events() {
                h.app.world_mut().write_message::<Event>(e);
            }
        }
    };
    let pump = |h: &mut TestHarness, c: Command| pump_event(h, Event::Command { command: c });

    // Boot the strip and focus window 0.
    pump(&mut h, Command::PrintState);

    // Scroll the strip so it sits at a non-zero x offset.
    pump_event(
        &mut h,
        Event::Swipe {
            delta: 0.3,
            fingers: 3,
        },
    );

    // Remember the settled strip x after scrolling.
    let strip_x_after_scroll = {
        let world = h.app.world_mut();
        let mut q = world.query_filtered::<&crate::ecs::Position, With<ActiveWorkspaceMarker>>();
        q.single(world)
            .expect("exactly one active strip after scroll")
            .0
            .x
    };
    assert_ne!(
        strip_x_after_scroll, 0,
        "test setup: strip should be scrolled to a non-zero position, got 0"
    );

    // Switch to VW1 (spawned on the fly, empty).
    pump(&mut h, Command::Window(Operation::VirtualNumber(1)));

    // Switch back to VW0. This is where the bug triggers: the strip should
    // snap to `strip_x_after_scroll` with no RepositionMarker causing further
    // horizontal motion.
    h.app.world_mut().write_message::<Event>(Event::Command {
        command: Command::Window(Operation::VirtualNumber(0)),
    });

    // After the VW switch back to VW0, pump frames and assert the active strip's
    // x position settles exactly at the pre-switch scroll value. Any deviation
    // means a stale reshuffle slid the strip sideways.
    for _ in 0..10 {
        h.app.update();
        for e in h.mock_state.drain_events() {
            h.app.world_mut().write_message::<Event>(e);
        }
    }

    let strip_x_final = {
        let world = h.app.world_mut();
        let mut q = world.query_filtered::<&crate::ecs::Position, With<ActiveWorkspaceMarker>>();
        q.single(world)
            .expect("exactly one active strip after switch-back")
            .0
            .x
    };

    assert_eq!(
        strip_x_final, strip_x_after_scroll,
        "strip x must equal the pre-switch scroll position after VW switch-back (no sideways slide). \
         Expected {strip_x_after_scroll}, got {strip_x_final}"
    );
}

/// When a strip is mid-animation (has a `RepositionMarker`) at the moment the
/// user switches to another virtual workspace, the animation must stop
/// immediately. Previously the `RepositionMarker` was left on the hidden strip
/// so `animate_entities` kept updating its position while it was off-screen,
/// making the two strips briefly visible at the same time (the hidden one still
/// sliding) and corrupting the saved restore position.
#[test]
fn test_virtual_workspace_switch_stops_in_flight_strip_animation() {
    let config: Config = (
        MainOptions {
            virtual_workspace_animations: Some(false),
            animation_speed: Some(12.0),
            swipe_gesture_fingers: Some(3),
            ..Default::default()
        },
        vec![],
    )
        .into();

    let mut h = TestHarness::new().with_config(config).with_windows(5);

    let pump_n = |h: &mut TestHarness, n: usize, ev: Event| {
        h.app.world_mut().write_message::<Event>(ev);
        for _ in 0..n {
            h.app.update();
            for e in h.mock_state.drain_events() {
                h.app.world_mut().write_message::<Event>(e);
            }
        }
    };
    let pump = |h: &mut TestHarness, c: Command| {
        pump_n(h, 8, Event::Command { command: c });
    };

    pump(&mut h, Command::PrintState);

    // Scroll and then switch to VW1 mid-animation (only 1 frame so animation
    // is still in progress when the switch fires).
    h.app.world_mut().write_message::<Event>(Event::Swipe {
        delta: 0.3,
        fingers: 3,
    });
    // One frame to start the animation.
    h.app.update();
    for e in h.mock_state.drain_events() {
        h.app.world_mut().write_message::<Event>(e);
    }

    // Switch to VW1 while the strip may still have a RepositionMarker.
    pump(&mut h, Command::Window(Operation::VirtualNumber(1)));

    // VW0's strip must have no RepositionMarker (animation stopped on hide).
    {
        let world = h.app.world_mut();
        let mut q = world.query_filtered::<Entity, (
            With<crate::ecs::layout::LayoutStrip>,
            Without<ActiveWorkspaceMarker>,
        )>();
        for entity in q.iter(world) {
            assert!(
                world.get::<RepositionMarker>(entity).is_none(),
                "hidden strip {entity:?} must not have RepositionMarker after VW switch"
            );
        }
    }

    // Switch back to VW0. The strip should restore to the saved position,
    // not to wherever the mid-flight animation would have taken it.
    let saved_x = {
        // The saved position is snapped to what it was at switch time; just
        // record where VW0's strip ends up after restoring.
        h.app.world_mut().write_message::<Event>(Event::Command {
            command: Command::Window(Operation::VirtualNumber(0)),
        });
        for _ in 0..10 {
            h.app.update();
            for e in h.mock_state.drain_events() {
                h.app.world_mut().write_message::<Event>(e);
            }
        }
        let world = h.app.world_mut();
        let mut q = world.query_filtered::<&crate::ecs::Position, With<ActiveWorkspaceMarker>>();
        q.single(world)
            .expect("exactly one active strip after restore")
            .0
            .x
    };

    // After restoring the strip must also have no RepositionMarker — it
    // should have snapped directly, not started a new animation.
    {
        let world = h.app.world_mut();
        let mut q = world.query_filtered::<Entity, (
            With<crate::ecs::layout::LayoutStrip>,
            With<ActiveWorkspaceMarker>,
        )>();
        for entity in q.iter(world) {
            assert!(
                world.get::<RepositionMarker>(entity).is_none(),
                "restored strip {entity:?} must not have RepositionMarker (no animation after no-anim VW switch)"
            );
        }
    }

    // The final position must be stable (no further drift).
    for _ in 0..5 {
        h.app.update();
        for e in h.mock_state.drain_events() {
            h.app.world_mut().write_message::<Event>(e);
        }
    }
    let world = h.app.world_mut();
    let mut q = world.query_filtered::<&crate::ecs::Position, With<ActiveWorkspaceMarker>>();
    let final_x = q.single(world).expect("exactly one active strip").0.x;
    assert_eq!(
        final_x, saved_x,
        "strip x drifted after restore: was {saved_x}, now {final_x}"
    );
}

/// With `auto_center` off, a reshuffle around the leftmost window of a
/// scrollable strip must pin the strip to the left edge — the leftmost
/// window's left edge must touch the display's left edge, never leaving empty
/// space to its left.
///
/// Regression: after a virtual-workspace switch parks the inactive strip at
/// `bounds.max - 10`, every window's on-screen frame is momentarily stale at
/// the right-edge sliver. A focus-driven `reshuffle_layout_strip` that read
/// that stale frame computed a large positive strip offset and pushed column 0
/// away from the left edge (leftmost window ended up right-aligned). This test
/// injects the stale right-edge frame directly (the real trigger is a delayed
/// duplicate OS focus event that the mock platform doesn't emit) and asserts
/// the reshuffle clamps the strip back to the left edge.
#[test]
fn test_reshuffle_leftmost_pins_strip_to_left_edge_with_stale_frame() {
    use crate::ecs::{Position, ReshuffleAroundMarker};

    let config: Config = (
        MainOptions {
            auto_center: Some(false),
            animation_speed: Some(30.0),
            ..Default::default()
        },
        vec![],
    )
        .into();

    // 5 windows @ 400px = 2000px strip on a 1024px display → scrollable.
    let mut h = TestHarness::new().with_config(config).with_windows(5);

    let pump = |h: &mut TestHarness, c: Command| {
        h.app
            .world_mut()
            .write_message::<Event>(Event::Command { command: c });
        for _ in 0..10 {
            h.app.update();
            for e in h.mock_state.drain_events() {
                h.app.world_mut().write_message::<Event>(e);
            }
        }
    };

    // Boot the strip; column 0 (window id 0) sits at layout x 0.
    pump(&mut h, Command::PrintState);

    let leftmost = find_window_entity(0, h.app.world_mut());

    // Simulate the stale post-VW-switch state: the leftmost window's on-screen
    // frame is parked at the right-edge sliver while its layout position is
    // still 0. Clear any in-flight animation so moving_frame reads the origin.
    {
        let world = h.app.world_mut();
        if let Ok(mut e) = world.get_entity_mut(leftmost) {
            e.insert(Position(Origin::new(
                TEST_DISPLAY_WIDTH - 5,
                TEST_MENUBAR_HEIGHT,
            )));
            e.remove::<RepositionMarker>();
            // Trigger a reshuffle around the leftmost window, as focus would.
            e.insert(ReshuffleAroundMarker);
        }
    }

    for _ in 0..15 {
        h.app.update();
        for e in h.mock_state.drain_events() {
            h.app.world_mut().write_message::<Event>(e);
        }
    }

    // The strip must be pinned to the left edge (offset 0): column 0 has
    // layout x 0, so its on-screen left edge lands at the display's left edge.
    let world = h.app.world_mut();
    let mut q = world.query_filtered::<&Position, With<ActiveWorkspaceMarker>>();
    let strip_x = q.single(world).expect("exactly one active strip").0.x;
    assert_eq!(
        strip_x, 0,
        "reshuffle around leftmost window must pin strip to left edge (offset 0), got {strip_x}"
    );
}

/// With `virtual_workspace_animations = true`, switching away from a scrolled
/// strip and back must restore its saved scroll position, not reset it.
///
/// Regression: the animated restore branch of `show_active_workspace` called
/// `reshuffle_around(focus)` in addition to animating the strip to its saved
/// origin. That reshuffle read stale mid-animation window frames a frame later
/// and overwrote the restore target with a different offset, discarding the
/// saved scroll (the strip jumped back to 0). The animated branch now restores
/// the origin without reshuffling, mirroring the non-animated branch.
#[test]
fn test_virtual_workspace_switch_preserves_scroll_with_animations() {
    use crate::ecs::Position;

    let config: Config = (
        MainOptions {
            auto_center: Some(false),
            animation_speed: Some(30.0),
            swipe_gesture_fingers: Some(3),
            virtual_workspace_animations: Some(true),
            ..Default::default()
        },
        vec![],
    )
        .into();

    // 5 windows @ 400px = 2000px strip on a 1024px display → scrollable.
    let mut h = TestHarness::new().with_config(config).with_windows(5);

    let pump_event = |h: &mut TestHarness, ev: Event| {
        h.app.world_mut().write_message::<Event>(ev);
        for _ in 0..14 {
            h.app.update();
            for e in h.mock_state.drain_events() {
                h.app.world_mut().write_message::<Event>(e);
            }
        }
    };
    let pump = |h: &mut TestHarness, c: Command| pump_event(h, Event::Command { command: c });

    // Boot and scroll the strip off the left edge to a non-zero offset.
    pump(&mut h, Command::PrintState);
    pump_event(
        &mut h,
        Event::Swipe {
            delta: 0.4,
            fingers: 3,
        },
    );

    let strip_x_after_scroll = {
        let world = h.app.world_mut();
        let mut q = world.query_filtered::<&Position, With<ActiveWorkspaceMarker>>();
        q.single(world).expect("active strip after scroll").0.x
    };
    assert_ne!(
        strip_x_after_scroll, 0,
        "test setup: strip should be scrolled off the left edge, got 0"
    );

    // Switch to an empty VW and back.
    pump(&mut h, Command::Window(Operation::VirtualNumber(1)));
    pump(&mut h, Command::Window(Operation::VirtualNumber(0)));

    let strip_x_restored = {
        let world = h.app.world_mut();
        let mut q = world.query_filtered::<&Position, With<ActiveWorkspaceMarker>>();
        q.single(world).expect("active strip after restore").0.x
    };
    assert_eq!(
        strip_x_restored, strip_x_after_scroll,
        "animated VW restore must preserve the saved scroll position. \
         Expected {strip_x_after_scroll}, got {strip_x_restored}"
    );
}

/// With `virtual_workspace_animations = true`, switching to another virtual
/// workspace must move the previously-active strip (and therefore its windows)
/// off-screen. Regression: `show_active_workspace` queued a `RepositionMarker`
/// to animate the old strip to `bounds.max - 10`, but a cleanup block right
/// after removed the very same marker in the same command flush — so the strip
/// never moved and all of the old workspace's windows stayed visible on top of
/// the workspace we switched to. The cleanup now runs before the hide
/// reposition is queued.
#[test]
fn test_virtual_workspace_switch_hides_old_strip_with_animations() {
    use crate::ecs::{Position, layout::LayoutStrip};

    let config: Config = (
        MainOptions {
            auto_center: Some(false),
            animation_speed: Some(30.0),
            swipe_gesture_fingers: Some(3),
            virtual_workspace_animations: Some(true),
            ..Default::default()
        },
        vec![],
    )
        .into();

    let mut h = TestHarness::new().with_config(config).with_windows(5);

    let pump = |h: &mut TestHarness, c: Command| {
        h.app
            .world_mut()
            .write_message::<Event>(Event::Command { command: c });
        for _ in 0..14 {
            h.app.update();
            for e in h.mock_state.drain_events() {
                h.app.world_mut().write_message::<Event>(e);
            }
        }
    };

    pump(&mut h, Command::PrintState);
    // Leave a window on VW0 (Stay) and spawn VW1 with one window, then switch.
    pump(
        &mut h,
        Command::Window(Operation::VirtualMoveNumber(1, MoveFocus::Stay)),
    );
    pump(&mut h, Command::Window(Operation::VirtualNumber(1)));

    // The inactive VW0 strip must have moved off-screen (its origin parked at
    // bounds.max - 10 = 1014), taking its windows with it.
    let world = h.app.world_mut();
    let mut q = world.query::<(&LayoutStrip, &Position, Has<ActiveWorkspaceMarker>)>();
    let inactive_x = q
        .iter(world)
        .find_map(|(_, pos, active)| (!active).then_some(pos.0.x))
        .expect("an inactive strip exists after switching workspaces");
    assert!(
        inactive_x >= TEST_DISPLAY_WIDTH - 10,
        "inactive strip must be parked off-screen (>= {}), got {inactive_x}",
        TEST_DISPLAY_WIDTH - 10
    );
}

/// Stacking or unstacking the focused window must bring it fully back into
/// view. Regression: stack_windows_handler mutated the strip but never
/// reshuffled, so when the strip was scrolled such that the focused window's
/// new column slot fell off-screen, the window stayed partially or fully
/// invisible even though it kept focus. It now reshuffles around the focused
/// window; the edge-clamp in reshuffle_layout_strip keeps the strip pinned to
/// the edges.
#[test]
fn test_stack_unstack_brings_focused_window_into_view() {
    use crate::ecs::{Bounds, Position, Scrolling};

    let config: Config = (
        MainOptions {
            auto_center: Some(false),
            animation_speed: Some(10000.0),
            swipe_gesture_fingers: Some(3),
            ..Default::default()
        },
        vec![],
    )
        .into();

    // 5 windows @ 400px = 2000px strip on a 1024px display → scrollable.
    let mut h = TestHarness::new().with_config(config).with_windows(5);

    let pump = |h: &mut TestHarness, c: Command| {
        h.app
            .world_mut()
            .write_message::<Event>(Event::Command { command: c });
        for _ in 0..16 {
            h.app.update();
            for e in h.mock_state.drain_events() {
                h.app.world_mut().write_message::<Event>(e);
            }
        }
    };

    // Emulate the real-app settled state after a swipe: the strip is scrolled
    // and the transient Scrolling has been removed (swiping_timeout drops it
    // within ~50ms in the app; the test harness's fast wall-clock wouldn't).
    let settle_scroll_offset = |h: &mut TestHarness| {
        h.app.world_mut().write_message::<Event>(Event::Swipe {
            delta: 0.9,
            fingers: 3,
        });
        for _ in 0..16 {
            h.app.update();
            for e in h.mock_state.drain_events() {
                h.app.world_mut().write_message::<Event>(e);
            }
        }
        let world = h.app.world_mut();
        let mut q = world.query_filtered::<Entity, With<Scrolling>>();
        for e in q.iter(world).collect::<Vec<_>>() {
            if let Ok(mut em) = world.get_entity_mut(e) {
                em.remove::<Scrolling>();
            }
        }
        for _ in 0..4 {
            h.app.update();
            for e in h.mock_state.drain_events() {
                h.app.world_mut().write_message::<Event>(e);
            }
        }
    };

    let is_fully_onscreen = |h: &mut TestHarness, id: i32| {
        let e = find_window_entity(id, h.app.world_mut());
        let world = h.app.world();
        let p = world.get::<Position>(e).unwrap().0;
        let b = world.get::<Bounds>(e).unwrap().0;
        p.x >= 0 && p.x + b.x <= TEST_DISPLAY_WIDTH
    };

    pump(&mut h, Command::PrintState);

    // Focus window 1, scroll it off the left edge, then stack it onto window 0.
    pump(&mut h, Command::Window(Operation::Focus(Direction::First)));
    pump(&mut h, Command::Window(Operation::Focus(Direction::East)));
    settle_scroll_offset(&mut h);
    assert!(
        !is_fully_onscreen(&mut h, 1),
        "test setup: focused window should be off-screen before stacking"
    );

    pump(&mut h, Command::Window(Operation::Stack(true)));
    assert!(
        is_fully_onscreen(&mut h, 1),
        "stacking must bring the focused window fully back into view"
    );

    // Scroll off-screen again, then unstack: the focused window moves out to
    // its own column and must likewise be brought into view.
    settle_scroll_offset(&mut h);
    assert!(
        !is_fully_onscreen(&mut h, 1),
        "test setup: focused window should be off-screen before unstacking"
    );

    pump(&mut h, Command::Window(Operation::Stack(false)));
    assert!(
        is_fully_onscreen(&mut h, 1),
        "unstacking must bring the focused window fully back into view"
    );
}

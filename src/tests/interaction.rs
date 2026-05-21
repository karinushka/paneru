use std::sync::Arc;

use bevy::prelude::*;
use objc2_core_foundation::CGPoint;

use crate::commands::{Command, Direction, Operation};
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
        .on_iteration(3, move |world, _state| {
            assert_window_at!(world, 0, 0, TEST_MENUBAR_HEIGHT);
            assert_window_at!(world, 1, 400, TEST_MENUBAR_HEIGHT);
            assert_window_at!(world, 2, 800, TEST_MENUBAR_HEIGHT);
        })
        .on_iteration(5, move |world, _state| {
            assert_window_at!(world, 0, -316, TEST_MENUBAR_HEIGHT);
            assert_window_at!(world, 1, 84, TEST_MENUBAR_HEIGHT);
            assert_window_at!(world, 2, 484, TEST_MENUBAR_HEIGHT);
        })
        .run(commands);
}

#[test]
#[allow(clippy::float_cmp)]
fn test_scrolling_stop() {
    let commands = vec![
        Event::MenuOpened { window_id: 0 },
        Event::Swipe {
            deltas: vec![0.1, 0.1, 0.1],
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

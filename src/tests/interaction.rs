use std::sync::Arc;
use std::time::Duration;

use bevy::prelude::*;
use objc2_core_foundation::CGPoint;

use crate::commands::{Command, Direction, MoveFocus, Operation};
use crate::config::{Config, MainOptions, WindowParams, parse_command};
use crate::ecs::display::FloatingLayer;
use crate::ecs::{
    ActiveWorkspaceMarker, FocusedMarker, ManualStripOffset, NativeFullscreenMarker, Position,
    Unmanaged, layout::LayoutStrip,
};
use crate::ecs::{RepositionMarker, SpawnWindowTrigger};
use crate::events::Event;
use crate::manager::{Origin, Size, Window};
use crate::platform::{Modifiers, WinID};
use crate::{assert_focused, assert_window_at, assert_window_size};

use super::*;

#[test]
fn native_fullscreen_transition_removes_window_from_original_strip_without_focus_marker() {
    const FULLSCREEN_WORKSPACE_ID: WorkspaceId = TEST_WORKSPACE_ID + 100;

    TestHarness::new()
        .with_windows(2)
        .on_iteration(0, |world, state| {
            let focused = world
                .query_filtered::<Entity, With<FocusedMarker>>()
                .iter(world)
                .collect::<Vec<_>>();
            for entity in focused {
                world.entity_mut(entity).remove::<FocusedMarker>();
            }

            state.update_window(0, |window| {
                window.workspace_id = FULLSCREEN_WORKSPACE_ID;
                window.is_full_screen = true;
            });
            state.activate_workspace(TEST_DISPLAY_ID, FULLSCREEN_WORKSPACE_ID, true);
        })
        .on_iteration(1, |world, _state| {
            let fullscreen_window = find_window_entity(0, world);
            let sibling_window = find_window_entity(1, world);
            let mut strips = world.query::<(&LayoutStrip, Option<&NativeFullscreenMarker>)>();

            let original_strip = strips
                .iter(world)
                .find_map(|(strip, marker)| {
                    (strip.id() == TEST_WORKSPACE_ID && marker.is_none()).then_some(strip)
                })
                .expect("original strip");
            assert!(
                !original_strip.contains(fullscreen_window),
                "fullscreen window must not leave a reserved column in the original strip"
            );
            assert!(original_strip.contains(sibling_window));

            let (fullscreen_strip, fullscreen_marker) = strips
                .iter(world)
                .find(|(strip, _)| strip.id() == FULLSCREEN_WORKSPACE_ID)
                .expect("fullscreen strip");
            assert!(fullscreen_strip.contains(fullscreen_window));
            assert!(fullscreen_marker.is_some());
        })
        .on_iteration(2, |world, _state| {
            let fullscreen_window = find_window_entity(0, world);
            let sibling_window = find_window_entity(1, world);
            let mut strips = world.query::<&LayoutStrip>();

            let original_strip = strips
                .iter(world)
                .find(|strip| strip.id() == TEST_WORKSPACE_ID)
                .expect("original strip");
            assert!(original_strip.contains(fullscreen_window));
            assert!(original_strip.contains(sibling_window));
            assert_eq!(
                original_strip
                    .index_of(fullscreen_window)
                    .expect("restored fullscreen window index"),
                0
            );
            assert!(
                strips
                    .iter(world)
                    .all(|strip| strip.id() != FULLSCREEN_WORKSPACE_ID)
            );
        })
        .run(vec![
            Event::Command {
                command: Command::PrintState,
            },
            Event::SpaceChanged,
            Event::SpaceDestroyed {
                space_id: FULLSCREEN_WORKSPACE_ID,
            },
        ]);
}

#[test]
fn frontmost_floating_window_is_focused_after_setup() {
    let mut params = WindowParams::new(".*", None);
    params.floating = Some(true);
    let config: Config = (MainOptions::default(), vec![params]).into();

    TestHarness::new()
        .with_config(config)
        .with_windows(1)
        .with_focused_window(0)
        .on_iteration(0, |world, _state| {
            assert_focused!(world, 0);
            let entity = find_window_entity(0, world);
            assert!(world.entity(entity).contains::<Unmanaged>());
        })
        .run(vec![Event::MenuOpened { window_id: 0 }]);
}

/// Regression: a floating window placed by a grid rule must land at the active
/// display's usable origin (menubar + padding offset), not at (0, 0). Dropping
/// the display bounds origin previously sent grid windows to the primary
/// display's top-left corner (and onto the wrong display in multi-display
/// setups).
#[test]
fn floating_grid_window_uses_active_display_usable_origin() {
    let options = MainOptions {
        padding_left: Some(40),
        padding_top: Some(15),
        ..MainOptions::default()
    };

    let mut params = WindowParams::new(".*", None);
    params.floating = Some(true);
    // Cell (0,0) spanning the full 1x1 grid: origin should equal the usable
    // top-left, independent of the display size.
    params.grid = Some("1:1:0:0:1:1".to_string());
    let config: Config = (options, vec![params]).into();

    TestHarness::new()
        .with_config(config)
        .on_iteration(1, |world, state| {
            let origin = Origin::new(0, 0);
            let size = Size::new(TEST_WINDOW_WIDTH, TEST_WINDOW_HEIGHT);
            let frame = IRect::from_corners(origin, origin + size);
            let window = state.spawn_window(TEST_PROCESS_ID, TEST_WORKSPACE_ID, 0, frame);
            world.trigger(SpawnWindowTrigger(vec![window]));
        })
        .on_iteration(3, |world, _state| {
            // usable origin = (pad_left, menubar + pad_top) = (40, 20 + 15).
            assert_window_at!(world, 0, 40, TEST_MENUBAR_HEIGHT + 15);
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
                command: Command::PrintState,
            },
        ]);
}

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
fn test_focus_window_by_number() {
    assert!(parse_command(&["window", "focus", "0"]).is_err());
    let command = parse_command(&["window", "focus", "2"]).unwrap();

    TestHarness::new()
        .with_windows(3)
        .on_iteration(1, |world, _state| assert_focused!(world, 1))
        .on_iteration(2, |world, _state| assert_focused!(world, 1))
        .on_iteration(3, |world, _state| assert_focused!(world, 2))
        .run(vec![
            Event::MenuOpened { window_id: 0 },
            Event::Command {
                command: command.clone(),
            },
            Event::Command {
                command: Command::Window(Operation::Manage),
            },
            Event::Command { command },
        ]);
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
        // A single event's delta is a fraction of the viewport travelled in
        // one frame, and the gesture velocity it produces is `delta / dt`.
        // 0.04 over a 20ms frame is two viewport widths per second — a brisk
        // but ordinary swipe, which is the regime this test is about. An order
        // of magnitude more and the strip simply flies into its clamp bound
        // and every window parks off-screen at the sliver, which asserts
        // nothing about scrolling.
        Event::Swipe {
            delta: 0.04,
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
        // The strip has come to rest mid-scroll: still one contiguous run of
        // 400px columns, none of them parked at an edge sliver.
        .on_iteration(5, move |world, _state| {
            assert_window_at!(world, 0, -186, TEST_MENUBAR_HEIGHT);
            assert_window_at!(world, 1, 214, TEST_MENUBAR_HEIGHT);
            assert_window_at!(world, 2, 614, TEST_MENUBAR_HEIGHT);
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
            assert_focused!(world, 1);
        })
        .run(commands);
}

#[test]
fn toggle_floating_layer_flips_state() {
    fn current_layer(world: &mut World) -> FloatingLayer {
        let mut query = world.query::<&FloatingLayer>();
        *query
            .query(world)
            .iter()
            .find(|layer| layer.workspace_id == TEST_WORKSPACE_ID)
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
            assert!(!current_layer(world).front);
        })
        .on_iteration(1, |world, _state| {
            assert!(current_layer(world).front);
        })
        .on_iteration(2, |world, _state| {
            assert!(!current_layer(world).front);
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

    let harness = TestHarness::new().with_config(config).with_windows(8);

    let commands = vec![
        Event::MenuOpened { window_id: 0 },
        // Build VW1 with four windows (scrollable), leaving four on VW0.
        Event::Command {
            command: Command::Window(Operation::VirtualMoveNumber(1, MoveFocus::Stay)),
        },
        Event::Command {
            command: Command::Window(Operation::VirtualMoveNumber(1, MoveFocus::Stay)),
        },
        Event::Command {
            command: Command::Window(Operation::VirtualMoveNumber(1, MoveFocus::Stay)),
        },
        Event::Command {
            command: Command::Window(Operation::VirtualMoveNumber(1, MoveFocus::Stay)),
        },
        // Scroll VW0 slightly to randomize the positions. "Slightly" is the
        // point: a single event's delta is viewport-fractions travelled in one
        // frame, so the velocity it yields is `delta / dt`. Anything much
        // larger throws the strip into its clamp bound, which parks the
        // focused window at an edge sliver and makes the offset compared below
        // that fixed sliver rather than a real layout position.
        Event::Swipe {
            delta: 0.06,
            fingers: 3,
        },
        // Used as a noop to let the scroll settle.
        Event::MenuOpened { window_id: 0 },
        // Change to VW1 and scroll it slightly as well.
        Event::Command {
            command: Command::Window(Operation::VirtualNumber(1)),
        },
        Event::Swipe {
            delta: 0.04,
            fingers: 3,
        },
        Event::MenuOpened { window_id: 0 },
        // Change back to VW0 and send one window over to VW1.
        Event::Command {
            command: Command::Window(Operation::VirtualNumber(0)),
        },
        Event::Command {
            command: Command::Window(Operation::VirtualMoveNumber(1, MoveFocus::Follow)),
        },
        Event::Command {
            command: Command::PrintState,
        },
    ];

    let previous_offset = std::rc::Rc::new(std::cell::RefCell::new(0));
    let previous_offset2 = previous_offset.clone();
    harness
        .on_iteration(10, move |world, _state| {
            let mut q =
                world.query_filtered::<(&Window, &Position), With<crate::ecs::FocusedMarker>>();
            let (_, position) = q.single(world).expect("a focused window");

            previous_offset.replace(position.x);
            assert_ne!(position.x, 0);
        })
        .on_iteration(11, move |world, _state| {
            let mut q =
                world.query_filtered::<(&Window, &Position), With<crate::ecs::FocusedMarker>>();
            let (_, position) = q.single(world).expect("a focused window");

            assert_eq!(position.x, previous_offset2.take());
        })
        .run(commands);
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

/// A follow-move that appends the window to an already-populated destination
/// strip must bring it fully on-screen. Regression test: the moved window
/// keeps focus, so no `Added<FocusedMarker>` fires to trigger the reshuffle,
/// and it used to land off the right edge until manually centered.
#[test]
fn test_follow_move_brings_appended_window_on_screen() {
    // Enough windows that the destination strip overflows the display width
    // (each window is 400px wide, display is 1024px), so an appended window
    // lands off the right edge unless the strip scrolls to expose it.
    let mut h = TestHarness::new().with_windows(5);

    let pump = |h: &mut TestHarness, cmd: Command| {
        h.app
            .world_mut()
            .write_message::<Event>(Event::Command { command: cmd });
        for _ in 0..8 {
            h.app.update();
            for event in h.mock_state.drain_events() {
                h.app.world_mut().write_message::<Event>(event);
            }
        }
    };

    // Seed VW1 with three windows (Stay keeps us on VW0), making the
    // destination strip wider than the display before the follow-move appends
    // to it.
    for _ in 0..3 {
        pump(
            &mut h,
            Command::Window(Operation::VirtualMoveNumber(1, MoveFocus::Stay)),
        );
    }

    let mover = focused_window_id(h.app.world_mut());

    pump(
        &mut h,
        Command::Window(Operation::VirtualMoveNumber(1, MoveFocus::Follow)),
    );

    assert_focused!(h.app.world_mut(), mover);
    let frame = {
        let world = h.app.world_mut();
        let mut q = world.query::<&Window>();
        q.iter(world)
            .find(|w| w.id() == mover)
            .expect("moved window")
            .frame()
    };
    assert!(
        frame.min.x >= 0 && frame.max.x <= TEST_DISPLAY_WIDTH,
        "moved window must be fully on-screen, got frame x {}..{} (display width {})",
        frame.min.x,
        frame.max.x,
        TEST_DISPLAY_WIDTH,
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
        let mut q = world.query_filtered::<&Position, With<ActiveWorkspaceMarker>>();
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
        let mut q = world.query_filtered::<&Position, With<ActiveWorkspaceMarker>>();
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

/// Switching virtual workspaces with `virtual_workspace_animations = false`
/// must switch focus to the focused window of the destination workspace.
#[test]
fn test_virtual_workspace_switch_restores_focus_without_animations() {
    let config: Config = (
        MainOptions {
            virtual_workspace_animations: Some(false),
            ..Default::default()
        },
        vec![],
    )
        .into();

    let mut h = TestHarness::new().with_config(config).with_windows(3);

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

    // Boot: Window 0 is focused on VW0 (workspace_virtual_num = 0).
    pump(&mut h, Command::PrintState);

    // Move focused window (Window 0) to VW1 with MoveFocus::Stay.
    pump(
        &mut h,
        Command::Window(Operation::VirtualMoveNumber(1, MoveFocus::Stay)),
    );

    // Focus on VW0 should have shifted to Window 1.
    let focused_on_vw0 = {
        let world = h.app.world_mut();
        let mut query = world.query_filtered::<&crate::manager::Window, With<FocusedMarker>>();
        query.iter(world).next().map(|w| w.id())
    };
    assert_eq!(
        focused_on_vw0,
        Some(1),
        "focus should remain on VW0 (shifting to Window 1) after MoveFocus::Stay"
    );

    // Switch to VW1: Window 0 (the window on VW1) should now be focused.
    pump(&mut h, Command::Window(Operation::VirtualNumber(1)));

    let focused_on_vw1 = {
        let world = h.app.world_mut();
        let mut query = world.query_filtered::<&crate::manager::Window, With<FocusedMarker>>();
        query.iter(world).next().map(|w| w.id())
    };
    assert_eq!(
        focused_on_vw1,
        Some(0),
        "focus should switch to Window 0 when activating VW1 with animations disabled"
    );

    // Switch back to VW0: Window 1 (the window on VW0) should be focused again.
    pump(&mut h, Command::Window(Operation::VirtualNumber(0)));

    let focused_back_on_vw0 = {
        let world = h.app.world_mut();
        let mut query = world.query_filtered::<&crate::manager::Window, With<FocusedMarker>>();
        query.iter(world).next().map(|w| w.id())
    };
    assert_eq!(
        focused_back_on_vw0,
        Some(1),
        "focus should switch back to Window 1 when activating VW0 with animations disabled"
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
        let mut q = world.query_filtered::<&Position, With<ActiveWorkspaceMarker>>();
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
    let mut q = world.query_filtered::<&Position, With<ActiveWorkspaceMarker>>();
    let final_x = q.single(world).expect("exactly one active strip").0.x;
    assert_eq!(
        final_x, saved_x,
        "strip x drifted after restore: was {saved_x}, now {final_x}"
    );
}

/// A virtual-workspace switch restores the strip's saved scroll position and
/// refocuses the remembered window. macOS acknowledges that focus several
/// ticks later, after `reshuffle_layout_strip`'s guards have expired; with
/// `auto_center` on, that late acknowledgment used to re-center the strip and
/// undo the restore — a "wiggle" on every switch.
#[test]
fn test_virtual_workspace_switch_focus_echo_does_not_recenter_strip() {
    let config: Config = (
        MainOptions {
            auto_center: Some(true),
            virtual_workspace_animations: Some(false),
            animation_speed: Some(12.0),
            ..Default::default()
        },
        vec![],
    )
        .into();

    let mut h = TestHarness::new().with_config(config).with_windows(6);

    // A second of simulated time, which is what the auto-centre animation
    // needs to reach its target. Expressed as a duration rather than a frame
    // count so it stays a second whatever the harness tick is: a settle that
    // stops short leaves an animation still in flight, and the displacement
    // below then gets overwritten by the tail of it.
    let settle = |h: &mut TestHarness| h.advance(Duration::from_secs(1));
    let pump = |h: &mut TestHarness, c: Command| {
        h.app
            .world_mut()
            .write_message::<Event>(Event::Command { command: c });
        settle(h);
    };
    let strip_x = |h: &mut TestHarness| -> i32 {
        let world = h.app.world_mut();
        let mut q = world.query_filtered::<&Position, With<ActiveWorkspaceMarker>>();
        q.single(world).expect("exactly one active strip").0.x
    };

    pump(&mut h, Command::PrintState);

    // Seed VW1 with one window so focus genuinely moves across the switch.
    h.mock_state.focus_window(5);
    settle(&mut h);
    pump(
        &mut h,
        Command::Window(Operation::VirtualMoveNumber(1, MoveFocus::Stay)),
    );

    // Focus window 0 and let auto-center settle the strip on it.
    h.mock_state.focus_window(0);
    settle(&mut h);
    let centered_x = strip_x(&mut h);

    // Displace the strip directly instead of swiping: the swipe pipeline's
    // finger-lift threshold is wall-clock based, which makes event order
    // load-dependent and the test flaky.
    let saved_x = centered_x - 250;
    {
        let world = h.app.world_mut();
        let mut q = world.query_filtered::<&mut Position, With<ActiveWorkspaceMarker>>();
        q.single_mut(world).expect("exactly one active strip").0.x = saved_x;
    }
    settle(&mut h);
    assert_eq!(
        strip_x(&mut h),
        saved_x,
        "test setup: the displaced strip position must stick"
    );

    // Switch to VW1; deliver the OS focus acknowledgment for its window.
    pump(&mut h, Command::Window(Operation::VirtualNumber(1)));
    h.mock_state.focus_window(5);
    settle(&mut h);

    // Switch back to VW0.
    pump(&mut h, Command::Window(Operation::VirtualNumber(0)));
    assert_eq!(
        strip_x(&mut h),
        saved_x,
        "strip must restore to its saved position on switch-back"
    );

    // Delayed focus acknowledgment for the remembered window.
    h.mock_state.focus_window(0);
    settle(&mut h);
    let final_x = strip_x(&mut h);
    assert_eq!(
        final_x, saved_x,
        "late focus acknowledgment re-centered the strip (wiggle): \
         restored to {saved_x}, ended at {final_x} (centered would be {centered_x})"
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
            continuous_swipe: Some(false),
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
    use Position;

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
/// view. Regression: `stack_windows_handler` mutated the strip but never
/// reshuffled, so when the strip was scrolled such that the focused window's
/// new column slot fell off-screen, the window stayed partially or fully
/// invisible even though it kept focus. It now reshuffles around the focused
/// window; the edge-clamp in `reshuffle_layout_strip` keeps the strip pinned to
/// the edges.
#[test]
fn test_stack_unstack_brings_focused_window_into_view() {
    fn check_if_offscreen(world: &mut World, _state: MockState) {
        let mut q = world.query_filtered::<(&Window, &Position), With<crate::ecs::FocusedMarker>>();
        let (_, position) = q.single(world).expect("a focused window");

        assert!(
            position.x < -(TEST_WINDOW_WIDTH / 4),
            "focused window should be somewhat offscreen after the scroll."
        );
    }

    let config: Config = (
        MainOptions {
            focus_follows_mouse: Some(false),
            auto_center: Some(false),
            animation_speed: Some(10000.0),
            swipe_gesture_fingers: Some(3),
            ..Default::default()
        },
        vec![],
    )
        .into();

    // 5 windows @ 400px = 2000px strip on a 1024px display → scrollable.
    let harness = TestHarness::new().with_config(config).with_windows(5);

    let commands = vec![
        Event::MenuOpened { window_id: 0 },
        Event::Command {
            command: Command::Window(Operation::Focus(Direction::East)),
        },
        // Swipe windows 0 and 1 off screen.
        Event::Swipe {
            delta: 0.3,
            fingers: 3,
        },
        // Noop to let the scroll settle.
        Event::MenuOpened { window_id: 0 },
        Event::Command {
            command: Command::Window(Operation::Stack(true)),
        },
        // Now swipe the stacked windows off screen again.
        Event::Swipe {
            delta: 0.1,
            fingers: 3,
        },
        // Noop to let the scroll settle.
        Event::MenuOpened { window_id: 0 },
        Event::Command {
            command: Command::Window(Operation::Stack(false)),
        },
    ];

    harness
        .on_iteration(3, check_if_offscreen)
        .on_iteration(4, |world, _state| {
            // Check that both window are stacked and moved into view.
            assert_window_at!(world, 0, 0, 20);
            assert_window_at!(world, 1, 0, 394);
        })
        .on_iteration(5, check_if_offscreen)
        .on_iteration(7, |world, _state| {
            // Check that both window are stacked and moved into view.
            assert_window_at!(world, 1, 0, 20);
        })
        .run(commands);
}

/// A window parked on a hidden virtual row must stay parked when its app
/// hides and re-shows itself (e.g. 1Password self-activating periodically),
/// which runs the whole unmanage/remanage cycle unprompted. Regression: the
/// remanage path used to reshuffle around the window's popped frame, dragging
/// the hidden strip back on screen and making the window unreachable to
/// commands that only act on the active strip.
#[test]
fn test_app_self_activation_keeps_window_parked_on_hidden_virtual_row() {
    /// Position of the parked window and of the hidden strip holding it.
    fn parked_state(world: &mut World) -> (Origin, Origin) {
        let entity = find_window_entity(0, world);
        let mut strips = world.query::<(&LayoutStrip, &Position, Has<ActiveWorkspaceMarker>)>();
        let (strip_position, active) = strips
            .iter(world)
            .find_map(|(strip, position, active)| {
                (strip.virtual_index == 1 && strip.contains(entity)).then_some((position.0, active))
            })
            .expect("window 0 parked on the hidden virtual row");
        assert!(!active, "virtual row 1 must not be the active one");

        let mut windows = world.query_filtered::<&Position, With<Window>>();
        let window_position = windows.get(world, entity).expect("window 0 position").0;
        (window_position, strip_position)
    }

    let commands = vec![
        Event::MenuOpened { window_id: 0 },
        // Park the focused window on VW1 while VW0 stays on screen.
        Event::Command {
            command: Command::Window(Operation::VirtualMoveNumber(1, MoveFocus::Stay)),
        },
        Event::Command {
            command: Command::PrintState,
        },
        // The app hides and re-shows itself, unmanaging and remanaging the
        // parked window.
        Event::ApplicationHidden {
            pid: TEST_PROCESS_ID,
        },
        Event::ApplicationVisible {
            pid: TEST_PROCESS_ID,
        },
        Event::Command {
            command: Command::PrintState,
        },
    ];

    let parked = std::rc::Rc::new(std::cell::RefCell::new(None));
    let parked_after = parked.clone();

    TestHarness::new()
        .with_windows(2)
        .on_iteration(2, move |world, _state| {
            parked.replace(Some(parked_state(world)));
        })
        .on_iteration(5, move |world, _state| {
            let (window_before, strip_before) =
                parked_after.borrow().expect("parked state was captured");
            let (window_after, strip_after) = parked_state(world);

            assert_eq!(
                window_after, window_before,
                "parked window must keep its off-screen frame across the hide/show cycle"
            );
            assert_eq!(
                strip_after, strip_before,
                "hidden virtual row must not be dragged back on screen"
            );
        })
        .run(commands);
}

/// A `WindowMoved` notification for a window paneru is not currently moving is
/// the app (or the user) moving it, and the layout must take that new origin on
/// board.
#[test]
fn test_foreign_window_move_is_adopted() {
    let commands = vec![
        Event::MenuOpened { window_id: 0 },
        Event::Command {
            command: Command::PrintState,
        },
        Event::Command {
            command: Command::PrintState,
        },
    ];

    let config: Config = (
        MainOptions {
            // Snappy, so no `RepositionMarker` is still in flight when the
            // notification below arrives.
            animation_speed: Some(10000.0),
            ..Default::default()
        },
        vec![],
    )
        .into();

    TestHarness::new()
        .with_config(config)
        .with_windows(2)
        .on_iteration(0, |_world, state| {
            state.os_move_window(0, Origin::new(77, 88));
        })
        .on_iteration(2, |world, _state| {
            let entity = find_window_entity(0, world);
            let position = world.get::<Position>(entity).expect("window position");
            assert_eq!(
                position.0,
                Origin::new(77, 88),
                "a move paneru did not make must be read back into the layout"
            );
        })
        .run(commands);
}

/// A `WindowMoved` echo of a move paneru itself just made must not perturb the
/// in-flight animation — reading it back naively made the animation and the
/// echo chase each other, causing jitter on every reflow.
///
/// Driven directly at the system instead of through the harness loop: the mock
/// applies its reposition synchronously, so a normal frame would resolve the
/// move before the notification could ever be read back.
#[test]
fn test_own_window_move_echo_is_ignored() {
    use bevy::ecs::system::RunSystemOnce as _;

    let mut harness = TestHarness::new().with_windows(2);
    harness.app.update();

    let state = harness.mock_state.clone();
    let world = harness.world();
    let entity = find_window_entity(0, world);
    let before = world.get::<Position>(entity).expect("window position").0;

    // A move of ours is in flight, and the app reports a frame we didn't ask
    // for. Displaced on the axis the animation leaves alone, so the assertion
    // can't be confused by how far the lerp has run.
    world
        .entity_mut(entity)
        .insert(RepositionMarker(Origin::new(5000, before.y)));
    state.os_move_window(0, Origin::new(before.x, before.y + 888));
    world.write_message(Event::WindowMoved { window_id: 0 });

    world
        .run_system_once(crate::ecs::systems::window_moved_update_frame)
        .expect("running window_moved_update_frame");

    assert_eq!(
        world.get::<Position>(entity).expect("window position").0,
        before,
        "the echo of our own move must not be read back over the animation"
    );
}

#[test]
fn test_virtual_directions_first_last_east_west() {
    use crate::config::{Config, MainOptions};

    let config: Config = (
        MainOptions {
            reap_empty_workspaces: Some(false),
            ..Default::default()
        },
        vec![],
    )
        .into();

    let commands = vec![
        // iteration 0: Create VW1
        Event::Command {
            command: Command::Window(Operation::VirtualAdd),
        },
        // iteration 1: Create VW2
        Event::Command {
            command: Command::Window(Operation::VirtualAdd),
        },
        // iteration 2: Switch First -> VW0
        Event::Command {
            command: Command::Window(Operation::Virtual(Direction::First)),
        },
        // iteration 3: Switch East (alias for South/next) -> VW1
        Event::Command {
            command: Command::Window(Operation::Virtual(Direction::East)),
        },
        // iteration 4: Switch Last -> VW2
        Event::Command {
            command: Command::Window(Operation::Virtual(Direction::Last)),
        },
        // iteration 5: Switch West (alias for North/prev) -> VW1
        Event::Command {
            command: Command::Window(Operation::Virtual(Direction::West)),
        },
        // iteration 6: Switch First -> VW0
        Event::Command {
            command: Command::Window(Operation::Virtual(Direction::First)),
        },
        // iteration 7: Move focused window to Last with Follow -> moves to VW2 & follows to VW2
        Event::Command {
            command: Command::Window(Operation::VirtualMove(Direction::Last, MoveFocus::Follow)),
        },
        // iteration 8: Move focused window to First with Follow -> moves to VW0 & follows to VW0
        Event::Command {
            command: Command::Window(Operation::VirtualMove(Direction::First, MoveFocus::Follow)),
        },
    ];

    let assert_active_vw = |expected: u32| {
        move |world: &mut World, _state: MockState| {
            let mut query = world.query::<(&LayoutStrip, Has<ActiveWorkspaceMarker>)>();
            let active = query
                .iter(world)
                .find_map(|(strip, active)| active.then_some(strip.virtual_index))
                .expect("an active virtual strip");
            assert_eq!(active, expected);
        }
    };

    TestHarness::new()
        .with_config(config)
        .with_windows(6)
        .on_iteration(2, assert_active_vw(0))
        .on_iteration(3, assert_active_vw(1))
        .on_iteration(4, assert_active_vw(2))
        .on_iteration(5, assert_active_vw(1))
        .on_iteration(6, assert_active_vw(0))
        .on_iteration(7, assert_active_vw(2))
        .on_iteration(8, assert_active_vw(0))
        .run(commands);
}

/// Focusing an unknown window (e.g. clicking an unmanaged tab or window)
/// must trigger auto-discovery and manage it on the fly into ECS.
#[test]
fn test_auto_discover_unmanaged_focused_window() {
    let events = vec![
        // Iteration 0: Boot with 1 managed window (window 0).
        Event::Command {
            command: Command::PrintState,
        },
        // Iteration 1: Send focus event for unmanaged window 1.
        Event::WindowFocused { window_id: 1 },
    ];

    TestHarness::new()
        .with_windows(1)
        .on_iteration(0, |world, state| {
            // Verify window 1 is not in ECS yet.
            let mut query = world.query::<&crate::manager::Window>();
            let is_managed = query.iter(world).any(|w| w.id() == 1);
            assert!(!is_managed, "Window 1 should not be managed yet");

            // Spawn window 1 in mock state without sending AX notification to Paneru (unmanaged tab).
            state.spawn_window(
                TEST_PROCESS_ID,
                TEST_WORKSPACE_ID,
                1,
                bevy::math::IRect::from_corners(
                    bevy::math::IVec2::new(0, 0),
                    bevy::math::IVec2::new(400, 400),
                ),
            );
            state.focus_window(1);
        })
        .on_iteration(1, |world, _state| {
            // Verify window 1 is now auto-discovered and managed in ECS.
            let mut query = world.query::<&crate::manager::Window>();
            let is_managed = query.iter(world).any(|w| w.id() == 1);
            assert!(
                is_managed,
                "Window 1 should be auto-discovered and managed in ECS after receiving focus"
            );
        })
        .run(events);
}

/// The centering config the manual-offset tests share: `auto_center` off and
/// `continuous_swipe` off is the combination that arms the edge invariant in
/// `reshuffle_layout_strip`, which is what used to snap a centered strip back
/// to the display's left edge.
fn manual_offset_config() -> Config {
    (
        MainOptions {
            auto_center: Some(false),
            continuous_swipe: Some(false),
            animation_speed: Some(10000.0),
            swipe_gesture_fingers: Some(3),
            ..Default::default()
        },
        vec![],
    )
        .into()
}

/// Where window 0 sits once `Operation::Center` has placed it.
const CENTERED_X: i32 = (TEST_DISPLAY_WIDTH - TEST_WINDOW_WIDTH) / 2;

fn active_strip_entity(world: &mut World) -> Entity {
    let mut query =
        world.query_filtered::<Entity, (With<LayoutStrip>, With<ActiveWorkspaceMarker>)>();
    query.single(world).expect("exactly one active strip")
}

fn has_manual_offset(world: &mut World) -> bool {
    let entity = active_strip_entity(world);
    world.get::<ManualStripOffset>(entity).is_some()
}

fn window_x(world: &mut World, id: WinID) -> i32 {
    let mut query = world.query::<&Window>();
    query
        .iter(world)
        .find(|window| window.id() == id)
        .expect("window not found")
        .frame()
        .min
        .x
}

/// A repeated OS focus event for the window that already holds focus — what a
/// browser emits when a new tab opens — is not new layout information. It used
/// to run a full reshuffle, which re-derived the strip offset and threw away a
/// manual centering.
#[test]
fn test_center_survives_repeated_focus_event() {
    let commands = vec![
        // 0: boot with focus on window 0.
        Event::MenuOpened { window_id: 0 },
        // 1: center it.
        Event::Command {
            command: Command::Window(Operation::Center),
        },
        // 2: the repeated focus event queued below plays out here.
        Event::Command {
            command: Command::PrintState,
        },
    ];

    TestHarness::new()
        .with_config(manual_offset_config())
        .with_windows(5)
        .on_iteration(1, |world, state| {
            assert_eq!(window_x(world, 0), CENTERED_X, "window 0 must be centered");
            // The app re-announces the window that already holds focus.
            state.focus_window(0);
        })
        .on_iteration(2, |world, _state| {
            assert_eq!(
                window_x(world, 0),
                CENTERED_X,
                "a repeated focus event must not undo the centering"
            );
            assert!(
                has_manual_offset(world),
                "the manual offset must survive a repeated focus event"
            );
        })
        .run(commands);
}

/// The offset is only a claim about the layout it was taken on. Adding a window
/// changes that layout, so the next reshuffle derives a fresh offset and the
/// strip goes back to the edge invariant.
#[test]
fn test_center_dropped_when_window_added() {
    let commands = vec![
        Event::MenuOpened { window_id: 0 },
        // 1: center window 0.
        Event::Command {
            command: Command::Window(Operation::Center),
        },
        // 2: the sixth window spawned below joins the strip.
        Event::Command {
            command: Command::PrintState,
        },
        // 3: a focus change asks for a reshuffle on the new layout.
        Event::Command {
            command: Command::Window(Operation::Focus(Direction::East)),
        },
    ];

    TestHarness::new()
        .with_config(manual_offset_config())
        .with_windows(5)
        .on_iteration(1, |world, state| {
            assert_eq!(window_x(world, 0), CENTERED_X, "window 0 must be centered");

            let window = state.spawn_window(
                TEST_PROCESS_ID,
                TEST_WORKSPACE_ID,
                5,
                IRect::new(0, 0, TEST_WINDOW_WIDTH, TEST_WINDOW_HEIGHT),
            );
            world.trigger(SpawnWindowTrigger(vec![window]));
        })
        .on_iteration(3, |world, _state| {
            assert_eq!(
                window_x(world, 0),
                0,
                "a reshuffle on a changed layout must re-derive the offset"
            );
            assert!(
                !has_manual_offset(world),
                "adding a window must invalidate the manual offset"
            );
        })
        .run(commands);
}

/// Skipping the reshuffle must not strand a window off-screen: if the strip has
/// scrolled the focused window past an edge, a focus event still has to bring
/// it back — and the manual placement it contradicts is dropped with it.
#[test]
fn test_partly_hidden_window_still_scrolled_into_view_after_center() {
    let commands = vec![
        Event::MenuOpened { window_id: 0 },
        // 1: center window 0.
        Event::Command {
            command: Command::Window(Operation::Center),
        },
        // 2: the strip is dragged left below, hiding half of window 0.
        Event::Command {
            command: Command::PrintState,
        },
        // 3: the focus event queued below plays out here.
        Event::Command {
            command: Command::PrintState,
        },
    ];

    TestHarness::new()
        .with_config(manual_offset_config())
        .with_windows(5)
        .on_iteration(1, |world, _state| {
            assert_eq!(window_x(world, 0), CENTERED_X, "window 0 must be centered");

            // Push the strip a full window width further left than the
            // centering did, which drags window 0 off the left edge.
            let strip_entity = active_strip_entity(world);
            let displaced = {
                let position = world.get::<Position>(strip_entity).expect("strip position");
                Origin::new(position.0.x - TEST_WINDOW_WIDTH, position.0.y)
            };
            world.entity_mut(strip_entity).insert(Position(displaced));
        })
        .on_iteration(2, |_world, state| {
            state.focus_window(0);
        })
        .on_iteration(3, |world, _state| {
            assert!(
                window_x(world, 0) >= 0,
                "a window scrolled off the left edge must be brought back into view, got {}",
                window_x(world, 0)
            );
            assert!(
                !has_manual_offset(world),
                "a manual offset that hides its own window is stale"
            );
        })
        .run(commands);
}

/// Switching virtual workspaces re-places the strip from its saved origin, so
/// the manual claim on the old offset goes away with the switch.
#[test]
fn test_center_dropped_on_virtual_workspace_switch() {
    let commands = vec![
        Event::MenuOpened { window_id: 0 },
        // 1: center window 0.
        Event::Command {
            command: Command::Window(Operation::Center),
        },
        // 2: move a window to VW1 and follow it there.
        Event::Command {
            command: Command::Window(Operation::VirtualMoveNumber(1, MoveFocus::Follow)),
        },
        // 3: back to VW0.
        Event::Command {
            command: Command::Window(Operation::VirtualNumber(0)),
        },
    ];

    TestHarness::new()
        .with_config(manual_offset_config())
        .with_windows(5)
        .on_iteration(1, |world, _state| {
            assert!(has_manual_offset(world), "center must mark the strip");
        })
        .on_iteration(3, |world, _state| {
            assert!(
                !has_manual_offset(world),
                "a workspace switch must invalidate the manual offset"
            );
        })
        .run(commands);
}

/// Once the user drives the strip by hand, the earlier placement no longer
/// describes where they want it.
#[test]
fn test_center_dropped_on_user_swipe() {
    let commands = vec![
        Event::MenuOpened { window_id: 0 },
        // 1: center window 0.
        Event::Command {
            command: Command::Window(Operation::Center),
        },
        // 2: the user swipes the strip.
        Event::Swipe {
            delta: -0.2,
            fingers: 3,
        },
    ];

    TestHarness::new()
        .with_config(manual_offset_config())
        .with_windows(5)
        .on_iteration(1, |world, _state| {
            assert!(has_manual_offset(world), "center must mark the strip");
        })
        .on_iteration(2, |world, _state| {
            assert!(
                !has_manual_offset(world),
                "a user swipe must invalidate the manual offset"
            );
        })
        .run(commands);
}

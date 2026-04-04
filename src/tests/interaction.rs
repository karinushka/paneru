use std::sync::{Arc, RwLock};

use bevy::prelude::*;
use tracing::debug;

use crate::commands::{Command, Direction, Operation};
use crate::config::{Config, MainOptions, WindowParams};
use crate::ecs::{FocusedMarker, SpawnWindowTrigger};
use crate::events::Event;
use crate::manager::{Origin, Size, Window, WindowManager};

use super::*;

#[test]
fn test_dont_focus() {
    let commands = vec![
        Event::MenuOpened { window_id: 0 }, // Noop allowing everything to settle
        Event::Command {
            command: Command::Window(Operation::Focus(Direction::Last)),
        },
        Event::Command {
            command: Command::Window(Operation::Focus(Direction::First)),
        },
        Event::Command {
            command: Command::PrintState,
        },
    ];

    let offscreen_right = TEST_DISPLAY_WIDTH - 5;
    let expected_positions = [
        (2, (0, TEST_MENUBAR_HEIGHT)),
        (1, (400, TEST_MENUBAR_HEIGHT)),
        (0, (800, TEST_MENUBAR_HEIGHT)),
        (3, (offscreen_right, TEST_MENUBAR_HEIGHT)),
    ];

    let mut bevy = setup_world();
    let app = setup_process(bevy.world_mut());
    let mock_app = app.clone();
    let internal_queue = Arc::new(RwLock::new(Vec::<Event>::new()));
    let event_queue = internal_queue.clone();
    let windows = window_spawner(3, event_queue, mock_app);
    let window_manager = MockWindowManager {
        windows,
        workspaces: vec![TEST_WORKSPACE_ID],
    };
    bevy.world_mut()
        .insert_resource(WindowManager(Box::new(window_manager)));

    let check_queue = internal_queue.clone();
    let check = |iteration, world: &mut World| {
        let iterations = [None, None, None, Some(expected_positions.as_slice())];

        if let Some(positions) = iterations[iteration] {
            debug!("Iteration: {iteration}");
            verify_window_positions(positions, world);

            let mut query = world.query::<(&Window, Has<FocusedMarker>)>();
            for (window, focused) in query.iter(world) {
                if focused {
                    // Check that focus stayed on the first window.
                    assert_eq!(window.id(), 2);
                }
            }
        }

        if iteration == 1 {
            let origin = Origin::new(0, 0);
            let size = Size::new(TEST_WINDOW_WIDTH, TEST_WINDOW_HEIGHT);
            let window = MockWindow::new(
                3,
                IRect {
                    min: origin,
                    max: origin + size,
                },
                check_queue.clone(),
                app.clone(),
            );
            let window = Window::new(Box::new(window));
            world.trigger(SpawnWindowTrigger(vec![window]));
        }
    };

    let mut params = WindowParams::new(".*", None);
    params.dont_focus = Some(true);
    params.index = Some(100);
    let config: Config = (MainOptions::default(), vec![params]).into();
    bevy.insert_resource(config);

    run_main_loop(&mut bevy, &internal_queue, &commands, check);
}

/// Off-screen windows should keep the same height as on-screen windows
/// when `sliver_height` is 1.0 (the default). A previous bug subtracted
/// `menubar_height` from off-screen window heights, causing a visible
/// resize when they came into focus.
#[test]
fn test_offscreen_windows_preserve_height() {
    let expected_height = TEST_DISPLAY_HEIGHT - TEST_MENUBAR_HEIGHT;

    let commands = vec![
        Event::MenuOpened { window_id: 0 }, // Settle
        Event::Command {
            command: Command::Window(Operation::Focus(Direction::First)),
        },
    ];

    let expected_sizes = [
        (4, (TEST_WINDOW_WIDTH, expected_height)),
        (3, (TEST_WINDOW_WIDTH, expected_height)),
        (2, (TEST_WINDOW_WIDTH, expected_height)),
        (1, (TEST_WINDOW_WIDTH, expected_height)),
        (0, (TEST_WINDOW_WIDTH, expected_height)),
    ];

    let check = |iteration, world: &mut World| {
        if iteration == 1 {
            verify_window_sizes(&expected_sizes, world);
        }
    };

    let mut bevy = setup_world();
    let mock_app = setup_process(bevy.world_mut());
    let internal_queue = Arc::new(RwLock::new(Vec::<Event>::new()));
    let event_queue = internal_queue.clone();
    let windows = window_spawner(5, event_queue, mock_app);
    let window_manager = MockWindowManager {
        windows,
        workspaces: vec![TEST_WORKSPACE_ID],
    };
    bevy.world_mut()
        .insert_resource(WindowManager(Box::new(window_manager)));

    run_main_loop(&mut bevy, &internal_queue, &commands, check);
}

/// When `sliver_width` is smaller than `edge_padding`, the off-screen
/// sliver must still be exactly `sliver_width` pixels from the real
/// display edge. A previous bug used `max(sliver, pad) - pad`, which
/// collapsed the sliver to `edge_padding` pixels when `pad > sliver`.
#[test]
fn test_sliver_smaller_than_edge_padding() {
    const PADDING: u16 = 8;
    const SLIVER: u16 = 1;

    let commands = vec![
        Event::MenuOpened { window_id: 0 }, // Settle
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
    // With sliver < padding, off-screen positions are measured from
    // the real display edge, so they go *into* the padding zone.
    let offscreen_right = TEST_DISPLAY_WIDTH - i32::from(SLIVER);
    let offscreen_left = i32::from(SLIVER) - TEST_WINDOW_WIDTH;

    let left_edge = i32::from(PADDING);

    // Focus first: windows 4,3 on-screen, 2 partial, 1,0 off-screen right.
    let expected_first = [
        (4, (left_edge, top_edge)),
        (3, (left_edge + TEST_WINDOW_WIDTH, top_edge)),
        (2, (left_edge + 2 * TEST_WINDOW_WIDTH, top_edge)),
        (1, (offscreen_right, top_edge)),
        (0, (offscreen_right, top_edge)),
    ];

    // Focus last: windows 0,1 on-screen, 2 partial, 3,4 off-screen left.
    let expected_last = [
        (4, (offscreen_left, top_edge)),
        (3, (offscreen_left, top_edge)),
        (2, (right_edge - 3 * TEST_WINDOW_WIDTH, top_edge)),
        (1, (right_edge - 2 * TEST_WINDOW_WIDTH, top_edge)),
        (0, (right_edge - TEST_WINDOW_WIDTH, top_edge)),
    ];

    let check = |iteration, world: &mut World| {
        if iteration == 2 {
            verify_window_positions(&expected_first, world);
        } else if iteration == 3 {
            verify_window_positions(&expected_last, world);
        }
    };

    let mut bevy = setup_world();
    let mock_app = setup_process(bevy.world_mut());
    let internal_queue = Arc::new(RwLock::new(Vec::<Event>::new()));
    let event_queue = internal_queue.clone();
    let windows = window_spawner(5, event_queue, mock_app);
    let window_manager = MockWindowManager {
        windows,
        workspaces: vec![TEST_WORKSPACE_ID],
    };
    bevy.world_mut()
        .insert_resource(WindowManager(Box::new(window_manager)));

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
    bevy.insert_resource(config);

    run_main_loop(&mut bevy, &internal_queue, &commands, check);
}

#[test]
fn test_scrolling() {
    let mut bevy = setup_world();
    let mock_app = setup_process(bevy.world_mut());
    let internal_queue = Arc::new(RwLock::new(Vec::<Event>::new()));
    let event_queue = internal_queue.clone();
    let windows = window_spawner(3, event_queue, mock_app);
    let window_manager = MockWindowManager {
        windows,
        workspaces: vec![TEST_WORKSPACE_ID],
    };
    bevy.world_mut()
        .insert_resource(WindowManager(Box::new(window_manager)));

    let commands = vec![
        Event::MenuOpened { window_id: 0 }, // Noop allowing everything to settle
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

    // Verify initial positions
    let expected_initial = [
        (2, (0, TEST_MENUBAR_HEIGHT)),
        (1, (400, TEST_MENUBAR_HEIGHT)),
        (0, (800, TEST_MENUBAR_HEIGHT)),
    ];

    let expected = [
        (2, (-395, TEST_MENUBAR_HEIGHT)),
        (1, (-395, TEST_MENUBAR_HEIGHT)),
        (0, (0, TEST_MENUBAR_HEIGHT)),
    ];

    let check = |iteration, world: &mut World| {
        let iterations = [
            None,
            None,
            None,
            Some(expected_initial.as_slice()),
            None,
            Some(expected.as_slice()),
        ];

        if let Some(positions) = iterations[iteration] {
            debug!("Iteration: {iteration}");
            verify_window_positions(positions, world);
        }
    };

    let config: Config = (
        MainOptions {
            swipe_gesture_fingers: Some(3),
            ..Default::default()
        },
        vec![],
    )
        .into();
    bevy.insert_resource(config);

    run_main_loop(&mut bevy, &internal_queue, &commands, check);
}

#[test]
fn test_window_hidden_ratio() {
    let mut bevy = setup_world();
    let mock_app = setup_process(bevy.world_mut());
    let internal_queue = Arc::new(RwLock::new(Vec::<Event>::new()));
    let event_queue = internal_queue.clone();
    let windows = window_spawner(2, event_queue, mock_app);
    let window_manager = MockWindowManager {
        windows,
        workspaces: vec![TEST_WORKSPACE_ID],
    };
    bevy.world_mut()
        .insert_resource(WindowManager(Box::new(window_manager)));

    // Set hidden ratio to 0.5 (tolerate up to 50% hidden)
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
    bevy.insert_resource(config);

    let commands = vec![
        Event::MenuOpened { window_id: 0 }, // Settle, window 1 is focused at x=0
        // Swipe left slightly.
        Event::Swipe {
            deltas: vec![0.1, 0.1, 0.1],
        },
        // Now focus it again. It SHOULD NOT move back to x=0.
        Event::Command {
            command: Command::Window(Operation::Focus(Direction::First)),
        },
    ];

    let check = |iteration, world: &mut World| {
        if iteration == 2 {
            let mut query = world.query::<&Window>();
            let window = query.iter(world).find(|w| w.id() == 1).unwrap();
            // Should still be off-screen.
            assert_ne!(window.frame().min.x, 0);
            assert!(window.frame().min.x < 0);
        }
    };

    run_main_loop(&mut bevy, &internal_queue, &commands, check);
}

#[test]
fn test_window_hidden_ratio_swap() {
    let mut bevy = setup_world();
    let mock_app = setup_process(bevy.world_mut());
    let internal_queue = Arc::new(RwLock::new(Vec::<Event>::new()));
    let event_queue = internal_queue.clone();
    let windows = window_spawner(5, event_queue, mock_app);
    let window_manager = MockWindowManager {
        windows,
        workspaces: vec![TEST_WORKSPACE_ID],
    };
    bevy.world_mut()
        .insert_resource(WindowManager(Box::new(window_manager)));

    // Set hidden ratio to 1.0 (never move unless fully hidden)
    // and high animation speed for instant results.
    let config: Config = (
        MainOptions {
            window_hidden_ratio: Some(1.0),
            animation_speed: Some(10000.0),
            ..Default::default()
        },
        vec![],
    )
        .into();
    bevy.insert_resource(config);

    let commands = vec![
        Event::MenuOpened { window_id: 0 }, // Settle, window 1 is focused at x=0
        // Focus second window (id 0). It's at x=400 initially.
        // It's 100% visible, so with hidden_ratio=1.0 it won't move.
        Event::Command {
            command: Command::Window(Operation::Center),
        },
        Event::Command {
            command: Command::Window(Operation::Swap(Direction::Last)),
        },
    ];

    let check = |iteration, world: &mut World| {
        let centered = (TEST_DISPLAY_WIDTH - TEST_WINDOW_WIDTH) / 2;
        if iteration == 1 {
            let mut query = world.query::<&Window>();
            let window = query.iter(world).find(|w| w.id() == 4).unwrap();
            // Should still be at 400 because 0% hidden < 1.0 ratio
            assert_eq!(window.frame().min.x, centered);
        }
        if iteration == 2 {
            let mut query = world.query::<&Window>();
            let window = query.iter(world).find(|w| w.id() == 4).unwrap();
            assert_eq!(window.frame().min.x, centered);
            let window = query.iter(world).find(|w| w.id() == 0).unwrap();
            // The tail of the strip (window 0) is now to the left of window 4.
            assert_eq!(window.frame().min.x, centered - TEST_WINDOW_WIDTH);
        }
    };

    run_main_loop(&mut bevy, &internal_queue, &commands, check);
}

/// Rapid focus keypresses should not get swallowed. When pressing West
/// three times from window 0 (rightmost), focus should land on window 3
/// — each press should advance one step even when the OS event
/// round-trip hasn't completed yet.
///
/// Simulates the race by writing all three commands as messages in one
/// frame before any Bevy update runs, so `FocusedMarker` cannot catch
/// up via mock events between presses.
#[test]
fn test_rapid_focus_not_swallowed() {
    // Phase 1: settle + move focus to last window via normal loop.
    let setup_commands = vec![
        Event::MenuOpened { window_id: 0 },
        Event::Command {
            command: Command::Window(Operation::Focus(Direction::Last)),
        },
    ];

    let check_setup = |iteration, world: &mut World| {
        if iteration == 1 {
            verify_focused_window(0, world);
        }
    };

    let mut bevy = setup_world();
    let mock_app = setup_process(bevy.world_mut());
    let internal_queue = Arc::new(RwLock::new(Vec::<Event>::new()));
    let event_queue = internal_queue.clone();
    let windows = window_spawner(5, event_queue, mock_app);
    let window_manager = MockWindowManager {
        windows,
        workspaces: vec![TEST_WORKSPACE_ID],
    };
    bevy.world_mut()
        .insert_resource(WindowManager(Box::new(window_manager)));

    run_main_loop(&mut bevy, &internal_queue, &setup_commands, check_setup);

    // Phase 2: send three Focus(West) commands, one per frame, but do
    // NOT flush mock events between frames. This simulates keypresses
    // arriving faster than the OS event round-trip can deliver
    // ApplicationFrontSwitched / WindowFocused back to the ECS.
    // Without the immediate FocusedMarker update in command_move_focus,
    // each press would re-target the same window (focus swallowed).
    let focus_west = Event::Command {
        command: Command::Window(Operation::Focus(Direction::West)),
    };
    for _ in 0..3 {
        bevy.world_mut().write_message::<Event>(focus_west.clone());
        bevy.update();
        // Deliberately skip flushing internal_queue — mock events from
        // focus_with_raise stay queued, simulating OS event delay.
    }

    // After three West presses from window 0 (strip order: [4,3,2,1,0]):
    //   0 → 1 → 2 → 3. Focus should be on window 3.
    verify_focused_window(3, bevy.world_mut());
}

/// A stale `WindowFocused` event arriving after focus has moved on should
/// not pull `FocusedMarker` back to the old window.
#[test]
fn test_stale_focus_event_ignored() {
    let commands = vec![
        Event::MenuOpened { window_id: 0 }, // Settle
        Event::Command {
            command: Command::Window(Operation::Focus(Direction::East)),
        },
        // Inject a stale WindowFocused for window 4 (the old focused window)
        // after focus has already moved to window 3.
        Event::WindowFocused { window_id: 4 },
        Event::Command {
            command: Command::PrintState,
        },
    ];

    let check = |iteration, world: &mut World| {
        if iteration == 1 {
            // After Focus(East): strip is [4,3,2,1,0], started at 4, moved to 3.
            verify_focused_window(3, world);
        }
        if iteration == 3 {
            // After the stale event, focus should STILL be on window 3.
            verify_focused_window(3, world);
        }
    };

    let mut bevy = setup_world();
    let mock_app = setup_process(bevy.world_mut());
    let internal_queue = Arc::new(RwLock::new(Vec::<Event>::new()));
    let event_queue = internal_queue.clone();
    let windows = window_spawner(5, event_queue, mock_app);
    let window_manager = MockWindowManager {
        windows,
        workspaces: vec![TEST_WORKSPACE_ID],
    };
    bevy.world_mut()
        .insert_resource(WindowManager(Box::new(window_manager)));

    run_main_loop(&mut bevy, &internal_queue, &commands, check);
}

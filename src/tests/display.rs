use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use bevy::prelude::*;
use bevy::time::TimeUpdateStrategy;

use crate::commands::{Command, MoveFocus, Operation};
use crate::ecs::Timeout;
use crate::ecs::layout::LayoutStrip;
use crate::events::Event;
use crate::manager::{Display, Origin, Size, Window, WindowManager};

use super::*;

#[test]
fn test_multi_display_lifecycle() {
    let mut bevy = setup_world();
    let mock_app = setup_process(bevy.world_mut());
    let internal_queue = Arc::new(RwLock::new(Vec::<Event>::new()));
    let event_queue = internal_queue.clone();
    let windows = window_spawner(1, event_queue, mock_app);
    let window_manager = MockWindowManager {
        windows,
        workspaces: vec![TEST_WORKSPACE_ID],
    };
    bevy.insert_resource(WindowManager(Box::new(window_manager)));
    bevy.insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_millis(
        500,
    )));

    let commands = vec![
        Event::MenuOpened { window_id: 0 }, // Noop allowing everything to settle
        Event::Command {
            command: Command::PrintState,
        },
        Event::DisplayRemoved {
            display_id: TEST_DISPLAY_ID,
        },
        Event::DisplayAdded {
            display_id: TEST_DISPLAY_ID,
        },
    ];

    let check = |iteration, world: &mut World| {
        match iteration {
            1 => {
                // Initial state check: 1 display, 1 workspace attached.
                let _display_entity = {
                    let mut query = world.query_filtered::<Entity, With<Display>>();
                    query.single(world).expect("should have one display")
                };
            }

            2 => {
                // Verify the display is gone and the workspace is orphaned.
                assert!(
                    world
                        .query_filtered::<Entity, With<Display>>()
                        .single(world)
                        .is_err(),
                    "display should be despawned"
                );

                {
                    let workspace_entity = {
                        let mut query = world.query_filtered::<Entity, With<LayoutStrip>>();
                        query.single(world).expect("should have one workspace")
                    };
                    let workspace = world.entity(workspace_entity);
                    assert!(
                        workspace.get::<Timeout>().is_some(),
                        "orphaned workspace should have a timeout"
                    );
                    assert!(
                        workspace.get::<ChildOf>().is_none(),
                        "orphaned workspace should have no parent"
                    );
                }
            }

            3 => {
                // Verify the display is back and the workspace is re-parented.
                let new_display_entity = world
                    .query_filtered::<Entity, With<Display>>()
                    .single(world)
                    .expect("display should be spawned again");

                let workspace_entity = {
                    let mut query = world.query_filtered::<Entity, With<LayoutStrip>>();
                    query.single(world).expect("should have one workspace")
                };
                let workspace = world.entity(workspace_entity);
                assert!(
                    workspace.get::<Timeout>().is_none(),
                    "re-parented workspace should no longer have a timeout"
                );
                let child_of = workspace
                    .get::<ChildOf>()
                    .expect("re-parented workspace should have a parent");
                assert_eq!(
                    child_of.parent(),
                    new_display_entity,
                    "workspace should be child of the new display"
                );
            }

            _ => {}
        }
    };

    run_main_loop(&mut bevy, &internal_queue, &commands, check);
}

#[test]
fn test_multi_workspace_orphaning() {
    let mut bevy = setup_world();
    let mock_app = setup_process(bevy.world_mut());
    let internal_queue = Arc::new(RwLock::new(Vec::<Event>::new()));
    let event_queue = internal_queue.clone();
    let windows = window_spawner(1, event_queue, mock_app);
    let window_manager = MockWindowManager {
        windows,
        workspaces: vec![TEST_WORKSPACE_ID, TEST_WORKSPACE_ID + 1],
    };
    bevy.world_mut()
        .insert_resource(WindowManager(Box::new(window_manager)));

    let commands = vec![
        Event::MenuOpened { window_id: 0 }, // Noop allowing everything to settle
        Event::Command {
            command: Command::PrintState,
        },
        Event::DisplayRemoved {
            display_id: TEST_DISPLAY_ID,
        },
    ];

    let check = |iteration, world: &mut World| {
        let workspace_entities = world
            .query_filtered::<Entity, With<LayoutStrip>>()
            .iter(world)
            .collect::<Vec<_>>();
        match iteration {
            1 => {
                // Verify initial state: 1 display, 2 workspaces.
                let display_entity = world
                    .query_filtered::<Entity, With<Display>>()
                    .single(world)
                    .expect("should have one display");

                assert_eq!(workspace_entities.len(), 2, "should have two workspaces");

                for &ws in &workspace_entities {
                    let child_of = world
                        .entity(ws)
                        .get::<ChildOf>()
                        .expect("workspace should have parent");
                    assert_eq!(child_of.parent(), display_entity);
                }
            }
            2 => {
                // Verify both workspaces are orphaned.
                for &ws in &workspace_entities {
                    let entity = world.entity(ws);
                    assert!(
                        entity.get::<Timeout>().is_some(),
                        "each workspace should have a timeout"
                    );
                    assert!(
                        entity.get::<ChildOf>().is_none(),
                        "each workspace should have no parent"
                    );
                }
            }
            _ => {}
        }
    };

    run_main_loop(&mut bevy, &internal_queue, &commands, check);
}

/// Regression test: switching focus to a shorter (internal) display must not
/// resize windows on the taller (external) display.  Before the fix,
/// `layout_strip_changed` used the active display's viewport height for ALL
/// strips, so the external strip's windows shrank to the internal height.
#[test]
fn test_multi_display_no_height_crosstalk() {
    let mut bevy = setup_world();
    let mock_app = setup_process(bevy.world_mut());
    let internal_queue = Arc::new(RwLock::new(Vec::<Event>::new()));
    let event_queue = internal_queue.clone();

    let active_display = Arc::new(AtomicU32::new(EXT_DISPLAY_ID));

    // External display gets one window (id 100), internal gets one (id 200).
    let eq1 = event_queue.clone();
    let eq2 = event_queue.clone();
    let app1 = mock_app.clone();
    let app2 = mock_app;
    let windows: TestWindowSpawner = Box::new(move |workspace_id| {
        if workspace_id == EXT_WORKSPACE_ID {
            let origin = Origin::new(0, 0);
            let size = Size::new(TEST_WINDOW_WIDTH, TEST_WINDOW_HEIGHT);
            vec![Window::new(Box::new(MockWindow::new(
                100,
                IRect::from_corners(origin, origin + size),
                eq1.clone(),
                app1.clone(),
            )))]
        } else if workspace_id == TEST_WORKSPACE_ID {
            let origin = Origin::new(0, 0);
            let size = Size::new(TEST_WINDOW_WIDTH, TEST_WINDOW_HEIGHT);
            vec![Window::new(Box::new(MockWindow::new(
                200,
                IRect::from_corners(origin, origin + size),
                eq2.clone(),
                app2.clone(),
            )))]
        } else {
            vec![]
        }
    });

    let window_manager = TwoDisplayMock {
        windows,
        active_display: active_display.clone(),
    };
    bevy.insert_resource(WindowManager(Box::new(window_manager)));

    // Expected height on the external display = display height - menubar.
    let ext_usable_height = EXT_DISPLAY_HEIGHT - TEST_MENUBAR_HEIGHT;

    let commands = vec![
        // 0: Settle — let initialization complete.
        Event::MenuOpened { window_id: 100 },
        // 1: Print to verify initial layout.
        Event::Command {
            command: Command::PrintState,
        },
        // 2: Simulate switching focus to the internal display.
        //    The mock's active_display_id will have been flipped in the
        //    verifier at iteration 1, and DisplayChanged triggers the
        //    ActiveDisplayMarker move + workspace switch.
        Event::DisplayChanged,
        // 3: Noop — the verifier for iteration 2 marks the external strip
        //    as Changed, simulating any mutation (window add/remove/tab-switch)
        //    that would touch the strip after a display switch.
        //    This iteration's updates run layout_strip_changed on it.
        Event::MenuOpened { window_id: 100 },
        // 4: Print to verify final layout.
        Event::Command {
            command: Command::PrintState,
        },
    ];

    let ad = active_display.clone();
    let check = move |iteration, world: &mut World| {
        match iteration {
            1 => {
                // After settling, external window should have the external
                // display's usable height.
                verify_window_sizes(&[(100, (TEST_WINDOW_WIDTH, ext_usable_height))], world);

                // Now switch the mock's active display so the next
                // DisplayChanged event picks it up.
                ad.store(TEST_DISPLAY_ID, Ordering::Relaxed);
            }
            2 => {
                // After the display switch, simulate a strip mutation on
                // the non-active (external) display.  In practice this
                // happens when window_focused_trigger or window_removal
                // touch the strip via DerefMut.
                use crate::ecs::ActiveWorkspaceMarker;
                let mut strip_query =
                    world.query_filtered::<&mut LayoutStrip, Without<ActiveWorkspaceMarker>>();
                // `iter_mut` yields `Mut<LayoutStrip>` — dereferencing
                // mutably triggers Bevy's `Changed` detection.
                for mut strip in strip_query.iter_mut(world) {
                    strip.set_changed();
                }
            }
            4 => {
                // After layout_strip_changed ran on the Changed external
                // strip, window 100 must still have the external display's
                // height — NOT the internal display's shorter height.
                verify_window_sizes(&[(100, (TEST_WINDOW_WIDTH, ext_usable_height))], world);
            }
            _ => {}
        }
    };

    run_main_loop(&mut bevy, &internal_queue, &commands, check);
}

/// Verify that `to_next_display` inserts the moved window into the target
/// display's strip instead of leaving it unmanaged ("Remaining").
#[test]
fn test_next_display_inserts_into_target_strip() {
    let mut bevy = setup_world();
    let mock_app = setup_process(bevy.world_mut());
    let internal_queue = Arc::new(RwLock::new(Vec::<Event>::new()));
    let event_queue = internal_queue.clone();

    let active_display = Arc::new(AtomicU32::new(EXT_DISPLAY_ID));

    // External display gets one window (id 100), internal display has none.
    let eq = event_queue.clone();
    let app = mock_app;
    let windows: TestWindowSpawner = Box::new(move |workspace_id| {
        if workspace_id == EXT_WORKSPACE_ID {
            let origin = Origin::new(0, 0);
            let size = Size::new(TEST_WINDOW_WIDTH, TEST_WINDOW_HEIGHT);
            vec![Window::new(Box::new(MockWindow::new(
                100,
                IRect::from_corners(origin, origin + size),
                eq.clone(),
                app.clone(),
            )))]
        } else {
            vec![]
        }
    });

    let window_manager = TwoDisplayMock {
        windows,
        active_display: active_display.clone(),
    };
    bevy.insert_resource(WindowManager(Box::new(window_manager)));

    let commands = vec![
        // 0: Settle.
        Event::MenuOpened { window_id: 100 },
        // 1: Print initial state.
        Event::Command {
            command: Command::PrintState,
        },
        // 2: Move focused window to the other display.
        Event::Command {
            command: Command::Window(Operation::ToNextDisplay(MoveFocus::Follow)),
        },
        // 3: Print final state for debugging.
        Event::Command {
            command: Command::PrintState,
        },
    ];

    let check = move |iteration, world: &mut World| {
        match iteration {
            1 => {
                // Window 100 should be on the external display's strip.
                let entity = find_window_entity(100, world);
                let mut strip_query = world.query::<&LayoutStrip>();
                let in_ext = strip_query
                    .iter(world)
                    .any(|strip| strip.id() == EXT_WORKSPACE_ID && strip.index_of(entity).is_ok());
                assert!(
                    in_ext,
                    "window 100 should be in the external strip before move"
                );
            }
            2 => {
                // After ToNextDisplay, window 100 must be in the target strip.
                let entity = find_window_entity(100, world);
                let mut strip_query = world.query::<&LayoutStrip>();
                let in_target = strip_query
                    .iter(world)
                    .any(|strip| strip.id() == TEST_WORKSPACE_ID && strip.index_of(entity).is_ok());
                let in_source = strip_query
                    .iter(world)
                    .any(|strip| strip.id() == EXT_WORKSPACE_ID && strip.index_of(entity).is_ok());
                assert!(
                    in_target,
                    "window 100 should be in the target (internal) strip after nextdisplay"
                );
                assert!(
                    !in_source,
                    "window 100 should NOT be in the source (external) strip after nextdisplay"
                );
            }
            _ => {}
        }
    };

    run_main_loop(&mut bevy, &internal_queue, &commands, check);
}

#[test]
fn test_send_next_display_stays_on_source() {
    let mut bevy = setup_world();
    let mock_app = setup_process(bevy.world_mut());
    let internal_queue = Arc::new(RwLock::new(Vec::<Event>::new()));
    let event_queue = internal_queue.clone();

    let active_display = Arc::new(AtomicU32::new(EXT_DISPLAY_ID));

    // External display gets two windows (ids 100 and 101) so the source strip
    // isn't empty after sending the focused window away.
    // Initialization focuses windows in order, so window 101 (listed second)
    // ends up as the focused window after init.
    let eq = event_queue.clone();
    let app = mock_app;
    let windows: TestWindowSpawner = Box::new(move |workspace_id| {
        if workspace_id == EXT_WORKSPACE_ID {
            let origin = Origin::new(0, 0);
            let size = Size::new(TEST_WINDOW_WIDTH, TEST_WINDOW_HEIGHT);
            vec![
                Window::new(Box::new(MockWindow::new(
                    100,
                    IRect::from_corners(origin, origin + size),
                    eq.clone(),
                    app.clone(),
                ))),
                Window::new(Box::new(MockWindow::new(
                    101,
                    IRect::from_corners(origin, origin + size),
                    eq.clone(),
                    app.clone(),
                ))),
            ]
        } else {
            vec![]
        }
    });

    let window_manager = TwoDisplayMock {
        windows,
        active_display: active_display.clone(),
    };
    bevy.insert_resource(WindowManager(Box::new(window_manager)));

    let commands = vec![
        // 0: Settle — window 101 is focused after init.
        Event::MenuOpened { window_id: 0 },
        // 1: Print initial state.
        Event::Command {
            command: Command::PrintState,
        },
        // 2: Send focused window (101) to the other display, but focus stays.
        Event::Command {
            command: Command::Window(Operation::ToNextDisplay(MoveFocus::Stay)),
        },
        // 3: Print final state for debugging.
        Event::Command {
            command: Command::PrintState,
        },
    ];

    let check = move |iteration, world: &mut World| {
        match iteration {
            1 => {
                // Window 101 should be on the external display's strip.
                let entity = find_window_entity(101, world);
                let mut strip_query = world.query::<&LayoutStrip>();
                let in_ext = strip_query
                    .iter(world)
                    .any(|strip| strip.id() == EXT_WORKSPACE_ID && strip.index_of(entity).is_ok());
                assert!(
                    in_ext,
                    "window 101 should be in the external strip before move"
                );
            }
            2 => {
                // After ToNextDisplay with MoveFocus::Stay, window 101 must be
                // in the target (internal) strip.
                let entity = find_window_entity(101, world);
                let mut strip_query = world.query::<&LayoutStrip>();
                let in_target = strip_query
                    .iter(world)
                    .any(|strip| strip.id() == TEST_WORKSPACE_ID && strip.index_of(entity).is_ok());
                let in_source = strip_query
                    .iter(world)
                    .any(|strip| strip.id() == EXT_WORKSPACE_ID && strip.index_of(entity).is_ok());
                assert!(
                    in_target,
                    "window 101 should be in the target (internal) strip after sendnextdisplay"
                );
                assert!(
                    !in_source,
                    "window 101 should NOT be in the source (external) strip after sendnextdisplay"
                );
                // Active display must remain the external display (focus stayed).
                assert_eq!(
                    active_display.load(Ordering::Relaxed),
                    EXT_DISPLAY_ID,
                    "active display should still be the external display after sendnextdisplay"
                );
            }
            _ => {}
        }
    };

    run_main_loop(&mut bevy, &internal_queue, &commands, check);
}

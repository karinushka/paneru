use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use bevy::prelude::*;
use bevy::time::TimeUpdateStrategy;

use crate::commands::{Command, MoveFocus, Operation};
use crate::ecs::Timeout;
use crate::ecs::layout::LayoutStrip;
use crate::events::Event;
use crate::manager::{Display, Origin, Size, Window};
use crate::{assert_not_on_workspace, assert_on_workspace, assert_window_size};

use super::*;

#[test]
fn test_multi_display_lifecycle() {
    let commands = vec![
        Event::MenuOpened { window_id: 0 },
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

    let mut harness = TestHarness::new().with_windows(1);
    harness
        .app
        .insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_millis(
            500,
        )));

    harness
        .on_iteration(1, |world| {
            let mut query = world.query_filtered::<Entity, With<Display>>();
            query.single(world).expect("should have one display");
        })
        .on_iteration(2, |world| {
            assert!(
                world
                    .query_filtered::<Entity, With<Display>>()
                    .single(world)
                    .is_err(),
                "display should be despawned"
            );

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
        })
        .on_iteration(3, |world| {
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
            let child_of: &ChildOf = workspace
                .get::<ChildOf>()
                .expect("re-parented workspace should have a parent");
            assert_eq!(
                child_of.parent(),
                new_display_entity,
                "workspace should be child of the new display"
            );
        })
        .run(commands);
}

#[test]
fn test_multi_workspace_orphaning() {
    let commands = vec![
        Event::MenuOpened { window_id: 0 },
        Event::Command {
            command: Command::PrintState,
        },
        Event::DisplayRemoved {
            display_id: TEST_DISPLAY_ID,
        },
    ];

    let mock_app = setup_process(setup_world().world_mut());
    let internal_queue = Arc::new(RwLock::new(Vec::new()));
    let spawner = window_spawner(1, internal_queue.clone(), mock_app);
    let wm = MockWindowManager {
        windows: spawner,
        workspaces: vec![TEST_WORKSPACE_ID, TEST_WORKSPACE_ID + 1],
    };

    TestHarness::new()
        .with_wm(wm)
        .on_iteration(1, |world| {
            let display_entity = world
                .query_filtered::<Entity, With<Display>>()
                .single(world)
                .expect("should have one display");

            let workspace_entities = world
                .query_filtered::<Entity, With<LayoutStrip>>()
                .iter(world)
                .collect::<Vec<_>>();
            assert_eq!(workspace_entities.len(), 2, "should have two workspaces");

            for &ws in &workspace_entities {
                let child_of: &ChildOf = world
                    .entity(ws)
                    .get::<ChildOf>()
                    .expect("workspace should have parent");
                assert_eq!(child_of.parent(), display_entity);
            }
        })
        .on_iteration(2, |world| {
            let workspace_entities = world
                .query_filtered::<Entity, With<LayoutStrip>>()
                .iter(world)
                .collect::<Vec<_>>();
            for &ws in &workspace_entities {
                let entity: EntityRef = world.entity(ws);
                assert!(
                    entity.get::<Timeout>().is_some(),
                    "each workspace should have a timeout"
                );
                assert!(
                    entity.get::<ChildOf>().is_none(),
                    "each workspace should have no parent"
                );
            }
        })
        .run(commands);
}

#[test]
fn test_multi_display_no_height_crosstalk() {
    let active_display = Arc::new(AtomicU32::new(EXT_DISPLAY_ID));
    let ad_clone = active_display.clone();

    let mut harness = TestHarness::new();
    let mock_app = setup_process(harness.app.world_mut());
    let internal_queue = harness.internal_queue.clone();

    let eq1 = internal_queue.clone();
    let eq2 = internal_queue.clone();
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

    let wm = TwoDisplayMock {
        windows,
        active_display: active_display.clone(),
    };
    harness = harness.with_wm(wm);

    let ext_usable_height = EXT_DISPLAY_HEIGHT - TEST_MENUBAR_HEIGHT;

    let commands = vec![
        Event::MenuOpened { window_id: 100 },
        Event::Command {
            command: Command::PrintState,
        },
        Event::DisplayChanged,
        Event::MenuOpened { window_id: 100 },
        Event::Command {
            command: Command::PrintState,
        },
    ];

    harness
        .on_iteration(1, move |world| {
            assert_window_size!(world, 100, TEST_WINDOW_WIDTH, ext_usable_height);
            ad_clone.store(TEST_DISPLAY_ID, Ordering::Relaxed);
        })
        .on_iteration(2, |world| {
            use crate::ecs::ActiveWorkspaceMarker;
            let mut strip_query =
                world.query_filtered::<&mut LayoutStrip, Without<ActiveWorkspaceMarker>>();
            for mut strip in strip_query.iter_mut(world) {
                strip.set_changed();
            }
        })
        .on_iteration(4, move |world| {
            assert_window_size!(world, 100, TEST_WINDOW_WIDTH, ext_usable_height);
        })
        .run(commands);
}

#[test]
fn test_next_display_inserts_into_target_strip() {
    let active_display = Arc::new(AtomicU32::new(EXT_DISPLAY_ID));

    let mut harness = TestHarness::new();
    let mock_app = setup_process(harness.app.world_mut());
    let internal_queue = harness.internal_queue.clone();

    let eq = internal_queue.clone();
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

    let wm = TwoDisplayMock {
        windows,
        active_display: active_display.clone(),
    };
    harness = harness.with_wm(wm);

    let commands = vec![
        Event::MenuOpened { window_id: 100 },
        Event::Command {
            command: Command::PrintState,
        },
        Event::Command {
            command: Command::Window(Operation::ToNextDisplay(MoveFocus::Follow)),
        },
        Event::Command {
            command: Command::PrintState,
        },
    ];

    harness
        .on_iteration(1, move |world| {
            assert_on_workspace!(world, 100, EXT_WORKSPACE_ID);
        })
        .on_iteration(2, move |world| {
            assert_on_workspace!(world, 100, TEST_WORKSPACE_ID);
            assert_not_on_workspace!(world, 100, EXT_WORKSPACE_ID);
        })
        .run(commands);
}

#[test]
fn test_send_next_display_stays_on_source() {
    let active_display = Arc::new(AtomicU32::new(EXT_DISPLAY_ID));
    let ad_clone = active_display.clone();

    let mut harness = TestHarness::new();
    let mock_app = setup_process(harness.app.world_mut());
    let internal_queue = harness.internal_queue.clone();

    let eq = internal_queue.clone();
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

    let wm = TwoDisplayMock {
        windows,
        active_display: active_display.clone(),
    };
    harness = harness.with_wm(wm);

    let commands = vec![
        Event::MenuOpened { window_id: 101 },
        Event::Command {
            command: Command::PrintState,
        },
        Event::Command {
            command: Command::Window(Operation::ToNextDisplay(MoveFocus::Stay)),
        },
        Event::Command {
            command: Command::PrintState,
        },
    ];

    harness
        .on_iteration(1, move |world| {
            assert_on_workspace!(world, 101, EXT_WORKSPACE_ID);
        })
        .on_iteration(2, move |world| {
            assert_on_workspace!(world, 101, TEST_WORKSPACE_ID);
            assert_not_on_workspace!(world, 101, EXT_WORKSPACE_ID);
            assert_eq!(
                ad_clone.load(Ordering::Relaxed),
                EXT_DISPLAY_ID,
                "active display should still be the external display after sendnextdisplay"
            );
        })
        .run(commands);
}

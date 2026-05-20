use bevy::ecs::query::Has;
use bevy::prelude::*;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};

use crate::config::{Config, MainOptions, WindowParams};
use crate::ecs::layout::{Column, LayoutStrip};
use crate::ecs::state::{
    PaneruState, SavedColumn, SavedDisplay, SavedRect, SavedStrip, SavedWindow, SavedWorkspace,
};
use crate::ecs::workspace::PreviousStripPosition;
use crate::ecs::{SpawnWindowTrigger, Unmanaged};
use crate::manager::{Display, Origin, Size, Window};
use crate::platform::{ProcessSerialNumber, WorkspaceId};
use crate::tests::{
    EXT_DISPLAY_ID, EXT_WORKSPACE_ID, MockWindow, MockWindowManager, TEST_DISPLAY_HEIGHT,
    TEST_DISPLAY_ID, TEST_DISPLAY_WIDTH, TEST_MENUBAR_HEIGHT, TEST_PROCESS_ID, TEST_WINDOW_HEIGHT,
    TEST_WINDOW_WIDTH, TEST_WORKSPACE_ID, TestHarness, TestWindowSpawner, TwoDisplayMock,
    setup_process,
};

#[test]
fn test_startup_restore_rebuilds_virtual_workspace_layout() {
    let mut harness = TestHarness::new().with_windows(2);

    harness.app.world_mut().insert_resource(PaneruState {
        version: 2,
        timestamp: 123_456_789,
        active_display_id: Some(TEST_DISPLAY_ID),
        displays: vec![SavedDisplay {
            display_id: TEST_DISPLAY_ID,
            bounds: SavedRect {
                min_x: 0,
                min_y: TEST_MENUBAR_HEIGHT,
                max_x: TEST_DISPLAY_WIDTH,
                max_y: TEST_DISPLAY_HEIGHT,
            },
            active: true,
            workspace_ids: vec![TEST_WORKSPACE_ID],
        }],
        workspaces: vec![SavedWorkspace {
            workspace_id: TEST_WORKSPACE_ID,
            display_id: Some(TEST_DISPLAY_ID),
            active_virtual_index: Some(1),
            strips: vec![SavedStrip {
                virtual_index: 1,
                columns: vec![
                    SavedColumn::Single(saved_window(0)),
                    SavedColumn::Single(saved_window(1)),
                ],
            }],
        }],
    });

    for _ in 0..5 {
        harness.app.update();
    }

    let world = harness.app.world_mut();
    let mut query = world.query::<(&LayoutStrip, Has<crate::ecs::ActiveWorkspaceMarker>)>();
    let active_strips = query
        .iter(world)
        .filter(|(strip, active)| strip.id() == TEST_WORKSPACE_ID && *active)
        .map(|(strip, _)| strip)
        .collect::<Vec<_>>();

    assert_eq!(
        active_strips.len(),
        1,
        "exactly one virtual row should be active for the restored workspace"
    );

    let restored = active_strips[0];
    assert_eq!(restored.virtual_index, 1);

    let columns = restored.columns().collect::<Vec<_>>();
    assert_eq!(columns.len(), 2);
    assert!(matches!(columns[0], Column::Single(_)));
    assert!(matches!(columns[1], Column::Single(_)));
}

#[test]
fn test_startup_restore_hard_matches_without_extra_fallback_metadata_reads() {
    let baseline_reads = startup_restore_metadata_reads(None);
    let restore_reads = startup_restore_metadata_reads(Some(state_with_strips(
        (0_u32..20)
            .map(|id| SavedStrip {
                virtual_index: id,
                columns: vec![SavedColumn::Single(saved_window(
                    i32::try_from(id).expect("test id should fit in i32"),
                ))],
            })
            .collect(),
    )));

    assert!(
        restore_reads <= baseline_reads,
        "hard-match restore should not add fallback window metadata reads"
    );
}

fn startup_restore_metadata_reads(state: Option<PaneruState>) -> usize {
    let metadata_reads = Arc::new(AtomicUsize::new(0));
    let mut harness = TestHarness::new();
    let mock_app = setup_process(harness.app.world_mut());
    let internal_queue = harness.internal_queue.clone();
    let reads = metadata_reads.clone();
    let windows: TestWindowSpawner = Box::new(move |_| {
        (0..20)
            .map(|id| {
                let origin = Origin::new(0, 0);
                let size = Size::new(TEST_WINDOW_WIDTH, TEST_WINDOW_HEIGHT);
                let mut window = MockWindow::new(
                    id,
                    IRect::from_corners(origin, origin + size),
                    internal_queue.clone(),
                    mock_app.clone(),
                );
                window.metadata_reads = Some(reads.clone());
                Window::new(Box::new(window))
            })
            .collect()
    });
    harness = harness.with_wm(MockWindowManager {
        windows,
        workspaces: vec![TEST_WORKSPACE_ID],
        visible_windows: HashMap::new(),
    });
    if let Some(state) = state {
        harness.app.world_mut().insert_resource(state);
    }

    for _ in 0..5 {
        harness.app.update();
    }

    metadata_reads.load(Ordering::Relaxed)
}

#[test]
fn test_startup_restore_replaces_consumed_empty_virtual_zero_strip() {
    let mut harness = TestHarness::new().with_windows(2);

    harness.app.world_mut().insert_resource(PaneruState {
        version: 2,
        timestamp: 123_456_789,
        active_display_id: Some(TEST_DISPLAY_ID),
        displays: vec![saved_display(TEST_DISPLAY_ID, true)],
        workspaces: vec![SavedWorkspace {
            workspace_id: TEST_WORKSPACE_ID,
            display_id: Some(TEST_DISPLAY_ID),
            active_virtual_index: Some(0),
            strips: vec![SavedStrip {
                virtual_index: 0,
                columns: vec![
                    SavedColumn::Single(saved_window(0)),
                    SavedColumn::Single(saved_window(1)),
                ],
            }],
        }],
    });

    for _ in 0..5 {
        harness.app.update();
    }

    let world = harness.app.world_mut();
    let mut query = world.query::<&LayoutStrip>();
    let matching = query
        .iter(world)
        .filter(|strip| strip.id() == TEST_WORKSPACE_ID && strip.virtual_index == 0)
        .collect::<Vec<_>>();

    assert_eq!(
        matching.len(),
        1,
        "restore should replace the emptied startup row 0 instead of leaving a duplicate"
    );

    let windows = matching[0].all_windows();
    assert_eq!(windows.len(), 2);
    assert!(
        windows.contains(&crate::tests::harness::find_window_entity(0, world)),
        "restored row should contain window 0"
    );
    assert!(
        windows.contains(&crate::tests::harness::find_window_entity(1, world)),
        "restored row should contain window 1"
    );
}

#[test]
fn test_startup_restore_preserves_fullscreen_strip() {
    let mut harness = TestHarness::new().with_windows(1);

    harness.app.world_mut().insert_resource(PaneruState {
        version: 2,
        timestamp: 123_456_789,
        active_display_id: Some(TEST_DISPLAY_ID),
        displays: vec![saved_display(TEST_DISPLAY_ID, true)],
        workspaces: vec![SavedWorkspace {
            workspace_id: TEST_WORKSPACE_ID,
            display_id: Some(TEST_DISPLAY_ID),
            active_virtual_index: Some(0),
            strips: vec![SavedStrip {
                virtual_index: 0,
                columns: vec![SavedColumn::Fullscreen(saved_window(0))],
            }],
        }],
    });

    for _ in 0..5 {
        harness.app.update();
    }

    let world = harness.app.world_mut();
    let mut query = world.query::<&LayoutStrip>();
    let matching = query
        .iter(world)
        .filter(|strip| strip.id() == TEST_WORKSPACE_ID && strip.virtual_index == 0)
        .collect::<Vec<_>>();

    assert_eq!(
        matching.len(),
        1,
        "restore should replace the emptied startup row 0 instead of leaving a duplicate"
    );
    assert!(
        matching[0].is_fullscreen(),
        "fullscreen restore should rebuild a fullscreen strip"
    );
}

#[test]
fn test_startup_restore_prefers_existing_workspace_parent_before_saved_or_active_display() {
    let active_display = Arc::new(AtomicU32::new(EXT_DISPLAY_ID));
    let mut harness = TestHarness::new();
    let mock_app = setup_process(harness.app.world_mut());
    let internal_queue = harness.internal_queue.clone();
    let windows: TestWindowSpawner = Box::new(move |workspace_id: WorkspaceId| {
        if workspace_id == TEST_WORKSPACE_ID {
            let origin = Origin::new(0, 0);
            let size = Size::new(TEST_WINDOW_WIDTH, TEST_WINDOW_HEIGHT);
            vec![Window::new(Box::new(crate::tests::MockWindow::new(
                200,
                IRect::from_corners(origin, origin + size),
                internal_queue.clone(),
                mock_app.clone(),
            )))]
        } else {
            vec![]
        }
    });

    harness = harness.with_wm(TwoDisplayMock {
        windows,
        active_display,
    });

    harness.app.world_mut().insert_resource(PaneruState {
        version: 2,
        timestamp: 123_456_789,
        active_display_id: Some(EXT_DISPLAY_ID),
        displays: vec![
            saved_display(EXT_DISPLAY_ID, true),
            saved_display(TEST_DISPLAY_ID, false),
        ],
        workspaces: vec![SavedWorkspace {
            workspace_id: TEST_WORKSPACE_ID,
            display_id: Some(EXT_DISPLAY_ID),
            active_virtual_index: Some(0),
            strips: vec![SavedStrip {
                virtual_index: 0,
                columns: vec![SavedColumn::Single(saved_window(200))],
            }],
        }],
    });

    for _ in 0..5 {
        harness.app.update();
    }

    let world = harness.app.world_mut();
    let restored_window = crate::tests::harness::find_window_entity(200, world);
    let restored_entity = {
        let mut query = world.query::<(Entity, &LayoutStrip)>();
        query
            .iter(world)
            .find(|(_, strip)| {
                strip.id() == TEST_WORKSPACE_ID
                    && strip.virtual_index == 0
                    && strip.contains(restored_window)
            })
            .map(|(entity, _)| entity)
            .expect("restored strip should exist")
    };
    let parent = world
        .entity(restored_entity)
        .get::<ChildOf>()
        .expect("restored strip should have a display parent")
        .parent();
    let display = world
        .entity(parent)
        .get::<Display>()
        .expect("parent should be a display");

    assert_eq!(
        display.id(),
        TEST_DISPLAY_ID,
        "stale saved display id should fall back to the display that already owns the native workspace"
    );
}

#[test]
fn test_startup_restore_preserves_saved_display_when_present() {
    let active_display = Arc::new(AtomicU32::new(TEST_DISPLAY_ID));
    let mut harness = TestHarness::new();
    let mock_app = setup_process(harness.app.world_mut());
    let internal_queue = harness.internal_queue.clone();
    let windows: TestWindowSpawner = Box::new(move |workspace_id: WorkspaceId| {
        if workspace_id == EXT_WORKSPACE_ID {
            let origin = Origin::new(0, -TEST_WINDOW_HEIGHT);
            let size = Size::new(TEST_WINDOW_WIDTH, TEST_WINDOW_HEIGHT);
            vec![Window::new(Box::new(MockWindow::new(
                300,
                IRect::from_corners(origin, origin + size),
                internal_queue.clone(),
                mock_app.clone(),
            )))]
        } else {
            vec![]
        }
    });

    harness = harness.with_wm(TwoDisplayMock {
        windows,
        active_display,
    });

    harness.app.world_mut().insert_resource(PaneruState {
        version: 2,
        timestamp: 123_456_789,
        active_display_id: Some(EXT_DISPLAY_ID),
        displays: vec![
            saved_display(EXT_DISPLAY_ID, true),
            saved_display(TEST_DISPLAY_ID, false),
        ],
        workspaces: vec![SavedWorkspace {
            workspace_id: EXT_WORKSPACE_ID,
            display_id: Some(EXT_DISPLAY_ID),
            active_virtual_index: Some(0),
            strips: vec![SavedStrip {
                virtual_index: 0,
                columns: vec![SavedColumn::Single(saved_window(300))],
            }],
        }],
    });

    for _ in 0..5 {
        harness.app.update();
    }

    let world = harness.app.world_mut();
    let restored_window = crate::tests::harness::find_window_entity(300, world);
    let parent = restored_strip_display_parent(world, EXT_WORKSPACE_ID, 0, restored_window);
    let display = world
        .entity(parent)
        .get::<Display>()
        .expect("parent should be a display");

    assert_eq!(
        display.id(),
        EXT_DISPLAY_ID,
        "restore should keep the exact saved display when it is present"
    );
}

#[test]
fn test_startup_restore_keeps_current_native_workspace_active_across_multiple_workspaces() {
    let active_display = Arc::new(AtomicU32::new(TEST_DISPLAY_ID));
    let mut harness = TestHarness::new();
    let mock_app = setup_process(harness.app.world_mut());
    let internal_queue = harness.internal_queue.clone();
    let windows: TestWindowSpawner = Box::new(move |workspace_id: WorkspaceId| {
        let (window_id, origin) = match workspace_id {
            TEST_WORKSPACE_ID => (100, Origin::new(0, 0)),
            EXT_WORKSPACE_ID => (300, Origin::new(0, -TEST_WINDOW_HEIGHT)),
            _ => return vec![],
        };
        let size = Size::new(TEST_WINDOW_WIDTH, TEST_WINDOW_HEIGHT);
        vec![Window::new(Box::new(MockWindow::new(
            window_id,
            IRect::from_corners(origin, origin + size),
            internal_queue.clone(),
            mock_app.clone(),
        )))]
    });

    harness = harness.with_wm(TwoDisplayMock {
        windows,
        active_display,
    });

    harness.app.world_mut().insert_resource(PaneruState {
        version: 2,
        timestamp: 123_456_789,
        active_display_id: Some(TEST_DISPLAY_ID),
        displays: vec![
            saved_display(TEST_DISPLAY_ID, true),
            saved_display(EXT_DISPLAY_ID, false),
        ],
        workspaces: vec![
            SavedWorkspace {
                workspace_id: TEST_WORKSPACE_ID,
                display_id: Some(TEST_DISPLAY_ID),
                active_virtual_index: Some(0),
                strips: vec![SavedStrip {
                    virtual_index: 0,
                    columns: vec![SavedColumn::Single(saved_window(100))],
                }],
            },
            SavedWorkspace {
                workspace_id: EXT_WORKSPACE_ID,
                display_id: Some(EXT_DISPLAY_ID),
                active_virtual_index: Some(0),
                strips: vec![SavedStrip {
                    virtual_index: 0,
                    columns: vec![SavedColumn::Single(saved_window(300))],
                }],
            },
        ],
    });

    for _ in 0..5 {
        harness.app.update();
    }

    let world = harness.app.world_mut();
    let mut query = world.query::<(
        &LayoutStrip,
        Has<crate::ecs::ActiveWorkspaceMarker>,
        Has<crate::ecs::SelectedVirtualMarker>,
    )>();
    let restored = query
        .iter(world)
        .filter(|(strip, _, _)| {
            (strip.id() == TEST_WORKSPACE_ID || strip.id() == EXT_WORKSPACE_ID)
                && strip.virtual_index == 0
                && !strip.all_windows().is_empty()
        })
        .collect::<Vec<_>>();

    assert_eq!(
        restored.iter().filter(|(_, active, _)| *active).count(),
        1,
        "restore should keep one global active native workspace"
    );
    assert!(
        restored
            .iter()
            .any(|(strip, active, selected)| strip.id() == TEST_WORKSPACE_ID
                && *active
                && *selected)
    );
    assert!(
        restored
            .iter()
            .any(|(strip, active, selected)| strip.id() == EXT_WORKSPACE_ID
                && !*active
                && *selected)
    );
}

#[test]
fn test_restore_resource_is_removed_after_grace_period() {
    let mut harness = TestHarness::new().with_windows(1);
    harness
        .app
        .world_mut()
        .insert_resource(state_with_strips(vec![SavedStrip {
            virtual_index: 0,
            columns: vec![SavedColumn::Single(saved_window(0))],
        }]));

    for _ in 0..5 {
        harness.app.update();
    }

    assert!(
        harness
            .app
            .world()
            .contains_resource::<crate::ecs::restore::SessionRestore>()
    );

    for _ in 0..30 {
        harness.app.update();
    }

    assert!(
        !harness
            .app
            .world()
            .contains_resource::<crate::ecs::restore::SessionRestore>()
    );
    assert!(!harness.app.world().contains_resource::<PaneruState>());
}

#[test]
fn test_startup_restore_disabled_by_config_keeps_normal_layout() {
    let config = Config::try_from(
        r"
[options]

[restore]
enabled = false

[bindings]
",
    )
    .expect("config should parse");
    let mut harness = TestHarness::new().with_config(config).with_windows(1);
    harness
        .app
        .world_mut()
        .insert_resource(state_with_strips(vec![SavedStrip {
            virtual_index: 1,
            columns: vec![SavedColumn::Single(saved_window(0))],
        }]));

    for _ in 0..5 {
        harness.app.update();
    }

    assert!(
        !harness
            .app
            .world()
            .contains_resource::<crate::ecs::restore::SessionRestore>()
    );
    assert!(!harness.app.world().contains_resource::<PaneruState>());

    let world = harness.app.world_mut();
    let mut query = world.query::<&LayoutStrip>();
    assert!(
        query
            .iter(world)
            .any(|strip| strip.id() == TEST_WORKSPACE_ID && strip.virtual_index == 0)
    );
    assert!(
        !query
            .iter(world)
            .any(|strip| strip.id() == TEST_WORKSPACE_ID && strip.virtual_index == 1)
    );
}

#[test]
fn test_startup_restore_uses_first_restored_row_when_active_metadata_is_missing() {
    let mut harness = TestHarness::new().with_windows(1);
    let mut state = state_with_strips(vec![SavedStrip {
        virtual_index: 2,
        columns: vec![SavedColumn::Single(saved_window(0))],
    }]);
    state.workspaces[0].active_virtual_index = None;
    harness.app.world_mut().insert_resource(state);

    for _ in 0..5 {
        harness.app.update();
    }

    let world = harness.app.world_mut();
    let mut query = world.query::<(&LayoutStrip, Has<crate::ecs::ActiveWorkspaceMarker>)>();
    let active_strips = query
        .iter(world)
        .filter(|(strip, active)| strip.id() == TEST_WORKSPACE_ID && *active)
        .map(|(strip, _)| strip)
        .collect::<Vec<_>>();

    assert_eq!(active_strips.len(), 1);
    assert_eq!(active_strips[0].virtual_index, 2);
}

#[test]
fn test_startup_restore_overrides_floating_config_for_matched_window() {
    let mut params = WindowParams::new(".*", Some("test".to_string()));
    params.floating = Some(true);
    let config: Config = (MainOptions::default(), vec![params]).into();
    let mut harness = TestHarness::new().with_config(config).with_windows(1);
    harness
        .app
        .world_mut()
        .insert_resource(state_with_strips(vec![SavedStrip {
            virtual_index: 0,
            columns: vec![SavedColumn::Single(saved_window(0))],
        }]));

    for _ in 0..5 {
        harness.app.update();
    }

    let world = harness.app.world_mut();
    let restored_window = crate::tests::harness::find_window_entity(0, world);
    assert!(
        world.entity(restored_window).get::<Unmanaged>().is_none(),
        "matched restore windows should not inherit floating config"
    );
}

#[test]
fn test_late_startup_window_restores_during_grace_period() {
    let mut harness = TestHarness::new();
    let mock_app = setup_process(harness.app.world_mut());
    let windows: TestWindowSpawner = Box::new(|_| vec![]);
    harness = harness.with_wm(MockWindowManager {
        windows,
        workspaces: vec![TEST_WORKSPACE_ID],
        visible_windows: HashMap::new(),
    });
    harness
        .app
        .world_mut()
        .insert_resource(state_with_strips(vec![SavedStrip {
            virtual_index: 1,
            columns: vec![SavedColumn::Single(saved_window(99))],
        }]));

    for _ in 0..5 {
        harness.app.update();
    }
    assert!(
        harness
            .app
            .world()
            .contains_resource::<crate::ecs::restore::SessionRestore>()
    );

    let origin = Origin::new(0, 0);
    let size = Size::new(TEST_WINDOW_WIDTH, TEST_WINDOW_HEIGHT);
    let window = Window::new(Box::new(MockWindow::new(
        99,
        IRect::from_corners(origin, origin + size),
        harness.internal_queue.clone(),
        mock_app,
    )));
    harness
        .app
        .world_mut()
        .trigger(SpawnWindowTrigger(vec![window]));

    for _ in 0..3 {
        harness.app.update();
    }

    let world = harness.app.world_mut();
    let restored_window = crate::tests::harness::find_window_entity(99, world);
    let mut query = world.query::<(&LayoutStrip, Has<crate::ecs::ActiveWorkspaceMarker>)>();
    let restored = query
        .iter(world)
        .find(|(strip, active)| {
            strip.id() == TEST_WORKSPACE_ID
                && strip.virtual_index == 1
                && *active
                && strip.contains(restored_window)
        })
        .map(|(strip, _)| strip);

    assert!(
        restored.is_some(),
        "late startup window should restore into saved row"
    );
}

#[test]
fn test_startup_restore_keeps_one_selected_row_and_hides_inactive_rows() {
    let mut harness = TestHarness::new().with_windows(2);
    harness.app.world_mut().insert_resource(PaneruState {
        version: 2,
        timestamp: 123_456_789,
        active_display_id: Some(TEST_DISPLAY_ID),
        displays: vec![saved_display(TEST_DISPLAY_ID, true)],
        workspaces: vec![SavedWorkspace {
            workspace_id: TEST_WORKSPACE_ID,
            display_id: Some(TEST_DISPLAY_ID),
            active_virtual_index: Some(1),
            strips: vec![
                SavedStrip {
                    virtual_index: 0,
                    columns: vec![SavedColumn::Single(saved_window(0))],
                },
                SavedStrip {
                    virtual_index: 1,
                    columns: vec![SavedColumn::Single(saved_window(1))],
                },
            ],
        }],
    });

    for _ in 0..5 {
        harness.app.update();
    }

    let world = harness.app.world_mut();
    let mut query = world.query::<(
        &LayoutStrip,
        Has<crate::ecs::ActiveWorkspaceMarker>,
        Has<crate::ecs::SelectedVirtualMarker>,
        Option<&PreviousStripPosition>,
    )>();
    let restored = query
        .iter(world)
        .filter(|(strip, _, _, _)| strip.id() == TEST_WORKSPACE_ID)
        .collect::<Vec<_>>();
    let active = restored
        .iter()
        .filter(|(strip, active, _, _)| *active && strip.virtual_index == 1)
        .count();
    let selected = restored
        .iter()
        .filter(|(_, _, selected, _)| *selected)
        .count();
    let inactive = restored.iter().find(|(strip, active, _, previous)| {
        strip.virtual_index == 0 && !*active && previous.is_some()
    });

    assert_eq!(active, 1);
    assert_eq!(selected, 1);
    assert!(
        inactive.is_some(),
        "inactive restored rows should be hidden with previous position"
    );
}

fn saved_display(display_id: u32, active: bool) -> SavedDisplay {
    SavedDisplay {
        display_id,
        bounds: SavedRect {
            min_x: 0,
            min_y: TEST_MENUBAR_HEIGHT,
            max_x: TEST_DISPLAY_WIDTH,
            max_y: TEST_DISPLAY_HEIGHT,
        },
        active,
        workspace_ids: vec![TEST_WORKSPACE_ID],
    }
}

fn state_with_strips(strips: Vec<SavedStrip>) -> PaneruState {
    PaneruState {
        version: 2,
        timestamp: 123_456_789,
        active_display_id: Some(TEST_DISPLAY_ID),
        displays: vec![saved_display(TEST_DISPLAY_ID, true)],
        workspaces: vec![SavedWorkspace {
            workspace_id: TEST_WORKSPACE_ID,
            display_id: Some(TEST_DISPLAY_ID),
            active_virtual_index: Some(0),
            strips,
        }],
    }
}

fn restored_strip_display_parent(
    world: &mut World,
    workspace_id: WorkspaceId,
    virtual_index: u32,
    window: Entity,
) -> Entity {
    let mut query = world.query::<(Entity, &LayoutStrip)>();
    let restored_entity = query
        .iter(world)
        .find(|(_, strip)| {
            strip.id() == workspace_id
                && strip.virtual_index == virtual_index
                && strip.contains(window)
        })
        .map(|(entity, _)| entity)
        .expect("restored strip should exist");
    world
        .entity(restored_entity)
        .get::<ChildOf>()
        .expect("restored strip should have a display parent")
        .parent()
}

fn saved_window(window_id: i32) -> SavedWindow {
    SavedWindow {
        window_id,
        pid: TEST_PROCESS_ID,
        psn: ProcessSerialNumber { high: 1, low: 2 },
        bundle_id: "test".to_string(),
        title: String::new(),
        identifier: String::new(),
        role: "AXWindow".to_string(),
        subrole: "AXStandardWindow".to_string(),
    }
}

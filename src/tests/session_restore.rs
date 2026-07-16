use bevy::ecs::query::Has;
use bevy::prelude::*;

use crate::commands::Command;
use crate::config::{Config, MainOptions, WindowParams};
use crate::ecs::layout::{Column, LayoutStrip};
use crate::ecs::state::{
    PaneruState, SavedColumn, SavedDisplay, SavedRect, SavedStrip, SavedWindow, SavedWorkspace,
};
use crate::ecs::workspace::PreviousStripPosition;
use crate::ecs::{
    ApplicationObserved, Bounds, Position, SpawnWindowTrigger, Unmanaged, WidthRatio,
    WindowDisposition,
};
use crate::events::Event;
use crate::manager::{Display, Origin, Size};
use crate::platform::{ProcessSerialNumber, WorkspaceId};
use crate::tests::{
    EXT_DISPLAY_HEIGHT, EXT_DISPLAY_ID, EXT_DISPLAY_WIDTH, EXT_WORKSPACE_ID, TEST_DISPLAY_HEIGHT,
    TEST_DISPLAY_ID, TEST_DISPLAY_WIDTH, TEST_MENUBAR_HEIGHT, TEST_PROCESS_ID, TEST_WINDOW_HEIGHT,
    TEST_WINDOW_WIDTH, TEST_WORKSPACE_ID, TestHarness, find_window_entity,
};

#[test]
fn test_startup_restore_rebuilds_virtual_workspace_layout() {
    let state = PaneruState {
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
    };

    let mut harness = TestHarness::new().with_windows(2).with_state(state);

    for _ in 0..5 {
        harness.app.update();
    }

    let world = harness.world();
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
    assert_eq!(
        world
            .query_filtered::<Entity, With<ApplicationObserved>>()
            .iter(world)
            .count(),
        1,
        "a background owner matched by startup restore must remain broadly observed"
    );
}

#[test]
fn test_startup_restore_reapplies_non_default_width_and_strip_pan() {
    let mut window = saved_window(0);
    window.width_ratio = Some(1.5);
    window.strip_position_x = Some(-321);
    let state = PaneruState {
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
                columns: vec![SavedColumn::Single(window)],
            }],
        }],
    };
    let mut harness = TestHarness::new().with_windows(1).with_state(state);
    for _ in 0..10 {
        harness.app.update();
    }

    let world = harness.world();
    let window_entity = find_window_entity(0, world);
    let (bounds, ratio) = world
        .query::<(&Bounds, &WidthRatio)>()
        .get(world, window_entity)
        .expect("restored window geometry should exist");
    assert_eq!(bounds.0.x, (f64::from(TEST_DISPLAY_WIDTH) * 1.5) as i32);
    assert!((ratio.0 - 1.5).abs() < f64::EPSILON);

    let strip_x = world
        .query::<(&LayoutStrip, &Position)>()
        .iter(world)
        .find(|(strip, _)| strip.contains(window_entity))
        .map(|(_, position)| position.0.x)
        .expect("restored strip should contain the window");
    assert_eq!(strip_x, -321);
}

#[test]
fn test_startup_restore_replaces_consumed_empty_virtual_zero_strip() {
    let mut harness = TestHarness::new().with_windows(2);

    harness.world().insert_resource(PaneruState {
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

    let world = harness.world();
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

    harness.world().insert_resource(PaneruState {
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

    let world = harness.world();
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
    let mut harness = TestHarness::new();

    let origin = Origin::new(0, 0);
    let size = Size::new(TEST_WINDOW_WIDTH, TEST_WINDOW_HEIGHT);
    harness.mock_state.spawn_window(
        TEST_PROCESS_ID,
        TEST_WORKSPACE_ID,
        200,
        IRect::from_corners(origin, origin + size),
    );
    harness.world().insert_resource(PaneruState {
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

    let world = harness.world();
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
    let mut harness = TestHarness::new();
    harness.mock_state.add_display(
        EXT_DISPLAY_ID,
        IRect::new(0, -EXT_DISPLAY_HEIGHT, EXT_DISPLAY_WIDTH, 0),
        vec![EXT_WORKSPACE_ID],
    );

    let size = Size::new(TEST_WINDOW_WIDTH, TEST_WINDOW_HEIGHT);
    let origin = Origin::new(0, -TEST_WINDOW_WIDTH);
    harness.mock_state.spawn_window(
        TEST_PROCESS_ID,
        EXT_WORKSPACE_ID,
        300,
        IRect::from_corners(origin, origin + size),
    );

    harness.world().insert_resource(PaneruState {
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

    let commands = vec![
        Event::MenuOpened { window_id: 100 },
        Event::Command {
            command: Command::PrintState,
        },
    ];

    harness
        .on_iteration(1, |world, _state| {
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
        })
        .run(commands);
}

#[test]
fn test_startup_restore_keeps_current_native_workspace_active_across_multiple_workspaces() {
    let mut harness = TestHarness::new();
    harness.mock_state.add_display(
        EXT_DISPLAY_ID,
        IRect::new(0, -EXT_DISPLAY_HEIGHT, EXT_DISPLAY_WIDTH, 0),
        vec![EXT_WORKSPACE_ID],
    );

    let size = Size::new(TEST_WINDOW_WIDTH, TEST_WINDOW_HEIGHT);
    let origin = Origin::new(0, 0);
    let ext_origin = Origin::new(0, -TEST_WINDOW_HEIGHT);
    harness.mock_state.spawn_window(
        TEST_PROCESS_ID,
        TEST_WORKSPACE_ID,
        100,
        IRect::from_corners(origin, origin + size),
    );
    harness.mock_state.spawn_window(
        TEST_PROCESS_ID,
        EXT_WORKSPACE_ID,
        300,
        IRect::from_corners(ext_origin, ext_origin + size),
    );

    harness.world().insert_resource(PaneruState {
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

    let commands = vec![
        Event::MenuOpened { window_id: 100 },
        Event::Command {
            command: Command::PrintState,
        },
    ];

    harness
        .on_iteration(1, |world, _state| {
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
        })
        .run(commands);
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

    harness
        .app
        .world_mut()
        .resource_mut::<crate::ecs::restore::SessionRestore>()
        .expire_now();
    harness.app.update();

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

    let world = harness.world();
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
    harness.world().insert_resource(state);

    for _ in 0..5 {
        harness.app.update();
    }

    let world = harness.world();
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
fn test_startup_restore_respects_floating_config_for_matched_window() {
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

    let world = harness.world();
    let restored_window = crate::tests::harness::find_window_entity(0, world);
    assert_eq!(
        world
            .entity(restored_window)
            .get::<WindowDisposition>()
            .copied(),
        Some(WindowDisposition::Floating)
    );
    assert_eq!(
        world.entity(restored_window).get::<Unmanaged>().copied(),
        Some(Unmanaged::Floating),
        "explicit floating rules must win over stale managed session state"
    );
    let mut strips = world.query::<&LayoutStrip>();
    assert!(
        !strips
            .iter(world)
            .any(|strip| strip.contains(restored_window))
    );
}

#[test]
fn test_startup_restore_does_not_capture_default_passthrough_window() {
    let mut harness = TestHarness::new()
        .with_default_window_disposition(WindowDisposition::Passthrough)
        .with_windows(1);
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

    let world = harness.world();
    let entity = crate::tests::harness::find_window_entity(0, world);
    assert_eq!(
        world.entity(entity).get::<WindowDisposition>().copied(),
        Some(WindowDisposition::Passthrough)
    );
    assert_eq!(
        world.entity(entity).get::<Unmanaged>().copied(),
        Some(Unmanaged::Passthrough)
    );
    let mut strips = world.query::<&LayoutStrip>();
    assert!(!strips.iter(world).any(|strip| strip.contains(entity)));
}

#[test]
fn test_late_startup_window_restores_during_grace_period() {
    let mut harness = TestHarness::new();
    harness
        .app
        .world_mut()
        .insert_resource(state_with_strips(vec![SavedStrip {
            virtual_index: 1,
            columns: vec![SavedColumn::Single(saved_window(99))],
        }]));

    let commands = vec![
        Event::MenuOpened { window_id: 100 },
        Event::Command {
            command: Command::PrintState,
        },
        Event::Command {
            command: Command::PrintState,
        },
    ];

    harness
        .on_iteration(1, move |world, state| {
            assert!(world.contains_resource::<crate::ecs::restore::SessionRestore>());

            let origin = Origin::new(0, 0);
            let size = Size::new(TEST_WINDOW_WIDTH, TEST_WINDOW_HEIGHT);
            let window = state.spawn_window(
                TEST_PROCESS_ID,
                TEST_WORKSPACE_ID,
                99,
                IRect::from_corners(origin, origin + size),
            );
            world.trigger(SpawnWindowTrigger(vec![window]));
        })
        .on_iteration(2, |world, _state| {
            let restored_window = find_window_entity(99, world);
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
        })
        .run(commands);
}

#[test]
fn test_startup_restore_keeps_one_selected_row_and_hides_inactive_rows() {
    let mut harness = TestHarness::new().with_windows(2);
    harness.world().insert_resource(PaneruState {
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

    let world = harness.world();
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
        width_ratio: None,
        strip_position_x: None,
    }
}

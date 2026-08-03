use bevy::prelude::*;

use crate::config::Config;
use crate::ecs::layout::LayoutStrip;
use crate::ecs::params::Windows;
use crate::ecs::restore::CurrentWindowIdentity;
use crate::ecs::state::QueryState;
use crate::ecs::state::{
    PaneruQueryState, PaneruState, SavedColumn, SavedDisplay, SavedRect, SavedStackItem,
    SavedStrip, SavedWindow, SavedWorkspace,
};
use crate::ecs::{ActiveDisplayMarker, ActiveWorkspaceMarker, SelectedVirtualMarker};
use crate::events::Event;
use crate::manager::{Application, Display, WindowManager};
use crate::platform::{Pid, ProcessSerialNumber, WinID};
use crate::tests::{
    TEST_DISPLAY_HEIGHT, TEST_DISPLAY_ID, TEST_DISPLAY_WIDTH, TEST_MENUBAR_HEIGHT,
    TEST_WORKSPACE_ID,
};
use bevy::ecs::hierarchy::ChildOf;
use bevy::ecs::query::Has;
use bevy::ecs::system::SystemState;

type StateExtractionState<'w, 's> = SystemState<(
    Query<
        'w,
        's,
        (
            Option<&'static ChildOf>,
            &'static LayoutStrip,
            Has<ActiveWorkspaceMarker>,
        ),
    >,
    Query<'w, 's, (&'static Display, Entity, Has<ActiveDisplayMarker>)>,
    Windows<'w, 's>,
    Query<'w, 's, &'static Application>,
)>;

type QueryStateExtractionState<'w, 's> = SystemState<(
    Query<
        'w,
        's,
        (
            &'static ChildOf,
            &'static LayoutStrip,
            Has<ActiveWorkspaceMarker>,
            Has<SelectedVirtualMarker>,
        ),
    >,
    Query<'w, 's, (&'static Display, Entity, Has<ActiveDisplayMarker>)>,
    Windows<'w, 's>,
    Query<'w, 's, &'static Application>,
    Res<'w, WindowManager>,
    Res<'w, Config>,
)>;

fn extract_query_state(world: &mut World) -> crate::errors::Result<PaneruQueryState> {
    let mut system_state: QueryStateExtractionState<'_, '_> = SystemState::new(world);
    let (workspaces, displays, windows, apps, window_manager, config) = system_state.get(world)?;
    PaneruQueryState::extract(
        &workspaces,
        &displays,
        &windows,
        &apps,
        &window_manager,
        &config,
    )
}

#[test]
fn test_state_serialization() {
    let window = SavedWindow {
        window_id: 1,
        pid: 123,
        psn: ProcessSerialNumber { high: 0, low: 1 },
        bundle_id: "com.apple.Finder".to_string(),
        title: "Finder".to_string(),
        identifier: "finder-main".to_string(),
        role: "AXWindow".to_string(),
        subrole: "AXStandardWindow".to_string(),
    };

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
            active_virtual_index: Some(0),
            strips: vec![SavedStrip {
                virtual_index: 0,
                columns: vec![SavedColumn::Single(window)],
            }],
        }],
    };

    let json = serde_json::to_string(&state).expect("Failed to serialize");
    let deserialized: PaneruState = serde_json::from_str(&json).expect("Failed to deserialize");

    assert_eq!(state, deserialized);
}

#[test]
fn test_state_restoration() {
    let window = SavedWindow {
        window_id: 1,
        pid: 123,
        psn: ProcessSerialNumber { high: 0, low: 1 },
        bundle_id: "com.apple.Finder".to_string(),
        title: "Finder".to_string(),
        identifier: "finder-main".to_string(),
        role: "AXWindow".to_string(),
        subrole: "AXStandardWindow".to_string(),
    };

    let state = PaneruState {
        version: 2,
        timestamp: 123_456_789,
        active_display_id: None,
        displays: Vec::new(),
        workspaces: vec![SavedWorkspace {
            workspace_id: TEST_WORKSPACE_ID,
            display_id: None,
            active_virtual_index: Some(1),
            strips: vec![SavedStrip {
                virtual_index: 1,
                columns: vec![SavedColumn::Single(window)],
            }],
        }],
    };

    let matched = state.find_match(1, 123, "com.apple.Finder");

    assert!(matched.is_some());
    let (ws_id, virt_idx, col_idx, _) = matched.unwrap();
    assert_eq!(ws_id, TEST_WORKSPACE_ID);
    assert_eq!(virt_idx, 1);
    assert_eq!(col_idx, 0);
}

#[test]
fn test_state_extraction() {
    use crate::ecs::state::SavedColumn;
    use crate::tests::harness::TestHarness;

    let mut harness = TestHarness::new().with_windows(1);

    // Initial world run to setup windows
    harness.app.update();

    let world = harness.world();
    let mut system_state: StateExtractionState<'_, '_> = SystemState::new(world);
    let (workspaces, displays, windows, apps) =
        system_state.get(world).expect("failed to get world state");

    let state = PaneruState::extract(&workspaces, &displays, &windows, &apps);

    assert_eq!(state.workspaces.len(), 1);
    assert_eq!(state.active_display_id, Some(TEST_DISPLAY_ID));
    assert_eq!(state.displays.len(), 1);
    assert_eq!(state.displays[0].display_id, TEST_DISPLAY_ID);
    assert_eq!(state.displays[0].workspace_ids, vec![TEST_WORKSPACE_ID]);
    assert!(state.displays[0].active);
    assert_eq!(state.workspaces[0].display_id, Some(TEST_DISPLAY_ID));
    assert_eq!(state.workspaces[0].active_virtual_index, Some(0));
    assert_eq!(state.workspaces[0].strips.len(), 1);
    assert_eq!(state.workspaces[0].strips[0].columns.len(), 1);

    if let SavedColumn::Single(ref win) = state.workspaces[0].strips[0].columns[0] {
        assert_eq!(win.window_id, 0); // MockWindow::new(i, ...)
        assert_eq!(win.bundle_id, "test");
    } else {
        panic!("Expected SavedColumn::Single");
    }
}

#[test]
fn test_state_serializes_display_and_active_virtual_workspace() {
    use crate::tests::harness::TestHarness;

    let mut harness = TestHarness::new().with_windows(1);

    harness.app.update();

    let world = harness.world();
    let mut active_display_query =
        world.query_filtered::<Entity, With<crate::ecs::ActiveDisplayMarker>>();
    let display_entity = active_display_query
        .single(world)
        .expect("active display should exist");
    world.spawn((
        LayoutStrip::new(TEST_WORKSPACE_ID, 2),
        ChildOf(display_entity),
    ));

    let mut system_state: StateExtractionState<'_, '_> = SystemState::new(world);
    let (workspaces, displays, windows, apps) =
        system_state.get(world).expect("failed to get world state");

    let state = PaneruState::extract(&workspaces, &displays, &windows, &apps);

    assert_eq!(state.version, 2);
    assert_eq!(state.active_display_id, Some(TEST_DISPLAY_ID));
    assert_eq!(
        state.displays,
        vec![SavedDisplay {
            display_id: TEST_DISPLAY_ID,
            bounds: SavedRect {
                min_x: 0,
                min_y: TEST_MENUBAR_HEIGHT,
                max_x: TEST_DISPLAY_WIDTH,
                max_y: TEST_DISPLAY_HEIGHT,
            },
            active: true,
            workspace_ids: vec![TEST_WORKSPACE_ID],
        }]
    );
    assert_eq!(state.workspaces.len(), 1);
    assert_eq!(state.workspaces[0].display_id, Some(TEST_DISPLAY_ID));
    assert_eq!(state.workspaces[0].active_virtual_index, Some(0));
    assert_eq!(state.workspaces[0].strips.len(), 2);

    let json = serde_json::to_value(&state).expect("state should serialize");
    assert_eq!(json["active_display_id"], TEST_DISPLAY_ID);
    assert_eq!(json["displays"][0]["display_id"], TEST_DISPLAY_ID);
    assert_eq!(
        json["displays"][0]["workspace_ids"],
        serde_json::json!([TEST_WORKSPACE_ID])
    );
    assert_eq!(json["workspaces"][0]["display_id"], TEST_DISPLAY_ID);
    assert_eq!(json["workspaces"][0]["active_virtual_index"], 0);
}

#[test]
fn test_state_extraction_includes_parentless_layout_strips() {
    use crate::tests::harness::TestHarness;

    let mut harness = TestHarness::new().with_windows(0);

    harness.app.update();

    let world = harness.world();
    let parentless_workspace_id = TEST_WORKSPACE_ID + 1;
    world.spawn(LayoutStrip::new(parentless_workspace_id, 0));

    let mut system_state: StateExtractionState<'_, '_> = SystemState::new(world);
    let (workspaces, displays, windows, apps) =
        system_state.get(world).expect("failed to get world state");

    let state = PaneruState::extract(&workspaces, &displays, &windows, &apps);

    let parentless_workspace = state
        .workspaces
        .iter()
        .find(|workspace| workspace.workspace_id == parentless_workspace_id)
        .expect("parentless layout strip should be saved");
    assert_eq!(parentless_workspace.display_id, None);
    assert_eq!(parentless_workspace.active_virtual_index, None);
    assert_eq!(parentless_workspace.strips.len(), 1);
    assert_eq!(parentless_workspace.strips[0].virtual_index, 0);

    assert!(
        state
            .displays
            .iter()
            .all(|display| !display.workspace_ids.contains(&parentless_workspace_id)),
        "parentless layout strip should not be associated with any display"
    );
}

#[test]
fn test_state_load_rejects_unsupported_version() {
    let path = unique_state_path("unsupported-version");
    std::fs::write(
        &path,
        r#"{"version":1,"timestamp":123456789,"workspaces":[]}"#,
    )
    .expect("state fixture should write");

    assert!(PaneruState::load_from_file(&path).is_none());

    let _ = std::fs::remove_file(path);
}

#[test]
fn test_state_save_is_loadable_from_path() {
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
            active_virtual_index: Some(0),
            strips: Vec::new(),
        }],
    };
    let path = unique_state_path("save-load");

    state
        .save_to_file(&path)
        .expect("state should save to requested path");

    let loaded = PaneruState::load_from_file(&path).expect("state should load from saved path");
    assert_eq!(loaded, state);

    let _ = std::fs::remove_file(path);
}

fn unique_state_path(name: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!(
        "paneru-{name}-{}-{}.json",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock should be after unix epoch")
            .as_nanos()
    ))
}

#[test]
fn restore_plan_compacts_missing_windows_and_preserves_active_virtual_row() {
    use crate::ecs::restore::{PlannedColumn, RestorePlanner};

    let mut world = World::new();
    let present_tab = world.spawn_empty().id();
    let present_stack_tab = world.spawn_empty().id();

    let state = restore_state(vec![SavedWorkspace {
        workspace_id: TEST_WORKSPACE_ID,
        display_id: Some(TEST_DISPLAY_ID),
        active_virtual_index: Some(1),
        strips: vec![
            SavedStrip {
                virtual_index: 0,
                columns: vec![SavedColumn::Single(saved_window(
                    10,
                    110,
                    "com.example.missing",
                    "Missing",
                ))],
            },
            SavedStrip {
                virtual_index: 1,
                columns: vec![
                    SavedColumn::Tabs(vec![
                        saved_window(11, 111, "com.example.missing", "Missing Tab"),
                        saved_window(12, 112, "com.example.editor", "Editor"),
                    ]),
                    SavedColumn::Stack(vec![
                        SavedStackItem::Single(saved_window(
                            13,
                            113,
                            "com.example.missing",
                            "Missing Stack",
                        )),
                        SavedStackItem::Tabs(vec![
                            saved_window(14, 114, "com.example.terminal", "Terminal"),
                            saved_window(15, 115, "com.example.missing", "Missing Stack Tab"),
                        ]),
                    ]),
                ],
            },
        ],
    }]);
    let current = vec![
        current_window(present_tab, 12, 112, "com.example.editor", "Editor"),
        current_window(
            present_stack_tab,
            14,
            114,
            "com.example.terminal",
            "Terminal",
        ),
    ];

    let plan = RestorePlanner::new(&state).plan(&current);

    assert_eq!(plan.strips.len(), 1);
    assert_eq!(plan.strips[0].workspace_id, TEST_WORKSPACE_ID);
    assert_eq!(plan.strips[0].display_id, Some(TEST_DISPLAY_ID));
    assert_eq!(plan.strips[0].virtual_index, 1);
    assert_eq!(
        plan.active_virtual_by_workspace.get(&TEST_WORKSPACE_ID),
        Some(&1)
    );
    assert_eq!(
        plan.strips[0].columns,
        vec![
            PlannedColumn::Single(present_tab),
            PlannedColumn::Single(present_stack_tab),
        ]
    );
    assert_eq!(
        plan.consumed_entities,
        [present_tab, present_stack_tab].into_iter().collect()
    );
    assert_eq!(plan.ignored_missing_windows, 4);
    assert_eq!(plan.skipped_ambiguous_matches, 0);
}

#[test]
fn restore_plan_prefers_later_hard_match_over_earlier_fallback_match() {
    use crate::ecs::restore::{PlannedColumn, RestorePlanner};

    let mut world = World::new();
    let current_x = world.spawn_empty().id();
    let saved_a = saved_window(20, 120, "com.example.notes", "Daily Notes");
    let saved_b = saved_window(21, 121, "com.example.notes", "Daily Notes");
    let state = restore_state(vec![SavedWorkspace {
        workspace_id: TEST_WORKSPACE_ID,
        display_id: Some(TEST_DISPLAY_ID),
        active_virtual_index: Some(0),
        strips: vec![
            SavedStrip {
                virtual_index: 0,
                columns: vec![SavedColumn::Single(saved_a)],
            },
            SavedStrip {
                virtual_index: 1,
                columns: vec![SavedColumn::Single(saved_b)],
            },
        ],
    }]);
    let current = vec![current_window(
        current_x,
        21,
        121,
        "com.example.notes",
        "Daily Notes",
    )];

    let plan = RestorePlanner::new(&state).plan(&current);

    assert_eq!(plan.strips.len(), 1);
    assert_eq!(plan.strips[0].virtual_index, 1);
    assert_eq!(
        plan.strips[0].columns,
        vec![PlannedColumn::Single(current_x)]
    );
    assert_eq!(plan.consumed_entities, [current_x].into_iter().collect());
    assert_eq!(plan.ignored_missing_windows, 1);
    assert_eq!(plan.skipped_ambiguous_matches, 0);
}

#[test]
fn restore_plan_skips_ambiguous_fallback_match() {
    use crate::ecs::restore::{CurrentWindowIdentity, RestorePlanner};

    let mut world = World::new();
    let first = world.spawn_empty().id();
    let second = world.spawn_empty().id();
    let saved = saved_window(20, 120, "com.example.notes", "Daily Notes");
    let state = restore_state(vec![SavedWorkspace {
        workspace_id: TEST_WORKSPACE_ID,
        display_id: None,
        active_virtual_index: Some(0),
        strips: vec![SavedStrip {
            virtual_index: 0,
            columns: vec![SavedColumn::Single(saved)],
        }],
    }]);
    let current = vec![
        CurrentWindowIdentity::fallback_only(first, "com.example.notes", "Daily Notes"),
        CurrentWindowIdentity::fallback_only(second, "com.example.notes", "Daily Notes"),
    ];

    let plan = RestorePlanner::new(&state).plan(&current);

    assert!(plan.strips.is_empty());
    assert!(plan.active_virtual_by_workspace.is_empty());
    assert!(plan.consumed_entities.is_empty());
    assert_eq!(plan.ignored_missing_windows, 0);
    assert_eq!(plan.skipped_ambiguous_matches, 1);
}

fn restore_state(workspaces: Vec<SavedWorkspace>) -> PaneruState {
    PaneruState {
        version: 2,
        timestamp: 123_456_789,
        active_display_id: Some(TEST_DISPLAY_ID),
        displays: Vec::new(),
        workspaces,
    }
}

fn saved_window(window_id: WinID, pid: Pid, bundle_id: &str, title: &str) -> SavedWindow {
    SavedWindow {
        window_id,
        pid,
        psn: ProcessSerialNumber { high: 0, low: 1 },
        bundle_id: bundle_id.to_string(),
        title: title.to_string(),
        identifier: "main".to_string(),
        role: "AXWindow".to_string(),
        subrole: "AXStandardWindow".to_string(),
    }
}

fn current_window(
    entity: Entity,
    window_id: WinID,
    pid: Pid,
    bundle_id: &str,
    title: &str,
) -> CurrentWindowIdentity {
    CurrentWindowIdentity {
        entity,
        window_id,
        pid,
        bundle_id: bundle_id.to_string(),
        title: title.to_string(),
        identifier: "main".to_string(),
        role: "AXWindow".to_string(),
        subrole: "AXStandardWindow".to_string(),
    }
}

#[test]
fn test_query_state_contract_exposes_active_virtual_workspace_and_windows() {
    use crate::tests::harness::TestHarness;

    let mut harness = TestHarness::new().with_windows(1);

    harness.app.update();

    let world = harness.world();
    let mut active_display_query =
        world.query_filtered::<Entity, With<crate::ecs::ActiveDisplayMarker>>();
    let display_entity = active_display_query
        .single(world)
        .expect("active display should exist");
    world.spawn((
        LayoutStrip::new(TEST_WORKSPACE_ID, 2),
        ChildOf(display_entity),
    ));

    let state = extract_query_state(world).expect("query state extraction");

    assert_eq!(state.version, 1);
    assert_eq!(state.active.virtual_workspace_number, Some(1));
    assert_eq!(state.active.native_workspace_id, Some(TEST_WORKSPACE_ID));
    assert_eq!(state.active.focused_window_id, Some(0));
    assert_eq!(state.active.focused_bundle_id.as_deref(), Some("test"));
    assert_eq!(state.virtual_workspaces.len(), 3);
    assert_eq!(state.virtual_workspaces[0].number, 1);
    assert!(state.virtual_workspaces[0].active);
    assert_eq!(state.virtual_workspaces[0].windows.len(), 1);
    assert_eq!(state.virtual_workspaces[0].windows[0].window_id, 0);
    assert_eq!(state.virtual_workspaces[0].windows[0].bundle_id, "test");
    assert!(state.virtual_workspaces[0].windows[0].focused);
    assert_eq!(state.virtual_workspaces[1].number, 2);
    assert!(state.virtual_workspaces[1].windows.is_empty());
    assert_eq!(state.virtual_workspaces[2].number, 3);
    assert!(state.virtual_workspaces[2].windows.is_empty());

    let json = serde_json::to_value(&state).expect("query state should serialize");
    assert_eq!(json["active"]["virtual_workspace_number"], 1);
    assert_eq!(
        json["virtual_workspaces"][0]["windows"][0]["bundle_id"],
        "test"
    );
}

#[test]
fn test_query_state_includes_floating_windows() {
    use crate::commands::{Command, Operation};
    use crate::tests::harness::TestHarness;

    let mut harness = TestHarness::new().with_windows(1);
    harness.app.update();
    harness.app.world_mut().write_message(Event::Command {
        command: Command::Window(Operation::Manage),
    });
    harness.app.update();

    let state = extract_query_state(harness.world()).expect("query state extraction");

    assert_eq!(state.active.focused_window_id, Some(0));
    assert_eq!(state.virtual_workspaces[0].windows.len(), 1);
    assert!(state.virtual_workspaces[0].windows[0].focused);
    assert!(state.virtual_workspaces[0].windows[0].floating);
}

#[test]
fn test_query_state_assigns_float_only_to_active_row() {
    use crate::commands::{Command, Operation};
    use crate::tests::harness::TestHarness;

    let mut harness = TestHarness::new().with_windows(1);
    harness.app.update();
    harness.app.world_mut().write_message(Event::Command {
        command: Command::Window(Operation::Manage),
    });
    harness.app.update();

    let world = harness.world();
    let display_entity = world
        .query_filtered::<Entity, With<ActiveDisplayMarker>>()
        .single(world)
        .expect("active display");
    let active_strip_entity = world
        .query_filtered::<Entity, With<ActiveWorkspaceMarker>>()
        .single(world)
        .expect("active workspace");
    world
        .entity_mut(active_strip_entity)
        .remove::<SelectedVirtualMarker>();
    world.spawn((
        LayoutStrip::new(TEST_WORKSPACE_ID, 1),
        ChildOf(display_entity),
        SelectedVirtualMarker,
    ));

    let state = extract_query_state(world).expect("query state extraction");
    let occurrences = state
        .virtual_workspaces
        .iter()
        .flat_map(|workspace| &workspace.windows)
        .filter(|window| window.window_id == 0)
        .count();
    let active = state
        .virtual_workspaces
        .iter()
        .find(|workspace| workspace.active)
        .expect("active workspace state");

    assert_eq!(occurrences, 1);
    assert_eq!(active.windows[0].window_id, 0);
}

#[test]
fn test_query_state_propagates_workspace_query_failure() {
    use crate::errors::Error;
    use crate::manager::MockWindowManagerApi;
    use crate::tests::harness::TestHarness;

    let mut harness = TestHarness::new().with_windows(1);
    harness.app.update();

    let mut window_manager = MockWindowManagerApi::new();
    window_manager
        .expect_windows_in_workspace()
        .returning(|_| Err(Error::Generic("workspace query failed".to_string())));
    harness
        .world()
        .insert_resource(WindowManager(Box::new(window_manager)));

    let error = extract_query_state(harness.world()).expect_err("workspace query should fail");

    assert_eq!(error.to_string(), "Generic error: workspace query failed");
}

#[test]
fn test_query_state_includes_configured_floating_windows() {
    use crate::config::{Config, MainOptions, WindowParams};
    use crate::tests::harness::TestHarness;

    let mut params = WindowParams::new(".*", Some("test".to_string()));
    params.floating = Some(true);
    let config: Config = (MainOptions::default(), vec![params]).into();
    let mut harness = TestHarness::new().with_config(config).with_windows(1);
    harness.app.update();
    harness.app.update();

    let state = extract_query_state(harness.world()).expect("query state extraction");

    assert_eq!(state.virtual_workspaces[0].windows.len(), 1);
    assert!(state.virtual_workspaces[0].windows[0].floating);
}

#[test]
fn test_query_state_tracks_float_after_virtual_workspace_is_reaped() {
    use crate::commands::{Command, MoveFocus, Operation};
    use crate::config::{Config, MainOptions};
    use crate::tests::harness::TestHarness;

    let config: Config = (
        MainOptions {
            reap_empty_workspaces: Some(true),
            ..Default::default()
        },
        vec![],
    )
        .into();

    let harness = TestHarness::new()
        .with_config(config)
        .with_display(
            TEST_DISPLAY_ID,
            IRect::new(0, 0, TEST_DISPLAY_WIDTH, TEST_DISPLAY_HEIGHT),
            vec![TEST_WORKSPACE_ID, TEST_WORKSPACE_ID + 1],
        )
        .with_windows(1);

    let commands = vec![
        Event::MenuOpened { window_id: 0 },
        Event::Command {
            command: Command::Window(Operation::VirtualMoveNumber(1, MoveFocus::Follow)),
        },
        Event::Command {
            command: Command::Window(Operation::Manage),
        },
        Event::Command {
            command: Command::Window(Operation::VirtualNumber(0)),
        },
        Event::Command {
            command: Command::PrintState,
        },
    ];

    harness
        .on_iteration(3, |world, _state| {
            let state = extract_query_state(world).expect("query state extraction");
            let active = state
                .virtual_workspaces
                .iter()
                .find(|workspace| workspace.active)
                .expect("active virtual workspace");

            assert_eq!(state.active.virtual_workspace_number, Some(1));
            assert_eq!(state.active.focused_window_id, Some(0));
            assert!(
                !state.virtual_workspaces.iter().any(|workspace| {
                    workspace.native_workspace_id == TEST_WORKSPACE_ID && workspace.number == 2
                }),
                "the empty remembered row should be reaped"
            );
            assert_eq!(active.windows.len(), 1);
            assert!(active.windows[0].focused);
            assert!(active.windows[0].floating);
        })
        .on_iteration(4, |world, state| {
            state.update_window(0, |window| window.workspace_id = TEST_WORKSPACE_ID + 1);
            let moved = extract_query_state(world).expect("query state extraction");
            let original_workspace = moved
                .virtual_workspaces
                .iter()
                .find(|workspace| workspace.native_workspace_id == TEST_WORKSPACE_ID)
                .expect("original native workspace");
            let live_workspace = moved
                .virtual_workspaces
                .iter()
                .find(|workspace| workspace.native_workspace_id == TEST_WORKSPACE_ID + 1)
                .expect("live native workspace");

            assert!(original_workspace.windows.is_empty());
            assert_eq!(live_workspace.windows.len(), 1);
            assert!(live_workspace.windows[0].floating);
        })
        .run(commands);
}

/// Builds the `WindowSet` a Lua handler would be given, from the live world.
#[cfg(feature = "lua")]
fn extract_window_set(
    world: &mut World,
) -> crate::errors::Result<paneru_shared_types::windowset::WindowSet> {
    use crate::ecs::state::QueryStateParams;

    let mut system_state: SystemState<QueryStateParams> = SystemState::new(world);
    let params = system_state.get(world)?;
    params.extract_window_set()
}

#[cfg(feature = "lua")]
#[test]
fn test_window_set_keeps_the_column_structure_a_flat_query_loses() {
    use crate::tests::harness::TestHarness;
    use paneru_shared_types::windowset::ColumnKind;

    let mut harness = TestHarness::new().with_windows(3);
    harness.app.update();

    let set = extract_window_set(harness.world()).expect("window set extraction");

    assert_eq!(set.displays().len(), 1);
    let display = &set.displays()[0];
    assert_eq!(display.id, TEST_DISPLAY_ID);
    assert!(display.active);

    let workspace = set.current().expect("an active workspace");
    assert_eq!(workspace.number, 1);
    assert_eq!(workspace.native_id, TEST_WORKSPACE_ID);
    assert_eq!(
        workspace.columns.len(),
        3,
        "three unstacked windows are three columns"
    );
    for column in workspace.columns.iter() {
        assert_eq!(column.kind, ColumnKind::Single);
        assert_eq!(column.windows.len(), 1);
    }

    // ...and that structure is what makes adjacency answerable at all.
    let leftmost = workspace.columns[0].top().expect("a window").id;
    let middle = workspace.columns[1].top().expect("a window").id;
    assert_eq!(set.east(leftmost), Some(middle));
    assert_eq!(set.west(middle), Some(leftmost));
    assert_eq!(set.column_of(middle), Some(1));
}

#[cfg(feature = "lua")]
#[test]
fn test_window_set_reports_focus_and_window_identity() {
    use crate::tests::harness::TestHarness;

    let mut harness = TestHarness::new().with_windows(2);
    harness.app.update();

    let set = extract_window_set(harness.world()).expect("window set extraction");

    let focused = set.focused().expect("something should be focused");
    let window = set
        .window(focused)
        .expect("the focused window is in the set");
    assert!(window.focused);
    assert!(window.managed, "a tiled window is managed");
    assert!(!window.floating);
    assert_eq!(window.bundle_id, "test");
    assert!(window.frame.is_some(), "a laid-out window has a frame");
    assert_eq!(
        set.windows().filter(|window| window.focused).count(),
        1,
        "exactly one window is focused"
    );
}

#[cfg(feature = "lua")]
#[test]
fn test_window_set_marks_floating_windows_outside_the_strip() {
    use crate::commands::{Command, Operation};
    use crate::tests::harness::TestHarness;

    let mut harness = TestHarness::new().with_windows(1);
    harness.app.update();
    harness.app.world_mut().write_message(Event::Command {
        command: Command::Window(Operation::Manage),
    });
    harness.app.update();

    let set = extract_window_set(harness.world()).expect("window set extraction");
    let workspace = set.current().expect("an active workspace");

    assert!(
        workspace.columns.is_empty(),
        "a floating window holds no column, even though the strip still tracks it"
    );
    assert_eq!(workspace.floating.len(), 1);
    assert!(workspace.floating[0].floating);
    assert!(!workspace.floating[0].managed);
}

#[cfg(feature = "lua")]
#[test]
fn test_layout_ops_apply_to_the_named_window_not_the_focused_one() {
    use crate::commands::Command;
    use crate::tests::harness::TestHarness;
    use paneru_shared_types::windowset::LayoutOp;

    let mut harness = TestHarness::new().with_windows(3);
    harness.app.update();

    let before = extract_window_set(harness.world()).expect("window set extraction");
    let workspace = before.current().expect("an active workspace");
    let order: Vec<i32> = workspace
        .columns
        .iter()
        .filter_map(|column| column.top().map(|window| window.id))
        .collect();
    assert_eq!(order.len(), 3);
    let focused = before.focused().expect("something is focused");

    // Swap the two windows that are *not* focused: every existing command
    // handler would have acted on the focused one instead.
    let (left, right) = (order[1], order[2]);
    assert!(
        left != focused && right != focused,
        "swapping unfocused windows"
    );

    harness.app.world_mut().write_message(Event::Command {
        command: Command::Layout(vec![LayoutOp::Swap(left, right)]),
    });
    harness.app.update();
    harness.app.update();

    let after = extract_window_set(harness.world()).expect("window set extraction");
    let swapped: Vec<i32> = after
        .current()
        .expect("an active workspace")
        .columns
        .iter()
        .filter_map(|column| column.top().map(|window| window.id))
        .collect();
    assert_eq!(swapped, vec![order[0], right, left]);
    assert_eq!(after.focused(), Some(focused), "the focus did not move");
}

#[cfg(feature = "lua")]
#[test]
fn test_layout_ops_skip_a_vanished_window_and_apply_its_neighbours() {
    use crate::commands::Command;
    use crate::tests::harness::TestHarness;
    use paneru_shared_types::windowset::LayoutOp;

    let mut harness = TestHarness::new().with_windows(2);
    harness.app.update();

    let before = extract_window_set(harness.world()).expect("window set extraction");
    let live = before.focused().expect("something is focused");
    let gone = 9999;
    assert!(before.window(gone).is_none(), "no such window");

    // The handler ran against a stale snapshot: one of its targets is gone.
    // That op is dropped; the rest of the batch still applies.
    harness.app.world_mut().write_message(Event::Command {
        command: Command::Layout(vec![
            LayoutOp::Focus(gone),
            LayoutOp::SetFloating {
                window: live,
                floating: true,
            },
        ]),
    });
    harness.app.update();
    harness.app.update();

    let after = extract_window_set(harness.world()).expect("window set extraction");
    let workspace = after.current().expect("an active workspace");
    assert!(
        workspace.floating.iter().any(|window| window.id == live),
        "the surviving op applied"
    );
}

#[cfg(feature = "lua")]
#[test]
fn test_set_frame_places_a_floating_window() {
    use crate::commands::Command;
    use crate::tests::harness::TestHarness;
    use paneru_shared_types::state::Frame;
    use paneru_shared_types::windowset::LayoutOp;

    let mut harness = TestHarness::new().with_windows(1);
    // Let the window settle into the strip first: a window still being added
    // is appended to the layout on the tick it appears, which would undo the
    // float on the very tick that asked for it.
    for _ in 0..3 {
        harness.app.update();
    }

    let before = extract_window_set(harness.world()).expect("window set extraction");
    let window = before.focused().expect("something is focused");

    // xmonad's customFloating: take it out of the layout, then place it.
    let placed = Frame {
        x: 100,
        y: 120,
        width: 640,
        height: 480,
    };
    harness.app.world_mut().write_message(Event::Command {
        command: Command::Layout(vec![
            LayoutOp::SetFloating {
                window,
                floating: true,
            },
            LayoutOp::SetFrame {
                window,
                frame: placed,
            },
        ]),
    });
    // Several ticks: the placement has to survive the layout passes that follow,
    // not just land for one frame.
    for _ in 0..4 {
        harness.app.update();
    }

    let after = extract_window_set(harness.world()).expect("window set extraction");
    let record = after.window(window).expect("the window survived");
    assert!(record.floating, "it should be out of the tiling layout");
    assert_eq!(
        record.frame,
        Some(placed),
        "and sitting exactly where it was put"
    );
}

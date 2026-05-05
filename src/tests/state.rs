use bevy::prelude::*;

use crate::ecs::state::{
    PaneruQueryState, PaneruState, SavedColumn, SavedStrip, SavedWindow, SavedWorkspace,
};
use crate::platform::ProcessSerialNumber;
use crate::tests::TEST_WORKSPACE_ID;

#[test]
fn test_state_serialization() {
    let window = SavedWindow {
        window_id: 1,
        pid: 123,
        psn: ProcessSerialNumber { high: 0, low: 1 },
        bundle_id: "com.apple.Finder".to_string(),
        identifier: "finder-main".to_string(),
        role: "AXWindow".to_string(),
        subrole: "AXStandardWindow".to_string(),
    };

    let state = PaneruState {
        version: 1,
        timestamp: 123_456_789,
        workspaces: vec![SavedWorkspace {
            workspace_id: TEST_WORKSPACE_ID,
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
        identifier: "finder-main".to_string(),
        role: "AXWindow".to_string(),
        subrole: "AXStandardWindow".to_string(),
    };

    let state = PaneruState {
        version: 1,
        timestamp: 123_456_789,
        workspaces: vec![SavedWorkspace {
            workspace_id: TEST_WORKSPACE_ID,
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
    use crate::ecs::layout::LayoutStrip;
    use crate::ecs::params::Windows;
    use crate::ecs::state::SavedColumn;
    use crate::manager::Application;
    use crate::tests::harness::TestHarness;
    use bevy::ecs::system::SystemState;

    let mut harness = TestHarness::new().with_windows(1);

    // Initial world run to setup windows
    harness.app.update();

    let world = harness.app.world_mut();
    let mut system_state: SystemState<(Query<&LayoutStrip>, Windows, Query<&Application>)> =
        SystemState::new(world);
    let (query, windows, apps) = system_state.get(world);

    let state = PaneruState::extract(&query, &windows, &apps);

    assert_eq!(state.workspaces.len(), 1);
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
#[allow(clippy::type_complexity)]
fn test_query_state_contract_exposes_active_virtual_workspace_and_windows() {
    use crate::ecs::layout::LayoutStrip;
    use crate::ecs::params::Windows;
    use crate::manager::{Application, Display};
    use crate::tests::harness::TestHarness;
    use bevy::ecs::hierarchy::ChildOf;
    use bevy::ecs::query::Has;
    use bevy::ecs::system::SystemState;

    let mut harness = TestHarness::new().with_windows(1);

    harness.app.update();

    let world = harness.app.world_mut();
    let mut active_display_query =
        world.query_filtered::<Entity, With<crate::ecs::ActiveDisplayMarker>>();
    let display_entity = active_display_query
        .single(world)
        .expect("active display should exist");
    world.spawn((
        LayoutStrip::new(TEST_WORKSPACE_ID, 2),
        ChildOf(display_entity),
    ));

    let mut system_state: SystemState<(
        Query<(
            &ChildOf,
            &LayoutStrip,
            Has<crate::ecs::ActiveWorkspaceMarker>,
        )>,
        Query<(&Display, Entity, Has<crate::ecs::ActiveDisplayMarker>)>,
        Windows,
        Query<&Application>,
    )> = SystemState::new(world);
    let (workspaces, displays, windows, apps) = system_state.get(world);

    let state = PaneruQueryState::extract(&workspaces, &displays, &windows, &apps);

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

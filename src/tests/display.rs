use std::time::Duration;

use bevy::prelude::*;
use bevy::time::TimeUpdateStrategy;

use crate::commands::{Command, Direction, MouseMove, MoveFocus, Operation};
use crate::config::{Config, MainOptions};
use crate::ecs::layout::{LayoutStrip, PARKED_STRIP_SLIVER};
use crate::ecs::{ActiveWorkspaceMarker, DockPosition, RefreshWindowSizes, Timeout};
use crate::events::Event;
use crate::manager::{Display, Origin, Size, Window};
use crate::platform::WinID;
use crate::{assert_not_on_workspace, assert_on_workspace, assert_window_at, assert_window_size};

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
        .on_iteration(1, |world, state| {
            let mut query = world.query_filtered::<Entity, With<Display>>();
            query.single(world).expect("should have one display");
            state.remove_display(TEST_DISPLAY_ID);
        })
        .on_iteration(2, |world, mut state| {
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
            state.add_display(
                TEST_DISPLAY_ID,
                IRect::new(0, 0, TEST_DISPLAY_WIDTH, TEST_DISPLAY_HEIGHT),
                vec![TEST_WORKSPACE_ID],
            );
        })
        .on_iteration(3, |world, _state| {
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

    let workspaces = vec![TEST_WORKSPACE_ID, TEST_WORKSPACE_ID + 1];
    let harness = TestHarness::new().with_display(
        TEST_DISPLAY_ID,
        IRect::new(0, 0, TEST_DISPLAY_WIDTH, TEST_DISPLAY_HEIGHT),
        workspaces,
    );
    harness
        .on_iteration(1, |world, state| {
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
            state.remove_display(TEST_DISPLAY_ID);
        })
        .on_iteration(2, |world, _state| {
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
    let mut harness = TestHarness::new();
    harness.mock_state.add_display(
        EXT_DISPLAY_ID,
        IRect::new(0, -EXT_DISPLAY_HEIGHT, EXT_DISPLAY_WIDTH, 0),
        vec![EXT_WORKSPACE_ID],
    );

    let origin = Origin::new(0, 0);
    let ext_origin = Origin::new(0, -EXT_DISPLAY_HEIGHT + TEST_MENUBAR_HEIGHT);
    let size = Size::new(TEST_WINDOW_WIDTH, TEST_WINDOW_HEIGHT);
    let frame = IRect::from_corners(origin, origin + size);
    let ext_frame = IRect::from_corners(ext_origin, ext_origin + size);

    harness
        .mock_state
        .spawn_window(TEST_PROCESS_ID, EXT_WORKSPACE_ID, 100, ext_frame);
    harness
        .mock_state
        .spawn_window(TEST_PROCESS_ID, TEST_WORKSPACE_ID, 200, frame);

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
        .on_iteration(1, move |world, _state| {
            assert_window_size!(world, 100, TEST_WINDOW_WIDTH, ext_usable_height);
        })
        .on_iteration(2, |world, _state| {
            use crate::ecs::ActiveWorkspaceMarker;
            let mut strip_query =
                world.query_filtered::<&mut LayoutStrip, Without<ActiveWorkspaceMarker>>();
            for mut strip in strip_query.iter_mut(world) {
                strip.set_changed();
            }
        })
        .on_iteration(4, move |world, _state| {
            assert_window_size!(world, 100, TEST_WINDOW_WIDTH, ext_usable_height);
        })
        .run(commands);
}

#[test]
fn test_next_display_inserts_into_target_strip() {
    let commands = vec![
        Event::MenuOpened { window_id: 0 },
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

    TestHarness::new()
        .with_windows(1)
        .with_display(
            EXT_DISPLAY_ID,
            IRect::new(0, -EXT_DISPLAY_HEIGHT, EXT_DISPLAY_WIDTH, 0),
            vec![EXT_WORKSPACE_ID],
        )
        .on_iteration(1, move |world, _state| {
            assert_on_workspace!(world, 0, TEST_WORKSPACE_ID);
        })
        .on_iteration(2, move |world, _state| {
            assert_on_workspace!(world, 0, EXT_WORKSPACE_ID);
            assert_not_on_workspace!(world, 0, TEST_WORKSPACE_ID);
        })
        .run(commands);
}

#[test]
fn test_send_next_display_stays_on_source() {
    let mut harness = TestHarness::new();
    harness.mock_state.add_display(
        EXT_DISPLAY_ID,
        IRect::new(0, -EXT_DISPLAY_HEIGHT, EXT_DISPLAY_WIDTH, 0),
        vec![EXT_WORKSPACE_ID],
    );

    let origin = Origin::new(0, 0);
    let size = Size::new(TEST_WINDOW_WIDTH, TEST_WINDOW_HEIGHT);
    let frame = IRect::from_corners(origin, origin + size);

    harness
        .mock_state
        .spawn_window(TEST_PROCESS_ID, TEST_WORKSPACE_ID, 101, frame);
    harness
        .mock_state
        .spawn_window(TEST_PROCESS_ID, TEST_WORKSPACE_ID, 100, frame);

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
        .on_iteration(1, move |world, _state| {
            assert_on_workspace!(world, 100, TEST_WORKSPACE_ID);
        })
        .on_iteration(2, move |world, state| {
            assert_on_workspace!(world, 100, EXT_WORKSPACE_ID);
            assert_not_on_workspace!(world, 100, TEST_WORKSPACE_ID);
            assert_eq!(state.active_display(), TEST_DISPLAY_ID);
        })
        .run(commands);
}

#[test]
fn test_mouse_to_next_display() {
    let commands = vec![
        Event::MenuOpened { window_id: 101 },
        Event::Command {
            command: Command::PrintState,
        },
        Event::Command {
            command: Command::Mouse(MouseMove::ToNextDisplay),
        },
        Event::Command {
            command: Command::PrintState,
        },
    ];
    let origin = Origin::new(0, 0);
    let size = Size::new(TEST_WINDOW_WIDTH, TEST_WINDOW_HEIGHT);
    let frame = IRect::from_corners(origin, origin + size);
    let display_bounds = IRect::new(0, -EXT_DISPLAY_HEIGHT, EXT_DISPLAY_WIDTH, 0);

    // harness
    //     .mock_state
    //     .spawn_window(TEST_PROCESS_ID, TEST_WORKSPACE_ID, 101, frame);
    // harness
    //     .mock_state
    //     .spawn_window(TEST_PROCESS_ID, TEST_WORKSPACE_ID, 100, frame);
    TestHarness::new()
        .with_display(EXT_DISPLAY_ID, display_bounds, vec![EXT_WORKSPACE_ID])
        .with_window(100, |data| {
            data.pid = TEST_PROCESS_ID;
            data.workspace_id = TEST_WORKSPACE_ID;
            data.frame = frame;
        })
        .on_iteration(1, move |world, state| {
            let entity = find_window_entity(100, world);
            let window = world.get::<Window>(entity).expect("need window");
            assert_eq!(state.cursor_position(), window.frame().center());
        })
        .on_iteration(3, move |world, state| {
            let mut query = world.query::<(&Display, Option<&DockPosition>)>();
            let (display, dock) = query
                .iter(world)
                .find(|display| display.0.id() == EXT_DISPLAY_ID)
                .expect("need display");
            let config = world.resource::<Config>();
            let bounds = display.actual_display_bounds(dock, config);
            assert_eq!(state.cursor_position(), bounds.center());
        })
        .run(commands);
}

/// Regression test: paneru's init pass must not drag windows that live on
/// inactive displays onto the active display. `apply_window_properties`
/// initially appends every observed window to the active strip; if the
/// layout writers run before `finish_setup` has reassigned them, they
/// cache active-display coordinates into `Position` and `commit_window_position`
/// later pushes those to macOS, moving the windows.
#[test]
fn test_init_keeps_windows_on_their_real_displays() {
    // Internal (test) display is active. Window 100 lives on the external
    // display's space, window 200 lives on the active display's space.

    let mut harness = TestHarness::new();
    harness.mock_state.add_display(
        EXT_DISPLAY_ID,
        IRect::new(0, -EXT_DISPLAY_HEIGHT, EXT_DISPLAY_WIDTH, 0),
        vec![EXT_WORKSPACE_ID],
    );

    let origin = Origin::new(0, 0);
    let ext_origin = Origin::new(0, -EXT_DISPLAY_HEIGHT + TEST_MENUBAR_HEIGHT);
    let size = Size::new(TEST_WINDOW_WIDTH, TEST_WINDOW_HEIGHT);
    let frame = IRect::from_corners(origin, origin + size);
    let ext_frame = IRect::from_corners(ext_origin, ext_origin + size);

    harness
        .mock_state
        .spawn_window(TEST_PROCESS_ID, TEST_WORKSPACE_ID, 200, ext_frame);
    harness
        .mock_state
        .spawn_window(TEST_PROCESS_ID, EXT_WORKSPACE_ID, 100, frame);

    let commands = vec![
        Event::Command {
            command: Command::PrintState,
        },
        Event::Command {
            command: Command::PrintState,
        },
    ];

    harness
        .on_iteration(0, move |world, _state| {
            assert_on_workspace!(world, 100, EXT_WORKSPACE_ID);
            assert_not_on_workspace!(world, 100, TEST_WORKSPACE_ID);
            assert_on_workspace!(world, 200, TEST_WORKSPACE_ID);
            assert_not_on_workspace!(world, 200, EXT_WORKSPACE_ID);
            // The OS frame for window 100 must stay within the external
            // display's vertical bounds (negative y); if init moved it
            // onto the active display the frame would land at y >= 0.
            assert_window_at!(world, 100, ext_origin.x, ext_origin.y);
        })
        .run(commands);
}

/// Waking from sleep (or a resolution/configuration change) with a monitor
/// gone should reconcile the ECS display set against the OS even though no
/// per-display `DisplayRemoved` flag arrives: the vanished display is removed
/// and its workspace is orphaned.
#[test]
fn test_wake_reconciles_unplugged_display() {
    let harness = TestHarness::new().with_display(
        EXT_DISPLAY_ID,
        IRect::new(0, -EXT_DISPLAY_HEIGHT, EXT_DISPLAY_WIDTH, 0),
        vec![EXT_WORKSPACE_ID],
    );

    // A window on the external display so its workspace strip actually exists.
    let ext_origin = Origin::new(0, -EXT_DISPLAY_HEIGHT + TEST_MENUBAR_HEIGHT);
    let size = Size::new(TEST_WINDOW_WIDTH, TEST_WINDOW_HEIGHT);
    let ext_frame = IRect::from_corners(ext_origin, ext_origin + size);
    harness
        .mock_state
        .spawn_window(TEST_PROCESS_ID, EXT_WORKSPACE_ID, 100, ext_frame);

    let commands = vec![
        Event::MenuOpened { window_id: 100 },
        Event::Command {
            command: Command::PrintState,
        },
        Event::SystemWoke { msg: String::new() },
    ];

    harness
        .on_iteration(1, |world, state| {
            let displays = world
                .query_filtered::<Entity, With<Display>>()
                .iter(world)
                .count();
            assert_eq!(displays, 2, "should start with two displays");

            // Unplug the external display behind paneru's back — no
            // DisplayRemoved event is sent, mimicking a wake-from-sleep.
            state.remove_display(EXT_DISPLAY_ID);
        })
        .on_iteration(2, |world, _state| {
            let displays = world
                .query_filtered::<Entity, With<Display>>()
                .iter(world)
                .count();
            assert_eq!(displays, 1, "reconcile should despawn the vanished display");

            // The external display's workspace must be orphaned, not lost.
            let orphan = world
                .query::<(&LayoutStrip, Option<&ChildOf>, Has<Timeout>)>()
                .iter(world)
                .find(|(strip, _, _)| strip.id() == EXT_WORKSPACE_ID)
                .map(|(_, child, timeout)| (child.is_some(), timeout));
            let (has_parent, has_timeout) =
                orphan.expect("external workspace strip should still exist");
            assert!(!has_parent, "orphaned workspace should have no parent");
            assert!(has_timeout, "orphaned workspace should carry a timeout");
        })
        .run(commands);
}

/// Even when the display set is unchanged, waking from sleep must force the
/// active workspace to re-tile, because macOS relocates window frames across a
/// sleep/wake cycle.
#[test]
fn test_wake_refreshes_active_workspace() {
    let harness = TestHarness::new().with_windows(1);

    let commands = vec![
        Event::MenuOpened { window_id: 0 },
        Event::SystemWoke { msg: String::new() },
    ];

    harness
        .on_iteration(1, |world, _state| {
            let refreshed = world
                .query_filtered::<Has<RefreshWindowSizes>, With<ActiveWorkspaceMarker>>()
                .iter(world)
                .any(|has| has);
            assert!(
                refreshed,
                "wake should mark the active workspace for a window-size refresh"
            );
        })
        .run(commands);
}

#[test]
fn test_vertical_swap_within_stack_stays_on_display() {
    // Regression test: with a display arranged *below* the active one, a
    // `Swap(South)` inside a stack used to swap the two windows and then
    // immediately send the focused one to the display below, because the
    // "is there anything left to swap with?" check ran against the layout
    // after the swap had already happened.
    let mut harness = TestHarness::new().with_windows(2);
    harness.mock_state.add_display(
        EXT_DISPLAY_ID,
        IRect::new(
            0,
            TEST_DISPLAY_HEIGHT,
            EXT_DISPLAY_WIDTH,
            TEST_DISPLAY_HEIGHT + EXT_DISPLAY_HEIGHT,
        ),
        vec![EXT_WORKSPACE_ID],
    );

    let commands = vec![
        Event::MenuOpened { window_id: 0 },
        Event::Command {
            command: Command::Window(Operation::Focus(Direction::Last)),
        },
        Event::Command {
            command: Command::Window(Operation::Stack(true)),
        },
        Event::Command {
            command: Command::Window(Operation::Focus(Direction::North)),
        },
        Event::Command {
            command: Command::PrintState,
        },
        Event::Command {
            command: Command::Window(Operation::Swap(Direction::South)),
        },
        Event::Command {
            command: Command::PrintState,
        },
    ];

    harness
        .on_iteration(4, |world, state| {
            assert_on_workspace!(world, 0, TEST_WORKSPACE_ID);
            assert_on_workspace!(world, 1, TEST_WORKSPACE_ID);
            assert_eq!(state.active_display(), TEST_DISPLAY_ID);
        })
        .on_iteration(6, |world, state| {
            assert_on_workspace!(world, 0, TEST_WORKSPACE_ID);
            assert_on_workspace!(world, 1, TEST_WORKSPACE_ID);
            assert_not_on_workspace!(world, 0, EXT_WORKSPACE_ID);
            assert_not_on_workspace!(world, 1, EXT_WORKSPACE_ID);
            assert_eq!(
                state.active_display(),
                TEST_DISPLAY_ID,
                "swapping inside a stack must not move focus to another display"
            );
        })
        .run(commands);
}

#[test]
fn test_hidden_stack_stays_off_the_display_below() {
    // Regression test: hiding a virtual workspace parks its strip at the
    // display's bottom-right corner. Windows below the strip origin - the
    // lower members of a stack - used to land past the bottom edge entirely,
    // inside the display underneath, which macOS then adopts them onto. The
    // window came back on the wrong display once the workspace was shown
    // again.
    let mut harness = TestHarness::new().with_windows(2);
    harness.mock_state.add_display(
        EXT_DISPLAY_ID,
        IRect::new(
            0,
            TEST_DISPLAY_HEIGHT,
            EXT_DISPLAY_WIDTH,
            TEST_DISPLAY_HEIGHT + EXT_DISPLAY_HEIGHT,
        ),
        vec![EXT_WORKSPACE_ID],
    );

    let commands = vec![
        Event::MenuOpened { window_id: 0 },
        Event::Command {
            command: Command::Window(Operation::Focus(Direction::Last)),
        },
        Event::Command {
            command: Command::Window(Operation::Stack(true)),
        },
        Event::Command {
            command: Command::Window(Operation::VirtualNumber(1)),
        },
        Event::Command {
            command: Command::PrintState,
        },
        Event::Command {
            command: Command::Window(Operation::VirtualNumber(0)),
        },
        Event::Command {
            command: Command::PrintState,
        },
    ];

    harness
        .on_iteration(4, |world, _state| {
            // Hidden, but still parked on their own display: every window keeps
            // its origin above the top edge of the display below.
            let mut query = world.query::<&crate::manager::Window>();
            for window in query.iter(world) {
                let frame = window.frame();
                assert!(
                    frame.min.y < TEST_DISPLAY_HEIGHT,
                    "window {} parked at {:?}, inside the display below",
                    window.id(),
                    frame
                );
                assert_eq!(
                    frame.min.y,
                    TEST_DISPLAY_HEIGHT - PARKED_STRIP_SLIVER,
                    "window {} should park on the corner sliver",
                    window.id()
                );
            }
        })
        .on_iteration(6, |world, _state| {
            assert_on_workspace!(world, 0, TEST_WORKSPACE_ID);
            assert_on_workspace!(world, 1, TEST_WORKSPACE_ID);
            assert_not_on_workspace!(world, 0, EXT_WORKSPACE_ID);
            assert_not_on_workspace!(world, 1, EXT_WORKSPACE_ID);
            assert_window_at!(world, 0, 400, TEST_MENUBAR_HEIGHT);
            assert_window_at!(world, 1, 400, 394);
        })
        .run(commands);
}

/// Focusing a window on another display and coming back must not re-derive the
/// strip offset on the display we left: the centering the user asked for there
/// is still what they want to see when they return.
#[test]
fn test_center_survives_display_round_trip() {
    let config: Config = (
        MainOptions {
            auto_center: Some(false),
            continuous_swipe: Some(false),
            animation_speed: Some(10000.0),
            ..Default::default()
        },
        vec![],
    )
        .into();

    let centered = (TEST_DISPLAY_WIDTH - TEST_WINDOW_WIDTH) / 2;
    let window_x = |world: &mut World, id: WinID| -> i32 {
        let mut query = world.query::<&Window>();
        query
            .iter(world)
            .find(|window| window.id() == id)
            .expect("window not found")
            .frame()
            .min
            .x
    };

    let commands = vec![
        // 0: boot with focus on window 0.
        Event::MenuOpened { window_id: 0 },
        // 1: center it on the main display.
        Event::Command {
            command: Command::Window(Operation::Center),
        },
        // 2: focus moves to the window on the external display.
        Event::Command {
            command: Command::PrintState,
        },
        // 3: and back to window 0.
        Event::Command {
            command: Command::PrintState,
        },
    ];

    TestHarness::new()
        .with_config(config)
        .with_display(
            EXT_DISPLAY_ID,
            IRect::new(0, -EXT_DISPLAY_HEIGHT, EXT_DISPLAY_WIDTH, 0),
            vec![EXT_WORKSPACE_ID],
        )
        .with_windows(4)
        .with_workspace_window(100, EXT_WORKSPACE_ID, |window| {
            window.workspace_id = EXT_WORKSPACE_ID;
        })
        .on_iteration(1, move |world, state| {
            assert_eq!(window_x(world, 0), centered, "window 0 must be centered");
            state.focus_window(100);
        })
        .on_iteration(2, move |_world, state| {
            state.focus_window(0);
        })
        .on_iteration(3, move |world, _state| {
            assert_eq!(
                window_x(world, 0),
                centered,
                "returning from another display must not undo the centering"
            );
        })
        .run(commands);
}

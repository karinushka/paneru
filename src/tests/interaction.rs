use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use bevy::prelude::*;
use stdext::prelude::RwLockExt;

use crate::commands::{Command, Direction, MoveFocus, Operation};
use crate::config::{Config, MainOptions, WindowParams};
use crate::ecs::Bounds;
use crate::ecs::SpawnWindowTrigger;
use crate::ecs::{ActiveWorkspaceMarker, Position, layout::LayoutStrip};
use crate::events::Event;
use crate::manager::{NativeTabDirection, Origin, Size, Window};
use crate::{assert_focused, assert_window_at, assert_window_size};

use super::*;

fn harness_with_one_window() -> (TestHarness, MockApplication) {
    let mut harness = TestHarness::new();
    let app = setup_process(harness.app.world_mut());
    let initial_app = app.clone();
    let initial_queue = harness.internal_queue.clone();
    let wm = MockWindowManager {
        windows: Box::new(move |_| {
            let origin = Origin::new(0, 0);
            let size = Size::new(TEST_WINDOW_WIDTH, TEST_WINDOW_HEIGHT);
            let window = MockWindow::new(
                0,
                IRect {
                    min: origin,
                    max: origin + size,
                },
                initial_queue.clone(),
                initial_app.clone(),
            );
            vec![Window::new(Box::new(window))]
        }),
        workspaces: vec![TEST_WORKSPACE_ID],
        visible_windows: HashMap::new(),
    };

    (harness.with_wm(wm), app)
}

fn spawn_matching_native_tab(
    world: &mut World,
    window_id: i32,
    event_queue: EventQueue,
    app: MockApplication,
) {
    let origin = Origin::new(0, TEST_MENUBAR_HEIGHT);
    let size = Size::new(TEST_WINDOW_WIDTH, TEST_DISPLAY_HEIGHT - TEST_MENUBAR_HEIGHT);
    let window = MockWindow::new(
        window_id,
        IRect {
            min: origin,
            max: origin + size,
        },
        event_queue,
        app,
    )
    .with_native_tab_count(2);
    world.trigger(SpawnWindowTrigger(vec![Window::new(Box::new(window))]));
}

fn spawn_native_window(
    world: &mut World,
    window_id: i32,
    origin: Origin,
    size: Size,
    event_queue: EventQueue,
    app: MockApplication,
) {
    let window = MockWindow::new(
        window_id,
        IRect {
            min: origin,
            max: origin + size,
        },
        event_queue,
        app,
    );
    world.trigger(SpawnWindowTrigger(vec![Window::new(Box::new(window))]));
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

    let mut harness = TestHarness::new().with_config(config).with_windows(3);

    let app = setup_process(harness.app.world_mut());
    let internal_queue = harness.internal_queue.clone();

    harness
        .on_iteration(1, move |world| {
            let origin = Origin::new(0, 0);
            let size = Size::new(TEST_WINDOW_WIDTH, TEST_WINDOW_HEIGHT);
            let window = MockWindow::new(
                3,
                IRect {
                    min: origin,
                    max: origin + size,
                },
                internal_queue.clone(),
                app.clone(),
            );
            let window = Window::new(Box::new(window));
            world.trigger(SpawnWindowTrigger(vec![window]));
        })
        .on_iteration(3, move |world| {
            assert_window_at!(world, 2, 0, TEST_MENUBAR_HEIGHT);
            assert_window_at!(world, 1, 400, TEST_MENUBAR_HEIGHT);
            assert_window_at!(world, 0, 800, TEST_MENUBAR_HEIGHT);
            assert_window_at!(world, 3, offscreen_right, TEST_MENUBAR_HEIGHT);
            assert_focused!(world, 2);
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
        .on_iteration(1, move |world| {
            assert_window_size!(world, 4, TEST_WINDOW_WIDTH, expected_height);
            assert_window_size!(world, 3, TEST_WINDOW_WIDTH, expected_height);
            assert_window_size!(world, 2, TEST_WINDOW_WIDTH, expected_height);
            assert_window_size!(world, 1, TEST_WINDOW_WIDTH, expected_height);
            assert_window_size!(world, 0, TEST_WINDOW_WIDTH, expected_height);
        })
        .run(commands);
}

#[test]
fn test_same_app_same_frame_native_tab_reuses_existing_column() {
    let commands = vec![
        Event::MenuOpened { window_id: 0 },
        Event::Command {
            command: Command::PrintState,
        },
    ];

    let (harness, app) = harness_with_one_window();
    let internal_queue = harness.internal_queue.clone();

    harness
        .on_iteration(0, move |world| {
            spawn_matching_native_tab(world, 1, internal_queue.clone(), app.clone());
        })
        .on_iteration(1, move |world| {
            let tab_zero = find_window_entity(0, world);
            let tab_one = find_window_entity(1, world);
            let mut query = world.query::<(&LayoutStrip, Has<ActiveWorkspaceMarker>)>();
            let strip = query
                .iter(world)
                .find_map(|(strip, active)| active.then_some(strip))
                .expect("active strip not found");

            assert_eq!(strip.len(), 1, "native tab should not add a column");
            assert_eq!(strip.tab_group(tab_zero), Some(vec![tab_zero, tab_one]));
            assert_eq!(strip.tab_group(tab_one), Some(vec![tab_zero, tab_one]));
            assert_window_size!(
                world,
                0,
                TEST_WINDOW_WIDTH,
                TEST_DISPLAY_HEIGHT - TEST_MENUBAR_HEIGHT
            );
            assert_window_size!(
                world,
                1,
                TEST_WINDOW_WIDTH,
                TEST_DISPLAY_HEIGHT - TEST_MENUBAR_HEIGHT
            );
        })
        .run(commands);
}

#[test]
fn test_native_tab_resize_syncs_sibling_size() {
    let commands = vec![
        Event::MenuOpened { window_id: 0 },
        Event::Command {
            command: Command::PrintState,
        },
        Event::Command {
            command: Command::PrintState,
        },
    ];

    let (harness, app) = harness_with_one_window();
    let internal_queue = harness.internal_queue.clone();

    harness
        .on_iteration(0, move |world| {
            spawn_matching_native_tab(world, 1, internal_queue.clone(), app.clone());
        })
        .on_iteration(1, move |world| {
            let tab_one = find_window_entity(1, world);
            let mut query = world.query::<&mut Bounds>();
            let mut bounds = query.get_mut(world, tab_one).expect("tab bounds missing");
            bounds.0.x = TEST_WINDOW_WIDTH + 160;
        })
        .on_iteration(2, move |world| {
            assert_window_size!(
                world,
                0,
                TEST_WINDOW_WIDTH + 160,
                TEST_DISPLAY_HEIGHT - TEST_MENUBAR_HEIGHT
            );
            assert_window_size!(
                world,
                1,
                TEST_WINDOW_WIDTH + 160,
                TEST_DISPLAY_HEIGHT - TEST_MENUBAR_HEIGHT
            );
        })
        .run(commands);
}

#[test]
fn test_native_tab_focus_east_switches_tabs_inside_group() {
    let commands = vec![
        Event::MenuOpened { window_id: 0 },
        Event::Command {
            command: Command::PrintState,
        },
        Event::Command {
            command: Command::Window(Operation::Focus(Direction::East)),
        },
    ];

    let (harness, app) = harness_with_one_window();
    let focused_app = app.clone();
    let internal_queue = harness.internal_queue.clone();

    harness
        .on_iteration(0, move |world| {
            spawn_matching_native_tab(world, 1, internal_queue.clone(), app.clone());
        })
        .on_iteration(2, move |world| {
            let tab_zero = find_window_entity(0, world);
            let tab_one = find_window_entity(1, world);
            let mut query = world.query::<(&LayoutStrip, Has<ActiveWorkspaceMarker>)>();
            let strip = query
                .iter(world)
                .find_map(|(strip, active)| active.then_some(strip))
                .expect("active strip not found");

            assert_eq!(strip.len(), 1);
            assert_eq!(strip.index_of(tab_zero).unwrap(), 0);
            assert_eq!(strip.index_of(tab_one).unwrap(), 0);
            assert_focused!(world, 1);
            assert_eq!(focused_app.inner.force_read().focused_id, Some(1));
            assert_eq!(
                focused_app.inner.force_read().native_tab_selections,
                vec![(NativeTabDirection::Next, 1)]
            );
        })
        .run(commands);
}

#[test]
fn test_native_tab_focus_east_at_last_tab_moves_to_neighbor_column() {
    let commands = vec![
        Event::MenuOpened { window_id: 0 },
        Event::Command {
            command: Command::PrintState,
        },
        Event::Command {
            command: Command::Window(Operation::Focus(Direction::East)),
        },
        Event::Command {
            command: Command::Window(Operation::Focus(Direction::East)),
        },
    ];

    let (harness, app) = harness_with_one_window();
    let internal_queue = harness.internal_queue.clone();

    harness
        .on_iteration(0, move |world| {
            spawn_matching_native_tab(world, 1, internal_queue.clone(), app.clone());
            spawn_native_window(
                world,
                2,
                Origin::new(TEST_WINDOW_WIDTH, TEST_MENUBAR_HEIGHT),
                Size::new(TEST_WINDOW_WIDTH, TEST_DISPLAY_HEIGHT - TEST_MENUBAR_HEIGHT),
                internal_queue.clone(),
                app.clone(),
            );
        })
        .on_iteration(3, move |world| {
            let tab_zero = find_window_entity(0, world);
            let tab_one = find_window_entity(1, world);
            let other = find_window_entity(2, world);
            let mut query = world.query::<(&LayoutStrip, Has<ActiveWorkspaceMarker>)>();
            let strip = query
                .iter(world)
                .find_map(|(strip, active)| active.then_some(strip))
                .expect("active strip not found");

            assert_eq!(strip.index_of(tab_zero).unwrap(), 0);
            assert_eq!(strip.index_of(tab_one).unwrap(), 0);
            assert_eq!(strip.index_of(other).unwrap(), 1);
            assert_focused!(world, 2);
        })
        .run(commands);
}

#[test]
fn test_native_tab_swap_east_switches_tabs_inside_group() {
    let commands = vec![
        Event::MenuOpened { window_id: 0 },
        Event::Command {
            command: Command::PrintState,
        },
        Event::Command {
            command: Command::Window(Operation::Swap(Direction::East)),
        },
    ];

    let (harness, app) = harness_with_one_window();
    let internal_queue = harness.internal_queue.clone();

    harness
        .on_iteration(0, move |world| {
            spawn_matching_native_tab(world, 1, internal_queue.clone(), app.clone());
            spawn_native_window(
                world,
                2,
                Origin::new(TEST_WINDOW_WIDTH, TEST_MENUBAR_HEIGHT),
                Size::new(TEST_WINDOW_WIDTH, TEST_DISPLAY_HEIGHT - TEST_MENUBAR_HEIGHT),
                internal_queue.clone(),
                app.clone(),
            );
        })
        .on_iteration(2, move |world| {
            let tab_zero = find_window_entity(0, world);
            let tab_one = find_window_entity(1, world);
            let other = find_window_entity(2, world);
            let mut query = world.query::<(&LayoutStrip, Has<ActiveWorkspaceMarker>)>();
            let strip = query
                .iter(world)
                .find_map(|(strip, active)| active.then_some(strip))
                .expect("active strip not found");

            assert_eq!(strip.index_of(tab_zero).unwrap(), 0);
            assert_eq!(strip.index_of(tab_one).unwrap(), 0);
            assert_eq!(strip.index_of(other).unwrap(), 1);
            assert_focused!(world, 1);
        })
        .run(commands);
}

#[test]
fn test_native_tab_swap_east_at_last_tab_moves_whole_column() {
    let commands = vec![
        Event::MenuOpened { window_id: 0 },
        Event::Command {
            command: Command::PrintState,
        },
        Event::Command {
            command: Command::Window(Operation::Focus(Direction::East)),
        },
        Event::Command {
            command: Command::Window(Operation::Swap(Direction::East)),
        },
    ];

    let (harness, app) = harness_with_one_window();
    let internal_queue = harness.internal_queue.clone();

    harness
        .on_iteration(0, move |world| {
            spawn_matching_native_tab(world, 1, internal_queue.clone(), app.clone());
            spawn_native_window(
                world,
                2,
                Origin::new(TEST_WINDOW_WIDTH, TEST_MENUBAR_HEIGHT),
                Size::new(TEST_WINDOW_WIDTH, TEST_DISPLAY_HEIGHT - TEST_MENUBAR_HEIGHT),
                internal_queue.clone(),
                app.clone(),
            );
        })
        .on_iteration(3, move |world| {
            let tab_zero = find_window_entity(0, world);
            let tab_one = find_window_entity(1, world);
            let other = find_window_entity(2, world);
            let mut query = world.query::<(&LayoutStrip, Has<ActiveWorkspaceMarker>)>();
            let strip = query
                .iter(world)
                .find_map(|(strip, active)| active.then_some(strip))
                .expect("active strip not found");

            assert_eq!(strip.index_of(tab_zero).unwrap(), 1);
            assert_eq!(strip.index_of(tab_one).unwrap(), 1);
            assert_eq!(strip.index_of(other).unwrap(), 0);
            assert_focused!(world, 1);
        })
        .run(commands);
}

#[test]
fn test_native_tab_virtual_move_moves_all_tabs() {
    let commands = vec![
        Event::MenuOpened { window_id: 0 },
        Event::Command {
            command: Command::PrintState,
        },
        Event::Command {
            command: Command::Window(Operation::VirtualMoveNumber(1, MoveFocus::Follow)),
        },
        Event::Command {
            command: Command::PrintState,
        },
    ];

    let (harness, app) = harness_with_one_window();
    let internal_queue = harness.internal_queue.clone();

    harness
        .on_iteration(0, move |world| {
            spawn_matching_native_tab(world, 1, internal_queue.clone(), app.clone());
        })
        .on_iteration(3, move |world| {
            let tab_zero = find_window_entity(0, world);
            let tab_one = find_window_entity(1, world);
            let mut query = world.query::<(&LayoutStrip, Has<ActiveWorkspaceMarker>)>();
            let active = query
                .iter(world)
                .find_map(|(strip, active)| active.then_some(strip))
                .expect("active strip not found");

            assert_eq!(active.virtual_index, 1);
            assert_eq!(active.len(), 1);
            assert_eq!(active.index_of(tab_zero).unwrap(), 0);
            assert_eq!(active.index_of(tab_one).unwrap(), 0);
            assert_eq!(active.tab_group(tab_zero), Some(vec![tab_zero, tab_one]));

            let mut query = world.query::<&LayoutStrip>();
            let source = query
                .iter(world)
                .find(|strip| strip.virtual_index == 0)
                .expect("source strip not found");
            assert!(!source.contains(tab_zero));
            assert!(!source.contains(tab_one));
        })
        .run(commands);
}

#[test]
fn test_native_tab_removal_keeps_remaining_window_column() {
    let commands = vec![
        Event::MenuOpened { window_id: 0 },
        Event::Command {
            command: Command::PrintState,
        },
        Event::Command {
            command: Command::PrintState,
        },
    ];

    let (harness, app) = harness_with_one_window();
    let internal_queue = harness.internal_queue.clone();

    harness
        .on_iteration(0, move |world| {
            spawn_matching_native_tab(world, 1, internal_queue.clone(), app.clone());
        })
        .on_iteration(1, move |world| {
            let tab_one = find_window_entity(1, world);
            world.entity_mut(tab_one).despawn();
        })
        .on_iteration(2, move |world| {
            let tab_zero = find_window_entity(0, world);
            let mut query = world.query::<(&LayoutStrip, Has<ActiveWorkspaceMarker>)>();
            let strip = query
                .iter(world)
                .find_map(|(strip, active)| active.then_some(strip))
                .expect("active strip not found");

            assert_eq!(strip.len(), 1, "removing a tab should not empty the column");
            assert_eq!(strip.tab_group(tab_zero), None);
            assert_eq!(strip.index_of(tab_zero).unwrap(), 0);
        })
        .run(commands);
}

fn spawn_native_window_tab(
    world: &mut World,
    window_id: i32,
    origin: Origin,
    size: Size,
    event_queue: EventQueue,
    app: MockApplication,
) {
    let window = MockWindow::new(
        window_id,
        IRect {
            min: origin,
            max: origin + size,
        },
        event_queue,
        app,
    )
    .with_native_tab_count(2);
    world.trigger(SpawnWindowTrigger(vec![Window::new(Box::new(window))]));
}

fn spawn_native_window_tab_count_ref(
    world: &mut World,
    window_id: i32,
    event_queue: EventQueue,
    app: MockApplication,
    native_tab_count: Arc<AtomicUsize>,
) {
    let origin = Origin::new(0, TEST_MENUBAR_HEIGHT);
    let size = Size::new(TEST_WINDOW_WIDTH, TEST_DISPLAY_HEIGHT - TEST_MENUBAR_HEIGHT);
    let window = MockWindow::new(
        window_id,
        IRect {
            min: origin,
            max: origin + size,
        },
        event_queue,
        app,
    )
    .with_native_tab_count_ref(native_tab_count);
    world.trigger(SpawnWindowTrigger(vec![Window::new(Box::new(window))]));
}

#[test]
fn test_same_app_distinct_windows_keep_separate_native_tab_groups() {
    let commands = vec![
        Event::MenuOpened { window_id: 0 },
        Event::Command {
            command: Command::PrintState,
        },
    ];

    let (harness, app) = harness_with_one_window();
    let internal_queue = harness.internal_queue.clone();

    harness
        .on_iteration(0, move |world| {
            spawn_matching_native_tab(world, 1, internal_queue.clone(), app.clone());
            spawn_native_window(
                world,
                2,
                Origin::new(TEST_WINDOW_WIDTH, TEST_MENUBAR_HEIGHT),
                Size::new(TEST_WINDOW_WIDTH, TEST_DISPLAY_HEIGHT - TEST_MENUBAR_HEIGHT),
                internal_queue.clone(),
                app.clone(),
            );
            spawn_native_window_tab(
                world,
                3,
                Origin::new(TEST_WINDOW_WIDTH, TEST_MENUBAR_HEIGHT),
                Size::new(TEST_WINDOW_WIDTH, TEST_DISPLAY_HEIGHT - TEST_MENUBAR_HEIGHT),
                internal_queue.clone(),
                app.clone(),
            );
        })
        .on_iteration(1, move |world| {
            let tab_zero = find_window_entity(0, world);
            let tab_one = find_window_entity(1, world);
            let tab_two = find_window_entity(2, world);
            let tab_three = find_window_entity(3, world);
            let mut query = world.query::<(&LayoutStrip, Has<ActiveWorkspaceMarker>)>();
            let strip = query
                .iter(world)
                .find_map(|(strip, active)| active.then_some(strip))
                .expect("active strip not found");

            assert_eq!(strip.len(), 2);
            assert_eq!(strip.index_of(tab_zero).unwrap(), 0);
            assert_eq!(strip.index_of(tab_one).unwrap(), 0);
            assert_eq!(strip.index_of(tab_two).unwrap(), 1);
            assert_eq!(strip.index_of(tab_three).unwrap(), 1);
            assert_eq!(strip.tab_group(tab_zero), Some(vec![tab_zero, tab_one]));
            assert_eq!(strip.tab_group(tab_two), Some(vec![tab_two, tab_three]));
        })
        .run(commands);
}

#[test]
fn test_same_app_overlapping_second_window_stays_separate_without_native_tab_signal() {
    let commands = vec![
        Event::MenuOpened { window_id: 0 },
        Event::Command {
            command: Command::PrintState,
        },
    ];

    let (harness, app) = harness_with_one_window();
    let internal_queue = harness.internal_queue.clone();

    harness
        .on_iteration(0, move |world| {
            spawn_native_window(
                world,
                1,
                Origin::new(0, TEST_MENUBAR_HEIGHT),
                Size::new(TEST_WINDOW_WIDTH, TEST_DISPLAY_HEIGHT - TEST_MENUBAR_HEIGHT),
                internal_queue.clone(),
                app.clone(),
            );
        })
        .on_iteration(1, move |world| {
            let first = find_window_entity(0, world);
            let second = find_window_entity(1, world);
            let mut query = world.query::<(&LayoutStrip, Has<ActiveWorkspaceMarker>)>();
            let strip = query
                .iter(world)
                .find_map(|(strip, active)| active.then_some(strip))
                .expect("active strip not found");

            assert_eq!(strip.len(), 2);
            assert_eq!(strip.index_of(first).unwrap(), 0);
            assert_eq!(strip.index_of(second).unwrap(), 1);
            assert_eq!(strip.tab_group(first), None);
            assert_eq!(strip.tab_group(second), None);
        })
        .run(commands);
}

#[test]
fn test_runtime_native_tab_groups_after_delayed_tab_signal() {
    let commands = vec![
        Event::MenuOpened { window_id: 0 },
        Event::Command {
            command: Command::PrintState,
        },
        Event::Command {
            command: Command::PrintState,
        },
    ];

    let (harness, app) = harness_with_one_window();
    let internal_queue = harness.internal_queue.clone();
    let native_tab_count = Arc::new(AtomicUsize::new(1));
    let count_for_spawn = native_tab_count.clone();
    let count_for_update = native_tab_count.clone();

    harness
        .on_iteration(0, move |world| {
            spawn_native_window_tab_count_ref(
                world,
                1,
                internal_queue.clone(),
                app.clone(),
                count_for_spawn.clone(),
            );
        })
        .on_iteration(1, move |_| {
            count_for_update.store(2, Ordering::Relaxed);
        })
        .on_iteration(2, move |world| {
            let tab_zero = find_window_entity(0, world);
            let tab_one = find_window_entity(1, world);
            let mut query = world.query::<(&LayoutStrip, Has<ActiveWorkspaceMarker>)>();
            let strip = query
                .iter(world)
                .find_map(|(strip, active)| active.then_some(strip))
                .expect("active strip not found");

            assert_eq!(strip.len(), 1);
            assert_eq!(strip.index_of(tab_zero).unwrap(), 0);
            assert_eq!(strip.index_of(tab_one).unwrap(), 0);
            assert_eq!(strip.tab_group(tab_zero), Some(vec![tab_zero, tab_one]));
        })
        .run(commands);
}

#[test]
fn test_runtime_native_tab_groups_when_previous_tab_disappears_from_screen() {
    let commands = vec![
        Event::MenuOpened { window_id: 0 },
        Event::Command {
            command: Command::PrintState,
        },
    ];

    let mut harness = TestHarness::new();
    let app = setup_process(harness.app.world_mut());
    let initial_app = app.clone();
    let initial_queue = harness.internal_queue.clone();
    let wm = MockWindowManager {
        windows: Box::new(move |_| {
            let origin = Origin::new(0, 0);
            let size = Size::new(TEST_WINDOW_WIDTH, TEST_WINDOW_HEIGHT);
            let window = MockWindow::new(
                0,
                IRect {
                    min: origin,
                    max: origin + size,
                },
                initial_queue.clone(),
                initial_app.clone(),
            );
            vec![Window::new(Box::new(window))]
        }),
        workspaces: vec![TEST_WORKSPACE_ID],
        visible_windows: HashMap::from([(0, vec![1])]),
    };
    let harness = harness.with_wm(wm);
    let internal_queue = harness.internal_queue.clone();

    harness
        .on_iteration(0, move |world| {
            spawn_native_window(
                world,
                1,
                Origin::new(0, TEST_MENUBAR_HEIGHT),
                Size::new(TEST_WINDOW_WIDTH, TEST_DISPLAY_HEIGHT - TEST_MENUBAR_HEIGHT),
                internal_queue.clone(),
                app.clone(),
            );
        })
        .on_iteration(1, move |world| {
            let tab_zero = find_window_entity(0, world);
            let tab_one = find_window_entity(1, world);
            let mut query = world.query::<(&LayoutStrip, Has<ActiveWorkspaceMarker>)>();
            let strip = query
                .iter(world)
                .find_map(|(strip, active)| active.then_some(strip))
                .expect("active strip not found");

            assert_eq!(strip.len(), 1);
            assert_eq!(strip.index_of(tab_zero).unwrap(), 0);
            assert_eq!(strip.index_of(tab_one).unwrap(), 0);
            assert_eq!(strip.tab_group(tab_zero), Some(vec![tab_zero, tab_one]));
        })
        .run(commands);
}

#[test]
fn test_runtime_native_tab_repair_preserves_existing_group_order() {
    let commands = vec![
        Event::MenuOpened { window_id: 0 },
        Event::Command {
            command: Command::PrintState,
        },
        Event::Command {
            command: Command::Window(Operation::Focus(Direction::East)),
        },
        Event::Command {
            command: Command::Window(Operation::Focus(Direction::East)),
        },
    ];

    let mut harness = TestHarness::new();
    let app = setup_process(harness.app.world_mut());
    let initial_app = app.clone();
    let focused_app = app.clone();
    let asserted_app = app.clone();
    let initial_queue = harness.internal_queue.clone();
    let wm = MockWindowManager {
        windows: Box::new(move |_| {
            let origin = Origin::new(0, 0);
            let size = Size::new(TEST_WINDOW_WIDTH, TEST_WINDOW_HEIGHT);
            let window = MockWindow::new(
                0,
                IRect {
                    min: origin,
                    max: origin + size,
                },
                initial_queue.clone(),
                initial_app.clone(),
            );
            vec![Window::new(Box::new(window))]
        }),
        workspaces: vec![TEST_WORKSPACE_ID],
        visible_windows: HashMap::from([(0, vec![2])]),
    };
    let harness = harness.with_wm(wm);
    let internal_queue = harness.internal_queue.clone();

    harness
        .on_iteration(0, move |world| {
            spawn_matching_native_tab(world, 1, internal_queue.clone(), app.clone());
            spawn_native_window(
                world,
                2,
                Origin::new(0, TEST_MENUBAR_HEIGHT),
                Size::new(TEST_WINDOW_WIDTH, TEST_DISPLAY_HEIGHT - TEST_MENUBAR_HEIGHT),
                internal_queue.clone(),
                app.clone(),
            );
            focused_app.inner.force_write().focused_id = Some(0);
        })
        .on_iteration(3, move |world| {
            let tab_zero = find_window_entity(0, world);
            let tab_one = find_window_entity(1, world);
            let tab_two = find_window_entity(2, world);
            let mut query = world.query::<(&LayoutStrip, Has<ActiveWorkspaceMarker>)>();
            let strip = query
                .iter(world)
                .find_map(|(strip, active)| active.then_some(strip))
                .expect("active strip not found");

            assert_eq!(strip.len(), 1);
            assert_eq!(
                strip.tab_group(tab_zero),
                Some(vec![tab_zero, tab_one, tab_two])
            );
            assert_focused!(world, 2);
            assert_eq!(
                asserted_app.inner.force_read().native_tab_selections,
                vec![(NativeTabDirection::Next, 1), (NativeTabDirection::Next, 2)]
            );
        })
        .run(commands);
}

#[test]
fn test_runtime_hidden_native_tab_joins_hidden_same_app_window() {
    let commands = vec![
        Event::MenuOpened { window_id: 0 },
        Event::Command {
            command: Command::PrintState,
        },
        Event::Command {
            command: Command::PrintState,
        },
    ];

    let mut harness = TestHarness::new();
    let app = setup_process(harness.app.world_mut());
    let initial_app = app.clone();
    let app_for_second_window = app.clone();
    let app_for_second_tab = app.clone();
    let focused_app = app.clone();
    let initial_queue = harness.internal_queue.clone();
    let wm = MockWindowManager {
        windows: Box::new(move |_| {
            let origin = Origin::new(0, 0);
            let size = Size::new(TEST_WINDOW_WIDTH, TEST_WINDOW_HEIGHT);
            let window = MockWindow::new(
                0,
                IRect {
                    min: origin,
                    max: origin + size,
                },
                initial_queue.clone(),
                initial_app.clone(),
            );
            vec![Window::new(Box::new(window))]
        }),
        workspaces: vec![TEST_WORKSPACE_ID],
        visible_windows: HashMap::from([(0, vec![0, 3])]),
    };
    let harness = harness.with_wm(wm);
    let second_queue = harness.internal_queue.clone();
    let tab_queue = harness.internal_queue.clone();

    harness
        .on_iteration(0, move |world| {
            spawn_native_window(
                world,
                2,
                Origin::new(TEST_WINDOW_WIDTH, TEST_MENUBAR_HEIGHT),
                Size::new(TEST_WINDOW_WIDTH, TEST_DISPLAY_HEIGHT - TEST_MENUBAR_HEIGHT),
                second_queue.clone(),
                app_for_second_window.clone(),
            );
            focused_app.inner.force_write().focused_id = Some(2);
        })
        .on_iteration(1, move |world| {
            spawn_native_window(
                world,
                3,
                Origin::new(TEST_WINDOW_WIDTH, TEST_MENUBAR_HEIGHT),
                Size::new(TEST_WINDOW_WIDTH, TEST_DISPLAY_HEIGHT - TEST_MENUBAR_HEIGHT),
                tab_queue.clone(),
                app_for_second_tab.clone(),
            );
        })
        .on_iteration(2, move |world| {
            let first_window = find_window_entity(0, world);
            let second_window = find_window_entity(2, world);
            let second_tab = find_window_entity(3, world);
            let mut query = world.query::<(&LayoutStrip, Has<ActiveWorkspaceMarker>)>();
            let strip = query
                .iter(world)
                .find_map(|(strip, active)| active.then_some(strip))
                .expect("active strip not found");

            assert_eq!(strip.len(), 2);
            assert_eq!(strip.index_of(first_window).unwrap(), 0);
            assert_eq!(strip.index_of(second_window).unwrap(), 1);
            assert_eq!(strip.index_of(second_tab).unwrap(), 1);
            assert_eq!(strip.tab_group(first_window), None);
            assert_eq!(
                strip.tab_group(second_window),
                Some(vec![second_window, second_tab])
            );
        })
        .run(commands);
}

#[test]
fn test_same_app_overlapping_visible_window_stays_separate() {
    let commands = vec![
        Event::MenuOpened { window_id: 0 },
        Event::Command {
            command: Command::PrintState,
        },
    ];

    let mut harness = TestHarness::new();
    let app = setup_process(harness.app.world_mut());
    let initial_app = app.clone();
    let initial_queue = harness.internal_queue.clone();
    let wm = MockWindowManager {
        windows: Box::new(move |_| {
            let origin = Origin::new(0, 0);
            let size = Size::new(TEST_WINDOW_WIDTH, TEST_WINDOW_HEIGHT);
            let window = MockWindow::new(
                0,
                IRect {
                    min: origin,
                    max: origin + size,
                },
                initial_queue.clone(),
                initial_app.clone(),
            );
            vec![Window::new(Box::new(window))]
        }),
        workspaces: vec![TEST_WORKSPACE_ID],
        visible_windows: HashMap::from([(0, vec![0, 1])]),
    };
    let harness = harness.with_wm(wm);
    let internal_queue = harness.internal_queue.clone();

    harness
        .on_iteration(0, move |world| {
            spawn_native_window(
                world,
                1,
                Origin::new(0, TEST_MENUBAR_HEIGHT),
                Size::new(TEST_WINDOW_WIDTH, TEST_DISPLAY_HEIGHT - TEST_MENUBAR_HEIGHT),
                internal_queue.clone(),
                app.clone(),
            );
        })
        .on_iteration(1, move |world| {
            let first = find_window_entity(0, world);
            let second = find_window_entity(1, world);
            let mut query = world.query::<(&LayoutStrip, Has<ActiveWorkspaceMarker>)>();
            let strip = query
                .iter(world)
                .find_map(|(strip, active)| active.then_some(strip))
                .expect("active strip not found");

            assert_eq!(strip.len(), 2);
            assert_eq!(strip.index_of(first).unwrap(), 0);
            assert_eq!(strip.index_of(second).unwrap(), 1);
            assert_eq!(strip.tab_group(first), None);
            assert_eq!(strip.tab_group(second), None);
        })
        .run(commands);
}

#[test]
fn test_native_tab_focus_east_uses_app_active_tab_at_group_edge() {
    let commands = vec![
        Event::MenuOpened { window_id: 0 },
        Event::Command {
            command: Command::PrintState,
        },
        Event::Command {
            command: Command::Window(Operation::Focus(Direction::East)),
        },
    ];

    let (harness, app) = harness_with_one_window();
    let focused_app = app.clone();
    let internal_queue = harness.internal_queue.clone();

    harness
        .on_iteration(0, move |world| {
            spawn_matching_native_tab(world, 1, internal_queue.clone(), app.clone());
            spawn_native_window(
                world,
                2,
                Origin::new(TEST_WINDOW_WIDTH, TEST_MENUBAR_HEIGHT),
                Size::new(TEST_WINDOW_WIDTH, TEST_DISPLAY_HEIGHT - TEST_MENUBAR_HEIGHT),
                internal_queue.clone(),
                app.clone(),
            );
        })
        .on_iteration(1, move |_| {
            focused_app.inner.force_write().focused_id = Some(1);
        })
        .on_iteration(2, move |world| {
            let tab_zero = find_window_entity(0, world);
            let tab_one = find_window_entity(1, world);
            let other = find_window_entity(2, world);
            let mut query = world.query::<(&LayoutStrip, Has<ActiveWorkspaceMarker>)>();
            let strip = query
                .iter(world)
                .find_map(|(strip, active)| active.then_some(strip))
                .expect("active strip not found");

            assert_eq!(strip.index_of(tab_zero).unwrap(), 0);
            assert_eq!(strip.index_of(tab_one).unwrap(), 0);
            assert_eq!(strip.index_of(other).unwrap(), 1);
            assert_focused!(world, 2);
        })
        .run(commands);
}

#[test]
fn test_native_tab_focus_west_uses_app_active_tab_inside_group() {
    let commands = vec![
        Event::MenuOpened { window_id: 0 },
        Event::Command {
            command: Command::PrintState,
        },
        Event::Command {
            command: Command::Window(Operation::Focus(Direction::West)),
        },
    ];

    let (harness, app) = harness_with_one_window();
    let focused_app = app.clone();
    let asserted_app = app.clone();
    let internal_queue = harness.internal_queue.clone();

    harness
        .on_iteration(0, move |world| {
            spawn_matching_native_tab(world, 1, internal_queue.clone(), app.clone());
        })
        .on_iteration(1, move |_| {
            focused_app.inner.force_write().focused_id = Some(1);
        })
        .on_iteration(2, move |world| {
            let tab_zero = find_window_entity(0, world);
            let tab_one = find_window_entity(1, world);
            let mut query = world.query::<(&LayoutStrip, Has<ActiveWorkspaceMarker>)>();
            let strip = query
                .iter(world)
                .find_map(|(strip, active)| active.then_some(strip))
                .expect("active strip not found");

            assert_eq!(strip.len(), 1);
            assert_eq!(strip.index_of(tab_zero).unwrap(), 0);
            assert_eq!(strip.index_of(tab_one).unwrap(), 0);
            assert_focused!(world, 0);
            assert_eq!(asserted_app.inner.force_read().focused_id, Some(0));
        })
        .run(commands);
}

#[test]
fn test_native_tab_ignores_inactive_tab_frame_updates() {
    let commands = vec![
        Event::MenuOpened { window_id: 0 },
        Event::Command {
            command: Command::PrintState,
        },
        Event::WindowFocused { window_id: 1 },
        Event::Command {
            command: Command::PrintState,
        },
    ];

    let (harness, app) = harness_with_one_window();
    let focused_app = app.clone();
    let internal_queue = harness.internal_queue.clone();

    harness
        .on_iteration(0, move |world| {
            spawn_matching_native_tab(world, 1, internal_queue.clone(), app.clone());
        })
        .on_iteration(1, move |_| {
            focused_app.inner.force_write().focused_id = Some(1);
        })
        .on_iteration(2, move |world| {
            let tab_zero = find_window_entity(0, world);
            let stale_origin = Origin::new(TEST_WINDOW_WIDTH, TEST_MENUBAR_HEIGHT);
            world
                .entity_mut(tab_zero)
                .get_mut::<Position>()
                .expect("tab position not found")
                .0 = stale_origin;
        })
        .on_iteration(3, move |world| {
            let tab_zero = find_window_entity(0, world);
            let tab_one = find_window_entity(1, world);

            let tab_zero_position = world
                .entity(tab_zero)
                .get::<Position>()
                .expect("tab zero position not found")
                .0;
            let tab_one_position = world
                .entity(tab_one)
                .get::<Position>()
                .expect("tab one position not found")
                .0;
            let expected_origin = Origin::new(0, TEST_MENUBAR_HEIGHT);

            assert_eq!(tab_zero_position, expected_origin);
            assert_eq!(tab_one_position, expected_origin);
            assert_focused!(world, 1);
        })
        .run(commands);
}

#[test]
fn test_native_tab_focus_normalizes_existing_stale_tab_frame() {
    let commands = vec![
        Event::MenuOpened { window_id: 0 },
        Event::Command {
            command: Command::PrintState,
        },
        Event::WindowFocused { window_id: 1 },
        Event::Command {
            command: Command::PrintState,
        },
    ];

    let (harness, app) = harness_with_one_window();
    let focused_app = app.clone();
    let internal_queue = harness.internal_queue.clone();

    harness
        .on_iteration(0, move |world| {
            spawn_matching_native_tab(world, 1, internal_queue.clone(), app.clone());
        })
        .on_iteration(1, move |world| {
            let tab_zero = find_window_entity(0, world);
            let stale_origin = Origin::new(TEST_WINDOW_WIDTH, TEST_MENUBAR_HEIGHT);
            world
                .entity_mut(tab_zero)
                .get_mut::<Position>()
                .expect("tab position not found")
                .bypass_change_detection()
                .0 = stale_origin;
            focused_app.inner.force_write().focused_id = Some(1);
        })
        .on_iteration(3, move |world| {
            let tab_zero = find_window_entity(0, world);
            let tab_one = find_window_entity(1, world);
            let expected_origin = Origin::new(0, TEST_MENUBAR_HEIGHT);

            assert_eq!(
                world
                    .entity(tab_zero)
                    .get::<Position>()
                    .expect("tab zero position not found")
                    .0,
                expected_origin
            );
            assert_eq!(
                world
                    .entity(tab_one)
                    .get::<Position>()
                    .expect("tab one position not found")
                    .0,
                expected_origin
            );
            assert_focused!(world, 1);
        })
        .run(commands);
}

#[test]
fn test_native_tab_hidden_cycle_keeps_grouped_column() {
    let commands = vec![
        Event::MenuOpened { window_id: 0 },
        Event::Command {
            command: Command::PrintState,
        },
        Event::WindowMinimized { window_id: 0 },
        Event::WindowDeminimized { window_id: 0 },
        Event::Command {
            command: Command::PrintState,
        },
    ];

    let (harness, app) = harness_with_one_window();
    let internal_queue = harness.internal_queue.clone();

    harness
        .on_iteration(0, move |world| {
            spawn_matching_native_tab(world, 1, internal_queue.clone(), app.clone());
        })
        .on_iteration(4, move |world| {
            let tab_zero = find_window_entity(0, world);
            let tab_one = find_window_entity(1, world);
            let mut query = world.query::<(&LayoutStrip, Has<ActiveWorkspaceMarker>)>();
            let strip = query
                .iter(world)
                .find_map(|(strip, active)| active.then_some(strip))
                .expect("active strip not found");

            assert_eq!(strip.len(), 1);
            assert_eq!(strip.index_of(tab_zero).unwrap(), 0);
            assert_eq!(strip.index_of(tab_one).unwrap(), 0);
            assert_eq!(strip.tab_group(tab_zero), Some(vec![tab_zero, tab_one]));
        })
        .run(commands);
}

#[test]
fn test_native_tab_virtual_move_stay_keeps_remaining_tab_on_target_after_close() {
    let commands = vec![
        Event::MenuOpened { window_id: 0 },
        Event::Command {
            command: Command::PrintState,
        },
        Event::Command {
            command: Command::Window(Operation::VirtualMoveNumber(1, MoveFocus::Stay)),
        },
        Event::Command {
            command: Command::PrintState,
        },
    ];

    let (harness, app) = harness_with_one_window();
    let internal_queue = harness.internal_queue.clone();

    harness
        .on_iteration(0, move |world| {
            spawn_matching_native_tab(world, 1, internal_queue.clone(), app.clone());
            spawn_native_window(
                world,
                2,
                Origin::new(TEST_WINDOW_WIDTH, TEST_MENUBAR_HEIGHT),
                Size::new(TEST_WINDOW_WIDTH, TEST_DISPLAY_HEIGHT - TEST_MENUBAR_HEIGHT),
                internal_queue.clone(),
                app.clone(),
            );
        })
        .on_iteration(3, move |world| {
            let tab_zero = find_window_entity(0, world);
            let tab_one = find_window_entity(1, world);
            let other = find_window_entity(2, world);

            world.entity_mut(tab_one).despawn();

            let mut query = world.query::<(&LayoutStrip, Has<ActiveWorkspaceMarker>)>();
            let source = query
                .iter(world)
                .find_map(|(strip, active)| active.then_some(strip))
                .expect("active strip not found");
            assert_eq!(source.virtual_index, 0);
            assert!(source.contains(other));
            assert!(!source.contains(tab_zero));
            assert!(!source.contains(tab_one));

            let mut query = world.query::<&LayoutStrip>();
            let target = query
                .iter(world)
                .find(|strip| strip.virtual_index == 1)
                .expect("target strip not found");
            assert!(target.contains(tab_zero));
            assert!(!target.contains(tab_one));
            assert_eq!(target.index_of(tab_zero).unwrap(), 0);
        })
        .run(commands);
}

#[test]
fn test_native_tab_swap_keeps_remaining_tab_in_moved_column_after_close() {
    let commands = vec![
        Event::MenuOpened { window_id: 0 },
        Event::Command {
            command: Command::PrintState,
        },
        Event::Command {
            command: Command::Window(Operation::Focus(Direction::East)),
        },
        Event::Command {
            command: Command::Window(Operation::Swap(Direction::East)),
        },
        Event::Command {
            command: Command::PrintState,
        },
    ];

    let (harness, app) = harness_with_one_window();
    let internal_queue = harness.internal_queue.clone();

    harness
        .on_iteration(0, move |world| {
            spawn_matching_native_tab(world, 1, internal_queue.clone(), app.clone());
            spawn_native_window(
                world,
                2,
                Origin::new(TEST_WINDOW_WIDTH, TEST_MENUBAR_HEIGHT),
                Size::new(TEST_WINDOW_WIDTH, TEST_DISPLAY_HEIGHT - TEST_MENUBAR_HEIGHT),
                internal_queue.clone(),
                app.clone(),
            );
        })
        .on_iteration(4, move |world| {
            let tab_zero = find_window_entity(0, world);
            let tab_one = find_window_entity(1, world);
            let other = find_window_entity(2, world);

            world.entity_mut(tab_one).despawn();

            let mut query = world.query::<(&LayoutStrip, Has<ActiveWorkspaceMarker>)>();
            let strip = query
                .iter(world)
                .find_map(|(strip, active)| active.then_some(strip))
                .expect("active strip not found");
            assert_eq!(strip.index_of(other).unwrap(), 0);
            assert_eq!(strip.index_of(tab_zero).unwrap(), 1);
            assert!(!strip.contains(tab_one));
        })
        .run(commands);
}

#[test]
fn test_native_tab_stack_keeps_remaining_tab_in_moved_column_after_close() {
    let commands = vec![
        Event::MenuOpened { window_id: 0 },
        Event::Command {
            command: Command::PrintState,
        },
        Event::Command {
            command: Command::Window(Operation::Focus(Direction::East)),
        },
        Event::Command {
            command: Command::Window(Operation::Focus(Direction::East)),
        },
        Event::Command {
            command: Command::Window(Operation::Stack(true)),
        },
        Event::Command {
            command: Command::PrintState,
        },
    ];

    let (harness, app) = harness_with_one_window();
    let internal_queue = harness.internal_queue.clone();

    harness
        .on_iteration(0, move |world| {
            spawn_matching_native_tab(world, 1, internal_queue.clone(), app.clone());
            spawn_native_window(
                world,
                2,
                Origin::new(TEST_WINDOW_WIDTH, TEST_MENUBAR_HEIGHT),
                Size::new(TEST_WINDOW_WIDTH, TEST_DISPLAY_HEIGHT - TEST_MENUBAR_HEIGHT),
                internal_queue.clone(),
                app.clone(),
            );
        })
        .on_iteration(5, move |world| {
            let tab_zero = find_window_entity(0, world);
            let tab_one = find_window_entity(1, world);
            let other = find_window_entity(2, world);

            world.entity_mut(tab_one).despawn();

            let mut query = world.query::<(&LayoutStrip, Has<ActiveWorkspaceMarker>)>();
            let strip = query
                .iter(world)
                .find_map(|(strip, active)| active.then_some(strip))
                .expect("active strip not found");
            assert_eq!(strip.len(), 1);
            assert_eq!(strip.index_of(tab_zero).unwrap(), 0);
            assert_eq!(strip.index_of(other).unwrap(), 0);
            assert!(!strip.contains(tab_one));
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
        .on_iteration(2, move |world| {
            assert_window_at!(world, 4, left_edge, top_edge);
            assert_window_at!(world, 3, left_edge + TEST_WINDOW_WIDTH, top_edge);
            assert_window_at!(world, 2, left_edge + 2 * TEST_WINDOW_WIDTH, top_edge);
            assert_window_at!(world, 1, offscreen_right, top_edge);
            assert_window_at!(world, 0, offscreen_right, top_edge);
        })
        .on_iteration(3, move |world| {
            assert_window_at!(world, 4, offscreen_left, top_edge);
            assert_window_at!(world, 3, offscreen_left, top_edge);
            assert_window_at!(world, 2, right_edge - 3 * TEST_WINDOW_WIDTH, top_edge);
            assert_window_at!(world, 1, right_edge - 2 * TEST_WINDOW_WIDTH, top_edge);
            assert_window_at!(world, 0, right_edge - TEST_WINDOW_WIDTH, top_edge);
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
        .on_iteration(3, move |world| {
            assert_window_at!(world, 2, 0, TEST_MENUBAR_HEIGHT);
            assert_window_at!(world, 1, 400, TEST_MENUBAR_HEIGHT);
            assert_window_at!(world, 0, 800, TEST_MENUBAR_HEIGHT);
        })
        .on_iteration(5, move |world| {
            assert_window_at!(world, 2, -316, TEST_MENUBAR_HEIGHT);
            assert_window_at!(world, 1, 84, TEST_MENUBAR_HEIGHT);
            assert_window_at!(world, 0, 484, TEST_MENUBAR_HEIGHT);
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
        .on_iteration(3, |world| {
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
        .on_iteration(2, |world| {
            let mut query = world.query::<&Window>();
            let window = query.iter(world).find(|w| w.id() == 1).unwrap();
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
            command: Command::Window(Operation::Center),
        },
        Event::Command {
            command: Command::Window(Operation::Swap(Direction::Last)),
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
        .on_iteration(1, move |world| {
            assert_window_at!(world, 4, centered, TEST_MENUBAR_HEIGHT);
        })
        .on_iteration(2, move |world| {
            assert_window_at!(world, 4, right_edge, TEST_MENUBAR_HEIGHT);
            assert_window_at!(
                world,
                0,
                right_edge - TEST_WINDOW_WIDTH,
                TEST_MENUBAR_HEIGHT
            );
            assert_focused!(world, 4);
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
        .on_iteration(2, |world| {
            assert_window_at!(world, 0, 0, TEST_MENUBAR_HEIGHT);
            assert_window_at!(world, 1, TEST_WINDOW_WIDTH, TEST_MENUBAR_HEIGHT);
            assert_focused!(world, 0);
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
        Event::Command {
            command: Command::PrintState,
        },
    ]);

    verify_focused_window(0, harness.app.world_mut());

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

    verify_focused_window(3, harness.app.world_mut());
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
        .on_iteration(1, |world| {
            assert_focused!(world, 3);
        })
        .on_iteration(2, |world| {
            assert_focused!(world, 3);
        })
        .run(commands);
}

#[test]
fn test_repeated_external_focus_reshuffles_already_focused_window() {
    let commands = vec![
        Event::MenuOpened { window_id: 0 },
        Event::Command {
            command: Command::Window(Operation::Focus(Direction::East)),
        },
        Event::Command {
            command: Command::Window(Operation::Focus(Direction::East)),
        },
        Event::Command {
            command: Command::Window(Operation::Focus(Direction::East)),
        },
        Event::Command {
            command: Command::Window(Operation::Focus(Direction::East)),
        },
        Event::WindowFocused { window_id: 0 },
    ];

    TestHarness::new()
        .with_windows(5)
        .on_iteration(5, |world| {
            assert_focused!(world, 0);

            let mut query =
                world.query::<(&mut Position, &LayoutStrip, Has<ActiveWorkspaceMarker>)>();
            let (mut position, _, _) = query
                .iter_mut(world)
                .find(|(_, _, active)| *active)
                .expect("active strip");
            position.0.x = TEST_DISPLAY_WIDTH * 2;
        })
        .on_iteration(4, |world| {
            assert_focused!(world, 0);
            assert_window_at!(
                world,
                0,
                TEST_DISPLAY_WIDTH - TEST_WINDOW_WIDTH,
                TEST_MENUBAR_HEIGHT
            );
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
        .on_iteration(1, |world| {
            let mut query = world.query::<(&LayoutStrip, Has<ActiveWorkspaceMarker>)>();
            let active = query
                .iter(world)
                .find_map(|(strip, active)| active.then_some(strip.virtual_index))
                .expect("an active virtual strip");
            assert_eq!(active, 1);
            assert_focused!(world, 0);
        })
        .on_iteration(3, |world| {
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
        .on_iteration(2, |world| {
            let mut query = world.query::<(&LayoutStrip, Has<ActiveWorkspaceMarker>)>();
            let active = query
                .iter(world)
                .find_map(|(strip, active)| active.then_some(strip.virtual_index))
                .expect("an active virtual strip");
            assert_eq!(active, 1);
        })
        .on_iteration(5, |world| {
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
    let ignored_repositions_for_window = ignored_repositions.clone();

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

    let mut harness = TestHarness::new();
    let mock_app = setup_process(harness.app.world_mut());
    let internal_queue = harness.internal_queue.clone();
    let wm = MockWindowManager {
        windows: Box::new(move |_| {
            let origin = Origin::new(0, 0);
            let size = Size::new(TEST_WINDOW_WIDTH, TEST_WINDOW_HEIGHT);
            let window = MockWindow::new(
                0,
                IRect {
                    min: origin,
                    max: origin + size,
                },
                internal_queue.clone(),
                mock_app.clone(),
            )
            .with_ignored_repositions(ignored_repositions_for_window.clone());
            vec![Window::new(Box::new(window))]
        }),
        workspaces: vec![TEST_WORKSPACE_ID],
        visible_windows: HashMap::new(),
    };

    harness
        .with_wm(wm)
        .on_iteration(1, move |world| {
            let mut query = world.query::<&mut Window>();
            let mut window = query
                .iter_mut(world)
                .find(|window| window.id() == 0)
                .expect("window 0");
            window.reposition(Origin::new(0, TEST_DISPLAY_HEIGHT));
            ignored_repositions.store(1, std::sync::atomic::Ordering::SeqCst);
        })
        .on_iteration(4, |world| {
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
    use crate::events::Event;
    use crate::platform::Modifiers;
    use objc2_core_foundation::CGPoint;

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
        .on_iteration(2, |world| {
            // After MouseMoved into corner dead zone: focus should remain on window 2
            // because the corner gate suppressed the focus-follow-mouse event.
            assert_focused!(world, 2);
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
        .on_iteration(2, |world| {
            // After MouseMoved outside corner: FFM should have fired and changed focus.
            // In the mock, find_window_at_point always returns window 0, so window 0
            // should now be focused (changed from window 2).
            assert_focused!(world, 0);
        })
        .run(commands);
}

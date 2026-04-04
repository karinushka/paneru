use std::sync::OnceLock;
use std::time::Duration;

use bevy::prelude::*;
use bevy::tasks::{AsyncComputeTaskPool, TaskPoolBuilder};
use bevy::time::TimeUpdateStrategy;
use tracing::debug;
use tracing_subscriber::{EnvFilter, fmt, prelude::*};

use crate::commands::register_commands;
use crate::config::Config;
use crate::ecs::{
    BProcess, ExistingMarker, FocusFollowsMouse, FocusedMarker, Initializing, MissionControlActive,
    PollForNotifications, SkipReshuffle, register_systems, register_triggers,
};
use crate::events::Event;
use crate::manager::{Application, Origin, Size, Window};
use crate::platform::ProcessSerialNumber;
use crate::platform::WinID;

use super::mocks::{MockApplication, MockProcess, MockWindow};
use super::*;

pub(crate) fn setup_world() -> App {
    static DONE: OnceLock<()> = OnceLock::new();
    DONE.get_or_init(|| {
        _ = tracing_subscriber::registry()
            .with(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
            .with(
                fmt::layer()
                    .with_level(true)
                    .with_line_number(true)
                    .with_file(true)
                    .with_target(true)
                    .with_thread_ids(false)
                    .with_writer(std::io::stderr)
                    .compact(),
            )
            .try_init();

        let _pool = AsyncComputeTaskPool::get_or_init(|| {
            TaskPoolBuilder::new()
                .num_threads(1) // Keep it light for tests
                .build()
        });
        assert!(AsyncComputeTaskPool::try_get().is_some());
    });
    let mut bevy_app = App::new();
    bevy_app
        .add_plugins(MinimalPlugins)
        .init_resource::<bevy::ecs::message::Messages<Event>>()
        .insert_resource(PollForNotifications)
        .insert_resource(SkipReshuffle(false))
        .insert_resource(MissionControlActive(false))
        .insert_resource(FocusFollowsMouse(None))
        .insert_resource(Config::default())
        .insert_resource(Initializing)
        .add_plugins((register_triggers, register_systems, register_commands));

    bevy_app.insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_millis(
        100,
    )));

    bevy_app
}

pub(crate) fn setup_process(world: &mut World) -> MockApplication {
    let psn = ProcessSerialNumber { high: 1, low: 2 };
    let mock_process = MockProcess { psn };
    let process = world.spawn(BProcess(Box::new(mock_process))).id();

    let application = MockApplication::new(psn, TEST_PROCESS_ID);
    world.spawn((
        ExistingMarker,
        ChildOf(process),
        Application::new(Box::new(application.clone())),
    ));
    application
}

pub(crate) fn run_main_loop(
    bevy_app: &mut App,
    event_queue: &EventQueue,
    commands: &[Event],
    mut verifier: impl FnMut(usize, &mut World),
) {
    for (iteration, command) in commands.iter().enumerate() {
        bevy_app.world_mut().write_message::<Event>(command.clone());

        for _ in 0..5 {
            bevy_app.update();

            // Flush the event queue with internally generated mock events.
            while let Some(event) = event_queue.write().unwrap().pop() {
                bevy_app.world_mut().write_message::<Event>(event);
            }
        }

        verifier(iteration, bevy_app.world_mut());
    }
}

pub(crate) fn verify_window_positions(
    expected_positions: &[(WinID, (i32, i32))],
    world: &mut World,
) {
    let mut query = world.query::<&Window>();

    for window in query.iter(world) {
        if let Some((window_id, (x, y))) = expected_positions.iter().find(|id| id.0 == window.id())
        {
            debug!("WinID: {window_id}");
            assert_eq!(*x, window.frame().min.x);
            assert_eq!(*y, window.frame().min.y);
        }
    }
}

pub(crate) fn verify_window_sizes(expected_sizes: &[(WinID, (i32, i32))], world: &mut World) {
    let mut query = world.query::<&Window>();

    for window in query.iter(world) {
        if let Some((window_id, (w, h))) = expected_sizes.iter().find(|id| id.0 == window.id()) {
            let frame = window.frame();
            assert_eq!(
                *w,
                frame.width(),
                "WinID {window_id}: expected width {w}, got {}",
                frame.width()
            );
            assert_eq!(
                *h,
                frame.height(),
                "WinID {window_id}: expected height {h}, got {}",
                frame.height()
            );
        }
    }
}

pub(crate) fn window_spawner(
    count: i32,
    event_queue: EventQueue,
    mock_app: MockApplication,
) -> TestWindowSpawner {
    Box::new(move |_| {
        (0..count)
            .map(|i| {
                let origin = Origin::new(0, 0);
                let size = Size::new(TEST_WINDOW_WIDTH, TEST_WINDOW_HEIGHT);
                let window = MockWindow::new(
                    i,
                    IRect {
                        min: origin,
                        max: origin + size,
                    },
                    event_queue.clone(),
                    mock_app.clone(),
                );
                Window::new(Box::new(window))
            })
            .collect::<Vec<_>>()
    })
}

pub(crate) fn find_window_entity(window_id: WinID, world: &mut World) -> Entity {
    let mut query = world.query::<(&Window, Entity)>();
    query
        .iter(world)
        .find(|(w, _)| w.id() == window_id)
        .map_or_else(|| panic!("window {window_id} not found"), |(_, e)| e)
}

pub(crate) fn verify_focused_window(expected_id: WinID, world: &mut World) {
    let mut query = world.query::<(&Window, Has<FocusedMarker>)>();
    let focused: Vec<_> = query.iter(world).filter(|(_, focused)| *focused).collect();
    assert_eq!(focused.len(), 1, "expected exactly one focused window");
    assert_eq!(
        focused[0].0.id(),
        expected_id,
        "expected window {expected_id} focused, got {}",
        focused[0].0.id()
    );
}

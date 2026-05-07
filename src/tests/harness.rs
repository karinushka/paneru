use std::collections::HashMap;
use std::sync::OnceLock;
use std::time::Duration;

use bevy::prelude::*;
use bevy::tasks::{AsyncComputeTaskPool, TaskPoolBuilder};
use bevy::time::TimeUpdateStrategy;
use tracing_subscriber::{EnvFilter, fmt, prelude::*};

use crate::commands::register_commands;
use crate::config::Config;
use crate::ecs::layout::LayoutEventsPlugin;
use crate::ecs::mouse::MouseEventsPlugin;
use crate::ecs::scroll::ScrollEventsPlugin;
use crate::ecs::workspace::WorkspaceEventsPlugin;
use crate::ecs::{
    BProcess, ExistingMarker, FocusFollowsMouse, FocusedMarker, Initializing, MissionControlActive,
    PollForNotifications, SkipReshuffle, register_systems, register_triggers,
};
use crate::events::Event;
use crate::manager::{Application, Origin, Size, Window, WindowManager, WindowManagerApi};
use crate::platform::ProcessSerialNumber;
use crate::platform::WinID;

use super::mocks::{MockApplication, MockProcess, MockWindow, MockWindowManager};
use super::*;

type Verifiers = HashMap<usize, Box<dyn FnMut(&mut World)>>;

pub(crate) struct TestHarness {
    pub(crate) app: App,
    pub(crate) internal_queue: EventQueue,
    pub(crate) verifiers: Verifiers,
}

impl TestHarness {
    pub(crate) fn new() -> Self {
        let app = setup_world();
        let internal_queue = Arc::new(RwLock::new(Vec::new()));
        Self {
            app,
            internal_queue,
            verifiers: HashMap::new(),
        }
    }

    pub(crate) fn with_windows(mut self, count: i32) -> Self {
        let mock_app = setup_process(self.app.world_mut());
        let spawner = window_spawner(count, self.internal_queue.clone(), mock_app);
        let wm = MockWindowManager {
            windows: spawner,
            workspaces: vec![TEST_WORKSPACE_ID],
        };
        self.app
            .world_mut()
            .insert_resource(WindowManager(Box::new(wm)));
        self
    }

    pub(crate) fn with_wm<T: WindowManagerApi + 'static>(mut self, wm: T) -> Self {
        self.app
            .world_mut()
            .insert_resource(WindowManager(Box::new(wm)));
        self
    }

    pub(crate) fn with_config(mut self, config: Config) -> Self {
        self.app.world_mut().insert_resource(config);
        self
    }

    pub(crate) fn on_iteration<F>(mut self, iteration: usize, verifier: F) -> Self
    where
        F: FnMut(&mut World) + 'static,
    {
        self.verifiers.insert(iteration, Box::new(verifier));
        self
    }

    pub(crate) fn run(&mut self, commands: Vec<Event>) {
        for (iteration, command) in commands.into_iter().enumerate() {
            self.app.world_mut().write_message::<Event>(command);

            for _ in 0..5 {
                self.app.update();

                // Flush the event queue with internally generated mock events.
                while let Some(event) = self.internal_queue.write().unwrap().pop() {
                    self.app.world_mut().write_message::<Event>(event);
                }
            }

            if let Some(verifier) = self.verifiers.get_mut(&iteration) {
                verifier(self.app.world_mut());
            }
        }
    }
}

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
        .add_plugins(MouseEventsPlugin)
        .add_plugins(ScrollEventsPlugin)
        .add_plugins(WorkspaceEventsPlugin)
        .add_plugins(LayoutEventsPlugin)
        .add_plugins(MouseEventsPlugin)
        .add_plugins((register_triggers, register_systems, register_commands));

    bevy_app.insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_millis(
        100,
    )));

    bevy_app
}

pub(crate) fn setup_process(world: &mut World) -> MockApplication {
    let psn = ProcessSerialNumber { high: 1, low: 2 };
    let bundle_id = "test".to_string();
    let mock_process = MockProcess { psn };
    let process = world.spawn(BProcess(Box::new(mock_process))).id();

    let application = MockApplication::new(psn, TEST_PROCESS_ID, bundle_id);
    world.spawn((
        ExistingMarker,
        ChildOf(process),
        Application::new(Box::new(application.clone())),
    ));
    application
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

#[macro_export]
macro_rules! assert_window_at {
    ($world:expr, $id:expr, $x:expr, $y:expr) => {{
        let mut query = $world.query::<&$crate::manager::Window>();
        let window = query
            .iter($world)
            .find(|w| w.id() == $id)
            .expect("window not found");
        assert_eq!(
            window.frame().min.x,
            $x,
            "window {} x position mismatch",
            $id
        );
        assert_eq!(
            window.frame().min.y,
            $y,
            "window {} y position mismatch",
            $id
        );
    }};
}

#[macro_export]
macro_rules! assert_window_size {
    ($world:expr, $id:expr, $w:expr, $h:expr) => {{
        let mut query = $world.query::<&$crate::manager::Window>();
        let window = query
            .iter($world)
            .find(|w| w.id() == $id)
            .expect("window not found");
        let frame = window.frame();
        assert_eq!(frame.width(), $w, "window {} width mismatch", $id);
        assert_eq!(frame.height(), $h, "window {} height mismatch", $id);
    }};
}

#[macro_export]
macro_rules! assert_focused {
    ($world:expr, $id:expr) => {{
        let mut query = $world.query::<(
            &$crate::manager::Window,
            bevy::ecs::query::Has<$crate::ecs::FocusedMarker>,
        )>();
        let (_, focused) = query
            .iter($world)
            .find(|(w, _)| w.id() == $id)
            .expect("window not found");
        assert!(focused, "window {} should be focused", $id);
    }};
}

#[macro_export]
macro_rules! assert_on_workspace {
    ($world:expr, $window_id:expr, $workspace_id:expr) => {{
        let entity = $crate::tests::harness::find_window_entity($window_id, $world);
        let mut query = $world.query::<&$crate::ecs::layout::LayoutStrip>();
        let found = query
            .iter($world)
            .any(|strip| strip.id() == $workspace_id && strip.index_of(entity).is_ok());
        assert!(
            found,
            "window {} should be on workspace {}",
            $window_id, $workspace_id
        );
    }};
}

#[macro_export]
macro_rules! assert_not_on_workspace {
    ($world:expr, $window_id:expr, $workspace_id:expr) => {{
        let entity = $crate::tests::harness::find_window_entity($window_id, $world);
        let mut query = $world.query::<&$crate::ecs::layout::LayoutStrip>();
        let found = query
            .iter($world)
            .any(|strip| strip.id() == $workspace_id && strip.index_of(entity).is_ok());
        assert!(
            !found,
            "window {} should NOT be on workspace {}",
            $window_id, $workspace_id
        );
    }};
}

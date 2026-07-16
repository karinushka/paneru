use std::collections::HashMap;
use std::sync::OnceLock;
use std::time::Duration;

use bevy::prelude::*;
use bevy::tasks::{AsyncComputeTaskPool, TaskPoolBuilder};
use bevy::time::TimeUpdateStrategy;
use tracing_subscriber::{EnvFilter, fmt, prelude::*};

use crate::commands::register_commands;
use crate::config::Config;
use crate::ecs::display::DisplayEventsPlugin;
use crate::ecs::focus::FocusEventsPlugin;
use crate::ecs::layout::LayoutEventsPlugin;
use crate::ecs::mouse::MouseEventsPlugin;
use crate::ecs::scroll::ScrollEventsPlugin;
use crate::ecs::state::PaneruState;
use crate::ecs::workspace::WorkspaceEventsPlugin;
use crate::ecs::{
    BProcess, DefaultWindowDisposition, ExistingMarker, FocusFollowsMouse, Initializing,
    MissionControlActive, SkipReshuffle, SpawnWindowTrigger, WindowDisposition, register_systems,
    register_triggers,
};
use crate::events::Event;
use crate::manager::{Window, WindowManager};
use crate::platform::{AxMainThread, Pid, WinID, WorkspaceId};

use super::*;

type VerifierFunc = Box<dyn FnMut(&mut World, MockState)>;
pub(crate) struct TestHarness {
    pub(crate) app: App,
    pub(crate) mock_state: MockState,
    pub(crate) verifiers: HashMap<usize, VerifierFunc>,
}

impl TestHarness {
    pub(crate) fn new() -> Self {
        let pid = TEST_PROCESS_ID;
        let mut app = setup_world();
        let mut mock_state = MockState::new();

        // Setup default display
        mock_state.add_display(
            TEST_DISPLAY_ID,
            IRect::new(0, 0, TEST_DISPLAY_WIDTH, TEST_DISPLAY_HEIGHT),
            vec![TEST_WORKSPACE_ID],
        );

        // Initialize Bevy with the mocked process and WM
        let world = app.world_mut();

        mock_state.spawn_app(pid, "test", "TestApp");

        let mock_process = mock_state.create_process(pid);
        let process_entity = world.spawn(BProcess(Box::new(mock_process))).id();

        let application = mock_state.create_application(pid);
        world.spawn((ExistingMarker, ChildOf(process_entity), application));

        let wm = mock_state.create_window_manager();
        world.insert_resource(WindowManager(Box::new(wm)));

        Self {
            app,
            mock_state,
            verifiers: HashMap::new(),
        }
    }

    pub(crate) fn world(&mut self) -> &mut World {
        self.app.world_mut()
    }

    pub(crate) fn with_windows(mut self, count: i32) -> Self {
        let pid = TEST_PROCESS_ID;

        let windows = (0..count)
            .map(|i| {
                let win_id = i as WinID;
                let frame = IRect::new(0, 0, TEST_WINDOW_WIDTH, TEST_WINDOW_HEIGHT);
                self.mock_state
                    .spawn_window(pid, TEST_WORKSPACE_ID, win_id, frame)
            })
            .collect::<Vec<_>>();
        self.app.world_mut().trigger(SpawnWindowTrigger(windows));

        self
    }

    pub(crate) fn with_window<F>(mut self, id: WinID, f: F) -> Self
    where
        F: FnOnce(&mut MockWindowData),
    {
        let pid = TEST_PROCESS_ID;
        let frame = IRect::new(0, 0, TEST_WINDOW_WIDTH, TEST_WINDOW_HEIGHT);
        let window = self
            .mock_state
            .spawn_window(pid, TEST_WORKSPACE_ID, id, frame);
        self.mock_state.update_window(id, f);
        self.app
            .world_mut()
            .trigger(SpawnWindowTrigger(vec![window]));
        self
    }

    pub(crate) fn with_workspace_window<F>(
        mut self,
        id: WinID,
        workspace_id: WorkspaceId,
        f: F,
    ) -> Self
    where
        F: FnOnce(&mut MockWindowData),
    {
        let pid = TEST_PROCESS_ID;
        let frame = IRect::new(0, 0, TEST_WINDOW_WIDTH, TEST_WINDOW_HEIGHT);
        let window = self.mock_state.spawn_window(pid, workspace_id, id, frame);
        self.mock_state.update_window(id, f);
        self.app
            .world_mut()
            .trigger(SpawnWindowTrigger(vec![window]));
        self
    }

    #[allow(unused)]
    pub(crate) fn with_focused_window(self, id: WinID) -> Self {
        self.mock_state.focus_window(id);
        self
    }

    pub(crate) fn with_display(
        mut self,
        id: u32,
        bounds: IRect,
        workspaces: Vec<WorkspaceId>,
    ) -> Self {
        self.mock_state.add_display(id, bounds, workspaces);
        self
    }

    #[allow(unused)]
    pub(crate) fn with_app<F>(mut self, pid: Pid, bundle_id: &str, name: &str, f: F) -> Self
    where
        F: FnOnce(&mut MockAppData),
    {
        self.mock_state.spawn_app(pid, bundle_id, name);
        self.mock_state.update_app(pid, f);

        let world = self.app.world_mut();
        let mock_process = self.mock_state.create_process(pid);
        let process_entity = world.spawn(BProcess(Box::new(mock_process))).id();

        let application = self.mock_state.create_application(pid);
        world.spawn((ExistingMarker, ChildOf(process_entity), application));

        self
    }

    pub(crate) fn with_config(mut self, config: Config) -> Self {
        self.app.world_mut().insert_resource(config);
        self
    }

    pub(crate) fn with_default_window_disposition(
        mut self,
        disposition: WindowDisposition,
    ) -> Self {
        self.app
            .world_mut()
            .insert_resource(DefaultWindowDisposition(disposition));
        self
    }

    pub(crate) fn with_state(mut self, state: PaneruState) -> Self {
        self.app.world_mut().insert_resource(state);
        self
    }

    pub(crate) fn on_iteration<F>(mut self, iteration: usize, verifier: F) -> Self
    where
        F: FnMut(&mut World, MockState) + 'static,
    {
        self.verifiers.insert(iteration, Box::new(verifier));
        self
    }

    pub(crate) fn run(&mut self, commands: Vec<Event>) {
        for (iteration, command) in commands.into_iter().enumerate() {
            self.app.world_mut().write_message::<Event>(command);

            for _ in 0..5 {
                self.app.update();

                // Drain and process events from our virtual OS
                for event in self.mock_state.drain_events() {
                    self.app.world_mut().write_message::<Event>(event);
                }
            }

            if let Some(verifier) = self.verifiers.get_mut(&iteration) {
                verifier(self.app.world_mut(), self.mock_state.clone());
            }
        }
    }
}

fn setup_world() -> App {
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
        .insert_resource(SkipReshuffle(false))
        .insert_resource(MissionControlActive(false))
        .insert_resource(FocusFollowsMouse(None))
        // Most historical layout tests exercise the managed strip. Production
        // setup uses `DefaultWindowDisposition::default()` (passthrough).
        .insert_resource(DefaultWindowDisposition(WindowDisposition::Managed))
        .insert_resource(Config::default())
        .insert_resource(Initializing)
        .insert_non_send_resource(AxMainThread::for_tests())
        .add_plugins(MouseEventsPlugin)
        .add_plugins(ScrollEventsPlugin)
        .add_plugins(WorkspaceEventsPlugin)
        .add_plugins(LayoutEventsPlugin)
        .add_plugins(FocusEventsPlugin)
        .add_plugins(DisplayEventsPlugin)
        .add_plugins((register_triggers, register_systems, register_commands));

    bevy_app.insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_millis(
        100,
    )));

    bevy_app
}

pub(crate) fn find_window_entity(window_id: WinID, world: &mut World) -> Entity {
    let mut query = world.query::<(&Window, Entity)>();
    query
        .iter(world)
        .find(|(w, _)| w.id() == window_id)
        .map_or_else(|| panic!("window {window_id} not found"), |(_, e)| e)
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

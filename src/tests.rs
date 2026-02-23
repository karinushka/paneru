use std::sync::{Arc, OnceLock, RwLock};
use std::time::Duration;

use super::*;
use bevy::prelude::*;
use bevy::tasks::{AsyncComputeTaskPool, TaskPoolBuilder};
use bevy::time::TimeUpdateStrategy;
use objc2_core_foundation::{CFRetained, CGPoint};
use objc2_core_graphics::CGDirectDisplayID;
use stdext::function_name;
use stdext::prelude::RwLockExt;
use tracing::{Level, debug, instrument};

use crate::commands::{Command, Direction, Operation, register_commands};
use crate::config::Config;
use crate::ecs::{
    BProcess, ExistingMarker, FocusFollowsMouse, FocusedMarker, Initializing, MissionControlActive,
    PollForNotifications, SkipReshuffle, SpawnWindowTrigger, register_systems, register_triggers,
};
use crate::errors::{Error, Result};
use crate::events::Event;
use crate::manager::{
    Application, ApplicationApi, Display, Origin, ProcessApi, Size, Window, WindowApi,
    WindowManager, WindowManagerApi,
};
use crate::platform::{ConnID, Pid, WinID, WorkspaceId};
use crate::{platform::ProcessSerialNumber, util::AXUIWrapper};

const TEST_PROCESS_ID: i32 = 1;
const TEST_DISPLAY_ID: u32 = 1;
const TEST_WORKSPACE_ID: u64 = 2;
const TEST_DISPLAY_WIDTH: i32 = 1024;
const TEST_DISPLAY_HEIGHT: i32 = 768;

const TEST_MENUBAR_HEIGHT: i32 = 20;
const TEST_WINDOW_WIDTH: i32 = 400;
const TEST_WINDOW_HEIGHT: i32 = 1000;

/// A mock implementation of the `ProcessApi` trait for testing purposes.
#[derive(Debug)]
struct MockProcess {
    psn: ProcessSerialNumber,
}

impl ProcessApi for MockProcess {
    /// Always returns `true`, indicating the mock process is observable.
    #[instrument(level = Level::DEBUG, ret)]
    fn is_observable(&mut self) -> bool {
        debug!("{}:", function_name!());
        true
    }

    /// Returns a static name for the mock process.
    #[instrument(level = Level::DEBUG, ret)]
    fn name(&self) -> &'static str {
        "test"
    }

    /// Returns a predefined PID for the mock process.
    #[instrument(level = Level::DEBUG, ret)]
    fn pid(&self) -> Pid {
        debug!("{}:", function_name!());
        TEST_PROCESS_ID
    }

    /// Returns the `ProcessSerialNumber` of the mock process.
    #[instrument(level = Level::TRACE, ret)]
    fn psn(&self) -> ProcessSerialNumber {
        debug!("{}: {:?}", function_name!(), self.psn);
        self.psn
    }

    /// Always returns `None` for the `NSRunningApplication`.
    #[instrument(level = Level::DEBUG, ret)]
    fn application(&self) -> Option<objc2::rc::Retained<objc2_app_kit::NSRunningApplication>> {
        debug!("{}:", function_name!());
        None
    }

    /// Always returns `true`, indicating the mock process is ready.
    #[instrument(level = Level::DEBUG, ret)]
    fn ready(&mut self) -> bool {
        debug!("{}:", function_name!());
        true
    }
}

/// A mock implementation of the `ApplicationApi` trait for testing purposes.
/// It internally holds an `InnerMockApplication` within an `Arc<RwLock>`.
#[derive(Clone, Debug)]
struct MockApplication {
    inner: Arc<RwLock<InnerMockApplication>>,
}

/// The inner state of `MockApplication`, containing process serial number, PID, and focused window ID.
#[derive(Debug)]
struct InnerMockApplication {
    psn: ProcessSerialNumber,
    pid: Pid,
    focused_id: Option<WinID>,
}

impl MockApplication {
    /// Creates a new `MockApplication` instance.
    ///
    /// # Arguments
    ///
    /// * `psn` - The `ProcessSerialNumber` for this mock application.
    /// * `pid` - The `Pid` for this mock application.
    #[instrument(level = Level::DEBUG, ret)]
    fn new(psn: ProcessSerialNumber, pid: Pid) -> Self {
        MockApplication {
            inner: Arc::new(RwLock::new(InnerMockApplication {
                psn,
                pid,
                focused_id: None,
            })),
        }
    }
}

impl ApplicationApi for MockApplication {
    /// Returns the PID of the mock application.
    #[instrument(level = Level::TRACE, skip(self), ret)]
    fn pid(&self) -> Pid {
        self.inner.force_read().pid
    }

    /// Returns the `ProcessSerialNumber` of the mock application.
    #[instrument(level = Level::TRACE, skip(self), ret)]
    fn psn(&self) -> ProcessSerialNumber {
        debug!("{}:", function_name!());
        self.inner.force_read().psn
    }

    /// Always returns `Some(0)` for the connection ID.
    #[instrument(level = Level::DEBUG, skip(self), ret)]
    fn connection(&self) -> Option<ConnID> {
        debug!("{}:", function_name!());
        Some(0)
    }

    /// Returns the currently focused window ID for the mock application.
    ///
    /// # Returns
    ///
    /// `Ok(WinID)` if a window is focused, otherwise `Err(Error::InvalidWindow)`.
    #[instrument(level = Level::DEBUG, skip(self), ret)]
    fn focused_window_id(&self) -> Result<WinID> {
        let id = self
            .inner
            .force_read()
            .focused_id
            .ok_or(Error::InvalidWindow);
        debug!("{}: {id:?}", function_name!());
        id
    }

    /// Always returns an empty vector of window lists for the mock application.
    fn window_list(&self) -> Vec<Window> {
        debug!("{}:", function_name!());
        vec![]
    }

    /// Always returns `Ok(true)` for observe operations on the mock application.
    #[instrument(level = Level::DEBUG, skip(self), ret)]
    fn observe(&mut self) -> Result<bool> {
        debug!("{}:", function_name!());
        Ok(true)
    }

    /// Always returns `Ok(true)` for observe window operations on the mock application.
    #[instrument(level = Level::DEBUG, skip_all, ret)]
    fn observe_window(&mut self, _window: &Window) -> Result<bool> {
        debug!("{}:", function_name!());
        Ok(true)
    }

    /// Does nothing for unobserve window operations on the mock application.
    #[instrument(level = Level::DEBUG, skip_all, ret)]
    fn unobserve_window(&mut self, _window: &Window) {
        debug!("{}:", function_name!());
    }

    /// Always returns `true`, indicating the mock application is frontmost.
    #[instrument(level = Level::DEBUG, skip(self), ret)]
    fn is_frontmost(&self) -> bool {
        debug!("{}:", function_name!());
        true
    }

    /// Always returns `Some("test")` for the bundle ID.
    #[instrument(level = Level::DEBUG, skip(self), ret)]
    fn bundle_id(&self) -> Option<&str> {
        debug!("{}:", function_name!());
        Some("test")
    }
}

/// A mock implementation of the `WindowManagerApi` trait for testing purposes.
struct MockWindowManager {
    windows: Box<dyn Fn(WorkspaceId) -> Vec<Window> + Send + Sync + 'static>,
}

impl std::fmt::Debug for MockWindowManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MockWindowManager")
            .field("windows", &"<closure>") // Placeholder text
            .finish()
    }
}

impl WindowManagerApi for MockWindowManager {
    /// Creates a new mock application.
    fn new_application(&self, process: &dyn ProcessApi) -> Result<Application> {
        debug!("{}: from process {}", function_name!(), process.name());
        Ok(Application::new(Box::new(MockApplication {
            inner: Arc::new(RwLock::new(InnerMockApplication {
                psn: process.psn(),
                pid: process.pid(),
                focused_id: None,
            })),
        })))
    }

    /// Always returns an empty vector, as associated windows are not tested at this level.
    #[instrument(level = Level::DEBUG, skip(self), ret)]
    fn get_associated_windows(&self, window_id: WinID) -> Vec<WinID> {
        debug!("{}:", function_name!());
        vec![]
    }

    /// Always returns an empty vector, as present displays are mocked elsewhere.
    #[instrument(level = Level::DEBUG, skip(self), ret)]
    fn present_displays(&self) -> Vec<(Display, Vec<WorkspaceId>)> {
        let display = Display::new(
            TEST_DISPLAY_ID,
            IRect::new(0, 0, TEST_DISPLAY_WIDTH, TEST_DISPLAY_HEIGHT),
            TEST_MENUBAR_HEIGHT,
        );
        vec![(display, vec![TEST_WORKSPACE_ID])]
    }

    /// Returns a predefined active display ID.
    #[instrument(level = Level::DEBUG, skip(self), ret)]
    fn active_display_id(&self) -> Result<u32> {
        Ok(TEST_DISPLAY_ID)
    }

    /// Returns a predefined active display space ID.
    #[instrument(level = Level::DEBUG, skip(self), ret)]
    fn active_display_space(&self, display_id: CGDirectDisplayID) -> Result<WorkspaceId> {
        Ok(TEST_WORKSPACE_ID)
    }

    /// Does nothing, as mouse centering is not tested at this level.
    #[instrument(level = Level::DEBUG, skip_all, ret)]
    fn center_mouse(&self, _window: Option<&Window>, _display_bounds: &IRect) {
        debug!("{}:", function_name!());
    }

    /// Always returns an empty vector of windows.
    #[instrument(level = Level::DEBUG, skip_all)]
    fn find_existing_application_windows(
        &self,
        app: &mut Application,
        spaces: &[WorkspaceId],
    ) -> Result<(Vec<Window>, Vec<WinID>)> {
        debug!(
            "{}: app {} spaces {:?}",
            function_name!(),
            app.pid(),
            spaces
        );

        let windows = spaces
            .iter()
            .flat_map(|workspace_id| (self.windows)(*workspace_id))
            .collect::<Vec<_>>();
        Ok((windows, vec![]))
    }

    /// Always returns `Ok(0)`.
    #[instrument(level = Level::DEBUG, skip(self), ret)]
    fn find_window_at_point(&self, point: &CGPoint) -> Result<WinID> {
        debug!("{}:", function_name!());
        Ok(0)
    }

    /// Always returns an empty vector of window IDs.
    #[instrument(level = Level::DEBUG, skip(self), ret)]
    fn windows_in_workspace(&self, workspace_id: WorkspaceId) -> Result<Vec<WinID>> {
        debug!("{}:", function_name!());
        let ids = (self.windows)(workspace_id)
            .iter()
            .map(|window| window.id())
            .collect();
        Ok(ids)
    }

    /// Always returns `Ok(())`.
    #[instrument(level = Level::DEBUG, skip(self), ret)]
    fn quit(&self) -> Result<()> {
        debug!("{}:", function_name!());
        Ok(())
    }

    #[instrument(level = Level::DEBUG, skip(self))]
    fn setup_config_watcher(&self, path: &std::path::Path) -> Result<Box<dyn notify::Watcher>> {
        todo!()
    }
}

/// A mock implementation of the `WindowApi` trait for testing purposes.
#[derive(Debug)]
struct MockWindow {
    id: WinID,
    frame: IRect,
    app: MockApplication,
    event_queue: EventQueue,
    pub minimized: bool,
}

impl WindowApi for MockWindow {
    /// Returns the ID of the mock window.
    #[instrument(level = Level::TRACE, skip(self), ret)]
    fn id(&self) -> WinID {
        self.id
    }

    /// Returns the frame (`CGRect`) of the mock window.
    #[instrument(level = Level::TRACE, skip(self), ret)]
    fn frame(&self) -> IRect {
        self.frame
    }

    /// Returns a dummy `CFRetained<AXUIWrapper>` for the mock window's accessibility element.
    #[instrument(level = Level::DEBUG, skip(self), ret)]
    fn element(&self) -> Option<CFRetained<AXUIWrapper>> {
        debug!("{}:", function_name!());
        None
    }

    /// Always returns an empty string for the window title.
    #[instrument(level = Level::TRACE, skip(self), ret)]
    fn title(&self) -> Result<String> {
        Ok(String::new())
    }

    /// Always returns `Ok(true)` for valid role.
    #[instrument(level = Level::DEBUG, skip(self), ret)]
    fn child_role(&self) -> Result<bool> {
        debug!("{}:", function_name!());
        Ok(true)
    }

    /// Always returns an empty string for the window role.
    #[instrument(level = Level::TRACE, skip(self), ret)]
    fn role(&self) -> Result<String> {
        Ok(String::new())
    }

    /// Always returns an empty string for the window subrole.
    #[instrument(level = Level::TRACE, skip(self), ret)]
    fn subrole(&self) -> Result<String> {
        Ok(String::new())
    }

    /// Always returns `true` for root status.
    #[instrument(level = Level::DEBUG, skip(self), ret)]
    fn is_root(&self) -> bool {
        debug!("{}:", function_name!());
        true
    }

    /// Repositions the mock window's frame to the given coordinates.
    #[instrument(level = Level::DEBUG, skip(self))]
    fn reposition(&mut self, origin: Origin) {
        debug!("{}: id {} to {origin}", function_name!(), self.id);
        let size = self.frame.size();
        self.frame.min = origin;
        self.frame.max = origin + size;
    }

    /// Resizes the mock window's frame to the given dimensions.
    #[instrument(level = Level::DEBUG, skip(self))]
    fn resize(&mut self, size: Size, display_width: i32) {
        debug!("{}: id {} to {size}", function_name!(), self.id);
        self.frame.max = self.frame.min + size;
    }

    /// Always returns `Ok(())` for updating the frame.
    #[instrument(level = Level::DEBUG, skip(self), ret)]
    fn update_frame(&mut self, bounds: &IRect) -> Result<()> {
        debug!("{}:", function_name!());
        Ok(())
    }

    /// Prints a debug message for focus without raise.
    #[instrument(level = Level::DEBUG, skip_all)]
    fn focus_without_raise(
        &self,
        _psn: ProcessSerialNumber,
        currently_focused: &Window,
        _ocused_psn: ProcessSerialNumber,
    ) {
        debug!(
            "{}: id {} {}",
            function_name!(),
            self.id,
            currently_focused.id()
        );
    }

    /// Prints a debug message for focus with raise and updates the mock application's focused ID.
    #[instrument(level = Level::DEBUG, skip(self), ret)]
    fn focus_with_raise(&self, psn: ProcessSerialNumber) {
        debug!("{}: id {}", function_name!(), self.id);
        self.event_queue
            .write()
            .unwrap()
            .push(Event::ApplicationFrontSwitched { psn });
        self.app.inner.force_write().focused_id = Some(self.id);
    }

    /// Does nothing for width ratio.
    #[instrument(level = Level::DEBUG, skip(self), ret)]
    fn width_ratio(&self) -> f64 {
        debug!("{}:", function_name!());
        0.5
    }

    #[instrument(level = Level::TRACE, skip(self), ret)]
    fn pid(&self) -> Result<Pid> {
        Ok(TEST_PROCESS_ID)
    }

    #[instrument(level = Level::DEBUG, skip(self), ret)]
    fn set_padding(&mut self, padding: manager::WindowPadding) {
        debug!("{}:", function_name!());
    }

    fn horizontal_padding(&self) -> i32 {
        0
    }

    #[instrument(level = Level::TRACE, skip(self), ret)]
    fn is_minimized(&self) -> bool {
        self.minimized
    }
}

impl MockWindow {
    /// Creates a new `MockWindow` instance.
    ///
    /// # Arguments
    ///
    /// * `id` - The `WinID` of the window.
    /// * `psn` - An `Option<ProcessSerialNumber>` for the owning process.
    /// * `frame` - The `CGRect` representing the window's initial frame.
    /// * `event_queue` - An optional reference to an `EventQueue` for simulating events.
    /// * `app` - A `MockApplication` instance associated with this window.
    fn new(id: WinID, frame: IRect, event_queue: EventQueue, app: MockApplication) -> Self {
        MockWindow {
            id,
            frame,
            app,
            event_queue,
            minimized: false,
        }
    }
}

fn setup_world() -> App {
    static DONE: OnceLock<()> = OnceLock::new();
    DONE.get_or_init(|| {
        tracing_subscriber::registry()
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
            .init();

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
        .init_resource::<Messages<Event>>()
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

fn setup_process(world: &mut World) -> MockApplication {
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

/// Type alias for a shared, thread-safe queue of `Event`s, used for simulating internal events in tests.
type EventQueue = Arc<RwLock<Vec<Event>>>;
// type WindowCreator = impl Fn(WorkspaceId) -> Vec<Window> + Send + Sync + 'static;

/// Runs the main test loop, simulating command dispatch and Bevy app updates.
/// For each command, the Bevy app is updated multiple times, and internal mock events are flushed.
/// A `verifier` closure is called after each command to assert the state of the world.
///
/// # Arguments
///
/// * `commands` - A slice of `Event`s representing commands to dispatch.
/// * `verifier` - A closure that takes the current iteration and a mutable reference to the `World` for assertions.
fn run_main_loop(
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

/// Verifies the positions of windows against a set of expected positions.
/// This function queries `Window` components from the world and asserts their `origin.x` and `origin.y` values.
///
/// # Arguments
///
/// * `expected_positions` - A slice of `(WinID, (i32, i32))` tuples, where `WinID` is the window ID and `(i32, i32)` are the expected (x, y) coordinates.
/// * `world` - A mutable reference to the Bevy `World` for querying window components.
fn verify_window_positions(expected_positions: &[(WinID, (i32, i32))], world: &mut World) {
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

#[test]
#[allow(clippy::too_many_lines)]
fn test_window_shuffle() {
    let commands = vec![
        Event::MenuOpened { window_id: 0 }, // Noop allowing everything to settle
        Event::Command {
            command: Command::Window(Operation::Focus(Direction::Last)),
        },
        Event::Command {
            command: Command::Window(Operation::Focus(Direction::First)),
        },
        Event::Command {
            command: Command::Window(Operation::Focus(Direction::East)),
        },
        Event::Command {
            command: Command::Window(Operation::Stack(true)),
        },
        Event::Command {
            command: Command::Window(Operation::Center),
        },
        Event::Command {
            command: Command::Window(Operation::Focus(Direction::East)),
        },
        Event::Command {
            command: Command::Window(Operation::Stack(true)),
        },
        Event::Command {
            command: Command::Window(Operation::Center),
        },
    ];

    let offscreen_left = 0 - TEST_WINDOW_WIDTH + 5;
    let offscreen_right = TEST_DISPLAY_WIDTH - 5;

    let expected_positions_last = [
        (4, (offscreen_left, TEST_MENUBAR_HEIGHT)),
        (3, (offscreen_left, TEST_MENUBAR_HEIGHT)),
        (2, (-176, TEST_MENUBAR_HEIGHT)),
        (1, (224, TEST_MENUBAR_HEIGHT)),
        (0, (624, TEST_MENUBAR_HEIGHT)),
    ];
    let expected_positions_first = [
        (4, (0, TEST_MENUBAR_HEIGHT)),
        (3, (400, TEST_MENUBAR_HEIGHT)),
        (2, (800, TEST_MENUBAR_HEIGHT)),
        (1, (offscreen_right, TEST_MENUBAR_HEIGHT)),
        (0, (offscreen_right, TEST_MENUBAR_HEIGHT)),
    ];

    let centered = (TEST_DISPLAY_WIDTH - TEST_WINDOW_WIDTH) / 2;
    let expected_positions_stacked = [
        (4, (centered, TEST_MENUBAR_HEIGHT)),
        (3, (centered, 374 + TEST_MENUBAR_HEIGHT)),
        (2, (centered + TEST_WINDOW_WIDTH, TEST_MENUBAR_HEIGHT)),
        (1, (offscreen_right, TEST_MENUBAR_HEIGHT)),
        (0, (offscreen_right, TEST_MENUBAR_HEIGHT)),
    ];
    let expected_positions_stacked2 = [
        (4, (centered, TEST_MENUBAR_HEIGHT)),
        (3, (centered, 249 + TEST_MENUBAR_HEIGHT)),
        (2, (centered, 498 + TEST_MENUBAR_HEIGHT)),
        (1, (712, TEST_MENUBAR_HEIGHT)),
        (0, (offscreen_right, TEST_MENUBAR_HEIGHT)),
    ];

    let check = |iteration, world: &mut World| {
        let iterations = [
            None,
            Some(expected_positions_last.as_slice()),
            Some(expected_positions_first.as_slice()),
            None,
            None,
            Some(expected_positions_stacked.as_slice()),
            None,
            None,
            Some(expected_positions_stacked2.as_slice()),
        ];

        if let Some(positions) = iterations[iteration] {
            debug!("Iteration: {iteration}");
            verify_window_positions(positions, world);
        }
    };

    let mut bevy = setup_world();
    let mock_app = setup_process(bevy.world_mut());
    let internal_queue = Arc::new(RwLock::new(Vec::<Event>::new()));
    let event_queue = internal_queue.clone();

    let windows = Box::new(move |_| {
        (0..5)
            .map(|i| {
                let origin = Origin::new(100 * i, 0);
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
    });
    let window_manager = MockWindowManager { windows };
    bevy.world_mut()
        .insert_resource(WindowManager(Box::new(window_manager)));

    run_main_loop(&mut bevy, &internal_queue, &commands, check);
}

#[test]
fn test_startup_windows() {
    let commands = vec![
        Event::MenuOpened { window_id: 0 }, // Noop allowing everything to settle
        Event::Command {
            command: Command::Window(Operation::Focus(Direction::East)),
        },
        Event::Command {
            command: Command::Window(Operation::Focus(Direction::East)),
        },
        Event::Command {
            command: Command::Window(Operation::Focus(Direction::First)),
        },
        Event::Command {
            command: Command::PrintState,
        },
    ];

    let expected_positions = [
        (4, (0, TEST_MENUBAR_HEIGHT)),
        (3, (400, TEST_MENUBAR_HEIGHT)),
        (2, (800, TEST_MENUBAR_HEIGHT)),
    ];

    let check = |iteration, world: &mut World| {
        let iterations = [None, None, None, None, Some(expected_positions.as_slice())];

        if let Some(positions) = iterations[iteration] {
            debug!("Iteration: {iteration}");
            verify_window_positions(positions, world);
        }
    };

    let mut bevy = setup_world();
    let mock_app = setup_process(bevy.world_mut());
    let internal_queue = Arc::new(RwLock::new(Vec::<Event>::new()));
    let event_queue = internal_queue.clone();

    let windows = Box::new(move |_| {
        (0..5)
            .map(|i| {
                let origin = Origin::new(100 * i, 0);
                let size = Size::new(TEST_WINDOW_WIDTH, TEST_WINDOW_HEIGHT);
                let mut window = MockWindow::new(
                    i,
                    IRect {
                        min: origin,
                        max: origin + size,
                    },
                    event_queue.clone(),
                    mock_app.clone(),
                );
                if i < 2 {
                    window.minimized = true;
                }
                Window::new(Box::new(window))
            })
            .collect::<Vec<_>>()
    });
    let window_manager = MockWindowManager { windows };
    bevy.world_mut()
        .insert_resource(WindowManager(Box::new(window_manager)));

    run_main_loop(&mut bevy, &internal_queue, &commands, check);
}

#[test]
fn test_dont_focus() {
    let commands = vec![
        Event::MenuOpened { window_id: 0 }, // Noop allowing everything to settle
        Event::Command {
            command: Command::Window(Operation::Focus(Direction::Last)),
        },
        Event::Command {
            command: Command::Window(Operation::Focus(Direction::First)),
        },
        Event::Command {
            command: Command::PrintState,
        },
    ];

    let offscreen_right = TEST_DISPLAY_WIDTH - 5;
    let expected_positions = [
        (2, (0, TEST_MENUBAR_HEIGHT)),
        (1, (400, TEST_MENUBAR_HEIGHT)),
        (0, (800, TEST_MENUBAR_HEIGHT)),
        (3, (offscreen_right, TEST_MENUBAR_HEIGHT)),
    ];

    let mut bevy = setup_world();
    let app = setup_process(bevy.world_mut());
    let mock_app = app.clone();
    let internal_queue = Arc::new(RwLock::new(Vec::<Event>::new()));
    let event_queue = internal_queue.clone();

    let windows = Box::new(move |_| {
        (0..3)
            .map(|i| {
                let origin = Origin::new(100 * i, 0);
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
    });
    let window_manager = MockWindowManager { windows };
    bevy.world_mut()
        .insert_resource(WindowManager(Box::new(window_manager)));

    let check_queue = internal_queue.clone();
    let check = |iteration, world: &mut World| {
        let iterations = [None, None, None, Some(expected_positions.as_slice())];

        if let Some(positions) = iterations[iteration] {
            debug!("Iteration: {iteration}");
            verify_window_positions(positions, world);

            let mut query = world.query::<(&Window, Has<FocusedMarker>)>();
            for (window, focused) in query.iter(world) {
                if focused {
                    // Check that focus stayed on the first window.
                    assert_eq!(window.id(), 2);
                }
            }
        }

        if iteration == 1 {
            let origin = Origin::new(0, 0);
            let size = Size::new(TEST_WINDOW_WIDTH, TEST_WINDOW_HEIGHT);
            let window = MockWindow::new(
                3,
                IRect {
                    min: origin,
                    max: origin + size,
                },
                check_queue.clone(),
                app.clone(),
            );
            let window = Window::new(Box::new(window));
            world.trigger(SpawnWindowTrigger(vec![window]));
        }
    };

    let config: Config = r#"
[options]
[bindings]
[windows]
[windows.skipfocus]
title = ".*"
dont_focus = true
index = 100
"#
    .try_into()
    .unwrap();
    bevy.insert_resource(config);
    run_main_loop(&mut bevy, &internal_queue, &commands, check);
}

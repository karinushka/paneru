use std::sync::atomic::{AtomicU32, Ordering};
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

use crate::commands::{Command, Direction, Operation, ResizeDirection, register_commands};
use crate::config::{Config, MainOptions, WindowParams};
use crate::ecs::layout::LayoutStrip;
use crate::ecs::{
    BProcess, ExistingMarker, FocusFollowsMouse, FocusedMarker, Initializing, MissionControlActive,
    PollForNotifications, SkipReshuffle, SpawnWindowTrigger, Timeout, register_systems,
    register_triggers,
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

type TestWindowSpawner = Box<dyn Fn(WorkspaceId) -> Vec<Window> + Send + Sync + 'static>;

/// A mock implementation of the `WindowManagerApi` trait for testing purposes.
struct MockWindowManager {
    windows: TestWindowSpawner,
    workspaces: Vec<WorkspaceId>,
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
            false,
        );
        vec![(display, self.workspaces.clone())]
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

    fn is_fullscreen_space(&self, _display_id: CGDirectDisplayID) -> bool {
        false
    }

    /// Does nothing, as mouse centering is not tested at this level.
    #[instrument(level = Level::DEBUG, skip_all, ret)]
    fn warp_mouse(&self, _origin: Origin) {
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

    fn cursor_position(&self) -> Option<CGPoint> {
        None
    }

    #[instrument(level = Level::DEBUG, skip(self))]
    fn dim_windows(&self, windows: &[WinID], level: f32) {}
}

/// A mock implementation of the `WindowApi` trait for testing purposes.
#[derive(Debug)]
struct MockWindow {
    id: WinID,
    frame: IRect,
    horizontal_padding: i32,
    vertical_padding: i32,
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
    fn resize(&mut self, size: Size) {
        debug!("{}: id {} to {size}", function_name!(), self.id);
        self.frame.max = self.frame.min + size;
    }

    /// Always returns `Ok(())` for updating the frame.
    #[instrument(level = Level::DEBUG, skip(self), ret)]
    fn update_frame(&mut self) -> Result<IRect> {
        debug!("{}:", function_name!());
        Ok(self.frame)
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
        self.event_queue
            .write()
            .unwrap()
            .push(Event::WindowFocused { window_id: self.id });
        self.app.inner.force_write().focused_id = Some(self.id);
    }

    #[instrument(level = Level::TRACE, skip(self), ret)]
    fn pid(&self) -> Result<Pid> {
        Ok(TEST_PROCESS_ID)
    }

    #[instrument(level = Level::DEBUG, skip(self), ret)]
    fn set_padding(&mut self, padding: manager::WindowPadding) {
        match padding {
            manager::WindowPadding::Vertical(padding) => {
                let delta = padding - self.vertical_padding;
                self.frame.min.y -= delta;
                self.frame.max.y += delta;
                self.vertical_padding = padding;
            }
            manager::WindowPadding::Horizontal(padding) => {
                let delta = padding - self.horizontal_padding;
                self.frame.min.x -= delta;
                self.frame.max.x += delta;
                self.horizontal_padding = padding;
            }
        }
    }

    fn horizontal_padding(&self) -> i32 {
        self.horizontal_padding
    }

    fn vertical_padding(&self) -> i32 {
        self.vertical_padding
    }

    #[instrument(level = Level::TRACE, skip(self), ret)]
    fn is_minimized(&self) -> bool {
        self.minimized
    }

    fn is_full_screen(&self) -> bool {
        false
    }

    fn border_radius(&self) -> Option<f64> {
        None
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
            horizontal_padding: 0,
            vertical_padding: 0,
            app,
            event_queue,
            minimized: false,
        }
    }
}

#[test]
fn test_set_padding_expands_frame() {
    use crate::manager::WindowPadding;

    let psn = ProcessSerialNumber { high: 0, low: 0 };
    let app = MockApplication::new(psn, 1);
    let event_queue = Arc::new(RwLock::new(Vec::new()));

    // Window at (100, 50) with size (400, 300).
    let frame = IRect::new(100, 50, 500, 350);
    let mut window = MockWindow::new(1, frame, event_queue, app);

    assert_eq!(window.frame().width(), 400);
    assert_eq!(window.frame().height(), 300);

    // Setting horizontal padding should expand the frame by the padding on each side.
    window.set_padding(WindowPadding::Horizontal(8));
    assert_eq!(
        window.frame().min.x,
        92,
        "min.x should shift left by padding"
    );
    assert_eq!(
        window.frame().max.x,
        508,
        "max.x should shift right by padding"
    );
    assert_eq!(
        window.frame().width(),
        416,
        "width should grow by 2 * padding"
    );

    // Setting vertical padding should expand the frame vertically.
    window.set_padding(WindowPadding::Vertical(5));
    assert_eq!(window.frame().min.y, 45, "min.y should shift up by padding");
    assert_eq!(
        window.frame().max.y,
        355,
        "max.y should shift down by padding"
    );
    assert_eq!(
        window.frame().height(),
        310,
        "height should grow by 2 * padding"
    );

    // Changing padding from 8 to 12 should only expand by the delta (4).
    window.set_padding(WindowPadding::Horizontal(12));
    assert_eq!(window.frame().min.x, 88);
    assert_eq!(window.frame().max.x, 512);
    assert_eq!(window.frame().width(), 424);
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

fn verify_window_sizes(expected_sizes: &[(WinID, (i32, i32))], world: &mut World) {
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

fn window_spawner(
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

#[test]
#[allow(clippy::too_many_lines)]
fn test_window_shuffle() {
    const PADDING_LEFT: u16 = 3;
    const PADDING_RIGHT: u16 = 5;
    const PADDING_TOP: u16 = 7;
    const PADDING_BOTTOM: u16 = 9;
    const SLIVER_WIDTH: u16 = 5;
    const H_PAD: i32 = 2;

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
        Event::Command {
            command: Command::PrintState,
        },
    ];

    // Logical width includes padding expansion on each side.
    let logical_width = TEST_WINDOW_WIDTH + 2 * H_PAD;
    let top_edge = TEST_MENUBAR_HEIGHT + i32::from(PADDING_TOP);
    let left_edge = i32::from(PADDING_LEFT);
    let right_edge = TEST_DISPLAY_WIDTH - i32::from(PADDING_RIGHT);
    // Offscreen positions: viewport already includes edge padding.
    // Formula from position_layout_windows:
    //   left:  viewport.min.x - width + sliver - pad_left + h_pad
    //   right: viewport.max.x - sliver + pad_right - h_pad
    let offscreen_right = right_edge - i32::from(SLIVER_WIDTH) + i32::from(PADDING_RIGHT) - H_PAD;
    let offscreen_left =
        left_edge - logical_width + i32::from(SLIVER_WIDTH) - i32::from(PADDING_LEFT) + H_PAD;
    let centered = (TEST_DISPLAY_WIDTH - logical_width) / 2;

    let expected_positions_last = [
        (4, (offscreen_left, top_edge)),
        (3, (offscreen_left, top_edge)),
        (2, (right_edge - 3 * logical_width, top_edge)),
        (1, (right_edge - 2 * logical_width, top_edge)),
        (0, (right_edge - logical_width, top_edge)),
    ];
    let expected_positions_first = [
        (4, (left_edge, top_edge)),
        (3, (left_edge + logical_width, top_edge)),
        (2, (left_edge + 2 * logical_width, top_edge)),
        (1, (offscreen_right, top_edge)),
        (0, (offscreen_right, top_edge)),
    ];

    let expected_positions_stacked = [
        (4, (centered, top_edge)),
        (3, (centered, 393)),
        (2, (centered + logical_width, top_edge)),
        (1, (offscreen_right, top_edge)),
        (0, (offscreen_right, top_edge)),
    ];
    let expected_positions_stacked2 = [
        (4, (centered, top_edge)),
        (3, (centered, 271)),
        (2, (centered, 515)),
        (1, (centered + logical_width, top_edge)),
        (0, (offscreen_right, top_edge)),
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
    let windows = window_spawner(5, event_queue, mock_app);
    let window_manager = MockWindowManager {
        windows,
        workspaces: vec![TEST_WORKSPACE_ID],
    };
    bevy.world_mut()
        .insert_resource(WindowManager(Box::new(window_manager)));

    let mut params = WindowParams::new(".*", None);
    params.vertical_padding = Some(3);
    params.horizontal_padding = Some(2);
    let config: Config = (
        MainOptions {
            padding_left: Some(PADDING_LEFT),
            padding_right: Some(PADDING_RIGHT),
            padding_top: Some(PADDING_TOP),
            padding_bottom: Some(PADDING_BOTTOM),
            ..Default::default()
        },
        vec![params],
    )
        .into();
    bevy.insert_resource(config);

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
    let windows = window_spawner(5, event_queue, mock_app);
    let window_manager = MockWindowManager {
        windows,
        workspaces: vec![TEST_WORKSPACE_ID],
    };
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
    let windows = window_spawner(3, event_queue, mock_app);
    let window_manager = MockWindowManager {
        windows,
        workspaces: vec![TEST_WORKSPACE_ID],
    };
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

    let mut params = WindowParams::new(".*", None);
    params.dont_focus = Some(true);
    params.index = Some(100);
    let config: Config = (MainOptions::default(), vec![params]).into();
    bevy.insert_resource(config);

    run_main_loop(&mut bevy, &internal_queue, &commands, check);
}

/// Off-screen windows should keep the same height as on-screen windows
/// when `sliver_height` is 1.0 (the default). A previous bug subtracted
/// `menubar_height` from off-screen window heights, causing a visible
/// resize when they came into focus.
#[test]
fn test_offscreen_windows_preserve_height() {
    let expected_height = TEST_DISPLAY_HEIGHT - TEST_MENUBAR_HEIGHT;

    let commands = vec![
        Event::MenuOpened { window_id: 0 }, // Settle
        Event::Command {
            command: Command::Window(Operation::Focus(Direction::First)),
        },
    ];

    let expected_sizes = [
        (4, (TEST_WINDOW_WIDTH, expected_height)),
        (3, (TEST_WINDOW_WIDTH, expected_height)),
        (2, (TEST_WINDOW_WIDTH, expected_height)),
        (1, (TEST_WINDOW_WIDTH, expected_height)),
        (0, (TEST_WINDOW_WIDTH, expected_height)),
    ];

    let check = |iteration, world: &mut World| {
        if iteration == 1 {
            verify_window_sizes(&expected_sizes, world);
        }
    };

    let mut bevy = setup_world();
    let mock_app = setup_process(bevy.world_mut());
    let internal_queue = Arc::new(RwLock::new(Vec::<Event>::new()));
    let event_queue = internal_queue.clone();
    let windows = window_spawner(5, event_queue, mock_app);
    let window_manager = MockWindowManager {
        windows,
        workspaces: vec![TEST_WORKSPACE_ID],
    };
    bevy.world_mut()
        .insert_resource(WindowManager(Box::new(window_manager)));

    run_main_loop(&mut bevy, &internal_queue, &commands, check);
}

/// When `sliver_width` is smaller than `edge_padding`, the off-screen
/// sliver must still be exactly `sliver_width` pixels from the real
/// display edge. A previous bug used `max(sliver, pad) - pad`, which
/// collapsed the sliver to `edge_padding` pixels when `pad > sliver`.
#[test]
fn test_sliver_smaller_than_edge_padding() {
    const PADDING: u16 = 8;
    const SLIVER: u16 = 1;

    let commands = vec![
        Event::MenuOpened { window_id: 0 }, // Settle
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
    // With sliver < padding, off-screen positions are measured from
    // the real display edge, so they go *into* the padding zone.
    let offscreen_right = TEST_DISPLAY_WIDTH - i32::from(SLIVER);
    let offscreen_left = i32::from(SLIVER) - TEST_WINDOW_WIDTH;

    let left_edge = i32::from(PADDING);

    // Focus first: windows 4,3 on-screen, 2 partial, 1,0 off-screen right.
    let expected_first = [
        (4, (left_edge, top_edge)),
        (3, (left_edge + TEST_WINDOW_WIDTH, top_edge)),
        (2, (left_edge + 2 * TEST_WINDOW_WIDTH, top_edge)),
        (1, (offscreen_right, top_edge)),
        (0, (offscreen_right, top_edge)),
    ];

    // Focus last: windows 0,1 on-screen, 2 partial, 3,4 off-screen left.
    let expected_last = [
        (4, (offscreen_left, top_edge)),
        (3, (offscreen_left, top_edge)),
        (2, (right_edge - 3 * TEST_WINDOW_WIDTH, top_edge)),
        (1, (right_edge - 2 * TEST_WINDOW_WIDTH, top_edge)),
        (0, (right_edge - TEST_WINDOW_WIDTH, top_edge)),
    ];

    let check = |iteration, world: &mut World| {
        if iteration == 2 {
            verify_window_positions(&expected_first, world);
        } else if iteration == 3 {
            verify_window_positions(&expected_last, world);
        }
    };

    let mut bevy = setup_world();
    let mock_app = setup_process(bevy.world_mut());
    let internal_queue = Arc::new(RwLock::new(Vec::<Event>::new()));
    let event_queue = internal_queue.clone();
    let windows = window_spawner(5, event_queue, mock_app);
    let window_manager = MockWindowManager {
        windows,
        workspaces: vec![TEST_WORKSPACE_ID],
    };
    bevy.world_mut()
        .insert_resource(WindowManager(Box::new(window_manager)));

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
    bevy.insert_resource(config);

    run_main_loop(&mut bevy, &internal_queue, &commands, check);
}

#[test]
fn test_window_resize_grow_and_shrink_cycle() {
    let commands = vec![
        Event::MenuOpened { window_id: 0 }, // Noop allowing everything to settle
        Event::Command {
            command: Command::Window(Operation::Resize(ResizeDirection::Grow)),
        },
        Event::Command {
            command: Command::Window(Operation::Resize(ResizeDirection::Grow)),
        },
        Event::Command {
            command: Command::Window(Operation::Resize(ResizeDirection::Grow)),
        },
        Event::Command {
            command: Command::Window(Operation::Resize(ResizeDirection::Shrink)),
        },
    ];

    let expected_widths = [None, Some(512), Some(768), Some(256), Some(768)];

    let check = |iteration, world: &mut World| {
        let Some(expected_width) = expected_widths[iteration] else {
            return;
        };
        let mut query = world.query::<&Window>();
        let window = query
            .iter(world)
            .find(|window| window.id() == 0)
            .expect("expected window with id 0");
        assert_eq!(
            window.frame().width(),
            expected_width,
            "iteration {iteration}: expected width {expected_width}, got {}",
            window.frame().width()
        );
    };

    let mut bevy = setup_world();
    let mock_app = setup_process(bevy.world_mut());
    let internal_queue = Arc::new(RwLock::new(Vec::<Event>::new()));
    let event_queue = internal_queue.clone();
    let windows = window_spawner(1, event_queue, mock_app);
    let window_manager = MockWindowManager {
        windows,
        workspaces: vec![TEST_WORKSPACE_ID],
    };
    bevy.world_mut()
        .insert_resource(WindowManager(Box::new(window_manager)));

    let config: Config = (
        MainOptions {
            preset_column_widths: vec![0.25, 0.5, 0.75],
            ..Default::default()
        },
        vec![],
    )
        .into();
    bevy.insert_resource(config);

    run_main_loop(&mut bevy, &internal_queue, &commands, check);
}

#[test]
fn test_scrolling() {
    let mut bevy = setup_world();
    let mock_app = setup_process(bevy.world_mut());
    let internal_queue = Arc::new(RwLock::new(Vec::<Event>::new()));
    let event_queue = internal_queue.clone();
    let windows = window_spawner(3, event_queue, mock_app);
    let window_manager = MockWindowManager {
        windows,
        workspaces: vec![TEST_WORKSPACE_ID],
    };
    bevy.world_mut()
        .insert_resource(WindowManager(Box::new(window_manager)));

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
        Event::Swipe {
            deltas: vec![0.1, 0.1, 0.1],
        },
        Event::Command {
            command: Command::PrintState,
        },
    ];

    // Verify initial positions
    let expected_initial = [
        (2, (0, TEST_MENUBAR_HEIGHT)),
        (1, (400, TEST_MENUBAR_HEIGHT)),
        (0, (800, TEST_MENUBAR_HEIGHT)),
    ];

    let expected = [
        (2, (-395, TEST_MENUBAR_HEIGHT)),
        (1, (-395, TEST_MENUBAR_HEIGHT)),
        (0, (0, TEST_MENUBAR_HEIGHT)),
    ];

    let check = |iteration, world: &mut World| {
        let iterations = [
            None,
            None,
            None,
            Some(expected_initial.as_slice()),
            None,
            Some(expected.as_slice()),
        ];

        if let Some(positions) = iterations[iteration] {
            debug!("Iteration: {iteration}");
            verify_window_positions(positions, world);
        }
    };

    let config: Config = (
        MainOptions {
            swipe_gesture_fingers: Some(3),
            ..Default::default()
        },
        vec![],
    )
        .into();
    bevy.insert_resource(config);

    run_main_loop(&mut bevy, &internal_queue, &commands, check);
}

#[test]
fn test_multi_display_lifecycle() {
    let mut bevy = setup_world();
    let mock_app = setup_process(bevy.world_mut());
    let internal_queue = Arc::new(RwLock::new(Vec::<Event>::new()));
    let event_queue = internal_queue.clone();
    let windows = window_spawner(1, event_queue, mock_app);
    let window_manager = MockWindowManager {
        windows,
        workspaces: vec![TEST_WORKSPACE_ID],
    };
    bevy.insert_resource(WindowManager(Box::new(window_manager)));
    bevy.insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_millis(
        500,
    )));

    let commands = vec![
        Event::MenuOpened { window_id: 0 }, // Noop allowing everything to settle
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

    let check = |iteration, world: &mut World| {
        match iteration {
            1 => {
                // Initial state check: 1 display, 1 workspace attached.
                let _display_entity = {
                    let mut query = world.query_filtered::<Entity, With<Display>>();
                    query.single(world).expect("should have one display")
                };
            }

            2 => {
                // Verify the display is gone and the workspace is orphaned.
                assert!(
                    world
                        .query_filtered::<Entity, With<Display>>()
                        .single(world)
                        .is_err(),
                    "display should be despawned"
                );

                {
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
                }
            }

            3 => {
                // Verify the display is back and the workspace is re-parented.
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
                let child_of = workspace
                    .get::<ChildOf>()
                    .expect("re-parented workspace should have a parent");
                assert_eq!(
                    child_of.parent(),
                    new_display_entity,
                    "workspace should be child of the new display"
                );
            }

            _ => {}
        }
    };

    run_main_loop(&mut bevy, &internal_queue, &commands, check);
}

#[test]
fn test_multi_workspace_orphaning() {
    let mut bevy = setup_world();
    let mock_app = setup_process(bevy.world_mut());
    let internal_queue = Arc::new(RwLock::new(Vec::<Event>::new()));
    let event_queue = internal_queue.clone();
    let windows = window_spawner(1, event_queue, mock_app);
    let window_manager = MockWindowManager {
        windows,
        workspaces: vec![TEST_WORKSPACE_ID, TEST_WORKSPACE_ID + 1],
    };
    bevy.world_mut()
        .insert_resource(WindowManager(Box::new(window_manager)));

    let commands = vec![
        Event::MenuOpened { window_id: 0 }, // Noop allowing everything to settle
        Event::Command {
            command: Command::PrintState,
        },
        Event::DisplayRemoved {
            display_id: TEST_DISPLAY_ID,
        },
    ];

    let check = |iteration, world: &mut World| {
        let workspace_entities = world
            .query_filtered::<Entity, With<LayoutStrip>>()
            .iter(world)
            .collect::<Vec<_>>();
        match iteration {
            1 => {
                // Verify initial state: 1 display, 2 workspaces.
                let display_entity = world
                    .query_filtered::<Entity, With<Display>>()
                    .single(world)
                    .expect("should have one display");

                assert_eq!(workspace_entities.len(), 2, "should have two workspaces");

                for &ws in &workspace_entities {
                    let child_of = world
                        .entity(ws)
                        .get::<ChildOf>()
                        .expect("workspace should have parent");
                    assert_eq!(child_of.parent(), display_entity);
                }
            }
            2 => {
                // Verify both workspaces are orphaned.
                for &ws in &workspace_entities {
                    let entity = world.entity(ws);
                    assert!(
                        entity.get::<Timeout>().is_some(),
                        "each workspace should have a timeout"
                    );
                    assert!(
                        entity.get::<ChildOf>().is_none(),
                        "each workspace should have no parent"
                    );
                }
            }
            _ => {}
        }
    };

    run_main_loop(&mut bevy, &internal_queue, &commands, check);
}

#[test]
fn test_window_hidden_ratio() {
    let mut bevy = setup_world();
    let mock_app = setup_process(bevy.world_mut());
    let internal_queue = Arc::new(RwLock::new(Vec::<Event>::new()));
    let event_queue = internal_queue.clone();
    let windows = window_spawner(2, event_queue, mock_app);
    let window_manager = MockWindowManager {
        windows,
        workspaces: vec![TEST_WORKSPACE_ID],
    };
    bevy.world_mut()
        .insert_resource(WindowManager(Box::new(window_manager)));

    // Set hidden ratio to 0.5 (tolerate up to 50% hidden)
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
    bevy.insert_resource(config);

    let commands = vec![
        Event::MenuOpened { window_id: 0 }, // Settle, window 1 is focused at x=0
        // Swipe left slightly.
        Event::Swipe {
            deltas: vec![0.1, 0.1, 0.1],
        },
        // Now focus it again. It SHOULD NOT move back to x=0.
        Event::Command {
            command: Command::Window(Operation::Focus(Direction::First)),
        },
    ];

    let check = |iteration, world: &mut World| {
        if iteration == 2 {
            let mut query = world.query::<&Window>();
            let window = query.iter(world).find(|w| w.id() == 1).unwrap();
            // Should still be off-screen.
            assert_ne!(window.frame().min.x, 0);
            assert!(window.frame().min.x < 0);
        }
    };

    run_main_loop(&mut bevy, &internal_queue, &commands, check);
}

#[test]
fn test_window_hidden_ratio_swap() {
    let mut bevy = setup_world();
    let mock_app = setup_process(bevy.world_mut());
    let internal_queue = Arc::new(RwLock::new(Vec::<Event>::new()));
    let event_queue = internal_queue.clone();
    let windows = window_spawner(5, event_queue, mock_app);
    let window_manager = MockWindowManager {
        windows,
        workspaces: vec![TEST_WORKSPACE_ID],
    };
    bevy.world_mut()
        .insert_resource(WindowManager(Box::new(window_manager)));

    // Set hidden ratio to 1.0 (never move unless fully hidden)
    // and high animation speed for instant results.
    let config: Config = (
        MainOptions {
            window_hidden_ratio: Some(1.0),
            animation_speed: Some(10000.0),
            ..Default::default()
        },
        vec![],
    )
        .into();
    bevy.insert_resource(config);

    let commands = vec![
        Event::MenuOpened { window_id: 0 }, // Settle, window 1 is focused at x=0
        // Focus second window (id 0). It's at x=400 initially.
        // It's 100% visible, so with hidden_ratio=1.0 it won't move.
        Event::Command {
            command: Command::Window(Operation::Center),
        },
        Event::Command {
            command: Command::Window(Operation::Swap(Direction::Last)),
        },
    ];

    let check = |iteration, world: &mut World| {
        let centered = (TEST_DISPLAY_WIDTH - TEST_WINDOW_WIDTH) / 2;
        if iteration == 1 {
            let mut query = world.query::<&Window>();
            let window = query.iter(world).find(|w| w.id() == 4).unwrap();
            // Should still be at 400 because 0% hidden < 1.0 ratio
            assert_eq!(window.frame().min.x, centered);
        }
        if iteration == 2 {
            let mut query = world.query::<&Window>();
            let window = query.iter(world).find(|w| w.id() == 4).unwrap();
            assert_eq!(window.frame().min.x, centered);
            let window = query.iter(world).find(|w| w.id() == 0).unwrap();
            // The tail of the strip (window 0) is now to the left of window 4.
            assert_eq!(window.frame().min.x, centered - TEST_WINDOW_WIDTH);
        }
    };

    run_main_loop(&mut bevy, &internal_queue, &commands, check);
}

/// Verify that focus state is on the expected window.
fn verify_focused_window(expected_id: WinID, world: &mut World) {
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

/// Rapid focus keypresses should not get swallowed. When pressing West
/// three times from window 0 (rightmost), focus should land on window 3
/// — each press should advance one step even when the OS event
/// round-trip hasn't completed yet.
///
/// Simulates the race by writing all three commands as messages in one
/// frame before any Bevy update runs, so `FocusedMarker` cannot catch
/// up via mock events between presses.
#[test]
fn test_rapid_focus_not_swallowed() {
    // Phase 1: settle + move focus to last window via normal loop.
    let setup_commands = vec![
        Event::MenuOpened { window_id: 0 },
        Event::Command {
            command: Command::Window(Operation::Focus(Direction::Last)),
        },
    ];

    let check_setup = |iteration, world: &mut World| {
        if iteration == 1 {
            verify_focused_window(0, world);
        }
    };

    let mut bevy = setup_world();
    let mock_app = setup_process(bevy.world_mut());
    let internal_queue = Arc::new(RwLock::new(Vec::<Event>::new()));
    let event_queue = internal_queue.clone();
    let windows = window_spawner(5, event_queue, mock_app);
    let window_manager = MockWindowManager {
        windows,
        workspaces: vec![TEST_WORKSPACE_ID],
    };
    bevy.world_mut()
        .insert_resource(WindowManager(Box::new(window_manager)));

    run_main_loop(&mut bevy, &internal_queue, &setup_commands, check_setup);

    // Phase 2: send three Focus(West) commands, one per frame, but do
    // NOT flush mock events between frames. This simulates keypresses
    // arriving faster than the OS event round-trip can deliver
    // ApplicationFrontSwitched / WindowFocused back to the ECS.
    // Without the immediate FocusedMarker update in command_move_focus,
    // each press would re-target the same window (focus swallowed).
    let focus_west = Event::Command {
        command: Command::Window(Operation::Focus(Direction::West)),
    };
    for _ in 0..3 {
        bevy.world_mut().write_message::<Event>(focus_west.clone());
        bevy.update();
        // Deliberately skip flushing internal_queue — mock events from
        // focus_with_raise stay queued, simulating OS event delay.
    }

    // After three West presses from window 0 (strip order: [4,3,2,1,0]):
    //   0 → 1 → 2 → 3. Focus should be on window 3.
    verify_focused_window(3, bevy.world_mut());
}

/// A stale `WindowFocused` event arriving after focus has moved on should
/// not pull `FocusedMarker` back to the old window.
#[test]
fn test_stale_focus_event_ignored() {
    let commands = vec![
        Event::MenuOpened { window_id: 0 }, // Settle
        Event::Command {
            command: Command::Window(Operation::Focus(Direction::East)),
        },
        // Inject a stale WindowFocused for window 4 (the old focused window)
        // after focus has already moved to window 3.
        Event::WindowFocused { window_id: 4 },
        Event::Command {
            command: Command::PrintState,
        },
    ];

    let check = |iteration, world: &mut World| {
        if iteration == 1 {
            // After Focus(East): strip is [4,3,2,1,0], started at 4, moved to 3.
            verify_focused_window(3, world);
        }
        if iteration == 3 {
            // After the stale event, focus should STILL be on window 3.
            verify_focused_window(3, world);
        }
    };

    let mut bevy = setup_world();
    let mock_app = setup_process(bevy.world_mut());
    let internal_queue = Arc::new(RwLock::new(Vec::<Event>::new()));
    let event_queue = internal_queue.clone();
    let windows = window_spawner(5, event_queue, mock_app);
    let window_manager = MockWindowManager {
        windows,
        workspaces: vec![TEST_WORKSPACE_ID],
    };
    bevy.world_mut()
        .insert_resource(WindowManager(Box::new(window_manager)));

    run_main_loop(&mut bevy, &internal_queue, &commands, check);
}

// ---------------------------------------------------------------------------
// Multi-display: switching focus to a shorter display must not shrink windows
// on the taller display.
// ---------------------------------------------------------------------------

const EXT_DISPLAY_ID: u32 = 2;
const EXT_WORKSPACE_ID: u64 = 20;
const EXT_DISPLAY_WIDTH: i32 = 1920;
const EXT_DISPLAY_HEIGHT: i32 = 1200;

/// Mock window manager with two displays of different heights.
/// The active display can be switched via the shared `AtomicU32`.
struct TwoDisplayMock {
    windows: TestWindowSpawner,
    active_display: Arc<AtomicU32>,
}

impl std::fmt::Debug for TwoDisplayMock {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TwoDisplayMock").finish()
    }
}

impl WindowManagerApi for TwoDisplayMock {
    fn new_application(&self, process: &dyn ProcessApi) -> Result<Application> {
        Ok(Application::new(Box::new(MockApplication {
            inner: Arc::new(RwLock::new(InnerMockApplication {
                psn: process.psn(),
                pid: process.pid(),
                focused_id: None,
            })),
        })))
    }

    fn get_associated_windows(&self, _window_id: WinID) -> Vec<WinID> {
        vec![]
    }

    fn present_displays(&self) -> Vec<(Display, Vec<WorkspaceId>)> {
        // External display sits above the internal one.
        let ext = Display::new(
            EXT_DISPLAY_ID,
            IRect::new(0, -EXT_DISPLAY_HEIGHT, EXT_DISPLAY_WIDTH, 0),
            TEST_MENUBAR_HEIGHT,
            false,
        );
        let int = Display::new(
            TEST_DISPLAY_ID,
            IRect::new(0, 0, TEST_DISPLAY_WIDTH, TEST_DISPLAY_HEIGHT),
            TEST_MENUBAR_HEIGHT,
            false,
        );
        vec![
            (ext, vec![EXT_WORKSPACE_ID]),
            (int, vec![TEST_WORKSPACE_ID]),
        ]
    }

    fn active_display_id(&self) -> Result<u32> {
        Ok(self.active_display.load(Ordering::Relaxed))
    }

    fn active_display_space(&self, display_id: CGDirectDisplayID) -> Result<WorkspaceId> {
        if display_id == EXT_DISPLAY_ID {
            Ok(EXT_WORKSPACE_ID)
        } else {
            Ok(TEST_WORKSPACE_ID)
        }
    }

    fn is_fullscreen_space(&self, _display_id: CGDirectDisplayID) -> bool {
        false
    }

    fn warp_mouse(&self, _origin: Origin) {}

    fn find_existing_application_windows(
        &self,
        _app: &mut Application,
        spaces: &[WorkspaceId],
    ) -> Result<(Vec<Window>, Vec<WinID>)> {
        let windows = spaces
            .iter()
            .flat_map(|workspace_id| (self.windows)(*workspace_id))
            .collect();
        Ok((windows, vec![]))
    }

    fn find_window_at_point(&self, _point: &CGPoint) -> Result<WinID> {
        Ok(0)
    }

    fn windows_in_workspace(&self, workspace_id: WorkspaceId) -> Result<Vec<WinID>> {
        Ok((self.windows)(workspace_id)
            .iter()
            .map(|w| w.id())
            .collect())
    }

    fn quit(&self) -> Result<()> {
        Ok(())
    }

    fn setup_config_watcher(&self, _path: &std::path::Path) -> Result<Box<dyn notify::Watcher>> {
        todo!()
    }

    fn cursor_position(&self) -> Option<CGPoint> {
        None
    }

    fn dim_windows(&self, _windows: &[WinID], _level: f32) {}
}

/// Regression test: switching focus to a shorter (internal) display must not
/// resize windows on the taller (external) display.  Before the fix,
/// `layout_strip_changed` used the active display's viewport height for ALL
/// strips, so the external strip's windows shrank to the internal height.
#[test]
fn test_multi_display_no_height_crosstalk() {
    let mut bevy = setup_world();
    let mock_app = setup_process(bevy.world_mut());
    let internal_queue = Arc::new(RwLock::new(Vec::<Event>::new()));
    let event_queue = internal_queue.clone();

    let active_display = Arc::new(AtomicU32::new(EXT_DISPLAY_ID));

    // External display gets one window (id 100), internal gets one (id 200).
    let eq1 = event_queue.clone();
    let eq2 = event_queue.clone();
    let app1 = mock_app.clone();
    let app2 = mock_app;
    let windows: TestWindowSpawner = Box::new(move |workspace_id| {
        if workspace_id == EXT_WORKSPACE_ID {
            let origin = Origin::new(0, 0);
            let size = Size::new(TEST_WINDOW_WIDTH, TEST_WINDOW_HEIGHT);
            vec![Window::new(Box::new(MockWindow::new(
                100,
                IRect::from_corners(origin, origin + size),
                eq1.clone(),
                app1.clone(),
            )))]
        } else if workspace_id == TEST_WORKSPACE_ID {
            let origin = Origin::new(0, 0);
            let size = Size::new(TEST_WINDOW_WIDTH, TEST_WINDOW_HEIGHT);
            vec![Window::new(Box::new(MockWindow::new(
                200,
                IRect::from_corners(origin, origin + size),
                eq2.clone(),
                app2.clone(),
            )))]
        } else {
            vec![]
        }
    });

    let window_manager = TwoDisplayMock {
        windows,
        active_display: active_display.clone(),
    };
    bevy.insert_resource(WindowManager(Box::new(window_manager)));

    // Expected height on the external display = display height - menubar.
    let ext_usable_height = EXT_DISPLAY_HEIGHT - TEST_MENUBAR_HEIGHT;

    let commands = vec![
        // 0: Settle — let initialization complete.
        Event::MenuOpened { window_id: 100 },
        // 1: Print to verify initial layout.
        Event::Command {
            command: Command::PrintState,
        },
        // 2: Simulate switching focus to the internal display.
        //    The mock's active_display_id will have been flipped in the
        //    verifier at iteration 1, and DisplayChanged triggers the
        //    ActiveDisplayMarker move + workspace switch.
        Event::DisplayChanged,
        // 3: Noop — the verifier for iteration 2 marks the external strip
        //    as Changed, simulating any mutation (window add/remove/tab-switch)
        //    that would touch the strip after a display switch.
        //    This iteration's updates run layout_strip_changed on it.
        Event::MenuOpened { window_id: 100 },
        // 4: Print to verify final layout.
        Event::Command {
            command: Command::PrintState,
        },
    ];

    let ad = active_display.clone();
    let check = move |iteration, world: &mut World| {
        match iteration {
            1 => {
                // After settling, external window should have the external
                // display's usable height.
                verify_window_sizes(&[(100, (TEST_WINDOW_WIDTH, ext_usable_height))], world);

                // Now switch the mock's active display so the next
                // DisplayChanged event picks it up.
                ad.store(TEST_DISPLAY_ID, Ordering::Relaxed);
            }
            2 => {
                // After the display switch, simulate a strip mutation on
                // the non-active (external) display.  In practice this
                // happens when window_focused_trigger, window_removal, or
                // active_workspace_trigger touch the strip via DerefMut.
                use crate::ecs::ActiveWorkspaceMarker;
                let mut strip_query =
                    world.query_filtered::<&mut LayoutStrip, Without<ActiveWorkspaceMarker>>();
                // `iter_mut` yields `Mut<LayoutStrip>` — dereferencing
                // mutably triggers Bevy's `Changed` detection.
                for mut strip in strip_query.iter_mut(world) {
                    strip.set_changed();
                }
            }
            4 => {
                // After layout_strip_changed ran on the Changed external
                // strip, window 100 must still have the external display's
                // height — NOT the internal display's shorter height.
                verify_window_sizes(&[(100, (TEST_WINDOW_WIDTH, ext_usable_height))], world);
            }
            _ => {}
        }
    };

    run_main_loop(&mut bevy, &internal_queue, &commands, check);
}

/// Verify that `to_next_display` inserts the moved window into the target
/// display's strip instead of leaving it unmanaged ("Remaining").
#[test]
fn test_next_display_inserts_into_target_strip() {
    let mut bevy = setup_world();
    let mock_app = setup_process(bevy.world_mut());
    let internal_queue = Arc::new(RwLock::new(Vec::<Event>::new()));
    let event_queue = internal_queue.clone();

    let active_display = Arc::new(AtomicU32::new(EXT_DISPLAY_ID));

    // External display gets one window (id 100), internal display has none.
    let eq = event_queue.clone();
    let app = mock_app;
    let windows: TestWindowSpawner = Box::new(move |workspace_id| {
        if workspace_id == EXT_WORKSPACE_ID {
            let origin = Origin::new(0, 0);
            let size = Size::new(TEST_WINDOW_WIDTH, TEST_WINDOW_HEIGHT);
            vec![Window::new(Box::new(MockWindow::new(
                100,
                IRect::from_corners(origin, origin + size),
                eq.clone(),
                app.clone(),
            )))]
        } else {
            vec![]
        }
    });

    let window_manager = TwoDisplayMock {
        windows,
        active_display: active_display.clone(),
    };
    bevy.insert_resource(WindowManager(Box::new(window_manager)));

    let commands = vec![
        // 0: Settle.
        Event::MenuOpened { window_id: 100 },
        // 1: Print initial state.
        Event::Command {
            command: Command::PrintState,
        },
        // 2: Move focused window to the other display.
        Event::Command {
            command: Command::Window(Operation::ToNextDisplay),
        },
        // 3: Print final state for debugging.
        Event::Command {
            command: Command::PrintState,
        },
    ];

    let check = move |iteration, world: &mut World| {
        match iteration {
            1 => {
                // Window 100 should be on the external display's strip.
                let entity = find_window_entity(100, world);
                let mut strip_query = world.query::<&LayoutStrip>();
                let in_ext = strip_query
                    .iter(world)
                    .any(|strip| strip.id() == EXT_WORKSPACE_ID && strip.index_of(entity).is_ok());
                assert!(
                    in_ext,
                    "window 100 should be in the external strip before move"
                );
            }
            2 => {
                // After ToNextDisplay, window 100 must be in the target strip.
                let entity = find_window_entity(100, world);
                let mut strip_query = world.query::<&LayoutStrip>();
                let in_target = strip_query
                    .iter(world)
                    .any(|strip| strip.id() == TEST_WORKSPACE_ID && strip.index_of(entity).is_ok());
                let in_source = strip_query
                    .iter(world)
                    .any(|strip| strip.id() == EXT_WORKSPACE_ID && strip.index_of(entity).is_ok());
                assert!(
                    in_target,
                    "window 100 should be in the target (internal) strip after nextdisplay"
                );
                assert!(
                    !in_source,
                    "window 100 should NOT be in the source (external) strip after nextdisplay"
                );
            }
            _ => {}
        }
    };

    run_main_loop(&mut bevy, &internal_queue, &commands, check);
}

fn find_window_entity(window_id: WinID, world: &mut World) -> Entity {
    let mut query = world.query::<(&Window, Entity)>();
    query
        .iter(world)
        .find(|(w, _)| w.id() == window_id)
        .map(|(_, e)| e)
        .unwrap_or_else(|| panic!("window {window_id} not found"))
}

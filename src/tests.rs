use std::sync::{Arc, RwLock};
use std::time::Duration;

use super::*;
use bevy::prelude::*;
use bevy::time::{TimePlugin, TimeUpdateStrategy};
use log::debug;
use objc2_core_foundation::{CFRetained, CFString, CGPoint, CGRect, CGSize};
use objc2_core_graphics::CGDirectDisplayID;
use stdext::function_name;
use stdext::prelude::RwLockExt;

use crate::commands::{Command, Direction, Operation, process_command_trigger};
use crate::config::Config;
use crate::ecs::{
    ActiveDisplayMarker, ActiveWorkspaceMarker, BProcess, FocusFollowsMouse, FocusedMarker,
    MissionControlActive, PollForNotifications, SkipReshuffle, register_systems, register_triggers,
};
use crate::errors::{Error, Result};
use crate::events::Event;
use crate::manager::{
    Application, ApplicationApi, Display, LayoutStrip, ProcessApi, Window, WindowApi,
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
struct MockProcess {
    psn: ProcessSerialNumber,
}

impl ProcessApi for MockProcess {
    /// Always returns `true`, indicating the mock process is observable.
    fn is_observable(&mut self) -> bool {
        println!("{}:", function_name!());
        true
    }

    /// Returns a static name for the mock process.
    fn name(&self) -> &'static str {
        "test"
    }

    /// Returns a predefined PID for the mock process.
    fn pid(&self) -> Pid {
        println!("{}:", function_name!());
        TEST_PROCESS_ID
    }

    /// Returns the `ProcessSerialNumber` of the mock process.
    fn psn(&self) -> ProcessSerialNumber {
        println!("{}: {:?}", function_name!(), self.psn);
        self.psn
    }

    /// Always returns `None` for the `NSRunningApplication`.
    fn application(&self) -> Option<objc2::rc::Retained<objc2_app_kit::NSRunningApplication>> {
        println!("{}:", function_name!());
        None
    }

    /// Always returns `true`, indicating the mock process is ready.
    fn ready(&mut self) -> bool {
        println!("{}:", function_name!());
        true
    }
}

/// A mock implementation of the `ApplicationApi` trait for testing purposes.
/// It internally holds an `InnerMockApplication` within an `Arc<RwLock>`.
#[derive(Clone)]
struct MockApplication {
    inner: Arc<RwLock<InnerMockApplication>>,
}

/// The inner state of `MockApplication`, containing process serial number, PID, and focused window ID.
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
    fn pid(&self) -> Pid {
        println!("{}:", function_name!());
        self.inner.force_read().pid
    }

    /// Returns the `ProcessSerialNumber` of the mock application.
    fn psn(&self) -> ProcessSerialNumber {
        println!("{}:", function_name!());
        self.inner.force_read().psn
    }

    /// Always returns `Some(0)` for the connection ID.
    fn connection(&self) -> Option<ConnID> {
        println!("{}:", function_name!());
        Some(0)
    }

    /// Returns the currently focused window ID for the mock application.
    ///
    /// # Returns
    ///
    /// `Ok(WinID)` if a window is focused, otherwise `Err(Error::InvalidWindow)`.
    fn focused_window_id(&self) -> Result<WinID> {
        let id = self
            .inner
            .force_read()
            .focused_id
            .ok_or(Error::InvalidWindow);
        println!("{}: {id:?}", function_name!());
        id
    }

    /// Always returns an empty vector of window lists for the mock application.
    fn window_list(&self) -> Vec<Window> {
        println!("{}:", function_name!());
        vec![]
    }

    /// Always returns `Ok(true)` for observe operations on the mock application.
    fn observe(&mut self) -> Result<bool> {
        println!("{}:", function_name!());
        Ok(true)
    }

    /// Always returns `Ok(true)` for observe window operations on the mock application.
    fn observe_window(&mut self, _window: &Window) -> Result<bool> {
        println!("{}:", function_name!());
        Ok(true)
    }

    /// Does nothing for unobserve window operations on the mock application.
    fn unobserve_window(&mut self, _window: &Window) {
        println!("{}:", function_name!());
    }

    /// Always returns `true`, indicating the mock application is frontmost.
    fn is_frontmost(&self) -> bool {
        println!("{}:", function_name!());
        true
    }

    /// Always returns `Some("test")` for the bundle ID.
    fn bundle_id(&self) -> Option<&str> {
        println!("{}:", function_name!());
        Some("test")
    }

    /// Always returns `Ok(0)` for the parent window ID.
    fn parent_window(&self, _display_id: CGDirectDisplayID) -> Result<WinID> {
        println!("{}:", function_name!());
        Ok(0)
    }
}

/// A mock implementation of the `WindowManagerApi` trait for testing purposes.
struct MockWindowManager {}

impl WindowManagerApi for MockWindowManager {
    /// Creates a new mock application.
    fn new_application(&self, _process: &dyn ProcessApi) -> Result<Application> {
        println!("{}:", function_name!());
        Ok(Application::new(Box::new(MockApplication {
            inner: Arc::new(RwLock::new(InnerMockApplication {
                psn: ProcessSerialNumber::default(),
                pid: 0,
                focused_id: None,
            })),
        })))
    }

    /// Always returns an empty vector, as associated windows are not tested at this level.
    fn get_associated_windows(&self, _window_id: WinID) -> Vec<WinID> {
        println!("{}:", function_name!());
        vec![]
    }

    /// Always returns an empty vector, as present displays are mocked elsewhere.
    fn present_displays(&self) -> Vec<(Display, Vec<WorkspaceId>)> {
        println!("{}: []", function_name!());
        vec![]
    }

    /// Returns a predefined active display ID.
    fn active_display_id(&self) -> Result<u32> {
        println!("{}: {TEST_DISPLAY_ID}", function_name!());
        Ok(TEST_DISPLAY_ID)
    }

    /// Returns a predefined active display space ID.
    fn active_display_space(&self, _display_id: CGDirectDisplayID) -> Result<WorkspaceId> {
        println!("{}: {TEST_WORKSPACE_ID}", function_name!());
        Ok(TEST_WORKSPACE_ID)
    }

    /// Does nothing, as mouse centering is not tested at this level.
    fn center_mouse(&self, _window: Option<&Window>, _display_bounds: &CGRect) {
        println!("{}:", function_name!());
    }

    /// Always returns an empty vector of windows.
    fn find_existing_application_windows(
        &self,
        _app: &mut Application,
        _spaces: &[WorkspaceId],
    ) -> Result<(Vec<Window>, Vec<WinID>)> {
        println!("{}:", function_name!());
        Ok((vec![], vec![]))
    }

    /// Always returns `Ok(0)`.
    fn find_window_at_point(&self, _point: &CGPoint) -> Result<WinID> {
        println!("{}:", function_name!());
        Ok(0)
    }

    /// Always returns an empty vector of window IDs.
    fn windows_in_workspace(&self, _space_id: WorkspaceId) -> Result<Vec<WinID>> {
        println!("{}:", function_name!());
        Ok(vec![])
    }

    /// Always returns `Ok(())`.
    fn quit(&self) -> Result<()> {
        println!("{}:", function_name!());
        Ok(())
    }

    fn setup_config_watcher(&self, _: &std::path::Path) -> Result<Box<dyn notify::Watcher>> {
        todo!()
    }
}

/// A mock implementation of the `WindowApi` trait for testing purposes.
struct MockWindow {
    id: WinID,
    psn: Option<ProcessSerialNumber>,
    frame: CGRect,
    event_queue: Option<EventQueue>,
    app: MockApplication,
}

impl WindowApi for MockWindow {
    /// Returns the ID of the mock window.
    fn id(&self) -> WinID {
        self.id
    }

    /// Returns the `ProcessSerialNumber` of the process owning the mock window.
    fn psn(&self) -> Option<ProcessSerialNumber> {
        println!("{}:", function_name!());
        self.psn
    }

    /// Returns the frame (`CGRect`) of the mock window.
    fn frame(&self) -> CGRect {
        self.frame
    }

    /// Returns a dummy `CFRetained<AXUIWrapper>` for the mock window's accessibility element.
    fn element(&self) -> CFRetained<AXUIWrapper> {
        println!("{}:", function_name!());
        let mut s = CFString::from_static_str("");
        AXUIWrapper::from_retained(&raw mut s).unwrap()
    }

    /// Always returns an empty string for the window title.
    fn title(&self) -> Result<String> {
        println!("{}:", function_name!());
        Ok(String::new())
    }

    /// Always returns `Ok(true)` for valid role.
    fn child_role(&self) -> Result<bool> {
        println!("{}:", function_name!());
        Ok(true)
    }

    /// Always returns an empty string for the window role.
    fn role(&self) -> Result<String> {
        println!("{}:", function_name!());
        Ok(String::new())
    }

    /// Always returns an empty string for the window subrole.
    fn subrole(&self) -> Result<String> {
        println!("{}:", function_name!());
        Ok(String::new())
    }

    /// Always returns `true` for root status.
    fn is_root(&self) -> bool {
        println!("{}:", function_name!());
        true
    }

    /// Always returns `true` for eligibility.
    fn is_eligible(&self) -> bool {
        println!("{}:", function_name!());
        true
    }

    /// Repositions the mock window's frame to the given coordinates.
    fn reposition(&mut self, x: f64, y: f64, _display_bounds: &CGRect) {
        println!("{}: id {} to {x:.02}:{y:.02}", function_name!(), self.id);
        self.frame.origin.x = x;
        self.frame.origin.y = y;
    }

    fn set_origin(&mut self, x: f64, y: f64) {
        self.frame.origin.x = x;
        self.frame.origin.y = y;
    }

    /// Resizes the mock window's frame to the given dimensions.
    fn resize(&mut self, width: f64, height: f64, _display_bounds: &CGRect) {
        println!("{}: id {} to {width}x{height}", function_name!(), self.id);
        self.frame.size.width = width;
        self.frame.size.height = height;
    }

    /// Always returns `Ok(())` for updating the frame.
    fn update_frame(&mut self, _display_bounds: &CGRect) -> Result<()> {
        println!("{}:", function_name!());
        Ok(())
    }

    /// Prints a debug message for focus without raise.
    fn focus_without_raise(&self, currently_focused: &Window) {
        println!(
            "{}: id {} {}",
            function_name!(),
            self.id,
            currently_focused.id()
        );
    }

    /// Prints a debug message for focus with raise and updates the mock application's focused ID.
    fn focus_with_raise(&self) {
        println!("{}: id {}", function_name!(), self.id);
        if let Some(events) = &self.event_queue {
            events
                .write()
                .unwrap()
                .push(Event::ApplicationFrontSwitched {
                    psn: self.psn.unwrap_or_default(),
                });
        }
        self.app.inner.force_write().focused_id = Some(self.id);
    }

    /// Does nothing for width ratio.
    fn width_ratio(&self) -> f64 {
        println!("{}:", function_name!());
        0.5
    }

    /// Always returns `Ok(0)` for PID.
    fn pid(&self) -> Result<Pid> {
        println!("{}:", function_name!());
        Ok(0)
    }

    /// Sets the `ProcessSerialNumber` for the mock window.
    fn set_psn(&mut self, psn: ProcessSerialNumber) {
        println!("{}:", function_name!());
        self.psn = Some(psn);
    }

    /// Does nothing for set eligible.
    fn set_eligible(&mut self, _eligible: bool) {
        println!("{}:", function_name!());
    }

    fn set_padding(&mut self, _padding: manager::WindowPadding) {
        println!("{}:", function_name!());
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
    fn new(
        id: WinID,
        psn: Option<ProcessSerialNumber>,
        frame: CGRect,
        event_queue: Option<&EventQueue>,
        app: MockApplication,
    ) -> Self {
        MockWindow {
            id,
            psn,
            frame,
            event_queue: event_queue.cloned(),
            app,
        }
    }
}

/// Sets up a test process with mock windows within the Bevy world.
///
/// # Arguments
///
/// * `psn` - The `ProcessSerialNumber` for the test process.
/// * `world` - A mutable reference to the Bevy `World`.
/// * `strip` - A mutable reference to a `LayoutStrip` to append windows to.
/// * `event_queue` - A reference to an `EventQueue` for mock application events.
///
/// # Returns
///
/// The created `MockApplication` instance.
fn setup_test_process(
    psn: ProcessSerialNumber,
    world: &mut World,
    strip: &mut LayoutStrip,
    event_queue: &EventQueue,
) -> MockApplication {
    let mock_process = MockProcess { psn };
    let process = world.spawn(BProcess(Box::new(mock_process))).id();
    let application = MockApplication::new(psn, TEST_PROCESS_ID);

    let windows = (0..5)
        .map(|i| {
            let size = CGSize::new(f64::from(TEST_WINDOW_WIDTH), f64::from(TEST_WINDOW_HEIGHT));
            let window = MockWindow::new(
                i,
                Some(psn),
                CGRect::new(CGPoint::new(100.0 * f64::from(i), 0.0), size),
                Some(event_queue),
                application.clone(),
            );
            Window::new(Box::new(window))
        })
        .collect::<Vec<_>>();

    let parent_app = world
        .spawn((
            ChildOf(process),
            Application::new(Box::new(application.clone())),
        ))
        .id();

    for window in windows {
        let entity = if window.id() == 0 {
            world
                .spawn((ChildOf(parent_app), window, FocusedMarker))
                .id()
        } else {
            world.spawn((ChildOf(parent_app), window)).id()
        };
        strip.append(entity);
    }
    println!("strip {strip}");

    application
}

/// Sets up the Bevy `App` and `World` with necessary resources and mock components for testing.
/// It configures the `TimePlugin`, `WindowManager` (with `MockWindowManager`), `Config`, and other resources, then spawns a mock display and process.
/// The `process_command_trigger` system and other core systems are registered.
///
/// # Arguments
///
/// * `app` - A mutable reference to the Bevy `App` instance.
/// * `event_queue` - A reference to an `EventQueue` for mock application events.
///
/// # Returns
///
/// The created `MockApplication` instance for the test setup.
fn setup_world(app: &mut App, event_queue: &EventQueue) -> MockApplication {
    let psn = ProcessSerialNumber { high: 1, low: 2 };

    app.add_plugins(TimePlugin)
        .init_resource::<Messages<Event>>()
        .insert_resource(WindowManager(Box::new(MockWindowManager {})))
        .insert_resource(PollForNotifications)
        .insert_resource(SkipReshuffle(false))
        .insert_resource(MissionControlActive(false))
        .insert_resource(FocusFollowsMouse(None))
        .insert_resource(Config::default())
        .add_observer(process_command_trigger)
        .add_plugins((register_triggers, register_systems));

    app.insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_millis(
        100,
    )));

    let world = app.world_mut();
    let display = Display::new(
        TEST_DISPLAY_ID,
        CGRect::new(
            CGPoint::new(0.0, 0.0),
            CGSize::new(
                f64::from(TEST_DISPLAY_WIDTH),
                f64::from(TEST_DISPLAY_HEIGHT),
            ),
        ),
        TEST_MENUBAR_HEIGHT as u32,
    );

    let display_id = world.spawn((display, ActiveDisplayMarker)).id();
    let mut strip = LayoutStrip::new(TEST_WORKSPACE_ID);
    let process = setup_test_process(psn, world, &mut strip, event_queue);
    world.spawn((strip, ActiveWorkspaceMarker, ChildOf(display_id)));

    process
}

/// Type alias for a shared, thread-safe queue of `Event`s, used for simulating internal events in tests.
type EventQueue = Arc<RwLock<Vec<Event>>>;

/// Runs the main test loop, simulating command dispatch and Bevy app updates.
/// For each command, the Bevy app is updated multiple times, and internal mock events are flushed.
/// A `verifier` closure is called after each command to assert the state of the world.
///
/// # Arguments
///
/// * `commands` - A slice of `Event`s representing commands to dispatch.
/// * `verifier` - A closure that takes the current iteration and a mutable reference to the `World` for assertions.
fn run_main_loop(commands: &[Event], mut verifier: impl FnMut(usize, &mut World)) {
    let mut app = App::new();
    let internal_events = Arc::new(RwLock::new(Vec::<Event>::new()));
    setup_world(&mut app, &internal_events);

    for (iteration, command) in commands.iter().enumerate() {
        app.world_mut().write_message::<Event>(command.clone());

        for _ in 0..10 {
            app.update();

            // Flush the event queue with internally generated mock events.
            while let Some(event) = internal_events.write().unwrap().pop() {
                app.world_mut().write_message::<Event>(event);
            }
        }

        verifier(iteration, app.world_mut());
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
        #[allow(clippy::cast_possible_truncation)]
        if let Some((window_id, (x, y))) = expected_positions.iter().find(|id| id.0 == window.id())
        {
            debug!("WinID: {window_id}");
            assert_eq!(*x, window.frame().origin.x as i32);
            assert_eq!(*y, window.frame().origin.y as i32);
        }
    }
}

#[test]
fn test_window_shuffle() {
    let _ = env_logger::builder().is_test(true).try_init();

    let commands = vec![
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

    let offscreen_left = 0 - TEST_WINDOW_WIDTH + 10;
    let offscreen_right = TEST_DISPLAY_WIDTH - 10;

    let expected_positions_last = [
        (0, (offscreen_left, TEST_MENUBAR_HEIGHT)),
        (1, (offscreen_left, TEST_MENUBAR_HEIGHT)),
        (2, (-176, TEST_MENUBAR_HEIGHT)),
        (3, (224, TEST_MENUBAR_HEIGHT)),
        (4, (624, TEST_MENUBAR_HEIGHT)),
    ];
    let expected_positions_first = [
        (0, (0, TEST_MENUBAR_HEIGHT)),
        (1, (400, TEST_MENUBAR_HEIGHT)),
        (2, (800, TEST_MENUBAR_HEIGHT)),
        (3, (offscreen_right, TEST_MENUBAR_HEIGHT)),
        (4, (offscreen_right, TEST_MENUBAR_HEIGHT)),
    ];

    let centered = (TEST_DISPLAY_WIDTH - TEST_WINDOW_WIDTH) / 2;
    let expected_positions_stacked = [
        (0, (centered, TEST_MENUBAR_HEIGHT)),
        (1, (centered, 374)),
        (2, (centered + TEST_WINDOW_WIDTH, TEST_MENUBAR_HEIGHT)),
        (3, (offscreen_right, TEST_MENUBAR_HEIGHT)),
        (4, (offscreen_right, TEST_MENUBAR_HEIGHT)),
    ];
    let expected_positions_stacked2 = [
        (0, (centered, TEST_MENUBAR_HEIGHT)),
        (1, (centered, 249)),
        (2, (centered, 498)),
        (3, (712, TEST_MENUBAR_HEIGHT)),
        (4, (offscreen_right, TEST_MENUBAR_HEIGHT)),
    ];

    let check = |iteration, world: &mut World| {
        let iterations = [
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

    run_main_loop(&commands, check);
}

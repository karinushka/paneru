use std::sync::{Arc, RwLock};
use std::time::Duration;

use super::*;
use bevy::prelude::*;
use bevy::time::{TimePlugin, TimeUpdateStrategy};
use objc2_core_foundation::{CFRetained, CFString, CGPoint, CGRect, CGSize};
use objc2_core_graphics::CGDirectDisplayID;
use stdext::prelude::RwLockExt;

use crate::app::{Application, ApplicationApi};
use crate::commands::{Command, Direction, Operation, process_command_trigger};
use crate::config::Config;
use crate::display::{Display, WindowPane};
use crate::errors::Result;
use crate::events::{
    ActiveDisplayMarker, BProcess, Event, FocusFollowsMouse, FocusedMarker, MissionControlActive,
    PollForNotifications, SkipReshuffle, Unmanaged,
};
use crate::manager::{WindowManager, WindowManagerApi};
use crate::platform::Pid;
use crate::process::ProcessApi;
use crate::skylight::ConnID;
use crate::systems::register_systems;
use crate::triggers::register_triggers;
use crate::windows::Window;
use crate::{
    platform::ProcessSerialNumber, skylight::WinID, util::AXUIWrapper, windows::WindowApi,
};

const TEST_PROCESS_ID: i32 = 1;
const TEST_DISPLAY_ID: u32 = 1;
const TEST_WORKSPACE_ID: u64 = 2;
const TEST_DISPLAY_WIDTH: i32 = 1024;
const TEST_DISPLAY_HEIGHT: i32 = 768;

const TEST_MENUBAR_HEIGHT: i32 = 20;
const TEST_WINDOW_WIDTH: i32 = 400;
const TEST_WINDOW_HEIGHT: i32 = 1000;

struct MockProcess {
    psn: ProcessSerialNumber,
}

impl ProcessApi for MockProcess {
    fn is_observable(&mut self) -> bool {
        println!("{}:", function_name!());
        true
    }

    fn name(&self) -> &'static str {
        "test"
    }

    fn pid(&self) -> Pid {
        println!("{}:", function_name!());
        TEST_PROCESS_ID
    }

    fn psn(&self) -> ProcessSerialNumber {
        println!("{}: {:?}", function_name!(), self.psn);
        self.psn
    }

    fn application(&self) -> Option<objc2::rc::Retained<objc2_app_kit::NSRunningApplication>> {
        println!("{}:", function_name!());
        None
    }

    fn ready(&mut self) -> bool {
        println!("{}:", function_name!());
        true
    }
}

#[derive(Clone)]
struct MockApplication {
    inner: Arc<RwLock<InnerMockApplication>>,
}

struct InnerMockApplication {
    psn: ProcessSerialNumber,
    pid: Pid,
    focused_id: Option<WinID>,
}

impl MockApplication {
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
    fn pid(&self) -> Pid {
        println!("{}:", function_name!());
        self.inner.force_read().pid
    }

    fn psn(&self) -> ProcessSerialNumber {
        println!("{}:", function_name!());
        self.inner.force_read().psn
    }

    fn connection(&self) -> Option<ConnID> {
        println!("{}:", function_name!());
        Some(0)
    }

    fn focused_window_id(&self) -> Result<WinID> {
        let id = self
            .inner
            .force_read()
            .focused_id
            .ok_or(Error::InvalidWindow);
        println!("{}: {id:?}", function_name!());
        id
    }

    fn window_list(&self) -> Result<Vec<Result<Window>>> {
        println!("{}:", function_name!());
        Ok(vec![])
    }

    fn observe(&mut self) -> Result<bool> {
        println!("{}:", function_name!());
        Ok(true)
    }

    fn observe_window(&mut self, _window: &Window) -> Result<bool> {
        println!("{}:", function_name!());
        Ok(true)
    }

    fn unobserve_window(&mut self, _window: &Window) {
        println!("{}:", function_name!());
    }

    fn is_frontmost(&self) -> bool {
        println!("{}:", function_name!());
        true
    }

    fn bundle_id(&self) -> Option<&str> {
        println!("{}:", function_name!());
        Some("test")
    }

    fn parent_window(&self, _display_id: CGDirectDisplayID) -> Result<WinID> {
        println!("{}:", function_name!());
        Ok(0)
    }
}

struct MockWindowManager {}

impl WindowManagerApi for MockWindowManager {
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

    fn refresh_display(
        &self,
        _display: &mut Display,
        _windows: &mut Query<(&mut Window, Entity, Has<Unmanaged>)>,
    ) {
        println!("{}:", function_name!());
    }

    fn get_associated_windows(&self, _window_id: WinID) -> Vec<WinID> {
        println!("{}:", function_name!());
        vec![]
    }

    fn present_displays(&self) -> Vec<Display> {
        println!("{}: []", function_name!());
        vec![]
    }

    fn active_display_id(&self) -> Result<u32> {
        println!("{}: {TEST_DISPLAY_ID}", function_name!());
        Ok(TEST_DISPLAY_ID)
    }

    fn active_display_space(&self, _display_id: CGDirectDisplayID) -> Result<u64> {
        println!("{}: {TEST_WORKSPACE_ID}", function_name!());
        Ok(TEST_WORKSPACE_ID)
    }

    fn center_mouse(&self, _window: &Window, _display_bounds: &CGRect) {
        println!("{}:", function_name!());
    }

    fn add_existing_application_windows(
        &self,
        _app: &mut Application,
        _spaces: &[u64],
        _refresh_index: i32,
    ) -> Result<Vec<Window>> {
        println!("{}:", function_name!());
        Ok(vec![])
    }

    fn find_window_at_point(&self, _point: &CGPoint) -> Result<WinID> {
        println!("{}:", function_name!());
        Ok(0)
    }

    fn windows_in_workspace(&self, _space_id: u64) -> Result<Vec<WinID>> {
        println!("{}:", function_name!());
        Ok(vec![])
    }

    fn quit(&self) -> Result<()> {
        println!("{}:", function_name!());
        Ok(())
    }
}

struct MockWindow {
    id: WinID,
    psn: Option<ProcessSerialNumber>,
    frame: CGRect,
    event_queue: Option<EventQueue>,
    app: MockApplication,
}

impl WindowApi for MockWindow {
    fn id(&self) -> WinID {
        self.id
    }

    fn psn(&self) -> Option<ProcessSerialNumber> {
        println!("{}:", function_name!());
        self.psn
    }

    fn frame(&self) -> CGRect {
        self.frame
    }

    fn next_size_ratio(&self, _: &[f64]) -> f64 {
        println!("{}:", function_name!());
        0.5
    }

    fn element(&self) -> CFRetained<AXUIWrapper> {
        println!("{}:", function_name!());
        let mut s = CFString::from_static_str("");
        AXUIWrapper::from_retained(&raw mut s).unwrap()
        // self.ax_element.clone()
    }

    fn title(&self) -> Result<String> {
        println!("{}:", function_name!());
        Ok(String::new())
    }

    fn valid_role(&self) -> Result<bool> {
        println!("{}:", function_name!());
        Ok(true)
    }

    fn role(&self) -> Result<String> {
        println!("{}:", function_name!());
        Ok(String::new())
    }

    fn subrole(&self) -> Result<String> {
        println!("{}:", function_name!());
        Ok(String::new())
    }

    fn is_root(&self) -> bool {
        println!("{}:", function_name!());
        true
    }

    fn is_eligible(&self) -> bool {
        println!("{}:", function_name!());
        true
    }

    fn reposition(&mut self, x: f64, y: f64, _display_bounds: &CGRect) {
        println!("{}: id {} to {x:.02}:{y:.02}", function_name!(), self.id);
        self.frame.origin.x = x;
        self.frame.origin.y = y;
    }

    fn resize(&mut self, width: f64, height: f64, _display_bounds: &CGRect) {
        println!("{}: id {} to {width}x{height}", function_name!(), self.id);
        self.frame.size.width = width;
        self.frame.size.height = height;
    }

    fn update_frame(&mut self, _display_bounds: Option<&CGRect>) -> Result<()> {
        println!("{}:", function_name!());
        Ok(())
    }

    fn focus_without_raise(&self, currently_focused: &Window) {
        println!(
            "{}: id {} {}",
            function_name!(),
            self.id,
            currently_focused.id()
        );
    }

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

    fn width_ratio(&mut self, _width_ratio: f64) {
        println!("{}:", function_name!());
    }

    fn pid(&self) -> Result<Pid> {
        println!("{}:", function_name!());
        Ok(0)
    }

    fn set_psn(&mut self, psn: ProcessSerialNumber) {
        println!("{}:", function_name!());
        self.psn = Some(psn);
    }

    fn set_eligible(&mut self, _eligible: bool) {
        println!("{}:", function_name!());
    }
}

impl MockWindow {
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

fn setup_test_process(
    psn: ProcessSerialNumber,
    world: &mut World,
    panel: &mut WindowPane,
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
        panel.append(entity);
    }
    println!("panel {panel}");

    application
}

fn setup_world(app: &mut App, event_queue: &EventQueue) -> MockApplication {
    let psn = ProcessSerialNumber { high: 1, low: 2 };

    app.add_plugins(TimePlugin)
        .init_resource::<Messages<crate::events::Event>>()
        .insert_resource(WindowManager(Box::new(MockWindowManager {})))
        .insert_resource(PollForNotifications(true))
        .insert_resource(SkipReshuffle(false))
        .insert_resource(MissionControlActive(false))
        .insert_resource(FocusFollowsMouse(None))
        .insert_resource(PollForNotifications(true))
        .insert_resource(Config::default())
        .add_observer(process_command_trigger);
    register_triggers(app);
    register_systems(app);

    app.insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_millis(
        100,
    )));

    let world = app.world_mut();
    let mut display = Display::new(
        TEST_DISPLAY_ID,
        vec![TEST_WORKSPACE_ID],
        CGRect::new(
            CGPoint::new(0.0, 0.0),
            CGSize::new(
                f64::from(TEST_DISPLAY_WIDTH),
                f64::from(TEST_DISPLAY_HEIGHT),
            ),
        ),
        TEST_MENUBAR_HEIGHT as u32,
    );
    let panel = display.active_panel_mut(TEST_WORKSPACE_ID).unwrap();

    let process = setup_test_process(psn, world, panel, event_queue);

    world.spawn((display, ActiveDisplayMarker));

    process
}

type EventQueue = Arc<RwLock<Vec<Event>>>;

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

fn verify_window_positions(expected_positions: &[(WinID, (i32, i32))], world: &mut World) {
    let mut query = world.query::<&Window>();
    for window in query.iter(world) {
        #[allow(clippy::cast_possible_truncation)]
        if let Some((_, (x, y))) = expected_positions.iter().find(|id| id.0 == window.id()) {
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
        (1, (centered, 364)),
        (2, (centered + TEST_WINDOW_WIDTH, TEST_MENUBAR_HEIGHT)),
        (3, (offscreen_right, TEST_MENUBAR_HEIGHT)),
        (4, (offscreen_right, TEST_MENUBAR_HEIGHT)),
    ];
    let expected_positions_stacked2 = [
        (0, (centered, TEST_MENUBAR_HEIGHT)),
        (1, (centered, 364)),
        (2, (centered, 546)),
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
            verify_window_positions(positions, world);
        }
    };

    run_main_loop(&commands, check);
}

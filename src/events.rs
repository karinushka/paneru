use bevy::app::{App as BevyApp, AppExit, Startup};
use bevy::ecs::message::{Message, Messages};
use bevy::ecs::schedule::IntoScheduleConfigs;
use bevy::ecs::world::World;
use bevy::time::{Time, TimePlugin, Virtual};
use log::{debug, error, warn};
use objc2::rc::Retained;
use objc2_core_foundation::{CFRetained, CGPoint};
use objc2_core_graphics::CGDirectDisplayID;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::thread;
use std::thread::JoinHandle;
use std::time::Duration;
use stdext::function_name;

use crate::commands::{Command, process_command_trigger};
use crate::config::{CONFIGURATION_FILE, Config};
use crate::ecs::{
    BProcess, ExistingMarker, FocusFollowsMouse, MissionControlActive, PollForNotifications,
    SkipReshuffle, gather_displays, register_systems, register_triggers,
    run_initial_oneshot_systems,
};
use crate::errors::Result;
use crate::manager::{Process, WindowManager, WindowManagerApi, WindowManagerOS};
use crate::platform::{ProcessSerialNumber, WinID, WorkspaceObserver};
use crate::util::AXUIWrapper;

/// `Event` represents various system-level and application-specific occurrences that the window manager reacts to.
/// These events drive the core logic of the window manager, from window creation to display changes.
#[allow(dead_code)]
#[derive(Clone, Debug, Message)]
pub enum Event {
    /// Signals the application to exit.
    Exit,
    /// Indicates that the initial set of processes has been loaded.
    ProcessesLoaded,

    /// Announces the initialy loaded configuration
    InitialConfig(Config),
    /// Signals that the configuration should be reloaded.
    ConfigRefresh(notify::Event),

    /// An application has been launched.
    ApplicationLaunched {
        psn: ProcessSerialNumber,
        observer: Retained<WorkspaceObserver>,
    },

    /// An application has terminated.
    ApplicationTerminated { psn: ProcessSerialNumber },
    /// The frontmost application has switched.
    ApplicationFrontSwitched { psn: ProcessSerialNumber },
    /// The application has been activated.
    ApplicationActivated,
    /// The application has been deactivated.
    ApplicationDeactivated,
    /// An application has become visible.
    ApplicationVisible { pid: i32 },
    /// An application has become hidden.
    ApplicationHidden { pid: i32 },

    /// A window has been created.
    WindowCreated { element: CFRetained<AXUIWrapper> },
    /// A window has been destroyed.
    WindowDestroyed { window_id: WinID },
    /// A window has gained focus.
    WindowFocused { window_id: WinID },
    /// A window has been moved.
    WindowMoved { window_id: WinID },
    /// A window has been resized.
    WindowResized { window_id: WinID },
    /// A window has been minimized.
    WindowMinimized { window_id: WinID },
    /// A window has been de-minimized (restored).
    WindowDeminimized { window_id: WinID },
    /// A window's title has changed.
    WindowTitleChanged { window_id: WinID },

    /// Indicates the currently focused item.
    CurrentlyFocused,

    /// A mouse down event has occurred.
    MouseDown { point: CGPoint },
    /// A mouse up event has occurred.
    MouseUp { point: CGPoint },
    /// A mouse drag event has occurred.
    MouseDragged { point: CGPoint },
    /// A mouse move event has occurred.
    MouseMoved { point: CGPoint },

    /// A swipe gesture has been detected.
    Swipe { deltas: Vec<f64> },

    /// A new space (virtual desktop) has been created.
    SpaceCreated,
    /// A space has been destroyed.
    SpaceDestroyed,
    /// The active space has changed.
    SpaceChanged,

    /// A new display has been added.
    DisplayAdded { display_id: CGDirectDisplayID },
    /// A display has been removed.
    DisplayRemoved { display_id: CGDirectDisplayID },
    /// A display has been moved.
    DisplayMoved { display_id: CGDirectDisplayID },
    /// A display has been resized.
    DisplayResized { display_id: CGDirectDisplayID },
    /// A display's configuration has changed.
    DisplayConfigured { display_id: CGDirectDisplayID },
    /// The overall display arrangement has changed.
    DisplayChanged,

    /// Mission Control: Show all windows.
    MissionControlShowAllWindows,
    /// Mission Control: Show frontmost application windows.
    MissionControlShowFrontWindows,
    /// Mission Control: Show desktop.
    MissionControlShowDesktop,
    /// Mission Control: Exit.
    MissionControlExit,

    /// Dock preferences have changed.
    DockDidChangePref { msg: String },
    /// The Dock has restarted.
    DockDidRestart { msg: String },

    /// A menu has been opened.
    MenuOpened { window_id: WinID },
    /// A menu has been closed.
    MenuClosed { window_id: WinID },
    /// The visibility of the menu bar has changed.
    MenuBarHiddenChanged { msg: String },
    /// The system has woken from sleep.
    SystemWoke { msg: String },

    /// A command has been issued to the window manager.
    Command { command: Command },

    /// Represents the total number of event types (for internal use, e.g., array sizing).
    TypeCount,
}

/// `EventSender` is a thin wrapper around a `std::sync::mpsc::Sender` for `Event`s.
/// It provides a convenient way to send events to the main event loop from various parts of the application.
#[derive(Clone, Debug)]
pub struct EventSender {
    tx: Sender<Event>,
}

impl EventSender {
    /// Creates a new `EventSender` and its corresponding `Receiver`.
    /// This function initializes an MPSC channel.
    ///
    /// # Returns
    ///
    /// A tuple containing the `EventSender` and `Receiver` for the created channel.
    fn new() -> (Self, Receiver<Event>) {
        let (tx, rx) = channel::<Event>();
        (Self { tx }, rx)
    }

    /// Sends an `Event` through the internal channel.
    ///
    /// # Arguments
    ///
    /// * `event` - The `Event` to send.
    ///
    /// # Returns
    ///
    /// `Ok(())` if the event is sent successfully, otherwise `Err(Error)` if the receiver has disconnected.
    pub fn send(&self, event: Event) -> Result<()> {
        Ok(self.tx.send(event)?)
    }
}

/// `EventHandler` is responsible for setting up and running the main event loop of the window manager.
/// It acts as the central hub for receiving system events, dispatching them to the Bevy ECS, and managing the application's lifecycle.
pub struct EventHandler;

impl EventHandler {
    /// Runs the main event loop in a new thread.
    /// This function sets up the MPSC channel for events, creates a quit signal, and spawns the event runner thread.
    ///
    /// # Returns
    ///
    /// A tuple containing the `EventSender` for sending events, an `Arc<AtomicBool>` to signal the application to quit,
    /// and the `JoinHandle` for the event runner thread.
    pub fn run() -> (EventSender, Arc<AtomicBool>, JoinHandle<()>) {
        let (sender, receiver) = EventSender::new();
        let quit = Arc::new(AtomicBool::new(false));

        (
            sender.clone(),
            quit.clone(),
            thread::spawn(move || {
                if let Err(err) = EventHandler::runner(receiver, sender, &quit) {
                    error!("{}: Error in the runner: {err}", function_name!());
                }
            }),
        )
    }

    /// The main runner function for the event loop, executed in a separate thread.
    /// It sets up the Bevy application, registers systems and triggers, and runs the custom Bevy loop.
    ///
    /// # Arguments
    ///
    /// * `receiver` - The `Receiver` for incoming events.
    /// * `sender` - The `EventSender` to send events (used for `WindowManagerOS` initialization).
    /// * `quit` - An `Arc<AtomicBool>` to signal when the application should exit.
    ///
    /// # Returns
    ///
    /// `Ok(())` if the runner completes successfully, otherwise `Err(Error)`.
    fn runner(
        receiver: Receiver<Event>,
        sender: EventSender,
        quit: &Arc<AtomicBool>,
    ) -> Result<()> {
        let (mut existing_processes, config) = EventHandler::gather_initial_processes(&receiver)?;
        let process_setup = move |world: &mut World| {
            EventHandler::initial_setup(world, &mut existing_processes, config.as_ref());
        };

        let window_manager: Box<dyn WindowManagerApi> = Box::new(WindowManagerOS::new(sender));
        let watcher = window_manager.setup_config_watcher(CONFIGURATION_FILE.as_path())?;

        let mut app = BevyApp::new();
        app.set_runner(move |app| EventHandler::custom_loop(app, &receiver))
            .add_plugins(TimePlugin)
            .init_resource::<Messages<Event>>()
            .insert_resource(Time::<Virtual>::from_max_delta(Duration::from_secs(10)))
            .insert_resource(WindowManager(window_manager))
            .insert_resource(SkipReshuffle(false))
            .insert_resource(MissionControlActive(false))
            .insert_resource(FocusFollowsMouse(None))
            .insert_resource(PollForNotifications(true))
            .add_observer(process_command_trigger)
            .add_systems(Startup, (gather_displays, process_setup).chain());

        app.insert_non_send_resource(watcher);

        register_triggers(&mut app);
        register_systems(&mut app);
        app.run();

        quit.store(true, std::sync::atomic::Ordering::Relaxed);
        Ok(())
    }

    /// The custom Bevy application loop, handling events from the receiver.
    /// This loop continuously updates the Bevy app and processes incoming events from the MPSC channel.
    /// It includes a timeout mechanism to prevent excessive CPU usage when no events are present.
    ///
    /// # Arguments
    ///
    /// * `app` - The Bevy application instance.
    /// * `rx` - The `Receiver` for incoming events.
    ///
    /// # Returns
    ///
    /// An `AppExit` code, typically `AppExit::Success`.
    fn custom_loop(mut app: BevyApp, rx: &Receiver<Event>) -> AppExit {
        const LOOP_MAX_TIMEOUT_MS: u64 = 5000;
        const LOOP_TIMEOUT_STEP: u64 = 1;
        app.finish();
        app.cleanup();

        let mut timeout = LOOP_TIMEOUT_STEP;
        while app.should_exit().is_none() {
            app.update();
            match rx.recv_timeout(Duration::from_millis(timeout)) {
                Ok(Event::Exit) => {
                    app.world_mut().write_message::<AppExit>(AppExit::Success);
                }
                Ok(event) => {
                    app.world_mut().write_message::<Event>(event);
                    timeout = LOOP_TIMEOUT_STEP;
                }
                Err(RecvTimeoutError::Timeout) => {
                    timeout = timeout.min(LOOP_MAX_TIMEOUT_MS) + LOOP_TIMEOUT_STEP;
                }
                _ => todo!(),
            }
        }
        AppExit::Success
    }

    /// Gathers initial processes and configuration before the main Bevy loop starts.
    /// It processes events from the receiver until `Event::ProcessesLoaded` or `Event::Exit` is received.
    ///
    /// # Arguments
    ///
    /// * `receiver` - The `Receiver` for incoming events.
    ///
    /// # Returns
    ///
    /// A tuple containing a vector of `BProcess` for initially launched processes and an `Option<Config>`.
    /// Returns an `Err(Error)` if an error occurs during event reception.
    fn gather_initial_processes(
        receiver: &Receiver<Event>,
    ) -> Result<(Vec<BProcess>, Option<Config>)> {
        let mut initial_processes = Vec::new();
        let mut initial_config = None;
        loop {
            match receiver.recv()? {
                Event::ProcessesLoaded | Event::Exit => break,
                Event::ApplicationLaunched { psn, observer } => {
                    initial_processes.push(Process::new(&psn, observer.clone()).into());
                }
                Event::InitialConfig(config) => {
                    initial_config = Some(config);
                }
                event => warn!(
                    "{}: Stray event during initial process gathering: {event:?}",
                    function_name!()
                ),
            }
        }
        Ok((initial_processes, initial_config))
    }

    /// Sets up the initial state of the Bevy world, spawning existing observable processes.
    /// This function adds the configuration as a resource and spawns `ExistingMarker` and `BProcess` components for observable processes.
    ///
    /// # Arguments
    ///
    /// * `world` - The Bevy world instance to set up.
    /// * `existing_processes` - A mutable vector of `BProcess` instances representing processes to add.
    /// * `config` - An `Option<&Config>` containing the application configuration if available.
    fn initial_setup(
        world: &mut World,
        existing_processes: &mut Vec<BProcess>,
        config: Option<&Config>,
    ) {
        if let Some(config) = config {
            world.insert_resource(config.clone());
        }

        while let Some(mut process) = existing_processes.pop() {
            if process.is_observable() {
                debug!(
                    "{}: Adding existing process {}",
                    function_name!(),
                    process.name()
                );
                world.spawn((ExistingMarker, process));
            } else {
                debug!(
                    "{}: Existing application '{}' is not observable, ignoring it.",
                    function_name!(),
                    process.name(),
                );
            }
        }

        run_initial_oneshot_systems(world);
    }
}

use bevy::MinimalPlugins;
use bevy::app::{App as BevyApp, Startup};
use bevy::ecs::message::{Message, Messages};
use bevy::ecs::schedule::IntoScheduleConfigs;
use bevy::ecs::world::World;
use bevy::time::{Time, Virtual};
use log::{debug, error, warn};
use objc2::rc::Retained;
use objc2_core_foundation::{CFRetained, CGPoint};
use objc2_core_graphics::CGDirectDisplayID;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::Duration;
use stdext::function_name;

use crate::commands::{Command, process_command_trigger};
use crate::config::{CONFIGURATION_FILE, Config};
use crate::ecs::{
    BProcess, ExistingMarker, FocusFollowsMouse, MissionControlActive, PollForNotifications,
    SkipReshuffle, gather_displays, initial_oneshot_systems, register_systems, register_triggers,
};
use crate::errors::Result;
use crate::manager::{Process, WindowManager, WindowManagerApi, WindowManagerOS};
use crate::platform::{PlatformCallbacks, ProcessSerialNumber, WinID, WorkspaceObserver};
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

    /// Dump internal window manager state.
    PrintState,
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
    pub fn new() -> (Self, Receiver<Event>) {
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
    pub fn setup_bevy_app(sender: EventSender, receiver: Receiver<Event>) -> Result<BevyApp> {
        let process_setup = move |world: &mut World| {
            let Some((mut existing_processes, config)) = world
                .get_non_send_resource::<Receiver<Event>>()
                .and_then(|receiver| EventHandler::gather_initial_processes(receiver).ok())
            else {
                error!("{}: gathering initial processes.", function_name!());
                return;
            };
            EventHandler::initial_setup(world, &mut existing_processes, config.as_ref());
        };

        let window_manager: Box<dyn WindowManagerApi> =
            Box::new(WindowManagerOS::new(sender.clone()));
        let watcher = window_manager.setup_config_watcher(CONFIGURATION_FILE.as_path())?;

        let mut app = BevyApp::new();
        app.add_plugins(MinimalPlugins)
            .init_resource::<Messages<Event>>()
            .insert_resource(Time::<Virtual>::from_max_delta(Duration::from_secs(10)))
            .insert_resource(WindowManager(window_manager))
            .insert_resource(SkipReshuffle(false))
            .insert_resource(MissionControlActive(false))
            .insert_resource(FocusFollowsMouse(None))
            .insert_resource(PollForNotifications(true))
            .add_observer(process_command_trigger)
            .add_systems(Startup, (gather_displays, process_setup).chain())
            .insert_non_send_resource(watcher)
            .add_plugins((register_triggers, register_systems));

        let mut platform_callbacks = PlatformCallbacks::new(sender);
        platform_callbacks.setup_handlers()?;
        app.insert_non_send_resource(platform_callbacks);
        app.insert_non_send_resource(receiver);

        Ok(app)
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

        let oneshot_systems = initial_oneshot_systems(world);

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
            for id in &oneshot_systems {
                _ = world.run_system(*id);
            }
        }
        world.flush();

        for id in oneshot_systems {
            _ = world.unregister_system(id);
        }
    }
}

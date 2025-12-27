use bevy::app::{App as BevyApp, AppExit, Startup};
use bevy::ecs::component::Component;
use bevy::ecs::entity::Entity;
use bevy::ecs::message::{Message, Messages};
use bevy::ecs::resource::Resource;
use bevy::ecs::schedule::IntoScheduleConfigs;
use bevy::ecs::world::World;
use bevy::prelude::Event as BevyEvent;
use bevy::time::{Time, TimePlugin, Timer, Virtual};
use log::{debug, error, warn};
use objc2::rc::Retained;
use objc2_core_foundation::{CFRetained, CGPoint, CGSize};
use objc2_core_graphics::CGDirectDisplayID;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::thread;
use std::thread::JoinHandle;
use std::time::Duration;
use stdext::function_name;

use crate::commands::{Command, process_command_trigger};
use crate::config::Config;
use crate::errors::Result;
use crate::manager::{WindowManagerApi, WindowManagerOS};
use crate::platform::{ProcessSerialNumber, WorkspaceObserver};
use crate::process::{Process, ProcessRef};
use crate::skylight::WinID;
use crate::systems::{gather_displays, register_systems, run_initial_oneshot_systems};
use crate::triggers::register_triggers;
use crate::util::AXUIWrapper;
use crate::windows::{Window, WindowPane};

#[allow(dead_code)]
#[derive(Clone, Debug, Message)]
pub enum Event {
    Exit,
    ProcessesLoaded,
    ConfigRefresh {
        config: Config,
    },

    ApplicationLaunched {
        psn: ProcessSerialNumber,
        observer: Retained<WorkspaceObserver>,
    },

    ApplicationTerminated {
        psn: ProcessSerialNumber,
    },
    ApplicationFrontSwitched {
        psn: ProcessSerialNumber,
    },
    ApplicationActivated,
    ApplicationDeactivated,
    ApplicationVisible {
        pid: i32,
    },
    ApplicationHidden {
        pid: i32,
    },

    WindowCreated {
        element: CFRetained<AXUIWrapper>,
    },
    WindowDestroyed {
        window_id: WinID,
    },
    WindowFocused {
        window_id: WinID,
    },
    WindowMoved {
        window_id: WinID,
    },
    WindowResized {
        window_id: WinID,
    },
    WindowMinimized {
        window_id: WinID,
    },
    WindowDeminimized {
        window_id: WinID,
    },
    WindowTitleChanged {
        window_id: WinID,
    },

    CurrentlyFocused,

    MouseDown {
        point: CGPoint,
    },
    MouseUp {
        point: CGPoint,
    },
    MouseDragged {
        point: CGPoint,
    },
    MouseMoved {
        point: CGPoint,
    },

    Swipe {
        deltas: Vec<f64>,
    },

    SpaceCreated,
    SpaceDestroyed,
    SpaceChanged,

    DisplayAdded {
        display_id: CGDirectDisplayID,
    },
    DisplayRemoved {
        display_id: CGDirectDisplayID,
    },
    DisplayMoved {
        display_id: CGDirectDisplayID,
    },
    DisplayResized {
        display_id: CGDirectDisplayID,
    },
    DisplayConfigured {
        display_id: CGDirectDisplayID,
    },
    DisplayChanged,

    MissionControlShowAllWindows,
    MissionControlShowFrontWindows,
    MissionControlShowDesktop,
    MissionControlExit,

    DockDidChangePref {
        msg: String,
    },
    DockDidRestart {
        msg: String,
    },

    MenuOpened {
        window_id: WinID,
    },
    MenuClosed {
        window_id: WinID,
    },
    MenuBarHiddenChanged {
        msg: String,
    },
    SystemWoke {
        msg: String,
    },

    Command {
        command: Command,
    },

    TypeCount,
}

#[derive(Clone, Debug)]
pub struct EventSender {
    tx: Sender<Event>,
}

impl EventSender {
    /// Creates a new `EventSender` and its corresponding `Receiver`.
    ///
    /// # Returns
    ///
    /// A tuple containing the `EventSender` and `Receiver`.
    fn new() -> (Self, Receiver<Event>) {
        let (tx, rx) = channel::<Event>();
        (Self { tx }, rx)
    }

    /// Sends an `Event` through the channel.
    ///
    /// # Arguments
    ///
    /// * `event` - The `Event` to send.
    ///
    /// # Returns
    ///
    /// `Ok(())` if the event is sent successfully, otherwise `Err(Error)`.
    pub fn send(&self, event: Event) -> Result<()> {
        Ok(self.tx.send(event)?)
    }
}

// Used to signify currently focused window.
#[derive(Component)]
pub struct FocusedMarker;

// Used to signify currently active display
#[derive(Component)]
pub struct ActiveDisplayMarker;

// Signifies freshly created Process, Application or Window.
#[derive(Component)]
pub struct FreshMarker;

// Used to gather existing processes and windows.
#[derive(Component)]
pub struct ExistingMarker;

#[derive(Component)]
pub struct RepositionMarker {
    pub origin: CGPoint,
    pub display_id: CGDirectDisplayID,
}

#[derive(Component)]
pub struct ResizeMarker {
    pub size: CGSize,
}

#[derive(Component)]
pub struct WindowDraggedMarker {
    pub entity: Entity,
    pub display_id: CGDirectDisplayID,
}

#[derive(Component)]
pub struct ReshuffleAroundMarker;

#[derive(Component)]
pub enum Unmanaged {
    Floating,
    Minimized,
    Hidden,
}

#[derive(Component)]
pub struct BProcess(pub ProcessRef);

#[derive(Component)]
pub struct Timeout {
    pub timer: Timer,
    pub message: Option<String>,
}

impl Timeout {
    /// Creates a new timeout with a duration and an optional message.
    pub fn new(duration: Duration, message: Option<String>) -> Self {
        let timer = Timer::from_seconds(duration.as_secs_f32(), bevy::time::TimerMode::Once);
        Self { timer, message }
    }
}

// Used as a retry for stray focus event arriving before window is created.
#[derive(Component)]
pub struct StrayFocusEvent(pub WinID);

#[derive(Component)]
pub struct OrphanedPane {
    pub id: u64,
    pub pane: WindowPane,
}

#[derive(Resource)]
pub struct SenderSocket(pub EventSender);

#[derive(Resource)]
pub struct SkipReshuffle(pub bool);

#[derive(Resource)]
pub struct MissionControlActive(pub bool);

#[derive(Resource)]
pub struct FocusFollowsMouse(pub Option<WinID>);

#[derive(Resource)]
pub struct WindowManager(pub Box<dyn WindowManagerApi>);

#[derive(PartialEq, Resource)]
pub struct PollForNotifications(pub bool);

#[derive(BevyEvent)]
pub struct WMEventTrigger(pub Event);

#[derive(BevyEvent)]
pub struct CommandTrigger(pub Command);

#[derive(BevyEvent)]
pub struct SpawnWindowTrigger(pub Vec<Window>);

pub struct EventHandler;

impl EventHandler {
    /// Runs the main event loop in a new thread.
    ///
    /// # Returns
    ///
    /// A tuple containing the `EventSender`, an `Arc<AtomicBool>` for quitting, and the `JoinHandle` for the thread.
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

    fn runner(
        receiver: Receiver<Event>,
        sender: EventSender,
        quit: &Arc<AtomicBool>,
    ) -> Result<()> {
        let (mut existing_processes, config) = EventHandler::gather_initial_processes(&receiver)?;
        let process_setup = move |world: &mut World| {
            EventHandler::initial_setup(world, &mut existing_processes, config.as_ref());
        };

        let mut app = BevyApp::new();
        app.set_runner(move |app| EventHandler::custom_loop(app, &receiver))
            .add_plugins(TimePlugin)
            .init_resource::<Messages<Event>>()
            .insert_resource(Time::<Virtual>::from_max_delta(Duration::from_secs(10)))
            .insert_resource(WindowManager(Box::new(WindowManagerOS::new())))
            .insert_resource(SenderSocket(sender))
            .insert_resource(SkipReshuffle(false))
            .insert_resource(MissionControlActive(false))
            .insert_resource(FocusFollowsMouse(None))
            .insert_resource(PollForNotifications(true))
            .add_observer(process_command_trigger)
            .add_systems(Startup, gather_displays)
            .add_systems(Startup, process_setup.after(gather_displays));
        register_triggers(&mut app);
        register_systems(&mut app);
        app.run();

        quit.store(true, std::sync::atomic::Ordering::Relaxed);
        Ok(())
    }

    /// The custom Bevy application loop, handling events from the receiver.
    ///
    /// # Arguments
    ///
    /// * `app` - The Bevy application instance.
    /// * `rx` - The `Receiver` for incoming events.
    ///
    /// # Returns
    ///
    /// An `AppExit` code.
    fn custom_loop(mut app: BevyApp, rx: &Receiver<Event>) -> AppExit {
        const LOOP_MAX_TIMEOUT_MS: u64 = 5000;
        const LOOP_TIMEOUT_STEP: u64 = 5;
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

    /// Gathers initial processes and configuration before the main loop starts.
    ///
    /// # Arguments
    ///
    /// * `receiver` - The `Receiver` for incoming events.
    /// * `sender` - The `EventSender` to send events.
    ///
    /// # Returns
    ///
    /// A vector of `ProcessRef` for the processes launched before the window manager started.
    fn gather_initial_processes(
        receiver: &Receiver<Event>,
    ) -> Result<(Vec<ProcessRef>, Option<Config>)> {
        let mut initial_processes = Vec::new();
        let mut initial_config = None;
        loop {
            match receiver.recv()? {
                Event::ProcessesLoaded | Event::Exit => break,
                Event::ApplicationLaunched { psn, observer } => {
                    initial_processes.push(Process::new(&psn, observer.clone()));
                }
                Event::ConfigRefresh { config } => {
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

    /// Sets up the initial state of the Bevy world, spawning existing processes.
    ///
    /// # Arguments
    ///
    /// * `world` - The Bevy world.
    /// * `existing_processes` - A mutable vector of `ProcessRef` for processes to add.
    fn initial_setup(
        world: &mut World,
        existing_processes: &mut Vec<ProcessRef>,
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
                    process.name
                );
                world.spawn((ExistingMarker, BProcess(process)));
            } else {
                debug!(
                    "{}: Existing application '{}' is not observable, ignoring it.",
                    function_name!(),
                    process.name,
                );
            }
        }

        run_initial_oneshot_systems(world);
    }
}

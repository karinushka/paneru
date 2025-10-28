use bevy::app::{App as BevyApp, AppExit, Startup, Update};
use bevy::ecs::component::Component;
use bevy::ecs::entity::Entity;
use bevy::ecs::message::{Message, MessageReader, Messages};
use bevy::ecs::query::With;
use bevy::ecs::resource::Resource;
use bevy::ecs::schedule::IntoScheduleConfigs;
use bevy::ecs::schedule::common_conditions::any_with_component;
use bevy::ecs::system::{Commands, Query, Res};
use bevy::ecs::world::World;
use bevy::prelude::Event as BevyEvent;
use bevy::time::{Time, Virtual};
use log::{debug, error, info, trace, warn};
use objc2::rc::Retained;
use objc2_core_foundation::{CFRetained, CGPoint};
use objc2_core_graphics::CGDirectDisplayID;
use std::collections::HashMap;
use std::io::{Error, ErrorKind, Result};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::thread;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};
use stdext::function_name;

use crate::commands::process_command_trigger;
use crate::config::Config;
use crate::manager::WindowManager;
use crate::platform::{ProcessSerialNumber, WorkspaceObserver};
use crate::process::{Process, ProcessRef};
use crate::skylight::{ConnID, SLSMainConnectionID, WinID};
use crate::util::AxuWrapperType;
use crate::windows::{Display, Window, WindowPane};

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
        msg: String,
    },
    ApplicationHidden {
        msg: String,
    },

    WindowCreated {
        element: CFRetained<AxuWrapperType>,
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
        argv: Vec<String>,
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
        self.tx
            .send(event)
            .map_err(|err| {
                Error::new(
                    ErrorKind::ConnectionAborted,
                    format!("{}: sending event: {err}", function_name!()),
                )
            })
            .inspect_err(|err| error!("{err}"))
    }
}

// While this marker exists in the world, the system is gathering existing windows.
#[derive(Component)]
pub struct InitializingMarker;

// Used to signify currently active display and focused window.
#[derive(Component)]
pub struct FocusedMarker;

// Signifies freshly created Process, Application or Window.
#[derive(Component)]
pub struct FreshMarker;

// Used to gather existing processes and windows.
#[derive(Component)]
pub struct ExistingMarker;

#[derive(Component)]
pub struct DestroyedMarker;

#[derive(Component)]
pub struct BProcess(pub ProcessRef);

#[derive(Resource)]
pub struct MainConnection(pub ConnID);

#[derive(Resource)]
pub struct SenderSocket(pub EventSender);

#[derive(Resource)]
pub struct SkipReshuffle(pub bool);

#[derive(Resource)]
pub struct MissionControlActive(pub bool);

#[derive(Resource)]
pub struct FocusFollowsMouse(pub Option<WinID>);

#[derive(Resource)]
pub struct OrphanedSpaces(pub HashMap<u64, WindowPane>);

#[derive(BevyEvent)]
pub struct WMEventTrigger(pub Event);

#[derive(BevyEvent)]
pub struct CommandTrigger(pub Vec<String>);

#[derive(BevyEvent)]
pub struct ReshuffleAroundTrigger(pub WinID);

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
                let main_cid = unsafe { SLSMainConnectionID() };
                debug!("{}: My connection id: {main_cid}", function_name!());

                let mut existing_processes =
                    EventHandler::gather_initial_processes(&receiver, &sender);
                let process_setup = move |world: &mut World| {
                    EventHandler::initial_setup(world, &mut existing_processes);
                };

                BevyApp::new()
                    .set_runner(move |app| EventHandler::custom_loop(app, &receiver))
                    .init_resource::<Messages<Event>>()
                    .insert_resource(Time::<Virtual>::from_max_delta(Duration::from_secs(10)))
                    .insert_resource(MainConnection(main_cid))
                    .insert_resource(SenderSocket(sender))
                    .insert_resource(SkipReshuffle(false))
                    .insert_resource(MissionControlActive(false))
                    .insert_resource(FocusFollowsMouse(None))
                    .insert_resource(OrphanedSpaces(HashMap::new()))
                    .add_observer(process_command_trigger)
                    .add_observer(WindowManager::mouse_moved_trigger)
                    .add_observer(WindowManager::mouse_down_trigger)
                    .add_observer(WindowManager::mouse_dragged_trigger)
                    .add_observer(WindowManager::display_change_trigger)
                    .add_observer(WindowManager::display_add_remove_trigger)
                    .add_observer(WindowManager::front_switched_trigger)
                    .add_observer(WindowManager::window_focused_trigger)
                    .add_observer(WindowManager::reshuffle_around_trigger)
                    .add_observer(WindowManager::swipe_gesture_trigger)
                    .add_observer(WindowManager::mission_control_trigger)
                    .add_systems(Startup, EventHandler::gather_displays)
                    .add_systems(Startup, process_setup.after(EventHandler::gather_displays))
                    .add_systems(
                        Update,
                        (
                            WindowManager::dispatch_process_messages,
                            WindowManager::dispatch_application_messages,
                        ),
                    )
                    .add_systems(
                        Update,
                        (
                            EventHandler::dispatch_toplevel_triggers,
                            WindowManager::add_existing_process
                                .run_if(any_with_component::<InitializingMarker>),
                            WindowManager::add_existing_application
                                .run_if(any_with_component::<InitializingMarker>),
                            EventHandler::finish_setup
                                .run_if(any_with_component::<InitializingMarker>),
                            WindowManager::add_launched_process,
                            WindowManager::add_launched_application,
                        ),
                    )
                    .add_systems(
                        Update,
                        (
                            WindowManager::window_create,
                            WindowManager::window_destroyed,
                        ),
                    )
                    .run();
                quit.store(true, std::sync::atomic::Ordering::Relaxed);
            }),
        )
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
        let mut last_update = Instant::now();
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

            // Manually get and update the Time resource.
            let now = Instant::now();
            let delta = now - last_update;
            last_update = now;
            app.world_mut()
                .resource_mut::<Time<Virtual>>()
                .advance_by(delta);
        }
        AppExit::Success
    }

    /// Processes a single incoming `Event`. It dispatches various event types to the `WindowManager` or other internal handlers.
    ///
    /// # Arguments
    ///
    /// * `messages` - A `MessageReader` for incoming `Event` messages.
    /// * `commands` - Bevy commands to trigger events or insert resources.
    #[allow(clippy::needless_pass_by_value)]
    fn dispatch_toplevel_triggers(mut messages: MessageReader<Event>, mut commands: Commands) {
        for event in messages.read() {
            match event {
                Event::Command { argv } => commands.trigger(CommandTrigger(argv.clone())),

                Event::ConfigRefresh { config } => {
                    info!("{}: Configuration changed.", function_name!());
                    commands.insert_resource(config.clone());
                }

                Event::WindowTitleChanged { window_id } => {
                    trace!("{}: WindowTitleChanged: {window_id:?}", function_name!());
                }
                Event::MenuClosed { window_id } => {
                    trace!("{}: MenuClosed event: {window_id:?}", function_name!());
                }
                Event::DisplayMoved { display_id } => {
                    debug!("{}: Display Moved: {display_id:?}", function_name!());
                }
                Event::DisplayResized { display_id } => {
                    debug!("{}: Display Resized: {display_id:?}", function_name!());
                }
                Event::DisplayConfigured { display_id } => {
                    debug!("{}: Display Configured: {display_id:?}", function_name!());
                }
                Event::SystemWoke { msg } => {
                    debug!("{}: system woke: {msg:?}", function_name!());
                }

                _ => commands.trigger(WMEventTrigger(event.clone())),
            }
        }
    }

    /// Gathers all present displays and spawns them as entities in the Bevy world.
    /// The active display is marked with `FocusedMarker`.
    ///
    /// # Arguments
    ///
    /// * `cid` - The main connection ID resource.
    /// * `commands` - Bevy commands to spawn entities.
    #[allow(clippy::needless_pass_by_value)]
    fn gather_displays(cid: Res<MainConnection>, mut commands: Commands) {
        let Ok(active_display) = Display::active_display_id(cid.0) else {
            error!("{}: Unable to get active display id!", function_name!());
            return;
        };
        for display in Display::present_displays(cid.0) {
            if display.id == active_display {
                commands.spawn((display, FocusedMarker));
            } else {
                commands.spawn(display);
            }
        }
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
        sender: &EventSender,
    ) -> Vec<ProcessRef> {
        let mut out = Vec::new();
        let mut initial_config = None;
        loop {
            match receiver.recv() {
                Ok(Event::ProcessesLoaded | Event::Exit) => break,
                Ok(Event::ApplicationLaunched { psn, observer }) => {
                    out.push(Process::new(&psn, observer.clone()));
                }
                Ok(Event::ConfigRefresh { config }) => {
                    initial_config = Some(config);
                }
                Ok(event) => warn!(
                    "{}: Stray event during initial process gathering: {event:?}",
                    function_name!()
                ),
                Err(err) => {
                    warn!(
                        "{}: Error reading initial processes: {err}",
                        function_name!()
                    );
                    break;
                }
            }
        }

        if let Some(config) = initial_config {
            _ = sender.send(Event::ConfigRefresh { config });
        }
        out
    }

    /// Sets up the initial state of the Bevy world, spawning existing processes.
    ///
    /// # Arguments
    ///
    /// * `world` - The Bevy world.
    /// * `existing_processes` - A mutable vector of `ProcessRef` for processes to add.
    fn initial_setup(world: &mut World, existing_processes: &mut Vec<ProcessRef>) {
        loop {
            let Some(mut process) = existing_processes.pop() else {
                break;
            };
            if !process.is_observable() {
                debug!(
                    "{}: Existing application {} is not observable, ignoring it.",
                    function_name!(),
                    process.name,
                );
                continue;
            }
            debug!(
                "{}: Adding existing process {}",
                function_name!(),
                process.name
            );
            world.spawn((ExistingMarker, BProcess(process)));
        }
        world.spawn(InitializingMarker);
    }

    /// Finishes the initialization process once all initial windows are loaded.
    /// Removes the `InitializingMarker` and triggers a focus check.
    ///
    /// # Arguments
    ///
    /// * `windows` - A query for all windows.
    /// * `fresh_windows` - A query for windows still marked as fresh.
    /// * `initializing` - A query for the initializing marker entity.
    /// * `displays` - A query for all displays.
    /// * `main_cid` - The main connection ID resource.
    /// * `commands` - Bevy commands to despawn entities and send messages.
    #[allow(clippy::needless_pass_by_value)]
    fn finish_setup(
        windows: Query<&Window>,
        fresh_windows: Query<&Window, With<FreshMarker>>,
        initializing: Query<(Entity, &InitializingMarker)>,
        displays: Query<&mut Display>,
        main_cid: Res<MainConnection>,
        mut commands: Commands,
    ) {
        if windows.iter().len() > 0
            && fresh_windows.iter().len() < 1
            && let Ok((entity, _)) = initializing.single()
        {
            commands.entity(entity).despawn();
            info!(
                "{}: Finished Initialization: found {} windows.",
                function_name!(),
                windows.iter().len()
            );
            let find_window = |window_id| {
                windows
                    .iter()
                    .find(|window| window.id() == window_id)
                    .cloned()
            };

            for mut display in displays {
                WindowManager::refresh_display(main_cid.0, &mut display, &find_window);
            }
            commands.write_message(Event::CurrentlyFocused);
        }
    }
}

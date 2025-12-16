use bevy::app::{App as BevyApp, AppExit, Startup, Update};
use bevy::ecs::component::Component;
use bevy::ecs::entity::Entity;
use bevy::ecs::message::{Message, MessageReader, Messages};
use bevy::ecs::query::{Has, With};
use bevy::ecs::resource::Resource;
use bevy::ecs::schedule::IntoScheduleConfigs;
use bevy::ecs::system::{Commands, Populated, Query, Res, Single};
use bevy::ecs::world::World;
use bevy::prelude::Event as BevyEvent;
use bevy::time::{Time, Timer, Virtual};
use log::{debug, error, info, trace, warn};
use objc2::rc::Retained;
use objc2_core_foundation::{CFRetained, CGPoint, CGSize};
use objc2_core_graphics::CGDirectDisplayID;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::thread;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};
use stdext::function_name;

use crate::commands::{Command, process_command_trigger};
use crate::config::Config;
use crate::errors::Result;
use crate::manager::WindowManager;
use crate::platform::{ProcessSerialNumber, WorkspaceObserver};
use crate::process::{Process, ProcessRef};
use crate::skylight::{ConnID, SLSMainConnectionID, WinID};
use crate::triggers::register_triggers;
use crate::util::AXUIWrapper;
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
pub struct RepositionMarker {
    pub origin: CGPoint,
}

#[derive(Component)]
pub struct ResizeMarker {
    pub size: CGSize,
}

#[derive(Component)]
pub struct WindowDraggedMarker(pub Entity);

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
pub struct MainConnection(pub ConnID);

#[derive(Resource)]
pub struct SenderSocket(pub EventSender);

#[derive(Resource)]
pub struct SkipReshuffle(pub bool);

#[derive(Resource)]
pub struct MissionControlActive(pub bool);

#[derive(Resource)]
pub struct FocusFollowsMouse(pub Option<WinID>);

#[derive(BevyEvent)]
pub struct WMEventTrigger(pub Event);

#[derive(BevyEvent)]
pub struct CommandTrigger(pub Command);

#[derive(BevyEvent)]
pub struct ReshuffleAroundTrigger(pub WinID);

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
        let main_cid = unsafe { SLSMainConnectionID() };
        debug!("{}: My connection id: {main_cid}", function_name!());

        let (mut existing_processes, config) = EventHandler::gather_initial_processes(&receiver)?;
        let process_setup = move |world: &mut World| {
            EventHandler::initial_setup(world, &mut existing_processes, config.as_ref());
        };

        let mut app = BevyApp::new();
        app.set_runner(move |app| EventHandler::custom_loop(app, &receiver))
            .init_resource::<Messages<Event>>()
            .insert_resource(Time::<Virtual>::from_max_delta(Duration::from_secs(10)))
            .insert_resource(MainConnection(main_cid))
            .insert_resource(SenderSocket(sender))
            .insert_resource(SkipReshuffle(false))
            .insert_resource(MissionControlActive(false))
            .insert_resource(FocusFollowsMouse(None))
            .add_observer(process_command_trigger)
            .add_systems(Startup, EventHandler::gather_displays)
            .add_systems(Startup, process_setup.after(EventHandler::gather_displays))
            .add_systems(
                Update,
                (
                    // NOTE: To avoid weird timing issues, the dispatcher should be the first one.
                    EventHandler::dispatch_toplevel_triggers,
                    EventHandler::animate_windows,
                    EventHandler::animate_resize_windows,
                ),
            );
        register_triggers(&mut app);
        WindowManager::register_systems(&mut app);
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
                Event::Command { command } => commands.trigger(CommandTrigger(command.clone())),

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
                    "{}: Existing application {} is not observable, ignoring it.",
                    function_name!(),
                    process.name,
                );
            }
        }

        // Run initial setup systems in a one-shot way.
        let existing_apps_setup = [
            world.register_system(WindowManager::add_existing_process),
            world.register_system(WindowManager::add_existing_application),
            world.register_system(EventHandler::finish_setup),
        ];

        let init = existing_apps_setup
            .into_iter()
            .map(|id| world.run_system(id))
            .collect::<std::result::Result<Vec<()>, _>>();
        if let Err(err) = init {
            error!("{}: Error running initial systems: {err}", function_name!());
        }
    }

    /// Finishes the initialization process once all initial windows are loaded.
    ///
    /// # Arguments
    ///
    /// * `apps` - A query for all applications, checking if they are still marked as fresh.
    /// * `windows` - A query for all windows.
    /// * `initializing` - A query for the initializing marker entity.
    /// * `displays` - A query for all displays.
    /// * `main_cid` - The main connection ID resource.
    /// * `commands` - Bevy commands to despawn entities and send messages.
    #[allow(clippy::needless_pass_by_value)]
    fn finish_setup(
        mut windows: Query<(&mut Window, Entity)>,
        displays: Query<(&mut Display, Has<FocusedMarker>)>,
        main_cid: Res<MainConnection>,
        mut commands: Commands,
    ) {
        info!(
            "{}: Finished Initialization: found {} windows.",
            function_name!(),
            windows.iter().len()
        );

        for (mut display, active) in displays {
            WindowManager::refresh_display(main_cid.0, &mut display, &mut windows);

            if active {
                let first_window = display
                    .active_panel(main_cid.0)
                    .ok()
                    .and_then(|panel| panel.first().ok())
                    .and_then(|panel| panel.top());
                if let Some(entity) = first_window {
                    debug!("{}: focusing {entity}", function_name!());
                    commands.entity(entity).insert(FocusedMarker);
                }
            }
        }
    }

    /// Animates window movement.
    /// This is a Bevy system that runs on `Update`. It smoothly moves windows to their target
    /// positions, as indicated by the `RepositionMarker` component.
    ///
    /// # Arguments
    ///
    /// * `windows` - A query for windows with a `RepositionMarker`.
    /// * `displays` - A query for the active display.
    /// * `time` - The Bevy `Time` resource.
    /// * `config` - The optional configuration resource, used for animation speed.
    /// * `commands` - Bevy commands to remove the `RepositionMarker` when animation is complete.
    #[allow(clippy::needless_pass_by_value)]
    fn animate_windows(
        windows: Populated<(&mut Window, Entity, &RepositionMarker)>,
        active_display: Single<&Display, With<FocusedMarker>>,
        time: Res<Time<Virtual>>,
        config: Res<Config>,
        mut commands: Commands,
    ) {
        let move_speed = config
            .options()
            .animation_speed
            // If unset, set it to something high, so the move happens immediately,
            // effectively disabling animation.
            .unwrap_or(1_000_000.0)
            .max(500.0);
        let move_delta = move_speed * time.delta_secs_f64();

        for (mut window, entity, RepositionMarker { origin }) in windows {
            let current = window.frame().origin;
            let mut delta_x = (origin.x - current.x).abs().min(move_delta);
            let mut delta_y = (origin.y - current.y).abs().min(move_delta);
            if delta_x < move_delta && delta_y < move_delta {
                commands.entity(entity).remove::<RepositionMarker>();
                window.reposition(
                    origin.x,
                    origin.y.max(active_display.menubar_height),
                    &active_display.bounds,
                );
                continue;
            }

            if origin.x < current.x {
                delta_x = -delta_x;
            }
            if origin.y < current.y {
                delta_y = -delta_y;
            }
            trace!(
                "{}: window {} dest {:?} delta {move_delta:.0} moving to {:.0}:{:.0}",
                function_name!(),
                window.id(),
                origin,
                current.x + delta_x,
                current.y + delta_y,
            );
            window.reposition(
                current.x + delta_x,
                (current.y + delta_y).max(active_display.menubar_height),
                &active_display.bounds,
            );
        }
    }

    /// Animates window resizing.
    /// This is a Bevy system that runs on `Update`. It resizes windows to their target
    /// dimensions, as indicated by the `ResizeMarker` component.
    ///
    /// # Arguments
    ///
    /// * `windows` - A query for windows with a `ResizeMarker`.
    /// * `displays` - A query for the active display.
    /// * `commands` - Bevy commands to remove the `ResizeMarker` when resizing is complete.
    #[allow(clippy::needless_pass_by_value)]
    fn animate_resize_windows(
        windows: Populated<(&mut Window, Entity, &ResizeMarker)>,
        active_display: Single<&Display, With<FocusedMarker>>,
        mut commands: Commands,
    ) {
        for (mut window, entity, ResizeMarker { size }) in windows {
            let origin = window.frame().origin;
            let width = if origin.x + size.width < active_display.bounds.size.width + 0.4 {
                commands.entity(entity).remove::<ResizeMarker>();
                size.width
            } else {
                active_display.bounds.size.width - origin.x
            };
            debug!(
                "{}: window {} resize {}:{}",
                function_name!(),
                window.id(),
                width,
                size.height,
            );
            window.resize(width, size.height, &active_display.bounds);
        }
    }
}

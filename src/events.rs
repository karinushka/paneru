use bevy::app::{App as BevyApp, AppExit, Startup, Update};
use bevy::ecs::component::Component;
use bevy::ecs::entity::Entity;
use bevy::ecs::hierarchy::Children;
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
use std::io::{Error, ErrorKind, Result};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::thread;
use std::thread::JoinHandle;
use std::time::Duration;
use stdext::function_name;

use crate::app::Application;
use crate::commands::process_command_trigger;
use crate::config::Config;
use crate::manager::WindowManager;
use crate::platform::{ProcessSerialNumber, WorkspaceObserver};
use crate::process::{Process, ProcessRef};
use crate::skylight::{ConnID, SLSMainConnectionID, WinID};
use crate::util::AxuWrapperType;
use crate::windows::{Display, Window};

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
    fn new() -> (Self, Receiver<Event>) {
        let (tx, rx) = channel::<Event>();
        (Self { tx }, rx)
    }

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

#[derive(Component)]
pub struct InitializingMarker;

#[derive(Component)]
pub struct FocusedMarker;

#[derive(Component)]
pub struct FrontSwitchedMarker;

#[derive(Component)]
pub struct FreshMarker;

#[derive(Component)]
pub struct ExistingMarker;

#[derive(Component)]
pub struct BProcess(pub ProcessRef);

#[derive(Resource)]
pub struct MainConnection(pub ConnID);

#[derive(Resource)]
pub struct SenderSocket(pub EventSender);

#[derive(Resource)]
pub struct WindowManagerResource(pub WindowManager);

#[derive(BevyEvent)]
pub struct CommandTrigger(pub Vec<String>);

#[derive(BevyEvent)]
pub struct ApplicationTrigger(pub Event);

#[derive(BevyEvent)]
pub struct MouseTrigger(pub Event);

#[derive(BevyEvent)]
pub struct DisplayChangeTrigger(pub Event);

#[derive(BevyEvent)]
pub struct DisplayAddRemoveTrigger;

pub struct EventHandler;

impl EventHandler {
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
                    .set_runner(move |app| EventHandler::custom_loop(app, &receiver, &quit))
                    .insert_resource(Time::<Virtual>::from_max_delta(Duration::from_secs(10)))
                    .init_resource::<Messages<Event>>()
                    .insert_resource(MainConnection(main_cid))
                    .insert_resource(WindowManagerResource(WindowManager::new(main_cid)))
                    .insert_resource(SenderSocket(sender))
                    .add_observer(process_command_trigger)
                    .add_observer(WindowManager::application_trigger)
                    .add_observer(WindowManager::mouse_trigger)
                    .add_observer(WindowManager::display_change_trigger)
                    .add_observer(WindowManager::display_add_remove_trigger)
                    .add_systems(Startup, EventHandler::gather_displays)
                    .add_systems(Startup, process_setup.after(EventHandler::gather_displays))
                    .add_systems(
                        Update,
                        (
                            EventHandler::dispatch_main_messages,
                            WindowManager::add_existing_process
                                .run_if(any_with_component::<InitializingMarker>),
                            WindowManager::add_existing_application
                                .run_if(any_with_component::<InitializingMarker>),
                            EventHandler::finish_setup
                                .run_if(any_with_component::<InitializingMarker>),
                            WindowManager::add_launched_process,
                            WindowManager::add_launched_application,
                            WindowManager::window_create,
                            WindowManager::front_switched,
                            WindowManager::window_focused,
                        ),
                    )
                    .run();
            }),
        )
    }

    fn custom_loop(mut app: BevyApp, rx: &Receiver<Event>, quit: &Arc<AtomicBool>) -> AppExit {
        const LOOP_MAX_TIMEOUT_MS: u64 = 5000;
        const LOOP_TIMEOUT_STEP: u64 = 5;
        app.finish();
        app.cleanup();

        let mut timeout = LOOP_TIMEOUT_STEP;
        loop {
            match rx.recv_timeout(Duration::from_millis(timeout)) {
                Ok(Event::Exit) => {
                    quit.store(true, std::sync::atomic::Ordering::Relaxed);
                    break;
                }
                Ok(event) => {
                    app.world_mut().write_message::<Event>(event);
                    timeout = LOOP_TIMEOUT_STEP;
                }
                Err(RecvTimeoutError::Timeout) => {
                    timeout = timeout.min(LOOP_MAX_TIMEOUT_MS) + LOOP_TIMEOUT_STEP;
                }
                _ => break,
            }

            app.update();
            if let Some(exit) = app.should_exit() {
                quit.store(true, std::sync::atomic::Ordering::Relaxed);
                return exit;
            }
        }
        AppExit::Success
    }

    /// Processes a single incoming `Event`. It dispatches various event types to the `WindowManager` or other internal handlers.
    ///
    /// # Arguments
    ///
    /// * `event` - The `Event` to process.
    ///
    /// # Returns
    ///
    /// `Ok(())` if the event is processed successfully, otherwise `Err(Error)`.
    #[allow(clippy::needless_pass_by_value)]
    fn dispatch_main_messages(mut messages: MessageReader<Event>, mut commands: Commands) {
        for event in messages.read() {
            match event {
                Event::Command { argv } => commands.trigger(CommandTrigger(argv.clone())),

                Event::MouseDown { point: _ }
                | Event::MouseUp { point: _ }
                | Event::MouseMoved { point: _ }
                | Event::MouseDragged { point: _ } => {
                    commands.trigger(MouseTrigger(event.clone()));
                }

                Event::DisplayChanged | Event::SpaceChanged => {
                    commands.trigger(DisplayChangeTrigger(event.clone()));
                }
                Event::DisplayAdded { display_id: _ } | Event::DisplayRemoved { display_id: _ } => {
                    commands.trigger(DisplayAddRemoveTrigger);
                }

                // Event::ProcessesLoaded => {
                //     info!("{}: === Existing windows loaded ===", function_name!());
                //
                //     // Signal that everything is ready.
                //     commands.write_message::<Event>(Event::ProcessesLoaded);
                //     commands.write_message::<Event>(Event::CurrentlyFocused);
                //     // self.initial_scan = false;
                //     // window_manager.refresh_displays()?;
                //     // return window_manager.set_focused_window();
                // }

                // Event::ApplicationLaunched { psn, observer } => {
                //     if self.initial_scan {
                //         window_manager.add_existing_process(psn, observer.clone());
                //     } else {
                //         debug!("{}: ApplicationLaunched: {psn:?}", function_name!(),);
                //         return window_manager.application_launched(psn, observer.clone());
                //     }
                // }
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

                _ => commands.trigger(ApplicationTrigger(event.clone())),
                // _ => match EventHandler::process_event(event, &mut window_manager.0) {
                //     // TODO: for now we'll treat the Other return values as non-error ones.
                //     // This can be adjusted later with a custom error space.
                //     Err(ref err) if err.kind() == ErrorKind::Other => trace!("{err}"),
                //     Err(err) => error!("{} {err}", function_name!()),
                //     _ => (),
                // },
            }
        }
    }

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
            sender.send(Event::ConfigRefresh { config });
        }
        out
    }

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
            let mut displays = displays.iter().collect::<Vec<_>>();
            WindowManager::refresh_displays(main_cid.0, &mut displays, &find_window);
            commands.write_message(Event::CurrentlyFocused);
        }
    }

    fn print_windows(apps: Query<(&Application, &Children)>, windows: Query<&Window>) {
        for (app, children) in apps {
            println!("Application {}:", app.name().unwrap());
            let windows = children.iter().flat_map(|entity| windows.get(*entity));
            for window in windows {
                println!("\tWindow: {}", window.title().unwrap_or_default());
            }
        }
        println!("done");
    }
}

use bevy::app::{App as BevyApp, AppExit};
use bevy::ecs::observer::On;
use bevy::ecs::resource::Resource;
use bevy::ecs::system::ResMut;
use bevy::prelude::Event as BevyEvent;
use log::{debug, error, info, trace};
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

use crate::commands::process_command;
use crate::config::Config;
use crate::manager::WindowManager;
use crate::platform::{ProcessSerialNumber, WorkspaceObserver};
use crate::skylight::{ConnID, SLSMainConnectionID, WinID};
use crate::util::AxuWrapperType;

#[allow(dead_code)]
#[derive(BevyEvent, Clone, Debug)]
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
    /// Creates a new `EventSender` instance.
    ///
    /// # Arguments
    ///
    /// * `tx` - The sending half of an MPSC channel.
    ///
    /// # Returns
    ///
    /// A new `EventSender`.
    fn new(tx: Sender<Event>) -> Self {
        Self { tx }
    }

    /// Sends an `Event` through the internal MPSC channel.
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

#[derive(Resource)]
pub struct EventHandler {
    quit: Arc<AtomicBool>,
    main_cid: ConnID,
    window_manager: WindowManager,
    initial_scan: bool,
}

impl EventHandler {
    pub fn run() -> (EventSender, Arc<AtomicBool>, JoinHandle<()>) {
        let (tx, rx) = channel::<Event>();
        let sender = EventSender::new(tx);
        let quit = Arc::new(AtomicBool::new(false));

        (
            sender.clone(),
            quit.clone(),
            thread::spawn(move || {
                let main_cid = unsafe { SLSMainConnectionID() };
                debug!("{}: My connection id: {main_cid}", function_name!());
                let state = EventHandler {
                    quit: quit.clone(),
                    main_cid,
                    window_manager: WindowManager::new(sender, main_cid),
                    initial_scan: true,
                };

                BevyApp::new()
                    .set_runner(move |app| EventHandler::custom_loop(app, &rx, &quit))
                    .add_observer(EventHandler::process_event_observer)
                    .insert_resource(state)
                    .run();
            }),
        )
    }

    fn custom_loop(mut app: BevyApp, rx: &Receiver<Event>, quit: &Arc<AtomicBool>) -> AppExit {
        app.finish();
        app.cleanup();

        loop {
            match rx.recv_timeout(Duration::from_millis(500)) {
                Ok(Event::Exit) => {
                    quit.store(true, std::sync::atomic::Ordering::Relaxed);
                    break;
                }
                Ok(event) => {
                    trace!("{}: Event {event:?}", function_name!());
                    app.world_mut().trigger(event);
                }
                Err(RecvTimeoutError::Timeout) => (),
                _ => break,
            }

            app.update();
            if let Some(exit) = app.should_exit() {
                return exit;
            }
        }
        AppExit::Success
    }

    #[allow(clippy::needless_pass_by_value)]
    fn process_event_observer(trigger: On<Event>, mut event_handler: ResMut<EventHandler>) {
        let event = trigger.event().clone();
        match event_handler.process_event(&event) {
            // TODO: for now we'll treat the Other return values as non-error ones.
            // This can be adjusted later with a custom error space.
            Err(ref err) if err.kind() == ErrorKind::Other => trace!("{err}"),
            Err(err) => error!("{} {err}", function_name!()),
            _ => (),
        }
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
    fn process_event(&mut self, event: &Event) -> Result<()> {
        match event {
            Event::ProcessesLoaded => {
                info!(
                    "{}: === Processes loaded - loading windows ===",
                    function_name!()
                );
                self.initial_scan = false;
                self.window_manager.refresh_displays()?;
                return self.window_manager.set_focused_window();
            }

            Event::ApplicationLaunched { psn, observer } => {
                if self.initial_scan {
                    self.window_manager
                        .add_existing_process(psn, observer.clone());
                } else {
                    debug!("{}: ApplicationLaunched: {psn:?}", function_name!(),);
                    return self
                        .window_manager
                        .application_launched(psn, observer.clone());
                }
            }
            Event::WindowTitleChanged { window_id } => {
                trace!("{}: WindowTitleChanged: {window_id:?}", function_name!());
            }

            Event::Command { argv } => {
                process_command(&self.window_manager, argv, &self.quit, self.main_cid);
            }

            Event::MenuClosed { window_id } => {
                trace!("{}: MenuClosed event: {window_id:?}", function_name!());
            }

            Event::DisplayAdded { display_id } => {
                debug!("{}: Display Added: {display_id:?}", function_name!());
                self.window_manager.display_add(*display_id);
            }
            Event::DisplayRemoved { display_id } => {
                debug!("{}: Display Removed: {display_id:?}", function_name!());
                self.window_manager.display_remove(*display_id);
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
            Event::DisplayChanged => {
                debug!("{}: Display Changed", function_name!());
                _ = self.window_manager.reorient_focus();
            }

            Event::SpaceChanged => {
                debug!("{}: Space Changed", function_name!());
                _ = self.window_manager.reorient_focus();
            }
            Event::SystemWoke { msg } => {
                debug!("{}: system woke: {msg:?}", function_name!());
            }

            _ => self.window_manager.process_event(event)?,
        }
        Ok(())
    }
}

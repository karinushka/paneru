use log::{debug, error, info, trace};
use objc2::rc::Retained;
use objc2_core_foundation::{CFRetained, CGPoint};
use objc2_core_graphics::CGDirectDisplayID;
use std::io::{Error, ErrorKind, Result};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::thread;
use std::thread::JoinHandle;
use stdext::function_name;

use crate::commands::process_command;
use crate::config::Config;
use crate::manager::WindowManager;
use crate::platform::{ProcessSerialNumber, WorkspaceObserver};
use crate::skylight::{ConnID, SLSMainConnectionID, WinID};
use crate::util::AxuWrapperType;

#[allow(dead_code)]
#[derive(Debug)]
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

pub struct EventHandler {
    quit: Arc<AtomicBool>,
    tx: EventSender,
    rx: Receiver<Event>,
    main_cid: ConnID,
    window_manager: WindowManager,
    initial_scan: bool,
}

impl EventHandler {
    /// Creates a new `EventHandler` instance. It initializes the main connection ID and `WindowManager`.
    ///
    /// # Returns
    ///
    /// `Ok(Self)` if the `EventHandler` is created successfully, otherwise `Err(Error)`.
    pub fn new() -> Self {
        let main_cid = unsafe { SLSMainConnectionID() };
        debug!("{}: My connection id: {main_cid}", function_name!());

        let (tx, rx) = channel::<Event>();
        let sender = EventSender::new(tx);
        EventHandler {
            quit: AtomicBool::new(false).into(),
            tx: sender.clone(),
            rx,
            main_cid,
            window_manager: WindowManager::new(sender, main_cid),
            initial_scan: true,
        }
    }

    /// Returns a clone of the `EventSender` for sending events to this handler.
    ///
    /// # Returns
    ///
    /// A cloned `EventSender`.
    pub fn sender(&self) -> EventSender {
        self.tx.clone()
    }

    /// Starts the event handler in a new thread.
    ///
    /// # Returns
    ///
    /// A tuple containing:
    /// * An `Arc<AtomicBool>` used to signal the handler to quit.
    /// * A `JoinHandle` for the spawned thread.
    pub fn start(mut self) -> (Arc<AtomicBool>, JoinHandle<()>) {
        let quit = self.quit.clone();
        let handle = thread::spawn(move || {
            self.run();
        });
        (quit, handle)
    }

    /// The main run loop for the event handler. It continuously receives and processes events until an `Exit` event or channel disconnection.
    fn run(&mut self) {
        loop {
            let Ok(event) = self.rx.recv() else {
                break;
            };
            trace!("{}: Event {event:?}", function_name!());

            if matches!(event, Event::Exit) {
                break;
            }

            _ = self
                .process_event(&event)
                .inspect_err(|err| match err.kind() {
                    // TODO: for now we'll treat the Other return values as non-error ones.
                    // This can be adjusted later with a custom error space.
                    ErrorKind::Other => trace!("{err}"),
                    kind => error!("ApplicationLaunched: {kind} {err}"),
                });
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
            Event::Exit => return Ok(()),

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

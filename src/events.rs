use bevy::ecs::message::Message;
use objc2::rc::Retained;
use objc2_core_foundation::{CFRetained, CGPoint};
use objc2_core_graphics::CGDirectDisplayID;
use std::sync::mpsc::{Receiver, Sender, channel};

use crate::commands::Command;
use crate::config::Config;
use crate::errors::Result;
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

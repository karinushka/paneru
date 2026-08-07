use bevy::ecs::message::Message;
use objc2::rc::Retained;
use objc2_core_foundation::{CFRetained, CGPoint};
use objc2_core_graphics::CGDirectDisplayID;
use std::os::unix::net::UnixStream;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};

use crate::commands::Command;
use crate::config::Config;
use crate::ecs::state::StateQueryKind;
use crate::errors::Result;
use crate::platform::{Modifiers, ProcessSerialNumber, WinID, WorkspaceId, WorkspaceObserver};
use crate::util::AXUIWrapper;

/// Where a [`Event::WindowDestroyed`] came from, which decides how far it can be
/// trusted. macOS reports a closing window through two unrelated channels, and
/// only one of them actually means "this window is gone".
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DestroySource {
    /// `kAXUIElementDestroyedNotification` on the window's own AX element. The
    /// element itself has been torn down, so the window is definitively gone.
    Accessibility,
    /// SLS `SpaceWindowDestroyed`. Despite the name this also fires when a
    /// window merely leaves a space, so it has to be confirmed before acting.
    SpaceNotification,
}

/// `Event` represents various system-level and application-specific occurrences that the window manager reacts to.
/// These events drive the core logic of the window manager, from window creation to display changes.
#[allow(dead_code)]
/// The pointer and gesture subset of [`Event`], republished on its own stream.
///
/// Every consumer of [`Event`] pays for the whole stream: fifty-six systems take
/// a `MessageReader<Event>`, and each one fetches the shared `Messages<Event>`
/// resource and walks the entire batch to pick out the one or two variants it
/// wants. With the scheduling overhead gone that resource fetch became the
/// second-largest cost on the main thread, and these are the events that arrive
/// most often — a trackpad produces them at the refresh rate whether or not
/// anything is listening.
///
/// [`demux_input_events`](crate::ecs::systems::demux_input_events) reads the
/// main stream once and republishes these, so the input systems read a stream
/// carrying nothing else and skip the frame entirely when it is empty. The
/// events stay on [`Event`] as well, because the Lua bridge and the IPC
/// subscribers want the undivided stream.
#[derive(Clone, Debug, Message)]
pub struct InputEvent(pub Event);

/// Several variants carry a payload that nothing in the daemon itself reads —
/// the modifier state on a mouse-up, the string on a Dock notification, the pid
/// on an activation. They are there to be forwarded to the scripting bridge,
/// and `src/lua/convert.rs` is the only reader. With the `lua` feature off that
/// reader is compiled out and the dead-code lint is right on its own terms and
/// unhelpful on ours: dropping the fields would mean adding them back the
/// moment the feature is on. Suppressed only in the configuration where they
/// genuinely have no consumer, so the lint still applies to the build that
/// ships.
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
    /// A window has been destroyed. `source` records which notification
    /// reported it; see [`DestroySource`].
    WindowDestroyed {
        window_id: WinID,
        source: DestroySource,
    },
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

    /// A mouse down event has occurred.
    MouseDown {
        point: CGPoint,
        modifiers: Modifiers,
    },
    /// A mouse up event has occurred.
    MouseUp {
        point: CGPoint,
        modifiers: Modifiers,
    },
    /// A mouse drag event has occurred.
    MouseDragged {
        point: CGPoint,
        modifiers: Modifiers,
    },
    /// A mouse move event has occurred.
    MouseMoved {
        point: CGPoint,
        modifiers: Modifiers,
    },

    /// A swipe gesture has been detected.
    Swipe { delta: f64, fingers: usize },

    /// A vertical trackpad gesture (accumulates delta to threshold before firing).
    VerticalSwipe { delta: f64, fingers: usize },

    /// A single scroll wheel tick for vertical workspace switching (fires immediately).
    VerticalScrollTick { delta: f64 },

    /// A mouse scroll has been detected.
    Scroll { delta: f64 },

    /// Fingers have been placed on the touchpad.
    TouchpadDown,
    /// All fingers are up from the touchpad.
    TouchpadUp,

    /// A new space (virtual desktop) has been created.
    SpaceCreated { space_id: WorkspaceId },
    /// A space has been destroyed.
    SpaceDestroyed { space_id: WorkspaceId },
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

    /// The system appearance (Light/Dark mode) has changed.
    ThemeChanged,

    /// A command has been issued to the window manager.
    Command { command: Command },

    /// A structured state query has been issued by a socket client.
    StateQuery {
        kind: StateQueryKind,
        respond_to: Sender<String>,
    },

    /// A socket client has subscribed to line-delimited state events.
    StateSubscribe { stream: Arc<Mutex<UnixStream>> },
}

/// `EventSender` is a thin wrapper around a `std::sync::mpsc::Sender` for `Event`s.
/// It provides a convenient way to send events to the main event loop from various parts of the application.
#[derive(Clone, Debug)]
pub struct EventSender {
    tx: Sender<Event>,
}

impl Event {
    /// Whether this is one of the pointer or gesture events republished on
    /// [`InputEvent`].
    pub fn is_input(&self) -> bool {
        matches!(
            self,
            Event::MouseDown { .. }
                | Event::MouseUp { .. }
                | Event::MouseDragged { .. }
                | Event::MouseMoved { .. }
                | Event::Swipe { .. }
                | Event::VerticalSwipe { .. }
                | Event::VerticalScrollTick { .. }
                | Event::Scroll { .. }
                | Event::TouchpadDown
                | Event::TouchpadUp
        )
    }
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

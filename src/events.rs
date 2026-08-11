use bevy::ecs::message::Message;
use objc2::rc::Retained;
use objc2_core_foundation::{CFRetained, CGPoint};
use objc2_core_graphics::CGDirectDisplayID;
use paneru_shared_types::wire::{Response, ScriptStateRequest};
use std::sync::Arc;
use std::sync::mpsc::{Receiver, Sender, channel};

use crate::commands::Command;
use crate::config::Config;
use crate::ecs::state::StateQueryKind;
use crate::errors::Result;
use crate::platform::{
    EventLoopWaker, Modifiers, Pid, ProcessSerialNumber, WinID, WorkspaceId, WorkspaceObserver,
};
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

/// Where a client's answer goes.
///
/// Bounded to one: exactly one answer is ever sent, and the ECS side answers
/// with `try_send` so it never blocks the main thread.
pub type Reply = async_channel::Sender<Response>;

/// `Event` represents various system-level and application-specific occurrences that the window manager reacts to.
/// These events drive the core logic of the window manager, from window creation to display changes.
#[cfg_attr(not(feature = "lua"), allow(dead_code))]
/// The pointer and gesture subset of [`Event`], republished on its own stream
/// by [`demux_input_events`](crate::ecs::systems::demux_input_events) so
/// input-heavy systems don't have to scan the full event stream. Events still
/// appear on [`Event`] too, for the Lua bridge and IPC subscribers.
#[derive(Clone, Debug, Message)]
pub struct InputEvent(pub Event);

/// Several variants carry a payload used only by the Lua bridge
/// (`src/lua/convert.rs`). The dead-code lint is suppressed only when the
/// `lua` feature is off, so it still applies to the build that ships.
#[cfg_attr(not(feature = "lua"), allow(dead_code))]
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
    /// An application has become the active (frontmost) application. Carries
    /// the pid for subscribers; Paneru's own focus handling uses
    /// [`Event::ApplicationFrontSwitched`] instead.
    ApplicationActivated { pid: i32 },
    /// An application has stopped being the active application.
    ApplicationDeactivated { pid: i32 },
    /// An application has become visible.
    ApplicationVisible { pid: i32 },
    /// An application has become hidden.
    ApplicationHidden { pid: i32 },

    /// A window has been created.
    WindowCreated { element: CFRetained<AXUIWrapper> },
    /// A window has been fully spawned and populated in the window manager.
    WindowSpawned {
        window_id: WinID,
        pid: Pid,
        app_name: String,
        bundle_id: String,
        title: String,
        frame: paneru_shared_types::state::Frame,
        floating: bool,
        managed: bool,
    },
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

    /// A structured state query has been issued by a client.
    StateQuery {
        kind: StateQueryKind,
        respond_to: Reply,
    },

    /// A client has asked for the window set: the same layout value a
    /// `paneru.windows` handler is given inside the daemon.
    WindowSetQuery { respond_to: Reply },

    /// A client has subscribed to state events. Carries the channel they are
    /// pushed to, which outlives the request that delivered it.
    StateSubscribe {
        subscriber: Arc<paneru_mach_ipc::Subscriber>,
    },

    /// A client has read or written the script state store. Answered
    /// from the same store the embedded Lua runtime uses, so the two see each
    /// other's writes.
    ScriptState {
        request: ScriptStateRequest,
        respond_to: Reply,
    },
}

/// `EventSender` is a thin wrapper around a `std::sync::mpsc::Sender` for `Event`s.
/// It provides a convenient way to send events to the main event loop from various parts of the application.
#[derive(Clone, Debug)]
pub struct EventSender {
    tx: Sender<Event>,
    /// Ends the Cocoa pump's wait once the event is queued. Shared so a
    /// wake-up from any producer covers every event queued before it.
    waker: Arc<EventLoopWaker>,
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
        (
            Self {
                tx,
                waker: Arc::new(EventLoopWaker::new()),
            },
            rx,
        )
    }

    /// The waker shared by every clone of this sender.
    pub fn waker(&self) -> &Arc<EventLoopWaker> {
        &self.waker
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
        self.tx.send(event)?;
        // After the queue push, so the pump cannot wake to an empty channel and
        // go back to sleep past the event that woke it.
        self.waker.wake();
        Ok(())
    }
}

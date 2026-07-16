use bevy::ecs::message::Message;
use objc2::rc::Retained;
use objc2_core_foundation::{
    CFRetained, CFRunLoop, CFRunLoopSource, CFRunLoopSourceContext, CGPoint, kCFRunLoopDefaultMode,
};
use objc2_core_graphics::CGDirectDisplayID;
use std::ffi::c_void;
use std::os::unix::net::UnixStream;
use std::ptr::{from_ref, null_mut};
use std::sync::atomic::{AtomicPtr, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, RecvError, Sender, TryRecvError, channel};
use std::sync::{Arc, Mutex};

use crate::commands::Command;
use crate::config::Config;
use crate::ecs::state::StateQueryKind;
use crate::errors::Result;
use crate::platform::{Modifiers, ProcessSerialNumber, WinID, WorkspaceId, WorkspaceObserver};
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
    /// Physical contact ended; a native momentum phase may still follow.
    TouchpadPhysicalUp,
    /// Native momentum began for the current physical gesture.
    TouchpadMomentumStart,
    /// The full touchpad gesture, including native momentum, has ended.
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
    wake: Arc<EventWake>,
}

#[derive(Debug)]
struct EventWake {
    generation: AtomicU64,
    source: AtomicPtr<CFRunLoopSource>,
    active_signals: AtomicUsize,
}

impl Default for EventWake {
    fn default() -> Self {
        Self {
            generation: AtomicU64::new(0),
            source: AtomicPtr::new(null_mut()),
            active_signals: AtomicUsize::new(0),
        }
    }
}

/// Receiver paired with [`EventSender`]. The generation counter makes the
/// queue-before-sleep protocol testable; the registered run-loop source closes
/// the final check-to-sleep race because its signalled state is latched.
pub struct EventReceiver {
    rx: Receiver<Event>,
    wake: Arc<EventWake>,
}

impl EventReceiver {
    pub fn recv(&self) -> std::result::Result<Event, RecvError> {
        self.rx.recv()
    }

    pub fn try_recv(&self) -> std::result::Result<Event, TryRecvError> {
        self.rx.try_recv()
    }

    pub(crate) fn generation(&self) -> u64 {
        self.wake.generation.load(Ordering::Acquire)
    }
}

pub(crate) struct EventWakeSource {
    source: CFRetained<CFRunLoopSource>,
    wake: Arc<EventWake>,
}

impl Drop for EventWakeSource {
    fn drop(&mut self) {
        self.wake.source.swap(null_mut(), Ordering::AcqRel);
        while self.wake.active_signals.load(Ordering::Acquire) != 0 {
            std::thread::yield_now();
        }
        if let Some(main_loop) = CFRunLoop::main() {
            main_loop.remove_source(Some(&self.source), unsafe { kCFRunLoopDefaultMode });
        }
        self.source.invalidate();
    }
}

unsafe extern "C-unwind" fn consume_event_wake(_: *mut c_void) {}

impl EventSender {
    /// Creates a new `EventSender` and its corresponding `Receiver`.
    /// This function initializes an MPSC channel.
    ///
    /// # Returns
    ///
    /// A tuple containing the `EventSender` and `Receiver` for the created channel.
    pub fn new() -> (Self, EventReceiver) {
        let (tx, rx) = channel::<Event>();
        let wake = Arc::new(EventWake::default());
        (
            Self {
                tx,
                wake: Arc::clone(&wake),
            },
            EventReceiver { rx, wake },
        )
    }

    pub(crate) fn install_main_run_loop_source(&self) -> Option<EventWakeSource> {
        let mut context = CFRunLoopSourceContext {
            version: 0,
            info: null_mut(),
            retain: None,
            release: None,
            copyDescription: None,
            equal: None,
            hash: None,
            schedule: None,
            cancel: None,
            perform: Some(consume_event_wake),
        };
        let source = unsafe { CFRunLoopSource::new(None, 0, &raw mut context) }?;
        let main_loop = CFRunLoop::main()?;
        main_loop.add_source(Some(&source), unsafe { kCFRunLoopDefaultMode });
        self.wake.source.store(
            from_ref::<CFRunLoopSource>(&source).cast_mut(),
            Ordering::Release,
        );
        Some(EventWakeSource {
            source,
            wake: Arc::clone(&self.wake),
        })
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
        self.wake.generation.fetch_add(1, Ordering::Release);
        self.wake.active_signals.fetch_add(1, Ordering::AcqRel);
        let source = self.wake.source.load(Ordering::Acquire);
        if !source.is_null() {
            // Main-thread sends must signal too: AppKit callbacks can run while
            // PlatformCallbacks is draining queued NSEvents immediately before
            // entering CFRunLoop::run_in_mode. Without a latched source, that
            // subsequent run could sleep despite the newly queued ECS event.
            // The source callback itself is empty and channel draining coalesces
            // events, so it adds at most one wake turn, not persistent frame work.
            // SAFETY: `EventWakeSource` owns the source until it first clears
            // this pointer with Release ordering. Core Foundation permits
            // signalling a run-loop source from arbitrary threads.
            unsafe { &*source }.signal();
        }
        self.wake.active_signals.fetch_sub(1, Ordering::Release);
        if let Some(main_loop) = CFRunLoop::main() {
            main_loop.wake_up();
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{Event, EventSender};
    use std::sync::{Arc, Barrier};
    use std::thread;

    #[test]
    fn generation_latches_queued_before_sleep_and_during_ecs() {
        let (sender, receiver) = EventSender::new();
        let before = receiver.generation();
        sender.send(Event::ApplicationActivated).unwrap();
        assert!(receiver.generation() > before);
        assert!(matches!(
            receiver.try_recv(),
            Ok(Event::ApplicationActivated)
        ));

        let barrier = Arc::new(Barrier::new(2));
        let worker_barrier = Arc::clone(&barrier);
        let worker = thread::spawn(move || {
            worker_barrier.wait();
            sender.send(Event::ApplicationDeactivated).unwrap();
        });
        let before_ecs = receiver.generation();
        barrier.wait();
        worker.join().unwrap();
        assert!(receiver.generation() > before_ecs);
        assert!(matches!(
            receiver.try_recv(),
            Ok(Event::ApplicationDeactivated)
        ));
    }
}

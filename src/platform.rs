use std::sync::OnceLock;

use accessibility_sys::{AXError, AXObserverRef, AXUIElementRef};
use objc2::MainThreadMarker;
use objc2::rc::{Retained, autoreleasepool};
use objc2_app_kit::{
    NSApplication, NSApplicationActivationPolicy, NSEvent, NSEventMask, NSEventModifierFlags,
    NSEventType,
};
use objc2_core_foundation::CFString;
use objc2_foundation::{NSDate, NSDefaultRunLoopMode, NSPoint, NSProcessInfo};
use std::ffi::{c_short, c_void};
use std::pin::Pin;
use std::ptr::NonNull;
use std::sync::atomic::{AtomicBool, Ordering};
use tracing::error;

use crate::config::{CONFIGURATION_FILE, Config};
use crate::errors::{Error, Result};
use crate::events::{Event, EventSender};
use crate::manager::{check_ax_privilege, check_separate_spaces};
use crate::platform::display::PinnedDisplayHandler;
use crate::platform::input::PinnedInputHandler;
use crate::platform::notify::{NotifyHandler, PinnedNotifyHandler};
use crate::platform::process::PinnedProcessHandler;
use display::DisplayHandler;
use input::InputHandler;
use mission_control::MissionControlHandler;
use process::ProcessHandler;
pub use process::ProcessSerialNumber;
pub use workspace::WorkspaceObserver;

pub(crate) mod app_launcher;
mod display;
pub(crate) mod input;
mod mission_control;
pub mod notify;
mod process;
pub mod service;
mod workspace;

/// Type alias for `OSStatus`, a 32-bit integer error code used by macOS system services.
pub type OSStatus = i32;
/// Type alias for `WinID`, a 32-bit integer representing a window identifier in `SkyLight`.
pub type WinID = i32;
/// Type alias for `ConnID`, a 64-bit integer representing a connection identifier in `SkyLight`.
pub type ConnID = i64;

pub type Pid = i32;
/// Type alias for a raw pointer to an immutable `CFString`.
pub type CFStringRef = *const CFString;

pub type WorkspaceId = u64;

bitflags::bitflags! {
    #[derive(Clone, Copy, Debug, PartialEq)]
    pub struct Modifiers: u16 {
        const LALT   = 1 << 0;
        const RALT   = 1 << 1;
        const LSHIFT = 1 << 2;
        const RSHIFT = 1 << 3;
        const LCMD   = 1 << 4;
        const RCMD   = 1 << 5;
        const LCTRL  = 1 << 6;
        const RCTRL  = 1 << 7;
        const FN     = 1 << 8;
        const ALT   = Self::LALT.bits() | Self::RALT.bits();
        const SHIFT = Self::LSHIFT.bits() | Self::RSHIFT.bits();
        const CMD   = Self::LCMD.bits() | Self::RCMD.bits();
        const CTRL  = Self::LCTRL.bits() | Self::RCTRL.bits();
    }
}

/// Type alias for the callback function signature used by `AXObserver`.
///
/// # Arguments
///
/// * `observer` - The `AXObserverRef` that invoked the callback.
/// * `element` - The `AXUIElementRef` associated with the notification.
/// * `notification` - The raw `CFStringRef` representing the notification name.
/// * `refcon` - A raw pointer to user-defined context data.
type AXObserverCallback = unsafe extern "C" fn(
    observer: AXObserverRef,
    element: AXUIElementRef,
    notification: CFStringRef,
    refcon: *mut c_void,
);

unsafe extern "C" {
    /// Creates an `AXObserver` for a given application process ID and a callback function.
    ///
    /// # Arguments
    ///
    /// * `application` - The process ID (`Pid`) of the application to observe.
    /// * `callback` - The `AXObserverCallback` function to be invoked when notifications occur.
    /// * `out_observer` - A mutable reference to an `AXObserverRef` where the created observer will be stored.
    ///
    /// # Returns
    ///
    /// An `AXError` indicating success or failure.
    pub fn AXObserverCreate(
        application: Pid,
        callback: AXObserverCallback,
        out_observer: &mut AXObserverRef,
    ) -> AXError;

    /// Adds a notification to an `AXObserver` for a specific UI element.
    ///
    /// # Arguments
    ///
    /// * `observer` - The `AXObserverRef` to add the notification to.
    /// * `element` - The `AXUIElementRef` to observe for the notification.
    /// * `notification` - A reference to a `CFString` representing the notification name (e.g., `kAXWindowMovedNotification`).
    /// * `refcon` - A raw pointer to user-defined context data, typically a `struct` instance.
    ///
    /// # Returns
    ///
    /// An `AXError` indicating success or failure, including `kAXErrorNotificationAlreadyRegistered`.
    pub fn AXObserverAddNotification(
        observer: AXObserverRef,
        element: AXUIElementRef,
        notification: &CFString,
        refcon: *mut c_void,
    ) -> AXError;

    /// Removes a notification from an `AXObserver` for a specific UI element.
    ///
    /// # Arguments
    ///
    /// * `observer` - The `AXObserverRef` from which to remove the notification.
    /// * `element` - The `AXUIElementRef` from which to remove the notification.
    /// * `notification` - A reference to a `CFString` representing the notification name.
    ///
    /// # Returns
    ///
    /// An `AXError` indicating success or failure.
    pub fn AXObserverRemoveNotification(
        observer: AXObserverRef,
        element: AXUIElementRef,
        notification: &CFString,
    ) -> AXError;
}

/// Tag on the synthetic event [`EventLoopWaker`] posts, so the pump can tell it
/// apart from a real one and drop it. Arbitrary; just has to be ours.
const WAKE_EVENT_SUBTYPE: c_short = 0x7061;

/// Wakes the Cocoa event pump from whatever thread produced an [`Event`].
///
/// `nextEventMatchingMask:untilDate:inMode:dequeue:` only returns early when it
/// dequeues an actual `NSEvent` — signalling a run loop source is not enough to
/// end the wait. Posting a real `ApplicationDefined` event is what does it.
#[derive(Debug)]
pub struct EventLoopWaker {
    /// Set while a wake-up has been posted but not yet consumed, so a burst of
    /// events costs one posted event rather than one per send.
    pending: AtomicBool,
    /// The shared `NSApplication`, held as a pointer because the waker is cloned
    /// out to threads with no `MainThreadMarker` (the socket reader, notably).
    /// The only message ever sent through it is `postEvent:atStart:`, which is
    /// documented as callable from any thread.
    ///
    /// Empty until [`Self::install`] runs, because `postEvent:` silently drops
    /// events until the app has finished launching.
    app: OnceLock<NonNull<NSApplication>>,
}

// SAFETY: the only message sent through `app` is `postEvent:atStart:`, which is
// documented as thread-safe. See the field comment.
unsafe impl Send for EventLoopWaker {}
unsafe impl Sync for EventLoopWaker {}

impl EventLoopWaker {
    pub fn new() -> Self {
        Self {
            pending: AtomicBool::new(false),
            app: OnceLock::new(),
        }
    }

    /// Points the waker at the running application. Called once, on the main
    /// thread, after `finishLaunching`.
    fn install(&self, app: &NSApplication) {
        _ = self.app.set(NonNull::from(app));
    }

    /// Ends the pump's current wait, unless one is already posted.
    pub fn wake(&self) {
        let Some(app) = self.app.get() else {
            return;
        };
        if self.pending.swap(true, Ordering::AcqRel) {
            return;
        }
        let Some(event) = NSEvent::otherEventWithType_location_modifierFlags_timestamp_windowNumber_context_subtype_data1_data2(
            NSEventType::ApplicationDefined,
            NSPoint::ZERO,
            NSEventModifierFlags::empty(),
            0.0,
            0,
            None,
            WAKE_EVENT_SUBTYPE,
            0,
            0,
        ) else {
            // Nothing else will clear the flag, so drop it here or the pump is
            // never woken again.
            self.pending.store(false, Ordering::Release);
            error!("unable to create wake-up event");
            return;
        };
        // SAFETY: `postEvent:atStart:` is callable from any thread.
        let app = unsafe { app.as_ref() };
        // Appended, not pushed to the front, so it can't reorder real input.
        app.postEvent_atStart(&event, false);
    }

    /// Re-arms the waker. Must be called before the pump waits, not after:
    /// clearing afterward could erase a wake-up that arrived in between.
    fn rearm(&self) {
        self.pending.store(false, Ordering::Release);
    }

    /// True for the synthetic event [`Self::wake`] posts.
    fn is_wake_event(event: &NSEvent) -> bool {
        // `subtype` is only meaningful for some event types, so the type check
        // must come first and short-circuit.
        event.r#type() == NSEventType::ApplicationDefined && event.subtype().0 == WAKE_EVENT_SUBTYPE
    }
}

impl Default for EventLoopWaker {
    fn default() -> Self {
        Self::new()
    }
}

/// `PlatformCallbacks` aggregates and manages all platform-specific event handlers and observers.
/// It serves as the central point for setting up and running macOS-specific interactions with the window manager.
pub struct PlatformCallbacks {
    pub main_thread_marker: MainThreadMarker,
    cocoa_app: Retained<NSApplication>,
    /// The main `EventSender` for dispatching events across the application.
    events: EventSender,
    /// Handler for Carbon process events.
    process_handler: Option<PinnedProcessHandler>,
    /// Handler for low-level input events (keyboard, mouse, gestures).
    event_handler: Option<PinnedInputHandler>,
    /// Observer for `NSWorkspace` and distributed notifications.
    workspace_observer: Retained<WorkspaceObserver>,
    /// Handler for Mission Control accessibility events.
    mission_control_observer: MissionControlHandler,
    /// Handler for Core Graphics display reconfiguration events.
    display_handler: Option<PinnedDisplayHandler>,
    notify_handler: Option<PinnedNotifyHandler>,
}

impl PlatformCallbacks {
    /// Creates a new `PlatformCallbacks` instance, initializing various handlers and watchers.
    /// This involves setting up `Config`, `WorkspaceObserver`, `ProcessHandler`, `InputHandler`,
    /// `MissionControlHandler`, `DisplayHandler`, and `FsEventWatcher`.
    ///
    /// # Arguments
    ///
    /// * `events` - An `EventSender` to be used by all platform callbacks.
    ///
    /// # Returns
    ///
    /// `Ok(std::pin::Pin<Box<Self>>)` if the instance is created successfully, otherwise `Err(Error)`.
    pub fn new(events: EventSender) -> Pin<Box<Self>> {
        // This is required to receive some Cocoa notifications into Carbon code, like
        // NSWorkspaceActiveSpaceDidChangeNotification and
        // NSWorkspaceActiveDisplayDidChangeNotification
        // Found on: https://stackoverflow.com/questions/68893386/unable-to-receive-nsworkspaceactivespacedidchangenotification-specifically-but
        let main_thread_marker = MainThreadMarker::new().unwrap();
        let cocoa_app = NSApplication::sharedApplication(main_thread_marker);
        cocoa_app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);
        cocoa_app.finishLaunching();
        NSApplication::load();

        // Only now is there an event queue for `postEvent:` to reach.
        events.waker().install(&cocoa_app);

        let workspace_observer = WorkspaceObserver::new(events.clone());
        Box::pin(PlatformCallbacks {
            main_thread_marker,
            cocoa_app,
            process_handler: None,
            event_handler: None,
            workspace_observer,
            mission_control_observer: MissionControlHandler::new(events.clone()),
            display_handler: None,
            notify_handler: None,
            events,
        })
    }

    /// Sets up and starts all platform-specific handlers, including input, display, Mission Control, workspace, and process handlers.
    /// It also performs initial checks for Accessibility permissions and sends a `ProcessesLoaded` event.
    ///
    /// # Returns
    ///
    /// `Ok(())` if all handlers are set up successfully, otherwise `Err(Error)`.
    ///
    /// # Side Effects
    ///
    /// - Starts the Cocoa run loop.
    /// - Requests Accessibility permissions if not already granted.
    /// - Activates `CGEventTap`, `CGDisplayReconfigurationCallback`, `AXObserver` for Mission Control,
    ///   `NSWorkspace` observers, and Carbon process event handlers.
    pub fn setup_handlers(&mut self) -> Result<()> {
        if !check_ax_privilege() {
            return Err(Error::PermissionDenied(
                "Accessibility permissions are required. Please enable them in System Preferences -> Security & Privacy -> Privacy -> Accessibility.".to_string(),
            ));
        }

        if !check_separate_spaces() {
            error!("Option 'display has separate spaces' disabled.");
            return Err(Error::InvalidConfig(
                "Option 'display has separate spaces' disabled.".to_string(),
            ));
        }

        let config = Config::load(CONFIGURATION_FILE.as_deref())?;
        self.events.send(Event::InitialConfig(config.clone()))?;
        self.event_handler = Some(InputHandler::new(self.events.clone(), config).start()?);

        self.notify_handler = Some(NotifyHandler::new(self.events.clone()).start()?);
        self.display_handler = Some(DisplayHandler::new(self.events.clone()).start()?);
        self.process_handler = Some(
            ProcessHandler::new(self.events.clone(), self.workspace_observer.clone()).start()?,
        );
        self.mission_control_observer.observe()?;
        self.workspace_observer.start();

        self.events.send(Event::ProcessesLoaded)
    }

    /// Returns `true` when at least one event was dispatched this pass.
    pub fn pump_cocoa_event_loop(&mut self, timeout: f64) -> bool {
        // Re-armed before the wait, not after it — see [`EventLoopWaker::rearm`].
        self.events.waker().rearm();

        autoreleasepool(|_| {
            // Only the *first* dequeue may block. Once something has arrived,
            // the rest of the queue is drained with an already-past deadline so
            // the pass ends as soon as the queue is empty.
            let mut deadline = NSDate::dateWithTimeIntervalSinceNow(timeout);
            let mut dispatched = false;

            // nextEventMatchingMask:untilDate:inMode:dequeue:
            // This is the core of the Cocoa event loop.
            while let Some(event) = unsafe {
                self.cocoa_app
                    .nextEventMatchingMask_untilDate_inMode_dequeue(
                        NSEventMask::Any,
                        Some(&deadline),
                        NSDefaultRunLoopMode,
                        true, // Dequeue so we can handle it
                    )
            } {
                deadline = NSDate::distantPast();

                // Synthetic, only meant to end the wait above; must not count
                // as dispatched or an idle wake-up would trigger `updateWindows`.
                if EventLoopWaker::is_wake_event(&event) {
                    continue;
                }

                // Dispatch the event to the system
                self.cocoa_app.sendEvent(&event);
                dispatched = true;
            }

            // `updateWindows` walks every window sending `update`; skip it on an
            // idle pass that dequeued nothing.
            if dispatched {
                self.cocoa_app.updateWindows();
            }
            dispatched
        })
    }
}

impl Modifiers {
    /// Returns true if `event` satisfies this binding's modifier requirements.
    /// For each modifier group (alt, shift, cmd, ctrl):
    ///   - If the binding requires the group, the event must have at least one matching side bit.
    ///   - If the binding does NOT require the group, the event must not have any bits from that group.
    pub fn matches(self, event: Modifiers) -> bool {
        const GROUPS: [Modifiers; 5] = [
            Modifiers::ALT,
            Modifiers::SHIFT,
            Modifiers::CMD,
            Modifiers::CTRL,
            Modifiers::FN,
        ];

        for group in GROUPS {
            let bind_group = self & group;
            let event_group = event & group;

            if bind_group.is_empty() {
                if !event_group.is_empty() {
                    return false;
                }
            } else {
                if (bind_group & event_group).is_empty() {
                    return false;
                }
                // Reject extra bits in this group (e.g. `lcmd` still held when the binding only allows `rcmd`).
                // e.g. rcmd does not match `lcmd + <key>` while lcmd is also held.
                if event_group | bind_group != bind_group {
                    return false;
                }
            }
        }
        true
    }
}

/// Cached macOS major version (e.g. 14 for Sonoma, 15 for Sequoia).
pub fn macos_major_version() -> u32 {
    static VERSION: OnceLock<u32> = OnceLock::new();
    *VERSION.get_or_init(|| {
        let version = NSProcessInfo::processInfo().operatingSystemVersion();
        u32::try_from(version.majorVersion).unwrap_or(16)
    })
}

#[cfg(test)]
mod tests {
    use super::Modifiers;

    #[test]
    fn macos_major_version_returns_valid() {
        let v = super::macos_major_version();
        assert!(v >= 13, "expected macOS 13+, got {v}");
    }

    #[test]
    fn matches_empty_binding_requires_no_modifiers() {
        assert!(Modifiers::empty().matches(Modifiers::empty()));
    }

    #[test]
    fn matches_empty_binding_rejects_any_modifier() {
        assert!(!Modifiers::empty().matches(Modifiers::LALT));
        assert!(!Modifiers::empty().matches(Modifiers::RSHIFT));
        assert!(!Modifiers::empty().matches(Modifiers::LCMD));
        assert!(!Modifiers::empty().matches(Modifiers::RCTRL));
        assert!(!Modifiers::empty().matches(Modifiers::FN));
    }

    #[test]
    fn matches_group_binding_accepts_either_side() {
        let alt_cmd = Modifiers::ALT | Modifiers::CMD;
        assert!(alt_cmd.matches(Modifiers::LALT | Modifiers::LCMD));
        assert!(alt_cmd.matches(Modifiers::RALT | Modifiers::RCMD));
        assert!(alt_cmd.matches(Modifiers::LALT | Modifiers::RCMD));
    }

    #[test]
    fn matches_group_binding_requires_all_groups() {
        let alt_cmd = Modifiers::ALT | Modifiers::CMD;
        assert!(!alt_cmd.matches(Modifiers::LALT));
        assert!(!alt_cmd.matches(Modifiers::LCMD));
        assert!(!alt_cmd.matches(Modifiers::LALT | Modifiers::LCMD | Modifiers::LSHIFT));
    }

    #[test]
    fn matches_rejects_extra_unlisted_groups() {
        let want_alt = Modifiers::ALT;
        assert!(!want_alt.matches(Modifiers::LALT | Modifiers::LSHIFT));
        assert!(!want_alt.matches(Modifiers::LALT | Modifiers::LCTRL));
        assert!(!want_alt.matches(Modifiers::LALT | Modifiers::FN));
    }

    #[test]
    fn matches_specific_side_does_not_match_other_side() {
        let left_alt = Modifiers::LALT;
        assert!(left_alt.matches(Modifiers::LALT));
        assert!(!left_alt.matches(Modifiers::RALT));
    }

    #[test]
    fn matches_specific_side_rejects_both_sides_at_once() {
        assert!(!Modifiers::RCMD.matches(Modifiers::LCMD | Modifiers::RCMD));
        assert!(!Modifiers::LCTRL.matches(Modifiers::LCTRL | Modifiers::RCTRL));
    }

    #[test]
    fn matches_right_specific_chord_rejects_left_modifier_in_same_group() {
        let east = Modifiers::RCMD | Modifiers::RCTRL | Modifiers::RSHIFT | Modifiers::RALT;
        assert!(
            east.matches(Modifiers::RCMD | Modifiers::RCTRL | Modifiers::RSHIFT | Modifiers::RALT)
        );
        assert!(!east.matches(
            Modifiers::LCMD
                | Modifiers::RCMD
                | Modifiers::RCTRL
                | Modifiers::RSHIFT
                | Modifiers::RALT
        ));
    }

    #[test]
    fn matches_all_five_groups() {
        let all =
            Modifiers::ALT | Modifiers::SHIFT | Modifiers::CMD | Modifiers::CTRL | Modifiers::FN;
        assert!(all.matches(
            Modifiers::LALT
                | Modifiers::RSHIFT
                | Modifiers::LCMD
                | Modifiers::RCTRL
                | Modifiers::FN
        ));
        assert!(
            !all.matches(Modifiers::LALT | Modifiers::LSHIFT | Modifiers::LCMD | Modifiers::FN)
        );
    }

    #[test]
    fn matches_fn_modifier() {
        let want_fn = Modifiers::FN;
        assert!(want_fn.matches(Modifiers::FN));
        assert!(!want_fn.matches(Modifiers::empty()));
        assert!(!want_fn.matches(Modifiers::LALT | Modifiers::FN));
    }

    #[test]
    fn matches_fn_combined_with_other_groups() {
        let fn_alt = Modifiers::FN | Modifiers::ALT;
        assert!(fn_alt.matches(Modifiers::FN | Modifiers::LALT));
        assert!(fn_alt.matches(Modifiers::FN | Modifiers::RALT));
        assert!(!fn_alt.matches(Modifiers::FN));
        assert!(!fn_alt.matches(Modifiers::LALT));
        assert!(!fn_alt.matches(Modifiers::FN | Modifiers::LALT | Modifiers::LSHIFT));
    }
}

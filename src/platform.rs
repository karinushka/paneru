use accessibility_sys::{AXError, AXObserverRef, AXUIElementRef};
use log::{error, info};
use objc2::rc::{Retained, autoreleasepool};
use objc2_app_kit::NSApplication;
use objc2_core_foundation::{CFRunLoop, CFString, kCFRunLoopDefaultMode};
use std::ffi::c_void;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use stdext::function_name;

use crate::config::{CONFIGURATION_FILE, Config};
use crate::errors::{Error, Result};
use crate::events::{Event, EventSender};
use crate::manager::{check_ax_privilege, check_separate_spaces};
use display::DisplayHandler;
use input::InputHandler;
use mission_control::MissionControlHandler;
use process::ProcessHandler;
pub use process::ProcessSerialNumber;
pub use workspace::WorkspaceObserver;

mod display;
mod input;
mod mission_control;
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

/// `PlatformCallbacks` aggregates and manages all platform-specific event handlers and observers.
/// It serves as the central point for setting up and running macOS-specific interactions with the window manager.
pub struct PlatformCallbacks {
    /// The main `EventSender` for dispatching events across the application.
    events: EventSender,
    /// Handler for Carbon process events.
    process_handler: ProcessHandler,
    /// Handler for low-level input events (keyboard, mouse, gestures).
    event_handler: InputHandler,
    /// Observer for `NSWorkspace` and distributed notifications.
    workspace_observer: Retained<WorkspaceObserver>,
    /// Handler for Mission Control accessibility events.
    mission_control_observer: MissionControlHandler,
    /// Handler for Core Graphics display reconfiguration events.
    display_handler: DisplayHandler,
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
    pub fn new(events: EventSender) -> Result<std::pin::Pin<Box<Self>>> {
        let config = Config::new(CONFIGURATION_FILE.as_path())?;
        events.send(Event::InitialConfig(config.clone()))?;

        let workspace_observer = WorkspaceObserver::new(events.clone());
        Ok(Box::pin(PlatformCallbacks {
            process_handler: ProcessHandler::new(events.clone(), workspace_observer.clone()),
            event_handler: InputHandler::new(events.clone(), config.clone()),
            workspace_observer,
            mission_control_observer: MissionControlHandler::new(events.clone()),
            display_handler: DisplayHandler::new(events.clone()),
            events,
        }))
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
        // This is required to receive some Cocoa notifications into Carbon code, like
        // NSWorkspaceActiveSpaceDidChangeNotification and
        // NSWorkspaceActiveDisplayDidChangeNotification
        // Found on: https://stackoverflow.com/questions/68893386/unable-to-receive-nsworkspaceactivespacedidchangenotification-specifically-but
        if !NSApplication::load() {
            return Err(Error::PermissionDenied(format!(
                "{}: Can not startup Cocoa runloop from Carbon code.",
                function_name!()
            )));
        }

        if !check_ax_privilege() {
            return Err(Error::PermissionDenied(format!(
                "{}: Accessibility permissions are required. Please enable them in System Preferences -> Security & Privacy -> Privacy -> Accessibility.",
                function_name!()
            )));
        }

        if !check_separate_spaces() {
            error!(
                "{}: Option 'display has separate spaces' disabled.",
                function_name!()
            );
            return Err(Error::InvalidConfig(
                "Option 'display has separate spaces' disabled.".to_string(),
            ));
        }

        self.event_handler.start()?;
        self.display_handler.start()?;
        self.mission_control_observer.observe()?;
        self.workspace_observer.start();
        self.process_handler.start()?;

        self.events.send(Event::ProcessesLoaded)
    }

    /// Runs the main event loop for platform callbacks. It continuously processes events until the `quit` signal is set.
    /// This function enters a `CFRunLoop` and waits for events, periodically checking the `quit` flag.
    ///
    /// # Arguments
    ///
    /// * `quit` - An `Arc<AtomicBool>` used to signal the run loop to exit.
    pub fn run(&mut self, quit: &Arc<AtomicBool>) {
        info!("{}: Starting run loop...", function_name!());
        loop {
            if quit.load(std::sync::atomic::Ordering::Relaxed) {
                break;
            }
            autoreleasepool(|_| unsafe {
                CFRunLoop::run_in_mode(kCFRunLoopDefaultMode, 3.0, false);
            });
        }
        _ = self.events.send(Event::Exit);
        info!("{}: Run loop finished.", function_name!());
    }
}

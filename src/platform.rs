use accessibility_sys::{AXError, AXObserverRef, AXUIElementRef};
use log::{error, info, warn};
use notify::event::{DataChange, ModifyKind};
use notify::{EventKind, FsEventWatcher, RecommendedWatcher, RecursiveMode, Watcher};
use objc2::rc::{Retained, autoreleasepool};
use objc2_app_kit::NSApplication;
use objc2_core_foundation::{CFRunLoop, CFString, kCFRunLoopDefaultMode};
use std::env;
use std::ffi::c_void;
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, LazyLock};
use std::time::Duration;
use stdext::function_name;

use crate::config::Config;
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

/// `ConfigHandler` is an implementation of `notify::EventHandler` that reloads the application configuration
/// when the configuration file changes. It also dispatches a `ConfigRefresh` event.
struct ConfigHandler {
    /// The `EventSender` for dispatching `ConfigRefresh` events.
    events: EventSender,
    /// The `Config` resource that is being watched and reloaded.
    config: Config,
}

impl ConfigHandler {
    /// Sends a `ConfigRefresh` event with the current configuration to the event handler.
    /// This signals the main application loop to update its configuration based on the latest file content.
    ///
    /// # Returns
    ///
    /// `Ok(())` if the event is sent successfully, otherwise `Err(Error)`.
    fn announce_fresh_config(&self) -> Result<()> {
        self.events.send(Event::ConfigRefresh {
            config: self.config.clone(),
        })?;
        Ok(())
    }
}

impl notify::EventHandler for ConfigHandler {
    /// Handles file system events for the configuration file. When the content changes, it reloads the configuration.
    /// Specifically, it responds to `ModifyKind::Data(DataChange::Content)` events.
    ///
    /// # Arguments
    ///
    /// * `event` - The result of a file system event.
    fn handle_event(&mut self, event: notify::Result<notify::Event>) {
        if let Ok(notify::Event {
            kind: EventKind::Modify(ModifyKind::Data(DataChange::Content)),
            paths: _,
            attrs: _,
        }) = event
        {
            info!("Reloading configuration file...");
            _ = self
                .config
                .reload_config(CONFIGURATION_FILE.as_path())
                .inspect_err(|err| {
                    error!("{}: error reloading config: {err}", function_name!());
                });
            _ = self.announce_fresh_config().inspect_err(|err| {
                error!("{}: error announcing fresh config: {err}", function_name!());
            });
        }
    }
}

/// Sets up a file system watcher for the configuration file.
/// It uses `notify` crate's `RecommendedWatcher` to monitor changes to `CONFIGURATION_FILE`.
///
/// # Arguments
///
/// * `events` - An `EventSender` to send `ConfigRefresh` events when the file changes.
/// * `config` - The initial `Config` object to be loaded and refreshed.
///
/// # Returns
///
/// `Ok(FsEventWatcher)` if the watcher is set up successfully, otherwise `Err(Error)`.
fn setup_config_watcher(events: EventSender, config: Config) -> Result<FsEventWatcher> {
    let setup = notify::Config::default().with_poll_interval(Duration::from_secs(3));
    let config_handler = ConfigHandler { events, config };
    config_handler.announce_fresh_config()?;
    let watcher = RecommendedWatcher::new(config_handler, setup).and_then(|mut watcher| {
        watcher.watch(CONFIGURATION_FILE.as_path(), RecursiveMode::NonRecursive)?;
        Ok(watcher)
    })?;
    Ok(watcher)
}

/// A `LazyLock` that determines the path to the application's configuration file.
/// It checks the `PANERU_CONFIG` environment variable first, then standard XDG locations and user home directory.
/// If no configuration file is found, the application will panic.
pub static CONFIGURATION_FILE: LazyLock<PathBuf> = LazyLock::new(|| {
    if let Ok(path_str) = env::var("PANERU_CONFIG") {
        let path = PathBuf::from(path_str);
        if path.exists() {
            return path;
        }
        warn!(
            "{}: $PANERU_CONFIG is set to {}, but the file does not exist. Falling back to default locations.",
            function_name!(),
            path.display()
        );
    }

    let standard_paths = [
        env::var("HOME")
            .ok()
            .map(|h| PathBuf::from(h).join(".paneru")),
        env::var("HOME")
            .ok()
            .map(|h| PathBuf::from(h).join(".paneru.toml")),
        env::var("XDG_CONFIG_HOME")
            .ok()
            .map(|x| PathBuf::from(x).join("paneru/paneru.toml")),
    ];

    standard_paths
        .into_iter()
        .flatten()
        .find(|path| path.exists())
        .unwrap_or_else(|| {
            panic!(
                "{}: Configuration file not found. Tried: $PANERU_CONFIG, $HOME/.paneru, $HOME/.paneru.toml, $XDG_CONFIG_HOME/paneru/paneru.toml",
                function_name!()
            )
        })
});

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
    /// The file system watcher for the configuration file.
    _config_watcher: FsEventWatcher,
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
        let workspace_observer = WorkspaceObserver::new(events.clone());
        Ok(Box::pin(PlatformCallbacks {
            process_handler: ProcessHandler::new(events.clone(), workspace_observer.clone()),
            event_handler: InputHandler::new(events.clone(), config.clone()),
            workspace_observer,
            mission_control_observer: MissionControlHandler::new(events.clone()),
            display_handler: DisplayHandler::new(events.clone()),
            _config_watcher: setup_config_watcher(events.clone(), config)?,
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

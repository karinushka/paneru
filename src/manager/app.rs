use accessibility_sys::{
    AXError, AXObserverRef, AXUIElementCreateApplication, AXUIElementRef,
    kAXErrorNotificationNotRegistered, kAXErrorSuccess,
};
use bevy::ecs::component::Component;
use core::ptr::NonNull;
use derive_more::{DerefMut, with_trait::Deref};
use mockall::automock;
use objc2_core_foundation::{CFRetained, CFString, kCFRunLoopCommonModes};
use std::ffi::c_void;
use std::pin::Pin;
use std::ptr::null_mut;
use std::sync::{Arc, LazyLock, RwLock};
use stdext::sync::rw_lock::RwLockExt;

use stdext::function_name;
use tracing::{debug, error};

use super::skylight::_SLPSGetFrontProcess;
use super::{ProcessApi, Window, WindowOS, ax_window_id};
use crate::config::Config;
use crate::errors::{Error, Result};
use crate::events::{Event, EventSender};
use crate::platform::{
    AXObserverAddNotification, AXObserverCreate, AXObserverRemoveNotification, CFStringRef, ConnID,
    Pid, ProcessSerialNumber, WinID,
};
use crate::util::{AXUIAttributes, AXUIWrapper, MacResult, add_run_loop, remove_run_loop};

/// A static `LazyLock` that holds a list of `AXNotification` strings to be observed for application-level events.
/// These notifications are general events related to an application's lifecycle and state changes,
/// such as a new window being created, the focused window changing, or a menu being opened/closed.
pub static AX_NOTIFICATIONS: LazyLock<Vec<&str>> = LazyLock::new(|| {
    vec![
        accessibility_sys::kAXCreatedNotification,
        accessibility_sys::kAXFocusedWindowChangedNotification,
        accessibility_sys::kAXFocusedUIElementChangedNotification,
        accessibility_sys::kAXWindowMovedNotification,
        accessibility_sys::kAXWindowResizedNotification,
        accessibility_sys::kAXTitleChangedNotification,
        accessibility_sys::kAXMenuOpenedNotification,
        accessibility_sys::kAXMenuClosedNotification,
    ]
});

/// A static `LazyLock` that holds a list of `AXNotification` strings to be observed for window-specific events.
/// These notifications are related to individual window lifecycle events,
/// such as a window being destroyed, miniaturized (minimized), or deminiaturized (restored).
pub static AX_WINDOW_NOTIFICATIONS: LazyLock<Vec<&str>> = LazyLock::new(|| {
    vec![
        accessibility_sys::kAXUIElementDestroyedNotification,
        accessibility_sys::kAXWindowMiniaturizedNotification,
        accessibility_sys::kAXWindowDeminiaturizedNotification,
    ]
});

#[automock]
pub trait ApplicationApi: Send + Sync {
    /// Returns the process ID of the application.
    fn pid(&self) -> Pid;
    /// Returns the process serial number of the application.
    fn psn(&self) -> ProcessSerialNumber;
    /// Returns the connection ID of the application.
    fn connection(&self) -> Option<ConnID>;
    /// Returns the ID of the currently focused window for this application.
    ///
    /// # Errors
    ///
    /// Returns an `Error` if the focused window cannot be determined.
    fn focused_window_id(&self) -> Result<WinID>;
    /// Returns a list of all windows belonging to this application.
    ///
    /// # Arguments
    ///
    /// * `config` - The current Paneru configuration, used to evaluate window rules.
    ///
    /// # Errors
    ///
    /// Returns an `Error` if the window list cannot be retrieved.
    fn window_list(&self, config: &Config) -> Vec<Window>;
    /// Starts observing application-level accessibility notifications.
    ///
    /// # Errors
    ///
    /// Returns an `Error` if observers cannot be registered.
    fn observe(&mut self) -> Result<bool>;
    /// Stops application-level accessibility notifications while preserving
    /// any explicit managed-window observers.
    fn unobserve(&mut self) -> bool;
    /// Starts observing window-specific accessibility notifications for a given window.
    ///
    /// # Arguments
    ///
    /// * `window` - The `Window` to observe.
    ///
    /// # Errors
    ///
    /// Returns an `Error` if observers cannot be registered.
    fn observe_window(&mut self, window: &Window) -> Result<bool>;
    /// Stops observing window-specific accessibility notifications for a given window.
    ///
    /// # Arguments
    ///
    /// * `window` - The `Window` to unobserve.
    fn unobserve_window(&mut self, window: &Window) -> bool;
    /// Retries AX removals that previously returned an ambiguous error.
    /// Returns `true` once no retained registrations remain pending.
    fn retry_observer_removals(&mut self) -> bool;
    /// Checks if the application is currently the frontmost application.
    fn is_frontmost(&self) -> bool;
    /// Returns the bundle identifier of the application.
    fn bundle_id(&self) -> Option<String>;
    /// Returns the display name of the application.
    fn name(&self) -> &str;
}

/// A wrapper struct for `ApplicationApi` trait objects, allowing for dynamic dispatch.
/// It implements `Deref` and `DerefMut` to easily access the underlying `ApplicationApi` methods.
#[derive(Component, Deref, DerefMut)]
pub struct Application(Box<dyn ApplicationApi>);

impl std::fmt::Debug for Application {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "app (pid {})", self.pid())
    }
}

impl std::fmt::Display for Application {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "app (pid {})", self.pid())
    }
}

impl Application {
    /// Creates a new `Application` instance from a boxed `ApplicationApi` trait object.
    ///
    /// # Arguments
    ///
    /// * `app` - A `Box<dyn ApplicationApi>` representing the application implementation.
    pub fn new(app: Box<dyn ApplicationApi>) -> Self {
        Application(app)
    }
}

/// `ApplicationOS` is a concrete implementation of the `ApplicationApi` trait for macOS.
/// It manages an application's accessibility UI element, process information, and event observation.
pub struct ApplicationOS {
    element: CFRetained<AXUIWrapper>,
    psn: ProcessSerialNumber,
    pid: Pid,
    connection: Option<ConnID>,
    handler: AxObserverHandler,
    bundle_id: Option<String>,
    name: String,
}

impl Drop for ApplicationOS {
    /// Cleans up the `AXObserver` by removing all registered notifications when the `Application` is dropped.
    fn drop(&mut self) {
        self.handler
            .remove_observer(&ObserverType::Application, &self.element, &AX_NOTIFICATIONS);
    }
}

impl std::fmt::Display for ApplicationOS {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "app '{}' (pid {})", self.name, self.pid)
    }
}

impl ApplicationOS {
    /// Creates a new `Application` instance for a given process.
    /// It obtains the Accessibility UI element for the application and its connection ID.
    ///
    /// # Arguments
    ///
    /// * `connection` - The main connection ID for the `SkyLight` API.
    /// * `process` - A reference to the `Process` associated with this application.
    /// * `events` - An `EventSender` to send events from the `AXObserver`.
    ///
    /// # Returns
    ///
    /// `Ok(Self)` if the `Application` is created successfully, otherwise `Err(Error)`.
    pub fn new(
        connection: Option<ConnID>,
        process: &dyn ProcessApi,
        events: &EventSender,
    ) -> Result<Self> {
        let refer = unsafe {
            let ptr = AXUIElementCreateApplication(process.pid());
            AXUIWrapper::retain(ptr)?
        };
        let bundle_id = process
            .application()
            .as_ref()
            .and_then(|app| app.bundleIdentifier())
            .map(|id| id.to_string());
        Ok(Self {
            element: refer,
            psn: process.psn(),
            pid: process.pid(),
            connection,
            handler: AxObserverHandler::new(process.pid(), events.clone())?,
            bundle_id,
            name: process.name().to_string(),
        })
    }
}

impl ApplicationApi for ApplicationOS {
    /// Retrieves the process ID (Pid) of the application.
    ///
    /// # Returns
    ///
    /// The process ID.
    fn pid(&self) -> Pid {
        self.pid
    }

    /// Retrieves the `ProcessSerialNumber` of the application.
    ///
    /// # Returns
    ///
    /// The process serial number.
    fn psn(&self) -> ProcessSerialNumber {
        self.psn
    }

    /// Retrieves the connection ID (`ConnID`) of the application.
    ///
    /// # Returns
    ///
    /// The connection ID.
    fn connection(&self) -> Option<ConnID> {
        self.connection
    }

    /// Retrieves the focused window ID of the application.
    ///
    /// # Returns
    ///
    /// `Ok(WinID)` with the focused window ID if successful, otherwise `Err(Error)`.
    fn focused_window_id(&self) -> Result<WinID> {
        self.element.focused_window_id()
    }

    /// Retrieves a list of all windows associated with the application.
    ///
    /// # Returns
    ///
    /// `Ok(Vec<Result<Window>>)` containing the list of window objects if successful, otherwise `Err(Error)`.
    fn window_list(&self, config: &Config) -> Vec<Window> {
        let bundle_id = self.bundle_id.as_deref();
        self.element
            .windows()
            .map(|windows| {
                windows
                    .into_iter()
                    .flat_map(|element| {
                        WindowOS::new_with_config(&element, config, bundle_id)
                            .map(|window| Window::new(Box::new(window)))
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Registers observers for general application-level accessibility notifications (e.g., `kAXCreatedNotification`).
    ///
    /// # Returns
    ///
    /// `Ok(bool)` where `true` means all observers were successfully registered and `retry` list is empty, otherwise `Err(Error)`.
    fn observe(&mut self) -> Result<bool> {
        self.handler
            .add_observer(&self.element, &AX_NOTIFICATIONS, ObserverType::Application)
            .map(|retry| retry.is_empty())
    }

    fn unobserve(&mut self) -> bool {
        self.handler
            .remove_observer(&ObserverType::Application, &self.element, &AX_NOTIFICATIONS)
    }

    /// Registers observers for specific window-level accessibility notifications (e.g., `kAXUIElementDestroyedNotification`).
    ///
    /// # Arguments
    ///
    /// * `window` - A reference to the `Window` object to observe.
    ///
    /// # Returns
    ///
    /// `Ok(bool)` where `true` means all observers were successfully registered and `retry` list is empty, otherwise `Err(Error)`.
    fn observe_window(&mut self, window: &Window) -> Result<bool> {
        if let Some(element) = window.element() {
            self.handler
                .add_observer(
                    &element,
                    &AX_WINDOW_NOTIFICATIONS,
                    ObserverType::Window(window.id()),
                )
                .map(|retry| retry.is_empty())
        } else {
            Err(Error::InvalidWindow)
        }
    }

    /// Unregisters observers for a specific window's accessibility notifications.
    ///
    /// # Arguments
    ///
    /// * `window` - A reference to the `Window` object to unobserve.
    fn unobserve_window(&mut self, window: &Window) -> bool {
        if let Some(element) = window.element() {
            self.handler.remove_observer(
                &ObserverType::Window(window.id()),
                &element,
                &AX_WINDOW_NOTIFICATIONS,
            )
        } else {
            true
        }
    }

    fn retry_observer_removals(&mut self) -> bool {
        self.handler.retry_pending_removals()
    }

    /// Checks if the application is currently the frontmost application.
    ///
    /// # Returns
    ///
    /// `true` if the application is frontmost, `false` otherwise.
    fn is_frontmost(&self) -> bool {
        let mut psn = ProcessSerialNumber::default();
        unsafe { _SLPSGetFrontProcess(&mut psn) }
            .to_result(function_name!())
            .is_ok()
            && self.psn == psn
    }

    /// Returns the bundle identifier of the application.
    ///
    /// # Returns
    ///
    /// An `Option<&str>` containing the bundle ID if available, otherwise `None`.
    fn bundle_id(&self) -> Option<String> {
        self.bundle_id.clone()
    }

    fn name(&self) -> &str {
        &self.name
    }
}

/// An enum representing the type of observer being used.
/// `Application` refers to an observer for application-level events.
/// `Window(WinID)` refers to an observer for a specific window, identified by its `WinID`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ObserverType {
    Application,
    Window(WinID),
}

fn should_store_new_context(has_existing: bool, newly_registered: bool) -> bool {
    !has_existing && newly_registered
}

fn removal_allows_context_release(results: impl IntoIterator<Item = AXError>) -> bool {
    results
        .into_iter()
        .all(|result| result == kAXErrorSuccess || result == kAXErrorNotificationNotRegistered)
}

/// `ObserverContext` holds the `EventSender` and the `ObserverType`,
/// which are used within the `AXObserver` callback to dispatch accessibility events.
struct ObserverContext {
    events: EventSender,
    which: ObserverType,
}

impl ObserverContext {
    /// Notifies the event sender about an accessibility event.
    /// It dispatches the event to either `notify_app` or `notify_window` based on the `ObserverType`.
    ///
    /// # Arguments
    ///
    /// * `notification` - The name of the accessibility notification as a `&str`.
    /// * `element` - The `AXUIElementRef` associated with the notification.
    fn notify(&self, notification: &str, element: AXUIElementRef) {
        match self.which {
            ObserverType::Application => self.notify_app(notification, element),
            ObserverType::Window(id) => self.notify_window(notification, id),
        }
    }

    /// Notifies the event sender about an application-level accessibility event.
    /// It translates the notification string and element into a corresponding `Event`.
    ///
    /// # Arguments
    ///
    /// * `notification` - The name of the accessibility notification as a `&str`.
    /// * `element` - The `AXUIElementRef` associated with the notification.
    fn notify_app(&self, notification: &str, element: AXUIElementRef) {
        if notification == accessibility_sys::kAXCreatedNotification {
            let Ok(element) = AXUIWrapper::retain(element).inspect_err(|err| {
                error!("invalid element {element:?}: {err}");
            }) else {
                return;
            };
            _ = self.events.send(Event::WindowCreated { element });
            return;
        }

        let Ok(window_id) =
            ax_window_id(element).inspect_err(|err| debug!("notification {notification}: {err}"))
        else {
            return;
        };
        let event = match notification {
            accessibility_sys::kAXFocusedWindowChangedNotification
            | accessibility_sys::kAXFocusedUIElementChangedNotification => {
                Event::WindowFocused { window_id }
            }
            accessibility_sys::kAXWindowMovedNotification => Event::WindowMoved { window_id },
            accessibility_sys::kAXWindowResizedNotification => Event::WindowResized { window_id },
            accessibility_sys::kAXTitleChangedNotification => {
                Event::WindowTitleChanged { window_id }
            }
            accessibility_sys::kAXMenuOpenedNotification => Event::MenuOpened { window_id },
            accessibility_sys::kAXMenuClosedNotification => Event::MenuClosed { window_id },
            _ => {
                error!("unhandled application notification: {notification:?}");
                return;
            }
        };
        _ = self.events.send(event);
    }

    /// Notifies the event sender about a window-level accessibility event.
    /// It translates the notification string and window ID into a corresponding `Event`.
    ///
    /// # Arguments
    ///
    /// * `notification` - The name of the accessibility notification as a `&str`.
    /// * `window_id` - The ID of the window associated with the notification.
    fn notify_window(&self, notification: &str, window_id: WinID) {
        let event = match notification {
            accessibility_sys::kAXWindowMiniaturizedNotification => {
                Event::WindowMinimized { window_id }
            }
            accessibility_sys::kAXWindowDeminiaturizedNotification => {
                Event::WindowDeminimized { window_id }
            }
            accessibility_sys::kAXUIElementDestroyedNotification => {
                Event::WindowDestroyed { window_id }
            }

            _ => {
                error!("unhandled window notification: {notification:?}");
                return;
            }
        };
        _ = self.events.send(event);
    }
}

/// `AxObserverHandler` manages the lifecycle of an `AXObserver`,
/// including its creation, registration of notifications, and removal from the run loop.
struct AxObserverHandler {
    observer: CFRetained<AXUIWrapper>,
    events: EventSender,
    contexts: Arc<RwLock<Vec<Pin<Box<ObserverContext>>>>>,
    pending_removals: Vec<PendingObserverRemoval>,
}

struct PendingObserverRemoval {
    which: ObserverType,
    element: CFRetained<AXUIWrapper>,
    notifications: Vec<&'static str>,
}

impl Drop for AxObserverHandler {
    /// Invalidates the run loop source associated with the `AXObserver` when the `AxObserverHandler` is dropped.
    fn drop(&mut self) {
        remove_run_loop(&self.observer);
    }
}

impl AxObserverHandler {
    /// Creates a new `AxObserverHandler` instance for a given process ID.
    /// It creates an `AXObserver` and adds its run loop source to the main run loop.
    ///
    /// # Arguments
    ///
    /// * `pid` - The process ID to create the observer for.
    /// * `events` - An `EventSender` to send events generated by the observer.
    ///
    /// # Returns
    ///
    /// `Ok(Self)` if the handler is created successfully, otherwise `Err(Error)`.
    fn new(pid: Pid, events: EventSender) -> Result<Self> {
        let observer = unsafe {
            let mut observer_ref: AXObserverRef = null_mut();
            if kAXErrorSuccess == AXObserverCreate(pid, Self::callback, &mut observer_ref) {
                AXUIWrapper::from_retained(observer_ref)?
            } else {
                return Err(Error::PermissionDenied(format!(
                    "{}: error creating observer.",
                    function_name!()
                )));
            }
        };

        unsafe { add_run_loop(&observer, kCFRunLoopCommonModes)? };
        Ok(Self {
            observer,
            events,
            contexts: Arc::new(RwLock::new(Vec::new())),
            pending_removals: Vec::new(),
        })
    }

    /// Adds accessibility notifications to be observed for a given UI element.
    ///
    /// # Arguments
    ///
    /// * `element` - The `&AXUIWrapper` to observe.
    /// * `notifications` - A slice of static strings representing the notification names to add.
    /// * `which` - The type of observer context (application-specific or window-specific).
    ///
    /// # Returns
    ///
    /// `Ok(Vec<&str>)` containing a list of notifications that could not be registered (retries), otherwise `Err(Error)`.
    pub fn add_observer(
        &mut self,
        element: &AXUIWrapper,
        notifications: &[&'static str],
        which: ObserverType,
    ) -> Result<Vec<&str>> {
        let observer: AXObserverRef = self.observer.as_ptr();
        let mut contexts = self.contexts.force_write();
        let existing_context = contexts.iter().find(|context| context.which == which);
        let new_context = existing_context.is_none().then(|| {
            Box::pin(ObserverContext {
                events: self.events.clone(),
                which,
            })
        });
        let context_ptr = existing_context.map_or_else(
            || NonNull::from_ref(&**new_context.as_ref().expect("new context exists")).as_ptr(),
            |context| NonNull::from_ref(context.as_ref().get_ref()).as_ptr(),
        );

        // TODO: retry re-registering these.
        let mut retry = vec![];
        let mut registered = 0;
        let mut newly_registered = false;
        for name in notifications {
            debug!("adding {name} {element:x?} {observer:?}");
            let notification = CFString::from_static_str(name);
            match unsafe {
                AXObserverAddNotification(
                    observer,
                    element.as_ptr(),
                    &notification,
                    context_ptr.cast(),
                )
            } {
                accessibility_sys::kAXErrorSuccess => {
                    registered += 1;
                    newly_registered = true;
                }
                accessibility_sys::kAXErrorNotificationAlreadyRegistered => {
                    registered += 1;
                }
                accessibility_sys::kAXErrorCannotComplete => {
                    retry.push(*name);
                }
                result => {
                    error!("error adding {name} {element:x?} {observer:?}: {result}");
                }
            }
        }
        if registered == 0 {
            Err(Error::PermissionDenied(format!(
                "{}: unable to register any observers!",
                function_name!()
            )))
        } else {
            // `AlreadyRegistered` retains the refcon supplied by the original
            // registration. Never add another context in that case.
            if should_store_new_context(existing_context.is_some(), newly_registered)
                && let Some(context) = new_context
            {
                contexts.push(context);
            }
            Ok(retry)
        }
    }

    /// Removes accessibility notifications from being observed for a given UI element.
    ///
    /// # Arguments
    ///
    /// * `which` - The type of observer context (application-specific or window-specific) for which to remove notifications.
    /// * `element` - The `&AXUIWrapper` from which to remove notifications.
    /// * `notifications` - A slice of static strings representing the notification names to remove.
    pub fn remove_observer(
        &mut self,
        which: &ObserverType,
        element: &AXUIWrapper,
        notifications: &[&'static str],
    ) -> bool {
        let mut existing = self.contexts.force_write();
        if !existing.iter().any(|context| context.which == *which) {
            debug!("{which:?} ({element}) already un-observed, skipping!");
            return true;
        }

        let mut results = Vec::with_capacity(notifications.len());
        for name in notifications {
            let observer: AXObserverRef = self.observer.deref().as_ptr();
            let notification = CFString::from_static_str(name);
            debug!("removing {name} {element:x?} {observer:?}");
            let result =
                unsafe { AXObserverRemoveNotification(observer, element.as_ptr(), &notification) };
            results.push(result);
            if result != kAXErrorSuccess && result != kAXErrorNotificationNotRegistered {
                debug!("error removing {name} {element:x?} {observer:?}: {result}");
            }
        }
        if removal_allows_context_release(results) {
            existing.retain(|context| context.which != *which);
            self.pending_removals
                .retain(|pending| pending.which != *which);
            true
        } else {
            if !self
                .pending_removals
                .iter()
                .any(|pending| pending.which == *which)
            {
                match AXUIWrapper::retain(element.as_ptr::<c_void>()) {
                    Ok(element) => self.pending_removals.push(PendingObserverRemoval {
                        which: *which,
                        element,
                        notifications: notifications.to_vec(),
                    }),
                    Err(err) => {
                        error!("unable to retain {which:?} for observer removal retry: {err}");
                    }
                }
            }
            debug!("retaining {which:?} observer context after ambiguous removal failure");
            false
        }
    }

    fn retry_pending_removals(&mut self) -> bool {
        let observer: AXObserverRef = self.observer.deref().as_ptr();
        let mut still_pending = Vec::new();
        let mut contexts = self.contexts.force_write();
        for pending in self.pending_removals.drain(..) {
            let results = pending.notifications.iter().map(|name| {
                let notification = CFString::from_static_str(name);
                unsafe {
                    AXObserverRemoveNotification(observer, pending.element.as_ptr(), &notification)
                }
            });
            if removal_allows_context_release(results) {
                contexts.retain(|context| context.which != pending.which);
                debug!("observer removal retry completed for {:?}", pending.which);
            } else {
                still_pending.push(pending);
            }
        }
        self.pending_removals = still_pending;
        self.pending_removals.is_empty()
    }

    /// The static callback function for `AXObserver`. This function is called by the macOS Accessibility API
    /// when an observed accessibility event occurs. It dispatches the event to the appropriate `notify_app` or `notify_window` handler.
    ///
    /// # Arguments
    ///
    /// * `_observer` - The `AXObserverRef` (unused).
    /// * `element` - The `AXUIElementRef` associated with the notification.
    /// * `notification` - The raw `CFStringRef` representing the notification name.
    /// * `context` - A raw pointer to the user-defined context `ObserverContext`.
    extern "C" fn callback(
        _observer: AXObserverRef,
        element: AXUIElementRef,
        notification: CFStringRef,
        context: *mut c_void,
    ) {
        let notification = NonNull::new(notification.cast_mut())
            .map(|ptr| unsafe { ptr.as_ref() })
            .map(CFString::to_string);
        let context =
            NonNull::new(context.cast::<ObserverContext>()).map(|ptr| unsafe { ptr.as_ref() });
        let Some((notification, context)) = notification.zip(context) else {
            return;
        };

        context.notify(&notification, element);
    }
}

#[cfg(test)]
mod tests {
    use std::ptr::NonNull;

    use accessibility_sys::{kAXErrorCannotComplete, kAXErrorNotificationNotRegistered};

    use super::{
        ObserverContext, ObserverType, removal_allows_context_release, should_store_new_context,
    };
    use crate::events::EventSender;

    #[test]
    fn already_registered_notifications_never_allocate_an_extra_context() {
        assert!(!should_store_new_context(false, false));
        assert!(!should_store_new_context(true, false));
        assert!(!should_store_new_context(true, true));
        assert!(should_store_new_context(false, true));
    }

    #[test]
    fn ambiguous_removal_retains_callback_context_until_retry_is_safe() {
        let (events, _receiver) = EventSender::new();
        let mut contexts = vec![Box::pin(ObserverContext {
            events,
            which: ObserverType::Application,
        })];
        let pointer = NonNull::from_ref(contexts[0].as_ref().get_ref());

        let release = removal_allows_context_release([kAXErrorCannotComplete]);
        if release {
            contexts.clear();
        }
        assert!(!release);
        assert_eq!(unsafe { pointer.as_ref().which }, ObserverType::Application);

        assert!(removal_allows_context_release([
            kAXErrorNotificationNotRegistered
        ]));
    }
}

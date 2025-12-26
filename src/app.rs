use accessibility_sys::{
    AXObserverRef, AXUIElementCreateApplication, AXUIElementRef, kAXCreatedNotification,
    kAXErrorSuccess, kAXFocusedWindowAttribute, kAXFocusedWindowChangedNotification,
    kAXMenuClosedNotification, kAXMenuOpenedNotification, kAXTitleChangedNotification,
    kAXUIElementDestroyedNotification, kAXWindowDeminiaturizedNotification,
    kAXWindowMiniaturizedNotification, kAXWindowMovedNotification, kAXWindowResizedNotification,
    kAXWindowsAttribute,
};
use bevy::ecs::component::Component;
use core::ptr::NonNull;
use log::{debug, error};
use objc2_core_foundation::{CFArray, CFNumberType, CFRetained, CFString, kCFRunLoopCommonModes};
use objc2_core_graphics::CGDirectDisplayID;
use std::ffi::c_void;
use std::io::ErrorKind;
use std::ops::Deref;
use std::pin::Pin;
use std::ptr::null_mut;
use std::sync::LazyLock;
use stdext::function_name;

use crate::errors::{Error, Result};
use crate::events::{Event, EventSender, WindowManager};
use crate::platform::{
    AXObserverAddNotification, AXObserverCreate, AXObserverRemoveNotification, CFStringRef, Pid,
    ProcessSerialNumber,
};
use crate::process::Process;
use crate::skylight::{
    _SLPSGetFrontProcess, ConnID, SLSWindowIteratorAdvance, SLSWindowIteratorGetCount,
    SLSWindowIteratorGetParentID, SLSWindowQueryResultCopyWindows, SLSWindowQueryWindows, WinID,
};
use crate::util::{
    AXUIWrapper, add_run_loop, create_array, get_array_values, get_attribute, remove_run_loop,
};
use crate::windows::{Window, ax_window_id};

pub static AX_NOTIFICATIONS: LazyLock<Vec<&str>> = LazyLock::new(|| {
    vec![
        kAXCreatedNotification,
        kAXFocusedWindowChangedNotification,
        kAXWindowMovedNotification,
        kAXWindowResizedNotification,
        kAXTitleChangedNotification,
        kAXMenuOpenedNotification,
        kAXMenuClosedNotification,
    ]
});

pub static AX_WINDOW_NOTIFICATIONS: LazyLock<Vec<&str>> = LazyLock::new(|| {
    vec![
        kAXUIElementDestroyedNotification,
        kAXWindowMiniaturizedNotification,
        kAXWindowDeminiaturizedNotification,
    ]
});

#[derive(Component)]
pub struct Application {
    element: CFRetained<AXUIWrapper>,
    psn: ProcessSerialNumber,
    pid: Pid,
    connection: Option<ConnID>,
    handler: AxObserverHandler,
    bundle_id: Option<String>,
    name: String,
}

impl Drop for Application {
    /// Cleans up the `AXObserver` by removing all registered notifications when the `Application` is dropped.
    fn drop(&mut self) {
        self.handler
            .remove_observer(&ObserverType::Application, &self.element, &AX_NOTIFICATIONS);
    }
}

impl std::fmt::Display for Application {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "app '{}' (pid {})", self.name, self.pid)
    }
}

impl Application {
    /// Creates a new `Application` instance for a given process.
    /// It obtains the Accessibility UI element for the application and its connection ID.
    ///
    /// # Arguments
    ///
    /// * `main_cid` - The main connection ID for the `SkyLight` API.
    /// * `process` - A reference to the `Process` associated with this application.
    /// * `events` - An `EventSender` to send events from the `AXObserver`.
    ///
    /// # Returns
    ///
    /// `Ok(Self)` if the `Application` is created successfully, otherwise `Err(Error)`.
    pub fn new(wm: &WindowManager, process: &Process, events: &EventSender) -> Result<Self> {
        let refer = unsafe {
            let ptr = AXUIElementCreateApplication(process.pid);
            AXUIWrapper::retain(ptr)?
        };
        let bundle_id = process
            .application
            .as_ref()
            .and_then(|app| app.bundleIdentifier())
            .map(|id| id.to_string());
        Ok(Self {
            element: refer,
            psn: process.psn,
            pid: process.pid,
            connection: wm.0.connection_for_process(process.psn),
            handler: AxObserverHandler::new(process.pid, events.clone())?,
            bundle_id,
            name: process.name.clone(),
        })
    }

    /// Retrieves the process ID (Pid) of the application.
    ///
    /// # Returns
    ///
    /// The process ID.
    pub fn pid(&self) -> Pid {
        self.pid
    }

    /// Retrieves the `ProcessSerialNumber` of the application.
    ///
    /// # Returns
    ///
    /// The process serial number.
    pub fn psn(&self) -> ProcessSerialNumber {
        self.psn
    }

    /// Retrieves the connection ID (`ConnID`) of the application.
    ///
    /// # Returns
    ///
    /// The connection ID.
    pub fn connection(&self) -> Option<ConnID> {
        self.connection
    }

    /// Retrieves the focused window ID of the application.
    ///
    /// # Returns
    ///
    /// `Ok(WinID)` with the focused window ID if successful, otherwise `Err(Error)`.
    pub fn focused_window_id(&self) -> Result<WinID> {
        let axmain = CFString::from_static_str(kAXFocusedWindowAttribute);
        let focused = get_attribute::<AXUIWrapper>(&self.element, &axmain)?;
        ax_window_id(focused.as_ptr())
    }

    /// Retrieves a list of all windows associated with the application.
    ///
    /// # Returns
    ///
    /// `Ok(CFRetained<CFArray>)` containing the list of window elements if successful, otherwise `Err(Error)`.
    pub fn window_list(&self) -> Result<Vec<Result<Window>>> {
        let axwindows = CFString::from_static_str(kAXWindowsAttribute);
        let array = get_attribute::<CFArray>(&self.element, &axwindows)?;
        let out = get_array_values::<accessibility_sys::__AXUIElement>(&array)
            .map(|element| {
                AXUIWrapper::retain(element.as_ptr()).and_then(|element| Window::new(&element))
            })
            .collect();
        Ok(out)
    }

    /// Registers observers for general application-level accessibility notifications (e.g., `kAXCreatedNotification`).
    ///
    /// # Returns
    ///
    /// `Ok(bool)` where `true` means all observers were successfully registered and `retry` list is empty, otherwise `Err(Error)`.
    pub fn observe(&mut self) -> Result<bool> {
        self.handler
            .add_observer(&self.element, &AX_NOTIFICATIONS, ObserverType::Application)
            .map(|retry| retry.is_empty())
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
    pub fn observe_window(&mut self, window: &Window) -> Result<bool> {
        self.handler
            .add_observer(
                window.element().deref(),
                &AX_WINDOW_NOTIFICATIONS,
                ObserverType::Window(window.id()),
            )
            .map(|retry| retry.is_empty())
    }

    /// Unregisters observers for a specific window's accessibility notifications.
    ///
    /// # Arguments
    ///
    /// * `window` - A reference to the `Window` object to unobserve.
    pub fn unobserve_window(&mut self, window: &Window) {
        self.handler.remove_observer(
            &ObserverType::Window(window.id()),
            window.element().deref(),
            &AX_WINDOW_NOTIFICATIONS,
        );
    }

    /// Checks if the application is currently the frontmost application.
    ///
    /// # Returns
    ///
    /// `true` if the application is frontmost, `false` otherwise.
    pub fn is_frontmost(&self) -> bool {
        let mut psn = ProcessSerialNumber::default();
        unsafe { _SLPSGetFrontProcess(&mut psn) };
        self.psn == psn
    }

    /// Returns the bundle identifier of the application.
    pub fn bundle_id(&self) -> Option<&String> {
        self.bundle_id.as_ref()
    }

    /// Returns the parent window for a given display.
    pub fn parent_window(&self, display_id: CGDirectDisplayID) -> Result<WinID> {
        let windows = create_array(&[display_id], CFNumberType::SInt32Type)?;
        unsafe {
            let query = CFRetained::from_raw(SLSWindowQueryWindows(
                self.connection.unwrap_or_default(),
                &raw const *windows,
                1,
            ));
            let iterator = &raw const *CFRetained::from_raw(SLSWindowQueryResultCopyWindows(
                query.deref().into(),
            ));
            if 1 == SLSWindowIteratorGetCount(iterator) && SLSWindowIteratorAdvance(iterator) {
                return Ok(SLSWindowIteratorGetParentID(iterator));
            }
        }
        Err(Error::new(
            ErrorKind::InvalidInput,
            format!("{}: error creating an array.", function_name!()),
        ))
    }
}

enum ObserverType {
    Application,
    Window(WinID),
}

struct ObserverContext {
    events: EventSender,
    which: ObserverType,
}

impl ObserverContext {
    /// Notifies the event sender about an accessibility event.
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
        match notification {
            accessibility_sys::kAXTitleChangedNotification => {
                // TODO: WindowTitleChanged does not have a valid window as its element reference.
                return;
            }
            accessibility_sys::kAXCreatedNotification => {
                let Ok(element) = AXUIWrapper::retain(element).inspect_err(|err| {
                    error!("{}: invalid element {element:?}: {err}", function_name!());
                }) else {
                    return;
                };
                _ = self.events.send(Event::WindowCreated { element });
                return;
            }
            _ => (),
        }

        let Ok(window_id) = ax_window_id(element)
            .inspect_err(|err| error!("{}: notification {notification}: {err}", function_name!(),))
        else {
            return;
        };
        let event = match notification {
            accessibility_sys::kAXFocusedWindowChangedNotification => {
                Event::WindowFocused { window_id }
            }
            accessibility_sys::kAXWindowMovedNotification => Event::WindowMoved { window_id },
            accessibility_sys::kAXWindowResizedNotification => Event::WindowResized { window_id },
            accessibility_sys::kAXMenuOpenedNotification => Event::MenuOpened { window_id },
            accessibility_sys::kAXMenuClosedNotification => Event::MenuClosed { window_id },
            _ => {
                error!(
                    "{}: unhandled application notification: {notification:?}",
                    function_name!()
                );
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
                error!(
                    "{}: unhandled window notification: {notification:?}",
                    function_name!()
                );
                return;
            }
        };
        _ = self.events.send(event);
    }
}

struct AxObserverHandler {
    observer: CFRetained<AXUIWrapper>,
    events: EventSender,
    contexts: Vec<Pin<Box<ObserverContext>>>,
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
                return Err(Error::new(
                    ErrorKind::PermissionDenied,
                    format!("{}: error creating observer.", function_name!()),
                ));
            }
        };

        unsafe { add_run_loop(&observer, kCFRunLoopCommonModes)? };
        Ok(Self {
            observer,
            events,
            contexts: Vec::new(),
        })
    }

    /// Adds accessibility notifications to be observed for a given UI element.
    ///
    /// # Arguments
    ///
    /// * `element` - The `&AXUIWrapper` to observe.
    /// * `notifications` - A slice of static strings representing the notification names to add.
    /// * `which` - Adds event type to callback context. I.e. an application or window specific.
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
        let context = Box::pin(ObserverContext {
            events: self.events.clone(),
            which,
        });
        let context_ptr = NonNull::from_ref(&*context).as_ptr();
        self.contexts.push(context);

        // TODO: retry re-registering these.
        let mut retry = vec![];
        let added = notifications
            .iter()
            .filter_map(|name| {
                debug!(
                    "{}: adding {name} {element:x?} {observer:?}",
                    function_name!()
                );
                let notification = CFString::from_static_str(name);
                match unsafe {
                    AXObserverAddNotification(
                        observer,
                        element.as_ptr(),
                        &notification,
                        context_ptr.cast(),
                    )
                } {
                    accessibility_sys::kAXErrorSuccess
                    | accessibility_sys::kAXErrorNotificationAlreadyRegistered => Some(*name),
                    accessibility_sys::kAXErrorCannotComplete => {
                        retry.push(*name);
                        None
                    }
                    result => {
                        error!(
                            "{}: error adding {name} {element:x?} {observer:?}: {result}",
                            function_name!()
                        );
                        None
                    }
                }
            })
            .collect::<Vec<_>>();
        if added.is_empty() {
            Err(Error::new(
                ErrorKind::PermissionDenied,
                format!("{}: unable to register any observers!", function_name!()),
            ))
        } else {
            Ok(retry)
        }
    }

    /// Removes accessibility notifications from being observed for a given UI element.
    ///
    /// # Arguments
    ///
    /// * `which` - Adds event type to callback context. I.e. an application or window specific.
    /// * `element` - The `&AXUIWrapper` from which to remove notifications.
    /// * `notifications` - A slice of static strings representing the notification names to remove.
    pub fn remove_observer(
        &mut self,
        which: &ObserverType,
        element: &AXUIWrapper,
        notifications: &[&'static str],
    ) {
        for name in notifications {
            let observer: AXObserverRef = self.observer.deref().as_ptr();
            let notification = CFString::from_static_str(name);
            debug!(
                "{}: removing {name} {element:x?} {observer:?}",
                function_name!()
            );
            let result =
                unsafe { AXObserverRemoveNotification(observer, element.as_ptr(), &notification) };
            if result != kAXErrorSuccess {
                debug!(
                    "{}: error removing {name} {element:x?} {observer:?}: {result}",
                    function_name!(),
                );
            }
        }
        if let ObserverType::Window(removed) = which {
            self.contexts.retain(
                    |context| !matches!(context.which, ObserverType::Window(window_id) if window_id == *removed),
                );
        }
    }

    /// The static callback function for `AXObserver`. This function is called by the macOS Accessibility API
    /// when an observed accessibility event occurs. It dispatches the event to the appropriate `notify_app` or `notify_window` handler.
    ///
    /// # Arguments
    ///
    /// * `_` - The `AXObserverRef` (unused).
    /// * `element` - The `AXUIElementRef` associated with the notification.
    /// * `notification` - The raw `CFStringRef` representing the notification name.
    /// * `context` - A raw pointer to the user-defined context `ObserverContext`.
    extern "C" fn callback(
        _: AXObserverRef,
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

use accessibility_sys::{
    AXObserverRef, AXUIElementCreateApplication, AXUIElementRef, kAXCreatedNotification,
    kAXErrorSuccess, kAXFocusedWindowAttribute, kAXFocusedWindowChangedNotification,
    kAXMainWindowAttribute, kAXMenuClosedNotification, kAXMenuOpenedNotification,
    kAXTitleChangedNotification, kAXUIElementDestroyedNotification,
    kAXWindowDeminiaturizedNotification, kAXWindowMiniaturizedNotification,
    kAXWindowMovedNotification, kAXWindowResizedNotification, kAXWindowsAttribute,
};
use bevy::ecs::component::Component;
use bevy::ecs::entity::Entity;
use core::ptr::NonNull;
use log::{debug, error};
use objc2_core_foundation::{CFArray, CFRetained, CFString, kCFRunLoopCommonModes};
use std::collections::HashMap;
use std::ffi::c_void;
use std::io::{Error, ErrorKind, Result};
use std::ops::Deref;
use std::pin::Pin;
use std::ptr::null_mut;
use std::sync::{Arc, LazyLock, RwLock};
use stdext::function_name;
use stdext::prelude::RwLockExt;

use crate::events::{Event, EventSender};
use crate::platform::{
    AXObserverAddNotification, AXObserverCreate, AXObserverRemoveNotification, CFStringRef, Pid,
    ProcessSerialNumber,
};
use crate::process::Process;
use crate::skylight::{_SLPSGetFrontProcess, ConnID, SLSGetConnectionIDForPSN, WinID};
use crate::util::{AxuWrapperType, add_run_loop, get_attribute, remove_run_loop};
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

#[derive(Clone, Component)]
pub struct Application {
    pub inner: Arc<RwLock<InnerApplication>>,
}

pub struct InnerApplication {
    entity: Option<Entity>,
    element: CFRetained<AxuWrapperType>,
    psn: ProcessSerialNumber,
    pid: Pid,
    name: String,
    connection: Option<ConnID>,
    handler: AxObserverHandler,
    windows: HashMap<WinID, Window>,
}

impl Drop for InnerApplication {
    /// Cleans up the `AXObserver` by removing all registered notifications when the `InnerApplication` is dropped.
    fn drop(&mut self) {
        self.handler
            .remove_observer(&ObserverType::Application, &self.element, &AX_NOTIFICATIONS);
    }
}

impl InnerApplication {
    /// Creates a new `InnerApplication` instance for a given process.
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
    /// `Ok(Self)` if the `InnerApplication` is created successfully, otherwise `Err(Error)`.
    pub fn new(main_cid: ConnID, process: &Process, events: &EventSender) -> Result<Self> {
        let refer = unsafe {
            let ptr = AXUIElementCreateApplication(process.pid);
            AxuWrapperType::retain(ptr)?
        };
        Ok(InnerApplication {
            entity: None,
            element: refer,
            psn: process.psn.clone(),
            pid: process.pid,
            name: process.name.clone(),
            connection: {
                unsafe {
                    let mut connection: ConnID = 0;
                    SLSGetConnectionIDForPSN(main_cid, &process.psn, &mut connection);
                    Some(connection)
                }
            },
            handler: AxObserverHandler::new(process.pid, events.clone())?,
            windows: HashMap::new(),
        })
    }
}

impl Application {
    pub fn new(main_cid: ConnID, process: &Process, events: &EventSender) -> Result<Self> {
        Ok(Self {
            inner: Arc::new(RwLock::new(InnerApplication::new(
                main_cid, process, events,
            )?)),
        })
    }

    /// Creates an `Error` indicating that the application has shut down, used for weak reference failures.
    ///
    /// # Arguments
    ///
    /// * `place` - A string indicating where the error occurred (e.g., function name).
    ///
    /// # Returns
    ///
    /// An `Error` of `ErrorKind::NotFound`.
    fn weak_error(place: &str) -> Error {
        Error::new(
            ErrorKind::NotFound,
            format!("{place}: application shut down."),
        )
    }

    pub fn entity(&self) -> Option<Entity> {
        self.inner.force_read().entity
    }

    /// Retrieves the name of the application.
    ///
    /// # Returns
    ///
    /// `Ok(String)` with the application name if successful, otherwise `Err(Error)` if the application has shut down.
    pub fn name(&self) -> Result<String> {
        Ok(self.inner.force_read().name.clone())
    }

    /// Retrieves the process ID (Pid) of the application.
    ///
    /// # Returns
    ///
    /// `Ok(Pid)` with the process ID if successful, otherwise `Err(Error)` if the application has shut down.
    pub fn pid(&self) -> Result<Pid> {
        Ok(self.inner.force_read().pid)
    }

    /// Retrieves the `ProcessSerialNumber` of the application.
    ///
    /// # Returns
    ///
    /// `Ok(ProcessSerialNumber)` with the PSN if successful, otherwise `Err(Error)` if the application has shut down.
    pub fn psn(&self) -> Result<ProcessSerialNumber> {
        Ok(self.inner.force_read().psn.clone())
    }

    /// Retrieves the connection ID (`ConnID`) of the application.
    ///
    /// # Returns
    ///
    /// `Ok(ConnID)` with the connection ID if successful, otherwise `Err(Error)` if the application has shut down.
    pub fn connection(&self) -> Option<ConnID> {
        self.inner.force_read().connection
    }

    /// Retrieves the `CFRetained<AxuWrapperType>` representing the Accessibility UI element of the application.
    ///
    /// # Returns
    ///
    /// `Ok(CFRetained<AxuWrapperType>)` if successful, otherwise `Err(Error)` if the application has shut down.
    pub fn element(&self) -> Result<CFRetained<AxuWrapperType>> {
        Ok(self.inner.force_read().element.clone())
    }

    /// Finds a `Window` associated with this application by its window ID.
    ///
    /// # Arguments
    ///
    /// * `window_id` - The ID of the window to find.
    ///
    /// # Returns
    ///
    /// `Some(Window)` if the window is found, otherwise `None`.
    pub fn find_window(&self, window_id: WinID) -> Option<Window> {
        self.inner.force_read().windows.get(&window_id).cloned()
    }

    /// Removes a window from the application's internal map of windows.
    ///
    /// # Arguments
    ///
    /// * `window_id` - The ID of the window to remove.
    ///
    /// # Returns
    ///
    /// `Some(Window)` if the window was removed, otherwise `None`.
    pub fn remove_window(&self, window_id: WinID) -> Option<Window> {
        self.inner.force_write().windows.remove(&window_id)
    }

    /// Adds a window to the application's internal map of windows.
    ///
    /// # Arguments
    ///
    /// * `window` - A reference to the `Window` to add.
    pub fn add_window(&self, window: &Window) {
        self.inner
            .force_write()
            .windows
            .insert(window.id(), window.clone());
    }

    /// Iterates over each window managed by this application, applying an accessor function.
    ///
    /// # Arguments
    ///
    /// * `accessor` - A closure that takes a reference to a `Window`.
    pub fn foreach_window(&self, accessor: impl FnMut(&Window)) {
        self.inner.force_read().windows.values().for_each(accessor);
    }

    /// Retrieves the main window ID of the application.
    ///
    /// # Returns
    ///
    /// `Ok(WinID)` with the main window ID if successful, otherwise `Err(Error)`.
    fn _main_window(&self) -> Result<WinID> {
        let axmain = CFString::from_static_str(kAXMainWindowAttribute);
        let focused = get_attribute::<AxuWrapperType>(&self.element()?, &axmain)?;
        ax_window_id(focused.as_ptr()).map_err(|err| {
            Error::new(
                ErrorKind::NotFound,
                format!(
                    "{}: can not find main window for application {}: {err}.",
                    function_name!(),
                    self.name().unwrap_or_default()
                ),
            )
        })
    }

    /// Retrieves the focused window ID of the application.
    ///
    /// # Returns
    ///
    /// `Ok(WinID)` with the focused window ID if successful, otherwise `Err(Error)`.
    pub fn focused_window_id(&self) -> Result<WinID> {
        let axmain = CFString::from_static_str(kAXFocusedWindowAttribute);
        let element = self.element()?;
        let focused = get_attribute::<AxuWrapperType>(&element, &axmain)?;
        ax_window_id(focused.as_ptr())
    }

    /// Retrieves a list of all windows associated with the application.
    ///
    /// # Returns
    ///
    /// `Ok(CFRetained<CFArray>)` containing the list of window elements if successful, otherwise `Err(Error)`.
    pub fn window_list(&self) -> Result<CFRetained<CFArray>> {
        let axwindows = CFString::from_static_str(kAXWindowsAttribute);
        let element = self.element()?;
        get_attribute::<CFArray>(&element, &axwindows)
    }

    /// Registers observers for general application-level accessibility notifications (e.g., `kAXCreatedNotification`).
    ///
    /// # Returns
    ///
    /// `Ok(bool)` where `true` means all observers were successfully registered and `retry` list is empty, otherwise `Err(Error)`.
    pub fn observe(&self) -> Result<bool> {
        let element = self.element()?;
        self.inner
            .force_write()
            .handler
            .add_observer(&element, &AX_NOTIFICATIONS, ObserverType::Application)
            .map(|retry| retry.is_empty())
    }

    /// Registers observers for specific window-level accessibility notifications (e.g., `kAXUIElementDestroyedNotification`).
    ///
    /// # Arguments
    ///
    /// * `element` - The `AXUIElementRef` of the window to observe.
    /// * `window` - A reference to the `Window` object.
    ///
    /// # Returns
    ///
    /// `Ok(bool)` where `true` means all observers were successfully registered and `retry` list is empty, otherwise `Err(Error)`.
    pub fn observe_window(&self, window: &Window) -> Result<bool> {
        self.inner
            .force_write()
            .handler
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
    /// * `element` - The `AXUIElementRef` of the window to unobserve.
    pub fn unobserve_window(&self, window: &Window) {
        self.inner.force_write().handler.remove_observer(
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
        self.psn().is_ok_and(|serial| serial == psn)
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
    /// * `notification` - The name of the accessibility notification as a `String`.
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
    /// * `notification` - The name of the accessibility notification as a `String`.
    /// * `element` - The `AXUIElementRef` associated with the notification.
    fn notify_app(&self, notification: &str, element: AXUIElementRef) {
        if notification == accessibility_sys::kAXCreatedNotification {
            let Ok(element) = AxuWrapperType::retain(element) else {
                error!("{}: invalid element {element:?}", function_name!());
                return;
            };
            _ = self.events.send(Event::WindowCreated { element });
            return;
        }

        let Ok(window_id) = ax_window_id(element) else {
            error!("{}: invalid window_id {element:?}", function_name!());
            return;
        };
        let event = match notification {
            accessibility_sys::kAXFocusedWindowChangedNotification => {
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
    /// * `notification` - The name of the accessibility notification as a `String`.
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
    observer: CFRetained<AxuWrapperType>,
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
    /// `Ok(Pin<Box<Self>>)` if the handler is created successfully, otherwise `Err(Error)`.
    fn new(pid: Pid, events: EventSender) -> Result<Self> {
        let observer = unsafe {
            let mut observer_ref: AXObserverRef = null_mut();
            if kAXErrorSuccess == AXObserverCreate(pid, Self::callback, &mut observer_ref) {
                AxuWrapperType::from_retained(observer_ref)?
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
    /// * `element` - The `AxuWrapperType` to observe.
    /// * `notifications` - A slice of static strings representing the notification names to add.
    /// * `which` - Adds event type to callback context. I.e. an application or window specific.
    ///
    /// # Returns
    ///
    /// `Ok(Vec<&str>)` containing a list of notifications that could not be registered (retries), otherwise `Err(Error)`.
    pub fn add_observer(
        &mut self,
        element: &AxuWrapperType,
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
    /// * `element` - The `AXUIElementRef` from which to remove notifications.
    /// * `notifications` - A slice of static strings representing the notification names to remove.
    pub fn remove_observer(
        &mut self,
        which: &ObserverType,
        element: &AxuWrapperType,
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

use accessibility_sys::{
    AXObserverRef, AXUIElementCreateApplication, AXUIElementRef, kAXCreatedNotification,
    kAXErrorSuccess, kAXFocusedWindowAttribute, kAXFocusedWindowChangedNotification,
    kAXMainWindowAttribute, kAXMenuClosedNotification, kAXMenuOpenedNotification,
    kAXTitleChangedNotification, kAXUIElementDestroyedNotification,
    kAXWindowDeminiaturizedNotification, kAXWindowMiniaturizedNotification,
    kAXWindowMovedNotification, kAXWindowResizedNotification, kAXWindowsAttribute,
};
use core::ptr::NonNull;
use log::{debug, error, warn};
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
use crate::windows::{InnerWindow, Window, ax_window_id};

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

#[derive(Clone)]
pub struct Application {
    inner: Arc<RwLock<InnerApplication>>,
}

struct InnerApplication {
    element_ref: CFRetained<AxuWrapperType>,
    psn: ProcessSerialNumber,
    pid: Pid,
    name: String,
    connection: Option<ConnID>,
    handler: Pin<Box<AxObserverHandler>>,
    windows: HashMap<WinID, Window>,
}

impl Drop for InnerApplication {
    fn drop(&mut self) {
        let element = self.element_ref.as_ptr::<c_void>();
        self.handler
            .remove_observer(element.cast(), &AX_NOTIFICATIONS);
    }
}

impl Application {
    pub fn new(main_cid: ConnID, process: &Process, events: EventSender) -> Result<Self> {
        let refer = unsafe {
            let ptr = AXUIElementCreateApplication(process.pid);
            AxuWrapperType::retain(ptr)?
        };
        Ok(Application {
            inner: Arc::new(RwLock::new(InnerApplication {
                element_ref: refer,
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
                handler: AxObserverHandler::new(process.pid, events)?,
                windows: HashMap::new(),
            })),
        })
        // app.inner.write().unwrap().handler = Some(ApplicationHandler::new(app.clone(), tx));
    }

    fn inner(&self) -> std::sync::RwLockReadGuard<'_, InnerApplication> {
        self.inner.force_read()
    }

    pub fn name(&self) -> String {
        self.inner().name.clone()
    }

    pub fn pid(&self) -> Pid {
        self.inner().pid
    }

    pub fn psn(&self) -> ProcessSerialNumber {
        self.inner().psn.clone()
    }

    pub fn connection(&self) -> Option<ConnID> {
        self.inner().connection
    }

    pub fn find_window(&self, window_id: WinID) -> Option<Window> {
        self.inner().windows.get(&window_id).cloned()
    }

    pub fn remove_window(&self, window_id: WinID) -> Option<Window> {
        self.inner.force_write().windows.remove(&window_id)
    }

    pub fn add_window(&self, window: &Window) {
        self.inner
            .force_write()
            .windows
            .insert(window.id(), window.clone());
    }

    pub fn foreach_window(&self, accessor: impl FnMut(&Window)) {
        self.inner().windows.values().for_each(accessor);
    }

    fn _main_window(&self) -> Result<WinID> {
        let axmain = CFString::from_static_str(kAXMainWindowAttribute);
        let focused = get_attribute::<AxuWrapperType>(&self.inner().element_ref, axmain)?;
        ax_window_id(focused.as_ptr()).map_err(|err| {
            Error::new(
                ErrorKind::NotFound,
                format!(
                    "{}: can not find main window for application {}: {err}.",
                    function_name!(),
                    self.inner().name
                ),
            )
        })
    }

    pub fn focused_window_id(&self) -> Result<WinID> {
        let axmain = CFString::from_static_str(kAXFocusedWindowAttribute);
        let focused = get_attribute::<AxuWrapperType>(&self.inner().element_ref, axmain)?;
        ax_window_id(focused.as_ptr()).map_err(|err| {
            Error::new(
                ErrorKind::NotFound,
                format!(
                    "{}: can not find focused window for application {}: {err}.",
                    function_name!(),
                    self.inner().name
                ),
            )
        })
    }

    pub fn window_list(&self) -> Result<CFRetained<CFArray>> {
        let axwindows = CFString::from_static_str(kAXWindowsAttribute);
        get_attribute::<CFArray>(&self.inner().element_ref, axwindows)
    }

    pub fn observe(&self) -> Result<bool> {
        let element = self.inner().element_ref.as_ptr::<c_void>();
        let context = NonNull::from(self.inner().handler.deref());
        self.inner
            .force_write()
            .handler
            .add_observer(element.cast(), &AX_NOTIFICATIONS, context.cast())
            .map(|retry| retry.is_empty())
    }

    pub fn observe_window(&self, element: AXUIElementRef, window: &Window) -> Result<bool> {
        let context = NonNull::from(window.inner().deref());
        self.inner
            .force_write()
            .handler
            .add_observer(element, &AX_WINDOW_NOTIFICATIONS, context.cast())
            .map(|retry| retry.is_empty())
    }

    pub fn unobserve_window(&self, element: AXUIElementRef) {
        self.inner
            .force_write()
            .handler
            .remove_observer(element, &AX_WINDOW_NOTIFICATIONS);
    }

    pub fn is_frontmost(&self) -> bool {
        let mut psn = ProcessSerialNumber::default();
        unsafe { _SLPSGetFrontProcess(&mut psn) };
        self.inner().psn == psn
    }
}

struct AxObserverHandler {
    observer: CFRetained<AxuWrapperType>,
    events: EventSender,
}

impl Drop for AxObserverHandler {
    fn drop(&mut self) {
        remove_run_loop(self.observer.deref());
    }
}

impl AxObserverHandler {
    fn new(pid: Pid, events: EventSender) -> Result<Pin<Box<Self>>> {
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

        unsafe { add_run_loop(observer.deref(), kCFRunLoopCommonModes)? };
        Ok(Box::pin(Self {
            observer,
            events,
            // handlers: Vec::new(),
        }))
    }

    pub fn add_observer(
        &mut self,
        element: AXUIElementRef,
        notifications: &[&'static str],
        context: NonNull<c_void>,
    ) -> Result<Vec<&str>> {
        let observer: AXObserverRef = self.observer.as_ptr();

        // TODO: retry re-registering these.
        let mut retry = vec![];
        let added = notifications
            .iter()
            .flat_map(|name| {
                debug!(
                    "{}: adding {name} {element:x?} {observer:?}",
                    function_name!()
                );
                let notification = CFString::from_static_str(name);
                match unsafe {
                    AXObserverAddNotification(
                        observer,
                        element,
                        notification.deref(),
                        context.as_ptr(),
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

    pub fn remove_observer(&mut self, element: AXUIElementRef, notifications: &[&'static str]) {
        for name in notifications {
            let observer: AXObserverRef = self.observer.deref().as_ptr();
            let notification = CFString::from_static_str(name);
            let ptr = NonNull::new(element);
            if let Some(element) = ptr {
                debug!(
                    "{}: removing {name} {element:x?} {observer:?}",
                    function_name!()
                );
                let result = unsafe {
                    AXObserverRemoveNotification(
                        observer,
                        element.as_ptr().cast(),
                        notification.deref(),
                    )
                };
                if result != kAXErrorSuccess {
                    warn!(
                        "{}: error removing {name} {element:x?} {observer:?}: {result}",
                        function_name!(),
                    );
                }
            }
        }
    }

    fn notify_app(&self, notification: String, element: AXUIElementRef) {
        let event = if accessibility_sys::kAXCreatedNotification == notification {
            match AxuWrapperType::retain(element) {
                Ok(element) => Event::WindowCreated { element },
                Err(err) => {
                    error!("{}: invalid element {element:?}: {err}", function_name!());
                    return;
                }
            }
        } else {
            let window_id = match ax_window_id(element) {
                Ok(window_id) => window_id,
                Err(err) => {
                    warn!("{}: invalid element: {err}.", function_name!());
                    return;
                }
            };
            match notification.as_str() {
                accessibility_sys::kAXFocusedWindowChangedNotification => {
                    Event::WindowFocused { window_id }
                }
                accessibility_sys::kAXWindowMovedNotification => Event::WindowMoved { window_id },
                accessibility_sys::kAXWindowResizedNotification => {
                    Event::WindowResized { window_id }
                }
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
            }
        };
        _ = self.events.send(event);
    }

    fn notify_window(&self, notification: String, window_id: WinID) {
        let event = match notification.as_str() {
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

    extern "C" fn callback(
        _: AXObserverRef,
        element: AXUIElementRef,
        notification: CFStringRef,
        context: *mut c_void,
    ) {
        let notification = match NonNull::new(notification as *mut CFString)
            .map(|ptr| unsafe { ptr.as_ref() })
            .map(CFString::to_string)
        {
            Some(n) => n,
            None => {
                //
                return;
            }
        };

        if AX_NOTIFICATIONS.iter().any(|n| *n == notification) {
            let handler = NonNull::new(context)
                .map(|handler| unsafe { handler.cast::<AxObserverHandler>().as_ref() });
            if let Some(handler) = handler {
                handler.notify_app(notification, element);
            }
        } else if AX_WINDOW_NOTIFICATIONS.iter().any(|n| *n == notification) {
            let inner = NonNull::new(context)
                .map(|handler| unsafe { handler.cast::<InnerWindow>().as_ref() })
                .map(|inner| (inner.app.inner(), inner.id));
            if let Some((handler, window_id)) = inner {
                handler
                    .deref()
                    .handler
                    .notify_window(notification, window_id);
            }
        } else {
            warn!(
                "{}: received unknown notification '{notification}'",
                function_name!()
            );
        };
    }
}

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
use std::sync::mpsc::Sender;
use std::sync::{Arc, LazyLock, RwLock};
use stdext::function_name;
use stdext::prelude::RwLockExt;

use crate::events::Event;
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

#[derive(Clone, Debug)]
pub struct Application {
    inner: Arc<RwLock<InnerApplication>>,
}

#[derive(Debug)]
struct InnerApplication {
    element_ref: CFRetained<AxuWrapperType>,
    psn: ProcessSerialNumber,
    pid: Pid,
    name: String,
    connection: Option<ConnID>,
    handler: Pin<Box<ApplicationHandler>>,
    windows: HashMap<WinID, Window>,
}

impl Application {
    pub fn from_process(main_cid: ConnID, process: &Process, tx: Sender<Event>) -> Result<Self> {
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
                handler: Box::pin(ApplicationHandler::new(tx)),
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

    pub fn observer_ref(&self) -> Option<CFRetained<AxuWrapperType>> {
        self.inner().handler.observer_ref.clone()
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
        let pid = self.inner().pid;
        let element = self.inner().element_ref.clone();
        self.inner.force_write().handler.observe(pid, element)
    }

    pub fn is_frontmost(&self) -> bool {
        let mut psn = ProcessSerialNumber::default();
        unsafe { _SLPSGetFrontProcess(&mut psn) };
        self.inner().psn == psn
    }
}

impl Drop for InnerApplication {
    fn drop(&mut self) {
        self.handler.unobserve()
    }
}

#[derive(Debug)]
pub struct ApplicationHandler {
    tx: Sender<Event>,
    ax_retry: bool,
    observing: Vec<bool>,
    element_ref: Option<CFRetained<AxuWrapperType>>,
    observer_ref: Option<CFRetained<AxuWrapperType>>,
    observing_windows: Vec<(WinID, CFRetained<AxuWrapperType>)>,
}

impl ApplicationHandler {
    fn new(tx: Sender<Event>) -> Self {
        Self {
            tx,
            ax_retry: false,
            observing: vec![],
            element_ref: None,
            observer_ref: None,
            observing_windows: vec![],
        }
    }

    fn observe(&mut self, pid: Pid, element: CFRetained<AxuWrapperType>) -> Result<bool> {
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

        let mut ax_retry = false;
        let observing = AX_NOTIFICATIONS
            .iter()
            .map(|name| unsafe {
                debug!(
                    "{}: {name:?} {:?}",
                    function_name!(),
                    observer.as_ptr::<AXObserverRef>()
                );
                let notification = CFString::from_static_str(name);
                match AXObserverAddNotification(
                    observer.deref().as_ptr(),
                    element.as_ptr(),
                    notification.deref(),
                    NonNull::new_unchecked(self).as_ptr().cast(),
                ) {
                    accessibility_sys::kAXErrorSuccess
                    | accessibility_sys::kAXErrorNotificationAlreadyRegistered => true,
                    accessibility_sys::kAXErrorCannotComplete => {
                        ax_retry = true;
                        false
                    }
                    result => {
                        error!(
                            "{}: error registering {name} for application {pid}: {result}",
                            function_name!()
                        );
                        false
                    }
                }
            })
            .collect();
        unsafe { add_run_loop(observer.deref(), kCFRunLoopCommonModes)? };

        self.ax_retry = ax_retry;
        self.observing = observing;
        self.element_ref = Some(element);
        self.observer_ref = Some(observer);
        Ok(self.observing.iter().all(|ok| *ok))
    }

    fn unobserve(&mut self) {
        if self.observer_ref.is_none() || self.observing.iter().all(|registered| !registered) {
            return;
        }
        if let Some((observer, element)) = self.observer_ref.take().zip(self.element_ref.as_ref()) {
            debug!(
                "{}: {:?}",
                function_name!(),
                observer.as_ptr::<AXObserverRef>()
            );
            AX_NOTIFICATIONS
                .iter()
                .zip(&self.observing)
                .filter(|(_, remove)| **remove)
                .for_each(|(name, _)| {
                    debug!("{}: name {name:?}", function_name!());
                    let notification = CFString::from_static_str(name);
                    let result = unsafe {
                        AXObserverRemoveNotification(
                            observer.as_ptr(),
                            element.as_ptr(),
                            notification.deref(),
                        )
                    };
                    if result != kAXErrorSuccess {
                        warn!(
                            "{}: error unregistering {:?}",
                            function_name!(),
                            observer.as_ptr::<AXObserverRef>()
                        );
                    }
                });

            remove_run_loop(observer.deref());
            drop(observer)
        }
    }

    fn application_handler(
        &self,
        _observer: AXObserverRef,
        element: AXUIElementRef,
        notification: &str,
        window_id: Option<WinID>,
    ) {
        let get_window_id = |element| {
            ax_window_id(element)
                .inspect_err(|err| warn!("{}: invalid element: {err}.", function_name!()))
                .ok()
        };
        let event = match notification {
            accessibility_sys::kAXCreatedNotification => match AxuWrapperType::retain(element) {
                Ok(element) => Event::WindowCreated { element },
                Err(err) => {
                    error!("{}: invalid element {element:?}: {err}", function_name!());
                    return;
                }
            },
            accessibility_sys::kAXFocusedWindowChangedNotification => Event::WindowFocused {
                window_id: get_window_id(element),
            },
            accessibility_sys::kAXWindowMovedNotification => Event::WindowMoved {
                window_id: get_window_id(element),
            },
            accessibility_sys::kAXWindowResizedNotification => Event::WindowResized {
                window_id: get_window_id(element),
            },
            accessibility_sys::kAXTitleChangedNotification => Event::WindowTitleChanged {
                window_id: get_window_id(element),
            },
            accessibility_sys::kAXMenuOpenedNotification => Event::MenuOpened {
                window_id: get_window_id(element),
            },
            accessibility_sys::kAXMenuClosedNotification => Event::MenuClosed {
                window_id: get_window_id(element),
            },
            accessibility_sys::kAXWindowMiniaturizedNotification => {
                Event::WindowMinimized { window_id }
            }
            accessibility_sys::kAXWindowDeminiaturizedNotification => {
                Event::WindowDeminimized { window_id }
            }
            accessibility_sys::kAXUIElementDestroyedNotification => {
                let window = window_id.and_then(|window_id| {
                    self.observing_windows
                        .iter()
                        .find(|(id, _)| window_id == *id)
                });
                if let Some(((window_id, element), observer)) =
                    window.zip(self.observer_ref.as_ref())
                {
                    AX_WINDOW_NOTIFICATIONS.iter().for_each(|name| {
                        let notification = CFString::from_static_str(name);
                        debug!(
                            "{}: unobserve {window_id:?}:  {name} {:?} {:?}",
                            function_name!(),
                            observer.deref().as_ptr::<AXObserverRef>(),
                            element.deref().as_ptr::<AXUIElementRef>()
                        );
                        let result = unsafe {
                            AXObserverRemoveNotification(
                                observer.deref().as_ptr(),
                                element.deref().as_ptr(),
                                notification.deref(),
                            )
                        };
                        if result != kAXErrorSuccess {
                            error!(
                                "{}: error unregistering {name} for {window_id:?}: {result}",
                                function_name!()
                            );
                        }
                    });
                }

                Event::WindowDestroyed { window_id }
            }
            _ => {
                error!(
                    "{}: unhandled application notification: {notification:?}",
                    function_name!()
                );
                return;
            }
        };
        _ = self.tx.send(event);
    }

    extern "C" fn callback(
        observer: AXObserverRef,
        element: AXUIElementRef,
        notification: CFStringRef,
        context: *mut c_void,
    ) {
        let (notification, context) =
            match NonNull::new(notification.cast_mut()).zip(NonNull::new(context)) {
                Some((notification, context)) => {
                    (unsafe { notification.as_ref() }.to_string(), context)
                }
                None => {
                    error!("{}: nullptr passed!", function_name!());
                    return;
                }
            };

        let (handler, window) = match notification.as_ref() {
            accessibility_sys::kAXWindowMiniaturizedNotification
            | accessibility_sys::kAXWindowDeminiaturizedNotification
            | accessibility_sys::kAXUIElementDestroyedNotification => {
                let inner_window =
                    unsafe { context.cast::<RwLock<InnerWindow>>().as_ref() }.force_read();
                let app = inner_window.app.clone();
                let this = unsafe { NonNull::from(app.inner().handler.deref()).as_ref() };
                (this, Some(inner_window.id))
            }
            _ => (
                unsafe { context.cast::<ApplicationHandler>().as_ref() },
                None,
            ),
        };
        handler.application_handler(observer, element, notification.as_ref(), window);
    }
}

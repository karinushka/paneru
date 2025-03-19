use accessibility_sys::{
    AXObserverGetRunLoopSource, AXObserverRef, AXUIElementCreateApplication, AXUIElementRef,
    kAXCreatedNotification, kAXErrorSuccess, kAXFocusedWindowAttribute,
    kAXFocusedWindowChangedNotification, kAXMainWindowAttribute, kAXMenuClosedNotification,
    kAXMenuOpenedNotification, kAXTitleChangedNotification, kAXUIElementDestroyedNotification,
    kAXWindowDeminiaturizedNotification, kAXWindowMiniaturizedNotification,
    kAXWindowMovedNotification, kAXWindowResizedNotification, kAXWindowsAttribute,
};
use core::ptr::NonNull;
use log::{debug, error, warn};
use objc2_core_foundation::{
    CFArray, CFRetained, CFRunLoopAddSource, CFRunLoopGetMain, CFRunLoopSource,
    CFRunLoopSourceInvalidate, CFString, kCFRunLoopCommonModes,
};
use std::ffi::c_void;
use std::ops::Deref;
use std::ptr::null_mut;
use std::sync::mpsc::Sender;
use std::sync::{Arc, LazyLock, RwLock};
use stdext::function_name;

use crate::events::Event;
use crate::platform::{
    AXObserverAddNotification, AXObserverCreate, AXObserverRemoveNotification, CFStringRef, Pid,
    ProcessSerialNumber,
};
use crate::process::Process;
use crate::skylight::{_SLPSGetFrontProcess, ConnID, SLSGetConnectionIDForPSN, WinID};
use crate::util::{AxuWrapperType, get_attribute};
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
    pub inner: Arc<RwLock<InnerApplication>>,
}

#[derive(Debug)]
pub struct InnerApplication {
    pub element_ref: CFRetained<AxuWrapperType>,
    pub psn: ProcessSerialNumber,
    pub pid: Pid,
    pub name: String,
    pub connection: Option<ConnID>,
    pub handler: ApplicationHandler,
}

impl Application {
    pub fn from_process(main_cid: ConnID, process: &Process, tx: Sender<Event>) -> Self {
        let refer = unsafe {
            let ptr = AXUIElementCreateApplication(process.pid);
            AxuWrapperType::retain(ptr).expect("Error fetching element from application!")
        };
        Application {
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
                handler: ApplicationHandler::new(tx),
            })),
        }
        // app.inner.write().unwrap().handler = Some(ApplicationHandler::new(app.clone(), tx));
    }

    pub fn inner(&self) -> std::sync::RwLockReadGuard<'_, InnerApplication> {
        self.inner.read().unwrap()
    }

    fn _main_window(&self) -> Option<WinID> {
        let axmain = CFString::from_static_str(kAXMainWindowAttribute);
        let focused = get_attribute::<AxuWrapperType>(&self.inner().element_ref, axmain)?;
        ax_window_id(focused.as_ptr())
    }

    pub fn focused_window_id(&self) -> Option<WinID> {
        let axmain = CFString::from_static_str(kAXFocusedWindowAttribute);
        let focused = get_attribute::<AxuWrapperType>(&self.inner().element_ref, axmain)?;
        ax_window_id(focused.as_ptr())
    }

    pub fn window_list(&self) -> Option<CFRetained<CFArray>> {
        let axwindows = CFString::from_static_str(kAXWindowsAttribute);
        get_attribute::<CFArray>(&self.inner().element_ref, axwindows)
    }

    pub fn observe(&self) -> bool {
        let pid = self.inner().pid;
        let element = self.inner().element_ref.clone();
        self.inner.write().unwrap().handler.observe(pid, element)
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

    pub fn window_observe(&self, window: &Window) -> bool {
        if self.observer_ref.is_none() {
            warn!(
                "{}: Can not observe window {} without application observer.",
                function_name!(),
                window.inner().id
            );
            return false;
        }
        let observer_ref = self.observer_ref.as_ref().unwrap();
        let observing = AX_WINDOW_NOTIFICATIONS
            .iter()
            .map(|name| unsafe {
                let notification = CFString::from_static_str(name);
                debug!(
                    "{}: {name} {:?} {:?}",
                    function_name!(),
                    observer_ref.as_ptr::<AXObserverRef>(),
                    window.inner().element_ref.as_ptr::<AXUIElementRef>(),
                );
                match AXObserverAddNotification(
                    observer_ref.as_ptr(),
                    window.inner().element_ref.as_ptr(),
                    notification.deref(),
                    window.inner.deref() as *const RwLock<InnerWindow> as *mut c_void,
                ) {
                    accessibility_sys::kAXErrorSuccess
                    | accessibility_sys::kAXErrorNotificationAlreadyRegistered => true,
                    result => {
                        error!(
                            "{}: error registering {name} for window {}: {result}",
                            function_name!(),
                            window.inner().id
                        );
                        false
                    }
                }
            })
            .collect::<Vec<_>>();
        let gotall = observing.iter().all(|status| *status);

        window.inner.write().unwrap().observing = observing;
        gotall
    }

    pub fn window_unobserve(window: &mut InnerWindow) {
        let observer_ref = window.app.inner().handler.observer_ref.clone();
        if observer_ref.is_none() {
            error!(
                "{}: No application reference to unregister a window {}",
                function_name!(),
                window.id
            );
            return;
        }
        AX_WINDOW_NOTIFICATIONS
            .iter()
            .zip(&window.observing)
            .filter(|(_, remove)| **remove)
            .for_each(|(name, _)| {
                let notification = CFString::from_static_str(name);
                debug!(
                    "{}: {name} {:?} {:?}",
                    function_name!(),
                    observer_ref.as_ref().unwrap().as_ptr::<AXObserverRef>(),
                    window.element_ref.as_ptr::<AXUIElementRef>(),
                );
                let result = unsafe {
                    AXObserverRemoveNotification(
                        observer_ref.as_ref().unwrap().as_ptr(),
                        window.element_ref.as_ptr(),
                        notification.deref(),
                    )
                };
                if result != kAXErrorSuccess {
                    warn!(
                        "{}: error unregistering {name} for window {}: {result}",
                        function_name!(),
                        window.id
                    );
                }
            });
    }

    fn observe(&mut self, pid: Pid, element: CFRetained<AxuWrapperType>) -> bool {
        let observer_ref = unsafe {
            let mut observer_ref: AXObserverRef = null_mut();
            if kAXErrorSuccess == AXObserverCreate(pid, Self::callback, &mut observer_ref) {
                AxuWrapperType::from_retained(observer_ref as AXUIElementRef)
            } else {
                None
            }
        };
        if let Some(observer_ref) = observer_ref {
            let mut ax_retry = false;
            let observing = AX_NOTIFICATIONS
                .iter()
                .map(|name| unsafe {
                    debug!(
                        "{}: {name:?} {:?}",
                        function_name!(),
                        observer_ref.as_ptr::<AXObserverRef>()
                    );
                    let notification = CFString::from_static_str(name);
                    match AXObserverAddNotification(
                        observer_ref.deref().as_ptr(),
                        element.as_ptr(),
                        notification.deref(),
                        self as *const Self as *mut c_void,
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
            unsafe {
                let main_loop = CFRunLoopGetMain().expect("Unable to get the main run loop.");
                let run_loop_source = CFRetained::from_raw(
                    NonNull::new(AXObserverGetRunLoopSource(observer_ref.deref().as_ptr()))
                        .expect("Can not get AXObserver run loop source.")
                        .cast(),
                );
                debug!(
                    "{}: adding runloop source: {run_loop_source:?} {observer_ref:?}",
                    function_name!()
                );
                CFRunLoopAddSource(&main_loop, Some(&run_loop_source), kCFRunLoopCommonModes);

                self.ax_retry = ax_retry;
                self.observing = observing;
                self.element_ref = Some(element);
                self.observer_ref = Some(observer_ref);
            };
        }
        self.observing.iter().all(|ok| *ok)
    }

    fn unobserve(&mut self) {
        if self.observer_ref.is_none() || self.observing.iter().all(|registered| !registered) {
            return;
        }
        debug!(
            "{}: {:?}",
            function_name!(),
            self.observer_ref
                .as_ref()
                .unwrap()
                .as_ptr::<AXObserverRef>()
        );
        if let Some(observer) = self.observer_ref.take() {
            AX_NOTIFICATIONS
                .iter()
                .zip(&self.observing)
                .filter(|(_, remove)| **remove)
                .for_each(|(name, _)| {
                    debug!("{}: name {name:?}", function_name!());
                    let element = self.element_ref.as_ref().unwrap();
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

            unsafe {
                let run_loop_source =
                    AXObserverGetRunLoopSource(observer.as_ptr()) as *mut CFRunLoopSource;
                debug!(
                    "{}: removing runloop source: {run_loop_source:?} ref {:?}",
                    function_name!(),
                    observer.as_ptr::<AXObserverRef>()
                );
                CFRunLoopSourceInvalidate(&*run_loop_source);
            }
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
        let event = match notification {
            accessibility_sys::kAXCreatedNotification => Event::WindowCreated {
                element: AxuWrapperType::retain(element).unwrap(),
            },
            accessibility_sys::kAXFocusedWindowChangedNotification => Event::WindowFocused {
                window_id: ax_window_id(element),
            },
            accessibility_sys::kAXWindowMovedNotification => Event::WindowMoved {
                window_id: ax_window_id(element),
            },
            accessibility_sys::kAXWindowResizedNotification => Event::WindowResized {
                window_id: ax_window_id(element),
            },
            accessibility_sys::kAXTitleChangedNotification => Event::WindowTitleChanged {
                window_id: ax_window_id(element),
            },
            accessibility_sys::kAXMenuOpenedNotification => Event::MenuOpened {
                window_id: ax_window_id(element),
            },
            accessibility_sys::kAXMenuClosedNotification => Event::MenuClosed {
                window_id: ax_window_id(element),
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
                if let Some((window_id, element)) = window {
                    AX_WINDOW_NOTIFICATIONS.iter().for_each(|name| {
                        let notification = CFString::from_static_str(name);
                        let observer = self.observer_ref.as_ref().unwrap();
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
        let notification = if let Some(notification) = NonNull::new(notification as *mut CFString) {
            unsafe { notification.as_ref() }.to_string()
        } else {
            error!("{}: nullptr 'notification' passed.", function_name!());
            return;
        };
        let (this, window) = match notification.as_ref() {
            accessibility_sys::kAXWindowMiniaturizedNotification
            | accessibility_sys::kAXWindowDeminiaturizedNotification
            | accessibility_sys::kAXUIElementDestroyedNotification => {
                let lock = unsafe { &*(context as *const RwLock<InnerWindow>) };
                let inner_window = lock.read().unwrap();
                let app = inner_window.app.clone();
                let this = &app.inner().handler as *const Self;
                (this, Some(inner_window.id))
            }
            _ => ((context as *const Self), None),
        };

        let result = unsafe {
            (this as *mut Self).as_ref().map(|this| {
                this.application_handler(observer, element, notification.as_ref(), window)
            })
        };
        if result.is_none() {
            error!("{}: nullptr passed as a self reference.", function_name!());
        }
    }
}

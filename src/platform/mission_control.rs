use accessibility_sys::{
    AXObserverRef, AXUIElementCreateApplication, AXUIElementRef,
    kAXErrorNotificationAlreadyRegistered, kAXErrorSuccess,
};
use core::ptr::NonNull;
use objc2_app_kit::NSRunningApplication;
use objc2_core_foundation::{CFRetained, CFString, kCFRunLoopDefaultMode};
use objc2_foundation::ns_string;
use std::ffi::c_void;
use std::ptr::null_mut;
use stdext::function_name;
use tracing::{debug, error, warn};

use super::{
    AXObserverAddNotification, AXObserverCreate, AXObserverRemoveNotification, CFStringRef, Pid,
};
use crate::errors::{Error, Result};
use crate::events::{Event, EventSender};
use crate::util::{AXUIWrapper, add_run_loop, remove_run_loop};

/// `MissionControlHandler` manages observation of Mission Control related accessibility events from the Dock process.
/// It dispatches specific `Event` types when Mission Control actions (e.g., showing all windows, showing desktop) occur.
#[derive(Debug)]
pub(super) struct MissionControlHandler {
    /// The `EventSender` to dispatch Mission Control events.
    events: EventSender,
    /// An optional `AXUIWrapper` for the Dock application's UI element.
    element: Option<CFRetained<AXUIWrapper>>,
    /// An optional `AXUIWrapper` for the `AXObserver` instance.
    observer: Option<CFRetained<AXUIWrapper>>,
}

impl MissionControlHandler {
    /// Creates a new `MissionControlHandler` instance.
    ///
    /// # Arguments
    ///
    /// * `events` - An `EventSender` to send Mission Control related events.
    ///
    /// # Returns
    ///
    /// A new `MissionControlHandler`.
    pub(super) fn new(events: EventSender) -> Self {
        Self {
            events,
            element: None,
            observer: None,
        }
    }

    /// A constant array of `&str` representing the Mission Control accessibility event names that are observed.
    const EVENTS: [&str; 4] = [
        "AXExposeShowAllWindows",
        "AXExposeShowFrontWindows",
        "AXExposeShowDesktop",
        "AXExposeExit",
    ];

    /// Handles Mission Control accessibility notifications. It translates the notification string into a corresponding `Event`.
    ///
    /// # Arguments
    ///
    /// * `_observer` - The `AXObserverRef` (unused).
    /// * `_element` - The `AXUIElementRef` (unused).
    /// * `notification` - The name of the Mission Control notification as a string.
    fn mission_control_handler(
        &self,
        _observer: AXObserverRef,
        _element: AXUIElementRef,
        notification: &str,
    ) {
        let event = match notification {
            "AXExposeShowAllWindows" => Event::MissionControlShowAllWindows,
            "AXExposeShowFrontWindows" => Event::MissionControlShowFrontWindows,
            "AXExposeShowDesktop" => Event::MissionControlShowDesktop,
            "AXExposeExit" => Event::MissionControlExit,
            _ => {
                warn!("Unknown mission control event: {notification}");
                return;
            }
        };
        _ = self
            .events
            .send(event)
            .inspect_err(|err| error!("error sending event: {err}"));
    }

    /// Retrieves the process ID (`Pid`) of the Dock application.
    /// This function uses `NSRunningApplication` to find the Dock by its bundle identifier.
    ///
    /// # Returns
    ///
    /// `Ok(Pid)` with the Dock's process ID if found, otherwise `Err(Error)`.
    fn dock_pid() -> Result<Pid> {
        let dock = ns_string!("com.apple.dock");
        let array = NSRunningApplication::runningApplicationsWithBundleIdentifier(dock);
        array
            .iter()
            .next()
            .map(|running| running.processIdentifier())
            .ok_or(Error::NotFound(format!(
                "{}: can not find dock.",
                function_name!()
            )))
    }

    /// Starts observing Mission Control accessibility notifications from the Dock process.
    /// It creates an `AXObserver` for the Dock application and registers for specific Mission Control events.
    /// The observer is then added to the run loop.
    ///
    /// # Returns
    ///
    /// `Ok(())` if observation is started successfully, otherwise `Err(Error)` if permissions are denied or setup fails.
    pub(super) fn observe(&mut self) -> Result<()> {
        let pid = MissionControlHandler::dock_pid()?;
        let element = AXUIWrapper::from_retained(unsafe { AXUIElementCreateApplication(pid) })?;
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

        for name in &Self::EVENTS {
            debug!("{name:?} {:?}", observer.as_ptr::<AXObserverRef>());
            let notification = CFString::from_static_str(name);
            match unsafe {
                AXObserverAddNotification(
                    observer.as_ptr(),
                    element.as_ptr(),
                    &notification,
                    NonNull::new_unchecked(self).as_ptr().cast(),
                )
            } {
                accessibility_sys::kAXErrorSuccess
                | accessibility_sys::kAXErrorNotificationAlreadyRegistered => (),
                result => error!("error registering {name} for application {pid}: {result}"),
            }
        }
        unsafe { add_run_loop(&observer, kCFRunLoopDefaultMode)? };
        self.observer = observer.into();
        self.element = element.into();
        Ok(())
    }

    /// Stops observing Mission Control accessibility notifications and cleans up resources.
    /// It removes all registered notifications from the `AXObserver` and invalidates the run loop source.
    ///
    /// # Side Effects
    ///
    /// - Deregisters `AXObserver` notifications.
    /// - Removes the `AXObserver` from the run loop.
    fn unobserve(&mut self) {
        if let Some((observer, element)) = self.observer.take().zip(self.element.as_ref()) {
            for name in &Self::EVENTS {
                debug!("{name:?} {:?}", observer.as_ptr::<AXObserverRef>());
                let notification = CFString::from_static_str(name);
                let result = unsafe {
                    AXObserverRemoveNotification(observer.as_ptr(), element.as_ptr(), &notification)
                };
                if result != kAXErrorSuccess && result != kAXErrorNotificationAlreadyRegistered {
                    error!("error unregistering {name}: {result}");
                }
            }
            remove_run_loop(&observer);
            drop(observer);
        } else {
            warn!("unobserving without observe or element");
        }
    }

    /// The static callback function for the Mission Control `AXObserver`. It dispatches to the `mission_control_handler` method.
    /// This function is declared as `extern "C"`.
    ///
    /// # Arguments
    ///
    /// * `observer` - The `AXObserverRef` that invoked the callback.
    /// * `element` - The `AXUIElementRef` associated with the notification.
    /// * `notification` - The raw `CFStringRef` representing the notification name.
    /// * `context` - A raw pointer to the `MissionControlHandler` instance.
    extern "C" fn callback(
        observer: AXObserverRef,
        element: AXUIElementRef,
        notification: CFStringRef,
        context: *mut c_void,
    ) {
        let Some(notification) = NonNull::new(notification.cast_mut()) else {
            error!("nullptr 'notification' passed.");
            return;
        };

        if let Some(this) = NonNull::new(context)
            .map(|this| unsafe { this.cast::<MissionControlHandler>().as_ref() })
        {
            let notification = unsafe { notification.as_ref() }.to_string();
            this.mission_control_handler(observer, element, &notification);
        } else {
            error!("Zero passed to MissionControlHandler.");
        }
    }
}

impl Drop for MissionControlHandler {
    /// Unobserves Mission Control notifications when the `MissionControlHandler` is dropped.
    /// This ensures that system resources are properly released when the handler is no longer needed.
    fn drop(&mut self) {
        self.unobserve();
    }
}

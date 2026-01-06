use core::ptr::NonNull;
use log::{debug, info, warn};
use objc2::rc::Retained;
use objc2::{AllocAnyThread, DefinedClass, define_class, msg_send, sel};
use objc2_app_kit::{
    NSApplicationActivationPolicy, NSRunningApplication, NSWorkspace, NSWorkspaceApplicationKey,
};
use objc2_foundation::{
    NSDictionary, NSDistributedNotificationCenter, NSKeyValueChangeNewKey, NSNotification,
    NSNotificationCenter, NSNumber, NSObject, NSString,
};
use std::ffi::c_void;
use stdext::function_name;

use crate::events::{Event, EventSender};
use crate::manager::Process;

/// `Ivars` is a helper struct to hold instance variables for Objective-C classes implemented in Rust.
/// It primarily stores an `EventSender` for communication with the main event loop.
#[derive(Debug, Clone)]
pub struct Ivars {
    /// The `EventSender` to dispatch events.
    events: EventSender,
}

define_class!(
    // SAFETY:
    // - The superclass NSObject does not have any subclassing requirements.
    // - `Observer` does not implement `Drop`.
    #[unsafe(super(NSObject))]
    // If we were implementing delegate methods like `NSApplicationDelegate`,
    // we would specify the object to only be usable on the main thread:
    // #[thread_kind = MainThreadOnly]
    #[name = "Observer"]
    #[ivars = Ivars]
    #[derive(Debug)]
    pub struct WorkspaceObserver;

    impl WorkspaceObserver {
        /// Called when the active display changes.
        ///
        /// # Arguments
        ///
        /// * `_` - The notification object (unused).
        #[unsafe(method(activeDisplayDidChange:))]
        fn display_changed(&self, _: &NSNotification) {
            _ = self.ivars().events.send(Event::DisplayChanged);
        }

        /// Called when the active space changes.
        ///
        /// # Arguments
        ///
        /// * `_` - The notification object (unused).
        #[unsafe(method(activeSpaceDidChange:))]
        fn space_changed(&self, _: &NSNotification) {
            _ = self.ivars().events.send(Event::SpaceChanged);
        }

        /// Called when an application is hidden.
        ///
        /// # Arguments
        ///
        /// * `notification` - The notification object containing application info.
        #[unsafe(method(didHideApplication:))]
        fn application_hidden(&self, notification: &NSObject) {
            let pid = unsafe {
                let user_info: &NSDictionary = msg_send![notification, userInfo];
                let app: &NSRunningApplication =  msg_send![user_info, objectForKey: NSWorkspaceApplicationKey];
                app.processIdentifier()
            };

            let msg = Event::ApplicationHidden{ pid };
            _ = self.ivars().events.send(msg);
        }

        /// Called when an application is unhidden.
        ///
        /// # Arguments
        ///
        /// * `notification` - The notification object containing application info.
        #[unsafe(method(didUnhideApplication:))]
        fn application_unhidden(&self, notification: &NSObject) {
            let pid = unsafe {
                let user_info: &NSDictionary = msg_send![notification, userInfo];
                let app: &NSRunningApplication =  msg_send![user_info, objectForKey: NSWorkspaceApplicationKey];
                app.processIdentifier()
            };
            let msg = Event::ApplicationVisible{ pid };
            _ = self.ivars().events.send(msg);
        }

        /// Called when the system wakes from sleep.
        ///
        /// # Arguments
        ///
        /// * `notification` - The notification object.
        #[unsafe(method(didWake:))]
        fn system_woke(&self, notification: &NSObject) {
            let msg = Event::SystemWoke{
                msg: format!("WorkspaceObserver: {notification:?}"),
            };
            _ = self.ivars().events.send(msg);
        }

        /// Called when the menu bar hiding state changes.
        ///
        /// # Arguments
        ///
        /// * `notification` - The notification object.
        #[unsafe(method(didChangeMenuBarHiding:))]
        fn menubar_hidden(&self, notification: &NSObject) {
            let msg = Event::MenuBarHiddenChanged{
                msg: format!("WorkspaceObserver: {notification:?}"),
            };
            _ = self.ivars().events.send(msg);
        }

        /// Called when the Dock restarts.
        ///
        /// # Arguments
        ///
        /// * `notification` - The notification object.
        #[unsafe(method(didRestartDock:))]
        fn dock_restarted(&self, notification: &NSObject) {
            let msg = Event::DockDidRestart{
                msg: format!("WorkspaceObserver: {notification:?}"),
            };
            _ = self.ivars().events.send(msg);
        }

        /// Called when Dock preferences change.
        ///
        /// # Arguments
        ///
        /// * `notification` - The notification object.
        #[unsafe(method(didChangeDockPref:))]
        fn dock_pref_changed(&self, notification: &NSObject) {
            let msg = Event::DockDidChangePref{
                msg: format!("WorkspaceObserver: {notification:?}"),
            };
            _ = self.ivars().events.send(msg);
        }

        /// Called when a key-value observed property changes for a process.
        ///
        /// # Arguments
        ///
        /// * `key_path` - The key path of the changed property.
        /// * `_object` - The object being observed (unused).
        /// * `change` - A dictionary containing details of the change.
        /// * `context` - The context pointer, expected to be a `*mut Process`.
        #[unsafe(method(observeValueForKeyPath:ofObject:change:context:))]
        fn observe_value_for_keypath(
            &self,
            key_path: &NSString,
            _object: &NSObject,
            change: &NSDictionary,
            context: *mut c_void,
        ) {
            let Some(process) = NonNull::new(context).map(|ptr| unsafe { ptr.cast::<Process>().as_mut() }) else {
                warn!("{}: null pointer passed as context", function_name!());
                return;
            };

            let result = unsafe { change.objectForKey(NSKeyValueChangeNewKey) };
            let policy = result.and_then(|result| result.downcast_ref::<NSNumber>().map(NSNumber::intValue));

            match key_path.to_string().as_ref() {
                "finishedLaunching" => {
                    if policy.is_some_and(|value| value != 1) {
                        return;
                    }
                    process.unobserve_finished_launching();
                }
                "activationPolicy" => {
                    if policy.is_some_and(|value| i32::try_from(process.policy.0).is_ok_and(|policy| value == policy)) {
                        return;
                    }
                    process.policy = NSApplicationActivationPolicy(policy.unwrap() as isize);
                    process.unobserve_activation_policy();
                }
                err => {
                    warn!("{}: unknown key path {err:?}", function_name!());
                    return;
                }
            }

            let msg = Event::ApplicationLaunched {
                psn: process.psn,
                observer: process.observer.clone(),
            };
            _= self.ivars().events.send(msg);
            debug!(
                "{}: got {key_path:?} for {}",
                function_name!(),
                process.name
            );
        }
    }

);

impl WorkspaceObserver {
    /// Creates a new `WorkspaceObserver` instance.
    ///
    /// # Arguments
    ///
    /// * `events` - An `EventSender` to send workspace-related events.
    ///
    /// # Returns
    ///
    /// A `Retained<Self>` containing the new `WorkspaceObserver` instance.
    pub(super) fn new(events: EventSender) -> Retained<Self> {
        // Initialize instance variables.
        let this = Self::alloc().set_ivars(Ivars { events });
        // Call `NSObject`'s `init` method.
        unsafe { msg_send![super(this), init] }
    }

    /// Starts observing workspace notifications by registering selectors with `NSWorkspace` and `NSDistributedNotificationCenter`.
    pub(super) fn start(&self) {
        let methods = [
            (
                sel!(activeDisplayDidChange:),
                "NSWorkspaceActiveDisplayDidChangeNotification",
            ),
            (
                sel!(activeSpaceDidChange:),
                "NSWorkspaceActiveSpaceDidChangeNotification",
            ),
            (
                sel!(didHideApplication:),
                "NSWorkspaceDidHideApplicationNotification",
            ),
            (
                sel!(didUnhideApplication:),
                "NSWorkspaceDidUnhideApplicationNotification",
            ),
            (sel!(didWake:), "NSWorkspaceDidWakeNotification"),
        ];
        let shared_ws = NSWorkspace::sharedWorkspace();
        let notification_center = shared_ws.notificationCenter();

        for (sel, name) in &methods {
            debug!("{}: registering {} with {name}", function_name!(), *sel);
            let notification_type = NSString::from_str(name);
            unsafe {
                notification_center.addObserver_selector_name_object(
                    self,
                    *sel,
                    Some(&notification_type),
                    None,
                );
            };
        }

        let methods = [
            (
                sel!(didChangeMenuBarHiding:),
                "AppleInterfaceMenuBarHidingChangedNotification",
            ),
            (sel!(didChangeDockPref:), "com.apple.dock.prefchanged"),
        ];
        let distributed_notification_center = NSDistributedNotificationCenter::defaultCenter();
        for (sel, name) in &methods {
            debug!("{}: registering {} with {name}", function_name!(), *sel);
            let notification_type = NSString::from_str(name);
            unsafe {
                distributed_notification_center.addObserver_selector_name_object(
                    self,
                    *sel,
                    Some(&notification_type),
                    None,
                );
            };
        }

        let methods = [(
            sel!(didRestartDock:),
            "NSApplicationDockDidRestartNotification",
        )];
        let default_center = NSNotificationCenter::defaultCenter();
        for (sel, name) in &methods {
            debug!("{}: registering {} with {name}", function_name!(), *sel);
            let notification_type = NSString::from_str(name);
            unsafe {
                default_center.addObserver_selector_name_object(
                    self,
                    *sel,
                    Some(&notification_type),
                    None,
                );
            };
        }
    }
}

impl Drop for WorkspaceObserver {
    /// Deregisters all previously registered notification callbacks when the `WorkspaceObserver` is dropped.
    fn drop(&mut self) {
        info!("{}: deregistering callbacks.", function_name!());
        unsafe {
            NSWorkspace::sharedWorkspace()
                .notificationCenter()
                .removeObserver(self);
            NSNotificationCenter::defaultCenter().removeObserver(self);
            NSDistributedNotificationCenter::defaultCenter().removeObserver(self);
        }
    }
}

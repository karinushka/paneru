use core::ptr::NonNull;
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
use tracing::{debug, info, warn};

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

        /// Called when an application becomes the active application.
        ///
        /// # Arguments
        ///
        /// * `notification` - The notification object containing application info.
        #[unsafe(method(didActivateApplication:))]
        fn application_activated(&self, notification: &NSObject) {
            let pid = unsafe {
                let user_info: &NSDictionary = msg_send![notification, userInfo];
                let app: &NSRunningApplication =  msg_send![user_info, objectForKey: NSWorkspaceApplicationKey];
                app.processIdentifier()
            };

            let msg = Event::ApplicationActivated{ pid };
            _ = self.ivars().events.send(msg);
        }

        /// Called when an application stops being the active application.
        ///
        /// # Arguments
        ///
        /// * `notification` - The notification object containing application info.
        #[unsafe(method(didDeactivateApplication:))]
        fn application_deactivated(&self, notification: &NSObject) {
            let pid = unsafe {
                let user_info: &NSDictionary = msg_send![notification, userInfo];
                let app: &NSRunningApplication =  msg_send![user_info, objectForKey: NSWorkspaceApplicationKey];
                app.processIdentifier()
            };

            let msg = Event::ApplicationDeactivated{ pid };
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

        /// Called when the system theme (Light/Dark mode) changes.
        ///
        /// # Arguments
        ///
        /// * `_` - The notification object (unused).
        #[unsafe(method(didChangeTheme:))]
        fn theme_changed(&self, _: &NSNotification) {
            _ = self.ivars().events.send(Event::ThemeChanged);
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
                warn!("null pointer passed as context", );
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
                    warn!("unknown key path {err:?}", );
                    return;
                }
            }

            let msg = Event::ApplicationLaunched {
                psn: process.psn,
                observer: process.observer.clone(),
            };
            _= self.ivars().events.send(msg);
            debug!(
                "got {key_path:?} for {}",

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
                sel!(didActivateApplication:),
                "NSWorkspaceDidActivateApplicationNotification",
            ),
            (
                sel!(didDeactivateApplication:),
                "NSWorkspaceDidDeactivateApplicationNotification",
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
            debug!("registering {} with {name}", *sel);
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
            (
                sel!(didChangeTheme:),
                "AppleInterfaceThemeChangedNotification",
            ),
            (sel!(didChangeDockPref:), "com.apple.dock.prefchanged"),
        ];
        let distributed_notification_center = NSDistributedNotificationCenter::defaultCenter();
        for (sel, name) in &methods {
            debug!("registering {} with {name}", *sel);
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
            debug!("registering {} with {name}", *sel);
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
        info!("deregistering callbacks.");
        unsafe {
            NSWorkspace::sharedWorkspace()
                .notificationCenter()
                .removeObserver(self);
            NSNotificationCenter::defaultCenter().removeObserver(self);
            NSDistributedNotificationCenter::defaultCenter().removeObserver(self);
        }
    }
}

#[cfg(test)]
mod delayed_readiness_tests {
    use super::*;
    use crate::ecs::BProcess;
    use crate::manager::{Application, WindowManager};
    use crate::tests::{TEST_WORKSPACE_ID, TestHarness};
    use std::time::Duration;

    #[test]
    fn readiness_notification_resumes_retained_process() {
        let mut harness = TestHarness::new().with_windows(1);
        harness.advance(Duration::from_secs(1));
        let pid = 43;
        harness.mock_state.spawn_app(pid, "org.gnu.Emacs", "Emacs");
        let process = harness.mock_state.create_process(pid);
        let psn = crate::manager::ProcessApi::psn(&process);
        // Initial readiness polling has expired, leaving its KVO observer alive.
        harness.world().spawn(BProcess(Box::new(process)));
        let mut wm = harness.mock_state.create_window_manager();
        let state = harness.mock_state.clone();
        wm.expect_new_application()
            .returning(move |process| Ok(state.create_application(process.pid())));
        harness.world().insert_resource(WindowManager(Box::new(wm)));
        harness.mock_state.spawn_window(
            pid,
            TEST_WORKSPACE_ID,
            43,
            bevy::math::IRect::new(0, 0, 400, 600),
        );
        let (events, _receiver) = crate::events::EventSender::new();
        let observer = WorkspaceObserver::new(events);
        harness
            .world()
            .write_message(crate::events::Event::ApplicationLaunched { psn, observer });
        harness.advance(Duration::from_secs(2));
        let world = harness.world();
        assert!(
            world
                .query::<&Application>()
                .iter(world)
                .any(|app| app.pid() == pid),
            "late readiness must resume an existing process"
        );
        crate::tests::find_window_entity(43, harness.world());
    }

    /// An app that hid its Dock icon (Bitwarden in the menu bar) when Paneru
    /// started is kept and watched, and adopted once it turns Regular.
    #[test]
    fn app_without_dock_icon_at_startup_is_adopted_when_it_turns_regular() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, Ordering};

        let mut harness = TestHarness::new().with_windows(1);
        harness.advance(Duration::from_secs(1));
        let pid = 44;
        harness
            .mock_state
            .spawn_app(pid, "com.bitwarden.desktop", "Bitwarden");
        let regular = Arc::new(AtomicBool::new(false));
        let mut process = crate::manager::MockProcessApi::new();
        process.expect_pid().return_const(pid);
        let psn = crate::platform::ProcessSerialNumber {
            high: 0,
            low: pid.cast_unsigned(),
        };
        process.expect_psn().return_const(psn);
        process.expect_name().return_const("Bitwarden".to_string());
        process.expect_application().return_const(None);
        let flag = regular.clone();
        process
            .expect_is_observable()
            .returning(move || flag.load(Ordering::SeqCst));
        let flag = regular.clone();
        process
            .expect_ready()
            .returning(move || flag.load(Ordering::SeqCst));

        {
            let world = harness.world();
            let mut commands = world.commands();
            crate::ecs::systems::spawn_initial_process(
                BProcess(Box::new(process)),
                false,
                &mut commands,
            );
            world.flush();
        }
        let mut wm = harness.mock_state.create_window_manager();
        let state = harness.mock_state.clone();
        wm.expect_new_application()
            .returning(move |process| Ok(state.create_application(process.pid())));
        harness.world().insert_resource(WindowManager(Box::new(wm)));
        harness.mock_state.spawn_window(
            pid,
            TEST_WORKSPACE_ID,
            44,
            bevy::math::IRect::new(0, 0, 400, 600),
        );
        harness.advance(Duration::from_secs(70));
        let world = harness.world();
        assert!(
            world
                .query::<&BProcess>()
                .iter(world)
                .any(|p| p.pid() == pid),
            "the process must be kept while it waits to turn Regular"
        );

        // The activation policy KVO fires as the app shows its Dock icon.
        regular.store(true, Ordering::SeqCst);
        let (events, _receiver) = crate::events::EventSender::new();
        let observer = WorkspaceObserver::new(events);
        harness
            .world()
            .write_message(crate::events::Event::ApplicationLaunched { psn, observer });
        harness.advance(Duration::from_secs(2));
        let world = harness.world();
        assert!(
            world
                .query::<&Application>()
                .iter(world)
                .any(|app| app.pid() == pid),
            "turning Regular must adopt the app"
        );
        crate::tests::find_window_entity(44, harness.world());
    }

    #[test]
    fn headless_process_is_despawned_after_recovery_window() {
        let mut harness = TestHarness::new().with_windows(1);
        harness.advance(Duration::from_secs(1));
        let pid: i32 = 45;
        let mut process = crate::manager::MockProcessApi::new();
        process.expect_pid().return_const(pid);
        process
            .expect_psn()
            .return_const(crate::platform::ProcessSerialNumber {
                high: 0,
                low: pid.cast_unsigned(),
            });
        process
            .expect_name()
            .return_const("headless-helper".to_string());
        process.expect_ready().returning(|| false);
        harness.world().spawn((
            BProcess(Box::new(process)),
            crate::ecs::systems::PendingWindowRecovery::default(),
        ));

        harness.advance(Duration::from_secs(10));
        let world = harness.world();
        assert!(
            world
                .query::<&BProcess>()
                .iter(world)
                .any(|p| p.pid() == pid),
            "process must remain retained during the recovery window"
        );

        harness.advance(Duration::from_secs(55));
        let world = harness.world();
        assert!(
            !world
                .query::<&BProcess>()
                .iter(world)
                .any(|p| p.pid() == pid),
            "headless process must be despawned once the recovery window expires"
        );
    }
}

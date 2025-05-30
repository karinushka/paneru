use accessibility_sys::{
    AXError, AXObserverRef, AXUIElementCreateApplication, AXUIElementRef,
    kAXErrorNotificationAlreadyRegistered, kAXErrorSuccess,
};
use core::ptr::NonNull;
use log::{debug, error, info, warn};
use notify::event::{DataChange, ModifyKind};
use notify::{EventKind, FsEventWatcher, RecommendedWatcher, RecursiveMode, Watcher};
use objc2::rc::{Retained, autoreleasepool};
use objc2::{AllocAnyThread, DefinedClass, define_class, msg_send, sel};
use objc2_app_kit::{
    NSApplicationActivationPolicy, NSRunningApplication, NSWorkspace, NSWorkspaceApplicationKey,
};
use objc2_core_foundation::{
    CFMachPortCreateRunLoopSource, CFMachPortInvalidate, CFRetained, CFRunLoopAddSource,
    CFRunLoopGetMain, CFRunLoopRemoveSource, CFRunLoopRunInMode, CFString, kCFRunLoopCommonModes,
    kCFRunLoopDefaultMode,
};
use objc2_core_graphics::{
    CGDirectDisplayID, CGDisplayChangeSummaryFlags, CGDisplayRegisterReconfigurationCallback,
    CGDisplayRemoveReconfigurationCallback, CGError, CGEvent, CGEventField, CGEventFlags,
    CGEventGetFlags, CGEventGetIntegerValueField, CGEventGetLocation, CGEventTapCreate,
    CGEventTapEnable, CGEventTapLocation, CGEventTapOptions, CGEventTapPlacement, CGEventTapProxy,
    CGEventType,
};
use objc2_foundation::{
    NSDictionary, NSDistributedNotificationCenter, NSKeyValueChangeNewKey, NSNotificationCenter,
    NSNumber, NSObject, NSString,
};
use std::env;
use std::ffi::c_void;
use std::io::{Error, ErrorKind, Result};
use std::ops::Deref;
use std::path::PathBuf;
use std::ptr::null_mut;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, LazyLock};
use std::time::Duration;
use stdext::function_name;

use crate::config::Config;
use crate::events::{Event, EventSender};
use crate::process::Process;
use crate::skylight::OSStatus;
use crate::util::{AxuWrapperType, Cleanuper, add_run_loop, remove_run_loop};

pub type Pid = i32;
pub type CFStringRef = *const CFString;

type AXObserverCallback = unsafe extern "C" fn(
    observer: AXObserverRef,
    element: AXUIElementRef,
    notification: CFStringRef,
    refcon: *mut c_void,
);

unsafe extern "C" {
    pub fn AXObserverCreate(
        application: Pid,
        callback: AXObserverCallback,
        out_observer: &mut AXObserverRef,
    ) -> AXError;
    pub fn AXObserverAddNotification(
        observer: AXObserverRef,
        element: AXUIElementRef,
        notification: &CFString,
        refcon: *mut c_void,
    ) -> AXError;
    pub fn AXObserverRemoveNotification(
        observer: AXObserverRef,
        element: AXUIElementRef,
        notification: &CFString,
    ) -> AXError;
}

#[derive(Clone, Debug, Default, Hash, PartialEq, Eq)]
#[repr(C)]
pub struct ProcessSerialNumber {
    pub high: u32,
    pub low: u32,
}

#[derive(Debug)]
#[repr(C)]
pub struct ProcessInfo {
    pub pid: Pid,
    pub name: CFStringRef,
    ns_application: *const c_void,
    policy: i32,
    pub terminated: bool,
}

impl Default for ProcessInfo {
    fn default() -> Self {
        ProcessInfo {
            pid: 0,
            name: std::ptr::null(),
            ns_application: std::ptr::null(),
            policy: 0,
            terminated: false,
        }
    }
}

type ProcessCallbackFn = extern "C-unwind" fn(
    this: *mut c_void,
    psn: &ProcessSerialNumber,
    event: ProcessEventApp,
) -> OSStatus;

#[link(name = "private")]
unsafe extern "C-unwind" {
    fn setup_process_handler(callback: ProcessCallbackFn, this: *mut c_void) -> *const c_void;
    fn remove_process_handler(handler: *const c_void) -> c_void;
    pub fn get_process_info(psn: &ProcessSerialNumber, pi: &mut ProcessInfo) -> OSStatus;
}

#[repr(C)]
struct ProcessHandler {
    events: EventSender,
    cleanup: Option<Cleanuper>,
    observer: Retained<WorkspaceObserver>,
}

#[repr(C)]
#[allow(dead_code)]
enum ProcessEventApp {
    Activated = 1,
    Deactivated = 2,
    Quit = 3,
    LaunchNotification = 4,
    Launched = 5,
    Terminated = 6,
    FrontSwitched = 7,

    FocusMenuBar = 8,
    FocusNextDocumentWindow = 9,
    FocusNextFloatingWindow = 10,
    FocusToolbar = 11,
    FocusDrawer = 12,

    GetDockTileMenu = 20,
    UpdateDockTile = 21,

    IsEventInInstantMouser = 104,

    Hidden = 107,
    Shown = 108,
    SystemUIModeChanged = 109,
    AvailableWindowBoundsChanged = 110,
    ActiveWindowChanged = 111,
}

impl ProcessHandler {
    fn new(events: EventSender, observer: Retained<WorkspaceObserver>) -> Self {
        ProcessHandler {
            events,
            cleanup: None,
            observer,
        }
    }

    fn start(&mut self) {
        info!("{}: Registering process_handler", function_name!());
        let handler = unsafe {
            let me = NonNull::new_unchecked(self).as_ptr();
            setup_process_handler(Self::callback, me.cast())
        };

        self.cleanup = Some(Cleanuper::new(Box::new(move || unsafe {
            info!(
                "{}: Unregistering process_handler: {handler:?}",
                function_name!()
            );
            remove_process_handler(handler);
        })));
    }

    extern "C-unwind" fn callback(
        this: *mut c_void,
        psn: &ProcessSerialNumber,
        event: ProcessEventApp,
    ) -> OSStatus {
        match NonNull::new(this).map(|this| unsafe { this.cast::<ProcessHandler>().as_mut() }) {
            Some(this) => this.process_handler(psn, event),
            None => error!("Zero passed to Process Handler."),
        }
        0
    }

    fn process_handler(&mut self, psn: &ProcessSerialNumber, event: ProcessEventApp) {
        let psn = psn.clone();
        let _ = match event {
            ProcessEventApp::Launched => self.events.send(Event::ApplicationLaunched {
                psn,
                observer: self.observer.clone(),
            }),
            ProcessEventApp::Terminated => self.events.send(Event::ApplicationTerminated { psn }),
            ProcessEventApp::FrontSwitched => {
                self.events.send(Event::ApplicationFrontSwitched { psn })
            }
            _ => {
                error!(
                    "{}: Unknown process event: {}",
                    function_name!(),
                    event as u32
                );
                Ok(())
            }
        }
        .inspect_err(|err| error!("{}: error sending event: {err}", function_name!()));
    }
}

struct InputHandler {
    events: EventSender,
    config: Config,
    cleanup: Option<Cleanuper>,
}

impl InputHandler {
    fn new(events: EventSender, config: Config) -> Self {
        InputHandler {
            events,
            config,
            cleanup: None,
        }
    }

    fn start(&mut self) -> Result<()> {
        let mouse_event_mask = (1 << CGEventType::MouseMoved.0)
            | (1 << CGEventType::LeftMouseDown.0)
            | (1 << CGEventType::LeftMouseUp.0)
            | (1 << CGEventType::LeftMouseDragged.0)
            | (1 << CGEventType::RightMouseDown.0)
            | (1 << CGEventType::RightMouseUp.0)
            | (1 << CGEventType::RightMouseDragged.0)
            | (1 << CGEventType::KeyDown.0);

        unsafe {
            let this = NonNull::new_unchecked(self).as_ptr();
            let port = CGEventTapCreate(
                CGEventTapLocation::HIDEventTap,
                CGEventTapPlacement::HeadInsertEventTap,
                CGEventTapOptions::Default,
                mouse_event_mask,
                Some(Self::callback),
                this.cast(),
            )
            .ok_or(Error::new(
                ErrorKind::PermissionDenied,
                format!("{}: Can not create EventTap.", function_name!()),
            ))?;

            let (run_loop_source, main_loop) = CFMachPortCreateRunLoopSource(None, Some(&port), 0)
                .zip(CFRunLoopGetMain())
                .ok_or(Error::new(
                    ErrorKind::PermissionDenied,
                    format!("{}: Unable to create run loop source", function_name!()),
                ))?;
            CFRunLoopAddSource(&main_loop, Some(&run_loop_source), kCFRunLoopCommonModes);

            self.cleanup = Some(Cleanuper::new(Box::new(move || {
                info!("{}: Unregistering event_handler", function_name!());
                CFRunLoopRemoveSource(&main_loop, Some(&run_loop_source), kCFRunLoopCommonModes);
                CFMachPortInvalidate(&port);
                CGEventTapEnable(&port, false);
            })));
        }
        Ok(())
    }

    extern "C-unwind" fn callback(
        _: CGEventTapProxy,
        event_type: CGEventType,
        mut event_ref: NonNull<CGEvent>,
        this: *mut c_void,
    ) -> *mut CGEvent {
        match NonNull::new(this).map(|this| unsafe { this.cast::<InputHandler>().as_mut() }) {
            Some(this) => {
                let intercept = this.input_handler(event_type, unsafe { event_ref.as_ref() });
                if intercept {
                    return null_mut();
                }
            }
            None => error!("Zero passed to Event Handler."),
        }
        unsafe { event_ref.as_mut() }
    }

    fn input_handler(&mut self, event_type: CGEventType, event: &CGEvent) -> bool {
        let result = match event_type {
            CGEventType::TapDisabledByTimeout | CGEventType::TapDisabledByUserInput => {
                warn!("{}: Tap Disabled", function_name!());
                Ok(())
            }
            CGEventType::LeftMouseDown | CGEventType::RightMouseDown => {
                let point = unsafe { CGEventGetLocation(Some(event)) };
                self.events.send(Event::MouseDown { point })
            }
            CGEventType::LeftMouseUp | CGEventType::RightMouseUp => {
                let point = unsafe { CGEventGetLocation(Some(event)) };
                self.events.send(Event::MouseUp { point })
            }
            CGEventType::LeftMouseDragged | CGEventType::RightMouseDragged => {
                let point = unsafe { CGEventGetLocation(Some(event)) };
                self.events.send(Event::MouseDragged { point })
            }
            CGEventType::MouseMoved => {
                let point = unsafe { CGEventGetLocation(Some(event)) };
                self.events.send(Event::MouseMoved { point })
            }
            CGEventType::KeyDown => {
                let keycode = unsafe {
                    CGEventGetIntegerValueField(Some(event), CGEventField::KeyboardEventKeycode)
                };
                let eventflags = unsafe { CGEventGetFlags(Some(event)) };
                // handle_keypress can intercept the event, so it may return true.
                return self.handle_keypress(keycode, eventflags);
            }
            _ => {
                info!(
                    "{}: Unknown event type received: {event_type:?}",
                    function_name!()
                );
                Ok(())
            }
        };
        if let Err(err) = result {
            error!("{}: error sending event: {err}", function_name!());
        }
        // Do not intercept this event, let it fall through.
        false
    }

    fn handle_keypress(&self, keycode: i64, eventflags: CGEventFlags) -> bool {
        const MODIFIER_MASKS: [[u64; 3]; 4] = [
            // Normal key, left, right.
            [0x00080000, 0x00000020, 0x00000040], // Alt
            [0x00020000, 0x00000002, 0x00000004], // Shift
            [0x00100000, 0x00000008, 0x00000010], // Command
            [0x00040000, 0x00000001, 0x00002000], // Control
        ];
        let mask = MODIFIER_MASKS
            .iter()
            .enumerate()
            .flat_map(|(bitshift, modifier)| {
                modifier
                    .iter()
                    .any(|mask| *mask == (eventflags.0 & mask))
                    .then_some(1 << bitshift)
            })
            .reduce(|acc, mask| acc + mask)
            .unwrap_or(0);

        let keycode = keycode.try_into().ok();
        let bind = keycode.and_then(|keycode| self.config.find_keybind(keycode, mask));
        bind.and_then(|bind| {
            let command = bind
                .command
                .split("_")
                .map(|s| s.to_string())
                .collect::<Vec<_>>();
            self.events
                .send(Event::Command { argv: command })
                .inspect_err(|err| error!("{}: Error sending command: {err}", function_name!()))
                .ok()
        })
        .is_some()
    }
}

#[derive(Debug, Clone)]
pub struct Ivars {
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
        #[unsafe(method(activeDisplayDidChange:))]
        fn display_changed(&self, notification: &NSObject) {
            let msg = Event::DisplayChanged{
                msg: format!("WorkspaceObserver: {:?}", notification),
            };
            _ = self.ivars().events.send(msg);
        }

        #[unsafe(method(activeSpaceDidChange:))]
        fn space_changed(&self, notification: &NSObject) {
            let msg = Event::SpaceChanged{
                msg: format!("WorkspaceObserver: {:?}", notification),
            };
            _ = self.ivars().events.send(msg);
        }

        #[unsafe(method(didHideApplication:))]
        fn application_hidden(&self, notification: &NSObject) {
            // pid_t pid = [[notification.userInfo objectForKey:NSWorkspaceApplicationKey] processIdentifier];
            let pid = unsafe {
                let user_info: &NSDictionary = msg_send![notification, userInfo];
                let app: &NSRunningApplication =  msg_send![user_info, objectForKey: NSWorkspaceApplicationKey];
                app.processIdentifier()
            };

            let msg = Event::ApplicationHidden{
                msg: format!("WorkspaceObserver: {pid}"),
            };
            _ = self.ivars().events.send(msg);
        }

        #[unsafe(method(didUnhideApplication:))]
        fn application_unhidden(&self, notification: &NSObject) {
            // pid_t pid = [[notification.userInfo objectForKey:NSWorkspaceApplicationKey] processIdentifier];
            let pid = unsafe {
                let user_info: &NSDictionary = msg_send![notification, userInfo];
                let app: &NSRunningApplication =  msg_send![user_info, objectForKey: NSWorkspaceApplicationKey];
                app.processIdentifier()
            };
            let msg = Event::ApplicationVisible{
                msg: format!("WorkspaceObserver: {pid}"),
            };
            _ = self.ivars().events.send(msg);
        }

        #[unsafe(method(didWake:))]
        fn system_woke(&self, notification: &NSObject) {
            let msg = Event::SystemWoke{
                msg: format!("WorkspaceObserver: {:?}", notification),
            };
            _ = self.ivars().events.send(msg);
        }

        #[unsafe(method(didChangeMenuBarHiding:))]
        fn menubar_hidden(&self, notification: &NSObject) {
            let msg = Event::MenuBarHiddenChanged{
                msg: format!("WorkspaceObserver: {:?}", notification),
            };
            _ = self.ivars().events.send(msg);
        }

        #[unsafe(method(didRestartDock:))]
        fn dock_restarted(&self, notification: &NSObject) {
            let msg = Event::DockDidRestart{
                msg: format!("WorkspaceObserver: {:?}", notification),
            };
            _ = self.ivars().events.send(msg);
        }

        #[unsafe(method(didChangeDockPref:))]
        fn dock_pref_changed(&self, notification: &NSObject) {
            let msg = Event::DockDidChangePref{
                msg: format!("WorkspaceObserver: {:?}", notification),
            };
            _ = self.ivars().events.send(msg);
        }

        #[unsafe(method(observeValueForKeyPath:ofObject:change:context:))]
        fn observe_value_for_keypath(
            &self,
            key_path: &NSString,
            _object: &NSObject,
            change: &NSDictionary,
            context: *mut c_void,
        ) {
            let process = match NonNull::new(context) {
                Some(process) => unsafe { process.cast::<Process>().as_mut() },
                None => {
                    warn!("{}: null pointer passed as context", function_name!());
                    return;
                },
            };

            let result = unsafe { change.objectForKey(NSKeyValueChangeNewKey) };
            let policy = result.and_then(|result| result.downcast_ref::<NSNumber>().map(|result| result.intValue()));

            match key_path.to_string().as_ref() {
                "finishedLaunching" => {
                    if policy.is_some_and(|value| value != 1) {
                        return;
                    }
                    process.unobserve_finished_launching();
                }
                "activationPolicy" => {
                    if policy.is_some_and(|value| value == process.policy.0 as i32) {
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
                psn: process.psn.clone(),
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
    fn new(events: EventSender) -> Retained<Self> {
        // Initialize instance variables.
        let this = Self::alloc().set_ivars(Ivars { events });
        // Call `NSObject`'s `init` method.
        unsafe { msg_send![super(this), init] }
    }

    fn start(&self) {
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
        let shared_ws = unsafe { NSWorkspace::sharedWorkspace() };
        let notification_center = unsafe { shared_ws.notificationCenter() };

        methods.iter().for_each(|(sel, name)| {
            debug!("{}: registering {} with {name}", function_name!(), *sel);
            let notification_type = NSString::from_str(name);
            unsafe {
                notification_center.addObserver_selector_name_object(
                    self,
                    *sel,
                    Some(&notification_type),
                    None,
                )
            };
        });

        let methods = [
            (
                sel!(didChangeMenuBarHiding:),
                "AppleInterfaceMenuBarHidingChangedNotification",
            ),
            (sel!(didChangeDockPref:), "com.apple.dock.prefchanged"),
        ];
        let distributed_notification_center =
            unsafe { NSDistributedNotificationCenter::defaultCenter() };
        methods.iter().for_each(|(sel, name)| {
            debug!("{}: registering {} with {name}", function_name!(), *sel);
            let notification_type = NSString::from_str(name);
            unsafe {
                distributed_notification_center.addObserver_selector_name_object(
                    self,
                    *sel,
                    Some(&notification_type),
                    None,
                )
            };
        });

        let methods = [(
            sel!(didRestartDock:),
            "NSApplicationDockDidRestartNotification",
        )];
        let default_center = unsafe { NSNotificationCenter::defaultCenter() };
        methods.iter().for_each(|(sel, name)| {
            debug!("{}: registering {} with {name}", function_name!(), *sel);
            let notification_type = NSString::from_str(name);
            unsafe {
                default_center.addObserver_selector_name_object(
                    self,
                    *sel,
                    Some(&notification_type),
                    None,
                )
            };
        });
    }
}

impl Drop for WorkspaceObserver {
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

#[derive(Debug)]
struct MissionControlHandler {
    events: EventSender,
    element: Option<CFRetained<AxuWrapperType>>,
    observer: Option<CFRetained<AxuWrapperType>>,
}

impl MissionControlHandler {
    fn new(events: EventSender) -> Self {
        Self {
            events,
            element: None,
            observer: None,
        }
    }

    const EVENTS: [&str; 4] = [
        "AXExposeShowAllWindows",
        "AXExposeShowFrontWindows",
        "AXExposeShowDesktop",
        "AXExposeExit",
    ];

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
                warn!(
                    "{}: Unknown mission control event: {notification}",
                    function_name!()
                );
                return;
            }
        };
        _ = self
            .events
            .send(event)
            .inspect_err(|err| error!("{}: error sending event: {err}", function_name!()));
    }

    fn dock_pid() -> Result<Pid> {
        let dock = NSString::from_str("com.apple.dock");
        let array = unsafe { NSRunningApplication::runningApplicationsWithBundleIdentifier(&dock) };
        array
            .iter()
            .next()
            .map(|running| unsafe { running.processIdentifier() })
            .ok_or(Error::new(
                ErrorKind::NotFound,
                format!("{}: can not find dock.", function_name!()),
            ))
    }

    fn observe(&mut self) -> Result<()> {
        let pid = MissionControlHandler::dock_pid()?;
        let element = AxuWrapperType::from_retained(unsafe { AXUIElementCreateApplication(pid) })?;
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

        Self::EVENTS.iter().for_each(|name| {
            debug!(
                "{}: {name:?} {:?}",
                function_name!(),
                observer.as_ptr::<AXObserverRef>()
            );
            let notification = CFString::from_static_str(name);
            match unsafe {
                AXObserverAddNotification(
                    observer.as_ptr(),
                    element.as_ptr(),
                    notification.deref(),
                    NonNull::new_unchecked(self).as_ptr().cast(),
                )
            } {
                accessibility_sys::kAXErrorSuccess
                | accessibility_sys::kAXErrorNotificationAlreadyRegistered => (),
                result => error!(
                    "{}: error registering {name} for application {pid}: {result}",
                    function_name!(),
                ),
            }
        });
        unsafe { add_run_loop(observer.deref(), kCFRunLoopDefaultMode)? };
        self.observer = observer.into();
        self.element = element.into();
        Ok(())
    }

    fn unobserve(&mut self) {
        if let Some((observer, element)) = self.observer.take().zip(self.element.as_ref()) {
            Self::EVENTS.iter().for_each(|name| {
                debug!(
                    "{}: {name:?} {:?}",
                    function_name!(),
                    observer.as_ptr::<AXObserverRef>()
                );
                let notification = CFString::from_static_str(name);
                let result = unsafe {
                    AXObserverRemoveNotification(
                        observer.as_ptr(),
                        element.as_ptr(),
                        notification.deref(),
                    )
                };
                if result != kAXErrorSuccess && result != kAXErrorNotificationAlreadyRegistered {
                    error!("{}: error unregistering {name}: {result}", function_name!());
                }
            });
            remove_run_loop(observer.deref());
            drop(observer)
        } else {
            warn!(
                "{}: unobserving without observe or element",
                function_name!()
            );
        }
    }

    extern "C" fn callback(
        observer: AXObserverRef,
        element: AXUIElementRef,
        notification: CFStringRef,
        context: *mut c_void,
    ) {
        let notification = match NonNull::new(notification.cast_mut()) {
            Some(notification) => unsafe { notification.as_ref() }.to_string(),
            None => {
                error!("{}: nullptr 'notification' passed.", function_name!());
                return;
            }
        };

        match NonNull::new(context)
            .map(|this| unsafe { this.cast::<MissionControlHandler>().as_ref() })
        {
            Some(this) => this.mission_control_handler(observer, element, notification.as_ref()),
            None => error!("Zero passed to MissionControlHandler."),
        }
    }
}

impl Drop for MissionControlHandler {
    fn drop(&mut self) {
        self.unobserve();
    }
}

struct DisplayHandler {
    events: EventSender,
    cleanup: Option<Cleanuper>,
}

impl DisplayHandler {
    fn new(events: EventSender) -> Self {
        Self {
            events,
            cleanup: None,
        }
    }

    fn start(&mut self) -> Result<()> {
        info!("{}: Registering display handler", function_name!());
        let this = unsafe { NonNull::new_unchecked(self).as_ptr() };
        let result =
            unsafe { CGDisplayRegisterReconfigurationCallback(Some(Self::callback), this.cast()) };
        if result != CGError::Success {
            return Err(Error::new(
                ErrorKind::PermissionDenied,
                format!(
                    "{}: registering display handler callback: {result:?}",
                    function_name!()
                ),
            ));
        }
        self.cleanup = Some(Cleanuper::new(Box::new(move || unsafe {
            info!("{}: Unregistering display handler", function_name!());
            CGDisplayRemoveReconfigurationCallback(Some(Self::callback), this.cast());
        })));
        Ok(())
    }

    extern "C-unwind" fn callback(
        display_id: CGDirectDisplayID,
        flags: CGDisplayChangeSummaryFlags,
        context: *mut c_void,
    ) {
        match NonNull::new(context).map(|this| unsafe { this.cast::<DisplayHandler>().as_mut() }) {
            Some(this) => this.display_handler(display_id, flags),
            None => error!("Zero passed to Display Handler."),
        }
    }

    fn display_handler(
        &mut self,
        display_id: CGDirectDisplayID,
        flags: CGDisplayChangeSummaryFlags,
    ) {
        info!("display_handler: display change {display_id:?}");
        let event = if flags.contains(CGDisplayChangeSummaryFlags::AddFlag) {
            Event::DisplayAdded { display_id }
        } else if flags.contains(CGDisplayChangeSummaryFlags::RemoveFlag) {
            Event::DisplayRemoved { display_id }
        } else if flags.contains(CGDisplayChangeSummaryFlags::MovedFlag) {
            Event::DisplayMoved { display_id }
        } else if flags.contains(CGDisplayChangeSummaryFlags::DesktopShapeChangedFlag) {
            Event::DisplayResized { display_id }
        } else {
            warn!("{}: unknown flag {flags:?}.", function_name!());
            return;
        };
        _ = self
            .events
            .send(event)
            .inspect_err(|err| warn!("{}: error sending event: {err}", function_name!()));
    }
}

struct ConfigHandler {
    events: EventSender,
    config: Config,
}

impl ConfigHandler {
    fn announce_fresh_config(&self) -> Result<()> {
        self.events.send(Event::ConfigRefresh {
            config: self.config.clone(),
        })?;
        Ok(())
    }
}

impl notify::EventHandler for ConfigHandler {
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
        }
    }
}

fn setup_config_watcher(events: EventSender, config: Config) -> Result<FsEventWatcher> {
    let setup = notify::Config::default().with_poll_interval(Duration::from_secs(3));
    let config_handler = ConfigHandler { events, config };
    config_handler.announce_fresh_config()?;
    let watcher = RecommendedWatcher::new(config_handler, setup);
    watcher
        .and_then(|mut watcher| {
            watcher.watch(CONFIGURATION_FILE.as_path(), RecursiveMode::NonRecursive)?;
            Ok(watcher)
        })
        .map_err(|err| {
            Error::new(
                ErrorKind::PermissionDenied,
                format!("{}: {err}", function_name!()),
            )
        })
}

pub static CONFIGURATION_FILE: LazyLock<PathBuf> = LazyLock::new(|| {
    let homedir = PathBuf::from(env::var("HOME").expect("Missing $HOME environment variable."));
    homedir.join(".paneru")
});

pub struct PlatformCallbacks {
    events: EventSender,
    process_handler: ProcessHandler,
    event_handler: InputHandler,
    workspace_observer: Retained<WorkspaceObserver>,
    mission_control_observer: MissionControlHandler,
    display_handler: DisplayHandler,
    _config_watcher: FsEventWatcher,
}

impl PlatformCallbacks {
    pub fn new(events: EventSender) -> Result<std::pin::Pin<Box<Self>>> {
        let config = Config::new(CONFIGURATION_FILE.as_path()).map_err(|err| {
            Error::new(
                ErrorKind::InvalidInput,
                format!("{}: failed loading config {err}", function_name!()),
            )
        })?;

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

    pub fn setup_handlers(&mut self) -> Result<()> {
        self.event_handler.start()?;
        self.display_handler.start()?;
        self.mission_control_observer.observe()?;
        self.workspace_observer.start();
        self.process_handler.start();

        self.events.send(Event::ProcessesLoaded)
    }

    // Does not return until 'quit' is signalled.
    pub fn run(&mut self, quit: Arc<AtomicBool>) {
        info!("{}: Starting run loop...", function_name!());
        loop {
            if quit.load(std::sync::atomic::Ordering::Relaxed) {
                break;
            }
            autoreleasepool(|_| unsafe {
                CFRunLoopRunInMode(kCFRunLoopDefaultMode, 3.0, false);
            });
        }
        _ = self.events.send(Event::Exit);
        info!("{}: Run loop finished.", function_name!());
    }
}

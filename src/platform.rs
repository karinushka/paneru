use accessibility_sys::{
    AXError, AXObserverRef, AXUIElementCreateApplication, AXUIElementRef,
    kAXErrorNotificationAlreadyRegistered, kAXErrorSuccess,
};
use core::ptr::NonNull;
use log::{debug, error, info, warn};
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
    CGDisplayRemoveReconfigurationCallback, CGError, CGEvent, CGEventGetLocation, CGEventTapCreate,
    CGEventTapEnable, CGEventTapLocation, CGEventTapOptions, CGEventTapPlacement, CGEventTapProxy,
    CGEventType,
};
use objc2_foundation::{
    NSDictionary, NSDistributedNotificationCenter, NSKeyValueChangeNewKey, NSNotificationCenter,
    NSNumber, NSObject, NSString,
};
use std::ffi::c_void;
use std::io::{Error, ErrorKind, Result};
use std::ops::Deref;
use std::ptr::null_mut;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::mpsc::Sender;
use stdext::function_name;

use crate::events::Event;
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
    tx: Sender<Event>,
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
    fn new(tx: Sender<Event>, observer: Retained<WorkspaceObserver>) -> Self {
        ProcessHandler {
            tx,
            cleanup: None,
            observer,
        }
    }

    fn start(&mut self) {
        info!("{}: Registering process_handler", function_name!());
        let handler = unsafe {
            let me = self as *const Self;
            setup_process_handler(Self::callback, me as *mut c_void)
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
        let result = match event {
            ProcessEventApp::Launched => self.tx.send(Event::ApplicationLaunched {
                psn,
                observer: self.observer.clone(),
            }),
            ProcessEventApp::Terminated => self.tx.send(Event::ApplicationTerminated { psn }),
            ProcessEventApp::FrontSwitched => self.tx.send(Event::ApplicationFrontSwitched { psn }),
            _ => {
                error!(
                    "{}: Unknown process event: {}",
                    function_name!(),
                    event as u32
                );
                Ok(())
            }
        };
        if let Err(err) = result {
            error!("{}: error sending event: {err}", function_name!());
        }
    }
}

struct MouseHandler {
    tx: Sender<Event>,
    cleanup: Option<Cleanuper>,
}

impl MouseHandler {
    fn new(tx: Sender<Event>) -> Self {
        MouseHandler { tx, cleanup: None }
    }

    fn start(&mut self) -> Result<()> {
        let mouse_event_mask = (1 << CGEventType::MouseMoved.0)
            | (1 << CGEventType::LeftMouseDown.0)
            | (1 << CGEventType::LeftMouseUp.0)
            | (1 << CGEventType::LeftMouseDragged.0)
            | (1 << CGEventType::RightMouseDown.0)
            | (1 << CGEventType::RightMouseUp.0)
            | (1 << CGEventType::RightMouseDragged.0);

        unsafe {
            let this: *mut MouseHandler = &mut *self;
            let port = CGEventTapCreate(
                CGEventTapLocation::HIDEventTap,
                CGEventTapPlacement::HeadInsertEventTap,
                CGEventTapOptions::Default,
                mouse_event_mask,
                Some(Self::callback),
                this as *mut c_void,
            )
            .ok_or(Error::new(
                ErrorKind::PermissionDenied,
                format!("{}: Can not create EventTap.", function_name!()),
            ))?;

            let run_loop_source = CFMachPortCreateRunLoopSource(None, Some(&port), 0)
                .expect("Unable to create Mach port.");
            let main_loop = CFRunLoopGetMain().expect("Unable to get the main run loop.");

            CFRunLoopAddSource(&main_loop, Some(&run_loop_source), kCFRunLoopCommonModes);

            self.cleanup = Some(Cleanuper::new(Box::new(move || {
                info!("{}: Unregistering mouse_handler", function_name!());
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
        match NonNull::new(this).map(|this| unsafe { this.cast::<MouseHandler>().as_mut() }) {
            Some(this) => this.mouse_handler(event_type, unsafe { event_ref.as_ref() }),
            None => error!("Zero passed to Mouse Handler."),
        }
        unsafe { event_ref.as_mut() }
    }

    fn mouse_handler(&mut self, event_type: CGEventType, event: &CGEvent) {
        let result = match event_type {
            CGEventType::TapDisabledByTimeout | CGEventType::TapDisabledByUserInput => {
                warn!("{}: Tap Disabled", function_name!());
                Ok(())
            }
            CGEventType::LeftMouseDown | CGEventType::RightMouseDown => {
                let point = unsafe { CGEventGetLocation(Some(event)) };
                self.tx.send(Event::MouseDown { point })
            }
            CGEventType::LeftMouseUp | CGEventType::RightMouseUp => {
                let point = unsafe { CGEventGetLocation(Some(event)) };
                self.tx.send(Event::MouseUp { point })
            }
            CGEventType::LeftMouseDragged | CGEventType::RightMouseDragged => {
                let point = unsafe { CGEventGetLocation(Some(event)) };
                self.tx.send(Event::MouseDragged { point })
            }
            CGEventType::MouseMoved => {
                let point = unsafe { CGEventGetLocation(Some(event)) };
                self.tx.send(Event::MouseMoved { point })
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
    }
}

#[derive(Debug, Clone)]
pub struct Ivars {
    tx: Sender<Event>,
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
            _ = self.ivars().tx.send(msg).inspect_err(|err| error!("{}: error sending event: {err}", function_name!()));
        }

        #[unsafe(method(activeSpaceDidChange:))]
        fn space_changed(&self, notification: &NSObject) {
            let msg = Event::SpaceChanged{
                msg: format!("WorkspaceObserver: {:?}", notification),
            };
            _ = self.ivars().tx.send(msg).inspect_err(|err| error!("{}: error sending event: {err}", function_name!()));
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
            _ = self.ivars().tx.send(msg).inspect_err(|err| error!("{}: error sending event: {err}", function_name!()));
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
            _ = self.ivars().tx.send(msg).inspect_err(|err| error!("{}: error sending event: {err}", function_name!()));
        }

        #[unsafe(method(didWake:))]
        fn system_woke(&self, notification: &NSObject) {
            let msg = Event::SystemWoke{
                msg: format!("WorkspaceObserver: {:?}", notification),
            };
            _ = self.ivars().tx.send(msg).inspect_err(|err| error!("{}: error sending event: {err}", function_name!()));
        }

        #[unsafe(method(didChangeMenuBarHiding:))]
        fn menubar_hidden(&self, notification: &NSObject) {
            let msg = Event::MenuBarHiddenChanged{
                msg: format!("WorkspaceObserver: {:?}", notification),
            };
            _ = self.ivars().tx.send(msg).inspect_err(|err| error!("{}: error sending event: {err}", function_name!()));
        }

        #[unsafe(method(didRestartDock:))]
        fn dock_restarted(&self, notification: &NSObject) {
            let msg = Event::DockDidRestart{
                msg: format!("WorkspaceObserver: {:?}", notification),
            };
            _ = self.ivars().tx.send(msg).inspect_err(|err| error!("{}: error sending event: {err}", function_name!()));
        }

        #[unsafe(method(didChangeDockPref:))]
        fn dock_pref_changed(&self, notification: &NSObject) {
            let msg = Event::DockDidChangePref{
                msg: format!("WorkspaceObserver: {:?}", notification),
            };
            _ = self.ivars().tx.send(msg).inspect_err(|err| error!("{}: error sending event: {err}", function_name!()));
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
            self.ivars()
                .tx
                .send(msg)
                .expect("observe_value_for_keypath: Error sending event!");
            debug!(
                "{}: got {key_path:?} for {}",
                function_name!(),
                process.name
            );
        }
    }

);

impl WorkspaceObserver {
    fn new(tx: Sender<Event>) -> Retained<Self> {
        // Initialize instance variables.
        let this = Self::alloc().set_ivars(Ivars { tx });
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
    tx: Sender<Event>,
    element: Option<CFRetained<AxuWrapperType>>,
    observer: Option<CFRetained<AxuWrapperType>>,
}

impl MissionControlHandler {
    fn new(tx: Sender<Event>) -> Self {
        Self {
            tx,
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
            .tx
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

        self.element =
            AxuWrapperType::from_retained(unsafe { AXUIElementCreateApplication(pid) })?.into();
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
                    self.element.as_ref().unwrap().as_ptr(),
                    notification.deref(),
                    self as *const Self as *mut c_void,
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
        Ok(())
    }

    fn unobserve(&mut self) {
        if let Some(observer) = self.observer.take() {
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
                        self.element.as_ref().unwrap().as_ptr(),
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
            warn!("{}: unobserving without observe", function_name!());
        }
    }

    extern "C" fn callback(
        observer: AXObserverRef,
        element: AXUIElementRef,
        notification: CFStringRef,
        context: *mut c_void,
    ) {
        let notification = match NonNull::new(notification as *mut CFString) {
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
    tx: Sender<Event>,
    cleanup: Option<Cleanuper>,
}

impl DisplayHandler {
    fn new(tx: Sender<Event>) -> Self {
        Self { tx, cleanup: None }
    }

    fn start(&mut self) -> Result<()> {
        info!("{}: Registering display handler", function_name!());
        let me = self as *const Self;
        let result = unsafe {
            CGDisplayRegisterReconfigurationCallback(Some(Self::callback), me as *mut c_void)
        };
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
            CGDisplayRemoveReconfigurationCallback(Some(Self::callback), me as *mut c_void);
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
            .tx
            .send(event)
            .inspect_err(|err| warn!("{}: error sending event: {err}", function_name!()));
    }
}

pub struct PlatformCallbacks {
    tx: Sender<Event>,
    process_handler: ProcessHandler,
    mouse_handler: MouseHandler,
    workspace_observer: Retained<WorkspaceObserver>,
    mission_control_observer: MissionControlHandler,
    display_handler: DisplayHandler,
}

impl PlatformCallbacks {
    pub fn new(tx: Sender<Event>) -> std::pin::Pin<Box<Self>> {
        let workspace_observer = WorkspaceObserver::new(tx.clone());
        Box::pin(PlatformCallbacks {
            process_handler: ProcessHandler::new(tx.clone(), workspace_observer.clone()),
            mouse_handler: MouseHandler::new(tx.clone()),
            workspace_observer,
            mission_control_observer: MissionControlHandler::new(tx.clone()),
            display_handler: DisplayHandler::new(tx.clone()),
            tx,
        })
    }

    pub fn setup_handlers(&mut self) -> Result<()> {
        self.mouse_handler.start()?;
        self.display_handler.start()?;
        self.mission_control_observer.observe()?;
        self.workspace_observer.start();
        self.process_handler.start();

        self.tx.send(Event::ProcessesLoaded).map_err(|err| {
            Error::new(
                ErrorKind::InvalidData,
                format!("{}: {err}", function_name!()),
            )
        })
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
        _ = self.tx.send(Event::Exit);
        info!("{}: Run loop finished.", function_name!());
    }
}

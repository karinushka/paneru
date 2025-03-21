use accessibility_sys::{
    AXError, AXObserverGetRunLoopSource, AXObserverRef, AXUIElementCreateApplication,
    AXUIElementRef, kAXErrorNotificationAlreadyRegistered, kAXErrorSuccess,
};
use core::ptr::NonNull;
use log::{debug, error, info, warn};
use objc2::rc::{Retained, autoreleasepool};
use objc2::{AllocAnyThread, DefinedClass, define_class, msg_send, sel};
use objc2_app_kit::{NSRunningApplication, NSWorkspace, NSWorkspaceApplicationKey};
use objc2_core_foundation::{
    CFMachPortCreateRunLoopSource, CFMachPortInvalidate, CFRetained, CFRunLoopAddSource,
    CFRunLoopGetMain, CFRunLoopRemoveSource, CFRunLoopRunInMode, CFRunLoopSource,
    CFRunLoopSourceInvalidate, CFString, kCFRunLoopCommonModes, kCFRunLoopDefaultMode,
};
use objc2_core_graphics::{
    CGEvent, CGEventGetLocation, CGEventTapCreate, CGEventTapEnable, CGEventTapLocation,
    CGEventTapOptions, CGEventTapPlacement, CGEventTapProxy, CGEventType,
};
use objc2_foundation::{NSDistributedNotificationCenter, NSNotificationCenter, NSObject, NSString};
use std::ffi::c_void;
use std::ops::Deref;
use std::ptr::null_mut;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::mpsc::Sender;
use stdext::function_name;

use crate::events::Event;
use crate::skylight::OSStatus;
use crate::util::{AxuWrapperType, Cleanuper};

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
    fn new(tx: Sender<Event>) -> Self {
        ProcessHandler { tx, cleanup: None }
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
        let mut this = NonNull::new(this)
            .expect("Zero passed to Process Handler.")
            .cast::<ProcessHandler>();
        let this = unsafe { this.as_mut() };
        this.process_handler(psn, event);
        0
    }

    fn process_handler(&mut self, psn: &ProcessSerialNumber, event: ProcessEventApp) {
        let psn = psn.clone();
        let err_message = "process handler: error sending event";
        match event {
            ProcessEventApp::Launched => self
                .tx
                .send(Event::ApplicationLaunched { psn })
                .expect(err_message),
            ProcessEventApp::Terminated => self
                .tx
                .send(Event::ApplicationTerminated { psn })
                .expect(err_message),
            ProcessEventApp::FrontSwitched => self
                .tx
                .send(Event::ApplicationFrontSwitched { psn })
                .expect(err_message),
            _ => {
                error!(
                    "{}: Unknown process event: {}",
                    function_name!(),
                    event as u32
                );
            }
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

    fn start(&mut self) {
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
            .expect("Unable to create EventTap.");

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
    }

    extern "C-unwind" fn callback(
        _: CGEventTapProxy,
        event_type: CGEventType,
        mut event_ref: NonNull<CGEvent>,
        this: *mut c_void,
    ) -> *mut CGEvent {
        unsafe {
            let this = (this as *mut Self)
                .as_mut()
                .expect("Zero passed to Mouse Handler.");
            this.mouse_handler(event_type, event_ref.as_ref());
            event_ref.as_mut()
        }
    }

    fn mouse_handler(&mut self, event_type: CGEventType, event: &CGEvent) {
        match event_type {
            CGEventType::TapDisabledByTimeout | CGEventType::TapDisabledByUserInput => {
                warn!("{}: Tap Disabled", function_name!());
            }
            CGEventType::LeftMouseDown | CGEventType::RightMouseDown => {
                let point = unsafe { CGEventGetLocation(Some(event)) };
                self.tx
                    .send(Event::MouseDown { point })
                    .expect("mouse handler: error sending event");
            }
            CGEventType::LeftMouseUp | CGEventType::RightMouseUp => {
                let point = unsafe { CGEventGetLocation(Some(event)) };
                self.tx
                    .send(Event::MouseUp { point })
                    .expect("mouse handler: error sending event");
            }
            CGEventType::LeftMouseDragged | CGEventType::RightMouseDragged => {
                let point = unsafe { CGEventGetLocation(Some(event)) };
                self.tx
                    .send(Event::MouseDragged { point })
                    .expect("mouse handler: error sending event");
            }
            CGEventType::MouseMoved => {
                let point = unsafe { CGEventGetLocation(Some(event)) };
                self.tx
                    .send(Event::MouseMoved { point })
                    .expect("mouse handler: error sending event");
            }
            _ => {
                info!(
                    "{}: Unknown event type received: {event_type:?}",
                    function_name!()
                );
            }
        }
    }
}

#[derive(Debug, Clone)]
struct Ivars {
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
    struct WorkspaceObserver;

    impl WorkspaceObserver {
        #[unsafe(method(activeDisplayDidChange:))]
        fn display_changed(&self, notification: &NSObject) {
            let msg = Event::DisplayChanged{
                msg: format!("WorkspaceObserver: {:?}", notification),
            };
            self.ivars().tx.send(msg).expect("display_changed: Error sending event!");
        }

        #[unsafe(method(activeSpaceDidChange:))]
        fn space_changed(&self, notification: &NSObject) {
            let msg = Event::SpaceChanged{
                msg: format!("WorkspaceObserver: {:?}", notification),
            };
            self.ivars().tx.send(msg).expect("space_changed: Error sending event!");
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
            self.ivars().tx.send(msg).expect("application_hidden: Error sending event!");
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
            self.ivars().tx.send(msg).expect("application_unhidden: Error sending event!");
        }

        #[unsafe(method(didWake:))]
        fn system_woke(&self, notification: &NSObject) {
            let msg = Event::SystemWoke{
                msg: format!("WorkspaceObserver: {:?}", notification),
            };
            self.ivars().tx.send(msg).expect("system_woke: Error sending event!");
        }

        #[unsafe(method(didChangeMenuBarHiding:))]
        fn menubar_hidden(&self, notification: &NSObject) {
            let msg = Event::MenuBarHiddenChanged{
                msg: format!("WorkspaceObserver: {:?}", notification),
            };
            self.ivars().tx.send(msg).expect("menubar_hidden: Error sending event!");
        }

        #[unsafe(method(didRestartDock:))]
        fn dock_restarted(&self, notification: &NSObject) {
            let msg = Event::DockDidRestart{
                msg: format!("WorkspaceObserver: {:?}", notification),
            };
            self.ivars().tx.send(msg).expect("dock_restarted: Error sending event!");
        }

        #[unsafe(method(didChangeDockPref:))]
        fn dock_pref_changed(&self, notification: &NSObject) {
            let msg = Event::DockDidChangePref{
                msg: format!("WorkspaceObserver: {:?}", notification),
            };
            self.ivars().tx.send(msg).expect("dock_pref_changed: Error sending event!");
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
        _ = self.tx.send(event);
    }

    fn dock_pid() -> Option<Pid> {
        let dock = NSString::from_str("com.apple.dock");
        let array = unsafe { NSRunningApplication::runningApplicationsWithBundleIdentifier(&dock) };
        array
            .iter()
            .next()
            .map(|running| unsafe { running.processIdentifier() })
    }

    fn observe(&mut self) {
        let pid = MissionControlHandler::dock_pid();
        if pid.is_none() {
            error!(
                "{}: Can not register MissionControlHandler",
                function_name!()
            );
            return;
        }
        let pid = pid.unwrap();

        self.element = AxuWrapperType::from_retained(unsafe { AXUIElementCreateApplication(pid) });
        self.observer = unsafe {
            let mut observer_ref: AXObserverRef = null_mut();
            (kAXErrorSuccess == AXObserverCreate(pid, Self::callback, &mut observer_ref))
                .then(|| AxuWrapperType::from_retained(observer_ref as AXUIElementRef))
                .flatten()
        };

        if let Some(observer) = &self.observer {
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
            unsafe {
                let main_loop = CFRunLoopGetMain().expect("Unable to get the main run loop.");
                let run_loop_source = CFRetained::from_raw(
                    NonNull::new(AXObserverGetRunLoopSource(observer.as_ptr()))
                        .expect("Can not get AXObserver run loop source.")
                        .cast(),
                );
                debug!(
                    "{}: adding runloop source: {run_loop_source:?} {:?}",
                    function_name!(),
                    observer.as_ptr::<AXObserverRef>()
                );
                CFRunLoopAddSource(&main_loop, Some(&run_loop_source), kCFRunLoopDefaultMode);
            };
        }
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
        let result = unsafe {
            (context as *mut Self)
                .as_ref()
                .map(|this| this.mission_control_handler(observer, element, notification.as_ref()))
        };
        if result.is_none() {
            error!("{}: nullptr passed as self.", function_name!());
        }
    }
}

impl Drop for MissionControlHandler {
    fn drop(&mut self) {
        self.unobserve();
    }
}

pub struct PlatformCallbacks {
    tx: Sender<Event>,
    process_handler: ProcessHandler,
    mouse_handler: MouseHandler,
    workspace_observer: Retained<WorkspaceObserver>,
    mission_control_observer: MissionControlHandler,
}

impl PlatformCallbacks {
    pub fn new(tx: Sender<Event>) -> std::pin::Pin<Box<Self>> {
        Box::pin(PlatformCallbacks {
            process_handler: ProcessHandler::new(tx.clone()),
            mouse_handler: MouseHandler::new(tx.clone()),
            workspace_observer: WorkspaceObserver::new(tx.clone()),
            mission_control_observer: MissionControlHandler::new(tx.clone()),
            tx,
        })
    }

    pub fn setup_handlers(&mut self) {
        self.mouse_handler.start();
        self.workspace_observer.start();
        self.mission_control_observer.observe();
        self.process_handler.start();
        self.tx.send(Event::ProcessesLoaded).unwrap();
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

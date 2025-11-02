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
    NSApplication, NSApplicationActivationPolicy, NSEvent, NSEventType, NSRunningApplication,
    NSTouch, NSTouchPhase, NSWorkspace, NSWorkspaceApplicationKey,
};
use objc2_core_foundation::{
    CFMachPort, CFRetained, CFRunLoop, CFString, kCFRunLoopCommonModes, kCFRunLoopDefaultMode,
};
use objc2_core_graphics::{
    CGDirectDisplayID, CGDisplayChangeSummaryFlags, CGDisplayRegisterReconfigurationCallback,
    CGDisplayRemoveReconfigurationCallback, CGError, CGEvent, CGEventField, CGEventFlags,
    CGEventTapLocation, CGEventTapOptions, CGEventTapPlacement, CGEventTapProxy, CGEventType,
};
use objc2_foundation::{
    NSDictionary, NSDistributedNotificationCenter, NSKeyValueChangeNewKey, NSNotification,
    NSNotificationCenter, NSNumber, NSObject, NSSet, NSString,
};
use std::env;
use std::ffi::c_void;
use std::io::{Error, ErrorKind, Result};
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
use crate::util::{AXUIWrapper, Cleanuper, add_run_loop, remove_run_loop};

pub type Pid = i32;
pub type CFStringRef = *const CFString;

type AXObserverCallback = unsafe extern "C" fn(
    observer: AXObserverRef,
    element: AXUIElementRef,
    notification: CFStringRef,
    refcon: *mut c_void,
);

unsafe extern "C" {
    /// Creates an `AXObserver` for a given application process ID and a callback function.
    ///
    /// # Arguments
    ///
    /// * `application` - The process ID (Pid) of the application to observe.
    /// * `callback` - The `AXObserverCallback` function to be invoked when notifications occur.
    /// * `out_observer` - A mutable reference to an `AXObserverRef` where the created observer will be stored.
    ///
    /// # Returns
    ///
    /// An `AXError` indicating success or failure.
    pub fn AXObserverCreate(
        application: Pid,
        callback: AXObserverCallback,
        out_observer: &mut AXObserverRef,
    ) -> AXError;

    /// Adds a notification to an `AXObserver` for a specific UI element.
    ///
    /// # Arguments
    ///
    /// * `observer` - The `AXObserverRef`.
    /// * `element` - The `AXUIElementRef` to observe.
    /// * `notification` - A reference to a `CFString` representing the notification name.
    /// * `refcon` - A raw pointer to user-defined context data.
    ///
    /// # Returns
    ///
    /// An `AXError` indicating success or failure, including `kAXErrorNotificationAlreadyRegistered`.
    pub fn AXObserverAddNotification(
        observer: AXObserverRef,
        element: AXUIElementRef,
        notification: &CFString,
        refcon: *mut c_void,
    ) -> AXError;

    /// Removes a notification from an `AXObserver` for a specific UI element.
    ///
    /// # Arguments
    ///
    /// * `observer` - The `AXObserverRef`.
    /// * `element` - The `AXUIElementRef` from which to remove the notification.
    /// * `notification` - A reference to a `CFString` representing the notification name.
    ///
    /// # Returns
    ///
    /// An `AXError` indicating success or failure.
    pub fn AXObserverRemoveNotification(
        observer: AXObserverRef,
        element: AXUIElementRef,
        notification: &CFString,
    ) -> AXError;
}

#[derive(Clone, Copy, Debug, Default, Hash, PartialEq, Eq)]
#[repr(C)]
pub struct ProcessSerialNumber {
    pub high: u32,
    pub low: u32,
}

type ProcessCallbackFn = extern "C-unwind" fn(
    this: *mut c_void,
    event: *const ProcessEvent,
    context: *const c_void,
) -> OSStatus;

unsafe extern "C" {
    /// Retrieves the application event target.
    ///
    /// # Returns
    ///
    /// A raw pointer to an `EventTargetRef` for the application.
    ///
    /// # Original signature
    /// extern `EventTargetRef` GetApplicationEventTarget(void)
    fn GetApplicationEventTarget() -> *const ProcessEventTarget;

    /// Installs an event handler for a specific event target and event types.
    ///
    /// # Arguments
    ///
    /// * `target` - The `EventTargetRef` to install the handler on.
    /// * `handler` - The `ProcessCallbackFn` to be called when events match.
    /// * `event_len` - The number of event types in `events`.
    /// * `events` - A raw pointer to an array of `EventTypeSpec` defining the events to handle.
    /// * `user_data` - A raw pointer to user-defined data to pass to the handler.
    /// * `handler_ref` - A mutable raw pointer to an `EventHandlerRef` where the installed handler
    ///   reference will be stored.
    ///
    /// # Returns
    ///
    /// An `OSStatus` indicating success or failure.
    ///
    /// # Original signature
    /// extern `OSStatus`
    /// `InstallEventHandler`(
    ///   `EventTargetRef`         inTarget,
    ///   `EventHandlerUPP`        inHandler,
    ///   `ItemCount`              inNumTypes,
    ///   const `EventTypeSpec` *  inList,
    ///   void *                 inUserData,
    ///   `EventHandlerRef` *      outRef)
    fn InstallEventHandler(
        target: *const ProcessEventTarget,
        handler: ProcessCallbackFn,
        event_len: u32,
        events: *const EventTypeSpec,
        user_data: *const c_void,
        handler_ref: *mut *const ProcessEventHandler,
    ) -> OSStatus;

    /// Removes a previously installed event handler.
    ///
    /// # Arguments
    ///
    /// * `handler_ref` - A raw pointer to the `EventHandlerRef` to remove.
    ///
    /// # Returns
    ///
    /// An `OSStatus` indicating success or failure.
    ///
    /// # Original signature
    fn RemoveEventHandler(handler_ref: *const ProcessEventHandler) -> OSStatus;

    /// Gets a piece of data from the given event, if it exists.
    ///
    /// # Discussion
    /// The Carbon Event Manager will automatically use `AppleEvent` coercion handlers to convert
    /// the data in the event into the desired type, if possible. You may also pass `typeWildCard`
    /// to request that the data be returned in its original format.
    ///
    /// # Mac OS X threading
    /// Not thread safe.
    ///
    /// # Arguments
    ///
    /// * `inEvent` - The event to get the parameter from.
    /// * `inName` - The symbolic name of the parameter.
    /// * `inDesiredType` - The desired type of the parameter.
    /// * `outActualType` - The actual type of the parameter, or `NULL`.
    /// * `inBufferSize` - The size of the output buffer specified by `outData`. Pass zero and
    ///   `NULL` for `outData` if data is not desired. * `outActualSize` - The actual size of the
    ///   data, or `NULL`.
    /// * `outData` - The pointer to the buffer which will receive the parameter data, or `NULL`.
    ///
    /// # Returns
    ///
    /// An operating system result code (`OSStatus`).
    ///
    /// # Original signature
    /// extern `OSStatus`
    /// `GetEventParameter`(
    ///   `EventRef`          inEvent,
    ///   `EventParamName`    inName,
    ///   `EventParamType`    inDesiredType,
    ///   `EventParamType` *  outActualType,       /* can be NULL */
    ///   `ByteCount`         inBufferSize,
    ///   `ByteCount` *       outActualSize,       /* can be NULL */
    ///   void *            outData)             /* can be NULL */
    fn GetEventParameter(
        event: *const ProcessEvent,
        param_name: u32,
        param_type: u32,
        actual_type: *mut u32,
        size: u32,
        actual_size: *mut u32,
        data: *mut c_void,
    ) -> OSStatus;

    /// Returns the kind of the given event (e.g., mousedown).
    ///
    /// # Discussion
    /// Event kinds overlap between event classes (e.g., `kEventMouseDown` and `kEventAppActivated`
    /// have the same value). The combination of class and kind determines an event signature.
    ///
    /// # Mac OS X threading
    /// Thread safe.
    ///
    /// # Arguments
    ///
    /// * `inEvent` - The event in question.
    ///
    /// # Returns
    ///
    /// The kind of the event (`UInt32`).
    ///
    /// # Original signature
    /// extern `UInt32` GetEventKind(EventRef inEvent)
    fn GetEventKind(event: *const ProcessEvent) -> u32;

    /// Retrieves the next available process's serial number.
    ///
    /// # Arguments
    ///
    /// * `psn` - A mutable pointer to a `ProcessSerialNumber` structure. On the first call, pass a
    ///   PSN with `kNoProcess` for `highLongOfPSN` and `lowLongOfPSN`. On subsequent calls, pass
    ///   the PSN returned by the previous call.
    ///
    /// # Returns
    ///
    /// An `OSStatus` code. `noErr` (0) if a process was found, otherwise an error code.
    ///
    /// # Original signature
    /// GetNextProcess(ProcessSerialNumber * pPSN)
    fn GetNextProcess(psn: *mut ProcessSerialNumber) -> OSStatus;
}

/*
 *  EventTypeSpec
 *
 *  Discussion:
 *    This structure is used to specify an event. Typically, a static
 *    array of EventTypeSpecs are passed into functions such as
 *    InstallEventHandler, as well as routines such as
 *    FlushEventsMatchingListFromQueue.
 */
// struct EventTypeSpec {
//   OSType              eventClass;
//   UInt32              eventKind;
// };
#[repr(C)]
struct EventTypeSpec {
    event_class: u32,
    event_kind: u32,
}

#[repr(C)]
struct ProcessEventHandler {
    _opaque: [u8; 0],
}

#[repr(C)]
struct ProcessEventTarget {
    _opaque: [u8; 0],
}

#[repr(C)]
struct ProcessEvent {
    _opaque: [u8; 0],
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
    /// Creates a new `ProcessHandler` instance.
    ///
    /// # Arguments
    ///
    /// * `events` - An `EventSender` to send process-related events.
    /// * `observer` - A `Retained<WorkspaceObserver>` to pass to `ApplicationLaunched` events.
    ///
    /// # Returns
    ///
    /// A new `ProcessHandler`.
    fn new(events: EventSender, observer: Retained<WorkspaceObserver>) -> Self {
        ProcessHandler {
            events,
            cleanup: None,
            observer,
        }
    }

    /// Starts the process handler by registering a C-callback with the underlying private API.
    /// It also sends initial `ApplicationLaunched` events for already running processes.
    fn start(&mut self) {
        const APPL_CLASS: &str = "appl";
        const PROCESS_EVENT_LAUNCHED: u32 = 5;
        const PROCESS_EVENT_TERMINATED: u32 = 6;
        const PROCESS_EVENT_FRONTSWITCHED: u32 = 7;

        info!("{}: Registering process_handler", function_name!());

        // Fake launch the existing processes.
        let mut psn = ProcessSerialNumber::default();
        while 0 == unsafe { GetNextProcess(&raw mut psn) } {
            self.process_handler(psn, ProcessEventApp::Launched);
        }

        let target = unsafe { GetApplicationEventTarget() };
        let event_class = u32::from_be_bytes(APPL_CLASS.as_bytes().try_into().unwrap());
        let events = [
            EventTypeSpec {
                event_class,
                event_kind: PROCESS_EVENT_LAUNCHED,
            },
            EventTypeSpec {
                event_class,
                event_kind: PROCESS_EVENT_TERMINATED,
            },
            EventTypeSpec {
                event_class,
                event_kind: PROCESS_EVENT_FRONTSWITCHED,
            },
        ];
        let mut handler: *const ProcessEventHandler = std::ptr::null();
        let result = unsafe {
            InstallEventHandler(
                target,
                Self::callback,
                3,
                events.as_ptr(),
                NonNull::new_unchecked(self).as_ptr().cast(),
                &raw mut handler,
            )
        };
        debug!(
            "{}: Registered process_handler (result = {result}): {handler:x?}",
            function_name!()
        );

        self.cleanup = Some(Cleanuper::new(Box::new(move || unsafe {
            info!(
                "{}: Unregistering process_handler: {handler:?}",
                function_name!()
            );
            RemoveEventHandler(handler);
        })));
    }

    /// The C-callback function invoked by the private process handling API. It dispatches to the `process_handler` method.
    /// This function is declared as `extern "C-unwind"`.
    ///
    /// # Arguments
    ///
    /// * `_` - Unused callback info parameter.
    /// * `event` - A raw pointer to the `ProcessEvent`.
    /// * `this` - A raw pointer to the `ProcessHandler` instance.
    ///
    /// # Returns
    ///
    /// An `OSStatus`.
    extern "C-unwind" fn callback(
        _: *mut c_void,
        event: *const ProcessEvent,
        this: *const c_void,
    ) -> OSStatus {
        match NonNull::new(this.cast_mut())
            .map(|this| unsafe { this.cast::<ProcessHandler>().as_mut() })
        {
            Some(this) => {
                const PARAM: &str = "psn "; // kEventParamProcessID and typeProcessSerialNumber
                let param_name = u32::from_be_bytes(PARAM.as_bytes().try_into().unwrap());
                let param_type = param_name; // Uses the same FourCharCode as param_name

                let mut psn = ProcessSerialNumber::default();

                let res = unsafe {
                    GetEventParameter(
                        event,
                        param_name,
                        param_type,
                        std::ptr::null_mut(),
                        std::mem::size_of::<ProcessSerialNumber>()
                            .try_into()
                            .unwrap(),
                        std::ptr::null_mut(),
                        NonNull::from(&mut psn).as_ptr().cast(),
                    )
                };
                if res == 0 {
                    let decoded: ProcessEventApp =
                        unsafe { std::mem::transmute(GetEventKind(event)) };
                    this.process_handler(psn, decoded);
                }
            }
            _ => error!("Zero passed to Process Handler."),
        }
        0
    }

    /// Handles various process events received from the C callback. It sends corresponding `Event`s via `events`.
    ///
    /// # Arguments
    ///
    /// * `psn` - A reference to the `ProcessSerialNumber` of the process involved in the event.
    /// * `event` - The `ProcessEventApp` indicating the type of event.
    fn process_handler(&mut self, psn: ProcessSerialNumber, event: ProcessEventApp) {
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
    finger_position: Option<Retained<NSSet<NSTouch>>>,
    tap_port: Option<CFRetained<CFMachPort>>,
}

impl InputHandler {
    /// Creates a new `InputHandler` instance.
    ///
    /// # Arguments
    ///
    /// * `events` - An `EventSender` to send input-related events.
    /// * `config` - The `Config` object for looking up keybindings.
    ///
    /// # Returns
    ///
    /// A new `InputHandler`.
    fn new(events: EventSender, config: Config) -> Self {
        InputHandler {
            events,
            config,
            cleanup: None,
            finger_position: None,
            tap_port: None,
        }
    }

    /// Starts the input handler by creating and enabling a `CGEventTap`. It also sets up a cleanup hook.
    ///
    /// # Returns
    ///
    /// `Ok(())` if the event tap is created and started successfully, otherwise `Err(Error)`.
    fn start(&mut self) -> Result<()> {
        let mouse_event_mask = (1 << CGEventType::MouseMoved.0)
            | (1 << CGEventType::LeftMouseDown.0)
            | (1 << CGEventType::LeftMouseUp.0)
            | (1 << CGEventType::LeftMouseDragged.0)
            | (1 << CGEventType::RightMouseDown.0)
            | (1 << CGEventType::RightMouseUp.0)
            | (1 << CGEventType::RightMouseDragged.0)
            | (1 << NSEventType::Gesture.0)
            | (1 << CGEventType::KeyDown.0);

        unsafe {
            let this = NonNull::new_unchecked(self).as_ptr();
            self.tap_port = CGEvent::tap_create(
                CGEventTapLocation::HIDEventTap,
                CGEventTapPlacement::HeadInsertEventTap,
                CGEventTapOptions::Default,
                mouse_event_mask,
                Some(Self::callback),
                this.cast(),
            );
            if self.tap_port.is_none() {
                return Err(Error::new(
                    ErrorKind::PermissionDenied,
                    format!("{}: Can not create EventTap.", function_name!()),
                ));
            }

            let (run_loop_source, main_loop) =
                CFMachPort::new_run_loop_source(None, self.tap_port.as_deref(), 0)
                    .zip(CFRunLoop::main())
                    .ok_or(Error::new(
                        ErrorKind::PermissionDenied,
                        format!("{}: Unable to create run loop source", function_name!()),
                    ))?;
            CFRunLoop::add_source(&main_loop, Some(&run_loop_source), kCFRunLoopCommonModes);

            let port = self.tap_port.clone().unwrap();
            self.cleanup = Some(Cleanuper::new(Box::new(move || {
                info!("{}: Unregistering event_handler", function_name!());
                CFRunLoop::remove_source(&main_loop, Some(&run_loop_source), kCFRunLoopCommonModes);
                CFMachPort::invalidate(&port);
                CGEvent::tap_enable(&port, false);
            })));
        }
        Ok(())
    }

    /// The C-callback function for the `CGEventTap`. It dispatches to the `input_handler` method.
    /// This function is declared as `extern "C-unwind"`.
    ///
    /// # Arguments
    ///
    /// * `_` - The `CGEventTapProxy` (unused).
    /// * `event_type` - The `CGEventType` of the event.
    /// * `event_ref` - A mutable `NonNull` pointer to the `CGEvent`.
    /// * `this` - A raw pointer to the `InputHandler` instance.
    ///
    /// # Returns
    ///
    /// A raw mutable pointer to `CGEvent`. Returns `null_mut()` if the event is intercepted.
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
            _ => error!("Zero passed to Event Handler."),
        }
        unsafe { event_ref.as_mut() }
    }

    /// Handles various input events received from the `CGEventTap` callback. It sends corresponding `Event`s.
    ///
    /// # Arguments
    ///
    /// * `event_type` - The `CGEventType` of the event.
    /// * `event` - A reference to the `CGEvent`.
    ///
    /// # Returns
    ///
    /// `true` if the event should be intercepted (not passed further), `false` otherwise.
    fn input_handler(&mut self, event_type: CGEventType, event: &CGEvent) -> bool {
        let result = match event_type {
            CGEventType::TapDisabledByTimeout | CGEventType::TapDisabledByUserInput => {
                info!("{}: Tap Disabled", function_name!());
                if let Some(port) = &self.tap_port {
                    CGEvent::tap_enable(port, true);
                }
                Ok(())
            }
            CGEventType::LeftMouseDown | CGEventType::RightMouseDown => {
                let point = CGEvent::location(Some(event));
                self.events.send(Event::MouseDown { point })
            }
            CGEventType::LeftMouseUp | CGEventType::RightMouseUp => {
                let point = CGEvent::location(Some(event));
                self.events.send(Event::MouseUp { point })
            }
            CGEventType::LeftMouseDragged | CGEventType::RightMouseDragged => {
                let point = CGEvent::location(Some(event));
                self.events.send(Event::MouseDragged { point })
            }
            CGEventType::MouseMoved => {
                let point = CGEvent::location(Some(event));
                self.events.send(Event::MouseMoved { point })
            }
            CGEventType::KeyDown => {
                let keycode =
                    CGEvent::integer_value_field(Some(event), CGEventField::KeyboardEventKeycode);
                let eventflags = CGEvent::flags(Some(event));
                // handle_keypress can intercept the event, so it may return true.
                return self.handle_keypress(keycode, eventflags);
            }
            _ => self.handle_swipe(event),
        };
        if let Err(err) = result {
            error!("{}: error sending event: {err}", function_name!());
        }
        // Do not intercept this event, let it fall through.
        false
    }

    /// Handles swipe gesture events.
    /// It calculates the delta of the swipe and sends a `Swipe` event.
    ///
    /// # Arguments
    ///
    /// * `event` - A reference to the `CGEvent`.
    ///
    /// # Returns
    ///
    /// `Ok(())` if the event is processed successfully, otherwise `Err(Error)`.
    fn handle_swipe(&mut self, event: &CGEvent) -> Result<()> {
        const GESTURE_MINIMAL_FINGERS: usize = 3;
        let Some(ns_event) = NSEvent::eventWithCGEvent(event) else {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                format!(
                    "{}: Unable to convert {event:?} to NSEvent.",
                    function_name!()
                ),
            ));
        };
        if ns_event.r#type() != NSEventType::Gesture {
            return Ok(());
        }
        let fingers = ns_event.allTouches();
        if fingers.len() < GESTURE_MINIMAL_FINGERS {
            return Ok(());
        }

        if fingers.iter().all(|f| f.phase() != NSTouchPhase::Began)
            && let Some(prev) = &self.finger_position
        {
            let deltas = prev
                .iter()
                .zip(&fingers)
                .map(|(p, c)| p.normalizedPosition().x - c.normalizedPosition().x)
                .collect::<Vec<_>>();
            _ = self.events.send(Event::Swipe { deltas });
        }
        self.finger_position = Some(fingers);
        Ok(())
    }

    /// Handles key press events. It determines the modifier mask and attempts to find a matching keybinding in the configuration.
    /// If a binding is found, it sends a `Command` event and intercepts the key press.
    ///
    /// # Arguments
    ///
    /// * `keycode` - The key code of the pressed key.
    /// * `eventflags` - The `CGEventFlags` representing active modifiers.
    ///
    /// # Returns
    ///
    /// `true` if the key press was handled and should be intercepted, `false` otherwise.
    fn handle_keypress(&self, keycode: i64, eventflags: CGEventFlags) -> bool {
        const MODIFIER_MASKS: [[u64; 3]; 4] = [
            // Normal key, left, right.
            [0x0008_0000, 0x0000_0020, 0x0000_0040], // Alt
            [0x0002_0000, 0x0000_0002, 0x0000_0004], // Shift
            [0x0010_0000, 0x0000_0008, 0x0000_0010], // Command
            [0x0004_0000, 0x0000_0001, 0x0000_2000], // Control
        ];
        let mask = MODIFIER_MASKS
            .iter()
            .enumerate()
            .filter_map(|(bitshift, modifier)| {
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
                .split('_')
                .map(std::string::ToString::to_string)
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

        /// Called when an application is unhidden.
        ///
        /// # Arguments
        ///
        /// * `notification` - The notification object containing application info.
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
    fn new(events: EventSender) -> Retained<Self> {
        // Initialize instance variables.
        let this = Self::alloc().set_ivars(Ivars { events });
        // Call `NSObject`'s `init` method.
        unsafe { msg_send![super(this), init] }
    }

    /// Starts observing workspace notifications by registering selectors with `NSWorkspace` and `NSDistributedNotificationCenter`.
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

#[derive(Debug)]
struct MissionControlHandler {
    events: EventSender,
    element: Option<CFRetained<AXUIWrapper>>,
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

    /// Retrieves the process ID (Pid) of the Dock application.
    ///
    /// # Returns
    ///
    /// `Ok(Pid)` with the Dock's process ID if found, otherwise `Err(Error)`.
    fn dock_pid() -> Result<Pid> {
        let dock = NSString::from_str("com.apple.dock");
        let array = NSRunningApplication::runningApplicationsWithBundleIdentifier(&dock);
        array
            .iter()
            .next()
            .map(|running| running.processIdentifier())
            .ok_or(Error::new(
                ErrorKind::NotFound,
                format!("{}: can not find dock.", function_name!()),
            ))
    }

    /// Starts observing Mission Control accessibility notifications from the Dock process.
    /// It creates an `AXObserver` and adds it to the run loop.
    ///
    /// # Returns
    ///
    /// `Ok(())` if observation is started successfully, otherwise `Err(Error)`.
    fn observe(&mut self) -> Result<()> {
        let pid = MissionControlHandler::dock_pid()?;
        let element = AXUIWrapper::from_retained(unsafe { AXUIElementCreateApplication(pid) })?;
        let observer = unsafe {
            let mut observer_ref: AXObserverRef = null_mut();
            if kAXErrorSuccess == AXObserverCreate(pid, Self::callback, &mut observer_ref) {
                AXUIWrapper::from_retained(observer_ref)?
            } else {
                return Err(Error::new(
                    ErrorKind::PermissionDenied,
                    format!("{}: error creating observer.", function_name!()),
                ));
            }
        };

        for name in &Self::EVENTS {
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
                    &notification,
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
        }
        unsafe { add_run_loop(&observer, kCFRunLoopDefaultMode)? };
        self.observer = observer.into();
        self.element = element.into();
        Ok(())
    }

    /// Stops observing Mission Control accessibility notifications and cleans up resources.
    fn unobserve(&mut self) {
        if let Some((observer, element)) = self.observer.take().zip(self.element.as_ref()) {
            for name in &Self::EVENTS {
                debug!(
                    "{}: {name:?} {:?}",
                    function_name!(),
                    observer.as_ptr::<AXObserverRef>()
                );
                let notification = CFString::from_static_str(name);
                let result = unsafe {
                    AXObserverRemoveNotification(observer.as_ptr(), element.as_ptr(), &notification)
                };
                if result != kAXErrorSuccess && result != kAXErrorNotificationAlreadyRegistered {
                    error!("{}: error unregistering {name}: {result}", function_name!());
                }
            }
            remove_run_loop(&observer);
            drop(observer);
        } else {
            warn!(
                "{}: unobserving without observe or element",
                function_name!()
            );
        }
    }

    /// The static callback function for the Mission Control `AXObserver`. It dispatches to the `mission_control_handler` method.
    ///
    /// # Arguments
    ///
    /// * `observer` - The `AXObserverRef`.
    /// * `element` - The `AXUIElementRef`.
    /// * `notification` - The raw `CFStringRef` representing the notification name.
    /// * `context` - A raw pointer to the `MissionControlHandler` instance.
    extern "C" fn callback(
        observer: AXObserverRef,
        element: AXUIElementRef,
        notification: CFStringRef,
        context: *mut c_void,
    ) {
        let Some(notification) = NonNull::new(notification.cast_mut()) else {
            error!("{}: nullptr 'notification' passed.", function_name!());
            return;
        };

        match NonNull::new(context)
            .map(|this| unsafe { this.cast::<MissionControlHandler>().as_ref() })
        {
            Some(this) => {
                let notification = unsafe { notification.as_ref() }.to_string();
                this.mission_control_handler(observer, element, &notification);
            }
            _ => error!("Zero passed to MissionControlHandler."),
        }
    }
}

impl Drop for MissionControlHandler {
    /// Unobserves Mission Control notifications when the `MissionControlHandler` is dropped.
    fn drop(&mut self) {
        self.unobserve();
    }
}

struct DisplayHandler {
    events: EventSender,
    cleanup: Option<Cleanuper>,
}

impl DisplayHandler {
    /// Creates a new `DisplayHandler` instance.
    ///
    /// # Arguments
    ///
    /// * `events` - An `EventSender` to send display-related events.
    ///
    /// # Returns
    ///
    /// A new `DisplayHandler`.
    fn new(events: EventSender) -> Self {
        Self {
            events,
            cleanup: None,
        }
    }

    /// Starts the display handler by registering a callback for display reconfiguration events.
    ///
    /// # Returns
    ///
    /// `Ok(())` if the callback is registered successfully, otherwise `Err(Error)`.
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

    /// The C-callback function for `CGDisplayReconfigurationCallback`. It dispatches to the `display_handler` method.
    /// This function is declared as `extern "C-unwind"`.
    ///
    /// # Arguments
    ///
    /// * `display_id` - The `CGDirectDisplayID` of the display that changed.
    /// * `flags` - The `CGDisplayChangeSummaryFlags` indicating the type of change.
    /// * `context` - A raw pointer to the `DisplayHandler` instance.
    extern "C-unwind" fn callback(
        display_id: CGDirectDisplayID,
        flags: CGDisplayChangeSummaryFlags,
        context: *mut c_void,
    ) {
        match NonNull::new(context).map(|this| unsafe { this.cast::<DisplayHandler>().as_mut() }) {
            Some(this) => this.display_handler(display_id, flags),
            _ => error!("Zero passed to Display Handler."),
        }
    }

    /// Handles display change events and sends corresponding `Event`s (e.g., `DisplayAdded`, `DisplayRemoved`).
    ///
    /// # Arguments
    ///
    /// * `display_id` - The `CGDirectDisplayID` of the display that changed.
    /// * `flags` - The `CGDisplayChangeSummaryFlags` indicating the type of change.
    fn display_handler(
        &mut self,
        display_id: CGDirectDisplayID,
        flags: CGDisplayChangeSummaryFlags,
    ) {
        debug!("display_handler: display change {display_id:?}");
        let event = if flags.contains(CGDisplayChangeSummaryFlags::AddFlag) {
            Event::DisplayAdded { display_id }
        } else if flags.contains(CGDisplayChangeSummaryFlags::RemoveFlag) {
            Event::DisplayRemoved { display_id }
        } else if flags.contains(CGDisplayChangeSummaryFlags::MovedFlag) {
            Event::DisplayMoved { display_id }
        } else if flags.contains(CGDisplayChangeSummaryFlags::DesktopShapeChangedFlag) {
            Event::DisplayResized { display_id }
        } else if flags.contains(CGDisplayChangeSummaryFlags::BeginConfigurationFlag) {
            Event::DisplayConfigured { display_id }
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
    /// Sends a `ConfigRefresh` event with the current configuration to the event handler.
    ///
    /// # Returns
    ///
    /// `Ok(())` if the event is sent successfully, otherwise `Err(Error)`.
    fn announce_fresh_config(&self) -> Result<()> {
        self.events.send(Event::ConfigRefresh {
            config: self.config.clone(),
        })?;
        Ok(())
    }
}

impl notify::EventHandler for ConfigHandler {
    /// Handles file system events for the configuration file. When the content changes, it reloads the configuration.
    ///
    /// # Arguments
    ///
    /// * `event` - The result of a file system event.
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
            _ = self.announce_fresh_config().inspect_err(|err| {
                error!("{}: error announcing fresh config: {err}", function_name!());
            });
        }
    }
}

/// Sets up a file system watcher for the configuration file.
///
/// # Arguments
///
/// * `events` - An `EventSender` to send `ConfigRefresh` events.
/// * `config` - The initial `Config` object.
///
/// # Returns
///
/// `Ok(FsEventWatcher)` if the watcher is set up successfully, otherwise `Err(Error)`.
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
    if let Ok(path_str) = env::var("PANERU_CONFIG") {
        let path = PathBuf::from(path_str);
        if path.exists() {
            return path;
        }
        warn!(
            "{}: $PANERU_CONFIG is set to {}, but the file does not exist. Falling back to default locations.",
            function_name!(),
            path.display()
        );
    }

    let standard_paths = [
        env::var("HOME")
            .ok()
            .map(|h| PathBuf::from(h).join(".paneru")),
        env::var("HOME")
            .ok()
            .map(|h| PathBuf::from(h).join(".paneru.toml")),
        env::var("XDG_CONFIG_HOME")
            .ok()
            .map(|x| PathBuf::from(x).join("paneru/paneru.toml")),
    ];

    standard_paths
        .into_iter()
        .flatten()
        .find(|path| path.exists())
        .unwrap_or_else(|| {
            panic!(
                "{}: Configuration file not found. Tried: $PANERU_CONFIG, $HOME/.paneru, $HOME/.paneru.toml, $XDG_CONFIG_HOME/paneru/paneru.toml",
                function_name!()
            )
        })
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
    /// Creates a new `PlatformCallbacks` instance, initializing various handlers and watchers.
    ///
    /// # Arguments
    ///
    /// * `events` - An `EventSender` to be used by all platform callbacks.
    ///
    /// # Returns
    ///
    /// `Ok(std::pin::Pin<Box<Self>>)` if the instance is created successfully, otherwise `Err(Error)`.
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

    /// Sets up and starts all platform-specific handlers, including input, display, Mission Control, workspace, and process handlers.
    /// It also sends a `ProcessesLoaded` event.
    ///
    /// # Returns
    ///
    /// `Ok(())` if all handlers are set up successfully, otherwise `Err(Error)`.
    pub fn setup_handlers(&mut self) -> Result<()> {
        // This is required to receive some Cocoa notifications into Carbon code, like
        // NSWorkspaceActiveSpaceDidChangeNotification and
        // NSWorkspaceActiveDisplayDidChangeNotification
        // Found on: https://stackoverflow.com/questions/68893386/unable-to-receive-nsworkspaceactivespacedidchangenotification-specifically-but
        if !NSApplication::load() {
            return Err(Error::new(
                ErrorKind::Unsupported,
                format!(
                    "{}: Can not startup Cocoa runloop from Carbon code.",
                    function_name!()
                ),
            ));
        }

        self.event_handler.start()?;
        self.display_handler.start()?;
        self.mission_control_observer.observe()?;
        self.workspace_observer.start();
        self.process_handler.start();

        self.events.send(Event::ProcessesLoaded)
    }

    /// Runs the main event loop for platform callbacks. It continuously processes events until the `quit` signal is set.
    ///
    /// # Arguments
    ///
    /// * `quit` - An `Arc<AtomicBool>` used to signal the run loop to exit.
    pub fn run(&mut self, quit: &Arc<AtomicBool>) {
        info!("{}: Starting run loop...", function_name!());
        loop {
            if quit.load(std::sync::atomic::Ordering::Relaxed) {
                break;
            }
            autoreleasepool(|_| unsafe {
                CFRunLoop::run_in_mode(kCFRunLoopDefaultMode, 3.0, false);
            });
        }
        _ = self.events.send(Event::Exit);
        info!("{}: Run loop finished.", function_name!());
    }
}

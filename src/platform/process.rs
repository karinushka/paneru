use core::ptr::NonNull;
use log::{debug, error, info};
use objc2::rc::Retained;
use std::ffi::c_void;
use stdext::function_name;

use super::workspace::WorkspaceObserver;
use crate::errors::{Error, Result};
use crate::events::{Event, EventSender};
use crate::platform::OSStatus;
use crate::util::Cleanuper;

/// Represents a process serial number (PSN), a unique identifier for a running process on macOS.
/// It is used by the Carbon APIs to identify applications.
#[derive(Clone, Copy, Debug, Default, Hash, PartialEq, Eq)]
#[repr(C)]
pub struct ProcessSerialNumber {
    /// The high-order 32 bits of the process serial number.
    pub high: u32,
    /// The low-order 32 bits of the process serial number.
    pub low: u32,
}

/// Type alias for the callback function signature used by `InstallEventHandler` for process events.
type ProcessCallbackFn = extern "C-unwind" fn(
    this: *mut c_void,
    event: *const ProcessEvent,
    context: *const c_void,
) -> OSStatus;

unsafe extern "C" {
    /// Retrieves the application event target.
    /// This function returns an `EventTargetRef` that represents the application's event queue.
    ///
    /// # Returns
    ///
    /// A raw pointer to an `EventTargetRef` for the application.
    ///
    /// # Original signature
    /// extern `EventTargetRef` GetApplicationEventTarget(void)
    fn GetApplicationEventTarget() -> *const ProcessEventTarget;

    /// Installs an event handler for a specific event target and event types.
    /// This function sets up a callback to be invoked when specified Carbon events occur.
    ///
    /// # Arguments
    ///
    /// * `target` - The `EventTargetRef` to install the handler on, typically obtained from `GetApplicationEventTarget`.
    /// * `handler` - The `ProcessCallbackFn` to be called when events match.
    /// * `event_len` - The number of event types in `events`.
    /// * `events` - A raw pointer to an array of `EventTypeSpec` defining the events to handle.
    /// * `user_data` - A raw pointer to user-defined data to pass to the handler's `context` parameter.
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
    /// extern `OSStatus` RemoveEventHandler(EventHandlerRef inHandlerRef)
    fn RemoveEventHandler(handler_ref: *const ProcessEventHandler) -> OSStatus;

    /// Gets a piece of data from the given event, if it exists.
    /// The Carbon Event Manager will automatically use `AppleEvent` coercion handlers to convert
    /// the data in the event into the desired type, if possible. You may also pass `typeWildCard`
    /// to request that the data be returned in its original format.
    ///
    /// # Mac OS X threading
    /// Not thread safe.
    ///
    /// # Arguments
    ///
    /// * `event` - The event to get the parameter from.
    /// * `param_name` - The symbolic name of the parameter (e.g., `kEventParamProcessID`).
    /// * `param_type` - The desired type of the parameter (e.g., `typeProcessSerialNumber`).
    /// * `actual_type` - A mutable pointer to `u32` to store the actual type of the parameter, or `NULL`.
    /// * `size` - The size of the output buffer specified by `data`. Pass zero and
    ///   `NULL` for `data` if data is not desired.
    /// * `actual_size` - A mutable pointer to `u32` to store the actual size of the
    ///   data, or `NULL`.
    /// * `data` - A mutable pointer to the buffer which will receive the parameter data, or `NULL`.
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
    /// Event kinds overlap between event classes (e.g., `kEventMouseDown` and `kEventAppActivated`
    /// have the same value). The combination of class and kind determines an event signature.
    ///
    /// # Mac OS X threading
    /// Thread safe.
    ///
    /// # Arguments
    ///
    /// * `event` - The event in question.
    ///
    /// # Returns
    ///
    /// The kind of the event (`UInt32`).
    ///
    /// # Original signature
    /// extern `UInt32` GetEventKind(EventRef inEvent)
    fn GetEventKind(event: *const ProcessEvent) -> u32;

    /// Retrieves the next available process's serial number.
    /// This function iterates through all running processes and returns their PSN.
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
/// Specifies a Carbon event by its class and kind.
/// Used for registering and matching events with event handlers.
#[repr(C)]
struct EventTypeSpec {
    /// The event class (e.g., `kEventClassApplication`).
    event_class: u32,
    /// The event kind within its class (e.g., `kEventAppLaunched`).
    event_kind: u32,
}

/// An opaque type representing a Carbon event handler reference.
#[repr(C)]
struct ProcessEventHandler {
    _opaque: [u8; 0],
}

/// An opaque type representing a Carbon event target reference.
#[repr(C)]
struct ProcessEventTarget {
    _opaque: [u8; 0],
}

/// An opaque type representing a Carbon event.
#[repr(C)]
struct ProcessEvent {
    _opaque: [u8; 0],
}

/// `ProcessHandler` is a Rust-side representation of the handler for Carbon process events.
/// It receives raw Carbon events and dispatches them as higher-level `Event`s through an `EventSender`.
#[repr(C)]
pub(super) struct ProcessHandler {
    /// The sender for dispatching processed events.
    events: EventSender,
    /// An optional `Cleanuper` to manage the unregistration of the Carbon event handler.
    cleanup: Option<Cleanuper>,
    /// A retained reference to the `WorkspaceObserver`, used for `ApplicationLaunched` events.
    observer: Retained<WorkspaceObserver>,
}

/// An enum representing different types of Carbon application-related events.
/// These correspond to specific event kinds within the `kEventClassApplication` event class.
#[repr(C)]
#[allow(dead_code)]
enum ProcessEventApp {
    /// Application activated event.
    Activated = 1,
    /// Application deactivated event.
    Deactivated = 2,
    /// Application quit event.
    Quit = 3,
    /// Application launch notification.
    LaunchNotification = 4,
    /// Application launched event.
    Launched = 5,
    /// Application terminated event.
    Terminated = 6,
    /// Frontmost application switched event.
    FrontSwitched = 7,

    /// Focus menu bar event.
    FocusMenuBar = 8,
    /// Focus next document window event.
    FocusNextDocumentWindow = 9,
    /// Focus next floating window event.
    FocusNextFloatingWindow = 10,
    /// Focus toolbar event.
    FocusToolbar = 11,
    /// Focus drawer event.
    FocusDrawer = 12,

    /// Get Dock tile menu event.
    GetDockTileMenu = 20,
    /// Update Dock tile event.
    UpdateDockTile = 21,

    /// Is event in Instant Mouser event.
    IsEventInInstantMouser = 104,

    /// Application hidden event.
    Hidden = 107,
    /// Application shown event.
    Shown = 108,
    /// System UI mode changed event.
    SystemUIModeChanged = 109,
    /// Available window bounds changed event.
    AvailableWindowBoundsChanged = 110,
    /// Active window changed event.
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
    pub(super) fn new(events: EventSender, observer: Retained<WorkspaceObserver>) -> Self {
        ProcessHandler {
            events,
            cleanup: None,
            observer,
        }
    }

    /// Starts the process handler by registering a C-callback with the underlying private API.
    /// It also sends initial `ApplicationLaunched` events for already running processes.
    ///
    /// # Side Effects
    ///
    /// - Registers a Carbon event handler, which will be unregistered when `cleanup` is dropped.
    /// - Iterates through existing processes and dispatches `ApplicationLaunched` events for them.
    pub(super) fn start(&mut self) -> Result<()> {
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
        let event_class = u32::from_be_bytes(APPL_CLASS.as_bytes().try_into()?);
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
                events.len().try_into()?,
                events.as_ptr(),
                NonNull::new_unchecked(self).as_ptr().cast(),
                &raw mut handler,
            )
        };
        if result != 0 || handler.is_null() {
            return Err(Error::PermissionDenied(format!(
                "{}: Error registering process event handler.",
                function_name!()
            )));
        }
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
        Ok(())
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
    /// * `psn` - The `ProcessSerialNumber` of the process involved in the event.
    /// * `event` - The `ProcessEventApp` indicating the type of event (e.g., `Launched`, `Terminated`).
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

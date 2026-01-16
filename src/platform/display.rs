use core::ptr::NonNull;
use log::{debug, error, info, warn};
use objc2_core_graphics::{
    CGDirectDisplayID, CGDisplayChangeSummaryFlags, CGDisplayRegisterReconfigurationCallback,
    CGDisplayRemoveReconfigurationCallback, CGError,
};
use std::ffi::c_void;
use stdext::function_name;

use crate::errors::{Error, Result};
use crate::events::{Event, EventSender};
use crate::util::Cleanuper;

/// `DisplayHandler` manages callbacks for macOS display reconfiguration events.
/// It dispatches `Event`s related to display changes (e.g., addition, removal, resizing) to the event loop.
pub(super) struct DisplayHandler {
    /// The `EventSender` for dispatching display-related events.
    events: EventSender,
    /// An optional `Cleanuper` to manage the unregistration of the display reconfiguration callback.
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
    pub(super) fn new(events: EventSender) -> Self {
        Self {
            events,
            cleanup: None,
        }
    }

    /// Starts the display handler by registering a callback for display reconfiguration events.
    /// This function uses `CGDisplayRegisterReconfigurationCallback` to receive notifications about display changes.
    ///
    /// # Returns
    ///
    /// `Ok(())` if the callback is registered successfully, otherwise `Err(Error)` if permission is denied.
    ///
    /// # Side Effects
    ///
    /// - Registers a `CGDisplayReconfigurationCallback`, which will be unregistered when `cleanup` is dropped.
    pub(super) fn start(&mut self) -> Result<()> {
        info!("{}: Registering display handler", function_name!());
        let this = unsafe { NonNull::new_unchecked(self).as_ptr() };
        let result =
            unsafe { CGDisplayRegisterReconfigurationCallback(Some(Self::callback), this.cast()) };
        if result != CGError::Success {
            return Err(Error::PermissionDenied(format!(
                "{}: registering display handler callback: {result:?}",
                function_name!()
            )));
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
    /// This function maps the `CGDisplayChangeSummaryFlags` to specific `Event` types and dispatches them.
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
        debug!("{}: display change {display_id:?}", function_name!());
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

use core::ptr::NonNull;
use num_enum::{IntoPrimitive, TryFromPrimitive};
use scopeguard::ScopeGuard;
use std::ffi::c_void;
use std::marker::PhantomPinned;
use std::pin::Pin;
use stdext::function_name;
use tracing::{Level, debug, error, instrument};

use crate::errors::Result;
use crate::events::{Event, EventSender};
use crate::platform::{ConnID, OSStatus, WinID, WorkspaceId};
use crate::util::MacResult;

unsafe extern "C" {
    pub fn SLSMainConnectionID() -> ConnID;
    pub fn SLSSpaceGetType(cid: ConnID, sid: WorkspaceId) -> libc::c_int;
    pub fn SLSRegisterConnectionNotifyProc(
        cid: ConnID,
        callback: extern "C-unwind" fn(u32, *mut c_void, usize, *mut c_void, ConnID),
        event: u32,
        data: *mut c_void,
    ) -> OSStatus;
}

pub(super) struct NotifyHandler {
    events: EventSender,
    conn: ConnID,
    _pin: PhantomPinned,
}

pub(super) type PinnedNotifyHandler =
    ScopeGuard<Pin<Box<NotifyHandler>>, Box<dyn FnOnce(Pin<Box<NotifyHandler>>)>>;

impl NotifyHandler {
    pub(super) fn new(events: EventSender) -> Self {
        Self {
            events,
            conn: unsafe { SLSMainConnectionID() },
            _pin: PhantomPinned,
        }
    }

    pub(super) fn start(self) -> Result<PinnedNotifyHandler> {
        debug!("Registering notify handler");
        let cid = self.conn;
        let mut pinned = Box::pin(self);
        let this = unsafe { NonNull::new_unchecked(pinned.as_mut().get_unchecked_mut()) }.as_ptr();
        let events = [
            KnownCGSEvent::SpaceCreated,
            KnownCGSEvent::SpaceCurrentChanged,
            KnownCGSEvent::SpaceDestroyed,
        ];
        for event in events {
            unsafe {
                SLSRegisterConnectionNotifyProc(cid, Self::callback, event.into(), this.cast())
            }
            .to_result(function_name!())?;
        }

        Ok(scopeguard::guard(
            pinned,
            Box::new(|_pin: Pin<Box<Self>>| {
                debug!("Unregistering notify handler");
            }),
        ))
    }

    extern "C-unwind" fn callback(
        event_id: u32,
        data: *mut c_void,
        len: usize,
        context: *mut c_void,
        cid: ConnID,
    ) {
        if let Some(this) =
            NonNull::new(context).map(|this| unsafe { this.cast::<NotifyHandler>().as_mut() })
        {
            this.notify_handler(event_id, data, len, cid);
        } else {
            error!("Zero passed to Notify Handler.");
        }
    }

    #[instrument(level = Level::DEBUG, skip_all, fields(event_id, len))]
    fn notify_handler(&self, event_id: u32, data: *mut c_void, len: usize, _cid: ConnID) {
        let CGSEventType::Known(event) = event_id.into() else {
            debug!("Unknown event received: {event_id}");
            return;
        };

        match event {
            KnownCGSEvent::SpaceCreated
            | KnownCGSEvent::SpaceDestroyed
            | KnownCGSEvent::SpaceCurrentChanged => {
                if let Some(space_id) = from_bytes::<WorkspaceId>(data, len) {
                    if matches!(event, KnownCGSEvent::SpaceDestroyed) {
                        _ = self.events.send(Event::SpaceDestroyed { space_id });
                    } else {
                        let space_type = unsafe { SLSSpaceGetType(self.conn, space_id) };
                        if space_type == 0 && matches!(event, KnownCGSEvent::SpaceCreated) {
                            _ = self.events.send(Event::SpaceCreated { space_id });
                        } else {
                            debug!("{event} space = {space_id}, space_type = {space_type}");
                        }
                    }
                }
            }

            KnownCGSEvent::SpaceWindowDestroyed | KnownCGSEvent::SpaceWindowCreated => {
                let offset = std::mem::size_of::<u64>();
                if let Some(space) = from_bytes::<WorkspaceId>(data, len)
                    && let Some(window_id) = from_bytes::<WinID>(
                        unsafe { data.byte_add(offset) },
                        len.saturating_sub(offset),
                    )
                {
                    debug!("{event} space = {space}, window_id = {window_id}");
                }
            }

            KnownCGSEvent::WindowClosed
            | KnownCGSEvent::WindowMoved
            | KnownCGSEvent::WindowResized
            | KnownCGSEvent::WindowReordered
            | KnownCGSEvent::WindowLevelChanged
            | KnownCGSEvent::WindowUnhidden
            | KnownCGSEvent::WindowHidden
            | KnownCGSEvent::WindowManagerActivatingClickOrdering
            | KnownCGSEvent::WindowOrderingGroupChanged
            | KnownCGSEvent::WindowParentChanged => {
                let window_id = from_bytes::<WinID>(data, len);
                debug!("{event} window_id = {window_id:?}");
            }

            _ => {
                let bytes = (!data.is_null() && len > 0)
                    .then_some(unsafe { std::slice::from_raw_parts(data as *const u8, len) });
                debug!("Unhandled event {event}: {bytes:?}");
            }
        }
    }
}

fn from_bytes<T>(data: *const c_void, len: usize) -> Option<T> {
    let size = std::mem::size_of::<T>();
    (!data.is_null() && len >= size)
        .then_some(unsafe { std::ptr::read_unaligned(data.cast::<T>()) })
}

// credits
// https://github.com/asmagill/hs._asm.undocumented.spaces/blob/master/CGSSpace.h.
// https://github.com/koekeishiya/yabai/blob/d55a647913ab72d8d8b348bee2d3e59e52ce4a5d/src/misc/extern.h.
// https://github.com/acsandmann/rift
#[repr(u32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, IntoPrimitive, TryFromPrimitive)]
enum KnownCGSEvent {
    DisplayWillSleep = 102,
    DisplayDidWake = 103,
    WindowUpdated = 723,
    // maybe loginwindow active? kCGSEventNotificationSystemDefined = 724,
    WindowClosed = 804,
    WindowMoved = 806,
    WindowResized = 807,
    WindowReordered = 808,
    WindowLevelChanged = 811,
    WindowUnhidden = 815,
    WindowHidden = 816,
    MissionControlEntered = 1204,
    /// Named in `_WSLogStringForNotifyType`; observed when the active display /
    /// status-bar space changes, including current-space and capability updates.
    PackagesStatusBarSpaceChanged = 1308,
    WindowTitleChanged = 1322,
    SpaceWindowCreated = 1325,
    SpaceWindowDestroyed = 1326,
    SpaceCreated = 1327,
    SpaceDestroyed = 1328,
    /// Posted by `managed_display_set_current_space` through
    /// `post_space_lifecycle_notification`; likely carries the new current
    /// space id for a display transition.
    SpaceCurrentChanged = 1329,
    /// Local WM notification posted during activating-click ordering; payload is
    /// believed to be window/order metadata, but the exact layout is still
    /// under investigation.
    WindowManagerActivatingClickOrdering = 1333,
    /// Local notification posted when the front connection for the current
    /// space changes.
    WindowManagerSpaceFrontConnectionChanged = 1334,
    /// Local notification posted when the global front connection changes.
    WindowManagerGlobalFrontConnectionChanged = 1335,
    /// Posted from `finish_order_windows`; observed payload is 3 x u32.
    WindowOrderingGroupChanged = 1336,
    /// Posted by `-[PKGSpaceWindowManager_commitTransaction]`; useful as a
    /// transaction boundary even when per-window membership notifications race.
    SpaceWindowTransactionCommitted = 1338,
    /// Posted from `finishBatchReassociateWindows`; observed payload starts with
    /// a u64 key/space followed by a u32 count and repeated window ids.
    SpaceWindowBatchReassociated = 1339,
    /// Posted via `__XSetSpaceWindowManagementCapabilities`; likely tied to
    /// space/window-management mode changes for a display or space.
    SpaceWindowManagementCapabilitiesChanged = 1340,
    /// Posted from `_WSWindowSetParent` and related reassociation paths.
    WindowParentChanged = 1341,
    /// Local notification from `managed_space_update_membership`; likely marks
    /// a completed space-membership mutation and may carry space/window ids.
    ManagedSpaceMembershipUpdated = 1342,
    WorkspaceWillChange = 1400,
    WorkspaceDidChange = 1401,
    WorkspaceWindowIsViewable = 1402,
    WorkspaceWindowIsNotViewable = 1403,
    WorkspaceWindowDidMove = 1404,
    WorkspacePrefsDidChange = 1405,
    WorkspacesWindowDragDidStart = 1411,
    WorkspacesWindowDragDidEnd = 1412,
    WorkspacesWindowDragWillEnd = 1413,
    WorkspacesShowSpaceForProcess = 1414,
    WorkspacesWindowDidOrderInOnNonCurrentManagedSpacesOnly = 1415,
    WorkspacesWindowDidOrderOutOnNonCurrentManagedSpaces = 1416,
    FrontmostApplicationChanged = 1508,
    TransitionDidFinish = 1700,
    All = 0xFFFF_FFFF,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum CGSEventType {
    Known(KnownCGSEvent),
    Unknown(u32),
}

impl From<u32> for CGSEventType {
    fn from(v: u32) -> Self {
        match KnownCGSEvent::try_from(v) {
            Ok(k) => Self::Known(k),
            Err(_) => Self::Unknown(v),
        }
    }
}
impl From<CGSEventType> for u32 {
    fn from(k: CGSEventType) -> u32 {
        match k {
            CGSEventType::Known(k) => k as u32,
            CGSEventType::Unknown(v) => v,
        }
    }
}

impl std::fmt::Display for KnownCGSEvent {
    #[inline]
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Debug::fmt(self, f)
    }
}

impl std::fmt::Display for CGSEventType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CGSEventType::Known(k) => write!(f, "{k}"),
            CGSEventType::Unknown(v) => write!(f, "Unknown({v})"),
        }
    }
}

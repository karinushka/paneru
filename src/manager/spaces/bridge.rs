//! Main-thread `SkyLight` census queries and runtime ABI-validated Objective-C
//! `SLSBridged*Operation` (`WMBridge`) dispatchers.

use core::ptr::NonNull;
use std::collections::HashMap;
use std::ffi::CStr;
use std::ops::Deref as _;
use std::panic::AssertUnwindSafe;

use objc2::rc::{Allocated, Retained, autoreleasepool};
use objc2::runtime::{AnyClass, AnyObject, Sel};
use objc2::{MainThreadMarker, msg_send, sel};
use objc2_app_kit::NSApplication;
use objc2_core_foundation::{
    CFArray, CFBoolean, CFDictionary, CFNumber, CFNumberType, CFRetained, CFString, CFType,
};
use objc2_core_graphics::{
    CGDirectDisplayID, CGGetActiveDisplayList, CGMainDisplayID, CGSessionCopyCurrentDictionary,
    CGWindowListCopyWindowInfo, CGWindowListOption, kCGNullWindowID, kCGWindowLayer,
    kCGWindowNumber, kCGWindowOwnerPID,
};
use objc2_foundation::{NSArray, NSDictionary, NSNumber, NSString};

use super::{
    AssignMode, DesktopBridgeApi, RawDisplaySpaces, STICKY_WINDOW_TAG, SpaceCensus, SpaceError,
    SpaceErrorKind, SpaceId, SpaceKind, WindowServerMeta, resolve_skylight_display_id,
};
use crate::manager::Display;
use crate::manager::skylight::{
    SLSCopyAssociatedWindows, SLSCopyManagedDisplaySpaces, SLSCopySpacesForWindows,
    SLSCopyWindowsWithOptionsAndTags, SLSMainConnectionID, SLSManagedDisplayGetCurrentSpace,
    SLSWindowIteratorAdvance, SLSWindowIteratorGetTags, SLSWindowIteratorGetWindowID,
    SLSWindowQueryResultCopyWindows, SLSWindowQueryWindows,
};
use crate::platform::{ConnID, Pid, WinID, WorkspaceId};
use crate::util::create_array;

/// Expected Objective-C class and method signature for a `WMBridge` operation.
pub struct OperationSpec {
    pub class_name: &'static CStr,
    pub init_sel: Sel,
    /// Expected Objective-C type encodings for explicit parameters after `self` and `_cmd`.
    pub init_args: &'static [&'static CStr],
    /// `c"@"` for synchronous operations, `c"v"` for asynchronous operations.
    pub perform_ret: &'static CStr,
}

/// Verifies that `cls` implements `selector` with exact return and argument
/// type encodings before dispatching via `objc_msgSend`.
#[must_use]
pub fn method_matches(
    cls: &AnyClass,
    selector: Sel,
    expected_ret: &CStr,
    expected_args: &[&CStr],
) -> bool {
    let Some(method) = cls.instance_method(selector) else {
        return false;
    };
    if method.arguments_count() != expected_args.len() + 2 {
        return false;
    }
    if method.return_type().as_ref() != expected_ret {
        return false;
    }
    expected_args.iter().enumerate().all(|(i, &expected)| {
        method
            .argument_type(i + 2)
            .is_some_and(|actual| actual.as_ref() == expected)
    })
}

fn resolve_operation(spec: &OperationSpec) -> Result<&'static AnyClass, SpaceError> {
    let name = spec.class_name.to_string_lossy();
    let cls = AnyClass::get(spec.class_name).ok_or_else(|| {
        SpaceError::new(
            SpaceErrorKind::Unsupported,
            format!("missing SkyLight class {name}"),
        )
    })?;
    let perform = sel!(performWithWMBridgeDelegate);
    if !method_matches(cls, spec.init_sel, c"@", spec.init_args)
        || !method_matches(cls, perform, spec.perform_ret, &[])
    {
        return Err(SpaceError::new(
            SpaceErrorKind::Unsupported,
            format!("SkyLight ABI mismatch on {name}"),
        ));
    }
    Ok(cls)
}

fn guarded<T>(f: impl FnOnce() -> Result<T, SpaceError>) -> Result<T, SpaceError> {
    autoreleasepool(|_| match objc2::exception::catch(AssertUnwindSafe(f)) {
        Ok(result) => result,
        Err(exc) => Err(SpaceError::new(
            SpaceErrorKind::OperationFailed,
            exc.map_or_else(
                || "unknown Objective-C exception in SkyLight".to_string(),
                |obj| format!("Objective-C exception in SkyLight: {obj:?}"),
            ),
        )),
    })
}

/// Extracts an integer from `value` only when its `CoreFoundation` type ID is
/// `CFNumber` and it is **not** a floating-point representation (§6.5).
#[must_use]
pub fn cf_int_i64(value: &CFType) -> Option<i64> {
    value
        .downcast_ref::<CFNumber>()
        .filter(|n| !n.is_float_type())
        .and_then(CFNumber::as_i64)
}

fn cf_space_id(value: &CFType) -> Option<SpaceId> {
    cf_int_i64(value)
        .and_then(|v| u64::try_from(v).ok())
        .and_then(SpaceId::new)
}

fn cf_positive_win_id(value: &CFType) -> Option<WinID> {
    cf_int_i64(value)
        .and_then(|v| i32::try_from(v).ok())
        .filter(|&v| v > 0)
}

fn session_flag(
    dict: &CFDictionary<CFString, CFType>,
    key: &str,
) -> Result<Option<bool>, SpaceError> {
    let cf_key = CFString::from_str(key);
    let Some(value) = dict.get(&cf_key) else {
        return Ok(None);
    };
    if let Some(boolean) = value.downcast_ref::<CFBoolean>() {
        return Ok(Some(boolean.value()));
    }
    if let Some(int_val) = cf_int_i64(&value) {
        return Ok(Some(int_val != 0));
    }
    Err(SpaceError::new(
        SpaceErrorKind::QueryFailed,
        format!("unparseable session flag {key}"),
    ))
}

/// Evaluates a `CGSessionCopyCurrentDictionary` dictionary: requires an active
/// console login and returns `Ok(true)` when the screen is locked (§2.3).
pub fn parse_session_locked(dict: &CFDictionary<CFString, CFType>) -> Result<bool, SpaceError> {
    // `<CoreGraphics/CGSession.h>` defines `kCGSessionOnConsoleKey` as
    // `"kCGSSessionOnConsoleKey"` (two `S`s) and `kCGSessionLoginDoneKey` as
    // `"kCGSessionLoginDoneKey"` (one `S`).
    let on_console = session_flag(dict, "kCGSSessionOnConsoleKey")?.unwrap_or(false);
    let login_done = session_flag(dict, "kCGSessionLoginDoneKey")?.unwrap_or(false);
    if !on_console || !login_done {
        return Err(SpaceError::new(
            SpaceErrorKind::NoGuiSession,
            "current session is not an active console login",
        ));
    }
    Ok(session_flag(dict, "CGSSessionScreenIsLocked")?.unwrap_or(false))
}

/// Whether the current macOS console session reports a locked screen.
#[must_use]
pub fn current_session_screen_locked() -> bool {
    let Some(session) = CGSessionCopyCurrentDictionary() else {
        return false;
    };
    let dict = unsafe { session.cast_unchecked::<CFString, CFType>() };
    match session_flag(dict, "CGSSessionScreenIsLocked") {
        Ok(Some(locked)) => locked,
        Ok(None) => false,
        // Fail closed when the lock flag exists with an unexpected CFType.
        Err(_) => true,
    }
}

fn space_ids_ns_array(ids: &[SpaceId]) -> Retained<NSArray<NSNumber>> {
    let nums: Vec<Retained<NSNumber>> = ids.iter().map(|s| NSNumber::new_u64(s.get())).collect();
    NSArray::from_retained_slice(&nums)
}

fn as_ns_array(cf: &CFArray) -> &NSArray {
    // SAFETY: `CFArray` and `NSArray` are toll-free bridged on macOS.
    unsafe { &*(&raw const *cf).cast::<NSArray>() }
}

fn active_display_table(cid: ConnID) -> Vec<(CGDirectDisplayID, String, WorkspaceId)> {
    let mut count = 0u32;
    unsafe { CGGetActiveDisplayList(0, std::ptr::null_mut(), &raw mut count) };
    if count == 0 {
        return Vec::new();
    }
    let len = usize::try_from(count).unwrap_or(0);
    let mut ids = Vec::with_capacity(len);
    unsafe {
        CGGetActiveDisplayList(count, ids.as_mut_ptr(), &raw mut count);
        ids.set_len(usize::try_from(count).unwrap_or(0));
    }
    ids.into_iter()
        .map(|id| {
            let (uuid_str, current_space) = match Display::uuid_from_id(id) {
                Ok(uuid) => {
                    let cur = unsafe { SLSManagedDisplayGetCurrentSpace(cid, &raw const *uuid) };
                    (uuid.to_string(), cur)
                }
                Err(_) => (String::new(), 0),
            };
            (id, uuid_str, current_space)
        })
        .collect()
}

fn parse_fullscreen_kind(space: &CFDictionary<CFString, CFType>) -> SpaceKind {
    let owner_wid = space
        .get(&CFString::from_static_str("fs_wid"))
        .and_then(|v| cf_positive_win_id(&v));
    let single_tile = owner_wid.is_some_and(|owner| {
        let Some(tlm) = space
            .get(&CFString::from_static_str("TileLayoutManager"))
            .and_then(|v| v.downcast::<CFDictionary>().ok())
        else {
            return false;
        };
        let tlm = unsafe { tlm.cast_unchecked::<CFString, CFType>() };
        let Some(tiles) = tlm
            .get(&CFString::from_static_str("TileSpaces"))
            .and_then(|v| v.downcast::<CFArray>().ok())
        else {
            return false;
        };
        if tiles.count() != 1 {
            return false;
        }
        let tiles = unsafe { tiles.cast_unchecked::<CFDictionary<CFString, CFType>>() };
        tiles.iter().next().is_some_and(|tile| {
            let fs = tile
                .get(&CFString::from_static_str("fs_wid"))
                .and_then(|v| cf_positive_win_id(&v));
            let wid = tile
                .get(&CFString::from_static_str("TileWindowID"))
                .and_then(|v| cf_positive_win_id(&v));
            fs == Some(owner) && wid == Some(owner)
        })
    });
    SpaceKind::Fullscreen {
        owner_wid,
        single_tile,
    }
}

/// Reads and validates `SLSCopyManagedDisplaySpaces` for connection `cid`.
pub fn read_managed_display_spaces(cid: ConnID) -> Result<SpaceCensus, SpaceError> {
    let raw_list = NonNull::new(unsafe { SLSCopyManagedDisplaySpaces(cid) })
        .map(|ptr| unsafe { CFRetained::from_raw(ptr) })
        .ok_or_else(|| {
            SpaceError::new(
                SpaceErrorKind::QueryFailed,
                "SLSCopyManagedDisplaySpaces returned null",
            )
        })?;
    let active_displays = active_display_table(cid);
    let active_refs: Vec<(CGDirectDisplayID, &str, WorkspaceId)> = active_displays
        .iter()
        .map(|(id, uuid, cur)| (*id, uuid.as_str(), *cur))
        .collect();
    let main_display = CGMainDisplayID();

    let displays_cf = unsafe { raw_list.cast_unchecked::<CFDictionary<CFString, CFType>>() };
    let raw_displays = displays_cf
        .iter()
        .filter_map(|display_dict| {
            let identifier = display_dict
                .get(&CFString::from_static_str("Display Identifier"))
                .and_then(|v| v.downcast_ref::<CFString>().map(ToString::to_string))?;
            let current_dict = display_dict
                .get(&CFString::from_static_str("Current Space"))
                .and_then(|v| v.downcast::<CFDictionary>().ok())?;
            let current_dict = unsafe { current_dict.cast_unchecked::<CFString, CFType>() };
            let current_space = current_dict
                .get(&CFString::from_static_str("id64"))
                .and_then(|v| cf_space_id(&v))?;
            let display_id = resolve_skylight_display_id(
                &identifier,
                current_space.get(),
                main_display,
                &active_refs,
            )?;

            let spaces_arr = display_dict
                .get(&CFString::from_static_str("Spaces"))
                .and_then(|v| v.downcast::<CFArray>().ok())?;
            let spaces_arr =
                unsafe { spaces_arr.cast_unchecked::<CFDictionary<CFString, CFType>>() };
            let spaces = spaces_arr
                .iter()
                .map(|space| {
                    let id = space
                        .get(&CFString::from_static_str("id64"))
                        .and_then(|v| cf_space_id(&v))
                        .ok_or_else(|| {
                            SpaceError::new(SpaceErrorKind::QueryFailed, "invalid space id64")
                        })?;
                    let raw_type = space
                        .get(&CFString::from_static_str("type"))
                        .and_then(|v| cf_int_i64(&v))
                        .and_then(|v| i32::try_from(v).ok())
                        .ok_or_else(|| {
                            SpaceError::new(SpaceErrorKind::QueryFailed, "invalid space type")
                        })?;
                    let uuid = space
                        .get(&CFString::from_static_str("uuid"))
                        .and_then(|v| v.downcast_ref::<CFString>().map(ToString::to_string))
                        .unwrap_or_default();
                    let kind = match raw_type {
                        0 => SpaceKind::Desktop,
                        4 => parse_fullscreen_kind(&space),
                        other => SpaceKind::Other(other),
                    };
                    Ok((id, uuid, kind))
                })
                .collect::<Result<Vec<_>, SpaceError>>();
            Some(spaces.map(|spaces| RawDisplaySpaces {
                display_id,
                skylight_display: identifier,
                current_space,
                spaces,
            }))
        })
        .collect::<Result<Vec<_>, SpaceError>>()?;

    SpaceCensus::from_raw(raw_displays)
}

fn all_window_meta() -> Option<HashMap<WinID, WindowServerMeta>> {
    let list = CGWindowListCopyWindowInfo(CGWindowListOption::OptionAll, kCGNullWindowID)?;
    let array = unsafe { list.cast_unchecked::<CFDictionary<CFString, CFType>>() };
    let map = array
        .iter()
        .filter_map(|dict| {
            let wid = dict
                .get(unsafe { kCGWindowNumber })
                .and_then(|v| cf_int_i64(&v))
                .and_then(|v| i32::try_from(v).ok())?;
            let layer = dict
                .get(unsafe { kCGWindowLayer })
                .and_then(|v| cf_int_i64(&v))
                .and_then(|v| i32::try_from(v).ok())?;
            let pid = dict
                .get(unsafe { kCGWindowOwnerPID })
                .and_then(|v| cf_int_i64(&v))
                .and_then(|v| i32::try_from(v).ok())
                .unwrap_or(0);
            Some((wid, WindowServerMeta { layer, pid }))
        })
        .collect();
    Some(map)
}

/// Real main-thread `SkyLight` + `WMBridge` implementation.
pub struct DesktopBridgeOS {
    cid: ConnID,
    _mtm: MainThreadMarker,
}

impl DesktopBridgeOS {
    pub fn new(mtm: MainThreadMarker) -> Result<Self, SpaceError> {
        let _app = NSApplication::sharedApplication(mtm);
        let cid = unsafe { SLSMainConnectionID() };
        if cid == 0 {
            return Err(SpaceError::new(
                SpaceErrorKind::NoGuiSession,
                "SLSMainConnectionID returned 0",
            ));
        }
        Ok(Self { cid, _mtm: mtm })
    }

    fn dispatch_async(
        spec: &OperationSpec,
        applied: &mut bool,
        build: impl FnOnce(&'static AnyClass) -> Option<Retained<AnyObject>>,
    ) -> Result<(), SpaceError> {
        let cls = resolve_operation(spec)?;
        let op = build(cls).ok_or_else(|| {
            SpaceError::new(
                SpaceErrorKind::OperationFailed,
                format!(
                    "{} initializer returned nil",
                    spec.class_name.to_string_lossy()
                ),
            )
            .with_applied(*applied)
        })?;
        *applied = true;
        let (): () = unsafe { msg_send![&*op, performWithWMBridgeDelegate] };
        Ok(())
    }
}

impl DesktopBridgeApi for DesktopBridgeOS {
    fn read_census(&self) -> Result<SpaceCensus, SpaceError> {
        guarded(|| read_managed_display_spaces(self.cid))
    }

    fn session_unlocked(&self) -> Result<(), SpaceError> {
        let session = CGSessionCopyCurrentDictionary().ok_or_else(|| {
            SpaceError::new(
                SpaceErrorKind::NoGuiSession,
                "CGSessionCopyCurrentDictionary returned null",
            )
        })?;
        let dict = unsafe { session.cast_unchecked::<CFString, CFType>() };
        if parse_session_locked(dict)? {
            return Err(SpaceError::new(
                SpaceErrorKind::SessionLocked,
                "screen is locked",
            ));
        }
        Ok(())
    }

    fn spaces_for_window(&self, window_id: WinID) -> Result<Vec<SpaceId>, SpaceError> {
        guarded(|| {
            let windows = create_array(&[window_id], CFNumberType::SInt32Type).map_err(|e| {
                SpaceError::new(SpaceErrorKind::QueryFailed, format!("window array: {e}"))
            })?;
            let ptr = unsafe { SLSCopySpacesForWindows(self.cid, 7, &raw const *windows) };
            let Some(ptr) = NonNull::new(ptr) else {
                return Err(SpaceError::new(
                    SpaceErrorKind::QueryFailed,
                    "SLSCopySpacesForWindows returned null",
                ));
            };
            let arr = unsafe { CFRetained::from_raw(ptr) };
            Ok(arr
                .iter()
                .filter_map(|n| {
                    (!n.is_float_type())
                        .then(|| n.as_i64())
                        .flatten()
                        .and_then(|v| u64::try_from(v).ok())
                        .and_then(SpaceId::new)
                })
                .collect())
        })
    }

    fn windows_in_space(
        &self,
        space_id: SpaceId,
        include_minimized: bool,
    ) -> Result<Vec<WinID>, SpaceError> {
        guarded(|| {
            let spaces = space_ids_ns_array(&[space_id]);
            let spaces_cf = unsafe { &*(&raw const *spaces).cast::<CFArray>() };
            let (mut set_tags, mut clear_tags) = (0i64, 0i64);
            let options = if include_minimized { 0x7 } else { 0x2 };
            let ptr = unsafe {
                SLSCopyWindowsWithOptionsAndTags(
                    self.cid,
                    0,
                    spaces_cf,
                    options,
                    &mut set_tags,
                    &mut clear_tags,
                )
            };
            let Some(ptr) = NonNull::new(ptr) else {
                return Err(SpaceError::new(
                    SpaceErrorKind::QueryFailed,
                    "SLSCopyWindowsWithOptionsAndTags returned null",
                ));
            };
            let raw_cf = unsafe { CFRetained::from_raw(ptr) };
            let raw_ids = unsafe { raw_cf.cast_unchecked::<CFNumber>() };
            let meta_map = all_window_meta().ok_or_else(|| {
                SpaceError::new(
                    SpaceErrorKind::QueryFailed,
                    "CGWindowListCopyWindowInfo returned null",
                )
            })?;
            raw_ids
                .iter()
                .filter_map(|n| (!n.is_float_type()).then(|| n.as_i32()).flatten())
                .filter_map(|wid| match meta_map.get(&wid) {
                    Some(meta) if meta.is_app_window() => Some(Ok(wid)),
                    // Fail closed if WindowServer reports a window with missing metadata.
                    None if include_minimized => Some(Err(SpaceError::new(
                        SpaceErrorKind::QueryFailed,
                        format!("missing CGWindow metadata for window {wid}"),
                    ))),
                    Some(_) | None => None,
                })
                .collect()
        })
    }

    fn window_is_sticky(&self, window_id: WinID) -> Option<bool> {
        let windows = create_array(&[window_id], CFNumberType::SInt32Type).ok()?;
        let query = unsafe {
            CFRetained::from_raw(SLSWindowQueryWindows(self.cid, &raw const *windows, 1))
        };
        let iter =
            unsafe { CFRetained::from_raw(SLSWindowQueryResultCopyWindows(query.deref().into())) };
        while unsafe { SLSWindowIteratorAdvance(&raw const *iter) } {
            if unsafe { SLSWindowIteratorGetWindowID(&raw const *iter) } == window_id {
                let tags = unsafe { SLSWindowIteratorGetTags(&raw const *iter) }.cast_unsigned();
                return Some((tags & STICKY_WINDOW_TAG) != 0);
            }
        }
        None
    }

    fn associated_windows(&self, window_id: WinID) -> Result<Vec<WinID>, SpaceError> {
        let arr = unsafe { CFRetained::from_raw(SLSCopyAssociatedWindows(self.cid, window_id)) };
        Ok(arr
            .iter()
            .filter_map(|n| (!n.is_float_type()).then(|| n.as_i32()).flatten())
            .collect())
    }

    fn window_meta(&self, window_id: WinID) -> Option<WindowServerMeta> {
        all_window_meta()?.remove(&window_id)
    }

    fn windows_for_pid(&self, pid: Pid) -> Vec<WinID> {
        let Some(meta) = all_window_meta() else {
            return Vec::new();
        };
        meta.into_iter()
            .filter(|(_, m)| m.pid == pid && m.is_app_window())
            .map(|(wid, _)| wid)
            .filter(|&wid| self.spaces_for_window(wid).is_ok_and(|s| !s.is_empty()))
            .collect()
    }

    fn activate_space(
        &mut self,
        skylight_display: &str,
        target: SpaceId,
        hide: &[SpaceId],
    ) -> Result<(), SpaceError> {
        self.session_unlocked()?;
        let mut applied = false;
        guarded(|| {
            let show_spec = OperationSpec {
                class_name: c"SLSBridgedShowSpacesOperation",
                init_sel: sel!(initWithSpaces:),
                init_args: &[c"@"],
                perform_ret: c"v",
            };
            let hide_spec = OperationSpec {
                class_name: c"SLSBridgedHideSpacesOperation",
                init_sel: sel!(initWithSpaces:),
                init_args: &[c"@"],
                perform_ret: c"v",
            };
            let set_spec = OperationSpec {
                class_name: c"SLSBridgedManagedDisplaySetCurrentSpaceOperation",
                init_sel: sel!(initWithDisplayIdentifier:spaceID:),
                init_args: &[c"@", c"Q"],
                perform_ret: c"v",
            };
            // Pre-validate all 3 ABIs before dispatching the first mutation.
            let _ = (
                resolve_operation(&show_spec)?,
                resolve_operation(&hide_spec)?,
                resolve_operation(&set_spec)?,
            );

            let show_arr = space_ids_ns_array(&[target]);
            Self::dispatch_async(&show_spec, &mut applied, |cls| {
                let alloc: Allocated<AnyObject> = unsafe { msg_send![cls, alloc] };
                unsafe { msg_send![alloc, initWithSpaces: &*show_arr] }
            })?;

            if !hide.is_empty() {
                let hide_arr = space_ids_ns_array(hide);
                Self::dispatch_async(&hide_spec, &mut applied, |cls| {
                    let alloc: Allocated<AnyObject> = unsafe { msg_send![cls, alloc] };
                    unsafe { msg_send![alloc, initWithSpaces: &*hide_arr] }
                })?;
            }

            let disp_ns = NSString::from_str(skylight_display);
            let target_raw = target.get();
            Self::dispatch_async(&set_spec, &mut applied, |cls| {
                let alloc: Allocated<AnyObject> = unsafe { msg_send![cls, alloc] };
                unsafe {
                    msg_send![alloc, initWithDisplayIdentifier: &*disp_ns, spaceID: target_raw]
                }
            })
        })
        .map_err(|e| e.with_applied(applied))
    }

    fn create_desktop(&mut self) -> Result<SpaceId, SpaceError> {
        self.session_unlocked()?;
        let mut applied = false;
        guarded(|| {
            let spec = OperationSpec {
                class_name: c"SLSBridgedSpaceCreateOperation",
                init_sel: sel!(initWithOptions:values:),
                init_args: &[c"I", c"@"],
                perform_ret: c"@",
            };
            let cls = resolve_operation(&spec)?;
            let values: Retained<NSDictionary<NSString, AnyObject>> = NSDictionary::new();
            let alloc: Allocated<AnyObject> = unsafe { msg_send![cls, alloc] };
            let op: Option<Retained<AnyObject>> =
                unsafe { msg_send![alloc, initWithOptions: 0u32, values: &*values] };
            let op = op.ok_or_else(|| {
                SpaceError::new(
                    SpaceErrorKind::OperationFailed,
                    "SLSBridgedSpaceCreateOperation returned nil",
                )
            })?;
            applied = true;
            let res: Option<Retained<AnyObject>> =
                unsafe { msg_send![&*op, performWithWMBridgeDelegate] };
            let res = res.ok_or_else(|| {
                SpaceError::new(
                    SpaceErrorKind::OperationFailed,
                    "SLSBridgedSpaceCreateOperation result was nil",
                )
                .with_applied(true)
            })?;
            if !method_matches(res.class(), sel!(spaceID), c"Q", &[]) {
                return Err(SpaceError::new(
                    SpaceErrorKind::Unsupported,
                    "space create result missing -spaceID:Q",
                )
                .with_applied(true));
            }
            let raw_id: u64 = unsafe { msg_send![&*res, spaceID] };
            SpaceId::new(raw_id).ok_or_else(|| {
                SpaceError::new(SpaceErrorKind::OperationFailed, "created spaceID was 0")
                    .with_applied(true)
            })
        })
        .map_err(|e| e.with_applied(applied))
    }

    fn destroy_space(&mut self, space_id: SpaceId) -> Result<(), SpaceError> {
        self.session_unlocked()?;
        let mut applied = false;
        let raw_id = space_id.get();
        guarded(|| {
            let spec = OperationSpec {
                class_name: c"SLSBridgedSpaceDestroyOperation",
                init_sel: sel!(initWithSpaceID:),
                init_args: &[c"Q"],
                perform_ret: c"v",
            };
            Self::dispatch_async(&spec, &mut applied, |cls| {
                let alloc: Allocated<AnyObject> = unsafe { msg_send![cls, alloc] };
                unsafe { msg_send![alloc, initWithSpaceID: raw_id] }
            })
        })
        .map_err(|e| e.with_applied(applied))
    }

    fn move_windows_to_space(
        &mut self,
        windows: &[WinID],
        space_id: SpaceId,
    ) -> Result<(), SpaceError> {
        self.session_unlocked()?;
        let mut applied = false;
        let raw_id = space_id.get();
        guarded(|| {
            let win_cf = create_array(windows, CFNumberType::SInt32Type)
                .map_err(|e| SpaceError::new(SpaceErrorKind::QueryFailed, e.to_string()))?;
            let spec = OperationSpec {
                class_name: c"SLSBridgedMoveWindowsToManagedSpaceOperation",
                init_sel: sel!(initWithWindows:spaceID:),
                init_args: &[c"@", c"Q"],
                perform_ret: c"v",
            };
            Self::dispatch_async(&spec, &mut applied, |cls| {
                let alloc: Allocated<AnyObject> = unsafe { msg_send![cls, alloc] };
                let ns_arr = as_ns_array(&win_cf);
                unsafe { msg_send![alloc, initWithWindows: ns_arr, spaceID: raw_id] }
            })
        })
        .map_err(|e| e.with_applied(applied))
    }

    fn add_windows_to_space(
        &mut self,
        windows: &[WinID],
        space_id: SpaceId,
        remove_others: bool,
    ) -> Result<(), SpaceError> {
        self.session_unlocked()?;
        let mut applied = false;
        let raw_id = space_id.get();
        let options: u32 = if remove_others { 7 } else { 0 };
        guarded(|| {
            let win_cf = create_array(windows, CFNumberType::SInt32Type)
                .map_err(|e| SpaceError::new(SpaceErrorKind::QueryFailed, e.to_string()))?;
            let spec = OperationSpec {
                class_name: c"SLSBridgedSpaceAddWindowsAndRemoveFromSpacesOperation",
                init_sel: sel!(initWithSpaceID:windows:options:),
                init_args: &[c"Q", c"@", c"I"],
                perform_ret: c"v",
            };
            Self::dispatch_async(&spec, &mut applied, |cls| {
                let alloc: Allocated<AnyObject> = unsafe { msg_send![cls, alloc] };
                let ns_arr = as_ns_array(&win_cf);
                unsafe {
                    msg_send![
                        alloc,
                        initWithSpaceID: raw_id,
                        windows: ns_arr,
                        options: options
                    ]
                }
            })
        })
        .map_err(|e| e.with_applied(applied))
    }

    fn assign_process(&mut self, pid: Pid, mode: AssignMode) -> Result<(), SpaceError> {
        self.session_unlocked()?;
        let mut applied = false;
        guarded(|| match mode {
            AssignMode::AllSpaces => {
                let spec = OperationSpec {
                    class_name: c"SLSBridgedProcessAssignToAllSpacesOperation",
                    init_sel: sel!(initWithProcess:),
                    init_args: &[c"i"],
                    perform_ret: c"v",
                };
                Self::dispatch_async(&spec, &mut applied, |cls| {
                    let alloc: Allocated<AnyObject> = unsafe { msg_send![cls, alloc] };
                    unsafe { msg_send![alloc, initWithProcess: pid] }
                })
            }
            AssignMode::Space(space_id) => {
                let raw_id = space_id.get();
                let spec = OperationSpec {
                    class_name: c"SLSBridgedProcessAssignToSpaceOperation",
                    init_sel: sel!(initWithProcess:spaceID:),
                    init_args: &[c"i", c"Q"],
                    perform_ret: c"v",
                };
                Self::dispatch_async(&spec, &mut applied, |cls| {
                    let alloc: Allocated<AnyObject> = unsafe { msg_send![cls, alloc] };
                    unsafe { msg_send![alloc, initWithProcess: pid, spaceID: raw_id] }
                })
            }
            AssignMode::Clear => {
                let spec = OperationSpec {
                    class_name: c"SLSBridgedProcessAssignToSpaceOperation",
                    init_sel: sel!(initWithProcess:spaceID:),
                    init_args: &[c"i", c"Q"],
                    perform_ret: c"v",
                };
                Self::dispatch_async(&spec, &mut applied, |cls| {
                    let alloc: Allocated<AnyObject> = unsafe { msg_send![cls, alloc] };
                    unsafe { msg_send![alloc, initWithProcess: pid, spaceID: 0u64] }
                })
            }
        })
        .map_err(|e| e.with_applied(applied))
    }

    fn move_space_to_index(
        &mut self,
        space_id: SpaceId,
        skylight_display: &str,
        index: u32,
    ) -> Result<(), SpaceError> {
        self.session_unlocked()?;
        let mut applied = false;
        let raw_id = space_id.get();
        guarded(|| {
            let disp_ns = NSString::from_str(skylight_display);
            let spec = OperationSpec {
                class_name: c"SLSBridgedMoveManagedSpaceToDisplayIndexOperation",
                init_sel: sel!(initWithSpaceID:displayIdentifier:index:),
                init_args: &[c"Q", c"@", c"I"],
                perform_ret: c"v",
            };
            Self::dispatch_async(&spec, &mut applied, |cls| {
                let alloc: Allocated<AnyObject> = unsafe { msg_send![cls, alloc] };
                unsafe {
                    msg_send![
                        alloc,
                        initWithSpaceID: raw_id,
                        displayIdentifier: &*disp_ns,
                        index: index
                    ]
                }
            })
        })
        .map_err(|e| e.with_applied(applied))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use objc2_core_foundation::{kCFBooleanFalse, kCFBooleanTrue};

    #[test]
    fn method_matches_validates_return_and_argument_encodings() {
        let cls = AnyClass::get(c"NSNumber").expect("NSNumber exists");
        assert!(method_matches(cls, sel!(initWithInt:), c"@", &[c"i"]));
        // Mismatched argument encoding ("Q" instead of "i") must be rejected.
        assert!(!method_matches(cls, sel!(initWithInt:), c"@", &[c"Q"]));
        // Mismatched return encoding ("v" instead of "@") must be rejected.
        assert!(!method_matches(cls, sel!(initWithInt:), c"v", &[c"i"]));
        // Mismatched arity must be rejected.
        assert!(!method_matches(cls, sel!(initWithInt:), c"@", &[]));
    }

    #[test]
    fn cf_int_i64_accepts_integers_and_rejects_floats_booleans_and_strings() {
        let int_num = CFNumber::new_i64(42);
        assert_eq!(cf_int_i64(int_num.as_ref()), Some(42));

        let float_num = CFNumber::new_f64(4.0);
        assert_eq!(cf_int_i64(float_num.as_ref()), None);

        let boolean: &CFType = unsafe { kCFBooleanTrue.unwrap() }.as_ref();
        assert_eq!(cf_int_i64(boolean), None);

        let text = CFString::from_static_str("4");
        assert_eq!(cf_int_i64(text.as_ref()), None);
    }

    #[test]
    fn parse_session_locked_matches_core_graphics_session_keys() {
        // `<CoreGraphics/CGSession.h>` defines:
        //   `#define kCGSessionOnConsoleKey "kCGSSessionOnConsoleKey"` (two `S`s)
        //   `#define kCGSessionLoginDoneKey "kCGSessionLoginDoneKey"`  (one `S`)
        let k_console = CFString::from_static_str("kCGSSessionOnConsoleKey");
        let k_login = CFString::from_static_str("kCGSessionLoginDoneKey");
        let k_locked = CFString::from_static_str("CGSSessionScreenIsLocked");

        let t: &CFType = unsafe { kCFBooleanTrue.unwrap() }.as_ref();
        let f: &CFType = unsafe { kCFBooleanFalse.unwrap() }.as_ref();
        let one = CFNumber::new_i64(1);
        let bad = CFString::from_static_str("yes");

        // Unlocked session using real CoreGraphics session keys.
        let unlocked = CFDictionary::from_slices(&[&*k_console, &*k_login], &[t, t]);
        assert_eq!(parse_session_locked(&unlocked), Ok(false));

        // Locked via integer CFNumber(1).
        let locked_num =
            CFDictionary::from_slices(&[&*k_console, &*k_login, &*k_locked], &[t, t, one.as_ref()]);
        assert_eq!(parse_session_locked(&locked_num), Ok(true));

        // Explicitly unlocked via CFBooleanFalse.
        let unlocked_bool =
            CFDictionary::from_slices(&[&*k_console, &*k_login, &*k_locked], &[t, t, f]);
        assert_eq!(parse_session_locked(&unlocked_bool), Ok(false));

        // Unexpected CFString lock value must fail closed (`QueryFailed`).
        let corrupt =
            CFDictionary::from_slices(&[&*k_console, &*k_login, &*k_locked], &[t, t, bad.as_ref()]);
        assert_eq!(
            parse_session_locked(&corrupt).unwrap_err().kind,
            SpaceErrorKind::QueryFailed
        );

        // Off-console session returns `NoGuiSession`.
        let off_console = CFDictionary::from_slices(&[&*k_console, &*k_login], &[f, t]);
        assert_eq!(
            parse_session_locked(&off_console).unwrap_err().kind,
            SpaceErrorKind::NoGuiSession
        );

        // When running inside an active macOS GUI login session, live
        // `CGSessionCopyCurrentDictionary()` must succeed without `NoGuiSession`.
        if let Some(live) = CGSessionCopyCurrentDictionary() {
            let live_dict = unsafe { live.cast_unchecked::<CFString, CFType>() };
            if session_flag(live_dict, "kCGSSessionOnConsoleKey") == Ok(Some(true)) {
                assert!(
                    parse_session_locked(live_dict).is_ok(),
                    "active console login session must not fail with NoGuiSession"
                );
            }
        }
    }
}

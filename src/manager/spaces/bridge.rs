//! Main-thread `SkyLight` census queries and runtime ABI-validated Objective-C
//! `SLSBridged*Operation` (`WMBridge`) dispatchers.

use core::ptr::NonNull;
use std::cell::OnceCell;
use std::collections::HashMap;
use std::ffi::CStr;
use std::ops::Deref as _;
use std::panic::AssertUnwindSafe;

use objc2::rc::{Allocated, Retained, autoreleasepool};
use objc2::runtime::{AnyClass, AnyObject, Sel};
use objc2::{MainThreadMarker, msg_send, sel};
use objc2_app_kit::NSApplication;
use objc2_core_foundation::{
    CFArray, CFBoolean, CFDictionary, CFNumber, CFRetained, CFString, CFType,
};
use objc2_core_graphics::{
    CGDirectDisplayID, CGError, CGGetActiveDisplayList, CGMainDisplayID,
    CGSessionCopyCurrentDictionary, CGWindowListCopyWindowInfo, CGWindowListOption,
    kCGNullWindowID, kCGWindowLayer, kCGWindowNumber, kCGWindowOwnerPID,
};
use objc2_foundation::{NSArray, NSDictionary, NSNumber, NSString, ns_string};

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

const ALL_USER_SPACES_MASK: i32 = 0x7;
const VISIBLE_SPACES_MASK: i32 = 0x2;
const REMOVE_FROM_OTHER_SPACES_OPT: u32 = 0x7;

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
    let cls = AnyClass::get(spec.class_name).ok_or_else(|| {
        SpaceError::new(
            SpaceErrorKind::Unsupported,
            format!(
                "missing SkyLight class {}",
                spec.class_name.to_string_lossy()
            ),
        )
    })?;
    let perform = sel!(performWithWMBridgeDelegate);
    if !method_matches(cls, spec.init_sel, c"@", spec.init_args)
        || !method_matches(cls, perform, spec.perform_ret, &[])
    {
        return Err(SpaceError::new(
            SpaceErrorKind::Unsupported,
            format!(
                "SkyLight ABI mismatch on {}",
                spec.class_name.to_string_lossy()
            ),
        ));
    }
    Ok(cls)
}

fn resolve_cached(
    slot: &OnceCell<Result<&'static AnyClass, SpaceError>>,
    spec: &OperationSpec,
) -> Result<&'static AnyClass, SpaceError> {
    slot.get_or_init(|| resolve_operation(spec)).clone()
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

fn as_cf_string(ns: &NSString) -> &CFString {
    // SAFETY: `NSString` and `CFString` are toll-free bridged on macOS.
    unsafe { &*(&raw const *ns).cast::<CFString>() }
}

fn as_cf_array(ns: &NSArray<NSNumber>) -> &CFArray {
    // SAFETY: `NSArray` and `CFArray` are toll-free bridged on macOS.
    unsafe { &*(&raw const *ns).cast::<CFArray>() }
}

fn session_flag(
    dict: &CFDictionary<CFString, CFType>,
    key: &NSString,
) -> Result<Option<bool>, SpaceError> {
    let Some(value) = dict.get(as_cf_string(key)) else {
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
    let on_console = session_flag(dict, ns_string!("kCGSSessionOnConsoleKey"))?.unwrap_or(false);
    let login_done = session_flag(dict, ns_string!("kCGSessionLoginDoneKey"))?.unwrap_or(false);
    if !on_console || !login_done {
        return Err(SpaceError::new(
            SpaceErrorKind::NoGuiSession,
            "current session is not an active console login",
        ));
    }
    Ok(session_flag(dict, ns_string!("CGSSessionScreenIsLocked"))?.unwrap_or(false))
}

/// Whether the current macOS console session reports a locked screen.
#[must_use]
pub fn current_session_screen_locked() -> bool {
    let Some(session) = CGSessionCopyCurrentDictionary() else {
        return false;
    };
    let dict = unsafe { session.cast_unchecked::<CFString, CFType>() };
    match session_flag(dict, ns_string!("CGSSessionScreenIsLocked")) {
        Ok(Some(locked)) => locked,
        Ok(None) => false,
        // Fail closed when the lock flag exists with an unexpected CFType.
        Err(_) => true,
    }
}

fn space_ids_ns_array(ids: &[SpaceId]) -> Retained<NSArray<NSNumber>> {
    if let [single] = ids {
        NSArray::from_retained_slice(&[NSNumber::new_u64(single.get())])
    } else {
        let nums: Vec<Retained<NSNumber>> =
            ids.iter().map(|s| NSNumber::new_u64(s.get())).collect();
        NSArray::from_retained_slice(&nums)
    }
}

fn win_ids_ns_array(wids: &[WinID]) -> Retained<NSArray<NSNumber>> {
    if let [single] = wids {
        NSArray::from_retained_slice(&[NSNumber::new_i32(*single)])
    } else {
        let nums: Vec<Retained<NSNumber>> =
            wids.iter().map(|&wid| NSNumber::new_i32(wid)).collect();
        NSArray::from_retained_slice(&nums)
    }
}

/// Safely queries active CoreGraphics display IDs without exposing
/// uninitialized memory if `CGGetActiveDisplayList` fails.
pub(crate) fn active_display_ids() -> Vec<CGDirectDisplayID> {
    let mut count = 0u32;
    let err = unsafe { CGGetActiveDisplayList(0, std::ptr::null_mut(), &raw mut count) };
    if err != CGError::Success || count == 0 {
        return Vec::new();
    }
    let len = usize::try_from(count).unwrap_or(0);
    let mut ids = vec![0u32; len];
    let mut actual = 0u32;
    let err = unsafe { CGGetActiveDisplayList(count, ids.as_mut_ptr(), &raw mut actual) };
    if err != CGError::Success {
        return Vec::new();
    }
    ids.truncate(usize::try_from(actual).unwrap_or(0).min(len));
    ids
}

fn active_display_table(
    cid: ConnID,
    display_ids: &[CGDirectDisplayID],
) -> Vec<(CGDirectDisplayID, String, WorkspaceId)> {
    display_ids
        .iter()
        .map(|&id| {
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
    let k_fs_wid = as_cf_string(ns_string!("fs_wid"));
    let owner_wid = space.get(k_fs_wid).and_then(|v| cf_positive_win_id(&v));
    let single_tile = owner_wid.is_some_and(|owner| {
        let Some(tlm) = space
            .get(as_cf_string(ns_string!("TileLayoutManager")))
            .and_then(|v| v.downcast::<CFDictionary>().ok())
        else {
            return false;
        };
        let tlm = unsafe { tlm.cast_unchecked::<CFString, CFType>() };
        let Some(tiles) = tlm
            .get(as_cf_string(ns_string!("TileSpaces")))
            .and_then(|v| v.downcast::<CFArray>().ok())
        else {
            return false;
        };
        if tiles.count() != 1 {
            return false;
        }
        let tiles = unsafe { tiles.cast_unchecked::<CFType>() };
        tiles
            .iter()
            .next()
            .and_then(|item| item.downcast::<CFDictionary>().ok())
            .is_some_and(|tile| {
                let tile = unsafe { tile.cast_unchecked::<CFString, CFType>() };
                let fs = tile.get(k_fs_wid).and_then(|v| cf_positive_win_id(&v));
                let wid = tile
                    .get(as_cf_string(ns_string!("TileWindowID")))
                    .and_then(|v| cf_positive_win_id(&v));
                fs == Some(owner) && wid == Some(owner)
            })
    });
    SpaceKind::Fullscreen {
        owner_wid,
        single_tile,
    }
}

fn parse_space_entry(
    item: CFRetained<CFType>,
    k_id64: &CFString,
    k_type: &CFString,
    k_uuid: &CFString,
) -> Result<(SpaceId, String, SpaceKind), SpaceError> {
    let space = item
        .downcast::<CFDictionary>()
        .map_err(|_| SpaceError::new(SpaceErrorKind::QueryFailed, "non-dict space entry"))?;
    let space = unsafe { space.cast_unchecked::<CFString, CFType>() };
    let id = space
        .get(k_id64)
        .and_then(|v| cf_space_id(&v))
        .ok_or_else(|| SpaceError::new(SpaceErrorKind::QueryFailed, "invalid space id64"))?;
    let raw_type = space
        .get(k_type)
        .and_then(|v| cf_int_i64(&v))
        .and_then(|v| i32::try_from(v).ok())
        .ok_or_else(|| SpaceError::new(SpaceErrorKind::QueryFailed, "invalid space type"))?;
    let uuid = space
        .get(k_uuid)
        .and_then(|v| v.downcast_ref::<CFString>().map(ToString::to_string))
        .unwrap_or_default();
    let kind = match raw_type {
        0 => SpaceKind::Desktop,
        4 => parse_fullscreen_kind(space),
        other => SpaceKind::Other(other),
    };
    Ok((id, uuid, kind))
}

/// Reads and validates `SLSCopyManagedDisplaySpaces` for connection `cid`.
pub fn read_managed_display_spaces(cid: ConnID) -> Result<SpaceCensus, SpaceError> {
    let display_ids = active_display_ids();
    read_managed_display_spaces_for_displays(cid, &display_ids)
}

/// Reads and validates `SLSCopyManagedDisplaySpaces` reusing a pre-fetched
/// `CGGetActiveDisplayList` slice.
pub(crate) fn read_managed_display_spaces_for_displays(
    cid: ConnID,
    display_ids: &[CGDirectDisplayID],
) -> Result<SpaceCensus, SpaceError> {
    let raw_list = NonNull::new(unsafe { SLSCopyManagedDisplaySpaces(cid) })
        .map(|ptr| unsafe { CFRetained::from_raw(ptr) })
        .ok_or_else(|| {
            SpaceError::new(
                SpaceErrorKind::QueryFailed,
                "SLSCopyManagedDisplaySpaces returned null",
            )
        })?;
    let active_displays = active_display_table(cid, display_ids);
    let main_display = CGMainDisplayID();

    let k_disp_id = as_cf_string(ns_string!("Display Identifier"));
    let k_current = as_cf_string(ns_string!("Current Space"));
    let k_id64 = as_cf_string(ns_string!("id64"));
    let k_spaces = as_cf_string(ns_string!("Spaces"));
    let k_type = as_cf_string(ns_string!("type"));
    let k_uuid = as_cf_string(ns_string!("uuid"));

    let raw_displays = raw_list
        .iter()
        .map(|entry| {
            let display_dict = entry.downcast::<CFDictionary>().map_err(|_| {
                SpaceError::new(SpaceErrorKind::QueryFailed, "non-dict display entry")
            })?;
            let display_dict = unsafe { display_dict.cast_unchecked::<CFString, CFType>() };
            let identifier = display_dict
                .get(k_disp_id)
                .and_then(|v| v.downcast_ref::<CFString>().map(ToString::to_string))
                .ok_or_else(|| {
                    SpaceError::new(SpaceErrorKind::QueryFailed, "missing Display Identifier")
                })?;
            let current_dict = display_dict
                .get(k_current)
                .and_then(|v| v.downcast::<CFDictionary>().ok())
                .ok_or_else(|| {
                    SpaceError::new(SpaceErrorKind::QueryFailed, "missing Current Space")
                })?;
            let current_dict = unsafe { current_dict.cast_unchecked::<CFString, CFType>() };
            let current_space = current_dict
                .get(k_id64)
                .and_then(|v| cf_space_id(&v))
                .ok_or_else(|| {
                    SpaceError::new(SpaceErrorKind::QueryFailed, "missing Current Space id64")
                })?;
            let display_id = resolve_skylight_display_id(
                &identifier,
                current_space.get(),
                main_display,
                &active_displays,
            )
            .ok_or_else(|| {
                SpaceError::new(
                    SpaceErrorKind::QueryFailed,
                    format!("unresolved display identifier '{identifier}'"),
                )
            })?;

            let spaces_arr = display_dict
                .get(k_spaces)
                .and_then(|v| v.downcast::<CFArray>().ok())
                .ok_or_else(|| {
                    SpaceError::new(SpaceErrorKind::QueryFailed, "missing Spaces array")
                })?;
            let spaces_arr = unsafe { spaces_arr.cast_unchecked::<CFType>() };
            let spaces = spaces_arr
                .iter()
                .map(|item| parse_space_entry(item, k_id64, k_type, k_uuid))
                .collect::<Result<Vec<_>, SpaceError>>()?;
            Ok(RawDisplaySpaces {
                display_id,
                skylight_display: identifier,
                current_space,
                spaces,
            })
        })
        .collect::<Result<Vec<_>, SpaceError>>()?;

    SpaceCensus::from_raw(raw_displays)
}

fn parse_window_meta_dict(
    dict: &CFDictionary<CFString, CFType>,
) -> Option<(WinID, WindowServerMeta)> {
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
}

fn all_window_meta() -> Option<HashMap<WinID, WindowServerMeta>> {
    let list = CGWindowListCopyWindowInfo(CGWindowListOption::OptionAll, kCGNullWindowID)?;
    let array = unsafe { list.cast_unchecked::<CFType>() };
    let mut map = HashMap::with_capacity(array.len());
    map.extend(array.iter().filter_map(|item| {
        let dict = item.downcast::<CFDictionary>().ok()?;
        let dict = unsafe { dict.cast_unchecked::<CFString, CFType>() };
        parse_window_meta_dict(dict)
    }));
    Some(map)
}

struct SlsWindowIter(CFRetained<CFType>);

impl SlsWindowIter {
    fn query(cid: ConnID, windows: &[WinID]) -> Option<Self> {
        let count = isize::try_from(windows.len()).ok()?;
        if count == 0 {
            return None;
        }
        let arr = win_ids_ns_array(windows);
        let query_ptr =
            NonNull::new(unsafe { SLSWindowQueryWindows(cid, as_cf_array(&arr), count) })?;
        let query = unsafe { CFRetained::from_raw(query_ptr) };
        let iter_ptr =
            NonNull::new(unsafe { SLSWindowQueryResultCopyWindows(query.deref().into()) })?;
        Some(Self(unsafe { CFRetained::from_raw(iter_ptr) }))
    }
}

impl Iterator for SlsWindowIter {
    type Item = (WinID, u64);

    fn next(&mut self) -> Option<Self::Item> {
        if !unsafe { SLSWindowIteratorAdvance(&raw const *self.0) } {
            return None;
        }
        let wid = unsafe { SLSWindowIteratorGetWindowID(&raw const *self.0) };
        let tags = unsafe { SLSWindowIteratorGetTags(&raw const *self.0) }.cast_unsigned();
        Some((wid, tags))
    }
}

#[derive(Default)]
struct OperationCache {
    show_spaces: OnceCell<Result<&'static AnyClass, SpaceError>>,
    hide_spaces: OnceCell<Result<&'static AnyClass, SpaceError>>,
    set_current_space: OnceCell<Result<&'static AnyClass, SpaceError>>,
    create_space: OnceCell<Result<&'static AnyClass, SpaceError>>,
    destroy_space: OnceCell<Result<&'static AnyClass, SpaceError>>,
    move_windows: OnceCell<Result<&'static AnyClass, SpaceError>>,
    add_windows: OnceCell<Result<&'static AnyClass, SpaceError>>,
    assign_all: OnceCell<Result<&'static AnyClass, SpaceError>>,
    assign_space: OnceCell<Result<&'static AnyClass, SpaceError>>,
    move_space_index: OnceCell<Result<&'static AnyClass, SpaceError>>,
}

/// Real main-thread `SkyLight` + `WMBridge` implementation.
pub struct DesktopBridgeOS {
    cid: ConnID,
    _mtm: MainThreadMarker,
    ops: OperationCache,
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
        Ok(Self {
            cid,
            _mtm: mtm,
            ops: OperationCache::default(),
        })
    }

    fn dispatch_async_cls(
        cls: &'static AnyClass,
        class_name: &'static CStr,
        applied: &mut bool,
        build: impl FnOnce(&'static AnyClass) -> Option<Retained<AnyObject>>,
    ) -> Result<(), SpaceError> {
        let op = build(cls).ok_or_else(|| {
            SpaceError::new(
                SpaceErrorKind::OperationFailed,
                format!("{} initializer returned nil", class_name.to_string_lossy()),
            )
            .with_applied(*applied)
        })?;
        *applied = true;
        let (): () = unsafe { msg_send![&*op, performWithWMBridgeDelegate] };
        Ok(())
    }

    fn run_mutation<T>(
        &self,
        f: impl FnOnce(&mut bool) -> Result<T, SpaceError>,
    ) -> Result<T, SpaceError> {
        self.session_unlocked()?;
        let mut applied = false;
        guarded(|| f(&mut applied)).map_err(|e| e.with_applied(applied))
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
            let windows = win_ids_ns_array(&[window_id]);
            let ptr = unsafe {
                SLSCopySpacesForWindows(self.cid, ALL_USER_SPACES_MASK, as_cf_array(&windows))
            };
            let Some(ptr) = NonNull::new(ptr) else {
                return Err(SpaceError::new(
                    SpaceErrorKind::QueryFailed,
                    "SLSCopySpacesForWindows returned null",
                ));
            };
            let arr = unsafe { CFRetained::from_raw(ptr) };
            Ok(arr.iter().filter_map(|v| cf_space_id(&v)).collect())
        })
    }

    fn windows_in_space(
        &self,
        space_id: SpaceId,
        include_minimized: bool,
    ) -> Result<Vec<WinID>, SpaceError> {
        guarded(|| {
            let spaces = space_ids_ns_array(&[space_id]);
            let (mut set_tags, mut clear_tags) = (0i64, 0i64);
            let options = if include_minimized {
                ALL_USER_SPACES_MASK
            } else {
                VISIBLE_SPACES_MASK
            };
            let ptr = unsafe {
                SLSCopyWindowsWithOptionsAndTags(
                    self.cid,
                    0,
                    as_cf_array(&spaces),
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
            let raw_ids = unsafe { raw_cf.cast_unchecked::<CFType>() };
            if raw_ids.is_empty() {
                return Ok(Vec::new());
            }
            let meta_map = all_window_meta().ok_or_else(|| {
                SpaceError::new(
                    SpaceErrorKind::QueryFailed,
                    "CGWindowListCopyWindowInfo returned null",
                )
            })?;
            raw_ids
                .iter()
                .filter_map(|item| {
                    let Some(wid) = cf_positive_win_id(&item) else {
                        return include_minimized.then(|| {
                            Err(SpaceError::new(
                                SpaceErrorKind::QueryFailed,
                                "unparseable window id in SLSCopyWindowsWithOptionsAndTags",
                            ))
                        });
                    };
                    match meta_map.get(&wid) {
                        Some(meta) if meta.is_app_window() => Some(Ok(wid)),
                        // Fail closed if WindowServer reports a window with missing metadata.
                        None if include_minimized => Some(Err(SpaceError::new(
                            SpaceErrorKind::QueryFailed,
                            format!("missing CGWindow metadata for window {wid}"),
                        ))),
                        Some(_) | None => None,
                    }
                })
                .collect()
        })
    }

    fn window_is_sticky(&self, window_id: WinID) -> Option<bool> {
        SlsWindowIter::query(self.cid, &[window_id])?
            .find(|&(wid, _)| wid == window_id)
            .map(|(_, tags)| (tags & STICKY_WINDOW_TAG) != 0)
    }

    fn sticky_windows(&self, windows: &[WinID]) -> HashMap<WinID, bool> {
        SlsWindowIter::query(self.cid, windows)
            .map(|iter| {
                iter.map(|(wid, tags)| (wid, (tags & STICKY_WINDOW_TAG) != 0))
                    .collect()
            })
            .unwrap_or_default()
    }

    fn associated_windows(&self, window_id: WinID) -> Result<Vec<WinID>, SpaceError> {
        let Some(ptr) = NonNull::new(unsafe { SLSCopyAssociatedWindows(self.cid, window_id) })
        else {
            return Err(SpaceError::new(
                SpaceErrorKind::QueryFailed,
                format!("SLSCopyAssociatedWindows returned null for window {window_id}"),
            ));
        };
        let arr = unsafe { CFRetained::from_raw(ptr) };
        Ok(arr.iter().filter_map(|v| cf_positive_win_id(&v)).collect())
    }

    fn window_meta(&self, window_id: WinID) -> Option<WindowServerMeta> {
        let wid_u32 = u32::try_from(window_id).ok()?;
        let list = CGWindowListCopyWindowInfo(CGWindowListOption::OptionIncludingWindow, wid_u32)?;
        let array = unsafe { list.cast_unchecked::<CFType>() };
        array.iter().find_map(|item| {
            let dict = item.downcast::<CFDictionary>().ok()?;
            let dict = unsafe { dict.cast_unchecked::<CFString, CFType>() };
            let (got_wid, meta) = parse_window_meta_dict(dict)?;
            (got_wid == window_id).then_some(meta)
        })
    }

    fn windows_for_pid(&self, pid: Pid) -> Vec<WinID> {
        let Some(list) = CGWindowListCopyWindowInfo(CGWindowListOption::OptionAll, kCGNullWindowID)
        else {
            return Vec::new();
        };
        let array = unsafe { list.cast_unchecked::<CFType>() };
        array
            .iter()
            .filter_map(|item| {
                let dict = item.downcast::<CFDictionary>().ok()?;
                let dict = unsafe { dict.cast_unchecked::<CFString, CFType>() };
                let owner_pid = dict
                    .get(unsafe { kCGWindowOwnerPID })
                    .and_then(|v| cf_int_i64(&v))
                    .and_then(|v| i32::try_from(v).ok())?;
                if owner_pid != pid {
                    return None;
                }
                let (wid, meta) = parse_window_meta_dict(dict)?;
                (wid > 0 && meta.is_app_window()).then_some(wid)
            })
            .collect()
    }

    fn activate_space(
        &mut self,
        skylight_display: &str,
        target: SpaceId,
        hide: &[SpaceId],
    ) -> Result<(), SpaceError> {
        self.run_mutation(|applied| {
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
            // Resolve & ABI-validate all 3 classes once (cached) before dispatching.
            let show_cls = resolve_cached(&self.ops.show_spaces, &show_spec)?;
            let hide_cls = resolve_cached(&self.ops.hide_spaces, &hide_spec)?;
            let set_cls = resolve_cached(&self.ops.set_current_space, &set_spec)?;

            let show_arr = space_ids_ns_array(&[target]);
            Self::dispatch_async_cls(show_cls, show_spec.class_name, applied, |cls| {
                let alloc: Allocated<AnyObject> = unsafe { msg_send![cls, alloc] };
                unsafe { msg_send![alloc, initWithSpaces: &*show_arr] }
            })?;

            if !hide.is_empty() {
                let hide_arr = space_ids_ns_array(hide);
                Self::dispatch_async_cls(hide_cls, hide_spec.class_name, applied, |cls| {
                    let alloc: Allocated<AnyObject> = unsafe { msg_send![cls, alloc] };
                    unsafe { msg_send![alloc, initWithSpaces: &*hide_arr] }
                })?;
            }

            let disp_ns = NSString::from_str(skylight_display);
            let target_raw = target.get();
            Self::dispatch_async_cls(set_cls, set_spec.class_name, applied, |cls| {
                let alloc: Allocated<AnyObject> = unsafe { msg_send![cls, alloc] };
                unsafe {
                    msg_send![alloc, initWithDisplayIdentifier: &*disp_ns, spaceID: target_raw]
                }
            })
        })
    }

    fn create_desktop(&mut self) -> Result<SpaceId, SpaceError> {
        self.run_mutation(|applied| {
            let spec = OperationSpec {
                class_name: c"SLSBridgedSpaceCreateOperation",
                init_sel: sel!(initWithOptions:values:),
                init_args: &[c"I", c"@"],
                perform_ret: c"@",
            };
            let cls = resolve_cached(&self.ops.create_space, &spec)?;
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
            *applied = true;
            let res: Option<Retained<AnyObject>> =
                unsafe { msg_send![&*op, performWithWMBridgeDelegate] };
            let res = res.ok_or_else(|| {
                SpaceError::new(
                    SpaceErrorKind::OperationFailed,
                    "SLSBridgedSpaceCreateOperation result was nil",
                )
            })?;
            if !method_matches(res.class(), sel!(spaceID), c"Q", &[]) {
                return Err(SpaceError::new(
                    SpaceErrorKind::Unsupported,
                    "space create result missing -spaceID:Q",
                ));
            }
            let raw_id: u64 = unsafe { msg_send![&*res, spaceID] };
            SpaceId::new(raw_id).ok_or_else(|| {
                SpaceError::new(SpaceErrorKind::OperationFailed, "created spaceID was 0")
            })
        })
    }

    fn destroy_space(&mut self, space_id: SpaceId) -> Result<(), SpaceError> {
        let raw_id = space_id.get();
        self.run_mutation(|applied| {
            let spec = OperationSpec {
                class_name: c"SLSBridgedSpaceDestroyOperation",
                init_sel: sel!(initWithSpaceID:),
                init_args: &[c"Q"],
                perform_ret: c"v",
            };
            let cls = resolve_cached(&self.ops.destroy_space, &spec)?;
            Self::dispatch_async_cls(cls, spec.class_name, applied, |cls| {
                let alloc: Allocated<AnyObject> = unsafe { msg_send![cls, alloc] };
                unsafe { msg_send![alloc, initWithSpaceID: raw_id] }
            })
        })
    }

    fn move_windows_to_space(
        &mut self,
        windows: &[WinID],
        space_id: SpaceId,
    ) -> Result<(), SpaceError> {
        if windows.is_empty() {
            return Ok(());
        }
        let raw_id = space_id.get();
        self.run_mutation(|applied| {
            let spec = OperationSpec {
                class_name: c"SLSBridgedMoveWindowsToManagedSpaceOperation",
                init_sel: sel!(initWithWindows:spaceID:),
                init_args: &[c"@", c"Q"],
                perform_ret: c"v",
            };
            let cls = resolve_cached(&self.ops.move_windows, &spec)?;
            let win_arr = win_ids_ns_array(windows);
            Self::dispatch_async_cls(cls, spec.class_name, applied, |cls| {
                let alloc: Allocated<AnyObject> = unsafe { msg_send![cls, alloc] };
                unsafe { msg_send![alloc, initWithWindows: &*win_arr, spaceID: raw_id] }
            })
        })
    }

    fn add_windows_to_space(
        &mut self,
        windows: &[WinID],
        space_id: SpaceId,
        remove_others: bool,
    ) -> Result<(), SpaceError> {
        if windows.is_empty() {
            return Ok(());
        }
        let raw_id = space_id.get();
        let options: u32 = if remove_others {
            REMOVE_FROM_OTHER_SPACES_OPT
        } else {
            0
        };
        self.run_mutation(|applied| {
            let spec = OperationSpec {
                class_name: c"SLSBridgedSpaceAddWindowsAndRemoveFromSpacesOperation",
                init_sel: sel!(initWithSpaceID:windows:options:),
                init_args: &[c"Q", c"@", c"I"],
                perform_ret: c"v",
            };
            let cls = resolve_cached(&self.ops.add_windows, &spec)?;
            let win_arr = win_ids_ns_array(windows);
            Self::dispatch_async_cls(cls, spec.class_name, applied, |cls| {
                let alloc: Allocated<AnyObject> = unsafe { msg_send![cls, alloc] };
                unsafe {
                    msg_send![
                        alloc,
                        initWithSpaceID: raw_id,
                        windows: &*win_arr,
                        options: options
                    ]
                }
            })
        })
    }

    fn assign_process(&mut self, pid: Pid, mode: AssignMode) -> Result<(), SpaceError> {
        self.run_mutation(|applied| match mode {
            AssignMode::AllSpaces => {
                let spec = OperationSpec {
                    class_name: c"SLSBridgedProcessAssignToAllSpacesOperation",
                    init_sel: sel!(initWithProcess:),
                    init_args: &[c"i"],
                    perform_ret: c"v",
                };
                let cls = resolve_cached(&self.ops.assign_all, &spec)?;
                Self::dispatch_async_cls(cls, spec.class_name, applied, |cls| {
                    let alloc: Allocated<AnyObject> = unsafe { msg_send![cls, alloc] };
                    unsafe { msg_send![alloc, initWithProcess: pid] }
                })
            }
            AssignMode::Space(_) | AssignMode::Clear => {
                let raw_id = match mode {
                    AssignMode::Space(space_id) => space_id.get(),
                    AssignMode::Clear | AssignMode::AllSpaces => 0u64,
                };
                let spec = OperationSpec {
                    class_name: c"SLSBridgedProcessAssignToSpaceOperation",
                    init_sel: sel!(initWithProcess:spaceID:),
                    init_args: &[c"i", c"Q"],
                    perform_ret: c"v",
                };
                let cls = resolve_cached(&self.ops.assign_space, &spec)?;
                Self::dispatch_async_cls(cls, spec.class_name, applied, |cls| {
                    let alloc: Allocated<AnyObject> = unsafe { msg_send![cls, alloc] };
                    unsafe { msg_send![alloc, initWithProcess: pid, spaceID: raw_id] }
                })
            }
        })
    }

    fn move_space_to_index(
        &mut self,
        space_id: SpaceId,
        skylight_display: &str,
        index: u32,
    ) -> Result<(), SpaceError> {
        let raw_id = space_id.get();
        self.run_mutation(|applied| {
            let spec = OperationSpec {
                class_name: c"SLSBridgedMoveManagedSpaceToDisplayIndexOperation",
                init_sel: sel!(initWithSpaceID:displayIdentifier:index:),
                init_args: &[c"Q", c"@", c"I"],
                perform_ret: c"v",
            };
            let cls = resolve_cached(&self.ops.move_space_index, &spec)?;
            let disp_ns = NSString::from_str(skylight_display);
            Self::dispatch_async_cls(cls, spec.class_name, applied, |cls| {
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
            if session_flag(live_dict, ns_string!("kCGSSessionOnConsoleKey")) == Ok(Some(true)) {
                assert!(
                    parse_session_locked(live_dict).is_ok(),
                    "active console login session must not fail with NoGuiSession"
                );
            }
        }
    }

    #[test]
    fn win_ids_ns_array_boxes_signed_32bit_numbers() {
        let arr = win_ids_ns_array(&[42]);
        let cf_arr = unsafe { as_cf_array(&arr).cast_unchecked::<CFType>() };
        let first = cf_arr.iter().next().expect("one element");
        let num = first.downcast_ref::<CFNumber>().expect("CFNumber");
        assert_eq!(num.as_i32(), Some(42));
        assert_eq!(
            num.r#type(),
            objc2_core_foundation::CFNumberType::SInt32Type
        );
    }
}

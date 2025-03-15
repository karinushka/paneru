use std::{ffi::c_void, ptr::NonNull};

use accessibility_sys::AXUIElementRef;
use objc2_core_foundation::{CFArray, CFMutableData, CFString, CFType, CFUUID, CGPoint, CGRect};
use objc2_core_graphics::{CGDirectDisplayID, CGError};

use crate::platform::{CFStringRef, ProcessSerialNumber};

pub type OSStatus = i32;
pub type WinID = i32;
pub type ConnID = i64;

#[link(name = "SkyLight", kind = "framework")]
unsafe extern "C" {

    // extern AXError _AXUIElementGetWindow(AXUIElementRef ref, uint32_t *wid);
    pub fn _AXUIElementGetWindow(element: AXUIElementRef, wid: &mut WinID) -> OSStatus;
    // extern int SLSMainConnectionID(void);
    pub fn SLSMainConnectionID() -> ConnID;

    // extern CGError SLSGetWindowBounds(int cid, uint32_t wid, CGRect *frame);
    pub fn SLSGetWindowBounds(cid: ConnID, window_id: WinID, frame: &mut CGRect) -> CGError;
    // extern CFStringRef SLSCopyManagedDisplayForWindow(int cid, uint32_t wid);
    pub fn SLSCopyManagedDisplayForWindow(cid: ConnID, window_id: WinID) -> CFStringRef;
    // extern CFStringRef SLSCopyBestManagedDisplayForRect(int cid, CGRect rect);
    pub fn SLSCopyBestManagedDisplayForRect(cid: ConnID, frame: CGRect) -> CFStringRef;
    // extern CFUUIDRef CGDisplayCreateUUIDFromDisplayID(uint32_t did);
    pub fn CGDisplayCreateUUIDFromDisplayID(display: CGDirectDisplayID) -> *mut CFUUID;
    pub fn CGDisplayGetDisplayIDFromUUID(display: &CFUUID) -> u32;
    // extern uint64_t SLSManagedDisplayGetCurrentSpace(int cid, CFStringRef uuid);
    pub fn SLSManagedDisplayGetCurrentSpace(cid: ConnID, uuid: CFStringRef) -> u64;
    // extern CFStringRef SLSCopyActiveMenuBarDisplayIdentifier(int cid);
    pub fn SLSCopyActiveMenuBarDisplayIdentifier(cid: ConnID) -> CFStringRef;
    // extern CFArrayRef SLSCopyWindowsWithOptionsAndTags(int cid, uint32_t owner, CFArrayRef spaces, uint32_t options, uint64_t *set_tags, uint64_t *clear_tags);
    pub fn SLSCopyWindowsWithOptionsAndTags(
        cid: ConnID,
        owner: ConnID,
        spaces: *const CFArray,
        options: i32,
        set_tags: &i64,
        clear_tags: &i64,
    ) -> *mut CFArray;
    // extern int SLSGetSpaceManagementMode(int cid);
    pub fn SLSGetSpaceManagementMode(cid: ConnID) -> i32;
    // extern CFArrayRef SLSCopyManagedDisplaySpaces(int cid);
    pub fn SLSCopyManagedDisplaySpaces(cid: ConnID) -> *mut CFArray;
    // extern CFArrayRef SLSCopyAssociatedWindows(int cid, uint32_t wid);
    pub fn SLSCopyAssociatedWindows(cid: ConnID, window_id: WinID) -> NonNull<CFArray>;
    // extern CFTypeRef SLSWindowQueryWindows(int cid, CFArrayRef windows, int count);
    pub fn SLSWindowQueryWindows(
        cid: ConnID,
        windows: *const CFArray,
        count: isize,
    ) -> NonNull<CFType>;
    // extern CFTypeRef SLSWindowQueryResultCopyWindows(CFTypeRef window_query);
    pub fn SLSWindowQueryResultCopyWindows(type_ref: NonNull<CFType>) -> NonNull<CFType>;
    // extern int SLSWindowIteratorGetCount(CFTypeRef iterator);
    pub fn SLSWindowIteratorGetCount(iterator: *const CFType) -> isize;
    // extern bool SLSWindowIteratorAdvance(CFTypeRef iterator);
    pub fn SLSWindowIteratorAdvance(iterator: *const CFType) -> bool;
    // extern uint32_t SLSWindowIteratorGetParentID(CFTypeRef iterator);
    pub fn SLSWindowIteratorGetParentID(iterator: *const CFType) -> i32;
    // extern uint32_t SLSWindowIteratorGetWindowID(CFTypeRef iterator);
    pub fn SLSWindowIteratorGetWindowID(iterator: *const CFType) -> i32;
    // extern uint64_t SLSWindowIteratorGetTags(CFTypeRef iterator);
    pub fn SLSWindowIteratorGetTags(iterator: *const CFType) -> i64;
    // extern uint64_t SLSWindowIteratorGetAttributes(CFTypeRef iterator);
    pub fn SLSWindowIteratorGetAttributes(iterator: *const CFType) -> i64;
    // extern int SLSWindowIteratorGetLevel(CFTypeRef iterator, int32_t index);
    // pub fn SLSWindowIteratorGetLevel(iterator: *const CFType, index: i32) -> isize;
    // extern OSStatus _SLPSGetFrontProcess(ProcessSerialNumber *psn);
    pub fn _SLPSGetFrontProcess(psn: &mut ProcessSerialNumber) -> OSStatus;
    // extern CGError SLSGetConnectionIDForPSN(int cid, ProcessSerialNumber *psn, int *psn_cid);
    pub fn SLSGetConnectionIDForPSN(
        cid: ConnID,
        psn: &ProcessSerialNumber,
        psn_cid: &mut ConnID,
    ) -> CGError;
    // extern CGError _SLPSSetFrontProcessWithOptions(ProcessSerialNumber *psn, uint32_t wid, uint32_t mode);
    pub fn _SLPSSetFrontProcessWithOptions(
        psn: &ProcessSerialNumber,
        window_id: WinID,
        mode: u32,
    ) -> CGError;
    // extern CGError SLPSPostEventRecordTo(ProcessSerialNumber *psn, uint8_t *bytes);
    pub fn SLPSPostEventRecordTo(psn: &ProcessSerialNumber, event: *const c_void) -> CGError;
    // extern OSStatus SLSFindWindowAndOwner(int cid, int zero, int one, int zero_again, CGPoint *screen_point, CGPoint *window_point, uint32_t *wid, int *wcid);
    pub fn SLSFindWindowAndOwner(
        cid: ConnID,
        filter_window_id: WinID,
        _: i64,
        _: i64,
        point: &CGPoint,
        window_point: &mut CGPoint,
        window_id: &mut WinID,
        window_cid: &mut ConnID,
    ) -> OSStatus;

    // extern CGError SLSGetCurrentCursorLocation(int cid, CGPoint *point);
    pub fn SLSGetCurrentCursorLocation(cid: ConnID, cursor: &mut CGPoint) -> CGError;

    pub fn AXUIElementCopyAttributeValue(
        element: AXUIElementRef,
        attribute: &CFString,
        value: *mut *mut CFType,
    ) -> i32;

    pub fn AXUIElementSetAttributeValue(
        element: AXUIElementRef,
        attribute: &CFString,
        value: &CFType,
    ) -> i32;

    pub fn AXUIElementPerformAction(element: AXUIElementRef, action: &CFString) -> i32;

    // extern AXUIElementRef _AXUIElementCreateWithRemoteToken(CFDataRef data);
    pub fn _AXUIElementCreateWithRemoteToken(data: &CFMutableData) -> AXUIElementRef;
}

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

    /// Retrieves the window ID (`WinID`) associated with an Accessibility UI element.
    ///
    /// # Arguments
    ///
    /// * `element` - An `AXUIElementRef` pointing to the UI element.
    /// * `wid` - A mutable reference to a `WinID` where the window ID will be stored.
    ///
    /// # Returns
    ///
    /// An `OSStatus` indicating success or failure.
    ///
    /// # Original signature
    /// extern `AXError` _AXUIElementGetWindow(AXUIElementRef ref, `uint32_t` *wid);
    pub fn _AXUIElementGetWindow(element: AXUIElementRef, wid: &mut WinID) -> OSStatus;

    /// Retrieves the main connection ID for the `SkyLight` API.
    ///
    /// # Returns
    ///
    /// A `ConnID` representing the main connection ID.
    ///
    /// # Original signature
    /// extern int SLSMainConnectionID(void);
    pub fn SLSMainConnectionID() -> ConnID;

    /// Retrieves the bounding rectangle (`CGRect`) of a window.
    ///
    /// # Arguments
    ///
    /// * `cid` - The connection ID.
    /// * `window_id` - The ID of the window.
    /// * `frame` - A mutable reference to a `CGRect` where the window bounds will be stored.
    ///
    /// # Returns
    ///
    /// A `CGError` indicating success or failure.
    ///
    /// # Original signature
    /// extern `CGError` SLSGetWindowBounds(int cid, `uint32_t` wid, `CGRect` *frame);
    pub fn SLSGetWindowBounds(cid: ConnID, window_id: WinID, frame: &mut CGRect) -> CGError;

    /// Copies the managed display identifier for a given window.
    ///
    /// # Arguments
    ///
    /// * `cid` - The connection ID.
    /// * `window_id` - The ID of the window.
    ///
    /// # Returns
    ///
    /// A `CFStringRef` representing the display identifier.
    ///
    /// # Original signature
    /// extern `CFStringRef` SLSCopyManagedDisplayForWindow(int cid, `uint32_t` wid);
    pub fn SLSCopyManagedDisplayForWindow(cid: ConnID, window_id: WinID) -> CFStringRef;

    /// Copies the best managed display identifier for a given rectangle.
    ///
    /// # Arguments
    ///
    /// * `cid` - The connection ID.
    /// * `frame` - The `CGRect` to find the best display for.
    ///
    /// # Returns
    ///
    /// A `CFStringRef` representing the best display identifier.
    ///
    /// # Original signature
    /// extern `CFStringRef` SLSCopyBestManagedDisplayForRect(int cid, `CGRect` rect);
    pub fn SLSCopyBestManagedDisplayForRect(cid: ConnID, frame: CGRect) -> CFStringRef;

    /// Creates a `CFUUIDRef` from a `CGDirectDisplayID`.
    ///
    /// # Arguments
    ///
    /// * `display` - The `CGDirectDisplayID` of the display.
    ///
    /// # Returns
    ///
    /// A raw pointer to a `CFUUID` if successful.
    ///
    /// # Original signature
    /// extern `CFUUIDRef` `CGDisplayCreateUUIDFromDisplayID`(`uint32_t` did);
    pub fn CGDisplayCreateUUIDFromDisplayID(display: CGDirectDisplayID) -> *mut CFUUID;

    /// Retrieves the `CGDirectDisplayID` from a `CFUUID`.
    ///
    /// # Arguments
    ///
    /// * `display` - A reference to the `CFUUID`.
    ///
    /// # Returns
    ///
    /// A `u32` representing the `CGDirectDisplayID`.
    pub fn CGDisplayGetDisplayIDFromUUID(display: &CFUUID) -> u32;

    /// Retrieves the current space ID for a managed display.
    ///
    /// # Arguments
    ///
    /// * `cid` - The connection ID.
    /// * `uuid` - A `CFStringRef` representing the display's UUID.
    ///
    /// # Returns
    ///
    /// A `u64` representing the current space ID.
    ///
    /// # Original signature
    /// extern `uint64_t` SLSManagedDisplayGetCurrentSpace(int cid, `CFStringRef` uuid);
    pub fn SLSManagedDisplayGetCurrentSpace(cid: ConnID, uuid: CFStringRef) -> u64;

    /// Copies the active menu bar display identifier.
    ///
    /// # Arguments
    ///
    /// * `cid` - The connection ID.
    ///
    /// # Returns
    ///
    /// A `CFStringRef` representing the active menu bar display identifier.
    ///
    /// # Original signature
    /// extern `CFStringRef` SLSCopyActiveMenuBarDisplayIdentifier(int cid);
    pub fn SLSCopyActiveMenuBarDisplayIdentifier(cid: ConnID) -> CFStringRef;

    /// Copies a list of windows with specified options and tags.
    ///
    /// # Arguments
    ///
    /// * `cid` - The connection ID.
    /// * `owner` - The owner connection ID (0 for all windows).
    /// * `spaces` - A raw pointer to a `CFArray` of space IDs.
    /// * `options` - An integer representing the query options.
    /// * `set_tags` - A mutable reference to an `i64` to store set tags.
    /// * `clear_tags` - A mutable reference to an `i64` to store clear tags.
    ///
    /// # Returns
    ///
    /// A raw pointer to a `CFArray` of window information.
    ///
    /// # Original signature
    /// extern `CFArrayRef` SLSCopyWindowsWithOptionsAndTags(int cid, `uint32_t` owner, `CFArrayRef` spaces, `uint32_t` options, `uint64_t` *`set_tags`, `uint64_t` *`clear_tags`);
    pub fn SLSCopyWindowsWithOptionsAndTags(
        cid: ConnID,
        owner: ConnID,
        spaces: *const CFArray,
        options: i32,
        set_tags: &mut i64,
        clear_tags: &mut i64,
    ) -> *mut CFArray;

    /// Retrieves the space management mode for a connection ID.
    ///
    /// # Arguments
    ///
    /// * `cid` - The connection ID.
    ///
    /// # Returns
    ///
    /// An `i32` representing the space management mode.
    ///
    /// # Original signature
    /// extern int SLSGetSpaceManagementMode(int cid);
    pub fn SLSGetSpaceManagementMode(cid: ConnID) -> i32;

    /// Copies a list of managed display spaces.
    ///
    /// # Arguments
    ///
    /// * `cid` - The connection ID.
    ///
    /// # Returns
    ///
    /// A raw pointer to a `CFArray` of managed display spaces.
    ///
    /// # Original signature
    /// extern `CFArrayRef` SLSCopyManagedDisplaySpaces(int cid);
    pub fn SLSCopyManagedDisplaySpaces(cid: ConnID) -> *mut CFArray;

    /// Copies a list of associated windows for a given window ID.
    ///
    /// # Arguments
    ///
    /// * `cid` - The connection ID.
    /// * `window_id` - The ID of the window.
    ///
    /// # Returns
    ///
    /// A `NonNull<CFArray>` containing associated windows.
    ///
    /// # Original signature
    /// extern `CFArrayRef` SLSCopyAssociatedWindows(int cid, `uint32_t` wid);
    pub fn SLSCopyAssociatedWindows(cid: ConnID, window_id: WinID) -> NonNull<CFArray>;

    /// Queries windows based on a provided `CFArray` of window IDs.
    ///
    /// # Arguments
    ///
    /// * `cid` - The connection ID.
    /// * `windows` - A raw pointer to a `CFArray` of window IDs.
    /// * `count` - The number of windows in the array.
    ///
    /// # Returns
    ///
    /// A `NonNull<CFType>` representing the window query result.
    ///
    /// # Original signature
    /// extern `CFTypeRef` SLSWindowQueryWindows(int cid, `CFArrayRef` windows, int count);
    pub fn SLSWindowQueryWindows(
        cid: ConnID,
        windows: *const CFArray,
        count: isize,
    ) -> NonNull<CFType>;

    /// Copies windows from a window query result.
    ///
    /// # Arguments
    ///
    /// * `type_ref` - A `NonNull<CFType>` representing the window query result.
    ///
    /// # Returns
    ///
    /// A `NonNull<CFType>` representing an iterator for the windows.
    ///
    /// # Original signature
    /// extern `CFTypeRef` SLSWindowQueryResultCopyWindows(CFTypeRef `window_query`);
    pub fn SLSWindowQueryResultCopyWindows(type_ref: NonNull<CFType>) -> NonNull<CFType>;

    /// Retrieves the count of windows in a window iterator.
    ///
    /// # Arguments
    ///
    /// * `iterator` - A raw pointer to a `CFType` representing the window iterator.
    ///
    /// # Returns
    ///
    /// An `isize` representing the count of windows.
    ///
    /// # Original signature
    /// extern int SLSWindowIteratorGetCount(CFTypeRef iterator);
    pub fn SLSWindowIteratorGetCount(iterator: *const CFType) -> isize;

    /// Advances the window iterator to the next window.
    ///
    /// # Arguments
    ///
    /// * `iterator` - A raw pointer to a `CFType` representing the window iterator.
    ///
    /// # Returns
    ///
    /// `true` if the iterator advanced successfully, `false` otherwise.
    ///
    /// # Original signature
    /// extern bool SLSWindowIteratorAdvance(CFTypeRef iterator);
    pub fn SLSWindowIteratorAdvance(iterator: *const CFType) -> bool;

    /// Retrieves the parent window ID from a window iterator.
    ///
    /// # Arguments
    ///
    /// * `iterator` - A raw pointer to a `CFType` representing the window iterator.
    ///
    /// # Returns
    ///
    /// An `i32` representing the parent window ID.
    ///
    /// # Original signature
    /// extern `uint32_t` SLSWindowIteratorGetParentID(CFTypeRef iterator);
    pub fn SLSWindowIteratorGetParentID(iterator: *const CFType) -> i32;

    /// Retrieves the window ID from a window iterator.
    ///
    /// # Arguments
    ///
    /// * `iterator` - A raw pointer to a `CFType` representing the window iterator.
    ///
    /// # Returns
    ///
    /// An `i32` representing the window ID.
    ///
    /// # Original signature
    /// extern `uint32_t` SLSWindowIteratorGetWindowID(CFTypeRef iterator);
    pub fn SLSWindowIteratorGetWindowID(iterator: *const CFType) -> i32;

    /// Retrieves the tags from a window iterator.
    ///
    /// # Arguments
    ///
    /// * `iterator` - A raw pointer to a `CFType` representing the window iterator.
    ///
    /// # Returns
    ///
    /// An `i64` representing the tags.
    ///
    /// # Original signature
    /// extern `uint64_t` SLSWindowIteratorGetTags(CFTypeRef iterator);
    pub fn SLSWindowIteratorGetTags(iterator: *const CFType) -> i64;

    /// Retrieves the attributes from a window iterator.
    ///
    /// # Arguments
    ///
    /// * `iterator` - A raw pointer to a `CFType` representing the window iterator.
    ///
    /// # Returns
    ///
    /// An `i64` representing the attributes.
    ///
    /// # Original signature
    /// extern `uint64_t` SLSWindowIteratorGetAttributes(CFTypeRef iterator);
    pub fn SLSWindowIteratorGetAttributes(iterator: *const CFType) -> i64;

    // extern int SLSWindowIteratorGetLevel(CFTypeRef iterator, int32_t index);
    // pub fn SLSWindowIteratorGetLevel(iterator: *const CFType, index: i32) -> isize;

    /// Retrieves the frontmost process's `ProcessSerialNumber`.
    ///
    /// # Arguments
    ///
    /// * `psn` - A mutable reference to a `ProcessSerialNumber` where the front process's PSN will be stored.
    ///
    /// # Returns
    ///
    /// An `OSStatus` indicating success or failure.
    ///
    /// # Original signature
    /// extern `OSStatus` _SLPSGetFrontProcess(ProcessSerialNumber *psn);
    pub fn _SLPSGetFrontProcess(psn: &mut ProcessSerialNumber) -> OSStatus;

    /// Retrieves the connection ID for a given `ProcessSerialNumber`.
    ///
    /// # Arguments
    ///
    /// * `cid` - The main connection ID.
    /// * `psn` - A reference to the `ProcessSerialNumber`.
    /// * `psn_cid` - A mutable reference to a `ConnID` where the process's connection ID will be stored.
    ///
    /// # Returns
    ///
    /// A `CGError` indicating success or failure.
    ///
    /// # Original signature
    /// extern `CGError` SLSGetConnectionIDForPSN(int cid, `ProcessSerialNumber` *psn, int *`psn_cid`);
    pub fn SLSGetConnectionIDForPSN(
        cid: ConnID,
        psn: &ProcessSerialNumber,
        psn_cid: &mut ConnID,
    ) -> CGError;

    /// Sets the frontmost process with additional options and a target window ID.
    ///
    /// # Arguments
    ///
    /// * `psn` - A reference to the `ProcessSerialNumber` of the process to bring to front.
    /// * `window_id` - The ID of the window to focus within the process.
    /// * `mode` - A `u32` representing the activation mode.
    ///
    /// # Returns
    ///
    /// A `CGError` indicating success or failure.
    ///
    /// # Original signature
    /// extern `CGError` _SLPSSetFrontProcessWithOptions(ProcessSerialNumber *psn, `uint32_t` wid, `uint32_t` mode);
    pub fn _SLPSSetFrontProcessWithOptions(
        psn: &ProcessSerialNumber,
        window_id: WinID,
        mode: u32,
    ) -> CGError;

    /// Posts an event record to a target `ProcessSerialNumber`.
    ///
    /// # Arguments
    ///
    /// * `psn` - A reference to the `ProcessSerialNumber` of the target process.
    /// * `event` - A raw pointer to the event data.
    ///
    /// # Returns
    ///
    /// A `CGError` indicating success or failure.
    ///
    /// # Original signature
    /// extern `CGError` SLPSPostEventRecordTo(ProcessSerialNumber *psn, `uint8_t` *bytes);
    pub fn SLPSPostEventRecordTo(psn: &ProcessSerialNumber, event: *const c_void) -> CGError;

    /// Finds a window and its owner at a specified screen point.
    ///
    /// # Arguments
    ///
    /// * `cid` - The connection ID.
    /// * `filter_window_id` - A window ID to filter the search (0 for no filter).
    /// * `_` - Two unused `i64` arguments.
    /// * `point` - A reference to a `CGPoint` representing the screen coordinate.
    /// * `window_point` - A mutable reference to a `CGPoint` to store the window-relative coordinate.
    /// * `window_id` - A mutable reference to a `WinID` to store the found window's ID.
    /// * `window_cid` - A mutable reference to a `ConnID` to store the found window's connection ID.
    ///
    /// # Returns
    ///
    /// An `OSStatus` indicating success or failure.
    ///
    /// # Original signature
    /// extern `OSStatus` SLSFindWindowAndOwner(int cid, int zero, int one, int `zero_again`, `CGPoint` *`screen_point`, `CGPoint` *`window_point`, `uint32_t` *wid, int *wcid);
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

    /// Retrieves the current cursor location.
    ///
    /// # Arguments
    ///
    /// * `cid` - The connection ID.
    /// * `cursor` - A mutable reference to a `CGPoint` where the cursor location will be stored.
    ///
    /// # Returns
    ///
    /// A `CGError` indicating success or failure.
    ///
    /// # Original signature
    /// extern `CGError` SLSGetCurrentCursorLocation(int cid, `CGPoint` *point);
    pub fn SLSGetCurrentCursorLocation(cid: ConnID, cursor: &mut CGPoint) -> CGError;

    /// Copies the value of an accessibility attribute from a UI element.
    ///
    /// # Arguments
    ///
    /// * `element` - An `AXUIElementRef` pointing to the UI element.
    /// * `attribute` - A reference to a `CFString` representing the attribute name.
    /// * `value` - A mutable reference to a raw pointer to a `CFType` where the attribute value will be stored.
    ///
    /// # Returns
    ///
    /// An `i32` indicating success or failure.
    pub fn AXUIElementCopyAttributeValue(
        element: AXUIElementRef,
        attribute: &CFString,
        value: &mut *mut CFType,
    ) -> i32;

    /// Sets the value of an accessibility attribute for a UI element.
    ///
    /// # Arguments
    ///
    /// * `element` - An `AXUIElementRef` pointing to the UI element.
    /// * `attribute` - A reference to a `CFString` representing the attribute name.
    /// * `value` - A reference to a `CFType` representing the new attribute value.
    ///
    /// # Returns
    ///
    /// An `i32` indicating success or failure.
    pub fn AXUIElementSetAttributeValue(
        element: AXUIElementRef,
        attribute: &CFString,
        value: &CFType,
    ) -> i32;

    /// Performs an action on an accessibility UI element.
    ///
    /// # Arguments
    ///
    /// * `element` - An `AXUIElementRef` pointing to the UI element.
    /// * `action` - A reference to a `CFString` representing the action to perform.
    ///
    /// # Returns
    ///
    /// An `i32` indicating success or failure.
    pub fn AXUIElementPerformAction(element: AXUIElementRef, action: &CFString) -> i32;

    /// Creates an `AXUIElementRef` from a remote token (`CFDataRef`).
    /// This is often used to get an `AXUIElementRef` for windows on inactive spaces.
    ///
    /// # Arguments
    ///
    /// * `data` - A reference to a `CFMutableData` containing the remote token.
    ///
    /// # Returns
    ///
    /// An `AXUIElementRef` if successful.
    ///
    /// # Original signature
    /// extern `AXUIElementRef` _AXUIElementCreateWithRemoteToken(CFDataRef data);
    pub fn _AXUIElementCreateWithRemoteToken(data: &CFMutableData) -> AXUIElementRef;
}

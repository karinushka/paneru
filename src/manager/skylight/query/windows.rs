use std::ptr::NonNull;

use objc2_core_foundation::{CFArray, CFNumber, CFNumberType, CFRetained, CFType, CGRect, CGSize};

use crate::platform::{ConnID, Pid, ProcessSerialNumber, WinID};
use crate::util::create_array;

use super::{
    SLSWindowIteratorAdvance, SLSWindowIteratorGetAlpha, SLSWindowIteratorGetAttachedWindowCount,
    SLSWindowIteratorGetAttributes, SLSWindowIteratorGetBounds,
    SLSWindowIteratorGetCornerMaskFlags, SLSWindowIteratorGetCornerRadii,
    SLSWindowIteratorGetCount, SLSWindowIteratorGetCurrentConstraints,
    SLSWindowIteratorGetCurrentMatchingSpaceID, SLSWindowIteratorGetFrameBounds,
    SLSWindowIteratorGetLastNonEmptyFrameBounds, SLSWindowIteratorGetLevel,
    SLSWindowIteratorGetOwner, SLSWindowIteratorGetPID, SLSWindowIteratorGetPSN,
    SLSWindowIteratorGetParentID, SLSWindowIteratorGetResolvedCornerRadii,
    SLSWindowIteratorGetScreenRect, SLSWindowIteratorGetSpaceAttributes,
    SLSWindowIteratorGetSpaceCount, SLSWindowIteratorGetSpaceTypeMask, SLSWindowIteratorGetTags,
    SLSWindowIteratorGetWindowID, SLSWindowQueryConstraints, SLSWindowQueryMatchingSpace,
    SLSWindowQueryWindowsRaw,
};

pub struct SLSWindowQueryWindows {
    pub(super) iterator: CFRetained<CFType>,
    pub(super) count: usize,
    pub(super) current_index: Option<u64>,
}

pub struct SLSWindowQueryWindow<'a> {
    iterator: &'a SLSWindowQueryWindows,
}

impl SLSWindowQueryWindows {
    pub fn from_window_list(cid: ConnID, windows: &[WinID]) -> Option<Self> {
        let windows = create_array(windows, CFNumberType::SInt32Type).ok()?;
        Some(Self {
            iterator: unsafe {
                CFRetained::from_raw(SLSWindowQueryWindowsRaw(
                    cid,
                    windows.as_ref() as *const CFArray,
                    windows.count() as isize,
                ))
            },
            count: windows.count() as usize,
            current_index: None,
        })
    }

    #[inline]
    fn as_ptr(&self) -> *const CFType {
        &raw const *self.iterator
    }

    #[inline]
    fn index(&self) -> u64 {
        self.current_index
            .expect("window query row accessed before advance()")
    }

    pub fn count(&self) -> usize {
        self.count
    }

    pub fn remaining(&self) -> usize {
        self.current_index.map_or(self.count, |index| {
            self.count.saturating_sub(index as usize + 1)
        })
    }

    pub fn is_empty(&self) -> bool {
        self.count == 0
    }

    pub fn current_index(&self) -> Option<u64> {
        self.current_index
    }

    pub fn advance(&mut self) -> bool {
        if !unsafe { SLSWindowIteratorAdvance(self.as_ptr()) } {
            self.current_index = None;
            return false;
        }

        self.current_index = Some(self.current_index.map_or(0, |index| index + 1));
        true
    }

    pub fn current(&self) -> Option<SLSWindowQueryWindow<'_>> {
        self.current_index
            .map(|_| SLSWindowQueryWindow { iterator: self })
    }

    pub fn into_inner(self) -> CFRetained<CFType> {
        self.iterator
    }

    pub fn as_inner(&self) -> &CFType {
        self.iterator.as_ref()
    }
}

impl SLSWindowQueryWindow<'_> {
    #[inline]
    fn as_ptr(&self) -> *const CFType {
        self.iterator.as_ptr()
    }

    #[inline]
    fn index(&self) -> u64 {
        self.iterator.index()
    }

    pub fn window_id(&self) -> WinID {
        unsafe { SLSWindowIteratorGetWindowID(self.as_ptr(), self.index()) }
    }

    pub fn parent_id(&self) -> WinID {
        unsafe { SLSWindowIteratorGetParentID(self.as_ptr(), self.index()) }
    }

    pub fn owner(&self) -> ConnID {
        unsafe { SLSWindowIteratorGetOwner(self.as_ptr(), self.index()) }
    }

    pub fn pid(&self) -> Pid {
        unsafe { SLSWindowIteratorGetPID(self.as_ptr(), self.index()) }
    }

    pub fn psn(&self) -> ProcessSerialNumber {
        unsafe { SLSWindowIteratorGetPSN(self.as_ptr(), self.index()) }
    }

    pub fn tags(&self) -> i64 {
        unsafe { SLSWindowIteratorGetTags(self.as_ptr(), self.index()) }
    }

    pub fn attributes(&self) -> i64 {
        unsafe { SLSWindowIteratorGetAttributes(self.as_ptr(), self.index()) }
    }

    pub fn alpha(&self) -> f32 {
        unsafe { SLSWindowIteratorGetAlpha(self.as_ptr(), self.index()) }
    }

    pub fn level(&self) -> i32 {
        unsafe { SLSWindowIteratorGetLevel(self.as_ptr(), self.index()) }
    }

    pub fn attached_window_count(&self) -> u32 {
        unsafe { SLSWindowIteratorGetAttachedWindowCount(self.as_ptr(), self.index()) }
    }

    pub fn space_count(&self) -> u32 {
        unsafe { SLSWindowIteratorGetSpaceCount(self.as_ptr(), self.index()) }
    }

    pub fn space_attributes(&self) -> u64 {
        unsafe { SLSWindowIteratorGetSpaceAttributes(self.as_ptr(), self.index()) }
    }

    pub fn space_type_mask(&self) -> u64 {
        unsafe { SLSWindowIteratorGetSpaceTypeMask(self.as_ptr(), self.index()) }
    }

    pub fn corner_mask_flags(&self) -> u32 {
        unsafe { SLSWindowIteratorGetCornerMaskFlags(self.as_ptr(), self.index()) }
    }

    pub fn bounds(&self) -> *const CGRect {
        unsafe { SLSWindowIteratorGetBounds(self.as_ptr(), self.index()) }
    }

    pub fn frame_bounds(&self) -> *const CGRect {
        unsafe { SLSWindowIteratorGetFrameBounds(self.as_ptr(), self.index()) }
    }

    pub fn last_non_empty_frame_bounds(&self) -> *const CGRect {
        unsafe { SLSWindowIteratorGetLastNonEmptyFrameBounds(self.as_ptr(), self.index()) }
    }

    pub fn screen_rect(&self) -> *const CGRect {
        unsafe { SLSWindowIteratorGetScreenRect(self.as_ptr(), self.index()) }
    }

    pub fn constraints(&self) -> SLSWindowQueryConstraints {
        let mut minimum = CGSize::ZERO;
        let mut maximum = CGSize::ZERO;
        let mut ideal = CGSize::ZERO;

        unsafe {
            SLSWindowIteratorGetCurrentConstraints(
                self.as_ptr(),
                &mut minimum,
                &mut maximum,
                &mut ideal,
            )
        };

        SLSWindowQueryConstraints {
            minimum,
            maximum,
            ideal,
        }
    }

    pub fn corner_radii(&self) -> Option<CFRetained<CFArray<CFNumber>>> {
        let array =
            NonNull::new(unsafe { SLSWindowIteratorGetCornerRadii(self.as_ptr(), self.index()) })?;
        Some(unsafe { CFRetained::from_raw(array) })
    }

    pub fn resolved_corner_radii(&self) -> Option<CFRetained<CFArray<CFNumber>>> {
        let array = NonNull::new(unsafe {
            SLSWindowIteratorGetResolvedCornerRadii(self.as_ptr(), self.index())
        })?;
        Some(unsafe { CFRetained::from_raw(array) })
    }

    pub fn matching_space(
        &self,
        include_mask: u64,
        exclude_mask: u64,
    ) -> SLSWindowQueryMatchingSpace {
        let mut space_id = 0;
        let mut space_type = 0;
        let mut space_attributes = 0;

        let result = unsafe {
            SLSWindowIteratorGetCurrentMatchingSpaceID(
                self.as_ptr(),
                include_mask,
                exclude_mask,
                &mut space_id,
                &mut space_type,
                &mut space_attributes,
            )
        };

        SLSWindowQueryMatchingSpace {
            result,
            space_id,
            space_type,
            space_attributes,
        }
    }
}

impl SLSWindowQueryWindows {
    pub fn native_count(&self) -> u32 {
        unsafe { SLSWindowIteratorGetCount(self.as_ptr()) }
    }
}

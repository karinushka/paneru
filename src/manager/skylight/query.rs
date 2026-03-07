#![allow(dead_code, unused_imports)]

use std::ptr::{NonNull, null};

use objc2_core_foundation::{
    CFArray, CFDictionary, CFNumber, CFNumberType, CFRetained, CFType, CGRect, CGSize,
};

use crate::{
    errors::Result,
    platform::{CFStringRef, ConnID, Pid, ProcessSerialNumber, WorkspaceId},
    util::create_array,
};

mod managed_displays;
mod spaces;
mod windows;

pub use managed_displays::{SLSManagedDisplayQueryItem, SLSWindowQueryManagedDisplays};
pub use spaces::{SLSWindowQuerySpace, SLSWindowQuerySpaces};
pub use windows::{SLSWindowQueryWindow, SLSWindowQueryWindows};

unsafe extern "C" {
    pub static SLSWindowQueryKeyOwner: CFStringRef;
    pub static SLSWindowQueryKeyWorkspaceWindowListOptions: CFStringRef;
    pub static SLSWindowQueryKeySpaceListOptions: CFStringRef;
    pub static SLSWindowQueryKeySpaces: CFStringRef;
    pub static SLSWindowQueryKeyIncludeTags: CFStringRef;
    pub static SLSWindowQueryKeyExcludeTags: CFStringRef;
    pub static SLSWindowQueryKeyExcludeProcess: CFStringRef;
    pub static SLSWindowQueryKeyExcludeProcesses: CFStringRef;

    pub fn SLSWindowQueryCreate(values: *const CFDictionary) -> *mut CFType;
    pub fn SLSWindowQuerySetValue(query: *const CFType, key: CFStringRef, value: *const CFType);
    pub fn SLSWindowQueryCopyValue(query: *const CFType, key: CFStringRef) -> *mut CFType;
    pub fn SLSWindowQueryRun(cid: ConnID, query: *const CFType, hint: isize) -> *mut CFType;
    #[link_name = "SLSWindowQueryWindows"]
    pub fn SLSWindowQueryWindowsRaw(
        cid: ConnID,
        windows: *const CFArray,
        count: isize,
    ) -> NonNull<CFType>;
    pub fn SLSWindowQueryResultCopyWindows(query_result: NonNull<CFType>) -> NonNull<CFType>;
    pub fn SLSWindowQueryResultCopySpaces(query_result: *const CFType) -> NonNull<CFType>;
    pub fn SLSWindowQueryResultCopyManagedDisplays(query_result: *const CFType) -> NonNull<CFType>;
    pub fn SLSWindowQueryResultGetWindowCount(query_result: *const CFType) -> u32;
    pub fn SLSWindowQueryResultGetSpaceCount(query_result: *const CFType) -> u32;
    pub fn SLSWindowQueryResultGetManagedDisplayCount(query_result: *const CFType) -> u32;
    pub fn SLSWindowIteratorAdvance(iterator: *const CFType) -> bool;
    pub fn SLSWindowIteratorGetAlpha(iterator: *const CFType, index: u64) -> f32;
    pub fn SLSWindowIteratorGetAttachedWindowCount(iterator: *const CFType, index: u64) -> u32;
    pub fn SLSWindowIteratorGetAttributes(iterator: *const CFType, index: u64) -> i64;
    pub fn SLSWindowIteratorGetBounds(iterator: *const CFType, index: u64) -> *const CGRect;
    #[link_name = "SLSWindowIteratorGetConstraints"]
    pub fn SLSWindowIteratorGetCurrentConstraints(
        iterator: *const CFType,
        minimum: *mut CGSize,
        maximum: *mut CGSize,
        ideal: *mut CGSize,
    );
    pub fn SLSWindowIteratorGetCornerMaskFlags(iterator: *const CFType, index: u64) -> u32;
    pub fn SLSWindowIteratorGetCornerRadii(
        iterator: *const CFType,
        index: u64,
    ) -> *mut CFArray<CFNumber>;
    pub fn SLSWindowIteratorGetFrameBounds(iterator: *const CFType, index: u64) -> *const CGRect;
    pub fn SLSWindowIteratorGetLastNonEmptyFrameBounds(
        iterator: *const CFType,
        index: u64,
    ) -> *const CGRect;
    pub fn SLSWindowIteratorGetLevel(iterator: *const CFType, index: u64) -> i32;
    #[link_name = "SLSWindowIteratorGetMatchingSpaceID"]
    pub fn SLSWindowIteratorGetCurrentMatchingSpaceID(
        iterator: *const CFType,
        include_mask: u64,
        exclude_mask: u64,
        space_id: *mut u64,
        space_type: *mut u32,
        space_attributes: *mut u64,
    ) -> u64;
    pub fn SLSWindowIteratorGetOwner(iterator: *const CFType, index: u64) -> ConnID;
    pub fn SLSWindowIteratorGetPID(iterator: *const CFType, index: u64) -> Pid;
    pub fn SLSWindowIteratorGetPSN(iterator: *const CFType, index: u64) -> ProcessSerialNumber;
    pub fn SLSWindowIteratorGetParentID(iterator: *const CFType, index: u64) -> i32;
    pub fn SLSWindowIteratorGetResolvedCornerRadii(
        iterator: *const CFType,
        index: u64,
    ) -> *mut CFArray<CFNumber>;
    pub fn SLSWindowIteratorGetScreenRect(iterator: *const CFType, index: u64) -> *const CGRect;
    pub fn SLSWindowIteratorGetSpaceAttributes(iterator: *const CFType, index: u64) -> u64;
    pub fn SLSWindowIteratorGetSpaceCount(iterator: *const CFType, index: u64) -> u32;
    pub fn SLSWindowIteratorGetSpaceTypeMask(iterator: *const CFType, index: u64) -> u64;
    pub fn SLSWindowIteratorGetTags(iterator: *const CFType, index: u64) -> i64;
    pub fn SLSWindowIteratorGetWindowID(iterator: *const CFType, index: u64) -> i32;
    pub fn SLSWindowIteratorGetCount(iterator: *const CFType) -> u32;
    pub fn SLSSpaceIteratorAdvance(iterator: *const CFType) -> bool;
    pub fn SLSSpaceIteratorGetAbsoluteLevel(iterator: *const CFType, index: u64) -> i32;
    pub fn SLSSpaceIteratorGetAttributes(iterator: *const CFType, index: u64) -> u64;
    pub fn SLSSpaceIteratorGetCount(iterator: *const CFType) -> u32;
    pub fn SLSSpaceIteratorGetParentSpaceID(iterator: *const CFType, index: u64) -> u64;
    pub fn SLSSpaceIteratorGetSpaceID(iterator: *const CFType, index: u64) -> u64;
    pub fn SLSSpaceIteratorGetType(iterator: *const CFType, index: u64) -> i32;
    pub fn SLSManagedDisplayIteratorAdvance(iterator: *const CFType) -> bool;
    pub fn SLSManagedDisplayIteratorGetAttributes(iterator: *const CFType, index: u64) -> u64;
    pub fn SLSManagedDisplayIteratorGetBounds(iterator: *const CFType, index: u64)
    -> *const CGRect;
    pub fn SLSManagedDisplayIteratorGetCount(iterator: *const CFType) -> u32;
    pub fn SLSManagedDisplayIteratorGetUUIDBytes(
        iterator: *const CFType,
        index: u64,
    ) -> SLSManagedDisplayUUIDBytes;
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct SLSManagedDisplayUUIDBytes {
    pub bytes: [u8; 16],
}

#[derive(Clone, Copy)]
pub struct SLSWindowQueryConstraints {
    pub minimum: CGSize,
    pub maximum: CGSize,
    pub ideal: CGSize,
}

#[derive(Clone, Copy)]
pub struct SLSWindowQueryMatchingSpace {
    pub result: u64,
    pub space_id: u64,
    pub space_type: u32,
    pub space_attributes: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SLSWindowQueryKey {
    Owner,
    WorkspaceWindowListOptions,
    SpaceListOptions,
    Spaces,
    IncludeTags,
    ExcludeTags,
    ExcludeProcess,
    ExcludeProcesses,
}

impl SLSWindowQueryKey {
    #[inline]
    pub fn as_raw(self) -> CFStringRef {
        unsafe {
            match self {
                Self::Owner => SLSWindowQueryKeyOwner,
                Self::WorkspaceWindowListOptions => SLSWindowQueryKeyWorkspaceWindowListOptions,
                Self::SpaceListOptions => SLSWindowQueryKeySpaceListOptions,
                Self::Spaces => SLSWindowQueryKeySpaces,
                Self::IncludeTags => SLSWindowQueryKeyIncludeTags,
                Self::ExcludeTags => SLSWindowQueryKeyExcludeTags,
                Self::ExcludeProcess => SLSWindowQueryKeyExcludeProcess,
                Self::ExcludeProcesses => SLSWindowQueryKeyExcludeProcesses,
            }
        }
    }
}

pub struct SLSWindowQuery {
    query: CFRetained<CFType>,
}

impl SLSWindowQuery {
    pub fn new() -> Option<Self> {
        let query = NonNull::new(unsafe { SLSWindowQueryCreate(null()) })?;
        Some(Self {
            query: unsafe { CFRetained::from_raw(query) },
        })
    }

    #[inline]
    fn as_ptr(&self) -> *const CFType {
        &raw const *self.query
    }

    fn set_cf_type(&self, key: SLSWindowQueryKey, value: &CFType) {
        unsafe { SLSWindowQuerySetValue(self.as_ptr(), key.as_raw(), value) }
    }

    fn set_i32(&self, key: SLSWindowQueryKey, value: i32) {
        let number = CFNumber::new_i32(value);
        self.set_cf_type(key, number.as_ref());
    }

    fn set_i64(&self, key: SLSWindowQueryKey, value: i64) {
        let number = CFNumber::new_i64(value);
        self.set_cf_type(key, number.as_ref());
    }

    fn set_array<T>(
        &self,
        key: SLSWindowQueryKey,
        values: &[T],
        number_type: CFNumberType,
    ) -> Result<()> {
        let values = create_array(values, number_type)?;
        self.set_cf_type(key, values.as_ref());
        Ok(())
    }

    pub fn with_owner(self, owner: ConnID) -> Self {
        self.set_i64(SLSWindowQueryKey::Owner, owner);
        self
    }

    pub fn with_workspace_window_list_options(self, options: u32) -> Self {
        self.set_i64(
            SLSWindowQueryKey::WorkspaceWindowListOptions,
            i64::from(options),
        );
        self
    }

    pub fn with_space_list_options(self, options: u32) -> Self {
        self.set_i64(SLSWindowQueryKey::SpaceListOptions, i64::from(options));
        self
    }

    pub fn with_spaces(self, spaces: &[WorkspaceId]) -> Result<Self> {
        self.set_array(SLSWindowQueryKey::Spaces, spaces, CFNumberType::SInt64Type)?;
        Ok(self)
    }

    pub fn with_include_tags(self, tags: u64) -> Self {
        self.set_i64(SLSWindowQueryKey::IncludeTags, tags as i64);
        self
    }

    pub fn with_exclude_tags(self, tags: u64) -> Self {
        self.set_i64(SLSWindowQueryKey::ExcludeTags, tags as i64);
        self
    }

    pub fn with_exclude_process(self, pid: Pid) -> Self {
        self.set_i32(SLSWindowQueryKey::ExcludeProcess, pid);
        self
    }

    pub fn with_exclude_processes(self, pids: &[Pid]) -> Result<Self> {
        self.set_array(
            SLSWindowQueryKey::ExcludeProcesses,
            pids,
            CFNumberType::SInt32Type,
        )?;
        Ok(self)
    }

    pub fn copy_value(&self, key: SLSWindowQueryKey) -> Option<CFRetained<CFType>> {
        let value = NonNull::new(unsafe { SLSWindowQueryCopyValue(self.as_ptr(), key.as_raw()) })?;
        Some(unsafe { CFRetained::from_raw(value) })
    }

    pub fn run(&self, cid: ConnID) -> Option<SLSWindowQueryResult> {
        self.run_with_hint(cid, 0)
    }

    pub fn run_with_hint(&self, cid: ConnID, hint: isize) -> Option<SLSWindowQueryResult> {
        let query_result = NonNull::new(unsafe { SLSWindowQueryRun(cid, self.as_ptr(), hint) })?;
        Some(SLSWindowQueryResult {
            query_result: unsafe { CFRetained::from_raw(query_result) },
        })
    }
}

pub struct SLSWindowQueryResult {
    pub(super) query_result: CFRetained<CFType>,
}

impl SLSWindowQueryResult {
    #[inline]
    fn as_ptr(&self) -> *const CFType {
        &raw const *self.query_result
    }

    pub fn window_count(&self) -> u32 {
        unsafe { SLSWindowQueryResultGetWindowCount(self.as_ptr()) }
    }

    pub fn space_count(&self) -> u32 {
        unsafe { SLSWindowQueryResultGetSpaceCount(self.as_ptr()) }
    }

    pub fn managed_display_count(&self) -> u32 {
        unsafe { SLSWindowQueryResultGetManagedDisplayCount(self.as_ptr()) }
    }

    pub fn windows(&self) -> SLSWindowQueryWindows {
        SLSWindowQueryWindows {
            iterator: unsafe {
                CFRetained::from_raw(SLSWindowQueryResultCopyWindows(NonNull::from(
                    &*self.query_result,
                )))
            },
            count: self.window_count() as usize,
            current_index: None,
        }
    }

    pub fn spaces(&self) -> SLSWindowQuerySpaces {
        SLSWindowQuerySpaces {
            iterator: unsafe {
                CFRetained::from_raw(SLSWindowQueryResultCopySpaces(self.as_ptr()))
            },
            count: self.space_count() as usize,
            current_index: None,
        }
    }

    pub fn managed_displays(&self) -> SLSWindowQueryManagedDisplays {
        SLSWindowQueryManagedDisplays {
            iterator: unsafe {
                CFRetained::from_raw(SLSWindowQueryResultCopyManagedDisplays(self.as_ptr()))
            },
            count: self.managed_display_count() as usize,
            current_index: None,
        }
    }
}

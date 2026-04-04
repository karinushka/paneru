use objc2_core_foundation::{CFRetained, CFType};

use crate::platform::WorkspaceId;

use super::{
    SLSSpaceIteratorAdvance, SLSSpaceIteratorGetAbsoluteLevel, SLSSpaceIteratorGetAttributes,
    SLSSpaceIteratorGetCount, SLSSpaceIteratorGetParentSpaceID, SLSSpaceIteratorGetSpaceID,
    SLSSpaceIteratorGetType,
};

pub struct SLSWindowQuerySpaces {
    pub(super) iterator: CFRetained<CFType>,
    pub(super) count: usize,
    pub(super) current_index: Option<u64>,
}

pub struct SLSWindowQuerySpace<'a> {
    iterator: &'a SLSWindowQuerySpaces,
}

impl SLSWindowQuerySpaces {
    #[inline]
    fn as_ptr(&self) -> *const CFType {
        &raw const *self.iterator
    }

    #[inline]
    fn index(&self) -> u64 {
        self.current_index
            .expect("space query row accessed before advance()")
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
        if !unsafe { SLSSpaceIteratorAdvance(self.as_ptr()) } {
            self.current_index = None;
            return false;
        }

        self.current_index = Some(self.current_index.map_or(0, |index| index + 1));
        true
    }

    pub fn current(&self) -> Option<SLSWindowQuerySpace<'_>> {
        self.current_index
            .map(|_| SLSWindowQuerySpace { iterator: self })
    }

    pub fn into_inner(self) -> CFRetained<CFType> {
        self.iterator
    }

    pub fn as_inner(&self) -> &CFType {
        self.iterator.as_ref()
    }

    pub fn native_count(&self) -> u32 {
        unsafe { SLSSpaceIteratorGetCount(self.as_ptr()) }
    }
}

impl SLSWindowQuerySpace<'_> {
    #[inline]
    fn as_ptr(&self) -> *const CFType {
        self.iterator.as_ptr()
    }

    #[inline]
    fn index(&self) -> u64 {
        self.iterator.index()
    }

    pub fn space_id(&self) -> WorkspaceId {
        unsafe { SLSSpaceIteratorGetSpaceID(self.as_ptr(), self.index()) }
    }

    pub fn parent_space_id(&self) -> WorkspaceId {
        unsafe { SLSSpaceIteratorGetParentSpaceID(self.as_ptr(), self.index()) }
    }

    pub fn attributes(&self) -> u64 {
        unsafe { SLSSpaceIteratorGetAttributes(self.as_ptr(), self.index()) }
    }

    pub fn absolute_level(&self) -> i32 {
        unsafe { SLSSpaceIteratorGetAbsoluteLevel(self.as_ptr(), self.index()) }
    }

    pub fn space_type(&self) -> i32 {
        unsafe { SLSSpaceIteratorGetType(self.as_ptr(), self.index()) }
    }
}

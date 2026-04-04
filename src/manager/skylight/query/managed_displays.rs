use objc2_core_foundation::{CFRetained, CFType, CGRect};

use super::{
    SLSManagedDisplayIteratorAdvance, SLSManagedDisplayIteratorGetAttributes,
    SLSManagedDisplayIteratorGetBounds, SLSManagedDisplayIteratorGetCount,
    SLSManagedDisplayIteratorGetUUIDBytes,
};

pub struct SLSWindowQueryManagedDisplays {
    pub(super) iterator: CFRetained<CFType>,
    pub(super) count: usize,
    pub(super) current_index: Option<u64>,
}

pub struct SLSManagedDisplayQueryItem<'a> {
    iterator: &'a SLSWindowQueryManagedDisplays,
}

impl SLSWindowQueryManagedDisplays {
    #[inline]
    fn as_ptr(&self) -> *const CFType {
        &raw const *self.iterator
    }

    #[inline]
    fn index(&self) -> u64 {
        self.current_index
            .expect("managed display query row accessed before advance()")
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
        if !unsafe { SLSManagedDisplayIteratorAdvance(self.as_ptr()) } {
            self.current_index = None;
            return false;
        }

        self.current_index = Some(self.current_index.map_or(0, |index| index + 1));
        true
    }

    pub fn current(&self) -> Option<SLSManagedDisplayQueryItem<'_>> {
        self.current_index
            .map(|_| SLSManagedDisplayQueryItem { iterator: self })
    }

    pub fn into_inner(self) -> CFRetained<CFType> {
        self.iterator
    }

    pub fn as_inner(&self) -> &CFType {
        self.iterator.as_ref()
    }

    pub fn native_count(&self) -> u32 {
        unsafe { SLSManagedDisplayIteratorGetCount(self.as_ptr()) }
    }
}

impl SLSManagedDisplayQueryItem<'_> {
    #[inline]
    fn as_ptr(&self) -> *const CFType {
        self.iterator.as_ptr()
    }

    #[inline]
    fn index(&self) -> u64 {
        self.iterator.index()
    }

    pub fn uuid_bytes(&self) -> [u8; 16] {
        unsafe { SLSManagedDisplayIteratorGetUUIDBytes(self.as_ptr(), self.index()).bytes }
    }

    pub fn attributes(&self) -> u64 {
        unsafe { SLSManagedDisplayIteratorGetAttributes(self.as_ptr(), self.index()) }
    }

    pub fn bounds(&self) -> *const CGRect {
        unsafe { SLSManagedDisplayIteratorGetBounds(self.as_ptr(), self.index()) }
    }
}

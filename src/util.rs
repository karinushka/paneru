use accessibility_sys::AXUIElementRef;
use core::ptr::NonNull;
use objc2_core_foundation::{
    CFArray, CFArrayCreate, CFArrayGetCount, CFArrayGetValueAtIndex, CFDictionary,
    CFDictionaryGetValue, CFNumber, CFNumberCreate, CFNumberType, CFRetained, CFString,
    CFStringGetCString, CFStringGetLength, CFStringGetMaximumSizeForEncoding, CFType, Type,
    kCFTypeArrayCallBacks,
};
use std::ffi::c_void;
use std::ops::Deref;
use std::ptr::null_mut;

use crate::platform::CFStringRef;
use crate::skylight::AXUIElementCopyAttributeValue;

pub struct Cleanuper {
    cleanup: Box<dyn Fn()>,
}

unsafe impl Send for Cleanuper {}

impl Drop for Cleanuper {
    fn drop(&mut self) {
        (self.cleanup)();
    }
}

impl Cleanuper {
    pub fn new(cleanup: Box<dyn Fn()>) -> Self {
        Cleanuper { cleanup }
    }
}

#[derive(Debug)]
pub struct AxuWrapperType;
unsafe impl objc2_core_foundation::Type for AxuWrapperType {}

impl AxuWrapperType {
    pub fn as_ptr<T>(&self) -> *mut T {
        NonNull::from(self).cast::<T>().as_ptr()
    }

    pub fn from_ptr<T>(ptr: *mut T) -> Option<NonNull<Self>> {
        NonNull::new(ptr).map(|ptr| ptr.cast())
    }

    // The pointer is already retained, so simply wrap it in the CFRetained.
    pub fn from_retained<T>(ptr: *mut T) -> Option<CFRetained<Self>> {
        let ptr = Self::from_ptr(ptr)?;
        Some(unsafe { CFRetained::from_raw(ptr) })
    }

    // The pointer is not retained, so retain it and wrap it in CFRetained.
    pub fn retain<T>(ptr: *mut T) -> Option<CFRetained<Self>> {
        let ptr = Self::from_ptr(ptr)?;
        Some(unsafe { CFRetained::retain(ptr) })
    }
}

impl<T> std::convert::AsRef<T> for AxuWrapperType {
    fn as_ref(&self) -> &T {
        let ptr = NonNull::from(self).cast();
        unsafe { ptr.as_ref() }
    }
}

impl std::fmt::Display for AxuWrapperType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:?}", self.as_ptr::<AXUIElementRef>())
    }
}

pub fn get_attribute<T: Type>(
    element_ref: &CFRetained<AxuWrapperType>,
    name: CFRetained<CFString>,
) -> Option<CFRetained<T>> {
    let mut attribute: *mut T = null_mut();
    if 0 != unsafe {
        AXUIElementCopyAttributeValue(
            element_ref.as_ptr(),
            name.deref(),
            (&mut attribute as *mut *mut T) as *mut *mut CFType,
        )
    } {
        return None;
    }
    let ptr = NonNull::new(attribute)?;
    Some(unsafe { CFRetained::from_raw(ptr) })
}

pub fn get_cfdict_value<T>(dict: &CFDictionary, key: &CFString) -> Option<NonNull<T>> {
    let ptr =
        unsafe { CFDictionaryGetValue(dict, (key as CFStringRef) as *const c_void) as *mut T };
    NonNull::new(ptr)
}

pub fn get_array_values<T>(array: &CFArray) -> impl Iterator<Item = NonNull<T>> + use<'_, T> {
    let count = unsafe { CFArrayGetCount(array) };
    (0..count)
        .flat_map(move |idx| NonNull::new(unsafe { CFArrayGetValueAtIndex(array, idx) as *mut T }))
}

pub fn get_string_from_string(nameptr: CFStringRef) -> String {
    const ENCODING: u32 = 0x08000100; // kCFStringEncodingUTF8 = 0x08000100
    NonNull::new(nameptr as *mut CFString)
        .map(|nameptr| unsafe {
            let name = CFRetained::retain(nameptr);
            let len = CFStringGetLength(name.as_ref());
            let size = CFStringGetMaximumSizeForEncoding(len, ENCODING) as usize;
            let mut buf = Vec::<u8>::with_capacity(size);
            if !CFStringGetCString(
                name.as_ref(),
                buf.as_mut_ptr() as *mut i8,
                size as isize,
                ENCODING,
            ) {
                return "".to_string();
            }
            buf.set_len(size);
            String::from_utf8_lossy(&buf)
                .trim_end_matches('\0')
                .to_owned()
        })
        .unwrap_or("".to_string())
}

pub fn create_array<T>(values: Vec<T>, cftype: CFNumberType) -> Option<CFRetained<CFArray>> {
    let numbers = values
        .iter()
        .flat_map(|value: &T| unsafe {
            CFNumberCreate(None, cftype, value as *const T as *const c_void)
        })
        .collect::<Vec<_>>();

    let mut ptrs = numbers
        .iter()
        .map(|num| num.deref() as *const CFNumber as *const c_void)
        .collect::<Vec<_>>();

    unsafe {
        CFArrayCreate(
            None,
            ptrs.as_mut_ptr(),
            numbers.len().try_into().unwrap(),
            &kCFTypeArrayCallBacks,
        )
    }
}

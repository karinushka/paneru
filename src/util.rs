use accessibility_sys::{AXObserverGetRunLoopSource, AXUIElementRef};
use core::ptr::NonNull;
use log::debug;
use objc2_core_foundation::{
    CFArray, CFDictionary, CFNumber, CFNumberType, CFRetained, CFRunLoop, CFRunLoopMode,
    CFRunLoopSource, CFString, CFType, Type, kCFTypeArrayCallBacks,
};
use std::{
    ffi::{CStr, OsStr, c_int, c_void},
    io::{Error, ErrorKind, Result},
    ops::Deref,
    os::unix::ffi::OsStrExt,
    path::PathBuf,
    ptr::null_mut,
};
use stdext::function_name;

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
    /// Creates a new `Cleanuper` instance with a given cleanup closure.
    ///
    /// # Arguments
    ///
    /// * `cleanup` - A boxed closure `Box<dyn Fn()>` to be executed when `Cleanuper` is dropped.
    ///
    /// # Returns
    ///
    /// A new `Cleanuper` instance.
    pub fn new(cleanup: Box<dyn Fn()>) -> Self {
        Cleanuper { cleanup }
    }
}

#[derive(Debug)]
pub struct AxuWrapperType;
unsafe impl objc2_core_foundation::Type for AxuWrapperType {}

impl AxuWrapperType {
    /// Converts `self` into a raw mutable pointer of type `T`.
    ///
    /// # Type Parameters
    ///
    /// * `T` - The target type for the raw pointer.
    ///
    /// # Returns
    ///
    /// A raw mutable pointer to `T`.
    pub fn as_ptr<T>(&self) -> *mut T {
        NonNull::from(self).cast::<T>().as_ptr()
    }

    /// Converts a raw mutable pointer of type `T` into a `NonNull<Self>`.
    ///
    /// # Type Parameters
    ///
    /// * `T` - The type of the input raw pointer.
    ///
    /// # Arguments
    ///
    /// * `ptr` - The raw mutable pointer.
    ///
    /// # Returns
    ///
    /// `Ok(NonNull<Self>)` if the pointer is not null, otherwise `Err(Error)`.
    pub fn from_ptr<T>(ptr: *mut T) -> Result<NonNull<Self>> {
        NonNull::new(ptr).map(|ptr| ptr.cast()).ok_or(Error::new(
            ErrorKind::InvalidInput,
            format!("{}: nullptr passed.", function_name!()),
        ))
    }

    /// Wraps an already retained raw pointer of type `T` into a `CFRetained<Self>`.
    /// This function assumes the caller has already handled the retention count.
    ///
    /// # Type Parameters
    ///
    /// * `T` - The type of the input raw pointer.
    ///
    /// # Arguments
    ///
    /// * `ptr` - The already retained raw mutable pointer.
    ///
    /// # Returns
    ///
    /// `Ok(CFRetained<Self>)` if the pointer is valid, otherwise `Err(Error)`.
    pub fn from_retained<T>(ptr: *mut T) -> Result<CFRetained<Self>> {
        let ptr = Self::from_ptr(ptr)?;
        Ok(unsafe { CFRetained::from_raw(ptr) })
    }

    /// Retains a raw pointer of type `T` and wraps it into a `CFRetained<Self>`.
    /// This function increments the retention count.
    ///
    /// # Type Parameters
    ///
    /// * `T` - The type of the input raw pointer.
    ///
    /// # Arguments
    ///
    /// * `ptr` - The raw mutable pointer to retain.
    ///
    /// # Returns
    ///
    /// `Ok(CFRetained<Self>)` if the pointer is valid, otherwise `Err(Error)`.
    pub fn retain<T>(ptr: *mut T) -> Result<CFRetained<Self>> {
        let ptr = Self::from_ptr(ptr)?;
        Ok(unsafe { CFRetained::retain(ptr) })
    }
}

impl<T> std::convert::AsRef<T> for AxuWrapperType {
    /// Provides a shared reference to the inner data as type `T`.
    ///
    /// # Returns
    ///
    /// A shared reference to `T`.
    fn as_ref(&self) -> &T {
        let ptr = NonNull::from(self).cast();
        unsafe { ptr.as_ref() }
    }
}

impl std::fmt::Display for AxuWrapperType {
    /// Formats the `AxuWrapperType` for display, showing the raw pointer value.
    ///
    /// # Arguments
    ///
    /// * `f` - The formatter.
    ///
    /// # Returns
    ///
    /// A `std::fmt::Result`.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:?}", self.as_ptr::<AXUIElementRef>())
    }
}

/// Retrieves the value of an accessibility attribute from a given UI element.
///
/// # Type Parameters
///
/// * `T` - The expected type of the attribute value, which must implement `objc2_core_foundation::Type`.
///
/// # Arguments
///
/// * `element_ref` - A reference to the `CFRetained<AxuWrapperType>` representing the UI element.
/// * `name` - A `CFRetained<CFString>` representing the name of the attribute.
///
/// # Returns
///
/// `Ok(CFRetained<T>)` with the attribute value if successful, otherwise `Err(Error)`.
pub fn get_attribute<T: Type>(
    element_ref: &CFRetained<AxuWrapperType>,
    name: CFRetained<CFString>,
) -> Result<CFRetained<T>> {
    let mut attribute: *mut CFType = null_mut();
    if 0 != unsafe {
        AXUIElementCopyAttributeValue(element_ref.as_ptr(), name.deref(), &mut attribute)
    } {
        Err(Error::new(
            ErrorKind::NotFound,
            format!("{}: failed getting attribute {name}.", function_name!()),
        ))
    } else {
        NonNull::new(attribute)
            .map(|ptr| unsafe { CFRetained::from_raw(ptr.cast()) })
            .ok_or(Error::new(
                ErrorKind::InvalidData,
                format!(
                    "{}: nullptr while getting attribute {name}.",
                    function_name!()
                ),
            ))
    }
}

/// Retrieves a value from a `CFDictionary` given a key.
///
/// # Type Parameters
///
/// * `T` - The expected type of the value to retrieve.
///
/// # Arguments
///
/// * `dict` - A reference to the `CFDictionary`.
/// * `key` - A reference to the `CFString` representing the key.
///
/// # Returns
///
/// `Ok(NonNull<T>)` with a non-null pointer to the value if found, otherwise `Err(Error)`.
pub fn get_cfdict_value<T>(dict: &CFDictionary, key: &CFString) -> Result<NonNull<T>> {
    let ptr = unsafe { CFDictionary::value(dict, NonNull::from(key).as_ptr().cast()) };
    NonNull::new(ptr.cast_mut())
        .map(|ptr| ptr.cast::<T>())
        .ok_or(Error::new(
            ErrorKind::InvalidData,
            format!("{}: can not get data for key {key}", function_name!(),),
        ))
}

/// Retrieves an iterator over the values in a `CFArray`.
///
/// # Type Parameters
///
/// * `T` - The expected type of the elements in the array.
///
/// # Arguments
///
/// * `array` - A reference to the `CFArray`.
///
/// # Returns
///
/// An iterator yielding `NonNull<T>` for each element in the array.
pub fn get_array_values<T>(array: &CFArray) -> impl Iterator<Item = NonNull<T>> + use<'_, T> {
    let count = CFArray::count(array);
    (0..count).flat_map(move |idx| {
        NonNull::new(unsafe { CFArray::value_at_index(array, idx).cast_mut() })
            .map(|ptr| ptr.cast::<T>())
    })
}

/// Creates a new `CFArray` from a vector of values and a specified `CFNumberType`.
///
/// # Type Parameters
///
/// * `T` - The type of the values in the input vector.
///
/// # Arguments
///
/// * `values` - A `Vec<T>` containing the values to be added to the array.
/// * `cftype` - The `CFNumberType` representing the type of numbers in the array.
///
/// # Returns
///
/// `Ok(CFRetained<CFArray>)` with the created array if successful, otherwise `Err(Error)`.
pub fn create_array<T>(values: Vec<T>, cftype: CFNumberType) -> Result<CFRetained<CFArray>> {
    let numbers = values
        .iter()
        .flat_map(|value: &T| unsafe {
            CFNumber::new(None, cftype, NonNull::from(value).as_ptr().cast())
        })
        .collect::<Vec<_>>();

    let mut ptrs = numbers
        .iter()
        .map(|num| NonNull::from(num.deref()).as_ptr() as *const c_void)
        .collect::<Vec<_>>();

    unsafe {
        CFArray::new(
            None,
            ptrs.as_mut_ptr(),
            numbers.len().try_into().unwrap(),
            &kCFTypeArrayCallBacks,
        )
    }
    .ok_or(Error::new(
        ErrorKind::InvalidData,
        format!("{}: can not create an array.", function_name!()),
    ))
}

/// Retrieves the `CFRunLoopSource` associated with an `AXObserver`.
///
/// # Arguments
///
/// * `observer` - A reference to the `AxuWrapperType` wrapping the `AXObserverRef`.
///
/// # Returns
///
/// `Some(&CFRunLoopSource)` if a run loop source is found, otherwise `None`.
fn run_loop_source(observer: &AxuWrapperType) -> Option<&CFRunLoopSource> {
    let ptr = NonNull::new(unsafe { AXObserverGetRunLoopSource(observer.as_ptr()) })?;
    Some(unsafe { ptr.cast::<CFRunLoopSource>().as_ref() })
}

/// Adds the `CFRunLoopSource` of an `AXObserver` to the main run loop.
///
/// # Arguments
///
/// * `observer` - A reference to the `AxuWrapperType` wrapping the `AXObserverRef`.
/// * `mode` - An optional `CFRunLoopMode` for adding the source.
///
/// # Returns
///
/// `Ok(())` if the run loop source is added successfully, otherwise `Err(Error)`.
pub fn add_run_loop(observer: &AxuWrapperType, mode: Option<&CFRunLoopMode>) -> Result<()> {
    let run_loop = run_loop_source(observer);

    match CFRunLoop::main() {
        Some(main_loop) if run_loop.is_some() => {
            debug!(
                "{}: add runloop: {run_loop:?} observer {:?}",
                function_name!(),
                observer.as_ptr::<CFRunLoopSource>(),
            );
            CFRunLoop::add_source(main_loop.deref(), run_loop, mode);
            Ok(())
        }
        _ => Err(Error::new(
            ErrorKind::PermissionDenied,
            format!(
                "{}: Unable to register run loop source for observer {:?} ",
                function_name!(),
                observer.as_ptr::<CFRunLoopSource>(),
            ),
        )),
    }
}

/// Invalidates and removes the `CFRunLoopSource` of an `AXObserver` from the main run loop.
///
/// # Arguments
///
/// * `observer` - A reference to the `AxuWrapperType` wrapping the `AXObserverRef`.
pub fn remove_run_loop(observer: &AxuWrapperType) {
    if let Some(run_loop_source) = run_loop_source(observer) {
        debug!(
            "{}: removing runloop: {run_loop_source:?} observer {:?}",
            function_name!(),
            observer.as_ptr::<CFRunLoopSource>(),
        );
        CFRunLoopSource::invalidate(run_loop_source);
    }
}

/// Returns the path of the current executable.
#[must_use]
pub fn exe_path() -> Option<PathBuf> {
    #[link(name = "Foundation", kind = "framework")]
    unsafe extern "C" {
        fn _NSGetExecutablePath(buf: *mut u8, buf_size: *mut u32) -> c_int;
    }

    let mut path_buf = [0_u8; 4096];

    let mut path_buf_size = path_buf.len() as u32;
    let path = unsafe { _NSGetExecutablePath(path_buf.as_mut_ptr(), &raw mut path_buf_size) == 0 }
        .then(|| CStr::from_bytes_until_nul(&path_buf).ok())??;
    Some(OsStr::from_bytes(path.to_bytes()).into())
}

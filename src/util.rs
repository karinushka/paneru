use accessibility_sys::{
    AXObserverGetRunLoopSource, AXUIElementRef, kAXFocusedWindowAttribute, kAXMinimizedAttribute,
    kAXParentAttribute, kAXRoleAttribute, kAXSubroleAttribute, kAXTitleAttribute,
    kAXWindowsAttribute,
};
use core::ptr::NonNull;
use log::debug;
use objc2_core_foundation::{
    CFArray, CFBoolean, CFNumber, CFNumberType, CFRetained, CFRunLoop, CFRunLoopMode,
    CFRunLoopSource, CFString, CFType, Type, kCFTypeArrayCallBacks,
};
use std::{
    ffi::{CStr, OsStr, c_int, c_void},
    os::unix::ffi::OsStrExt,
    path::{Path, PathBuf},
    ptr::null_mut,
};
use stdext::function_name;

use crate::{
    errors::{Error, Result},
    manager::{AXUIElementCopyAttributeValue, ax_window_id},
    platform::WinID,
};

#[derive(Debug)]
pub struct AXUIWrapper;
unsafe impl objc2_core_foundation::Type for AXUIWrapper {}

impl AXUIWrapper {
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
        NonNull::new(ptr)
            .map(std::ptr::NonNull::cast)
            .ok_or(Error::InvalidInput(format!(
                "{}: nullptr passed.",
                function_name!()
            )))
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
        Ok(unsafe { ptr.as_ref() }.retain())
    }
}

impl<T> std::convert::AsRef<T> for AXUIWrapper {
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

impl std::fmt::Display for AXUIWrapper {
    /// Formats the `AXUIWrapper` for display, showing the raw pointer value.
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

pub trait AXUIAttributes {
    fn parent(&self) -> Result<CFRetained<CFType>> {
        let axname = CFString::from_static_str(kAXParentAttribute);
        self.get_attribute::<CFType>(&axname)
    }

    fn subrole(&self) -> Result<String> {
        let axname = CFString::from_static_str(kAXSubroleAttribute);
        self.get_attribute::<CFString>(&axname)
            .map(|value| value.to_string())
    }

    fn role(&self) -> Result<String> {
        let axname = CFString::from_static_str(kAXRoleAttribute);
        self.get_attribute::<CFString>(&axname)
            .map(|value| value.to_string())
    }

    fn title(&self) -> Result<String> {
        let axname = CFString::from_static_str(kAXTitleAttribute);
        self.get_attribute::<CFString>(&axname)
            .map(|value| value.to_string())
    }

    fn minimized(&self) -> Result<bool> {
        let axname = CFString::from_static_str(kAXMinimizedAttribute);
        self.get_attribute::<CFBoolean>(&axname)
            .map(|value| CFBoolean::value(&value))
    }

    fn focused_window_id(&self) -> Result<WinID> {
        let axname = CFString::from_static_str(kAXFocusedWindowAttribute);
        self.get_attribute::<AXUIWrapper>(&axname)
            .and_then(|focused| ax_window_id(focused.as_ptr()))
    }

    fn windows(&self) -> Result<Vec<CFRetained<AXUIWrapper>>> {
        let axname = CFString::from_static_str(kAXWindowsAttribute);
        let array = self.get_attribute::<CFArray<AXUIWrapper>>(&axname)?;
        Ok(array.to_vec())
    }

    fn get_attribute<T: Type>(&self, name: &CFRetained<CFString>) -> Result<CFRetained<T>>;
}

impl AXUIAttributes for CFRetained<AXUIWrapper> {
    fn get_attribute<T: Type>(&self, name: &CFRetained<CFString>) -> Result<CFRetained<T>> {
        let mut attribute: *mut CFType = null_mut();
        if 0 == unsafe { AXUIElementCopyAttributeValue(self.as_ptr(), name, &mut attribute) } {
            NonNull::new(attribute)
                .map(|ptr| unsafe { CFRetained::from_raw(ptr.cast()) })
                .ok_or(Error::InvalidInput(format!(
                    "{}: nullptr while getting attribute {name}.",
                    function_name!()
                )))
        } else {
            Err(Error::NotFound(format!(
                "{}: failed getting attribute {name}.",
                function_name!()
            )))
        }
    }
}

/// Creates a new `CFArray` from a slice of values and a specified `CFNumberType`.
///
/// # Type Parameters
///
/// * `T` - The type of the values in the input slice.
///
/// # Arguments
///
/// * `values` - A slice containing the values to be added to the array.
/// * `cftype` - The `CFNumberType` representing the type of numbers in the array.
///
/// # Returns
///
/// `Ok(CFRetained<CFArray>)` with the created array if successful, otherwise `Err(Error)`.
pub fn create_array<T>(values: &[T], cftype: CFNumberType) -> Result<CFRetained<CFArray>> {
    let numbers = values
        .iter()
        .filter_map(|value: &T| unsafe {
            CFNumber::new(None, cftype, NonNull::from(value).as_ptr().cast())
        })
        .collect::<Vec<_>>();

    let mut ptrs = numbers
        .iter()
        .map(|num| NonNull::from(&**num).as_ptr() as *const c_void)
        .collect::<Vec<_>>();

    unsafe {
        CFArray::new(
            None,
            ptrs.as_mut_ptr(),
            numbers.len().try_into()?,
            &raw const kCFTypeArrayCallBacks,
        )
    }
    .ok_or(Error::InvalidInput(format!(
        "{}: can not create an array.",
        function_name!()
    )))
}

/// Retrieves the `CFRunLoopSource` associated with an `AXObserver`.
///
/// # Arguments
///
/// * `observer` - A reference to the `AXUIWrapper` wrapping the `AXObserverRef`.
///
/// # Returns
///
/// `Some(&CFRunLoopSource)` if a run loop source is found, otherwise `None`.
fn run_loop_source(observer: &AXUIWrapper) -> Option<&CFRunLoopSource> {
    let ptr = NonNull::new(unsafe { AXObserverGetRunLoopSource(observer.as_ptr()) })?;
    Some(unsafe { ptr.cast::<CFRunLoopSource>().as_ref() })
}

/// Adds the `CFRunLoopSource` of an `AXObserver` to the main run loop.
///
/// # Arguments
///
/// * `observer` - A reference to the `AXUIWrapper` wrapping the `AXObserverRef`.
/// * `mode` - An optional `CFRunLoopMode` for adding the source.
///
/// # Returns
///
/// `Ok(())` if the run loop source is added successfully, otherwise `Err(Error)`.
pub fn add_run_loop(observer: &AXUIWrapper, mode: Option<&CFRunLoopMode>) -> Result<()> {
    let run_loop = run_loop_source(observer);

    match CFRunLoop::main() {
        Some(main_loop) if run_loop.is_some() => {
            debug!(
                "{}: add runloop: {run_loop:?} observer {:?}",
                function_name!(),
                observer.as_ptr::<CFRunLoopSource>(),
            );
            CFRunLoop::add_source(&main_loop, run_loop, mode);
            Ok(())
        }
        _ => Err(Error::PermissionDenied(format!(
            "{}: Unable to register run loop source for observer {:?} ",
            function_name!(),
            observer.as_ptr::<CFRunLoopSource>(),
        ))),
    }
}

/// Invalidates and removes the `CFRunLoopSource` of an `AXObserver` from the main run loop.
///
/// # Arguments
///
/// * `observer` - A reference to the `AXUIWrapper` wrapping the `AXObserverRef`.
pub fn remove_run_loop(observer: &AXUIWrapper) {
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

    let mut path_buf_size = u32::try_from(path_buf.len()).ok()?;
    let path = unsafe { _NSGetExecutablePath(path_buf.as_mut_ptr(), &raw mut path_buf_size) == 0 }
        .then(|| CStr::from_bytes_until_nul(&path_buf).ok())??;
    Some(OsStr::from_bytes(path.to_bytes()).into())
}

pub fn symlink_target(path: &Path) -> Option<PathBuf> {
    if let Ok(metadata) = std::fs::symlink_metadata(path)
        && metadata.file_type().is_symlink()
        && let Ok(target) = std::fs::canonicalize(path)
    {
        Some(target)
    } else {
        None
    }
}

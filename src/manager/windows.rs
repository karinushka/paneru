use accessibility_sys::{
    AXUIElementRef, AXValueCreate, AXValueGetValue, kAXFloatingWindowSubrole, kAXPositionAttribute,
    kAXRaiseAction, kAXSizeAttribute, kAXStandardWindowSubrole, kAXUnknownSubrole,
    kAXValueTypeCGPoint, kAXValueTypeCGSize, kAXWindowRole,
};
use bevy::ecs::component::Component;
use bevy::math::IRect;
use core::ptr::NonNull;
use derive_more::{DerefMut, with_trait::Deref};
use objc2_core_foundation::{CFEqual, CFRetained, CFString, CFType, CGPoint, CGRect, CGSize};
use std::ptr::null_mut;
use std::thread;
use std::time::Duration;
use stdext::function_name;
use tracing::{debug, trace};

use super::skylight::{
    _AXUIElementGetWindow, _SLPSSetFrontProcessWithOptions, AXUIElementCopyAttributeValue,
    AXUIElementPerformAction, AXUIElementSetAttributeValue, SLPSPostEventRecordTo,
};
use crate::errors::{Error, Result};
use crate::manager::{Origin, Size, irect_from};
use crate::platform::{Pid, ProcessSerialNumber, WinID};
use crate::util::{AXUIAttributes, AXUIWrapper, MacResult};

#[derive(Debug)]
pub enum WindowPadding {
    Vertical(i32),
    Horizontal(i32),
}

pub trait WindowApi: Send + Sync {
    fn id(&self) -> WinID;
    fn frame(&self) -> IRect;
    fn element(&self) -> Option<CFRetained<AXUIWrapper>>;
    fn title(&self) -> Result<String>;
    fn child_role(&self) -> Result<bool>;
    fn role(&self) -> Result<String>;
    fn subrole(&self) -> Result<String>;
    fn is_root(&self) -> bool;
    fn is_minimized(&self) -> bool;
    fn reposition(&mut self, origin: Origin);
    fn resize(&mut self, size: Size, display_width: i32);
    fn update_frame(&mut self, display_bounds: &IRect) -> Result<()>;
    fn focus_without_raise(
        &self,
        psn: ProcessSerialNumber,
        currently_focused: &Window,
        focused_psn: ProcessSerialNumber,
    );
    fn focus_with_raise(&self, psn: ProcessSerialNumber);
    fn width_ratio(&self) -> f64;
    fn set_width_ratio(&mut self, ratio: f64);
    fn pid(&self) -> Result<Pid>;
    fn set_padding(&mut self, padding: WindowPadding);
    fn horizontal_padding(&self) -> i32;
}

#[derive(Component, Deref, DerefMut)]
pub struct Window(Box<dyn WindowApi>);

impl Window {
    pub fn new(window: Box<dyn WindowApi>) -> Self {
        Window(window)
    }
}

/// Retrieves the window ID (`WinID`) from an `AXUIElementRef`.
///
/// # Arguments
///
/// * `element_ref` - The `AXUIElementRef` to extract the window ID from.
///
/// # Returns
///
/// `Ok(WinID)` with the window ID if successful, otherwise `Err(Error)`.
pub fn ax_window_id(element_ref: AXUIElementRef) -> Result<WinID> {
    let ptr = NonNull::new(element_ref).ok_or(Error::InvalidInput(format!(
        "{}: nullptr passed as element.",
        function_name!()
    )))?;
    let mut window_id: WinID = 0;
    unsafe { _AXUIElementGetWindow(ptr.as_ptr(), &mut window_id) }.to_result(function_name!())?;
    if window_id == 0 {
        return Err(Error::InvalidInput(format!(
            "{}: Unable to get window id from element {element_ref:?}.",
            function_name!()
        )));
    }
    Ok(window_id)
}

// const CPS_ALL_WINDOWS: u32 = 0x100;
const CPS_USER_GENERATED: u32 = 0x200;
// const CPS_NO_WINDOWS: u32 = 0x400;

#[derive(Debug)]
pub struct WindowOS {
    id: WinID,
    ax_element: CFRetained<AXUIWrapper>,
    frame: IRect,
    vertical_padding: i32,
    horizontal_padding: i32,
    width_ratio: f64,
}

impl WindowOS {
    /// Creates a new `Window` instance.
    ///
    /// # Arguments
    ///
    /// * `element` - A `CFRetained<AXUIWrapper>` reference to the Accessibility UI element.
    ///
    /// # Returns
    ///
    /// `Ok(Window)` if the window is created successfully, otherwise `Err(Error)`.
    pub fn new(element: &CFRetained<AXUIWrapper>) -> Result<Self> {
        let id = ax_window_id(element.as_ptr())?;
        let window = Self {
            id,
            ax_element: element.clone(),
            frame: IRect::default(),
            vertical_padding: 0,
            horizontal_padding: 0,
            width_ratio: 0.33,
        };

        if window.is_unknown() {
            return Err(Error::invalid_window(&format!(
                "Ignoring AXUnknown window, id: {}",
                window.id()
            )));
        }

        if !window.is_real() {
            return Err(Error::invalid_window(&format!(
                "Ignoring non-real window, id: {}",
                window.id()
            )));
        }

        trace!(
            "created {} title: {} role: {} subrole: {}",
            window.id(),
            window.title().unwrap_or_default(),
            window.role().unwrap_or_default(),
            window.subrole().unwrap_or_default(),
        );
        Ok(window)
    }

    /// Checks if the window's subrole is "`AXUnknownSubrole`".
    ///
    /// # Returns
    ///
    /// `true` if the subrole is unknown, `false` otherwise.
    fn is_unknown(&self) -> bool {
        self.subrole()
            .is_ok_and(|subrole| subrole.eq(kAXUnknownSubrole))
    }

    /// Checks if the window is a "real" window based on its role and subrole.
    /// It considers standard and floating window subroles as real.
    ///
    /// # Returns
    ///
    /// `true` if the window is real, `false` otherwise.
    fn is_real(&self) -> bool {
        let role = self.role().ok();
        let subrole = self.subrole().ok();

        subrole.as_deref() == Some(kAXStandardWindowSubrole)
            || (role.as_deref() == Some(kAXWindowRole)
                && subrole.as_deref() == Some(kAXFloatingWindowSubrole))
    }

    /// Makes the window the key window for its application by sending synthesized events.
    ///
    /// # Arguments
    ///
    /// * `psn` - The process serial number of the application.
    fn make_key_window(&self, psn: &ProcessSerialNumber) {
        let window_id = self.id();
        let mut event_bytes = [0u8; 0xf8];

        event_bytes[0x04] = 0xf8;
        event_bytes[0x3a] = 0x10;
        event_bytes[0x3c..0x40].copy_from_slice(&window_id.to_ne_bytes());
        event_bytes[0x20..0x30].fill(0xff);

        event_bytes[0x08] = 0x01;
        unsafe { SLPSPostEventRecordTo(psn, event_bytes.as_ptr().cast()) };

        event_bytes[0x08] = 0x02;
        unsafe { SLPSPostEventRecordTo(psn, event_bytes.as_ptr().cast()) };
    }
}

impl WindowApi for WindowOS {
    /// Returns the ID of the window.
    ///
    /// # Returns
    ///
    /// The window ID as `WinID`.
    fn id(&self) -> WinID {
        self.id
    }

    /// Returns the current frame (`CGRect`) of the window.
    ///
    /// # Returns
    ///
    /// The window's frame as `CGRect`.
    fn frame(&self) -> IRect {
        self.frame
    }

    /// Returns the accessibility element of the window.
    ///
    /// # Returns
    ///
    /// A `CFRetained<AXUIWrapper>` representing the accessibility element.
    fn element(&self) -> Option<CFRetained<AXUIWrapper>> {
        Some(self.ax_element.clone())
    }

    /// Retrieves the title of the window.
    ///
    /// # Returns
    ///
    /// `Ok(String)` with the window title if successful, otherwise `Err(Error)`.
    fn title(&self) -> Result<String> {
        self.ax_element.title()
    }

    /// Returns true if the window has a child role.
    fn child_role(&self) -> Result<bool> {
        let role = self.role()?;
        Ok(["AXSheet", "AXDrawer"]
            .iter()
            .any(|axrole| axrole.eq(&role)))
    }

    /// Retrieves the role of the window (e.g., "`AXWindow`").
    ///
    /// # Returns
    ///
    /// `Ok(String)` with the window role if successful, otherwise `Err(Error)`.
    fn role(&self) -> Result<String> {
        self.ax_element.role()
    }

    /// Retrieves the subrole of the window (e.g., "`AXStandardWindow`").
    ///
    /// # Returns
    ///
    /// `Ok(String)` with the window subrole if successful, otherwise `Err(Error)`.
    fn subrole(&self) -> Result<String> {
        self.ax_element.subrole()
    }

    /// Checks if the window is a root window (i.e., not a child of another window).
    ///
    /// # Returns
    ///
    /// `true` if the window is a root window, `false` otherwise.
    fn is_root(&self) -> bool {
        let cftype = self.ax_element.as_ref();
        self.ax_element
            .parent()
            .is_ok_and(|parent| !CFEqual(Some(&*parent), Some(cftype)))
    }

    fn is_minimized(&self) -> bool {
        self.ax_element.minimized().is_ok_and(|minimized| minimized)
    }

    fn reposition(&mut self, origin: Origin) {
        if self.frame.min == origin {
            trace!("already in position.");
            return;
        }
        let mut point = CGPoint::new(
            f64::from(origin.x + self.horizontal_padding),
            f64::from(origin.y + self.vertical_padding),
        );
        let position_ref = unsafe {
            AXValueCreate(
                kAXValueTypeCGPoint,
                NonNull::from(&mut point).as_ptr().cast(),
            )
        };
        if let Ok(position) = AXUIWrapper::retain(position_ref) {
            unsafe {
                AXUIElementSetAttributeValue(
                    self.ax_element.as_ptr(),
                    CFString::from_static_str(kAXPositionAttribute).as_ref(),
                    position.as_ref(),
                )
            };
            let size = self.frame.size();
            self.frame.min = origin;
            self.frame.max = origin + size;
        }
    }

    fn resize(&mut self, size: Size, display_width: i32) {
        if self.frame.size() == size {
            trace!("already correct size.");
            return;
        }
        let width_padding = 2 * self.horizontal_padding;
        let height_padding = 2 * self.vertical_padding;
        let mut cgsize = CGSize::new(
            f64::from(size.x - width_padding),
            f64::from(size.y - height_padding),
        );
        let size_ref = unsafe {
            AXValueCreate(
                kAXValueTypeCGSize,
                NonNull::from(&mut cgsize).as_ptr().cast(),
            )
        };
        if let Ok(position) = AXUIWrapper::retain(size_ref) {
            unsafe {
                AXUIElementSetAttributeValue(
                    self.ax_element.as_ptr(),
                    CFString::from_static_str(kAXSizeAttribute).as_ref(),
                    position.as_ref(),
                )
            };
            self.frame.max = self.frame.min + size;
            self.width_ratio = f64::from(self.frame.width()) / f64::from(display_width);
        }
    }

    /// Updates the internal `frame` of the window by querying its current position and size from the Accessibility API.
    /// It also updates the `width_ratio`.
    ///
    /// # Arguments
    ///
    /// * `display_bounds` - An optional `CGRect` representing the bounds of the display the window is on.
    ///
    /// # Returns
    ///
    /// `Ok(())` if the frame is updated successfully, otherwise `Err(Error)`.
    fn update_frame(&mut self, display_bounds: &IRect) -> Result<()> {
        let window_ref = self.ax_element.as_ptr();

        let position = unsafe {
            let mut position_ref: *mut CFType = null_mut();
            AXUIElementCopyAttributeValue(
                window_ref,
                CFString::from_static_str(kAXPositionAttribute).as_ref(),
                &mut position_ref,
            )
            .to_result(function_name!())?;
            AXUIWrapper::retain(position_ref)?
        };
        let size = unsafe {
            let mut size_ref: *mut CFType = null_mut();
            AXUIElementCopyAttributeValue(
                window_ref,
                CFString::from_static_str(kAXSizeAttribute).as_ref(),
                &mut size_ref,
            )
            .to_result(function_name!())?;
            AXUIWrapper::retain(size_ref)?
        };

        let mut frame = CGRect::default();
        unsafe {
            AXValueGetValue(
                position.as_ptr(),
                kAXValueTypeCGPoint,
                NonNull::from(&mut frame.origin).as_ptr().cast(),
            );
            AXValueGetValue(
                size.as_ptr(),
                kAXValueTypeCGSize,
                NonNull::from(&mut frame.size).as_ptr().cast(),
            );
        }
        // if (CGRectEqualToRect(new_frame, window->frame)) {
        //     debug("%s:DEBOUNCED %s %d\n", __FUNCTION__, window->application->name, window->id);
        // }
        self.frame = irect_from(frame);

        self.frame.min.x -= self.horizontal_padding;
        self.frame.min.y -= self.vertical_padding;
        self.frame.max.x += self.horizontal_padding;
        self.frame.max.y += self.vertical_padding;

        self.width_ratio = f64::from(self.frame.width()) / f64::from(display_bounds.width());
        Ok(())
    }

    /// Focuses the window without raising it. This involves sending specific events to the process.
    ///
    /// # Arguments
    ///
    /// * `currently_focused` - A reference to the currently focused window.
    fn focus_without_raise(
        &self,
        psn: ProcessSerialNumber,
        currently_focused: &Window,
        focused_psn: ProcessSerialNumber,
    ) {
        let window_id = self.id();
        debug!("{window_id}");
        if focused_psn == psn {
            let mut event_bytes = [0u8; 0xf8];
            event_bytes[0x04] = 0xf8;
            event_bytes[0x08] = 0x0d;

            event_bytes[0x8a] = 0x02;
            event_bytes[0x3c..0x40].copy_from_slice(&currently_focused.id().to_ne_bytes());
            unsafe {
                SLPSPostEventRecordTo(&focused_psn, event_bytes.as_ptr().cast());
            }

            // Artificially delay the activation by 1ms. This is necessary because some
            // applications appear to be confused if both of the events appear instantaneously.
            thread::sleep(Duration::from_millis(20));

            event_bytes[0x8a] = 0x01;
            event_bytes[0x3c..0x40].copy_from_slice(&window_id.to_ne_bytes());
            unsafe {
                SLPSPostEventRecordTo(&psn, event_bytes.as_ptr().cast());
            }
        }

        unsafe {
            _SLPSSetFrontProcessWithOptions(&psn, window_id, CPS_USER_GENERATED);
        }
        self.make_key_window(&psn);
    }

    /// Focuses the window and raises it to the front.
    fn focus_with_raise(&self, psn: ProcessSerialNumber) {
        let window_id = self.id();
        unsafe {
            _SLPSSetFrontProcessWithOptions(&psn, window_id, CPS_USER_GENERATED);
        }
        self.make_key_window(&psn);
        let element_ref = self.ax_element.as_ptr();
        let action = CFString::from_static_str(kAXRaiseAction);
        unsafe { AXUIElementPerformAction(element_ref, &action) };
    }

    fn width_ratio(&self) -> f64 {
        self.width_ratio
    }

    fn set_width_ratio(&mut self, ratio: f64) {
        self.width_ratio = ratio;
    }

    fn pid(&self) -> Result<Pid> {
        let pid: Pid = unsafe {
            NonNull::new_unchecked(self.ax_element.as_ptr::<Pid>())
                .byte_add(0x10)
                .read()
        };
        (pid != 0).then_some(pid).ok_or(Error::InvalidInput(format!(
            "can not get pid from {:?}.",
            self.ax_element
        )))
    }

    fn set_padding(&mut self, padding: WindowPadding) {
        match padding {
            WindowPadding::Vertical(padding) => self.vertical_padding = padding,
            WindowPadding::Horizontal(padding) => self.horizontal_padding = padding,
        }
    }

    fn horizontal_padding(&self) -> i32 {
        self.horizontal_padding
    }
}

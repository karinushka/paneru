use accessibility_sys::{
    AXUIElementRef, AXValueCreate, AXValueGetValue, kAXFloatingWindowSubrole,
    kAXMinimizedAttribute, kAXParentAttribute, kAXPositionAttribute, kAXRaiseAction,
    kAXRoleAttribute, kAXSizeAttribute, kAXStandardWindowSubrole, kAXSubroleAttribute,
    kAXTitleAttribute, kAXUnknownSubrole, kAXValueTypeCGPoint, kAXValueTypeCGSize, kAXWindowRole,
};
use bevy::ecs::component::Component;
use bevy::ecs::entity::Entity;
use bevy::ecs::system::Commands;
use core::ptr::NonNull;
use log::{debug, trace};
use objc2_core_foundation::{
    CFBoolean, CFEqual, CFRetained, CFString, CFType, CGPoint, CGRect, CGSize,
};
use objc2_core_graphics::CGRectEqualToRect;
use std::ops::{Deref, DerefMut};
use std::ptr::null_mut;
use std::thread;
use std::time::Duration;
use stdext::function_name;

use crate::ecs::RepositionMarker;
use crate::ecs::params::ActiveDisplay;
use crate::errors::{Error, Result};
use crate::platform::{Pid, ProcessSerialNumber};
use crate::skylight::{
    _AXUIElementGetWindow, _SLPSSetFrontProcessWithOptions, AXUIElementCopyAttributeValue,
    AXUIElementPerformAction, AXUIElementSetAttributeValue, SLPSPostEventRecordTo, WinID,
};
use crate::util::{AXUIWrapper, get_attribute};

pub trait WindowApi: Send + Sync {
    fn id(&self) -> WinID;
    fn psn(&self) -> Option<ProcessSerialNumber>;
    fn frame(&self) -> CGRect;
    fn next_size_ratio(&self, size_ratios: &[f64]) -> f64;
    fn element(&self) -> CFRetained<AXUIWrapper>;
    fn title(&self) -> Result<String>;
    fn valid_role(&self) -> Result<bool>;
    fn role(&self) -> Result<String>;
    fn subrole(&self) -> Result<String>;
    fn is_root(&self) -> bool;
    fn is_eligible(&self) -> bool;
    fn reposition(&mut self, x: f64, y: f64, display_bounds: &CGRect);
    fn resize(&mut self, width: f64, height: f64, display_bounds: &CGRect);
    fn update_frame(&mut self, display_bounds: Option<&CGRect>) -> Result<()>;
    fn focus_without_raise(&self, currently_focused: &Window);
    fn focus_with_raise(&self);

    fn fully_visible(&self, display_bounds: &CGRect) -> bool {
        self.frame().origin.x > 0.0
            && self.frame().origin.x < display_bounds.size.width - self.frame().size.width
    }

    fn expose_window(
        &self,
        active_display: &ActiveDisplay,
        moving: Option<&RepositionMarker>,
        entity: Entity,
        commands: &mut Commands,
    ) -> CGRect {
        // Check if window needs to be fully exposed
        let window_id = self.id();
        let (mut frame, display_bounds) =
            if let Some(RepositionMarker { origin, display_id }) = moving {
                let frame = CGRect {
                    origin: *origin,
                    ..self.frame()
                };
                let bounds = active_display
                    .other()
                    .find(|display| display.id() == *display_id)
                    .map_or(active_display.bounds(), |display| display.bounds);

                (frame, bounds)
            } else {
                (self.frame(), active_display.bounds())
            };
        trace!("{}: focus original position {frame:?}", function_name!());
        let moved = if frame.origin.x + frame.size.width > display_bounds.size.width {
            trace!(
                "{}: Bumped window {} to the left",
                function_name!(),
                window_id
            );
            frame.origin.x = display_bounds.size.width - frame.size.width;
            true
        } else if frame.origin.x < 0.0 {
            trace!(
                "{}: Bumped window {} to the right",
                function_name!(),
                window_id
            );
            frame.origin.x = 0.0;
            true
        } else {
            false
        };
        if moved {
            let display_id = match moving {
                Some(marker) => marker.display_id,
                None => active_display.id(),
            };
            commands.entity(entity).try_insert(RepositionMarker {
                origin: frame.origin,
                display_id,
            });
            trace!("{}: focus resposition to {frame:?}", function_name!());
        }
        frame
    }

    fn width_ratio(&mut self, width_ratio: f64);
    fn pid(&self) -> Result<Pid>;
    fn set_psn(&mut self, psn: ProcessSerialNumber);
    fn set_eligible(&mut self, eligible: bool);
}

#[derive(Component)]
pub struct Window(Box<dyn WindowApi>);

impl Deref for Window {
    type Target = Box<dyn WindowApi>;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl DerefMut for Window {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

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
    if 0 != unsafe { _AXUIElementGetWindow(ptr.as_ptr(), &mut window_id) } || window_id == 0 {
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
    psn: Option<ProcessSerialNumber>,
    ax_element: CFRetained<AXUIWrapper>,
    frame: CGRect,
    minimized: bool,
    eligible: bool,
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
        let mut window = Self {
            id,
            psn: None,
            ax_element: element.clone(),
            frame: CGRect::default(),
            minimized: false,
            eligible: false,
            width_ratio: 0.33,
        };

        if window.is_unknown() {
            return Err(Error::invalid_window(&format!(
                "{}: Ignoring AXUnknown window, id: {}",
                function_name!(),
                window.id()
            )));
        }

        if !window.is_real() {
            return Err(Error::invalid_window(&format!(
                "{}: Ignoring non-real window, id: {}",
                function_name!(),
                window.id()
            )));
        }

        window.minimized = window.is_minimized();
        trace!(
            "{}: created {} title: {} role: {} subrole: {}",
            function_name!(),
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

    /// Checks if the window is minimized.
    ///
    /// # Returns
    ///
    /// `true` if the window is minimized, `false` otherwise.
    fn is_minimized(&self) -> bool {
        let axminimized = CFString::from_static_str(kAXMinimizedAttribute);
        get_attribute::<CFBoolean>(&self.ax_element, &axminimized)
            .map(|minimized| CFBoolean::value(&minimized))
            .is_ok_and(|minimized| minimized)
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

    /// Returns the process serial number of the window.
    fn psn(&self) -> Option<ProcessSerialNumber> {
        self.psn
    }

    /// Returns the current frame (`CGRect`) of the window.
    ///
    /// # Returns
    ///
    /// The window's frame as `CGRect`.
    fn frame(&self) -> CGRect {
        self.frame
    }

    /// Calculates the next preferred size ratio for resizing the window.
    /// It cycles through a predefined set of ratios.
    ///
    /// # Arguments
    ///
    /// * `size_ratios` - A slice of `f64` representing the preset size ratios.
    ///
    /// # Returns
    ///
    /// The next size ratio as `f64`.
    fn next_size_ratio(&self, size_ratios: &[f64]) -> f64 {
        let current = self.width_ratio;
        size_ratios
            .iter()
            .find(|r| **r > current + 0.05)
            .copied()
            .unwrap_or_else(|| *size_ratios.first().unwrap())
    }

    /// Returns the accessibility element of the window.
    ///
    /// # Returns
    ///
    /// A `CFRetained<AXUIWrapper>` representing the accessibility element.
    fn element(&self) -> CFRetained<AXUIWrapper> {
        // unsafe { NonNull::new_unchecked(self.inner().ax_element.as_ptr::<c_void>()).addr() }
        self.ax_element.clone()
    }

    /// Retrieves the title of the window.
    ///
    /// # Returns
    ///
    /// `Ok(String)` with the window title if successful, otherwise `Err(Error)`.
    fn title(&self) -> Result<String> {
        let axtitle = CFString::from_static_str(kAXTitleAttribute);
        let title = get_attribute::<CFString>(&self.ax_element, &axtitle)?;
        Ok(title.to_string())
    }

    /// Returns true if the window has a valid role.
    fn valid_role(&self) -> Result<bool> {
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
        let axrole = CFString::from_static_str(kAXRoleAttribute);
        let role = get_attribute::<CFString>(&self.ax_element, &axrole)?;
        Ok(role.to_string())
    }

    /// Retrieves the subrole of the window (e.g., "`AXStandardWindow`").
    ///
    /// # Returns
    ///
    /// `Ok(String)` with the window subrole if successful, otherwise `Err(Error)`.
    fn subrole(&self) -> Result<String> {
        let axrole = CFString::from_static_str(kAXSubroleAttribute);
        let role = get_attribute::<CFString>(&self.ax_element, &axrole)?;
        Ok(role.to_string())
    }

    /// Checks if the window is a root window (i.e., not a child of another window).
    ///
    /// # Returns
    ///
    /// `true` if the window is a root window, `false` otherwise.
    fn is_root(&self) -> bool {
        let cftype = self.ax_element.as_ref();
        let axparent = CFString::from_static_str(kAXParentAttribute);
        // if (AXUIElementCopyAttributeValue(window->ref, kAXParentAttribute, &value) == kAXErrorSuccess) {
        //     result = !(value && !CFEqual(value, window->application->ref));
        // }
        get_attribute::<CFType>(&self.ax_element, &axparent)
            .is_ok_and(|parent| !CFEqual(Some(&*parent), Some(cftype)))
    }

    /// Checks if the window is eligible for management (i.e., it is a root and a real window).
    ///
    /// # Returns
    ///
    /// `true` if the window is eligible, `false` otherwise.
    fn is_eligible(&self) -> bool {
        // bool result = window->is_root && (window_is_real(window) || window_check_rule_flag(window, WINDOW_RULE_MANAGED));
        self.eligible
    }

    /// Repositions the window to the specified x and y coordinates.
    ///
    /// # Arguments
    ///
    /// * `x` - The new x-coordinate for the window's origin.
    /// * `y` - The new y-coordinate for the window's origin.
    /// * `display_bounds` - The `CGRect` of the display.
    fn reposition(&mut self, x: f64, y: f64, display_bounds: &CGRect) {
        if (self.frame.origin.x - x).abs() < 0.1 && (self.frame.origin.y - y).abs() < 0.1 {
            trace!("{}: already in position.", function_name!());
            return;
        }
        let mut point = CGPoint::new(x + display_bounds.origin.x, y + display_bounds.origin.y);
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
            self.frame.origin.x = x;
            self.frame.origin.y = y;
        }
    }

    /// Resizes the window to the specified width and height. It also updates the `width_ratio`.
    ///
    /// # Arguments
    ///
    /// * `width` - The new width of the window.
    /// * `height` - The new height of the window.
    /// * `display_bounds` - The `CGRect` representing the bounds of the display the window is on.
    fn resize(&mut self, width: f64, height: f64, display_bounds: &CGRect) {
        if (self.frame.size.width - width).abs() < 0.1
            && (self.frame.size.height - height).abs() < 0.1
        {
            trace!("{}: already correct size.", function_name!());
            return;
        }
        let mut size = CGSize::new(width, height);
        let size_ref =
            unsafe { AXValueCreate(kAXValueTypeCGSize, NonNull::from(&mut size).as_ptr().cast()) };
        if let Ok(position) = AXUIWrapper::retain(size_ref) {
            unsafe {
                AXUIElementSetAttributeValue(
                    self.ax_element.as_ptr(),
                    CFString::from_static_str(kAXSizeAttribute).as_ref(),
                    position.as_ref(),
                )
            };
            self.frame.size = size;
            self.width_ratio = size.width / display_bounds.size.width;
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
    fn update_frame(&mut self, display_bounds: Option<&CGRect>) -> Result<()> {
        let window_ref = self.ax_element.as_ptr();

        let position = unsafe {
            let mut position_ref: *mut CFType = null_mut();
            AXUIElementCopyAttributeValue(
                window_ref,
                CFString::from_static_str(kAXPositionAttribute).as_ref(),
                &mut position_ref,
            );
            AXUIWrapper::retain(position_ref)?
        };
        let size = unsafe {
            let mut size_ref: *mut CFType = null_mut();
            AXUIElementCopyAttributeValue(
                window_ref,
                CFString::from_static_str(kAXSizeAttribute).as_ref(),
                &mut size_ref,
            );
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
        if !CGRectEqualToRect(frame, self.frame) {
            self.frame = frame;
            self.width_ratio = if let Some(display_bounds) = display_bounds {
                frame.size.width / display_bounds.size.width
            } else {
                0.5
            };
        }
        Ok(())
    }

    /// Focuses the window without raising it. This involves sending specific events to the process.
    ///
    /// # Arguments
    ///
    /// * `currently_focused` - A reference to the currently focused window.
    fn focus_without_raise(&self, currently_focused: &Window) {
        let Some((psn, focused_psn)) = self.psn().zip(currently_focused.psn()) else {
            return;
        };
        let window_id = self.id();
        debug!("{}: {window_id}", function_name!());
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
    fn focus_with_raise(&self) {
        let Some(psn) = self.psn else {
            return;
        };
        let window_id = self.id();
        unsafe {
            _SLPSSetFrontProcessWithOptions(&psn, window_id, CPS_USER_GENERATED);
        }
        self.make_key_window(&psn);
        let element_ref = self.ax_element.as_ptr();
        let action = CFString::from_static_str(kAXRaiseAction);
        unsafe { AXUIElementPerformAction(element_ref, &action) };
    }

    fn width_ratio(&mut self, width_ratio: f64) {
        self.width_ratio = width_ratio;
    }

    fn pid(&self) -> Result<Pid> {
        let pid: Pid = unsafe {
            NonNull::new_unchecked(self.ax_element.as_ptr::<Pid>())
                .byte_add(0x10)
                .read()
        };
        (pid != 0).then_some(pid).ok_or(Error::InvalidInput(format!(
            "{}: can not get pid from {:?}.",
            function_name!(),
            self.ax_element
        )))
    }

    fn set_psn(&mut self, psn: ProcessSerialNumber) {
        self.psn = Some(psn);
    }

    fn set_eligible(&mut self, eligible: bool) {
        self.eligible = eligible;
    }
}

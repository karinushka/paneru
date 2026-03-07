#![allow(dead_code)]

use std::ptr::NonNull;

use objc2_core_foundation::{CFArray, CFRetained, CFType, CGAffineTransform, CGPoint, CGRect};
use objc2_core_graphics::{CGError, CGWindowID, CGWindowLevel};

use crate::platform::ConnID;

unsafe extern "C" {
    /* Transaction lifecycle methods. */
    pub fn SLSTransactionCreate(cid: ConnID) -> *const CFType;
    pub fn SLSTransactionCommit(transaction: *const CFType, synchronous: i32) -> CGError;
    pub fn SLSTransactionCommitUsingMethod(transaction: *const CFType, method: u32) -> CGError;

    /* Window transaction methods. */
    pub fn SLSTransactionSetWindowLevel(
        transaction: *const CFType,
        wid: CGWindowID,
        level: CGWindowLevel,
    ) -> CGError;
    pub fn SLSTransactionSetWindowSubLevel(
        transaction: *const CFType,
        wid: CGWindowID,
        level: CGWindowLevel,
    ) -> CGError;
    pub fn SLSTransactionSetWindowShape(
        transaction: *const CFType,
        wid: CGWindowID,
        x_offset: f32,
        y_offset: f32,
        shape: *const CFType,
    ) -> CGError;
    pub fn SLSTransactionMoveWindowWithGroup(
        transaction: *const CFType,
        wid: CGWindowID,
        point: CGPoint,
    ) -> CGError;
    pub fn SLSTransactionOrderWindow(
        transaction: *const CFType,
        wid: CGWindowID,
        order: i32,
        rel_wid: CGWindowID,
    ) -> CGError;
    pub fn SLSTransactionSetWindowAlpha(
        transaction: *const CFType,
        wid: CGWindowID,
        alpha: f32,
    ) -> CGError;
    pub fn SLSTransactionSetWindowResolution(
        resolution: f64,
        transaction: *const CFType,
        wid: CGWindowID,
    ) -> CGError;
    pub fn SLSTransactionSetWindowSystemAlpha(
        alpha: f32,
        transaction: *const CFType,
        wid: CGWindowID,
    ) -> CGError;
    pub fn SLSTransactionSetWindowTransform(
        transaction: *const CFType,
        wid: CGWindowID,
        _: i32,
        _: i32,
        transform: CGAffineTransform,
    ) -> CGError;
    pub fn SLSTransactionResetWindow(transaction: *const CFType, wid: CGWindowID) -> CGError;
    pub fn SLSTransactionResetWindowSubLevel(
        transaction: *const CFType,
        wid: CGWindowID,
    ) -> CGError;
    pub fn SLSTransactionSetWindowSystemLevel(
        transaction: *const CFType,
        wid: CGWindowID,
        level: CGWindowLevel,
    ) -> CGError;
    pub fn SLSTransactionClearWindowSystemLevel(
        transaction: *const CFType,
        wid: CGWindowID,
    ) -> CGError;
    pub fn SLSTransactionSetWindowCornerRadius(
        radius: f64,
        transaction: *const CFType,
        wid: CGWindowID,
    ) -> CGError;
    pub fn SLSTransactionClearWindowCornerRadius(
        transaction: *const CFType,
        wid: CGWindowID,
    ) -> CGError;
    pub fn SLSTransactionSetWindowSystemCornerRadius(
        radius: f64,
        transaction: *const CFType,
        wid: CGWindowID,
    ) -> CGError;
    pub fn SLSTransactionClearWindowSystemCornerRadius(
        transaction: *const CFType,
        wid: CGWindowID,
    ) -> CGError;
    pub fn SLSTransactionSetWindowHasKeyAppearance(
        transaction: *const CFType,
        wid: CGWindowID,
        enabled: bool,
    ) -> CGError;
    pub fn SLSTransactionSetWindowHasMainAppearance(
        transaction: *const CFType,
        wid: CGWindowID,
        enabled: bool,
    ) -> CGError;
    pub fn SLSTransactionSetWindowPrefersCurrentSpace(
        transaction: *const CFType,
        wid: CGWindowID,
        enabled: bool,
    ) -> CGError;

    /* Space transaction methods. */
    pub fn SLSTransactionAddWindowToSpace(
        transaction: *const CFType,
        wid: CGWindowID,
        space_id: u64,
    ) -> CGError;
    pub fn SLSTransactionRemoveWindowFromSpace(
        transaction: *const CFType,
        wid: CGWindowID,
        space_id: u64,
    ) -> CGError;
    pub fn SLSTransactionMoveWindowsToManagedSpace(
        transaction: *const CFType,
        window_ids: *const CFArray,
        space_id: u64,
    ) -> CGError;
    pub fn SLSTransactionSetSpaceAbsoluteLevel(
        transaction: *const CFType,
        space_id: u64,
        level: i32,
    ) -> CGError;
    pub fn SLSTransactionSetSpaceAlpha(
        alpha: f32,
        transaction: *const CFType,
        space_id: u64,
    ) -> CGError;
    pub fn SLSTransactionShowSpace(transaction: *const CFType, space_id: u64) -> CGError;

    /* Surface transaction methods. */
    pub fn SLSTransactionBindSurface(
        transaction: *const CFType,
        wid: CGWindowID,
        surface_id: u32,
        slot: i32,
        context_id: u32,
        options: u32,
    ) -> CGError;
    pub fn SLSTransactionOrderSurface(
        transaction: *const CFType,
        wid: CGWindowID,
        surface_id: u32,
        order: i8,
        relative_surface_id: u32,
    ) -> CGError;
    pub fn SLSTransactionSetSurfaceBounds(
        x: f64,
        y: f64,
        width: f64,
        height: f64,
        transaction: *const CFType,
        wid: CGWindowID,
        surface_id: u32,
    ) -> CGError;
    pub fn SLSTransactionSetSurfaceOpacity(
        transaction: *const CFType,
        wid: CGWindowID,
        surface_id: u32,
        opacity: u8,
    ) -> CGError;
    pub fn SLSTransactionSetSurfaceResolution(
        resolution: f64,
        transaction: *const CFType,
        wid: CGWindowID,
        surface_id: u32,
    ) -> CGError;
    pub fn SLSTransactionRemoveSurface(
        transaction: *const CFType,
        wid: CGWindowID,
        surface_id: u32,
    ) -> CGError;
}

#[repr(u32)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SLSTransactionCommitMethod {
    /* Standard transaction commit methods. */
    Async = 0,
    Sync = 1,
    /* Core Animation-backed transaction commit methods. */
    CoreAnimation = 3,
    CoreAnimationDeferred = 4,
}

pub struct SLSTransactionGroup {
    transaction: CFRetained<CFType>,
}

impl SLSTransactionGroup {
    pub fn new(cid: ConnID) -> Option<Self> {
        let transaction = NonNull::new(unsafe { SLSTransactionCreate(cid).cast_mut() })?;
        Some(Self {
            // SLSTransactionCreate follows CoreFoundation create-rule semantics.
            transaction: unsafe { CFRetained::from_raw(transaction) },
        })
    }

    pub fn window_transaction(&self, window_id: CGWindowID) -> SLSWindowTransaction<'_> {
        SLSWindowTransaction {
            group: self,
            window_id,
        }
    }

    #[inline]
    fn as_ptr(&self) -> *const CFType {
        &raw const *self.transaction
    }

    pub fn commit(&self, synchronous: bool) -> CGError {
        unsafe { SLSTransactionCommit(self.as_ptr(), i32::from(synchronous)) }
    }

    /* Space transaction methods. */
    pub fn add_window_to_space(&self, window_id: CGWindowID, space_id: u64) -> CGError {
        unsafe { SLSTransactionAddWindowToSpace(self.as_ptr(), window_id, space_id) }
    }

    pub fn remove_window_from_space(&self, window_id: CGWindowID, space_id: u64) -> CGError {
        unsafe { SLSTransactionRemoveWindowFromSpace(self.as_ptr(), window_id, space_id) }
    }

    pub fn move_windows_to_managed_space(&self, window_ids: &CFArray, space_id: u64) -> CGError {
        unsafe {
            SLSTransactionMoveWindowsToManagedSpace(
                self.as_ptr(),
                window_ids as *const CFArray,
                space_id,
            )
        }
    }

    pub fn set_space_absolute_level(&self, space_id: u64, level: i32) -> CGError {
        unsafe { SLSTransactionSetSpaceAbsoluteLevel(self.as_ptr(), space_id, level) }
    }

    pub fn set_space_alpha(&self, space_id: u64, alpha: f32) -> CGError {
        unsafe { SLSTransactionSetSpaceAlpha(alpha, self.as_ptr(), space_id) }
    }

    pub fn show_space(&self, space_id: u64) -> CGError {
        unsafe { SLSTransactionShowSpace(self.as_ptr(), space_id) }
    }

    /* Surface transaction methods. */
    pub fn bind_surface(
        &self,
        window_id: CGWindowID,
        surface_id: u32,
        slot: i32,
        context_id: u32,
        options: u32,
    ) -> CGError {
        unsafe {
            SLSTransactionBindSurface(
                self.as_ptr(),
                window_id,
                surface_id,
                slot,
                context_id,
                options,
            )
        }
    }

    pub fn order_surface(
        &self,
        window_id: CGWindowID,
        surface_id: u32,
        order: i8,
        relative_surface_id: u32,
    ) -> CGError {
        unsafe {
            SLSTransactionOrderSurface(
                self.as_ptr(),
                window_id,
                surface_id,
                order,
                relative_surface_id,
            )
        }
    }

    pub fn set_surface_bounds(
        &self,
        window_id: CGWindowID,
        surface_id: u32,
        bounds: CGRect,
    ) -> CGError {
        unsafe {
            SLSTransactionSetSurfaceBounds(
                bounds.origin.x,
                bounds.origin.y,
                bounds.size.width,
                bounds.size.height,
                self.as_ptr(),
                window_id,
                surface_id,
            )
        }
    }

    pub fn set_surface_opacity(
        &self,
        window_id: CGWindowID,
        surface_id: u32,
        opacity: u8,
    ) -> CGError {
        unsafe { SLSTransactionSetSurfaceOpacity(self.as_ptr(), window_id, surface_id, opacity) }
    }

    pub fn set_surface_resolution(
        &self,
        window_id: CGWindowID,
        surface_id: u32,
        resolution: f64,
    ) -> CGError {
        unsafe {
            SLSTransactionSetSurfaceResolution(resolution, self.as_ptr(), window_id, surface_id)
        }
    }

    pub fn remove_surface(&self, window_id: CGWindowID, surface_id: u32) -> CGError {
        unsafe { SLSTransactionRemoveSurface(self.as_ptr(), window_id, surface_id) }
    }
}

pub struct SLSWindowTransaction<'a> {
    group: &'a SLSTransactionGroup,
    window_id: CGWindowID,
}

impl SLSWindowTransaction<'_> {
    pub fn window_id(&self) -> CGWindowID {
        self.window_id
    }

    pub fn set_level(&self, level: CGWindowLevel) -> CGError {
        unsafe { SLSTransactionSetWindowLevel(self.group.as_ptr(), self.window_id, level) }
    }

    pub fn set_sub_level(&self, level: CGWindowLevel) -> CGError {
        unsafe { SLSTransactionSetWindowSubLevel(self.group.as_ptr(), self.window_id, level) }
    }

    pub fn set_shape(&self, x_offset: f32, y_offset: f32, shape: *const CFType) -> CGError {
        unsafe {
            SLSTransactionSetWindowShape(
                self.group.as_ptr(),
                self.window_id,
                x_offset,
                y_offset,
                shape,
            )
        }
    }

    pub fn move_with_group(&self, point: CGPoint) -> CGError {
        unsafe { SLSTransactionMoveWindowWithGroup(self.group.as_ptr(), self.window_id, point) }
    }

    pub fn order(&self, order: i32, relative_window_id: CGWindowID) -> CGError {
        unsafe {
            SLSTransactionOrderWindow(
                self.group.as_ptr(),
                self.window_id,
                order,
                relative_window_id,
            )
        }
    }

    pub fn set_alpha(&self, alpha: f32) -> CGError {
        unsafe { SLSTransactionSetWindowAlpha(self.group.as_ptr(), self.window_id, alpha) }
    }

    /* Window scalar properties. */
    pub fn set_resolution(&self, resolution: f64) -> CGError {
        unsafe {
            SLSTransactionSetWindowResolution(resolution, self.group.as_ptr(), self.window_id)
        }
    }

    pub fn set_system_alpha(&self, alpha: f32) -> CGError {
        unsafe { SLSTransactionSetWindowSystemAlpha(alpha, self.group.as_ptr(), self.window_id) }
    }

    pub fn set_transform(&self, transform: CGAffineTransform) -> CGError {
        unsafe {
            SLSTransactionSetWindowTransform(self.group.as_ptr(), self.window_id, 0, 0, transform)
        }
    }

    pub fn set_system_level(&self, level: CGWindowLevel) -> CGError {
        unsafe { SLSTransactionSetWindowSystemLevel(self.group.as_ptr(), self.window_id, level) }
    }

    /* Window reset methods. */
    pub fn reset(&self) -> CGError {
        unsafe { SLSTransactionResetWindow(self.group.as_ptr(), self.window_id) }
    }

    pub fn reset_sub_level(&self) -> CGError {
        unsafe { SLSTransactionResetWindowSubLevel(self.group.as_ptr(), self.window_id) }
    }

    pub fn clear_system_level(&self) -> CGError {
        unsafe { SLSTransactionClearWindowSystemLevel(self.group.as_ptr(), self.window_id) }
    }

    /* Window corner radius methods. */
    pub fn set_corner_radius(&self, radius: f64) -> CGError {
        unsafe { SLSTransactionSetWindowCornerRadius(radius, self.group.as_ptr(), self.window_id) }
    }

    pub fn clear_corner_radius(&self) -> CGError {
        unsafe { SLSTransactionClearWindowCornerRadius(self.group.as_ptr(), self.window_id) }
    }

    pub fn set_system_corner_radius(&self, radius: f64) -> CGError {
        unsafe {
            SLSTransactionSetWindowSystemCornerRadius(radius, self.group.as_ptr(), self.window_id)
        }
    }

    pub fn clear_system_corner_radius(&self) -> CGError {
        unsafe { SLSTransactionClearWindowSystemCornerRadius(self.group.as_ptr(), self.window_id) }
    }

    /* Window appearance methods. */
    pub fn set_has_key_appearance(&self, enabled: bool) -> CGError {
        unsafe {
            SLSTransactionSetWindowHasKeyAppearance(self.group.as_ptr(), self.window_id, enabled)
        }
    }

    pub fn set_has_main_appearance(&self, enabled: bool) -> CGError {
        unsafe {
            SLSTransactionSetWindowHasMainAppearance(self.group.as_ptr(), self.window_id, enabled)
        }
    }

    pub fn set_prefers_current_space(&self, enabled: bool) -> CGError {
        unsafe {
            SLSTransactionSetWindowPrefersCurrentSpace(self.group.as_ptr(), self.window_id, enabled)
        }
    }

    pub fn commit(&self, synchronous: bool) -> CGError {
        self.group.commit(synchronous)
    }
}

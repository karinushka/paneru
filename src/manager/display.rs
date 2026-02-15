use bevy::ecs::component::Component;
use core::ptr::NonNull;
use objc2_core_foundation::{CFRetained, CFString, CFUUID, CGRect};
use objc2_core_graphics::CGDirectDisplayID;
use objc2_foundation::NSRect;
use stdext::function_name;

use super::skylight::{CGDisplayCreateUUIDFromDisplayID, CGDisplayGetDisplayIDFromUUID};
use crate::{
    ecs::DockPosition,
    errors::{Error, Result},
};

/// `Display` represents a physical monitor and manages its associated workspaces and window panes.
/// Each display has a unique ID, bounds, and a collection of `LayoutStrip`s for different spaces.
#[derive(Component, Debug)]
pub struct Display {
    /// The unique identifier for this display provided by Core Graphics.
    id: CGDirectDisplayID,
    /// The physical bounds (origin and size) of the display.
    pub bounds: CGRect,
    /// The height of the menubar on this display.
    pub menubar_height: f64,
}

impl Display {
    /// Creates a new `Display` instance.
    ///
    /// # Arguments
    ///
    /// * `id` - The `CGDirectDisplayID` of the display.
    /// * `spaces` - A vector of space IDs associated with this display.
    /// * `bounds` - The `CGRect` representing the bounds of the display.
    /// * `menubar_height` - The height of the menubar on this display.
    ///
    /// # Returns
    ///
    /// A new `Display` instance.
    pub fn new(id: CGDirectDisplayID, bounds: CGRect, menubar_height: u32) -> Self {
        Self {
            id,
            bounds,
            menubar_height: menubar_height.into(),
        }
    }

    /// Converts a `CGDirectDisplayID` to a `CFUUID` string.
    ///
    /// # Arguments
    ///
    /// * `id` - The `CGDirectDisplayID` to convert.
    ///
    /// # Returns
    ///
    /// `Ok(CFRetained<CFString>)` with the UUID string if successful, otherwise `Err(Error)`.
    pub fn uuid_from_id(id: CGDirectDisplayID) -> Result<CFRetained<CFString>> {
        unsafe {
            let uuid = NonNull::new(CGDisplayCreateUUIDFromDisplayID(id))
                .map(|ptr| CFRetained::from_raw(ptr))
                .ok_or(Error::InvalidInput(format!(
                    "{}: can not create uuid from {id}.",
                    function_name!()
                )))?;
            CFUUID::new_string(None, Some(&uuid)).ok_or(Error::InvalidInput(format!(
                "{}: can not create string from {uuid:?}.",
                function_name!()
            )))
        }
    }

    /// Converts a `CFUUID` string to a `CGDirectDisplayID`.
    ///
    /// # Arguments
    ///
    /// * `uuid` - The `CFRetained<CFString>` representing the UUID.
    ///
    /// # Returns
    ///
    /// `Ok(u32)` with the `CGDirectDisplayID` if successful, otherwise `Err(Error)`.
    pub fn id_from_uuid(uuid: &CFRetained<CFString>) -> Result<u32> {
        unsafe {
            let id = CFUUID::from_string(None, Some(uuid)).ok_or(Error::NotFound(format!(
                "{}: can not convert from {uuid}.",
                function_name!()
            )))?;
            Ok(CGDisplayGetDisplayIDFromUUID(&id))
        }
    }

    /// Returns the `CGDirectDisplayID` of the display.
    ///
    /// # Returns
    ///
    /// The `CGDirectDisplayID` of the display.
    pub fn id(&self) -> CGDirectDisplayID {
        self.id
    }

    pub fn locate_dock(&self, visible_frame: &NSRect) -> DockPosition {
        if self.bounds.origin.x < visible_frame.origin.x {
            DockPosition::Left(visible_frame.origin.x - self.bounds.origin.x)
        } else if visible_frame.size.width < self.bounds.size.width {
            DockPosition::Right(self.bounds.size.width - visible_frame.size.width)
        } else if visible_frame.size.height < self.bounds.size.height - self.menubar_height {
            DockPosition::Bottom(
                self.bounds.size.height - visible_frame.size.height - self.menubar_height,
            )
        } else {
            DockPosition::Hidden
        }
    }
}

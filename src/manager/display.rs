use bevy::{ecs::component::Component, math::IRect};
use core::ptr::NonNull;
use objc2_core_foundation::{CFRetained, CFString, CFUUID};
use objc2_core_graphics::CGDirectDisplayID;
use stdext::function_name;

use super::skylight::{CGDisplayCreateUUIDFromDisplayID, CGDisplayGetDisplayIDFromUUID};
use crate::{
    ecs::DockPosition,
    errors::{Error, Result},
    manager::Origin,
};

/// `Display` represents a physical monitor and manages its associated workspaces and window panes.
/// Each display has a unique ID, bounds, and a collection of `LayoutStrip`s for different spaces.
#[derive(Component, Debug)]
pub struct Display {
    /// The unique identifier for this display provided by Core Graphics.
    id: CGDirectDisplayID,
    /// The physical bounds (origin and size) of the display.
    bounds: IRect,
    /// The height of the menubar on this display.
    menubar_height: i32,
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
    pub fn new(id: CGDirectDisplayID, bounds: IRect, menubar_height: i32) -> Self {
        Self {
            id,
            bounds,
            menubar_height,
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

    pub fn locate_dock(&self, visible_frame: &IRect) -> DockPosition {
        if self.bounds.min.x < visible_frame.min.x {
            DockPosition::Left(visible_frame.min.x - self.bounds.min.x)
        } else if visible_frame.width() < self.bounds.width() {
            DockPosition::Right(self.bounds.max.x - visible_frame.max.x)
        } else if visible_frame.height() < self.bounds.height() - self.menubar_height {
            DockPosition::Bottom(
                self.bounds.height() - visible_frame.height() - self.menubar_height,
            )
        } else {
            DockPosition::Hidden
        }
    }

    pub fn absolute_coords(&self, origin: Origin) -> Origin {
        self.bounds.min + origin
    }

    pub fn bounds(&self) -> IRect {
        let mut bounds = self.bounds;
        bounds.min.y += self.menubar_height;
        bounds
    }

    pub fn width(&self) -> i32 {
        self.bounds().width()
    }

    pub fn height(&self) -> i32 {
        self.bounds().height()
    }

    pub fn menubar_height(&self) -> i32 {
        self.menubar_height
    }
}

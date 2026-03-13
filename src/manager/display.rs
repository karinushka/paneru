use bevy::{ecs::component::Component, math::IRect};
use core::ptr::NonNull;
use objc2_core_foundation::{CFRetained, CFString, CFUUID};
use objc2_core_graphics::CGDirectDisplayID;
use stdext::function_name;
use tracing::{Level, instrument};

use super::skylight::{CGDisplayCreateUUIDFromDisplayID, CGDisplayGetDisplayIDFromUUID};
use crate::{
    config::Config,
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
    bounds: IRect,
    /// The height of the menubar on this display (from the system).
    menubar_height: i32,
    /// Optional config override for the menubar height.
    menubar_height_override: Option<i32>,
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
            menubar_height_override: None,
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
        } else if visible_frame.height() < self.bounds.height() - self.menubar_height() {
            DockPosition::Bottom(
                self.bounds.height() - visible_frame.height() - self.menubar_height(),
            )
        } else {
            DockPosition::Hidden
        }
    }

    pub fn bounds(&self) -> IRect {
        let mut bounds = self.bounds;
        bounds.min.y += self.menubar_height();
        bounds
    }

    pub fn width(&self) -> i32 {
        self.bounds().width()
    }

    pub fn menubar_height(&self) -> i32 {
        self.menubar_height_override.unwrap_or(self.menubar_height)
    }

    pub fn set_menubar_height_override(&mut self, height: Option<i32>) {
        self.menubar_height_override = height;
    }

    #[instrument(level = Level::TRACE, skip_all, ret)]
    pub fn actual_display_bounds(&self, dock: Option<&DockPosition>, config: &Config) -> IRect {
        let (pad_top, pad_right, pad_bottom, pad_left) = config.edge_padding();
        let mut viewport = self.bounds();
        viewport.min.x += pad_left;
        viewport.min.y += pad_top;
        viewport.max.x -= pad_right;
        viewport.max.y -= pad_bottom;

        match dock {
            Some(DockPosition::Bottom(size)) => viewport.max.y -= size,
            Some(DockPosition::Left(size)) => {
                viewport.min.x += size;
            }
            Some(DockPosition::Right(size)) => viewport.max.x -= size,
            _ => (),
        }
        viewport
    }
}

use bevy::ecs::component::Component;
use bevy::ecs::entity::Entity;
use core::ptr::NonNull;
use objc2_core_foundation::{CFRetained, CFString, CFUUID, CGRect};
use objc2_core_graphics::CGDirectDisplayID;
use objc2_foundation::NSRect;
use std::collections::VecDeque;
use stdext::function_name;
use tracing::debug;

use super::skylight::{CGDisplayCreateUUIDFromDisplayID, CGDisplayGetDisplayIDFromUUID};
use crate::{
    ecs::DockPosition,
    errors::{Error, Result},
    platform::WorkspaceId,
};

/// Represents a single panel within a `LayoutStrip`, which can either hold a single window or a stack of windows.
#[derive(Clone, Debug)]
pub enum Column {
    /// A panel containing a single window, identified by its `Entity`.
    Single(Entity),
    /// A panel containing a stack of windows, ordered from top to bottom.
    Stack(Vec<Entity>),
}

impl Column {
    /// Returns the top window entity in the panel.
    /// For a `Single` panel, it's the contained window. For a `Stack`, it's the first window in the stack.
    pub fn top(&self) -> Option<Entity> {
        match self {
            Column::Single(id) => Some(id),
            Column::Stack(stack) => stack.first(),
        }
        .copied()
    }
}

/// `LayoutStrip` manages a horizontal strip of `Panel`s, where each panel can contain a single window or a stack of windows.
/// It provides methods for manipulating the arrangement and access to windows within the pane.
#[derive(Component, Debug, Default)]
pub struct LayoutStrip {
    id: WorkspaceId,
    columns: VecDeque<Column>,
}

impl LayoutStrip {
    pub fn new(id: WorkspaceId) -> Self {
        Self {
            id,
            columns: VecDeque::new(),
        }
    }

    /// Finds the index of a window within the pane.
    /// If the window is part of a stack, it returns the index of the panel containing the stack.
    ///
    /// # Arguments
    ///
    /// * `entity` - Entity of the window to find.
    ///
    /// # Returns
    ///
    /// `Ok(usize)` with the index if found, otherwise `Err(Error)`.
    pub fn index_of(&self, entity: Entity) -> Result<usize> {
        self.columns
            .iter()
            .position(|column| match column {
                Column::Single(id) => *id == entity,
                Column::Stack(stack) => stack.contains(&entity),
            })
            .ok_or(Error::NotFound(format!(
                "{}: can not find window {entity} in the current pane.",
                function_name!()
            )))
    }

    /// Inserts a window ID into the pane at a specified position.
    /// The new window will be placed as a `Single` panel.
    ///
    /// # Arguments
    ///
    /// * `after` - The index at which to insert the window. If `after` is greater than or equal to the entity length,
    ///   the window is appended to the end.
    /// * `entity` - Entity of the window to insert.
    pub fn insert_at(&mut self, after: usize, entity: Entity) {
        let index = after;
        if index >= self.len() {
            self.columns.push_back(Column::Single(entity));
        } else {
            self.columns.insert(index, Column::Single(entity));
        }
    }

    /// Appends a window ID as a `Single` panel to the end of the pane.
    ///
    /// # Arguments
    ///
    /// * `entity` - Entity of the window to append.
    pub fn append(&mut self, entity: Entity) {
        self.columns.push_back(Column::Single(entity));
    }

    /// Removes a window ID from the pane.
    /// If the window is part of a stack, it is removed from the stack.
    /// If the stack becomes empty or contains only one window, the panel type adjusts accordingly.
    ///
    /// # Arguments
    ///
    /// * `entity` - Entity of the window to remove.
    pub fn remove(&mut self, entity: Entity) {
        let removed = self
            .index_of(entity)
            .ok()
            .and_then(|index| self.columns.remove(index).zip(Some(index)));

        if let Some((Column::Stack(mut stack), index)) = removed {
            stack.retain(|id| *id != entity);
            if stack.len() > 1 {
                self.columns.insert(index, Column::Stack(stack));
            } else if let Some(remaining_id) = stack.first() {
                self.columns.insert(index, Column::Single(*remaining_id));
            }
        }
    }

    /// Retrieves the `Panel` at a specified index in the pane.
    ///
    /// # Arguments
    ///
    /// * `at` - The index from which to retrieve the panel.
    ///
    /// # Returns
    ///
    /// `Ok(Panel)` with the panel if the index is valid, otherwise `Err(Error)`.
    pub fn get(&self, at: usize) -> Result<Column> {
        self.columns
            .get(at)
            .cloned()
            .ok_or(Error::InvalidInput(format!(
                "{}: {at} out of bounds",
                function_name!()
            )))
    }

    /// Swaps the positions of two panels within the pane.
    ///
    /// # Arguments
    ///
    /// * `left` - The index of the first panel.
    /// * `right` - The index of the second panel.
    pub fn swap(&mut self, left: usize, right: usize) {
        self.columns.swap(left, right);
    }

    /// Returns the number of panels in the pane.
    ///
    /// # Returns
    ///
    /// The number of panels as `usize`.
    pub fn len(&self) -> usize {
        self.columns.len()
    }

    /// Returns the first `Panel` in the pane.
    ///
    /// # Returns
    ///
    /// `Ok(Panel)` with the first panel, otherwise `Err(Error)` if the pane is empty.
    pub fn first(&self) -> Result<Column> {
        self.columns.front().cloned().ok_or(Error::NotFound(format!(
            "{}: can not find first element.",
            function_name!()
        )))
    }

    /// Returns the last `Panel` in the pane.
    ///
    /// # Returns
    ///
    /// `Ok(Panel)` with the last panel, otherwise `Err(Error)` if the pane is empty.
    pub fn last(&self) -> Result<Column> {
        self.columns.back().cloned().ok_or(Error::NotFound(format!(
            "{}: can not find last element.",
            function_name!()
        )))
    }

    pub fn right_neighbour(&self, entity: Entity) -> Option<Entity> {
        let index = self.index_of(entity).ok()?;
        (index < self.columns.len())
            .then_some(index + 1)
            .and_then(|index| self.columns.get(index))
            .and_then(Column::top)
    }

    pub fn left_neighbour(&self, entity: Entity) -> Option<Entity> {
        let index = self.index_of(entity).ok()?;
        (index > 0)
            .then(|| index - 1)
            .and_then(|index| self.columns.get(index))
            .and_then(Column::top)
    }

    /// Stacks the window with the given ID onto the panel to its left.
    /// If the window is already in a stack or is the leftmost window, no action is taken.
    ///
    /// # Arguments
    ///
    /// * `entity` - Entity of the window to stack.
    ///
    /// # Returns
    ///
    /// `Ok(())` if the stacking is successful or not needed, otherwise `Err(Error)` if the window is not found.
    pub fn stack(&mut self, entity: Entity) -> Result<()> {
        let index = self.index_of(entity)?;
        if index == 0 {
            // Can not stack to the left if left most window already.
            return Ok(());
        }
        if let Column::Stack(_) = self.columns[index] {
            // Already in a stack, do nothing.
            return Ok(());
        }

        self.columns.remove(index);
        let column = self.columns.remove(index - 1);
        if let Some(column) = column {
            let newstack = match column {
                Column::Stack(mut stack) => {
                    stack.push(entity);
                    stack
                }
                Column::Single(id) => vec![id, entity],
            };

            debug!("Stacked windows: {newstack:#?}");
            self.columns.insert(index - 1, Column::Stack(newstack));
        }

        Ok(())
    }

    /// Unstacks the window with the given ID from its entity stack.
    /// If the window is in a single panel, no action is taken.
    ///
    /// # Arguments
    ///
    /// * `entity` - Entity of the window to unstack.
    ///
    /// # Returns
    ///
    /// `Ok(())` if the unstacking is successful or not needed, otherwise `Err(Error)` if the window is not found.
    pub fn unstack(&mut self, entity: Entity) -> Result<()> {
        let index = self.index_of(entity)?;
        if let Column::Single(_) = self.columns[index] {
            // Can not unstack a single pane
            return Ok(());
        }

        let column = self.columns.remove(index);
        if let Some(column) = column {
            let newstack = match column {
                Column::Stack(mut stack) => {
                    stack.retain(|id| *id != entity);
                    if stack.len() == 1 {
                        Column::Single(stack[0])
                    } else {
                        Column::Stack(stack)
                    }
                }
                Column::Single(_) => unreachable!("Is checked at the start of the function"),
            };
            // Re-insert the unstacked window as a single panel
            self.columns.insert(index, Column::Single(entity));
            // Re-insert the modified stack (if not empty) at the original position
            self.columns.insert(index, newstack);
        }

        Ok(())
    }

    /// Returns a vector of all window IDs present in all panels within the pane, maintaining their order.
    /// For stacked panels, all windows in the stack are included.
    ///
    /// # Returns
    ///
    /// A `Vec<Entity>` containing all window IDs.
    pub fn all_windows(&self) -> Vec<Entity> {
        self.columns
            .iter()
            .flat_map(|column| match column {
                Column::Single(entity) => vec![*entity],
                Column::Stack(ids) => ids.clone(),
            })
            .collect()
    }

    pub fn all_columns(&self) -> Vec<Entity> {
        self.columns.iter().filter_map(Column::top).collect()
    }

    pub fn id(&self) -> WorkspaceId {
        self.id
    }
}

impl std::fmt::Display for LayoutStrip {
    /// Formats the `LayoutStrip` for display, showing the arrangement of its panels.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let out = self
            .columns
            .iter()
            .map(|column| format!("{column:?}"))
            .collect::<Vec<_>>();
        write!(f, "[{}]", out.join(", "))
    }
}

/// `Display` represents a physical monitor and manages its associated workspaces and window panes.
/// Each display has a unique ID, bounds, and a collection of `LayoutStrip`s for different spaces.
#[derive(Component)]
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

#[cfg(test)]
mod tests {
    use super::*;
    use bevy::prelude::*;

    fn setup_world_and_strip() -> (World, LayoutStrip, Vec<Entity>) {
        let mut world = World::new();
        let entities = world.spawn_batch(vec![(), (), ()]).collect::<Vec<Entity>>();

        let mut strip = LayoutStrip::default();
        strip.append(entities[0]);
        strip.append(entities[1]);
        strip.append(entities[2]);

        (world, strip, entities)
    }

    #[test]
    fn test_window_pane_index_of() {
        let (_world, strip, entities) = setup_world_and_strip();
        assert_eq!(strip.index_of(entities[0]).unwrap(), 0);
        assert_eq!(strip.index_of(entities[1]).unwrap(), 1);
        assert_eq!(strip.index_of(entities[2]).unwrap(), 2);
    }

    #[test]
    fn test_window_pane_swap() {
        let (_world, mut strip, entities) = setup_world_and_strip();
        strip.swap(0, 2);
        assert_eq!(strip.index_of(entities[2]).unwrap(), 0);
        assert_eq!(strip.index_of(entities[0]).unwrap(), 2);
    }

    #[test]
    fn test_window_pane_stack_and_unstack() {
        let (_world, mut strip, entities) = setup_world_and_strip();

        // Stack [1] onto [0]
        strip.stack(entities[1]).unwrap();
        assert_eq!(strip.len(), 2);
        assert_eq!(strip.index_of(entities[0]).unwrap(), 0);
        assert_eq!(strip.index_of(entities[1]).unwrap(), 0); // Both in the same panel

        // Check internal structure
        match strip.get(0).unwrap() {
            Column::Stack(stack) => {
                assert_eq!(stack.len(), 2);
                assert_eq!(stack[0], entities[0]);
                assert_eq!(stack[1], entities[1]);
            }
            Column::Single(_) => panic!("Expected a stack"),
        }

        // Unstack [0]
        strip.unstack(entities[0]).unwrap();
        assert_eq!(strip.len(), 3);
        assert_eq!(strip.index_of(entities[1]).unwrap(), 0);
        assert_eq!(strip.index_of(entities[0]).unwrap(), 1);
        assert_eq!(strip.index_of(entities[2]).unwrap(), 2);
    }
}

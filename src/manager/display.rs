use bevy::ecs::component::Component;
use bevy::ecs::entity::Entity;
use core::ptr::NonNull;
use log::debug;
use objc2_core_foundation::{CFRetained, CFString, CFUUID, CGRect};
use objc2_core_graphics::CGDirectDisplayID;
use std::collections::{HashMap, VecDeque};
use stdext::function_name;

use crate::errors::{Error, Result};
use crate::skylight::{CGDisplayCreateUUIDFromDisplayID, CGDisplayGetDisplayIDFromUUID};

/// Represents a single panel within a `WindowPane`, which can either hold a single window or a stack of windows.
#[derive(Clone, Debug)]
pub enum Panel {
    /// A panel containing a single window, identified by its `Entity`.
    Single(Entity),
    /// A panel containing a stack of windows, ordered from top to bottom.
    Stack(Vec<Entity>),
}

impl Panel {
    /// Returns the top window entity in the panel.
    /// For a `Single` panel, it's the contained window. For a `Stack`, it's the first window in the stack.
    pub fn top(&self) -> Option<Entity> {
        match self {
            Panel::Single(id) => Some(id),
            Panel::Stack(stack) => stack.first(),
        }
        .copied()
    }
}

/// `WindowPane` manages a horizontal strip of `Panel`s, where each panel can contain a single window or a stack of windows.
/// It provides methods for manipulating the arrangement and access to windows within the pane.
#[derive(Debug, Default)]
pub struct WindowPane {
    pane: VecDeque<Panel>,
}

impl WindowPane {
    /// Finds the index of a window within the pane.
    /// If the window is part of a stack, it returns the index of the panel containing the stack.
    ///
    /// # Arguments
    ///
    /// * `window_id` - The ID of the window to find.
    ///
    /// # Returns
    ///
    /// `Ok(usize)` with the index if found, otherwise `Err(Error)`.
    pub fn index_of(&self, window_id: Entity) -> Result<usize> {
        self.pane
            .iter()
            .position(|panel| match panel {
                Panel::Single(id) => *id == window_id,
                Panel::Stack(stack) => stack.contains(&window_id),
            })
            .ok_or(Error::NotFound(format!(
                "{}: can not find window {window_id} in the current pane.",
                function_name!()
            )))
    }

    /// Inserts a window ID into the pane at a specified position.
    /// The new window will be placed as a `Single` panel.
    ///
    /// # Arguments
    ///
    /// * `after` - The index at which to insert the window. If `after` is greater than or equal to the current length,
    ///   the window is appended to the end.
    /// * `window_id` - The ID of the window to insert.
    pub fn insert_at(&mut self, after: usize, window_id: Entity) {
        let index = after;
        if index >= self.len() {
            self.pane.push_back(Panel::Single(window_id));
        }
        self.pane.insert(index, Panel::Single(window_id));
    }

    /// Appends a window ID as a `Single` panel to the end of the pane.
    ///
    /// # Arguments
    ///
    /// * `window_id` - The ID of the window to append.
    pub fn append(&mut self, window_id: Entity) {
        self.pane.push_back(Panel::Single(window_id));
    }

    /// Removes a window ID from the pane.
    /// If the window is part of a stack, it is removed from the stack.
    /// If the stack becomes empty or contains only one window, the panel type adjusts accordingly.
    ///
    /// # Arguments
    ///
    /// * `window_id` - The ID of the window to remove.
    pub fn remove(&mut self, window_id: Entity) {
        let removed = self
            .index_of(window_id)
            .ok()
            .and_then(|index| self.pane.remove(index).zip(Some(index)));

        if let Some((Panel::Stack(mut stack), index)) = removed {
            stack.retain(|id| *id != window_id);
            if stack.len() > 1 {
                self.pane.insert(index, Panel::Stack(stack));
            } else if let Some(remaining_id) = stack.first() {
                self.pane.insert(index, Panel::Single(*remaining_id));
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
    pub fn get(&self, at: usize) -> Result<Panel> {
        self.pane
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
        self.pane.swap(left, right);
    }

    /// Returns the number of panels in the pane.
    ///
    /// # Returns
    ///
    /// The number of panels as `usize`.
    pub fn len(&self) -> usize {
        self.pane.len()
    }

    /// Returns the first `Panel` in the pane.
    ///
    /// # Returns
    ///
    /// `Ok(Panel)` with the first panel, otherwise `Err(Error)` if the pane is empty.
    pub fn first(&self) -> Result<Panel> {
        self.pane.front().cloned().ok_or(Error::NotFound(format!(
            "{}: can not find first element.",
            function_name!()
        )))
    }

    /// Returns the last `Panel` in the pane.
    ///
    /// # Returns
    ///
    /// `Ok(Panel)` with the last panel, otherwise `Err(Error)` if the pane is empty.
    pub fn last(&self) -> Result<Panel> {
        self.pane.back().cloned().ok_or(Error::NotFound(format!(
            "{}: can not find last element.",
            function_name!()
        )))
    }

    /// Iterates over panels to the right of a given window's panel, applying an accessor function to each.
    /// Iteration starts from the panel immediately to the right of the window's panel.
    ///
    /// # Arguments
    ///
    /// * `window_id` - The ID of the starting window.
    /// * `accessor` - A closure that takes a `&Panel` and returns `true` to continue iteration, `false` to stop.
    ///
    /// # Returns
    ///
    /// `Ok(())` if successful, otherwise `Err(Error)` if the starting window is not found.
    pub fn access_right_of(
        &self,
        window_id: Entity,
        mut accessor: impl FnMut(&Panel) -> bool,
    ) -> Result<()> {
        let index = self.index_of(window_id)?;
        for panel in self.pane.range(1 + index..) {
            if !accessor(panel) {
                break;
            }
        }
        Ok(())
    }

    /// Iterates over panels to the left of a given window's panel (in reverse order), applying an accessor function to each.
    /// Iteration starts from the panel immediately to the left of the window's panel.
    ///
    /// # Arguments
    ///
    /// * `window_id` - The ID of the starting window.
    /// * `accessor` - A closure that takes a `&Panel` and returns `true` to continue iteration, `false` to stop.
    ///
    /// # Returns
    ///
    /// `Ok(())` if successful, otherwise `Err(Error)` if the starting window is not found.
    pub fn access_left_of(
        &self,
        window_id: Entity,
        mut accessor: impl FnMut(&Panel) -> bool,
    ) -> Result<()> {
        let index = self.index_of(window_id)?;
        for panel in self.pane.range(0..index).rev() {
            // NOTE: left side iterates backwards.
            if !accessor(panel) {
                break;
            }
        }
        Ok(())
    }

    /// Stacks the window with the given ID onto the panel to its left.
    /// If the window is already in a stack or is the leftmost window, no action is taken.
    ///
    /// # Arguments
    ///
    /// * `window_id` - The ID of the window to stack.
    ///
    /// # Returns
    ///
    /// `Ok(())` if the stacking is successful or not needed, otherwise `Err(Error)` if the window is not found.
    pub fn stack(&mut self, window_id: Entity) -> Result<()> {
        let index = self.index_of(window_id)?;
        if index == 0 {
            // Can not stack to the left if left most window already.
            return Ok(());
        }
        if let Panel::Stack(_) = self.pane[index] {
            // Already in a stack, do nothing.
            return Ok(());
        }

        self.pane.remove(index);
        let panel = self.pane.remove(index - 1);
        if let Some(panel) = panel {
            let newstack = match panel {
                Panel::Stack(mut stack) => {
                    stack.push(window_id);
                    stack
                }
                Panel::Single(id) => vec![id, window_id],
            };

            debug!("Stacked windows: {newstack:#?}");
            self.pane.insert(index - 1, Panel::Stack(newstack));
        }

        Ok(())
    }

    /// Unstacks the window with the given ID from its current stack.
    /// If the window is in a single panel, no action is taken.
    ///
    /// # Arguments
    ///
    /// * `window_id` - The ID of the window to unstack.
    ///
    /// # Returns
    ///
    /// `Ok(())` if the unstacking is successful or not needed, otherwise `Err(Error)` if the window is not found.
    pub fn unstack(&mut self, window_id: Entity) -> Result<()> {
        let index = self.index_of(window_id)?;
        if let Panel::Single(_) = self.pane[index] {
            // Can not unstack a single pane
            return Ok(());
        }

        let panel = self.pane.remove(index);
        if let Some(panel) = panel {
            let newstack = match panel {
                Panel::Stack(mut stack) => {
                    stack.retain(|id| *id != window_id);
                    if stack.len() == 1 {
                        Panel::Single(stack[0])
                    } else {
                        Panel::Stack(stack)
                    }
                }
                Panel::Single(_) => unreachable!("Is checked at the start of the function"),
            };
            // Re-insert the unstacked window as a single panel
            self.pane.insert(index, Panel::Single(window_id));
            // Re-insert the modified stack (if not empty) at the original position
            self.pane.insert(index, newstack);
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
        self.pane
            .iter()
            .flat_map(|panel| match panel {
                Panel::Single(window_id) => vec![*window_id],
                Panel::Stack(ids) => ids.clone(),
            })
            .collect()
    }
}

impl std::fmt::Display for WindowPane {
    /// Formats the `WindowPane` for display, showing the arrangement of its panels.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let out = self
            .pane
            .iter()
            .map(|panel| format!("{panel:?}"))
            .collect::<Vec<_>>();
        write!(f, "[{}]", out.join(", "))
    }
}

/// `Display` represents a physical monitor and manages its associated workspaces and window panes.
/// Each display has a unique ID, bounds, and a collection of `WindowPane`s for different spaces.
#[derive(Component)]
pub struct Display {
    /// The unique identifier for this display provided by Core Graphics.
    id: CGDirectDisplayID,
    /// A map of space IDs to their corresponding `WindowPane`s.
    pub spaces: HashMap<u64, WindowPane>,
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
    pub fn new(
        id: CGDirectDisplayID,
        spaces: Vec<u64>,
        bounds: CGRect,
        menubar_height: u32,
    ) -> Self {
        let spaces = spaces
            .into_iter()
            .map(|id| (id, WindowPane::default()))
            .collect::<HashMap<_, _>>();
        Self {
            id,
            spaces,
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

    /// Removes a window from all panes across all spaces on this display.
    ///
    /// # Arguments
    ///
    /// * `window_id` - The ID of the window to remove.
    pub fn remove_window(&mut self, window_id: Entity) {
        self.spaces
            .values_mut()
            .for_each(|pane| pane.remove(window_id));
    }

    /// Retrieves an immutable reference to the `WindowPane` corresponding to the active space on this display.
    ///
    /// # Arguments
    ///
    /// * `space_id` - The ID of the active space.
    ///
    /// # Returns
    ///
    /// `Ok(&WindowPane)` if the active panel is found, otherwise `Err(Error)`.
    pub fn active_panel(&self, space_id: u64) -> Result<&WindowPane> {
        self.spaces.get(&space_id).ok_or(Error::NotFound(format!(
            "{}: space {space_id}.",
            function_name!()
        )))
    }

    /// Retrieves a mutable reference to the `WindowPane` corresponding to the active space on this display.
    ///
    /// # Arguments
    ///
    /// * `space_id` - The ID of the active space.
    ///
    /// # Returns
    ///
    /// `Ok(&mut WindowPane)` if the active panel is found, otherwise `Err(Error)`.
    pub fn active_panel_mut(&mut self, space_id: u64) -> Result<&mut WindowPane> {
        self.spaces
            .get_mut(&space_id)
            .ok_or(Error::NotFound(format!(
                "{}: space {space_id}.",
                function_name!()
            )))
    }

    /// Returns the `CGDirectDisplayID` of the display.
    ///
    /// # Returns
    ///
    /// The `CGDirectDisplayID` of the display.
    pub fn id(&self) -> CGDirectDisplayID {
        self.id
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bevy::prelude::*;

    fn setup_world_and_pane() -> (World, WindowPane, Vec<Entity>) {
        let mut world = World::new();
        let entities = world.spawn_batch(vec![(), (), ()]).collect::<Vec<Entity>>();

        let mut pane = WindowPane::default();
        pane.append(entities[0]);
        pane.append(entities[1]);
        pane.append(entities[2]);

        (world, pane, entities)
    }

    #[test]
    fn test_window_pane_index_of() {
        let (_world, pane, entities) = setup_world_and_pane();
        assert_eq!(pane.index_of(entities[0]).unwrap(), 0);
        assert_eq!(pane.index_of(entities[1]).unwrap(), 1);
        assert_eq!(pane.index_of(entities[2]).unwrap(), 2);
    }

    #[test]
    fn test_window_pane_swap() {
        let (_world, mut pane, entities) = setup_world_and_pane();
        pane.swap(0, 2);
        assert_eq!(pane.index_of(entities[2]).unwrap(), 0);
        assert_eq!(pane.index_of(entities[0]).unwrap(), 2);
    }

    #[test]
    fn test_window_pane_stack_and_unstack() {
        let (_world, mut pane, entities) = setup_world_and_pane();

        // Stack [1] onto [0]
        pane.stack(entities[1]).unwrap();
        assert_eq!(pane.len(), 2);
        assert_eq!(pane.index_of(entities[0]).unwrap(), 0);
        assert_eq!(pane.index_of(entities[1]).unwrap(), 0); // Both in the same panel

        // Check internal structure
        match pane.get(0).unwrap() {
            Panel::Stack(stack) => {
                assert_eq!(stack.len(), 2);
                assert_eq!(stack[0], entities[0]);
                assert_eq!(stack[1], entities[1]);
            }
            Panel::Single(_) => panic!("Expected a stack"),
        }

        // Unstack [0]
        pane.unstack(entities[0]).unwrap();
        assert_eq!(pane.len(), 3);
        assert_eq!(pane.index_of(entities[1]).unwrap(), 0);
        assert_eq!(pane.index_of(entities[0]).unwrap(), 1);
        assert_eq!(pane.index_of(entities[2]).unwrap(), 2);
    }
}

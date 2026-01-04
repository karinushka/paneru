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

#[derive(Clone, Debug)]
pub enum Panel {
    Single(Entity),
    Stack(Vec<Entity>),
}

impl Panel {
    /// Returns the top window entity in the panel.
    pub fn top(&self) -> Option<Entity> {
        match self {
            Panel::Single(id) => Some(id),
            Panel::Stack(stack) => stack.first(),
        }
        .copied()
    }
}

#[derive(Debug, Default)]
pub struct WindowPane {
    pane: VecDeque<Panel>,
}

impl WindowPane {
    /// Finds the index of a window within the pane.
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
    ///
    /// # Arguments
    ///
    /// * `after` - The index after which to insert the window.
    /// * `window_id` - The ID of the window to insert.
    ///
    /// If the index is out of bounds, it will simply append at the end.
    pub fn insert_at(&mut self, after: usize, window_id: Entity) {
        let index = after;
        if index >= self.len() {
            self.pane.push_back(Panel::Single(window_id));
        }
        self.pane.insert(index, Panel::Single(window_id));
    }

    /// Appends a window ID to the end of the pane.
    ///
    /// # Arguments
    ///
    /// * `window_id` - The ID of the window to append.
    pub fn append(&mut self, window_id: Entity) {
        self.pane.push_back(Panel::Single(window_id));
    }

    /// Removes a window ID from the pane.
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
            } else {
                self.pane.insert(index, Panel::Single(stack[0]));
            }
        }
    }

    /// Retrieves the window panel at a specified index in the pane.
    ///
    /// # Arguments
    ///
    /// * `at` - The index from which to retrieve the window panel.
    ///
    /// # Returns
    ///
    /// `Ok(Panel)` with the window panel if the index is valid, otherwise `Err(Error)`.
    pub fn get(&self, at: usize) -> Result<Panel> {
        self.pane
            .get(at)
            .cloned()
            .ok_or(Error::InvalidInput(format!(
                "{}: {at} out of bounds",
                function_name!()
            )))
    }

    /// Swaps the positions of two windows within the pane.
    ///
    /// # Arguments
    ///
    /// * `left` - The index of the first window.
    /// * `right` - The index of the second window.
    pub fn swap(&mut self, left: usize, right: usize) {
        self.pane.swap(left, right);
    }

    /// Returns the number of windows in the pane.
    ///
    /// # Returns
    ///
    /// The number of windows as `usize`.
    pub fn len(&self) -> usize {
        self.pane.len()
    }

    /// Returns the first panel in the pane.
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

    /// Returns the last panel in the pane.
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

    /// Iterates over windows to the right of a given window, applying an accessor function to each.
    /// Iteration stops if the accessor returns `false`.
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

    /// Iterates over windows to the left of a given window (in reverse order), applying an accessor function to each.
    /// Iteration stops if the accessor returns `false`.
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
    ///
    /// # Arguments
    ///
    /// * `window_id` - The ID of the window to stack.
    pub fn stack(&mut self, window_id: Entity) -> Result<()> {
        let index = self.index_of(window_id)?;
        if index == 0 {
            // Can not stack to the left if left most window already.
            return Ok(());
        }
        if let Panel::Stack(_) = self.pane[index] {
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
    ///
    /// # Arguments
    ///
    /// * `window_id` - The ID of the window to unstack.
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
            self.pane.insert(index, Panel::Single(window_id));
            self.pane.insert(index, newstack);
        }

        Ok(())
    }

    /// Returns a vector of all window IDs in the pane.
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
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let out = self
            .pane
            .iter()
            .map(|panel| format!("{panel:?}"))
            .collect::<Vec<_>>();
        write!(f, "[{}]", out.join(", "))
    }
}

#[derive(Component)]
pub struct Display {
    id: CGDirectDisplayID,
    // uuid: CFRetained<CFString>,
    // Map of workspaces, containing panels of windows.
    pub spaces: HashMap<u64, WindowPane>,
    pub bounds: CGRect,
    pub menubar_height: f64,
}

impl Display {
    /// Creates a new `Display` instance.
    ///
    /// # Arguments
    ///
    /// * `id` - The `CGDirectDisplayID` of the display.
    /// * `spaces` - A vector of space IDs associated with this display.
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

    /// Retrieves the `WindowPane` corresponding to the active space on this display.
    ///
    /// # Arguments
    ///
    /// * `cid` - The connection ID.
    ///
    /// # Returns
    ///
    /// `Ok(&mut WindowPane)` if the active panel is found, otherwise `Err(Error)`.
    pub fn active_panel(&self, space_id: u64) -> Result<&WindowPane> {
        self.spaces.get(&space_id).ok_or(Error::NotFound(format!(
            "{}: space {space_id}.",
            function_name!()
        )))
    }

    pub fn active_panel_mut(&mut self, space_id: u64) -> Result<&mut WindowPane> {
        self.spaces
            .get_mut(&space_id)
            .ok_or(Error::NotFound(format!(
                "{}: space {space_id}.",
                function_name!()
            )))
    }

    /// Returns the ID of the display.
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

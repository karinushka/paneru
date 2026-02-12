use bevy::ecs::component::Component;
use bevy::ecs::entity::Entity;
use objc2_core_foundation::CGRect;
use std::collections::VecDeque;
use stdext::function_name;
use tracing::debug;

use crate::errors::{Error, Result};
use crate::platform::WorkspaceId;

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

    pub fn absolute_positions<W>(
        &self,
        get_window_frame: &W,
    ) -> impl Iterator<Item = (&Column, f64)>
    where
        W: Fn(Entity) -> Option<CGRect>,
    {
        let mut left_edge = 0.0;

        self.all_columns()
            .into_iter()
            .filter_map(|entity| {
                let frame = get_window_frame(entity);
                let column = self
                    .index_of(entity)
                    .ok()
                    .and_then(|index| self.columns.get(index));
                column.zip(frame)
            })
            .map(move |(column, frame)| {
                let temp = left_edge;
                left_edge += frame.size.width;
                (column, temp)
            })
    }

    pub fn calculate_layout<W>(
        &self,
        offset: f64,
        viewport: &CGRect,
        get_window_frame: &W,
    ) -> impl Iterator<Item = (Entity, CGRect)>
    where
        W: Fn(Entity) -> Option<CGRect>,
    {
        const MIN_WINDOW_HEIGHT: f64 = 200.0;
        const WINDOW_HIDDEN_THRESHOLD: f64 = 10.0;

        debug!("viewport offset of {offset}");
        self.absolute_positions(get_window_frame)
            .filter_map(move |(column, position)| {
                let windows = match column {
                    Column::Single(entity) => vec![*entity],
                    Column::Stack(stack) => stack.clone(),
                };
                let current_heights = windows
                    .iter()
                    .filter_map(|&entity| get_window_frame(entity))
                    .map(|frame| frame.size.height)
                    .collect::<Vec<_>>();
                let heights =
                    binpack_heights(&current_heights, MIN_WINDOW_HEIGHT, viewport.size.height)?;

                let column_width = windows
                    .first()
                    .and_then(|&entity| get_window_frame(entity))
                    .map(|frame| frame.size.width)?;
                let mut prev_y = 0.0;
                let frames = windows
                    .into_iter()
                    .zip(heights)
                    .filter_map(|(entity, height)| {
                        let mut frame = get_window_frame(entity)?;
                        let top_left = position - offset;
                        frame.origin.x = top_left.clamp(
                            // Make sure a small sliver of a window is visible.
                            WINDOW_HIDDEN_THRESHOLD - frame.size.width,
                            viewport.size.width - WINDOW_HIDDEN_THRESHOLD,
                        );
                        frame.origin.y = prev_y;

                        prev_y += height;
                        frame.size.height = height;
                        frame.size.width = column_width;

                        Some((entity, frame))
                    })
                    .collect::<Vec<_>>();

                Some(frames)
            })
            .flatten()
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

pub fn binpack_heights(heights: &[f64], min_height: f64, total_height: f64) -> Option<Vec<f64>> {
    let mut count = heights.len();
    let mut output = vec![];

    loop {
        let mut idx = 0;

        let mut remaining = total_height;
        while idx < count {
            let remaining_windows = u32::try_from(heights.len() - idx).unwrap();

            if heights[idx] < remaining {
                if idx + 1 == count {
                    output.push(remaining);
                } else {
                    output.push(heights[idx]);
                }
                remaining -= heights[idx];
            } else if remaining >= min_height * f64::from(remaining_windows) {
                output.push(remaining);
                remaining = 0.0;
            } else {
                break;
            }
            idx += 1;
        }

        if idx == count {
            break;
        }
        count -= 1;
        output.clear();
    }

    let remaining = heights.len() - count;
    if remaining > 0 && count > 0 {
        count -= 1;
        output.truncate(count);
        let sum = output.iter().fold(0.0, |acc, height| acc + height);
        let avg_height =
            ((total_height - sum) / f64::from(u32::try_from(remaining + 1).unwrap())).floor();
        if avg_height < min_height {
            return None;
        }

        while count < heights.len() {
            output.push(avg_height);
            count += 1;
        }
    }

    Some(output)
}

#[cfg(test)]
mod tests {
    use super::*;
    use bevy::prelude::*;
    use objc2_core_foundation::{CGPoint, CGSize};

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

    #[test]
    fn test_binpack() {
        const MIN_HEIGHT: f64 = 100.0;
        let heights = [300.0, 300.0, 300.0, 300.0];

        let out = binpack_heights(&heights, MIN_HEIGHT, 1500.0).unwrap();
        assert_eq!(out, vec![300.0, 300.0, 300.0, 600.0]);

        let out = binpack_heights(&heights, MIN_HEIGHT, 1024.0).unwrap();
        assert_eq!(out, vec![300.0, 300.0, 300.0, 124.0]);

        let out = binpack_heights(&heights, MIN_HEIGHT, 800.0).unwrap();
        assert_eq!(out, vec![300.0, 300.0, 100.0, 100.0]);

        let out = binpack_heights(&heights, MIN_HEIGHT, 440.0).unwrap();
        assert_eq!(out, vec![110.0, 110.0, 110.0, 110.0]);

        let out = binpack_heights(&heights, MIN_HEIGHT, 390.0);
        assert_eq!(out, None);
    }

    #[test]
    fn test_layout_positioning() {
        let mut world = World::new();
        let entities = world
            .spawn_batch(vec![(), (), (), ()])
            .collect::<Vec<Entity>>();
        let sizes = [
            CGRect::new(CGPoint::new(0.0, 0.0), CGSize::new(300.0, 300.0)),
            CGRect::new(CGPoint::new(0.0, 0.0), CGSize::new(300.0, 300.0)),
            CGRect::new(CGPoint::new(0.0, 0.0), CGSize::new(300.0, 300.0)),
            CGRect::new(CGPoint::new(0.0, 0.0), CGSize::new(300.0, 300.0)),
        ];

        let mut strip = LayoutStrip::default();
        strip.append(entities[0]);
        strip.append(entities[1]);
        strip.append(entities[2]);
        strip.append(entities[3]);

        _ = strip.stack(entities[2]);
        let viewport = CGRect::new(CGPoint::new(0.0, 0.0), CGSize::new(600.0, 500.0));
        let get_window_frame = |_| Some(sizes[0]);
        let out = strip
            .calculate_layout(50.0, &viewport, &get_window_frame)
            .collect::<Vec<_>>();

        let xpos = out
            .iter()
            .map(|(_, frame)| frame.origin.x)
            .collect::<Vec<_>>();
        assert_eq!(xpos, vec![-50.0, 250.0, 250.0, 550.0]);

        let height = out
            .iter()
            .map(|(_, frame)| frame.size.height)
            .collect::<Vec<_>>();
        assert_eq!(height, vec![500.0, 300.0, 200.0, 500.0]);
    }
}

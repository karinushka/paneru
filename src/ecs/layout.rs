use bevy::ecs::change_detection::DetectChangesMut as _;
use bevy::ecs::component::Component;
use bevy::ecs::entity::Entity;
use bevy::ecs::hierarchy::ChildOf;
use bevy::ecs::query::{Changed, Has, Or, With, Without};
use bevy::ecs::system::{ParallelCommands, Populated, Query, Res};
use bevy::math::IRect;
use std::collections::VecDeque;
use stdext::function_name;
use tracing::{Level, instrument, trace};

use crate::config::Config;
use crate::ecs::params::Windows;
use crate::ecs::{
    Bounds, DockPosition, LayoutPosition, Position, RepositionMarker, ReshuffleAroundMarker,
    Scrolling,
};
use crate::errors::{Error, Result};
use crate::manager::{Display, Window};
use crate::platform::WorkspaceId;

/// Represents an item within a stack, which can either be a single window or a group of tabs.
#[derive(Clone, Debug, PartialEq)]
pub enum StackItem {
    /// A single window within the stack.
    Single(Entity),
    /// A group of tabs within the stack.
    Tabs(Vec<Entity>),
}

impl StackItem {
    /// Returns the top window entity in the item.
    pub fn top(&self) -> Option<Entity> {
        match self {
            StackItem::Single(id) => Some(*id),
            StackItem::Tabs(tabs) => tabs.first().copied(),
        }
    }

    /// Returns true if the item contains the specified entity.
    pub fn contains(&self, entity: Entity) -> bool {
        match self {
            StackItem::Single(id) => *id == entity,
            StackItem::Tabs(tabs) => tabs.contains(&entity),
        }
    }

    /// Returns all window entities within the item.
    pub fn all_windows(&self) -> Vec<Entity> {
        match self {
            StackItem::Single(id) => vec![*id],
            StackItem::Tabs(tabs) => tabs.clone(),
        }
    }
}

/// Represents a single panel within a `LayoutStrip`, which can either hold a single window, a stack of items, or a group of tabs.
#[derive(Clone, Debug)]
pub enum Column {
    /// A panel containing a single window, identified by its `Entity`.
    Single(Entity),
    /// A panel containing a stack of items (windows or tabs), ordered from top to bottom.
    Stack(Vec<StackItem>),
    /// A panel containing a group of native tabs, with the active "Leader" at the front.
    Tabs(Vec<Entity>),
    Fullscren(Entity),
}

impl Column {
    /// Returns the top window entity in the panel.
    /// For a `Single` panel, it's the contained window.
    /// For a `Stack` or `Tabs`, it's the first window in the vector.
    pub fn top(&self) -> Option<Entity> {
        match self {
            Column::Single(id) | Column::Fullscren(id) => Some(*id),
            Column::Stack(stack) => stack.first().and_then(StackItem::top),
            Column::Tabs(tabs) => tabs.first().copied(),
        }
    }

    /// Returns the entity at the given index, or the last entity if the index exceeds the size.
    pub fn at_or_last(&self, index: usize) -> Option<Entity> {
        match self {
            Column::Single(id) | Column::Fullscren(id) => Some(*id),
            Column::Stack(stack) => stack
                .get(index)
                .or_else(|| stack.last())
                .and_then(StackItem::top),
            Column::Tabs(tabs) => tabs.first().copied(),
        }
    }

    /// Returns the position of an entity within this column (0 for Single/Tabs, index for Stack).
    pub fn position_of(&self, entity: Entity) -> Option<usize> {
        match self {
            Column::Single(id) | Column::Fullscren(id) => (*id == entity).then_some(0),
            Column::Stack(stack) => stack.iter().position(|item| item.contains(entity)),
            Column::Tabs(tabs) => tabs.contains(&entity).then_some(0),
        }
    }

    /// Moves the specified entity to the front of the vector (index 0).
    /// This is used to change the Leader of a Tab group.
    pub fn move_to_front(&mut self, entity: Entity) {
        match self {
            Column::Single(_) | Column::Fullscren(_) => {}
            Column::Stack(stack) => {
                if let Some(StackItem::Tabs(tabs)) =
                    stack.iter_mut().find(|item| item.contains(entity))
                    && let Some(pos) = tabs.iter().position(|&e| e == entity)
                {
                    tabs.swap(0, pos);
                }
            }
            Column::Tabs(tabs) => {
                if let Some(pos) = tabs.iter().position(|&e| e == entity) {
                    tabs.swap(0, pos);
                }
            }
        }
    }
}

/// `LayoutStrip` manages a horizontal strip of `Panel`s, where each panel can contain a single window or a stack of windows.
/// It provides methods for manipulating the arrangement and access to windows within the pane.
#[derive(Component, Debug, Default)]
pub struct LayoutStrip {
    id: WorkspaceId,
    pub virtual_index: u32,
    columns: VecDeque<Column>,
}

impl LayoutStrip {
    pub fn new(id: WorkspaceId, virtual_index: u32) -> Self {
        Self {
            id,
            virtual_index,
            columns: VecDeque::new(),
        }
    }

    pub fn fullscreen(id: WorkspaceId, entity: Entity) -> Self {
        let mut columns = VecDeque::new();
        columns.push_back(Column::Fullscren(entity));
        Self {
            id,
            virtual_index: 0,
            columns,
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
                Column::Single(id) | Column::Fullscren(id) => *id == entity,
                Column::Stack(stack) => stack.iter().any(|item| item.contains(entity)),
                Column::Tabs(stack) => stack.contains(&entity),
            })
            .ok_or(Error::NotFound(format!(
                "{}: can not find window {entity} in the current pane.",
                function_name!()
            )))
    }

    /// Returns `true` if the strip contains the given entity.
    pub fn contains(&self, entity: Entity) -> bool {
        self.columns.iter().any(|column| match column {
            Column::Single(id) | Column::Fullscren(id) => *id == entity,
            Column::Stack(stack) => stack.iter().any(|item| item.contains(entity)),
            Column::Tabs(stack) => stack.contains(&entity),
        })
    }

    /// Inserts a window ID into the pane at a specified position.
    /// The new window will be placed as a `Single` panel.
    ///
    /// # Arguments
    ///
    /// * `after` - The index at which to insert the window. If `after` is greater than or equal to the entity length,
    ///   the window is appended to the end.
    /// * `entity` - Entity of the window to insert.
    pub fn insert_at(&mut self, index: usize, entity: Entity) {
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
        if self.contains(entity) {
            return;
        }
        self.columns.push_back(Column::Single(entity));
    }

    /// Converts a column containing `leader` to a `Tabs` column and adds `follower`.
    pub fn convert_to_tabs(&mut self, leader: Entity, follower: Entity) -> Result<()> {
        let index = self.index_of(leader)?;
        let column = self.columns.remove(index).unwrap();
        match column {
            Column::Single(id) | Column::Fullscren(id) => {
                self.columns.insert(index, Column::Tabs(vec![id, follower]));
            }
            Column::Stack(mut items) => {
                if let Some(pos) = items.iter().position(|item| item.contains(leader)) {
                    match &mut items[pos] {
                        StackItem::Single(id) => {
                            let id = *id;
                            items[pos] = StackItem::Tabs(vec![id, follower]);
                        }
                        StackItem::Tabs(tabs) => {
                            if !tabs.contains(&follower) {
                                tabs.push(follower);
                            }
                        }
                    }
                }
                self.columns.insert(index, Column::Stack(items));
            }
            Column::Tabs(mut tabs) => {
                if !tabs.contains(&follower) {
                    tabs.push(follower);
                }
                self.columns.insert(index, Column::Tabs(tabs));
            }
        }
        Ok(())
    }

    /// Removes a window ID from the pane.
    /// If the window is part of a stack or tabs, it is removed from the vector.
    /// If the collection becomes empty or contains only one window, the panel type adjusts accordingly.
    ///
    /// # Arguments
    ///
    /// * `entity` - Entity of the window to remove.
    pub fn remove(&mut self, entity: Entity) {
        let removed = self
            .index_of(entity)
            .ok()
            .and_then(|index| self.columns.remove(index).zip(Some(index)));

        if let Some((column, index)) = removed {
            match column {
                Column::Single(_) | Column::Fullscren(_) => {
                    // Already removed from self.columns.
                }
                Column::Stack(mut stack) => {
                    for item in &mut stack {
                        match item {
                            StackItem::Single(_) => {}
                            StackItem::Tabs(tabs) => {
                                tabs.retain(|id| *id != entity);
                            }
                        }
                    }
                    stack.retain(|item| match item {
                        StackItem::Single(id) => *id != entity,
                        StackItem::Tabs(tabs) => !tabs.is_empty(),
                    });
                    if stack.len() > 1 {
                        self.columns.insert(index, Column::Stack(stack));
                    } else if let Some(remaining_item) = stack.first() {
                        match remaining_item {
                            StackItem::Single(id) => {
                                self.columns.insert(index, Column::Single(*id));
                            }
                            StackItem::Tabs(tabs) => {
                                self.columns.insert(index, Column::Tabs(tabs.clone()));
                            }
                        }
                    }
                }
                Column::Tabs(mut tabs) => {
                    tabs.retain(|id| *id != entity);
                    if tabs.len() > 1 {
                        self.columns.insert(index, Column::Tabs(tabs));
                    } else if let Some(remaining_id) = tabs.first() {
                        self.columns.insert(index, Column::Single(*remaining_id));
                    }
                }
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
        let stack_pos = self.columns.get(index)?.position_of(entity)?;
        (index < self.columns.len())
            .then_some(index + 1)
            .and_then(|i| self.columns.get(i))
            .and_then(|col| col.at_or_last(stack_pos))
    }

    pub fn left_neighbour(&self, entity: Entity) -> Option<Entity> {
        let index = self.index_of(entity).ok()?;
        let stack_pos = self.columns.get(index)?.position_of(entity)?;
        (index > 0)
            .then(|| index - 1)
            .and_then(|i| self.columns.get(i))
            .and_then(|col| col.at_or_last(stack_pos))
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

        let column_to_stack = self.columns.remove(index).unwrap();
        let items_to_stack = match column_to_stack {
            Column::Fullscren(_) => return Ok(()),
            Column::Single(id) => vec![StackItem::Single(id)],
            Column::Tabs(tabs) => vec![StackItem::Tabs(tabs)],
            Column::Stack(items) => items,
        };

        let target_column = self.columns.remove(index - 1).unwrap();
        let new_column = match target_column {
            Column::Fullscren(_) => return Ok(()),
            Column::Single(id) => {
                Column::Stack([vec![StackItem::Single(id)], items_to_stack].concat())
            }
            Column::Tabs(tabs) => {
                Column::Stack([vec![StackItem::Tabs(tabs)], items_to_stack].concat())
            }
            Column::Stack(items) => Column::Stack([items, items_to_stack].concat()),
        };

        self.columns.insert(index - 1, new_column);
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
        let column = self.columns.remove(index).unwrap();

        if let Column::Stack(mut items) = column {
            let item_index = items
                .iter()
                .position(|item| item.contains(entity))
                .ok_or(Error::NotFound(format!("Entity {entity} not in stack")))?;

            let removed_item = items.remove(item_index);

            // Re-insert the unstacked item as a single/tabs panel
            let unstacked_column = match removed_item {
                StackItem::Single(id) => Column::Single(id),
                StackItem::Tabs(tabs) => Column::Tabs(tabs),
            };
            self.columns.insert(index, unstacked_column);

            // Re-insert the modified stack (if not empty) at the original position
            if !items.is_empty() {
                let new_column = if items.len() == 1 {
                    match items.remove(0) {
                        StackItem::Single(id) => Column::Single(id),
                        StackItem::Tabs(tabs) => Column::Tabs(tabs),
                    }
                } else {
                    Column::Stack(items)
                };
                self.columns.insert(index, new_column);
            }
            Ok(())
        } else {
            // Not in a stack, put it back
            self.columns.insert(index, column);
            Ok(())
        }
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
                Column::Single(entity) | Column::Fullscren(entity) => vec![*entity],
                Column::Stack(items) => items.iter().flat_map(StackItem::all_windows).collect(),
                Column::Tabs(ids) => ids.clone(),
            })
            .collect()
    }

    pub fn get_column_mut(&mut self, index: usize) -> Option<&mut Column> {
        self.columns.get_mut(index)
    }

    pub fn all_columns(&self) -> Vec<Entity> {
        self.columns.iter().filter_map(Column::top).collect()
    }

    pub fn id(&self) -> WorkspaceId {
        self.id
    }

    pub fn columns(&self) -> impl Iterator<Item = &Column> {
        self.columns.iter()
    }

    #[instrument(level = Level::TRACE, skip_all, fields(offset))]
    pub fn relative_positions<W>(
        &self,
        layout_strip_height: i32,
        get_window_frame: &W,
    ) -> impl Iterator<Item = (Entity, IRect)>
    where
        W: Fn(Entity) -> Option<IRect>,
    {
        const MIN_WINDOW_HEIGHT: i32 = 200;

        self.column_positions(get_window_frame)
            .filter_map(move |(column, position)| {
                let items: Vec<StackItem> = match column {
                    Column::Single(entity) | Column::Fullscren(entity) => {
                        vec![StackItem::Single(*entity)]
                    }
                    Column::Stack(stack) => stack.clone(),
                    Column::Tabs(tabs) => vec![StackItem::Tabs(tabs.clone())],
                };

                let current_heights = items
                    .iter()
                    .filter_map(|item| item.top().and_then(get_window_frame))
                    .map(|frame| frame.height())
                    .collect::<Vec<_>>();

                let heights =
                    binpack_heights(&current_heights, MIN_WINDOW_HEIGHT, layout_strip_height)?;

                let column_width = items
                    .first()
                    .and_then(|item| item.top().and_then(get_window_frame))
                    .map(|frame| frame.width())?;

                let mut next_y = 0;
                let frames = items
                    .into_iter()
                    .zip(heights)
                    .filter_map(|(item, height)| {
                        let entity = item.top()?;
                        let mut frame = get_window_frame(entity)?;
                        frame.min.x = position;
                        frame.max.x = frame.min.x + column_width;

                        frame.min.y = next_y;
                        frame.max.y = frame.min.y + height;

                        next_y = frame.max.y;

                        // Return ALL windows in the item with the same frame
                        let results = item
                            .all_windows()
                            .into_iter()
                            .map(|e| (e, frame))
                            .collect::<Vec<_>>();
                        Some(results)
                    })
                    .flatten()
                    .collect::<Vec<_>>();

                Some(frames)
            })
            .flatten()
    }

    #[instrument(level = Level::TRACE, skip_all)]
    pub fn column_positions<W>(&self, get_window_frame: &W) -> impl Iterator<Item = (&Column, i32)>
    where
        W: Fn(Entity) -> Option<IRect>,
    {
        let mut left_edge = 0;

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
                left_edge += frame.width();
                (column, temp)
            })
    }

    pub fn above(&self, entity: Entity) -> Option<Entity> {
        let index = self.index_of(entity).ok()?;
        let column = self.get(index).ok()?;
        match column {
            Column::Single(_) | Column::Tabs(_) | Column::Fullscren(_) => None,
            Column::Stack(items) => {
                let pos = items.iter().position(|item| item.contains(entity))?;
                (pos > 0).then(|| items[pos - 1].top()).flatten()
            }
        }
    }

    pub fn tabbed(&self, entity: Entity) -> bool {
        self.index_of(entity)
            .and_then(|idx| self.get(idx))
            .map(|col| match col {
                Column::Tabs(tabs) => tabs.contains(&entity),
                Column::Stack(items) => items.iter().any(|item| {
                    if let StackItem::Tabs(tabs) = item {
                        tabs.contains(&entity)
                    } else {
                        false
                    }
                }),
                Column::Single(_) | Column::Fullscren(_) => false,
            })
            .is_ok_and(|t| t)
    }

    pub fn is_fullscreen(&self) -> bool {
        self.columns
            .front()
            .is_some_and(|column| matches!(column, Column::Fullscren(_)))
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

fn binpack_heights(heights: &[i32], min_height: i32, total_height: i32) -> Option<Vec<i32>> {
    let mut count = heights.len();
    let mut output = vec![];

    loop {
        let mut idx = 0;

        let mut remaining = total_height;
        while idx < count {
            let remaining_windows = heights.len() - idx;

            if heights[idx] < remaining {
                if idx + 1 == count {
                    output.push(remaining);
                } else {
                    output.push(heights[idx]);
                }
                remaining -= heights[idx];
            } else if remaining >= min_height * i32::try_from(remaining_windows).ok()? {
                output.push(remaining);
                remaining = 0;
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

    let remaining = i32::try_from(heights.len() - count).ok()?;
    if remaining > 0 && count > 0 {
        count -= 1;
        output.truncate(count);
        let sum = output.iter().sum::<i32>();
        let avg_height = (f64::from(total_height - sum) / f64::from(remaining + 1)) as i32;
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

/// Watches for size changes to windows and if they are changed, signals to the layout strip.
#[allow(clippy::needless_pass_by_value, clippy::type_complexity)]
#[instrument(level = Level::DEBUG, skip_all)]
pub(super) fn layout_sizes_changed(
    changed_sizes: Populated<
        Entity,
        Or<(
            (Changed<Bounds>, With<Window>),
            (Changed<Position>, With<Window>),
        )>,
    >,
    mut workspaces: Query<&mut LayoutStrip>,
) {
    workspaces.par_iter_mut().for_each(|mut strip| {
        if changed_sizes.iter().any(|entity| strip.contains(entity)) {
            strip.set_changed();
        }
    });
}

/// Watches for changes to `LayoutStrip` (i.e. a window added or window order changed) and
/// re-calculates the logical positions of all the windows in the layout strip.
#[allow(clippy::needless_pass_by_value, clippy::type_complexity)]
#[instrument(level = Level::DEBUG, skip_all)]
pub(super) fn layout_strip_changed(
    changed_strips: Populated<(&LayoutStrip, &ChildOf), Changed<LayoutStrip>>,
    mut windows: Query<
        (&Position, &mut Bounds, &mut LayoutPosition),
        (Without<LayoutStrip>, With<Window>),
    >,
    displays: Query<(&Display, Option<&DockPosition>)>,
    config: Res<Config>,
) {
    let get_window_frame = |entity| {
        windows
            .get(entity)
            .map(|(position, bounds, _)| IRect::from_corners(position.0, position.0 + bounds.0))
            .ok()
    };

    let changed = changed_strips
        .into_iter()
        .filter_map(|(layout_strip, child_of)| {
            displays
                .get(child_of.parent())
                .map(|(display, dock)| {
                    let height = display.actual_display_bounds(dock, &config).height();
                    layout_strip.relative_positions(height, &get_window_frame)
                })
                .ok()
        })
        .flatten()
        .collect::<Vec<_>>();

    for (entity, frame) in changed {
        if let Ok((_, mut bounds, mut layout_position)) = windows.get_mut(entity) {
            if layout_position.0 != frame.min {
                layout_position.0 = frame.min;
            }
            if bounds.0 != frame.size() {
                bounds.0 = frame.size();
            }
        }
    }
}

#[allow(clippy::needless_pass_by_value)]
#[instrument(level = Level::DEBUG, skip_all)]
pub(super) fn reshuffle_layout_strip(
    markers: Populated<(Entity, &LayoutPosition), With<ReshuffleAroundMarker>>,
    strips: Query<(&LayoutStrip, Entity, &Position, &ChildOf)>,
    displays: Query<(&Display, Option<&DockPosition>)>,
    windows: Windows,
    config: Res<Config>,
    commands: ParallelCommands,
) {
    markers.par_iter().for_each(|(entity, layout_position)| {
        commands.command_scope(|mut command| {
            if let Ok(mut cmd) = command.get_entity(entity) {
                cmd.try_remove::<ReshuffleAroundMarker>();
            }
        });
        let Some((_, strip_entity, active_strip, child)) =
            strips.into_iter().find(|strip| strip.0.contains(entity))
        else {
            return;
        };
        let Ok((active_display, dock)) = displays.get(child.parent()) else {
            return;
        };
        let display_bounds = active_display.actual_display_bounds(dock, &config);
        let Some(mut frame) = windows.moving_frame(entity) else {
            return;
        };

        let size = frame.size();
        let visible_width = display_bounds.intersect(frame).width();

        // Expose the window by clamping it into the viewport.
        frame.min = frame
            .min
            .clamp(display_bounds.min, display_bounds.max - size);
        frame.max = frame.min + size;

        let strip_position = (frame.min - layout_position.0).with_y(display_bounds.min.y);

        // Check how much of the window is hidden. Slivers don't count as
        // meaningfully visible, so subtract sliver_width from the visible
        // portion. If the hidden fraction is within the allowed ratio, skip.
        let hidden_ratio = config.window_hidden_ratio();
        if hidden_ratio > 0.0 {
            let meaningful = (visible_width - config.sliver_width()).max(0);
            let visible_fraction = f64::from(meaningful) / f64::from(frame.width().max(1));
            let hidden_fraction = 1.0 - visible_fraction;

            // Do not move the window if the hidden fraction is lower than threshold
            // or if the layout strip movement is shorter than the hidden width.
            let strip_movement = (active_strip.x - strip_position.x).abs();
            if hidden_fraction <= hidden_ratio && frame.width() - visible_width >= strip_movement {
                return;
            }
        }

        trace!("reshuffle_layout_strip: triggered for entity {entity}, offset {strip_position}");
        commands.command_scope(|mut command| {
            if let Ok(mut cmd) = command.get_entity(strip_entity) {
                cmd.try_insert(RepositionMarker(strip_position));
            }
        });
    });
}

/// Reacts to changes in the position of the `LayoutStrip` to Display, and if changed,
/// marks all the windows in the strip as requiring re-positioning.
#[allow(clippy::needless_pass_by_value)]
#[instrument(level = Level::DEBUG, skip_all)]
pub(super) fn position_layout_strips(
    moved_strips: Populated<&LayoutStrip, Changed<Position>>,
    mut windows: Query<&mut LayoutPosition, (With<Window>, Without<LayoutStrip>)>,
) {
    for strip in moved_strips {
        for entity in strip.all_windows() {
            if let Ok(mut position) = windows.get_mut(entity) {
                position.set_changed();
            }
        }
    }
}

/// Reacts to changes of logical window layout in the strip and any have been changed, reposition
/// the layout strip against the current display viewport.
#[allow(clippy::needless_pass_by_value, clippy::type_complexity)]
#[instrument(level = Level::DEBUG, skip_all)]
pub(super) fn position_layout_windows(
    mut positioned_windows: Populated<
        (Entity, &Window, &LayoutPosition, &mut Position, &mut Bounds),
        (Changed<LayoutPosition>, With<Window>, Without<LayoutStrip>),
    >,
    workspaces: Query<(&LayoutStrip, &Position, Has<Scrolling>, &ChildOf), With<LayoutStrip>>,
    displays: Query<(&Display, Option<&DockPosition>)>,
    config: Res<Config>,
) {
    let offscreen_sliver_width = config.sliver_width();
    let (_, pad_right, _, pad_left) = config.edge_padding();

    positioned_windows.par_iter_mut().for_each(
        |(entity, window, layout_position, mut position, mut bounds)| {
            let Some((layout_strip, Position(strip_position), swiping, child_of)) =
                workspaces.iter().find(|strip| strip.0.contains(entity))
            else {
                return;
            };
            let Ok((display, dock)) = displays.get(child_of.parent()) else {
                return;
            };
            let viewport = display.actual_display_bounds(dock, &config);

            // Account for per-window horizontal_padding: reposition() adds
            // h_pad to the virtual x, so subtract it here so the OS window
            // lands exactly sliver_width pixels from the screen edge.
            let h_pad = window.horizontal_padding();
            let mut frame = IRect::from_corners(layout_position.0, layout_position.0 + bounds.0);
            let width = frame.width();
            frame.min += strip_position;
            frame.max += strip_position;

            if frame.max.x <= viewport.min.x + h_pad {
                // Window hidden to the left — position so exactly
                // sliver_width CG pixels are visible from the real
                // display edge.  The +h_pad accounts for the gap that
                // reposition() adds, which can leave a window just
                // inside the viewport edge while its CG frame is fully
                // past it.
                frame.min.x = viewport.min.x - width + offscreen_sliver_width - pad_left + h_pad;
            } else if frame.min.x >= viewport.max.x - h_pad {
                // Window hidden to the right — mirror of above.
                frame.min.x = viewport.max.x - offscreen_sliver_width + pad_right - h_pad;
            }
            frame.max.x = frame.min.x + width;

            // During swipe, keep full height.
            if !swiping {
                let stacked = layout_strip
                    .index_of(entity)
                    .ok()
                    .and_then(|idx| layout_strip.get(idx).ok())
                    .is_some_and(|col| matches!(col, Column::Stack(_)));

                // Don't compress stacked windows vertically when off-screen.
                // The height reduction corrupts their proportions: when the
                // column scrolls back on-screen, binpack_heights makes the
                // last window absorb all remaining space.
                if !stacked {
                    let inset = (f64::from(viewport.height()) * (1.0 - config.sliver_height())
                        / 2.0) as i32;
                    frame.min.y += inset;
                    frame.max.y += inset;
                }
            }

            if bounds.0 != frame.size() {
                bounds.0 = frame.size();
            }

            if position.0 != frame.min {
                position.0 = frame.min;
            }
        },
    );
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
                assert_eq!(stack[0], StackItem::Single(entities[0]));
                assert_eq!(stack[1], StackItem::Single(entities[1]));
            }
            Column::Single(_) | Column::Fullscren(_) | Column::Tabs(_) => {
                panic!("Expected a stack")
            }
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
        const MIN_HEIGHT: i32 = 100;
        let heights = [300, 300, 300, 300];

        let out = binpack_heights(&heights, MIN_HEIGHT, 1500).unwrap();
        assert_eq!(out, vec![300, 300, 300, 600]);

        let out = binpack_heights(&heights, MIN_HEIGHT, 1024).unwrap();
        assert_eq!(out, vec![300, 300, 300, 124]);

        let out = binpack_heights(&heights, MIN_HEIGHT, 800).unwrap();
        assert_eq!(out, vec![300, 300, 100, 100]);

        let out = binpack_heights(&heights, MIN_HEIGHT, 440).unwrap();
        assert_eq!(out, vec![110, 110, 110, 110]);

        let out = binpack_heights(&heights, MIN_HEIGHT, 390);
        assert_eq!(out, None);
    }

    #[test]
    fn test_layout_positioning() {
        let mut world = World::new();
        let entities = world
            .spawn_batch(vec![(), (), (), ()])
            .collect::<Vec<Entity>>();
        let sizes = [
            IRect::new(0, 0, 300, 300),
            IRect::new(0, 0, 300, 300),
            IRect::new(0, 0, 300, 300),
            IRect::new(0, 0, 300, 300),
        ];

        let mut strip = LayoutStrip::default();
        strip.append(entities[0]);
        strip.append(entities[1]);
        strip.append(entities[2]);
        strip.append(entities[3]);

        _ = strip.stack(entities[2]);
        let get_window_frame = |_| Some(sizes[0]);
        let out = strip
            .relative_positions(500, &get_window_frame)
            .collect::<Vec<_>>();

        let xpos = out.iter().map(|(_, frame)| frame.min.x).collect::<Vec<_>>();
        assert_eq!(xpos, vec![0, 300, 300, 600]);

        let height = out
            .iter()
            .map(|(_, frame)| frame.height())
            .collect::<Vec<_>>();
        assert_eq!(height, vec![500, 300, 200, 500]);
    }

    /// Every single-column window must fill the full viewport height.
    #[test]
    fn test_layout_singles_get_full_viewport_height() {
        let mut world = World::new();
        let entities = world.spawn_batch(vec![(), (), ()]).collect::<Vec<Entity>>();

        let mut strip = LayoutStrip::default();
        for &e in &entities {
            strip.append(e);
        }

        let get_window_frame = |_| Some(IRect::new(0, 0, 300, 400));
        let out: Vec<_> = strip.relative_positions(800, &get_window_frame).collect();

        assert_eq!(out.len(), 3);
        for (_, f) in &out {
            assert_eq!(f.height(), 800, "single window should fill viewport height");
            assert_eq!(f.min.y, 0);
        }
        // x positions: 0, 300, 600
        let xs: Vec<_> = out.iter().map(|(_, f)| f.min.x).collect();
        assert_eq!(xs, vec![0, 300, 600]);
    }

    /// Stacked windows share the viewport height; all use the top window's width.
    #[test]
    fn test_layout_stack_shares_height_and_width() {
        let mut world = World::new();
        let entities = world
            .spawn_batch(vec![(), (), (), ()])
            .collect::<Vec<Entity>>();

        let mut strip = LayoutStrip::default();
        for &e in &entities {
            strip.append(e);
        }
        // Stack e1, e2 onto e0: [Stack(e0, e1, e2), Single(e3)]
        strip.stack(entities[1]).unwrap();
        strip.stack(entities[2]).unwrap();

        // Give different heights; top window (e0) is 400px wide, others 300px.
        let get_window_frame = |e: Entity| {
            if e == entities[0] {
                Some(IRect::new(0, 0, 400, 200))
            } else if e == entities[1] || e == entities[2] {
                Some(IRect::new(0, 0, 300, 200))
            } else {
                Some(IRect::new(0, 0, 400, 500))
            }
        };

        let out: Vec<_> = strip.relative_positions(600, &get_window_frame).collect();
        assert_eq!(out.len(), 4);

        // All stacked windows use the top window's width (400).
        for &(e, ref f) in &out {
            if e == entities[0] || e == entities[1] || e == entities[2] {
                assert_eq!(
                    f.width(),
                    400,
                    "stacked window should use top window's width"
                );
            }
        }

        // Stacked heights should sum to viewport height.
        let stack_heights: i32 = out
            .iter()
            .filter(|(e, _)| *e != entities[3])
            .map(|(_, f)| f.height())
            .sum();
        assert_eq!(stack_heights, 600, "stack heights must sum to viewport");

        // Stacked y positions should be contiguous from 0.
        let stack_frames: Vec<_> = out
            .iter()
            .filter(|(e, _)| *e != entities[3])
            .map(|(_, f)| *f)
            .collect();
        assert_eq!(stack_frames[0].min.y, 0);
        assert_eq!(stack_frames[0].max.y, stack_frames[1].min.y);
        assert_eq!(stack_frames[1].max.y, stack_frames[2].min.y);
        assert_eq!(stack_frames[2].max.y, 600);

        // e3 (single) gets full viewport height.
        let e3_frame = out.iter().find(|(e, _)| *e == entities[3]).unwrap().1;
        assert_eq!(e3_frame.height(), 600);
    }

    #[test]
    fn test_tabs_in_stack() {
        let mut world = World::new();
        let e1 = world.spawn_empty().id();
        let e2 = world.spawn_empty().id();
        let e3 = world.spawn_empty().id();
        let e4 = world.spawn_empty().id();

        let mut strip = LayoutStrip::default();
        strip.append(e1);
        strip.append(e2);
        strip.append(e3);

        // [Single(e1), Single(e2), Single(e3)]
        strip.stack(e2).unwrap();
        // [Stack([Single(e1), Single(e2)]), Single(e3)]

        // Convert e1 (in stack) to tabs with e4
        strip.convert_to_tabs(e1, e4).unwrap();
        // [Stack([Tabs([e1, e4]), Single(e2)]), Single(e3)]

        assert_eq!(strip.len(), 2);
        match strip.get(0).unwrap() {
            Column::Stack(items) => {
                assert_eq!(items.len(), 2);
                match &items[0] {
                    StackItem::Tabs(tabs) => assert_eq!(tabs, &vec![e1, e4]),
                    StackItem::Single(_) => panic!("Expected Tabs in stack"),
                }
            }
            _ => panic!("Expected Stack"),
        }

        // relative_positions should yield e1, e4 (same frame) and e2
        let get_window_frame = |_| Some(IRect::new(0, 0, 100, 100));
        let out: Vec<_> = strip.relative_positions(400, &get_window_frame).collect();

        // We expect e1, e4, e2 from the first column, and e3 from the second.
        assert_eq!(out.len(), 4);

        let e1_frame = out.iter().find(|(e, _)| *e == e1).unwrap().1;
        let e4_frame = out.iter().find(|(e, _)| *e == e4).unwrap().1;
        let e2_frame = out.iter().find(|(e, _)| *e == e2).unwrap().1;

        assert_eq!(e1_frame, e4_frame);
        assert_eq!(e1_frame.max.y, e2_frame.min.y);
    }

    /// Unstacking a window from a stack gives it its own column with full height.
    #[test]
    fn test_layout_unstack_gives_full_height() {
        let mut world = World::new();
        let entities = world.spawn_batch(vec![(), (), ()]).collect::<Vec<Entity>>();

        let mut strip = LayoutStrip::default();
        for &e in &entities {
            strip.append(e);
        }
        // [Stack(e0, e1), Single(e2)]
        strip.stack(entities[1]).unwrap();

        let get_window_frame = |_| Some(IRect::new(0, 0, 300, 250));

        // Before unstack: e0 and e1 share 500px height.
        let out: Vec<_> = strip.relative_positions(500, &get_window_frame).collect();
        let e1_height = out
            .iter()
            .find(|(e, _)| *e == entities[1])
            .unwrap()
            .1
            .height();
        assert!(e1_height < 500, "stacked e1 should not have full height");

        // Unstack e1: [Single(e0), Single(e1), Single(e2)]
        strip.unstack(entities[1]).unwrap();
        assert_eq!(strip.len(), 3);

        let out: Vec<_> = strip.relative_positions(500, &get_window_frame).collect();
        for (_, f) in &out {
            assert_eq!(
                f.height(),
                500,
                "after unstack every single column gets full viewport height"
            );
        }
    }

    /// Re-stacking after unstack restores shared height distribution.
    #[test]
    fn test_layout_restack_restores_shared_heights() {
        let mut world = World::new();
        let entities = world.spawn_batch(vec![(), ()]).collect::<Vec<Entity>>();

        let mut strip = LayoutStrip::default();
        strip.append(entities[0]);
        strip.append(entities[1]);

        let get_window_frame = |_| Some(IRect::new(0, 0, 300, 250));

        // Stack: [Stack(e0, e1)]
        strip.stack(entities[1]).unwrap();
        let out: Vec<_> = strip.relative_positions(500, &get_window_frame).collect();
        let heights: Vec<_> = out.iter().map(|(_, f)| f.height()).collect();
        assert_eq!(heights.iter().sum::<i32>(), 500);
        assert_eq!(heights.len(), 2);

        // Unstack: [Single(e0), Single(e1)]
        strip.unstack(entities[1]).unwrap();
        let out: Vec<_> = strip.relative_positions(500, &get_window_frame).collect();
        for (_, f) in &out {
            assert_eq!(f.height(), 500);
        }

        // Re-stack: [Stack(e0, e1)] — e1 stacks onto left neighbor e0
        strip.stack(entities[1]).unwrap();
        let out: Vec<_> = strip.relative_positions(500, &get_window_frame).collect();
        let heights: Vec<_> = out.iter().map(|(_, f)| f.height()).collect();
        assert_eq!(heights.iter().sum::<i32>(), 500);
        assert_eq!(heights.len(), 2);
    }

    /// When window frames include padding (logical frame is wider than the visual
    /// window), columns must be placed edge-to-edge using the full logical width.
    /// This ensures the visual gap between windows equals the sum of their padding.
    #[test]
    fn test_column_positions_with_padded_frames() {
        let mut world = World::new();
        let entities = world.spawn_batch(vec![(), (), ()]).collect::<Vec<Entity>>();

        let mut strip = LayoutStrip::default();
        for &e in &entities {
            strip.append(e);
        }

        // Simulate windows with padding=8: logical width = OS_width + 2*8.
        // Window 0: OS width 284, logical width 300
        // Window 1: OS width 384, logical width 400
        // Window 2: OS width 484, logical width 500
        let padded_frames = [
            IRect::new(0, 0, 300, 600), // logical frame with padding included
            IRect::new(0, 0, 400, 600),
            IRect::new(0, 0, 500, 600),
        ];

        let get_window_frame = |e: Entity| {
            if e == entities[0] {
                Some(padded_frames[0])
            } else if e == entities[1] {
                Some(padded_frames[1])
            } else {
                Some(padded_frames[2])
            }
        };

        let out: Vec<_> = strip.relative_positions(600, &get_window_frame).collect();
        assert_eq!(out.len(), 3);

        // Columns must be edge-to-edge: each column starts where the previous ends.
        let xs: Vec<_> = out.iter().map(|(_, f)| f.min.x).collect();
        assert_eq!(
            xs,
            vec![0, 300, 700],
            "columns must be edge-to-edge using logical widths"
        );

        // Right edge of each window must equal left edge of the next.
        for i in 0..out.len() - 1 {
            assert_eq!(
                out[i].1.max.x,
                out[i + 1].1.min.x,
                "window {} right edge must equal window {} left edge",
                i,
                i + 1
            );
        }
    }

    /// Frames with no padding (padding=0) should still produce edge-to-edge layout.
    #[test]
    fn test_column_positions_no_padding() {
        let mut world = World::new();
        let entities = world.spawn_batch(vec![(), (), ()]).collect::<Vec<Entity>>();

        let mut strip = LayoutStrip::default();
        for &e in &entities {
            strip.append(e);
        }

        let get_window_frame = |_| Some(IRect::new(0, 0, 300, 600));

        let out: Vec<_> = strip.relative_positions(600, &get_window_frame).collect();
        let xs: Vec<_> = out.iter().map(|(_, f)| f.min.x).collect();
        assert_eq!(xs, vec![0, 300, 600]);

        // No gaps or overlaps.
        for i in 0..out.len() - 1 {
            assert_eq!(out[i].1.max.x, out[i + 1].1.min.x);
        }
    }

    #[test]
    fn test_convert_to_tabs() {
        let mut world = World::new();
        let e1 = world.spawn_empty().id();
        let e2 = world.spawn_empty().id();
        let e3 = world.spawn_empty().id();

        let mut strip = LayoutStrip::default();
        strip.append(e1);
        strip.append(e3);

        // Convert e1 to a tab group with follower e2
        strip.convert_to_tabs(e1, e2).unwrap();

        assert_eq!(strip.len(), 2);
        match strip.get(0).unwrap() {
            Column::Tabs(tabs) => {
                assert_eq!(tabs, vec![e1, e2]);
            }
            _ => panic!("Expected Tabs column"),
        }

        // Add another tab e4
        let e4 = world.spawn_empty().id();
        strip.convert_to_tabs(e1, e4).unwrap();
        assert_eq!(strip.len(), 2);
        match strip.get(0).unwrap() {
            Column::Tabs(tabs) => {
                assert_eq!(tabs, vec![e1, e2, e4]);
            }
            _ => panic!("Expected Tabs column"),
        }
    }

    #[test]
    fn test_tab_leader_rotation() {
        let mut world = World::new();
        let e1 = world.spawn_empty().id();
        let e2 = world.spawn_empty().id();
        let e3 = world.spawn_empty().id();

        let mut column = Column::Tabs(vec![e1, e2, e3]);
        assert_eq!(column.top(), Some(e1));

        // Move e2 to front (new leader)
        column.move_to_front(e2);
        assert_eq!(column.top(), Some(e2));
        match column {
            Column::Tabs(ref tabs) => assert_eq!(tabs, &vec![e2, e1, e3]),
            _ => panic!(),
        }

        // Move e3 to front
        column.move_to_front(e3);
        assert_eq!(column.top(), Some(e3));
        match column {
            Column::Tabs(ref tabs) => assert_eq!(tabs, &vec![e3, e1, e2]),
            _ => panic!(),
        }
    }

    #[test]
    fn test_remove_from_tabs() {
        let mut world = World::new();
        let e1 = world.spawn_empty().id();
        let e2 = world.spawn_empty().id();
        let e3 = world.spawn_empty().id();

        let mut strip = LayoutStrip::default();
        strip.append(e1);
        strip.convert_to_tabs(e1, e2).unwrap();
        strip.convert_to_tabs(e1, e3).unwrap();

        assert_eq!(strip.len(), 1);

        // Remove e2 (follower)
        strip.remove(e2);
        assert_eq!(strip.len(), 1);
        match strip.get(0).unwrap() {
            Column::Tabs(tabs) => assert_eq!(tabs, vec![e1, e3]),
            _ => panic!(),
        }

        // Remove e1 (leader)
        strip.remove(e1);
        assert_eq!(strip.len(), 1);
        // Should convert back to Single since only e3 remains
        match strip.get(0).unwrap() {
            Column::Single(id) => assert_eq!(id, e3),
            _ => panic!("Expected Single column after removing all but one tab"),
        }
    }

    #[test]
    fn test_overlapping_frame_strategy_simulation() {
        let mut world = World::new();
        let e1 = world.spawn_empty().id();
        let e2 = world.spawn_empty().id();

        let mut strip = LayoutStrip::default();
        strip.append(e1);

        // Simulate detection logic from spawn_window_trigger
        let leader_match = Some(e1); // Mocked match from frame comparison

        if let Some(leader) = leader_match {
            strip.convert_to_tabs(leader, e2).unwrap();
        } else {
            strip.append(e2);
        }

        assert_eq!(strip.len(), 1);
        match strip.get(0).unwrap() {
            Column::Tabs(tabs) => assert_eq!(tabs, vec![e1, e2]),
            _ => panic!(),
        }
    }
}

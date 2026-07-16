use bevy::app::{App, Plugin, Update};
use bevy::ecs::change_detection::{DetectChanges, DetectChangesMut, Ref};
use bevy::ecs::component::Component;
use bevy::ecs::entity::{Entity, EntityHashMap, EntityHashSet};
use bevy::ecs::hierarchy::ChildOf;
use bevy::ecs::query::{Changed, Has, Or, With, Without};
use bevy::ecs::schedule::IntoScheduleConfigs as _;
use bevy::ecs::schedule::common_conditions::{not, resource_exists};
use bevy::ecs::system::{Commands, ParamSet, Populated, Query, Res};
use bevy::math::IRect;
use std::collections::{HashMap, VecDeque};
use stdext::function_name;
use tracing::{Level, instrument, trace};

use crate::config::Config;
use crate::ecs::params::Windows;
use crate::ecs::{
    ActiveWorkspaceMarker, Bounds, DockPosition, EnsureVisibleMarker, Initializing, LayoutPosition,
    Position, RepositionMarker, ReshuffleAroundMarker, Scrolling, SpawnCommandsExt,
};
use crate::errors::{Error, Result};
use crate::manager::{Display, Origin, Size, Window};
use crate::platform::WorkspaceId;

pub struct LayoutEventsPlugin;

/// Clamp a window origin to the range where it still touches both viewport
/// edges. For an oversized window this range is reversed: from right-aligned
/// to left-aligned, which lets the strip pan across the hidden content.
pub(crate) fn clamp_origin_to_viewport(origin: Origin, size: Size, viewport: IRect) -> Origin {
    let far_edge = viewport.max - size;
    let minimum = viewport.min.min(far_edge);
    let maximum = viewport.min.max(far_edge);
    origin.clamp(minimum, maximum)
}

impl Plugin for LayoutEventsPlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(
            Update,
            (
                // Wait for finish_setup before tiling: until then every window
                // sits in the active strip regardless of its real display.
                (
                    sync_tab_group_frames,
                    layout_sizes_changed,
                    layout_strip_changed,
                    reshuffle_layout_strip,
                    ensure_visible_in_strip,
                    position_layout_strips,
                    position_layout_windows,
                )
                    .chain()
                    .after(super::systems::finish_setup)
                    .before(super::workspace::show_active_workspace)
                    .run_if(not(resource_exists::<Initializing>)),
            ),
        );
    }
}

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

    /// Returns an iterator over all window entities in this stack item.
    pub fn window_iter(&self) -> StackItemIter<'_> {
        match self {
            StackItem::Single(entity) => StackItemIter::Single(std::iter::once(*entity)),
            StackItem::Tabs(tabs) => StackItemIter::Tabs(tabs.iter().copied()),
        }
    }
}

pub enum StackItemIter<'a> {
    Single(std::iter::Once<Entity>),
    Tabs(std::iter::Copied<std::slice::Iter<'a, Entity>>),
}

impl Iterator for StackItemIter<'_> {
    type Item = Entity;

    fn next(&mut self) -> Option<Self::Item> {
        match self {
            StackItemIter::Single(iter) => iter.next(),
            StackItemIter::Tabs(iter) => iter.next(),
        }
    }
}

impl DoubleEndedIterator for StackItemIter<'_> {
    fn next_back(&mut self) -> Option<Self::Item> {
        match self {
            StackItemIter::Single(iter) => iter.next_back(),
            StackItemIter::Tabs(iter) => iter.next_back(),
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
    /// A panel containing a group of native tabs.
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

    /// Returns an iterator over all window entities in this column
    pub fn window_iter(&self) -> ColumnWindowIter<'_> {
        match self {
            Column::Single(entity) | Column::Fullscren(entity) => {
                ColumnWindowIter::Single(std::iter::once(*entity))
            }
            Column::Tabs(tabs) => ColumnWindowIter::Tabs(tabs.iter().copied()),
            Column::Stack(items) => {
                ColumnWindowIter::Stack(items.iter().flat_map(StackItem::window_iter))
            }
        }
    }

    pub fn width<W>(&self, get_window_frame: &W) -> Option<i32>
    where
        W: Fn(Entity) -> Option<IRect>,
    {
        self.window_iter()
            .filter_map(get_window_frame)
            .map(|frame| frame.width())
            .max()
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

    /// Moves the specified entity to the front of stack-local ordering.
    /// Native tab ordering is stable; the focused tab is tracked by `FocusedMarker`.
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

pub enum ColumnWindowIter<'a> {
    Single(std::iter::Once<Entity>),
    Tabs(std::iter::Copied<std::slice::Iter<'a, Entity>>),
    Stack(
        std::iter::FlatMap<
            std::slice::Iter<'a, StackItem>,
            StackItemIter<'a>,
            fn(&'a StackItem) -> StackItemIter<'a>,
        >,
    ),
}

impl Iterator for ColumnWindowIter<'_> {
    type Item = Entity;

    fn next(&mut self) -> Option<Self::Item> {
        match self {
            Self::Single(iter) => iter.next(),
            Self::Tabs(iter) => iter.next(),
            Self::Stack(iter) => iter.next(),
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

    pub fn append_tab_group(&mut self, entities: &[Entity]) {
        let group = dedup_entities(entities);
        if group.is_empty() {
            return;
        }

        // Re-grouping existing members keeps them at their lowest current index;
        // a foreign group lands at the end.
        let index = group
            .iter()
            .filter_map(|entity| self.index_of(*entity).ok())
            .min()
            .unwrap_or(self.len());

        self.insert_tab_group_at(index, &group);
    }

    /// Inserts `entities` as a single column at `index` (clamped to the column
    /// count), after removing any existing occurrences. A one-element group
    /// becomes a `Single` column, more than one a `Tabs` column.
    pub fn insert_tab_group_at(&mut self, index: usize, entities: &[Entity]) {
        let group = dedup_entities(entities);
        if group.is_empty() {
            return;
        }

        for entity in &group {
            self.remove(*entity);
        }

        let index = index.min(self.len());
        if group.len() == 1 {
            self.insert_at(index, group[0]);
        } else if index >= self.len() {
            self.columns.push_back(Column::Tabs(group));
        } else {
            self.columns.insert(index, Column::Tabs(group));
        }
    }

    /// Converts a column containing `leader` to a `Tabs` column and adds `follower`.
    pub fn convert_to_tabs(&mut self, leader: Entity, follower: Entity) -> Result<()> {
        self.remove(follower);
        let index = self.index_of(leader)?;
        let column = self.columns.remove(index).unwrap();
        match column {
            Column::Single(id) | Column::Fullscren(id) => {
                self.columns.insert(index, Column::Tabs(vec![follower, id]));
            }
            Column::Stack(mut items) => {
                if let Some(pos) = items.iter().position(|item| item.contains(leader)) {
                    match &mut items[pos] {
                        StackItem::Single(id) => {
                            let id = *id;
                            items[pos] = StackItem::Tabs(vec![follower, id]);
                        }
                        StackItem::Tabs(tabs) => {
                            if !tabs.contains(&follower) {
                                tabs.insert(0, follower);
                            }
                        }
                    }
                }
                self.columns.insert(index, Column::Stack(items));
            }
            Column::Tabs(mut tabs) => {
                if !tabs.contains(&follower) {
                    tabs.insert(0, follower);
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
                Column::Stack(items) => items.iter().flat_map(StackItem::window_iter).collect(),
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

    #[instrument(level = Level::TRACE, skip_all, fields(layout_strip_height))]
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

                // Every window in a column shares the master's (top item's)
                // width, so a window stacked onto a master of a different width
                // resizes to match it instead of keeping its own width. This
                // also matches the column slot width from column_positions,
                // which is the widest member.
                let column_width = items
                    .first()
                    .and_then(StackItem::top)
                    .and_then(&get_window_frame)
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
                        let results = item.window_iter().map(|e| (e, frame)).collect::<Vec<_>>();
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

        self.columns().filter_map(move |column| {
            let width = column.width(get_window_frame);

            width.map(|width| {
                let temp = left_edge;
                left_edge += width;
                (column, temp)
            })
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

    pub fn tab_group(&self, entity: Entity) -> Option<Vec<Entity>> {
        self.columns.iter().find_map(|column| match column {
            Column::Tabs(tabs) if tabs.contains(&entity) && tabs.len() > 1 => Some(tabs.clone()),
            Column::Stack(items) => items.iter().find_map(|item| match item {
                StackItem::Tabs(tabs) if tabs.contains(&entity) && tabs.len() > 1 => {
                    Some(tabs.clone())
                }
                StackItem::Single(_) | StackItem::Tabs(_) => None,
            }),
            Column::Single(_) | Column::Fullscren(_) | Column::Tabs(_) => None,
        })
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

/// Deduplicates `entities`, preserving first-seen order.
fn dedup_entities(entities: &[Entity]) -> Vec<Entity> {
    let mut seen = EntityHashSet::default();
    entities
        .iter()
        .copied()
        .filter(|entity| seen.insert(*entity))
        .collect()
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

#[allow(clippy::needless_pass_by_value, clippy::type_complexity)]
#[instrument(level = Level::DEBUG, skip_all)]
fn sync_tab_group_frames(
    mut windows: ParamSet<(
        Query<
            (Entity, &Position, &Bounds),
            (With<Window>, Or<(Changed<Bounds>, Changed<Position>)>),
        >,
        Query<(&mut Position, &mut Bounds), With<Window>>,
    )>,
    workspaces: Query<&LayoutStrip>,
) {
    let updates = windows
        .p0()
        .into_iter()
        .filter_map(|(entity, Position(position), Bounds(bounds))| {
            workspaces
                .iter()
                .find_map(|strip| strip.tab_group(entity))
                .map(move |tab_group| {
                    tab_group.into_iter().filter_map(move |sibling| {
                        (sibling != entity).then_some((sibling, (*position, *bounds)))
                    })
                })
        })
        .flatten()
        .collect::<HashMap<_, _>>();

    for (entity, (source_position, source_bounds)) in updates {
        let mut write_windows = windows.p1();
        if let Ok((mut position, mut bounds)) = write_windows.get_mut(entity) {
            if position.0 != source_position {
                position.0 = source_position;
            }
            if bounds.0 != source_bounds {
                bounds.0 = source_bounds;
            }
        }
    }
}

/// Watches for size changes to windows and if they are changed, signals to the layout strip.
#[allow(clippy::needless_pass_by_value, clippy::type_complexity)]
#[instrument(level = Level::DEBUG, skip_all)]
fn layout_sizes_changed(
    changed_sizes: Populated<
        Entity,
        Or<(
            (Changed<Bounds>, With<Window>),
            (Changed<Position>, With<Window>),
        )>,
    >,
    workspaces: Query<&mut LayoutStrip>,
) {
    let changed_entities = changed_sizes.iter().collect::<EntityHashSet>();
    workspaces.into_iter().for_each(|mut strip| {
        if strip_has_changed_window(&strip, &changed_entities) {
            strip.set_changed();
        }
    });
}

fn strip_has_changed_window(strip: &LayoutStrip, changed_entities: &EntityHashSet) -> bool {
    strip
        .columns
        .iter()
        .any(|column| column_has_changed_window(column, changed_entities))
}

fn column_has_changed_window(column: &Column, changed_entities: &EntityHashSet) -> bool {
    match column {
        Column::Single(entity) | Column::Fullscren(entity) => changed_entities.contains(entity),
        Column::Stack(stack) => stack
            .iter()
            .any(|item| stack_item_has_changed_window(item, changed_entities)),
        Column::Tabs(entities) => entities
            .iter()
            .any(|entity| changed_entities.contains(entity)),
    }
}

fn stack_item_has_changed_window(item: &StackItem, changed_entities: &EntityHashSet) -> bool {
    match item {
        StackItem::Single(entity) => changed_entities.contains(entity),
        StackItem::Tabs(entities) => entities
            .iter()
            .any(|entity| changed_entities.contains(entity)),
    }
}

/// Watches for changes to `LayoutStrip` (i.e. a window added or window order changed) and
/// re-calculates the logical positions of all the windows in the layout strip.
#[allow(clippy::needless_pass_by_value, clippy::type_complexity)]
#[instrument(level = Level::DEBUG, skip_all)]
fn layout_strip_changed(
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

#[allow(clippy::needless_pass_by_value, clippy::type_complexity)]
#[instrument(level = Level::DEBUG, skip_all)]
fn reshuffle_layout_strip(
    markers: Query<(Entity, &LayoutPosition), With<ReshuffleAroundMarker>>,
    strips: Query<(
        &LayoutStrip,
        Entity,
        &Position,
        &ChildOf,
        Option<Ref<ActiveWorkspaceMarker>>,
    )>,
    displays: Query<(&Display, Option<&DockPosition>)>,
    windows: Windows,
    config: Res<Config>,
    mut commands: Commands,
) {
    markers.into_iter().for_each(|(entity, layout_position)| {
        if let Ok(mut cmd) = commands.get_entity(entity) {
            cmd.try_remove::<ReshuffleAroundMarker>();
        }
        let Some((strip, strip_entity, active_strip, child, active_marker)) =
            strips.into_iter().find(|strip| strip.0.contains(entity))
        else {
            return;
        };

        if active_marker.is_some_and(|m| m.is_added()) {
            trace!("reshuffle_layout_strip: skipping newly active workspace {strip_entity}");
            return;
        }
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
        frame.min = clamp_origin_to_viewport(frame.min, size, display_bounds);
        frame.max = frame.min + size;

        let mut strip_position = (frame.min - layout_position.0).with_y(display_bounds.min.y);

        // Enforce the edge invariant when auto-center is off: the leftmost
        // window must touch the left edge and the rightmost the right edge
        // if more than 1 windows in workspace.
        if !config.auto_center()
            && let Some(total_strip_width) = strip
                .last()
                .ok()
                .and_then(|column| column.top())
                .and_then(|last| {
                    windows
                        .layout_position(last)
                        .map(|position| position.0.x)
                        .zip(windows.moving_frame(last).map(|frame| frame.width()))
                })
                .map(|(last_x, last_width)| last_x + last_width)
        {
            strip_position.x = if display_bounds.width() < total_strip_width {
                strip_position.x.clamp(
                    display_bounds.max.x - total_strip_width,
                    display_bounds.min.x,
                )
            } else {
                // Strip fits entirely: pin the leftmost window to the left edge.
                display_bounds.min.x
            };
        }

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
        commands.reposition_entity(strip_entity, strip_position);
    });
}

/// Scrolls the strip the minimum amount needed to keep `EnsureVisibleMarker`
/// entities on-screen at their new layout position. If the entity already fits
/// inside the viewport with the strip where it is, the strip is left alone and
/// the per-window animator slides the entity into its slot. Only when the new
/// slot would fall past an edge does the strip translate, and only by the
/// shortfall — never to anchor the entity to a particular position.
#[allow(clippy::needless_pass_by_value, clippy::type_complexity)]
#[instrument(level = Level::DEBUG, skip_all)]
fn ensure_visible_in_strip(
    markers: Query<(Entity, &LayoutPosition), With<EnsureVisibleMarker>>,
    strips: Query<(
        &LayoutStrip,
        Entity,
        &Position,
        &ChildOf,
        Option<Ref<ActiveWorkspaceMarker>>,
    )>,
    displays: Query<(&Display, Option<&DockPosition>)>,
    windows: Windows,
    config: Res<Config>,
    mut commands: Commands,
) {
    for (entity, layout_position) in markers {
        if let Ok(mut cmd) = commands.get_entity(entity) {
            cmd.try_remove::<EnsureVisibleMarker>();
        }
        let Some((_, strip_entity, strip_position, child, active_marker)) =
            strips.into_iter().find(|s| s.0.contains(entity))
        else {
            return;
        };

        if active_marker.is_some_and(|m| m.is_added()) {
            trace!("ensure_visible_in_strip: skipping newly active workspace {strip_entity}");
            return;
        }
        let Ok((display, dock)) = displays.get(child.parent()) else {
            return;
        };
        let Some(size) = windows.size(entity) else {
            return;
        };
        let viewport = display.actual_display_bounds(dock, &config);

        // Where the entity would appear if the strip stays put.
        let candidate_min = layout_position.0 + strip_position.0;
        // Clamp into the viewport. If already on-screen, this is a no-op and
        // the strip target equals its current position — no movement.
        let clamped_min = clamp_origin_to_viewport(candidate_min, size, viewport);
        if clamped_min == candidate_min {
            return;
        }
        let strip_target = (clamped_min - layout_position.0).with_y(strip_position.0.y);
        trace!("ensure_visible_in_strip: entity {entity}, scroll strip to {strip_target}");
        commands.reposition_entity(strip_entity, strip_target);
    }
}

/// Reacts to changes in the position of the `LayoutStrip` to Display, and if changed,
/// marks all the windows in the strip as requiring re-positioning.
#[allow(clippy::needless_pass_by_value)]
#[instrument(level = Level::DEBUG, skip_all)]
fn position_layout_strips(
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

#[derive(Clone, Copy)]
struct StripWindowContext {
    strip_position: Origin,
    swiping: bool,
    display_entity: Entity,
    stacked: bool,
}

fn insert_strip_window_contexts(
    contexts: &mut EntityHashMap<StripWindowContext>,
    strip: &LayoutStrip,
    strip_position: Origin,
    swiping: bool,
    display_entity: Entity,
) {
    for column in &strip.columns {
        insert_column_window_contexts(
            contexts,
            column,
            strip_position,
            swiping,
            display_entity,
            matches!(column, Column::Stack(_)),
        );
    }
}

fn insert_column_window_contexts(
    contexts: &mut EntityHashMap<StripWindowContext>,
    column: &Column,
    strip_position: Origin,
    swiping: bool,
    display_entity: Entity,
    stacked: bool,
) {
    match column {
        Column::Single(entity) | Column::Fullscren(entity) => {
            contexts.insert(
                *entity,
                StripWindowContext {
                    strip_position,
                    swiping,
                    display_entity,
                    stacked,
                },
            );
        }
        Column::Stack(items) => {
            for item in items {
                insert_stack_item_window_contexts(
                    contexts,
                    item,
                    strip_position,
                    swiping,
                    display_entity,
                    stacked,
                );
            }
        }
        Column::Tabs(entities) => {
            for entity in entities {
                contexts.insert(
                    *entity,
                    StripWindowContext {
                        strip_position,
                        swiping,
                        display_entity,
                        stacked,
                    },
                );
            }
        }
    }
}

fn insert_stack_item_window_contexts(
    contexts: &mut EntityHashMap<StripWindowContext>,
    item: &StackItem,
    strip_position: Origin,
    swiping: bool,
    display_entity: Entity,
    stacked: bool,
) {
    match item {
        StackItem::Single(entity) => {
            contexts.insert(
                *entity,
                StripWindowContext {
                    strip_position,
                    swiping,
                    display_entity,
                    stacked,
                },
            );
        }
        StackItem::Tabs(entities) => {
            for entity in entities {
                contexts.insert(
                    *entity,
                    StripWindowContext {
                        strip_position,
                        swiping,
                        display_entity,
                        stacked,
                    },
                );
            }
        }
    }
}

/// Reacts to changes of logical window layout in the strip and any have been changed, reposition
/// the layout strip against the current display viewport.
#[allow(clippy::needless_pass_by_value, clippy::type_complexity)]
#[instrument(level = Level::DEBUG, skip_all)]
fn position_layout_windows(
    positioned_windows: Populated<
        (Entity, &Window, &LayoutPosition, &mut Position, &mut Bounds),
        (Changed<LayoutPosition>, With<Window>, Without<LayoutStrip>),
    >,
    workspaces: Query<(&LayoutStrip, &Position, Has<Scrolling>, &ChildOf), With<LayoutStrip>>,
    displays: Query<(&Display, Option<&DockPosition>)>,
    config: Res<Config>,
    mut commands: Commands,
) {
    let offscreen_sliver_width = config.sliver_width();
    let (_, pad_right, _, pad_left) = config.edge_padding();
    let mut strip_contexts = EntityHashMap::default();
    for (layout_strip, Position(strip_position), swiping, child_of) in &workspaces {
        insert_strip_window_contexts(
            &mut strip_contexts,
            layout_strip,
            *strip_position,
            swiping,
            child_of.parent(),
        );
    }

    for (entity, window, layout_position, mut position, mut bounds) in positioned_windows {
        let Some(context) = strip_contexts.get(&entity) else {
            return;
        };
        let Ok((display, dock)) = displays.get(context.display_entity) else {
            return;
        };
        let viewport = display.actual_display_bounds(dock, &config);
        // Gets 80% of the display height as threshold.
        let Ok(vertical_move_threshold) = u32::try_from(viewport.height() * 8 / 10) else {
            continue;
        };

        // Account for per-window horizontal_padding: reposition() adds
        // h_pad to the virtual x, so subtract it here so the OS window
        // lands exactly sliver_width pixels from the screen edge.
        let h_pad = window.horizontal_padding();
        let mut frame = IRect::from_corners(layout_position.0, layout_position.0 + bounds.0);
        let width = frame.width();
        frame.min += context.strip_position;
        frame.max += context.strip_position;

        let mut offscreen = false;
        if frame.max.x <= viewport.min.x + h_pad {
            // Window hidden to the left — position so exactly
            // sliver_width CG pixels are visible from the real
            // display edge.  The +h_pad accounts for the gap that
            // reposition() adds, which can leave a window just
            // inside the viewport edge while its CG frame is fully
            // past it.
            frame.min.x = viewport.min.x - width + offscreen_sliver_width - pad_left + h_pad;
            offscreen = true;
        } else if frame.min.x >= viewport.max.x - h_pad {
            // Window hidden to the right — mirror of above.
            frame.min.x = viewport.max.x - offscreen_sliver_width + pad_right - h_pad;
            offscreen = true;
        }
        frame.max.x = frame.min.x + width;

        // During swipe, keep full height. The vertical sliver inset only
        // applies to horizontally off-screen windows, so they expose just
        // a `sliver_height` fraction of their height at the viewport's
        // vertical center.
        if !context.swiping && offscreen {
            // Don't compress stacked windows vertically when off-screen.
            // The height reduction corrupts their proportions: when the
            // column scrolls back on-screen, binpack_heights makes the
            // last window absorb all remaining space.
            if !context.stacked {
                let inset =
                    (f64::from(viewport.height()) * (1.0 - config.sliver_height()) / 2.0) as i32;
                frame.min.y += inset;
                frame.max.y += inset;
            }
        }

        if bounds.0 != frame.size() {
            bounds.0 = frame.size();
        }

        if position.0 != frame.min {
            // Direct-assign (snap) when:
            //   - The user is actively swiping: windows must track the finger in lockstep.
            //   - A workspace switch just moved the strip vertically: jumping the full off-screen
            //   distance should be instantaneous.
            // Otherwise (programmatic strip animation, or pure layout change), animate toward the
            // new position so layout changes (swap/add/remove) slide instead of teleport. When the
            // strip is also being animated, the per-window target is recomputed each tick from the
            // strip's current position, so the two motions compose: e.g., on swap, the focused
            // window's target converges back to its old visual position as the strip settles, while
            // the other window slides past.
            let offscreen_move = position.0.y.abs_diff(frame.min.y) > vertical_move_threshold;
            if context.swiping || offscreen_move && !config.virtual_workspace_animations() {
                position.0 = frame.min;
                if let Ok(mut entity_commands) = commands.get_entity(entity) {
                    entity_commands.try_remove::<RepositionMarker>();
                }
            } else {
                commands.reposition_entity(entity, frame.min);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bevy::prelude::*;

    #[test]
    fn clamp_origin_supports_oversized_windows() {
        let viewport = IRect::new(0, 20, 1024, 768);
        let size = Size::new(2048, 748);

        assert_eq!(
            clamp_origin_to_viewport(Origin::new(300, 20), size, viewport),
            Origin::new(0, 20)
        );
        assert_eq!(
            clamp_origin_to_viewport(Origin::new(-1600, 20), size, viewport),
            Origin::new(-1024, 20)
        );
        assert_eq!(
            clamp_origin_to_viewport(Origin::new(-600, 20), size, viewport),
            Origin::new(-600, 20)
        );
    }

    #[test]
    fn clamp_origin_keeps_regular_windows_inside_viewport() {
        let viewport = IRect::new(0, 20, 1024, 768);
        let size = Size::new(400, 300);

        assert_eq!(
            clamp_origin_to_viewport(Origin::new(-100, 900), size, viewport),
            Origin::new(0, 468)
        );
    }

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
    fn strip_has_changed_window_matches_nested_entities() {
        let (mut world, mut strip, entities) = setup_world_and_strip();
        strip.stack(entities[1]).unwrap();
        strip.convert_to_tabs(entities[0], entities[1]).unwrap();

        let mut changed = EntityHashSet::default();
        changed.insert(entities[1]);

        assert!(strip_has_changed_window(&strip, &changed));

        changed.clear();
        changed.insert(world.spawn_empty().id());

        assert!(!strip_has_changed_window(&strip, &changed));
    }

    #[test]
    fn strip_window_contexts_capture_stack_membership_once() {
        let (mut world, mut strip, entities) = setup_world_and_strip();
        strip.stack(entities[1]).unwrap();
        let display_entity = world.spawn_empty().id();
        let strip_position = Origin::new(10, 20);
        let mut contexts = EntityHashMap::default();

        insert_strip_window_contexts(&mut contexts, &strip, strip_position, true, display_entity);

        let stacked_leader = contexts.get(&entities[0]).unwrap();
        let stacked_follower = contexts.get(&entities[1]).unwrap();
        let single_window = contexts.get(&entities[2]).unwrap();

        assert_eq!(stacked_leader.strip_position, strip_position);
        assert_eq!(stacked_leader.display_entity, display_entity);
        assert!(stacked_leader.swiping);
        assert!(stacked_leader.stacked);
        assert!(stacked_follower.stacked);
        assert!(!single_window.stacked);
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

        // Give the master (e0) a distinct width so we can verify children
        // adopt it. Here e0 is 500px while its stacked children are 300px;
        // children must widen to the master's 500, not keep their own width.
        let get_window_frame = |e: Entity| {
            if e == entities[0] {
                Some(IRect::new(0, 0, 500, 200))
            } else if e == entities[1] || e == entities[2] {
                Some(IRect::new(0, 0, 300, 200))
            } else {
                Some(IRect::new(0, 0, 400, 500))
            }
        };

        let out: Vec<_> = strip.relative_positions(600, &get_window_frame).collect();
        assert_eq!(out.len(), 4);

        // Every window in the stacked column must adopt the master's width.
        for e in [entities[0], entities[1], entities[2]] {
            let frame = out.iter().find(|(entity, _)| *entity == e).unwrap().1;
            assert_eq!(
                frame.width(),
                500,
                "stacked window must share the master's (top) width"
            );
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
                    StackItem::Tabs(tabs) => assert_eq!(tabs, &vec![e4, e1]),
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
                assert_eq!(tabs, vec![e2, e1]);
            }
            _ => panic!("Expected Tabs column"),
        }

        // Add another tab e4
        let e4 = world.spawn_empty().id();
        strip.convert_to_tabs(e1, e4).unwrap();
        assert_eq!(strip.len(), 2);
        match strip.get(0).unwrap() {
            Column::Tabs(tabs) => {
                assert_eq!(tabs, vec![e4, e2, e1]);
            }
            _ => panic!("Expected Tabs column"),
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
            Column::Tabs(tabs) => assert_eq!(tabs, vec![e3, e1]),
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
    fn test_tab_group_returns_all_siblings() {
        let mut world = World::new();
        let e1 = world.spawn_empty().id();
        let e2 = world.spawn_empty().id();
        let e3 = world.spawn_empty().id();

        let mut strip = LayoutStrip::default();
        strip.append(e1);
        strip.convert_to_tabs(e1, e2).unwrap();
        strip.append(e3);

        assert_eq!(strip.tab_group(e1), Some(vec![e2, e1]));
        assert_eq!(strip.tab_group(e2), Some(vec![e2, e1]));
        assert_eq!(strip.tab_group(e3), None);
    }

    #[test]
    fn test_append_tab_group_merges_existing_members_without_duplicate_columns() {
        let mut world = World::new();
        let e1 = world.spawn_empty().id();
        let e2 = world.spawn_empty().id();
        let e3 = world.spawn_empty().id();

        let mut strip = LayoutStrip::default();
        strip.append(e2);
        strip.append(e3);

        strip.append_tab_group(&[e1, e2]);

        assert_eq!(strip.len(), 2);
        match strip.get(0).unwrap() {
            Column::Tabs(tabs) => assert_eq!(tabs, vec![e1, e2]),
            _ => panic!("Expected merged Tabs column"),
        }
        assert_eq!(strip.index_of(e1).unwrap(), 0);
        assert_eq!(strip.index_of(e2).unwrap(), 0);
        assert_eq!(strip.index_of(e3).unwrap(), 1);
        assert_eq!(strip.all_windows(), vec![e1, e2, e3]);
    }

    #[test]
    fn test_tab_relative_positions_use_stable_slot_representative() {
        let mut world = World::new();
        let e1 = world.spawn_empty().id();
        let e2 = world.spawn_empty().id();

        let mut strip = LayoutStrip::default();
        strip.append(e1);
        strip.convert_to_tabs(e1, e2).unwrap();
        strip
            .get_column_mut(0)
            .expect("tab column")
            .move_to_front(e2);

        let get_window_frame = |entity| {
            if entity == e1 {
                Some(IRect::new(0, 0, 300, 600))
            } else {
                Some(IRect::new(900, 0, 1200, 400))
            }
        };

        let out = strip
            .relative_positions(600, &get_window_frame)
            .collect::<Vec<_>>();

        assert_eq!(out.len(), 2);
        assert!(
            out.iter()
                .all(|(_, frame)| *frame == IRect::new(0, 0, 300, 600))
        );
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
            Column::Tabs(tabs) => assert_eq!(tabs, vec![e2, e1]),
            _ => panic!(),
        }
    }

    // Mirrors the real `detect_tabbed_windows` flow: spawn_window_trigger
    // appends the new window to the strip first, and only then does the
    // tab detector merge it into the leader's column. Before the fix, the
    // follower was left in both columns, and right_neighbour from the
    // duplicated entity would self-loop because index_of returned the Tabs
    // column index, while the column at the next index was the orphaned
    // Single(follower).
    #[test]
    fn test_convert_to_tabs_removes_pre_existing_follower_column() {
        let mut world = World::new();
        let leader = world.spawn_empty().id();
        let follower = world.spawn_empty().id();

        let mut strip = LayoutStrip::default();
        strip.append(leader);
        strip.append(follower);

        strip.convert_to_tabs(leader, follower).unwrap();

        assert_eq!(
            strip.len(),
            1,
            "follower must not remain in its own column after being tabbed onto leader",
        );
        match strip.get(0).unwrap() {
            Column::Tabs(tabs) => assert_eq!(tabs, vec![follower, leader]),
            other => panic!("expected Tabs column, got {other:?}"),
        }
    }

    #[test]
    fn test_convert_to_tabs_no_self_loop_on_neighbour() {
        let mut world = World::new();
        let a = world.spawn_empty().id();
        let leader = world.spawn_empty().id();
        let follower = world.spawn_empty().id();
        let b = world.spawn_empty().id();

        let mut strip = LayoutStrip::default();
        strip.append(a);
        strip.append(leader);
        strip.append(follower);
        strip.append(b);

        strip.convert_to_tabs(leader, follower).unwrap();

        // Both leader-as-focus and follower-as-focus must navigate to `b`
        // east, not back onto themselves.
        assert_eq!(strip.right_neighbour(leader), Some(b));
        assert_eq!(strip.right_neighbour(follower), Some(b));
        assert_eq!(strip.left_neighbour(leader), Some(a));
        assert_eq!(strip.left_neighbour(follower), Some(a));
    }

    #[test]
    fn test_convert_to_tabs_handles_follower_left_of_leader() {
        let mut world = World::new();
        let follower = world.spawn_empty().id();
        let leader = world.spawn_empty().id();
        let b = world.spawn_empty().id();

        let mut strip = LayoutStrip::default();
        strip.append(follower);
        strip.append(leader);
        strip.append(b);

        strip.convert_to_tabs(leader, follower).unwrap();

        assert_eq!(strip.len(), 2);
        match strip.get(0).unwrap() {
            Column::Tabs(tabs) => assert_eq!(tabs, vec![follower, leader]),
            other => panic!("expected Tabs column, got {other:?}"),
        }
        assert_eq!(strip.right_neighbour(leader), Some(b));
        assert_eq!(strip.right_neighbour(follower), Some(b));
    }
}

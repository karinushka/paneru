use bevy::ecs::change_detection::DetectChangesMut as _;
use bevy::ecs::component::Component;
use bevy::ecs::entity::Entity;
use bevy::ecs::query::{Changed, Has, With, Without};
use bevy::ecs::system::{Commands, Populated, Query, Res, Single};
use bevy::math::IRect;
use bevy::time::Time;
use std::collections::VecDeque;
use std::time::Duration;
use stdext::function_name;
use tracing::{Level, debug, instrument, trace};

use crate::config::Config;
use crate::config::swipe::SwipeGestureDirection;
use crate::ecs::params::{ActiveDisplay, Configuration, Windows};
use crate::ecs::{
    ActiveDisplayMarker, ActiveWorkspaceMarker, Bounds, DockPosition, LayoutPosition, Position,
    ReshuffleAroundMarker, Scrolling, WMEventTrigger, reposition_entity,
};
use crate::errors::{Error, Result};
use crate::events::Event;
use crate::manager::{Display, Window, WindowManager};
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

    /// Returns the entity at the given stack index, or the last entity if the index exceeds the stack size.
    pub fn at_or_last(&self, index: usize) -> Option<Entity> {
        match self {
            Column::Single(id) => Some(*id),
            Column::Stack(stack) => stack.get(index).or_else(|| stack.last()).copied(),
        }
    }

    /// Returns the position of an entity within this column (0 for Single, stack index for Stack).
    pub fn position_of(&self, entity: Entity) -> Option<usize> {
        match self {
            Column::Single(id) => (*id == entity).then_some(0),
            Column::Stack(stack) => stack.iter().position(|&e| e == entity),
        }
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
                let windows = match column {
                    Column::Single(entity) => vec![*entity],
                    Column::Stack(stack) => stack.clone(),
                };
                let current_heights = windows
                    .iter()
                    .filter_map(|&entity| get_window_frame(entity))
                    .map(|frame| frame.height())
                    .collect::<Vec<_>>();
                let heights =
                    binpack_heights(&current_heights, MIN_WINDOW_HEIGHT, layout_strip_height)?;

                let column_width = windows
                    .first()
                    .and_then(|&entity| get_window_frame(entity))
                    .map(|frame| frame.width())?;

                let mut next_y = 0;
                let frames = windows
                    .into_iter()
                    .zip(heights)
                    .filter_map(|(entity, height)| {
                        let mut frame = get_window_frame(entity)?;
                        frame.min.x = position;
                        frame.max.x = frame.min.x + column_width;

                        frame.min.y = next_y;
                        frame.max.y = frame.min.y + height;

                        next_y = frame.max.y;

                        Some((entity, frame))
                    })
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
        let stack = self.index_of(entity).and_then(|idx| self.get(idx)).ok()?;
        match stack {
            Column::Single(_) => None,
            Column::Stack(items) => {
                let pos = items.iter().position(|&e| e == entity)?;
                (pos > 0).then(|| items[pos - 1])
            }
        }
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

pub fn binpack_heights(heights: &[i32], min_height: i32, total_height: i32) -> Option<Vec<i32>> {
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

#[instrument(level = Level::TRACE, skip_all, fields(current_offset, shift), ret)]
fn clamp_viewport_offset<W>(
    current_offset: i32,
    shift: i32,
    layout_strip: &LayoutStrip,
    windows: &Windows,
    get_window_frame: &W,
    viewport: &IRect,
    config: &Config,
) -> Option<i32>
where
    W: Fn(Entity) -> Option<IRect>,
{
    let swipe_direction_modifier = match config.swipe_gesture_direction() {
        SwipeGestureDirection::Natural => 1,
        SwipeGestureDirection::Reversed => -1,
    };
    let shift = shift * swipe_direction_modifier;

    let total_strip_width = layout_strip
        .last()
        .ok()
        .and_then(|column| column.top())
        .and_then(|entity| {
            windows
                .layout_position(entity)
                .zip(get_window_frame(entity))
        })
        .map(|(position, frame)| position.x + frame.width())?;

    // Continous swipe is on by default.
    let continuous_swipe = config.options().continuous_swipe.is_none_or(|swipe| swipe);
    let strip_position = |column: Result<Column>| {
        column
            .ok()
            .and_then(|column| column.top())
            .and_then(|entity| windows.layout_position(entity))
            .map(|position| position.0.x)
    };
    let left_snap = strip_position(layout_strip.last());
    let right_snap = strip_position(layout_strip.get(1));
    Some(
        if continuous_swipe && let Some((left_snap, right_snap)) = left_snap.zip(right_snap) {
            // Allow to scroll away until the last or first window snaps.
            (current_offset - shift).clamp(viewport.min.x - left_snap, viewport.max.x - right_snap)
        } else if viewport.width() < total_strip_width {
            // Snap the strip directly to the edges.
            (current_offset - shift).clamp(viewport.max.x - total_strip_width, viewport.min.x)
        } else {
            // Snap the strip directly to the edges.
            (current_offset - shift).clamp(viewport.min.x, viewport.max.x - total_strip_width)
        },
    )
}

#[allow(clippy::needless_pass_by_value, clippy::type_complexity)]
#[instrument(level = Level::DEBUG, skip_all)]
pub fn magnetic_snap_to_center(
    mut active_workspace: Single<
        (&LayoutStrip, &mut Position, &mut Scrolling),
        (With<ActiveWorkspaceMarker>, Without<Window>),
    >,
    active_display: Single<(&Display, Option<&DockPosition>), With<ActiveDisplayMarker>>,
    windows: Windows,
    config: Configuration,
    time: Res<Time>,
) {
    const CENTER_MAGNETIC_PULL: f64 = 0.8;
    const CENTER_MAGNETIC_FORCE: f64 = 4.0;
    // Use 5% of the display width as the snap threshold
    const SNAP_DISPLAY_RATIO: f64 = 0.05;

    let (strip, ref mut position, ref mut scroll) = *active_workspace;
    if scroll.is_user_swiping {
        return;
    }

    let get_window_frame = |entity| windows.moving_frame(entity);
    let viewport = active_display
        .0
        .actual_display_bounds(active_display.1, config.config());

    let current_position = position.x;
    let viewport_center = viewport.center().x;
    let target_offset = strip
        .all_columns()
        .into_iter()
        .filter_map(|entity| {
            windows
                .layout_position(entity)
                .map(|p| p.0.x)
                .zip(Some(entity))
        })
        .map(|(position, entity)| {
            let col_width = get_window_frame(entity).map_or(0, |f| f.width());
            viewport_center - (position + col_width / 2)
        })
        .min_by_key(|target| (current_position - target).abs())
        .unwrap_or(current_position);

    let snap_threshold = SNAP_DISPLAY_RATIO * f64::from(viewport.width());
    let dist_to_snap = f64::from(current_position - target_offset);

    if dist_to_snap.abs() < snap_threshold {
        // Magnetic pull: slow down and nudge towards center.
        let dt = time.delta_secs_f64();
        scroll.velocity *= CENTER_MAGNETIC_PULL;
        position.x -= (dist_to_snap * dt * CENTER_MAGNETIC_FORCE) as i32;
    }
}

#[allow(clippy::needless_pass_by_value, clippy::type_complexity)]
#[instrument(level = Level::DEBUG, skip_all)]
pub(super) fn apply_scroll_physics(
    mut active_workspace: Single<
        (&LayoutStrip, &mut Position, &mut Scrolling),
        (With<ActiveWorkspaceMarker>, Without<Window>),
    >,
    active_display: Single<(&Display, Option<&DockPosition>), With<ActiveDisplayMarker>>,
    windows: Windows,
    config: Configuration,
    time: Res<Time>,
) {
    const FINGER_LIFT_THRESHOLD: Duration = Duration::from_millis(50);

    let (strip, ref mut position, ref mut scroll) = *active_workspace;

    // Finger lift detection
    if scroll.is_user_swiping && scroll.last_event.elapsed() > FINGER_LIFT_THRESHOLD {
        scroll.is_user_swiping = false;
        return;
    }

    let get_window_frame = |entity| windows.moving_frame(entity);
    let viewport = active_display
        .0
        .actual_display_bounds(active_display.1, config.config());
    let dt = time.delta_secs_f64();
    let frame_delta = scroll.velocity * dt;
    let shift = (f64::from(viewport.width()) * frame_delta) as i32;

    if let Some(clamped_offset) = clamp_viewport_offset(
        position.x,
        shift,
        strip,
        &windows,
        &get_window_frame,
        &viewport,
        config.config(),
    ) {
        position.x = clamped_offset;
    } else {
        scroll.velocity = 0.0;
    }
}

#[allow(clippy::needless_pass_by_value, clippy::type_complexity)]
#[instrument(level = Level::DEBUG, skip_all)]
pub(super) fn apply_scroll_physics_post_swipe(
    mut active_workspace: Single<
        (Entity, &mut Scrolling),
        (With<ActiveWorkspaceMarker>, Without<Window>),
    >,
    active_display: Single<(&Display, Option<&DockPosition>), With<ActiveDisplayMarker>>,
    window_manager: Res<WindowManager>,
    mut config: Configuration,
    time: Res<Time>,
    mut commands: Commands,
) {
    const FOCUS_VELOCITY_RATIO: f64 = 0.3;
    const MIN_VELOCITY_PX: f64 = 100.0;

    let (entity, ref mut scroll) = *active_workspace;
    if scroll.is_user_swiping {
        return;
    }

    // While user is swiping, velocity is directly applied in the trigger.
    // We just need to update the position.
    let dt = time.delta_secs_f64();
    let display_width = f64::from(active_display.0.bounds().width());
    let scroll_velocity = scroll.velocity.abs() * display_width;

    if scroll_velocity < FOCUS_VELOCITY_RATIO * display_width
        && config.focus_follows_mouse()
        && let Some(point) = window_manager.cursor_position()
    {
        config.set_ffm_flag(None);
        commands.trigger(WMEventTrigger(Event::MouseMoved { point }));
    }

    if scroll_velocity < MIN_VELOCITY_PX {
        // Below threshold: stop and focus
        if let Ok(mut cmd) = commands.get_entity(entity) {
            cmd.try_remove::<Scrolling>();
        }
        return;
    }
    // Apply inertia decay
    let decay_rate = config.config().swipe_deceleration();
    scroll.velocity *= (-decay_rate * dt).exp();
}

/// Watches for size changes to windows and if they are changed, re-calculates the logical
/// positions of all the windows in their layout strip.
#[allow(clippy::needless_pass_by_value)]
#[instrument(level = Level::DEBUG, skip_all)]
pub(super) fn layout_sizes_changed(
    changed_sizes: Populated<Entity, Changed<Bounds>>,
    windows: Query<(&Position, &Bounds, &Window), Without<LayoutStrip>>,
    mut layout_position: Query<&mut LayoutPosition, With<Window>>,
    active_display: ActiveDisplay,
    config: Res<Config>,
) {
    let viewport = active_display
        .display()
        .actual_display_bounds(active_display.dock(), &config);
    let layout_strip = active_display.active_strip();

    let get_window_frame = |entity| {
        windows
            .get(entity)
            .map(|(position, bounds, _)| IRect::from_corners(position.0, position.0 + bounds.0))
            .ok()
    };

    changed_sizes
        .into_iter()
        .filter_map(|entity| {
            layout_strip
                .index_of(entity)
                .is_ok()
                .then_some(layout_strip.relative_positions(viewport.height(), &get_window_frame))
        })
        .flatten()
        .for_each(|(entity, frame)| {
            if let Ok(mut layout_position) = layout_position.get_mut(entity) {
                layout_position.0 = frame.min;
            }
        });
}

/// Watches for changes to `LayoutStrip` (i.e. a window added or window order changed) and
/// re-calculates the logical positions of all the windows in the layout strip.
#[allow(clippy::needless_pass_by_value, clippy::type_complexity)]
#[instrument(level = Level::DEBUG, skip_all)]
pub(super) fn layout_strip_changed(
    changed_strips: Populated<&LayoutStrip, Changed<LayoutStrip>>,
    mut windows: Query<
        (&Position, &mut Bounds, &mut LayoutPosition),
        (Without<LayoutStrip>, With<Window>),
    >,
    active_display: ActiveDisplay,
    config: Res<Config>,
) {
    let viewport = active_display
        .display()
        .actual_display_bounds(active_display.dock(), &config);

    let get_window_frame = |entity| {
        windows
            .get(entity)
            .map(|(position, bounds, _)| IRect::from_corners(position.0, position.0 + bounds.0))
            .ok()
    };

    let changed = changed_strips
        .into_iter()
        .flat_map(|layout_strip| {
            layout_strip.relative_positions(viewport.height(), &get_window_frame)
        })
        .collect::<Vec<_>>();

    for (entity, frame) in changed {
        if let Ok((_, mut bounds, mut layout_position)) = windows.get_mut(entity) {
            layout_position.0 = frame.min;
            if bounds.0 != frame.size() {
                bounds.0 = frame.size();
            }
        }
    }
}

#[allow(clippy::needless_pass_by_value)]
#[instrument(level = Level::DEBUG, skip_all)]
pub(super) fn reshuffle_layout_strip(
    marker: Populated<(Entity, &LayoutPosition), With<ReshuffleAroundMarker>>,
    active_display: ActiveDisplay,
    windows: Windows,
    config: Res<Config>,
    mut commands: Commands,
) {
    let display_bounds = active_display
        .display()
        .actual_display_bounds(active_display.dock(), &config);

    for (entity, layout_position) in marker {
        if let Ok(mut cmd) = commands.get_entity(entity) {
            cmd.try_remove::<ReshuffleAroundMarker>();
        }
        if active_display.active_strip().index_of(entity).is_err() {
            continue;
        }

        let Some(mut frame) = windows.moving_frame(entity) else {
            continue;
        };
        let size = frame.size();

        if frame.max.x > display_bounds.max.x {
            trace!("Bumped window {entity} to the left");
            frame.min.x = display_bounds.max.x - size.x;
        } else if frame.min.x < display_bounds.min.x {
            trace!("Bumped window {entity} to the right");
            frame.min.x = display_bounds.min.x;
        }
        frame.max.x = frame.min.x + size.x;

        let strip_position = frame.min - layout_position.0;
        trace!("reshuffle_layout_strip: triggered for entity {entity}, offset {strip_position}");
        reposition_entity(
            active_display.active_strip_entity(),
            strip_position,
            &mut commands,
        );
    }
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
    positioned_windows: Populated<
        (Entity, &Window, &LayoutPosition, &mut Position, &mut Bounds),
        (Changed<LayoutPosition>, With<Window>),
    >,
    active_workspace: Single<
        (&LayoutStrip, &Position, Has<Scrolling>),
        (With<ActiveWorkspaceMarker>, Without<Window>),
    >,
    active_display: Single<(&Display, Option<&DockPosition>), With<ActiveDisplayMarker>>,
    config: Res<Config>,
) {
    let (active_display, dock) = *active_display;
    let viewport = active_display.actual_display_bounds(dock, &config);
    let (layout_strip, strip_position, swiping) = *active_workspace;
    let strip_position = strip_position.0.with_y(viewport.min.y);
    let offscreen_sliver_width = config.sliver_width();
    let (_, pad_right, _, pad_left) = config.edge_padding();

    for (entity, window, layout_position, mut position, mut bounds) in positioned_windows {
        if layout_strip.index_of(entity).is_err() {
            continue;
        }

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
            position.0 = frame.min;
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
}

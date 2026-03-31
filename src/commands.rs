use bevy::app::PreUpdate;
use bevy::ecs::entity::{Entity, EntityHashSet};
use bevy::ecs::hierarchy::ChildOf;
use bevy::ecs::message::MessageReader;
use bevy::ecs::query::{Has, With, Without};
use bevy::ecs::system::{Commands, Query, Res, ResMut, Single};
use bevy::math::IRect;
use tracing::{Level, instrument};
use tracing::{debug, info};

use crate::config::Config;
use crate::ecs::layout::{Column, LayoutStrip, StackItem};
use crate::ecs::params::{ActiveDisplay, ActiveDisplayMut, Windows};
use crate::ecs::{
    ActiveDisplayMarker, ActiveWorkspaceMarker, FocusFollowsMouse, FocusedMarker, FullWidthMarker,
    SelectedVirtualMarker, SendMessageTrigger, Unmanaged, VirtualMoveMarker, WMEventTrigger,
    reposition_entity, reshuffle_around, resize_entity,
};
use crate::events::Event;
use crate::manager::{Application, Display, Origin, Size, Window, WindowManager, origin_to};

/// Represents a cardinal or directional choice for window manipulation.
#[derive(Clone, Debug)]
pub enum Direction {
    North,
    South,
    West,
    East,
    First,
    Last,
}

/// Direction used when cycling preset resize widths.
#[derive(Clone, Copy, Debug)]
pub enum ResizeDirection {
    Grow,
    Shrink,
}

/// Defines the various operations that can be performed on windows.
#[derive(Clone, Debug)]
pub enum Operation {
    /// Focuses on a window in the specified `Direction`.
    Focus(Direction),
    /// Swaps the current window with another in the specified `Direction`.
    Swap(Direction),
    /// Centers the currently focused window on the display.
    Center,
    /// Resizes the focused window in the given direction.
    Resize(ResizeDirection),
    /// Toggles the focused window to full width or a preset width.
    FullWidth,
    /// Moves the focused window to the next available display.
    ToNextDisplay,
    /// Distributes heights equally among windows in the focused stack.
    Equalize,
    /// Toggles the managed state of the focused window.
    Manage,
    /// Stacks or unstacks a window. The boolean indicates whether to stack (`true`) or unstack (`false`).
    Stack(bool),
    /// Resizes and repositions the focused window to fit within the visible viewport
    /// (including edge padding).
    Snap,
    /// Cyclically selects the virtual strip for the current workspace.
    Virtual(Direction),
    /// Moves the focused window to the virtual strip.
    VirtualMove(Direction),
}

/// Defines operations that can be performed on the mouse.
#[derive(Clone, Debug)]
pub enum MouseMove {
    /// Moves the mouse pointer to the next available display.
    ToNextDisplay,
}

/// Represents a command that can be issued to the window manager.
#[derive(Clone, Debug)]
pub enum Command {
    /// A command targeting a window with a specific `Operation`.
    Window(Operation),
    /// A command targeting the mouse with a specific `MouseOperation`.
    Mouse(MouseMove),
    /// A command to quit the window manager application.
    Quit,
    PrintState,
}

pub fn register_commands(app: &mut bevy::app::App) {
    app.add_systems(
        PreUpdate,
        (
            command_quit_handler,
            print_internal_state_handler,
            mouse_to_next_display,
            resize_window,
            command_center_window,
            full_width_window,
            to_next_display,
            equalize_column,
            manage_window,
            stack_windows_handler,
            command_move_focus,
            command_swap_focus,
            snap_window,
            virtual_workspace_handler,
            virtual_window_move_handler,
        ),
    );
}

fn filter_window_operations<'a, F: Fn(&Operation) -> bool>(
    messages: &'a mut MessageReader<Event>,
    filter: F,
) -> impl Iterator<Item = &'a Operation> {
    messages.read().filter_map(move |event| {
        if let Event::Command {
            command: Command::Window(op),
        } = event
            && filter(op)
        {
            Some(op)
        } else {
            None
        }
    })
}

#[instrument(level = Level::DEBUG, skip_all)]
#[allow(clippy::needless_pass_by_value)]
fn virtual_workspace_handler(
    mut messages: MessageReader<Event>,
    active_display: ActiveDisplay,
    workspaces: Query<(Entity, &LayoutStrip, Has<ActiveWorkspaceMarker>)>,
    mut commands: Commands,
) {
    let Some(Operation::Virtual(direction)) =
        filter_window_operations(&mut messages, |op| matches!(op, Operation::Virtual(_))).next()
    else {
        return;
    };

    let workspace_id = active_display.active_strip().id();
    let mut rows = workspaces
        .iter()
        .filter(|(_, strip, _)| strip.id() == workspace_id)
        .collect::<Vec<_>>();

    if rows.is_empty() {
        return;
    }
    rows.sort_by_key(|(_, strip, _)| strip.virtual_index);

    let current_index = rows.iter().position(|(_, _, active)| *active).unwrap_or(0);
    let next_index = match direction {
        Direction::South => (current_index + 1) % rows.len(),
        Direction::North => (current_index + rows.len() - 1) % rows.len(),
        _ => return,
    };

    if next_index == current_index {
        return;
    }

    let old_entity = rows[current_index].0;
    let new_entity = rows[next_index].0;
    if let Ok(mut entity_cmmands) = commands.get_entity(old_entity) {
        entity_cmmands
            .try_remove::<SelectedVirtualMarker>()
            .try_remove::<ActiveWorkspaceMarker>();
    }
    if let Ok(mut entity_cmmands) = commands.get_entity(new_entity) {
        entity_cmmands
            .try_insert(SelectedVirtualMarker)
            .try_insert(ActiveWorkspaceMarker);
    }
    debug!(
        "Switched virtual workspace on display {} from {} to {}",
        active_display.id(),
        rows[current_index].1.virtual_index,
        rows[next_index].1.virtual_index
    );
}

#[instrument(level = Level::DEBUG, skip_all)]
#[allow(clippy::needless_pass_by_value)]
fn virtual_window_move_handler(
    mut messages: MessageReader<Event>,
    windows: Windows,
    active_display: ActiveDisplay,
    mut commands: Commands,
) {
    let Some(Operation::VirtualMove(direction)) =
        filter_window_operations(&mut messages, |op| matches!(op, Operation::VirtualMove(_)))
            .next()
    else {
        return;
    };

    let Some((_, focused_entity)) = windows.focused() else {
        return;
    };

    let current_virtual_index = active_display.active_strip().virtual_index;

    let target_virtual_index = match direction {
        Direction::South if active_display.active_strip().len() > 1 => current_virtual_index + 1,
        Direction::North => {
            if current_virtual_index == 0 {
                return;
            }
            current_virtual_index - 1
        }
        _ => return,
    };

    commands.entity(focused_entity).insert(VirtualMoveMarker {
        target_virtual_index,
    });
    debug!("Moving {focused_entity} to new virtual space {target_virtual_index}");
}

/// Retrieves a window `Entity` in a specified direction relative to a `current_window_id` within a `LayoutStrip`.
///
/// # Arguments
///
/// * `direction` - The direction (e.g., `West`, `East`, `First`, `Last`, `North`, `South`).
/// * `current_window_id` - The `Entity` of the current window.
/// * `strip` - A reference to the `LayoutStrip` to search within.
///
/// # Returns
///
/// `Some(Entity)` with the found window's entity, otherwise `None`.
#[instrument(level = Level::DEBUG, ret)]
fn get_window_in_direction(
    direction: &Direction,
    entity: Entity,
    strip: &LayoutStrip,
) -> Option<Entity> {
    let index = strip.index_of(entity).ok()?;

    match direction {
        Direction::West => strip.left_neighbour(entity),
        Direction::East => strip.right_neighbour(entity),

        Direction::First => strip.first().ok().and_then(|column| column.top()),

        Direction::Last => strip.last().ok().and_then(|column| column.top()),

        Direction::North => match strip.get(index).ok()? {
            Column::Single(_) | Column::Tabs(_) => None,
            Column::Stack(stack) => stack
                .iter()
                .enumerate()
                .find(|(_, item)| item.contains(entity))
                .and_then(|(index, _)| (index > 0).then(|| stack.get(index - 1)).flatten())
                .and_then(StackItem::top),
        },

        Direction::South => match strip.get(index).ok()? {
            Column::Single(_) | Column::Tabs(_) => None,
            Column::Stack(stack) => stack
                .iter()
                .enumerate()
                .find(|(_, item)| item.contains(entity))
                .and_then(|(index, _)| {
                    (index < stack.len() - 1)
                        .then(|| stack.get(index + 1))
                        .flatten()
                })
                .and_then(StackItem::top),
        },
    }
}

/// Handles the "focus" command, moving focus to a window in a specified direction.
///
/// # Arguments
///
/// * `direction` - The `Direction` to move focus (e.g., `Direction::East`).
/// * `current_window` - The `Entity` of the currently focused `Window`.
/// * `strip` - A reference to the active `LayoutStrip`.
/// * `windows` - A query for all `Window` components.
///
/// # Returns
///
/// `Some(Entity)` with the entity of the newly focused window, otherwise `None`.
#[instrument(level = Level::DEBUG, skip_all)]
#[allow(clippy::needless_pass_by_value)]
fn command_move_focus(
    mut messages: MessageReader<Event>,
    windows: Windows,
    active_display: ActiveDisplay,
    apps: Query<&Application>,
    fs_windows: Query<(Entity, &crate::ecs::NativeFullscreenMarker)>,
    mut commands: Commands,
) {
    let Some(Operation::Focus(direction)) =
        filter_window_operations(&mut messages, |op| matches!(op, Operation::Focus(_))).next()
    else {
        return;
    };

    let strip = active_display.active_strip();

    // Fullscreen windows are ordered after the tiled strip:
    //   [tiled…] [fs order=0] [fs order=1] …
    // West → previous in chain (or last tiled).  East → next in chain.
    if active_display.display().is_fullscreen {
        let Some(cur) = windows
            .focused()
            .and_then(|(_, e)| fs_windows.get(e).ok())
            .map(|(_, m)| m.order)
        else {
            return;
        };
        let mut sorted: Vec<_> = fs_windows.iter().collect();
        sorted.sort_by_key(|(_, m)| m.order);
        let pos = sorted.iter().position(|(_, m)| m.order == cur);
        let target = match direction {
            Direction::West => pos
                .and_then(|p| p.checked_sub(1))
                .map(|p| sorted[p].0)
                .or_else(|| strip.get(strip.len().wrapping_sub(1)).ok()?.top()),
            Direction::East => pos.and_then(|p| sorted.get(p + 1)).map(|(e, _)| *e),
            _ => None,
        };
        if let Some(entity) = target
            && let Some(window) = windows.get(entity)
            && let Some(psn) = windows.psn(window.id(), &apps)
        {
            debug!("fullscreen: raising {entity}");
            window.focus_with_raise(psn);
        }
        return;
    }

    let Some((_, focused_entity)) = windows.focused() else {
        return;
    };

    // At the right edge going East, enter the fullscreen chain (lowest order first).
    let candidate = get_window_in_direction(direction, focused_entity, strip).or_else(|| {
        (matches!(direction, Direction::East) && strip.right_neighbour(focused_entity).is_none())
            .then(|| {
                fs_windows
                    .iter()
                    .min_by_key(|(_, m)| m.order)
                    .map(|(e, _)| e)
            })
            .flatten()
    });

    if let Some(entity) = candidate
        && let Some(window) = windows.get(entity)
        && let Some(psn) = windows.psn(window.id(), &apps)
    {
        window.focus_with_raise(psn);
        // Update FocusedMarker immediately so that rapid successive keypresses
        // see the correct focused entity without waiting for the OS event round-trip.
        commands
            .entity(focused_entity)
            .try_remove::<FocusedMarker>();
        commands.entity(entity).try_insert(FocusedMarker);
        // Explicitly reshuffle so the target window is brought into view.
        // This avoids a race where focus-follows-mouse leaves skip_reshuffle
        // set, causing the WindowFocused handler to skip the reshuffle.
        reshuffle_around(entity, &mut commands);
        return;
    }

    // Check if the movement can switch to another display.
    let Some(other_display) = active_display.other().next() else {
        return;
    };
    let change_display = match direction {
        Direction::North => active_display.bounds().min.y > other_display.bounds().min.y,
        Direction::South => active_display.bounds().min.y < other_display.bounds().min.y,
        _ => false,
    };
    debug!("moving focus to another display: {change_display}");
    if change_display {
        commands.trigger(SendMessageTrigger(Event::Command {
            command: Command::Mouse(MouseMove::ToNextDisplay),
        }));
    }
}

/// Handles the "swap" command, swapping the positions of the current window with another window in a specified direction.
///
/// # Arguments
///
/// * `direction` - The `Direction` to swap the window (e.g., `Direction::West`).
/// * `current` - The `Entity` of the currently focused `Window`.
/// * `active_display` - A mutable reference to the `ActiveDisplayMut` representing the active display.
/// * `windows` - A mutable query for all `Window` components.
/// * `commands` - Bevy commands to trigger events.
///
/// # Returns
///
/// `Some(Entity)` with the entity that was swapped with, otherwise `None`.
#[instrument(level = Level::DEBUG, skip_all)]
#[allow(clippy::needless_pass_by_value)]
fn command_swap_focus(
    mut messages: MessageReader<Event>,
    windows: Windows,
    apps: Query<&Application>,
    mut active_display: ActiveDisplayMut,
    mut commands: Commands,
) {
    let Some(Operation::Swap(direction)) =
        filter_window_operations(&mut messages, |op| matches!(op, Operation::Swap(_))).next()
    else {
        return;
    };

    // On a fullscreen space, swap just raises the last tiled window.
    if active_display.display().is_fullscreen {
        let strip = active_display.active_strip();
        if let Some(entity) = strip
            .get(strip.len().wrapping_sub(1))
            .ok()
            .and_then(|c| c.top())
            && let Some(window) = windows.get(entity)
            && let Some(psn) = windows.psn(window.id(), &apps)
        {
            debug!("fullscreen: swap raising {entity}");
            window.focus_with_raise(psn);
        }
        return;
    }

    let active_strip = active_display.active_strip();

    let mut handler = || {
        let (_, current) = windows.focused()?;
        let index = active_strip.index_of(current).ok()?;
        let other_window = get_window_in_direction(direction, current, active_strip)?;
        let new_index = active_strip.index_of(other_window).ok()?;
        debug!(
            "swap {direction:?}: current={current} idx={index}, other={other_window} idx={new_index}, strip_len={}",
            active_strip.len()
        );

        if index < new_index {
            (index..new_index).for_each(|idx| active_strip.swap(idx, idx + 1));
        } else {
            (new_index..index)
                .rev()
                .for_each(|idx| active_strip.swap(idx, idx + 1));
        }
        Some(current)
    };

    if let Some(window) = handler() {
        reshuffle_around(window, &mut commands);
    } else {
        debug!(
            "swap {direction:?}: handler returned None (focused={:?}, strip_len={})",
            windows.focused().map(|(_, e)| e),
            active_strip.len()
        );
    }

    if windows
        .focused()
        .and_then(|(_, current)| get_window_in_direction(direction, current, active_strip))
        .is_none()
    {
        // Check if the movement can swap to another display.
        let bounds = active_display.bounds();
        let Some(other_display) = active_display.other().next() else {
            return;
        };
        let change_display = match direction {
            Direction::North => bounds.min.y > other_display.bounds().min.y,
            Direction::South => bounds.min.y < other_display.bounds().min.y,
            _ => false,
        };
        debug!("swapping window to another display: {change_display}");
        if change_display {
            commands.trigger(SendMessageTrigger(Event::Command {
                command: Command::Window(Operation::ToNextDisplay),
            }));
        }
    }
}

/// Centers the focused window on the active display.
///
/// # Arguments
///
/// * `focused_entity` - The `Entity` of the currently focused window.
/// * `windows` - A mutable query for all `Window` components.
/// * `window_manager` - The `WindowManager` resource.
/// * `active_display` - The `ActiveDisplayMut` resource representing the active display.
/// * `commands` - Bevy commands to trigger events.
#[allow(clippy::needless_pass_by_value)]
fn command_center_window(
    mut messages: MessageReader<Event>,
    windows: Windows,
    active_display: ActiveDisplay,
    window_manager: Res<WindowManager>,
    mut commands: Commands,
) {
    if filter_window_operations(&mut messages, |op| matches!(op, Operation::Center))
        .next()
        .is_none()
    {
        return;
    }

    if let Some((_, entity)) = windows.focused()
        && let Some(layout_position) = windows.layout_position(entity)
        && let Some(size) = windows.size(entity)
        && let Some(mut origin) = windows.origin(entity)
    {
        let center = active_display.bounds().center().x;
        origin.x = center - size.x / 2;
        // Directly reposition the strip (bypasses hidden_ratio check).
        let strip_position = origin - layout_position.0;
        reposition_entity(
            active_display.active_strip_entity(),
            strip_position,
            &mut commands,
        );
        window_manager.warp_mouse(active_display.bounds().center());
    }
}

/// Resizes the focused window based on preset column widths.
///
/// # Arguments
///
/// * `active_display` - A mutable reference to the `Display` resource.
/// * `focused_entity` - The `Entity` of the currently focused window.
/// * `windows` - A mutable query for all `Window` components.
/// * `commands` - Bevy commands to trigger events.
/// * `config` - The `Config` resource.
#[allow(clippy::needless_pass_by_value)]
fn resize_window(
    mut messages: MessageReader<Event>,
    windows: Windows,
    active_display: ActiveDisplay,
    config: Res<Config>,
    mut commands: Commands,
) {
    let Some(Operation::Resize(direction)) =
        filter_window_operations(&mut messages, |op| matches!(op, Operation::Resize(_))).next()
    else {
        return;
    };

    let Some((frame, entity)) = windows
        .focused()
        .and_then(|(_, entity)| windows.frame(entity).zip(Some(entity)))
    else {
        return;
    };
    if let Ok(mut cmds) = commands.get_entity(entity) {
        cmds.try_remove::<FullWidthMarker>();
    }

    let display_width = active_display.bounds().width();
    let (_, pad_right, _, pad_left) = config.edge_padding();
    let padded_width = display_width - pad_left - pad_right;
    let current_ratio = f64::from(frame.width()) / f64::from(padded_width);
    let widths = config.preset_column_widths();
    let fallback = *widths.first().unwrap_or(&0.5);
    let next_ratio = match direction {
        ResizeDirection::Grow => widths
            .iter()
            .copied()
            .find(|&r| r > current_ratio + 0.05)
            .unwrap_or(fallback),
        ResizeDirection::Shrink => widths
            .iter()
            .rev()
            .copied()
            .find(|&r| r < current_ratio - 0.05)
            .unwrap_or_else(|| *widths.last().unwrap_or(&fallback)),
    };

    let new_width = (next_ratio * f64::from(padded_width)).round() as i32;
    let size = Size::new(new_width, frame.height());
    let mut frame = IRect::from_center_size(frame.center(), size);

    if frame.max.x > active_display.bounds().max.x - pad_right {
        frame.min.x = active_display.bounds().max.x - pad_right - size.x;
        reposition_entity(entity, frame.min, &mut commands);
    }

    // Resize all windows in the column so stacked siblings share the new width.
    let strip = active_display.active_strip();
    if let Some(Column::Stack(stack)) = strip
        .index_of(entity)
        .ok()
        .and_then(|idx| strip.get(idx).ok())
    {
        for sibling in stack.iter().flat_map(StackItem::all_windows) {
            if sibling != entity
                && let Some(size) = windows.size(sibling)
            {
                resize_entity(sibling, size.with_x(new_width), &mut commands);
            }
        }
    }

    resize_entity(entity, size, &mut commands);
}

/// Toggles the focused window between full-width and a preset width.
///
/// # Arguments
///
/// * `active_display` - A mutable reference to the `Display` resource.
/// * `focused_entity` - The `Entity` of the currently focused window.
/// * `windows` - A mutable query for all `Window` components.
/// * `commands` - Bevy commands to trigger events.
/// * `config` - The `Config` resource.
#[allow(clippy::needless_pass_by_value)]
fn full_width_window(
    mut messages: MessageReader<Event>,
    windows: Windows,
    mut active_display: ActiveDisplayMut,
    config: Res<Config>,
    mut commands: Commands,
) {
    if filter_window_operations(&mut messages, |op| matches!(op, Operation::FullWidth))
        .next()
        .is_none()
    {
        return;
    }

    let Some((frame, (_, entity))) = windows
        .focused()
        .and_then(|(window, entity)| windows.frame(entity).zip(Some((window, entity))))
    else {
        return;
    };

    let viewport = active_display
        .display()
        .actual_display_bounds(active_display.dock(), &config);
    let height = frame.height();
    let y = frame.min.y;

    let (width, x) = if let Some(marker) = windows.full_width(entity) {
        let previous_ratio = marker.width_ratio;
        let was_stacked = marker.was_stacked;
        commands.entity(entity).try_remove::<FullWidthMarker>();
        if was_stacked {
            _ = active_display.active_strip().stack(entity);
        }
        let w = (previous_ratio * f64::from(viewport.width())).round() as i32;
        (w, viewport.center().x - frame.width() / 2)
    } else {
        let strip = active_display.active_strip();
        let was_stacked = strip
            .index_of(entity)
            .ok()
            .and_then(|idx| strip.get(idx).ok())
            .is_some_and(|col| matches!(col, Column::Stack(_)));
        if was_stacked {
            _ = strip.unstack(entity);
        }
        let width_ratio = windows.width_ratio(entity).unwrap_or(0.5);
        commands.entity(entity).try_insert(FullWidthMarker {
            width_ratio,
            was_stacked,
        });
        // Logical frame spans the full viewport; reposition() and
        // resize() handle h_pad conversion to CG coordinates.
        (viewport.width(), viewport.min.x)
    };

    reposition_entity(entity, Origin::new(x, y), &mut commands);
    resize_entity(entity, Size::new(width, height), &mut commands);
}

/// Toggles the managed state of the focused window.
/// If the window is currently unmanaged, it becomes managed. If managed, it becomes unmanaged (floating).
///
/// # Arguments
///
/// * `focused_entity` - The `Entity` of the currently focused window.
/// * `windows` - A mutable query for `Window` components, their `Entity`, and whether they have the `Unmanaged` marker.
/// * `commands` - Bevy commands to modify entities.
#[allow(clippy::needless_pass_by_value)]
fn manage_window(mut messages: MessageReader<Event>, windows: Windows, mut commands: Commands) {
    if filter_window_operations(&mut messages, |op| matches!(op, Operation::Manage))
        .next()
        .is_none()
    {
        return;
    }

    let Some((window, entity, unmanaged)) = windows
        .focused()
        .and_then(|(_, entity)| windows.get_managed(entity))
    else {
        return;
    };
    debug!(
        "window: {} {entity} unmanaged: {}.",
        window.id(),
        unmanaged.is_some()
    );
    if unmanaged.is_some() {
        commands.entity(entity).try_remove::<Unmanaged>();
    } else {
        commands.entity(entity).try_insert(Unmanaged::Floating);
    }
}

/// Moves the focused window to the next available display.
/// The window will be repositioned to the center of the new display.
///
/// # Arguments
///
/// * `focused_entity` - The `Entity` of the currently focused window.
/// * `windows` - A mutable query for `Window` components, their `Entity`, and whether they have the `Unmanaged` marker.
/// * `active_display` - A mutable reference to the `ActiveDisplayMut` resource.
/// * `commands` - Bevy commands to modify entities and trigger events.
#[allow(clippy::needless_pass_by_value)]
fn to_next_display(
    mut messages: MessageReader<Event>,
    windows: Windows,
    mut active_display: ActiveDisplayMut,
    mut other_workspaces: Query<
        (&mut LayoutStrip, Has<SelectedVirtualMarker>),
        Without<ActiveWorkspaceMarker>,
    >,
    window_manager: Res<WindowManager>,
    mut commands: Commands,
) {
    if filter_window_operations(&mut messages, |op| matches!(op, Operation::ToNextDisplay))
        .next()
        .is_none()
    {
        return;
    }

    let Some((window, entity, unmanaged)) = windows
        .focused()
        .and_then(|(_, entity)| windows.get_managed(entity))
    else {
        return;
    };
    if unmanaged.is_some() {
        return;
    }

    let Some(other) = active_display.other().next() else {
        debug!("no other display to move window to.");
        return;
    };

    debug!(
        "moving window (id {}, {entity}) to display {}: {}.",
        window.id(),
        other.id(),
        other.width() / 2,
    );
    let center = other.bounds().center().x;
    let target_display_id = other.id();

    let Some(size) = windows.size(entity) else {
        return;
    };
    let dest = other.bounds().min.with_x(center - size.x / 2);
    reposition_entity(entity, dest, &mut commands);

    window_manager.warp_mouse(other.bounds().center());

    // Remove the window from the source strip.
    let source_neighbour = active_display.active_strip().right_neighbour(entity);
    active_display.active_strip().remove(entity);
    if let Some(neighbour) = source_neighbour {
        reshuffle_around(neighbour, &mut commands);
    }

    // Insert into the target display's selected strip.
    if let Ok(target_space_id) = window_manager.active_display_space(target_display_id) {
        if let Some((mut target_strip, _)) = other_workspaces
            .iter_mut()
            .find(|(strip, selected)| *selected && strip.id() == target_space_id)
        {
            target_strip.append(entity);
            reshuffle_around(entity, &mut commands);
        }
    }
}

/// Moves the mouse pointer to the next available display.
#[instrument(level = Level::DEBUG, skip_all)]
#[allow(clippy::needless_pass_by_value)]
fn mouse_to_next_display(
    mut messages: MessageReader<Event>,
    windows: Windows,
    layout_strips: Query<(&LayoutStrip, Entity)>,
    displays: Query<(&Display, Entity, Has<ActiveDisplayMarker>)>,
    window_manager: Res<WindowManager>,
    mut ffm_flag: ResMut<FocusFollowsMouse>,
    mut commands: Commands,
) {
    if !messages.read().any(|event| {
        matches!(
            event,
            Event::Command {
                command: Command::Mouse(MouseMove::ToNextDisplay),
            }
        )
    }) {
        return;
    }

    let Some((other, _, _)) = displays.iter().find(|(_, _, active)| !*active) else {
        debug!("no other display to move mouse to.");
        return;
    };
    let Some((other_strip, _)) = window_manager
        .active_display_space(other.id())
        .ok()
        .and_then(|id| layout_strips.iter().find(|(strip, _)| strip.id() == id))
    else {
        return;
    };

    let visible_width = |frame: IRect| other.bounds().intersect(frame).width();
    let Some(frame) = other_strip
        .all_windows()
        .iter()
        .filter_map(|entity| windows.frame(*entity))
        .max_by(|left, right| {
            if visible_width(*left) < visible_width(*right) {
                std::cmp::Ordering::Less
            } else {
                std::cmp::Ordering::Greater
            }
        })
    else {
        debug!("no suitable windows on the other display to move the mouse.");
        return;
    };

    let visible_frame = other.bounds().intersect(frame);
    debug!("warping mouse to {visible_frame:?}",);
    window_manager.warp_mouse(visible_frame.center());

    let point = origin_to(visible_frame.center());
    ffm_flag.as_mut().0 = None;
    commands.trigger(WMEventTrigger(Event::MouseMoved { point }));
}

/// Distributes heights equally among all windows in the currently focused stack.
#[allow(clippy::needless_pass_by_value)]
fn equalize_column(
    mut messages: MessageReader<Event>,
    current_focus: Single<(&Window, Entity), With<FocusedMarker>>,
    windows: Windows,
    active_display: ActiveDisplay,
    mut commands: Commands,
) {
    if filter_window_operations(&mut messages, |op| matches!(op, Operation::Equalize))
        .next()
        .is_none()
    {
        return;
    }

    let (_, entity) = *current_focus;
    let active_strip = active_display.active_strip();
    let Ok(column) = active_strip
        .index_of(entity)
        .and_then(|index| active_strip.get(index))
    else {
        return;
    };

    if let Column::Stack(stack) = column {
        #[allow(clippy::cast_precision_loss)]
        let equal_height = active_display.bounds().height() / i32::try_from(stack.len()).unwrap();

        for item in &stack {
            for entity in item.all_windows() {
                if let Some(size) = windows.size(entity) {
                    resize_entity(entity, size.with_y(equal_height), &mut commands);
                }
            }
        }
    }
}

/// Slides the strip so the focused window is fully visible, snapping to the
/// nearest edge: left-aligned when the window overflows left, right-aligned
/// when it overflows right. No resize — the window keeps its current size.
/// Bypasses the lazy-viewport check since the user explicitly asked to reveal.
#[allow(clippy::needless_pass_by_value)]
fn snap_window(
    mut messages: MessageReader<Event>,
    windows: Windows,
    active_display: ActiveDisplay,
    config: Res<Config>,
    mut commands: Commands,
) {
    if filter_window_operations(&mut messages, |op| matches!(op, Operation::Snap))
        .next()
        .is_none()
    {
        return;
    }

    let Some((_, entity)) = windows.focused() else {
        return;
    };
    let Some(layout_position) = windows.layout_position(entity) else {
        return;
    };
    let Some(mut frame) = windows.moving_frame(entity) else {
        return;
    };

    let display_bounds = active_display
        .display()
        .actual_display_bounds(active_display.dock(), &config);

    // Clamp the frame into the display and reposition the *strip* (not the
    // window) so the layout stays consistent.
    let size = frame.size();
    frame.min = frame
        .min
        .clamp(display_bounds.min, display_bounds.max - size);
    frame.max = frame.min + size;

    let strip_position = frame.min - layout_position.0;
    reposition_entity(
        active_display.active_strip_entity(),
        strip_position,
        &mut commands,
    );
}

#[instrument(level = Level::DEBUG, skip_all)]
#[allow(clippy::needless_pass_by_value)]
pub fn stack_windows_handler(
    mut messages: MessageReader<Event>,
    windows: Windows,
    mut active_display: ActiveDisplayMut,
) {
    let Some(Operation::Stack(stack)) =
        filter_window_operations(&mut messages, |op| matches!(op, Operation::Stack(_))).next()
    else {
        return;
    };

    if let Some((_, entity, unmanaged)) = windows
        .focused()
        .and_then(|(_, entity)| windows.get_managed(entity))
        && unmanaged.is_none()
    {
        let strip = active_display.active_strip();
        if *stack {
            _ = strip.stack(entity);
        } else {
            _ = strip.unstack(entity);
        }
    }
}

/// Dispatches a command based on the `CommandTrigger` event.
/// This function is a Bevy system that reacts to `CommandTrigger` events and executes the corresponding window manager command.
///
/// # Arguments
///
/// * `trigger` - The `On<CommandTrigger>` event trigger containing the command to process.
/// * `windows` - A query for `Window` components, their `Entity`, and whether they have the `Unmanaged` marker.
/// * `active_display` - A mutable reference to the `ActiveDisplayMut` resource.
/// * `window_manager` - The `WindowManager` resource for interacting with the window management logic.
/// * `commands` - Bevy commands to trigger events and modify entities.
/// * `config` - The `Config` resource, containing application settings.
#[instrument(level = Level::DEBUG, skip_all)]
#[allow(clippy::needless_pass_by_value)]
pub fn command_quit_handler(
    mut messages: MessageReader<Event>,
    window_manager: Res<WindowManager>,
) {
    if messages.read().any(|event| {
        matches!(
            event,
            Event::Command {
                command: Command::Quit
            }
        )
    }) {
        _ = window_manager.quit();
    }
}

#[instrument(level = Level::DEBUG, skip_all)]
#[allow(clippy::needless_pass_by_value)]
fn print_internal_state_handler(
    mut messages: MessageReader<Event>,
    focused: Query<(&Window, Entity), With<FocusedMarker>>,
    windows: Query<(&Window, Entity, &ChildOf, Option<&Unmanaged>)>,
    apps: Query<&Application>,
    workspaces: Query<(&LayoutStrip, Entity, &ChildOf)>,
    displays: Query<(&Display, Entity, Has<ActiveDisplayMarker>)>,
) {
    if !messages.read().any(|event| {
        matches!(
            event,
            Event::Command {
                command: Command::PrintState,
            }
        )
    }) {
        return;
    }

    let focused = focused.single().ok();
    let print_window = |(window, entity, child, unmanaged): (
        &Window,
        Entity,
        &ChildOf,
        Option<_>,
    )| {
        let bundle_id = apps
            .get(child.parent())
            .ok()
            .and_then(|app| app.bundle_id().map(str::to_owned))
            .unwrap_or_default();
        format!(
            "\tid: {}, {entity}, {}:{}, {}x{}{}{}, bundle: {}, role: {}, subrole: {}, title: '{:.70}'",
            window.id(),
            window.frame().min.x,
            window.frame().min.y,
            window.frame().width(),
            window.frame().height(),
            if focused.is_some_and(|(_, focus)| focus == entity) {
                ", focused"
            } else {
                ""
            },
            unmanaged.map(|m| format!(", {m:?}")).unwrap_or_default(),
            bundle_id,
            window.role().unwrap_or_default(),
            window.subrole().unwrap_or_default(),
            window.title().unwrap_or_default()
        )
    };

    let mut seen = EntityHashSet::new();

    for (display, display_entity, active) in displays {
        for (strip, _, _) in workspaces
            .iter()
            .filter(|(_, _, child)| child.parent() == display_entity)
        {
            let windows = strip
                .all_windows()
                .iter()
                .filter_map(|entity| windows.get(*entity).ok())
                .inspect(|(_, entity, _, _)| {
                    seen.insert(*entity);
                })
                .map(print_window)
                .collect::<Vec<_>>();

            let display_id = display.id();
            info!(
                "Display {display_id}{}, workspace id {}: {strip}:\n{}",
                if active { ", active" } else { "" },
                strip.id(),
                windows.join("\n")
            );
        }
    }

    let remaining = windows
        .iter()
        .filter(|entity| !seen.contains(&entity.1))
        .map(print_window)
        .collect::<Vec<_>>();
    info!("Remaining:\n{}", remaining.join("\n"));

    if let Some(pool) = bevy::tasks::ComputeTaskPool::try_get() {
        info!("Running with {} threads", pool.thread_num());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bevy::prelude::*;

    fn setup_world_with_layout() -> (World, LayoutStrip, Vec<Entity>) {
        let mut world = World::new();
        // e0, e1 are stacked, e2 is single, e3 is single
        let entities = world
            .spawn_batch(vec![(), (), (), ()])
            .collect::<Vec<Entity>>();

        let mut strip = LayoutStrip::default();
        strip.append(entities[0]); // This will become a stack
        strip.append(entities[1]);
        strip.append(entities[2]);
        strip.append(entities[3]);
        strip.stack(entities[1]).unwrap(); // Stack e1 onto e0

        (world, strip, entities)
    }

    #[test]
    fn test_get_window_in_direction_simple() {
        let (_world, strip, entities) = setup_world_with_layout();
        let e0 = entities[0];
        let e2 = entities[2];
        let e3 = entities[3];
        let east = Direction::East;
        let west = Direction::West;

        // From e2, east should be e3, west should be e0 (top of stack)
        assert_eq!(get_window_in_direction(&east, e2, &strip), Some(e3));
        assert_eq!(get_window_in_direction(&west, e2, &strip), Some(e0));

        // From e3, west is e2, east is None
        assert_eq!(get_window_in_direction(&west, e3, &strip), Some(e2));
        assert_eq!(get_window_in_direction(&east, e3, &strip), None);

        // From e0, east is e2, west is None
        assert_eq!(get_window_in_direction(&east, e0, &strip), Some(e2));
        assert_eq!(get_window_in_direction(&west, e0, &strip), None);
    }

    #[test]
    fn test_get_window_in_direction_stacked() {
        let (_world, strip, entities) = setup_world_with_layout();
        let e0 = entities[0];
        let e1 = entities[1];
        let north = Direction::North;
        let south = Direction::South;

        // From e0 (top of stack), south should be e1, north is None
        assert_eq!(get_window_in_direction(&south, e0, &strip), Some(e1));
        assert_eq!(get_window_in_direction(&north, e0, &strip), None);

        // From e1 (bottom of stack), north should be e0, south is None
        assert_eq!(get_window_in_direction(&north, e1, &strip), Some(e0));
        assert_eq!(get_window_in_direction(&south, e1, &strip), None);
    }

    #[test]
    fn test_get_window_in_direction_adjacent_stacks() {
        // Layout: [Stack(e0, e1), Stack(e2, e3)]
        let mut world = World::new();
        let entities = world
            .spawn_batch(vec![(), (), (), ()])
            .collect::<Vec<Entity>>();

        let mut strip = LayoutStrip::default();
        strip.append(entities[0]);
        strip.append(entities[1]);
        strip.append(entities[2]);
        strip.append(entities[3]);
        strip.stack(entities[1]).unwrap(); // Stack e1 onto e0: [Stack(e0, e1), e2, e3]
        strip.stack(entities[3]).unwrap(); // Stack e3 onto e2: [Stack(e0, e1), Stack(e2, e3)]

        let east = Direction::East;
        let west = Direction::West;

        // From e0 (top-left), east should go to e2 (top-right)
        assert_eq!(
            get_window_in_direction(&east, entities[0], &strip),
            Some(entities[2])
        );
        // From e1 (bottom-left), east should go to e3 (bottom-right)
        assert_eq!(
            get_window_in_direction(&east, entities[1], &strip),
            Some(entities[3])
        );
        // From e2 (top-right), west should go to e0 (top-left)
        assert_eq!(
            get_window_in_direction(&west, entities[2], &strip),
            Some(entities[0])
        );
        // From e3 (bottom-right), west should go to e1 (bottom-left)
        assert_eq!(
            get_window_in_direction(&west, entities[3], &strip),
            Some(entities[1])
        );
    }
}

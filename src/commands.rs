use bevy::ecs::entity::Entity;
use bevy::ecs::observer::On;
use bevy::ecs::system::{Commands, Res, ResMut};
use objc2_core_foundation::CGPoint;
use tracing::{Level, instrument};
use tracing::{debug, error};

use crate::config::Config;
use crate::ecs::params::{ActiveDisplayMut, Windows};
use crate::ecs::{
    CommandTrigger, FocusFollowsMouse, FullWidthMarker, Unmanaged, WMEventTrigger,
    reposition_entity, reshuffle_around, resize_entity,
};
use crate::errors::Result;
use crate::events::Event;
use crate::manager::{Column, LayoutStrip, WindowManager};

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

/// Defines the various operations that can be performed on windows.
#[derive(Clone, Debug)]
pub enum Operation {
    /// Focuses on a window in the specified `Direction`.
    Focus(Direction),
    /// Swaps the current window with another in the specified `Direction`.
    Swap(Direction),
    /// Centers the currently focused window on the display.
    Center,
    /// Resizes the focused window.
    Resize,
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
            Column::Single(window) => Some(window),
            Column::Stack(stack) => stack
                .iter()
                .enumerate()
                .find(|(_, window_id)| entity == **window_id)
                .and_then(|(index, _)| (index > 0).then(|| stack.get(index - 1)).flatten())
                .copied(),
        },

        Direction::South => match strip.get(index).ok()? {
            Column::Single(window) => Some(window),
            Column::Stack(stack) => stack
                .iter()
                .enumerate()
                .find(|(_, window_id)| entity == **window_id)
                .and_then(|(index, _)| {
                    (index < stack.len() - 1)
                        .then(|| stack.get(index + 1))
                        .flatten()
                })
                .copied(),
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
#[instrument(level = Level::DEBUG, ret, skip(windows))]
fn command_move_focus(
    direction: &Direction,
    strip: &LayoutStrip,
    windows: &Windows,
) -> Option<Entity> {
    let (_, entity) = windows.focused()?;
    get_window_in_direction(direction, entity, strip).inspect(|entity| {
        if let Some(window) = windows.get(*entity) {
            window.focus_with_raise();
        }
    })
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
#[instrument(level = Level::DEBUG, ret, skip_all, fields(direction))]
fn command_swap_focus(
    direction: &Direction,
    windows: &Windows,
    active_display: &mut ActiveDisplayMut,
    commands: &mut Commands,
) -> Option<Entity> {
    let display_bounds = active_display.bounds();
    let display_id = active_display.id();
    let active_strip = active_display.active_strip();

    let (_, current) = windows.focused()?;
    let index = active_strip.index_of(current).ok()?;
    let other_window = get_window_in_direction(direction, current, active_strip)?;
    let new_index = active_strip.index_of(other_window).ok()?;
    let current_frame = windows.get(current)?.frame();

    let origin = if new_index == 0 {
        // If reached far left, snap the window to left.
        CGPoint::new(0.0, 0.0)
    } else if new_index == (active_strip.len() - 1) {
        // If reached full right, snap the window to right.
        CGPoint::new(display_bounds.size.width - current_frame.size.width, 0.0)
    } else {
        active_strip
            .get(new_index)
            .ok()
            .and_then(|column| column.top())
            .and_then(|entity| windows.get(entity))?
            .frame()
            .origin
    };
    reposition_entity(current, origin.x, origin.y, display_id, commands);
    if index < new_index {
        (index..new_index).for_each(|idx| active_strip.swap(idx, idx + 1));
    } else {
        (new_index..index)
            .rev()
            .for_each(|idx| active_strip.swap(idx, idx + 1));
    }
    Some(other_window)
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
fn command_center_window(
    windows: &Windows,
    active_display: &ActiveDisplayMut,
    window_manager: &WindowManager,
    commands: &mut Commands,
) {
    let Some((window, entity)) = windows.focused() else {
        return;
    };
    let frame = window.frame();
    reposition_entity(
        entity,
        (active_display.bounds().size.width - frame.size.width) / 2.0,
        frame.origin.y,
        active_display.id(),
        commands,
    );
    window_manager.center_mouse(Some(window), &active_display.bounds());
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
fn resize_window(
    windows: &Windows,
    active_display: &mut ActiveDisplayMut,
    commands: &mut Commands,
    config: &Config,
) {
    let Some((window, entity)) = windows.focused() else {
        return;
    };
    let display_width = active_display.bounds().size.width;
    let current_ratio = window.frame().size.width / display_width;
    let next_ratio = config
        .preset_column_widths()
        .into_iter()
        .find(|&r| r > current_ratio + 0.05)
        .unwrap_or_else(|| *config.preset_column_widths().first().unwrap_or(&0.5));

    let width = next_ratio * display_width;
    let height = window.frame().size.height;
    let x = (display_width - width).min(window.frame().origin.x);
    let y = window.frame().origin.y;

    reposition_entity(entity, x, y, active_display.id(), commands);
    resize_entity(entity, width, height, active_display.id(), commands);
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
fn full_width_window(
    windows: &Windows,
    active_display: &mut ActiveDisplayMut,
    commands: &mut Commands,
) {
    let Some((window, entity, _, _)) = windows
        .focused()
        .and_then(|(_, entity)| windows.get_all(entity))
    else {
        return;
    };

    let display_width = active_display.bounds().size.width;
    let height = window.frame().size.height;
    let y = window.frame().origin.y;

    let (width, x) = if let Some(previous_ratio) = windows.full_width(entity) {
        commands.entity(entity).try_remove::<FullWidthMarker>();
        let w = previous_ratio * display_width;
        let x_pos = (display_width - w).min(window.frame().origin.x);
        (w, x_pos)
    } else {
        commands
            .entity(entity)
            .try_insert(FullWidthMarker(window.width_ratio()));
        (display_width, 0.0)
    };

    reposition_entity(entity, x, y, active_display.id(), commands);
    resize_entity(entity, width, height, active_display.id(), commands);
}

/// Toggles the managed state of the focused window.
/// If the window is currently unmanaged, it becomes managed. If managed, it becomes unmanaged (floating).
///
/// # Arguments
///
/// * `focused_entity` - The `Entity` of the currently focused window.
/// * `windows` - A mutable query for `Window` components, their `Entity`, and whether they have the `Unmanaged` marker.
/// * `commands` - Bevy commands to modify entities.
fn manage_window(windows: &Windows, commands: &mut Commands) {
    let Some((window, entity, _, unmanaged)) = windows
        .focused()
        .and_then(|(_, entity)| windows.get_all(entity))
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
fn to_next_display(
    windows: &Windows,
    active_display: &mut ActiveDisplayMut,
    window_manager: &WindowManager,
    commands: &mut Commands,
) {
    let Some((window, entity, _, unmanaged)) = windows
        .focused()
        .and_then(|(_, entity)| windows.get_all(entity))
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
        "moving window (id {}, {entity}) to display {}: {}:{}.",
        window.id(),
        other.id(),
        other.bounds.size.width / 2.0,
        other.menubar_height,
    );
    let dest = CGPoint::new(other.bounds.size.width / 2.0, other.menubar_height);
    reposition_entity(entity, dest.x, dest.y, other.id(), commands);
    reshuffle_around(entity, commands);

    window_manager.center_mouse(None, &other.bounds);
    active_display.active_strip().remove(entity);
}

/// Moves the mouse pointer to the next available display.
fn mouse_to_next_display(
    active_display: &mut ActiveDisplayMut,
    window_manager: &WindowManager,
    ffm_flag: &mut ResMut<FocusFollowsMouse>,
    commands: &mut Commands,
) {
    let Some(other) = active_display.other().next() else {
        debug!("no other display to move mouse to.");
        return;
    };

    let point = CGPoint::new(
        other.bounds.origin.x + other.bounds.size.width / 2.0,
        other.bounds.origin.y + other.bounds.size.height / 2.0,
    );
    window_manager.center_mouse(None, &other.bounds);
    ffm_flag.as_mut().0 = None;
    commands.trigger(WMEventTrigger(Event::MouseMoved { point }));
}

/// Distributes heights equally among all windows in the currently focused stack.
fn equalize_column(
    windows: &Windows,
    active_display: &mut ActiveDisplayMut,
    commands: &mut Commands,
) {
    let Some((_, entity)) = windows.focused() else {
        return;
    };
    let active_strip = active_display.active_strip();
    let Ok(column) = active_strip
        .index_of(entity)
        .and_then(|index| active_strip.get(index))
    else {
        return;
    };

    if let Column::Stack(stack) = column {
        let display_height =
            active_display.bounds().size.height - active_display.display().menubar_height;
        #[allow(clippy::cast_precision_loss)]
        let equal_height = (display_height / stack.len() as f64).floor();

        for &entity in &stack {
            if let Some(window) = windows.get(entity) {
                let width = window.frame().size.width;
                resize_entity(entity, width, equal_height, active_display.id(), commands);
            }
        }
    }
}

/// Handles various "window" commands, such as focus, swap, center, resize, manage, and stack.
///
/// # Arguments
///
/// * `operation` - The `Operation` to perform on the window.
/// * `window_manager` - The `WindowManager` resource.
/// * `active_display` - A mutable reference to the `ActiveDisplayMut` resource.
/// * `focused_entity` - The `Entity` of the focused window.
/// * `windows` - A mutable query for `Window` components, their `Entity`, and whether they have the `Unmanaged` marker.
/// * `commands` - Bevy commands to trigger events.
/// * `config` - The `Config` resource.
///
/// # Returns
///
/// `Ok(())` if the command is processed successfully, otherwise `Err(Error)`.
#[instrument(level = Level::DEBUG, skip_all, fields(operation), err)]
fn command_windows(
    operation: &Operation,
    windows: &Windows,
    active_display: &mut ActiveDisplayMut,
    window_manager: &WindowManager,
    commands: &mut Commands,
    config: &Config,
) -> Result<()> {
    match operation {
        Operation::Focus(direction) => {
            command_move_focus(direction, active_display.active_strip(), windows);
        }

        Operation::Swap(direction) => {
            command_swap_focus(direction, windows, active_display, commands);
        }

        Operation::Center => {
            command_center_window(windows, active_display, window_manager, commands);
        }

        Operation::Resize => {
            resize_window(windows, active_display, commands, config);
        }

        Operation::FullWidth => {
            full_width_window(windows, active_display, commands);
        }

        Operation::ToNextDisplay => {
            to_next_display(windows, active_display, window_manager, commands);
        }

        Operation::Equalize => {
            equalize_column(windows, active_display, commands);
        }

        Operation::Manage => {
            manage_window(windows, commands);
        }

        Operation::Stack(stack) => {
            if let Some((_, entity, _, unmanaged)) = windows
                .focused()
                .and_then(|(_, entity)| windows.get_all(entity))
            {
                if unmanaged.is_some() {
                    return Ok(());
                } else if *stack {
                    active_display.active_strip().stack(entity)?;
                } else {
                    active_display.active_strip().unstack(entity)?;
                }
            } else {
                return Ok(());
            }
        }
    }
    if let Some((_, entity)) = windows.focused() {
        reshuffle_around(entity, commands);
    }
    Ok(())
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
#[allow(clippy::needless_pass_by_value, clippy::too_many_arguments)]
#[instrument(level = Level::DEBUG, fields(trigger), skip_all)]
pub fn process_command_trigger(
    trigger: On<CommandTrigger>,
    windows: Windows,
    mut active_display: ActiveDisplayMut,
    window_manager: Res<WindowManager>,
    config: Res<Config>,
    mut ffm_flag: ResMut<FocusFollowsMouse>,
    mut commands: Commands,
) {
    let res = match &trigger.event().0 {
        Command::Window(operation) => {
            let eligible = windows
                .focused()
                .is_some_and(|(window, _)| window.is_eligible());
            if eligible {
                command_windows(
                    operation,
                    &windows,
                    &mut active_display,
                    &window_manager,
                    &mut commands,
                    config.as_ref(),
                )
            } else {
                Ok(())
            }
        }
        Command::Mouse(movement) => {
            match movement {
                MouseMove::ToNextDisplay => {
                    mouse_to_next_display(
                        &mut active_display,
                        &window_manager,
                        &mut ffm_flag,
                        &mut commands,
                    );
                }
            }
            Ok(())
        }
        Command::PrintState => {
            commands.trigger(WMEventTrigger(Event::PrintState));
            Ok(())
        }
        Command::Quit => window_manager.quit(),
    };
    if let Err(err) = res {
        error!("{err}");
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
}

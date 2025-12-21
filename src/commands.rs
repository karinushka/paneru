use bevy::ecs::entity::Entity;
use bevy::ecs::observer::On;
use bevy::ecs::query::{Has, With};
use bevy::ecs::system::{Commands, Query, Res, Single};
use log::{debug, error};
use objc2_core_foundation::{CGPoint, CGRect, CGSize};
use stdext::function_name;

use crate::config::{Config, preset_column_widths};
use crate::errors::Result;
use crate::events::{
    CommandTrigger, Event, FocusedMarker, RepositionMarker, ReshuffleAroundMarker, ResizeMarker,
    SenderSocket, Unmanaged, WMEventTrigger,
};
use crate::manager::WindowManager;
use crate::params::ActiveDisplayMut;
use crate::windows::{Display, Panel, Window, WindowPane};

#[derive(Clone, Debug)]
pub enum Direction {
    North,
    South,
    West,
    East,
    First,
    Last,
}

#[derive(Clone, Debug)]
pub enum Operation {
    Focus(Direction),
    Swap(Direction),
    Center,
    Resize,
    FullWidth,
    Manage,
    Stack(bool),
}

#[derive(Clone, Debug)]
pub enum Command {
    Window(Operation),
    Quit,
}

/// Retrieves a window `Entity` in a specified direction relative to a `current_window_id` within a `WindowPane`.
///
/// # Arguments
///
/// * `direction` - The direction (e.g., "west", "east", "first", "last").
/// * `current_window_id` - The `Entity` of the current window.
/// * `strip` - A reference to the `WindowPane` to search within.
///
/// # Returns
///
/// `Some(Entity)` with the found window's entity, otherwise `None`.
fn get_window_in_direction(
    direction: &Direction,
    current_window_id: Entity,
    strip: &WindowPane,
) -> Option<Entity> {
    let index = strip.index_of(current_window_id).ok()?;
    match direction {
        Direction::West => (index > 0)
            .then(|| strip.get(index - 1).ok())
            .flatten()
            .and_then(|panel| panel.top()),

        Direction::East => (index < strip.len() - 1)
            .then(|| strip.get(index + 1).ok())
            .flatten()
            .and_then(|panel| panel.top()),

        Direction::First => strip.first().ok().and_then(|panel| panel.top()),

        Direction::Last => strip.last().ok().and_then(|panel| panel.top()),

        Direction::North => match strip.get(index).ok()? {
            Panel::Single(window_id) => Some(window_id),
            Panel::Stack(stack) => stack
                .iter()
                .enumerate()
                .find(|(_, window_id)| current_window_id == **window_id)
                .and_then(|(index, _)| (index > 0).then(|| stack.get(index - 1)).flatten())
                .copied(),
        },

        Direction::South => match strip.get(index).ok()? {
            Panel::Single(window_id) => Some(window_id),
            Panel::Stack(stack) => stack
                .iter()
                .enumerate()
                .find(|(_, window_id)| current_window_id == **window_id)
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
/// * `argv` - A slice of strings representing the command arguments (e.g., [`east`]).
/// * `current_window` - The `Entity` of the currently focused `Window`.
/// * `strip` - A reference to the active `WindowPane`.
/// * `windows` - A query for all `Window` components.
///
/// # Returns
///
/// `Some(Entity)` with the entity of the newly focused window, otherwise `None`.
fn command_move_focus(
    direction: &Direction,
    current_window: Entity,
    strip: &WindowPane,
    windows: &Query<&Window>,
) -> Option<Entity> {
    get_window_in_direction(direction, current_window, strip).inspect(|entity| {
        if let Ok(window) = windows.get(*entity) {
            window.focus_with_raise();
        }
    })
}

/// Handles the "swap" command, swapping the positions of the current window with another window in a specified direction.
///
/// # Arguments
///
/// * `argv` - A slice of strings representing the command arguments (e.g., [`west`]).
/// * `current` - The `Entity` of the currently focused `Window`.
/// * `panel` - A reference to the active `WindowPane`.
/// * `display_bounds` - The `CGRect` representing the bounds of the display.
/// * `windows` - A mutable query for all `Window` components.
/// * `commands` - Bevy commands to trigger events.
///
/// # Returns
///
/// `Some(Entity)` with the entity that was swapped with, otherwise `None`.
fn command_swap_focus(
    direction: &Direction,
    current: Entity,
    panel: &mut WindowPane,
    display_bounds: &CGRect,
    windows: &mut Query<&mut Window>,
    commands: &mut Commands,
) -> Option<Entity> {
    let index = panel.index_of(current).ok()?;
    let other_window = get_window_in_direction(direction, current, panel)?;
    let new_index = panel.index_of(other_window).ok()?;
    let current_frame = windows.get(current).ok()?.frame();

    let origin = if new_index == 0 {
        // If reached far left, snap the window to left.
        CGPoint::new(0.0, 0.0)
    } else if new_index == (panel.len() - 1) {
        // If reached full right, snap the window to right.
        CGPoint::new(display_bounds.size.width - current_frame.size.width, 0.0)
    } else {
        panel
            .get(new_index)
            .ok()
            .and_then(|panel| panel.top())
            .and_then(|entity| windows.get(entity).ok())?
            .frame()
            .origin
    };
    commands.entity(current).insert(RepositionMarker {
        origin: CGPoint {
            x: origin.x,
            y: origin.y,
        },
    });
    if index < new_index {
        (index..new_index).for_each(|idx| panel.swap(idx, idx + 1));
    } else {
        (new_index..index)
            .rev()
            .for_each(|idx| panel.swap(idx, idx + 1));
    }
    Some(other_window)
}

/// Centers the focused window on the active display.
fn command_center_window(
    focused_entity: Entity,
    windows: &mut Query<&mut Window>,
    window_manager: &WindowManager,
    active_display: &ActiveDisplayMut,
    commands: &mut Commands,
) {
    let Ok(window) = windows.get_mut(focused_entity) else {
        return;
    };
    let frame = window.frame();
    commands.entity(focused_entity).insert(RepositionMarker {
        origin: CGPoint {
            x: (active_display.bounds().size.width - frame.size.width) / 2.0,
            y: frame.origin.y,
        },
    });
    window_manager.center_mouse(&window, &active_display.bounds());
}

/// Resizes the focused window based on preset column widths.
fn resize_window(
    active_display: &mut Display,
    focused_entity: Entity,
    windows: &mut Query<&mut Window>,
    commands: &mut Commands,
    config: &Config,
) {
    let Ok(window) = windows.get_mut(focused_entity) else {
        return;
    };
    let display_width = active_display.bounds.size.width;
    let width_ratios = preset_column_widths(config);
    let width_ratio = window.next_size_ratio(&width_ratios);

    let width = width_ratio * display_width;
    let height = window.frame().size.height;
    let x = (display_width - width).min(window.frame().origin.x);
    let y = window.frame().origin.y;

    commands.entity(focused_entity).insert(RepositionMarker {
        origin: CGPoint { x, y },
    });
    commands.entity(focused_entity).insert(ResizeMarker {
        size: CGSize { width, height },
    });
}

/// Toggles the focused window between full-width and a preset width.
fn full_width_window(
    active_display: &mut Display,
    focused_entity: Entity,
    windows: &mut Query<&mut Window>,
    commands: &mut Commands,
    config: &Config,
) {
    let Ok(mut window) = windows.get_mut(focused_entity) else {
        return;
    };

    let display_width = active_display.bounds.size.width;
    let height = window.frame().size.height;
    let y = window.frame().origin.y;

    let is_full_width = (window.frame().size.width - display_width).abs() < 1.0;

    let (width, width_ratio, x) = if is_full_width {
        let width_ratios = preset_column_widths(config);
        let ratio = *width_ratios.first().unwrap_or(&0.5);
        let w = ratio * display_width;
        let x_pos = (display_width - w).min(window.frame().origin.x);
        (w, ratio, x_pos)
    } else {
        (display_width, 1.0, 0.0)
    };

    commands.entity(focused_entity).insert(RepositionMarker {
        origin: CGPoint { x, y },
    });
    commands.entity(focused_entity).insert(ResizeMarker {
        size: CGSize { width, height },
    });

    window.width_ratio = width_ratio;
}

/// Toggles the managed state of the focused window.
fn manage_window(
    focused_entity: Entity,
    windows: &mut Query<(&mut Window, Entity, Has<Unmanaged>)>,
    commands: &mut Commands,
) {
    let Ok((window, entity, unmanaged)) = windows.get_mut(focused_entity) else {
        return;
    };
    debug!(
        "{}: window: {} {entity} unmanaged: {unmanaged}.",
        function_name!(),
        window.id()
    );
    if unmanaged {
        commands.entity(entity).remove::<Unmanaged>();
    } else {
        commands.entity(entity).insert(Unmanaged::Floating);
    }
}

/// Handles various "window" commands, such as focus, swap, center, resize, and manage.
///
/// # Arguments
///
/// * `argv` - A slice of strings representing the command arguments.
/// * `main_cid` - The main connection ID.
/// * `active_display` - The currently active display.
/// * `focused_entity` - The `Entity` of the focused window.
/// * `windows` - A mutable query for all `Window` components.
/// * `commands` - Bevy commands to trigger events.
/// * `config` - The optional configuration resource.
///
/// # Returns
///
/// `Ok(())` if the command is processed successfully, otherwise `Err(Error)`.
fn command_windows(
    operation: &Operation,
    window_manager: &WindowManager,
    active_display: &mut ActiveDisplayMut,
    focused_entity: Entity,
    windows: &mut Query<(&mut Window, Entity, Has<Unmanaged>)>,
    commands: &mut Commands,
    config: &Config,
) -> Result<()> {
    let bounds = active_display.bounds();
    let active_panel = active_display.active_panel()?;
    let managed = windows
        .get(focused_entity)
        .is_ok_and(|(_, _, unmanaged)| !unmanaged);

    if managed && active_panel.index_of(focused_entity).is_err() {
        // TODO: Workaround for mising workspace change notifications.
        commands.trigger(WMEventTrigger(Event::SpaceChanged));
        return Ok(());
    }
    let mut lens = windows.transmute_lens::<&mut Window>();

    match operation {
        Operation::Focus(direction) => {
            let mut lens = windows.transmute_lens::<&Window>();
            command_move_focus(direction, focused_entity, active_panel, &lens.query());
        }

        Operation::Swap(direction) => {
            command_swap_focus(
                direction,
                focused_entity,
                active_panel,
                &bounds,
                &mut lens.query(),
                commands,
            );
        }

        Operation::Center => {
            command_center_window(
                focused_entity,
                &mut lens.query(),
                window_manager,
                active_display,
                commands,
            );
        }

        Operation::Resize => {
            resize_window(
                active_display.display(),
                focused_entity,
                &mut lens.query(),
                commands,
                config,
            );
        }

        Operation::FullWidth => {
            full_width_window(
                active_display.display(),
                focused_entity,
                &mut lens.query(),
                commands,
                config,
            );
        }

        Operation::Manage => {
            manage_window(focused_entity, windows, commands);
        }

        Operation::Stack(stack) => {
            let (_, _, unmanaged) = windows.get(focused_entity)?;
            if unmanaged {
                return Ok(());
            } else if *stack {
                active_panel.stack(focused_entity)?;
            } else {
                active_panel.unstack(focused_entity)?;
            }
        }
    }
    if let Ok(mut cmd) = commands.get_entity(focused_entity) {
        cmd.try_insert(ReshuffleAroundMarker);
    }
    Ok(())
}

/// Dispatches a command based on the first argument (e.g., "window", "quit").
///
/// # Arguments
///
/// * `trigger` - The Bevy event trigger containing the command arguments.
/// * `sender` - The event sender socket.
/// * `main_cid` - The main connection ID resource.
/// * `windows` - A query for all windows, including the focused one.
/// * `display` - A query for the active display.
/// * `commands` - Bevy commands to trigger events.
/// * `config` - The optional configuration resource.
#[allow(clippy::needless_pass_by_value, clippy::too_many_arguments)]
pub fn process_command_trigger(
    trigger: On<CommandTrigger>,
    sender: Res<SenderSocket>,
    window_manager: Res<WindowManager>,
    current_focus: Single<Entity, With<FocusedMarker>>,
    mut windows: Query<(&mut Window, Entity, Has<Unmanaged>)>,
    mut active_display: ActiveDisplayMut,
    mut commands: Commands,
    config: Res<Config>,
) {
    let focused_entity = *current_focus;
    let eligible = windows
        .get(focused_entity)
        .is_ok_and(|(window, _, _)| window.is_eligible());

    let res = match &trigger.event().0 {
        Command::Window(operation) => {
            if eligible {
                let mut lens = windows.transmute_lens::<(&mut Window, Entity, Has<Unmanaged>)>();
                command_windows(
                    operation,
                    &window_manager,
                    &mut active_display,
                    focused_entity,
                    &mut lens.query(),
                    &mut commands,
                    config.as_ref(),
                )
            } else {
                Ok(())
            }
        }
        Command::Quit => sender.0.send(Event::Exit),
    };
    if let Err(err) = res {
        error!("{}: {err}", function_name!());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::windows::WindowPane;
    use bevy::prelude::*;

    fn setup_world_with_layout() -> (World, WindowPane, Vec<Entity>) {
        let mut world = World::new();
        // e0, e1 are stacked, e2 is single, e3 is single
        let entities = world
            .spawn_batch(vec![(), (), (), ()])
            .collect::<Vec<Entity>>();

        let mut pane = WindowPane::default();
        pane.append(entities[0]); // This will become a stack
        pane.append(entities[1]);
        pane.append(entities[2]);
        pane.append(entities[3]);
        pane.stack(entities[1]).unwrap(); // Stack e1 onto e0

        (world, pane, entities)
    }

    #[test]
    fn test_get_window_in_direction_simple() {
        let (_world, pane, entities) = setup_world_with_layout();
        let e0 = entities[0];
        let e2 = entities[2];
        let e3 = entities[3];
        let east = Direction::East;
        let west = Direction::West;

        // From e2, east should be e3, west should be e0 (top of stack)
        assert_eq!(get_window_in_direction(&east, e2, &pane), Some(e3));
        assert_eq!(get_window_in_direction(&west, e2, &pane), Some(e0));

        // From e3, west is e2, east is None
        assert_eq!(get_window_in_direction(&west, e3, &pane), Some(e2));
        assert_eq!(get_window_in_direction(&east, e3, &pane), None);

        // From e0, east is e2, west is None
        assert_eq!(get_window_in_direction(&east, e0, &pane), Some(e2));
        assert_eq!(get_window_in_direction(&west, e0, &pane), None);
    }

    #[test]
    fn test_get_window_in_direction_stacked() {
        let (_world, pane, entities) = setup_world_with_layout();
        let e0 = entities[0];
        let e1 = entities[1];
        let north = Direction::North;
        let south = Direction::South;

        // From e0 (top of stack), south should be e1, north is None
        assert_eq!(get_window_in_direction(&south, e0, &pane), Some(e1));
        assert_eq!(get_window_in_direction(&north, e0, &pane), None);

        // From e1 (bottom of stack), north should be e0, south is None
        assert_eq!(get_window_in_direction(&north, e1, &pane), Some(e0));
        assert_eq!(get_window_in_direction(&south, e1, &pane), None);
    }
}

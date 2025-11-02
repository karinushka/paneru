use bevy::ecs::entity::Entity;
use bevy::ecs::observer::On;
use bevy::ecs::query::With;
use bevy::ecs::system::{Commands, Query, Res};
use log::{error, warn};
use objc2_core_foundation::{CGPoint, CGRect};
use std::io::{ErrorKind, Result};
use stdext::function_name;

use crate::config::{Config, preset_column_widths};
use crate::events::{
    CommandTrigger, Event, FocusedMarker, MainConnection, RepositionMarker, ReshuffleAroundTrigger,
    SenderSocket, WMEventTrigger,
};
use crate::skylight::ConnID;
use crate::windows::{Display, Panel, Window, WindowPane};

/// Retrieves a window ID in a specified direction relative to a `current_window_id` within a `WindowPane`.
///
/// # Arguments
///
/// * `direction` - The direction (e.g., "west", "east", "first", "last").
/// * `current_window_id` - The ID of the current window.
/// * `strip` - A reference to the `WindowPane` to search within.
///
/// # Returns
///
/// `Some(WinID)` with the found window's ID, otherwise `None`.
fn get_window_in_direction(
    direction: &str,
    current_window_id: Entity,
    strip: &WindowPane,
) -> Option<Entity> {
    let index = strip.index_of(current_window_id).ok()?;
    match direction {
        "west" => (index > 0)
            .then(|| strip.get(index - 1).ok())
            .flatten()
            .and_then(|panel| panel.top()),
        "east" => (index < strip.len() - 1)
            .then(|| strip.get(index + 1).ok())
            .flatten()
            .and_then(|panel| panel.top()),
        "first" => strip.first().ok().and_then(|panel| panel.top()),
        "last" => strip.last().ok().and_then(|panel| panel.top()),
        "north" => match strip.get(index).ok()? {
            Panel::Single(window_id) => Some(window_id),
            Panel::Stack(stack) => stack
                .iter()
                .enumerate()
                .find(|(_, window_id)| current_window_id == **window_id)
                .and_then(|(index, _)| (index > 0).then(|| stack.get(index - 1)).flatten())
                .copied(),
        },
        "south" => match strip.get(index).ok()? {
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
        dir => {
            error!("{}: Unhandled direction {dir}", function_name!());
            None
        }
    }
}

/// Handles the "focus" command, moving focus to a window in a specified direction.
///
/// # Arguments
///
/// * `argv` - A slice of strings representing the command arguments (e.g., [`east`]).
/// * `current_window` - A reference to the currently focused `Window`.
/// * `strip` - A reference to the active `WindowPane`.
/// * `windows` - A query for all `Window` components.
///
/// # Returns
///
/// `Some(WinID)` with the ID of the newly focused window, otherwise `None`.
fn command_move_focus(
    argv: &[String],
    current_window: Entity,
    strip: &WindowPane,
    windows: &Query<&Window>,
) -> Option<Entity> {
    let direction = argv.first()?;

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
/// * `current_window` - A reference to the currently focused `Window`.
/// * `panel` - A reference to the active `WindowPane`.
/// * `display_bounds` - The `CGRect` representing the bounds of the display.
/// * `windows` - A mutable query for all `Window` components.
///
/// # Returns
///
/// `Some(Window)` with the window that was swapped with, otherwise `None`.
fn command_swap_focus(
    argv: &[String],
    current: Entity,
    panel: &mut WindowPane,
    display_bounds: &CGRect,
    windows: &mut Query<&mut Window>,
    commands: &mut Commands,
) -> Option<Entity> {
    let direction = argv.first()?;
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

/// Handles various "window" commands, such as focus, swap, center, resize, and manage.
///
/// # Arguments
///
/// * `argv` - A slice of strings representing the command arguments.
/// * `main_cid` - The main connection ID.
/// * `active_display` - The currently active display.
/// * `focused_window` - A query for the currently focused window.
/// * `windows` - A mutable query for all `Window` components.
/// * `commands` - Bevy commands to trigger events.
/// * `config` - The optional configuration resource.
///
/// # Returns
///
/// `Ok(())` if the command is processed successfully, otherwise `Err(Error)`.
#[allow(clippy::needless_pass_by_value)]
fn command_windows(
    argv: &[String],
    main_cid: ConnID,
    active_display: &mut Display,
    focused_entity: Entity,
    windows: &mut Query<&mut Window>,
    commands: &mut Commands,
    config: Option<Res<Config>>,
) -> Result<()> {
    if !windows.get(focused_entity).is_ok_and(Window::is_eligible) {
        return Ok(());
    }

    let empty = String::new();
    let bounds = active_display.bounds;
    let active_panel = active_display.active_panel(main_cid)?;
    let error_msg =
        |err| std::io::Error::new(ErrorKind::NotFound, format!("{}: {err}", function_name!()));

    match argv.first().unwrap_or(&empty).as_ref() {
        "focus" => {
            let mut lens = windows.transmute_lens::<&Window>();
            command_move_focus(&argv[1..], focused_entity, active_panel, &lens.query());
        }

        "swap" => {
            command_swap_focus(
                &argv[1..],
                focused_entity,
                active_panel,
                &bounds,
                windows,
                commands,
            );
        }

        "center" => {
            let window = windows.get_mut(focused_entity).map_err(error_msg)?;
            let frame = window.frame();
            commands.entity(focused_entity).insert(RepositionMarker {
                origin: CGPoint {
                    x: (active_display.bounds.size.width - frame.size.width) / 2.0,
                    y: frame.origin.y,
                },
            });
            window.center_mouse(main_cid);
        }

        "resize" => {
            let mut window = windows.get_mut(focused_entity).map_err(error_msg)?;
            let width_ratios = preset_column_widths(config.as_ref());
            let width_ratio = window.next_size_ratio(&width_ratios);
            let height = window.frame().size.height;
            window.resize(
                width_ratio * active_display.bounds.size.width,
                height,
                &active_display.bounds,
            );
        }

        "manage" => {
            let mut window = windows.get_mut(focused_entity).map_err(error_msg)?;
            if window.managed() {
                // Window already managed, remove it from the managed stack.
                active_panel.remove(focused_entity);
                window.manage(false);
            } else {
                // Add newly managed window to the stack.
                let frame = window.frame();
                commands.entity(focused_entity).insert(RepositionMarker {
                    origin: CGPoint {
                        x: frame.origin.x,
                        y: 0.0,
                    },
                });
                window.resize(frame.size.width, bounds.size.height, &bounds);
                active_panel.append(focused_entity);
                window.manage(true);
            }
        }

        "stack" => {
            let window = windows.get(focused_entity).map_err(error_msg)?;
            if !window.managed() {
                return Ok(());
            }
            active_panel.stack(focused_entity)?;
        }

        "unstack" => {
            let window = windows.get(focused_entity).map_err(error_msg)?;
            if !window.managed() {
                return Ok(());
            }
            active_panel.unstack(focused_entity)?;
        }

        _ => (),
    }
    let window = windows.get(focused_entity).map_err(error_msg)?;
    commands.trigger(ReshuffleAroundTrigger(window.id()));
    Ok(())
}

/// Dispatches a command based on the first argument (e.g., "window", "quit").
///
/// # Arguments
///
/// * `trigger` - The Bevy event trigger containing the command arguments.
/// * `sender` - The event sender socket.
/// * `main_cid` - The main connection ID resource.
/// * `windows` - A query for all windows.
/// * `focused_window` - A query for the focused window.
/// * `display` - A query for the active display.
/// * `commands` - Bevy commands to trigger events.
/// * `config` - The optional configuration resource.
#[allow(clippy::needless_pass_by_value)]
pub fn process_command_trigger(
    trigger: On<CommandTrigger>,
    sender: Res<SenderSocket>,
    main_cid: Res<MainConnection>,
    mut windows: Query<(&mut Window, Entity, Option<&FocusedMarker>)>,
    mut display: Query<&mut Display, With<FocusedMarker>>,
    mut commands: Commands,
    config: Option<Res<Config>>,
) {
    let Ok(mut active_display) = display.single_mut() else {
        warn!("{}: Unable to get current display.", function_name!());
        return;
    };
    let Some((focused_window, focused_entity, _)) =
        windows.iter().find(|(_, _, focus)| focus.is_some())
    else {
        warn!("{}: Unable to get focused window.", function_name!());
        return;
    };
    let main_cid = main_cid.0;
    if focused_window.managed()
        && active_display
            .active_panel(main_cid)
            .and_then(|panel| panel.index_of(focused_entity))
            .is_err()
    {
        commands.trigger(WMEventTrigger(Event::DisplayChanged));
    }

    let mut lens = windows.transmute_lens::<&mut Window>();
    let argv = &trigger.event().0;
    if let Some(first) = argv.first() {
        match first.as_ref() {
            "window" => {
                _ = command_windows(
                    &argv[1..],
                    main_cid,
                    &mut active_display,
                    focused_entity,
                    &mut lens.query(),
                    &mut commands,
                    config,
                )
                .inspect_err(|err| warn!("{}: {err}", function_name!()));
            }
            "quit" => sender.0.send(Event::Exit).unwrap(),
            _ => warn!("{}: Unhandled command: {argv:?}", function_name!()),
        }
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

        // From e2, east should be e3, west should be e0 (top of stack)
        assert_eq!(get_window_in_direction("east", e2, &pane), Some(e3));
        assert_eq!(get_window_in_direction("west", e2, &pane), Some(e0));

        // From e3, west is e2, east is None
        assert_eq!(get_window_in_direction("west", e3, &pane), Some(e2));
        assert_eq!(get_window_in_direction("east", e3, &pane), None);

        // From e0, east is e2, west is None
        assert_eq!(get_window_in_direction("east", e0, &pane), Some(e2));
        assert_eq!(get_window_in_direction("west", e0, &pane), None);
    }

    #[test]
    fn test_get_window_in_direction_stacked() {
        let (_world, pane, entities) = setup_world_with_layout();
        let e0 = entities[0];
        let e1 = entities[1];

        // From e0 (top of stack), south should be e1, north is None
        assert_eq!(get_window_in_direction("south", e0, &pane), Some(e1));
        assert_eq!(get_window_in_direction("north", e0, &pane), None);

        // From e1 (bottom of stack), north should be e0, south is None
        assert_eq!(get_window_in_direction("north", e1, &pane), Some(e0));
        assert_eq!(get_window_in_direction("south", e1, &pane), None);
    }
}

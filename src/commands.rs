use bevy::ecs::entity::Entity;
use bevy::ecs::observer::On;
use bevy::ecs::query::With;
use bevy::ecs::system::{Commands, Query, Res};
use log::{error, warn};
use objc2_core_foundation::{CGPoint, CGRect};
use std::io::Result;
use stdext::function_name;

use crate::config::{Config, preset_column_widths};
use crate::events::{
    CommandTrigger, Event, FocusedMarker, MainConnection, ReshuffleAroundTrigger, SenderSocket,
    WMEventTrigger,
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
/// * `find_window` - A closure to find a window by its ID.
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
/// * `find_window` - A closure to find a window by its ID.
///
/// # Returns
///
/// `Some(Window)` with the window that was swapped with, otherwise `None`.
fn command_swap_focus(
    argv: &[String],
    current: Entity,
    panel: &WindowPane,
    display_bounds: &CGRect,
    windows: &Query<&Window>,
) -> Option<Entity> {
    let direction = argv.first()?;
    let index = panel.index_of(current).ok()?;
    let other_window = get_window_in_direction(direction, current, panel)?;
    let new_index = panel.index_of(other_window).ok()?;
    let current_window = windows.get(current).ok()?;

    let origin = if new_index == 0 {
        // If reached far left, snap the window to left.
        CGPoint::new(0.0, 0.0)
    } else if new_index == (panel.len() - 1) {
        // If reached full right, snap the window to right.
        CGPoint::new(
            display_bounds.size.width - current_window.frame().size.width,
            0.0,
        )
    } else {
        panel
            .get(new_index)
            .ok()
            .and_then(|panel| panel.top())
            .and_then(|entity| windows.get(entity).ok())?
            .frame()
            .origin
    };
    current_window.reposition(origin.x, origin.y, display_bounds);
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
/// * `find_window` - A closure to find a window by its ID.
/// * `commands` - Bevy commands to trigger events.
/// * `config` - The optional configuration resource.
///
/// # Returns
///
/// `Ok(())` if the command is processed successfully, otherwise `Err(Error)`.
fn command_windows(
    argv: &[String],
    main_cid: ConnID,
    active_display: &Display,
    focused_window: &Query<(&Window, Entity), With<FocusedMarker>>,
    windows: &Query<&Window>,
    commands: &mut Commands,
    config: Option<Res<Config>>,
) -> Result<()> {
    let empty = String::new();
    let Ok((window, entity)) = focused_window.single() else {
        warn!("{}: No window focused.", function_name!());
        return Ok(());
    };
    if !window.is_eligible() {
        return Ok(());
    }

    let active_panel = active_display.active_panel(main_cid)?;

    match argv.first().unwrap_or(&empty).as_ref() {
        "focus" => {
            command_move_focus(&argv[1..], entity, &active_panel, windows);
        }

        "swap" => {
            command_swap_focus(
                &argv[1..],
                entity,
                &active_panel,
                &active_display.bounds,
                windows,
            );
        }

        "center" => {
            let frame = window.frame();
            window.reposition(
                (active_display.bounds.size.width - frame.size.width) / 2.0,
                frame.origin.y,
                &active_display.bounds,
            );
            window.center_mouse(main_cid);
        }

        "resize" => {
            let width_ratios = preset_column_widths(config.as_ref());
            let width_ratio = window.next_size_ratio(width_ratios);
            window.resize(
                width_ratio * active_display.bounds.size.width,
                window.frame().size.height,
                &active_display.bounds,
            );
        }

        "manage" => {
            if window.managed() {
                // Window already managed, remove it from the managed stack.
                active_panel.remove(entity);
                window.manage(false);
            } else {
                // Add newly managed window to the stack.
                let frame = window.frame();
                window.reposition(frame.origin.x, 0.0, &active_display.bounds);
                window.resize(
                    frame.size.width,
                    active_display.bounds.size.height,
                    &active_display.bounds,
                );
                active_panel.append(entity);
                window.manage(true);
            }
        }

        "stack" => {
            if !window.managed() {
                return Ok(());
            }
            active_panel.stack(entity)?;
        }

        "unstack" => {
            if !window.managed() {
                return Ok(());
            }
            active_panel.unstack(entity)?;
        }

        _ => (),
    }
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
    windows: Query<&Window>,
    focused_window: Query<(&Window, Entity), With<FocusedMarker>>,
    display: Query<&Display, With<FocusedMarker>>,
    mut commands: Commands,
    config: Option<Res<Config>>,
) {
    let Ok(active_display) = display.single() else {
        warn!("{}: Unable to get current display.", function_name!());
        return;
    };
    let Ok((active_window, entity)) = focused_window.single() else {
        warn!("{}: Unable to get focused window.", function_name!());
        return;
    };
    let main_cid = main_cid.0;
    if active_window.managed()
        && active_display
            .active_panel(main_cid)
            .and_then(|panel| panel.index_of(entity))
            .is_err()
    {
        commands.trigger(WMEventTrigger(Event::DisplayChanged));
    }

    let argv = &trigger.event().0;
    if let Some(first) = argv.first() {
        match first.as_ref() {
            "window" => {
                _ = command_windows(
                    &argv[1..],
                    main_cid,
                    active_display,
                    &focused_window,
                    &windows,
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

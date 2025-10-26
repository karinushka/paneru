use bevy::ecs::observer::On;
use bevy::ecs::query::With;
use bevy::ecs::system::{Query, Res};
use log::{error, warn};
use objc2_core_foundation::{CGPoint, CGRect};
use std::io::Result;
use stdext::function_name;

use crate::events::{
    CommandTrigger, Event, FocusedMarker, MainConnection, SenderSocket, WindowManagerResource,
};
use crate::manager::WindowManager;
use crate::skylight::{ConnID, WinID};
use crate::windows::{Display, Panel, Window, WindowPane};

/// Retrieves a window ID in a specified direction relative to a `current_window_id` within a `WindowPane`.
///
/// # Arguments
///
/// * `direction` - The direction (e.g., "west", "east", "first", "last").
/// * `current_window_id` - The ID of the current window.
/// * `panel` - A reference to the `WindowPane` to search within.
///
/// # Returns
///
/// `Some(WinID)` with the found window's ID, otherwise `None`.
fn get_window_in_direction(
    direction: &str,
    current_window_id: WinID,
    strip: &WindowPane,
) -> Option<WinID> {
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
/// * `panel` - A reference to the active `WindowPane`.
///
/// # Returns
///
/// `Some(WinID)` with the ID of the newly focused window, otherwise `None`.
fn command_move_focus<F: Fn(WinID) -> Option<Window>>(
    argv: &[String],
    current_window: &Window,
    strip: &WindowPane,
    find_window: &F,
) -> Option<WinID> {
    let direction = argv.first()?;

    get_window_in_direction(direction, current_window.id(), strip).inspect(|window_id| {
        let window = find_window(*window_id);
        if let Some(window) = window {
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
/// * `bounds` - The `CGRect` representing the bounds of the display.
///
/// # Returns
///
/// `Some(Window)` with the window that was swapped with, otherwise `None`.
fn command_swap_focus<F: Fn(WinID) -> Option<Window>>(
    argv: &[String],
    current_window: &Window,
    panel: &WindowPane,
    display_bounds: &CGRect,
    find_window: &F,
) -> Option<Window> {
    let direction = argv.first()?;
    let index = panel.index_of(current_window.id()).ok()?;
    let window =
        get_window_in_direction(direction, current_window.id(), panel).and_then(&find_window)?;
    let new_index = panel.index_of(window.id()).ok()?;

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
            .and_then(&find_window)?
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
    Some(window)
}

/// Handles various "window" commands, such as focus, swap, center, resize, and manage.
///
/// # Arguments
///
/// * `argv` - A slice of strings representing the command arguments.
///
/// # Returns
///
/// `Ok(())` if the command is processed successfully, otherwise `Err(Error)`.
fn command_windows<F: Fn(WinID) -> Option<Window>>(
    window_manager: &WindowManager,
    argv: &[String],
    main_cid: ConnID,
    active_display: &Display,
    find_window: &F,
) -> Result<()> {
    let empty = String::new();
    let Some(window) = window_manager
        .focused_window
        .and_then(&find_window)
        .filter(Window::is_eligible)
    else {
        warn!("{}: No window focused.", function_name!());
        return Ok(());
    };

    let active_panel = active_display.active_panel(main_cid)?;

    // FIXME:
    // let window_id = window.id();
    // if window.managed() && active_panel.index_of(window_id).is_err() {
    //     window_manager.reorient_focus()?;
    // }

    match argv.first().unwrap_or(&empty).as_ref() {
        "focus" => {
            command_move_focus(&argv[1..], &window, &active_panel, find_window);
        }

        "swap" => {
            command_swap_focus(
                &argv[1..],
                &window,
                &active_panel,
                &active_display.bounds,
                &find_window,
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
            let width_ratio = window.next_size_ratio();
            window.resize(
                width_ratio * active_display.bounds.size.width,
                window.frame().size.height,
                &active_display.bounds,
            );
        }

        "manage" => {
            if window.managed() {
                // Window already managed, remove it from the managed stack.
                active_panel.remove(window.id());
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
                active_panel.append(window.id());
                window.manage(true);
            }
        }

        "stack" => {
            if !window.managed() {
                return Ok(());
            }
            active_panel.stack(window.id())?;
        }

        "unstack" => {
            if !window.managed() {
                return Ok(());
            }
            active_panel.unstack(window.id())?;
        }

        _ => (),
    }
    window_manager.reshuffle_around(&window, active_display, find_window)
}

/// Dispatches a command based on the first argument (e.g., "window", "quit").
///
/// # Arguments
///
/// * `argv` - A vector of strings representing the command and its arguments.
#[allow(clippy::needless_pass_by_value)]
pub fn process_command_trigger(
    trigger: On<CommandTrigger>,
    window_manager: Res<WindowManagerResource>,
    sender: Res<SenderSocket>,
    main_cid: Res<MainConnection>,
    windows: Query<&Window>,
    displays: Query<&Display, With<FocusedMarker>>,
) {
    let Ok(active_display) = displays.single() else {
        warn!("{}: Unable to get current display.", function_name!());
        return;
    };
    let find_window = |window_id| {
        windows
            .iter()
            .find(|window| window.id() == window_id)
            .cloned()
    };
    let argv = &trigger.event().0;
    if let Some(first) = argv.first() {
        match first.as_ref() {
            "window" => {
                _ = command_windows(
                    &window_manager.0,
                    &argv[1..],
                    main_cid.0,
                    active_display,
                    &find_window,
                )
                .inspect_err(|err| warn!("{}: {err}", function_name!()));
            }
            "quit" => sender.0.send(Event::Exit).unwrap(),
            _ => warn!("{}: Unhandled command: {argv:?}", function_name!()),
        }
    }
}

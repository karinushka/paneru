use log::{error, warn};
use objc2_core_foundation::{CGPoint, CGRect};
use std::io::Result;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use stdext::function_name;

use crate::manager::WindowManager;
use crate::skylight::{ConnID, WinID};
use crate::windows::{Panel, Window, WindowPane};

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
fn command_move_focus(
    window_manager: &WindowManager,
    argv: &[String],
    current_window: &Window,
    strip: &WindowPane,
) -> Option<WinID> {
    let direction = argv.first()?;

    get_window_in_direction(direction, current_window.id(), strip).inspect(|window_id| {
        let window = window_manager.find_window(*window_id);
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
fn command_swap_focus(
    window_manager: &WindowManager,
    argv: &[String],
    current_window: &Window,
    panel: &WindowPane,
    display_bounds: &CGRect,
) -> Option<Window> {
    let direction = argv.first()?;
    let index = panel.index_of(current_window.id()).ok()?;
    let window = get_window_in_direction(direction, current_window.id(), panel)
        .and_then(|window_id| window_manager.find_window(window_id))?;
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
            .and_then(|window_id| window_manager.find_window(window_id))?
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
fn command_windows(
    window_manager: &WindowManager,
    argv: &[String],
    main_cid: ConnID,
) -> Result<()> {
    let empty = String::new();
    let Some(window) = window_manager
        .focused_window
        .and_then(|window_id| window_manager.find_window(window_id))
        .filter(Window::is_eligible)
    else {
        warn!("{}: No window focused.", function_name!());
        return Ok(());
    };

    let active_display = window_manager.active_display()?;
    let active_panel = active_display.active_panel(main_cid)?;
    let display_bounds = window_manager.current_display_bounds()?;

    let window_id = window.id();
    if window.managed() && active_panel.index_of(window_id).is_err() {
        window_manager.reorient_focus()?;
    }

    match argv.first().unwrap_or(&empty).as_ref() {
        "focus" => {
            command_move_focus(window_manager, &argv[1..], &window, &active_panel);
        }

        "swap" => {
            command_swap_focus(
                window_manager,
                &argv[1..],
                &window,
                &active_panel,
                &display_bounds,
            );
        }

        "center" => {
            let frame = window.frame();
            window.reposition(
                (display_bounds.size.width - frame.size.width) / 2.0,
                frame.origin.y,
                &display_bounds,
            );
            window.center_mouse(main_cid);
        }

        "resize" => {
            let width_ratio = window.next_size_ratio();
            window.resize(
                width_ratio * display_bounds.size.width,
                window.frame().size.height,
                &display_bounds,
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
                window.reposition(frame.origin.x, 0.0, &display_bounds);
                window.resize(
                    frame.size.width,
                    display_bounds.size.height,
                    &display_bounds,
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
    window_manager.reshuffle_around(&window)
}

/// Dispatches a command based on the first argument (e.g., "window", "quit").
///
/// # Arguments
///
/// * `argv` - A vector of strings representing the command and its arguments.
pub fn process_command(
    window_manager: &WindowManager,
    argv: &[String],
    quit: &Arc<AtomicBool>,
    main_cid: ConnID,
) {
    if let Some(first) = argv.first() {
        match first.as_ref() {
            "window" => {
                _ = command_windows(window_manager, &argv[1..], main_cid)
                    .inspect_err(|err| warn!("{}: {err}", function_name!()));
            }
            "quit" => quit.store(true, std::sync::atomic::Ordering::Relaxed),
            _ => warn!("{}: Unhandled command: {argv:?}", function_name!()),
        }
    }
}

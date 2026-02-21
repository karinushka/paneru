use bevy::app::PreUpdate;
use bevy::ecs::entity::{Entity, EntityHashSet};
use bevy::ecs::hierarchy::ChildOf;
use bevy::ecs::message::MessageReader;
use bevy::ecs::query::{Has, With};
use bevy::ecs::system::{Commands, Query, Res, ResMut, Single};
use objc2_core_foundation::{CGPoint, CGRect, CGSize};
use tracing::debug;
use tracing::{Level, instrument};

use crate::config::Config;
use crate::ecs::params::{ActiveDisplay, ActiveDisplayMut, Windows};
use crate::ecs::{
    ActiveDisplayMarker, FocusFollowsMouse, FocusedMarker, FullWidthMarker, Unmanaged,
    WMEventTrigger, reposition_entity, reshuffle_around, resize_entity,
};
use crate::events::Event;
use crate::manager::{Application, Column, Display, LayoutStrip, Window, WindowManager};

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
#[instrument(level = Level::DEBUG, skip_all)]
#[allow(clippy::needless_pass_by_value)]
fn command_move_focus(
    mut messages: MessageReader<Event>,
    windows: Windows,
    active_display: ActiveDisplay,
    apps: Query<&Application>,
    mut commands: Commands,
) {
    let Some(Operation::Focus(direction)) =
        filter_window_operations(&mut messages, |op| matches!(op, Operation::Focus(_))).next()
    else {
        return;
    };

    let (_, entity) = windows.focused().unwrap();
    if let Some(window) = get_window_in_direction(direction, entity, active_display.active_strip())
        .inspect(|entity| {
            if let Some(window) = windows.get(*entity)
                && let Some(psn) = windows.psn(window.id(), &apps)
            {
                window.focus_with_raise(psn);
            }
        })
    {
        reshuffle_around(window, &mut commands);
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
    mut active_display: ActiveDisplayMut,
    mut commands: Commands,
) {
    let Some(Operation::Swap(direction)) =
        filter_window_operations(&mut messages, |op| matches!(op, Operation::Swap(_))).next()
    else {
        return;
    };

    let display_bounds = active_display.bounds();
    let display_id = active_display.id();
    let active_strip = active_display.active_strip();

    let mut handler = || {
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
        reposition_entity(current, origin.x, origin.y, display_id, &mut commands);
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
    current_focus: Single<(&Window, Entity), With<FocusedMarker>>,
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

    let (window, entity) = *current_focus;
    let frame = window.frame();
    reposition_entity(
        entity,
        (active_display.bounds().size.width - frame.size.width) / 2.0,
        frame.origin.y,
        active_display.id(),
        &mut commands,
    );
    window_manager.center_mouse(Some(window), &active_display.bounds());
    reshuffle_around(entity, &mut commands);
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
    current_focus: Single<(&Window, Entity), With<FocusedMarker>>,
    active_display: ActiveDisplay,
    config: Res<Config>,
    mut commands: Commands,
) {
    if filter_window_operations(&mut messages, |op| matches!(op, Operation::Resize))
        .next()
        .is_none()
    {
        return;
    }

    let (window, entity) = *current_focus;
    let display_width = active_display.bounds().size.width;
    let current_ratio = window.frame().size.width / display_width;
    let next_ratio = config
        .preset_column_widths()
        .into_iter()
        .find(|&r| r > current_ratio + 0.05)
        .unwrap_or_else(|| *config.preset_column_widths().first().unwrap_or(&0.5));

    let width = next_ratio * display_width;
    let height = window.frame().size.height;
    let x = (display_width - width - 1.0).min(window.frame().origin.x);
    let y = window.frame().origin.y;

    reposition_entity(entity, x, y, active_display.id(), &mut commands);
    resize_entity(entity, width, height, active_display.id(), &mut commands);
    reshuffle_around(entity, &mut commands);
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
    current_focus: Single<(&Window, Entity), With<FocusedMarker>>,
    windows: Windows,
    active_display: ActiveDisplay,
    mut commands: Commands,
) {
    if filter_window_operations(&mut messages, |op| matches!(op, Operation::FullWidth))
        .next()
        .is_none()
    {
        return;
    }

    let (window, entity) = *current_focus;
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
        (display_width - 1.0, 0.0)
    };

    reposition_entity(entity, x, y, active_display.id(), &mut commands);
    resize_entity(entity, width, height, active_display.id(), &mut commands);
    reshuffle_around(entity, &mut commands);
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
    reshuffle_around(entity, &mut commands);
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
        "moving window (id {}, {entity}) to display {}: {}:{}.",
        window.id(),
        other.id(),
        other.bounds.size.width / 2.0,
        other.menubar_height,
    );
    let dest = CGPoint::new(other.bounds.size.width / 2.0, other.menubar_height);
    reposition_entity(entity, dest.x, dest.y, other.id(), &mut commands);
    reshuffle_around(entity, &mut commands);

    window_manager.center_mouse(None, &other.bounds);

    if let Some(neighbour) = active_display.active_strip().right_neighbour(entity) {
        reshuffle_around(neighbour, &mut commands);
    }
    active_display.active_strip().remove(entity);
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

    let visible_width = |window: &Window| {
        window.frame().origin.x.min(0.0) + window.frame().size.width
            - (window.frame().origin.x + window.frame().size.width - other.bounds.size.width)
                .max(0.0)
    };
    let Some(window) = other_strip
        .all_windows()
        .iter()
        .filter_map(|entity| windows.get(*entity))
        .max_by(|left, right| {
            if visible_width(left) < visible_width(right) {
                std::cmp::Ordering::Less
            } else {
                std::cmp::Ordering::Greater
            }
        })
    else {
        debug!("no suitable windows on the other display to move the mouse.");
        return;
    };

    let visible_frame = CGRect::new(
        CGPoint::new(
            other.bounds.origin.x + window.frame().origin.x.max(0.0),
            other.bounds.origin.y + window.frame().origin.y,
        ),
        CGSize::new(visible_width(window), window.frame().size.height),
    );
    debug!("warping mouse to {visible_frame:?}",);
    window_manager.center_mouse(None, &visible_frame);

    let point = CGPoint::new(
        visible_frame.origin.x + visible_width(window) / 2.0,
        visible_frame.origin.y + window.frame().size.height / 2.0,
    );
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
        let display_height =
            active_display.bounds().size.height - active_display.display().menubar_height;
        #[allow(clippy::cast_precision_loss)]
        let equal_height = (display_height / stack.len() as f64).floor();

        for &entity in &stack {
            if let Some(window) = windows.get(entity) {
                let width = window.frame().size.width;
                resize_entity(
                    entity,
                    width,
                    equal_height,
                    active_display.id(),
                    &mut commands,
                );
            }
        }
    }
    reshuffle_around(entity, &mut commands);
}

#[instrument(level = Level::DEBUG, skip_all)]
#[allow(clippy::needless_pass_by_value)]
pub fn stack_windows_handler(
    mut messages: MessageReader<Event>,
    windows: Windows,
    mut active_display: ActiveDisplayMut,
    mut commands: Commands,
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
        reshuffle_around(entity, &mut commands);
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
    let print_window = |(window, entity, _, unmanaged): (&Window, Entity, &ChildOf, Option<_>)| {
        format!(
            "\tid: {}, {entity}, {:.0}:{:.0}, {:.0}x{:.0}{}{}, role: {}, subrole: {}, title: '{:.70}'",
            window.id(),
            window.frame().origin.x,
            window.frame().origin.y,
            window.frame().size.width,
            window.frame().size.height,
            if focused.is_some_and(|(_, focus)| focus == entity) {
                ", focused"
            } else {
                ""
            },
            unmanaged.map(|m| format!(", {m:?}")).unwrap_or_default(),
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
            debug!(
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
    debug!("Remaining:\n{}", remaining.join("\n"));

    if let Some(pool) = bevy::tasks::ComputeTaskPool::try_get() {
        debug!("Running with {} threads", pool.thread_num());
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

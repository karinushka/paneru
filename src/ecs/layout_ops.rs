//! Replays a script's [`LayoutOp`]s (returned by a Lua handler that
//! transformed a `WindowSet`) against the live world.
//!
//! Unlike other command handlers, each op names a window explicitly rather
//! than acting on the focused one. Because the snapshot a script transformed
//! may be a frame stale, each op is applied independently and best-effort: one
//! that can't be resolved is logged at debug and skipped without affecting
//! the rest.

use crate::types::windowset::LayoutOp;
use bevy::ecs::entity::Entity;
use bevy::ecs::hierarchy::ChildOf;
use bevy::ecs::message::MessageReader;
use bevy::ecs::query::{Has, Without};
use bevy::ecs::system::{Commands, Query, Res, SystemParam};
use bevy::platform::collections::HashSet;
use tracing::debug;

use crate::commands::{Command, MoveFocus, Operation};
use crate::config::Config;
use crate::ecs::drag::ColumnInTransit;
use crate::ecs::layout::{Column, DisplayViewports, LayoutStrip};
use crate::ecs::params::Windows;
use crate::ecs::workspace::VirtualMoveMarker;
use crate::ecs::{
    ActiveWorkspaceMarker, FullWidthMarker, SendMessageTrigger, SpawnCommandsExt, Unmanaged, Window,
};
use crate::events::Event;
use crate::manager::{Origin, Size, WindowManager};
use crate::platform::WorkspaceId;
use crate::util::round_px;

/// Every strip, whether it is the one on screen, and the display it is on.
type Strips<'w, 's> = Query<
    'w,
    's,
    (
        &'static mut LayoutStrip,
        Has<ActiveWorkspaceMarker>,
        Option<&'static ChildOf>,
        Entity,
    ),
    Without<Window>,
>;

#[derive(SystemParam)]
pub(crate) struct LayoutDisplays<'w, 's> {
    viewports: DisplayViewports<'w, 's>,
    window_manager: Res<'w, WindowManager>,
}

/// Applies the layout operations a Lua handler returned.
pub(crate) fn apply_layout_ops(
    mut messages: MessageReader<Event>,
    windows: Windows,
    mut workspaces: Strips,
    displays: LayoutDisplays,
    config: Res<Config>,
    mut commands: Commands,
) {
    let batches: Vec<Vec<LayoutOp>> = messages
        .read()
        .filter_map(|message| match message {
            Event::Command {
                command: Command::Layout(ops),
            } => Some(ops.clone()),
            _ => None,
        })
        .collect();

    for ops in batches {
        // `Unmanaged` inserts don't take effect until commands flush, so track
        // what this batch floated to tell a just-floated window from one still
        // genuinely tiled (needed by `SetFrame`).
        let mut floated: HashSet<Entity> = HashSet::new();
        for op in ops {
            apply(
                op,
                &windows,
                &mut workspaces,
                &displays,
                &config,
                &mut floated,
                &mut commands,
            );
        }
    }
}

/// Applies one op, or explains why it could not be.
#[allow(clippy::too_many_lines)]
fn apply(
    op: LayoutOp,
    windows: &Windows,
    workspaces: &mut Strips,
    displays: &LayoutDisplays,
    config: &Config,
    floated: &mut HashSet<Entity>,
    commands: &mut Commands,
) {
    // Resolve up front so every arm below can assume the window still exists.
    let entity = if let Some(window_id) = op.target() {
        let Some((_, entity)) = windows.find(window_id) else {
            debug!(
                target: "paneru::lua",
                "skipping {op:?}: window {window_id} is gone since the handler ran"
            );
            return;
        };
        Some(entity)
    } else {
        None
    };

    match op {
        LayoutOp::Focus(_) => {
            let entity = entity.expect("Focus names a window");
            commands.focus_entity(entity, true);
        }

        LayoutOp::Swap(_, other) => {
            let entity = entity.expect("Swap names a window");
            let Some((_, other_entity)) = windows.find(other) else {
                debug!(target: "paneru::lua", "skipping {op:?}: window {other} is gone");
                return;
            };
            let Some((mut strip, _, _, _)) = workspaces
                .iter_mut()
                .find(|(strip, _, _, _)| strip.contains(entity) && strip.contains(other_entity))
            else {
                debug!(target: "paneru::lua", "skipping {op:?}: the two windows share no strip");
                return;
            };
            let (Ok(left), Ok(right)) = (strip.index_of(entity), strip.index_of(other_entity))
            else {
                return;
            };
            strip.swap(left, right);
            commands.reshuffle_around(entity);
        }

        LayoutOp::MoveToWorkspace {
            workspace, follow, ..
        } => {
            let entity = entity.expect("MoveToWorkspace names a window");
            // Virtual workspaces are numbered from one for a script and from
            // zero inside the layout.
            let Some(target_virtual_index) = workspace.checked_sub(1) else {
                debug!(target: "paneru::lua", "skipping {op:?}: workspaces are numbered from 1");
                return;
            };
            if let Ok(mut entity_commands) = commands.get_entity(entity) {
                entity_commands.try_insert(VirtualMoveMarker {
                    target_virtual_index,
                    move_focus: if follow {
                        MoveFocus::Follow
                    } else {
                        MoveFocus::Stay
                    },
                });
            }
        }

        LayoutOp::Place {
            space,
            workspace,
            index,
            ..
        } => {
            let entity = entity.expect("Place names a window");
            if !floated.contains(&entity) {
                place_column(
                    entity,
                    (space, workspace, index),
                    windows,
                    workspaces,
                    displays,
                    config,
                    commands,
                );
            }
        }

        LayoutOp::View { workspace } => {
            let Some(index) = workspace.checked_sub(1) else {
                debug!(target: "paneru::lua", "skipping {op:?}: workspaces are numbered from 1");
                return;
            };
            commands.trigger(SendMessageTrigger(Event::Command {
                command: Command::Window(Operation::VirtualNumber(index)),
            }));
        }

        LayoutOp::SetFloating { floating, .. } => {
            let entity = entity.expect("SetFloating names a window");
            if floating {
                floated.insert(entity);
            } else {
                floated.remove(&entity);
            }
            set_floating(entity, floating, workspaces, commands);
        }

        LayoutOp::SetManaged { managed, .. } => {
            let entity = entity.expect("SetManaged names a window");
            if managed {
                floated.remove(&entity);
            } else {
                floated.insert(entity);
            }
            set_floating(entity, !managed, workspaces, commands);
        }

        LayoutOp::SetWidth { ratio, .. } => {
            let entity = entity.expect("SetWidth names a window");
            if !ratio.is_finite() || ratio <= 0.0 {
                debug!(target: "paneru::lua", "skipping {op:?}: a width ratio must be positive");
                return;
            }
            // The interactive resize path (`resize_window`, used by the menubar
            // width picker) does the real work for the focused window: pixel
            // size, viewport clamping, stacked siblings, reshuffle.
            if windows
                .focused()
                .is_some_and(|(_, focused)| focused == entity)
            {
                commands.trigger(SendMessageTrigger(Event::Command {
                    command: Command::Window(Operation::SetWidth(ratio)),
                }));
                return;
            }
            // Any other window is resized here, against the usable width of
            // the display its strip is on. Storing a `WidthRatio` would not
            // do: nothing reacts to it, and the next bounds sync recomputes it
            // from the live frame.
            let Some((strip, _, parent, _)) = workspaces
                .iter()
                .find(|(strip, _, _, _)| strip.contains(entity))
            else {
                debug!(target: "paneru::lua", "skipping {op:?}: the window is in no strip");
                return;
            };
            let Some(viewport) = parent
                .and_then(|parent| displays.viewports.get(parent.parent()).ok())
                .map(|(display, dock)| display.actual_display_bounds(dock, config))
            else {
                debug!(target: "paneru::lua", "skipping {op:?}: the strip is on no display");
                return;
            };
            let width = round_px(ratio * f64::from(viewport.width()));
            // Stacked siblings share a column, and so its width.
            let members: Vec<Entity> = strip
                .index_of(entity)
                .ok()
                .and_then(|index| strip.get(index).ok())
                .map_or_else(|| vec![entity], |column| column.window_iter().collect());
            for member in members {
                if let Some(size) = windows.size(member) {
                    commands.resize_entity(member, size.with_x(width));
                }
            }
            if windows.full_width(entity).is_some()
                && let Ok(mut entity_commands) = commands.get_entity(entity)
            {
                entity_commands.try_remove::<FullWidthMarker>();
            }
            commands.reshuffle_around(entity);
        }

        LayoutOp::SetFrame { frame, .. } => {
            let entity = entity.expect("SetFrame names a window");
            // The layout engine owns a tiled window's geometry and will move
            // it back, so warn if the target isn't floated.
            if !floated.contains(&entity)
                && windows
                    .get_managed(entity)
                    .is_some_and(|(_, _, unmanaged)| unmanaged.is_none())
            {
                debug!(
                    target: "paneru::lua",
                    "{op:?} targets a tiled window; float it first or the layout will move it back"
                );
            }
            let origin = Origin::new(frame.x, frame.y);
            let size = Size::new(frame.width.max(1), frame.height.max(1));
            commands.reposition_entity(entity, origin);
            commands.resize_entity(entity, size);
        }

        LayoutOp::Stack { onto, tabs, .. } => {
            let entity = entity.expect("Stack names a window");
            let Some((_, onto_entity)) = windows.find(onto) else {
                debug!(target: "paneru::lua", "skipping {op:?}: window {onto} is gone");
                return;
            };
            let Some((mut strip, _, _, _)) = workspaces
                .iter_mut()
                .find(|(strip, _, _, _)| strip.contains(entity) && strip.contains(onto_entity))
            else {
                debug!(target: "paneru::lua", "skipping {op:?}: the two windows share no strip");
                return;
            };
            let (Ok(entity_index), Ok(onto_index)) =
                (strip.index_of(entity), strip.index_of(onto_entity))
            else {
                return;
            };
            if [entity_index, onto_index]
                .iter()
                .any(|&at| matches!(strip.get(at), Ok(Column::Fullscren(_))))
            {
                return;
            }
            if entity_index == onto_index {
                if tabs != strip.is_tabbed_display(entity) {
                    let active = windows
                        .focused()
                        .map(|(_, focused)| focused)
                        .filter(|&focused| strip.index_of(focused).ok() == Some(entity_index))
                        .unwrap_or(onto_entity);
                    strip.toggle_tabbed_display(active);
                    commands.reshuffle_around(active);
                }
                return;
            }
            // `LayoutStrip::stack` joins a column onto its left neighbour, so
            // first bring the window, alone, to just right of `onto`'s column:
            // the window set told the script it would land in `onto`'s column,
            // not in whichever one happened to sit to its left.
            strip.remove(entity);
            let Ok(index) = strip.index_of(onto_entity) else {
                return;
            };
            strip.insert_at(index + 1, entity);
            if strip.stack(entity).is_err() {
                debug!(target: "paneru::lua", "skipping {op:?}: the layout refused the stack");
                return;
            }
            if tabs != strip.is_tabbed_display(entity) {
                let active = windows
                    .focused()
                    .map(|(_, focused)| focused)
                    .filter(|&focused| strip.index_of(focused).ok() == Some(index))
                    .unwrap_or(onto_entity);
                strip.toggle_tabbed_display(active);
            }
            // As `window stack` does: a full-width window would keep its width
            // in a column it now shares.
            if windows.full_width(entity).is_some()
                && let Ok(mut entity_commands) = commands.get_entity(entity)
            {
                entity_commands.try_remove::<FullWidthMarker>();
            }
            commands.reshuffle_around(entity);
        }

        LayoutOp::Unstack(_) => {
            let entity = entity.expect("Unstack names a window");
            let Some((mut strip, _, _, _)) = workspaces
                .iter_mut()
                .find(|(strip, _, _, _)| strip.contains(entity))
            else {
                return;
            };
            if strip.unstack(entity).is_err() {
                debug!(target: "paneru::lua", "skipping {op:?}: the window is not in a stack");
                return;
            }
            commands.reshuffle_around(entity);
        }
    }
}

/// Place a whole tiled column without switching displays or workspaces.
fn place_column(
    entity: Entity,
    (space, workspace, index): (WorkspaceId, u32, usize),
    windows: &Windows,
    workspaces: &mut Strips,
    displays: &LayoutDisplays,
    config: &Config,
    commands: &mut Commands,
) {
    let Some(workspace) = workspace.checked_sub(1) else {
        return;
    };
    let Some((source, source_active, Some(source_display), source_id)) =
        workspaces.iter().find(|(strip, ..)| strip.contains(entity))
    else {
        return;
    };
    let Some((target, _, Some(target_display), target_id)) = workspaces
        .iter()
        .find(|(strip, ..)| strip.id() == space && strip.virtual_index == workspace)
    else {
        return;
    };
    if source.is_fullscreen() || target.is_fullscreen() {
        return;
    }
    let target_display = target_display.parent();
    let Ok((target_viewport, target_dock)) = displays.viewports.get(target_display) else {
        return;
    };
    let display_id = target_viewport.id();
    // Read the active native Space at dispatch time. The Space may have
    // changed since the client queried its destination.
    if !displays
        .window_manager
        .active_display_space(display_id)
        .is_ok_and(|shown_space| shown_space == space)
    {
        debug!(
            space,
            display_id, "skipping Place: target native Space is not shown"
        );
        return;
    }
    let Ok(from) = source.index_of(entity) else {
        return;
    };

    if source_id == target_id {
        if index < source.len()
            && from != index
            && let Ok((mut strip, ..)) = workspaces.get_mut(source_id)
        {
            strip.move_column(from, index);
        }
        return;
    }

    let source_display = source_display.parent();
    let Ok((source_viewport, source_dock)) = displays.viewports.get(source_display) else {
        return;
    };
    let Ok(column) = source.get(from) else {
        return;
    };
    let members: Vec<_> = column.window_iter().collect();
    if index > target.len() || members.iter().any(|member| target.contains(*member)) {
        return;
    }
    let source_focus = windows.focused().map(|(_, focused)| focused);
    let neighbour = source_focus
        .filter(|focused| source_active && members.contains(focused))
        .and_then(|focused| {
            source
                .left_neighbour(focused)
                .or_else(|| source.right_neighbour(focused))
        });

    let Ok([(mut source, ..), (mut target, ..)]) = workspaces.get_many_mut([source_id, target_id])
    else {
        return;
    };
    if !source.transfer_column(&mut target, from, index) {
        return;
    }
    if let Some(neighbour) = neighbour {
        commands.focus_entity(neighbour, false);
    }
    if source_display != target_display {
        let source_width = source_viewport
            .actual_display_bounds(source_dock, config)
            .width();
        let target_width = target_viewport
            .actual_display_bounds(target_dock, config)
            .width();
        if source_width > 0 && target_width > 0 {
            for &member in &members {
                if let Some(size) = windows.moving_frame(member).map(|frame| frame.size()) {
                    let ratio = f64::from(size.x) / f64::from(source_width);
                    let width = round_px(ratio * f64::from(target_width)).clamp(1, target_width);
                    commands.resize_entity(member, Size::new(width, size.y));
                }
            }
        }
    }
    if source_display != target_display || source.id() != target.id() {
        ColumnInTransit::spawn(members, commands);
    }
}

/// Takes a window out of the tiling layout or puts it back. Mirrors
/// `manage_window`: floating→tiled has to re-append the window to a strip
/// itself, since nothing downstream does it automatically.
fn set_floating(entity: Entity, floating: bool, workspaces: &mut Strips, commands: &mut Commands) {
    if let Ok(mut entity_commands) = commands.get_entity(entity) {
        if floating {
            entity_commands.try_insert(Unmanaged::Floating);
        } else {
            entity_commands.try_remove::<Unmanaged>();
        }
    }

    if !floating
        && !workspaces
            .iter()
            .any(|(strip, _, _, _)| strip.contains(entity))
        && let Some((mut strip, _, _, _)) = workspaces.iter_mut().find(|(_, active, _, _)| *active)
    {
        strip.append(entity);
        commands.reshuffle_around(entity);
    }
}

//! Replays a script's [`LayoutOp`]s against the live world.
//!
//! A Lua handler transforms a `WindowSet` — a pure value — and returns it; what
//! comes back over the worker channel is the sequence of operations that
//! produced it. This is where those become real window movements.
//!
//! Two things make this different from every other command handler. First, the
//! ops name a window: everything else in `src/commands.rs` acts on whatever is
//! focused, which is right for a keybinding and useless for
//! `for _, w in ipairs(ws:windows())`. Second, the snapshot the script
//! transformed is up to a frame old, so a window it names may be gone by now.
//! Each op is therefore applied independently and best-effort: one that cannot
//! be resolved is logged at debug and skipped, and its neighbours still apply.
//! A handler that cares can check for itself — the window set it was handed
//! says what existed when it was taken.

use bevy::ecs::entity::Entity;
use bevy::ecs::message::MessageReader;
use bevy::ecs::query::{Has, Without};
use bevy::ecs::system::{Commands, Query};
use paneru_shared_types::windowset::LayoutOp;
use tracing::debug;

use crate::commands::{Command, MoveFocus, Operation};
use crate::ecs::focus::FocusWindow;
use crate::ecs::layout::{Column, LayoutStrip, StackItem};
use crate::ecs::params::Windows;
use crate::ecs::workspace::VirtualMoveMarker;
use crate::ecs::{
    ActiveWorkspaceMarker, SendMessageTrigger, SpawnCommandsExt, Unmanaged, WidthRatio, Window,
};
use crate::events::Event;

/// Applies the layout operations a Lua handler returned.
#[allow(clippy::needless_pass_by_value)]
pub(crate) fn apply_layout_ops(
    mut messages: MessageReader<Event>,
    windows: Windows,
    mut workspaces: Query<(&mut LayoutStrip, Has<ActiveWorkspaceMarker>), Without<Window>>,
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
        for op in ops {
            apply(op, &windows, &mut workspaces, &mut commands);
        }
    }
}

/// Applies one op, or explains why it could not be.
// One arm per operation, kept flat so adding one is a local change rather than
// a hunt across helpers.
#[allow(clippy::too_many_lines)]
fn apply(
    op: LayoutOp,
    windows: &Windows,
    workspaces: &mut Query<(&mut LayoutStrip, Has<ActiveWorkspaceMarker>), Without<Window>>,
    commands: &mut Commands,
) {
    // Ops that name a window need it to still exist. Resolve up front so every
    // arm below can assume it does.
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
            commands.trigger(FocusWindow {
                entity,
                raise: true,
            });
        }

        LayoutOp::Swap(_, other) => {
            let entity = entity.expect("Swap names a window");
            let Some((_, other_entity)) = windows.find(other) else {
                debug!(target: "paneru::lua", "skipping {op:?}: window {other} is gone");
                return;
            };
            let Some((mut strip, _)) = workspaces
                .iter_mut()
                .find(|(strip, _)| strip.contains(entity) && strip.contains(other_entity))
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

        LayoutOp::View { workspace } => {
            let Some(index) = workspace.checked_sub(1) else {
                debug!(target: "paneru::lua", "skipping {op:?}: workspaces are numbered from 1");
                return;
            };
            // Showing a workspace is not window-addressed at all, so the
            // existing command does exactly the right thing.
            commands.trigger(SendMessageTrigger(Event::Command {
                command: Command::Window(Operation::VirtualNumber(index)),
            }));
        }

        LayoutOp::SetFloating { floating, .. } => {
            set_floating(
                entity.expect("SetFloating names a window"),
                floating,
                workspaces,
                commands,
            );
        }

        // Floating *is* how a window becomes unmanaged here, so the two ops
        // are one mechanism seen from either end.
        LayoutOp::SetManaged { managed, .. } => {
            set_floating(
                entity.expect("SetManaged names a window"),
                !managed,
                workspaces,
                commands,
            );
        }

        LayoutOp::SetWidth { ratio, .. } => {
            let entity = entity.expect("SetWidth names a window");
            if !ratio.is_finite() || ratio <= 0.0 {
                debug!(target: "paneru::lua", "skipping {op:?}: a width ratio must be positive");
                return;
            }
            // The layout pipeline turns the ratio into an actual width against
            // whichever display the window is on, so this does not have to.
            if let Ok(mut entity_commands) = commands.get_entity(entity) {
                entity_commands.try_insert(WidthRatio(ratio));
            }
            // Stacked siblings share a column, and so its width.
            if let Some((strip, _)) = workspaces.iter().find(|(strip, _)| strip.contains(entity))
                && let Some(Column::Stack(stack)) = strip
                    .index_of(entity)
                    .ok()
                    .and_then(|index| strip.get(index).ok())
            {
                for sibling in stack.iter().flat_map(StackItem::window_iter) {
                    if sibling != entity
                        && let Ok(mut entity_commands) = commands.get_entity(sibling)
                    {
                        entity_commands.try_insert(WidthRatio(ratio));
                    }
                }
            }
            commands.reshuffle_around(entity);
        }

        LayoutOp::Stack { onto, .. } => {
            let entity = entity.expect("Stack names a window");
            let Some((_, onto_entity)) = windows.find(onto) else {
                debug!(target: "paneru::lua", "skipping {op:?}: window {onto} is gone");
                return;
            };
            let Some((mut strip, _)) = workspaces
                .iter_mut()
                .find(|(strip, _)| strip.contains(entity) && strip.contains(onto_entity))
            else {
                debug!(target: "paneru::lua", "skipping {op:?}: the two windows share no strip");
                return;
            };
            if strip.stack(entity).is_err() {
                debug!(target: "paneru::lua", "skipping {op:?}: the layout refused the stack");
                return;
            }
            commands.reshuffle_around(entity);
        }

        LayoutOp::Unstack(_) => {
            let entity = entity.expect("Unstack names a window");
            let Some((mut strip, _)) = workspaces
                .iter_mut()
                .find(|(strip, _)| strip.contains(entity))
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

/// Takes a window out of the tiling layout or puts it back.
///
/// Mirrors `manage_window`: going floating→tiled only flips the component, and
/// nothing downstream re-inserts the window into a strip, so a window that lost
/// its membership has to be appended back or the change is invisible.
fn set_floating(
    entity: Entity,
    floating: bool,
    workspaces: &mut Query<(&mut LayoutStrip, Has<ActiveWorkspaceMarker>), Without<Window>>,
    commands: &mut Commands,
) {
    if let Ok(mut entity_commands) = commands.get_entity(entity) {
        if floating {
            entity_commands.try_insert(Unmanaged::Floating);
        } else {
            entity_commands.try_remove::<Unmanaged>();
        }
    }

    if !floating
        && !workspaces.iter().any(|(strip, _)| strip.contains(entity))
        && let Some((mut strip, _)) = workspaces.iter_mut().find(|(_, active)| *active)
    {
        strip.append(entity);
        commands.reshuffle_around(entity);
    }
}

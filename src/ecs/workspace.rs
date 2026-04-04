use bevy::ecs::component::Component;
use bevy::ecs::entity::Entity;
use bevy::ecs::hierarchy::ChildOf;
use bevy::ecs::lifecycle::{Add, RemovedComponents};
use bevy::ecs::message::MessageReader;
use bevy::ecs::observer::On;
use bevy::ecs::query::{Added, Has, With, Without};
use bevy::ecs::system::{Commands, Local, Populated, Query, Res, Single};
use std::collections::HashSet;
use tracing::{Level, debug, error, instrument, warn};

use super::{ActiveDisplayMarker, SpawnWindowTrigger, WMEventTrigger};
use crate::commands::{Direction, MoveFocus, Operation, filter_window_operations};
use crate::ecs::layout::LayoutStrip;
use crate::ecs::params::{ActiveDisplay, Windows};
use crate::ecs::{
    ActiveWorkspaceMarker, Bounds, NativeFullscreenMarker, Position, RefreshWindowSizes,
    SelectedVirtualMarker, Timeout, Unmanaged, WorkspaceIndicator, focus_entity, reposition_entity,
    reshuffle_around,
};
use crate::errors::Result;
use crate::events::Event;
use crate::manager::{Application, Display, Origin, Window, WindowManager};
use crate::platform::{WinID, WorkspaceId};

/// Marker component to move a window to a specific virtual index on its current workspace.
#[derive(Component)]
pub(super) struct VirtualMoveMarker {
    pub target_virtual_index: u32,
    pub move_focus: MoveFocus,
}

#[derive(Component, Debug)]
pub(crate) struct PreviousStripPosition {
    origin: Origin,
    focus: Option<Entity>,
}

#[allow(clippy::needless_pass_by_value)]
#[instrument(level = Level::DEBUG, skip_all, fields(trigger))]
pub(super) fn workspace_change_trigger(
    trigger: On<WMEventTrigger>,
    windows: Windows,
    mut workspaces: Query<(
        &mut LayoutStrip,
        Entity,
        Has<ActiveWorkspaceMarker>,
        Has<SelectedVirtualMarker>,
    )>,
    active_display: Single<(&Display, Entity), With<ActiveDisplayMarker>>,
    window_manager: Res<WindowManager>,
    mut commands: Commands,
) {
    let Event::SpaceChanged = trigger.event().0 else {
        return;
    };
    let (active_display, display_entity) = *active_display;

    let Ok(workspace_id) = window_manager.active_display_space(active_display.id()) else {
        error!("Unable to get active workspace id!");
        return;
    };

    let mut remove_from = None;
    let mut insert_into = None;
    for (strip, entity, active, selected) in &workspaces {
        if active && strip.id() != workspace_id {
            debug!("Workspace id {} no longer active", strip.id());
            remove_from = Some(entity);
        }
        if !active && strip.id() == workspace_id && selected {
            debug!("Workspace id {} is active", strip.id());
            insert_into = Some(entity);
        }
    }

    if insert_into.is_none() {
        // Fallback: find any strip for this workspace, preferably the one with virtual_index 0.
        insert_into = workspaces
            .iter()
            .filter(|(strip, _, _, _)| strip.id() == workspace_id)
            .min_by_key(|(strip, _, _, _)| strip.virtual_index)
            .map(|(_, entity, _, _)| entity);
    }

    if insert_into.is_none()
        && let Some(old_space) = remove_from
        && window_manager.is_fullscreen_space(active_display.id())
        && let Some((_, focused)) = windows.focused()
        && let Ok((mut old_strip, _, _, _)) = workspaces.get_mut(old_space)
    {
        debug!("workspace_change: space={workspace_id} fullscreen");

        let fullscreen_marker = NativeFullscreenMarker {
            previous_strip: old_strip.id(),
            previous_index: old_strip.index_of(focused).unwrap_or(0),
        };
        old_strip.remove(focused);

        let fullscreen_strip = LayoutStrip::fullscreen(workspace_id, focused);
        let entity = commands
            .spawn((
                Position(active_display.bounds().min),
                fullscreen_marker,
                fullscreen_strip,
                ChildOf(display_entity),
            ))
            .id();
        insert_into = Some(entity);
    }

    if let Some(into) = insert_into
        && let Ok(mut entity_commands) = commands.get_entity(into)
    {
        entity_commands
            .try_insert(ActiveWorkspaceMarker)
            .try_insert(SelectedVirtualMarker);
    }
}

#[allow(clippy::needless_pass_by_value, clippy::too_many_arguments)]
#[instrument(level = Level::DEBUG, skip_all, fields(trigger))]
pub(super) fn detect_moved_windows(
    activated_workspace: Single<Entity, Added<ActiveWorkspaceMarker>>,
    windows: Windows,
    mut workspaces: Query<(&mut LayoutStrip, &ChildOf, Has<NativeFullscreenMarker>), With<ChildOf>>,
    active_display: Single<(Entity, &Display), With<ActiveDisplayMarker>>,
    apps: Query<&mut Application>,
    window_manager: Res<WindowManager>,
    mut ignored_windows: Local<HashSet<WinID>>,
    mut commands: Commands,
) {
    let Ok(workspace_id) = workspaces
        .get(*activated_workspace)
        .map(|strip| strip.0.id())
    else {
        return;
    };
    debug!("workspace {workspace_id}");

    let strips = workspaces
        .iter()
        .filter_map(|(strip, _, _)| (strip.id() == workspace_id).then_some(strip))
        .collect::<Vec<_>>();
    let find_window = |window_id| windows.find_managed(window_id).map(|(_, entity)| entity);
    let Ok((moved_windows, mut unresolved)) =
        windows_not_in_strips(workspace_id, find_window, &strips, &window_manager).inspect_err(
            |err| {
                warn!("unable to get windows in the current workspace: {err}");
            },
        )
    else {
        return;
    };
    // Skip known, but unmanaged windows.
    unresolved.retain(|window_id| {
        !ignored_windows.contains(window_id) && windows.find(*window_id).is_none()
    });

    if !unresolved.is_empty() {
        // Retry unresolved window IDs: during startup bruteforce, windows on
        // inactive workspaces may have stale AX attributes (e.g. AXGroup instead
        // of AXWindow).  Now that this workspace is active, re-query each app's
        // window list — the AX data should be correct.
        let retry_windows = apps
            .into_iter()
            .flat_map(|app| {
                app.window_list()
                    .into_iter()
                    .filter(|window| unresolved.contains(&window.id()))
            })
            .collect::<Vec<_>>();
        if retry_windows.is_empty() {
            for id in unresolved {
                ignored_windows.insert(id);
            }
        } else {
            debug!(
                "retrying unresolved windows: {}",
                retry_windows
                    .iter()
                    .map(|window| format!("{}", window.id()))
                    .collect::<Vec<_>>()
                    .join(" ")
            );
            commands.trigger(SpawnWindowTrigger(retry_windows));
        }
    }

    for entity in moved_windows {
        if workspaces
            .iter()
            .any(|(strip, _, fullscreen)| fullscreen && strip.contains(entity))
        {
            // Do not relocate fullscreen windows, this will happen
            // during the destructino of their workspace.
            continue;
        }

        debug!("Window {entity} moved to workspace {workspace_id}.");
        for (mut strip, child, _) in &mut workspaces {
            strip.remove(entity);

            if strip.id() == workspace_id && child.parent() == active_display.0 {
                strip.append(entity);
            }
        }
    }
}

#[allow(clippy::needless_pass_by_value)]
#[instrument(level = Level::DEBUG, skip_all, fields(trigger))]
pub(super) fn workspace_destroyed_trigger(
    trigger: On<WMEventTrigger>,
    mut workspaces: Populated<(&mut LayoutStrip, Entity, Option<&NativeFullscreenMarker>)>,
    mut commands: Commands,
) {
    let Event::SpaceDestroyed { space_id } = trigger.event().0 else {
        return;
    };

    let Some((entity, fullscreen)) = workspaces.iter().find_map(|(strip, entity, fullscreen)| {
        let window = strip.first().ok().and_then(|col| col.top());
        (strip.id() == space_id).then_some((entity, window.zip(fullscreen.cloned())))
    }) else {
        return;
    };

    if let Some((
        window,
        NativeFullscreenMarker {
            previous_strip,
            previous_index,
        },
    )) = fullscreen
        && let Some((mut strip, _, _)) = workspaces
            .iter_mut()
            .find(|(strip, _, _)| strip.id() == previous_strip)
    {
        debug!(
            "previously fullscreened window {entity} inserted at {}",
            previous_index
        );
        strip.insert_at(previous_index, window);
        reshuffle_around(window, &mut commands);
    }

    if let Ok(mut entity_commands) = commands.get_entity(entity) {
        debug!("Workspace destroyed {space_id} {entity}");
        entity_commands.try_despawn();
    }
}

#[allow(clippy::needless_pass_by_value)]
#[instrument(level = Level::DEBUG, skip_all, fields(trigger))]
pub(super) fn workspace_created_trigger(
    trigger: On<WMEventTrigger>,
    active_display: Single<(&Display, Entity), With<ActiveDisplayMarker>>,
    workspaces: Query<&LayoutStrip>,
    mut commands: Commands,
) {
    let Event::SpaceCreated { space_id } = trigger.event().0 else {
        return;
    };

    if workspaces.into_iter().any(|strip| strip.id() == space_id) {
        warn!("Workspace {space_id} already exists!");
        return;
    }
    debug!("Workspace create {space_id}");
    let (active_display, display_entity) = *active_display;
    let strip = LayoutStrip::new(space_id, 0);
    let origin = Position(active_display.bounds().min);
    commands.spawn((
        strip,
        origin,
        SelectedVirtualMarker,
        ChildOf(display_entity),
    ));
}

fn windows_not_in_strips<F: Fn(WinID) -> Option<Entity>>(
    workspace_id: WorkspaceId,
    find_window: F,
    strips: &[&LayoutStrip],
    window_manager: &WindowManager,
) -> Result<(Vec<Entity>, Vec<WinID>)> {
    window_manager
        .windows_in_workspace(workspace_id)
        .map(|ids| {
            let mut moved = Vec::new();
            let mut unresolved = Vec::new();
            for id in ids {
                if let Some(entity) = find_window(id) {
                    // If window exists in any of the active workspace rows.
                    if strips.iter().any(|strip| strip.contains(entity)) {
                        continue;
                    }
                    moved.push(entity);
                } else {
                    unresolved.push(id);
                }
            }
            (moved, unresolved)
        })
}

#[allow(clippy::needless_pass_by_value)]
#[instrument(level = Level::DEBUG, skip_all)]
pub(super) fn find_orphaned_workspaces(
    orphans: Populated<(&LayoutStrip, Entity, &Timeout, Option<&ChildOf>), With<Timeout>>,
    displays: Query<(&Display, Entity)>,
    window_manager: Res<WindowManager>,
    mut commands: Commands,
) {
    let present = window_manager.present_displays();

    for (orphan, orphan_entity, timeout, child) in orphans {
        if orphan.len() == 0 {
            if let Ok(mut cmd) = commands.get_entity(orphan_entity) {
                cmd.try_despawn();
            }
            debug!("despawning empty orphan workspace {}", orphan.id());
            continue;
        }
        if child.is_some() {
            // Was reparented, remove timer.
            if let Ok(mut cmd) = commands.get_entity(orphan_entity) {
                cmd.try_remove::<Timeout>();
                cmd.insert(RefreshWindowSizes::default());
            }
            debug!(
                "layout strip {} was re-parented, removing timeout.",
                orphan.id()
            );
            continue;
        }

        if timeout.timer.is_finished() {
            // Rescue windows from orphaned strips before despawning by floating them.
            debug!("Rescue windows from timed out orphan {}.", orphan.id());
            for lost_window in orphan.all_windows() {
                if let Ok(mut cmd) = commands.get_entity(lost_window) {
                    cmd.try_insert(Unmanaged::Floating);
                }
            }
            continue;
        }

        // Find which display now owns this space ID.
        let target = present.iter().find_map(|(present_display, spaces)| {
            if spaces.iter().any(|&id| id == orphan.id()) {
                displays
                    .iter()
                    .find(|(d, _)| d.id() == present_display.id())
            } else {
                None
            }
        });
        let Some((target_display, target_entity)) = target else {
            continue; // No display owns this space yet; wait for next tick.
        };

        debug!(
            "Re-parenting orphaned strip {} to display {}",
            orphan.id(),
            target_display.id(),
        );

        if let Ok(mut cmd) = commands.get_entity(orphan_entity) {
            cmd.try_remove::<Timeout>()
                .insert(ChildOf(target_entity))
                .insert(RefreshWindowSizes::default());
        }
    }
}

#[allow(clippy::needless_pass_by_value)]
pub(crate) fn refresh_workspace_window_sizes(
    layout_strip: Single<(&LayoutStrip, Entity, &RefreshWindowSizes), With<ActiveWorkspaceMarker>>,
    mut windows: Query<(Entity, &mut Window, &mut Bounds, Option<&Unmanaged>)>,
    active_display: ActiveDisplay,
    window_manager: Res<WindowManager>,
    mut commands: Commands,
) {
    let (strip, strip_entity, marker) = *layout_strip;
    if !marker.ready() {
        return;
    }

    debug!("refreshing workspace {} sizes", strip.id());
    let mut in_workspace = window_manager
        .windows_in_workspace(strip.id())
        .inspect_err(|err| {
            warn!("getting windows in workspace: {err}");
        })
        .unwrap_or_default();

    // Resize windows for the new display dimensions.
    for entity in strip.all_windows() {
        let Ok((_, ref mut window, ref mut bounds, _)) = windows.get_mut(entity) else {
            continue;
        };
        let Ok(frame) = window.update_frame() else {
            continue;
        };
        bounds.0 = frame.size();
        debug!("refreshing window {} frame {:?}", window.id(), frame);

        in_workspace.retain(|window_id| *window_id != window.id());
    }

    // Find remaining windows which are outside of the strip.                                                  ...
    let floating = in_workspace
        .into_iter()
        .filter_map(|window_id| {
            windows
                .iter()
                .find_map(|(entity, window, _, unmanaged)| {
                    (window_id == window.id()).then_some(unmanaged.zip(Some(entity)))
                })
                .flatten()
        })
        .filter_map(|(unmanaged, entity)| {
            matches!(unmanaged, Unmanaged::Floating).then_some(entity)
        });
    for window_entity in floating {
        debug!("repositioning floating window {window_entity}");
        reposition_entity(window_entity, active_display.bounds().min, &mut commands);
    }

    if let Ok(mut cmds) = commands.get_entity(strip_entity) {
        cmds.try_remove::<RefreshWindowSizes>();
    }
}

/// Periodically checks for changes in the active workspace (space) on the active display.
/// This system acts as a workaround for inconsistent workspace change notifications on some macOS versions.
/// If a change is detected, it triggers an `Event::SpaceChanged` event.
///
/// # Arguments
///
/// * `active_display` - An `ActiveDisplay` system parameter providing immutable access to the active display.
/// * `window_manager` - The `WindowManager` resource for querying active space information.
/// * `throttle` - A `ThrottledSystem` to control the execution rate of this system.
/// * `current_space` - A `Local` resource storing the ID of the currently observed space.
/// * `commands` - Bevy commands to trigger `WMEventTrigger` events for space changes.
#[allow(clippy::needless_pass_by_value)]
pub(super) fn workspace_change_watcher(
    active_display: ActiveDisplay,
    window_manager: Res<WindowManager>,
    mut current_space: Local<WorkspaceId>,
    mut commands: Commands,
) {
    let Ok(space_id) = window_manager
        .0
        .active_display_space(active_display.id())
        .inspect_err(|err| warn!("{err}"))
    else {
        return;
    };

    if *current_space != space_id {
        *current_space = space_id;
        debug!("workspace changed to {space_id}");
        commands.trigger(WMEventTrigger(Event::SpaceChanged));
    }
}

/// Removes previuos `ActiveWorkspaceMarker`'s when a new one is inserted.
#[allow(clippy::needless_pass_by_value)]
#[instrument(level = Level::DEBUG, skip_all, fields(trigger))]
pub(super) fn cleanup_active_workspace_marker(
    trigger: On<Add, ActiveWorkspaceMarker>,
    workspaces: Query<(Entity, Has<ActiveWorkspaceMarker>), With<LayoutStrip>>,
    mut commands: Commands,
) {
    workspaces.iter().for_each(|(entity, marker)| {
        if marker
            && entity != trigger.entity
            && let Ok(mut entity_commands) = commands.get_entity(entity)
        {
            entity_commands.try_remove::<ActiveWorkspaceMarker>();
        }
    });
}

/// Removes previuos `SelectedVirtualMarker`'s when a new one is inserted.
#[allow(clippy::needless_pass_by_value)]
#[instrument(level = Level::DEBUG, skip_all, fields(trigger))]
pub(super) fn cleanup_selected_space_marker(
    trigger: On<Add, SelectedVirtualMarker>,
    workspaces: Query<(Entity, &LayoutStrip, Has<SelectedVirtualMarker>)>,
    mut commands: Commands,
) {
    let Ok(workspace_id) = workspaces
        .get(trigger.entity)
        .map(|(_, strip, _)| strip.id())
    else {
        return;
    };

    // Remove the marker from other strips on the same workspace.
    workspaces.iter().for_each(|(entity, strip, marker)| {
        if marker
            && entity != trigger.entity
            && strip.id() == workspace_id
            && let Ok(mut entity_commands) = commands.get_entity(entity)
        {
            entity_commands.try_remove::<SelectedVirtualMarker>();
        }
    });
}

#[allow(clippy::needless_pass_by_value)]
pub(super) fn cleanup_virtual_workspaces(
    changed: Single<Entity, Added<ActiveWorkspaceMarker>>,
    mut strips: Populated<(Entity, &mut LayoutStrip)>,
    mut commands: Commands,
) {
    let changed_entity = *changed;
    let Some(workspace_id) = strips.get(changed_entity).ok().map(|(_, strip)| strip.id()) else {
        return;
    };
    debug!("cleaning up virtual workspaces on space {workspace_id}");
    let mut rows = strips
        .iter_mut()
        .filter(|(_, strip)| strip.id() == workspace_id)
        .collect::<Vec<_>>();
    rows.sort_by_key(|(_, strip)| strip.virtual_index);

    if rows.is_empty() {
        return;
    }

    let primary_entity = rows[0].0;
    let mut next_idx = 0;
    for (entity, mut strip) in rows {
        if strip.virtual_index > 0 && strip.len() == 0 {
            if entity == changed_entity {
                debug!("moving markers from despawned virtual workspace to primary");
                commands
                    .entity(primary_entity)
                    .try_insert(ActiveWorkspaceMarker)
                    .try_insert(SelectedVirtualMarker);
            }
            commands.entity(entity).despawn();
            continue;
        }
        if strip.virtual_index != next_idx {
            strip.virtual_index = next_idx;
        }
        next_idx += 1;
    }
}

#[allow(clippy::needless_pass_by_value)]
pub(super) fn handle_virtual_window_moves(
    moved_windows: Populated<(Entity, &VirtualMoveMarker), With<Window>>,
    mut workspaces: Query<(
        Entity,
        &mut LayoutStrip,
        &Position,
        Has<ActiveWorkspaceMarker>,
    )>,
    active_display: Single<(Entity, &Display), With<ActiveDisplayMarker>>,
    mut commands: Commands,
) {
    let Some((workspace_id, source_entity)) = workspaces
        .iter()
        .find_map(|(entity, strip, _, active)| active.then_some((strip.id(), entity)))
    else {
        return;
    };

    let (display_entity, active_display) = *active_display;
    for (window_entity, move_marker) in &moved_windows {
        commands.entity(window_entity).remove::<VirtualMoveMarker>();
        let follow = matches!(move_marker.move_focus, MoveFocus::Follow);

        let target_idx = move_marker.target_virtual_index;
        let target = workspaces.iter().find_map(|(entity, strip, _, _)| {
            (strip.id() == workspace_id && strip.virtual_index == target_idx).then_some(entity)
        });

        // Must be captured before strip.remove below.
        let source_neighbour = workspaces
            .get(source_entity)
            .ok()
            .and_then(|(_, strip, _, _)| {
                strip
                    .left_neighbour(window_entity)
                    .or_else(|| strip.right_neighbour(window_entity))
            });
        // If source will be empty after the move, Stay becomes Follow
        // since there's nothing left to look at.
        let stay = !follow && source_neighbour.is_some();

        let target_entity = if let Some(entity) = target {
            entity
        } else {
            // Stay: spawn offscreen with PreviousStripPosition for later restoration.
            // Follow (or empty source): spawn visible, user is switching to it.
            let visible_origin = active_display.bounds().min;
            let origin = if stay {
                active_display.bounds().max - 10
            } else {
                visible_origin
            };
            debug!(
                "Creating new virtual row {target_idx} on workspace {}",
                workspace_id
            );
            let mut new_strip = LayoutStrip::new(workspace_id, target_idx);
            new_strip.append(window_entity);

            let mut spawned = commands.spawn((
                new_strip,
                Position(origin),
                SelectedVirtualMarker,
                ChildOf(display_entity),
            ));
            if stay {
                // show_active_workspace needs this to restore the strip
                // onscreen when the user later switches to this workspace.
                spawned.insert(PreviousStripPosition {
                    origin: visible_origin,
                    focus: Some(window_entity),
                });
            }
            spawned.id()
        };

        // Preserve the source strip's scroll position for when the user returns.
        if !stay
            && let Ok(mut entity_commands) = commands.get_entity(source_entity)
            && let Ok((_, source_strip, position, _)) = workspaces.get(source_entity)
        {
            let focus = source_strip
                .left_neighbour(window_entity)
                .or_else(|| source_strip.right_neighbour(window_entity));
            entity_commands.try_insert(PreviousStripPosition {
                origin: position.0,
                focus,
            });
        }

        // Move the window before moving markers to avoid being detected as a moved window.
        for (entity, mut strip, _, _) in &mut workspaces {
            if entity == target_entity {
                strip.append(window_entity);
            } else {
                strip.remove(window_entity);
            }
        }

        // Insert new markers. ActiveWorkspaceMarker switches the view.
        if let Ok(mut entity_commands) = commands.get_entity(target_entity) {
            entity_commands.try_insert(SelectedVirtualMarker);
            if !stay {
                entity_commands.try_insert(ActiveWorkspaceMarker);
            }
        }

        if stay && let Some(neighbour) = source_neighbour {
            // Layout chain repositions the window offscreen with its hidden strip.
            focus_entity(neighbour, false, &mut commands);
            reshuffle_around(neighbour, &mut commands);
        } else {
            reshuffle_around(window_entity, &mut commands);
        }
        debug!(
            "Moved window {} to virtual workspace {}",
            window_entity, target_idx
        );
    }
}

/// Handles the keybinding for switching between virtual workspaces.
#[instrument(level = Level::DEBUG, skip_all)]
#[allow(clippy::needless_pass_by_value)]
pub(crate) fn switch_virtual_workspace_bind(
    mut messages: MessageReader<Event>,
    active_display: ActiveDisplay,
    workspaces: Query<(Entity, &LayoutStrip, Has<ActiveWorkspaceMarker>)>,
    indicators: Query<Entity, With<WorkspaceIndicator>>,
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
        Direction::South => (current_index + 1).clamp(0, rows.len() - 1),
        Direction::North => current_index.saturating_sub(1),
        _ => return,
    };

    if next_index == current_index {
        return;
    }

    let new_entity = rows[next_index].0;
    let next_virtual_index = rows[next_index].1.virtual_index;
    if let Ok(mut entity_commands) = commands.get_entity(new_entity) {
        entity_commands
            .try_insert(SelectedVirtualMarker)
            .try_insert(ActiveWorkspaceMarker);

        // Flash workspace number
        for indicator_entity in &indicators {
            commands.entity(indicator_entity).despawn();
        }
        commands.spawn(WorkspaceIndicator {
            number: next_virtual_index + 1,
            timer: bevy::time::Timer::from_seconds(1.0, bevy::time::TimerMode::Once),
        });
    }
    debug!(
        "Switched virtual workspace on display {} from {} to {}",
        active_display.id(),
        rows[current_index].1.virtual_index,
        rows[next_index].1.virtual_index
    );
}

/// Handles the keybinding to move windows between virtual workspaces.
#[instrument(level = Level::DEBUG, skip_all)]
#[allow(clippy::needless_pass_by_value)]
pub(crate) fn move_virtual_workspace_bind(
    mut messages: MessageReader<Event>,
    windows: Windows,
    active_display: ActiveDisplay,
    indicators: Query<Entity, With<WorkspaceIndicator>>,
    mut commands: Commands,
) {
    let Some(Operation::VirtualMove(direction, move_focus)) =
        filter_window_operations(&mut messages, |op| {
            matches!(op, Operation::VirtualMove(_, _))
        })
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
        move_focus: *move_focus,
    });

    if *move_focus == MoveFocus::Follow {
        for indicator_entity in &indicators {
            commands.entity(indicator_entity).despawn();
        }
        commands.spawn(WorkspaceIndicator {
            number: target_virtual_index + 1,
            timer: bevy::time::Timer::from_seconds(1.0, bevy::time::TimerMode::Once),
        });
    }

    debug!("Moving {focused_entity} to new virtual space {target_virtual_index}");
}

/// Hide windows on virtual workspaces which do not have an active marker.
/// This is a system and not the usual trigger, because the event should come delayed in the next
/// "frame" - so the active marker can be moved to another strip.
#[allow(clippy::needless_pass_by_value, clippy::type_complexity)]
#[instrument(level = Level::DEBUG, skip_all)]
pub(super) fn hide_inactive_workspace(
    mut removed: RemovedComponents<ActiveWorkspaceMarker>,
    windows: Windows,
    mut workspaces: Query<
        (
            &mut Position,
            &LayoutStrip,
            Has<ActiveWorkspaceMarker>,
            Option<&PreviousStripPosition>,
        ),
        Without<Window>,
    >,
    active_display: Single<&Display, With<ActiveDisplayMarker>>,
    mut commands: Commands,
) {
    for entity in removed.read() {
        let Ok(workspace_id) = workspaces.get(entity).map(|(_, strip, _, _)| strip.id()) else {
            continue;
        };
        let still_active = workspaces
            .iter()
            .filter_map(|(_, strip, active, _)| (strip.id() == workspace_id).then_some(active))
            .any(|active| active);
        if !still_active {
            // This system checks whether any of the other virtual strips in the current workspace have an
            // active marker - if not, the focus probably moved to another display, so don't hide.
            continue;
        }

        let Ok((mut position, _, _, previous)) = workspaces.get_mut(entity) else {
            continue;
        };

        if previous.is_none() {
            let focus = windows.focused().map(|(_, entity)| entity);
            if let Ok(mut entity_commands) = commands.get_entity(entity) {
                entity_commands.try_insert(PreviousStripPosition {
                    origin: position.0,
                    focus,
                });
            }
        }

        let bounds = active_display.bounds();
        position.0 = bounds.max - 10;
    }
}

#[allow(clippy::needless_pass_by_value)]
#[instrument(level = Level::DEBUG, skip_all)]
pub(super) fn show_active_workspace(
    activated: Single<Entity, Added<ActiveWorkspaceMarker>>,
    windows: Windows,
    mut workspaces: Query<
        (&mut Position, &LayoutStrip, Option<&PreviousStripPosition>),
        Without<Window>,
    >,
    mut commands: Commands,
) {
    let Ok((mut position, strip, previous_position)) = workspaces.get_mut(*activated) else {
        return;
    };

    // If no previous strip position exists, then the workspace was not hidden.
    if let Some(PreviousStripPosition { origin, focus }) = previous_position {
        if let Ok(mut entity_commands) = commands.get_entity(*activated) {
            entity_commands.try_remove::<PreviousStripPosition>();
        }
        position.0 = *origin;

        if let Some((_, current_focus)) = windows.focused()
            && strip.contains(current_focus)
        {
            return;
        }

        // Focus on the previous window
        if let Some(entity) = focus
            && strip.contains(*entity)
        {
            focus_entity(*entity, false, &mut commands);
        }
    }
}

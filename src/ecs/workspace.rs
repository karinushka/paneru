use bevy::ecs::entity::Entity;
use bevy::ecs::hierarchy::ChildOf;
use bevy::ecs::lifecycle::Add;
use bevy::ecs::observer::On;
use bevy::ecs::query::{Has, With, Without};
use bevy::ecs::system::{Commands, Local, Populated, Query, Res, Single};
use tracing::{Level, debug, error, instrument, warn};

use super::{ActiveDisplayMarker, SpawnWindowTrigger, WMEventTrigger};
use crate::ecs::layout::LayoutStrip;
use crate::ecs::params::{ActiveDisplay, Windows};
use crate::ecs::{
    ActiveWorkspaceMarker, Bounds, NativeFullscreenMarker, Position, RefreshWindowSizes, Timeout,
    Unmanaged, reposition_entity, reshuffle_around,
};
use crate::errors::Result;
use crate::events::Event;
use crate::manager::{Application, Display, Window, WindowManager};
use crate::platform::{WinID, WorkspaceId};

#[allow(clippy::needless_pass_by_value)]
#[instrument(level = Level::DEBUG, skip_all, fields(trigger))]
pub(super) fn workspace_change_trigger(
    trigger: On<WMEventTrigger>,
    windows: Windows,
    mut workspaces: Query<(&mut LayoutStrip, Entity, Has<ActiveWorkspaceMarker>)>,
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
    for (strip, entity, active) in &workspaces {
        if active && strip.id() != workspace_id {
            debug!("Workspace id {} no longer active", strip.id());
            remove_from = Some(entity);
        }
        if !active && strip.id() == workspace_id {
            debug!("Workspace id {} is active", strip.id());
            insert_into = Some(entity);
        }
    }

    if insert_into.is_none()
        && let Some(old_space) = remove_from
        && window_manager.is_fullscreen_space(active_display.id())
        && let Some((_, focused)) = windows.focused()
        && let Ok((mut old_strip, _, _)) = workspaces.get_mut(old_space)
    {
        debug!("workspace_change: space={workspace_id} fullscreen");

        let fullscreen_marker = NativeFullscreenMarker {
            previous_strip: old_strip.id(),
            previous_index: old_strip
                .index_of(focused)
                .inspect_err(|err| {
                    warn!("Error removing the maximized window from previous strip: {err}");
                })
                .unwrap_or(0),
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

    if let Some((from, into)) = remove_from.zip(insert_into) {
        if let Ok(mut entity_commands) = commands.get_entity(from) {
            entity_commands.try_remove::<ActiveWorkspaceMarker>();
        }
        if let Ok(mut entity_commands) = commands.get_entity(into) {
            entity_commands.try_insert(ActiveWorkspaceMarker);
        }
    }
}

#[allow(clippy::needless_pass_by_value, clippy::type_complexity)]
#[instrument(level = Level::DEBUG, skip_all, fields(trigger))]
pub(super) fn active_workspace_trigger(
    trigger: On<Add, ActiveWorkspaceMarker>,
    windows: Windows,
    mut workspaces: Query<
        (&mut LayoutStrip, &ChildOf, Option<&NativeFullscreenMarker>),
        With<ChildOf>,
    >,
    active_display: Single<(Entity, &Display), With<ActiveDisplayMarker>>,
    apps: Query<&mut Application>,
    window_manager: Res<WindowManager>,
    mut commands: Commands,
) {
    let Ok((active_strip, _, _)) = workspaces.get(trigger.entity) else {
        return;
    };
    let workspace_id = active_strip.id();
    debug!("workspace {workspace_id}");

    let find_window = |window_id| windows.find_managed(window_id).map(|(_, entity)| entity);
    let Ok((moved_windows, mut unresolved)) =
        windows_not_in_strip(workspace_id, find_window, active_strip, &window_manager).inspect_err(
            |err| {
                warn!("unable to get windows in the current workspace: {err}");
            },
        )
    else {
        return;
    };
    // Skip known, but unmanaged windows.
    unresolved.retain(|window_id| windows.find(*window_id).is_none());

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
        if !retry_windows.is_empty() {
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

    let had_moved_windows = !moved_windows.is_empty();
    let fullscreened = workspaces
        .iter()
        .filter_map(|(_, _, marker)| marker)
        .cloned()
        .collect::<Vec<_>>();
    for entity in moved_windows {
        debug!("Window {entity} moved to workspace {workspace_id}.");

        workspaces.iter_mut().for_each(|(mut strip, child, _)| {
            strip.remove(entity);
            if strip.id() == workspace_id && child.parent() == active_display.0 {
                if let Some(fullscreen) = fullscreened
                    .iter()
                    .find(|marker| marker.previous_strip == workspace_id)
                {
                    debug!(
                        "previously fullscreened window {entity} inserted at {}",
                        fullscreen.previous_index
                    );
                    strip.insert_at(fullscreen.previous_index, entity);
                } else {
                    strip.append(entity);
                }
            }
        });
        reshuffle_around(entity, &mut commands);
    }

    // Always reshuffle on workspace activation so that windows are
    // re-laid-out after returning from a different space (e.g. native
    // fullscreen) where they may have been positioned with stale data.
    // Prefer the focused window so the viewport centres on what the user
    // was looking at; fall back to the first column.
    if !had_moved_windows {
        let focused_entity = windows.focused().map(|(_, entity)| entity).filter(|e| {
            workspaces
                .get(trigger.entity)
                .is_ok_and(|(strip, _, _)| strip.contains(*e))
        });
        let fallback = || {
            workspaces
                .get(trigger.entity)
                .ok()
                .and_then(|(strip, _, _)| strip.get(0).ok())
                .and_then(|col| col.top())
        };
        if let Some(entity) = focused_entity.or_else(fallback) {
            reshuffle_around(entity, &mut commands);
        }
    }
}

#[allow(clippy::needless_pass_by_value)]
#[instrument(level = Level::DEBUG, skip_all, fields(trigger))]
pub(super) fn workspace_destroyed_trigger(
    trigger: On<WMEventTrigger>,
    workspaces: Populated<(&LayoutStrip, Entity)>,
    mut commands: Commands,
) {
    let Event::SpaceDestroyed { space_id } = trigger.event().0 else {
        return;
    };

    let Some((_, entity)) = &workspaces
        .iter()
        .find(|(layout_strip, _)| layout_strip.id() == space_id)
    else {
        return;
    };

    if let Ok(mut entity_commands) = commands.get_entity(*entity) {
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
    let strip = LayoutStrip::new(space_id);
    let origin = Position(active_display.bounds().min);
    commands.spawn((strip, origin, ChildOf(display_entity)));
}

fn windows_not_in_strip<F: Fn(WinID) -> Option<Entity>>(
    workspace_id: WorkspaceId,
    find_window: F,
    strip: &LayoutStrip,
    window_manager: &WindowManager,
) -> Result<(Vec<Entity>, Vec<WinID>)> {
    window_manager
        .windows_in_workspace(workspace_id)
        .map(|ids| {
            let mut moved = Vec::new();
            let mut unresolved = Vec::new();
            for id in ids {
                match find_window(id) {
                    Some(entity) if !strip.contains(entity) => moved.push(entity),
                    Some(_) => {}
                    None => unresolved.push(id),
                }
            }
            (moved, unresolved)
        })
}

#[allow(clippy::needless_pass_by_value)]
#[instrument(level = Level::DEBUG, skip_all)]
pub(super) fn find_orphaned_workspaces(
    orphans: Populated<(&LayoutStrip, Entity, &Timeout, Option<&ChildOf>), With<Timeout>>,
    mut attached: Query<(&mut LayoutStrip, Entity, &ChildOf), Without<Timeout>>,
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

        let refresh_entity = if let Some((mut target_strip, strip_entity, _)) = attached
            .iter_mut()
            .find(|(strip, _, child)| child.parent() == target_entity && strip.id() == orphan.id())
        {
            // Move windows into existing workspace strip.
            debug!("moving windows into existing layout strip.");
            for entity in orphan.all_windows() {
                target_strip.append(entity);
            }
            if let Ok(mut cmd) = commands.get_entity(orphan_entity) {
                cmd.despawn();
            }
            strip_entity
        } else {
            // Display does not have this strip, add it.
            debug!("adding the layout strip directly.");
            if let Ok(mut commands) = commands.get_entity(orphan_entity) {
                commands
                    .try_remove::<Timeout>()
                    .insert(ChildOf(target_entity));
            }
            orphan_entity
        };

        if let Ok(mut cmd) = commands.get_entity(refresh_entity) {
            cmd.insert(RefreshWindowSizes::default());
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

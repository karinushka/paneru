use bevy::app::{App, Plugin, PreUpdate, Update};
use bevy::ecs::component::Component;
use bevy::ecs::entity::Entity;
use bevy::ecs::hierarchy::ChildOf;
use bevy::ecs::lifecycle::Add;
use bevy::ecs::message::MessageReader;
use bevy::ecs::observer::On;
use bevy::ecs::query::{Added, Has, With, Without};
use bevy::ecs::schedule::IntoScheduleConfigs as _;
use bevy::ecs::schedule::common_conditions::{not, resource_exists};
use bevy::ecs::system::{Commands, Local, ParamSet, Populated, Query, Res, ResMut, Single};
use bevy::time::common_conditions::on_timer;
use std::collections::HashSet;
use std::time::Duration;
use tracing::{Level, debug, error, instrument, warn};

use super::{ActiveDisplayMarker, SpawnWindowTrigger};
use crate::commands::{Direction, MoveFocus, Operation, filter_window_operations};
use crate::config::Config;
use crate::ecs::focus::FocusHistory;
use crate::ecs::layout::LayoutStrip;
use crate::ecs::params::{ActiveDisplay, Windows};
use crate::ecs::{
    ActiveWorkspaceMarker, Bounds, Initializing, NativeFullscreenMarker, Position,
    RefreshWindowSizes, SelectedVirtualMarker, SpawnCommandsExt, Timeout, Unmanaged,
};
use crate::errors::Result;
use crate::events::Event;
use crate::manager::{Application, Display, Origin, Window, WindowManager};
use crate::platform::{WinID, WorkspaceId};

pub struct WorkspaceEventsPlugin;

impl Plugin for WorkspaceEventsPlugin {
    fn build(&self, app: &mut App) {
        const REFRESH_WINDOW_CHECK_FREQ_MS: u64 = 1000;
        const DISPLAY_CHANGE_CHECK_FREQ_MS: u64 = 1000;

        let reap_workspaces = |config: Option<Res<Config>>| {
            config.is_some_and(|config| config.reap_empty_workspaces())
        };

        app.add_systems(
            PreUpdate,
            (switch_virtual_workspace_bind, move_virtual_workspace_bind),
        );
        app.add_systems(
            Update,
            (
                renumber_virtual_indexes,
                reap_empty_virtual_workspaces.run_if(reap_workspaces),
                workspace_change_trigger,
                workspace_created_trigger,
                workspace_destroyed_trigger,
                show_active_workspace,
                handle_virtual_window_moves,
                detect_moved_windows.run_if(not(resource_exists::<Initializing>)),
                refresh_workspace_window_sizes.run_if(on_timer(Duration::from_millis(
                    REFRESH_WINDOW_CHECK_FREQ_MS,
                ))),
                find_orphaned_workspaces
                    .after(crate::ecs::display::displays_rearranged)
                    .run_if(on_timer(Duration::from_millis(
                        DISPLAY_CHANGE_CHECK_FREQ_MS,
                    ))),
            ),
        );
        app.add_observer(cleanup_active_workspace_marker)
            .add_observer(cleanup_selected_space_marker);
    }
}

/// Marker component to move a window to a specific virtual index on its current workspace.
#[derive(Component)]
struct VirtualMoveMarker {
    pub target_virtual_index: u32,
    pub move_focus: MoveFocus,
}

#[derive(Component, Debug)]
pub(crate) struct PreviousStripPosition {
    pub origin: Origin,
    pub focus: Option<Entity>,
}

#[allow(clippy::needless_pass_by_value)]
#[instrument(level = Level::DEBUG, skip_all, fields(trigger))]
fn workspace_change_trigger(
    mut messages: MessageReader<Event>,
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
    if !messages
        .read()
        .any(|event| matches!(event, Event::SpaceChanged))
    {
        return;
    }
    let (active_display, display_entity) = *active_display;

    let Ok(workspace_id) = window_manager.active_display_space(active_display.id()) else {
        error!("Unable to get active workspace id!");
        return;
    };

    let mut remove_from = None;
    let mut insert_into = None;
    for (strip, entity, active, selected) in &workspaces {
        if active && strip.id() == workspace_id {
            debug!("Workspace id {} already active", strip.id());
            return;
        }
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

#[allow(clippy::needless_pass_by_value)]
#[instrument(level = Level::DEBUG, skip_all, fields(trigger))]
fn detect_moved_windows(
    activated_workspace: Single<Entity, Added<ActiveWorkspaceMarker>>,
    windows: Windows,
    mut workspaces: Query<(&mut LayoutStrip, Entity, Has<NativeFullscreenMarker>)>,
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
        .filter_map(|strip| (strip.0.id() == workspace_id).then_some(strip.0))
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
        let unresolved_ids = unresolved.iter().copied().collect::<HashSet<_>>();
        // Retry unresolved window IDs: during startup bruteforce, windows on
        // inactive workspaces may have stale AX attributes (e.g. AXGroup instead
        // of AXWindow).  Now that this workspace is active, re-query each app's
        // window list — the AX data should be correct.
        let retry_windows = apps
            .into_iter()
            .flat_map(|app| {
                app.window_list()
                    .into_iter()
                    .filter(|window| unresolved_ids.contains(&window.id()))
            })
            .collect::<Vec<_>>();
        if retry_windows.is_empty() {
            for id in unresolved_ids {
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
        let moving_entities = workspaces
            .iter()
            .find_map(|(strip, _, _)| strip.tab_group(entity))
            .unwrap_or_else(|| vec![entity]);
        for (mut strip, strip_entity, _) in &mut workspaces {
            if strip_entity == *activated_workspace {
                strip.append_tab_group(&moving_entities);
            } else {
                for moving_entity in &moving_entities {
                    strip.remove(*moving_entity);
                }
            }
        }
    }
}

#[allow(clippy::needless_pass_by_value)]
#[instrument(level = Level::DEBUG, skip_all, fields(trigger))]
fn workspace_destroyed_trigger(
    mut messages: MessageReader<Event>,
    mut workspaces: Populated<(&mut LayoutStrip, Entity, Option<&NativeFullscreenMarker>)>,
    mut focus_history: ResMut<FocusHistory>,
    mut commands: Commands,
) {
    for event in messages.read() {
        let Event::SpaceDestroyed { space_id } = event else {
            continue;
        };
        focus_history.forget_workspace(*space_id);

        let Some((entity, fullscreen)) =
            workspaces.iter().find_map(|(strip, entity, fullscreen)| {
                let window = strip.first().ok().and_then(|col| col.top());
                (strip.id() == *space_id).then_some((entity, window.zip(fullscreen.cloned())))
            })
        else {
            continue;
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
            commands.reshuffle_around(window);
        }

        if let Ok(mut entity_commands) = commands.get_entity(entity) {
            debug!("Workspace destroyed {space_id} {entity}");
            entity_commands.try_despawn();
        }
    }
}

#[allow(clippy::needless_pass_by_value)]
#[instrument(level = Level::DEBUG, skip_all, fields(trigger))]
fn workspace_created_trigger(
    mut messages: MessageReader<Event>,
    active_display: Single<(&Display, Entity), With<ActiveDisplayMarker>>,
    workspaces: Query<&LayoutStrip>,
    mut commands: Commands,
) {
    for event in messages.read() {
        let Event::SpaceCreated { space_id } = event else {
            continue;
        };

        if workspaces.into_iter().any(|strip| strip.id() == *space_id) {
            warn!("Workspace {space_id} already exists!");
            continue;
        }
        debug!("Workspace create {space_id}");
        let (active_display, display_entity) = *active_display;
        let strip = LayoutStrip::new(*space_id, 0);
        let origin = active_display.bounds().min;
        commands.spawn_layout_strip(strip, origin, display_entity, false);
    }
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
fn find_orphaned_workspaces(
    orphans: Populated<(&LayoutStrip, Entity, &Timeout, Option<&ChildOf>), With<Timeout>>,
    displays: Populated<(&Display, Entity)>,
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
fn refresh_workspace_window_sizes(
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
        commands.reposition_entity(window_entity, active_display.bounds().min);
    }

    if let Ok(mut cmds) = commands.get_entity(strip_entity) {
        cmds.try_remove::<RefreshWindowSizes>();
    }
}

/// Removes previuos `ActiveWorkspaceMarker`'s when a new one is inserted.
#[allow(clippy::needless_pass_by_value)]
#[instrument(level = Level::DEBUG, skip_all, fields(trigger))]
fn cleanup_active_workspace_marker(
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
fn cleanup_selected_space_marker(
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
fn handle_virtual_window_moves(
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
        let moving_entities = workspaces
            .get(source_entity)
            .ok()
            .and_then(|(_, strip, _, _)| strip.tab_group(window_entity))
            .unwrap_or_else(|| vec![window_entity]);
        for moving_entity in &moving_entities {
            commands
                .entity(*moving_entity)
                .remove::<VirtualMoveMarker>();
        }
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
            new_strip.append_tab_group(&moving_entities);

            let mut spawned = commands.spawn_layout_strip(new_strip, origin, display_entity, false);
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
                strip.append_tab_group(&moving_entities);
            } else {
                for moving_entity in &moving_entities {
                    strip.remove(*moving_entity);
                }
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
            commands.focus_entity(neighbour, false);
            commands.reshuffle_around(neighbour);
        } else {
            commands.reshuffle_around(window_entity);
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
fn switch_virtual_workspace_bind(
    mut messages: MessageReader<Event>,
    active_display: ActiveDisplay,
    workspaces: Query<(Entity, &LayoutStrip, Has<ActiveWorkspaceMarker>)>,
    config: Res<Config>,
    mut commands: Commands,
) {
    let Some(operation) = filter_window_operations(&mut messages, |op| {
        matches!(op, Operation::Virtual(_) | Operation::VirtualNumber(_))
    })
    .next() else {
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
    let next_index = match operation {
        Operation::Virtual(Direction::South) => (current_index + 1).clamp(0, rows.len() - 1),
        Operation::Virtual(Direction::North) => current_index.saturating_sub(1),
        Operation::VirtualNumber(target_virtual_index) => {
            let Some(index) = rows
                .iter()
                .position(|(_, strip, _)| strip.virtual_index == *target_virtual_index)
            else {
                if *target_virtual_index == 0 {
                    return;
                }
                commands.spawn_layout_strip(
                    LayoutStrip::new(workspace_id, *target_virtual_index),
                    active_display.bounds().min,
                    active_display.entity(),
                    true,
                );

                if config.workspace_popup_status() {
                    commands.flash_message(format!("{}", *target_virtual_index + 1), 1.0);
                }
                return;
            };
            index
        }
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

        if config.workspace_popup_status() {
            commands.flash_message(format!("{}", next_virtual_index + 1), 1.0);
        }
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
fn move_virtual_workspace_bind(
    mut messages: MessageReader<Event>,
    windows: Windows,
    active_display: ActiveDisplay,
    config: Res<Config>,
    mut commands: Commands,
) {
    let Some(operation) = filter_window_operations(&mut messages, |op| {
        matches!(
            op,
            Operation::VirtualMove(_, _) | Operation::VirtualMoveNumber(_, _)
        )
    })
    .next() else {
        return;
    };

    let Some((_, focused_entity)) = windows.focused() else {
        return;
    };

    let current_virtual_index = active_display.active_strip().virtual_index;

    let (target_virtual_index, move_focus) = match operation {
        Operation::VirtualMove(Direction::South, move_focus)
            if active_display.active_strip().len() > 1 =>
        {
            (current_virtual_index + 1, *move_focus)
        }
        Operation::VirtualMove(Direction::North, move_focus) => {
            if current_virtual_index == 0 {
                return;
            }
            (current_virtual_index - 1, *move_focus)
        }
        Operation::VirtualMoveNumber(target_virtual_index, move_focus) => {
            if *target_virtual_index == current_virtual_index {
                return;
            }
            (*target_virtual_index, *move_focus)
        }
        _ => return,
    };

    commands.entity(focused_entity).insert(VirtualMoveMarker {
        target_virtual_index,
        move_focus,
    });

    if move_focus == MoveFocus::Follow && config.workspace_popup_status() {
        commands.flash_message(format!("{}", target_virtual_index + 1), 1.0);
    }

    debug!("Moving {focused_entity} to new virtual space {target_virtual_index}");
}

#[allow(clippy::needless_pass_by_value)]
#[instrument(level = Level::DEBUG, skip_all)]
fn show_active_workspace(
    activated: Single<Entity, Added<ActiveWorkspaceMarker>>,
    windows: Windows,
    mut workspaces: Query<
        (
            Entity,
            &mut Position,
            &LayoutStrip,
            Option<&PreviousStripPosition>,
        ),
        Without<Window>,
    >,
    active_display: Single<&Display, With<ActiveDisplayMarker>>,
    mut commands: Commands,
) {
    let Some(workspace_id) = workspaces
        .iter()
        .find_map(|(entity, _, strip, _)| (entity == *activated).then_some(strip.id()))
    else {
        return;
    };

    // Hide other strips on the current workspace
    let current_focus = windows.focused();
    let current_workspace = workspaces
        .iter_mut()
        .filter(|(entity, _, strip, _)| strip.id() == workspace_id && *entity != *activated);

    for (entity, mut position, strip, previous) in current_workspace {
        if previous.is_none() {
            let mut focus =
                current_focus.and_then(|(_, entity)| strip.contains(entity).then_some(entity));
            if focus.is_none() {
                let display_center = active_display.bounds().center().x;
                let closest = strip
                    .all_columns()
                    .into_iter()
                    .filter_map(|candidate| {
                        let center = windows.moving_frame(candidate)?.center().x;
                        let distance = (center - display_center).abs();
                        Some((candidate, distance))
                    })
                    .min_by_key(|(_, dist)| *dist)
                    .map(|min| min.0);
                focus = closest;
                debug!("No previous focus, taking centered one {focus:?}.");
            }

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

    let Ok((_, mut position, strip, previous_position)) = workspaces.get_mut(*activated) else {
        return;
    };
    debug!("showing virtual workspace {} ({})", strip.id(), *activated);

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
            commands.focus_entity(*entity, false);
        }
    }
}

/// Resolves duplicate `virtual_index` values by reassigning each
/// duplicate to the lowest unused index on its workspace. Triggered by
/// `Added<LayoutStrip>` — the only event that can introduce a duplicate
/// (despawning a strip can leave a gap but never collides). Runs
/// independently of `reap_empty_workspaces` because without it, duplicate
/// indices silently break navigation: `switch_virtual_workspace_bind`
/// sorts rows by `virtual_index` and flashes `next_virtual_index + 1` as
/// the OSD label, so two rows both at 0 mean South moves between them
/// while the OSD stays at "1" and North no-ops at the bottom of the
/// saturating sub.
///
/// Sources of duplicate creation we have to defend against:
/// - `LayoutStrip::fullscreen` pins `virtual_index` to 0 unconditionally.
/// - Restoration trusts whatever the saved state contained.
/// - Races in `handle_virtual_window_moves` between checking the target
///   and spawning a new strip for it.
///
/// Gaps in the index sequence are preserved — only duplicates get
/// renumbered. Single-strip configurations at non-zero indices stay
/// where they are.
#[allow(clippy::type_complexity)]
fn renumber_virtual_indexes(
    mut set: ParamSet<(
        Query<&LayoutStrip, Added<LayoutStrip>>,
        Query<(Entity, &mut LayoutStrip)>,
    )>,
) {
    let affected: HashSet<WorkspaceId> = set.p0().iter().map(LayoutStrip::id).collect();
    if affected.is_empty() {
        return;
    }

    let mut strips = set.p1();
    for workspace_id in affected {
        let mut rows = strips
            .iter_mut()
            .filter(|(_, strip)| strip.id() == workspace_id)
            .collect::<Vec<_>>();
        rows.sort_by_key(|(_, strip)| strip.virtual_index);

        let mut taken: HashSet<u32> = HashSet::new();
        let mut dups: Vec<usize> = Vec::new();
        for (i, (_, strip)) in rows.iter().enumerate() {
            if !taken.insert(strip.virtual_index) {
                dups.push(i);
            }
        }
        if dups.is_empty() {
            continue;
        }

        // Reassign each dup to the lowest free index, growing `taken` as
        // we go so the next dup doesn't pick the same slot.
        for i in dups {
            let mut probe = 0u32;
            while !taken.insert(probe) {
                probe += 1;
            }
            rows[i].1.virtual_index = probe;
        }
    }
}

#[allow(clippy::needless_pass_by_value)]
fn reap_empty_virtual_workspaces(
    changed: Single<Entity, Added<ActiveWorkspaceMarker>>,
    strips: Populated<(Entity, &LayoutStrip)>,
    mut commands: Commands,
) {
    let changed_entity = *changed;
    let Some(workspace_id) = strips.get(changed_entity).ok().map(|(_, strip)| strip.id()) else {
        return;
    };
    debug!("cleaning up virtual workspaces on space {workspace_id}");
    let mut rows = strips
        .iter()
        .filter(|(_, strip)| strip.id() == workspace_id)
        .collect::<Vec<_>>();
    rows.sort_by_key(|(_, strip)| strip.virtual_index);

    if rows.is_empty() {
        return;
    }

    let primary_entity = rows[0].0;
    for (entity, strip) in rows {
        if strip.virtual_index > 0 && strip.len() == 0 {
            if entity == changed_entity {
                debug!("moving markers from despawned virtual workspace to primary");
                commands
                    .entity(primary_entity)
                    .try_insert(ActiveWorkspaceMarker)
                    .try_insert(SelectedVirtualMarker);
            }
            commands.entity(entity).despawn();
        }
    }
}

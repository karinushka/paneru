use bevy::app::{App, Plugin, Update};
use bevy::ecs::entity::Entity;
use bevy::ecs::hierarchy::ChildOf;
use bevy::ecs::lifecycle::Add;
use bevy::ecs::message::MessageReader;
use bevy::ecs::observer::On;
use bevy::ecs::query::{Has, With};
use bevy::ecs::system::{Commands, Query, Res};
use bevy::math::IRect;
use objc2_core_graphics::CGDirectDisplayID;
use std::time::Duration;
use tracing::{Level, debug, error, instrument};

use crate::config::Config;
use crate::ecs::layout::LayoutStrip;
use crate::ecs::{
    ActiveDisplayMarker, Position, RefreshWindowSizes, SelectedVirtualMarker, Timeout,
    WMEventTrigger,
};
use crate::events::Event;
use crate::manager::{Display, WindowManager};
use crate::platform::WorkspaceId;

const ORPHANED_SPACES_TIMEOUT_SEC: u64 = 30;

pub struct DisplayEventsPlugin;

impl Plugin for DisplayEventsPlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(Update, displays_rearranged)
            .add_observer(display_change_trigger)
            .add_observer(cleanup_active_display_marker);
    }
}

#[allow(clippy::needless_pass_by_value)]
#[instrument(level = Level::DEBUG, skip_all, fields(trigger))]
fn cleanup_active_display_marker(
    trigger: On<Add, ActiveDisplayMarker>,
    displays: Query<(Entity, Has<ActiveDisplayMarker>), With<Display>>,
    mut commands: Commands,
) {
    for (entity, active) in displays {
        if active
            && entity != trigger.entity
            && let Ok(mut cmd) = commands.get_entity(entity)
        {
            debug!("Display id {entity} lost active marker.");
            cmd.try_remove::<ActiveDisplayMarker>();
        }
    }
}

/// Handles display change events.
#[allow(clippy::needless_pass_by_value)]
#[instrument(level = Level::DEBUG, skip_all, fields(trigger))]
fn display_change_trigger(
    trigger: On<WMEventTrigger>,
    displays: Query<(&Display, Entity, Has<ActiveDisplayMarker>)>,
    window_manager: Res<WindowManager>,
    mut commands: Commands,
) {
    let Event::DisplayChanged = trigger.event().0 else {
        return;
    };

    let Ok(active_id) = window_manager.active_display_id() else {
        error!("Unable to get active display id!");
        return;
    };

    for (display, entity, focused) in displays {
        let display_id = display.id();
        if !focused
            && display_id == active_id
            && let Ok(mut cmd) = commands.get_entity(entity)
        {
            debug!("Display id {display_id} is active");
            cmd.try_insert(ActiveDisplayMarker);
        }
    }
    commands.trigger(WMEventTrigger(Event::SpaceChanged));
}

#[allow(clippy::needless_pass_by_value)]
pub(crate) fn displays_rearranged(
    mut messages: MessageReader<Event>,
    workspaces: Query<(&LayoutStrip, Entity, Option<&ChildOf>)>,
    mut displays: Query<(&mut Display, Entity)>,
    window_manager: Res<WindowManager>,
    config: Res<Config>,
    mut commands: Commands,
) {
    for event in messages.read() {
        match event {
            Event::DisplayAdded { display_id } => {
                add_display(
                    *display_id,
                    &workspaces,
                    &window_manager,
                    &config,
                    &mut commands,
                );
            }
            Event::DisplayRemoved { display_id } => {
                remove_display(*display_id, &workspaces, &mut displays, &mut commands);
            }
            Event::DisplayMoved { display_id } => {
                move_display(
                    *display_id,
                    &mut displays,
                    &window_manager,
                    &workspaces,
                    &config,
                    &mut commands,
                );
            }
            _ => continue,
        }
        commands.trigger(WMEventTrigger(Event::DisplayChanged));
    }
}

#[instrument(level = Level::DEBUG, skip_all, fields(display_id))]
fn add_display(
    display_id: CGDirectDisplayID,
    existing_strips: &Query<(&LayoutStrip, Entity, Option<&ChildOf>)>,
    window_manager: &WindowManager,
    config: &Config,
    commands: &mut Commands,
) {
    debug!("Display Added: {display_id:?}");
    let Some((mut display, workspace_ids)) = window_manager
        .0
        .present_displays()
        .into_iter()
        .find(|(display, _)| display.id() == display_id)
    else {
        error!("Unable to find added display id {display_id}!");
        return;
    };

    display.set_menubar_height_override(config.menubar_height());
    let display_bounds = display.bounds();
    let display_entity = commands.spawn(display).id();

    reparent_existing_workspaces(
        &workspace_ids,
        display_entity,
        &display_bounds,
        existing_strips,
        commands,
    );
}

#[instrument(level = Level::DEBUG, skip_all, fields(display_id))]
fn remove_display(
    display_id: CGDirectDisplayID,
    workspaces: &Query<(&LayoutStrip, Entity, Option<&ChildOf>)>,
    displays: &mut Query<(&mut Display, Entity)>,
    commands: &mut Commands,
) {
    debug!("Display Removed: {display_id:?}");
    let Some((display, display_entity)) = displays
        .into_iter()
        .find(|(display, _)| display.id() == display_id)
    else {
        error!("Unable to find removed display!");
        return;
    };

    for (strip, entity, _) in workspaces
        .into_iter()
        .filter(|(_, _, child)| child.is_some_and(|child| child.parent() == display_entity))
    {
        let display_id = display.id();
        debug!(
            "orphaning strip {} after removal of display {display_id}.",
            strip.id(),
        );
        let timeout = Timeout::new(
            Duration::from_secs(ORPHANED_SPACES_TIMEOUT_SEC),
            Some(format!(
                "Orphaned strip {} ({strip}) could not be re-inserted after {ORPHANED_SPACES_TIMEOUT_SEC}s.",
                strip.id()
            )),
        );
        if let Ok(mut commands) = commands.get_entity(entity) {
            commands.try_insert(timeout);
        }
        if let Ok(mut commands) = commands.get_entity(display_entity) {
            commands.detach_child(entity);
        }
    }

    if let Ok(mut commands) = commands.get_entity(display_entity) {
        commands.despawn();
    }
}

#[instrument(level = Level::DEBUG, skip_all, fields(display_id))]
fn move_display(
    display_id: CGDirectDisplayID,
    displays: &mut Query<(&mut Display, Entity)>,
    window_manager: &Res<WindowManager>,
    existing_strips: &Query<(&LayoutStrip, Entity, Option<&ChildOf>)>,
    config: &Config,
    commands: &mut Commands,
) {
    debug!("Display Moved: {display_id:?}");
    let Some((mut display, display_entity)) = displays
        .iter_mut()
        .find(|(display, _)| display.id() == display_id)
    else {
        error!("Unable to find moved display!");
        return;
    };
    let Some((moved_display, workspace_ids)) = window_manager
        .0
        .present_displays()
        .into_iter()
        .find(|(display, _)| display.id() == display_id)
    else {
        return;
    };
    *display = moved_display;
    display.set_menubar_height_override(config.menubar_height());

    reparent_existing_workspaces(
        &workspace_ids,
        display_entity,
        &display.bounds(),
        existing_strips,
        commands,
    );
}

fn reparent_existing_workspaces(
    workspace_ids: &[WorkspaceId],
    display_entity: Entity,
    display_bounds: &IRect,
    existing_strips: &Query<(&LayoutStrip, Entity, Option<&ChildOf>)>,
    commands: &mut Commands,
) {
    // Verifies that a moved display has all the workspaces which it owns.
    for &id in workspace_ids {
        let mut found = false;
        for (strip, entity, child) in existing_strips {
            if strip.id() == id {
                found = true;
                if child.is_none_or(|child| child.parent() != display_entity) {
                    // Re-parent this workspace
                    if let Ok(mut cmd) = commands.get_entity(entity) {
                        debug!("reparenting workspace {id} to display {display_entity}");
                        cmd.try_remove::<Timeout>()
                            .try_remove::<ChildOf>()
                            .insert(ChildOf(display_entity));

                        cmd.insert(RefreshWindowSizes::default());
                    }
                }
            }
        }

        if !found {
            // New workspace.
            let origin = Position(display_bounds.min);
            debug!("new workspace {id} on display {display_entity}");
            commands.spawn((
                origin.clone(),
                LayoutStrip::new(id, 0),
                SelectedVirtualMarker,
                ChildOf(display_entity),
            ));
        }
    }
}

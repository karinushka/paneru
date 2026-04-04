use bevy::ecs::entity::Entity;
use bevy::ecs::lifecycle::{Add, Remove};
use bevy::ecs::observer::On;
use bevy::ecs::query::{Has, With};
use bevy::ecs::system::{Commands, Query, Res};
use tracing::{Level, debug, instrument, warn};

use super::{FocusedMarker, MouseHeldMarker, SystemTheme};
use crate::config::Config;
use crate::ecs::layout::LayoutStrip;
use crate::ecs::params::{ActiveDisplay, Configuration, Windows};
use crate::ecs::{
    ActiveWorkspaceMarker, Scrolling, SelectedVirtualMarker, reposition_entity, reshuffle_around,
};
use crate::manager::{Window, WindowManager};

#[allow(clippy::needless_pass_by_value)]
#[instrument(level = Level::DEBUG, skip_all, fields(trigger))]
pub(super) fn maintain_focus_singleton(
    trigger: On<Add, FocusedMarker>,
    windows: Query<(Entity, Has<FocusedMarker>), With<Window>>,
    mut config: Configuration,
    mut commands: Commands,
) {
    let focused_entity = trigger.event().entity;

    for (entity, focused) in windows {
        if focused
            && entity != focused_entity
            && let Ok(mut entity_commands) = commands.get_entity(entity)
        {
            debug!("window {entity} lost focus.");
            entity_commands.try_remove::<FocusedMarker>();
        }
    }

    // Check if the reshuffle was caused by a keyboard switch or mouse move.
    // Skip reshuffle if caused by mouse - because then it won't center.
    if config.ffm_flag().is_none() {
        config.set_skip_reshuffle(false);
    }
    config.set_ffm_flag(None);
}

#[allow(clippy::needless_pass_by_value)]
#[instrument(level = Level::DEBUG, skip_all, fields(trigger))]
pub(super) fn autocenter_on_focus(
    trigger: On<Add, FocusedMarker>,
    mouse_held: Query<&MouseHeldMarker>,
    windows: Windows,
    config: Configuration,
    active_display: ActiveDisplay,
    mut commands: Commands,
) {
    let entity = trigger.event().entity;

    if config.skip_reshuffle() || config.initializing() || !mouse_held.is_empty() {
        return;
    }
    if config.auto_center()
        && let Some((_, _, None)) = windows.get_managed(entity)
        && let Some(size) = windows.size(entity)
        && let Some(mut origin) = windows.origin(entity)
    {
        let center = active_display.bounds().center();
        origin.x = center.x - size.x / 2;
        reposition_entity(entity, origin, &mut commands);
    }
    reshuffle_around(entity, &mut commands);
}

#[allow(clippy::needless_pass_by_value)]
#[instrument(level = Level::DEBUG, skip_all, fields(trigger))]
pub(super) fn center_mouse_trigger(
    trigger: On<Add, FocusedMarker>,
    windows: Windows,
    config: Configuration,
    active_display: ActiveDisplay,
    window_manager: Res<WindowManager>,
    active_workspace: Query<&Scrolling, With<ActiveWorkspaceMarker>>,
) {
    let entity = trigger.event().entity;
    let Some(window) = windows.get(entity) else {
        return;
    };
    if active_workspace
        .iter()
        .next()
        .is_some_and(|scrolling| scrolling.is_user_swiping)
    {
        debug!("Suppressing center mouse due to a swipe");
        return;
    }

    debug!(
        "window {}, skip_reshuffle {}, ffm flag {:?}.",
        window.id(),
        config.skip_reshuffle(),
        config.ffm_flag()
    );
    if config.mouse_follows_focus()
        && !config.skip_reshuffle()
        && config.ffm_flag().is_none_or(|id| id != window.id())
        && let Some(frame) = windows.moving_frame(entity)
    {
        let display_bounds = active_display.bounds();
        let visible = display_bounds.intersect(frame);
        let origin = visible.center();
        debug!("centering on {} {origin}", window.id());
        window_manager.warp_mouse(origin);
    }
}

#[allow(clippy::needless_pass_by_value)]
pub(super) fn dim_window_trigger(
    trigger: On<Add, FocusedMarker>,
    windows: Windows,
    window_manager: Res<WindowManager>,
    config: Res<Config>,
    theme: Option<Res<SystemTheme>>,
) {
    let Some(window) = windows.get(trigger.event().entity) else {
        return;
    };

    let dark = theme.is_some_and(|theme| theme.is_dark);
    if config.window_dim_ratio(dark).is_some() {
        window_manager.dim_windows(&[window.id()], 0.0);
    }
}

#[allow(clippy::needless_pass_by_value)]
pub(super) fn dim_remove_window_trigger(
    trigger: On<Remove, FocusedMarker>,
    windows: Windows,
    window_manager: Res<WindowManager>,
    config: Res<Config>,
    theme: Option<Res<SystemTheme>>,
) {
    let Some(window) = windows.get(trigger.event().entity) else {
        return;
    };

    let dark = theme.is_some_and(|theme| theme.is_dark);
    if let Some(dim_ratio) = config.window_dim_ratio(dark) {
        window_manager.dim_windows(&[window.id()], dim_ratio);
    }
}

#[allow(clippy::needless_pass_by_value)]
#[instrument(level = Level::DEBUG, skip_all, fields(trigger))]
pub(super) fn virtual_strip_activated(
    trigger: On<Add, FocusedMarker>,
    workspaces: Query<(Entity, &LayoutStrip, Has<ActiveWorkspaceMarker>)>,
    mut commands: Commands,
) {
    let Some((_, active_strip, _)) = workspaces.iter().find(|(_, _, active)| *active) else {
        return;
    };
    if active_strip.contains(trigger.entity) {
        return;
    }

    for (entity, strip, _) in workspaces {
        if strip.contains(trigger.entity)
            && let Ok(mut entity_commands) = commands.get_entity(entity)
        {
            entity_commands
                .try_insert(ActiveWorkspaceMarker)
                .try_insert(SelectedVirtualMarker);
        }
    }
}

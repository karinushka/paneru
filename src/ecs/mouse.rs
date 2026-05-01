use bevy::ecs::entity::Entity;
use bevy::ecs::observer::On;
use bevy::ecs::query::With;
use bevy::ecs::system::{Commands, Query, Res};
use std::time::Duration;
use tracing::{debug, trace, warn};

use super::{MissionControlActive, MouseHeldMarker, Timeout, WMEventTrigger};
use crate::config::Config;
use crate::ecs::params::{Configuration, Windows};
use crate::ecs::{ActiveWorkspaceMarker, Scrolling, focus_entity, reshuffle_around};
use crate::events::Event;
use crate::manager::{Display, WindowManager, origin_from};

/// Handles mouse moved events.
///
/// If "focus follows mouse" is enabled, this function finds the window under the cursor and
/// focuses it. It also handles child windows like sheets and drawers to ensure the correct
/// window receives focus.
///
/// # Arguments
///
/// * `trigger` - The Bevy event trigger containing the mouse moved event.
/// * `windows` - A query for all windows.
/// * `focused_window` - A query for the currently focused window.
/// * `main_cid` - The main connection ID resource.
/// * `config` - The optional configuration resource.
#[allow(clippy::needless_pass_by_value)]
pub(super) fn mouse_moved_trigger(
    trigger: On<WMEventTrigger>,
    windows: Windows,
    window_manager: Res<WindowManager>,
    mut config: Configuration,
    mut commands: Commands,
) {
    let Event::MouseMoved {
        point,
        modifiers: _,
    } = trigger.event().0
    else {
        return;
    };

    if !config.focus_follows_mouse() {
        return;
    }
    if config.mission_control_active() {
        return;
    }
    if config.ffm_flag().is_some() {
        trace!("ffm_window_id > 0");
        return;
    }
    let Ok(window_id) = window_manager.find_window_at_point(&point) else {
        debug!("can not find window at point {point:?}");
        return;
    };
    if windows
        .focused()
        .is_some_and(|(window, _)| window.id() == window_id)
    {
        trace!("allready focused {window_id}");
        return;
    }
    let Some((window, entity)) = windows.find(window_id) else {
        trace!("can not find focused window: {window_id}");
        return;
    };

    let child_window = window_manager
        .get_associated_windows(window_id)
        .into_iter()
        .find_map(|child_wid| {
            windows.find(child_wid).and_then(|(window, _)| {
                window
                    .child_role()
                    .inspect_err(|err| {
                        warn!("getting role {window_id}: {err}");
                    })
                    .is_ok_and(|child| child)
                    .then_some(window)
            })
        });
    if let Some(child) = child_window {
        debug!("found child of {}: {}", child.id(), window.id());
    }

    // Do not reshuffle windows due to moved mouse focus.
    config.set_skip_reshuffle(true);
    config.set_ffm_flag(Some(window.id()));
    focus_entity(entity, false, &mut commands);
}

/// Handles mouse down events.
///
/// This function finds the window at the click point. If the window is not fully visible,
/// it triggers a reshuffle to expose it.
///
/// # Arguments
///
/// * `trigger` - The Bevy event trigger containing the mouse down event.
/// * `windows` - A query for all windows.
/// * `active_display` - A query for the active display.
/// * `main_cid` - The main connection ID resource.
/// * `mission_control_active` - A resource indicating if Mission Control is active.
/// * `commands` - Bevy commands to trigger a reshuffle.
#[allow(clippy::needless_pass_by_value, clippy::too_many_arguments)]
pub(super) fn mouse_down_trigger(
    trigger: On<WMEventTrigger>,
    windows: Windows,
    active_workspace: Query<(Entity, Option<&Scrolling>), With<ActiveWorkspaceMarker>>,
    window_manager: Res<WindowManager>,
    mission_control_active: Res<MissionControlActive>,
    config: Configuration,
    mouse_held: Query<Entity, With<MouseHeldMarker>>,
    mut commands: Commands,
) {
    let Event::MouseDown {
        point,
        modifiers: _,
    } = trigger.event().0
    else {
        return;
    };
    if mission_control_active.0 {
        return;
    }
    trace!("{point:?}");

    let Some((_, entity)) = window_manager
        .find_window_at_point(&point)
        .ok()
        .and_then(|window_id| windows.find(window_id))
    else {
        return;
    };

    // Stop any ongoing scroll.
    for (entity, scroll) in active_workspace {
        if scroll.is_some() {
            commands.entity(entity).try_remove::<Scrolling>();
        }
    }

    // Clean up any stale marker from a previous click.
    for held in &mouse_held {
        commands.entity(held).despawn();
    }

    if config.window_hidden_ratio() >= 1.0 {
        // At max hidden ratio, never reshuffle on click.
    } else {
        // Defer reshuffle until mouse-up so the window doesn't shift
        // mid-click. The Timeout auto-despawns if mouse-up is lost.
        let timeout = Timeout::new(Duration::from_secs(5), None);
        commands.spawn((MouseHeldMarker(entity), timeout));
    }
}

/// Handles mouse-up events. Triggers the deferred reshuffle so the clicked
/// window slides into view after the user releases the button.
#[allow(clippy::needless_pass_by_value)]
pub(super) fn mouse_up_trigger(
    trigger: On<WMEventTrigger>,
    mouse_held: Query<(Entity, &MouseHeldMarker)>,
    mut commands: Commands,
) {
    let Event::MouseUp {
        point: _,
        modifiers: _,
    } = trigger.event().0
    else {
        return;
    };
    for (held_entity, marker) in &mouse_held {
        reshuffle_around(marker.0, &mut commands);
        commands.entity(held_entity).despawn();
    }
}

#[allow(clippy::needless_pass_by_value)]
pub(super) fn horizontal_warp_mouse_trigger(
    trigger: On<WMEventTrigger>,
    displays: Query<&Display>,
    window_manager: Res<WindowManager>,
    config: Res<Config>,
) {
    const EDGE_THRESHOLD: i32 = 3;
    let Event::MouseMoved {
        point,
        modifiers: _,
    } = trigger.event().0
    else {
        return;
    };
    let Some(warp_direction) = config.horizontal_mouse_warp() else {
        return;
    };
    if displays.count() < 2 {
        return;
    }

    let point = origin_from(point);
    let Some(current_display) = displays
        .iter()
        .find(|display| display.bounds().contains(point))
    else {
        return;
    };

    let mut target_displays = if (point.x - current_display.bounds().min.x).abs() < EDGE_THRESHOLD {
        // Left edge of the screen.
        displays
            .iter()
            .filter(|display| {
                if warp_direction > 0 {
                    display.bounds().min.y > current_display.bounds().min.y
                } else {
                    display.bounds().min.y < current_display.bounds().min.y
                }
            })
            .collect::<Vec<_>>()
    } else if (current_display.bounds().max.x - point.x).abs() < EDGE_THRESHOLD {
        // Right edge of the screen.
        displays
            .iter()
            .filter(|display| {
                if warp_direction > 0 {
                    display.bounds().min.y < current_display.bounds().min.y
                } else {
                    display.bounds().min.y > current_display.bounds().min.y
                }
            })
            .collect::<Vec<_>>()
    } else {
        return;
    };

    target_displays
        .sort_by_key(|display| (display.bounds().min.y - current_display.bounds().min.y).abs());
    if let Some(warp_to) = target_displays.first() {
        window_manager.warp_mouse(warp_to.bounds().center());
    }
}

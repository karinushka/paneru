use bevy::app::{App, Plugin, Update};
use bevy::ecs::entity::Entity;
use bevy::ecs::message::MessageReader;
use bevy::ecs::query::With;
use bevy::ecs::schedule::IntoScheduleConfigs as _;
use bevy::ecs::system::{Commands, Local, Query, Res, Single};
use std::time::{Duration, Instant};
use tracing::{debug, trace, warn};

use super::{MouseHeldMarker, Timeout};
use crate::config::Config;
use crate::ecs::layout::LayoutStrip;
use crate::ecs::params::{GlobalState, Windows};
use crate::ecs::{
    ActiveWorkspaceMarker, MissionControlActive, Position, Scrolling, focus_entity,
    reposition_entity, reshuffle_around, resize_entity,
};
use crate::events::Event;
use crate::manager::{Display, Origin, WindowManager, origin_from};
use crate::platform::WinID;

pub struct MouseEventsPlugin;

impl Plugin for MouseEventsPlugin {
    fn build(&self, app: &mut App) {
        let mission_control_inactive = |mission_control: Option<Res<MissionControlActive>>| {
            mission_control.is_none_or(|active| !active.0)
        };

        app.add_systems(
            Update,
            (
                (
                    mouse_moved_trigger,
                    mouse_resize_trigger,
                    mouse_down_trigger,
                )
                    .run_if(mission_control_inactive),
                mouse_up_trigger,
                horizontal_warp_mouse_trigger,
            ),
        );
    }
}

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
fn mouse_moved_trigger(
    mut messages: MessageReader<Event>,
    windows: Windows,
    window_manager: Res<WindowManager>,
    config: Res<Config>,
    mut global_state: GlobalState,
    mut commands: Commands,
) {
    for event in messages.read() {
        let Event::MouseMoved { point, modifiers } = event else {
            continue;
        };

        if config
            .mouse_resize_modifier()
            .is_some_and(|modifier| modifier.matches(*modifiers))
        {
            // Resizing is handled by a separate trigger or logic.
            // For now, let's just intercept it here to prevent focus changes during resize.
            continue;
        }

        if !config.focus_follows_mouse() {
            continue;
        }
        if global_state.ffm_flag().is_some() {
            trace!("ffm_window_id > 0");
            continue;
        }
        let Ok(window_id) = window_manager.find_window_at_point(point) else {
            debug!("can not find window at point {point:?}");
            continue;
        };
        if windows
            .focused()
            .is_some_and(|(window, _)| window.id() == window_id)
        {
            trace!("allready focused {window_id}");
            continue;
        }
        let Some((window, entity)) = windows.find(window_id) else {
            trace!("can not find focused window: {window_id}");
            continue;
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
        global_state.set_skip_reshuffle(true);
        global_state.set_ffm_flag(Some(window.id()));
        focus_entity(entity, false, &mut commands);
    }
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
/// * `commands` - Bevy commands to trigger a reshuffle.
#[allow(clippy::needless_pass_by_value, clippy::too_many_arguments)]
fn mouse_down_trigger(
    mut messages: MessageReader<Event>,
    windows: Windows,
    active_workspace: Query<(Entity, Option<&Scrolling>), With<ActiveWorkspaceMarker>>,
    window_manager: Res<WindowManager>,
    config: Res<Config>,
    mouse_held: Query<Entity, With<MouseHeldMarker>>,
    mut commands: Commands,
) {
    for event in messages.read() {
        let Event::MouseDown { point, .. } = event else {
            continue;
        };
        trace!("{point:?}");

        let Some((_, entity)) = window_manager
            .find_window_at_point(point)
            .ok()
            .and_then(|window_id| windows.find(window_id))
        else {
            continue;
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
            let timeout = Timeout::new(Duration::from_secs(5), None, &mut commands);
            commands.spawn((MouseHeldMarker(entity), timeout));
        }
    }
}

/// Handles mouse-up events. Triggers the deferred reshuffle so the clicked
/// window slides into view after the user releases the button.
#[allow(clippy::needless_pass_by_value)]
fn mouse_up_trigger(
    mut messages: MessageReader<Event>,
    mouse_held: Query<(Entity, &MouseHeldMarker)>,
    mut commands: Commands,
) {
    for event in messages.read() {
        if !matches!(event, Event::MouseUp { .. }) {
            continue;
        }

        for (held_entity, marker) in &mouse_held {
            reshuffle_around(marker.0, &mut commands);
            commands.entity(held_entity).despawn();
        }
    }
}

#[derive(Default)]
pub(super) struct MouseResizeState {
    last_point: Option<Origin>,
    window_id: Option<WinID>,
}

#[allow(clippy::needless_pass_by_value)]
fn mouse_resize_trigger(
    mut messages: MessageReader<Event>,
    windows: Windows,
    active_workspace: Single<(Entity, &LayoutStrip, &Position), With<ActiveWorkspaceMarker>>,
    window_manager: Res<WindowManager>,
    config: Res<Config>,
    mut state: Local<MouseResizeState>,
    mut commands: Commands,
) {
    for event in messages.read() {
        let Event::MouseMoved { point, modifiers } = event else {
            continue;
        };

        if config
            .mouse_resize_modifier()
            .is_none_or(|modifier| !modifier.matches(*modifiers))
        {
            state.last_point = None;
            state.window_id = None;
            continue;
        }
        let pointer = origin_from(*point);

        let Some(last_point) = state.last_point else {
            state.last_point = Some(pointer);
            continue;
        };
        state.last_point = Some(pointer);

        let dx = (pointer.x - last_point.x) * 5;
        if dx.abs() < 1 {
            continue;
        }

        let Ok(window_id) = window_manager.find_window_at_point(point) else {
            continue;
        };
        if state.window_id.is_some_and(|id| window_id != id) {
            continue;
        }
        state.window_id = Some(window_id);

        let Some((window, entity)) = windows.find(window_id) else {
            continue;
        };
        let (strip_entity, strip, strip_position) = *active_workspace;
        let floating = !strip.contains(entity);

        let mut frame = window.frame();
        let center = frame.center();

        if pointer.x < center.x {
            if floating && let Some(mut origin) = windows.origin(entity) {
                // For floating windows, move the window itself.
                origin.x += dx;
                reposition_entity(entity, origin, &mut commands);
            } else {
                // Resize Left Edge: increase/decrease width AND shift the strip so the right edge stays
                // anchored.
                let mut origin = strip_position.0;
                origin.x += dx;
                reposition_entity(strip_entity, origin, &mut commands);
            }

            frame.min.x += dx;
        } else {
            frame.max.x += dx;
        }
        resize_entity(entity, frame.size(), &mut commands);
    }
}

#[derive(Default)]
pub(super) struct WarpVelocityState {
    last: Option<(Origin, Instant)>,
}

#[allow(clippy::needless_pass_by_value)]
fn horizontal_warp_mouse_trigger(
    mut messages: MessageReader<Event>,
    displays: Query<&Display>,
    window_manager: Res<WindowManager>,
    config: Res<Config>,
    mut state: Local<WarpVelocityState>,
) {
    const EDGE_THRESHOLD: i32 = 3;
    /// Inset from the destination display's edge so the cursor doesn't land
    /// directly on the threshold and immediately re-warp back.
    const LANDING_INSET: i32 = 6;
    /// Extrapolate pre-warp horizontal motion by this duration so the cursor
    /// does not feel like it starts from rest on the target display.
    const CARRY_DURATION: Duration = Duration::from_millis(30);
    /// Cap on how far the carry-over can push past the inset, in pixels.
    const MAX_CARRY_PX: i32 = 80;
    /// Stale velocity samples (e.g. from a prior gesture) shouldn't carry.
    const VELOCITY_FRESHNESS: Duration = Duration::from_millis(80);

    for event in messages.read() {
        let Event::MouseMoved { point, .. } = event else {
            continue;
        };

        let now = Instant::now();
        let point = origin_from(*point);

        // Compute velocity from the previous sample before deciding whether to
        // warp, then refresh the sample so subsequent events build on this one.
        let velocity_x = state.last.and_then(|(prev, t)| {
            let dt = now.saturating_duration_since(t);
            if dt.is_zero() || dt > VELOCITY_FRESHNESS {
                return None;
            }
            let dx = f64::from(point.x - prev.x);
            Some(dx / dt.as_secs_f64())
        });
        state.last = Some((point, now));

        let Some(warp_direction) = config.horizontal_mouse_warp() else {
            return;
        };
        if displays.count() < 2 {
            return;
        }

        let Some(current_display) = displays
            .iter()
            .find(|display| display.bounds().contains(point))
        else {
            return;
        };

        let on_left_edge = (point.x - current_display.bounds().min.x).abs() < EDGE_THRESHOLD;
        let on_right_edge = (current_display.bounds().max.x - point.x).abs() < EDGE_THRESHOLD;
        if !on_left_edge && !on_right_edge {
            return;
        }

        let mut target_displays = displays
            .iter()
            .filter(|display| {
                let above = display.bounds().min.y < current_display.bounds().min.y;
                let below = display.bounds().min.y > current_display.bounds().min.y;
                if on_left_edge {
                    if warp_direction > 0 { below } else { above }
                } else if warp_direction > 0 {
                    above
                } else {
                    below
                }
            })
            .collect::<Vec<_>>();

        target_displays
            .sort_by_key(|display| (display.bounds().min.y - current_display.bounds().min.y).abs());
        let Some(warp_to) = target_displays.first() else {
            return;
        };
        let target = warp_to.bounds();

        // Land at the *opposite* edge so the cursor flow is continuous: leaving
        // the right edge appears at the left edge of the target, and vice versa.
        // Carry over horizontal velocity so the cursor does not feel "stuck" at
        // the edge — extrapolate motion forward into the target display.
        let carry = velocity_x
            .map_or(0, |v| (v * CARRY_DURATION.as_secs_f64()) as i32)
            .clamp(-MAX_CARRY_PX, MAX_CARRY_PX);
        let target_x = if on_left_edge {
            // Cursor was moving leftward; carry is negative. Push further from
            // the right edge of the target.
            (target.max.x - LANDING_INSET + carry).clamp(target.min.x + 1, target.max.x - 1)
        } else {
            // Cursor was moving rightward; carry is positive. Push further from
            // the left edge of the target.
            (target.min.x + LANDING_INSET + carry).clamp(target.min.x + 1, target.max.x - 1)
        };

        // Preserve relative Y offset from the source display's top so vertical
        // motion feels continuous (matches macOS's behavior for side-by-side
        // displays). Apply the configured offset signed by warp direction:
        // positive offset pushes the cursor lower when warping downward, and
        // raises it when warping upward — matching the user's physical desk
        // arrangement (e.g. monitor sitting below the laptop).
        // If the equivalent position falls outside the target's Y range (e.g. a
        // tall portrait monitor's bottom region maps off a shorter laptop's
        // bottom), skip the warp — matches macOS native side-by-side behavior
        // where the cursor can only cross at Y values where both displays exist.
        let relative_y = point.y - current_display.bounds().min.y;
        let direction_sign = if target.min.y > current_display.bounds().min.y {
            1
        } else {
            -1
        };
        let signed_offset = config.horizontal_mouse_warp_offset() * direction_sign;
        let target_y = target.min.y + relative_y + signed_offset;
        if target_y < target.min.y || target_y >= target.max.y {
            return;
        }

        let landing = Origin::new(target_x, target_y);
        window_manager.warp_mouse(landing);
        // Reset the velocity sample to the landing point so the next motion
        // event computes velocity from the new position, not the pre-warp one.
        state.last = Some((landing, now));
    }
}

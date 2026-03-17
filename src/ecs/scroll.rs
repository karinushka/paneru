use bevy::ecs::entity::Entity;
use bevy::ecs::message::MessageReader;
use bevy::ecs::query::{With, Without};
use bevy::ecs::system::{Commands, Populated, Res, Single};
use bevy::math::IRect;
use bevy::time::Time;
use std::time::{Duration, Instant};
use tracing::{Level, instrument};

use crate::config::Config;
use crate::config::swipe::SwipeGestureDirection;
use crate::ecs::layout::{Column, LayoutStrip};
use crate::ecs::params::{ActiveDisplay, Configuration, Windows};
use crate::ecs::{ActiveWorkspaceMarker, Position, Scrolling, WMEventTrigger};
use crate::errors::Result;
use crate::events::Event;
use crate::manager::{Window, WindowManager};

#[allow(clippy::needless_pass_by_value)]
#[instrument(level = Level::TRACE, skip_all)]
pub(super) fn swipe_gesture(
    mut messages: MessageReader<Event>,
    active_display: ActiveDisplay,
    mut active_workspace: Single<
        (Entity, &Position, Option<&mut Scrolling>),
        With<ActiveWorkspaceMarker>,
    >,
    time: Res<Time>,
    config: Configuration,
    mut commands: Commands,
) {
    if config.mission_control_active() {
        return;
    }

    for event in messages.read() {
        let Event::Swipe { deltas } = event else {
            continue;
        };

        if config
            .swipe_gesture_fingers()
            .is_none_or(|fingers| deltas.len() != fingers)
        {
            return;
        }
        let swipe_resolution = 1.0 / f64::from(active_display.bounds().width());
        let delta = deltas.iter().sum::<f64>();
        if delta.abs() < swipe_resolution {
            return;
        }

        let dt = time.delta_secs_f64();
        let new_velocity = if dt > 0.0 {
            delta * config.config().swipe_sensitivity() / dt
        } else {
            0.0
        };

        let (entity, position, scrolling) = &mut *active_workspace;
        if let Some(scrolling) = scrolling.as_mut() {
            let velocity = 0.3 * new_velocity + 0.7 * scrolling.velocity;
            scrolling.velocity = velocity;
            scrolling.is_user_swiping = true;
            scrolling.last_event = Instant::now();
        } else if let Ok(mut entity_cmmands) = commands.get_entity(*entity) {
            entity_cmmands.try_insert(Scrolling {
                velocity: new_velocity,
                position: f64::from(position.0.x),
                is_user_swiping: true,
                ..Default::default()
            });
            // Do not keep re-inserting the marker for other messages.
            break;
        }
    }
}

#[allow(clippy::needless_pass_by_value)]
#[instrument(level = Level::TRACE, skip_all)]
pub(super) fn swiping_timeout(
    mut strips: Populated<(Entity, &mut Scrolling), With<LayoutStrip>>,
    active_display: ActiveDisplay,
    time: Res<Time>,
    window_manager: Res<WindowManager>,
    mut commands: Commands,
) {
    const FINGER_LIFT_THRESHOLD: Duration = Duration::from_millis(50);
    const MIN_VELOCITY_PX: f64 = 5.0;
    let dt = time.delta_secs_f64();
    let viewport_width = f64::from(active_display.bounds().width());

    for (entity, mut scroll) in &mut strips {
        if scroll.last_event.elapsed() > FINGER_LIFT_THRESHOLD {
            scroll.is_user_swiping = false;

            if scroll.velocity.abs() * dt * viewport_width < MIN_VELOCITY_PX {
                commands.entity(entity).remove::<Scrolling>();
            }
            if let Some(point) = window_manager.cursor_position() {
                commands.trigger(WMEventTrigger(Event::MouseMoved { point }));
            }
        }
    }
}

#[allow(clippy::needless_pass_by_value)]
#[instrument(level = Level::TRACE, skip_all)]
pub(super) fn apply_inertia(
    mut strips: Populated<(Entity, &mut Scrolling), With<LayoutStrip>>,
    time: Res<Time>,
    config: Configuration,
) {
    let dt = time.delta_secs_f64();
    for (_, mut scroll) in &mut strips {
        if scroll.is_user_swiping {
            continue;
        }
        if scroll.velocity.abs() > 0.001 {
            let decay_rate = config.config().swipe_deceleration();
            scroll.velocity *= (-decay_rate * dt).exp();
        } else {
            scroll.velocity = 0.0;
        }
    }
}

#[allow(clippy::needless_pass_by_value)]
#[instrument(level = Level::TRACE, skip_all)]
pub(super) fn apply_snap_force(
    mut strip: Single<(&LayoutStrip, &Position, &mut Scrolling)>,
    active_display: ActiveDisplay,
    windows: Windows,
    config: Configuration,
    time: Res<Time>,
) {
    const CENTER_MAGNETIC_FORCE: f64 = 4.0;
    const SNAP_DISPLAY_RATIO: f64 = 0.1;

    if !config.config().auto_center() {
        return;
    }

    let viewport = active_display
        .display()
        .actual_display_bounds(active_display.dock(), config.config());
    let viewport_center = viewport.center().x;
    let snap_threshold = SNAP_DISPLAY_RATIO * f64::from(viewport.width());

    let (strip, position, ref mut scroll) = *strip;
    if scroll.velocity.abs() > 0.5 {
        return;
    }

    let target_offset = strip
        .all_columns()
        .into_iter()
        .filter_map(|entity| {
            windows
                .layout_position(entity)
                .map(|p| p.0.x)
                .zip(Some(entity))
        })
        .map(|(position, entity)| {
            let col_width = windows.moving_frame(entity).map_or(0, |f| f.width());
            viewport_center - (position + col_width / 2)
        })
        .min_by_key(|target| (position.x - target).abs())
        .unwrap_or(position.x);

    let dist_to_snap = f64::from(position.x - target_offset);
    let magnetic_pull = dist_to_snap.abs() / f64::from(viewport.width());
    if dist_to_snap.abs() < snap_threshold {
        let dt = time.delta_secs_f64();
        scroll.velocity *= magnetic_pull.powf(3.0);
        scroll.position -= dist_to_snap * dt * CENTER_MAGNETIC_FORCE;
    }
}

#[allow(clippy::needless_pass_by_value)]
#[instrument(level = Level::TRACE, skip_all)]
pub(super) fn scrolling_integrator(
    mut strip: Single<&mut Scrolling, With<LayoutStrip>>,
    time: Res<Time>,
    active_display: ActiveDisplay,
    config: Configuration,
) {
    let dt = time.delta_secs_f64();
    let viewport = active_display
        .display()
        .actual_display_bounds(active_display.dock(), config.config());
    let viewport_width = f64::from(viewport.width());

    // Direction modifier: Natural moves strip left (negative offset) for positive delta (finger left)
    let direction_modifier = match config.config().swipe_gesture_direction() {
        SwipeGestureDirection::Natural => -1.0,
        SwipeGestureDirection::Reversed => 1.0,
    };

    let scroll = &mut *strip;
    if scroll.velocity.abs() > 0.0001 {
        scroll.position += scroll.velocity * dt * viewport_width * direction_modifier;
    }
}

#[allow(clippy::needless_pass_by_value, clippy::type_complexity)]
#[instrument(level = Level::TRACE, skip_all)]
pub(super) fn apply_scrolling_constraints(
    mut strip: Single<
        (&LayoutStrip, &mut Position, &mut Scrolling),
        (With<ActiveWorkspaceMarker>, Without<Window>),
    >,
    active_display: ActiveDisplay,
    windows: Windows,
    config: Configuration,
) {
    let viewport = active_display
        .display()
        .actual_display_bounds(active_display.dock(), config.config());
    let (strip, ref mut position, ref mut scroll) = *strip;

    let get_window_frame = |entity| windows.moving_frame(entity);
    if let Some(clamped_offset) = clamp_viewport_offset(
        scroll.position as i32,
        strip,
        &windows,
        &get_window_frame,
        &viewport,
        config.config(),
    ) {
        position.x = clamped_offset;
        scroll.position = f64::from(clamped_offset);
    } else {
        scroll.velocity = 0.0;
    }
}

#[instrument(level = Level::TRACE, skip_all)]
fn clamp_viewport_offset<W>(
    current_offset: i32,
    layout_strip: &LayoutStrip,
    windows: &Windows,
    get_window_frame: &W,
    viewport: &IRect,
    config: &Config,
) -> Option<i32>
where
    W: Fn(Entity) -> Option<IRect>,
{
    let total_strip_width = layout_strip
        .last()
        .ok()
        .and_then(|column| column.top())
        .and_then(|entity| {
            windows
                .layout_position(entity)
                .zip(get_window_frame(entity))
        })
        .map(|(position, frame)| position.x + frame.width())?;

    let continuous_swipe = config.continuous_swipe();
    let strip_position = |column: Result<Column>| {
        column
            .ok()
            .and_then(|column| column.top())
            .and_then(|entity| windows.layout_position(entity))
            .map(|position| position.0.x)
    };

    let left_snap = strip_position(layout_strip.last());
    let right_snap = strip_position(layout_strip.get(1));

    Some(
        if continuous_swipe && let Some((left_snap, right_snap)) = left_snap.zip(right_snap) {
            // Allow to scroll away until the last or first window snaps.
            current_offset.clamp(viewport.min.x - left_snap, viewport.max.x - right_snap)
        } else if viewport.width() < total_strip_width {
            // Snap the strip directly to the edges.
            current_offset.clamp(viewport.max.x - total_strip_width, viewport.min.x)
        } else {
            // Snap the strip directly to the edges.
            current_offset.clamp(viewport.min.x, viewport.max.x - total_strip_width)
        },
    )
}

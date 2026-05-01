use bevy::ecs::entity::Entity;
use bevy::ecs::message::MessageReader;
use bevy::ecs::query::{With, Without};
use bevy::ecs::system::{Commands, Local, Populated, Res, Single};
use bevy::math::IRect;
use bevy::time::Time;
use std::time::{Duration, Instant};
use tracing::{Level, instrument};

use crate::commands::{Command, Direction, Operation};
use crate::config::Config;
use crate::config::swipe::SwipeGestureDirection;
use crate::ecs::layout::{Column, LayoutStrip};
use crate::ecs::params::{ActiveDisplay, Windows};
use crate::ecs::{ActiveWorkspaceMarker, Position, Scrolling, SendMessageTrigger, WMEventTrigger};
use crate::errors::Result;
use crate::events::Event;
use crate::manager::{Window, WindowManager};
use crate::platform::Modifiers;

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
    config: Res<Config>,
    mut commands: Commands,
) {
    for event in messages.read() {
        let delta = match event {
            Event::TouchpadDown => {
                let (_, _, scrolling) = &mut *active_workspace;
                if let Some(scrolling) = scrolling.as_mut() {
                    scrolling.velocity = 0.0;
                    scrolling.is_user_swiping = true;
                    scrolling.last_event = Instant::now();
                }
                continue;
            }
            Event::TouchpadUp => {
                let (_, _, scrolling) = &mut *active_workspace;
                if let Some(scrolling) = scrolling.as_mut() {
                    scrolling.is_user_swiping = false;
                    scrolling.last_event = Instant::now();
                }
                continue;
            }
            Event::Scroll { delta } => {
                // Normalization: Touchpad deltas are typically small fractions.
                // Scroll wheel deltas can be larger. We scale it down slightly
                // to match the "feel" of a finger swipe.
                const SCROLL_SCALE_UPPER: f64 = 0.15;
                const SCROLL_SCALE_LOWER: f64 = 0.005;
                const SCROLL_FULL_RANGE: f64 = 2.0;
                let scroll_scale = SCROLL_SCALE_LOWER
                    + ((SCROLL_SCALE_UPPER - SCROLL_SCALE_LOWER) / SCROLL_FULL_RANGE)
                        * config.swipe_sensitivity();

                *delta * scroll_scale
            }
            Event::Swipe { deltas } => {
                if config
                    .swipe_gesture_fingers()
                    .is_none_or(|fingers| deltas.len() != fingers)
                {
                    continue;
                }
                deltas.iter().sum::<f64>()
            }
            _ => continue,
        };

        let viewport_width = f64::from(active_display.bounds().width());
        let swipe_resolution = 1.0 / viewport_width;
        if delta.abs() < swipe_resolution {
            continue;
        }

        let dt = time.delta_secs_f64();
        let new_velocity = if dt > 0.0 {
            delta * config.swipe_sensitivity() / dt
        } else {
            0.0
        };

        let direction_modifier = match config.swipe_gesture_direction() {
            SwipeGestureDirection::Natural => -1.0,
            SwipeGestureDirection::Reversed => 1.0,
        };

        let (entity, position, scrolling) = &mut *active_workspace;
        if let Some(scrolling) = scrolling.as_mut() {
            scrolling.velocity = new_velocity;
            scrolling.is_user_swiping = true;
            scrolling.last_event = Instant::now();
            scrolling.position +=
                delta * viewport_width * direction_modifier * config.swipe_sensitivity();
        } else if let Ok(mut entity_commands) = commands.get_entity(*entity) {
            entity_commands.try_insert(Scrolling {
                velocity: new_velocity,
                position: f64::from(position.0.x) + delta * viewport_width * direction_modifier,
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
    strips: Populated<(Entity, &Scrolling), With<LayoutStrip>>,
    active_display: ActiveDisplay,
    time: Res<Time>,
    window_manager: Res<WindowManager>,
    mut commands: Commands,
) {
    const SWIPE_FOCUS_TIMEOUT: Duration = Duration::from_millis(500);
    const MIN_VELOCITY_PX: f64 = 5.0;
    let dt = time.delta_secs_f64();
    let viewport_width = f64::from(active_display.bounds().width());

    for (entity, scroll) in strips {
        if !scroll.is_user_swiping && scroll.velocity.abs() * dt * viewport_width < MIN_VELOCITY_PX
        {
            commands.entity(entity).remove::<Scrolling>();
        }

        if scroll.last_event.elapsed() > SWIPE_FOCUS_TIMEOUT
            && let Some(point) = window_manager.cursor_position()
        {
            commands.trigger(WMEventTrigger(Event::MouseMoved {
                point,
                modifiers: Modifiers::empty(),
            }));
        }
    }
}

#[allow(clippy::needless_pass_by_value)]
#[instrument(level = Level::TRACE, skip_all)]
pub(super) fn apply_inertia(
    mut strips: Populated<(Entity, &mut Scrolling), With<LayoutStrip>>,
    time: Res<Time>,
    config: Res<Config>,
) {
    let dt = time.delta_secs_f64();
    for (_, mut scroll) in &mut strips {
        if scroll.is_user_swiping {
            continue;
        }
        if scroll.velocity.abs() > 0.001 {
            let decay_rate = config.swipe_deceleration();
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
    config: Res<Config>,
    time: Res<Time>,
) {
    const CENTER_MAGNETIC_FORCE: f64 = 10.0;
    const SNAP_DISPLAY_RATIO: f64 = 0.45;

    if !config.auto_center() {
        return;
    }

    let viewport = active_display
        .display()
        .actual_display_bounds(active_display.dock(), &config);
    let viewport_center = viewport.center().x;
    let snap_threshold = SNAP_DISPLAY_RATIO * f64::from(viewport.width());

    let (strip, position, ref mut scroll) = *strip;
    if scroll.is_user_swiping || scroll.velocity.abs() > 0.5 {
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
    if dist_to_snap.abs() < snap_threshold {
        let dt = time.delta_secs_f64();
        scroll.position -= dist_to_snap * dt * CENTER_MAGNETIC_FORCE;
    }
}

#[allow(clippy::needless_pass_by_value)]
#[instrument(level = Level::TRACE, skip_all)]
pub(super) fn scrolling_integrator(
    mut strip: Single<&mut Scrolling, With<LayoutStrip>>,
    time: Res<Time>,
    active_display: ActiveDisplay,
    config: Res<Config>,
) {
    let dt = time.delta_secs_f64();
    let viewport = active_display
        .display()
        .actual_display_bounds(active_display.dock(), &config);
    let viewport_width = f64::from(viewport.width());

    // Direction modifier: Natural moves strip left (negative offset) for positive delta (finger left)
    let direction_modifier = match config.swipe_gesture_direction() {
        SwipeGestureDirection::Natural => -1.0,
        SwipeGestureDirection::Reversed => 1.0,
    };

    let scroll = &mut *strip;
    if scroll.is_user_swiping || scroll.velocity.abs() <= 0.0001 {
        return;
    }

    scroll.position += scroll.velocity * dt * viewport_width * direction_modifier;
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
    config: Res<Config>,
) {
    let viewport = active_display
        .display()
        .actual_display_bounds(active_display.dock(), &config);
    let (strip, ref mut position, ref mut scroll) = *strip;

    let get_window_frame = |entity| windows.moving_frame(entity);
    if let Some(clamped_offset) = clamp_viewport_offset(
        scroll.position as i32,
        strip,
        &windows,
        &get_window_frame,
        &viewport,
        &config,
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

#[derive(Default)]
pub(super) struct VerticalGestureState {
    accumulated: f64,
    last_event: Option<Instant>,
    fired: bool,
}

#[allow(clippy::needless_pass_by_value)]
#[instrument(level = Level::TRACE, skip_all)]
pub(super) fn vertical_swipe_gesture(
    mut messages: MessageReader<Event>,
    active_display: ActiveDisplay,
    config: Res<Config>,
    mut commands: Commands,
    mut state: Local<VerticalGestureState>,
) {
    if active_display.fullscreen().is_some() {
        return;
    }

    let switch_virtual = |delta: f64, commands: &mut Commands| {
        let physical_finger_direction = if delta > 0.0 {
            Direction::South
        } else {
            Direction::North
        };
        let direction = match config.swipe_gesture_direction() {
            SwipeGestureDirection::Natural => physical_finger_direction.reverse(),
            SwipeGestureDirection::Reversed => physical_finger_direction,
        };
        commands.trigger(SendMessageTrigger(Event::Command {
            command: Command::Window(Operation::Virtual(direction)),
        }));
    };

    const GESTURE_TIMEOUT: Duration = Duration::from_millis(150);

    // Reset state when the gesture times out (fingers lifted).
    if let Some(last) = state.last_event
        && last.elapsed() > GESTURE_TIMEOUT
    {
        state.accumulated = 0.0;
        state.fired = false;
    }

    // Already fired for this trackpad gesture. Drain the reader to advance
    // its cursor but only update timing so the timeout tracks the real gesture end.
    // Scroll wheel ticks still fire since each tick is independent.
    if state.fired {
        for event in messages.read() {
            match event {
                Event::VerticalScrollTick { delta } => {
                    switch_virtual(*delta, &mut commands);
                }
                Event::VerticalSwipe { .. } => {
                    state.last_event = Some(Instant::now());
                }
                _ => {}
            }
        }
        return;
    }

    for event in messages.read() {
        match event {
            Event::VerticalScrollTick { delta } => {
                switch_virtual(*delta, &mut commands);
            }
            Event::VerticalSwipe { delta } => {
                state.accumulated += delta;
                state.last_event = Some(Instant::now());
            }
            _ => {}
        }
    }

    if state.accumulated != 0.0 {
        // Threshold needs to be high enough that incidental vertical movement
        // during horizontal swipes doesn't trigger a workspace switch.
        let threshold = 0.15 / config.swipe_sensitivity();
        if state.accumulated.abs() >= threshold {
            switch_virtual(state.accumulated, &mut commands);
            state.accumulated = 0.0;
            state.fired = true;
        }
    }
}

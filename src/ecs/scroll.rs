use bevy::app::{App, Plugin, Update};
use bevy::ecs::entity::Entity;
use bevy::ecs::message::MessageReader;
use bevy::ecs::query::{With, Without};
use bevy::ecs::schedule::IntoScheduleConfigs as _;
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
use crate::ecs::{
    ActiveWorkspaceMarker, MissionControlActive, PagingGesture, Position, Scrolling,
    SendMessageTrigger,
};
use crate::errors::Result;
use crate::events::Event;
use crate::manager::{Window, WindowManager};
use crate::platform::Modifiers;

mod paging;
use paging::{
    capture_gesture as capture_paging_gesture, constrain_motion as constrain_paging_motion,
    ready_to_snap as scrolling_ready_to_snap, snap_target as paging_snap_target,
};

pub struct ScrollEventsPlugin;

const NATIVE_SCROLL_RESPONSE_SECONDS: f64 = 0.04;
const NATIVE_SCROLL_SETTLE_PX: f64 = 0.25;
/// Logical-point distance inside a window edge at which sticky scrolling
/// engages. This is a hit zone, not a visual gap: the snap lands on the edge.
const STICKY_EDGE_THRESHOLD_POINTS: i32 = 32;

#[derive(Default)]
struct GestureInput {
    scroll_delta: Option<f64>,
    gesture_delta: Option<f64>,
    lifecycle: u8,
}

const TOUCHPAD_DOWN: u8 = 1 << 0;
const TOUCHPAD_PHYSICAL_UP: u8 = 1 << 1;
const TOUCHPAD_MOMENTUM_START: u8 = 1 << 2;
const TOUCHPAD_UP: u8 = 1 << 3;

impl Plugin for ScrollEventsPlugin {
    fn build(&self, app: &mut App) {
        let mission_control_inactive = |mission_control: Option<Res<MissionControlActive>>| {
            mission_control.is_none_or(|active| !active.0)
        };

        app.add_systems(
            Update,
            (
                vertical_swipe_gesture.run_if(mission_control_inactive),
                (
                    swipe_gesture.run_if(mission_control_inactive),
                    apply_inertia,
                    apply_snap_force,
                    scrolling_integrator,
                    apply_scrolling_constraints,
                    swiping_timeout,
                )
                    .chain(),
            ),
        );
    }
}

// This ECS system intentionally keeps event aggregation and component updates
// in one schedule boundary; pure paging math lives in `scroll::paging`.
#[allow(clippy::needless_pass_by_value, clippy::too_many_lines)]
#[instrument(level = Level::TRACE, skip_all)]
fn swipe_gesture(
    mut messages: MessageReader<Event>,
    active_display: ActiveDisplay,
    mut active_workspace: Single<
        (Entity, &LayoutStrip, &Position, Option<&mut Scrolling>),
        With<ActiveWorkspaceMarker>,
    >,
    windows: Windows,
    time: Res<Time>,
    config: Res<Config>,
    mut commands: Commands,
) {
    let swipe_sensitivity = config.swipe_sensitivity();
    let snap_enabled = config.swipe_paging() || config.sticky_scroll() || config.auto_center();
    // Normalization: Touchpad deltas are typically small fractions.
    // Scroll wheel deltas can be larger. We scale it down slightly
    // to match the "feel" of a finger swipe.
    const SCROLL_SCALE_UPPER: f64 = 0.15;
    const SCROLL_SCALE_LOWER: f64 = 0.005;
    const SCROLL_FULL_RANGE: f64 = 2.0;
    let scroll_scale = SCROLL_SCALE_LOWER
        + ((SCROLL_SCALE_UPPER - SCROLL_SCALE_LOWER) / SCROLL_FULL_RANGE) * swipe_sensitivity;
    let input = read_gesture_input(&mut messages, &config, scroll_scale);
    let GestureInput {
        scroll_delta,
        gesture_delta,
        lifecycle,
    } = input;
    let touchpad_down = lifecycle & TOUCHPAD_DOWN != 0;
    let touchpad_physical_up = lifecycle & TOUCHPAD_PHYSICAL_UP != 0;
    let touchpad_momentum_start = lifecycle & TOUCHPAD_MOMENTUM_START != 0;
    let touchpad_up = lifecycle & TOUCHPAD_UP != 0;
    let has_gesture_event = gesture_delta.is_some();
    let has_scroll_event = scroll_delta.is_some() || has_gesture_event;
    let scroll_delta = scroll_delta.unwrap_or_default();
    let gesture_delta = gesture_delta.unwrap_or_default();

    if lifecycle == 0 && !has_scroll_event {
        return;
    }

    let (entity, layout_strip, position, scrolling) = &mut *active_workspace;
    let has_active_session = scrolling.as_ref().is_some_and(|scrolling| {
        scrolling.gesture_active
            || scrolling.is_user_swiping
            || scrolling.snap_pending
            || scrolling.paging_gesture.is_some()
    });
    let resumes_gesture = has_active_session && (touchpad_down || touchpad_momentum_start);
    let starts_new_gesture = (touchpad_down && !has_active_session)
        || (!has_active_session
            && has_scroll_event
            && !touchpad_momentum_start
            && scrolling
                .as_ref()
                .is_none_or(|scrolling| !scrolling.is_user_swiping));
    let viewport = active_display.actual_bounds(&config);
    let paging_gesture = (config.swipe_paging() && starts_new_gesture)
        .then(|| {
            current_paging_gesture(
                layout_strip,
                position,
                scrolling.as_deref(),
                &windows,
                &viewport,
            )
        })
        .flatten();

    begin_touchpad_gesture(
        starts_new_gesture,
        touchpad_down,
        snap_enabled,
        paging_gesture,
        scrolling.as_deref_mut(),
    );
    // AppKit can report physical Ended and momentum Began together. Apply the
    // physical end first so the momentum phase remains the final state.
    mark_physical_touch_end(touchpad_physical_up, scrolling.as_deref_mut());
    resume_touchpad_gesture(resumes_gesture, touchpad_down, scrolling.as_deref_mut());

    if touchpad_down && !has_scroll_event && scrolling.is_none() {
        insert_touchpad_begin_state(
            *entity,
            position.x,
            snap_enabled,
            paging_gesture,
            &mut commands,
        );
    }

    if has_scroll_event {
        // Preserve the established gesture-distance normalization. Paging
        // anchors themselves use the usable viewport below.
        let viewport_width = f64::from(active_display.bounds().width());
        let direction_modifier = horizontal_direction_modifier(&config);

        let dt = time.delta_secs_f64();
        let new_velocity = if has_gesture_event && dt > 0.0 {
            gesture_delta * swipe_sensitivity / dt
        } else {
            0.0
        };
        let gesture_distance =
            gesture_delta * viewport_width * direction_modifier * swipe_sensitivity;
        let scroll_distance =
            scroll_delta * viewport_width * direction_modifier * swipe_sensitivity;

        if let Some(scrolling) = scrolling.as_mut() {
            let was_user_swiping = scrolling.is_user_swiping;
            // Native modifier-scroll has momentum; synthesize inertia only for raw gestures.
            scrolling.velocity = if has_gesture_event {
                // Smoothen gesture velocity changes using EMA.
                0.3 * new_velocity + 0.7 * scrolling.velocity
            } else {
                0.0
            };
            scrolling.is_user_swiping = true;
            scrolling.snap_pending = snap_enabled;
            scrolling.last_event = Instant::now();

            if has_gesture_event {
                scrolling.target_position = None;
                scrolling.position += gesture_distance;
            }

            if scroll_delta != 0.0 {
                // A new physical gesture interrupts an in-flight sticky snap.
                // Native momentum events keep extending the same target.
                if !was_user_swiping {
                    scrolling.target_position = None;
                }
                let target = scrolling.target_position.unwrap_or(scrolling.position);
                scrolling.target_position = Some(target + scroll_distance);
            }
            constrain_paging_motion(scrolling, direction_modifier);
        } else if let Ok(mut entity_commands) = commands.get_entity(*entity) {
            let initial_position = f64::from(position.0.x) + gesture_distance;
            let mut scrolling = Scrolling {
                velocity: new_velocity,
                position: initial_position,
                target_position: (scroll_delta != 0.0)
                    .then_some(initial_position + scroll_distance),
                snap_pending: snap_enabled,
                is_user_swiping: !touchpad_up && (touchpad_down || has_scroll_event),
                gesture_active: touchpad_down && !touchpad_up,
                paging_gesture,
                last_event: Instant::now(),
            };
            constrain_paging_motion(&mut scrolling, direction_modifier);
            entity_commands.try_insert(scrolling);
        }
    }

    let direction_modifier = horizontal_direction_modifier(&config);
    finish_touchpad_gesture(touchpad_up, direction_modifier, scrolling.as_deref_mut());
}

fn read_gesture_input(
    messages: &mut MessageReader<Event>,
    config: &Config,
    scroll_scale: f64,
) -> GestureInput {
    let mut input = GestureInput::default();
    for event in messages.read() {
        match event {
            Event::TouchpadDown => input.lifecycle |= TOUCHPAD_DOWN,
            Event::TouchpadPhysicalUp => input.lifecycle |= TOUCHPAD_PHYSICAL_UP,
            Event::TouchpadMomentumStart => input.lifecycle |= TOUCHPAD_MOMENTUM_START,
            Event::TouchpadUp => input.lifecycle |= TOUCHPAD_UP,
            Event::Scroll { delta } => {
                *input.scroll_delta.get_or_insert(0.0) += *delta * scroll_scale;
            }
            Event::Swipe { delta, fingers }
                if config
                    .swipe_gesture_fingers()
                    .is_some_and(|configured| configured == *fingers) =>
            {
                *input.gesture_delta.get_or_insert(0.0) += *delta;
            }
            _ => {}
        }
    }
    input
}

fn insert_touchpad_begin_state(
    entity: Entity,
    position: i32,
    snap_enabled: bool,
    paging_gesture: Option<PagingGesture>,
    commands: &mut Commands,
) {
    if let Ok(mut entity_commands) = commands.get_entity(entity) {
        entity_commands.try_insert(Scrolling {
            position: f64::from(position),
            snap_pending: snap_enabled,
            is_user_swiping: true,
            gesture_active: true,
            paging_gesture,
            ..Default::default()
        });
    }
}

fn horizontal_direction_modifier(config: &Config) -> f64 {
    match config.swipe_gesture_direction() {
        SwipeGestureDirection::Natural => -1.0,
        SwipeGestureDirection::Reversed => 1.0,
    }
}

fn current_paging_gesture(
    layout_strip: &LayoutStrip,
    position: &Position,
    scrolling: Option<&Scrolling>,
    windows: &Windows<'_, '_>,
    viewport: &IRect,
) -> Option<PagingGesture> {
    let get_window_frame = |entity| windows.moving_frame(entity);
    let columns = layout_strip.columns().filter_map(|column| {
        let entity = column.top()?;
        Some((
            windows.layout_position(entity)?.0.x,
            column.width(&get_window_frame)?,
        ))
    });
    let current_position = scrolling.map_or(f64::from(position.x), |scrolling| scrolling.position);
    capture_paging_gesture(current_position, viewport, columns)
}

fn begin_touchpad_gesture(
    starts_new_gesture: bool,
    touchpad_down: bool,
    snap_enabled: bool,
    paging_gesture: Option<PagingGesture>,
    scrolling: Option<&mut Scrolling>,
) {
    if starts_new_gesture && let Some(scrolling) = scrolling {
        scrolling.velocity = 0.0;
        scrolling.target_position = None;
        scrolling.snap_pending = snap_enabled;
        scrolling.is_user_swiping = true;
        scrolling.gesture_active = touchpad_down;
        scrolling.paging_gesture = paging_gesture;
        scrolling.last_event = Instant::now();
    }
}

fn resume_touchpad_gesture(
    resumes_gesture: bool,
    interrupts_target: bool,
    scrolling: Option<&mut Scrolling>,
) {
    if resumes_gesture && let Some(scrolling) = scrolling {
        if interrupts_target {
            scrolling.target_position = None;
        }
        scrolling.snap_pending = true;
        scrolling.is_user_swiping = true;
        scrolling.gesture_active = true;
        scrolling.last_event = Instant::now();
    }
}

fn mark_physical_touch_end(physical_up: bool, scrolling: Option<&mut Scrolling>) {
    if physical_up && let Some(scrolling) = scrolling {
        scrolling.gesture_active = false;
        // Keep user-swiping true until either momentum starts or the inactivity
        // fallback proves there will be no native momentum phase.
        scrolling.is_user_swiping = true;
        scrolling.last_event = Instant::now();
    }
}

fn finish_touchpad_gesture(
    touchpad_up: bool,
    direction_modifier: f64,
    scrolling: Option<&mut Scrolling>,
) {
    // Momentum can keep moving afterwards, but sticky selection starts only
    // after both the gesture and any remaining target/velocity have settled.
    if touchpad_up && let Some(scrolling) = scrolling {
        if let Some(paging) = scrolling.paging_gesture.as_mut() {
            paging.release_velocity = scrolling.velocity * direction_modifier;
        }
        scrolling.gesture_active = false;
        scrolling.is_user_swiping = false;
    }
}

#[allow(clippy::needless_pass_by_value)]
#[instrument(level = Level::TRACE, skip_all)]
pub(super) fn swiping_timeout(
    strips: Populated<(Entity, &mut Scrolling), With<LayoutStrip>>,
    active_display: ActiveDisplay,
    time: Res<Time>,
    window_manager: Res<WindowManager>,
    mut commands: Commands,
) {
    const FINGER_LIFT_THRESHOLD: Duration = Duration::from_millis(50);
    const MIN_VELOCITY_PX: f64 = 5.0;
    let dt = time.delta_secs_f64();
    let viewport_width = f64::from(active_display.bounds().width());

    for (entity, mut scroll) in strips {
        if !scroll.gesture_active && scroll.last_event.elapsed() > FINGER_LIFT_THRESHOLD {
            scroll.is_user_swiping = false;

            if scroll.velocity.abs() * dt * viewport_width < MIN_VELOCITY_PX
                && scroll.target_position.is_none()
                && !scroll.snap_pending
                && let Ok(mut entity_commands) = commands.get_entity(entity)
            {
                entity_commands.try_remove::<Scrolling>();
            }
            if let Some(point) = window_manager.cursor_position() {
                commands.trigger(SendMessageTrigger(Event::MouseMoved {
                    point,
                    modifiers: Modifiers::empty(),
                }));
            }
        }
    }
}

#[allow(clippy::needless_pass_by_value)]
#[instrument(level = Level::TRACE, skip_all)]
fn apply_inertia(
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
fn apply_snap_force(
    mut strip: Single<(&LayoutStrip, &Position, &mut Scrolling)>,
    active_display: ActiveDisplay,
    windows: Windows,
    config: Res<Config>,
) {
    const SNAP_DISPLAY_RATIO: f64 = 0.45;

    let (layout_strip, position, ref mut scroll) = *strip;
    let paging = config.swipe_paging();
    let sticky = config.sticky_scroll();
    if !paging && !sticky && !config.auto_center() {
        scroll.snap_pending = false;
        return;
    }

    let viewport = active_display.actual_bounds(&config);
    let snap_threshold = SNAP_DISPLAY_RATIO * f64::from(viewport.width());

    if !scrolling_ready_to_snap(scroll) {
        return;
    }

    let get_window_frame = |entity| windows.moving_frame(entity);
    let target_offset = if paging {
        let Some(paging_gesture) = scroll.paging_gesture else {
            scroll.snap_pending = false;
            return;
        };
        paging_snap_target(scroll.position, f64::from(viewport.width()), paging_gesture) as i32
    } else if sticky {
        let Some(target_offset) = sticky_edge_snap_target(
            position.x,
            &viewport,
            layout_strip.columns().filter_map(|column| {
                let entity = column.top()?;
                let column_position = windows.layout_position(entity)?.0.x;
                let column_width = column.width(&get_window_frame)?;
                Some((column_position, column_width))
            }),
        ) else {
            scroll.snap_pending = false;
            return;
        };
        target_offset
    } else {
        let viewport_center = viewport.center().x;
        layout_strip
            .all_columns()
            .into_iter()
            .filter_map(|entity| {
                windows
                    .layout_position(entity)
                    .map(|p| p.0.x)
                    .zip(Some(entity))
            })
            .map(|(column_position, entity)| {
                let column_width = windows.moving_frame(entity).map_or(0, |f| f.width());
                viewport_center - (column_position + column_width / 2)
            })
            .min_by_key(|target| (position.x - target).abs())
            .unwrap_or(position.x)
    };

    let dist_to_snap = f64::from(position.x - target_offset);
    scroll.snap_pending = false;
    if paging || sticky || dist_to_snap.abs() < snap_threshold {
        // Keep Scrolling alive until the shared target integrator reaches the
        // anchor. This works for native modifier-scroll and raw gestures.
        scroll.velocity = 0.0;
        scroll.target_position = Some(f64::from(target_offset));
    }
}

fn sticky_edge_snap_target(
    current_offset: i32,
    viewport: &IRect,
    columns: impl IntoIterator<Item = (i32, i32)>,
) -> Option<i32> {
    let current_offset = i64::from(current_offset);
    let threshold = i64::from(STICKY_EDGE_THRESHOLD_POINTS);

    columns
        .into_iter()
        .flat_map(|(column_position, column_width)| {
            [
                (
                    viewport.min.x - column_position,
                    // The viewport's left edge is inside the window.
                    -threshold..=0,
                ),
                (
                    viewport.max.x - (column_position + column_width),
                    // The viewport's right edge is inside the window.
                    0..=threshold,
                ),
            ]
        })
        .filter_map(|(target, hit_zone)| {
            hit_zone
                .contains(&(current_offset - i64::from(target)))
                .then_some(target)
        })
        .min_by_key(|target| (current_offset - i64::from(*target)).abs())
}

#[allow(clippy::needless_pass_by_value)]
#[instrument(level = Level::TRACE, skip_all)]
fn scrolling_integrator(
    mut strip: Single<&mut Scrolling, With<LayoutStrip>>,
    time: Res<Time>,
    active_display: ActiveDisplay,
    config: Res<Config>,
) {
    let dt = time.delta_secs_f64();
    let viewport = active_display.actual_bounds(&config);
    let viewport_width = f64::from(viewport.width());

    // Direction modifier: Natural moves strip left (negative offset) for positive delta (finger left)
    let direction_modifier = horizontal_direction_modifier(&config);

    let scroll = &mut *strip;
    if let Some(target) = scroll.target_position {
        let (position, settled) = smooth_native_scroll(scroll.position, target, dt);
        scroll.position = position;
        if settled {
            scroll.target_position = None;
            if !scroll.snap_pending {
                scroll.paging_gesture = None;
            }
        }
        return;
    }

    if scroll.velocity.abs() > 0.0001 {
        scroll.position += scroll.velocity * dt * viewport_width * direction_modifier;
        constrain_paging_motion(scroll, direction_modifier);
    }
}

fn smooth_native_scroll(position: f64, target: f64, dt: f64) -> (f64, bool) {
    let blend = 1.0 - (-dt / NATIVE_SCROLL_RESPONSE_SECONDS).exp();
    let position = position + (target - position) * blend;

    if (target - position).abs() <= NATIVE_SCROLL_SETTLE_PX {
        (target, true)
    } else {
        (position, false)
    }
}

/// Preserve the integrator's subpixel remainder unless viewport constraints
/// actually changed the integer position that macOS can apply.
fn reconcile_integrated_position(
    integrated_position: f64,
    effective_position: i32,
    clamped_position: i32,
) -> f64 {
    if effective_position == clamped_position {
        integrated_position
    } else {
        f64::from(clamped_position)
    }
}

#[allow(clippy::needless_pass_by_value, clippy::type_complexity)]
#[instrument(level = Level::TRACE, skip_all)]
fn apply_scrolling_constraints(
    mut strip: Single<
        (&LayoutStrip, &mut Position, &mut Scrolling),
        (With<ActiveWorkspaceMarker>, Without<Window>),
    >,
    active_display: ActiveDisplay,
    windows: Windows,
    config: Res<Config>,
) {
    let viewport = active_display.actual_bounds(&config);
    let (strip, ref mut position, ref mut scroll) = *strip;

    let get_window_frame = |entity| windows.moving_frame(entity);
    let effective_offset = scroll.position as i32;
    if let Some(clamped_offset) = clamp_viewport_offset(
        effective_offset,
        strip,
        &windows,
        &get_window_frame,
        &viewport,
        &config,
    ) {
        position.x = clamped_offset;
        scroll.position =
            reconcile_integrated_position(scroll.position, effective_offset, clamped_offset);
        if let Some(target) = scroll.target_position
            && let effective_target = target as i32
            && let Some(clamped_target) = clamp_viewport_offset(
                effective_target,
                strip,
                &windows,
                &get_window_frame,
                &viewport,
                &config,
            )
        {
            scroll.target_position = Some(reconcile_integrated_position(
                target,
                effective_target,
                clamped_target,
            ));
        }
    } else {
        scroll.velocity = 0.0;
        scroll.target_position = None;
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

    if config.swipe_paging() {
        let content_min = layout_strip
            .columns()
            .filter_map(Column::top)
            .filter_map(|entity| windows.layout_position(entity))
            .map(|position| position.0.x)
            .min()?;
        let first_edge = viewport.min.x - content_min;
        let last_edge = viewport.max.x - total_strip_width;
        return Some(current_offset.clamp(first_edge.min(last_edge), first_edge.max(last_edge)));
    }

    let continuous_swipe = config.continuous_swipe();
    let has_oversized_column = layout_strip.columns().any(|column| {
        column
            .width(get_window_frame)
            .is_some_and(|width| width > viewport.width())
    });
    let strip_position = |column: Result<Column>| {
        column
            .ok()
            .and_then(|column| column.top())
            .and_then(|entity| windows.layout_position(entity))
            .map(|position| position.0.x)
    };

    let left_snap = strip_position(layout_strip.last());
    let right_snap = strip_position(layout_strip.get(1));

    let (first_edge, last_edge) = if continuous_swipe
        && !has_oversized_column
        && let Some((left_snap, right_snap)) = left_snap.zip(right_snap)
    {
        // Allow scrolling until the last or first window reaches the viewport
        // edge exactly. Sticky's 32pt value is only an activation threshold.
        (viewport.min.x - left_snap, viewport.max.x - right_snap)
    } else {
        // Pan between the leading and trailing strip edges. The min/max form
        // also handles strips narrower than the viewport without an inverted
        // clamp range.
        (viewport.min.x, viewport.max.x - total_strip_width)
    };

    Some(current_offset.clamp(first_edge.min(last_edge), first_edge.max(last_edge)))
}

#[derive(Default)]
struct VerticalGestureState {
    accumulated: f64,
    last_event: Option<Instant>,
    fired: bool,
}

#[allow(clippy::needless_pass_by_value)]
#[instrument(level = Level::TRACE, skip_all)]
fn vertical_swipe_gesture(
    mut messages: MessageReader<Event>,
    active_display: ActiveDisplay,
    config: Res<Config>,
    mut commands: Commands,
    mut state: Local<VerticalGestureState>,
) {
    const GESTURE_TIMEOUT: Duration = Duration::from_millis(150);

    if active_display.fullscreen().is_some() {
        return;
    }

    // Reset state when the gesture times out (fingers lifted).
    if let Some(last) = state.last_event
        && last.elapsed() > GESTURE_TIMEOUT
    {
        state.accumulated = 0.0;
        state.fired = false;
    }

    for event in messages.read() {
        match event {
            Event::VerticalScrollTick { delta } => {
                switch_virtual_workspace(*delta, &config, &mut commands);
            }
            Event::VerticalSwipe { delta, fingers }
                if config
                    .swipe_gesture_fingers()
                    .is_some_and(|fingers_configured| fingers_configured == *fingers) =>
            {
                state.last_event = Some(Instant::now());

                if !state.fired {
                    state.accumulated += delta;
                }
            }
            _ => {}
        }
    }

    // Threshold needs to be high enough that incidental vertical movement
    // during horizontal swipes doesn't trigger a workspace switch.
    let threshold = 0.15 / config.swipe_sensitivity();
    if state.accumulated.abs() >= threshold {
        switch_virtual_workspace(state.accumulated, &config, &mut commands);
        state.accumulated = 0.0;
        state.fired = true;
    }
}

fn switch_virtual_workspace(delta: f64, config: &Config, commands: &mut Commands) {
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
}

#[cfg(test)]
mod tests;

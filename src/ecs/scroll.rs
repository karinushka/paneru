use bevy::app::{App, Plugin, PreUpdate, Update};
use bevy::ecs::entity::Entity;
use bevy::ecs::lifecycle::Remove;
use bevy::ecs::message::{Message, MessageReader, MessageWriter};
use bevy::ecs::observer::On;
use bevy::ecs::query::{With, Without};
use bevy::ecs::resource::Resource;
use bevy::ecs::schedule::IntoScheduleConfigs as _;
use bevy::ecs::schedule::SystemCondition as _;
use bevy::ecs::system::{Commands, Local, Populated, Query, Res, ResMut, Single};
use bevy::math::IRect;
use bevy::time::Time;
use std::collections::VecDeque;
use std::time::{Duration, Instant};
use tracing::{Level, instrument};

use crate::commands::{Command, Direction, Operation};
use crate::config::Config;
use crate::config::swipe::{SwipeGestureDirection, focus_direction};
use crate::ecs::layout::LayoutStrip;
use crate::ecs::params::{ActiveDisplay, Windows};
use crate::ecs::workspace::PreviousStripPosition;
use crate::ecs::{
    ActiveWorkspaceMarker, ManualStripOffset, MissionControlActive, Position, PreUpdateSet,
    Scrolling, SendMessageTrigger, UpdateSet,
};
use bevy::ecs::schedule::common_conditions::on_message;

use crate::events::{Event, InputEvent};
use crate::manager::{Window, WindowManager};
use crate::platform::Modifiers;
use crate::util::round_px;

pub struct ScrollEventsPlugin;

const PAGE_GESTURE_TIMEOUT: Duration = Duration::from_millis(150);
// AppKit can emit a touch-phase update tens of microseconds after the prior
// event while reporting millimeters of older finger motion. Such an interval
// is useful for position but cannot estimate a sustained release velocity.
const MIN_VELOCITY_SAMPLE_SECS: f64 = 0.004;
const FINGER_LIFT_THRESHOLD: Duration = Duration::from_millis(50);
/// Bound recovery if a stretched gesture never receives its lift event.
const STRETCHED_GESTURE_TIMEOUT: Duration = Duration::from_secs(2);

fn swipe_direction_modifier(direction: SwipeGestureDirection) -> f64 {
    match direction {
        SwipeGestureDirection::Natural => -1.0,
        SwipeGestureDirection::Reversed => 1.0,
    }
}

fn scroll_wheel_scale(swipe_sensitivity: f64) -> f64 {
    const SCROLL_SCALE_UPPER: f64 = 0.15;
    const SCROLL_SCALE_LOWER: f64 = 0.005;
    const SCROLL_FULL_RANGE: f64 = 2.0 * crate::config::SWIPE_SENSITIVITY_CALIBRATION;
    (SCROLL_SCALE_LOWER
        + ((SCROLL_SCALE_UPPER - SCROLL_SCALE_LOWER) / SCROLL_FULL_RANGE) * swipe_sensitivity)
        / crate::config::SWIPE_SENSITIVITY_CALIBRATION
}

struct VelocitySamples {
    delta: f64,
    elapsed_secs: Option<f64>,
    count: usize,
}

impl Default for VelocitySamples {
    fn default() -> Self {
        Self {
            delta: 0.0,
            elapsed_secs: Some(0.0),
            count: 0,
        }
    }
}

impl VelocitySamples {
    fn add(&mut self, delta: f64, elapsed_secs: Option<f64>) {
        if elapsed_secs.is_some_and(|elapsed| elapsed < MIN_VELOCITY_SAMPLE_SECS) {
            return;
        }
        self.delta += delta;
        self.count += 1;
        self.elapsed_secs = self
            .elapsed_secs
            .and_then(|total| elapsed_secs.map(|elapsed| total + elapsed));
    }
}

/// One gesture-earned focus step, routed to [`crate::commands::command_move_focus`].
#[derive(Message, Debug, Clone)]
pub(crate) struct PageFocusRequest {
    pub(crate) direction: Direction,
    pub(crate) generation: u64,
}

#[derive(Debug, Clone, Copy)]
pub(crate) enum PageInFlight {
    Dispatched,
    AwaitingFocus {
        target: Entity,
        deadline: Option<Duration>,
    },
}

#[derive(Default, Resource)]
pub(crate) struct PageGestureState {
    accumulated_cm: f64,
    pending: VecDeque<(Direction, u64)>,
    pub(crate) in_flight: Option<PageInFlight>,
    last_event: Option<Duration>,
    active: bool,
    config: Option<(bool, Option<usize>, SwipeGestureDirection, f64, bool)>,
    pub(crate) generation: u64,
}

impl PageGestureState {
    pub(crate) fn reset(&mut self) {
        self.generation = self.generation.wrapping_add(1);
        self.accumulated_cm = 0.0;
        self.pending.clear();
        self.in_flight = None;
        self.last_event = None;
        self.active = false;
    }

    fn has_work(&self) -> bool {
        self.active || !self.pending.is_empty() || self.in_flight.is_some()
    }

    pub(crate) fn await_focus(&mut self, target: Entity) {
        self.in_flight = Some(PageInFlight::AwaitingFocus {
            target,
            deadline: None,
        });
    }

    fn earn_steps(&mut self, threshold_cm: f64, gesture_direction: SwipeGestureDirection) {
        let travel = self.accumulated_cm.abs();
        if travel < threshold_cm {
            return;
        }
        let direction = focus_direction(gesture_direction, self.accumulated_cm);
        // Saturating float-to-integer conversion bounds an implausibly large
        // gesture while preserving the earned count for normal device input.
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let steps = (travel / threshold_cm).floor() as u64;
        let remainder = travel % threshold_cm;
        self.accumulated_cm = self.accumulated_cm.signum() * remainder;
        if let Some((last_direction, count)) = self.pending.back_mut()
            && *last_direction == direction
            && let Some(merged) = count.checked_add(steps)
        {
            *count = merged;
            return;
        }
        self.pending.push_back((direction, steps));
    }

    fn next_step(&mut self) -> Option<Direction> {
        let (direction, count) = self.pending.front_mut()?;
        let direction = direction.clone();
        *count -= 1;
        if *count == 0 {
            self.pending.pop_front();
        }
        Some(direction)
    }
}

fn paging_enabled(config: Res<Config>, state: Res<PageGestureState>) -> bool {
    config.swipe_gesture_paging() || state.has_work() || state.config.as_ref().is_some_and(|c| c.0)
}

fn paging_active(config: Res<Config>, state: Res<PageGestureState>) -> bool {
    state.has_work()
        || (!config.swipe_gesture_paging() && state.config.as_ref().is_some_and(|c| c.0))
}

#[cfg(test)]
mod paging_tests {
    use super::*;

    #[test]
    fn large_swipe_preserves_every_earned_step_without_expanding_the_queue() {
        let mut state = PageGestureState {
            accumulated_cm: 80.5,
            ..Default::default()
        };
        state.earn_steps(2.0, SwipeGestureDirection::Natural);
        assert_eq!(state.pending.len(), 1);
        assert!((state.accumulated_cm - 0.5).abs() < f64::EPSILON);

        let expected = focus_direction(SwipeGestureDirection::Natural, 1.0);
        for _ in 0..40 {
            assert_eq!(state.next_step(), Some(expected.clone()));
        }
        assert_eq!(state.next_step(), None);
    }

    #[test]
    fn in_flight_focus_keeps_paging_system_active_after_finger_lift() {
        let state = PageGestureState {
            in_flight: Some(PageInFlight::Dispatched),
            ..Default::default()
        };
        assert!(state.has_work());
    }
}

/// Upper bound on the timestep used to *advance* the strip.
///
/// `Time::<Virtual>` is deliberately given a 10s `max_delta` (see [`crate::ecs`])
/// so `elapsed()` keeps tracking wall-clock across a stalled frame — the
/// finger-lift timeout below depends on that. It is the wrong bound for
/// integration: a frame delayed by a blocking AX round trip would otherwise
/// advance the strip by `velocity * dt * viewport_width` in one step and slam
/// it into the far clamp bound. Integrate at most one 30fps frame per update,
/// however long the frame actually took.
const MAX_STEP_SECS: f64 = 1.0 / 30.0;

/// Lower bound on the timestep used to *derive* a velocity from a gesture delta.
///
/// Here the true delta is what we want — a backlog drained after a stall really
/// does represent that much finger travel over that much time. The hazard is
/// the opposite one: on a catch-up frame `dt` approaches zero, and
/// `gesture_delta / dt` diverges.
const MIN_STEP_SECS: f64 = 1.0 / 1000.0;

/// The on-screen strip, its origin, and the scroll state being applied to it.
type ScrollingStrip<'w, 's> = Single<
    'w,
    's,
    (
        &'static LayoutStrip,
        &'static mut Position,
        &'static mut Scrolling,
    ),
    (With<ActiveWorkspaceMarker>, Without<Window>),
>;

impl Plugin for ScrollEventsPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<PageGestureState>();
        app.add_observer(scroll_stopped);
        app.add_message::<PageFocusRequest>();
        // The overview owns the trackpad while it is up, like Mission Control.
        let mission_control_inactive =
            |mission_control: Option<Res<MissionControlActive>>,
             overview: Option<Res<crate::ecs::overview::OverviewPhase>>| {
                mission_control.is_none_or(|active| !active.0)
                    && overview.is_none_or(|phase| !phase.is_active())
            };

        app.add_systems(
            PreUpdate,
            paging_gesture
                .in_set(PreUpdateSet::Demux)
                .after(super::systems::demux_input_events)
                .run_if(mission_control_inactive)
                .run_if(paging_enabled)
                .run_if(on_message::<InputEvent>.or_else(paging_active)),
        );

        // Only the two gesture systems are gated on an input event. The rest of
        // the chain (inertia, snap force, integrator) must keep running after
        // the fingers stop sending events, since that's when they take over.
        app.add_systems(
            Update,
            (
                vertical_swipe_gesture
                    .run_if(mission_control_inactive)
                    .run_if(on_message::<InputEvent>),
                (
                    swipe_gesture
                        .run_if(mission_control_inactive)
                        .run_if(on_message::<InputEvent>),
                    apply_inertia,
                    apply_snap_force,
                    scrolling_integrator,
                    apply_scrolling_constraints,
                    swiping_timeout,
                )
                    .chain(),
            )
                .in_set(UpdateSet::Input),
        );
    }
}

fn paging_gesture(
    mut messages: MessageReader<InputEvent>,
    active_workspace: Single<Entity, With<ActiveWorkspaceMarker>>,
    config: Res<Config>,
    time: Res<Time>,
    mut state: ResMut<PageGestureState>,
    mut events: MessageWriter<PageFocusRequest>,
    mut commands: Commands,
) {
    let current_config = config.swipe_paging_config();
    if state.config.as_ref() != Some(&current_config) {
        state.reset();
        state.config = Some(current_config.clone());
    }
    if !current_config.0 {
        return;
    }

    if state
        .last_event
        .is_some_and(|last| time.elapsed().abs_diff(last) > PAGE_GESTURE_TIMEOUT)
    {
        state.accumulated_cm = 0.0;
        state.last_event = None;
        state.active = false;
    }

    for InputEvent(event) in messages.read() {
        match event {
            Event::TouchpadDown => {
                state.reset();
                state.active = true;
            }
            Event::TouchpadUp => {
                state.accumulated_cm = 0.0;
                state.last_event = None;
                state.active = false;
            }
            Event::Swipe {
                distance_cm,
                fingers,
                ..
            } if Some(*fingers) == current_config.1 => {
                state.last_event = Some(time.elapsed());
                state.active = true;
                if !distance_cm.is_finite() {
                    continue;
                }
                state.accumulated_cm += *distance_cm;
                if !state.accumulated_cm.is_finite() {
                    state.accumulated_cm = 0.0;
                    continue;
                }
                commands.entity(*active_workspace).try_remove::<Scrolling>();
                state.earn_steps(current_config.3, current_config.2.clone());
            }
            Event::Swipe { .. } => state.reset(),
            _ => {}
        }
    }

    if state.in_flight.is_none()
        && let Some(direction) = state.next_step()
    {
        state.in_flight = Some(PageInFlight::Dispatched);
        events.write(PageFocusRequest {
            direction,
            generation: state.generation,
        });
    }
}

#[instrument(level = Level::TRACE, skip_all)]
fn swipe_gesture(
    mut messages: MessageReader<InputEvent>,
    active_display: ActiveDisplay,
    mut active_workspace: Single<
        (Entity, &Position, Option<&mut Scrolling>),
        With<ActiveWorkspaceMarker>,
    >,
    time: Res<Time>,
    config: Res<Config>,
    mut commands: Commands,
) {
    let swipe_sensitivity = config.swipe_sensitivity();
    let mut total_delta = 0.0;
    let mut velocity_samples = VelocitySamples::default();
    let mut touchpad_down = false;
    let mut touchpad_up = false;
    let mut has_scroll_event = false;
    let mut has_gesture_event = false;

    // Normalization: Touchpad deltas are typically small fractions.
    // Scroll wheel deltas can be larger. We scale it down slightly
    // to match the "feel" of a finger swipe.
    let scroll_scale = scroll_wheel_scale(swipe_sensitivity);

    for InputEvent(event) in messages.read() {
        match event {
            Event::TouchpadDown => {
                touchpad_down = true;
                touchpad_up = false;
                total_delta = 0.0;
            }
            Event::TouchpadUp => {
                touchpad_up = true;
            }
            Event::Scroll { delta } => {
                total_delta += *delta * scroll_scale;
                has_scroll_event = true;
            }
            Event::Swipe {
                delta,
                fingers,
                elapsed_secs,
                ..
            } if config
                .swipe_gesture_fingers()
                .is_some_and(|fingers_configured| fingers_configured == *fingers)
                && !config.swipe_gesture_paging() =>
            {
                touchpad_up = false;
                total_delta += delta;
                velocity_samples.add(*delta, *elapsed_secs);
                has_scroll_event = true;
                has_gesture_event = true;
            }
            _ => (),
        }
    }

    if !touchpad_down && !touchpad_up && !has_scroll_event {
        return;
    }

    let (entity, position, scrolling) = &mut *active_workspace;

    // The user is driving the strip by hand now, so an earlier deliberate
    // placement (center, snap) no longer describes where they want it.
    if (touchpad_down || has_scroll_event)
        && let Ok(mut entity_commands) = commands.get_entity(*entity)
    {
        entity_commands.try_remove::<ManualStripOffset>();
    }

    if touchpad_down && let Some(scrolling) = scrolling.as_mut() {
        // A replacement finger can report Began before the gesture ends.
        // Only a new touch after coasting has started should cancel inertia.
        if !scrolling.is_user_swiping {
            scrolling.velocity = 0.0;
        }
        scrolling.is_user_swiping = true;
        scrolling.last_event = time.elapsed();
    }

    if has_scroll_event {
        let viewport_width = f64::from(active_display.bounds().width());
        let direction_modifier = swipe_direction_modifier(config.swipe_gesture_direction());

        // Gesture samples carry the time spanned by their finger displacement.
        // An ECS frame can process them much sooner (or later) than that span.
        let dt = velocity_samples
            .elapsed_secs
            .unwrap_or_else(|| time.delta_secs_f64().max(MIN_STEP_SECS));
        let velocity_sample_valid = velocity_samples.count > 0 && dt > 0.0;
        let new_velocity = if velocity_sample_valid {
            velocity_samples.delta * swipe_sensitivity / dt
        } else {
            0.0
        };

        if let Some(scrolling) = scrolling.as_mut() {
            // Native modifier-scroll events already include macOS momentum.
            // Add synthetic inertia only for raw multi-finger gestures.
            scrolling.velocity = if velocity_sample_valid {
                // Smoothen gesture velocity changes using EMA.
                0.3 * new_velocity + 0.7 * scrolling.velocity
            } else if has_gesture_event {
                scrolling.velocity
            } else {
                0.0
            };
            scrolling.is_user_swiping = true;
            scrolling.last_event = time.elapsed();
            scrolling.rubber_band = has_gesture_event;
            scrolling.position +=
                total_delta * viewport_width * direction_modifier * swipe_sensitivity;
        } else if let Ok(mut entity_commands) = commands.get_entity(*entity) {
            entity_commands.try_insert(Scrolling {
                velocity: new_velocity,
                position: f64::from(position.0.x)
                    + total_delta * viewport_width * direction_modifier * swipe_sensitivity,
                is_user_swiping: !touchpad_up,
                last_event: time.elapsed(),
                rubber_band: has_gesture_event,
                ends: None,
            });
        }
    }
    if touchpad_up && let Some(scrolling) = scrolling.as_mut() {
        scrolling.release(time.elapsed());
    }
}

#[instrument(level = Level::TRACE, skip_all)]
pub(super) fn swiping_timeout(
    strips: Populated<(Entity, &Scrolling), With<LayoutStrip>>,
    active_display: ActiveDisplay,
    time: Res<Time>,
    config: Res<Config>,
    window_manager: Res<WindowManager>,
    mut commands: Commands,
) {
    const MIN_VELOCITY_PX: f64 = 5.0;
    let viewport_width = f64::from(active_display.bounds().width());

    for (entity, scroll) in strips {
        let since_last_event = time.elapsed().abs_diff(scroll.last_event);
        if since_last_event > FINGER_LIFT_THRESHOLD {
            if scroll.is_user_swiping {
                // Stationary fingers may send no displacement. Keep the
                // stretch until lift, with a fallback for a lost phase event.
                if scroll.past_end().is_some() && since_last_event < STRETCHED_GESTURE_TIMEOUT {
                    continue;
                }
                // Silence cannot prove the fingers lifted. Drop stale state
                // without ever converting it into a coast.
                if let Ok(mut entity_commands) = commands.get_entity(entity) {
                    entity_commands.try_remove::<Scrolling>();
                }
                continue;
            }

            // Under exponential decay, velocity / decay_rate is the total
            // distance still to come. A short frame says nothing about that.
            let remaining_px = scroll.velocity.abs() * viewport_width / config.swipe_deceleration();
            let remove = scroll.past_end().is_none() && remaining_px < MIN_VELOCITY_PX;
            if remove {
                if let Ok(mut entity_commands) = commands.get_entity(entity) {
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
}

#[instrument(level = Level::TRACE, skip_all)]
fn apply_inertia(
    mut strips: Populated<(Entity, &mut Scrolling), With<LayoutStrip>>,
    time: Res<Time>,
    config: Res<Config>,
) {
    let dt = time.delta_secs_f64();
    for (_, mut scroll) in &mut strips {
        if scroll.is_user_swiping || scroll.past_end().is_some() {
            continue;
        }

        if scroll.velocity.abs() > 0.001 {
            // Unbounded on purpose: decay is monotonic toward zero, so a long
            // frame can only shed more momentum, never overshoot. After a stall
            // that is what we want.
            let decay_rate = config.swipe_deceleration();
            scroll.velocity *= (-decay_rate * dt).exp();
        } else {
            scroll.velocity = 0.0;
        }
    }
}

#[instrument(level = Level::TRACE, skip_all)]
fn apply_snap_force(
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

    let viewport = active_display.actual_bounds(&config);
    let viewport_center = viewport.center().x;
    let snap_threshold = SNAP_DISPLAY_RATIO * f64::from(viewport.width());

    let (strip, position, ref mut scroll) = *strip;
    if scroll.is_user_swiping || scroll.past_end().is_some() || scroll.velocity.abs() > 0.5 {
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
        // This is an exponential approach, and it only pulls *toward* the
        // target while the factor stays under 1. Past that it overshoots to the
        // far side; at a 10s delta it would fling the strip 100x the distance
        // the wrong way. Clamping the factor rather than `dt` keeps that true
        // for any timestep and any `CENTER_MAGNETIC_FORCE`.
        let approach = (time.delta_secs_f64() * CENTER_MAGNETIC_FORCE).min(1.0);
        scroll.position -= dist_to_snap * approach;
    }
}

#[instrument(level = Level::TRACE, skip_all)]
fn scrolling_integrator(
    mut strip: Single<&mut Scrolling, With<LayoutStrip>>,
    time: Res<Time>,
    active_display: ActiveDisplay,
    config: Res<Config>,
) {
    let dt = time.delta_secs_f64().min(MAX_STEP_SECS);
    let viewport = active_display.actual_bounds(&config);
    let viewport_width = f64::from(viewport.width());

    // Direction modifier: Natural moves strip left (negative offset) for positive delta (finger left)
    let direction_modifier = swipe_direction_modifier(config.swipe_gesture_direction());

    let scroll = &mut *strip;
    // Finger displacement is applied directly by swipe_gesture.
    if scroll.is_user_swiping || viewport_width <= 0.0 {
        return;
    }
    let scale = viewport_width * direction_modifier;
    let speed = scroll.velocity * scale;
    if scroll.rubber_band
        && let Some(ends) = scroll.ends
    {
        let (position, speed) = coast(scroll.position, speed, dt, ends);
        scroll.position = position;
        scroll.velocity = speed / scale;
    } else if scroll.velocity.abs() > 0.0001 {
        scroll.position += speed * dt;
    }
}

#[instrument(level = Level::TRACE, skip_all)]
fn apply_scrolling_constraints(
    mut strip: ScrollingStrip,
    active_display: ActiveDisplay,
    windows: Windows,
    config: Res<Config>,
) {
    let viewport = active_display.actual_bounds(&config);
    let (strip, ref mut position, ref mut scroll) = *strip;

    let get_window_frame = |entity| windows.moving_frame(entity);
    let Some((min, max)) = strip_ends(strip, &windows, &get_window_frame, &viewport, &config)
    else {
        scroll.velocity = 0.0;
        scroll.ends = None;
        return;
    };
    let ends = (f64::from(min.min(max)), f64::from(max.max(min)));
    scroll.ends = Some(ends);
    if scroll.rubber_band {
        let shown = scroll.past_end().map_or(scroll.position, |end| {
            end + rubber_band(scroll.position - end, f64::from(viewport.width().max(1)))
        });
        position.x = round_px(shown);
    } else {
        scroll.position = scroll.position.clamp(ends.0, ends.1);
        position.x = round_px(scroll.position);
    }
}

type InterruptedScrolls<'w, 's> = Query<
    'w,
    's,
    (
        &'static Scrolling,
        &'static mut Position,
        Option<&'static mut PreviousStripPosition>,
    ),
    With<LayoutStrip>,
>;

/// An interrupted scroll must leave the strip within its bounds, including
/// when switching away saves the stretched position for later restoration.
fn scroll_stopped(trigger: On<Remove<Scrolling>>, mut strips: InterruptedScrolls) {
    let Ok((scroll, mut position, previous)) = strips.get_mut(trigger.entity) else {
        return;
    };
    let Some(end) = scroll.past_end() else {
        return;
    };
    let end = round_px(end);
    if let Some(mut previous) = previous {
        previous.origin.x = end;
    } else {
        position.x = end;
    }
}

/// How far a pull of the strip shows past an end: half as far as the fingers
/// pull it at first, and less the further they do, never as far as the
/// display is `wide`, as macOS's rubber band.
const RUBBER_BAND: f64 = 0.55;

/// How fast a strip let go past its end springs back, critically damped, in
/// radians a second: back from a pull in 0.3 s, settled in 0.6; a glide into
/// the end goes on past it for 60 ms, as far as its speed over 44.
const SPRING: f64 = 16.0;

/// The speed a glide goes past an end at most, in pixels a second: past it
/// 230 pixels, shown 120 on a 1800-pixel display. A quick flick reaches the end
/// at 80,000, and went most of a display past it.
const MAX_SPEED_PAST_END: f64 = 10_000.0;

impl Scrolling {
    fn release(&mut self, now: Duration) {
        if self.past_end().is_some() && now.abs_diff(self.last_event) > FINGER_LIFT_THRESHOLD {
            self.velocity = 0.0;
        }
        self.is_user_swiping = false;
    }

    /// The end the strip is past, if it is.
    fn past_end(&self) -> Option<f64> {
        self.ends
            .filter(|_| self.rubber_band)
            .and_then(|ends| end_past(self.position, ends))
    }
}

fn end_past(position: f64, (min, max): (f64, f64)) -> Option<f64> {
    if position < min {
        Some(min)
    } else if position > max {
        Some(max)
    } else {
        None
    }
}

/// How far the strip shows past an end, pulled `past` it on a display `wide`.
fn rubber_band(past: f64, wide: f64) -> f64 {
    let pulled = past.abs() * RUBBER_BAND / wide;
    pulled / (pulled + 1.0) * wide * past.signum()
}

/// A frame, `dt` long, of a strip let go at `position` going at `speed`, in
/// pixels and pixels a second: it goes on, and past one of its `ends` springs
/// back to it, from the end if it reaches it in the frame.
fn coast(position: f64, speed: f64, dt: f64, ends: (f64, f64)) -> (f64, f64) {
    let (end, position, dt) = match (
        end_past(position, ends),
        end_past(position + speed * dt, ends),
    ) {
        (Some(end), _) => (end, position, dt),
        (None, Some(end)) => (end, end, dt - (end - position) / speed),
        (None, None) => return (position + speed * dt, speed),
    };
    let (past, speed) = spring(position - end, speed, dt);
    (end + past, speed)
}

/// A frame, `dt` long, of a strip let go `past` its end at `speed`: it springs
/// back, critically damped, and comes to rest on the end once nearly there.
fn spring(past: f64, speed: f64, dt: f64) -> (f64, f64) {
    const REST_PX: f64 = 0.5;
    const REST_PX_PER_SEC: f64 = 20.0;
    let speed = if past * speed >= 0.0 {
        speed.clamp(-MAX_SPEED_PAST_END, MAX_SPEED_PAST_END)
    } else {
        speed
    };
    let carried = speed + SPRING * past;
    let fading = (-SPRING * dt).exp();
    let past = (past + carried * dt) * fading;
    let speed = (speed - SPRING * carried * dt) * fading;
    if past.abs() < REST_PX && speed.abs() < REST_PX_PER_SEC {
        (0.0, 0.0)
    } else {
        (past, speed)
    }
}

#[instrument(level = Level::TRACE, skip_all)]
fn strip_ends<W>(
    layout_strip: &LayoutStrip,
    windows: &Windows,
    get_window_frame: &W,
    viewport: &IRect,
    config: &Config,
) -> Option<(i32, i32)>
where
    W: Fn(Entity) -> Option<IRect>,
{
    let geometry = |entity| {
        windows
            .layout_position(entity)
            .zip(get_window_frame(entity))
            .map(|(position, frame)| (position.x, position.x + frame.width()))
    };
    let live_column = |column| layout_strip.resolve_column(column, geometry);
    let (left_snap, total_strip_width) = layout_strip.columns().rev().find_map(live_column)?;

    Some(
        if config.continuous_swipe()
            && let Some((right_snap, _)) = layout_strip.columns().filter_map(live_column).nth(1)
        {
            // Allow to scroll away until the last or first window snaps.
            (viewport.min.x - left_snap, viewport.max.x - right_snap)
        } else if viewport.width() < total_strip_width {
            // Snap the strip directly to the edges.
            (viewport.max.x - total_strip_width, viewport.min.x)
        } else {
            // Snap the strip directly to the edges.
            (viewport.min.x, viewport.max.x - total_strip_width)
        },
    )
}

#[derive(Default)]
struct VerticalGestureState {
    accumulated: f64,
    last_event: Option<Instant>,
    fired: bool,
}

#[instrument(level = Level::TRACE, skip_all)]
fn vertical_swipe_gesture(
    mut messages: MessageReader<InputEvent>,
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

    for InputEvent(event) in messages.read() {
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
mod rubber_band_tests {
    use super::*;

    const FRAME: f64 = 1.0 / 120.0;

    #[test]
    fn a_pull_past_the_end_shows_less_the_further_it_goes() {
        let wide = 1000.0;
        assert!(rubber_band(0.0, wide).abs() < f64::EPSILON);
        assert!((rubber_band(1.0, wide) - RUBBER_BAND).abs() < 0.01);
        assert!((rubber_band(-300.0, wide) + rubber_band(300.0, wide)).abs() < f64::EPSILON);
        let shown: Vec<f64> = (1..100)
            .map(|pull| rubber_band(f64::from(pull) * 100.0, wide))
            .collect();
        let steps: Vec<f64> = shown.windows(2).map(|pair| pair[1] - pair[0]).collect();
        assert!(steps.iter().all(|step| *step > 0.0));
        assert!(steps.windows(2).all(|pair| pair[1] < pair[0]));
        assert!(shown.last().is_some_and(|last| *last < wide));
    }

    #[test]
    fn a_strip_let_go_past_its_end_springs_back_without_passing_it() {
        let (mut past, mut speed) = (300.0, 0.0);
        let mut took = 0.0;
        while past != 0.0 || speed != 0.0 {
            (past, speed) = spring(past, speed, FRAME);
            took += FRAME;
            assert!(past >= 0.0, "it passed the end: {past}");
            assert!(took < 1.0, "it never came to rest: {past} at {speed}");
        }
        assert!((0.3..0.8).contains(&took), "back in {took} s");
    }

    #[test]
    fn a_glide_into_the_end_goes_a_little_past_it_however_fast() {
        let ends = (-1000.0, 0.0);
        let most = MAX_SPEED_PAST_END / (SPRING * std::f64::consts::E);
        for speed in [2_000.0, 10_000.0, 100_000.0] {
            let (mut position, mut now) = (-1.0, speed);
            let mut deepest: f64 = 0.0;
            for _ in 0..120 {
                (position, now) = coast(position, now, FRAME, ends);
                deepest = deepest.max(position);
            }
            assert!(
                deepest > 0.0 && deepest <= most + 1.0,
                "{speed}: {deepest} past the end, at most {most}"
            );
            assert!(position.abs() < f64::EPSILON && now.abs() < f64::EPSILON);
        }
    }

    #[test]
    fn switching_away_restores_the_edge_without_moving_the_parked_strip() {
        use crate::manager::Origin;

        let mut app = App::new();
        app.add_observer(scroll_stopped);
        let parked = Origin::new(1000, 700);
        let strip = app
            .world_mut()
            .spawn((
                LayoutStrip::new(1, 0),
                Position(parked),
                PreviousStripPosition {
                    origin: Origin::new(85, 20),
                    focus: None,
                },
                Scrolling {
                    position: 150.0,
                    velocity: 0.0,
                    is_user_swiping: true,
                    last_event: Duration::ZERO,
                    rubber_band: true,
                    ends: Some((-400.0, 0.0)),
                },
            ))
            .id();
        app.world_mut().entity_mut(strip).remove::<Scrolling>();

        let strip = app.world().entity(strip);
        assert_eq!(strip.get::<Position>().expect("parked position").0, parked);
        assert_eq!(
            strip
                .get::<PreviousStripPosition>()
                .expect("saved position")
                .origin,
            Origin::new(0, 20)
        );
    }

    #[test]
    fn a_strip_thrown_back_from_past_its_end_glides_on_inside() {
        let ends = (-1000.0, 0.0);
        let (position, speed) = coast(20.0, -5_000.0, FRAME, ends);
        assert!(position < 0.0 && speed < -1_000.0, "{position} at {speed}");
        let (next, same) = coast(position, speed, FRAME, ends);
        assert!((next - (position + speed * FRAME)).abs() < 1e-9 && (same - speed).abs() < 1e-9);
    }
}

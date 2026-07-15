use bevy::app::{App, Plugin, Update};
use bevy::ecs::entity::Entity;
use bevy::ecs::hierarchy::ChildOf;
use bevy::ecs::message::MessageReader;
use bevy::ecs::query::{With, Without};
use bevy::ecs::schedule::IntoScheduleConfigs as _;
use bevy::ecs::system::{Commands, Local, Populated, Query, Res, Single};
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
    ActiveWorkspaceMarker, DockPosition, MissionControlActive, Position, Scrolling,
    SendMessageTrigger,
};
use crate::errors::Result;
use crate::events::Event;
use crate::manager::{Display, Window, WindowManager};
use crate::platform::Modifiers;

pub struct ScrollEventsPlugin;

const NATIVE_SCROLL_RESPONSE_SECONDS: f64 = 0.04;
const NATIVE_SCROLL_SETTLE_PX: f64 = 0.25;

impl Plugin for ScrollEventsPlugin {
    fn build(&self, app: &mut App) {
        let mission_control_inactive = |mission_control: Option<Res<MissionControlActive>>| {
            mission_control.is_none_or(|active| !active.0)
        };

        app.add_systems(
            Update,
            (
                cleanup_detached_scrolling,
                vertical_swipe_gesture.run_if(mission_control_inactive),
                (
                    swipe_gesture.run_if(mission_control_inactive),
                    apply_inertia,
                    apply_snap_force,
                    scrolling_integrator,
                    apply_scrolling_constraints,
                    swiping_timeout,
                )
                    .chain()
                    .after(crate::ecs::workspace::show_active_workspace),
            ),
        );
    }
}

#[allow(clippy::needless_pass_by_value)]
fn cleanup_detached_scrolling(
    detached: Query<Entity, (With<Scrolling>, Without<ChildOf>)>,
    mut commands: Commands,
) {
    for entity in detached {
        if let Ok(mut entity_commands) = commands.get_entity(entity) {
            entity_commands.try_remove::<Scrolling>();
        }
    }
}

#[allow(clippy::needless_pass_by_value)]
#[instrument(level = Level::TRACE, skip_all)]
fn swipe_gesture(
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
    let swipe_sensitivity = config.swipe_sensitivity();
    let snap_enabled = config.sticky_scroll() || config.auto_center();
    let mut scroll_delta = 0.0;
    let mut gesture_delta = 0.0;
    let mut touchpad_down = false;
    let mut has_scroll_event = false;
    let mut has_gesture_event = false;

    // Normalization: Touchpad deltas are typically small fractions.
    // Scroll wheel deltas can be larger. We scale it down slightly
    // to match the "feel" of a finger swipe.
    const SCROLL_SCALE_UPPER: f64 = 0.15;
    const SCROLL_SCALE_LOWER: f64 = 0.005;
    const SCROLL_FULL_RANGE: f64 = 2.0;
    let scroll_scale = SCROLL_SCALE_LOWER
        + ((SCROLL_SCALE_UPPER - SCROLL_SCALE_LOWER) / SCROLL_FULL_RANGE) * swipe_sensitivity;

    for event in messages.read() {
        match event {
            Event::TouchpadDown => {
                touchpad_down = true;
            }
            Event::Scroll { delta } => {
                scroll_delta += *delta * scroll_scale;
                has_scroll_event = true;
            }
            Event::Swipe { delta, fingers }
                if config
                    .swipe_gesture_fingers()
                    .is_some_and(|fingers_configured| fingers_configured == *fingers) =>
            {
                gesture_delta += *delta;
                has_scroll_event = true;
                has_gesture_event = true;
            }
            _ => (),
        }
    }

    if !touchpad_down && !has_scroll_event {
        return;
    }

    let (entity, position, scrolling) = &mut *active_workspace;

    if touchpad_down && let Some(scrolling) = scrolling.as_mut() {
        scrolling.velocity = 0.0;
        scrolling.target_position = None;
        scrolling.snap_pending = snap_enabled;
        scrolling.is_user_swiping = true;
        scrolling.last_event = Instant::now();
    }

    if has_scroll_event {
        let viewport_width = f64::from(active_display.bounds().width());
        let direction_modifier = match config.swipe_gesture_direction() {
            SwipeGestureDirection::Natural => -1.0,
            SwipeGestureDirection::Reversed => 1.0,
        };

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
            // Native modifier-scroll events already include macOS momentum.
            // Add synthetic inertia only for raw multi-finger gestures.
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
        } else if let Ok(mut entity_commands) = commands.get_entity(*entity) {
            let initial_position = f64::from(position.0.x) + gesture_distance;
            entity_commands.try_insert(Scrolling {
                velocity: new_velocity,
                position: initial_position,
                target_position: (scroll_delta != 0.0)
                    .then_some(initial_position + scroll_distance),
                snap_pending: snap_enabled,
                is_user_swiping: touchpad_down || has_scroll_event,
                last_event: Instant::now(),
            });
        }
    }
}

#[allow(clippy::needless_pass_by_value)]
#[instrument(level = Level::TRACE, skip_all)]
pub(super) fn swiping_timeout(
    strips: Populated<(Entity, &mut Scrolling, &ChildOf), With<LayoutStrip>>,
    displays: Query<(&Display, Option<&DockPosition>)>,
    time: Res<Time>,
    config: Res<Config>,
    window_manager: Res<WindowManager>,
    mut commands: Commands,
) {
    const FINGER_LIFT_THRESHOLD: Duration = Duration::from_millis(50);
    let dt = time.delta_secs_f64();

    for (entity, mut scroll, parent) in strips {
        let Ok((display, dock)) = displays.get(parent.parent()) else {
            continue;
        };
        let viewport_width = f64::from(display.actual_display_bounds(dock, &config).width());
        let timed_out = scroll.last_event.elapsed() > FINGER_LIFT_THRESHOLD;
        let outcome = update_swipe_timeout(&mut scroll, timed_out, dt, viewport_width);
        if outcome.remove
            && let Ok(mut entity_commands) = commands.get_entity(entity)
        {
            entity_commands.try_remove::<Scrolling>();
        }
        if outcome.emit_mouse_moved
            && let Some(point) = window_manager.cursor_position()
        {
            commands.trigger(SendMessageTrigger(Event::MouseMoved {
                point,
                modifiers: Modifiers::empty(),
            }));
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct SwipeTimeoutOutcome {
    emit_mouse_moved: bool,
    remove: bool,
}

fn update_swipe_timeout(
    scroll: &mut Scrolling,
    timed_out: bool,
    dt: f64,
    viewport_width: f64,
) -> SwipeTimeoutOutcome {
    const MIN_VELOCITY_PX: f64 = 5.0;
    let emit_mouse_moved = timed_out && scroll.is_user_swiping;
    if emit_mouse_moved {
        scroll.is_user_swiping = false;
    }
    SwipeTimeoutOutcome {
        emit_mouse_moved,
        remove: timed_out
            && scroll.velocity.abs() * dt * viewport_width < MIN_VELOCITY_PX
            && scroll.target_position.is_none()
            && !scroll.snap_pending,
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
    mut strips: Populated<(&LayoutStrip, &Position, &mut Scrolling, &ChildOf)>,
    displays: Query<(&Display, Option<&DockPosition>)>,
    windows: Windows,
    config: Res<Config>,
) {
    const SNAP_DISPLAY_RATIO: f64 = 0.45;

    let sticky = config.sticky_scroll();
    for (layout_strip, position, mut scroll, parent) in &mut strips {
        if !sticky && !config.auto_center() {
            scroll.snap_pending = false;
            continue;
        }
        let Ok((display, dock)) = displays.get(parent.parent()) else {
            scroll.snap_pending = false;
            continue;
        };
        let viewport = display.actual_display_bounds(dock, &config);
        let viewport_center = viewport.center().x;
        let snap_threshold = SNAP_DISPLAY_RATIO * f64::from(viewport.width());

        if scroll.is_user_swiping || scroll.velocity.abs() > 0.5 || scroll.target_position.is_some()
        {
            continue;
        }

        let target_offset = layout_strip
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
        scroll.snap_pending = false;
        if sticky || dist_to_snap.abs() < snap_threshold {
            scroll.velocity = 0.0;
            scroll.target_position = Some(f64::from(target_offset));
        }
    }
}

#[allow(clippy::needless_pass_by_value)]
#[instrument(level = Level::TRACE, skip_all)]
fn scrolling_integrator(
    mut strips: Populated<(&mut Scrolling, &ChildOf), With<LayoutStrip>>,
    time: Res<Time>,
    displays: Query<(&Display, Option<&DockPosition>)>,
    config: Res<Config>,
) {
    let dt = time.delta_secs_f64();

    // Direction modifier: Natural moves strip left (negative offset) for positive delta (finger left)
    let direction_modifier = match config.swipe_gesture_direction() {
        SwipeGestureDirection::Natural => -1.0,
        SwipeGestureDirection::Reversed => 1.0,
    };

    for (mut scroll, parent) in &mut strips {
        let viewport_width = displays
            .get(parent.parent())
            .map_or(0.0, |(display, dock)| {
                f64::from(display.actual_display_bounds(dock, &config).width())
            });
        integrate_scrolling(&mut scroll, dt, viewport_width, direction_modifier);
    }
}

fn integrate_scrolling(
    scroll: &mut Scrolling,
    dt: f64,
    viewport_width: f64,
    direction_modifier: f64,
) {
    if let Some(target) = scroll.target_position {
        let (position, settled) = smooth_native_scroll(scroll.position, target, dt);
        scroll.position = position;
        if settled {
            scroll.target_position = None;
        }
    } else if scroll.velocity.abs() > 0.0001 {
        scroll.position += scroll.velocity * dt * viewport_width * direction_modifier;
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
    mut strips: Populated<(&LayoutStrip, &mut Position, &mut Scrolling, &ChildOf), Without<Window>>,
    displays: Query<(&Display, Option<&DockPosition>)>,
    windows: Windows,
    config: Res<Config>,
) {
    for (strip, mut position, mut scroll, parent) in &mut strips {
        let Ok((display, dock)) = displays.get(parent.parent()) else {
            continue;
        };
        let viewport = display.actual_display_bounds(dock, &config);
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

    Some(
        if continuous_swipe
            && !has_oversized_column
            && let Some((left_snap, right_snap)) = left_snap.zip(right_snap)
        {
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
mod tests {
    use std::time::{Duration, Instant};

    use bevy::ecs::query::With;
    use bevy::prelude::{App, Entity, Update};
    use bevy::time::TimeUpdateStrategy;

    use super::{
        ChildOf, Scrolling, cleanup_detached_scrolling, integrate_scrolling,
        reconcile_integrated_position, smooth_native_scroll, update_swipe_timeout,
    };
    use crate::commands::Command;
    use crate::ecs::{ActiveWorkspaceMarker, Position};
    use crate::events::Event;
    use crate::tests::TestHarness;

    #[test]
    fn native_scroll_smoothing_converges_without_overshoot() {
        let mut position = 0.0;
        let mut settled = false;

        for _ in 0..120 {
            let previous = position;
            (position, settled) = smooth_native_scroll(position, 100.0, 1.0 / 60.0);
            assert!(position >= previous);
            assert!(position <= 100.0);

            if settled {
                break;
            }
        }

        assert!((position - 100.0).abs() < f64::EPSILON);
        assert!(settled);
    }

    #[test]
    fn detached_strip_cancels_scrolling_on_next_ecs_update() {
        let mut app = App::new();
        app.add_systems(Update, cleanup_detached_scrolling);
        let parent = app.world_mut().spawn_empty().id();
        let strip = app
            .world_mut()
            .spawn((Scrolling::default(), ChildOf(parent)))
            .id();
        app.world_mut().entity_mut(strip).remove::<ChildOf>();

        app.update();

        assert!(!app.world().entity(strip).contains::<Scrolling>());
    }

    #[test]
    fn two_scrolling_strips_integrate_independently() {
        let now = Instant::now();
        let mut first = Scrolling {
            velocity: 1.0,
            position: 10.0,
            last_event: now,
            ..Scrolling::default()
        };
        let mut second = Scrolling {
            velocity: -2.0,
            position: -30.0,
            last_event: now,
            ..Scrolling::default()
        };
        integrate_scrolling(&mut first, 0.1, 1000.0, 1.0);
        integrate_scrolling(&mut second, 0.1, 500.0, 1.0);
        assert!((first.position - 110.0).abs() < f64::EPSILON);
        assert!((second.position + 130.0).abs() < f64::EPSILON);
    }

    #[test]
    fn timeout_uses_each_parent_display_width_and_emits_lift_once() {
        let now = Instant::now();
        let mut narrow = Scrolling {
            velocity: 1.0,
            is_user_swiping: true,
            last_event: now,
            ..Scrolling::default()
        };
        let mut wide = Scrolling {
            velocity: 1.0,
            is_user_swiping: true,
            last_event: now,
            ..Scrolling::default()
        };
        let narrow_result = update_swipe_timeout(&mut narrow, true, 0.016, 100.0);
        let wide_result = update_swipe_timeout(&mut wide, true, 0.016, 1000.0);
        assert!(narrow_result.remove);
        assert!(!wide_result.remove);
        assert!(narrow_result.emit_mouse_moved);
        assert!(wide_result.emit_mouse_moved);
        assert!(
            !update_swipe_timeout(&mut narrow, true, 0.016, 100.0).emit_mouse_moved,
            "synthetic mouse move is emitted only on the swiping transition"
        );
    }

    #[test]
    fn clamp_reconciliation_preserves_remainder_only_inside_boundary() {
        assert!((reconcile_integrated_position(-0.75, 0, 0) + 0.75).abs() < f64::EPSILON);
        assert!((reconcile_integrated_position(2.25, 2, 1) - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn scrolling_component_is_removed_after_integer_effective_dead_zone() {
        let commands = vec![
            Event::MenuOpened { window_id: 0 },
            Event::Command {
                command: Command::PrintState,
            },
        ];

        TestHarness::new()
            .with_windows(3)
            .on_iteration(0, |world, _state| {
                world.insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_millis(
                    16,
                )));
                let entity = {
                    let mut query = world.query_filtered::<Entity, With<ActiveWorkspaceMarker>>();
                    query.single(world).expect("one active workspace")
                };
                world.entity_mut(entity).insert(Scrolling {
                    velocity: 0.0,
                    position: 0.0,
                    target_position: Some(-1.0),
                    snap_pending: false,
                    is_user_swiping: false,
                    last_event: Instant::now()
                        .checked_sub(Duration::from_millis(100))
                        .expect("100ms must fit before now"),
                });
            })
            .on_iteration(1, |world, _state| {
                let mut query = world.query_filtered::<&Scrolling, With<ActiveWorkspaceMarker>>();
                assert!(query.single(world).is_err());
                let mut positions =
                    world.query_filtered::<&Position, With<ActiveWorkspaceMarker>>();
                assert_eq!(positions.single(world).expect("one active workspace").x, -1);
            })
            .run(commands);
    }
}

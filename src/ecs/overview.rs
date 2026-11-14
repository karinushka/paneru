//! ECS side of the overview: snapshots the strips into the pure layout,
//! drives the transition (toggle command, 4-finger scrub, clicks, scroll)
//! and hands frames to the native [`OverviewManager`].
//!
//! The real windows are only touched through the regular paths: selecting
//! a window focuses it, and focusing a window in a hidden row already
//! activates that row (see `focus::virtual_strip_activated`). While the
//! overview is open, hotkeys keep driving the real layout and the overview
//! follows it (`overview_follow_layout`).

use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use bevy::app::{App, Last, PreUpdate, Update};
use bevy::ecs::entity::Entity;
use bevy::ecs::hierarchy::ChildOf;
use bevy::ecs::message::MessageReader;
use bevy::ecs::query::{Added, Changed, Has, Or, With, Without};
use bevy::ecs::resource::Resource;
use bevy::ecs::schedule::IntoScheduleConfigs as _;
use bevy::ecs::schedule::common_conditions::on_message;
use bevy::ecs::system::{Commands, NonSendMut, Query, Res, ResMut, SystemParam};
use objc2_core_foundation::CGPoint;
use tracing::debug;

use crate::commands::Command;
use crate::config::Config;
use crate::ecs::layout::{Column, LayoutStrip, PARKED_STRIP_SLIVER, StackItem, strip_signature};
use crate::ecs::params::Windows;
use crate::ecs::workspace::PreviousStripPosition;
use crate::ecs::workspace::spawn_snap_strip_guard;
use crate::ecs::{
    ActiveWorkspaceMarker, Bounds, DockPosition, FocusedMarker, LayoutPosition, ManualStripOffset,
    Position, PreUpdateSet, RepositionMarker, Scrolling, SelectedVirtualMarker,
    SpawnCommandsExt as _, Unmanaged,
};
use crate::events::Event;
use crate::manager::WindowManager;
use crate::manager::{Display, Window};
use crate::overview::view::{DisplayScene, OverviewManager, WindowServerInfo, window_server_info};
use crate::overview::{
    DisplayLayout, Hit, Metrics, OVERVIEW_OPEN, Rect, SPRING_RESPONSE, Spring, StripInput,
    WindowInput, hit_test, layout_display, middle_row, reanchor, release_opens, rubber_band,
    same_geometry,
};

/// Progress per unit of normalized vertical finger travel: ~55% of the
/// trackpad's height opens the overview fully.
const GESTURE_GAIN: f64 = 1.8;

/// Last-resort release for a scrub that hears nothing for this long. Lifts
/// are normally caught from the touches themselves (see the input
/// handler), so this only guards against a stuck half-open overview; it is
/// long enough that pausing mid-swipe doesn't trigger it.
const SCRUB_TIMEOUT: Duration = Duration::from_millis(1500);

/// Downward finger travel (normalized, like the swipe delta) that cancels an
/// overview built on touch-down before it ever moved up: the swipe is
/// heading down, and there's nothing below closed to scrub into.
const CANCEL_TRAVEL: f64 = 0.02;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum Phase {
    #[default]
    Closed,
    Opening,
    Open,
    Closing,
    Scrubbing,
}

struct DisplayEntry {
    cg_bounds: Rect,
    layout: DisplayLayout<Entity>,
    /// Strip entities, parallel to `layout.strips`.
    strips: Vec<Entity>,
    /// Where each strip starts (the shown row) or will start when shown
    /// again (the others), parallel to `layout.strips`; what a pan is
    /// committed against.
    origins: Vec<bevy::math::IVec2>,
    /// Where this display parks its hidden rows.
    parked: bevy::math::IVec2,
}

#[derive(Resource, Default)]
pub struct OverviewState {
    phase: Phase,
    displays: Vec<DisplayEntry>,
    /// Progress at `spring.1`, or the scrubbed progress while tracking.
    progress: f64,
    spring: Option<(Spring, Instant)>,
    generation: u64,
    gesture_raw: f64,
    gesture_velocity: f64,
    /// When the last scrub sample arrived; see [`SCRUB_TIMEOUT`].
    last_sample: Option<Instant>,
    /// The close has written what the overview showed back to the ECS: no
    /// reopening into the stale layout, the next open builds afresh.
    committed: bool,
    /// Built on touch-down and not yet swiped up: a swipe down cancels it.
    prebuilt: bool,
    /// Downward travel so far while `prebuilt`; see [`CANCEL_TRAVEL`].
    down_travel: f64,
}

impl OverviewState {
    fn current(&self) -> (f64, f64) {
        match self.spring {
            Some((spring, start)) if self.phase != Phase::Scrubbing => {
                let t = start.elapsed().as_secs_f64();
                (spring.value(t), spring.velocity_at(t))
            }
            _ => (self.progress, self.gesture_velocity),
        }
    }

    pub fn is_active(&self) -> bool {
        self.phase != Phase::Closed
    }

    /// The main loop must keep ticking: a spring runs, or a scrub is in
    /// progress and its watchdog has to be able to fire.
    pub fn needs_frames(&self) -> bool {
        matches!(
            self.phase,
            Phase::Opening | Phase::Closing | Phase::Scrubbing
        )
    }
}

/// Run condition: pointer and swipe systems pause while the overview owns
/// the screen, so clicks on cards don't also reach the windows beneath.
pub fn overview_inactive(state: Option<Res<OverviewState>>) -> bool {
    state.is_none_or(|state| !state.is_active())
}

pub fn register_overview(app: &mut App) {
    app.init_resource::<OverviewState>()
        .add_systems(
            PreUpdate,
            overview_handler
                .in_set(PreUpdateSet::Dispatch)
                .run_if(on_message::<Event>),
        )
        .add_systems(
            Update,
            overview_tick.run_if(|state: Res<OverviewState>| state.is_active()),
        )
        // After every layout system has written this frame's targets.
        .add_systems(
            Last,
            overview_follow_layout.run_if(|state: Res<OverviewState>| state.phase == Phase::Open),
        );
}

/// Per-frame work while the overview is up: release a scrub that went quiet,
/// and pick up the snapshots that landed from the background captures.
fn overview_tick(
    mut state: ResMut<OverviewState>,
    manager: Option<NonSendMut<OverviewManager>>,
    strips: StripQuery,
    windows: Windows,
    mut commands: Commands,
) {
    let Some(mut manager) = manager else {
        return;
    };
    if scrub_timed_out(&state, Instant::now()) {
        debug!("overview: scrub went quiet, releasing");
        release(&mut state, &mut manager, &strips, &windows, &mut commands);
    }
    manager.poll_captures();
}

type StripQuery<'w, 's> = Query<
    'w,
    's,
    (
        Entity,
        &'static LayoutStrip,
        &'static Position,
        Option<&'static RepositionMarker>,
        Option<&'static PreviousStripPosition>,
        Has<ActiveWorkspaceMarker>,
        &'static ChildOf,
        Has<SelectedVirtualMarker>,
    ),
>;

type WindowQuery<'w, 's> = Query<
    'w,
    's,
    (
        Entity,
        &'static Window,
        &'static LayoutPosition,
        &'static Bounds,
        Option<&'static Unmanaged>,
        Has<FocusedMarker>,
        &'static Position,
    ),
    Without<LayoutStrip>,
>;

/// Everything needed to snapshot the ECS into overview layouts.
#[derive(SystemParam)]
struct Scene<'w, 's> {
    displays: Query<'w, 's, (Entity, &'static Display, Option<&'static DockPosition>)>,
    strips: StripQuery<'w, 's>,
    windows: WindowQuery<'w, 's>,
    config: Res<'w, Config>,
    window_manager: Res<'w, WindowManager>,
}

#[allow(clippy::too_many_lines, reason = "one arm per overview event")]
fn overview_handler(
    mut messages: MessageReader<Event>,
    mut state: ResMut<OverviewState>,
    manager: Option<NonSendMut<OverviewManager>>,
    scene: Scene,
    windows: Windows,
    focused: Query<Entity, With<FocusedMarker>>,
    mut commands: Commands,
) {
    let Some(mut manager) = manager else {
        messages.clear();
        return;
    };
    for event in messages.read() {
        match event {
            Event::Command {
                command: Command::Overview,
            } => match state.phase {
                Phase::Closed => {
                    if open(&mut state, &mut manager, &scene) {
                        settle(&mut state, &mut manager, 1.0, 0.0);
                    }
                }
                // Already committed: let it finish closing.
                Phase::Closing if state.committed => {}
                Phase::Closing => settle(&mut state, &mut manager, 1.0, 0.0),
                Phase::Open | Phase::Opening | Phase::Scrubbing => {
                    close(
                        &mut state,
                        &mut manager,
                        &scene.strips,
                        &windows,
                        &mut commands,
                        None,
                    );
                }
            },

            Event::OverviewSwipe {
                delta,
                elapsed_secs,
            } => {
                if state.phase == Phase::Closed {
                    // Only an upward swipe opens.
                    if *delta <= 0.0 || !open(&mut state, &mut manager, &scene) {
                        continue;
                    }
                    state.gesture_raw = 0.0;
                    state.gesture_velocity = 0.0;
                    state.phase = Phase::Scrubbing;
                } else if state.committed {
                    continue;
                } else if state.phase != Phase::Scrubbing {
                    // Grab a running or settled transition where it is,
                    // heading for where letting go closes into.
                    preview_close(&state, &mut manager, &scene.strips, &windows);
                    let (p, _) = state.current();
                    state.gesture_raw = p;
                    state.gesture_velocity = 0.0;
                    state.phase = Phase::Scrubbing;
                }
                match downward_from_closed(&mut state, *delta) {
                    Some(true) => {
                        debug!("overview: swipe went down from closed, cancelling");
                        // The panel may already have taken key focus.
                        if let Ok(entity) = focused.single() {
                            commands.focus_entity(entity, true);
                        }
                        finish_close(&mut state, &mut manager);
                        continue;
                    }
                    Some(false) => continue,
                    None => {}
                }
                let step = delta * GESTURE_GAIN;
                // Nothing below closed: swiping down from it just stays put.
                state.gesture_raw = (state.gesture_raw + step).max(0.0);
                if *elapsed_secs > 0.0 {
                    let sample = step / elapsed_secs;
                    state.gesture_velocity = 0.3 * sample + 0.7 * state.gesture_velocity;
                }
                state.progress = rubber_band(state.gesture_raw);
                state.last_sample = Some(Instant::now());
                manager.set_progress(state.progress);
            }

            Event::OverviewCaptureLanded => manager.poll_captures(),

            Event::OverviewTouchDown if state.phase == Phase::Closed => {
                // Build now, at progress 0 (identical to the real screen), so
                // the first movement only has to move layers.
                if open(&mut state, &mut manager, &scene) {
                    state.gesture_raw = 0.0;
                    state.gesture_velocity = 0.0;
                    state.last_sample = Some(Instant::now());
                    state.phase = Phase::Scrubbing;
                    state.prebuilt = true;
                }
            }

            Event::OverviewSwipeEnd if state.phase == Phase::Scrubbing => {
                release(
                    &mut state,
                    &mut manager,
                    &scene.strips,
                    &windows,
                    &mut commands,
                );
            }

            Event::OverviewClick { point }
                if matches!(state.phase, Phase::Open | Phase::Opening) =>
            {
                select(
                    &mut state,
                    &mut manager,
                    *point,
                    &scene.strips,
                    &windows,
                    &mut commands,
                );
            }

            Event::OverviewScroll { point, dx, dy }
                if matches!(state.phase, Phase::Open | Phase::Opening) =>
            {
                scroll(&mut state, &mut manager, *point, *dx, *dy);
            }

            Event::OverviewDismiss if state.is_active() => {
                // The panel took key focus; hand it back.
                if let Ok(entity) = focused.single() {
                    commands.focus_entity(entity, true);
                }
                restore_rests(&state, &mut manager);
                settle(&mut state, &mut manager, 0.0, 0.0);
            }

            Event::OverviewAccept if state.is_active() && !state.committed => {
                close(
                    &mut state,
                    &mut manager,
                    &scene.strips,
                    &windows,
                    &mut commands,
                    None,
                );
            }

            Event::OverviewTransitionDone { generation } if *generation == state.generation => {
                match state.phase {
                    Phase::Closing => finish_close(&mut state, &mut manager),
                    Phase::Opening => state.phase = Phase::Open,
                    _ => {}
                }
            }
            _ => {}
        }
    }
}

/// A swipe sample on an overview built on touch-down: `None` once it has
/// moved up (scrub as usual), otherwise whether it has now gone down far
/// enough to cancel ([`CANCEL_TRAVEL`]); `Some(false)` ignores the sample.
fn downward_from_closed(state: &mut OverviewState, delta: f64) -> Option<bool> {
    if !state.prebuilt {
        return None;
    }
    if delta > 0.0 {
        state.prebuilt = false;
        return None;
    }
    state.down_travel -= delta;
    Some(state.down_travel > CANCEL_TRAVEL)
}

/// Removes the overview and forgets it, ready for the next open.
fn finish_close(state: &mut OverviewState, manager: &mut OverviewManager) {
    if let Some((_, start)) = state.spring {
        debug!("overview: closed {:?} after letting go", start.elapsed());
    }
    manager.teardown();
    OVERVIEW_OPEN.store(false, Ordering::Relaxed);
    *state = OverviewState {
        generation: state.generation,
        ..OverviewState::default()
    };
}

/// Ends a scrub: open or close depending on where and how fast it was let go.
fn release(
    state: &mut OverviewState,
    manager: &mut OverviewManager,
    strips: &StripQuery,
    windows: &Windows,
    commands: &mut Commands,
) {
    let (p, v) = (state.progress, state.gesture_velocity);
    state.last_sample = None;
    if state.prebuilt {
        // Lifted without ever moving up: nothing was looked at, so
        // nothing to commit.
        settle(state, manager, 0.0, 0.0);
    } else if release_opens(p, v) {
        settle(state, manager, 1.0, v);
    } else {
        commit(state, manager, strips, windows, commands, None);
        settle(state, manager, 0.0, v);
    }
}

/// Closes into what the overview shows (everything but Escape): see
/// [`commit`].
fn close(
    state: &mut OverviewState,
    manager: &mut OverviewManager,
    strips: &StripQuery,
    windows: &Windows,
    commands: &mut Commands,
    pick: Option<Pick>,
) {
    commit(state, manager, strips, windows, commands, pick);
    settle(state, manager, 0.0, 0.0);
}

/// The row a display closes into, and the window to bring into view there.
type Anchor = (u32, Option<Entity>);

/// A card or row the user clicked to close into.
struct Pick {
    display: usize,
    virtual_index: u32,
    focus: Option<Entity>,
}

/// What you see is what you get: every strip the user panned is left where
/// it shows, on every display, and the close animation lands there.
///
/// The shown row's strip moves at once (the panel covers the real windows
/// while they get there), marked as placed by hand so paneru's reshuffle
/// keeps it; a hidden row's remembered origin moves, for when it is shown
/// again. When the pan took the focused window out of view, focus moves to
/// the window nearest the middle, so paneru has nothing to scroll back for.
fn commit(
    state: &mut OverviewState,
    manager: &mut OverviewManager,
    strips: &StripQuery,
    windows: &Windows,
    commands: &mut Commands,
    pick: Option<Pick>,
) {
    let (anchors, wrote) = write_back(&state.displays, strips, windows, commands, pick.as_ref());
    state.committed = wrote || pick.is_some();
    for (entry, anchor) in state.displays.iter_mut().zip(anchors) {
        if let Some((virtual_index, focus)) = anchor {
            reanchor(&mut entry.layout, virtual_index, focus.as_ref());
        }
    }
    let layouts: Vec<_> = state.displays.iter().map(|d| &d.layout).collect();
    manager.retarget(&layouts);
}

/// The ECS half of [`commit`]: shows on each display the row the user left
/// in the middle (the clicked one on the clicked display), keeps every pan,
/// and moves focus when the pan took it out of view. Returns, per display,
/// the row to close into and the window to bring into view there.
fn write_back(
    displays: &[DisplayEntry],
    strips: &StripQuery,
    windows: &Windows,
    commands: &mut Commands,
    pick: Option<&Pick>,
) -> (Vec<Option<Anchor>>, bool) {
    let focused = windows.focused().map(|(_, entity)| entity);
    let mut anchors = Vec::new();
    let mut wrote = false;
    for (index, entry) in displays.iter().enumerate() {
        let picked = pick.filter(|pick| pick.display == index);
        let (shown, target) = close_rows(&entry.layout, picked);
        wrote |= keep_pans(entry, index, strips, windows, commands);
        let Some(target) = target else {
            anchors.push(None);
            continue;
        };
        let virtual_index = entry.layout.strips[target].virtual_index;
        let holds_focus = holds_focus(entry, shown, strips);
        let mut focus = picked.and_then(|pick| pick.focus);
        if picked.is_none() && Some(target) != shown {
            if let (Some(old), Some(&new)) = (shown, entry.strips.get(target)) {
                wrote = true;
                debug!("overview: display {index} closes into row {virtual_index}");
                if holds_focus && pick.is_none() {
                    // paneru's own switch hides the old row and restores
                    // the new one; focus goes to what is in the middle.
                    if let Ok(mut strip) = commands.get_entity(new) {
                        strip.try_insert(ActiveWorkspaceMarker);
                    }
                    focus = nearest_middle(&entry.layout, target);
                    if let Some(key) = focus {
                        commands.focus_entity(key, true);
                    }
                } else if let (Some(&old_strip), Some(&origin)) =
                    (entry.strips.get(old), entry.origins.get(old))
                {
                    #[allow(clippy::cast_possible_truncation, reason = "on-screen points")]
                    let dx = entry.layout.pan_points(old).round() as i32;
                    let origin = origin + bevy::math::IVec2::new(dx, 0);
                    show_row(old_strip, origin, new, entry.parked, commands);
                }
            }
        } else if picked.is_none()
            && pick.is_none()
            && let Some(focus_entity) = focused
            && let Some(key) = refocus(&entry.layout, focus_entity)
        {
            debug!("overview: focused window panned out of view, focusing {key}");
            commands.focus_entity(key, true);
            focus = Some(key);
            wrote = true;
        }
        anchors.push(Some((virtual_index, focus)));
    }
    (anchors, wrote)
}

/// The row a display shows now, and the one it closes into: the picked
/// one, else the one left in the middle.
fn close_rows(
    layout: &DisplayLayout<Entity>,
    picked: Option<&Pick>,
) -> (Option<usize>, Option<usize>) {
    let shown = layout.strips.iter().position(|strip| strip.active);
    let target = picked
        .and_then(|pick| {
            layout
                .strips
                .iter()
                .position(|strip| strip.virtual_index == pick.virtual_index)
        })
        .or_else(|| middle_row(layout))
        .or(shown);
    (shown, target)
}

/// Whether the display's shown row is the one holding focus.
fn holds_focus(entry: &DisplayEntry, shown: Option<usize>, strips: &StripQuery) -> bool {
    shown
        .and_then(|row| entry.strips.get(row))
        .and_then(|&strip| strips.get(strip).ok())
        .is_some_and(|strip| strip.5)
}

/// Aims the cards at where a close without a pick will leave the windows
/// (what [`commit`] does, without writing anything), so a swipe down from
/// the open overview heads there all the way rather than back to where the
/// windows were taken from. [`restore_rests`] undoes it.
fn preview_close(
    state: &OverviewState,
    manager: &mut OverviewManager,
    strips: &StripQuery,
    windows: &Windows,
) {
    let focused = windows.focused().map(|(_, entity)| entity);
    let layouts: Vec<_> = state
        .displays
        .iter()
        .map(|entry| {
            let mut layout = entry.layout.clone();
            let (shown, target) = close_rows(&layout, None);
            if let Some(target) = target {
                let focus = if Some(target) == shown {
                    focused.and_then(|key| refocus(&layout, key))
                } else if holds_focus(entry, shown, strips) {
                    nearest_middle(&layout, target)
                } else {
                    None
                };
                let virtual_index = layout.strips[target].virtual_index;
                reanchor(&mut layout, virtual_index, focus.as_ref());
            }
            layout
        })
        .collect();
    let layouts: Vec<_> = layouts.iter().collect();
    manager.retarget(&layouts);
}

/// Puts the cards' targets back to where the windows were taken from.
fn restore_rests(state: &OverviewState, manager: &mut OverviewManager) {
    let layouts: Vec<_> = state.displays.iter().map(|d| &d.layout).collect();
    manager.retarget(&layouts);
}

/// Writes each panned row of a display back: the shown row's strip moves,
/// a hidden row's remembered origin does. Returns whether any row was.
fn keep_pans(
    entry: &DisplayEntry,
    index: usize,
    strips: &StripQuery,
    windows: &Windows,
    commands: &mut Commands,
) -> bool {
    let mut wrote = false;
    for (row, strip) in entry.layout.strips.iter().enumerate() {
        #[allow(clippy::cast_possible_truncation, reason = "on-screen points")]
        let dx = entry.layout.pan_points(row).round() as i32;
        let (Some(&entity), Some(&origin)) = (entry.strips.get(row), entry.origins.get(row)) else {
            continue;
        };
        if dx == 0 {
            continue;
        }
        wrote = true;
        let origin = origin + bevy::math::IVec2::new(dx, 0);
        debug!(
            "overview: keeping row {} of display {index} panned by {dx} at {origin}",
            strip.virtual_index
        );
        if strip.active {
            let signature = strips
                .get(entity)
                .ok()
                .map(|strip| strip_signature(strip.1, windows));
            commands.queue(move |world: &mut bevy::ecs::world::World| {
                if let Some(mut position) = world.get_mut::<Position>(entity) {
                    position.0 = origin;
                }
                let Ok(mut strip) = world.get_entity_mut(entity) else {
                    return;
                };
                strip.remove::<(RepositionMarker, Scrolling)>();
                if let Some(signature) = signature {
                    strip.insert(ManualStripOffset { signature });
                }
            });
            spawn_snap_strip_guard(entity, commands);
        } else {
            commands.queue(move |world: &mut bevy::ecs::world::World| {
                if let Some(mut previous) = world.get_mut::<PreviousStripPosition>(entity) {
                    previous.origin = origin;
                }
            });
        }
    }
    wrote
}

/// Switches a display that doesn't hold focus from row `old` to row `new`
/// without touching focus: parks `old`, remembering it at `origin` (where the
/// overview showed it, not its own possibly stale one), and puts `new` where
/// it was parked from, as paneru's own switch does.
fn show_row(
    old: Entity,
    origin: bevy::math::IVec2,
    new: Entity,
    parked: bevy::math::IVec2,
    commands: &mut Commands,
) {
    commands.queue(move |world: &mut bevy::ecs::world::World| {
        if let Ok(mut strip) = world.get_entity_mut(old) {
            strip.insert(PreviousStripPosition {
                origin,
                focus: None,
            });
            strip.remove::<(RepositionMarker, Scrolling, ManualStripOffset)>();
            if let Some(mut position) = strip.get_mut::<Position>() {
                position.0 = parked;
            }
        }
        let Ok(mut strip) = world.get_entity_mut(new) else {
            return;
        };
        let restored = strip.take::<PreviousStripPosition>();
        strip.remove::<(RepositionMarker, Scrolling)>();
        if let Some(previous) = restored
            && let Some(mut position) = strip.get_mut::<Position>()
        {
            position.0 = previous.origin;
        }
        strip.insert(SelectedVirtualMarker);
    });
    spawn_snap_strip_guard(new, commands);
}

/// When the shown row holds `focus` and the pan left it not fully inside
/// the screen box, the tiled window nearest the middle to focus instead.
fn refocus(layout: &DisplayLayout<Entity>, focus: Entity) -> Option<Entity> {
    let strip = layout.strips.iter().find(|strip| strip.active)?;
    let window = strip.windows.iter().find(|w| w.key == focus)?;
    if window.floating {
        return None;
    }
    let pan = strip.pan / layout.scale.max(f64::EPSILON);
    let (left, right) = layout.work_area;
    let inside = |w: &crate::overview::WindowPlacement<Entity>| {
        let frame = w.layout.offset(pan, 0.0);
        frame.x >= left - 0.5 && frame.x + frame.w <= right + 0.5
    };
    if inside(window) {
        return None;
    }
    let row = layout.strips.iter().position(|strip| strip.active)?;
    nearest_middle(layout, row)
}

/// The tiled window of `row` whose middle, as panned, is nearest the middle
/// of the work area.
fn nearest_middle(layout: &DisplayLayout<Entity>, row: usize) -> Option<Entity> {
    let strip = layout.strips.get(row)?;
    let pan = strip.pan / layout.scale.max(f64::EPSILON);
    let (left, right) = layout.work_area;
    let middle = f64::midpoint(left, right);
    let distance = |w: &crate::overview::WindowPlacement<Entity>| {
        let frame = w.layout.offset(pan, 0.0);
        (frame.x + frame.w / 2.0 - middle).abs()
    };
    strip
        .windows
        .iter()
        .filter(|w| !w.floating)
        .min_by(|a, b| distance(a).total_cmp(&distance(b)))
        .map(|w| w.key)
}

/// Whether a scrub has gone without samples for longer than [`SCRUB_TIMEOUT`].
fn scrub_timed_out(state: &OverviewState, now: Instant) -> bool {
    state.phase == Phase::Scrubbing
        && state
            .last_sample
            .is_none_or(|last| now.saturating_duration_since(last) > SCRUB_TIMEOUT)
}

/// Springs from wherever the overview is now to `target`.
fn settle(state: &mut OverviewState, manager: &mut OverviewManager, target: f64, velocity: f64) {
    let (from, current_velocity) = state.current();
    let velocity = if state.phase == Phase::Scrubbing {
        velocity
    } else {
        current_velocity
    };
    // A faster release settles a little quicker.
    let response = SPRING_RESPONSE * (1.0 - 0.25 * (velocity.abs() / 0.5).tanh());
    let spring = Spring::new(from, target, velocity, response);
    state.generation = manager.animate(&spring);
    state.spring = Some((spring, Instant::now()));
    state.progress = from;
    state.phase = if target > 0.5 {
        Phase::Opening
    } else {
        Phase::Closing
    };
}

/// Trackpad scroll inside the open overview: mostly-horizontal motion pans
/// the strip under the pointer, mostly-vertical scrolls the stack.
fn scroll(
    state: &mut OverviewState,
    manager: &mut OverviewManager,
    point: CGPoint,
    dx: f64,
    dy: f64,
) {
    let Some(index) = state
        .displays
        .iter()
        .position(|d| d.cg_bounds.contains(point.x, point.y))
    else {
        return;
    };
    let entry = &mut state.displays[index];
    let (x, y) = (point.x - entry.cg_bounds.x, point.y - entry.cg_bounds.y);
    let moved = if dx.abs() > dy.abs() {
        entry
            .layout
            .strip_at(x, y)
            .is_some_and(|row| entry.layout.pan_by(row, dx))
    } else {
        entry.layout.scroll_by(-dy)
    };
    if moved {
        manager.apply_offsets(index, &entry.layout);
    }
}

fn select(
    state: &mut OverviewState,
    manager: &mut OverviewManager,
    point: CGPoint,
    strips: &StripQuery,
    windows: &Windows,
    commands: &mut Commands,
) {
    let hit = state
        .displays
        .iter()
        .enumerate()
        .find(|(_, d)| d.cg_bounds.contains(point.x, point.y))
        .map(|(index, entry)| {
            let (x, y) = (point.x - entry.cg_bounds.x, point.y - entry.cg_bounds.y);
            (index, hit_test(&entry.layout, x, y))
        });
    let pick = match &hit {
        Some((display, Hit::Window { key, virtual_index })) => Some(Pick {
            display: *display,
            virtual_index: *virtual_index,
            focus: Some(*key),
        }),
        Some((display, Hit::Strip { virtual_index })) => Some(Pick {
            display: *display,
            virtual_index: *virtual_index,
            focus: None,
        }),
        _ => None,
    };
    // Pans first: a row shown by the click starts from its panned origin.
    commit(state, manager, strips, windows, commands, pick);
    match hit {
        Some((_, Hit::Window { key, virtual_index })) => {
            debug!("overview: selected window {key} on row {virtual_index}");
            commands.focus_entity(key, true);
        }
        Some((display, Hit::Strip { virtual_index })) => {
            let strip = state.displays.get(display).and_then(|entry| {
                let row = entry
                    .layout
                    .strips
                    .iter()
                    .position(|s| s.virtual_index == virtual_index)?;
                entry.strips.get(row).copied()
            });
            if let Some(strip) = strip
                && let Ok(mut entity) = commands.get_entity(strip)
            {
                entity.try_insert(ActiveWorkspaceMarker);
            }
        }
        _ => {}
    }
    settle(state, manager, 0.0, 0.0);
}

/// While open, re-snapshots the layout whenever paneru changed it (a hotkey
/// scrolled a strip, switched rows, moved focus) and springs the cards to
/// their new places, so the overview never shows stale positions.
#[allow(clippy::type_complexity)]
fn overview_follow_layout(
    mut state: ResMut<OverviewState>,
    manager: Option<NonSendMut<OverviewManager>>,
    scene: Scene,
    changed_strips: Query<
        (),
        (
            With<LayoutStrip>,
            Or<(
                Changed<LayoutStrip>,
                Changed<RepositionMarker>,
                Added<ActiveWorkspaceMarker>,
                Added<PreviousStripPosition>,
            )>,
        ),
    >,
    changed_windows: Query<
        (),
        (
            Without<LayoutStrip>,
            Or<(
                Changed<LayoutPosition>,
                Changed<Bounds>,
                Added<FocusedMarker>,
            )>,
        ),
    >,
) {
    let Some(mut manager) = manager else {
        return;
    };
    if changed_strips.is_empty() && changed_windows.is_empty() {
        return;
    }
    let mut entries = gather(&scene, None);
    let unchanged = entries.len() == state.displays.len()
        && entries
            .iter()
            .zip(&state.displays)
            .all(|(new, old)| same_geometry(&new.layout, &old.layout));
    if unchanged {
        return;
    }
    // Keep the user's vertical scroll; per-strip pans give way to the real
    // strip's own scrolling, which the new layout already reflects.
    for (new, old) in entries.iter_mut().zip(&state.displays) {
        new.layout.scroll_by(old.layout.scroll);
    }
    let layouts: Vec<_> = entries.iter().map(|e| &e.layout).collect();
    let spring = Spring::new(0.0, 1.0, 0.0, SPRING_RESPONSE);
    if !manager.relayout(&layouts, &spring) {
        // Windows came or went: rebuild in place, fully open.
        let info = window_server_info();
        let scenes: Vec<_> = entries
            .iter()
            .map(|entry| DisplayScene {
                cg_bounds: entry.cg_bounds,
                layout: &entry.layout,
            })
            .collect();
        manager.build(&scenes, &info, 1.0);
    }
    state.displays = entries;
}

/// Snapshots the ECS into per-display layouts and builds the panels at
/// progress 0. Returns `false` when there is nothing to show.
fn open(state: &mut OverviewState, manager: &mut OverviewManager, scene: &Scene) -> bool {
    let info = window_server_info();
    let entries = gather(scene, Some(&info));
    if entries.is_empty() {
        return false;
    }
    let scenes: Vec<_> = entries
        .iter()
        .map(|entry| DisplayScene {
            cg_bounds: entry.cg_bounds,
            layout: &entry.layout,
        })
        .collect();
    manager.set_live_limits(
        scene.config.overview_live_previews(),
        scene.config.overview_live_margin(),
    );
    manager.build(&scenes, &info, 0.0);
    OVERVIEW_OPEN.store(true, Ordering::Relaxed);
    state.displays = entries;
    state.progress = 0.0;
    state.spring = None;
    true
}

/// Builds the overview layout of every display. With `info`, windows of the
/// active rows start from exactly where the `WindowServer` has them (opening);
/// without it they rest at their layout targets (following a change while
/// open, when real windows may be mid-animation).
#[allow(
    clippy::too_many_lines,
    reason = "one pass over displays, rows and windows"
)]
fn gather(scene: &Scene, info: Option<&WindowServerInfo>) -> Vec<DisplayEntry> {
    let metrics = Metrics::default();
    let mut entries = Vec::new();
    for (display_entity, display, dock) in &scene.displays {
        // The whole screen, menu bar included: the overview covers it, and
        // the wallpaper and menu bar windows are matched to its corner.
        let bounds = display.screen_bounds();
        let viewport = display.actual_display_bounds(dock, &scene.config);
        let size = (f64::from(bounds.width()), f64::from(bounds.height()));
        let origin = (f64::from(bounds.min.x), f64::from(bounds.min.y));
        let display_rect = Rect::new(origin.0, origin.1, size.0, size.1);
        let local = move |r: Rect| r.offset(-origin.0, -origin.1);

        // Every display joins, as in Mission Control. Only the focused
        // display's row carries `ActiveWorkspaceMarker`; the others show
        // their current space's selected row.
        let display_id = display.id();
        let workspace = scene
            .strips
            .iter()
            .find(|s| s.5 && s.6.parent() == display_entity)
            .map(|s| s.1.id())
            .or_else(|| {
                scene
                    .window_manager
                    .active_display_space(display_id)
                    .inspect_err(|err| debug!("overview: space of display {display_id}: {err}"))
                    .ok()
            });
        let Some(workspace) = workspace else {
            continue;
        };
        let mut rows: Vec<_> = scene
            .strips
            .iter()
            .filter(|s| s.6.parent() == display_entity && s.1.id() == workspace)
            .collect();
        rows.sort_by_key(|s| s.1.virtual_index);
        let flags: Vec<_> = rows.iter().map(|s| (s.1.virtual_index, s.5, s.7)).collect();
        let shown = shown_row(display_id, workspace, &flags);

        let mut inputs = Vec::new();
        let mut strip_entities = Vec::new();
        let mut origins = Vec::new();
        for (index, (entity, strip, position, reposition, previous, _, _, _)) in
            rows.into_iter().enumerate()
        {
            let active = index == shown;
            let strip_origin = if active {
                shown_origin(
                    scene,
                    display,
                    strip,
                    reposition.map_or(position.0, |t| t.0),
                )
                .unwrap_or(viewport.min)
            } else {
                previous.map_or(viewport.min, |previous| previous.origin)
            };
            let mut windows: Vec<_> = visible_windows(strip)
                .filter_map(|entity| {
                    let (_, window, layout, bounds, unmanaged, focused, _) =
                        scene.windows.get(entity).ok()?;
                    if matches!(unmanaged, Some(Unmanaged::Floating)) {
                        return None;
                    }
                    let min = layout.0 + strip_origin;
                    let layout_frame = local(Rect::new(
                        f64::from(min.x),
                        f64::from(min.y),
                        f64::from(bounds.0.x),
                        f64::from(bounds.0.y),
                    ));
                    let real_frame = info
                        .filter(|_| active)
                        .and_then(|info| info.frames.get(&window.id()))
                        .map(|frame| local(*frame));
                    Some(WindowInput {
                        key: entity,
                        window_id: window.id(),
                        layout_frame,
                        real_frame,
                        floating: false,
                        focused,
                    })
                })
                .collect();
            if active {
                windows.extend(floating_windows(scene, info, display_rect, local));
            }
            inputs.push(StripInput {
                virtual_index: strip.virtual_index,
                active,
                windows,
            });
            strip_entities.push(entity);
            origins.push(strip_origin);
        }
        let mut layout = layout_display(size, &inputs, 1.0, &metrics);
        layout.work_area = (
            f64::from(viewport.min.x) - origin.0,
            f64::from(viewport.max.x) - origin.0,
        );
        entries.push(DisplayEntry {
            cg_bounds: display_rect,
            layout,
            strips: strip_entities,
            origins,
            parked: display.bounds().max - PARKED_STRIP_SLIVER,
        });
    }
    entries
}

/// Where the shown row really starts. Its origin, unless that is the parked
/// one (seen on a display that isn't focused, with the row's windows on
/// screen): then where its windows are, so the row isn't drawn a screen off.
fn shown_origin(
    scene: &Scene,
    display: &Display,
    strip: &LayoutStrip,
    origin: bevy::math::IVec2,
) -> Option<bevy::math::IVec2> {
    if origin != display.bounds().max - PARKED_STRIP_SLIVER {
        return Some(origin);
    }
    let real = origin_from_windows(scene, strip);
    let (display_id, row) = (display.id(), strip.virtual_index);
    debug!(
        "overview: display {display_id} row {row} origin {origin} is the parked one, using {real:?}"
    );
    real
}

/// A strip's origin recovered from its first tiled window: where the window
/// is minus its slot in the strip.
fn origin_from_windows(scene: &Scene, strip: &LayoutStrip) -> Option<bevy::math::IVec2> {
    visible_windows(strip).find_map(|entity| {
        let (_, _, layout, _, unmanaged, _, position) = scene.windows.get(entity).ok()?;
        (!matches!(unmanaged, Some(Unmanaged::Floating))).then(|| position.0 - layout.0)
    })
}

/// The row a display has on screen: the one carrying the global
/// `ActiveWorkspaceMarker` (focused display), else its space's selected row.
/// `rows` are `(virtual_index, active, selected)` sorted by index.
fn shown_row(
    display_id: impl std::fmt::Display,
    workspace: impl std::fmt::Display,
    rows: &[(u32, bool, bool)],
) -> usize {
    let active = rows.iter().position(|s| s.1);
    let selected = rows.iter().position(|s| s.2);
    let shown = active.or(selected).unwrap_or(0);
    debug!(
        "overview: display {display_id} space {workspace}: rows {:?}, active {active:?}, selected {selected:?}, showing row {shown}",
        rows.iter().map(|s| s.0).collect::<Vec<_>>(),
    );
    shown
}

/// Floating windows on this display, shown on the active row at their real
/// position. The `WindowServer`'s frame is preferred: it is what the snapshot
/// is cut to, and only windows it lists as on screen (this space) are shown.
fn floating_windows<'a>(
    scene: &'a Scene,
    info: Option<&'a WindowServerInfo>,
    display: Rect,
    local: impl Fn(Rect) -> Rect + 'a,
) -> impl Iterator<Item = WindowInput<Entity>> + 'a {
    scene
        .windows
        .iter()
        .filter(|w| matches!(w.4, Some(Unmanaged::Floating)))
        .filter_map(move |(entity, window, _, _, _, focused, _)| {
            let frame = if let Some(info) = info {
                *info.frames.get(&window.id())?
            } else {
                let f = window.frame();
                Rect::new(
                    f64::from(f.min.x),
                    f64::from(f.min.y),
                    f64::from(f.width()),
                    f64::from(f.height()),
                )
            };
            let (cx, cy) = (frame.x + frame.w / 2.0, frame.y + frame.h / 2.0);
            if !display.contains(cx, cy) {
                return None;
            }
            Some(WindowInput {
                key: entity,
                window_id: window.id(),
                layout_frame: local(frame),
                real_frame: Some(local(frame)),
                floating: true,
                focused,
            })
        })
}

/// The windows a strip shows: native tab groups contribute their front tab.
/// Tabbed-display stacks still contribute every member (they share a frame,
/// so the last one drawn wins) — good enough for the spike.
fn visible_windows(strip: &LayoutStrip) -> impl Iterator<Item = Entity> + '_ {
    strip.columns().flat_map(|column| -> Vec<Entity> {
        match column {
            Column::Single(entity) | Column::Fullscren(entity) => vec![*entity],
            Column::Tabs(tabs) => tabs.first().copied().into_iter().collect(),
            Column::Stack(items) => items.iter().filter_map(StackItem::top).collect(),
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_pan_on_a_display_without_focus_is_kept_after_closing() {
        use std::cell::RefCell;
        use std::rc::Rc;

        use bevy::ecs::system::RunSystemOnce as _;

        use crate::events::Event;
        use crate::manager::{Origin, Size};
        use crate::tests::{
            EXT_DISPLAY_HEIGHT, EXT_DISPLAY_ID, EXT_DISPLAY_WIDTH, EXT_WORKSPACE_ID,
            TEST_MENUBAR_HEIGHT, TEST_PROCESS_ID, TEST_WINDOW_HEIGHT, TEST_WINDOW_WIDTH,
            TEST_WORKSPACE_ID, TestHarness,
        };

        let mut harness = TestHarness::new();
        harness.mock_state.add_display(
            EXT_DISPLAY_ID,
            bevy::math::IRect::new(0, -EXT_DISPLAY_HEIGHT, EXT_DISPLAY_WIDTH, 0),
            vec![EXT_WORKSPACE_ID],
        );
        let size = Size::new(TEST_WINDOW_WIDTH, TEST_WINDOW_HEIGHT);
        let origin = Origin::new(0, 0);
        harness.mock_state.spawn_window(
            TEST_PROCESS_ID,
            TEST_WORKSPACE_ID,
            100,
            bevy::math::IRect::from_corners(origin, origin + size),
        );
        // Six 400pt columns on a 1920pt display: the strip can scroll.
        for i in 0..6 {
            let origin = Origin::new(
                i * TEST_WINDOW_WIDTH,
                -EXT_DISPLAY_HEIGHT + TEST_MENUBAR_HEIGHT,
            );
            harness.mock_state.spawn_window(
                TEST_PROCESS_ID,
                EXT_WORKSPACE_ID,
                200 + i,
                bevy::math::IRect::from_corners(origin, origin + size),
            );
        }
        let ext_x = |world: &mut bevy::ecs::world::World| {
            let mut windows = world.query::<(&Window, &Position)>();
            let mut xs: Vec<_> = windows
                .iter(world)
                .filter(|(window, _)| window.id() >= 200)
                .map(|(window, position)| (window.id(), position.0.x))
                .collect();
            xs.sort_unstable();
            xs
        };
        let before = Rc::new(RefCell::new((Vec::new(), 0)));
        let (record, check) = (before.clone(), before);
        let commands = vec![
            Event::Command {
                command: Command::PrintState,
            };
            6
        ];
        harness
            .on_iteration(0, move |world, _| {
                let xs = ext_x(world);
                let dx = world
                    .run_system_once(|scene: Scene, windows: Windows, mut commands: Commands| {
                        let mut entries = gather(&scene, None);
                        let entry = entries
                            .iter_mut()
                            .find(|entry| entry.cg_bounds.y < 0.0)
                            .expect("external display joins");
                        let row = entry
                            .layout
                            .strips
                            .iter()
                            .position(|strip| strip.active)
                            .expect("a shown row");
                        // As far left as it goes: the last column at the
                        // right edge.
                        entry.layout.pan_by(row, -100_000.0);
                        #[allow(clippy::cast_possible_truncation)]
                        let dx = entry.layout.pan_points(row).round() as i32;
                        write_back(&entries, &scene.strips, &windows, &mut commands, None);
                        dx
                    })
                    .expect("commit runs");
                assert!(dx < 0, "the strip can pan: {dx}");
                *record.borrow_mut() = (xs, dx);
            })
            .on_iteration(5, move |world, _| {
                let (before, dx) = check.borrow().clone();
                let after = ext_x(world);
                // Columns off screen sit as slivers at the edges, so compare
                // the ones on screen both before and after.
                let on_screen = |x: i32| (0..=EXT_DISPLAY_WIDTH - TEST_WINDOW_WIDTH).contains(&x);
                let kept: Vec<_> = before
                    .iter()
                    .zip(&after)
                    .filter(|((_, x), (_, y))| on_screen(*x) && on_screen(*y))
                    .map(|(&(id, x), &(_, y))| (id, y - x))
                    .collect();
                assert!(kept.len() >= 2, "{before:?} -> {after:?}");
                assert!(
                    kept.iter().all(|&(_, moved)| moved == dx),
                    "panned by {dx} and stays there: {before:?} -> {after:?}"
                );
            })
            .run(commands);
    }

    #[test]
    fn a_pan_on_the_focused_display_is_kept_and_focus_follows_it() {
        use std::cell::RefCell;
        use std::rc::Rc;

        use bevy::ecs::system::RunSystemOnce as _;

        use crate::events::Event;
        use crate::manager::{Origin, Size};
        use crate::tests::{
            TEST_DISPLAY_WIDTH, TEST_MENUBAR_HEIGHT, TEST_PROCESS_ID, TEST_WINDOW_HEIGHT,
            TEST_WINDOW_WIDTH, TEST_WORKSPACE_ID, TestHarness,
        };

        let harness = TestHarness::new();
        let size = Size::new(TEST_WINDOW_WIDTH, TEST_WINDOW_HEIGHT);
        // Five 400pt columns on a 1024pt display.
        for i in 0..5 {
            let origin = Origin::new(i * TEST_WINDOW_WIDTH, TEST_MENUBAR_HEIGHT);
            harness.mock_state.spawn_window(
                TEST_PROCESS_ID,
                TEST_WORKSPACE_ID,
                100 + i,
                bevy::math::IRect::from_corners(origin, origin + size),
            );
        }
        let strip_x = |world: &mut bevy::ecs::world::World| {
            let mut strips = world.query_filtered::<&Position, With<ActiveWorkspaceMarker>>();
            strips.single(world).map(|position| position.0.x).ok()
        };
        let focused = |world: &mut bevy::ecs::world::World| {
            let mut windows =
                world.query_filtered::<(&Window, &Position, &Bounds), With<FocusedMarker>>();
            windows
                .single(world)
                .map(|(window, position, bounds)| (window.id(), position.0.x, bounds.0.x))
                .ok()
        };
        let before = Rc::new(RefCell::new((None, 0, None)));
        let (record, check) = (before.clone(), before);
        let commands = vec![
            Event::Command {
                command: Command::PrintState,
            };
            6
        ];
        harness
            .on_iteration(0, move |world, _| {
                let origin = strip_x(world);
                let was_focused = focused(world);
                // Pan the whole way the other way from where focus is, so
                // the focused window leaves the screen.
                let dx = world
                    .run_system_once(|scene: Scene, windows: Windows, mut commands: Commands| {
                        let mut entries = gather(&scene, None);
                        let entry = entries.first_mut().expect("the display joins");
                        let row = entry
                            .layout
                            .strips
                            .iter()
                            .position(|strip| strip.active)
                            .expect("a shown row");
                        let (lo, hi) = entry.layout.pan_range(row);
                        let pan = if lo.abs() > hi.abs() { lo } else { hi };
                        entry.layout.pan_by(row, pan);
                        #[allow(clippy::cast_possible_truncation)]
                        let dx = entry.layout.pan_points(row).round() as i32;
                        write_back(&entries, &scene.strips, &windows, &mut commands, None);
                        dx
                    })
                    .expect("commit runs");
                assert!(dx != 0, "the strip can pan");
                *record.borrow_mut() = (origin, dx, was_focused);
            })
            .on_iteration(5, move |world, _| {
                let (origin, dx, was_focused) = *check.borrow();
                let origin = origin.expect("a shown strip");
                assert_eq!(
                    strip_x(world),
                    Some(origin + dx),
                    "panned by {dx} from {origin} and stays there (focus was {was_focused:?})"
                );
                let (id, x, width) = focused(world).expect("a focused window");
                assert!(
                    x >= 0 && x + width <= TEST_DISPLAY_WIDTH,
                    "focus {id} is on screen at {x} (was {was_focused:?})"
                );
            })
            .run(commands);
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn clicking_another_display_keeps_the_row_scrolled_to_on_the_focused_one() {
        use bevy::ecs::query::Has;
        use bevy::ecs::system::RunSystemOnce as _;
        use bevy::math::IRect;

        use crate::commands::{MoveFocus, Operation};
        use crate::config::Config;
        use crate::events::Event;
        use crate::tests::{
            EXT_DISPLAY_HEIGHT, EXT_DISPLAY_ID, EXT_DISPLAY_WIDTH, EXT_WORKSPACE_ID,
            TEST_DISPLAY_WIDTH, TEST_MENUBAR_HEIGHT, TEST_WINDOW_HEIGHT, TEST_WINDOW_WIDTH,
            TEST_WORKSPACE_ID, TestHarness,
        };

        let config = Config::try_from("default_workspaces = 3\n").expect("config parses");
        let tick = || Event::Command {
            command: Command::PrintState,
        };
        let commands = vec![
            // Window 100 goes to the laptop's second row; focus stays on the
            // laptop's first.
            Event::MenuOpened { window_id: 100 },
            Event::Command {
                command: Command::Window(Operation::VirtualMoveNumber(1, MoveFocus::Stay)),
            },
            tick(),
            // The overview closes with the laptop scrolled to that row.
            tick(),
            tick(),
            tick(),
            tick(),
        ];
        let rows = |world: &mut bevy::ecs::world::World| {
            let mut strips = world.query::<(&LayoutStrip, Has<SelectedVirtualMarker>)>();
            let mut rows: Vec<_> = strips
                .iter(world)
                .filter(|(strip, _)| strip.id() == TEST_WORKSPACE_ID)
                .map(|(strip, selected)| (strip.virtual_index, strip.len(), selected))
                .collect();
            rows.sort_unstable();
            rows
        };
        let origin_of = |world: &mut bevy::ecs::world::World, id: i32| {
            let mut windows = world.query::<(&Window, &Position)>();
            windows
                .iter(world)
                .find(|(window, _)| window.id() == id)
                .map(|(_, position)| position.0)
        };
        let focus = |world: &mut bevy::ecs::world::World| {
            let mut focus = world.query_filtered::<&Window, With<FocusedMarker>>();
            focus
                .iter(world)
                .map(|window| window.id())
                .collect::<Vec<_>>()
        };
        TestHarness::new()
            .with_config(config)
            .with_display(
                EXT_DISPLAY_ID,
                IRect::new(0, -EXT_DISPLAY_HEIGHT, EXT_DISPLAY_WIDTH, 0),
                vec![EXT_WORKSPACE_ID],
            )
            .with_workspace_window(100, TEST_WORKSPACE_ID, |_| {})
            .with_workspace_window(101, TEST_WORKSPACE_ID, |_| {})
            .with_workspace_window(200, EXT_WORKSPACE_ID, |data| {
                data.frame = IRect::new(
                    0,
                    -EXT_DISPLAY_HEIGHT,
                    TEST_WINDOW_WIDTH,
                    -EXT_DISPLAY_HEIGHT + TEST_WINDOW_HEIGHT,
                );
            })
            .on_iteration(3, move |world, _| {
                let before = rows(world);
                assert_eq!(focus(world), [101], "laptop focused: {before:?}");
                assert!(
                    before.contains(&(1, 1, false)),
                    "100 sits on the hidden second row: {before:?}"
                );
                world
                    .run_system_once(|scene: Scene, windows: Windows, mut commands: Commands| {
                        let mut entries = gather(&scene, None);
                        let laptop = entries
                            .iter()
                            .position(|entry| entry.cg_bounds.y >= 0.0)
                            .expect("laptop joins");
                        let external = entries
                            .iter()
                            .position(|entry| entry.cg_bounds.y < 0.0)
                            .expect("external joins");
                        let entry = &mut entries[laptop];
                        let second = entry
                            .layout
                            .strips
                            .iter()
                            .position(|strip| strip.virtual_index == 1)
                            .expect("a second row");
                        while middle_row(&entry.layout) != Some(second) {
                            assert!(entry.layout.scroll_by(10.0), "scrolls to the second row");
                        }
                        // Then a click on window 200, as `select` does it.
                        let key = entries[external]
                            .layout
                            .strips
                            .iter()
                            .flat_map(|strip| &strip.windows)
                            .find(|w| w.window_id == 200)
                            .map(|w| w.key)
                            .expect("window 200 has a card");
                        let pick = Pick {
                            display: external,
                            virtual_index: 0,
                            focus: Some(key),
                        };
                        write_back(
                            &entries,
                            &scene.strips,
                            &windows,
                            &mut commands,
                            Some(&pick),
                        );
                        commands.focus_entity(key, true);
                    })
                    .expect("commit runs");
            })
            .on_iteration(6, move |world, _| {
                let after = rows(world);
                let selected: Vec<_> = after.iter().filter(|row| row.2).map(|row| row.0).collect();
                assert_eq!(selected, [1], "the laptop shows its second row: {after:?}");
                let shown = origin_of(world, 100).expect("window 100");
                assert!(
                    (0..TEST_DISPLAY_WIDTH).contains(&shown.x) && shown.y == TEST_MENUBAR_HEIGHT,
                    "100 is on the laptop screen: {shown}"
                );
                let hidden = origin_of(world, 101).expect("window 101");
                assert!(
                    hidden.x >= TEST_DISPLAY_WIDTH - PARKED_STRIP_SLIVER,
                    "101 stays parked: {hidden}"
                );
                assert_eq!(focus(world), [200], "the clicked window has focus");
                let mut active =
                    world.query_filtered::<&LayoutStrip, With<ActiveWorkspaceMarker>>();
                let active: Vec<_> = active.iter(world).map(LayoutStrip::id).collect();
                assert_eq!(
                    active,
                    [EXT_WORKSPACE_ID],
                    "one active row, the clicked display's"
                );
            })
            .run(commands);
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn scrolling_the_focused_display_to_another_row_switches_to_it_on_close() {
        use bevy::ecs::query::Has;
        use bevy::ecs::system::RunSystemOnce as _;
        use bevy::math::IRect;

        use crate::commands::{MoveFocus, Operation};
        use crate::config::Config;
        use crate::events::Event;
        use crate::tests::{
            EXT_DISPLAY_HEIGHT, EXT_DISPLAY_ID, EXT_DISPLAY_WIDTH, EXT_WORKSPACE_ID,
            TEST_DISPLAY_WIDTH, TEST_MENUBAR_HEIGHT, TEST_WINDOW_HEIGHT, TEST_WINDOW_WIDTH,
            TEST_WORKSPACE_ID, TestHarness,
        };

        let config = Config::try_from("default_workspaces = 3\n").expect("config parses");
        let tick = || Event::Command {
            command: Command::PrintState,
        };
        let commands = vec![
            // Window 100 goes to the laptop's second row; focus stays on the
            // laptop's first.
            Event::MenuOpened { window_id: 100 },
            Event::Command {
                command: Command::Window(Operation::VirtualMoveNumber(1, MoveFocus::Stay)),
            },
            tick(),
            // The overview closes with the laptop scrolled to that row.
            tick(),
            tick(),
            tick(),
            tick(),
        ];
        let rows = |world: &mut bevy::ecs::world::World| {
            let mut strips = world.query::<(&LayoutStrip, Has<SelectedVirtualMarker>)>();
            let mut rows: Vec<_> = strips
                .iter(world)
                .filter(|(strip, _)| strip.id() == TEST_WORKSPACE_ID)
                .map(|(strip, selected)| (strip.virtual_index, strip.len(), selected))
                .collect();
            rows.sort_unstable();
            rows
        };
        let origin_of = |world: &mut bevy::ecs::world::World, id: i32| {
            let mut windows = world.query::<(&Window, &Position)>();
            windows
                .iter(world)
                .find(|(window, _)| window.id() == id)
                .map(|(_, position)| position.0)
        };
        let focus = |world: &mut bevy::ecs::world::World| {
            let mut focus = world.query_filtered::<&Window, With<FocusedMarker>>();
            focus
                .iter(world)
                .map(|window| window.id())
                .collect::<Vec<_>>()
        };
        TestHarness::new()
            .with_config(config)
            .with_display(
                EXT_DISPLAY_ID,
                IRect::new(0, -EXT_DISPLAY_HEIGHT, EXT_DISPLAY_WIDTH, 0),
                vec![EXT_WORKSPACE_ID],
            )
            .with_workspace_window(100, TEST_WORKSPACE_ID, |_| {})
            .with_workspace_window(101, TEST_WORKSPACE_ID, |_| {})
            .with_workspace_window(200, EXT_WORKSPACE_ID, |data| {
                data.frame = IRect::new(
                    0,
                    -EXT_DISPLAY_HEIGHT,
                    TEST_WINDOW_WIDTH,
                    -EXT_DISPLAY_HEIGHT + TEST_WINDOW_HEIGHT,
                );
            })
            .on_iteration(3, move |world, _| {
                let before = rows(world);
                assert_eq!(focus(world), [101], "laptop focused: {before:?}");
                assert!(
                    before.contains(&(1, 1, false)),
                    "100 sits on the hidden second row: {before:?}"
                );
                world
                    .run_system_once(|scene: Scene, windows: Windows, mut commands: Commands| {
                        let mut entries = gather(&scene, None);
                        let entry = entries
                            .iter_mut()
                            .find(|entry| entry.cg_bounds.y >= 0.0)
                            .expect("laptop joins");
                        let second = entry
                            .layout
                            .strips
                            .iter()
                            .position(|strip| strip.virtual_index == 1)
                            .expect("a second row");
                        while middle_row(&entry.layout) != Some(second) {
                            assert!(entry.layout.scroll_by(10.0), "scrolls to the second row");
                        }
                        write_back(&entries, &scene.strips, &windows, &mut commands, None);
                    })
                    .expect("commit runs");
            })
            .on_iteration(6, move |world, _| {
                let after = rows(world);
                let selected: Vec<_> = after.iter().filter(|row| row.2).map(|row| row.0).collect();
                assert_eq!(selected, [1], "the second row is shown: {after:?}");
                let shown = origin_of(world, 100).expect("window 100");
                assert!(
                    (0..TEST_DISPLAY_WIDTH).contains(&shown.x) && shown.y == TEST_MENUBAR_HEIGHT,
                    "100 is on the laptop screen: {shown}"
                );
                let hidden = origin_of(world, 101).expect("window 101");
                assert!(
                    hidden.x >= TEST_DISPLAY_WIDTH - PARKED_STRIP_SLIVER,
                    "101 is parked: {hidden}"
                );
                assert_eq!(focus(world), [100], "focus moves into the row shown");
                let mut active =
                    world.query_filtered::<&LayoutStrip, With<ActiveWorkspaceMarker>>();
                let active: Vec<_> = active
                    .iter(world)
                    .map(|strip| strip.virtual_index)
                    .collect();
                assert_eq!(active, [1], "the second row is the active one");
            })
            .run(commands);
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn scrolling_a_display_without_focus_to_another_row_shows_it_on_close() {
        use bevy::ecs::query::Has;
        use bevy::ecs::system::RunSystemOnce as _;
        use bevy::math::IRect;

        use crate::commands::{MouseMove, MoveFocus, Operation};
        use crate::config::Config;
        use crate::events::Event;
        use crate::tests::{
            EXT_DISPLAY_HEIGHT, EXT_DISPLAY_ID, EXT_DISPLAY_WIDTH, EXT_WORKSPACE_ID,
            TEST_DISPLAY_WIDTH, TEST_MENUBAR_HEIGHT, TEST_WINDOW_HEIGHT, TEST_WINDOW_WIDTH,
            TEST_WORKSPACE_ID, TestHarness,
        };

        let config = Config::try_from("default_workspaces = 3\n").expect("config parses");
        let tick = || Event::Command {
            command: Command::PrintState,
        };
        let commands = vec![
            // Window 100 goes to the laptop's second row, then focus goes to
            // the external display.
            Event::MenuOpened { window_id: 100 },
            Event::Command {
                command: Command::Window(Operation::VirtualMoveNumber(1, MoveFocus::Stay)),
            },
            Event::Command {
                command: Command::Mouse(MouseMove::ToNextDisplay),
            },
            // The overview closes with the laptop scrolled to that row.
            tick(),
            tick(),
            tick(),
            tick(),
        ];
        let rows = |world: &mut bevy::ecs::world::World| {
            let mut strips = world.query::<(&LayoutStrip, Has<SelectedVirtualMarker>)>();
            let mut rows: Vec<_> = strips
                .iter(world)
                .filter(|(strip, _)| strip.id() == TEST_WORKSPACE_ID)
                .map(|(strip, selected)| (strip.virtual_index, strip.len(), selected))
                .collect();
            rows.sort_unstable();
            rows
        };
        let origin_of = |world: &mut bevy::ecs::world::World, id: i32| {
            let mut windows = world.query::<(&Window, &Position)>();
            windows
                .iter(world)
                .find(|(window, _)| window.id() == id)
                .map(|(_, position)| position.0)
        };
        let focus = |world: &mut bevy::ecs::world::World| {
            let mut focus = world.query_filtered::<&Window, With<FocusedMarker>>();
            focus
                .iter(world)
                .map(|window| window.id())
                .collect::<Vec<_>>()
        };
        TestHarness::new()
            .with_config(config)
            .with_display(
                EXT_DISPLAY_ID,
                IRect::new(0, -EXT_DISPLAY_HEIGHT, EXT_DISPLAY_WIDTH, 0),
                vec![EXT_WORKSPACE_ID],
            )
            .with_workspace_window(100, TEST_WORKSPACE_ID, |_| {})
            .with_workspace_window(101, TEST_WORKSPACE_ID, |_| {})
            .with_workspace_window(200, EXT_WORKSPACE_ID, |data| {
                data.frame = IRect::new(
                    0,
                    -EXT_DISPLAY_HEIGHT,
                    TEST_WINDOW_WIDTH,
                    -EXT_DISPLAY_HEIGHT + TEST_WINDOW_HEIGHT,
                );
            })
            .on_iteration(3, move |world, _| {
                let before = rows(world);
                assert_eq!(focus(world), [200], "external focused: {before:?}");
                assert!(
                    before.contains(&(1, 1, false)),
                    "100 sits on the hidden second row: {before:?}"
                );
                // As on the Dell: the shown row's own origin says parked.
                let mut displays = world.query::<&Display>();
                let parked = displays
                    .iter(world)
                    .find(|display| display.bounds().min.y >= 0)
                    .map(|display| display.bounds().max - PARKED_STRIP_SLIVER)
                    .expect("laptop");
                let mut strips = world.query::<(&LayoutStrip, &mut Position)>();
                for (strip, mut position) in strips.iter_mut(world) {
                    if strip.id() == TEST_WORKSPACE_ID && strip.virtual_index == 0 {
                        position.0 = parked;
                    }
                }
                world
                    .run_system_once(|scene: Scene, windows: Windows, mut commands: Commands| {
                        let mut entries = gather(&scene, None);
                        let entry = entries
                            .iter_mut()
                            .find(|entry| entry.cg_bounds.y >= 0.0)
                            .expect("laptop joins");
                        let second = entry
                            .layout
                            .strips
                            .iter()
                            .position(|strip| strip.virtual_index == 1)
                            .expect("a second row");
                        while middle_row(&entry.layout) != Some(second) {
                            assert!(entry.layout.scroll_by(10.0), "scrolls to the second row");
                        }
                        write_back(&entries, &scene.strips, &windows, &mut commands, None);
                    })
                    .expect("commit runs");
            })
            .on_iteration(6, move |world, _| {
                let after = rows(world);
                let selected: Vec<_> = after.iter().filter(|row| row.2).map(|row| row.0).collect();
                assert_eq!(selected, [1], "the second row is shown: {after:?}");
                let shown = origin_of(world, 100).expect("window 100");
                assert!(
                    (0..TEST_DISPLAY_WIDTH).contains(&shown.x) && shown.y == TEST_MENUBAR_HEIGHT,
                    "100 is on the laptop screen: {shown}"
                );
                let hidden = origin_of(world, 101).expect("window 101");
                assert!(
                    hidden.x >= TEST_DISPLAY_WIDTH - PARKED_STRIP_SLIVER,
                    "101 is parked: {hidden}"
                );
                assert_eq!(focus(world), [200], "focus stays on the external display");
                // The row it left is remembered where it was shown, so
                // switching back brings it on screen.
                let mut previous = world.query::<(&LayoutStrip, &PreviousStripPosition)>();
                let remembered: Vec<_> = previous
                    .iter(world)
                    .filter(|(strip, _)| {
                        strip.id() == TEST_WORKSPACE_ID && strip.virtual_index == 0
                    })
                    .map(|(_, previous)| previous.origin)
                    .collect();
                assert_eq!(
                    remembered.len(),
                    1,
                    "the first row is hidden: {remembered:?}"
                );
                assert!(
                    remembered[0].x <= 0 && remembered[0].y < TEST_WINDOW_HEIGHT,
                    "remembered on screen, not parked: {remembered:?}"
                );
            })
            .run(commands);
    }

    #[test]
    fn shown_row_with_a_parked_origin_is_placed_where_its_windows_are() {
        use bevy::ecs::system::RunSystemOnce as _;

        use crate::events::Event;
        use crate::manager::{Origin, Size};
        use crate::tests::{
            EXT_DISPLAY_HEIGHT, EXT_DISPLAY_ID, EXT_DISPLAY_WIDTH, EXT_WORKSPACE_ID,
            TEST_MENUBAR_HEIGHT, TEST_PROCESS_ID, TEST_WINDOW_HEIGHT, TEST_WINDOW_WIDTH,
            TEST_WORKSPACE_ID, TestHarness,
        };

        let mut harness = TestHarness::new();
        harness.mock_state.add_display(
            EXT_DISPLAY_ID,
            bevy::math::IRect::new(0, -EXT_DISPLAY_HEIGHT, EXT_DISPLAY_WIDTH, 0),
            vec![EXT_WORKSPACE_ID],
        );
        let size = Size::new(TEST_WINDOW_WIDTH, TEST_WINDOW_HEIGHT);
        let origin = Origin::new(0, 0);
        let ext_origin = Origin::new(0, -EXT_DISPLAY_HEIGHT + TEST_MENUBAR_HEIGHT);
        harness.mock_state.spawn_window(
            TEST_PROCESS_ID,
            TEST_WORKSPACE_ID,
            100,
            bevy::math::IRect::from_corners(origin, origin + size),
        );
        harness.mock_state.spawn_window(
            TEST_PROCESS_ID,
            EXT_WORKSPACE_ID,
            200,
            bevy::math::IRect::from_corners(ext_origin, ext_origin + size),
        );
        let commands = vec![Event::Command {
            command: Command::PrintState,
        }];
        harness
            .on_iteration(0, |world, _| {
                // What the Dell showed: its on-screen row still at the parked
                // origin, its windows on screen.
                let mut strips = world.query::<(&LayoutStrip, &ChildOf, &mut Position)>();
                let mut displays = world.query::<&Display>();
                let parks: Vec<_> = displays
                    .iter(world)
                    .filter(|display| display.id() == EXT_DISPLAY_ID)
                    .map(|display| display.bounds().max - PARKED_STRIP_SLIVER)
                    .collect();
                let [park] = parks[..] else {
                    panic!("external display: {parks:?}")
                };
                for (strip, _, mut position) in strips.iter_mut(world) {
                    if strip.id() == EXT_WORKSPACE_ID {
                        position.0 = park;
                    }
                }
                let frames = world
                    .run_system_once(|scene: Scene| {
                        gather(&scene, None)
                            .iter()
                            .filter(|entry| entry.cg_bounds.y < 0.0)
                            .flat_map(|entry| {
                                entry
                                    .layout
                                    .strips
                                    .iter()
                                    .filter(|strip| strip.active)
                                    .flat_map(|strip| strip.windows.iter().map(|w| w.layout))
                                    .collect::<Vec<_>>()
                            })
                            .collect::<Vec<_>>()
                    })
                    .expect("scene system runs");
                let [frame] = frames[..] else {
                    panic!("one window on the external row: {frames:?}")
                };
                assert_eq!(
                    (frame.x, frame.y),
                    (0.0, f64::from(TEST_MENUBAR_HEIGHT)),
                    "placed on screen, not a screen off"
                );
            })
            .run(commands);
    }

    #[test]
    fn every_display_joins_the_overview() {
        use bevy::ecs::system::RunSystemOnce as _;

        use crate::events::Event;
        use crate::manager::{Origin, Size};
        use crate::tests::{
            EXT_DISPLAY_HEIGHT, EXT_DISPLAY_ID, EXT_DISPLAY_WIDTH, EXT_WORKSPACE_ID,
            TEST_MENUBAR_HEIGHT, TEST_PROCESS_ID, TEST_WINDOW_HEIGHT, TEST_WINDOW_WIDTH,
            TEST_WORKSPACE_ID, TestHarness,
        };

        let mut harness = TestHarness::new();
        harness.mock_state.add_display(
            EXT_DISPLAY_ID,
            bevy::math::IRect::new(0, -EXT_DISPLAY_HEIGHT, EXT_DISPLAY_WIDTH, 0),
            vec![EXT_WORKSPACE_ID],
        );
        let size = Size::new(TEST_WINDOW_WIDTH, TEST_WINDOW_HEIGHT);
        let origin = Origin::new(0, 0);
        let ext_origin = Origin::new(0, -EXT_DISPLAY_HEIGHT + TEST_MENUBAR_HEIGHT);
        harness.mock_state.spawn_window(
            TEST_PROCESS_ID,
            TEST_WORKSPACE_ID,
            100,
            bevy::math::IRect::from_corners(origin, origin + size),
        );
        harness.mock_state.spawn_window(
            TEST_PROCESS_ID,
            EXT_WORKSPACE_ID,
            200,
            bevy::math::IRect::from_corners(ext_origin, ext_origin + size),
        );
        let commands = vec![Event::Command {
            command: Command::PrintState,
        }];
        harness
            .on_iteration(0, |world, _| {
                let shown = world
                    .run_system_once(|scene: Scene| {
                        gather(&scene, None)
                            .iter()
                            .map(|entry| {
                                let active: Vec<_> = entry
                                    .layout
                                    .strips
                                    .iter()
                                    .filter(|strip| strip.active)
                                    .flat_map(|strip| strip.windows.iter().map(|w| w.window_id))
                                    .collect();
                                (entry.cg_bounds.y < 0.0, active)
                            })
                            .collect::<Vec<_>>()
                    })
                    .expect("scene system runs");
                assert_eq!(shown.len(), 2, "both displays join: {shown:?}");
                for (external, windows) in shown {
                    let expected = if external { 200 } else { 100 };
                    assert_eq!(windows, [expected], "external display: {external}");
                }
            })
            .run(commands);
    }

    fn scrubbing(last_sample: Option<Instant>) -> OverviewState {
        OverviewState {
            phase: Phase::Scrubbing,
            last_sample,
            ..OverviewState::default()
        }
    }

    #[test]
    fn a_quiet_scrub_times_out() {
        let now = Instant::now();
        let fresh = scrubbing(Some(now));
        assert!(!scrub_timed_out(&fresh, now + Duration::from_millis(100)));
        assert!(scrub_timed_out(
            &fresh,
            now + SCRUB_TIMEOUT + Duration::from_millis(1)
        ));
        // A scrub that never got a sample (opened by a grab) is released too.
        assert!(scrub_timed_out(&scrubbing(None), now));
    }

    #[test]
    fn only_scrubs_time_out() {
        let now = Instant::now();
        for phase in [Phase::Closed, Phase::Opening, Phase::Open, Phase::Closing] {
            let state = OverviewState {
                phase,
                last_sample: Some(now),
                ..OverviewState::default()
            };
            assert!(!scrub_timed_out(&state, now + Duration::from_secs(5)));
        }
    }

    fn prebuilt() -> OverviewState {
        OverviewState {
            prebuilt: true,
            ..scrubbing(Some(Instant::now()))
        }
    }

    #[test]
    fn a_swipe_down_cancels_an_overview_built_on_touch_down() {
        let mut state = prebuilt();
        // A little jitter downwards is ignored, not cancelled.
        assert_eq!(downward_from_closed(&mut state, -0.01), Some(false));
        assert_eq!(downward_from_closed(&mut state, -0.015), Some(true));
    }

    #[test]
    fn once_swiped_up_a_swipe_down_scrubs_instead() {
        let mut state = prebuilt();
        assert_eq!(downward_from_closed(&mut state, -0.01), Some(false));
        assert_eq!(downward_from_closed(&mut state, 0.01), None);
        assert_eq!(downward_from_closed(&mut state, -0.5), None);
        // Nor does it apply to an overview opened any other way.
        assert_eq!(downward_from_closed(&mut scrubbing(None), -0.5), None);
    }

    #[test]
    fn scrubbing_keeps_the_loop_awake() {
        assert!(scrubbing(None).needs_frames());
        assert!(!OverviewState::default().needs_frames());
    }
}

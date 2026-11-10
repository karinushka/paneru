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
use crate::ecs::layout::{Column, LayoutStrip, StackItem};
use crate::ecs::workspace::PreviousStripPosition;
use crate::ecs::{
    ActiveWorkspaceMarker, Bounds, DockPosition, FocusedMarker, LayoutPosition, Position,
    PreUpdateSet, RepositionMarker, SpawnCommandsExt as _, Unmanaged,
};
use crate::events::Event;
use crate::manager::{Display, Window};
use crate::overview::view::{DisplayScene, OverviewManager, WindowServerInfo, window_server_info};
use crate::overview::{
    DisplayLayout, Hit, Metrics, OVERVIEW_OPEN, Rect, SPRING_RESPONSE, Spring, StripInput,
    WindowInput, hit_test, layout_display, reanchor, release_opens, rubber_band, same_geometry,
};

/// Progress per unit of normalized vertical finger travel: ~55% of the
/// trackpad's height opens the overview fully.
const GESTURE_GAIN: f64 = 1.8;

/// A scrub that hears nothing for this long is treated as released: the
/// fingers lifted but the end of the gesture never reached us.
const SCRUB_TIMEOUT: Duration = Duration::from_millis(400);

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

/// Per-frame work while the overview is up: ramp the background blur with
/// the running spring, and fill in hidden-row snapshots one at a time (each
/// capture blocks the main thread for several milliseconds, which would
/// stall gesture tracking if done in one go).
fn overview_tick(mut state: ResMut<OverviewState>, manager: Option<NonSendMut<OverviewManager>>) {
    let Some(mut manager) = manager else {
        return;
    };
    if scrub_timed_out(&state, Instant::now()) {
        debug!("overview: scrub went quiet, releasing");
        release(&mut state, &mut manager);
    }
    match state.phase {
        Phase::Opening | Phase::Closing => manager.set_blur(state.current().0),
        Phase::Open => manager.set_blur(1.0),
        _ => {}
    }
    manager.capture_pending(1);
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
}

fn overview_handler(
    mut messages: MessageReader<Event>,
    mut state: ResMut<OverviewState>,
    manager: Option<NonSendMut<OverviewManager>>,
    scene: Scene,
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
                Phase::Closing => settle(&mut state, &mut manager, 1.0, 0.0),
                Phase::Open | Phase::Opening | Phase::Scrubbing => {
                    settle(&mut state, &mut manager, 0.0, 0.0);
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
                } else if state.phase != Phase::Scrubbing {
                    // Grab a running or settled transition where it is.
                    let (p, _) = state.current();
                    state.gesture_raw = p;
                    state.gesture_velocity = 0.0;
                    state.phase = Phase::Scrubbing;
                }
                let step = delta * GESTURE_GAIN;
                state.gesture_raw += step;
                if *elapsed_secs > 0.0 {
                    let sample = step / elapsed_secs;
                    state.gesture_velocity = 0.3 * sample + 0.7 * state.gesture_velocity;
                }
                state.progress = rubber_band(state.gesture_raw);
                state.last_sample = Some(Instant::now());
                manager.set_progress(state.progress);
            }

            Event::OverviewSwipeEnd if state.phase == Phase::Scrubbing => {
                release(&mut state, &mut manager);
            }

            Event::OverviewClick { point }
                if matches!(state.phase, Phase::Open | Phase::Opening) =>
            {
                select(&mut state, &mut manager, *point, &mut commands);
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
                settle(&mut state, &mut manager, 0.0, 0.0);
            }

            Event::OverviewTransitionDone { generation } if *generation == state.generation => {
                match state.phase {
                    Phase::Closing => {
                        manager.teardown();
                        OVERVIEW_OPEN.store(false, Ordering::Relaxed);
                        *state = OverviewState {
                            generation: state.generation,
                            ..OverviewState::default()
                        };
                    }
                    Phase::Opening => state.phase = Phase::Open,
                    _ => {}
                }
            }
            _ => {}
        }
    }
}

/// Ends a scrub: open or close depending on where and how fast it was let go.
fn release(state: &mut OverviewState, manager: &mut OverviewManager) {
    let (p, v) = (state.progress, state.gesture_velocity);
    let target = if release_opens(p, v) { 1.0 } else { 0.0 };
    state.last_sample = None;
    settle(state, manager, target, v);
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
    commands: &mut Commands,
) {
    let Some(entry) = state
        .displays
        .iter_mut()
        .find(|d| d.cg_bounds.contains(point.x, point.y))
    else {
        settle(state, manager, 0.0, 0.0);
        return;
    };
    let (x, y) = (point.x - entry.cg_bounds.x, point.y - entry.cg_bounds.y);
    let target_row = match hit_test(&entry.layout, x, y) {
        Hit::Window { key, virtual_index } => {
            debug!("overview: selected window {key} on row {virtual_index}");
            commands.focus_entity(key, true);
            Some((virtual_index, Some(key)))
        }
        Hit::Strip { virtual_index } => {
            let index = entry
                .layout
                .strips
                .iter()
                .position(|s| s.virtual_index == virtual_index);
            if let Some(strip) = index.map(|i| entry.strips[i])
                && let Ok(mut entity) = commands.get_entity(strip)
            {
                entity.try_insert(ActiveWorkspaceMarker);
            }
            Some((virtual_index, None))
        }
        Hit::Background => None,
    };
    if let Some((row, focus)) = target_row {
        reanchor(&mut entry.layout, row, focus.as_ref());
        let layouts: Vec<_> = state.displays.iter().map(|d| &d.layout).collect();
        manager.retarget(&layouts);
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

        let Some(workspace) = scene
            .strips
            .iter()
            .find(|s| s.5 && s.6.parent() == display_entity)
            .map(|s| s.1.id())
        else {
            continue;
        };
        let mut rows: Vec<_> = scene
            .strips
            .iter()
            .filter(|s| s.6.parent() == display_entity && s.1.id() == workspace)
            .collect();
        rows.sort_by_key(|s| s.1.virtual_index);

        let mut inputs = Vec::new();
        let mut strip_entities = Vec::new();
        for (entity, strip, position, reposition, previous, active, _) in rows {
            let strip_origin = if active {
                reposition.map_or(position.0, |target| target.0)
            } else {
                previous.map_or(viewport.min, |previous| previous.origin)
            };
            let mut windows: Vec<_> = visible_windows(strip)
                .filter_map(|entity| {
                    let (_, window, layout, bounds, unmanaged, focused) =
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
        });
    }
    entries
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
        .filter_map(move |(entity, window, _, _, _, focused)| {
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

    #[test]
    fn scrubbing_keeps_the_loop_awake() {
        assert!(scrubbing(None).needs_frames());
        assert!(!OverviewState::default().needs_frames());
    }
}

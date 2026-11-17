//! ECS side of the overview: snapshots the strips into the pure layout,
//! drives the transition (toggle command, 4-finger scrub, clicks, scroll)
//! and hands frames to the native [`OverviewManager`].
//!
//! The real windows are only touched through the regular paths: selecting
//! a window focuses it, and focusing a window in a hidden row already
//! activates that row (see `focus::virtual_strip_activated`). While the
//! overview is open, hotkeys keep driving the real layout and the overview
//! follows it (`overview_follow_layout`).

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use bevy::app::{App, Last, PreUpdate, Update};
use bevy::ecs::entity::Entity;
use bevy::ecs::hierarchy::ChildOf;
use bevy::ecs::lifecycle::RemovedComponents;
use bevy::ecs::message::MessageReader;
use bevy::ecs::query::{Added, Changed, Has, Or, With, Without};
use bevy::ecs::resource::Resource;
use bevy::ecs::schedule::IntoScheduleConfigs as _;
use bevy::ecs::schedule::common_conditions::on_message;
use bevy::ecs::system::{Commands, Local, NonSend, NonSendMut, Query, Res, ResMut, SystemParam};
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
    MissionControlActive, Position, PreUpdateSet, RepositionMarker, Scrolling,
    SelectedVirtualMarker, SpawnCommandsExt as _, Unmanaged,
};
use crate::events::{Event, KeyModifiers, OverviewEvent};
use crate::manager::WindowManager;
use crate::manager::{Application, Display, Window};
use crate::overview::search;
use crate::overview::view::{
    CardLabel, CardMatch, DisplayScene, OverviewManager, SearchView, WindowServerInfo,
    window_server_info,
};
use crate::overview::{
    Direction, DisplayLayout, FloatingInput, Hit, Metrics, OVERVIEW_ACCEPTS_KEYS, OVERVIEW_OPEN,
    Rect, SPRING_RESPONSE, Spring, StripInput, WindowInput, exposing_shift, hit_test,
    layout_display, middle_row, neighbour, overview_move, reanchor, release_opens, rubber_band,
    same_geometry,
};
use crate::screen_capture;

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

#[derive(Resource, Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum OverviewPhase {
    #[default]
    Closed,
    Opening,
    Open,
    Closing,
    Scrubbing,
}

impl OverviewPhase {
    pub fn is_active(self) -> bool {
        self != Self::Closed
    }
}

struct DisplayEntry {
    display_id: u32,
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
    /// `Unmanaged::Floating` window IDs that `WindowServer` reported as
    /// off-Space or zero-alpha when the overview opened, shared across
    /// displays and follow-up relayouts (which avoid re-querying
    /// `WindowServer`) so parked popovers and off-space windows stay hidden.
    rejected_floating: Arc<HashSet<i32>>,
}

struct Scrub {
    progress: f64,
    raw: f64,
    velocity: f64,
    /// When the last scrub sample arrived; see [`SCRUB_TIMEOUT`].
    last_sample: Option<Instant>,
    /// Downward travel accumulated while built on touch-down before ever
    /// moving up (`None` once swiped up or opened another way).
    prebuilt_down_travel: Option<f64>,
}

impl Scrub {
    fn timed_out(&self, now: Instant) -> bool {
        self.last_sample
            .is_none_or(|last| now.saturating_duration_since(last) > SCRUB_TIMEOUT)
    }

    /// Evaluates a swipe sample on an overview built on touch-down: `None`
    /// once it has moved up (scrub as usual), otherwise whether downward
    /// travel has exceeded [`CANCEL_TRAVEL`]; `Some(false)` ignores jitter.
    fn downward_cancel(&mut self, delta: f64) -> Option<bool> {
        let down_travel = self.prebuilt_down_travel.as_mut()?;
        if delta > 0.0 {
            self.prebuilt_down_travel = None;
            return None;
        }
        *down_travel -= delta;
        Some(*down_travel > CANCEL_TRAVEL)
    }
}

enum Motion {
    Scrub(Scrub),
    Spring {
        spring: Spring,
        started_at: Instant,
        generation: u64,
    },
    Settled,
}

struct ActiveSession {
    displays: Vec<DisplayEntry>,
    motion: Motion,
    /// The close has written what the overview showed back to the ECS: no
    /// reopening into the stale layout, the next open builds afresh.
    committed: bool,
    /// The window search (`/`), while it is on.
    search: Option<Search>,
    /// The window the keyboard selection is on (ringed; Return focuses it).
    /// `None` until an arrow key, Tab or the search moves it: then the
    /// focused window carries the ring.
    selection: Option<Match>,
}

/// The window search: typed with the overview open, it dims every window
/// that doesn't match; Tab steps through the matches best first.
#[derive(Default)]
struct Search {
    query: String,
    /// Best first; ties keep the overview's order (display, row, column).
    matches: Vec<Match>,
    /// Where the selection goes on the next refresh.
    pick: SearchPick,
}

/// Where a search refresh puts the selection.
#[derive(Default, Clone, Copy, PartialEq, Eq)]
enum SearchPick {
    /// The focused window (search just started, or the query is empty):
    /// already in view, so nothing scrolls.
    #[default]
    Focused,
    /// The best match (the query changed).
    Best,
    /// Where it is, if that still matches (the layout changed).
    Keep,
}

/// A window in the overview.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Match {
    display: usize,
    key: Entity,
    window_id: i32,
    /// The window's row; `None` on the floating shelf.
    virtual_index: Option<u32>,
}

/// What a key press in the overview does; see [`overview_key`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum KeyAction {
    None,
    /// The query changed (or the search started).
    Query,
    /// Tab: the next (or previous) match by score.
    Step {
        forward: bool,
    },
    /// An arrow: the window that way on screen.
    Move(Direction),
}

impl ActiveSession {
    fn current_progress_and_velocity(&self) -> (f64, f64) {
        match &self.motion {
            Motion::Scrub(scrub) => (scrub.progress, scrub.velocity),
            Motion::Spring {
                spring, started_at, ..
            } => {
                let t = started_at.elapsed().as_secs_f64();
                (spring.value(t), spring.velocity_at(t))
            }
            Motion::Settled => (1.0, 0.0),
        }
    }
}

/// Synchronizes the lock-free event-tap flags ([`OVERVIEW_OPEN`] for trackpad
/// scroll capture and [`OVERVIEW_ACCEPTS_KEYS`] for bare `Escape` / `Return`
/// interception) with the current [`OverviewPhase`] and session motion.
fn sync_tap_flags(phase: OverviewPhase, session: Option<&ActiveSession>) {
    let open = phase.is_active();
    let accepts_keys = match phase {
        OverviewPhase::Opening | OverviewPhase::Open => true,
        OverviewPhase::Scrubbing => session.is_some_and(|active| {
            !matches!(&active.motion, Motion::Scrub(scrub) if scrub.prebuilt_down_travel.is_some())
        }),
        OverviewPhase::Closed | OverviewPhase::Closing => false,
    };
    OVERVIEW_OPEN.store(open, Ordering::Relaxed);
    OVERVIEW_ACCEPTS_KEYS.store(accepts_keys, Ordering::Relaxed);
}

/// Session state for an active overview, kept separate from [`OverviewPhase`]
/// so pointer/gesture run conditions only inspect the 1-byte phase resource
/// while `activate` / `close` / `Drop` own [`OVERVIEW_OPEN`] and
/// [`OVERVIEW_ACCEPTS_KEYS`] synchronization.
#[derive(Resource, Default)]
struct OverviewSession(Option<ActiveSession>);

impl OverviewSession {
    fn activate(
        &mut self,
        phase: &mut OverviewPhase,
        next_phase: OverviewPhase,
        displays: Vec<DisplayEntry>,
        motion: Motion,
    ) -> &mut ActiveSession {
        debug_assert!(next_phase.is_active());
        *phase = next_phase;
        let active = self.0.insert(ActiveSession {
            displays,
            motion,
            committed: false,
            search: None,
            selection: None,
        });
        sync_tap_flags(*phase, Some(active));
        active
    }

    fn close(&mut self, phase: &mut OverviewPhase) -> Option<ActiveSession> {
        *phase = OverviewPhase::Closed;
        sync_tap_flags(*phase, None);
        self.0.take()
    }
}

impl Drop for OverviewSession {
    fn drop(&mut self) {
        OVERVIEW_OPEN.store(false, Ordering::Relaxed);
        OVERVIEW_ACCEPTS_KEYS.store(false, Ordering::Relaxed);
    }
}

/// Run condition: pointer and swipe systems pause while the overview owns
/// the screen, so clicks on cards don't also reach the windows beneath.
pub fn overview_inactive(phase: Option<Res<OverviewPhase>>) -> bool {
    phase.is_none_or(|phase| !phase.is_active())
}

pub fn register_overview(app: &mut App) {
    app.add_message::<OverviewEvent>()
        .init_resource::<OverviewPhase>()
        .init_resource::<OverviewSession>()
        .add_systems(
            PreUpdate,
            overview_handler
                .in_set(PreUpdateSet::Dispatch)
                .run_if(on_message::<OverviewEvent>),
        )
        .add_systems(Update, overview_tick.run_if(overview_tick_needed))
        // After every layout system has written this frame's targets.
        .add_systems(Last, configure_overview_follow(overview_follow_layout));
}

fn configure_overview_follow<M>(
    system: impl bevy::ecs::schedule::IntoScheduleConfigs<bevy::ecs::system::ScheduleSystem, M>,
) -> bevy::ecs::schedule::ScheduleConfigs<bevy::ecs::system::ScheduleSystem> {
    system.run_if(overview_follow_needed)
}

fn overview_tick_needed(
    phase: Res<OverviewPhase>,
    manager: Option<NonSend<OverviewManager>>,
) -> bool {
    *phase == OverviewPhase::Scrubbing || manager.is_some_and(|manager| manager.has_pending_tick())
}

/// Per-frame work while the overview is up: release a scrub that went quiet,
/// and pick up the snapshots that landed from the background captures.
fn overview_tick(
    mut phase: ResMut<OverviewPhase>,
    mut session: ResMut<OverviewSession>,
    manager: Option<NonSendMut<OverviewManager>>,
    strips: StripQuery,
    windows: Windows,
    mut commands: Commands,
) {
    let Some(mut manager) = manager else {
        return;
    };
    if *phase == OverviewPhase::Scrubbing
        && let Some(active) = session.0.as_mut()
        && matches!(&active.motion, Motion::Scrub(scrub) if scrub.timed_out(Instant::now()))
    {
        debug!("overview: scrub went quiet, releasing");
        release(
            &mut phase,
            active,
            &mut manager,
            &strips,
            &windows,
            &mut commands,
        );
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
    /// A window's app (its parent entity), for the search.
    owners: Query<'w, 's, &'static ChildOf, With<Window>>,
    apps: Query<'w, 's, &'static Application>,
    config: Res<'w, Config>,
    window_manager: Res<'w, WindowManager>,
    mission_control: Option<Res<'w, MissionControlActive>>,
}

#[allow(clippy::too_many_lines, reason = "one arm per overview event")]
fn overview_handler(
    mut messages: MessageReader<OverviewEvent>,
    mut phase: ResMut<OverviewPhase>,
    mut session: ResMut<OverviewSession>,
    manager: Option<NonSendMut<OverviewManager>>,
    scene: Scene,
    windows: Windows,
    mut commands: Commands,
) {
    let Some(mut manager) = manager else {
        messages.clear();
        return;
    };
    for OverviewEvent(event) in messages.read() {
        match event {
            Event::Command {
                command: Command::Overview,
            } => match *phase {
                OverviewPhase::Closed => {
                    if let Some(entries) = open_displays(&mut manager, &scene) {
                        let active = session.activate(
                            &mut phase,
                            OverviewPhase::Opening,
                            entries,
                            Motion::Scrub(Scrub {
                                progress: 0.0,
                                raw: 0.0,
                                velocity: 0.0,
                                last_sample: None,
                                prebuilt_down_travel: None,
                            }),
                        );
                        settle(&mut phase, active, &mut manager, 1.0, 0.0);
                    }
                }
                OverviewPhase::Closing => {
                    if let Some(active) = session.0.as_mut()
                        && !active.committed
                    {
                        settle(&mut phase, active, &mut manager, 1.0, 0.0);
                    }
                }
                OverviewPhase::Open | OverviewPhase::Opening | OverviewPhase::Scrubbing => {
                    if let Some(active) = session.0.as_mut() {
                        close(
                            &mut phase,
                            active,
                            &mut manager,
                            &scene.strips,
                            &windows,
                            &mut commands,
                            None,
                        );
                    }
                }
            },

            Event::OverviewSwipe {
                delta,
                elapsed_secs,
            } => {
                if *phase == OverviewPhase::Closed {
                    // Only an upward swipe opens.
                    if *delta <= 0.0 {
                        continue;
                    }
                    let Some(entries) = open_displays(&mut manager, &scene) else {
                        continue;
                    };
                    session.activate(
                        &mut phase,
                        OverviewPhase::Scrubbing,
                        entries,
                        Motion::Scrub(Scrub {
                            progress: 0.0,
                            raw: 0.0,
                            velocity: 0.0,
                            last_sample: Some(Instant::now()),
                            prebuilt_down_travel: None,
                        }),
                    );
                } else {
                    let Some(active) = session.0.as_mut() else {
                        continue;
                    };
                    if active.committed {
                        continue;
                    }
                    if !matches!(active.motion, Motion::Scrub(_)) {
                        // Grab a running or settled transition where it is,
                        // heading for where letting go closes into.
                        preview_close(&active.displays, &mut manager, &scene.strips, &windows);
                        let (p, _) = active.current_progress_and_velocity();
                        *phase = OverviewPhase::Scrubbing;
                        active.motion = Motion::Scrub(Scrub {
                            progress: p,
                            raw: p,
                            velocity: 0.0,
                            last_sample: Some(Instant::now()),
                            prebuilt_down_travel: None,
                        });
                    }
                }
                let Some(active) = session.0.as_mut() else {
                    continue;
                };
                let Motion::Scrub(scrub) = &mut active.motion else {
                    continue;
                };
                match scrub.downward_cancel(*delta) {
                    Some(true) => {
                        debug!("overview: swipe went down from closed, cancelling");
                        // The panel may already have taken key focus.
                        if let Some((_, entity)) = windows.focused() {
                            commands.focus_entity(entity, true);
                        }
                        finish_close(&mut phase, &mut session, &mut manager);
                        continue;
                    }
                    Some(false) => {
                        scrub.last_sample = Some(Instant::now());
                        continue;
                    }
                    None => {}
                }
                let step = delta * GESTURE_GAIN;
                // Nothing below closed: swiping down from it just stays put.
                scrub.raw = (scrub.raw + step).max(0.0);
                if *elapsed_secs > 0.0 {
                    let sample = step / elapsed_secs;
                    scrub.velocity = 0.3 * sample + 0.7 * scrub.velocity;
                }
                scrub.progress = rubber_band(scrub.raw);
                scrub.last_sample = Some(Instant::now());
                manager.set_progress(scrub.progress);
            }

            Event::OverviewCaptureLanded => manager.poll_captures(),

            Event::OverviewTouchDown if *phase == OverviewPhase::Closed => {
                // Build now, at progress 0 (identical to the real screen), so
                // the first movement only has to move layers.
                if let Some(entries) = open_displays(&mut manager, &scene) {
                    session.activate(
                        &mut phase,
                        OverviewPhase::Scrubbing,
                        entries,
                        Motion::Scrub(Scrub {
                            progress: 0.0,
                            raw: 0.0,
                            velocity: 0.0,
                            last_sample: Some(Instant::now()),
                            prebuilt_down_travel: Some(0.0),
                        }),
                    );
                }
            }

            Event::OverviewSwipeEnd if *phase == OverviewPhase::Scrubbing => {
                if let Some(active) = session.0.as_mut() {
                    release(
                        &mut phase,
                        active,
                        &mut manager,
                        &scene.strips,
                        &windows,
                        &mut commands,
                    );
                }
            }

            Event::OverviewClick { point }
                if matches!(*phase, OverviewPhase::Open | OverviewPhase::Opening) =>
            {
                if let Some(active) = session.0.as_mut() {
                    select(
                        &mut phase,
                        active,
                        &mut manager,
                        *point,
                        &scene.strips,
                        &windows,
                        &mut commands,
                    );
                }
            }

            Event::OverviewScroll { point, dx, dy }
                if matches!(*phase, OverviewPhase::Open | OverviewPhase::Opening) =>
            {
                if let Some(active) = session.0.as_mut() {
                    scroll(active, &mut manager, *point, *dx, *dy);
                }
            }

            Event::OverviewDismiss
                if session
                    .0
                    .as_ref()
                    .is_some_and(|active| active.search.is_some()) =>
            {
                if let Some(active) = session.0.as_mut() {
                    active.search = None;
                }
                manager.set_search(None);
            }

            Event::OverviewAccept
                if session.0.as_ref().is_some_and(|active| {
                    active.selection.is_some() || active.search.is_some()
                }) =>
            {
                let Some(active) = session.0.as_mut() else {
                    continue;
                };
                // Searching with nothing matching: nothing to go to.
                let Some(pick) = active.selection else {
                    continue;
                };
                if active.committed
                    || !matches!(*phase, OverviewPhase::Open | OverviewPhase::Opening)
                {
                    continue;
                }
                let hit = match pick.virtual_index {
                    Some(virtual_index) => Hit::Window {
                        key: pick.key,
                        virtual_index,
                    },
                    None => Hit::Floating { key: pick.key },
                };
                select_hit(
                    &mut phase,
                    active,
                    &mut manager,
                    Some((pick.display, hit)),
                    &scene.strips,
                    &windows,
                    &mut commands,
                );
            }

            Event::Command {
                command: Command::Window(op),
            } if matches!(*phase, OverviewPhase::Open | OverviewPhase::Opening)
                && overview_move(op).is_some() =>
            {
                // The focus commands move the selection while open; the
                // usual handlers drop every window command meanwhile (see
                // `commands::WindowCommands`).
                if let Some(active) = session.0.as_mut()
                    && !active.committed
                    && let Some(direction) = overview_move(op)
                {
                    let focused = windows.focused().map(|(_, entity)| entity);
                    let from = active.selection.map(|m| m.key).or(focused);
                    move_selection(active, &mut manager, from, direction);
                }
            }

            Event::OverviewKey {
                code,
                text,
                modifiers,
            } if matches!(*phase, OverviewPhase::Open | OverviewPhase::Opening) => {
                let Some(active) = session.0.as_mut().filter(|active| !active.committed) else {
                    continue;
                };
                match overview_key(&mut active.search, *code, text, *modifiers) {
                    KeyAction::None => {}
                    KeyAction::Query => refresh_search(active, &mut manager, &scene),
                    KeyAction::Step { forward } => {
                        let next = active.search.as_ref().and_then(|search| {
                            stepped(&search.matches, active.selection.map(|m| m.key), forward)
                        });
                        if next.is_some() {
                            set_selection(active, &mut manager, next, true);
                        }
                    }
                    KeyAction::Move(direction) => {
                        let focused = windows.focused().map(|(_, entity)| entity);
                        let from = active.selection.map(|m| m.key).or(focused);
                        move_selection(active, &mut manager, from, direction);
                    }
                }
            }

            Event::OverviewDismiss
                if matches!(
                    *phase,
                    OverviewPhase::Open | OverviewPhase::Opening | OverviewPhase::Scrubbing
                ) =>
            {
                // The panel took key focus; hand it back.
                if let Some((_, entity)) = windows.focused() {
                    commands.focus_entity(entity, true);
                }
                if let Some(active) = session.0.as_mut() {
                    restore_rests(&active.displays, &mut manager);
                    settle(&mut phase, active, &mut manager, 0.0, 0.0);
                }
            }

            Event::OverviewAccept
                if matches!(
                    *phase,
                    OverviewPhase::Open | OverviewPhase::Opening | OverviewPhase::Scrubbing
                ) =>
            {
                if let Some(active) = session.0.as_mut()
                    && !active.committed
                {
                    close(
                        &mut phase,
                        active,
                        &mut manager,
                        &scene.strips,
                        &windows,
                        &mut commands,
                        None,
                    );
                }
            }

            Event::OverviewTransitionDone { generation } => {
                if let Some(active) = session.0.as_mut()
                    && matches!(active.motion, Motion::Spring { generation: g, .. } if g == *generation)
                {
                    match *phase {
                        OverviewPhase::Closing => {
                            finish_close(&mut phase, &mut session, &mut manager);
                        }
                        OverviewPhase::Opening => {
                            *phase = OverviewPhase::Open;
                            active.motion = Motion::Settled;
                        }
                        _ => {}
                    }
                }
            }

            event if phase.is_active() && event.invalidates_overview() => {
                debug!("overview: environment changed ({event:?}), dismissing");
                finish_close(&mut phase, &mut session, &mut manager);
            }

            _ => {}
        }
        sync_tap_flags(*phase, session.0.as_ref());
    }
}

/// Removes the overview and forgets it, ready for the next open.
fn finish_close(
    phase: &mut OverviewPhase,
    session: &mut OverviewSession,
    manager: &mut OverviewManager,
) {
    if let Some(active) = session.close(phase)
        && let Motion::Spring { started_at, .. } = active.motion
    {
        debug!(
            "overview: closed {:?} after letting go",
            started_at.elapsed()
        );
    }
    manager.teardown();
    // Ask for screen recording once the overlay is gone (it would cover the
    // system prompt), and only once per run.
    if !manager.previews() {
        screen_capture::request_once();
    }
}

/// Ends a scrub: open or close depending on where and how fast it was let go.
fn release(
    phase: &mut OverviewPhase,
    active: &mut ActiveSession,
    manager: &mut OverviewManager,
    strips: &StripQuery,
    windows: &Windows,
    commands: &mut Commands,
) {
    let Motion::Scrub(scrub) = &mut active.motion else {
        return;
    };
    let (p, v) = (scrub.progress, scrub.velocity);
    let prebuilt = scrub.prebuilt_down_travel.is_some();
    scrub.last_sample = None;
    if prebuilt {
        // Lifted without ever moving up: nothing was looked at, so
        // nothing to commit.
        settle(phase, active, manager, 0.0, 0.0);
    } else if release_opens(p, v) {
        settle(phase, active, manager, 1.0, v);
    } else {
        commit(active, manager, strips, windows, commands, None);
        settle(phase, active, manager, 0.0, v);
    }
}

/// Closes into what the overview shows (everything but Escape): see
/// [`commit`].
fn close(
    phase: &mut OverviewPhase,
    active: &mut ActiveSession,
    manager: &mut OverviewManager,
    strips: &StripQuery,
    windows: &Windows,
    commands: &mut Commands,
    pick: Option<Pick>,
) {
    commit(active, manager, strips, windows, commands, pick);
    settle(phase, active, manager, 0.0, 0.0);
}

/// The row a display closes into, and the window to bring into view there.
type Anchor = (u32, Option<Entity>);

/// A card or row the user clicked to close into.
struct Pick {
    display: usize,
    /// Chosen row, or `None` when a pinned floating shelf card was picked
    /// (closes into the middle/shown row while focusing `focus`).
    virtual_index: Option<u32>,
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
    active: &mut ActiveSession,
    manager: &mut OverviewManager,
    strips: &StripQuery,
    windows: &Windows,
    commands: &mut Commands,
    pick: Option<Pick>,
) {
    let (anchors, wrote) = write_back(&active.displays, strips, windows, commands, pick.as_ref());
    active.committed = wrote || pick.is_some();
    for (entry, anchor) in active.displays.iter_mut().zip(anchors) {
        if let Some((virtual_index, focus)) = anchor {
            reanchor(&mut entry.layout, virtual_index, focus.as_ref());
        }
    }
    restore_rests(&active.displays, manager);
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
    displays.iter().enumerate().fold(
        (Vec::with_capacity(displays.len()), false),
        |(mut anchors, wrote), (index, entry)| {
            let (anchor, row_wrote) =
                write_back_display(index, entry, focused, pick, strips, windows, commands);
            anchors.push(anchor);
            (anchors, wrote || row_wrote)
        },
    )
}

fn write_back_display(
    index: usize,
    entry: &DisplayEntry,
    focused: Option<Entity>,
    pick: Option<&Pick>,
    strips: &StripQuery,
    windows: &Windows,
    commands: &mut Commands,
) -> (Option<Anchor>, bool) {
    let picked = pick.filter(|pick| pick.display == index);
    let (shown, target) = close_rows(&entry.layout, picked);
    // A picked window out of view: the strip goes straight to where it is
    // exposed (where the cards land), rather than paneru scrolling it there
    // with an animation once the focus arrives.
    let expose = picked
        .and_then(|pick| Some((target?, pick.focus?)))
        .map(|(row, key)| (row, entry.layout.exposing_points(row, &key)));
    let panned = keep_pans(entry, index, strips, windows, commands, expose);
    let Some(target) = target else {
        return (None, panned);
    };
    let virtual_index = entry.layout.strips[target].virtual_index;
    let holds_focus = holds_focus(entry, shown, strips);
    let picked_focus = picked.and_then(|pick| pick.focus);
    let explicit_row = picked.and_then(|pick| pick.virtual_index).is_some();
    let (focus, switched) = if !explicit_row
        && Some(target) != shown
        && let (Some(old), Some(&new)) = (shown, entry.strips.get(target))
    {
        debug!("overview: display {index} closes into row {virtual_index}");
        let focus = if holds_focus && pick.is_none() {
            // paneru's own switch hides the old row and restores
            // the new one; focus goes to what is in the middle.
            if let Ok(mut strip) = commands.get_entity(new) {
                strip.try_insert(ActiveWorkspaceMarker);
            }
            let middle = nearest_middle(&entry.layout, target);
            if let Some(key) = middle {
                commands.focus_entity(key, true);
            }
            middle
        } else {
            if holds_focus && picked.is_some() {
                // Picked a floating shelf card on this display after
                // scrolling to another row: activate the scrolled row,
                // leaving focus for the picked floating window.
                if let Ok(mut strip) = commands.get_entity(new) {
                    strip.try_insert(ActiveWorkspaceMarker);
                }
            } else if let (Some(&old_strip), Some(&old_origin), Some(&new_base)) = (
                entry.strips.get(old),
                entry.origins.get(old),
                entry.origins.get(target),
            ) {
                #[allow(clippy::cast_possible_truncation, reason = "on-screen points")]
                let old_dx = entry.layout.pan_points(old).round() as i32;
                #[allow(clippy::cast_possible_truncation, reason = "on-screen points")]
                let new_dx = entry.layout.pan_points(target).round() as i32;
                show_row(
                    old_strip,
                    old_origin + bevy::math::IVec2::new(old_dx, 0),
                    new,
                    new_base + bevy::math::IVec2::new(new_dx, 0),
                    entry.parked,
                    commands,
                );
            }
            picked_focus
        };
        (focus, true)
    } else if picked.is_none()
        && pick.is_none()
        && let Some(focus_entity) = focused
        && let Some(key) = refocus(&entry.layout, focus_entity)
    {
        debug!("overview: focused window panned out of view, focusing {key}");
        commands.focus_entity(key, true);
        (Some(key), true)
    } else {
        (picked_focus, false)
    };
    (Some((virtual_index, focus)), panned || switched)
}

/// The row a display shows now, and the one it closes into: the picked
/// one, else the one left in the middle.
fn close_rows(
    layout: &DisplayLayout<Entity>,
    picked: Option<&Pick>,
) -> (Option<usize>, Option<usize>) {
    let shown = layout.strips.iter().position(|strip| strip.active);
    let target = picked
        .and_then(|pick| pick.virtual_index)
        .and_then(|virtual_index| {
            layout
                .strips
                .iter()
                .position(|strip| strip.virtual_index == virtual_index)
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
    displays: &[DisplayEntry],
    manager: &mut OverviewManager,
    strips: &StripQuery,
    windows: &Windows,
) {
    let focused = windows.focused().map(|(_, entity)| entity);
    let layouts: Vec<_> = displays
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
fn restore_rests(displays: &[DisplayEntry], manager: &mut OverviewManager) {
    let layouts: Vec<_> = displays.iter().map(|d| &d.layout).collect();
    manager.retarget(&layouts);
}

/// Writes each panned row of a display back: the shown row's strip moves,
/// a hidden row's remembered origin does. `expose` adds a further shift, in
/// points, to one row. Returns whether any row was written.
fn keep_pans(
    entry: &DisplayEntry,
    index: usize,
    strips: &StripQuery,
    windows: &Windows,
    commands: &mut Commands,
    expose: Option<(usize, f64)>,
) -> bool {
    entry
        .layout
        .strips
        .iter()
        .enumerate()
        .filter_map(|(row, strip)| {
            let extra = expose
                .filter(|(at, _)| *at == row)
                .map_or(0.0, |(_, dx)| dx);
            #[allow(clippy::cast_possible_truncation, reason = "on-screen points")]
            let dx = (entry.layout.pan_points(row) + extra).round() as i32;
            (dx != 0)
                .then(|| Some((strip, *entry.strips.get(row)?, *entry.origins.get(row)?, dx)))
                .flatten()
        })
        .fold(false, |_, (strip, entity, base_origin, dx)| {
            let origin = base_origin + bevy::math::IVec2::new(dx, 0);
            debug!(
                "overview: keeping row {} of display {index} panned by {dx} at {origin}",
                strip.virtual_index
            );
            if strip.active {
                let signature = strips
                    .get(entity)
                    .ok()
                    .map(|strip| strip_signature(strip.1, windows));
                if let Ok(mut strip_cmds) = commands.get_entity(entity) {
                    strip_cmds.try_insert(Position(origin));
                    strip_cmds.try_remove::<(RepositionMarker, Scrolling)>();
                    if let Some(signature) = signature {
                        strip_cmds.try_insert(ManualStripOffset { signature });
                    }
                }
                spawn_snap_strip_guard(entity, commands);
            } else {
                let focus = strips
                    .get(entity)
                    .ok()
                    .and_then(|s| s.4.and_then(|previous| previous.focus));
                if let Ok(mut strip_cmds) = commands.get_entity(entity) {
                    strip_cmds.try_insert(PreviousStripPosition { origin, focus });
                }
            }
            true
        })
}

/// Switches a display that doesn't hold focus from row `old` to row `new`
/// without touching focus: parks `old`, remembering it at `old_origin`, and
/// places `new` at `new_origin`.
fn show_row(
    old: Entity,
    old_origin: bevy::math::IVec2,
    new: Entity,
    new_origin: bevy::math::IVec2,
    parked: bevy::math::IVec2,
    commands: &mut Commands,
) {
    if let Ok(mut old_cmds) = commands.get_entity(old) {
        old_cmds.try_insert((
            Position(parked),
            PreviousStripPosition {
                origin: old_origin,
                focus: None,
            },
        ));
        old_cmds.try_remove::<(RepositionMarker, Scrolling, ManualStripOffset)>();
    }
    if let Ok(mut new_cmds) = commands.get_entity(new) {
        new_cmds.try_insert((Position(new_origin), SelectedVirtualMarker));
        new_cmds.try_remove::<(PreviousStripPosition, RepositionMarker, Scrolling)>();
    }
    spawn_snap_strip_guard(new, commands);
}

/// When the shown row holds `focus` and the pan left it not fully inside
/// the screen box, the tiled window nearest the middle to focus instead.
fn refocus(layout: &DisplayLayout<Entity>, focus: Entity) -> Option<Entity> {
    let strip = layout.strips.iter().find(|strip| strip.active)?;
    let window = strip.windows.iter().find(|w| w.key == focus)?;
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
        (w.layout.offset(pan, 0.0).centre().0 - middle).abs()
    };
    strip
        .windows
        .iter()
        .min_by(|a, b| distance(a).total_cmp(&distance(b)))
        .map(|w| w.key)
}

/// Springs from wherever the overview is now to `target`.
fn settle(
    phase: &mut OverviewPhase,
    active: &mut ActiveSession,
    manager: &mut OverviewManager,
    target: f64,
    scrub_velocity: f64,
) {
    let (from, current_velocity) = active.current_progress_and_velocity();
    let velocity = if matches!(active.motion, Motion::Scrub(_)) {
        scrub_velocity
    } else {
        current_velocity
    };
    // A faster release settles a little quicker.
    let response = SPRING_RESPONSE * (1.0 - 0.25 * (velocity.abs() / 0.5).tanh());
    let spring = Spring::new(from, target, velocity, response);
    let generation = manager.animate(&spring);
    active.motion = Motion::Spring {
        spring,
        started_at: Instant::now(),
        generation,
    };
    *phase = if target > 0.5 {
        OverviewPhase::Opening
    } else {
        OverviewPhase::Closing
    };
    sync_tap_flags(*phase, Some(active));
}

/// Trackpad scroll inside the open overview: mostly-horizontal motion pans
/// the strip under the pointer, mostly-vertical scrolls the stack.
fn scroll(
    active: &mut ActiveSession,
    manager: &mut OverviewManager,
    point: CGPoint,
    dx: f64,
    dy: f64,
) {
    let Some(index) = active
        .displays
        .iter()
        .position(|d| d.cg_bounds.contains(point.x, point.y))
    else {
        return;
    };
    let entry = &mut active.displays[index];
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
    phase: &mut OverviewPhase,
    active: &mut ActiveSession,
    manager: &mut OverviewManager,
    point: CGPoint,
    strips: &StripQuery,
    windows: &Windows,
    commands: &mut Commands,
) {
    let hit = active
        .displays
        .iter()
        .enumerate()
        .find(|(_, d)| d.cg_bounds.contains(point.x, point.y))
        .map(|(index, entry)| {
            let (x, y) = (point.x - entry.cg_bounds.x, point.y - entry.cg_bounds.y);
            (index, hit_test(&entry.layout, x, y))
        });
    select_hit(phase, active, manager, hit, strips, windows, commands);
}

/// Closes into `hit`: focuses the window, or shows the row.
fn select_hit(
    phase: &mut OverviewPhase,
    active: &mut ActiveSession,
    manager: &mut OverviewManager,
    hit: Option<(usize, Hit<Entity>)>,
    strips: &StripQuery,
    windows: &Windows,
    commands: &mut Commands,
) {
    let pick = match &hit {
        Some((display, Hit::Window { key, virtual_index })) => Some(Pick {
            display: *display,
            virtual_index: Some(*virtual_index),
            focus: Some(*key),
        }),
        Some((display, Hit::Floating { key })) => Some(Pick {
            display: *display,
            virtual_index: None,
            focus: Some(*key),
        }),
        Some((display, Hit::Strip { virtual_index })) => Some(Pick {
            display: *display,
            virtual_index: Some(*virtual_index),
            focus: None,
        }),
        _ => None,
    };
    // Pans first: a row shown by the click starts from its panned origin.
    commit(active, manager, strips, windows, commands, pick);
    match hit {
        Some((_, Hit::Window { key, virtual_index })) => {
            debug!("overview: selected window {key} on row {virtual_index}");
            commands.focus_entity(key, true);
        }
        Some((_, Hit::Floating { key })) => {
            debug!("overview: selected floating window {key}");
            commands.focus_entity(key, true);
        }
        Some((display, Hit::Strip { virtual_index })) => {
            let strip = active.displays.get(display).and_then(|entry| {
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
    settle(phase, active, manager, 0.0, 0.0);
}

type ChangedOverviewStrips<'w, 's> = Query<
    'w,
    's,
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
>;

type ChangedOverviewWindows<'w, 's> = Query<
    'w,
    's,
    (),
    (
        Without<LayoutStrip>,
        Or<(
            Changed<LayoutPosition>,
            Changed<Bounds>,
            Added<FocusedMarker>,
        )>,
    ),
>;

fn overview_follow_needed(
    phase: Res<OverviewPhase>,
    changed_strips: ChangedOverviewStrips,
    changed_windows: ChangedOverviewWindows,
    mut removed_windows: RemovedComponents<Window>,
    mut pending_while_opening: Local<bool>,
) -> bool {
    let dirty = removed_windows.read().count() > 0
        || !changed_strips.is_empty()
        || !changed_windows.is_empty();
    match *phase {
        OverviewPhase::Open => dirty || std::mem::replace(&mut *pending_while_opening, false),
        OverviewPhase::Opening | OverviewPhase::Scrubbing => {
            *pending_while_opening |= dirty;
            false
        }
        OverviewPhase::Closed | OverviewPhase::Closing => {
            *pending_while_opening = false;
            false
        }
    }
}

/// While open, re-snapshots the layout whenever paneru changed it (a hotkey
/// scrolled a strip, switched rows, moved focus, or closed a window) and
/// springs the cards to their new places, so the overview never shows stale
/// positions.
fn overview_follow_layout(
    mut session: ResMut<OverviewSession>,
    manager: Option<NonSendMut<OverviewManager>>,
    scene: Scene,
) {
    let (Some(mut manager), Some(active)) = (manager, session.0.as_mut()) else {
        return;
    };
    let mut entries = gather_following(&scene, &active.displays);
    let unchanged = entries.len() == active.displays.len()
        && entries
            .iter()
            .zip(&active.displays)
            .all(|(new, old)| same_geometry(&new.layout, &old.layout));
    if unchanged {
        return;
    }
    // Keep the user's vertical scroll; per-strip pans give way to the real
    // strip's own scrolling, which the new layout already reflects.
    for (new, old) in entries.iter_mut().zip(&active.displays) {
        new.layout.scroll_by(old.layout.scroll);
    }
    let layouts: Vec<_> = entries.iter().map(|e| &e.layout).collect();
    let spring = Spring::new(0.0, 1.0, 0.0, SPRING_RESPONSE);
    if !manager.relayout(&layouts, &spring) {
        // Windows came or went: rebuild in place, fully open.
        let info = window_server_info();
        build_scenes(&mut manager, &scene, &entries, &info, 1.0);
    }
    active.displays = entries;
    // Windows may have come, gone or moved: find the selection again (or
    // drop it), and match again.
    if let Some(selection) = active.selection {
        let found = locate(&active.displays, selection.key);
        if found != Some(selection) {
            set_selection(active, &mut manager, found, false);
        }
    }
    refresh_search(active, &mut manager, &scene);
}

/// What a key does, editing the query in place.
///
/// The arrow keys move the selection to the window that way on screen,
/// searching or not. With the search on (`/`), Tab and Shift-Tab step
/// through the matches by score, and editing follows macOS text fields:
/// Backspace deletes a character, Option- or Control-Backspace (and
/// Control-W) the word before it, Command-Backspace everything.
fn overview_key(
    search: &mut Option<Search>,
    code: u16,
    text: &str,
    keys: KeyModifiers,
) -> KeyAction {
    const TAB: u16 = 48;
    const BACKSPACE: u16 = 51;
    const W: u16 = 13;
    const LEFT: u16 = 123;
    const RIGHT: u16 = 124;
    const DOWN: u16 = 125;
    const UP: u16 = 126;
    let arrow = match code {
        LEFT => Some(Direction::Left),
        RIGHT => Some(Direction::Right),
        DOWN => Some(Direction::Down),
        UP => Some(Direction::Up),
        _ => None,
    };
    if let Some(direction) = arrow {
        return KeyAction::Move(direction);
    }
    let Some(search) = search.as_mut() else {
        if !keys.command && !keys.control && text == "/" {
            *search = Some(Search::default());
            return KeyAction::Query;
        }
        return KeyAction::None;
    };
    match code {
        TAB => {
            return KeyAction::Step {
                forward: !keys.shift,
            };
        }
        BACKSPACE if keys.command => search.query.clear(),
        BACKSPACE if keys.option || keys.control => delete_word(&mut search.query),
        W if keys.control && !keys.command => delete_word(&mut search.query),
        BACKSPACE => _ = search.query.pop(),
        _ if keys.command || keys.control => return KeyAction::None,
        _ => {
            // Function keys come as private-use characters, Escape and the
            // like as control characters: only real text goes in.
            let typed = !text.is_empty()
                && text
                    .chars()
                    .all(|c| !c.is_control() && !('\u{F700}'..='\u{F8FF}').contains(&c));
            if !typed {
                return KeyAction::None;
            }
            search.query.push_str(text);
        }
    }
    search.pick = SearchPick::Best;
    KeyAction::Query
}

/// The match after (or before) `current` in `matches`, wrapping around;
/// the first (or last) when `current` isn't one.
fn stepped(matches: &[Match], current: Option<Entity>, forward: bool) -> Option<Match> {
    let n = matches.len();
    if n == 0 {
        return None;
    }
    let at = current.and_then(|key| matches.iter().position(|m| m.key == key));
    let next = match (at, forward) {
        (Some(i), true) => (i + 1) % n,
        (Some(i), false) => (i + n - 1) % n,
        (None, true) => 0,
        (None, false) => n - 1,
    };
    matches.get(next).copied()
}

/// Where `key`'s card is in the overview (a row, or the floating shelf).
fn locate(displays: &[DisplayEntry], key: Entity) -> Option<Match> {
    displays.iter().enumerate().find_map(|(display, entry)| {
        let rows = entry
            .layout
            .strips
            .iter()
            .map(|strip| (Some(strip.virtual_index), &strip.windows));
        let shelf = std::iter::once((None, &entry.layout.floating));
        rows.chain(shelf).find_map(|(virtual_index, placements)| {
            let window = placements.iter().find(|w| w.key == key)?;
            Some(Match {
                display,
                key,
                window_id: window.window_id,
                virtual_index,
            })
        })
    })
}

/// Puts the keyboard selection on `selection` (`None`: back to the focused
/// window's ring) and, with `reveal`, scrolls it into view if it moved.
fn set_selection(
    active: &mut ActiveSession,
    manager: &mut OverviewManager,
    selection: Option<Match>,
    reveal: bool,
) {
    let moved = selection.map(|m| m.key) != active.selection.map(|m| m.key);
    active.selection = selection;
    manager.set_selection(selection.map(|m| m.window_id));
    if reveal
        && moved
        && let Some(selection) = selection
    {
        reveal_window(&mut active.displays, manager, selection);
    }
}

/// An arrow key: the selection goes to the window that way on screen from
/// `from` (the selection, else the focused window), skipping windows the
/// search doesn't match.
///
/// The arrows walk the rows: a window on the floating shelf is no place to
/// start from, and none of them lands there.
fn move_selection(
    active: &mut ActiveSession,
    manager: &mut OverviewManager,
    from: Option<Entity>,
    direction: Direction,
) {
    let matched: Option<HashSet<Entity>> = active
        .search
        .as_ref()
        .filter(|search| !search.query.is_empty())
        .map(|search| search.matches.iter().map(|m| m.key).collect());
    let allowed = |key: &Entity| matched.as_ref().is_none_or(|set| set.contains(key));
    let from = from
        .and_then(|key| locate(&active.displays, key))
        .filter(|from| from.virtual_index.is_some());
    let Some(from) = from else {
        // Nothing to start from (no focused window shows in a row): the
        // first window.
        let first = active
            .displays
            .iter()
            .enumerate()
            .find_map(|(display, entry)| {
                entry.layout.strips.iter().find_map(|strip| {
                    let window = strip.windows.iter().find(|w| allowed(&w.key))?;
                    Some(Match {
                        display,
                        key: window.key,
                        window_id: window.window_id,
                        virtual_index: Some(strip.virtual_index),
                    })
                })
            });
        set_selection(active, manager, first, true);
        return;
    };
    let layout = &active.displays[from.display].layout;
    let Some((row, key)) = neighbour(layout, &from.key, direction, allowed) else {
        // At an edge: the selection stays, but shows (it may have been the
        // focused window's ring until now).
        set_selection(active, manager, Some(from), false);
        return;
    };
    let strip = &layout.strips[row];
    let window_id = strip
        .windows
        .iter()
        .find(|w| w.key == key)
        .map_or(0, |w| w.window_id);
    let next = Match {
        display: from.display,
        key,
        window_id,
        virtual_index: Some(strip.virtual_index),
    };
    set_selection(active, manager, Some(next), true);
}

/// Deletes the word before the end of `query`, and the spaces after it:
/// a run of letters and digits, or else a run of other symbols.
fn delete_word(query: &mut String) {
    let trimmed = query.trim_end().len();
    query.truncate(trimmed);
    let alphanumeric = query.chars().next_back().is_some_and(char::is_alphanumeric);
    while let Some(c) = query.chars().next_back() {
        if c.is_whitespace() || c.is_alphanumeric() != alphanumeric {
            break;
        }
        query.pop();
    }
}

/// Name of the app that owns `window`; empty when it has none.
fn app_name(scene: &Scene, window: Entity) -> String {
    scene
        .owners
        .get(window)
        .ok()
        .and_then(|owner| scene.apps.get(owner.parent()).ok())
        .map(|app| app.name().to_owned())
        .unwrap_or_default()
}

/// Matches the query against every window the overview shows, shows the
/// result, and moves the selection as [`Search::pick`] says.
fn refresh_search(active: &mut ActiveSession, manager: &mut OverviewManager, scene: &Scene) {
    let previous = active.selection.map(|m| m.key);
    let Some(search) = active.search.as_mut() else {
        return;
    };
    let mut focused = None;
    let mut scored = Vec::new();
    for (display, entry) in active.displays.iter().enumerate() {
        let rows = entry
            .layout
            .strips
            .iter()
            .map(|strip| (Some(strip.virtual_index), &strip.windows));
        // The floating shelf belongs to no workspace.
        let shelf = std::iter::once((None, &entry.layout.floating));
        for (virtual_index, placements) in rows.chain(shelf) {
            let workspace = virtual_index
                .map(|index| {
                    format!(
                        "Workspace {}",
                        crate::menubar::virtual_workspace_label(index)
                    )
                })
                .unwrap_or_default();
            for window in placements {
                let Ok((_, win, _, _, _, is_focused, _)) = scene.windows.get(window.key) else {
                    continue;
                };
                if is_focused {
                    focused = Some(window.key);
                }
                let title = win.title().unwrap_or_default();
                let app = app_name(scene, window.key);
                let fields = [title.as_str(), app.as_str(), workspace.as_str()];
                if let Some(found) = search::find(&search.query, &fields) {
                    let at = Match {
                        display,
                        key: window.key,
                        window_id: window.window_id,
                        virtual_index,
                    };
                    let mut hits = found.hits.into_iter();
                    let label = CardMatch {
                        title_hits: hits.next().unwrap_or_default(),
                        app_hits: hits.next().unwrap_or_default(),
                        title,
                        app,
                    };
                    scored.push((found.score, at, label));
                }
            }
        }
    }
    // Stable: equal scores keep the overview's order.
    scored.sort_by_key(|(score, ..)| std::cmp::Reverse(*score));
    let mut labels = HashMap::new();
    search.matches = scored
        .into_iter()
        .map(|(_, at, label)| {
            labels.insert(at.window_id, label);
            at
        })
        .collect();
    let find = |key: Option<Entity>| {
        key.and_then(|key| search.matches.iter().find(|m| m.key == key).copied())
    };
    let pick = if search.query.is_empty() && search.pick == SearchPick::Best {
        SearchPick::Focused
    } else {
        search.pick
    };
    search.pick = SearchPick::Keep;
    let (selection, reveal) = match pick {
        // Already in view (or wherever the selection was): no scrolling.
        SearchPick::Focused => (find(previous).or(find(focused)), false),
        SearchPick::Best => (search.matches.first().copied(), true),
        SearchPick::Keep => (find(previous).or(search.matches.first().copied()), false),
    };
    manager.set_search(Some(SearchView {
        query: search.query.clone(),
        // Every card names its app and title as soon as the search starts,
        // with the matched characters highlighted once something is typed.
        labels,
        matched: search.matches.iter().map(|m| m.window_id).collect(),
    }));
    set_selection(active, manager, selection, reveal);
}

/// Scrolls the overview so `target`'s row sits in the middle of the screen,
/// and pans the row so its card is in view.
///
/// The floating shelf is pinned in view, so a window on it needs neither.
fn reveal_window(displays: &mut [DisplayEntry], manager: &mut OverviewManager, target: Match) {
    let Some(entry) = displays.get_mut(target.display) else {
        return;
    };
    let layout = &mut entry.layout;
    let Some((row, window)) = layout.strips.iter().enumerate().find_map(|(row, strip)| {
        let window = strip.windows.iter().find(|w| w.key == target.key)?;
        Some((row, window.clone()))
    }) else {
        return;
    };
    let strip = &layout.strips[row];
    // The row's screen centred between the margins, as far as the stack
    // scrolls (the same middle `scroll_range` brings the first and last row
    // to). Steps within a row don't move it.
    let (top, bottom) = layout.margins;
    let middle = top + (layout.size.1 - top - bottom) / 2.0;
    let viewport = &strip.viewport_overview;
    let dy = viewport.y + viewport.h / 2.0 - layout.scroll - middle;
    let shown = window.overview.offset(strip.pan, 0.0);
    let ribbon = &strip.ribbon;
    let dx = exposing_shift(&shown, (ribbon.x, ribbon.x + ribbon.w));
    let scrolled = layout.scroll_by(dy);
    let panned = layout.pan_by(row, dx);
    if scrolled || panned {
        manager.apply_offsets(target.display, layout);
    }
}

/// Snapshots the ECS into per-display layouts and builds the panels at
/// progress 0. Returns `None` when Mission Control is active or there is
/// nothing to show.
fn open_displays(manager: &mut OverviewManager, scene: &Scene) -> Option<Vec<DisplayEntry>> {
    if scene
        .mission_control
        .as_ref()
        .is_some_and(|active| active.0)
    {
        return None;
    }
    let info = window_server_info();
    let entries = gather(scene, Some(&info));
    if entries.is_empty() {
        return None;
    }
    manager.set_live_limits(
        scene.config.overview_live_previews(),
        scene.config.overview_live_margin(),
    );
    manager.set_ring_style(
        scene.config.overview_ring_color(),
        scene.config.overview_ring_width(),
        scene.config.overview_ring_gap(),
    );
    // Checked on every open: permission can be granted (or revoked) while
    // paneru runs. Without it, cards show the app icon and title instead.
    manager.set_previews(screen_capture::allowed());
    build_scenes(manager, scene, &entries, &info, 0.0);
    Some(entries)
}

fn build_scenes(
    manager: &mut OverviewManager,
    scene: &Scene,
    entries: &[DisplayEntry],
    info: &WindowServerInfo,
    progress: f64,
) {
    let scenes: Vec<_> = entries
        .iter()
        .map(|entry| DisplayScene {
            display_id: entry.display_id,
            cg_bounds: entry.cg_bounds,
            layout: &entry.layout,
        })
        .collect();
    let labels = card_labels(scene, entries, manager.previews());
    manager.build(&scenes, info, &labels, progress);
}

/// Builds the overview layout of every display. With `info`, windows of the
/// active rows start from exactly where the `WindowServer` has them (opening);
/// without it they rest at their layout targets (following a change while
/// open, when real windows may be mid-animation).
fn gather(scene: &Scene, info: Option<&WindowServerInfo>) -> Vec<DisplayEntry> {
    gather_inner(scene, info, None)
}

fn gather_following(scene: &Scene, previous: &[DisplayEntry]) -> Vec<DisplayEntry> {
    gather_inner(scene, None, Some(previous))
}

fn gather_inner(
    scene: &Scene,
    info: Option<&WindowServerInfo>,
    previous: Option<&[DisplayEntry]>,
) -> Vec<DisplayEntry> {
    let metrics = Metrics::default();
    let display_rects: Vec<(u32, Rect)> = scene
        .displays
        .iter()
        .map(|(_, display, _)| {
            let b = display.frame();
            (
                display.id(),
                Rect::new(
                    f64::from(b.min.x),
                    f64::from(b.min.y),
                    f64::from(b.width()),
                    f64::from(b.height()),
                ),
            )
        })
        .collect();
    let (mut floating_map, rejected_floating) =
        floating_by_display(scene, info, previous, &display_rects);

    scene
        .displays
        .iter()
        .filter_map(|(display_entity, display, dock)| {
            // The whole screen, menu bar included: the overview covers it, and
            // the wallpaper and menu bar windows are matched to its corner.
            let bounds = display.frame();
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
                })?;

            let mut rows: Vec<_> = scene
                .strips
                .iter()
                .filter(|s| s.6.parent() == display_entity && s.1.id() == workspace)
                .collect();
            rows.sort_by_key(|s| s.1.virtual_index);
            let flags: Vec<_> = rows.iter().map(|s| (s.1.virtual_index, s.5, s.7)).collect();
            let shown = shown_row(display_id, workspace, &flags);

            let (inputs, (strip_entities, origins)): (Vec<_>, (Vec<_>, Vec<_>)) = rows
                .into_iter()
                .enumerate()
                .map(
                    |(index, (entity, strip, position, reposition, previous, _, _, _))| {
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
                        let windows =
                            gather_strip_windows(scene, strip, strip_origin, active, info, local);
                        (
                            StripInput {
                                virtual_index: strip.virtual_index,
                                active,
                                windows,
                            },
                            (entity, strip_origin),
                        )
                    },
                )
                .unzip();

            let floating = floating_map.remove(&display_id).unwrap_or_default();
            let mut layout = layout_display(size, &inputs, &floating, 1.0, &metrics);
            layout.work_area = (
                f64::from(viewport.min.x) - origin.0,
                f64::from(viewport.max.x) - origin.0,
            );
            Some(DisplayEntry {
                display_id,
                cg_bounds: display_rect,
                layout,
                strips: strip_entities,
                origins,
                parked: display.bounds().max - PARKED_STRIP_SLIVER,
                rejected_floating: Arc::clone(&rejected_floating),
            })
        })
        .collect()
}

fn gather_strip_windows(
    scene: &Scene,
    strip: &LayoutStrip,
    strip_origin: bevy::math::IVec2,
    active: bool,
    info: Option<&WindowServerInfo>,
    local: impl Fn(Rect) -> Rect,
) -> Vec<WindowInput<Entity>> {
    visible_windows(strip)
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
                focused,
            })
        })
        .collect()
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

/// Title and owning app of each window placed in `entries`, for the cards
/// drawn without previews (no screen recording permission). Empty when
/// previews are on.
fn card_labels(scene: &Scene, entries: &[DisplayEntry], previews: bool) -> HashMap<i32, CardLabel> {
    if previews {
        return HashMap::new();
    }
    entries
        .iter()
        .flat_map(|entry| {
            entry
                .layout
                .strips
                .iter()
                .flat_map(|strip| &strip.windows)
                .chain(&entry.layout.floating)
        })
        .filter_map(|placement| {
            let (_, window, ..) = scene.windows.get(placement.key).ok()?;
            let label = CardLabel {
                title: window.title().unwrap_or_default(),
                pid: window.pid().ok(),
            };
            Some((placement.window_id, label))
        })
        .collect()
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

type FloatingShelves = HashMap<u32, Vec<FloatingInput<Entity>>>;

/// Partitions visible `Unmanaged::Floating` windows across displays in a single
/// pass, sorting each display's shelf left-to-right by screen centre so visual
/// shelf order matches `focus west` / `focus east`.
///
/// On open (`info` is `Some`), `WindowServer`'s frame is preferred and only
/// windows listed as on screen (this space) and actually drawing (not zero
/// alpha) are shown; rejected IDs are shared via [`Arc`] so follow-up relayouts
/// (`info` is `None`) keep parked popovers and off-space windows excluded
/// without cloning the set.
fn floating_by_display(
    scene: &Scene,
    info: Option<&WindowServerInfo>,
    previous: Option<&[DisplayEntry]>,
    all_displays: &[(u32, Rect)],
) -> (FloatingShelves, Arc<HashSet<i32>>) {
    let prev_rejected = previous
        .and_then(|list| list.first())
        .map(|entry| Arc::clone(&entry.rejected_floating));
    let mut fresh_rejected = HashSet::new();
    let mut by_display: FloatingShelves = HashMap::new();

    for (entity, window, _, _, _, focused, _) in scene
        .windows
        .iter()
        .filter(|w| matches!(w.4, Some(Unmanaged::Floating)))
    {
        let id = window.id();
        let Some(frame) = floating_global_frame(
            id,
            window,
            info,
            previous,
            prev_rejected.as_deref(),
            &mut fresh_rejected,
        ) else {
            continue;
        };
        if let Some((display_id, display_rect, _)) = all_displays
            .iter()
            .map(|&(disp_id, rect)| (disp_id, rect, rect.intersection_area(&frame)))
            .filter(|(_, _, area)| *area > 0.0)
            .max_by(|(_, _, a), (_, _, b)| a.total_cmp(b))
        {
            by_display
                .entry(display_id)
                .or_default()
                .push(FloatingInput {
                    key: entity,
                    window_id: id,
                    frame: frame.offset(-display_rect.x, -display_rect.y),
                    focused,
                });
        }
    }

    for shelf in by_display.values_mut() {
        shelf.sort_by(|a, b| {
            let (ax, ay) = a.frame.centre();
            let (bx, by) = b.frame.centre();
            ax.total_cmp(&bx)
                .then_with(|| ay.total_cmp(&by))
                .then_with(|| a.window_id.cmp(&b.window_id))
        });
    }

    let rejected = if info.is_some() {
        Arc::new(fresh_rejected)
    } else {
        prev_rejected.unwrap_or_default()
    };
    (by_display, rejected)
}

fn floating_global_frame(
    id: i32,
    window: &Window,
    info: Option<&WindowServerInfo>,
    previous: Option<&[DisplayEntry]>,
    prev_rejected: Option<&HashSet<i32>>,
    fresh_rejected: &mut HashSet<i32>,
) -> Option<Rect> {
    if let Some(info) = info {
        let frame = (!info.invisible.contains(&id))
            .then(|| info.frames.get(&id).copied())
            .flatten();
        if frame.is_none() {
            fresh_rejected.insert(id);
        }
        return frame;
    }
    if prev_rejected.is_some_and(|set| set.contains(&id)) {
        return None;
    }
    previous
        .into_iter()
        .flatten()
        .find_map(|entry| {
            entry
                .layout
                .floating
                .iter()
                .find(|w| w.window_id == id)
                .map(|w| w.rest.offset(entry.cg_bounds.x, entry.cg_bounds.y))
        })
        .or_else(|| {
            let f = window.frame();
            Some(Rect::new(
                f64::from(f.min.x),
                f64::from(f.min.y),
                f64::from(f.width()),
                f64::from(f.height()),
            ))
        })
}

enum ColumnWindows<'a> {
    None,
    One(Entity),
    Stack(std::slice::Iter<'a, StackItem>),
}

impl Iterator for ColumnWindows<'_> {
    type Item = Entity;

    fn next(&mut self) -> Option<Self::Item> {
        match self {
            Self::None => None,
            Self::One(entity) => {
                let out = *entity;
                *self = Self::None;
                Some(out)
            }
            Self::Stack(items) => items.find_map(StackItem::top),
        }
    }
}

/// The windows a strip shows on screen: native tab groups contribute their
/// front tab, tabbed-display stacks contribute only their active tab (parked
/// tabs sit off-screen at the right-edge sliver), and vertical stacks
/// contribute each item's top tab.
fn visible_windows(strip: &LayoutStrip) -> impl Iterator<Item = Entity> + '_ {
    strip.columns().flat_map(|column| match column {
        Column::Single(entity) | Column::Fullscren(entity) => ColumnWindows::One(*entity),
        Column::Tabs(tabs) => tabs
            .first()
            .copied()
            .map_or(ColumnWindows::None, ColumnWindows::One),
        Column::Stack(items) => {
            if let Some(active) = items
                .first()
                .and_then(StackItem::top)
                .and_then(|first| strip.active_tab(first))
            {
                ColumnWindows::One(active)
            } else {
                ColumnWindows::Stack(items.iter())
            }
        }
    })
}

#[cfg(test)]
mod tests;

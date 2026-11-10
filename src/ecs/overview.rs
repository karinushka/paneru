//! ECS side of the overview: snapshots the strips into the pure layout,
//! drives the transition (toggle command, 4-finger scrub, clicks) and hands
//! frames to the native [`OverviewManager`].
//!
//! The real windows are only touched when a selection is made: focusing a
//! window in a hidden row already activates that row (see
//! `focus::virtual_strip_activated`), so the regular layout path re-arranges
//! them behind the overlay while it closes.

use std::time::Instant;

use bevy::app::{App, PreUpdate};
use bevy::ecs::entity::Entity;
use bevy::ecs::hierarchy::ChildOf;
use bevy::ecs::message::MessageReader;
use bevy::ecs::query::{Has, With};
use bevy::ecs::resource::Resource;
use bevy::ecs::schedule::IntoScheduleConfigs as _;
use bevy::ecs::schedule::common_conditions::on_message;
use bevy::ecs::system::{Commands, NonSendMut, Query, Res, ResMut};
use objc2_core_foundation::CGPoint;
use tracing::debug;

use crate::commands::Command;
use crate::config::Config;
use crate::ecs::layout::{Column, LayoutStrip, StackItem};
use crate::ecs::workspace::PreviousStripPosition;
use crate::ecs::{
    ActiveWorkspaceMarker, Bounds, DockPosition, FocusedMarker, LayoutPosition, Position,
    PreUpdateSet, RepositionMarker, SpawnCommandsExt as _,
};
use crate::events::Event;
use crate::manager::{Display, Window};
use crate::overview::view::{DisplayScene, OverviewManager};
use crate::overview::{
    DisplayLayout, Hit, Metrics, Rect, SPRING_RESPONSE, Spring, StripInput, WindowInput, hit_test,
    layout_display, reanchor, release_opens, rubber_band,
};

/// Progress per unit of normalized vertical finger travel: ~55% of the
/// trackpad's height opens the overview fully.
const GESTURE_GAIN: f64 = 1.8;

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
}

pub fn register_overview(app: &mut App) {
    app.init_resource::<OverviewState>().add_systems(
        PreUpdate,
        overview_handler
            .in_set(PreUpdateSet::Dispatch)
            .run_if(on_message::<Event>),
    );
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
    (&'static Window, &'static LayoutPosition, &'static Bounds),
    bevy::ecs::query::Without<LayoutStrip>,
>;

#[allow(clippy::too_many_arguments)]
fn overview_handler(
    mut messages: MessageReader<Event>,
    mut state: ResMut<OverviewState>,
    manager: Option<NonSendMut<OverviewManager>>,
    displays: Query<(Entity, &Display, Option<&DockPosition>)>,
    strips: StripQuery,
    windows: WindowQuery,
    focused: Query<Entity, With<FocusedMarker>>,
    config: Res<Config>,
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
                    if open(
                        &mut state,
                        &mut manager,
                        &displays,
                        &strips,
                        &windows,
                        &config,
                    ) {
                        settle(&mut state, &mut manager, 1.0, 0.0);
                        manager.capture_pending(usize::MAX);
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
                    if *delta <= 0.0
                        || !open(
                            &mut state,
                            &mut manager,
                            &displays,
                            &strips,
                            &windows,
                            &config,
                        )
                    {
                        continue;
                    }
                    state.gesture_raw = 0.0;
                    state.gesture_velocity = 0.0;
                    state.phase = Phase::Scrubbing;
                    manager.capture_pending(usize::MAX);
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
                manager.set_progress(state.progress);
            }

            Event::OverviewSwipeEnd if state.phase == Phase::Scrubbing => {
                let (p, v) = (state.progress, state.gesture_velocity);
                let target = if release_opens(p, v) { 1.0 } else { 0.0 };
                settle(&mut state, &mut manager, target, v);
            }

            Event::OverviewClick { point }
                if matches!(state.phase, Phase::Open | Phase::Opening) =>
            {
                select(&mut state, &mut manager, *point, &mut commands);
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
            Some(virtual_index)
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
            Some(virtual_index)
        }
        Hit::Background => None,
    };
    if let Some(row) = target_row {
        reanchor(&mut entry.layout, row);
        let layouts: Vec<_> = state.displays.iter().map(|d| &d.layout).collect();
        manager.retarget(&layouts);
    }
    settle(state, manager, 0.0, 0.0);
}

/// Snapshots the ECS into per-display layouts and builds the panels at
/// progress 0. Returns `false` when there is nothing to show.
fn open(
    state: &mut OverviewState,
    manager: &mut OverviewManager,
    displays: &Query<(Entity, &Display, Option<&DockPosition>)>,
    strips: &StripQuery,
    windows: &WindowQuery,
    config: &Config,
) -> bool {
    let metrics = Metrics::default();
    let mut entries = Vec::new();
    for (display_entity, display, dock) in displays {
        let bounds = display.bounds();
        let viewport = display.actual_display_bounds(dock, config);
        let size = (f64::from(bounds.width()), f64::from(bounds.height()));
        let origin = (f64::from(bounds.min.x), f64::from(bounds.min.y));

        let Some(workspace) = strips
            .iter()
            .find(|s| s.5 && s.6.parent() == display_entity)
            .map(|s| s.1.id())
        else {
            continue;
        };
        let mut rows: Vec<_> = strips
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
            let window_inputs = visible_windows(strip)
                .filter_map(|entity| {
                    let (window, layout, bounds) = windows.get(entity).ok()?;
                    let min = layout.0 + strip_origin - bounds_min(origin);
                    let layout_frame = Rect::new(
                        f64::from(min.x),
                        f64::from(min.y),
                        f64::from(bounds.0.x),
                        f64::from(bounds.0.y),
                    );
                    let real = window.frame();
                    let real_frame = Rect::new(
                        f64::from(real.min.x) - origin.0,
                        f64::from(real.min.y) - origin.1,
                        f64::from(real.width()),
                        f64::from(real.height()),
                    );
                    Some(WindowInput {
                        key: entity,
                        window_id: window.id(),
                        layout_frame,
                        real_frame: active.then_some(real_frame),
                    })
                })
                .collect();
            inputs.push(StripInput {
                virtual_index: strip.virtual_index,
                active,
                windows: window_inputs,
            });
            strip_entities.push(entity);
        }
        entries.push(DisplayEntry {
            cg_bounds: Rect::new(origin.0, origin.1, size.0, size.1),
            layout: layout_display(size, &inputs, 1.0, &metrics),
            strips: strip_entities,
        });
    }
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
    manager.build(&scenes);
    state.displays = entries;
    state.progress = 0.0;
    state.spring = None;
    true
}

fn bounds_min(origin: (f64, f64)) -> crate::manager::Origin {
    #[allow(clippy::cast_possible_truncation)]
    crate::manager::Origin::new(origin.0 as i32, origin.1 as i32)
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

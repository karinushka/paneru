//! ECS integration for native macOS Desktops (`SkyLight` census resource,
//! bounded `WMBridge` confirmation polling, and sticky-window protection).

use std::collections::{HashSet, VecDeque};
use std::time::Duration;

use bevy::app::{App, Plugin, PostUpdate, PreUpdate, Update};
use bevy::ecs::component::Component;
use bevy::ecs::entity::Entity;
use bevy::ecs::message::MessageReader;
use bevy::ecs::query::{Added, Has, With, Without};
use bevy::ecs::resource::Resource;
use bevy::ecs::schedule::IntoScheduleConfigs as _;
use bevy::ecs::schedule::SystemCondition as _;
use bevy::ecs::schedule::common_conditions::{not, on_message, resource_changed, resource_exists};
use bevy::ecs::system::{
    Commands, NonSend, NonSendMut, Populated, Query, Res, ResMut, SystemParam,
};
use bevy::time::{Time, Timer, TimerMode, Virtual};
use objc2_core_graphics::CGDirectDisplayID;
use tracing::{debug, warn};

use crate::ecs::layout::LayoutStrip;
use crate::ecs::{
    ActiveWorkspaceMarker, FocusedMarker, Initializing, PostUpdateSet, PreUpdateSet,
    SelectedVirtualMarker, SendMessageTrigger, SpawnCommandsExt as _, Unmanaged, UpdateSet,
};
use crate::events::Event;
use crate::manager::spaces::{DesktopBridge, DisplaySpaces, SpaceCensus, SpaceId};
use crate::manager::{Display, Window};
use crate::platform::{WinID, WorkspaceId};

pub(crate) mod lifecycle;
pub(crate) mod ops;
pub(crate) mod pin;

#[cfg(test)]
pub use pin::PinnedDesktopWindow;
pub use pin::SentToInactiveDesktop;

/// Maximum wall/virtual time a dispatched `WMBridge` mutation waits for OS
/// census/membership confirmation before expiring (§2.4).
pub const DESKTOP_CONFIRM_BUDGET: Duration = Duration::from_secs(2);

/// Fallback census poll cadence while awaiting an asynchronous `WMBridge` op.
pub const DESKTOP_POLL_INTERVAL: Duration = Duration::from_millis(50);

/// Cached `SkyLight` Space census in ECS, updated reactively on display/space
/// events and confirmed `WMBridge` operations so readers avoid FFI calls.
#[derive(Resource, Clone, Debug, Default, PartialEq, Eq)]
pub struct DesktopCensus(pub SpaceCensus);

#[allow(
    dead_code,
    reason = "helpers consumed across desktop commands, queries, and menubar in subsequent commits"
)]
impl DesktopCensus {
    #[must_use]
    pub fn display_for_space(&self, space_id: WorkspaceId) -> Option<CGDirectDisplayID> {
        SpaceId::new(space_id)
            .and_then(|id| self.0.display_for_space(id))
            .map(|d| d.display_id)
    }

    #[must_use]
    pub fn active_space(&self, display_id: CGDirectDisplayID) -> Option<WorkspaceId> {
        self.0.display(display_id).map(|d| d.current_space.get())
    }

    #[must_use]
    pub fn desktop_number(&self, space_id: WorkspaceId) -> Option<u32> {
        SpaceId::new(space_id)
            .and_then(|id| self.0.find_space(id))
            .and_then(|record| record.desktop_number)
    }

    #[must_use]
    pub fn desktop_count(&self, display_id: CGDirectDisplayID) -> usize {
        self.0
            .display(display_id)
            .map_or(0, DisplaySpaces::desktop_count)
    }

    #[must_use]
    pub fn is_fullscreen(&self, space_id: WorkspaceId) -> bool {
        SpaceId::new(space_id)
            .and_then(|id| self.0.find_space(id))
            .is_some_and(|record| record.kind.is_fullscreen())
    }
}

/// Marks a window carrying `SkyLight`'s sticky bit (`0x800`) or multi-Desktop
/// pinning so `detect_moved_windows` never rips it between `LayoutStrip`s when
/// switching native Spaces.
#[derive(Component, Clone, Copy, Debug, Default)]
pub struct StickyWindowMarker;

/// Expected OS state probed by [`confirm_desktop_ops`].
#[derive(Clone, Debug)]
pub enum ConfirmCondition {
    SpaceActive(SpaceId),
    SpacePresent(SpaceId),
    SpaceAbsent {
        space_id: SpaceId,
        migrated: Vec<WinID>,
    },
    WindowMemberships {
        windows: Vec<WinID>,
        expected: HashSet<SpaceId>,
    },
    PerWindowMemberships(Vec<(WinID, HashSet<SpaceId>)>),
    AllSpacesSubset(Vec<(WinID, CGDirectDisplayID)>),
    DisplayOrder {
        display_id: CGDirectDisplayID,
        expected: Vec<SpaceId>,
        active_space: SpaceId,
    },
}

/// Follow-up work executed once a [`ConfirmCondition`] is observed.
#[derive(Clone, Debug)]
pub enum PostConfirmAction {
    /// Activate the target Space's selected virtual strip and restore AX focus.
    ActivateAndFocusSpace {
        space_id: SpaceId,
        preferred_window: Option<Entity>,
    },
    /// Ensure a `LayoutStrip` exists for a newly created Desktop, and optionally switch to it.
    EnsureStripAndMaybeFocus { space_id: SpaceId, focus: bool },
    /// Second leg of closing the currently active Desktop: focus neighbor and destroy vacated Space.
    DestroyVacatedSpace {
        space_id: SpaceId,
        neighbor_id: SpaceId,
        preferred_window: Option<Entity>,
        migrated: Vec<WinID>,
    },
    /// Ensure ECS strips and `FocusHistory` for a destroyed Space are cleaned up.
    DespawnDestroyedSpace(SpaceId),
    /// Clear `StickyWindowMarker` on windows whose `0x800` tag cleared after `assign none`.
    ReconcileStickyMarkers(Vec<WinID>),
    /// Second leg of a non-adjacent Desktop swap (§5.10).
    SecondSwapStep {
        space_id: SpaceId,
        skylight_display: String,
        index: u32,
        display_id: CGDirectDisplayID,
        expected: Vec<SpaceId>,
        active_space: SpaceId,
    },
}

/// A single bounded in-flight `WMBridge` confirmation item.
pub struct PendingDesktopOp {
    pub description: String,
    pub condition: ConfirmCondition,
    pub remaining: Timer,
    pub poll: Timer,
    pub fresh: bool,
    pub on_confirmed: Option<PostConfirmAction>,
}

impl PendingDesktopOp {
    #[must_use]
    pub fn new(
        description: impl Into<String>,
        condition: ConfirmCondition,
        on_confirmed: Option<PostConfirmAction>,
    ) -> Self {
        Self {
            description: description.into(),
            condition,
            remaining: Timer::new(DESKTOP_CONFIRM_BUDGET, TimerMode::Once),
            poll: Timer::new(DESKTOP_POLL_INTERVAL, TimerMode::Repeating),
            fresh: true,
            on_confirmed,
        }
    }
}

/// Bounded queue of in-flight `WMBridge` confirmation probes.
#[derive(Resource, Default)]
pub struct PendingDesktopOps {
    queue: VecDeque<PendingDesktopOp>,
}

impl PendingDesktopOps {
    pub fn push(&mut self, op: PendingDesktopOp) {
        self.queue.push_back(op);
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.queue.is_empty()
    }
}

pub struct DesktopEventsPlugin;

impl Plugin for DesktopEventsPlugin {
    fn build(&self, app: &mut App) {
        let has_added_displays = |added: Query<(), Added<Display>>| !added.is_empty();
        let has_pending_ops =
            |pending: Option<Res<PendingDesktopOps>>| pending.is_some_and(|p| !p.is_empty());

        app.init_resource::<DesktopCensus>()
            .init_resource::<PendingDesktopOps>()
            .add_systems(
                PreUpdate,
                (
                    sync_desktop_census.run_if(has_added_displays.or_eager(on_message::<Event>)),
                    ops::desktop_command_handler
                        .after(sync_desktop_census)
                        .run_if(on_message::<Event>),
                )
                    .in_set(PreUpdateSet::Dispatch),
            )
            .add_systems(
                Update,
                (
                    confirm_desktop_ops.run_if(has_pending_ops),
                    sync_sticky_windows,
                    pin::join_pinned_windows_to_new_desktops
                        .run_if(resource_changed::<DesktopCensus>),
                )
                    .in_set(UpdateSet::Events)
                    .before(crate::ecs::workspace::workspace_change_handler),
            )
            .add_systems(
                Update,
                pin::apply_window_desktop_rules
                    .in_set(UpdateSet::Spawn)
                    .after(super::triggers::apply_window_defaults)
                    .before(super::triggers::apply_window_positions)
                    .run_if(not(resource_exists::<Initializing>)),
            )
            .add_systems(
                PostUpdate,
                pin::cleanup_sent_to_inactive_markers.in_set(PostUpdateSet::Bookkeeping),
            );
    }
}

fn is_census_event(event: &Event) -> bool {
    matches!(
        event,
        Event::ProcessesLoaded
            | Event::SpaceChanged
            | Event::SpaceCurrentChanged { .. }
            | Event::SpaceCreated { .. }
            | Event::SpaceDestroyed { .. }
            | Event::DisplayChanged
            | Event::DisplayAdded { .. }
            | Event::DisplayRemoved { .. }
            | Event::DisplayConfigured { .. }
    )
}

fn is_confirmation_wake_event(event: &Event) -> bool {
    is_census_event(event) || matches!(event, Event::SpaceMembershipChanged)
}

fn refresh_census_from_bridge(bridge: &DesktopBridge, census: &mut ResMut<DesktopCensus>) {
    match bridge.read_census() {
        Ok(next) => {
            if census.0 != next {
                census.0 = next;
            }
        }
        Err(err) => debug!("desktop census refresh skipped: {err}"),
    }
}

fn sync_desktop_census(
    mut messages: MessageReader<Event>,
    added_displays: Query<(), Added<Display>>,
    displays: Query<(), With<Display>>,
    bridge: Option<NonSend<DesktopBridge>>,
    mut census: ResMut<DesktopCensus>,
) {
    let event_triggered = messages.read().any(is_census_event);
    let initial_needed = !displays.is_empty() && census.0.displays.is_empty();
    if !event_triggered && added_displays.is_empty() && !initial_needed {
        return;
    }
    let Some(bridge) = bridge else {
        return;
    };
    refresh_census_from_bridge(&bridge, &mut census);
}

fn condition_met(bridge: &DesktopBridge, condition: &ConfirmCondition) -> Option<SpaceCensus> {
    let census = bridge.read_census().ok()?;
    let satisfied = match condition {
        ConfirmCondition::SpaceActive(target) => {
            census.find_space(*target).is_some_and(|s| s.active)
        }
        ConfirmCondition::SpacePresent(target) => census
            .find_space(*target)
            .is_some_and(|s| s.kind.is_desktop()),
        ConfirmCondition::SpaceAbsent { space_id, migrated } => {
            census.find_space(*space_id).is_none()
                && migrated.iter().all(|&wid| {
                    bridge
                        .spaces_for_window(wid)
                        .is_ok_and(|m| !m.is_empty() && !m.contains(space_id))
                })
        }
        ConfirmCondition::WindowMemberships { windows, expected } => windows.iter().all(|&wid| {
            bridge.spaces_for_window(wid).is_ok_and(|now| {
                let actual: HashSet<SpaceId> = now.into_iter().collect();
                &actual == expected
            })
        }),
        ConfirmCondition::PerWindowMemberships(pairs) => pairs.iter().all(|(wid, expected)| {
            bridge.spaces_for_window(*wid).is_ok_and(|now| {
                let actual: HashSet<SpaceId> = now.into_iter().collect();
                &actual == expected
            })
        }),
        ConfirmCondition::AllSpacesSubset(items) => items.iter().all(|&(wid, disp_id)| {
            let Some(disp) = census.display(disp_id) else {
                return false;
            };
            let desktops: HashSet<SpaceId> = disp
                .spaces
                .iter()
                .filter(|s| s.kind.is_desktop())
                .map(|s| s.id)
                .collect();
            bridge.spaces_for_window(wid).is_ok_and(|now| {
                let actual: HashSet<SpaceId> = now.into_iter().collect();
                desktops.is_subset(&actual)
            })
        }),
        ConfirmCondition::DisplayOrder {
            display_id,
            expected,
            active_space,
        } => census.display(*display_id).is_some_and(|disp| {
            disp.current_space == *active_space
                && disp.spaces.len() == expected.len()
                && disp
                    .spaces
                    .iter()
                    .zip(expected)
                    .all(|(rec, exp)| rec.id == *exp && rec.kind.is_desktop())
        }),
    };
    satisfied.then_some(census)
}

#[derive(SystemParam)]
struct ConfirmFocusTargets<'w, 's> {
    displays: Query<'w, 's, (Entity, &'static Display)>,
    strips: Query<
        'w,
        's,
        (
            Entity,
            &'static LayoutStrip,
            Has<ActiveWorkspaceMarker>,
            Has<SelectedVirtualMarker>,
        ),
    >,
    windows: Query<'w, 's, (Entity, &'static Window, Has<FocusedMarker>)>,
}

type UnmarkedAddedWindows<'w, 's> = Populated<
    'w,
    's,
    (Entity, &'static Window, Has<Unmanaged>),
    (Added<Window>, Without<StickyWindowMarker>),
>;

fn activate_space_in_ecs(
    space_id: SpaceId,
    preferred_window: Option<Entity>,
    confirmed_census: &SpaceCensus,
    targets: &ConfirmFocusTargets,
    commands: &mut Commands,
) {
    let raw_id = space_id.get();
    let target_strip = targets
        .strips
        .iter()
        .filter(|(_, s, _, _)| s.id() == raw_id)
        .min_by_key(|(_, s, _, selected)| (!*selected, s.virtual_index))
        .map(|(entity, strip, active, _)| (entity, strip.first(), active));
    let focus_target = if let Some((strip_entity, first_win, already_active)) = target_strip {
        if !already_active && let Ok(mut entity_cmds) = commands.get_entity(strip_entity) {
            entity_cmds.try_insert(ActiveWorkspaceMarker);
        }
        preferred_window.or(first_win)
    } else if let Some((disp_entity, disp)) = confirmed_census
        .find_space(space_id)
        .filter(|rec| rec.kind.is_desktop())
        .and_then(|rec| {
            targets
                .displays
                .iter()
                .find(|(_, d)| d.id() == rec.display_id)
        })
    {
        commands.spawn_layout_strip(
            LayoutStrip::new(raw_id, 0),
            disp.bounds().min,
            disp_entity,
            true,
        );
        preferred_window
    } else {
        commands.trigger(SendMessageTrigger(Event::SpaceCurrentChanged {
            space_id: raw_id,
        }));
        preferred_window
    };
    if let Some(win_entity) = focus_target {
        if let Ok((_, _, true)) = targets.windows.get(win_entity)
            && let Ok(mut ec) = commands.get_entity(win_entity)
        {
            ec.try_remove::<FocusedMarker>();
        }
        commands.focus_entity(win_entity, true);
    }
}

fn ensure_strip_and_maybe_focus(
    space_id: SpaceId,
    focus: bool,
    confirmed_census: &SpaceCensus,
    bridge: &mut DesktopBridge,
    targets: &ConfirmFocusTargets,
    commands: &mut Commands,
) -> Option<PendingDesktopOp> {
    let rec = confirmed_census.find_space(space_id)?;
    if !focus {
        let raw_id = space_id.get();
        let exists = targets.strips.iter().any(|(_, s, _, _)| s.id() == raw_id);
        if !exists
            && let Some((disp_entity, disp)) = targets
                .displays
                .iter()
                .find(|(_, d)| d.id() == rec.display_id)
        {
            commands.spawn_layout_strip(
                LayoutStrip::new(raw_id, 0),
                disp.bounds().min,
                disp_entity,
                false,
            );
        }
        return None;
    }
    let disp = confirmed_census.display(rec.display_id)?;
    let hide: Vec<SpaceId> = disp
        .spaces
        .iter()
        .filter(|s| s.id != space_id)
        .map(|s| s.id)
        .collect();
    if let Err(err) = bridge.activate_space(&disp.skylight_display, space_id, &hide) {
        warn!("failed to focus newly created desktop {space_id}: {err}");
        return None;
    }
    Some(PendingDesktopOp::new(
        format!("focus newly created desktop {space_id}"),
        ConfirmCondition::SpaceActive(space_id),
        Some(PostConfirmAction::ActivateAndFocusSpace {
            space_id,
            preferred_window: None,
        }),
    ))
}

fn run_post_confirm(
    action: PostConfirmAction,
    confirmed_census: &SpaceCensus,
    bridge: &mut DesktopBridge,
    targets: &ConfirmFocusTargets,
    commands: &mut Commands,
) -> Option<PendingDesktopOp> {
    match action {
        PostConfirmAction::ActivateAndFocusSpace {
            space_id,
            preferred_window,
        } => {
            activate_space_in_ecs(
                space_id,
                preferred_window,
                confirmed_census,
                targets,
                commands,
            );
            None
        }
        PostConfirmAction::EnsureStripAndMaybeFocus { space_id, focus } => {
            ensure_strip_and_maybe_focus(
                space_id,
                focus,
                confirmed_census,
                bridge,
                targets,
                commands,
            )
        }
        PostConfirmAction::DestroyVacatedSpace {
            space_id,
            neighbor_id,
            preferred_window,
            migrated,
        } => {
            activate_space_in_ecs(
                neighbor_id,
                preferred_window,
                confirmed_census,
                targets,
                commands,
            );
            if let Err(err) = bridge.destroy_space(space_id) {
                warn!("failed to destroy vacated space {space_id}: {err}");
                return None;
            }
            Some(PendingDesktopOp::new(
                format!("destroy vacated desktop {space_id}"),
                ConfirmCondition::SpaceAbsent { space_id, migrated },
                Some(PostConfirmAction::DespawnDestroyedSpace(space_id)),
            ))
        }
        PostConfirmAction::DespawnDestroyedSpace(space_id) => {
            let raw_id = space_id.get();
            if targets.strips.iter().any(|(_, s, _, _)| s.id() == raw_id) {
                commands.trigger(SendMessageTrigger(Event::SpaceDestroyed {
                    space_id: raw_id,
                }));
            }
            None
        }
        PostConfirmAction::ReconcileStickyMarkers(wids) => {
            for (entity, win, _) in &targets.windows {
                if wids.contains(&win.id())
                    && bridge.window_is_sticky(win.id()) == Some(false)
                    && let Ok(mut ec) = commands.get_entity(entity)
                {
                    ec.try_remove::<StickyWindowMarker>();
                }
            }
            None
        }
        PostConfirmAction::SecondSwapStep {
            space_id,
            skylight_display,
            index,
            display_id,
            expected,
            active_space,
        } => {
            if let Err(err) = bridge.move_space_to_index(space_id, &skylight_display, index) {
                warn!("second step of desktop swap failed for {space_id}: {err}");
                return None;
            }
            Some(PendingDesktopOp::new(
                format!("swap desktop step 2 for {space_id}"),
                ConfirmCondition::DisplayOrder {
                    display_id,
                    expected,
                    active_space,
                },
                None,
            ))
        }
    }
}

fn confirm_desktop_ops(
    time: Res<Time<Virtual>>,
    mut messages: MessageReader<Event>,
    mut pending: ResMut<PendingDesktopOps>,
    mut census: ResMut<DesktopCensus>,
    bridge: Option<NonSendMut<DesktopBridge>>,
    targets: ConfirmFocusTargets,
    mut commands: Commands,
) {
    let Some(mut bridge) = bridge else {
        pending.queue.clear();
        return;
    };
    let woke_by_event = messages.read().any(is_confirmation_wake_event);
    let delta = time.delta();

    for step in 0..4 {
        let Some(front) = pending.queue.front_mut() else {
            return;
        };
        let initial_probe = std::mem::replace(&mut front.fresh, false);
        let step_delta = if step == 0 { delta } else { Duration::ZERO };
        front.remaining.tick(step_delta);
        let poll_due = step == 0 && front.poll.tick(step_delta).just_finished();
        let should_probe = initial_probe
            || poll_due
            || front.remaining.is_finished()
            || (step == 0 && woke_by_event);
        if !should_probe {
            return;
        }

        if let Some(confirmed_census) = condition_met(&bridge, &front.condition) {
            let completed = pending.queue.pop_front().expect("front checked");
            debug!("confirmed desktop op: {}", completed.description);
            if census.0 != confirmed_census {
                census.0 = confirmed_census.clone();
            }
            if let Some(action) = completed.on_confirmed
                && let Some(follow_up) = run_post_confirm(
                    action,
                    &confirmed_census,
                    &mut bridge,
                    &targets,
                    &mut commands,
                )
            {
                pending.queue.push_front(follow_up);
                continue;
            }
            return;
        }

        if front.remaining.is_finished() {
            let expired = pending.queue.pop_front().expect("front checked");
            warn!(
                "desktop op '{}' timed out after {:?}; resyncing census",
                expired.description, DESKTOP_CONFIRM_BUDGET
            );
            refresh_census_from_bridge(&bridge, &mut census);
        }
        return;
    }
}

/// Automatically marks newly spawned `0x800` sticky windows as
/// `(StickyWindowMarker, Unmanaged::Floating)` and removes them from any
/// single-Desktop `LayoutStrip`.
fn sync_sticky_windows(
    added_windows: UnmarkedAddedWindows,
    mut strips: Query<&mut LayoutStrip>,
    bridge: Option<NonSend<DesktopBridge>>,
    mut commands: Commands,
) {
    let Some(bridge) = bridge else {
        return;
    };
    for (entity, window, unmanaged) in &added_windows {
        if bridge.window_is_sticky(window.id()) != Some(true) {
            continue;
        }
        debug!(
            "window {} carries SkyLight sticky tag (0x800); marking floating",
            window.id()
        );
        for mut strip in &mut strips {
            if strip.contains(entity) {
                strip.remove(entity);
            }
        }
        if let Ok(mut entity_cmds) = commands.get_entity(entity) {
            if unmanaged {
                entity_cmds.try_insert(StickyWindowMarker);
            } else {
                entity_cmds.try_insert((StickyWindowMarker, Unmanaged::Floating));
            }
        }
    }
}

#[cfg(test)]
mod tests;

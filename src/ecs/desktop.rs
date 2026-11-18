//! ECS integration for native macOS Desktops (`SkyLight` census resource,
//! bounded `WMBridge` confirmation polling, and sticky-window protection).

use std::collections::{HashSet, VecDeque};
use std::time::Duration;

use bevy::app::{App, Plugin, PreUpdate, Update};
use bevy::ecs::component::Component;
use bevy::ecs::entity::Entity;
use bevy::ecs::message::MessageReader;
use bevy::ecs::query::{Added, Has, With, Without};
use bevy::ecs::resource::Resource;
use bevy::ecs::schedule::IntoScheduleConfigs as _;
use bevy::ecs::schedule::SystemCondition as _;
use bevy::ecs::schedule::common_conditions::on_message;
use bevy::ecs::system::{
    Commands, NonSend, NonSendMut, Populated, Query, Res, ResMut, SystemParam,
};
use bevy::time::{Time, Timer, TimerMode, Virtual};
use objc2_core_graphics::CGDirectDisplayID;
use tracing::{debug, warn};

use crate::ecs::layout::LayoutStrip;
use crate::ecs::{
    ActiveWorkspaceMarker, FocusedMarker, PreUpdateSet, SelectedVirtualMarker, SendMessageTrigger,
    SpawnCommandsExt as _, Unmanaged, UpdateSet,
};
use crate::events::Event;
use crate::manager::spaces::{DesktopBridge, DisplaySpaces, SpaceCensus, SpaceId};
use crate::manager::{Display, Window};
use crate::platform::{WinID, WorkspaceId};

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
#[allow(dead_code, reason = "variants used by desktop commands in Commits 3-5")]
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
    DisplayOrder {
        display_id: CGDirectDisplayID,
        expected: Vec<SpaceId>,
    },
}

/// Follow-up work executed once a [`ConfirmCondition`] is observed.
#[allow(dead_code, reason = "variants used by desktop commands in Commits 3-5")]
#[derive(Clone, Debug)]
pub enum PostConfirmAction {
    /// Activate the target Space's selected virtual strip and restore AX focus.
    ActivateAndFocusSpace {
        space_id: SpaceId,
        preferred_window: Option<Entity>,
    },
    /// Second leg of closing the currently active Desktop: destroy the vacated Space.
    DestroyVacatedSpace {
        space_id: SpaceId,
        migrated: Vec<WinID>,
    },
    /// Second leg of a non-adjacent Desktop swap (§5.10).
    SecondSwapStep {
        space_id: SpaceId,
        skylight_display: String,
        index: u32,
        display_id: CGDirectDisplayID,
        expected: Vec<SpaceId>,
    },
}

/// A single bounded in-flight `WMBridge` confirmation item.
pub struct PendingDesktopOp {
    pub description: String,
    pub condition: ConfirmCondition,
    pub remaining: Timer,
    pub poll: Timer,
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
    #[allow(dead_code, reason = "called by desktop commands in Commit 3")]
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
                sync_desktop_census
                    .in_set(PreUpdateSet::Dispatch)
                    .run_if(has_added_displays.or_eager(on_message::<Event>)),
            )
            .add_systems(
                Update,
                (
                    confirm_desktop_ops.run_if(has_pending_ops),
                    sync_sticky_windows,
                )
                    .in_set(UpdateSet::Events)
                    .before(crate::ecs::workspace::workspace_change_handler),
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
        ConfirmCondition::DisplayOrder {
            display_id,
            expected,
        } => census.display(*display_id).is_some_and(|disp| {
            disp.spaces.len() == expected.len()
                && disp
                    .spaces
                    .iter()
                    .zip(expected)
                    .all(|(rec, exp)| rec.id == *exp)
        }),
    };
    satisfied.then_some(census)
}

#[derive(SystemParam)]
struct ConfirmFocusTargets<'w, 's> {
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
    windows: Query<'w, 's, (Entity, Has<FocusedMarker>), With<Window>>,
}

type UnmarkedAddedWindows<'w, 's> = Populated<
    'w,
    's,
    (Entity, &'static Window, Has<Unmanaged>),
    (Added<Window>, Without<StickyWindowMarker>),
>;

fn run_post_confirm(
    action: PostConfirmAction,
    bridge: &mut DesktopBridge,
    targets: &ConfirmFocusTargets,
    commands: &mut Commands,
) -> Option<PendingDesktopOp> {
    match action {
        PostConfirmAction::ActivateAndFocusSpace {
            space_id,
            preferred_window,
        } => {
            let raw_id = space_id.get();
            let target_strip = targets
                .strips
                .iter()
                .filter(|(_, s, _, _)| s.id() == raw_id)
                .min_by_key(|(_, s, _, selected)| (!*selected, s.virtual_index))
                .map(|(entity, strip, active, _)| (entity, strip.first(), active));
            if let Some((strip_entity, first_win, already_active)) = target_strip {
                if !already_active && let Ok(mut entity_cmds) = commands.get_entity(strip_entity) {
                    entity_cmds.try_insert(ActiveWorkspaceMarker);
                }
                let focus_target = preferred_window.or(first_win);
                if let Some(win_entity) = focus_target {
                    if let Ok((_, true)) = targets.windows.get(win_entity)
                        && let Ok(mut ec) = commands.get_entity(win_entity)
                    {
                        ec.try_remove::<FocusedMarker>();
                    }
                    commands.focus_entity(win_entity, true);
                }
            } else {
                commands.trigger(SendMessageTrigger(Event::SpaceCurrentChanged {
                    space_id: raw_id,
                }));
            }
            None
        }
        PostConfirmAction::DestroyVacatedSpace { space_id, migrated } => {
            if let Err(err) = bridge.destroy_space(space_id) {
                warn!("failed to destroy vacated space {space_id}: {err}");
                return None;
            }
            Some(PendingDesktopOp::new(
                format!("destroy vacated desktop {space_id}"),
                ConfirmCondition::SpaceAbsent { space_id, migrated },
                None,
            ))
        }
        PostConfirmAction::SecondSwapStep {
            space_id,
            skylight_display,
            index,
            display_id,
            expected,
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

    let Some(front) = pending.queue.front_mut() else {
        return;
    };
    front.remaining.tick(delta);
    let poll_due = front.poll.tick(delta).just_finished();
    if !woke_by_event && !poll_due && !front.remaining.is_finished() {
        return;
    }

    if let Some(confirmed_census) = condition_met(&bridge, &front.condition) {
        let completed = pending.queue.pop_front().expect("front checked");
        debug!("confirmed desktop op: {}", completed.description);
        if census.0 != confirmed_census {
            census.0 = confirmed_census;
        }
        if let Some(action) = completed.on_confirmed
            && let Some(follow_up) = run_post_confirm(action, &mut bridge, &targets, &mut commands)
        {
            pending.queue.push_front(follow_up);
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

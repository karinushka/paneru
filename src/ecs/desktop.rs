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
    Commands, Local, NonSend, NonSendMut, Populated, Query, Res, ResMut, SystemParam,
};
use bevy::time::{Time, Timer, TimerMode, Virtual};
use objc2_core_graphics::CGDirectDisplayID;
use tracing::{debug, warn};

use crate::ecs::layout::LayoutStrip;
use crate::ecs::{
    ActiveDisplayMarker, ActiveWorkspaceMarker, FocusedMarker, Initializing, PostUpdateSet,
    PreUpdateSet, SelectedVirtualMarker, SendMessageTrigger, SpawnCommandsExt as _, Unmanaged,
    UpdateSet,
};
use crate::events::Event;
#[cfg(test)]
use crate::manager::spaces::DisplaySpaces;
use crate::manager::spaces::{DesktopBridge, SpaceCensus, SpaceId};
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

    #[cfg(test)]
    #[must_use]
    pub fn desktop_count(&self, display_id: CGDirectDisplayID) -> usize {
        self.0
            .display(display_id)
            .map_or(0, DisplaySpaces::desktop_count)
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
    AssignClear {
        memberships: Vec<(WinID, HashSet<SpaceId>)>,
        clear_sticky: Vec<WinID>,
    },
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
    /// Second leg of closing an active Desktop: destroy vacated Space, migrate its strips
    /// to `neighbor_id`, and activate/focus `neighbor_id` when on the active display.
    DestroyVacatedSpace {
        space_id: SpaceId,
        neighbor_id: SpaceId,
        display_id: CGDirectDisplayID,
        activate_display: bool,
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

    /// Pushes a Space-focus op, discarding any superseded `ActivateAndFocusSpace`
    /// probes targeting another Space on `display_id` so rapid switches never
    /// head-of-line block for the 2s timeout budget.
    pub fn push_superseding_focus(
        &mut self,
        display_id: CGDirectDisplayID,
        census: &SpaceCensus,
        op: PendingDesktopOp,
    ) {
        self.queue.retain(|existing| {
            let is_focus_on_same_display = matches!(
                (&existing.condition, &existing.on_confirmed),
                (
                    ConfirmCondition::SpaceActive(sid),
                    Some(PostConfirmAction::ActivateAndFocusSpace { .. }),
                ) if census
                    .find_space(*sid)
                    .is_none_or(|rec| rec.display_id == display_id)
            );
            !is_focus_on_same_display
        });
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
                        .after(confirm_desktop_ops)
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
                    .after(super::systems::detect_tabbed_windows)
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

const MAX_INITIAL_CENSUS_ATTEMPTS: u8 = 3;

fn sync_desktop_census(
    mut messages: MessageReader<Event>,
    added_displays: Query<(), Added<Display>>,
    displays: Query<(), With<Display>>,
    bridge: Option<NonSend<DesktopBridge>>,
    mut census: ResMut<DesktopCensus>,
    mut initial_attempts: Local<u8>,
) {
    let event_triggered = messages.read().any(is_census_event);
    messages.clear();
    let display_added = !added_displays.is_empty();
    if event_triggered || display_added {
        *initial_attempts = 0;
    }
    let initial_needed = !displays.is_empty()
        && census.0.displays.is_empty()
        && *initial_attempts < MAX_INITIAL_CENSUS_ATTEMPTS;
    if !event_triggered && !display_added && !initial_needed {
        return;
    }
    let Some(bridge) = bridge else {
        return;
    };
    if initial_needed && !event_triggered && !display_added {
        *initial_attempts = initial_attempts.saturating_add(1);
    }
    refresh_census_from_bridge(&bridge, &mut census);
    if !census.0.displays.is_empty() {
        *initial_attempts = 0;
    }
}

fn membership_matches(now: &[SpaceId], expected: &HashSet<SpaceId>) -> bool {
    expected.iter().all(|sid| now.contains(sid)) && now.iter().all(|sid| expected.contains(sid))
}

enum ConfirmProbe {
    Confirmed(Option<SpaceCensus>),
    Pending,
}

fn confirm_with_census(
    bridge: &DesktopBridge,
    check: impl FnOnce(&SpaceCensus) -> bool,
) -> ConfirmProbe {
    let Ok(census) = bridge.read_census() else {
        return ConfirmProbe::Pending;
    };
    if check(&census) {
        ConfirmProbe::Confirmed(Some(census))
    } else {
        ConfirmProbe::Pending
    }
}

fn condition_met(bridge: &DesktopBridge, condition: &ConfirmCondition) -> ConfirmProbe {
    match condition {
        ConfirmCondition::WindowMemberships { windows, expected } => {
            let ok = windows.iter().all(|&wid| {
                bridge
                    .spaces_for_window(wid)
                    .is_ok_and(|now| membership_matches(&now, expected))
            });
            if ok {
                ConfirmProbe::Confirmed(None)
            } else {
                ConfirmProbe::Pending
            }
        }
        ConfirmCondition::AssignClear {
            memberships,
            clear_sticky,
        } => {
            let memberships_ok = memberships.iter().all(|(wid, expected)| {
                bridge
                    .spaces_for_window(*wid)
                    .is_ok_and(|now| membership_matches(&now, expected))
            });
            if !memberships_ok {
                return ConfirmProbe::Pending;
            }
            if !clear_sticky.is_empty() {
                let sticky_map = bridge.sticky_windows(clear_sticky);
                if clear_sticky
                    .iter()
                    .any(|wid| sticky_map.get(wid) == Some(&true))
                {
                    return ConfirmProbe::Pending;
                }
            }
            ConfirmProbe::Confirmed(None)
        }
        ConfirmCondition::SpaceActive(target) => {
            confirm_with_census(bridge, |c| c.find_space(*target).is_some_and(|s| s.active))
        }
        ConfirmCondition::SpacePresent(target) => confirm_with_census(bridge, |c| {
            c.find_space(*target).is_some_and(|s| s.kind.is_desktop())
        }),
        ConfirmCondition::SpaceAbsent { space_id, migrated } => confirm_with_census(bridge, |c| {
            c.find_space(*space_id).is_none()
                && migrated.iter().all(|&wid| {
                    bridge
                        .spaces_for_window(wid)
                        .is_ok_and(|m| !m.contains(space_id))
                })
        }),
        ConfirmCondition::AllSpacesSubset(items) => confirm_with_census(bridge, |c| {
            items.iter().all(|&(wid, disp_id)| {
                let Some(disp) = c.display(disp_id) else {
                    return false;
                };
                bridge.spaces_for_window(wid).is_ok_and(|now| {
                    disp.spaces
                        .iter()
                        .filter(|s| s.kind.is_desktop())
                        .all(|s| now.contains(&s.id))
                })
            })
        }),
        ConfirmCondition::DisplayOrder {
            display_id,
            expected,
            active_space,
        } => confirm_with_census(bridge, |c| {
            c.display(*display_id).is_some_and(|disp| {
                disp.current_space == *active_space
                    && disp.spaces.len() == expected.len()
                    && disp
                        .spaces
                        .iter()
                        .zip(expected)
                        .all(|(rec, exp)| rec.id == *exp && rec.kind.is_desktop())
            })
        }),
    }
}

#[derive(SystemParam)]
struct ConfirmFocusTargets<'w, 's> {
    displays: Query<'w, 's, (Entity, &'static Display, Has<ActiveDisplayMarker>)>,
    strips: Query<
        'w,
        's,
        (
            Entity,
            &'static mut LayoutStrip,
            Has<ActiveWorkspaceMarker>,
            Has<SelectedVirtualMarker>,
        ),
    >,
    windows: Query<
        'w,
        's,
        (
            Entity,
            &'static Window,
            Has<FocusedMarker>,
            Has<StickyWindowMarker>,
        ),
    >,
}

type UnmarkedAddedWindows<'w, 's> = Populated<
    'w,
    's,
    (Entity, &'static Window, Has<Unmanaged>),
    (Added<Window>, Without<StickyWindowMarker>),
>;

fn ensure_display_active(
    space_id: SpaceId,
    confirmed_census: &SpaceCensus,
    targets: &ConfirmFocusTargets,
    commands: &mut Commands,
) {
    let Some(target_disp_id) = confirmed_census
        .find_space(space_id)
        .map(|rec| rec.display_id)
    else {
        return;
    };
    let Some((disp_entity, _, already_active)) = targets
        .displays
        .iter()
        .find(|(_, d, _)| d.id() == target_disp_id)
    else {
        return;
    };
    if !already_active && let Ok(mut ec) = commands.get_entity(disp_entity) {
        ec.try_insert(ActiveDisplayMarker);
    }
}

fn activate_space_in_ecs(
    space_id: SpaceId,
    preferred_window: Option<Entity>,
    confirmed_census: &SpaceCensus,
    targets: &ConfirmFocusTargets,
    commands: &mut Commands,
) {
    ensure_display_active(space_id, confirmed_census, targets, commands);
    let live_preferred = preferred_window.filter(|&e| targets.windows.contains(e));
    let raw_id = space_id.get();
    let target_strip = targets
        .strips
        .iter()
        .filter(|(_, s, _, _)| s.id() == raw_id && !s.is_fullscreen())
        .min_by_key(|(_, s, _, selected)| (!*selected, s.virtual_index))
        .map(|(entity, strip, active, _)| (entity, strip.first(), active));
    let focus_target = if let Some((strip_entity, first_win, already_active)) = target_strip {
        if !already_active && let Ok(mut entity_cmds) = commands.get_entity(strip_entity) {
            entity_cmds.try_insert(ActiveWorkspaceMarker);
        }
        live_preferred.or(first_win)
    } else if let Some((disp_entity, disp, _)) = confirmed_census
        .find_space(space_id)
        .filter(|rec| rec.kind.is_desktop())
        .and_then(|rec| {
            targets
                .displays
                .iter()
                .find(|(_, d, _)| d.id() == rec.display_id)
        })
    {
        commands.spawn_layout_strip(
            LayoutStrip::new(raw_id, 0),
            disp.bounds().min,
            disp_entity,
            true,
        );
        live_preferred
    } else {
        commands.trigger(SendMessageTrigger(Event::SpaceCurrentChanged {
            space_id: raw_id,
        }));
        live_preferred
    };
    if let Some(win_entity) = focus_target {
        if let Ok((_, _, true, _)) = targets.windows.get(win_entity)
            && let Ok(mut ec) = commands.get_entity(win_entity)
        {
            ec.try_remove::<FocusedMarker>();
        }
        commands.focus_entity(win_entity, true);
    }
}

fn migrate_confirmed_strips_to_neighbor(
    source_space: SpaceId,
    neighbor_space: SpaceId,
    display_id: CGDirectDisplayID,
    targets: &mut ConfirmFocusTargets,
    commands: &mut Commands,
) -> Option<Entity> {
    let raw_src = source_space.get();
    let raw_dst = neighbor_space.get();
    let mut source_entities: Vec<(u32, Entity)> = targets
        .strips
        .iter()
        .filter(|(_, s, _, _)| s.id() == raw_src)
        .map(|(entity, s, _, _)| (s.virtual_index, entity))
        .collect();
    source_entities.sort_by_key(|(v, _)| *v);
    if source_entities.is_empty() {
        return None;
    }

    let mut combined = LayoutStrip::new(raw_dst, 0);
    for (_, src_entity) in source_entities {
        if let Ok((_, mut src_strip, _, _)) = targets.strips.get_mut(src_entity) {
            combined.append_strip(&mut src_strip);
        }
    }
    let migrated_first = combined.first();
    if combined.len() == 0 {
        return None;
    }

    let existing_dest = targets
        .strips
        .iter()
        .filter(|(_, s, _, _)| s.id() == raw_dst && !s.is_fullscreen())
        .min_by_key(|(_, s, _, selected)| (!*selected, s.virtual_index))
        .map(|(entity, _, _, _)| entity);
    if let Some(dest_entity) = existing_dest {
        if let Ok((_, mut dest_strip, _, _)) = targets.strips.get_mut(dest_entity) {
            dest_strip.append_strip(&mut combined);
        }
    } else if let Some((disp_entity, disp, _)) = targets
        .displays
        .iter()
        .find(|(_, d, _)| d.id() == display_id)
    {
        commands.spawn_layout_strip(combined, disp.bounds().min, disp_entity, false);
    }
    migrated_first
}

fn ensure_strip_exists(
    space_id: SpaceId,
    display_id: CGDirectDisplayID,
    targets: &ConfirmFocusTargets,
    commands: &mut Commands,
) {
    let raw_id = space_id.get();
    let exists = targets.strips.iter().any(|(_, s, _, _)| s.id() == raw_id);
    if !exists
        && let Some((disp_entity, disp, _)) = targets
            .displays
            .iter()
            .find(|(_, d, _)| d.id() == display_id)
    {
        commands.spawn_layout_strip(
            LayoutStrip::new(raw_id, 0),
            disp.bounds().min,
            disp_entity,
            false,
        );
    }
}

fn ensure_strip_and_maybe_focus(
    space_id: SpaceId,
    focus: bool,
    confirmed_census: &SpaceCensus,
    bridge: &mut DesktopBridge,
    targets: &ConfirmFocusTargets,
    commands: &mut Commands,
) -> (Option<PendingDesktopOp>, bool) {
    let Some(rec) = confirmed_census.find_space(space_id) else {
        return (None, false);
    };
    if !focus {
        ensure_strip_exists(space_id, rec.display_id, targets, commands);
        return (None, false);
    }
    let Some(disp) = confirmed_census.display(rec.display_id) else {
        return (None, false);
    };
    let hide: Vec<SpaceId> = disp
        .spaces
        .iter()
        .filter(|s| s.id != space_id)
        .map(|s| s.id)
        .collect();
    if let Err(err) = bridge.activate_space(&disp.skylight_display, space_id, &hide) {
        warn!("failed to focus newly created desktop {space_id}: {err}");
        ensure_strip_exists(space_id, rec.display_id, targets, commands);
        return (None, err.request_may_have_applied);
    }
    (
        Some(PendingDesktopOp::new(
            format!("focus newly created desktop {space_id}"),
            ConfirmCondition::SpaceActive(space_id),
            Some(PostConfirmAction::ActivateAndFocusSpace {
                space_id,
                preferred_window: None,
            }),
        )),
        false,
    )
}

#[allow(clippy::too_many_lines)]
fn run_post_confirm(
    action: PostConfirmAction,
    confirmed_census: &SpaceCensus,
    bridge: &mut DesktopBridge,
    targets: &mut ConfirmFocusTargets,
    commands: &mut Commands,
) -> (Option<PendingDesktopOp>, bool) {
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
            (None, false)
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
            display_id,
            activate_display,
            preferred_window,
            migrated,
        } => {
            if let Err(err) = bridge.destroy_space(space_id) {
                warn!("failed to destroy vacated space {space_id}: {err}");
                if activate_display {
                    activate_space_in_ecs(
                        neighbor_id,
                        preferred_window,
                        confirmed_census,
                        targets,
                        commands,
                    );
                }
                return (None, err.request_may_have_applied);
            }
            let migrated_first = migrate_confirmed_strips_to_neighbor(
                space_id,
                neighbor_id,
                display_id,
                targets,
                commands,
            );
            if activate_display {
                activate_space_in_ecs(
                    neighbor_id,
                    preferred_window.or(migrated_first),
                    confirmed_census,
                    targets,
                    commands,
                );
            } else {
                let raw_neighbor = neighbor_id.get();
                if let Some(strip_entity) = targets
                    .strips
                    .iter()
                    .filter(|(_, s, _, _)| s.id() == raw_neighbor && !s.is_fullscreen())
                    .min_by_key(|(_, s, _, selected)| (!*selected, s.virtual_index))
                    .map(|(entity, _, _, _)| entity)
                    && let Ok(mut ec) = commands.get_entity(strip_entity)
                {
                    ec.try_insert(SelectedVirtualMarker);
                }
            }
            (
                Some(PendingDesktopOp::new(
                    format!("destroy vacated desktop {space_id}"),
                    ConfirmCondition::SpaceAbsent { space_id, migrated },
                    Some(PostConfirmAction::DespawnDestroyedSpace(space_id)),
                )),
                false,
            )
        }
        PostConfirmAction::DespawnDestroyedSpace(space_id) => {
            let raw_id = space_id.get();
            if targets.strips.iter().any(|(_, s, _, _)| s.id() == raw_id) {
                commands.trigger(SendMessageTrigger(Event::SpaceDestroyed {
                    space_id: raw_id,
                }));
            }
            (None, false)
        }
        PostConfirmAction::ReconcileStickyMarkers(wids) => {
            let sticky_map = bridge.sticky_windows(&wids);
            for (entity, win, _, is_sticky_marked) in &targets.windows {
                if is_sticky_marked
                    && sticky_map.get(&win.id()) == Some(&false)
                    && let Ok(mut ec) = commands.get_entity(entity)
                {
                    ec.try_remove::<StickyWindowMarker>();
                }
            }
            (None, false)
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
                return (None, err.request_may_have_applied);
            }
            (
                Some(PendingDesktopOp::new(
                    format!("swap desktop step 2 for {space_id}"),
                    ConfirmCondition::DisplayOrder {
                        display_id,
                        expected,
                        active_space,
                    },
                    None,
                )),
                false,
            )
        }
    }
}

fn confirm_desktop_ops(
    time: Res<Time<Virtual>>,
    mut messages: MessageReader<Event>,
    mut pending: ResMut<PendingDesktopOps>,
    mut census: ResMut<DesktopCensus>,
    bridge: Option<NonSendMut<DesktopBridge>>,
    mut targets: ConfirmFocusTargets,
    mut commands: Commands,
) {
    let Some(mut bridge) = bridge else {
        pending.queue.clear();
        return;
    };
    let woke_by_event = messages.read().any(is_confirmation_wake_event);
    messages.clear();
    let delta = time.delta();

    // Advance budgets for already-waiting items (never for brand-new `fresh`
    // items, whose frame delta includes any pre-command idle sleep).
    for op in &mut pending.queue {
        if !op.fresh {
            op.remaining.tick(delta);
        }
    }

    for step in 0..8 {
        let Some(front) = pending.queue.front_mut() else {
            return;
        };
        let initial_probe = std::mem::replace(&mut front.fresh, false);
        let poll_delta = if initial_probe || step > 0 {
            Duration::ZERO
        } else {
            delta
        };
        let poll_due = !initial_probe && step == 0 && front.poll.tick(poll_delta).just_finished();
        let should_probe =
            initial_probe || poll_due || front.remaining.is_finished() || woke_by_event || step > 0;
        if !should_probe {
            return;
        }

        if let ConfirmProbe::Confirmed(maybe_census) = condition_met(&bridge, &front.condition) {
            let completed = pending.queue.pop_front().expect("front checked");
            debug!("confirmed desktop op: {}", completed.description);
            if let Some(confirmed_census) = maybe_census
                && census.0 != confirmed_census
            {
                census.0 = confirmed_census;
            }
            if let Some(action) = completed.on_confirmed {
                let (follow_up, needs_resync) =
                    run_post_confirm(action, &census.0, &mut bridge, &mut targets, &mut commands);
                if let Some(follow_up) = follow_up {
                    pending.queue.push_front(follow_up);
                }
                if needs_resync {
                    refresh_census_from_bridge(&bridge, &mut census);
                }
            }
            continue;
        }

        if front.remaining.is_finished() {
            let expired = pending.queue.pop_front().expect("front checked");
            warn!(
                "desktop op '{}' timed out after {:?}; resyncing census",
                expired.description, DESKTOP_CONFIRM_BUDGET
            );
            refresh_census_from_bridge(&bridge, &mut census);
            continue;
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
    let added: Vec<(Entity, WinID, bool)> = added_windows
        .iter()
        .map(|(entity, window, unmanaged)| (entity, window.id(), unmanaged))
        .collect();
    let wids: Vec<WinID> = added.iter().map(|&(_, wid, _)| wid).collect();
    let sticky_map = bridge.sticky_windows(&wids);
    for (entity, wid, unmanaged) in added {
        if sticky_map.get(&wid) != Some(&true) {
            continue;
        }
        debug!("window {wid} carries SkyLight sticky tag (0x800); marking floating");
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

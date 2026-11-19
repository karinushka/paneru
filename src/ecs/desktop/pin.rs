//! Per-window multi-Desktop pinning (§5.7), ephemeral process-wide Space
//! assignment (§5.9), and `[windows]` `desktop` placement rules.

use std::collections::HashSet;

use bevy::ecs::component::Component;
use bevy::ecs::entity::Entity;
use bevy::ecs::hierarchy::ChildOf;
use bevy::ecs::query::{Added, Has, With, Without};
use bevy::ecs::system::{Commands, NonSendMut, Populated, Query, Res, ResMut, SystemParam};
use objc2_core_graphics::CGDirectDisplayID;
use tracing::{debug, warn};

use super::ops::{
    DesktopOpWorld, active_display_id, focus_heir_on_active_strip, relocate_tiled_group_to_space,
    resolve_desktop_target,
};
use super::{
    ConfirmCondition, DesktopCensus, PendingDesktopOp, PendingDesktopOps, PostConfirmAction,
    StickyWindowMarker, refresh_census_from_bridge,
};
use crate::commands::{DesktopAssignTarget, DesktopPinTarget};
use crate::config::{Config, DesktopRuleTarget};
use crate::ecs::layout::LayoutStrip;
use crate::ecs::{
    ActiveDisplayMarker, ActiveWorkspaceMarker, SelectedVirtualMarker, SpawnCommandsExt as _,
    Unmanaged, WindowProperties,
};
use crate::manager::spaces::{
    AssignMode, DesktopBridge, SpaceCensus, SpaceId, SpaceRecord, WindowServerMeta,
};
use crate::manager::{Application, Display, Window};
use crate::platform::{Pid, WinID};

/// Tracks windows pinned across multiple ordinary Desktops via `WMBridge` §5.7.
#[derive(Component, Clone, Copy, Debug, PartialEq, Eq)]
pub enum PinnedDesktopWindow {
    /// Joined to all ordinary Desktops on `display_id` and automatically joined
    /// whenever a new Desktop is created on that display.
    AllDesktops { display_id: CGDirectDisplayID },
    /// Joined to a specific subset of Desktops via `desktop pin <N>`.
    SpecificDesktops,
}

/// One-frame spawn marker suppressing automatic `WindowFocused` synthesis when
/// a `[windows]` `desktop = N` rule sends a newly created window to an inactive Desktop.
#[derive(Component, Clone, Copy, Debug, Default)]
pub struct SentToInactiveDesktop;

fn detach_and_float_window(
    entity: Entity,
    pinned: Option<PinnedDesktopWindow>,
    world: &mut DesktopOpWorld,
) {
    for (_, mut strip, _, _, _, _) in &mut world.strips {
        if strip.contains(entity) {
            strip.remove(entity);
        }
    }
    if let Ok(mut ec) = world.commands.get_entity(entity) {
        ec.try_insert((StickyWindowMarker, Unmanaged::Floating));
        if let Some(marker) = pinned {
            ec.try_insert(marker);
        }
    }
}

fn validate_pin_source(
    wid: WinID,
    census: &SpaceCensus,
    bridge: &DesktopBridge,
) -> Result<(CGDirectDisplayID, Vec<SpaceId>), String> {
    if !bridge
        .window_meta(wid)
        .is_some_and(WindowServerMeta::is_app_window)
    {
        return Err(format!(
            "window {wid} is not a manageable application window"
        ));
    }
    if bridge.window_is_sticky(wid) == Some(true) {
        return Err(format!(
            "window {wid} already carries OS sticky tag (0x800)"
        ));
    }
    let memberships = bridge
        .spaces_for_window(wid)
        .map_err(|e| format!("reading spaces for window {wid}: {e}"))?;
    let Some(&first) = memberships.first() else {
        return Err(format!("window {wid} has no managed Space membership"));
    };
    let Some(first_rec) = census.find_space(first) else {
        return Err(format!("window {wid} space {first} missing from census"));
    };
    let disp_id = first_rec.display_id;
    let all_same_display_desktops = memberships.iter().all(|sid| {
        census
            .find_space(*sid)
            .is_some_and(|rec| rec.kind.is_desktop() && rec.display_id == disp_id)
    });
    if !all_same_display_desktops {
        return Err(format!(
            "window {wid} is not exclusively on ordinary Desktops of display {disp_id}"
        ));
    }
    Ok((disp_id, memberships))
}

/// Joins the focused window to one or all ordinary Desktops on its display (§5.7)
/// and transitions it to `Unmanaged::Floating`.
pub(super) fn handle_pin(
    target: &DesktopPinTarget,
    bridge: &mut DesktopBridge,
    world: &mut DesktopOpWorld,
) {
    let Some((win, focused_entity)) = world.windows.focused() else {
        debug!("desktop pin: no focused window");
        return;
    };
    let wid = win.id();
    let Some(active_disp_id) = active_display_id(&world.displays) else {
        return;
    };
    let Ok(census) = bridge
        .read_census()
        .inspect_err(|err| warn!("desktop pin census failed: {err}"))
    else {
        return;
    };

    if matches!(target, DesktopPinTarget::All) && bridge.window_is_sticky(wid) == Some(true) {
        detach_and_float_window(focused_entity, None, world);
        return;
    }

    let (disp_id, memberships) = match validate_pin_source(wid, &census, bridge) {
        Ok(pair) => pair,
        Err(reason) => {
            warn!("desktop pin refused: {reason}");
            return;
        }
    };

    match target {
        DesktopPinTarget::All => {
            pin_window_to_all_desktops(
                wid,
                focused_entity,
                disp_id,
                memberships,
                &census,
                bridge,
                world,
            );
        }
        DesktopPinTarget::Desktop(dest_target) => {
            let Some(dest) = resolve_desktop_target(&census, active_disp_id, dest_target, false)
            else {
                debug!("desktop pin target {dest_target:?} not found");
                return;
            };
            if !dest.kind.is_desktop() || dest.display_id != disp_id {
                warn!(
                    "desktop pin refused: target {} must be a Desktop on display {disp_id}",
                    dest.id
                );
                return;
            }
            if memberships.as_slice() == [dest.id] {
                debug!(
                    "desktop pin: window {wid} is already solely on desktop {}",
                    dest.id
                );
                return;
            }
            if !memberships.contains(&dest.id)
                && let Err(err) = bridge.add_windows_to_space(&[wid], dest.id, false)
            {
                warn!("desktop pin failed adding {wid} to {}: {err}", dest.id);
                if err.request_may_have_applied {
                    refresh_census_from_bridge(bridge, &mut world.census);
                }
                return;
            }
            detach_and_float_window(
                focused_entity,
                Some(PinnedDesktopWindow::SpecificDesktops),
                world,
            );
            let expected: HashSet<SpaceId> = memberships.into_iter().chain([dest.id]).collect();
            world.pending.push(PendingDesktopOp::new(
                format!("pin window {wid} to desktop {}", dest.id),
                ConfirmCondition::WindowMemberships {
                    windows: vec![wid],
                    expected,
                },
                None,
            ));
        }
    }
}

fn pin_window_to_all_desktops(
    wid: WinID,
    focused_entity: Entity,
    disp_id: CGDirectDisplayID,
    memberships: Vec<SpaceId>,
    census: &SpaceCensus,
    bridge: &mut DesktopBridge,
    world: &mut DesktopOpWorld,
) {
    let Some(disp) = census.display(disp_id) else {
        return;
    };
    let desktops: Vec<SpaceId> = disp
        .spaces
        .iter()
        .filter(|s| s.kind.is_desktop())
        .map(|s| s.id)
        .collect();
    let mut added = Vec::new();
    let mut partial_failure = false;
    for &sid in &desktops {
        if memberships.contains(&sid) {
            continue;
        }
        if let Err(err) = bridge.add_windows_to_space(&[wid], sid, false) {
            warn!("desktop pin all failed adding {wid} to {sid}: {err}");
            if err.request_may_have_applied {
                refresh_census_from_bridge(bridge, &mut world.census);
            }
            partial_failure = true;
            break;
        }
        added.push(sid);
    }
    if partial_failure && added.is_empty() && memberships.len() <= 1 {
        return;
    }
    let pin_marker = if partial_failure {
        PinnedDesktopWindow::SpecificDesktops
    } else {
        PinnedDesktopWindow::AllDesktops {
            display_id: disp_id,
        }
    };
    detach_and_float_window(focused_entity, Some(pin_marker), world);
    let expected: HashSet<SpaceId> = memberships.into_iter().chain(added).collect();
    world.pending.push(PendingDesktopOp::new(
        format!("pin window {wid} to all desktops on display {disp_id}"),
        ConfirmCondition::WindowMemberships {
            windows: vec![wid],
            expected,
        },
        None,
    ));
}

/// Collapses a multi-Desktop pinned window back onto the active Desktop (§5.7).
pub(super) fn handle_unpin(bridge: &mut DesktopBridge, world: &mut DesktopOpWorld) {
    let Some((win, focused_entity)) = world.windows.focused() else {
        debug!("desktop unpin: no focused window");
        return;
    };
    let wid = win.id();
    if bridge.window_is_sticky(wid) == Some(true) {
        warn!("desktop unpin refused: window {wid} is sticky (0x800); use desktop assign none");
        return;
    }
    let Some(active_disp_id) = active_display_id(&world.displays) else {
        return;
    };
    let Ok(census) = bridge
        .read_census()
        .inspect_err(|err| warn!("desktop unpin census failed: {err}"))
    else {
        return;
    };
    let Some(cur_space) = census
        .display(active_disp_id)
        .and_then(|d| census.find_space(d.current_space))
        .filter(|s| s.kind.is_desktop())
    else {
        warn!("desktop unpin refused: active Space is not an ordinary Desktop");
        return;
    };

    if let Err(err) = bridge.move_windows_to_space(&[wid], cur_space.id) {
        warn!("desktop unpin {wid} -> {} failed: {err}", cur_space.id);
        if err.request_may_have_applied {
            refresh_census_from_bridge(bridge, &mut world.census);
        }
        return;
    }
    if let Ok(mut ec) = world.commands.get_entity(focused_entity) {
        ec.try_remove::<PinnedDesktopWindow>()
            .try_remove::<StickyWindowMarker>();
    }
    world.pending.push(PendingDesktopOp::new(
        format!("unpin window {wid} onto desktop {}", cur_space.id),
        ConfirmCondition::WindowMemberships {
            windows: vec![wid],
            expected: HashSet::from([cur_space.id]),
        },
        None,
    ));
}

fn collect_pid_desktop_windows(
    pid: Pid,
    census: &SpaceCensus,
    bridge: &DesktopBridge,
) -> Result<Vec<(WinID, CGDirectDisplayID, HashSet<SpaceId>)>, String> {
    let wids = bridge.windows_for_pid(pid);
    let mut out = Vec::new();
    for wid in wids {
        let memberships = bridge
            .spaces_for_window(wid)
            .map_err(|e| format!("reading spaces for window {wid}: {e}"))?;
        if memberships.is_empty() {
            continue;
        }
        let Some(first_rec) = census.find_space(memberships[0]) else {
            return Err(format!(
                "window {wid} space {} missing from census",
                memberships[0]
            ));
        };
        let disp_id = first_rec.display_id;
        if !memberships.iter().all(|sid| {
            census
                .find_space(*sid)
                .is_some_and(|rec| rec.kind.is_desktop() && rec.display_id == disp_id)
        }) {
            return Err(format!(
                "window {wid} of pid {pid} is not on ordinary Desktops"
            ));
        }
        out.push((wid, disp_id, memberships.into_iter().collect()));
    }
    if out.is_empty() {
        return Err(format!("pid {pid} has no managed Desktop windows"));
    }
    Ok(out)
}

/// Applies Dock-equivalent ephemeral process Space assignment (`AllSpaces`,
/// `Space(id)`, or `Clear`, §5.9).
pub(super) fn handle_assign(
    target: &DesktopAssignTarget,
    explicit_pid: Option<Pid>,
    bridge: &mut DesktopBridge,
    world: &mut DesktopOpWorld,
) {
    let Some(pid) = explicit_pid
        .or_else(|| world.windows.focused().and_then(|(w, _)| w.pid().ok()))
        .filter(|&p| p > 0)
    else {
        warn!("desktop assign: unable to resolve positive process pid");
        return;
    };
    let Some(active_disp_id) = active_display_id(&world.displays) else {
        return;
    };
    let Ok(census) = bridge
        .read_census()
        .inspect_err(|err| warn!("desktop assign census failed: {err}"))
    else {
        return;
    };
    let pid_windows = match collect_pid_desktop_windows(pid, &census, bridge) {
        Ok(list) => list,
        Err(reason) => {
            warn!("desktop assign refused: {reason}");
            return;
        }
    };

    match target {
        DesktopAssignTarget::All => assign_pid_all(pid, &pid_windows, bridge, world),
        DesktopAssignTarget::This => {
            let Some(dest) = census
                .display(active_disp_id)
                .and_then(|d| census.find_space(d.current_space))
            else {
                return;
            };
            assign_pid_space(pid, dest, &pid_windows, bridge, world);
        }
        DesktopAssignTarget::Desktop(t) => {
            let Some(dest) = resolve_desktop_target(&census, active_disp_id, t, false) else {
                debug!("desktop assign target {t:?} not found");
                return;
            };
            assign_pid_space(pid, dest, &pid_windows, bridge, world);
        }
        DesktopAssignTarget::None => assign_pid_clear(pid, &census, &pid_windows, bridge, world),
    }
}

fn assign_pid_all(
    pid: Pid,
    pid_windows: &[(WinID, CGDirectDisplayID, HashSet<SpaceId>)],
    bridge: &mut DesktopBridge,
    world: &mut DesktopOpWorld,
) {
    if let Err(err) = bridge.assign_process(pid, AssignMode::AllSpaces) {
        warn!("desktop assign all for pid {pid} failed: {err}");
        if err.request_may_have_applied {
            refresh_census_from_bridge(bridge, &mut world.census);
        }
        return;
    }
    let entities: Vec<Entity> = pid_windows
        .iter()
        .filter_map(|&(wid, ..)| world.windows.find(wid).map(|(_, e)| e))
        .collect();
    for entity in entities {
        detach_and_float_window(entity, None, world);
    }
    let subset_items = pid_windows
        .iter()
        .map(|&(wid, disp_id, _)| (wid, disp_id))
        .collect();
    world.pending.push(PendingDesktopOp::new(
        format!("assign pid {pid} to all desktops"),
        ConfirmCondition::AllSpacesSubset(subset_items),
        None,
    ));
}

fn assign_pid_space(
    pid: Pid,
    dest: &SpaceRecord,
    pid_windows: &[(WinID, CGDirectDisplayID, HashSet<SpaceId>)],
    bridge: &mut DesktopBridge,
    world: &mut DesktopOpWorld,
) {
    if !dest.kind.is_desktop() {
        warn!("desktop assign refused: space {} is not a Desktop", dest.id);
        return;
    }
    if pid_windows
        .iter()
        .any(|(_, disp_id, _)| *disp_id != dest.display_id)
    {
        warn!(
            "desktop assign refused: pid {pid} has windows on another display than {}",
            dest.display_id
        );
        return;
    }
    if let Err(err) = bridge.assign_process(pid, AssignMode::Space(dest.id)) {
        warn!("desktop assign pid {pid} -> {} failed: {err}", dest.id);
        if err.request_may_have_applied {
            refresh_census_from_bridge(bridge, &mut world.census);
        }
        return;
    }

    let focused_entity = world.windows.focused().map(|(_, e)| e);
    let win_entities: Vec<(Entity, bool)> = pid_windows
        .iter()
        .filter_map(|&(wid, ..)| {
            world.windows.find(wid).and_then(|(_, e)| {
                world
                    .windows
                    .get_managed(e)
                    .map(|(_, _, u)| (e, u.is_none()))
            })
        })
        .collect();

    let mut moved_all = HashSet::new();
    let mut active_heir = None;
    let active_space_id = world
        .strips
        .iter()
        .find(|(_, _, _, _, active, _)| *active)
        .map(|(_, s, _, _, _, _)| s.id());
    for &(entity, managed) in &win_entities {
        if let Ok(mut ec) = world.commands.get_entity(entity) {
            ec.try_remove::<PinnedDesktopWindow>()
                .try_remove::<StickyWindowMarker>();
        }
        if managed && !moved_all.contains(&entity) {
            let (moved, heir) =
                relocate_tiled_group_to_space(entity, dest.id, dest.display_id, world);
            if Some(entity) == focused_entity && active_space_id != Some(dest.id.get()) {
                active_heir = heir;
            }
            moved_all.extend(moved);
        } else {
            moved_all.insert(entity);
        }
    }
    if let Some(history) = world.focus_history.as_deref_mut() {
        for &entity in &moved_all {
            history.forget(entity);
        }
    }
    if let Some(focused) = focused_entity
        && moved_all.contains(&focused)
        && active_space_id != Some(dest.id.get())
    {
        focus_heir_on_active_strip(focused, active_heir, world);
    }

    let wids: Vec<WinID> = pid_windows.iter().map(|&(wid, ..)| wid).collect();
    world.pending.push(PendingDesktopOp::new(
        format!("assign pid {pid} to desktop {}", dest.id),
        ConfirmCondition::WindowMemberships {
            windows: wids,
            expected: HashSet::from([dest.id]),
        },
        None,
    ));
}

fn assign_pid_clear(
    pid: Pid,
    census: &SpaceCensus,
    pid_windows: &[(WinID, CGDirectDisplayID, HashSet<SpaceId>)],
    bridge: &mut DesktopBridge,
    world: &mut DesktopOpWorld,
) {
    let mut expected_pairs = Vec::with_capacity(pid_windows.len());
    let mut clear_sticky = Vec::new();
    for (wid, disp_id, prior) in pid_windows {
        let Some(disp) = census.display(*disp_id) else {
            return;
        };
        let is_sticky = bridge.window_is_sticky(*wid) == Some(true);
        if is_sticky {
            if !census
                .find_space(disp.current_space)
                .is_some_and(|s| s.kind.is_desktop())
            {
                warn!(
                    "desktop assign none refused: current space on display {disp_id} is not a Desktop"
                );
                return;
            }
            expected_pairs.push((*wid, HashSet::from([disp.current_space])));
            clear_sticky.push(*wid);
        } else {
            expected_pairs.push((*wid, prior.clone()));
        }
    }

    if let Err(err) = bridge.assign_process(pid, AssignMode::Clear) {
        warn!("desktop assign clear for pid {pid} failed: {err}");
        if err.request_may_have_applied {
            refresh_census_from_bridge(bridge, &mut world.census);
        }
        return;
    }

    for &wid in &clear_sticky {
        if let Some((_, entity)) = world.windows.find(wid)
            && let Ok(mut ec) = world.commands.get_entity(entity)
        {
            ec.try_remove::<PinnedDesktopWindow>();
        }
    }

    world.pending.push(PendingDesktopOp::new(
        format!("clear desktop assignment for pid {pid}"),
        ConfirmCondition::AssignClear {
            memberships: expected_pairs,
            clear_sticky: clear_sticky.clone(),
        },
        Some(PostConfirmAction::ReconcileStickyMarkers(clear_sticky)),
    ));
}

/// Automatically joins `PinnedDesktopWindow::AllDesktops` windows onto newly
/// created ordinary Desktops on their display (§5.7).
pub(super) fn join_pinned_windows_to_new_desktops(
    pinned_windows: Populated<(&Window, &PinnedDesktopWindow)>,
    mut census: ResMut<DesktopCensus>,
    bridge: Option<NonSendMut<DesktopBridge>>,
    mut pending: ResMut<PendingDesktopOps>,
) {
    let Some(mut bridge) = bridge else {
        return;
    };
    let mut resync_census = false;
    for (window, pinned) in &pinned_windows {
        let PinnedDesktopWindow::AllDesktops { display_id } = *pinned else {
            continue;
        };
        let Some(disp) = census.0.display(display_id) else {
            continue;
        };
        let wid = window.id();
        let Ok(memberships) = bridge.spaces_for_window(wid) else {
            continue;
        };
        let missing: Vec<SpaceId> = disp
            .spaces
            .iter()
            .filter(|s| s.kind.is_desktop())
            .map(|s| s.id)
            .filter(|sid| !memberships.contains(sid))
            .collect();
        if missing.is_empty() {
            continue;
        }
        let mut added = Vec::new();
        for &sid in &missing {
            if let Err(err) = bridge.add_windows_to_space(&[wid], sid, false) {
                warn!("auto-join pinned window {wid} to new desktop {sid} failed: {err}");
                resync_census |= err.request_may_have_applied;
                break;
            }
            added.push(sid);
        }
        if added.is_empty() {
            continue;
        }
        let expected: HashSet<SpaceId> = memberships.into_iter().chain(added).collect();
        pending.push(PendingDesktopOp::new(
            format!("auto-join pinned window {wid} to new desktops"),
            ConfirmCondition::WindowMemberships {
                windows: vec![wid],
                expected,
            },
            None,
        ));
    }
    if resync_census {
        refresh_census_from_bridge(&bridge, &mut census);
    }
}

type AddedRuleWindows<'w, 's> = Populated<
    'w,
    's,
    (Entity, &'static Window, &'static ChildOf),
    (Added<Window>, Without<StickyWindowMarker>),
>;
type RuleStrips<'w, 's> = Query<
    'w,
    's,
    (
        Entity,
        &'static mut LayoutStrip,
        Has<ActiveWorkspaceMarker>,
        Has<SelectedVirtualMarker>,
    ),
    Without<Window>,
>;

#[derive(SystemParam)]
pub(super) struct SpawnRuleWorld<'w, 's> {
    apps: Query<'w, 's, &'static Application>,
    displays: Query<'w, 's, (Entity, &'static Display, Has<ActiveDisplayMarker>)>,
    strips: RuleStrips<'w, 's>,
    config: Res<'w, Config>,
    pending: ResMut<'w, PendingDesktopOps>,
    commands: Commands<'w, 's>,
}

/// Applies `[windows.<rule>]` `desktop = N | "all"` placement when new windows
/// spawn at runtime (after startup `Initializing` finishes).
pub(super) fn apply_window_desktop_rules(
    added: AddedRuleWindows,
    bridge: Option<NonSendMut<DesktopBridge>>,
    mut world: SpawnRuleWorld,
) {
    let Some(mut bridge) = bridge else {
        return;
    };
    let mut cached_census: Option<SpaceCensus> = None;

    for (entity, window, parent) in &added {
        let Ok(app) = world.apps.get(parent.parent()) else {
            continue;
        };
        let props = WindowProperties::new(app, window, &world.config);
        let Some(rule_target) = props.desktop() else {
            continue;
        };
        let census = if let Some(c) = &cached_census {
            c
        } else {
            let Ok(fresh) = bridge.read_census() else {
                return;
            };
            cached_census.insert(fresh)
        };
        apply_single_window_desktop_rule(
            entity,
            window.id(),
            rule_target,
            props.floating(),
            census,
            &mut bridge,
            &mut world,
        );
    }
}

fn apply_single_window_desktop_rule(
    entity: Entity,
    wid: WinID,
    rule_target: DesktopRuleTarget,
    floating: bool,
    census: &SpaceCensus,
    bridge: &mut DesktopBridge,
    world: &mut SpawnRuleWorld,
) {
    if bridge.window_is_sticky(wid) == Some(true) {
        debug!("skipping [windows] desktop rule for sticky (0x800) window {wid}");
        return;
    }
    if world
        .strips
        .iter()
        .any(|(_, strip, _, _)| strip.tabbed(entity))
    {
        debug!("skipping [windows] desktop rule for tabbed window {wid}");
        return;
    }
    let Ok(memberships) = bridge.spaces_for_window(wid) else {
        return;
    };
    let disp_id = memberships
        .first()
        .and_then(|&sid| census.find_space(sid))
        .map(|rec| rec.display_id)
        .or_else(|| active_display_id(&world.displays));
    let Some(disp_id) = disp_id else {
        return;
    };
    let Some(disp) = census.display(disp_id) else {
        return;
    };

    match rule_target {
        DesktopRuleTarget::All => {
            apply_rule_all(entity, wid, disp_id, disp, memberships, bridge, world);
        }
        DesktopRuleTarget::Number(num) => {
            apply_rule_number(
                entity,
                wid,
                num,
                floating,
                disp_id,
                disp,
                &memberships,
                bridge,
                world,
            );
        }
    }
}

fn apply_rule_all(
    entity: Entity,
    wid: WinID,
    disp_id: CGDirectDisplayID,
    disp: &crate::manager::spaces::DisplaySpaces,
    memberships: Vec<SpaceId>,
    bridge: &mut DesktopBridge,
    world: &mut SpawnRuleWorld,
) {
    let desktops: Vec<SpaceId> = disp
        .spaces
        .iter()
        .filter(|s| s.kind.is_desktop())
        .map(|s| s.id)
        .collect();
    let mut added = Vec::new();
    let mut failed = false;
    for &sid in &desktops {
        if memberships.contains(&sid) {
            continue;
        }
        if let Err(err) = bridge.add_windows_to_space(&[wid], sid, false) {
            warn!("window rule desktop='all' failed adding {wid} to {sid}: {err}");
            failed = true;
            break;
        }
        added.push(sid);
    }
    if failed && added.is_empty() && memberships.len() <= 1 {
        return;
    }
    let pin_marker = if failed {
        PinnedDesktopWindow::SpecificDesktops
    } else {
        PinnedDesktopWindow::AllDesktops {
            display_id: disp_id,
        }
    };
    if let Ok(mut ec) = world.commands.get_entity(entity) {
        ec.try_insert((StickyWindowMarker, Unmanaged::Floating, pin_marker));
    }
    let expected: HashSet<SpaceId> = memberships.into_iter().chain(added).collect();
    world.pending.push(PendingDesktopOp::new(
        format!("window rule desktop='all' for {wid}"),
        ConfirmCondition::WindowMemberships {
            windows: vec![wid],
            expected,
        },
        None,
    ));
}

#[allow(clippy::too_many_arguments)]
fn apply_rule_number(
    entity: Entity,
    wid: WinID,
    num: std::num::NonZeroU32,
    floating: bool,
    disp_id: CGDirectDisplayID,
    disp: &crate::manager::spaces::DisplaySpaces,
    memberships: &[SpaceId],
    bridge: &mut DesktopBridge,
    world: &mut SpawnRuleWorld,
) {
    let Some(dest) = disp.desktop_by_number(num.get()) else {
        debug!("window rule desktop={num}: out of range on display {disp_id}");
        return;
    };
    let moved = memberships != [dest.id];
    if moved && let Err(err) = bridge.move_windows_to_space(&[wid], dest.id) {
        warn!("window rule desktop={num} move failed for {wid}: {err}");
        return;
    }
    if dest.active {
        if moved {
            if !floating {
                place_spawned_window_on_strip(entity, dest.id, disp_id, world);
            }
            world.pending.push(PendingDesktopOp::new(
                format!("window rule desktop={num} for {wid}"),
                ConfirmCondition::WindowMemberships {
                    windows: vec![wid],
                    expected: HashSet::from([dest.id]),
                },
                None,
            ));
        }
        return;
    }
    if let Ok(mut ec) = world.commands.get_entity(entity) {
        ec.try_insert(SentToInactiveDesktop);
    }
    if !floating {
        place_spawned_window_on_strip(entity, dest.id, disp_id, world);
    }
    world.pending.push(PendingDesktopOp::new(
        format!("window rule desktop={num} for {wid}"),
        ConfirmCondition::WindowMemberships {
            windows: vec![wid],
            expected: HashSet::from([dest.id]),
        },
        None,
    ));
}

fn place_spawned_window_on_strip(
    entity: Entity,
    dest_space: SpaceId,
    display_id: CGDirectDisplayID,
    world: &mut SpawnRuleWorld,
) {
    let raw_dest = dest_space.get();
    let target_strip = world
        .strips
        .iter()
        .filter(|(_, s, _, _)| s.id() == raw_dest && !s.is_fullscreen())
        .min_by_key(|(_, s, _, selected)| (!*selected, s.virtual_index))
        .map(|(e, _, _, _)| e);
    if let Some(strip_entity) = target_strip {
        if let Ok((_, mut strip, _, _)) = world.strips.get_mut(strip_entity) {
            strip.append(entity);
        }
    } else if let Some((disp_entity, disp, _)) =
        world.displays.iter().find(|(_, d, _)| d.id() == display_id)
    {
        let mut new_strip = LayoutStrip::new(raw_dest, 0);
        new_strip.append(entity);
        world
            .commands
            .spawn_layout_strip(new_strip, disp.bounds().min, disp_entity, false)
            .insert((SelectedVirtualMarker, ChildOf(disp_entity)));
    }
}

pub(super) fn cleanup_sent_to_inactive_markers(
    marked: Populated<Entity, With<SentToInactiveDesktop>>,
    mut commands: Commands,
) {
    for entity in &marked {
        if let Ok(mut ec) = commands.get_entity(entity) {
            ec.try_remove::<SentToInactiveDesktop>();
        }
    }
}

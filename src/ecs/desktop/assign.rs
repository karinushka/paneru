//! Ephemeral process-wide native Space assignment (`desktop assign`, §5.9).

use std::collections::HashSet;

use bevy::ecs::entity::Entity;
use objc2_core_graphics::CGDirectDisplayID;
use tracing::{debug, warn};

use super::ops::{
    DesktopOpWorld, focus_heir_on_active_strip, read_active_census, relocate_tiled_group_to_space,
    resolve_desktop_target,
};
use super::pin::{PinnedDesktopWindow, detach_and_float_window};
use super::{
    ConfirmCondition, PendingDesktopOp, PostConfirmAction, StickyWindowMarker,
    validate_desktop_memberships,
};
use crate::commands::DesktopAssignTarget;
use crate::manager::spaces::{AssignMode, DesktopBridge, SpaceCensus, SpaceId, SpaceRecord};
use crate::platform::{Pid, WinID};

fn collect_pid_desktop_windows(
    pid: Pid,
    census: &SpaceCensus,
    bridge: &DesktopBridge,
) -> Result<Vec<(WinID, CGDirectDisplayID, HashSet<SpaceId>)>, String> {
    let out: Vec<_> = bridge
        .windows_for_pid(pid)
        .into_iter()
        .filter_map(
            |wid| match validate_desktop_memberships(wid, census, bridge) {
                Ok(Some((disp_id, memberships))) => {
                    Some(Ok((wid, disp_id, memberships.into_iter().collect())))
                }
                Ok(None) => None,
                Err(reason) => Some(Err(format!("pid {pid}: {reason}"))),
            },
        )
        .collect::<Result<_, _>>()?;
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
    let Some((active_disp_id, census)) = read_active_census("desktop assign", bridge, world) else {
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
                .and_then(|d| d.current_space_record())
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
    let res = bridge.assign_process(pid, AssignMode::AllSpaces);
    if world
        .exec_bridge_op(&format!("desktop assign all for pid {pid}"), res, bridge)
        .is_none()
    {
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
    let res = bridge.assign_process(pid, AssignMode::Space(dest.id));
    if world
        .exec_bridge_op(
            &format!("desktop assign pid {pid} -> {}", dest.id),
            res,
            bridge,
        )
        .is_none()
    {
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
        .find(|(_, _, _, active, _)| *active)
        .map(|(_, s, _, _, _)| s.id());
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
    let wids: Vec<WinID> = pid_windows.iter().map(|&(wid, ..)| wid).collect();
    let sticky_map = bridge.sticky_windows(&wids);
    let Some((expected_pairs, clear_sticky)) = pid_windows.iter().try_fold(
        (Vec::with_capacity(pid_windows.len()), Vec::new()),
        |(mut expected, mut clear), (wid, disp_id, prior)| {
            let disp = census.display(*disp_id)?;
            if sticky_map.get(wid) == Some(&true) {
                if !disp
                    .current_space_record()
                    .is_some_and(|s| s.kind.is_desktop())
                {
                    warn!(
                        "desktop assign none refused: current space on display {disp_id} is not a Desktop"
                    );
                    return None;
                }
                expected.push((*wid, HashSet::from([disp.current_space])));
                clear.push(*wid);
            } else {
                expected.push((*wid, prior.clone()));
            }
            Some((expected, clear))
        },
    ) else {
        return;
    };

    let res = bridge.assign_process(pid, AssignMode::Clear);
    if world
        .exec_bridge_op(&format!("desktop assign clear for pid {pid}"), res, bridge)
        .is_none()
    {
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

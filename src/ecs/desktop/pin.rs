//! Per-window multi-Desktop pinning and auto-joining (`desktop pin`/`unpin`, §5.7).

use std::collections::{HashMap, HashSet};

use bevy::ecs::component::Component;
use bevy::ecs::entity::Entity;
use bevy::ecs::system::{Local, NonSendMut, Populated, ResMut};
use objc2_core_graphics::CGDirectDisplayID;
use tracing::{debug, warn};

use super::ops::{DesktopOpWorld, read_active_census, resolve_desktop_target};
use super::{
    ConfirmCondition, DesktopCensus, PendingDesktopOp, PendingDesktopOps, StickyWindowMarker,
    exec_bridge_op, join_window_to_desktops, refresh_census_from_bridge,
    validate_desktop_memberships,
};
use crate::commands::DesktopPinTarget;
use crate::ecs::Unmanaged;
use crate::manager::Window;
use crate::manager::spaces::{DesktopBridge, SpaceCensus, SpaceId, WindowServerMeta};
use crate::platform::WinID;

/// Tracks windows pinned across multiple ordinary Desktops via `WMBridge` §5.7.
#[derive(Component, Clone, Copy, Debug, PartialEq, Eq)]
pub enum PinnedDesktopWindow {
    /// Joined to all ordinary Desktops on `display_id` and automatically joined
    /// whenever a new Desktop is created on that display.
    AllDesktops { display_id: CGDirectDisplayID },
    /// Joined to a specific subset of Desktops via `desktop pin <N>`.
    SpecificDesktops,
}

pub(super) fn detach_and_float_window(
    entity: Entity,
    pinned: Option<PinnedDesktopWindow>,
    world: &mut DesktopOpWorld,
) {
    world.remove_from_strips_preserving_focus(&[entity], None);
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
    validate_desktop_memberships(wid, census, bridge)?
        .ok_or_else(|| format!("window {wid} has no managed Space membership"))
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
    let Some((active_disp_id, census)) = read_active_census("desktop pin", bridge, world) else {
        return;
    };

    if bridge.window_is_sticky(wid) == Some(true) {
        if matches!(target, DesktopPinTarget::All) {
            detach_and_float_window(focused_entity, None, world);
        } else {
            warn!("desktop pin refused: window {wid} already carries OS sticky tag (0x800)");
        }
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
            if !memberships.contains(&dest.id) {
                let res = bridge.add_windows_to_space(&[wid], dest.id, false);
                if exec_bridge_op(
                    &format!("desktop pin adding {wid} to {}", dest.id),
                    res,
                    bridge,
                    &mut world.census,
                )
                .is_none()
                {
                    return;
                }
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
    let (added, partial_failure, may_have_applied) =
        join_window_to_desktops(wid, disp, &memberships, bridge, "desktop pin all");
    if may_have_applied {
        refresh_census_from_bridge(bridge, &mut world.census);
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
    let Some((active_disp_id, census)) = read_active_census("desktop unpin", bridge, world) else {
        return;
    };
    let Some(cur_space) = census
        .display(active_disp_id)
        .and_then(|d| d.current_space_record())
        .filter(|s| s.kind.is_desktop())
    else {
        warn!("desktop unpin refused: active Space is not an ordinary Desktop");
        return;
    };

    let res = bridge.move_windows_to_space(&[wid], cur_space.id);
    if exec_bridge_op(
        &format!("desktop unpin {wid} -> {}", cur_space.id),
        res,
        bridge,
        &mut world.census,
    )
    .is_none()
    {
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

/// Automatically joins `PinnedDesktopWindow::AllDesktops` windows onto newly
/// created ordinary Desktops on their display (§5.7).
pub(super) fn join_pinned_windows_to_new_desktops(
    pinned_windows: Populated<(&Window, &PinnedDesktopWindow)>,
    mut census: ResMut<DesktopCensus>,
    bridge: Option<NonSendMut<DesktopBridge>>,
    mut pending: ResMut<PendingDesktopOps>,
    mut seen_desktops: Local<HashMap<CGDirectDisplayID, Vec<SpaceId>>>,
) {
    let Some(mut bridge) = bridge else {
        return;
    };
    seen_desktops.retain(|disp_id, _| census.0.display(*disp_id).is_some());

    let current_by_display: Vec<(CGDirectDisplayID, Vec<SpaceId>)> = census
        .0
        .displays
        .iter()
        .map(|disp| (disp.display_id, disp.desktop_ids().collect()))
        .collect();
    let displays_with_new_desktops: HashSet<CGDirectDisplayID> = current_by_display
        .iter()
        .filter(|(disp_id, current)| {
            seen_desktops
                .get(disp_id)
                .is_none_or(|seen| current.iter().any(|sid| !seen.contains(sid)))
        })
        .map(|&(disp_id, _)| disp_id)
        .collect();

    let mut failed_displays = HashSet::new();
    let mut resync_census = false;
    if !displays_with_new_desktops.is_empty() {
        for (window, pinned) in &pinned_windows {
            let PinnedDesktopWindow::AllDesktops { display_id } = *pinned else {
                continue;
            };
            if !displays_with_new_desktops.contains(&display_id) {
                continue;
            }
            let Some(disp) = census.0.display(display_id) else {
                continue;
            };
            let wid = window.id();
            let Ok(memberships) = bridge.spaces_for_window(wid) else {
                failed_displays.insert(display_id);
                continue;
            };
            let (added, failed, may_have_applied) = join_window_to_desktops(
                wid,
                disp,
                &memberships,
                &mut bridge,
                "auto-join pinned window",
            );
            resync_census |= may_have_applied;
            if failed {
                failed_displays.insert(display_id);
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
    }

    for (disp_id, current) in current_by_display {
        if !failed_displays.contains(&disp_id) {
            seen_desktops.insert(disp_id, current);
        }
    }
    if resync_census {
        refresh_census_from_bridge(&bridge, &mut census);
    }
}

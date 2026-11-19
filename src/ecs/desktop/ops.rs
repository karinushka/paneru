//! Execution of `Command::Desktop(...)` operations (`Focus`, `MoveWindow`).

use std::collections::HashSet;

use bevy::ecs::entity::Entity;
use bevy::ecs::hierarchy::ChildOf;
use bevy::ecs::message::MessageReader;
use bevy::ecs::query::{Has, Without};
use bevy::ecs::system::{Commands, NonSendMut, Query, ResMut, SystemParam};
use objc2_core_graphics::CGDirectDisplayID;
use tracing::{debug, warn};

use super::{
    ConfirmCondition, DesktopCensus, PendingDesktopOp, PendingDesktopOps, PostConfirmAction,
    refresh_census_from_bridge,
};
use crate::commands::{Command, DesktopOperation, DesktopTarget, Direction, MoveFocus};
use crate::ecs::focus::FocusHistory;
use crate::ecs::layout::LayoutStrip;
use crate::ecs::params::Windows;
use crate::ecs::workspace::PreviousStripPosition;
use crate::ecs::{
    ActiveDisplayMarker, ActiveWorkspaceMarker, FocusedMarker, OpenedFrom, Position,
    SelectedVirtualMarker, SpawnCommandsExt as _, Unmanaged, Window,
};
use crate::events::Event;
use crate::manager::Display;
use crate::manager::spaces::{
    DesktopBridge, DisplaySpaces, SpaceCensus, SpaceId, SpaceKind, SpaceRecord, WindowServerMeta,
};
use crate::platform::WinID;

pub(super) type DesktopStrips<'w, 's> = Query<
    'w,
    's,
    (
        Entity,
        &'static mut LayoutStrip,
        &'static Position,
        Option<&'static PreviousStripPosition>,
        Has<ActiveWorkspaceMarker>,
        Has<SelectedVirtualMarker>,
    ),
    Without<Window>,
>;

#[derive(SystemParam)]
pub(super) struct DesktopOpWorld<'w, 's> {
    pub(super) displays: Query<'w, 's, (Entity, &'static Display, Has<ActiveDisplayMarker>)>,
    pub(super) strips: DesktopStrips<'w, 's>,
    pub(super) windows: Windows<'w, 's>,
    pub(super) opened_from: Query<'w, 's, &'static OpenedFrom>,
    pub(super) focus_history: Option<ResMut<'w, FocusHistory>>,
    pub(super) pending: ResMut<'w, PendingDesktopOps>,
    pub(super) census: ResMut<'w, DesktopCensus>,
    pub(super) commands: Commands<'w, 's>,
}

/// Resolves a [`DesktopTarget`] against `census`.
///
/// When `include_fullscreen_on_direction` is `true` (`desktop focus`),
/// `West`/`East`/`First`/`Last` step across every managed Space on the display
/// (including native Fullscreen Spaces). When `false` (`move`, `send`, etc.),
/// directional targets step to the nearest ordinary `SpaceKind::Desktop`.
#[must_use]
pub(crate) fn resolve_desktop_target<'a>(
    census: &'a SpaceCensus,
    active_display_id: CGDirectDisplayID,
    target: &DesktopTarget,
    include_fullscreen_on_direction: bool,
) -> Option<&'a SpaceRecord> {
    match target {
        DesktopTarget::SpaceId(raw) => SpaceId::new(*raw).and_then(|sid| census.find_space(sid)),
        DesktopTarget::Number { number, display } => census
            .display(display.unwrap_or(active_display_id))
            .and_then(|disp| disp.desktop_by_number(*number)),
        DesktopTarget::Direction { direction, display } => {
            let disp = census.display(display.unwrap_or(active_display_id))?;
            resolve_directional_space(disp, direction, include_fullscreen_on_direction)
        }
    }
}

fn resolve_directional_space<'a>(
    disp: &'a DisplaySpaces,
    direction: &Direction,
    include_fullscreen: bool,
) -> Option<&'a SpaceRecord> {
    let cur_idx = disp.spaces.iter().position(|s| s.active)?;
    if include_fullscreen {
        return match direction {
            Direction::West | Direction::North => {
                cur_idx.checked_sub(1).and_then(|i| disp.spaces.get(i))
            }
            Direction::East | Direction::South => disp.spaces.get(cur_idx + 1),
            Direction::First => disp.spaces.first(),
            Direction::Last => disp.spaces.last(),
            Direction::Nth(idx) => u32::try_from(idx.saturating_add(1))
                .ok()
                .and_then(|n| disp.desktop_by_number(n)),
        };
    }
    match direction {
        Direction::West | Direction::North => {
            disp.spaces[..cur_idx].iter().rfind(|s| s.kind.is_desktop())
        }
        Direction::East | Direction::South => disp.spaces[cur_idx + 1..]
            .iter()
            .find(|s| s.kind.is_desktop()),
        Direction::First => disp.spaces.iter().find(|s| s.kind.is_desktop()),
        Direction::Last => disp.spaces.iter().rfind(|s| s.kind.is_desktop()),
        Direction::Nth(idx) => u32::try_from(idx.saturating_add(1))
            .ok()
            .and_then(|n| disp.desktop_by_number(n)),
    }
}

pub(super) fn active_display_id(
    displays: &Query<(Entity, &Display, Has<ActiveDisplayMarker>)>,
) -> Option<CGDirectDisplayID> {
    displays
        .iter()
        .find_map(|(_, d, active)| active.then_some(d.id()))
        .or_else(|| displays.iter().next().map(|(_, d, _)| d.id()))
}

pub(super) fn sibling_space_ids(census: &SpaceCensus, target: &SpaceRecord) -> Vec<SpaceId> {
    census
        .display(target.display_id)
        .map(|d| {
            d.spaces
                .iter()
                .filter(|s| s.id != target.id)
                .map(|s| s.id)
                .collect()
        })
        .unwrap_or_default()
}

pub(super) fn preferred_focus_on_space(
    space_id: SpaceId,
    strips: &DesktopStrips,
    focus_history: Option<&FocusHistory>,
) -> Option<Entity> {
    let raw = space_id.get();
    let target_strip = strips
        .iter()
        .filter(|(_, s, _, _, _, _)| s.id() == raw)
        .min_by_key(|(_, s, _, _, _, selected)| (!*selected, s.virtual_index));
    if let Some((_, strip, _, prev, _, _)) = target_strip {
        if let Some(prev_focus) = prev.and_then(|p| p.focus).filter(|&e| strip.contains(e)) {
            return Some(prev_focus);
        }
        if let Some(hist) = focus_history
            .and_then(|h| h.last_managed(raw))
            .filter(|&e| strip.contains(e))
        {
            return Some(hist);
        }
        if let Some(first) = strip.first() {
            return Some(first);
        }
    }
    focus_history.and_then(|h| h.last_floating(raw))
}

/// Records the active native Space's remembered window in [`FocusHistory`]
/// before switching away. Never inserts `PreviousStripPosition` onto an active
/// or native-Desktop strip: `PreviousStripPosition` belongs solely to parked
/// off-screen virtual workspace rows, and its presence causes
/// `reshuffle_layout_strip` / `ensure_visible_in_strip` to treat the strip as
/// newly activating and skip scrolling windows into view.
pub(super) fn save_active_strip_focus(
    strips: &DesktopStrips,
    focused_entity: Option<Entity>,
    windows: &Windows,
    focus_history: Option<&mut FocusHistory>,
) {
    let Some(history) = focus_history else {
        return;
    };
    let Some((_, strip, _, _, _, _)) = strips.iter().find(|(_, _, _, _, active, _)| *active) else {
        return;
    };
    if let Some(entity) = focused_entity {
        let unmanaged = windows.get_managed(entity).and_then(|(_, _, u)| u);
        if strip.contains(entity) || matches!(unmanaged, Some(Unmanaged::Floating)) {
            history.record(strip.id(), entity, unmanaged);
            return;
        }
    }
    if let Some(first) = strip.first() {
        history.record(strip.id(), first, None);
    }
}

/// Validates that `wid` can be moved to `dest` under §5.6 (either from ordinary
/// Desktops, or rescued as an unassociated guest from a single-owner Fullscreen Space).
fn validate_window_move(
    wid: WinID,
    dest: &SpaceRecord,
    census: &SpaceCensus,
    bridge: &DesktopBridge,
) -> Result<Vec<SpaceId>, String> {
    if !dest.kind.is_desktop() {
        return Err(format!("destination space {} is not a Desktop", dest.id));
    }
    if !bridge
        .window_meta(wid)
        .is_some_and(WindowServerMeta::is_app_window)
    {
        return Err(format!(
            "window {wid} is not a manageable application window"
        ));
    }
    let memberships = bridge
        .spaces_for_window(wid)
        .map_err(|e| format!("reading spaces for window {wid}: {e}"))?;
    if memberships.is_empty() {
        return Err(format!("window {wid} has no managed Space membership"));
    }
    match bridge.window_is_sticky(wid) {
        Some(true) => {
            return Err(format!(
                "window {wid} is sticky (0x800) and cannot be moved"
            ));
        }
        None if memberships.len() > 1 => {
            return Err(format!(
                "cannot verify sticky tag on multi-Space window {wid}"
            ));
        }
        Some(false) | None => {}
    }

    let all_desktops = memberships
        .iter()
        .all(|sid| census.find_space(*sid).is_some_and(|s| s.kind.is_desktop()));
    if all_desktops {
        return Ok(memberships);
    }

    // §5.6 Case B: Single-owner Fullscreen guest window rescue.
    if memberships.len() == 1
        && let Some(source) = census.find_space(memberships[0])
        && let SpaceKind::Fullscreen {
            owner_wid: Some(owner),
            single_tile: true,
        } = source.kind
        && owner != wid
        && source.display_id == dest.display_id
    {
        let assoc = bridge
            .associated_windows(wid)
            .map_err(|e| format!("associated windows for {wid}: {e}"))?;
        if !assoc.contains(&owner) {
            return Ok(memberships);
        }
        return Err(format!(
            "window {wid} is associated with fullscreen owner {owner}"
        ));
    }

    Err(format!(
        "window {wid} is on a fullscreen or unsupported Space"
    ))
}

#[allow(clippy::too_many_arguments)]
fn dispatch_focus_space(
    target: &SpaceRecord,
    census: &SpaceCensus,
    active_disp_id: CGDirectDisplayID,
    preferred_window: Option<Entity>,
    departure_focus: Option<Entity>,
    bridge: &mut DesktopBridge,
    world: &mut DesktopOpWorld,
) -> bool {
    if target.active && target.display_id == active_disp_id {
        if let Some(win) = preferred_window {
            world.commands.focus_entity(win, true);
        }
        return true;
    }
    let hide = sibling_space_ids(census, target);
    if let Err(err) = bridge.activate_space(&target.skylight_display, target.id, &hide) {
        warn!("desktop focus {} failed: {err}", target.id);
        if err.request_may_have_applied {
            refresh_census_from_bridge(bridge, &mut world.census);
        }
        return false;
    }
    save_active_strip_focus(
        &world.strips,
        departure_focus,
        &world.windows,
        world.focus_history.as_deref_mut(),
    );
    let preferred = preferred_window.or_else(|| {
        preferred_focus_on_space(target.id, &world.strips, world.focus_history.as_deref())
    });
    world.pending.push_superseding_focus(
        target.display_id,
        census,
        PendingDesktopOp::new(
            format!("activate space {}", target.id),
            ConfirmCondition::SpaceActive(target.id),
            Some(PostConfirmAction::ActivateAndFocusSpace {
                space_id: target.id,
                preferred_window: preferred,
            }),
        ),
    );
    true
}

fn handle_focus(target: &DesktopTarget, bridge: &mut DesktopBridge, world: &mut DesktopOpWorld) {
    let Some(active_disp_id) = active_display_id(&world.displays) else {
        return;
    };
    let Ok(census) = bridge
        .read_census()
        .inspect_err(|err| warn!("desktop focus census failed: {err}"))
    else {
        return;
    };
    let Some(dest) = resolve_desktop_target(&census, active_disp_id, target, true) else {
        debug!("desktop focus target {target:?} out of range or not found");
        return;
    };
    let focused_entity = world.windows.focused().map(|(_, e)| e);
    let fullscreen_owner = match dest.kind {
        SpaceKind::Fullscreen {
            owner_wid: Some(owner),
            ..
        } => world.windows.find(owner).map(|(_, e)| e),
        _ => None,
    };
    dispatch_focus_space(
        dest,
        &census,
        active_disp_id,
        fullscreen_owner,
        focused_entity,
        bridge,
        world,
    );
}

pub(super) fn relocate_tiled_group_to_space(
    focused_entity: Entity,
    dest_space: SpaceId,
    dest_display_id: CGDirectDisplayID,
    world: &mut DesktopOpWorld,
) -> (Vec<Entity>, Option<Entity>) {
    let source_info = world
        .strips
        .iter()
        .find(|(_, s, _, _, _, _)| s.contains(focused_entity))
        .map(|(_, strip, _, _, _, _)| {
            let group = strip.relocation_group(focused_entity);
            let opener = world.opened_from.get(focused_entity).ok().map(|o| o.0);
            let heir = strip.heir(focused_entity, opener);
            (group, heir)
        });
    let (group, heir) = match source_info {
        Some(pair) => pair,
        None if matches!(
            world.windows.get_managed(focused_entity),
            Some((_, _, None))
        ) =>
        {
            (None, None)
        }
        None => return (vec![focused_entity], None),
    };
    let moving_entities = group
        .as_ref()
        .map_or_else(|| vec![focused_entity], |g| g.members().to_vec());

    let raw_dest = dest_space.get();
    let existing_dest = world
        .strips
        .iter()
        .filter(|(_, s, _, _, _, _)| s.id() == raw_dest && !s.is_fullscreen())
        .min_by_key(|(_, s, _, _, _, selected)| (!*selected, s.virtual_index))
        .map(|(entity, _, _, _, _, _)| entity);

    for (entity, mut strip, _, _, _, _) in &mut world.strips {
        if Some(entity) == existing_dest {
            strip.append_relocation_group(focused_entity, group.as_ref());
        } else {
            for &moving in &moving_entities {
                strip.remove(moving);
            }
        }
    }

    if existing_dest.is_none()
        && let Some((disp_entity, disp, _)) = world
            .displays
            .iter()
            .find(|(_, d, _)| d.id() == dest_display_id)
    {
        let mut new_strip = LayoutStrip::new(raw_dest, 0);
        new_strip.append_relocation_group(focused_entity, group.as_ref());
        world
            .commands
            .spawn_layout_strip(new_strip, disp.bounds().min, disp_entity, false)
            .insert((SelectedVirtualMarker, ChildOf(disp_entity)));
    }

    (moving_entities, heir)
}

fn handle_move_window(
    target: &DesktopTarget,
    move_focus: MoveFocus,
    bridge: &mut DesktopBridge,
    world: &mut DesktopOpWorld,
) {
    let Some((_, focused_entity)) = world.windows.focused() else {
        debug!("desktop move/send: no focused window");
        return;
    };
    let Some(active_disp_id) = active_display_id(&world.displays) else {
        return;
    };
    let Ok(census) = bridge
        .read_census()
        .inspect_err(|err| warn!("desktop move census failed: {err}"))
    else {
        return;
    };
    let Some(dest) = resolve_desktop_target(&census, active_disp_id, target, false) else {
        debug!("desktop move target {target:?} out of range or not found");
        return;
    };

    let moving_entities: Vec<Entity> = world
        .strips
        .iter()
        .find(|(_, s, _, _, _, _)| s.contains(focused_entity))
        .and_then(|(_, s, _, _, _, _)| s.relocation_group(focused_entity))
        .map_or_else(|| vec![focused_entity], |g| g.members().to_vec());
    let moving_wids: Vec<WinID> = moving_entities
        .iter()
        .filter_map(|&e| world.windows.get(e).map(|w| w.id()))
        .collect();
    if moving_wids.is_empty() {
        return;
    }

    let mut already_sole = true;
    for &wid in &moving_wids {
        match validate_window_move(wid, dest, &census, bridge) {
            Ok(memberships) => {
                already_sole &= memberships.as_slice() == [dest.id];
            }
            Err(reason) => {
                warn!("desktop move refused: {reason}");
                return;
            }
        }
    }

    if already_sole {
        if matches!(move_focus, MoveFocus::Follow) {
            dispatch_focus_space(
                dest,
                &census,
                active_disp_id,
                Some(focused_entity),
                Some(focused_entity),
                bridge,
                world,
            );
        }
        return;
    }

    if let Err(err) = bridge.move_windows_to_space(&moving_wids, dest.id) {
        warn!("desktop move to {} failed: {err}", dest.id);
        if err.request_may_have_applied {
            refresh_census_from_bridge(bridge, &mut world.census);
        }
        return;
    }

    let (moved, heir) =
        relocate_tiled_group_to_space(focused_entity, dest.id, dest.display_id, world);
    if let Some(history) = world.focus_history.as_deref_mut() {
        for entity in moved {
            history.forget(entity);
        }
        history.forget(focused_entity);
    }

    finish_move_window(
        move_focus,
        focused_entity,
        heir,
        moving_wids,
        dest,
        &census,
        active_disp_id,
        bridge,
        world,
    );
}

pub(super) fn focus_heir_on_active_strip(
    focused_entity: Entity,
    heir: Option<Entity>,
    world: &mut DesktopOpWorld,
) {
    let next_focus = heir.or_else(|| {
        world
            .strips
            .iter()
            .find(|(_, _, _, _, active, _)| *active)
            .and_then(|(_, s, _, _, _, _)| s.first())
    });
    if let Some(next_entity) = next_focus {
        world.commands.focus_entity(next_entity, true);
    } else if let Ok(mut ec) = world.commands.get_entity(focused_entity) {
        ec.try_remove::<FocusedMarker>();
    }
}

#[allow(clippy::too_many_arguments)]
fn finish_move_window(
    move_focus: MoveFocus,
    focused_entity: Entity,
    heir: Option<Entity>,
    moving_wids: Vec<WinID>,
    dest: &SpaceRecord,
    census: &SpaceCensus,
    active_disp_id: CGDirectDisplayID,
    bridge: &mut DesktopBridge,
    world: &mut DesktopOpWorld,
) {
    match move_focus {
        MoveFocus::Stay => {
            world.pending.push(PendingDesktopOp::new(
                format!("send windows {moving_wids:?} to {}", dest.id),
                ConfirmCondition::WindowMemberships {
                    windows: moving_wids,
                    expected: HashSet::from([dest.id]),
                },
                None,
            ));
            focus_heir_on_active_strip(focused_entity, heir, world);
        }
        MoveFocus::Follow => {
            let already_active_dest = dest.active && dest.display_id == active_disp_id;
            let switched = dispatch_focus_space(
                dest,
                census,
                active_disp_id,
                Some(focused_entity),
                heir,
                bridge,
                world,
            );
            if !switched || already_active_dest {
                world.pending.push(PendingDesktopOp::new(
                    format!("move windows {moving_wids:?} to {}", dest.id),
                    ConfirmCondition::WindowMemberships {
                        windows: moving_wids,
                        expected: HashSet::from([dest.id]),
                    },
                    None,
                ));
                if !switched {
                    focus_heir_on_active_strip(focused_entity, heir, world);
                }
            }
        }
    }
}

pub(super) fn desktop_command_handler(
    mut messages: MessageReader<Event>,
    bridge: Option<NonSendMut<DesktopBridge>>,
    mut world: DesktopOpWorld,
) {
    let ops: Vec<DesktopOperation> = messages
        .read()
        .filter_map(|event| match event {
            Event::Command {
                command: Command::Desktop(op),
            } => Some(op.clone()),
            _ => None,
        })
        .collect();
    if ops.is_empty() {
        return;
    }
    let Some(mut bridge) = bridge else {
        warn!("desktop command ignored: native DesktopBridge unavailable");
        return;
    };

    for op in &ops {
        match op {
            DesktopOperation::Focus(target) => handle_focus(target, &mut bridge, &mut world),
            DesktopOperation::MoveWindow { target, move_focus } => {
                handle_move_window(target, *move_focus, &mut bridge, &mut world);
            }
            DesktopOperation::Create { focus } => {
                super::lifecycle::handle_create(*focus, &mut bridge, &mut world);
            }
            DesktopOperation::Destroy { target, empty_only } => {
                super::lifecycle::handle_destroy(
                    target.as_ref(),
                    *empty_only,
                    &mut bridge,
                    &mut world,
                );
            }
            DesktopOperation::Reorder(target) => {
                super::lifecycle::handle_reorder(target, &mut bridge, &mut world);
            }
            DesktopOperation::Swap(target) => {
                super::lifecycle::handle_swap(target, &mut bridge, &mut world);
            }
            DesktopOperation::Pin(target) => {
                super::pin::handle_pin(target, &mut bridge, &mut world);
            }
            DesktopOperation::Unpin => {
                super::pin::handle_unpin(&mut bridge, &mut world);
            }
            DesktopOperation::Assign { target, pid } => {
                super::pin::handle_assign(target, *pid, &mut bridge, &mut world);
            }
        }
    }
}

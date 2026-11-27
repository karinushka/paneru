//! Execution of `Command::Desktop(...)` operations (`Focus`, `MoveWindow`).

use std::collections::HashSet;

use bevy::ecs::entity::Entity;
use bevy::ecs::hierarchy::ChildOf;
use bevy::ecs::message::MessageReader;
use bevy::ecs::query::{Has, With, Without};
use bevy::ecs::system::{Commands, NonSendMut, Query, Res, ResMut, SystemParam};
use objc2_core_graphics::CGDirectDisplayID;
use tracing::{debug, warn};

use super::{
    ConfirmCondition, DesktopBridgeState, DesktopCensus, PendingDesktopOp, PendingDesktopOps,
    PostConfirmAction, exec_bridge_op, flash_unavailable_once, probe_bridge_if_needed,
    refresh_census_from_bridge, select_desktop_strip, spawn_desktop_strip,
};
use crate::commands::{Command, DesktopOperation, DesktopTarget, Direction, MoveFocus};
use crate::config::Config;
use crate::ecs::focus::FocusHistory;
use crate::ecs::layout::{
    DisplayViewports, LayoutStrip, compensate_left_removal, remove_left_column_width,
};
use crate::ecs::params::Windows;
use crate::ecs::workspace::PreviousStripPosition;
use crate::ecs::{
    ActiveDisplayMarker, ActiveWorkspaceMarker, FocusedMarker, ManualStripOffset, OpenedFrom,
    Position, RepositionMarker, Scrolling, SelectedVirtualMarker, SpawnCommandsExt as _, Unmanaged,
    Window,
};
use crate::events::Event;
use crate::manager::Display;
use crate::manager::spaces::{
    DesktopBridge, DisplaySpaces, SpaceCensus, SpaceError, SpaceId, SpaceKind, SpaceRecord,
    WindowServerMeta,
};
use crate::platform::WinID;

pub(super) type DesktopStrips<'w, 's> = Query<
    'w,
    's,
    (
        Entity,
        &'static mut LayoutStrip,
        Option<&'static PreviousStripPosition>,
        Has<ActiveWorkspaceMarker>,
        Has<SelectedVirtualMarker>,
    ),
    Without<Window>,
>;

type DesktopStripOffsets<'w, 's> = Query<
    'w,
    's,
    (
        &'static mut Position,
        Option<&'static mut RepositionMarker>,
        Option<&'static mut Scrolling>,
        Option<&'static ChildOf>,
        Has<ManualStripOffset>,
    ),
    (With<LayoutStrip>, Without<Window>),
>;

#[derive(SystemParam)]
pub(super) struct DesktopOpWorld<'w, 's> {
    pub(super) displays: Query<'w, 's, (Entity, &'static Display, Has<ActiveDisplayMarker>)>,
    pub(super) display_viewports: DisplayViewports<'w, 's>,
    pub(super) strips: DesktopStrips<'w, 's>,
    pub(super) strip_offsets: DesktopStripOffsets<'w, 's>,
    pub(super) windows: Windows<'w, 's>,
    pub(super) opened_from: Query<'w, 's, &'static OpenedFrom>,
    pub(super) focus_history: Option<ResMut<'w, FocusHistory>>,
    pub(super) pending: ResMut<'w, PendingDesktopOps>,
    pub(super) census: ResMut<'w, DesktopCensus>,
    pub(super) bridge_state: ResMut<'w, DesktopBridgeState>,
    pub(super) config: Res<'w, Config>,
    pub(super) commands: Commands<'w, 's>,
}

impl DesktopOpWorld<'_, '_> {
    pub(super) fn exec_bridge_op<T>(
        &mut self,
        label: &str,
        result: Result<T, SpaceError>,
        bridge: &DesktopBridge,
    ) -> Option<T> {
        exec_bridge_op(
            label,
            result,
            bridge,
            &mut self.census,
            &mut self.bridge_state,
            Some(&mut self.commands),
        )
    }

    /// Removes `entities` from every strip other than `skip_strip`, shifting an
    /// active strip's origin rightward when a column strictly left of focus
    /// leaves it so the focused window stays at the same screen `x`.
    pub(super) fn remove_from_strips_preserving_focus(
        &mut self,
        entities: &[Entity],
        skip_strip: Option<Entity>,
    ) {
        let focused = self.windows.focused().map(|(_, focused)| focused);
        for (strip_entity, mut strip, previous, active, _) in &mut self.strips {
            if Some(strip_entity) == skip_strip {
                continue;
            }
            let effective_focused = (active && previous.is_none()).then_some(focused).flatten();
            for &entity in entities {
                if !strip.contains(entity) {
                    continue;
                }
                if let Some(removed_width) =
                    remove_left_column_width(&mut strip, entity, effective_focused, &self.windows)
                    && let Ok((
                        mut position,
                        mut reposition,
                        mut scrolling,
                        child_of,
                        has_manual_offset,
                    )) = self.strip_offsets.get_mut(strip_entity)
                    && let Some(viewport_min_x) = child_of
                        .and_then(|parent| self.display_viewports.get(parent.parent()).ok())
                        .map(|(display, dock)| {
                            display.actual_display_bounds(dock, &self.config).min.x
                        })
                {
                    compensate_left_removal(
                        removed_width,
                        &mut position,
                        reposition.as_deref_mut(),
                        scrolling.as_deref_mut(),
                        has_manual_offset,
                        viewport_min_x,
                        &self.config,
                    );
                }
            }
        }
    }
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
    let matches = |s: &&SpaceRecord| include_fullscreen || s.kind.is_desktop();
    match direction {
        Direction::West | Direction::North => disp.spaces[..cur_idx].iter().rfind(matches),
        Direction::East | Direction::South => disp.spaces[cur_idx + 1..].iter().find(matches),
        Direction::First => disp.spaces.iter().find(matches),
        Direction::Last => disp.spaces.iter().rfind(matches),
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

pub(super) fn read_active_census(
    op_name: &str,
    bridge: &DesktopBridge,
    world: &DesktopOpWorld,
) -> Option<(CGDirectDisplayID, SpaceCensus)> {
    let active_disp_id = active_display_id(&world.displays)?;
    let census = bridge
        .read_census()
        .inspect_err(|err| warn!("{op_name} census failed: {err}"))
        .ok()?;
    Some((active_disp_id, census))
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
        .filter(|(_, s, _, _, _)| s.id() == raw)
        .min_by_key(|(_, s, _, _, selected)| (!*selected, s.virtual_index));
    if let Some((_, strip, prev, _, _)) = target_strip {
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
    let Some((_, strip, _, _, _)) = strips.iter().find(|(_, _, _, active, _)| *active) else {
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
    let Some(disp) = census.display(target.display_id) else {
        return false;
    };
    let hide = sibling_space_ids(census, target);
    let res = bridge.activate_space(&disp.skylight_display, target.id, &hide);
    if world
        .exec_bridge_op(&format!("desktop focus {}", target.id), res, bridge)
        .is_none()
    {
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
    let Some((active_disp_id, census)) = read_active_census("desktop focus", bridge, world) else {
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

struct RelocationPlan {
    group: Option<crate::ecs::layout::RelocationGroup>,
    moving_entities: Vec<Entity>,
    heir: Option<Entity>,
    managed: bool,
}

fn plan_relocation(focused_entity: Entity, world: &DesktopOpWorld) -> RelocationPlan {
    if let Some((_, strip, _, _, _)) = world
        .strips
        .iter()
        .find(|(_, s, _, _, _)| s.contains(focused_entity))
    {
        let group = strip.relocation_group(focused_entity);
        let opener = world.opened_from.get(focused_entity).ok().map(|o| o.0);
        let heir = strip.heir(focused_entity, opener);
        let moving_entities = group
            .as_ref()
            .map_or_else(|| vec![focused_entity], |g| g.members().to_vec());
        return RelocationPlan {
            group,
            moving_entities,
            heir,
            managed: true,
        };
    }
    let managed = matches!(
        world.windows.get_managed(focused_entity),
        Some((_, _, None))
    );
    RelocationPlan {
        group: None,
        moving_entities: vec![focused_entity],
        heir: None,
        managed,
    }
}

fn apply_relocation_plan(
    focused_entity: Entity,
    dest_space: SpaceId,
    dest_display_id: CGDirectDisplayID,
    plan: &RelocationPlan,
    world: &mut DesktopOpWorld,
) {
    if !plan.managed {
        return;
    }

    let raw_dest = dest_space.get();
    let existing_dest = select_desktop_strip(
        world.strips.iter().map(|(e, s, _, _, sel)| (e, s, sel)),
        raw_dest,
    );

    world.remove_from_strips_preserving_focus(&plan.moving_entities, existing_dest);
    if let Some(dest_entity) = existing_dest
        && let Ok((_, mut strip, ..)) = world.strips.get_mut(dest_entity)
    {
        strip.append_relocation_group(focused_entity, plan.group.as_ref());
    }

    if existing_dest.is_none() {
        let mut new_strip = LayoutStrip::new(raw_dest, 0);
        new_strip.append_relocation_group(focused_entity, plan.group.as_ref());
        spawn_desktop_strip(
            new_strip,
            dest_display_id,
            &world.displays,
            false,
            &mut world.commands,
        );
    }
}

pub(super) fn relocate_tiled_group_to_space(
    focused_entity: Entity,
    dest_space: SpaceId,
    dest_display_id: CGDirectDisplayID,
    world: &mut DesktopOpWorld,
) -> (Vec<Entity>, Option<Entity>) {
    let plan = plan_relocation(focused_entity, world);
    apply_relocation_plan(focused_entity, dest_space, dest_display_id, &plan, world);
    (plan.moving_entities, plan.heir)
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
    let Some((active_disp_id, census)) = read_active_census("desktop move", bridge, world) else {
        return;
    };
    let Some(dest) = resolve_desktop_target(&census, active_disp_id, target, false) else {
        debug!("desktop move target {target:?} out of range or not found");
        return;
    };

    let plan = plan_relocation(focused_entity, world);
    let moving_wids: Vec<WinID> = plan
        .moving_entities
        .iter()
        .filter_map(|&e| world.windows.get(e).map(|w| w.id()))
        .collect();
    if moving_wids.is_empty() {
        return;
    }

    let already_sole = match moving_wids.iter().try_fold(true, |sole, &wid| {
        validate_window_move(wid, dest, &census, bridge)
            .map(|memberships| sole && memberships.as_slice() == [dest.id])
    }) {
        Ok(sole) => sole,
        Err(reason) => {
            warn!("desktop move refused: {reason}");
            return;
        }
    };

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

    let res = bridge.move_windows_to_space(&moving_wids, dest.id);
    if world
        .exec_bridge_op(&format!("desktop move to {}", dest.id), res, bridge)
        .is_none()
    {
        return;
    }

    apply_relocation_plan(focused_entity, dest.id, dest.display_id, &plan, world);
    if let Some(history) = world.focus_history.as_deref_mut() {
        for entity in &plan.moving_entities {
            history.forget(*entity);
        }
        history.forget(focused_entity);
    }

    finish_move_window(
        move_focus,
        focused_entity,
        plan.heir,
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
            .find(|(_, _, _, active, _)| *active)
            .and_then(|(_, s, _, _, _)| s.first())
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
    let Some(mut bridge) = bridge else {
        if messages.read().any(|event| {
            matches!(
                event,
                Event::Command {
                    command: Command::Desktop(_),
                }
            )
        }) {
            warn!("desktop command ignored: native DesktopBridge unavailable");
        }
        messages.clear();
        return;
    };

    for event in messages.read() {
        let Event::Command {
            command: Command::Desktop(op),
        } = event
        else {
            continue;
        };
        if world.bridge_state.should_probe() {
            if world.census.0.displays.is_empty() {
                refresh_census_from_bridge(&bridge, &mut world.census);
            }
            probe_bridge_if_needed(&bridge, &world.census.0, &mut world.bridge_state);
        }
        if world.bridge_state.is_unavailable() {
            debug!("desktop command {op:?} ignored: WMBridge mutations unavailable");
            flash_unavailable_once(&mut world.bridge_state, &mut world.commands);
            continue;
        }
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
                super::assign::handle_assign(target, *pid, &mut bridge, &mut world);
            }
        }
    }
}
